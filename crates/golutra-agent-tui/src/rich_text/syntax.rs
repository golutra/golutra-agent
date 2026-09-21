use std::{cell::RefCell, collections::VecDeque, sync::OnceLock};

use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, HighlightState, ThemeSet},
    parsing::{ParseState, SyntaxSet},
};

const MAX_SOURCE: usize = 256 * 1024;
static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
static THEMES: OnceLock<ThemeSet> = OnceLock::new();

struct Snapshot {
    language: String,
    light: bool,
    source: String,
    lines: Vec<Vec<Span<'static>>>,
    state: (HighlightState, ParseState),
}

thread_local! {
    static CACHE: RefCell<VecDeque<Snapshot>> = const { RefCell::new(VecDeque::new()) };
}

pub(super) fn highlight(source: &str, language: Option<&str>) -> Vec<Vec<Span<'static>>> {
    let language = language.unwrap_or("");
    if language.eq_ignore_ascii_case("diff") {
        return source
            .lines()
            .map(|line| super::code::detail_line(line).spans)
            .collect();
    }
    let plain = || {
        source
            .split_terminator('\n')
            .map(|line| vec![Span::raw(line.to_owned())])
            .collect()
    };
    if language.is_empty()
        || source.len() > MAX_SOURCE
        || source.lines().any(|line| line.len() > 16 * 1024)
    {
        return plain();
    }
    let syntaxes = SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines);
    let Some(syntax) = syntaxes.find_syntax_by_token(language) else {
        return plain();
    };
    let light = crate::terminal_appearance::current().light;
    let themes = THEMES.get_or_init(ThemeSet::load_defaults);
    let theme = &themes.themes[if light {
        "InspiredGitHub"
    } else {
        "base16-ocean.dark"
    }];
    CACHE.with_borrow_mut(|cache| {
        let ancestor = cache
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.language == language
                    && entry.light == light
                    && source.starts_with(&entry.source)
            })
            .max_by_key(|(_, entry)| entry.source.len())
            .map(|(index, _)| index);
        let (mut highlighter, mut lines, offset) =
            match ancestor.and_then(|index| cache.remove(index)) {
                Some(entry) => (
                    HighlightLines::from_state(theme, entry.state.0, entry.state.1),
                    entry.lines,
                    entry.source.len(),
                ),
                None => (HighlightLines::new(syntax, theme), Vec::new(), 0),
            };
        let stable_end = source.rfind('\n').map_or(0, |index| index + 1);
        for line in source[offset..stable_end].split_inclusive('\n') {
            lines.push(highlight_line(&mut highlighter, line, syntaxes));
        }
        let state = highlighter.state();
        let mut rendered = lines.clone();
        if stable_end < source.len() {
            let mut tail = HighlightLines::from_state(theme, state.0.clone(), state.1.clone());
            rendered.push(highlight_line(&mut tail, &source[stable_end..], syntaxes));
        }
        cache.push_back(Snapshot {
            language: language.to_owned(),
            light,
            source: source[..stable_end].to_owned(),
            lines,
            state,
        });
        while cache.len() > 8
            || cache.iter().map(|entry| entry.source.len()).sum::<usize>() > MAX_SOURCE
        {
            cache.pop_front();
        }
        rendered
    })
}

fn highlight_line(
    highlighter: &mut HighlightLines<'_>,
    line: &str,
    syntaxes: &SyntaxSet,
) -> Vec<Span<'static>> {
    let Ok(tokens) = highlighter.highlight_line(line, syntaxes) else {
        return vec![Span::raw(line.trim_end_matches('\n').to_owned())];
    };
    tokens
        .into_iter()
        .filter_map(|(token, text)| {
            let text = text.trim_end_matches('\n');
            if text.is_empty() {
                return None;
            }
            let mut style = Style::default().fg(Color::Rgb(
                token.foreground.r,
                token.foreground.g,
                token.foreground.b,
            ));
            for (flag, modifier) in [
                (FontStyle::BOLD, Modifier::BOLD),
                (FontStyle::ITALIC, Modifier::ITALIC),
                (FontStyle::UNDERLINE, Modifier::UNDERLINED),
            ] {
                if token.font_style.contains(flag) {
                    style = style.add_modifier(modifier);
                }
            }
            Some(Span::styled(text.to_owned(), style))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_multiline_syntax_matches_canonical_rendering() {
        for (language, source) in [
            (
                "rust",
                "/* start\ncomment */\n#[derive(Debug)]\nstruct 中文;\n",
            ),
            ("python", "s = \"\"\"hello\nworld\"\"\"\nprint(s)\n"),
        ] {
            CACHE.with_borrow_mut(|cache| cache.clear());
            let expected = highlight(source, Some(language));
            CACHE.with_borrow_mut(|cache| cache.clear());
            for (index, character) in source.char_indices() {
                highlight(&source[..index + character.len_utf8()], Some(language));
            }
            assert_eq!(highlight(source, Some(language)), expected);
            assert_eq!(
                expected
                    .iter()
                    .map(|line| line
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n"),
                source.trim_end_matches('\n')
            );
        }
    }

    #[test]
    fn multiline_comment_state_is_not_reset_at_newline() {
        let lines = highlight("/* start\nstill comment\n*/\nlet value = 1;", Some("rust"));
        assert_eq!(lines[0][0].style, lines[1][0].style);
        assert_ne!(lines[1][0].style, lines[3][0].style);
    }
}
