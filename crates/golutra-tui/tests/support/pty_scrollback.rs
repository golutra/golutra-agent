//! 用真实 PTY 与 VT 屏幕/滚动历史验证归档，不以原始输出中出现过文字代替可见性验收。

use super::*;
use golutra_auth::{CredentialRef, SecretKind};
use golutra_config::{ProviderConfigPaths, ProviderProfile, ProviderSettings};
use golutra_llm::ProviderProtocol;
use serde_json::json;
use std::net::TcpListener;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

// vt100 解析屏幕、Unicode、滚动与 alt-screen；补齐其未实现的 xterm ED(3) 扩展。
// 仅清空滚动历史，保留可见屏幕及光标，不能用清空整个测试模型掩盖覆盖错误。
struct ScreenModel {
    parser: vt100::Parser,
    last_bytes: [u8; 4],
}

impl ScreenModel {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 10_000),
            last_bytes: [0; 4],
        }
    }

    fn process(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.parser.process(&[byte]);
            self.last_bytes.rotate_left(1);
            self.last_bytes[3] = byte;
            if &self.last_bytes == b"\x1b[3J" && !self.parser.screen().alternate_screen() {
                let state = self.parser.screen().state_formatted();
                let (rows, cols) = self.parser.screen().size();
                self.parser = vt100::Parser::new(rows, cols, 10_000);
                self.parser.process(&state);
            }
        }
    }
}

impl std::ops::Deref for ScreenModel {
    type Target = vt100::Parser;
    fn deref(&self) -> &Self::Target {
        &self.parser
    }
}

impl std::ops::DerefMut for ScreenModel {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.parser
    }
}

struct FixtureServer {
    url: String,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    requests: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

impl FixtureServer {
    fn new(responses: Vec<String>) -> Self {
        Self::with_rounds(
            responses
                .into_iter()
                .map(|text| (text, Vec::new()))
                .collect(),
        )
    }

    fn with_rounds(responses: Vec<(String, Vec<serde_json::Value>)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = requests.clone();
        let worker = thread::spawn(move || {
            let mut responses = responses.into_iter();
            while !flag.load(Ordering::Relaxed) {
                let Ok((mut socket, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let Ok(count) = socket.read(&mut buffer) else {
                        break;
                    };
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                    && let Ok(body) = serde_json::from_slice(&request[end + 4..])
                {
                    captured.lock().unwrap().push(body);
                }
                let (content, calls) = responses
                    .next()
                    .unwrap_or_else(|| ("FIXTURE_DONE".to_owned(), Vec::new()));
                let mut frames = Vec::new();
                for chunk in content.chars().collect::<Vec<_>>().chunks(17) {
                    frames.push(format!("data: {}\n\n", json!({
                        "id": "pty", "object": "chat.completion.chunk", "created": 0,
                        "model": "pty-model", "choices": [{"index":0,"delta":{"content":chunk.iter().collect::<String>()},"finish_reason":null}]
                    })));
                }
                if !calls.is_empty() {
                    frames.push(format!("data: {}\n\n", json!({
                        "id":"pty", "object":"chat.completion.chunk", "created":0,"model":"pty-model",
                        "choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":null}]
                    })));
                }
                frames.push(format!("data: {}\n\ndata: [DONE]\n\n", json!({
                    "id":"pty", "object":"chat.completion.chunk", "created":0,"model":"pty-model",
                    "choices":[{"index":0,"delta":{},"finish_reason":if calls.is_empty() {"stop"} else {"tool_calls"}}],
                    "usage":{"prompt_tokens":100,"completion_tokens":100,"total_tokens":200}
                })));
                let length: usize = frames.iter().map(String::len).sum();
                if write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n").is_err() { continue; }
                for frame in frames {
                    if flag.load(Ordering::Relaxed) || socket.write_all(frame.as_bytes()).is_err() {
                        break;
                    }
                    let _ = socket.flush();
                    thread::sleep(Duration::from_millis(8));
                }
            }
        });
        Self {
            url,
            stopped,
            worker: Some(worker),
            requests,
        }
    }

    fn install(&self, home: &Path) {
        let credential =
            CredentialRef::environment("GOLUTRA_PTY_TEST_KEY", SecretKind::ApiKey).unwrap();
        let profile = ProviderProfile::live_profile(
            "pty-local",
            ProviderProtocol::OpenAiCompatible,
            self.url.clone(),
            "pty-model".to_owned(),
            credential,
        )
        .unwrap();
        let mut settings = ProviderSettings::default();
        settings.upsert_profile(profile, true);
        settings
            .save(&ProviderConfigPaths::from_home(home).unwrap().user_config)
            .unwrap();
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn all_terminal_rows(parser: &mut ScreenModel) -> String {
    parser.screen_mut().set_scrollback(usize::MAX);
    let count = parser.screen().scrollback();
    let width = parser.screen().size().1;
    let mut rows = Vec::new();
    for offset in (1..=count).rev() {
        parser.screen_mut().set_scrollback(offset);
        rows.push(parser.screen().rows(0, width).next().unwrap());
    }
    parser.screen_mut().set_scrollback(0);
    rows.extend(parser.screen().rows(0, width));
    rows.iter()
        .map(|row| row.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_once_in_order(text: &str, needles: &[String]) {
    assert_message_spacing(text);
    assert_eq!(
        text.matches("SHELL_HISTORY_MARKER").count(),
        1,
        "shell prefix must survive:\n{text}"
    );
    assert_eq!(
        text.matches("START_COMMAND_MARKER").count(),
        1,
        "launch command must survive:\n{text}"
    );
    let mut previous = 0;
    for needle in needles {
        assert_eq!(
            text.matches(needle).count(),
            1,
            "expected one {needle:?} in terminal history:\n{text}"
        );
        let index = text.find(needle).unwrap();
        assert!(index >= previous, "out of order {needle:?}:\n{text}");
        previous = index + needle.len();
    }
}

fn assert_message_spacing(text: &str) {
    let rows = text.lines().collect::<Vec<_>>();
    for (index, row) in rows.iter().enumerate().skip(2) {
        if !(row.starts_with("› ") || row.starts_with("• "))
            || row.contains("Ask Golutra")
            || row.contains("tokens/s")
        {
            continue;
        }
        assert!(
            rows[index - 1].trim().is_empty(),
            "missing message separator before {row:?}:\n{text}"
        );
        assert!(
            !rows[index - 2].trim().is_empty(),
            "duplicate message separator before {row:?}:\n{text}"
        );
    }
}

fn submit(pty: &mut PtyHarness, parser: &mut ScreenModel, text: &str) {
    pty.write(format!("\x1b[200~{text}\x1b[201~").as_bytes());
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    pty.write(b"\r");
}

fn assert_idle_composer_at_bottom(parser: &ScreenModel) {
    assert_idle_composer_compact(parser, true);
}

fn assert_idle_composer_compact(parser: &ScreenModel, at_bottom: bool) {
    let width = parser.screen().size().1;
    let rows = parser.screen().rows(0, width).collect::<Vec<_>>();
    let last = rows
        .iter()
        .rposition(|line| line.contains("pty-model"))
        .expect("visible footer");
    if at_bottom {
        assert_eq!(
            last,
            rows.len() - 1,
            "footer should reach bottom:\n{}",
            rows.join("\n")
        );
    }
    assert!(
        rows[last].contains("pty-model"),
        "footer must occupy the last screen row:\n{}",
        rows.join("\n")
    );
    assert!(
        rows[last - 1].contains("Ask Golutra"),
        "idle composer must have no internal padding:\n{}",
        rows.join("\n")
    );
    assert!(
        rows[last - 2].starts_with('─'),
        "composer border:\n{}",
        rows.join("\n")
    );
}

#[test]
fn startup_banner_stays_next_to_shell_command_at_any_cursor_row() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let server = FixtureServer::new(vec![]);
    server.install(home.path());
    // 编译日志常使 shell 光标触底；从空屏启动无法覆盖这一真实入口。
    for (width, height, shell_lines) in [(120, 48, 0), (120, 48, 44), (120, 48, 60), (60, 24, 30)] {
        let mut pty = PtyHarness::spawn_with_shell_lines(
            home.path(),
            workspace.path(),
            width,
            height,
            true,
            None,
            shell_lines,
        );
        let mut parser = ScreenModel::new(height, width);
        parser.process(&pty.collect_until(b"Ask Golutra", Duration::from_secs(8)));
        parser.process(&pty.collect_for(Duration::from_millis(300)));
        let text = all_terminal_rows(&mut parser);
        assert_once_in_order(&text, &[]);
        let rows = text.lines().collect::<Vec<_>>();
        let command = rows
            .iter()
            .position(|row| row.contains("START_COMMAND_MARKER"))
            .unwrap();
        let banner = rows
            .iter()
            .enumerate()
            .skip(command + 1)
            .find(|(_, row)| row.contains("██") || row.contains("GOLUTRA"))
            .map(|(index, _)| index)
            .expect("startup banner");
        assert!(
            banner - command <= 3,
            "startup must not pad before the banner (shell_lines={shell_lines}):\n{text}"
        );
        assert!(!parser.screen().alternate_screen());
        assert_eq!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        );
        submit(&mut pty, &mut parser, "/quit");
        assert!(pty.wait().1.success());
    }
}

#[test]
fn slash_suggestions_expand_inline_complete_and_restore_history() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let markers = (1..=25)
        .map(|n| format!("SLASH_HISTORY_{n:03}"))
        .collect::<Vec<_>>();
    let server = FixtureServer::new(vec![markers.join("\n\n")]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 100, 26, true);
    let mut parser = ScreenModel::new(26, 100);
    parser.process(&pty.collect_until(b"Ask Golutra", Duration::from_secs(8)));
    // 初始短历史和长回复触底后都必须能完整显示候选。
    for after_reply in [false, true] {
        if after_reply {
            submit(&mut pty, &mut parser, "生成历史");
            parser.process(&pty.collect_for(Duration::from_secs(3)));
        }
        let idle_y = screen_row(&parser, "Ask Golutra");
        let last_history_y = after_reply.then(|| screen_row(&parser, "SLASH_HISTORY_025"));
        pty.write(b"/");
        let expanded = pty.collect_for(Duration::from_millis(350));
        assert!(
            !expanded.windows(4).any(|bytes| bytes == b"\x1b[6n"),
            "expansion must not query cursor"
        );
        parser.process(&expanded);
        let expanded_y = screen_row(&parser, "› /");
        if let Some(history_y) = last_history_y {
            assert!(
                expanded_y < idle_y,
                "full screen must scroll up for suggestions"
            );
            assert_eq!(
                screen_row(&parser, "SLASH_HISTORY_025"),
                history_y - (idle_y - expanded_y)
            );
        } else {
            assert_eq!(
                expanded_y, idle_y,
                "free space should be consumed before scrolling"
            );
        }
        let text = parser.screen().contents();
        for command in ["/help", "/model", "/resume", "/status", "/new"] {
            assert!(text.contains(command), "missing {command}:\n{text}");
        }
        assert!(!parser.screen().alternate_screen());
        assert_eq!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        );
        // 筛选与恢复列表只改变高度，不能把历史反向拉回屏幕。
        pty.write(b"re");
        parser.process(&pty.collect_for(Duration::from_millis(200)));
        assert_eq!(screen_row(&parser, "› /re"), expanded_y);
        pty.write(b"\x7f\x7f");
        parser.process(&pty.collect_for(Duration::from_millis(200)));
        assert_eq!(screen_row(&parser, "› /"), expanded_y);
        pty.write(b"\x1b");
        parser.process(&pty.collect_for(Duration::from_millis(350)));
        let text = parser.screen().contents();
        assert!(text.lines().any(|line| line.trim() == "› /"), "{text}");
        assert_eq!(screen_row(&parser, "› /"), expanded_y);
        assert!(
            !text.contains("open contextual keyboard reference"),
            "{text}"
        );
        pty.write(b"\x15");
        parser.process(&pty.collect_for(Duration::from_millis(250)));
    }
    pty.write(b"/");
    parser.process(&pty.collect_for(Duration::from_millis(250)));
    pty.resize(60, 6);
    parser.screen_mut().set_size(6, 60);
    parser.process(&pty.collect_for(Duration::from_millis(350)));
    pty.write(b"\x1b[B\x1b[B\x1b[B\x1b[B");
    parser.process(&pty.collect_for(Duration::from_millis(350)));
    assert!(parser.screen().contents().contains("› /new"));
    assert!(parser.screen().contents().contains("Esc close"));
    pty.resize(100, 26);
    parser.screen_mut().set_size(26, 100);
    parser.process(&pty.collect_for(Duration::from_millis(350)));
    pty.write(b"\x1b[A\t");
    parser.process(&pty.collect_for(Duration::from_millis(350)));
    assert!(parser.screen().contents().contains("› /status"));
    assert!(!parser.screen().contents().contains("• Status"));
    pty.write(b"\r");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    assert!(parser.screen().contents().contains("• Status"));
    assert_once_in_order(&all_terminal_rows(&mut parser), &markers);
    pty.write(b"/re\t");
    parser.process(&pty.collect_for(Duration::from_millis(350)));
    assert!(parser.screen().contents().contains("› /resume"));
    assert!(!parser.screen().alternate_screen());
    pty.write(b"\r");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    assert!(parser.screen().alternate_screen());
    pty.write(b"\x1b");
    parser.process(&pty.collect_for(Duration::from_millis(350)));
    assert!(!parser.screen().alternate_screen());
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    assert_once_in_order(&all_terminal_rows(&mut parser), &markers);
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}

fn screen_row(parser: &ScreenModel, marker: &str) -> usize {
    parser
        .screen()
        .contents()
        .lines()
        .position(|line| line.contains(marker))
        .unwrap_or_else(|| panic!("missing {marker}:\n{}", parser.screen().contents()))
}

fn wait_for_visible(pty: &mut PtyHarness, parser: &mut ScreenModel, marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !parser.screen().contents().contains(marker) && Instant::now() < deadline {
        parser.process(&pty.collect_for(Duration::from_millis(50)));
    }
    assert!(
        parser.screen().contents().contains(marker),
        "missing {marker}:\n{}",
        parser.screen().contents()
    );
}

#[test]
fn pending_inputs_move_from_preview_to_history_for_tab_and_enter() {
    for (key, preview) in [
        (b'\t', "Queued follow-up inputs"),
        (b'\r', "Pending current-turn inputs"),
    ] {
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let content = format!(
            "FIRST_REPLY_BEGIN\n{}\nFIRST_REPLY_END",
            "Streaming previous reply.\n".repeat(250)
        );
        let server = FixtureServer::new(vec![content, "SECOND_REPLY_DONE".to_owned()]);
        server.install(home.path());
        let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 100, 26, true);
        let mut parser = ScreenModel::new(26, 100);
        parser.process(&pty.collect_until(b"Ask Golutra", Duration::from_secs(8)));
        submit(&mut pty, &mut parser, "first request");
        wait_for_visible(&mut pty, &mut parser, "FIRST_REPLY_BEGIN");
        pty.write(b"hi");
        parser.process(&pty.collect_for(Duration::from_millis(150)));
        pty.write(&[key]);
        wait_for_visible(&mut pty, &mut parser, preview);
        let screen = parser.screen().contents();
        assert!(screen.contains("↳ hi"), "{screen}");
        assert!(
            screen.find("↳ hi").unwrap() < screen.find("› Ask Golutra").unwrap(),
            "{screen}"
        );
        assert!(!all_terminal_rows(&mut parser).contains("› hi"));
        wait_for_visible(&mut pty, &mut parser, "SECOND_REPLY_DONE");
        parser.process(&pty.collect_for(Duration::from_millis(250)));
        let text = all_terminal_rows(&mut parser);
        assert_once_in_order(
            &text,
            &[
                "› first request".into(),
                "FIRST_REPLY_BEGIN".into(),
                "FIRST_REPLY_END".into(),
                "› hi".into(),
                "SECOND_REPLY_DONE".into(),
            ],
        );
        assert!(
            !text.contains(preview),
            "preview must not enter scrollback: {text}"
        );
        assert!(!parser.screen().alternate_screen());
        submit(&mut pty, &mut parser, "/quit");
        assert!(pty.wait().1.success());
    }
}

fn visible_screen_rows(parser: &ScreenModel) -> Vec<String> {
    // VT 将显式写入的空格与擦除后的空格区别保存；比较显示内容和行位置，忽略行尾编码差异。
    parser
        .screen()
        .rows(0, parser.screen().size().1)
        .map(|line| line.trim_end().to_owned())
        .collect()
}

#[test]
fn queued_tasks_and_current_turn_supplements_use_distinct_provider_batches() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let server = FixtureServer::new(vec![
        format!(
            "BATCH_START\n{}\nBATCH_END",
            "Original response still streaming.\n".repeat(350)
        ),
        "STEERS_HANDLED".into(),
        "FOLLOW_ONE_HANDLED".into(),
        "FOLLOW_TWO_HANDLED".into(),
    ]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 100, 26, true);
    let mut parser = ScreenModel::new(26, 100);
    parser.process(&pty.collect_until(b"Ask Golutra", Duration::from_secs(8)));
    submit(&mut pty, &mut parser, "begin");
    wait_for_visible(&mut pty, &mut parser, "BATCH_START");
    for (text, key) in [
        ("FOLLOW_ONE", b'\t'),
        ("FOLLOW_TWO", b'\t'),
        ("STEER_ONE", b'\r'),
        ("STEER_TWO", b'\r'),
    ] {
        pty.write(text.as_bytes());
        parser.process(&pty.collect_for(Duration::from_millis(150)));
        pty.write(&[key]);
        wait_for_visible(&mut pty, &mut parser, &format!("↳ {text}"));
    }
    wait_for_visible(&mut pty, &mut parser, "FOLLOW_TWO_HANDLED");
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let markers = |index: usize| {
        requests[index]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .filter_map(|message| message["content"].as_str())
            .filter(|text| text.starts_with("STEER_") || text.starts_with("FOLLOW_"))
            .collect::<Vec<_>>()
    };
    assert_eq!(markers(1), vec!["STEER_ONE", "STEER_TWO"]);
    assert_eq!(markers(2), vec!["STEER_ONE", "STEER_TWO", "FOLLOW_ONE"]);
    assert_eq!(
        markers(3),
        vec!["STEER_ONE", "STEER_TWO", "FOLLOW_ONE", "FOLLOW_TWO"]
    );
    drop(requests);
    parser.process(&pty.collect_for(Duration::from_millis(250)));
    let text = all_terminal_rows(&mut parser);
    assert_once_in_order(
        &text,
        &[
            "› STEER_ONE".into(),
            "› STEER_TWO".into(),
            "STEERS_HANDLED".into(),
            "› FOLLOW_ONE".into(),
            "FOLLOW_ONE_HANDLED".into(),
            "› FOLLOW_TWO".into(),
            "FOLLOW_TWO_HANDLED".into(),
        ],
    );
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}

#[test]
fn interrupt_sends_pending_supplements_once_as_one_message_and_preserves_draft() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let server = FixtureServer::new(vec![
        format!(
            "INTERRUPT_START\n{}",
            "Waiting for a user correction.\n".repeat(500)
        ),
        "RECOVERY_DONE".into(),
    ]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 100, 26, true);
    let mut parser = ScreenModel::new(26, 100);
    parser.process(&pty.collect_until(b"Ask Golutra", Duration::from_secs(8)));
    submit(&mut pty, &mut parser, "begin");
    wait_for_visible(&mut pty, &mut parser, "INTERRUPT_START");
    for text in ["STEER_FIRST", "STEER_SECOND"] {
        pty.write(text.as_bytes());
        parser.process(&pty.collect_for(Duration::from_millis(150)));
        pty.write(b"\r");
        wait_for_visible(&mut pty, &mut parser, &format!("↳ {text}"));
    }
    pty.write(b"draft keep");
    parser.process(&pty.collect_for(Duration::from_millis(150)));
    pty.write(b"\x1b");
    wait_for_visible(&mut pty, &mut parser, "RECOVERY_DONE");
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "user"
                && message["content"] == "STEER_FIRST\nSTEER_SECOND")
    );
    assert!(!requests[1].to_string().contains("draft keep"));
    drop(requests);
    assert!(parser.screen().contents().contains("› draft keep"));
    pty.write(b"\x15");
    parser.process(&pty.collect_for(Duration::from_millis(150)));
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}

#[test]
fn streaming_with_suggestions_and_status_preserves_every_message_once() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let markers = (1..=350)
        .map(|n| format!("并发候选段落{n:03}完整有序。"))
        .collect::<Vec<_>>();
    let server = FixtureServer::new(vec![format!(
        "{}\n\nPOPUP_STREAM_DONE",
        markers.join("\n\n")
    )]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 100, 26, true);
    let mut parser = ScreenModel::new(26, 100);
    parser.process(&pty.collect_until(b"Ask Golutra", Duration::from_secs(8)));
    submit(&mut pty, &mut parser, "生成时检查候选和状态");
    for _ in 0..80 {
        parser.process(&pty.collect_for(Duration::from_millis(50)));
        if all_terminal_rows(&mut parser).contains(&markers[9]) {
            break;
        }
    }
    assert!(!all_terminal_rows(&mut parser).contains("POPUP_STREAM_DONE"));
    pty.write(b"/");
    wait_for_visible(&mut pty, &mut parser, "/resume");
    pty.write(b"st\t");
    wait_for_visible(&mut pty, &mut parser, "› /status");
    assert!(!all_terminal_rows(&mut parser).contains("• Status"));
    pty.write(b"\r");
    parser.process(&pty.collect_for(Duration::from_secs(7)));
    let text = all_terminal_rows(&mut parser);
    assert_once_in_order(&text, &markers);
    for marker in [
        "SHELL_HISTORY_MARKER",
        "START_COMMAND_MARKER",
        "› 生成时检查候选和状态",
        "› /status",
        "• Status",
        "POPUP_STREAM_DONE",
    ] {
        assert_eq!(text.matches(marker).count(), 1, "{marker}:\n{text}");
    }
    assert!(
        !text.contains("↑/↓ select"),
        "transient popup must not enter history"
    );
    assert!(!parser.screen().alternate_screen());
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}

#[test]
fn completed_fullscreen_height_stream_does_not_leave_a_page_gap() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    // 单个长代码块在完成前全部属于活动尾部；完成后一次归档，不能继续为旧尾部保留一整屏。
    let markers = (1..=100)
        .map(|n| format!("TAIL_{n:03}"))
        .collect::<Vec<_>>();
    let server = FixtureServer::new(vec![format!("```text\n{}\n```", markers.join("\n"))]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 100, 40, true);
    let mut parser = ScreenModel::new(40, 100);
    parser.process(&pty.collect_until(b"Ask Golutra", Duration::from_secs(8)));
    submit(&mut pty, &mut parser, "长代码块");
    parser.process(&pty.collect_for(Duration::from_secs(3)));
    assert_idle_composer_compact(&parser, false);
    assert_eq!(
        screen_row(&parser, "Ask Golutra"),
        screen_row(&parser, "TAIL_100") + 2
    );
    submit(&mut pty, &mut parser, "/status");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    assert_once_in_order(&all_terminal_rows(&mut parser), &markers);
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}

#[test]
fn compact_anchor_survives_completion_input_shrink_and_height_only_resize() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let markers = (1..=35)
        .map(|n| format!("边缘检查{n:03}。"))
        .collect::<Vec<_>>();
    let server = FixtureServer::new(vec![format!("{}\n\nEDGE_DONE", markers.join("\n\n"))]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 100, 24, true);
    let mut parser = ScreenModel::new(24, 100);
    parser.process(&pty.collect_until(b"Ask Golutra", Duration::from_secs(8)));
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    assert!(
        parser
            .screen()
            .rows(0, 100)
            .last()
            .unwrap()
            .trim()
            .is_empty(),
        "startup should retain compact inline layout"
    );
    submit(&mut pty, &mut parser, "底部边缘测试");
    parser.process(&pty.collect_for(Duration::from_secs(3)));
    assert_once_in_order(&all_terminal_rows(&mut parser), &markers);
    assert_idle_composer_compact(&parser, false);
    let idle = pty.collect_for(Duration::from_millis(500));
    assert!(
        !idle.windows(4).any(|bytes| bytes == b"\x1b[3J"),
        "idle frames must not repeatedly rebuild history"
    );
    parser.process(&idle);

    pty.write("\x1b[200~草稿一\n草稿二\n草稿三\n草稿四\n草稿五\x1b[201~".as_bytes());
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    pty.write(b"\x15");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    assert_idle_composer_compact(&parser, false);
    assert_once_in_order(&all_terminal_rows(&mut parser), &markers);
    assert!(!all_terminal_rows(&mut parser).contains("草稿"));

    submit(&mut pty, &mut parser, "/status");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    assert_idle_composer_at_bottom(&parser);
    for height in [36, 112, 6, 28] {
        pty.resize(100, height);
        parser.screen_mut().set_size(height, 100);
        parser.process(&pty.collect_for(Duration::from_millis(800)));
        assert_idle_composer_compact(&parser, false);
        let text = all_terminal_rows(&mut parser);
        assert_once_in_order(&text, &markers);
        assert_eq!(text.matches("• Status").count(), 1, "{text}");
    }
    submit(&mut pty, &mut parser, "/resume");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    pty.resize(100, 32);
    parser.screen_mut().set_size(32, 100);
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    pty.write(b"\x1b");
    parser.process(&pty.collect_for(Duration::from_millis(800)));
    assert_idle_composer_compact(&parser, false);
    assert_once_in_order(&all_terminal_rows(&mut parser), &markers);
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}

#[test]
fn inline_scrollback_preserves_long_cjk_responses_prompts_and_status() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let markers = (1..=40)
        .map(|n| format!("段落{n:03}中文完整内容。"))
        .collect::<Vec<_>>();
    let long = format!("{}\n\nFIRST_DONE", markers.join("\n\n"));
    let code_markers = (1..=400).map(|n| format!("ROW_{n:03}")).collect::<Vec<_>>();
    let server = FixtureServer::new(vec![
        long,
        format!("```text\n{}\n```\n\nSECOND_DONE", code_markers.join("\n")),
    ]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_with_resume(
        home.path(),
        workspace.path(),
        140,
        24,
        true,
        Some("pty-history-roundtrip"),
    );
    let mut parser = ScreenModel::new(24, 140);
    parser.process(&pty.collect_until(b"Ask Golutra", Duration::from_secs(8)));
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    assert!(
        parser.screen().contents().contains("██████╗"),
        "wide startup must display the logo"
    );
    submit(&mut pty, &mut parser, "请输出第一篇");
    let mut intermediate_frames = 0;
    for _ in 0..50 {
        parser.process(&pty.collect_for(Duration::from_millis(100)));
        let snapshot = all_terminal_rows(&mut parser);
        assert_message_spacing(&snapshot);
        let mut last = 0;
        for marker in &markers {
            if let Some(index) = snapshot.find(marker) {
                assert!(index >= last, "streamed paragraphs reordered:\n{snapshot}");
                assert_eq!(
                    snapshot.matches(marker).count(),
                    1,
                    "streamed paragraph duplicated:\n{snapshot}"
                );
                last = index;
            }
        }
        if snapshot.contains(&markers[0]) && !snapshot.contains("FIRST_DONE") {
            intermediate_frames += 1;
            let answer = snapshot.split_once("› 请输出第一篇").unwrap().1;
            for line in answer.lines().take_while(|line| !line.starts_with('─')) {
                let line = line.trim().strip_prefix("• ").unwrap_or(line.trim());
                if line.is_empty() || line.contains("tokens/s") {
                    continue;
                }
                assert!(
                    markers.iter().any(|marker| marker.starts_with(line))
                        || "FIRST_DONE".starts_with(line),
                    "in-flight text is not a source prefix: {line:?}\n{snapshot}"
                );
            }
        }
        if snapshot.contains("FIRST_DONE") {
            break;
        }
    }
    assert!(
        intermediate_frames > 0,
        "must verify actual in-flight frames, not only final replacement"
    );
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    let first = all_terminal_rows(&mut parser);
    let mut expected = vec!["› 请输出第一篇".to_owned()];
    expected.extend(markers);
    expected.push("FIRST_DONE".to_owned());
    assert_once_in_order(&first, &expected);
    assert!(
        first.contains("段落001中文完整内容。\n\n"),
        "paragraph blank line lost:\n{first}"
    );

    submit(&mut pty, &mut parser, "/status");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    expected.push("• Status".to_owned());
    let status_text = all_terminal_rows(&mut parser);
    assert_once_in_order(&status_text, &expected);

    submit(&mut pty, &mut parser, "请输出第二篇");
    parser.process(&pty.collect_for(Duration::from_secs(6)));
    expected.push("› 请输出第二篇".to_owned());
    expected.extend(code_markers);
    expected.push("SECOND_DONE".to_owned());
    assert_once_in_order(&all_terminal_rows(&mut parser), &expected);

    pty.resize(80, 30);
    parser.screen_mut().set_size(30, 80);
    parser.process(&pty.collect_for(Duration::from_millis(800)));
    assert_once_in_order(&all_terminal_rows(&mut parser), &expected);
    submit(&mut pty, &mut parser, "/resume");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    pty.write(b"\x1b");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    assert_once_in_order(&all_terminal_rows(&mut parser), &expected);
    submit(&mut pty, &mut parser, "/quit");
    let (_, status) = pty.wait();
    assert!(status.success());

    // 新进程从持久化事件恢复，不能借用上一进程的内存或终端缓存补齐长回复。
    let mut resumed = PtyHarness::spawn_with_resume(
        home.path(),
        workspace.path(),
        100,
        24,
        true,
        Some("pty-history-roundtrip"),
    );
    let mut restored = ScreenModel::new(24, 100);
    restored.process(&resumed.collect_until(b"Ask Golutra", Duration::from_secs(8)));
    restored.process(&resumed.collect_for(Duration::from_millis(500)));
    expected.retain(|needle| needle != "• Status");
    assert_once_in_order(&all_terminal_rows(&mut restored), &expected);
    submit(&mut resumed, &mut restored, "/quit");
    assert!(resumed.wait().1.success());
}

#[test]
fn status_during_stream_stays_between_the_same_paragraphs_after_resize() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let markers = (1..=120)
        .map(|n| format!("流式段落{n:03}中文内容保持顺序。"))
        .collect::<Vec<_>>();
    let server = FixtureServer::new(vec![format!("{}\n\nSTREAM_DONE", markers.join("\n\n"))]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 100, 24, true);
    let mut parser = ScreenModel::new(24, 100);
    parser.process(&pty.collect_until(b"Ask Golutra", Duration::from_secs(8)));
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    submit(&mut pty, &mut parser, "流式状态测试");
    for _ in 0..30 {
        parser.process(&pty.collect_for(Duration::from_millis(50)));
        if all_terminal_rows(&mut parser).contains(&markers[0]) {
            break;
        }
    }
    submit(&mut pty, &mut parser, "/status");
    parser.process(&pty.collect_for(Duration::from_secs(3)));
    let before = all_terminal_rows(&mut parser);
    assert_once_in_order(&before, &markers);
    assert!(
        before.contains("status Running"),
        "status must be requested during streaming:\n{before}"
    );
    let status_offset = before.find("• Status").unwrap();
    let preceding = markers
        .iter()
        .filter(|marker| before.find(marker.as_str()).unwrap() < status_offset)
        .count();
    assert!(preceding > 0 && preceding < markers.len());
    pty.resize(72, 28);
    parser.screen_mut().set_size(28, 72);
    parser.process(&pty.collect_for(Duration::from_millis(800)));
    let after = all_terminal_rows(&mut parser);
    assert_once_in_order(&after, &markers);
    assert_eq!(after.matches("• Status").count(), 1);
    let status_offset = after.find("• Status").unwrap();
    assert_eq!(
        markers
            .iter()
            .filter(|marker| after.find(marker.as_str()).unwrap() < status_offset)
            .count(),
        preceding
    );
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}

#[test]
fn narration_and_real_tool_results_remain_in_order_without_raw_file_previews() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    for name in ["first.txt", "second.txt"] {
        std::fs::write(
            workspace.path().join(name),
            "<img src=\"RAW_HTML_SHOULD_STAY_IN_DETAILS\">\n",
        )
        .unwrap();
    }
    let read = |id: &str, path: &str| {
        json!({
            "index":0, "id":id, "type":"function", "function": {
                "name":"read_file", "arguments":json!({"path":path,"offset":1,"limit":20}).to_string()
            }
        })
    };
    let server = FixtureServer::with_rounds(vec![
        (
            "先读第一个文件。".into(),
            vec![read("read-first", "first.txt")],
        ),
        (
            "第一个已读，继续读第二个。".into(),
            vec![read("read-second", "second.txt")],
        ),
        ("读取完成，两个文件都已检查。".into(), vec![]),
    ]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 100, 24, true);
    let mut parser = ScreenModel::new(24, 100);
    parser.process(&pty.collect_until(b"Ask Golutra", Duration::from_secs(8)));
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    submit(&mut pty, &mut parser, "检查两个文件");
    parser.process(&pty.collect_for(Duration::from_secs(5)));
    let text = all_terminal_rows(&mut parser);
    assert_once_in_order(
        &text,
        &[
            "› 检查两个文件",
            "先读第一个文件。",
            "• ▸ read first.txt",
            "第一个已读，继续读第二个。",
            "• ▸ read second.txt",
            "读取完成，两个文件都已检查。",
        ]
        .map(str::to_owned),
    );
    assert!(
        !text.contains("RAW_HTML_SHOULD_STAY_IN_DETAILS"),
        "raw file preview leaked:\n{text}"
    );
    assert!(
        !text.contains("\"path\":"),
        "raw argument JSON leaked:\n{text}"
    );
    assert!(
        !text.contains("Task Completed"),
        "redundant system completion card:\n{text}"
    );
    assert!(
        !parser.screen().alternate_screen(),
        "chat must remain inline"
    );
    pty.write("\x1b[200~保留草稿\x1b[201~".as_bytes());
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    let inline_screen = visible_screen_rows(&parser);
    let inline_cursor = parser.screen().cursor_position();
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None,
        "inline must allow native scroll and selection"
    );
    pty.write(b"\x0f");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    assert!(
        parser.screen().alternate_screen(),
        "{}",
        parser.screen().contents()
    );
    assert_ne!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    let expanded = parser.screen().contents();
    assert!(expanded.contains("Back"), "{expanded}");
    assert!(
        expanded.contains("RAW_HTML_SHOULD_STAY_IN_DETAILS"),
        "{expanded}"
    );
    assert!(
        !expanded.contains("first.txt"),
        "only the selected tool opens: {expanded}"
    );

    // Details consume paste/typing without changing the hidden composer.
    pty.write("\x1b[200~不可混入草稿\x1b[201~x".as_bytes());
    parser.process(&pty.collect_for(Duration::from_millis(200)));
    pty.write(b"\x1b");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    assert!(!parser.screen().alternate_screen());
    assert_eq!(
        visible_screen_rows(&parser),
        inline_screen,
        "Back restores exact inline screen and draft"
    );
    assert_eq!(parser.screen().cursor_position(), inline_cursor);

    // Keyboard access and resizing must use the same reversible screen lifecycle.
    pty.write(b"\x0f");
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    assert!(parser.screen().alternate_screen());
    pty.resize(80, 30);
    parser.screen_mut().set_size(30, 80);
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    assert!(
        parser
            .screen()
            .rows(0, 80)
            .last()
            .unwrap()
            .contains("Tool details")
    );
    pty.write(b"\x1b[<0;3;1M\x1b[<0;3;1m");
    parser.process(&pty.collect_for(Duration::from_millis(600)));
    assert!(!parser.screen().alternate_screen());
    assert!(parser.screen().contents().contains("保留草稿"));
    assert!(!all_terminal_rows(&mut parser).contains("RAW_HTML_SHOULD_STAY_IN_DETAILS"));
    pty.write(b"\x15");
    parser.process(&pty.collect_for(Duration::from_millis(200)));
    assert_once_in_order(
        &all_terminal_rows(&mut parser),
        &[
            "› 检查两个文件".to_owned(),
            "• ▸ read first.txt".to_owned(),
            "• ▸ read second.txt".to_owned(),
            "读取完成，两个文件都已检查。".to_owned(),
        ],
    );
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    assert_eq!(
        all_terminal_rows(&mut parser)
            .matches("SHELL_HISTORY_MARKER")
            .count(),
        1
    );
    assert_eq!(
        all_terminal_rows(&mut parser)
            .matches("START_COMMAND_MARKER")
            .count(),
        1
    );
    pty.write(b"\x0f");
    parser.process(&pty.collect_for(Duration::from_millis(200)));
    pty.write(b"\x1b[D");
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    assert!(parser.screen().contents().contains("first.txt"));
    assert!(!parser.screen().contents().contains("second.txt"));
    pty.write(b"\x1b");
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}

#[test]
fn output_arriving_in_tool_details_is_archived_once_after_return() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    std::fs::write(workspace.path().join("live.txt"), "DETAIL_ONLY_MARKER").unwrap();
    let markers = (1..=300)
        .map(|n| format!("后台回复段落{n:03}保持完整。"))
        .collect::<Vec<_>>();
    let server = FixtureServer::with_rounds(vec![
        (
            "检查文件。".into(),
            vec![json!({"index":0,"id":"live-read","type":"function",
            "function":{"name":"read_file","arguments":json!({"path":"live.txt","offset":1,"limit":20}).to_string()}})],
        ),
        (
            format!("{}\n\nWHILE_DETAILS_DONE", markers.join("\n\n")),
            vec![],
        ),
    ]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 100, 24, true);
    let mut parser = ScreenModel::new(24, 100);
    parser.process(&pty.collect_until(b"Ask Golutra", Duration::from_secs(8)));
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    submit(&mut pty, &mut parser, "边读详情边接收回复");
    for _ in 0..60 {
        parser.process(&pty.collect_for(Duration::from_millis(50)));
        if all_terminal_rows(&mut parser).contains("▸ read live.txt") {
            break;
        }
    }
    assert!(!all_terminal_rows(&mut parser).contains("WHILE_DETAILS_DONE"));
    pty.write(b"\x0f");
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    assert!(parser.screen().alternate_screen());
    assert!(parser.screen().contents().contains("DETAIL_ONLY_MARKER"));
    parser.process(&pty.collect_for(Duration::from_secs(5)));
    assert!(
        parser.screen().alternate_screen(),
        "runtime completion must not close details"
    );
    assert!(!parser.screen().contents().contains("WHILE_DETAILS_DONE"));
    pty.write(b"\x1b");
    parser.process(&pty.collect_for(Duration::from_millis(700)));
    assert!(!parser.screen().alternate_screen());
    let text = all_terminal_rows(&mut parser);
    assert_once_in_order(&text, &markers);
    assert_eq!(text.matches("WHILE_DETAILS_DONE").count(), 1);
    assert!(!text.contains("DETAIL_ONLY_MARKER"));
    assert_idle_composer_at_bottom(&parser);
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}

#[test]
fn native_mouse_and_shell_prefix_survive_draft_growth_and_fullscreen_picker() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let server = FixtureServer::new(vec![]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 100, 20, true);
    let mut parser = ScreenModel::new(20, 100);
    parser.process(&pty.collect_until(b"Ask Golutra", Duration::from_secs(8)));
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    assert!(!parser.screen().alternate_screen());
    pty.write(format!("\x1b[200~{}\x1b[201~", "多行草稿\n".repeat(12)).as_bytes());
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    pty.write(b"\x15");
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    assert_once_in_order(&all_terminal_rows(&mut parser), &[]);
    pty.resize(100, 36);
    parser.screen_mut().set_size(36, 100);
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    assert_once_in_order(&all_terminal_rows(&mut parser), &[]);
    submit(&mut pty, &mut parser, "/help");
    parser.process(&pty.collect_for(Duration::from_millis(400)));
    assert!(parser.screen().alternate_screen());
    assert_ne!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    pty.write(b"\x1b");
    parser.process(&pty.collect_for(Duration::from_millis(400)));
    assert!(!parser.screen().alternate_screen());
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    assert_once_in_order(&all_terminal_rows(&mut parser), &[]);
    submit(&mut pty, &mut parser, "/terminal true");
    parser.process(&pty.collect_for(Duration::from_millis(700)));
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    assert_once_in_order(&all_terminal_rows(&mut parser), &[]);
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}

#[test]
fn xterm_erase_saved_lines_preserves_the_visible_screen() {
    let mut parser = ScreenModel::new(3, 20);
    parser.process(b"old1\r\nold2\r\nold3\r\nvisible\r\ntail");
    let screen = parser.screen().contents();
    assert!(all_terminal_rows(&mut parser).contains("old1"));
    parser.process(b"\x1b[3J");
    assert_eq!(parser.screen().contents(), screen);
    assert!(!all_terminal_rows(&mut parser).contains("old1"));
}
