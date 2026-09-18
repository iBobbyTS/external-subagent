//! Typed wire types and protocol limits for the daemon RPC surface.
//!
//! Extracted mechanically from the former single-file `rpc` module; the
//! facade at `crate::rpc` keeps every historical path importable.
use super::views::{
    AgentStatusView, MessageDispositionView, MessageReceiptView, PendingRequestView,
    ResponseOutcomeView, SystemStatusView, TaskActivityView, TaskObservationView, TaskResultView,
    TaskView,
};
use crate::agent_status::{
    AgentModelsInput, AgentModelsOutput, AgentProbeEvidence, AgentProbeInput,
};
use external_core::GeneralTaskManifest;
use external_store::{TaskOutcome, TaskPhase};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::errors::RpcError;

/// Diagnostic-only MCP tool surface version reported by status. It never
/// participates in request admission.
pub const MCP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MAX_REQUEST_FRAME_BYTES: usize = 512 * 1024;
pub const MAX_RESPONSE_FRAME_BYTES: usize = 2 * 1024 * 1024;
pub(super) const MAX_REQUEST_ID_BYTES: usize = 128;
pub const MAX_LIST_TASKS: usize = 100;
pub const MAX_PENDING_REQUESTS: usize = 100;
/// A result page is capped below the transport frame cap so that even the
/// worst-case JSON escaping (one input byte becoming a six-byte `\\u00XX`
/// escape), the response envelope, and the trailing newline fit in one frame.
pub const MAX_RESULT_CHUNK_BYTES: usize = 256 * 1024;
pub const MAX_WAIT: Duration = Duration::from_secs(299);
pub const DEFAULT_WAIT_TIME: u64 = 290;
pub const RPC_TRANSPORT_SUPPORTED: bool = cfg!(unix);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RpcRequest {
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
    DaemonAbortDrain,
    DaemonActivateReady,
    AgentProbe(AgentProbeInput),
    AgentModels(AgentModelsInput),
    SubmitGeneral(GeneralSubmitInput),
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
    pub(super) fn is_known(name: &str) -> bool {
        matches!(
            name,
            "system_status"
                | "daemon_begin_drain"
                | "daemon_drain_status"
                | "daemon_abort_drain"
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
    #[serde(alias = "subagent")]
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
    #[serde(alias = "subagent")]
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
    #[serde(default)]
    pub message_id: Option<String>,
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
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Answer => "answer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcResponse {
    pub request_id: Option<String>,
    #[serde(flatten)]
    pub outcome: RpcOutcome,
}

impl RpcResponse {
    pub fn success(request_id: String, result: RpcSuccess) -> Self {
        Self {
            request_id: Some(request_id),
            outcome: RpcOutcome::Success {
                result: Box::new(result),
            },
        }
    }

    pub fn error(request_id: Option<String>, error: RpcError) -> Self {
        Self {
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
    },
    TaskListed {
        tasks: Vec<TaskView>,
        next_cursor: Option<String>,
    },
    TaskWait {
        task: TaskView,
        pending_requests: Vec<PendingRequestView>,
        result_available: bool,
        activity: TaskActivityView,
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

fn default_result_limit() -> usize {
    MAX_RESULT_CHUNK_BYTES
}
