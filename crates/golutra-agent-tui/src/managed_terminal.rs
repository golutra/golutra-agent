//! 统一管理主屏活动区和临时全屏的几何、缓冲区与历史插入。
//! Ratatui 0.28 的 Inline 矩形不能直接更新；这里保留其控件与 Backend，避免重建终端时查询光标和隐式补行。

use std::io;

use ratatui::{
    TerminalOptions, Viewport,
    backend::{Backend, ClearType},
    buffer::{Buffer, Cell},
    layout::{Position, Rect, Size},
    widgets::Widget,
};

pub(crate) struct Frame<'a> {
    buffer: &'a mut Buffer,
    cursor: Option<Position>,
}

impl Frame<'_> {
    pub(crate) fn area(&self) -> Rect {
        self.buffer.area
    }

    pub(crate) fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffer
    }

    pub(crate) fn render_widget(&mut self, widget: impl Widget, area: Rect) {
        widget.render(area, self.buffer);
    }

    pub(crate) fn set_cursor_position(&mut self, position: impl Into<Position>) {
        self.cursor = Some(position.into());
    }
}

/// 主屏始终以原顶部为锚点；只有溢出屏幕才上推历史，收缩不会反向滚动历史。
#[derive(Debug, PartialEq, Eq)]
struct ViewportChange {
    area: Rect,
    scroll: u16,
}

impl ViewportChange {
    fn inline(previous: Rect, height: u16, size: Size) -> Self {
        let height = height.max(1).min(size.height.max(1));
        let top = previous.y.min(size.height.saturating_sub(1));
        let scroll = top.saturating_add(height).saturating_sub(size.height);
        Self {
            area: Rect::new(0, top.saturating_sub(scroll), size.width.max(1), height),
            scroll,
        }
    }
}

pub(crate) struct Terminal<B: Backend> {
    backend: B,
    current: Buffer,
    previous: Buffer,
    viewport: Viewport,
    screen_size: Size,
}

impl<B: Backend> Terminal<B> {
    pub(crate) fn new(backend: B) -> io::Result<Self> {
        Self::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )
    }

    pub(crate) fn with_options(mut backend: B, options: TerminalOptions) -> io::Result<Self> {
        let size = backend.size()?;
        let (area, scroll) = match options.viewport {
            Viewport::Inline(height) => {
                let cursor = backend.get_cursor_position()?;
                let change =
                    ViewportChange::inline(Rect::new(0, cursor.y, size.width, 1), height, size);
                (change.area, change.scroll)
            }
            Viewport::Fullscreen => (Rect::new(0, 0, size.width, size.height), 0),
            Viewport::Fixed(area) => (area, 0),
        };
        let mut terminal = Self {
            backend,
            current: Buffer::empty(area),
            previous: Buffer::empty(area),
            viewport: options.viewport,
            screen_size: size,
        };
        terminal.scroll_up(scroll)?;
        Ok(terminal)
    }

    pub(crate) fn backend(&self) -> &B {
        &self.backend
    }

    pub(crate) fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    pub(crate) fn current_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.current
    }

    pub(crate) fn size(&self) -> io::Result<Size> {
        self.backend.size()
    }

    #[cfg(test)]
    pub(crate) fn set_cursor_position(&mut self, position: impl Into<Position>) -> io::Result<()> {
        self.backend.set_cursor_position(position)
    }

    fn bind_area(&mut self, area: Rect, size: Size) {
        self.current.resize(area);
        self.current.reset();
        self.previous.resize(area);
        self.previous.reset();
        self.screen_size = size;
    }

    pub(crate) fn restore_inline(&mut self, area: Rect) -> io::Result<()> {
        let size = self.size()?;
        self.viewport = Viewport::Inline(area.height);
        self.bind_area(area, size);
        // 备用屏绘制已替换缓冲区；主屏恢复时仅清理活动区，历史由终端保留。
        self.clear()
    }

    pub(crate) fn set_viewport(&mut self, viewport: Viewport) -> io::Result<()> {
        let size = self.size()?;
        self.viewport = viewport.clone();
        match viewport {
            Viewport::Inline(height) => self.resize_inline(height, size),
            Viewport::Fullscreen => {
                self.bind_area(Rect::new(0, 0, size.width, size.height), size);
                self.clear()
            }
            Viewport::Fixed(area) => {
                self.bind_area(area, size);
                self.clear()
            }
        }
    }

    pub(crate) fn resize_inline(&mut self, height: u16, size: Size) -> io::Result<()> {
        let change = ViewportChange::inline(self.current.area, height, size);
        if self.current.area == change.area && self.screen_size == size {
            return Ok(());
        }
        // 先擦旧活动区再滚动，旧输入框和候选不能被写进 scrollback。
        let clear_y = self.current.area.y.min(size.height.saturating_sub(1));
        self.clear_after(clear_y)?;
        self.screen_size = size;
        self.scroll_up(change.scroll)?;
        self.viewport = Viewport::Inline(change.area.height);
        self.bind_area(change.area, size);
        Ok(())
    }

    pub(crate) fn resize(&mut self, area: Rect) -> io::Result<()> {
        let size = area.as_size();
        match self.viewport {
            Viewport::Inline(height) => self.resize_inline(height, size),
            Viewport::Fullscreen | Viewport::Fixed(_) => {
                self.bind_area(area, size);
                self.clear()
            }
        }
    }

    pub(crate) fn autoresize(&mut self) -> io::Result<()> {
        let size = self.size()?;
        if size != self.screen_size && !matches!(self.viewport, Viewport::Fixed(_)) {
            self.resize(Rect::new(0, 0, size.width, size.height))?;
        }
        Ok(())
    }

    fn clear_after(&mut self, y: u16) -> io::Result<()> {
        self.backend.set_cursor_position(Position::new(0, y))?;
        self.backend.clear_region(ClearType::AfterCursor)?;
        self.previous.reset();
        Ok(())
    }

    pub(crate) fn clear(&mut self) -> io::Result<()> {
        match self.viewport {
            Viewport::Inline(_) => self.clear_after(self.current.area.y)?,
            Viewport::Fullscreen => self.backend.clear()?,
            Viewport::Fixed(area) => {
                let blank = Buffer::empty(area);
                self.backend
                    .draw(blank.content.iter().enumerate().map(|(index, cell)| {
                        (
                            area.x + (index % usize::from(area.width)) as u16,
                            area.y + (index / usize::from(area.width)) as u16,
                            cell,
                        )
                    }))?;
            }
        }
        self.previous.reset();
        Ok(())
    }

    pub(crate) fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> io::Result<()> {
        self.autoresize()?;
        self.current.reset();
        let mut frame = Frame {
            buffer: &mut self.current,
            cursor: None,
        };
        render(&mut frame);
        let cursor = frame.cursor;
        self.backend
            .draw(self.previous.diff(&self.current).into_iter())?;
        if let Some(cursor) = cursor.filter(|pos| self.current.area.contains(*pos)) {
            self.backend.set_cursor_position(cursor)?;
            self.backend.show_cursor()?;
        } else {
            self.backend.hide_cursor()?;
        }
        self.backend.flush()?;
        std::mem::swap(&mut self.current, &mut self.previous);
        Ok(())
    }

    fn scroll_up(&mut self, rows: u16) -> io::Result<()> {
        if rows > 0 {
            self.backend
                .set_cursor_position(Position::new(0, self.screen_size.height.saturating_sub(1)))?;
            self.backend.append_lines(rows)?;
        }
        Ok(())
    }

    /// 归档与视口使用同一锚点。逐屏写入可处理超过屏高的批次，且不把活动区残影滚入历史。
    pub(crate) fn insert_before(
        &mut self,
        height: u16,
        render: impl FnOnce(&mut Buffer),
    ) -> io::Result<()> {
        if height == 0 || !matches!(self.viewport, Viewport::Inline(_)) {
            return Ok(());
        }
        let area = self.current.area;
        let mut buffer = Buffer::empty(Rect::new(0, 0, area.width, height));
        render(&mut buffer);
        self.clear()?;
        let screen_height = i32::from(self.screen_size.height.max(1));
        let mut next_y = i32::from(area.y);
        let mut remaining = i32::from(height);
        let mut cells = buffer.content.as_slice();
        while remaining + i32::from(area.height) > screen_height {
            let count = remaining.min(screen_height);
            let scroll = (next_y + count - screen_height).max(0);
            self.scroll_up(scroll as u16)?;
            cells = self.draw_rows((next_y - scroll) as u16, count as u16, area.width, cells)?;
            next_y += count - scroll;
            remaining -= count;
        }
        let scroll = (next_y + remaining + i32::from(area.height) - screen_height).max(0);
        self.scroll_up(scroll as u16)?;
        self.draw_rows(
            (next_y - scroll) as u16,
            remaining as u16,
            area.width,
            cells,
        )?;
        let next_area = Rect {
            y: (next_y + remaining - scroll) as u16,
            ..area
        };
        self.bind_area(next_area, self.screen_size);
        self.clear()
    }

    fn draw_rows<'a>(
        &mut self,
        y: u16,
        rows: u16,
        width: u16,
        cells: &'a [Cell],
    ) -> io::Result<&'a [Cell]> {
        let (drawn, rest) = cells.split_at(usize::from(rows) * usize::from(width));
        if !drawn.is_empty() {
            self.backend
                .draw(drawn.iter().enumerate().map(|(index, cell)| {
                    (
                        (index % usize::from(width)) as u16,
                        y + (index / usize::from(width)) as u16,
                        cell,
                    )
                }))?;
            self.backend.flush()?;
        }
        Ok(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, widgets::Paragraph};

    #[test]
    fn resizing_and_overlay_return_keep_the_same_backend_and_history_cells() {
        let mut backend = TestBackend::new(80, 24);
        let shell = Cell::new("S");
        backend.draw([(0, 0, &shell)].into_iter()).unwrap();
        backend.set_cursor_position(Position::new(0, 2)).unwrap();
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(3),
            },
        )
        .unwrap();
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("draft"), frame.area()))
            .unwrap();
        terminal.resize_inline(8, Size::new(80, 24)).unwrap();
        assert_eq!(terminal.current.area.y, 2);
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("expanded"), frame.area()))
            .unwrap();
        terminal.resize_inline(3, Size::new(80, 24)).unwrap();
        assert_eq!(terminal.current.area.y, 2);
        assert_eq!(terminal.backend.buffer()[(0, 0)].symbol(), "S");
        // 返回主屏时无需获取新光标；后端光标即使被外部操作移动也不能改变已保存的锚点。
        terminal
            .backend
            .set_cursor_position(Position::new(0, 23))
            .unwrap();
        terminal.restore_inline(Rect::new(0, 2, 80, 3)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("draft"), frame.area()))
            .unwrap();
        assert_eq!(terminal.backend.buffer()[(0, 0)].symbol(), "S");
        assert_eq!(terminal.backend.buffer()[(0, 2)].symbol(), "d");
        for row in 5..24 {
            assert_eq!(terminal.backend.buffer()[(0, row)].symbol(), " ");
        }
    }

    #[test]
    fn popup_grows_into_free_space_then_scrolls_only_overflow() {
        let size = Size::new(80, 24);
        assert_eq!(
            ViewportChange::inline(Rect::new(0, 5, 80, 3), 8, size),
            ViewportChange {
                area: Rect::new(0, 5, 80, 8),
                scroll: 0,
            }
        );
        assert_eq!(
            ViewportChange::inline(Rect::new(0, 21, 80, 3), 8, size),
            ViewportChange {
                area: Rect::new(0, 16, 80, 8),
                scroll: 5,
            }
        );
        assert_eq!(
            ViewportChange::inline(Rect::new(0, 16, 80, 8), 3, size),
            ViewportChange {
                area: Rect::new(0, 16, 80, 3),
                scroll: 0,
            }
        );
    }
}
