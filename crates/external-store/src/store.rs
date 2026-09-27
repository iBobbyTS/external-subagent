use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
#[cfg(test)]
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{StoreError, StoreResult};
use crate::schema::initialize_schema;

const STORE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Store {
    pub(crate) connection: Mutex<Connection>,
    database_path: PathBuf,
    /// Test-only seam invoked by [`Store::task_with_result`] between its two
    /// reads while the connection lock is held. It exists so a test can prove
    /// no concurrent writer can commit inside the snapshot window.
    #[cfg(test)]
    pub(crate) snapshot_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> StoreResult<Self> {
        let mut connection = Connection::open(path.as_ref())?;
        connection.busy_timeout(STORE_BUSY_TIMEOUT)?;
        initialize_schema(&mut connection)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        let database_path = std::fs::canonicalize(path.as_ref()).map_err(|error| {
            StoreError::InvalidState(format!("database path cannot be canonicalized: {error}"))
        })?;
        Ok(Self {
            connection: Mutex::new(connection),
            database_path,
            #[cfg(test)]
            snapshot_hook: Mutex::new(None),
        })
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }
    pub fn journal_mode(&self) -> StoreResult<String> {
        let connection = self.connection.lock().unwrap();
        Ok(connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?)
    }

    #[cfg(test)]
    pub(crate) fn set_snapshot_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.snapshot_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    pub(crate) fn clear_snapshot_hook(&self) {
        *self.snapshot_hook.lock().unwrap() = None;
    }

    #[cfg(test)]
    pub(crate) fn run_snapshot_hook(&self) {
        if let Some(hook) = self.snapshot_hook.lock().unwrap().clone() {
            hook();
        }
    }
}

pub(crate) fn usize_to_i64(value: usize) -> StoreResult<i64> {
    i64::try_from(value).map_err(|_| StoreError::InvalidState("value exceeds SQLite i64".into()))
}

pub(crate) fn u64_to_i64(value: u64) -> StoreResult<i64> {
    i64::try_from(value).map_err(|_| StoreError::InvalidState("value exceeds SQLite i64".into()))
}

pub(crate) fn i64_to_u64(value: i64) -> StoreResult<u64> {
    u64::try_from(value).map_err(|_| StoreError::InvalidState("negative SQLite value".into()))
}

pub(crate) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}
