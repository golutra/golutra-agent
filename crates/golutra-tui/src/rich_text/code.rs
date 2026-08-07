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
    if language.is_some_and(|language| language.eq_ignore_ascii_case("diff")) {
        return detail_line(line).spans;
    }
    let trimmed = line.trim_start();
    if trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with("--")
        || trimmed.starts_with(';')
    {
        return vec![Span::styled(line.to_owned(), theme::code_comment())];
    }

    let mut spans = Vec::new();
    let mut token = String::new();
    let mut quoted = None;
    for character in line.chars() {
        if let Some(quote) = quoted {
            token.push(character);
            if character == quote {
                spans.push(Span::styled(
                    std::mem::take(&mut token),
                    theme::code_string(),
                ));
                quoted = None;
            }
            continue;
        }
        if matches!(character, '\'' | '"' | '`') {
            push_code_token(&mut spans, &mut token);
            token.push(character);
            quoted = Some(character);
        } else if character.is_alphanumeric() || character == '_' {
            token.push(character);
        } else {
            push_code_token(&mut spans, &mut token);
            spans.push(Span::styled(
                character.to_string(),
                theme::code_punctuation(),
            ));
        }
    }
    if quoted.is_some() {
        spans.push(Span::styled(token, theme::code_string()));
    } else {
        push_code_token(&mut spans, &mut token);
    }
    spans
}

fn push_code_token(spans: &mut Vec<Span<'static>>, token: &mut String) {
    if token.is_empty() {
        return;
    }
    let style = if is_keyword(token) {
        theme::code_keyword()
    } else if token.chars().all(|character| character.is_ascii_digit()) {
        theme::code_number()
    } else {
        theme::body()
    };
    spans.push(Span::styled(std::mem::take(token), style));
}

fn is_keyword(value: &str) -> bool {
    matches!(
        value,
        "as" | "async"
            | "await"
            | "break"
            | "class"
            | "const"
            | "continue"
            | "def"
            | "else"
            | "enum"
            | "false"
            | "fn"
            | "for"
            | "from"
            | "function"
            | "if"
            | "impl"
            | "import"
            | "in"
            | "let"
            | "match"
            | "mod"
            | "mut"
            | "None"
            | "null"
            | "pub"
            | "return"
            | "self"
            | "Some"
            | "struct"
            | "true"
            | "type"
            | "use"
            | "while"
    )
}

fn looks_like_json(value: &str) -> bool {
    let trimmed = value.trim();
    (trimmed.starts_with('{') || trimmed.starts_with('[') || trimmed.starts_with('"'))
        && (trimmed.ends_with('}') || trimmed.ends_with(']') || trimmed.ends_with(','))
}
