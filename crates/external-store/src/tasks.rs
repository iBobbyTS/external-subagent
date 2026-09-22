use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::error::{StoreError, StoreResult};
use crate::lifecycle::insert_ledger;
use crate::records::{
    NewTask, StoredProcessIdentity, TaskClaim, TaskOutcome, TaskPage, TaskPageFilter, TaskPhase,
    TaskQueryScope, TaskRecord, TaskWindowQuery, TurnState,
};
use crate::store::{i64_to_u64, now_millis, u64_to_i64, usize_to_i64, Store};

impl Store {
    /// Reserve the next public task identity. The increment is committed before
    /// preparation so a later filesystem failure can never cause reuse.
    pub fn reserve_task_id(&self) -> StoreResult<String> {
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next: i64 = transaction.query_row(
            "SELECT next_id FROM task_id_allocator WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        if !(10_000_000..=99_999_999).contains(&next) {
            return Err(StoreError::Conflict("task id allocator exhausted".into()));
        }
        transaction.execute(
            "UPDATE task_id_allocator SET next_id=?1 WHERE id=1",
            [next + 1],
        )?;
        transaction.commit()?;
        Ok(next.to_string())
    }

    /// Enqueue a fresh task. Submission is intentionally non-idempotent:
    /// agent-id and active-workspace collisions are conflicts, never reuse,
    /// so the only possible outcome is the newly created record.
    pub fn enqueue_task_authoritative(&self, task: &NewTask) -> StoreResult<TaskRecord> {
        validate_task(task)?;
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if query_task(&transaction, &task.agent_id)?.is_some() {
            return Err(StoreError::Conflict(format!(
                "agent id {} already exists",
                task.agent_id
            )));
        }
        // Workspace ownership is the collision boundary.  It applies to every
        // submission, regardless of launch metadata or repository identity;
        // terminal rows remain queryable and therefore do not block reuse.
        if let Some(active_agent_id) = transaction
            .query_row(
                "SELECT agent_id FROM tasks WHERE workspace_path=?1
             AND phase IN ('QUEUED','PREPARING','RUNNING','WAITING_INPUT','CANCELLING')
             ORDER BY created_at, rowid LIMIT 1",
                [&task.workspace_path],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return Err(StoreError::Conflict(format!(
                "WORKSPACE_BUSY active_agent_id={active_agent_id}"
            )));
        }
        let created_at = now_millis();
        transaction.execute(
            "INSERT INTO tasks (
                agent_id,repository,phase,workspace_path,runtime_hash,prepared_launch_json,
                initial_prompt,created_at
             ) VALUES (?1,?2,'QUEUED',?3,?4,?5,?6,?7)",
            params![
                task.agent_id,
                task.repository,
                task.workspace_path,
                task.runtime_hash,
                task.prepared_launch_json,
                task.initial_prompt,
                created_at,
            ],
        )?;
        insert_ledger(
            &transaction,
            &task.agent_id,
            0,
            None,
            TaskPhase::Queued,
            None,
            None,
        )?;
        let stored = query_task(&transaction, &task.agent_id)?
            .ok_or_else(|| StoreError::InvalidState("inserted task disappeared".into()))?;
        transaction.commit()?;
        Ok(stored)
    }

    pub fn get_task(&self, agent_id: &str) -> StoreResult<Option<TaskRecord>> {
        let connection = self.connection.lock().unwrap();
        query_task(&connection, agent_id)
    }

    pub fn get_task_scoped(
        &self,
        agent_id: &str,
        scope: TaskQueryScope<'_>,
    ) -> StoreResult<Option<TaskRecord>> {
        validate_scope(&scope)?;
        let connection = self.connection.lock().unwrap();
        let found = connection
            .query_row(
                "SELECT agent_id FROM tasks
                 WHERE agent_id=?1
                   AND (?2 IS NULL OR repository=?2)
                   ",
                params![agent_id, scope.repository],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        found
            .map(|id| query_task(&connection, &id))
            .transpose()
            .map(Option::flatten)
    }

    pub fn list_task_page(
        &self,
        scope: TaskQueryScope<'_>,
        filter: TaskPageFilter,
        cursor: Option<u64>,
        limit: usize,
    ) -> StoreResult<TaskPage> {
        validate_scope(&scope)?;
        if limit == 0 {
            return Err(StoreError::InvalidState(
                "task page limit must be positive".into(),
            ));
        }
        let connection = self.connection.lock().unwrap();
        let mut statement = connection.prepare(
            "SELECT rowid,agent_id FROM tasks
             WHERE (?1 IS NULL OR repository=?1)
               AND (?2 IS NULL OR phase=?2)
               AND (?3 IS NULL OR outcome=?3)
               AND (?4 IS NULL OR rowid < ?4)
               AND (?6 IS NULL OR CASE WHEN json_valid(prepared_launch_json) THEN json_extract(prepared_launch_json, '$.admission.agent') END = ?6)
             ORDER BY rowid DESC LIMIT ?5",
        )?;
        let rows = statement
            .query_map(
                params![
                    scope.repository,
                    filter.phase.map(TaskPhase::as_str),
                    filter.outcome.map(TaskOutcome::as_str),
                    cursor.map(u64_to_i64).transpose()?,
                    usize_to_i64(limit.saturating_add(1))?,
                    filter.agent,
                ],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let has_more = rows.len() > limit;
        let selected = rows.into_iter().take(limit).collect::<Vec<_>>();
        let next_cursor = has_more
            .then(|| selected.last().map(|(rowid, _)| i64_to_u64(*rowid)))
            .flatten()
            .transpose()?;
        let tasks = selected
            .into_iter()
            .map(|(_, id)| {
                query_task(&connection, &id)?
                    .ok_or_else(|| StoreError::InvalidState("listed task disappeared".into()))
            })
            .collect::<StoreResult<Vec<_>>>()?;
        Ok(TaskPage { tasks, next_cursor })
    }

    pub fn list_task_window(&self, query: &TaskWindowQuery) -> StoreResult<Vec<TaskRecord>> {
        if query.agent_id.is_none() && query.workspace_path.is_none() {
            return Err(StoreError::InvalidState(
                "agent_id or workspace_path query scope is required".into(),
            ));
        }
        if let (Some(start), Some(end)) = (query.start_ms, query.end_ms) {
            if start > end {
                return Err(StoreError::InvalidState(
                    "query start_ms must be less than or equal to end_ms".into(),
                ));
            }
        }
        let connection = self.connection.lock().unwrap();
        let mut statement = connection.prepare(
            "SELECT agent_id FROM tasks
             WHERE (?1 IS NULL OR agent_id=?1)
               AND (?2 IS NULL OR workspace_path=?2)
               AND (?3 IS NULL OR created_at>=?3)
               AND (?4 IS NULL OR (completed_at IS NOT NULL AND completed_at<=?4))
             ORDER BY created_at, agent_id",
        )?;
        let ids = statement
            .query_map(
                params![
                    query.agent_id,
                    query.workspace_path,
                    query.start_ms,
                    query.end_ms
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| {
                query_task(&connection, &id)?
                    .ok_or_else(|| StoreError::InvalidState("task disappeared during query".into()))
            })
            .collect()
    }

    pub fn claim_next(
        &self,
        owner_id: &str,
        global_limit: usize,
        per_workspace_limit: usize,
    ) -> StoreResult<Option<TaskClaim>> {
        // The product contract deliberately has no process-wide agent cap.
        // Keep the parameter for source compatibility with older daemon
        // callers, but do not use it as an admission gate.  Concurrency is
        // scoped to the canonical repository/workspace below.
        let _ = global_limit;
        if per_workspace_limit == 0 {
            return Ok(None);
        }
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidate = transaction
            .query_row(
                "SELECT agent_id FROM tasks queued
                 WHERE phase='QUEUED' AND close_requested=0 AND stop_requested=0
                   AND (SELECT COUNT(*) FROM tasks active
                        WHERE active.repository=queued.repository
                          AND active.phase IN ('PREPARING','RUNNING','WAITING_INPUT','CANCELLING')) < ?1
                 ORDER BY queued.created_at,queued.rowid LIMIT 1",
                [usize_to_i64(per_workspace_limit)?],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(agent_id) = candidate else {
            transaction.commit()?;
            return Ok(None);
        };
        let now = now_millis();
        let changed = transaction.execute(
            "UPDATE tasks SET phase='PREPARING',owner_id=?1,owner_epoch=owner_epoch+1,
                 started_at=COALESCE(started_at,?2),last_heartbeat_at=?2
             WHERE agent_id=?3 AND phase='QUEUED' AND close_requested=0 AND stop_requested=0",
            params![owner_id, now, agent_id],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(format!(
                "task {agent_id} lost its queue claim"
            )));
        }
        let task = query_task(&transaction, &agent_id)?
            .ok_or_else(|| StoreError::InvalidState("claimed task disappeared".into()))?;
        insert_ledger(
            &transaction,
            &agent_id,
            task.owner_epoch,
            Some(TaskPhase::Queued),
            TaskPhase::Preparing,
            None,
            Some("CLAIMED"),
        )?;
        transaction.commit()?;
        Ok(Some(TaskClaim {
            owner_epoch: task.owner_epoch,
            task,
        }))
    }

    pub fn mark_session_running(
        &self,
        agent_id: &str,
        owner_epoch: u64,
        runtime_agent_id: &str,
        identity: Option<&StoredProcessIdentity>,
        session_id: Option<&str>,
        turn_state: Option<TurnState>,
    ) -> StoreResult<bool> {
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE tasks SET phase='RUNNING',runtime_agent_id=?1,pid=?2,process_group_id=?3,
                 process_uid=?4,process_start_token=?5,last_heartbeat_at=?6,
                 session_id=COALESCE(?7,session_id),turn_state=COALESCE(?8,turn_state)
             WHERE agent_id=?9 AND owner_epoch=?10 AND phase='PREPARING'",
            params![
                runtime_agent_id,
                identity.map(|value| value.pid),
                identity.map(|value| value.process_group_id),
                identity.map(|value| value.uid),
                identity.map(|value| value.start_token.as_str()),
                now_millis(),
                session_id,
                turn_state.map(TurnState::as_str),
                agent_id,
                u64_to_i64(owner_epoch)?,
            ],
        )?;
        if changed == 1 {
            insert_ledger(
                &transaction,
                agent_id,
                owner_epoch,
                Some(TaskPhase::Preparing),
                TaskPhase::Running,
                None,
                Some("RUNTIME_STARTED"),
            )?;
        }
        transaction.commit()?;
        Ok(changed == 1)
    }

    pub fn active_count(&self) -> StoreResult<u64> {
        let connection = self.connection.lock().unwrap();
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM tasks WHERE phase IN ('PREPARING','RUNNING','WAITING_INPUT','CANCELLING')",
            [],
            |row| row.get(0),
        )?;
        i64_to_u64(count)
    }

    /// Activation requires no nonterminal task and a cleanup receipt for every terminal task.
    pub fn all_tasks_reaped(&self) -> StoreResult<bool> {
        let connection = self.connection.lock().unwrap();
        let pending: i64 = connection.query_row(
            "SELECT COUNT(*) FROM tasks WHERE phase!='TERMINAL' OR reaped_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(pending == 0)
    }
}

fn validate_task(task: &NewTask) -> StoreResult<()> {
    for (name, value) in [
        ("agent_id", task.agent_id.as_str()),
        ("repository", task.repository.as_str()),
        ("workspace_path", task.workspace_path.as_str()),
        ("prepared_launch_json", task.prepared_launch_json.as_str()),
        ("initial_prompt", task.initial_prompt.as_str()),
    ] {
        if value.trim().is_empty() || value.contains('\0') {
            return Err(StoreError::InvalidState(format!("{name} is invalid")));
        }
    }
    Ok(())
}

fn validate_scope(scope: &TaskQueryScope<'_>) -> StoreResult<()> {
    if scope.repository.is_none() {
        Err(StoreError::InvalidState(
            "repository or group task scope is required".into(),
        ))
    } else {
        Ok(())
    }
}

type TaskRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    i64,
    i64,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    i64,
    i64,
);

pub(crate) fn query_task(
    connection: &Connection,
    agent_id: &str,
) -> StoreResult<Option<TaskRecord>> {
    let row = connection
        .query_row(
            "SELECT agent_id,repository,phase,outcome,
                    workspace_path,runtime_hash,prepared_launch_json,
                    initial_prompt,owner_id,owner_epoch,
                    close_requested,stop_requested,failure_code,failure_message,runtime_agent_id,
                    session_id,turn_state,pid,process_group_id,process_uid,process_start_token,
                    closed_at,reaped_at,created_at,last_event_seq
             FROM tasks WHERE agent_id=?1",
            [agent_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                    row.get(14)?,
                    row.get(15)?,
                    row.get(16)?,
                    row.get(17)?,
                    row.get(18)?,
                    row.get(19)?,
                    row.get(20)?,
                    row.get(21)?,
                    row.get(22)?,
                    row.get(23)?,
                    row.get(24)?,
                ))
            },
        )
        .optional()?;
    row.map(convert_task_row).transpose()
}

fn convert_task_row(row: TaskRow) -> StoreResult<TaskRecord> {
    let process_identity = match (row.17, row.18, row.19, row.20) {
        (Some(pid), Some(process_group_id), Some(uid), Some(start_token)) => {
            Some(StoredProcessIdentity {
                pid: u32::try_from(pid)
                    .map_err(|_| StoreError::InvalidState("stored pid is invalid".into()))?,
                process_group_id: i32::try_from(process_group_id).map_err(|_| {
                    StoreError::InvalidState("stored process group is invalid".into())
                })?,
                uid: u32::try_from(uid)
                    .map_err(|_| StoreError::InvalidState("stored uid is invalid".into()))?,
                start_token,
            })
        }
        (None, None, None, None) => None,
        _ => {
            return Err(StoreError::InvalidState(
                "stored process identity is incomplete".into(),
            ))
        }
    };
    Ok(TaskRecord {
        agent_id: row.0,
        repository: row.1,
        phase: TaskPhase::parse(&row.2)?,
        outcome: row.3.map(|value| TaskOutcome::parse(&value)).transpose()?,
        workspace_path: row.4,
        runtime_hash: row.5,
        prepared_launch_json: row.6,
        initial_prompt: row.7,
        owner_id: row.8,
        owner_epoch: i64_to_u64(row.9)?,
        close_requested: row.10 != 0,
        stop_requested: row.11 != 0,
        failure_code: row.12,
        failure_message: row.13,
        runtime_agent_id: row.14,
        session_id: row.15,
        turn_state: TurnState::parse(&row.16)?,
        process_identity,
        closed_at: row.21,
        reaped_at: row.22,
        created_at: row.23,
        last_event_seq: i64_to_u64(row.24)?,
    })
}

pub(crate) fn query_guard(
    transaction: &Transaction<'_>,
    agent_id: &str,
) -> StoreResult<(TaskPhase, Option<TaskOutcome>, u64, bool, bool)> {
    let value = transaction
        .query_row(
            "SELECT phase,outcome,owner_epoch,close_requested,stop_requested
             FROM tasks WHERE agent_id=?1",
            [agent_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::InvalidState(format!("unknown task {agent_id}")))?;
    Ok((
        TaskPhase::parse(&value.0)?,
        value
            .1
            .map(|value| TaskOutcome::parse(&value))
            .transpose()?,
        i64_to_u64(value.2)?,
        value.3 != 0,
        value.4 != 0,
    ))
}
