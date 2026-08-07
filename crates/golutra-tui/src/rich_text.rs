//! Rich transcript rendering.
//!
//! Markdown parsing, display-width layout, and terminal-facing styling are deliberately separate:
//! the parser produces semantic blocks, the layout layer owns wrapping and indentation, and this
//! module exposes the small facade consumed by transcript and history views.

mod code;
mod layout;
mod markdown;
mod model;
mod table;
mod theme;
mod wrap;

use ratatui::text::Line;

pub(crate) fn markdown_lines(markdown: &str, width: u16) -> Vec<Line<'static>> {
    let document = markdown::parse_markdown(markdown);
    layout::render_markdown_document(&document, usize::from(width.max(1)))
}

pub(crate) fn detail_line(value: &str) -> Line<'static> {
    code::detail_line(value)
}

#[cfg(test)]
mod tests {
    use ratatui::{
        style::{Color, Modifier},
        text::Line,
    };

    use super::*;

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn markdown_preserves_structure_without_showing_delimiters() {
        let lines = markdown_lines("# Result\n\n- **one**\n- `two`", 80);
        assert_eq!(text(&lines), "Result\n\n- one\n- two");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn markdown_lists_keep_semantic_markers_without_detached_bullets() {
        let lines = markdown_lines(
            "- **Persistent services**\n\n  Keep the service running.\n\n  - Child\n\n- Next\n\n3. Third\n4. Fourth",
            80,
        );

        assert_eq!(
            text(&lines),
            "- Persistent services\n\n  Keep the service running.\n  - Child\n- Next\n\n3. Third\n4. Fourth"
        );
        assert!(
            lines
                .iter()
                .all(|line| !matches!(line.to_string().trim(), "•" | "-"))
        );
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::DarkGray));
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content == "3. " && span.style.fg == Some(Color::LightBlue))
        }));
    }

    #[test]
    fn heading_style_does_not_leak_into_following_body_text() {
        let lines = markdown_lines("## Result\n\nPlain body", 80);

        assert_eq!(text(&lines), "Result\n\nPlain body");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            !lines[2].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn semantic_blocks_use_a_restrained_color_hierarchy() {
        let lines = markdown_lines(
            "# Heading\n\nBody with [docs](https://example.com) and `code`.\n\n> quoted\n\n- item",
            80,
        );

        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        let link = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.contains("docs"))
            .expect("link span");
        assert_eq!(link.style.fg, Some(Color::Cyan));
        assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));
        let inline_code = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == "code")
            .expect("inline code span");
        assert_eq!(inline_code.style.fg, Some(Color::Cyan));
        let quote = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == "quoted")
            .expect("quote span");
        assert_eq!(quote.style.fg, Some(Color::Green));
    }

    #[test]
    fn nested_inline_semantics_keep_enclosing_modifiers() {
        let lines = markdown_lines(
            "***[docs](https://example.com)*** and **![diagram](diagram.png)**",
            80,
        );

        let docs = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.contains("docs"))
            .expect("link label");
        assert_eq!(docs.style.fg, Some(Color::Cyan));
        assert!(docs.style.add_modifier.contains(Modifier::BOLD));
        assert!(docs.style.add_modifier.contains(Modifier::ITALIC));
        assert!(docs.style.add_modifier.contains(Modifier::UNDERLINED));

        let image = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.contains("diagram"))
            .expect("image label");
        assert_eq!(image.style.fg, Some(Color::Magenta));
        assert!(image.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn fenced_code_and_diff_lines_have_distinct_styles() {
        let lines = markdown_lines("```rust\nlet value = 1;\n```", 80);
        assert_eq!(text(&lines), "┌ rust\n│ let value = 1;");
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|span| span.content == "let" && span.style.fg == Some(Color::Cyan))
        );

        assert_eq!(detail_line("+added").spans[0].style.fg, Some(Color::Green));
        assert_eq!(detail_line("-removed").spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn markdown_wraps_english_and_cjk_to_the_requested_display_width() {
        let english = markdown_lines("alpha beta gamma delta", 12);
        assert_eq!(text(&english), "alpha beta\ngamma delta");

        let chinese = markdown_lines("这是一个用于验证中文换行宽度的段落", 12);
        assert!(chinese.len() > 1);
        assert!(chinese.iter().all(|line| line.width() <= 12));
        assert_eq!(
            text(&chinese).replace('\n', ""),
            "这是一个用于验证中文换行宽度的段落"
        );
    }

    #[test]
    fn wrapped_lists_use_hanging_indentation() {
        let lines = markdown_lines(
            "- alpha beta gamma delta\n10. first second third fourth",
            16,
        );

        assert_eq!(
            text(&lines),
            "- alpha beta\n  gamma delta\n\n10. first second\n    third fourth"
        );
        assert!(lines.iter().all(|line| line.width() <= 16));

        let nested_only = text(&markdown_lines("-\n  - child", 16));
        assert!(!nested_only.contains("- - child"));
    }

    #[test]
    fn tables_use_columns_when_wide_and_records_when_narrow() {
        let source = "| Name | Status |\n| --- | --- |\n| parser | ready |\n| renderer | active |";

        let wide = text(&markdown_lines(source, 40));
        assert!(wide.contains("Name"));
        assert!(wide.contains("Status"));
        assert!(wide.contains('━'));

        let narrow_lines = markdown_lines(source, 14);
        let narrow = text(&narrow_lines);
        assert!(narrow.contains("Name: parser"));
        assert!(narrow.contains("Status: ready"));
        assert!(narrow_lines.iter().all(|line| line.width() <= 14));

        let header_only = text(&markdown_lines("| Name | Status |\n| --- | --- |", 10));
        assert!(!header_only.contains("Name: Name"));
    }

    #[test]
    fn every_markdown_block_respects_narrow_terminal_boundaries() {
        let source = "# Heading\n\n100. ordered content with a longwordthatmustsplit\n\n> quoted 中文 content\n\n```rust\nlet very_long_identifier = 123;\n```\n\n| Name | Status |\n| --- | --- |\n| renderer | active |";

        for width in 2..=18 {
            let lines = markdown_lines(source, width);
            assert!(
                lines.iter().all(|line| line.width() <= usize::from(width)),
                "width {width} overflowed: {:?}",
                lines.iter().map(ToString::to_string).collect::<Vec<_>>()
            );
        }
    }
}
