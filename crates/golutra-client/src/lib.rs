use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use fs2::FileExt;
use golutra_config::{ProviderConfigPaths, load_provider_runtime_env_from_paths};
use golutra_context::ContextContributor;
use golutra_core::{
    Actor, ApprovalDecision, ApprovalId, ApprovalResolution, ArtifactId, ArtifactRecord,
    BusyPolicy, CommandId, EventId, MemoryId, PolicyBlockDisposition, PolicyDecision,
    PolicyEvaluation, PostTaskJob, PostTaskJobId, PostTaskJobKind, PostTaskJobStatus,
    ProviderAuthRequestId, RedactionStatus, RunProvenance, SessionId, TaskId,
    TaskReconciliationDecision, TaskReconciliationRecord, TaskRecoveryRecord, TaskStatus, ThreadId,
    TokenUsageRecord, TurnId, WorkspaceId,
};
#[cfg(test)]
use golutra_eval::EvaluationRunner;
use golutra_eval::{
    BenchmarkRun, CandidateStatus, EvaluationError, EvaluationStore, PromotionDecisionKind,
    ReviewMode, TaskEvaluationBundle, TaskEvaluationInput, TrajectoryFailureCluster,
    TrajectorySummary,
};
use golutra_evolution::{EvolutionError, EvolutionStore};
use golutra_llm::{ConfiguredProvider, ProviderError, ProviderRole, protocol_capabilities};
use golutra_mcp::McpToolBackend;
use golutra_memory::{MemoryError, MemoryFeedbackKind, MemoryScope, MemoryStore};
use golutra_plugin::PluginStore;
use golutra_policy::{WorkspacePolicy, approval_resource_matches};
use golutra_protocol::{
    ArtifactChunk, ArtifactReadRequest, CommandAck, EventFilter, EventPage, EventPageDirection,
    EventPageRequest, ExternalVerificationSpec, ProtocolVersionRange, RUNTIME_PROTOCOL_VERSION,
    RuntimeEvent, RuntimeEventSource, RuntimeEventType, RuntimeQuery, RuntimeQueryKind,
    SessionCommand, SessionCommandKind, StorageMaintenanceReport, StorageStats, TaskTracePage,
    TaskTraceRequest,
};
use golutra_runtime::{
    AgentExecutionControl, AgentExecutionHandle, AgentHarness, AgentLoopError, AgentLoopTraceEvent,
    AgentRun, AgentTaskRequest, BeforeSideEffectRecorder, PendingAgentTurn, RuntimeLaneError,
    RuntimeLaneManager, RuntimeObservation, RuntimeObservationSink, RuntimeVerificationService,
    WorkspaceCheckpointManager, agent_execution_channel, is_active_status,
};
use golutra_store::{CommandClaim, RuntimeRepositories, RuntimeStore, StoreError, ThreadRecord};
use golutra_tools::{
    FileBeforeImage, ProcessSupervisor, ToolRequest, ToolRuntime, discover_project_verifiers,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    sync::{Mutex, broadcast, mpsc, oneshot, watch},
    task::AbortHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const APP_SERVER_ATTACHMENT_HEADER: &str = "x-golutra-attachment";
pub const APP_SERVER_ACTOR_HEADER: &str = "x-golutra-actor-id";
pub const APP_SERVER_ATTACHMENT_ACTOR_PREFIX: &str = "app-attachment-";
pub const APP_SERVER_PROTOCOL_HEADER: &str = "x-golutra-protocol-version";
pub const APP_SERVER_TRANSPORT_TOKEN_ENV: &str = "GOLUTRA_TRANSPORT_TOKEN";
pub const RUNTIME_RELEASE_ID_ENV: &str = "GOLUTRA_RELEASE_ID";
const PROVISIONAL_COMMAND_ACK_REASON: &str = "command accepted for processing";
const EVENT_REPLAY_PAGE_SIZE: u32 = 256;
const MAX_EVENT_PAGE_SIZE: u32 = 512;
const CHECKPOINTS_TO_RETAIN_PER_WORKSPACE: usize = 20;
const MAX_HISTORY_SOURCE_EVENTS: u32 = 512;
const MAX_HTTP_JSON_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RUN_BUNDLE_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
const MAX_COMMAND_PAYLOAD_JSON_BYTES: usize = 256 * 1024;
const MAX_IDEMPOTENCY_KEY_CHARS: usize = 512;
const MAX_ACTOR_ID_CHARS: usize = 256;
const MAX_ROLLOUT_LINE_BYTES: usize = 20 * 1024 * 1024;
const ROLLOUT_FORMAT_VERSION: u32 = 1;
const POST_TASK_JOB_MAX_ATTEMPTS: u32 = 3;
const POST_TASK_JOB_LEASE_MINUTES: i64 = 5;
const POST_TASK_JOB_POLL_MILLIS: u64 = 250;
const POST_TASK_JOB_IDLE_POLL_MILLIS: u64 = 1_000;
const EXTERNAL_VERIFIERS_REQUIRE_OS_SANDBOX_KEY: &str = "_external_verifiers_require_os_sandbox";
const TASK_CONTROL_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Capabilities granted by the process that owns an embedded runtime.
///
/// A turn can request a capability, but it can never grant itself a capability
/// that the host did not enable. Keeping this separate from turn payloads also
/// makes daemon and remote transports explicit about their ownership boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeExecutionOptions {
    pub allow_network: bool,
}

impl RuntimeExecutionOptions {
    #[must_use]
    pub const fn isolated() -> Self {
        Self {
            allow_network: false,
        }
    }

    #[must_use]
    pub const fn with_network_access(allow_network: bool) -> Self {
        Self { allow_network }
    }
}

#[must_use]
pub fn app_server_attachment_actor_id(attachment_id: &str) -> String {
    format!("{APP_SERVER_ATTACHMENT_ACTOR_PREFIX}{attachment_id}")
}

pub(crate) fn runtime_identity() -> String {
    std::env::var(RUNTIME_RELEASE_ID_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 256
                && value.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
                })
        })
        .unwrap_or_else(|| format!("build:{}", env!("CARGO_PKG_VERSION")))
}

mod agent;
mod agent_projection;
mod application;
mod causal_recorder;
mod change_tracker;
mod command;
mod context;
mod debug_export;
mod delegation;
mod diagnosis;
mod event_codec;
mod evolution;
mod execution;
mod execution_trace;
mod external_evaluation;
mod governance;
mod governance_commands;
mod legacy_task;
mod observation;
mod paths;
mod post_task;
mod provenance;
mod provider_runtime;
mod query;
mod recovery;
mod regression;
mod replay;
mod rollout;
mod run_bundle;
mod session;
mod task_governance;
mod trace;
mod trace_integrity;
mod transport;

pub use agent::{AgentClient, AgentThread, TurnHandle};
pub use agent_projection::AgentEventProjector;
pub use application::{
    GovernedRuntime, RuntimeApplication, RuntimeCommandService, RuntimeGovernanceService,
    RuntimeQueryService, RuntimeSessionService, TaskTraceService,
};
pub(crate) use context::{
    compact_event_summary, compact_history_text, compact_history_with_summary,
    completion_criteria_from_payload, context_compaction_from_event, conversation_history_line,
    effective_model_history_events, environment_context_prompt, load_project_instructions,
    memory_context, model_prompt_from_payload, preview_from_payload, prompt_from_payload,
    system_prompt, task_contract_from_payload, title_from_payload,
};
pub use debug_export::{
    DebugExportCoordinator, DebugExportManifest, DebugExportReceipt, DebugExportRequest,
    ExportedArtifactManifest, ExportedArtifactState, ExportedSessionManifest, ExportedTaskManifest,
    parse_session_range,
};
pub(crate) use event_codec::redact_provider_json;
pub(crate) use event_codec::{
    agent_event, agent_event_for_turn, candidate_id_from_payload, context_compaction_artifact,
    context_replay_request_artifact, context_request_artifact, event_matches_filter, host_event,
    parent_thread_id_from_payload, provider_raw_artifact, provider_response_replay_artifact,
    recovered_pending_turn_from_event, task_status_from_loop_action, thread_id_from_payload,
    thread_title_for_prompt, trace_event_payload, with_command_payload,
};
pub use event_codec::{event_sequence_no, projection_status};
pub(crate) use execution_trace::CanonicalFactRecorder;
pub use golutra_protocol::{
    SessionCursor, SessionPage, SessionPageRequest, SessionRangeDirection, SessionRangeSpec,
    SessionSummary, SessionWindow, SessionWindowRequest,
};
pub(crate) use legacy_task::LegacyTaskAdapter;
#[cfg(test)]
pub(crate) use legacy_task::LegacyWriteFileArgs;
pub use observation::{
    ConversationEntry, ConversationRole, ObservedSession, ObservedTask,
    RuntimeObservationCollector, RuntimeObservationSnapshot,
};
pub use paths::{AppServerPaths, RuntimePaths};
pub(crate) use paths::{ensure_private_dir, set_owner_only_file, workspace_hash};
pub use post_task::PostTaskCoordinator;
#[cfg(test)]
pub(crate) use provider_runtime::configured_provider_plan;
pub(crate) use provider_runtime::{
    MockProviderPlan, isolated_mock_provider_plan, mock_provider_plan, pin_provider_turn_settings,
};
pub use rollout::{RolloutEnvelope, RolloutExport, ThreadRebindResult, redact_runtime_value};
pub(crate) use rollout::{
    append_rollout_line, normalize_rebind_source, rebuild_rollout_file, rollout_line,
    rollout_path_for_workspace,
};
#[cfg(test)]
pub(crate) use rollout::{redact_rollout_value, rollout_lock_path};
pub use run_bundle::{
    DebugExportManifestReceipt, DebugExportOutcome, ObservationBundleManifest,
    ObservationSessionManifest, ObservationTaskManifest, RawStateManifest, RunBundleExportRequest,
    RunBundleExporter, RunBundleFile, RunBundleManifest, RunBundlePath, RunBundleReceipt,
    RunBundleTerminalOutcome,
};
pub use trace::merge_task_trace_page;
#[cfg(unix)]
pub use transport::UnixIpcTransport;
pub(crate) use transport::run_blocking;
pub use transport::{
    AppServerInfo, EmbeddedTransport, HttpSseTransport, RuntimeAttachment, RuntimeClient,
    RuntimeEventStream, RuntimeHostInfo, RuntimeTransport, TaskTraceClient,
};
#[cfg(test)]
pub(crate) use transport::{
    ParsedSseEvent, parse_sse_frame, sse_frame_complete, validate_local_app_server_base_url,
    validate_remote_app_server_base_url,
};

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("runtime store failed: {0}")]
    Store(#[from] StoreError),
    #[error("runtime lane failed")]
    RuntimeLane(#[from] RuntimeLaneError),
    #[error("query result serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("runtime workspace io failed: {0}")]
    Io(String),
    #[error("runtime session id is invalid: {0}")]
    InvalidSession(String),
    #[error("runtime task execution failed: {0}")]
    TaskExecution(String),
    #[error("runtime task was cancelled")]
    TaskCancelled,
    #[error("runtime HTTP transport failed: {0}")]
    Http(String),
    #[error("runtime daemon failed: {0}")]
    Daemon(String),
    #[error("runtime memory failed")]
    Memory(#[from] MemoryError),
    #[error("runtime evaluation failed")]
    Evaluation(#[from] EvaluationError),
    #[error("runtime evolution failed")]
    Evolution(#[from] EvolutionError),
}

async fn wait_for_task_control_cleanup(
    completion: &mut watch::Receiver<bool>,
    timeout: Duration,
) -> Result<(), ClientError> {
    match tokio::time::timeout(timeout, completion.changed()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(ClientError::TaskExecution(
            "previous task supervisor stopped before releasing the session".to_owned(),
        )),
        Err(_) => Err(ClientError::TaskExecution(format!(
            "previous task supervisor did not release the session within {} milliseconds",
            timeout.as_millis()
        ))),
    }
}

#[derive(Debug)]
struct RuntimeHostStorage {
    runtime_paths: Option<RuntimePaths>,
    provider_config_paths: Option<ProviderConfigPaths>,
    checkpoint_evaluation_tasks: HashSet<TaskId>,
    temporary_root: Option<Arc<tempfile::TempDir>>,
}

impl RuntimeHostStorage {
    fn in_memory() -> Result<Self, ClientError> {
        let temporary_root = tempfile::tempdir()
            .map(Arc::new)
            .map_err(|error| ClientError::Io(error.to_string()))?;
        Ok(Self {
            runtime_paths: None,
            provider_config_paths: None,
            checkpoint_evaluation_tasks: HashSet::new(),
            temporary_root: Some(temporary_root),
        })
    }

    fn durable(runtime_paths: RuntimePaths) -> Result<Self, ClientError> {
        let provider_config_paths = ProviderConfigPaths::from_home(&runtime_paths.home)
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        Ok(Self {
            runtime_paths: Some(runtime_paths),
            provider_config_paths: Some(provider_config_paths),
            checkpoint_evaluation_tasks: HashSet::new(),
            temporary_root: None,
        })
    }

    fn ephemeral(cwd: impl AsRef<Path>) -> Result<Self, ClientError> {
        let provider_config_paths = ProviderConfigPaths::global()
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        let temporary_root = tempfile::tempdir()
            .map(Arc::new)
            .map_err(|error| ClientError::Io(error.to_string()))?;
        let runtime_paths = RuntimePaths::from_home_and_cwd(temporary_root.path(), cwd)?;
        Ok(Self {
            runtime_paths: Some(runtime_paths),
            provider_config_paths: Some(provider_config_paths),
            checkpoint_evaluation_tasks: HashSet::new(),
            temporary_root: Some(temporary_root),
        })
    }

    fn ephemeral_persistent(runtime_paths: RuntimePaths) -> Result<Self, ClientError> {
        let provider_config_paths = ProviderConfigPaths::global()
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        Ok(Self {
            runtime_paths: Some(runtime_paths),
            provider_config_paths: Some(provider_config_paths),
            checkpoint_evaluation_tasks: HashSet::new(),
            temporary_root: None,
        })
    }

    fn opened_persisted_run(
        runtime_paths: RuntimePaths,
        checkpoint_evaluation_tasks: HashSet<TaskId>,
    ) -> Result<Self, ClientError> {
        let mut storage = Self::ephemeral_persistent(runtime_paths)?;
        storage.checkpoint_evaluation_tasks = checkpoint_evaluation_tasks;
        Ok(storage)
    }
}

#[derive(Debug)]
struct RuntimeHostBootstrap {
    store: RuntimeStore,
    workspace_root: Option<PathBuf>,
    storage: RuntimeHostStorage,
    workspace_id: WorkspaceId,
    default_session_id: SessionId,
    default_thread_id: ThreadId,
    force_mock_provider: bool,
    execution_options: RuntimeExecutionOptions,
}

#[derive(Debug)]
pub struct RuntimeHost {
    store: RuntimeStore,
    repositories: RuntimeRepositories,
    memory_store: MemoryStore,
    evaluation_store: EvaluationStore,
    evolution_store: EvolutionStore,
    governance: governance::GovernanceService,
    lane_manager: Mutex<RuntimeLaneManager>,
    event_bus: broadcast::Sender<RuntimeEvent>,
    next_sequence_no: AtomicU64,
    event_writer: Mutex<()>,
    causal_ledger: Mutex<causal_recorder::CausalLedger>,
    workspace_id: WorkspaceId,
    workspace_root: Option<PathBuf>,
    runtime_paths: Option<RuntimePaths>,
    provider_config_paths: Option<ProviderConfigPaths>,
    default_session_id: SessionId,
    default_thread_id: ThreadId,
    instance_id: String,
    started_at: chrono::DateTime<chrono::Utc>,
    command_mutex: Mutex<()>,
    task_controls: Mutex<HashMap<SessionId, HostedTaskControl>>,
    provider_auth_waiters: Mutex<HashMap<SessionId, PendingProviderAuth>>,
    process_supervisor: ProcessSupervisor,
    workspace_change_tracker: Mutex<change_tracker::WorkspaceChangeTracker>,
    deep_evaluation_inputs: Mutex<HashMap<PostTaskJobId, TaskEvaluationInput>>,
    force_mock_provider: bool,
    execution_options: RuntimeExecutionOptions,
    checkpoint_evaluation_tasks: HashSet<TaskId>,
    _temporary_root: Option<Arc<tempfile::TempDir>>,
}

impl Drop for RuntimeHost {
    fn drop(&mut self) {
        self.process_supervisor.shutdown();
    }
}

#[derive(Debug, Clone)]
pub(crate) struct HostedAgentTask {
    pub(crate) session_id: SessionId,
    pub(crate) task_id: TaskId,
    pub(crate) turn_id: TurnId,
    pub(crate) payload: Value,
}

#[derive(Debug, Clone)]
pub(crate) struct RecoveredPendingTurn {
    pub(crate) sequence_no: u64,
    pub(crate) actor: Actor,
    pub(crate) payload: Value,
    pub(crate) pending: PendingAgentTurn,
}

struct HostedTaskEvaluation<'a> {
    objective: &'a str,
    task_status: TaskStatus,
    verification: Option<golutra_core::VerificationRecord>,
    tool_reports: &'a [golutra_tools::ToolExecutionReport],
    failure_summary: Option<String>,
    latency: Duration,
}

#[derive(Debug, Clone)]
struct HostedTaskControl {
    task_id: TaskId,
    allow_network: bool,
    yolo: bool,
    provider_settings: ProviderTurnSettings,
    execution: AgentExecutionHandle,
    abort_handle: AbortHandle,
    completion: watch::Receiver<bool>,
    _session_lease: Option<Arc<File>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct ProviderTurnSettings {
    profile: Option<Value>,
    model: Option<Value>,
    generation_config: Option<Value>,
}

impl ProviderTurnSettings {
    fn from_payload(payload: &Value) -> Self {
        Self {
            profile: payload.get("provider_profile").cloned(),
            model: payload.get("provider_model").cloned(),
            generation_config: payload.get("provider_generation_config").cloned(),
        }
    }

    fn normalize_queued_payload(&self, payload: &mut Value) -> Result<(), &'static str> {
        let requested = Self::from_payload(payload);
        if (requested.profile.is_some() && requested.profile != self.profile)
            || (requested.model.is_some() && requested.model != self.model)
            || (requested.generation_config.is_some()
                && requested.generation_config != self.generation_config)
        {
            return Err("queued prompt cannot change provider settings while a task is active");
        }
        for (key, value) in [
            ("provider_profile", &self.profile),
            ("provider_model", &self.model),
            ("provider_generation_config", &self.generation_config),
        ] {
            if let Some(value) = value {
                payload[key] = value.clone();
            }
        }
        Ok(())
    }
}

enum SessionLeaseAttempt {
    Acquired(Option<Arc<File>>),
    Busy,
}

enum HostedTraceCommand {
    Event(Box<RuntimeObservation>),
    Flush(oneshot::Sender<Result<(), ClientError>>),
}

#[derive(Debug)]
struct PendingProviderAuth {
    request_id: ProviderAuthRequestId,
    resolution: oneshot::Sender<ProviderAuthResolution>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderAuthResolution {
    Submitted,
    Cancelled,
}

#[derive(Debug, Clone)]
struct HostedCheckpointRecorder {
    host: Arc<RuntimeHost>,
    task: HostedAgentTask,
    trace_sender: mpsc::UnboundedSender<HostedTraceCommand>,
}

#[async_trait]
impl BeforeSideEffectRecorder for HostedCheckpointRecorder {
    async fn persist_before_side_effect(
        &self,
        request: &ToolRequest,
        before_images: &[FileBeforeImage],
        complete: bool,
    ) -> Result<(), AgentLoopError> {
        let (flush_sender, flush_receiver) = oneshot::channel();
        self.trace_sender
            .send(HostedTraceCommand::Flush(flush_sender))
            .map_err(|_| AgentLoopError::Checkpoint("trace recorder is unavailable".to_owned()))?;
        flush_receiver
            .await
            .map_err(|_| AgentLoopError::Checkpoint("trace recorder stopped".to_owned()))?
            .map_err(|error| AgentLoopError::Checkpoint(error.to_string()))?;
        self.host
            .persist_checkpoint_before_side_effect(&self.task, request, before_images, complete)
            .await
            .map_err(|error| AgentLoopError::Checkpoint(error.to_string()))
    }
}

impl RuntimeHost {
    pub async fn in_memory() -> Result<Arc<Self>, ClientError> {
        Self::in_memory_with_options(RuntimeExecutionOptions::isolated()).await
    }

    pub async fn in_memory_with_options(
        execution_options: RuntimeExecutionOptions,
    ) -> Result<Arc<Self>, ClientError> {
        let store = RuntimeStore::in_memory().await?;
        let default_session_id = SessionId::new();
        let default_thread_id = ThreadId::new();
        Self::from_store(RuntimeHostBootstrap {
            store,
            workspace_root: None,
            storage: RuntimeHostStorage::in_memory()?,
            workspace_id: WorkspaceId::new(),
            default_session_id,
            default_thread_id,
            force_mock_provider: false,
            execution_options,
        })
        .await
    }

    /// Build an in-memory runtime with a real workspace boundary. Runtime
    /// state is kept below a process-owned temporary root, while provider
    /// credentials still come from the user's global configuration.
    pub async fn ephemeral_for_cwd(cwd: impl AsRef<Path>) -> Result<Arc<Self>, ClientError> {
        Self::ephemeral_for_cwd_with_options(cwd, RuntimeExecutionOptions::isolated()).await
    }

    pub async fn ephemeral_for_cwd_with_options(
        cwd: impl AsRef<Path>,
        execution_options: RuntimeExecutionOptions,
    ) -> Result<Arc<Self>, ClientError> {
        let storage = RuntimeHostStorage::ephemeral(cwd)?;
        let cwd = storage
            .runtime_paths
            .as_ref()
            .expect("ephemeral runtime paths are initialized")
            .cwd
            .clone();
        let store = RuntimeStore::in_memory().await?;
        Self::from_store(RuntimeHostBootstrap {
            store,
            workspace_root: Some(cwd),
            storage,
            workspace_id: WorkspaceId::new(),
            default_session_id: SessionId::new(),
            default_thread_id: ThreadId::new(),
            force_mock_provider: false,
            execution_options,
        })
        .await
    }

    /// Build an isolated runtime whose state remains below `state_home` after
    /// this process exits. Provider configuration continues to come from the
    /// global Golutra home, so the persisted run directory contains no
    /// provider configuration or credentials.
    pub async fn ephemeral_persistent_for_cwd(
        cwd: impl AsRef<Path>,
        state_home: impl AsRef<Path>,
    ) -> Result<Arc<Self>, ClientError> {
        Self::ephemeral_persistent_for_cwd_with_options(
            cwd,
            state_home,
            RuntimeExecutionOptions::isolated(),
        )
        .await
    }

    pub async fn ephemeral_persistent_for_cwd_with_options(
        cwd: impl AsRef<Path>,
        state_home: impl AsRef<Path>,
        execution_options: RuntimeExecutionOptions,
    ) -> Result<Arc<Self>, ClientError> {
        let paths = RuntimePaths::for_ephemeral_state_dir(state_home, cwd)?;
        let store = RuntimeStore::connect_single_writer_with_artifact_root(
            &paths.sqlite_url(),
            paths.artifacts_dir.clone(),
        )
        .await?;
        set_owner_only_file(&paths.runtime_db)?;
        let storage = RuntimeHostStorage::ephemeral_persistent(paths.clone())?;
        let host = Self::from_store(RuntimeHostBootstrap {
            store,
            workspace_root: Some(paths.cwd.clone()),
            storage,
            workspace_id: WorkspaceId::new(),
            default_session_id: SessionId::new(),
            default_thread_id: ThreadId::new(),
            force_mock_provider: false,
            execution_options,
        })
        .await?;
        host.recover_unscheduled_post_task_jobs().await?;
        Ok(host)
    }

    /// Reopen a completed or checkpointed owner-only run bundle for appending
    /// evaluator overlays. The recorded cwd may belong to a deleted container
    /// and is therefore used as an identity boundary without filesystem access.
    pub async fn open_persisted_run(run_root: impl AsRef<Path>) -> Result<Arc<Self>, ClientError> {
        let run_root = run_root.as_ref();
        let manifest_path = run_root.join("manifest.json");
        let metadata = fs::symlink_metadata(&manifest_path)
            .map_err(|error| ClientError::Io(format!("{}: {error}", manifest_path.display())))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_RUN_BUNDLE_MANIFEST_BYTES
        {
            return Err(ClientError::Io(
                "persisted run manifest must be a bounded regular file".to_owned(),
            ));
        }
        let manifest_bytes = fs::read(&manifest_path)
            .map_err(|error| ClientError::Io(format!("{}: {error}", manifest_path.display())))?;
        let manifest: RunBundleManifest = serde_json::from_slice(&manifest_bytes)?;
        if manifest.format != "golutra-run-bundle" || !matches!(manifest.format_version, 1 | 2) {
            return Err(ClientError::TaskExecution(
                "persisted run manifest format is unsupported".to_owned(),
            ));
        }
        let workspace_id = manifest
            .workspace_id
            .parse::<WorkspaceId>()
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        let paths = RuntimePaths::open_ephemeral_state_dir(run_root, &manifest.workspace_root)?;
        let store = RuntimeStore::connect_single_writer_with_artifact_root(
            &paths.sqlite_url(),
            paths.artifacts_dir.clone(),
        )
        .await?;
        set_owner_only_file(&paths.runtime_db)?;
        run_bundle::validate_persisted_run_store(run_root, &manifest, &store).await?;
        let permits_prefix_evaluation = matches!(
            &manifest.terminal_outcome,
            RunBundleTerminalOutcome::InProgress { .. }
                | RunBundleTerminalOutcome::Aborted { .. }
                | RunBundleTerminalOutcome::Result {
                    result: golutra_protocol::AgentTurnResult {
                        status: TaskStatus::Cancelled
                            | TaskStatus::Interrupted
                            | TaskStatus::Uncertain,
                        ..
                    }
                }
        );
        let checkpoint_evaluation_tasks = if permits_prefix_evaluation {
            manifest
                .observations
                .sessions
                .iter()
                .flat_map(|session| &session.tasks)
                .filter(|task| !task.complete)
                .map(|task| {
                    task.task_id.parse::<TaskId>().map_err(|error| {
                        ClientError::TaskExecution(format!(
                            "persisted run checkpoint task id is invalid: {error}"
                        ))
                    })
                })
                .collect::<Result<HashSet<_>, _>>()?
        } else {
            HashSet::new()
        };
        let latest_thread = store
            .list_threads(Some(&manifest.workspace_root), 1)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                ClientError::TaskExecution(
                    "persisted run has no thread for its recorded workspace".to_owned(),
                )
            })?;
        let storage =
            RuntimeHostStorage::opened_persisted_run(paths.clone(), checkpoint_evaluation_tasks)?;
        let host = Self::from_store(RuntimeHostBootstrap {
            store,
            workspace_root: Some(paths.cwd.clone()),
            storage,
            workspace_id,
            default_session_id: latest_thread.session_id,
            default_thread_id: latest_thread.thread_id,
            force_mock_provider: false,
            execution_options: RuntimeExecutionOptions::isolated(),
        })
        .await?;
        host.recover_unscheduled_post_task_jobs().await?;
        Ok(host)
    }

    pub async fn for_cwd(cwd: impl AsRef<Path>) -> Result<Arc<Self>, ClientError> {
        Self::for_cwd_with_options(cwd, RuntimeExecutionOptions::isolated()).await
    }

    pub async fn for_cwd_with_options(
        cwd: impl AsRef<Path>,
        execution_options: RuntimeExecutionOptions,
    ) -> Result<Arc<Self>, ClientError> {
        let paths = RuntimePaths::for_cwd(cwd)?;
        Self::from_runtime_paths(paths, execution_options).await
    }

    pub async fn from_home_and_cwd(
        home: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
    ) -> Result<Arc<Self>, ClientError> {
        Self::from_home_and_cwd_with_options(home, cwd, RuntimeExecutionOptions::isolated()).await
    }

    pub async fn from_home_and_cwd_with_options(
        home: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
        execution_options: RuntimeExecutionOptions,
    ) -> Result<Arc<Self>, ClientError> {
        let paths = RuntimePaths::from_home_and_cwd(home, cwd)?;
        Self::from_runtime_paths(paths, execution_options).await
    }

    async fn from_runtime_paths(
        paths: RuntimePaths,
        execution_options: RuntimeExecutionOptions,
    ) -> Result<Arc<Self>, ClientError> {
        let store = RuntimeStore::connect_with_artifact_root(
            &paths.sqlite_url(),
            paths.artifacts_dir.clone(),
        )
        .await?;
        set_owner_only_file(&paths.runtime_db)?;
        let cwd = paths.cwd.to_string_lossy().to_string();
        let latest_thread = store.list_threads(Some(&cwd), 1).await?.into_iter().next();
        let (default_session_id, default_thread_id) = latest_thread.map_or_else(
            || (SessionId::new(), ThreadId::new()),
            |thread| (thread.session_id, thread.thread_id),
        );
        let storage = RuntimeHostStorage::durable(paths.clone())?;
        let host = Self::from_store(RuntimeHostBootstrap {
            store,
            workspace_root: Some(paths.cwd.clone()),
            storage,
            workspace_id: paths.workspace_id(),
            default_session_id,
            default_thread_id,
            force_mock_provider: false,
            execution_options,
        })
        .await?;
        host.synchronize_workspace_rollouts().await?;
        host.recover_orphaned_tasks().await?;
        host.run_storage_maintenance().await?;
        Ok(host)
    }

    async fn from_store(bootstrap: RuntimeHostBootstrap) -> Result<Arc<Self>, ClientError> {
        let RuntimeHostBootstrap {
            store,
            workspace_root,
            storage,
            workspace_id,
            default_session_id,
            default_thread_id,
            force_mock_provider,
            execution_options,
        } = bootstrap;
        let RuntimeHostStorage {
            runtime_paths,
            provider_config_paths,
            checkpoint_evaluation_tasks,
            temporary_root,
        } = storage;
        let (event_bus, _) = broadcast::channel(512);
        let max_sequence_no = store.max_sequence_no().await?;
        let next_sequence_no = max_sequence_no.saturating_add(1);
        let memory_store = runtime_paths
            .as_ref()
            .map_or_else(MemoryStore::in_memory, |paths| {
                MemoryStore::new(paths.memory_file.clone())
            });
        let evaluation_store = runtime_paths
            .as_ref()
            .map_or_else(EvaluationStore::in_memory, |paths| {
                EvaluationStore::new(paths.evaluation_file.clone())
            });
        let evolution_store = runtime_paths.as_ref().map_or_else(
            || {
                let root = temporary_root
                    .as_ref()
                    .expect("in-memory runtime temporary root is initialized")
                    .path();
                EvolutionStore::new(root.join("evolution.json"), root.join("skills"))
            },
            |paths| {
                EvolutionStore::new(
                    paths.evolution_file.clone(),
                    paths.evolution_skills_dir.clone(),
                )
            },
        );
        let repositories = store.repositories();
        let governance = governance::GovernanceService::new(
            repositories.clone(),
            evaluation_store.clone(),
            memory_store.clone(),
        );
        let host = Arc::new(Self {
            store,
            repositories,
            memory_store,
            evaluation_store,
            evolution_store,
            governance,
            lane_manager: Mutex::new(RuntimeLaneManager::new()),
            event_bus,
            next_sequence_no: AtomicU64::new(next_sequence_no),
            event_writer: Mutex::new(()),
            causal_ledger: Mutex::new(causal_recorder::CausalLedger::default()),
            workspace_id,
            workspace_root,
            runtime_paths,
            provider_config_paths,
            default_session_id,
            default_thread_id,
            instance_id: Uuid::now_v7().to_string(),
            started_at: chrono::Utc::now(),
            command_mutex: Mutex::new(()),
            task_controls: Mutex::new(HashMap::new()),
            provider_auth_waiters: Mutex::new(HashMap::new()),
            process_supervisor: ProcessSupervisor::new(),
            workspace_change_tracker: Mutex::new(change_tracker::WorkspaceChangeTracker::default()),
            deep_evaluation_inputs: Mutex::new(HashMap::new()),
            force_mock_provider,
            execution_options,
            checkpoint_evaluation_tasks,
            _temporary_root: temporary_root,
        });
        host.repositories
            .jobs
            .recover_expired(&host.workspace_id.to_string(), chrono::Utc::now())
            .await?;
        post_task::PostTaskCoordinator::start(&host);
        Ok(host)
    }

    #[must_use]
    pub fn default_session_id(&self) -> SessionId {
        self.default_session_id
    }

    #[must_use]
    pub fn default_thread_id(&self) -> ThreadId {
        self.default_thread_id
    }

    #[must_use]
    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    #[must_use]
    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    #[must_use]
    pub fn execution_options(&self) -> RuntimeExecutionOptions {
        self.execution_options
    }

    pub(crate) fn network_access_enabled(&self, requested: bool) -> bool {
        requested && self.execution_options.allow_network
    }

    pub(crate) fn execution_capabilities(&self, requested_network: bool, yolo: bool) -> Value {
        let enabled = self.network_access_enabled(requested_network);
        json!({
            "network": {
                "requested": requested_network,
                "enabled": enabled,
                "reason": if enabled {
                    "enabled"
                } else if requested_network {
                    "runtime capability not enabled"
                } else {
                    "not requested"
                }
            },
            "policy": {
                "mode": if yolo { "unrestricted" } else { "guarded" },
                "tool_sandbox_mode": if yolo { "process_only" } else { "detected" },
                "permission_profile": if yolo { "full_access" } else { "workspace" },
                "approval_mode": if yolo { "never" } else { "on_request" },
            }
        })
    }

    fn capture_run_provenance(&self, task_id: TaskId) -> RunProvenance {
        provenance::run_provenance(
            task_id,
            self.workspace_id,
            self.workspace_root.as_deref(),
            self.provider_config_paths
                .as_ref()
                .map(|paths| paths.user_config.as_path()),
        )
    }

    pub async fn runtime_info(
        &self,
        base_url: impl Into<String>,
    ) -> Result<RuntimeHostInfo, ClientError> {
        let workspace_root = self.workspace_root_string();
        let latest_thread = self
            .repositories
            .threads
            .list(workspace_root.as_deref(), 1)
            .await?
            .into_iter()
            .next();
        let (default_session_id, default_thread_id) = latest_thread.map_or_else(
            || (self.default_session_id, self.default_thread_id),
            |thread| (thread.session_id, thread.thread_id),
        );
        Ok(RuntimeHostInfo {
            instance_id: self.instance_id.clone(),
            pid: std::process::id(),
            base_url: base_url.into(),
            cwd: self
                .workspace_root
                .as_ref()
                .map_or_else(String::new, |path| path.display().to_string()),
            workspace_id: self.workspace_id,
            default_session_id,
            default_thread_id,
            started_at: self.started_at,
        })
    }

    #[must_use]
    pub fn subscribe_live(&self, _filter: EventFilter) -> broadcast::Receiver<RuntimeEvent> {
        self.event_bus.subscribe()
    }

    async fn event_stream(
        self: Arc<Self>,
        filter: EventFilter,
    ) -> Result<RuntimeEventStream, ClientError> {
        self.ensure_session_in_workspace(filter.session_id).await?;
        let mut live = self.event_bus.subscribe();
        let (sender, receiver) = mpsc::channel(256);
        tokio::spawn(async move {
            let mut cursor = filter.after_sequence_no;
            match self.send_replay_pages(&filter, &mut cursor, &sender).await {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                    return;
                }
            }
            loop {
                match live.recv().await {
                    Ok(event) if event_matches_filter(&event, &filter, cursor) => {
                        cursor = Some(event.sequence_no);
                        if sender.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        match self.send_replay_pages(&filter, &mut cursor, &sender).await {
                            Ok(true) => {}
                            Ok(false) => return,
                            Err(error) => {
                                let _ = sender.send(Err(error)).await;
                                return;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Ok(RuntimeEventStream::new(receiver))
    }

    async fn send_replay_pages(
        &self,
        filter: &EventFilter,
        cursor: &mut Option<u64>,
        sender: &mpsc::Sender<Result<RuntimeEvent, ClientError>>,
    ) -> Result<bool, ClientError> {
        loop {
            let events = self
                .repositories
                .events
                .load_page(
                    filter.session_id,
                    filter.task_id,
                    *cursor,
                    EVENT_REPLAY_PAGE_SIZE,
                )
                .await?;
            let page_is_full = events.len() == EVENT_REPLAY_PAGE_SIZE as usize;
            for event in events {
                *cursor = Some(event.sequence_no);
                if sender.send(Ok(event)).await.is_err() {
                    return Ok(false);
                }
            }
            if !page_is_full {
                return Ok(true);
            }
        }
    }

    pub async fn recover_orphaned_tasks(self: &Arc<Self>) -> Result<usize, ClientError> {
        let Some(workspace_root) = self.workspace_root_string() else {
            return Ok(0);
        };
        let threads = self
            .repositories
            .threads
            .list(Some(&workspace_root), u32::MAX)
            .await?;
        let mut recovered = 0;
        for thread in threads {
            let state = self
                .repositories
                .projections
                .state(thread.session_id, None)
                .await?;
            let orphan_is_active = state.task_status.is_active();
            let may_have_pending_turns = state
                .runtime_lane
                .as_ref()
                .is_some_and(|lane| !lane.pending_turns.is_empty());
            if state.task_status.requires_reconciliation() {
                continue;
            }
            if !orphan_is_active && !may_have_pending_turns {
                continue;
            }
            let SessionLeaseAttempt::Acquired(lease) =
                self.try_acquire_session_lease(state.session_id)?
            else {
                continue;
            };
            let pending_turns = self
                .recoverable_pending_turns(state.session_id, state.active_task_id)
                .await?;
            let reconciliation_required =
                if orphan_is_active && let Some(task_id) = state.active_task_id {
                    self.record_orphaned_task_recovery(
                        state.session_id,
                        task_id,
                        "runtime_process_restart",
                    )
                    .await?
                    .reconciliation_required
                } else {
                    false
                };
            if !reconciliation_required && !pending_turns.is_empty() {
                self.clone()
                    .restart_pending_turns(state.session_id, pending_turns, lease)
                    .await?;
            }
            recovered += 1;
        }
        self.recover_unscheduled_post_task_jobs().await?;
        Ok(recovered)
    }

    async fn recoverable_pending_turns(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
    ) -> Result<Vec<RecoveredPendingTurn>, ClientError> {
        let Some(task_id) = task_id else {
            return Ok(Vec::new());
        };
        let events = self
            .repositories
            .events
            .load(session_id, Some(task_id), None)
            .await?;
        let mut pending = HashMap::<TurnId, RecoveredPendingTurn>::new();
        for event in events {
            match event.event_type {
                RuntimeEventType::TurnQueued => {
                    let referenced_sequences = event
                        .payload
                        .get("recovered_pending_sequence_nos")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_u64)
                        .collect::<Vec<_>>();
                    let is_pending_transfer_event = !referenced_sequences.is_empty()
                        && event.turn_id.is_none()
                        && event.payload.get("payload").is_none();
                    for sequence_no in referenced_sequences {
                        if let Some(referenced_event) = self
                            .repositories
                            .events
                            .load_by_sequence(session_id, sequence_no)
                            .await?
                            && let Some(referenced) =
                                recovered_pending_turn_from_event(&referenced_event)?
                        {
                            pending.insert(referenced.pending.turn_id, referenced);
                        }
                    }
                    if !is_pending_transfer_event
                        && let Some(recovered) = recovered_pending_turn_from_event(&event)?
                    {
                        pending.insert(recovered.pending.turn_id, recovered);
                    }
                }
                RuntimeEventType::TurnUpdated => {
                    if let Some(mut updated) = recovered_pending_turn_from_event(&event)?
                        && let Some(original) = pending.get(&updated.pending.turn_id)
                    {
                        updated.sequence_no = original.sequence_no;
                        updated.actor = original.actor.clone();
                        pending.insert(updated.pending.turn_id, updated);
                    }
                }
                RuntimeEventType::TurnStarted | RuntimeEventType::TurnCancelled => {
                    if let Some(turn_id) = event.turn_id {
                        pending.remove(&turn_id);
                    }
                }
                _ => {}
            }
        }
        let mut pending = pending.into_values().collect::<Vec<_>>();
        pending.sort_by_key(|turn| turn.sequence_no);
        Ok(pending)
    }

    async fn restart_pending_turns(
        self: Arc<Self>,
        session_id: SessionId,
        mut pending_turns: Vec<RecoveredPendingTurn>,
        session_lease: Option<Arc<File>>,
    ) -> Result<(), ClientError> {
        if pending_turns.is_empty() {
            return Ok(());
        }
        let recovered_network_capability = pending_turns[0].pending.allow_network;
        if pending_turns
            .iter()
            .any(|turn| turn.pending.allow_network != recovered_network_capability)
        {
            return Err(ClientError::TaskExecution(
                "durable pending turn batch changes network capability".to_owned(),
            ));
        }
        let recovered_yolo_capability = pending_turns[0].pending.yolo;
        if pending_turns
            .iter()
            .any(|turn| turn.pending.yolo != recovered_yolo_capability)
        {
            return Err(ClientError::TaskExecution(
                "durable pending turn batch changes yolo capability".to_owned(),
            ));
        }
        let first = pending_turns.remove(0);
        let task_id = TaskId::new();
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            Some(task_id),
            RuntimeEventType::TurnQueued,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "durable pending turns transferred to a recovery task",
                "recovery": "durable_pending_turn_batch",
                "recovered_pending_sequence_nos": std::iter::once(&first)
                    .chain(pending_turns.iter())
                    .map(|turn| turn.sequence_no)
                    .collect::<Vec<_>>(),
            }),
        ))
        .await?;
        let mut lane_manager = self.lane_manager.lock().await;
        let mut transition = lane_manager.start_task(
            self.workspace_id,
            session_id,
            task_id,
            first.pending.turn_id,
            first.actor,
            self.next_sequence_no(),
        )?;
        for pending in &pending_turns {
            lane_manager.queue_turn(session_id, pending.pending.turn_id, 0)?;
        }
        if let Some(lane) = lane_manager.lane(session_id) {
            transition.event.payload["runtime_lane"] = json!(lane);
        }
        drop(lane_manager);
        transition.event.event_type = RuntimeEventType::TurnStarted;
        transition.event.payload["summary"] =
            json!("durable pending turn restarted after runtime recovery");
        transition.event.payload["recovery"] = json!("durable_pending_turn");
        transition.event.payload["command_id"] = json!(first.pending.command_id);
        transition.event.payload["runtime_identity"] = json!(runtime_identity());
        self.record_event(transition.event).await?;
        self.spawn_agent_task(
            HostedAgentTask {
                session_id,
                task_id,
                turn_id: first.pending.turn_id,
                payload: first.payload,
            },
            session_lease,
            pending_turns.into_iter().map(|turn| turn.pending).collect(),
        )
        .await
    }

    async fn record_orphaned_task_cancelled(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
        recovery: &str,
        summary: &str,
    ) -> Result<(), ClientError> {
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            task_id,
            RuntimeEventType::TaskAborted,
            RuntimeEventSource::Runtime,
            json!({
                "summary": summary,
                "status": TaskStatus::Cancelled,
                "recovery": recovery,
                "post_task_governance": {"status": "pending"},
            }),
        ))
        .await
    }

    async fn record_orphaned_task_recovery(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        recovery: &str,
    ) -> Result<TaskRecoveryRecord, ClientError> {
        let events = self
            .repositories
            .events
            .load(session_id, Some(task_id), None)
            .await?;
        if let Some(record) = events.iter().rev().find_map(|event| {
            matches!(
                event.event_type,
                RuntimeEventType::TaskInterrupted | RuntimeEventType::TaskUncertain
            )
            .then(|| event.payload.get("record").cloned())
            .flatten()
        }) {
            return serde_json::from_value(record).map_err(ClientError::Serialization);
        }
        let record = recovery::analyze_task(&events, task_id, &runtime_identity());
        let (event_type, status) = match record.disposition {
            golutra_core::TaskRecoveryDisposition::Interrupted => {
                (RuntimeEventType::TaskInterrupted, TaskStatus::Interrupted)
            }
            golutra_core::TaskRecoveryDisposition::Uncertain => {
                (RuntimeEventType::TaskUncertain, TaskStatus::Uncertain)
            }
        };
        let mut event = host_event(
            self.next_sequence_no(),
            session_id,
            Some(task_id),
            event_type,
            RuntimeEventSource::Runtime,
            json!({
                "summary": &record.reason,
                "status": status,
                "recovery": recovery,
                "record": &record,
                "safe_to_replay": false,
                "post_task_governance": {"status": "pending"},
            }),
        );
        event.turn_id = events.iter().rev().find_map(|event| {
            matches!(
                event.event_type,
                RuntimeEventType::TurnStarted | RuntimeEventType::TaskCreated
            )
            .then_some(event.turn_id)
            .flatten()
        });
        self.record_event(event).await?;
        Ok(record)
    }

    async fn record_event(&self, event: RuntimeEvent) -> Result<(), ClientError> {
        let _writer = self.event_writer.lock().await;
        let causal_before = self.causal_ledger.lock().await.clone();
        let event = match self.prepare_canonical_event(event).await {
            Ok(event) => event,
            Err(error) => {
                *self.causal_ledger.lock().await = causal_before;
                return Err(error);
            }
        };
        let event = match self
            .repositories
            .events
            .append_assigning_sequence(event)
            .await
        {
            Ok(event) => event,
            Err(error) => {
                *self.causal_ledger.lock().await = causal_before;
                return Err(error.into());
            }
        };
        self.publish_committed_event(event).await
    }

    async fn claim_command_journal(
        &self,
        idempotency_key: &str,
        command_id: CommandId,
        provisional_ack: &CommandAck,
        receipt_event: RuntimeEvent,
    ) -> Result<CommandClaim, ClientError> {
        let _writer = self.event_writer.lock().await;
        let causal_before = self.causal_ledger.lock().await.clone();
        let receipt_event = match self.prepare_canonical_event(receipt_event).await {
            Ok(event) => event,
            Err(error) => {
                *self.causal_ledger.lock().await = causal_before;
                return Err(error);
            }
        };
        let claim = match self
            .repositories
            .events
            .claim_command(idempotency_key, command_id, provisional_ack, receipt_event)
            .await
        {
            Ok(claim) => claim,
            Err(error) => {
                *self.causal_ledger.lock().await = causal_before;
                return Err(error.into());
            }
        };
        if let CommandClaim::Claimed {
            receipt_event: Some(event),
        } = &claim
        {
            self.publish_committed_event(event.clone()).await?;
        } else {
            *self.causal_ledger.lock().await = causal_before;
        }
        Ok(claim)
    }

    async fn complete_command_journal(
        &self,
        idempotency_key: &str,
        command_id: CommandId,
        ack: &CommandAck,
        completion_event: RuntimeEvent,
    ) -> Result<(), ClientError> {
        let _writer = self.event_writer.lock().await;
        let causal_before = self.causal_ledger.lock().await.clone();
        let completion_event = match self.prepare_canonical_event(completion_event).await {
            Ok(event) => event,
            Err(error) => {
                *self.causal_ledger.lock().await = causal_before;
                return Err(error);
            }
        };
        let event = match self
            .repositories
            .events
            .complete_command(idempotency_key, command_id, ack, completion_event)
            .await
        {
            Ok(event) => event,
            Err(error) => {
                *self.causal_ledger.lock().await = causal_before;
                return Err(error.into());
            }
        };
        self.publish_committed_event(event).await
    }

    async fn publish_committed_event(&self, event: RuntimeEvent) -> Result<(), ClientError> {
        self.append_rollout_event(&event).await?;
        let _ = self.event_bus.send(event);
        Ok(())
    }

    async fn run_storage_maintenance(&self) -> Result<StorageMaintenanceReport, ClientError> {
        let now = chrono::Utc::now();
        let artifact_report = self.store.run_artifact_maintenance(now).await?;
        let checkpoint_directories_removed = if let (Some(workspace_root), Some(paths)) =
            (self.workspace_root.clone(), self.runtime_paths.clone())
        {
            run_blocking(move || {
                WorkspaceCheckpointManager::new(workspace_root, paths.checkpoints_dir)
                    .prune_checkpoints(CHECKPOINTS_TO_RETAIN_PER_WORKSPACE)
                    .map_err(|error| ClientError::Io(error.to_string()))
            })
            .await??
        } else {
            0
        };
        Ok(StorageMaintenanceReport {
            artifact_blobs_removed: artifact_report.artifact_blobs_removed,
            protected_artifacts_skipped: artifact_report.protected_artifacts_skipped,
            temporary_artifacts_removed: artifact_report.temporary_artifacts_removed,
            checkpoint_directories_removed,
            completed_at: now,
            stats: self.storage_stats().await?,
        })
    }

    async fn storage_stats(&self) -> Result<StorageStats, ClientError> {
        let mut stats = self.store.storage_stats().await?;
        if let (Some(workspace_root), Some(paths)) =
            (self.workspace_root.clone(), self.runtime_paths.clone())
        {
            let (checkpoint_directories, rollout_files) = run_blocking(move || {
                let checkpoint_directories =
                    WorkspaceCheckpointManager::new(workspace_root, &paths.checkpoints_dir)
                        .checkpoint_count()
                        .map_err(|error| ClientError::Io(error.to_string()))?;
                let rollout_files = count_regular_directory_entries(&paths.rollouts_dir, "jsonl")?;
                Ok::<_, ClientError>((checkpoint_directories, rollout_files))
            })
            .await??;
            stats.checkpoint_directories = checkpoint_directories;
            stats.rollout_files = rollout_files;
        }
        Ok(stats)
    }

    async fn synchronize_workspace_rollouts(&self) -> Result<(), ClientError> {
        let Some(workspace_root) = self.workspace_root_string() else {
            return Ok(());
        };
        let threads = self
            .repositories
            .threads
            .list(Some(&workspace_root), u32::MAX)
            .await?;
        for mut thread in threads {
            self.ensure_thread_rollout_path(&mut thread).await?;
            self.rebuild_thread_rollout(&thread).await?;
        }
        Ok(())
    }

    async fn ensure_thread_rollout_path(
        &self,
        thread: &mut ThreadRecord,
    ) -> Result<(), ClientError> {
        let Some(paths) = &self.runtime_paths else {
            thread.rollout_path = None;
            return Ok(());
        };
        let expected = paths.rollout_path(thread.thread_id).display().to_string();
        if thread.rollout_path.as_deref() != Some(expected.as_str()) {
            thread.rollout_path = Some(expected);
            thread.updated_at = chrono::Utc::now();
            self.repositories.threads.upsert(thread).await?;
        }
        Ok(())
    }

    async fn append_rollout_event(&self, event: &RuntimeEvent) -> Result<(), ClientError> {
        let Some(mut thread) = self
            .repositories
            .threads
            .by_session(event.session_id)
            .await?
        else {
            return Ok(());
        };
        self.ensure_thread_rollout_path(&mut thread).await?;
        let Some(path) = thread.rollout_path.as_deref().map(PathBuf::from) else {
            return Ok(());
        };
        if !path.exists() {
            self.rebuild_thread_rollout(&thread).await?;
            return Ok(());
        }
        let line = rollout_line(&thread, event)?;
        run_blocking(move || append_rollout_line(&path, &line)).await??;
        Ok(())
    }

    async fn rebuild_thread_rollout(
        &self,
        thread: &ThreadRecord,
    ) -> Result<RolloutExport, ClientError> {
        let Some(path) = thread.rollout_path.as_deref().map(PathBuf::from) else {
            return Ok(RolloutExport {
                thread_id: thread.thread_id,
                session_id: thread.session_id,
                path: String::new(),
                event_count: 0,
                last_sequence_no: None,
            });
        };
        let events = self
            .repositories
            .events
            .load(thread.session_id, None, None)
            .await?;
        let lines = events
            .iter()
            .map(|event| rollout_line(thread, event))
            .collect::<Result<Vec<_>, _>>()?;
        let last_sequence_no = events.last().map(|event| event.sequence_no);
        let event_count = events.len();
        let export_path = path.display().to_string();
        run_blocking(move || rebuild_rollout_file(&path, &lines)).await??;
        Ok(RolloutExport {
            thread_id: thread.thread_id,
            session_id: thread.session_id,
            path: export_path,
            event_count,
            last_sequence_no,
        })
    }

    async fn persisted_active_task(
        &self,
        session_id: SessionId,
    ) -> Result<Option<TaskId>, ClientError> {
        let state = self
            .repositories
            .projections
            .state(session_id, None)
            .await?;
        if is_active_status(state.task_status) {
            Ok(state.active_task_id)
        } else {
            Ok(None)
        }
    }

    async fn wait_for_finishing_task_control(
        &self,
        session_id: SessionId,
    ) -> Result<(), ClientError> {
        let mut completion = self
            .task_controls
            .lock()
            .await
            .get(&session_id)
            .map(|control| control.completion.clone());
        let Some(completion) = completion.as_mut() else {
            return Ok(());
        };
        if !*completion.borrow() {
            wait_for_task_control_cleanup(completion, TASK_CONTROL_CLEANUP_TIMEOUT).await?;
        }
        Ok(())
    }

    async fn acquire_command_lease(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<File>, ClientError> {
        let Some(paths) = &self.runtime_paths else {
            return Ok(None);
        };
        let path = paths.command_lock(idempotency_key);
        run_blocking(move || {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|error| ClientError::Io(format!("{}: {error}", path.display())))?;
            set_owner_only_file(&path)?;
            file.lock_exclusive()
                .map_err(|error| ClientError::Io(format!("{}: {error}", path.display())))?;
            Ok(file)
        })
        .await?
        .map(Some)
    }

    fn try_acquire_session_lease(
        &self,
        session_id: SessionId,
    ) -> Result<SessionLeaseAttempt, ClientError> {
        let Some(paths) = &self.runtime_paths else {
            return Ok(SessionLeaseAttempt::Acquired(None));
        };
        let path = paths.session_lock(session_id);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| ClientError::Io(format!("{}: {error}", path.display())))?;
        set_owner_only_file(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(SessionLeaseAttempt::Acquired(Some(Arc::new(file)))),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Ok(SessionLeaseAttempt::Busy)
            }
            Err(error) => Err(ClientError::Io(format!("{}: {error}", path.display()))),
        }
    }

    async fn ensure_session_in_workspace(&self, session_id: SessionId) -> Result<(), ClientError> {
        if let Some(thread) = self.repositories.threads.by_session(session_id).await? {
            self.ensure_thread_in_workspace(&thread)?;
        }
        Ok(())
    }

    async fn ensure_owned_session_in_workspace(
        &self,
        session_id: SessionId,
    ) -> Result<(), ClientError> {
        if session_id == self.default_session_id {
            return Ok(());
        }
        if let Some(thread) = self.repositories.threads.by_session(session_id).await? {
            return self.ensure_thread_in_workspace(&thread);
        }
        Err(ClientError::InvalidSession(format!(
            "session `{session_id}` has no ownership record in this workspace"
        )))
    }

    async fn ensure_task_in_session(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<(), ClientError> {
        self.ensure_session_in_workspace(session_id).await?;
        if self
            .repositories
            .events
            .load_page(session_id, Some(task_id), None, 1)
            .await?
            .is_empty()
        {
            return Err(ClientError::InvalidSession(format!(
                "task `{task_id}` was not found in session `{session_id}`"
            )));
        }
        Ok(())
    }

    fn workspace_root_string(&self) -> Option<String> {
        self.workspace_root
            .as_ref()
            .map(|path| path.to_string_lossy().to_string())
    }

    fn ensure_thread_in_workspace(&self, thread: &ThreadRecord) -> Result<(), ClientError> {
        let Some(workspace_root) = self.workspace_root_string() else {
            return Ok(());
        };
        if thread.workspace_root.as_deref() == Some(workspace_root.as_str()) {
            return Ok(());
        }
        Err(ClientError::InvalidSession(format!(
            "thread `{}` does not belong to workspace `{workspace_root}`",
            thread.thread_id
        )))
    }

    fn ensure_thread_not_removed(&self, thread: &ThreadRecord) -> Result<(), ClientError> {
        if !thread.removed {
            return Ok(());
        }
        Err(ClientError::InvalidSession(format!(
            "thread `{}` was removed",
            thread.thread_id
        )))
    }
}

fn task_id_from_candidate_id(candidate_id: &str) -> Option<TaskId> {
    [
        "automation-benchmark-",
        "automation-generated-task-",
        "automation-skill-",
        "automation-runtime-change-",
    ]
    .iter()
    .find_map(|prefix| candidate_id.strip_prefix(prefix))
    .and_then(|task_id| task_id.parse().ok())
}

fn fork_sequence_for_turn(events: &[RuntimeEvent], turn_id: TurnId) -> Option<u64> {
    let first_sequence = events
        .iter()
        .find(|event| event.turn_id == Some(turn_id))?
        .sequence_no;
    if let Some(next_turn_sequence) = events
        .iter()
        .find(|event| {
            event.sequence_no > first_sequence
                && event
                    .turn_id
                    .is_some_and(|event_turn_id| event_turn_id != turn_id)
        })
        .map(|event| event.sequence_no)
    {
        return Some(next_turn_sequence.saturating_sub(1));
    }
    events
        .iter()
        .filter(|event| event.turn_id == Some(turn_id))
        .filter(|event| event.event_type.is_task_terminal())
        .map(|event| event.sequence_no)
        .max()
        .or_else(|| {
            events
                .iter()
                .filter(|event| event.turn_id == Some(turn_id))
                .map(|event| event.sequence_no)
                .max()
        })
}

fn count_regular_directory_entries(path: &Path, extension: &str) -> Result<u64, ClientError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(ClientError::Io(error.to_string())),
    };
    let mut count = 0_u64;
    for entry in entries {
        let entry = entry.map_err(|error| ClientError::Io(error.to_string()))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| ClientError::Io(error.to_string()))?;
        if metadata.file_type().is_symlink() {
            return Err(ClientError::Io(format!(
                "runtime storage entry cannot be a symbolic link: {}",
                entry.path().display()
            )));
        }
        if metadata.is_file()
            && entry.path().extension().and_then(|value| value.to_str()) == Some(extension)
        {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

fn provider_auth_request_id_from_payload(
    payload: &Value,
) -> Result<Option<ProviderAuthRequestId>, ClientError> {
    payload
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            value.parse().map_err(|error: uuid::Error| {
                ClientError::TaskExecution(format!("provider auth request id is invalid: {error}"))
            })
        })
        .transpose()
}

fn provider_auth_failure_message(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    [
        "invalid_api_key",
        "invalid api key",
        "authentication_error",
        "unauthenticated",
        "credential is missing",
        "required env is not set",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

#[cfg(test)]
mod tests;
