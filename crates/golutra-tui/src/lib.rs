use golutra_llm::ProviderProtocol;
use serde_json::Value;

pub use golutra_protocol::{DebugProjection, RuntimeEvent, UserProjection};

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
    Resume { thread_id: Option<String> },
    Threads { limit: u32 },
    Fork { thread_id: String },
    Status,
    Debug,
    Abort,
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
    Login(OpenAiCompatibleLogin),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthConfigScope {
    User,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiCompatibleLogin {
    pub profile: String,
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub api_key: Option<String>,
    pub scope: AuthConfigScope,
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
        description: "toggle timeline",
        selection: SlashCommandSelection::Execute,
    },
    SlashCommandHint {
        command: "/abort",
        description: "abort active task",
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
        description: "save OpenAI-compatible profile",
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
        "/threads" | "/thread" => parse_threads_command(&tokens),
        "/fork" => tokens
            .get(1)
            .cloned()
            .map(|thread_id| SlashInput::Command(SlashCommand::Fork { thread_id }))
            .unwrap_or_else(|| SlashInput::Error("/fork requires a thread id".to_owned())),
        "/auth" => parse_auth_command(&tokens),
        "/status" => SlashInput::Command(SlashCommand::Status),
        "/debug" => SlashInput::Command(SlashCommand::Debug),
        "/abort" => SlashInput::Command(SlashCommand::Abort),
        "/clear" => SlashInput::Command(SlashCommand::Clear),
        "/quit" | "/exit" => SlashInput::Command(SlashCommand::Quit),
        other => SlashInput::Error(format!("unknown slash command `{other}`; try /help")),
    }
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
        other => SlashInput::Error(format!(
            "unknown /auth action `{other}`; use setup, status, protocols, mock, use or login"
        )),
    }
}

fn parse_auth_use(tokens: &[String]) -> SlashInput {
    let Some(profile) = tokens.get(2).cloned() else {
        return SlashInput::Error("/auth use requires a profile name".to_owned());
    };
    let scope = match parse_auth_scope(tokens.get(3).map(String::as_str).unwrap_or("workspace")) {
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
                        | ProviderProtocol::Gemini),
                    ) => protocol,
                    Some(other) => {
                        return SlashInput::Error(format!(
                            "custom provider protocol `{}` is not supported by setup",
                            other.id()
                        ));
                    }
                    None => {
                        return SlashInput::Error(
                            "--protocol must be openai-compatible, anthropic, or gemini".to_owned(),
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
    SlashInput::Command(SlashCommand::Auth(SlashAuthCommand::Login(
        OpenAiCompatibleLogin {
            profile,
            protocol,
            base_url,
            model,
            api_key_env,
            api_key,
            scope,
        },
    )))
}

fn parse_auth_scope(value: &str) -> Result<AuthConfigScope, String> {
    match value {
        "user" => Ok(AuthConfigScope::User),
        "workspace" => Ok(AuthConfigScope::Workspace),
        _ => Err("auth scope must be `user` or `workspace`".to_owned()),
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
            parse_slash_input("/resume 019f"),
            SlashInput::Command(SlashCommand::Resume {
                thread_id: Some("019f".to_owned())
            })
        );
        assert_eq!(
            parse_slash_input(
                "/auth login --base-url api.golutra.cn --model qwen --api-key-env GOLUTRA_KEY --scope workspace"
            ),
            SlashInput::Command(SlashCommand::Auth(SlashAuthCommand::Login(
                OpenAiCompatibleLogin {
                    profile: "default".to_owned(),
                    protocol: ProviderProtocol::OpenAiCompatible,
                    base_url: "api.golutra.cn".to_owned(),
                    model: "qwen".to_owned(),
                    api_key_env: "GOLUTRA_KEY".to_owned(),
                    api_key: None,
                    scope: AuthConfigScope::Workspace,
                }
            )))
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
    fn slash_suggestions_follow_top_level_prefix() {
        assert!(slash_command_suggestions("read README.md").is_empty());
        assert_eq!(
            slash_command_suggestions("/"),
            vec![
                "/new - start a new session".to_owned(),
                "/resume - open sessions".to_owned(),
                "/threads - list threads".to_owned(),
                "/fork - fork a thread".to_owned(),
                "/auth - provider setup".to_owned(),
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
                "/auth login - save OpenAI-compatible profile".to_owned(),
            ]
        );
        assert_eq!(
            slash_command_suggestions("/auth l"),
            vec!["/auth login - save OpenAI-compatible profile".to_owned()]
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
