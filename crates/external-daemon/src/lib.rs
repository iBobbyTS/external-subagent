use external_contract::{
    event_type, normalized_zai_model, offered_permission_response, turn_id_from_result,
    CreateSessionParams, LifecycleOrder, ResumeSessionParams, RuntimePreferences, SendParams,
    SessionCreateProjection, SessionParams, StdioMcpServer, SubscribeParams, WireId, WireMessage,
    WorkspaceRef, INTERACTION_REQUEST_PERMISSION, INTERACTION_REQUEST_USER_INPUT, SESSION_CREATE,
    SESSION_REQUEST_RUNTIME_PREFERENCES, SESSION_RESUME, SESSION_SEND, SESSION_STOP,
    SESSION_SUBSCRIBE,
};
use external_runtime::{
    observe_process, observe_process_group, stop_and_reap_persisted_process_group, ChildExit,
    Driver, Inbound, ProcessIdentity, RequestError, StopOutcome,
};
use external_store::{
    LifecycleWrite, MessageState, NewTask, PendingRequestState, PendingResponseClaimDisposition,
    Store, StoreError, StoredMessage, StoredProcessIdentity, TaskClaim, TaskOutcome, TaskPhase,
    TaskRecord, TaskResult, TaskSubmissionDisposition, TurnState,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt, fs, io,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{sync_channel, SyncSender},
        Arc, Condvar, Mutex, MutexGuard, OnceLock, TryLockError,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub mod agent_status;
pub mod mcp;
pub mod observation;
pub mod rpc;
mod scheduler;
use external_core::{
    general_launch_prompt, CompletionOutcome, GeneralCompletion, GeneralFinalizer,
    GeneralTaskManifest, GeneralTaskPreparer, PolicyLauncher, PreparedGeneralTask,
    ValidatedPermissionDenial,
};
pub use scheduler::configure_diagnostic_log;
use scheduler::{
    bounded_error, bounded_prefix, runtime_failure_record, update_latest_failure,
    DIAGNOSTIC_FILE_BYTES, DIAGNOSTIC_QUEUE_CAPACITY, DIAGNOSTIC_RECORD_BYTES,
};
pub use scheduler::{SchedulerConfig, SchedulerError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeLoss {
    InvalidIdentity,
    UnsupportedIdentity,
    MissingLeader,
    IdentityMismatch,
    UnknownMembership,
    SessionLost,
    StopFailed(String),
    EventStreamLost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeTerminal {
    Stopped(StopOutcome),
    Completed(StopOutcome),
    FailedTurn(StopOutcome),
    Exited(ChildExit),
    FailedRuntimeLost(RuntimeLoss),
    Orphaned(RuntimeLoss),
}

fn terminal_proves_process_group_reaped(terminal: &RuntimeTerminal) -> bool {
    matches!(
        terminal,
        RuntimeTerminal::Stopped(_)
            | RuntimeTerminal::Completed(_)
            | RuntimeTerminal::FailedTurn(_)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnBoundary {
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSnapshot {
    pub generation: u64,
    pub active: bool,
    pub boundary: Option<TurnBoundary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeActivitySnapshot {
    pub turn: TurnSnapshot,
    pub model_request_elapsed: Option<Duration>,
    pub transport_idle_elapsed: Option<Duration>,
}

const PASSIVE_ACTIVITY_WINDOW: Duration = Duration::from_secs(60);
const MAX_ACTIVITY_IDENTITIES: usize = 65_536;
const MAX_LATEST_TEXT_BYTES: usize = 8 * 1024;
const MAX_ACTIVITY_ID_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassiveToolKind {
    Read,
    Bash,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassiveActiveTool {
    pub tool_call_id: String,
    pub kind: PassiveToolKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassiveActivityWindow {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassiveActivitySnapshot {
    pub revision: u64,
    pub last_runtime_event_at: Option<u64>,
    pub last_activity_age_ms: Option<u64>,
    pub model_request_active: bool,
    pub model_request_age_ms: Option<u64>,
    pub model_last_delta_age_ms: Option<u64>,
    pub latest_text_tail: String,
    pub latest_text_updated_at: Option<u64>,
    pub latest_text_truncated: bool,
    /// Latest assistant message that has reached an explicit finish/done
    /// boundary. In-flight deltas are deliberately excluded.
    pub latest_progress: Option<String>,
    pub active_tools: Vec<PassiveActiveTool>,
    pub(crate) oldest_active_tool_age_ms: Option<u64>,
    pub window_60s: PassiveActivityWindow,
    pub telemetry_degraded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivitySource {
    Session,
    Telemetry,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivitySampleKind {
    ReasoningDelta { bytes: u64 },
    TextDelta { bytes: u64 },
    ToolStarted { kind: PassiveToolKind },
    ToolCompleted,
    ToolFailed,
}

#[derive(Debug, Clone)]
struct ActivitySample {
    source: ActivitySource,
    observed_at: Instant,
    kind: ActivitySampleKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivityTransition {
    ModelStarted,
    ModelCompleted,
    ToolScheduled,
    ToolStarted,
    ToolCompleted,
    ToolFailed,
    PermissionRequested,
    PermissionResolved,
    TurnStarted,
    TurnCompleted,
    TurnFailed,
}

struct ParsedActivity {
    source: ActivitySource,
    identity: Option<String>,
    stream_key: Option<String>,
    sample: Option<ActivitySampleKind>,
    text_delta: Option<String>,
    transition: Option<ActivityTransition>,
    request_id: Option<String>,
    tool_call_id: Option<String>,
    tool_kind: PassiveToolKind,
    telemetry_known: bool,
    assistant_message_id: Option<String>,
    message_finished: bool,
    terminal_response: Option<String>,
}

impl ParsedActivity {
    fn runtime() -> Self {
        Self {
            source: ActivitySource::Runtime,
            identity: None,
            stream_key: None,
            sample: None,
            text_delta: None,
            transition: None,
            request_id: None,
            tool_call_id: None,
            tool_kind: PassiveToolKind::Other,
            telemetry_known: true,
            assistant_message_id: None,
            message_finished: false,
            terminal_response: None,
        }
    }
}

#[derive(Default)]
struct PassiveActivityState {
    revision: u64,
    last_runtime_event_at: Option<(Instant, u64)>,
    active_model_requests: HashMap<String, Instant>,
    last_model_delta_at: Option<Instant>,
    latest_text_tail: String,
    latest_text_updated_at: Option<u64>,
    latest_text_truncated: bool,
    assistant_buffers: HashMap<String, String>,
    latest_progress: Option<String>,
    terminal_text: String,
    active_tools: HashMap<String, (PassiveToolKind, Instant)>,
    samples: HashMap<String, ActivitySample>,
    sample_order: VecDeque<String>,
    telemetry_degraded: bool,
    observation: observation::ObservationState,
}

struct PassiveActivityTracker {
    state: Mutex<PassiveActivityState>,
    changed: Condvar,
    runtime_source_verified: AtomicBool,
}

impl PassiveActivityTracker {
    fn new(runtime_source_verified: bool) -> Self {
        let state = PassiveActivityState::default();
        Self {
            state: Mutex::new(state),
            changed: Condvar::new(),
            runtime_source_verified: AtomicBool::new(runtime_source_verified),
        }
    }

    fn observe(&self, event: &RuntimeEvent) {
        self.observe_at(event, Instant::now(), activity_wall_now_millis());
    }

    fn observe_at(&self, event: &RuntimeEvent, now: Instant, wall_now_ms: u64) {
        let mut state = self.state.lock().unwrap();
        match event {
            RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(event))) => {
                state
                    .observation
                    .observe_message(&event.method, &event.params);
            }
            RuntimeEvent::Driver(Inbound::Message(WireMessage::UnknownEvent { method, raw })) => {
                let params = raw.get("params").unwrap_or(&serde_json::Value::Null);
                state.observation.observe_message(method, params);
            }
            RuntimeEvent::Driver(Inbound::Malformed(_) | Inbound::OversizedLine { .. }) => {
                state.observation.observe_loss();
            }
            _ => {}
        }
        state.revision = state.revision.saturating_add(1);
        state.last_runtime_event_at = Some((now, wall_now_ms));
        let parsed = parse_passive_activity(event);
        if parsed.source == ActivitySource::Telemetry && !parsed.telemetry_known {
            state.telemetry_degraded = true;
        }

        let mut admitted = true;
        if admitted {
            if let (Some(identity), Some(sample)) = (parsed.identity.as_ref(), parsed.sample) {
                let replace = match state.samples.get(identity) {
                    Some(existing) => {
                        existing.source == ActivitySource::Telemetry
                            && parsed.source == ActivitySource::Session
                    }
                    None => true,
                };
                if replace {
                    if !state.samples.contains_key(identity) {
                        state.sample_order.push_back(identity.clone());
                    }
                    state.samples.insert(
                        identity.clone(),
                        ActivitySample {
                            source: parsed.source,
                            observed_at: now,
                            kind: sample,
                        },
                    );
                } else {
                    admitted = false;
                }
            }
        }

        while state.sample_order.len() > MAX_ACTIVITY_IDENTITIES {
            if let Some(identity) = state.sample_order.pop_front() {
                state.samples.remove(&identity);
            }
        }

        if admitted
            && matches!(
                parsed.sample,
                Some(
                    ActivitySampleKind::ReasoningDelta { .. }
                        | ActivitySampleKind::TextDelta { .. }
                )
            )
        {
            state.last_model_delta_at = Some(now);
        }
        if admitted {
            if let Some(delta) = parsed.text_delta.as_deref() {
                append_latest_text(&mut state, delta, wall_now_ms);
                if let Some(message_id) = parsed.assistant_message_id.as_deref() {
                    let buffer = state
                        .assistant_buffers
                        .entry(message_id.to_owned())
                        .or_default();
                    buffer.push_str(delta);
                    if buffer.len() > MAX_LATEST_TEXT_BYTES {
                        let mut keep_from = buffer.len().saturating_sub(MAX_LATEST_TEXT_BYTES);
                        while keep_from < buffer.len() && !buffer.is_char_boundary(keep_from) {
                            keep_from += 1;
                        }
                        *buffer = buffer[keep_from..].to_owned();
                    }
                }
            }
            if parsed.message_finished {
                if let Some(message_id) = parsed.assistant_message_id.as_deref() {
                    if let Some(buffer) = state.assistant_buffers.remove(message_id) {
                        state.latest_progress = Some(buffer);
                    }
                }
            }
            if let Some(response) = parsed.terminal_response.as_deref() {
                state.terminal_text = response.to_owned();
            }
        }

        match parsed.transition {
            Some(ActivityTransition::ModelStarted) => {
                let request_id = parsed.request_id.unwrap_or_else(|| "model-request".into());
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    state.active_model_requests.entry(request_id)
                {
                    entry.insert(now);
                    state.last_model_delta_at = Some(now);
                }
            }
            Some(ActivityTransition::ModelCompleted) => {
                if let Some(request_id) = parsed.request_id.as_deref() {
                    state.active_model_requests.remove(request_id);
                } else {
                    state.active_model_requests.clear();
                }
            }
            Some(ActivityTransition::ToolScheduled | ActivityTransition::ToolStarted) => {
                if let Some(tool_call_id) = parsed.tool_call_id {
                    state
                        .active_tools
                        .entry(tool_call_id)
                        .or_insert((parsed.tool_kind, now));
                }
            }
            Some(
                ActivityTransition::ToolCompleted
                | ActivityTransition::ToolFailed
                | ActivityTransition::PermissionResolved,
            ) => {
                if let Some(tool_call_id) = parsed.tool_call_id {
                    state.active_tools.remove(&tool_call_id);
                }
            }
            Some(ActivityTransition::TurnCompleted | ActivityTransition::TurnFailed) => {
                state.active_model_requests.clear();
                state.active_tools.clear();
            }
            Some(ActivityTransition::PermissionRequested | ActivityTransition::TurnStarted)
            | None => {}
        }
        self.changed.notify_all();
    }

    fn snapshot(&self) -> PassiveActivitySnapshot {
        self.snapshot_at(Instant::now())
    }

    fn snapshot_at(&self, now: Instant) -> PassiveActivitySnapshot {
        let state = self.state.lock().unwrap();
        let mut window = PassiveActivityWindow::default();
        for sample in state.samples.values() {
            if now.saturating_duration_since(sample.observed_at) > PASSIVE_ACTIVITY_WINDOW {
                continue;
            }
            match sample.kind {
                ActivitySampleKind::ReasoningDelta { bytes } => {
                    window.reasoning_delta_events = window.reasoning_delta_events.saturating_add(1);
                    window.reasoning_delta_bytes =
                        window.reasoning_delta_bytes.saturating_add(bytes);
                }
                ActivitySampleKind::TextDelta { bytes } => {
                    window.text_delta_events = window.text_delta_events.saturating_add(1);
                    window.text_delta_bytes = window.text_delta_bytes.saturating_add(bytes);
                }
                ActivitySampleKind::ToolStarted { kind } => {
                    window.tool_calls_started = window.tool_calls_started.saturating_add(1);
                    match kind {
                        PassiveToolKind::Read => {
                            window.read_calls = window.read_calls.saturating_add(1)
                        }
                        PassiveToolKind::Bash => {
                            window.bash_calls = window.bash_calls.saturating_add(1)
                        }
                        PassiveToolKind::Other => {
                            window.other_tool_calls = window.other_tool_calls.saturating_add(1)
                        }
                    }
                }
                ActivitySampleKind::ToolCompleted => {
                    window.tool_calls_completed = window.tool_calls_completed.saturating_add(1)
                }
                ActivitySampleKind::ToolFailed => {
                    window.tool_calls_failed = window.tool_calls_failed.saturating_add(1)
                }
            }
        }
        let mut active_tools = state
            .active_tools
            .iter()
            .map(|(tool_call_id, (kind, _))| PassiveActiveTool {
                tool_call_id: tool_call_id.clone(),
                kind: *kind,
            })
            .collect::<Vec<_>>();
        active_tools.sort_by(|left, right| left.tool_call_id.cmp(&right.tool_call_id));
        PassiveActivitySnapshot {
            revision: state.revision,
            last_runtime_event_at: state.last_runtime_event_at.map(|(_, wall)| wall),
            last_activity_age_ms: state
                .last_runtime_event_at
                .map(|(at, _)| duration_millis(now.saturating_duration_since(at))),
            model_request_active: !state.active_model_requests.is_empty(),
            model_request_age_ms: state
                .active_model_requests
                .values()
                .min()
                .map(|at| duration_millis(now.saturating_duration_since(*at))),
            model_last_delta_age_ms: state
                .last_model_delta_at
                .map(|at| duration_millis(now.saturating_duration_since(at))),
            latest_text_tail: state.latest_text_tail.clone(),
            latest_text_updated_at: state.latest_text_updated_at,
            latest_text_truncated: state.latest_text_truncated,
            latest_progress: state.latest_progress.clone(),
            active_tools,
            oldest_active_tool_age_ms: state
                .active_tools
                .values()
                .map(|(_, at)| duration_millis(now.saturating_duration_since(*at)))
                .max(),
            window_60s: window,
            telemetry_degraded: state.telemetry_degraded,
        }
    }

    fn take_terminal_text(&self) -> TerminalText {
        let mut state = self.state.lock().unwrap();
        if state.terminal_text.trim().is_empty() {
            state.terminal_text.clear();
            TerminalText::Missing
        } else {
            TerminalText::Visible(std::mem::take(&mut state.terminal_text))
        }
    }

    fn observation_snapshot(&self) -> observation::ObservationSnapshot {
        self.state.lock().unwrap().observation.snapshot()
    }

    fn confirm_runtime_source(&self, still_verified: bool) {
        if !still_verified {
            self.runtime_source_verified.store(false, Ordering::Release);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TerminalText {
    Visible(String),
    Missing,
}

fn duration_millis(value: Duration) -> u64 {
    value.as_millis().try_into().unwrap_or(u64::MAX)
}

fn activity_wall_now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn append_latest_text(state: &mut PassiveActivityState, delta: &str, wall_now_ms: u64) {
    state.terminal_text.push_str(delta);
    state.latest_text_tail.push_str(delta);
    if state.latest_text_tail.len() > MAX_LATEST_TEXT_BYTES {
        let mut split = state.latest_text_tail.len() - MAX_LATEST_TEXT_BYTES;
        while !state.latest_text_tail.is_char_boundary(split) {
            split += 1;
        }
        state.latest_text_tail.drain(..split);
        state.latest_text_truncated = true;
    }
    state.latest_text_updated_at = Some(wall_now_ms);
}

fn parse_passive_activity(event: &RuntimeEvent) -> ParsedActivity {
    match event {
        RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(event))) => {
            parse_activity_message(&event.method, &event.params, ActivitySource::Session)
        }
        RuntimeEvent::Driver(Inbound::Message(WireMessage::UnknownEvent { method, raw })) => {
            let params = raw.get("params").unwrap_or(&serde_json::Value::Null);
            let source = if method == "v4/telemetry/event" {
                ActivitySource::Telemetry
            } else if method == "session/event" {
                ActivitySource::Session
            } else {
                ActivitySource::Runtime
            };
            parse_activity_message(method, params, source)
        }
        RuntimeEvent::Driver(Inbound::Message(WireMessage::Request(request)))
            if request.method == INTERACTION_REQUEST_PERMISSION
                || request.method == INTERACTION_REQUEST_USER_INPUT =>
        {
            let mut parsed = ParsedActivity::runtime();
            parsed.transition = Some(ActivityTransition::PermissionRequested);
            parsed.request_id = activity_id(request.params.get("requestId"));
            parsed.tool_call_id = activity_id(request.params.get("toolCallId"));
            parsed.tool_kind = classify_passive_tool(request.params.get("toolName"));
            parsed.identity = parsed
                .request_id
                .as_ref()
                .map(|id| format!("permission:{id}:requested"));
            parsed
        }
        _ => ParsedActivity::runtime(),
    }
}

fn parse_activity_message(
    method: &str,
    params: &serde_json::Value,
    source: ActivitySource,
) -> ParsedActivity {
    let mut parsed = ParsedActivity::runtime();
    parsed.source = source;
    parsed.telemetry_known = source != ActivitySource::Telemetry;
    if method == "session/event" {
        let kind = params.get("type").and_then(serde_json::Value::as_str);
        let payload = params.get("payload").unwrap_or(&serde_json::Value::Null);
        let payload_kind = payload.get("kind").and_then(serde_json::Value::as_str);
        let payload_type = payload.get("type").and_then(serde_json::Value::as_str);
        let event_id = activity_id(params.get("eventId"));
        let turn_id = activity_id(params.get("turnId"));
        match (kind, payload_kind, payload_type) {
            (Some("model.streaming"), Some("reasoning_delta"), _) => {
                let delta = payload.get("delta").and_then(serde_json::Value::as_str);
                let bytes = delta.map(|value| value.len() as u64).unwrap_or(0);
                parsed.stream_key = stream_key(params, payload, "reasoning");
                parsed.identity = event_id.map(|id| format!("stream:{id}"));
                parsed.sample = Some(ActivitySampleKind::ReasoningDelta { bytes });
            }
            (Some("model.streaming"), Some("text_delta"), _) => {
                let delta = payload.get("delta").and_then(serde_json::Value::as_str);
                let bytes = delta.map(|value| value.len() as u64).unwrap_or(0);
                parsed.stream_key = stream_key(params, payload, "text");
                parsed.identity = event_id.map(|id| format!("stream:{id}"));
                parsed.sample = Some(ActivitySampleKind::TextDelta { bytes });
                parsed.text_delta = delta.map(str::to_owned);
                parsed.assistant_message_id = activity_id(payload.get("assistantMessageId"));
            }
            (Some("model.streaming"), kind, _)
                if matches!(
                    kind,
                    Some("message_finished") | Some("message_done") | Some("text_done")
                ) =>
            {
                parsed.assistant_message_id = activity_id(payload.get("assistantMessageId"));
                parsed.message_finished = true;
            }
            (
                Some("message.completed" | "message.finished" | "message.done" | "text.done"),
                _,
                _,
            ) => {
                parsed.assistant_message_id = activity_id(payload.get("assistantMessageId"));
                parsed.message_finished = true;
            }
            (Some("tool.updated" | "streamRecovery.updated"), _, _) => {
                parse_tool_activity(&mut parsed, payload, source);
            }
            (Some("session.updated"), _, Some("model_request_started")) => {
                parse_model_activity(&mut parsed, payload, true);
            }
            (Some("session.updated"), _, Some("model_request_completed")) => {
                parse_model_activity(&mut parsed, payload, false);
            }
            (Some("permission.requested"), _, _) => {
                parse_permission_activity(&mut parsed, payload, true);
            }
            (Some("permission.resolved"), _, _) => {
                parse_permission_activity(&mut parsed, payload, false);
            }
            (Some("turn.started"), _, _) => {
                parsed.transition = Some(ActivityTransition::TurnStarted);
                parsed.identity = event_id.or(turn_id).map(|id| format!("turn:{id}:started"));
            }
            (Some("turn.completed"), _, _) => {
                parsed.transition = Some(ActivityTransition::TurnCompleted);
                parsed.terminal_response = payload
                    .get("response")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                parsed.identity = event_id
                    .or(turn_id)
                    .map(|id| format!("turn:{id}:completed"));
            }
            (Some("turn.failed"), _, _) => {
                parsed.transition = Some(ActivityTransition::TurnFailed);
                parsed.identity = event_id.or(turn_id).map(|id| format!("turn:{id}:failed"));
            }
            _ => {}
        }
    } else if method == "v4/telemetry/event" {
        parsed.telemetry_known = true;
        match params.get("kind").and_then(serde_json::Value::as_str) {
            Some("stream.chunk") => {
                let channel = match params.get("channel").and_then(serde_json::Value::as_str) {
                    Some("thought") => "reasoning",
                    Some("text") => "text",
                    _ => {
                        parsed.telemetry_known = false;
                        return parsed;
                    }
                };
                let Some(bytes) = params
                    .get("chunkLength")
                    .and_then(serde_json::Value::as_u64)
                else {
                    parsed.telemetry_known = false;
                    return parsed;
                };
                parsed.stream_key = stream_key(params, params, channel);
                parsed.identity =
                    activity_id(params.get("eventId")).map(|id| format!("stream:{id}"));
                parsed.sample = Some(if channel == "reasoning" {
                    ActivitySampleKind::ReasoningDelta { bytes }
                } else {
                    ActivitySampleKind::TextDelta { bytes }
                });
            }
            Some("tool.lifecycle") => parse_tool_activity(&mut parsed, params, source),
            Some("model.request.status") => {
                let started = params.get("status").and_then(serde_json::Value::as_str)
                    == Some("model_request_started");
                let completed = params.get("status").and_then(serde_json::Value::as_str)
                    == Some("model_request_completed");
                if started || completed {
                    parse_model_activity(&mut parsed, params, started);
                } else {
                    parsed.telemetry_known = false;
                }
            }
            Some("permission.lifecycle") => {
                match params.get("phase").and_then(serde_json::Value::as_str) {
                    Some("requested") => parse_permission_activity(&mut parsed, params, true),
                    Some("resolved") => parse_permission_activity(&mut parsed, params, false),
                    _ => parsed.telemetry_known = false,
                }
            }
            Some("turn.started") => parsed.transition = Some(ActivityTransition::TurnStarted),
            Some("turn.completed") => parsed.transition = Some(ActivityTransition::TurnCompleted),
            Some("turn.failed") => parsed.transition = Some(ActivityTransition::TurnFailed),
            Some("usage.delta") => {}
            _ => parsed.telemetry_known = false,
        }
    }
    parsed
}

fn parse_model_activity(parsed: &mut ParsedActivity, payload: &serde_json::Value, started: bool) {
    parsed.request_id = activity_id(payload.get("requestId"));
    let phase = if started { "started" } else { "completed" };
    parsed.identity = parsed
        .request_id
        .as_ref()
        .map(|id| format!("model:{id}:{phase}"));
    parsed.transition = Some(if started {
        ActivityTransition::ModelStarted
    } else {
        ActivityTransition::ModelCompleted
    });
}

fn parse_permission_activity(
    parsed: &mut ParsedActivity,
    payload: &serde_json::Value,
    requested: bool,
) {
    parsed.request_id = activity_id(payload.get("requestId"));
    parsed.tool_call_id = activity_id(payload.get("toolCallId"));
    parsed.tool_kind = classify_passive_tool(payload.get("toolName"));
    let phase = if requested { "requested" } else { "resolved" };
    parsed.identity = parsed
        .request_id
        .as_ref()
        .map(|id| format!("permission:{id}:{phase}"));
    parsed.transition = Some(if requested {
        ActivityTransition::PermissionRequested
    } else {
        ActivityTransition::PermissionResolved
    });
}

fn parse_tool_activity(
    parsed: &mut ParsedActivity,
    payload: &serde_json::Value,
    source: ActivitySource,
) {
    let phase = payload
        .get(if source == ActivitySource::Telemetry {
            "phase"
        } else {
            "kind"
        })
        .and_then(serde_json::Value::as_str)
        .and_then(|phase| match phase {
            "scheduled" => Some(ActivityTransition::ToolScheduled),
            "started" => Some(ActivityTransition::ToolStarted),
            "result" | "tool_result" | "completed" => Some(ActivityTransition::ToolCompleted),
            "error" | "tool_error" | "failed" => Some(ActivityTransition::ToolFailed),
            "batch" => None,
            _ => {
                if source == ActivitySource::Telemetry {
                    parsed.telemetry_known = false;
                }
                None
            }
        });
    parsed.tool_call_id = activity_id(payload.get("toolCallId"));
    parsed.tool_kind = classify_passive_tool(payload.get("toolName"));
    parsed.transition = phase;
    if let (Some(tool_call_id), Some(phase)) = (parsed.tool_call_id.as_ref(), phase) {
        let phase_name = match phase {
            ActivityTransition::ToolScheduled => "scheduled",
            ActivityTransition::ToolStarted => "started",
            ActivityTransition::ToolCompleted => "completed",
            ActivityTransition::ToolFailed => "failed",
            _ => return,
        };
        parsed.identity = Some(format!("tool:{tool_call_id}:{phase_name}"));
        parsed.sample = match phase {
            ActivityTransition::ToolStarted => Some(ActivitySampleKind::ToolStarted {
                kind: parsed.tool_kind,
            }),
            ActivityTransition::ToolCompleted => Some(ActivitySampleKind::ToolCompleted),
            ActivityTransition::ToolFailed => Some(ActivitySampleKind::ToolFailed),
            _ => None,
        };
    }
}

fn stream_key(
    params: &serde_json::Value,
    payload: &serde_json::Value,
    channel: &str,
) -> Option<String> {
    let turn_id = activity_id(params.get("turnId"));
    let message_id = activity_id(payload.get("assistantMessageId"));
    match (turn_id, message_id) {
        (Some(turn_id), Some(message_id)) => Some(format!("{turn_id}:{message_id}:{channel}")),
        (None, Some(message_id)) => Some(format!("{message_id}:{channel}")),
        _ => None,
    }
}

fn activity_id(value: Option<&serde_json::Value>) -> Option<String> {
    value
        .and_then(serde_json::Value::as_str)
        .filter(|value| {
            !value.is_empty() && value.len() <= MAX_ACTIVITY_ID_BYTES && !value.contains('\0')
        })
        .map(str::to_owned)
}

fn classify_passive_tool(value: Option<&serde_json::Value>) -> PassiveToolKind {
    match value.and_then(serde_json::Value::as_str) {
        Some("Read" | "read") => PassiveToolKind::Read,
        Some("Bash" | "bash") => PassiveToolKind::Bash,
        _ => PassiveToolKind::Other,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReady {
    pub session_id: String,
    pub initial_turn_id: Option<String>,
    /// Model echoed by session/create configuration. This is not evidence of
    /// the model that produced a response.
    pub configured_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeCommandError {
    Unsupported,
    Timeout,
    Transport(String),
    Remote(serde_json::Value),
    InvalidSession(String),
}

impl fmt::Display for RuntimeCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => write!(f, "runtime command plane is unsupported"),
            Self::Timeout => write!(f, "runtime command deadline elapsed"),
            Self::Transport(_) => write!(f, "runtime command transport failed"),
            Self::Remote(_) => write!(f, "runtime command was rejected"),
            Self::InvalidSession(message) => write!(f, "invalid session response: {message}"),
        }
    }
}

impl RuntimeCommandError {
    fn diagnostic(&self, operation: &str) -> String {
        let mut detail = serde_json::json!({"operation": operation, "message": bounded_error(&self.to_string())});
        if let Self::Remote(value) = self {
            detail["remote_code"] = value.get("code").and_then(serde_json::Value::as_i64).into();
            detail["remote_message"] = value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(|message| bounded_prefix(message, 1024))
                .into();
        }
        detail.to_string()
    }
}

// Keep only the remote code/message, never error.data or provider configuration.
impl std::error::Error for RuntimeCommandError {}

impl From<RequestError> for RuntimeCommandError {
    fn from(error: RequestError) -> Self {
        match error {
            RequestError::Timeout => Self::Timeout,
            RequestError::Remote(value) => Self::Remote(value),
            other => Self::Transport(other.to_string()),
        }
    }
}

#[derive(Debug)]
struct TurnTrackerState {
    generation: u64,
    active: bool,
    boundary: Option<TurnBoundary>,
    model_request_started_at: Option<Instant>,
    last_stream_activity_at: Option<Instant>,
}

struct TurnTracker {
    state: Mutex<TurnTrackerState>,
    changed: Condvar,
}

impl TurnTracker {
    fn new() -> Self {
        Self {
            state: Mutex::new(TurnTrackerState {
                generation: 0,
                active: false,
                boundary: None,
                model_request_started_at: None,
                last_stream_activity_at: None,
            }),
            changed: Condvar::new(),
        }
    }

    fn observe(&self, inbound: &Inbound) {
        let mut state = self.state.lock().unwrap();
        if state.active {
            state.last_stream_activity_at = Some(Instant::now());
        }
        let Inbound::Message(WireMessage::Event(event)) = inbound else {
            return;
        };
        let Some(kind) = event_type(event) else {
            return;
        };
        match kind {
            "turn.started" => {
                let now = Instant::now();
                state.generation = state.generation.saturating_add(1);
                state.active = true;
                state.boundary = None;
                state.model_request_started_at = Some(now);
                state.last_stream_activity_at = Some(now);
            }
            "turn.completed" if state.active => {
                state.active = false;
                state.boundary = Some(TurnBoundary::Completed);
                state.model_request_started_at = None;
                state.last_stream_activity_at = None;
            }
            "turn.failed" if state.active => {
                state.active = false;
                state.boundary = Some(TurnBoundary::Failed);
                state.model_request_started_at = None;
                state.last_stream_activity_at = None;
            }
            _ => return,
        }
        self.changed.notify_all();
    }

    fn snapshot(&self) -> TurnSnapshot {
        let state = self.state.lock().unwrap();
        TurnSnapshot {
            generation: state.generation,
            active: state.active,
            boundary: state.boundary,
        }
    }

    fn activity_snapshot(&self) -> RuntimeActivitySnapshot {
        let state = self.state.lock().unwrap();
        let now = Instant::now();
        RuntimeActivitySnapshot {
            turn: TurnSnapshot {
                generation: state.generation,
                active: state.active,
                boundary: state.boundary,
            },
            model_request_elapsed: state
                .model_request_started_at
                .and_then(|started| now.checked_duration_since(started)),
            transport_idle_elapsed: state
                .last_stream_activity_at
                .and_then(|activity| now.checked_duration_since(activity)),
        }
    }

    fn wait_started_after(
        &self,
        previous_generation: u64,
        timeout: Duration,
    ) -> Result<TurnSnapshot, RuntimeCommandError> {
        self.wait_until(timeout, |state| state.generation > previous_generation)
    }

    fn wait_boundary_after(
        &self,
        generation: u64,
        timeout: Duration,
    ) -> Result<TurnSnapshot, RuntimeCommandError> {
        self.wait_until(timeout, |state| {
            state.generation >= generation && !state.active && state.boundary.is_some()
        })
    }

    fn wait_until(
        &self,
        timeout: Duration,
        predicate: impl Fn(&TurnTrackerState) -> bool,
    ) -> Result<TurnSnapshot, RuntimeCommandError> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().unwrap();
        loop {
            if predicate(&state) {
                return Ok(TurnSnapshot {
                    generation: state.generation,
                    active: state.active,
                    boundary: state.boundary,
                });
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(RuntimeCommandError::Timeout);
            }
            let (next, result) = self.changed.wait_timeout(state, deadline - now).unwrap();
            state = next;
            if result.timed_out() && !predicate(&state) {
                return Err(RuntimeCommandError::Timeout);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    Driver(Inbound),
    Terminal(RuntimeTerminal),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleRecord {
    pub sequence: u64,
    pub event: RuntimeEvent,
}

pub trait LifecycleSink: Send + Sync + 'static {
    fn emit(&self, record: LifecycleRecord);
}

#[derive(Debug)]
enum OwnerState {
    Running,
    Stopping,
    Terminal(RuntimeTerminal),
}

#[derive(Debug)]
struct PublisherState {
    next_sequence: u64,
    owner: OwnerState,
    exit_boundary_delivered: bool,
}

struct Publisher {
    sink: Arc<dyn LifecycleSink>,
    state: Mutex<PublisherState>,
    changed: Condvar,
}

impl Publisher {
    fn new(sink: Arc<dyn LifecycleSink>) -> Self {
        Self {
            sink,
            state: Mutex::new(PublisherState {
                next_sequence: 1,
                owner: OwnerState::Running,
                exit_boundary_delivered: false,
            }),
            changed: Condvar::new(),
        }
    }

    fn emit_driver(&self, event: Inbound, exit_terminal: Option<RuntimeTerminal>) {
        let mut state = self.state.lock().unwrap();
        if matches!(state.owner, OwnerState::Terminal(_)) {
            return;
        }
        let is_exit_boundary = matches!(event, Inbound::ChildExited(_));
        self.emit_locked(&mut state, RuntimeEvent::Driver(event));
        if is_exit_boundary {
            state.exit_boundary_delivered = true;
            self.changed.notify_all();
        }
        if let Some(terminal) = exit_terminal {
            if matches!(state.owner, OwnerState::Running) {
                self.publish_terminal_locked(&mut state, terminal);
            }
        }
    }

    fn begin_stopping(&self) -> Option<RuntimeTerminal> {
        let mut state = self.state.lock().unwrap();
        match &state.owner {
            OwnerState::Terminal(terminal) => Some(terminal.clone()),
            OwnerState::Running => {
                state.owner = OwnerState::Stopping;
                None
            }
            OwnerState::Stopping => None,
        }
    }

    fn publish_terminal(&self, terminal: RuntimeTerminal) -> RuntimeTerminal {
        let mut state = self.state.lock().unwrap();
        if let OwnerState::Terminal(existing) = &state.owner {
            return existing.clone();
        }
        self.publish_terminal_locked(&mut state, terminal.clone());
        terminal
    }

    fn wait_for_exit_boundary(&self, timeout: Duration) -> Option<RuntimeTerminal> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().unwrap();
        loop {
            if state.exit_boundary_delivered {
                return None;
            }
            if let OwnerState::Terminal(terminal) = &state.owner {
                return Some(terminal.clone());
            }
            let now = Instant::now();
            if now >= deadline {
                return Some(RuntimeTerminal::FailedRuntimeLost(
                    RuntimeLoss::EventStreamLost,
                ));
            }
            let (next, wait) = self.changed.wait_timeout(state, deadline - now).unwrap();
            state = next;
            if wait.timed_out() && !state.exit_boundary_delivered {
                return Some(RuntimeTerminal::FailedRuntimeLost(
                    RuntimeLoss::EventStreamLost,
                ));
            }
        }
    }

    fn publish_terminal_locked(&self, state: &mut PublisherState, terminal: RuntimeTerminal) {
        state.owner = OwnerState::Terminal(terminal.clone());
        self.emit_locked(state, RuntimeEvent::Terminal(terminal));
        self.changed.notify_all();
    }

    fn emit_locked(&self, state: &mut PublisherState, event: RuntimeEvent) {
        let record = LifecycleRecord {
            sequence: state.next_sequence,
            event,
        };
        state.next_sequence = state.next_sequence.saturating_add(1);
        self.sink.emit(record);
    }

    fn wait_terminal(&self, timeout: Duration) -> Option<RuntimeTerminal> {
        let deadline = Instant::now().checked_add(timeout)?;
        let mut state = self.state.lock().unwrap();
        loop {
            if let OwnerState::Terminal(terminal) = &state.owner {
                return Some(terminal.clone());
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (next, wait) = self.changed.wait_timeout(state, deadline - now).unwrap();
            state = next;
            if wait.timed_out() && !matches!(state.owner, OwnerState::Terminal(_)) {
                return None;
            }
        }
    }
}

pub struct RuntimeOwner {
    driver: Arc<Driver>,
    publisher: Arc<Publisher>,
    shutdown_pump: Arc<AtomicBool>,
    turn_tracker: Arc<TurnTracker>,
    session_id: Mutex<Option<String>>,
    diagnostic_session_id: Mutex<Option<String>>,
    permission_responses: Arc<Mutex<OfferedPermissionCache>>,
    stop_boundaries: AtomicU64,
}

#[derive(Debug, Clone)]
struct PermissionResponses {
    allow: serde_json::Value,
    deny: serde_json::Value,
    params: serde_json::Value,
}

const MAX_PENDING_PERMISSION_RESPONSES: usize = 128;

#[derive(Debug, Default)]
struct OfferedPermissionCache {
    requests: HashMap<String, PermissionResponses>,
    denied_fingerprints: HashSet<String>,
}

impl OfferedPermissionCache {
    fn observe(&mut self, key: String, params: &serde_json::Value) {
        let reused = self.requests.remove(&key).is_some();
        let offered = offered_permission_response(params, "allow")
            .zip(offered_permission_response(params, "deny"))
            .map(|(allow, deny)| PermissionResponses {
                allow,
                deny,
                params: params.clone(),
            });
        if !reused && self.requests.len() < MAX_PENDING_PERMISSION_RESPONSES {
            if let Some(offered) = offered {
                self.requests.insert(key, offered);
            }
        }
    }

    fn response(
        &self,
        key: &str,
        decision: &str,
        validated_denial: Option<&ValidatedPermissionDenial>,
    ) -> Option<serde_json::Value> {
        let offered = self.requests.get(key)?;
        match decision {
            "allow" => Some(offered.allow.clone()),
            "deny" => {
                let validated_denial = validated_denial
                    .cloned()
                    .or_else(|| PolicyLauncher::external_zcode_denial(&offered.params))?;
                let fingerprint = validated_denial.fingerprint();
                let repeated = self.denied_fingerprints.contains(&fingerprint);
                let feedback = validated_denial.feedback(repeated);
                let mut response = offered.deny.clone();
                response.as_object_mut()?.insert(
                    "reason".into(),
                    serde_json::Value::String(if repeated {
                        format!(
                            "{feedback} Stop this evidence path; use Read, prepared inputs, or record a coverage gap."
                        )
                    } else {
                        feedback
                    }),
                );
                Some(response)
            }
            _ => None,
        }
    }

    fn complete(&mut self, key: &str) {
        self.requests.remove(key);
    }

    fn record_denial(&mut self, key: &str, validated_denial: Option<&ValidatedPermissionDenial>) {
        let fingerprint = self.requests.get(key).and_then(|responses| {
            validated_denial
                .cloned()
                .or_else(|| PolicyLauncher::external_zcode_denial(&responses.params))
                .map(|denial| denial.fingerprint())
        });
        if let Some(fingerprint) = fingerprint {
            if self.denied_fingerprints.len() < MAX_PENDING_PERMISSION_RESPONSES {
                self.denied_fingerprints.insert(fingerprint);
            }
        }
    }

    fn clear(&mut self) {
        self.requests.clear();
        self.denied_fingerprints.clear();
    }
}

impl RuntimeOwner {
    pub fn spawn(command: Command, sink: Arc<dyn LifecycleSink>) -> io::Result<Self> {
        let driver = Arc::new(Driver::spawn(command)?);
        let publisher = Arc::new(Publisher::new(sink));
        let shutdown_pump = Arc::new(AtomicBool::new(false));
        let turn_tracker = Arc::new(TurnTracker::new());
        let permission_responses = Arc::new(Mutex::new(OfferedPermissionCache::default()));
        spawn_event_pump(
            Arc::clone(&driver),
            Arc::clone(&publisher),
            Arc::clone(&shutdown_pump),
            Arc::clone(&turn_tracker),
            Arc::clone(&permission_responses),
        );
        Ok(Self {
            driver,
            publisher,
            shutdown_pump,
            turn_tracker,
            session_id: Mutex::new(None),
            diagnostic_session_id: Mutex::new(None),
            permission_responses,
            stop_boundaries: AtomicU64::new(0),
        })
    }

    pub fn bootstrap_session(
        &self,
        workspace_path: &str,
        initial_prompt: &str,
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        self.bootstrap_session_with_mcp_for_requested_model(
            workspace_path,
            initial_prompt,
            &[],
            None,
            None,
            timeout,
        )
    }

    pub fn bootstrap_session_with_mcp(
        &self,
        workspace_path: &str,
        initial_prompt: &str,
        mcp_servers: &[StdioMcpServer],
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        self.bootstrap_session_with_mcp_for_requested_model(
            workspace_path,
            initial_prompt,
            mcp_servers,
            None,
            None,
            timeout,
        )
    }

    pub fn resume_session_with_mcp(
        &self,
        task: &TaskRecord,
        mcp_servers: &[StdioMcpServer],
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        let session_id = task.zcode_session_id.as_deref().ok_or_else(|| {
            RuntimeCommandError::InvalidSession("task has no persisted session id".into())
        })?;
        *self.diagnostic_session_id.lock().unwrap() = Some(session_id.to_owned());
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RuntimeCommandError::Timeout)?;
        let workspace = WorkspaceRef {
            workspace_key: &task.workspace_path,
            workspace_path: &task.workspace_path,
        };
        let params = serde_json::to_value(ResumeSessionParams {
            session_id,
            workspace: Some(workspace),
            mcp_servers,
        })
        .map_err(|error| RuntimeCommandError::Transport(error.to_string()))?;
        self.driver
            .request(SESSION_RESUME, params, remaining_runtime_time(deadline)?)?;
        let subscribe_params = serde_json::to_value(SubscribeParams {
            session_id,
            delivery_kind: "desktop-continuous",
            include_snapshot: true,
        })
        .map_err(|error| RuntimeCommandError::Transport(error.to_string()))?;
        self.driver.request(
            SESSION_SUBSCRIBE,
            subscribe_params,
            remaining_runtime_time(deadline)?,
        )?;
        *self.session_id.lock().unwrap() = Some(session_id.to_owned());
        Ok(SessionReady {
            session_id: session_id.to_owned(),
            initial_turn_id: None,
            configured_model: None,
        })
    }

    fn bootstrap_prepared_session(
        &self,
        task: &TaskRecord,
        mcp_servers: &[StdioMcpServer],
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        let requested_model =
            requested_model_from_prepared_launch(Some(task.prepared_launch_json.as_str()));
        self.bootstrap_session_with_mcp_for_requested_model(
            &task.workspace_path,
            &task.initial_prompt,
            mcp_servers,
            requested_model.as_deref(),
            permission_mode_from_task(task),
            timeout,
        )
    }

    fn bootstrap_session_with_mcp_for_requested_model(
        &self,
        workspace_path: &str,
        initial_prompt: &str,
        mcp_servers: &[StdioMcpServer],
        requested_model: Option<&str>,
        mode: Option<&str>,
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RuntimeCommandError::Timeout)?;
        let workspace = WorkspaceRef {
            workspace_key: workspace_path,
            workspace_path,
        };
        let create_params = serde_json::to_value(CreateSessionParams {
            workspace,
            mode,
            mcp_servers,
        })
        .map_err(|error| RuntimeCommandError::Transport(error.to_string()))?;
        let created = self.driver.request(
            SESSION_CREATE,
            create_params,
            remaining_runtime_time(deadline)?,
        )?;
        let result = created.result.as_ref().ok_or_else(|| {
            RuntimeCommandError::InvalidSession("session/create result is missing".into())
        })?;
        let projection = SessionCreateProjection::from_result(result).map_err(|error| {
            RuntimeCommandError::InvalidSession(format!(
                "session/create projection is invalid: {error}"
            ))
        })?;
        let session_id = projection.session_id;
        // Correlation only: the command-plane session is still registered only
        // after subscribe succeeds. A rejected subscribe must remain diagnosable.
        *self.diagnostic_session_id.lock().unwrap() = Some(session_id.clone());
        let configured_model = projection.requested_model;
        validate_requested_model(requested_model, configured_model.as_deref())
            .map_err(|code| RuntimeCommandError::InvalidSession(code.into()))?;
        let subscribe_params = serde_json::to_value(SubscribeParams {
            session_id: &session_id,
            delivery_kind: "desktop-continuous",
            include_snapshot: true,
        })
        .map_err(|error| RuntimeCommandError::Transport(error.to_string()))?;
        self.driver.request(
            SESSION_SUBSCRIBE,
            subscribe_params,
            remaining_runtime_time(deadline)?,
        )?;
        *self.session_id.lock().unwrap() = Some(session_id.clone());
        let initial_turn_id = self.send_turn_before(&session_id, initial_prompt, deadline)?;
        Ok(SessionReady {
            session_id,
            initial_turn_id,
            configured_model,
        })
    }

    pub fn send_turn(
        &self,
        session_id: &str,
        content: &str,
        timeout: Duration,
    ) -> Result<Option<String>, RuntimeCommandError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RuntimeCommandError::Timeout)?;
        self.send_turn_before(session_id, content, deadline)
    }

    fn send_turn_before(
        &self,
        session_id: &str,
        content: &str,
        deadline: Instant,
    ) -> Result<Option<String>, RuntimeCommandError> {
        self.validate_session(session_id)?;
        let previous = self.turn_tracker.snapshot().generation;
        let params = serde_json::to_value(SendParams {
            session_id,
            content,
        })
        .map_err(|error| RuntimeCommandError::Transport(error.to_string()))?;
        let response =
            self.driver
                .request(SESSION_SEND, params, remaining_runtime_time(deadline)?)?;
        let turn_id = response
            .result
            .as_ref()
            .and_then(turn_id_from_result)
            .map(str::to_owned);
        self.turn_tracker
            .wait_started_after(previous, remaining_runtime_time(deadline)?)?;
        Ok(turn_id)
    }

    pub fn stop_turn(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> Result<TurnSnapshot, RuntimeCommandError> {
        let deadline = Instant::now() + timeout;
        self.validate_session(session_id)?;
        let current = self.turn_tracker.snapshot();
        if !current.active {
            return Ok(current);
        }
        let params = serde_json::to_value(SessionParams { session_id })
            .map_err(|error| RuntimeCommandError::Transport(error.to_string()))?;
        self.driver
            .request(SESSION_STOP, params, remaining_runtime_time(deadline)?)?;
        let boundary = self
            .turn_tracker
            .wait_boundary_after(current.generation, remaining_runtime_time(deadline)?)?;
        self.stop_boundaries.fetch_add(1, Ordering::AcqRel);
        Ok(boundary)
    }

    pub fn respond_request(
        &self,
        correlation_id: &str,
        decision: &str,
        content: Option<&str>,
        validated_denial: Option<&ValidatedPermissionDenial>,
        deadline: Instant,
    ) -> Result<(), RuntimeCommandError> {
        let id = serde_json::from_str::<WireId>(correlation_id).map_err(|_| {
            RuntimeCommandError::InvalidSession("stored request correlation is invalid".into())
        })?;
        if !matches!(decision, "allow" | "deny") {
            return Err(RuntimeCommandError::Unsupported);
        }
        let key = serde_json::to_string(&id)
            .map_err(|error| RuntimeCommandError::Transport(error.to_string()))?;
        let result = {
            self.permission_responses
                .lock()
                .unwrap()
                .response(&key, decision, validated_denial)
                .ok_or_else(|| {
                    RuntimeCommandError::InvalidSession(
                        "runtime offered no matching permission response".into(),
                    )
                })?
        };
        let _ = content;
        self.driver
            .respond_before(id, result, deadline)
            .map_err(RuntimeCommandError::from)?;
        if decision == "deny" {
            self.permission_responses
                .lock()
                .unwrap()
                .record_denial(&key, validated_denial);
        }
        self.permission_responses.lock().unwrap().complete(&key);
        Ok(())
    }

    pub fn close_session(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> Result<(), RuntimeCommandError> {
        self.validate_session(session_id)?;
        let params = serde_json::to_value(SessionParams { session_id })
            .map_err(|error| RuntimeCommandError::Transport(error.to_string()))?;
        self.driver
            .request(external_contract::SESSION_CLOSE, params, timeout)?;
        Ok(())
    }

    pub fn turn_snapshot(&self) -> TurnSnapshot {
        self.turn_tracker.snapshot()
    }

    pub fn stop_boundary_count(&self) -> u64 {
        self.stop_boundaries.load(Ordering::Acquire)
    }

    fn validate_session(&self, session_id: &str) -> Result<(), RuntimeCommandError> {
        if self.session_id.lock().unwrap().as_deref() == Some(session_id) {
            Ok(())
        } else {
            Err(RuntimeCommandError::InvalidSession(
                "session id does not belong to this runtime".into(),
            ))
        }
    }

    pub fn identity(&self) -> ProcessIdentity {
        self.driver.identity()
    }

    pub fn stop(&self, grace: Duration) -> RuntimeTerminal {
        self.finish_process(grace, None)
    }

    pub fn finish_turn(&self, boundary: TurnBoundary, grace: Duration) -> RuntimeTerminal {
        self.finish_process(grace, Some(boundary))
    }

    fn finish_process(&self, grace: Duration, boundary: Option<TurnBoundary>) -> RuntimeTerminal {
        if let Some(terminal) = self.publisher.begin_stopping() {
            return terminal;
        }
        let terminal = match self.driver.stop_and_reap(grace) {
            Ok(outcome) => match self.publisher.wait_for_exit_boundary(grace) {
                Some(terminal) => terminal,
                None => match boundary {
                    Some(TurnBoundary::Completed) => RuntimeTerminal::Completed(outcome),
                    Some(TurnBoundary::Failed) => RuntimeTerminal::FailedTurn(outcome),
                    None => RuntimeTerminal::Stopped(outcome),
                },
            },
            Err(error) => {
                RuntimeTerminal::FailedRuntimeLost(RuntimeLoss::StopFailed(error.to_string()))
            }
        };
        self.permission_responses.lock().unwrap().clear();
        self.publisher.publish_terminal(terminal)
    }

    pub fn close(&self, grace: Duration) -> RuntimeTerminal {
        self.stop(grace)
    }

    pub fn wait_terminal(&self, timeout: Duration) -> Option<RuntimeTerminal> {
        self.publisher.wait_terminal(timeout)
    }
}

fn remaining_runtime_time(deadline: Instant) -> Result<Duration, RuntimeCommandError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(RuntimeCommandError::Timeout)
}

fn control_failure_code(error: &RuntimeCommandError) -> &'static str {
    if matches!(error, RuntimeCommandError::Timeout) {
        "CONTROL_DEADLINE_EXCEEDED"
    } else {
        "CONTROL_RUNTIME_FAILED"
    }
}

fn validate_requested_model(
    requested: Option<&str>,
    observed: Option<&str>,
) -> Result<(), &'static str> {
    let Some(requested) = requested else {
        return Ok(());
    };
    let Some(requested) = normalized_zai_model(requested) else {
        return Err("MODEL_REQUEST_INVALID");
    };
    let Some(observed) = observed.and_then(normalized_zai_model) else {
        return Err("MODEL_NOT_OBSERVED");
    };
    if requested != observed {
        return Err("MODEL_MISMATCH");
    }
    Ok(())
}

fn requested_model_from_prepared_launch(prepared_launch_json: Option<&str>) -> Option<String> {
    prepared_launch_json
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .and_then(|prepared| {
            prepared
                .get("model")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
}

fn permission_mode_from_task(task: &TaskRecord) -> Option<&'static str> {
    let value = serde_json::from_str::<serde_json::Value>(&task.prepared_launch_json).ok()?;
    match value
        .get("permission_mode")
        .and_then(serde_json::Value::as_str)
    {
        Some("plan") => Some("plan"),
        Some("build") => Some("build"),
        // ZCode's ACP session mode uses `build` for interactive tool approval;
        // its `edit` mode auto-approves workspace mutations. The public
        // contract keeps `edit`, but must map it to the approval-bearing mode.
        Some("edit") => Some("build"),
        Some("yolo") => Some("yolo"),
        _ => None,
    }
}

impl Drop for RuntimeOwner {
    fn drop(&mut self) {
        let _ = self.stop(Duration::from_secs(1));
        self.shutdown_pump.store(true, Ordering::Release);
    }
}

fn spawn_event_pump(
    driver: Arc<Driver>,
    publisher: Arc<Publisher>,
    shutdown: Arc<AtomicBool>,
    turn_tracker: Arc<TurnTracker>,
    permission_responses: Arc<Mutex<OfferedPermissionCache>>,
) {
    thread::spawn(move || loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        match driver.recv_timeout(Duration::from_millis(20)) {
            Ok(event) => {
                if let Inbound::Message(WireMessage::Request(request)) = &event {
                    if request.method == SESSION_REQUEST_RUNTIME_PREFERENCES {
                        let result = serde_json::to_value(RuntimePreferences::default())
                            .expect("runtime preferences serialize");
                        if driver.respond(request.id.clone(), result).is_err() {
                            publisher.publish_terminal(RuntimeTerminal::FailedRuntimeLost(
                                RuntimeLoss::EventStreamLost,
                            ));
                            return;
                        }
                    } else if request.method == INTERACTION_REQUEST_PERMISSION {
                        if let Ok(key) = serde_json::to_string(&request.id) {
                            permission_responses
                                .lock()
                                .unwrap()
                                .observe(key, &request.params);
                        }
                    }
                }
                turn_tracker.observe(&event);
                let is_exit_boundary = matches!(event, Inbound::ChildExited(_));
                if is_exit_boundary {
                    driver.wait_diagnostics(Duration::from_secs(1));
                }
                let terminal = match &event {
                    Inbound::ChildExited(exit) => {
                        match observe_process_group(driver.identity().pgid) {
                            Ok(members) if members.is_empty() => match exit {
                                ChildExit::Exited(Some(0)) => {
                                    let turn = turn_tracker.snapshot();
                                    if !turn.active
                                        && turn.boundary == Some(TurnBoundary::Completed)
                                    {
                                        Some(RuntimeTerminal::Completed(
                                            StopOutcome::AlreadyExited(exit.clone()),
                                        ))
                                    } else {
                                        Some(RuntimeTerminal::FailedRuntimeLost(
                                            RuntimeLoss::EventStreamLost,
                                        ))
                                    }
                                }
                                _ => Some(RuntimeTerminal::Exited(exit.clone())),
                            },
                            Ok(_) | Err(_) => {
                                Some(RuntimeTerminal::Orphaned(RuntimeLoss::UnknownMembership))
                            }
                        }
                    }
                    _ => None,
                };
                publisher.emit_driver(event, terminal);
                if is_exit_boundary {
                    permission_responses.lock().unwrap().clear();
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                permission_responses.lock().unwrap().clear();
                publisher.publish_terminal(RuntimeTerminal::FailedRuntimeLost(
                    RuntimeLoss::EventStreamLost,
                ));
                return;
            }
        }
    });
}

pub fn classify_restart(identity: &ProcessIdentity) -> RuntimeTerminal {
    if identity.pid <= 1
        || identity.pgid <= 1
        || identity.pid as i32 != identity.pgid
        || identity.start_token.is_empty()
    {
        return RuntimeTerminal::Orphaned(RuntimeLoss::InvalidIdentity);
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = identity;
        return RuntimeTerminal::Orphaned(RuntimeLoss::UnsupportedIdentity);
    }

    #[cfg(target_os = "macos")]
    {
        let first = match observe_process(identity.pid) {
            Ok(observed) => observed,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return RuntimeTerminal::Orphaned(RuntimeLoss::MissingLeader);
            }
            Err(_) => return RuntimeTerminal::Orphaned(RuntimeLoss::UnsupportedIdentity),
        };
        if &first != identity {
            return RuntimeTerminal::Orphaned(RuntimeLoss::IdentityMismatch);
        }
        let members = match observe_process_group(identity.pgid) {
            Ok(members) => members,
            Err(_) => return RuntimeTerminal::Orphaned(RuntimeLoss::UnknownMembership),
        };
        if members.is_empty()
            || !members.iter().any(|member| member == identity)
            || members.iter().any(|member| {
                member.pgid != identity.pgid
                    || member.uid != identity.uid
                    || member.start_token.is_empty()
            })
        {
            return RuntimeTerminal::Orphaned(RuntimeLoss::UnknownMembership);
        }
        match observe_process(identity.pid) {
            Ok(second) if second == first => {
                RuntimeTerminal::FailedRuntimeLost(RuntimeLoss::SessionLost)
            }
            Ok(_) => RuntimeTerminal::Orphaned(RuntimeLoss::IdentityMismatch),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                RuntimeTerminal::Orphaned(RuntimeLoss::MissingLeader)
            }
            Err(_) => RuntimeTerminal::Orphaned(RuntimeLoss::UnsupportedIdentity),
        }
    }
}

#[derive(Clone)]
enum TaskRoute {
    General(Box<PreparedGeneralTask>),
}

fn task_route(task: &TaskRecord) -> Result<TaskRoute, String> {
    let json = task.prepared_launch_json.as_str();
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|_| "stored prepared launch is invalid")?;
    match value.get("schema").and_then(serde_json::Value::as_str) {
        Some(external_core::GENERAL_TASK_SCHEMA) => {
            let prepared: PreparedGeneralTask = serde_json::from_value(value)
                .map_err(|_| "stored general preparation is invalid")?;
            prepared
                .validate_digest()
                .map_err(|_| "stored general preparation digest is invalid")?;
            if task.prepared_launch_sha256 != prepared.prepared_sha256
                || task.workspace_path != prepared.workspace.path.to_string_lossy()
            {
                return Err("stored task does not match its general preparation".into());
            }
            Ok(TaskRoute::General(Box::new(prepared)))
        }
        Some(_) => Err("stored prepared launch uses an unknown task schema".into()),
        None => Err("stored prepared launch omitted task schema".into()),
    }
}

fn validate_task_route(task: Option<&TaskRecord>, route: &TaskRoute) -> Result<(), String> {
    match (task, route) {
        (Some(_), TaskRoute::General(_)) => Ok(()),
        (None, TaskRoute::General(_)) => Err("prepared task metadata is missing".into()),
    }
}

fn route_policy(
    route: &TaskRoute,
    resumed: bool,
) -> external_core::PreparationResult<Option<PolicyLauncher>> {
    match route {
        TaskRoute::General(prepared) => {
            if resumed {
                let mut launcher = prepared.resume_launcher()?;
                launcher.set_interactive_bash(matches!(
                    prepared.permission_mode,
                    external_core::PermissionMode::Edit
                ));
                Ok(Some(launcher))
            } else {
                let mut launcher = prepared.launcher()?;
                launcher.set_interactive_bash(matches!(
                    prepared.permission_mode,
                    external_core::PermissionMode::Edit
                ));
                Ok(Some(launcher))
            }
        }
    }
}

pub trait ManagedRuntime: Send + Sync + 'static {
    fn identity(&self) -> Option<ProcessIdentity>;
    fn stop(&self, grace: Duration) -> RuntimeTerminal;
    fn wait_terminal(&self, timeout: Duration) -> Option<RuntimeTerminal>;
    fn diagnostic_tail(&self) -> String {
        String::new()
    }
    fn diagnostic_session_id(&self) -> Option<String> {
        None
    }
    fn wait_diagnostics(&self, _timeout: Duration) {}
    fn bootstrap_session(
        &self,
        _job: &TaskRecord,
        _timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        Err(RuntimeCommandError::Unsupported)
    }
    fn bootstrap_session_with_mcp(
        &self,
        task: &TaskRecord,
        _mcp_servers: &[StdioMcpServer],
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        self.bootstrap_session(task, timeout)
    }
    fn resume_session_with_mcp(
        &self,
        _task: &TaskRecord,
        _mcp_servers: &[StdioMcpServer],
        _timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        Err(RuntimeCommandError::Unsupported)
    }
    fn send_turn(
        &self,
        _session_id: &str,
        _content: &str,
        _timeout: Duration,
    ) -> Result<Option<String>, RuntimeCommandError> {
        Err(RuntimeCommandError::Unsupported)
    }
    fn stop_turn(
        &self,
        _session_id: &str,
        _timeout: Duration,
    ) -> Result<TurnSnapshot, RuntimeCommandError> {
        Err(RuntimeCommandError::Unsupported)
    }
    fn respond_request(
        &self,
        _correlation_id: &str,
        _decision: &str,
        _content: Option<&str>,
        _validated_denial: Option<&ValidatedPermissionDenial>,
        _deadline: Instant,
    ) -> Result<(), RuntimeCommandError> {
        Err(RuntimeCommandError::Unsupported)
    }
    fn close_session(
        &self,
        _session_id: &str,
        _timeout: Duration,
    ) -> Result<(), RuntimeCommandError> {
        Ok(())
    }
    fn turn_snapshot(&self) -> TurnSnapshot {
        TurnSnapshot {
            generation: 0,
            active: false,
            boundary: None,
        }
    }
    fn activity_snapshot(&self) -> RuntimeActivitySnapshot {
        let turn = self.turn_snapshot();
        RuntimeActivitySnapshot {
            model_request_elapsed: turn.active.then_some(Duration::ZERO),
            transport_idle_elapsed: turn.active.then_some(Duration::ZERO),
            turn,
        }
    }
    fn stop_boundary_count(&self) -> u64 {
        0
    }
    fn finish_turn(&self, boundary: TurnBoundary, grace: Duration) -> RuntimeTerminal {
        let _ = boundary;
        self.stop(grace)
    }
}

impl ManagedRuntime for RuntimeOwner {
    fn identity(&self) -> Option<ProcessIdentity> {
        Some(self.identity())
    }

    fn stop(&self, grace: Duration) -> RuntimeTerminal {
        self.stop(grace)
    }

    fn wait_terminal(&self, timeout: Duration) -> Option<RuntimeTerminal> {
        self.wait_terminal(timeout)
    }

    fn diagnostic_tail(&self) -> String {
        self.driver.diagnostic_tail()
    }

    fn diagnostic_session_id(&self) -> Option<String> {
        self.diagnostic_session_id.lock().unwrap().clone()
    }

    fn wait_diagnostics(&self, timeout: Duration) {
        self.driver.wait_diagnostics(timeout);
    }

    fn bootstrap_session(
        &self,
        task: &TaskRecord,
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        self.bootstrap_prepared_session(task, &[], timeout)
    }

    fn bootstrap_session_with_mcp(
        &self,
        task: &TaskRecord,
        mcp_servers: &[StdioMcpServer],
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        self.bootstrap_prepared_session(task, mcp_servers, timeout)
    }

    fn resume_session_with_mcp(
        &self,
        task: &TaskRecord,
        mcp_servers: &[StdioMcpServer],
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        self.resume_session_with_mcp(task, mcp_servers, timeout)
    }

    fn send_turn(
        &self,
        session_id: &str,
        content: &str,
        timeout: Duration,
    ) -> Result<Option<String>, RuntimeCommandError> {
        self.send_turn(session_id, content, timeout)
    }

    fn stop_turn(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> Result<TurnSnapshot, RuntimeCommandError> {
        self.stop_turn(session_id, timeout)
    }

    fn respond_request(
        &self,
        correlation_id: &str,
        decision: &str,
        content: Option<&str>,
        validated_denial: Option<&ValidatedPermissionDenial>,
        deadline: Instant,
    ) -> Result<(), RuntimeCommandError> {
        self.respond_request(
            correlation_id,
            decision,
            content,
            validated_denial,
            deadline,
        )
    }

    fn close_session(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> Result<(), RuntimeCommandError> {
        self.close_session(session_id, timeout)
    }

    fn turn_snapshot(&self) -> TurnSnapshot {
        self.turn_snapshot()
    }

    fn activity_snapshot(&self) -> RuntimeActivitySnapshot {
        self.turn_tracker.activity_snapshot()
    }

    fn stop_boundary_count(&self) -> u64 {
        self.stop_boundary_count()
    }

    fn finish_turn(&self, boundary: TurnBoundary, grace: Duration) -> RuntimeTerminal {
        self.finish_turn(boundary, grace)
    }
}

pub trait RuntimeFactory: Send + Sync + 'static {
    fn spawn(
        &self,
        task: &TaskRecord,
        sink: Arc<dyn LifecycleSink>,
    ) -> io::Result<Arc<dyn ManagedRuntime>>;
}

pub struct CommandRuntimeFactory<F> {
    command: F,
    require_prepared: bool,
}

impl<F> CommandRuntimeFactory<F> {
    pub fn new(command: F) -> Self {
        Self {
            command,
            require_prepared: false,
        }
    }

    pub fn new_prepared(command: F) -> Self {
        Self {
            command,
            require_prepared: true,
        }
    }
}

/// Bind the daemon-owned policy envelope to every ZCode child.
fn apply_agent_policy_environment(command: &mut Command, task: &TaskRecord) -> io::Result<()> {
    const MAX_AGENT_WRITE_MANIFEST_ENTRIES: usize = 256;
    const MAX_AGENT_WRITE_MANIFEST_BYTES: usize = 64 * 1024;
    let root = Path::new(&task.workspace_path);
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "runtime workspace path must be absolute",
        ));
    }
    let manifest = match task_route(task)
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?
    {
        TaskRoute::General(prepared) => prepared
            .write_manifest
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
    };
    let serialized = serde_json::to_string(&manifest).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("write manifest could not be serialized: {error}"),
        )
    })?;
    if manifest.len() > MAX_AGENT_WRITE_MANIFEST_ENTRIES
        || serialized.len() > MAX_AGENT_WRITE_MANIFEST_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "write manifest exceeds runtime policy bounds",
        ));
    }
    command
        .env("ZCODE_AGENT_POLICY", "1")
        .env(
            "ZCODE_AGENT_PERMISSION_MODE",
            match task_route(task)
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?
            {
                TaskRoute::General(prepared) => match prepared.permission_mode {
                    external_core::PermissionMode::Build => "build",
                    external_core::PermissionMode::Edit => "edit",
                    external_core::PermissionMode::Plan => "plan",
                    external_core::PermissionMode::Yolo => "yolo",
                },
            },
        )
        .env("ZCODE_AGENT_WORKSPACE_ROOT", root)
        .env("ZCODE_AGENT_BOOTSTRAP_ROOTS", "/Applications/ZCode.app")
        .env("ZCODE_AGENT_WRITE_MANIFEST", serialized);
    Ok(())
}

impl<F> RuntimeFactory for CommandRuntimeFactory<F>
where
    F: Fn(&TaskRecord) -> io::Result<Command> + Send + Sync + 'static,
{
    fn spawn(
        &self,
        task: &TaskRecord,
        sink: Arc<dyn LifecycleSink>,
    ) -> io::Result<Arc<dyn ManagedRuntime>> {
        if self.require_prepared {
            match task_route(task)
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?
            {
                TaskRoute::General(prepared) => {
                    prepared
                        .launcher()
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
                }
            }
        }
        let mut command = (self.command)(task)?;
        apply_agent_policy_environment(&mut command, task)?;
        Ok(Arc::new(RuntimeOwner::spawn(command, sink)?))
    }
}

fn general_initial_prompt(prepared: &PreparedGeneralTask) -> Result<String, SchedulerError> {
    prepared
        .validate_prepared_content()
        .map_err(|error| SchedulerError::InvalidConfig(error.to_string()))?;
    let caller_prompt = fs::read_to_string(&prepared.prompt_path)
        .map_err(|error| SchedulerError::InvalidConfig(error.to_string()))?;
    general_launch_prompt(prepared, &caller_prompt)
        .map_err(|error| SchedulerError::InvalidConfig(error.to_string()))
}

#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<SchedulerInner>,
}

struct SchedulerInner {
    owner_id: String,
    store: Arc<Store>,
    factory: Arc<dyn RuntimeFactory>,
    config: SchedulerConfig,
    #[cfg(test)]
    preflight_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    response_claim_hook: Mutex<Option<Arc<ResponseClaimHook>>>,
    #[cfg(test)]
    result_persist_hook: Mutex<Option<Arc<ResultPersistHook>>>,
    state: Mutex<SchedulerState>,
}

#[cfg(test)]
type ResponseClaimHook = dyn Fn(ResponseClaimHookStage, &str) + Send + Sync;

#[cfg(test)]
type ResultPersistHook = dyn Fn(&str) + Send + Sync;

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseClaimHookStage {
    BeforeClaim,
    AfterClaim,
}

#[derive(Default)]
struct SchedulerState {
    active: HashMap<String, ActiveRuntime>,
    activities: HashMap<String, Arc<PassiveActivityTracker>>,
    failures: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeLifecyclePhase {
    Running,
    StopRequested,
    StopAcknowledged,
    ForceTerminating,
    Terminal,
}

#[derive(Debug, Clone)]
struct RuntimeLifecycleSnapshot {
    phase: RuntimeLifecyclePhase,
    runtime_generation: u64,
    turn_generation: u64,
    stop_requested_at: Option<Instant>,
    observed_boundary: Option<TurnBoundary>,
    force_termination_count: u64,
    late_event_count: u64,
}

struct RuntimeLifecycle {
    state: Mutex<RuntimeLifecycleSnapshot>,
}

const MAX_BOUNDED_LATE_EVENT_DIAGNOSTICS: u64 = 64;

impl RuntimeLifecycle {
    fn new(runtime_generation: u64) -> Self {
        Self {
            state: Mutex::new(RuntimeLifecycleSnapshot {
                phase: RuntimeLifecyclePhase::Running,
                runtime_generation,
                turn_generation: 0,
                stop_requested_at: None,
                observed_boundary: None,
                force_termination_count: 0,
                late_event_count: 0,
            }),
        }
    }

    #[cfg(test)]
    fn snapshot(&self) -> RuntimeLifecycleSnapshot {
        self.state.lock().unwrap().clone()
    }

    fn request_stop(&self, turn: &TurnSnapshot) {
        let mut state = self.state.lock().unwrap();
        if state.phase == RuntimeLifecyclePhase::Running {
            state.phase = RuntimeLifecyclePhase::StopRequested;
            state.turn_generation = turn.generation;
            state.stop_requested_at = Some(Instant::now());
        }
    }

    fn acknowledge_boundary(&self, turn: &TurnSnapshot) -> bool {
        let mut state = self.state.lock().unwrap();
        if matches!(
            state.phase,
            RuntimeLifecyclePhase::StopRequested | RuntimeLifecyclePhase::StopAcknowledged
        ) && turn.generation == state.turn_generation
            && !turn.active
            && turn.boundary.is_some()
        {
            state.phase = RuntimeLifecyclePhase::StopAcknowledged;
            state.observed_boundary = turn.boundary;
            true
        } else {
            false
        }
    }

    fn force_terminating(&self) {
        let mut state = self.state.lock().unwrap();
        if !matches!(
            state.phase,
            RuntimeLifecyclePhase::ForceTerminating | RuntimeLifecyclePhase::Terminal
        ) {
            state.phase = RuntimeLifecyclePhase::ForceTerminating;
            state.force_termination_count = state.force_termination_count.saturating_add(1);
        }
    }

    fn terminalize(&self) {
        self.state.lock().unwrap().phase = RuntimeLifecyclePhase::Terminal;
    }

    fn ingress_reason(&self) -> Option<&'static str> {
        let state = self.state.lock().unwrap();
        debug_assert!(state.runtime_generation > 0);
        match state.phase {
            RuntimeLifecyclePhase::Running => None,
            RuntimeLifecyclePhase::StopRequested
            | RuntimeLifecyclePhase::StopAcknowledged
            | RuntimeLifecyclePhase::ForceTerminating => Some("TASK_STOPPING"),
            RuntimeLifecyclePhase::Terminal => Some("LATE_AFTER_STOP"),
        }
    }

    fn admit_event(&self) -> Option<MutexGuard<'_, RuntimeLifecycleSnapshot>> {
        let mut state = self.state.lock().unwrap();
        if state.phase == RuntimeLifecyclePhase::Running {
            return Some(state);
        }
        state.late_event_count = state
            .late_event_count
            .saturating_add(1)
            .min(MAX_BOUNDED_LATE_EVENT_DIAGNOSTICS);
        None
    }
}

struct ActiveRuntime {
    owner_epoch: u64,
    runtime: Arc<dyn ManagedRuntime>,
    sink: Arc<StoreLifecycleSink>,
    session_id: String,
    operation: Arc<Mutex<()>>,
    runtime_lifecycle: Arc<RuntimeLifecycle>,
    route: TaskRoute,
    task: Option<TaskRecord>,
    check: Arc<ActiveCheck>,
}

#[derive(Debug, Default)]
struct ActiveCheck {
    cancelled: AtomicBool,
}

impl ActiveCheck {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

struct TerminalTarget<'a> {
    agent_id: &'a str,
    sink: &'a StoreLifecycleSink,
    route: &'a TaskRoute,
    runtime: &'a Arc<dyn ManagedRuntime>,
}

struct TerminalDecision {
    terminal: RuntimeTerminal,
    natural_completion: bool,
    forced_outcome: Option<(CompletionOutcome, String)>,
    failure_message: Option<String>,
}

struct MonitorContext {
    agent_id: String,
    owner_epoch: u64,
    runtime: Arc<dyn ManagedRuntime>,
    sink: Arc<StoreLifecycleSink>,
    session_id: String,
    operation: Arc<Mutex<()>>,
    runtime_lifecycle: Arc<RuntimeLifecycle>,
    route: TaskRoute,
    task: Option<TaskRecord>,
    check: Arc<ActiveCheck>,
}

type ActiveSession = (
    u64,
    Arc<dyn ManagedRuntime>,
    String,
    Arc<Mutex<()>>,
    Arc<RuntimeLifecycle>,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDisposition {
    Queued,
    Delivered,
    AlreadyDelivered,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseDisposition {
    Responded,
    AlreadyResponded,
    InFlight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseOutcome {
    pub disposition: ResponseDisposition,
    pub requested_decision: String,
    pub effective_decision: String,
    pub policy_overrode: bool,
    pub policy_reason_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmittedTask {
    pub task: TaskRecord,
    pub disposition: TaskSubmissionDisposition,
}

struct StoreLifecycleSink {
    store: Arc<Store>,
    agent_id: String,
    runtime_agent_id: String,
    owner_epoch: u64,
    runtime_lifecycle: Arc<RuntimeLifecycle>,
    activity: Arc<PassiveActivityTracker>,
    write_state: Mutex<SinkWriteState>,
    #[cfg(test)]
    after_admission_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    before_natural_completion_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    after_result_persist_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NaturalCompletionAdmission {
    Ready,
    Deferred { pending: bool, queued: bool },
    Bypass,
}

#[derive(Default)]
struct SinkWriteState {
    first_error: Option<String>,
    last_source_sequence: u64,
    pending_terminal_sequence: Option<u64>,
    terminal_written: bool,
}

struct LifecycleProjection {
    event_type: &'static str,
    payload_json: String,
    redaction_level: &'static str,
}

impl StoreLifecycleSink {
    fn new(
        store: Arc<Store>,
        agent_id: String,
        runtime_agent_id: String,
        owner_epoch: u64,
        runtime_lifecycle: Arc<RuntimeLifecycle>,
        activity: Arc<PassiveActivityTracker>,
    ) -> Self {
        Self {
            store,
            agent_id,
            runtime_agent_id,
            owner_epoch,
            runtime_lifecycle,
            activity,
            write_state: Mutex::new(SinkWriteState::default()),
            #[cfg(test)]
            after_admission_hook: Mutex::new(None),
            #[cfg(test)]
            before_natural_completion_hook: Mutex::new(None),
            #[cfg(test)]
            after_result_persist_hook: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn set_after_admission_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.after_admission_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    fn set_before_natural_completion_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.before_natural_completion_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    fn set_after_result_persist_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.after_result_persist_hook.lock().unwrap() = Some(hook);
    }

    fn begin_natural_completion(&self) -> Result<NaturalCompletionAdmission, StoreError> {
        #[cfg(test)]
        if let Some(hook) = self.before_natural_completion_hook.lock().unwrap().clone() {
            hook();
        }
        let mut runtime_lifecycle = self.runtime_lifecycle.state.lock().unwrap();
        if runtime_lifecycle.phase != RuntimeLifecyclePhase::Running {
            return Ok(NaturalCompletionAdmission::Bypass);
        }
        let (pending, queued) = self.store.completion_blockers(&self.agent_id)?;
        if pending || queued {
            return Ok(NaturalCompletionAdmission::Deferred { pending, queued });
        }
        runtime_lifecycle.phase = RuntimeLifecyclePhase::Terminal;
        Ok(NaturalCompletionAdmission::Ready)
    }

    fn finish_general(
        &self,
        terminal: &RuntimeTerminal,
        prepared: &PreparedGeneralTask,
        completion: &GeneralCompletion,
    ) -> Result<TaskPhase, StoreError> {
        let mut state = self.write_state.lock().unwrap();
        if let Some(error) = &state.first_error {
            return Err(StoreError::InvalidState(error.clone()));
        }
        if state.terminal_written {
            return self
                .store
                .get_task(&self.agent_id)?
                .map(|job| job.phase)
                .ok_or_else(|| StoreError::InvalidState("terminal task disappeared".into()));
        }
        let source_sequence = state
            .pending_terminal_sequence
            .unwrap_or_else(|| state.last_source_sequence.saturating_add(1));
        let projection = lifecycle_projection(&RuntimeEvent::Terminal(terminal.clone()), None);
        self.store.append_lifecycle(&LifecycleWrite {
            agent_id: self.agent_id.clone(),
            runtime_agent_id: self.runtime_agent_id.clone(),
            owner_epoch: self.owner_epoch,
            source_sequence,
            event_type: projection.event_type.into(),
            turn_id: None,
            payload_json: projection.payload_json,
            redaction_level: projection.redaction_level.into(),
            terminal: None,
            turn_state: None,
        })?;
        let reap_after_persist =
            completion.cleaned && terminal_proves_process_group_reaped(terminal);
        persist_general_result(&self.store, &self.agent_id, prepared, completion)?;
        #[cfg(test)]
        if let Some(hook) = self.after_result_persist_hook.lock().unwrap().clone() {
            hook();
        }
        if reap_after_persist {
            self.store.reap_task(&self.agent_id)?;
        }
        state.terminal_written = true;
        self.store
            .get_task(&self.agent_id)?
            .map(|job| job.phase)
            .ok_or_else(|| StoreError::InvalidState("terminal task disappeared".into()))
    }

    fn error(&self) -> Option<String> {
        self.write_state.lock().unwrap().first_error.clone()
    }
}

fn persist_general_result(
    store: &Store,
    agent_id: &str,
    prepared: &PreparedGeneralTask,
    completion: &GeneralCompletion,
) -> Result<(), StoreError> {
    let result = task_result(completion);
    let _ = prepared;
    store_result_with_cancel_precedence(store, agent_id, &result)
}

fn store_result_with_cancel_precedence(
    store: &Store,
    agent_id: &str,
    result: &TaskResult,
) -> Result<(), StoreError> {
    match store.store_task_result(agent_id, result) {
        Ok(()) => Ok(()),
        Err(error @ StoreError::Conflict(_)) => {
            let task = store.get_task(agent_id)?.ok_or_else(|| {
                StoreError::InvalidState("terminal result task disappeared".into())
            })?;
            if (task.stop_requested || task.close_requested)
                && result.outcome != TaskOutcome::Cancelled
                && store.task_result(agent_id)?.is_none()
            {
                store.store_task_result(agent_id, &bounded_cancelled_task_result())
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

fn task_result(completion: &GeneralCompletion) -> TaskResult {
    let summary = if completion.summary.trim().is_empty() {
        completion
            .reason_code
            .clone()
            .unwrap_or_else(|| format!("general task ended with {:?}", completion.outcome))
    } else {
        completion.summary.clone()
    };
    TaskResult {
        outcome: task_outcome(completion.outcome),
        final_text: summary,
        partial: completion.outcome != CompletionOutcome::Completed,
    }
}

fn task_outcome(outcome: CompletionOutcome) -> TaskOutcome {
    match outcome {
        CompletionOutcome::Completed => TaskOutcome::Completed,
        CompletionOutcome::Failed => TaskOutcome::Failed,
        CompletionOutcome::Cancelled => TaskOutcome::Cancelled,
        CompletionOutcome::TimedOut => TaskOutcome::TimedOut,
        CompletionOutcome::RuntimeLost => TaskOutcome::RuntimeLost,
        CompletionOutcome::ResultInvalid => TaskOutcome::ResultInvalid,
    }
}

fn minimal_task_result(outcome: CompletionOutcome, summary: &str, reason_code: &str) -> TaskResult {
    TaskResult {
        outcome: task_outcome(outcome),
        final_text: if summary.trim().is_empty() {
            reason_code.into()
        } else {
            summary.into()
        },
        partial: outcome != CompletionOutcome::Completed,
    }
}

fn bounded_cancelled_task_result() -> TaskResult {
    TaskResult {
        outcome: TaskOutcome::Cancelled,
        final_text: "task cancelled".into(),
        partial: true,
    }
}

fn bounded_result_invalid_task_result() -> TaskResult {
    TaskResult {
        outcome: TaskOutcome::ResultInvalid,
        final_text: "result unavailable".into(),
        partial: true,
    }
}

#[derive(Debug, Clone, Copy)]
struct UnstartedTerminal<'a> {
    outcome: CompletionOutcome,
    reason_code: &'a str,
    message: &'a str,
}

fn finalized_general(
    prepared: &PreparedGeneralTask,
    outcome: CompletionOutcome,
    reason_code: &str,
    message: &str,
) -> GeneralCompletion {
    let mut completion = GeneralFinalizer::finalize(prepared, outcome);
    if completion.summary.trim().is_empty() {
        completion.summary = if message.trim().is_empty() {
            reason_code.into()
        } else {
            message.into()
        };
    }
    if completion.reason_code.is_none() && outcome != CompletionOutcome::Completed {
        completion.reason_code = Some(reason_code.into());
    }
    completion
}

fn unreaped_general(
    outcome: CompletionOutcome,
    reason_code: &str,
    message: &str,
) -> GeneralCompletion {
    GeneralCompletion {
        outcome,
        reason_code: (outcome != CompletionOutcome::Completed).then(|| reason_code.into()),
        summary: if message.trim().is_empty() {
            reason_code.into()
        } else {
            message.into()
        },
        residual_gaps: Vec::new(),
        cleaned: false,
    }
}

impl LifecycleSink for StoreLifecycleSink {
    fn emit(&self, record: LifecycleRecord) {
        let Some(_admission) = self.runtime_lifecycle.admit_event() else {
            return;
        };
        #[cfg(test)]
        if let Some(hook) = self.after_admission_hook.lock().unwrap().clone() {
            hook();
        }
        self.activity.observe(&record.event);
        let mut state = self.write_state.lock().unwrap();
        if state.first_error.is_some() {
            return;
        }
        state.last_source_sequence = state.last_source_sequence.max(record.sequence);
        if matches!(record.event, RuntimeEvent::Terminal(_)) {
            state.pending_terminal_sequence = Some(record.sequence);
            return;
        }
        let pending_request_id = match &record.event {
            RuntimeEvent::Driver(Inbound::Message(WireMessage::Request(request)))
                if matches!(
                    request.method.as_str(),
                    INTERACTION_REQUEST_PERMISSION | INTERACTION_REQUEST_USER_INPUT
                ) =>
            {
                let request_id = format!("{}:request:{}", self.agent_id, record.sequence);
                let correlation_id = match serde_json::to_string(&request.id) {
                    Ok(value) => value,
                    Err(error) => {
                        state.first_error = Some(error.to_string());
                        return;
                    }
                };
                let request_type = if request.method == INTERACTION_REQUEST_PERMISSION {
                    "permission"
                } else {
                    "unsupported_input"
                };
                if let Err(error) = self.store.insert_pending_request(
                    &request_id,
                    &self.agent_id,
                    &correlation_id,
                    request_type,
                    &request.params.to_string(),
                ) {
                    state.first_error = Some(error.to_string());
                    return;
                }
                Some(request_id)
            }
            _ => None,
        };
        let projection = lifecycle_projection(&record.event, pending_request_id.as_deref());
        let write = LifecycleWrite {
            agent_id: self.agent_id.clone(),
            runtime_agent_id: self.runtime_agent_id.clone(),
            owner_epoch: self.owner_epoch,
            source_sequence: record.sequence,
            event_type: projection.event_type.into(),
            turn_id: None,
            payload_json: projection.payload_json,
            redaction_level: projection.redaction_level.into(),
            terminal: None,
            turn_state: match &record.event {
                RuntimeEvent::Driver(Inbound::Lifecycle { method, .. }) => match method.as_str() {
                    "turn.started" => Some(TurnState::Active),
                    "turn.completed" => Some(TurnState::Idle),
                    "turn.failed" => Some(TurnState::Failed),
                    _ => None,
                },
                _ => None,
            },
        };
        if let Err(error) = self.store.append_lifecycle(&write) {
            state.first_error = Some(error.to_string());
        }
    }
}

fn lifecycle_projection(
    event: &RuntimeEvent,
    pending_request_id: Option<&str>,
) -> LifecycleProjection {
    let (event_type, payload, redaction_level) = match event {
        RuntimeEvent::Driver(Inbound::Message(WireMessage::Request(request))) => (
            "driver.message",
            serde_json::json!({
                "kind": "request",
                "method": request.method,
                "request_id": pending_request_id,
            }),
            "redacted",
        ),
        RuntimeEvent::Driver(Inbound::Message(WireMessage::Response(response))) => (
            "driver.message",
            serde_json::json!({
                "kind": "response",
                "outcome": if response.error.is_some() { "error" } else { "result" },
            }),
            "redacted",
        ),
        RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(message))) => (
            "driver.message",
            serde_json::json!({
                "kind": "event",
                "method": message.method,
                "type": event_type(message),
            }),
            "redacted",
        ),
        RuntimeEvent::Driver(Inbound::Message(WireMessage::UnknownEvent { .. })) => (
            "raw.unknown",
            serde_json::json!({"kind": "unknown_event", "raw": "[REDACTED]"}),
            "redacted",
        ),
        RuntimeEvent::Driver(Inbound::Lifecycle {
            sequence,
            method,
            order,
        }) => (
            "driver.lifecycle",
            serde_json::json!({
                "kind": "lifecycle",
                "sequence": sequence,
                "method": method,
                "order": lifecycle_order_name(order),
            }),
            "allowlisted",
        ),
        RuntimeEvent::Driver(Inbound::Malformed(_)) => (
            "driver.malformed",
            serde_json::json!({"kind": "malformed", "detail": "[REDACTED]"}),
            "redacted",
        ),
        RuntimeEvent::Driver(Inbound::OversizedLine { bytes }) => (
            "driver.oversized_line",
            serde_json::json!({"kind": "oversized_line", "bytes": bytes}),
            "allowlisted",
        ),
        RuntimeEvent::Driver(Inbound::ChildExited(exit)) => (
            "driver.child_exited",
            serde_json::json!({"kind": "child_exited", "outcome": child_exit_name(exit)}),
            "allowlisted",
        ),
        RuntimeEvent::Driver(Inbound::UnmatchedResponse { id: _, outcome }) => (
            "driver.unmatched_response",
            serde_json::json!({"kind": "unmatched_response", "outcome": outcome}),
            "redacted",
        ),
        RuntimeEvent::Terminal(RuntimeTerminal::Stopped(outcome)) => (
            "runtime.stopped",
            serde_json::json!({"kind": "stopped", "outcome": stop_outcome_name(outcome)}),
            "allowlisted",
        ),
        RuntimeEvent::Terminal(RuntimeTerminal::Completed(outcome)) => (
            "runtime.completed",
            serde_json::json!({"kind": "completed", "outcome": stop_outcome_name(outcome)}),
            "allowlisted",
        ),
        RuntimeEvent::Terminal(RuntimeTerminal::FailedTurn(outcome)) => (
            "runtime.turn_failed",
            serde_json::json!({"kind": "turn_failed", "outcome": stop_outcome_name(outcome)}),
            "allowlisted",
        ),
        RuntimeEvent::Terminal(RuntimeTerminal::Exited(exit)) => (
            "runtime.exited",
            serde_json::json!({"kind": "exited", "outcome": child_exit_name(exit)}),
            "allowlisted",
        ),
        RuntimeEvent::Terminal(RuntimeTerminal::FailedRuntimeLost(loss)) => (
            "runtime.failed_runtime_lost",
            serde_json::json!({"kind": "failed_runtime_lost", "reason": runtime_loss_name(loss)}),
            runtime_loss_redaction(loss),
        ),
        RuntimeEvent::Terminal(RuntimeTerminal::Orphaned(loss)) => (
            "runtime.orphaned",
            serde_json::json!({"kind": "orphaned", "reason": runtime_loss_name(loss)}),
            runtime_loss_redaction(loss),
        ),
    };
    LifecycleProjection {
        event_type,
        payload_json: payload.to_string(),
        redaction_level,
    }
}

fn lifecycle_order_name(order: &LifecycleOrder) -> &'static str {
    match order {
        LifecycleOrder::NotLifecycle => "not_lifecycle",
        LifecycleOrder::InOrder => "in_order",
        LifecycleOrder::OutOfOrder { .. } => "out_of_order",
    }
}

fn child_exit_name(exit: &ChildExit) -> &'static str {
    match exit {
        ChildExit::Exited(Some(0)) => "exited_success",
        ChildExit::Exited(Some(_)) => "exited_failure",
        ChildExit::Exited(None) => "exited_unknown",
        ChildExit::Signaled(_) => "signaled",
        ChildExit::Unknown => "unknown",
    }
}

fn stop_outcome_name(outcome: &StopOutcome) -> &'static str {
    match outcome {
        StopOutcome::AlreadyExited(_) => "already_exited",
        StopOutcome::Terminated(_) => "terminated",
    }
}

fn runtime_loss_name(loss: &RuntimeLoss) -> &'static str {
    match loss {
        RuntimeLoss::InvalidIdentity => "invalid_identity",
        RuntimeLoss::UnsupportedIdentity => "unsupported_identity",
        RuntimeLoss::MissingLeader => "missing_leader",
        RuntimeLoss::IdentityMismatch => "identity_mismatch",
        RuntimeLoss::UnknownMembership => "unknown_membership",
        RuntimeLoss::SessionLost => "session_lost",
        RuntimeLoss::StopFailed(_) => "stop_failed",
        RuntimeLoss::EventStreamLost => "event_stream_lost",
    }
}

fn runtime_loss_redaction(loss: &RuntimeLoss) -> &'static str {
    if matches!(loss, RuntimeLoss::StopFailed(_)) {
        "redacted"
    } else {
        "allowlisted"
    }
}

struct DiagnosticLogger {
    sender: SyncSender<String>,
    dropped: Arc<AtomicU64>,
}

impl DiagnosticLogger {
    fn start<W: Write + Send + 'static>(mut writer: W) -> io::Result<Self> {
        let (sender, receiver) = sync_channel::<String>(DIAGNOSTIC_QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let pending_drops = Arc::clone(&dropped);
        thread::Builder::new()
            .name("zcode-diagnostic-write".into())
            .spawn(move || {
                while let Ok(line) = receiver.recv() {
                    let count = pending_drops.swap(0, Ordering::Relaxed);
                    if count > 0 {
                        let marker = format!("[zcode-agentd] diagnostic_writes_dropped={count}\n");
                        if writer.write_all(marker.as_bytes()).is_err() {
                            pending_drops.fetch_add(count, Ordering::Relaxed);
                        }
                    }
                    if writer.write_all(line.as_bytes()).is_err() {
                        pending_drops.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })?;
        Ok(Self { sender, dropped })
    }

    fn submit(&self, line: String) {
        // Bound both the queue length and each entry before enqueueing. Logging
        // never blocks scheduler control, even when the sink stops consuming.
        if line.len() > DIAGNOSTIC_RECORD_BYTES || self.sender.try_send(line).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct RotatingDiagnosticWriter {
    path: PathBuf,
}

impl RotatingDiagnosticWriter {
    fn copy_tail(source: &Path, destination: &Path) -> io::Result<()> {
        let mut input = match fs::File::open(source) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let size = input.metadata()?.len();
        input.seek(SeekFrom::Start(size.saturating_sub(DIAGNOSTIC_FILE_BYTES)))?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(destination)?;
        io::copy(&mut input.take(DIAGNOSTIC_FILE_BYTES), &mut output)?;
        Ok(())
    }
}

impl Write for RotatingDiagnosticWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > DIAGNOSTIC_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "diagnostic record exceeds file cap",
            ));
        }
        let mut output = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&self.path)?;
        if output.metadata()?.len().saturating_add(bytes.len() as u64) > DIAGNOSTIC_FILE_BYTES {
            let first = PathBuf::from(format!("{}.1", self.path.display()));
            let second = PathBuf::from(format!("{}.2", self.path.display()));
            Self::copy_tail(&first, &second)?;
            Self::copy_tail(&self.path, &first)?;
            // Keep the inode: launchd still owns an open stderr descriptor.
            // Renaming would strand that descriptor on an old rotation.
            output.set_len(0)?;
        }
        output.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod failure_log_tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{self, Write};
    use std::sync::mpsc;
    use std::time::Duration;

    fn diagnostic_scheduler(script: &str) -> (tempfile::TempDir, Scheduler, String) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        fs::create_dir_all(&root).unwrap();
        let workspace = tempfile::Builder::new()
            .prefix("s02-fault-")
            .tempdir_in(root)
            .unwrap();
        let store = Arc::new(Store::open(workspace.path().join("state.sqlite")).unwrap());
        let script = script.to_owned();
        let factory = CommandRuntimeFactory::new(move |_: &TaskRecord| {
            let mut command = Command::new("sh");
            command.args(["-c", &script]);
            Ok(command)
        });
        let scheduler = Scheduler::new(
            "diagnostic-test",
            store,
            Arc::new(factory),
            SchedulerConfig::default(),
        )
        .unwrap();
        let submitted = scheduler
            .enqueue_general(&GeneralTaskManifest {
                schema: "zcode-general-task/v1".into(),
                agent_id: "diagnostic-agent".into(),
                repository: workspace.path().canonicalize().unwrap(),
                permission_mode: external_core::PermissionMode::Plan,
                prompt: "diagnostic fixture".into(),
                write_manifest: Vec::new(),
            })
            .unwrap();
        (workspace, scheduler, submitted.task.agent_id)
    }

    fn await_result(scheduler: &Scheduler, agent_id: &str) -> external_store::StoredTaskResult {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = scheduler.store().task_result(agent_id).unwrap() {
                return result;
            }
            assert!(Instant::now() < deadline, "no terminal result");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn failure_record(scheduler: &Scheduler, agent_id: &str) -> serde_json::Value {
        let record = scheduler
            .last_error(agent_id)
            .expect("correlated failure record");
        assert!(record.len() < 192 * 1024);
        let record: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert_eq!(record["agent_id"], agent_id);
        record
    }

    #[test]
    fn enqueue_preparation_failure_consumes_id_and_preserves_classification() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let factory = CommandRuntimeFactory::new(|_: &TaskRecord| {
            let mut command = Command::new("sh");
            command.args(["-c", "exit 0"]);
            Ok(command)
        });
        let scheduler = Scheduler::new(
            "s02-preparation-failure",
            Arc::clone(&store),
            Arc::new(factory),
            SchedulerConfig::default(),
        )
        .unwrap();
        let invalid = GeneralTaskManifest {
            schema: "zcode-general-task/v1".into(),
            agent_id: "caller-value-is-ignored".into(),
            repository: directory.path().join("does-not-exist"),
            permission_mode: external_core::PermissionMode::Plan,
            prompt: "preparation failure".into(),
            write_manifest: Vec::new(),
        };
        let error = scheduler.enqueue_general(&invalid).unwrap_err();
        assert!(matches!(error, SchedulerError::InvalidConfig(_)));

        let valid = GeneralTaskManifest {
            repository: directory.path().canonicalize().unwrap(),
            prompt: "valid submission".into(),
            ..invalid
        };
        let submitted = scheduler.enqueue_general(&valid).unwrap();
        assert_eq!(submitted.task.agent_id, "10000001");
        assert!(store.get_task("10000000").unwrap().is_none());
    }

    #[test]
    fn startup_and_protocol_failures_record_stderr_without_changing_result() {
        let cases = [
            (
                "read request; printf startup-tail >&2; exit 7",
                None,
                "startup-tail",
            ),
            (
                r#"read request; printf invalid-projection-tail >&2; printf '%s\n' '{"id":1,"result":{}}'; sleep 2"#,
                None,
                "invalid-projection-tail",
            ),
            (
                r#"read request; printf '%s\n' '{"id":1,"result":{"session":{"sessionId":"known-session"}}}'; read request; printf subscribe-tail >&2; printf '%s\n' '{"id":2,"error":{"code":-1,"message":"reject"}}'; sleep 2"#,
                Some("known-session"),
                "subscribe-tail",
            ),
        ];
        for (script, session, tail) in cases {
            let (_workspace, scheduler, agent_id) = diagnostic_scheduler(script);
            assert!(scheduler.start_ready().is_err());
            let record = failure_record(&scheduler, &agent_id);
            assert_eq!(record["stage"], "session_start");
            assert_eq!(record["error_code"], "SESSION_START_FAILED");
            assert_eq!(record["session_id"].as_str(), session);
            assert!(record["stderr_tail"].as_str().unwrap().contains(tail));
            let result = await_result(&scheduler, &agent_id);
            let result_json = serde_json::to_string(&result.result).unwrap();
            assert!(
                !result_json.contains(tail),
                "stderr leaked into task result"
            );
            assert_eq!(
                scheduler
                    .store()
                    .get_task(&agent_id)
                    .unwrap()
                    .unwrap()
                    .outcome,
                Some(TaskOutcome::Failed)
            );
        }
    }

    const RUNNING_PROTOCOL: &str = r#"
read request
printf '%s\n' '{"id":1,"result":{"session":{"sessionId":"running-session"}}}'
read request
printf '%s\n' '{"id":2,"result":{}}'
read request
printf '%s\n' '{"id":3,"result":{}}' '{"method":"session/event","params":{"type":"turn.started"}}'
sleep 0.1
"#;

    #[test]
    fn scheduler_queue_drains_once_through_driver_and_persists_receipt() {
        let (workspace, mut scheduler, agent_id) = diagnostic_scheduler("unused");
        let directory = workspace.path().to_owned();
        let script = format!(
            r#"{RUNNING_PROTOCOL}
while [ ! -f release-turn ]; do sleep 0.01; done
printf '%s\n' '{{"method":"session/event","params":{{"type":"turn.completed"}}}}'
read request
printf '%s\n' "$request" >> deliveries.jsonl
printf '%s\n' '{{"id":4,"result":{{"turnId":"queued-turn"}}}}' '{{"method":"session/event","params":{{"type":"turn.started"}}}}'
while [ ! -f finish-turn ]; do sleep 0.01; done
printf '%s\n' '{{"method":"session/event","params":{{"type":"model.streaming","payload":{{"kind":"text_delta","delta":"queued answer","assistantMessageId":"m2"}}}}}}' '{{"method":"session/event","params":{{"type":"message.finished","payload":{{"assistantMessageId":"m2"}}}}}}' '{{"method":"session/event","params":{{"type":"turn.completed"}}}}'
while read request; do printf '%s\n' "$request" >> deliveries.jsonl; done
"#
        );
        Arc::get_mut(&mut scheduler.inner).unwrap().factory =
            Arc::new(CommandRuntimeFactory::new(move |_: &TaskRecord| {
                let mut command = Command::new("sh");
                command.args(["-c", &script]).current_dir(&directory);
                Ok(command)
            }));
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        for _ in 0..2 {
            assert_eq!(
                scheduler
                    .queue_message(&agent_id, "counted", "queue", "follow-up")
                    .unwrap(),
                MessageDisposition::Queued
            );
        }
        fs::write(workspace.path().join("release-turn"), "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let receipt = loop {
            let receipt = scheduler.store().message("counted").unwrap().unwrap();
            if receipt.state == MessageState::Delivered {
                break receipt;
            }
            assert!(
                Instant::now() < deadline,
                "queue was not delivered: {receipt:?}"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(receipt.target_turn_id.as_deref(), Some("queued-turn"));
        assert!(receipt.delivered_at.is_some());
        assert_eq!(
            scheduler
                .queue_message(&agent_id, "counted", "queue", "follow-up")
                .unwrap(),
            MessageDisposition::AlreadyDelivered
        );
        fs::write(workspace.path().join("finish-turn"), "").unwrap();
        assert_eq!(
            await_result(&scheduler, &agent_id).result.final_text,
            "queued answer"
        );
        let deliveries = fs::read_to_string(workspace.path().join("deliveries.jsonl")).unwrap();
        let sends: Vec<serde_json::Value> = deliveries
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|v: &serde_json::Value| v["method"] == "session/send")
            .collect();
        assert_eq!(sends.len(), 1, "duplicate queue delivery: {deliveries}");
        assert!(sends[0].to_string().contains("follow-up"));
        assert_eq!(
            scheduler.store().message("counted").unwrap().unwrap(),
            receipt
        );
    }

    #[test]
    fn scheduler_restart_reaps_runtime_without_replaying_unknown_sending() {
        for cancelled in [false, true] {
            let (workspace, scheduler, agent_id) = diagnostic_scheduler("unused");
            let store = scheduler.store();
            let claim = store.claim_next("old-daemon", 1, 1).unwrap().unwrap();
            // An actual isolated child supplies the persisted identity. No monitor is
            // attached: this models loss of the daemon after claiming delivery.
            let mut command = Command::new("sh");
            command.args(["-c", "exec sleep 30"]);
            let driver = Driver::spawn(command).unwrap();
            let identity = driver.identity();
            assert!(store
                .mark_session_running(
                    &agent_id,
                    claim.owner_epoch,
                    "old-runtime",
                    Some(&StoredProcessIdentity {
                        pid: identity.pid,
                        process_group_id: identity.pgid,
                        uid: identity.uid,
                        start_token: identity.start_token.clone(),
                    }),
                    Some("old-session"),
                    Some(TurnState::Active)
                )
                .unwrap());
            scheduler
                .queue_message(&agent_id, "unknown", "queue", "never replay")
                .unwrap();
            assert_eq!(
                store.claim_next_message(&agent_id).unwrap().unwrap().state,
                MessageState::Sending
            );
            if cancelled {
                store.request_stop(&agent_id).unwrap();
            }
            drop(scheduler);
            drop(store);
            let reopened = Arc::new(Store::open(workspace.path().join("state.sqlite")).unwrap());
            let spawns = Arc::new(AtomicU64::new(0));
            let count = Arc::clone(&spawns);
            let factory = CommandRuntimeFactory::new(move |_: &TaskRecord| {
                count.fetch_add(1, Ordering::SeqCst);
                Err(io::Error::other("recovery must not spawn or replay"))
            });
            let recovered = Scheduler::new(
                "new-daemon",
                Arc::clone(&reopened),
                Arc::new(factory),
                SchedulerConfig::default(),
            )
            .unwrap();
            let expected = if cancelled {
                TaskOutcome::Cancelled
            } else {
                TaskOutcome::RuntimeLost
            };
            assert_eq!(
                recovered.reconcile_startup().unwrap(),
                vec![(agent_id.clone(), expected)]
            );
            let task = reopened.get_task(&agent_id).unwrap().unwrap();
            assert_eq!(task.phase, TaskPhase::Terminal);
            assert_eq!(task.outcome, Some(expected));
            assert!(task.reaped_at.is_some());
            assert!(observe_process_group(identity.pgid).unwrap().is_empty());
            driver.wait().unwrap();
            let receipt = reopened.message("unknown").unwrap().unwrap();
            assert_eq!(receipt.state, MessageState::Failed);
            assert!(receipt.delivered_at.is_none());
            assert!(!reopened
                .complete_message("unknown", Some("late-turn"))
                .unwrap());
            assert!(recovered.start_ready().unwrap().is_empty());
            assert!(recovered.reconcile_startup().unwrap().is_empty());
            assert_eq!(spawns.load(Ordering::SeqCst), 0);
            assert_eq!(
                reopened
                    .task_result(&agent_id)
                    .unwrap()
                    .unwrap()
                    .result
                    .outcome,
                expected
            );
        }
    }

    #[test]
    fn abnormal_exit_records_correlated_tail_and_preserves_runtime_lost_outcome() {
        let script = format!("{RUNNING_PROTOCOL}\nprintf abnormal-exit-tail >&2; exit 7");
        let (_workspace, scheduler, agent_id) = diagnostic_scheduler(&script);
        scheduler.start_ready().unwrap();
        let result = await_result(&scheduler, &agent_id);
        let record = failure_record(&scheduler, &agent_id);
        assert_eq!(record["stage"], "runtime_terminal");
        assert_eq!(record["session_id"], "running-session");
        assert_eq!(record["error_code"], "RUNTIME_TERMINAL");
        assert!(record["stderr_tail"]
            .as_str()
            .unwrap()
            .contains("abnormal-exit-tail"));
        assert!(!serde_json::to_string(&result.result)
            .unwrap()
            .contains("abnormal-exit-tail"));
        assert_eq!(
            scheduler
                .store()
                .get_task(&agent_id)
                .unwrap()
                .unwrap()
                .outcome,
            Some(TaskOutcome::RuntimeLost)
        );
    }

    #[test]
    fn successful_runtime_stderr_is_not_published_as_failure_or_final_text() {
        let script = format!(
            r#"{RUNNING_PROTOCOL}
printf normal-stderr-tail >&2
printf '%s\n' '{{"method":"session/event","params":{{"type":"model.streaming","payload":{{"kind":"text_delta","delta":"task answer","assistantMessageId":"m1"}}}}}}' '{{"method":"session/event","params":{{"type":"message.finished","payload":{{"assistantMessageId":"m1"}}}}}}' '{{"method":"session/event","params":{{"type":"turn.completed"}}}}'
sleep 2
"#
        );
        let (_workspace, scheduler, agent_id) = diagnostic_scheduler(&script);
        scheduler.start_ready().unwrap();
        let result = await_result(&scheduler, &agent_id);
        assert!(scheduler.last_error(&agent_id).is_none());
        let result_json = serde_json::to_string(&result.result).unwrap();
        assert!(result_json.contains("task answer"));
        assert!(!result_json.contains("normal-stderr-tail"));
        assert_eq!(
            scheduler
                .store()
                .get_task(&agent_id)
                .unwrap()
                .unwrap()
                .outcome,
            Some(TaskOutcome::Completed)
        );
    }

    #[test]
    fn natural_completion_queued_send_failure_records_tail_without_changing_outcomes() {
        struct CompletedRuntime;
        impl ManagedRuntime for CompletedRuntime {
            fn identity(&self) -> Option<ProcessIdentity> {
                None
            }
            fn stop(&self, _: Duration) -> RuntimeTerminal {
                RuntimeTerminal::Completed(StopOutcome::AlreadyExited(ChildExit::Exited(Some(0))))
            }
            fn wait_terminal(&self, _: Duration) -> Option<RuntimeTerminal> {
                Some(self.stop(Duration::ZERO))
            }
            fn bootstrap_session(
                &self,
                _: &TaskRecord,
                _: Duration,
            ) -> Result<SessionReady, RuntimeCommandError> {
                Ok(SessionReady {
                    session_id: "completed-session".into(),
                    initial_turn_id: None,
                    configured_model: None,
                })
            }
            fn send_turn(
                &self,
                _: &str,
                _: &str,
                _: Duration,
            ) -> Result<Option<String>, RuntimeCommandError> {
                Err(RuntimeCommandError::Remote(serde_json::json!({
                    "code": -32031, "message": "ZCODE_RUNTIME_MODEL_UNAVAILABLE api_key=secret-value",
                    "data": {"provider": "must-not-record"}
                })))
            }
            fn diagnostic_tail(&self) -> String {
                "queued-send-tail".into()
            }
        }
        struct CompletedFactory;
        impl RuntimeFactory for CompletedFactory {
            fn spawn(
                &self,
                _: &TaskRecord,
                sink: Arc<dyn LifecycleSink>,
            ) -> io::Result<Arc<dyn ManagedRuntime>> {
                for (index, params) in [
                    serde_json::json!({"type":"model.streaming", "payload":{"kind":"text_delta", "delta":"completed answer", "assistantMessageId":"m1"}}),
                    serde_json::json!({"type":"message.finished", "payload":{"assistantMessageId":"m1"}}),
                ].into_iter().enumerate() {
                    sink.emit(LifecycleRecord {
                        sequence: index as u64 + 1,
                        event: RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(external_contract::EventEnvelope { method: "session/event".into(), params }))),
                    });
                }
                Ok(Arc::new(CompletedRuntime))
            }
        }
        let (_workspace, mut scheduler, agent_id) = diagnostic_scheduler("unused");
        Arc::get_mut(&mut scheduler.inner).unwrap().factory = Arc::new(CompletedFactory);
        scheduler
            .store()
            .insert_message("queued-after-completion", &agent_id, "queue", "follow-up")
            .unwrap();
        scheduler.start_ready().unwrap();
        let result = await_result(&scheduler, &agent_id);
        let record = failure_record(&scheduler, &agent_id);
        assert_eq!(record["stage"], "message_delivery");
        assert_eq!(record["error_code"], "SESSION_SEND_FAILED");
        assert_eq!(record["session_id"], "completed-session");
        assert_eq!(record["stderr_tail"], "queued-send-tail");
        assert_eq!(record["operation"], "session/send");
        assert_eq!(record["remote_code"], -32031);
        assert!(record["remote_message"]
            .as_str()
            .unwrap()
            .contains("ZCODE_RUNTIME_MODEL_UNAVAILABLE"));
        assert!(record.to_string().contains("secret-value"));
        assert!(!record.to_string().contains("must-not-record"));
        assert_eq!(result.result.outcome, TaskOutcome::Completed);
        assert_eq!(result.result.final_text, "completed answer");
        let message = scheduler
            .store()
            .message("queued-after-completion")
            .unwrap()
            .unwrap();
        assert_eq!(message.state, external_store::MessageState::Failed);
        assert_eq!(message.failure_code.as_deref(), Some("SESSION_SEND_FAILED"));
    }

    #[test]
    fn message_id_collision_rejects_other_content_or_agent_without_mutation() {
        let (_workspace, scheduler, agent_id) = diagnostic_scheduler("unused");
        scheduler
            .store()
            .insert_message("existing", &agent_id, "queue", "original")
            .unwrap();
        for (agent, content) in [
            (agent_id.as_str(), "changed"),
            ("different-agent", "original"),
        ] {
            let error = scheduler
                .queue_message(agent, "existing", "queue", content)
                .unwrap_err();
            assert!(
                matches!(error, SchedulerError::Store(StoreError::Conflict(ref message)) if message == "MESSAGE_ID_CONFLICT")
            );
        }
        let message = scheduler.store().message("existing").unwrap().unwrap();
        assert_eq!(message.agent_id, agent_id);
        assert_eq!(message.content, "original");
        assert_eq!(message.state, MessageState::Queued);
        assert_eq!(
            scheduler
                .queue_message(&agent_id, "existing", "queue", "original")
                .unwrap(),
            MessageDisposition::Queued
        );
    }

    #[test]
    fn remote_rejection_preserves_bounded_message_and_separates_cleanup() {
        let error = RuntimeCommandError::Remote(serde_json::json!({
            "code": -32031,
            "message": "unavailable Authorization: Bearer abc-secret password=def-secret api_key=\"quoted secret words\" https://user:pass@host/path",
            "data": {"api_key": "do-not-copy"}
        }));
        let mut detail: serde_json::Value =
            serde_json::from_str(&error.diagnostic("session/send")).unwrap();
        detail["cleanup_result"] = "Stopped(Terminated(Signaled(15)))".into();
        let record = runtime_failure_record(
            "a",
            Some("s"),
            "runtime_terminal",
            "SESSION_SEND_FAILED",
            &detail.to_string(),
            "stderr-end",
        );
        let value: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert_eq!(value["remote_code"], -32031);
        assert_eq!(value["operation"], "session/send");
        assert_eq!(value["cleanup_result"], "Stopped(Terminated(Signaled(15)))");
        let remote_message = value["remote_message"].as_str().unwrap();
        assert!(remote_message.contains("abc-secret"));
        assert!(remote_message.contains("def-secret"));
        assert!(remote_message.contains("quoted secret words"));
        assert!(remote_message.contains("user:pass"));
        assert!(!record.contains("do-not-copy"));
        assert!(bounded_prefix(&"界".repeat(5000), 1024).len() <= 1024);
    }

    #[test]
    fn known_driver_loss_marks_observation_coverage_without_query_side_effects() {
        let tracker = PassiveActivityTracker::new(true);
        tracker.observe(&RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(
            external_contract::EventEnvelope {
                method: "session/event".into(),
                params: serde_json::json!({
                    "type":"model.streaming",
                    "eventId":"reasoning-1",
                    "turnId":"turn-1",
                    "payload":{"kind":"reasoning_delta", "delta":"visible"}
                }),
            },
        ))));
        let before = tracker.observation_snapshot();
        assert!(before.coverage.tool_history_complete);
        assert!(before.coverage.reasoning_complete);

        tracker.observe(&RuntimeEvent::Driver(Inbound::Malformed(
            "invalid JSON".into(),
        )));
        tracker.observe(&RuntimeEvent::Driver(Inbound::OversizedLine {
            bytes: 1024 * 1024 + 1,
        }));
        let after = tracker.observation_snapshot();
        assert!(!after.coverage.tool_history_complete);
        assert!(!after.coverage.reasoning_complete);
        assert_eq!(after.coverage.dropped_events, 2);
        assert_eq!(after.snapshot_seq, before.snapshot_seq + 2);
        assert_eq!(tracker.observation_snapshot(), after);
        assert_eq!(tracker.observation_snapshot(), after);
    }

    #[test]
    fn near_limit_observation_does_not_starve_the_shared_cancel_lifecycle_lock() {
        let lifecycle = Arc::new(RuntimeLifecycle::new(1));
        let tracker = Arc::new(PassiveActivityTracker::new(true));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker_lifecycle = Arc::clone(&lifecycle);
        let worker_tracker = Arc::clone(&tracker);
        let worker_barrier = Arc::clone(&barrier);
        let worker = thread::spawn(move || {
            let _admission = worker_lifecycle.admit_event().unwrap();
            worker_barrier.wait();
            worker_tracker.observe(&RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(
                external_contract::EventEnvelope {
                    method: "session/event".into(),
                    params: serde_json::json!({
                        "type":"model.streaming",
                        "eventId":"large-tool-1",
                        "turnId":"turn-1",
                        "payload":{
                            "kind":"tool_call",
                            "toolCallId":"call-1",
                            "toolName":"Bash",
                            "input":{"command":"x".repeat(900 * 1024)}
                        }
                    }),
                },
            ))));
        });
        barrier.wait();
        let started = Instant::now();
        lifecycle.request_stop(&TurnSnapshot {
            generation: 1,
            active: true,
            boundary: None,
        });
        let elapsed = started.elapsed();
        worker.join().unwrap();
        assert!(
            elapsed < Duration::from_secs(5),
            "cancel lifecycle lock waited {elapsed:?}"
        );
        assert!(tracker.observation_snapshot().tools[0].recent_calls[0].arguments_truncated);
    }

    #[test]
    fn escaped_runtime_failure_record_is_bounded_and_keeps_latest_stderr() {
        let large = "\0".repeat(30000);
        let record = runtime_failure_record(
            &large,
            Some(&large),
            &large,
            &large,
            &large,
            &(large.clone() + "END"),
        );
        assert!(record.len() < 192 * 1024);
        let record: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert!(record["stderr_tail"].as_str().unwrap().ends_with("END"));
        assert!(record["stderr_tail"].as_str().unwrap().len() <= 16 * 1024);
        assert!(!record.to_string().contains('\n'));
    }

    #[test]
    fn bounded_error_respects_utf8_byte_limit() {
        let value = bounded_error(&"界".repeat(5000));
        assert!(value.len() <= 4096);
        assert!(value.ends_with('…'));
        assert!(std::str::from_utf8(value.as_bytes()).is_ok());
    }

    #[test]
    fn failure_log_queue_is_bounded_and_reports_dropped_writes_after_recovery() {
        struct BlockingWriter {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            output: mpsc::Sender<String>,
            blocked: bool,
        }
        impl Write for BlockingWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if !self.blocked {
                    self.blocked = true;
                    self.entered.send(()).unwrap();
                    self.release.recv().unwrap();
                }
                self.output
                    .send(String::from_utf8_lossy(buf).into_owned())
                    .unwrap();
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (output_tx, output_rx) = mpsc::channel();
        let logger = DiagnosticLogger::start(BlockingWriter {
            entered: entered_tx,
            release: release_rx,
            output: output_tx,
            blocked: false,
        })
        .unwrap();
        logger.submit("first\n".into());
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        for _ in 0..DIAGNOSTIC_QUEUE_CAPACITY + 100 {
            logger.submit("queued\n".into());
        }
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(logger.dropped.load(Ordering::Relaxed), 100);
        release_tx.send(()).unwrap();
        assert_eq!(
            output_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "first\n"
        );
        assert_eq!(
            output_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "[zcode-agentd] diagnostic_writes_dropped=100\n"
        );
        for _ in 0..DIAGNOSTIC_QUEUE_CAPACITY {
            output_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
    }

    #[test]
    fn failure_log_write_errors_are_ignored_and_reported_on_next_success() {
        struct FailingOnce {
            output: mpsc::Sender<String>,
            failed: bool,
        }
        impl Write for FailingOnce {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if !self.failed {
                    self.failed = true;
                    return Err(io::Error::other("synthetic log failure"));
                }
                self.output
                    .send(String::from_utf8_lossy(buf).into_owned())
                    .unwrap();
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (output_tx, output_rx) = mpsc::channel();
        let logger = DiagnosticLogger::start(FailingOnce {
            output: output_tx,
            failed: false,
        })
        .unwrap();
        logger.submit("fails\n".into());
        logger.submit("succeeds\n".into());
        assert_eq!(
            output_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "[zcode-agentd] diagnostic_writes_dropped=1\n"
        );
        assert_eq!(
            output_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "succeeds\n"
        );
    }

    #[test]
    fn failure_log_rotates_with_finite_files_and_preserves_open_stderr_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon-error.log");
        let mut stderr = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .unwrap();
        let mut writer = RotatingDiagnosticWriter { path: path.clone() };
        let record = vec![b'x'; DIAGNOSTIC_RECORD_BYTES];
        for _ in 0..100 {
            writer.write_all(&record).unwrap();
        }
        stderr.write_all(b"launchd-stderr-still-current\n").unwrap();
        assert!(fs::read_to_string(&path)
            .unwrap()
            .ends_with("launchd-stderr-still-current\n"));
        let entries = fs::read_dir(directory.path())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 3);
        for entry in entries {
            assert!(entry.metadata().unwrap().len() <= DIAGNOSTIC_FILE_BYTES);
        }
        // Pre-existing oversized logs are trimmed to the same retention cap.
        stderr.set_len(DIAGNOSTIC_FILE_BYTES * 3).unwrap();
        writer.write_all(b"recovered\n").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 10);
        assert_eq!(
            fs::metadata(directory.path().join("daemon-error.log.1"))
                .unwrap()
                .len(),
            DIAGNOSTIC_FILE_BYTES
        );
    }

    #[test]
    fn latest_failure_replaces_previous_failure() {
        let mut failures = HashMap::new();
        update_latest_failure(&mut failures, "agent", "first".into());
        update_latest_failure(&mut failures, "agent", "latest".into());
        assert_eq!(failures.get("agent").map(String::as_str), Some("latest"));
    }
}

#[cfg(unix)]
mod daemon;
#[cfg(unix)]
pub use daemon::Daemon;
