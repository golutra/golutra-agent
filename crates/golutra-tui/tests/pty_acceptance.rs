#![cfg(unix)]

//! 真实交互二进制的验收覆盖。
//!
//! 这里故意不使用 ratatui 的 `TestBackend`：进程连接原生 PTY，ANSI 字节被增量解析到
//! 一个感知 Unicode 宽度的小型屏幕模型。夹具使用进程内 mock provider，不访问外部服务。

use std::{
    io::{Read, Write},
    path::Path,
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tempfile::tempdir;
use unicode_width::UnicodeWidthChar;

const ANSI_ESC: u8 = 0x1b;

struct PtyHarness {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    chunks: Receiver<Vec<u8>>,
    buffered: Vec<u8>,
}

impl Drop for PtyHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

impl PtyHarness {
    fn spawn(home: &Path, cwd: &Path, width: u16, height: u16) -> Self {
        let system = native_pty_system();
        let pair = system
            .openpty(PtySize {
                rows: height,
                cols: width,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open PTY");
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_golutra-tui"));
        command.arg("--cwd");
        command.arg(cwd);
        command.env("GOLUTRA_HOME", home);
        command.env("GOLUTRA_PROVIDER_MODE", "mock");
        command.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(command).expect("spawn TUI binary");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
        let (sender, chunks) = mpsc::channel();
        thread::spawn(move || {
            let mut bytes = [0_u8; 4096];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if sender.send(bytes[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let writer = pair.master.take_writer().expect("take PTY writer");
        Self {
            master: pair.master,
            writer,
            child,
            chunks,
            buffered: Vec::new(),
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write PTY input");
        self.writer.flush().expect("flush PTY input");
    }

    fn collect_for(&mut self, duration: Duration) -> Vec<u8> {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self
                .chunks
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(chunk) => {
                    self.buffered.extend_from_slice(&chunk);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        std::mem::take(&mut self.buffered)
    }

    fn collect_until(&mut self, needle: &[u8], timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        loop {
            if contains_bytes(&self.buffered, needle) || Instant::now() >= deadline {
                return std::mem::take(&mut self.buffered);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self
                .chunks
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(chunk) => self.buffered.extend_from_slice(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return std::mem::take(&mut self.buffered);
                }
            }
        }
    }

    fn wait(mut self) -> (Vec<u8>, portable_pty::ExitStatus) {
        let status = self.child.wait().expect("wait TUI binary");
        let tail = self.collect_for(Duration::from_millis(200));
        (tail, status)
    }

    fn resize(&self, width: u16, height: u16) {
        self.master
            .resize(PtySize {
                rows: height,
                cols: width,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize PTY");
    }
}

#[derive(Clone, Debug, Default)]
struct Cell {
    symbol: String,
    continuation: bool,
}

struct Emulator {
    width: usize,
    height: usize,
    cursor_x: usize,
    cursor_y: usize,
    cells: Vec<Cell>,
    esc: bool,
    osc: bool,
    osc_esc: bool,
    csi: Option<Vec<u8>>,
    utf8: Vec<u8>,
    erase_display_count: usize,
    absolute_cursor_moves: usize,
}

impl Emulator {
    fn new(width: u16, height: u16) -> Self {
        Self {
            width: usize::from(width),
            height: usize::from(height),
            cursor_x: 0,
            cursor_y: 0,
            cells: vec![Cell::default(); usize::from(width) * usize::from(height)],
            esc: false,
            osc: false,
            osc_esc: false,
            csi: None,
            utf8: Vec::new(),
            erase_display_count: 0,
            absolute_cursor_moves: 0,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.osc {
                if self.osc_esc && byte == b'\\' {
                    self.osc = false;
                    self.osc_esc = false;
                } else if byte == 0x07 {
                    self.osc = false;
                } else {
                    self.osc_esc = byte == ANSI_ESC;
                }
                continue;
            }
            if let Some(csi) = &mut self.csi {
                csi.push(byte);
                if (0x40..=0x7e).contains(&byte) {
                    let sequence = std::mem::take(csi);
                    self.csi = None;
                    self.apply_csi(&sequence);
                }
                continue;
            }
            if self.esc {
                self.esc = false;
                if byte == b'[' {
                    self.csi = Some(Vec::new());
                } else if byte == b']' {
                    self.osc = true;
                }
                continue;
            }
            if byte == ANSI_ESC {
                self.flush_utf8();
                self.esc = true;
            } else if byte.is_ascii() {
                self.flush_utf8();
                match byte {
                    b'\r' => self.cursor_x = 0,
                    b'\n' => self.cursor_y = (self.cursor_y + 1).min(self.height.saturating_sub(1)),
                    0x08 => self.cursor_x = self.cursor_x.saturating_sub(1),
                    0x20..=0x7e => self.put_char(char::from(byte)),
                    _ => {}
                }
            } else {
                self.utf8.push(byte);
                self.flush_utf8();
            }
        }
        self.flush_utf8();
    }

    fn flush_utf8(&mut self) {
        while !self.utf8.is_empty() {
            let Ok(text) = std::str::from_utf8(&self.utf8) else {
                if self.utf8.len() >= 4 {
                    self.utf8.remove(0);
                }
                return;
            };
            let Some(character) = text.chars().next() else {
                return;
            };
            let len = character.len_utf8();
            if self.utf8.len() < len {
                return;
            }
            self.utf8.drain(..len);
            self.put_char(character);
        }
    }

    fn put_char(&mut self, character: char) {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width == 0 {
            return;
        }
        if width > 1 && self.cursor_x + width > self.width {
            return;
        }
        let index = self.cursor_y * self.width + self.cursor_x;
        self.cells[index] = Cell {
            symbol: character.to_string(),
            continuation: false,
        };
        for offset in 1..width {
            self.cells[index + offset] = Cell {
                symbol: String::new(),
                continuation: true,
            };
        }
        self.cursor_x += width;
        if self.cursor_x >= self.width {
            self.cursor_x = self.width.saturating_sub(1);
        }
    }

    fn apply_csi(&mut self, sequence: &[u8]) {
        let Some(&final_byte) = sequence.last() else {
            return;
        };
        let params = String::from_utf8_lossy(&sequence[..sequence.len() - 1]);
        let params = params.trim_start_matches('?');
        let values = params
            .split(';')
            .map(|part| part.parse::<usize>().unwrap_or(0))
            .collect::<Vec<_>>();
        let first = |default| {
            values
                .first()
                .copied()
                .filter(|value| *value != 0)
                .unwrap_or(default)
        };
        match final_byte {
            b'H' | b'f' => {
                self.cursor_y = first(1)
                    .saturating_sub(1)
                    .min(self.height.saturating_sub(1));
                self.cursor_x = values
                    .get(1)
                    .copied()
                    .filter(|value| *value != 0)
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(self.width.saturating_sub(1));
                self.absolute_cursor_moves += 1;
            }
            b'G' | b'`' => {
                self.cursor_x = first(1).saturating_sub(1).min(self.width.saturating_sub(1))
            }
            b'A' => self.cursor_y = self.cursor_y.saturating_sub(first(1)),
            b'B' => self.cursor_y = (self.cursor_y + first(1)).min(self.height.saturating_sub(1)),
            b'C' => self.cursor_x = (self.cursor_x + first(1)).min(self.width.saturating_sub(1)),
            b'D' => self.cursor_x = self.cursor_x.saturating_sub(first(1)),
            b'J' => {
                if first(0) == 2 {
                    self.erase_display_count += 1;
                }
                if first(0) == 2 {
                    self.cells.fill(Cell::default());
                }
            }
            b'K' => {
                let start = self.cursor_y * self.width + self.cursor_x;
                self.cells[start..(self.cursor_y + 1) * self.width].fill(Cell::default());
            }
            _ => {}
        }
    }

    fn text(&self) -> String {
        self.cells
            .chunks(self.width)
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.width = usize::from(width);
        self.height = usize::from(height);
        self.cursor_x = self.cursor_x.min(self.width.saturating_sub(1));
        self.cursor_y = self.cursor_y.min(self.height.saturating_sub(1));
        self.cells.resize(self.width * self.height, Cell::default());
    }
}

fn install_mock_provider(home: &Path) {
    std::fs::create_dir_all(home).expect("create mock home");
    std::fs::write(home.join("provider.json"), r#"{"version":2,"active_profile":"mock","profiles":[{"name":"mock","protocol":"mock","model_id":"mock-model","enabled":true}]}"#).expect("write provider fixture");
}

#[test]
fn interactive_binary_accepts_real_pty_unicode_redraw_resize_and_restores_terminal() {
    let home = tempdir().expect("home tempdir");
    let workspace = tempdir().expect("workspace tempdir");
    install_mock_provider(home.path());
    let mut pty = PtyHarness::spawn(home.path(), workspace.path(), 80, 24);
    let mut emulator = Emulator::new(80, 24);
    // Embedded runtime 会在首帧前执行迁移；标题出现后即可继续，同时保留有界冷启动超时。
    let initial_raw = pty.collect_until(b"GOLUTRA", Duration::from_secs(8));
    for chunk in initial_raw.chunks(37) {
        emulator.feed(chunk);
    }
    let initial_text = emulator.text();
    assert!(
        initial_text.contains("GOLUTRA"),
        "initial frame missing title: {initial_text:?}; raw={:?}",
        String::from_utf8_lossy(&initial_raw)
    );
    assert!(
        emulator.absolute_cursor_moves > 0,
        "renderer did not use absolute cursor addressing"
    );

    pty.write("你好 incremental".as_bytes());
    let redraw = pty.collect_for(Duration::from_millis(400));
    assert!(!redraw.is_empty(), "typing produced no PTY redraw");
    let moves_before_redraw = emulator.absolute_cursor_moves;
    for chunk in redraw.chunks(3) {
        emulator.feed(chunk);
    }
    assert!(
        emulator.absolute_cursor_moves > moves_before_redraw,
        "middle redraw did not address the changed region"
    );
    assert!(
        emulator.text().contains("你好"),
        "CJK input was lost: {:?}",
        emulator.text()
    );
    assert!(
        emulator.cells.iter().any(|cell| cell.continuation),
        "CJK cells have no width-2 continuation"
    );

    pty.resize(42, 10);
    let resized = pty.collect_for(Duration::from_millis(700));
    emulator.resize(42, 10);
    for chunk in resized.chunks(5) {
        emulator.feed(chunk);
    }
    assert!(
        contains_bytes(&resized, b"\x1b["),
        "resize did not trigger ANSI redraw"
    );
    assert_eq!((emulator.width, emulator.height), (42, 10));

    pty.write(b"\x15/quit\r");
    let (tail, status) = pty.wait();
    assert!(status.success(), "TUI exited unsuccessfully: {status:?}");
    assert!(
        contains_bytes(&tail, b"\x1b[?2004l"),
        "bracketed paste was not disabled on exit"
    );
    assert!(
        contains_bytes(&tail, b"\x1b[?25h"),
        "cursor was not shown on exit"
    );
    let total = [
        initial_raw.as_slice(),
        redraw.as_slice(),
        resized.as_slice(),
        tail.as_slice(),
    ]
    .concat();
    let clears = total
        .windows(4)
        .filter(|bytes| *bytes == b"\x1b[2J")
        .count();
    assert!(
        clears <= 1,
        "full-screen clear repeated across frames ({clears} occurrences)"
    );
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
