use super::*;
use rusqlite::Connection;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

use crate::schema::schema_is_current;

fn task(id: &str, repository: &str, _scope: Option<&str>) -> NewTask {
    NewTask {
        agent_id: id.into(),
        repository: repository.into(),
        workspace_path: format!("/workspace/{id}"),
        runtime_hash: Some("runtime".into()),
        prepared_launch_json: "{}".into(),
        initial_prompt: "do work".into(),
    }
}

fn store() -> (TempDir, PathBuf, Store) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store.sqlite3");
    let store = Store::open(&path).unwrap();
    (directory, path, store)
}

fn running(store: &Store, id: &str) -> u64 {
    let claim = store.claim_next("daemon", 10, 10).unwrap().unwrap();
    assert_eq!(claim.task.agent_id, id);
    assert!(store
        .mark_session_running(id, claim.owner_epoch, "runtime", None, None, None)
        .unwrap());
    claim.owner_epoch
}

fn result(outcome: TaskOutcome) -> TaskResult {
    TaskResult {
        outcome,
        final_text: "terminal text".into(),
        partial: outcome != TaskOutcome::Completed,
    }
}

#[test]
fn queued_cancel_fence_blocks_claim_and_survives_reopen() {
    let (_directory, path, store) = store();
    store
        .enqueue_task_authoritative(&task("10000001", "/one", None))
        .unwrap();
    store
        .enqueue_task_authoritative(&task("10000002", "/two", None))
        .unwrap();
    running(&store, "10000001");
    store.fence_queued_cancellation().unwrap();
    assert!(!store.get_task("10000001").unwrap().unwrap().stop_requested);
    assert!(store.get_task("10000002").unwrap().unwrap().stop_requested);
    assert!(store.claim_next("loop", 10, 1).unwrap().is_none());
    drop(store);
    let reopened = Store::open(path).unwrap();
    assert!(reopened.claim_next("restart", 10, 1).unwrap().is_none());
}

#[test]
fn all_tasks_reaped_excludes_queued_and_requires_terminal_cleanup() {
    let (_directory, _path, store) = store();
    assert!(store.all_tasks_reaped().unwrap());
    store
        .enqueue_task_authoritative(&task("10000001", "/one", None))
        .unwrap();
    assert_eq!(store.active_count().unwrap(), 0);
    assert!(!store.all_tasks_reaped().unwrap());
    store.request_stop("10000001").unwrap();
    store
        .store_task_result("10000001", &result(TaskOutcome::Cancelled))
        .unwrap();
    assert!(!store.all_tasks_reaped().unwrap());
    store.reap_task("10000001").unwrap();
    assert!(store.all_tasks_reaped().unwrap());
}

#[test]
fn management_nonterminal_ids_include_queued_and_running_but_not_terminal() {
    let (_directory, _path, store) = store();
    store
        .enqueue_task_authoritative(&task("10000001", "/one", None))
        .unwrap();
    store
        .enqueue_task_authoritative(&task("10000002", "/two", None))
        .unwrap();
    running(&store, "10000001");
    assert_eq!(
        store.nonterminal_task_ids().unwrap(),
        vec!["10000001", "10000002"]
    );
    store.request_stop("10000002").unwrap();
    assert_eq!(
        store.nonterminal_task_ids().unwrap(),
        vec!["10000001", "10000002"]
    );
    store
        .store_task_result("10000002", &result(TaskOutcome::Cancelled))
        .unwrap();
    assert_eq!(store.nonterminal_task_ids().unwrap(), vec!["10000001"]);
}

#[test]
fn fresh_schema_is_minimal_and_forbidden_names_are_absent() {
    let (_directory, _path, store) = store();
    let connection = store.connection.lock().unwrap();
    assert!(schema_is_current(&connection).unwrap());
    assert_eq!(
        connection
            .pragma_query_value(None, "foreign_keys", |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let sql: String = connection
        .query_row(
            "SELECT group_concat(sql,' ') FROM sqlite_master WHERE sql IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    for forbidden in [
        concat!("task_", "attempts"),
        concat!("task_", "identities"),
        concat!("compatibility_", "runs"),
        concat!("task_", "kind"),
        concat!("attempt_", "sequence"),
        concat!("checkpoint_", "number"),
        concat!("public_", "agent_id"),
        concat!("execution_", "agent_id"),
    ] {
        assert!(!sql.contains(forbidden), "schema retained {forbidden}");
    }
}

#[test]
fn legacy_schema_is_rejected_without_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.sqlite3");
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA user_version=8; CREATE TABLE agents(agent_id TEXT PRIMARY KEY);")
            .unwrap();
    }
    let before = fs::read(&path).unwrap();
    let error = match Store::open(&path) {
        Ok(_) => panic!("legacy schema unexpectedly opened"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "STORE_SCHEMA_VERSION_UNSUPPORTED");
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(!path.with_extension("sqlite3-wal").exists());
    assert!(!path.with_extension("sqlite3-shm").exists());
}

#[test]
fn task_id_allocator_is_persistent_bounded_and_exhausts_without_wrap() {
    let (_directory, path, store) = store();
    assert_eq!(store.reserve_task_id().unwrap(), "10000000");
    assert_eq!(store.reserve_task_id().unwrap(), "10000001");
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.reserve_task_id().unwrap(), "10000002");
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE task_id_allocator SET next_id=99999999 WHERE id=1",
                [],
            )
            .unwrap();
    }
    assert_eq!(reopened.reserve_task_id().unwrap(), "99999999");
    assert!(reopened.reserve_task_id().is_err());
    let next: i64 = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT next_id FROM task_id_allocator WHERE id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(next, 100000000);
}

#[test]
fn task_id_allocator_is_unique_across_connections() {
    let (_directory, path, _store) = store();
    let path = std::sync::Arc::new(path);
    let mut workers = Vec::new();
    for _ in 0..8 {
        let path = std::sync::Arc::clone(&path);
        workers.push(std::thread::spawn(move || {
            Store::open(&*path).unwrap().reserve_task_id().unwrap()
        }));
    }
    let ids: std::collections::HashSet<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(ids.len(), 8);
    assert!(ids.contains("10000000"));
}

#[test]
fn allocated_id_is_not_reused_after_preparation_failure() {
    let (_directory, _path, store) = store();
    let first = store.reserve_task_id().unwrap();
    assert_eq!(first, "10000000");
    let preparation_failed = store.enqueue_task_authoritative(&task("", "/missing", None));
    assert!(preparation_failed.is_err());
    assert_eq!(store.reserve_task_id().unwrap(), "10000001");
}

#[test]
fn lifecycle_has_one_phase_and_terminal_outcome() {
    let (_directory, _path, store) = store();
    store
        .enqueue_task_authoritative(&task("agent", "/repo", None))
        .unwrap();
    running(&store, "agent");
    store
        .store_task_result("agent", &result(TaskOutcome::Completed))
        .unwrap();
    let task = store.get_task("agent").unwrap().unwrap();
    assert_eq!(task.phase, TaskPhase::Terminal);
    assert_eq!(task.outcome, Some(TaskOutcome::Completed));
    assert_eq!(store.reap_task("agent").unwrap(), TaskOutcome::Completed);
    assert!(store
        .get_task("agent")
        .unwrap()
        .unwrap()
        .reaped_at
        .is_some());
}

#[test]
fn pending_and_cancel_fence_terminal_completion() {
    let (_directory, _path, store) = store();
    store
        .enqueue_task_authoritative(&task("agent", "/repo", None))
        .unwrap();
    running(&store, "agent");
    store
        .insert_pending_request("request", "agent", "1", "permission", "{}")
        .unwrap();
    assert_eq!(
        store.get_task("agent").unwrap().unwrap().phase,
        TaskPhase::WaitingInput
    );
    assert!(matches!(
        store.store_task_result("agent", &result(TaskOutcome::Completed)),
        Err(StoreError::Conflict(_))
    ));
    assert_eq!(
        store
            .claim_pending_response_if_accepting("agent", "request", "deny", None)
            .unwrap(),
        PendingResponseClaimDisposition::Claimed
    );
    store.complete_pending_response("agent", "request").unwrap();
    assert_eq!(
        store.get_task("agent").unwrap().unwrap().phase,
        TaskPhase::Running
    );
    let decision = store.request_stop("agent").unwrap();
    assert_eq!(decision.phase, TaskPhase::Cancelling);
    assert!(store.pending_requests("agent").unwrap().is_empty());
    for outcome in [
        TaskOutcome::Completed,
        TaskOutcome::Failed,
        TaskOutcome::TimedOut,
        TaskOutcome::RuntimeLost,
        TaskOutcome::ResultInvalid,
    ] {
        assert!(matches!(
            store.store_task_result("agent", &result(outcome)),
            Err(StoreError::Conflict(_))
        ));
    }
    store
        .store_task_result("agent", &result(TaskOutcome::Cancelled))
        .unwrap();
}

#[test]
fn close_intent_coerces_non_cancel_terminal_transition() {
    let (_directory, _path, store) = store();
    store
        .enqueue_task_authoritative(&task("agent", "/repo", None))
        .unwrap();
    let owner_epoch = running(&store, "agent");
    store.request_close("agent").unwrap();
    assert_eq!(
        store
            .transition_terminal(
                "agent",
                owner_epoch,
                &TerminalUpdate {
                    outcome: TaskOutcome::RuntimeLost,
                    failure_code: Some("LATE_RUNTIME_LOST".into()),
                    failure_message: None,
                },
            )
            .unwrap(),
        TaskOutcome::Cancelled
    );
    let task = store.get_task("agent").unwrap().unwrap();
    assert_eq!(task.outcome, Some(TaskOutcome::Cancelled));
    assert!(task.closed_at.is_some());
}

#[test]
fn startup_recovery_includes_fenced_queue_but_preserves_unfenced_queue() {
    let (_directory, path, store) = store();
    store
        .enqueue_task_authoritative(&task("fenced", "/one", None))
        .unwrap();
    store.fence_queued_cancellation().unwrap();
    store
        .enqueue_task_authoritative(&task("ordinary", "/two", None))
        .unwrap();
    drop(store);
    let reopened = Store::open(path).unwrap();
    let recovery = reopened.startup_recovery_tasks().unwrap();
    assert_eq!(recovery.len(), 1);
    assert_eq!(recovery[0].agent_id, "fenced");
    assert_eq!(recovery[0].phase, TaskPhase::Queued);
    assert!(recovery[0].stop_requested);
    assert!(reopened.task_result("fenced").unwrap().is_none());
    let ordinary = reopened.claim_next("claim-loop", 10, 1).unwrap().unwrap();
    assert_eq!(ordinary.task.agent_id, "ordinary");
}

#[test]
fn startup_recovery_inventory_is_read_only() {
    let (_directory, path, store) = store();
    store
        .enqueue_task_authoritative(&task("agent", "/repo", None))
        .unwrap();
    running(&store, "agent");
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(
        reopened
            .startup_recovery_tasks()
            .unwrap()
            .into_iter()
            .map(|task| task.agent_id)
            .collect::<Vec<_>>(),
        vec!["agent"]
    );
    assert_eq!(
        reopened.get_task("agent").unwrap().unwrap().phase,
        TaskPhase::Running
    );
    assert!(reopened.task_result("agent").unwrap().is_none());
}

#[test]
fn message_receipt_contains_delivery_timestamps_and_agent_scope() {
    let (_directory, _path, store) = store();
    store
        .enqueue_task_authoritative(&task("agent", "/repo", None))
        .unwrap();
    running(&store, "agent");
    store
        .insert_message("m1", "agent", "queue", "hello")
        .unwrap();
    let queued = store.message("m1").unwrap().unwrap();
    assert_eq!(queued.state, MessageState::Queued);
    assert!(queued.created_at > 0);
    assert!(queued.delivered_at.is_none());
    assert!(store.message("missing").unwrap().is_none());
    let claimed = store.claim_next_message("agent").unwrap().unwrap();
    assert_eq!(claimed.state, MessageState::Sending);
    store.complete_message("m1", Some("turn-1")).unwrap();
    let delivered = store.message("m1").unwrap().unwrap();
    assert_eq!(delivered.state, MessageState::Delivered);
    assert_eq!(delivered.target_turn_id.as_deref(), Some("turn-1"));
    assert!(delivered.delivered_at.unwrap() >= delivered.created_at);
}

#[test]
fn pending_projection_is_bounded_and_nonrespondable_tail_does_not_change_state() {
    let (_directory, _path, store) = store();
    store
        .enqueue_task_authoritative(&task("agent", "/repo", None))
        .unwrap();
    running(&store, "agent");
    for index in 0..101 {
        store
            .insert_pending_request(
                &format!("request-{index}"),
                "agent",
                &format!("correlation-{index}"),
                "permission",
                "{\"toolName\":\"Read\"}",
            )
            .unwrap();
    }
    let projection = store.pending_requests_bounded("agent", 100).unwrap();
    assert_eq!(projection.len(), 100);
    assert_eq!(projection[0].request_id, "request-0");
    assert_eq!(projection[99].request_id, "request-99");
    assert!(store.completion_blockers("agent").unwrap().0);
    assert_eq!(
        store.get_task("agent").unwrap().unwrap().phase,
        TaskPhase::WaitingInput
    );
}

#[test]
fn cancellation_fences_queue_and_late_or_duplicate_responses() {
    let (_directory, _path, store) = store();
    store
        .enqueue_task_authoritative(&task("agent", "/repo", None))
        .unwrap();
    running(&store, "agent");
    store
        .insert_message("message", "agent", "queue", "follow-up")
        .unwrap();
    store
        .insert_pending_request("request", "agent", "corr", "permission", "{}")
        .unwrap();
    let decision = store.request_stop("agent").unwrap();
    assert!(decision.needs_runtime_stop);
    assert_eq!(decision.phase, TaskPhase::Cancelling);
    assert!(matches!(
        store
            .claim_pending_response_if_accepting("agent", "request", "allow", None)
            .unwrap(),
        PendingResponseClaimDisposition::NotFound | PendingResponseClaimDisposition::TaskStopping
    ));
    assert!(store.pending_requests("agent").unwrap().is_empty());
    assert_eq!(
        store.message("message").unwrap().unwrap().state,
        MessageState::Failed
    );
    assert!(matches!(
        store.store_task_result("agent", &result(TaskOutcome::Completed)),
        Err(StoreError::Conflict(_))
    ));
    store
        .store_task_result("agent", &result(TaskOutcome::Cancelled))
        .unwrap();
    assert!(matches!(
        store
            .claim_pending_response_if_accepting("agent", "request", "deny", None)
            .unwrap(),
        PendingResponseClaimDisposition::NotFound
    ));
}

#[test]
fn recovery_inventory_does_not_replay_or_mutate_unknown_runtime_delivery() {
    let (_directory, path, store) = store();
    store
        .enqueue_task_authoritative(&task("agent", "/repo", None))
        .unwrap();
    running(&store, "agent");
    store
        .insert_message("message", "agent", "queue", "once")
        .unwrap();
    let first_claim = store.claim_next_message("agent").unwrap().unwrap();
    assert_eq!(first_claim.state, MessageState::Sending);
    drop(store);
    let reopened = Store::open(&path).unwrap();
    let inventory = reopened.startup_recovery_tasks().unwrap();
    assert_eq!(inventory.len(), 1);
    let recovered_message = reopened.message("message").unwrap().unwrap();
    assert_eq!(recovered_message.state, MessageState::Sending);
    assert!(reopened.claim_next_message("agent").unwrap().is_none());
    assert!(reopened
        .complete_message("message", Some("replayed-turn"))
        .unwrap());
}

#[test]
fn message_insert_is_idempotent_and_rejects_identity_drift() {
    let (_directory, _path, store) = store();
    store
        .enqueue_task_authoritative(&task("agent", "/repo", None))
        .unwrap();
    running(&store, "agent");
    assert!(store
        .insert_message("m", "agent", "queue", "hello")
        .unwrap());
    assert!(!store
        .insert_message("m", "agent", "queue", "hello")
        .unwrap());
    assert!(matches!(
        store.insert_message("m", "agent", "queue", "changed"),
        Err(StoreError::Conflict(_))
    ));
}

#[test]
fn pending_response_claim_complete_and_duplicate_are_stateful() {
    let (_directory, _path, store) = store();
    store
        .enqueue_task_authoritative(&task("agent", "/repo", None))
        .unwrap();
    running(&store, "agent");
    assert!(store
        .insert_pending_request("r", "agent", "c", "permission", "{}")
        .unwrap());
    assert_eq!(
        store.pending_request("agent", "r").unwrap().unwrap().state,
        PendingRequestState::Pending
    );
    assert_eq!(
        store
            .claim_pending_response_if_accepting("agent", "r", "allow", Some("ok"))
            .unwrap(),
        PendingResponseClaimDisposition::Claimed
    );
    assert_eq!(
        store
            .claim_pending_response_if_accepting("agent", "r", "allow", None)
            .unwrap(),
        PendingResponseClaimDisposition::NotPending(PendingRequestState::Sending)
    );
    assert!(store.complete_pending_response("agent", "r").unwrap());
    assert!(!store.complete_pending_response("agent", "r").unwrap());
    assert_eq!(
        store.pending_request("agent", "r").unwrap().unwrap().state,
        PendingRequestState::Responded
    );
}

#[test]
fn failed_message_insert_rolls_back_without_receipt() {
    let (_directory, _path, store) = store();
    store
        .enqueue_task_authoritative(&task("agent", "/repo", None))
        .unwrap();
    running(&store, "agent");
    {
        let connection = store.connection.lock().unwrap();
        connection.execute_batch("CREATE TRIGGER reject_message BEFORE INSERT ON messages BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
    }
    assert!(matches!(
        store.insert_message("m", "agent", "queue", "hello"),
        Err(StoreError::Sqlite(_))
    ));
    assert!(store.message("m").unwrap().is_none());
}

#[test]
fn separate_store_handles_claim_messages_once_in_fifo_order() {
    let (_directory, path, store_a) = store();
    store_a
        .enqueue_task_authoritative(&task("agent", "/repo", None))
        .unwrap();
    running(&store_a, "agent");
    let store_b = Store::open(&path).unwrap();
    store_a
        .insert_message("m1", "agent", "queue", "one")
        .unwrap();
    store_a
        .insert_message("m2", "agent", "queue", "two")
        .unwrap();
    assert_eq!(
        store_a
            .claim_next_message("agent")
            .unwrap()
            .unwrap()
            .message_id,
        "m1"
    );
    assert_eq!(
        store_b
            .claim_next_message("agent")
            .unwrap()
            .unwrap()
            .message_id,
        "m2"
    );
    assert!(store_a.claim_next_message("agent").unwrap().is_none());
}

#[test]
fn failed_second_insert_preserves_prior_message_receipt() {
    let (_directory, _path, store) = store();
    store
        .enqueue_task_authoritative(&task("agent", "/repo", None))
        .unwrap();
    running(&store, "agent");
    store
        .insert_message("m1", "agent", "queue", "prior")
        .unwrap();
    {
        let connection = store.connection.lock().unwrap();
        connection.execute_batch("CREATE TRIGGER reject_message_two BEFORE INSERT ON messages WHEN NEW.message_id='m2' BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
    }
    assert!(matches!(
        store.insert_message("m2", "agent", "queue", "bad"),
        Err(StoreError::Sqlite(_))
    ));
    assert_eq!(store.message("m1").unwrap().unwrap().content, "prior");
    assert!(store.message("m2").unwrap().is_none());
}

#[test]
fn terminal_reason_code_reads_the_explicit_reason() {
    let (_directory, _path, store) = store();
    store
        .enqueue_task_authoritative(&task("agent", "/repo", None))
        .unwrap();
    running(&store, "agent");
    store
        .store_task_result_with_reason(
            "agent",
            &result(TaskOutcome::Failed),
            Some("MODEL_REJECTED"),
        )
        .unwrap();
    assert_eq!(
        store.terminal_reason_code("agent").unwrap().as_deref(),
        Some("MODEL_REJECTED")
    );
}

#[test]
fn terminal_reason_code_maps_placeholder_completed_and_absent_rows_to_none() {
    let (_directory, _path, store) = store();
    // The legacy `store_task_result` path still writes the compatibility
    // placeholder, which the reader must not surface as a machine reason.
    store
        .enqueue_task_authoritative(&task("placeholder", "/repo", None))
        .unwrap();
    running(&store, "placeholder");
    store
        .store_task_result("placeholder", &result(TaskOutcome::Failed))
        .unwrap();
    assert_eq!(store.terminal_reason_code("placeholder").unwrap(), None);

    store
        .enqueue_task_authoritative(&task("completed", "/repo", None))
        .unwrap();
    running(&store, "completed");
    store
        .store_task_result("completed", &result(TaskOutcome::Completed))
        .unwrap();
    assert_eq!(store.terminal_reason_code("completed").unwrap(), None);

    // No terminal row yet (queued) and an unknown task both read None.
    store
        .enqueue_task_authoritative(&task("queued", "/repo", None))
        .unwrap();
    assert_eq!(store.terminal_reason_code("queued").unwrap(), None);
    assert_eq!(store.terminal_reason_code("missing").unwrap(), None);
}

#[test]
fn terminal_reason_code_after_resume_completion_ignores_the_stale_failure() {
    let (_directory, _path, store) = store();
    store
        .enqueue_task_authoritative(&task("agent", "/repo", None))
        .unwrap();
    let claim = store.claim_next("daemon", 10, 10).unwrap().unwrap();
    store
        .mark_session_running(
            "agent",
            claim.owner_epoch,
            "runtime",
            None,
            Some("session"),
            None,
        )
        .unwrap();
    store
        .store_task_result_with_reason(
            "agent",
            &result(TaskOutcome::Failed),
            Some("MODEL_REJECTED"),
        )
        .unwrap();
    assert!(store
        .requeue_task_for_resume_with_message("agent", "resume-msg", "continue")
        .unwrap());
    // The requeue row is not a TERMINAL row, so the stale failure still reads
    // until the resumed task terminalizes.
    assert_eq!(
        store.terminal_reason_code("agent").unwrap().as_deref(),
        Some("MODEL_REJECTED")
    );

    let claim = store.claim_next("daemon", 10, 10).unwrap().unwrap();
    store
        .mark_session_running(
            "agent",
            claim.owner_epoch,
            "runtime",
            None,
            Some("session"),
            None,
        )
        .unwrap();
    // The resume message must be delivered before a completed result is
    // admitted.
    let message = store.claim_next_message("agent").unwrap().unwrap();
    assert_eq!(message.content, "continue");
    assert!(store
        .complete_message(&message.message_id, None)
        .unwrap());
    store
        .store_task_result("agent", &result(TaskOutcome::Completed))
        .unwrap();
    // The latest terminal row is the completed one (NULL reason); the older
    // failure row must not resurface.
    assert_eq!(store.terminal_reason_code("agent").unwrap(), None);
}
