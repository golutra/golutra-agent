//! 按 Markdown 源码边界冻结流式前缀，禁止用屏幕行数推断语义是否稳定。

use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag};
use ratatui::text::Line;

pub(crate) fn stable_source_end(source: &str) -> usize {
    let mut depth = 0usize;
    let mut last_block_start = 0;
    let mut fenced = false;
    for (event, range) in
        Parser::new_ext(source, super::rich_text::markdown_options()).into_offset_iter()
    {
        match event {
            Event::Start(tag) => {
                if depth == 0 {
                    last_block_start = range.start;
                    fenced = matches!(tag, Tag::CodeBlock(CodeBlockKind::Fenced(_)));
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            Event::Rule if depth == 0 => {
                last_block_start = range.start;
                fenced = false;
            }
            _ => {}
        }
    }
    if fenced {
        complete_code_source_end(source, last_block_start)
    } else {
        last_block_start
    }
}

pub(crate) fn complete_code_source_end(source: &str, start: usize) -> usize {
    let tail = &source[start..];
    let opener = tail.trim_start_matches(' ');
    if tail.len() - opener.len() > 3 || (!opener.starts_with("```") && !opener.starts_with("~~~")) {
        return start;
    }
    let Some(header_end) = tail.find('\n') else {
        return start;
    };
    let Some(end) = tail.rfind('\n') else {
        return start;
    };
    if end > header_end {
        start + end + 1
    } else {
        start
    }
}

pub(crate) fn stable_rendered_prefix(
    complete: &[Line<'static>],
    prefix: &[Line<'static>],
) -> usize {
    complete
        .iter()
        .zip(prefix)
        .take_while(|(a, b)| a == b)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_fenced_lines_are_archived_before_the_closing_fence() {
        for fence in ["```rust", "~~~python"] {
            let source = format!("intro\n\n{fence}\nfirst\nsecond\npartial");
            assert_eq!(&source[stable_source_end(&source)..], "partial");
        }
        assert_eq!(complete_code_source_end("    first\n    second\n", 0), 0);
    }

    #[test]
    fn holds_open_tables_lists_and_fences_in_the_mutable_tail() {
        for tail in [
            "| 名称 | 值 |\n| --- | --- |\n| a | b |",
            "- 一\n- 二",
            "```rust\nfn main() {",
        ] {
            let source = format!("前言。\n\n{tail}");
            assert_eq!(&source[..stable_source_end(&source)], "前言。\n\n");
        }
    }

    #[test]
    fn incomplete_first_block_is_never_committed_by_visual_line_count() {
        assert_eq!(stable_source_end(&"长中文段落".repeat(100)), 0);
        assert_eq!(stable_source_end("| a | b |\n| --- | --- |\n| x |"), 0);
    }
}
