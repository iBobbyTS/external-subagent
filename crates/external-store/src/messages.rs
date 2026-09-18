use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::error::{StoreError, StoreResult};
use crate::records::{MessageState, StoredMessage, TaskPhase};
use crate::store::{now_millis, Store};
use crate::tasks::query_guard;

impl Store {
    pub fn insert_message(
        &self,
        message_id: &str,
        agent_id: &str,
        mode: &str,
        content: &str,
    ) -> StoreResult<bool> {
        if mode != "queue" {
            return Err(StoreError::InvalidState(
                "only queue message mode is supported".into(),
            ));
        }
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (phase, _, _, close_requested, stop_requested) = query_guard(&transaction, agent_id)?;
        if phase == TaskPhase::Terminal || close_requested || stop_requested {
            return Err(StoreError::Conflict(
                "terminal or stopping task cannot accept messages".into(),
            ));
        }
        let changed = transaction.execute(
            "INSERT OR IGNORE INTO messages(message_id,agent_id,mode,content,state,created_at)
             VALUES (?1,?2,?3,?4,'QUEUED',?5)",
            params![message_id, agent_id, mode, content, now_millis()],
        )?;
        if changed == 0 {
            let existing = query_message(&transaction, message_id)?.ok_or_else(|| {
                StoreError::InvalidState("message idempotency row disappeared".into())
            })?;
            if existing.agent_id != agent_id || existing.mode != mode || existing.content != content
            {
                return Err(StoreError::Conflict(
                    "message id is already bound to different content".into(),
                ));
            }
        }
        transaction.commit()?;
        Ok(changed == 1)
    }

    pub fn message(&self, message_id: &str) -> StoreResult<Option<StoredMessage>> {
        let connection = self.connection.lock().unwrap();
        query_message(&connection, message_id)
    }

    pub fn claim_next_message(&self, agent_id: &str) -> StoreResult<Option<StoredMessage>> {
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (phase, _, _, close_requested, stop_requested) = query_guard(&transaction, agent_id)?;
        if phase != TaskPhase::Running || close_requested || stop_requested {
            transaction.commit()?;
            return Ok(None);
        }
        let id = transaction
            .query_row(
                "SELECT message_id FROM messages WHERE agent_id=?1 AND state='QUEUED'
                 ORDER BY created_at,rowid LIMIT 1",
                [agent_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(id) = id else {
            transaction.commit()?;
            return Ok(None);
        };
        transaction.execute(
            "UPDATE messages SET state='SENDING' WHERE message_id=?1 AND state='QUEUED'",
            [&id],
        )?;
        let message = query_message(&transaction, &id)?;
        transaction.commit()?;
        Ok(message)
    }

    pub fn complete_message(
        &self,
        message_id: &str,
        target_turn_id: Option<&str>,
    ) -> StoreResult<bool> {
        let connection = self.connection.lock().unwrap();
        Ok(connection.execute(
            "UPDATE messages SET state='DELIVERED',delivered_at=?1,target_turn_id=?2
             WHERE message_id=?3 AND state='SENDING'",
            params![now_millis(), target_turn_id, message_id],
        )? == 1)
    }

    pub fn fail_message(&self, message_id: &str, code: &str, message: &str) -> StoreResult<bool> {
        let connection = self.connection.lock().unwrap();
        Ok(connection.execute(
            "UPDATE messages SET state='FAILED',failure_code=?1,failure_message=?2
             WHERE message_id=?3 AND state IN ('QUEUED','SENDING')",
            params![code, message, message_id],
        )? == 1)
    }
}

fn query_message(connection: &Connection, message_id: &str) -> StoreResult<Option<StoredMessage>> {
    let row = connection
        .query_row(
            "SELECT message_id,agent_id,mode,content,state,target_turn_id,failure_code,created_at,delivered_at
             FROM messages WHERE message_id=?1",
            [message_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?, row.get::<_, i64>(7)?, row.get::<_, Option<i64>>(8)?,
                ))
            },
        )
        .optional()?;
    row.map(|row| {
        let state = match row.4.as_str() {
            "QUEUED" => MessageState::Queued,
            "SENDING" => MessageState::Sending,
            "DELIVERED" => MessageState::Delivered,
            "FAILED" => MessageState::Failed,
            other => {
                return Err(StoreError::InvalidState(format!(
                    "unknown message state {other}"
                )))
            }
        };
        Ok(StoredMessage {
            message_id: row.0,
            agent_id: row.1,
            mode: row.2,
            content: row.3,
            state,
            target_turn_id: row.5,
            failure_code: row.6,
            created_at: row.7,
            delivered_at: row.8,
        })
    })
    .transpose()
}
