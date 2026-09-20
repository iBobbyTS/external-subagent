//! The Codex runtime owner: driver spawn, the `ManagedRuntime` trait
//! surface (bootstrap, resume, turns, cancellation, stop/reap), and the
//! process teardown that preserves turn-boundary evidence.

use std::{
    collections::HashMap,
    io,
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use external_runtime::{Driver, FrameCodec};
use external_store::TaskRecord;

use super::events::CodexShared;
use super::session::remaining_time;
use super::transport::spawn_codex_pump;
use crate::{
    LifecycleSink, ManagedRuntime, ProcessIdentity, Publisher, RuntimeCommandError, RuntimeLoss,
    RuntimeTerminal, SessionReady, TurnBoundary, TurnSnapshot, TurnTracker,
};

pub struct CodexRuntimeOwner {
    pub(super) driver: Arc<Driver>,
    pub(super) shared: Arc<CodexShared>,
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
            admitted_effort: Mutex::new(None),
            diagnostic_session_id: Mutex::new(None),
            current_turn: Mutex::new(None),
            retired_turns: Mutex::new(Vec::new()),
            start_in_flight: AtomicBool::new(false),
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
        let admitted = Self::admitted_thread(task)?;
        self.initialize_before_threads(deadline)?;
        let thread_id = self.start_thread(
            &admitted.model,
            &task.workspace_path,
            admitted.permission_mode,
            admitted.effort.as_deref(),
            deadline,
        )?;
        *self.shared.session_id.lock().unwrap() = Some(thread_id.clone());
        *self.shared.admitted_model.lock().unwrap() = Some(admitted.model.clone());
        // The admitted effort is stored beside the model so every follow-up
        // turn keeps naming it instead of falling back to the low default.
        *self.shared.admitted_effort.lock().unwrap() = admitted.effort.clone();
        *self.shared.diagnostic_session_id.lock().unwrap() = Some(thread_id.clone());
        let prompt = task.initial_prompt.clone();
        let initial_turn_id = self.start_turn(
            &thread_id,
            &admitted.model,
            admitted.effort.as_deref(),
            &prompt,
            deadline,
        )?;
        Ok(SessionReady {
            session_id: thread_id,
            initial_turn_id,
            configured_model: Some(admitted.model),
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
        let thread_id = task
            .session_id
            .as_deref()
            .filter(|id| !id.is_empty() && id.len() <= 512)
            .ok_or_else(|| {
                RuntimeCommandError::InvalidSession("task has no persisted session id".into())
            })?;
        let admitted = Self::admitted_thread(task)?;
        *self.shared.diagnostic_session_id.lock().unwrap() = Some(thread_id.to_owned());
        self.initialize_before_threads(deadline)?;
        self.resume_thread(
            thread_id,
            &admitted.model,
            &task.workspace_path,
            admitted.permission_mode,
            admitted.effort.as_deref(),
            deadline,
        )?;
        *self.shared.session_id.lock().unwrap() = Some(thread_id.to_owned());
        *self.shared.admitted_model.lock().unwrap() = Some(admitted.model.clone());
        // A resumed process must keep admitting the same effort, or the
        // follow-up turn below would silently drop back to low.
        *self.shared.admitted_effort.lock().unwrap() = admitted.effort.clone();
        // A resumed thread never replays the interrupted pre-crash turn: the
        // queued message below is the sole trigger for the next turn.
        Ok(SessionReady {
            session_id: thread_id.to_owned(),
            initial_turn_id: None,
            configured_model: Some(admitted.model),
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
        let admitted_model = self
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
        // The follow-up effort is stored at bootstrap/resume; a None keeps
        // the historical low default rather than re-deriving anything.
        let admitted_effort = self.shared.admitted_effort.lock().unwrap().clone();
        self.start_turn(
            session_id,
            &admitted_model,
            admitted_effort.as_deref(),
            content,
            deadline,
        )
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
