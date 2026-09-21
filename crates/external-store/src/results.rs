use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sha2::Digest;

use crate::error::{StoreError, StoreResult};
use crate::lifecycle::apply_terminal;
use crate::pending::completion_blockers_tx;
use crate::records::{StoredTaskResult, TaskOutcome, TaskPhase, TaskResult, TerminalUpdate};
use crate::store::{now_millis, Store};
use crate::tasks::query_task;

impl Store {
    pub fn store_task_result(&self, agent_id: &str, result: &TaskResult) -> StoreResult<()> {
        self.store_task_result_with_reason(agent_id, result, None)
    }

    /// Store an immutable terminal result and thread an explicit machine
    /// reason through the terminal ledger row. `reason` wins over the task's
    /// own `failure_code`; both yield to the compatibility placeholder only
    /// when the outcome is not `Completed`, so a successful terminal row keeps
    /// a NULL reason.
    pub fn store_task_result_with_reason(
        &self,
        agent_id: &str,
        result: &TaskResult,
        reason: Option<&str>,
    ) -> StoreResult<()> {
        validate_result(result)?;
        let canonical = task_result_bytes(result)?;
        let digest = task_result_digest(&canonical);
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = query_task(&transaction, agent_id)?
            .ok_or_else(|| StoreError::InvalidState(format!("unknown task {agent_id}")))?;
        if let Some(existing) = transaction
            .query_row(
                "SELECT result_sha256 FROM task_results WHERE agent_id=?1",
                [agent_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            if existing == digest {
                transaction.commit()?;
                return Ok(());
            }
            return Err(StoreError::Conflict(format!(
                "task {agent_id} already has a different immutable result"
            )));
        }
        if task.phase == TaskPhase::Terminal {
            return Err(StoreError::Conflict(format!(
                "task {agent_id} is terminal without this result"
            )));
        }
        if !matches!(
            task.phase,
            TaskPhase::Preparing
                | TaskPhase::Running
                | TaskPhase::WaitingInput
                | TaskPhase::Cancelling
        ) {
            return Err(StoreError::Conflict(format!(
                "task {agent_id} cannot complete from {:?}",
                task.phase
            )));
        }
        if result.outcome == TaskOutcome::Completed {
            let (pending, queued) = completion_blockers_tx(&transaction, agent_id)?;
            if pending || queued {
                return Err(StoreError::Conflict(
                    "task completion is blocked by pending input or queued messages".into(),
                ));
            }
        }
        if (task.stop_requested || task.close_requested) && result.outcome != TaskOutcome::Cancelled
        {
            return Err(StoreError::Conflict(
                "cancellation or close intent wins over late result".into(),
            ));
        }
        transaction.execute(
            "INSERT INTO task_results(agent_id,outcome,final_text,partial,result_sha256,completed_at)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                agent_id,
                result.outcome.as_str(),
                result.final_text,
                result.partial,
                digest,
                now_millis(),
            ],
        )?;
        apply_terminal(
            &transaction,
            agent_id,
            task.owner_epoch,
            task.phase,
            task.close_requested,
            task.stop_requested,
            &TerminalUpdate {
                outcome: result.outcome,
                failure_code: reason.map(str::to_owned).or(task.failure_code).or_else(|| {
                    (result.outcome != TaskOutcome::Completed).then(|| "task failed".to_string())
                }),
                failure_message: task.failure_message,
            },
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn task_result(&self, agent_id: &str) -> StoreResult<Option<StoredTaskResult>> {
        let connection = self.connection.lock().unwrap();
        query_task_result(&connection, agent_id)
    }

    /// The latest terminal ledger row's machine reason. Older-than-terminal
    /// rows (CLAIMED/RUNTIME_STARTED/...) are excluded by `to_phase`, so a
    /// failed-then-resumed-then-completed task yields the completed row's NULL
    /// (`None`) instead of the stale failure. A NULL row and the legacy
    /// `"task failed"` placeholder both read as `None`; a task without a
    /// terminal row also yields `None`.
    pub fn terminal_reason_code(&self, agent_id: &str) -> StoreResult<Option<String>> {
        let connection = self.connection.lock().unwrap();
        let reason = connection
            .query_row(
                "SELECT reason_code FROM lifecycle_ledger
                 WHERE agent_id=?1 AND to_phase='TERMINAL'
                 ORDER BY ledger_id DESC LIMIT 1",
                [agent_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(reason.flatten().filter(|value| value != "task failed"))
    }
}

pub(crate) fn task_result_bytes(result: &TaskResult) -> StoreResult<Vec<u8>> {
    serde_json::to_vec(result).map_err(|error| StoreError::InvalidState(error.to_string()))
}

pub(crate) fn task_result_digest(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}

fn validate_result(result: &TaskResult) -> StoreResult<()> {
    if result.final_text.trim().is_empty() {
        return Err(StoreError::InvalidState("invalid task result".into()));
    }
    if result.partial && result.outcome == TaskOutcome::Completed {
        return Err(StoreError::InvalidState(
            "partial result cannot be completed".into(),
        ));
    }
    Ok(())
}

fn query_task_result(
    connection: &Connection,
    agent_id: &str,
) -> StoreResult<Option<StoredTaskResult>> {
    let row = connection
        .query_row(
            "SELECT outcome,final_text,partial,result_sha256
             FROM task_results WHERE agent_id=?1",
            [agent_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    row.map(|row| {
        Ok(StoredTaskResult {
            result: TaskResult {
                outcome: TaskOutcome::parse(&row.0)?,
                final_text: row.1,
                partial: row.2 != 0,
            },
            result_sha256: row.3,
        })
    })
    .transpose()
}
