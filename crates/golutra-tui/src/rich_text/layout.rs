//! Width-aware block layout from the semantic Markdown model to terminal lines.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use super::{
    code::highlight_code,
    model::{MarkdownBlock, MarkdownDocument, MarkdownList},
    table::render_table,
    theme,
    wrap::{hard_wrap_spans, prefix_lines, wrap_rich_text},
};

#[derive(Clone, Copy)]
enum BlockContext {
    Document,
    ListItem,
}

pub(super) fn render_markdown_document(
    document: &MarkdownDocument,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = render_blocks(
        &document.blocks,
        width.max(1),
        BlockContext::Document,
        theme::body(),
    );
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

fn render_blocks(
    blocks: &[MarkdownBlock],
    width: usize,
    context: BlockContext,
    base_style: Style,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut previous = None;
    for block in blocks {
        if previous.is_some_and(|_| needs_gap(block, context))
            && lines
                .last()
                .is_some_and(|line: &Line<'_>| !line.spans.is_empty())
        {
            lines.push(Line::default());
        }
        lines.extend(render_block(block, width.max(1), base_style));
        previous = Some(block);
    }
    lines
}

fn needs_gap(current: &MarkdownBlock, context: BlockContext) -> bool {
    if matches!(context, BlockContext::ListItem) && matches!(current, MarkdownBlock::List(_)) {
        return false;
    }
    true
}

fn render_block(block: &MarkdownBlock, width: usize, base_style: Style) -> Vec<Line<'static>> {
    match block {
        MarkdownBlock::Paragraph(content) => wrap_rich_text(content, width, base_style),
        MarkdownBlock::Heading { level, content } => {
            wrap_rich_text(content, width, base_style.patch(theme::heading(*level)))
        }
        MarkdownBlock::Quote(blocks) => render_quote(blocks, width, base_style),
        MarkdownBlock::List(list) => render_list(list, width, base_style),
        MarkdownBlock::Code { language, source } => render_code(language.as_deref(), source, width),
        MarkdownBlock::Rule => vec![Line::from(Span::styled(
            "─".repeat(width.min(32)),
            theme::muted(),
        ))],
        MarkdownBlock::Table(table) => render_table(table, width, base_style),
    }
}

fn render_quote(blocks: &[MarkdownBlock], width: usize, base_style: Style) -> Vec<Line<'static>> {
    if width <= 2 {
        return render_blocks(
            blocks,
            width,
            BlockContext::Document,
            base_style.patch(theme::quote()),
        );
    }
    let prefix = if width >= 4 { "│ " } else { "│" };
    let prefix_width = UnicodeWidthStr::width(prefix);
    let inner = render_blocks(
        blocks,
        width - prefix_width,
        BlockContext::Document,
        base_style.patch(theme::quote()),
    );
    prefix_lines(
        inner,
        vec![Span::styled(prefix, theme::muted())],
        vec![Span::styled(prefix, theme::muted())],
    )
}

fn render_list(list: &MarkdownList, width: usize, base_style: Style) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut index = list.start.unwrap_or(1);
    for item in &list.items {
        let (marker, style) = if list.start.is_some() {
            let marker = format!("{index}. ");
            index = index.saturating_add(1);
            (marker, theme::ordered_marker())
        } else {
            ("- ".to_owned(), theme::muted())
        };
        if let [MarkdownBlock::List(nested)] = item.as_slice() {
            let indent_width = if width >= 4 { 2 } else { 0 };
            let nested_lines = render_list(
                nested,
                width.saturating_sub(indent_width).max(1),
                base_style,
            );
            if indent_width == 0 {
                lines.extend(nested_lines);
            } else {
                lines.extend(prefix_lines(
                    nested_lines,
                    vec![Span::raw(" ".repeat(indent_width))],
                    vec![Span::raw(" ".repeat(indent_width))],
                ));
            }
            continue;
        }
        let marker_width = UnicodeWidthStr::width(marker.as_str());
        if marker_width >= width || width.saturating_sub(marker_width) < 2 {
            lines.extend(hard_wrap_spans(&[Span::styled(marker, style)], width));
            lines.extend(render_blocks(
                item,
                width,
                BlockContext::ListItem,
                base_style,
            ));
            continue;
        }
        let inner_width = width.saturating_sub(marker_width).max(1);
        let mut item_lines = render_blocks(item, inner_width, BlockContext::ListItem, base_style);
        if item_lines.is_empty() {
            item_lines.push(Line::default());
        }
        lines.extend(item_lines.into_iter().enumerate().map(|(row, line)| {
            if row > 0 && line.spans.is_empty() {
                return Line::default();
            }
            let mut spans = vec![if row == 0 {
                Span::styled(marker.clone(), style)
            } else {
                Span::raw(" ".repeat(marker_width))
            }];
            spans.extend(line.spans);
            Line::from(spans)
        }));
    }
    lines
}

fn render_code(language: Option<&str>, source: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(language) = language {
        lines.extend(hard_wrap_spans(
            &[
                Span::styled("┌ ", theme::muted()),
                Span::styled(language.to_owned(), theme::accent()),
            ],
            width,
        ));
    }

    let code_prefix = if width >= 4 {
        "│ "
    } else if width >= 3 {
        "│"
    } else {
        ""
    };
    let prefix_width = UnicodeWidthStr::width(code_prefix);
    let content_width = width.saturating_sub(prefix_width).max(1);
    let mut source_lines = source.split('\n').collect::<Vec<_>>();
    if source_lines.last() == Some(&"") && source_lines.len() > 1 {
        source_lines.pop();
    }
    if source_lines.is_empty() {
        source_lines.push("");
    }
    for source_line in source_lines {
        let highlighted = highlight_code(source_line, language);
        let wrapped = hard_wrap_spans(&highlighted, content_width);
        if prefix_width > 0 && width > prefix_width {
            lines.extend(prefix_lines(
                wrapped,
                vec![Span::styled(code_prefix, theme::muted())],
                vec![Span::styled(code_prefix, theme::muted())],
            ));
        } else {
            lines.extend(wrapped);
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::super::{markdown::parse_markdown, wrap::line_width};
    use super::*;

    fn plain(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn nested_lists_and_quotes_stay_inside_the_requested_width() {
        let document = parse_markdown(
            "- parent text that wraps\n  - child text that wraps\n\n> quoted text that also wraps",
        );
        let lines = render_markdown_document(&document, 16);

        assert!(lines.iter().all(|line| line_width(line) <= 16));
        let output = plain(&lines);
        assert!(output.contains("- parent text"));
        assert!(output.contains("  - child text"));
        assert!(output.contains("│ quoted text"));
    }
}
