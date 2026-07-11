use clap::{Parser, Subcommand};
use golutra_client::{RuntimeClient, RuntimeTransport};
use golutra_config::{
    ProviderConfigPaths, ProviderConfigScope, ProviderInstallPlan, ProviderProfile,
    apply_provider_install_plan_verified, load_provider_runtime_env, provider_onboarding_state,
    update_provider_settings_verified, validate_provider_protocol_runtime_supported,
};
use golutra_core::{Actor, ActorKind, CommandId, SessionId, TaskStatus, ThreadId};
use golutra_llm::{
    ConfiguredProvider, ProviderGenerationConfig, ProviderProtocol, ProviderReasoningEffort,
    provider_protocol_catalog,
};
use golutra_protocol::{
    EventFilter, RuntimeEvent, RuntimeEventType, RuntimeQuery, RuntimeQueryKind, SessionCommand,
    SessionCommandKind,
};
use std::io::{IsTerminal, Write};
use tokio::time::{Duration, sleep};
use uuid::Uuid;

const CLI_ACTOR_ID: &str = "golutra-cli";

#[derive(Debug, Parser)]
#[command(name = "golutra")]
#[command(about = "Golutra coding agent runtime CLI")]
struct Cli {
    #[arg(long, global = true)]
    cwd: Option<std::path::PathBuf>,
    #[arg(long, global = true, conflicts_with = "connect")]
    daemon: bool,
    #[arg(long, global = true, value_name = "URL", conflicts_with = "daemon")]
    connect: Option<String>,
    #[arg(long, global = true, value_name = "UUID")]
    session_id: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    AppServer {
        #[arg(long, env = "GOLUTRA_APP_ADDR", default_value = "127.0.0.1:47831")]
        addr: std::net::SocketAddr,
    },
    Chat {
        #[arg(default_value = "")]
        prompt: String,
    },
    Status,
    Resume {
        thread_id: Option<String>,
    },
    Fork {
        thread_id: String,
    },
    Abort,
    Takeover,
    Pause,
    Approve {
        approval_id: Option<String>,
    },
    Deny {
        approval_id: Option<String>,
    },
    Compact,
    Trace,
    Export,
    Thread {
        #[command(subcommand)]
        command: ThreadCommand,
    },
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    Eval {
        #[command(subcommand)]
        command: EvalCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ThreadCommand {
    List {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    Resume {
        thread_id: String,
    },
    Fork {
        thread_id: String,
    },
}

#[derive(Debug, Subcommand)]
enum ProviderCommand {
    Current,
    Probe,
    Protocols,
    Login {
        #[arg(long, default_value = "openai-compatible")]
        protocol: String,
        #[arg(long, default_value = "default")]
        profile: String,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value = "GOLUTRA_PROVIDER_API_KEY")]
        api_key_env: String,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long, default_value_t = false)]
        enable_thinking: bool,
        #[arg(long)]
        reasoning_effort: Option<String>,
        #[arg(long)]
        context_window_size: Option<u64>,
        #[arg(long)]
        max_tokens: Option<u64>,
        #[arg(long, default_value = "user")]
        scope: String,
        #[arg(long, default_value_t = true)]
        activate: bool,
    },
    SetKey {
        #[arg(long, default_value = "default")]
        profile: String,
        #[arg(long)]
        api_key: String,
    },
    Use {
        profile: String,
        #[arg(long, default_value = "user")]
        scope: String,
    },
}

#[derive(Debug, Subcommand)]
enum MemoryCommand {
    List,
    Rollback {
        memory_id: String,
        #[arg(long, default_value = "rolled back by user")]
        reason: String,
    },
}

#[derive(Debug, Subcommand)]
enum EvalCommand {
    Results,
    Improvements,
    Candidates,
    Regress {
        candidate_id: String,
    },
    Apply {
        candidate_id: String,
    },
    Rollback {
        candidate_id: String,
        #[arg(long, default_value = "rolled back by user")]
        reason: String,
    },
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    let cli = Cli::parse();
    if let Command::AppServer { addr } = &cli.command {
        return golutra_app_server::run(*addr).await;
    }
    let cwd = cli
        .cwd
        .clone()
        .map_or_else(std::env::current_dir, Ok)
        .map_err(|error| miette::miette!("{error}"))?;
    let transport = if let Some(base_url) = cli.connect.clone() {
        RuntimeTransport::connect(base_url, &cwd).await
    } else if cli.daemon {
        RuntimeTransport::local_daemon(&cwd).await
    } else {
        RuntimeTransport::for_cwd(&cwd).await
    }
    .map_err(|error| miette::miette!("{error}"))?;
    let session_id = resolve_session_id(cli.session_id.as_deref(), &transport)?;

    match cli.command {
        Command::AppServer { .. } => unreachable!("app-server exits before runtime setup"),
        Command::Chat { prompt } => {
            let ack = transport
                .send_command(command(
                    session_id,
                    SessionCommandKind::Prompt,
                    serde_json::json!({ "prompt": prompt }),
                ))
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!("{}", serde_json::to_string_pretty(&ack).unwrap_or_default());
            if ack.accepted {
                let state = wait_for_terminal_state(&transport, session_id).await?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&state).unwrap_or_default()
                );
            }
        }
        Command::Status => {
            let state = transport
                .query(RuntimeQuery {
                    query_id: golutra_core::QueryId::new(),
                    session_id,
                    task_id: None,
                    kind: RuntimeQueryKind::SessionState,
                    requester: ActorKind::Cli,
                    cursor: None,
                    timestamp: chrono::Utc::now(),
                })
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&state).unwrap_or_default()
            );
        }
        Command::Resume { thread_id } => {
            let parsed_thread_id = parse_optional_thread_id(thread_id.as_deref(), &transport)?;
            match transport.resume_thread(parsed_thread_id).await {
                Ok(thread) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&thread).unwrap_or_default()
                    );
                }
                Err(error) if thread_id.is_none() => {
                    let ack = transport
                        .send_command(command(
                            session_id,
                            SessionCommandKind::Resume,
                            serde_json::json!({}),
                        ))
                        .await
                        .map_err(|error| miette::miette!("{error}"))?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "fallback": "lane-resume",
                            "reason": error.to_string(),
                            "ack": ack,
                        }))
                        .unwrap_or_default()
                    );
                }
                Err(error) => return Err(miette::miette!("{error}")),
            }
        }
        Command::Fork { thread_id } => {
            let thread = transport
                .fork_thread(parse_thread_id(&thread_id)?)
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&thread).unwrap_or_default()
            );
        }
        Command::Abort => {
            let ack = transport
                .send_command(command(
                    session_id,
                    SessionCommandKind::Abort,
                    serde_json::json!({}),
                ))
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!("{}", serde_json::to_string_pretty(&ack).unwrap_or_default());
        }
        Command::Takeover => {
            let ack = transport
                .send_command(command(
                    session_id,
                    SessionCommandKind::Takeover,
                    serde_json::json!({}),
                ))
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!("{}", serde_json::to_string_pretty(&ack).unwrap_or_default());
        }
        Command::Pause => {
            let ack = transport
                .send_command(command(
                    session_id,
                    SessionCommandKind::Pause,
                    serde_json::json!({}),
                ))
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!("{}", serde_json::to_string_pretty(&ack).unwrap_or_default());
        }
        Command::Approve { approval_id } => {
            let ack = transport
                .send_command(command(
                    session_id,
                    SessionCommandKind::Approve,
                    approval_payload(approval_id),
                ))
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!("{}", serde_json::to_string_pretty(&ack).unwrap_or_default());
        }
        Command::Deny { approval_id } => {
            let ack = transport
                .send_command(command(
                    session_id,
                    SessionCommandKind::Deny,
                    approval_payload(approval_id),
                ))
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!("{}", serde_json::to_string_pretty(&ack).unwrap_or_default());
        }
        Command::Compact => {
            let ack = transport
                .send_command(command(
                    session_id,
                    SessionCommandKind::Compact,
                    serde_json::json!({}),
                ))
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!("{}", serde_json::to_string_pretty(&ack).unwrap_or_default());
        }
        Command::Trace => {
            let trace = transport
                .query(RuntimeQuery {
                    query_id: golutra_core::QueryId::new(),
                    session_id,
                    task_id: None,
                    kind: RuntimeQueryKind::DebugProjection,
                    requester: ActorKind::Cli,
                    cursor: None,
                    timestamp: chrono::Utc::now(),
                })
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&trace).unwrap_or_default()
            );
        }
        Command::Export => {
            let debug = transport
                .query(RuntimeQuery {
                    query_id: golutra_core::QueryId::new(),
                    session_id,
                    task_id: None,
                    kind: RuntimeQueryKind::DebugProjection,
                    requester: ActorKind::Cli,
                    cursor: None,
                    timestamp: chrono::Utc::now(),
                })
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            let artifacts = debug
                .get("artifacts")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([]));
            println!(
                "{}",
                serde_json::to_string_pretty(&artifacts).unwrap_or_default()
            );
        }
        Command::Thread { command } => match command {
            ThreadCommand::List { limit } => {
                let threads = transport
                    .list_threads(limit)
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&threads).unwrap_or_default()
                );
            }
            ThreadCommand::Resume { thread_id } => {
                let thread = transport
                    .resume_thread(parse_thread_id(&thread_id)?)
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&thread).unwrap_or_default()
                );
            }
            ThreadCommand::Fork { thread_id } => {
                let thread = transport
                    .fork_thread(parse_thread_id(&thread_id)?)
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&thread).unwrap_or_default()
                );
            }
        },
        Command::Provider { command } => match command {
            ProviderCommand::Current => {
                let config = provider_env_for_cli()
                    .ok()
                    .map(|env| ConfiguredProvider::redacted_from_reader(|key| env.get(key)))
                    .transpose()
                    .map_err(|error| miette::miette!("{error}"))?
                    .unwrap_or_else(|| {
                        ConfiguredProvider::redacted_from_env()
                            .unwrap_or_else(|error| provider_error_config(error.to_string()))
                    });
                let onboarding = provider_onboarding_for_cli()?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "provider": config,
                        "onboarding": onboarding,
                    }))
                    .unwrap_or_default()
                );
            }
            ProviderCommand::Probe => {
                let env = provider_env_for_cli().map_err(|error| miette::miette!("{error}"))?;
                let result = ConfiguredProvider::probe_from_reader(|key| env.get(key))
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_default()
                );
            }
            ProviderCommand::Protocols => {
                let protocols = provider_protocol_catalog();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&protocols).unwrap_or_default()
                );
            }
            ProviderCommand::Login {
                protocol,
                profile,
                base_url,
                model,
                api_key_env,
                api_key,
                enable_thinking,
                reasoning_effort,
                context_window_size,
                max_tokens,
                scope,
                activate,
            } => {
                let protocol = parse_provider_protocol(&protocol)?;
                validate_provider_protocol_runtime_supported(protocol)
                    .map_err(|error| miette::miette!("{error}"))?;
                let scope = parse_provider_scope(&scope)?;
                let paths = provider_paths_for_cli()?;
                let cwd = provider_cwd_for_cli(&transport)?;
                let mut provider_profile = match protocol {
                    ProviderProtocol::Mock => ProviderProfile::mock(),
                    ProviderProtocol::OpenAiCompatible => ProviderProfile::openai_compatible(
                        profile,
                        base_url.ok_or_else(|| {
                            miette::miette!("--base-url is required for openai-compatible login")
                        })?,
                        model.ok_or_else(|| {
                            miette::miette!("--model is required for openai-compatible login")
                        })?,
                        api_key_env,
                    )
                    .map_err(|error| miette::miette!("{error}"))?,
                    _ => unreachable!("unsupported provider protocols are rejected before install"),
                };
                provider_profile.api_key = api_key;
                provider_profile.generation_config = generation_config_from_cli(
                    enable_thinking,
                    reasoning_effort.as_deref(),
                    context_window_size,
                    max_tokens,
                )?;
                let plan = ProviderInstallPlan {
                    scope,
                    profile: provider_profile.redacted(),
                    activate,
                };
                let install_plan = ProviderInstallPlan {
                    scope,
                    profile: provider_profile,
                    activate,
                };
                apply_provider_install_plan_verified(&paths, cwd, &install_plan)
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "installed": true,
                        "plan": plan,
                        "paths": {
                            "user": paths.user_config,
                        }
                    }))
                    .unwrap_or_default()
                );
            }
            ProviderCommand::SetKey { profile, api_key } => {
                let paths = provider_paths_for_cli()?;
                let cwd = provider_cwd_for_cli(&transport)?;
                let profile_name = profile.clone();
                let missing_profile = profile.clone();
                let api_key_value = api_key.clone();
                update_provider_settings_verified(&paths, cwd, move |user_settings| {
                    let target_index = user_settings
                        .profiles
                        .iter()
                        .position(|item| item.name == profile_name)
                        .ok_or_else(|| {
                            golutra_config::ConfigError::Validation(format!(
                                "provider profile `{missing_profile}` does not exist in user config"
                            ))
                        })?;
                    let env_key = user_settings.profiles[target_index]
                        .api_key_env
                        .clone()
                        .ok_or_else(|| {
                            golutra_config::ConfigError::Validation(format!(
                                "provider profile `{missing_profile}` does not declare api_key_env"
                            ))
                        })?;
                    user_settings.env.insert(env_key, api_key_value);
                    user_settings.profiles[target_index].api_key = None;
                    Ok(())
                })
                .await
                .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::json!({"updated": true, "profile": profile})
                );
            }
            ProviderCommand::Use { profile, scope } => {
                let scope = parse_provider_scope(&scope)?;
                let paths = provider_paths_for_cli()?;
                let cwd = provider_cwd_for_cli(&transport)?;
                let profile_name = profile.clone();
                update_provider_settings_verified(
                    &paths,
                    cwd,
                    move |user_settings| {
                        if scope == ProviderConfigScope::Workspace {
                            return Err(golutra_config::ConfigError::Validation(
                                "workspace provider config is no longer supported; use global user provider config"
                                    .to_owned(),
                            ));
                        }
                        user_settings.set_active_profile(profile_name)?;
                        Ok(())
                    },
                )
                .await
                .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::json!({"updated": true, "active_profile": profile, "scope": scope})
                );
            }
        },
        Command::Memory { command: memory } => match memory {
            MemoryCommand::List => {
                let records = transport
                    .query(RuntimeQuery {
                        query_id: golutra_core::QueryId::new(),
                        session_id,
                        task_id: None,
                        kind: RuntimeQueryKind::MemoryList,
                        requester: ActorKind::Cli,
                        cursor: None,
                        timestamp: chrono::Utc::now(),
                    })
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&records).unwrap_or_default()
                );
            }
            MemoryCommand::Rollback { memory_id, reason } => {
                let ack = transport
                    .send_command(command(
                        session_id,
                        SessionCommandKind::MemoryRollback,
                        serde_json::json!({
                            "memory_id": memory_id,
                            "reason": reason,
                        }),
                    ))
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                println!("{}", serde_json::to_string_pretty(&ack).unwrap_or_default());
            }
        },
        Command::Eval {
            command: evaluation,
        } => match evaluation {
            EvalCommand::Results => {
                print_runtime_query(&transport, session_id, RuntimeQueryKind::EvaluationResults)
                    .await?;
            }
            EvalCommand::Improvements => {
                print_runtime_query(
                    &transport,
                    session_id,
                    RuntimeQueryKind::ImprovementCandidates,
                )
                .await?;
            }
            EvalCommand::Candidates => {
                print_runtime_query(
                    &transport,
                    session_id,
                    RuntimeQueryKind::AutomationCandidates,
                )
                .await?;
            }
            EvalCommand::Regress { candidate_id } => {
                print_command_ack(
                    &transport,
                    command(
                        session_id,
                        SessionCommandKind::RunRegression,
                        serde_json::json!({"candidate_id": candidate_id}),
                    ),
                )
                .await?;
            }
            EvalCommand::Apply { candidate_id } => {
                print_command_ack(
                    &transport,
                    command(
                        session_id,
                        SessionCommandKind::ApplyCandidate,
                        serde_json::json!({"candidate_id": candidate_id}),
                    ),
                )
                .await?;
            }
            EvalCommand::Rollback {
                candidate_id,
                reason,
            } => {
                print_command_ack(
                    &transport,
                    command(
                        session_id,
                        SessionCommandKind::RollbackCandidate,
                        serde_json::json!({
                            "candidate_id": candidate_id,
                            "reason": reason,
                        }),
                    ),
                )
                .await?;
            }
        },
    }
    Ok(())
}

async fn print_runtime_query(
    transport: &RuntimeTransport,
    session_id: SessionId,
    kind: RuntimeQueryKind,
) -> miette::Result<()> {
    let value = transport
        .query(RuntimeQuery {
            query_id: golutra_core::QueryId::new(),
            session_id,
            task_id: None,
            kind,
            requester: ActorKind::Cli,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&value).unwrap_or_default()
    );
    Ok(())
}

async fn print_command_ack(
    transport: &RuntimeTransport,
    command: SessionCommand,
) -> miette::Result<()> {
    let ack = transport
        .send_command(command)
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    println!("{}", serde_json::to_string_pretty(&ack).unwrap_or_default());
    Ok(())
}

fn provider_paths_for_cli() -> miette::Result<ProviderConfigPaths> {
    ProviderConfigPaths::global().map_err(|error| miette::miette!("{error}"))
}

fn provider_cwd_for_cli(transport: &RuntimeTransport) -> miette::Result<&std::path::Path> {
    transport
        .cwd()
        .ok_or_else(|| miette::miette!("provider config requires a cwd"))
}

fn provider_env_for_cli() -> miette::Result<golutra_config::ProviderRuntimeEnv> {
    load_provider_runtime_env().map_err(|error| miette::miette!("{error}"))
}

fn provider_onboarding_for_cli() -> miette::Result<golutra_config::ProviderOnboardingState> {
    provider_onboarding_state().map_err(|error| miette::miette!("{error}"))
}

fn parse_provider_protocol(value: &str) -> miette::Result<ProviderProtocol> {
    ProviderProtocol::from_config_value(value)
        .ok_or_else(|| miette::miette!("unsupported provider protocol `{value}`"))
}

fn parse_provider_scope(value: &str) -> miette::Result<ProviderConfigScope> {
    match value.trim().to_ascii_lowercase().as_str() {
        "user" => Ok(ProviderConfigScope::User),
        "workspace" => Err(miette::miette!(
            "workspace provider config is no longer supported; use `--scope user`"
        )),
        _ => Err(miette::miette!("provider scope must be `user`")),
    }
}

fn generation_config_from_cli(
    enable_thinking: bool,
    reasoning_effort: Option<&str>,
    context_window_size: Option<u64>,
    max_tokens: Option<u64>,
) -> miette::Result<Option<ProviderGenerationConfig>> {
    let config = ProviderGenerationConfig {
        enable_thinking,
        reasoning_effort: reasoning_effort.map(parse_reasoning_effort).transpose()?,
        context_window_size,
        max_tokens,
    };
    Ok((!config.is_empty()).then_some(config))
}

fn parse_reasoning_effort(value: &str) -> miette::Result<ProviderReasoningEffort> {
    match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "low" => Ok(ProviderReasoningEffort::Low),
        "medium" => Ok(ProviderReasoningEffort::Medium),
        "high" => Ok(ProviderReasoningEffort::High),
        "xhigh" | "x_high" => Ok(ProviderReasoningEffort::Xhigh),
        _ => Err(miette::miette!(
            "reasoning effort must be one of: low, medium, high, xhigh"
        )),
    }
}

fn provider_error_config(error: String) -> golutra_llm::RedactedProviderConfig {
    golutra_llm::RedactedProviderConfig {
        mode: "unknown".to_owned(),
        provider_id: "unknown".to_owned(),
        protocol: ProviderProtocol::Mock,
        native_protocol: "unknown".to_owned(),
        base_url: None,
        model_id: None,
        api_key_env: None,
        api_key_configured: false,
        generation_config: None,
        missing_env: Vec::new(),
        supported: false,
        status: error,
    }
}

fn parse_optional_thread_id(
    value: Option<&str>,
    transport: &RuntimeTransport,
) -> miette::Result<ThreadId> {
    value
        .map(parse_thread_id)
        .transpose()
        .map(|thread_id| thread_id.unwrap_or_else(|| transport.default_thread_id()))
}

fn parse_thread_id(value: &str) -> miette::Result<ThreadId> {
    value
        .parse()
        .map_err(|error: uuid::Error| miette::miette!("invalid thread id: {error}"))
}

fn resolve_session_id(
    value: Option<&str>,
    transport: &RuntimeTransport,
) -> miette::Result<SessionId> {
    value
        .map(|value| {
            Uuid::parse_str(value)
                .map(SessionId)
                .map_err(|error| miette::miette!("invalid session id: {error}"))
        })
        .transpose()
        .map(|session_id| session_id.unwrap_or_else(|| transport.default_session_id()))
}

fn command(
    session_id: golutra_core::SessionId,
    kind: SessionCommandKind,
    payload: serde_json::Value,
) -> SessionCommand {
    SessionCommand {
        command_id: CommandId::new(),
        session_id: Some(session_id),
        kind,
        idempotency_key: CommandId::new().to_string(),
        actor: Actor {
            kind: ActorKind::Cli,
            id: CLI_ACTOR_ID.to_owned(),
        },
        payload,
        timestamp: chrono::Utc::now(),
    }
}

fn approval_payload(approval_id: Option<String>) -> serde_json::Value {
    approval_id.map_or_else(
        || serde_json::json!({}),
        |approval_id| serde_json::json!({"approval_id": approval_id}),
    )
}

async fn wait_for_terminal_state(
    transport: &RuntimeTransport,
    session_id: SessionId,
) -> miette::Result<serde_json::Value> {
    let mut interrupt_count = 0_u8;
    let mut handled_approval = None;
    loop {
        let state = transport
            .query(RuntimeQuery {
                query_id: golutra_core::QueryId::new(),
                session_id,
                task_id: None,
                kind: RuntimeQueryKind::SessionState,
                requester: ActorKind::Cli,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        let status = state
            .get("task_status")
            .and_then(|value| serde_json::from_value::<TaskStatus>(value.clone()).ok());
        if status.is_some_and(is_terminal_status) {
            return Ok(state);
        }
        if status == Some(TaskStatus::WaitingApproval) {
            let approval_id = state
                .get("pending_approval")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    miette::miette!("runtime is waiting for approval without an approval id")
                })?
                .to_owned();
            if handled_approval.as_deref() != Some(approval_id.as_str()) {
                let approved = prompt_for_cli_approval(transport, session_id, &approval_id).await?;
                let ack = transport
                    .send_command(command(
                        session_id,
                        if approved {
                            SessionCommandKind::Approve
                        } else {
                            SessionCommandKind::Deny
                        },
                        serde_json::json!({"approval_id": approval_id}),
                    ))
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                if !ack.accepted {
                    return Err(miette::miette!(
                        "runtime rejected approval resolution: {}",
                        ack.reason.unwrap_or_else(|| "unknown reason".to_owned())
                    ));
                }
                handled_approval = Some(approval_id);
            }
        } else {
            handled_approval = None;
        }

        tokio::select! {
            _ = sleep(Duration::from_millis(100)) => {}
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|error| miette::miette!("failed to listen for Ctrl+C: {error}"))?;
                if interrupt_count > 0 {
                    return Err(miette::miette!("runtime wait interrupted"));
                }
                interrupt_count = interrupt_count.saturating_add(1);
                let ack = transport
                    .send_command(command(
                        session_id,
                        SessionCommandKind::Abort,
                        serde_json::json!({}),
                    ))
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                if !ack.accepted {
                    return Err(miette::miette!(
                        "runtime abort was rejected: {}",
                        ack.reason.unwrap_or_else(|| "unknown reason".to_owned())
                    ));
                }
                eprintln!("abort requested; press Ctrl+C again to stop waiting");
            }
        }
    }
}

async fn prompt_for_cli_approval(
    transport: &RuntimeTransport,
    session_id: SessionId,
    approval_id: &str,
) -> miette::Result<bool> {
    let detail = approval_detail(transport, session_id, approval_id).await?;
    if !std::io::stdin().is_terminal() {
        eprintln!("approval denied because stdin is not interactive: {detail}");
        return Ok(false);
    }
    let prompt = format!("Approval required: {detail}\nApprove? [y/N] ");
    tokio::task::spawn_blocking(move || {
        eprint!("{prompt}");
        std::io::stderr()
            .flush()
            .map_err(|error| miette::miette!("failed to flush approval prompt: {error}"))?;
        let mut response = String::new();
        std::io::stdin()
            .read_line(&mut response)
            .map_err(|error| miette::miette!("failed to read approval response: {error}"))?;
        Ok(matches!(
            response.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    })
    .await
    .map_err(|error| miette::miette!("approval prompt task failed: {error}"))?
}

async fn approval_detail(
    transport: &RuntimeTransport,
    session_id: SessionId,
    approval_id: &str,
) -> miette::Result<String> {
    let events = transport
        .replay_events(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    let detail = events
        .into_iter()
        .filter_map(|value| serde_json::from_value::<RuntimeEvent>(value).ok())
        .rev()
        .find(|event| {
            event.event_type == RuntimeEventType::ApprovalRequested
                && event
                    .payload
                    .get("approval_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(approval_id)
        })
        .and_then(|event| event.payload.get("request").cloned())
        .map(|request| {
            let tool = request
                .get("tool_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool");
            let resource = request
                .get("resource")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown resource");
            format!("{tool}: {resource}")
        })
        .unwrap_or_else(|| format!("approval {approval_id}"));
    Ok(detail)
}

fn is_terminal_status(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Completed
            | TaskStatus::Partial
            | TaskStatus::Failed
            | TaskStatus::Blocked
            | TaskStatus::Cancelled
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separate_cli_commands_use_the_same_controller_identity() {
        let session_id = SessionId::new();
        let takeover = command(
            session_id,
            SessionCommandKind::Takeover,
            serde_json::json!({}),
        );
        let abort = command(session_id, SessionCommandKind::Abort, serde_json::json!({}));

        assert_eq!(takeover.actor, abort.actor);
        assert_eq!(takeover.actor.id, CLI_ACTOR_ID);
    }
}
