use rusqlite::{Connection, TransactionBehavior};

use crate::error::{StoreError, StoreResult};

const SCHEMA_VERSION: i64 = 14;

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE tasks (
    agent_id TEXT PRIMARY KEY,
    repository TEXT NOT NULL,
    phase TEXT NOT NULL,
    outcome TEXT,
    workspace_path TEXT NOT NULL,
    runtime_hash TEXT,
    prepared_launch_json TEXT NOT NULL,
    initial_prompt TEXT NOT NULL,
    session_id TEXT,
    turn_state TEXT NOT NULL DEFAULT 'IDLE',
    pid INTEGER,
    process_group_id INTEGER,
    process_uid INTEGER,
    process_start_token TEXT,
    runtime_agent_id TEXT,
    owner_id TEXT,
    owner_epoch INTEGER NOT NULL DEFAULT 0,
    lease_expires_at INTEGER,
    close_requested INTEGER NOT NULL DEFAULT 0,
    stop_requested INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    completed_at INTEGER,
    last_heartbeat_at INTEGER,
    last_event_seq INTEGER NOT NULL DEFAULT 0,
    failure_code TEXT,
    failure_message TEXT,
    closed_at INTEGER,
    reaped_at INTEGER,
    CHECK ((phase = 'TERMINAL') = (outcome IS NOT NULL))
);
CREATE INDEX tasks_queue_idx ON tasks(phase, created_at, agent_id);
CREATE INDEX tasks_workspace_phase_idx ON tasks(workspace_path, phase);
CREATE INDEX tasks_scope_idx ON tasks(repository, phase, created_at);

CREATE TABLE events (
    agent_id TEXT NOT NULL REFERENCES tasks(agent_id) ON DELETE CASCADE,
    runtime_agent_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    source_seq INTEGER NOT NULL,
    timestamp INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    turn_id TEXT,
    payload_json TEXT NOT NULL,
    redaction_level TEXT NOT NULL,
    PRIMARY KEY (agent_id, runtime_agent_id, seq),
    UNIQUE (agent_id, runtime_agent_id, source_seq)
);

CREATE TABLE messages (
    message_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL REFERENCES tasks(agent_id) ON DELETE CASCADE,
    mode TEXT NOT NULL,
    content TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    delivered_at INTEGER,
    target_turn_id TEXT,
    failure_code TEXT,
    failure_message TEXT
);

CREATE TABLE pending_requests (
    request_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL REFERENCES tasks(agent_id) ON DELETE CASCADE,
    correlation_id TEXT NOT NULL,
    request_type TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    responded_at INTEGER,
    response_decision TEXT,
    response_content TEXT,
    UNIQUE (agent_id, correlation_id)
);

CREATE TABLE task_results (
    agent_id TEXT PRIMARY KEY REFERENCES tasks(agent_id) ON DELETE CASCADE,
    outcome TEXT NOT NULL,
    final_text TEXT NOT NULL,
    partial INTEGER NOT NULL,
    result_sha256 TEXT NOT NULL,
    completed_at INTEGER NOT NULL
);

CREATE TABLE lifecycle_ledger (
    ledger_id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL REFERENCES tasks(agent_id) ON DELETE CASCADE,
    owner_epoch INTEGER NOT NULL,
    from_phase TEXT,
    to_phase TEXT NOT NULL,
    outcome TEXT,
    reason_code TEXT,
    recorded_at INTEGER NOT NULL
);
CREATE TABLE task_id_allocator (id INTEGER PRIMARY KEY CHECK (id = 1), next_id INTEGER NOT NULL);
INSERT INTO task_id_allocator(id, next_id) VALUES (1, 10000000);
"#;

pub(crate) fn initialize_schema(connection: &mut Connection) -> StoreResult<()> {
    // Lock before reading the version so concurrent openers cannot both migrate v12.
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let user_tables: u64 = transaction.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    if user_tables != 0 {
        if !matches!(version, 12 | 13 | SCHEMA_VERSION) || !schema_is_current(&transaction)? {
            return Err(StoreError::LegacySchemaUnsupported);
        }
        if version == 12 {
            // v12 carried the historical column name; fold that rename and the
            // v14 column removal into one transaction so the database either
            // reaches the current shape or keeps its original bytes.
            transaction.execute_batch(concat!(
                "ALTER TABLE tasks RENAME COLUMN ",
                "zcode_",
                "session_id TO session_id"
            ))?;
        }
        if version != SCHEMA_VERSION {
            transaction.execute_batch("ALTER TABLE tasks DROP COLUMN prepared_launch_sha256")?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
    } else {
        if version != 0 {
            return Err(StoreError::LegacySchemaUnsupported);
        }
        transaction.execute_batch(SCHEMA)?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    transaction.commit()?;
    Ok(())
}

pub(crate) fn schema_is_current(connection: &Connection) -> StoreResult<bool> {
    let expected = [
        "events",
        "lifecycle_ledger",
        "messages",
        "pending_requests",
        "task_id_allocator",
        "task_results",
        "tasks",
    ];
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let actual = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(actual == expected)
}
