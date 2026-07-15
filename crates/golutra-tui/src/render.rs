//! TUI 的布局与 transcript 投影。

use std::collections::HashSet;

use crossterm::terminal::size;
use golutra_protocol::VisibleStep;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};

use super::*;

const COMPOSER_PREFIX: &str = "› ";
const COMPOSER_PREFIX_WIDTH: u16 = 2;
const MAX_COMPOSER_ROWS: u16 = 5;

pub(crate) fn draw_ui(frame: &mut Frame<'_>, app: &TuiApp) {
    let bottom_height = bottom_pane_height_for_width(app, frame.area().width);
    let constraints = if app.debug_mode {
        vec![
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(10),
            Constraint::Length(bottom_height),
        ]
    } else {
        vec![
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(bottom_height),
        ]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    draw_header(frame, chunks[0], app);
    draw_transcript(frame, chunks[1], app);
    if app.debug_mode {
        draw_developer_panel(frame, chunks[2], app);
        draw_bottom_pane(frame, chunks[3], app);
    } else {
        draw_bottom_pane(frame, chunks[2], app);
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn bottom_pane_height(app: &TuiApp) -> u16 {
    let width = size().map(|(width, _)| width).unwrap_or(80);
    bottom_pane_height_for_width(app, width)
}

pub(crate) fn bottom_pane_height_for_width(app: &TuiApp, width: u16) -> u16 {
    let slash_rows = app.slash_candidates().len() as u16;
    let overlay_rows = u16::from(app.auth_dialog.is_some() || app.resume_picker.is_some());
    let provider_rows = u16::from(provider_footer_line(app).is_some());
    let composer_rows = if app.auth_dialog.is_some() || app.resume_picker.is_some() {
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
    2 + composer_rows + slash_rows + overlay_rows + provider_rows
}

pub(crate) fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let mode = header_mode(app);
    let lines = vec![Line::from(vec![
        Span::styled(
            "Golutra",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(mode, Style::default().fg(Color::DarkGray)),
    ])];
    let paragraph = Paragraph::new(lines).alignment(Alignment::Left);
    frame.render_widget(paragraph, area);
}

pub(crate) fn header_mode(app: &TuiApp) -> String {
    if app.auth_dialog.is_some() {
        return "  auth".to_owned();
    }
    if app.resume_picker.is_some() {
        return "  resume".to_owned();
    }
    if app.debug_mode {
        return "  developer".to_owned();
    }
    match app.projection.as_ref().map(|projection| projection.status) {
        Some(golutra_core::TaskStatus::Running) => "  running".to_owned(),
        Some(golutra_core::TaskStatus::WaitingApproval) => "  waiting".to_owned(),
        Some(golutra_core::TaskStatus::Failed) => "  failed".to_owned(),
        Some(golutra_core::TaskStatus::Blocked) => "  blocked".to_owned(),
        Some(golutra_core::TaskStatus::Completed) => "  complete".to_owned(),
        _ => String::new(),
    }
}

pub(crate) fn draw_transcript(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    if let Some(dialog) = &app.auth_dialog {
        draw_auth_dialog(frame, area, dialog);
        return;
    }
    if let Some(picker) = &app.resume_picker {
        draw_resume_picker(frame, area, picker, app.thread_id);
        return;
    }

    let mut items = transcript_rows(app);
    if items.is_empty() {
        return;
    }
    let visible_rows = area.height.saturating_sub(1) as usize;
    let window = transcript_visible_window(items.len(), visible_rows, app.transcript_scroll_offset);
    if window.end < items.len() {
        items.drain(window.end..);
    }
    if window.start > 0 {
        items.drain(..window.start);
    }
    let list = List::new(items).block(Block::default().borders(Borders::TOP));
    frame.render_widget(list, area);
}

pub(crate) fn draw_auth_dialog(frame: &mut Frame<'_>, area: Rect, dialog: &AuthDialogState) {
    let lines = match dialog.step {
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
    };
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title("Provider setup")
                .borders(Borders::TOP),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
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
        Line::from(""),
        Line::from(Span::styled(
            "Up/Down select   Space toggle/cycle   Type digits for numeric fields   Enter continue   Esc back",
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
) {
    let visible_rows = area.height.saturating_sub(1) as usize;
    let visible_count = visible_rows.max(1);
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
            let marker = if selected { "> " } else { "  " };
            let current_marker = if current { "current" } else { "" };
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        marker,
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
                        item.title.clone(),
                        Style::default()
                            .fg(if selected { Color::White } else { Color::Gray })
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::raw("  "),
                    Span::styled(current_marker, Style::default().fg(Color::Green)),
                ]),
                Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        short_id(&item.session_id.to_string()),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw("  "),
                    Span::styled(item.preview.clone(), Style::default().fg(Color::DarkGray)),
                ]),
            ])
        })
        .collect::<Vec<_>>();
    let list = List::new(items).block(
        Block::default()
            .title("Resume session")
            .borders(Borders::TOP),
    );
    frame.render_widget(list, area);
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

pub(crate) fn draw_developer_panel(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let content_width = usize::from(area.width.saturating_sub(2));
    let rows = if let Some(error) = &app.developer_error {
        vec![DeveloperPanelRow::Summary(format!("error {}", error))]
    } else if let Some(projection) = &app.developer_projection {
        developer_panel_rows(projection, 4)
    } else {
        vec![DeveloperPanelRow::Summary(
            "loading developer projection".to_owned(),
        )]
    };
    let items = rows
        .into_iter()
        .map(|row| match row {
            DeveloperPanelRow::Summary(summary) => ListItem::new(Line::from(Span::styled(
                truncate_end_to_width(&summary, content_width),
                Style::default().fg(Color::Cyan),
            ))),
            DeveloperPanelRow::Event {
                sequence_no,
                label,
                summary,
            } => {
                let sequence = format!("#{sequence_no} ");
                let summary_width = content_width
                    .saturating_sub(display_width(&sequence))
                    .saturating_sub(display_width(&label))
                    .saturating_sub(2);
                ListItem::new(Line::from(vec![
                    Span::styled(sequence, Style::default().fg(Color::DarkGray)),
                    Span::styled(label, Style::default().fg(Color::Yellow)),
                    Span::styled("  ", Style::default()),
                    Span::raw(truncate_end_to_width(&summary, summary_width)),
                ]))
            }
        })
        .collect::<Vec<_>>();
    let list = List::new(items).block(
        Block::default()
            .title("Developer runtime")
            .borders(Borders::TOP),
    );
    frame.render_widget(list, area);
}

pub(crate) fn draw_bottom_pane(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let overlay_help = if app.auth_dialog.is_some() {
        Some("Provider setup   Enter continue   Esc back   Ctrl+C twice quit")
    } else if app.resume_picker.is_some() {
        Some("Enter resume   Up/Down select   Esc cancel   Ctrl+C twice quit")
    } else {
        None
    };
    let candidates = app.slash_candidates();
    let mut lines = if let Some(dialog) = &app.auth_dialog {
        vec![Line::from(vec![
            Span::styled(COMPOSER_PREFIX, Style::default().fg(Color::Cyan)),
            Span::styled(auth_composer_line(dialog), composer_style(app)),
        ])]
    } else if app.resume_picker.is_some() {
        vec![Line::from(vec![
            Span::styled(COMPOSER_PREFIX, Style::default().fg(Color::Cyan)),
            Span::styled("Select a session to resume", composer_style(app)),
        ])]
    } else {
        let text_width = area.width.saturating_sub(COMPOSER_PREFIX_WIDTH).max(1);
        let viewport = app.input.viewport(text_width, MAX_COMPOSER_ROWS);
        viewport
            .lines
            .into_iter()
            .enumerate()
            .map(|(index, line)| {
                let prefix = if index == 0 { COMPOSER_PREFIX } else { "  " };
                let content = if index == 0 && app.input.is_empty() {
                    "Ask Golutra to change code or inspect the workspace".to_owned()
                } else {
                    line
                };
                Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Cyan)),
                    Span::styled(content, composer_style(app)),
                ])
            })
            .collect()
    };
    lines.extend(slash_candidate_lines(app, &candidates));
    if overlay_help.is_some() {
        lines.push(footer_status_line(app));
    } else {
        lines.push(footer_context_line(app, usize::from(area.width)));
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
            Style::default().fg(Color::DarkGray),
        )));
    }
    let paragraph = Paragraph::new(lines).block(Block::default().borders(Borders::TOP));
    frame.render_widget(paragraph, area);
    if let Some((x, y)) = composer_cursor_position(area, app) {
        frame.set_cursor_position((x, y));
    }
}

pub(crate) fn composer_cursor_position(area: Rect, app: &TuiApp) -> Option<(u16, u16)> {
    if app.resume_picker.is_some() || area.width <= COMPOSER_PREFIX_WIDTH || area.height <= 1 {
        return None;
    }

    let text_x = area.x.saturating_add(COMPOSER_PREFIX_WIDTH);
    let text_width = area.width.saturating_sub(COMPOSER_PREFIX_WIDTH).max(1);
    let cursor = if app.auth_dialog.is_some() {
        auth_cursor_column(app.auth_dialog.as_ref()?)?
    } else {
        let viewport = app.input.viewport(text_width, MAX_COMPOSER_ROWS);
        return Some((
            text_x.saturating_add(viewport.cursor.0),
            area.y
                .saturating_add(1)
                .saturating_add(viewport.cursor.1)
                .min(area.bottom().saturating_sub(1)),
        ));
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
    candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let selected = index == app.slash_selected.min(candidates.len().saturating_sub(1));
            let marker = if selected { "› " } else { "  " };
            Line::from(vec![
                Span::styled(
                    marker,
                    Style::default().fg(if selected {
                        Color::Cyan
                    } else {
                        Color::DarkGray
                    }),
                ),
                Span::styled(
                    candidate.command.clone(),
                    Style::default()
                        .fg(if selected { Color::White } else { Color::Gray })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::raw("  "),
                Span::styled(
                    candidate.description.clone(),
                    Style::default().fg(Color::DarkGray),
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
            _ => "Advanced config".to_owned(),
        },
        AuthDialogStep::Review => "Review install plan".to_owned(),
    }
}

pub(crate) fn footer_context_line(app: &TuiApp, max_width: usize) -> Line<'static> {
    const INDENT_WIDTH: usize = 2;
    let indent_width = INDENT_WIDTH.min(max_width);
    let content_width = max_width.saturating_sub(indent_width);
    let indent = " ".repeat(indent_width);
    if app.quit_shortcut_is_active() && !app.status_message.trim().is_empty() {
        return Line::from(vec![
            Span::raw(indent),
            Span::styled(
                truncate_end_to_width(&app.status_message, content_width),
                Style::default().fg(Color::Yellow),
            ),
        ]);
    }
    Line::from(vec![
        Span::raw(indent),
        Span::styled(
            footer_context_text(app, content_width),
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

pub(crate) fn footer_context_text(app: &TuiApp, max_width: usize) -> String {
    let model = if app.provider_model.trim().is_empty() {
        "unconfigured"
    } else {
        app.provider_model.trim()
    };
    let workspace = workspace_path_label(&app.workspace_path);
    fit_model_and_workspace(model, &workspace, max_width)
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
            Span::styled(detail, Style::default().fg(Color::DarkGray)),
        ]
    };
    Line::from(spans)
}

pub(crate) fn footer_status_detail(app: &TuiApp) -> String {
    if app.auth_dialog.is_some() {
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

pub(crate) fn transcript_items(app: &TuiApp) -> Vec<TranscriptItem> {
    if app.auth_dialog.is_some() {
        return Vec::new();
    }
    let mut items = Vec::new();
    let event_items = event_transcript_items(&app.events);
    let has_event_items = !event_items.is_empty();
    items.extend(event_items);
    items.extend(app.command_messages.clone());
    if let Some(projection) = &app.projection {
        if has_event_items {
            items.extend(projection_overlay_items(projection));
        } else {
            items.extend(projection_items(projection));
        }
    } else {
        items.push(TranscriptItem {
            role: TranscriptRole::System,
            title: "Connecting".to_owned(),
            body: vec!["loading runtime state".to_owned()],
        });
    }

    items
}

pub(crate) fn transcript_rows(app: &TuiApp) -> Vec<ListItem<'static>> {
    transcript_items(app)
        .into_iter()
        .flat_map(transcript_list_items)
        .collect()
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

pub(crate) fn transcript_page_rows(app: &TuiApp) -> usize {
    let (terminal_width, terminal_height) = size().unwrap_or((80, 24));
    usize::from(
        terminal_height
            .saturating_sub(1)
            .saturating_sub(bottom_pane_height_for_width(app, terminal_width))
            .saturating_sub(1)
            .max(1),
    )
}

pub(crate) fn transcript_scroll_status(scroll_offset: usize) -> String {
    if scroll_offset == 0 {
        "history at latest".to_owned()
    } else {
        format!("history offset {scroll_offset} rows from latest")
    }
}

pub(crate) fn event_transcript_items(events: &[Value]) -> Vec<TranscriptItem> {
    let mut typed_events = events
        .iter()
        .filter_map(|value| serde_json::from_value::<RuntimeEvent>(value.clone()).ok())
        .collect::<Vec<_>>();
    typed_events.sort_by_key(|event| event.sequence_no);

    let mut items = Vec::new();
    let mut visible_user_turns = HashSet::new();
    for event in typed_events {
        match event.event_type {
            RuntimeEventType::TaskCreated | RuntimeEventType::TurnQueued => {
                let is_new_turn = event
                    .turn_id
                    .is_none_or(|turn_id| visible_user_turns.insert(turn_id));
                if is_new_turn && let Some(item) = user_event_transcript_item(&event) {
                    items.push(item);
                }
            }
            RuntimeEventType::AssistantMessage => {
                if let Some(item) = assistant_event_transcript_item(&event) {
                    items.push(item);
                }
            }
            _ => {
                if let Some(item) = status_event_transcript_item(&event) {
                    items.push(item);
                }
            }
        }
    }
    items
}

pub(crate) fn user_event_transcript_item(event: &RuntimeEvent) -> Option<TranscriptItem> {
    event
        .payload
        .get("payload")
        .and_then(|payload| payload.get("prompt"))
        .and_then(Value::as_str)
        .filter(|prompt| !prompt.trim().is_empty())
        .map(|prompt| TranscriptItem {
            role: TranscriptRole::User,
            title: "You".to_owned(),
            body: vec![prompt.to_owned()],
        })
}

pub(crate) fn assistant_event_transcript_item(event: &RuntimeEvent) -> Option<TranscriptItem> {
    event
        .payload
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| !content.trim().is_empty())
        .map(|content| TranscriptItem {
            role: TranscriptRole::Assistant,
            title: "Golutra".to_owned(),
            body: vec![content.to_owned()],
        })
}

pub(crate) fn status_event_transcript_item(event: &RuntimeEvent) -> Option<TranscriptItem> {
    if event.event_type == RuntimeEventType::ApprovalRequested {
        let request = event.payload.get("request")?;
        let tool_name = request
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("tool");
        let resource = request
            .get("resource")
            .and_then(Value::as_str)
            .unwrap_or("unknown resource");
        let reason = request
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("explicit approval is required");
        return Some(TranscriptItem {
            role: TranscriptRole::Status,
            title: "Approval required".to_owned(),
            body: vec![format!("{tool_name}: {resource}"), reason.to_owned()],
        });
    }
    if event.event_type == RuntimeEventType::TaskCompleted
        && event
            .payload
            .get("status")
            .cloned()
            .and_then(|status| serde_json::from_value::<golutra_core::TaskStatus>(status).ok())
            == Some(golutra_core::TaskStatus::Completed)
    {
        return None;
    }
    let title = event_status_title(event.event_type)?;
    let summary = event_summary(event)?;
    if event.event_type == RuntimeEventType::LoopDecided
        && !summary.contains("failed")
        && !summary.contains("error")
    {
        return None;
    }
    Some(TranscriptItem {
        role: TranscriptRole::Status,
        title: title.to_owned(),
        body: vec![summary],
    })
}

pub(crate) fn event_status_title(event_type: RuntimeEventType) -> Option<&'static str> {
    match event_type {
        RuntimeEventType::ToolCompleted => Some("Tool Completed"),
        RuntimeEventType::TaskCompleted => Some("Task Completed"),
        RuntimeEventType::CommandRejected => Some("Command Rejected"),
        RuntimeEventType::ControllerChanged => Some("Controller Changed"),
        RuntimeEventType::LoopDecided => Some("Loop Decided"),
        _ => None,
    }
}

pub(crate) fn event_summary(event: &RuntimeEvent) -> Option<String> {
    event
        .payload
        .get("summary")
        .and_then(Value::as_str)
        .map_or_else(
            || {
                event
                    .payload
                    .get("error")
                    .and_then(Value::as_str)
                    .map(|error| {
                        if error.trim().is_empty() {
                            "runtime event recorded".to_owned()
                        } else {
                            error.to_owned()
                        }
                    })
            },
            |summary| {
                if summary.trim().is_empty() {
                    None
                } else {
                    Some(summary.to_owned())
                }
            },
        )
}

pub(crate) fn projection_items(projection: &UserProjection) -> Vec<TranscriptItem> {
    let mut items = projection
        .visible_steps
        .iter()
        .filter(|step| significant_step(step))
        .map(step_item)
        .collect::<Vec<_>>();

    if let Some(pending_approval) = &projection.pending_approval {
        items.push(TranscriptItem {
            role: TranscriptRole::Status,
            title: "Approval required".to_owned(),
            body: vec![pending_approval.to_owned()],
        });
    }

    if let Some(final_message) = &projection.final_message {
        items.push(TranscriptItem {
            role: TranscriptRole::Assistant,
            title: "Golutra".to_owned(),
            body: vec![final_message.to_owned()],
        });
    }

    if !projection.residual_risks.is_empty() {
        items.push(TranscriptItem {
            role: TranscriptRole::Status,
            title: "Residual risks".to_owned(),
            body: projection.residual_risks.clone(),
        });
    }
    items
}

pub(crate) fn projection_overlay_items(projection: &UserProjection) -> Vec<TranscriptItem> {
    let mut items = Vec::new();
    if !projection.residual_risks.is_empty() {
        items.push(TranscriptItem {
            role: TranscriptRole::Status,
            title: "Residual risks".to_owned(),
            body: projection.residual_risks.clone(),
        });
    }
    items
}

pub(crate) fn significant_step(step: &VisibleStep) -> bool {
    matches!(step.label.as_str(), "ToolCompleted" | "CommandRejected")
        || (step.label == "TaskCompleted" && step.status != "Completed")
        || (step.label == "LoopDecided"
            && (step.summary.contains("failed") || step.summary.contains("error")))
}

pub(crate) fn step_item(step: &VisibleStep) -> TranscriptItem {
    TranscriptItem {
        role: TranscriptRole::Status,
        title: readable_step_label(&step.label),
        body: vec![format!("{} - {}", step.status, step.summary)],
    }
}

pub(crate) fn transcript_list_items(item: TranscriptItem) -> Vec<ListItem<'static>> {
    let color = role_color(&item.role);
    let mut rows = vec![ListItem::new(Line::from(vec![
        Span::styled(role_marker(&item.role), Style::default().fg(color)),
        Span::styled(
            item.title,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ]))];
    rows.extend(item.body.into_iter().map(|line| {
        ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(line, Style::default().fg(Color::White)),
        ]))
    }));
    rows.push(ListItem::new(Line::from("")));
    rows
}

pub(crate) fn readable_step_label(label: &str) -> String {
    label
        .chars()
        .enumerate()
        .fold(String::new(), |mut output, (index, character)| {
            if index > 0 && character.is_uppercase() {
                output.push(' ');
            }
            output.push(character);
            output
        })
}

pub(crate) fn role_marker(role: &TranscriptRole) -> &'static str {
    match role {
        TranscriptRole::User => "› ",
        TranscriptRole::Assistant | TranscriptRole::Status | TranscriptRole::System => "• ",
    }
}

pub(crate) fn role_color(role: &TranscriptRole) -> Color {
    match role {
        TranscriptRole::User => Color::Cyan,
        TranscriptRole::Assistant => Color::Green,
        TranscriptRole::Status => Color::Yellow,
        TranscriptRole::System => Color::DarkGray,
    }
}

pub(crate) fn status_chip(app: &TuiApp) -> &'static str {
    if app.auth_dialog.is_some() {
        return "auth";
    }
    if app.resume_picker.is_some() {
        return "resume";
    }
    match app.projection.as_ref().map(|projection| projection.status) {
        Some(golutra_core::TaskStatus::Running) => "running",
        Some(golutra_core::TaskStatus::WaitingApproval) => "waiting approval",
        Some(golutra_core::TaskStatus::Completed) => "complete",
        Some(golutra_core::TaskStatus::Failed) => "failed",
        Some(golutra_core::TaskStatus::Blocked) => "blocked",
        Some(golutra_core::TaskStatus::Cancelled) => "cancelled",
        Some(golutra_core::TaskStatus::Aborting) => "aborting",
        Some(golutra_core::TaskStatus::Paused) => "paused",
        Some(golutra_core::TaskStatus::Pausing) => "pausing",
        Some(golutra_core::TaskStatus::Partial) => "partial",
        Some(golutra_core::TaskStatus::Idle) | None => "ready",
    }
}

pub(crate) fn status_color(app: &TuiApp) -> Color {
    if app.auth_dialog.is_some() || app.resume_picker.is_some() {
        return Color::Cyan;
    }
    match app.projection.as_ref().map(|projection| projection.status) {
        Some(golutra_core::TaskStatus::Running) => Color::Cyan,
        Some(golutra_core::TaskStatus::Completed) => Color::Green,
        Some(golutra_core::TaskStatus::Failed)
        | Some(golutra_core::TaskStatus::Blocked)
        | Some(golutra_core::TaskStatus::Cancelled) => Color::Red,
        Some(golutra_core::TaskStatus::WaitingApproval)
        | Some(golutra_core::TaskStatus::Aborting)
        | Some(golutra_core::TaskStatus::Pausing)
        | Some(golutra_core::TaskStatus::Partial) => Color::Yellow,
        Some(golutra_core::TaskStatus::Paused) => Color::Magenta,
        Some(golutra_core::TaskStatus::Idle) | None => Color::DarkGray,
    }
}

pub(crate) fn provider_color(app: &TuiApp) -> Color {
    if app.provider_message.contains("ready") {
        Color::Green
    } else if app.provider_message.contains("missing") || app.provider_message.contains("setup") {
        Color::Yellow
    } else {
        Color::DarkGray
    }
}

pub(crate) fn has_active_task(app: &TuiApp) -> bool {
    matches!(
        app.projection.as_ref().map(|projection| projection.status),
        Some(golutra_core::TaskStatus::Running)
            | Some(golutra_core::TaskStatus::WaitingApproval)
            | Some(golutra_core::TaskStatus::Aborting)
            | Some(golutra_core::TaskStatus::Pausing)
            | Some(golutra_core::TaskStatus::Paused)
    )
}

pub(crate) fn composer_style(app: &TuiApp) -> Style {
    if let Some(dialog) = &app.auth_dialog {
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
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };
    }
    if app.input.is_empty() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
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
