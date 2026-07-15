use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// 主聊天输入框的编辑状态。
///
/// 文本仍以 UTF-8 字节索引保存，但所有编辑边界都按 grapheme cluster 计算。
/// 这样组合字符、emoji 和中文输入法提交的字符串不会被拆成无效或半个字符。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ComposerInput {
    text: String,
    cursor: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposerViewport {
    pub(crate) lines: Vec<String>,
    pub(crate) cursor: (u16, u16),
}

#[derive(Debug)]
struct VisualLayout {
    lines: Vec<Range<usize>>,
    cursor_line: usize,
    cursor_col: usize,
}

impl ComposerInput {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub(crate) fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
    }

    pub(crate) fn trimmed(&self) -> String {
        self.text.trim().to_owned()
    }

    pub(crate) fn insert_char(&mut self, character: char) {
        self.text.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    pub(crate) fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    pub(crate) fn delete_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = previous_grapheme_boundary(&self.text, self.cursor);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub(crate) fn delete_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let end = next_grapheme_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..end, "");
    }

    pub(crate) fn move_left(&mut self) {
        self.cursor = previous_grapheme_boundary(&self.text, self.cursor);
    }

    pub(crate) fn move_right(&mut self) {
        self.cursor = next_grapheme_boundary(&self.text, self.cursor);
    }

    pub(crate) fn move_to_start(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn move_to_end(&mut self) {
        self.cursor = self.text.len();
    }

    /// 根据终端可用列数计算可见行和真实光标位置。
    ///
    /// 输入框最多显示 `max_rows` 行；内容超过高度时，视口跟随光标，保证输入法候选
    /// 窗口始终锚定在当前插入点附近，而不是停留在已经滚出屏幕的旧位置。
    pub(crate) fn viewport(&self, width: u16, max_rows: u16) -> ComposerViewport {
        let width = usize::from(width.max(1));
        let max_rows = usize::from(max_rows.max(1));
        let layout = self.visual_layout(width);
        let cursor_line = layout.cursor_line.min(layout.lines.len().saturating_sub(1));
        let start = cursor_line.saturating_sub(max_rows.saturating_sub(1));
        let end = (start + max_rows).min(layout.lines.len());
        let lines = layout.lines[start..end]
            .iter()
            .map(|range| self.text[range.clone()].to_owned())
            .collect();
        let cursor = (
            layout.cursor_col.min(u16::MAX as usize) as u16,
            cursor_line.saturating_sub(start).min(u16::MAX as usize) as u16,
        );
        ComposerViewport { lines, cursor }
    }

    fn visual_layout(&self, width: usize) -> VisualLayout {
        let mut lines = Vec::new();
        let mut line_start = 0;
        let mut line_width = 0;
        let mut cursor_line = 0;
        let mut cursor_col = 0;
        let mut cursor_recorded = self.cursor == 0;

        for (start, grapheme) in self.text.grapheme_indices(true) {
            if grapheme == "\n" {
                if !cursor_recorded && self.cursor == start {
                    cursor_line = lines.len();
                    cursor_col = line_width;
                    cursor_recorded = true;
                }
                lines.push(line_start..start);
                line_start = start + grapheme.len();
                line_width = 0;
                if self.cursor == line_start {
                    cursor_line = lines.len();
                    cursor_col = 0;
                    cursor_recorded = true;
                }
                continue;
            }

            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if line_width > 0 && line_width + grapheme_width > width {
                lines.push(line_start..start);
                line_start = start;
                line_width = 0;
                if self.cursor == start {
                    cursor_line = lines.len();
                    cursor_col = 0;
                    cursor_recorded = true;
                }
            }

            if !cursor_recorded && self.cursor == start {
                cursor_line = lines.len();
                cursor_col = line_width;
                cursor_recorded = true;
            }

            line_width += grapheme_width;
            let end = start + grapheme.len();
            if line_width >= width {
                lines.push(line_start..end);
                line_start = end;
                line_width = 0;
                if self.cursor == end {
                    cursor_line = lines.len();
                    cursor_col = 0;
                    cursor_recorded = true;
                }
            } else if self.cursor == end {
                cursor_line = lines.len();
                cursor_col = line_width;
                cursor_recorded = true;
            }
        }

        if line_start <= self.text.len() {
            lines.push(line_start..self.text.len());
        }
        if lines.is_empty() {
            lines.push(0..0);
        }
        if !cursor_recorded {
            cursor_line = lines.len().saturating_sub(1);
            cursor_col = line_width;
        }

        VisualLayout {
            lines,
            cursor_line,
            cursor_col,
        }
    }
}

fn previous_grapheme_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(start, _)| start)
}

pub(crate) fn delete_last_grapheme(text: &mut String) {
    let end = text.len();
    if end == 0 {
        return;
    }
    let start = previous_grapheme_boundary(text, end);
    text.replace_range(start..end, "");
}

fn next_grapheme_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .graphemes(true)
        .next()
        .map_or(text.len(), |grapheme| cursor + grapheme.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_graphemes_without_splitting_emoji_or_combining_marks() {
        let mut input = ComposerInput::default();
        input.insert_str("a👍e\u{301}中");
        input.move_left();
        input.delete_backward();
        assert_eq!(input.text(), "a👍中");
        input.move_to_start();
        input.move_right();
        input.delete_forward();
        assert_eq!(input.text(), "a中");
    }

    #[test]
    fn chinese_display_width_drives_cursor_column() {
        let mut input = ComposerInput::default();
        input.insert_str("你好a");
        let viewport = input.viewport(20, 3);
        assert_eq!(viewport.lines, vec!["你好a"]);
        assert_eq!(viewport.cursor, (5, 0));
    }

    #[test]
    fn long_input_keeps_cursor_visible_in_a_small_viewport() {
        let mut input = ComposerInput::default();
        input.insert_str("一二三四五六七八");
        let viewport = input.viewport(4, 2);
        assert_eq!(viewport.lines, vec!["七八", ""]);
        assert_eq!(viewport.cursor, (0, 1));
    }

    #[test]
    fn cursor_before_wrapped_grapheme_stays_on_the_new_visual_line() {
        let mut input = ComposerInput::default();
        input.insert_str("abc中");
        input.move_left();

        assert_eq!(input.viewport(4, 3).cursor, (0, 1));
    }

    #[test]
    fn pasted_newlines_create_editable_visual_lines() {
        let mut input = ComposerInput::default();
        input.insert_str("第一行\n第二行");
        input.move_left();
        input.move_left();
        assert_eq!(input.viewport(20, 3).cursor, (2, 1));
        input.delete_backward();
        assert_eq!(input.text(), "第一行\n二行");
    }
}
