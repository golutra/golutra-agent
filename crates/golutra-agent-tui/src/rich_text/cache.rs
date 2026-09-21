//! 复用已完成块的解析和排版；可变尾部增量解析，跨块引用等情况回退完整文档。

use std::{collections::VecDeque, sync::Arc};

use ratatui::text::Line;

use super::{layout, markdown, model::MarkdownBlock, theme};

const MAX_DOCUMENTS: usize = 32;
const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub(crate) struct MarkdownCache {
    documents: VecDeque<Document>,
    cwd: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone)]
struct Document {
    source: String,
    width: u16,
    blocks: Vec<Block>,
    lines: Arc<Vec<Line<'static>>>,
    stable_source_len: usize,
    stable_blocks: usize,
    source_wide: bool,
    archive_source_end: usize,
}

#[derive(Debug, Clone)]
struct Block {
    model: MarkdownBlock,
    lines: Arc<Vec<Line<'static>>>,
}

impl MarkdownCache {
    pub(crate) fn set_cwd(&mut self, cwd: &std::path::Path) {
        if self.cwd.as_deref() != Some(cwd) {
            self.cwd = Some(cwd.to_owned());
            self.documents.clear();
        }
    }

    pub(crate) fn render(&mut self, source: &str, width: u16) -> Vec<Line<'static>> {
        let width = width.max(1);
        if source.len() > MAX_SOURCE_BYTES {
            return layout::render_markdown_document(
                &markdown::parse_markdown_in(source, self.cwd.as_deref()),
                usize::from(width),
            );
        }
        if let Some(index) = self
            .documents
            .iter()
            .position(|entry| entry.width == width && entry.source == source)
        {
            let entry = self.documents.remove(index).expect("cached document");
            let lines = entry.lines.as_ref().clone();
            self.documents.push_back(entry);
            return lines;
        }
        // Only reuse an append ancestor. A corrected final response or resize
        // naturally takes the canonical path; no stale styles or text survive.
        let previous_index = self
            .documents
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.width == width
                    && !entry.source.is_empty()
                    && source.starts_with(&entry.source)
            })
            .max_by_key(|(_, entry)| entry.source.len())
            .map(|(index, _)| index);
        let previous = previous_index.and_then(|index| self.documents.get(index));
        let retained = previous.filter(|entry| !entry.source_wide);
        let offset = retained.map_or(0, |entry| entry.stable_source_len);
        let mut parsed = markdown::parse_stream(&source[offset..], self.cwd.as_deref());
        // 尾部新增引用定义可能改变任何旧块，不能只修订最后一个块。
        let retained = if parsed.source_wide && offset > 0 {
            parsed = markdown::parse_stream(source, self.cwd.as_deref());
            None
        } else {
            retained
        };
        let retained_blocks = retained.map_or(0, |entry| entry.stable_blocks);
        let stable_source_len =
            retained.map_or(0, |entry| entry.stable_source_len) + parsed.stable_source_len;
        let stable_blocks = retained_blocks + parsed.stable_blocks;
        // 归档沿用原来的最后顶层块边界，并比较渲染前缀；解析复用必须更保守，
        // 不能把两者合并成同一个游标，否则未换行正文会推迟已有历史的提交。
        let mut archive_source_end = parsed.last_block_start.map_or_else(
            || crate::stream_commit::stable_source_end(source),
            |start| retained.map_or(0, |entry| entry.stable_source_len) + start,
        );
        if matches!(
            parsed.document.blocks.last(),
            Some(MarkdownBlock::Code { .. })
        ) {
            archive_source_end =
                crate::stream_commit::complete_code_source_end(source, archive_source_end);
        }
        let models = retained
            .into_iter()
            .flat_map(|entry| {
                entry.blocks[..retained_blocks]
                    .iter()
                    .map(|b| b.model.clone())
            })
            .chain(parsed.document.blocks);
        let mut blocks = Vec::new();
        let mut lines = Vec::new();
        for (index, model) in models.enumerate() {
            let old = previous.and_then(|entry| entry.blocks.get(index));
            let rendered = if let Some(old) = old.filter(|old| old.model == model) {
                old.lines.clone()
            } else {
                Arc::new(append_code(old, &model, width).unwrap_or_else(|| {
                    layout::render_block(&model, usize::from(width), theme::body())
                }))
            };
            if !blocks.is_empty()
                && lines
                    .last()
                    .is_some_and(|line: &Line<'_>| !line.spans.is_empty())
            {
                lines.push(Line::default());
            }
            lines.extend(rendered.iter().cloned());
            blocks.push(Block {
                model,
                lines: rendered,
            });
        }
        if lines.is_empty() {
            lines.push(Line::default());
        }
        // Keep recent exact snapshots for the history-prefix and live-body consumers.
        // Bound retained source as well as entry count for arbitrarily long sessions.
        if source.len() <= MAX_SOURCE_BYTES {
            if let Some(index) = previous_index {
                self.documents.remove(index);
            }
            self.documents.push_back(Document {
                source: source.to_owned(),
                width,
                blocks,
                lines: Arc::new(lines.clone()),
                stable_source_len,
                stable_blocks,
                source_wide: parsed.source_wide,
                archive_source_end,
            });
            let mut bytes = self
                .documents
                .iter()
                .map(|entry| entry.source.len())
                .sum::<usize>();
            while self.documents.len() > MAX_DOCUMENTS || bytes > MAX_SOURCE_BYTES {
                bytes -= self
                    .documents
                    .pop_front()
                    .expect("nonempty cache")
                    .source
                    .len();
            }
        }
        lines
    }

    pub(crate) fn stable_source_end(&self, source: &str) -> usize {
        self.documents
            .iter()
            .rev()
            .find(|entry| entry.source == source)
            .map_or_else(
                || crate::stream_commit::stable_source_end(source),
                |entry| entry.archive_source_end,
            )
    }
}

fn append_code(
    previous: Option<&Block>,
    next: &MarkdownBlock,
    width: u16,
) -> Option<Vec<Line<'static>>> {
    let previous = previous?;
    let MarkdownBlock::Code { language, source } = next else {
        return None;
    };
    let MarkdownBlock::Code {
        language: old_language,
        source: old_source,
    } = &previous.model
    else {
        return None;
    };
    if language != old_language || old_source.is_empty() {
        return None;
    }
    let body = old_source.strip_suffix('\n').unwrap_or(old_source);
    let stable_end = body.rfind('\n').map_or(0, |index| index + 1);
    if !source.starts_with(&old_source[..stable_end]) {
        return None;
    }
    let old_tail_rows = layout::render_code_body_from(
        language.as_deref(),
        old_source,
        stable_end,
        usize::from(width),
    )
    .len();
    let mut lines = previous.lines.as_ref().clone();
    lines.truncate(lines.len().checked_sub(old_tail_rows)?);
    if stable_end == 0 || stable_end < source.len() {
        lines.extend(layout::render_code_body_from(
            language.as_deref(),
            source,
            stable_end,
            usize::from(width),
        ));
    }
    Some(lines)
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;
