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
                codex_posture(prepared.permission_mode)?;
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
                    permission_mode: prepared.permission_mode,
                })
            }
            Err(message) => Err(RuntimeCommandError::InvalidSession(message)),
        }
    }

    pub(super) fn start_thread(
        &self,
        model: &str,
        workspace_path: &str,
        permission_mode: external_core::PermissionMode,
        requested_effort: Option<&str>,
        deadline: Instant,
    ) -> Result<String, RuntimeCommandError> {
        let posture = codex_posture(permission_mode)?;
        let params = serde_json::json!({
            "model": model,
            "cwd": workspace_path,
            "approvalPolicy": "never",
            "sandbox": posture.sandbox,
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
        // The start result echoes the posture the server actually applied —
        // verified on codex-cli 0.154.0 for BOTH admitted presets: plan
        // resolves `{"type":"readOnly","networkAccess":false}`, yolo
        // `{"type":"dangerFullAccess"}`, each with `approvalPolicy` and
        // `cwd` at the result root (see docs/compatibility/codex.md).
        // Request params alone are therefore no more trusted for the first
        // executable turn than they are on resume: an unconfirmed or
        // divergent posture fails closed before any `turn/start` is sent.
        if thread_result_field(&result, "approvalPolicy").and_then(|value| value.as_str())
            != Some("never")
        {
            return Err(RuntimeCommandError::InvalidSession(
                "start approvalPolicy was not confirmed as never".into(),
            ));
        }
        let confirmed = thread_result_field(&result, "sandbox")
            .is_some_and(|value| sandbox_confirmed(value, permission_mode));
        if !confirmed {
            return Err(RuntimeCommandError::InvalidSession(format!(
                "start sandbox was not confirmed as the {}",
                posture.label
            )));
        }
        let cwd = thread_result_field(&result, "cwd").and_then(|value| value.as_str());
        if !cwd.is_some_and(|cwd| Path::new(cwd) == Path::new(workspace_path)) {
            return Err(RuntimeCommandError::InvalidSession(
                "start cwd does not match the task workspace".into(),
            ));
        }
        // The start-side reasoningEffort echo is diagnostic-only: OBSERVED on
        // codex-cli 0.154.0, a thread/start that carries no effort echoes
        // the model default (gpt-5.6-terra: reasoningEffort="medium",
        // defaultReasoningEffort="medium" — .agent-work/tmp/codex-app-server-
        // probe/result-20260915-persistent.json), not an acknowledgement of
        // the admitted selection, so comparing them would fail healthy
        // starts. The turn itself still names the admitted effort.
        note_start_effort_echo(requested_effort, &result);
        Ok(thread_id.to_owned())
    }

    pub(super) fn resume_thread(
        &self,
        thread_id: &str,
        model: &str,
        workspace_path: &str,
        permission_mode: external_core::PermissionMode,
        requested_effort: Option<&str>,
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
        // A resumed thread runs on a fresh process, so the admitted posture
        // must be re-confirmed from the resume result before any turn is
        // trusted — the same confirmation start_thread applies to the first
        // launch: never-approve policy plus the sandbox matching the
        // admitted permission mode. An unconfirmed or divergent posture
        // fails closed instead of resuming with a different capability.
        //
        // The live probes resolve the resumed sandbox as an object
        // (plan: `{"type":"readOnly","networkAccess":false}`; a yolo
        // thread on codex-cli 0.154.0 resumes as the narrowed
        // workspace-write reconstruction of its persisted
        // danger-full-access rollout — the faithful
        // `{"type":"dangerFullAccess"}` shape is accepted too).
        // Request-time string presets, any posture belonging to another
        // permission mode, an unknown representation, or a
        // network-capable sandbox are all unconfirmed and fail closed.
        let posture = codex_posture(permission_mode)?;
        let confirmed = thread_result_field(&result, "sandbox")
            .is_some_and(|value| sandbox_confirmed(value, permission_mode));
        if !confirmed {
            return Err(RuntimeCommandError::InvalidSession(format!(
                "resume sandbox was not confirmed as the {}",
                posture.label
            )));
        }
        if thread_result_field(&result, "approvalPolicy").and_then(|value| value.as_str())
            != Some("never")
        {
            return Err(RuntimeCommandError::InvalidSession(
                "resume approvalPolicy was not confirmed as never".into(),
            ));
        }
        let cwd = thread_result_field(&result, "cwd").and_then(|value| value.as_str());
        if !cwd.is_some_and(|cwd| Path::new(cwd) == Path::new(workspace_path)) {
            return Err(RuntimeCommandError::InvalidSession(
                "resume cwd does not match the task workspace".into(),
            ));
        }
        confirm_resumed_thread_effort(requested_effort, &result)?;
        Ok(())
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
        // A task without an admitted effort keeps the historical default:
        // its turn/start wire stays byte-identical with `"effort":"low"`.
        let params = serde_json::json!({
            "threadId": thread_id,
            "model": model,
            "effort": admitted_effort.unwrap_or("low"),
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

/// Record the thread/start `reasoningEffort` echo as a diagnostic only.
/// A fresh thread has no prior turn, so the echo is the model's default
/// effort rather than an acknowledgement of the admitted selection (OBSERVED
/// on codex-cli 0.154.0: a start without an effort echoes "medium" for
/// gpt-5.6-terra). Only a task that explicitly selected an effort is worth
/// a note; an omitted effort keeps its low default silently.
///
/// Production code in this crate historically had no eprintln sites (the
/// rpc/config.rs one is `cfg(test)`); these diagnostics go to stderr, which
/// launchd captures into logs/daemon-error.log, and deliberately do not
/// open a new observable field.
fn note_start_effort_echo(requested_effort: Option<&str>, result: &serde_json::Value) {
    let Some(requested_effort) = requested_effort else {
        return;
    };
    if let Some(echo) = thread_result_field(result, "reasoningEffort") {
        eprintln!(
            "codex thread/start echoed reasoningEffort {} (the model default, not the admitted effort); turn/start still names the admitted {requested_effort}",
            echo.as_str().unwrap_or("<non-string>")
        );
    }
}

/// Confirm the thread/resume-result `reasoningEffort` echo against the
/// admitted effort. Unlike a fresh start, a resumed thread's echo reflects
/// the effort its previous turn actually ran with (OBSERVED on codex-cli
/// 0.154.0: a persistent thread resumed after a low turn echoes "low"), so
/// a task that explicitly admitted an effort must see that effort echoed:
/// a divergent echo fails closed before any follow-up `turn/start` is sent,
/// while a missing echo (e.g. a thread resumed before any turn) is never
/// compared against anything and only reaches the daemon diagnostic log.
fn confirm_resumed_thread_effort(
    requested_effort: Option<&str>,
    result: &serde_json::Value,
) -> Result<(), RuntimeCommandError> {
    let Some(requested_effort) = requested_effort else {
        return Ok(());
    };
    match thread_result_field(result, "reasoningEffort") {
        Some(echo) => {
            if echo.as_str() != Some(requested_effort) {
                return Err(RuntimeCommandError::InvalidSession(format!(
                    "resume reasoningEffort was not confirmed as the admitted {requested_effort}"
                )));
            }
        }
        None => {
            // No echo to compare: record the unconfirmed admission as a
            // stderr diagnostic (launchd captures it into
            // logs/daemon-error.log) instead of fabricating a confirmation
            // or opening a new observable field.
            eprintln!(
                "codex thread/resume result carried no reasoningEffort echo; the admitted effort {requested_effort} stays unconfirmed"
            );
        }
    }
    Ok(())
}

/// The admitted Codex thread identity: the model and optional reasoning
/// effort every turn must name, and the permission mode whose posture pins
/// the thread launch and every later resume.
pub(super) struct AdmittedThread {
    pub(super) model: String,
    pub(super) effort: Option<String>,
    pub(super) permission_mode: external_core::PermissionMode,
}

/// The pinned Codex thread posture for one admitted permission mode. Both
/// admitted modes pin `approvalPolicy=never`: plan runs the read-only
/// sandbox, yolo runs `danger-full-access` with no codex-side confinement.
pub(super) struct CodexPosture {
    pub(super) sandbox: &'static str,
    pub(super) label: &'static str,
}

pub(super) fn codex_posture(
    mode: external_core::PermissionMode,
) -> Result<CodexPosture, RuntimeCommandError> {
    match mode {
        external_core::PermissionMode::Plan => Ok(CodexPosture {
            sandbox: "read-only",
            label: "read-only object",
        }),
        external_core::PermissionMode::Yolo => Ok(CodexPosture {
            sandbox: "danger-full-access",
            label: "danger-full-access posture",
        }),
        _ => Err(RuntimeCommandError::InvalidSession(
            "codex runtime supports only the plan and yolo permission modes".into(),
        )),
    }
}

/// Confirm a thread-result sandbox against the admitted permission mode,
/// for the first launch and every resume alike. Plan threads must resolve
/// the read-only object. A yolo thread must resolve the faithful
/// `{"type":"dangerFullAccess"}` object — the shape a real 0.154.0 start
/// echoes — or the exact narrowed workspace-write reconstruction codex-cli
/// 0.154.0 returns when resuming a persisted danger-full-access rollout;
/// both confirmed yolo shapes are the requested posture or strictly
/// narrower (no network, no extra writable roots), so any other object
/// fails closed.
fn sandbox_confirmed(sandbox: &serde_json::Value, mode: external_core::PermissionMode) -> bool {
    match mode {
        external_core::PermissionMode::Plan => {
            sandbox.get("type").and_then(|value| value.as_str()) == Some("readOnly")
                && sandbox
                    .get("networkAccess")
                    .and_then(|access| access.as_bool())
                    == Some(false)
        }
        external_core::PermissionMode::Yolo => {
            *sandbox == serde_json::json!({"type": "dangerFullAccess"})
                || *sandbox
                    == serde_json::json!({
                        "type": "workspaceWrite",
                        "networkAccess": false,
                        "writableRoots": [],
                        "excludeSlashTmp": false,
                        "excludeTmpdirEnvVar": false,
                    })
        }
        _ => false,
    }
}

/// A thread-result posture field, accepted from the thread object or the
/// result root, mirroring how the start/resume results carry the model.
fn thread_result_field<'a>(
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
