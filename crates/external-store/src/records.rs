use crate::error::{StoreError, StoreResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskPhase {
    Queued,
    Preparing,
    Running,
    WaitingInput,
    Cancelling,
    Terminal,
}

impl TaskPhase {
    pub fn is_terminal(self) -> bool {
        self == Self::Terminal
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "QUEUED",
            Self::Preparing => "PREPARING",
            Self::Running => "RUNNING",
            Self::WaitingInput => "WAITING_INPUT",
            Self::Cancelling => "CANCELLING",
            Self::Terminal => "TERMINAL",
        }
    }

    pub(crate) fn parse(value: &str) -> StoreResult<Self> {
        match value {
            "QUEUED" => Ok(Self::Queued),
            "PREPARING" => Ok(Self::Preparing),
            "RUNNING" => Ok(Self::Running),
            "WAITING_INPUT" => Ok(Self::WaitingInput),
            "CANCELLING" => Ok(Self::Cancelling),
            "TERMINAL" => Ok(Self::Terminal),
            other => Err(StoreError::InvalidState(format!(
                "unknown task phase {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskOutcome {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    RuntimeLost,
    ResultInvalid,
}

impl TaskOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
            Self::Cancelled => "CANCELLED",
            Self::TimedOut => "TIMED_OUT",
            Self::RuntimeLost => "RUNTIME_LOST",
            Self::ResultInvalid => "RESULT_INVALID",
        }
    }

    pub(crate) fn parse(value: &str) -> StoreResult<Self> {
        match value {
            "COMPLETED" => Ok(Self::Completed),
            "FAILED" => Ok(Self::Failed),
            "CANCELLED" => Ok(Self::Cancelled),
            "TIMED_OUT" => Ok(Self::TimedOut),
            "RUNTIME_LOST" => Ok(Self::RuntimeLost),
            "RESULT_INVALID" => Ok(Self::ResultInvalid),
            other => Err(StoreError::InvalidState(format!(
                "unknown task outcome {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTask {
    pub agent_id: String,
    pub repository: String,
    pub workspace_path: String,
    pub runtime_hash: Option<String>,
    pub prepared_launch_json: String,
    pub initial_prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    pub agent_id: String,
    pub repository: String,
    pub phase: TaskPhase,
    pub outcome: Option<TaskOutcome>,
    pub workspace_path: String,
    pub runtime_hash: Option<String>,
    pub prepared_launch_json: String,
    pub initial_prompt: String,
    pub owner_id: Option<String>,
    pub owner_epoch: u64,
    pub close_requested: bool,
    pub stop_requested: bool,
    pub last_event_seq: u64,
    pub failure_code: Option<String>,
    pub failure_message: Option<String>,
    pub runtime_agent_id: Option<String>,
    pub session_id: Option<String>,
    pub turn_state: TurnState,
    pub process_identity: Option<StoredProcessIdentity>,
    pub closed_at: Option<i64>,
    pub reaped_at: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TaskResult {
    pub outcome: TaskOutcome,
    pub final_text: String,
    pub partial: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTaskResult {
    pub result: TaskResult,
    pub result_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskQueryScope<'a> {
    pub repository: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPageFilter {
    pub agent: Option<String>,
    pub phase: Option<TaskPhase>,
    pub outcome: Option<TaskOutcome>,
}

/// Inclusive time/workspace selector used by the reduced adapter query API.
/// `start_ms` is compared with `created_at`; `end_ms` is compared with
/// `completed_at` and therefore only matches tasks that have completed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskWindowQuery {
    pub agent_id: Option<String>,
    pub workspace_path: Option<String>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPage {
    pub tasks: Vec<TaskRecord>,
    pub next_cursor: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    Idle,
    Active,
    Failed,
}

impl TurnState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Active => "ACTIVE",
            Self::Failed => "FAILED",
        }
    }

    pub(crate) fn parse(value: &str) -> StoreResult<Self> {
        match value {
            "IDLE" => Ok(Self::Idle),
            "ACTIVE" => Ok(Self::Active),
            "FAILED" => Ok(Self::Failed),
            other => Err(StoreError::InvalidState(format!(
                "unknown turn state {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredProcessIdentity {
    pub pid: u32,
    pub process_group_id: i32,
    pub uid: u32,
    pub start_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskClaim {
    pub task: TaskRecord,
    pub owner_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleWrite {
    pub agent_id: String,
    pub runtime_agent_id: String,
    pub owner_epoch: u64,
    pub source_sequence: u64,
    pub event_type: String,
    pub turn_id: Option<String>,
    pub payload_json: String,
    pub redaction_level: String,
    pub terminal: Option<TerminalUpdate>,
    pub turn_state: Option<TurnState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalUpdate {
    pub outcome: TaskOutcome,
    pub failure_code: Option<String>,
    pub failure_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlDecision {
    pub phase: TaskPhase,
    pub outcome: Option<TaskOutcome>,
    pub owner_epoch: u64,
    pub needs_runtime_stop: bool,
    pub prior_stop_or_close: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageState {
    Queued,
    Sending,
    Delivered,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMessage {
    pub message_id: String,
    pub agent_id: String,
    pub mode: String,
    pub content: String,
    pub state: MessageState,
    pub target_turn_id: Option<String>,
    pub failure_code: Option<String>,
    pub created_at: i64,
    pub delivered_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingRequestState {
    Pending,
    Sending,
    Responded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPendingRequest {
    pub request_id: String,
    pub agent_id: String,
    pub correlation_id: String,
    pub request_type: String,
    pub payload_json: String,
    pub state: PendingRequestState,
    pub response_decision: Option<String>,
    pub response_content: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingResponseClaimDisposition {
    Claimed,
    TaskStopping,
    NotPending(PendingRequestState),
    NotFound,
}
