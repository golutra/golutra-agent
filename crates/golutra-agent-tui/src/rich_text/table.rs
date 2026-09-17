//! Adaptive table layout with column and narrow key/value presentations.

use pulldown_cmark::Alignment;
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use super::{
    model::{MarkdownTable, RichText},
    theme,
    wrap::{hard_wrap_spans, line_width, prefix_lines, push_span, wrap_rich_text},
};

const COLUMN_GAP: usize = 2;
const MIN_COLUMN_WIDTH: usize = 7;

pub(super) fn render_table(
    table: &MarkdownTable,
    width: usize,
    base_style: Style,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let column_count = table
        .alignments
        .len()
        .max(table.header.len())
        .max(table.rows.iter().map(Vec::len).max().unwrap_or_default());
    if column_count == 0 {
        return Vec::new();
    }

    let gap_width = COLUMN_GAP.saturating_mul(column_count.saturating_sub(1));
    let content_width = width.saturating_sub(gap_width);
    if content_width < MIN_COLUMN_WIDTH.saturating_mul(column_count) {
        return render_records(table, column_count, width, base_style);
    }

    let mut widths = natural_column_widths(table, column_count);
    shrink_columns(&mut widths, content_width);
    if widths.iter().any(|column| *column < MIN_COLUMN_WIDTH) {
        return render_records(table, column_count, width, base_style);
    }
    render_columns(table, column_count, &widths, base_style)
}

fn render_columns(
    table: &MarkdownTable,
    column_count: usize,
    widths: &[usize],
    base_style: Style,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !table.header.is_empty() {
        lines.extend(render_row(
            &table.header,
            widths,
            &table.alignments,
            true,
            base_style,
        ));
        lines.push(separator_line(widths, '━'));
    }

    for (row_index, row) in table.rows.iter().enumerate() {
        lines.extend(render_row(
            row,
            widths,
            &table.alignments,
            false,
            base_style,
        ));
        if row_index + 1 < table.rows.len() {
            lines.push(separator_line(widths, '─'));
        }
    }

    if lines.is_empty() && column_count > 0 {
        lines.push(Line::default());
    }
    lines
}

fn render_row(
    cells: &[RichText],
    widths: &[usize],
    alignments: &[Alignment],
    header: bool,
    base_style: Style,
) -> Vec<Line<'static>> {
    let wrapped = widths
        .iter()
        .enumerate()
        .map(|(column, width)| {
            let mut lines = cells.get(column).map_or_else(
                || vec![Line::default()],
                |cell| wrap_rich_text(cell, *width, base_style),
            );
            if header {
                for line in &mut lines {
                    for span in &mut line.spans {
                        span.style = span.style.patch(theme::table_header());
                    }
                }
            }
            lines
        })
        .collect::<Vec<_>>();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);

    (0..height)
        .map(|row| {
            let mut spans = Vec::new();
            for (column, width) in widths.iter().copied().enumerate() {
                if column > 0 {
                    push_span(
                        &mut spans,
                        Span::styled(" ".repeat(COLUMN_GAP), theme::muted()),
                    );
                }
                let line = wrapped[column].get(row).cloned().unwrap_or_default();
                let alignment = alignments.get(column).copied().unwrap_or(Alignment::Left);
                spans.extend(align_line(line, width, alignment));
            }
            Line::from(spans)
        })
        .collect()
}

fn render_records(
    table: &MarkdownTable,
    column_count: usize,
    width: usize,
    base_style: Style,
) -> Vec<Line<'static>> {
    if table.rows.is_empty() {
        return table
            .header
            .iter()
            .flat_map(|cell| {
                let mut lines = wrap_rich_text(cell, width, base_style);
                for line in &mut lines {
                    for span in &mut line.spans {
                        span.style = span.style.patch(theme::table_header());
                    }
                }
                lines
            })
            .collect();
    }
    let records = &table.rows;
    let mut lines = Vec::new();
    for (record_index, record) in records.iter().enumerate() {
        if record_index > 0 {
            lines.push(Line::from(Span::styled(
                "─".repeat(width.min(24)),
                theme::muted(),
            )));
        }
        for column in 0..column_count {
            let label = table
                .header
                .get(column)
                .map(RichText::plain_text)
                .filter(|label| !label.trim().is_empty())
                .unwrap_or_else(|| format!("Column {}", column + 1));
            let prefix = format!("{}: ", label.trim());
            let prefix_width = UnicodeWidthStr::width(prefix.as_str());
            let value = record.get(column).cloned().unwrap_or_default();
            if prefix_width < width {
                let value_lines =
                    wrap_rich_text(&value, width.saturating_sub(prefix_width), base_style);
                lines.extend(prefix_lines(
                    value_lines,
                    vec![Span::styled(
                        prefix,
                        theme::muted().patch(theme::table_header()),
                    )],
                    vec![Span::raw(" ".repeat(prefix_width))],
                ));
            } else {
                lines.extend(hard_wrap_spans(
                    &[Span::styled(
                        label,
                        theme::muted().patch(theme::table_header()),
                    )],
                    width,
                ));
                if width >= 4 {
                    lines.extend(prefix_lines(
                        wrap_rich_text(&value, width - 2, base_style),
                        vec![Span::raw("  ")],
                        vec![Span::raw("  ")],
                    ));
                } else {
                    lines.extend(wrap_rich_text(&value, width, base_style));
                }
            }
        }
    }
    lines
}

fn natural_column_widths(table: &MarkdownTable, column_count: usize) -> Vec<usize> {
    (0..column_count)
        .map(|column| {
            table
                .header
                .get(column)
                .into_iter()
                .chain(table.rows.iter().filter_map(|row| row.get(column)))
                .map(rich_text_width)
                .max()
                .unwrap_or(MIN_COLUMN_WIDTH)
                .max(MIN_COLUMN_WIDTH)
        })
        .collect()
}

fn rich_text_width(text: &RichText) -> usize {
    text.lines
        .iter()
        .map(|line| {
            line.iter()
                .map(|run| UnicodeWidthStr::width(run.text.as_str()))
                .sum()
        })
        .max()
        .unwrap_or_default()
}

fn shrink_columns(widths: &mut [usize], target: usize) {
    let mut total = widths.iter().sum::<usize>();
    while total > target {
        let Some((index, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, width)| **width > MIN_COLUMN_WIDTH)
            .max_by_key(|(_, width)| **width)
        else {
            break;
        };
        widths[index] = widths[index].saturating_sub(1);
        total = total.saturating_sub(1);
    }
}

fn separator_line(widths: &[usize], character: char) -> Line<'static> {
    let mut spans = Vec::new();
    for (column, width) in widths.iter().copied().enumerate() {
        if column > 0 {
            push_span(
                &mut spans,
                Span::styled(" ".repeat(COLUMN_GAP), theme::muted()),
            );
        }
        push_span(
            &mut spans,
            Span::styled(character.to_string().repeat(width), theme::muted()),
        );
    }
    Line::from(spans)
}

fn align_line(line: Line<'static>, width: usize, alignment: Alignment) -> Vec<Span<'static>> {
    let content_width = line_width(&line).min(width);
    let padding = width.saturating_sub(content_width);
    let (left, right) = match alignment {
        Alignment::Left | Alignment::None => (0, padding),
        Alignment::Center => (padding / 2, padding.saturating_sub(padding / 2)),
        Alignment::Right => (padding, 0),
    };
    let mut spans = Vec::new();
    if left > 0 {
        spans.push(Span::raw(" ".repeat(left)));
    }
    spans.extend(line.spans);
    if right > 0 {
        spans.push(Span::raw(" ".repeat(right)));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::super::{markdown::parse_markdown, model::MarkdownBlock};
    use super::*;

    #[test]
    fn wide_table_rows_never_cross_the_layout_width() {
        let document = parse_markdown(
            "| Name | Description |\n| --- | --- |\n| parser | semantic markdown parser |",
        );
        let MarkdownBlock::Table(table) = &document.blocks[0] else {
            panic!("expected table");
        };

        let lines = render_table(table, 32, theme::body());

        assert!(lines.iter().all(|line| line_width(line) <= 32));
        assert!(lines.iter().any(|line| line.to_string().contains('━')));
    }
}
