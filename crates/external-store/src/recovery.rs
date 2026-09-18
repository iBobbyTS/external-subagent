use rusqlite::TransactionBehavior;

use crate::error::{StoreError, StoreResult};
use crate::records::TaskRecord;
use crate::store::Store;
use crate::tasks::query_task;

impl Store {
    /// Persist queued cancellation intent before any running provider is
    /// awaited. This UPDATE and claim_next serialize on the same database
    /// transaction owner; an unclaimed queued task cannot start afterwards.
    /// Keep QUEUED until ordinary cancel_task settles its unstarted lifecycle.
    pub fn fence_queued_cancellation(&self) -> StoreResult<()> {
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("UPDATE tasks SET stop_requested=1 WHERE phase='QUEUED'", [])?;
        transaction.commit()?;
        Ok(())
    }

    /// Daemon management snapshot after admission closes. Unlike startup
    /// recovery this includes queued tasks which explicit drain cancellation
    /// must settle without starting a provider.
    pub fn nonterminal_task_ids(&self) -> StoreResult<Vec<String>> {
        let connection = self.connection.lock().unwrap();
        let mut statement = connection.prepare(
            "SELECT agent_id FROM tasks WHERE phase != 'TERMINAL' ORDER BY created_at,rowid",
        )?;
        let ids = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(ids)
    }

    pub fn startup_recovery_tasks(&self) -> StoreResult<Vec<TaskRecord>> {
        let connection = self.connection.lock().unwrap();
        let mut statement = connection.prepare(
            "SELECT agent_id FROM tasks
             WHERE phase IN ('PREPARING','RUNNING','WAITING_INPUT','CANCELLING')
                OR (phase='QUEUED' AND stop_requested=1)
                OR (phase='TERMINAL' AND reaped_at IS NULL)
             ORDER BY created_at,rowid",
        )?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|agent_id| {
                query_task(&connection, &agent_id)?.ok_or_else(|| {
                    StoreError::InvalidState("startup recovery task disappeared".into())
                })
            })
            .collect()
    }
}
