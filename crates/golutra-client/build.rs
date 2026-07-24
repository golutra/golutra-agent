use std::{
    env, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
};

use sha2::{Digest, Sha256};

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let workspace = manifest_dir.join("../..");
    let workspace = workspace.canonicalize().unwrap_or(workspace);

    println!(
        "cargo:rerun-if-changed={}",
        workspace.join("Cargo.lock").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        workspace.join(".git/HEAD").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        workspace.join(".git/index").display()
    );
    emit_tracked_reruns(&workspace);

    let commit = git_text(&workspace, &["rev-parse", "HEAD"]);
    let status = git_text(
        &workspace,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )
    .unwrap_or_default();
    let dirty = !status.trim().is_empty();
    let source_digest = source_digest(&workspace, commit.as_deref(), &status);
    let cargo_lock_digest = digest_file(&workspace.join("Cargo.lock")).ok();
    let rustc_version = command_text("rustc", &["--version"]).unwrap_or_else(|| "unknown".into());
    let features = enabled_features();

    emit("GOLUTRA_BUILD_GIT_COMMIT", commit.as_deref().unwrap_or(""));
    emit(
        "GOLUTRA_BUILD_GIT_DIRTY",
        if dirty { "true" } else { "false" },
    );
    emit("GOLUTRA_BUILD_SOURCE_DIGEST", &source_digest);
    emit(
        "GOLUTRA_BUILD_CARGO_LOCK_DIGEST",
        cargo_lock_digest.as_deref().unwrap_or(""),
    );
    emit("GOLUTRA_BUILD_RUSTC_VERSION", &rustc_version);
    emit(
        "GOLUTRA_BUILD_TARGET",
        &env::var("TARGET").unwrap_or_else(|_| "unknown".into()),
    );
    emit(
        "GOLUTRA_BUILD_PROFILE",
        &env::var("PROFILE").unwrap_or_else(|_| "unknown".into()),
    );
    emit("GOLUTRA_BUILD_FEATURES", &features.join(","));
}

fn emit(name: &str, value: &str) {
    let value = value.replace(['\r', '\n'], " ");
    println!("cargo:rustc-env={name}={value}");
}

fn emit_tracked_reruns(workspace: &Path) {
    let Some(files) = git_text(workspace, &["ls-files"]) else {
        return;
    };
    for relative in files.lines().filter(|line| !line.trim().is_empty()) {
        println!(
            "cargo:rerun-if-changed={}",
            workspace.join(relative).display()
        );
    }
}

fn enabled_features() -> Vec<String> {
    let mut features = env::vars_os()
        .filter_map(|(name, _)| name.to_str().map(str::to_owned))
        .filter_map(|name| {
            name.strip_prefix("CARGO_FEATURE_")
                .map(str::to_ascii_lowercase)
        })
        .map(|name| name.replace('_', "-"))
        .collect::<Vec<_>>();
    features.sort();
    features
}

fn source_digest(workspace: &Path, commit: Option<&str>, status: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(commit.unwrap_or("unknown").as_bytes());
    digest.update(status.as_bytes());
    if let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["diff", "--binary", "HEAD"])
        .output()
        && output.status.success()
    {
        digest.update(&output.stdout);
    }
    for line in status.lines().filter(|line| line.starts_with("?? ")) {
        let relative = line.trim_start_matches("?? ");
        digest.update(relative.as_bytes());
        let _ = hash_file_into(&workspace.join(relative), &mut digest);
    }
    format!("sha256:{:x}", digest.finalize())
}

fn digest_file(path: &Path) -> io::Result<String> {
    let mut digest = Sha256::new();
    hash_file_into(path, &mut digest)?;
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn hash_file_into(path: &Path, digest: &mut Sha256) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Ok(());
    }
    let mut file = fs::File::open(path)?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        digest.update(&buffer[..read]);
    }
}

fn git_text(workspace: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn command_text(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
