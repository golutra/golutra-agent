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
    let width = parser.screen().size().1;
    let rows = parser.screen().rows(0, width).collect::<Vec<_>>();
    let last = rows.len() - 1;
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
fn bottom_alignment_survives_completion_input_shrink_and_height_only_resize() {
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
    assert_idle_composer_at_bottom(&parser);
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
    assert_idle_composer_at_bottom(&parser);
    assert_once_in_order(&all_terminal_rows(&mut parser), &markers);
    assert!(!all_terminal_rows(&mut parser).contains("草稿"));

    submit(&mut pty, &mut parser, "/status");
    parser.process(&pty.collect_for(Duration::from_millis(500)));
    assert_idle_composer_at_bottom(&parser);
    for height in [36, 112, 6, 28] {
        pty.resize(100, height);
        parser.screen_mut().set_size(height, 100);
        parser.process(&pty.collect_for(Duration::from_millis(800)));
        assert_idle_composer_at_bottom(&parser);
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
    assert_idle_composer_at_bottom(&parser);
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
            "• read first.txt",
            "第一个已读，继续读第二个。",
            "• read second.txt",
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
    pty.write(b"\x0f");
    parser.process(&pty.collect_for(Duration::from_millis(600)));
    let expanded = all_terminal_rows(&mut parser);
    assert!(
        expanded.contains("RAW_HTML_SHOULD_STAY_IN_DETAILS"),
        "archived tools must remain inspectable with Ctrl+O:\n{expanded}"
    );
    pty.write(b"\x0f");
    parser.process(&pty.collect_for(Duration::from_millis(600)));
    assert!(!all_terminal_rows(&mut parser).contains("RAW_HTML_SHOULD_STAY_IN_DETAILS"));
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
