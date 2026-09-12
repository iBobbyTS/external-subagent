use crate::{
    agent_status::{
        AgentEvidenceStore, AgentModelsInput, AgentModelsOutput, AgentProbeEvidence,
        AgentProbeInput, EvidenceState, ProbeScope, ScopeEvidence,
    },
    observation::{ObservationCoverage, ObservedReasoning, ObservedTool, OBSERVATION_SCHEMA},
    MessageDisposition, PassiveActivitySnapshot, PassiveActivityWindow, PassiveToolKind,
    ResponseDisposition, Scheduler, SchedulerError,
};
use external_core::{canonical_general_repository, GeneralTaskManifest, PreparedGeneralTask};
use external_store::{
    PendingRequestState, Store, StoreError, StoredPendingRequest, StoredTaskResult, TaskOutcome,
    TaskPageFilter, TaskPhase, TaskQueryScope, TaskRecord, TaskSubmissionDisposition,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const RPC_VERSION: u16 = 13;
pub const MAX_REQUEST_FRAME_BYTES: usize = 512 * 1024;
pub const MAX_RESPONSE_FRAME_BYTES: usize = 2 * 1024 * 1024;
const MAX_REQUEST_ID_BYTES: usize = 128;
pub const MAX_LIST_TASKS: usize = 100;
pub const MAX_PENDING_REQUESTS: usize = 100;
/// A result page is capped below the transport frame cap so that even the
/// worst-case JSON escaping (one input byte becoming a six-byte `\\u00XX`
/// escape), the response envelope, and the trailing newline fit in one frame.
pub const MAX_RESULT_CHUNK_BYTES: usize = 256 * 1024;
pub const MAX_WAIT: Duration = Duration::from_secs(299);
pub const DEFAULT_WAIT_TIME: u64 = 290;
pub const RPC_TRANSPORT_SUPPORTED: bool = cfg!(unix);

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::{remove_matching_socket, remove_stale_socket, SocketIdentity};
#[cfg(unix)]
pub use unix::{RpcClient, RpcServer, ServerOptions};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RpcRequest {
    pub version: u16,
    pub request_id: String,
    #[serde(flatten)]
    pub method: RpcMethod,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "method",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
#[allow(clippy::large_enum_variant)]
pub enum RpcMethod {
    SystemStatus,
    DaemonBeginDrain {
        #[serde(default)]
        cancel_active: bool,
    },
    DaemonDrainStatus,
    DaemonActivateReady,
    AgentProbe {
        input: AgentProbeInput,
    },
    AgentModels {
        input: AgentModelsInput,
    },
    SubmitGeneral {
        input: GeneralSubmitInput,
    },
    TaskList(TaskListQuery),
    TaskWait(TaskWaitQuery),
    TaskMessage(MessageInput),
    TaskRespond(RespondInput),
    TaskCancel {
        agent_id: String,
    },
    TaskResult {
        agent_id: String,
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_result_limit")]
        limit: usize,
    },
    TaskClose {
        agent_id: String,
    },
    TaskObserve {
        agent_id: String,
    },
}

impl RpcMethod {
    fn is_known(name: &str) -> bool {
        matches!(
            name,
            "system_status"
                | "daemon_begin_drain"
                | "daemon_drain_status"
                | "daemon_activate_ready"
                | "agent_probe"
                | "agent_models"
                | "submit_general"
                | "task_list"
                | "task_wait"
                | "task_message"
                | "task_respond"
                | "task_cancel"
                | "task_result"
                | "task_close"
                | "task_observe"
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskListQuery {
    #[serde(
        default,
        deserialize_with = "optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub agent: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub phase: Option<TaskPhaseFilter>,
    #[serde(default)]
    pub outcome: Option<TaskOutcome>,
    #[serde(default)]
    pub cursor: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskPhaseFilter {
    Queued,
    Preparing,
    Running,
    WaitingInput,
    Cancelling,
    Terminal,
}

impl From<TaskPhaseFilter> for TaskPhase {
    fn from(value: TaskPhaseFilter) -> Self {
        match value {
            TaskPhaseFilter::Queued => Self::Queued,
            TaskPhaseFilter::Preparing => Self::Preparing,
            TaskPhaseFilter::Running => Self::Running,
            TaskPhaseFilter::WaitingInput => Self::WaitingInput,
            TaskPhaseFilter::Cancelling => Self::Cancelling,
            TaskPhaseFilter::Terminal => Self::Terminal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneralSubmitInput {
    #[serde(
        default,
        deserialize_with = "optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub agent: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub model: Option<String>,
    pub manifest: GeneralTaskManifest,
}

fn optional_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskWaitQuery {
    pub agent_id: String,
    #[serde(default)]
    pub after_revision: u64,
    #[serde(default = "default_wait_time")]
    pub wait_time: u64,
    #[serde(default)]
    pub message_id: Option<String>,
}

fn default_wait_time() -> u64 {
    DEFAULT_WAIT_TIME
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageInput {
    pub agent_id: String,
    pub message_id: String,
    pub mode: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RespondInput {
    pub agent_id: String,
    pub request_id: String,
    pub decision: ResponseDecision,
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseDecision {
    Allow,
    Deny,
    Answer,
}

impl ResponseDecision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Answer => "answer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcResponse {
    pub version: u16,
    pub request_id: Option<String>,
    #[serde(flatten)]
    pub outcome: RpcOutcome,
}

impl RpcResponse {
    pub fn success(request_id: String, result: RpcSuccess) -> Self {
        Self {
            version: RPC_VERSION,
            request_id: Some(request_id),
            outcome: RpcOutcome::Success {
                result: Box::new(result),
            },
        }
    }

    pub fn error(request_id: Option<String>, error: RpcError) -> Self {
        Self {
            version: RPC_VERSION,
            request_id,
            outcome: RpcOutcome::Error { error },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RpcOutcome {
    Success { result: Box<RpcSuccess> },
    Error { error: RpcError },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RpcSuccess {
    SystemStatus {
        status: SystemStatusView,
    },
    DaemonDrainStatus {
        is_draining: bool,
        active_count: usize,
        resources_reaped: bool,
        ready_for_activation: bool,
        updater_fired: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        activation_claim: Option<String>,
    },
    AgentProbed {
        evidence: AgentProbeEvidence,
        status: AgentStatusView,
    },
    AgentModels {
        catalog: AgentModelsOutput,
    },
    GeneralSubmitted {
        task: TaskView,
        disposition: SubmissionDispositionView,
    },
    TaskListed {
        tasks: Vec<TaskView>,
        next_cursor: Option<String>,
    },
    TaskWait {
        task: TaskView,
        revision: u64,
        next_revision: u64,
        pending_requests: Vec<PendingRequestView>,
        command_pending_approval: bool,
        result_available: bool,
        activity: TaskActivityView,
        latest_progress: Option<String>,
        result: Option<TaskResultView>,
        instruction: Option<String>,
        timed_out: bool,
        message_receipt: Option<MessageReceiptView>,
    },
    TaskResult {
        task: TaskView,
        result: Option<TaskResultView>,
    },
    Message {
        message_id: String,
        disposition: MessageDispositionView,
        task: TaskView,
    },
    Respond {
        outcome: ResponseOutcomeView,
        task: TaskView,
    },
    Stopped {
        task: TaskView,
    },
    Closed {
        task: TaskView,
    },
    TaskObserved {
        observation: TaskObservationView,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageReceiptView {
    pub message_id: String,
    pub state: String,
    pub target_turn_id: Option<String>,
    pub failure_code: Option<String>,
    pub created_at_ms: i64,
    pub delivered_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ComponentStateView {
    Ready,
    Degraded,
    Unavailable,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityMaturityView {
    BetaReady,
    ExperimentalUnverifiedRuntime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionDispositionView {
    Created,
    Existing,
}

impl From<TaskSubmissionDisposition> for SubmissionDispositionView {
    fn from(value: TaskSubmissionDisposition) -> Self {
        match value {
            TaskSubmissionDisposition::Created => Self::Created,
            TaskSubmissionDisposition::Existing => Self::Existing,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemStatusView {
    pub api_surface: String,
    pub protocol_version: u16,
    pub service_generation: String,
    pub components: BTreeMap<String, ComponentStateView>,
    pub capabilities: AgentCapabilitiesView,
    #[serde(default)]
    pub agents: Vec<AgentStatusView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<DaemonIdentityView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatusView {
    pub agent: String,
    pub config_revision: u64,
    pub configured: bool,
    pub enabled: bool,
    pub spawn_supported: bool,
    pub transport_support: AgentTransportSupportView,
    pub permission_modes: Vec<AgentPermissionModeView>,
    pub model_selection: AgentModelSelectionCapabilityView,
    pub local: AgentScopeStatusView,
    pub auth: AgentScopeStatusView,
    pub hi: AgentScopeStatusView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTransportView {
    ZcodeAppServer,
    DshAcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTransportSupportView {
    pub transport: AgentTransportView,
    pub probe: bool,
    pub spawn: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPermissionModeView {
    Build,
    Edit,
    Plan,
    Yolo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentModelSelectionModeView {
    NativeOnly,
    CatalogToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentModelSelectionCapabilityView {
    pub supported: bool,
    pub mode: AgentModelSelectionModeView,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentConfigSnapshot {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    default_agent: Option<String>,
    #[serde(default)]
    agents: BTreeMap<String, AgentConfigEntry>,
    #[serde(default)]
    runtime: Option<String>,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    socket: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentConfigEntry {
    enabled: bool,
    spawn_supported: bool,
    #[serde(default)]
    default_model: Option<String>,
}

impl Default for AgentConfigSnapshot {
    fn default() -> Self {
        Self {
            schema_version: 1,
            revision: 0,
            default_agent: None,
            agents: BTreeMap::from([
                (
                    "zcode".into(),
                    AgentConfigEntry {
                        enabled: true,
                        spawn_supported: true,
                        default_model: None,
                    },
                ),
                (
                    "dsh".into(),
                    AgentConfigEntry {
                        enabled: false,
                        spawn_supported: false,
                        default_model: None,
                    },
                ),
            ]),
            runtime: None,
            database: None,
            socket: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentScopeStatusView {
    pub state: ComponentStateView,
    pub scope: ProbeScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonIdentityView {
    pub daemon: ComponentIdentityView,
    pub runtime: RuntimeIdentityView,
    pub models: ModelIdentityView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentIdentityView {
    pub component: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_dirty: Option<bool>,
    pub artifact: ArtifactIdentityView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactIdentityView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub source: String,
    pub captured_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeIdentityView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configured_path: Option<String>,
    pub configured_path_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_version: Option<String>,
    pub observed_version_source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelIdentityView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configured: Option<ModelIdentityFactView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_response: Option<ModelIdentityFactView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelIdentityFactView {
    pub value: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCapabilitiesView {
    pub max_rpc_request_frame_bytes: usize,
    pub max_rpc_response_frame_bytes: usize,
    pub max_wait_ms: u64,
    pub maturity: BTreeMap<String, CapabilityMaturityView>,
    pub observation: ObservationCapabilityView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationCapabilityView {
    pub protocol: String,
    pub public_reasoning_default: bool,
    pub runtime_source_verified: bool,
    pub defaults: ObservationDefaultsView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationDefaultsView {
    pub top_tools: usize,
    pub recent_calls_per_tool: usize,
    pub reasoning_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskObservationView {
    pub schema: String,
    pub agent_id: String,
    pub service_generation: String,
    pub snapshot_seq: u64,
    pub count_scope: String,
    pub tools: Vec<ObservedTool>,
    pub reasoning: ObservedReasoning,
    pub coverage: ObservationCoverage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    pub agent_id: String,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub phase: String,
    pub outcome: Option<TaskOutcome>,
    pub reason_code: Option<String>,
    pub stop_requested: bool,
    pub close_requested: bool,
    pub closed: bool,
    pub reaped: bool,
    pub input_identity: InputIdentityView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputIdentityView {
    #[serde(default)]
    pub admission: Option<external_core::AdmissionIdentity>,
    pub workspace_path: Option<String>,
    pub permission_mode: Option<String>,
    pub caller_prompt_sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskActivityStateView {
    Queued,
    Preparing,
    Active,
    WaitingInput,
    Cancelling,
    Idle,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryStatusView {
    Healthy,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityToolKindView {
    Read,
    Bash,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveToolView {
    pub tool_call_id: String,
    pub kind: ActivityToolKindView,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityWindowView {
    pub reasoning_delta_events: u64,
    pub reasoning_delta_bytes: u64,
    pub text_delta_events: u64,
    pub text_delta_bytes: u64,
    pub tool_calls_started: u64,
    pub tool_calls_completed: u64,
    pub tool_calls_failed: u64,
    pub read_calls: u64,
    pub bash_calls: u64,
    pub other_tool_calls: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskActivityView {
    pub state: TaskActivityStateView,
    pub last_runtime_event_at: Option<u64>,
    pub last_activity_age_ms: Option<u64>,
    pub model_request_active: bool,
    pub model_request_age_ms: Option<u64>,
    pub model_last_delta_age_ms: Option<u64>,
    pub latest_text_tail: String,
    pub latest_text_updated_at: Option<u64>,
    pub latest_text_truncated: bool,
    pub latest_progress: Option<String>,
    pub active_tools: Vec<ActiveToolView>,
    pub window_60s: ActivityWindowView,
    pub telemetry_status: TelemetryStatusView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskResultView {
    pub outcome: TaskOutcome,
    pub final_text: String,
    pub partial: bool,
    pub result_sha256: String,
    pub offset: usize,
    pub total_bytes: usize,
    pub next_offset: Option<usize>,
    pub complete: bool,
}

fn default_result_limit() -> usize {
    MAX_RESULT_CHUNK_BYTES
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcErrorCode {
    Malformed,
    Oversized,
    UnsupportedVersion,
    UnknownMethod,
    Validation,
    AgentRequired,
    AgentUnknown,
    AgentDisabled,
    AgentUnsupported,
    ModelSelectionUnsupported,
    NotFound,
    Conflict,
    Persistence,
    Timeout,
    RuntimeLost,
    ResultInvalid,
    Unavailable,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: RpcErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_agent_id: Option<String>,
}

impl RpcError {
    pub fn new(code: RpcErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        message.truncate(512);
        Self {
            code,
            message,
            active_agent_id: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageDispositionView {
    Queued,
    Delivered,
    AlreadyDelivered,
    Failed,
}

impl From<MessageDisposition> for MessageDispositionView {
    fn from(value: MessageDisposition) -> Self {
        match value {
            MessageDisposition::Queued => Self::Queued,
            MessageDisposition::Delivered => Self::Delivered,
            MessageDisposition::AlreadyDelivered => Self::AlreadyDelivered,
            MessageDisposition::Failed => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseDispositionView {
    Responded,
    AlreadyResponded,
    InFlight,
}

impl From<ResponseDisposition> for ResponseDispositionView {
    fn from(value: ResponseDisposition) -> Self {
        match value {
            ResponseDisposition::Responded => Self::Responded,
            ResponseDisposition::AlreadyResponded => Self::AlreadyResponded,
            ResponseDisposition::InFlight => Self::InFlight,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseOutcomeView {
    pub disposition: ResponseDispositionView,
    pub requested_decision: String,
    pub effective_decision: String,
    pub policy_overrode: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_reason_code: Option<String>,
}

impl From<crate::ResponseOutcome> for ResponseOutcomeView {
    fn from(value: crate::ResponseOutcome) -> Self {
        Self {
            disposition: value.disposition.into(),
            requested_decision: value.requested_decision,
            effective_decision: value.effective_decision,
            policy_overrode: value.policy_overrode,
            policy_reason_code: value.policy_reason_code,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingRequestStateView {
    Pending,
    Sending,
    Responded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRequestView {
    pub request_id: String,
    pub kind: String,
    pub state: PendingRequestStateView,
    pub respondable: bool,
    pub tool_name: Option<String>,
    pub operation: String,
    pub summary: String,
    pub policy_preview: String,
}

#[derive(Clone)]
pub struct RpcService {
    scheduler: Scheduler,
    store: Arc<Store>,
    service_generation: String,
    daemon_identity: ComponentIdentityView,
    agent_evidence: AgentEvidenceStore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcServiceConfigError {
    MismatchedStore,
    GenerationUnavailable,
}

impl RpcService {
    #[cfg(test)]
    pub(crate) fn store_for_wait_test(&self) -> Arc<Store> {
        self.store.clone()
    }

    pub fn new(scheduler: Scheduler, store: Arc<Store>) -> Result<Self, RpcServiceConfigError> {
        let service_generation = opaque_generation()?;
        Self::new_with_service_generation(scheduler, store, service_generation)
    }

    pub(crate) fn new_with_service_generation(
        scheduler: Scheduler,
        store: Arc<Store>,
        service_generation: String,
    ) -> Result<Self, RpcServiceConfigError> {
        let evidence = AgentEvidenceStore::new(scheduler.configured_runtime_source());
        Self::new_with_service_generation_and_evidence(
            scheduler,
            store,
            service_generation,
            evidence,
        )
    }

    fn new_with_service_generation_and_evidence(
        scheduler: Scheduler,
        store: Arc<Store>,
        service_generation: String,
        agent_evidence: AgentEvidenceStore,
    ) -> Result<Self, RpcServiceConfigError> {
        if !Arc::ptr_eq(&scheduler.store(), &store) {
            return Err(RpcServiceConfigError::MismatchedStore);
        }
        Ok(Self {
            agent_evidence,
            scheduler,
            store,
            service_generation,
            daemon_identity: running_component_identity("daemon", env!("CARGO_PKG_VERSION")),
        })
    }

    pub fn handle_bytes(&self, frame: &[u8]) -> RpcResponse {
        self.handle_bytes_interruptible(frame, &|| false)
    }

    pub(crate) fn handle_bytes_interruptible(
        &self,
        frame: &[u8],
        interrupted: &dyn Fn() -> bool,
    ) -> RpcResponse {
        if frame.len().saturating_add(1) > MAX_REQUEST_FRAME_BYTES {
            return RpcResponse::error(
                None,
                RpcError::new(RpcErrorCode::Oversized, "request frame exceeds the RPC cap"),
            );
        }
        let mut value = match serde_json::from_slice::<Value>(frame) {
            Ok(value) => value,
            Err(_) => {
                return RpcResponse::error(
                    None,
                    RpcError::new(RpcErrorCode::Malformed, "request is not valid JSON"),
                )
            }
        };
        let request_id = value
            .get("request_id")
            .and_then(Value::as_str)
            .filter(|request_id| valid_request_id(request_id))
            .map(str::to_owned);
        let version = value.get("version").and_then(Value::as_u64);
        if version != Some(u64::from(RPC_VERSION)) {
            return RpcResponse::error(
                request_id,
                RpcError::new(
                    RpcErrorCode::UnsupportedVersion,
                    "unsupported RPC protocol version",
                ),
            );
        }
        if value.as_object().is_none_or(|object| {
            object
                .keys()
                .any(|key| !matches!(key.as_str(), "version" | "request_id" | "method" | "params"))
        }) {
            return RpcResponse::error(
                request_id,
                RpcError::new(RpcErrorCode::Validation, "request fields are invalid"),
            );
        }
        let method = value.get("method").and_then(Value::as_str);
        if let Some(method) = method {
            if !RpcMethod::is_known(method) {
                return RpcResponse::error(
                    request_id,
                    RpcError::new(RpcErrorCode::UnknownMethod, "unknown RPC method"),
                );
            }
        }
        // Legacy drain requests omitted params; absence still means passive
        // draining. Explicit malformed params continue to fail validation.
        if method == Some("daemon_begin_drain") && value.get("params").is_none() {
            value["params"] = serde_json::json!({});
        }
        let request = match serde_json::from_value::<RpcRequest>(value) {
            Ok(request) => request,
            Err(_) => {
                return RpcResponse::error(
                    request_id,
                    RpcError::new(RpcErrorCode::Validation, "request fields are invalid"),
                )
            }
        };
        if !valid_request_id(&request.request_id) {
            return RpcResponse::error(
                None,
                RpcError::new(RpcErrorCode::Validation, "request_id is invalid"),
            );
        }
        let request_id = request.request_id;
        match self.dispatch_interruptible(request.method, interrupted) {
            Ok(result) => RpcResponse::success(request_id, result),
            Err(error) => RpcResponse::error(Some(request_id), error),
        }
    }

    pub fn dispatch(&self, method: RpcMethod) -> Result<RpcSuccess, RpcError> {
        self.dispatch_interruptible(method, &|| false)
    }

    fn dispatch_interruptible(
        &self,
        method: RpcMethod,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<RpcSuccess, RpcError> {
        match method {
            RpcMethod::SystemStatus => Ok(RpcSuccess::SystemStatus {
                status: self.system_status(),
            }),
            RpcMethod::DaemonBeginDrain { cancel_active } => {
                self.scheduler.begin_drain();
                if cancel_active {
                    self.scheduler.cancel_draining_tasks().map_err(map_scheduler)?;
                }
                Ok(RpcSuccess::DaemonDrainStatus {
                    is_draining: true,
                    active_count: self.scheduler.active_count(),
                    resources_reaped: self.scheduler.resources_reaped(),
                    ready_for_activation: self.scheduler.ready_for_activation(),
                    updater_fired: self.scheduler.updater_fired(),
                    activation_claim: None,
                })
            }
            RpcMethod::DaemonDrainStatus => Ok(RpcSuccess::DaemonDrainStatus {
                is_draining: self.scheduler.is_draining(),
                active_count: self.scheduler.active_count(),
                resources_reaped: self.scheduler.resources_reaped(),
                ready_for_activation: self.scheduler.ready_for_activation(),
                updater_fired: self.scheduler.updater_fired(),
                activation_claim: None,
            }),
            RpcMethod::DaemonActivateReady => {
                let claim = self.scheduler.claim_activation();
                Ok(RpcSuccess::DaemonDrainStatus {
                    is_draining: self.scheduler.is_draining(),
                    active_count: self.scheduler.active_count(),
                    resources_reaped: self.scheduler.resources_reaped(),
                    ready_for_activation: self.scheduler.ready_for_activation(),
                    updater_fired: self.scheduler.updater_fired(),
                    activation_claim: claim,
                })
            }
            RpcMethod::AgentProbe { input } => {
                validate_agent_probe_input(&input)?;
                let config = read_agent_config_snapshot()?;
                let evidence = self.agent_evidence.probe(&input, config.revision);
                let status = configured_agent_statuses(&config, &self.agent_evidence)
                    .into_iter()
                    .find(|status| status.agent == input.agent)
                    .ok_or_else(|| RpcError::new(RpcErrorCode::AgentUnknown, "agent is unknown"))?;
                Ok(RpcSuccess::AgentProbed { evidence, status })
            }
            RpcMethod::AgentModels { input } => {
                validate_agent_models_input(&input)?;
                let config = read_agent_config_snapshot()?;
                Ok(RpcSuccess::AgentModels {
                    catalog: self.agent_evidence.models(&input, config.revision),
                })
            }
            RpcMethod::SubmitGeneral { input } => {
                let config = read_agent_config_snapshot()?;
                let admission = resolve_admission(&input, &config)?;
                let manifest = input.manifest;
                let submitted = self
                    .scheduler
                    .enqueue_general_with_admission(&manifest, Some(admission))
                    .map_err(map_scheduler)?;
                Ok(RpcSuccess::GeneralSubmitted {
                    task: task_view(submitted.task),
                    disposition: submitted.disposition.into(),
                })
            }
            RpcMethod::TaskList(query) => {
                if let Some(agent) = query.agent.as_deref() {
                    if !matches!(agent, "zcode" | "dsh") {
                        return Err(RpcError::new(
                            RpcErrorCode::AgentUnknown,
                            "agent is unknown",
                        ));
                    }
                }
                if query.limit == 0 || query.limit > MAX_LIST_TASKS {
                    return Err(RpcError::new(
                        RpcErrorCode::Validation,
                        "task list limit is outside the allowed range",
                    ));
                }
                for (field, value, cap) in [("repository", query.repository.as_deref(), 4096usize)]
                {
                    if let Some(value) = value {
                        validate_text(value, field, cap)?;
                    }
                }
                if let Some(cursor) = query.cursor.as_deref() {
                    validate_text(cursor, "cursor", 64)?;
                }
                if query.repository.is_none() {
                    return Err(RpcError::new(
                        RpcErrorCode::Validation,
                        "at least one task list scope is required",
                    ));
                }
                let canonical_repository = query
                    .repository
                    .as_deref()
                    .map(|repository| canonical_general_repository(Path::new(repository)))
                    .transpose()
                    .map_err(|_| {
                        RpcError::new(RpcErrorCode::Validation, "repository scope is invalid")
                    })?
                    .map(|repository| repository.to_string_lossy().into_owned());
                let page = self
                    .store
                    .list_task_page(
                        TaskQueryScope {
                            repository: canonical_repository.as_deref(),
                        },
                        TaskPageFilter {
                            agent: query.agent.clone(),
                            phase: query.phase.map(Into::into),
                            outcome: query.outcome,
                        },
                        query.cursor.as_deref().map(parse_task_cursor).transpose()?,
                        query.limit,
                    )
                    .map_err(map_store)?;
                let mut views = Vec::with_capacity(page.tasks.len());
                for task in page.tasks {
                    views.push(task_view(task));
                }
                Ok(RpcSuccess::TaskListed {
                    tasks: views,
                    next_cursor: page.next_cursor.map(format_task_cursor),
                })
            }
            RpcMethod::TaskWait(query) => {
                if query.wait_time > MAX_WAIT.as_secs() {
                    return Err(RpcError::new(
                        RpcErrorCode::Validation,
                        "wait_time must be between 0 and 299 seconds",
                    ));
                }
                let deadline = Instant::now() + Duration::from_secs(query.wait_time);
                self.task_wait(query, deadline, interrupted)
            }
            RpcMethod::TaskMessage(input) => {
                let task = self.require_task(&input.agent_id)?;
                validate_id(&input.message_id, "message_id")?;
                // The generic control plane only queues clarification. A
                // terminal task may be resumed by the scheduler when the
                // persisted ZCode session accepts a restore; other
                // interrupt-and-continue paths remain private.
                if input.mode != "queue" {
                    return Err(RpcError::new(
                        RpcErrorCode::Validation,
                        "generic agent messages must use queue mode",
                    ));
                }
                validate_text(&input.content, "content", 16 * 1024)?;
                let disposition = self
                    .scheduler
                    .queue_message(
                        &task.agent_id,
                        &input.message_id,
                        &input.mode,
                        &input.content,
                    )
                    .map_err(map_scheduler)?;
                let task = self.require_task(&input.agent_id)?;
                Ok(RpcSuccess::Message {
                    message_id: input.message_id,
                    disposition: disposition.into(),
                    task: task_view(task),
                })
            }
            RpcMethod::TaskRespond(input) => {
                let task = self.require_task(&input.agent_id)?;
                validate_id(&input.request_id, "request_id")?;
                if let Some(content) = input.content.as_deref() {
                    validate_text(content, "response content", 16 * 1024)?;
                }
                let outcome = self
                    .scheduler
                    .respond_request(
                        &task.agent_id,
                        &input.request_id,
                        input.decision.as_str(),
                        input.content.as_deref(),
                    )
                    .map_err(map_scheduler)?;
                let task = self.require_task(&input.agent_id)?;
                Ok(RpcSuccess::Respond {
                    outcome: outcome.into(),
                    task: task_view(task),
                })
            }
            RpcMethod::TaskCancel { agent_id } => {
                let task = self.require_task(&agent_id)?;
                self.scheduler
                    .cancel_task(&task.agent_id)
                    .map_err(map_scheduler)?;
                let task = self.require_task(&agent_id)?;
                Ok(RpcSuccess::Stopped {
                    task: task_view(task),
                })
            }
            RpcMethod::TaskResult {
                agent_id,
                offset,
                limit,
            } => {
                let task = self.require_task(&agent_id)?;
                if limit == 0 || limit > MAX_RESULT_CHUNK_BYTES {
                    return Err(RpcError::new(
                        RpcErrorCode::Validation,
                        format!(
                            "result limit must be between 1 and {MAX_RESULT_CHUNK_BYTES} bytes; received {limit}"
                        ),
                    ));
                }
                let result = self
                    .store
                    .task_result(&task.agent_id)
                    .map_err(map_store)?
                    .map(|stored| self.task_result_view(stored, offset, limit))
                    .transpose()?;
                Ok(RpcSuccess::TaskResult {
                    task: task_view(task),
                    result,
                })
            }
            RpcMethod::TaskClose { agent_id } => {
                let task = self.require_task(&agent_id)?;
                self.scheduler
                    .close_task(&task.agent_id)
                    .map_err(map_scheduler)?;
                let task = self.require_task(&agent_id)?;
                Ok(RpcSuccess::Closed {
                    task: task_view(task),
                })
            }
            RpcMethod::TaskObserve { agent_id } => {
                let task = self.require_task(&agent_id)?;
                let (snapshot, runtime_source_verified) =
                    self.scheduler.observation_snapshot(&task.agent_id);
                if !runtime_source_verified {
                    return Err(RpcError::new(
                        RpcErrorCode::Unavailable,
                        "observation runtime source is not verified",
                    ));
                }
                Ok(RpcSuccess::TaskObserved {
                    observation: TaskObservationView {
                        schema: OBSERVATION_SCHEMA.into(),
                        agent_id: task.agent_id,
                        service_generation: self.service_generation.clone(),
                        snapshot_seq: snapshot.snapshot_seq,
                        count_scope: "agent_lifetime".into(),
                        tools: snapshot.tools,
                        reasoning: snapshot.reasoning,
                        coverage: snapshot.coverage,
                    },
                })
            }
        }
    }

    fn system_status(&self) -> SystemStatusView {
        let mut components = BTreeMap::new();
        components.insert("facade".into(), ComponentStateView::Unknown);
        components.insert("daemon".into(), ComponentStateView::Ready);
        components.insert(
            "store".into(),
            match self.store.journal_mode() {
                Ok(mode) if mode.eq_ignore_ascii_case("wal") => ComponentStateView::Ready,
                Ok(_) => ComponentStateView::Degraded,
                Err(_) => ComponentStateView::Unavailable,
            },
        );
        components.insert("scheduler".into(), ComponentStateView::Ready);
        components.insert("driver".into(), ComponentStateView::Unknown);
        components.insert("runtime".into(), ComponentStateView::Unknown);
        components.insert("model_auth".into(), ComponentStateView::Unknown);
        SystemStatusView {
            api_surface: "generic_agent".into(),
            protocol_version: RPC_VERSION,
            service_generation: self.service_generation.clone(),
            components,
            capabilities: agent_capabilities(self.scheduler.runtime_source_verified()),
            agents: read_agent_config_snapshot()
                .map(|config| configured_agent_statuses(&config, &self.agent_evidence))
                .unwrap_or_else(|_| unavailable_agent_statuses()),
            identity: Some(DaemonIdentityView {
                daemon: self.daemon_identity.clone(),
                runtime: configured_runtime_identity(self.scheduler.configured_runtime_source()),
                // Status has no Agent/session scope, and the current runtime
                // exposes no verified response-producer model identity.
                models: ModelIdentityView {
                    configured: None,
                    observed_response: None,
                },
            }),
        }
    }

    fn require_task(&self, agent_id: &str) -> Result<TaskRecord, RpcError> {
        validate_id(agent_id, "agent_id")?;
        let task = self
            .store
            .get_task(agent_id)
            .map_err(map_store)?
            .ok_or_else(|| RpcError::new(RpcErrorCode::NotFound, "task was not found"))?;
        let prepared = serde_json::from_str::<PreparedGeneralTask>(&task.prepared_launch_json)
            .map_err(|_| RpcError::new(RpcErrorCode::NotFound, "task was not found"))?;
        if prepared.repository.to_string_lossy() != task.repository {
            return Err(RpcError::new(RpcErrorCode::NotFound, "task was not found"));
        }
        Ok(task)
    }

    fn task_result_view(
        &self,
        stored: StoredTaskResult,
        offset: usize,
        limit: usize,
    ) -> Result<TaskResultView, RpcError> {
        let text = stored.result.final_text;
        let total_bytes = text.len();
        let (end, next_offset) = result_page_bounds(&text, offset, limit)?;
        Ok(TaskResultView {
            outcome: stored.result.outcome,
            final_text: text[offset..end].to_owned(),
            partial: stored.result.partial,
            result_sha256: stored.result_sha256,
            offset,
            total_bytes,
            next_offset,
            complete: next_offset.is_none(),
        })
    }

    fn task_wait(
        &self,
        query: TaskWaitQuery,
        deadline: Instant,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<RpcSuccess, RpcError> {
        loop {
            if interrupted() {
                return Err(RpcError::new(RpcErrorCode::Unavailable, "wait interrupted"));
            }
            let task = self.require_task(&query.agent_id)?;
            let message_receipt = if let Some(id) = &query.message_id {
                self.store.message(id).map_err(map_store)?.and_then(|m| {
                    (m.agent_id == query.agent_id).then(|| MessageReceiptView {
                        message_id: m.message_id,
                        state: format!("{:?}", m.state).to_lowercase(),
                        target_turn_id: m.target_turn_id,
                        failure_code: m.failure_code,
                        created_at_ms: m.created_at,
                        delivered_at_ms: m.delivered_at,
                    })
                })
            } else {
                None
            };
            let pending_requests = self
                .store
                .pending_requests_bounded(&task.agent_id, MAX_PENDING_REQUESTS)
                .map_err(map_store)?
                .into_iter()
                .map(pending_request_view)
                .collect::<Vec<_>>();
            let command_pending_approval = pending_requests.iter().any(respondable_pending_request);
            let stored_result = self.store.task_result(&task.agent_id).map_err(map_store)?;
            let result_available = stored_result.is_some();
            let activity = self.scheduler.passive_activity_snapshot(&task.agent_id);
            let revision = activity
                .as_ref()
                .map(|activity| activity.revision)
                .unwrap_or(0)
                .max(task.last_event_seq);
            let terminal = task.phase == TaskPhase::Terminal;
            let now = Instant::now();
            if command_pending_approval || terminal || now >= deadline {
                let timed_out = !command_pending_approval && !terminal && now >= deadline;
                let mut response = RpcSuccess::TaskWait {
                    activity: task_activity_view(task.phase, activity),
                    task: task_view(task.clone()),
                    revision,
                    next_revision: revision,
                    pending_requests,
                    command_pending_approval,
                    result_available,
                    latest_progress: self
                        .scheduler
                        .passive_activity_snapshot(&task.agent_id)
                        .and_then(|a| a.latest_progress),
                    result: stored_result.filter(|_| terminal).map(Into::into),
                    instruction: if result_available {
                        Some("Result now available".to_owned())
                    } else if !terminal {
                        Some("Not finished yet, call wait again; use observe only if latest_text_tail may indicate subagent runs into a meaningless loop".to_owned())
                    } else {
                        None
                    },
                    timed_out,
                    message_receipt,
                };
                bound_wait_result(&mut response)?;
                return Ok(response);
            }
            thread::sleep((deadline - now).min(Duration::from_millis(10)));
        }
    }
}

fn configured_agent_statuses(
    config: &AgentConfigSnapshot,
    evidence: &AgentEvidenceStore,
) -> Vec<AgentStatusView> {
    ["zcode", "dsh"]
        .into_iter()
        .map(|agent| {
            let entry = &config.agents[agent];
            let observed = evidence.latest(agent);
            AgentStatusView {
                agent: agent.into(),
                config_revision: config.revision,
                configured: true,
                enabled: entry.enabled,
                spawn_supported: effective_spawn_supported(agent, entry),
                transport_support: transport_support(agent, entry),
                permission_modes: permission_modes(agent, entry),
                model_selection: model_selection(agent, entry),
                local: current_scope(&observed, config.revision, |evidence| &evidence.local),
                auth: current_scope(&observed, config.revision, |evidence| &evidence.auth),
                hi: current_scope(&observed, config.revision, |evidence| &evidence.hi),
            }
        })
        .collect()
}

fn unavailable_agent_statuses() -> Vec<AgentStatusView> {
    ["zcode", "dsh"]
        .into_iter()
        .map(|agent| AgentStatusView {
            agent: agent.into(),
            config_revision: 0,
            configured: false,
            enabled: false,
            spawn_supported: false,
            transport_support: transport_support(
                agent,
                &AgentConfigEntry {
                    enabled: false,
                    spawn_supported: false,
                    default_model: None,
                },
            ),
            permission_modes: Vec::new(),
            model_selection: model_selection(
                agent,
                &AgentConfigEntry {
                    enabled: false,
                    spawn_supported: false,
                    default_model: None,
                },
            ),
            local: unprobed_scope(),
            auth: unprobed_scope(),
            hi: unprobed_scope(),
        })
        .collect()
}

fn effective_spawn_supported(agent: &str, entry: &AgentConfigEntry) -> bool {
    if agent != "dsh" {
        return entry.enabled && entry.spawn_supported;
    }
    entry.enabled
        && entry.spawn_supported
        && env::var_os("DSH_RUNTIME_PATH")
            .map(PathBuf::from)
            .is_some_and(|path| path.is_absolute() && fs::metadata(path).is_ok_and(|m| m.is_file()))
}

fn transport_support(agent: &str, entry: &AgentConfigEntry) -> AgentTransportSupportView {
    AgentTransportSupportView {
        transport: if agent == "zcode" {
            AgentTransportView::ZcodeAppServer
        } else {
            AgentTransportView::DshAcp
        },
        probe: true,
        spawn: effective_spawn_supported(agent, entry),
    }
}

fn permission_modes(agent: &str, entry: &AgentConfigEntry) -> Vec<AgentPermissionModeView> {
    if !effective_spawn_supported(agent, entry) {
        return Vec::new();
    }
    vec![
        AgentPermissionModeView::Build,
        AgentPermissionModeView::Edit,
        AgentPermissionModeView::Plan,
        AgentPermissionModeView::Yolo,
    ]
}

fn model_selection(agent: &str, entry: &AgentConfigEntry) -> AgentModelSelectionCapabilityView {
    if agent == "zcode" {
        AgentModelSelectionCapabilityView {
            supported: false,
            mode: AgentModelSelectionModeView::NativeOnly,
        }
    } else {
        AgentModelSelectionCapabilityView {
            supported: agent == "dsh" && effective_spawn_supported(agent, entry),
            mode: AgentModelSelectionModeView::CatalogToken,
        }
    }
}

fn current_scope(
    evidence: &Option<AgentProbeEvidence>,
    config_revision: u64,
    select: impl FnOnce(&AgentProbeEvidence) -> &ScopeEvidence,
) -> AgentScopeStatusView {
    match evidence {
        Some(evidence) if evidence.config_revision == config_revision => {
            scope_status_view(select(evidence))
        }
        Some(evidence) => stale_scope(select(evidence)),
        None => unprobed_scope(),
    }
}

fn stale_scope(value: &ScopeEvidence) -> AgentScopeStatusView {
    AgentScopeStatusView {
        state: ComponentStateView::Unknown,
        scope: value.scope.clone(),
        version: value.version.clone(),
        checked_at_ms: Some(value.checked_at_ms),
        reason: Some("stale_config_revision".into()),
    }
}

fn unprobed_scope() -> AgentScopeStatusView {
    AgentScopeStatusView {
        state: ComponentStateView::Unknown,
        scope: ProbeScope::default(),
        version: None,
        checked_at_ms: None,
        reason: Some("not_probed".into()),
    }
}

fn scope_status_view(value: &ScopeEvidence) -> AgentScopeStatusView {
    AgentScopeStatusView {
        state: match value.state {
            EvidenceState::Ready => ComponentStateView::Ready,
            EvidenceState::Degraded => ComponentStateView::Degraded,
            EvidenceState::Unavailable => ComponentStateView::Unavailable,
            EvidenceState::Unknown => ComponentStateView::Unknown,
        },
        scope: value.scope.clone(),
        version: value.version.clone(),
        checked_at_ms: Some(value.checked_at_ms),
        reason: value.reason.clone(),
    }
}

fn validate_agent_probe_input(input: &AgentProbeInput) -> Result<(), RpcError> {
    if !matches!(input.agent.as_str(), "zcode" | "dsh") {
        return Err(RpcError::new(
            RpcErrorCode::AgentUnknown,
            "agent is unknown",
        ));
    }
    for (name, value) in [
        ("workspace", input.scope.workspace.as_deref()),
        ("home", input.scope.home.as_deref()),
    ] {
        if let Some(value) = value {
            validate_text(value, name, 4096)?;
            if !Path::new(value).is_absolute() {
                return Err(RpcError::new(
                    RpcErrorCode::Validation,
                    format!("{name} must be absolute"),
                ));
            }
        }
    }
    Ok(())
}

fn validate_agent_models_input(input: &AgentModelsInput) -> Result<(), RpcError> {
    validate_agent_probe_input(&AgentProbeInput {
        agent: input.agent.clone(),
        through: crate::agent_status::ProbeLayer::Local,
        scope: input.scope.clone(),
    })
}

fn resolve_admission(
    input: &GeneralSubmitInput,
    config: &AgentConfigSnapshot,
) -> Result<external_core::AdmissionIdentity, RpcError> {
    for (field, value) in [
        ("agent", input.agent.as_deref()),
        ("model", input.model.as_deref()),
    ] {
        if let Some(value) = value {
            validate_text(value.trim(), field, 4096)?;
        }
    }
    let agent = input
        .agent
        .as_deref()
        .or(config.default_agent.as_deref())
        .ok_or_else(|| {
            RpcError::new(
                RpcErrorCode::AgentRequired,
                "agent is required when no default_agent is configured",
            )
        })?;
    let configured = config
        .agents
        .get(agent)
        .ok_or_else(|| RpcError::new(RpcErrorCode::AgentUnknown, "agent is unknown"))?;
    if !configured.enabled {
        return Err(RpcError::new(
            RpcErrorCode::AgentDisabled,
            "agent is disabled",
        ));
    }
    if agent == "zcode" && (input.model.is_some() || configured.default_model.is_some()) {
        return Err(RpcError::new(
            RpcErrorCode::ModelSelectionUnsupported,
            "model selection is unsupported for zcode; prompt_count=0",
        ));
    }
    if !effective_spawn_supported(agent, configured) {
        return Err(RpcError::new(
            RpcErrorCode::AgentUnsupported,
            format!("agent {agent} is unsupported; prompt_count=0"),
        ));
    }
    let (model, model_source) = if agent == "dsh" {
        (input.model.clone(), "catalog")
    } else {
        (None, "native")
    };
    Ok(external_core::AdmissionIdentity {
        agent: agent.to_owned(),
        config_revision: config.revision,
        adapter_version: env!("CARGO_PKG_VERSION").into(),
        model,
        model_source: model_source.into(),
    })
}

fn read_agent_config_snapshot() -> Result<AgentConfigSnapshot, RpcError> {
    let Some(path) =
        env::var_os("EXTERNAL_SUBAGENT_CONFIG").or_else(|| env::var_os("ZCODE_AGENT_CONFIG"))
    else {
        return Ok(AgentConfigSnapshot::default());
    };
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AgentConfigSnapshot::default())
        }
        Err(_) => {
            return Err(RpcError::new(
                RpcErrorCode::Validation,
                "agent config is unreadable",
            ))
        }
    };
    let mut value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| RpcError::new(RpcErrorCode::Validation, "agent config is invalid"))?;
    normalize_agent_config_value(&mut value)?;
    let mut snapshot: AgentConfigSnapshot = serde_json::from_value(value)
        .map_err(|_| RpcError::new(RpcErrorCode::Validation, "agent config is invalid"))?;
    if snapshot.schema_version != 0 && snapshot.schema_version != 1 {
        return Err(RpcError::new(
            RpcErrorCode::Validation,
            "unsupported agent config schema version",
        ));
    }
    if snapshot
        .agents
        .keys()
        .any(|agent| !matches!(agent.as_str(), "zcode" | "dsh"))
    {
        return Err(RpcError::new(
            RpcErrorCode::Validation,
            "agent config contains an unknown agent",
        ));
    }
    let defaults = AgentConfigSnapshot::default();
    for (name, entry) in defaults.agents {
        snapshot.agents.entry(name).or_insert(entry);
    }
    if snapshot
        .default_agent
        .as_deref()
        .is_some_and(|agent| !snapshot.agents.contains_key(agent))
    {
        return Err(RpcError::new(
            RpcErrorCode::AgentUnknown,
            "default_agent is unknown",
        ));
    }
    if snapshot
        .default_agent
        .as_deref()
        .is_some_and(|agent| !snapshot.agents[agent].enabled)
    {
        return Err(RpcError::new(
            RpcErrorCode::AgentDisabled,
            "default_agent is disabled",
        ));
    }
    Ok(snapshot)
}

fn normalize_agent_config_value(value: &mut Value) -> Result<(), RpcError> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| RpcError::new(RpcErrorCode::Validation, "agent config is invalid"))?;
    if let Some(schema) = object.get("schema_version") {
        if schema.as_u64() != Some(1) {
            return Err(RpcError::new(
                RpcErrorCode::Validation,
                "unsupported agent config schema version",
            ));
        }
    }
    for field in ["runtime", "database", "socket"] {
        if let Some(value) = object.get(field) {
            if !value.as_str().is_some_and(|value| !value.is_empty()) {
                return Err(RpcError::new(
                    RpcErrorCode::Validation,
                    "agent config path fields must be non-null strings",
                ));
            }
        }
    }
    let Some(agents) = object.get_mut("agents") else {
        return Ok(());
    };
    let agents = agents.as_object_mut().ok_or_else(|| {
        RpcError::new(
            RpcErrorCode::Validation,
            "agent config agents must be an object",
        )
    })?;
    for (name, entry) in agents.iter_mut() {
        let entry = entry.as_object_mut().ok_or_else(|| {
            RpcError::new(
                RpcErrorCode::Validation,
                "agent config entry must be an object",
            )
        })?;
        let default_enabled = name == "zcode";
        entry
            .entry("enabled")
            .or_insert(Value::Bool(default_enabled));
        entry
            .entry("spawn_supported")
            .or_insert(Value::Bool(default_enabled));
        entry.entry("default_model").or_insert(Value::Null);
        if !entry.get("enabled").is_some_and(Value::is_boolean)
            || !entry.get("spawn_supported").is_some_and(Value::is_boolean)
        {
            return Err(RpcError::new(
                RpcErrorCode::Validation,
                "agent config flags must be booleans",
            ));
        }
        if let Some(model) = entry.get("default_model") {
            if model.as_str().is_some_and(str::is_empty)
                || (name == "zcode" && !model.is_null())
                || (!model.is_null() && !model.is_string())
            {
                return Err(RpcError::new(
                    RpcErrorCode::Validation,
                    "agent config model selection is invalid",
                ));
            }
        }
    }
    Ok(())
}

fn respondable_pending_request(request: &PendingRequestView) -> bool {
    request.state == PendingRequestStateView::Pending && request.respondable
}

// Measure the complete envelope with the largest valid request ID. Large result
// text falls back to the existing first-page contract; oversized metadata still
// uses the transport's existing Oversized response.
fn bound_wait_result(response: &mut RpcSuccess) -> Result<(), RpcError> {
    let envelope = RpcResponse::success("\u{1}".repeat(MAX_REQUEST_ID_BYTES), response.clone());
    let fits = serde_json::to_vec(&envelope)
        .map_err(|_| RpcError::new(RpcErrorCode::Oversized, "response encoding failed"))?
        .len()
        .saturating_add(1)
        <= MAX_RESPONSE_FRAME_BYTES;
    if !fits {
        if let RpcSuccess::TaskWait {
            result: Some(result),
            ..
        } = response
        {
            let (end, next_offset) =
                result_page_bounds(&result.final_text, 0, MAX_RESULT_CHUNK_BYTES)?;
            result.final_text.truncate(end);
            result.next_offset = next_offset;
            result.complete = next_offset.is_none();
        }
        let envelope = RpcResponse::success("\u{1}".repeat(MAX_REQUEST_ID_BYTES), response.clone());
        if serde_json::to_vec(&envelope)
            .map_or(true, |bytes| bytes.len() + 1 > MAX_RESPONSE_FRAME_BYTES)
        {
            return Err(RpcError::new(
                RpcErrorCode::Oversized,
                "response frame exceeds cap",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod wait_tests {
    use super::*;
    use crate::{CommandRuntimeFactory, SchedulerConfig};
    use external_store::TaskResult;
    use std::process::Command;

    #[test]
    fn draining_existing_task_message_is_idempotent_but_new_rejected() {
        let (_dir, service, id) = fixture();
        let msg = MessageInput {
            agent_id: id.clone(),
            message_id: "drain-msg".into(),
            mode: "queue".into(),
            content: "x".into(),
        };
        service
            .dispatch(RpcMethod::TaskMessage(msg.clone()))
            .unwrap();
        service.dispatch(RpcMethod::DaemonBeginDrain { cancel_active: false }).unwrap();
        let err = service
            .dispatch(RpcMethod::TaskMessage(MessageInput {
                message_id: "new-msg".into(),
                ..msg.clone()
            }))
            .unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Unavailable);
        assert!(service.dispatch(RpcMethod::TaskMessage(msg)).is_ok());
    }

    #[test]
    fn drain_transition_serializes_with_new_message_admission() {
        let (_dir, service, id) = fixture();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        service.scheduler.set_admission_hook({
            let barrier = Arc::clone(&barrier);
            Arc::new(move || {
                barrier.wait();
                barrier.wait();
            })
        });

        let message_service = Arc::clone(&service);
        let first_id = id.clone();
        let message = std::thread::spawn(move || {
            message_service.dispatch(RpcMethod::TaskMessage(MessageInput {
                agent_id: first_id,
                message_id: "before-drain".into(),
                mode: "queue".into(),
                content: "accepted before the drain linearization point".into(),
            }))
        });

        barrier.wait();
        let drain_service = Arc::clone(&service);
        let drain = std::thread::spawn(move || drain_service.dispatch(RpcMethod::DaemonBeginDrain { cancel_active: false }));
        barrier.wait();

        assert!(message.join().unwrap().is_ok());
        assert!(drain.join().unwrap().is_ok());
        service.scheduler.set_admission_hook(Arc::new(|| {}));

        let error = service
            .dispatch(RpcMethod::TaskMessage(MessageInput {
                agent_id: id,
                message_id: "after-drain".into(),
                mode: "queue".into(),
                content: "must be rejected".into(),
            }))
            .unwrap_err();
        assert_eq!(error.code, RpcErrorCode::Unavailable);
        assert_eq!(error.message, "daemon_draining");
        assert!(service.store.message("after-drain").unwrap().is_none());
    }

    #[test]
    fn drain_transition_serializes_with_new_task_admission() {
        let (directory, service, _) = fixture();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        service.scheduler.set_admission_hook({
            let barrier = Arc::clone(&barrier);
            Arc::new(move || {
                barrier.wait();
                barrier.wait();
            })
        });

        let task_workspace = directory.path().join("spawn-admission");
        std::fs::create_dir(&task_workspace).unwrap();
        let manifest = GeneralTaskManifest {
            schema: "zcode-general-task/v1".into(),
            agent_id: String::new(),
            repository: task_workspace.canonicalize().unwrap(),
            permission_mode: external_core::PermissionMode::Plan,
            prompt: "accepted before the drain linearization point".into(),
            write_manifest: vec![],
        };
        let enqueue_service = Arc::clone(&service);
        let first_manifest = manifest.clone();
        let enqueue =
            std::thread::spawn(move || enqueue_service.scheduler.enqueue_general(&first_manifest));

        barrier.wait();
        let drain_service = Arc::clone(&service);
        let drain = std::thread::spawn(move || drain_service.dispatch(RpcMethod::DaemonBeginDrain { cancel_active: false }));
        barrier.wait();

        assert!(enqueue.join().unwrap().is_ok());
        assert!(drain.join().unwrap().is_ok());
        service.scheduler.set_admission_hook(Arc::new(|| {}));

        let error = service.scheduler.enqueue_general(&manifest).unwrap_err();
        assert!(matches!(
            error,
            SchedulerError::InvalidConfig(ref message) if message == "daemon_draining"
        ));
    }

    #[test]
    fn draining_lifecycle_methods_are_not_gate_rejected() {
        let (_dir, service, id) = fixture();
        service.dispatch(RpcMethod::DaemonBeginDrain { cancel_active: false }).unwrap();
        let methods = [
            RpcMethod::TaskWait(TaskWaitQuery {
                agent_id: id.clone(),
                after_revision: 0,
                wait_time: 0,
                message_id: None,
            }),
            RpcMethod::TaskCancel {
                agent_id: id.clone(),
            },
            RpcMethod::TaskResult {
                agent_id: id.clone(),
                offset: 0,
                limit: 10,
            },
            RpcMethod::TaskClose { agent_id: id },
        ];
        for method in methods {
            if let Err(error) = service.dispatch(method) {
                assert!(!error.message.contains("daemon_draining"));
            }
        }
    }

    pub(crate) fn fixture() -> (tempfile::TempDir, Arc<RpcService>, String) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("s01-wait-")
            .tempdir_in(root)
            .unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let factory = CommandRuntimeFactory::new(|_: &TaskRecord| -> std::io::Result<Command> {
            panic!("wait fixture must never start a runtime")
        });
        let scheduler = Scheduler::new(
            "wait-test",
            store.clone(),
            Arc::new(factory),
            SchedulerConfig::default(),
        )
        .unwrap();
        let submitted = scheduler
            .enqueue_general(&GeneralTaskManifest {
                schema: "zcode-general-task/v1".into(),
                agent_id: "".into(),
                repository: directory.path().canonicalize().unwrap(),
                permission_mode: external_core::PermissionMode::Plan,
                prompt: "wait fixture".into(),
                write_manifest: vec![],
            })
            .unwrap();
        let id = submitted.task.agent_id;
        let claim = store.claim_next("wait-test", 10, 10).unwrap().unwrap();
        store
            .mark_session_running(&id, claim.owner_epoch, "runtime", None, None, None)
            .unwrap();
        (
            directory,
            Arc::new(RpcService::new(scheduler, store).unwrap()),
            id,
        )
    }

    pub(crate) fn query(id: &str, wait_time: u64) -> TaskWaitQuery {
        TaskWaitQuery {
            agent_id: id.into(),
            after_revision: 0,
            wait_time,
            message_id: None,
        }
    }

    #[test]
    fn wait_defaults_limits_and_old_method_rejection() {
        let (_, service, id) = fixture();
        let parsed: TaskWaitQuery =
            serde_json::from_value(serde_json::json!({"agent_id":id})).unwrap();
        assert_eq!(parsed.wait_time, 290);
        assert_eq!(agent_capabilities(false).max_wait_ms, 299000);
        for value in [-1, 300] {
            let response = service.handle_bytes(
                &serde_json::to_vec(&serde_json::json!({
                    "version":RPC_VERSION,"request_id":"limits","method":"task_wait",
                    "params":{"agent_id":id,"wait_time":value}
                }))
                .unwrap(),
            );
            assert!(matches!(
                response.outcome,
                RpcOutcome::Error {
                    error: RpcError {
                        code: RpcErrorCode::Validation,
                        ..
                    }
                }
            ));
        }
        assert!(!RpcMethod::is_known("task_poll"));
        let before = service.store.get_task(&id).unwrap();
        let start = Instant::now();
        let RpcSuccess::TaskWait {
            timed_out: true,
            instruction,
            ..
        } = service
            .dispatch(RpcMethod::TaskWait(query(&id, 0)))
            .unwrap()
        else {
            panic!("expected unfinished wait response")
        };
        assert_eq!(
            instruction.as_deref(),
            Some("Not finished yet, call wait again; use observe only if latest_text_tail may indicate subagent runs into a meaningless loop")
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(before, service.store.get_task(&id).unwrap());
        // A terminal task accepts the maximum without actually sleeping.
        service
            .store
            .store_task_result(
                &id,
                &TaskResult {
                    outcome: TaskOutcome::Completed,
                    final_text: "done".into(),
                    partial: false,
                },
            )
            .unwrap();
        let RpcSuccess::TaskWait {
            timed_out: false,
            instruction,
            result_available,
            ..
        } = service
            .dispatch(RpcMethod::TaskWait(query(&id, 299)))
            .unwrap()
        else {
            panic!("expected terminal wait response")
        };
        assert!(result_available);
        assert_eq!(instruction.as_deref(), Some("Result now available"));
    }

    #[test]
    fn wait_ignores_revision_message_and_ordinary_activity() {
        let (_, service, id) = fixture();
        service
            .store
            .insert_message("message-1", &id, "queue", "continue")
            .unwrap();
        let tracker = Arc::new(crate::PassiveActivityTracker::new(true));
        service
            .scheduler
            .inner
            .state
            .lock()
            .unwrap()
            .activities
            .insert(id.clone(), tracker.clone());
        let mut input = query(&id, 1);
        input.message_id = Some("message-1".into());
        let before = service.store.get_task(&id).unwrap();
        let start = Instant::now();
        let iterations = std::cell::Cell::new(0usize);
        let response = service
            .task_wait(input, start + Duration::from_millis(80), &|| {
                // Deterministic mid-wait snapshot changes, without timing a worker.
                iterations.set(iterations.get() + 1);
                if iterations.get() == 2 {
                    let mut state = tracker.state.lock().unwrap();
                    state.revision = 900;
                    state.latest_text_tail = "ordinary text".into();
                    state.latest_progress = Some("ordinary progress".into());
                    state.last_model_delta_at = Some(Instant::now());
                    state.active_tools.insert(
                        "tool".into(),
                        (crate::PassiveToolKind::Bash, Instant::now()),
                    );
                }
                false
            })
            .unwrap();
        assert!(start.elapsed() >= Duration::from_millis(80));
        let RpcSuccess::TaskWait {
            timed_out: true,
            revision: 900,
            activity,
            instruction,
            message_receipt: Some(_),
            ..
        } = response
        else {
            panic!("ordinary activity ended wait")
        };
        assert_eq!(activity.latest_text_tail, "ordinary text");
        assert_eq!(
            instruction.as_deref(),
            Some("Not finished yet, call wait again; use observe only if latest_text_tail may indicate subagent runs into a meaningless loop")
        );
        assert_eq!(activity.active_tools.len(), 1);
        assert_eq!(before, service.store.get_task(&id).unwrap());
    }

    #[test]
    fn wait_respondable_pending_predicate_ignores_tool_kind_and_state() {
        for (kind, tool, state, respondable, wakes) in [
            (
                "permission",
                "bAsH",
                PendingRequestStateView::Pending,
                true,
                true,
            ),
            (
                "permission",
                "Read",
                PendingRequestStateView::Pending,
                true,
                true,
            ),
            (
                "permission",
                "Other",
                PendingRequestStateView::Pending,
                true,
                true,
            ),
            (
                "unsupported_input",
                "Read",
                PendingRequestStateView::Pending,
                true,
                true,
            ),
            (
                "permission",
                "Bash",
                PendingRequestStateView::Pending,
                false,
                false,
            ),
            (
                "permission",
                "Bash",
                PendingRequestStateView::Sending,
                true,
                false,
            ),
            (
                "permission",
                "Bash",
                PendingRequestStateView::Responded,
                true,
                false,
            ),
        ] {
            let request = PendingRequestView {
                request_id: "r".into(),
                kind: kind.into(),
                state,
                respondable,
                tool_name: Some(tool.into()),
                operation: "command".into(),
                summary: "fixture".into(),
                policy_preview: "unknown".into(),
            };
            assert_eq!(respondable_pending_request(&request), wakes);
        }
    }

    #[test]
    fn wait_wakes_for_non_bash_respondable_permission() {
        let (_, service, id) = fixture();
        service
            .store
            .insert_pending_request(
                "request",
                &id,
                "correlation",
                "permission",
                r#"{"toolName":"Read"}"#,
            )
            .unwrap();
        let RpcSuccess::TaskWait {
            command_pending_approval,
            pending_requests,
            timed_out,
            ..
        } = service
            .dispatch(RpcMethod::TaskWait(query(&id, 299)))
            .unwrap()
        else {
            panic!("expected wait response")
        };
        assert!(command_pending_approval);
        assert!(!timed_out);
        assert_eq!(pending_requests.len(), 1);
        assert_eq!(pending_requests[0].tool_name.as_deref(), Some("Read"));
        assert!(pending_requests[0].respondable);
    }

    #[test]
    fn wait_projection_cap_does_not_promote_the_101st_request() {
        let (_, service, id) = fixture();
        for index in 0..=MAX_PENDING_REQUESTS {
            let (request_type, payload) = if index < MAX_PENDING_REQUESTS {
                ("unsupported_input", r#"{}"#)
            } else {
                ("permission", r#"{"toolName":"Read"}"#)
            };
            service
                .store
                .insert_pending_request(
                    &format!("request-{index}"),
                    &id,
                    &format!("correlation-{index}"),
                    request_type,
                    payload,
                )
                .unwrap();
        }
        let start = Instant::now();
        let response = service
            .task_wait(query(&id, 1), start + Duration::from_millis(60), &|| false)
            .unwrap();
        let RpcSuccess::TaskWait {
            command_pending_approval,
            pending_requests,
            timed_out,
            ..
        } = response
        else {
            panic!("expected wait response")
        };
        assert_eq!(pending_requests.len(), MAX_PENDING_REQUESTS);
        assert!(pending_requests
            .iter()
            .all(|request| !respondable_pending_request(request)));
        assert!(!command_pending_approval);
        assert!(timed_out);
        assert!(start.elapsed() >= Duration::from_millis(40));
    }

    #[test]
    fn wait_bash_state_transition_stops_early_wake() {
        let (_, service, id) = fixture();
        service
            .store
            .insert_pending_request(
                "request",
                &id,
                "correlation",
                "permission",
                r#"{"toolName":"Bash"}"#,
            )
            .unwrap();
        assert!(matches!(
            service
                .dispatch(RpcMethod::TaskWait(query(&id, 299)))
                .unwrap(),
            RpcSuccess::TaskWait {
                command_pending_approval: true,
                timed_out: false,
                ..
            }
        ));
        service
            .store
            .claim_pending_response_if_accepting(&id, "request", "allow", None)
            .unwrap();
        assert!(matches!(
            service
                .task_wait(
                    query(&id, 1),
                    Instant::now() + Duration::from_millis(30),
                    &|| false
                )
                .unwrap(),
            RpcSuccess::TaskWait {
                command_pending_approval: false,
                timed_out: true,
                ..
            }
        ));
    }

    #[test]
    fn wait_terminal_results_preserve_outcomes_and_page_large_text() {
        for (outcome, text, complete) in [
            (
                TaskOutcome::Completed,
                "x".repeat(MAX_RESULT_CHUNK_BYTES + 10),
                true,
            ),
            (TaskOutcome::Failed, "failure".into(), true),
            (TaskOutcome::Cancelled, "cancelled".into(), true),
            (
                TaskOutcome::Completed,
                "\0".repeat(MAX_RESPONSE_FRAME_BYTES),
                false,
            ),
        ] {
            let (_, service, id) = fixture();
            service
                .store
                .store_task_result(
                    &id,
                    &TaskResult {
                        outcome,
                        final_text: text.clone(),
                        partial: outcome != TaskOutcome::Completed,
                    },
                )
                .unwrap();
            let response = service
                .dispatch(RpcMethod::TaskWait(query(&id, 299)))
                .unwrap();
            let RpcSuccess::TaskWait {
                result: Some(result),
                timed_out: false,
                ..
            } = &response
            else {
                panic!("terminal result missing")
            };
            assert_eq!(result.outcome, outcome);
            assert_eq!(result.total_bytes, text.len());
            assert_eq!(result.complete, complete);
            assert_eq!(
                result.next_offset,
                (!complete).then_some(MAX_RESULT_CHUNK_BYTES)
            );
            assert_eq!(result.final_text, text[..result.final_text.len()]);
            assert!(
                serde_json::to_vec(&RpcResponse::success("q".repeat(128), response))
                    .unwrap()
                    .len()
                    + 1
                    <= MAX_RESPONSE_FRAME_BYTES
            );
            assert_eq!(
                service
                    .store
                    .task_result(&id)
                    .unwrap()
                    .unwrap()
                    .result
                    .final_text,
                text
            );
        }
    }

    #[test]
    fn wait_interruption_does_not_mutate_task() {
        let (_, service, id) = fixture();
        let before = service.store.get_task(&id).unwrap();
        let response = service.handle_bytes_interruptible(
            &serde_json::to_vec(&RpcRequest {
                version: RPC_VERSION,
                request_id: "interrupt".into(),
                method: RpcMethod::TaskWait(query(&id, 299)),
            })
            .unwrap(),
            &|| true,
        );
        assert!(matches!(response.outcome, RpcOutcome::Error { .. }));
        assert_eq!(before, service.store.get_task(&id).unwrap());
    }

    #[test]
    fn assembled_service_queues_message_and_exposes_receipt() {
        let (_directory, service, id) = fixture();
        let response = service
            .dispatch(RpcMethod::TaskMessage(MessageInput {
                agent_id: id.clone(),
                message_id: "assembled-message".into(),
                mode: "queue".into(),
                content: "continue with the requested work".into(),
            }))
            .unwrap();
        let RpcSuccess::Message { disposition, .. } = response else {
            panic!("expected message response");
        };
        assert_eq!(disposition, MessageDispositionView::Queued);
        let receipt = service
            .store
            .message("assembled-message")
            .unwrap()
            .expect("queued message receipt");
        assert_eq!(receipt.message_id, "assembled-message");
        assert_eq!(receipt.mode, "queue");
        assert_eq!(receipt.content, "continue with the requested work");
    }

    #[test]
    fn terminal_task_rejects_message_without_deleting_result() {
        let (_directory, service, id) = fixture();
        service
            .store
            .store_task_result(
                &id,
                &TaskResult {
                    outcome: TaskOutcome::Completed,
                    final_text: "terminal result".into(),
                    partial: false,
                },
            )
            .unwrap();
        let before = service.store.task_result(&id).unwrap().unwrap();
        let response = service.dispatch(RpcMethod::TaskMessage(MessageInput {
            agent_id: id.clone(),
            message_id: "terminal-message".into(),
            mode: "queue".into(),
            content: "must not resume".into(),
        }));
        assert!(matches!(
            response,
            Err(RpcError {
                code: RpcErrorCode::Validation,
                ..
            })
        ));
        assert_eq!(
            service.store.task_result(&id).unwrap().unwrap().result,
            before.result
        );
        assert!(service.store.message("terminal-message").unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn wait_socket_disconnect_and_shutdown_release_workers_without_task_mutation() {
        use std::io::{Read, Write};
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixStream;
        let (directory, service, id) = fixture();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        // macOS sockaddr_un caps path length; a cwd-relative path still keeps
        // the fixture inside the repository's prescribed workspace directory.
        let absolute_socket = directory.path().join("rpc.sock");
        let socket = absolute_socket
            .strip_prefix(std::env::current_dir().unwrap())
            .unwrap()
            .to_path_buf();
        let server = RpcServer::bind(
            &socket,
            service.clone(),
            ServerOptions {
                max_connections: 1,
                ..ServerOptions::default()
            },
        )
        .unwrap();
        let before = service.store.get_task(&id).unwrap();
        let request = RpcRequest {
            version: RPC_VERSION,
            request_id: "wait".into(),
            method: RpcMethod::TaskWait(query(&id, 299)),
        };
        let mut frame = serde_json::to_vec(&request).unwrap();
        frame.push(b'\n');
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream.write_all(&frame).unwrap();
        // Let the bounded connection worker enter the request, then disconnect.
        thread::sleep(Duration::from_millis(30));
        drop(stream);
        let client = RpcClient::new(&socket, Duration::from_secs(1));
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let response = client
                .call(&RpcRequest {
                    version: RPC_VERSION,
                    request_id: "status".into(),
                    method: RpcMethod::SystemStatus,
                })
                .unwrap();
            if matches!(response.outcome, RpcOutcome::Success { .. }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "disconnected wait retained connection slot"
            );
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(20));
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream.write_all(&frame).unwrap();
        thread::sleep(Duration::from_millis(30));
        let start = Instant::now();
        server.shutdown();
        assert!(start.elapsed() < Duration::from_secs(1));
        let mut returned = String::new();
        stream.read_to_string(&mut returned).unwrap();
        assert!(returned.contains("wait interrupted"));
        assert_eq!(before, service.store.get_task(&id).unwrap());
    }
}

fn result_page_bounds(
    text: &str,
    offset: usize,
    limit: usize,
) -> Result<(usize, Option<usize>), RpcError> {
    let total_bytes = text.len();
    if offset > total_bytes || !text.is_char_boundary(offset) {
        return Err(RpcError::new(
            RpcErrorCode::Validation,
            "result offset is outside the result",
        ));
    }
    let mut end = offset.saturating_add(limit).min(total_bytes);
    while end > offset && !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == offset && offset < total_bytes {
        return Err(RpcError::new(
            RpcErrorCode::Validation,
            "result limit does not include a complete UTF-8 character",
        ));
    }
    let next_offset = (end < total_bytes).then_some(end);
    debug_assert!(next_offset.is_none_or(|next| next > offset));
    Ok((end, next_offset))
}

fn opaque_generation() -> Result<String, RpcServiceConfigError> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|_| RpcServiceConfigError::GenerationUnavailable)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn agent_capabilities(runtime_source_verified: bool) -> AgentCapabilitiesView {
    let maturity = BTreeMap::new();
    AgentCapabilitiesView {
        max_rpc_request_frame_bytes: MAX_REQUEST_FRAME_BYTES,
        max_rpc_response_frame_bytes: MAX_RESPONSE_FRAME_BYTES,
        max_wait_ms: MAX_WAIT.as_millis() as u64,
        maturity,
        observation: ObservationCapabilityView {
            protocol: OBSERVATION_SCHEMA.into(),
            public_reasoning_default: true,
            runtime_source_verified,
            defaults: ObservationDefaultsView {
                top_tools: 3,
                recent_calls_per_tool: 5,
                reasoning_chars: 200,
            },
        },
    }
}

fn parse_task_cursor(cursor: &str) -> Result<u64, RpcError> {
    let value = cursor
        .strip_prefix("task:")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| RpcError::new(RpcErrorCode::Validation, "task cursor is invalid"))?;
    Ok(value)
}

fn format_task_cursor(cursor: u64) -> String {
    format!("task:{cursor}")
}

fn task_view(task: TaskRecord) -> TaskView {
    let prepared = serde_json::from_str::<serde_json::Value>(&task.prepared_launch_json).ok();
    let permission_mode = prepared.as_ref().and_then(|v| {
        v.get("permission_mode")
            .and_then(|x| x.as_str())
            .map(str::to_owned)
    });
    let caller_prompt_sha256 = prepared.as_ref().and_then(|v| {
        v.get("prompt_sha256")
            .and_then(|x| x.as_str())
            .map(str::to_owned)
    });
    let workspace_path = Some(task.workspace_path.clone());
    TaskView {
        agent_id: task.agent_id,
        session_id: task.zcode_session_id,
        turn_id: None,
        phase: match task.phase {
            TaskPhase::Queued => "QUEUED",
            TaskPhase::Preparing => "PREPARING",
            TaskPhase::Running => "RUNNING",
            TaskPhase::WaitingInput => "WAITING_INPUT",
            TaskPhase::Cancelling => "CANCELLING",
            TaskPhase::Terminal => "TERMINAL",
        }
        .into(),
        outcome: task.outcome,
        reason_code: task.failure_code,
        stop_requested: task.stop_requested,
        close_requested: task.close_requested,
        closed: task.closed_at.is_some(),
        reaped: task.reaped_at.is_some(),
        input_identity: InputIdentityView {
            admission: prepared
                .as_ref()
                .and_then(|v| v.get("admission"))
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
            workspace_path,
            permission_mode,
            caller_prompt_sha256,
        },
    }
}

fn task_activity_view(
    phase: TaskPhase,
    snapshot: Option<PassiveActivitySnapshot>,
) -> TaskActivityView {
    let terminal = phase == TaskPhase::Terminal;
    let state = match phase {
        TaskPhase::Queued => TaskActivityStateView::Queued,
        TaskPhase::Preparing => TaskActivityStateView::Preparing,
        TaskPhase::Running => TaskActivityStateView::Active,
        TaskPhase::WaitingInput => TaskActivityStateView::WaitingInput,
        TaskPhase::Cancelling => TaskActivityStateView::Cancelling,
        TaskPhase::Terminal => TaskActivityStateView::Terminal,
    };
    let Some(snapshot) = snapshot else {
        return TaskActivityView {
            state,
            last_runtime_event_at: None,
            last_activity_age_ms: None,
            model_request_active: false,
            model_request_age_ms: None,
            model_last_delta_age_ms: None,
            latest_text_tail: String::new(),
            latest_text_updated_at: None,
            latest_text_truncated: false,
            latest_progress: None,
            active_tools: Vec::new(),
            window_60s: ActivityWindowView::default(),
            telemetry_status: TelemetryStatusView::Unavailable,
        };
    };
    TaskActivityView {
        state,
        last_runtime_event_at: snapshot.last_runtime_event_at,
        last_activity_age_ms: snapshot.last_activity_age_ms,
        model_request_active: !terminal && snapshot.model_request_active,
        model_request_age_ms: if terminal {
            None
        } else {
            snapshot.model_request_age_ms
        },
        model_last_delta_age_ms: snapshot.model_last_delta_age_ms,
        latest_text_tail: snapshot.latest_text_tail,
        latest_text_updated_at: snapshot.latest_text_updated_at,
        latest_text_truncated: snapshot.latest_text_truncated,
        latest_progress: snapshot.latest_progress,
        active_tools: snapshot
            .active_tools
            .into_iter()
            .map(|tool| ActiveToolView {
                tool_call_id: tool.tool_call_id,
                kind: match tool.kind {
                    PassiveToolKind::Read => ActivityToolKindView::Read,
                    PassiveToolKind::Bash => ActivityToolKindView::Bash,
                    PassiveToolKind::Other => ActivityToolKindView::Other,
                },
            })
            .collect(),
        window_60s: activity_window_view(snapshot.window_60s),
        telemetry_status: if snapshot.telemetry_degraded {
            TelemetryStatusView::Degraded
        } else {
            TelemetryStatusView::Healthy
        },
    }
}

#[cfg(test)]
mod activity_projection_tests {
    use super::{agent_capabilities, task_activity_view, TaskActivityStateView};
    use crate::{PassiveActivitySnapshot, PassiveActivityWindow};
    use external_store::TaskPhase;

    fn active_model_request_snapshot() -> PassiveActivitySnapshot {
        PassiveActivitySnapshot {
            revision: 7,
            last_runtime_event_at: Some(1_000),
            last_activity_age_ms: Some(250),
            model_request_active: true,
            model_request_age_ms: Some(900),
            model_last_delta_age_ms: Some(300),
            latest_text_tail: "preserved tail".into(),
            latest_text_updated_at: Some(950),
            latest_text_truncated: false,
            latest_progress: Some("preserved progress".into()),
            active_tools: Vec::new(),
            oldest_active_tool_age_ms: None,
            window_60s: PassiveActivityWindow {
                reasoning_delta_events: 2,
                ..PassiveActivityWindow::default()
            },
            telemetry_degraded: true,
        }
    }

    #[test]
    fn terminal_phase_clears_stale_model_request_activity_and_preserves_history() {
        let activity =
            task_activity_view(TaskPhase::Terminal, Some(active_model_request_snapshot()));

        assert_eq!(activity.state, TaskActivityStateView::Terminal);
        assert!(!activity.model_request_active);
        assert_eq!(activity.model_request_age_ms, None);
        assert_eq!(activity.last_runtime_event_at, Some(1_000));
        assert_eq!(activity.last_activity_age_ms, Some(250));
        assert_eq!(activity.model_last_delta_age_ms, Some(300));
        assert_eq!(activity.latest_text_tail, "preserved tail");
        assert_eq!(
            activity.latest_progress.as_deref(),
            Some("preserved progress")
        );
        assert_eq!(activity.window_60s.reasoning_delta_events, 2);
    }

    #[test]
    fn running_phase_preserves_live_model_request_activity() {
        let activity =
            task_activity_view(TaskPhase::Running, Some(active_model_request_snapshot()));

        assert_eq!(activity.state, TaskActivityStateView::Active);
        assert!(activity.model_request_active);
        assert_eq!(activity.model_request_age_ms, Some(900));
    }

    #[test]
    fn terminal_phase_without_runtime_snapshot_is_inactive() {
        let activity = task_activity_view(TaskPhase::Terminal, None);

        assert_eq!(activity.state, TaskActivityStateView::Terminal);
        assert!(!activity.model_request_active);
        assert_eq!(activity.model_request_age_ms, None);
    }

    #[test]
    fn status_reports_observation_contract_and_real_source_state() {
        let verified = agent_capabilities(true).observation;
        assert_eq!(verified.protocol, "zas-observation/1.1");
        assert!(verified.public_reasoning_default);
        assert!(verified.runtime_source_verified);
        assert_eq!(verified.defaults.top_tools, 3);
        assert_eq!(verified.defaults.recent_calls_per_tool, 5);
        assert_eq!(verified.defaults.reasoning_chars, 200);
        assert!(
            !agent_capabilities(false)
                .observation
                .runtime_source_verified
        );
    }
}

fn activity_window_view(value: PassiveActivityWindow) -> ActivityWindowView {
    ActivityWindowView {
        reasoning_delta_events: value.reasoning_delta_events,
        reasoning_delta_bytes: value.reasoning_delta_bytes,
        text_delta_events: value.text_delta_events,
        text_delta_bytes: value.text_delta_bytes,
        tool_calls_started: value.tool_calls_started,
        tool_calls_completed: value.tool_calls_completed,
        tool_calls_failed: value.tool_calls_failed,
        read_calls: value.read_calls,
        bash_calls: value.bash_calls,
        other_tool_calls: value.other_tool_calls,
    }
}

impl From<StoredTaskResult> for TaskResultView {
    fn from(stored: StoredTaskResult) -> Self {
        let total_bytes = stored.result.final_text.len();
        Self {
            outcome: stored.result.outcome,
            final_text: stored.result.final_text,
            partial: stored.result.partial,
            result_sha256: stored.result_sha256,
            offset: 0,
            total_bytes,
            next_offset: None,
            complete: true,
        }
    }
}

fn pending_request_view(request: StoredPendingRequest) -> PendingRequestView {
    let state = match request.state {
        PendingRequestState::Pending => PendingRequestStateView::Pending,
        PendingRequestState::Sending => PendingRequestStateView::Sending,
        PendingRequestState::Responded => PendingRequestStateView::Responded,
    };
    if request.request_type != "permission" {
        return PendingRequestView {
            request_id: request.request_id,
            kind: "unsupported_input".into(),
            state,
            respondable: false,
            tool_name: None,
            operation: "user_input".into(),
            summary: "unsupported user input request".into(),
            policy_preview: "unknown".into(),
        };
    }
    let params = serde_json::from_str::<Value>(&request.payload_json).ok();
    let tool_name = params
        .as_ref()
        .and_then(|value| value.get("toolName"))
        .and_then(Value::as_str)
        .map(|value| value.chars().take(64).collect::<String>());
    let operation = tool_name
        .as_deref()
        .map(operation_category)
        .unwrap_or("unknown")
        .to_owned();
    let summary = params
        .as_ref()
        .map(sanitized_permission_summary)
        .unwrap_or_else(|| "unrecognized permission request".into());
    let policy_preview = "official_permission_request".to_owned();
    PendingRequestView {
        request_id: request.request_id,
        kind: "permission".into(),
        state,
        respondable: true,
        tool_name,
        operation,
        summary,
        policy_preview,
    }
}

#[cfg(test)]
mod result_paging_tests {
    use super::{
        result_page_bounds, InputIdentityView, RpcResponse, RpcSuccess, TaskResultView, TaskView,
        MAX_RESPONSE_FRAME_BYTES, MAX_RESULT_CHUNK_BYTES,
    };
    use external_store::TaskOutcome;

    fn task() -> TaskView {
        TaskView {
            agent_id: "a".repeat(256),
            session_id: None,
            turn_id: None,
            phase: "TERMINAL".into(),
            outcome: Some(TaskOutcome::Completed),
            reason_code: Some("r".repeat(256)),
            stop_requested: false,
            close_requested: false,
            closed: false,
            reaped: true,
            input_identity: InputIdentityView {
                admission: None,
                workspace_path: None,
                permission_mode: None,
                caller_prompt_sha256: None,
            },
        }
    }

    #[test]
    fn non_terminal_pages_always_advance() {
        assert_eq!(result_page_bounds("abcdef", 0, 3).unwrap(), (3, Some(3)));
        assert_eq!(result_page_bounds("abcdef", 3, 3).unwrap(), (6, None));
    }

    #[test]
    fn too_small_utf8_page_is_rejected_instead_of_stalling() {
        let error = result_page_bounds("你a", 0, 1).unwrap_err();
        assert_eq!(error.code, super::RpcErrorCode::Validation);
        assert_eq!(result_page_bounds("你a", 0, 3).unwrap(), (3, Some(3)));
        assert!(result_page_bounds("你a", 1, 3).is_err());
    }

    #[test]
    fn worst_case_encoded_result_response_and_newline_fit_the_frame() {
        let text = "\u{0}".repeat(MAX_RESULT_CHUNK_BYTES);
        let response = RpcResponse::success(
            "q".repeat(128),
            RpcSuccess::TaskResult {
                task: task(),
                result: Some(TaskResultView {
                    outcome: TaskOutcome::Completed,
                    final_text: text,
                    partial: false,
                    result_sha256: "f".repeat(64),
                    offset: 0,
                    total_bytes: MAX_RESULT_CHUNK_BYTES + 1,
                    next_offset: Some(MAX_RESULT_CHUNK_BYTES),
                    complete: false,
                }),
            },
        );
        assert!(serde_json::to_vec(&response).unwrap().len() + 1 <= MAX_RESPONSE_FRAME_BYTES);
    }

    #[test]
    fn transport_projection_does_not_change_the_stored_outcome() {
        let view = TaskResultView {
            outcome: TaskOutcome::Failed,
            final_text: "failure".into(),
            partial: true,
            result_sha256: "f".repeat(64),
            offset: 0,
            total_bytes: 7,
            next_offset: None,
            complete: true,
        };
        assert_eq!(view.outcome, TaskOutcome::Failed);
        assert!(view.partial);
    }
}

fn configured_runtime_identity(path: Option<PathBuf>) -> RuntimeIdentityView {
    RuntimeIdentityView {
        configured_path_source: if path.is_some() {
            "daemon_configuration".into()
        } else {
            "unknown".into()
        },
        configured_path: path.map(|path| path.to_string_lossy().into_owned()),
        // A configured path is not proof that a process ran or which version
        // answered. No status query starts the runtime to fill this field.
        observed_version: None,
        observed_version_source: "unknown".into(),
    }
}

pub fn running_component_identity(component: &str, version: &str) -> ComponentIdentityView {
    running_component_identity_from(
        component,
        version,
        option_env!("ZAS_SOURCE_REVISION"),
        option_env!("ZAS_SOURCE_DIRTY"),
        env::current_exe().ok(),
        SystemTime::now(),
    )
}

fn running_component_identity_from(
    component: &str,
    version: &str,
    revision: Option<&str>,
    dirty: Option<&str>,
    executable: Option<PathBuf>,
    captured_at: SystemTime,
) -> ComponentIdentityView {
    let source_revision = revision
        .filter(|value| *value != "unknown" && !value.is_empty())
        .map(str::to_owned);
    let source_dirty = match dirty {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    };
    let (path, sha256) = executable.map_or((None, None), |path| {
        let hash = File::open(&path).ok().and_then(|mut file| {
            let mut hasher = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = file.read(&mut buffer).ok()?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            Some(format!("{:x}", hasher.finalize()))
        });
        (Some(path.to_string_lossy().into_owned()), hash)
    });
    ComponentIdentityView {
        component: component.into(),
        version: version.into(),
        source_revision,
        source_dirty,
        artifact: ArtifactIdentityView {
            path,
            sha256,
            source: "running_executable".into(),
            captured_at_ms: captured_at
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        },
    }
}

fn operation_category(tool_name: &str) -> &'static str {
    match tool_name.to_ascii_lowercase().as_str() {
        "read" | "grep" | "glob" => "read",
        "write" | "edit" | "delete" | "move" => "write",
        "bash" | "execute" | "terminal" => "command",
        "network" => "network",
        "git_ref_mutation" => "git_ref_mutation",
        _ => "unknown",
    }
}

fn sanitized_permission_summary(params: &Value) -> String {
    let input = params.get("input").unwrap_or(&Value::Null);
    let leaf = |name: &str| {
        input
            .get(name)
            .and_then(Value::as_str)
            .and_then(|value| std::path::Path::new(value).file_name())
            .map(|value| value.to_string_lossy().chars().take(96).collect::<String>())
    };
    if let Some(program) = leaf("program") {
        let count = input
            .get("args")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        return format!("command {program} with {count} arguments");
    }
    if let Some(path) = leaf("path")
        .or_else(|| leaf("destination"))
        .or_else(|| leaf("source"))
    {
        return format!("target {path}");
    }
    if params
        .get("toolName")
        .and_then(Value::as_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("git_ref_mutation"))
    {
        return "Git reference mutation".into();
    }
    "permission request".into()
}

fn validate_id(value: &str, field: &str) -> Result<(), RpcError> {
    validate_text(value, field, 256)
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_REQUEST_ID_BYTES && !value.contains('\0')
}

fn validate_text(value: &str, field: &str, max: usize) -> Result<(), RpcError> {
    if value.is_empty() || value.len() > max || value.contains('\0') {
        return Err(RpcError::new(
            RpcErrorCode::Validation,
            format!("{field} is invalid"),
        ));
    }
    Ok(())
}

fn map_scheduler(error: SchedulerError) -> RpcError {
    match error {
        SchedulerError::Store(error) => map_store(error),
        SchedulerError::InvalidConfig(message) => {
            if message == "daemon_draining" {
                return RpcError::new(RpcErrorCode::Unavailable, "daemon_draining");
            }
            // Preserve the bounded, actionable preparation reason. The MCP
            // facade may still redact it for callers, but RPC diagnostics
            // must distinguish repository, path, budget, and state errors.
            RpcError::new(
                RpcErrorCode::Validation,
                format!("scheduler rejected the operation: {message}"),
            )
        }
        SchedulerError::RuntimeSpawn { .. } | SchedulerError::LifecycleSink { .. } => {
            RpcError::new(RpcErrorCode::RuntimeLost, "runtime operation failed")
        }
        SchedulerError::RuntimeCommand { .. } => {
            let message = match &error {
                SchedulerError::RuntimeCommand { message, .. } => message.as_str(),
                _ => unreachable!(),
            };
            if message == "TERMINAL_SEND_UNSUPPORTED" {
                RpcError::new(RpcErrorCode::Validation, message)
            } else if message == "daemon_draining" {
                RpcError::new(RpcErrorCode::Unavailable, message)
            } else {
                RpcError::new(RpcErrorCode::Unavailable, "RUNTIME_COMMAND_FAILED")
            }
        }
    }
}

fn map_store(error: StoreError) -> RpcError {
    match error {
        StoreError::LegacySchemaUnsupported => RpcError::new(
            RpcErrorCode::Persistence,
            "STORE_SCHEMA_VERSION_UNSUPPORTED",
        ),
        StoreError::Sqlite(_) => {
            RpcError::new(RpcErrorCode::Persistence, "durable store operation failed")
        }
        StoreError::Conflict(message) if message.starts_with("WORKSPACE_BUSY") => {
            let active_agent_id = message
                .strip_prefix("WORKSPACE_BUSY active_agent_id=")
                .map(str::to_owned);
            let mut error = RpcError::new(RpcErrorCode::Conflict, "WORKSPACE_BUSY");
            error.active_agent_id = active_agent_id;
            error
        }
        StoreError::Conflict(message)
            if message == "message id is already bound to different content"
                || message == "MESSAGE_ID_CONFLICT"
                || (message.starts_with("message ") && message.ends_with(" already exists")) =>
        {
            RpcError::new(RpcErrorCode::Conflict, "MESSAGE_ID_CONFLICT")
        }
        StoreError::Conflict(_) => RpcError::new(RpcErrorCode::Conflict, "durable state conflict"),
        StoreError::InvalidState(_) => RpcError::new(
            RpcErrorCode::Validation,
            "durable state rejected the operation",
        ),
    }
}

#[cfg(test)]
mod error_classification_tests {
    use super::*;
    #[test]
    fn preserves_safe_reasons_without_exposing_store_or_runtime_details() {
        let conflict = map_store(StoreError::Conflict(
            "message id is already bound to different content".into(),
        ));
        assert_eq!(conflict.code, RpcErrorCode::Conflict);
        assert_eq!(conflict.message, "MESSAGE_ID_CONFLICT");
        let rejection = map_scheduler(SchedulerError::RuntimeCommand {
            agent_id: "a".into(),
            message: "sensitive".into(),
        });
        assert_eq!(rejection.code, RpcErrorCode::Unavailable);
        assert_eq!(rejection.message, "RUNTIME_COMMAND_FAILED");
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn missing_build_and_executable_facts_remain_unknown() {
        let identity = running_component_identity_from(
            "daemon",
            "1.2.3",
            Some("unknown"),
            Some("unknown"),
            None,
            UNIX_EPOCH + Duration::from_millis(7),
        );
        assert_eq!(identity.source_revision, None);
        assert_eq!(identity.source_dirty, None);
        assert_eq!(identity.artifact.path, None);
        assert_eq!(identity.artifact.sha256, None);
        assert_eq!(identity.artifact.captured_at_ms, 7);
    }

    #[test]
    fn artifact_hash_is_bound_to_the_reported_running_path() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("old-running-daemon");
        let disk_payload = directory.path().join("new-distributed-daemon");
        File::create(&executable)
            .unwrap()
            .write_all(b"old")
            .unwrap();
        File::create(&disk_payload)
            .unwrap()
            .write_all(b"new")
            .unwrap();
        let identity = running_component_identity_from(
            "daemon",
            "1.2.3",
            Some("abc123"),
            Some("true"),
            Some(executable.clone()),
            UNIX_EPOCH,
        );
        assert_eq!(identity.artifact.path.as_deref(), executable.to_str());
        assert_eq!(
            identity.artifact.sha256.as_deref(),
            Some("cba06b5736faf67e54b07b561eae94395e774c517a7d910a54369e1263ccfbd4")
        );
        assert_ne!(identity.artifact.path.as_deref(), disk_payload.to_str());
        assert_eq!(identity.source_revision.as_deref(), Some("abc123"));
        assert_eq!(identity.source_dirty, Some(true));
    }

    #[test]
    fn configured_runtime_is_not_promoted_to_observed_version_or_model() {
        let runtime = configured_runtime_identity(Some(PathBuf::from("/runtime/zcode")));
        assert_eq!(runtime.configured_path.as_deref(), Some("/runtime/zcode"));
        assert_eq!(runtime.configured_path_source, "daemon_configuration");
        assert_eq!(runtime.observed_version, None);
        assert_eq!(runtime.observed_version_source, "unknown");
        let models = ModelIdentityView {
            configured: Some(ModelIdentityFactView {
                value: "configured-model".into(),
                source: "session_create_configuration".into(),
            }),
            observed_response: None,
        };
        assert!(models.configured.is_some());
        assert!(models.observed_response.is_none());
    }

    #[test]
    fn same_version_legacy_status_frame_without_identity_still_decodes() {
        let response = RpcResponse::success(
            "legacy-status".into(),
            RpcSuccess::SystemStatus {
                status: SystemStatusView {
                    api_surface: "generic_agent".into(),
                    protocol_version: RPC_VERSION,
                    service_generation: "legacy-generation".into(),
                    components: BTreeMap::from([("daemon".into(), ComponentStateView::Ready)]),
                    capabilities: agent_capabilities(false),
                    agents: Vec::new(),
                    identity: Some(DaemonIdentityView {
                        daemon: running_component_identity_from(
                            "daemon", "0.1.0", None, None, None, UNIX_EPOCH,
                        ),
                        runtime: configured_runtime_identity(None),
                        models: ModelIdentityView {
                            configured: None,
                            observed_response: None,
                        },
                    }),
                },
            },
        );
        let mut legacy = serde_json::to_value(response).unwrap();
        legacy["result"]["status"]
            .as_object_mut()
            .unwrap()
            .remove("identity");
        let decoded: RpcResponse = serde_json::from_value(legacy).unwrap();
        let RpcOutcome::Success { result } = decoded.outcome else {
            panic!("expected success")
        };
        let RpcSuccess::SystemStatus { status } = *result else {
            panic!("expected status")
        };
        assert_eq!(status.service_generation, "legacy-generation");
        assert_eq!(status.components["daemon"], ComponentStateView::Ready);
        assert_eq!(status.identity, None);
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    fn input(repository: &Path) -> GeneralSubmitInput {
        GeneralSubmitInput {
            agent: Some("zcode".into()),
            model: None,
            manifest: GeneralTaskManifest {
                schema: external_core::GENERAL_TASK_SCHEMA.into(),
                agent_id: "daemon-prepared".into(),
                repository: repository.into(),
                permission_mode: external_core::PermissionMode::Plan,
                prompt: "identity fixture".into(),
                write_manifest: vec![],
            },
        }
    }

    #[test]
    fn null_is_rejected_but_omission_round_trips() {
        let mut value = serde_json::to_value(input(Path::new("/repository"))).unwrap();
        value.as_object_mut().unwrap().remove("agent");
        assert!(value.get("model").is_none());
        let omitted: GeneralSubmitInput = serde_json::from_value(value.clone()).unwrap();
        assert!(omitted.agent.is_none() && omitted.model.is_none());
        assert_eq!(serde_json::to_value(omitted).unwrap(), value);
        for field in ["agent", "model"] {
            let mut invalid = value.clone();
            invalid[field] = serde_json::Value::Null;
            assert!(serde_json::from_value::<GeneralSubmitInput>(invalid).is_err());
        }
    }

    fn static_env_guard() -> &'static Mutex<()> {
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn canonical_admission_precedence_and_dsh_gate() {
        let mut input = input(Path::new("/repository"));
        let mut config = AgentConfigSnapshot::default();
        input.agent = None;
        assert_eq!(
            resolve_admission(&input, &config).unwrap_err().code,
            RpcErrorCode::AgentRequired
        );
        input.agent = Some("unknown".into());
        assert_eq!(
            resolve_admission(&input, &config).unwrap_err().code,
            RpcErrorCode::AgentUnknown
        );
        input.agent = Some("dsh".into());
        assert_eq!(
            resolve_admission(&input, &config).unwrap_err().code,
            RpcErrorCode::AgentDisabled
        );
        config.agents.get_mut("dsh").unwrap().enabled = true;
        config.agents.get_mut("dsh").unwrap().spawn_supported = true;
        input.model = Some("opaque-token".into());
        let env_guard = static_env_guard().lock().unwrap();
        let previous_runtime = env::var_os("DSH_RUNTIME_PATH");
        env::set_var("DSH_RUNTIME_PATH", "relative/runtime");
        assert_eq!(
            resolve_admission(&input, &config).unwrap_err().code,
            RpcErrorCode::AgentUnsupported
        );
        let runtime = tempfile::NamedTempFile::new().unwrap();
        env::set_var("DSH_RUNTIME_PATH", runtime.path());
        let identity = resolve_admission(&input, &config).unwrap();
        assert_eq!(identity.agent, "dsh");
        assert_eq!(identity.model.as_deref(), Some("opaque-token"));
        assert_eq!(identity.model_source, "catalog");
        match previous_runtime {
            Some(value) => env::set_var("DSH_RUNTIME_PATH", value),
            None => env::remove_var("DSH_RUNTIME_PATH"),
        }
        drop(env_guard);
        input.agent = Some("zcode".into());
        input.model = Some("model".into());
        config.agents.get_mut("zcode").unwrap().spawn_supported = false;
        assert_eq!(
            resolve_admission(&input, &config).unwrap_err().code,
            RpcErrorCode::ModelSelectionUnsupported
        );
        input.model = Some(" ".into());
        assert_eq!(
            resolve_admission(&input, &config).unwrap_err().code,
            RpcErrorCode::Validation
        );
    }

    #[test]
    fn admission_snapshot_survives_config_change_reopen_and_filtered_pagination() {
        let (directory, service, previous_id) = wait_tests::fixture();
        service
            .store
            .store_task_result(
                &previous_id,
                &external_store::TaskResult {
                    outcome: TaskOutcome::Completed,
                    final_text: "done".into(),
                    partial: false,
                },
            )
            .unwrap();
        let input = input(directory.path());
        let mut config = AgentConfigSnapshot::default();
        config.revision = 41;
        let identity = resolve_admission(&input, &config).unwrap();
        let task = service
            .scheduler
            .enqueue_general_with_admission(&input.manifest, Some(identity.clone()))
            .unwrap()
            .task;
        config.revision = 42;
        config.agents.get_mut("zcode").unwrap().enabled = false;
        assert!(resolve_admission(&input, &config).is_err());
        let reopened = Store::open(directory.path().join("state.sqlite")).unwrap();
        let stored = reopened.get_task(&task.agent_id).unwrap().unwrap();
        let prepared: external_core::PreparedGeneralTask =
            serde_json::from_str(&stored.prepared_launch_json).unwrap();
        prepared.validate_digest().unwrap();
        assert_eq!(prepared.admission.as_ref(), Some(&identity));
        assert_eq!(task_view(stored).input_identity.admission, Some(identity));
        let query = |agent: &str| TaskListQuery {
            agent: Some(agent.into()),
            repository: Some(directory.path().to_string_lossy().into_owned()),
            phase: None,
            outcome: None,
            cursor: None,
            limit: 1,
        };
        let RpcSuccess::TaskListed { tasks, next_cursor } = service
            .dispatch(RpcMethod::TaskList(query("zcode")))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].agent_id, task.agent_id);
        assert!(next_cursor.is_none()); // Unclassified older task does not occupy a filtered page.
        let RpcSuccess::TaskListed { tasks, .. } =
            service.dispatch(RpcMethod::TaskList(query("dsh"))).unwrap()
        else {
            panic!()
        };
        assert!(tasks.is_empty());
    }
}

#[cfg(test)]
mod agent_probe_tests {
    use super::*;
    use crate::{agent_status::AgentProbeBackend, CommandRuntimeFactory, SchedulerConfig};
    use std::process::Command;

    struct FixtureProbe;

    impl AgentProbeBackend for FixtureProbe {
        fn probe(&self, input: &AgentProbeInput) -> AgentProbeEvidence {
            let ready = ScopeEvidence {
                state: EvidenceState::Ready,
                scope: input.scope.clone(),
                version: Some("3.8.1".into()),
                checked_at_ms: 123,
                reason: None,
            };
            AgentProbeEvidence {
                agent: input.agent.clone(),
                config_revision: 0,
                local: ready.clone(),
                auth: ready.clone(),
                hi: ready,
            }
        }
    }

    fn service() -> (tempfile::TempDir, RpcService) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let factory = CommandRuntimeFactory::new(|_: &TaskRecord| -> std::io::Result<Command> {
            panic!("probe fixture must use its dedicated backend")
        });
        let scheduler = Scheduler::new(
            "agent-probe-test",
            store.clone(),
            Arc::new(factory),
            SchedulerConfig::default(),
        )
        .unwrap();
        let evidence = AgentEvidenceStore::with_backend(Arc::new(FixtureProbe));
        let service = RpcService::new_with_service_generation_and_evidence(
            scheduler,
            store,
            "probe-generation".into(),
            evidence,
        )
        .unwrap();
        (directory, service)
    }

    #[test]
    fn status_is_passive_and_explicit_probe_records_scoped_evidence() {
        let (_directory, service) = service();
        let RpcSuccess::SystemStatus { status } =
            service.dispatch(RpcMethod::SystemStatus).unwrap()
        else {
            panic!("expected status")
        };
        assert_eq!(status.agents[0].local.state, ComponentStateView::Unknown);
        assert_eq!(status.agents[0].local.reason.as_deref(), Some("not_probed"));

        let scope = ProbeScope {
            workspace: Some("/workspace-a".into()),
            home: Some("/home-a".into()),
        };
        let RpcSuccess::AgentProbed { evidence, status } = service
            .dispatch(RpcMethod::AgentProbe {
                input: AgentProbeInput {
                    agent: "zcode".into(),
                    through: crate::agent_status::ProbeLayer::Hi,
                    scope: scope.clone(),
                },
            })
            .unwrap()
        else {
            panic!("expected probe")
        };
        assert_eq!(evidence.hi.scope, scope);
        assert_eq!(evidence.config_revision, status.config_revision);
        assert_eq!(status.hi.state, ComponentStateView::Ready);
        assert_eq!(status.hi.checked_at_ms, Some(123));

        let RpcSuccess::SystemStatus { status } =
            service.dispatch(RpcMethod::SystemStatus).unwrap()
        else {
            panic!("expected status")
        };
        let zcode = status
            .agents
            .iter()
            .find(|status| status.agent == "zcode")
            .unwrap();
        assert_eq!(zcode.hi.scope.workspace.as_deref(), Some("/workspace-a"));
        let dsh = status
            .agents
            .iter()
            .find(|status| status.agent == "dsh")
            .unwrap();
        assert!(!dsh.spawn_supported);
        assert_eq!(dsh.hi.state, ComponentStateView::Unknown);
        assert!(dsh.configured);
        assert_eq!(dsh.transport_support.transport, AgentTransportView::DshAcp);
        assert!(!dsh.transport_support.spawn);
        assert!(dsh.permission_modes.is_empty());
        assert_eq!(
            dsh.model_selection.mode,
            AgentModelSelectionModeView::CatalogToken
        );
        assert!(!dsh.model_selection.supported);
    }

    #[test]
    fn config_revision_change_marks_previous_probe_evidence_stale() {
        let (_directory, service) = service();
        let input = AgentProbeInput {
            agent: "zcode".into(),
            through: crate::agent_status::ProbeLayer::Hi,
            scope: ProbeScope::default(),
        };
        let evidence = service.agent_evidence.probe(&input, 7);
        assert_eq!(evidence.config_revision, 7);
        let mut config = AgentConfigSnapshot::default();
        config.revision = 8;
        let status = configured_agent_statuses(&config, &service.agent_evidence)
            .into_iter()
            .find(|status| status.agent == "zcode")
            .unwrap();
        assert_eq!(status.config_revision, 8);
        for layer in [&status.local, &status.auth, &status.hi] {
            assert_eq!(layer.state, ComponentStateView::Unknown);
            assert_eq!(layer.reason.as_deref(), Some("stale_config_revision"));
        }
    }

    #[test]
    fn probe_rejects_unknown_agent_and_relative_scopes_before_execution() {
        let (_directory, service) = service();
        for input in [
            AgentProbeInput {
                agent: "other".into(),
                through: crate::agent_status::ProbeLayer::Local,
                scope: ProbeScope::default(),
            },
            AgentProbeInput {
                agent: "zcode".into(),
                through: crate::agent_status::ProbeLayer::Hi,
                scope: ProbeScope {
                    workspace: Some("relative".into()),
                    home: None,
                },
            },
        ] {
            assert!(service.dispatch(RpcMethod::AgentProbe { input }).is_err());
        }
    }

    #[test]
    fn agent_models_rpc_preserves_native_only_result_and_config_identity() {
        let (_directory, service) = service();
        let RpcSuccess::AgentModels { catalog } = service
            .dispatch(RpcMethod::AgentModels {
                input: AgentModelsInput {
                    agent: "zcode".into(),
                    scope: ProbeScope::default(),
                },
            })
            .unwrap()
        else {
            panic!("expected models result")
        };
        assert!(!catalog.supported);
        assert!(catalog.models.is_empty());
        assert_eq!(catalog.reason.as_deref(), Some("native_only"));
        assert_eq!(catalog.evidence.source, "zcode_native_model");
        assert_eq!(
            catalog.config_revision,
            AgentConfigSnapshot::default().revision
        );
    }

    #[test]
    fn draining_management_methods_are_public_rpc_names() {
        assert!(RpcMethod::is_known("daemon_begin_drain"));
        assert!(RpcMethod::is_known("daemon_drain_status"));
    }

    #[test]
    fn draining_status_read_is_side_effect_free_and_activation_is_once() {
        let (_directory, service) = service();
        let RpcSuccess::DaemonDrainStatus { updater_fired, .. } =
            service.dispatch(RpcMethod::DaemonDrainStatus).unwrap()
        else {
            panic!("status")
        };
        assert!(!updater_fired);
        let RpcSuccess::DaemonDrainStatus { updater_fired, .. } =
            service.dispatch(RpcMethod::DaemonBeginDrain { cancel_active: false }).unwrap()
        else {
            panic!("begin")
        };
        assert!(!updater_fired);
        let RpcSuccess::DaemonDrainStatus { updater_fired, .. } =
            service.dispatch(RpcMethod::DaemonDrainStatus).unwrap()
        else {
            panic!("status")
        };
        assert!(!updater_fired);
        let RpcSuccess::DaemonDrainStatus { updater_fired, .. } =
            service.dispatch(RpcMethod::DaemonActivateReady).unwrap()
        else {
            panic!("activate")
        };
        assert!(updater_fired);
        let RpcSuccess::DaemonDrainStatus { updater_fired, .. } =
            service.dispatch(RpcMethod::DaemonActivateReady).unwrap()
        else {
            panic!("activate")
        };
        assert!(updater_fired);
    }
}
