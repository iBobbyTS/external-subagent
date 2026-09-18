//! The DSH runtime owner: driver spawn with the strict-plan patch lifetime,
//! the `ManagedRuntime` trait surface (bootstrap, turns, cancellation,
//! single-shot permission answers, stop/reap), and process teardown.

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

use external_agent_dsh::acp::{
    permission::{self, OfferCache},
    result::MessageAggregation,
    session::AcpSession,
};
use external_contract::WireId;
use external_runtime::{Driver, FrameCodec};
use external_store::TaskRecord;

use super::events::DshRuntimeShared;
use super::session::{dsh_session_message, remaining_time};
use super::transport::spawn_dsh_pump;
use crate::{
    LifecycleSink, ManagedRuntime, ProcessIdentity, Publisher, RuntimeCommandError, RuntimeLoss,
    RuntimeTerminal, SessionReady, TurnBoundary, TurnSnapshot, TurnTracker,
};

pub struct DshRuntimeOwner {
    driver: Arc<Driver>,
    pub(super) shared: Arc<DshRuntimeShared>,
    pub(super) session: Mutex<AcpSession>,
    shutdown: Arc<AtomicBool>,
    _patch_directory: Option<tempfile::TempDir>,
}

impl DshRuntimeOwner {
    pub fn spawn(command: Command, sink: Arc<dyn LifecycleSink>) -> io::Result<Self> {
        Self::spawn_with_patch(command, sink, None)
    }

    pub(super) fn spawn_with_patch(
        command: Command,
        sink: Arc<dyn LifecycleSink>,
        patch_directory: Option<tempfile::TempDir>,
    ) -> io::Result<Self> {
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
            _patch_directory: patch_directory,
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
        content: Option<&str>,
        _validated_denial: Option<&external_core::ValidatedPermissionDenial>,
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
