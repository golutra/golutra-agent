use std::{fs::File, io::Read, path::Path, sync::OnceLock};

use golutra_core::{
    BUILD_PROVENANCE_SCHEMA_VERSION, BuildProvenance, RUN_PROVENANCE_SCHEMA_VERSION, RunId,
    RunProvenance, TaskId, WorkspaceId,
};
use sha2::{Digest, Sha256};

use super::runtime_identity;

static BUILD_PROVENANCE: OnceLock<BuildProvenance> = OnceLock::new();

#[must_use]
pub(crate) fn build_provenance() -> BuildProvenance {
    BUILD_PROVENANCE
        .get_or_init(capture_build_provenance)
        .clone()
}

#[must_use]
pub(crate) fn run_provenance(
    task_id: TaskId,
    workspace_id: WorkspaceId,
    workspace_root: Option<&Path>,
    provider_config: Option<&Path>,
) -> RunProvenance {
    let workspace_initial_digest = workspace_root.map(path_identity_digest);
    let provider_config_digest = provider_config.and_then(digest_file);
    let runtime_config_digest = Some(digest_parts(&[
        env!("CARGO_PKG_VERSION"),
        &workspace_id.to_string(),
        &runtime_identity(),
    ]));
    RunProvenance {
        schema_version: RUN_PROVENANCE_SCHEMA_VERSION,
        run_id: RunId::from(task_id),
        runtime_identity: runtime_identity(),
        build: build_provenance(),
        runtime_config_digest,
        provider_config_digest,
        tool_manifest_digest: Some(digest_parts(&[
            "shell",
            "read_file",
            "write_file",
            "process",
            "mcp",
        ])),
        policy_digest: Some(digest_parts(&["workspace-policy-v1"])),
        verifier_digest: Some(digest_parts(&["runtime-verification-service-v1"])),
        workspace_initial_digest,
        captured_at: chrono::Utc::now(),
    }
}

fn capture_build_provenance() -> BuildProvenance {
    BuildProvenance {
        schema_version: BUILD_PROVENANCE_SCHEMA_VERSION,
        package_version: env!("CARGO_PKG_VERSION").to_owned(),
        git_commit: non_empty(option_env!("GOLUTRA_BUILD_GIT_COMMIT")),
        dirty: option_env!("GOLUTRA_BUILD_GIT_DIRTY") == Some("true"),
        source_digest: non_empty(option_env!("GOLUTRA_BUILD_SOURCE_DIGEST")),
        cargo_lock_digest: non_empty(option_env!("GOLUTRA_BUILD_CARGO_LOCK_DIGEST")),
        target: option_env!("GOLUTRA_BUILD_TARGET")
            .unwrap_or("unknown")
            .to_owned(),
        profile: option_env!("GOLUTRA_BUILD_PROFILE")
            .unwrap_or("unknown")
            .to_owned(),
        features: option_env!("GOLUTRA_BUILD_FEATURES")
            .unwrap_or_default()
            .split(',')
            .filter(|feature| !feature.is_empty())
            .map(str::to_owned)
            .collect(),
        rustc_version: option_env!("GOLUTRA_BUILD_RUSTC_VERSION")
            .unwrap_or("unknown")
            .to_owned(),
        binary_checksum: std::env::current_exe()
            .ok()
            .as_deref()
            .and_then(digest_file),
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn path_identity_digest(path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    digest_parts(&[canonical.to_string_lossy().as_ref()])
}

fn digest_parts(parts: &[&str]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part.len().to_be_bytes());
        digest.update(part.as_bytes());
    }
    format!("sha256:{:x}", digest.finalize())
}

fn digest_file(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            return Some(format!("sha256:{:x}", digest.finalize()));
        }
        digest.update(&buffer[..read]);
    }
}
