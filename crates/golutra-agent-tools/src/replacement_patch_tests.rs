//! 完整替换是删除后重建的明确意图，仍须复用整批校验、权限与原子应用。

use super::*;

#[tokio::test]
async fn replacement_patch_updates_existing_files_without_a_second_model_call() {
    let workspace = tempdir().unwrap();
    fs::write(workspace.path().join("one.txt"), "old\n").unwrap();
    fs::write(workspace.path().join("empty.txt"), "remove me\n").unwrap();
    let patch = concat!(
        "*** Begin Patch\n",
        "*** Delete File: one.txt\n",
        "*** Delete File: empty.txt\n",
        "*** Add File: created.txt\n+created\n",
        "*** Add File: one.txt\n+替换内容\n\\ No newline at end of file\n",
        "*** Add File: empty.txt\n",
        "*** End Patch\n",
    );
    let report = executor(workspace.path())
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect("unambiguous delete/add should become one atomic replacement");
    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        fs::read_to_string(workspace.path().join("one.txt")).unwrap(),
        "替换内容"
    );
    assert_eq!(fs::read(workspace.path().join("empty.txt")).unwrap(), b"");
    assert_eq!(
        fs::read_to_string(workspace.path().join("created.txt")).unwrap(),
        "created\n"
    );
    assert_eq!(report.changed_files.len(), 3);
    assert_eq!(report.before_images.len(), 3);
    assert_eq!(report.after_images.len(), 3);
    assert!(
        report.envelope.structured_facts["change_preview"]
            .to_string()
            .contains("替换内容")
    );
}

#[tokio::test]
async fn replacement_patch_keeps_all_files_when_another_entry_is_invalid() {
    let workspace = tempdir().unwrap();
    fs::write(workspace.path().join("one.txt"), "original\n").unwrap();
    for invalid in [
        "*** Delete File: missing.txt\n*** Add File: missing.txt\n+new\n",
        "*** Update File: one.txt\n@@\n-original\n+collision\n",
        "*** Add File: created.txt\n+new\n*** Update File: missing.txt\n@@\n-old\n+new\n",
    ] {
        let patch = format!(
            "*** Begin Patch\n*** Delete File: one.txt\n*** Add File: one.txt\n+changed\n{invalid}*** End Patch\n"
        );
        assert!(
            executor(workspace.path())
                .execute(
                    request("apply_patch", json!({"patch": patch})),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("one.txt")).unwrap(),
            "original\n"
        );
        assert!(!workspace.path().join("missing.txt").exists());
        assert!(!workspace.path().join("created.txt").exists());
    }
}

#[test]
fn replacement_patch_does_not_relax_other_duplicate_or_alias_rules() {
    for entries in [
        "*** Add File: a\n+new\n*** Delete File: a\n",
        "*** Delete File: a\n*** Add File: dir/../a\n+new\n",
        "*** Delete File: a\n*** Add File: a\n+new\n*** Add File: a\n+again\n",
        "*** Update File: a\n*** Move to: b\n@@\n-old\n+new\n*** Delete File: b\n*** Add File: b\n+other\n",
    ] {
        assert!(model_patch::parse(&format!("*** Begin Patch\n{entries}*** End Patch\n")).is_err());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn replacement_patch_preserves_existing_file_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let workspace = tempdir().unwrap();
    let path = workspace.path().join("run.sh");
    fs::write(&path, "old\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    let executor = executor(workspace.path());
    let patch =
        "*** Begin Patch\n*** Delete File: run.sh\n*** Add File: run.sh\n+new\n*** End Patch\n";
    let report = executor
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o755
    );
}
