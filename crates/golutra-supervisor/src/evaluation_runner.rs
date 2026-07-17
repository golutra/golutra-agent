use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsString,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use base64::Engine;
use golutra_core::VerificationResult;
use golutra_protocol::{RuntimeEvaluationWorkerRequest, RuntimeEvaluationWorkerResponse};
use golutra_release::{BuildArtifact, BuildReport, TrustedBuilder};
use golutra_sandbox::{SandboxRequest, SystemSandbox, WorkspaceAccess};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    CandidateStatus, EvaluationInput, EvolutionSupervisor, GateVerdict, RuntimeEvaluationAssertion,
    RuntimeEvaluationCase, RuntimeEvaluationPartition, RuntimeEvaluationSuite, SupervisorError,
    TrustedEvaluationBuild, candidate_tree_digest, observation_from_trace,
    producer::run_producer_process, validate_id,
};

const MAX_EVALUATION_CASES: usize = 32;
const MAX_FIXTURE_FILES: usize = 256;
const MAX_FIXTURE_BYTES: usize = 8 * 1024 * 1024;
const MAX_WORKER_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_TRACE_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;
const MAX_EVALUATION_FILE_BYTES: u64 = 256 * 1024 * 1024;
const WORKER_TIMEOUT: Duration = Duration::from_secs(120);
const EVALUATION_WORKER_BINARY: &str = "golutra-eval-worker";

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeEvaluationRole {
    Baseline,
    Candidate,
}

#[derive(Debug, Serialize)]
struct RuntimeAssertionOutcome {
    assertion: String,
    path: Option<String>,
    passed: bool,
}

#[derive(Debug, Serialize)]
struct RuntimeExecutionEvidence {
    format: &'static str,
    candidate_id: String,
    case_id: String,
    partition: RuntimeEvaluationPartition,
    role: RuntimeEvaluationRole,
    runtime_ref: String,
    binary_checksum: String,
    workspace_digest: String,
    elapsed_ms: Option<u64>,
    trace_chain_digest: Option<String>,
    trace: Option<golutra_protocol::TaskTracePage>,
    artifact_blobs: Vec<RuntimeArtifactBlob>,
    assertion_outcomes: Vec<RuntimeAssertionOutcome>,
    passed: bool,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct RuntimeArtifactBlob {
    artifact_id: String,
    checksum: String,
    content_base64: String,
}

struct CompletedExecution {
    evidence_ref: String,
    passed: bool,
    elapsed_ms: u64,
}

struct RuntimeBinary<'a> {
    path: PathBuf,
    checksum: String,
    runtime_ref: &'a str,
}

impl EvolutionSupervisor {
    pub async fn evaluate_suite(
        &self,
        suite: RuntimeEvaluationSuite,
    ) -> Result<crate::GeneralizationGateResult, SupervisorError> {
        validate_suite(&suite)?;
        let snapshot = self.store().snapshot()?;
        let candidate = snapshot
            .candidates
            .iter()
            .find(|candidate| candidate.candidate_id == suite.candidate_id)
            .cloned()
            .ok_or_else(|| {
                SupervisorError::NotFound(format!("candidate {}", suite.candidate_id))
            })?;
        if candidate.status != CandidateStatus::Frozen {
            return Err(SupervisorError::InvalidTransition(format!(
                "candidate {} is {:?}, not frozen",
                candidate.candidate_id, candidate.status
            )));
        }
        self.validate_frozen_candidate_source(&candidate)?;
        let epoch = snapshot
            .epochs
            .iter()
            .find(|epoch| epoch.epoch_id == candidate.epoch_id)
            .ok_or_else(|| SupervisorError::NotFound(format!("epoch {}", candidate.epoch_id)))?;
        let baseline_release_id = epoch.parent_release_id.as_deref().ok_or_else(|| {
            SupervisorError::InvalidTransition(
                "runtime candidate evaluation requires a bootstrapped stable release".to_owned(),
            )
        })?;
        let baseline_path = self
            .release_store()
            .binary_path(baseline_release_id, EVALUATION_WORKER_BINARY)?;
        let baseline_checksum = sha256_file(&baseline_path)?;

        let artifact_root = self
            .store()
            .paths()
            .artifacts_root
            .join(&candidate.candidate_id);
        let report = TrustedBuilder::new()
            .run(&candidate.worktree, &artifact_root)
            .await?;
        if !report.passed
            || !report.sandbox_enforced
            || report.source_digest != candidate.patch_digest
        {
            return Err(SupervisorError::GateRejected(
                "candidate evaluation build did not pass the trusted build gate".to_owned(),
            ));
        }
        let worker_artifact = evaluation_worker_artifact(&report)?;
        if worker_artifact.checksum == baseline_checksum {
            return Err(SupervisorError::GateRejected(
                "runtime candidate did not produce a distinct evaluation worker binary".to_owned(),
            ));
        }
        let candidate_path = artifact_root.join(&worker_artifact.relative_path);
        verify_staged_worker(&candidate_path, worker_artifact)?;
        let report_ref = self
            .store()
            .store_artifact("build-report", &serde_json::to_vec(&report)?)?;
        let stored_report = report.clone();
        let stored_report_ref = report_ref.clone();
        let stored_candidate_id = candidate.candidate_id.clone();
        self.store().transact(
            "CandidateEvaluationBuildCompleted",
            Some(&candidate.epoch_id),
            Some(&candidate.candidate_id),
            serde_json::json!({
                "source_digest": report.source_digest,
                "sandbox_backend": report.sandbox_backend,
                "binary_artifact_count": report.binary_artifacts.len(),
                "report_ref": report_ref,
            }),
            move |state| {
                state
                    .evaluation_builds
                    .retain(|build| build.candidate_id != stored_candidate_id);
                state.evaluation_builds.push(TrustedEvaluationBuild {
                    candidate_id: stored_candidate_id,
                    report_ref: stored_report_ref,
                    report: stored_report,
                    completed_at: chrono::Utc::now(),
                });
                Ok(())
            },
        )?;

        let run_root = self.store().paths().root.join("evaluation-runs");
        fs::create_dir_all(&run_root).map_err(|error| SupervisorError::Io(error.to_string()))?;
        let run = tempfile::Builder::new()
            .prefix("paired-")
            .tempdir_in(&run_root)
            .map_err(|error| SupervisorError::Io(error.to_string()))?;
        let baseline_runtime = RuntimeBinary {
            path: baseline_path,
            checksum: baseline_checksum,
            runtime_ref: baseline_release_id,
        };
        let candidate_runtime_ref = candidate.candidate_id.clone();
        let candidate_runtime = RuntimeBinary {
            path: candidate_path,
            checksum: worker_artifact.checksum.clone(),
            runtime_ref: &candidate_runtime_ref,
        };

        let mut paired_refs = Vec::with_capacity(suite.cases.len().saturating_mul(2));
        let mut baseline_passes = 0_usize;
        let mut candidate_passes = 0_usize;
        let mut baseline_latency = 0_u64;
        let mut candidate_latency = 0_u64;
        let mut candidate_results = Vec::with_capacity(suite.cases.len());
        for case in &suite.cases {
            let case_root = run.path().join(Uuid::now_v7().to_string());
            fs::create_dir_all(&case_root)
                .map_err(|error| SupervisorError::Io(error.to_string()))?;
            let baseline = self
                .run_runtime_case(
                    &candidate.candidate_id,
                    case,
                    RuntimeEvaluationRole::Baseline,
                    &baseline_runtime,
                    &case_root,
                )
                .await?;
            let candidate_execution = self
                .run_runtime_case(
                    &candidate.candidate_id,
                    case,
                    RuntimeEvaluationRole::Candidate,
                    &candidate_runtime,
                    &case_root,
                )
                .await?;
            for (role, execution) in [("baseline", &baseline), ("candidate", &candidate_execution)]
            {
                self.store().transact(
                    "PairedRuntimeExecutionCompleted",
                    Some(&candidate.epoch_id),
                    Some(&candidate.candidate_id),
                    serde_json::json!({
                        "case_id": case.case_id,
                        "partition": case.partition,
                        "role": role,
                        "passed": execution.passed,
                        "evidence_ref": execution.evidence_ref,
                    }),
                    |_| Ok(()),
                )?;
            }
            baseline_passes = baseline_passes.saturating_add(usize::from(baseline.passed));
            candidate_passes =
                candidate_passes.saturating_add(usize::from(candidate_execution.passed));
            baseline_latency = baseline_latency.saturating_add(baseline.elapsed_ms);
            candidate_latency = candidate_latency.saturating_add(candidate_execution.elapsed_ms);
            paired_refs.push(baseline.evidence_ref);
            paired_refs.push(candidate_execution.evidence_ref.clone());
            candidate_results.push((case.partition, candidate_execution.passed));
        }

        let case_count = suite.cases.len() as f32;
        let quality_delta = (candidate_passes as f32 - baseline_passes as f32) / case_count;
        let latency_delta_ms = i64::try_from(candidate_latency)
            .unwrap_or(i64::MAX)
            .saturating_sub(i64::try_from(baseline_latency).unwrap_or(i64::MAX));
        self.validate_frozen_candidate_source(&candidate)?;
        let input = EvaluationInput {
            candidate_id: candidate.candidate_id,
            paired_execution_refs: paired_refs.clone(),
            development_verdict: partition_verdict(
                &candidate_results,
                RuntimeEvaluationPartition::Development,
            ),
            security_verdict: partition_verdict(
                &candidate_results,
                RuntimeEvaluationPartition::Security,
            ),
            migration_verdict: partition_verdict(
                &candidate_results,
                RuntimeEvaluationPartition::Migration,
            ),
            sealed_verdict: partition_verdict(
                &candidate_results,
                RuntimeEvaluationPartition::Sealed,
            ),
            fresh_verdict: partition_verdict(&candidate_results, RuntimeEvaluationPartition::Fresh),
            quality_delta,
            cost_delta_usd: 0.0,
            latency_delta_ms,
            holdout_queries: u32::try_from(
                suite
                    .cases
                    .iter()
                    .filter(|case| case.partition == RuntimeEvaluationPartition::Sealed)
                    .count(),
            )
            .unwrap_or(u32::MAX),
            exact_feedback_exposed: false,
            evidence_refs: paired_refs,
        };
        self.evaluate_input(input)
    }

    async fn run_runtime_case(
        &self,
        candidate_id: &str,
        case: &RuntimeEvaluationCase,
        role: RuntimeEvaluationRole,
        runtime: &RuntimeBinary<'_>,
        case_root: &Path,
    ) -> Result<CompletedExecution, SupervisorError> {
        let role_name = match role {
            RuntimeEvaluationRole::Baseline => "baseline",
            RuntimeEvaluationRole::Candidate => "candidate",
        };
        let role_root = case_root.join(Uuid::now_v7().to_string());
        let workspace = role_root.join("workspace");
        let scratch = role_root.join("scratch");
        let home = scratch.join("home");
        fs::create_dir_all(&workspace)
            .and_then(|_| fs::create_dir_all(&home))
            .map_err(|error| SupervisorError::Io(error.to_string()))?;
        write_fixture(&workspace, &case.fixture_files)?;
        let request = RuntimeEvaluationWorkerRequest {
            objective: case.objective.clone(),
            payload: case.payload.clone(),
        };
        let request_json = serde_json::to_vec(&request)?;
        let binary_parent = runtime.path.parent().ok_or_else(|| {
            SupervisorError::Integrity("evaluation worker binary has no parent".to_owned())
        })?;
        let launch = SystemSandbox::detect()
            .plan(&SandboxRequest {
                program: runtime.path.as_os_str().to_owned(),
                args: vec![
                    OsString::from("--home"),
                    home.as_os_str().to_owned(),
                    OsString::from("--workspace"),
                    workspace.as_os_str().to_owned(),
                ],
                cwd: workspace.clone(),
                workspace_root: workspace.clone(),
                scratch_dir: scratch.clone(),
                read_only_roots: vec![binary_parent.to_path_buf()],
                workspace_access: WorkspaceAccess::ReadWrite,
                allow_network: false,
            })
            .map_err(|error| SupervisorError::Producer(error.to_string()))?;
        if !launch.os_enforced {
            return Err(SupervisorError::GateRejected(
                "runtime evaluation requires macOS Seatbelt or Linux bubblewrap".to_owned(),
            ));
        }
        let started = Instant::now();
        let process = run_producer_process(
            launch,
            &workspace,
            &request_json,
            WORKER_TIMEOUT,
            MAX_WORKER_OUTPUT_BYTES,
        )
        .await;
        let observed_elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let workspace_digest = candidate_tree_digest(&workspace)?;
        let mut response = None;
        let mut error = None;
        match process {
            Ok(output) if output.status.success() => {
                match serde_json::from_slice::<RuntimeEvaluationWorkerResponse>(&output.stdout) {
                    Ok(parsed) => response = Some(parsed),
                    Err(parse_error) => {
                        error = Some(format!(
                            "evaluation worker output is invalid: {parse_error}"
                        ))
                    }
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                error = Some(format!(
                    "evaluation worker exited with {}: {}",
                    output.status,
                    stderr.trim().chars().take(512).collect::<String>()
                ));
            }
            Err(process_error) => error = Some(process_error.to_string()),
        }

        let mut outcomes = Vec::with_capacity(case.assertions.len());
        let artifact_blobs = response
            .as_ref()
            .map(|response| capture_trace_artifacts(&home, response))
            .transpose();
        let artifact_blobs_valid = artifact_blobs.as_ref().is_ok_and(|value| value.is_some());
        if let Err(artifact_error) = &artifact_blobs
            && error.is_none()
        {
            error = Some(artifact_error.clone());
        }
        let trace_valid = response.as_ref().is_some_and(|response| {
            artifact_blobs_valid && observation_from_trace(&response.trace, role_name).is_ok()
        });
        for assertion in &case.assertions {
            outcomes.push(evaluate_assertion(
                assertion,
                &workspace,
                response.as_ref(),
                trace_valid,
            )?);
        }
        let passed = trace_valid && outcomes.iter().all(|outcome| outcome.passed);
        if response.is_some() && !trace_valid && error.is_none() {
            error = Some("evaluation worker trace failed Supervisor validation".to_owned());
        }
        let evidence = RuntimeExecutionEvidence {
            format: "golutra.supervisor-runtime-execution.v1",
            candidate_id: candidate_id.to_owned(),
            case_id: case.case_id.clone(),
            partition: case.partition,
            role,
            runtime_ref: runtime.runtime_ref.to_owned(),
            binary_checksum: runtime.checksum.clone(),
            workspace_digest,
            elapsed_ms: Some(observed_elapsed_ms),
            trace_chain_digest: response
                .as_ref()
                .map(|response| response.trace.integrity.event_chain_digest.clone()),
            trace: response.map(|response| response.trace),
            artifact_blobs: artifact_blobs.ok().flatten().unwrap_or_default(),
            assertion_outcomes: outcomes,
            passed,
            error,
        };
        let bytes = serde_json::to_vec(&evidence)?;
        let evidence_ref = self.store().store_artifact("evaluation", &bytes)?;
        Ok(CompletedExecution {
            evidence_ref,
            passed,
            elapsed_ms: observed_elapsed_ms,
        })
    }
}

fn validate_suite(suite: &RuntimeEvaluationSuite) -> Result<(), SupervisorError> {
    if suite.candidate_id.trim().is_empty() {
        return Err(SupervisorError::Invalid(
            "evaluation candidate id is required".to_owned(),
        ));
    }
    if suite.cases.is_empty() || suite.cases.len() > MAX_EVALUATION_CASES {
        return Err(SupervisorError::Invalid(format!(
            "evaluation suite must contain 1..={MAX_EVALUATION_CASES} cases"
        )));
    }
    let mut ids = HashSet::new();
    let mut partitions = HashSet::new();
    for case in &suite.cases {
        validate_id(&case.case_id, "evaluation case id")?;
        if !ids.insert(case.case_id.clone()) {
            return Err(SupervisorError::Invalid(
                "evaluation case ids must be unique".to_owned(),
            ));
        }
        if case.objective.trim().is_empty() || case.objective.len() > 16 * 1024 {
            return Err(SupervisorError::Invalid(format!(
                "evaluation case {} has an invalid objective",
                case.case_id
            )));
        }
        if !case.payload.is_object() {
            return Err(SupervisorError::Invalid(format!(
                "evaluation case {} payload must be an object",
                case.case_id
            )));
        }
        if case.assertions.is_empty() {
            return Err(SupervisorError::Invalid(format!(
                "evaluation case {} requires trusted assertions",
                case.case_id
            )));
        }
        validate_fixture(&case.fixture_files)?;
        for assertion in &case.assertions {
            match assertion {
                RuntimeEvaluationAssertion::VerificationPass => {}
                RuntimeEvaluationAssertion::FileExists { path }
                | RuntimeEvaluationAssertion::FileAbsent { path }
                | RuntimeEvaluationAssertion::FileSha256 { path, .. } => {
                    validate_relative_path(path)?;
                }
            }
            if let RuntimeEvaluationAssertion::FileSha256 { checksum, .. } = assertion
                && (checksum.len() != 71
                    || !checksum.starts_with("sha256:")
                    || !checksum[7..]
                        .chars()
                        .all(|character| character.is_ascii_hexdigit()))
            {
                return Err(SupervisorError::Invalid(
                    "evaluation file checksum must be a sha256 digest".to_owned(),
                ));
            }
        }
        partitions.insert(case.partition);
    }
    for required in [
        RuntimeEvaluationPartition::Development,
        RuntimeEvaluationPartition::Security,
        RuntimeEvaluationPartition::Migration,
        RuntimeEvaluationPartition::Sealed,
        RuntimeEvaluationPartition::Fresh,
    ] {
        if !partitions.contains(&required) {
            return Err(SupervisorError::Invalid(format!(
                "evaluation suite is missing the {required:?} partition"
            )));
        }
    }
    Ok(())
}

fn validate_fixture(files: &BTreeMap<String, String>) -> Result<(), SupervisorError> {
    if files.len() > MAX_FIXTURE_FILES {
        return Err(SupervisorError::Invalid(
            "evaluation fixture exceeds its file-count limit".to_owned(),
        ));
    }
    let mut total = 0_usize;
    for (path, content) in files {
        validate_relative_path(path)?;
        total = total.saturating_add(content.len());
        if total > MAX_FIXTURE_BYTES {
            return Err(SupervisorError::Invalid(
                "evaluation fixture exceeds its byte limit".to_owned(),
            ));
        }
    }
    Ok(())
}

fn write_fixture(
    workspace: &Path,
    files: &BTreeMap<String, String>,
) -> Result<(), SupervisorError> {
    for (relative, content) in files {
        let target = workspace.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| SupervisorError::Io(error.to_string()))?;
        }
        fs::write(&target, content)
            .map_err(|error| SupervisorError::Io(format!("{}: {error}", target.display())))?;
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> Result<(), SupervisorError> {
    let path = Path::new(value);
    if value.trim().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
        || path.components().any(|component| {
            matches!(component, Component::Normal(value) if matches!(value.to_str(), Some(".git" | ".golutra" | "target" | "node_modules")))
        })
    {
        return Err(SupervisorError::Invalid(format!(
            "evaluation path is unsafe: {value}"
        )));
    }
    Ok(())
}

pub(crate) fn evaluation_worker_artifact(
    report: &BuildReport,
) -> Result<&BuildArtifact, SupervisorError> {
    report
        .binary_artifacts
        .iter()
        .find(|artifact| {
            Path::new(&artifact.relative_path)
                .file_name()
                .is_some_and(|name| name == EVALUATION_WORKER_BINARY)
        })
        .ok_or_else(|| {
            SupervisorError::GateRejected(
                "candidate build contains no sealed evaluation worker".to_owned(),
            )
        })
}

fn verify_staged_worker(path: &Path, artifact: &BuildArtifact) -> Result<(), SupervisorError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| SupervisorError::Io(format!("{}: {error}", path.display())))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() != artifact.size_bytes
        || sha256_file(path)? != artifact.checksum
    {
        return Err(SupervisorError::Integrity(
            "staged evaluation worker does not match its trusted build report".to_owned(),
        ));
    }
    Ok(())
}

fn evaluate_assertion(
    assertion: &RuntimeEvaluationAssertion,
    workspace: &Path,
    response: Option<&RuntimeEvaluationWorkerResponse>,
    trace_valid: bool,
) -> Result<RuntimeAssertionOutcome, SupervisorError> {
    let (assertion_name, path, passed) = match assertion {
        RuntimeEvaluationAssertion::VerificationPass => (
            "verification_pass".to_owned(),
            None,
            trace_valid
                && response.is_some_and(|response| {
                    response
                        .trace
                        .verification
                        .as_ref()
                        .is_some_and(|record| record.result == VerificationResult::Pass)
                }),
        ),
        RuntimeEvaluationAssertion::FileExists { path } => {
            let metadata = fs::symlink_metadata(workspace.join(path)).ok();
            (
                "file_exists".to_owned(),
                Some(path.clone()),
                metadata.is_some_and(|metadata| !metadata.file_type().is_symlink()),
            )
        }
        RuntimeEvaluationAssertion::FileAbsent { path } => (
            "file_absent".to_owned(),
            Some(path.clone()),
            matches!(
                fs::symlink_metadata(workspace.join(path)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ),
        ),
        RuntimeEvaluationAssertion::FileSha256 { path, checksum } => {
            let actual = sha256_file(&workspace.join(path)).ok();
            (
                "file_sha256".to_owned(),
                Some(path.clone()),
                actual.is_some_and(|actual| actual.eq_ignore_ascii_case(checksum)),
            )
        }
    };
    Ok(RuntimeAssertionOutcome {
        assertion: assertion_name,
        path,
        passed,
    })
}

fn capture_trace_artifacts(
    home: &Path,
    response: &RuntimeEvaluationWorkerResponse,
) -> Result<Vec<RuntimeArtifactBlob>, String> {
    let manifests = response
        .trace
        .artifacts
        .iter()
        .map(|artifact| (artifact.artifact_id, artifact))
        .collect::<BTreeMap<_, _>>();
    if manifests.len() != response.trace.artifacts.len() {
        return Err("runtime trace contains duplicate artifact manifests".to_owned());
    }
    for snapshot in &response.trace.context_snapshots {
        let Some(artifact_id) = snapshot.redacted_request_artifact_ref else {
            return Err(format!(
                "context snapshot {} has no redacted request artifact",
                snapshot.snapshot_id
            ));
        };
        if !manifests.contains_key(&artifact_id) {
            return Err(format!(
                "context snapshot {} references an absent redacted artifact",
                snapshot.snapshot_id
            ));
        }
    }
    for event in &response.trace.events {
        if let Some(artifact_id) = event.payload_ref
            && !manifests.contains_key(&artifact_id)
        {
            return Err(format!(
                "runtime event {} references an absent artifact",
                event.id
            ));
        }
    }
    let root = home.join("state/artifacts");
    let root_metadata = fs::symlink_metadata(&root)
        .map_err(|error| format!("artifact root {} cannot be read: {error}", root.display()))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err("runtime artifact root is not a regular directory".to_owned());
    }
    let expected_names = manifests
        .keys()
        .map(|artifact_id| format!("{artifact_id}.blob"))
        .collect::<HashSet<_>>();
    let actual_names = fs::read_dir(&root)
        .map_err(|error| format!("artifact root {} cannot be read: {error}", root.display()))?
        .map(|entry| {
            let entry = entry.map_err(|error| error.to_string())?;
            let file_type = entry.file_type().map_err(|error| error.to_string())?;
            if file_type.is_symlink() || !file_type.is_file() {
                return Err(format!(
                    "runtime artifact root contains a non-file entry: {}",
                    entry.path().display()
                ));
            }
            Ok(entry.file_name().to_string_lossy().into_owned())
        })
        .collect::<Result<HashSet<_>, String>>()?;
    if actual_names != expected_names {
        return Err("runtime artifact directory does not match the trace manifest".to_owned());
    }
    let mut total = 0_usize;
    let mut blobs = Vec::with_capacity(manifests.len());
    for (artifact_id, artifact) in manifests {
        let path = root.join(format!("{artifact_id}.blob"));
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("artifact {} cannot be read: {error}", path.display()))?;
        let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        total = total.saturating_add(size);
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() != artifact.size_bytes
            || total > MAX_TRACE_ARTIFACT_BYTES
        {
            return Err(format!(
                "runtime artifact {} violates its manifest or size limit",
                artifact.artifact_id
            ));
        }
        let bytes = fs::read(&path)
            .map_err(|error| format!("artifact {} cannot be read: {error}", path.display()))?;
        let checksum = format!("sha256:{:x}", Sha256::digest(&bytes));
        if checksum != artifact.checksum {
            return Err(format!(
                "runtime artifact {} checksum does not match its manifest",
                artifact.artifact_id
            ));
        }
        blobs.push(RuntimeArtifactBlob {
            artifact_id: artifact.artifact_id.to_string(),
            checksum,
            content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
    }
    Ok(blobs)
}

fn partition_verdict(
    results: &[(RuntimeEvaluationPartition, bool)],
    partition: RuntimeEvaluationPartition,
) -> GateVerdict {
    let matching = results
        .iter()
        .filter(|(candidate_partition, _)| *candidate_partition == partition)
        .map(|(_, passed)| *passed)
        .collect::<Vec<_>>();
    if matching.is_empty() {
        GateVerdict::Inconclusive
    } else if matching.into_iter().all(|passed| passed) {
        GateVerdict::Pass
    } else {
        GateVerdict::Fail
    }
}

fn sha256_file(path: &Path) -> Result<String, SupervisorError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| SupervisorError::Io(format!("{}: {error}", path.display())))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_EVALUATION_FILE_BYTES
    {
        return Err(SupervisorError::Integrity(format!(
            "evaluation artifact violates its file boundary: {}",
            path.display()
        )));
    }
    let mut file = fs::File::open(path)
        .map_err(|error| SupervisorError::Io(format!("{}: {error}", path.display())))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| SupervisorError::Io(format!("{}: {error}", path.display())))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        if total > MAX_EVALUATION_FILE_BYTES {
            return Err(SupervisorError::Invalid(
                "evaluation artifact exceeds its size limit".to_owned(),
            ));
        }
        digest.update(&buffer[..read]);
    }
    if total != metadata.len() {
        return Err(SupervisorError::Integrity(
            "evaluation artifact changed while hashing".to_owned(),
        ));
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(partition: RuntimeEvaluationPartition) -> RuntimeEvaluationCase {
        RuntimeEvaluationCase {
            case_id: format!("case-{partition:?}"),
            partition,
            objective: "read README.md".to_owned(),
            payload: serde_json::json!({"path": "README.md"}),
            fixture_files: BTreeMap::from([("README.md".to_owned(), "fixture".to_owned())]),
            assertions: vec![RuntimeEvaluationAssertion::VerificationPass],
        }
    }

    #[test]
    fn suite_requires_every_governance_partition() {
        let suite = RuntimeEvaluationSuite {
            candidate_id: "candidate-test".to_owned(),
            cases: vec![case(RuntimeEvaluationPartition::Development)],
        };
        assert!(validate_suite(&suite).is_err());

        let suite = RuntimeEvaluationSuite {
            candidate_id: "candidate-test".to_owned(),
            cases: [
                RuntimeEvaluationPartition::Development,
                RuntimeEvaluationPartition::Security,
                RuntimeEvaluationPartition::Migration,
                RuntimeEvaluationPartition::Sealed,
                RuntimeEvaluationPartition::Fresh,
            ]
            .into_iter()
            .map(case)
            .collect(),
        };
        validate_suite(&suite).expect("complete suite");
    }

    #[test]
    fn unsafe_fixture_and_assertion_paths_are_rejected() {
        for path in ["../outside", ".golutra/runtime.sqlite", "target/debug/bin"] {
            assert!(validate_relative_path(path).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_absent_assertion_rejects_a_broken_symlink() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::os::unix::fs::symlink(
            workspace.path().join("missing-target"),
            workspace.path().join("claimed-absent"),
        )
        .expect("symlink");
        let outcome = evaluate_assertion(
            &RuntimeEvaluationAssertion::FileAbsent {
                path: "claimed-absent".to_owned(),
            },
            workspace.path(),
            None,
            false,
        )
        .expect("assertion");

        assert!(!outcome.passed);
    }
}
