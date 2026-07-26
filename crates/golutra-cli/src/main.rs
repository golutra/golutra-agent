use clap::{Args, Parser, Subcommand, ValueEnum};
use golutra_auth::{
    CredentialRef, CredentialSource, OAuthFlow, OAuthProviderDescriptor, SecretKind,
};
use golutra_client::{
    AgentClient, DebugExportCoordinator, DebugExportRequest, RunBundleExportRequest,
    RunBundleExporter, RunBundleTerminalOutcome, RuntimeClient, RuntimeExecutionOptions,
    RuntimeTransport, TaskTraceClient, parse_session_range,
};
use golutra_config::{
    BuiltinOAuthMethod, ProviderConfigPaths, ProviderConfigScope, ProviderInstallPlan,
    ProviderProfile, apply_oauth_provider_install_plan_verified,
    apply_provider_install_plan_verified, builtin_oauth_method, builtin_oauth_methods,
    builtin_oauth_methods_for_provider, golutra_home, load_provider_runtime_env,
    load_provider_settings, logout_provider_profile_verified, provider_auth_service,
    provider_onboarding_state, replace_provider_credential_verified,
    update_provider_settings_verified, validate_provider_protocol_runtime_supported,
};
use golutra_core::{
    Actor, ActorKind, CommandId, SessionId, TaskId, TaskReconciliationDecision, TaskStatus,
    ThreadId, TraceView, TurnId,
};
use golutra_eval::{ExternalEvaluationRecord, external_evaluation_result_digest};
use golutra_llm::{
    ConfiguredProvider, ProviderGenerationConfig, ProviderHeaderConfig, ProviderHeaderValue,
    ProviderProtocol, ProviderReasoningEffort, provider_protocol_catalog,
};
use golutra_plugin::PluginStore;
use golutra_protocol::{
    AgentStreamEvent, AgentTurnOptions, EventFilter, ExternalVerificationSpec, RuntimeEvent,
    RuntimeEventType, RuntimeQuery, RuntimeQueryKind, SessionCommand, SessionCommandKind,
    TaskTraceRequest,
};
use secrecy::SecretString;
use std::io::{IsTerminal, Write};
use tokio::io::AsyncReadExt;
use tokio::time::{Duration, sleep};
use uuid::Uuid;

const CLI_ACTOR_ID: &str = "golutra-cli";

mod mcp_server;

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
    /// Reopen a completed or checkpointed owner-only `exec --run-dir` bundle for evaluator ingestion.
    #[arg(
        long,
        global = true,
        value_name = "DIR",
        conflicts_with_all = ["daemon", "connect"]
    )]
    run_bundle: Option<std::path::PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    AppServer {
        #[arg(long, env = "GOLUTRA_APP_ADDR", default_value = "127.0.0.1:47831")]
        addr: std::net::SocketAddr,
        #[arg(long, conflicts_with = "addr")]
        stdio: bool,
    },
    Chat {
        #[arg(default_value = "")]
        prompt: String,
    },
    /// Run one agent turn without opening the interactive TUI.
    Exec(ExecArgs),
    /// Expose the shared Agent Runtime as an MCP stdio server.
    McpServer(McpServerArgs),
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
    Reconcile {
        task_id: Option<String>,
        #[arg(long, value_enum)]
        decision: ReconciliationDecisionArg,
        #[arg(long)]
        note: Option<String>,
    },
    Takeover,
    Pause,
    Approve {
        approval_id: Option<String>,
    },
    Deny {
        approval_id: Option<String>,
    },
    Compact,
    Trace {
        #[arg(long)]
        task_id: Option<String>,
        #[arg(long)]
        full: bool,
        #[arg(long)]
        wait_evaluation: bool,
    },
    /// Show the bounded diagnosis and replay inputs for a completed task.
    Diagnose {
        #[arg(long)]
        task_id: Option<String>,
    },
    /// Re-enter AgentLoop with the task's recorded provider/tool artifacts.
    Replay {
        #[arg(long)]
        task_id: Option<String>,
        #[arg(long)]
        capsule_id: Option<String>,
    },
    /// Compare a deterministic replay with the source task outcome.
    Compare {
        #[arg(long)]
        task_id: Option<String>,
        #[arg(long)]
        execution_id: Option<String>,
    },
    Export {
        #[arg(value_name = "DESTINATION")]
        destination: std::path::PathBuf,
        #[arg(long)]
        thread_id: Option<String>,
        #[arg(long, default_value = "1")]
        range: String,
    },
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
    #[command(alias = "evaluation")]
    Eval {
        #[command(subcommand)]
        command: EvalCommand,
    },
    /// Run an execution-backed, coverage-gated regression campaign.
    Campaign {
        #[command(subcommand)]
        command: CampaignCommand,
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ReconciliationDecisionArg {
    NoSideEffectObserved,
    SideEffectObserved,
    Abandon,
}

impl From<ReconciliationDecisionArg> for TaskReconciliationDecision {
    fn from(value: ReconciliationDecisionArg) -> Self {
        match value {
            ReconciliationDecisionArg::NoSideEffectObserved => Self::NoSideEffectObserved,
            ReconciliationDecisionArg::SideEffectObserved => Self::SideEffectObserved,
            ReconciliationDecisionArg::Abandon => Self::Abandon,
        }
    }
}

#[derive(Debug, Clone, Args)]
struct ExecArgs {
    /// Resume an existing thread instead of creating a new one.
    #[command(subcommand)]
    command: Option<ExecCommand>,
    /// Initial instructions. Use `-` or omit it when stdin is piped.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,
    /// Emit lifecycle and item events as compact JSONL on stdout.
    #[arg(long, alias = "experimental-json")]
    json: bool,
    /// Do not persist runtime state after this process exits.
    #[arg(long)]
    ephemeral: bool,
    /// Allow this embedded run's child tools to access the network.
    #[arg(long)]
    allow_network: bool,
    /// Write isolated runtime state and full owner-only observations to this new directory.
    /// Implies --ephemeral. The legacy --ephemeral-state-dir spelling remains accepted.
    #[arg(
        long = "run-dir",
        visible_alias = "ephemeral-state-dir",
        value_name = "DIR"
    )]
    run_dir: Option<std::path::PathBuf>,
    /// JSON Schema file for the final response.
    #[arg(long, value_name = "FILE")]
    output_schema: Option<std::path::PathBuf>,
    /// Write the final assistant message to a file.
    #[arg(short = 'o', long, value_name = "FILE")]
    output_last_message: Option<std::path::PathBuf>,
    /// Add an objective completion criterion. May be repeated.
    #[arg(long = "completion-criterion", value_name = "TEXT")]
    completion_criteria: Vec<String>,
    /// Run this caller-trusted program after the agent stops. No shell is used.
    #[arg(long, value_name = "PROGRAM")]
    verify_program: Option<String>,
    /// Append one argv element to the verifier command. May be repeated.
    #[arg(
        long,
        value_name = "ARG",
        requires = "verify_program",
        allow_hyphen_values = true
    )]
    verify_arg: Vec<String>,
    /// Workspace-relative verifier working directory.
    #[arg(
        long,
        value_name = "PATH",
        default_value = ".",
        requires = "verify_program"
    )]
    verify_cwd: std::path::PathBuf,
    #[arg(long, default_value_t = 120_000, requires = "verify_program")]
    verify_timeout_ms: u64,
    #[arg(long, default_value_t = 0, requires = "verify_program")]
    verify_expected_exit_code: i32,
    #[arg(long, default_value_t = 256 * 1024, requires = "verify_program")]
    verify_max_output_bytes: usize,
    /// How exec resolves runtime requests already classified as requiring approval.
    #[arg(long, value_enum, default_value_t = ExecApprovalModeArg::Prompt)]
    approval_mode: ExecApprovalModeArg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ExecApprovalModeArg {
    /// Ask on a terminal; deny when stdin is not interactive.
    Prompt,
    /// Deny every approval request.
    Deny,
    /// Approve `Ask` decisions. Policy-blocked actions remain blocked.
    Auto,
}

impl std::fmt::Display for ExecApprovalModeArg {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Prompt => "prompt",
            Self::Deny => "deny",
            Self::Auto => "auto",
        })
    }
}

#[derive(Debug, Clone, Args)]
struct McpServerArgs {
    /// Use an in-process runtime instead of the user-level app server.
    #[arg(long)]
    embedded: bool,
}

#[derive(Debug, Clone, Subcommand)]
enum ExecCommand {
    /// Resume a thread and optionally send a new prompt.
    Resume {
        thread_id: String,
        #[arg(value_name = "PROMPT")]
        prompt: Option<String>,
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
        #[arg(long, value_name = "JSON_FILE")]
        candidate_files: std::path::PathBuf,
        #[arg(long)]
        candidate_digest: Option<String>,
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
    /// Ingest a grader/evaluator result and bind it to a canonical task trace.
    Ingest {
        #[arg(value_name = "JSON_FILE")]
        file: std::path::PathBuf,
        /// Resolve relative evaluator evidence references from this directory.
        #[arg(long, value_name = "DIR")]
        artifact_base: Option<std::path::PathBuf>,
    },
    CompareCounterfactual {
        group_id: String,
    },
}

#[derive(Debug, Subcommand)]
enum CampaignCommand {
    Run {
        candidate_id: String,
        #[arg(long, value_name = "JSON_FILE")]
        candidate_files: std::path::PathBuf,
        #[arg(long)]
        candidate_digest: Option<String>,
        #[arg(long, value_delimiter = ',')]
        case_refs: Vec<String>,
        #[arg(long, value_delimiter = ',', value_enum)]
        required_partitions: Vec<EvaluationPartitionArg>,
        #[arg(long, value_delimiter = ',', default_value = "isolated-mock")]
        provider_matrix: Vec<String>,
        #[arg(long, value_delimiter = ',', default_value = "0")]
        seeds: Vec<u64>,
        #[arg(
            long,
            visible_alias = "minimum-trusted-external-evaluations",
            default_value_t = 0
        )]
        minimum_trusted_external_pairs: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum EvaluationPartitionArg {
    Source,
    Historical,
    Generated,
    Holdout,
    Adversarial,
}

impl EvaluationPartitionArg {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Historical => "historical",
            Self::Generated => "generated",
            Self::Holdout => "holdout",
            Self::Adversarial => "adversarial",
        }
    }
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
    if let Command::AppServer { addr, stdio } = &cli.command {
        return if *stdio {
            golutra_app_server::run_stdio().await
        } else {
            golutra_app_server::run(*addr).await
        };
    }
    if let Command::Plugin { command } = &cli.command {
        return run_plugin_command(command);
    }
    if let Command::McpServer(args) = &cli.command {
        let cwd = cli
            .cwd
            .clone()
            .map_or_else(std::env::current_dir, Ok)
            .map_err(|error| miette::miette!("{error}"))?;
        return mcp_server::run(mcp_server::Config {
            cwd,
            connect: cli.connect.clone(),
            daemon: cli.daemon,
            embedded: args.embedded,
        })
        .await
        .map_err(|error| miette::miette!("{error}"));
    }
    let cwd = cli
        .cwd
        .clone()
        .map_or_else(std::env::current_dir, Ok)
        .map_err(|error| miette::miette!("{error}"))?;
    let opened_run_bundle = cli.run_bundle.clone();
    let ephemeral_exec =
        matches!(&cli.command, Command::Exec(args) if args.ephemeral || args.run_dir.is_some());
    let allow_network = matches!(&cli.command, Command::Exec(args) if args.allow_network);
    let run_dir = match &cli.command {
        Command::Exec(args) => args.run_dir.clone(),
        _ => None,
    };
    if cli.run_bundle.is_some() && !command_allows_persisted_run(&cli.command) {
        return Err(miette::miette!(
            "--run-bundle only supports status, trace, diagnose, compare, and eval ingest/results commands"
        ));
    }
    if ephemeral_exec && (cli.daemon || cli.connect.is_some()) {
        return Err(miette::miette!(
            "exec --ephemeral or --run-dir cannot be combined with --daemon or --connect"
        ));
    }
    if allow_network && (cli.daemon || cli.connect.is_some()) {
        return Err(miette::miette!(
            "exec --allow-network requires an embedded runtime; configure network capability on the app-server host before using --daemon or --connect"
        ));
    }
    let execution_options = RuntimeExecutionOptions::with_network_access(allow_network);
    let transport = if let Some(run_bundle) = cli.run_bundle.as_ref() {
        RuntimeTransport::open_persisted_run(run_bundle).await
    } else if let Some(state_dir) = run_dir.as_ref() {
        RuntimeTransport::ephemeral_persistent_for_cwd_with_options(
            &cwd,
            state_dir,
            execution_options,
        )
        .await
    } else if ephemeral_exec {
        RuntimeTransport::ephemeral_for_cwd_with_options(&cwd, execution_options).await
    } else if let Some(base_url) = cli.connect.clone() {
        RuntimeTransport::connect(base_url, &cwd).await
    } else if cli.daemon {
        RuntimeTransport::local_daemon(&cwd).await
    } else {
        RuntimeTransport::for_cwd_with_options(&cwd, execution_options).await
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
        Command::Exec(args) => run_exec(&transport, args).await?,
        Command::McpServer(_) => unreachable!("mcp-server exits before runtime setup"),
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
        Command::Reconcile {
            task_id,
            decision,
            note,
        } => {
            let ack = transport
                .send_command(command(
                    session_id,
                    SessionCommandKind::ReconcileTask,
                    serde_json::json!({
                        "task_id": task_id,
                        "decision": TaskReconciliationDecision::from(decision),
                        "note": note,
                    }),
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
        Command::Trace {
            task_id,
            full,
            wait_evaluation,
        } => {
            let task_id = match task_id.as_deref() {
                Some(task_id) => parse_task_id(task_id)?,
                None => {
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
                    state
                        .get("active_task_id")
                        .and_then(|value| value.as_str())
                        .map(parse_task_id)
                        .transpose()?
                        .ok_or_else(|| {
                            miette::miette!("trace requires an active or explicit task_id")
                        })?
                }
            };
            let view = if full {
                TraceView::Full
            } else {
                TraceView::Summary
            };
            let limit = if full { 512 } else { 64 };
            let request = TaskTraceRequest {
                session_id,
                task_id,
                view,
                cursor: None,
                limit,
                wait_for_evaluation: wait_evaluation,
            };
            let trace = if full {
                transport.complete_task_trace(request).await
            } else {
                transport.task_trace(request).await
            }
            .map_err(|error| miette::miette!("{error}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&trace).unwrap_or_default()
            );
        }
        Command::Diagnose { task_id } => {
            let task_id = resolve_cli_task_id(&transport, session_id, task_id.as_deref()).await?;
            let projection = query_task_evaluation(&transport, session_id, task_id).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "task_id": task_id,
                    "failure_diagnoses": projection
                        .get("failure_diagnoses")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                    "diagnostic_slices": projection
                        .get("diagnostic_slices")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                    "replay_capsules": projection
                        .get("replay_capsules")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                    "external_evaluations": projection
                        .get("external_evaluations")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                    "integrity_warnings": projection
                        .get("integrity_warnings")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                }))
                .unwrap_or_default()
            );
        }
        Command::Replay {
            task_id,
            capsule_id,
        } => {
            let task_id = resolve_cli_task_id(&transport, session_id, task_id.as_deref()).await?;
            let ack = transport
                .send_command(command(
                    session_id,
                    SessionCommandKind::Replay,
                    serde_json::json!({
                        "task_id": task_id,
                        "capsule_id": capsule_id,
                    }),
                ))
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            if !ack.accepted {
                return Err(miette::miette!(
                    "replay was rejected: {}",
                    ack.reason.unwrap_or_else(|| "unknown reason".to_owned())
                ));
            }
            let projection = query_task_evaluation(&transport, session_id, task_id).await?;
            let execution = projection
                .get("replay_executions")
                .and_then(serde_json::Value::as_array)
                .and_then(|executions| executions.last())
                .cloned()
                .ok_or_else(|| miette::miette!("replay completed without a durable result"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&execution).unwrap_or_default()
            );
        }
        Command::Compare {
            task_id,
            execution_id,
        } => {
            let task_id = resolve_cli_task_id(&transport, session_id, task_id.as_deref()).await?;
            let projection = query_task_evaluation(&transport, session_id, task_id).await?;
            let executions = projection
                .get("replay_executions")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let execution = executions
                .iter()
                .rev()
                .find(|execution| {
                    execution_id.as_deref().is_none_or(|execution_id| {
                        execution
                            .get("execution_id")
                            .and_then(serde_json::Value::as_str)
                            == Some(execution_id)
                    })
                })
                .cloned()
                .ok_or_else(|| miette::miette!("no matching replay execution was found"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "task_id": task_id,
                    "replay": execution,
                    "external_evaluations": projection
                        .get("external_evaluations")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                }))
                .unwrap_or_default()
            );
        }
        Command::Export {
            destination,
            thread_id,
            range,
        } => {
            let anchor_thread_id = parse_optional_thread_id(thread_id.as_deref(), &transport)?;
            let range = parse_session_range(&range).map_err(|error| miette::miette!("{error}"))?;
            let receipt = DebugExportCoordinator::new(&transport)
                .export(DebugExportRequest {
                    selection: golutra_protocol::SessionWindowRequest {
                        anchor_thread_id,
                        range,
                    },
                    destination,
                })
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&receipt).unwrap_or_default()
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
            EvalCommand::Regress {
                candidate_id,
                candidate_files,
                candidate_digest,
            } => {
                let content = std::fs::read_to_string(&candidate_files).map_err(|error| {
                    miette::miette!(
                        "failed to read candidate files {}: {error}",
                        candidate_files.display()
                    )
                })?;
                let files: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_str(&content).map_err(|error| {
                        miette::miette!("candidate files JSON is invalid: {error}")
                    })?;
                print_command_ack(
                    &transport,
                    command(
                        session_id,
                        SessionCommandKind::RunRegression,
                        serde_json::json!({
                            "candidate_id": candidate_id,
                            "candidate_files": files,
                            "candidate_digest": candidate_digest,
                        }),
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
            EvalCommand::Ingest {
                file,
                artifact_base,
            } => {
                let artifact_base_path =
                    evaluation_artifact_base_path(&file, artifact_base.as_deref()).map_err(
                        |error| {
                            miette::miette!(
                                "failed to resolve evaluation artifact base for {}: {error}",
                                file.display()
                            )
                        },
                    )?;
                let content = std::fs::read_to_string(&file).map_err(|error| {
                    miette::miette!(
                        "failed to read external evaluation {}: {error}",
                        file.display()
                    )
                })?;
                let mut value: serde_json::Value = serde_json::from_str(&content)
                    .map_err(|error| miette::miette!("evaluation JSON is invalid: {error}"))?;
                let task_id = value
                    .get("source_task_id")
                    .and_then(serde_json::Value::as_str)
                    .map(parse_task_id)
                    .transpose()?
                    .ok_or_else(|| {
                        miette::miette!("external evaluation source_task_id is required")
                    })?;
                let trace = transport
                    .complete_task_trace(TaskTraceRequest {
                        session_id,
                        task_id,
                        view: TraceView::Full,
                        cursor: None,
                        limit: 512,
                        wait_for_evaluation: true,
                    })
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                if value
                    .get("base_trace_digest")
                    .and_then(serde_json::Value::as_str)
                    .is_none_or(|value| value.is_empty() || value == "auto")
                {
                    value["base_trace_digest"] =
                        serde_json::Value::String(trace.integrity.event_chain_digest);
                }
                if value
                    .get("runtime_identity")
                    .and_then(serde_json::Value::as_str)
                    .is_none_or(|value| value.is_empty() || value == "auto")
                {
                    value["runtime_identity"] = serde_json::Value::String(trace.runtime_identity);
                }
                if value.get("trust").is_none() {
                    value["trust"] = serde_json::Value::String("owner_local".to_owned());
                }
                value["ingested_at"] = serde_json::Value::String(chrono::Utc::now().to_rfc3339());
                if value.get("result_digest").is_none() {
                    value["result_digest"] = serde_json::Value::String(String::new());
                }
                let mut record: ExternalEvaluationRecord = serde_json::from_value(value)
                    .map_err(|error| miette::miette!("evaluation record is invalid: {error}"))?;
                if record.result_digest.is_empty() || record.result_digest == "auto" {
                    record.result_digest = external_evaluation_result_digest(&record);
                }
                print_command_ack(
                    &transport,
                    command(
                        session_id,
                        SessionCommandKind::IngestExternalEvaluation,
                        serde_json::json!({
                            "record": record,
                            "artifact_base_path": artifact_base_path,
                        }),
                    ),
                )
                .await?;
                if let Some(destination) = opened_run_bundle.as_ref() {
                    let receipt = RunBundleExporter::new(&transport)
                        .refresh(destination)
                        .await
                        .map_err(|error| {
                            miette::miette!(
                                "external evaluation was ingested but run bundle refresh failed: {error}"
                            )
                        })?;
                    eprintln!(
                        "golutra run bundle observations refreshed at {}; complete: {}",
                        destination.display(),
                        receipt.complete
                    );
                }
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
        Command::Campaign { command: campaign } => match campaign {
            CampaignCommand::Run {
                candidate_id,
                candidate_files,
                candidate_digest,
                case_refs,
                required_partitions,
                provider_matrix,
                seeds,
                minimum_trusted_external_pairs,
            } => {
                let files = read_candidate_files(&candidate_files)?;
                print_command_ack(
                    &transport,
                    command(
                        session_id,
                        SessionCommandKind::RunRegressionCampaign,
                        serde_json::json!({
                            "candidate_id": candidate_id,
                            "candidate_files": files,
                            "candidate_digest": candidate_digest,
                            "case_refs": case_refs,
                            "required_partitions": required_partitions
                                .into_iter()
                                .map(EvaluationPartitionArg::as_str)
                                .collect::<Vec<_>>(),
                            "provider_matrix": provider_matrix,
                            "seeds": seeds,
                            "minimum_trusted_external_pairs": minimum_trusted_external_pairs,
                        }),
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

fn command_allows_persisted_run(command: &Command) -> bool {
    matches!(
        command,
        Command::Status
            | Command::Trace { .. }
            | Command::Diagnose { .. }
            | Command::Compare { .. }
            | Command::Eval {
                command: EvalCommand::Results
                    | EvalCommand::Improvements
                    | EvalCommand::Candidates
                    | EvalCommand::Ingest { .. }
                    | EvalCommand::CompareCounterfactual { .. },
            }
    )
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

async fn resolve_cli_task_id(
    transport: &RuntimeTransport,
    session_id: SessionId,
    value: Option<&str>,
) -> miette::Result<TaskId> {
    if let Some(value) = value {
        return parse_task_id(value);
    }
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
    state
        .get("active_task_id")
        .and_then(serde_json::Value::as_str)
        .map(parse_task_id)
        .transpose()?
        .ok_or_else(|| miette::miette!("an explicit task_id is required"))
}

async fn query_task_evaluation(
    transport: &RuntimeTransport,
    session_id: SessionId,
    task_id: TaskId,
) -> miette::Result<serde_json::Value> {
    transport
        .query(RuntimeQuery {
            query_id: golutra_core::QueryId::new(),
            session_id,
            task_id: Some(task_id),
            kind: RuntimeQueryKind::EvaluationProjection,
            requester: ActorKind::Cli,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .map_err(|error| miette::miette!("{error}"))
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

fn read_candidate_files(
    path: &std::path::Path,
) -> miette::Result<serde_json::Map<String, serde_json::Value>> {
    let content = std::fs::read_to_string(path).map_err(|error| {
        miette::miette!("failed to read candidate files {}: {error}", path.display())
    })?;
    serde_json::from_str(&content)
        .map_err(|error| miette::miette!("candidate files JSON is invalid: {error}"))
}

fn parse_task_id(value: &str) -> miette::Result<TaskId> {
    value
        .parse::<TaskId>()
        .map_err(|error| miette::miette!("invalid task id `{value}`: {error}"))
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

async fn run_exec(transport: &RuntimeTransport, args: ExecArgs) -> miette::Result<()> {
    let external_verifiers = args
        .verify_program
        .clone()
        .map_or_else(Vec::new, |program| {
            vec![ExternalVerificationSpec {
                program,
                args: args.verify_arg.clone(),
                cwd: args.verify_cwd.display().to_string(),
                timeout_ms: args.verify_timeout_ms,
                expected_exit_code: args.verify_expected_exit_code,
                max_output_bytes: args.verify_max_output_bytes,
            }]
        });
    let prompt = match &args.command {
        Some(ExecCommand::Resume { prompt, .. }) => prompt.clone().or(args.prompt.clone()),
        None => args.prompt.clone(),
    };
    let prompt = read_exec_prompt(prompt).await?;
    if prompt.trim().is_empty() {
        return Err(miette::miette!(
            "exec requires PROMPT or piped stdin (use `-` to read stdin)"
        ));
    }
    let output_schema = match args.output_schema {
        Some(path) => {
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|error| miette::miette!("{}: {error}", path.display()))?;
            Some(serde_json::from_slice(&bytes).map_err(|error| {
                miette::miette!("{} is not valid JSON: {error}", path.display())
            })?)
        }
        None => None,
    };
    let json_output = args.json;
    let output_last_message = args.output_last_message;
    let run_dir = args.run_dir;
    let approval_mode = args.approval_mode;
    let client = AgentClient::new(transport.clone());
    let (thread, prompt) = match args.command {
        Some(ExecCommand::Resume { thread_id, .. }) => {
            let thread_id = thread_id
                .parse()
                .map_err(|error| miette::miette!("invalid thread id `{thread_id}`: {error}"))?;
            (
                client
                    .resume_thread(thread_id)
                    .await
                    .map_err(|error| miette::miette!("{error}"))?,
                prompt,
            )
        }
        None => (
            client
                .start_thread()
                .await
                .map_err(|error| miette::miette!("{error}"))?,
            prompt,
        ),
    };
    let mut handle = thread
        .start_turn(
            prompt,
            AgentTurnOptions {
                output_schema,
                completion_criteria: args.completion_criteria,
                allow_network: args.allow_network,
                external_verifiers,
            },
        )
        .await
        .map_err(|error| miette::miette!("{error}"))?;

    // Leave a recoverable identity/event boundary before consuming the turn.
    // An external harness can terminate the caller while the runtime is still
    // working, so a later collector must be able to reopen this run.
    if let Some(destination) = run_dir.as_ref() {
        if let Err(error) =
            checkpoint_exec_run_bundle(transport, thread.thread_id(), destination.clone()).await
        {
            let _ = handle.interrupt().await;
            return Err(miette::miette!(
                "initial runtime data checkpoint failed: {error}"
            ));
        }
    }

    let turn_result = async {
        let mut interrupt_requested = false;
        loop {
            let next_event = tokio::select! {
                event = handle.next_event() => event,
                signal = tokio::signal::ctrl_c() => {
                    signal.map_err(|error| golutra_client::ClientError::TaskExecution(
                        format!("failed to listen for Ctrl+C: {error}"),
                    ))?;
                    if interrupt_requested {
                        return Err(golutra_client::ClientError::TaskExecution(
                            "exec interrupted while waiting for runtime abort".to_owned(),
                        ));
                    }
                    let ack = handle.interrupt().await?;
                    if !ack.accepted {
                        return Err(golutra_client::ClientError::TaskExecution(format!(
                            "runtime rejected interrupt: {}",
                            ack.reason.unwrap_or_else(|| "unknown reason".to_owned())
                        )));
                    }
                    interrupt_requested = true;
                    eprintln!("interrupt requested; waiting for runtime to settle");
                    continue;
                }
            }?;
            let Some(event) = next_event else {
                break;
            };
            if json_output {
                println!("{}", serde_json::to_string(&event)?);
            } else {
                report_exec_progress(&event);
            }
            if let Some(approval_id) = approval_id_from_exec_event(&event) {
                let approve = resolve_exec_approval(&approval_id, approval_mode)
                    .await
                    .map_err(|error| {
                        golutra_client::ClientError::TaskExecution(error.to_string())
                    })?;
                let ack = match handle.resolve_approval(approval_id, approve).await {
                    Ok(ack) => ack,
                    Err(error) if interrupt_requested => {
                        eprintln!(
                            "approval resolution stopped after interrupt; waiting for terminal runtime event: {error}"
                        );
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if !ack.accepted {
                    if interrupt_requested {
                        continue;
                    }
                    return Err(golutra_client::ClientError::TaskExecution(format!(
                        "runtime rejected approval resolution: {}",
                        ack.reason.unwrap_or_else(|| "unknown reason".to_owned())
                    )));
                }
            }
        }
        handle.wait().await
    }
    .await;

    let export_result = if let Some(destination) = run_dir {
        let terminal_outcome = match &turn_result {
            Ok(result) => RunBundleTerminalOutcome::Result {
                result: result.clone(),
            },
            Err(error) => RunBundleTerminalOutcome::Error {
                error: error.to_string(),
            },
        };
        export_exec_run_bundle(transport, thread.thread_id(), destination, terminal_outcome).await
    } else {
        Ok(())
    };

    let result = match (turn_result, export_result) {
        (Err(turn_error), Err(export_error)) => {
            return Err(miette::miette!(
                "agent turn failed: {turn_error}; runtime data export failed: {export_error}"
            ));
        }
        (Err(error), Ok(())) => return Err(miette::miette!("{error}")),
        (Ok(_), Err(error)) => return Err(miette::miette!("runtime data export failed: {error}")),
        (Ok(result), Ok(())) => result,
    };
    if let Some(path) = output_last_message {
        tokio::fs::write(&path, result.final_message.as_deref().unwrap_or_default())
            .await
            .map_err(|error| miette::miette!("{}: {error}", path.display()))?;
    }
    if !json_output && let Some(message) = &result.final_message {
        println!("{message}");
    }
    if result.status != TaskStatus::Completed {
        return Err(miette::miette!(
            "agent turn ended with status {:?}",
            result.status
        ));
    }
    Ok(())
}

async fn export_exec_run_bundle(
    transport: &RuntimeTransport,
    thread_id: ThreadId,
    destination: std::path::PathBuf,
    terminal_outcome: RunBundleTerminalOutcome,
) -> Result<(), golutra_client::ClientError> {
    let receipt = RunBundleExporter::new(transport)
        .export(RunBundleExportRequest {
            destination: destination.clone(),
            selection: golutra_client::SessionWindowRequest {
                anchor_thread_id: thread_id,
                range: golutra_client::SessionRangeSpec {
                    direction: golutra_client::SessionRangeDirection::Single,
                    count: 1,
                },
            },
            terminal_outcome,
        })
        .await?;
    let debug_export = receipt
        .debug_export_path
        .as_deref()
        .map(|path| destination.join(path).display().to_string())
        .unwrap_or_else(|| "unavailable".to_owned());
    eprintln!(
        "golutra run bundle retained at {}; observations: {}; redacted debug export: {}; complete: {}",
        destination.display(),
        destination.join(&receipt.observations_path).display(),
        debug_export,
        receipt.complete,
    );
    if let Some(error) = receipt.debug_export_error {
        eprintln!("redacted debug export failed without losing raw observations: {error}");
    }
    Ok(())
}

async fn checkpoint_exec_run_bundle(
    transport: &RuntimeTransport,
    thread_id: ThreadId,
    destination: std::path::PathBuf,
) -> Result<(), golutra_client::ClientError> {
    let receipt = RunBundleExporter::new(transport)
        .checkpoint(RunBundleExportRequest {
            destination: destination.clone(),
            selection: golutra_client::SessionWindowRequest {
                anchor_thread_id: thread_id,
                range: golutra_client::SessionRangeSpec {
                    direction: golutra_client::SessionRangeDirection::Single,
                    count: 1,
                },
            },
            terminal_outcome: RunBundleTerminalOutcome::InProgress {
                reason: "agent turn is still running; terminal outcome is pending".to_owned(),
            },
        })
        .await?;
    eprintln!(
        "golutra runtime checkpoint retained at {}; observations: {}; complete: {}",
        destination.display(),
        destination.join(&receipt.observations_path).display(),
        receipt.complete,
    );
    Ok(())
}

async fn read_exec_prompt(prompt: Option<String>) -> miette::Result<String> {
    let read_stdin = prompt.as_deref() == Some("-") || !std::io::stdin().is_terminal();
    if !read_stdin {
        return Ok(prompt.unwrap_or_default());
    }
    let mut stdin_prompt = String::new();
    tokio::io::stdin()
        .read_to_string(&mut stdin_prompt)
        .await
        .map_err(|error| miette::miette!("failed to read stdin: {error}"))?;
    if prompt.as_deref() == Some("-") {
        return Ok(stdin_prompt);
    }
    match prompt {
        Some(prompt) if !prompt.trim().is_empty() => Ok(format!("{prompt}\n\n{stdin_prompt}")),
        _ => Ok(stdin_prompt),
    }
}

fn report_exec_progress(event: &AgentStreamEvent) {
    match event {
        AgentStreamEvent::ItemStarted { item } => {
            eprintln!("• {}", item.title);
        }
        AgentStreamEvent::ItemUpdated { item } => {
            if let Some(content) = &item.content {
                eprintln!("  {}", content);
            }
        }
        AgentStreamEvent::RuntimeEvent { event } => {
            if let Some(summary) = event
                .payload
                .get("summary")
                .and_then(serde_json::Value::as_str)
            {
                eprintln!("  {summary}");
            }
        }
        AgentStreamEvent::TurnFailed { error, .. } => eprintln!("• failed: {error}"),
        AgentStreamEvent::TurnCompleted { .. }
        | AgentStreamEvent::ThreadStarted { .. }
        | AgentStreamEvent::TurnStarted { .. }
        | AgentStreamEvent::ItemCompleted { .. } => {}
    }
}

fn approval_id_from_exec_event(event: &AgentStreamEvent) -> Option<String> {
    let AgentStreamEvent::ItemStarted { item } = event else {
        return None;
    };
    (item.kind == golutra_protocol::AgentItemKind::Approval)
        .then(|| {
            item.data
                .pointer("/payload/approval_id")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
        .flatten()
}

async fn prompt_for_exec_approval(approval_id: &str) -> miette::Result<bool> {
    if !std::io::stdin().is_terminal() {
        eprintln!("approval {approval_id} denied because stdin is not interactive");
        return Ok(false);
    }
    let approval_id = approval_id.to_owned();
    tokio::task::spawn_blocking(move || {
        eprint!("Approval required ({approval_id}). Approve? [y/N] ");
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

async fn resolve_exec_approval(
    approval_id: &str,
    mode: ExecApprovalModeArg,
) -> miette::Result<bool> {
    match mode {
        ExecApprovalModeArg::Auto => {
            eprintln!("approval {approval_id} accepted by explicit exec auto mode");
            Ok(true)
        }
        ExecApprovalModeArg::Deny => {
            eprintln!("approval {approval_id} denied by exec policy");
            Ok(false)
        }
        ExecApprovalModeArg::Prompt => prompt_for_exec_approval(approval_id).await,
    }
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
    status.is_terminal()
}

fn evaluation_artifact_base_path(
    file: &std::path::Path,
    explicit_base: Option<&std::path::Path>,
) -> std::io::Result<std::path::PathBuf> {
    let base = explicit_base.unwrap_or_else(|| {
        file.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."))
    });
    let canonical = base.canonicalize()?;
    if !canonical.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "evaluation artifact base must be a directory",
        ));
    }
    Ok(canonical)
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
