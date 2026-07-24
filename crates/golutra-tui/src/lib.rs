use golutra_auth::{CredentialRef, OAuthFlow};
use golutra_llm::{
    ProviderGenerationConfig, ProviderHeaderConfig, ProviderHeaderValue, ProviderProtocol,
    ProviderReasoningEffort,
};
use serde_json::Value;

pub use golutra_protocol::{DebugProjection, RuntimeEvent, UserProjection};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptScrollAction {
    LineUp,
    LineDown,
    PageUp,
    PageDown,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaneScrollState {
    pub offset_from_bottom: usize,
    pub row_count: usize,
    pub follow_tail: bool,
    pub unseen_rows: usize,
}

impl PaneScrollState {
    pub fn set_row_count(&mut self, row_count: usize) {
        if self.follow_tail {
            self.offset_from_bottom = 0;
        } else if row_count > self.row_count {
            let added = row_count.saturating_sub(self.row_count);
            self.offset_from_bottom = self.offset_from_bottom.saturating_add(added);
            self.unseen_rows = self.unseen_rows.saturating_add(added);
        }
        self.row_count = row_count;
        self.clamp(usize::MAX);
    }

    /// Add rows before the currently loaded window without treating them as new tail content.
    pub fn set_row_count_after_prepend(&mut self, row_count: usize) {
        if !self.follow_tail && row_count > self.row_count {
            self.offset_from_bottom = self
                .offset_from_bottom
                .saturating_add(row_count.saturating_sub(self.row_count));
        }
        self.row_count = row_count;
        self.clamp(usize::MAX);
    }

    pub fn reset(&mut self, row_count: usize) {
        self.offset_from_bottom = 0;
        self.row_count = row_count;
        self.follow_tail = true;
        self.unseen_rows = 0;
    }

    pub fn max_offset(&self, visible_rows: usize) -> usize {
        self.row_count.saturating_sub(visible_rows.max(1))
    }

    pub fn clamp(&mut self, visible_rows: usize) {
        let max = if visible_rows == usize::MAX {
            self.row_count.saturating_sub(1)
        } else {
            self.max_offset(visible_rows)
        };
        self.offset_from_bottom = self.offset_from_bottom.min(max);
        if self.offset_from_bottom == 0 {
            self.follow_tail = true;
            self.unseen_rows = 0;
        }
    }

    pub fn scroll(&mut self, action: TranscriptScrollAction, visible_rows: usize) {
        let page = visible_rows.max(1);
        match action {
            TranscriptScrollAction::LineUp => {
                self.offset_from_bottom = self.offset_from_bottom.saturating_add(1);
                self.follow_tail = false;
            }
            TranscriptScrollAction::LineDown => {
                self.offset_from_bottom = self.offset_from_bottom.saturating_sub(1);
            }
            TranscriptScrollAction::PageUp => {
                self.offset_from_bottom = self.offset_from_bottom.saturating_add(page);
                self.follow_tail = false;
            }
            TranscriptScrollAction::PageDown => {
                self.offset_from_bottom = self.offset_from_bottom.saturating_sub(page);
            }
            TranscriptScrollAction::Top => {
                self.offset_from_bottom = self.max_offset(visible_rows);
                self.follow_tail = false;
            }
            TranscriptScrollAction::Bottom => {
                self.offset_from_bottom = 0;
                self.follow_tail = true;
                self.unseen_rows = 0;
            }
        }
        self.clamp(visible_rows);
    }
}

#[must_use]
pub fn tui_boundary() -> &'static str {
    "projection-only terminal UI"
}

#[must_use]
pub fn render_user_projection(projection: &UserProjection) -> String {
    let steps = projection
        .visible_steps
        .iter()
        .map(|step| format!("{} [{}] {}", step.label, step.status, step.summary))
        .collect::<Vec<_>>()
        .join("\n");
    format!("status: {:?}\n{steps}", projection.status)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventTimelineLine {
    pub sequence_no: u64,
    pub label: String,
    pub summary: String,
}

#[must_use]
pub fn event_timeline_lines(events: &[Value]) -> Vec<EventTimelineLine> {
    events
        .iter()
        .filter_map(|value| serde_json::from_value::<RuntimeEvent>(value.clone()).ok())
        .map(|event| EventTimelineLine {
            sequence_no: event.sequence_no,
            label: format!("{:?} / {:?}", event.event_type, event.source),
            summary: event
                .payload
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("runtime event recorded")
                .to_owned(),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    New,
    Auth(SlashAuthCommand),
    Resume {
        thread_id: Option<String>,
    },
    Export,
    Threads {
        limit: u32,
    },
    Fork {
        thread_id: String,
        from_turn_id: Option<String>,
    },
    Status,
    Debug,
    Takeover,
    Abort,
    Pause,
    Continue,
    Approve,
    Deny,
    Compact,
    Clear,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashAuthCommand {
    Setup,
    Status,
    Protocols,
    Mock,
    Use {
        profile: String,
        scope: AuthConfigScope,
    },
    Login(Box<OpenAiCompatibleLogin>),
    OAuthLogin(Box<OAuthLoginCommand>),
    Logout {
        profile: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthConfigScope {
    User,
    Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthCredentialStore {
    Disk,
    Environment,
    Ephemeral,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiCompatibleLogin {
    pub profile: String,
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub api_key: Option<String>,
    pub credential_store: AuthCredentialStore,
    pub credential_ref: Option<CredentialRef>,
    pub generation_config: Option<ProviderGenerationConfig>,
    pub custom_headers: Vec<ProviderHeaderConfig>,
    pub scope: AuthConfigScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthLoginCommand {
    pub descriptor_path: String,
    pub flow: OAuthFlow,
    pub profile: String,
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub model: String,
    pub credential_store: AuthCredentialStore,
    pub no_open_browser: bool,
    pub generation_config: Option<ProviderGenerationConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashInput {
    Prompt(String),
    Command(SlashCommand),
    Empty,
    Error(String),
}

#[derive(Debug, Clone, Copy)]
struct SlashCommandHint {
    command: &'static str,
    description: &'static str,
    selection: SlashCommandSelection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlashCommandSelection {
    Execute,
    Fill,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommandCandidate {
    pub command: String,
    pub description: String,
    pub execute_on_select: bool,
}

const TOP_LEVEL_SLASH_HINTS: &[SlashCommandHint] = &[
    SlashCommandHint {
        command: "/new",
        description: "start a new session",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/resume",
        description: "open sessions",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/export",
        description: "export session history and runtime facts",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/threads",
        description: "list threads",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/fork",
        description: "fork a thread",
        selection: SlashCommandSelection::Fill,
    },
    SlashCommandHint {
        command: "/auth",
        description: "provider setup",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/status",
        description: "show runtime status",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/debug",
        description: "toggle developer runtime view",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/takeover",
        description: "take control of active task",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/abort",
        description: "abort active task",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/pause",
        description: "pause active task",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/continue",
        description: "resume paused task",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/approve",
        description: "approve pending tool",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/deny",
        description: "deny pending tool",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/compact",
        description: "compact conversation history",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/clear",
        description: "clear local messages",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/quit",
        description: "leave TUI",
        selection: SlashCommandSelection::Execute,
    },
];

const AUTH_SLASH_HINTS: &[SlashCommandHint] = &[
    SlashCommandHint {
        command: "/auth setup",
        description: "connect provider",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/auth status",
        description: "show provider state",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/auth protocols",
        description: "list protocols",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/auth mock",
        description: "use mock provider",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/auth login",
        description: "save API key profile",
        selection: SlashCommandSelection::Fill,
    },
    SlashCommandHint {
        command: "/auth oauth-login",
        description: "authorize with OAuth descriptor",
        selection: SlashCommandSelection::Fill,
    },
    SlashCommandHint {
        command: "/auth logout",
        description: "remove provider credential",
        selection: SlashCommandSelection::Fill,
    },
    SlashCommandHint {
        command: "/auth use",
        description: "activate profile",
        selection: SlashCommandSelection::Fill,
    },
];

#[must_use]
pub fn slash_command_suggestions(input: &str) -> Vec<String> {
    slash_command_candidates(input)
        .into_iter()
        .map(|candidate| format!("{} - {}", candidate.command, candidate.description))
        .collect()
}

#[must_use]
pub fn slash_command_candidates(input: &str) -> Vec<SlashCommandCandidate> {
    let input = input.trim_start();
    if !input.starts_with('/') {
        return Vec::new();
    }

    let tokens = input.split_whitespace().collect::<Vec<_>>();
    let first_token = tokens.first().copied().unwrap_or("/");
    let suggestions =
        if first_token == "/auth" && (input.ends_with(char::is_whitespace) || tokens.len() > 1) {
            let action_prefix = if input.ends_with(char::is_whitespace) && tokens.len() == 1 {
                ""
            } else {
                tokens.get(1).copied().unwrap_or("")
            };
            matching_hints(AUTH_SLASH_HINTS, action_prefix, "/auth ")
        } else {
            matching_hints(TOP_LEVEL_SLASH_HINTS, first_token, "")
        };

    suggestions.into_iter().take(5).collect()
}

fn matching_hints(
    hints: &[SlashCommandHint],
    prefix: &str,
    auth_prefix_to_strip: &str,
) -> Vec<SlashCommandCandidate> {
    hints
        .iter()
        .filter(|hint| {
            let command = hint
                .command
                .strip_prefix(auth_prefix_to_strip)
                .unwrap_or(hint.command);
            command.starts_with(prefix)
        })
        .map(|hint| SlashCommandCandidate {
            command: hint.command.to_owned(),
            description: hint.description.to_owned(),
            execute_on_select: hint.selection == SlashCommandSelection::Execute,
        })
        .collect()
}

#[must_use]
pub fn parse_slash_input(input: &str) -> SlashInput {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return SlashInput::Empty;
    }
    if !trimmed.starts_with('/') {
        return SlashInput::Prompt(trimmed.to_owned());
    }
    let tokens = trimmed
        .split_whitespace()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let Some(command) = tokens.first().map(|value| value.as_str()) else {
        return SlashInput::Empty;
    };
    match command {
        "/help" | "/?" => SlashInput::Command(SlashCommand::Help),
        "/new" => SlashInput::Command(SlashCommand::New),
        "/resume" => SlashInput::Command(SlashCommand::Resume {
            thread_id: tokens.get(1).cloned(),
        }),
        "/export" if tokens.len() == 1 => SlashInput::Command(SlashCommand::Export),
        "/export" => {
            SlashInput::Error("/export does not take arguments; select a session first".to_owned())
        }
        "/threads" | "/thread" => parse_threads_command(&tokens),
        "/fork" => parse_fork_command(&tokens),
        "/auth" => parse_auth_command(&tokens),
        "/status" => SlashInput::Command(SlashCommand::Status),
        "/debug" => SlashInput::Command(SlashCommand::Debug),
        "/takeover" => SlashInput::Command(SlashCommand::Takeover),
        "/abort" => SlashInput::Command(SlashCommand::Abort),
        "/pause" => SlashInput::Command(SlashCommand::Pause),
        "/continue" => SlashInput::Command(SlashCommand::Continue),
        "/approve" => SlashInput::Command(SlashCommand::Approve),
        "/deny" => SlashInput::Command(SlashCommand::Deny),
        "/compact" => SlashInput::Command(SlashCommand::Compact),
        "/clear" => SlashInput::Command(SlashCommand::Clear),
        "/quit" | "/exit" => SlashInput::Command(SlashCommand::Quit),
        other => SlashInput::Error(format!("unknown slash command `{other}`; try /help")),
    }
}

fn parse_fork_command(tokens: &[String]) -> SlashInput {
    let Some(thread_id) = tokens.get(1).cloned() else {
        return SlashInput::Error("/fork requires a thread id".to_owned());
    };
    let from_turn_id = match tokens.get(2).map(String::as_str) {
        None => None,
        Some("--from-turn") => {
            let Some(turn_id) = tokens.get(3).cloned() else {
                return SlashInput::Error("--from-turn requires a turn id".to_owned());
            };
            if tokens.len() > 4 {
                return SlashInput::Error("/fork received unexpected arguments".to_owned());
            }
            Some(turn_id)
        }
        Some(_) => {
            return SlashInput::Error(
                "/fork syntax: /fork <thread-id> [--from-turn <turn-id>]".to_owned(),
            );
        }
    };
    SlashInput::Command(SlashCommand::Fork {
        thread_id,
        from_turn_id,
    })
}

fn parse_threads_command(tokens: &[String]) -> SlashInput {
    let limit = tokens.get(1).map(|value| value.parse::<u32>()).transpose();
    match limit {
        Ok(limit) => SlashInput::Command(SlashCommand::Threads {
            limit: limit.unwrap_or(20),
        }),
        Err(_) => SlashInput::Error("/threads limit must be a positive integer".to_owned()),
    }
}

fn parse_auth_command(tokens: &[String]) -> SlashInput {
    match tokens.get(1).map(|value| value.as_str()).unwrap_or("setup") {
        "setup" => SlashInput::Command(SlashCommand::Auth(SlashAuthCommand::Setup)),
        "status" => SlashInput::Command(SlashCommand::Auth(SlashAuthCommand::Status)),
        "protocols" => SlashInput::Command(SlashCommand::Auth(SlashAuthCommand::Protocols)),
        "mock" => SlashInput::Command(SlashCommand::Auth(SlashAuthCommand::Mock)),
        "use" => parse_auth_use(tokens),
        "login" => parse_auth_login(tokens),
        "oauth-login" => parse_auth_oauth_login(tokens),
        "logout" => parse_auth_logout(tokens),
        other => SlashInput::Error(format!(
            "unknown /auth action `{other}`; use setup, status, protocols, mock, use, login, oauth-login or logout"
        )),
    }
}

fn parse_auth_logout(tokens: &[String]) -> SlashInput {
    if tokens.len() > 3 {
        return SlashInput::Error("/auth logout syntax: /auth logout [profile]".to_owned());
    }
    SlashInput::Command(SlashCommand::Auth(SlashAuthCommand::Logout {
        profile: tokens.get(2).cloned(),
    }))
}

fn parse_auth_use(tokens: &[String]) -> SlashInput {
    let Some(profile) = tokens.get(2).cloned() else {
        return SlashInput::Error("/auth use requires a profile name".to_owned());
    };
    let scope = match parse_auth_scope(tokens.get(3).map(String::as_str).unwrap_or("user")) {
        Ok(scope) => scope,
        Err(error) => return SlashInput::Error(error),
    };
    SlashInput::Command(SlashCommand::Auth(SlashAuthCommand::Use { profile, scope }))
}

fn parse_auth_login(tokens: &[String]) -> SlashInput {
    let mut profile = "default".to_owned();
    let mut protocol = ProviderProtocol::OpenAiCompatible;
    let mut base_url = None;
    let mut model = None;
    let mut api_key_env = "GOLUTRA_PROVIDER_API_KEY".to_owned();
    let mut api_key = None;
    let mut credential_store = AuthCredentialStore::Disk;
    let mut enable_thinking = false;
    let mut reasoning_effort = None;
    let mut context_window_size = None;
    let mut max_tokens = None;
    let mut custom_headers = Vec::new();
    let mut scope = AuthConfigScope::User;
    let mut index = 2;
    while index < tokens.len() {
        match tokens[index].as_str() {
            "--profile" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--profile requires a value".to_owned());
                };
                profile = value.clone();
            }
            "--protocol" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--protocol requires a value".to_owned());
                };
                protocol = match ProviderProtocol::from_config_value(value) {
                    Some(
                        protocol @ (ProviderProtocol::OpenAiCompatible
                        | ProviderProtocol::Anthropic
                        | ProviderProtocol::Gemini
                        | ProviderProtocol::VertexAi
                        | ProviderProtocol::Genai),
                    ) => protocol,
                    Some(other) => {
                        return SlashInput::Error(format!(
                            "custom provider protocol `{}` is not supported by setup",
                            other.id()
                        ));
                    }
                    None => {
                        return SlashInput::Error(
                            "--protocol must be openai-compatible, anthropic, gemini, vertex-ai, or genai"
                                .to_owned(),
                        );
                    }
                };
            }
            "--base-url" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--base-url requires a value".to_owned());
                };
                base_url = Some(value.clone());
            }
            "--model" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--model requires a value".to_owned());
                };
                model = Some(value.clone());
            }
            "--api-key-env" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--api-key-env requires a value".to_owned());
                };
                api_key_env = value.clone();
            }
            "--api-key" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--api-key requires a value".to_owned());
                };
                api_key = Some(value.clone());
            }
            "--store" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--store requires a value".to_owned());
                };
                credential_store = match value.as_str() {
                    "disk" => AuthCredentialStore::Disk,
                    "environment" | "env" => AuthCredentialStore::Environment,
                    _ => {
                        return SlashInput::Error("--store must be disk or environment".to_owned());
                    }
                };
            }
            "--enable-thinking" => {
                enable_thinking = true;
            }
            "--reasoning-effort" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--reasoning-effort requires a value".to_owned());
                };
                reasoning_effort = match parse_reasoning_effort(value) {
                    Ok(value) => Some(value),
                    Err(error) => return SlashInput::Error(error),
                };
            }
            "--context-window-size" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--context-window-size requires a value".to_owned());
                };
                context_window_size = match parse_positive_u64(value, "--context-window-size") {
                    Ok(value) => Some(value),
                    Err(error) => return SlashInput::Error(error),
                };
            }
            "--max-tokens" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--max-tokens requires a value".to_owned());
                };
                max_tokens = match parse_positive_u64(value, "--max-tokens") {
                    Ok(value) => Some(value),
                    Err(error) => return SlashInput::Error(error),
                };
            }
            "--header" | "--header-env" => {
                let environment = tokens[index] == "--header-env";
                let flag = tokens[index].clone();
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error(format!("{flag} requires NAME=VALUE"));
                };
                let header = match parse_provider_header(value, environment) {
                    Ok(header) => header,
                    Err(error) => return SlashInput::Error(error),
                };
                custom_headers.push(header);
            }
            "--scope" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--scope requires a value".to_owned());
                };
                scope = match parse_auth_scope(value) {
                    Ok(scope) => scope,
                    Err(error) => return SlashInput::Error(error),
                };
            }
            value => {
                return SlashInput::Error(format!("unknown /auth login option `{value}`"));
            }
        }
        index += 1;
    }
    let Some(base_url) = base_url else {
        return SlashInput::Error("/auth login requires --base-url".to_owned());
    };
    let Some(model) = model else {
        return SlashInput::Error("/auth login requires --model".to_owned());
    };
    SlashInput::Command(SlashCommand::Auth(SlashAuthCommand::Login(Box::new(
        OpenAiCompatibleLogin {
            profile,
            protocol,
            base_url,
            model,
            api_key_env,
            api_key,
            credential_store,
            credential_ref: None,
            generation_config: build_generation_config(
                enable_thinking,
                reasoning_effort,
                context_window_size,
                max_tokens,
            ),
            custom_headers,
            scope,
        },
    ))))
}

fn parse_auth_oauth_login(tokens: &[String]) -> SlashInput {
    let mut descriptor_path = None;
    let mut flow = OAuthFlow::BrowserPkce;
    let mut profile = "default".to_owned();
    let mut protocol = ProviderProtocol::OpenAiCompatible;
    let mut base_url = None;
    let mut model = None;
    let mut credential_store = AuthCredentialStore::Disk;
    let mut no_open_browser = false;
    let mut enable_thinking = false;
    let mut reasoning_effort = None;
    let mut context_window_size = None;
    let mut max_tokens = None;
    let mut index = 2;
    while index < tokens.len() {
        match tokens[index].as_str() {
            "--descriptor" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--descriptor requires a path".to_owned());
                };
                descriptor_path = Some(value.clone());
            }
            "--flow" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--flow requires a value".to_owned());
                };
                flow = match value.as_str() {
                    "browser" | "browser-pkce" => OAuthFlow::BrowserPkce,
                    "device" | "device-code" => OAuthFlow::DeviceCode,
                    _ => {
                        return SlashInput::Error("--flow must be browser or device".to_owned());
                    }
                };
            }
            "--profile" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--profile requires a value".to_owned());
                };
                profile = value.clone();
            }
            "--protocol" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--protocol requires a value".to_owned());
                };
                protocol = match ProviderProtocol::from_config_value(value) {
                    Some(
                        protocol @ (ProviderProtocol::OpenAiCompatible
                        | ProviderProtocol::Anthropic
                        | ProviderProtocol::Gemini
                        | ProviderProtocol::VertexAi
                        | ProviderProtocol::Genai),
                    ) => protocol,
                    _ => {
                        return SlashInput::Error(
                            "--protocol must be openai-compatible, anthropic, gemini, vertex-ai, or genai"
                                .to_owned(),
                        );
                    }
                };
            }
            "--base-url" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--base-url requires a value".to_owned());
                };
                base_url = Some(value.clone());
            }
            "--model" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--model requires a value".to_owned());
                };
                model = Some(value.clone());
            }
            "--store" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--store requires a value".to_owned());
                };
                credential_store = match value.as_str() {
                    "disk" => AuthCredentialStore::Disk,
                    _ => {
                        return SlashInput::Error("--store must be disk".to_owned());
                    }
                };
            }
            "--no-open-browser" => no_open_browser = true,
            "--enable-thinking" => enable_thinking = true,
            "--reasoning-effort" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--reasoning-effort requires a value".to_owned());
                };
                reasoning_effort = match parse_reasoning_effort(value) {
                    Ok(value) => Some(value),
                    Err(error) => return SlashInput::Error(error),
                };
            }
            "--context-window-size" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--context-window-size requires a value".to_owned());
                };
                context_window_size = match parse_positive_u64(value, "--context-window-size") {
                    Ok(value) => Some(value),
                    Err(error) => return SlashInput::Error(error),
                };
            }
            "--max-tokens" => {
                index += 1;
                let Some(value) = tokens.get(index) else {
                    return SlashInput::Error("--max-tokens requires a value".to_owned());
                };
                max_tokens = match parse_positive_u64(value, "--max-tokens") {
                    Ok(value) => Some(value),
                    Err(error) => return SlashInput::Error(error),
                };
            }
            value => {
                return SlashInput::Error(format!("unknown /auth oauth-login option `{value}`"));
            }
        }
        index += 1;
    }
    let Some(descriptor_path) = descriptor_path else {
        return SlashInput::Error("/auth oauth-login requires --descriptor".to_owned());
    };
    let Some(base_url) = base_url else {
        return SlashInput::Error("/auth oauth-login requires --base-url".to_owned());
    };
    let Some(model) = model else {
        return SlashInput::Error("/auth oauth-login requires --model".to_owned());
    };
    SlashInput::Command(SlashCommand::Auth(SlashAuthCommand::OAuthLogin(Box::new(
        OAuthLoginCommand {
            descriptor_path,
            flow,
            profile,
            protocol,
            base_url,
            model,
            credential_store,
            no_open_browser,
            generation_config: build_generation_config(
                enable_thinking,
                reasoning_effort,
                context_window_size,
                max_tokens,
            ),
        },
    ))))
}

fn build_generation_config(
    enable_thinking: bool,
    reasoning_effort: Option<ProviderReasoningEffort>,
    context_window_size: Option<u64>,
    max_tokens: Option<u64>,
) -> Option<ProviderGenerationConfig> {
    let config = ProviderGenerationConfig {
        enable_thinking,
        reasoning_effort,
        context_window_size,
        max_tokens,
    };
    (!config.is_empty()).then_some(config)
}

fn parse_reasoning_effort(value: &str) -> Result<ProviderReasoningEffort, String> {
    match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "low" => Ok(ProviderReasoningEffort::Low),
        "medium" => Ok(ProviderReasoningEffort::Medium),
        "high" => Ok(ProviderReasoningEffort::High),
        "xhigh" | "x_high" => Ok(ProviderReasoningEffort::Xhigh),
        _ => Err("reasoning effort must be one of: low, medium, high, xhigh".to_owned()),
    }
}

fn parse_provider_header(
    assignment: &str,
    environment: bool,
) -> Result<ProviderHeaderConfig, String> {
    let (name, value) = assignment
        .split_once('=')
        .ok_or_else(|| "provider header requires NAME=VALUE".to_owned())?;
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() || value.is_empty() {
        return Err("provider header requires non-empty NAME=VALUE".to_owned());
    }
    let header = ProviderHeaderConfig {
        name: name.to_owned(),
        value: if environment {
            ProviderHeaderValue::Environment {
                key: value.to_owned(),
            }
        } else {
            ProviderHeaderValue::Literal {
                value: value.to_owned(),
            }
        },
    };
    header.validate()?;
    Ok(header)
}

fn parse_positive_u64(value: &str, option: &str) -> Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("{option} must be a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{option} must be a positive integer"));
    }
    Ok(parsed)
}

fn parse_auth_scope(value: &str) -> Result<AuthConfigScope, String> {
    match value {
        "user" => Ok(AuthConfigScope::User),
        "workspace" => {
            Err("workspace provider config is no longer supported; use `--scope user`".to_owned())
        }
        _ => Err("auth scope must be `user`".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use golutra_core::{EventId, SessionId};
    use golutra_protocol::{RuntimeEventSource, RuntimeEventType};
    use serde_json::json;

    use super::*;

    #[test]
    fn event_lines_ignore_invalid_json_values() {
        let session_id = SessionId::new();
        let event = RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: EventId::new(),
            sequence_no: 7,
            session_id,
            turn_id: None,
            task_id: None,
            parent_event_id: None,
            event_type: RuntimeEventType::CommandAccepted,
            timestamp: chrono::Utc::now(),
            source: RuntimeEventSource::User,
            payload: json!({"summary": "accepted prompt"}),
            payload_ref: None,
            durable: true,
        };

        let lines = event_timeline_lines(&[json!({"bad": true}), json!(event)]);

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].sequence_no, 7);
        assert_eq!(lines[0].summary, "accepted prompt");
    }

    #[test]
    fn pane_scroll_distinguishes_new_tail_rows_from_prepended_history() {
        let mut scroll = PaneScrollState::default();
        scroll.reset(20);
        scroll.scroll(TranscriptScrollAction::PageUp, 5);

        scroll.set_row_count(23);
        assert_eq!(scroll.offset_from_bottom, 8);
        assert_eq!(scroll.unseen_rows, 3);

        scroll.set_row_count_after_prepend(25);
        assert_eq!(scroll.offset_from_bottom, 10);
        assert_eq!(scroll.unseen_rows, 3);

        scroll.scroll(TranscriptScrollAction::Bottom, 5);
        assert_eq!(scroll.offset_from_bottom, 0);
        assert_eq!(scroll.unseen_rows, 0);
        scroll.set_row_count(27);
        assert_eq!(scroll.unseen_rows, 0);
    }

    #[test]
    fn slash_parser_keeps_regular_prompts() {
        assert_eq!(
            parse_slash_input("read README.md"),
            SlashInput::Prompt("read README.md".to_owned())
        );
    }

    #[test]
    fn slash_parser_accepts_resume_and_auth_login() {
        assert_eq!(
            parse_slash_input("/new"),
            SlashInput::Command(SlashCommand::New)
        );
        assert_eq!(
            parse_slash_input("/export"),
            SlashInput::Command(SlashCommand::Export)
        );
        assert_eq!(
            parse_slash_input("/resume 019f"),
            SlashInput::Command(SlashCommand::Resume {
                thread_id: Some("019f".to_owned())
            })
        );
        assert_eq!(
            parse_slash_input(
                "/auth login --base-url api.golutra.cn --model qwen --api-key-env GOLUTRA_KEY --scope user"
            ),
            SlashInput::Command(SlashCommand::Auth(SlashAuthCommand::Login(Box::new(
                OpenAiCompatibleLogin {
                    profile: "default".to_owned(),
                    protocol: ProviderProtocol::OpenAiCompatible,
                    base_url: "api.golutra.cn".to_owned(),
                    model: "qwen".to_owned(),
                    api_key_env: "GOLUTRA_KEY".to_owned(),
                    api_key: None,
                    credential_store: AuthCredentialStore::Disk,
                    credential_ref: None,
                    generation_config: None,
                    custom_headers: Vec::new(),
                    scope: AuthConfigScope::User,
                }
            ))))
        );
    }

    #[test]
    fn slash_parser_rejects_workspace_auth_scope() {
        assert!(matches!(
            parse_slash_input("/auth login --base-url api.golutra.cn --model qwen --scope workspace"),
            SlashInput::Error(error) if error.contains("workspace provider config is no longer supported")
        ));
    }

    #[test]
    fn slash_parser_accepts_literal_and_environment_provider_headers() {
        let input = parse_slash_input(
            "/auth login --base-url https://api.example.com/v1 --model model-test --header X-Client=golutra --header-env X-Api-Key=PROVIDER_HEADER_KEY",
        );
        let SlashInput::Command(SlashCommand::Auth(SlashAuthCommand::Login(login))) = input else {
            panic!("expected auth login");
        };

        assert_eq!(
            login.custom_headers,
            vec![
                ProviderHeaderConfig {
                    name: "X-Client".to_owned(),
                    value: ProviderHeaderValue::Literal {
                        value: "golutra".to_owned(),
                    },
                },
                ProviderHeaderConfig {
                    name: "X-Api-Key".to_owned(),
                    value: ProviderHeaderValue::Environment {
                        key: "PROVIDER_HEADER_KEY".to_owned(),
                    },
                },
            ]
        );
    }

    #[test]
    fn slash_parser_accepts_auth_generation_config() {
        let input = parse_slash_input(
            "/auth login --base-url api.golutra.cn --model gpt-5.5 --enable-thinking --reasoning-effort high --context-window-size 128000 --max-tokens 512",
        );

        let SlashInput::Command(SlashCommand::Auth(SlashAuthCommand::Login(login))) = input else {
            panic!("expected auth login");
        };
        assert_eq!(
            login.generation_config,
            Some(ProviderGenerationConfig {
                enable_thinking: true,
                reasoning_effort: Some(ProviderReasoningEffort::High),
                context_window_size: Some(128_000),
                max_tokens: Some(512),
            })
        );
    }

    #[test]
    fn slash_parser_accepts_oauth_login_and_logout() {
        let input = parse_slash_input(
            "/auth oauth-login --descriptor oauth.json --flow device --profile qwen --base-url https://api.example.com/v1 --model qwen-coder --reasoning-effort high",
        );
        let SlashInput::Command(SlashCommand::Auth(SlashAuthCommand::OAuthLogin(command))) = input
        else {
            panic!("expected OAuth login command");
        };
        assert_eq!(command.descriptor_path, "oauth.json");
        assert_eq!(command.flow, OAuthFlow::DeviceCode);
        assert_eq!(command.profile, "qwen");
        assert_eq!(
            command.generation_config,
            Some(ProviderGenerationConfig {
                enable_thinking: false,
                reasoning_effort: Some(ProviderReasoningEffort::High),
                context_window_size: None,
                max_tokens: None,
            })
        );
        assert_eq!(
            parse_slash_input("/auth logout qwen"),
            SlashInput::Command(SlashCommand::Auth(SlashAuthCommand::Logout {
                profile: Some("qwen".to_owned()),
            }))
        );
        assert_eq!(
            parse_slash_input("/auth logout"),
            SlashInput::Command(SlashCommand::Auth(SlashAuthCommand::Logout {
                profile: None,
            }))
        );
    }

    #[test]
    fn slash_parser_reports_missing_arguments() {
        assert!(matches!(parse_slash_input("/fork"), SlashInput::Error(_)));
        assert!(matches!(
            parse_slash_input("/auth login --model qwen"),
            SlashInput::Error(_)
        ));
    }

    #[test]
    fn slash_parser_accepts_fork_turn_boundary() {
        assert_eq!(
            parse_slash_input("/fork thread-1 --from-turn turn-1"),
            SlashInput::Command(SlashCommand::Fork {
                thread_id: "thread-1".to_owned(),
                from_turn_id: Some("turn-1".to_owned()),
            })
        );
        assert!(matches!(
            parse_slash_input("/fork thread-1 --from-turn"),
            SlashInput::Error(error) if error.contains("requires a turn id")
        ));
        assert!(matches!(
            parse_slash_input("/fork thread-1 --unknown turn-1"),
            SlashInput::Error(error) if error.contains("syntax")
        ));
    }

    #[test]
    fn slash_suggestions_follow_top_level_prefix() {
        assert!(slash_command_suggestions("read README.md").is_empty());
        assert_eq!(
            slash_command_suggestions("/"),
            vec![
                "/new - start a new session".to_owned(),
                "/resume - open sessions".to_owned(),
                "/export - export session history and runtime facts".to_owned(),
                "/threads - list threads".to_owned(),
                "/fork - fork a thread".to_owned(),
            ]
        );
        assert_eq!(
            slash_command_suggestions("/n"),
            vec!["/new - start a new session".to_owned()]
        );
        assert_eq!(
            slash_command_suggestions("/r"),
            vec!["/resume - open sessions".to_owned()]
        );
    }

    #[test]
    fn slash_suggestions_follow_auth_subcommand_prefix() {
        assert_eq!(
            slash_command_suggestions("/auth "),
            vec![
                "/auth setup - connect provider".to_owned(),
                "/auth status - show provider state".to_owned(),
                "/auth protocols - list protocols".to_owned(),
                "/auth mock - use mock provider".to_owned(),
                "/auth login - save API key profile".to_owned(),
            ]
        );
        assert_eq!(
            slash_command_suggestions("/auth l"),
            vec![
                "/auth login - save API key profile".to_owned(),
                "/auth logout - remove provider credential".to_owned(),
            ]
        );
        assert_eq!(
            slash_command_suggestions("/auth o"),
            vec!["/auth oauth-login - authorize with OAuth descriptor".to_owned()]
        );
    }

    #[test]
    fn slash_candidates_mark_fill_only_commands() {
        let fork = slash_command_candidates("/f")
            .into_iter()
            .find(|candidate| candidate.command == "/fork")
            .expect("fork candidate");
        let resume = slash_command_candidates("/r")
            .into_iter()
            .find(|candidate| candidate.command == "/resume")
            .expect("resume candidate");

        assert!(!fork.execute_on_select);
        assert!(resume.execute_on_select);
    }
}
