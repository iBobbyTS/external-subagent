use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};

use crate::error::{StoreError, StoreResult};
use crate::records::{
    ControlDecision, LifecycleWrite, TaskOutcome, TaskPhase, TaskRecord, TaskResult,
    TerminalUpdate, TurnState,
};
use crate::results::{task_result_bytes, task_result_digest};
use crate::store::{i64_to_u64, now_millis, u64_to_i64, Store};
use crate::tasks::query_guard;

impl Store {
    pub fn append_lifecycle(&self, write: &LifecycleWrite) -> StoreResult<u64> {
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = transaction
            .query_row(
                "SELECT seq FROM events WHERE agent_id=?1 AND runtime_agent_id=?2 AND source_seq=?3",
                params![
                    write.agent_id,
                    write.runtime_agent_id,
                    u64_to_i64(write.source_sequence)?
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
        {
            transaction.commit()?;
            return i64_to_u64(existing);
        }
        let (phase, outcome, epoch, close_requested, stop_requested) =
            query_guard(&transaction, &write.agent_id)?;
        if phase == TaskPhase::Terminal || epoch != write.owner_epoch {
            return Err(StoreError::Conflict(format!(
                "late lifecycle record rejected for {} epoch {}",
                write.agent_id, write.owner_epoch
            )));
        }
        debug_assert!(outcome.is_none());
        let last_seq: i64 = transaction.query_row(
            "SELECT last_event_seq FROM tasks WHERE agent_id=?1",
            [&write.agent_id],
            |row| row.get(0),
        )?;
        let sequence = last_seq
            .checked_add(1)
            .ok_or_else(|| StoreError::InvalidState("event sequence overflow".into()))?;
        transaction.execute(
            "INSERT INTO events(agent_id,runtime_agent_id,seq,source_seq,timestamp,event_type,
                 turn_id,payload_json,redaction_level) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                write.agent_id,
                write.runtime_agent_id,
                sequence,
                u64_to_i64(write.source_sequence)?,
                now_millis(),
                write.event_type,
                write.turn_id,
                write.payload_json,
                write.redaction_level,
            ],
        )?;
        transaction.execute(
            "UPDATE tasks SET last_event_seq=?1,last_heartbeat_at=?2,
                 turn_state=COALESCE(?3,turn_state) WHERE agent_id=?4",
            params![
                sequence,
                now_millis(),
                write.turn_state.map(TurnState::as_str),
                write.agent_id,
            ],
        )?;
        if let Some(terminal) = &write.terminal {
            apply_terminal(
                &transaction,
                &write.agent_id,
                write.owner_epoch,
                phase,
                close_requested,
                stop_requested,
                terminal,
            )?;
        }
        transaction.commit()?;
        i64_to_u64(sequence)
    }

    pub fn transition_terminal(
        &self,
        agent_id: &str,
        owner_epoch: u64,
        terminal: &TerminalUpdate,
    ) -> StoreResult<TaskOutcome> {
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (phase, outcome, epoch, close_requested, stop_requested) =
            query_guard(&transaction, agent_id)?;
        if phase == TaskPhase::Terminal {
            transaction.commit()?;
            return outcome.ok_or_else(|| {
                StoreError::InvalidState("terminal task is missing its outcome".into())
            });
        }
        if epoch != owner_epoch {
            return Err(StoreError::Conflict(format!(
                "owner epoch changed for {agent_id}"
            )));
        }
        let outcome = apply_terminal(
            &transaction,
            agent_id,
            owner_epoch,
            phase,
            close_requested,
            stop_requested,
            terminal,
        )?;
        transaction.commit()?;
        Ok(outcome)
    }

    pub fn fail_claim(
        &self,
        agent_id: &str,
        owner_epoch: u64,
        failure_code: &str,
        message: &str,
    ) -> StoreResult<TaskOutcome> {
        self.transition_terminal(
            agent_id,
            owner_epoch,
            &TerminalUpdate {
                outcome: TaskOutcome::RuntimeLost,
                failure_code: Some(failure_code.into()),
                failure_message: Some(message.into()),
            },
        )
    }

    pub fn request_close(&self, agent_id: &str) -> StoreResult<ControlDecision> {
        self.request_stop_internal(agent_id, true, true)
    }

    pub fn request_stop(&self, agent_id: &str) -> StoreResult<ControlDecision> {
        self.request_stop_internal(agent_id, false, true)
    }

    pub fn request_runtime_stop(&self, agent_id: &str) -> StoreResult<ControlDecision> {
        self.request_stop_internal(agent_id, false, false)
    }

    fn request_stop_internal(
        &self,
        agent_id: &str,
        close: bool,
        cancellation_intent: bool,
    ) -> StoreResult<ControlDecision> {
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (phase, outcome, epoch, close_requested, stop_requested) =
            query_guard(&transaction, agent_id)?;
        let needs_runtime_stop = matches!(
            phase,
            TaskPhase::Preparing
                | TaskPhase::Running
                | TaskPhase::WaitingInput
                | TaskPhase::Cancelling
        );
        let next_phase = if phase == TaskPhase::Terminal {
            phase
        } else {
            TaskPhase::Cancelling
        };
        transaction.execute(
            "UPDATE tasks SET phase=?1,
                 close_requested=CASE WHEN ?2=1 THEN 1 ELSE close_requested END,
                 stop_requested=CASE WHEN ?3=1 THEN 1 ELSE stop_requested END,
                 closed_at=CASE WHEN ?2=1 THEN COALESCE(closed_at,?4) ELSE closed_at END
             WHERE agent_id=?5",
            params![
                next_phase.as_str(),
                close,
                cancellation_intent,
                now_millis(),
                agent_id,
            ],
        )?;
        if phase != TaskPhase::Terminal {
            settle_terminal_commands(&transaction, agent_id, "STOP_REQUESTED")?;
            if phase != next_phase {
                insert_ledger(
                    &transaction,
                    agent_id,
                    epoch,
                    Some(phase),
                    next_phase,
                    None,
                    Some(if close {
                        "CLOSE_REQUESTED"
                    } else {
                        "STOP_REQUESTED"
                    }),
                )?;
            }
        }
        transaction.commit()?;
        Ok(ControlDecision {
            phase: next_phase,
            outcome,
            owner_epoch: epoch,
            needs_runtime_stop,
            prior_stop_or_close: close_requested || stop_requested,
        })
    }

    /// Explicit terminal-send resume admission. The full durable
    /// eligibility check and the requeue state update share one immediate
    /// transaction, so a concurrent close or cancel that committed first can
    /// never be overwritten by a later resume. Returns `Ok(false)` without
    /// mutating anything when the durable state refuses the resume: a
    /// cancelled outcome, a stop/close request, a closed task, or a
    /// persisted old process identity whose process group was not proven
    /// reaped (`reaped_at` stays the only durable reap proof, so the old
    /// PID/PGID is preserved for recovery instead of being cleared).
    pub fn requeue_task_for_resume_with_message(
        &self,
        agent_id: &str,
        message_id: &str,
        content: &str,
    ) -> StoreResult<bool> {
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (
            phase,
            session_id,
            workspace_path,
            owner_epoch,
            outcome,
            stop_requested,
            close_requested,
            closed_at,
            pid,
            reaped_at,
        ): (
            TaskPhase,
            Option<String>,
            String,
            i64,
            Option<TaskOutcome>,
            bool,
            bool,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        ) = transaction.query_row(
            "SELECT phase,session_id,workspace_path,owner_epoch,outcome,
                    stop_requested,close_requested,closed_at,pid,reaped_at
             FROM tasks WHERE agent_id=?1",
            [agent_id],
            |row| {
                Ok((
                    TaskPhase::parse(&row.get::<_, String>(0)?).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get::<_, Option<String>>(4)?
                        .map(|value| TaskOutcome::parse(&value))
                        .transpose()
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                    row.get::<_, i64>(5)? != 0,
                    row.get::<_, i64>(6)? != 0,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                ))
            },
        )?;
        if phase != TaskPhase::Terminal {
            return Err(StoreError::Conflict(format!(
                "task {agent_id} is not terminal"
            )));
        }
        if session_id.is_none() {
            return Err(StoreError::InvalidState(
                "task has no persisted session id to resume".into(),
            ));
        }
        if outcome == Some(TaskOutcome::Cancelled)
            || stop_requested
            || close_requested
            || closed_at.is_some()
        {
            return Ok(false);
        }
        if pid.is_some() && reaped_at.is_none() {
            return Ok(false);
        }
        if let Some(active_agent_id) = transaction
            .query_row(
                "SELECT agent_id FROM tasks
                 WHERE workspace_path=?1 AND agent_id!=?2
                   AND phase IN ('QUEUED','PREPARING','RUNNING','WAITING_INPUT','CANCELLING')
                 ORDER BY created_at,rowid LIMIT 1",
                params![workspace_path, agent_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return Err(StoreError::Conflict(format!(
                "WORKSPACE_BUSY active_agent_id={active_agent_id}"
            )));
        }
        if transaction
            .query_row(
                "SELECT 1 FROM messages WHERE message_id=?1",
                [message_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            return Err(StoreError::Conflict(format!(
                "message {message_id} already exists"
            )));
        }
        transaction.execute("DELETE FROM task_results WHERE agent_id=?1", [agent_id])?;
        let changed = transaction.execute(
            "UPDATE tasks SET phase='QUEUED',outcome=NULL,owner_id=NULL,
                 lease_expires_at=NULL,close_requested=0,stop_requested=0,
                 failure_code=NULL,failure_message=NULL,closed_at=NULL,reaped_at=NULL,
                 completed_at=NULL,last_event_seq=0,turn_state='IDLE',pid=NULL,
                 process_group_id=NULL,process_uid=NULL,process_start_token=NULL,
                 runtime_agent_id=NULL WHERE agent_id=?1 AND phase='TERMINAL'",
            [agent_id],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(format!(
                "task {agent_id} changed before resume"
            )));
        }
        insert_ledger(
            &transaction,
            agent_id,
            i64_to_u64(owner_epoch)?,
            Some(TaskPhase::Terminal),
            TaskPhase::Queued,
            None,
            Some("RESUME_REQUESTED"),
        )?;
        transaction.execute(
            "INSERT INTO messages(message_id,agent_id,mode,content,state,created_at)
             VALUES (?1,?2,'queue',?3,'QUEUED',?4)",
            params![message_id, agent_id, content, now_millis()],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn reap_task(&self, agent_id: &str) -> StoreResult<TaskOutcome> {
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (phase, outcome, _, _, _) = query_guard(&transaction, agent_id)?;
        if phase != TaskPhase::Terminal {
            return Err(StoreError::InvalidState(format!(
                "cannot reap nonterminal task {agent_id}"
            )));
        }
        transaction.execute(
            "UPDATE tasks SET owner_id=NULL,lease_expires_at=NULL,
                 reaped_at=COALESCE(reaped_at,?1) WHERE agent_id=?2",
            params![now_millis(), agent_id],
        )?;
        transaction.commit()?;
        outcome.ok_or_else(|| StoreError::InvalidState("terminal task has no outcome".into()))
    }

    pub fn restore_terminal_after_resume_failure(
        &self,
        original: &TaskRecord,
        result: Option<&TaskResult>,
    ) -> StoreResult<()> {
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE tasks SET phase='TERMINAL',outcome=?1,owner_id=?2,lease_expires_at=NULL,
                 close_requested=?3,stop_requested=?4,failure_code=?5,failure_message=?6,
                 runtime_agent_id=?7,session_id=?8,turn_state=?9,pid=?10,
                 process_group_id=?11,process_uid=?12,process_start_token=?13,
                 closed_at=?14,reaped_at=?15,last_event_seq=?16
             WHERE agent_id=?17",
            params![
                original.outcome.map(TaskOutcome::as_str),
                original.owner_id,
                original.close_requested,
                original.stop_requested,
                original.failure_code,
                original.failure_message,
                original.runtime_agent_id,
                original.session_id,
                original.turn_state.as_str(),
                original.process_identity.as_ref().map(|v| v.pid),
                original
                    .process_identity
                    .as_ref()
                    .map(|v| v.process_group_id),
                original.process_identity.as_ref().map(|v| v.uid),
                original.process_identity.as_ref().map(|v| &v.start_token),
                original.closed_at,
                original.reaped_at,
                u64_to_i64(original.last_event_seq)?,
                original.agent_id,
            ],
        )?;
        if let Some(result) = result {
            transaction.execute(
                "DELETE FROM task_results WHERE agent_id=?1",
                [original.agent_id.as_str()],
            )?;
            let canonical = task_result_bytes(result)?;
            let digest = task_result_digest(&canonical);
            transaction.execute(
                "INSERT INTO task_results(agent_id,outcome,final_text,partial,result_sha256,completed_at)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    original.agent_id,
                    result.outcome.as_str(),
                    result.final_text,
                    result.partial,
                    digest,
                    now_millis(),
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }
}

pub(crate) fn apply_terminal(
    transaction: &Transaction<'_>,
    agent_id: &str,
    owner_epoch: u64,
    from_phase: TaskPhase,
    close_requested: bool,
    stop_requested: bool,
    terminal: &TerminalUpdate,
) -> StoreResult<TaskOutcome> {
    if from_phase == TaskPhase::Terminal {
        return Err(StoreError::Conflict("task is already terminal".into()));
    }
    let outcome =
        if (stop_requested || close_requested) && terminal.outcome != TaskOutcome::Cancelled {
            TaskOutcome::Cancelled
        } else {
            terminal.outcome
        };
    let now = now_millis();
    let changed = transaction.execute(
        "UPDATE tasks SET phase='TERMINAL',outcome=?1,completed_at=COALESCE(completed_at,?2),
             failure_code=?3,failure_message=?4,
             turn_state=CASE WHEN ?1 IN ('FAILED','RUNTIME_LOST','RESULT_INVALID')
                             THEN 'FAILED' ELSE 'IDLE' END,
             closed_at=CASE WHEN close_requested=1 THEN COALESCE(closed_at,?2) ELSE closed_at END
         WHERE agent_id=?5 AND owner_epoch=?6 AND phase!='TERMINAL'",
        params![
            outcome.as_str(),
            now,
            terminal.failure_code,
            terminal.failure_message,
            agent_id,
            u64_to_i64(owner_epoch)?,
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::Conflict(format!(
            "terminal transition lost for {agent_id} epoch {owner_epoch}"
        )));
    }
    settle_terminal_commands(
        transaction,
        agent_id,
        terminal
            .failure_code
            .as_deref()
            .unwrap_or("TASK_TERMINATED"),
    )?;
    insert_ledger(
        transaction,
        agent_id,
        owner_epoch,
        Some(from_phase),
        TaskPhase::Terminal,
        Some(outcome),
        terminal.failure_code.as_deref(),
    )?;
    Ok(outcome)
}

fn settle_terminal_commands(
    transaction: &Transaction<'_>,
    agent_id: &str,
    reason_code: &str,
) -> StoreResult<()> {
    transaction.execute(
        "UPDATE messages SET state='FAILED',failure_code=?1,
             failure_message='runtime is no longer available'
         WHERE agent_id=?2 AND state IN ('QUEUED','SENDING')",
        params![reason_code, agent_id],
    )?;
    transaction.execute(
        "DELETE FROM pending_requests
         WHERE agent_id=?1 AND state IN ('PENDING','SENDING')",
        [agent_id],
    )?;
    Ok(())
}

pub(crate) fn insert_ledger(
    transaction: &Transaction<'_>,
    agent_id: &str,
    owner_epoch: u64,
    from_phase: Option<TaskPhase>,
    to_phase: TaskPhase,
    outcome: Option<TaskOutcome>,
    reason_code: Option<&str>,
) -> StoreResult<()> {
    transaction.execute(
        "INSERT INTO lifecycle_ledger(agent_id,owner_epoch,from_phase,to_phase,outcome,
             reason_code,recorded_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            agent_id,
            u64_to_i64(owner_epoch)?,
            from_phase.map(TaskPhase::as_str),
            to_phase.as_str(),
            outcome.map(TaskOutcome::as_str),
            reason_code,
            now_millis(),
        ],
    )?;
    Ok(())
}
