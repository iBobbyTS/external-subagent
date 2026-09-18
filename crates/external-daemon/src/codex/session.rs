//! The Codex thread and turn control plane: the `initialize` handshake,
//! persistent `thread/start`/`thread/resume` identity, and fail-closed
//! `turn/start` reconciliation with the started notification.

use std::{
    path::Path,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use external_store::TaskRecord;

use super::owner::CodexRuntimeOwner;
use crate::{task_route, RuntimeCommandError};

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

    pub(super) fn admitted_model(task: &TaskRecord) -> Result<String, RuntimeCommandError> {
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

    pub(super) fn start_thread(
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

    pub(super) fn resume_thread(
        &self,
        thread_id: &str,
        model: &str,
        workspace_path: &str,
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
        if result
            .pointer("/thread/id")
            .and_then(|value| value.as_str())
            != Some(thread_id)
        {
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
        // A resumed thread runs on a fresh process, so the plan-only
        // posture must be re-confirmed from the resume result before any
        // turn is trusted: read-only sandbox, never-approve policy, and the
        // persisted task workspace. An unconfirmed or divergent posture
        // fails closed instead of resuming with write capability.
        //
        // The live probe resolves the resumed sandbox as the object
        // `{"type":"readOnly","networkAccess":false}`; only that exact
        // read-only posture is accepted. Request-time string presets, any
        // write mode, an unknown representation, or a network-capable
        // sandbox are all unconfirmed and fail closed.
        let sandbox_confirmed = resume_thread_field(&result, "sandbox").is_some_and(|value| {
            value.get("type").and_then(|mode| mode.as_str()) == Some("readOnly")
                && value
                    .get("networkAccess")
                    .and_then(|access| access.as_bool())
                    == Some(false)
        });
        if !sandbox_confirmed {
            return Err(RuntimeCommandError::InvalidSession(
                "resume sandbox was not confirmed as the read-only object".into(),
            ));
        }
        if resume_thread_field(&result, "approvalPolicy").and_then(|value| value.as_str())
            != Some("never")
        {
            return Err(RuntimeCommandError::InvalidSession(
                "resume approvalPolicy was not confirmed as never".into(),
            ));
        }
        let cwd = resume_thread_field(&result, "cwd").and_then(|value| value.as_str());
        if !cwd.is_some_and(|cwd| Path::new(cwd) == Path::new(workspace_path)) {
            return Err(RuntimeCommandError::InvalidSession(
                "resume cwd does not match the task workspace".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn start_turn(
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
        // The response must name the turn it started: a missing or unbounded
        // id cannot be reconciled with the started notification, so the
        // start fails closed instead of adopting an unverified turn.
        let Some(turn_id) = response
            .result
            .as_ref()
            .and_then(|result| result.pointer("/turn/id"))
            .and_then(|value| value.as_str())
            .filter(|id| !id.is_empty() && id.len() <= 512)
            .map(str::to_owned)
        else {
            return Err(RuntimeCommandError::InvalidSession(
                "turn/start response is missing a bounded turn id".into(),
            ));
        };
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

fn validate_thread_model(
    requested: &str,
    observed: Option<&serde_json::Value>,
) -> Result<(), RuntimeCommandError> {
    let observed = observed.and_then(|value| value.as_str()).ok_or_else(|| {
        RuntimeCommandError::InvalidSession("MODEL_NOT_OBSERVED: thread model is missing".into())
    })?;
    if observed != requested {
        return Err(RuntimeCommandError::InvalidSession(
            "MODEL_MISMATCH: thread model differs from the admitted request".into(),
        ));
    }
    Ok(())
}

/// A plan-only posture field of the resumed thread, accepted from the
/// thread object or the result root, mirroring how the resume result
/// carries the model.
fn resume_thread_field<'a>(
    result: &'a serde_json::Value,
    key: &str,
) -> Option<&'a serde_json::Value> {
    result
        .pointer(&format!("/thread/{key}"))
        .or_else(|| result.get(key))
}

pub(super) fn remaining_time(deadline: Instant) -> Result<Duration, RuntimeCommandError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(RuntimeCommandError::Timeout)
}
