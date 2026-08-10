#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn project_service_registry_is_stored_below_runtime_state() {
    let home = tempfile::tempdir().expect("Golutra home");
    let workspace = tempfile::tempdir().expect("workspace");
    let fake_bin = tempfile::tempdir().expect("fake bin");
    let fake_tmux = fake_bin.path().join("tmux");
    let fake_state = home.path().join("fake-tmux-session");
    fs::write(
        &fake_tmux,
        r#"#!/bin/sh
case "$1" in
  has-session)
    if test -f "$FAKE_TMUX_STATE"; then
      exit 0
    fi
    printf "can't find session: fake\n" >&2
    exit 1
    ;;
  new-session) : > "$FAKE_TMUX_STATE" ;;
  kill-session) rm -f "$FAKE_TMUX_STATE" ;;
  capture-pane) printf 'fake service log\n' ;;
  *) exit 2 ;;
esac
"#,
    )
    .expect("fake tmux");
    fs::set_permissions(&fake_tmux, fs::Permissions::from_mode(0o755))
        .expect("fake tmux permissions");
    let path = std::env::join_paths(std::iter::once(fake_bin.path().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .expect("test PATH");

    let output = Command::new(env!("CARGO_BIN_EXE_golutra-cli"))
        .env("GOLUTRA_HOME", home.path())
        .env("FAKE_TMUX_STATE", &fake_state)
        .env("PATH", &path)
        .arg("--cwd")
        .arg(workspace.path())
        .args(["service", "start", "web", "tmux", "printf", "ready"])
        .output()
        .expect("start project service");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let registry_root = home.path().join("state/project-services");
    let workspace_registry = fs::read_dir(&registry_root)
        .expect("state project service registry")
        .next()
        .expect("workspace registry")
        .expect("workspace registry entry")
        .path();
    assert!(workspace_registry.join("web.json").is_file());
    assert!(!home.path().join("project-services").exists());

    let stop = Command::new(env!("CARGO_BIN_EXE_golutra-cli"))
        .env("GOLUTRA_HOME", home.path())
        .env("FAKE_TMUX_STATE", &fake_state)
        .env("PATH", &path)
        .arg("--cwd")
        .arg(workspace.path())
        .args(["service", "stop", "web"])
        .output()
        .expect("stop project service");
    assert!(
        stop.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(!workspace_registry.join("web.json").exists());
}
