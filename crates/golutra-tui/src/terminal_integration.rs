//! Terminal capabilities that are deliberately kept outside the render loop.

use std::{
    fs,
    io::{self, Write},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use crossterm::{
    cursor::MoveTo,
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute, queue,
    style::{
        Attribute as CrosstermAttribute, Color as CrosstermColor, Colors, Print, SetAttribute,
        SetBackgroundColor, SetColors, SetForegroundColor, SetUnderlineColor,
    },
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    backend::{Backend, ClearType, CrosstermBackend, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
    style::{Color, Modifier},
};
use unicode_width::UnicodeWidthStr;

use crate::InteractiveTerminal;

const MAX_OSC52_BYTES: usize = 100 * 1024;
static ALTERNATE_SCREEN_ACTIVE: AtomicBool = AtomicBool::new(true);

/// Crossterm backend that keeps adjacent wide glyphs contiguous in the output stream.
///
/// Ratatui 0.28 compares the next cell with `previous_x + 1`, even when the previous glyph
/// occupies two columns. The resulting absolute cursor move is visually correct, but terminal
/// scrollback and copied text can materialize the covered column as a space between every CJK
/// glyph. Tracking the displayed end column preserves both the screen and its textual history.
pub(crate) struct ContiguousCrosstermBackend<W: Write> {
    writer: W,
}

impl<W: Write> ContiguousCrosstermBackend<W> {
    pub(crate) const fn new(writer: W) -> Self {
        Self { writer }
    }

    #[cfg(test)]
    fn into_inner(self) -> W {
        self.writer
    }
}

impl<W: Write> Write for ContiguousCrosstermBackend<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.writer.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl<W: Write> Backend for ContiguousCrosstermBackend<W> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut foreground = Color::Reset;
        let mut background = Color::Reset;
        let mut underline = Color::Reset;
        let mut modifier = Modifier::empty();
        let mut previous_end = None;

        for (x, y, cell) in content {
            // `insert_before` draws the complete temporary buffer, including the reset cell
            // covered by a preceding wide grapheme. That cell is not a terminal character and
            // writing it would erase the grapheme's second column in scrollback.
            if previous_end.is_some_and(|(end, row)| row == y && x < end) {
                continue;
            }
            if !matches!(previous_end, Some((end, row)) if x == end && y == row) {
                queue!(self.writer, MoveTo(x, y))?;
            }
            let width = u16::try_from(UnicodeWidthStr::width(cell.symbol()))
                .unwrap_or(u16::MAX)
                .max(1);
            previous_end = Some((x.saturating_add(width), y));

            if cell.modifier != modifier {
                ModifierDiff {
                    from: modifier,
                    to: cell.modifier,
                }
                .queue(&mut self.writer)?;
                modifier = cell.modifier;
            }
            if cell.fg != foreground || cell.bg != background {
                queue!(
                    self.writer,
                    SetColors(Colors::new(cell.fg.into(), cell.bg.into()))
                )?;
                foreground = cell.fg;
                background = cell.bg;
            }
            if cell.underline_color != underline {
                queue!(
                    self.writer,
                    SetUnderlineColor(CrosstermColor::from(cell.underline_color))
                )?;
                underline = cell.underline_color;
            }
            queue!(self.writer, Print(cell.symbol()))?;
        }

        queue!(
            self.writer,
            SetForegroundColor(CrosstermColor::Reset),
            SetBackgroundColor(CrosstermColor::Reset),
            SetUnderlineColor(CrosstermColor::Reset),
            SetAttribute(CrosstermAttribute::Reset),
        )
    }

    fn append_lines(&mut self, count: u16) -> io::Result<()> {
        CrosstermBackend::new(&mut self.writer).append_lines(count)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        CrosstermBackend::new(&mut self.writer).hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        CrosstermBackend::new(&mut self.writer).show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        CrosstermBackend::new(&mut self.writer).get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        CrosstermBackend::new(&mut self.writer).set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        CrosstermBackend::new(&mut self.writer).clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        CrosstermBackend::new(&mut self.writer).clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        let (width, height) = crossterm::terminal::size()?;
        Ok(Size { width, height })
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        let window = crossterm::terminal::window_size()?;
        Ok(WindowSize {
            columns_rows: Size {
                width: window.columns,
                height: window.rows,
            },
            pixels: Size {
                width: window.width,
                height: window.height,
            },
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

struct ModifierDiff {
    from: Modifier,
    to: Modifier,
}

impl ModifierDiff {
    fn queue(self, writer: &mut impl Write) -> io::Result<()> {
        let removed = self.from - self.to;
        if removed.contains(Modifier::REVERSED) {
            queue!(writer, SetAttribute(CrosstermAttribute::NoReverse))?;
        }
        if removed.contains(Modifier::BOLD) {
            queue!(writer, SetAttribute(CrosstermAttribute::NormalIntensity))?;
            if self.to.contains(Modifier::DIM) {
                queue!(writer, SetAttribute(CrosstermAttribute::Dim))?;
            }
        }
        if removed.contains(Modifier::ITALIC) {
            queue!(writer, SetAttribute(CrosstermAttribute::NoItalic))?;
        }
        if removed.contains(Modifier::UNDERLINED) {
            queue!(writer, SetAttribute(CrosstermAttribute::NoUnderline))?;
        }
        if removed.contains(Modifier::DIM) {
            queue!(writer, SetAttribute(CrosstermAttribute::NormalIntensity))?;
        }
        if removed.contains(Modifier::CROSSED_OUT) {
            queue!(writer, SetAttribute(CrosstermAttribute::NotCrossedOut))?;
        }
        if removed.intersects(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK) {
            queue!(writer, SetAttribute(CrosstermAttribute::NoBlink))?;
        }

        let added = self.to - self.from;
        for (modifier, attribute) in [
            (Modifier::REVERSED, CrosstermAttribute::Reverse),
            (Modifier::BOLD, CrosstermAttribute::Bold),
            (Modifier::ITALIC, CrosstermAttribute::Italic),
            (Modifier::UNDERLINED, CrosstermAttribute::Underlined),
            (Modifier::DIM, CrosstermAttribute::Dim),
            (Modifier::CROSSED_OUT, CrosstermAttribute::CrossedOut),
            (Modifier::SLOW_BLINK, CrosstermAttribute::SlowBlink),
            (Modifier::RAPID_BLINK, CrosstermAttribute::RapidBlink),
        ] {
            if added.contains(modifier) {
                queue!(writer, SetAttribute(attribute))?;
            }
        }
        Ok(())
    }
}

/// Keeps ratatui's inline viewport usable when a terminal does not answer a cursor-position query.
pub(crate) struct CursorFallbackBackend<B> {
    inner: B,
    last_known_cursor_position: Position,
    cursor_queries_supported: bool,
}

impl<B> CursorFallbackBackend<B> {
    pub(crate) fn new(inner: B) -> Self {
        Self {
            inner,
            last_known_cursor_position: Position::ORIGIN,
            cursor_queries_supported: true,
        }
    }

    fn resolve_cursor_position(&mut self, result: io::Result<Position>) -> Position {
        match result {
            Ok(position) => self.last_known_cursor_position = position,
            Err(_) => self.cursor_queries_supported = false,
        }
        self.last_known_cursor_position
    }
}

impl<B: Write> Write for CursorFallbackBackend<B> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<B: Backend> Backend for CursorFallbackBackend<B> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, count: u16) -> io::Result<()> {
        self.inner.append_lines(count)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        if !self.cursor_queries_supported {
            return Ok(self.last_known_cursor_position);
        }
        let result = self.inner.get_cursor_position();
        Ok(self.resolve_cursor_position(result))
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        self.inner.set_cursor_position(position)?;
        self.last_known_cursor_position = position;
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub(crate) fn set_alternate_screen_active(active: bool) {
    ALTERNATE_SCREEN_ACTIVE.store(active, Ordering::Relaxed);
}

pub(crate) fn clear_inline_scrollback(terminal: &mut InteractiveTerminal) -> io::Result<()> {
    let size = terminal.size()?;
    terminal.set_cursor_position(Position::ORIGIN)?;
    let backend = terminal.backend_mut();
    Write::write_all(backend, b"\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[H")?;
    Write::flush(backend)?;
    let result = terminal.resize(ratatui::layout::Rect::new(0, 0, size.width, size.height));
    if result.is_ok() {
        // `Terminal::resize` resets the inactive buffer, while the inline rebuild begins with
        // the previously rendered buffer still current. Clear it too so the replay-loading
        // frame cannot carry old transcript rows into the next scrollback insertion.
        terminal.current_buffer_mut().reset();
    }
    result
}

pub(crate) fn copy_to_terminal_clipboard(value: &str) -> io::Result<(usize, bool)> {
    let (value, truncated) = bounded_utf8_prefix(value, MAX_OSC52_BYTES);
    let sequence = osc52_sequence(value);
    let mut stdout = io::stdout();
    stdout.write_all(sequence.as_bytes())?;
    stdout.flush()?;
    Ok((value.len(), truncated))
}

pub(crate) fn edit_prompt_externally(initial: &str) -> io::Result<String> {
    let mut file = tempfile::Builder::new()
        .prefix("golutra-prompt-")
        .suffix(".md")
        .tempfile()?;
    file.write_all(initial.as_bytes())?;
    file.flush()?;

    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("EDITOR").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| if cfg!(windows) { "notepad" } else { "vi" }.to_owned());
    let arguments = shlex::split(&editor)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "editor command is invalid"))?;
    let (program, editor_args) = arguments
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "editor command is empty"))?;

    let status = with_suspended_terminal(|| {
        Command::new(program)
            .args(editor_args)
            .arg(file.path())
            .status()
    })?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "editor exited with status {status}"
        )));
    }
    fs::read_to_string(file.path())
}

pub(crate) fn run_terminal_session(command: Option<&str>) -> io::Result<()> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("COMSPEC").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }.to_owned());
    let status = with_suspended_terminal(|| {
        let mut process = Command::new(&shell);
        if let Some(command) = command.filter(|command| !command.trim().is_empty()) {
            if cfg!(windows) {
                process.args(["/C", command]);
            } else {
                process.args(["-lc", command]);
            }
        }
        process.status()
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "terminal command exited with status {status}"
        )))
    }
}

fn with_suspended_terminal<T>(operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    suspend_terminal()?;
    let result = operation();
    let restored = resume_terminal();
    match (result, restored) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(operation_error), Err(restore_error)) => Err(io::Error::other(format!(
            "operation failed: {operation_error}; terminal restore failed: {restore_error}"
        ))),
    }
}

fn suspend_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    let alternate_screen = ALTERNATE_SCREEN_ACTIVE.load(Ordering::Relaxed);
    if let Err(error) = suspend_terminal_output(&mut io::stdout(), alternate_screen) {
        let output_restore = resume_terminal_output(&mut io::stdout(), alternate_screen);
        let raw_restore = enable_raw_mode();
        return Err(combine_terminal_errors(
            "suspend terminal output",
            error,
            [
                ("restore terminal output", output_restore),
                ("restore raw mode", raw_restore),
            ],
        ));
    }
    Ok(())
}

fn resume_terminal() -> io::Result<()> {
    let output = resume_terminal_output(
        &mut io::stdout(),
        ALTERNATE_SCREEN_ACTIVE.load(Ordering::Relaxed),
    );
    let raw_mode = enable_raw_mode();
    combine_terminal_results([
        ("restore terminal output", output),
        ("restore raw mode", raw_mode),
    ])
}

fn combine_terminal_results<const N: usize>(
    results: [(&'static str, io::Result<()>); N],
) -> io::Result<()> {
    let failures = results
        .into_iter()
        .filter_map(|(label, result)| result.err().map(|error| format!("{label}: {error}")))
        .collect::<Vec<_>>();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(failures.join("; ")))
    }
}

fn combine_terminal_errors<const N: usize>(
    label: &'static str,
    error: io::Error,
    recovery: [(&'static str, io::Result<()>); N],
) -> io::Error {
    let mut failures = vec![format!("{label}: {error}")];
    failures.extend(recovery.into_iter().filter_map(|(label, result)| {
        result
            .err()
            .map(|recovery_error| format!("{label}: {recovery_error}"))
    }));
    io::Error::other(failures.join("; "))
}

fn suspend_terminal_output(writer: &mut impl Write, alternate_screen: bool) -> io::Result<()> {
    execute!(
        writer,
        DisableBracketedPaste,
        crossterm::event::DisableMouseCapture
    )?;
    if alternate_screen {
        execute!(writer, LeaveAlternateScreen)?;
    }
    Ok(())
}

fn resume_terminal_output(writer: &mut impl Write, alternate_screen: bool) -> io::Result<()> {
    if alternate_screen {
        execute!(
            writer,
            EnterAlternateScreen,
            crossterm::event::EnableMouseCapture
        )?;
    }
    execute!(writer, EnableBracketedPaste)
}

fn osc52_sequence(value: &str) -> String {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(value.as_bytes()))
}

fn bounded_utf8_prefix(value: &str, max_bytes: usize) -> (&str, bool) {
    if value.len() <= max_bytes {
        return (value, false);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    (&value[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_cjk_cells_are_written_as_contiguous_text() {
        let mut first = Cell::default();
        first.set_symbol("你");
        let continuation = Cell::default();
        let mut second = Cell::default();
        second.set_symbol("好");
        let second_continuation = Cell::default();
        let mut backend = ContiguousCrosstermBackend::new(Vec::new());

        backend
            .draw(
                [
                    (0, 0, &first),
                    (1, 0, &continuation),
                    (2, 0, &second),
                    (3, 0, &second_continuation),
                ]
                .into_iter(),
            )
            .expect("draw adjacent CJK cells");

        let bytes = backend.into_inner();
        let output = String::from_utf8_lossy(&bytes);
        assert!(output.contains("你好"), "terminal output was {output:?}");
        assert!(
            !output.contains("\x1b[1;2H"),
            "continuation cell was addressed: {output:?}"
        );
        assert!(
            !output.contains("\x1b[1;4H"),
            "continuation cell was addressed: {output:?}"
        );
    }
    use ratatui::backend::TestBackend;

    #[test]
    fn osc52_payload_is_base64_encoded() {
        assert_eq!(osc52_sequence("hello"), "\u{1b}]52;c;aGVsbG8=\u{7}");
    }

    #[test]
    fn clipboard_bound_never_splits_utf8() {
        let (prefix, truncated) = bounded_utf8_prefix("ab你cd", 4);
        assert_eq!(prefix, "ab");
        assert!(truncated);
    }

    #[test]
    fn suspended_terminal_preserves_inline_and_alternate_screen_modes() {
        let mut alternate_suspend = Vec::new();
        suspend_terminal_output(&mut alternate_suspend, true).expect("alternate suspend");
        let mut alternate_resume = Vec::new();
        resume_terminal_output(&mut alternate_resume, true).expect("alternate resume");
        assert!(contains_bytes(&alternate_suspend, b"\x1b[?1049l"));
        assert!(contains_bytes(&alternate_resume, b"\x1b[?1049h"));

        let mut inline_suspend = Vec::new();
        suspend_terminal_output(&mut inline_suspend, false).expect("inline suspend");
        let mut inline_resume = Vec::new();
        resume_terminal_output(&mut inline_resume, false).expect("inline resume");
        assert!(!contains_bytes(&inline_suspend, b"\x1b[?1049l"));
        assert!(!contains_bytes(&inline_resume, b"\x1b[?1049h"));

        for output in [alternate_suspend, inline_suspend] {
            assert!(contains_bytes(&output, b"\x1b[?1000l"));
        }
        assert!(contains_bytes(&alternate_resume, b"\x1b[?1000h"));
        assert!(!contains_bytes(&inline_resume, b"\x1b[?1000h"));
    }

    #[test]
    fn terminal_restore_attempts_report_every_failed_stage() {
        let error = combine_terminal_results([
            ("output", Err(io::Error::other("closed"))),
            ("raw", Err(io::Error::other("unsupported"))),
        ])
        .expect_err("combined failure");

        assert!(error.to_string().contains("output: closed"));
        assert!(error.to_string().contains("raw: unsupported"));
    }

    #[test]
    fn cursor_backend_uses_the_last_known_position_when_queries_fail() {
        let mut backend = CursorFallbackBackend::new(TestBackend::new(80, 24));
        backend
            .set_cursor_position(Position::new(7, 9))
            .expect("set cursor");

        let position = backend.resolve_cursor_position(Err(io::Error::other("no DSR reply")));

        assert_eq!(position, Position::new(7, 9));
        assert!(!backend.cursor_queries_supported);
        assert_eq!(
            backend.get_cursor_position().expect("cached cursor"),
            Position::new(7, 9)
        );
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
