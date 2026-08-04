//! Prompt history, workspace mentions, attachments, and queued-turn projections.

use std::{
    collections::{HashMap, VecDeque},
    fs,
    ops::Range,
    path::{Path, PathBuf},
};

use golutra_core::TurnId;
use golutra_protocol::{RuntimeEvent, RuntimeEventType};
use ignore::WalkBuilder;
use serde_json::Value;

use super::ComposerInput;

const MAX_PROMPT_HISTORY: usize = 200;
const MAX_MENTION_FILES: usize = 4_000;
const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub(crate) struct PromptHistory {
    entries: VecDeque<String>,
    cursor: Option<usize>,
    draft: String,
}

impl PromptHistory {
    pub(crate) fn record(&mut self, prompt: &str) {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return;
        }
        if self.entries.back().is_none_or(|entry| entry != prompt) {
            self.entries.push_back(prompt.to_owned());
            if self.entries.len() > MAX_PROMPT_HISTORY {
                self.entries.pop_front();
            }
        }
        self.cursor = None;
        self.draft.clear();
    }

    pub(crate) fn previous(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let next = match self.cursor {
            Some(cursor) => cursor.saturating_sub(1),
            None => {
                self.draft = current.to_owned();
                self.entries.len().saturating_sub(1)
            }
        };
        self.cursor = Some(next);
        self.entries.get(next).cloned()
    }

    pub(crate) fn next(&mut self) -> Option<String> {
        let cursor = self.cursor?;
        if cursor + 1 < self.entries.len() {
            self.cursor = Some(cursor + 1);
            self.entries.get(cursor + 1).cloned()
        } else {
            self.cursor = None;
            Some(std::mem::take(&mut self.draft))
        }
    }

    pub(crate) fn reset_navigation(&mut self) {
        self.cursor = None;
        self.draft.clear();
    }

    pub(crate) fn search(&self, query: &str) -> Vec<String> {
        let query = query.trim().to_lowercase();
        self.entries
            .iter()
            .rev()
            .filter(|entry| query.is_empty() || entry.to_lowercase().contains(&query))
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct HistorySearchState {
    pub(crate) input: ComposerInput,
    pub(crate) matches: Vec<String>,
    pub(crate) selected: usize,
}

impl HistorySearchState {
    pub(crate) fn rebuild(&mut self, history: &PromptHistory) {
        self.matches = history.search(self.input.text());
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    pub(crate) fn move_selection(&mut self, forward: bool) {
        if self.matches.is_empty() {
            self.selected = 0;
        } else if forward {
            self.selected = (self.selected + 1) % self.matches.len();
        } else {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or_else(|| self.matches.len().saturating_sub(1));
        }
    }

    pub(crate) fn selected(&self) -> Option<&str> {
        self.matches.get(self.selected).map(String::as_str)
    }

    pub(crate) fn status(&self) -> String {
        if self.matches.is_empty() {
            "no history match".to_owned()
        } else {
            format!("{} of {}", self.selected + 1, self.matches.len())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MentionKind {
    File,
    Skill,
    App,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MentionCandidate {
    pub(crate) kind: MentionKind,
    pub(crate) label: String,
    pub(crate) insertion: String,
    pub(crate) detail: String,
    pub(crate) source_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MentionCatalog {
    candidates: Vec<MentionCandidate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MentionCompletion {
    pub(crate) replacement: Range<usize>,
    pub(crate) candidates: Vec<MentionCandidate>,
    pub(crate) selected: usize,
}

impl MentionCatalog {
    pub(crate) fn discover(workspace: &Path) -> Self {
        let mut candidates = Vec::new();
        let walker = WalkBuilder::new(workspace)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .max_depth(Some(12))
            .build();
        for entry in walker.filter_map(Result::ok) {
            if candidates.len() >= MAX_MENTION_FILES {
                break;
            }
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                continue;
            }
            let Ok(relative) = entry.path().strip_prefix(workspace) else {
                continue;
            };
            let path = relative.to_string_lossy().replace('\\', "/");
            candidates.push(MentionCandidate {
                kind: MentionKind::File,
                label: path.clone(),
                insertion: format!("@{path}"),
                detail: "file".to_owned(),
                source_path: Some(entry.path().to_path_buf()),
            });
        }
        discover_special_mentions(workspace, &mut candidates);
        candidates.sort_by(|left, right| {
            mention_rank(left.kind)
                .cmp(&mention_rank(right.kind))
                .then_with(|| left.label.cmp(&right.label))
        });
        candidates.dedup_by(|left, right| left.insertion == right.insertion);
        Self { candidates }
    }

    pub(crate) fn complete(&self, input: &ComposerInput) -> Option<MentionCompletion> {
        let replacement = active_mention_range(input.text(), input.cursor())?;
        let query = input.text()[replacement.start + 1..replacement.end].to_lowercase();
        let candidates = self
            .candidates
            .iter()
            .filter(|candidate| mention_matches(candidate, &query))
            .take(12)
            .cloned()
            .collect::<Vec<_>>();
        (!candidates.is_empty()).then_some(MentionCompletion {
            replacement,
            candidates,
            selected: 0,
        })
    }
}

impl MentionCompletion {
    pub(crate) fn move_selection(&mut self, forward: bool) {
        if self.candidates.is_empty() {
            self.selected = 0;
        } else if forward {
            self.selected = (self.selected + 1) % self.candidates.len();
        } else {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or_else(|| self.candidates.len().saturating_sub(1));
        }
    }

    pub(crate) fn selected(&self) -> Option<&MentionCandidate> {
        self.candidates.get(self.selected)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachmentKind {
    Image,
    Text,
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposerAttachment {
    pub(crate) path: PathBuf,
    pub(crate) display_path: String,
    pub(crate) kind: AttachmentKind,
    pub(crate) bytes: u64,
}

pub(crate) fn attachment_from_path(
    workspace: &Path,
    value: &str,
) -> Result<ComposerAttachment, String> {
    let requested = Path::new(value.trim());
    let path = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        workspace.join(requested)
    };
    let workspace = workspace
        .canonicalize()
        .map_err(|error| format!("resolve workspace: {error}"))?;
    let path = path
        .canonicalize()
        .map_err(|error| format!("resolve attachment: {error}"))?;
    if !path.starts_with(&workspace) {
        return Err("attachment must be inside the workspace".to_owned());
    }
    let metadata = fs::metadata(&path).map_err(|error| format!("read attachment: {error}"))?;
    if !metadata.is_file() {
        return Err("attachment must be a file".to_owned());
    }
    if metadata.len() > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "attachment exceeds {} MiB",
            MAX_ATTACHMENT_BYTES / (1024 * 1024)
        ));
    }
    let relative = path
        .strip_prefix(&workspace)
        .unwrap_or(&path)
        .to_string_lossy()
        .replace('\\', "/");
    Ok(ComposerAttachment {
        kind: attachment_kind(&path),
        path,
        display_path: relative,
        bytes: metadata.len(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueuedPrompt {
    pub(crate) turn_id: TurnId,
    pub(crate) prompt: String,
    pub(crate) steer: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct QueuePickerState {
    pub(crate) items: Vec<QueuedPrompt>,
    pub(crate) selected: usize,
}

impl QueuePickerState {
    pub(crate) fn selected(&self) -> Option<&QueuedPrompt> {
        self.items.get(self.selected)
    }

    pub(crate) fn move_selection(&mut self, forward: bool) {
        if self.items.is_empty() {
            self.selected = 0;
        } else if forward {
            self.selected = (self.selected + 1).min(self.items.len().saturating_sub(1));
        } else {
            self.selected = self.selected.saturating_sub(1);
        }
    }

    pub(crate) fn select_first(&mut self) {
        self.selected = 0;
    }

    pub(crate) fn select_last(&mut self) {
        self.selected = self.items.len().saturating_sub(1);
    }
}

pub(crate) fn queued_prompts(events: &[RuntimeEvent]) -> Vec<QueuedPrompt> {
    let mut queued = Vec::<QueuedPrompt>::new();
    let mut positions = HashMap::<TurnId, usize>::new();
    let mut ordered = events.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|event| event.sequence_no);
    for event in ordered {
        match event.event_type {
            RuntimeEventType::TurnQueued | RuntimeEventType::TurnUpdated => {
                let Some(turn_id) = event.turn_id else {
                    continue;
                };
                let prompt = event
                    .payload
                    .pointer("/payload/prompt")
                    .or_else(|| event.payload.get("prompt"))
                    .and_then(Value::as_str)
                    .unwrap_or("queued prompt")
                    .to_owned();
                let steer = event
                    .payload
                    .pointer("/payload/steer")
                    .or_else(|| event.payload.get("steer"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let updated = QueuedPrompt {
                    turn_id,
                    prompt,
                    steer,
                };
                if let Some(index) = positions.get(&turn_id).copied() {
                    queued[index] = updated;
                } else {
                    positions.insert(turn_id, queued.len());
                    queued.push(updated);
                }
            }
            RuntimeEventType::TurnStarted | RuntimeEventType::TurnCancelled => {
                if let Some(turn_id) = event.turn_id
                    && let Some(index) = positions.remove(&turn_id)
                {
                    queued.remove(index);
                    positions = queued
                        .iter()
                        .enumerate()
                        .map(|(index, queued)| (queued.turn_id, index))
                        .collect();
                }
            }
            RuntimeEventType::TaskAborted | RuntimeEventType::TaskInterrupted => {
                queued.clear();
                positions.clear();
            }
            _ => {}
        }
    }
    queued
}

fn active_mention_range(text: &str, cursor: usize) -> Option<Range<usize>> {
    let prefix = text.get(..cursor)?;
    let start = prefix.char_indices().rev().find_map(|(index, character)| {
        if character == '@' {
            Some(index)
        } else if character.is_whitespace() {
            Some(usize::MAX)
        } else {
            None
        }
    })?;
    (start != usize::MAX).then_some(start..cursor)
}

pub(crate) fn mention_is_active(input: &ComposerInput) -> bool {
    active_mention_range(input.text(), input.cursor()).is_some()
}

fn mention_matches(candidate: &MentionCandidate, query: &str) -> bool {
    let label = candidate.label.to_lowercase();
    let insertion = candidate.insertion.trim_start_matches('@').to_lowercase();
    label.contains(query) || insertion.contains(query)
}

fn mention_rank(kind: MentionKind) -> u8 {
    match kind {
        MentionKind::Skill => 0,
        MentionKind::App => 1,
        MentionKind::File => 2,
    }
}

fn discover_special_mentions(workspace: &Path, candidates: &mut Vec<MentionCandidate>) {
    let walker = WalkBuilder::new(workspace)
        .hidden(false)
        .git_ignore(true)
        .max_depth(Some(8))
        .build();
    for entry in walker.filter_map(Result::ok) {
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if name == "SKILL.md" {
            let Some(parent) = entry.path().parent().and_then(Path::file_name) else {
                continue;
            };
            let name = parent.to_string_lossy().into_owned();
            candidates.push(MentionCandidate {
                kind: MentionKind::Skill,
                label: name.clone(),
                insertion: format!("@skill:{name}"),
                detail: "skill".to_owned(),
                source_path: Some(entry.path().to_path_buf()),
            });
        } else if name == "plugin.json"
            && entry
                .path()
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|directory| directory == ".codex-plugin")
            && let Ok(value) = fs::read_to_string(entry.path())
            && let Ok(manifest) = serde_json::from_str::<Value>(&value)
            && let Some(name) = manifest.get("name").and_then(Value::as_str)
        {
            candidates.push(MentionCandidate {
                kind: MentionKind::App,
                label: name.to_owned(),
                insertion: format!("@app:{name}"),
                detail: "app".to_owned(),
                source_path: Some(entry.path().to_path_buf()),
            });
        }
    }
    let mcp_path = workspace.join(".mcp.json");
    if let Ok(value) = fs::read_to_string(&mcp_path)
        && let Ok(config) = serde_json::from_str::<Value>(&value)
        && let Some(servers) = config.get("mcpServers").and_then(Value::as_object)
    {
        candidates.extend(servers.keys().map(|name| MentionCandidate {
            kind: MentionKind::App,
            label: name.clone(),
            insertion: format!("@app:{name}"),
            detail: "MCP app".to_owned(),
            source_path: Some(mcp_path.clone()),
        }));
    }
}

fn attachment_kind(path: &Path) -> AttachmentKind {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("avif" | "gif" | "heic" | "jpeg" | "jpg" | "png" | "webp") => AttachmentKind::Image,
        Some(
            "c" | "cc" | "cpp" | "css" | "go" | "h" | "hpp" | "html" | "java" | "js" | "json"
            | "md" | "py" | "rb" | "rs" | "sh" | "toml" | "ts" | "txt" | "xml" | "yaml" | "yml",
        ) => AttachmentKind::Text,
        _ => AttachmentKind::Binary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use golutra_core::{EventId, SessionId};
    use golutra_protocol::RuntimeEventSource;
    use serde_json::json;
    use tempfile::tempdir;

    fn queue_event(
        sequence_no: u64,
        turn_id: TurnId,
        event_type: RuntimeEventType,
        payload: Value,
    ) -> RuntimeEvent {
        RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: EventId::new(),
            sequence_no,
            session_id: SessionId::new(),
            turn_id: Some(turn_id),
            task_id: None,
            parent_event_id: None,
            event_type,
            timestamp: Utc::now(),
            source: RuntimeEventSource::User,
            payload,
            payload_ref: None,
            durable: true,
        }
    }

    #[test]
    fn history_restores_the_unsent_draft_after_navigation() {
        let mut history = PromptHistory::default();
        history.record("first");
        history.record("second");
        assert_eq!(history.previous("draft").as_deref(), Some("second"));
        assert_eq!(history.previous("second").as_deref(), Some("first"));
        assert_eq!(history.next().as_deref(), Some("second"));
        assert_eq!(history.next().as_deref(), Some("draft"));
    }

    #[test]
    fn mention_catalog_finds_files_skills_and_apps() {
        let workspace = tempdir().expect("workspace");
        fs::create_dir_all(workspace.path().join(".agents/skills/review")).expect("skill dir");
        fs::write(workspace.path().join("src.rs"), "fn main() {}").expect("source");
        fs::write(
            workspace.path().join(".agents/skills/review/SKILL.md"),
            "# Review",
        )
        .expect("skill");
        fs::write(
            workspace.path().join(".mcp.json"),
            r#"{"mcpServers":{"browser":{}}}"#,
        )
        .expect("mcp config");
        let catalog = MentionCatalog::discover(workspace.path());

        let mut input = ComposerInput::default();
        input.set_text("inspect @skill:rev");
        let completion = catalog.complete(&input).expect("skill completion");
        assert_eq!(
            completion.selected().expect("skill").insertion,
            "@skill:review"
        );

        input.set_text("use @app:bro");
        assert_eq!(
            catalog
                .complete(&input)
                .and_then(|completion| completion.selected().cloned())
                .map(|candidate| candidate.insertion),
            Some("@app:browser".to_owned())
        );
    }

    #[test]
    fn attachments_are_workspace_bounded_and_typed() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("screen.png"), b"png").expect("image");
        let attachment = attachment_from_path(workspace.path(), "screen.png").expect("attachment");
        assert_eq!(attachment.kind, AttachmentKind::Image);
        assert_eq!(attachment.display_path, "screen.png");

        let outside = tempdir().expect("outside");
        fs::write(outside.path().join("secret.txt"), "secret").expect("outside file");
        assert!(
            attachment_from_path(
                workspace.path(),
                outside.path().join("secret.txt").to_str().expect("path")
            )
            .is_err()
        );
    }

    #[test]
    fn queued_prompt_edits_keep_order_and_cancellations_remove_items() {
        let first = TurnId::new();
        let second = TurnId::new();
        let events = vec![
            queue_event(
                4,
                second,
                RuntimeEventType::TurnQueued,
                json!({"payload": {"prompt": "second", "steer": true}}),
            ),
            queue_event(
                1,
                first,
                RuntimeEventType::TurnQueued,
                json!({"payload": {"prompt": "first"}}),
            ),
            queue_event(
                5,
                first,
                RuntimeEventType::TurnUpdated,
                json!({"payload": {"prompt": "first edited"}}),
            ),
        ];

        let queued = queued_prompts(&events);
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[0].turn_id, first);
        assert_eq!(queued[0].prompt, "first edited");
        assert_eq!(queued[1].turn_id, second);
        assert!(queued[1].steer);

        let mut cancelled = events;
        cancelled.push(queue_event(
            6,
            first,
            RuntimeEventType::TurnCancelled,
            json!({}),
        ));
        assert_eq!(queued_prompts(&cancelled), vec![queued[1].clone()]);
    }

    #[test]
    fn queue_picker_selection_is_bounded() {
        let turn_id = TurnId::new();
        let mut picker = QueuePickerState {
            items: vec![QueuedPrompt {
                turn_id,
                prompt: "queued".to_owned(),
                steer: false,
            }],
            selected: 0,
        };

        picker.move_selection(false);
        assert_eq!(picker.selected, 0);
        picker.move_selection(true);
        assert_eq!(picker.selected, 0);
        picker.select_last();
        assert_eq!(picker.selected().map(|item| item.turn_id), Some(turn_id));
    }
}
