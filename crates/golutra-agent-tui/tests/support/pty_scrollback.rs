//! 用真实 PTY 与 VT 屏幕/滚动历史验证归档，不以原始输出中出现过文字代替可见性验收。

use super::*;
use golutra_agent_auth::{CredentialRef, SecretKind};
use golutra_agent_config::{ProviderConfigPaths, ProviderProfile, ProviderSettings};
use golutra_agent_llm::ProviderProtocol;
use serde_json::json;
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
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
    protocol: ProviderProtocol,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    requests: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    completed_streams: Arc<AtomicUsize>,
}

impl FixtureServer {
    fn with_held_first_stream(responses: Vec<String>) -> (Self, Arc<AtomicBool>) {
        let release = Arc::new(AtomicBool::new(false));
        let server = Self::with_stream_gate(
            responses
                .into_iter()
                .map(|text| (text, Vec::new()))
                .collect(),
            None,
            ProviderProtocol::OpenAiCompatible,
            Some(release.clone()),
        );
        (server, release)
    }

    fn new(responses: Vec<String>) -> Self {
        Self::with_rounds(
            responses
                .into_iter()
                .map(|text| (text, Vec::new()))
                .collect(),
        )
    }

    fn with_rounds(responses: Vec<(String, Vec<serde_json::Value>)>) -> Self {
        Self::with_stream_error(responses, None)
    }

    fn with_stream_error(
        responses: Vec<(String, Vec<serde_json::Value>)>,
        error: Option<serde_json::Value>,
    ) -> Self {
        Self::with_protocol_stream_error(responses, error, ProviderProtocol::OpenAiCompatible)
    }

    fn with_protocol_stream_error(
        responses: Vec<(String, Vec<serde_json::Value>)>,
        error: Option<serde_json::Value>,
        protocol: ProviderProtocol,
    ) -> Self {
        Self::with_stream_gate(responses, error, protocol, None)
    }

    fn with_stream_gate(
        responses: Vec<(String, Vec<serde_json::Value>)>,
        error: Option<serde_json::Value>,
        protocol: ProviderProtocol,
        first_stream_release: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self::with_stream_interval(
            responses,
            error,
            protocol,
            first_stream_release,
            Duration::from_millis(8),
        )
    }

    fn with_stream_interval(
        responses: Vec<(String, Vec<serde_json::Value>)>,
        error: Option<serde_json::Value>,
        protocol: ProviderProtocol,
        first_stream_release: Option<Arc<AtomicBool>>,
        interval: Duration,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = requests.clone();
        let completed_streams = Arc::new(AtomicUsize::new(0));
        let completed = completed_streams.clone();
        let worker = thread::spawn(move || {
            let mut responses = responses.into_iter();
            while !flag.load(Ordering::Relaxed) {
                let Ok((mut socket, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                socket.set_nodelay(true).unwrap();
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
                // TCP 探测/放弃连接并非 HTTP 请求，不能消耗下一轮夹具响应。
                if request.is_empty() {
                    continue;
                }
                let end = request
                    .windows(4)
                    .position(|bytes| bytes == b"\r\n\r\n")
                    .expect("fixture request must contain complete headers");
                let body = serde_json::from_slice(&request[end + 4..]).unwrap_or_else(|error| {
                    panic!(
                        "fixture must capture a complete JSON request: {error}; body_bytes={}",
                        request.len() - end - 4
                    )
                });
                captured.lock().unwrap().push(body);
                let first_stream = captured.lock().unwrap().len() == 1;
                let (content, calls) = responses
                    .next()
                    .unwrap_or_else(|| ("FIXTURE_DONE".to_owned(), Vec::new()));
                let mut frames = Vec::new();
                for chunk in content.chars().collect::<Vec<_>>().chunks(17) {
                    if protocol == ProviderProtocol::OpenAiResponses {
                        frames.push(format!("data: {}\n\n", json!({
                            "type":"response.output_text.delta", "delta":chunk.iter().collect::<String>()
                        })));
                        continue;
                    }
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
                if let Some(error) = &error {
                    if protocol == ProviderProtocol::OpenAiResponses {
                        frames.push(format!(
                            "data: {}\n\n",
                            json!({
                                "type":"response.failed", "response":{
                                    "id":"resp-pty-failure", "status":"failed", "model":"pty-model",
                                    "error":error["error"], "output":[]
                                }
                            })
                        ));
                    } else {
                        frames.push(format!("data: {error}\n\n"));
                    }
                } else {
                    frames.push(format!("data: {}\n\ndata: [DONE]\n\n", json!({
                    "id":"pty", "object":"chat.completion.chunk", "created":0,"model":"pty-model",
                    "choices":[{"index":0,"delta":{},"finish_reason":if calls.is_empty() {"stop"} else {"tool_calls"}}],
                    "usage":{"prompt_tokens":100,"completion_tokens":100,"total_tokens":200}
                })));
                }
                let length: usize = frames.iter().map(String::len).sum();
                if write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n").is_err() { continue; }
                let mut sent_all = true;
                let stream_started = Instant::now();
                let terminal_frame = frames.len() - 1;
                for (index, frame) in frames.into_iter().enumerate() {
                    // 补充输入测试由真实可见状态放行终帧，避免依赖固定流时长；
                    // 析构停止仍能中断等待，测试断言失败时不会卡住清理。
                    if first_stream && index == terminal_frame {
                        while first_stream_release
                            .as_ref()
                            .is_some_and(|release| !release.load(Ordering::Acquire))
                            && !flag.load(Ordering::Relaxed)
                        {
                            thread::sleep(Duration::from_millis(10));
                        }
                    }
                    if flag.load(Ordering::Relaxed) || socket.write_all(frame.as_bytes()).is_err() {
                        sent_all = false;
                        break;
                    }
                    let _ = socket.flush();
                    // 以流起点计时，避免 macOS 每帧调度超时累计成几十秒的夹具延迟。
                    let due = stream_started + interval * (index as u32 + 1);
                    if let Some(remaining) = due.checked_duration_since(Instant::now()) {
                        thread::sleep(remaining);
                    }
                }
                if sent_all {
                    completed.fetch_add(1, Ordering::Release);
                }
            }
        });
        Self {
            url,
            protocol,
            stopped,
            worker: Some(worker),
            requests,
            completed_streams,
        }
    }

    fn install(&self, home: &Path) {
        let credential =
            CredentialRef::environment("GOLUTRA_AGENT_PTY_TEST_KEY", SecretKind::ApiKey).unwrap();
        let profile = ProviderProfile::live_profile(
            "pty-local",
            self.protocol,
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
        let result = self.worker.take().unwrap().join();
        // 测试已经失败时保留首个诊断，避免析构再次 panic 中止整个测试进程。
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

#[test]
fn fixture_empty_connection_does_not_consume_a_response() {
    use std::io::BufRead;
    let server = FixtureServer::new(vec!["FIRST_RESPONSE".to_owned()]);
    let address = server
        .url
        .strip_prefix("http://")
        .unwrap()
        .strip_suffix("/v1")
        .unwrap();
    drop(TcpStream::connect(address).unwrap());
    let mut socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
        .write_all(
            b"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}",
        )
        .unwrap();
    // 按 HTTP 长度读取正文；正文完整后关闭连接的 EOF/RST 差异不属于夹具语义。
    let mut reader = std::io::BufReader::new(socket);
    let mut length = None;
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length: ") {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0; length.expect("fixture response content length")];
    reader.read_exact(&mut body).unwrap();
    let response = String::from_utf8(body).unwrap();
    assert!(response.contains("FIRST_RESPONSE"), "{response}");
    assert_eq!(*server.requests.lock().unwrap(), vec![json!({})]);
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

#[test]
fn partial_stream_failure_is_visible_once_with_diagnostics_in_real_pty() {
    let server = FixtureServer::with_stream_error(
        vec![("Hi! 你好。".to_owned(), Vec::new())],
        Some(json!({"error":{"status":400,"code":"invalid_request",
            "message":"请求参数错误", "request_id":"pty-error-400"}})),
    );
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 110, 32, true);
    let mut parser = ScreenModel::new(32, 110);
    wait_for_visible(&mut pty, &mut parser, "pty-model");
    submit(&mut pty, &mut parser, "hi");
    wait_for_visible(&mut pty, &mut parser, "Request ID: pty-error-400");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    let text = all_terminal_rows(&mut parser);
    assert_eq!(text.matches("Hi! 你好。").count(), 1, "{text}");
    assert_eq!(text.matches("Task failed").count(), 1, "{text}");
    assert_eq!(text.matches("请求参数错误").count(), 1, "{text}");
    assert!(text.contains("HTTP response: 200"), "{text}");
    assert!(text.contains("Error status: 400"), "{text}");
    assert!(!text.contains("Loop Decided"), "{text}");
    assert!(!text.contains("Task Completed"), "{text}");
    assert!(
        !text.contains("verification evidence is insufficient"),
        "{text}"
    );
    assert_eq!(
        server.requests.lock().unwrap().len(),
        1,
        "no replay after partial output"
    );
}

#[test]
fn responses_failure_displays_full_cause_after_compact_runtime_summary() {
    let cause = "Invalid prompt: your prompt was flagged as potentially violating our usage policy. Please try again with a different prompt. 完整错误末尾。";
    let server = FixtureServer::with_protocol_stream_error(
        vec![("Hi! 你好。".to_owned(), Vec::new())],
        Some(json!({"error":{"code":"invalid_prompt","message":cause}})),
        ProviderProtocol::OpenAiResponses,
    );
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 70, 20, true);
    let mut parser = ScreenModel::new(20, 70);
    wait_for_visible(&mut pty, &mut parser, "pty-model");
    submit(&mut pty, &mut parser, "hi");
    let expected = cause.split_whitespace().collect::<String>();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !parser
        .screen()
        .contents()
        .split_whitespace()
        .collect::<String>()
        .contains(&expected)
        && Instant::now() < deadline
    {
        parser.process(&pty.collect_for(Duration::from_millis(50)));
    }
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    let text = all_terminal_rows(&mut parser);
    let compact = text.split_whitespace().collect::<String>();
    assert!(compact.contains(&expected), "{text}");
    assert_eq!(text.matches("Task failed").count(), 1, "{text}");
    assert!(text.contains("Hi! 你好。"), "{text}");
    assert!(!text.contains("Failed to parse stream"), "{text}");
    assert!(!text.contains("runtime task execution failed"), "{text}");
    assert!(!text.contains("Task Completed"), "{text}");
    assert_eq!(
        server.requests.lock().unwrap().len(),
        1,
        "no retry after partial output"
    );
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
    // 输入尚未提交时 composer 也以 › 开头；只检查分隔线之前的消息历史。
    let history_end = rows
        .iter()
        .rposition(|row| row.starts_with('─'))
        .unwrap_or(rows.len());
    for (index, row) in rows.iter().enumerate().take(history_end).skip(2) {
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
    wait_for_visible_markers(pty, parser, &[marker]);
}

fn wait_for_idle_reply(pty: &mut PtyHarness, parser: &mut ScreenModel, marker: &str) {
    // 最后一个文本 delta 不代表任务已完成；等待 composer 和运行状态一起恢复。
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        parser.process(&pty.collect_for(Duration::from_millis(50)));
        let screen = parser.screen().contents();
        if screen.contains(marker)
            && screen.contains("Ask Golutra")
            && !screen.contains("esc to interrupt")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "task did not become idle:\n{screen}"
        );
    }
}

fn wait_for_visible_markers(pty: &mut PtyHarness, parser: &mut ScreenModel, markers: &[&str]) {
    // 长流夹具逐帧 sleep；macOS 的定时器合并和 CI 调度会延长实际发送时间。
    // 等待可见状态而非假定发送速率，仍保留有界超时。
    let deadline = Instant::now() + Duration::from_secs(30);
    while !markers
        .iter()
        .all(|marker| parser.screen().contents().contains(marker))
        && Instant::now() < deadline
    {
        parser.process(&pty.collect_for(Duration::from_millis(50)));
    }
    assert!(
        markers
            .iter()
            .all(|marker| parser.screen().contents().contains(marker)),
        "missing {markers:?}:\n{}",
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
        // 同一次重绘也可拆成多个 PTY read；标题出现不代表预览行和 composer 已收到。
        wait_for_visible_markers(&mut pty, &mut parser, &[preview, "↳ hi", "› Ask Golutra"]);
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
    let (server, release_first_stream) = FixtureServer::with_held_first_stream(vec![
        "BATCH_START".into(),
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
    assert_eq!(server.completed_streams.load(Ordering::Acquire), 0);
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    release_first_stream.store(true, Ordering::Release);
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
    // 回复和输入框可能分属两次 PTY read，须等草稿实际绘制，不能只等回复标记。
    wait_for_visible(&mut pty, &mut parser, "› draft keep");
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
    wait_for_visible(&mut pty, &mut parser, "POPUP_STREAM_DONE");
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
    wait_for_visible(&mut pty, &mut parser, "STREAM_DONE");
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
fn shell_soft_output_budget_reaches_provider_without_rejection_or_paging() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let content = format!(
        "PREVIEW_BEGIN\n{}PREVIEW_END\n",
        "中文🙂 information\n".repeat(400)
    );
    assert!(content.len() > 4096 && content.len() < 12000);
    std::fs::write(workspace.path().join("output.txt"), &content).unwrap();
    let calls = [12000, 20000].into_iter().enumerate().map(|(index, size)| json!({
        "index":index,"id":format!("soft-budget-{index}"),"type":"function","function":{
            "name":"shell","arguments":json!({"argv":["cat","output.txt"],"max_output_bytes":size}).to_string()
        }
    })).collect();
    let server = FixtureServer::with_rounds(vec![
        ("读取测试输出。".into(), calls),
        ("SOFT_BUDGET_DONE".into(), vec![]),
    ]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 100, 28, true);
    let mut parser = ScreenModel::new(28, 100);
    wait_for_visible(&mut pty, &mut parser, "pty-model");
    submit(&mut pty, &mut parser, "检查输出");
    // This fixture runs in guarded mode: explicitly approve the local read
    // commands instead of weakening the production execution policy.
    wait_for_visible(&mut pty, &mut parser, "Approval required");
    pty.write(b"3");
    wait_for_visible(&mut pty, &mut parser, "SOFT_BUDGET_DONE");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    let text = all_terminal_rows(&mut parser);
    assert!(!text.contains("Failed"), "{text}");
    assert!(!text.contains("Waited for background terminal"), "{text}");
    assert!(!text.contains("Background terminal completed"), "{text}");
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let outputs = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| message["content"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(outputs.len(), 2);
    for output in outputs {
        assert!(
            output.contains(&content),
            "provider lost preview text: {output}"
        );
        assert!(output.contains("\"output_has_more\":false"), "{output}");
        assert!(output.contains("\"process_state\":\"exited\""), "{output}");
    }
    drop(requests);
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}

#[test]
fn invalid_shell_arguments_are_visible_and_returned_to_the_provider_before_correction() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    std::fs::write(workspace.path().join("PAGE_SIZE_EXECUTED"), "fixture").unwrap();
    let shell = |id: &str, size: u64| {
        json!({
            "index":0, "id":id, "type":"function", "function": {
                "name":"shell", "arguments":json!({
                "argv":["ls"], "max_output_bytes":size
                }).to_string()
            }
        })
    };
    let server = FixtureServer::with_rounds(vec![
        ("检查工作区。".into(), vec![shell("invalid-size", 255)]),
        (
            "按错误提示修正分页大小。".into(),
            vec![shell("correct-size", 4096)],
        ),
        ("PAGE_SIZE_RECOVERY_DONE".into(), vec![]),
    ]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 100, 28, true);
    let mut parser = ScreenModel::new(28, 100);
    wait_for_visible(&mut pty, &mut parser, "pty-model");
    submit(&mut pty, &mut parser, "检查工作区");
    wait_for_visible(&mut pty, &mut parser, "PAGE_SIZE_RECOVERY_DONE");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    let text = all_terminal_rows(&mut parser);
    let cause = "max_output_bytes: 255 is less than the minimum of 256";
    assert!(text.contains("Failed · shell"), "{text}");
    assert!(text.contains(cause), "{text}");
    assert!(!text.contains("tool request is invalid"), "{text}");
    assert!(!text.contains("Task failed"), "{text}");
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for name in ["shell", "shell_session"] {
        let tool = requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["function"]["name"] == name)
            .unwrap();
        let description =
            tool["function"]["parameters"]["properties"]["max_output_bytes"]["description"]
                .as_str()
                .unwrap();
        assert!(description.contains("minimum 256"), "{description}");
        assert!(description.contains("Default 12288"), "{description}");
        assert!(
            description.contains("capped by runtime/context policy"),
            "{description}"
        );
    }
    let tool_results = |index: usize| {
        requests[index]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| message["content"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let rejected = tool_results(1);
    assert_eq!(rejected.len(), 1);
    assert!(rejected[0].contains(cause), "{:?}", rejected);
    assert!(!rejected[0].contains("PAGE_SIZE_EXECUTED"));
    let corrected = tool_results(2);
    assert_eq!(corrected.len(), 2);
    assert!(
        corrected[1].contains("PAGE_SIZE_EXECUTED"),
        "{:?}",
        corrected
    );
    drop(requests);
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
        "tool cards must preserve native terminal selection"
    );
    pty.write(b"\x0f");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    assert!(
        parser.screen().alternate_screen(),
        "{}",
        parser.screen().contents()
    );
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    let expanded = parser.screen().contents();
    assert!(expanded.contains("Ctrl+O / Esc back"), "{expanded}");
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
    pty.write(b"\x0f");
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
    // 回复在详情页不可见，先确认服务端已发送两轮完整流，再返回验证归档。
    let deadline = Instant::now() + Duration::from_secs(30);
    while server.completed_streams.load(Ordering::Acquire) < 2 && Instant::now() < deadline {
        parser.process(&pty.collect_for(Duration::from_millis(50)));
    }
    assert_eq!(server.completed_streams.load(Ordering::Acquire), 2);
    assert!(
        parser.screen().alternate_screen(),
        "runtime completion must not close details"
    );
    assert!(!parser.screen().contents().contains("WHILE_DETAILS_DONE"));
    pty.write(b"\x1b");
    wait_for_visible(&mut pty, &mut parser, "WHILE_DETAILS_DONE");
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
fn model_editor_uses_native_mouse_and_enter_applies_to_next_provider_request() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let server = FixtureServer::new(vec![
        "MODEL_SAVE_CONFIRMED".to_owned(),
        "MODEL_ESC_SAVE_CONFIRMED".to_owned(),
        "MODEL_RESTART_CONFIRMED".to_owned(),
    ]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 110, 32, true);
    let mut parser = ScreenModel::new(32, 110);
    wait_for_visible(&mut pty, &mut parser, "pty-model");
    submit(&mut pty, &mut parser, "/model");
    wait_for_visible(&mut pty, &mut parser, "Ctrl+U clear");
    assert!(parser.screen().alternate_screen());
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    // Help temporarily owns mouse navigation; returning restores native selection.
    pty.write(b"\x1bOP");
    parser.process(&pty.collect_for(Duration::from_millis(400)));
    assert_ne!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    pty.write(b"\x1b");
    wait_for_visible(&mut pty, &mut parser, "Ctrl+U clear");
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    pty.write(b"\x15\x1b[200~gpt-5.6-sol\x1b[201~\r");
    wait_for_visible(&mut pty, &mut parser, "Ask Golutra");
    assert!(!parser.screen().alternate_screen());
    assert!(parser.screen().contents().contains("gpt-5.6-sol"));
    submit(&mut pty, &mut parser, "hi");
    wait_for_idle_reply(&mut pty, &mut parser, "MODEL_SAVE_CONFIRMED");
    assert_eq!(server.requests.lock().unwrap()[0]["model"], "gpt-5.6-sol");
    submit(&mut pty, &mut parser, "/model");
    wait_for_visible(&mut pty, &mut parser, "Ctrl+U clear");
    pty.write(b"\x1b");
    wait_for_visible(&mut pty, &mut parser, "Enter edit/change");
    pty.write(b"\x1b[B\x1b[A\r");
    wait_for_visible(&mut pty, &mut parser, "Ctrl+U clear");
    pty.write(b"\x15\x1b[200~gpt-6-astra\x1b[201~");
    pty.write(b"\x1b");
    wait_for_visible(&mut pty, &mut parser, "Enter edit/change");
    assert!(parser.screen().alternate_screen());
    pty.write(b"\x1b");
    wait_for_visible(&mut pty, &mut parser, "Ask Golutra");
    assert!(!parser.screen().alternate_screen());
    assert!(parser.screen().contents().contains("gpt-6-astra"));
    submit(&mut pty, &mut parser, "hi again");
    wait_for_idle_reply(&mut pty, &mut parser, "MODEL_ESC_SAVE_CONFIRMED");
    assert_eq!(server.requests.lock().unwrap()[1]["model"], "gpt-6-astra");
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());

    // A fresh process must load the saved selection before its first request,
    // including when the editor is reopened and accepted without further edits.
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 110, 32, true);
    let mut parser = ScreenModel::new(32, 110);
    wait_for_visible(&mut pty, &mut parser, "gpt-6-astra");
    submit(&mut pty, &mut parser, "/model");
    wait_for_visible(&mut pty, &mut parser, "Ctrl+U clear");
    assert!(parser.screen().contents().contains("gpt-6-astra"));
    pty.write(b"\r");
    wait_for_visible(&mut pty, &mut parser, "Ask Golutra");
    submit(&mut pty, &mut parser, "hi after restart");
    wait_for_idle_reply(&mut pty, &mut parser, "MODEL_RESTART_CONFIRMED");
    assert_eq!(server.requests.lock().unwrap()[2]["model"], "gpt-6-astra");
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}

#[test]
fn auth_credential_shortcut_saves_disk_or_env_without_storage_page() {
    use golutra_agent_auth::CredentialSource;

    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    install_mock_provider(home.path());
    let mut pty = PtyHarness::spawn(home.path(), workspace.path(), 110, 32);
    let mut parser = ScreenModel::new(32, 110);
    let paths = ProviderConfigPaths::from_home(home.path()).unwrap();
    let secret = "isolated-auth-shortcut-secret";

    for environment in [false, true] {
        wait_for_visible(&mut pty, &mut parser, "Ask Golutra");
        submit(&mut pty, &mut parser, "/auth");
        wait_for_visible(&mut pty, &mut parser, "Connect a Provider");
        pty.write(b"3");
        wait_for_visible(&mut pty, &mut parser, "Step 1/6");
        pty.write(b"1");
        wait_for_visible(&mut pty, &mut parser, "Step 2/6");
        submit(&mut pty, &mut parser, "http://127.0.0.1:9");
        wait_for_visible_markers(&mut pty, &mut parser, &["Step 3/6", "API Key", "Ctrl+E"]);
        assert!(!parser.screen().contents().contains("Credential storage"));
        pty.write(secret.as_bytes());
        parser.process(&pty.collect_for(Duration::from_millis(150)));
        assert!(!parser.screen().contents().contains(secret));
        // 验证真实 Ctrl+E 字节经过主输入分发后到达认证页，而不是插入字符 e。
        pty.write(b"\x05");
        wait_for_visible(&mut pty, &mut parser, "Environment variable name");
        assert!(!parser.screen().contents().contains(secret));
        assert!(
            !parser
                .screen()
                .contents()
                .contains("GOLUTRA_AGENT_CUSTOM_PROVIDER_API_KEY")
        );
        if environment {
            submit(&mut pty, &mut parser, "GOLUTRA_AGENT_PTY_TEST_KEY");
        } else {
            pty.write(b"\x05");
            wait_for_visible(&mut pty, &mut parser, "API Key");
            assert!(!parser.screen().contents().contains("*****"));
            submit(&mut pty, &mut parser, secret);
        }
        wait_for_visible(&mut pty, &mut parser, "Step 4/6");
        submit(&mut pty, &mut parser, "gpt-golden");
        wait_for_visible(&mut pty, &mut parser, "Step 5/6");
        wait_for_visible(&mut pty, &mut parser, "> 1 Continue");
        if !environment {
            // 真正的终端字节覆盖双向调整、Enter 编辑和 Esc 保留草稿；不修改时直接继续。
            pty.write(b"\x1b[B\r");
            wait_for_visible(&mut pty, &mut parser, "enabled");
            pty.write(b"\x1b[B\r\x1b[C\x1b[C\x1b[D\x1b[C");
            wait_for_visible(&mut pty, &mut parser, "high");
            pty.write(b"\x1b[B\r");
            wait_for_visible(&mut pty, &mut parser, "Enter/Esc keep edit");
            pty.write(b"128000\x1b");
            wait_for_visible(&mut pty, &mut parser, "Enter change/continue");
            assert!(parser.screen().contents().contains("128000"));
            pty.write(b"\x1b[B\r");
            wait_for_visible(&mut pty, &mut parser, "Enter/Esc keep edit");
            submit(&mut pty, &mut parser, "8192");
            wait_for_visible(&mut pty, &mut parser, "Enter change/continue");
            pty.write(b"\x1b[B\r");
            wait_for_visible(&mut pty, &mut parser, "Enter/Esc keep edit");
            submit(&mut pty, &mut parser, "X-Client=jk-test");
            wait_for_visible(&mut pty, &mut parser, "Enter change/continue");
            for _ in 0..5 {
                pty.write(b"\x1b[A");
            }
            wait_for_visible(&mut pty, &mut parser, "> 1 Continue");
        }
        pty.write(b"\r");
        wait_for_visible(&mut pty, &mut parser, "Review provider setup");
        assert!(!parser.screen().contents().contains(secret));
        pty.write(b"\r");
        wait_for_visible(&mut pty, &mut parser, "Ask Golutra");
        assert!(!parser.screen().alternate_screen());
        assert_eq!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        );
        let settings = ProviderSettings::load(&paths.user_config).unwrap();
        let profile = settings
            .profiles
            .iter()
            .find(|profile| profile.name == "custom")
            .unwrap();
        let source = &profile.credential_ref.as_ref().unwrap().source;
        if environment {
            assert!(profile.generation_config.is_none());
            assert!(profile.custom_headers.is_empty());
        } else {
            let generation = profile.generation_config.as_ref().unwrap();
            assert!(generation.enable_thinking);
            assert_eq!(
                generation.reasoning_effort,
                Some(golutra_agent_llm::ProviderReasoningEffort::High)
            );
            assert_eq!(generation.context_window_size, Some(128000));
            assert_eq!(generation.max_tokens, Some(8192));
            assert_eq!(profile.custom_headers.len(), 1);
            assert_eq!(profile.custom_headers[0].name, "X-Client");
        }
        if environment {
            assert!(
                matches!(source, CredentialSource::Environment { key } if key == "GOLUTRA_AGENT_PTY_TEST_KEY")
            );
        } else {
            assert!(matches!(source, CredentialSource::Disk));
        }
        assert!(
            !std::fs::read_to_string(&paths.user_config)
                .unwrap()
                .contains(secret)
        );
        let credentials_path = home.path().join("credentials.json");
        if environment {
            // 最后一个本地凭据被替换后，存储层会移除空文件。
            assert!(!credentials_path.exists());
        } else {
            assert!(
                std::fs::read_to_string(credentials_path)
                    .unwrap()
                    .contains(secret)
            );
        }
    }
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}

#[test]
fn debug_before_first_token_keeps_one_reply_in_live_view_and_scrollback() {
    for expanded in [true, false] {
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let answer = "Hi! I'm Golutra, ready to help with the project in golutra-agent. What would you like to work on — a bug fix, a new feature, refactoring, tests, or a question about the codebase?\n\n也可以用中文描述需要解决的问题。";
        let release = Arc::new(AtomicBool::new(false));
        let server = FixtureServer::with_stream_interval(
            vec![(answer.into(), vec![])],
            None,
            ProviderProtocol::OpenAiCompatible,
            Some(release.clone()),
            Duration::from_millis(80),
        );
        server.install(home.path());
        let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 120, 32, true);
        let mut parser = ScreenModel::new(32, 120);
        wait_for_visible(&mut pty, &mut parser, "pty-model");
        if !expanded {
            submit(&mut pty, &mut parser, "/debug switch");
        }
        submit(&mut pty, &mut parser, "/debug");
        wait_for_visible(&mut pty, &mut parser, "Alt+D events");
        submit(&mut pty, &mut parser, "hi");
        wait_for_visible(&mut pty, &mut parser, "Hi! I'm");
        wait_for_visible(&mut pty, &mut parser, "需要解决的问题。");
        assert_eq!(
            server.completed_streams.load(Ordering::Acquire),
            0,
            "text must be visible before completion"
        );
        let assert_reply = |parser: &mut ScreenModel| {
            let text = all_terminal_rows(parser);
            assert!(!text.contains("Updated response"), "{text}");
            let left = text
                .lines()
                .map(|line| {
                    let mut width = 0;
                    line.chars()
                        .take_while(|c| {
                            width += c.width().unwrap_or(0);
                            width <= 60
                        })
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(left.matches("Hi! I'm").count(), 1, "{text}");
            let compact = left
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>();
            let expected = answer
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>();
            assert!(
                compact.contains(&expected),
                "lost or duplicated reply:\n{text}"
            );
        };
        assert_reply(&mut parser);
        release.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            parser.process(&pty.collect_for(Duration::from_millis(50)));
            let screen = parser.screen().contents();
            if server.completed_streams.load(Ordering::Acquire) > 0
                && screen.contains("Ask Golutra")
                && !screen.contains("esc to interrupt")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "stream did not complete:\n{screen}"
            );
        }
        assert_reply(&mut parser);
        let text = all_terminal_rows(&mut parser);
        assert!(text.contains("ProviderStreamed/Provider"), "{text}");
        assert!(text.contains("AssistantMessage/Runtime"), "{text}");
        let rows = text.lines().collect::<Vec<_>>();
        let accepted = rows
            .iter()
            .position(|row| row.contains("CommandCompleted/Runtime"))
            .unwrap();
        let started = rows
            .iter()
            .position(|row| row.contains("StepStarted/Runtime"))
            .unwrap();
        assert_eq!(
            started,
            accepted + 1,
            "diagnostic-only events must not add blank chat rows:\n{text}"
        );
        assert_eq!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        );
        submit(&mut pty, &mut parser, "/quit");
        assert!(pty.wait().1.success());
    }
}

#[test]
fn debug_reload_and_keyboard_event_details_preserve_live_scrollback_and_draft() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let markers = (0..40)
        .map(|index| format!("DEBUG_LINE_{index:03}"))
        .collect::<Vec<_>>();
    let (server, release) = FixtureServer::with_held_first_stream(vec![markers.join("\n\n")]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 120, 32, true);
    let mut parser = ScreenModel::new(32, 120);
    wait_for_visible(&mut pty, &mut parser, "pty-model");
    submit(&mut pty, &mut parser, "Produce the diagnostic fixture");
    wait_for_visible(&mut pty, &mut parser, "DEBUG_LINE_020");
    submit(&mut pty, &mut parser, "/debug");
    wait_for_visible(&mut pty, &mut parser, "Alt+D events");
    // 结束流之前连续重载；新事件仍必须衔接到持久历史，不能覆盖输入草稿。
    for command in ["/debug switch", "/debug switch"] {
        submit(&mut pty, &mut parser, command);
        parser.process(&pty.collect_for(Duration::from_millis(150)));
    }
    release.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        parser.process(&pty.collect_for(Duration::from_millis(50)));
        let screen = parser.screen().contents();
        // debug 的末尾还有观测事件；最终正文可以已进入原生历史，不要求留在活动视口。
        if all_terminal_rows(&mut parser).contains("DEBUG_LINE_039")
            && screen.contains("Ask Golutra")
            && !screen.contains("esc to interrupt")
            && !screen.contains("loading complete history")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "debug stream did not complete:\n{screen}"
        );
    }
    pty.write(b"draft-kept");
    wait_for_visible(&mut pty, &mut parser, "draft-kept");
    pty.write(b"\x1bd");
    wait_for_visible(&mut pty, &mut parser, "Recorded event (redacted)");
    assert!(parser.screen().alternate_screen());
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    pty.write(b"\x1b[D");
    parser.process(&pty.collect_for(Duration::from_millis(100)));
    pty.write(b"\x1b");
    wait_for_visible(&mut pty, &mut parser, "draft-kept");
    assert!(!parser.screen().alternate_screen());
    let assert_transcript = |parser: &mut ScreenModel| {
        // 右栏的 AssistantMessage 摘要可以引用正文；只统计正文行，不能把观测当作重复回复。
        let text = all_terminal_rows(parser);
        let actual = text
            .lines()
            .filter_map(|line| {
                let left_column = line.chars().take(60).collect::<String>();
                let line = left_column.trim();
                let line = line.strip_prefix("• ").unwrap_or(line);
                line.starts_with("DEBUG_LINE_").then(|| line.to_owned())
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, markers, "{text}");
    };
    assert_transcript(&mut parser);
    pty.write(b"\x15");
    submit(&mut pty, &mut parser, "/debug");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    assert_transcript(&mut parser);
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}

#[test]
fn auth_keeps_fullscreen_keyboard_navigation_and_native_mouse_selection() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    install_mock_provider(home.path());
    let mut pty = PtyHarness::spawn(home.path(), workspace.path(), 110, 32);
    let mut parser = ScreenModel::new(32, 110);
    wait_for_visible(&mut pty, &mut parser, "Ask Golutra");
    let provider_before = std::fs::read(home.path().join("provider.json")).unwrap();
    for command in ["/auth", "/login"] {
        submit(&mut pty, &mut parser, command);
        wait_for_visible(&mut pty, &mut parser, "Connect a Provider");
        assert!(parser.screen().alternate_screen());
        assert!(parser.screen().contents().contains("Esc close"));
        pty.write(b"\x1b");
        wait_for_visible(&mut pty, &mut parser, "Ask Golutra");
        assert!(!parser.screen().alternate_screen());
        assert_eq!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        );
        assert_eq!(
            std::fs::read(home.path().join("provider.json")).unwrap(),
            provider_before
        );
    }
    submit(&mut pty, &mut parser, "/login");
    wait_for_visible(&mut pty, &mut parser, "Connect a Provider");
    assert!(parser.screen().alternate_screen());
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    pty.resize(100, 30);
    parser.screen_mut().set_size(30, 100);
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    // 两个全屏页面之间切换不离开 alternate screen，仍需分别恢复鼠标策略。
    pty.write(b"\x1bOP");
    parser.process(&pty.collect_for(Duration::from_millis(400)));
    assert!(parser.screen().alternate_screen());
    assert_ne!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    pty.write(b"\x1b");
    wait_for_visible(&mut pty, &mut parser, "Connect a Provider");
    assert!(parser.screen().alternate_screen());
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    // 用键盘选择 mock 完成认证，不访问网络；回到主屏后鼠标仍由终端处理。
    pty.write(b"\x1b[B\x1b[B\x1b[B\r");
    wait_for_visible(&mut pty, &mut parser, "Ask Golutra");
    assert!(!parser.screen().alternate_screen());
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    submit(&mut pty, &mut parser, "/help");
    parser.process(&pty.collect_for(Duration::from_millis(400)));
    assert!(parser.screen().alternate_screen());
    assert_ne!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    pty.write(b"\x1b");
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
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
    // 登录 shell 的初始化耗时取决于宿主环境；恢复 composer 后再检查归档，不能采样挂起帧。
    wait_for_visible(&mut pty, &mut parser, "Ask Golutra");
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    assert_once_in_order(&all_terminal_rows(&mut parser), &[]);
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}

#[test]
fn logout_removes_active_config_reopens_setup_and_stays_logged_out_after_restart() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let server = FixtureServer::new(vec!["LOGOUT_HISTORY_PRESERVED".to_owned()]);
    server.install(home.path());
    let paths = ProviderConfigPaths::from_home(home.path()).unwrap();
    let mut settings = ProviderSettings::load(&paths.user_config).unwrap();
    let mut spare = ProviderProfile::mock();
    spare.name = "spare".to_owned();
    settings.upsert_profile(spare, false);
    settings.save(&paths.user_config).unwrap();
    let mut pty = PtyHarness::spawn_with_resume(
        home.path(),
        workspace.path(),
        110,
        32,
        true,
        Some("logout-history"),
    );
    let mut parser = ScreenModel::new(32, 110);
    wait_for_visible(&mut pty, &mut parser, "Ask Golutra");
    submit(&mut pty, &mut parser, "hello");
    wait_for_idle_reply(&mut pty, &mut parser, "LOGOUT_HISTORY_PRESERVED");
    submit(&mut pty, &mut parser, "/logout");
    wait_for_visible(&mut pty, &mut parser, "Connect a Provider");
    assert!(parser.screen().alternate_screen());
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    let saved = ProviderSettings::load(&paths.user_config).unwrap();
    assert!(saved.active_profile.is_none());
    assert_eq!(saved.profiles.len(), 1);
    assert_eq!(saved.profiles[0].name, "spare");
    // 正常退出向导后重新启动，不允许回退到另一个 Provider 或恢复被删配置。
    pty.write(b"\x03");
    parser.process(&pty.collect_for(Duration::from_millis(150)));
    pty.write(b"\x03");
    assert!(pty.wait().1.success());
    let mut resumed = PtyHarness::spawn_with_resume(
        home.path(),
        workspace.path(),
        110,
        32,
        true,
        Some("logout-history"),
    );
    let mut screen = ScreenModel::new(32, 110);
    wait_for_visible(&mut resumed, &mut screen, "Connect a Provider");
    assert!(screen.screen().alternate_screen());
    // 再次配置可正常返回主屏；此前的历史仍在。
    resumed.write(b"\x1b[B\x1b[B\x1b[B\r");
    wait_for_visible(&mut resumed, &mut screen, "Ask Golutra");
    assert!(all_terminal_rows(&mut screen).contains("LOGOUT_HISTORY_PRESERVED"));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    submit(&mut resumed, &mut screen, "/quit");
    assert!(resumed.wait().1.success());
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

#[test]
fn command_cards_keep_native_mouse_with_keyboard_details_and_resume() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let output = (0..240)
        .map(|n| format!("日志{n:03} 中文🙂\n\n"))
        .collect::<String>();
    std::fs::write(workspace.path().join("log.txt"), &output).unwrap();
    let server = FixtureServer::with_rounds(vec![
        (
            "读取命令日志。".into(),
            vec![
                json!({"index":0,"id":"long-log","type":"function","function":{"name":"shell","arguments":json!({"command":"cat log.txt"}).to_string()}}),
            ],
        ),
        ("CARD_TEST_DONE".into(), vec![]),
    ]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_with_resume(
        home.path(),
        workspace.path(),
        100,
        32,
        true,
        Some("saved-card-test"),
    );
    let mut parser = ScreenModel::new(32, 100);
    wait_for_visible(&mut pty, &mut parser, "Ask Golutra");
    submit(&mut pty, &mut parser, "运行日志命令");
    wait_for_visible(&mut pty, &mut parser, "Approval required");
    pty.write(b"3");
    wait_for_idle_reply(&mut pty, &mut parser, "CARD_TEST_DONE");
    assert!(!all_terminal_rows(&mut parser).contains("日志239"));
    pty.write("\x1b[200~保留草稿\x1b[201~".as_bytes());
    parser.process(&pty.collect_for(Duration::from_millis(200)));
    let original = visible_screen_rows(&parser);
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    // 迟到的鼠标事件不再打开卡片、接管滚轮或修改界面。
    let row = screen_row(&parser, "cat log.txt") as u16 + 1;
    pty.write(
        format!("\x1b[<35;5;{row}M\x1b[<0;5;{row}M\x1b[<0;5;{row}m\x1b[<64;5;{row}M").as_bytes(),
    );
    parser.process(&pty.collect_for(Duration::from_millis(200)));
    assert!(!parser.screen().alternate_screen());
    assert_eq!(visible_screen_rows(&parser), original);
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    pty.write(b"\x0f");
    wait_for_visible(&mut pty, &mut parser, "Tool details");
    assert!(parser.screen().alternate_screen());
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    wait_for_visible(&mut pty, &mut parser, "日志000");
    pty.write("/日志239\r".as_bytes());
    wait_for_visible(&mut pty, &mut parser, "日志239");
    assert!(parser.screen().contents().contains("中文🙂"));
    assert!(!parser.screen().contents().contains("Content unavailable"));
    // 详情标题和正文均可由终端选择，点击不再执行返回动作。
    pty.write(b"\x1b[<0;5;1M\x1b[<0;5;1m");
    parser.process(&pty.collect_for(Duration::from_millis(150)));
    assert!(parser.screen().alternate_screen());
    pty.resize(72, 24);
    parser.screen_mut().set_size(24, 72);
    parser.process(&pty.collect_for(Duration::from_millis(250)));
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    pty.write(b"\x0f");
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    assert!(!parser.screen().alternate_screen());
    assert!(parser.screen().contents().contains("保留草稿"));
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    pty.write(b"\x0f");
    wait_for_visible(&mut pty, &mut parser, "Tool details");
    pty.write(b"\x1b");
    parser.process(&pty.collect_for(Duration::from_millis(250)));
    assert!(!parser.screen().alternate_screen());
    assert!(parser.screen().contents().contains("保留草稿"));
    assert_eq!(
        parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    pty.write(b"\x15");
    parser.process(&pty.collect_for(Duration::from_millis(100)));
    assert_eq!(
        server.requests.lock().unwrap().len(),
        2,
        "viewing output must not call the provider"
    );
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());

    // 删除当前文件后依然查看历史 artifact，防止把新文件内容误作旧执行输出。
    std::fs::remove_file(workspace.path().join("log.txt")).unwrap();
    let mut resumed = PtyHarness::spawn_with_resume(
        home.path(),
        workspace.path(),
        100,
        24,
        true,
        Some("saved-card-test"),
    );
    let mut restored = ScreenModel::new(24, 100);
    wait_for_visible(&mut resumed, &mut restored, "Ask Golutra");
    assert_eq!(
        restored.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    resumed.write(b"\x0f");
    wait_for_visible(&mut resumed, &mut restored, "Tool details");
    resumed.write("/日志239\r".as_bytes());
    wait_for_visible(&mut resumed, &mut restored, "日志239");
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    resumed.write(b"\x1b");
    restored.process(&resumed.collect_for(Duration::from_millis(200)));
    submit(&mut resumed, &mut restored, "/quit");
    assert!(resumed.wait().1.success());
}

#[test]
fn single_file_diff_has_tree_counts_backgrounds_and_no_metadata() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let before = "# Demo\n\nText\n\n```python\n# 中文注释\nprint(\"Hello, world!\")\n```\n";
    std::fs::write(workspace.path().join("test.md"), before).unwrap();
    let server = FixtureServer::with_rounds(vec![
        (
            "修改注释。".into(),
            vec![
                json!({"index":0,"id":"edit-comment","type":"function","function":{"name":"edit_file","arguments":json!({"path":"test.md","edits":[{"old_text":"# 中文注释","new_text":"# English comment"}]}).to_string()}}),
            ],
        ),
        ("COMMENT_DONE".into(), vec![]),
    ]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 100, 32, true);
    let mut parser = ScreenModel::new(32, 100);
    wait_for_visible(&mut pty, &mut parser, "Ask Golutra");
    submit(&mut pty, &mut parser, "修改注释");
    wait_for_visible(&mut pty, &mut parser, "COMMENT_DONE");
    parser.process(&pty.collect_for(Duration::from_millis(250)));
    for expanded in [false, true] {
        if expanded {
            pty.write(b"\x0f");
            wait_for_visible(&mut pty, &mut parser, "Tool details");
            wait_for_visible(&mut pty, &mut parser, "6 + # English comment");
            parser.process(&pty.collect_for(Duration::from_millis(300)));
        }
        let text = parser.screen().contents();
        assert_eq!(text.matches("test.md").count(), 1, "{text}");
        assert!(text.contains("└ (+1 -1)"), "{text}");
        assert!(text.contains("6 - # 中文注释"), "{text}");
        assert!(text.contains("6 + # English comment"), "{text}");
        for hidden in [
            "@@",
            "Arguments",
            "old_text",
            "new_text",
            "more changes",
            "--- a/",
        ] {
            assert!(!text.contains(hidden), "unexpected {hidden}: {text}");
        }
        let removed = screen_row(&parser, "6 - # 中文注释") as u16;
        let added = screen_row(&parser, "6 + # English comment") as u16;
        let context = screen_row(&parser, "7   print") as u16;
        let red = parser.screen().cell(removed, 90).unwrap().bgcolor();
        let green = parser.screen().cell(added, 90).unwrap().bgcolor();
        assert_ne!(red, green);
        assert_ne!(red, parser.screen().cell(context, 90).unwrap().bgcolor());
        assert_ne!(green, parser.screen().cell(context, 90).unwrap().bgcolor());
        assert_eq!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        );
    }
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    pty.write(b"\x0f");
    parser.process(&pty.collect_for(Duration::from_millis(200)));
    pty.resize(80, 36);
    parser.screen_mut().set_size(36, 80);
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    let removed = screen_row(&parser, "6 - # 中文注释") as u16;
    let added = screen_row(&parser, "6 + # English comment") as u16;
    assert_ne!(
        parser.screen().cell(removed, 70).unwrap().bgcolor(),
        parser.screen().cell(added, 70).unwrap().bgcolor()
    );
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}

#[test]
fn file_cards_show_numbered_diff_and_open_saved_multi_file_changes() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let old = (1..=100)
        .map(|n| format!("old-{n:03}\n"))
        .collect::<String>();
    let new = (1..=100)
        .map(|n| format!("新行-{n:03}\n"))
        .collect::<String>();
    // 纯展示夹具使用文档文件，不触发行为变更的独立验证；查看详情仍必须零额外模型请求。
    std::fs::write(workspace.path().join("sample.md"), &old).unwrap();
    std::fs::write(workspace.path().join("gone.md"), "remove me\n").unwrap();
    let patch = format!(
        "*** Begin Patch\n*** Update File: sample.md\n@@\n{}{}*** Add File: added.md\n+hello world\n*** Delete File: gone.md\n*** End Patch\n",
        old.lines()
            .map(|line| format!("-{line}\n"))
            .collect::<String>(),
        new.lines()
            .map(|line| format!("+{line}\n"))
            .collect::<String>()
    );
    let server = FixtureServer::with_rounds(vec![
        (
            "修改三个文件。".into(),
            vec![
                json!({"index":0,"id":"file-diff","type":"function","function":{"name":"apply_patch","arguments":json!({"patch":patch}).to_string()}}),
            ],
        ),
        ("DIFF_TEST_DONE".into(), vec![]),
    ]);
    server.install(home.path());
    let mut pty = PtyHarness::spawn_configured(home.path(), workspace.path(), 100, 40, true);
    let mut parser = ScreenModel::new(40, 100);
    wait_for_visible(&mut pty, &mut parser, "Ask Golutra");
    submit(&mut pty, &mut parser, "修改文件");
    wait_for_visible(&mut pty, &mut parser, "DIFF_TEST_DONE");
    parser.process(&pty.collect_for(Duration::from_millis(300)));
    let text = all_terminal_rows(&mut parser);
    assert!(text.contains("Edited 3 files"), "{text}");
    assert!(text.contains("└ (+101 -101)"), "{text}");
    assert!(!text.contains("@@"), "{text}");
    assert!(!text.contains("Arguments"), "{text}");
    assert!(text.contains("old-001"), "{text}");
    assert!(!text.contains("新行-100"), "default diff is bounded");
    let added_row = screen_row(&parser, "hello world") as u16;
    let deleted_row = screen_row(&parser, "remove me") as u16;
    assert_ne!(
        parser.screen().cell(added_row, 90).unwrap().bgcolor(),
        parser.screen().cell(deleted_row, 90).unwrap().bgcolor(),
        "added and removed backgrounds extend past the code"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("sample.md")).unwrap(),
        new
    );
    pty.write(b"\x0f");
    wait_for_visible(&mut pty, &mut parser, "Tool details");
    pty.write("/新行-100\r".as_bytes());
    wait_for_visible(&mut pty, &mut parser, "100 + 新行-100");
    assert!(
        parser.screen().contents().contains("100 + 新行-100"),
        "{}",
        parser.screen().contents()
    );
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    pty.write(b"\x1b");
    parser.process(&pty.collect_for(Duration::from_millis(200)));
    submit(&mut pty, &mut parser, "/quit");
    assert!(pty.wait().1.success());
}
