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
use external_contract::ResponseEnvelope;
use external_store::TaskRecord;

use super::owner::CodexRuntimeOwner;
use crate::{task_route, RuntimeCommandDetail, RuntimeCommandError};

/// The [`external_agent_codex::session::CodexError`] seam: every variant
/// maps onto the daemon's [`RuntimeCommandError`] one for one, with the
/// payload verbatim, so the crate's message strings are the wire oracle.
fn runtime_command_error(error: CodexError) -> RuntimeCommandError {
    match error {
        CodexError::Unsupported => RuntimeCommandError::Unsupported,
        CodexError::Timeout => RuntimeCommandError::Timeout,
        CodexError::Transport(message) => RuntimeCommandError::Transport(message),
        CodexError::Remote(value) => RuntimeCommandError::Remote(value),
        CodexError::InvalidSession(message) => RuntimeCommandError::InvalidSession(message.into()),
    }
}

/// A rejected command keeps its classification message while the complete
/// native error response travels beside it. Remote errors already carry the
/// original; a local validation error has no native cause of its own.
fn rejected_command(message: &str, response: &ResponseEnvelope) -> RuntimeCommandError {
    match response.error.as_ref() {
        Some(original) => RuntimeCommandError::InvalidSession(RuntimeCommandDetail::from_native(
            message.to_owned(),
            original,
        )),
        None => RuntimeCommandError::InvalidSession(message.into()),
    }
}

/// Attach the response's native error to a validation failure that lost the
/// original while inspecting an absent/malformed result.
fn response_error(error: CodexError, response: &ResponseEnvelope) -> RuntimeCommandError {
    let mapped = runtime_command_error(error);
    let (RuntimeCommandError::InvalidSession(detail), Some(original)) =
        (&mapped, response.error.as_ref())
    else {
        return mapped;
    };
    if detail.native_failure.is_some() {
        return mapped;
    }
    RuntimeCommandError::InvalidSession(RuntimeCommandDetail::from_native(
        detail.message.clone(),
        original,
    ))
}

impl CodexRuntimeOwner {
    /// The `initialize` + `initialized` handshake every thread call requires.
    pub(super) fn initialize_before_threads(
        &self,
        deadline: Instant,
    ) -> Result<(), RuntimeCommandError> {
        let response = self.driver.request(
            "initialize",
            wire::initialize_params(env!("CARGO_PKG_VERSION")),
            remaining_time(deadline)?,
        )?;
        // The shared handshake shapes live in the protocol crate so the
        // task-session and catalog-probe paths cannot drift apart.
        wire::validate_initialize_codex_home(response.result.as_ref())
            .map_err(|error| response_error(error, &response))?;
        self.driver
            .send(&wire::initialized_notification())
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
                    // the retired legacy model read (lib.rs) where the field
                    // is not actually persisted.
                    effort: admission.effort.clone(),
                    developer_instructions: admission.developer_instructions.clone(),
                    permission_mode,
                })
            }
            Err(message) => Err(RuntimeCommandError::InvalidSession(message.into())),
        }
    }

    pub(super) fn start_thread(
        &self,
        model: &str,
        workspace_path: &str,
        permission_mode: CodexPermissionMode,
        requested_effort: Option<&str>,
        developer_instructions: Option<&str>,
        deadline: Instant,
    ) -> Result<String, RuntimeCommandError> {
        let posture = codex_posture(permission_mode);
        let params = thread_start_params(model, workspace_path, &posture, developer_instructions);
        let response = self
            .driver
            .request("thread/start", params, remaining_time(deadline)?)?;
        let Some(result) = response.result.clone() else {
            return Err(rejected_command(
                "thread/start returned an error",
                &response,
            ));
        };
        start_result_thread_id(
            &result,
            model,
            workspace_path,
            permission_mode,
            requested_effort,
        )
        .map_err(|error| response_error(error, &response))
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
        let posture = codex_posture(permission_mode);
        let params = thread_resume_params(thread_id, &posture);
        let response = self
            .driver
            .request("thread/resume", params, remaining_time(deadline)?)?;
        let Some(result) = response.result.clone() else {
            return Err(rejected_command(
                "thread/resume returned an error",
                &response,
            ));
        };
        validate_resume_result(
            &result,
            thread_id,
            model,
            workspace_path,
            permission_mode,
            requested_effort,
        )
        .map_err(|error| response_error(error, &response))
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

    /// 实测并发 turn/start 回显当前 turn；此路径不打开 started 归因门，
    /// 也不改 tracker 或 retirement。竞态开出新 turn 时必须拒绝该回显。
    pub(super) fn inject_active_turn(
        &self,
        thread_id: &str,
        current_turn: &str,
        model: &str,
        effort: Option<&str>,
        content: &str,
        deadline: Instant,
    ) -> Result<Option<String>, RuntimeCommandError> {
        let response = self.driver.request(
            "turn/start",
            turn_start_params(thread_id, model, effort, content),
            remaining_time(deadline)?,
        )?;
        let echoed = turn_start_response_turn_id(response.result.as_ref())
            .map_err(|error| response_error(error, &response))?;
        if echoed != current_turn {
            return Err(RuntimeCommandError::InvalidSession(
                "inject turn/start response does not match current turn".into(),
            ));
        }
        Ok(None)
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
        let turn_id = turn_start_response_turn_id(response.result.as_ref())
            .map_err(|error| response_error(error, &response))?;
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
    pub(super) developer_instructions: Option<String>,
    pub(super) permission_mode: CodexPermissionMode,
}

pub(super) fn remaining_time(deadline: Instant) -> Result<Duration, RuntimeCommandError> {
    wire::remaining_time(deadline).map_err(runtime_command_error)
}

#[cfg(test)]
mod native_command_failure_tests {
    use super::*;
    use external_contract::WireId;
    use serde_json::json;

    #[test]
    fn rejected_commands_keep_the_complete_native_error_beside_the_classification() {
        let response = ResponseEnvelope {
            id: WireId::Integer(2),
            result: None,
            error: Some(json!({
                "code": -32602,
                "message": "model not found",
                "data": {"detail": "complete native detail"},
            })),
        };
        let error = rejected_command("thread/start returned an error", &response);
        // The classification message is unchanged; the original rides beside it.
        assert_eq!(
            error.to_string(),
            "invalid session response: thread/start returned an error"
        );
        let failure = error.native_failure().expect("native failure");
        assert_eq!(failure.original, response.error.clone().unwrap());
    }

    #[test]
    fn validation_failures_attach_the_response_error_only_when_one_exists() {
        let malformed = ResponseEnvelope {
            id: WireId::Integer(3),
            result: Some(json!({"turn": {"status": "inProgress"}})),
            error: None,
        };
        let error = response_error(
            CodexError::InvalidSession("missing a bounded turn id".into()),
            &malformed,
        );
        assert!(error.native_failure().is_none());
        assert_eq!(
            error.to_string(),
            "invalid session response: missing a bounded turn id"
        );

        let rejected = ResponseEnvelope {
            id: WireId::Integer(4),
            result: None,
            error: Some(json!({"message": "rejected", "data": {"retry": true}})),
        };
        let error = response_error(
            CodexError::InvalidSession("missing a bounded turn id".into()),
            &rejected,
        );
        assert_eq!(
            error.native_failure().unwrap().original,
            rejected.error.clone().unwrap()
        );
    }
}
