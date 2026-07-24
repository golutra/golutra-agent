//! Validation and canonical ingestion for out-of-process evaluator results.

use std::{collections::HashMap, fs, path::Path};

use base64::Engine;
use golutra_core::{ActorKind, TraceView};
use golutra_eval::{
    EvaluationAttestation, EvaluationPartitionKind, ExternalEvaluationRecord,
    ExternalEvaluationTrust, external_evaluation_result_digest,
};
use golutra_protocol::{RuntimeEventSource, RuntimeEventType};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::Deserialize;

use super::*;

const MAX_TRUST_STORE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Deserialize)]
struct EvaluationTrustStore {
    version: u32,
    keys: HashMap<String, EvaluationTrustKey>,
}

#[derive(Debug, Deserialize)]
struct EvaluationTrustKey {
    algorithm: String,
    public_key_base64: String,
}

impl RuntimeHost {
    pub(super) async fn handle_external_evaluation_command(
        self: &Arc<Self>,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let mut record = command
            .payload
            .get("record")
            .cloned()
            .map(serde_json::from_value::<ExternalEvaluationRecord>)
            .transpose()?
            .ok_or_else(|| {
                ClientError::InvalidSession("external evaluation record is required".to_owned())
            })?;
        self.validate_external_evaluation(session_id, &command.actor, &record)
            .await?;
        record.ingested_at = chrono::Utc::now();
        let evaluation_store = self.evaluation_store.clone();
        let stored = record.clone();
        let inserted =
            run_blocking(move || evaluation_store.record_external_evaluation(stored)).await??;
        let comparison = if inserted {
            let evaluation_store = self.evaluation_store.clone();
            let evaluation_id = record.evaluation_id.clone();
            run_blocking(move || {
                evaluation_store.snapshot().map(|state| {
                    state.causal_comparisons.into_iter().find(|comparison| {
                        comparison.baseline_evaluation_ref.as_deref()
                            == Some(evaluation_id.as_str())
                            || comparison.candidate_evaluation_ref.as_deref()
                                == Some(evaluation_id.as_str())
                    })
                })
            })
            .await??
        } else {
            None
        };
        if inserted {
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                Some(record.source_task_id),
                RuntimeEventType::ExternalEvaluationIngested,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!(
                        "external evaluation {} ingested as {:?}",
                        record.evaluation_id, record.verdict
                    ),
                    "record": record,
                    "command_id": command.command_id,
                }),
            ))
            .await?;
            if let Some(comparison) = comparison {
                self.record_event(host_event(
                    self.next_sequence_no(),
                    session_id,
                    Some(record.source_task_id),
                    RuntimeEventType::ExternalEvaluationCompared,
                    RuntimeEventSource::Evaluator,
                    json!({
                        "summary": format!(
                            "external baseline/candidate comparison {} recorded",
                            comparison.comparison_id
                        ),
                        "record": comparison,
                        "command_id": command.command_id,
                    }),
                ))
                .await?;
            }
        }
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(if inserted {
                format!("external evaluation {} ingested", record.evaluation_id)
            } else {
                format!(
                    "external evaluation {} was already present",
                    record.evaluation_id
                )
            }),
        })
    }

    async fn validate_external_evaluation(
        self: &Arc<Self>,
        session_id: SessionId,
        actor: &Actor,
        record: &ExternalEvaluationRecord,
    ) -> Result<(), ClientError> {
        if record.partition == EvaluationPartitionKind::Holdout && !record.holdout_protected {
            return Err(ClientError::TaskExecution(
                "holdout evaluation must declare holdout_protected=true".to_owned(),
            ));
        }
        if record.result_digest != external_evaluation_result_digest(record) {
            return Err(ClientError::TaskExecution(
                "external evaluation result_digest does not match canonical result facts"
                    .to_owned(),
            ));
        }
        let trace = TaskTraceService::new(self.clone())
            .read_complete(TaskTraceRequest {
                session_id,
                task_id: record.source_task_id,
                view: TraceView::Full,
                cursor: None,
                limit: 512,
                wait_for_evaluation: true,
            })
            .await?;
        if !trace.integrity.complete {
            return Err(ClientError::TaskExecution(format!(
                "external evaluation base trace is incomplete: {:?}",
                trace.integrity.missing_sections
            )));
        }
        if trace.integrity.event_chain_digest != record.base_trace_digest {
            return Err(ClientError::TaskExecution(format!(
                "external evaluation base_trace_digest does not match task {}",
                record.source_task_id
            )));
        }
        if trace.runtime_identity != record.runtime_identity {
            return Err(ClientError::TaskExecution(
                "external evaluation runtime_identity does not match the source trace".to_owned(),
            ));
        }
        match record.trust {
            ExternalEvaluationTrust::UntrustedLocal => {}
            ExternalEvaluationTrust::OwnerLocal => {
                if !matches!(
                    actor.kind,
                    ActorKind::User | ActorKind::Cli | ActorKind::Tui
                ) {
                    return Err(ClientError::TaskExecution(
                        "owner-local evaluation requires an authenticated owner interaction"
                            .to_owned(),
                    ));
                }
            }
            ExternalEvaluationTrust::Signed => {
                let attestation = record.attestation.as_ref().ok_or_else(|| {
                    ClientError::TaskExecution(
                        "signed external evaluation has no attestation".to_owned(),
                    )
                })?;
                self.verify_evaluation_attestation(attestation, &record.result_digest)?;
            }
        }
        Ok(())
    }

    fn verify_evaluation_attestation(
        &self,
        attestation: &EvaluationAttestation,
        result_digest: &str,
    ) -> Result<(), ClientError> {
        let trust_path = self.evaluation_trust_store_path()?;
        verify_evaluation_attestation_at(&trust_path, attestation, result_digest)
    }

    fn evaluation_trust_store_path(&self) -> Result<std::path::PathBuf, ClientError> {
        if let Some(path) = self
            .provider_config_paths
            .as_ref()
            .and_then(|paths| paths.user_config.parent())
        {
            return Ok(path.join("evaluation-trust.json"));
        }
        self.runtime_paths
            .as_ref()
            .map(|paths| paths.home.join("evaluation-trust.json"))
            .ok_or_else(|| {
                ClientError::TaskExecution(
                    "signed evaluation trust store is unavailable in this runtime".to_owned(),
                )
            })
    }
}

fn verify_evaluation_attestation_at(
    trust_path: &Path,
    attestation: &EvaluationAttestation,
    result_digest: &str,
) -> Result<(), ClientError> {
    if attestation.algorithm != "ed25519"
        || attestation.signed_digest != result_digest
        || attestation.key_id.trim().is_empty()
    {
        return Err(ClientError::TaskExecution(
            "external evaluation attestation metadata is invalid".to_owned(),
        ));
    }
    let trust = load_evaluation_trust_store(trust_path)?;
    let key = trust.keys.get(&attestation.key_id).ok_or_else(|| {
        ClientError::TaskExecution(format!(
            "evaluation attestation key {} is not trusted",
            attestation.key_id
        ))
    })?;
    if key.algorithm != "ed25519" {
        return Err(ClientError::TaskExecution(format!(
            "evaluation trust key {} has unsupported algorithm {}",
            attestation.key_id, key.algorithm
        )));
    }
    let public_key = base64::engine::general_purpose::STANDARD
        .decode(&key.public_key_base64)
        .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
    let signature = base64::engine::general_purpose::STANDARD
        .decode(&attestation.signature)
        .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(result_digest.as_bytes(), &signature)
        .map_err(|_| {
            ClientError::TaskExecution(
                "external evaluation attestation signature is invalid".to_owned(),
            )
        })
}

fn load_evaluation_trust_store(path: &Path) -> Result<EvaluationTrustStore, ClientError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        ClientError::TaskExecution(format!(
            "failed to read evaluation trust store {}: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_TRUST_STORE_BYTES
    {
        return Err(ClientError::TaskExecution(
            "evaluation trust store must be a bounded regular file".to_owned(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(ClientError::TaskExecution(
                "evaluation trust store cannot be group/world writable".to_owned(),
            ));
        }
    }
    let bytes = fs::read(path).map_err(|error| ClientError::Io(error.to_string()))?;
    let trust: EvaluationTrustStore = serde_json::from_slice(&bytes)?;
    if trust.version != 1 || trust.keys.is_empty() {
        return Err(ClientError::TaskExecution(
            "evaluation trust store version or key set is invalid".to_owned(),
        ));
    }
    Ok(trust)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use base64::Engine;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn signed_attestation_requires_the_trusted_key_and_exact_digest() {
        let directory = tempdir().expect("trust directory");
        let path = directory.path().join("evaluation-trust.json");
        let seed = [7_u8; 32];
        let key_pair = Ed25519KeyPair::from_seed_unchecked(&seed).expect("key pair");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "keys": {
                    "terminal-bench": {
                        "algorithm": "ed25519",
                        "public_key_base64": base64::engine::general_purpose::STANDARD
                            .encode(key_pair.public_key().as_ref()),
                    }
                }
            }))
            .expect("trust json"),
        )
        .expect("trust store");
        let digest = "sha256:trusted-result";
        let attestation = EvaluationAttestation {
            algorithm: "ed25519".to_owned(),
            key_id: "terminal-bench".to_owned(),
            signature: base64::engine::general_purpose::STANDARD
                .encode(key_pair.sign(digest.as_bytes()).as_ref()),
            signed_digest: digest.to_owned(),
        };

        verify_evaluation_attestation_at(&path, &attestation, digest).expect("valid signature");
        assert!(verify_evaluation_attestation_at(&path, &attestation, "sha256:other").is_err());

        let mut wrong_key = attestation.clone();
        wrong_key.key_id = "unknown".to_owned();
        assert!(verify_evaluation_attestation_at(&path, &wrong_key, digest).is_err());

        let mut wrong_signature = attestation;
        wrong_signature.signature = base64::engine::general_purpose::STANDARD.encode([0_u8; 64]);
        assert!(verify_evaluation_attestation_at(&path, &wrong_signature, digest).is_err());
    }

    #[test]
    fn trust_store_rejects_invalid_version_and_unsafe_permissions() {
        let directory = tempdir().expect("trust directory");
        let path = directory.path().join("evaluation-trust.json");
        fs::write(
            &path,
            br#"{"version":2,"keys":{"key":{"algorithm":"ed25519","public_key_base64":"AA=="}}}"#,
        )
        .expect("trust store");
        assert!(load_evaluation_trust_store(&path).is_err());

        fs::write(
            &path,
            br#"{"version":1,"keys":{"key":{"algorithm":"ed25519","public_key_base64":"AA=="}}}"#,
        )
        .expect("trust store");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(&path, fs::Permissions::from_mode(0o666))
                .expect("unsafe permissions");
            assert!(load_evaluation_trust_store(&path).is_err());
        }
    }
}
