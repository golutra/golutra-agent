//! Stable child identities and a bounded durable mailbox. Neither starts execution.
use super::*;
use serde_json::Value;

pub(crate) const SCHEMA: &[&str] = &[
    "CREATE TABLE subagents (child_session_id TEXT PRIMARY KEY, parent_session_id TEXT NOT NULL, name TEXT NOT NULL, UNIQUE(parent_session_id,name))",
    "CREATE INDEX idx_subagents_parent ON subagents (parent_session_id)",
    "CREATE TABLE subagent_messages (message_id TEXT PRIMARY KEY, parent_session_id TEXT NOT NULL, child_session_id TEXT NOT NULL, body TEXT NOT NULL, delivered_task_id TEXT)",
    "CREATE INDEX idx_subagent_messages_pending ON subagent_messages (child_session_id,delivered_task_id)",
    "INSERT INTO subagents (child_session_id,parent_session_id,name)
        SELECT child.session_id,parent.session_id,'agent-' || child.session_id FROM threads child
        JOIN threads parent ON child.parent_thread_id=parent.thread_id
        WHERE EXISTS (SELECT 1 FROM runtime_events event WHERE event.session_id=child.session_id AND event.event_type='TaskCreated' AND json_extract(event.payload_json,'$.payload._delegated_task')=1)",
];

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubagentRecord {
    pub cursor: u64,
    pub name: String,
    pub thread: ThreadRecord,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubagentMessage {
    pub message_id: String,
    pub parent_session_id: SessionId,
    pub content: String,
}

impl RuntimeStore {
    pub async fn subagent_question(
        &self,
        child: SessionId,
        task: TaskId,
    ) -> StoreResult<Option<Value>> {
        let json:Option<String>=sqlx::query_scalar("SELECT json_extract(payload_json,'$.envelope.structured_facts') FROM runtime_events
            WHERE session_id=? AND task_id=? AND event_type='ToolCompleted' AND json_extract(payload_json,'$.envelope.structured_facts.needs_parent_input')=1 ORDER BY sequence_no DESC LIMIT 1")
            .bind(child.to_string()).bind(task.to_string()).fetch_optional(&self.pool).await?;
        json.map(|value| serde_json::from_str(&value).map_err(StoreError::from))
            .transpose()
    }
    pub async fn subagent_name(
        &self,
        parent: SessionId,
        child: SessionId,
    ) -> StoreResult<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT name FROM subagents WHERE parent_session_id=? AND child_session_id=?",
        )
        .bind(parent.to_string())
        .bind(child.to_string())
        .fetch_optional(&self.pool)
        .await?)
    }
    pub async fn register_subagent(
        &self,
        parent: SessionId,
        child: SessionId,
        name: &str,
    ) -> StoreResult<()> {
        if name.trim().is_empty()
            || name.trim() != name
            || name.chars().count() > 80
            || name.chars().any(char::is_control)
        {
            return Err(StoreError::InvalidBackgroundOperation(
                "invalid subagent name".into(),
            ));
        }
        sqlx::query("INSERT OR IGNORE INTO subagents (child_session_id,parent_session_id,name)
            SELECT child.session_id,parent.session_id,? FROM threads child JOIN threads parent ON child.parent_thread_id=parent.thread_id
            WHERE child.session_id=? AND parent.session_id=? AND child.removed=0 AND parent.removed=0 AND child.workspace_root IS parent.workspace_root")
            .bind(name).bind(child.to_string()).bind(parent.to_string()).execute(&self.pool).await?;
        let existing: Option<(String, String)> =
            sqlx::query_as("SELECT parent_session_id,name FROM subagents WHERE child_session_id=?")
                .bind(child.to_string())
                .fetch_optional(&self.pool)
                .await?;
        if existing != Some((parent.to_string(), name.into())) {
            return Err(StoreError::InvalidBackgroundOperation(
                "subagent name is already used or identity belongs to another parent".into(),
            ));
        }
        Ok(())
    }

    pub async fn subagents(
        &self,
        parent: SessionId,
        after: u64,
        limit: u32,
    ) -> StoreResult<Vec<SubagentRecord>> {
        let rows = sqlx::query("SELECT a.rowid AS agent_cursor,a.name,t.* FROM subagents a
            JOIN threads t ON t.session_id=a.child_session_id JOIN threads p ON p.session_id=a.parent_session_id
            WHERE a.parent_session_id=? AND t.parent_thread_id=p.thread_id AND t.removed=0 AND a.rowid>?
            ORDER BY a.rowid LIMIT ?")
            .bind(parent.to_string()).bind(i64::try_from(after).unwrap_or(i64::MAX)).bind(i64::from(limit.min(65)))
            .fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| {
                Ok(SubagentRecord {
                    cursor: row.try_get::<i64, _>("agent_cursor")? as u64,
                    name: row.try_get("name")?,
                    thread: thread_from_row(row)?,
                })
            })
            .collect()
    }

    pub async fn enqueue_subagent_message(
        &self,
        parent: SessionId,
        child: SessionId,
        id: &str,
        body: &str,
    ) -> StoreResult<bool> {
        if body.trim().is_empty() || body.len() > 8 * 1024 || id.is_empty() || id.len() > 128 {
            return Err(StoreError::InvalidBackgroundOperation(
                "subagent message is empty or too large".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE subagents SET name=name WHERE child_session_id=?")
            .bind(child.to_string())
            .execute(&mut *tx)
            .await?;
        let owner: Option<String> =
            sqlx::query_scalar("SELECT parent_session_id FROM subagents WHERE child_session_id=?")
                .bind(child.to_string())
                .fetch_optional(&mut *tx)
                .await?;
        if owner.as_deref() != Some(&parent.to_string()) {
            return Err(StoreError::InvalidBackgroundOperation(
                "message target is not owned by this parent".into(),
            ));
        }
        let existing:Option<(String,String,String)>=sqlx::query_as("SELECT parent_session_id,child_session_id,body FROM subagent_messages WHERE message_id=?")
            .bind(id).fetch_optional(&mut *tx).await?;
        if let Some(existing) = existing {
            if existing != (parent.to_string(), child.to_string(), body.into()) {
                return Err(StoreError::InvalidBackgroundOperation(
                    "message identity reused with different content".into(),
                ));
            }
            tx.commit().await?;
            return Ok(false);
        }
        let pending:i64=sqlx::query_scalar("SELECT count(*) FROM subagent_messages WHERE child_session_id=? AND delivered_task_id IS NULL")
            .bind(child.to_string()).fetch_one(&mut *tx).await?;
        if pending >= 128 {
            return Err(StoreError::InvalidBackgroundOperation(
                "subagent mailbox is full; consume pending messages".into(),
            ));
        }
        sqlx::query("INSERT INTO subagent_messages (message_id,parent_session_id,child_session_id,body) VALUES (?,?,?,?)")
            .bind(id).bind(parent.to_string()).bind(child.to_string()).bind(body).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn pending_subagent_messages(
        &self,
        child: SessionId,
    ) -> StoreResult<Vec<SubagentMessage>> {
        let rows=sqlx::query("SELECT message_id,parent_session_id,body FROM subagent_messages WHERE child_session_id=? AND delivered_task_id IS NULL ORDER BY rowid LIMIT 128")
            .bind(child.to_string()).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| {
                Ok(SubagentMessage {
                    message_id: row.try_get("message_id")?,
                    parent_session_id: SessionId::from_str(row.try_get("parent_session_id")?)
                        .map_err(|error| StoreError::InvalidId(error.to_string()))?,
                    content: row.try_get("body")?,
                })
            })
            .collect()
    }

    pub async fn pending_subagent_message_count(&self, child: SessionId) -> StoreResult<u64> {
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM subagent_messages WHERE child_session_id=? AND delivered_task_id IS NULL")
            .bind(child.to_string()).fetch_one(&self.pool).await?;
        Ok(count as u64)
    }

    pub async fn acknowledge_subagent_messages(
        &self,
        child: SessionId,
        task: TaskId,
        ids: &[String],
    ) -> StoreResult<()> {
        let mut tx = self.pool.begin().await?;
        for id in ids {
            sqlx::query("UPDATE subagent_messages SET delivered_task_id=? WHERE child_session_id=? AND message_id=? AND delivered_task_id IS NULL")
                .bind(task.to_string()).bind(child.to_string()).bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    async fn thread(store: &RuntimeStore, parent: Option<ThreadId>) -> ThreadRecord {
        let now = Utc::now();
        let thread = ThreadRecord {
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            parent_thread_id: parent,
            forked_from_turn_id: None,
            forked_from_sequence_no: None,
            workspace_root: Some("/workspace".into()),
            rebound_from_workspace_root: None,
            rollout_path: None,
            title: "child".into(),
            preview: "task".into(),
            created_at: now,
            updated_at: now,
            recency_at: now,
            archived: false,
            removed: false,
        };
        store.upsert_thread(&thread).await.unwrap();
        thread
    }

    #[tokio::test]
    async fn subagent_registry_pages_all_children_and_enforces_ownership_and_names() {
        let store = RuntimeStore::in_memory().await.unwrap();
        let parent = thread(&store, None).await;
        let other = thread(&store, None).await;
        for index in 0..35 {
            let child = thread(&store, Some(parent.thread_id)).await;
            let name = format!("worker-{index}");
            assert!(
                store
                    .register_subagent(other.session_id, child.session_id, &name)
                    .await
                    .is_err()
            );
            store
                .register_subagent(parent.session_id, child.session_id, &name)
                .await
                .unwrap();
            store
                .register_subagent(parent.session_id, child.session_id, &name)
                .await
                .unwrap();
        }
        let mut cursor = 0;
        let mut ids = std::collections::HashSet::new();
        loop {
            let page = store
                .subagents(parent.session_id, cursor, 16)
                .await
                .unwrap();
            if page.is_empty() {
                break;
            }
            for record in page {
                assert!(ids.insert(record.thread.session_id));
                cursor = record.cursor;
            }
        }
        assert_eq!(ids.len(), 35);
        assert!(
            store
                .subagents(other.session_id, 0, 64)
                .await
                .unwrap()
                .is_empty()
        );
        let child = thread(&store, Some(parent.thread_id)).await;
        assert!(
            store
                .register_subagent(parent.session_id, child.session_id, "worker-0")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn subagent_mailbox_is_bounded_idempotent_and_consumed_only_by_its_child() {
        let store = RuntimeStore::in_memory().await.unwrap();
        let parent = thread(&store, None).await;
        let child = thread(&store, Some(parent.thread_id)).await;
        store
            .register_subagent(parent.session_id, child.session_id, "worker")
            .await
            .unwrap();
        assert!(
            store
                .enqueue_subagent_message(SessionId::new(), child.session_id, "m0", "hello")
                .await
                .is_err()
        );
        assert!(
            store
                .enqueue_subagent_message(parent.session_id, child.session_id, "m0", "hello")
                .await
                .unwrap()
        );
        assert!(
            !store
                .enqueue_subagent_message(parent.session_id, child.session_id, "m0", "hello")
                .await
                .unwrap()
        );
        assert!(
            store
                .enqueue_subagent_message(parent.session_id, child.session_id, "m0", "changed")
                .await
                .is_err()
        );
        store
            .acknowledge_subagent_messages(SessionId::new(), TaskId::new(), &["m0".into()])
            .await
            .unwrap();
        assert_eq!(
            store
                .pending_subagent_messages(child.session_id)
                .await
                .unwrap()
                .len(),
            1
        );
        for index in 1..128 {
            store
                .enqueue_subagent_message(
                    parent.session_id,
                    child.session_id,
                    &format!("m{index}"),
                    "queued",
                )
                .await
                .unwrap();
        }
        assert!(
            store
                .enqueue_subagent_message(parent.session_id, child.session_id, "overflow", "queued")
                .await
                .is_err()
        );
        store
            .acknowledge_subagent_messages(child.session_id, TaskId::new(), &["m0".into()])
            .await
            .unwrap();
        assert!(
            !store
                .enqueue_subagent_message(parent.session_id, child.session_id, "m0", "hello")
                .await
                .unwrap()
        );
        assert_eq!(
            store
                .pending_subagent_messages(child.session_id)
                .await
                .unwrap()
                .len(),
            127
        );
        store
            .enqueue_subagent_message(parent.session_id, child.session_id, "overflow", "queued")
            .await
            .unwrap();
    }
}
