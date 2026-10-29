//! Typed wire types and protocol limits for the daemon RPC surface.
//!
//! Extracted mechanically from the former single-file `rpc` module; the
//! facade at `crate::rpc` keeps every historical path importable.
use super::views::{
    AgentStatusView, MessageDispositionView, MessageReceiptView, PendingRequestView,
    ResponseOutcomeView, SystemStatusView, TaskActivityView, TaskHeaderView, TaskObservationView,
    TaskResultView, TaskView,
};
use crate::agent_status::{
    AgentModelsInput, AgentModelsOutput, AgentProbeEvidence, AgentProbeInput,
};
use external_core::GeneralTaskManifest;
use external_store::{TaskOutcome, TaskPhase};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
/// Upper bound on the number of agent ids accepted by a list-mode wait.
pub const MAX_WAIT_AGENTS: usize = 16;
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
    /// `system_status` with an explicit refresh request. A distinct variant —
    /// not a field on `SystemStatus` — keeps the historical unit-variant wire
    /// shape (no `params` key) byte-compatible for older CLI frames.
    SystemStatusRefresh {
        #[serde(default)]
        probe: bool,
    },
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
                | "system_status_refresh"
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
    pub profile: Option<String>,
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
    #[serde(
        default,
        deserialize_with = "optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub effort: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub developer_instructions: Option<String>,
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
    pub agent_id: WaitTarget,
    #[serde(default = "default_wait_time")]
    pub wait_time: u64,
    #[serde(default)]
    pub message_id: Option<String>,
}

/// A wait targets either one agent (the historical scalar shape) or an
/// ordered list of agents. The wire form is untagged, so a scalar request
/// keeps its exact historical encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WaitTarget {
    One(String),
    Many(Vec<String>),
}

impl WaitTarget {
    pub fn as_one(&self) -> Option<&str> {
        match self {
            Self::One(id) => Some(id),
            Self::Many(_) => None,
        }
    }
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
    TaskWait(TaskWaitBody),
    /// List-mode wait response. On the wire this serializes as a FLAT object
    /// (via the custom `wait_many` bridge): one key per distinct awaited
    /// agent id mapping to that agent's wait body, plus one `error` key
    /// mapping every problematic id (duplicated, unknown) to its marker.
    /// A duplicated id is still awaited and returned under its own key; its
    /// repetition is only reported under `error`.
    TaskWaitMany {
        #[serde(flatten, with = "wait_many")]
        waits: TaskWaitManyBody,
    },
    TaskResult {
        task: TaskView,
        result: Option<TaskResultView>,
        /// Present only for a failed terminal result whose runtime version is a
        /// known mismatch; carries the byte-exact advisory.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instruction: Option<String>,
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

/// The six-field per-agent wait projection shared by the scalar and list
/// responses. `RpcSuccess::TaskWait` wraps it flat, so the scalar wire shape
/// is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskWaitBody {
    pub task: TaskHeaderView,
    pub pending_requests: Vec<PendingRequestView>,
    pub activity: TaskActivityView,
    pub result: Option<TaskResultView>,
    pub timed_out: bool,
    pub message_receipt: Option<MessageReceiptView>,
}

/// List-mode wait payload: one wait body per distinct awaited id, plus the
/// concentrated error map for problematic ids (`duplicated` / `unknown_agent`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskWaitManyBody {
    /// Caller-order per-agent bodies. Serialized flat (not nested under a
    /// `waits` key): the wire object is keyed directly by agent id.
    #[serde(skip)]
    pub bodies: Vec<(String, TaskWaitBody)>,
    /// Every problematic id from the request and why it was not awaited
    /// normally. A duplicated id still gets its wait body under its own
    /// key; here it is only flagged.
    pub error: BTreeMap<String, String>,
}

/// Serde bridge for `TaskWaitMany`: the wire form is one flat JSON object —
/// `{"<agent_id>": <wait body>, ..., "error": {"<id>": "duplicated", ...}}`.
/// Agent ids are 8-digit numeric strings, so the reserved `error` key can
/// never collide with an id key.
mod wait_many {
    use super::{TaskWaitBody, TaskWaitManyBody};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub(super) fn serialize<S: Serializer>(
        body: &TaskWaitManyBody,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut map = serde_json::Map::new();
        for (id, wait) in &body.bodies {
            map.insert(id.clone(), serde_json::to_value(wait).map_err(|_| {
                serde::ser::Error::custom("wait body serialization failed")
            })?);
        }
        if !body.error.is_empty() {
            map.insert(
                "error".to_owned(),
                serde_json::to_value(&body.error).map_err(|_| {
                    serde::ser::Error::custom("error map serialization failed")
                })?,
            );
        }
        map.serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<TaskWaitManyBody, D::Error> {
        let map = BTreeMap::<String, serde_json::Value>::deserialize(deserializer)?;
        let mut body = TaskWaitManyBody::default();
        for (key, value) in map {
            if key == "error" {
                body.error = serde_json::from_value(value)
                    .map_err(|_| serde::de::Error::custom("error map deserialization failed"))?;
            } else {
                let wait: TaskWaitBody = serde_json::from_value(value).map_err(|_| {
                    serde::de::Error::custom("wait body deserialization failed")
                })?;
                body.bodies.push((key, wait));
            }
        }
        Ok(body)
    }
}
