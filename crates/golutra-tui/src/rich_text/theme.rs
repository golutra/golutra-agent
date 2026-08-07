//! Central mapping from Markdown semantics to the restrained terminal style hierarchy.

use pulldown_cmark::HeadingLevel;
use ratatui::style::{Color, Modifier, Style};

use super::model::{InlineStyle, InlineTone};

pub(super) fn body() -> Style {
    Style::default().fg(Color::White)
}

pub(super) fn heading(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 => Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        HeadingLevel::H2 => Style::default().add_modifier(Modifier::BOLD),
        HeadingLevel::H3 => Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC),
        HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => {
            Style::default().add_modifier(Modifier::ITALIC)
        }
    }
}

pub(super) fn quote() -> Style {
    Style::default().fg(Color::Green)
}

pub(super) fn muted() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub(super) fn accent() -> Style {
    Style::default().fg(Color::Cyan)
}

pub(super) fn ordered_marker() -> Style {
    Style::default().fg(Color::LightBlue)
}

pub(super) fn table_header() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub(super) fn detail_heading() -> Style {
    muted().add_modifier(Modifier::BOLD)
}

pub(super) fn diff_metadata() -> Style {
    accent()
}

pub(super) fn diff_hunk() -> Style {
    Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD)
}

pub(super) fn diff_addition() -> Style {
    Style::default().fg(Color::Green)
}

pub(super) fn diff_deletion() -> Style {
    Style::default().fg(Color::Red)
}

pub(super) fn code_comment() -> Style {
    muted()
}

pub(super) fn code_string() -> Style {
    Style::default().fg(Color::Green)
}

pub(super) fn code_punctuation() -> Style {
    Style::default().fg(Color::Gray)
}

pub(super) fn code_keyword() -> Style {
    accent().add_modifier(Modifier::BOLD)
}

pub(super) fn code_number() -> Style {
    Style::default().fg(Color::Magenta)
}

pub(super) fn inline(base: Style, semantic: InlineStyle) -> Style {
    let mut style = match semantic.tone {
        None => base,
        Some(InlineTone::Code) | Some(InlineTone::Link) => {
            base.patch(Style::default().fg(Color::Cyan))
        }
        Some(InlineTone::Image) => base.patch(Style::default().fg(Color::Magenta)),
        Some(InlineTone::CheckedTask) => base.patch(Style::default().fg(Color::Green)),
        Some(InlineTone::UncheckedTask) => base.patch(muted()),
        Some(InlineTone::Superscript) => base.patch(Style::default().fg(Color::LightBlue)),
        Some(InlineTone::Subscript) => base.patch(Style::default().fg(Color::LightMagenta)),
    };
    if matches!(semantic.tone, Some(InlineTone::Link)) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if semantic.emphasis {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if semantic.strong {
        style = style.add_modifier(Modifier::BOLD);
    }
    if semantic.strikethrough {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    style
}
