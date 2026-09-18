//! The DSH session control plane: prompt turns with their settlement
//! watchers, and the initialize/session-new/model bootstrap sequence whose
//! verification gates the first prompt.

use std::{
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use external_agent_dsh::acp::{session::SessionError, transport};
use external_store::TaskRecord;

use super::owner::DshRuntimeOwner;
use crate::{task_route, RuntimeCommandError, SessionReady};

impl DshRuntimeOwner {
    /// Begin one prompt turn and register its settlement watcher. Returns
    /// once the request is on the wire; the agent turn runs to settlement on
    /// the watcher thread so control-plane operations never block on it.
    pub(super) fn send_prompt(&self, prompt: &str) -> Result<(), RuntimeCommandError> {
        let snapshot = self.shared.turn_tracker.snapshot();
        if snapshot.active {
            return Err(RuntimeCommandError::InvalidSession(
                "a prompt is already in flight for this dsh session".into(),
            ));
        }
        let (prompt_id, pending) = {
            let mut session = self.session.lock().unwrap();
            session
                .prompt(prompt)
                .map_err(|error| RuntimeCommandError::InvalidSession(dsh_session_message(&error)))
        }?;
        self.shared.begin_turn(prompt_id);
        let shared = Arc::clone(&self.shared);
        thread::Builder::new()
            .name("dsh-settlement".into())
            .spawn(move || {
                let settlement = match pending.wait(Duration::from_secs(24 * 60 * 60)) {
                    Ok(response) => match response.result.as_ref() {
                        Some(result) => match transport::parse_prompt_settlement(result) {
                            Ok(settlement) => Ok(settlement),
                            Err(error) => Err(error.to_string()),
                        },
                        None => Err("session/prompt settled without a result".into()),
                    },
                    Err(error) => Err(error.to_string()),
                };
                match settlement {
                    Ok(settlement) => shared.apply_settlement(&settlement),
                    Err(message) => shared.apply_failed_settlement(&message),
                }
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
        let model = match task_route(task) {
            Ok(crate::TaskRoute::General(prepared)) => prepared
                .admission
                .as_ref()
                .and_then(|identity| identity.model.clone()),
            Err(message) => {
                return Err(RuntimeCommandError::InvalidSession(message));
            }
        };
        let mut session = self.session.lock().unwrap();
        let remaining = || remaining_time(deadline);
        let capabilities = session
            .initialize(remaining()?)
            .map_err(|error| RuntimeCommandError::InvalidSession(dsh_session_message(&error)))?;
        transport::require_build_capabilities(&capabilities)
            .map_err(|error| RuntimeCommandError::InvalidSession(error.to_string()))?;
        let cwd = PathBuf::from(&task.workspace_path);
        session
            .new_session(&cwd, remaining()?)
            .map_err(|error| RuntimeCommandError::InvalidSession(dsh_session_message(&error)))?;
        let session_id = session
            .session_id()
            .expect("session id is set after new_session")
            .to_owned();
        *self.shared.session_id.lock().unwrap() = Some(session_id.clone());
        let configured_model = if let Some(token) = model.as_deref() {
            session.set_model(token, remaining()?).map_err(|error| {
                RuntimeCommandError::InvalidSession(dsh_session_message(&error))
            })?;
            Some(token.to_owned())
        } else {
            None
        };
        // The initial prompt only leaves after initialize, session/new, and
        // any model selection have all been verified (X05/P02).
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

pub(super) fn dsh_session_message(error: &SessionError) -> String {
    let text = error.to_string();
    text.chars().take(512).collect()
}

pub(super) fn remaining_time(deadline: Instant) -> Result<Duration, RuntimeCommandError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(RuntimeCommandError::Timeout)
}
