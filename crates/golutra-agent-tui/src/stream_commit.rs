//! 按 Markdown 源码边界冻结流式前缀，禁止用屏幕行数推断语义是否稳定。

use pulldown_cmark::{Event, Parser};
use ratatui::text::Line;

/// 最后一个顶层块仍可能被后续 token 改写（表格列宽、列表或代码围栏），保留为活动尾部。
pub(crate) fn stable_source_end(source: &str) -> usize {
    let mut depth = 0usize;
    let mut last_block_start = 0;
    for (event, range) in
        Parser::new_ext(source, super::rich_text::markdown_options()).into_offset_iter()
    {
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    last_block_start = range.start;
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    last_block_start
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
