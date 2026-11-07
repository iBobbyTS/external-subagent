//! Shared adapter facts, not a wire protocol or a task-outcome policy.
//!
//! Adapters own attribution, credential redaction and native interpretation.
//! A publisher assigns one strictly increasing sequence per launch to each fact,
//! including facts derived from the same input. Apply an entire decoded batch
//! before notifying waiters. Native timestamps never order facts.
//! No type here parses raw runtime payloads or reconstructs historical state.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A diagnostic discriminator, never arbitrary payload text. Adapters must pass
/// only a method/type/kind token here. Lexical validation is not credential
/// redaction; payloads, IDs, errors and parameters must never be passed here.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct SafeToken(String);

impl SafeToken {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (value.len() <= 64
            && value
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic)
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b)))
        .then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SafeToken {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?)
            .ok_or_else(|| serde::de::Error::custom("invalid diagnostic token"))
    }
}

macro_rules! identity {
    ($($name:ident),+ $(,)?) => {$(
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);
    )+};
}
identity!(TaskId, LaunchId, SessionId, MessageId, CallId, RequestId);

/// Session identity is deliberately a different type. A local turn belongs to
/// one control operation; adapters must not synthesize a native turn from a session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "origin", content = "id", rename_all = "snake_case")]
pub enum TurnId {
    Native(String),
    Local(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceProtocol {
    CodexAppServer,
    ZcodeSession,
    ZcodeTelemetryV4,
    DshAcpV1,
    AgyStreamJson,
    Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeSource {
    pub protocol: SourceProtocol,
    pub discriminator: Option<SafeToken>,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub call_id: Option<String>,
    /// Only an actual native input identity, never a content hash. Same ID on
    /// multiple derived facts denotes one input, not duplicate derived facts.
    pub event_id: Option<String>,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventContext {
    pub task_id: TaskId,
    pub launch_id: LaunchId,
    /// Strictly increasing within this launch (not reset at turn boundaries).
    pub sequence: u64,
    pub turn_id: Option<TurnId>,
    pub source: NativeSource,
}

/// Metadata of the native input, including when no fact is produced. An
/// abnormal disposition must retain its frame identity for once-per-input
/// diagnostics. This has no second sequence: only facts receive sequences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeInputContext {
    pub task_id: TaskId,
    pub launch_id: LaunchId,
    pub turn_id: Option<TurnId>,
    pub source: NativeSource,
}

impl NativeInputContext {
    pub fn frame_key(&self) -> Option<NativeFrameKey> {
        Some(NativeFrameKey {
            task_id: self.task_id.clone(),
            launch_id: self.launch_id.clone(),
            protocol: self.source.protocol,
            session_id: self.source.session_id.clone(),
            event_id: self.source.event_id.clone()?,
        })
    }
}

/// Replay is scoped by launch and protocol/session. Logical call and phase
/// identity do not participate: two sources can report the same call phase.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NativeFrameKey {
    pub task_id: TaskId,
    pub launch_id: LaunchId,
    pub protocol: SourceProtocol,
    pub session_id: Option<SessionId>,
    pub event_id: String,
}

impl EventContext {
    pub fn input_context(&self) -> NativeInputContext {
        NativeInputContext {
            task_id: self.task_id.clone(),
            launch_id: self.launch_id.clone(),
            turn_id: self.turn_id.clone(),
            source: self.source.clone(),
        }
    }

    pub fn frame_key(&self) -> Option<NativeFrameKey> {
        self.input_context().frame_key()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LogicalCallKey {
    pub task_id: TaskId,
    pub launch_id: LaunchId,
    pub turn_id: TurnId,
    pub call_id: CallId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CallPhaseKey {
    pub call: LogicalCallKey,
    pub phase: ToolPhase,
}

/// Adapter decision. These are not shared-state heuristics or decode failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attribution {
    Current,
    Late,
    Replay,
    ForeignSession,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedEvent {
    pub context: EventContext,
    pub fact: EventFact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum EventFact {
    SessionReady(SessionInfo),
    SessionChanged(SessionInfo),
    TurnStarted {
        turn_id: TurnId,
    },
    TurnSettled(TurnSettlement),
    MessageUpdated(MessageUpdate),
    MessageFinished {
        message_id: MessageId,
    },
    PublicReasoningDelta(PublicReasoning),
    ToolChanged(ToolChange),
    ModelActivityChanged(ModelActivity),
    InputRequested(InputRequest),
    InputResolved(InputResolution),
    ErrorObserved {
        operation: Operation,
        failure: NativeFailure,
    },
    Diagnostic(Diagnostic),
    CoverageGap(CoverageGap),
    RuntimeExited {
        exit_code: Option<i32>,
        signal: Option<i32>,
        stderr_ref: Option<String>,
    },
    TransportFault {
        kind: TransportFaultKind,
        failure: Option<NativeFailure>,
        stderr_ref: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub model: Option<String>,
    /// Omitted capability means unobserved, not unsupported.
    pub capabilities: Vec<CapabilityObservation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    ToolIdentity,
    ToolArguments,
    ToolPhase,
    ToolOutcome,
    PublicReasoning,
    ModelActivity,
    RespondToPermission,
    RespondToQuestion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityObservation {
    pub capability: Capability,
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSettlement {
    pub turn_id: TurnId,
    pub outcome: TurnOutcome,
    pub stop_reason: Option<SafeToken>,
    /// Final message snapshots, where provided, precede this boundary in the
    /// same batch. This optional final text belongs exclusively to this turn.
    pub final_text: Option<String>,
    pub failure: Option<NativeFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "value", rename_all = "snake_case")]
pub enum TextUpdate {
    Delta(String),
    Snapshot(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageUpdate {
    /// Native message ID or adapter-local stream identity, scoped to the turn.
    pub message_id: MessageId,
    pub text: TextUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicReasoning {
    pub message_id: Option<MessageId>,
    pub part_id: Option<String>,
    pub channel: PublicReasoningChannel,
    pub delta: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicReasoningChannel {
    Summary,
    Thought,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPhase {
    Scheduled,
    Started,
    Progress,
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    Unknown,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "value", rename_all = "snake_case")]
pub enum ArgumentUpdate {
    /// Streaming JSON argument text; a later snapshot may replace it.
    Delta(String),
    Snapshot(Value),
    /// Native partial-object update; absent members preserve known arguments.
    MergePatch(Value),
}

/// Missing fields preserve known values. A completion without a start creates
/// the logical call once with no fabricated start time. DONE => Finished with
/// unknown outcome unless the native source supplies explicit result evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolChange {
    pub call_id: CallId,
    pub name: Option<String>,
    pub category: Option<SafeToken>,
    pub arguments: Option<ArgumentUpdate>,
    pub phase: Option<ToolPhase>,
    pub outcome: Option<ToolOutcome>,
    pub failure: Option<NativeFailure>,
    pub denied: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPhase {
    Started,
    Completed,
    Failed,
    RetryScheduled,
    StreamStalled,
}

/// Native retry information is descriptive. `will_retry` is not permission for
/// ES or its caller to retry, and a failed attempt is not a failed turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeRetry {
    pub will_retry: Option<bool>,
    pub retryable: Option<bool>,
    pub attempt: Option<u64>,
    pub max_attempts: Option<u64>,
    pub next_attempt: Option<u64>,
    pub delay_ms: Option<u64>,
    pub reset_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelActivity {
    pub request_id: Option<RequestId>,
    pub phase: ModelPhase,
    pub retry: Option<NativeRetry>,
    pub failure: Option<NativeFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputRequest {
    /// Preserve integer versus string RPC identity for native responses.
    pub request_id: crate::WireId,
    pub kind: InputKind,
    pub respondable: bool,
    /// Full public question/options, credential-redacted by adapter. Never
    /// included in diagnostics. The native response contract stays adapter-owned.
    pub question: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Permission,
    Question,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputResolution {
    pub request_id: crate::WireId,
    pub outcome: InputOutcome,
    pub call_id: Option<CallId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputOutcome {
    Answered,
    Allowed,
    Denied,
    Cancelled,
}

/// Complete received native error, redacted only for credentials by the adapter
/// before construction. No truncation, generic replacement, stringify/parse or
/// projection is permitted in place of `original`. Transport hard limits are
/// reported as faults rather than claiming complete delivery. This type is also
/// used by command errors; normal RPC responses need not become EventFacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeFailure {
    pub original: Value,
    pub projection: Option<FailureProjection>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureProjection {
    pub code: Option<Value>,
    pub status: Option<i64>,
    pub retry: Option<NativeRetry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Initialize,
    SelectModel,
    Send,
    Turn,
    Tool,
    Input,
    Transport,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportFaultKind {
    InvalidFrame,
    FrameLimit,
    Disconnected,
    Io,
}

/// Controlled vocabulary; no free-text native errors or raw status strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticReason {
    UnknownSemantics,
    Malformed,
    Truncated,
    Dropped,
    PermissionDenied,
    AuxiliaryTerminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageDimension {
    Text,
    Tools,
    PublicReasoning,
    ModelActivity,
    Input,
    Turn,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "scope", content = "dimensions", rename_all = "snake_case")]
pub enum Impact {
    Unknown,
    Known(Vec<CoverageDimension>),
}

/// Only current affected correlations influence telemetry status. History gaps
/// remain after settlement; unrelated normal frames do not repair either scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", content = "id", rename_all = "snake_case")]
pub enum ActivityScope {
    None,
    Turn(TurnId),
    ModelRequest(RequestId),
    Tool(CallId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageGap {
    pub reason: LossReason,
    pub impact: Impact,
    pub activity: ActivityScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub reason: DiagnosticReason,
    pub impact: Impact,
    pub discriminator: Option<SafeToken>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IgnoreReason {
    Usage,
    AccountLimits,
    Ui,
    Configuration,
    PlanOrDiff,
    Compaction,
    StreamBoundary,
    NoConsumer,
    Late,
    Replay,
    ForeignSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LossReason {
    UnknownSemantics,
    Malformed,
    Truncated,
    Dropped,
}

/// Classification of one native input, independent of WireMessage::UnknownEvent.
/// Unknown is uncertainty, not evidence of actual dropped events. Recognized
/// ignored inputs generate neither loss nor a per-frame diagnostic record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum DecodeDisposition {
    Consumed,
    RecognizedIgnored(IgnoreReason),
    Unknown(Diagnostic),
    InvalidLost(CoverageGap),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispositionKind {
    Consumed,
    RecognizedIgnored,
    Unknown,
    InvalidLost,
}

impl DecodeDisposition {
    pub fn kind(&self) -> DispositionKind {
        match self {
            Self::Consumed => DispositionKind::Consumed,
            Self::RecognizedIgnored(_) => DispositionKind::RecognizedIgnored,
            Self::Unknown(_) => DispositionKind::Unknown,
            Self::InvalidLost(_) => DispositionKind::InvalidLost,
        }
    }
}

/// Ordered facts from one native input. `disposition` is recorded once per input
/// (using frame identity only when available), never once per derived fact.
/// Unknown/invalid inputs may carry safe Diagnostic/CoverageGap facts; partial
/// valid facts from a damaged input remain ordered before their gap record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "DecodedInputFields", into = "DecodedInputFields")]
pub struct DecodedInput {
    input: NativeInputContext,
    attribution: Attribution,
    disposition: DecodeDisposition,
    events: Vec<NormalizedEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodedInputFields {
    input: NativeInputContext,
    attribution: Attribution,
    disposition: DecodeDisposition,
    events: Vec<NormalizedEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchError {
    EmptyConsumed,
    IgnoredFacts,
    AttributionMismatch,
    MixedLaunch,
    Unordered,
    DispositionReasonMismatch,
}

impl std::fmt::Display for BatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid decoded batch: {self:?}")
    }
}

impl std::error::Error for BatchError {}

impl DecodedInput {
    pub fn new(
        input: NativeInputContext,
        attribution: Attribution,
        disposition: DecodeDisposition,
        events: Vec<NormalizedEvent>,
    ) -> Result<Self, BatchError> {
        match (&disposition, events.is_empty()) {
            (DecodeDisposition::Consumed, true) => return Err(BatchError::EmptyConsumed),
            (DecodeDisposition::RecognizedIgnored(_), false) => {
                return Err(BatchError::IgnoredFacts)
            }
            _ => {}
        }
        match &disposition {
            DecodeDisposition::Unknown(diagnostic)
                if diagnostic.reason != DiagnosticReason::UnknownSemantics =>
            {
                return Err(BatchError::DispositionReasonMismatch)
            }
            DecodeDisposition::InvalidLost(gap) if gap.reason == LossReason::UnknownSemantics => {
                return Err(BatchError::DispositionReasonMismatch)
            }
            _ => {}
        }
        let expected_ignore = match attribution {
            Attribution::Current => None,
            Attribution::Late => Some(IgnoreReason::Late),
            Attribution::Replay => Some(IgnoreReason::Replay),
            Attribution::ForeignSession => Some(IgnoreReason::ForeignSession),
        };
        let attribution_ignore = match disposition {
            DecodeDisposition::RecognizedIgnored(IgnoreReason::Late) => Some(IgnoreReason::Late),
            DecodeDisposition::RecognizedIgnored(IgnoreReason::Replay) => {
                Some(IgnoreReason::Replay)
            }
            DecodeDisposition::RecognizedIgnored(IgnoreReason::ForeignSession) => {
                Some(IgnoreReason::ForeignSession)
            }
            _ => None,
        };
        if expected_ignore != attribution_ignore {
            return Err(BatchError::AttributionMismatch);
        }
        if events.iter().any(|event| {
            event.context.task_id != input.task_id || event.context.launch_id != input.launch_id
        }) {
            return Err(BatchError::MixedLaunch);
        }
        for pair in events.windows(2) {
            let (a, b) = (&pair[0].context, &pair[1].context);
            if a.task_id != b.task_id || a.launch_id != b.launch_id {
                return Err(BatchError::MixedLaunch);
            }
            if a.sequence >= b.sequence {
                return Err(BatchError::Unordered);
            }
        }
        Ok(Self {
            input,
            attribution,
            disposition,
            events,
        })
    }

    pub fn input(&self) -> &NativeInputContext {
        &self.input
    }

    pub fn attribution(&self) -> Attribution {
        self.attribution
    }
    pub fn disposition(&self) -> &DecodeDisposition {
        &self.disposition
    }
    pub fn events(&self) -> &[NormalizedEvent] {
        &self.events
    }
    pub fn into_events(self) -> Vec<NormalizedEvent> {
        self.events
    }
}

impl TryFrom<DecodedInputFields> for DecodedInput {
    type Error = BatchError;
    fn try_from(fields: DecodedInputFields) -> Result<Self, Self::Error> {
        Self::new(
            fields.input,
            fields.attribution,
            fields.disposition,
            fields.events,
        )
    }
}

impl From<DecodedInput> for DecodedInputFields {
    fn from(input: DecodedInput) -> Self {
        Self {
            input: input.input,
            attribution: input.attribution,
            disposition: input.disposition,
            events: input.events,
        }
    }
}

/// Safe source type for lifecycle summaries; consumers choose only this enum
/// and safe diagnostics, never serialize EventFact's content as a summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleCategory {
    Session,
    Turn,
    Message,
    PublicReasoning,
    Tool,
    ModelActivity,
    Input,
    Error,
    Diagnostic,
    Coverage,
    Runtime,
    Transport,
}

/// Content-free input to the existing lifecycle sink. S09 owns generation;
/// S08 owns bounded aggregation, counts and receive times. No raw IDs, payloads,
/// body, arguments or free-text errors can be carried by this summary type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleSummary {
    pub category: Option<LifecycleCategory>,
    pub source: SourceProtocol,
    pub discriminator: Option<SafeToken>,
    pub disposition: DispositionKind,
    pub reason: Option<DiagnosticReason>,
    pub impact: Option<Impact>,
}

impl EventFact {
    pub fn lifecycle_category(&self) -> LifecycleCategory {
        match self {
            Self::SessionReady(_) | Self::SessionChanged(_) => LifecycleCategory::Session,
            Self::TurnStarted { .. } | Self::TurnSettled(_) => LifecycleCategory::Turn,
            Self::MessageUpdated(_) | Self::MessageFinished { .. } => LifecycleCategory::Message,
            Self::PublicReasoningDelta(_) => LifecycleCategory::PublicReasoning,
            Self::ToolChanged(_) => LifecycleCategory::Tool,
            Self::ModelActivityChanged(_) => LifecycleCategory::ModelActivity,
            Self::InputRequested(_) | Self::InputResolved(_) => LifecycleCategory::Input,
            Self::ErrorObserved { .. } => LifecycleCategory::Error,
            Self::Diagnostic(_) => LifecycleCategory::Diagnostic,
            Self::CoverageGap(_) => LifecycleCategory::Coverage,
            Self::RuntimeExited { .. } => LifecycleCategory::Runtime,
            Self::TransportFault { .. } => LifecycleCategory::Transport,
        }
    }
}

#[cfg(test)]
mod tests;
