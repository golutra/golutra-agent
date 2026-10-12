//! 按受管进程的连续字节游标还原验证输出；脱敏、丢失和缺口不能拼出不存在的证据。

use golutra_agent_tools::{ArtifactContent, ToolExecutionReport};

const MAX_PENDING_LINE_BYTES: usize = 16 * 1024;

/// 只在同一进程的调用者已完成身份校验后使用；派生内容不写回原始 artifact。
pub(super) fn normalize(reports: &[&ToolExecutionReport]) -> Vec<ArtifactContent> {
    let mut output = Vec::new();
    let mut pending = Vec::new();
    let mut cursor = None;
    let mut discard_line = false;
    let mut seen = std::collections::HashSet::new();
    for report in reports {
        let facts = &report.envelope.structured_facts;
        for artifact in &report.artifact_contents {
            if !seen.insert(artifact.artifact_id) {
                continue;
            }
            let range = facts["output_start_cursor"]
                .as_u64()
                .zip(facts["output_end_cursor"].as_u64())
                .filter(|(start, end)| {
                    end.checked_sub(*start) == Some(artifact.bytes.len() as u64)
                        && facts["output_lost"] != true
                        && report.artifact_contents.len() == 1
                });
            let contiguous = range.is_some_and(|(start, _)| cursor == Some(start));
            if !contiguous {
                finish_line(&mut output, &mut pending, discard_line);
                discard_line = false;
            }
            // 重叠续读独立解析，不裁剪后拼接；这样不会把不一致或重复字节当成一行。
            let mut lines = Vec::new();
            for &byte in &artifact.bytes {
                if byte == b'\n' {
                    if !discard_line {
                        lines.append(&mut pending);
                        lines.push(b'\n');
                    }
                    pending.clear();
                    discard_line = false;
                } else if !discard_line {
                    if pending.len() < MAX_PENDING_LINE_BYTES {
                        pending.push(byte);
                    } else {
                        pending.clear();
                        discard_line = true;
                    }
                }
            }
            push_content(&mut output, lines);
            cursor = range.map(|(_, end)| end);
        }
    }
    finish_line(&mut output, &mut pending, discard_line);
    output
}

fn finish_line(output: &mut Vec<ArtifactContent>, pending: &mut Vec<u8>, discard: bool) {
    if !discard {
        push_content(output, std::mem::take(pending));
    }
    pending.clear();
}

fn push_content(output: &mut Vec<ArtifactContent>, bytes: Vec<u8>) {
    if !bytes.is_empty() {
        output.push(ArtifactContent {
            artifact_id: golutra_agent_core::ArtifactId::new(),
            bytes,
        });
    }
}
