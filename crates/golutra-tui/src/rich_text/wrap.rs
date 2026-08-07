//! Styled, grapheme-safe wrapping by terminal display width.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{
    model::{RichText, TextRun},
    theme,
};

#[derive(Clone)]
struct StyledGrapheme {
    text: String,
    style: Style,
    width: usize,
    whitespace: bool,
}

struct Token {
    graphemes: Vec<StyledGrapheme>,
    whitespace: bool,
    width: usize,
}

pub(super) fn wrap_rich_text(
    text: &RichText,
    width: usize,
    base_style: Style,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    if text.lines.is_empty() {
        return vec![Line::default()];
    }
    text.lines
        .iter()
        .flat_map(|line| wrap_spans(&styled_spans(line, base_style), width))
        .collect()
}

fn styled_spans(runs: &[TextRun], base_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for run in runs {
        push_span(
            &mut spans,
            Span::styled(run.text.clone(), theme::inline(base_style, run.style)),
        );
    }
    spans
}

pub(super) fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let tokens = tokens(spans);
    if tokens.is_empty() {
        return vec![Line::default()];
    }

    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0_usize;
    let mut pending_space = Vec::new();
    let mut pending_width = 0_usize;

    for token in tokens {
        if token.whitespace {
            pending_width = pending_width.saturating_add(token.width);
            pending_space.extend(token.graphemes);
            continue;
        }

        if current_width > 0
            && current_width
                .saturating_add(pending_width)
                .saturating_add(token.width)
                > width
        {
            finish_line(&mut lines, &mut current, &mut current_width);
            pending_space.clear();
            pending_width = 0;
        } else if current_width > 0 {
            for grapheme in pending_space.drain(..) {
                push_grapheme(&mut current, grapheme);
            }
            current_width = current_width.saturating_add(pending_width);
            pending_width = 0;
        } else {
            pending_space.clear();
            pending_width = 0;
        }

        if token.width <= width.saturating_sub(current_width) {
            current_width = current_width.saturating_add(token.width);
            for grapheme in token.graphemes {
                push_grapheme(&mut current, grapheme);
            }
            continue;
        }

        if current_width > 0 {
            finish_line(&mut lines, &mut current, &mut current_width);
        }
        let mut graphemes = token.graphemes.into_iter().peekable();
        while let Some(grapheme) = graphemes.next() {
            if current_width > 0 && current_width.saturating_add(grapheme.width) > width {
                finish_line(&mut lines, &mut current, &mut current_width);
            }
            if current_width > 0
                && current_width.saturating_add(grapheme.width) <= width
                && graphemes.peek().is_some_and(|next| {
                    prohibits_line_start(next)
                        && current_width
                            .saturating_add(grapheme.width)
                            .saturating_add(next.width)
                            > width
                })
            {
                finish_line(&mut lines, &mut current, &mut current_width);
            }
            current_width = current_width.saturating_add(grapheme.width);
            push_grapheme(&mut current, grapheme);
            if current_width >= width {
                finish_line(&mut lines, &mut current, &mut current_width);
            }
        }
    }

    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

pub(super) fn hard_wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let graphemes = styled_graphemes(spans);
    if graphemes.is_empty() {
        return vec![Line::default()];
    }

    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0_usize;
    for grapheme in graphemes {
        if current_width > 0 && current_width.saturating_add(grapheme.width) > width {
            finish_line(&mut lines, &mut current, &mut current_width);
        }
        current_width = current_width.saturating_add(grapheme.width);
        push_grapheme(&mut current, grapheme);
        if current_width >= width {
            finish_line(&mut lines, &mut current, &mut current_width);
        }
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

pub(super) fn prefix_lines(
    lines: Vec<Line<'static>>,
    first_prefix: Vec<Span<'static>>,
    subsequent_prefix: Vec<Span<'static>>,
) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let mut spans = if index == 0 {
                first_prefix.clone()
            } else {
                subsequent_prefix.clone()
            };
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

pub(super) fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

pub(super) fn push_span(spans: &mut Vec<Span<'static>>, span: Span<'static>) {
    if span.content.is_empty() {
        return;
    }
    if let Some(previous) = spans.last_mut()
        && previous.style == span.style
    {
        previous.content.to_mut().push_str(span.content.as_ref());
    } else {
        spans.push(span);
    }
}

fn tokens(spans: &[Span<'static>]) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    for grapheme in styled_graphemes(spans) {
        match tokens.last_mut() {
            Some(token) if token.whitespace == grapheme.whitespace => {
                token.width = token.width.saturating_add(grapheme.width);
                token.graphemes.push(grapheme);
            }
            _ => tokens.push(Token {
                width: grapheme.width,
                whitespace: grapheme.whitespace,
                graphemes: vec![grapheme],
            }),
        }
    }
    tokens
}

fn styled_graphemes(spans: &[Span<'static>]) -> Vec<StyledGrapheme> {
    spans
        .iter()
        .flat_map(|span| {
            span.content
                .graphemes(true)
                .map(|grapheme| StyledGrapheme {
                    text: grapheme.to_owned(),
                    style: span.style,
                    width: UnicodeWidthStr::width(grapheme),
                    whitespace: grapheme.chars().all(char::is_whitespace),
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn push_grapheme(spans: &mut Vec<Span<'static>>, grapheme: StyledGrapheme) {
    push_span(spans, Span::styled(grapheme.text, grapheme.style));
}

fn prohibits_line_start(grapheme: &StyledGrapheme) -> bool {
    grapheme.text.chars().next().is_some_and(|character| {
        matches!(
            character,
            ',' | '.'
                | '!'
                | '?'
                | ';'
                | ':'
                | '%'
                | ')'
                | ']'
                | '}'
                | '、'
                | '。'
                | '，'
                | '．'
                | '！'
                | '？'
                | '；'
                | '：'
                | '％'
                | '）'
                | '］'
                | '｝'
                | '》'
                | '〉'
                | '】'
                | '〕'
                | '」'
                | '』'
                | '〗'
                | '〙'
                | '〛'
                | '’'
                | '”'
                | '…'
        )
    })
}

fn finish_line(
    lines: &mut Vec<Line<'static>>,
    current: &mut Vec<Span<'static>>,
    current_width: &mut usize,
) {
    lines.push(Line::from(std::mem::take(current)));
    *current_width = 0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn plain(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn wrapping_preserves_styles_across_word_boundaries() {
        let spans = vec![
            Span::styled("alpha beta ", Style::default().fg(Color::White)),
            Span::styled("gamma", Style::default().fg(Color::Cyan)),
        ];

        let lines = wrap_spans(&spans, 10);

        assert_eq!(plain(&lines), ["alpha beta", "gamma"]);
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Cyan));
    }

    #[test]
    fn wrapping_splits_cjk_on_grapheme_boundaries() {
        let spans = vec![Span::raw("中文宽度换行")];
        let lines = wrap_spans(&spans, 6);

        assert_eq!(plain(&lines), ["中文宽", "度换行"]);
        assert!(lines.iter().all(|line| line_width(line) <= 6));
    }

    #[test]
    fn wrapping_keeps_closing_punctuation_off_the_next_line_when_possible() {
        let chinese = wrap_spans(&[Span::raw("稳定换行。")], 8);
        assert_eq!(plain(&chinese), ["稳定换", "行。"]);

        let url = wrap_spans(&[Span::raw("example.com")], 7);
        assert_eq!(plain(&url), ["exampl", "e.com"]);
    }
}
