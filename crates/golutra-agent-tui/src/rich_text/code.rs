//! Styling for code blocks, diffs, and structured tool details.

use ratatui::text::{Line, Span};

use super::theme;

pub(super) fn detail_line(value: &str) -> Line<'static> {
    if matches!(value, "Output" | "Diff" | "Arguments") {
        return Line::from(Span::styled(value.to_owned(), theme::detail_heading()));
    }
    if value.starts_with("diff ")
        || value.starts_with("index ")
        || value.starts_with("--- ")
        || value.starts_with("+++ ")
    {
        return Line::from(Span::styled(value.to_owned(), theme::diff_metadata()));
    }
    if value.starts_with("@@") {
        return Line::from(Span::styled(value.to_owned(), theme::diff_hunk()));
    }
    if value.starts_with('+') && !value.starts_with("+++") {
        return Line::from(Span::styled(value.to_owned(), theme::diff_addition()));
    }
    if value.starts_with('-') && !value.starts_with("---") {
        return Line::from(Span::styled(value.to_owned(), theme::diff_deletion()));
    }
    if looks_like_json(value) {
        return Line::from(highlight_code(value, Some("json")));
    }
    Line::from(Span::styled(value.to_owned(), theme::body()))
}

pub(super) fn highlight_code(line: &str, language: Option<&str>) -> Vec<Span<'static>> {
    super::syntax::highlight(line, language)
        .into_iter()
        .next()
        .unwrap_or_default()
}

fn looks_like_json(value: &str) -> bool {
    let trimmed = value.trim();
    (trimmed.starts_with('{') || trimmed.starts_with('[') || trimmed.starts_with('"'))
        && (trimmed.ends_with('}') || trimmed.ends_with(']') || trimmed.ends_with(','))
}
