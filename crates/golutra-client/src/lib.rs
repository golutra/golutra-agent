use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
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
    AgentToolProfile, ArtifactChunk, ArtifactReadRequest, CommandAck, EventFilter, EventPage,
    EventPageDirection, EventPageRequest, ExternalVerificationSpec, ProtocolVersionRange,
    RUNTIME_PROTOCOL_VERSION, RuntimeEvent, RuntimeEventSource, RuntimeEventType, RuntimeQuery,
    RuntimeQueryKind, SessionCommand, SessionCommandKind, StorageMaintenanceReport, StorageStats,
    TaskTracePage, TaskTraceRequest,
};
use golutra_runtime::{
    AgentExecutionControl, AgentExecutionHandle, AgentGovernorUsage, AgentHarness, AgentLoopError,
    AgentLoopTraceEvent, AgentTaskRequest, BeforeSideEffectRecorder, ConfiguredAgentRun,
    ConfiguredPendingAgentTurn, PendingAgentTurn, PendingTurnExecutionOptions, RuntimeLaneError,
    RuntimeLaneManager, RuntimeObservation, RuntimeObservationSink, RuntimeVerificationService,
    WorkspaceCheckpointManager, agent_execution_channel_with_cancellation,
    default_agent_max_elapsed_ms, is_active_status,
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
const MAX_LIVE_SUBSCRIPTIONS: usize = 1_024;
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
mod delegation_policy;
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
mod observation_recorder;
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
mod task_mode;
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
    effective_model_history_events, environment_context_prompt, load_project_instruction_bundle,
    memory_context, model_prompt_from_payload, preview_from_payload, prompt_from_payload,
    select_memories_for_context, system_prompt, task_contract_from_payload, title_from_payload,
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
    append_rollout_line, normalize_rebind_source, rebuild_rollout_file, remove_rollout_projection,
    rollout_line, rollout_path_for_workspace, rollout_projection_files,
};
#[cfg(test)]
pub(crate) use rollout::{redact_rollout_value, rollout_lock_path};
pub use run_bundle::{
    DebugExportManifestReceipt, DebugExportOutcome, ObservationBundleManifest,
    ObservationSessionManifest, ObservationTaskManifest, RawStateManifest, RunBundleExportRequest,
    RunBundleExporter, RunBundleFile, RunBundleManifest, RunBundlePath, RunBundleReceipt,
    RunBundleTerminalOutcome,
};
pub(crate) use task_mode::{
    NormalizedExecutionMode, TOOL_PROFILE_KEY, VERIFY_ON_CHANGE_KEY, execution_mode_from_payload,
    explicit_task_contract, should_apply_legacy_adapter, strict_execution_requested,
    tool_profile_from_payload, verify_on_change_auto, write_normalized_execution_mode,
};
pub use trace::merge_task_trace_page;
#[cfg(unix)]
pub use transport::UnixIpcTransport;
pub(crate) use transport::run_blocking;
pub use transport::{
    AppServerInfo, EmbeddedTransport, HttpSseTransport, RuntimeAttachment, RuntimeClient,
    RuntimeEventStream, RuntimeHostInfo, RuntimeOperation, RuntimeOperationClient,
    RuntimeOperationResult, RuntimeTransport, TaskTraceClient,
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
    #[error("runtime transport protocol failed: {0}")]
    Protocol(String),
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
struct RuntimeHostStorageState {
    store: RuntimeStore,
    repositories: RuntimeRepositories,
    memory_store: MemoryStore,
    evaluation_store: EvaluationStore,
    evolution_store: EvolutionStore,
    governance: governance::GovernanceService,
    deep_evaluation_inputs: Mutex<HashMap<PostTaskJobId, TaskEvaluationInput>>,
    checkpoint_evaluation_tasks: HashSet<TaskId>,
    _temporary_root: Option<Arc<tempfile::TempDir>>,
}

#[derive(Debug)]
struct RuntimeHostExecutionState {
    shutdown: CancellationToken,
    post_task_worker: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    lane_manager: Mutex<RuntimeLaneManager>,
    event_bus: broadcast::Sender<RuntimeEvent>,
    live_subscriptions: StdMutex<Vec<LiveSubscription>>,
    next_sequence_no: AtomicU64,
    event_writer: Mutex<()>,
    causal_ledger: Mutex<causal_recorder::CausalLedger>,
    command_mutex: Mutex<()>,
    task_controls: Mutex<HashMap<SessionId, HostedTaskControl>>,
    delegation_admissions: Mutex<HashMap<SessionId, delegation::DelegationAdmission>>,
    delegation_operations: Mutex<HashMap<String, Arc<delegation::DelegationOperation>>>,
    provider_auth_waiters: Mutex<HashMap<SessionId, PendingProviderAuth>>,
    process_supervisor: ProcessSupervisor,
    workspace_change_tracker: Mutex<change_tracker::WorkspaceChangeTracker>,
    rollout_projection_failures: Mutex<HashMap<SessionId, String>>,
}

#[derive(Debug)]
struct LiveSubscription {
    filter: EventFilter,
    sender: broadcast::Sender<RuntimeEvent>,
}

#[derive(Debug)]
pub struct RuntimeHost {
    storage: RuntimeHostStorageState,
    execution: RuntimeHostExecutionState,
    workspace_id: WorkspaceId,
    workspace_root: Option<PathBuf>,
    runtime_paths: Option<RuntimePaths>,
    provider_config_paths: Option<ProviderConfigPaths>,
    default_session_id: SessionId,
    default_thread_id: ThreadId,
    instance_id: String,
    started_at: chrono::DateTime<chrono::Utc>,
    force_mock_provider: bool,
    execution_options: RuntimeExecutionOptions,
}

impl Drop for RuntimeHost {
    fn drop(&mut self) {
        // Drop cannot await. Explicit owners should call `close()` while the
        // Tokio runtime is alive so process supervisors can finish bookkeeping.
        self.execution.shutdown.cancel();
        self.execution.process_supervisor.shutdown();
        if let Ok(mut worker) = self.execution.post_task_worker.lock()
            && let Some(worker) = worker.take()
        {
            worker.abort();
        }
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
    pub(crate) execution: PendingTurnExecutionOptions,
    continuation: Option<RecoveredTaskContinuation>,
}

const RECOVERED_PENDING_TURNS_KEY: &str = "recovered_pending_turns";
const RECOVERY_CONTINUATION_KEY: &str = "recovery_continuation";

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct RecoveredTaskContinuation {
    governor_usage: AgentGovernorUsage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    accounted_cost_response_ids: Vec<String>,
    delegation: Option<delegation_policy::TimedDelegationRecoveryState>,
}

#[derive(Debug, Default)]
struct RecoveredGovernorFacts {
    baseline: AgentGovernorUsage,
    max_governor_iterations: u32,
    max_governor_tool_calls: u32,
    max_governor_failed_tool_calls: u32,
    max_step_iteration: u32,
    completed_tool_calls: u32,
    completed_failed_tool_calls: u32,
    consecutive_failed_tool_calls: u32,
    provider_tool_call_ids: HashSet<String>,
    completed_tool_call_ids: HashSet<String>,
    estimated_cost_microusd: Option<u64>,
    accounted_cost_response_ids: Vec<String>,
    accounted_cost_response_id_set: HashSet<String>,
}

impl RecoveredGovernorFacts {
    fn usage(&self) -> AgentGovernorUsage {
        AgentGovernorUsage {
            iterations: self
                .baseline
                .iterations
                .max(self.max_governor_iterations)
                .max(self.max_step_iteration),
            tool_calls: self
                .baseline
                .tool_calls
                .saturating_add(self.completed_tool_calls)
                .max(self.max_governor_tool_calls),
            failed_tool_calls: self
                .baseline
                .failed_tool_calls
                .saturating_add(self.completed_failed_tool_calls)
                .max(self.max_governor_failed_tool_calls),
            consecutive_failed_tool_calls: self.consecutive_failed_tool_calls,
            estimated_cost_microusd: self.estimated_cost_microusd,
        }
    }

    fn merge_transfer(&mut self, transferred: &RecoveredTaskContinuation) {
        let current = self.usage();
        let incoming = transferred.governor_usage;
        let baseline = AgentGovernorUsage {
            iterations: current.iterations.max(incoming.iterations),
            tool_calls: current.tool_calls.max(incoming.tool_calls),
            failed_tool_calls: current.failed_tool_calls.max(incoming.failed_tool_calls),
            // Unlike cumulative counters, a successful tool call legitimately
            // resets this value. The later transfer is therefore authoritative.
            consecutive_failed_tool_calls: incoming.consecutive_failed_tool_calls,
            estimated_cost_microusd: match (
                current.estimated_cost_microusd,
                incoming.estimated_cost_microusd,
            ) {
                (Some(current), Some(incoming)) => Some(current.max(incoming)),
                (Some(value), None) | (None, Some(value)) => Some(value),
                (None, None) => None,
            },
        };
        for response_id in &transferred.accounted_cost_response_ids {
            if self
                .accounted_cost_response_id_set
                .insert(response_id.clone())
            {
                self.accounted_cost_response_ids.push(response_id.clone());
            }
        }
        self.baseline = baseline;
        self.max_governor_iterations = baseline.iterations;
        self.max_governor_tool_calls = baseline.tool_calls;
        self.max_governor_failed_tool_calls = baseline.failed_tool_calls;
        self.max_step_iteration = baseline.iterations;
        self.completed_tool_calls = 0;
        self.completed_failed_tool_calls = 0;
        self.consecutive_failed_tool_calls = baseline.consecutive_failed_tool_calls;
        self.provider_tool_call_ids.clear();
        self.completed_tool_call_ids.clear();
        self.estimated_cost_microusd = baseline.estimated_cost_microusd;
    }

    fn observe_governor(&mut self, record: &Value) {
        if let Some(value) = value_u32(record.get("iteration")) {
            self.max_governor_iterations = self.max_governor_iterations.max(value);
        }
        if let Some(value) = value_u32(record.get("tool_calls")) {
            self.max_governor_tool_calls = self.max_governor_tool_calls.max(value);
        }
        if let Some(value) = value_u32(record.get("failed_tool_calls")) {
            self.max_governor_failed_tool_calls = self.max_governor_failed_tool_calls.max(value);
        }
        if let Some(value) = value_u32(record.get("consecutive_failed_tool_calls")) {
            self.consecutive_failed_tool_calls = value;
        }
    }

    fn observe_step_started(&mut self, event: &RuntimeEvent) -> bool {
        let Some(step_no) = value_u32(event.payload.pointer("/step/step_no")) else {
            return false;
        };
        self.max_step_iteration = self.max_step_iteration.max(
            self.baseline
                .iterations
                .saturating_add(step_no)
                .saturating_add(1),
        );
        true
    }

    fn observe_tool_started(&mut self, event: &RuntimeEvent) {
        if event
            .payload
            .get("provider_tool_call_id")
            .is_some_and(|value| !value.is_null())
            && let Some(tool_call_id) = event.payload.get("tool_call_id").and_then(Value::as_str)
        {
            self.provider_tool_call_ids.insert(tool_call_id.to_owned());
        }
    }

    fn observe_tool_completed(&mut self, event: &RuntimeEvent) -> bool {
        let Some(tool_call_id) = event
            .payload
            .pointer("/envelope/tool_call_id")
            .and_then(Value::as_str)
        else {
            return false;
        };
        let Some(status) = event
            .payload
            .pointer("/envelope/status")
            .and_then(Value::as_str)
        else {
            return false;
        };
        if !self.provider_tool_call_ids.contains(tool_call_id) {
            return false;
        }
        if !self.completed_tool_call_ids.insert(tool_call_id.to_owned()) {
            return false;
        }

        self.completed_tool_calls = self.completed_tool_calls.saturating_add(1);
        if status == "ok" {
            self.consecutive_failed_tool_calls = 0;
        } else {
            self.completed_failed_tool_calls = self.completed_failed_tool_calls.saturating_add(1);
            self.consecutive_failed_tool_calls =
                self.consecutive_failed_tool_calls.saturating_add(1);
        }
        true
    }

    fn observe_token_usage(&mut self, record: TokenUsageRecord) -> bool {
        let Some(cost) = record.estimated_cost.and_then(cost_to_microusd) else {
            return false;
        };
        let response_id = record.response_event_id.to_string();
        if !self
            .accounted_cost_response_id_set
            .insert(response_id.clone())
        {
            return false;
        }
        self.accounted_cost_response_ids.push(response_id);
        self.estimated_cost_microusd = Some(
            self.estimated_cost_microusd
                .unwrap_or_default()
                .saturating_add(cost),
        );
        true
    }
}

pub(crate) enum DelegationContextSeed {
    Live(delegation_policy::DelegationContext),
    Recovered(delegation_policy::TimedDelegationRecoveryState),
}

#[derive(Debug, Clone)]
struct RecoveredActiveTurnSurface {
    payload: Value,
    budget_turn_id: Option<TurnId>,
    budget_started_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn is_pending_transfer_event(event: &RuntimeEvent) -> bool {
    event.event_type == RuntimeEventType::TurnQueued
        && event.turn_id.is_none()
        && (event
            .payload
            .get("recovered_pending_sequence_nos")
            .is_some()
            || event.payload.get(RECOVERED_PENDING_TURNS_KEY).is_some())
}

fn inline_recovered_pending_event(
    source: &RuntimeEvent,
    entry: &Value,
) -> Result<RuntimeEvent, ClientError> {
    let turn_id = entry
        .get("turn_id")
        .cloned()
        .ok_or_else(|| {
            ClientError::TaskExecution("recovery transfer entry is missing turn_id".to_owned())
        })
        .and_then(|value| {
            serde_json::from_value(value).map_err(|error| {
                ClientError::TaskExecution(format!(
                    "recovery transfer entry has an invalid turn_id: {error}"
                ))
            })
        })?;
    let payload = entry.get("payload").cloned().ok_or_else(|| {
        ClientError::TaskExecution(format!(
            "recovery transfer entry for turn {turn_id} is missing payload"
        ))
    })?;
    let command_id = entry
        .get("command_id")
        .cloned()
        .unwrap_or_else(|| Value::String(CommandId::default().to_string()));
    let actor = entry
        .get("actor")
        .cloned()
        .or_else(|| {
            source
                .payload
                .get("runtime_lane")
                .and_then(|lane| lane.get("active_controller"))
                .cloned()
        })
        .unwrap_or_else(|| {
            json!({
                "kind": "runtime",
                "id": "runtime-pending-turn-recovery"
            })
        });
    let sequence_no = entry
        .get("sequence_no")
        .and_then(Value::as_u64)
        .unwrap_or(source.sequence_no);
    let mut event = source.clone();
    event.sequence_no = sequence_no;
    event.turn_id = Some(turn_id);
    event.payload = json!({
        "command_id": command_id,
        "payload": payload,
        "runtime_lane": {
            "active_controller": actor,
        },
    });
    Ok(event)
}

async fn recoverable_transfer_turns(
    host: &RuntimeHost,
    session_id: SessionId,
    events: &[RuntimeEvent],
) -> Result<HashMap<TurnId, RecoveredPendingTurn>, ClientError> {
    let mut transferred = HashMap::new();
    for event in events
        .iter()
        .filter(|event| is_pending_transfer_event(event))
    {
        if let Some(entries) = event
            .payload
            .get(RECOVERED_PENDING_TURNS_KEY)
            .and_then(Value::as_array)
        {
            for entry in entries {
                let synthetic = inline_recovered_pending_event(event, entry)?;
                if let Some(recovered) = recovered_pending_turn_from_event(&synthetic)? {
                    transferred.insert(recovered.pending.turn_id, recovered);
                }
            }
        }
        let referenced_sequences = event
            .payload
            .get("recovered_pending_sequence_nos")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64);
        for sequence_no in referenced_sequences {
            if let Some(referenced_event) = host
                .storage
                .repositories
                .events
                .load_by_sequence(session_id, sequence_no)
                .await?
                && let Some(recovered) = recovered_pending_turn_from_event(&referenced_event)?
            {
                transferred
                    .entry(recovered.pending.turn_id)
                    .or_insert(recovered);
            }
        }
    }
    Ok(transferred)
}

fn recovery_continuation_from_value(
    value: &Value,
) -> Result<RecoveredTaskContinuation, ClientError> {
    serde_json::from_value(value.clone()).map_err(|error| {
        ClientError::TaskExecution(format!(
            "durable recovery continuation is malformed: {error}"
        ))
    })
}

fn value_u32(value: Option<&Value>) -> Option<u32> {
    value
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn cost_to_microusd(cost_usd: f64) -> Option<u64> {
    if !cost_usd.is_finite() || cost_usd.is_sign_negative() {
        return None;
    }
    let microusd = cost_usd * 1_000_000.0;
    if microusd >= u64::MAX as f64 {
        Some(u64::MAX)
    } else {
        Some(microusd.round() as u64)
    }
}

fn delegation_recovery_from_metadata(
    metadata: &Value,
    captured_at: chrono::DateTime<chrono::Utc>,
) -> Result<delegation_policy::TimedDelegationRecoveryState, ClientError> {
    let budget = metadata.get("budget").unwrap_or(metadata);
    let parse_budget = |key: &str| {
        budget.get(key).and_then(Value::as_u64).ok_or_else(|| {
            ClientError::TaskExecution(format!(
                "delegation recovery metadata is missing numeric field `{key}`"
            ))
        })
    };
    let optional_id = |key: &str| -> Result<Option<Uuid>, ClientError> {
        match metadata.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => serde_json::from_value(value.clone())
                .map(Some)
                .map_err(|error| {
                    ClientError::TaskExecution(format!(
                        "delegation recovery metadata has an invalid `{key}`: {error}"
                    ))
                }),
        }
    };
    let root_session_id = metadata
        .get("root_session_id")
        .cloned()
        .ok_or_else(|| {
            ClientError::TaskExecution(
                "delegation recovery metadata is missing root_session_id".to_owned(),
            )
        })
        .and_then(|value| {
            serde_json::from_value(value).map_err(|error| {
                ClientError::TaskExecution(format!(
                    "delegation recovery metadata has an invalid root_session_id: {error}"
                ))
            })
        })?;
    let reserved_tokens = parse_budget("reserved_tokens")?;
    let reserved_cost_microusd = parse_budget("reserved_cost_microusd")?;
    let active_children = parse_budget("active_children")?;
    let max_tokens = parse_budget("max_tokens")?;
    let spent_tokens = parse_budget("spent_tokens")?;
    let spent_cost_microusd = parse_budget("spent_cost_microusd")?;
    let has_unsettled_reservation =
        active_children > 0 || reserved_tokens > 0 || reserved_cost_microusd > 0;
    let state = delegation_policy::DelegationRecoveryState {
        root_session_id,
        parent_session_id: optional_id("parent_session_id")?.map(SessionId),
        parent_task_id: optional_id("parent_task_id")?.map(TaskId),
        parent_thread_id: optional_id("parent_thread_id")?.map(ThreadId),
        // Context metadata has always stored depth beside `budget`. Accept a
        // budget-local value as well for compatibility with early recovery
        // snapshots that flattened all numeric fields into one object.
        depth: u8::try_from(
            metadata
                .get("depth")
                .and_then(Value::as_u64)
                .or_else(|| budget.get("depth").and_then(Value::as_u64))
                .ok_or_else(|| {
                    ClientError::TaskExecution(
                        "delegation recovery metadata is missing numeric field `depth`".to_owned(),
                    )
                })?,
        )
        .map_err(|_| {
            ClientError::TaskExecution(
                "delegation recovery metadata depth is out of range".to_owned(),
            )
        })?,
        remaining_elapsed_ms: parse_budget("remaining_elapsed_ms")?,
        local_remaining_elapsed_ms: metadata
            .get("local_remaining_elapsed_ms")
            .and_then(Value::as_u64),
        max_tokens,
        max_cost_microusd: match budget.get("max_cost_microusd") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_u64().ok_or_else(|| {
                ClientError::TaskExecution(
                    "delegation recovery metadata has an invalid max_cost_microusd: expected u64"
                        .to_owned(),
                )
            })?),
        },
        started_children: usize::try_from(parse_budget("started_children")?).unwrap_or(usize::MAX),
        // 活动 reservation 代表旧进程可能已经执行但尚未完成结算。恢复时必须先耗尽
        // 对应上限，只有新的 durable checkpoint 才能用实际 usage 替换这个保守状态。
        spent_tokens: if has_unsettled_reservation {
            spent_tokens.max(max_tokens)
        } else {
            spent_tokens.saturating_add(reserved_tokens)
        },
        spent_cost_microusd: if has_unsettled_reservation {
            match budget.get("max_cost_microusd").and_then(Value::as_u64) {
                Some(max_cost) => spent_cost_microusd.max(max_cost),
                None => spent_cost_microusd,
            }
        } else {
            spent_cost_microusd.saturating_add(reserved_cost_microusd)
        },
    };
    Ok(delegation_policy::TimedDelegationRecoveryState { captured_at, state })
}

fn validate_canonical_delegation_checkpoint(
    event: &RuntimeEvent,
    candidate: &delegation_policy::TimedDelegationRecoveryState,
    previous: Option<&delegation_policy::TimedDelegationRecoveryState>,
) -> Result<(), ClientError> {
    let state = &candidate.state;
    if state.root_session_id != event.session_id {
        return Err(ClientError::TaskExecution(format!(
            "durable delegation recovery checkpoint root session {} does not match event session {}",
            state.root_session_id, event.session_id
        )));
    }
    if state.depth != 0
        || state.parent_session_id.is_some()
        || state.parent_task_id.is_some()
        || state.parent_thread_id.is_some()
    {
        return Err(ClientError::TaskExecution(
            "durable delegation recovery checkpoint is not a canonical root state".to_owned(),
        ));
    }
    let Some(previous) = previous else {
        return Ok(());
    };
    let previous = &previous.state;
    if previous.root_session_id != state.root_session_id
        || previous.max_tokens != state.max_tokens
        || previous.max_cost_microusd != state.max_cost_microusd
    {
        return Err(ClientError::TaskExecution(
            "durable delegation recovery checkpoint changes immutable root budget identity"
                .to_owned(),
        ));
    }
    if state.started_children < previous.started_children {
        return Err(ClientError::TaskExecution(
            "durable delegation recovery checkpoint regresses started child count".to_owned(),
        ));
    }
    Ok(())
}

fn recovered_task_continuation(
    events: &[RuntimeEvent],
    recovered_at: chrono::DateTime<chrono::Utc>,
) -> Result<Option<RecoveredTaskContinuation>, ClientError> {
    let mut governor = RecoveredGovernorFacts::default();
    let mut has_continuation = false;
    let mut delegation = None;
    let mut canonical_delegation = None;
    let mut has_authoritative_delegation_state = false;

    for event in events {
        if is_pending_transfer_event(event)
            && let Some(value) = event.payload.get(RECOVERY_CONTINUATION_KEY)
        {
            let transferred = recovery_continuation_from_value(value)?;
            governor.merge_transfer(&transferred);
            delegation = transferred.delegation;
            if let Some(candidate) = delegation.as_ref() {
                validate_canonical_delegation_checkpoint(event, candidate, None)?;
                canonical_delegation = Some(candidate.clone());
            }
            has_authoritative_delegation_state = true;
            has_continuation = true;
        }

        if event.event_type == RuntimeEventType::GovernorDecided
            && let Some(record) = event.payload.get("record")
        {
            governor.observe_governor(record);
            has_continuation = true;
        }

        if event.event_type == RuntimeEventType::StepStarted && governor.observe_step_started(event)
        {
            has_continuation = true;
        }

        if event.event_type == RuntimeEventType::ToolStarted {
            governor.observe_tool_started(event);
        }

        if event.event_type == RuntimeEventType::ToolCompleted
            && governor.observe_tool_completed(event)
        {
            has_continuation = true;
        }

        if matches!(
            event.event_type,
            RuntimeEventType::TaskCreated | RuntimeEventType::TurnStarted
        ) && !has_authoritative_delegation_state
            && let Some(metadata) = event
                .payload
                .get("payload")
                .and_then(|payload| payload.get("_delegation"))
        {
            delegation = Some(delegation_recovery_from_metadata(
                metadata,
                event.timestamp,
            )?);
        }

        if event.event_type == RuntimeEventType::TokenUsageRecorded {
            let record = decode_token_usage_record(event)?;
            if governor.observe_token_usage(record) {
                has_continuation = true;
            }
        }

        if event.event_type == RuntimeEventType::CheckpointCreated
            && event.payload.get("recovery_kind").and_then(Value::as_str)
                == Some("delegation_budget")
            && let Some(value) = event.payload.get("delegation_recovery")
        {
            let candidate = serde_json::from_value(value.clone()).map_err(|error| {
                ClientError::TaskExecution(format!(
                    "durable delegation recovery checkpoint is malformed: {error}"
                ))
            })?;
            validate_canonical_delegation_checkpoint(
                event,
                &candidate,
                canonical_delegation.as_ref(),
            )?;
            canonical_delegation = Some(candidate.clone());
            delegation = Some(candidate);
            has_authoritative_delegation_state = true;
        }
    }

    let continuation = RecoveredTaskContinuation {
        governor_usage: governor.usage(),
        accounted_cost_response_ids: governor.accounted_cost_response_ids,
        delegation: delegation.map(|state| state.refreshed(recovered_at)),
    };
    if has_continuation || continuation.delegation.is_some() {
        Ok(Some(continuation))
    } else {
        Ok(None)
    }
}

fn decode_token_usage_record(event: &RuntimeEvent) -> Result<TokenUsageRecord, ClientError> {
    let value = event.payload.get("record").ok_or_else(|| {
        ClientError::TaskExecution(format!(
            "token usage event at sequence {} is missing its record",
            event.sequence_no
        ))
    })?;
    serde_json::from_value(value.clone()).map_err(|error| {
        ClientError::TaskExecution(format!(
            "token usage event at sequence {} is malformed: {error}",
            event.sequence_no
        ))
    })
}

fn inherit_steering_execution_surface(
    active_task_payload: &Value,
    steering_payload: &mut Value,
) -> Result<(), ClientError> {
    let execution_mode = execution_mode_from_payload(active_task_payload).map_err(|error| {
        ClientError::TaskExecution(format!(
            "cannot recover a leading steering turn from an invalid active execution mode: {error}"
        ))
    })?;
    let inherited_tool_profile = tool_profile_from_payload(active_task_payload)
        .map_err(|error| {
            ClientError::TaskExecution(format!(
                "cannot recover a leading steering turn from an invalid active tool profile: {error}"
            ))
        })?;
    let mut task_contract = task_contract_from_payload(active_task_payload)?;
    if !explicit_task_contract(active_task_payload)
        && should_apply_legacy_adapter(active_task_payload, execution_mode)
    {
        LegacyTaskAdapter::new(
            active_task_payload,
            &model_prompt_from_payload(active_task_payload),
        )
        .apply_to(&mut task_contract);
    }

    let explicit_tool_profile = steering_payload
        .get(TOOL_PROFILE_KEY)
        .is_some_and(|value| !value.is_null());
    let steering_object = steering_payload.as_object_mut().ok_or_else(|| {
        ClientError::TaskExecution(
            "cannot recover a leading steering turn from a non-object payload".to_owned(),
        )
    })?;
    for key in [
        "completion_criteria",
        "output_schema",
        "external_verifiers",
        "max_elapsed_ms",
        "allow_network",
        "yolo",
        "defer_external_verification",
        VERIFY_ON_CHANGE_KEY,
        "provider_profile",
        "provider_model",
        "provider_generation_config",
        EXTERNAL_VERIFIERS_REQUIRE_OS_SANDBOX_KEY,
        crate::delegation::DELEGATED_TASK_MARKER,
        "_delegation_parent_session_id",
        "_delegation_parent_tool_call_id",
        "_delegation",
        delegation_policy::DELEGATION_COST_BUDGET_KEY,
    ] {
        match active_task_payload.get(key) {
            Some(value) => {
                steering_object.insert(key.to_owned(), value.clone());
            }
            None => {
                steering_object.remove(key);
            }
        }
    }
    steering_object.insert(
        "task_contract".to_owned(),
        serde_json::to_value(task_contract)?,
    );
    steering_object.insert(
        "_task_contract_origin".to_owned(),
        Value::String("active_task".to_owned()),
    );
    write_normalized_execution_mode(steering_payload, execution_mode);
    if !explicit_tool_profile {
        steering_payload[TOOL_PROFILE_KEY] = serde_json::to_value(inherited_tool_profile)?;
    }
    Ok(())
}

fn materialize_recovered_leading_steer(
    active_surface: &RecoveredActiveTurnSurface,
    recovered_at: chrono::DateTime<chrono::Utc>,
    steering_payload: &mut Value,
) -> Result<(), ClientError> {
    inherit_steering_execution_surface(&active_surface.payload, steering_payload)?;
    match active_surface.payload.get("max_elapsed_ms") {
        None | Some(Value::Null) => {
            let budget_ms = default_agent_max_elapsed_ms();
            let elapsed_ms = recovered_elapsed_ms(active_surface.budget_started_at, recovered_at);
            steering_payload["max_elapsed_ms"] = json!(budget_ms.saturating_sub(elapsed_ms).max(1));
        }
        Some(value) if value.as_u64().is_some_and(|value| value > 0) => {
            let budget_ms = value.as_u64().expect("positive elapsed budget");
            let elapsed_ms = recovered_elapsed_ms(active_surface.budget_started_at, recovered_at);
            steering_payload["max_elapsed_ms"] = json!(budget_ms.saturating_sub(elapsed_ms).max(1));
        }
        Some(_) => {
            return Err(ClientError::TaskExecution(
                "cannot recover a leading steering turn from an invalid active elapsed-time budget"
                    .to_owned(),
            ));
        }
    }
    Ok(())
}

fn recovered_steer_has_materialized_surface(payload: &Value) -> bool {
    payload.get("steer").and_then(Value::as_bool) == Some(true)
        && payload.get("_task_contract_origin").and_then(Value::as_str) == Some("active_task")
        && payload
            .get("task_contract")
            .is_some_and(|value| value.is_object())
        && matches!(
            payload.get("_execution_mode").and_then(Value::as_str),
            Some("legacy" | "open" | "strict")
        )
        && matches!(
            payload.get(TOOL_PROFILE_KEY).and_then(Value::as_str),
            Some("coding" | "full")
        )
        && payload
            .get("max_elapsed_ms")
            .and_then(Value::as_u64)
            .is_some_and(|value| value > 0)
}

fn recovered_elapsed_ms(
    budget_started_at: Option<chrono::DateTime<chrono::Utc>>,
    recovered_at: chrono::DateTime<chrono::Utc>,
) -> u64 {
    let Some(budget_started_at) = budget_started_at else {
        return 0;
    };
    let elapsed_ms = recovered_at
        .signed_duration_since(budget_started_at)
        .num_milliseconds()
        .max(0);
    u64::try_from(elapsed_ms).unwrap_or(u64::MAX)
}

fn recovered_active_turn_surface(
    events: &[RuntimeEvent],
    transferred_payloads: &HashMap<TurnId, Value>,
) -> Result<Option<RecoveredActiveTurnSurface>, ClientError> {
    let mut active = None::<RecoveredActiveTurnSurface>;
    let mut queued_payloads = HashMap::<TurnId, Value>::new();

    for event in events {
        match event.event_type {
            RuntimeEventType::TaskCreated => {
                if let Some(payload) = event.payload.get("payload").cloned() {
                    active = Some(RecoveredActiveTurnSurface {
                        payload,
                        budget_turn_id: event.turn_id,
                        budget_started_at: None,
                    });
                }
            }
            RuntimeEventType::TurnQueued | RuntimeEventType::TurnUpdated => {
                if let (Some(turn_id), Some(payload)) =
                    (event.turn_id, event.payload.get("payload").cloned())
                {
                    queued_payloads.insert(turn_id, payload);
                }
            }
            RuntimeEventType::TurnCancelled => {
                if let Some(turn_id) = event.turn_id {
                    queued_payloads.remove(&turn_id);
                }
            }
            RuntimeEventType::TurnStarted => {
                let Some(turn_id) = event.turn_id else {
                    continue;
                };
                let inline_payload = event.payload.get("payload").cloned();
                let source_payload = inline_payload
                    .clone()
                    .or_else(|| queued_payloads.get(&turn_id).cloned())
                    .or_else(|| transferred_payloads.get(&turn_id).cloned())
                    .ok_or_else(|| {
                        ClientError::TaskExecution(format!(
                            "cannot recover active turn {turn_id}: its durable payload is missing"
                        ))
                    })?;
                queued_payloads.remove(&turn_id);
                let steer = source_payload
                    .get("steer")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let restarted_after_recovery =
                    event.payload.get("recovery").and_then(Value::as_str)
                        == Some("durable_pending_turn");
                if steer && !restarted_after_recovery {
                    let current = active.as_mut().ok_or_else(|| {
                        ClientError::TaskExecution(format!(
                            "cannot recover steering turn {turn_id} without an active execution surface"
                        ))
                    })?;
                    let mut merged = source_payload;
                    inherit_steering_execution_surface(&current.payload, &mut merged)?;
                    current.payload = merged;
                } else {
                    active = Some(RecoveredActiveTurnSurface {
                        payload: source_payload,
                        budget_turn_id: Some(turn_id),
                        budget_started_at: None,
                    });
                }
            }
            RuntimeEventType::StepStarted => {
                if let Some(current) = active.as_mut()
                    && current.budget_started_at.is_none()
                    && current
                        .budget_turn_id
                        .is_none_or(|turn_id| event.turn_id == Some(turn_id))
                {
                    current.budget_started_at = Some(event.timestamp);
                }
            }
            _ => {}
        }
    }

    Ok(active)
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
    delegation: Option<delegation_policy::DelegationContext>,
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
    trace_sender: observation_recorder::ObservationSender,
}

#[async_trait]
impl BeforeSideEffectRecorder for HostedCheckpointRecorder {
    async fn persist_before_side_effect(
        &self,
        request: &ToolRequest,
        before_images: &[FileBeforeImage],
        complete: bool,
    ) -> Result<(), AgentLoopError> {
        self.trace_sender.flush().await.map_err(|error| {
            AgentLoopError::Checkpoint(format!("trace recorder is unavailable: {error}"))
        })?;
        self.host
            .persist_checkpoint_before_side_effect(&self.task, request, before_images, complete)
            .await
            .map_err(|error| AgentLoopError::Checkpoint(error.to_string()))
    }
}

impl RuntimeHost {
    /// Shut down runtime-owned managed processes and wait for each supervisor
    /// to persist its terminal state before the host is torn down.
    pub async fn close(&self) -> Result<(), ClientError> {
        self.execution.shutdown.cancel();
        let work_result = self.shutdown_active_work().await;
        let process_result = self
            .execution
            .process_supervisor
            .shutdown_and_wait()
            .await
            .map_err(|error| ClientError::TaskExecution(error.to_string()));
        match (work_result, process_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(work), Ok(())) | (Ok(()), Err(work)) => Err(work),
            (Err(work), Err(process)) => Err(ClientError::TaskExecution(format!(
                "{work}; additionally, {process}"
            ))),
        }
    }

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
            storage: RuntimeHostStorageState {
                store,
                repositories,
                memory_store,
                evaluation_store,
                evolution_store,
                governance,
                deep_evaluation_inputs: Mutex::new(HashMap::new()),
                checkpoint_evaluation_tasks,
                _temporary_root: temporary_root,
            },
            execution: RuntimeHostExecutionState {
                shutdown: CancellationToken::new(),
                post_task_worker: StdMutex::new(None),
                lane_manager: Mutex::new(RuntimeLaneManager::new()),
                event_bus,
                live_subscriptions: StdMutex::new(Vec::new()),
                next_sequence_no: AtomicU64::new(next_sequence_no),
                event_writer: Mutex::new(()),
                causal_ledger: Mutex::new(causal_recorder::CausalLedger::default()),
                command_mutex: Mutex::new(()),
                task_controls: Mutex::new(HashMap::new()),
                delegation_admissions: Mutex::new(HashMap::new()),
                delegation_operations: Mutex::new(HashMap::new()),
                provider_auth_waiters: Mutex::new(HashMap::new()),
                process_supervisor: ProcessSupervisor::new(),
                workspace_change_tracker: Mutex::new(
                    change_tracker::WorkspaceChangeTracker::default(),
                ),
                rollout_projection_failures: Mutex::new(HashMap::new()),
            },
            workspace_id,
            workspace_root,
            runtime_paths,
            provider_config_paths,
            default_session_id,
            default_thread_id,
            instance_id: Uuid::now_v7().to_string(),
            started_at: chrono::Utc::now(),
            force_mock_provider,
            execution_options,
        });
        host.storage
            .repositories
            .jobs
            .recover_expired(&host.workspace_id.to_string(), chrono::Utc::now())
            .await?;
        let post_task_worker = post_task::PostTaskCoordinator::start(&host);
        *host
            .execution
            .post_task_worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(post_task_worker);
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
            .storage
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
    pub fn subscribe_live(&self, filter: EventFilter) -> broadcast::Receiver<RuntimeEvent> {
        let (sender, receiver) = broadcast::channel(512);
        let mut subscriptions = self
            .execution
            .live_subscriptions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A raw broadcast receiver cannot notify this registry from Drop. Prune
        // abandoned receivers whenever a new one is registered and keep the
        // registry bounded even when the runtime is otherwise idle.
        subscriptions.retain(|subscription| subscription.sender.receiver_count() > 0);
        if subscriptions.len() >= MAX_LIVE_SUBSCRIPTIONS {
            subscriptions.remove(0);
        }
        subscriptions.push(LiveSubscription { filter, sender });
        receiver
    }

    async fn event_stream(
        self: Arc<Self>,
        filter: EventFilter,
    ) -> Result<RuntimeEventStream, ClientError> {
        self.ensure_session_in_workspace(filter.session_id).await?;
        let mut live = self.execution.event_bus.subscribe();
        let (sender, receiver) = mpsc::channel(256);
        let shutdown = self.execution.shutdown.clone();
        tokio::spawn(async move {
            let mut cursor = filter.after_sequence_no;
            if sender.is_closed() || shutdown.is_cancelled() {
                return;
            }
            match self.send_replay_pages(&filter, &mut cursor, &sender).await {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                    return;
                }
            }
            loop {
                let received = tokio::select! {
                    _ = sender.closed() => return,
                    _ = shutdown.cancelled() => return,
                    received = live.recv() => received,
                };
                match received {
                    Ok(event) if event_matches_filter(&event, &filter, cursor) => {
                        cursor = Some(event.sequence_no);
                        if sender.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if sender.is_closed() || shutdown.is_cancelled() {
                            return;
                        }
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
                .storage
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
            .storage
            .repositories
            .threads
            .list(Some(&workspace_root), u32::MAX)
            .await?;
        let mut recovered = 0;
        for thread in threads {
            let state = self
                .storage
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
            .storage
            .repositories
            .events
            .load(session_id, Some(task_id), None)
            .await?;
        let transferred = recoverable_transfer_turns(self, session_id, &events).await?;
        let transferred_payloads = transferred
            .iter()
            .map(|(turn_id, turn)| (*turn_id, turn.payload.clone()))
            .collect::<HashMap<_, _>>();
        let active_surface = recovered_active_turn_surface(&events, &transferred_payloads)?;
        let continuation = recovered_task_continuation(&events, chrono::Utc::now())?;
        let mut pending = transferred;
        for event in events {
            match event.event_type {
                RuntimeEventType::TurnQueued => {
                    if !is_pending_transfer_event(&event)
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
        if let Some(first) = pending.first_mut() {
            first.continuation = continuation;
        }
        if pending.first().is_some_and(|turn| turn.pending.steer)
            && !recovered_steer_has_materialized_surface(&pending[0].payload)
        {
            let active_surface = active_surface.as_ref().ok_or_else(|| {
                ClientError::TaskExecution(
                    "cannot recover a leading steering turn without its active task payload"
                        .to_owned(),
                )
            })?;
            materialize_recovered_leading_steer(
                active_surface,
                chrono::Utc::now(),
                &mut pending[0].payload,
            )?;
        }
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
        let continuation = first.continuation.clone().unwrap_or_default();
        let task_id = TaskId::new();
        let recovered_pending_payloads = std::iter::once(&first)
            .chain(pending_turns.iter())
            .map(|turn| {
                json!({
                    "sequence_no": turn.sequence_no,
                    "turn_id": turn.pending.turn_id,
                    "command_id": turn.pending.command_id,
                    "actor": turn.actor,
                    "payload": turn.payload,
                })
            })
            .collect::<Vec<_>>();
        let mut transfer_payload = json!({
            "summary": "durable pending turns transferred to a recovery task",
            "recovery": "durable_pending_turn_batch",
            "recovered_pending_sequence_nos": std::iter::once(&first)
                .chain(pending_turns.iter())
                .map(|turn| turn.sequence_no)
                .collect::<Vec<_>>(),
        });
        transfer_payload[RECOVERED_PENDING_TURNS_KEY] = json!(recovered_pending_payloads);
        transfer_payload[RECOVERY_CONTINUATION_KEY] = serde_json::to_value(&continuation)?;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            Some(task_id),
            RuntimeEventType::TurnQueued,
            RuntimeEventSource::Runtime,
            transfer_payload,
        ))
        .await?;
        let mut lane_manager = self.execution.lane_manager.lock().await;
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
        transition.event.payload["payload"] = first.payload.clone();
        self.record_event(transition.event).await?;
        let delegation_seed = continuation
            .delegation
            .clone()
            .map(DelegationContextSeed::Recovered);
        let spawn = Box::pin(
            self.spawn_agent_task(
                HostedAgentTask {
                    session_id,
                    task_id,
                    turn_id: first.pending.turn_id,
                    payload: first.payload,
                },
                session_lease,
                pending_turns
                    .into_iter()
                    .map(|turn| ConfiguredPendingAgentTurn {
                        turn: turn.pending,
                        execution: turn.execution,
                    })
                    .collect(),
                delegation_seed,
                continuation.governor_usage,
            ),
        );
        spawn.await
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
            .storage
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
        let _writer = self.execution.event_writer.lock().await;
        let causal_before = self.execution.causal_ledger.lock().await.clone();
        let event = match self.prepare_canonical_event(event).await {
            Ok(event) => event,
            Err(error) => {
                *self.execution.causal_ledger.lock().await = causal_before;
                return Err(error);
            }
        };
        let event = match self
            .storage
            .repositories
            .events
            .append_assigning_sequence(event)
            .await
        {
            Ok(event) => event,
            Err(error) => {
                *self.execution.causal_ledger.lock().await = causal_before;
                return Err(error.into());
            }
        };
        self.publish_committed_event(event).await
    }

    async fn record_tool_completed_bundle(
        &self,
        event: RuntimeEvent,
        artifacts: &[(ArtifactRecord, Vec<u8>)],
        evidence: &[golutra_core::EvidenceRecord],
    ) -> Result<(), ClientError> {
        let _writer = self.execution.event_writer.lock().await;
        let causal_before = self.execution.causal_ledger.lock().await.clone();
        let event = match self.prepare_canonical_event(event).await {
            Ok(event) => event,
            Err(error) => {
                *self.execution.causal_ledger.lock().await = causal_before;
                return Err(error);
            }
        };
        let event = match self
            .storage
            .repositories
            .events
            .append_tool_completed_bundle(event, artifacts, evidence)
            .await
        {
            Ok(event) => event,
            Err(error) => {
                *self.execution.causal_ledger.lock().await = causal_before;
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
        let _writer = self.execution.event_writer.lock().await;
        let causal_before = self.execution.causal_ledger.lock().await.clone();
        let receipt_event = match self.prepare_canonical_event(receipt_event).await {
            Ok(event) => event,
            Err(error) => {
                *self.execution.causal_ledger.lock().await = causal_before;
                return Err(error);
            }
        };
        let claim = match self
            .storage
            .repositories
            .events
            .claim_command(idempotency_key, command_id, provisional_ack, receipt_event)
            .await
        {
            Ok(claim) => claim,
            Err(error) => {
                *self.execution.causal_ledger.lock().await = causal_before;
                return Err(error.into());
            }
        };
        if let CommandClaim::Claimed {
            receipt_event: Some(event),
        } = &claim
        {
            self.publish_committed_event(event.clone()).await?;
        } else {
            *self.execution.causal_ledger.lock().await = causal_before;
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
        let _writer = self.execution.event_writer.lock().await;
        let causal_before = self.execution.causal_ledger.lock().await.clone();
        let completion_event = match self.prepare_canonical_event(completion_event).await {
            Ok(event) => event,
            Err(error) => {
                *self.execution.causal_ledger.lock().await = causal_before;
                return Err(error);
            }
        };
        let event = match self
            .storage
            .repositories
            .events
            .complete_command(idempotency_key, command_id, ack, completion_event)
            .await
        {
            Ok(event) => event,
            Err(error) => {
                *self.execution.causal_ledger.lock().await = causal_before;
                return Err(error.into());
            }
        };
        self.publish_committed_event(event).await
    }

    async fn publish_committed_event(&self, event: RuntimeEvent) -> Result<(), ClientError> {
        self.publish_live_event(event.clone());
        match self.append_rollout_event(&event).await {
            Ok(()) => {
                self.execution
                    .rollout_projection_failures
                    .lock()
                    .await
                    .remove(&event.session_id);
            }
            Err(append_error) => {
                let repair_result = match self
                    .storage
                    .repositories
                    .threads
                    .by_session(event.session_id)
                    .await
                {
                    Ok(Some(thread)) => self.rebuild_thread_rollout(&thread).await.map(|_| ()),
                    Ok(None) => Ok(()),
                    Err(error) => Err(error.into()),
                };
                let mut failures = self.execution.rollout_projection_failures.lock().await;
                match repair_result {
                    Ok(()) => {
                        failures.remove(&event.session_id);
                    }
                    Err(repair_error) => {
                        let detail =
                            format!("{append_error}; rollout rebuild failed: {repair_error}");
                        failures.insert(event.session_id, detail);
                    }
                }
            }
        }
        Ok(())
    }

    fn publish_live_event(&self, event: RuntimeEvent) {
        let _ = self.execution.event_bus.send(event.clone());
        let mut subscriptions = self
            .execution
            .live_subscriptions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        subscriptions.retain(|subscription| {
            if subscription.sender.receiver_count() == 0 {
                return false;
            }
            if !event_matches_filter(
                &event,
                &subscription.filter,
                subscription.filter.after_sequence_no,
            ) {
                return true;
            }
            subscription.sender.send(event.clone()).is_ok()
        });
    }

    async fn run_storage_maintenance(&self) -> Result<StorageMaintenanceReport, ClientError> {
        let now = chrono::Utc::now();
        let artifact_report = self.storage.store.run_artifact_maintenance(now).await?;
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
        let mut stats = self.storage.store.storage_stats().await?;
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
        let Some(paths) = self.runtime_paths.clone() else {
            return Ok(());
        };
        let rollout_directory = paths.rollouts_dir;
        let projections =
            run_blocking(move || rollout_projection_files(&rollout_directory)).await??;
        for (thread_id, path) in projections {
            if self
                .storage
                .repositories
                .threads
                .by_id(thread_id)
                .await?
                .is_none()
            {
                run_blocking(move || remove_rollout_projection(&path)).await??;
            }
        }
        let threads = self
            .storage
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
            self.storage.repositories.threads.upsert(thread).await?;
        }
        Ok(())
    }

    async fn append_rollout_event(&self, event: &RuntimeEvent) -> Result<(), ClientError> {
        let Some(mut thread) = self
            .storage
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
            .storage
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
            .storage
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
            .execution
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
        if let Some(thread) = self
            .storage
            .repositories
            .threads
            .by_session(session_id)
            .await?
        {
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
        if let Some(thread) = self
            .storage
            .repositories
            .threads
            .by_session(session_id)
            .await?
        {
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
            .storage
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
