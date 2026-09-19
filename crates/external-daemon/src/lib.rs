use external_contract::{
    event_type, normalized_zai_model, offered_permission_response, turn_id_from_result,
    CreateSessionParams, ResumeSessionParams, RuntimePreferences, SendParams,
    SessionCreateProjection, SessionParams, StdioMcpServer, SubscribeParams, WireId, WireMessage,
    WorkspaceRef, INTERACTION_REQUEST_PERMISSION, INTERACTION_REQUEST_UNSUPPORTED_INPUT,
    INTERACTION_REQUEST_USER_INPUT, SESSION_CREATE, SESSION_REQUEST_RUNTIME_PREFERENCES,
    SESSION_RESUME, SESSION_SEND, SESSION_STOP, SESSION_SUBSCRIBE,
};
use external_runtime::{
    observe_process, observe_process_group, stop_and_reap_persisted_process_group, ChildExit,
    Driver, Inbound, ProcessIdentity, RequestError, StopOutcome,
};
use external_store::{
    MessageState, NewTask, PendingRequestState, PendingResponseClaimDisposition, Store,
    StoredMessage, StoredProcessIdentity, TaskClaim, TaskOutcome, TaskPhase, TaskRecord, TurnState,
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

mod activity_parser;
mod activity_tracker;
use activity_parser::{
    parse_passive_activity, ActivitySample, ActivitySampleKind, ActivitySource, ActivityTransition,
};
pub mod agent_status;
pub mod codex;
pub mod dsh;
mod lifecycle_sink;
pub mod mcp;
pub mod observation;
mod projection;
use activity_tracker::{PassiveActivityTracker, TerminalText};
use lifecycle_sink::{
    bounded_result_invalid_task_result, finalized_general, minimal_task_result,
    persist_general_result, store_result_with_cancel_precedence, unreaped_general,
    NaturalCompletionAdmission, StoreLifecycleSink, UnstartedTerminal,
};
pub mod rpc;
mod runtime_owner;
mod scheduler;
use external_core::{
    CompletionOutcome, GeneralCompletion, GeneralFinalizer, GeneralTaskManifest,
    GeneralTaskPreparer, PolicyLauncher, PreparedGeneralTask, ValidatedPermissionDenial,
};
use runtime_owner::general_initial_prompt;
pub use runtime_owner::{CommandRuntimeFactory, ManagedRuntime, RuntimeFactory};
pub use scheduler::configure_diagnostic_log;
use scheduler::{bounded_error, bounded_prefix, RuntimeLifecycle, RuntimeLifecyclePhase};
pub use scheduler::{
    MessageDisposition, ResponseDisposition, ResponseOutcome, Scheduler, SchedulerConfig,
    SchedulerError,
};

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

pub(crate) fn terminal_proves_process_group_reaped(terminal: &RuntimeTerminal) -> bool {
    matches!(
        terminal,
        RuntimeTerminal::Stopped(_)
            | RuntimeTerminal::Completed(_)
            | RuntimeTerminal::FailedTurn(_)
    )
}

pub(crate) fn task_agent(task: &TaskRecord) -> String {
    match task_route(task) {
        Ok(TaskRoute::General(prepared)) => prepared
            .admission
            .as_ref()
            .map(|identity| identity.agent.clone())
            .unwrap_or_else(|| "zcode".into()),
        Err(_) => "zcode".into(),
    }
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
    pub text_delta_events: u64,
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
    /// Verified-public reasoning tail (bounded Unicode chars); empty when the
    /// runtime source is not verified.
    pub latest_reasoning: String,
    pub active_tools: Vec<PassiveActiveTool>,
    pub(crate) oldest_active_tool_age_ms: Option<u64>,
    pub window_60s: PassiveActivityWindow,
    pub telemetry_degraded: bool,
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

pub(crate) struct TurnTracker {
    state: Mutex<TurnTrackerState>,
    changed: Condvar,
}
impl TurnTracker {
    pub(crate) fn new() -> Self {
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

    pub(crate) fn observe(&self, inbound: &Inbound) {
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

    pub(crate) fn snapshot(&self) -> TurnSnapshot {
        let state = self.state.lock().unwrap();
        TurnSnapshot {
            generation: state.generation,
            active: state.active,
            boundary: state.boundary,
        }
    }

    pub(crate) fn activity_snapshot(&self) -> RuntimeActivitySnapshot {
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

    pub(crate) fn wait_boundary_after(
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

pub(crate) struct Publisher {
    sink: Arc<dyn LifecycleSink>,
    state: Mutex<PublisherState>,
    changed: Condvar,
}

impl Publisher {
    pub(crate) fn new(sink: Arc<dyn LifecycleSink>) -> Self {
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

    pub(crate) fn emit_driver(&self, event: Inbound, exit_terminal: Option<RuntimeTerminal>) {
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

    pub(crate) fn begin_stopping(&self) -> Option<RuntimeTerminal> {
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

    pub(crate) fn publish_terminal(&self, terminal: RuntimeTerminal) -> RuntimeTerminal {
        let mut state = self.state.lock().unwrap();
        if let OwnerState::Terminal(existing) = &state.owner {
            return existing.clone();
        }
        self.publish_terminal_locked(&mut state, terminal.clone());
        terminal
    }

    pub(crate) fn wait_for_exit_boundary(&self, timeout: Duration) -> Option<RuntimeTerminal> {
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

    pub(crate) fn wait_terminal(&self, timeout: Duration) -> Option<RuntimeTerminal> {
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
        // Answerable user-input requests carry the answer as the plain
        // JSON-RPC result; the driver already accepts arbitrary JSON results.
        if decision == "answer" {
            let answer = content
                .filter(|value| !value.trim().is_empty())
                .ok_or(RuntimeCommandError::Unsupported)?;
            return self
                .driver
                .respond_before(id, serde_json::Value::String(answer.to_owned()), deadline)
                .map_err(RuntimeCommandError::from);
        }
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

#[cfg(unix)]
mod daemon;
#[cfg(unix)]
pub use daemon::Daemon;

#[cfg(test)]
mod task_route_tests {
    use super::*;
    use external_store::{TaskPhase, TaskRecord, TurnState};

    fn record(prepared: &PreparedGeneralTask) -> TaskRecord {
        TaskRecord {
            agent_id: prepared.agent_id.clone(),
            repository: prepared.repository.to_string_lossy().into_owned(),
            phase: TaskPhase::Queued,
            outcome: None,
            workspace_path: prepared.workspace.path.to_string_lossy().into_owned(),
            runtime_hash: None,
            prepared_launch_json: serde_json::to_string(prepared).unwrap(),
            prepared_launch_sha256: prepared.prepared_sha256.clone(),
            initial_prompt: String::new(),
            owner_id: None,
            owner_epoch: 0,
            close_requested: false,
            stop_requested: false,
            last_event_seq: 0,
            failure_code: None,
            failure_message: None,
            runtime_agent_id: None,
            zcode_session_id: None,
            turn_state: TurnState::Idle,
            process_identity: None,
            closed_at: None,
            reaped_at: None,
            created_at: 0,
        }
    }

    #[test]
    fn legacy_prepared_launch_without_effort_still_routes_with_none() {
        let repository = tempfile::tempdir().unwrap();
        let manifest = external_core::GeneralTaskManifest {
            schema: external_core::GENERAL_TASK_SCHEMA.into(),
            agent_id: "s01-legacy-row".into(),
            repository: repository.path().to_path_buf(),
            permission_mode: external_core::PermissionMode::Plan,
            prompt: "legacy row".into(),
            write_manifest: Vec::new(),
        };
        // Build the persisted row exactly as a pre-effort daemon wrote it:
        // admission has no effort key at all, and the digest covers those
        // bytes. A later re-serialization must not insert a synthetic
        // `"effort": null` or the digest breaks on resume.
        let prepared = external_core::GeneralTaskPreparer::new(Vec::new())
            .unwrap()
            .prepare_direct_submission(&manifest)
            .unwrap()
            .with_admission(external_core::AdmissionIdentity {
                agent: "zcode".into(),
                config_revision: 1,
                adapter_version: "legacy".into(),
                model: None,
                model_source: "native".into(),
                effort: None,
            })
            .unwrap();
        let legacy_json = serde_json::to_string(&prepared).unwrap();
        let encoded: serde_json::Value = serde_json::from_str(&legacy_json).unwrap();
        assert!(
            encoded["admission"].get("effort").is_none(),
            "legacy row must not carry an effort key: {legacy_json}"
        );
        let record = record(&prepared);
        assert_eq!(record.prepared_launch_json, legacy_json);
        let TaskRoute::General(routed) = task_route(&record).expect("legacy row must still route")
        else {
            panic!("expected the general route");
        };
        routed.validate_digest().expect("legacy digest stays valid");
        assert_eq!(routed.admission.as_ref().unwrap().effort, None);
    }
}
