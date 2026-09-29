//! The `agy` runtime owner: driver spawn with the NDJSON codec, the
//! `ManagedRuntime` trait surface (bootstrap on `init`, stdin `user` turns,
//! process-group termination for cancellation, stop/reap), and process
//! teardown.

use std::{
    io,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use external_runtime::{Driver, FrameCodec};
use external_store::TaskRecord;

use super::events::AgyRuntimeShared;
use super::transport::spawn_agy_pump;
use crate::{
    LifecycleSink, ManagedRuntime, ProcessIdentity, Publisher, RuntimeCommandError, RuntimeLoss,
    RuntimeTerminal, SessionReady, TurnBoundary, TurnSnapshot, TurnTracker,
};

pub struct AgyRuntimeOwner {
    pub(super) driver: Arc<Driver>,
    pub(super) shared: Arc<AgyRuntimeShared>,
    shutdown: Arc<AtomicBool>,
}

impl AgyRuntimeOwner {
    pub fn spawn(command: Command, sink: Arc<dyn LifecycleSink>) -> io::Result<Self> {
        let driver = Arc::new(Driver::spawn_with_codec(command, FrameCodec::Ndjson)?);
        let publisher = Arc::new(Publisher::new(sink));
        let shared = Arc::new(AgyRuntimeShared::new(
            Arc::clone(&publisher),
            Arc::new(TurnTracker::new()),
        ));
        let shutdown = Arc::new(AtomicBool::new(false));
        spawn_agy_pump(
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
        if self.shared.session_id().as_deref() == Some(session_id) {
            Ok(())
        } else {
            Err(RuntimeCommandError::InvalidSession(
                "session id does not belong to this runtime".into(),
            ))
        }
    }

    /// Write one NDJSON `user` event to the child's stdin.
    fn send_user_line(&self, content: &str) -> Result<(), RuntimeCommandError> {
        let event = serde_json::json!({
            "event": "user",
            "message": {"content": content},
        });
        self.driver
            .send(&event)
            .map_err(|error| RuntimeCommandError::Transport(error.to_string()))
    }

    /// The admitted `agy` identity: an `agy` agent and its optional admitted
    /// model. A missing/empty model is the `native` fallback (the child was
    /// launched without `--model`), so it is reported as `None` rather than
    /// refused.
    fn admitted_model(task: &TaskRecord) -> Result<Option<String>, RuntimeCommandError> {
        match crate::task_route(task) {
            Ok(crate::TaskRoute::General(prepared)) => {
                let admission = prepared.admission.as_ref().ok_or_else(|| {
                    RuntimeCommandError::InvalidSession(
                        "agy task is missing its admission identity".into(),
                    )
                })?;
                if admission.agent != "agy" {
                    return Err(RuntimeCommandError::InvalidSession(
                        "runtime is not the admitted agy agent".into(),
                    ));
                }
                Ok(admission.model.clone().filter(|model| !model.is_empty()))
            }
            Err(message) => Err(RuntimeCommandError::InvalidSession(message)),
        }
    }

    /// See [`crate::RuntimeOwner`]: never short-circuited by a terminal a late
    /// child-exit boundary published, so the real stop/reap is always tried.
    fn cleanup_for_forced_failure(&self, grace: Duration) -> RuntimeTerminal {
        let terminal = crate::cleanup_owned_group(&self.driver, grace);
        self.shared
            .publisher
            .publish_cleanup_terminal(terminal.clone());
        terminal
    }
}

impl Drop for AgyRuntimeOwner {
    fn drop(&mut self) {
        let _ = self.stop(Duration::from_secs(1));
        self.shutdown.store(true, Ordering::Release);
    }
}

impl ManagedRuntime for AgyRuntimeOwner {
    fn identity(&self) -> Option<ProcessIdentity> {
        Some(self.driver.identity())
    }

    fn stop(&self, grace: Duration) -> RuntimeTerminal {
        self.finish_process(grace, None)
    }

    fn cleanup_for_forced_failure(&self, grace: Duration) -> RuntimeTerminal {
        self.cleanup_for_forced_failure(grace)
    }

    fn terminal_latch(&self) -> Option<crate::TerminalLatch<'_>> {
        Some(self.shared.publisher.decision_latch())
    }

    fn wait_terminal(&self, timeout: Duration) -> Option<RuntimeTerminal> {
        self.shared.publisher.wait_terminal(timeout)
    }

    fn diagnostic_tail(&self) -> String {
        let stderr = self.driver.diagnostic_tail();
        let agy = self.shared.diagnostic_tail();
        if agy.is_empty() {
            stderr
        } else {
            format!("{stderr}\n{agy}")
        }
    }

    fn diagnostic_session_id(&self) -> Option<String> {
        self.shared.session_id()
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
        // Open the turn before the user line leaves, so the pump can never
        // observe a result without a turn to settle.
        self.shared.begin_turn();
        self.send_user_line(&task.initial_prompt)?;
        let session_id = self.shared.wait_start(remaining_time(deadline)?)?;
        Ok(SessionReady {
            session_id,
            initial_turn_id: None,
            configured_model: model,
        })
    }

    fn send_turn(
        &self,
        session_id: &str,
        content: &str,
        _timeout: Duration,
    ) -> Result<Option<String>, RuntimeCommandError> {
        self.validate_session(session_id)?;
        if self.shared.turn_tracker.snapshot().active {
            return Err(RuntimeCommandError::InvalidSession(
                "a turn is already in flight for this agy session".into(),
            ));
        }
        self.shared.begin_turn();
        self.send_user_line(content)?;
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
        // `agy` has no cooperative cancel: the process group is terminated and
        // the measured `interrupted` result settles the turn. Take ownership
        // of the stop first so the pump's child-exit classification cannot
        // publish a terminal ahead of the scheduler's real reap.
        if self.shared.publisher.begin_stopping().is_some() {
            return Ok(self.shared.turn_tracker.snapshot());
        }
        self.driver
            .stop_and_reap(remaining_time(deadline)?)
            .map_err(|error| RuntimeCommandError::Transport(error.to_string()))?;
        let boundary = self
            .shared
            .turn_tracker
            .wait_boundary_after(current.generation, remaining_time(deadline)?)?;
        self.shared.stop_boundaries.fetch_add(1, Ordering::AcqRel);
        Ok(boundary)
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

fn remaining_time(deadline: Instant) -> Result<Duration, RuntimeCommandError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(RuntimeCommandError::Timeout)
}
