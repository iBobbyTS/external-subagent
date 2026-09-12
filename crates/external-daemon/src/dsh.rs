//! DSH ACP runtime owner, gated factory, and agent routing composition.
//!
//! The provider protocol (methods, frames, offers, settlement folding) lives
//! in `external-agent-dsh`; this module composes it into the shared daemon
//! lifecycle: one child process per task, a continuous stdio pump that
//! normalizes ACP traffic into the canonical internal event envelope, prompt
//! settlement watchers that drive turn boundaries, and single-shot public
//! permission responses that only echo offered options.
//!
//! Production DSH spawn stays behind a closed gate until the S04 parent is
//! accepted; the closed factory refuses to spawn even if routing changes.

use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use external_agent_dsh::acp::{
    permission::{self, OfferCache},
    result::{fold_settlement, MessageAggregation, SettlementOutcome},
    session::{AcpSession, SessionError},
    transport, update,
};
use external_contract::{
    classify_lifecycle, EventEnvelope, RequestEnvelope, WireId, WireMessage,
    INTERACTION_REQUEST_PERMISSION, INTERACTION_REQUEST_USER_INPUT, SESSION_EVENT,
};
use external_runtime::{
    observe_process_group, ChildExit, Driver, FrameCodec, Inbound, StopOutcome,
};
use external_store::TaskRecord;

use crate::{
    task_agent, task_route, CommandRuntimeFactory, LifecycleSink, ManagedRuntime, ProcessIdentity,
    Publisher, RuntimeCommandError, RuntimeFactory, RuntimeLoss, RuntimeTerminal, SessionReady,
    TurnBoundary, TurnSnapshot, TurnTracker,
};

/// Whether the DSH factory may spawn adapter processes at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DshSpawnGate {
    /// S04.A production state: the adapter exists but spawn is refused.
    Closed,
    /// Controlled test harness only; never constructed by the production
    /// composition root.
    #[cfg(test)]
    TestHarness,
}

/// Factory for DSH ACP runtimes. Closed by default: production routing can
/// register the factory without enabling DSH spawn support.
pub struct DshRuntimeFactory {
    gate: DshSpawnGate,
    #[cfg(test)]
    executable: Option<PathBuf>,
}

impl DshRuntimeFactory {
    pub fn closed() -> Self {
        Self {
            gate: DshSpawnGate::Closed,
            #[cfg(test)]
            executable: None,
        }
    }

    #[cfg(test)]
    pub fn test_harness(executable: Option<PathBuf>) -> Self {
        Self {
            gate: DshSpawnGate::TestHarness,
            executable,
        }
    }
}

impl RuntimeFactory for DshRuntimeFactory {
    fn spawn(
        &self,
        _task: &TaskRecord,
        _sink: Arc<dyn LifecycleSink>,
    ) -> io::Result<Arc<dyn ManagedRuntime>> {
        match self.gate {
            DshSpawnGate::Closed => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "dsh spawn gate is closed; production DSH spawn is not enabled",
            )),
            #[cfg(test)]
            DshSpawnGate::TestHarness => {
                let prepared = match task_route(_task) {
                    Ok(crate::TaskRoute::General(prepared)) => prepared,
                    Err(message) => {
                        return Err(io::Error::new(io::ErrorKind::InvalidInput, message))
                    }
                };
                let executable = self
                    .executable
                    .clone()
                    .or_else(|| std::env::var_os("DSH_RUNTIME_PATH").map(PathBuf::from));
                let launch = external_agent_dsh::profile::DshLaunch::new(
                    executable,
                    prepared.workspace.path.clone(),
                    std::env::var_os("DSH_HOME").map(PathBuf::from),
                );
                let command = external_agent_dsh::profile::resolve_launch(&launch)?;
                let owner = DshRuntimeOwner::spawn(command, _sink)?;
                Ok(Arc::new(owner))
            }
        }
    }
}

/// Route each prepared task to its agent factory. Legacy tasks without an
/// admission identity keep the ZCode route; DSH tasks route to the (closed)
/// DSH factory so opening the gate later is a single composition change.
pub struct RoutingRuntimeFactory<F> {
    zcode: CommandRuntimeFactory<F>,
    dsh: DshRuntimeFactory,
}

impl<F> RoutingRuntimeFactory<F> {
    pub fn new(zcode: CommandRuntimeFactory<F>, dsh: DshRuntimeFactory) -> Self {
        Self { zcode, dsh }
    }
}

impl<F> RuntimeFactory for RoutingRuntimeFactory<F>
where
    F: Fn(&TaskRecord) -> io::Result<Command> + Send + Sync + 'static,
{
    fn spawn(
        &self,
        task: &TaskRecord,
        sink: Arc<dyn LifecycleSink>,
    ) -> io::Result<Arc<dyn ManagedRuntime>> {
        match task_agent(task).as_str() {
            "zcode" => self.zcode.spawn(task, sink),
            "dsh" => self.dsh.spawn(task, sink),
            agent => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("task routes to unknown agent {agent:?}"),
            )),
        }
    }
}

struct DshRuntimeShared {
    publisher: Arc<Publisher>,
    turn_tracker: Arc<TurnTracker>,
    offers: Mutex<OfferCache>,
    tool_names: Mutex<HashMap<String, String>>,
    messages: Mutex<MessageAggregation>,
    session_id: Mutex<Option<String>>,
    current_prompt: Mutex<Option<String>>,
    sequence: AtomicU64,
    stop_boundaries: AtomicU64,
}

impl DshRuntimeShared {
    fn next_event_id(&self) -> String {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        format!("dsh-event-{sequence}")
    }

    fn canonical_event(&self, params: serde_json::Value) -> Inbound {
        Inbound::Message(WireMessage::Event(EventEnvelope {
            method: SESSION_EVENT.into(),
            params,
        }))
    }

    fn emit_canonical(&self, params: serde_json::Value) {
        self.publisher
            .emit_driver(self.canonical_event(params), None);
    }

    fn emit_lifecycle(&self, method: &str) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        self.publisher.emit_driver(
            Inbound::Lifecycle {
                sequence,
                method: method.into(),
                order: classify_lifecycle(method, false),
            },
            None,
        );
    }

    fn begin_turn(&self, prompt_id: u64) {
        *self.current_prompt.lock().unwrap() = Some(format!("dsh-prompt-{prompt_id}"));
        let params = serde_json::json!({"type": "turn.started"});
        let started = self.canonical_event(params.clone());
        // The canonical start event opens the shared tracker's per-turn
        // terminal text before any of the turn's streaming can arrive.
        self.emit_canonical(params);
        self.turn_tracker.observe(&started);
        self.emit_lifecycle("turn.started");
    }

    /// Apply one prompt settlement as the single turn-boundary authority.
    fn apply_boundary(&self, kind: TurnBoundary, final_text: Option<&str>, reason: Option<&str>) {
        let turn_id = self.current_prompt.lock().unwrap().take();
        let event_id = self.next_event_id();
        let mut payload = serde_json::json!({});
        if let Some(text) = final_text {
            payload["response"] = serde_json::Value::String(text.to_owned());
        }
        if let Some(reason) = reason {
            payload["reason_code"] = serde_json::Value::String(reason.to_owned());
        }
        let event_type = match kind {
            TurnBoundary::Completed => "turn.completed",
            TurnBoundary::Failed => "turn.failed",
        };
        let params = serde_json::json!({
            "type": event_type,
            "eventId": event_id,
            "turnId": turn_id,
            "payload": payload,
        });
        let boundary_event = self.canonical_event(params.clone());
        // The sink must observe the settlement's authoritative payload before
        // the tracker exposes the boundary: the scheduler reacts to the
        // tracker (delivering the next prompt or terminalizing the task), and
        // events admitted after terminalization are dropped, which would
        // strand the turn's final text.
        self.emit_canonical(params);
        self.turn_tracker.observe(&boundary_event);
        self.emit_lifecycle(event_type);
    }

    fn apply_settlement(&self, settlement: &transport::PromptSettlement) {
        let outcome = {
            let messages = self.messages.lock().unwrap();
            fold_settlement(settlement, &messages)
        };
        match outcome {
            SettlementOutcome::Completed { final_text } => {
                self.apply_boundary(TurnBoundary::Completed, final_text.as_deref(), None);
            }
            SettlementOutcome::Failed { reason_code } => {
                self.apply_boundary(TurnBoundary::Failed, None, Some(&reason_code));
            }
        }
    }

    fn apply_failed_settlement(&self, message: &str) {
        let bounded: String = message.chars().take(256).collect();
        self.apply_boundary(TurnBoundary::Failed, None, Some(&bounded));
    }
}

pub struct DshRuntimeOwner {
    driver: Arc<Driver>,
    shared: Arc<DshRuntimeShared>,
    session: Mutex<AcpSession>,
    shutdown: Arc<AtomicBool>,
}

const MAX_TRACKED_TOOLS: usize = 128;

impl DshRuntimeOwner {
    pub fn spawn(command: Command, sink: Arc<dyn LifecycleSink>) -> io::Result<Self> {
        let driver = Arc::new(Driver::spawn_with_codec(command, FrameCodec::JsonRpc2)?);
        AcpSession::codec_check(&driver).map_err(|error| io::Error::other(error.to_string()))?;
        let publisher = Arc::new(Publisher::new(sink));
        let shared = Arc::new(DshRuntimeShared {
            publisher: Arc::clone(&publisher),
            turn_tracker: Arc::new(TurnTracker::new()),
            offers: Mutex::new(OfferCache::default()),
            tool_names: Mutex::new(HashMap::new()),
            messages: Mutex::new(MessageAggregation::default()),
            session_id: Mutex::new(None),
            current_prompt: Mutex::new(None),
            sequence: AtomicU64::new(0),
            stop_boundaries: AtomicU64::new(0),
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        spawn_dsh_pump(
            Arc::clone(&driver),
            Arc::clone(&publisher),
            Arc::clone(&shared),
            Arc::clone(&shutdown),
        );
        let session = AcpSession::new(Arc::clone(&driver));
        Ok(Self {
            driver,
            shared,
            session: Mutex::new(session),
            shutdown,
        })
    }

    /// Begin one prompt turn and register its settlement watcher. Returns
    /// once the request is on the wire; the agent turn runs to settlement on
    /// the watcher thread so control-plane operations never block on it.
    fn send_prompt(&self, prompt: &str) -> Result<(), RuntimeCommandError> {
        let snapshot = self.shared.turn_tracker.snapshot();
        if snapshot.active {
            return Err(RuntimeCommandError::InvalidSession(
                "a prompt is already in flight for this dsh session".into(),
            ));
        }
        let (prompt_id, pending) = {
            let mut session = self.session.lock().unwrap();
            session
                .prompt(prompt)
                .map_err(|error| RuntimeCommandError::InvalidSession(dsh_session_message(&error)))
        }?;
        self.shared.begin_turn(prompt_id);
        let shared = Arc::clone(&self.shared);
        thread::Builder::new()
            .name("dsh-settlement".into())
            .spawn(move || {
                let settlement = match pending.wait(Duration::from_secs(24 * 60 * 60)) {
                    Ok(response) => match response.result.as_ref() {
                        Some(result) => match transport::parse_prompt_settlement(result) {
                            Ok(settlement) => Ok(settlement),
                            Err(error) => Err(error.to_string()),
                        },
                        None => Err("session/prompt settled without a result".into()),
                    },
                    Err(error) => Err(error.to_string()),
                };
                match settlement {
                    Ok(settlement) => shared.apply_settlement(&settlement),
                    Err(message) => shared.apply_failed_settlement(&message),
                }
            })
            .map_err(|error| RuntimeCommandError::Transport(error.to_string()))?;
        Ok(())
    }

    fn bootstrap(
        &self,
        task: &TaskRecord,
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RuntimeCommandError::Timeout)?;
        let model = match task_route(task) {
            Ok(crate::TaskRoute::General(prepared)) => prepared
                .admission
                .as_ref()
                .and_then(|identity| identity.model.clone()),
            Err(message) => {
                return Err(RuntimeCommandError::InvalidSession(message));
            }
        };
        let mut session = self.session.lock().unwrap();
        let remaining = || remaining_time(deadline);
        let capabilities = session
            .initialize(remaining()?)
            .map_err(|error| RuntimeCommandError::InvalidSession(dsh_session_message(&error)))?;
        transport::require_build_capabilities(&capabilities)
            .map_err(|error| RuntimeCommandError::InvalidSession(error.to_string()))?;
        let cwd = PathBuf::from(&task.workspace_path);
        session
            .new_session(&cwd, remaining()?)
            .map_err(|error| RuntimeCommandError::InvalidSession(dsh_session_message(&error)))?;
        let session_id = session
            .session_id()
            .expect("session id is set after new_session")
            .to_owned();
        *self.shared.session_id.lock().unwrap() = Some(session_id.clone());
        let configured_model = if let Some(token) = model.as_deref() {
            session.set_model(token, remaining()?).map_err(|error| {
                RuntimeCommandError::InvalidSession(dsh_session_message(&error))
            })?;
            Some(token.to_owned())
        } else {
            None
        };
        // The initial prompt only leaves after initialize, session/new, and
        // any model selection have all been verified (X05/P02).
        let prompt = task.initial_prompt.clone();
        drop(session);
        self.send_prompt(&prompt)?;
        Ok(SessionReady {
            session_id,
            initial_turn_id: None,
            configured_model,
        })
    }

    fn finish_process(&self, grace: Duration, boundary: Option<TurnBoundary>) -> RuntimeTerminal {
        if let Some(terminal) = self.shared.publisher.begin_stopping() {
            return terminal;
        }
        let terminal = match self.driver.stop_and_reap(grace) {
            Ok(outcome) => match self.shared.publisher.wait_for_exit_boundary(grace) {
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
        self.shared.offers.lock().unwrap().clear();
        self.shared.publisher.publish_terminal(terminal)
    }

    fn validate_session(&self, session_id: &str) -> Result<(), RuntimeCommandError> {
        if self.shared.session_id.lock().unwrap().as_deref() == Some(session_id) {
            Ok(())
        } else {
            Err(RuntimeCommandError::InvalidSession(
                "session id does not belong to this runtime".into(),
            ))
        }
    }
}

fn dsh_session_message(error: &SessionError) -> String {
    let text = error.to_string();
    text.chars().take(512).collect()
}

fn remaining_time(deadline: Instant) -> Result<Duration, RuntimeCommandError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(RuntimeCommandError::Timeout)
}

fn spawn_dsh_pump(
    driver: Arc<Driver>,
    publisher: Arc<Publisher>,
    shared: Arc<DshRuntimeShared>,
    shutdown: Arc<AtomicBool>,
) {
    thread::spawn(move || loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        match driver.recv_timeout(Duration::from_millis(20)) {
            Ok(event) => {
                if let Some(projected) = project_dsh_inbound(&shared, &event) {
                    publisher.emit_driver(projected, None);
                }
                if let Inbound::ChildExited(exit) = &event {
                    driver.wait_diagnostics(Duration::from_secs(1));
                    let terminal = classify_child_exit(driver.identity().pgid, &shared, exit);
                    publisher.emit_driver(event, terminal);
                    shared.offers.lock().unwrap().clear();
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                shared.offers.lock().unwrap().clear();
                publisher.publish_terminal(RuntimeTerminal::FailedRuntimeLost(
                    RuntimeLoss::EventStreamLost,
                ));
                return;
            }
        }
    });
}

fn classify_child_exit(
    pgid: i32,
    shared: &DshRuntimeShared,
    exit: &ChildExit,
) -> Option<RuntimeTerminal> {
    // Completed is only provable when the whole process group is already
    // reaped; any surviving member keeps the terminal orphaned instead.
    match observe_process_group(pgid) {
        Ok(members) if members.is_empty() => match exit {
            ChildExit::Exited(Some(0)) => {
                let turn = shared.turn_tracker.snapshot();
                if !turn.active && turn.boundary == Some(TurnBoundary::Completed) {
                    Some(RuntimeTerminal::Completed(StopOutcome::AlreadyExited(
                        exit.clone(),
                    )))
                } else {
                    Some(RuntimeTerminal::FailedRuntimeLost(
                        RuntimeLoss::EventStreamLost,
                    ))
                }
            }
            _ => Some(RuntimeTerminal::Exited(exit.clone())),
        },
        Ok(_) | Err(_) => Some(RuntimeTerminal::Orphaned(RuntimeLoss::UnknownMembership)),
    }
}

/// Normalize one inbound ACP frame into the canonical internal envelope.
/// Returns `None` when the frame itself is the projection (pass-through).
fn project_dsh_inbound(shared: &DshRuntimeShared, event: &Inbound) -> Option<Inbound> {
    let Inbound::Message(message) = event else {
        return Some(event.clone());
    };
    match message {
        WireMessage::UnknownEvent { method, raw } if method == transport::SESSION_UPDATE => {
            let params = raw
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let Some(parsed) = update::parse_update(&params) else {
                return Some(event.clone());
            };
            let expected_session = shared.session_id.lock().unwrap().clone();
            if let (Some(expected), Some(actual)) =
                (expected_session.as_deref(), parsed.session_id.as_deref())
            {
                if expected != actual {
                    // Updates for foreign sessions are never projected.
                    return None;
                }
            }
            if let update::UpdateKind::ToolCall { tool_call_id, .. } = &parsed.kind {
                let name = update::tool_name(&parsed.kind).unwrap_or_else(|| "dsh_tool".into());
                let mut tools = shared.tool_names.lock().unwrap();
                if tools.len() < MAX_TRACKED_TOOLS || tools.contains_key(tool_call_id) {
                    tools.insert(tool_call_id.clone(), name);
                }
            }
            if let update::UpdateKind::AgentMessage {
                message_id: Some(message_id),
                text,
                committed: true,
            } = &parsed.kind
            {
                shared
                    .messages
                    .lock()
                    .unwrap()
                    .observe_committed(message_id, text);
            }
            let event_id = shared.next_event_id();
            let turn_id = shared
                .current_prompt
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "dsh-session".into());
            for payload in update::canonical_event_payloads(&parsed, &event_id, &turn_id) {
                // Streaming events refresh turn-liveness exactly like inbound
                // frames do on the ZCode path.
                let event = shared.canonical_event(payload);
                shared.turn_tracker.observe(&event);
                shared.publisher.emit_driver(event, None);
            }
            None
        }
        WireMessage::Request(request) => {
            if request.method == transport::SESSION_REQUEST_PERMISSION {
                match permission::parse_offer(&request.params) {
                    Some(offer) => {
                        let correlated = offer
                            .tool_call_id
                            .as_deref()
                            .and_then(|id| shared.tool_names.lock().unwrap().get(id).cloned());
                        let normalized =
                            permission::normalize_permission_request(&offer, correlated.as_deref());
                        shared
                            .offers
                            .lock()
                            .unwrap()
                            .observe(permission::correlation_key(&request.id), offer);
                        Some(Inbound::Message(WireMessage::Request(
                            RequestEnvelope::new(
                                request.id.clone(),
                                INTERACTION_REQUEST_PERMISSION,
                                normalized,
                            ),
                        )))
                    }
                    None => Some(unsupported_input_request(
                        request.id.clone(),
                        "dsh permission request carried no usable offer",
                    )),
                }
            } else {
                Some(unsupported_input_request(
                    request.id.clone(),
                    "dsh server request is not a supported interaction",
                ))
            }
        }
        _ => Some(event.clone()),
    }
}

fn unsupported_input_request(id: WireId, reason: &str) -> Inbound {
    Inbound::Message(WireMessage::Request(RequestEnvelope::new(
        id,
        INTERACTION_REQUEST_USER_INPUT,
        serde_json::json!({"origin": "dsh_acp", "reason": reason}),
    )))
}

impl Drop for DshRuntimeOwner {
    fn drop(&mut self) {
        let _ = self.stop(Duration::from_secs(1));
        self.shutdown.store(true, Ordering::Release);
    }
}

impl ManagedRuntime for DshRuntimeOwner {
    fn identity(&self) -> Option<ProcessIdentity> {
        Some(self.driver.identity())
    }

    fn stop(&self, grace: Duration) -> RuntimeTerminal {
        self.finish_process(grace, None)
    }

    fn wait_terminal(&self, timeout: Duration) -> Option<RuntimeTerminal> {
        self.shared.publisher.wait_terminal(timeout)
    }

    fn diagnostic_tail(&self) -> String {
        self.driver.diagnostic_tail()
    }

    fn diagnostic_session_id(&self) -> Option<String> {
        self.shared.session_id.lock().unwrap().clone()
    }

    fn wait_diagnostics(&self, timeout: Duration) {
        self.driver.wait_diagnostics(timeout);
    }

    fn bootstrap_session_with_mcp(
        &self,
        task: &TaskRecord,
        _mcp_servers: &[external_contract::StdioMcpServer],
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        self.bootstrap(task, timeout)
    }

    fn send_turn(
        &self,
        session_id: &str,
        content: &str,
        _timeout: Duration,
    ) -> Result<Option<String>, RuntimeCommandError> {
        self.validate_session(session_id)?;
        self.send_prompt(content)?;
        Ok(None)
    }

    fn stop_turn(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> Result<TurnSnapshot, RuntimeCommandError> {
        self.validate_session(session_id)?;
        let current = self.shared.turn_tracker.snapshot();
        if !current.active {
            return Ok(current);
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RuntimeCommandError::Timeout)?;
        {
            let session = self.session.lock().unwrap();
            session.cancel().map_err(|error| {
                RuntimeCommandError::InvalidSession(dsh_session_message(&error))
            })?;
        }
        let boundary = self
            .shared
            .turn_tracker
            .wait_boundary_after(current.generation, remaining_time(deadline)?)?;
        self.shared.stop_boundaries.fetch_add(1, Ordering::AcqRel);
        Ok(boundary)
    }

    fn respond_request(
        &self,
        correlation_id: &str,
        decision: &str,
        _content: Option<&str>,
        _validated_denial: Option<&external_core::ValidatedPermissionDenial>,
        deadline: Instant,
    ) -> Result<(), RuntimeCommandError> {
        let id = serde_json::from_str::<WireId>(correlation_id).map_err(|_| {
            RuntimeCommandError::InvalidSession("stored request correlation is invalid".into())
        })?;
        if !matches!(decision, "allow" | "deny") {
            return Err(RuntimeCommandError::Unsupported);
        }
        let outcome = {
            let mut offers = self.shared.offers.lock().unwrap();
            let offer = offers
                .take(&permission::correlation_key(&id))
                .ok_or_else(|| {
                    RuntimeCommandError::InvalidSession(
                        "runtime offered no matching permission response".into(),
                    )
                })?;
            offer.select(decision).ok_or_else(|| {
                RuntimeCommandError::InvalidSession(
                    "the offered options do not include that decision".into(),
                )
            })?
        };
        self.driver
            .respond_before(id, outcome, deadline)
            .map_err(RuntimeCommandError::from)
    }

    fn close_session(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> Result<(), RuntimeCommandError> {
        self.validate_session(session_id)?;
        let session = self.session.lock().unwrap();
        session
            .close(timeout)
            .map_err(|error| RuntimeCommandError::InvalidSession(dsh_session_message(&error)))
    }

    fn turn_snapshot(&self) -> TurnSnapshot {
        self.shared.turn_tracker.snapshot()
    }

    fn activity_snapshot(&self) -> crate::RuntimeActivitySnapshot {
        self.shared.turn_tracker.activity_snapshot()
    }

    fn stop_boundary_count(&self) -> u64 {
        self.shared.stop_boundaries.load(Ordering::Acquire)
    }

    fn finish_turn(&self, boundary: TurnBoundary, grace: Duration) -> RuntimeTerminal {
        self.finish_process(grace, Some(boundary))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        terminal_proves_process_group_reaped, MessageDisposition, ResponseDisposition, Scheduler,
        SchedulerConfig, SchedulerError,
    };
    use external_core::{
        AdmissionIdentity, GeneralTaskManifest, PermissionMode, GENERAL_TASK_SCHEMA,
    };
    use external_store::{MessageState, PendingRequestState, TaskOutcome, TaskPhase};
    use std::io::Write;

    const SESSION_ID: &str = "dsh-build-session";

    fn dsh_admission(model: Option<&str>) -> AdmissionIdentity {
        AdmissionIdentity {
            agent: "dsh".into(),
            config_revision: 1,
            adapter_version: env!("CARGO_PKG_VERSION").into(),
            model: model.map(str::to_owned),
            model_source: "catalog".into(),
        }
    }

    /// Serialize the scripted-children tests so at most one extra provider
    /// process runs at a time inside the parallel daemon suite; the older
    /// hi-probe fixtures are timing-marginal under concurrent process load.
    static SCRIPTED_CHILD_LOCK: Mutex<()> = Mutex::new(());

    fn scripted_test_guard() -> std::sync::MutexGuard<'static, ()> {
        SCRIPTED_CHILD_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Write an executable scripted ACP child whose every inbound frame is
    /// appended (one JSON per line) to `<workspace>/wire.jsonl`.
    fn scripted_child(workspace: &std::path::Path, script: &str) -> std::path::PathBuf {
        let path = workspace.join("acp-child.sh");
        let mut file = std::fs::File::create(&path).unwrap();
        write!(
            file,
            "#!/bin/sh\nLOG_PATH={:?}\nlog() {{ printf '%s\\n' \"$1\" >> \"$LOG_PATH\"; }}\nread_frame() {{ IFS= read -r line; log \"$line\"; }}\n{script}\nwhile IFS= read -r line; do log \"$line\"; done\n",
            workspace.join("wire.jsonl").to_string_lossy()
        )
        .unwrap();
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn wire_frames(workspace: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(workspace.join("wire.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn wait_for_frames(workspace: &std::path::Path, expected: usize) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let frames = wire_frames(workspace);
            if frames.len() >= expected {
                return frames;
            }
            assert!(
                Instant::now() < deadline,
                "scripted child never observed {expected} frames"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn request_methods(frames: &[serde_json::Value]) -> Vec<&str> {
        frames
            .iter()
            .filter_map(|frame| frame.get("method").and_then(|value| value.as_str()))
            .collect()
    }

    fn dsh_scheduler(workspace: &std::path::Path, dsh: DshRuntimeFactory) -> Scheduler {
        let store = Arc::new(external_store::Store::open(workspace.join("state.sqlite")).unwrap());
        // The zcode factory fails loudly if a test accidentally routes a task
        // away from the DSH adapter under test.
        let zcode = CommandRuntimeFactory::new(|_: &TaskRecord| {
            Err(io::Error::other("zcode route must not spawn in dsh tests"))
        });
        // The scripted children speak real process I/O; under the parallel
        // workspace suite the default 2s windows are too tight, so the tests
        // budget generously (none of them asserts deadline behavior).
        let config = SchedulerConfig {
            bootstrap_timeout: Duration::from_secs(30),
            control_timeout: Duration::from_secs(10),
            ..SchedulerConfig::default()
        };
        Scheduler::new(
            "dsh-test",
            store,
            Arc::new(RoutingRuntimeFactory::new(zcode, dsh)),
            config,
        )
        .unwrap()
    }

    fn dsh_workspace() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("s04a-dsh-")
            .tempdir_in(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../tests/live-agent/workspace"),
            )
            .unwrap()
    }

    fn manifest_for(workspace: &std::path::Path, prompt: &str) -> GeneralTaskManifest {
        GeneralTaskManifest {
            schema: GENERAL_TASK_SCHEMA.into(),
            agent_id: "dsh-build".into(),
            repository: workspace.canonicalize().unwrap(),
            permission_mode: PermissionMode::Build,
            prompt: prompt.into(),
            write_manifest: Vec::new(),
        }
    }

    fn enqueue_dsh(
        scheduler: &Scheduler,
        workspace: &std::path::Path,
        model: Option<&str>,
    ) -> String {
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for(workspace, "build the fixture"),
                Some(dsh_admission(model)),
            )
            .unwrap();
        submitted.task.agent_id
    }

    fn await_terminal_task(scheduler: &Scheduler, agent_id: &str) -> external_store::TaskRecord {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let task = scheduler.store().get_task(agent_id).unwrap().unwrap();
            if task.phase.is_terminal() {
                return task;
            }
            assert!(
                Instant::now() < deadline,
                "task never terminalized: {task:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
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

    fn await_pending_permission(
        scheduler: &Scheduler,
        agent_id: &str,
    ) -> external_store::StoredPendingRequest {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let requests = scheduler.store().pending_requests(agent_id).unwrap();
            if let Some(request) = requests.first() {
                assert_eq!(request.request_type, "permission");
                assert_eq!(request.state, PendingRequestState::Pending);
                return request.clone();
            }
            assert!(Instant::now() < deadline, "permission never became pending");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn closed_gate_refuses_dsh_spawn_without_touching_any_process() {
        let workspace = dsh_workspace();
        let scheduler = dsh_scheduler(workspace.path(), DshRuntimeFactory::closed());
        let agent_id = enqueue_dsh(&scheduler, workspace.path(), None);
        let error = scheduler.start_ready().unwrap_err();
        match &error {
            SchedulerError::RuntimeSpawn { message, .. } => {
                assert!(message.contains("dsh spawn gate is closed"), "{message}");
            }
            other => panic!("expected a runtime spawn refusal, got {other:?}"),
        }
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        assert_eq!(task.phase, TaskPhase::Terminal);
        assert_eq!(task.outcome, Some(TaskOutcome::Failed));
        let failure = scheduler.last_error(&agent_id).expect("failure record");
        assert!(failure.contains("RUNTIME_SPAWN_FAILED"), "{failure}");
        // The workspace saw no provider process at all.
        assert!(!workspace.path().join("wire.jsonl").exists());
    }

    /// Minimal sink for direct factory probes; records nothing.
    struct NoopSink;
    impl crate::LifecycleSink for NoopSink {
        fn emit(&self, _record: crate::LifecycleRecord) {}
    }

    #[test]
    fn routing_factory_keeps_the_zcode_route_on_the_zcode_factory() {
        let _guard = scripted_test_guard();
        let workspace = dsh_workspace();
        let store =
            Arc::new(external_store::Store::open(workspace.path().join("state.sqlite")).unwrap());
        let zcode = CommandRuntimeFactory::new(|_: &TaskRecord| {
            let mut command = Command::new("sh");
            command.args(["-c", "sleep 5"]);
            Ok(command)
        });
        let factory = RoutingRuntimeFactory::new(zcode, DshRuntimeFactory::closed());
        // A scheduler is used only to persist the prepared task; spawn goes
        // through the routing factory directly.
        let enqueue_only = Scheduler::new(
            "enqueue-only",
            Arc::clone(&store),
            Arc::new(DshRuntimeFactory::closed()),
            SchedulerConfig::default(),
        )
        .unwrap();
        let submitted = enqueue_only
            .enqueue_general(&manifest_for(workspace.path(), "route check"))
            .unwrap();
        // Enqueue with plan mode stores a prepared task whose route lacks an
        // admission identity; such tasks keep the zcode route.
        let task = store.get_task(&submitted.task.agent_id).unwrap().unwrap();
        assert_eq!(task_agent(&task), "zcode");
        let runtime = RuntimeFactory::spawn(&factory, &task, Arc::new(NoopSink))
            .expect("zcode tasks keep the zcode factory");
        assert!(runtime.identity().is_some());
        let terminal = runtime.stop(Duration::from_millis(200));
        assert!(terminal_proves_process_group_reaped(&terminal));
    }

    /// Shared bootstrap + one build turn prefix for the scripted children.
    const BOOTSTRAP_PREFIX: &str = r#"
read_frame
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"capabilities":{"models":true,"cancel":true,"permission":true}}}'
read_frame
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"SESSION","configOptions":[{"configId":"model"}]}}'
read_frame
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"configOptions":[]}}'
read_frame
printf '%s\n' \
  '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"SESSION","update":{"type":"tool_call","toolCallId":"tool-7","kind":"edit","title":"Edit fixture"}}}' \
  '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"SESSION","update":{"type":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}}' \
  '{"jsonrpc":"2.0","id":"srv-1","method":"session/request_permission","params":{"sessionId":"SESSION","toolCallId":"tool-7","options":[{"optionId":"allow-once","kind":"allow_once"},{"optionId":"reject-once","kind":"reject_once"}]}}'
read_frame
"#;

    #[test]
    fn build_task_flows_model_permission_and_result_through_the_shared_lifecycle() {
        let _guard = scripted_test_guard();
        let workspace = dsh_workspace();
        let script = format!(
            "{BOOTSTRAP_PREFIX}\
printf '%s\\n' \
  '{{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{{\"sessionId\":\"{SESSION_ID}\",\"update\":{{\"type\":\"agent_message\",\"messageId\":\"message-final\",\"content\":[{{\"type\":\"text\",\"text\":\"build \"}},{{\"type\":\"text\",\"text\":\"answer\"}}]}}}}}}' \
  '{{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{{\"stopReason\":\"end_turn\",\"messageId\":\"message-final\"}}}}'
read_frame
printf '%s\\n' \
  '{{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{{\"sessionId\":\"{SESSION_ID}\",\"update\":{{\"type\":\"agent_message\",\"messageId\":\"message-2\",\"content\":[{{\"type\":\"text\",\"text\":\"follow-up settled\"}}]}}}}}}' \
  '{{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{{\"stopReason\":\"end_turn\",\"messageId\":\"message-2\"}}}}'
"
        )
        .replace("SESSION", SESSION_ID);
        let child = scripted_child(workspace.path(), &script);
        let scheduler = dsh_scheduler(
            workspace.path(),
            DshRuntimeFactory::test_harness(Some(child)),
        );
        let agent_id = enqueue_dsh(&scheduler, workspace.path(), Some("fixture-model"));

        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);

        // The permission request reaches the store with the correlated tool
        // identity (X06): the toolCallId came from a prior tool update.
        let request = await_pending_permission(&scheduler, &agent_id);
        let payload: serde_json::Value = serde_json::from_str(&request.payload_json).unwrap();
        assert_eq!(payload["toolName"], "edit");
        assert_eq!(payload["toolCallId"], "tool-7");

        // Queue the follow-up while the turn is still blocked on permission;
        // delivery may only happen after settlement.
        assert_eq!(
            scheduler
                .queue_message(&agent_id, "follow-up", "queue", "follow-up prompt")
                .unwrap(),
            MessageDisposition::Queued
        );

        let outcome = scheduler
            .respond_request(&agent_id, &request.request_id, "allow", None)
            .unwrap();
        assert_eq!(outcome.disposition, ResponseDisposition::Responded);
        assert_eq!(outcome.effective_decision, "allow");
        assert!(!outcome.policy_overrode);

        let stored = await_result(&scheduler, &agent_id);
        assert_eq!(stored.result.outcome, TaskOutcome::Completed);
        assert_eq!(stored.result.final_text, "follow-up settled");
        assert!(!stored.result.partial);

        // The queued message was delivered exactly once.
        let receipt = scheduler.store().message("follow-up").unwrap().unwrap();
        assert_eq!(receipt.state, MessageState::Delivered);
        assert_eq!(
            scheduler
                .queue_message(&agent_id, "follow-up", "queue", "follow-up prompt")
                .unwrap(),
            MessageDisposition::AlreadyDelivered
        );

        // Natural completion released the runtime and terminalized the task.
        let deadline = Instant::now() + Duration::from_secs(5);
        while scheduler.active_count() != 0 {
            assert!(Instant::now() < deadline, "runtime was not released");
            thread::sleep(Duration::from_millis(10));
        }
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        assert_eq!(task.phase, TaskPhase::Terminal);
        assert_eq!(task.outcome, Some(TaskOutcome::Completed));

        // Wire evidence: bootstrap order, model verified before the prompt,
        // one prompt per turn, and a single-shot allow echoing the offered id.
        // (The allow response sits between the two prompts in arrival order.)
        let frames = wait_for_frames(workspace.path(), 6);
        assert_eq!(
            request_methods(&frames),
            vec![
                "initialize",
                "session/new",
                "session/set_config_option",
                "session/prompt",
                "session/prompt"
            ]
        );
        assert_eq!(frames[2]["params"]["configId"], "model");
        assert_eq!(frames[2]["params"]["value"], "fixture-model");
        let prompts: Vec<&serde_json::Value> = frames
            .iter()
            .filter(|frame| {
                frame.get("method").and_then(|value| value.as_str()) == Some("session/prompt")
            })
            .collect();
        assert_eq!(prompts.len(), 2);
        assert!(prompts[0]["params"]["prompt"]
            .as_str()
            .unwrap()
            .contains("build the fixture"));
        assert!(prompts[1]["params"]["prompt"]
            .as_str()
            .unwrap()
            .contains("follow-up prompt"));
        let permission_responses: Vec<&serde_json::Value> = frames
            .iter()
            .filter(|frame| frame.get("id").and_then(|id| id.as_str()) == Some("srv-1"))
            .collect();
        assert_eq!(permission_responses.len(), 1, "respond must happen once");
        assert_eq!(
            permission_responses[0]["result"]["outcome"]["optionId"],
            "allow-once"
        );
        for frame in &frames {
            assert_eq!(frame["jsonrpc"], "2.0");
        }
    }

    #[test]
    fn max_tokens_settlement_fails_the_task_without_faking_completion() {
        let _guard = scripted_test_guard();
        let workspace = dsh_workspace();
        let script = format!(
            "{BOOTSTRAP_PREFIX}\
printf '%s\\n' \
  '{{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{{\"sessionId\":\"{SESSION_ID}\",\"update\":{{\"type\":\"agent_message\",\"messageId\":\"message-final\",\"content\":[{{\"type\":\"text\",\"text\":\"truncated\"}}]}}}}}}' \
  '{{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{{\"stopReason\":\"max_tokens\",\"messageId\":\"message-final\"}}}}'
"
        )
        .replace("SESSION", SESSION_ID);
        let child = scripted_child(workspace.path(), &script);
        let scheduler = dsh_scheduler(
            workspace.path(),
            DshRuntimeFactory::test_harness(Some(child)),
        );
        let agent_id = enqueue_dsh(&scheduler, workspace.path(), Some("fixture-model"));
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);

        let request = await_pending_permission(&scheduler, &agent_id);
        scheduler
            .respond_request(&agent_id, &request.request_id, "allow", None)
            .unwrap();

        let stored = await_result(&scheduler, &agent_id);
        assert_eq!(stored.result.outcome, TaskOutcome::Failed);
        // The committed message the child emitted before the max_tokens
        // settlement must never become the task result.
        assert!(
            !stored.result.final_text.contains("truncated"),
            "leaked settlement text: {}",
            stored.result.final_text
        );
        let task = await_terminal_task(&scheduler, &agent_id);
        assert_eq!(task.outcome, Some(TaskOutcome::Failed));
        // The terminal is the failed turn boundary (never a runtime loss and
        // never a faked completion).
        let failure = scheduler.last_error(&agent_id).expect("failure record");
        assert!(failure.contains("RUNTIME_TERMINAL"), "{failure}");
        assert!(failure.contains("FailedTurn"), "{failure}");

        let frames = wire_frames(workspace.path());
        assert_eq!(
            request_methods(&frames),
            vec![
                "initialize",
                "session/new",
                "session/set_config_option",
                "session/prompt"
            ]
        );
    }

    #[test]
    fn rejected_model_selection_fails_before_any_prompt_is_sent() {
        let _guard = scripted_test_guard();
        let workspace = dsh_workspace();
        let script = format!(
            r#"
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"capabilities":{{"models":true,"cancel":true,"permission":true}}}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"{SESSION_ID}","configOptions":[{{"configId":"model"}}]}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"error":{{"code":-32602,"message":"unknown model option: nope"}}}}'
"#
        );
        let child = scripted_child(workspace.path(), &script);
        let scheduler = dsh_scheduler(
            workspace.path(),
            DshRuntimeFactory::test_harness(Some(child)),
        );
        let agent_id = enqueue_dsh(&scheduler, workspace.path(), Some("nope"));
        // The refused model selection fails bootstrap and start_ready surfaces
        // the bounded provider rejection.
        let error = scheduler.start_ready().unwrap_err();
        assert!(
            error.to_string().contains("unknown model option"),
            "{error}"
        );

        let task = await_terminal_task(&scheduler, &agent_id);
        assert_eq!(task.outcome, Some(TaskOutcome::Failed));
        let failure = scheduler.last_error(&agent_id).expect("failure record");
        assert!(failure.contains("SESSION_START_FAILED"), "{failure}");
        assert!(failure.contains("unknown model option"), "{failure}");

        let frames = wire_frames(workspace.path());
        assert_eq!(
            request_methods(&frames),
            vec!["initialize", "session/new", "session/set_config_option"],
            "no prompt may follow a refused model selection"
        );
    }

    #[test]
    fn cross_provider_shared_scheduler_contract() {
        let _guard = scripted_test_guard();
        let workspace = dsh_workspace();
        let scheduler = dsh_scheduler(workspace.path(), DshRuntimeFactory::closed());
        let agent_id = enqueue_dsh(&scheduler, workspace.path(), None);
        let error = scheduler.start_ready().expect_err("closed DSH gate must reject");
        assert!(error.to_string().contains("unsupported") || error.to_string().contains("closed"));
        let task = await_terminal_task(&scheduler, &agent_id);
        assert_eq!(task.outcome, Some(TaskOutcome::Failed));
        assert!(scheduler.last_error(&agent_id).is_some());
    }

    #[test]
    fn dsh_pending_task_cancel_is_terminal_and_non_resurrecting() {
        let workspace = dsh_workspace();
        let scheduler = dsh_scheduler(workspace.path(), DshRuntimeFactory::closed());
        let agent_id = enqueue_dsh(&scheduler, workspace.path(), None);
        let phase = scheduler.cancel_task(&agent_id).expect("cancel pending task");
        assert_eq!(phase, TaskPhase::Cancelled);
        assert_eq!(scheduler.cancel_task(&agent_id).unwrap(), TaskPhase::Cancelled);
    }
}
