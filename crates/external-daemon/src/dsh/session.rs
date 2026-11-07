//! The DSH session control plane: prompt turns with their settlement
//! watchers, and the initialize/session-new/model/reasoning-effort bootstrap
//! sequence whose verification gates the first prompt.

use std::{
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use external_agent_dsh::acp::{session::SessionError, transport};
use external_store::TaskRecord;

use super::events::{SettledTurn, REMOTE_ERROR_REASON};
use super::owner::DshRuntimeOwner;
use crate::{task_route, RuntimeCommandDetail, RuntimeCommandError, SessionReady};

impl DshRuntimeOwner {
    /// Begin one prompt turn and register its settlement watcher. Returns
    /// once the request is on the wire; the agent turn runs to settlement on
    /// the watcher thread so control-plane operations never block on it. The
    /// watcher never folds: it deposits the parsed settlement for the pump,
    /// which applies it strictly after every frame that preceded the
    /// response on the wire has been projected.
    pub(super) fn send_prompt(&self, prompt: &str) -> Result<(), RuntimeCommandError> {
        let snapshot = self.shared.turn_tracker.snapshot();
        if snapshot.active {
            return Err(RuntimeCommandError::InvalidSession(
                "a prompt is already in flight for this dsh session".into(),
            ));
        }
        let mut started = false;
        let result = {
            let mut session = self.session.lock().unwrap();
            session.prompt_with_start(prompt, |prompt_id| {
                self.shared.begin_turn(prompt_id);
                started = true;
            })
        };
        let (_, pending) = result.map_err(|error| {
            if started {
                // No watcher exists for a failed write. Seal the reserved
                // bucket and close the shared turn rather than leaving a
                // phantom prompt active or retaining its text.
                self.shared
                    .apply_failed_settlement("ACP_PROMPT_WRITE_FAILED", None);
            }
            RuntimeCommandError::InvalidSession(dsh_session_detail(&error))
        })?;
        let shared = Arc::clone(&self.shared);
        thread::Builder::new()
            .name("dsh-settlement".into())
            .spawn(move || {
                let settled = match pending.wait(Duration::from_secs(24 * 60 * 60)) {
                    Ok(response) => match response.result.as_ref() {
                        Some(result) => match transport::parse_prompt_settlement(result) {
                            Ok(settlement) => SettledTurn::Settled(settlement),
                            Err(error) => SettledTurn::Failed {
                                reason: error.to_string(),
                                native: None,
                            },
                        },
                        None => SettledTurn::Failed {
                            reason: "session/prompt settled without a result".into(),
                            native: None,
                        },
                    },
                    Err(error) => match error.remote_error() {
                        // The peer answered with a JSON-RPC error: deliver that
                        // object whole instead of flattening it into the
                        // runtime's generic label. Upstream sends `message`
                        // only, so no code/status/requestId is invented.
                        Some(native) => SettledTurn::Failed {
                            reason: REMOTE_ERROR_REASON.into(),
                            native: Some(native.clone()),
                        },
                        None => SettledTurn::Failed {
                            reason: error.to_string(),
                            native: None,
                        },
                    },
                };
                shared.deposit_settled_turn(settled);
            })
            .map_err(|error| RuntimeCommandError::Transport(error.to_string()))?;
        Ok(())
    }

    pub(super) fn bootstrap(
        &self,
        task: &TaskRecord,
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RuntimeCommandError::Timeout)?;
        let (model, effort) = match task_route(task) {
            Ok(crate::TaskRoute::General(prepared)) => {
                let admission = prepared.admission.as_ref();
                (
                    admission.and_then(|identity| identity.model.clone()),
                    admission.and_then(|identity| identity.effort.clone()),
                )
            }
            Err(message) => {
                return Err(RuntimeCommandError::InvalidSession(message.into()));
            }
        };
        let mut session = self.session.lock().unwrap();
        let remaining = || remaining_time(deadline);
        let capabilities = session
            .initialize(remaining()?)
            .map_err(|error| RuntimeCommandError::InvalidSession(dsh_session_detail(&error)))?;
        transport::require_build_capabilities(&capabilities)
            .map_err(|error| RuntimeCommandError::InvalidSession(error.to_string().into()))?;
        let cwd = PathBuf::from(&task.workspace_path);
        session
            .new_session(&cwd, remaining()?)
            .map_err(|error| RuntimeCommandError::InvalidSession(dsh_session_detail(&error)))?;
        let session_id = session
            .session_id()
            .expect("session id is set after new_session")
            .to_owned();
        *self.shared.session_id.lock().unwrap() = Some(session_id.clone());
        let configured_model = if let Some(token) = model.as_deref() {
            session
                .set_model(token, remaining()?)
                .map_err(|error| match error {
                    SessionError::Remote(_) | SessionError::Model(_) => {
                        RuntimeCommandError::ModelRejected(model_rejection_detail(&error))
                    }
                    error => RuntimeCommandError::InvalidSession(dsh_session_detail(&error)),
                })?;
            Some(token.to_owned())
        } else {
            None
        };
        // The admitted reasoning effort rides the same verified config
        // option channel as the model (X05): applied after the model
        // selection and strictly before the first prompt, and never sent
        // when the admission carries no effort token. A DSH task has no
        // separate resume entry point — every claim re-enters this
        // bootstrap — so the session-scoped setting cannot go stale.
        if let Some(token) = effort.as_deref() {
            session
                .set_reasoning_effort(token, remaining()?)
                .map_err(|error| RuntimeCommandError::InvalidSession(dsh_session_detail(&error)))?;
        }
        // The initial prompt only leaves after initialize, session/new, and
        // any model or reasoning-effort selection have all been verified
        // (X05/P02).
        let prompt = task.initial_prompt.clone();
        drop(session);
        self.send_prompt(&prompt)?;
        Ok(SessionReady {
            session_id,
            initial_turn_id: None,
            configured_model,
        })
    }
}

/// The bounded classification text plus, when the peer answered with one, the
/// complete credential-redacted JSON-RPC error. The original is the authority
/// for the failure; the bounded text only keeps the established classification
/// bytes stable, and never replaces the structured `data` or a long `message`.
pub(super) fn dsh_session_detail(error: &SessionError) -> RuntimeCommandDetail {
    let message: String = error.to_string().chars().take(512).collect();
    match error {
        SessionError::Remote(value) if !value.is_null() => {
            RuntimeCommandDetail::from_native(message, value)
        }
        _ => RuntimeCommandDetail::from(message),
    }
}

/// The public model-rejection reason: the server's own `error.message` when the
/// JSON-RPC rejection carries one, and the whole object text otherwise. Other
/// session errors fall back to their Display text. The bounded classification
/// text travels beside the complete native rejection.
fn model_rejection_detail(error: &SessionError) -> RuntimeCommandDetail {
    let text = match error {
        SessionError::Remote(value) => value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string()),
        other => other.to_string(),
    };
    let message: String = text.chars().take(512).collect();
    match error {
        SessionError::Remote(value) if !value.is_null() => {
            RuntimeCommandDetail::from_native(message, value)
        }
        _ => RuntimeCommandDetail::from(message),
    }
}

pub(super) fn remaining_time(deadline: Instant) -> Result<Duration, RuntimeCommandError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(RuntimeCommandError::Timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refused command keeps the peer's complete error (structured `data`
    /// and a long `message`) beside the bounded classification text. A local
    /// validation failure has no native original and invents none.
    #[test]
    fn command_failures_keep_the_complete_native_error() {
        let native = serde_json::json!({
            "code": -32602,
            "message": "unknown model option: nope",
            "data": {"retryAfter": 30, "apiKey": "secret"}
        });
        let remote = SessionError::Remote(native.clone());

        let detail = dsh_session_detail(&remote);
        assert!(detail.message.contains("unknown model option: nope"));
        let failure = detail.native_failure.expect("native failure carrier");
        assert_eq!(failure.original["code"], -32602);
        assert_eq!(failure.original["data"]["retryAfter"], 30);
        assert_eq!(failure.original["data"]["apiKey"], "[REDACTED]");

        let model = model_rejection_detail(&remote);
        assert_eq!(model.message, "unknown model option: nope");
        assert_eq!(
            model
                .native_failure
                .expect("native failure carrier")
                .original["data"]["retryAfter"],
            30
        );

        for local in [
            SessionError::Timeout,
            SessionError::Transport("pipe closed".into()),
        ] {
            let detail = dsh_session_detail(&local);
            assert!(detail.native_failure.is_none(), "no native object to carry");
        }
        // A missing result is not a native error object either.
        assert!(
            dsh_session_detail(&SessionError::Remote(serde_json::Value::Null))
                .native_failure
                .is_none()
        );
    }
}
