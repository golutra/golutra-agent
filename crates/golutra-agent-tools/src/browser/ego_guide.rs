//! 从已安装 SDK 读取完整说明后分页；用内容指纹阻止升级期间混读不同版本。
use serde_json::json;

use super::{ExternalToolOutput, ShellOutput, ToolRequest, checksum};

pub(super) fn output(request: &ToolRequest, process: ShellOutput) -> ExternalToolOutput {
    let offset = request.arguments["offset"].as_u64().unwrap_or(0) as usize;
    let guide_id = checksum(process.raw_output.as_bytes());
    let end_marker = process
        .raw_output
        .lines()
        .find(|line| line.starts_with("[ego-browser:skill] end of guide"));
    let complete = !process.output_truncated && end_marker.is_some();
    let matching = match request.arguments["guide_id"].as_str() {
        Some(id) => id == guide_id,
        None => offset == 0,
    };
    let chars = process.raw_output.chars().collect::<Vec<_>>();
    let end = offset.saturating_add(6_000).min(chars.len());
    let failed = process.exit_code != Some(0)
        || process.timed_out
        || process.cancelled
        || !complete
        || !matching
        || offset >= chars.len();
    let mut content: String = chars.get(offset..end).unwrap_or_default().iter().collect();
    if offset == 0 {
        content.insert_str(0, "Browser selection: automatic mode prefers installed Ego, then Chrome. Ego uses the native SDK and does not need the OpenCLI extension. Keep OpenCLI disabled in Ego to prevent conflicts with Chrome. If neither browser is installed, ask for consent before running golutra-agent browser install-ego --yes.\n\n");
        content.insert_str(0, "Golutra integration: use the provided task/page. Never create/claim unrelated Spaces or call task.finish/handOff directly. browser_finish defaults to keep:all. A later continuation reclaims only this goal's confirmed retained Space and adopts pages with NEW labels; omit page on first continuation. Set new_goal:true for a genuinely new goal after finish. Unknown outcomes require fixed observation WITHOUT code. When requires_reload is true, observation does not unlock actions: recovery:reload reloads all managed pages (unsaved edits may be lost), then observes. User handoff requires confirmation and resume_after_user:true. Use browser.screenshot(page) to attach an image. Read every help page with next_offset and guide_id. Standalone lifecycle examples below do not override these rules.\n\n");
    }
    ExternalToolOutput {
        summary: if failed {
            "Ego guide incomplete, changed or unavailable; restart browser_help at offset 0"
        } else {
            "Ego SDK guide; follow next_offset with guide_id until null"
        }
        .into(),
        content,
        structured_facts: json!({"exit_code":process.exit_code,"truncated":process.output_truncated,
            "guide_complete":complete,"guide_id":guide_id,"end_marker":end_marker,
            "guide_matches":matching,"offset":offset,"next_offset":(!failed && end < chars.len()).then_some(end),
            "total_chars":chars.len(),"timed_out":process.timed_out,"cancelled":process.cancelled}),
        is_error: failed,
        images: Vec::new(),
    }
}
