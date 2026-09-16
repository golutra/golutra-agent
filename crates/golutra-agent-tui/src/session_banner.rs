//! Scrollable session introduction.

use super::*;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

const SESSION_PANEL_MAX_WIDTH: usize = 60;
const SESSION_PANEL_MIN_WIDTH: usize = 40;
const SESSION_OUTER_MARGIN: usize = 2;
const SESSION_LOGO_GAP: usize = 3;
const SESSION_FIELD_LABEL_WIDTH: usize = 7;
const GOLUTRA_AGENT_LOGO_GLYPHS: [[&str; 6]; 7] = [
    [
        " ██████╗",
        "██╔════╝",
        "██║  ███╗",
        "██║   ██║",
        "╚██████╔╝",
        " ╚═════╝",
    ],
    [
        " ██████╗",
        "██╔═══██╗",
        "██║   ██║",
        "██║   ██║",
        "╚██████╔╝",
        " ╚═════╝",
    ],
    ["██╗", "██║", "██║", "██║", "███████╗", "╚══════╝"],
    [
        "██╗   ██╗",
        "██║   ██║",
        "██║   ██║",
        "██║   ██║",
        "╚██████╔╝",
        " ╚═════╝",
    ],
    [
        "████████╗",
        "╚══██╔══╝",
        "   ██║",
        "   ██║",
        "   ██║",
        "   ╚═╝",
    ],
    [
        "██████╗",
        "██╔══██╗",
        "██████╔╝",
        "██╔══██╗",
        "██║  ██║",
        "╚═╝  ╚═╝",
    ],
    [
        " █████╗",
        "██╔══██╗",
        "███████║",
        "██╔══██║",
        "██║  ██║",
        "╚═╝  ╚═╝",
    ],
];
const SESSION_LOGO_GRADIENT: [[u8; 3]; 3] =
    [[0x0E, 0xA5, 0xE9], [0x10, 0xB9, 0x81], [0xF5, 0x9E, 0x0B]];
pub(crate) fn session_history_lines(app: &TuiApp, width: u16) -> Vec<Line<'static>> {
    let palette = app.palette();
    let available = usize::from(width);
    if available < 8 {
        return vec![Line::from(Span::styled(
            truncate_end_to_width("GOLUTRA", available),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))];
    }

    let margin_width = if available >= 16 {
        SESSION_OUTER_MARGIN
    } else {
        0
    };
    let usable_width = available.saturating_sub(margin_width.saturating_mul(2));
    let logo_width = session_logo_width();
    let show_logo = !app.preferences.screen_reader
        && usable_width
            >= logo_width
                .saturating_add(SESSION_LOGO_GAP)
                .saturating_add(SESSION_PANEL_MIN_WIDTH);
    let panel_width = if show_logo {
        usable_width
            .saturating_sub(logo_width)
            .saturating_sub(SESSION_LOGO_GAP)
            .min(SESSION_PANEL_MAX_WIDTH)
    } else {
        usable_width.min(SESSION_PANEL_MAX_WIDTH)
    };

    let model = app.runtime_controls.effective_model().trim();
    let model = if model.is_empty() {
        "unconfigured"
    } else {
        model
    };
    let directory = workspace_path_label(&app.workspace_path);
    let panel = session_panel_lines(app, panel_width, model, &directory);
    let mut lines = if show_logo {
        let gradient =
            app.preferences.theme != ColorTheme::Monochrome && !app.preferences.high_contrast;
        combine_session_logo_and_panel(
            session_logo_lines(palette, gradient),
            panel,
            margin_width,
            logo_width,
        )
    } else {
        panel
            .into_iter()
            .map(|line| prepend_session_margin(line, margin_width))
            .collect()
    };

    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("  Tip:", Style::default().fg(palette.accent)),
        Span::styled(
            truncate_end_to_width(
                " Use /help to view commands and interaction options.",
                available.saturating_sub(6),
            ),
            Style::default().fg(palette.muted),
        ),
    ]));
    // 页首提示与第一条消息共用一行分隔，后续提交不能再叠加留白。
    lines.push(Line::default());
    lines
}

fn session_logo_width() -> usize {
    GOLUTRA_AGENT_LOGO_GLYPHS
        .iter()
        .map(|glyph| {
            glyph
                .iter()
                .map(|row| display_width(row))
                .max()
                .unwrap_or(0)
        })
        .sum::<usize>()
        .saturating_add(GOLUTRA_AGENT_LOGO_GLYPHS.len().saturating_sub(1))
}

fn session_logo_lines(palette: TuiPalette, gradient: bool) -> Vec<Line<'static>> {
    let logo_width = session_logo_width();
    (0..GOLUTRA_AGENT_LOGO_GLYPHS[0].len())
        .map(|row| {
            let mut text = String::with_capacity(logo_width);
            for (index, glyph) in GOLUTRA_AGENT_LOGO_GLYPHS.iter().enumerate() {
                if index > 0 {
                    text.push(' ');
                }
                let glyph_width = glyph
                    .iter()
                    .map(|line| display_width(line))
                    .max()
                    .unwrap_or(0);
                text.push_str(glyph[row]);
                text.push_str(&" ".repeat(glyph_width.saturating_sub(display_width(glyph[row]))));
            }
            session_logo_line(text, palette, gradient, logo_width)
        })
        .collect()
}

fn session_logo_line(
    text: String,
    palette: TuiPalette,
    gradient: bool,
    logo_width: usize,
) -> Line<'static> {
    let style = Style::default().add_modifier(Modifier::BOLD);
    if !gradient {
        return Line::from(Span::styled(text, style.fg(palette.accent)));
    }

    Line::from(
        text.chars()
            .enumerate()
            .map(|(column, character)| {
                if character == ' ' {
                    Span::raw(" ")
                } else {
                    Span::styled(
                        character.to_string(),
                        style.fg(session_logo_gradient_color(column, logo_width)),
                    )
                }
            })
            .collect::<Vec<_>>(),
    )
}

fn session_logo_gradient_color(column: usize, width: usize) -> Color {
    let last = SESSION_LOGO_GRADIENT.len().saturating_sub(1);
    let denominator = width.saturating_sub(1);
    if denominator == 0 || column >= denominator {
        let [red, green, blue] = SESSION_LOGO_GRADIENT[last];
        return Color::Rgb(red, green, blue);
    }

    let scaled = column.saturating_mul(last);
    let segment = scaled / denominator;
    let numerator = scaled % denominator;
    let start = SESSION_LOGO_GRADIENT[segment];
    let end = SESSION_LOGO_GRADIENT[segment.saturating_add(1)];
    Color::Rgb(
        interpolate_logo_channel(start[0], end[0], numerator, denominator),
        interpolate_logo_channel(start[1], end[1], numerator, denominator),
        interpolate_logo_channel(start[2], end[2], numerator, denominator),
    )
}

fn interpolate_logo_channel(start: u8, end: u8, numerator: usize, denominator: usize) -> u8 {
    let start = usize::from(start);
    let end = usize::from(end);
    let value = start
        .saturating_mul(denominator.saturating_sub(numerator))
        .saturating_add(end.saturating_mul(numerator))
        .saturating_add(denominator / 2)
        / denominator;
    u8::try_from(value).unwrap_or(u8::MAX)
}

fn session_panel_lines(
    app: &TuiApp,
    panel_width: usize,
    model: &str,
    directory: &str,
) -> Vec<Line<'static>> {
    let palette = app.palette();
    let border_style = Style::default().fg(palette.subtle);
    let content_width = panel_width.saturating_sub(4);
    let value_width = content_width.saturating_sub(SESSION_FIELD_LABEL_WIDTH);
    let model_hint = "  /model";
    let show_model_hint =
        display_width(model).saturating_add(display_width(model_hint)) <= value_width;
    let model = if show_model_hint {
        model.to_owned()
    } else {
        truncate_end_to_width(model, value_width)
    };
    let directory = truncate_start_to_width(directory, value_width);
    let (guard, guard_style) = match app.runtime_controls.permission_mode {
        PermissionMode::Unrestricted => (
            "unrestricted",
            Style::default()
                .fg(palette.warning)
                .add_modifier(Modifier::BOLD),
        ),
        PermissionMode::Guarded => (
            "guarded",
            Style::default()
                .fg(palette.success)
                .add_modifier(Modifier::BOLD),
        ),
    };

    let mut engine_spans = vec![session_field_label("engine", palette)];
    engine_spans.push(Span::styled(model, Style::default().fg(palette.text)));
    if show_model_hint {
        engine_spans.push(Span::styled(
            model_hint.to_owned(),
            Style::default().fg(palette.accent),
        ));
    }

    vec![
        Line::from(Span::styled(
            format!("╭{}╮", "─".repeat(panel_width.saturating_sub(2))),
            border_style,
        )),
        session_panel_row(
            vec![
                Span::styled(
                    "GOLUTRA",
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  v{}", env!("CARGO_PKG_VERSION")),
                    Style::default().fg(palette.muted),
                ),
            ],
            content_width,
            border_style,
        ),
        session_panel_row(engine_spans, content_width, border_style),
        session_panel_row(
            vec![
                session_field_label("scope", palette),
                Span::styled(directory, Style::default().fg(palette.text)),
            ],
            content_width,
            border_style,
        ),
        session_panel_row(
            vec![
                session_field_label("guard", palette),
                Span::styled(guard, guard_style),
            ],
            content_width,
            border_style,
        ),
        Line::from(Span::styled(
            format!("╰{}╯", "─".repeat(panel_width.saturating_sub(2))),
            border_style,
        )),
    ]
}

fn session_field_label(label: &str, palette: TuiPalette) -> Span<'static> {
    Span::styled(
        format!("{label:<SESSION_FIELD_LABEL_WIDTH$}"),
        Style::default().fg(palette.muted),
    )
}

fn session_panel_row(
    spans: Vec<Span<'static>>,
    content_width: usize,
    border_style: Style,
) -> Line<'static> {
    let mut fitted = Vec::new();
    let mut remaining = content_width;
    for span in spans {
        if remaining == 0 {
            break;
        }
        let style = span.style;
        let content = span.content.into_owned();
        let content_width = display_width(&content);
        if content_width <= remaining {
            fitted.push(Span::styled(content, style));
            remaining = remaining.saturating_sub(content_width);
        } else {
            let content = truncate_end_to_width(&content, remaining);
            remaining = remaining.saturating_sub(display_width(&content));
            fitted.push(Span::styled(content, style));
            break;
        }
    }
    fitted.push(Span::raw(" ".repeat(remaining)));

    let mut row = vec![Span::styled("│ ", border_style)];
    row.extend(fitted);
    row.push(Span::styled(" │", border_style));
    Line::from(row)
}

fn combine_session_logo_and_panel(
    logo: Vec<Line<'static>>,
    panel: Vec<Line<'static>>,
    margin_width: usize,
    logo_width: usize,
) -> Vec<Line<'static>> {
    let logo_offset = panel.len().saturating_sub(logo.len()) / 2;
    panel
        .into_iter()
        .enumerate()
        .map(|(row, panel_line)| {
            let mut spans = vec![Span::raw(" ".repeat(margin_width))];
            if let Some(logo_line) = row.checked_sub(logo_offset).and_then(|row| logo.get(row)) {
                spans.extend(logo_line.spans.iter().cloned());
            } else {
                spans.push(Span::raw(" ".repeat(logo_width)));
            }
            spans.push(Span::raw(" ".repeat(SESSION_LOGO_GAP)));
            spans.extend(panel_line.spans);
            Line::from(spans)
        })
        .collect()
}

fn prepend_session_margin(line: Line<'static>, margin_width: usize) -> Line<'static> {
    let mut spans = vec![Span::raw(" ".repeat(margin_width))];
    spans.extend(line.spans);
    Line::from(spans)
}
