//! Codex upstream app-server runtime owner, gated factory, and launch
//! resolution.
//!
//! The Codex app-server speaks NDJSON over stdio with the same strict
//! no-`jsonrpc` envelope the ZCode driver already pins, so this owner reuses
//! [`external_runtime::Driver`] with the `ZcodeStrict` codec. What is
//! Codex-specific lives here: the `initialize`/`initialized` handshake, the
//! persistent `thread/start`/`thread/resume` identity, `turn/start` and
//! `turn/interrupt`, and the normalization of Codex `item/*` and `turn/*`
//! notifications into the canonical internal `session/event` lifecycle so the
//! existing scheduler, result, storage, and observation consumers keep working.
//!
//! Production spawn is selected only by the explicit configuration gate; the
//! closed factory remains the fail-closed default. Only `plan` tasks are
//! admitted (`sandbox=read-only`, `approvalPolicy=never`), and the child
//! always runs with an explicitly resolved `CODEX_HOME` — never a silent
//! `~/.codex` fallback.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use external_contract::{EventEnvelope, RequestEnvelope, WireMessage};
use external_runtime::{observe_process_group, ChildExit, Driver, FrameCodec, Inbound, StopOutcome};
use external_store::TaskRecord;

use crate::{
    task_route, LifecycleSink, ManagedRuntime, ProcessIdentity, Publisher, RuntimeCommandError,
    RuntimeFactory, RuntimeLoss, RuntimeTerminal, SessionReady, TurnBoundary, TurnSnapshot,
    TurnTracker,
};

/// Whether the Codex factory may spawn app-server processes at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexSpawnGate {
    /// Fail-closed default: the adapter exists but spawn is refused.
    Closed,
    /// Production launch after the persisted runtime/home gate succeeds.
    Enabled,
    /// Controlled test harness only; never constructed by the production
    /// composition root.
    #[cfg(test)]
    TestHarness,
}

/// Resolved Codex child launch contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexLaunch {
    runtime_path: PathBuf,
    home: PathBuf,
}

/// Home precedence for the Codex child: the persisted `agents.codex.home`
/// wins over the inherited `CODEX_HOME`; neither being present rejects the
/// launch instead of ever falling back to `~/.codex`.
pub fn resolve_codex_home(
    configured: Option<&str>,
    inherited: Option<&str>,
) -> Option<Result<PathBuf, &'static str>> {
    match (configured, inherited) {
        (Some(configured), _) => Some(
            Path::new(configured)
                .is_absolute()
                .then(|| PathBuf::from(configured))
                .ok_or("agents.codex.home must be absolute"),
        ),
        (None, Some(inherited)) => Some(
            Path::new(inherited)
                .is_absolute()
                .then(|| PathBuf::from(inherited))
                .ok_or("inherited CODEX_HOME must be absolute"),
        ),
        (None, None) => None,
    }
}

impl CodexLaunch {
    /// Resolve the launch contract from the daemon environment. `main` only
    /// exports `CODEX_HOME` when the persisted configuration supplies one, so
    /// an inherited value here is the deliberate second-priority source.
    pub fn from_environment() -> Result<Self, io::Error> {
        let runtime_path = env_runtime_path()?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "CODEX_RUNTIME_PATH is unavailable",
            )
        })?;
        let home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "CODEX_HOME is unconfigured; refusing a ~/.codex fallback",
                )
            })?;
        if !home.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CODEX_HOME must be absolute",
            ));
        }
        Ok(Self { runtime_path, home })
    }

    #[cfg(test)]
    pub fn new(runtime_path: PathBuf, home: PathBuf) -> Self {
        Self { runtime_path, home }
    }

    /// Fail-closed validation of the resolved contract: an absolute,
    /// executable runtime file and an absolute home.
    #[cfg(test)]
    fn validate(&self) -> io::Result<()> {
        if !self.runtime_path.is_absolute()
            || !self.runtime_path.is_file()
            || {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    !std::fs::metadata(&self.runtime_path)
                        .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
                }
                #[cfg(not(unix))]
                {
                    false
                }
            }
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CODEX_RUNTIME_PATH must be an absolute executable file",
            ));
        }
        if !self.home.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CODEX_HOME must be absolute",
            ));
        }
        Ok(())
    }

    pub fn runtime_path(&self) -> &Path {
        &self.runtime_path
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The pinned child command: `<runtime> app-server --listen stdio://`
    /// with the resolved home exported as `CODEX_HOME` and the task
    /// workspace as the working directory (mirroring the observed probe
    /// launch, where the process cwd matched the thread cwd).
    pub fn command(&self, cwd: &Path) -> Command {
        let mut command = Command::new(&self.runtime_path);
        command.args(["app-server", "--listen", "stdio://"]);
        command.env("CODEX_HOME", &self.home);
        command.current_dir(cwd);
        command
    }
}

fn env_runtime_path() -> Result<Option<PathBuf>, io::Error> {
    let Some(path) = std::env::var_os("CODEX_RUNTIME_PATH").map(PathBuf::from) else {
        return Ok(None);
    };
    if !path.is_absolute()
        || !path.is_file()
        || {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(&path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CODEX_RUNTIME_PATH must be an absolute executable file",
        ));
    }
    Ok(Some(path))
}

/// Factory for Codex app-server runtimes. Closed by default: production
/// routing can register the factory without enabling Codex spawn support.
pub struct CodexRuntimeFactory {
    gate: CodexSpawnGate,
    #[cfg(test)]
    launch: Option<CodexLaunch>,
}

impl CodexRuntimeFactory {
    pub fn closed() -> Self {
        Self {
            gate: CodexSpawnGate::Closed,
            #[cfg(test)]
            launch: None,
        }
    }

    pub fn enabled() -> Self {
        Self {
            gate: CodexSpawnGate::Enabled,
            #[cfg(test)]
            launch: None,
        }
    }

    #[cfg(test)]
    pub fn test_harness(launch: Option<CodexLaunch>) -> Self {
        Self {
            gate: CodexSpawnGate::TestHarness,
            launch,
        }
    }

    #[cfg_attr(not(test), allow(unused_variables))]
    fn resolve_launch(&self, task: &TaskRecord) -> io::Result<CodexLaunch> {
        match task_route(task) {
            Ok(crate::TaskRoute::General(prepared)) => {
                let admission = prepared.admission.as_ref().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "codex task is missing its admission identity",
                    )
                })?;
                if admission.agent != "codex" {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "codex factory received a non-codex task",
                    ));
                }
                if !matches!(
                    prepared.permission_mode,
                    external_core::PermissionMode::Plan
                ) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "codex runtime supports only the plan permission mode",
                    ));
                }
                if admission.model.as_deref().is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "codex task is missing its admitted model",
                    ));
                }
            }
            Err(message) => {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, message));
            }
        }
        match self.gate {
            CodexSpawnGate::Closed => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "codex spawn gate is closed; production Codex spawn is not enabled",
            )),
            CodexSpawnGate::Enabled => CodexLaunch::from_environment(),
            #[cfg(test)]
            CodexSpawnGate::TestHarness => {
                let launch = self
                    .launch
                    .clone()
                    .ok_or_else(|| CodexLaunch::from_environment().unwrap_err())?;
                launch.validate()?;
                Ok(launch)
            }
        }
    }
}

impl RuntimeFactory for CodexRuntimeFactory {
    fn spawn(
        &self,
        task: &TaskRecord,
        sink: Arc<dyn LifecycleSink>,
    ) -> io::Result<Arc<dyn ManagedRuntime>> {
        let launch = self.resolve_launch(task)?;
        let cwd = PathBuf::from(&task.workspace_path);
        Ok(Arc::new(CodexRuntimeOwner::spawn(
            launch.command(&cwd),
            sink,
        )?))
    }
}

const MAX_ITEM_TEXT_BYTES: usize = 512 * 1024;
const MAX_TRACKED_ITEMS: usize = 128;
const MAX_TURN_FAILURE_DETAIL_BYTES: usize = 512;
const MAX_MCP_DIAGNOSTIC_BYTES: usize = 2 * 1024;

struct CodexShared {
    publisher: Arc<Publisher>,
    turn_tracker: Arc<TurnTracker>,
    session_id: Mutex<Option<String>>,
    admitted_model: Mutex<Option<String>>,
    diagnostic_session_id: Mutex<Option<String>>,
    current_turn: Mutex<Option<String>>,
    last_message_item: Mutex<Option<String>>,
    items: Mutex<HashMap<String, String>>,
    turn_failure: Mutex<Option<String>>,
    mcp_tail: Mutex<String>,
    sequence: AtomicU64,
    stop_boundaries: AtomicU64,
}

impl CodexShared {
    fn next_event_id(&self) -> String {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        format!("codex-event-{sequence}")
    }

    fn canonical_event(&self, params: serde_json::Value) -> Inbound {
        Inbound::Message(WireMessage::Event(EventEnvelope {
            method: external_contract::SESSION_EVENT.into(),
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
                order: external_contract::classify_lifecycle(method, false),
            },
            None,
        );
    }

    fn observe_item_text(&self, item_id: &str, text: &str) {
        let mut items = self.items.lock().unwrap();
        if !items.contains_key(item_id) && items.len() >= MAX_TRACKED_ITEMS {
            return;
        }
        let entry = items.entry(item_id.to_owned()).or_default();
        let bounded = text.len() + entry.len() <= MAX_ITEM_TEXT_BYTES;
        if bounded {
            entry.push_str(text);
        } else {
            let remaining = MAX_ITEM_TEXT_BYTES.saturating_sub(entry.len());
            if remaining > 0 {
                let mut end = remaining;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                entry.push_str(&text[..end]);
            }
        }
    }

    fn item_text(&self, item_id: &str) -> Option<String> {
        self.items.lock().unwrap().get(item_id).cloned()
    }

    fn record_turn_failure(&self, detail: String) {
        let bounded: String = detail.chars().take(MAX_TURN_FAILURE_DETAIL_BYTES).collect();
        *self.turn_failure.lock().unwrap() = Some(bounded);
    }

    fn record_mcp_failure(&self, name: &str, error: &str) {
        let mut tail = self.mcp_tail.lock().unwrap();
        let line = format!("mcp {name} failed: {error}\n");
        tail.push_str(&line);
        if tail.len() > MAX_MCP_DIAGNOSTIC_BYTES {
            let mut keep = tail.len() - MAX_MCP_DIAGNOSTIC_BYTES;
            while keep < tail.len() && !tail.is_char_boundary(keep) {
                keep += 1;
            }
            tail.drain(..keep);
        }
    }

    /// One inbound Codex notification, normalized. Returns `None` when the
    /// frame was fully projected into canonical events; otherwise the
    /// original frame is re-emitted unchanged as a diagnostic observation.
    fn project_notification(&self, method: &str, raw: &serde_json::Value) -> bool {
        let params = raw
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let session = self.session_id.lock().unwrap().clone();
        let thread_id = params.get("threadId").and_then(|value| value.as_str());
        if let (Some(expected), Some(actual)) = (session.as_deref(), thread_id) {
            if expected != actual {
                // Foreign-thread traffic is never projected onto this task.
                return true;
            }
        }
        let event_id = self.next_event_id();
        match method {
            "turn/started" => {
                let Some(turn_id) = params
                    .pointer("/turn/id")
                    .and_then(|value| value.as_str())
                    .filter(|id| !id.is_empty())
                else {
                    return true;
                };
                *self.current_turn.lock().unwrap() = Some(turn_id.to_owned());
                *self.last_message_item.lock().unwrap() = None;
                // A stale failure detail from an earlier turn must never
                // label this turn's boundary.
                *self.turn_failure.lock().unwrap() = None;
                // The sink observes the canonical event before the tracker
                // exposes the new turn, matching the boundary ordering.
                let event = self.canonical_event(serde_json::json!({
                    "type": "turn.started",
                    "eventId": event_id,
                    "turnId": turn_id,
                }));
                self.emit_canonical(serde_json::json!({
                    "type": "turn.started",
                    "eventId": event_id,
                    "turnId": turn_id,
                }));
                self.turn_tracker.observe(&event);
                self.emit_lifecycle("turn.started");
                false
            }
            "item/agentMessage/delta" => {
                let (Some(delta), Some(item_id)) = (
                    params.get("delta").and_then(|value| value.as_str()),
                    params.get("itemId").and_then(|value| value.as_str()),
                ) else {
                    return true;
                };
                let turn_id = self
                    .current_turn
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| "codex-thread".into());
                self.observe_item_text(item_id, delta);
                self.emit_canonical(serde_json::json!({
                    "type": "model.streaming",
                    "eventId": event_id,
                    "turnId": turn_id,
                    "payload": {
                        "kind": "text_delta",
                        "delta": delta,
                        "assistantMessageId": item_id,
                    },
                }));
                false
            }
            "item/completed" => {
                let item = params.get("item").unwrap_or(&serde_json::Value::Null);
                if item.get("type").and_then(|value| value.as_str()) != Some("agentMessage") {
                    return true;
                }
                let Some(item_id) = item.get("id").and_then(|value| value.as_str()) else {
                    return true;
                };
                if let Some(text) = item.get("text").and_then(|value| value.as_str()) {
                    let mut items = self.items.lock().unwrap();
                    if items.contains_key(item_id) || items.len() < MAX_TRACKED_ITEMS {
                        items.insert(item_id.to_owned(), text.to_owned());
                    }
                }
                *self.last_message_item.lock().unwrap() = Some(item_id.to_owned());
                let turn_id = self
                    .current_turn
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| "codex-thread".into());
                self.emit_canonical(serde_json::json!({
                    "type": "message.finished",
                    "eventId": event_id,
                    "turnId": turn_id,
                    "payload": {"assistantMessageId": item_id},
                }));
                false
            }
            "turn/completed" => {
                let turn = params.get("turn").unwrap_or(&serde_json::Value::Null);
                let status = turn.get("status").and_then(|value| value.as_str());
                let turn_id = turn.get("id").and_then(|value| value.as_str());
                let current = self.current_turn.lock().unwrap().clone();
                if let (Some(current), Some(arrived)) = (current.as_deref(), turn_id) {
                    if current != arrived {
                        // A late boundary for a foreign turn never settles
                        // this task's active turn.
                        return true;
                    }
                }
                match status {
                    Some("completed") => {
                        let final_text = self.final_text(turn);
                        let mut payload = serde_json::json!({});
                        if let Some(text) = final_text {
                            payload["response"] = serde_json::Value::String(text);
                        }
                        let params = serde_json::json!({
                            "type": "turn.completed",
                            "eventId": event_id,
                            "turnId": turn_id.unwrap_or("codex-thread"),
                            "payload": payload,
                        });
                        let boundary = self.canonical_event(params.clone());
                        self.emit_canonical(params);
                        self.turn_tracker.observe(&boundary);
                        self.emit_lifecycle("turn.completed");
                        false
                    }
                    Some("failed") | Some("interrupted") => {
                        let reason = self.turn_failure_reason(turn, status.unwrap_or("failed"));
                        self.record_turn_failure(reason.clone());
                        let params = serde_json::json!({
                            "type": "turn.failed",
                            "eventId": event_id,
                            "turnId": turn_id.unwrap_or("codex-thread"),
                            "payload": {"reason_code": reason},
                        });
                        let boundary = self.canonical_event(params.clone());
                        self.emit_canonical(params);
                        self.turn_tracker.observe(&boundary);
                        self.emit_lifecycle("turn.failed");
                        false
                    }
                    _ => true,
                }
            }
            "error" => {
                // A turn-scoped application error (for example a
                // serverOverloaded capacity failure) is recorded for the
                // boundary projection; by itself it never completes a task.
                let error = params.get("error").cloned().unwrap_or(serde_json::Value::Null);
                let info = error
                    .get("codexErrorInfo")
                    .and_then(|value| value.as_str())
                    .unwrap_or("codex_error");
                let message = error
                    .get("message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("codex reported a turn error");
                self.record_turn_failure(format!("{info}: {message}"));
                true
            }
            "mcpServer/startupStatus/updated" => {
                if params.get("status").and_then(|value| value.as_str()) == Some("failed") {
                    let name = params
                        .get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown");
                    let error = params
                        .get("error")
                        .and_then(|value| value.as_str())
                        .unwrap_or("startup failed");
                    // An unrelated MCP startup failure stays diagnostic: it
                    // never fails the turn by itself.
                    self.record_mcp_failure(name, error);
                }
                true
            }
            _ => true,
        }
    }

    fn final_text(&self, turn: &serde_json::Value) -> Option<String> {
        if let Some(item_id) = self.last_message_item.lock().unwrap().clone() {
            if let Some(text) = self.item_text(&item_id) {
                return Some(text);
            }
        }
        turn.get("items")?
            .as_array()?
            .iter()
            .find(|item| {
                item.get("type").and_then(|value| value.as_str()) == Some("agentMessage")
            })
            .and_then(|item| item.get("text"))
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    }

    fn turn_failure_reason(&self, turn: &serde_json::Value, status: &str) -> String {
        if let Some(recorded) = self.turn_failure.lock().unwrap().clone() {
            return recorded;
        }
        let message = turn
            .pointer("/error/message")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("codex turn ended with status {status}"));
        let bounded: String = message.chars().take(256).collect();
        bounded
    }

    fn diagnostic_tail(&self) -> String {
        let mut sections = Vec::new();
        if let Some(failure) = self.turn_failure.lock().unwrap().clone() {
            sections.push(format!("codex turn failure: {failure}"));
        }
        let mcp = self.mcp_tail.lock().unwrap().clone();
        if !mcp.is_empty() {
            sections.push(mcp.trim_end().to_owned());
        }
        sections.join("\n")
    }
}

pub struct CodexRuntimeOwner {
    driver: Arc<Driver>,
    shared: Arc<CodexShared>,
    shutdown: Arc<AtomicBool>,
}

impl CodexRuntimeOwner {
    pub fn spawn(command: Command, sink: Arc<dyn LifecycleSink>) -> io::Result<Self> {
        let driver = Arc::new(Driver::spawn_with_codec(command, FrameCodec::ZcodeStrict)?);
        let publisher = Arc::new(Publisher::new(sink));
        let shared = Arc::new(CodexShared {
            publisher: Arc::clone(&publisher),
            turn_tracker: Arc::new(TurnTracker::new()),
            session_id: Mutex::new(None),
            admitted_model: Mutex::new(None),
            diagnostic_session_id: Mutex::new(None),
            current_turn: Mutex::new(None),
            last_message_item: Mutex::new(None),
            items: Mutex::new(HashMap::new()),
            turn_failure: Mutex::new(None),
            mcp_tail: Mutex::new(String::new()),
            sequence: AtomicU64::new(0),
            stop_boundaries: AtomicU64::new(0),
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        spawn_codex_pump(
            Arc::clone(&driver),
            Arc::clone(&publisher),
            Arc::clone(&shared),
            Arc::clone(&shutdown),
        );
        Ok(Self {
            driver,
            shared,
            shutdown,
        })
    }

    /// The `initialize` + `initialized` handshake every thread call requires.
    fn initialize_before_threads(&self, deadline: Instant) -> Result<(), RuntimeCommandError> {
        let params = serde_json::json!({
            "clientInfo": {
                "name": "external-subagent",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {},
        });
        let response = self
            .driver
            .request("initialize", params, remaining_time(deadline)?)?;
        let home = response
            .result
            .as_ref()
            .and_then(|result| result.get("codexHome"))
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                RuntimeCommandError::InvalidSession(
                    "initialize result is missing codexHome".into(),
                )
            })?;
        if home.is_empty() || home.len() > 4096 {
            return Err(RuntimeCommandError::InvalidSession(
                "initialize returned an invalid codexHome".into(),
            ));
        }
        self.driver
            .send(&serde_json::json!({"method": "initialized"}))
            .map_err(|error| RuntimeCommandError::Transport(error.to_string()))?;
        Ok(())
    }

    fn admitted_model(task: &TaskRecord) -> Result<String, RuntimeCommandError> {
        match task_route(task) {
            Ok(crate::TaskRoute::General(prepared)) => {
                let admission = prepared
                    .admission
                    .as_ref()
                    .ok_or_else(|| {
                        RuntimeCommandError::InvalidSession(
                            "codex task is missing its admission identity".into(),
                        )
                    })?;
                if admission.agent != "codex" {
                    return Err(RuntimeCommandError::InvalidSession(
                        "runtime is not the admitted codex agent".into(),
                    ));
                }
                if !matches!(
                    prepared.permission_mode,
                    external_core::PermissionMode::Plan
                ) {
                    return Err(RuntimeCommandError::InvalidSession(
                        "codex runtime supports only the plan permission mode".into(),
                    ));
                }
                admission.model.clone().ok_or_else(|| {
                    RuntimeCommandError::InvalidSession(
                        "codex task is missing its admitted model".into(),
                    )
                })
            }
            Err(message) => Err(RuntimeCommandError::InvalidSession(message)),
        }
    }

    fn start_thread(
        &self,
        model: &str,
        workspace_path: &str,
        deadline: Instant,
    ) -> Result<String, RuntimeCommandError> {
        let params = serde_json::json!({
            "model": model,
            "cwd": workspace_path,
            "approvalPolicy": "never",
            "sandbox": "read-only",
            "ephemeral": false,
        });
        let response = self
            .driver
            .request("thread/start", params, remaining_time(deadline)?)?;
        let result = response.result.ok_or_else(|| {
            RuntimeCommandError::InvalidSession("thread/start returned an error".into())
        })?;
        let thread_id = result
            .pointer("/thread/id")
            .and_then(|value| value.as_str())
            .filter(|id| !id.is_empty() && id.len() <= 512)
            .ok_or_else(|| {
                RuntimeCommandError::InvalidSession(
                    "thread/start result is missing a bounded thread id".into(),
                )
            })?;
        if result
            .pointer("/thread/ephemeral")
            .and_then(|value| value.as_bool())
            != Some(false)
        {
            return Err(RuntimeCommandError::InvalidSession(
                "codex thread is not persistent (ephemeral must be false)".into(),
            ));
        }
        validate_thread_model(model, result.get("model"))?;
        Ok(thread_id.to_owned())
    }

    fn resume_thread(
        &self,
        thread_id: &str,
        model: &str,
        deadline: Instant,
    ) -> Result<(), RuntimeCommandError> {
        let params = serde_json::json!({
            "threadId": thread_id,
            "excludeTurns": true,
        });
        let response = self
            .driver
            .request("thread/resume", params, remaining_time(deadline)?)?;
        let result = response.result.ok_or_else(|| {
            RuntimeCommandError::InvalidSession("thread/resume returned an error".into())
        })?;
        if result.pointer("/thread/id").and_then(|value| value.as_str()) != Some(thread_id) {
            return Err(RuntimeCommandError::InvalidSession(
                "thread/resume returned a different thread id".into(),
            ));
        }
        if result
            .pointer("/thread/ephemeral")
            .and_then(|value| value.as_bool())
            != Some(false)
        {
            return Err(RuntimeCommandError::InvalidSession(
                "resumed codex thread is not persistent".into(),
            ));
        }
        validate_thread_model(model, result.get("model"))?;
        Ok(())
    }

    fn start_turn(
        &self,
        thread_id: &str,
        model: &str,
        input: &str,
        deadline: Instant,
    ) -> Result<Option<String>, RuntimeCommandError> {
        let previous = self.shared.turn_tracker.snapshot().generation;
        let params = serde_json::json!({
            "threadId": thread_id,
            "model": model,
            "effort": "low",
            "input": [{"type": "text", "text": input}],
        });
        let response = self
            .driver
            .request("turn/start", params, remaining_time(deadline)?)?;
        let turn_id = response
            .result
            .as_ref()
            .and_then(|result| result.pointer("/turn/id"))
            .and_then(|value| value.as_str())
            .filter(|id| !id.is_empty() && id.len() <= 512)
            .map(str::to_owned);
        self.shared
            .turn_tracker
            .wait_started_after(previous, remaining_time(deadline)?)?;
        Ok(turn_id)
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

fn validate_thread_model(requested: &str, observed: Option<&serde_json::Value>) -> Result<(), RuntimeCommandError> {
    let observed = observed
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            RuntimeCommandError::InvalidSession("MODEL_NOT_OBSERVED: thread model is missing".into())
        })?;
    if observed != requested {
        return Err(RuntimeCommandError::InvalidSession(
            "MODEL_MISMATCH: thread model differs from the admitted request".into(),
        ));
    }
    Ok(())
}

fn remaining_time(deadline: Instant) -> Result<Duration, RuntimeCommandError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(RuntimeCommandError::Timeout)
}

fn spawn_codex_pump(
    driver: Arc<Driver>,
    publisher: Arc<Publisher>,
    shared: Arc<CodexShared>,
    shutdown: Arc<AtomicBool>,
) {
    thread::spawn(move || loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        match driver.recv_timeout(Duration::from_millis(20)) {
            Ok(event) => {
                if let Inbound::Message(WireMessage::UnknownEvent { method, raw }) = &event {
                    if !shared.project_notification(method, raw) {
                        continue;
                    }
                }
                if let Inbound::Message(WireMessage::Request(request)) = &event {
                    // Plan-mode Codex (approvalPolicy=never) should not issue
                    // interaction requests; anything it does issue stays an
                    // observable, non-respondable record instead of hanging.
                    let unsupported = RequestEnvelope::new(
                        request.id.clone(),
                        external_contract::INTERACTION_REQUEST_UNSUPPORTED_INPUT,
                        serde_json::json!({
                            "origin": "codex_app_server",
                            "method": request.method,
                        }),
                    );
                    publisher.emit_driver(
                        Inbound::Message(WireMessage::Request(unsupported)),
                        None,
                    );
                    continue;
                }
                if let Inbound::ChildExited(exit) = &event {
                    driver.wait_diagnostics(Duration::from_secs(1));
                    let terminal = classify_child_exit(driver.identity().pgid, &shared, exit);
                    publisher.emit_driver(event, terminal);
                    return;
                }
                publisher.emit_driver(event, None);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
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
    shared: &CodexShared,
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

impl Drop for CodexRuntimeOwner {
    fn drop(&mut self) {
        let _ = self.stop(Duration::from_secs(1));
        self.shutdown.store(true, Ordering::Release);
    }
}

impl ManagedRuntime for CodexRuntimeOwner {
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
        let stderr = self.driver.diagnostic_tail();
        let codex = self.shared.diagnostic_tail();
        if codex.is_empty() {
            stderr
        } else {
            format!("{stderr}\n{codex}")
        }
    }

    fn diagnostic_session_id(&self) -> Option<String> {
        self.shared.diagnostic_session_id.lock().unwrap().clone()
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
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RuntimeCommandError::Timeout)?;
        let model = Self::admitted_model(task)?;
        self.initialize_before_threads(deadline)?;
        let thread_id = self.start_thread(&model, &task.workspace_path, deadline)?;
        *self.shared.session_id.lock().unwrap() = Some(thread_id.clone());
        *self.shared.admitted_model.lock().unwrap() = Some(model.clone());
        *self.shared.diagnostic_session_id.lock().unwrap() = Some(thread_id.clone());
        let prompt = task.initial_prompt.clone();
        let initial_turn_id = self.start_turn(&thread_id, &model, &prompt, deadline)?;
        Ok(SessionReady {
            session_id: thread_id,
            initial_turn_id,
            configured_model: Some(model),
        })
    }

    fn resume_session_with_mcp(
        &self,
        task: &TaskRecord,
        _mcp_servers: &[external_contract::StdioMcpServer],
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RuntimeCommandError::Timeout)?;
        let thread_id = task.zcode_session_id.as_deref().filter(|id| {
            !id.is_empty() && id.len() <= 512
        }).ok_or_else(|| {
            RuntimeCommandError::InvalidSession("task has no persisted session id".into())
        })?;
        let model = Self::admitted_model(task)?;
        *self.shared.diagnostic_session_id.lock().unwrap() = Some(thread_id.to_owned());
        self.initialize_before_threads(deadline)?;
        self.resume_thread(thread_id, &model, deadline)?;
        *self.shared.session_id.lock().unwrap() = Some(thread_id.to_owned());
        *self.shared.admitted_model.lock().unwrap() = Some(model.clone());
        // A resumed thread never replays the interrupted pre-crash turn: the
        // queued message below is the sole trigger for the next turn.
        Ok(SessionReady {
            session_id: thread_id.to_owned(),
            initial_turn_id: None,
            configured_model: Some(model),
        })
    }

    fn send_turn(
        &self,
        session_id: &str,
        content: &str,
        timeout: Duration,
    ) -> Result<Option<String>, RuntimeCommandError> {
        self.validate_session(session_id)?;
        if self.shared.turn_tracker.snapshot().active {
            return Err(RuntimeCommandError::InvalidSession(
                "a turn is already in flight for this codex thread".into(),
            ));
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RuntimeCommandError::Timeout)?;
        let admitted = self
            .shared
            .admitted_model
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| {
                RuntimeCommandError::InvalidSession(
                    "codex thread has no admitted model for a follow-up turn".into(),
                )
            })?;
        self.start_turn(session_id, &admitted, content, deadline)
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
        let turn_id = self.shared.current_turn.lock().unwrap().clone();
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RuntimeCommandError::Timeout)?;
        // turn/interrupt ends the active turn in the provider; the process
        // itself keeps running and the thread identity stays durable.
        let params = serde_json::json!({
            "threadId": session_id,
            "turnId": turn_id.unwrap_or_default(),
        });
        self.driver
            .request("turn/interrupt", params, remaining_time(deadline)?)?;
        let boundary = self
            .shared
            .turn_tracker
            .wait_boundary_after(current.generation, remaining_time(deadline)?)?;
        self.shared.stop_boundaries.fetch_add(1, Ordering::AcqRel);
        Ok(boundary)
    }

    fn respond_request(
        &self,
        _correlation_id: &str,
        decision: &str,
        _content: Option<&str>,
        _validated_denial: Option<&external_core::ValidatedPermissionDenial>,
        _deadline: Instant,
    ) -> Result<(), RuntimeCommandError> {
        let _ = decision;
        Err(RuntimeCommandError::Unsupported)
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
    use crate::{Scheduler, SchedulerConfig, terminal_proves_process_group_reaped};
    use external_core::{
        AdmissionIdentity, GeneralTaskManifest, PermissionMode, GENERAL_TASK_SCHEMA,
    };
    use external_store::{TaskOutcome, TaskPhase};

    fn codex_admission(model: Option<&str>) -> AdmissionIdentity {
        AdmissionIdentity {
            agent: "codex".into(),
            config_revision: 7,
            adapter_version: "test".into(),
            model: model.map(str::to_owned),
            model_source: "spawn_catalog".into(),
        }
    }

    /// Serialize tests that drive scripted app-server children through the
    /// whole scheduler, mirroring the DSH suite's scripted-child guard: the
    /// parallel suite already runs timing-sensitive fixture probes.
    fn scripted_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static SCRIPTED_CHILD_LOCK: Mutex<()> = Mutex::new(());
        SCRIPTED_CHILD_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn codex_workspace() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/live-agent/workspace")
            .canonicalize()
            .unwrap();
        tempfile::Builder::new()
            .prefix("s01-codex-")
            .tempdir_in(root)
            .unwrap()
    }

    fn manifest_for(workspace: &Path, prompt: &str) -> GeneralTaskManifest {
        GeneralTaskManifest {
            schema: GENERAL_TASK_SCHEMA.into(),
            agent_id: "codex-test".into(),
            repository: workspace.to_path_buf(),
            permission_mode: PermissionMode::Plan,
            prompt: prompt.into(),
            write_manifest: Vec::new(),
        }
    }

    fn codex_scheduler(
        workspace: &Path,
        factory: CodexRuntimeFactory,
    ) -> Scheduler {
        let store = Arc::new(external_store::Store::open(workspace.join("state.sqlite")).unwrap());
        let zcode = crate::CommandRuntimeFactory::new(|_: &TaskRecord| {
            Err::<Command, _>(io::Error::other("zcode factory must not spawn codex tasks"))
        });
        Scheduler::new(
            "codex-test",
            store,
            Arc::new(crate::dsh::RoutingRuntimeFactory::with_codex(
                zcode,
                crate::dsh::DshRuntimeFactory::closed(),
                factory,
            )),
            // Generous deadlines: the scripted children share the machine
            // with the rest of the parallel suite.
            SchedulerConfig {
                bootstrap_timeout: Duration::from_secs(30),
                control_timeout: Duration::from_secs(30),
                ..SchedulerConfig::default()
            },
        )
        .unwrap()
    }

    fn harness_factory(script: &str, workspace: &Path) -> CodexRuntimeFactory {
        let child = workspace.join("codex-fake.sh");
        std::fs::write(&child, format!("#!/bin/sh\n{script}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut mode = std::fs::metadata(&child).unwrap().permissions();
            mode.set_mode(0o755);
            std::fs::set_permissions(&child, mode).unwrap();
        }
        let launch = CodexLaunch::new(child, workspace.join("codex-home"));
        CodexRuntimeFactory::test_harness(Some(launch))
    }

    fn await_terminal_task(scheduler: &Scheduler, agent_id: &str) -> external_store::TaskRecord {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let task = scheduler.store().get_task(agent_id).unwrap().unwrap();
            if task.phase == TaskPhase::Terminal {
                return task;
            }
            assert!(Instant::now() < deadline, "task never became terminal");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn await_result(scheduler: &Scheduler, agent_id: &str) -> external_store::StoredTaskResult {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = scheduler.store().task_result(agent_id).unwrap() {
                return result;
            }
            assert!(Instant::now() < deadline, "result was never persisted");
            thread::sleep(Duration::from_millis(10));
        }
    }

    const THREAD_ID: &str = "codex-thread-1";
    const MODEL: &str = "gpt-5.6-terra";

    /// A scripted app-server speaking the strict frame sequence of a fresh
    /// task: initialize(id1), initialized notification, thread/start(id2),
    /// turn/start(id3). Every inbound frame is appended to deliveries.jsonl
    /// as it arrives.
    const HAPPY_TURN: &str = r#"
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{"id":1,"result":{"codexHome":"/tmp/codex-home","userAgent":"fake"}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-5.6-terra"}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{"id":3,"result":{"turn":{"id":"codex-turn-1","status":"inProgress"}}}' \
  '{"method":"turn/started","params":{"threadId":"codex-thread-1","turn":{"id":"codex-turn-1","status":"inProgress"}}}' \
  '{"method":"item/agentMessage/delta","params":{"threadId":"codex-thread-1","turnId":"codex-turn-1","itemId":"msg_1","delta":"CODEX_OK"}}' \
  '{"method":"item/completed","params":{"threadId":"codex-thread-1","turnId":"codex-turn-1","item":{"type":"agentMessage","id":"msg_1","text":"CODEX_OK"}}}' \
  '{"method":"turn/completed","params":{"threadId":"codex-thread-1","turn":{"id":"codex-turn-1","status":"completed","error":null}}}'
while IFS= read -r line; do printf '%s\n' "$line" >> deliveries.jsonl; done
"#;

    #[test]
    fn public_submit_reaches_persistent_thread_and_persists_the_id() {
        let _guard = scripted_test_guard();
        let workspace = codex_workspace();
        let scheduler = codex_scheduler(workspace.path(), harness_factory(HAPPY_TURN, workspace.path()));
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for(workspace.path(), "inspect the repository"),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let agent_id = submitted.task.agent_id.clone();
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        let result = await_result(&scheduler, &agent_id);
        assert_eq!(result.result.outcome, TaskOutcome::Completed);
        assert_eq!(result.result.final_text, "CODEX_OK");
        let task = await_terminal_task(&scheduler, &agent_id);
        assert_eq!(task.zcode_session_id.as_deref(), Some(THREAD_ID));
        // The exact child contract is visible in the recorded deliveries.
        let deliveries =
            std::fs::read_to_string(workspace.path().join("deliveries.jsonl")).unwrap();
        let initialize: serde_json::Value = serde_json::from_str(
            deliveries.lines().next().expect("at least one request"),
        )
        .unwrap();
        assert_eq!(initialize["method"], "initialize");
        let mut requests = deliveries.lines().map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()
        });
        let thread_start = requests
            .find(|frame| frame["method"] == "thread/start")
            .expect("thread/start frame");
        assert_eq!(thread_start["params"]["ephemeral"], false);
        assert_eq!(thread_start["params"]["approvalPolicy"], "never");
        assert_eq!(thread_start["params"]["sandbox"], "read-only");
        assert_eq!(thread_start["params"]["model"], MODEL);
        assert_eq!(
            thread_start["params"]["cwd"],
            workspace.path().to_string_lossy().as_ref()
        );
        let turn_start = requests
            .find(|frame| frame["method"] == "turn/start")
            .expect("turn/start frame");
        assert_eq!(turn_start["params"]["threadId"], THREAD_ID);
        assert_eq!(turn_start["params"]["model"], MODEL);
        assert!(turn_start["params"]["input"][0]["text"]
            .as_str()
            .unwrap()
            .contains("inspect the repository"));
    }

    #[test]
    fn closed_gate_and_plan_only_tasks_refuse_spawn_without_a_process() {
        let _guard = scripted_test_guard();
        let workspace = codex_workspace();
        let scheduler = codex_scheduler(workspace.path(), CodexRuntimeFactory::closed());
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for(workspace.path(), "never runs"),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let error = scheduler.start_ready().unwrap_err();
        assert!(error.to_string().contains("codex spawn gate is closed"));
        let task = await_terminal_task(&scheduler, &submitted.task.agent_id);
        assert_eq!(task.outcome, Some(TaskOutcome::Failed));

        // Write modes refuse before the prompt: at the factory seam the
        // admitted plan-only contract is re-checked fail-closed.
        let mut manifest = manifest_for(workspace.path(), "write mode");
        manifest.permission_mode = PermissionMode::Build;
        let prepared = external_core::GeneralTaskPreparer::new(Vec::new())
            .unwrap()
            .prepare_direct_submission(&manifest)
            .unwrap()
            .with_admission(codex_admission(Some(MODEL)))
            .unwrap();
        let task = TaskRecord {
            agent_id: "build-mode".into(),
            repository: workspace.path().to_string_lossy().into_owned(),
            phase: TaskPhase::Queued,
            outcome: None,
            workspace_path: workspace.path().to_string_lossy().into_owned(),
            runtime_hash: None,
            prepared_launch_json: serde_json::to_string(&prepared).unwrap(),
            prepared_launch_sha256: prepared.prepared_sha256.clone(),
            initial_prompt: "prompt".into(),
            owner_id: None,
            owner_epoch: 0,
            close_requested: false,
            stop_requested: false,
            last_event_seq: 0,
            failure_code: None,
            failure_message: None,
            runtime_agent_id: None,
            zcode_session_id: None,
            turn_state: external_store::TurnState::Idle,
            process_identity: None,
            closed_at: None,
            reaped_at: None,
            created_at: 0,
        };
        let sink: Arc<dyn LifecycleSink> = Arc::new(NoopSink);
        for mode in [PermissionMode::Build, PermissionMode::Edit, PermissionMode::Yolo] {
            let mut prepared_manifest = manifest_for(workspace.path(), "write mode");
            prepared_manifest.permission_mode = mode;
            let prepared = external_core::GeneralTaskPreparer::new(Vec::new())
                .unwrap()
                .prepare_direct_submission(&prepared_manifest)
                .unwrap()
                .with_admission(codex_admission(Some(MODEL)))
                .unwrap();
            let mut mode_task = task.clone();
            mode_task.prepared_launch_json = serde_json::to_string(&prepared).unwrap();
            mode_task.prepared_launch_sha256 = prepared.prepared_sha256.clone();
            let error = harness_factory(HAPPY_TURN, workspace.path())
                .spawn(&mode_task, Arc::clone(&sink))
                .err()
                .expect("write mode must refuse the spawn");
            assert!(
                error.to_string().contains("only the plan permission mode"),
                "write mode {mode:?} was not refused: {error}"
            );
        }
    }

    struct NoopSink;
    impl crate::LifecycleSink for NoopSink {
        fn emit(&self, _record: crate::LifecycleRecord) {}
    }

    #[test]
    fn terminal_send_resumes_the_same_thread_in_a_new_process_without_replay() {
        let _guard = scripted_test_guard();
        let workspace = codex_workspace();
        let directory = workspace.path().to_owned();
        let resume_script = r#"
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
printf '%s\n' '{"id":1,"result":{"codexHome":"/tmp/codex-home","userAgent":"fake"}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
printf '%s\n' '{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-5.6-terra"}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
printf '%s\n' '{"id":3,"result":{"turn":{"id":"codex-turn-2","status":"inProgress"}}}' \
  '{"method":"turn/started","params":{"threadId":"codex-thread-1","turn":{"id":"codex-turn-2","status":"inProgress"}}}' \
  '{"method":"item/completed","params":{"threadId":"codex-thread-1","turnId":"codex-turn-2","item":{"type":"agentMessage","id":"msg_2","text":"RESUMED_OK"}}}' \
  '{"method":"turn/completed","params":{"threadId":"codex-thread-1","turn":{"id":"codex-turn-2","status":"completed","error":null}}}'
while IFS= read -r line; do printf '%s\n' "$line" >> deliveries-resume.jsonl; done
"#;
        struct TwoPhaseFactory {
            first: Mutex<Option<CodexRuntimeFactory>>,
            second: CodexRuntimeFactory,
        }
        impl RuntimeFactory for TwoPhaseFactory {
            fn spawn(
                &self,
                task: &TaskRecord,
                sink: Arc<dyn LifecycleSink>,
            ) -> io::Result<Arc<dyn ManagedRuntime>> {
                let mut first = self.first.lock().unwrap();
                match first.take() {
                    Some(factory) => factory.spawn(task, sink),
                    None => self.second.spawn(task, sink),
                }
            }
        }
        let first = harness_factory(HAPPY_TURN, &directory);
        let second = {
            let child = directory.join("codex-resume.sh");
            std::fs::write(&child, format!("#!/bin/sh\n{resume_script}")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut mode = std::fs::metadata(&child).unwrap().permissions();
                mode.set_mode(0o755);
                std::fs::set_permissions(&child, mode).unwrap();
            }
            CodexRuntimeFactory::test_harness(Some(CodexLaunch::new(
                child,
                directory.join("codex-home"),
            )))
        };
        let store = Arc::new(external_store::Store::open(directory.join("state.sqlite")).unwrap());
        let scheduler = Scheduler::new(
            "codex-resume-test",
            store,
            Arc::new(TwoPhaseFactory {
                first: Mutex::new(Some(first)),
                second,
            }),
            SchedulerConfig {
                bootstrap_timeout: Duration::from_secs(30),
                control_timeout: Duration::from_secs(30),
                ..SchedulerConfig::default()
            },
        )
        .unwrap();
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for(&directory, "first turn"),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let agent_id = submitted.task.agent_id.clone();
        scheduler.start_ready().unwrap();
        let first_result = await_result(&scheduler, &agent_id);
        assert_eq!(first_result.result.final_text, "CODEX_OK");
        let deadline = Instant::now() + Duration::from_secs(5);
        while scheduler.active_count() > 0 {
            assert!(Instant::now() < deadline, "first runtime was never released");
            thread::sleep(Duration::from_millis(10));
        }

        // Public terminal send is the explicit recovery trigger.
        assert_eq!(
            scheduler
                .queue_message(&agent_id, "resume-msg-1", "follow-up question")
                .unwrap(),
            crate::MessageDisposition::Queued
        );
        // queue_message only requeues; the daemon claim loop performs the
        // spawn. Either this call or the finishing monitor's trailing claim
        // wins the single resume claim.
        scheduler.start_ready().unwrap();
        let resumed = await_result(&scheduler, &agent_id);
        assert_eq!(resumed.result.outcome, TaskOutcome::Completed);
        assert_eq!(resumed.result.final_text, "RESUMED_OK");

        let deliveries = std::fs::read_to_string(directory.join("deliveries-resume.jsonl"))
            .expect("the resumed process logs its own frames");
        let mut frames = deliveries
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap());
        let resume = frames
            .find(|frame| frame["method"] == "thread/resume")
            .expect("thread/resume frame");
        assert_eq!(resume["params"]["threadId"], THREAD_ID);
        assert_eq!(resume["params"]["excludeTurns"], true);
        let turn_starts = deliveries
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|frame| frame["method"] == "turn/start")
            .collect::<Vec<_>>();
        assert_eq!(turn_starts.len(), 1, "interrupted turn must not replay");
        assert!(turn_starts[0]["params"]["input"][0]["text"]
            .as_str()
            .unwrap()
            .contains("follow-up question"));
        let message = scheduler.store().message("resume-msg-1").unwrap().unwrap();
        assert_eq!(
            message.state,
            external_store::MessageState::Delivered,
            "resume message was not delivered"
        );
        assert_eq!(message.target_turn_id.as_deref(), Some("codex-turn-2"));
    }

    #[test]
    fn terminal_send_rejects_non_codex_cancelled_and_sessionless_tasks() {
        let _guard = scripted_test_guard();
        let workspace = codex_workspace();
        let scheduler = codex_scheduler(workspace.path(), harness_factory(HAPPY_TURN, workspace.path()));
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for(workspace.path(), "completed codex task"),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let agent_id = submitted.task.agent_id.clone();
        scheduler.start_ready().unwrap();
        await_terminal_task(&scheduler, &agent_id);

        // A zcode terminal task keeps the generic rejection.
        let zcode_task = scheduler
            .enqueue_general(&manifest_for(workspace.path(), "zcode terminal task"))
            .unwrap();
        let store = scheduler.store();
        let claim = store.claim_next("terminal-reject", usize::MAX, 1).unwrap().unwrap();
        assert_eq!(claim.task.agent_id, zcode_task.task.agent_id);
        store
            .mark_session_running(
                &claim.task.agent_id,
                claim.owner_epoch,
                "runtime",
                None,
                None,
                None,
            )
            .unwrap();
        store
            .store_task_result(
                &zcode_task.task.agent_id,
                &external_store::TaskResult {
                    outcome: TaskOutcome::Completed,
                    final_text: "done".into(),
                    partial: false,
                },
            )
            .unwrap();
        store
            .transition_terminal(
                &zcode_task.task.agent_id,
                claim.owner_epoch,
                &external_store::TerminalUpdate {
                    outcome: TaskOutcome::Completed,
                    failure_code: None,
                    failure_message: None,
                },
            )
            .unwrap();
        let error = scheduler
            .queue_message(&zcode_task.task.agent_id, "zcode-msg", "nope")
            .unwrap_err();
        assert!(error.to_string().contains("TERMINAL_SEND_UNSUPPORTED"));

        // A cancelled codex terminal task is rejected even with a thread id.
        let cancelled = scheduler
            .enqueue_general_with_admission(
                &manifest_for(workspace.path(), "cancelled codex task"),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let store = scheduler.store();
        let claim = store.claim_next("terminal-reject", usize::MAX, 1).unwrap().unwrap();
        assert_eq!(claim.task.agent_id, cancelled.task.agent_id);
        store
            .mark_session_running(
                &claim.task.agent_id,
                claim.owner_epoch,
                "runtime",
                None,
                Some(THREAD_ID),
                Some(external_store::TurnState::Idle),
            )
            .unwrap();
        store
            .request_stop(&cancelled.task.agent_id)
            .unwrap();
        store
            .transition_terminal(
                &cancelled.task.agent_id,
                claim.owner_epoch,
                &external_store::TerminalUpdate {
                    outcome: TaskOutcome::Cancelled,
                    failure_code: None,
                    failure_message: None,
                },
            )
            .unwrap();
        let error = scheduler
            .queue_message(&cancelled.task.agent_id, "cancel-msg", "nope")
            .unwrap_err();
        assert!(error.to_string().contains("TERMINAL_SEND_UNSUPPORTED"));

        // A codex terminal task without a persisted thread id is rejected.
        let sessionless = scheduler
            .enqueue_general_with_admission(
                &manifest_for(workspace.path(), "sessionless codex task"),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let store = scheduler.store();
        let claim = store.claim_next("terminal-reject", usize::MAX, 1).unwrap().unwrap();
        assert_eq!(claim.task.agent_id, sessionless.task.agent_id);
        store
            .mark_session_running(
                &claim.task.agent_id,
                claim.owner_epoch,
                "runtime",
                None,
                None,
                None,
            )
            .unwrap();
        store
            .store_task_result(
                &sessionless.task.agent_id,
                &external_store::TaskResult {
                    outcome: TaskOutcome::Failed,
                    final_text: "never started".into(),
                    partial: true,
                },
            )
            .unwrap();
        store
            .transition_terminal(
                &sessionless.task.agent_id,
                claim.owner_epoch,
                &external_store::TerminalUpdate {
                    outcome: TaskOutcome::Failed,
                    failure_code: Some("SESSION_START_FAILED".into()),
                    failure_message: None,
                },
            )
            .unwrap();
        let error = scheduler
            .queue_message(&sessionless.task.agent_id, "sessionless-msg", "nope")
            .unwrap_err();
        assert!(error.to_string().contains("TERMINAL_SEND_UNSUPPORTED"));
    }

    #[test]
    fn server_overloaded_and_mcp_startup_failures_stay_bounded_and_diagnostic() {
        let _guard = scripted_test_guard();
        let workspace = codex_workspace();
        let script = r#"
IFS= read -r line
printf '%s\n' '{"id":1,"result":{"codexHome":"/tmp/codex-home","userAgent":"fake"}}'
IFS= read -r line
printf '%s\n' '{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-5.6-terra"}}'
IFS= read -r line
IFS= read -r line
printf '%s\n' '{"id":3,"result":{"turn":{"id":"codex-turn-1","status":"inProgress"}}}' \
  '{"method":"turn/started","params":{"threadId":"codex-thread-1","turn":{"id":"codex-turn-1","status":"inProgress"}}}' \
  '{"method":"mcpServer/startupStatus/updated","params":{"threadId":"codex-thread-1","name":"cloudflare-api","status":"failed","error":"requires OAuth reauthentication","failureReason":"reauthenticationRequired"}}' \
  '{"method":"error","params":{"error":{"message":"Selected model is at capacity. Please try a different model.","codexErrorInfo":"serverOverloaded"},"willRetry":false,"threadId":"codex-thread-1","turnId":"codex-turn-1"}}' \
  '{"method":"turn/completed","params":{"threadId":"codex-thread-1","turn":{"id":"codex-turn-1","status":"failed","error":{"message":"Selected model is at capacity.","codexErrorInfo":"serverOverloaded"}}}}'
sleep 1
"#;
        let scheduler = codex_scheduler(workspace.path(), harness_factory(script, workspace.path()));
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for(workspace.path(), "overloaded probe"),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let agent_id = submitted.task.agent_id.clone();
        scheduler.start_ready().unwrap();
        let result = await_result(&scheduler, &agent_id);
        assert_eq!(result.result.outcome, TaskOutcome::Failed);
        let task = await_terminal_task(&scheduler, &agent_id);
        assert_eq!(task.zcode_session_id.as_deref(), Some(THREAD_ID));
        let record = scheduler
            .last_error(&agent_id)
            .expect("correlated failure record");
        assert!(record.contains("serverOverloaded"), "record: {record}");
        assert!(record.contains("cloudflare-api"), "record: {record}");
    }

    #[test]
    fn strict_envelope_missing_thread_and_model_mismatch_fail_the_start_bounded() {
        let _guard = scripted_test_guard();
        for (script, marker) in [
            // Malformed envelope: a jsonrpc frame is rejected by the strict
            // codec, then the child exits without a turn boundary.
            (
                r#"
IFS= read -r line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"codexHome":"/tmp"}}'
exit 7
"#,
                "SESSION_START_FAILED",
            ),
            // thread/start result without a thread id.
            (
                r#"
IFS= read -r line
printf '%s\n' '{"id":1,"result":{"codexHome":"/tmp/codex-home"}}'
IFS= read -r line
printf '%s\n' '{"id":2,"result":{"thread":{"ephemeral":false},"model":"gpt-5.6-terra"}}'
sleep 1
"#,
                "thread id",
            ),
            // Model mismatch between admission and the started thread.
            (
                r#"
IFS= read -r line
printf '%s\n' '{"id":1,"result":{"codexHome":"/tmp/codex-home"}}'
IFS= read -r line
printf '%s\n' '{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-other"}}'
sleep 1
"#,
                "MODEL_MISMATCH",
            ),
            // Auth failure arrives as an error response to initialize.
            (
                r#"
IFS= read -r line
printf '%s\n' '{"id":1,"error":{"code":401,"message":"not logged in"}}'
sleep 1
"#,
                "SESSION_START_FAILED",
            ),
            // EOF while a request is pending.
            (
                r#"
IFS= read -r line
exit 0
"#,
                "SESSION_START_FAILED",
            ),
        ] {
            let workspace = codex_workspace();
            let scheduler =
                codex_scheduler(workspace.path(), harness_factory(script, workspace.path()));
            let submitted = scheduler
                .enqueue_general_with_admission(
                    &manifest_for(workspace.path(), "bounded failure"),
                    Some(codex_admission(Some(MODEL))),
                )
                .unwrap();
            let agent_id = submitted.task.agent_id.clone();
            assert!(
                scheduler.start_ready().is_err(),
                "start must fail closed for {marker}"
            );
            let task = await_terminal_task(&scheduler, &agent_id);
            assert_eq!(task.outcome, Some(TaskOutcome::Failed));
            let record = scheduler
                .last_error(&agent_id)
                .expect("failure record is persisted");
            assert!(
                record.contains(marker) || marker == "SESSION_START_FAILED",
                "record for {marker}: {record}"
            );
            assert_eq!(task.zcode_session_id, None);
        }
    }

    #[test]
    fn interrupt_preserves_the_thread_identity_and_cancels_bounded() {
        let _guard = scripted_test_guard();
        let workspace = codex_workspace();
        let directory = workspace.path().to_owned();
        let script = format!(
            r#"
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{{"id":1,"result":{{"codexHome":"/tmp/codex-home"}}}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"{THREAD_ID}","ephemeral":false}},"model":"{MODEL}"}}}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"codex-turn-1","status":"inProgress"}}}}}}' \
  '{{"method":"turn/started","params":{{"threadId":"{THREAD_ID}","turn":{{"id":"codex-turn-1","status":"inProgress"}}}}}}'
while [ ! -f release ]; do sleep 0.01; done
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{{"id":4,"result":{{}}}}' '{{"method":"turn/completed","params":{{"threadId":"{THREAD_ID}","turn":{{"id":"codex-turn-1","status":"interrupted","error":null}}}}}}'
while IFS= read -r line; do printf '%s\n' "$line" >> deliveries.jsonl; done
"#
        );
        let scheduler = codex_scheduler(&directory, harness_factory(&script, &directory));
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for(&directory, "interrupt me"),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let agent_id = submitted.task.agent_id.clone();
        scheduler.start_ready().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
            if matches!(task.turn_state, external_store::TurnState::Active) {
                break;
            }
            assert!(Instant::now() < deadline, "turn never became active");
            thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(directory.join("release"), "").unwrap();
        scheduler.cancel_task(&agent_id).unwrap();
        let task = await_terminal_task(&scheduler, &agent_id);
        assert_eq!(task.outcome, Some(TaskOutcome::Cancelled));
        assert_eq!(task.zcode_session_id.as_deref(), Some(THREAD_ID));
        let deliveries = std::fs::read_to_string(directory.join("deliveries.jsonl")).unwrap();
        let interrupt = deliveries
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|frame| frame["method"] == "turn/interrupt")
            .expect("turn/interrupt frame");
        assert_eq!(interrupt["params"]["threadId"], THREAD_ID);
        assert_eq!(interrupt["params"]["turnId"], "codex-turn-1");
    }

    #[test]
    fn home_precedence_and_runtime_path_bounds() {
        // Configured home wins over the inherited environment.
        assert_eq!(
            resolve_codex_home(Some("/cfg/home"), Some("/inherited/home")),
            Some(Ok(PathBuf::from("/cfg/home")))
        );
        assert_eq!(
            resolve_codex_home(None, Some("/inherited/home")),
            Some(Ok(PathBuf::from("/inherited/home")))
        );
        assert_eq!(resolve_codex_home(Some("relative"), None), Some(Err("agents.codex.home must be absolute")));
        assert_eq!(resolve_codex_home(None, Some("relative")), Some(Err("inherited CODEX_HOME must be absolute")));
        // Neither source present rejects instead of falling back to ~/.codex.
        assert_eq!(resolve_codex_home(None, None), None);

        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("codex-runtime");
        std::fs::write(&runtime, b"runtime").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut mode = std::fs::metadata(&runtime).unwrap().permissions();
            mode.set_mode(0o755);
            std::fs::set_permissions(&runtime, mode).unwrap();
        }
        let launch = CodexLaunch::new(runtime.clone(), directory.path().join("home"));
        assert_eq!(launch.runtime_path(), runtime.as_path());
        assert_eq!(launch.home(), directory.path().join("home"));
        let script = directory.path().join("probe.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \"$CODEX_HOME $1 $2 $3\" > args.txt\nsleep 5\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut mode = std::fs::metadata(&script).unwrap().permissions();
            mode.set_mode(0o755);
            std::fs::set_permissions(&script, mode).unwrap();
        }
        let launch = CodexLaunch::new(script.clone(), directory.path().join("home"));
        let sink: Arc<dyn LifecycleSink> = Arc::new(NoopSink);
        let owner = CodexRuntimeOwner::spawn(launch.command(directory.path()), sink).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(args) = std::fs::read_to_string(directory.path().join("args.txt")) {
                assert_eq!(
                    args.trim(),
                    format!("{} app-server --listen stdio://", directory.path().join("home").display())
                );
                break;
            }
            assert!(Instant::now() < deadline, "child never recorded its argv");
            thread::sleep(Duration::from_millis(10));
        }
        let terminal = owner.stop(Duration::from_secs(2));
        assert!(terminal_proves_process_group_reaped(&terminal));

        // A non-executable or relative runtime path is rejected.
        let plain = directory.path().join("plain.txt");
        std::fs::write(&plain, b"plain").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut mode = std::fs::metadata(&plain).unwrap().permissions();
            mode.set_mode(0o644);
            std::fs::set_permissions(&plain, mode).unwrap();
        }
        let sink: Arc<dyn LifecycleSink> = Arc::new(NoopSink);
        let bad = CodexLaunch::new(plain, directory.path().join("home"));
        let error = CodexRuntimeFactory::test_harness(Some(bad))
            .spawn(&codex_task_record(directory.path()), sink)
            .err()
            .expect("non-executable runtime must refuse the spawn");
        assert!(error.to_string().contains("absolute executable file"));
    }

    fn codex_task_record(directory: &Path) -> TaskRecord {
        let canonical = directory.canonicalize().unwrap();
        let prepared = external_core::GeneralTaskPreparer::new(Vec::new())
            .unwrap()
            .prepare_direct_submission(&manifest_for(&canonical, "launch"))
            .unwrap()
            .with_admission(codex_admission(Some(MODEL)))
            .unwrap();
        TaskRecord {
            agent_id: "launch-check".into(),
            repository: canonical.to_string_lossy().into_owned(),
            phase: TaskPhase::Queued,
            outcome: None,
            workspace_path: canonical.to_string_lossy().into_owned(),
            runtime_hash: None,
            prepared_launch_json: serde_json::to_string(&prepared).unwrap(),
            prepared_launch_sha256: prepared.prepared_sha256.clone(),
            initial_prompt: "prompt".into(),
            owner_id: None,
            owner_epoch: 0,
            close_requested: false,
            stop_requested: false,
            last_event_seq: 0,
            failure_code: None,
            failure_message: None,
            runtime_agent_id: None,
            zcode_session_id: None,
            turn_state: external_store::TurnState::Idle,
            process_identity: None,
            closed_at: None,
            reaped_at: None,
            created_at: 0,
        }
    }

    #[test]
    fn routing_keeps_zcode_and_dsh_on_their_factories() {
        let workspace = codex_workspace();
        let zcode = crate::CommandRuntimeFactory::new(|_: &TaskRecord| {
            Err::<Command, _>(io::Error::other("zcode factory must not spawn"))
        });
        let factory = crate::dsh::RoutingRuntimeFactory::with_codex(
            zcode,
            crate::dsh::DshRuntimeFactory::closed(),
            CodexRuntimeFactory::closed(),
        );
        let sink: Arc<dyn LifecycleSink> = Arc::new(NoopSink);
        // No admission → legacy zcode route.
        let prepared = external_core::GeneralTaskPreparer::new(Vec::new())
            .unwrap()
            .prepare_direct_submission(&manifest_for(workspace.path(), "legacy"))
            .unwrap();
        let mut task = codex_task_record(workspace.path());
        task.prepared_launch_json = serde_json::to_string(&prepared).unwrap();
        task.prepared_launch_sha256 = prepared.prepared_sha256.clone();
        let error = factory
            .spawn(&task, Arc::clone(&sink))
            .err()
            .expect("legacy route must stay on the zcode factory");
        assert!(error.to_string().contains("zcode factory must not spawn"));
        // dsh admission → dsh factory (closed gate).
        let dsh_admission = AdmissionIdentity {
            agent: "dsh".into(),
            config_revision: 1,
            adapter_version: "test".into(),
            model: None,
            model_source: "native".into(),
        };
        let prepared = external_core::GeneralTaskPreparer::new(Vec::new())
            .unwrap()
            .prepare_direct_submission(&manifest_for(workspace.path(), "dsh route"))
            .unwrap()
            .with_admission(dsh_admission)
            .unwrap();
        let mut task = codex_task_record(workspace.path());
        task.prepared_launch_json = serde_json::to_string(&prepared).unwrap();
        task.prepared_launch_sha256 = prepared.prepared_sha256.clone();
        let error = factory
            .spawn(&task, Arc::clone(&sink))
            .err()
            .expect("dsh admission must stay on the dsh factory");
        assert!(error.to_string().contains("dsh spawn gate is closed"));
        // codex admission → codex factory (closed gate).
        let error = factory
            .spawn(&codex_task_record(workspace.path()), sink)
            .err()
            .expect("codex admission must route to the codex factory");
        assert!(error.to_string().contains("codex spawn gate is closed"));
    }
}
