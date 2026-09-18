use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::error::{StoreError, StoreResult};
use crate::lifecycle::insert_ledger;
use crate::records::{
    PendingRequestState, PendingResponseClaimDisposition, StoredPendingRequest, TaskPhase,
};
use crate::store::{now_millis, usize_to_i64, Store};
use crate::tasks::query_guard;

impl Store {
    pub fn insert_pending_request(
        &self,
        request_id: &str,
        agent_id: &str,
        correlation_id: &str,
        request_type: &str,
        payload_json: &str,
    ) -> StoreResult<bool> {
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (phase, _, owner_epoch, close_requested, stop_requested) =
            query_guard(&transaction, agent_id)?;
        if !matches!(phase, TaskPhase::Running | TaskPhase::WaitingInput)
            || close_requested
            || stop_requested
        {
            return Err(StoreError::Conflict(
                "task cannot publish a pending request in its current phase".into(),
            ));
        }
        let changed = transaction.execute(
            "INSERT OR IGNORE INTO pending_requests(request_id,agent_id,correlation_id,
                 request_type,payload_json,state,created_at)
             VALUES (?1,?2,?3,?4,?5,'PENDING',?6)",
            params![
                request_id,
                agent_id,
                correlation_id,
                request_type,
                payload_json,
                now_millis()
            ],
        )?;
        if changed == 0 {
            let existing = query_pending_request(&transaction, request_id)?.ok_or_else(|| {
                StoreError::InvalidState("pending request idempotency row disappeared".into())
            })?;
            if existing.agent_id != agent_id
                || existing.correlation_id != correlation_id
                || existing.request_type != request_type
                || existing.payload_json != payload_json
            {
                return Err(StoreError::Conflict(
                    "pending request identity has different content".into(),
                ));
            }
        }
        if changed == 1 && phase == TaskPhase::Running {
            transaction.execute(
                "UPDATE tasks SET phase='WAITING_INPUT' WHERE agent_id=?1 AND phase='RUNNING'",
                [agent_id],
            )?;
            insert_ledger(
                &transaction,
                agent_id,
                owner_epoch,
                Some(TaskPhase::Running),
                TaskPhase::WaitingInput,
                None,
                Some("INPUT_REQUESTED"),
            )?;
        }
        transaction.commit()?;
        Ok(changed == 1)
    }

    pub fn pending_request(
        &self,
        agent_id: &str,
        request_id: &str,
    ) -> StoreResult<Option<StoredPendingRequest>> {
        let connection = self.connection.lock().unwrap();
        let request = query_pending_request(&connection, request_id)?;
        Ok(request.filter(|request| request.agent_id == agent_id))
    }

    pub fn pending_requests(&self, agent_id: &str) -> StoreResult<Vec<StoredPendingRequest>> {
        self.pending_requests_bounded(agent_id, i64::MAX as usize)
    }

    pub fn pending_requests_bounded(
        &self,
        agent_id: &str,
        limit: usize,
    ) -> StoreResult<Vec<StoredPendingRequest>> {
        let connection = self.connection.lock().unwrap();
        let mut statement = connection.prepare(
            "SELECT request_id FROM pending_requests
             WHERE agent_id=?1 AND state IN ('PENDING','SENDING')
             ORDER BY created_at,rowid LIMIT ?2",
        )?;
        let ids = statement
            .query_map(params![agent_id, usize_to_i64(limit)?], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| {
                query_pending_request(&connection, &id)?
                    .ok_or_else(|| StoreError::InvalidState("pending request disappeared".into()))
            })
            .collect()
    }

    pub fn completion_blockers(&self, agent_id: &str) -> StoreResult<(bool, bool)> {
        let connection = self.connection.lock().unwrap();
        completion_blockers_tx(&connection, agent_id)
    }

    pub fn claim_pending_response_if_accepting(
        &self,
        agent_id: &str,
        request_id: &str,
        decision: &str,
        content: Option<&str>,
    ) -> StoreResult<PendingResponseClaimDisposition> {
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(request) = query_pending_request(&transaction, request_id)? else {
            transaction.commit()?;
            return Ok(PendingResponseClaimDisposition::NotFound);
        };
        if request.agent_id != agent_id {
            transaction.commit()?;
            return Ok(PendingResponseClaimDisposition::NotFound);
        }
        if request.state != PendingRequestState::Pending {
            transaction.commit()?;
            return Ok(PendingResponseClaimDisposition::NotPending(request.state));
        }
        let (phase, _, _, close_requested, stop_requested) = query_guard(&transaction, agent_id)?;
        if !matches!(phase, TaskPhase::Running | TaskPhase::WaitingInput)
            || close_requested
            || stop_requested
        {
            transaction.commit()?;
            return Ok(PendingResponseClaimDisposition::TaskStopping);
        }
        transaction.execute(
            "UPDATE pending_requests SET state='SENDING',response_decision=?1,response_content=?2
             WHERE request_id=?3 AND agent_id=?4 AND state='PENDING'",
            params![decision, content, request_id, agent_id],
        )?;
        transaction.commit()?;
        Ok(PendingResponseClaimDisposition::Claimed)
    }

    pub fn complete_pending_response(&self, agent_id: &str, request_id: &str) -> StoreResult<bool> {
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE pending_requests SET state='RESPONDED',responded_at=?1
             WHERE request_id=?2 AND agent_id=?3 AND state='SENDING'",
            params![now_millis(), request_id, agent_id],
        )? == 1;
        if changed {
            let remaining: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM pending_requests
                 WHERE agent_id=?1 AND state IN ('PENDING','SENDING'))",
                [agent_id],
                |row| row.get(0),
            )?;
            let (phase, _, owner_epoch, close_requested, stop_requested) =
                query_guard(&transaction, agent_id)?;
            if !remaining && phase == TaskPhase::WaitingInput && !close_requested && !stop_requested
            {
                transaction.execute(
                    "UPDATE tasks SET phase='RUNNING'
                     WHERE agent_id=?1 AND phase='WAITING_INPUT'",
                    [agent_id],
                )?;
                insert_ledger(
                    &transaction,
                    agent_id,
                    owner_epoch,
                    Some(TaskPhase::WaitingInput),
                    TaskPhase::Running,
                    None,
                    Some("INPUT_RESOLVED"),
                )?;
            }
        }
        transaction.commit()?;
        Ok(changed)
    }

    pub fn release_pending_response(&self, agent_id: &str, request_id: &str) -> StoreResult<bool> {
        let connection = self.connection.lock().unwrap();
        Ok(connection.execute(
            "UPDATE pending_requests SET state='PENDING',response_decision=NULL,response_content=NULL
             WHERE request_id=?1 AND agent_id=?2 AND state='SENDING'",
            params![request_id, agent_id],
        )? == 1)
    }
}

pub(crate) fn completion_blockers_tx(
    connection: &Connection,
    agent_id: &str,
) -> StoreResult<(bool, bool)> {
    let pending: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pending_requests WHERE agent_id=?1 AND state IN ('PENDING','SENDING'))",
        [agent_id],
        |row| row.get(0),
    )?;
    let queued: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM messages WHERE agent_id=?1 AND state IN ('QUEUED','SENDING'))",
        [agent_id],
        |row| row.get(0),
    )?;
    Ok((pending, queued))
}

fn query_pending_request(
    connection: &Connection,
    request_id: &str,
) -> StoreResult<Option<StoredPendingRequest>> {
    let row = connection
        .query_row(
            "SELECT request_id,agent_id,correlation_id,request_type,payload_json,state,
                    response_decision,response_content,created_at
             FROM pending_requests WHERE request_id=?1",
            [request_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            },
        )
        .optional()?;
    row.map(|row| {
        let state = match row.5.as_str() {
            "PENDING" => PendingRequestState::Pending,
            "SENDING" => PendingRequestState::Sending,
            "RESPONDED" => PendingRequestState::Responded,
            other => {
                return Err(StoreError::InvalidState(format!(
                    "unknown pending request state {other}"
                )))
            }
        };
        Ok(StoredPendingRequest {
            request_id: row.0,
            agent_id: row.1,
            correlation_id: row.2,
            request_type: row.3,
            payload_json: row.4,
            state,
            response_decision: row.6,
            response_content: row.7,
            created_at: row.8,
        })
    })
    .transpose()
}
