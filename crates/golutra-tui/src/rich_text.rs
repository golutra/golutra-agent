//! Markdown, code, and diff rendering for transcript content.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

pub(crate) fn markdown_lines(markdown: &str) -> Vec<Line<'static>> {
    let mut renderer = MarkdownRenderer::default();
    for event in Parser::new_ext(
        markdown,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS,
    ) {
        renderer.event(event);
    }
    renderer.finish()
}

pub(crate) fn detail_line(value: &str) -> Line<'static> {
    if value == "Output" || value == "Diff" || value == "Arguments" {
        return Line::from(Span::styled(
            value.to_owned(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if value.starts_with("diff ")
        || value.starts_with("index ")
        || value.starts_with("--- ")
        || value.starts_with("+++ ")
    {
        return Line::from(Span::styled(
            value.to_owned(),
            Style::default().fg(Color::Cyan),
        ));
    }
    if value.starts_with("@@") {
        return Line::from(Span::styled(
            value.to_owned(),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if value.starts_with('+') && !value.starts_with("+++") {
        return Line::from(Span::styled(
            value.to_owned(),
            Style::default().fg(Color::Green),
        ));
    }
    if value.starts_with('-') && !value.starts_with("---") {
        return Line::from(Span::styled(
            value.to_owned(),
            Style::default().fg(Color::Red),
        ));
    }
    if looks_like_json(value) {
        return Line::from(highlight_code(value, Some("json")));
    }
    Line::from(Span::styled(
        value.to_owned(),
        Style::default().fg(Color::White),
    ))
}

#[derive(Default)]
struct MarkdownRenderer {
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    styles: Vec<Style>,
    list_depth: usize,
    quote_depth: usize,
    code_language: Option<String>,
}

impl MarkdownRenderer {
    fn event(&mut self, event: Event<'_>) {
        if self.code_language.is_some() {
            match event {
                Event::Text(text) => self.push_code_text(&text),
                Event::End(TagEnd::CodeBlock) => {
                    self.flush();
                    self.code_language = None;
                }
                _ => {}
            }
            return;
        }

        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.push_text(text.into_string()),
            Event::Code(code) => self.spans.push(Span::styled(
                code.into_string(),
                self.style()
                    .patch(Style::default().fg(Color::Cyan).bg(Color::Rgb(32, 36, 40))),
            )),
            Event::SoftBreak => self.push_text(" ".to_owned()),
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.flush();
                self.lines.push(Line::from(Span::styled(
                    "────────────────────────",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            Event::TaskListMarker(checked) => self.spans.push(Span::styled(
                if checked { "[x] " } else { "[ ] " },
                Style::default().fg(if checked {
                    Color::Green
                } else {
                    Color::DarkGray
                }),
            )),
            Event::Html(html) | Event::InlineHtml(html) => self.push_text(html.into_string()),
            Event::FootnoteReference(reference) => {
                self.push_text(format!("[{reference}]"));
            }
            Event::InlineMath(value) | Event::DisplayMath(value) => {
                self.push_text(value.into_string());
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.flush(),
            Tag::Heading { .. } => {
                self.flush();
                self.styles.push(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote_depth = self.quote_depth.saturating_add(1);
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.code_language = Some(match kind {
                    CodeBlockKind::Indented => String::new(),
                    CodeBlockKind::Fenced(language) => language.into_string(),
                });
                if self
                    .code_language
                    .as_ref()
                    .is_some_and(|language| !language.is_empty())
                {
                    self.lines.push(Line::from(vec![
                        Span::styled("┌ ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            self.code_language.clone().unwrap_or_default(),
                            Style::default().fg(Color::Cyan),
                        ),
                    ]));
                }
            }
            Tag::List(_) => {
                self.flush();
                self.list_depth = self.list_depth.saturating_add(1);
            }
            Tag::Item => {
                self.flush();
                self.spans.push(Span::styled(
                    format!("{}• ", "  ".repeat(self.list_depth.saturating_sub(1))),
                    Style::default().fg(Color::Cyan),
                ));
            }
            Tag::Emphasis => self
                .styles
                .push(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self
                .styles
                .push(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { .. } => self.styles.push(
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::UNDERLINED),
            ),
            Tag::Image { dest_url, .. } => {
                self.spans.push(Span::styled(
                    "[image: ",
                    Style::default().fg(Color::Magenta),
                ));
                self.styles.push(Style::default().fg(Color::Magenta));
                self.spans.push(Span::raw(dest_url.into_string()));
            }
            Tag::Table(_) | Tag::TableHead | Tag::TableRow => self.flush(),
            Tag::TableCell => {
                if !self.spans.is_empty() {
                    self.spans
                        .push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
                }
            }
            Tag::FootnoteDefinition(_) | Tag::HtmlBlock | Tag::MetadataBlock(_) => self.flush(),
            Tag::DefinitionList | Tag::DefinitionListTitle | Tag::DefinitionListDefinition => {
                self.flush();
            }
            Tag::Superscript => self.styles.push(Style::default().fg(Color::LightBlue)),
            Tag::Subscript => self.styles.push(Style::default().fg(Color::LightMagenta)),
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item | TagEnd::TableRow => {
                self.flush()
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::List(_) => {
                self.flush();
                self.list_depth = self.list_depth.saturating_sub(1);
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Link
            | TagEnd::Image
            | TagEnd::Superscript
            | TagEnd::Subscript => {
                self.styles.pop();
                if tag == TagEnd::Image {
                    self.spans
                        .push(Span::styled("]", Style::default().fg(Color::Magenta)));
                }
            }
            TagEnd::CodeBlock => {
                self.flush();
                self.code_language = None;
            }
            TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableCell
            | TagEnd::FootnoteDefinition
            | TagEnd::HtmlBlock
            | TagEnd::MetadataBlock(_)
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition => self.flush(),
        }
    }

    fn push_text(&mut self, text: String) {
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.flush();
            }
            if !part.is_empty() {
                self.spans.push(Span::styled(part.to_owned(), self.style()));
            }
        }
    }

    fn push_code_text(&mut self, text: &str) {
        let language = self.code_language.clone();
        for line in text.lines() {
            self.flush();
            let mut spans = vec![Span::styled("│ ", Style::default().fg(Color::DarkGray))];
            spans.extend(highlight_code(line, language.as_deref()));
            self.lines.push(Line::from(spans));
        }
    }

    fn style(&self) -> Style {
        self.styles
            .iter()
            .copied()
            .fold(Style::default().fg(Color::White), Style::patch)
    }

    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let mut spans = Vec::new();
        if self.quote_depth > 0 {
            spans.push(Span::styled(
                format!("{}│ ", "  ".repeat(self.quote_depth.saturating_sub(1))),
                Style::default().fg(Color::DarkGray),
            ));
        }
        spans.append(&mut self.spans);
        self.lines.push(Line::from(spans));
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        if self.lines.is_empty() {
            self.lines.push(Line::from(""));
        }
        self.lines
    }
}

fn highlight_code(line: &str, language: Option<&str>) -> Vec<Span<'static>> {
    if language.is_some_and(|language| language.eq_ignore_ascii_case("diff")) {
        return detail_line(line).spans;
    }
    let trimmed = line.trim_start();
    if trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with("--")
        || trimmed.starts_with(';')
    {
        return vec![Span::styled(
            line.to_owned(),
            Style::default().fg(Color::DarkGray),
        )];
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
                    Style::default().fg(Color::Green),
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
                Style::default().fg(Color::Gray),
            ));
        }
    }
    if quoted.is_some() {
        spans.push(Span::styled(token, Style::default().fg(Color::Green)));
    } else {
        push_code_token(&mut spans, &mut token);
    }
    spans
}

fn push_code_token(spans: &mut Vec<Span<'static>>, token: &mut String) {
    if token.is_empty() {
        return;
    }
    let color = if is_keyword(token) {
        Color::Cyan
    } else if token.chars().all(|character| character.is_ascii_digit()) {
        Color::Magenta
    } else {
        Color::White
    };
    let style = if is_keyword(token) {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn markdown_preserves_structure_without_showing_delimiters() {
        let lines = markdown_lines("# Result\n\n- **one**\n- `two`");
        assert_eq!(text(&lines), "Result\n• one\n• two");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn fenced_code_and_diff_lines_have_distinct_styles() {
        let lines = markdown_lines("```rust\nlet value = 1;\n```");
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
}
