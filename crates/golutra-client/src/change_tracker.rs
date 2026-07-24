use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

use golutra_core::{
    FileChangeKind, FileChangeSummary, FileContentKind, FileDiffPreview, FileStateMetadata, TaskId,
    TurnChangeSummary, TurnId,
};
use golutra_tools::{FileBeforeImage, ToolExecutionReport, redact_sensitive_text};
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};

const MAX_DIFF_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TURN_CONTENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_DIFF_PREVIEW_LINES: usize = 80;
const MAX_DIFF_PREVIEW_BYTES: usize = 24 * 1024;
const MAX_DIFF_PREVIEW_TOTAL_BYTES: usize = 256 * 1024;
const MAX_DIFF_ARTIFACT_BYTES: usize = 2 * 1024 * 1024;
const DIFF_ARTIFACT_TRUNCATION_MARKER: &str = "\n[diff artifact truncated]\n";

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileContent {
    Missing,
    Text(Vec<u8>),
    Unavailable,
}

impl FileContent {
    fn byte_len(&self) -> usize {
        match self {
            Self::Text(bytes) => bytes.len(),
            Self::Missing | Self::Unavailable => 0,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ChangeSample {
    path: String,
    before: FileContent,
    after: FileContent,
    before_metadata: Option<FileStateMetadata>,
    after_metadata: Option<FileStateMetadata>,
}

#[derive(Debug)]
struct TrackedFile {
    baseline: FileContent,
    latest: FileContent,
    baseline_metadata: Option<FileStateMetadata>,
    latest_metadata: Option<FileStateMetadata>,
}

#[derive(Debug, Default)]
struct TurnChangeState {
    files: BTreeMap<String, TrackedFile>,
    retained_content_bytes: usize,
}

#[derive(Debug, Default)]
pub(crate) struct WorkspaceChangeTracker {
    turns: HashMap<(TaskId, TurnId), TurnChangeState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolChangeFacts {
    pub(crate) operation_changes: Vec<FileChangeSummary>,
    pub(crate) diff_previews: Vec<FileDiffPreview>,
    pub(crate) diff_artifact: Option<DiffArtifact>,
    pub(crate) turn_summary: TurnChangeSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffArtifact {
    pub(crate) content: String,
    pub(crate) truncated: bool,
}

impl WorkspaceChangeTracker {
    pub(crate) fn record(
        &mut self,
        task_id: TaskId,
        turn_id: TurnId,
        samples: Vec<ChangeSample>,
    ) -> ToolChangeFacts {
        let turn = self.turns.entry((task_id, turn_id)).or_default();
        let mut delta_samples = Vec::with_capacity(samples.len());
        for sample in samples {
            if let Some(tracked) = turn.files.get_mut(&sample.path) {
                delta_samples.push(ChangeSample {
                    path: sample.path.clone(),
                    before: tracked.latest.clone(),
                    after: sample.after.clone(),
                    before_metadata: tracked.latest_metadata.clone(),
                    after_metadata: sample.after_metadata.clone(),
                });
                turn.retained_content_bytes = turn
                    .retained_content_bytes
                    .saturating_sub(tracked.latest.byte_len());
                tracked.latest = retain_bounded_content(
                    sample.after,
                    &mut turn.retained_content_bytes,
                    MAX_TURN_CONTENT_BYTES,
                );
                tracked.latest_metadata = sample.after_metadata;
                continue;
            }
            delta_samples.push(sample.clone());
            let baseline = retain_bounded_content(
                sample.before,
                &mut turn.retained_content_bytes,
                MAX_TURN_CONTENT_BYTES,
            );
            let latest = retain_bounded_content(
                sample.after,
                &mut turn.retained_content_bytes,
                MAX_TURN_CONTENT_BYTES,
            );
            turn.files.insert(
                sample.path,
                TrackedFile {
                    baseline,
                    latest,
                    baseline_metadata: sample.before_metadata,
                    latest_metadata: sample.after_metadata,
                },
            );
        }
        let operation_changes = delta_samples
            .iter()
            .filter_map(sample_summary)
            .collect::<Vec<_>>();
        let diff_previews = bounded_diff_previews(&delta_samples);
        let diff_artifact = build_diff_artifact(&delta_samples);

        ToolChangeFacts {
            operation_changes,
            diff_previews,
            diff_artifact,
            turn_summary: turn_summary(turn),
        }
    }

    pub(crate) fn remove_task(&mut self, task_id: TaskId) {
        self.turns
            .retain(|(tracked_task_id, _), _| *tracked_task_id != task_id);
    }
}

fn retain_bounded_content(
    content: FileContent,
    retained_bytes: &mut usize,
    limit: usize,
) -> FileContent {
    let content_bytes = content.byte_len();
    if retained_bytes.saturating_add(content_bytes) > limit {
        return FileContent::Unavailable;
    }
    *retained_bytes = retained_bytes.saturating_add(content_bytes);
    content
}

pub(crate) async fn capture_change_samples(
    workspace_root: Option<&Path>,
    report: &ToolExecutionReport,
) -> Vec<ChangeSample> {
    let mut samples = Vec::with_capacity(report.changed_files.len());
    for path in &report.changed_files {
        let before = report
            .before_images
            .iter()
            .find(|image| image.path == *path)
            .map(file_content_from_before_image)
            .unwrap_or(FileContent::Unavailable);
        let before_metadata = report
            .before_images
            .iter()
            .find(|image| image.path == *path)
            .and_then(|image| image.metadata.clone());
        let (after, after_metadata) =
            if let Some(image) = report.after_images.iter().find(|image| image.path == *path) {
                (
                    file_content_from_before_image(image),
                    image.metadata.clone(),
                )
            } else {
                read_after_content(workspace_root, report, path).await
            };
        samples.push(ChangeSample {
            path: display_path(workspace_root, path),
            before,
            after,
            before_metadata,
            after_metadata,
        });
    }
    samples.sort_by(|left, right| left.path.cmp(&right.path));
    samples.dedup_by(|left, right| left.path == right.path);
    samples
}

fn file_content_from_before_image(image: &FileBeforeImage) -> FileContent {
    match image.content.as_ref() {
        Some(bytes) if bytes.len() <= MAX_DIFF_FILE_BYTES => FileContent::Text(bytes.clone()),
        Some(_) => FileContent::Unavailable,
        None if image.metadata.is_some() => FileContent::Unavailable,
        None => FileContent::Missing,
    }
}

async fn read_after_content(
    workspace_root: Option<&Path>,
    report: &ToolExecutionReport,
    path: &Path,
) -> (FileContent, Option<FileStateMetadata>) {
    if let Some(root) = workspace_root
        && path.strip_prefix(root).is_ok()
    {
        return match tokio::fs::metadata(path).await {
            Ok(metadata) if metadata.len() <= MAX_DIFF_FILE_BYTES as u64 => {
                match tokio::fs::read(path).await {
                    Ok(bytes) if bytes.len() <= MAX_DIFF_FILE_BYTES => {
                        let state = state_metadata(&bytes, unix_mode(&metadata), true);
                        (FileContent::Text(bytes), Some(state))
                    }
                    Ok(_) | Err(_) => (FileContent::Unavailable, None),
                }
            }
            Ok(metadata) => (
                FileContent::Unavailable,
                Some(FileStateMetadata {
                    size_bytes: metadata.len(),
                    checksum: None,
                    unix_mode: unix_mode(&metadata),
                    content_kind: FileContentKind::Unknown,
                    content_available: false,
                }),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (FileContent::Missing, None)
            }
            Err(_) => (FileContent::Unavailable, None),
        };
    }

    if report.changed_files.len() == 1
        && let Some(content) = report.artifact_contents.first()
        && content.bytes.len() <= MAX_DIFF_FILE_BYTES
    {
        return (
            FileContent::Text(content.bytes.clone()),
            Some(state_metadata(&content.bytes, None, true)),
        );
    }
    (FileContent::Unavailable, None)
}

pub(crate) fn display_path(workspace_root: Option<&Path>, path: &Path) -> String {
    workspace_root
        .and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn sample_summary(sample: &ChangeSample) -> Option<FileChangeSummary> {
    proven_change(
        &sample.before,
        &sample.after,
        sample.before_metadata.as_ref(),
        sample.after_metadata.as_ref(),
    )
    .then(|| {
        change_summary(
            &sample.path,
            &sample.before,
            &sample.after,
            sample.before_metadata.clone(),
            sample.after_metadata.clone(),
        )
    })
}

fn turn_summary(turn: &TurnChangeState) -> TurnChangeSummary {
    let files = turn
        .files
        .iter()
        .filter(|(_, tracked)| {
            proven_change(
                &tracked.baseline,
                &tracked.latest,
                tracked.baseline_metadata.as_ref(),
                tracked.latest_metadata.as_ref(),
            )
        })
        .map(|(path, tracked)| {
            change_summary(
                path,
                &tracked.baseline,
                &tracked.latest,
                tracked.baseline_metadata.clone(),
                tracked.latest_metadata.clone(),
            )
        })
        .collect::<Vec<_>>();
    let stats_complete = files
        .iter()
        .all(|change| change.added_lines.is_some() && change.removed_lines.is_some());
    let added_lines = stats_complete.then(|| {
        files
            .iter()
            .filter_map(|change| change.added_lines)
            .fold(0_u64, u64::saturating_add)
    });
    let removed_lines = stats_complete.then(|| {
        files
            .iter()
            .filter_map(|change| change.removed_lines)
            .fold(0_u64, u64::saturating_add)
    });
    let file_count = u64::try_from(files.len()).unwrap_or(u64::MAX);
    TurnChangeSummary {
        files,
        added_lines,
        removed_lines,
        stats_complete,
        file_count,
        files_truncated: false,
    }
}

fn proven_change(
    before: &FileContent,
    after: &FileContent,
    before_metadata: Option<&FileStateMetadata>,
    after_metadata: Option<&FileStateMetadata>,
) -> bool {
    match (before, after) {
        (FileContent::Missing, FileContent::Missing) => false,
        (FileContent::Missing, FileContent::Text(_))
        | (FileContent::Text(_), FileContent::Missing) => true,
        (FileContent::Missing, FileContent::Unavailable) => after_metadata.is_some(),
        (FileContent::Unavailable, FileContent::Missing) => before_metadata.is_some(),
        (FileContent::Text(before), FileContent::Text(after)) => before != after,
        _ => before_metadata
            .zip(after_metadata)
            .is_some_and(|(before, after)| {
                before.size_bytes != after.size_bytes
                    || before.unix_mode != after.unix_mode
                    || before
                        .checksum
                        .as_ref()
                        .zip(after.checksum.as_ref())
                        .is_some_and(|(before, after)| before != after)
            }),
    }
}

fn change_summary(
    path: &str,
    before: &FileContent,
    after: &FileContent,
    before_metadata: Option<FileStateMetadata>,
    after_metadata: Option<FileStateMetadata>,
) -> FileChangeSummary {
    let kind = match (before_metadata.as_ref(), after_metadata.as_ref()) {
        (None, Some(_)) => FileChangeKind::Added,
        (Some(_), None) => FileChangeKind::Deleted,
        _ => FileChangeKind::Modified,
    };
    let (added_lines, removed_lines) = line_changes(before, after)
        .map_or((None, None), |(added, removed)| {
            (Some(added), Some(removed))
        });
    FileChangeSummary {
        path: path.to_owned(),
        kind,
        added_lines,
        removed_lines,
        before: before_metadata,
        after: after_metadata,
    }
}

fn line_changes(before: &FileContent, after: &FileContent) -> Option<(u64, u64)> {
    let before = text_content(before)?;
    let after = text_content(after)?;
    let mut added = 0_u64;
    let mut removed = 0_u64;
    for change in TextDiff::from_lines(before, after).iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added = added.saturating_add(1),
            ChangeTag::Delete => removed = removed.saturating_add(1),
            ChangeTag::Equal => {}
        }
    }
    Some((added, removed))
}

fn diff_preview(sample: &ChangeSample) -> Option<FileDiffPreview> {
    if !proven_change(
        &sample.before,
        &sample.after,
        sample.before_metadata.as_ref(),
        sample.after_metadata.as_ref(),
    ) {
        return None;
    }
    let before = text_content(&sample.before)?;
    let after = text_content(&sample.after)?;
    let mut lines = Vec::new();
    let mut retained_bytes = 0_usize;
    let mut truncated = false;
    for change in TextDiff::from_lines(before, after).iter_all_changes() {
        if change.tag() == ChangeTag::Equal {
            continue;
        }
        let prefix = match change.tag() {
            ChangeTag::Insert => '+',
            ChangeTag::Delete => '-',
            ChangeTag::Equal => ' ',
        };
        let value = change.value().trim_end_matches(['\r', '\n']);
        let (line, _) = redact_sensitive_text(&format!("{prefix}{value}"));
        let line_bytes = line.len().saturating_add(1);
        if lines.len() >= MAX_DIFF_PREVIEW_LINES
            || retained_bytes.saturating_add(line_bytes) > MAX_DIFF_PREVIEW_BYTES
        {
            truncated = true;
            break;
        }
        retained_bytes = retained_bytes.saturating_add(line_bytes);
        lines.push(line);
    }
    (!lines.is_empty()).then(|| FileDiffPreview {
        path: sample.path.clone(),
        lines,
        truncated,
    })
}

fn bounded_diff_previews(samples: &[ChangeSample]) -> Vec<FileDiffPreview> {
    let mut previews = Vec::new();
    let mut retained_bytes = 0_usize;
    for sample in samples {
        let Some(mut preview) = diff_preview(sample) else {
            continue;
        };
        let path_overhead = preview.path.len().saturating_add(64);
        let available = MAX_DIFF_PREVIEW_TOTAL_BYTES.saturating_sub(retained_bytes);
        if available <= path_overhead {
            break;
        }
        let line_budget = available - path_overhead;
        let mut lines = Vec::new();
        let mut line_bytes = 0_usize;
        let mut globally_truncated = false;
        for line in preview.lines {
            let cost = line.len().saturating_add(1);
            if line_bytes.saturating_add(cost) > line_budget {
                globally_truncated = true;
                break;
            }
            line_bytes = line_bytes.saturating_add(cost);
            lines.push(line);
        }
        if lines.is_empty() {
            break;
        }
        preview.lines = lines;
        preview.truncated |= globally_truncated;
        retained_bytes = retained_bytes
            .saturating_add(path_overhead)
            .saturating_add(line_bytes);
        previews.push(preview);
        if globally_truncated {
            break;
        }
    }
    previews
}

fn build_diff_artifact(samples: &[ChangeSample]) -> Option<DiffArtifact> {
    let mut content = String::new();
    let mut truncated = false;
    for sample in samples {
        if !proven_change(
            &sample.before,
            &sample.after,
            sample.before_metadata.as_ref(),
            sample.after_metadata.as_ref(),
        ) {
            continue;
        }
        let Some(before) = text_content(&sample.before) else {
            continue;
        };
        let Some(after) = text_content(&sample.after) else {
            continue;
        };
        let diff = TextDiff::from_lines(before, after);
        let old_path = format!("a/{}", sample.path);
        let new_path = format!("b/{}", sample.path);
        let patch = diff
            .unified_diff()
            .context_radius(3)
            .header(&old_path, &new_path)
            .to_string();
        let patch = redact_sensitive_text(&patch).0;
        if patch.is_empty() {
            continue;
        }
        let content_limit =
            MAX_DIFF_ARTIFACT_BYTES.saturating_sub(DIFF_ARTIFACT_TRUNCATION_MARKER.len());
        let remaining = content_limit.saturating_sub(content.len());
        if patch.len() <= remaining {
            content.push_str(&patch);
            continue;
        }
        content.push_str(utf8_prefix(&patch, remaining));
        content.push_str(DIFF_ARTIFACT_TRUNCATION_MARKER);
        truncated = true;
        break;
    }
    if content.is_empty() {
        return None;
    }
    Some(DiffArtifact { content, truncated })
}

fn state_metadata(
    bytes: &[u8],
    unix_mode: Option<u32>,
    content_available: bool,
) -> FileStateMetadata {
    FileStateMetadata {
        size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        checksum: Some(format!("sha256:{:x}", Sha256::digest(bytes))),
        unix_mode,
        content_kind: if std::str::from_utf8(bytes).is_ok() && !bytes.contains(&0) {
            FileContentKind::Text
        } else {
            FileContentKind::Binary
        },
        content_available,
    }
}

#[cfg(unix)]
fn unix_mode(metadata: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;

    Some(metadata.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn unix_mode(_metadata: &std::fs::Metadata) -> Option<u32> {
    None
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn text_content(content: &FileContent) -> Option<&str> {
    match content {
        FileContent::Missing => Some(""),
        FileContent::Text(bytes) => std::str::from_utf8(bytes).ok(),
        FileContent::Unavailable => None,
    }
}

#[cfg(test)]
mod tests {
    use golutra_core::{SessionId, ToolCallId};
    use golutra_policy::WorkspacePolicy;
    use golutra_tools::{BasicToolExecutor, ToolRequest};
    use serde_json::json;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    use super::*;

    #[test]
    fn line_changes_are_net_against_the_first_turn_baseline() {
        let task = TaskId::new();
        let turn = TurnId::new();
        let mut tracker = WorkspaceChangeTracker::default();
        let state = tracker.turns.entry((task, turn)).or_default();
        state.files.insert(
            "src/lib.rs".to_owned(),
            TrackedFile {
                baseline: FileContent::Text(b"one\ntwo\n".to_vec()),
                latest: FileContent::Text(b"one\nthree\nfour\n".to_vec()),
                baseline_metadata: None,
                latest_metadata: None,
            },
        );

        let summary = turn_summary(state);

        assert!(summary.stats_complete);
        assert_eq!(summary.added_lines, Some(2));
        assert_eq!(summary.removed_lines, Some(1));
        assert_eq!(summary.files[0].path, "src/lib.rs");
    }

    #[test]
    fn unavailable_content_never_becomes_zero_line_changes() {
        let summary = change_summary(
            "binary.dat",
            &FileContent::Unavailable,
            &FileContent::Unavailable,
            None,
            None,
        );

        assert_eq!(summary.added_lines, None);
        assert_eq!(summary.removed_lines, None);
    }

    #[test]
    fn repeated_process_snapshot_does_not_emit_duplicate_operation_changes() {
        let task = TaskId::new();
        let turn = TurnId::new();
        let mut tracker = WorkspaceChangeTracker::default();
        let sample = ChangeSample {
            path: "src/lib.rs".to_owned(),
            before: FileContent::Text(b"before\n".to_vec()),
            after: FileContent::Text(b"after\n".to_vec()),
            before_metadata: None,
            after_metadata: None,
        };

        let first = tracker.record(task, turn, vec![sample.clone()]);
        let repeated = tracker.record(task, turn, vec![sample]);

        assert_eq!(first.operation_changes.len(), 1);
        assert!(repeated.operation_changes.is_empty());
        assert!(repeated.diff_previews.is_empty());
        assert!(repeated.diff_artifact.is_none());
        assert_eq!(repeated.turn_summary.file_count, 1);
    }

    #[test]
    fn binary_metadata_changes_remain_visible_without_text_line_counts() {
        let task = TaskId::new();
        let turn = TurnId::new();
        let mut tracker = WorkspaceChangeTracker::default();
        let metadata = |checksum: &str| FileStateMetadata {
            size_bytes: 3 * 1024 * 1024,
            checksum: Some(checksum.to_owned()),
            unix_mode: Some(0o644),
            content_kind: FileContentKind::Binary,
            content_available: false,
        };

        let facts = tracker.record(
            task,
            turn,
            vec![ChangeSample {
                path: "assets/model.bin".to_owned(),
                before: FileContent::Unavailable,
                after: FileContent::Unavailable,
                before_metadata: Some(metadata("sha256:before")),
                after_metadata: Some(metadata("sha256:after")),
            }],
        );

        assert_eq!(facts.operation_changes.len(), 1);
        let change = &facts.operation_changes[0];
        assert_eq!(change.kind, FileChangeKind::Modified);
        assert_eq!(change.added_lines, None);
        assert_eq!(change.removed_lines, None);
        assert_eq!(
            change
                .before
                .as_ref()
                .and_then(|state| state.checksum.as_deref()),
            Some("sha256:before")
        );
        assert_eq!(
            change
                .after
                .as_ref()
                .and_then(|state| state.checksum.as_deref()),
            Some("sha256:after")
        );
        assert!(!facts.turn_summary.stats_complete);
        assert_eq!(facts.turn_summary.added_lines, None);
        assert_eq!(facts.turn_summary.removed_lines, None);
        assert!(facts.diff_previews.is_empty());
    }

    #[test]
    fn known_noop_writes_do_not_emit_file_changes() {
        let task = TaskId::new();
        let turn = TurnId::new();
        let mut tracker = WorkspaceChangeTracker::default();

        let facts = tracker.record(
            task,
            turn,
            vec![ChangeSample {
                path: "src/lib.rs".to_owned(),
                before: FileContent::Text(b"same\n".to_vec()),
                after: FileContent::Text(b"same\n".to_vec()),
                before_metadata: None,
                after_metadata: None,
            }],
        );

        assert!(facts.operation_changes.is_empty());
        assert!(facts.turn_summary.files.is_empty());
    }

    #[test]
    fn unavailable_content_without_comparable_metadata_is_not_claimed_as_a_change() {
        let task = TaskId::new();
        let turn = TurnId::new();
        let mut tracker = WorkspaceChangeTracker::default();

        let facts = tracker.record(
            task,
            turn,
            vec![ChangeSample {
                path: "opaque.bin".to_owned(),
                before: FileContent::Unavailable,
                after: FileContent::Unavailable,
                before_metadata: None,
                after_metadata: None,
            }],
        );

        assert!(facts.operation_changes.is_empty());
        assert!(facts.turn_summary.files.is_empty());
    }

    #[test]
    fn changed_text_emits_a_bounded_structured_diff_preview() {
        let task = TaskId::new();
        let turn = TurnId::new();
        let mut tracker = WorkspaceChangeTracker::default();

        let facts = tracker.record(
            task,
            turn,
            vec![ChangeSample {
                path: "src/lib.rs".to_owned(),
                before: FileContent::Text(b"one\ntwo\n".to_vec()),
                after: FileContent::Text(b"one\nthree\nfour\n".to_vec()),
                before_metadata: None,
                after_metadata: None,
            }],
        );

        assert_eq!(facts.diff_previews.len(), 1);
        assert_eq!(facts.diff_previews[0].path, "src/lib.rs");
        assert_eq!(
            facts.diff_previews[0].lines,
            vec!["-two", "+three", "+four"]
        );
        assert!(!facts.diff_previews[0].truncated);
    }

    #[test]
    fn diff_previews_are_redacted_before_they_enter_runtime_facts() {
        let task = TaskId::new();
        let turn = TurnId::new();
        let mut tracker = WorkspaceChangeTracker::default();

        let facts = tracker.record(
            task,
            turn,
            vec![ChangeSample {
                path: "config.txt".to_owned(),
                before: FileContent::Text(b"old\n".to_vec()),
                after: FileContent::Text(b"api_key=super-secret-value\n".to_vec()),
                before_metadata: None,
                after_metadata: None,
            }],
        );

        let rendered = facts.diff_previews[0].lines.join("\n");
        assert!(rendered.contains("<redacted-secret>"));
        assert!(!rendered.contains("super-secret-value"));
    }

    #[test]
    fn diff_previews_have_an_aggregate_budget_across_files() {
        let task = TaskId::new();
        let turn = TurnId::new();
        let mut tracker = WorkspaceChangeTracker::default();
        let samples = (0..512)
            .map(|index| ChangeSample {
                path: format!("src/file-{index}.txt"),
                before: FileContent::Text(b"old\n".to_vec()),
                after: FileContent::Text(
                    (0..80)
                        .map(|line| format!("new-{line}\n"))
                        .collect::<String>()
                        .into_bytes(),
                ),
                before_metadata: None,
                after_metadata: None,
            })
            .collect::<Vec<_>>();

        let facts = tracker.record(task, turn, samples);
        let total_bytes = facts
            .diff_previews
            .iter()
            .map(|preview| {
                preview.path.len()
                    + preview
                        .lines
                        .iter()
                        .map(|line| line.len().saturating_add(1))
                        .sum::<usize>()
                    + 64
            })
            .sum::<usize>();

        assert!(total_bytes <= MAX_DIFF_PREVIEW_TOTAL_BYTES);
        assert!(
            facts
                .diff_previews
                .last()
                .is_some_and(|preview| preview.truncated)
        );
    }

    #[test]
    fn diff_artifact_reserves_space_for_its_truncation_marker() {
        let sample = ChangeSample {
            path: "large.txt".to_owned(),
            before: FileContent::Text(Vec::new()),
            after: FileContent::Text(vec![b'x'; MAX_DIFF_ARTIFACT_BYTES + 1024]),
            before_metadata: None,
            after_metadata: None,
        };

        let artifact = build_diff_artifact(&[sample]).expect("diff artifact");

        assert!(artifact.truncated);
        assert!(artifact.content.len() <= MAX_DIFF_ARTIFACT_BYTES);
        assert!(artifact.content.ends_with(DIFF_ARTIFACT_TRUNCATION_MARKER));
    }

    #[tokio::test]
    async fn operation_after_images_prevent_later_tools_from_rewriting_the_diff() {
        let workspace = tempdir().expect("workspace");
        let path = workspace.path().join("result.txt");
        std::fs::write(&path, "before\n").expect("before");
        let executor = BasicToolExecutor::new(
            WorkspacePolicy::new(workspace.path()).expect("workspace policy"),
        );
        let request = ToolRequest {
            tool_call_id: ToolCallId::new(),
            provider_tool_call_id: None,
            session_id: SessionId::new(),
            turn_id: Some(TurnId::new()),
            tool_name: "write_file".to_owned(),
            arguments: json!({"path": "result.txt", "content": "operation\n"}),
        };
        let policy = executor.evaluate(&request).expect("policy");
        let before_images = executor
            .prepare_side_effect(&request)
            .await
            .expect("before image");
        let report = executor
            .execute_with_policy_and_before_images(
                request,
                policy,
                true,
                CancellationToken::new(),
                before_images,
            )
            .await
            .expect("write report");
        std::fs::write(&path, "later tool\n").expect("later write");

        let samples = capture_change_samples(Some(workspace.path()), &report).await;

        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].after, FileContent::Text(b"operation\n".to_vec()));
    }

    #[test]
    fn repeated_edits_remain_net_against_the_first_turn_baseline() {
        let task = TaskId::new();
        let turn = TurnId::new();
        let mut tracker = WorkspaceChangeTracker::default();

        tracker.record(
            task,
            turn,
            vec![ChangeSample {
                path: "src/lib.rs".to_owned(),
                before: FileContent::Text(b"one\ntwo\n".to_vec()),
                after: FileContent::Text(b"one\nthree\n".to_vec()),
                before_metadata: None,
                after_metadata: None,
            }],
        );
        let facts = tracker.record(
            task,
            turn,
            vec![ChangeSample {
                path: "src/lib.rs".to_owned(),
                before: FileContent::Text(b"one\nthree\n".to_vec()),
                after: FileContent::Text(b"one\nthree\nfour\n".to_vec()),
                before_metadata: None,
                after_metadata: None,
            }],
        );

        assert_eq!(facts.operation_changes[0].added_lines, Some(1));
        assert_eq!(facts.operation_changes[0].removed_lines, Some(0));
        assert_eq!(facts.turn_summary.added_lines, Some(2));
        assert_eq!(facts.turn_summary.removed_lines, Some(1));
    }

    #[test]
    fn reverting_a_file_removes_it_from_the_turn_net_change() {
        let task = TaskId::new();
        let turn = TurnId::new();
        let mut tracker = WorkspaceChangeTracker::default();

        tracker.record(
            task,
            turn,
            vec![ChangeSample {
                path: "src/lib.rs".to_owned(),
                before: FileContent::Text(b"one\ntwo\n".to_vec()),
                after: FileContent::Text(b"one\nthree\n".to_vec()),
                before_metadata: None,
                after_metadata: None,
            }],
        );
        let facts = tracker.record(
            task,
            turn,
            vec![ChangeSample {
                path: "src/lib.rs".to_owned(),
                before: FileContent::Text(b"one\nthree\n".to_vec()),
                after: FileContent::Text(b"one\ntwo\n".to_vec()),
                before_metadata: None,
                after_metadata: None,
            }],
        );

        assert_eq!(facts.operation_changes.len(), 1);
        assert!(facts.turn_summary.files.is_empty());
        assert_eq!(facts.turn_summary.added_lines, Some(0));
        assert_eq!(facts.turn_summary.removed_lines, Some(0));
        assert!(facts.turn_summary.stats_complete);
    }

    #[test]
    fn retained_turn_content_is_bounded_without_reporting_fake_stats() {
        let mut retained_bytes = 0;
        let baseline =
            retain_bounded_content(FileContent::Text(b"1234".to_vec()), &mut retained_bytes, 6);
        let latest =
            retain_bounded_content(FileContent::Text(b"5678".to_vec()), &mut retained_bytes, 6);

        assert!(matches!(baseline, FileContent::Text(_)));
        assert_eq!(latest, FileContent::Unavailable);
        assert_eq!(retained_bytes, 4);
        assert_eq!(line_changes(&baseline, &latest), None);
    }
}
