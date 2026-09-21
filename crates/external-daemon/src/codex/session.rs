//! The Codex thread and turn control-plane glue: the `initialize` +
//! `initialized` handshake, the driver round-trips over the pure protocol
//! shapes in [`external_agent_codex::session`], and the fail-closed
//! `turn/start` reconciliation with the started notification.

use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use external_agent_codex::session::{
    self as wire, codex_posture, start_result_thread_id, thread_resume_params, thread_start_params,
    turn_start_params, turn_start_response_turn_id, validate_resume_result, CodexError,
    CodexPermissionMode,
};
use external_store::TaskRecord;

use super::owner::CodexRuntimeOwner;
use crate::{task_route, RuntimeCommandError};

/// The [`external_agent_codex::session::CodexError`] seam: every variant
/// maps onto the daemon's [`RuntimeCommandError`] one for one, with the
/// payload verbatim, so the crate's message strings are the wire oracle.
fn runtime_command_error(error: CodexError) -> RuntimeCommandError {
    match error {
        CodexError::Unsupported => RuntimeCommandError::Unsupported,
        CodexError::Timeout => RuntimeCommandError::Timeout,
        CodexError::Transport(message) => RuntimeCommandError::Transport(message),
        CodexError::Remote(value) => RuntimeCommandError::Remote(value),
        CodexError::InvalidSession(message) => RuntimeCommandError::InvalidSession(message),
    }
}

impl CodexRuntimeOwner {
    /// The `initialize` + `initialized` handshake every thread call requires.
    pub(super) fn initialize_before_threads(
        &self,
        deadline: Instant,
    ) -> Result<(), RuntimeCommandError> {
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
                RuntimeCommandError::InvalidSession("initialize result is missing codexHome".into())
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

    pub(super) fn admitted_thread(
        task: &TaskRecord,
    ) -> Result<AdmittedThread, RuntimeCommandError> {
        match task_route(task) {
            Ok(crate::TaskRoute::General(prepared)) => {
                let admission = prepared.admission.as_ref().ok_or_else(|| {
                    RuntimeCommandError::InvalidSession(
                        "codex task is missing its admission identity".into(),
                    )
                })?;
                if admission.agent != "codex" {
                    return Err(RuntimeCommandError::InvalidSession(
                        "runtime is not the admitted codex agent".into(),
                    ));
                }
                // The permission-mode seam: the daemon's four-value
                // external_core::PermissionMode narrows to the crate's
                // CodexPermissionMode here, at the admitted-thread
                // boundary; build and edit share the workspace-write
                // posture.
                let permission_mode = match prepared.permission_mode {
                    external_core::PermissionMode::Plan => CodexPermissionMode::Plan,
                    external_core::PermissionMode::Build | external_core::PermissionMode::Edit => {
                        CodexPermissionMode::WorkspaceWrite
                    }
                    external_core::PermissionMode::Yolo => CodexPermissionMode::Yolo,
                };
                Ok(AdmittedThread {
                    model: admission.model.clone().ok_or_else(|| {
                        RuntimeCommandError::InvalidSession(
                            "codex task is missing its admitted model".into(),
                        )
                    })?,
                    // The admitted effort lives at admission.effort
                    // (prepared_launch_json.admission.effort). Reading it
                    // from the prepared-launch top level instead would copy
                    // the known requested_model_from_prepared_launch defect
                    // (lib.rs) where the field is not actually persisted.
                    effort: admission.effort.clone(),
                    permission_mode,
                })
            }
            Err(message) => Err(RuntimeCommandError::InvalidSession(message)),
        }
    }

    pub(super) fn start_thread(
        &self,
        model: &str,
        workspace_path: &str,
        permission_mode: CodexPermissionMode,
        requested_effort: Option<&str>,
        deadline: Instant,
    ) -> Result<String, RuntimeCommandError> {
        let posture = codex_posture(permission_mode);
        let params = thread_start_params(model, workspace_path, &posture);
        let response = self
            .driver
            .request("thread/start", params, remaining_time(deadline)?)?;
        let result = response.result.ok_or_else(|| {
            RuntimeCommandError::InvalidSession("thread/start returned an error".into())
        })?;
        start_result_thread_id(
            &result,
            model,
            workspace_path,
            permission_mode,
            requested_effort,
        )
        .map_err(runtime_command_error)
    }

    pub(super) fn resume_thread(
        &self,
        thread_id: &str,
        model: &str,
        workspace_path: &str,
        permission_mode: CodexPermissionMode,
        requested_effort: Option<&str>,
        deadline: Instant,
    ) -> Result<(), RuntimeCommandError> {
        let params = thread_resume_params(thread_id);
        let response = self
            .driver
            .request("thread/resume", params, remaining_time(deadline)?)?;
        let result = response.result.ok_or_else(|| {
            RuntimeCommandError::InvalidSession("thread/resume returned an error".into())
        })?;
        validate_resume_result(
            &result,
            thread_id,
            model,
            workspace_path,
            permission_mode,
            requested_effort,
        )
        .map_err(runtime_command_error)
    }

    pub(super) fn start_turn(
        &self,
        thread_id: &str,
        model: &str,
        admitted_effort: Option<&str>,
        input: &str,
        deadline: Instant,
    ) -> Result<Option<String>, RuntimeCommandError> {
        let previous = self.shared.turn_tracker.snapshot().generation;
        let params = turn_start_params(thread_id, model, admitted_effort, input);
        // A started notification is attributable only while the start
        // request it answers is in flight; the flag closes on every exit so
        // a later replay cannot pose as the expected notification.
        self.shared.start_in_flight.store(true, Ordering::Release);
        let started = self.drive_start_turn(params, previous, deadline);
        self.shared.start_in_flight.store(false, Ordering::Release);
        started
    }

    fn drive_start_turn(
        &self,
        params: serde_json::Value,
        previous: u64,
        deadline: Instant,
    ) -> Result<Option<String>, RuntimeCommandError> {
        let response = self
            .driver
            .request("turn/start", params, remaining_time(deadline)?)?;
        let turn_id =
            turn_start_response_turn_id(response.result.as_ref()).map_err(runtime_command_error)?;
        self.shared
            .turn_tracker
            .wait_started_after(previous, remaining_time(deadline)?)?;
        // The turn the provider started must be the turn it acknowledged in
        // the start response; a mismatch means stale or foreign traffic won
        // the boundary race and the start fails closed instead of adopting
        // an unverified turn.
        let started = self.shared.current_turn.lock().unwrap().clone();
        if started.as_deref() != Some(turn_id.as_str()) {
            return Err(RuntimeCommandError::InvalidSession(
                "turn/start response turn id does not match the started turn".into(),
            ));
        }
        Ok(Some(turn_id))
    }
}

/// The admitted Codex thread identity: the model and optional reasoning
/// effort every turn must name, and the permission mode whose posture pins
/// the thread launch and every later resume.
pub(super) struct AdmittedThread {
    pub(super) model: String,
    pub(super) effort: Option<String>,
    pub(super) permission_mode: CodexPermissionMode,
}

pub(super) fn remaining_time(deadline: Instant) -> Result<Duration, RuntimeCommandError> {
    wire::remaining_time(deadline).map_err(runtime_command_error)
}
