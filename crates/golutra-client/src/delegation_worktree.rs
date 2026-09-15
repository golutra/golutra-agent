use super::*;
use std::path::{Path, PathBuf};

fn managed_path(host: &RuntimeHost, child: SessionId) -> Result<PathBuf, ClientError> {
    host.runtime_paths
        .as_ref()
        .map(|paths| {
            paths
                .workspace_state_dir
                .join("worktrees")
                .join(child.to_string())
        })
        .ok_or_else(|| {
            ClientError::TaskExecution(
                "worktree isolation requires durable runtime storage".to_owned(),
            )
        })
}

async fn git(root: &Path, args: &[&str]) -> Result<String, ClientError> {
    let output = timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| ClientError::TaskExecution("git worktree operation timed out".to_owned()))?
    .map_err(|error| ClientError::Io(error.to_string()))?;
    if !output.status.success() {
        return Err(ClientError::TaskExecution(format!(
            "git worktree: {}",
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(2048)
                .collect::<String>()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn validate(root: &Path, path: &Path) -> Result<(), ClientError> {
    let canonical = path
        .canonicalize()
        .map_err(|error| ClientError::Io(error.to_string()))?;
    if canonical != path || !path.join(".git").is_file() {
        return Err(ClientError::TaskExecution(
            "managed worktree path was replaced".to_owned(),
        ));
    }
    let common_args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    let original = git(root, &common_args).await?;
    let child = git(path, &common_args).await?;
    if original != child || Path::new(&git(path, &["rev-parse", "--show-toplevel"]).await?) != path
    {
        return Err(ClientError::TaskExecution(
            "managed worktree belongs to a different repository".to_owned(),
        ));
    }
    Ok(())
}

pub(super) async fn prepare(
    host: &RuntimeHost,
    request: &ToolRequest,
    child: SessionId,
    resumed: bool,
) -> Result<Option<PathBuf>, ClientError> {
    let isolation = request
        .arguments
        .get("isolation")
        .and_then(Value::as_str)
        .unwrap_or("shared");
    if !matches!(isolation, "shared" | "worktree") {
        return Err(ClientError::TaskExecution(
            "isolation must be shared or worktree".to_owned(),
        ));
    }
    let root = host.execution_workspace_root()?;
    if resumed {
        let existing = workspace_root(host, child).await?;
        if request
            .arguments
            .get("isolation")
            .is_some_and(|value| !value.is_null())
            && (existing != root) != (isolation == "worktree")
        {
            return Err(ClientError::TaskExecution(
                "resume must preserve the child's workspace isolation".to_owned(),
            ));
        }
        return Ok((existing != root).then_some(existing));
    }
    if isolation == "shared" {
        return Ok(None);
    }
    let path = managed_path(host, child)?;
    if !path.exists() {
        let parent = path.parent().ok_or_else(|| {
            ClientError::TaskExecution("managed worktree parent is missing".to_owned())
        })?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| ClientError::Io(error.to_string()))?;
        if parent
            .canonicalize()
            .map_err(|error| ClientError::Io(error.to_string()))?
            != parent
        {
            return Err(ClientError::TaskExecution(
                "managed worktree directory was replaced".to_owned(),
            ));
        }
        git(
            &root,
            &[
                "worktree",
                "add",
                "--detach",
                path.to_str().ok_or_else(|| {
                    ClientError::TaskExecution("worktree path is not UTF-8".to_owned())
                })?,
                "HEAD",
            ],
        )
        .await?;
    }
    validate(&root, &path).await?;
    Ok(Some(path))
}

pub(crate) async fn workspace_root(
    host: &RuntimeHost,
    session: SessionId,
) -> Result<PathBuf, ClientError> {
    let root = host.execution_workspace_root()?;
    let Some(thread) = host
        .storage
        .repositories
        .threads
        .by_session(session)
        .await?
    else {
        return Ok(root);
    };
    if thread.parent_thread_id.is_none() || host.runtime_paths.is_none() {
        return Ok(root);
    }
    host.ensure_thread_in_workspace(&thread)?;
    let path = managed_path(host, session)?;
    if path.exists() {
        validate(&root, &path).await?;
        return Ok(path);
    }
    let events = host
        .storage
        .repositories
        .events
        .load(session, None, None)
        .await?;
    if events.iter().any(|event| {
        event.event_type == RuntimeEventType::TaskCreated
            && event
                .payload
                .pointer("/payload/_delegation_isolation")
                .and_then(Value::as_str)
                == Some("worktree")
    }) {
        return Err(ClientError::TaskExecution(
            "child worktree is missing; restore it before resuming".to_owned(),
        ));
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn worktree_keeps_parent_dirty_changes_separate_and_resumes_the_same_checkout() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]).await.unwrap();
        std::fs::write(root.path().join("file.txt"), "committed").unwrap();
        git(root.path(), &["add", "file.txt"]).await.unwrap();
        git(
            root.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "fixture",
            ],
        )
        .await
        .unwrap();
        std::fs::write(root.path().join("file.txt"), "parent dirty").unwrap();
        let host = RuntimeHost::from_home_and_cwd(home.path(), root.path())
            .await
            .unwrap();
        let child = SessionId::new();
        let request = ToolRequest {
            tool_call_id: ToolCallId::new(),
            provider_tool_call_id: None,
            session_id: host.default_session_id(),
            turn_id: None,
            tool_name: "subagent".to_owned(),
            arguments: json!({"task":"edit","isolation":"worktree"}),
        };
        let checkout = prepare(&host, &request, child, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(checkout.join("file.txt")).unwrap(),
            "committed"
        );
        std::fs::write(checkout.join("file.txt"), "child dirty").unwrap();
        let now = Utc::now();
        host.storage
            .repositories
            .threads
            .upsert(&golutra_store::ThreadRecord {
                thread_id: ThreadId::new(),
                session_id: child,
                parent_thread_id: Some(host.default_thread_id()),
                forked_from_turn_id: None,
                forked_from_sequence_no: None,
                workspace_root: host.workspace_root_string(),
                rebound_from_workspace_root: None,
                rollout_path: None,
                title: "child".to_owned(),
                preview: String::new(),
                created_at: now,
                updated_at: now,
                recency_at: now,
                archived: false,
                removed: false,
            })
            .await
            .unwrap();
        assert_eq!(workspace_root(&host, child).await.unwrap(), checkout);
        let mut resumed = request.clone();
        resumed.arguments = json!({"action":"resume","task":"continue"});
        assert_eq!(
            prepare(&host, &resumed, child, true).await.unwrap(),
            Some(checkout.clone())
        );
        resumed.arguments["isolation"] = Value::Null;
        assert_eq!(
            prepare(&host, &resumed, child, true).await.unwrap(),
            Some(checkout.clone())
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("file.txt")).unwrap(),
            "parent dirty"
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("file.txt")).unwrap(),
            "child dirty"
        );
        resumed.arguments["isolation"] = json!("shared");
        assert!(prepare(&host, &resumed, child, true).await.is_err());
        host.close().await.unwrap();
        drop(host);
        let host = RuntimeHost::from_home_and_cwd(home.path(), root.path())
            .await
            .unwrap();
        assert_eq!(workspace_root(&host, child).await.unwrap(), checkout);
        host.record_event(crate::event_codec::host_event(
            host.next_sequence_no(),
            child,
            Some(golutra_core::TaskId::new()),
            RuntimeEventType::TaskCreated,
            golutra_protocol::RuntimeEventSource::Runtime,
            json!({"payload":{"_delegation_isolation":"worktree"}}),
        ))
        .await
        .unwrap();
        let relocated = checkout.with_extension("relocated");
        std::fs::rename(&checkout, &relocated).unwrap();
        assert!(
            workspace_root(&host, child)
                .await
                .unwrap_err()
                .to_string()
                .contains("worktree is missing")
        );
        std::fs::create_dir(&checkout).unwrap();
        assert!(
            workspace_root(&host, child)
                .await
                .unwrap_err()
                .to_string()
                .contains("path was replaced")
        );
        std::fs::remove_dir(&checkout).unwrap();
        std::fs::rename(&relocated, &checkout).unwrap();
        host.close().await.unwrap();
    }
}
