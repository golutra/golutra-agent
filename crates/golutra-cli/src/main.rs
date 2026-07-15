use clap::{Parser, Subcommand, ValueEnum};
use golutra_auth::{
    CredentialRef, CredentialSource, OAuthFlow, OAuthProviderDescriptor, SecretKind,
};
use golutra_client::{RuntimeClient, RuntimeTransport};
use golutra_config::{
    BuiltinOAuthMethod, ProviderConfigPaths, ProviderConfigScope, ProviderInstallPlan,
    ProviderProfile, apply_oauth_provider_install_plan_verified,
    apply_provider_install_plan_verified, builtin_oauth_method, builtin_oauth_methods,
    builtin_oauth_methods_for_provider, golutra_home, load_provider_runtime_env,
    load_provider_settings, logout_provider_profile_verified, provider_auth_service,
    provider_onboarding_state, replace_provider_credential_verified,
    update_provider_settings_verified, validate_provider_protocol_runtime_supported,
};
use golutra_core::{Actor, ActorKind, CommandId, SessionId, TaskStatus, ThreadId, TurnId};
use golutra_llm::{
    ConfiguredProvider, ProviderGenerationConfig, ProviderHeaderConfig, ProviderHeaderValue,
    ProviderProtocol, ProviderReasoningEffort, provider_protocol_catalog,
};
use golutra_plugin::PluginStore;
use golutra_protocol::{
    EventFilter, RuntimeEvent, RuntimeEventType, RuntimeQuery, RuntimeQueryKind, SessionCommand,
    SessionCommandKind,
};
use secrecy::SecretString;
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
        #[arg(long)]
        from_turn: Option<String>,
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
        command: Box<ProviderCommand>,
    },
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    Eval {
        #[command(subcommand)]
        command: EvalCommand,
    },
    Evolution {
        #[command(subcommand)]
        command: EvolutionCommand,
    },
    Storage {
        #[command(subcommand)]
        command: StorageCommand,
    },
    Code {
        #[command(subcommand)]
        command: CodeCommand,
    },
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
}

#[derive(Debug, Subcommand)]
enum PluginCommand {
    List,
    Stage {
        package: std::path::PathBuf,
    },
    Review {
        plugin_id: String,
        revision_id: String,
    },
    Enable {
        plugin_id: String,
        revision_id: String,
    },
    Disable {
        plugin_id: String,
    },
    Rollback {
        plugin_id: String,
    },
}

#[derive(Debug, Subcommand)]
enum CodeCommand {
    Index,
    Symbols {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    References {
        symbol: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
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
        #[arg(long)]
        from_turn: Option<String>,
    },
    Export {
        thread_id: String,
    },
    Rebind {
        thread_id: String,
        #[arg(long, value_name = "OLD_PATH")]
        from: std::path::PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum StorageCommand {
    Status,
    Clean,
}

#[derive(Debug, Subcommand)]
enum ProviderCommand {
    Current,
    Probe,
    Protocols,
    #[command(name = "auth-methods")]
    AuthMethods {
        #[arg(long)]
        provider: Option<String>,
    },
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
        #[arg(long, value_enum, default_value_t = CredentialStoreArg::Disk)]
        store: CredentialStoreArg,
        #[arg(long, default_value_t = false)]
        enable_thinking: bool,
        #[arg(long)]
        reasoning_effort: Option<String>,
        #[arg(long)]
        context_window_size: Option<u64>,
        #[arg(long)]
        max_tokens: Option<u64>,
        #[arg(long, value_name = "NAME=VALUE")]
        header: Vec<String>,
        #[arg(long, value_name = "NAME=ENV")]
        header_env: Vec<String>,
        #[arg(long, default_value = "user")]
        scope: String,
        #[arg(long, default_value_t = true)]
        activate: bool,
    },
    SetKey {
        #[arg(long, default_value = "default")]
        profile: String,
        #[arg(long, conflicts_with = "env_key", required_unless_present = "env_key")]
        api_key: Option<String>,
        #[arg(long, conflicts_with = "api_key")]
        env_key: Option<String>,
        #[arg(long, value_enum, default_value_t = CredentialStoreArg::Disk)]
        store: CredentialStoreArg,
    },
    #[command(name = "oauth-login")]
    OAuthLogin {
        #[arg(
            long,
            value_name = "JSON",
            conflicts_with = "provider",
            required_unless_present = "provider"
        )]
        descriptor: Option<std::path::PathBuf>,
        #[arg(
            long,
            conflicts_with = "descriptor",
            required_unless_present = "descriptor"
        )]
        provider: Option<String>,
        #[arg(long, requires = "provider")]
        method: Option<String>,
        #[arg(long, value_enum)]
        flow: Option<OAuthFlowArg>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        protocol: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, value_enum, default_value_t = CredentialStoreArg::Disk)]
        store: CredentialStoreArg,
        #[arg(long, default_value_t = false)]
        no_open_browser: bool,
        #[arg(long, default_value_t = false)]
        enable_thinking: bool,
        #[arg(long)]
        reasoning_effort: Option<String>,
        #[arg(long)]
        context_window_size: Option<u64>,
        #[arg(long)]
        max_tokens: Option<u64>,
        #[arg(long, default_value_t = true)]
        activate: bool,
    },
    Logout {
        #[arg(long)]
        profile: Option<String>,
    },
    Use {
        profile: String,
        #[arg(long, default_value = "user")]
        scope: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CredentialStoreArg {
    Disk,
}

impl CredentialStoreArg {
    fn source(self) -> CredentialSource {
        match self {
            Self::Disk => CredentialSource::Disk,
        }
    }

    fn api_key_reference(self) -> CredentialRef {
        match self {
            Self::Disk => CredentialRef::disk(SecretKind::ApiKey),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OAuthFlowArg {
    Browser,
    Device,
    OpenAiDevice,
}

impl OAuthFlowArg {
    fn auth_flow(self) -> OAuthFlow {
        match self {
            Self::Browser => OAuthFlow::BrowserPkce,
            Self::Device => OAuthFlow::DeviceCode,
            Self::OpenAiDevice => OAuthFlow::OpenAiDeviceAuth,
        }
    }
}

#[derive(Debug, Subcommand)]
enum MemoryCommand {
    List,
    Feedback {
        memory_id: String,
        #[arg(long, value_enum)]
        feedback: MemoryFeedbackArg,
        #[arg(long, default_value = "")]
        reason: String,
    },
    Rollback {
        memory_id: String,
        #[arg(long, default_value = "rolled back by user")]
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum MemoryFeedbackArg {
    Helpful,
    Irrelevant,
    Incorrect,
}

impl MemoryFeedbackArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Helpful => "helpful",
            Self::Irrelevant => "irrelevant",
            Self::Incorrect => "incorrect",
        }
    }
}

#[derive(Debug, Subcommand)]
enum EvalCommand {
    Results,
    Improvements,
    Candidates,
    Regress {
        candidate_id: String,
    },
    Review {
        candidate_id: String,
        #[arg(long, value_enum)]
        decision: ReviewDecisionArg,
        #[arg(long)]
        reason: String,
    },
    Apply {
        candidate_id: String,
    },
    Rollback {
        candidate_id: String,
        #[arg(long, default_value = "rolled back by user")]
        reason: String,
    },
    RecordBenchmark {
        #[arg(value_name = "JSON_FILE")]
        file: std::path::PathBuf,
    },
    CompareCounterfactual {
        group_id: String,
    },
}

#[derive(Debug, Subcommand)]
enum EvolutionCommand {
    Status,
    Plan {
        #[arg(default_value = "expand verified workspace capabilities")]
        objective: String,
        #[arg(long, default_value_t = 20)]
        max_generated_tasks: u32,
        #[arg(long, default_value_t = 3)]
        max_selected_tasks: u32,
        #[arg(long, default_value_t = 8)]
        max_tool_calls_per_task: u32,
        #[arg(long, default_value_t = 120_000)]
        max_runtime_ms_per_task: u64,
    },
    Run {
        run_id: Option<String>,
    },
    Skill {
        #[command(subcommand)]
        command: EvolutionSkillCommand,
    },
}

#[derive(Debug, Subcommand)]
enum EvolutionSkillCommand {
    Stage {
        candidate_id: String,
    },
    Review {
        skill_id: String,
        #[arg(long, value_enum)]
        decision: ReviewDecisionArg,
        #[arg(long)]
        reason: String,
        #[arg(long = "regression-ref")]
        regression_refs: Vec<String>,
    },
    Install {
        skill_id: String,
    },
    Rollback {
        skill_id: String,
        #[arg(long, default_value = "rolled back by user")]
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ReviewDecisionArg {
    Approve,
    Reject,
}

impl ReviewDecisionArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject => "reject",
        }
    }
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    let cli = Cli::parse();
    if let Command::AppServer { addr } = &cli.command {
        return golutra_app_server::run(*addr).await;
    }
    if let Command::Plugin { command } = &cli.command {
        return run_plugin_command(command);
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
        Command::Plugin { .. } => unreachable!("plugin exits before runtime setup"),
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
        Command::Fork {
            thread_id,
            from_turn,
        } => {
            let thread = transport
                .fork_thread(
                    parse_thread_id(&thread_id)?,
                    from_turn.as_deref().map(parse_turn_id).transpose()?,
                )
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
            ThreadCommand::Fork {
                thread_id,
                from_turn,
            } => {
                let thread = transport
                    .fork_thread(
                        parse_thread_id(&thread_id)?,
                        from_turn.as_deref().map(parse_turn_id).transpose()?,
                    )
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&thread).unwrap_or_default()
                );
            }
            ThreadCommand::Export { thread_id } => {
                let export = transport
                    .export_thread_rollout(parse_thread_id(&thread_id)?)
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&export).unwrap_or_default()
                );
            }
            ThreadCommand::Rebind { thread_id, from } => {
                let result = transport
                    .rebind_thread(parse_thread_id(&thread_id)?, from)
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_default()
                );
            }
        },
        Command::Provider { command } => match *command {
            ProviderCommand::Current => {
                let env = provider_env_for_cli().map_err(|error| miette::miette!("{error}"))?;
                let config = ConfiguredProvider::redacted_from_reader(|key| env.get(key))
                    .map_err(|error| miette::miette!("{error}"))?;
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
                let result = ConfiguredProvider::probe_from_reader_with_credential(
                    |key| env.get(key),
                    env.credential_provider(),
                )
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
            ProviderCommand::AuthMethods { provider } => {
                let methods = match provider {
                    Some(provider) => builtin_oauth_methods_for_provider(&provider),
                    None => builtin_oauth_methods(),
                };
                println!(
                    "{}",
                    serde_json::to_string_pretty(&methods).unwrap_or_default()
                );
            }
            ProviderCommand::Login {
                protocol,
                profile,
                base_url,
                model,
                api_key_env,
                api_key,
                store,
                enable_thinking,
                reasoning_effort,
                context_window_size,
                max_tokens,
                header,
                header_env,
                scope,
                activate,
            } => {
                let protocol = parse_provider_protocol(&protocol)?;
                validate_provider_protocol_runtime_supported(protocol)
                    .map_err(|error| miette::miette!("{error}"))?;
                let scope = parse_provider_scope(&scope)?;
                let paths = provider_paths_for_cli()?;
                let cwd = provider_cwd_for_cli(&transport)?;
                let (mut provider_profile, pending_secret) = if protocol == ProviderProtocol::Mock {
                    (ProviderProfile::mock(), None)
                } else {
                    let (credential_ref, pending_secret) = match api_key {
                        Some(api_key) => {
                            (store.api_key_reference(), Some(SecretString::from(api_key)))
                        }
                        None => (
                            CredentialRef::environment(api_key_env, SecretKind::ApiKey)
                                .map_err(|error| miette::miette!("{error}"))?,
                            None,
                        ),
                    };
                    (
                        ProviderProfile::live_profile(
                            profile,
                            protocol,
                            base_url.ok_or_else(|| {
                                miette::miette!(
                                    "--base-url is required for {} login",
                                    protocol.id()
                                )
                            })?,
                            model.ok_or_else(|| {
                                miette::miette!("--model is required for {} login", protocol.id())
                            })?,
                            credential_ref,
                        )
                        .map_err(|error| miette::miette!("{error}"))?,
                        pending_secret,
                    )
                };
                provider_profile.generation_config = generation_config_from_cli(
                    enable_thinking,
                    reasoning_effort.as_deref(),
                    context_window_size,
                    max_tokens,
                )?;
                provider_profile.custom_headers = provider_headers_from_cli(&header, &header_env)?;
                let plan = ProviderInstallPlan {
                    scope,
                    profile: provider_profile.redacted(),
                    activate,
                    pending_secret: None,
                };
                let install_plan = ProviderInstallPlan {
                    scope,
                    profile: provider_profile,
                    activate,
                    pending_secret,
                };
                apply_provider_install_plan_verified(&paths, cwd, &install_plan)
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                notify_provider_configured_cli(&transport).await?;
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
            ProviderCommand::SetKey {
                profile,
                api_key,
                env_key,
                store,
            } => {
                let paths = provider_paths_for_cli()?;
                let cwd = provider_cwd_for_cli(&transport)?;
                let (reference, secret) = match (api_key, env_key) {
                    (Some(api_key), None) => {
                        (store.api_key_reference(), Some(SecretString::from(api_key)))
                    }
                    (None, Some(env_key)) => (
                        CredentialRef::environment(env_key, SecretKind::ApiKey)
                            .map_err(|error| miette::miette!("{error}"))?,
                        None,
                    ),
                    _ => {
                        return Err(miette::miette!(
                            "set-key requires exactly one of --api-key or --env-key"
                        ));
                    }
                };
                replace_provider_credential_verified(
                    &paths,
                    cwd,
                    profile.clone(),
                    reference,
                    secret,
                )
                .await
                .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::json!({"updated": true, "profile": profile})
                );
            }
            ProviderCommand::OAuthLogin {
                descriptor,
                provider,
                method,
                flow,
                profile,
                protocol,
                base_url,
                model,
                store,
                no_open_browser,
                enable_thinking,
                reasoning_effort,
                context_window_size,
                max_tokens,
                activate,
            } => {
                let paths = provider_paths_for_cli()?;
                let cwd = provider_cwd_for_cli(&transport)?;
                let ResolvedOAuthLogin {
                    descriptor,
                    flow,
                    profile,
                    protocol,
                    base_url,
                    model,
                } = resolve_oauth_login(
                    descriptor.as_deref(),
                    provider.as_deref(),
                    method.as_deref(),
                    flow,
                    profile,
                    protocol.as_deref(),
                    base_url,
                    model,
                )?;
                if !descriptor.flows.contains(&flow) {
                    return Err(miette::miette!(
                        "OAuth descriptor `{}` does not support {:?}",
                        descriptor.provider_id,
                        flow
                    ));
                }
                validate_provider_protocol_runtime_supported(protocol)
                    .map_err(|error| miette::miette!("{error}"))?;
                let mut provider_profile = ProviderProfile::live_profile(
                    profile.clone(),
                    protocol,
                    base_url,
                    model,
                    CredentialRef::ephemeral(SecretKind::OAuthTokenSet),
                )
                .map_err(|error| miette::miette!("{error}"))?;
                provider_profile.oauth = Some(descriptor.clone());
                provider_profile.generation_config = generation_config_from_cli(
                    enable_thinking,
                    reasoning_effort.as_deref(),
                    context_window_size,
                    max_tokens,
                )?;
                provider_profile
                    .validate()
                    .map_err(|error| miette::miette!("{error}"))?;
                let auth =
                    provider_auth_service(&paths).map_err(|error| miette::miette!("{error}"))?;
                let login = match flow {
                    OAuthFlow::BrowserPkce => {
                        let login = auth
                            .begin_browser_login(descriptor.clone(), store.source())
                            .await
                            .map_err(|error| miette::miette!("{error}"))?;
                        println!("Open this URL to authorize:\n{}", login.authorization_url());
                        std::io::stdout()
                            .flush()
                            .map_err(|error| miette::miette!("{error}"))?;
                        if !no_open_browser && let Err(error) = login.open_browser().await {
                            eprintln!("browser could not be opened automatically: {error}");
                        }
                        login
                            .complete()
                            .await
                            .map_err(|error| miette::miette!("{error}"))?
                    }
                    OAuthFlow::DeviceCode => {
                        let login = auth
                            .begin_device_login(descriptor.clone(), store.source())
                            .await
                            .map_err(|error| miette::miette!("{error}"))?;
                        println!(
                            "Open {} and enter code {}",
                            login
                                .verification_uri_complete()
                                .unwrap_or_else(|| login.verification_uri()),
                            login.user_code()
                        );
                        std::io::stdout()
                            .flush()
                            .map_err(|error| miette::miette!("{error}"))?;
                        login
                            .complete()
                            .await
                            .map_err(|error| miette::miette!("{error}"))?
                    }
                    OAuthFlow::OpenAiDeviceAuth => {
                        let login = auth
                            .begin_openai_device_login(descriptor.clone(), store.source())
                            .await
                            .map_err(|error| miette::miette!("{error}"))?;
                        println!(
                            "Open {} and enter code {}",
                            login.verification_uri(),
                            login.user_code()
                        );
                        std::io::stdout()
                            .flush()
                            .map_err(|error| miette::miette!("{error}"))?;
                        if !no_open_browser && let Err(error) = login.open_browser().await {
                            eprintln!("browser could not be opened automatically: {error}");
                        }
                        login
                            .complete()
                            .await
                            .map_err(|error| miette::miette!("{error}"))?
                    }
                };
                provider_profile.credential_ref = Some(login.credential_ref);
                let plan = ProviderInstallPlan {
                    scope: ProviderConfigScope::User,
                    profile: provider_profile,
                    activate,
                    pending_secret: None,
                };
                apply_oauth_provider_install_plan_verified(&paths, cwd, &plan)
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "installed": true,
                        "profile": profile,
                        "provider_id": plan.profile.oauth.as_ref().map(|value| &value.provider_id),
                        "credential": plan.profile.credential_ref.as_ref().map(CredentialRef::source_label),
                        "token": login.metadata,
                    }))
                    .unwrap_or_default()
                );
            }
            ProviderCommand::Logout { profile } => {
                let paths = provider_paths_for_cli()?;
                let cwd = provider_cwd_for_cli(&transport)?;
                let profile = match profile {
                    Some(profile) => profile,
                    None => load_provider_settings(&paths)
                        .map_err(|error| miette::miette!("{error}"))?
                        .active_profile
                        .ok_or_else(|| miette::miette!("no active provider profile to log out"))?,
                };
                logout_provider_profile_verified(&paths, cwd, profile.clone())
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::json!({"logged_out": true, "profile": profile})
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
            MemoryCommand::Feedback {
                memory_id,
                feedback,
                reason,
            } => {
                print_command_ack(
                    &transport,
                    command(
                        session_id,
                        SessionCommandKind::MemoryFeedback,
                        serde_json::json!({
                            "memory_id": memory_id,
                            "feedback": feedback.as_str(),
                            "reason": reason,
                        }),
                    ),
                )
                .await?;
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
            EvalCommand::Review {
                candidate_id,
                decision,
                reason,
            } => {
                print_command_ack(
                    &transport,
                    command(
                        session_id,
                        SessionCommandKind::ReviewCandidate,
                        serde_json::json!({
                            "candidate_id": candidate_id,
                            "decision": decision.as_str(),
                            "reason": reason,
                        }),
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
            EvalCommand::RecordBenchmark { file } => {
                let content = std::fs::read_to_string(&file).map_err(|error| {
                    miette::miette!("failed to read benchmark {}: {error}", file.display())
                })?;
                let run: golutra_eval::BenchmarkRun = serde_json::from_str(&content)
                    .map_err(|error| miette::miette!("benchmark JSON is invalid: {error}"))?;
                print_command_ack(
                    &transport,
                    command(
                        session_id,
                        SessionCommandKind::RecordBenchmark,
                        serde_json::json!({"run": run}),
                    ),
                )
                .await?;
            }
            EvalCommand::CompareCounterfactual { group_id } => {
                print_command_ack(
                    &transport,
                    command(
                        session_id,
                        SessionCommandKind::CompareCounterfactual,
                        serde_json::json!({"group_id": group_id}),
                    ),
                )
                .await?;
            }
        },
        Command::Evolution { command: evolution } => match evolution {
            EvolutionCommand::Status => {
                print_runtime_query(&transport, session_id, RuntimeQueryKind::EvolutionState)
                    .await?;
            }
            EvolutionCommand::Plan {
                objective,
                max_generated_tasks,
                max_selected_tasks,
                max_tool_calls_per_task,
                max_runtime_ms_per_task,
            } => {
                print_command_ack(
                    &transport,
                    command(
                        session_id,
                        SessionCommandKind::PlanEvolution,
                        serde_json::json!({
                            "objective": objective,
                            "budget": {
                                "max_generated_tasks": max_generated_tasks,
                                "max_selected_tasks": max_selected_tasks,
                                "max_tool_calls_per_task": max_tool_calls_per_task,
                                "max_runtime_ms_per_task": max_runtime_ms_per_task,
                            },
                        }),
                    ),
                )
                .await?;
            }
            EvolutionCommand::Run { run_id } => {
                print_command_ack(
                    &transport,
                    command(
                        session_id,
                        SessionCommandKind::RunEvolution,
                        serde_json::json!({"run_id": run_id}),
                    ),
                )
                .await?;
            }
            EvolutionCommand::Skill { command: skill } => match skill {
                EvolutionSkillCommand::Stage { candidate_id } => {
                    print_command_ack(
                        &transport,
                        command(
                            session_id,
                            SessionCommandKind::StageSkill,
                            serde_json::json!({"candidate_id": candidate_id}),
                        ),
                    )
                    .await?;
                }
                EvolutionSkillCommand::Review {
                    skill_id,
                    decision,
                    reason,
                    regression_refs,
                } => {
                    print_command_ack(
                        &transport,
                        command(
                            session_id,
                            SessionCommandKind::ReviewSkill,
                            serde_json::json!({
                                "skill_id": skill_id,
                                "decision": decision.as_str(),
                                "reason": reason,
                                "regression_refs": regression_refs,
                            }),
                        ),
                    )
                    .await?;
                }
                EvolutionSkillCommand::Install { skill_id } => {
                    print_command_ack(
                        &transport,
                        command(
                            session_id,
                            SessionCommandKind::InstallSkill,
                            serde_json::json!({"skill_id": skill_id}),
                        ),
                    )
                    .await?;
                }
                EvolutionSkillCommand::Rollback { skill_id, reason } => {
                    print_command_ack(
                        &transport,
                        command(
                            session_id,
                            SessionCommandKind::RollbackSkill,
                            serde_json::json!({"skill_id": skill_id, "reason": reason}),
                        ),
                    )
                    .await?;
                }
            },
        },
        Command::Storage { command: storage } => match storage {
            StorageCommand::Status => {
                print_runtime_query(&transport, session_id, RuntimeQueryKind::StorageStatus)
                    .await?;
            }
            StorageCommand::Clean => {
                print_command_ack(
                    &transport,
                    command(
                        session_id,
                        SessionCommandKind::RunStorageMaintenance,
                        serde_json::json!({}),
                    ),
                )
                .await?;
            }
        },
        Command::Code { command: code } => {
            let cwd = transport
                .cwd()
                .ok_or_else(|| miette::miette!("code index requires a cwd"))?;
            let paths = golutra_client::RuntimePaths::for_cwd(cwd)
                .map_err(|error| miette::miette!("{error}"))?;
            let indexer = golutra_code_intelligence::CodeIntelligence::new(&paths.cwd)
                .map_err(|error| miette::miette!("{error}"))?;
            let store = golutra_code_intelligence::CodeIndexStore::new(&paths.code_index_file);
            match code {
                CodeCommand::Index => {
                    let graph = indexer
                        .build()
                        .map_err(|error| miette::miette!("{error}"))?;
                    store
                        .save(&graph)
                        .map_err(|error| miette::miette!("{error}"))?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "path": paths.code_index_file,
                            "files_indexed": graph.files_indexed,
                            "symbols": graph.symbols.len(),
                            "references": graph.references.len(),
                            "source_digest": graph.source_digest,
                        }))
                        .unwrap_or_default()
                    );
                }
                CodeCommand::Symbols { query, limit } => {
                    let graph = load_or_build_code_graph(&indexer, &store)?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &golutra_code_intelligence::CodeIntelligence::query_symbols(
                                &graph, &query, limit,
                            ),
                        )
                        .unwrap_or_default()
                    );
                }
                CodeCommand::References { symbol, limit } => {
                    let graph = load_or_build_code_graph(&indexer, &store)?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &golutra_code_intelligence::CodeIntelligence::query_references(
                                &graph, &symbol, limit,
                            ),
                        )
                        .unwrap_or_default()
                    );
                }
            }
        }
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

fn load_or_build_code_graph(
    indexer: &golutra_code_intelligence::CodeIntelligence,
    store: &golutra_code_intelligence::CodeIndexStore,
) -> miette::Result<golutra_code_intelligence::CodeGraph> {
    if let Some(graph) = store.load().map_err(|error| miette::miette!("{error}"))? {
        return Ok(graph);
    }
    let graph = indexer
        .build()
        .map_err(|error| miette::miette!("{error}"))?;
    store
        .save(&graph)
        .map_err(|error| miette::miette!("{error}"))?;
    Ok(graph)
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

fn load_oauth_descriptor(path: &std::path::Path) -> miette::Result<OAuthProviderDescriptor> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| miette::miette!("failed to read OAuth descriptor: {error}"))?;
    let descriptor: OAuthProviderDescriptor = serde_json::from_str(&content)
        .map_err(|error| miette::miette!("OAuth descriptor JSON is invalid: {error}"))?;
    descriptor
        .validate()
        .map_err(|error| miette::miette!("{error}"))?;
    Ok(descriptor)
}

struct ResolvedOAuthLogin {
    descriptor: OAuthProviderDescriptor,
    flow: OAuthFlow,
    profile: String,
    protocol: ProviderProtocol,
    base_url: String,
    model: String,
}

#[allow(clippy::too_many_arguments)]
fn resolve_oauth_login(
    descriptor_path: Option<&std::path::Path>,
    provider: Option<&str>,
    method: Option<&str>,
    flow: Option<OAuthFlowArg>,
    profile: Option<String>,
    protocol: Option<&str>,
    base_url: Option<String>,
    model: Option<String>,
) -> miette::Result<ResolvedOAuthLogin> {
    if let Some(provider) = provider {
        let method = match method {
            Some(method) => builtin_oauth_method(provider, method).ok_or_else(|| {
                let available = builtin_oauth_methods_for_provider(provider)
                    .into_iter()
                    .map(|method| method.method_id)
                    .collect::<Vec<_>>()
                    .join(", ");
                miette::miette!(
                    "unknown OAuth method `{method}` for `{provider}`; available: {available}"
                )
            })?,
            None => builtin_oauth_methods_for_provider(provider)
                .into_iter()
                .next()
                .ok_or_else(|| {
                    miette::miette!("provider `{provider}` has no builtin OAuth method")
                })?,
        };
        return resolved_builtin_oauth_login(method, flow, profile, protocol, base_url, model);
    }

    let descriptor_path = descriptor_path
        .ok_or_else(|| miette::miette!("oauth-login requires --provider or --descriptor"))?;
    let descriptor = load_oauth_descriptor(descriptor_path)?;
    let flow = flow.unwrap_or(OAuthFlowArg::Browser).auth_flow();
    let protocol = parse_provider_protocol(protocol.unwrap_or("openai-compatible"))?;
    Ok(ResolvedOAuthLogin {
        descriptor,
        flow,
        profile: profile.unwrap_or_else(|| "default".to_owned()),
        protocol,
        base_url: base_url
            .ok_or_else(|| miette::miette!("custom descriptor OAuth login requires --base-url"))?,
        model: model
            .ok_or_else(|| miette::miette!("custom descriptor OAuth login requires --model"))?,
    })
}

fn resolved_builtin_oauth_login(
    method: BuiltinOAuthMethod,
    flow: Option<OAuthFlowArg>,
    profile: Option<String>,
    protocol: Option<&str>,
    base_url: Option<String>,
    model: Option<String>,
) -> miette::Result<ResolvedOAuthLogin> {
    method
        .validate()
        .map_err(|error| miette::miette!("{error}"))?;
    if let Some(flow) = flow
        && flow.auth_flow() != method.flow
    {
        return Err(miette::miette!(
            "builtin OAuth method `{}` uses {:?}; do not override it with {:?}",
            method.method_id,
            method.flow,
            flow.auth_flow()
        ));
    }
    if let Some(protocol) = protocol {
        let protocol = parse_provider_protocol(protocol)?;
        if protocol != method.protocol {
            return Err(miette::miette!(
                "builtin OAuth method `{}` requires protocol `{}`",
                method.method_id,
                method.protocol.id()
            ));
        }
    }
    if let Some(base_url) = base_url.as_deref()
        && base_url.trim_end_matches('/') != method.base_url.trim_end_matches('/')
    {
        return Err(miette::miette!(
            "builtin OAuth method `{}` requires its registered API endpoint",
            method.method_id
        ));
    }
    Ok(ResolvedOAuthLogin {
        descriptor: method.descriptor,
        flow: method.flow,
        profile: profile.unwrap_or(method.profile),
        protocol: method.protocol,
        base_url: base_url.unwrap_or(method.base_url),
        model: model.unwrap_or(method.default_model),
    })
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

async fn notify_provider_configured_cli(transport: &RuntimeTransport) -> miette::Result<()> {
    let ack = transport
        .send_command(command(
            transport.default_session_id(),
            SessionCommandKind::ProviderConfigured,
            serde_json::json!({"verified": true}),
        ))
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    if ack.accepted {
        Ok(())
    } else {
        Err(miette::miette!(
            "runtime rejected provider reload: {}",
            ack.reason.unwrap_or_else(|| "unknown reason".to_owned())
        ))
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

fn provider_headers_from_cli(
    literal_headers: &[String],
    environment_headers: &[String],
) -> miette::Result<Vec<ProviderHeaderConfig>> {
    let mut headers = Vec::with_capacity(literal_headers.len() + environment_headers.len());
    for raw in literal_headers {
        let (name, value) = parse_header_assignment(raw, "--header")?;
        headers.push(ProviderHeaderConfig {
            name,
            value: ProviderHeaderValue::Literal { value },
        });
    }
    for raw in environment_headers {
        let (name, key) = parse_header_assignment(raw, "--header-env")?;
        headers.push(ProviderHeaderConfig {
            name,
            value: ProviderHeaderValue::Environment { key },
        });
    }
    for header in &headers {
        header.validate().map_err(|error| miette::miette!(error))?;
    }
    let mut names = std::collections::BTreeSet::new();
    for header in &headers {
        if !names.insert(header.name.to_ascii_lowercase()) {
            return Err(miette::miette!(
                "provider header `{}` is configured more than once",
                header.name
            ));
        }
    }
    Ok(headers)
}

fn parse_header_assignment(raw: &str, flag: &str) -> miette::Result<(String, String)> {
    let (name, value) = raw
        .split_once('=')
        .ok_or_else(|| miette::miette!("{flag} requires NAME=VALUE"))?;
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() || value.is_empty() {
        return Err(miette::miette!("{flag} requires non-empty NAME=VALUE"));
    }
    Ok((name.to_owned(), value.to_owned()))
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

fn parse_turn_id(value: &str) -> miette::Result<TurnId> {
    value
        .parse()
        .map_err(|error: uuid::Error| miette::miette!("invalid turn id: {error}"))
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
    let mut reported_auth_required = false;
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
        if status == Some(TaskStatus::WaitingAuthentication) {
            if !reported_auth_required {
                eprintln!(
                    "provider authentication is required; run `golutra provider login ...` in another terminal"
                );
                reported_auth_required = true;
            }
        } else {
            reported_auth_required = false;
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

fn run_plugin_command(command: &PluginCommand) -> miette::Result<()> {
    let home = golutra_home().map_err(|error| miette::miette!("{error}"))?;
    let store = PluginStore::new(home).map_err(|error| miette::miette!("{error}"))?;
    let value = match command {
        PluginCommand::List => {
            serde_json::to_value(store.state().map_err(|error| miette::miette!("{error}"))?)
        }
        PluginCommand::Stage { package } => serde_json::to_value(
            store
                .stage(package)
                .map_err(|error| miette::miette!("{error}"))?,
        ),
        PluginCommand::Review {
            plugin_id,
            revision_id,
        } => serde_json::to_value(
            store
                .review(plugin_id, revision_id)
                .map_err(|error| miette::miette!("{error}"))?,
        ),
        PluginCommand::Enable {
            plugin_id,
            revision_id,
        } => serde_json::to_value(
            store
                .enable(plugin_id, revision_id)
                .map_err(|error| miette::miette!("{error}"))?,
        ),
        PluginCommand::Disable { plugin_id } => serde_json::to_value(
            store
                .disable(plugin_id)
                .map_err(|error| miette::miette!("{error}"))?,
        ),
        PluginCommand::Rollback { plugin_id } => serde_json::to_value(
            store
                .rollback(plugin_id)
                .map_err(|error| miette::miette!("{error}"))?,
        ),
    }
    .map_err(|error| miette::miette!("failed to encode plugin result: {error}"))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&value)
            .map_err(|error| miette::miette!("failed to encode plugin result: {error}"))?
    );
    Ok(())
}

#[cfg(test)]
mod tests;
