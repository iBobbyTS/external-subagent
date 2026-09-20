//! The pure Codex thread and turn control-plane shapes: the
//! `thread/start`, `thread/resume`, and `turn/start` parameter shapes,
//! posture admission for the two admitted permission modes, and the
//! fail-closed echo validation of thread results. The daemon drives the
//! requests and maps every [`CodexError`] onto its own
//! `RuntimeCommandError` variant for variant with the payload verbatim.

use std::{
    path::Path,
    time::{Duration, Instant},
};

/// The two admitted Codex postures. The daemon's four-value permission
/// mode narrows to this binary pair at the admitted-thread boundary; every
/// other mode fails closed there with [`CodexPermissionMode::unsupported`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexPermissionMode {
    Plan,
    Yolo,
}

impl CodexPermissionMode {
    /// The fail-closed refusal for any posture outside the admitted pair.
    /// The daemon glue's four-value-to-two-value seam maps every
    /// non-admitted permission mode onto exactly this error, whose message
    /// is the session-level admission oracle (moved verbatim from the
    /// daemon session glue).
    pub fn unsupported() -> CodexError {
        CodexError::InvalidSession(
            "codex runtime supports only the plan and yolo permission modes".into(),
        )
    }
}

/// The Codex control-plane error surface, variant for variant with the
/// daemon's `RuntimeCommandError`; the daemon glue maps each variant with
/// its payload verbatim, so every message string here is the wire oracle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexError {
    Unsupported,
    Timeout,
    Transport(String),
    Remote(serde_json::Value),
    InvalidSession(String),
}

/// The pinned Codex thread posture for one admitted permission mode. Both
/// admitted modes pin `approvalPolicy=never`: plan runs the read-only
/// sandbox, yolo runs `danger-full-access` with no codex-side confinement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexPosture {
    pub sandbox: &'static str,
    pub label: &'static str,
}

pub fn codex_posture(mode: CodexPermissionMode) -> CodexPosture {
    match mode {
        CodexPermissionMode::Plan => CodexPosture {
            sandbox: "read-only",
            label: "read-only object",
        },
        CodexPermissionMode::Yolo => CodexPosture {
            sandbox: "danger-full-access",
            label: "danger-full-access posture",
        },
    }
}

/// The `thread/start` request parameters: the admitted model and workspace
/// with the never-approve policy, the mode's pinned sandbox, and a
/// persistent (non-ephemeral) thread.
pub fn thread_start_params(
    model: &str,
    workspace_path: &str,
    posture: &CodexPosture,
) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "cwd": workspace_path,
        "approvalPolicy": "never",
        "sandbox": posture.sandbox,
        "ephemeral": false,
    })
}

/// The `thread/resume` request parameters: the persistent thread identity
/// with the turn history excluded.
pub fn thread_resume_params(thread_id: &str) -> serde_json::Value {
    serde_json::json!({
        "threadId": thread_id,
        "excludeTurns": true,
    })
}

/// The `turn/start` request parameters. A task without an admitted effort
/// keeps the historical default: its turn/start wire stays byte-identical
/// with `"effort":"low"`.
pub fn turn_start_params(
    thread_id: &str,
    model: &str,
    admitted_effort: Option<&str>,
    input: &str,
) -> serde_json::Value {
    serde_json::json!({
        "threadId": thread_id,
        "model": model,
        "effort": admitted_effort.unwrap_or("low"),
        "input": [{"type": "text", "text": input}],
    })
}

/// Validate a `thread/start` result against the admitted request and
/// return its bounded thread id. The result echoes the posture the server
/// actually applied — verified on codex-cli 0.154.0 for BOTH admitted
/// presets: plan resolves `{"type":"readOnly","networkAccess":false}`,
/// yolo `{"type":"dangerFullAccess"}`, each with `approvalPolicy` and
/// `cwd` at the result root (see docs/compatibility/codex.md). Request
/// params alone are therefore no more trusted for the first executable
/// turn than they are on resume: an unconfirmed or divergent posture
/// fails closed before any `turn/start` is sent.
pub fn start_result_thread_id(
    result: &serde_json::Value,
    model: &str,
    workspace_path: &str,
    permission_mode: CodexPermissionMode,
    requested_effort: Option<&str>,
) -> Result<String, CodexError> {
    let thread_id = result
        .pointer("/thread/id")
        .and_then(|value| value.as_str())
        .filter(|id| !id.is_empty() && id.len() <= 512)
        .ok_or_else(|| {
            CodexError::InvalidSession("thread/start result is missing a bounded thread id".into())
        })?;
    if result
        .pointer("/thread/ephemeral")
        .and_then(|value| value.as_bool())
        != Some(false)
    {
        return Err(CodexError::InvalidSession(
            "codex thread is not persistent (ephemeral must be false)".into(),
        ));
    }
    validate_thread_model(model, result.get("model"))?;
    let confirmed = thread_result_field(result, "approvalPolicy").and_then(|value| value.as_str())
        == Some("never");
    if !confirmed {
        return Err(CodexError::InvalidSession(
            "start approvalPolicy was not confirmed as never".into(),
        ));
    }
    let posture = codex_posture(permission_mode);
    let confirmed = thread_result_field(result, "sandbox")
        .is_some_and(|value| sandbox_confirmed(value, permission_mode));
    if !confirmed {
        return Err(CodexError::InvalidSession(format!(
            "start sandbox was not confirmed as the {}",
            posture.label
        )));
    }
    let cwd = thread_result_field(result, "cwd").and_then(|value| value.as_str());
    if !cwd.is_some_and(|cwd| Path::new(cwd) == Path::new(workspace_path)) {
        return Err(CodexError::InvalidSession(
            "start cwd does not match the task workspace".into(),
        ));
    }
    note_start_effort_echo(requested_effort, result);
    Ok(thread_id.to_owned())
}

/// Validate a `thread/resume` result against the admitted request. A
/// resumed thread runs on a fresh process, so the admitted posture must be
/// re-confirmed from the resume result before any turn is trusted — the
/// same confirmation a start applies to the first launch: never-approve
/// policy plus the sandbox matching the admitted permission mode. An
/// unconfirmed or divergent posture fails closed instead of resuming with
/// a different capability.
///
/// The live probes resolve the resumed sandbox as an object (plan:
/// `{"type":"readOnly","networkAccess":false}`; a yolo thread on
/// codex-cli 0.154.0 resumes as the narrowed workspace-write
/// reconstruction of its persisted danger-full-access rollout — the
/// faithful `{"type":"dangerFullAccess"}` shape is accepted too).
/// Request-time string presets, any posture belonging to another
/// permission mode, an unknown representation, or a network-capable
/// sandbox are all unconfirmed and fail closed.
pub fn validate_resume_result(
    result: &serde_json::Value,
    thread_id: &str,
    model: &str,
    workspace_path: &str,
    permission_mode: CodexPermissionMode,
    requested_effort: Option<&str>,
) -> Result<(), CodexError> {
    if result
        .pointer("/thread/id")
        .and_then(|value| value.as_str())
        != Some(thread_id)
    {
        return Err(CodexError::InvalidSession(
            "thread/resume returned a different thread id".into(),
        ));
    }
    if result
        .pointer("/thread/ephemeral")
        .and_then(|value| value.as_bool())
        != Some(false)
    {
        return Err(CodexError::InvalidSession(
            "resumed codex thread is not persistent".into(),
        ));
    }
    validate_thread_model(model, result.get("model"))?;
    let posture = codex_posture(permission_mode);
    let confirmed = thread_result_field(result, "sandbox")
        .is_some_and(|value| sandbox_confirmed(value, permission_mode));
    if !confirmed {
        return Err(CodexError::InvalidSession(format!(
            "resume sandbox was not confirmed as the {}",
            posture.label
        )));
    }
    if thread_result_field(result, "approvalPolicy").and_then(|value| value.as_str())
        != Some("never")
    {
        return Err(CodexError::InvalidSession(
            "resume approvalPolicy was not confirmed as never".into(),
        ));
    }
    let cwd = thread_result_field(result, "cwd").and_then(|value| value.as_str());
    if !cwd.is_some_and(|cwd| Path::new(cwd) == Path::new(workspace_path)) {
        return Err(CodexError::InvalidSession(
            "resume cwd does not match the task workspace".into(),
        ));
    }
    confirm_resumed_thread_effort(requested_effort, result)
}

/// The bounded turn id a `turn/start` response must name: a missing or
/// unbounded id cannot be reconciled with the started notification, so the
/// start fails closed instead of adopting an unverified turn.
pub fn turn_start_response_turn_id(
    result: Option<&serde_json::Value>,
) -> Result<String, CodexError> {
    result
        .and_then(|result| result.pointer("/turn/id"))
        .and_then(|value| value.as_str())
        .filter(|id| !id.is_empty() && id.len() <= 512)
        .map(str::to_owned)
        .ok_or_else(|| {
            CodexError::InvalidSession("turn/start response is missing a bounded turn id".into())
        })
}

pub fn validate_thread_model(
    requested: &str,
    observed: Option<&serde_json::Value>,
) -> Result<(), CodexError> {
    let observed = observed.and_then(|value| value.as_str()).ok_or_else(|| {
        CodexError::InvalidSession("MODEL_NOT_OBSERVED: thread model is missing".into())
    })?;
    if observed != requested {
        return Err(CodexError::InvalidSession(
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
/// The daemon crate's production code historically had no eprintln sites
/// (its rpc/config.rs one is `cfg(test)`); these diagnostics go to stderr,
/// which launchd captures into logs/daemon-error.log, and deliberately do
/// not open a new observable field.
pub fn note_start_effort_echo(requested_effort: Option<&str>, result: &serde_json::Value) {
    let Some(requested_effort) = requested_effort else {
        return;
    };
    if let Some(echo) = thread_result_field(result, "reasoningEffort") {
        eprintln!(
            "codex thread/start echoed reasoningEffort {} (the model default, not an acknowledgement of the request); turn/start still names the admitted {requested_effort}",
            echo.as_str().unwrap_or("<non-string>")
        );
    }
}

/// Confirm the thread/resume-result `reasoningEffort` echo against the
/// admitted effort. Unlike a fresh start, a resumed thread's echo reflects
/// the effort its previous turn actually ran with (OBSERVED on codex-cli
/// 0.154.0: a persistent thread resumed after a low turn echoes "low"), so
/// a task that explicitly admitted an effort must see that effort echoed:
/// a divergent echo fails closed before any follow-up `turn/start` is
/// sent, while a missing echo (e.g. a thread resumed before any turn) is
/// never compared against anything and only reaches the daemon diagnostic
/// log.
pub fn confirm_resumed_thread_effort(
    requested_effort: Option<&str>,
    result: &serde_json::Value,
) -> Result<(), CodexError> {
    let Some(requested_effort) = requested_effort else {
        return Ok(());
    };
    match thread_result_field(result, "reasoningEffort") {
        Some(echo) => {
            if echo.as_str() != Some(requested_effort) {
                return Err(CodexError::InvalidSession(format!(
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

/// Confirm a thread-result sandbox against the admitted permission mode,
/// for the first launch and every resume alike. Plan threads must resolve
/// the read-only object. A yolo thread must resolve the faithful
/// `{"type":"dangerFullAccess"}` object — the shape a real 0.154.0 start
/// echoes — or the exact narrowed workspace-write reconstruction codex-cli
/// 0.154.0 returns when resuming a persisted danger-full-access rollout;
/// both confirmed yolo shapes are the requested posture or strictly
/// narrower (no network, no extra writable roots), so any other object
/// fails closed.
pub fn sandbox_confirmed(sandbox: &serde_json::Value, mode: CodexPermissionMode) -> bool {
    match mode {
        CodexPermissionMode::Plan => {
            sandbox.get("type").and_then(|value| value.as_str()) == Some("readOnly")
                && sandbox
                    .get("networkAccess")
                    .and_then(|access| access.as_bool())
                    == Some(false)
        }
        CodexPermissionMode::Yolo => {
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
    }
}

/// A thread-result posture field, accepted from the thread object or the
/// result root, mirroring how the start/resume results carry the model.
pub fn thread_result_field<'a>(
    result: &'a serde_json::Value,
    key: &str,
) -> Option<&'a serde_json::Value> {
    result
        .pointer(&format!("/thread/{key}"))
        .or_else(|| result.get(key))
}

pub fn remaining_time(deadline: Instant) -> Result<Duration, CodexError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(CodexError::Timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: CodexPermissionMode = CodexPermissionMode::Plan;
    const YOLO: CodexPermissionMode = CodexPermissionMode::Yolo;
    const WORKSPACE: &str = "/tmp/codex-echo-workspace";

    fn plan_start_result() -> serde_json::Value {
        serde_json::json!({
            "thread": {
                "id": "thread-1",
                "ephemeral": false,
                "sandbox": {"type": "readOnly", "networkAccess": false},
            },
            "model": "gpt-test",
            "approvalPolicy": "never",
            "cwd": WORKSPACE,
        })
    }

    fn yolo_start_result() -> serde_json::Value {
        serde_json::json!({
            "thread": {
                "id": "thread-2",
                "ephemeral": false,
                "sandbox": {"type": "dangerFullAccess"},
            },
            "model": "gpt-test",
            "approvalPolicy": "never",
            "cwd": WORKSPACE,
        })
    }

    #[test]
    fn posture_maps_the_two_admitted_modes() {
        assert_eq!(
            codex_posture(PLAN),
            CodexPosture {
                sandbox: "read-only",
                label: "read-only object",
            }
        );
        assert_eq!(
            codex_posture(YOLO),
            CodexPosture {
                sandbox: "danger-full-access",
                label: "danger-full-access posture",
            }
        );
    }

    #[test]
    fn unsupported_permission_modes_fail_closed_with_the_session_message() {
        assert_eq!(
            CodexPermissionMode::unsupported(),
            CodexError::InvalidSession(
                "codex runtime supports only the plan and yolo permission modes".into()
            )
        );
    }

    #[test]
    fn start_params_pin_the_admitted_posture() {
        let params = thread_start_params("gpt-test", WORKSPACE, &codex_posture(YOLO));
        assert_eq!(
            params,
            serde_json::json!({
                "model": "gpt-test",
                "cwd": WORKSPACE,
                "approvalPolicy": "never",
                "sandbox": "danger-full-access",
                "ephemeral": false,
            })
        );
    }

    #[test]
    fn resume_params_exclude_turn_history() {
        assert_eq!(
            thread_resume_params("thread-1"),
            serde_json::json!({"threadId": "thread-1", "excludeTurns": true})
        );
    }

    #[test]
    fn turn_params_keep_the_low_effort_default() {
        assert_eq!(
            turn_start_params("thread-1", "gpt-test", None, "build it"),
            serde_json::json!({
                "threadId": "thread-1",
                "model": "gpt-test",
                "effort": "low",
                "input": [{"type": "text", "text": "build it"}],
            })
        );
        assert_eq!(
            turn_start_params("thread-1", "gpt-test", Some("high"), "build it")["effort"],
            "high"
        );
    }

    #[test]
    fn thread_result_field_prefers_the_thread_object_then_the_root() {
        let result = serde_json::json!({
            "thread": {"cwd": "/thread/cwd"},
            "cwd": "/root/cwd",
            "approvalPolicy": "never",
        });
        assert_eq!(
            thread_result_field(&result, "cwd").and_then(|value| value.as_str()),
            Some("/thread/cwd")
        );
        assert_eq!(
            thread_result_field(&result, "approvalPolicy").and_then(|value| value.as_str()),
            Some("never")
        );
        assert!(thread_result_field(&result, "absent").is_none());
    }

    #[test]
    fn thread_model_validation_messages_are_verbatim() {
        assert_eq!(
            validate_thread_model("gpt-test", None).unwrap_err(),
            CodexError::InvalidSession("MODEL_NOT_OBSERVED: thread model is missing".into())
        );
        assert_eq!(
            validate_thread_model("gpt-test", Some(&serde_json::Value::Null)).unwrap_err(),
            CodexError::InvalidSession("MODEL_NOT_OBSERVED: thread model is missing".into())
        );
        assert_eq!(
            validate_thread_model("gpt-test", Some(&serde_json::json!("other-model"))).unwrap_err(),
            CodexError::InvalidSession(
                "MODEL_MISMATCH: thread model differs from the admitted request".into()
            )
        );
        assert_eq!(
            validate_thread_model("gpt-test", Some(&serde_json::json!("gpt-test"))),
            Ok(())
        );
    }

    #[test]
    fn plan_sandbox_confirms_only_the_read_only_object() {
        assert!(sandbox_confirmed(
            &serde_json::json!({"type": "readOnly", "networkAccess": false}),
            PLAN
        ));
        assert!(!sandbox_confirmed(
            &serde_json::json!({"type": "readOnly", "networkAccess": true}),
            PLAN
        ));
        assert!(!sandbox_confirmed(
            &serde_json::json!({"type": "readOnly"}),
            PLAN
        ));
        assert!(!sandbox_confirmed(
            &serde_json::json!({"type": "dangerFullAccess"}),
            PLAN
        ));
        assert!(!sandbox_confirmed(&serde_json::json!("read-only"), PLAN));
    }

    #[test]
    fn yolo_sandbox_confirms_the_faithful_and_narrowed_shapes() {
        assert!(sandbox_confirmed(
            &serde_json::json!({"type": "dangerFullAccess"}),
            YOLO
        ));
        assert!(sandbox_confirmed(
            &serde_json::json!({
                "type": "workspaceWrite",
                "networkAccess": false,
                "writableRoots": [],
                "excludeSlashTmp": false,
                "excludeTmpdirEnvVar": false,
            }),
            YOLO
        ));
        assert!(!sandbox_confirmed(
            &serde_json::json!({"type": "workspaceWrite", "networkAccess": true, "writableRoots": []}),
            YOLO
        ));
        assert!(!sandbox_confirmed(
            &serde_json::json!({"type": "workspaceWrite", "networkAccess": false, "writableRoots": ["/extra"]}),
            YOLO
        ));
        assert!(!sandbox_confirmed(
            &serde_json::json!({"type": "readOnly", "networkAccess": false}),
            YOLO
        ));
    }

    #[test]
    fn start_result_echo_validation_accepts_both_admitted_postures() {
        assert_eq!(
            start_result_thread_id(&plan_start_result(), "gpt-test", WORKSPACE, PLAN, None),
            Ok("thread-1".into())
        );
        assert_eq!(
            start_result_thread_id(&yolo_start_result(), "gpt-test", WORKSPACE, YOLO, None),
            Ok("thread-2".into())
        );
    }

    #[test]
    fn start_result_echo_failures_are_verbatim() {
        let missing_id = serde_json::json!({"thread": {"ephemeral": false}});
        assert_eq!(
            start_result_thread_id(&missing_id, "gpt-test", WORKSPACE, PLAN, None).unwrap_err(),
            CodexError::InvalidSession("thread/start result is missing a bounded thread id".into())
        );
        let empty_id = serde_json::json!({"thread": {"id": "", "ephemeral": false}});
        assert_eq!(
            start_result_thread_id(&empty_id, "gpt-test", WORKSPACE, PLAN, None).unwrap_err(),
            CodexError::InvalidSession("thread/start result is missing a bounded thread id".into())
        );
        let unbounded_id =
            serde_json::json!({"thread": {"id": "x".repeat(513), "ephemeral": false}});
        assert_eq!(
            start_result_thread_id(&unbounded_id, "gpt-test", WORKSPACE, PLAN, None).unwrap_err(),
            CodexError::InvalidSession("thread/start result is missing a bounded thread id".into())
        );

        let mut ephemeral = plan_start_result();
        ephemeral["thread"]["ephemeral"] = serde_json::json!(true);
        assert_eq!(
            start_result_thread_id(&ephemeral, "gpt-test", WORKSPACE, PLAN, None).unwrap_err(),
            CodexError::InvalidSession(
                "codex thread is not persistent (ephemeral must be false)".into()
            )
        );

        let mut model_mismatch = plan_start_result();
        model_mismatch["model"] = serde_json::json!("other-model");
        assert_eq!(
            start_result_thread_id(&model_mismatch, "gpt-test", WORKSPACE, PLAN, None).unwrap_err(),
            CodexError::InvalidSession(
                "MODEL_MISMATCH: thread model differs from the admitted request".into()
            )
        );

        let mut policy = plan_start_result();
        policy["approvalPolicy"] = serde_json::json!("on-request");
        assert_eq!(
            start_result_thread_id(&policy, "gpt-test", WORKSPACE, PLAN, None).unwrap_err(),
            CodexError::InvalidSession("start approvalPolicy was not confirmed as never".into())
        );

        let mut sandbox = plan_start_result();
        sandbox["thread"]["sandbox"] = serde_json::json!({"type": "dangerFullAccess"});
        assert_eq!(
            start_result_thread_id(&sandbox, "gpt-test", WORKSPACE, PLAN, None).unwrap_err(),
            CodexError::InvalidSession(
                "start sandbox was not confirmed as the read-only object".into()
            )
        );

        let mut yolo_sandbox = yolo_start_result();
        yolo_sandbox["thread"]["sandbox"] =
            serde_json::json!({"type": "readOnly", "networkAccess": false});
        assert_eq!(
            start_result_thread_id(&yolo_sandbox, "gpt-test", WORKSPACE, YOLO, None).unwrap_err(),
            CodexError::InvalidSession(
                "start sandbox was not confirmed as the danger-full-access posture".into()
            )
        );

        let mut cwd = plan_start_result();
        cwd["cwd"] = serde_json::json!("/tmp/other-workspace");
        assert_eq!(
            start_result_thread_id(&cwd, "gpt-test", WORKSPACE, PLAN, None).unwrap_err(),
            CodexError::InvalidSession("start cwd does not match the task workspace".into())
        );
    }

    #[test]
    fn start_result_reads_echo_fields_from_the_thread_object_or_root() {
        // The same posture fields the root carries may ride on the thread
        // object; both placements must confirm (mirroring how the results
        // carry the model).
        let thread_carried = serde_json::json!({
            "thread": {
                "id": "thread-3",
                "ephemeral": false,
                "sandbox": {"type": "readOnly", "networkAccess": false},
                "approvalPolicy": "never",
                "cwd": WORKSPACE,
            },
            "model": "gpt-test",
        });
        assert_eq!(
            start_result_thread_id(&thread_carried, "gpt-test", WORKSPACE, PLAN, None),
            Ok("thread-3".into())
        );
    }

    #[test]
    fn resume_result_echo_validation_accepts_both_admitted_postures() {
        let mut resumed = plan_start_result();
        resumed["thread"]["id"] = serde_json::json!("resumed-1");
        assert_eq!(
            validate_resume_result(&resumed, "resumed-1", "gpt-test", WORKSPACE, PLAN, None),
            Ok(())
        );

        let mut narrowed_yolo = yolo_start_result();
        narrowed_yolo["thread"]["id"] = serde_json::json!("resumed-2");
        narrowed_yolo["thread"]["sandbox"] = serde_json::json!({
            "type": "workspaceWrite",
            "networkAccess": false,
            "writableRoots": [],
            "excludeSlashTmp": false,
            "excludeTmpdirEnvVar": false,
        });
        assert_eq!(
            validate_resume_result(
                &narrowed_yolo,
                "resumed-2",
                "gpt-test",
                WORKSPACE,
                YOLO,
                None
            ),
            Ok(())
        );
    }

    #[test]
    fn resume_result_echo_failures_are_verbatim() {
        let resumed = plan_start_result();

        assert_eq!(
            validate_resume_result(&resumed, "other-thread", "gpt-test", WORKSPACE, PLAN, None)
                .unwrap_err(),
            CodexError::InvalidSession("thread/resume returned a different thread id".into())
        );

        let mut ephemeral = resumed.clone();
        ephemeral["thread"]["ephemeral"] = serde_json::json!(true);
        assert_eq!(
            validate_resume_result(&ephemeral, "thread-1", "gpt-test", WORKSPACE, PLAN, None)
                .unwrap_err(),
            CodexError::InvalidSession("resumed codex thread is not persistent".into())
        );

        let mut model_mismatch = resumed.clone();
        model_mismatch["model"] = serde_json::json!("other-model");
        assert_eq!(
            validate_resume_result(
                &model_mismatch,
                "thread-1",
                "gpt-test",
                WORKSPACE,
                PLAN,
                None
            )
            .unwrap_err(),
            CodexError::InvalidSession(
                "MODEL_MISMATCH: thread model differs from the admitted request".into()
            )
        );

        let mut sandbox = resumed.clone();
        sandbox["thread"]["sandbox"] = serde_json::json!({"type": "dangerFullAccess"});
        assert_eq!(
            validate_resume_result(&sandbox, "thread-1", "gpt-test", WORKSPACE, PLAN, None)
                .unwrap_err(),
            CodexError::InvalidSession(
                "resume sandbox was not confirmed as the read-only object".into()
            )
        );

        let mut policy = resumed.clone();
        policy["approvalPolicy"] = serde_json::json!("on-request");
        assert_eq!(
            validate_resume_result(&policy, "thread-1", "gpt-test", WORKSPACE, PLAN, None)
                .unwrap_err(),
            CodexError::InvalidSession("resume approvalPolicy was not confirmed as never".into())
        );

        let mut cwd = resumed.clone();
        cwd["cwd"] = serde_json::json!("/tmp/other-workspace");
        assert_eq!(
            validate_resume_result(&cwd, "thread-1", "gpt-test", WORKSPACE, PLAN, None)
                .unwrap_err(),
            CodexError::InvalidSession("resume cwd does not match the task workspace".into())
        );

        let mut effort = resumed.clone();
        effort["thread"]["reasoningEffort"] = serde_json::json!("medium");
        assert_eq!(
            validate_resume_result(
                &effort,
                "thread-1",
                "gpt-test",
                WORKSPACE,
                PLAN,
                Some("low")
            )
            .unwrap_err(),
            CodexError::InvalidSession(
                "resume reasoningEffort was not confirmed as the admitted low".into()
            )
        );
        // A missing echo is diagnostic-only: the admission stays unconfirmed
        // but the resume still succeeds.
        assert_eq!(
            validate_resume_result(
                &resumed,
                "thread-1",
                "gpt-test",
                WORKSPACE,
                PLAN,
                Some("low")
            ),
            Ok(())
        );
        // A matching echo confirms the admitted effort.
        let mut confirmed_effort = resumed.clone();
        confirmed_effort["thread"]["reasoningEffort"] = serde_json::json!("low");
        assert_eq!(
            validate_resume_result(
                &confirmed_effort,
                "thread-1",
                "gpt-test",
                WORKSPACE,
                PLAN,
                Some("low")
            ),
            Ok(())
        );
    }

    #[test]
    fn turn_start_response_requires_a_bounded_turn_id() {
        let response = serde_json::json!({"turn": {"id": "turn-1"}});
        assert_eq!(
            turn_start_response_turn_id(Some(&response)),
            Ok("turn-1".to_owned())
        );
        assert_eq!(
            turn_start_response_turn_id(None).unwrap_err(),
            CodexError::InvalidSession("turn/start response is missing a bounded turn id".into())
        );
        let empty = serde_json::json!({"turn": {"id": ""}});
        assert_eq!(
            turn_start_response_turn_id(Some(&empty)).unwrap_err(),
            CodexError::InvalidSession("turn/start response is missing a bounded turn id".into())
        );
        let unbounded = serde_json::json!({"turn": {"id": "x".repeat(513)}});
        assert_eq!(
            turn_start_response_turn_id(Some(&unbounded)).unwrap_err(),
            CodexError::InvalidSession("turn/start response is missing a bounded turn id".into())
        );
    }

    #[test]
    fn remaining_time_rejects_elapsed_deadlines() {
        assert_eq!(remaining_time(Instant::now()), Err(CodexError::Timeout));
        let future = Instant::now() + Duration::from_secs(1);
        assert!(remaining_time(future).is_ok());
    }
}
