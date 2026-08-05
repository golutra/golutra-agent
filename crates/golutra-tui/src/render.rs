//! TUI 的布局与 transcript 投影。

use crossterm::terminal::size;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};

use super::*;

const COMPOSER_PREFIX: &str = "› ";
const COMPOSER_PREFIX_WIDTH: u16 = 2;
const MAX_COMPOSER_ROWS: u16 = 5;
const QUESTION_FREE_TEXT_PREFIX: &str = "    ";
const QUESTION_FREE_TEXT_PREFIX_WIDTH: u16 = 4;
const MAX_QUESTION_FREE_TEXT_ROWS: u16 = 4;
const SETTINGS_MODEL_PREFIX_WIDTH: u16 = 9;
pub(crate) fn debug_pane_widths(width: u16) -> (u16, u16) {
    let transcript = width / 2;
    let developer = width.saturating_sub(transcript);
    (transcript, developer)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum BodyLayoutMode {
    #[default]
    Transcript,
    Developer,
    ResponseAndDeveloper,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct UiLayoutSnapshot {
    pub(crate) body: Rect,
    pub(crate) transcript: Rect,
    pub(crate) developer: Option<Rect>,
    pub(crate) bottom: Rect,
    pub(crate) body_mode: BodyLayoutMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiHitTarget {
    Transcript,
    Developer,
    Bottom,
    Overlay,
    None,
}

impl UiLayoutSnapshot {
    pub(crate) fn hit_test(self, x: u16, y: u16, app: &TuiApp) -> UiHitTarget {
        if app.overlay_surface().is_some() {
            if rect_contains(self.transcript, x, y) || rect_contains(self.bottom, x, y) {
                return UiHitTarget::Overlay;
            }
            return UiHitTarget::None;
        }
        if self.developer.is_some_and(|area| rect_contains(area, x, y)) {
            UiHitTarget::Developer
        } else if rect_contains(self.transcript, x, y) {
            UiHitTarget::Transcript
        } else if rect_contains(self.bottom, x, y) {
            UiHitTarget::Bottom
        } else {
            UiHitTarget::None
        }
    }
}

fn rect_contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

pub(crate) fn ui_layout(area: Rect, app: &TuiApp) -> UiLayoutSnapshot {
    let bottom_height = bottom_pane_height_for_width(app, area.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(bottom_height)])
        .split(area);
    let overlay_visible = app.overlay_surface().is_some();
    let body_mode = if overlay_visible || !app.debug_mode {
        BodyLayoutMode::Transcript
    } else {
        match app.body_view_mode {
            BodyViewMode::Transcript => BodyLayoutMode::Transcript,
            BodyViewMode::Developer => BodyLayoutMode::Developer,
            BodyViewMode::Auto | BodyViewMode::Split => BodyLayoutMode::ResponseAndDeveloper,
        }
    };
    let (transcript, developer) = match body_mode {
        BodyLayoutMode::ResponseAndDeveloper => {
            let (transcript_width, developer_width) = debug_pane_widths(chunks[0].width);
            let transcript =
                Rect::new(chunks[0].x, chunks[0].y, transcript_width, chunks[0].height);
            let developer = Rect::new(
                transcript.right(),
                chunks[0].y,
                developer_width,
                chunks[0].height,
            );
            (transcript, Some(developer))
        }
        BodyLayoutMode::Developer => (Rect::default(), Some(chunks[0])),
        BodyLayoutMode::Transcript => (chunks[0], None),
    };
    UiLayoutSnapshot {
        body: chunks[0],
        transcript,
        developer,
        bottom: chunks[1],
        body_mode,
    }
}

pub(crate) fn draw_ui(frame: &mut Frame<'_>, app: &mut TuiApp) {
    let next_layout = ui_layout(frame.area(), app);
    if next_layout.body_mode == BodyLayoutMode::Transcript {
        app.ensure_transcript_layout(next_layout.transcript);
        let row_count = app
            .transcript_layout_cache
            .as_ref()
            .map_or(0, |cache| cache.layout.row_count);
        app.sync_transcript_row_count_to(app.transcript_scroll.row_count, row_count);
    }
    app.layout = next_layout;
    let layout = app.layout;
    match layout.body_mode {
        BodyLayoutMode::Transcript => draw_transcript(frame, layout.transcript, app),
        BodyLayoutMode::Developer => {
            draw_developer_panel(frame, layout.developer.expect("developer layout"), app);
        }
        BodyLayoutMode::ResponseAndDeveloper => {
            draw_debug_timeline(frame, layout.body, app);
        }
    }
    draw_bottom_pane(frame, layout.bottom, app);
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn bottom_pane_height(app: &TuiApp) -> u16 {
    let width = size().map(|(width, _)| width).unwrap_or(80);
    bottom_pane_height_for_width(app, width)
}

pub(crate) fn bottom_pane_height_for_width(app: &TuiApp, width: u16) -> u16 {
    let composer_suppressed = app.overlay_surface().is_some()
        || app.transcript_search.is_some()
        || app.history_search.is_some();
    let mention_rows = if composer_suppressed {
        0
    } else {
        app.mention_completion
            .as_ref()
            .map_or(0, |completion| completion.candidates.len().min(6)) as u16
    };
    let slash_rows = if mention_rows > 0 {
        0
    } else {
        app.slash_candidates().len() as u16
    };
    let queued_rows = if composer_suppressed {
        0
    } else {
        queued_prompts(&app.events).len().min(3) as u16
    };
    let attachment_rows = u16::from(!composer_suppressed && !app.attachments.is_empty());
    let overlay_rows = u16::from(app.overlay_surface().is_some());
    let provider_rows = u16::from(provider_footer_line(app).is_some());
    let activity_rows = u16::from(live_status_text(app, usize::from(width)).is_some());
    let composer_rows = if app.transcript_search.is_some()
        || app.history_search.is_some()
        || app.overlay_surface().is_some()
    {
        1
    } else {
        app.input
            .viewport(
                width.saturating_sub(COMPOSER_PREFIX_WIDTH).max(1),
                MAX_COMPOSER_ROWS,
            )
            .lines
            .len()
            .try_into()
            .unwrap_or(MAX_COMPOSER_ROWS)
    };
    2 + composer_rows
        + mention_rows
        + slash_rows
        + queued_rows
        + attachment_rows
        + overlay_rows
        + provider_rows
        + activity_rows
}

pub(crate) fn draw_transcript(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    if let Some(surface) = app.overlay_surface() {
        match surface {
            OverlaySurface::Help => draw_help_dialog(
                frame,
                area,
                app.help_dialog.as_ref().expect("help surface"),
                app,
            ),
            OverlaySurface::Auth => draw_auth_dialog(
                frame,
                area,
                app.auth_dialog.as_ref().expect("auth surface"),
                app,
            ),
            OverlaySurface::Approval => draw_approval_dialog(
                frame,
                area,
                app.approval_dialog.as_ref().expect("approval surface"),
                app,
            ),
            OverlaySurface::Question => draw_question_dialog(
                frame,
                area,
                app.question_dialog.as_ref().expect("question surface"),
                app,
            ),
            OverlaySurface::Resume => draw_resume_picker(
                frame,
                area,
                app.resume_picker.as_ref().expect("resume surface"),
                app.thread_id,
                app,
            ),
            OverlaySurface::Queue => draw_queue_picker(
                frame,
                area,
                app.queue_picker.as_ref().expect("queue surface"),
                app,
            ),
            OverlaySurface::Dashboard => draw_dashboard(
                frame,
                area,
                app.dashboard.as_ref().expect("dashboard surface"),
                &app.events,
                app,
            ),
            OverlaySurface::Settings => draw_settings_dialog(
                frame,
                area,
                app.settings_dialog.as_ref().expect("settings surface"),
                app,
            ),
            OverlaySurface::Export => draw_export_flow(
                frame,
                area,
                app.export_flow.as_ref().expect("export surface"),
                app.thread_id,
                app,
            ),
        }
        return;
    }

    let layout = &app
        .transcript_layout_cache
        .as_ref()
        .expect("transcript layout is prepared before drawing")
        .layout;
    let visible_rows = area.height as usize;
    let top_padding = transcript_top_padding(app, layout, area);
    let window = layout.visible_window(
        visible_rows,
        app.transcript_scroll.offset_from_bottom,
        app.transcript_top_row_override,
    );
    let palette = app.palette();
    let (logical_window, local_scroll) = layout.logical_window(window);
    let logical_start = logical_window.start;
    let mut lines = if app.transcript_presentation == TranscriptPresentation::Raw {
        layout.lines[logical_window.clone()]
            .iter()
            .map(|line| {
                Line::from(
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>(),
                )
            })
            .collect::<Vec<_>>()
    } else {
        layout.lines[logical_window].to_vec()
    };
    if let Some(search) = &app.transcript_search {
        for (match_index, line_index) in search.matches.iter().copied().enumerate() {
            if let Some(line) = line_index
                .checked_sub(logical_start)
                .and_then(|line_index| lines.get_mut(line_index))
            {
                let style = if match_index == search.selected {
                    Style::default()
                        .fg(palette.selected_foreground)
                        .bg(palette.warning)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(palette.warning)
                };
                for span in &mut line.spans {
                    span.style = span.style.patch(style);
                }
            }
        }
    }
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((u16::try_from(local_scroll).unwrap_or(u16::MAX), 0));
    let content_area = Rect::new(
        area.x,
        area.y.saturating_add(top_padding),
        area.width,
        area.height.saturating_sub(top_padding),
    );
    frame.render_widget(paragraph, content_area);
}

pub(crate) fn draw_auth_dialog(
    frame: &mut Frame<'_>,
    area: Rect,
    dialog: &AuthDialogState,
    app: &TuiApp,
) {
    let mut lines = auth_dialog_lines(dialog);
    apply_palette_to_lines(&mut lines, app.palette());
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title("Provider setup")
                .borders(Borders::TOP),
        )
        .wrap(Wrap { trim: false })
        .scroll((auth_scroll_offset(dialog, area), 0));
    frame.render_widget(paragraph, area);
}

fn auth_dialog_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    match dialog.step {
        AuthDialogStep::GroupChoice => auth_group_lines(dialog),
        AuthDialogStep::ThirdPartyChoice => auth_third_party_lines(dialog),
        AuthDialogStep::AuthMethod => auth_method_lines(dialog),
        AuthDialogStep::Protocol => auth_protocol_lines(dialog),
        AuthDialogStep::BaseUrl => auth_input_lines(
            &auth_step_title(dialog),
            "Base URL",
            "endpoint URL for the selected protocol",
            &dialog.base_url,
            dialog.error.as_deref(),
            false,
        ),
        AuthDialogStep::CredentialStore => auth_credential_store_lines(dialog),
        AuthDialogStep::ApiKey => auth_input_lines(
            &auth_step_title(dialog),
            "API key",
            "stored in $GOLUTRA_HOME/credentials.json",
            &dialog.api_key,
            dialog.error.as_deref(),
            true,
        ),
        AuthDialogStep::EnvKey => auth_input_lines(
            &auth_step_title(dialog),
            "Environment variable",
            "for example OPENAI_API_KEY",
            &dialog.api_key_env,
            dialog.error.as_deref(),
            false,
        ),
        AuthDialogStep::Model => auth_model_lines(dialog),
        AuthDialogStep::AdvancedConfig => auth_advanced_config_lines(dialog),
        AuthDialogStep::Review => auth_review_lines(dialog),
    }
}

pub(crate) fn auth_scroll_offset(dialog: &AuthDialogState, area: Rect) -> u16 {
    let visible = usize::from(area.height.saturating_sub(1)).max(1);
    let ranges = auth_visual_ranges(dialog, area.width);
    let max_scroll = auth_scroll_max(dialog, area);
    let selected = if dialog.step == AuthDialogStep::AdvancedConfig {
        dialog.advanced_selected
    } else {
        dialog.selected
    };
    let offset = if dialog.manual_scroll {
        dialog.scroll.min(max_scroll)
    } else {
        ranges
            .iter()
            .find_map(|(index, start, height)| {
                (*index == selected).then_some(scroll_offset_for_range(*start, *height, visible))
            })
            .unwrap_or_else(|| dialog.scroll.min(max_scroll))
    };
    u16::try_from(offset).unwrap_or(u16::MAX)
}

pub(crate) fn auth_scroll_max(dialog: &AuthDialogState, area: Rect) -> usize {
    let visible = usize::from(area.height.saturating_sub(1)).max(1);
    auth_visual_height(dialog, area.width).saturating_sub(visible)
}

fn auth_visual_ranges(dialog: &AuthDialogState, width: u16) -> Vec<(usize, usize, usize)> {
    let lines = auth_dialog_lines(dialog);
    let option_lines = auth_interactive_line_indexes(dialog);
    let mut starts = Vec::with_capacity(lines.len());
    let mut cursor = 0_usize;
    for line in &lines {
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        starts.push(cursor);
        cursor = cursor.saturating_add(wrapped_text_height(&text, width));
    }
    option_lines
        .into_iter()
        .filter_map(|(index, line_index)| {
            let start = *starts.get(line_index)?;
            let end = starts.get(line_index + 1).copied().unwrap_or(cursor);
            Some((index, start, end.saturating_sub(start).max(1)))
        })
        .collect()
}

fn auth_visual_height(dialog: &AuthDialogState, width: u16) -> usize {
    auth_dialog_lines(dialog)
        .iter()
        .map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            wrapped_text_height(&text, width)
        })
        .sum()
}

fn auth_interactive_line_indexes(dialog: &AuthDialogState) -> Vec<(usize, usize)> {
    let (start, count) = match dialog.step {
        AuthDialogStep::GroupChoice => (2_usize, AUTH_GROUP_ITEMS.len()),
        AuthDialogStep::ThirdPartyChoice => (2_usize, THIRD_PARTY_PROVIDER_PRESETS.len()),
        AuthDialogStep::AuthMethod => (2_usize, dialog.auth_method_count()),
        AuthDialogStep::Protocol => (2_usize, dialog.protocol_options().len()),
        AuthDialogStep::CredentialStore => (2_usize, 2),
        AuthDialogStep::Model if !dialog.model_options().is_empty() => {
            (3_usize, dialog.custom_model_index().saturating_add(1))
        }
        AuthDialogStep::AdvancedConfig => (2_usize, AUTH_ADVANCED_ITEMS),
        AuthDialogStep::BaseUrl
        | AuthDialogStep::ApiKey
        | AuthDialogStep::EnvKey
        | AuthDialogStep::Model
        | AuthDialogStep::Review => return Vec::new(),
    };
    (0..count)
        .map(|index| (index, start.saturating_add(index)))
        .collect()
}

fn apply_palette_to_lines(lines: &mut [Line<'static>], palette: TuiPalette) {
    for line in lines {
        for span in &mut line.spans {
            if let Some(color) = span.style.fg {
                span.style.fg = Some(palette.map_color(color));
            }
            if let Some(color) = span.style.bg {
                span.style.bg = Some(palette.map_color(color));
            }
        }
    }
}

pub(crate) fn auth_group_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![Span::styled(
        "Connect a Provider",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.push(Line::from(""));
    lines.extend(
        AUTH_GROUP_ITEMS
            .iter()
            .enumerate()
            .map(|(index, (title, detail))| {
                auth_option_line(index, title, detail, index == dialog.selected)
            }),
    );
    push_auth_error(&mut lines, dialog.error.as_deref());
    lines
}

pub(crate) fn auth_third_party_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![Span::styled(
        "Third-party Providers",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.push(Line::from(""));
    lines.extend(
        THIRD_PARTY_PROVIDER_PRESETS
            .iter()
            .enumerate()
            .map(|(index, preset)| {
                auth_option_line(index, preset.title, preset.detail, index == dialog.selected)
            }),
    );
    push_auth_error(&mut lines, dialog.error.as_deref());
    lines
}

pub(crate) fn auth_method_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let provider = dialog
        .provider
        .map(|provider| provider.title)
        .unwrap_or("Provider");
    let mut lines = vec![Line::from(vec![Span::styled(
        format!("{provider} authentication"),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.push(Line::from(""));
    let methods = dialog.oauth_methods();
    lines.extend(methods.iter().enumerate().map(|(index, method)| {
        let detail = match method.flow {
            OAuthFlow::BrowserPkce => "Open browser and complete PKCE authorization",
            OAuthFlow::DeviceCode => "Open a verification page and enter a device code",
            OAuthFlow::OpenAiDeviceAuth => "Open the ChatGPT device page and enter a code",
        };
        auth_option_line(index, &method.label, detail, index == dialog.selected)
    }));
    if dialog
        .provider
        .is_some_and(|provider| provider.api_key_supported)
    {
        lines.push(auth_option_line(
            methods.len(),
            "API key",
            "Store a key on local disk or reference an environment variable",
            dialog.selected >= methods.len(),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter to select, Up/Down to navigate, Esc to go back",
        Style::default().fg(Color::DarkGray),
    )));
    push_auth_error(&mut lines, dialog.error.as_deref());
    lines
}

pub(crate) fn auth_protocol_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![Span::styled(
        auth_step_title(dialog),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.push(Line::from(""));
    lines.extend(
        dialog
            .protocol_options()
            .iter()
            .enumerate()
            .map(|(index, protocol)| {
                let (title, detail) = protocol_option_text(*protocol);
                auth_option_line(index, title, detail, index == dialog.selected)
            }),
    );
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter to select, ↑↓ to navigate, Esc to go back",
        Style::default().fg(Color::DarkGray),
    )));
    push_auth_error(&mut lines, dialog.error.as_deref());
    lines
}

pub(crate) fn auth_credential_store_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![Span::styled(
        auth_step_title(dialog),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.push(Line::from(""));
    lines.push(auth_option_line(
        0,
        "Local disk",
        "Store in $GOLUTRA_HOME/credentials.json (owner-only)",
        dialog.selected == 0,
    ));
    lines.push(auth_option_line(
        1,
        "Environment variable",
        "Store only a read-only env reference for CI or managed shells",
        dialog.selected == 1,
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter to select, Up/Down to navigate, Esc to go back",
        Style::default().fg(Color::DarkGray),
    )));
    push_auth_error(&mut lines, dialog.error.as_deref());
    lines
}

pub(crate) fn protocol_option_text(protocol: ProviderProtocol) -> (&'static str, &'static str) {
    match protocol {
        ProviderProtocol::OpenAiCompatible => (
            "OpenAI-compatible",
            "Standard OpenAI API format (most common)",
        ),
        ProviderProtocol::Anthropic => ("Anthropic-compatible", "Anthropic Messages API format"),
        ProviderProtocol::Gemini => ("Gemini-compatible", "Google Gemini API format"),
        ProviderProtocol::VertexAi => (
            "Vertex AI",
            "Google Cloud project/location endpoint with OAuth token",
        ),
        ProviderProtocol::Genai => ("rust-genai", "Model-routed native provider adapter"),
        _ => ("Unsupported", "Not available for custom provider setup"),
    }
}

pub(crate) fn auth_model_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![Span::styled(
        auth_step_title(dialog),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.push(Line::from(""));
    if dialog.model_options().is_empty() {
        lines.push(Line::from(vec![
            Span::styled("Model: ", Style::default().fg(Color::Cyan)),
            Span::styled(
                if dialog.model.is_empty() {
                    "model id, for example gpt-4.1 or qwen-coder".to_owned()
                } else {
                    dialog.model.clone()
                },
                Style::default().fg(if dialog.model.is_empty() {
                    Color::DarkGray
                } else {
                    Color::White
                }),
            ),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "Recommended models",
            Style::default().fg(Color::DarkGray),
        )));
        lines.extend(
            dialog
                .model_options()
                .iter()
                .enumerate()
                .map(|(index, model)| {
                    auth_option_line(
                        index,
                        model,
                        "built-in recommendation",
                        index == dialog.selected,
                    )
                }),
        );
        let custom_value = if dialog.model.is_empty() {
            "type a custom model id"
        } else {
            dialog.model.as_str()
        };
        lines.push(auth_option_line(
            dialog.custom_model_index(),
            "Custom model",
            custom_value,
            dialog.is_custom_model_selected(),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter continue   Type to use custom model   Esc back",
        Style::default().fg(Color::DarkGray),
    )));
    push_auth_error(&mut lines, dialog.error.as_deref());
    lines
}

pub(crate) fn auth_advanced_config_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![Span::styled(
            auth_step_title(dialog),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        auth_option_line(
            0,
            "Thinking",
            if dialog.enable_thinking {
                "enabled"
            } else {
                "default"
            },
            dialog.advanced_selected == 0,
        ),
        auth_option_line(
            1,
            "Reasoning effort",
            reasoning_effort_label(dialog.reasoning_effort),
            dialog.advanced_selected == 1,
        ),
        auth_option_line(
            2,
            "Context window",
            if dialog.context_window_size.is_empty() {
                "default"
            } else {
                dialog.context_window_size.as_str()
            },
            dialog.advanced_selected == 2,
        ),
        auth_option_line(
            3,
            "Max output tokens",
            if dialog.max_tokens.is_empty() {
                "default"
            } else {
                dialog.max_tokens.as_str()
            },
            dialog.advanced_selected == 3,
        ),
        auth_option_line(
            4,
            "Custom headers",
            if dialog.custom_headers.is_empty() {
                "none"
            } else {
                dialog.custom_headers.as_str()
            },
            dialog.advanced_selected == 4,
        ),
        Line::from(""),
        Line::from(Span::styled(
            "Up/Down select   Space toggle/cycle   Enter continue   Esc back",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    push_auth_error(&mut lines, dialog.error.as_deref());
    lines
}

pub(crate) fn auth_review_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let Some(review) = &dialog.review else {
        return vec![Line::from(Span::styled(
            "Review is not ready",
            Style::default().fg(Color::Red),
        ))];
    };
    let update_line = if review.replaces_unreadable_config {
        "will replace unreadable provider config"
    } else if review.updates_existing_profile {
        "will update existing profile"
    } else {
        "will create new profile"
    };
    let mut lines = vec![
        Line::from(vec![Span::styled(
            "Review provider setup",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        auth_kv_line("Provider", review.provider_title),
        auth_kv_line("Profile", &review.profile),
        auth_kv_line("Protocol", &review.protocol),
        auth_kv_line("Base URL", &review.base_url),
        auth_kv_line("Model", &review.model),
        auth_kv_line("Credential", &review.credential),
        auth_kv_line("Advanced", &review.advanced),
        auth_kv_line("Scope", provider_scope_label(review.scope)),
        auth_kv_line("Config", &review.config_path.display().to_string()),
        auth_kv_line("Plan", update_line),
        Line::from(""),
        Line::from(Span::styled(
            "Install plan preview",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    lines.extend(review.preview_json.lines().take(8).map(|line| {
        Line::from(Span::styled(
            line.to_owned(),
            Style::default().fg(Color::Gray),
        ))
    }));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter save   Esc back   Ctrl+C twice quit",
        Style::default().fg(Color::DarkGray),
    )));
    push_auth_error(&mut lines, dialog.error.as_deref());
    lines
}

pub(crate) fn auth_kv_line(label: &'static str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), Style::default().fg(Color::Cyan)),
        Span::styled(value.to_owned(), Style::default().fg(Color::White)),
    ])
}

pub(crate) fn provider_scope_label(scope: ProviderConfigScope) -> &'static str {
    match scope {
        ProviderConfigScope::User => "user",
        ProviderConfigScope::Workspace => "workspace",
    }
}

pub(crate) fn auth_option_line(
    index: usize,
    title: &str,
    detail: &str,
    selected: bool,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if selected { "> " } else { "  " },
            Style::default().fg(if selected {
                Color::Cyan
            } else {
                Color::DarkGray
            }),
        ),
        Span::styled(
            format!("{} ", index + 1),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            title.to_owned(),
            Style::default()
                .fg(if selected { Color::White } else { Color::Gray })
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        Span::raw("  "),
        Span::styled(detail.to_owned(), Style::default().fg(Color::DarkGray)),
    ])
}

pub(crate) fn push_auth_error(lines: &mut Vec<Line<'static>>, error: Option<&str>) {
    if let Some(error) = error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.to_owned(),
            Style::default().fg(Color::Red),
        )));
    }
}

pub(crate) fn auth_step_title(dialog: &AuthDialogState) -> String {
    let provider_title = dialog
        .provider
        .map(|provider| provider.title)
        .unwrap_or("Connect provider");
    if matches!(
        dialog.provider.map(|provider| provider.source),
        Some(AuthProviderSource::Custom)
    ) {
        let step = match dialog.step {
            AuthDialogStep::Protocol => "Step 1/7 · Protocol",
            AuthDialogStep::BaseUrl => "Step 2/7 · Base URL",
            AuthDialogStep::CredentialStore => "Step 3/7 · Credential storage",
            AuthDialogStep::ApiKey => "Step 4/7 · API Key",
            AuthDialogStep::EnvKey => "Step 4/7 · Environment variable",
            AuthDialogStep::Model => "Step 5/7 · Model IDs",
            AuthDialogStep::AdvancedConfig => "Step 6/7 · Advanced Config",
            AuthDialogStep::Review => "Step 7/7 · Review",
            AuthDialogStep::GroupChoice
            | AuthDialogStep::ThirdPartyChoice
            | AuthDialogStep::AuthMethod => "",
        };
        if step.is_empty() {
            provider_title.to_owned()
        } else {
            format!("{provider_title} · {step}")
        }
    } else {
        provider_title.to_owned()
    }
}

pub(crate) fn auth_input_lines(
    title: &str,
    label: &'static str,
    hint: &'static str,
    value: &str,
    error: Option<&str>,
    secret: bool,
) -> Vec<Line<'static>> {
    let visible_value = if secret && !value.is_empty() {
        "*".repeat(value.chars().count())
    } else {
        value.to_owned()
    };
    let input = if visible_value.is_empty() {
        hint.to_owned()
    } else {
        visible_value
    };
    let mut lines = vec![
        Line::from(vec![Span::styled(
            title.to_owned(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("{label}: "), Style::default().fg(Color::Cyan)),
            Span::styled(
                input,
                Style::default().fg(if value.is_empty() {
                    Color::DarkGray
                } else {
                    Color::White
                }),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Enter continue   Esc back   Ctrl+C twice quit",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    if let Some(error) = error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.to_owned(),
            Style::default().fg(Color::Red),
        )));
    }
    lines
}

pub(crate) fn draw_resume_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    picker: &ResumePickerState,
    current_thread_id: ThreadId,
    app: &TuiApp,
) {
    let palette = app.palette();
    if let Some(action) = picker.action {
        let selected = picker.items.get(picker.selected);
        let rename_prefix = "Title: ";
        let rename_prefix_width = u16::try_from(display_width(rename_prefix)).unwrap_or(u16::MAX);
        let rename_viewport = (action == SessionPickerAction::Rename).then(|| {
            picker
                .action_input
                .viewport(area.width.saturating_sub(rename_prefix_width).max(1), 1)
        });
        let (title, prompt) = match action {
            SessionPickerAction::Rename => (
                "Rename session",
                format!(
                    "{rename_prefix}{}",
                    rename_viewport
                        .as_ref()
                        .and_then(|viewport| viewport.lines.first())
                        .cloned()
                        .unwrap_or_default()
                ),
            ),
            SessionPickerAction::Archive => (
                "Archive session",
                format!(
                    "Archive '{}' and remove it from the session list?  y/n",
                    selected.map_or("session", |item| item.title.as_str())
                ),
            ),
            SessionPickerAction::Delete => (
                "Remove from history",
                format!(
                    "Remove '{}' from session history? Runtime audit records follow retention policy.  y/n",
                    selected.map_or("session", |item| item.title.as_str())
                ),
            ),
        };
        frame.render_widget(
            Paragraph::new(prompt)
                .block(Block::default().title(title).borders(Borders::TOP))
                .wrap(Wrap { trim: false }),
            area,
        );
        if let Some(viewport) = rename_viewport
            && area.width > rename_prefix_width
            && area.height > 1
        {
            frame.set_cursor_position((
                area.x
                    .saturating_add(rename_prefix_width)
                    .saturating_add(viewport.cursor.0)
                    .min(area.right().saturating_sub(1)),
                area.y
                    .saturating_add(1)
                    .min(area.bottom().saturating_sub(1)),
            ));
        }
        return;
    }
    let visible_count = resume_picker_page_size(area);
    let offset = resume_picker_offset(picker.selected, visible_count, picker.items.len());
    let items = picker
        .items
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_count)
        .map(|(index, item)| {
            let selected = index == picker.selected;
            let current = item.thread_id == current_thread_id;
            let marker = selection_marker(app, selected);
            let current_marker = if current { "current" } else { "" };
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(
                        marker,
                        Style::default().fg(if selected {
                            palette.accent
                        } else {
                            palette.muted
                        }),
                    ),
                    Span::styled(
                        format!("{} ", index + 1),
                        Style::default().fg(palette.muted),
                    ),
                    Span::styled(
                        item.title.clone(),
                        Style::default()
                            .fg(if selected {
                                palette.text
                            } else {
                                palette.subtle
                            })
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::raw("  "),
                    Span::styled(current_marker, Style::default().fg(palette.success)),
                ]),
                Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        short_id(&item.session_id.to_string()),
                        Style::default().fg(palette.muted),
                    ),
                    Span::raw("  "),
                    Span::styled(item.preview.clone(), Style::default().fg(palette.muted)),
                ]),
            ];
            if selected && picker.show_details {
                lines.push(Line::from(vec![
                    Span::raw("    thread  "),
                    Span::styled(
                        item.thread_id.to_string(),
                        Style::default().fg(palette.muted),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::raw("    session "),
                    Span::styled(
                        item.session_id.to_string(),
                        Style::default().fg(palette.muted),
                    ),
                ]));
            }
            ListItem::new(lines)
        })
        .collect::<Vec<_>>();
    let filter_prefix = "Resume session · filter: ";
    let filter_prefix_width = u16::try_from(display_width(filter_prefix)).unwrap_or(u16::MAX);
    let filter_viewport = picker
        .search
        .viewport(area.width.saturating_sub(filter_prefix_width).max(1), 1);
    let title = format!(
        "{filter_prefix}{}",
        filter_viewport.lines.first().cloned().unwrap_or_default()
    );
    let list = List::new(items).block(Block::default().title(title).borders(Borders::TOP));
    frame.render_widget(list, area);
    if area.width > filter_prefix_width && area.height > 0 {
        frame.set_cursor_position((
            area.x
                .saturating_add(filter_prefix_width)
                .saturating_add(filter_viewport.cursor.0)
                .min(area.right().saturating_sub(1)),
            area.y,
        ));
    }
}

pub(crate) fn draw_queue_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    picker: &QueuePickerState,
    app: &TuiApp,
) {
    let palette = app.palette();
    let visible_count = usize::from(area.height.saturating_sub(1)).max(1);
    let offset = resume_picker_offset(picker.selected, visible_count, picker.items.len());
    let items = picker
        .items
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_count)
        .map(|(index, queued)| {
            let selected = index == picker.selected;
            let marker = selection_marker(app, selected);
            let mode = if queued.steer { "steer" } else { "queued" };
            ListItem::new(Line::from(vec![
                Span::styled(
                    marker,
                    Style::default().fg(if selected {
                        palette.accent
                    } else {
                        palette.muted
                    }),
                ),
                Span::styled(
                    format!("{} {mode}  ", index + 1),
                    Style::default().fg(palette.warning),
                ),
                Span::styled(
                    truncate_end_to_width(
                        &queued.prompt.replace('\n', " "),
                        usize::from(area.width).saturating_sub(16),
                    ),
                    Style::default()
                        .fg(if selected {
                            palette.text
                        } else {
                            palette.subtle
                        })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ]))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title("Queued prompts")
                .borders(Borders::TOP),
        ),
        area,
    );
}

pub(crate) fn draw_approval_dialog(
    frame: &mut Frame<'_>,
    area: Rect,
    dialog: &ApprovalDialogState,
    app: &TuiApp,
) {
    let palette = app.palette();
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Tool: ", Style::default().fg(palette.muted)),
            Span::styled(
                dialog.request.tool_name.clone(),
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Resource: ", Style::default().fg(palette.muted)),
            Span::styled(
                dialog.request.resource.clone(),
                Style::default().fg(palette.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("Reason: ", Style::default().fg(palette.muted)),
            Span::styled(
                dialog.request.reason.clone(),
                Style::default().fg(palette.subtle),
            ),
        ]),
        Line::from(""),
    ];
    lines.extend(
        ApprovalChoice::ALL
            .iter()
            .copied()
            .enumerate()
            .flat_map(|(index, choice)| {
                let selected = index == dialog.selected;
                let marker = selection_marker(app, selected);
                let color = if choice == ApprovalChoice::Deny {
                    palette.error
                } else if selected {
                    palette.accent
                } else {
                    palette.subtle
                };
                [
                    Line::from(vec![
                        Span::styled(marker, Style::default().fg(color)),
                        Span::styled(
                            format!("{}  {}", index + 1, choice.label()),
                            Style::default().fg(color).add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                        ),
                    ]),
                    Line::from(vec![
                        Span::raw("     "),
                        Span::styled(choice.detail(), Style::default().fg(palette.muted)),
                    ]),
                ]
            }),
    );
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "A scoped grant expires when this task execution ends.",
        Style::default().fg(palette.muted),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Approval required")
                    .borders(Borders::TOP),
            )
            .wrap(Wrap { trim: false })
            .scroll((approval_scroll_offset(dialog, area), 0)),
        area,
    );
}

pub(crate) fn draw_question_dialog(
    frame: &mut Frame<'_>,
    area: Rect,
    dialog: &QuestionDialogState,
    app: &TuiApp,
) {
    let palette = app.palette();
    let question = dialog.current_question();
    let mode = match question.mode {
        golutra_core::UserQuestionMode::Single => "select one or enter another answer",
        golutra_core::UserQuestionMode::Multiple => "select one or more, or enter another answer",
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!(
                    "Question {}/{}  ",
                    dialog.question_index + 1,
                    dialog.request.questions.len()
                ),
                Style::default().fg(palette.muted),
            ),
            Span::styled(
                question.header.clone(),
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            question.question.clone(),
            Style::default().fg(palette.text),
        )),
        Line::from(Span::styled(mode, Style::default().fg(palette.muted))),
        Line::from(""),
    ];
    for (index, option) in question.options.iter().enumerate() {
        let focused = !dialog.is_free_text_focused() && index == dialog.option_index;
        let selected = dialog.is_selected(index);
        let marker = match (app.preferences.screen_reader, question.mode) {
            (true, golutra_core::UserQuestionMode::Single) => {
                if selected {
                    "selected"
                } else {
                    "option"
                }
            }
            (true, golutra_core::UserQuestionMode::Multiple) => {
                if selected {
                    "checked"
                } else {
                    "unchecked"
                }
            }
            (false, golutra_core::UserQuestionMode::Single) => {
                if selected {
                    "(*)"
                } else {
                    "( )"
                }
            }
            (false, golutra_core::UserQuestionMode::Multiple) => {
                if selected {
                    "[x]"
                } else {
                    "[ ]"
                }
            }
        };
        lines.push(Line::from(vec![
            Span::styled(
                selection_marker(app, focused),
                Style::default().fg(if focused {
                    palette.accent
                } else {
                    palette.muted
                }),
            ),
            Span::styled(
                format!("{marker} {}", option.label),
                Style::default()
                    .fg(if focused {
                        palette.text
                    } else {
                        palette.subtle
                    })
                    .add_modifier(if focused {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ]));
        if let Some(description) = &option.description {
            lines.push(Line::from(vec![
                Span::raw("       "),
                Span::styled(description.clone(), Style::default().fg(palette.muted)),
            ]));
        }
    }
    lines.push(Line::from(""));
    let free_text_focused = dialog.is_free_text_focused();
    lines.push(Line::from(vec![
        Span::styled(
            selection_marker(app, free_text_focused),
            Style::default().fg(if free_text_focused {
                palette.accent
            } else {
                palette.muted
            }),
        ),
        Span::styled(
            if app.preferences.screen_reader && dialog.free_text_is_filled() {
                "Other answer / notes (filled)"
            } else {
                "Other answer / notes"
            },
            Style::default()
                .fg(if free_text_focused {
                    palette.text
                } else {
                    palette.subtle
                })
                .add_modifier(if free_text_focused {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
    ]));
    let text_width = area
        .width
        .saturating_sub(QUESTION_FREE_TEXT_PREFIX_WIDTH)
        .max(1);
    let text_viewport = dialog
        .current_free_text()
        .viewport(text_width, MAX_QUESTION_FREE_TEXT_ROWS);
    for (index, line) in text_viewport.lines.iter().enumerate() {
        let content = if index == 0 && dialog.current_free_text().is_empty() {
            truncate_end_to_width(
                "Type a different answer or add context",
                usize::from(text_width),
            )
        } else {
            line.clone()
        };
        lines.push(Line::from(vec![
            Span::raw(QUESTION_FREE_TEXT_PREFIX),
            Span::styled(
                content,
                Style::default().fg(if dialog.current_free_text().is_empty() {
                    palette.muted
                } else {
                    palette.text
                }),
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        if dialog.all_answered() {
            "[ Submit answers ]"
        } else {
            "[ Answer every question to submit ]"
        },
        Style::default().fg(if dialog.all_answered() {
            palette.accent
        } else {
            palette.muted
        }),
    )));
    let scroll = question_scroll_offset(dialog, app, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Input required")
                    .borders(Borders::TOP),
            )
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
    if free_text_focused && area.width > QUESTION_FREE_TEXT_PREFIX_WIDTH && area.height > 1 {
        let layout = question_visual_layout(dialog, app, area.width);
        let cursor_row = layout
            .free_text_input_start
            .saturating_add(usize::from(text_viewport.cursor.1));
        let scroll = usize::from(scroll);
        let visible = usize::from(area.height.saturating_sub(1));
        if cursor_row >= scroll && cursor_row.saturating_sub(scroll) < visible {
            frame.set_cursor_position((
                area.x
                    .saturating_add(QUESTION_FREE_TEXT_PREFIX_WIDTH)
                    .saturating_add(text_viewport.cursor.0)
                    .min(area.right().saturating_sub(1)),
                area.y
                    .saturating_add(1)
                    .saturating_add(
                        u16::try_from(cursor_row.saturating_sub(scroll)).unwrap_or(u16::MAX),
                    )
                    .min(area.bottom().saturating_sub(1)),
            ));
        }
    }
}

fn approval_scroll_offset(dialog: &ApprovalDialogState, area: Rect) -> u16 {
    let visible = usize::from(area.height.saturating_sub(1)).max(1);
    let selected = dialog.selected.min(ApprovalChoice::ALL.len() - 1);
    let (start, height) = approval_visual_ranges(dialog, area.width)
        .into_iter()
        .find_map(|(index, start, height)| (index == selected).then_some((start, height)))
        .unwrap_or_default();
    u16::try_from(scroll_offset_for_range(start, height, visible)).unwrap_or(u16::MAX)
}

fn approval_visual_ranges(dialog: &ApprovalDialogState, width: u16) -> Vec<(usize, usize, usize)> {
    let mut cursor = wrapped_text_height(&format!("Tool: {}", dialog.request.tool_name), width)
        .saturating_add(wrapped_text_height(
            &format!("Resource: {}", dialog.request.resource),
            width,
        ))
        .saturating_add(wrapped_text_height(
            &format!("Reason: {}", dialog.request.reason),
            width,
        ))
        .saturating_add(1);
    ApprovalChoice::ALL
        .iter()
        .copied()
        .enumerate()
        .map(|(index, choice)| {
            let primary = format!("  {}  {}", index + 1, choice.label());
            let detail = format!("     {}", choice.detail());
            let height = wrapped_text_height(&primary, width)
                .saturating_add(wrapped_text_height(&detail, width));
            let start = cursor;
            cursor = cursor.saturating_add(height);
            (index, start, height)
        })
        .collect()
}

#[derive(Debug, Default)]
struct QuestionVisualLayout {
    options: Vec<(usize, usize, usize)>,
    free_text: (usize, usize),
    free_text_input_start: usize,
    submit: (usize, usize),
}

fn question_scroll_offset(dialog: &QuestionDialogState, app: &TuiApp, area: Rect) -> u16 {
    let visible = usize::from(area.height.saturating_sub(1)).max(1);
    let layout = question_visual_layout(dialog, app, area.width);
    let (start, height) = if dialog.is_free_text_focused() {
        layout.free_text
    } else {
        layout
            .options
            .iter()
            .find_map(|(index, start, height)| {
                (*index == dialog.option_index).then_some((*start, *height))
            })
            .unwrap_or_default()
    };
    let (start, height) = if !dialog.is_free_text_focused()
        && dialog.all_answered()
        && dialog.question_index + 1 == dialog.request.questions.len()
    {
        layout.submit
    } else {
        (start, height)
    };
    u16::try_from(scroll_offset_for_range(start, height, visible)).unwrap_or(u16::MAX)
}

fn question_visual_layout(
    dialog: &QuestionDialogState,
    app: &TuiApp,
    width: u16,
) -> QuestionVisualLayout {
    let question = dialog.current_question();
    let mode = match question.mode {
        golutra_core::UserQuestionMode::Single => "select one or enter another answer",
        golutra_core::UserQuestionMode::Multiple => "select one or more, or enter another answer",
    };
    let mut cursor = wrapped_text_height(
        &format!(
            "Question {}/{}  {}",
            dialog.question_index + 1,
            dialog.request.questions.len(),
            question.header
        ),
        width,
    )
    .saturating_add(wrapped_text_height(&question.question, width))
    .saturating_add(wrapped_text_height(mode, width))
    .saturating_add(1);
    let mut options = Vec::with_capacity(question.options.len());
    for (index, option) in question.options.iter().enumerate() {
        let selected = dialog.is_selected(index);
        let marker = match (app.preferences.screen_reader, question.mode) {
            (true, golutra_core::UserQuestionMode::Single) => {
                if selected {
                    "selected"
                } else {
                    "option"
                }
            }
            (true, golutra_core::UserQuestionMode::Multiple) => {
                if selected {
                    "checked"
                } else {
                    "unchecked"
                }
            }
            (false, golutra_core::UserQuestionMode::Single) => {
                if selected {
                    "(*)"
                } else {
                    "( )"
                }
            }
            (false, golutra_core::UserQuestionMode::Multiple) => {
                if selected {
                    "[x]"
                } else {
                    "[ ]"
                }
            }
        };
        let primary = format!("  {marker} {}", option.label);
        let mut height = wrapped_text_height(&primary, width);
        if let Some(description) = &option.description {
            height =
                height.saturating_add(wrapped_text_height(&format!("       {description}"), width));
        }
        options.push((index, cursor, height));
        cursor = cursor.saturating_add(height);
    }
    cursor = cursor.saturating_add(1);
    let free_text_start = cursor;
    let free_text_label = if app.preferences.screen_reader && dialog.free_text_is_filled() {
        "  Other answer / notes (filled)"
    } else {
        "  Other answer / notes"
    };
    let free_text_label_height = wrapped_text_height(free_text_label, width);
    let free_text_input_start = cursor.saturating_add(free_text_label_height);
    let text_width = width.saturating_sub(QUESTION_FREE_TEXT_PREFIX_WIDTH).max(1);
    let free_text_input_height = dialog
        .current_free_text()
        .viewport(text_width, MAX_QUESTION_FREE_TEXT_ROWS)
        .lines
        .len();
    let free_text_height = free_text_label_height.saturating_add(free_text_input_height);
    cursor = cursor.saturating_add(free_text_height).saturating_add(1);
    let submit_text = if dialog.all_answered() {
        "[ Submit answers ]"
    } else {
        "[ Answer every question to submit ]"
    };
    let submit_height = wrapped_text_height(submit_text, width);
    QuestionVisualLayout {
        options,
        free_text: (free_text_start, free_text_height),
        free_text_input_start,
        submit: (cursor, submit_height),
    }
}

fn scroll_offset_for_range(start: usize, height: usize, visible: usize) -> usize {
    if height >= visible {
        start
    } else {
        start.saturating_add(height).saturating_sub(visible)
    }
}

pub(crate) fn draw_help_dialog(
    frame: &mut Frame<'_>,
    area: Rect,
    dialog: &HelpDialogState,
    app: &TuiApp,
) {
    let lines = help_dialog_lines(dialog, app, area.width);
    let scroll = dialog.scroll.min(help_scroll_max(dialog, app, area));
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title("Help").borders(Borders::TOP))
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        area,
    );
}

fn help_dialog_lines(dialog: &HelpDialogState, app: &TuiApp, width: u16) -> Vec<Line<'static>> {
    let palette = app.palette();
    let tabs = HelpTopic::ALL
        .iter()
        .copied()
        .enumerate()
        .flat_map(|(index, topic)| {
            let selected = topic == dialog.topic;
            [
                Span::styled(
                    format!(" {} {} ", index + 1, help_tab_label(topic, width)),
                    Style::default()
                        .fg(if selected {
                            palette.selected_foreground
                        } else {
                            palette.subtle
                        })
                        .bg(if selected {
                            palette.accent
                        } else {
                            Color::Reset
                        })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::raw(" "),
            ]
        })
        .collect::<Vec<_>>();
    let mut lines = vec![Line::from(tabs), Line::from("")];
    lines.extend(
        help_lines(dialog.topic, app.preferences.keymap, &dialog.context)
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(palette.text)))),
    );
    lines
}

pub(crate) fn help_scroll_max(dialog: &HelpDialogState, app: &TuiApp, area: Rect) -> usize {
    let content_height = help_dialog_lines(dialog, app, area.width)
        .iter()
        .map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            wrapped_text_height(&text, area.width)
        })
        .sum::<usize>();
    let visible = usize::from(area.height.saturating_sub(1)).max(1);
    content_height.saturating_sub(visible)
}

pub(crate) fn draw_dashboard(
    frame: &mut Frame<'_>,
    area: Rect,
    dashboard: &DashboardState,
    events: &[RuntimeEvent],
    app: &TuiApp,
) {
    let palette = app.palette();
    let tabs = DashboardTab::ALL
        .iter()
        .copied()
        .enumerate()
        .flat_map(|(index, tab)| {
            let selected = tab == dashboard.tab;
            [
                Span::styled(
                    format!(" {} {} ", index + 1, tab.label()),
                    Style::default()
                        .fg(if selected {
                            palette.selected_foreground
                        } else {
                            palette.subtle
                        })
                        .bg(if selected {
                            palette.accent
                        } else {
                            Color::Reset
                        })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::raw(" "),
            ]
        })
        .collect::<Vec<_>>();
    let mut lines = vec![Line::from(tabs), Line::from("")];
    lines.extend(
        dashboard_lines(dashboard.tab, events)
            .into_iter()
            .map(Line::from),
    );
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title("Runtime").borders(Borders::TOP))
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(dashboard.scroll).unwrap_or(u16::MAX), 0)),
        area,
    );
}

pub(crate) fn draw_settings_dialog(
    frame: &mut Frame<'_>,
    area: Rect,
    dialog: &SettingsDialogState,
    app: &TuiApp,
) {
    let palette = app.palette();
    let rows = settings_display_rows(dialog);
    let mut lines = vec![Line::from(vec![Span::styled(
        "Session controls",
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.push(Line::from(""));
    for display in rows {
        if display.row == SettingsRow::Keymap {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Interface",
                Style::default()
                    .fg(palette.text)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        let selected = dialog.selected_row == display.row;
        let marker = selection_marker(app, selected);
        let locked = dialog.runtime_locked && display.row.is_runtime_control();
        let label_style = Style::default()
            .fg(if locked {
                palette.muted
            } else if selected {
                palette.text
            } else {
                palette.subtle
            })
            .add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        let value_style = Style::default().fg(
            if display.row == SettingsRow::Permissions
                && dialog.draft.permission_mode == PermissionMode::Unrestricted
            {
                palette.warning
            } else {
                palette.accent
            },
        );
        if display.row == SettingsRow::Model && dialog.editing_model {
            let viewport = settings_model_viewport(dialog, area.width);
            lines.push(Line::from(vec![
                Span::styled(
                    marker,
                    Style::default().fg(if selected {
                        palette.accent
                    } else {
                        palette.muted
                    }),
                ),
                Span::styled(format!("{}: ", display.label), label_style),
                Span::styled(
                    viewport.lines.first().cloned().unwrap_or_default(),
                    value_style,
                ),
            ]));
            lines.extend(viewport.lines.into_iter().skip(1).map(|line| {
                Line::from(vec![
                    Span::raw(" ".repeat(usize::from(SETTINGS_MODEL_PREFIX_WIDTH))),
                    Span::styled(line, value_style),
                ])
            }));
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(display.detail, Style::default().fg(palette.muted)),
            ]));
        } else {
            lines.extend([
                Line::from(vec![
                    Span::styled(
                        marker,
                        Style::default().fg(if selected {
                            palette.accent
                        } else {
                            palette.muted
                        }),
                    ),
                    Span::styled(format!("{}: ", display.label), label_style),
                    Span::styled(display.value, value_style),
                    Span::styled(
                        if locked {
                            "  locked while task is active"
                        } else {
                            ""
                        },
                        Style::default().fg(palette.muted),
                    ),
                ]),
                Line::from(vec![
                    Span::raw("    "),
                    Span::styled(display.detail, Style::default().fg(palette.muted)),
                ]),
            ]);
        }
    }
    if dialog.unrestricted_confirmation {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Press Ctrl+S again to confirm unrestricted execution.",
            Style::default().fg(palette.warning),
        )));
    }
    let scroll = settings_scroll_offset(dialog, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title("Settings").borders(Borders::TOP))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
    if dialog.editing_model && area.width > SETTINGS_MODEL_PREFIX_WIDTH && area.height > 1 {
        let viewport = settings_model_viewport(dialog, area.width);
        let model_start = settings_visual_ranges(dialog, area.width)
            .into_iter()
            .find_map(|(row, start, _)| (row == SettingsRow::Model).then_some(start))
            .unwrap_or_default();
        let cursor_row = model_start.saturating_add(usize::from(viewport.cursor.1));
        let scroll = usize::from(scroll);
        let visible = usize::from(area.height.saturating_sub(1));
        if cursor_row >= scroll && cursor_row.saturating_sub(scroll) < visible {
            frame.set_cursor_position((
                area.x
                    .saturating_add(SETTINGS_MODEL_PREFIX_WIDTH)
                    .saturating_add(viewport.cursor.0)
                    .min(area.right().saturating_sub(1)),
                area.y
                    .saturating_add(1)
                    .saturating_add(
                        u16::try_from(cursor_row.saturating_sub(scroll)).unwrap_or(u16::MAX),
                    )
                    .min(area.bottom().saturating_sub(1)),
            ));
        }
    }
}

fn settings_model_viewport(dialog: &SettingsDialogState, width: u16) -> ComposerViewport {
    dialog.model_input.viewport(
        width.saturating_sub(SETTINGS_MODEL_PREFIX_WIDTH).max(1),
        u16::MAX,
    )
}

pub(crate) const fn toggle_label(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

pub(crate) fn settings_scroll_offset(dialog: &SettingsDialogState, area: Rect) -> u16 {
    let visible = usize::from(area.height.saturating_sub(1)).max(1);
    let selected_end = settings_visual_ranges(dialog, area.width)
        .into_iter()
        .find(|(row, _, _)| *row == dialog.selected_row)
        .map_or(0, |(_, start, height)| start.saturating_add(height));
    u16::try_from(selected_end.saturating_sub(visible)).unwrap_or(u16::MAX)
}

#[derive(Debug)]
struct SettingsDisplayRow {
    row: SettingsRow,
    label: &'static str,
    value: String,
    detail: &'static str,
}

fn settings_display_rows(dialog: &SettingsDialogState) -> Vec<SettingsDisplayRow> {
    let profile = dialog
        .draft
        .profile_name
        .as_deref()
        .unwrap_or("active provider");
    let model = if dialog.editing_model {
        dialog.model_input.text().to_owned()
    } else {
        dialog.draft.effective_model().to_owned()
    };
    vec![
        SettingsDisplayRow {
            row: SettingsRow::Profile,
            label: "Provider profile",
            value: profile.to_owned(),
            detail: if dialog.choices.is_empty() {
                "remote/default profile"
            } else {
                "Left/Right switches configured profiles"
            },
        },
        SettingsDisplayRow {
            row: SettingsRow::Model,
            label: "Model",
            value: model,
            detail: "Enter or e edits a per-session model id",
        },
        SettingsDisplayRow {
            row: SettingsRow::Reasoning,
            label: "Reasoning effort",
            value: effort_label(dialog.draft.reasoning_effort).to_owned(),
            detail: "default, low, medium, high, xhigh",
        },
        SettingsDisplayRow {
            row: SettingsRow::Permissions,
            label: "Permissions",
            value: dialog.draft.permission_mode.label().to_owned(),
            detail: if dialog.draft.permission_mode == PermissionMode::Unrestricted {
                "workspace, approval and sandbox guards are disabled"
            } else {
                "workspace guards and on-request approvals"
            },
        },
        SettingsDisplayRow {
            row: SettingsRow::Keymap,
            label: "Keymap",
            value: dialog.draft_preferences.keymap.label().to_owned(),
            detail: "standard editing or a Vim insert/normal workflow",
        },
        SettingsDisplayRow {
            row: SettingsRow::Theme,
            label: "Theme",
            value: dialog.draft_preferences.theme.label().to_owned(),
            detail: "classic, amber or monochrome semantic accents",
        },
        SettingsDisplayRow {
            row: SettingsRow::HighContrast,
            label: "High contrast",
            value: toggle_label(dialog.draft_preferences.high_contrast).to_owned(),
            detail: "brighter text, status colors and focus markers",
        },
        SettingsDisplayRow {
            row: SettingsRow::ReducedMotion,
            label: "Reduced motion",
            value: toggle_label(dialog.draft_preferences.reduced_motion).to_owned(),
            detail: "slower refresh cadence for changing status indicators",
        },
        SettingsDisplayRow {
            row: SettingsRow::ScreenReader,
            label: "Screen reader symbols",
            value: toggle_label(dialog.draft_preferences.screen_reader).to_owned(),
            detail: "ASCII state and disclosure markers",
        },
    ]
}

fn settings_visual_ranges(
    dialog: &SettingsDialogState,
    width: u16,
) -> Vec<(SettingsRow, usize, usize)> {
    let mut cursor = 2_usize;
    settings_display_rows(dialog)
        .into_iter()
        .map(|display| {
            if display.row == SettingsRow::Keymap {
                cursor = cursor.saturating_add(2);
            }
            let locked = dialog.runtime_locked && display.row.is_runtime_control();
            let primary_height = if display.row == SettingsRow::Model && dialog.editing_model {
                settings_model_viewport(dialog, width).lines.len()
            } else {
                let primary = format!(
                    "  {}: {}{}",
                    display.label,
                    display.value,
                    if locked {
                        "  locked while task is active"
                    } else {
                        ""
                    }
                );
                wrapped_text_height(&primary, width)
            };
            let detail = format!("    {}", display.detail);
            let height = primary_height.saturating_add(wrapped_text_height(&detail, width));
            let start = cursor;
            cursor = cursor.saturating_add(height);
            (display.row, start, height)
        })
        .collect()
}

fn wrapped_text_height(value: &str, width: u16) -> usize {
    if width == 0 {
        return 0;
    }
    Paragraph::new(value.to_owned())
        .wrap(Wrap { trim: false })
        .line_count(width)
        .max(1)
}

pub(crate) fn draw_export_flow(
    frame: &mut Frame<'_>,
    area: Rect,
    flow: &ExportFlowState,
    current_thread_id: ThreadId,
    app: &TuiApp,
) {
    match flow.step {
        ExportFlowStep::SelectSession => {
            draw_resume_picker_with_title(
                frame,
                area,
                &flow.picker,
                current_thread_id,
                "Export session",
                app,
            );
        }
        ExportFlowStep::Range => {
            let selected = flow
                .selected_item()
                .map(|item| {
                    format!(
                        "{} ({})",
                        item.title,
                        short_id(&item.session_id.to_string())
                    )
                })
                .unwrap_or_else(|| "session".to_owned());
            draw_export_input_step(
                frame,
                area,
                "Export range",
                vec![
                    "Export selected session".to_owned(),
                    format!("Anchor: {selected}"),
                    String::new(),
                ],
                "Range: ",
                &flow.range_input,
                vec![
                    String::new(),
                    "1 = anchor only   +N = newer   -N = older".to_owned(),
                    "Enter continue   Esc back".to_owned(),
                ],
            );
        }
        ExportFlowStep::Destination => {
            draw_export_input_step(
                frame,
                area,
                "Export destination",
                vec![
                    "Choose an absolute destination directory".to_owned(),
                    String::new(),
                ],
                "Path: ",
                &flow.destination_input,
                vec![
                    String::new(),
                    "The directory must not already exist".to_owned(),
                    "Enter review   Esc back".to_owned(),
                ],
            );
        }
        ExportFlowStep::Review => {
            let selected = flow
                .selected_item()
                .map(|item| item.title.clone())
                .unwrap_or_else(|| "session".to_owned());
            let lines = vec![
                Line::from("Review export"),
                Line::from(format!("Session: {selected}")),
                Line::from(format!("Range: {}", flow.range_input.text())),
                Line::from(format!("Destination: {}", flow.destination_input.text())),
                Line::from("Mode: full-redacted"),
                Line::from("Enter export   Esc back"),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .title("Export review")
                        .borders(Borders::TOP),
                ),
                area,
            );
        }
        ExportFlowStep::Running => {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from("Writing export bundle..."),
                    Line::from("Please wait"),
                ])
                .block(Block::default().title("Export").borders(Borders::TOP)),
                area,
            );
        }
        ExportFlowStep::Completed => {
            let receipt = flow.receipt.as_ref();
            let lines = vec![
                Line::from("Export complete"),
                Line::from(
                    receipt
                        .map(|receipt| format!("{}", receipt.destination.display()))
                        .unwrap_or_else(|| flow.destination_input.text().to_owned()),
                ),
                Line::from("Enter or Esc close"),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(Block::default().title("Export").borders(Borders::TOP)),
                area,
            );
        }
        ExportFlowStep::Error => {
            let lines = vec![
                Line::from("Export failed"),
                Line::from(
                    flow.error
                        .clone()
                        .unwrap_or_else(|| "unknown error".to_owned()),
                ),
                Line::from("Enter or Esc close"),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(Block::default().title("Export").borders(Borders::TOP)),
                area,
            );
        }
    }
}

fn draw_export_input_step(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    before: Vec<String>,
    prefix: &str,
    input: &ComposerInput,
    after: Vec<String>,
) {
    let prefix_width = u16::try_from(display_width(prefix)).unwrap_or(u16::MAX);
    let input_rows = area
        .height
        .saturating_sub(1)
        .saturating_sub(u16::try_from(before.len() + after.len()).unwrap_or(u16::MAX))
        .max(1);
    let viewport = input.viewport(area.width.saturating_sub(prefix_width).max(1), input_rows);
    let input_start = before.len();
    let mut lines = before
        .into_iter()
        .map(|line| Line::from(truncate_end_to_width(&line, usize::from(area.width))))
        .collect::<Vec<_>>();
    lines.push(Line::from(format!(
        "{prefix}{}",
        viewport.lines.first().cloned().unwrap_or_default()
    )));
    lines.extend(
        viewport
            .lines
            .iter()
            .skip(1)
            .map(|line| Line::from(format!("{}{line}", " ".repeat(usize::from(prefix_width))))),
    );
    lines.extend(
        after
            .into_iter()
            .map(|line| Line::from(truncate_end_to_width(&line, usize::from(area.width)))),
    );
    let cursor_row = input_start.saturating_add(usize::from(viewport.cursor.1));
    let visible = usize::from(area.height.saturating_sub(1)).max(1);
    let scroll = cursor_row.saturating_sub(visible.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(title).borders(Borders::TOP))
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        area,
    );
    if area.width > prefix_width && area.height > 1 {
        frame.set_cursor_position((
            area.x
                .saturating_add(prefix_width)
                .saturating_add(viewport.cursor.0)
                .min(area.right().saturating_sub(1)),
            area.y
                .saturating_add(1)
                .saturating_add(
                    u16::try_from(cursor_row.saturating_sub(scroll)).unwrap_or(u16::MAX),
                )
                .min(area.bottom().saturating_sub(1)),
        ));
    }
}

fn draw_resume_picker_with_title(
    frame: &mut Frame<'_>,
    area: Rect,
    picker: &ResumePickerState,
    current_thread_id: ThreadId,
    title: &str,
    app: &TuiApp,
) {
    let palette = app.palette();
    let visible_count = resume_picker_page_size(area);
    let offset = resume_picker_offset(picker.selected, visible_count, picker.items.len());
    let items = picker
        .items
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_count)
        .map(|(index, item)| {
            let selected = index == picker.selected;
            let current = item.thread_id == current_thread_id;
            let marker = selection_marker(app, selected);
            let current_marker = if current { "current" } else { "" };
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        marker,
                        Style::default().fg(if selected {
                            palette.accent
                        } else {
                            palette.muted
                        }),
                    ),
                    Span::styled(
                        format!("{} ", index + 1),
                        Style::default().fg(palette.muted),
                    ),
                    Span::styled(
                        item.title.clone(),
                        Style::default()
                            .fg(if selected {
                                palette.text
                            } else {
                                palette.subtle
                            })
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::raw("  "),
                    Span::styled(current_marker, Style::default().fg(palette.success)),
                ]),
                Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        short_id(&item.session_id.to_string()),
                        Style::default().fg(palette.muted),
                    ),
                    Span::raw("  "),
                    Span::styled(item.preview.clone(), Style::default().fg(palette.muted)),
                ]),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(Block::default().title(title).borders(Borders::TOP)),
        area,
    );
}

pub(crate) fn resume_picker_offset(
    selected: usize,
    visible_count: usize,
    item_count: usize,
) -> usize {
    if visible_count == 0 || item_count <= visible_count || selected < visible_count {
        return 0;
    }
    let last_window_start = item_count.saturating_sub(visible_count);
    selected
        .saturating_add(1)
        .saturating_sub(visible_count)
        .min(last_window_start)
}

pub(crate) fn resume_picker_page_size(area: Rect) -> usize {
    usize::from(area.height.saturating_sub(1))
        .saturating_div(2)
        .max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OverlayMouseRegion {
    pub(crate) press: UiMousePress,
    pub(crate) area: Rect,
}

pub(crate) fn overlay_mouse_regions(area: Rect, app: &TuiApp) -> Vec<OverlayMouseRegion> {
    let surface = app.overlay_surface();
    if surface == Some(OverlaySurface::Help) {
        return tab_mouse_regions(
            area,
            &HelpTopic::ALL
                .map(|topic| (UiMousePress::Help(topic), help_tab_label(topic, area.width))),
        );
    }
    if surface == Some(OverlaySurface::Auth)
        && let Some(dialog) = &app.auth_dialog
    {
        let scroll = usize::from(auth_scroll_offset(dialog, area));
        return auth_visual_ranges(dialog, area.width)
            .into_iter()
            .filter_map(|(index, start, height)| {
                scrolled_content_mouse_region(area, start, height, scroll).map(|area| {
                    OverlayMouseRegion {
                        press: UiMousePress::Auth(index),
                        area,
                    }
                })
            })
            .collect();
    }
    if surface == Some(OverlaySurface::Resume)
        && let Some(picker) = &app.resume_picker
    {
        return if picker.action.is_none() {
            resume_mouse_regions(area, picker)
        } else {
            Vec::new()
        };
    }
    if surface == Some(OverlaySurface::Queue)
        && let Some(picker) = &app.queue_picker
    {
        let visible_count = usize::from(area.height.saturating_sub(1)).max(1);
        let offset = resume_picker_offset(picker.selected, visible_count, picker.items.len());
        return (offset..picker.items.len().min(offset.saturating_add(visible_count)))
            .filter_map(|index| {
                content_mouse_region(area, index.saturating_sub(offset), 1).map(|area| {
                    OverlayMouseRegion {
                        press: UiMousePress::Queue(index),
                        area,
                    }
                })
            })
            .collect();
    }
    if surface == Some(OverlaySurface::Approval)
        && let Some(dialog) = &app.approval_dialog
    {
        let scroll = usize::from(approval_scroll_offset(dialog, area));
        return approval_visual_ranges(dialog, area.width)
            .into_iter()
            .filter_map(|(index, start, height)| {
                scrolled_content_mouse_region(area, start, height, scroll).map(|area| {
                    OverlayMouseRegion {
                        press: UiMousePress::Approval(ApprovalChoice::ALL[index]),
                        area,
                    }
                })
            })
            .collect();
    }
    if surface == Some(OverlaySurface::Question)
        && let Some(dialog) = &app.question_dialog
    {
        let layout = question_visual_layout(dialog, app, area.width);
        let scroll = usize::from(question_scroll_offset(dialog, app, area));
        let mut regions = Vec::new();
        for (option, start, height) in layout.options {
            if let Some(area) = scrolled_content_mouse_region(area, start, height, scroll) {
                regions.push(OverlayMouseRegion {
                    press: UiMousePress::QuestionOption {
                        question: dialog.question_index,
                        option,
                    },
                    area,
                });
            }
        }
        if let Some(area) =
            scrolled_content_mouse_region(area, layout.free_text.0, layout.free_text.1, scroll)
        {
            regions.push(OverlayMouseRegion {
                press: UiMousePress::QuestionFreeText {
                    question: dialog.question_index,
                },
                area,
            });
        }
        if let Some(area) =
            scrolled_content_mouse_region(area, layout.submit.0, layout.submit.1, scroll)
        {
            regions.push(OverlayMouseRegion {
                press: UiMousePress::QuestionSubmit,
                area,
            });
        }
        return regions;
    }
    if surface == Some(OverlaySurface::Dashboard) {
        return tab_mouse_regions(
            area,
            &DashboardTab::ALL.map(|tab| (UiMousePress::Dashboard(tab), tab.label())),
        );
    }
    if surface == Some(OverlaySurface::Settings)
        && let Some(dialog) = &app.settings_dialog
        && !dialog.editing_model
    {
        let scroll = usize::from(settings_scroll_offset(dialog, area));
        return settings_visual_ranges(dialog, area.width)
            .into_iter()
            .filter_map(|(row, start, height)| {
                scrolled_content_mouse_region(area, start, height, scroll).map(|area| {
                    OverlayMouseRegion {
                        press: UiMousePress::Settings(row),
                        area,
                    }
                })
            })
            .collect();
    }
    if surface == Some(OverlaySurface::Export)
        && let Some(flow) = &app.export_flow
        && flow.step == ExportFlowStep::SelectSession
    {
        return resume_mouse_regions(area, &flow.picker);
    }
    Vec::new()
}

pub(crate) fn overlay_mouse_press_at(
    area: Rect,
    app: &TuiApp,
    x: u16,
    y: u16,
) -> Option<UiMousePress> {
    overlay_mouse_regions(area, app)
        .into_iter()
        .find(|region| rect_contains(region.area, x, y))
        .map(|region| region.press)
}

fn resume_mouse_regions(area: Rect, picker: &ResumePickerState) -> Vec<OverlayMouseRegion> {
    let visible_count = resume_picker_page_size(area);
    let offset = resume_picker_offset(picker.selected, visible_count, picker.items.len());
    let mut row = 0_usize;
    let mut regions = Vec::new();
    for index in offset..picker.items.len().min(offset.saturating_add(visible_count)) {
        let height = 2 + usize::from(picker.show_details && index == picker.selected) * 2;
        if let Some(area) = content_mouse_region(area, row, height) {
            regions.push(OverlayMouseRegion {
                press: UiMousePress::Resume(index),
                area,
            });
        }
        row = row.saturating_add(height);
    }
    regions
}

fn tab_mouse_regions(area: Rect, tabs: &[(UiMousePress, &'static str)]) -> Vec<OverlayMouseRegion> {
    let mut start = 0_usize;
    tabs.iter()
        .enumerate()
        .filter_map(|(index, (press, label))| {
            let width = display_width(&format!(" {} {} ", index + 1, label));
            let region = tab_mouse_region(area, start, width).map(|area| OverlayMouseRegion {
                press: *press,
                area,
            });
            start = start.saturating_add(width).saturating_add(1);
            region
        })
        .collect()
}

fn help_tab_label(topic: HelpTopic, width: u16) -> &'static str {
    if width >= 75 {
        return topic.label();
    }
    match topic {
        HelpTopic::Overview => "All",
        HelpTopic::Composer => "Edit",
        HelpTopic::Navigation => "Nav",
        HelpTopic::Runtime => "Run",
        HelpTopic::WhatsNew => "New",
    }
}

fn tab_mouse_region(area: Rect, start: usize, width: usize) -> Option<Rect> {
    let x = area
        .x
        .saturating_add(u16::try_from(start).unwrap_or(u16::MAX));
    let width = u16::try_from(width).unwrap_or(u16::MAX);
    clipped_mouse_region(area, Rect::new(x, area.y.saturating_add(1), width, 1))
}

fn content_mouse_region(area: Rect, row: usize, height: usize) -> Option<Rect> {
    let y = area
        .y
        .saturating_add(1)
        .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
    clipped_mouse_region(
        area,
        Rect::new(
            area.x,
            y,
            area.width,
            u16::try_from(height).unwrap_or(u16::MAX),
        ),
    )
}

fn scrolled_content_mouse_region(
    area: Rect,
    row: usize,
    height: usize,
    scroll: usize,
) -> Option<Rect> {
    let end = row.saturating_add(height);
    if end <= scroll {
        return None;
    }
    let visible_start = row.max(scroll).saturating_sub(scroll);
    let visible_height = end.saturating_sub(row.max(scroll));
    content_mouse_region(area, visible_start, visible_height)
}

fn clipped_mouse_region(visible: Rect, region: Rect) -> Option<Rect> {
    let clipped = region.intersection(visible);
    (clipped.width > 0 && clipped.height > 0).then_some(clipped)
}

pub(crate) fn draw_bottom_pane(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let composer_prefix = if app.preferences.screen_reader {
        "> "
    } else {
        COMPOSER_PREFIX
    };
    let palette = app.palette();
    let surface = app.overlay_surface();
    let overlay_help = match surface {
        Some(OverlaySurface::Help) => {
            Some("1-5 topic   Tab switch   Up/Down scroll   F1 or Esc close")
        }
        Some(OverlaySurface::Auth) => {
            Some("Provider setup   Enter continue   Esc back   Ctrl+C twice quit")
        }
        Some(OverlaySurface::Approval) => {
            Some("Enter choose   1 once   2 resource   3 task   4 deny")
        }
        Some(OverlaySurface::Question) => Some(
            "Space select   Tab options/notes   Left/Right question   Enter next   Ctrl+S submit",
        ),
        Some(OverlaySurface::Resume) => Some(
            "Type filter   Enter resume   Alt+I details   Alt+R rename   Alt+A archive   Alt+D delete",
        ),
        Some(OverlaySurface::Queue) => {
            Some("Enter edit   Delete cancel prompt   Up/Down select   Esc close")
        }
        Some(OverlaySurface::Dashboard) => {
            Some("1 Plan   2 Tasks   3 Usage   Tab switch   Up/Down scroll   Esc close")
        }
        Some(OverlaySurface::Settings) => {
            Some("Arrows change   Enter edit   Ctrl+S apply   Esc discard")
        }
        Some(OverlaySurface::Export) => Some("Enter continue   Esc cancel   Ctrl+C twice quit"),
        None => None,
    };
    let candidates = app.slash_candidates();
    let mut lines = if surface == Some(OverlaySurface::Help) {
        vec![Line::from(vec![
            Span::styled(composer_prefix, Style::default().fg(palette.accent)),
            Span::styled("Contextual keyboard reference", composer_style(app)),
        ])]
    } else if surface == Some(OverlaySurface::Auth) {
        let dialog = app.auth_dialog.as_ref().expect("auth surface");
        vec![Line::from(vec![
            Span::styled(composer_prefix, Style::default().fg(palette.accent)),
            Span::styled(auth_composer_line(dialog), composer_style(app)),
        ])]
    } else if surface == Some(OverlaySurface::Approval) {
        vec![Line::from(vec![
            Span::styled(composer_prefix, Style::default().fg(palette.accent)),
            Span::styled("Resolve the pending tool request", composer_style(app)),
        ])]
    } else if surface == Some(OverlaySurface::Question) {
        vec![Line::from(vec![
            Span::styled(composer_prefix, Style::default().fg(palette.accent)),
            Span::styled("Answer the agent's question", composer_style(app)),
        ])]
    } else if surface == Some(OverlaySurface::Resume) {
        vec![Line::from(vec![
            Span::styled(composer_prefix, Style::default().fg(palette.accent)),
            Span::styled("Select a session to resume", composer_style(app)),
        ])]
    } else if surface == Some(OverlaySurface::Queue) {
        vec![Line::from(vec![
            Span::styled(composer_prefix, Style::default().fg(palette.accent)),
            Span::styled("Manage queued prompts", composer_style(app)),
        ])]
    } else if surface == Some(OverlaySurface::Dashboard) {
        vec![Line::from(vec![
            Span::styled(composer_prefix, Style::default().fg(palette.accent)),
            Span::styled("Inspect runtime state", composer_style(app)),
        ])]
    } else if surface == Some(OverlaySurface::Settings) {
        vec![Line::from(vec![
            Span::styled(composer_prefix, Style::default().fg(palette.accent)),
            Span::styled("Configure this session", composer_style(app)),
        ])]
    } else if surface == Some(OverlaySurface::Export) {
        vec![Line::from(vec![
            Span::styled(composer_prefix, Style::default().fg(palette.accent)),
            Span::styled("Export session history", composer_style(app)),
        ])]
    } else if let Some(search) = &app.transcript_search {
        vec![Line::from(vec![
            Span::styled("Find: ", Style::default().fg(palette.warning)),
            Span::styled(
                search.input.text().to_owned(),
                Style::default().fg(palette.text),
            ),
            Span::raw("  "),
            Span::styled(search.status(), Style::default().fg(palette.muted)),
        ])]
    } else if let Some(search) = &app.history_search {
        vec![Line::from(vec![
            Span::styled("History: ", Style::default().fg(palette.secondary)),
            Span::styled(
                search.input.text().to_owned(),
                Style::default().fg(palette.text),
            ),
            Span::raw("  "),
            Span::styled(search.status(), Style::default().fg(palette.muted)),
        ])]
    } else {
        let text_width = area.width.saturating_sub(COMPOSER_PREFIX_WIDTH).max(1);
        let viewport = app.input.viewport(text_width, MAX_COMPOSER_ROWS);
        viewport
            .lines
            .into_iter()
            .enumerate()
            .map(|(index, line)| {
                let prefix = if index == 0 { composer_prefix } else { "  " };
                let content = if index == 0 && app.input.is_empty() {
                    "Ask Golutra to change code or inspect the workspace".to_owned()
                } else {
                    line
                };
                Line::from(vec![
                    Span::styled(prefix, Style::default().fg(palette.accent)),
                    Span::styled(content, composer_style(app)),
                ])
            })
            .collect()
    };
    let composer_visible =
        surface.is_none() && app.transcript_search.is_none() && app.history_search.is_none();
    if composer_visible {
        if let Some(completion) = &app.mention_completion {
            lines.extend(mention_candidate_lines(app, completion));
        } else {
            lines.extend(slash_candidate_lines(app, &candidates));
        }
        if !app.attachments.is_empty() {
            lines.push(attachment_line(
                app,
                &app.attachments,
                app.selected_attachment,
                usize::from(area.width),
            ));
        }
        lines.extend(queued_prompt_lines(app, usize::from(area.width)));
    }
    if let Some(provider_line) = provider_footer_line(app) {
        lines.push(Line::from(Span::styled(
            provider_line,
            Style::default().fg(provider_color(app)),
        )));
    }
    if let Some(help) = overlay_help {
        lines.push(Line::from(Span::styled(
            help,
            Style::default().fg(palette.muted),
        )));
    }
    if overlay_help.is_some() {
        lines.push(footer_status_line(app));
    } else {
        lines.push(footer_context_line(app, usize::from(area.width)));
    }
    let status = live_status_line(app, usize::from(area.width));
    let composer_area = if let Some(status) = status {
        if area.height > 0 {
            let status_area = Rect::new(area.x, area.y, area.width, 1);
            frame.render_widget(Paragraph::new(status), status_area);
            Rect::new(
                area.x,
                area.y.saturating_add(1),
                area.width,
                area.height.saturating_sub(1),
            )
        } else {
            area
        }
    } else {
        area
    };
    let paragraph = Paragraph::new(lines).block(Block::default().borders(Borders::TOP));
    frame.render_widget(paragraph, composer_area);
    if let Some((x, y)) = composer_cursor_position(area, app) {
        frame.set_cursor_position((x, y));
    }
}

pub(crate) fn composer_cursor_position(area: Rect, app: &TuiApp) -> Option<(u16, u16)> {
    if area.width <= COMPOSER_PREFIX_WIDTH || area.height <= 1 {
        return None;
    }

    let text_x = area.x.saturating_add(COMPOSER_PREFIX_WIDTH);
    let text_width = area.width.saturating_sub(COMPOSER_PREFIX_WIDTH).max(1);
    let activity_rows = u16::from(live_status_text(app, usize::from(area.width)).is_some());
    let cursor = match app.overlay_surface() {
        Some(OverlaySurface::Auth) => auth_cursor_column(app.auth_dialog.as_ref()?)?,
        Some(_) => return None,
        None if app.transcript_search.is_some() => {
            let search = app.transcript_search.as_ref()?;
            let prefix_width = 6_u16;
            let viewport = search
                .input
                .viewport(text_width.saturating_sub(prefix_width), 1);
            return Some((
                area.x
                    .saturating_add(prefix_width)
                    .saturating_add(viewport.cursor.0),
                area.y.saturating_add(1).saturating_add(activity_rows),
            ));
        }
        None if app.history_search.is_some() => {
            let search = app.history_search.as_ref()?;
            let prefix_width = 9_u16;
            let viewport = search
                .input
                .viewport(text_width.saturating_sub(prefix_width), 1);
            return Some((
                area.x
                    .saturating_add(prefix_width)
                    .saturating_add(viewport.cursor.0),
                area.y.saturating_add(1).saturating_add(activity_rows),
            ));
        }
        None => {
            let viewport = app.input.viewport(text_width, MAX_COMPOSER_ROWS);
            return Some((
                text_x.saturating_add(viewport.cursor.0),
                area.y
                    .saturating_add(1)
                    .saturating_add(activity_rows)
                    .saturating_add(viewport.cursor.1)
                    .min(area.bottom().saturating_sub(1)),
            ));
        }
    };

    Some((
        text_x.saturating_add(cursor.min(text_width.saturating_sub(1))),
        area.y.saturating_add(1),
    ))
}

fn auth_cursor_column(dialog: &AuthDialogState) -> Option<u16> {
    let value = match dialog.step {
        AuthDialogStep::BaseUrl => Some(dialog.base_url.as_str()),
        AuthDialogStep::ApiKey => {
            return Some(dialog.api_key.chars().count().min(u16::MAX as usize) as u16);
        }
        AuthDialogStep::EnvKey => Some(dialog.api_key_env.as_str()),
        AuthDialogStep::Model if dialog.is_custom_model_selected() => Some(dialog.model.as_str()),
        _ => None,
    }?;
    Some(display_width(value).min(u16::MAX as usize) as u16)
}

pub(crate) fn slash_candidate_lines(
    app: &TuiApp,
    candidates: &[SlashCommandCandidate],
) -> Vec<Line<'static>> {
    let palette = app.palette();
    candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let selected = index == app.slash_selected.min(candidates.len().saturating_sub(1));
            let marker = selection_marker(app, selected);
            Line::from(vec![
                Span::styled(
                    marker,
                    Style::default().fg(if selected {
                        palette.accent
                    } else {
                        palette.muted
                    }),
                ),
                Span::styled(
                    candidate.command.clone(),
                    Style::default()
                        .fg(if selected {
                            palette.text
                        } else {
                            palette.subtle
                        })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::raw("  "),
                Span::styled(
                    candidate.description.clone(),
                    Style::default().fg(palette.muted),
                ),
            ])
        })
        .collect()
}

pub(crate) fn mention_candidate_lines(
    app: &TuiApp,
    completion: &MentionCompletion,
) -> Vec<Line<'static>> {
    let palette = app.palette();
    completion
        .candidates
        .iter()
        .take(6)
        .enumerate()
        .map(|(index, candidate)| {
            let selected = index == completion.selected;
            let color = match candidate.kind {
                MentionKind::File => palette.accent,
                MentionKind::Skill => palette.success,
                MentionKind::App => palette.secondary,
            };
            Line::from(vec![
                Span::styled(selection_marker(app, selected), Style::default().fg(color)),
                Span::styled(
                    candidate.insertion.clone(),
                    Style::default()
                        .fg(if selected { palette.text } else { color })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::raw("  "),
                Span::styled(candidate.detail.clone(), Style::default().fg(palette.muted)),
            ])
        })
        .collect()
}

fn attachment_line(
    app: &TuiApp,
    attachments: &[ComposerAttachment],
    selected_attachment: Option<usize>,
    max_width: usize,
) -> Line<'static> {
    let palette = app.palette();
    let value = attachments
        .iter()
        .enumerate()
        .map(|(index, attachment)| {
            let kind = match attachment.kind {
                AttachmentKind::Image => "image",
                AttachmentKind::Text => "text",
                AttachmentKind::Binary => "file",
            };
            let marker = if selected_attachment == Some(index) {
                ">"
            } else {
                ""
            };
            format!("{marker}{kind}:{}", attachment.display_path)
        })
        .collect::<Vec<_>>()
        .join("  ");
    Line::from(vec![
        Span::styled("  attached  ", Style::default().fg(palette.secondary)),
        Span::styled(
            truncate_end_to_width(&value, max_width.saturating_sub(12)),
            Style::default().fg(palette.subtle),
        ),
    ])
}

fn queued_prompt_lines(app: &TuiApp, max_width: usize) -> Vec<Line<'static>> {
    let palette = app.palette();
    queued_prompts(&app.events)
        .into_iter()
        .take(3)
        .enumerate()
        .map(|(index, queued)| {
            let mode = if queued.steer { "steer" } else { "queued" };
            Line::from(vec![
                Span::styled(
                    format!("  {} {}  ", index + 1, mode),
                    Style::default().fg(palette.warning),
                ),
                Span::styled(
                    truncate_end_to_width(&queued.prompt, max_width.saturating_sub(14)),
                    Style::default().fg(palette.subtle),
                ),
            ])
        })
        .collect()
}

pub(crate) fn auth_composer_line(dialog: &AuthDialogState) -> String {
    match dialog.step {
        AuthDialogStep::GroupChoice => "Select provider group".to_owned(),
        AuthDialogStep::ThirdPartyChoice => "Select provider".to_owned(),
        AuthDialogStep::AuthMethod => "Select authentication method".to_owned(),
        AuthDialogStep::Protocol => "Select protocol".to_owned(),
        AuthDialogStep::BaseUrl if dialog.base_url.is_empty() => "Base URL".to_owned(),
        AuthDialogStep::BaseUrl => dialog.base_url.clone(),
        AuthDialogStep::CredentialStore => match dialog.selected {
            1 => "Environment variable".to_owned(),
            _ => "Local disk".to_owned(),
        },
        AuthDialogStep::ApiKey if dialog.api_key.is_empty() => "API key".to_owned(),
        AuthDialogStep::ApiKey => "*".repeat(dialog.api_key.chars().count()),
        AuthDialogStep::EnvKey if dialog.api_key_env.is_empty() => {
            "Environment variable".to_owned()
        }
        AuthDialogStep::EnvKey => dialog.api_key_env.clone(),
        AuthDialogStep::Model if dialog.is_custom_model_selected() && !dialog.model.is_empty() => {
            dialog.model.clone()
        }
        AuthDialogStep::Model => dialog
            .selected_recommended_model()
            .unwrap_or("Custom model")
            .to_owned(),
        AuthDialogStep::AdvancedConfig => match dialog.advanced_selected {
            0 => format!(
                "Thinking: {}",
                if dialog.enable_thinking {
                    "enabled"
                } else {
                    "default"
                }
            ),
            1 => format!(
                "Reasoning effort: {}",
                reasoning_effort_label(dialog.reasoning_effort)
            ),
            2 => {
                if dialog.context_window_size.is_empty() {
                    "Context window".to_owned()
                } else {
                    dialog.context_window_size.clone()
                }
            }
            3 => {
                if dialog.max_tokens.is_empty() {
                    "Max output tokens".to_owned()
                } else {
                    dialog.max_tokens.clone()
                }
            }
            4 => {
                if dialog.custom_headers.is_empty() {
                    "Name=Value; X-Api-Key=@ENV".to_owned()
                } else {
                    dialog.custom_headers.clone()
                }
            }
            _ => "Advanced config".to_owned(),
        },
        AuthDialogStep::Review => "Review install plan".to_owned(),
    }
}

pub(crate) fn footer_context_line(app: &TuiApp, max_width: usize) -> Line<'static> {
    const INDENT_WIDTH: usize = 2;
    let indent_width = INDENT_WIDTH.min(max_width);
    let indent = " ".repeat(indent_width);
    let content_width = max_width.saturating_sub(indent_width);
    let (text, style) = if app.quit_shortcut_is_active() && !app.status_message.trim().is_empty() {
        (
            app.status_message.clone(),
            Style::default().fg(app.palette().warning),
        )
    } else {
        (
            footer_context_text(app, content_width),
            Style::default().fg(app.palette().muted),
        )
    };
    let text = truncate_end_to_width(&text, content_width);
    let padding = " ".repeat(content_width.saturating_sub(display_width(&text)));
    Line::from(vec![
        Span::raw(indent),
        Span::styled(text, style),
        Span::raw(padding),
    ])
}

pub(crate) fn footer_context_text(app: &TuiApp, max_width: usize) -> String {
    let model = if app.runtime_controls.effective_model().trim().is_empty() {
        "unconfigured"
    } else {
        app.runtime_controls.effective_model().trim()
    };
    let effort = effort_label(app.runtime_controls.reasoning_effort);
    let mut model = match (app.runtime_controls.permission_mode, effort) {
        (PermissionMode::Unrestricted, "default") => format!("[unrestricted] {model}"),
        (PermissionMode::Unrestricted, effort) => {
            format!("[unrestricted] {model} {effort}")
        }
        (PermissionMode::Guarded, "default") => model.to_owned(),
        (PermissionMode::Guarded, effort) => format!("{model} {effort}"),
    };
    if let Some(mode) = app.composer_mode.label() {
        model = format!("[{mode}] {model}");
    }
    let workspace = workspace_path_label(&app.workspace_path);
    fit_model_and_workspace(&model, &workspace, max_width)
}

pub(crate) fn fit_model_and_workspace(model: &str, workspace: &str, max_width: usize) -> String {
    const SEPARATOR: &str = " · ";
    let full = format!("{model}{SEPARATOR}{workspace}");
    if display_width(&full) <= max_width {
        return full;
    }
    let separator_width = display_width(SEPARATOR);
    if max_width <= separator_width + 2 {
        return truncate_end_to_width(&full, max_width);
    }
    let minimum_workspace_width = 8.min(max_width / 2);
    let model_budget = max_width
        .saturating_sub(separator_width)
        .saturating_sub(minimum_workspace_width)
        .max(1);
    let model = truncate_end_to_width(model, model_budget);
    let workspace_budget = max_width
        .saturating_sub(display_width(&model))
        .saturating_sub(separator_width);
    let workspace = truncate_start_to_width(workspace, workspace_budget);
    format!("{model}{SEPARATOR}{workspace}")
}

pub(crate) fn workspace_path_label(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    workspace_path_label_with_home(path, home.as_deref())
}

pub(crate) fn workspace_path_label_with_home(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home
        && let Ok(relative) = path.strip_prefix(home)
    {
        if relative.as_os_str().is_empty() {
            return "~".to_owned();
        }
        return format!("~{}{}", std::path::MAIN_SEPARATOR, relative.display());
    }
    path.display().to_string()
}

pub(crate) fn truncate_end_to_width(value: &str, max_width: usize) -> String {
    if display_width(value) <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut result = String::new();
    for character in value.chars() {
        let mut candidate = result.clone();
        candidate.push(character);
        if display_width(&candidate) + 1 > max_width {
            break;
        }
        result.push(character);
    }
    result.push('…');
    result
}

pub(crate) fn truncate_start_to_width(value: &str, max_width: usize) -> String {
    if display_width(value) <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut reversed = String::new();
    for character in value.chars().rev() {
        let mut candidate = reversed.clone();
        candidate.push(character);
        let suffix = candidate.chars().rev().collect::<String>();
        if display_width(&suffix) + 1 > max_width {
            break;
        }
        reversed.push(character);
    }
    format!("…{}", reversed.chars().rev().collect::<String>())
}

pub(crate) fn display_width(value: &str) -> usize {
    Line::from(value).width()
}

pub(crate) const fn selection_marker(app: &TuiApp, selected: bool) -> &'static str {
    if !selected {
        "  "
    } else if app.preferences.screen_reader {
        "> "
    } else {
        "› "
    }
}

pub(crate) fn footer_status_line(app: &TuiApp) -> Line<'static> {
    let detail = footer_status_detail(app);
    let spans = if detail.is_empty() {
        vec![Span::styled(
            status_chip(app),
            Style::default().fg(status_color(app)),
        )]
    } else {
        vec![
            Span::styled(status_chip(app), Style::default().fg(status_color(app))),
            Span::raw("  "),
            Span::styled(detail, Style::default().fg(app.palette().muted)),
        ]
    };
    Line::from(spans)
}

pub(crate) fn footer_status_detail(app: &TuiApp) -> String {
    if app.overlay_surface() == Some(OverlaySurface::Auth) {
        return "connect provider".to_owned();
    }
    if app.status_message.trim().is_empty() {
        return match app.projection.as_ref().map(|projection| projection.status) {
            Some(golutra_core::TaskStatus::Idle) | None => "new session".to_owned(),
            _ => String::new(),
        };
    }
    if matches!(
        app.projection.as_ref().map(|projection| projection.status),
        Some(golutra_core::TaskStatus::Running) | Some(golutra_core::TaskStatus::Completed)
    ) && app.status_message == "task started"
    {
        String::new()
    } else {
        app.status_message.clone()
    }
}

pub(crate) fn provider_footer_line(app: &TuiApp) -> Option<String> {
    if app.auth_dialog.is_some() {
        return None;
    }
    if app.provider_message == "ready (mock)" || app.provider_message.starts_with("ready (") {
        None
    } else {
        Some(app.provider_message.clone())
    }
}

pub(crate) fn transcript_visible_window(
    total_rows: usize,
    visible_rows: usize,
    scroll_offset: usize,
) -> std::ops::Range<usize> {
    if total_rows == 0 || visible_rows == 0 {
        return 0..0;
    }
    let visible_rows = visible_rows.min(total_rows);
    let max_offset = total_rows.saturating_sub(visible_rows);
    let offset = scroll_offset.min(max_offset);
    let end = total_rows.saturating_sub(offset);
    end.saturating_sub(visible_rows)..end
}

pub(crate) fn transcript_scroll_status(scroll_offset: usize, unseen_rows: usize) -> String {
    if scroll_offset == 0 {
        "history at latest".to_owned()
    } else if unseen_rows > 0 {
        format!("history offset {scroll_offset} rows from latest · {unseen_rows} new")
    } else {
        format!("history offset {scroll_offset} rows from latest")
    }
}

pub(crate) fn status_chip(app: &TuiApp) -> &'static str {
    if let Some(surface) = app.overlay_surface() {
        return match surface {
            OverlaySurface::Help => "help",
            OverlaySurface::Auth => "auth",
            OverlaySurface::Approval => "approval",
            OverlaySurface::Question => "question",
            OverlaySurface::Resume => "resume",
            OverlaySurface::Queue => "queue",
            OverlaySurface::Dashboard => "dashboard",
            OverlaySurface::Settings => "settings",
            OverlaySurface::Export => "export",
        };
    }
    match app.projection.as_ref().map(|projection| projection.status) {
        Some(golutra_core::TaskStatus::Running) => "running",
        Some(golutra_core::TaskStatus::WaitingApproval) => "waiting approval",
        Some(golutra_core::TaskStatus::WaitingAuthentication) => "auth required",
        Some(golutra_core::TaskStatus::Completed) => "complete",
        Some(golutra_core::TaskStatus::Failed) => "failed",
        Some(golutra_core::TaskStatus::Blocked) => "blocked",
        Some(golutra_core::TaskStatus::Cancelled) => "cancelled",
        Some(golutra_core::TaskStatus::Interrupted) => "interrupted",
        Some(golutra_core::TaskStatus::Uncertain) => "uncertain",
        Some(golutra_core::TaskStatus::Aborting) => "aborting",
        Some(golutra_core::TaskStatus::Paused) => "paused",
        Some(golutra_core::TaskStatus::Pausing) => "pausing",
        Some(golutra_core::TaskStatus::Partial) => "partial",
        Some(golutra_core::TaskStatus::Idle) | None => "ready",
    }
}

pub(crate) fn status_color(app: &TuiApp) -> Color {
    let palette = app.palette();
    if app.overlay_surface().is_some() {
        return palette.accent;
    }
    match app.projection.as_ref().map(|projection| projection.status) {
        Some(golutra_core::TaskStatus::Running) => palette.accent,
        Some(golutra_core::TaskStatus::Completed) => palette.success,
        Some(golutra_core::TaskStatus::Failed)
        | Some(golutra_core::TaskStatus::Blocked)
        | Some(golutra_core::TaskStatus::Cancelled)
        | Some(golutra_core::TaskStatus::Interrupted)
        | Some(golutra_core::TaskStatus::Uncertain) => palette.error,
        Some(golutra_core::TaskStatus::WaitingApproval)
        | Some(golutra_core::TaskStatus::WaitingAuthentication)
        | Some(golutra_core::TaskStatus::Aborting)
        | Some(golutra_core::TaskStatus::Pausing)
        | Some(golutra_core::TaskStatus::Partial) => palette.warning,
        Some(golutra_core::TaskStatus::Paused) => palette.secondary,
        Some(golutra_core::TaskStatus::Idle) | None => palette.muted,
    }
}

pub(crate) fn provider_color(app: &TuiApp) -> Color {
    let palette = app.palette();
    if app.provider_message.contains("ready") {
        palette.success
    } else if app.provider_message.contains("missing") || app.provider_message.contains("setup") {
        palette.warning
    } else {
        palette.muted
    }
}

pub(crate) fn has_active_task(app: &TuiApp) -> bool {
    matches!(
        app.projection.as_ref().map(|projection| projection.status),
        Some(golutra_core::TaskStatus::Running)
            | Some(golutra_core::TaskStatus::WaitingApproval)
            | Some(golutra_core::TaskStatus::WaitingAuthentication)
            | Some(golutra_core::TaskStatus::Aborting)
            | Some(golutra_core::TaskStatus::Pausing)
            | Some(golutra_core::TaskStatus::Paused)
    )
}

pub(crate) fn composer_style(app: &TuiApp) -> Style {
    let palette = app.palette();
    if app.overlay_surface() == Some(OverlaySurface::Auth)
        && let Some(dialog) = &app.auth_dialog
    {
        let empty = match dialog.step {
            AuthDialogStep::GroupChoice
            | AuthDialogStep::ThirdPartyChoice
            | AuthDialogStep::AuthMethod
            | AuthDialogStep::Protocol
            | AuthDialogStep::CredentialStore => true,
            AuthDialogStep::BaseUrl => dialog.base_url.is_empty(),
            AuthDialogStep::ApiKey => dialog.api_key.is_empty(),
            AuthDialogStep::EnvKey => dialog.api_key_env.is_empty(),
            AuthDialogStep::Model => dialog.is_custom_model_selected() && dialog.model.is_empty(),
            AuthDialogStep::AdvancedConfig => false,
            AuthDialogStep::Review => false,
        };
        return if empty {
            Style::default().fg(palette.muted)
        } else {
            Style::default().fg(palette.text)
        };
    }
    if app.input.is_empty() {
        Style::default().fg(palette.muted)
    } else {
        Style::default().fg(palette.text)
    }
}

pub(crate) fn short_id(value: &str) -> String {
    if value.len() <= 12 {
        value.to_owned()
    } else {
        format!("{}...", &value[..12])
    }
}

pub(crate) fn compact_ack_reason(reason: &Option<String>) -> String {
    match reason.as_deref() {
        Some(value) if value.starts_with("started task ") => "task started".to_owned(),
        Some(value) if value.starts_with("session already has an active") => {
            "session already has an active task".to_owned()
        }
        Some(value) => value.to_owned(),
        None => "prompt accepted".to_owned(),
    }
}
