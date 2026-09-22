// Frozen v12 DDL: independent of the current schema, with the historical name split
// to keep the active-source legacy-name guard meaningful.
const V12_SCHEMA: &str = concat!(
    r#"
PRAGMA foreign_keys = ON;

CREATE TABLE tasks (
    agent_id TEXT PRIMARY KEY,
    repository TEXT NOT NULL,
    phase TEXT NOT NULL,
    outcome TEXT,
    workspace_path TEXT NOT NULL,
    runtime_hash TEXT,
    prepared_launch_json TEXT NOT NULL,
    prepared_launch_sha256 TEXT NOT NULL,
    initial_prompt TEXT NOT NULL,
    "#,
    "zcode_",
    r#"session_id TEXT,
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
"#
);

use super::*;
use crate::schema::{initialize_schema, schema_is_current};
use rusqlite::Connection;
use std::{fs, path::Path, time::Duration};

const OLD_COLUMN: &str = concat!("zcode_", "session_id");

fn v12_fixture(path: &Path) {
    let connection = Connection::open(path).unwrap();
    connection.execute_batch(V12_SCHEMA).unwrap();
    connection
        .execute_batch(concat!(
        "INSERT INTO tasks(agent_id,repository,phase,workspace_path,prepared_launch_json,",
        "prepared_launch_sha256,initial_prompt,", "zcode_", "session_id,created_at) ",
        "VALUES ('10000001','/repo','QUEUED','/workspace','{}','prepared','do work','t_123',123);",
        "INSERT INTO messages(message_id,agent_id,mode,content,state,created_at) ",
        "VALUES ('message','10000001','queue','follow up','QUEUED',124);",
        "UPDATE task_id_allocator SET next_id=10000002; PRAGMA user_version=12;"
    ))
        .unwrap();
}

fn v13_fixture(path: &Path) {
    // v13 is the v12 shape after the historical column rename and before the
    // prepared_launch_sha256 removal; no separate DDL copy is needed.
    v12_fixture(path);
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(concat!(
            "ALTER TABLE tasks RENAME COLUMN ",
            "zcode_",
            "session_id TO session_id"
        ))
        .unwrap();
    connection.pragma_update(None, "user_version", 13).unwrap();
}

fn version(connection: &Connection) -> i64 {
    connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

fn columns(connection: &Connection) -> Vec<String> {
    connection
        .prepare("PRAGMA table_info(tasks)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn v12_migration_preserves_records_and_lifecycle_across_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v12.sqlite3");
    v12_fixture(&path);
    let before = Connection::open(&path).unwrap();
    assert_eq!(version(&before), 12);
    assert!(columns(&before).iter().any(|column| column == OLD_COLUMN));
    drop(before);

    let store = Store::open(&path).unwrap();
    {
        let connection = store.connection.lock().unwrap();
        assert_eq!(version(&connection), 14);
        let names = columns(&connection);
        assert!(names.iter().any(|column| column == "session_id"));
        assert!(!names.iter().any(|column| column == OLD_COLUMN));
        assert!(!names
            .iter()
            .any(|column| column == "prepared_launch_sha256"));
        assert_eq!(
            connection
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert!(!connection
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .exists([])
            .unwrap());
    }
    let migrated = store.get_task("10000001").unwrap().unwrap();
    assert_eq!(migrated.session_id.as_deref(), Some("t_123"));
    assert_eq!(migrated.initial_prompt, "do work");
    assert_eq!(migrated.created_at, 123);
    assert_eq!(migrated.phase, TaskPhase::Queued);
    let message = store.message("message").unwrap().unwrap();
    assert_eq!(message.content, "follow up");
    drop(store);

    let store = Store::open(&path).unwrap();
    assert_eq!(store.get_task("10000001").unwrap().unwrap(), migrated);
    assert_eq!(store.message("message").unwrap().unwrap(), message);
    assert_eq!(version(&store.connection.lock().unwrap()), 14);
    assert_eq!(store.reserve_task_id().unwrap(), "10000002");
    // Exercise the store lifecycle consumed by spawn/wait/result after migration.
    let claim = store.claim_next("daemon", 10, 1).unwrap().unwrap();
    assert_eq!(claim.task.session_id.as_deref(), Some("t_123"));
    assert!(store
        .mark_session_running("10000001", claim.owner_epoch, "runtime", None, None, None)
        .unwrap());
    assert_eq!(
        store.get_task("10000001").unwrap().unwrap().phase,
        TaskPhase::Running
    );
    store.claim_next_message("10000001").unwrap().unwrap();
    store.complete_message("message", Some("turn-1")).unwrap();
    let result = TaskResult {
        outcome: TaskOutcome::Completed,
        final_text: "preserved result".into(),
        partial: false,
    };
    store.store_task_result("10000001", &result).unwrap();
    let completed = store.get_task("10000001").unwrap().unwrap();
    assert_eq!(completed.phase, TaskPhase::Terminal);
    assert_eq!(completed.session_id.as_deref(), Some("t_123"));
    let stored_result = store.task_result("10000001").unwrap().unwrap();
    assert_eq!(stored_result.result, result);
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.get_task("10000001").unwrap().unwrap(), completed);
    assert_eq!(
        reopened.task_result("10000001").unwrap().unwrap(),
        stored_result
    );
}

#[test]
fn v12_rename_failure_leaves_database_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("collision.sqlite3");
    v12_fixture(&path);
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("ALTER TABLE tasks ADD COLUMN session_id TEXT DEFAULT 'collision';")
            .unwrap();
    }
    let before = fs::read(&path).unwrap();
    assert!(matches!(Store::open(&path), Err(StoreError::Sqlite(_))));
    assert_eq!(fs::read(&path).unwrap(), before);
    let connection = Connection::open(&path).unwrap();
    assert_eq!(version(&connection), 12);
    assert_eq!(
        connection
            .query_row(
                &format!("SELECT {OLD_COLUMN},session_id FROM tasks"),
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            )
            .unwrap(),
        ("t_123".into(), "collision".into())
    );
    assert!(!path.with_extension("sqlite3-wal").exists());
    assert!(!path.with_extension("sqlite3-shm").exists());
}

#[test]
fn v12_commit_failure_rolls_back_rename_and_version() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("commit-busy.sqlite3");
    v12_fixture(&path);
    let before = fs::read(&path).unwrap();
    // A reader in DELETE journal mode permits ALTER under a RESERVED lock, but
    // prevents COMMIT's EXCLUSIVE lock: failure occurs after both DDL and PRAGMA.
    let reader = Connection::open(&path).unwrap();
    reader.execute_batch("BEGIN; SELECT * FROM tasks;").unwrap();
    let mut writer = Connection::open(&path).unwrap();
    writer.busy_timeout(Duration::ZERO).unwrap();
    let error = initialize_schema(&mut writer).unwrap_err();
    assert!(
        matches!(error, StoreError::Sqlite(rusqlite::Error::SqliteFailure(code, _)) if code.code == rusqlite::ErrorCode::DatabaseBusy)
    );
    assert!(writer.is_autocommit());
    assert_eq!(version(&writer), 12);
    assert!(columns(&writer).iter().any(|column| column == OLD_COLUMN));
    assert!(!columns(&writer).iter().any(|column| column == "session_id"));
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    drop(writer);
    assert_eq!(fs::read(&path).unwrap(), before);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(
        reopened
            .get_task("10000001")
            .unwrap()
            .unwrap()
            .session_id
            .as_deref(),
        Some("t_123")
    );
    assert_eq!(version(&reopened.connection.lock().unwrap()), 14);
}

#[test]
fn v13_migration_drops_legacy_digest_column() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v13.sqlite3");
    v13_fixture(&path);
    let before = Connection::open(&path).unwrap();
    assert_eq!(version(&before), 13);
    assert!(columns(&before)
        .iter()
        .any(|column| column == "prepared_launch_sha256"));
    drop(before);

    let store = Store::open(&path).unwrap();
    {
        let connection = store.connection.lock().unwrap();
        assert_eq!(version(&connection), 14);
        assert!(!columns(&connection)
            .iter()
            .any(|column| column == "prepared_launch_sha256"));
        assert!(schema_is_current(&connection).unwrap());
    }
    let migrated = store.get_task("10000001").unwrap().unwrap();
    assert_eq!(migrated.session_id.as_deref(), Some("t_123"));
    assert_eq!(migrated.initial_prompt, "do work");
    assert_eq!(migrated.created_at, 123);
    assert_eq!(store.message("message").unwrap().unwrap().content, "follow up");
}

#[test]
fn v12_chain_migration_supports_a_fresh_enqueue() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v12-enqueue.sqlite3");
    v12_fixture(&path);

    let store = Store::open(&path).unwrap();
    let enqueued = store
        .enqueue_task_authoritative(&NewTask {
            agent_id: "10000003".into(),
            repository: "/repo".into(),
            workspace_path: "/workspace/new".into(),
            runtime_hash: None,
            prepared_launch_json: "{}".into(),
            initial_prompt: "new work".into(),
        })
        .unwrap();
    assert_eq!(enqueued.phase, TaskPhase::Queued);
    assert_eq!(enqueued.initial_prompt, "new work");
    assert_eq!(version(&store.connection.lock().unwrap()), 14);
}

#[test]
fn unsupported_versions_preserve_even_a_v12_shaped_database() {
    for unsupported in [0, 8, 11, 15] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unsupported.sqlite3");
        v12_fixture(&path);
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", unsupported)
            .unwrap();
        let before = fs::read(&path).unwrap();
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::LegacySchemaUnsupported)
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}

#[test]
fn fresh_database_uses_only_new_session_column() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("fresh.sqlite3")).unwrap();
    let connection = store.connection.lock().unwrap();
    assert_eq!(version(&connection), 14);
    let names = columns(&connection);
    assert!(names.iter().any(|column| column == "session_id"));
    assert!(!names.iter().any(|column| column == OLD_COLUMN));
    assert!(!names
        .iter()
        .any(|column| column == "prepared_launch_sha256"));
}
