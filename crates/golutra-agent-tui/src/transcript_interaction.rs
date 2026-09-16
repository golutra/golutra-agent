//! Fullscreen pointer interaction uses the displayed cells, never raw ANSI or provider text.

use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
use unicode_width::UnicodeWidthStr;

use super::{OperationId, TuiApp};

#[derive(Debug, Clone)]
pub(crate) struct TranscriptPointer {
    pub(crate) start: (u16, u16),
    pub(crate) end: (u16, u16),
    pub(crate) operation: Option<OperationId>,
    pub(crate) snapshot: Buffer,
    pub(crate) area: Rect,
    pub(crate) released: bool,
}

impl TranscriptPointer {
    pub(crate) fn is_selection(&self) -> bool {
        self.start != self.end
    }

    fn bounds(&self) -> ((u16, u16), (u16, u16)) {
        let start = (self.start.1, self.start.0);
        let end = (self.end.1, self.end.0);
        (start.min(end), start.max(end))
    }

    pub(crate) fn text(&self) -> String {
        let (start, end) = self.bounds();
        let mut rows = Vec::new();
        for y in start.0..=end.0 {
            let mut row = String::new();
            let mut x = self.area.x;
            while x < self.area.right() {
                let Some(cell) = self.snapshot.cell((x, y)) else {
                    break;
                };
                let width = u16::try_from(UnicodeWidthStr::width(cell.symbol()))
                    .unwrap_or(1)
                    .max(1);
                // 选区从中文或 emoji 的续格开始时仍复制完整字形，不产生占位空格。
                if (y, x.saturating_add(width - 1)) >= start && (y, x) <= end {
                    row.push_str(cell.symbol());
                }
                x = x.saturating_add(width);
            }
            rows.push(row.trim_end().to_owned());
        }
        rows.join("\n")
    }

    fn highlight(&self, buffer: &mut Buffer) {
        let (start, end) = self.bounds();
        for y in self.area.y..self.area.bottom() {
            for x in self.area.x..self.area.right() {
                if (y, x) >= start
                    && (y, x) <= end
                    && let Some(cell) = buffer.cell_mut((x, y))
                {
                    cell.modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }
}

pub(crate) fn update_transcript_screen(app: &mut TuiApp, buffer: &mut Buffer) {
    let area = app.layout.transcript;
    if app.overlay_surface().is_some() {
        app.transcript_pointer = None;
        app.transcript_screen = None;
        return;
    }
    if let Some(pointer) = &app.transcript_pointer {
        // 内容或尺寸发生变化后取消选区，防止高亮与复制的事实错位。
        let unchanged = pointer.area == area
            && (area.y..area.bottom()).all(|y| {
                (area.x..area.right()).all(|x| pointer.snapshot.cell((x, y)) == buffer.cell((x, y)))
            });
        if !unchanged {
            app.transcript_pointer = None;
        }
    }
    app.transcript_screen = Some(buffer.clone());
    if let Some(pointer) = &app.transcript_pointer
        && pointer.is_selection()
    {
        pointer.highlight(buffer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_copies_wide_graphemes_in_both_directions() {
        let area = Rect::new(0, 0, 12, 2);
        let mut snapshot = Buffer::empty(area);
        snapshot.set_string(0, 0, "中🙂文 test", ratatui::style::Style::default());
        snapshot.set_string(0, 1, "second", ratatui::style::Style::default());
        let mut pointer = TranscriptPointer {
            start: (1, 0),
            end: (5, 1),
            operation: None,
            snapshot,
            area,
            released: true,
        };
        assert_eq!(pointer.text(), "中🙂文 test\nsecond");
        std::mem::swap(&mut pointer.start, &mut pointer.end);
        assert_eq!(pointer.text(), "中🙂文 test\nsecond");
    }
}
