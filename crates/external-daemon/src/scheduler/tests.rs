use super::diagnostics::{
    persistable_failure_record, persistable_record_with_tail, runtime_failure_record,
    update_latest_failure, DiagnosticLogger, RotatingDiagnosticWriter, DIAGNOSTIC_FILE_BYTES,
    DIAGNOSTIC_QUEUE_CAPACITY, DIAGNOSTIC_RECORD_BYTES, PERSISTABLE_RECORD_BYTES,
};
use super::*;

#[cfg(test)]
mod queued_recovery_tests {
    use super::*;

    #[test]
    fn fenced_queue_reopen_recovers_cancelled_without_spawn_and_becomes_ready() {
        assert_fenced_queue_recovery(false);
    }

    #[test]
    fn second_crash_after_stop_commit_recovers_without_spawn() {
        assert_fenced_queue_recovery(true);
    }

    fn assert_fenced_queue_recovery(crash_before_result: bool) {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/live-agent/workspace");
        std::fs::create_dir_all(&base).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("queued-recovery-")
            .tempdir_in(base)
            .unwrap();
        let database = directory.path().join("state.sqlite");
        let factory = Arc::new(CommandRuntimeFactory::new(
            |_: &TaskRecord| -> io::Result<Command> {
                panic!("startup recovery must never spawn a provider");
            },
        ));
        let scheduler = Scheduler::new(
            "before-exit",
            Arc::new(Store::open(&database).unwrap()),
            factory.clone(),
            SchedulerConfig::default(),
        )
        .unwrap();
        let task = scheduler
            .enqueue_general(&GeneralTaskManifest {
                schema: "zcode-general-task/v1".into(),
                agent_id: String::new(),
                repository: directory.path().canonicalize().unwrap(),
                permission_mode: external_core::PermissionMode::Build,
                prompt: "never execute this fenced queue".into(),
                write_manifest: vec![],
            })
            .unwrap();
        scheduler.begin_drain();
        scheduler.store().fence_queued_cancellation().unwrap();
        assert_eq!(
            scheduler
                .store()
                .get_task(&task.agent_id)
                .unwrap()
                .unwrap()
                .phase,
            TaskPhase::Queued
        );
        assert!(!scheduler.ready_for_activation());
        drop(scheduler); // Exit before the asynchronous cancellation worker runs.

        let reopened = Scheduler::new(
            "after-exit",
            Arc::new(Store::open(&database).unwrap()),
            factory.clone(),
            SchedulerConfig::default(),
        )
        .unwrap();
        let reopened = if crash_before_result {
            // Inject failure at the real result transaction, after the
            // cancellation transaction has committed. Then discard all
            // in-memory recovery state exactly as a second exit would.
            let db = rusqlite::Connection::open(&database).unwrap();
            db.execute_batch("CREATE TRIGGER fail_cancel_result BEFORE INSERT ON task_results BEGIN SELECT RAISE(ABORT, 'injected second exit'); END;").unwrap();
            assert!(reopened.reconcile_startup().is_err());
            let interrupted = reopened.store().get_task(&task.agent_id).unwrap().unwrap();
            assert_eq!(interrupted.phase, TaskPhase::Cancelling);
            assert!(interrupted.stop_requested);
            assert_eq!(interrupted.owner_epoch, 0);
            assert!(interrupted.runtime_agent_id.is_none());
            assert!(interrupted.process_identity.is_none());
            assert!(interrupted.reaped_at.is_none());
            assert!(reopened
                .store()
                .task_result(&task.agent_id)
                .unwrap()
                .is_none());
            drop(reopened);
            db.execute_batch("DROP TRIGGER fail_cancel_result;")
                .unwrap();
            drop(db);
            Scheduler::new(
                "after-second-exit",
                Arc::new(Store::open(&database).unwrap()),
                factory,
                SchedulerConfig::default(),
            )
            .unwrap()
        } else {
            reopened
        };
        assert_eq!(
            reopened.reconcile_startup().unwrap(),
            vec![(task.agent_id.clone(), TaskOutcome::Cancelled)]
        );
        let recovered = reopened.store().get_task(&task.agent_id).unwrap().unwrap();
        assert_eq!(recovered.phase, TaskPhase::Terminal);
        assert_eq!(recovered.outcome, Some(TaskOutcome::Cancelled));
        assert!(recovered.reaped_at.is_some());
        assert_eq!(recovered.owner_epoch, 0);
        let result = reopened
            .store()
            .task_result(&task.agent_id)
            .unwrap()
            .unwrap();
        assert_eq!(result.result.outcome, TaskOutcome::Cancelled);
        assert!(reopened.start_ready().unwrap().is_empty());
        assert!(reopened.reconcile_startup().unwrap().is_empty());
        assert_eq!(
            reopened
                .store()
                .task_result(&task.agent_id)
                .unwrap()
                .unwrap(),
            result
        );
        reopened.begin_drain();
        assert!(reopened.ready_for_activation());
        assert!(reopened.claim_activation().is_some());
    }
    #[test]
    fn identityless_cancelling_requires_never_claimed_and_explicit_stop() {
        for claimed in [false, true] {
            let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/live-agent/workspace");
            let directory = tempfile::Builder::new()
                .prefix("invalid-recovery-")
                .tempdir_in(base)
                .unwrap();
            let database = directory.path().join("state.sqlite");
            let factory = Arc::new(CommandRuntimeFactory::new(
                |_: &TaskRecord| -> io::Result<Command> {
                    panic!("recovery must not spawn");
                },
            ));
            let scheduler = Scheduler::new(
                "invalid",
                Arc::new(Store::open(&database).unwrap()),
                factory.clone(),
                SchedulerConfig::default(),
            )
            .unwrap();
            let task = scheduler
                .enqueue_general(&GeneralTaskManifest {
                    schema: "zcode-general-task/v1".into(),
                    agent_id: String::new(),
                    repository: directory.path().canonicalize().unwrap(),
                    permission_mode: external_core::PermissionMode::Build,
                    prompt: "invalid runtime identity".into(),
                    write_manifest: vec![],
                })
                .unwrap();
            if claimed {
                scheduler
                    .store()
                    .claim_next("prior-owner", 10, 1)
                    .unwrap()
                    .unwrap();
                scheduler.store().request_stop(&task.agent_id).unwrap();
            } else {
                scheduler
                    .store()
                    .request_runtime_stop(&task.agent_id)
                    .unwrap();
            }
            drop(scheduler);
            let reopened = Scheduler::new(
                "after-exit",
                Arc::new(Store::open(&database).unwrap()),
                factory,
                SchedulerConfig::default(),
            )
            .unwrap();
            let error = reopened.reconcile_startup().unwrap_err();
            assert!(error.to_string().contains("runtime identity is incomplete"));
            let retained = reopened.store().get_task(&task.agent_id).unwrap().unwrap();
            assert_eq!(retained.phase, TaskPhase::Cancelling);
            assert!(retained.reaped_at.is_none());
            assert!(reopened
                .store()
                .task_result(&task.agent_id)
                .unwrap()
                .is_none());
        }
    }
}

#[cfg(test)]
mod observation_evidence_tests {
    use super::*;

    /// The literal pinned ZCode runtime path. Using it as the scheduler's
    /// global `runtime_source` is the strongest "pinned ZCode installation
    /// present" fixture a deterministic test can build: every assertion below
    /// is invariant to whether that file exists or matches the pinned digest,
    /// because the verdicts are bound to the launched adapter, not the file.
    const PINNED_ZCODE_SOURCE: &str = "/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs";
    /// Mirrors observation.rs's public-argument bound (4 KiB).
    const MAX_ARGUMENT_BYTES: usize = 4 * 1024;

    fn admission(agent: &str) -> external_core::AdmissionIdentity {
        external_core::AdmissionIdentity {
            agent: agent.into(),
            config_revision: 1,
            adapter_version: env!("CARGO_PKG_VERSION").into(),
            model: None,
            model_source: "catalog".into(),
            effort: None,
        }
    }

    fn manifest_for(directory: &std::path::Path, agent: &str) -> GeneralTaskManifest {
        GeneralTaskManifest {
            schema: external_core::GENERAL_TASK_SCHEMA.into(),
            agent_id: format!("{agent}-observe"),
            repository: directory.canonicalize().unwrap(),
            permission_mode: external_core::PermissionMode::Build,
            prompt: "observation evidence binding".into(),
            write_manifest: Vec::new(),
        }
    }

    fn workspace(prefix: &str) -> tempfile::TempDir {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/live-agent/workspace");
        std::fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(root)
            .unwrap()
    }

    fn observation_scheduler(
        directory: &std::path::Path,
        script: &str,
        runtime_source: Option<PathBuf>,
    ) -> Scheduler {
        let directory = directory.to_owned();
        let store = Arc::new(Store::open(directory.join("state.sqlite")).unwrap());
        let script = script.to_owned();
        // The scripted child runs for every adapter identity: this suite
        // isolates the scheduler's launch-scoped evidence binding, while the
        // dsh/codex suites own real adapter routing and spawn behavior.
        let factory = Arc::new(CommandRuntimeFactory::new_prepared(
            move |_: &TaskRecord| {
                let mut command = Command::new("sh");
                command.args(["-c", &script]).current_dir(&directory);
                Ok(command)
            },
        ));
        Scheduler::new(
            "observation-evidence",
            store,
            factory,
            SchedulerConfig {
                bootstrap_timeout: Duration::from_secs(30),
                control_timeout: Duration::from_secs(10),
                runtime_source,
                ..SchedulerConfig::default()
            },
        )
        .unwrap()
    }

    fn await_result(scheduler: &Scheduler, agent_id: &str) -> external_store::StoredTaskResult {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = scheduler.store().task_result(agent_id).unwrap() {
                return result;
            }
            assert!(Instant::now() < deadline, "no terminal result");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// A bootstrap plus one streaming turn whose public observation events
    /// carry reasoning, an encrypted-content tool argument and an oversized
    /// tool argument; the turn completes only after `release-observe` exists.
    const OBSERVATION_PROTOCOL: &str = r#"
read request
printf '%s\n' '{"id":1,"result":{"session":{"sessionId":"obs-session"}}}'
read request
printf '%s\n' '{"id":2,"result":{}}'
read request
printf '%s\n' '{"id":3,"result":{}}' '{"method":"session/event","params":{"type":"turn.started"}}'
printf '%s\n' '{"method":"session/event","params":{"type":"model.streaming","eventId":"e-reason","turnId":"t1","payload":{"kind":"reasoning_delta","delta":"ADAPTER-SCOPED reasoning tail"}}}'
printf '%s\n' '{"method":"session/event","params":{"type":"model.streaming","eventId":"e-tool","turnId":"t1","payload":{"kind":"tool_call","toolCallId":"c-enc","toolName":"Bash","input":{"command":"echo ok","nested":{"encrypted_content":"NEVER-SECRET"}}}}}'
pad=$(awk 'BEGIN{for(i=0;i<12000;i++)printf "x"}')
printf '%s\n' "{\"method\":\"session/event\",\"params\":{\"type\":\"model.streaming\",\"eventId\":\"e-big\",\"turnId\":\"t1\",\"payload\":{\"kind\":\"tool_call\",\"toolCallId\":\"c-big\",\"toolName\":\"Read\",\"input\":{\"path\":\"$pad\"}}}}"
while [ ! -f release-observe ]; do sleep 0.01; done
printf '%s\n' '{"method":"session/event","params":{"type":"model.streaming","payload":{"kind":"text_delta","delta":"final answer","assistantMessageId":"m1"}}}' '{"method":"session/event","params":{"type":"message.finished","payload":{"assistantMessageId":"m1"}}}' '{"method":"session/event","params":{"type":"turn.completed"}}'
while read request; do printf '%s\n' "$request" >> deliveries.jsonl; done
"#;

    fn await_observed_content(
        scheduler: &Scheduler,
        agent_id: &str,
        adapter: &str,
    ) -> observation::ObservationSnapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (snapshot, verified) = scheduler.observation_snapshot(agent_id);
            assert_eq!(
                verified,
                adapter == "dsh",
                "only DSH has its own public source"
            );
            // The fixture emits both tools after reasoning; hidden adapters
            // must not wait for reasoning that must never be collected.
            if snapshot.tools.len() == 2 {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "observation content never arrived: {snapshot:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn non_zcode_adapters_never_inherit_the_zcode_runtime_proof() {
        for (agent, runtime_source) in [
            ("dsh", None),
            ("dsh", Some(PINNED_ZCODE_SOURCE)),
            ("codex", Some(PINNED_ZCODE_SOURCE)),
            ("unsupported-adapter-fixture", Some(PINNED_ZCODE_SOURCE)),
        ] {
            let directory = workspace("s04-observation-");
            let scheduler = observation_scheduler(
                directory.path(),
                OBSERVATION_PROTOCOL,
                runtime_source.map(PathBuf::from),
            );
            let submitted = scheduler
                .enqueue_general_with_admission(
                    &manifest_for(directory.path(), agent),
                    Some(admission(agent)),
                )
                .unwrap();
            let agent_id = submitted.agent_id;
            scheduler.start_ready().unwrap();
            let snapshot = await_observed_content(&scheduler, &agent_id, agent);

            // DSH's public ACP evidence is independent of the ZCode pin.
            // Codex and unknown adapters never collect reasoning.
            if agent == "dsh" {
                assert!(snapshot
                    .reasoning
                    .text
                    .contains("ADAPTER-SCOPED reasoning tail"));
                assert_eq!(
                    snapshot.reasoning.source,
                    observation::ReasoningSource::dsh()
                );
                assert!(snapshot.coverage.reasoning_complete);
            } else {
                assert!(snapshot.reasoning.text.is_empty());
                assert!(!snapshot.coverage.reasoning_complete);
            }
            assert!(!snapshot.coverage.tool_history_complete);
            let encoded = serde_json::to_string(&snapshot.tools).unwrap();
            assert!(!encoded.contains("encrypted_content"));
            assert!(!encoded.contains("NEVER-SECRET"));
            let bash = snapshot
                .tools
                .iter()
                .find(|tool| tool.tool_name == "Bash")
                .expect("Bash tool observed");
            assert_eq!(bash.recent_calls[0].redacted_fields, 1);
            let read = snapshot
                .tools
                .iter()
                .find(|tool| tool.tool_name == "Read")
                .expect("Read tool observed");
            assert!(read.recent_calls[0].arguments_truncated);
            for tool in &snapshot.tools {
                for call in &tool.recent_calls {
                    assert!(
                        serde_json::to_vec(&call.arguments).unwrap().len() <= MAX_ARGUMENT_BYTES
                    );
                }
            }

            std::fs::write(directory.path().join("release-observe"), "").unwrap();
            let result = await_result(&scheduler, &agent_id);
            assert_eq!(result.result.outcome, TaskOutcome::Completed);
            // Terminalization preserves the same adapter-scoped evidence.
            let (terminal_snapshot, verified) = scheduler.observation_snapshot(&agent_id);
            assert_eq!(verified, agent == "dsh");
            assert_eq!(terminal_snapshot.reasoning, snapshot.reasoning);
            assert_eq!(terminal_snapshot.coverage, snapshot.coverage);
            assert!(!terminal_snapshot.tools.is_empty());
        }
    }

    #[test]
    fn missing_activity_never_borrows_the_global_runtime_proof() {
        let directory = workspace("s04-observation-missing-");
        // A task that was never launched has no launch-scoped activity; the
        // scheduler-global pinned ZCode path must not stand in for it.
        let scheduler = observation_scheduler(
            directory.path(),
            "exit 0",
            Some(PathBuf::from(PINNED_ZCODE_SOURCE)),
        );
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for(directory.path(), "dsh"),
                Some(admission("dsh")),
            )
            .unwrap();
        let (queued, verified) = scheduler.observation_snapshot(&submitted.agent_id);
        assert!(!verified);
        assert_eq!(queued.snapshot_seq, 0);
        assert!(queued.tools.is_empty());
        // An unknown task id reports the same unavailable, untrusted verdict.
        let (unknown, unknown_verified) = scheduler.observation_snapshot("99999999");
        assert!(!unknown_verified);
        assert_eq!(unknown.snapshot_seq, queued.snapshot_seq);
        assert_eq!(unknown.tools, queued.tools);
        assert_eq!(unknown.coverage, queued.coverage);
        assert!(!queued.coverage.tool_history_complete);
        assert!(!queued.coverage.reasoning_complete);
        assert_eq!(unknown.reasoning.text, queued.reasoning.text);
        assert!(queued.reasoning.text.is_empty());
        assert_eq!(unknown.reasoning.truncated, queued.reasoning.truncated);
        assert_eq!(queued.reasoning.source, observation::ReasoningSource::dsh());
        assert_ne!(unknown.reasoning.source, queued.reasoning.source);
    }
}

#[cfg(test)]
mod failure_log_tests {
    use super::*;
    use external_store::StoreError;
    use std::collections::HashMap;
    use std::io::{self, Write};
    use std::sync::mpsc;
    use std::time::Duration;

    fn diagnostic_scheduler(script: &str) -> (tempfile::TempDir, Scheduler, String) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        fs::create_dir_all(&root).unwrap();
        let workspace = tempfile::Builder::new()
            .prefix("s02-fault-")
            .tempdir_in(root)
            .unwrap();
        let store = Arc::new(Store::open(workspace.path().join("state.sqlite")).unwrap());
        let script = script.to_owned();
        let factory = CommandRuntimeFactory::new(move |_: &TaskRecord| {
            let mut command = Command::new("sh");
            command.args(["-c", &script]);
            Ok(command)
        });
        let scheduler = Scheduler::new(
            "diagnostic-test",
            store,
            Arc::new(factory),
            SchedulerConfig::default(),
        )
        .unwrap();
        let submitted = scheduler
            .enqueue_general(&GeneralTaskManifest {
                schema: "zcode-general-task/v1".into(),
                agent_id: "diagnostic-agent".into(),
                repository: workspace.path().canonicalize().unwrap(),
                permission_mode: external_core::PermissionMode::Plan,
                prompt: "diagnostic fixture".into(),
                write_manifest: Vec::new(),
            })
            .unwrap();
        (workspace, scheduler, submitted.agent_id)
    }

    fn await_result(scheduler: &Scheduler, agent_id: &str) -> external_store::StoredTaskResult {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = scheduler.store().task_result(agent_id).unwrap() {
                return result;
            }
            assert!(Instant::now() < deadline, "no terminal result");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn failure_record(scheduler: &Scheduler, agent_id: &str) -> serde_json::Value {
        let record = scheduler
            .last_error(agent_id)
            .expect("correlated failure record");
        assert!(record.len() < 192 * 1024);
        let record: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert_eq!(record["agent_id"], agent_id);
        record
    }

    #[test]
    fn enqueue_preparation_failure_consumes_id_and_preserves_classification() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let factory = CommandRuntimeFactory::new(|_: &TaskRecord| {
            let mut command = Command::new("sh");
            command.args(["-c", "exit 0"]);
            Ok(command)
        });
        let scheduler = Scheduler::new(
            "s02-preparation-failure",
            Arc::clone(&store),
            Arc::new(factory),
            SchedulerConfig::default(),
        )
        .unwrap();
        let invalid = GeneralTaskManifest {
            schema: "zcode-general-task/v1".into(),
            agent_id: "caller-value-is-ignored".into(),
            repository: directory.path().join("does-not-exist"),
            permission_mode: external_core::PermissionMode::Plan,
            prompt: "preparation failure".into(),
            write_manifest: Vec::new(),
        };
        let error = scheduler.enqueue_general(&invalid).unwrap_err();
        assert!(matches!(error, SchedulerError::InvalidConfig(_)));

        let valid = GeneralTaskManifest {
            repository: directory.path().canonicalize().unwrap(),
            prompt: "valid submission".into(),
            ..invalid
        };
        let submitted = scheduler.enqueue_general(&valid).unwrap();
        assert_eq!(submitted.agent_id, "10000001");
        assert!(store.get_task("10000000").unwrap().is_none());
    }

    #[test]
    fn startup_and_protocol_failures_record_stderr_without_changing_result() {
        let cases = [
            (
                "read request; printf startup-tail >&2; exit 7",
                None,
                "startup-tail",
            ),
            (
                r#"read request; printf invalid-projection-tail >&2; printf '%s\n' '{"id":1,"result":{}}'; sleep 2"#,
                None,
                "invalid-projection-tail",
            ),
            (
                r#"read request; printf '%s\n' '{"id":1,"result":{"session":{"sessionId":"known-session"}}}'; read request; printf subscribe-tail >&2; printf '%s\n' '{"id":2,"error":{"code":-1,"message":"reject"}}'; sleep 2"#,
                Some("known-session"),
                "subscribe-tail",
            ),
        ];
        for (script, session, tail) in cases {
            let (_workspace, scheduler, agent_id) = diagnostic_scheduler(script);
            assert!(scheduler.start_ready().is_err());
            let record = failure_record(&scheduler, &agent_id);
            assert_eq!(record["stage"], "session_start");
            assert_eq!(record["error_code"], "SESSION_START_FAILED");
            assert_eq!(record["session_id"].as_str(), session);
            assert!(record["stderr_tail"].as_str().unwrap().contains(tail));
            let result = await_result(&scheduler, &agent_id);
            let result_json = serde_json::to_string(&result.result).unwrap();
            assert!(
                !result_json.contains(tail),
                "stderr leaked into task result"
            );
            let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
            assert_eq!(task.outcome, Some(TaskOutcome::Failed));
            // Bootstrap failures persist the real runtime session and stderr
            // suffix in the task row, not just in the diagnostic log.
            let raw = task.failure_message.expect("persisted failure detail");
            let persisted: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(persisted["stage"], "session_start");
            assert_eq!(persisted["error_code"], "SESSION_START_FAILED");
            assert_eq!(persisted["session_id"].as_str(), session);
            assert!(persisted["stderr_tail"].as_str().unwrap().contains(tail));
            assert!(!raw.contains('\n'));
        }
    }

    const RUNNING_PROTOCOL: &str = r#"
read request
printf '%s\n' '{"id":1,"result":{"session":{"sessionId":"running-session"}}}'
read request
printf '%s\n' '{"id":2,"result":{}}'
read request
printf '%s\n' '{"id":3,"result":{}}' '{"method":"session/event","params":{"type":"turn.started"}}'
sleep 0.1
"#;

    /// One started turn settled by a model-reported failure boundary.
    const FAILED_TURN_PROTOCOL: &str = r#"
read request
printf '%s\n' '{"id":1,"result":{"session":{"sessionId":"failed-session"}}}'
read request
printf '%s\n' '{"id":2,"result":{}}'
read request
printf '%s\n' '{"id":3,"result":{}}' '{"method":"session/event","params":{"type":"turn.started"}}'
printf failed-turn-tail >&2
printf '%s\n' '{"method":"session/event","params":{"type":"turn.failed"}}'
sleep 2
"#;

    /// A completed turn that never verifies a visible final text.
    const MISSING_TEXT_PROTOCOL: &str = r#"
read request
printf '%s\n' '{"id":1,"result":{"session":{"sessionId":"missing-session"}}}'
read request
printf '%s\n' '{"id":2,"result":{}}'
read request
printf '%s\n' '{"id":3,"result":{}}' '{"method":"session/event","params":{"type":"turn.started"}}'
printf missing-text-tail >&2
printf '%s\n' '{"method":"session/event","params":{"type":"turn.completed"}}'
sleep 2
"#;

    #[test]
    fn failed_turn_persists_a_bounded_failure_record_in_the_task_row() {
        let (_workspace, scheduler, agent_id) = diagnostic_scheduler(FAILED_TURN_PROTOCOL);
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        let result = await_result(&scheduler, &agent_id);
        assert_eq!(result.result.outcome, TaskOutcome::Failed);
        assert_eq!(result.result.final_text, "RUNTIME_TERMINAL");
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        let raw = task.failure_message.expect("persisted failure detail");
        assert!(raw.len() <= PERSISTABLE_RECORD_BYTES);
        assert!(!raw.contains('\n'));
        let persisted: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(persisted["stage"], "runtime_terminal");
        assert_eq!(persisted["error_code"], "RUNTIME_TERMINAL");
        assert_eq!(persisted["session_id"], "failed-session");
        assert!(persisted["stderr_tail"]
            .as_str()
            .unwrap()
            .contains("failed-turn-tail"));
        // The immutable result keeps the bare reason code (final_text
        // semantics unchanged).
        assert_eq!(result.result.final_text, "RUNTIME_TERMINAL");
    }

    #[test]
    fn natural_result_invalid_persists_final_text_missing_detail() {
        let (_workspace, scheduler, agent_id) = diagnostic_scheduler(MISSING_TEXT_PROTOCOL);
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        let result = await_result(&scheduler, &agent_id);
        assert_eq!(result.result.outcome, TaskOutcome::ResultInvalid);
        assert_eq!(
            result.result.final_text,
            "runtime completed without visible final text"
        );
        assert_eq!(
            scheduler
                .store()
                .terminal_reason_code(&agent_id)
                .unwrap()
                .as_deref(),
            Some("FINAL_TEXT_MISSING")
        );
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        assert_eq!(task.outcome, Some(TaskOutcome::ResultInvalid));
        let raw = task.failure_message.expect("persisted failure detail");
        let persisted: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(persisted["stage"], "runtime_terminal");
        assert_eq!(persisted["error_code"], "FINAL_TEXT_MISSING");
        assert!(persisted["message"]
            .as_str()
            .unwrap()
            .contains("without visible final text"));
        assert!(persisted["stderr_tail"]
            .as_str()
            .unwrap()
            .contains("missing-text-tail"));
    }

    #[test]
    fn spawn_failure_persists_preparation_detail_with_empty_tail() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        fs::create_dir_all(&root).unwrap();
        let workspace = tempfile::Builder::new()
            .prefix("spawn-failure-")
            .tempdir_in(root)
            .unwrap();
        let store = Arc::new(Store::open(workspace.path().join("state.sqlite")).unwrap());
        let factory = Arc::new(CommandRuntimeFactory::new(
            |_: &TaskRecord| -> io::Result<Command> {
                Err(io::Error::other("synthetic spawn refusal"))
            },
        ));
        let scheduler = Scheduler::new(
            "spawn-failure",
            store,
            factory,
            SchedulerConfig::default(),
        )
        .unwrap();
        let submitted = scheduler
            .enqueue_general(&GeneralTaskManifest {
                schema: "zcode-general-task/v1".into(),
                agent_id: "10000007".into(),
                repository: workspace.path().canonicalize().unwrap(),
                permission_mode: external_core::PermissionMode::Plan,
                prompt: "spawn failure fixture".into(),
                write_manifest: Vec::new(),
            })
            .unwrap();
        assert!(scheduler.start_ready().is_err());
        let task = scheduler
            .store()
            .get_task(&submitted.agent_id)
            .unwrap()
            .unwrap();
        assert_eq!(task.outcome, Some(TaskOutcome::Failed));
        let raw = task.failure_message.expect("persisted failure detail");
        let persisted: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(persisted["stage"], "spawn");
        assert_eq!(persisted["error_code"], "RUNTIME_SPAWN_FAILED");
        // No runtime ever existed: never fabricate a session or tail.
        assert_eq!(persisted["session_id"], serde_json::Value::Null);
        assert_eq!(persisted["stderr_tail"], "");
        assert_eq!(persisted["tail_truncated"], false);
    }

    #[test]
    fn failed_turn_message_delivery_persists_remote_detail_and_fails_the_task() {
        struct DeliveryFailRuntime;
        impl ManagedRuntime for DeliveryFailRuntime {
            fn identity(&self) -> Option<ProcessIdentity> {
                None
            }
            fn stop(&self, _: Duration) -> RuntimeTerminal {
                RuntimeTerminal::FailedTurn(StopOutcome::AlreadyExited(ChildExit::Exited(Some(1))))
            }
            fn wait_terminal(&self, _: Duration) -> Option<RuntimeTerminal> {
                None
            }
            fn turn_snapshot(&self) -> TurnSnapshot {
                TurnSnapshot {
                    generation: 1,
                    active: false,
                    boundary: Some(TurnBoundary::Failed),
                }
            }
            fn bootstrap_session(
                &self,
                _: &TaskRecord,
                _: Duration,
            ) -> Result<SessionReady, RuntimeCommandError> {
                Ok(SessionReady {
                    session_id: "delivery-session".into(),
                    initial_turn_id: None,
                    configured_model: None,
                })
            }
            fn send_turn(
                &self,
                _: &str,
                _: &str,
                _: Duration,
            ) -> Result<Option<String>, RuntimeCommandError> {
                Err(RuntimeCommandError::Remote(serde_json::json!({
                    "code": -32031,
                    "message": "delivery rejected with remote detail"
                })))
            }
            fn diagnostic_tail(&self) -> String {
                "delivery-tail".into()
            }
            fn diagnostic_session_id(&self) -> Option<String> {
                Some("delivery-session".into())
            }
        }
        struct DeliveryFailFactory;
        impl RuntimeFactory for DeliveryFailFactory {
            fn spawn(
                &self,
                _: &TaskRecord,
                _: Arc<dyn LifecycleSink>,
            ) -> io::Result<Arc<dyn ManagedRuntime>> {
                Ok(Arc::new(DeliveryFailRuntime))
            }
        }
        let (_workspace, mut scheduler, agent_id) = diagnostic_scheduler("unused");
        Arc::get_mut(&mut scheduler.inner).unwrap().factory = Arc::new(DeliveryFailFactory);
        scheduler
            .store()
            .insert_message("queued-delivery", &agent_id, "queue", "follow-up")
            .unwrap();
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        let result = await_result(&scheduler, &agent_id);
        assert_eq!(result.result.outcome, TaskOutcome::Failed);
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        // The terminal message-delivery failure hands its already-built detail
        // to the routed closure instead of inspecting only after persistence.
        let raw = task.failure_message.expect("persisted delivery detail");
        let persisted: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(persisted["error_code"], "MESSAGE_DELIVERY_FAILED");
        assert_eq!(persisted["session_id"], "delivery-session");
        assert!(persisted["stderr_tail"]
            .as_str()
            .unwrap()
            .contains("delivery-tail"));
        assert!(persisted["remote_message"]
            .as_str()
            .unwrap()
            .contains("delivery rejected"));
        assert_eq!(persisted["remote_code"], -32031);
    }

    #[test]
    fn claim_prepared_decode_failure_persists_preparation_detail() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        fs::create_dir_all(&root).unwrap();
        let workspace = tempfile::Builder::new()
            .prefix("prepared-failure-")
            .tempdir_in(root)
            .unwrap();
        let repository = workspace.path().canonicalize().unwrap();
        let store = Arc::new(Store::open(workspace.path().join("state.sqlite")).unwrap());
        let factory = Arc::new(CommandRuntimeFactory::new(
            |_: &TaskRecord| -> io::Result<Command> {
                panic!("an undecodable preparation must never spawn")
            },
        ));
        let scheduler = Scheduler::new(
            "prepared-failure",
            Arc::clone(&store),
            factory,
            SchedulerConfig::default(),
        )
        .unwrap();
        store
            .enqueue_task_authoritative(&NewTask {
                agent_id: "10000009".into(),
                repository: repository.to_string_lossy().into_owned(),
                workspace_path: repository.to_string_lossy().into_owned(),
                runtime_hash: None,
                prepared_launch_json: "{not valid json".into(),
                initial_prompt: "undecodable".into(),
            })
            .unwrap();
        assert!(scheduler.start_ready().is_err());
        let task = store.get_task("10000009").unwrap().unwrap();
        assert_eq!(task.outcome, Some(TaskOutcome::ResultInvalid));
        let raw = task.failure_message.expect("persisted failure detail");
        let persisted: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(persisted["stage"], "preparation");
        assert_eq!(persisted["error_code"], "PREPARED_LAUNCH_INVALID");
    }

    #[test]
    fn resumed_policy_launcher_failure_persists_preparation_detail() {
        let (_workspace, scheduler, agent_id) = diagnostic_scheduler("unused");
        let store = scheduler.store();
        let claim = store.claim_next("old-daemon", 10, 1).unwrap().unwrap();
        assert!(store
            .mark_session_running(
                &agent_id,
                claim.owner_epoch,
                "old-runtime",
                None,
                Some("policy-session"),
                None,
            )
            .unwrap());
        store
            .store_task_result(
                &agent_id,
                &external_store::TaskResult {
                    outcome: TaskOutcome::Failed,
                    final_text: "prior turn failed".into(),
                    partial: true,
                },
            )
            .unwrap();
        assert!(store
            .requeue_task_for_resume_with_message(&agent_id, "resume-policy", "queue", "continue")
            .unwrap());
        let task = store.get_task(&agent_id).unwrap().unwrap();
        let prepared: external_core::PreparedGeneralTask =
            serde_json::from_str(&task.prepared_launch_json).unwrap();
        let scratch = prepared.workspace.scratch_root.clone();
        // A resume with an existing scratch path that is now a plain file
        // still decodes but fails `resume_launcher`'s create_dir_all.
        fs::remove_dir_all(&scratch).unwrap();
        fs::write(&scratch, b"not a directory").unwrap();
        let error = scheduler.start_ready().unwrap_err();
        assert!(matches!(error, SchedulerError::InvalidConfig(_)));
        let task = store.get_task(&agent_id).unwrap().unwrap();
        assert_eq!(task.outcome, Some(TaskOutcome::ResultInvalid));
        let raw = task.failure_message.expect("persisted failure detail");
        let persisted: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(persisted["stage"], "preparation");
        assert_eq!(persisted["error_code"], "PREPARED_CONTENT_INVALID");
        // No runtime was built; the persisted session is the honest evidence.
        assert_eq!(persisted["session_id"], "policy-session");
        assert_eq!(persisted["stderr_tail"], "");
    }

    #[test]
    fn persist_failure_fallback_carries_the_store_error_evidence() {
        let (workspace, scheduler, agent_id) =
            diagnostic_scheduler("read request; printf bootstrap-detail >&2; exit 7");
        {
            let connection =
                rusqlite::Connection::open(workspace.path().join("state.sqlite")).unwrap();
            connection
                .execute_batch(
                    "CREATE TRIGGER reject_failed_result BEFORE INSERT ON task_results
                     WHEN NEW.outcome='FAILED'
                     BEGIN SELECT RAISE(ABORT, 'injected persist failure'); END;",
                )
                .unwrap();
        }
        assert!(scheduler.start_ready().is_err());
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        assert_eq!(task.outcome, Some(TaskOutcome::ResultInvalid));
        let result = scheduler
            .store()
            .task_result(&agent_id)
            .unwrap()
            .unwrap()
            .result;
        // The bounded placeholder keeps its original final_text/hash.
        assert_eq!(result.final_text, "result unavailable");
        assert!(result.partial);
        let raw = task.failure_message.expect("persisted persistence error");
        let persisted: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(persisted["stage"], "result_persist");
        assert_eq!(persisted["error_code"], "RESULT_PERSIST_FAILED");
        assert!(persisted["message"]
            .as_str()
            .unwrap()
            .contains("injected persist failure"));
    }

    #[test]
    fn zcode_steer_stops_then_sends() {
        let (workspace, mut scheduler, agent_id) = diagnostic_scheduler("unused");
        let directory = workspace.path().to_owned();
        let script = format!(
            r#"{RUNNING_PROTOCOL}
read request
printf '%s\n' "$request" >> steer.jsonl
printf '%s\n' '{{"id":4,"result":{{}}}}' '{{"method":"session/event","params":{{"type":"turn.failed"}}}}'
read request
printf '%s\n' "$request" >> steer.jsonl
printf '%s\n' '{{"id":5,"result":{{"turnId":"steered-turn"}}}}' '{{"method":"session/event","params":{{"type":"turn.started"}}}}'
while read request; do
  request_id=$(printf '%s' "$request" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  printf '%s\n' "{{\"id\":$request_id,\"result\":{{}}}}" '{{"method":"session/event","params":{{"type":"turn.failed"}}}}'
done
"#
        );
        Arc::get_mut(&mut scheduler.inner).unwrap().factory =
            Arc::new(CommandRuntimeFactory::new(move |_: &TaskRecord| {
                let mut command = Command::new("sh");
                command.args(["-c", &script]).current_dir(&directory);
                Ok(command)
            }));
        scheduler.start_ready().unwrap();
        assert_eq!(
            scheduler
                .send_message(&agent_id, "buffered", "queue", "later")
                .unwrap(),
            MessageDisposition::Queued
        );
        assert_eq!(
            scheduler
                .send_message(&agent_id, "steered", "steer", "new direction")
                .unwrap(),
            MessageDisposition::Delivered
        );
        let frames = fs::read_to_string(workspace.path().join("steer.jsonl"))
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str::<serde_json::Value>(s).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(frames[0]["method"], "session/stop");
        assert_eq!(frames[1]["method"], "session/send");
        assert_eq!(frames[1]["params"]["content"], "new direction");
        assert_eq!(
            scheduler
                .store()
                .message("steered")
                .unwrap()
                .unwrap()
                .target_turn_id
                .as_deref(),
            Some("steered-turn")
        );
        assert_eq!(
            scheduler
                .store()
                .message("buffered")
                .unwrap()
                .unwrap()
                .state,
            MessageState::Queued
        );
        scheduler.cancel_task(&agent_id).unwrap();
    }

    #[test]
    fn zcode_steer_failures_settle_the_claim_with_the_failing_operation() {
        for failure in ["stop", "send"] {
            let (workspace, mut scheduler, agent_id) = diagnostic_scheduler("unused");
            let directory = workspace.path().to_owned();
            let reply = if failure == "stop" {
                r#"printf '%s\n' '{"id":4,"error":{"code":-32001,"message":"stop refused"}}'"#
            } else {
                r#"printf '%s\n' '{"id":4,"result":{}}' '{"method":"session/event","params":{"type":"turn.failed"}}'
read request
printf '%s\n' '{"id":5,"error":{"code":-32002,"message":"send refused"}}'"#
            };
            let script = format!(
                r#"{RUNNING_PROTOCOL}
read request
{reply}
while read request; do
  request_id=$(printf '%s' "$request" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  printf '%s\n' "{{\"id\":$request_id,\"result\":{{}}}}" '{{"method":"session/event","params":{{"type":"turn.failed"}}}}'
done
"#
            );
            Arc::get_mut(&mut scheduler.inner).unwrap().factory =
                Arc::new(CommandRuntimeFactory::new(move |_: &TaskRecord| {
                    let mut command = Command::new("sh");
                    command.args(["-c", &script]).current_dir(&directory);
                    Ok(command)
                }));
            scheduler.start_ready().unwrap();
            assert!(scheduler
                .send_message(&agent_id, "failed-steer", "steer", "new direction")
                .is_err());
            let receipt = scheduler.store().message("failed-steer").unwrap().unwrap();
            assert_eq!(receipt.state, MessageState::Failed);
            assert_eq!(
                receipt.failure_code.as_deref(),
                Some(if failure == "stop" {
                    "SESSION_STOP_FAILED"
                } else {
                    "SESSION_SEND_FAILED"
                })
            );
            assert!(receipt.delivered_at.is_none());
            assert_eq!(
                scheduler
                    .send_message(&agent_id, "failed-steer", "steer", "new direction")
                    .unwrap(),
                MessageDisposition::Failed
            );
            if failure == "stop" {
                assert_eq!(
                    scheduler
                        .store()
                        .get_task(&agent_id)
                        .unwrap()
                        .unwrap()
                        .phase,
                    TaskPhase::Running
                );
            }
            scheduler.cancel_task(&agent_id).unwrap();
        }
    }

    #[test]
    fn scheduler_queue_drains_once_through_driver_and_persists_receipt() {
        let (workspace, mut scheduler, agent_id) = diagnostic_scheduler("unused");
        let directory = workspace.path().to_owned();
        let script = format!(
            r#"{RUNNING_PROTOCOL}
while [ ! -f release-turn ]; do sleep 0.01; done
printf '%s\n' '{{"method":"session/event","params":{{"type":"turn.completed"}}}}'
read request
printf '%s\n' "$request" >> deliveries.jsonl
printf '%s\n' '{{"id":4,"result":{{"turnId":"queued-turn"}}}}' '{{"method":"session/event","params":{{"type":"turn.started"}}}}'
while [ ! -f finish-turn ]; do sleep 0.01; done
printf '%s\n' '{{"method":"session/event","params":{{"type":"model.streaming","payload":{{"kind":"text_delta","delta":"queued answer","assistantMessageId":"m2"}}}}}}' '{{"method":"session/event","params":{{"type":"message.finished","payload":{{"assistantMessageId":"m2"}}}}}}' '{{"method":"session/event","params":{{"type":"turn.completed"}}}}'
while read request; do printf '%s\n' "$request" >> deliveries.jsonl; done
"#
        );
        Arc::get_mut(&mut scheduler.inner).unwrap().factory =
            Arc::new(CommandRuntimeFactory::new(move |_: &TaskRecord| {
                let mut command = Command::new("sh");
                command.args(["-c", &script]).current_dir(&directory);
                Ok(command)
            }));
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        for _ in 0..2 {
            assert_eq!(
                scheduler
                    .send_message(&agent_id, "counted", "queue", "follow-up")
                    .unwrap(),
                MessageDisposition::Queued
            );
        }
        fs::write(workspace.path().join("release-turn"), "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let receipt = loop {
            let receipt = scheduler.store().message("counted").unwrap().unwrap();
            if receipt.state == MessageState::Delivered {
                break receipt;
            }
            assert!(
                Instant::now() < deadline,
                "queue was not delivered: {receipt:?}"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(receipt.target_turn_id.as_deref(), Some("queued-turn"));
        assert!(receipt.delivered_at.is_some());
        assert_eq!(
            scheduler
                .send_message(&agent_id, "counted", "queue", "follow-up")
                .unwrap(),
            MessageDisposition::AlreadyDelivered
        );
        fs::write(workspace.path().join("finish-turn"), "").unwrap();
        assert_eq!(
            await_result(&scheduler, &agent_id).result.final_text,
            "queued answer"
        );
        let deliveries = fs::read_to_string(workspace.path().join("deliveries.jsonl")).unwrap();
        let sends: Vec<serde_json::Value> = deliveries
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|v: &serde_json::Value| v["method"] == "session/send")
            .collect();
        assert_eq!(sends.len(), 1, "duplicate queue delivery: {deliveries}");
        assert!(sends[0].to_string().contains("follow-up"));
        assert_eq!(
            scheduler.store().message("counted").unwrap().unwrap(),
            receipt
        );
    }

    #[test]
    fn answerable_user_input_executes_answer_content_through_the_zcode_runtime_seam() {
        let (workspace, mut scheduler, agent_id) = diagnostic_scheduler("unused");
        let directory = workspace.path().to_owned();
        let script = format!(
            r#"{RUNNING_PROTOCOL}
printf '%s\n' '{{"id":"srv-input-1","method":"interaction/requestUserInput","params":{{"question":"which scope?"}}}}'
read answer
printf '%s\n' "$answer" >> deliveries.jsonl
while [ ! -f release-turn ]; do sleep 0.01; done
printf '%s\n' '{{"method":"session/event","params":{{"type":"model.streaming","payload":{{"kind":"text_delta","delta":"answered with the release scope","assistantMessageId":"m2"}}}}}}' '{{"method":"session/event","params":{{"type":"message.finished","payload":{{"assistantMessageId":"m2"}}}}}}' '{{"method":"session/event","params":{{"type":"turn.completed"}}}}'
while read request; do printf '%s\n' "$request" >> deliveries.jsonl; done
"#
        );
        Arc::get_mut(&mut scheduler.inner).unwrap().factory =
            Arc::new(CommandRuntimeFactory::new(move |_: &TaskRecord| {
                let mut command = Command::new("sh");
                command.args(["-c", &script]).current_dir(&directory);
                Ok(command)
            }));
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        let request = {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(request) = scheduler
                    .store()
                    .pending_requests(&agent_id)
                    .unwrap()
                    .first()
                {
                    assert_eq!(request.request_type, "user_input");
                    assert_eq!(request.state, PendingRequestState::Pending);
                    break request.clone();
                }
                assert!(Instant::now() < deadline, "user input never became pending");
                thread::sleep(Duration::from_millis(10));
            }
        };
        // Decision/type mismatches fail closed before the runtime seam.
        for (decision, content) in [("allow", None), ("answer", Some("  "))] {
            assert!(
                scheduler
                    .respond_request(&agent_id, &request.request_id, decision, content)
                    .is_err(),
                "{decision} must not execute against a user_input request"
            );
        }
        assert_eq!(
            scheduler
                .respond_request(
                    &agent_id,
                    &request.request_id,
                    "answer",
                    Some("use the release scope")
                )
                .unwrap(),
            ResponseOutcome {
                disposition: ResponseDisposition::Responded,
                requested_decision: "answer".into(),
                effective_decision: "answer".into(),
                policy_overrode: false,
                policy_reason_code: None,
            }
        );
        fs::write(workspace.path().join("release-turn"), "").unwrap();
        assert_eq!(
            await_result(&scheduler, &agent_id).result.final_text,
            "answered with the release scope"
        );
        // The runtime received the answer as the plain JSON-RPC result.
        let deliveries = fs::read_to_string(workspace.path().join("deliveries.jsonl")).unwrap();
        let answer: serde_json::Value = serde_json::from_str(
            deliveries
                .lines()
                .find(|line| line.contains("srv-input-1"))
                .expect("runtime never observed the answer frame"),
        )
        .unwrap();
        assert_eq!(answer["id"], "srv-input-1");
        assert_eq!(answer["result"], "use the release scope");
    }

    #[test]
    fn scheduler_restart_reaps_runtime_without_replaying_unknown_sending() {
        for cancelled in [false, true] {
            let (workspace, scheduler, agent_id) = diagnostic_scheduler("unused");
            let store = scheduler.store();
            let claim = store.claim_next("old-daemon", 1, 1).unwrap().unwrap();
            // An actual isolated child supplies the persisted identity. No monitor is
            // attached: this models loss of the daemon after claiming delivery.
            let mut command = Command::new("sh");
            command.args(["-c", "exec sleep 30"]);
            let driver = Driver::spawn(command).unwrap();
            let identity = driver.identity();
            assert!(store
                .mark_session_running(
                    &agent_id,
                    claim.owner_epoch,
                    "old-runtime",
                    Some(&StoredProcessIdentity {
                        pid: identity.pid,
                        process_group_id: identity.pgid,
                        uid: identity.uid,
                        start_token: identity.start_token.clone(),
                    }),
                    Some("old-session"),
                    Some(TurnState::Active)
                )
                .unwrap());
            scheduler
                .send_message(&agent_id, "unknown", "queue", "never replay")
                .unwrap();
            assert_eq!(
                store.claim_next_message(&agent_id).unwrap().unwrap().state,
                MessageState::Sending
            );
            if cancelled {
                store.request_stop(&agent_id).unwrap();
            }
            drop(scheduler);
            drop(store);
            let reopened = Arc::new(Store::open(workspace.path().join("state.sqlite")).unwrap());
            let spawns = Arc::new(AtomicU64::new(0));
            let count = Arc::clone(&spawns);
            let factory = CommandRuntimeFactory::new(move |_: &TaskRecord| {
                count.fetch_add(1, Ordering::SeqCst);
                Err(io::Error::other("recovery must not spawn or replay"))
            });
            let recovered = Scheduler::new(
                "new-daemon",
                Arc::clone(&reopened),
                Arc::new(factory),
                SchedulerConfig::default(),
            )
            .unwrap();
            let expected = if cancelled {
                TaskOutcome::Cancelled
            } else {
                TaskOutcome::RuntimeLost
            };
            assert_eq!(
                recovered.reconcile_startup().unwrap(),
                vec![(agent_id.clone(), expected)]
            );
            let task = reopened.get_task(&agent_id).unwrap().unwrap();
            assert_eq!(task.phase, TaskPhase::Terminal);
            assert_eq!(task.outcome, Some(expected));
            assert!(task.reaped_at.is_some());
            assert!(observe_process_group(identity.pgid).unwrap().is_empty());
            driver.wait().unwrap();
            let receipt = reopened.message("unknown").unwrap().unwrap();
            assert_eq!(receipt.state, MessageState::Failed);
            assert!(receipt.delivered_at.is_none());
            assert!(!reopened
                .complete_message("unknown", Some("late-turn"))
                .unwrap());
            assert!(recovered.start_ready().unwrap().is_empty());
            assert!(recovered.reconcile_startup().unwrap().is_empty());
            assert_eq!(spawns.load(Ordering::SeqCst), 0);
            assert_eq!(
                reopened
                    .task_result(&agent_id)
                    .unwrap()
                    .unwrap()
                    .result
                    .outcome,
                expected
            );
            // Startup recovery persists a record for the RuntimeLost rows and
            // never invents one for the cancelled row.
            if cancelled {
                assert_eq!(task.failure_message, None);
            } else {
                let raw = task.failure_message.expect("recovery failure detail");
                let persisted: serde_json::Value = serde_json::from_str(&raw).unwrap();
                assert_eq!(persisted["stage"], "recovery");
                assert_eq!(persisted["error_code"], "DAEMON_RESTART_RUNTIME_LOST");
                assert_eq!(persisted["session_id"], "old-session");
                // No runtime survives a restart: never claim a stderr tail.
                assert_eq!(persisted["stderr_tail"], "");
            }
        }
    }

    #[test]
    fn abnormal_exit_records_correlated_tail_and_preserves_runtime_lost_outcome() {
        let script = format!("{RUNNING_PROTOCOL}\nprintf abnormal-exit-tail >&2; exit 7");
        let (_workspace, scheduler, agent_id) = diagnostic_scheduler(&script);
        scheduler.start_ready().unwrap();
        let result = await_result(&scheduler, &agent_id);
        let record = failure_record(&scheduler, &agent_id);
        assert_eq!(record["stage"], "runtime_terminal");
        assert_eq!(record["session_id"], "running-session");
        assert_eq!(record["error_code"], "RUNTIME_TERMINAL");
        assert!(record["stderr_tail"]
            .as_str()
            .unwrap()
            .contains("abnormal-exit-tail"));
        assert!(!serde_json::to_string(&result.result)
            .unwrap()
            .contains("abnormal-exit-tail"));
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        assert_eq!(task.outcome, Some(TaskOutcome::RuntimeLost));
        // The persistable variant lands in the task row with the same
        // correlation and the latest stderr suffix.
        let raw = task.failure_message.expect("persisted failure detail");
        assert!(raw.len() <= PERSISTABLE_RECORD_BYTES);
        assert!(!raw.contains('\n'));
        let persisted: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(persisted["stage"], "runtime_terminal");
        assert_eq!(persisted["error_code"], "RUNTIME_TERMINAL");
        assert_eq!(persisted["session_id"], "running-session");
        assert!(persisted["stderr_tail"]
            .as_str()
            .unwrap()
            .contains("abnormal-exit-tail"));
        // The immutable result carries neither the detail nor the stderr.
        assert!(!serde_json::to_string(&result.result)
            .unwrap()
            .contains("abnormal-exit-tail"));
    }

    #[test]
    fn successful_runtime_stderr_is_not_published_as_failure_or_final_text() {
        let script = format!(
            r#"{RUNNING_PROTOCOL}
printf normal-stderr-tail >&2
printf '%s\n' '{{"method":"session/event","params":{{"type":"model.streaming","payload":{{"kind":"text_delta","delta":"task answer","assistantMessageId":"m1"}}}}}}' '{{"method":"session/event","params":{{"type":"message.finished","payload":{{"assistantMessageId":"m1"}}}}}}' '{{"method":"session/event","params":{{"type":"turn.completed"}}}}'
sleep 2
"#
        );
        let (_workspace, scheduler, agent_id) = diagnostic_scheduler(&script);
        scheduler.start_ready().unwrap();
        let result = await_result(&scheduler, &agent_id);
        assert!(scheduler.last_error(&agent_id).is_none());
        let result_json = serde_json::to_string(&result.result).unwrap();
        assert!(result_json.contains("task answer"));
        assert!(!result_json.contains("normal-stderr-tail"));
        assert_eq!(
            scheduler
                .store()
                .get_task(&agent_id)
                .unwrap()
                .unwrap()
                .outcome,
            Some(TaskOutcome::Completed)
        );
    }

    #[test]
    fn natural_completion_queued_send_failure_records_tail_without_changing_outcomes() {
        struct CompletedRuntime;
        impl ManagedRuntime for CompletedRuntime {
            fn identity(&self) -> Option<ProcessIdentity> {
                None
            }
            fn stop(&self, _: Duration) -> RuntimeTerminal {
                RuntimeTerminal::Completed(StopOutcome::AlreadyExited(ChildExit::Exited(Some(0))))
            }
            fn wait_terminal(&self, _: Duration) -> Option<RuntimeTerminal> {
                Some(self.stop(Duration::ZERO))
            }
            fn bootstrap_session(
                &self,
                _: &TaskRecord,
                _: Duration,
            ) -> Result<SessionReady, RuntimeCommandError> {
                Ok(SessionReady {
                    session_id: "completed-session".into(),
                    initial_turn_id: None,
                    configured_model: None,
                })
            }
            fn send_turn(
                &self,
                _: &str,
                _: &str,
                _: Duration,
            ) -> Result<Option<String>, RuntimeCommandError> {
                Err(RuntimeCommandError::Remote(serde_json::json!({
                    "code": -32031, "message": "ZCODE_RUNTIME_MODEL_UNAVAILABLE api_key=secret-value",
                    "data": {"provider": "must-not-record"}
                })))
            }
            fn diagnostic_tail(&self) -> String {
                "queued-send-tail".into()
            }
        }
        struct CompletedFactory;
        impl RuntimeFactory for CompletedFactory {
            fn spawn(
                &self,
                _: &TaskRecord,
                sink: Arc<dyn LifecycleSink>,
            ) -> io::Result<Arc<dyn ManagedRuntime>> {
                for (index, params) in [
                    serde_json::json!({"type":"model.streaming", "payload":{"kind":"text_delta", "delta":"completed answer", "assistantMessageId":"m1"}}),
                    serde_json::json!({"type":"message.finished", "payload":{"assistantMessageId":"m1"}}),
                ].into_iter().enumerate() {
                    sink.emit(LifecycleRecord {
                        sequence: index as u64 + 1,
                        event: RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(external_contract::EventEnvelope { method: "session/event".into(), params }))),
                    });
                }
                Ok(Arc::new(CompletedRuntime))
            }
        }
        let (_workspace, mut scheduler, agent_id) = diagnostic_scheduler("unused");
        Arc::get_mut(&mut scheduler.inner).unwrap().factory = Arc::new(CompletedFactory);
        scheduler
            .store()
            .insert_message("queued-after-completion", &agent_id, "queue", "follow-up")
            .unwrap();
        scheduler.start_ready().unwrap();
        let result = await_result(&scheduler, &agent_id);
        let record = failure_record(&scheduler, &agent_id);
        assert_eq!(record["stage"], "message_delivery");
        assert_eq!(record["error_code"], "SESSION_SEND_FAILED");
        assert_eq!(record["session_id"], "completed-session");
        assert_eq!(record["stderr_tail"], "queued-send-tail");
        assert_eq!(record["operation"], "session/send");
        assert_eq!(record["remote_code"], -32031);
        assert!(record["remote_message"]
            .as_str()
            .unwrap()
            .contains("ZCODE_RUNTIME_MODEL_UNAVAILABLE"));
        assert!(record.to_string().contains("secret-value"));
        assert!(!record.to_string().contains("must-not-record"));
        assert_eq!(result.result.outcome, TaskOutcome::Completed);
        assert_eq!(result.result.final_text, "completed answer");
        // A message-only failure on a task that still completes must never
        // reach tasks.failure_message.
        assert_eq!(
            scheduler
                .store()
                .get_task(&agent_id)
                .unwrap()
                .unwrap()
                .failure_message,
            None
        );
        let message = scheduler
            .store()
            .message("queued-after-completion")
            .unwrap()
            .unwrap();
        assert_eq!(message.state, external_store::MessageState::Failed);
        assert_eq!(message.failure_code.as_deref(), Some("SESSION_SEND_FAILED"));
    }

    #[test]
    fn message_id_collision_rejects_other_content_or_agent_without_mutation() {
        let (_workspace, scheduler, agent_id) = diagnostic_scheduler("unused");
        scheduler
            .store()
            .insert_message("existing", &agent_id, "queue", "original")
            .unwrap();
        for (agent, content) in [
            (agent_id.as_str(), "changed"),
            ("different-agent", "original"),
        ] {
            let error = scheduler
                .send_message(agent, "existing", "queue", content)
                .unwrap_err();
            assert!(
                matches!(error, SchedulerError::Store(StoreError::Conflict(ref message)) if message == "MESSAGE_ID_CONFLICT")
            );
        }
        let message = scheduler.store().message("existing").unwrap().unwrap();
        assert_eq!(message.agent_id, agent_id);
        assert_eq!(message.content, "original");
        assert_eq!(message.state, MessageState::Queued);
        assert_eq!(
            scheduler
                .send_message(&agent_id, "existing", "queue", "original")
                .unwrap(),
            MessageDisposition::Queued
        );
    }

    #[test]
    fn remote_rejection_preserves_bounded_message_and_separates_cleanup() {
        let error = RuntimeCommandError::Remote(serde_json::json!({
            "code": -32031,
            "message": "unavailable Authorization: Bearer abc-secret password=def-secret api_key=\"quoted secret words\" https://user:pass@host/path",
            "data": {"api_key": "do-not-copy"}
        }));
        let mut detail: serde_json::Value =
            serde_json::from_str(&error.diagnostic("session/send")).unwrap();
        detail["cleanup_result"] = "Stopped(Terminated(Signaled(15)))".into();
        let record = runtime_failure_record(
            "a",
            Some("s"),
            "runtime_terminal",
            "SESSION_SEND_FAILED",
            &detail.to_string(),
            "stderr-end",
        );
        let value: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert_eq!(value["remote_code"], -32031);
        assert_eq!(value["operation"], "session/send");
        assert_eq!(value["cleanup_result"], "Stopped(Terminated(Signaled(15)))");
        let remote_message = value["remote_message"].as_str().unwrap();
        assert!(remote_message.contains("abc-secret"));
        assert!(remote_message.contains("def-secret"));
        assert!(remote_message.contains("quoted secret words"));
        assert!(remote_message.contains("user:pass"));
        assert!(!record.contains("do-not-copy"));
        assert!(bounded_prefix(&"界".repeat(5000), 1024).len() <= 1024);
    }

    #[test]
    fn known_driver_loss_marks_observation_coverage_without_query_side_effects() {
        let tracker = PassiveActivityTracker::new(true);
        tracker.observe(&RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(
            external_contract::EventEnvelope {
                method: "session/event".into(),
                params: serde_json::json!({
                    "type":"model.streaming",
                    "eventId":"reasoning-1",
                    "turnId":"turn-1",
                    "payload":{"kind":"reasoning_delta", "delta":"visible"}
                }),
            },
        ))));
        let before = tracker.observation_snapshot();
        assert!(before.coverage.tool_history_complete);
        assert!(before.coverage.reasoning_complete);

        tracker.observe(&RuntimeEvent::Driver(Inbound::Malformed(
            "invalid JSON".into(),
        )));
        tracker.observe(&RuntimeEvent::Driver(Inbound::OversizedLine {
            bytes: 1024 * 1024 + 1,
        }));
        let after = tracker.observation_snapshot();
        assert!(!after.coverage.tool_history_complete);
        assert!(!after.coverage.reasoning_complete);
        assert_eq!(after.coverage.dropped_events, 2);
        assert_eq!(after.snapshot_seq, before.snapshot_seq + 2);
        assert_eq!(tracker.observation_snapshot(), after);
        assert_eq!(tracker.observation_snapshot(), after);
    }

    #[test]
    fn terminal_text_is_scoped_to_the_settling_turn() {
        let tracker = PassiveActivityTracker::new(true);
        let session_event = |params: serde_json::Value| {
            RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(
                external_contract::EventEnvelope {
                    method: "session/event".into(),
                    params,
                },
            )))
        };

        // Turn 1 streams its answer, settles with the verified final text, and
        // a streaming echo racing past the boundary must not duplicate it.
        tracker.observe(&session_event(serde_json::json!({"type": "turn.started"})));
        tracker.observe(&session_event(serde_json::json!({
            "type": "model.streaming",
            "eventId": "delta-1",
            "payload": {"kind": "text_delta", "delta": "build ", "assistantMessageId": "m1"}
        })));
        tracker.observe(&session_event(serde_json::json!({
            "type": "model.streaming",
            "eventId": "delta-2",
            "payload": {"kind": "text_delta", "delta": "answer", "assistantMessageId": "m1"}
        })));
        tracker.observe(&session_event(serde_json::json!({
            "type": "turn.completed",
            "eventId": "boundary-1",
            "payload": {"response": "build answer"}
        })));
        tracker.observe(&session_event(serde_json::json!({
            "type": "model.streaming",
            "eventId": "delta-3",
            "payload": {"kind": "text_delta", "delta": "build answer", "assistantMessageId": "m1"}
        })));
        assert_eq!(
            tracker.take_terminal_text(),
            TerminalText::Visible("build answer".into())
        );

        // Turn 2 starts fresh: the first turn's text must never leak into the
        // follow-up turn's result.
        tracker.observe(&session_event(serde_json::json!({"type": "turn.started"})));
        tracker.observe(&session_event(serde_json::json!({
            "type": "model.streaming",
            "eventId": "delta-4",
            "payload": {"kind": "text_delta", "delta": "follow-up settled", "assistantMessageId": "m2"}
        })));
        tracker.observe(&session_event(serde_json::json!({
            "type": "turn.completed",
            "eventId": "boundary-2",
            "payload": {"response": "follow-up settled"}
        })));
        assert_eq!(
            tracker.take_terminal_text(),
            TerminalText::Visible("follow-up settled".into())
        );

        // A turn that never verifies a final text neither inherits the
        // previous turn's text nor fakes one.
        tracker.observe(&session_event(serde_json::json!({"type": "turn.started"})));
        tracker.observe(&session_event(serde_json::json!({
            "type": "turn.completed",
            "eventId": "boundary-3",
            "payload": {}
        })));
        assert_eq!(tracker.take_terminal_text(), TerminalText::Missing);
    }

    #[test]
    fn near_limit_observation_does_not_starve_the_shared_cancel_lifecycle_lock() {
        let lifecycle = Arc::new(RuntimeLifecycle::new(1));
        let tracker = Arc::new(PassiveActivityTracker::new(true));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker_lifecycle = Arc::clone(&lifecycle);
        let worker_tracker = Arc::clone(&tracker);
        let worker_barrier = Arc::clone(&barrier);
        let worker = thread::spawn(move || {
            let _admission = worker_lifecycle.admit_event().unwrap();
            worker_barrier.wait();
            worker_tracker.observe(&RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(
                external_contract::EventEnvelope {
                    method: "session/event".into(),
                    params: serde_json::json!({
                        "type":"model.streaming",
                        "eventId":"large-tool-1",
                        "turnId":"turn-1",
                        "payload":{
                            "kind":"tool_call",
                            "toolCallId":"call-1",
                            "toolName":"Bash",
                            "input":{"command":"x".repeat(900 * 1024)}
                        }
                    }),
                },
            ))));
        });
        barrier.wait();
        let started = Instant::now();
        lifecycle.request_stop(&TurnSnapshot {
            generation: 1,
            active: true,
            boundary: None,
        });
        let elapsed = started.elapsed();
        worker.join().unwrap();
        assert!(
            elapsed < Duration::from_secs(5),
            "cancel lifecycle lock waited {elapsed:?}"
        );
        assert!(tracker.observation_snapshot().tools[0].recent_calls[0].arguments_truncated);
    }

    #[test]
    fn escaped_runtime_failure_record_is_bounded_and_keeps_latest_stderr() {
        let large = "\0".repeat(30000);
        let record = runtime_failure_record(
            &large,
            Some(&large),
            &large,
            &large,
            &large,
            &(large.clone() + "END"),
        );
        assert!(record.len() < 192 * 1024);
        let record: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert!(record["stderr_tail"].as_str().unwrap().ends_with("END"));
        assert!(record["stderr_tail"].as_str().unwrap().len() <= 16 * 1024);
        assert!(!record.to_string().contains('\n'));
    }

    fn escaped_worst_case_message() -> String {
        serde_json::json!({
            "message": "\u{1}".repeat(512),
            "operation": "\u{1}".repeat(192),
            "remote_message": "\u{1}".repeat(192),
            "cleanup_result": "\u{1}".repeat(192),
            "bytes": u64::MAX,
            "cap": u64::MAX,
            "last_event_seq": u64::MAX,
            "remote_code": i64::MIN,
        })
        .to_string()
    }

    #[test]
    fn persistable_empty_tail_worst_case_stays_under_twelve_kib() {
        let id = "\u{1}".repeat(256);
        let code = "\u{1}".repeat(128);
        let message = escaped_worst_case_message();
        let without_marker =
            persistable_record_with_tail(&id, Some(&id), &code, &code, &message, "", false);
        let with_marker =
            persistable_record_with_tail(&id, Some(&id), &code, &code, &message, "", true);
        assert!(
            without_marker.len() <= 12 * 1024,
            "worst empty tail: {}",
            without_marker.len()
        );
        assert!(
            with_marker.len() <= 12 * 1024,
            "worst empty tail with marker: {}",
            with_marker.len()
        );
        assert!(with_marker.len() <= PERSISTABLE_RECORD_BYTES);
        let value: serde_json::Value = serde_json::from_str(&with_marker).unwrap();
        assert_eq!(value["tail_truncated"], true);
        assert_eq!(value["bytes"].as_u64(), Some(u64::MAX));
        assert_eq!(value["remote_code"].as_i64(), Some(i64::MIN));
        assert_eq!(value["operation"].as_str().unwrap().len(), 192);
        assert_eq!(value["message"].as_str().unwrap().chars().count(), 512);
    }

    #[test]
    fn persistable_record_keeps_latest_stderr_and_is_parseable_single_line_json() {
        let large = "\0".repeat(30000);
        let record = persistable_failure_record(
            &large,
            Some(&large),
            &large,
            &large,
            &large,
            &(large.clone() + "END"),
        );
        assert!(record.len() <= PERSISTABLE_RECORD_BYTES);
        assert!(!record.contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert!(value["stderr_tail"].as_str().unwrap().ends_with("END"));
        assert_eq!(value["tail_truncated"], true);
        assert_eq!(value["agent_id"].as_str().unwrap().len(), 256);
    }

    #[test]
    fn persistable_record_marks_initial_twelve_kib_truncation() {
        let tail = "x".repeat(16 * 1024);
        let record = persistable_failure_record(
            "agent",
            None,
            "runtime_terminal",
            "RUNTIME_TERMINAL",
            "short message",
            &tail,
        );
        let value: serde_json::Value = serde_json::from_str(&record).unwrap();
        // The independent 12 KiB cap already drops the older prefix, so the
        // marker must be set even though no second shrink was needed.
        assert_eq!(value["stderr_tail"].as_str().unwrap().len(), 12 * 1024);
        assert_eq!(value["tail_truncated"], true);
        assert!(record.len() <= PERSISTABLE_RECORD_BYTES);
    }

    #[test]
    fn persistable_record_finds_the_maximal_feasible_tail_suffix() {
        // U+0001 escapes to six bytes, so this construction has a fixed part
        // of exactly 3,752 bytes and can retain (16,384 - 3,752) / 6 = 2,105
        // tail bytes. A halving search would have stopped at 1,536.
        let agent = "1000020700";
        let session = "\u{1}".repeat(88);
        let message = "\u{1}".repeat(512);
        let fixed = persistable_record_with_tail(
            agent,
            Some(&session),
            "runtime_terminal",
            "RUNTIME_TERMINAL",
            &message,
            "",
            true,
        );
        assert_eq!(fixed.len(), 3752, "{fixed}");
        let record = persistable_failure_record(
            agent,
            Some(&session),
            "runtime_terminal",
            "RUNTIME_TERMINAL",
            &message,
            &"\u{1}".repeat(3000),
        );
        let value: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert_eq!(value["stderr_tail"].as_str().unwrap().len(), 2105);
        assert_eq!(record.len(), 16382);
        assert_eq!(value["tail_truncated"], true);
        let one_more = persistable_record_with_tail(
            agent,
            Some(&session),
            "runtime_terminal",
            "RUNTIME_TERMINAL",
            &message,
            &"\u{1}".repeat(2106),
            true,
        );
        assert!(one_more.len() > PERSISTABLE_RECORD_BYTES);
    }

    #[test]
    fn bounded_error_respects_utf8_byte_limit() {
        let value = bounded_error(&"界".repeat(5000));
        assert!(value.len() <= 4096);
        assert!(value.ends_with('…'));
        assert!(std::str::from_utf8(value.as_bytes()).is_ok());
    }

    #[test]
    fn failure_log_queue_is_bounded_and_reports_dropped_writes_after_recovery() {
        struct BlockingWriter {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            output: mpsc::Sender<String>,
            blocked: bool,
        }
        impl Write for BlockingWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if !self.blocked {
                    self.blocked = true;
                    self.entered.send(()).unwrap();
                    self.release.recv().unwrap();
                }
                self.output
                    .send(String::from_utf8_lossy(buf).into_owned())
                    .unwrap();
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (output_tx, output_rx) = mpsc::channel();
        let logger = DiagnosticLogger::start(BlockingWriter {
            entered: entered_tx,
            release: release_rx,
            output: output_tx,
            blocked: false,
        })
        .unwrap();
        logger.submit("first\n".into());
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        for _ in 0..DIAGNOSTIC_QUEUE_CAPACITY + 100 {
            logger.submit("queued\n".into());
        }
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(logger.dropped.load(Ordering::Relaxed), 100);
        release_tx.send(()).unwrap();
        assert_eq!(
            output_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "first\n"
        );
        assert_eq!(
            output_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "[external-subagentd] diagnostic_writes_dropped=100\n"
        );
        for _ in 0..DIAGNOSTIC_QUEUE_CAPACITY {
            output_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
    }

    #[test]
    fn failure_log_write_errors_are_ignored_and_reported_on_next_success() {
        struct FailingOnce {
            output: mpsc::Sender<String>,
            failed: bool,
        }
        impl Write for FailingOnce {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if !self.failed {
                    self.failed = true;
                    return Err(io::Error::other("synthetic log failure"));
                }
                self.output
                    .send(String::from_utf8_lossy(buf).into_owned())
                    .unwrap();
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (output_tx, output_rx) = mpsc::channel();
        let logger = DiagnosticLogger::start(FailingOnce {
            output: output_tx,
            failed: false,
        })
        .unwrap();
        logger.submit("fails\n".into());
        logger.submit("succeeds\n".into());
        assert_eq!(
            output_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "[external-subagentd] diagnostic_writes_dropped=1\n"
        );
        assert_eq!(
            output_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "succeeds\n"
        );
    }

    #[test]
    fn failure_log_rotates_with_finite_files_and_preserves_open_stderr_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon-error.log");
        let mut stderr = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .unwrap();
        let mut writer = RotatingDiagnosticWriter { path: path.clone() };
        let record = vec![b'x'; DIAGNOSTIC_RECORD_BYTES];
        for _ in 0..100 {
            writer.write_all(&record).unwrap();
        }
        stderr.write_all(b"launchd-stderr-still-current\n").unwrap();
        assert!(fs::read_to_string(&path)
            .unwrap()
            .ends_with("launchd-stderr-still-current\n"));
        let entries = fs::read_dir(directory.path())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 3);
        for entry in entries {
            assert!(entry.metadata().unwrap().len() <= DIAGNOSTIC_FILE_BYTES);
        }
        // Pre-existing oversized logs are trimmed to the same retention cap.
        stderr.set_len(DIAGNOSTIC_FILE_BYTES * 3).unwrap();
        writer.write_all(b"recovered\n").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 10);
        assert_eq!(
            fs::metadata(directory.path().join("daemon-error.log.1"))
                .unwrap()
                .len(),
            DIAGNOSTIC_FILE_BYTES
        );
    }

    #[test]
    fn latest_failure_replaces_previous_failure() {
        let mut failures = HashMap::new();
        update_latest_failure(&mut failures, "agent", "first".into());
        update_latest_failure(&mut failures, "agent", "latest".into());
        assert_eq!(failures.get("agent").map(String::as_str), Some("latest"));
    }

    #[test]
    fn registered_cleanup_captures_runtime_session_and_tail() {
        // Review C-02(1) asked for an assertion on the unstarted sibling at
        // `cleanup_registered_runtime_with_grace` (the `else` branch). That
        // branch is only taken when the post-stop row phase is outside
        // {RUNNING, CANCELLING, TERMINAL}, but `request_runtime_stop` runs
        // immediately before it and unconditionally moves any non-terminal row
        // to CANCELLING (crates/external-store/src/lifecycle.rs:169-187), so
        // the sibling is defensive-only and no production path reaches it.
        // This test pins the reachable registered-cleanup handoff instead: the
        // failure record must carry the still-owned runtime's session and tail.
        struct CleanupRuntime;
        impl ManagedRuntime for CleanupRuntime {
            fn identity(&self) -> Option<ProcessIdentity> {
                None
            }
            fn stop(&self, _: Duration) -> RuntimeTerminal {
                RuntimeTerminal::Completed(StopOutcome::AlreadyExited(ChildExit::Exited(Some(0))))
            }
            fn wait_terminal(&self, _: Duration) -> Option<RuntimeTerminal> {
                Some(self.stop(Duration::ZERO))
            }
            fn bootstrap_session(
                &self,
                _: &TaskRecord,
                _: Duration,
            ) -> Result<SessionReady, RuntimeCommandError> {
                Ok(SessionReady {
                    session_id: "cleanup-session".into(),
                    initial_turn_id: None,
                    configured_model: None,
                })
            }
            fn diagnostic_session_id(&self) -> Option<String> {
                Some("cleanup-session".into())
            }
            fn diagnostic_tail(&self) -> String {
                "cleanup-tail".into()
            }
        }
        struct CleanupFactory;
        impl RuntimeFactory for CleanupFactory {
            fn spawn(
                &self,
                _: &TaskRecord,
                _: Arc<dyn LifecycleSink>,
            ) -> io::Result<Arc<dyn ManagedRuntime>> {
                Ok(Arc::new(CleanupRuntime))
            }
        }

        let (workspace, mut scheduler, agent_id) = diagnostic_scheduler("unused");
        Arc::get_mut(&mut scheduler.inner).unwrap().factory = Arc::new(CleanupFactory);
        // Abort the RUNNING transition so registration succeeds but the store
        // mark fails: cleanup then hands off while still owning the runtime.
        {
            let connection =
                rusqlite::Connection::open(workspace.path().join("state.sqlite")).unwrap();
            connection
                .execute_batch(
                    "CREATE TRIGGER reject_running BEFORE UPDATE ON tasks
                     WHEN NEW.phase='RUNNING' AND OLD.phase='PREPARING'
                     BEGIN SELECT RAISE(ABORT, 'injected running failure'); END;",
                )
                .unwrap();
        }
        assert!(scheduler.start_ready().is_err());
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        let raw = task.failure_message.expect("persisted failure detail");
        let persisted: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(persisted["error_code"], "STORE_START_FAILED");
        // The still-owned runtime supplies the honest session/tail evidence.
        assert_eq!(persisted["session_id"], "cleanup-session");
        assert_eq!(persisted["stderr_tail"], "cleanup-tail");
        assert_eq!(persisted["stage"], "runtime_terminal");
    }

    #[test]
    fn legacy_top_level_model_fails_route_decoding_before_the_model_gate() {
        let (_workspace, mut scheduler, agent_id) = diagnostic_scheduler("unused");
        // A legacy row that carried the requested model at the prepared JSON
        // top level is rejected by the strict PreparedGeneralTask decode
        // before the session-level model gate can run, so it lands as a
        // preparation failure rather than MODEL_MISMATCH.
        {
            let store = scheduler.store();
            let task = store.get_task(&agent_id).unwrap().unwrap();
            let mut prepared: serde_json::Value =
                serde_json::from_str(&task.prepared_launch_json).unwrap();
            prepared["model"] = serde_json::json!("zai/requested-model");
            let connection = rusqlite::Connection::open(store.database_path()).unwrap();
            connection
                .execute(
                    "UPDATE tasks SET prepared_launch_json=?1 WHERE agent_id=?2",
                    rusqlite::params![prepared.to_string(), agent_id],
                )
                .unwrap();
        }
        let error = scheduler.start_ready().unwrap_err();
        assert!(matches!(error, SchedulerError::InvalidConfig(_)));
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        assert_eq!(task.outcome, Some(TaskOutcome::ResultInvalid));
        let raw = task.failure_message.expect("persisted failure detail");
        let persisted: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(persisted["error_code"], "PREPARED_LAUNCH_INVALID");
    }
}

#[cfg(test)]
mod bare_prompt_admission_tests {
    use super::*;

    fn workspace(prefix: &str) -> tempfile::TempDir {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/live-agent/workspace");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(base)
            .unwrap()
    }

    fn open_scheduler(directory: &tempfile::TempDir) -> Scheduler {
        let factory = Arc::new(CommandRuntimeFactory::new(
            |_: &TaskRecord| -> io::Result<Command> {
                panic!("admission must never spawn a provider")
            },
        ));
        Scheduler::new(
            "prompt-owner",
            Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap()),
            factory,
            SchedulerConfig::default(),
        )
        .unwrap()
    }

    #[test]
    fn admitted_prompt_is_the_first_turn_byte_for_byte() {
        for prompt in [
            "第一行\n\n徐→π\n trailing ",
            "  leading and trailing whitespace  ",
            "single line",
        ] {
            let directory = workspace("bare-prompt-");
            let scheduler = open_scheduler(&directory);
            let task = scheduler
                .enqueue_general(&GeneralTaskManifest {
                    schema: "zcode-general-task/v1".into(),
                    agent_id: String::new(),
                    repository: directory.path().canonicalize().unwrap(),
                    permission_mode: external_core::PermissionMode::Build,
                    prompt: prompt.into(),
                    write_manifest: vec![],
                })
                .unwrap();
            assert_eq!(task.initial_prompt.as_bytes(), prompt.as_bytes());

            let reopened = Store::open(directory.path().join("state.sqlite")).unwrap();
            let stored = reopened.get_task(&task.agent_id).unwrap().unwrap();
            assert_eq!(stored.initial_prompt.as_bytes(), prompt.as_bytes());
            let prepared: external_core::PreparedGeneralTask =
                serde_json::from_str(&stored.prepared_launch_json).unwrap();
            assert!(
                !prepared.workspace.scratch_root.join("prompt.txt").exists(),
                "admission must not persist prompt.txt"
            );
        }
    }
}

#[cfg(test)]
mod legacy_row_recovery_tests {
    use super::*;
    use external_store::TaskResult;

    fn workspace(prefix: &str) -> tempfile::TempDir {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/live-agent/workspace");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(base)
            .unwrap()
    }

    fn open_scheduler(directory: &tempfile::TempDir) -> Scheduler {
        let factory = Arc::new(CommandRuntimeFactory::new(
            |_: &TaskRecord| -> io::Result<Command> {
                panic!("startup recovery must never spawn a provider")
            },
        ));
        Scheduler::new(
            "recovery-owner",
            Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap()),
            factory,
            SchedulerConfig::default(),
        )
        .unwrap()
    }

    /// A pre-v14 prepared payload: it still names the removed prompt path and
    /// digest fields, so `deny_unknown_fields` refuses to decode it.
    fn legacy_prepared_json(repository: &Path, scratch_root: &Path) -> String {
        serde_json::json!({
            "schema": external_core::GENERAL_TASK_SCHEMA,
            "agent_id": "10000001",
            "repository": repository,
            "workspace": {"path": repository, "scratch_root": scratch_root},
            "permission_mode": "plan",
            "prompt_path": scratch_root.join("prompt.txt"),
            "prompt_sha256": "legacy-prompt",
            "write_manifest": [],
            "manifest_sha256": "legacy-manifest",
            "prepared_sha256": "legacy-prepared",
        })
        .to_string()
    }

    fn enqueue_legacy(scheduler: &Scheduler, directory: &tempfile::TempDir) -> (String, PathBuf) {
        let repository = directory.path().canonicalize().unwrap();
        let scratch_root = repository.join("scratch");
        fs::create_dir_all(&scratch_root).unwrap();
        scheduler
            .store()
            .enqueue_task_authoritative(&NewTask {
                agent_id: "10000001".into(),
                repository: repository.to_string_lossy().into_owned(),
                workspace_path: repository.to_string_lossy().into_owned(),
                runtime_hash: None,
                prepared_launch_json: legacy_prepared_json(&repository, &scratch_root),
                initial_prompt: "legacy prompt".into(),
            })
            .unwrap();
        (repository.to_string_lossy().into_owned(), scratch_root)
    }

    fn fresh_manifest(directory: &tempfile::TempDir) -> GeneralTaskManifest {
        GeneralTaskManifest {
            schema: external_core::GENERAL_TASK_SCHEMA.into(),
            agent_id: String::new(),
            repository: directory.path().canonicalize().unwrap(),
            permission_mode: external_core::PermissionMode::Build,
            prompt: "fresh work after upgrade".into(),
            write_manifest: vec![],
        }
    }

    #[test]
    fn legacy_active_row_without_intent_converges_to_result_invalid_and_stays_usable() {
        let directory = workspace("legacy-active-");
        let scheduler = open_scheduler(&directory);
        let (repository, _scratch) = enqueue_legacy(&scheduler, &directory);
        scheduler
            .store()
            .claim_next("prior-owner", 10, 1)
            .unwrap()
            .unwrap();
        drop(scheduler);

        let reopened = open_scheduler(&directory);
        let recovered = reopened.reconcile_startup().unwrap();
        assert_eq!(
            recovered,
            vec![("10000001".to_string(), TaskOutcome::ResultInvalid)]
        );
        let settled = reopened.store().get_task("10000001").unwrap().unwrap();
        assert_eq!(settled.phase, TaskPhase::Terminal);
        assert_eq!(settled.outcome, Some(TaskOutcome::ResultInvalid));
        let raw = settled
            .failure_message
            .expect("legacy recovery failure detail");
        let persisted: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(persisted["stage"], "recovery");
        assert_eq!(persisted["error_code"], "PREPARED_LAUNCH_INVALID");
        assert!(!raw.contains('\n'));

        // New admissions still work once the legacy row has a terminal state.
        let fresh = reopened
            .enqueue_general(&fresh_manifest(&directory))
            .unwrap();
        assert_eq!(fresh.phase, TaskPhase::Queued);
        assert_eq!(fresh.repository, Path::new(&repository).to_string_lossy());
    }

    #[test]
    fn legacy_terminal_unreaped_row_is_reaped_without_rewriting_result() {
        let directory = workspace("legacy-terminal-");
        let scheduler = open_scheduler(&directory);
        enqueue_legacy(&scheduler, &directory);
        scheduler
            .store()
            .claim_next("prior-owner", 10, 1)
            .unwrap()
            .unwrap();
        let original = TaskResult {
            outcome: TaskOutcome::RuntimeLost,
            final_text: "old immutable result".into(),
            partial: true,
        };
        scheduler
            .store()
            .store_task_result("10000001", &original)
            .unwrap();
        let before = scheduler.store().get_task("10000001").unwrap().unwrap();
        assert_eq!(before.phase, TaskPhase::Terminal);
        assert!(before.reaped_at.is_none());
        drop(scheduler);

        let reopened = open_scheduler(&directory);
        let recovered = reopened.reconcile_startup().unwrap();
        assert_eq!(
            recovered,
            vec![("10000001".to_string(), TaskOutcome::RuntimeLost)]
        );
        let after = reopened.store().get_task("10000001").unwrap().unwrap();
        assert_eq!(after.outcome, before.outcome);
        assert!(after.reaped_at.is_some());
        assert_eq!(
            reopened
                .store()
                .task_result("10000001")
                .unwrap()
                .unwrap()
                .result,
            original
        );
    }

    #[test]
    fn legacy_never_claimed_cancellation_converges_to_cancelled() {
        let directory = workspace("legacy-never-claimed-");
        let scheduler = open_scheduler(&directory);
        enqueue_legacy(&scheduler, &directory);
        scheduler.store().request_stop("10000001").unwrap();
        let before = scheduler.store().get_task("10000001").unwrap().unwrap();
        assert_eq!(before.phase, TaskPhase::Cancelling);
        assert_eq!(before.owner_epoch, 0);
        assert!(before.session_id.is_none());
        assert!(before.stop_requested);
        assert!(before.runtime_agent_id.is_none());
        assert!(before.process_identity.is_none());
        drop(scheduler);

        let reopened = open_scheduler(&directory);
        let recovered = reopened.reconcile_startup().unwrap();
        assert_eq!(
            recovered,
            vec![("10000001".to_string(), TaskOutcome::Cancelled)]
        );
        let after = reopened.store().get_task("10000001").unwrap().unwrap();
        assert_eq!(after.phase, TaskPhase::Terminal);
        assert_eq!(after.outcome, Some(TaskOutcome::Cancelled));
    }
}

#[cfg(test)]
mod transport_failure_tests {
    use super::*;

    const FRAME_LIMIT: usize = external_runtime::MAX_NDJSON_LINE_BYTES;
    const TRANSPORT_REASON: &str = "RUNTIME_TRANSPORT_FRAME_LIMIT";

    /// The transport fixtures each push a >16 MiB frame through a real child
    /// pipe; serialize them so the parallel suite is not starved by their
    /// byte-wise reads (mirrors the codex/DSH scripted-child guards).
    static TRANSPORT_CHILD_LOCK: Mutex<()> = Mutex::new(());

    fn transport_child_guard() -> std::sync::MutexGuard<'static, ()> {
        TRANSPORT_CHILD_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn transport_workspace(prefix: &str) -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(root)
            .unwrap()
    }

    /// A scheduler whose child command is selected from the task's prompt, so
    /// one fixture can exercise both the failing transport task and a queued
    /// follow-up task with a normal completion.
    fn transport_scheduler<F>(directory: &Path, script_for: F) -> Scheduler
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        let store = Arc::new(Store::open(directory.join("state.sqlite")).unwrap());
        let directory = directory.to_owned();
        let factory = CommandRuntimeFactory::new(move |task: &TaskRecord| {
            let mut command = Command::new("sh");
            command
                .args(["-c", &script_for(&task.initial_prompt)])
                .current_dir(&directory);
            Ok(command)
        });
        Scheduler::new(
            "transport-test",
            store,
            Arc::new(factory),
            SchedulerConfig {
                bootstrap_timeout: Duration::from_secs(30),
                control_timeout: Duration::from_secs(30),
                stop_grace: Duration::from_millis(250),
                ..SchedulerConfig::default()
            },
        )
        .unwrap()
    }

    fn enqueue(scheduler: &Scheduler, directory: &Path, prompt: &str) -> String {
        scheduler
            .enqueue_general(&GeneralTaskManifest {
                schema: "zcode-general-task/v1".into(),
                agent_id: String::new(),
                repository: directory.canonicalize().unwrap(),
                permission_mode: external_core::PermissionMode::Plan,
                prompt: prompt.into(),
                write_manifest: Vec::new(),
            })
            .unwrap()
            .agent_id
    }

    fn await_result(scheduler: &Scheduler, agent_id: &str) -> external_store::StoredTaskResult {
        await_result_within(scheduler, agent_id, Duration::from_secs(30))
    }

    fn await_result_within(
        scheduler: &Scheduler,
        agent_id: &str,
        within: Duration,
    ) -> external_store::StoredTaskResult {
        let deadline = Instant::now() + within;
        loop {
            if let Some(result) = scheduler.store().task_result(agent_id).unwrap() {
                return result;
            }
            assert!(Instant::now() < deadline, "no terminal result in time");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn await_running_identity(
        scheduler: &Scheduler,
        agent_id: &str,
    ) -> (TaskRecord, external_store::StoredProcessIdentity) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let task = scheduler.store().get_task(agent_id).unwrap().unwrap();
            if let Some(identity) = task.process_identity.clone() {
                if task.phase == TaskPhase::Running {
                    return (task, identity);
                }
            }
            assert!(Instant::now() < deadline, "task never reached RUNNING");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn oversized_events(directory: &Path, agent_id: &str) -> i64 {
        let connection = rusqlite::Connection::open(directory.join("state.sqlite")).unwrap();
        connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE agent_id=?1 AND event_type='driver.oversized_line'",
                [agent_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// The zcode bootstrap prefix used by the S01 transport fixtures: three
    /// request/response pairs plus a turn.started boundary, leaving the task
    /// RUNNING with its monitor attached.
    const ZCODE_RUNNING_PREFIX: &str = r#"
read request
printf '%s\n' '{"id":1,"result":{"session":{"sessionId":"transport-session"}}}'
read request
printf '%s\n' '{"id":2,"result":{}}'
read request
printf '%s\n' '{"id":3,"result":{}}' '{"method":"session/event","params":{"type":"turn.started"}}'
"#;

    fn oversized_output() -> String {
        format!("head -c {} /dev/zero\n", FRAME_LIMIT + 1)
    }

    /// A zcode fixture that completes one turn without any fault.
    fn normal_completion_script() -> String {
        format!(
            "{ZCODE_RUNNING_PREFIX}\
printf '%s\n' '{{\"method\":\"session/event\",\"params\":{{\"type\":\"model.streaming\",\"payload\":{{\"kind\":\"text_delta\",\"delta\":\"queued task answer\",\"assistantMessageId\":\"m1\"}}}}}}' \
'{{\"method\":\"session/event\",\"params\":{{\"type\":\"message.finished\",\"payload\":{{\"assistantMessageId\":\"m1\"}}}}}}' \
'{{\"method\":\"session/event\",\"params\":{{\"type\":\"turn.completed\"}}}}'\nsleep 1\n"
        )
    }

    /// Directly queue a second task in the same workspace (the submission API
    /// refuses to enqueue while any non-terminal task owns the workspace), so
    /// the fault closure's queue advancement can be observed without a manual
    /// `start_ready`.
    fn insert_queued_task(directory: &Path, source: &str, new_agent: &str, prompt: &str) {
        let connection = rusqlite::Connection::open(directory.join("state.sqlite")).unwrap();
        let created: i64 = connection
            .query_row(
                "SELECT created_at FROM tasks WHERE agent_id=?1",
                [source],
                |row| row.get(0),
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO tasks(agent_id,repository,phase,workspace_path,runtime_hash,
                     prepared_launch_json,initial_prompt,created_at)
                 SELECT ?1,repository,'QUEUED',workspace_path,runtime_hash,
                     prepared_launch_json,?2,?3 FROM tasks WHERE agent_id=?4",
                rusqlite::params![new_agent, prompt, created + 1, source],
            )
            .unwrap();
    }

    fn assert_transport_diagnostic(record: &serde_json::Value, expect_cleanup: &str) {
        assert_eq!(record["stage"], "transport");
        assert_eq!(record["error_code"], TRANSPORT_REASON);
        assert_eq!(record["bytes"], (FRAME_LIMIT + 1) as u64);
        assert_eq!(record["cap"], FRAME_LIMIT as u64);
        assert!(record["last_event_seq"].as_u64().unwrap() >= 1);
        assert!(
            record["cleanup_result"]
                .as_str()
                .unwrap()
                .contains(expect_cleanup),
            "{record}"
        );
    }

    /// Pauses the monitor inside the transport-failure closure so a test can
    /// pin the exact interleaving before the real cleanup runs.
    struct TransportBarrier {
        ready: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
    }

    impl TransportBarrier {
        fn install(scheduler: &Scheduler) -> Self {
            let ready = Arc::new(AtomicBool::new(false));
            let release = Arc::new(AtomicBool::new(false));
            let ready_hook = Arc::clone(&ready);
            let release_hook = Arc::clone(&release);
            scheduler.set_before_transport_cleanup_hook(Arc::new(move || {
                ready_hook.store(true, Ordering::Release);
                let deadline = Instant::now() + Duration::from_secs(30);
                while !release_hook.load(Ordering::Acquire) {
                    assert!(
                        Instant::now() < deadline,
                        "transport cleanup barrier never released"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
            }));
            Self { ready, release }
        }

        fn wait_ready(&self, within: Duration) {
            let deadline = Instant::now() + within;
            while !self.ready.load(Ordering::Acquire) {
                assert!(
                    Instant::now() < deadline,
                    "monitor never latched the transport failure"
                );
                thread::sleep(Duration::from_millis(1));
            }
        }

        fn release(&self) {
            self.release.store(true, Ordering::Release);
        }
    }

    #[test]
    fn zcode_oversized_frame_fails_explicitly_reaps_and_releases_the_queue() {
        let _guard = transport_child_guard();
        let directory = transport_workspace("s01-transport-zcode-");
        let failing = format!("{ZCODE_RUNNING_PREFIX}{}\nsleep 30\n", oversized_output());
        let normal = normal_completion_script();
        let scheduler = transport_scheduler(directory.path(), move |prompt| {
            if prompt == "oversized transport" {
                failing.clone()
            } else {
                normal.clone()
            }
        });
        let barrier = TransportBarrier::install(&scheduler);
        let failing_id = enqueue(&scheduler, directory.path(), "oversized transport");
        assert_eq!(scheduler.start_ready().unwrap(), vec![failing_id.clone()]);
        let (_, identity) = await_running_identity(&scheduler, &failing_id);
        let pgid = identity.process_group_id;
        barrier.wait_ready(Duration::from_secs(30));
        // Accident condition: the provider is still alive after emitting the
        // oversized frame, so no child-exit terminal could have closed it.
        assert!(
            external_runtime::observe_process(identity.pid as u32).is_ok(),
            "provider must survive the oversized output"
        );
        barrier.release();

        // Bounded wait, far below the transport test's own cleanup budget.
        let result = await_result_within(&scheduler, &failing_id, Duration::from_secs(10));
        assert_eq!(result.result.outcome, TaskOutcome::Failed);
        assert!(result.result.partial);
        assert_eq!(
            scheduler
                .store()
                .terminal_reason_code(&failing_id)
                .unwrap()
                .as_deref(),
            Some(TRANSPORT_REASON)
        );
        // The oversized event was persisted exactly once.
        assert_eq!(oversized_events(directory.path(), &failing_id), 1);

        let task = scheduler.store().get_task(&failing_id).unwrap().unwrap();
        assert_eq!(task.phase, TaskPhase::Terminal);
        assert_eq!(task.outcome, Some(TaskOutcome::Failed));
        assert!(task.reaped_at.is_some(), "cleanup must prove the reap");
        assert!(observe_process_group(pgid).unwrap().is_empty());
        assert_eq!(scheduler.active_count(), 0);

        // The diagnostic closure carries the transport stage, bounded
        // evidence, and the real cleanup result.
        let record: serde_json::Value =
            serde_json::from_str(&scheduler.last_error(&failing_id).unwrap()).unwrap();
        assert_transport_diagnostic(&record, "Stopped");

        // The result is immutable and no second terminal was produced.
        assert_eq!(
            scheduler.store().task_result(&failing_id).unwrap().unwrap(),
            result
        );

        // Capacity released: the workspace slot is free again and the
        // scheduler admits a follow-up task through the same closure path.
        let queued_id = enqueue(&scheduler, directory.path(), "queued normal task");
        assert_eq!(scheduler.start_ready().unwrap(), vec![queued_id.clone()]);
        let queued = await_result(&scheduler, &queued_id);
        assert_eq!(queued.result.outcome, TaskOutcome::Completed);
        assert_eq!(queued.result.final_text, "queued task answer");
    }

    #[test]
    fn zcode_oversized_frame_with_a_pending_input_still_fails_the_task() {
        let _guard = transport_child_guard();
        let directory = transport_workspace("s01-transport-zcode-pending-");
        let script = format!(
            "{ZCODE_RUNNING_PREFIX}\
while [ ! -f release-request ]; do sleep 0.01; done\n\
printf '%s\n' '{{\"id\":\"srv-input-1\",\"method\":\"interaction/requestUserInput\",\"params\":{{\"question\":\"which scope?\"}}}}'\n\
while [ ! -f release-oversize ]; do sleep 0.01; done\n\
{}\nsleep 30\n",
            oversized_output()
        );
        let scheduler = transport_scheduler(directory.path(), move |_| script.clone());
        let agent_id = enqueue(&scheduler, directory.path(), "oversized with pending");
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        // The interaction request requires a RUNNING row, so gate it on the
        // observed phase instead of racing process bootstrap.
        let (_, _) = await_running_identity(&scheduler, &agent_id);
        fs::write(directory.path().join("release-request"), b"").unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if !scheduler
                .store()
                .pending_requests(&agent_id)
                .unwrap()
                .is_empty()
            {
                break;
            }
            assert!(Instant::now() < deadline, "pending input never arrived");
            thread::sleep(Duration::from_millis(10));
        }
        fs::write(directory.path().join("release-oversize"), b"").unwrap();
        let result = await_result_within(&scheduler, &agent_id, Duration::from_secs(10));
        assert_eq!(result.result.outcome, TaskOutcome::Failed);
        assert_eq!(
            scheduler
                .store()
                .terminal_reason_code(&agent_id)
                .unwrap()
                .as_deref(),
            Some(TRANSPORT_REASON)
        );
        // The closure terminalizes once: the pending interaction is settled
        // away instead of leaving the task waiting for input.
        assert!(scheduler
            .store()
            .pending_requests(&agent_id)
            .unwrap()
            .is_empty());
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        assert_eq!(task.outcome, Some(TaskOutcome::Failed));
        // The transport closure's bounded evidence reaches tasks.failure_message.
        let raw = task.failure_message.expect("persisted transport detail");
        let persisted: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(persisted["stage"], "transport");
        assert_eq!(persisted["error_code"], TRANSPORT_REASON);
        assert!(persisted["bytes"].as_u64().is_some());
        assert!(persisted["cap"].as_u64().is_some());
        assert!(persisted["last_event_seq"].as_u64().is_some());
        assert!(!raw.contains('\n'));
    }

    #[test]
    fn zcode_committed_cancellation_outranks_the_transport_failure() {
        let _guard = transport_child_guard();
        let directory = transport_workspace("s01-transport-zcode-cancel-");
        let script = format!(
            "{ZCODE_RUNNING_PREFIX}\
while [ ! -f release-oversize ]; do sleep 0.01; done\n\
{}\nsleep 30\n",
            oversized_output()
        );
        let scheduler = transport_scheduler(directory.path(), move |_| script.clone());
        let agent_id = enqueue(&scheduler, directory.path(), "oversized after cancel");
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        let (_, identity) = await_running_identity(&scheduler, &agent_id);
        let pgid = identity.process_group_id;
        scheduler.store().request_stop(&agent_id).unwrap();
        fs::write(directory.path().join("release-oversize"), b"").unwrap();
        let result = await_result_within(&scheduler, &agent_id, Duration::from_secs(10));
        assert_eq!(result.result.outcome, TaskOutcome::Cancelled);
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        assert_eq!(task.outcome, Some(TaskOutcome::Cancelled));
        // The cancellation precedence must clear the transport failure detail.
        assert_eq!(task.failure_message, None);
        assert!(observe_process_group(pgid).unwrap().is_empty());
    }

    #[test]
    fn latched_transport_failure_cleans_up_after_a_late_orphaned_terminal() {
        let _guard = transport_child_guard();
        let directory = transport_workspace("s01-transport-b2-01-");
        // The leader publishes an oversized frame, forks a descendant that
        // keeps the process group alive, then exits so the pump publishes an
        // Orphaned terminal before the monitor reaches its cleanup.
        let script = format!(
            "{ZCODE_RUNNING_PREFIX}{}\n\
sleep 30 &\n\
sleep 0.3\n\
exit 0\n",
            oversized_output()
        );
        let scheduler = transport_scheduler(directory.path(), move |prompt| {
            if prompt == "oversized then orphan" {
                script.clone()
            } else {
                normal_completion_script()
            }
        });
        let barrier = TransportBarrier::install(&scheduler);
        let agent_id = enqueue(&scheduler, directory.path(), "oversized then orphan");
        let queued_id = "10009999".to_string();
        // R4: an already-queued same-workspace task must be admitted by the
        // fault closure's own queue advancement.
        insert_queued_task(
            directory.path(),
            &agent_id,
            &queued_id,
            "queued normal task",
        );
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        barrier.wait_ready(Duration::from_secs(30));

        let (_, runtime, _, _, _) = scheduler
            .active_session(&agent_id)
            .expect("active instance retained for the barrier");
        let pgid = runtime.identity().expect("owned process group").pgid;
        // Barrier: the monitor is paused before cleanup, the late child-exit
        // boundary has already published its terminal, and a cleanable
        // descendant still holds the group.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(terminal) = runtime.wait_terminal(Duration::ZERO) {
                assert!(
                    matches!(terminal, RuntimeTerminal::Orphaned(_)),
                    "expected an orphaned late terminal, got {terminal:?}"
                );
                break;
            }
            assert!(Instant::now() < deadline, "late terminal never published");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !observe_process_group(pgid).unwrap().is_empty(),
            "the barrier requires a live cleanable descendant"
        );

        barrier.release();
        let result = await_result_within(&scheduler, &agent_id, Duration::from_secs(15));
        assert_eq!(result.result.outcome, TaskOutcome::Failed);
        assert_eq!(
            scheduler
                .store()
                .terminal_reason_code(&agent_id)
                .unwrap()
                .as_deref(),
            Some(TRANSPORT_REASON)
        );
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        assert!(task.reaped_at.is_some());
        assert!(observe_process_group(pgid).unwrap().is_empty());
        assert_eq!(oversized_events(directory.path(), &agent_id), 1);
        let record: serde_json::Value =
            serde_json::from_str(&scheduler.last_error(&agent_id).unwrap()).unwrap();
        assert_transport_diagnostic(&record, "Stopped");
        assert_eq!(
            scheduler.store().task_result(&agent_id).unwrap().unwrap(),
            result
        );

        // The closure released the slot and advanced the queue by itself: the
        // pre-queued task starts and completes without a manual start_ready.
        let queued = await_result(&scheduler, &queued_id);
        assert_eq!(queued.result.outcome, TaskOutcome::Completed);
        assert_eq!(queued.result.final_text, "queued task answer");
        assert_eq!(scheduler.active_count(), 0);
    }

    struct InjectingFactory {
        injected: RuntimeTerminal,
        fault_terminal: Option<RuntimeTerminal>,
        calls: Arc<AtomicU64>,
        store: Arc<Store>,
        session_id: String,
    }

    impl RuntimeFactory for InjectingFactory {
        fn spawn(
            &self,
            task: &TaskRecord,
            sink: Arc<dyn LifecycleSink>,
        ) -> io::Result<Arc<dyn ManagedRuntime>> {
            Ok(Arc::new(InjectingRuntime {
                agent_id: task.agent_id.clone(),
                publisher: Arc::new(Publisher::new(sink)),
                tracker: Arc::new(TurnTracker::new()),
                injected: self.injected.clone(),
                fault_terminal: self.fault_terminal.clone(),
                calls: Arc::clone(&self.calls),
                store: Arc::clone(&self.store),
                session_id: self.session_id.clone(),
            }) as Arc<dyn ManagedRuntime>)
        }
    }

    struct InjectingRuntime {
        agent_id: String,
        publisher: Arc<Publisher>,
        tracker: Arc<TurnTracker>,
        injected: RuntimeTerminal,
        fault_terminal: Option<RuntimeTerminal>,
        calls: Arc<AtomicU64>,
        store: Arc<Store>,
        session_id: String,
    }

    impl ManagedRuntime for InjectingRuntime {
        fn identity(&self) -> Option<ProcessIdentity> {
            None
        }
        fn stop(&self, _: Duration) -> RuntimeTerminal {
            self.injected.clone()
        }
        fn wait_terminal(&self, timeout: Duration) -> Option<RuntimeTerminal> {
            self.publisher.wait_terminal(timeout)
        }
        fn cleanup_for_forced_failure(&self, _: Duration) -> RuntimeTerminal {
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.injected.clone()
        }
        fn diagnostic_session_id(&self) -> Option<String> {
            Some(self.session_id.clone())
        }
        fn bootstrap_session_with_mcp(
            &self,
            _task: &TaskRecord,
            _mcp_servers: &[external_contract::StdioMcpServer],
            _timeout: Duration,
        ) -> Result<SessionReady, RuntimeCommandError> {
            let publisher = Arc::clone(&self.publisher);
            let store = Arc::clone(&self.store);
            let agent_id = self.agent_id.clone();
            let fault_terminal = self.fault_terminal.clone();
            thread::spawn(move || {
                // Wait until the durable row is RUNNING so the latch lands on
                // a monitor-managed instance.
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    let running = store
                        .get_task(&agent_id)
                        .ok()
                        .flatten()
                        .is_some_and(|task| task.phase == TaskPhase::Running);
                    if running {
                        break;
                    }
                    if Instant::now() >= deadline {
                        return;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                publisher.emit_driver(
                    Inbound::OversizedLine {
                        bytes: FRAME_LIMIT + 1,
                    },
                    None,
                );
                // Simulate a pump that publishes a normal terminal boundary
                // right after the oversized frame was latched.
                if let Some(terminal) = fault_terminal {
                    publisher.publish_terminal(terminal);
                }
            });
            Ok(SessionReady {
                session_id: self.session_id.clone(),
                initial_turn_id: None,
                configured_model: None,
            })
        }
        fn turn_snapshot(&self) -> TurnSnapshot {
            self.tracker.snapshot()
        }
    }

    #[test]
    fn injected_cleanup_failure_keeps_unreaped_evidence() {
        let directory = transport_workspace("s01-transport-cleanup-failure-");
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let calls = Arc::new(AtomicU64::new(0));
        let scheduler = Scheduler::new(
            "transport-injection",
            Arc::clone(&store),
            Arc::new(InjectingFactory {
                injected: RuntimeTerminal::FailedRuntimeLost(RuntimeLoss::StopFailed(
                    "injected cleanup failure".into(),
                )),
                fault_terminal: None,
                calls: Arc::clone(&calls),
                store: Arc::clone(&store),
                session_id: "injected-cleanup-session".into(),
            }),
            SchedulerConfig {
                bootstrap_timeout: Duration::from_secs(30),
                control_timeout: Duration::from_secs(30),
                ..SchedulerConfig::default()
            },
        )
        .unwrap();
        let agent_id = enqueue(&scheduler, directory.path(), "injected cleanup failure");
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        let result = await_result_within(&scheduler, &agent_id, Duration::from_secs(10));
        assert_eq!(result.result.outcome, TaskOutcome::Failed);
        assert_eq!(
            scheduler
                .store()
                .terminal_reason_code(&agent_id)
                .unwrap()
                .as_deref(),
            Some(TRANSPORT_REASON)
        );
        assert_eq!(calls.load(Ordering::Acquire), 1);
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        assert!(task.reaped_at.is_none(), "a failed cleanup is not a reap");
        let record: serde_json::Value =
            serde_json::from_str(&scheduler.last_error(&agent_id).unwrap()).unwrap();
        assert_transport_diagnostic(&record, "injected cleanup failure");
        assert_eq!(record["session_id"], "injected-cleanup-session");
        assert_eq!(
            scheduler.store().task_result(&agent_id).unwrap().unwrap(),
            result
        );
        assert_eq!(scheduler.active_count(), 0);
    }

    /// S01 AC4: a normal terminal published by the pump after the oversized
    /// frame (here a Completed boundary) must not turn the task COMPLETED.
    #[test]
    fn published_completed_terminal_does_not_override_the_latched_failure() {
        let directory = transport_workspace("s01-transport-late-completed-");
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let calls = Arc::new(AtomicU64::new(0));
        let scheduler = Scheduler::new(
            "transport-late-completed",
            Arc::clone(&store),
            Arc::new(InjectingFactory {
                injected: RuntimeTerminal::Stopped(StopOutcome::Terminated(ChildExit::Signaled(
                    15,
                ))),
                fault_terminal: Some(RuntimeTerminal::Completed(StopOutcome::AlreadyExited(
                    ChildExit::Exited(Some(0)),
                ))),
                calls: Arc::clone(&calls),
                store: Arc::clone(&store),
                session_id: "late-completed-session".into(),
            }),
            SchedulerConfig {
                bootstrap_timeout: Duration::from_secs(30),
                control_timeout: Duration::from_secs(30),
                ..SchedulerConfig::default()
            },
        )
        .unwrap();
        let agent_id = enqueue(&scheduler, directory.path(), "completed after fault");
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        let result = await_result_within(&scheduler, &agent_id, Duration::from_secs(10));
        assert_eq!(result.result.outcome, TaskOutcome::Failed);
        assert_eq!(
            scheduler
                .store()
                .terminal_reason_code(&agent_id)
                .unwrap()
                .as_deref(),
            Some(TRANSPORT_REASON)
        );
        assert_eq!(calls.load(Ordering::Acquire), 1);
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        assert!(task.reaped_at.is_some(), "the real cleanup proves the reap");
        let record: serde_json::Value =
            serde_json::from_str(&scheduler.last_error(&agent_id).unwrap()).unwrap();
        assert_transport_diagnostic(&record, "Stopped");
        assert_eq!(
            scheduler.store().task_result(&agent_id).unwrap().unwrap(),
            result
        );
        assert_eq!(scheduler.active_count(), 0);
    }
}

#[cfg(test)]
mod turn_adjudication_tests {
    use super::*;

    /// The former no-activity window, retained only as the magnitude AC3
    /// advances the injected test clock by; no production code reads it.
    const NO_ACTIVITY_WINDOW: Duration = Duration::from_secs(30 * 60);

    /// Manual monotonic clock injected through the test-only `Scheduler`
    /// clock seam, proving elapsed time alone never terminalizes a task.
    struct ManualClock {
        base: Instant,
        offset: Mutex<Duration>,
    }

    impl ManualClock {
        fn new() -> Self {
            Self {
                base: Instant::now(),
                offset: Mutex::new(Duration::ZERO),
            }
        }
        fn now(&self) -> Instant {
            self.base + *self.offset.lock().unwrap()
        }
        fn advance(&self, delta: Duration) {
            *self.offset.lock().unwrap() += delta;
        }
    }

    struct HarnessRuntime {
        publisher: Arc<Publisher>,
        tracker: Arc<TurnTracker>,
        respond_fails: bool,
        cleanup_calls: Arc<AtomicU64>,
    }

    impl HarnessRuntime {
        fn emit(&self, event: Inbound) {
            self.publisher.emit_driver(event, None);
        }

        fn emit_activity(&self) {
            self.emit(Inbound::Message(WireMessage::UnknownEvent {
                method: "session/event".into(),
                raw: serde_json::json!({
                    "method": "session/event",
                    "params": {"type": "model.streaming"},
                }),
            }));
        }

        fn emit_request(&self, id: &str, method: &str) {
            self.emit(Inbound::Message(WireMessage::Request(
                external_contract::RequestEnvelope::new(
                    WireId::String(id.into()),
                    method,
                    serde_json::json!({"question": "still working?"}),
                ),
            )));
        }
    }

    impl ManagedRuntime for HarnessRuntime {
        fn identity(&self) -> Option<ProcessIdentity> {
            None
        }
        fn stop(&self, _: Duration) -> RuntimeTerminal {
            RuntimeTerminal::Stopped(StopOutcome::Terminated(ChildExit::Signaled(15)))
        }
        fn cleanup_for_forced_failure(&self, _: Duration) -> RuntimeTerminal {
            self.cleanup_calls.fetch_add(1, Ordering::AcqRel);
            RuntimeTerminal::Stopped(StopOutcome::Terminated(ChildExit::Signaled(15)))
        }
        fn wait_terminal(&self, timeout: Duration) -> Option<RuntimeTerminal> {
            self.publisher.wait_terminal(timeout)
        }
        fn terminal_latch(&self) -> Option<TerminalLatch<'_>> {
            Some(self.publisher.decision_latch())
        }
        fn diagnostic_session_id(&self) -> Option<String> {
            Some("adjudication-session".into())
        }
        fn bootstrap_session_with_mcp(
            &self,
            _task: &TaskRecord,
            _mcp_servers: &[external_contract::StdioMcpServer],
            _timeout: Duration,
        ) -> Result<SessionReady, RuntimeCommandError> {
            // Echo the harness's admitted model, exactly like a real
            // adapter: the bootstrap now cross-checks admission.model against
            // the runtime's configured model.
            Ok(SessionReady {
                session_id: "adjudication-session".into(),
                initial_turn_id: None,
                configured_model: Some("fixture-model".into()),
            })
        }
        fn resume_session_with_mcp(
            &self,
            _task: &TaskRecord,
            _mcp_servers: &[external_contract::StdioMcpServer],
            _timeout: Duration,
        ) -> Result<SessionReady, RuntimeCommandError> {
            // A resumed session is model-sticky: like the zcode owner, this
            // fixture cannot re-read the configured model and returns None.
            // The scheduler must not re-validate the admitted model on resume.
            Ok(SessionReady {
                session_id: "adjudication-session".into(),
                initial_turn_id: None,
                configured_model: None,
            })
        }
        fn send_turn(
            &self,
            _session_id: &str,
            _content: &str,
            _timeout: Duration,
        ) -> Result<Option<String>, RuntimeCommandError> {
            Ok(None)
        }
        fn respond_request(
            &self,
            _correlation_id: &str,
            _decision: &str,
            _content: Option<&str>,
            _validated_denial: Option<&external_core::ValidatedPermissionDenial>,
            _deadline: Instant,
        ) -> Result<(), RuntimeCommandError> {
            if self.respond_fails {
                Err(RuntimeCommandError::InvalidSession(
                    "injected response failure".into(),
                ))
            } else {
                Ok(())
            }
        }
        fn turn_snapshot(&self) -> TurnSnapshot {
            self.tracker.snapshot()
        }
    }

    struct HarnessFactory {
        runtimes: Arc<Mutex<Vec<Arc<HarnessRuntime>>>>,
        respond_fails: bool,
        cleanup_calls: Arc<AtomicU64>,
    }

    impl RuntimeFactory for HarnessFactory {
        fn spawn(
            &self,
            _task: &TaskRecord,
            sink: Arc<dyn LifecycleSink>,
        ) -> io::Result<Arc<dyn ManagedRuntime>> {
            let runtime = Arc::new(HarnessRuntime {
                publisher: Arc::new(Publisher::new(sink)),
                tracker: Arc::new(TurnTracker::new()),
                respond_fails: self.respond_fails,
                cleanup_calls: Arc::clone(&self.cleanup_calls),
            });
            self.runtimes.lock().unwrap().push(Arc::clone(&runtime));
            Ok(runtime as Arc<dyn ManagedRuntime>)
        }
    }

    struct Harness {
        _directory: tempfile::TempDir,
        scheduler: Scheduler,
        clock: Arc<ManualClock>,
        runtimes: Arc<Mutex<Vec<Arc<HarnessRuntime>>>>,
        cleanup_calls: Arc<AtomicU64>,
        agent_id: String,
    }

    fn harness(respond_fails: bool, admission_agent: &str) -> Harness {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("turn-adjudication-")
            .tempdir_in(root)
            .unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let runtimes = Arc::new(Mutex::new(Vec::new()));
        let cleanup_calls = Arc::new(AtomicU64::new(0));
        let factory = Arc::new(HarnessFactory {
            runtimes: Arc::clone(&runtimes),
            respond_fails,
            cleanup_calls: Arc::clone(&cleanup_calls),
        });
        let mut scheduler = Scheduler::new(
            "adjudication-test",
            store,
            factory,
            SchedulerConfig {
                bootstrap_timeout: Duration::from_secs(30),
                control_timeout: Duration::from_secs(30),
                stop_grace: Duration::from_millis(250),
                ..SchedulerConfig::default()
            },
        )
        .unwrap();
        let clock = Arc::new(ManualClock::new());
        {
            let clock = Arc::clone(&clock);
            scheduler.set_clock(Arc::new(move || clock.now()));
        }
        let manifest = GeneralTaskManifest {
            schema: "zcode-general-task/v1".into(),
            agent_id: String::new(),
            repository: directory.path().canonicalize().unwrap(),
            permission_mode: external_core::PermissionMode::Plan,
            prompt: "turn fixture".into(),
            write_manifest: Vec::new(),
        };
        let submitted = if admission_agent.is_empty() {
            scheduler.enqueue_general(&manifest).unwrap()
        } else {
            scheduler
                .enqueue_general_with_admission(
                    &manifest,
                    Some(external_core::AdmissionIdentity {
                        agent: admission_agent.into(),
                        config_revision: 1,
                        adapter_version: "test".into(),
                        model: Some("fixture-model".into()),
                        model_source: "catalog".into(),
                        effort: None,
                    }),
                )
                .unwrap()
        };
        Harness {
            _directory: directory,
            scheduler,
            clock,
            runtimes,
            cleanup_calls,
            agent_id: submitted.agent_id,
        }
    }

    fn await_phase(scheduler: &Scheduler, agent_id: &str, phase: TaskPhase) -> TaskRecord {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let task = scheduler.store().get_task(agent_id).unwrap().unwrap();
            if task.phase == phase {
                return task;
            }
            assert!(Instant::now() < deadline, "task never reached {phase:?}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn await_result_within(
        scheduler: &Scheduler,
        agent_id: &str,
        within: Duration,
    ) -> external_store::StoredTaskResult {
        let deadline = Instant::now() + within;
        loop {
            if let Some(result) = scheduler.store().task_result(agent_id).unwrap() {
                return result;
            }
            assert!(Instant::now() < deadline, "no terminal result in time");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn fresh_bootstrap_still_enforces_the_requested_model_gate() {
        let harness = harness(false, "zcode");
        let store = harness.scheduler.store();
        // Rewrite the persisted admission model so the fixture's echoed
        // bootstrap model no longer matches it. A fresh bootstrap must still
        // fail closed on the divergence (only the resume path skips the gate).
        {
            let task = store.get_task(&harness.agent_id).unwrap().unwrap();
            let mut prepared: serde_json::Value =
                serde_json::from_str(&task.prepared_launch_json).unwrap();
            prepared["admission"]["model"] = serde_json::json!("other-model");
            let connection = rusqlite::Connection::open(store.database_path()).unwrap();
            connection
                .execute(
                    "UPDATE tasks SET prepared_launch_json=?1 WHERE agent_id=?2",
                    rusqlite::params![prepared.to_string(), harness.agent_id],
                )
                .unwrap();
        }
        let error = harness.scheduler.start_ready().unwrap_err();
        assert!(matches!(error, SchedulerError::RuntimeCommand { .. }));
        let task = store.get_task(&harness.agent_id).unwrap().unwrap();
        assert_eq!(task.outcome, Some(TaskOutcome::Failed));
        assert_eq!(
            store
                .terminal_reason_code(&harness.agent_id)
                .unwrap()
                .as_deref(),
            Some("MODEL_MISMATCH")
        );
    }

    #[test]
    fn resume_skips_the_requested_model_gate_for_a_sticky_session() {
        let harness = harness(false, "zcode");
        let store = harness.scheduler.store();
        // The task carries an admitted model, but its resume adapter cannot
        // re-read the persisted model (configured_model None). Resume must not
        // re-validate the sticky session, so the task still reaches RUNNING.
        let claim = store.claim_next("adjudication-test", 10, 1).unwrap().unwrap();
        assert!(store
            .mark_session_running(
                &harness.agent_id,
                claim.owner_epoch,
                "old-runtime",
                None,
                Some("sticky-session"),
                None,
            )
            .unwrap());
        store
            .store_task_result(
                &harness.agent_id,
                &external_store::TaskResult {
                    outcome: TaskOutcome::Failed,
                    final_text: "prior turn failed".into(),
                    partial: true,
                },
            )
            .unwrap();
        assert!(store
            .requeue_task_for_resume_with_message(
                &harness.agent_id,
                "resume-model-gate",
                "queue",
                "continue"
            )
            .unwrap());
        let resumed = store.get_task(&harness.agent_id).unwrap().unwrap();
        assert_eq!(resumed.session_id.as_deref(), Some("sticky-session"));
        assert_eq!(
            harness.scheduler.start_ready().unwrap(),
            vec![harness.agent_id.clone()]
        );
        await_phase(&harness.scheduler, &harness.agent_id, TaskPhase::Running);
    }

    /// AC3: a RUNNING task that emits nothing for far longer than the former
    /// no-activity window must never be terminalized by the daemon, and it
    /// keeps its capacity slot. Advancing the injected test clock proves that
    /// elapsed time alone cannot terminalize the task after a full monitor
    /// iteration has consumed the advanced clock.
    #[test]
    fn silence_never_terminalizes_and_keeps_capacity() {
        let harness = harness(false, "");
        let scheduler = &harness.scheduler;
        let agent_id = &harness.agent_id;
        let iterations = Arc::new(Mutex::new(0_u64));
        let hook_iterations = Arc::clone(&iterations);
        scheduler.set_before_terminal_wait_hook(Arc::new(move || {
            *hook_iterations.lock().unwrap() += 1;
        }));
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        await_phase(scheduler, agent_id, TaskPhase::Running);

        let before_advance = {
            let count = iterations.lock().unwrap();
            harness.clock.advance(NO_ACTIVITY_WINDOW * 3);
            *count
        };
        // Two post-advance wait entries acknowledge a full intervening loop.
        // The count lock prevents a pre-advance entry from being counted;
        // a watchdog result exits the wait so the assertion below exposes it.
        let deadline = Instant::now() + Duration::from_secs(10);
        while *iterations.lock().unwrap() < before_advance + 2 {
            if scheduler.store().task_result(agent_id).unwrap().is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "monitor did not complete a post-advance iteration"
            );
            thread::sleep(Duration::from_millis(1));
        }

        assert!(
            scheduler.store().task_result(agent_id).unwrap().is_none(),
            "silence must never terminalize a RUNNING task"
        );
        assert_eq!(
            scheduler.store().get_task(agent_id).unwrap().unwrap().phase,
            TaskPhase::Running
        );
        assert_eq!(
            scheduler.active_count(),
            1,
            "the silent task keeps its capacity slot"
        );
        assert_eq!(harness.cleanup_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn runtime_response_failure_fails_closed() {
        let harness = harness(true, "");
        let scheduler = &harness.scheduler;
        let agent_id = &harness.agent_id;
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        await_phase(scheduler, agent_id, TaskPhase::Running);
        harness.runtimes.lock().unwrap()[0].emit_request(
            "srv-fail",
            external_contract::INTERACTION_REQUEST_USER_INPUT,
        );
        await_phase(scheduler, agent_id, TaskPhase::WaitingInput);
        let requests = scheduler.store().pending_requests(agent_id).unwrap();
        assert!(
            scheduler
                .respond_request(agent_id, &requests[0].request_id, "answer", Some("scope"))
                .is_err(),
            "the injected runtime response failure must surface"
        );
        // A failed response never grants a new lease on the task: the existing
        // fail-closed control path owns the terminal, with its own code.
        let result = await_result_within(scheduler, agent_id, Duration::from_secs(10));
        assert_eq!(result.result.outcome, TaskOutcome::Failed);
        assert_eq!(
            scheduler
                .store()
                .terminal_reason_code(agent_id)
                .unwrap()
                .as_deref(),
            Some("CONTROL_RUNTIME_FAILED"),
        );
        assert_eq!(harness.cleanup_calls.load(Ordering::Acquire), 0);
    }

    /// Park the monitor just before its terminal wait and return the latch
    /// release handle, so a test can latch a fault in the window the loop-top
    /// check has already passed without publishing a terminal.
    fn latch_fault_at_the_terminal_wait(
        scheduler: &Scheduler,
    ) -> (Arc<AtomicBool>, Arc<AtomicBool>) {
        let ready = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let armed = Arc::new(AtomicBool::new(true));
        let ready_hook = Arc::clone(&ready);
        let release_hook = Arc::clone(&release);
        scheduler.set_before_terminal_wait_hook(Arc::new(move || {
            if armed.swap(false, Ordering::AcqRel) {
                ready_hook.store(true, Ordering::Release);
                let deadline = Instant::now() + Duration::from_secs(30);
                while !release_hook.load(Ordering::Acquire) {
                    assert!(Instant::now() < deadline, "terminal-wait barrier never released");
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }));
        (ready, release)
    }

    fn await_flag(flag: &AtomicBool, within: Duration, message: &str) {
        let deadline = Instant::now() + within;
        while !flag.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "{message}");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn turn_event(kind: &str) -> Inbound {
        Inbound::Message(WireMessage::Event(external_contract::EventEnvelope {
            method: "session/event".into(),
            params: serde_json::json!({ "type": kind }),
        }))
    }

    /// AC2(d): with no published runtime terminal, a completed turn whose
    /// fault was latched after the loop-top check must still terminalize as a
    /// transport failure. Without the turn-path arbitration the natural
    /// completion would win and the task would be reported COMPLETED.
    #[test]
    fn completed_turn_with_a_latched_fault_takes_the_transport_closure() {
        const TRANSPORT_REASON: &str = "RUNTIME_TRANSPORT_FRAME_LIMIT";
        let bytes = external_runtime::MAX_NDJSON_LINE_BYTES + 1;
        let cap = external_runtime::MAX_NDJSON_LINE_BYTES;
        let harness = harness(false, "");
        let scheduler = &harness.scheduler;
        let agent_id = &harness.agent_id;
        let (ready, release) = latch_fault_at_the_terminal_wait(scheduler);
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        await_phase(scheduler, agent_id, TaskPhase::Running);
        await_flag(
            &ready,
            Duration::from_secs(30),
            "monitor never reached the terminal wait",
        );

        let runtime = Arc::clone(&harness.runtimes.lock().unwrap()[0]);
        // Latch the fault and complete the turn with no published terminal.
        runtime.emit(Inbound::OversizedLine { bytes });
        runtime.tracker.observe(&turn_event("turn.started"));
        runtime.tracker.observe(&turn_event("turn.completed"));
        release.store(true, Ordering::Release);

        let result = await_result_within(scheduler, agent_id, Duration::from_secs(10));
        assert_eq!(result.result.outcome, TaskOutcome::Failed);
        assert_eq!(
            scheduler
                .store()
                .terminal_reason_code(agent_id)
                .unwrap()
                .as_deref(),
            Some(TRANSPORT_REASON),
            "the latched transport fault must outrank natural completion"
        );
        let task = scheduler.store().get_task(agent_id).unwrap().unwrap();
        assert_eq!(task.outcome, Some(TaskOutcome::Failed));
        assert!(task.reaped_at.is_some());
        assert_eq!(harness.cleanup_calls.load(Ordering::Acquire), 1);
        let record: serde_json::Value =
            serde_json::from_str(&scheduler.last_error(agent_id).unwrap()).unwrap();
        assert_eq!(record["stage"], "transport");
        assert_eq!(record["error_code"], TRANSPORT_REASON);
        assert_eq!(record["bytes"], bytes as u64);
        assert_eq!(record["cap"], cap as u64);
        assert_eq!(
            scheduler.store().task_result(agent_id).unwrap().unwrap(),
            result
        );
    }

    /// AC2(d) transient variant: a decision read failure at the arbitration
    /// point retries and still converges on the transport closure instead of
    /// falling through into a natural completion.
    #[test]
    fn latched_fault_survives_transient_reads_on_the_turn_path() {
        const TRANSPORT_REASON: &str = "RUNTIME_TRANSPORT_FRAME_LIMIT";
        let bytes = external_runtime::MAX_NDJSON_LINE_BYTES + 1;
        let harness = harness(false, "");
        let scheduler = &harness.scheduler;
        let agent_id = &harness.agent_id;
        let reads = Arc::new(AtomicU64::new(0));
        {
            let reads = Arc::clone(&reads);
            scheduler
                .set_decision_read_fault(Arc::new(move || reads.fetch_add(1, Ordering::AcqRel) < 2));
        }
        let (ready, release) = latch_fault_at_the_terminal_wait(scheduler);
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        await_phase(scheduler, agent_id, TaskPhase::Running);
        await_flag(
            &ready,
            Duration::from_secs(30),
            "monitor never reached the terminal wait",
        );

        let runtime = Arc::clone(&harness.runtimes.lock().unwrap()[0]);
        runtime.emit(Inbound::OversizedLine { bytes });
        runtime.tracker.observe(&turn_event("turn.started"));
        runtime.tracker.observe(&turn_event("turn.completed"));
        release.store(true, Ordering::Release);

        let result = await_result_within(scheduler, agent_id, Duration::from_secs(10));
        assert_eq!(result.result.outcome, TaskOutcome::Failed);
        assert_eq!(
            scheduler
                .store()
                .terminal_reason_code(agent_id)
                .unwrap()
                .as_deref(),
            Some(TRANSPORT_REASON),
            "the confirmed latch must survive transient read failures"
        );
        assert!(
            reads.load(Ordering::Acquire) >= 2,
            "the read fault must have fired"
        );
    }

    /// R7: a transport fault confirmed at the terminal re-check must keep its
    /// priority through transient decision read failures instead of falling
    /// into the normal terminal closure.
    #[test]
    fn latched_transport_fault_survives_transient_read_failures_at_the_terminal_recheck() {
        const TRANSPORT_REASON: &str = "RUNTIME_TRANSPORT_FRAME_LIMIT";
        let bytes = external_runtime::MAX_NDJSON_LINE_BYTES + 1;
        let harness = harness(false, "");
        let scheduler = &harness.scheduler;
        let agent_id = &harness.agent_id;

        // Fail the first two decision reads, then recover.
        let reads = Arc::new(AtomicU64::new(0));
        {
            let reads = Arc::clone(&reads);
            scheduler
                .set_decision_read_fault(Arc::new(move || reads.fetch_add(1, Ordering::AcqRel) < 2));
        }
        // Block the monitor once just before its terminal wait, so the loop
        // top latch check has already missed the fault latched afterwards.
        let ready = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        {
            let armed = Arc::new(AtomicBool::new(true));
            let ready = Arc::clone(&ready);
            let release = Arc::clone(&release);
            scheduler.set_before_terminal_wait_hook(Arc::new(move || {
                if armed.swap(false, Ordering::AcqRel) {
                    ready.store(true, Ordering::Release);
                    let deadline = Instant::now() + Duration::from_secs(30);
                    while !release.load(Ordering::Acquire) {
                        assert!(
                            Instant::now() < deadline,
                            "terminal-wait barrier never released"
                        );
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            }));
        }

        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        await_phase(scheduler, agent_id, TaskPhase::Running);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ready.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "monitor never reached the terminal wait"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let runtime = Arc::clone(&harness.runtimes.lock().unwrap()[0]);
        // Latch the fault and publish a normal terminal in the window the
        // loop-top check already passed.
        runtime.emit(Inbound::OversizedLine { bytes });
        runtime
            .publisher
            .publish_terminal(RuntimeTerminal::Completed(StopOutcome::AlreadyExited(
                ChildExit::Exited(Some(0)),
            )));
        release.store(true, Ordering::Release);

        let result = await_result_within(scheduler, agent_id, Duration::from_secs(10));
        assert_eq!(result.result.outcome, TaskOutcome::Failed);
        assert_eq!(
            scheduler
                .store()
                .terminal_reason_code(agent_id)
                .unwrap()
                .as_deref(),
            Some(TRANSPORT_REASON),
            "the confirmed transport latch must survive the read failures"
        );
        assert_eq!(harness.cleanup_calls.load(Ordering::Acquire), 1);
        let task = scheduler.store().get_task(agent_id).unwrap().unwrap();
        assert!(task.reaped_at.is_some());
        let record: serde_json::Value =
            serde_json::from_str(&scheduler.last_error(agent_id).unwrap()).unwrap();
        assert_eq!(record["stage"], "transport");
        assert_eq!(record["error_code"], TRANSPORT_REASON);
        assert!(record["last_event_seq"].as_u64().unwrap() >= 1);
        assert_eq!(
            scheduler.store().task_result(agent_id).unwrap().unwrap(),
            result
        );
    }

    #[test]
    fn early_permission_request_waits_for_the_running_commit_instead_of_failing() {
        // A provider may emit a respondable request between session-ready and
        // the RUNNING commit; the durable row is still PREPARING then, so the
        // store's phase gate conflicts. The sink must wait out the commit
        // instead of latching a sink error that kills the task (the eager
        // scripted dsh providers hit this window on slow CI runners). On the
        // unfixed code this test is red: the emit latches the conflict as the
        // sink error and no pending request ever exists.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("sink-race-")
            .tempdir_in(root)
            .unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let runtimes = Arc::new(Mutex::new(Vec::new()));
        let factory = Arc::new(HarnessFactory {
            runtimes,
            respond_fails: false,
            cleanup_calls: Arc::new(AtomicU64::new(0)),
        });
        let scheduler = Scheduler::new(
            "sink-race-test",
            Arc::clone(&store),
            factory,
            SchedulerConfig {
                bootstrap_timeout: Duration::from_secs(30),
                control_timeout: Duration::from_secs(30),
                stop_grace: Duration::from_millis(250),
                ..SchedulerConfig::default()
            },
        )
        .unwrap();
        let manifest = GeneralTaskManifest {
            schema: "zcode-general-task/v1".into(),
            agent_id: String::new(),
            repository: directory.path().canonicalize().unwrap(),
            permission_mode: external_core::PermissionMode::Plan,
            prompt: "sink race fixture".into(),
            write_manifest: Vec::new(),
        };
        let agent_id = scheduler.enqueue_general(&manifest).unwrap().agent_id;
        let claim = store.claim_next("sink-race", 999, 1).unwrap().unwrap();
        assert_eq!(claim.task.phase, TaskPhase::Preparing);

        let lifecycle = Arc::new(RuntimeLifecycle::new(claim.owner_epoch));
        let sink = Arc::new(crate::lifecycle_sink::StoreLifecycleSink::new(
            Arc::clone(&store),
            agent_id.clone(),
            "sink-race-runtime".into(),
            claim.owner_epoch,
            Arc::clone(&lifecycle),
            Arc::new(crate::activity_tracker::PassiveActivityTracker::new(false)),
        ));
        let record = crate::LifecycleRecord {
            sequence: 7,
            event: RuntimeEvent::Driver(Inbound::Message(WireMessage::Request(
                external_contract::RequestEnvelope::new(
                    WireId::String("srv-early".into()),
                    external_contract::INTERACTION_REQUEST_PERMISSION,
                    serde_json::json!({"options": []}),
                ),
            ))),
        };
        let emitter = {
            let sink = Arc::clone(&sink);
            thread::spawn(move || sink.emit(record))
        };
        // The emit is still inside the retry window: PREPARING conflicts and
        // no pending request exists yet.
        thread::sleep(Duration::from_millis(200));
        assert!(
            store.pending_requests(&agent_id).unwrap().is_empty(),
            "the request must not be publishable before the RUNNING commit"
        );
        store
            .mark_session_running(
                &agent_id,
                claim.owner_epoch,
                "sink-race-runtime",
                None,
                Some("sink-race-session"),
                None,
            )
            .unwrap();
        emitter.join().unwrap();
        let requests = store.pending_requests(&agent_id).unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].request_type, "permission");
        assert_eq!(requests[0].state, PendingRequestState::Pending);
        assert!(
            sink.error().is_none(),
            "an early request must never latch a sink error"
        );
    }
}

mod wait_tail_inheritance_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A runtime that bootstraps, then reports a clean terminal so the monitor
    /// thread exits promptly; the wait-tail tests only need a registered
    /// activity tracker.
    struct InheritRuntime {
        session_id: String,
    }

    impl ManagedRuntime for InheritRuntime {
        fn identity(&self) -> Option<ProcessIdentity> {
            None
        }
        fn stop(&self, _: Duration) -> RuntimeTerminal {
            RuntimeTerminal::Completed(StopOutcome::AlreadyExited(ChildExit::Exited(Some(0))))
        }
        fn wait_terminal(&self, _: Duration) -> Option<RuntimeTerminal> {
            Some(self.stop(Duration::ZERO))
        }
        fn bootstrap_session(
            &self,
            _: &TaskRecord,
            _: Duration,
        ) -> Result<SessionReady, RuntimeCommandError> {
            Ok(SessionReady {
                session_id: self.session_id.clone(),
                initial_turn_id: None,
                configured_model: None,
            })
        }
    }

    /// Optionally pauses inside `spawn`, i.e. after `start_claim` built the new
    /// tracker (lifecycle.rs:133) but before the activities map replacement
    /// critical section, so a test can advance the outgoing tracker's cursor in
    /// that exact window. `inject_before_replacement` emits a text delta through
    /// the runtime sink while the new tracker is still outside the map.
    struct InheritFactory {
        ready: Option<Arc<AtomicBool>>,
        release: Option<Arc<AtomicBool>>,
        inject_before_replacement: Option<String>,
    }

    impl RuntimeFactory for InheritFactory {
        fn spawn(
            &self,
            task: &TaskRecord,
            sink: Arc<dyn LifecycleSink>,
        ) -> io::Result<Arc<dyn ManagedRuntime>> {
            if let Some(delta) = &self.inject_before_replacement {
                sink.emit(LifecycleRecord {
                    sequence: 1,
                    event: RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(
                        external_contract::EventEnvelope {
                            method: "session/event".into(),
                            params: serde_json::json!({
                                "type": "model.streaming",
                                "eventId": format!("pre-swap-{delta}"),
                                "payload": {
                                    "kind": "text_delta",
                                    "delta": delta,
                                    "assistantMessageId": "m1"
                                }
                            }),
                        },
                    ))),
                });
            }
            if let (Some(ready), Some(release)) = (&self.ready, &self.release) {
                ready.store(true, Ordering::Release);
                let deadline = Instant::now() + Duration::from_secs(30);
                while !release.load(Ordering::Acquire) {
                    assert!(
                        Instant::now() < deadline,
                        "tracker inheritance barrier never released"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
            }
            Ok(Arc::new(InheritRuntime {
                session_id: format!("inherit-{}", task.agent_id),
            }))
        }
    }

    struct InheritHarness {
        _directory: tempfile::TempDir,
        scheduler: Scheduler,
        agent_id: String,
    }

    fn inherit_harness(
        paused: bool,
        inject_before_replacement: Option<&str>,
    ) -> (InheritHarness, Arc<AtomicBool>, Arc<AtomicBool>) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace");
        fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("s01-wait-inherit-")
            .tempdir_in(root)
            .unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let ready = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let factory = Arc::new(InheritFactory {
            ready: paused.then(|| Arc::clone(&ready)),
            release: paused.then(|| Arc::clone(&release)),
            inject_before_replacement: inject_before_replacement.map(str::to_owned),
        });
        let scheduler = Scheduler::new("inherit-test", store, factory, SchedulerConfig::default())
            .unwrap();
        let submitted = scheduler
            .enqueue_general(&GeneralTaskManifest {
                schema: "zcode-general-task/v1".into(),
                agent_id: String::new(),
                repository: directory.path().canonicalize().unwrap(),
                permission_mode: external_core::PermissionMode::Plan,
                prompt: "wait tail inheritance".into(),
                write_manifest: Vec::new(),
            })
            .unwrap();
        (
            InheritHarness {
                _directory: directory,
                scheduler,
                agent_id: submitted.agent_id,
            },
            ready,
            release,
        )
    }

    fn inject_text(tracker: &PassiveActivityTracker, delta: &str) {
        tracker.observe(&RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(
            external_contract::EventEnvelope {
                method: "session/event".into(),
                params: serde_json::json!({
                    "type": "model.streaming",
                    "eventId": format!("delta-{delta}"),
                    "payload": {
                        "kind": "text_delta",
                        "delta": delta,
                        "assistantMessageId": "m1"
                    }
                }),
            },
        ))));
    }

    fn tracker_for(scheduler: &Scheduler, agent_id: &str) -> Arc<PassiveActivityTracker> {
        scheduler
            .inner
            .state
            .lock()
            .unwrap()
            .activities
            .get(agent_id)
            .cloned()
            .expect("activity tracker in the map")
    }

    /// Install an outgoing tracker whose window is "AB" with "A" delivered.
    fn outgoing_tracker(scheduler: &Scheduler, agent_id: &str) -> Arc<PassiveActivityTracker> {
        let old = Arc::new(PassiveActivityTracker::new(true));
        old.set_wait_tail_fixture("A");
        assert_eq!(old.take_wait_tail().text, "A");
        inject_text(&old, "B");
        scheduler
            .inner
            .state
            .lock()
            .unwrap()
            .activities
            .insert(agent_id.to_owned(), Arc::clone(&old));
        old
    }

    #[test]
    fn wait_tail_survives_tracker_replacement_with_undelivered_text() {
        // AC9(a): a follow-up/resume build replaces the tracker; bytes that were
        // never delivered must still be deliverable, and the cursor must not
        // reset to zero.
        let (harness, _, _) = inherit_harness(false, None);
        let scheduler = &harness.scheduler;
        let id = &harness.agent_id;
        let old = outgoing_tracker(scheduler, id);

        assert_eq!(scheduler.start_ready().unwrap(), vec![id.clone()]);
        let new = tracker_for(scheduler, id);
        assert!(!Arc::ptr_eq(&old, &new), "the claim replaced the tracker");

        inject_text(&new, "C");
        assert_eq!(
            scheduler.take_wait_tail(id).expect("new tracker").text,
            "BC",
            "undelivered old text (B) plus the new delta (C)"
        );
    }

    #[test]
    fn wait_tail_inheritance_reads_the_latest_cursor_inside_the_replacement_lock() {
        // AC9(b): deterministic interleaving. The new tracker exists but the map
        // replacement is paused; a wait consumes "B" from the outgoing tracker
        // in that window. The inheritance read must happen under the same state
        // lock as the map swap, so it sees the advanced cursor and only "C" is
        // delivered. A lock-outside pre-copy (lifecycle.rs:133) would capture
        // delivered=A and re-deliver "BC".
        let (harness, ready, release) = inherit_harness(true, None);
        let scheduler = harness.scheduler.clone();
        let id = harness.agent_id.clone();
        let old = outgoing_tracker(&harness.scheduler, &id);

        let spawner = thread::spawn(move || scheduler.start_ready());
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ready.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "spawn never paused");
            thread::sleep(Duration::from_millis(1));
        }
        // The map still holds the outgoing tracker; consume its pending "B".
        assert_eq!(
            harness
                .scheduler
                .take_wait_tail(&id)
                .expect("old tracker")
                .text,
            "B"
        );
        release.store(true, Ordering::Release);
        assert_eq!(spawner.join().unwrap().unwrap(), vec![id.clone()]);

        let new = tracker_for(&harness.scheduler, &id);
        assert!(!Arc::ptr_eq(&old, &new));
        inject_text(&new, "C");
        assert_eq!(
            harness
                .scheduler
                .take_wait_tail(&id)
                .expect("new tracker")
                .text,
            "C",
            "already consumed bytes must not be re-delivered"
        );
    }

    #[test]
    fn wait_tail_inheritance_merges_text_appended_before_the_map_swap() {
        // MAJOR repair: the runtime sink is wired to the new tracker before the
        // map replacement (lifecycle.rs:133-147), so the incoming tracker can
        // already hold text (e.g. a resume's initial output). The inheritance
        // must MERGE that text after the old window instead of overwriting it.
        // Old window "AB" with "A" delivered; the new tracker collects "N"
        // during spawn, before the swap. A correct merge yields "BN" (old
        // undelivered "B" plus "N"); an overwriting implementation loses "N".
        let (harness, _, _) = inherit_harness(false, Some("N"));
        let scheduler = &harness.scheduler;
        let id = &harness.agent_id;
        let old = outgoing_tracker(scheduler, id);

        assert_eq!(scheduler.start_ready().unwrap(), vec![id.clone()]);
        let new = tracker_for(scheduler, id);
        assert!(!Arc::ptr_eq(&old, &new));

        let merged = scheduler.take_wait_tail(id).expect("new tracker");
        assert_eq!(
            merged.text, "BN",
            "old undelivered text plus the pre-swap append"
        );
        assert!(!merged.truncated);
        // The merged stream is not re-delivered after the take.
        assert_eq!(scheduler.take_wait_tail(id).expect("new tracker").text, "");
    }
}

#[cfg(test)]
mod spawn_established_return_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};
    use external_core::PermissionMode;
    use external_store::{TaskPhase, TaskOutcome};

    struct TestRuntime {
        session_id: String,
        model: Option<String>,
        stopped: Arc<AtomicBool>,
        identity: Option<external_runtime::ProcessIdentity>,
        bootstrap_hook: Option<Arc<dyn Fn() + Send + Sync>>,
        fail_bootstrap: Option<String>,
    }

    impl ManagedRuntime for TestRuntime {
        fn identity(&self) -> Option<external_runtime::ProcessIdentity> {
            self.identity.clone()
        }
        fn stop(&self, _: Duration) -> RuntimeTerminal {
            self.stopped.store(true, Ordering::Release);
            RuntimeTerminal::Stopped(external_runtime::StopOutcome::AlreadyExited(
                external_runtime::ChildExit::Exited(Some(0)),
            ))
        }
        fn wait_terminal(&self, _: Duration) -> Option<RuntimeTerminal> {
            std::thread::sleep(Duration::from_millis(1));
            self.stopped
                .load(Ordering::Acquire)
                .then(|| self.stop(Duration::ZERO))
        }
        fn bootstrap_session(
            &self,
            _: &TaskRecord,
            _: Duration,
        ) -> Result<SessionReady, RuntimeCommandError> {
            if let Some(hook) = &self.bootstrap_hook {
                hook();
            }
            if let Some(err) = &self.fail_bootstrap {
                return Err(RuntimeCommandError::Transport(err.clone()));
            }
            Ok(SessionReady {
                session_id: self.session_id.clone(),
                initial_turn_id: Some("turn-1".into()),
                configured_model: self.model.clone(),
            })
        }
        fn resume_session_with_mcp(
            &self,
            record: &TaskRecord,
            _mcp_servers: &[crate::StdioMcpServer],
            timeout: Duration,
        ) -> Result<SessionReady, RuntimeCommandError> {
            self.bootstrap_session(record, timeout)
        }
        fn turn_snapshot(&self) -> TurnSnapshot {
            TurnSnapshot {
                generation: 1,
                active: true,
                boundary: None,
            }
        }
        fn inject_turn(
            &self,
            _: &str,
            _: &str,
            _: Duration,
        ) -> Result<Option<String>, RuntimeCommandError> {
            Ok(None)
        }
    }

    struct TestFactory {
        spawn_fn: Arc<dyn Fn(&TaskRecord) -> std::io::Result<Arc<dyn ManagedRuntime>> + Send + Sync>,
    }

    impl RuntimeFactory for TestFactory {
        fn spawn(
            &self,
            record: &TaskRecord,
            _: Arc<dyn LifecycleSink>,
        ) -> std::io::Result<Arc<dyn ManagedRuntime>> {
            (self.spawn_fn)(record)
        }
    }

    fn test_harness(
        spawn_fn: impl Fn(&TaskRecord) -> std::io::Result<Arc<dyn ManagedRuntime>> + Send + Sync + 'static,
    ) -> (tempfile::TempDir, Scheduler, Arc<Store>) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
        let factory = Arc::new(TestFactory {
            spawn_fn: Arc::new(spawn_fn),
        });
        let scheduler = Scheduler::new(
            "test-scheduler",
            store.clone(),
            factory,
            SchedulerConfig::default(),
        )
        .unwrap();
        (directory, scheduler, store)
    }

    fn default_manifest(repo: &std::path::Path) -> GeneralTaskManifest {
        GeneralTaskManifest {
            schema: "zcode-general-task/v1".into(),
            agent_id: String::new(),
            repository: repo.to_path_buf(),
            permission_mode: PermissionMode::Plan,
            prompt: "test prompt".into(),
            write_manifest: vec![],
        }
    }

    fn default_admission() -> Option<external_core::AdmissionIdentity> {
        Some(external_core::AdmissionIdentity {
            agent: "zcode".into(),
            config_revision: 1,
            adapter_version: "test".into(),
            model: Some("test-model".into()),
            model_source: "native".into(),
            effort: None,
        })
    }

    #[test]
    fn ac1_submit_and_start_general_success_returns_running_with_session_id() {
        let (dir, scheduler, _) = test_harness(|_| {
            Ok(Arc::new(TestRuntime {
                session_id: "session-ac1".into(),
                model: Some("test-model".into()),
                stopped: Arc::new(AtomicBool::new(false)),
                identity: None,
                bootstrap_hook: None,
                fail_bootstrap: None,
            }))
        });
        let manifest = default_manifest(dir.path());
        let admission = default_admission();
        let record = scheduler
            .submit_and_start_general(&manifest, admission, &|| false)
            .expect("spawn must succeed");
        assert_eq!(record.phase, TaskPhase::Running);
        assert_eq!(record.session_id.as_deref(), Some("session-ac1"));
    }

    #[test]
    fn ac1b_fast_completed_before_first_poll_returns_completed_with_session_id() {
        let poll_gate = Arc::new(AtomicBool::new(false));
        let poll_gate_clone = Arc::clone(&poll_gate);
        let task_started = Arc::new(AtomicBool::new(false));
        let task_started_clone = Arc::clone(&task_started);

        let (dir, scheduler, store) = test_harness(move |_| {
            task_started_clone.store(true, Ordering::Release);
            Ok(Arc::new(TestRuntime {
                session_id: "session-ac1b-completed".into(),
                model: Some("test-model".into()),
                stopped: Arc::new(AtomicBool::new(false)),
                identity: None,
                bootstrap_hook: None,
                fail_bootstrap: None,
            }))
        });

        let poll_count = Arc::new(AtomicU64::new(0));
        let poll_count_clone = Arc::clone(&poll_count);
        scheduler.set_spawn_poll_hook(Some(Arc::new(move |_| {
            if poll_count_clone.fetch_add(1, Ordering::SeqCst) == 0 {
                while !poll_gate_clone.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        })));

        let manifest = default_manifest(dir.path());
        let admission = default_admission();
        let scheduler_clone = scheduler.clone();
        let join_handle = std::thread::spawn(move || {
            scheduler_clone.submit_and_start_general(&manifest, admission, &|| false)
        });

        while !task_started.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(2));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut agent_id = String::new();
        while Instant::now() < deadline {
            if let Some(t) = store.get_task("10000000").unwrap() {
                if t.phase == TaskPhase::Running {
                    agent_id = t.agent_id.clone();
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!agent_id.is_empty(), "task did not become Running");

        store
            .transition_terminal(
                &agent_id,
                1,
                &external_store::TerminalUpdate {
                    outcome: TaskOutcome::Completed,
                    failure_code: None,
                    failure_message: None,
                },
            )
            .unwrap();

        poll_gate.store(true, Ordering::Release);

        let record = join_handle.join().unwrap().expect("must succeed with completed");
        assert_eq!(record.phase, TaskPhase::Terminal);
        assert_eq!(record.outcome, Some(TaskOutcome::Completed));
        assert_eq!(record.session_id.as_deref(), Some("session-ac1b-completed"));
    }

    #[test]
    fn ac1b_fast_failed_after_established_returns_failed_with_session_id() {
        let poll_gate = Arc::new(AtomicBool::new(false));
        let poll_gate_clone = Arc::clone(&poll_gate);
        let task_started = Arc::new(AtomicBool::new(false));
        let task_started_clone = Arc::clone(&task_started);

        let (dir, scheduler, store) = test_harness(move |_| {
            task_started_clone.store(true, Ordering::Release);
            Ok(Arc::new(TestRuntime {
                session_id: "session-ac1b-failed".into(),
                model: Some("test-model".into()),
                stopped: Arc::new(AtomicBool::new(false)),
                identity: None,
                bootstrap_hook: None,
                fail_bootstrap: None,
            }))
        });

        let poll_count = Arc::new(AtomicU64::new(0));
        let poll_count_clone = Arc::clone(&poll_count);
        scheduler.set_spawn_poll_hook(Some(Arc::new(move |_| {
            if poll_count_clone.fetch_add(1, Ordering::SeqCst) == 0 {
                while !poll_gate_clone.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        })));

        let manifest = default_manifest(dir.path());
        let admission = default_admission();
        let scheduler_clone = scheduler.clone();
        let join_handle = std::thread::spawn(move || {
            scheduler_clone.submit_and_start_general(&manifest, admission, &|| false)
        });

        while !task_started.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(2));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut agent_id = String::new();
        while Instant::now() < deadline {
            if let Some(t) = store.get_task("10000000").unwrap() {
                if t.phase == TaskPhase::Running {
                    agent_id = t.agent_id.clone();
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!agent_id.is_empty(), "task did not become Running");

        store
            .transition_terminal(
                &agent_id,
                1,
                &external_store::TerminalUpdate {
                    outcome: TaskOutcome::Failed,
                    failure_code: Some("RUNTIME_EXITED_EARLY".into()),
                    failure_message: Some("fast failed result".into()),
                },
            )
            .unwrap();

        poll_gate.store(true, Ordering::Release);

        let record = join_handle.join().unwrap().expect("must return record");
        assert_eq!(record.phase, TaskPhase::Terminal);
        assert_eq!(record.outcome, Some(TaskOutcome::Failed));
        assert_eq!(record.session_id.as_deref(), Some("session-ac1b-failed"));
    }

    #[test]
    fn ac2_spawn_or_bootstrap_failure_returns_start_failed_with_reason() {
        let (dir, scheduler, store) = test_harness(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "executable not found",
            ))
        });
        let manifest = default_manifest(dir.path());
        let admission = default_admission();
        let err = scheduler
            .submit_and_start_general(&manifest, admission.clone(), &|| false)
            .unwrap_err();
        match err {
            SchedulerError::StartFailed { reason, agent_id, .. } => {
                assert_eq!(reason, "RUNTIME_SPAWN_FAILED");
                let record = store.get_task(&agent_id).unwrap().unwrap();
                assert_eq!(record.phase, TaskPhase::Terminal);
                assert_eq!(record.outcome, Some(TaskOutcome::Failed));
                assert!(record.session_id.is_none());
            }
            other => panic!("expected StartFailed, got {other:?}"),
        }

        let (dir2, scheduler2, store2) = test_harness(|_| {
            Ok(Arc::new(TestRuntime {
                session_id: "never".into(),
                model: Some("test-model".into()),
                stopped: Arc::new(AtomicBool::new(false)),
                identity: None,
                bootstrap_hook: None,
                fail_bootstrap: Some("connection refused".into()),
            }))
        });
        let manifest2 = default_manifest(dir2.path());
        let err2 = scheduler2
            .submit_and_start_general(&manifest2, admission, &|| false)
            .unwrap_err();
        match err2 {
            SchedulerError::StartFailed { reason, agent_id, .. } => {
                assert_eq!(reason, "SESSION_START_FAILED");
                let record = store2.get_task(&agent_id).unwrap().unwrap();
                assert_eq!(record.phase, TaskPhase::Terminal);
                assert_eq!(record.outcome, Some(TaskOutcome::Failed));
                assert!(record.session_id.is_none());
            }
            other => panic!("expected StartFailed, got {other:?}"),
        }
    }

    /// Fill a pipe's kernel buffer to capacity using non-blocking writes and
    /// return only after the filling write has come back. A following blocking
    /// `write_all` on a reader that never drains stdin (`sleep`) therefore
    /// cannot complete on *any* schedule until the reader dies. Publishing
    /// `write_entered` after this returns makes "entered && !done at cancel"
    /// prove a write the lock-free kill is required to unblock, rather than a
    /// marker set before the write call was reached.
    fn prefill_pipe_buffer(writer: &std::process::ChildStdin) -> usize {
        use std::os::unix::io::AsRawFd;
        let fd = writer.as_raw_fd();
        let original_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(original_flags >= 0, "F_GETFL must succeed");
        assert!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags | libc::O_NONBLOCK) } >= 0,
            "F_SETFL O_NONBLOCK must succeed"
        );
        let chunk = [b'a'; 4096];
        let mut total = 0usize;
        loop {
            let written =
                unsafe { libc::write(fd, chunk.as_ptr() as *const libc::c_void, chunk.len()) };
            if written > 0 {
                total += written as usize;
                continue;
            }
            if written < 0 {
                let error = std::io::Error::last_os_error();
                match error.kind() {
                    std::io::ErrorKind::WouldBlock => break,
                    std::io::ErrorKind::Interrupted => continue,
                    _ => panic!("pipe prefill write failed: {error}"),
                }
            }
            break;
        }
        assert!(
            total > 0,
            "pipe prefill must have transferred data before the buffer filled"
        );
        assert!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags) } >= 0,
            "F_SETFL restore must succeed"
        );
        total
    }

    #[test]
    fn ac3a_starter_hangs_on_unread_stdin_killed_and_cancelled() {
        use std::process::{Command, Stdio};
        use std::os::unix::process::CommandExt;

        // Lock-free write acknowledgement: bootstrap flips `write_entered`
        // once it starts the oversized write and `write_done` after it
        // returns. Both are plain atomics, so an observer holding no store,
        // state, or Driver lock can prove the deadline cancelled a genuinely
        // in-flight, unfinished bootstrap write instead of merely a late
        // claim/handle publication.
        let write_entered = Arc::new(AtomicBool::new(false));
        let write_done = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let write_entered_for_factory = Arc::clone(&write_entered);
        let write_done_for_factory = Arc::clone(&write_done);
        let stopped_for_factory = Arc::clone(&stopped);

        let (dir, scheduler, store) = test_harness(move |_| {
            let mut cmd = Command::new("sleep");
            cmd.arg("60")
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut child = cmd.spawn().unwrap();
            let pid = child.id();
            let pgid = pid as i32;
            let stdin = child.stdin.take().unwrap();
            let identity = external_runtime::ProcessIdentity {
                pid,
                pgid,
                uid: 0,
                start_token: "token".into(),
            };
            let child = Arc::new(std::sync::Mutex::new(Some(child)));
            let child_clone = Arc::clone(&child);

            struct StdinHangingRuntime {
                identity: external_runtime::ProcessIdentity,
                stopped: Arc<AtomicBool>,
                child: Arc<std::sync::Mutex<Option<std::process::Child>>>,
                stdin: std::sync::Mutex<Option<std::process::ChildStdin>>,
                write_entered: Arc<AtomicBool>,
                write_done: Arc<AtomicBool>,
            }
            impl ManagedRuntime for StdinHangingRuntime {
                fn identity(&self) -> Option<external_runtime::ProcessIdentity> {
                    Some(self.identity.clone())
                }
                fn stop(&self, _: Duration) -> RuntimeTerminal {
                    self.stopped.store(true, Ordering::Release);
                    if let Some(mut child) = self.child.lock().unwrap().take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    RuntimeTerminal::Stopped(external_runtime::StopOutcome::AlreadyExited(
                        external_runtime::ChildExit::Exited(Some(0)),
                    ))
                }
                fn wait_terminal(&self, _: Duration) -> Option<RuntimeTerminal> {
                    std::thread::sleep(Duration::from_millis(5));
                    if self.stopped.load(Ordering::Acquire) {
                        return Some(self.stop(Duration::ZERO));
                    }
                    if let Ok(mut lock) = self.child.try_lock() {
                        if let Some(ref mut c) = *lock {
                            if let Ok(Some(status)) = c.try_wait() {
                                return Some(RuntimeTerminal::Stopped(external_runtime::StopOutcome::AlreadyExited(
                                    external_runtime::ChildExit::Exited(status.code()),
                                )));
                            }
                        }
                    }
                    None
                }
                fn bootstrap_session(
                    &self,
                    _: &TaskRecord,
                    _: Duration,
                ) -> Result<SessionReady, RuntimeCommandError> {
                    use std::io::Write;
                    let mut writer = self
                        .stdin
                        .lock()
                        .unwrap()
                        .take()
                        .expect("bootstrap stdin must be available once");
                    // Fill the pipe to capacity first and only then publish
                    // `write_entered`: with a full pipe and a `sleep` reader
                    // that never drains stdin, the following write cannot
                    // return on any schedule until the reader dies.
                    prefill_pipe_buffer(&writer);
                    let big_payload = vec![b'a'; 4 * 1024 * 1024];
                    self.write_entered.store(true, Ordering::Release);
                    let _ = writer.write_all(&big_payload);
                    self.write_done.store(true, Ordering::Release);
                    Ok(SessionReady {
                        session_id: "never".into(),
                        initial_turn_id: None,
                        configured_model: Some("test-model".into()),
                    })
                }
                fn turn_snapshot(&self) -> TurnSnapshot {
                    TurnSnapshot { generation: 1, active: true, boundary: None }
                }
                fn inject_turn(&self, _: &str, _: &str, _: Duration) -> Result<Option<String>, RuntimeCommandError> {
                    Ok(None)
                }
            }

            Ok(Arc::new(StdinHangingRuntime {
                identity,
                stopped: Arc::clone(&stopped_for_factory),
                child: child_clone,
                stdin: std::sync::Mutex::new(Some(stdin)),
                write_entered: Arc::clone(&write_entered_for_factory),
                write_done: Arc::clone(&write_done_for_factory),
            }))
        });

        // Deterministic ordering: the deadline cancellation runs only after
        // the bootstrap write is confirmed entered and still unfinished.
        let write_entered_for_cancel = Arc::clone(&write_entered);
        let write_done_for_cancel = Arc::clone(&write_done);
        let write_blocked_at_cancel = Arc::new(AtomicBool::new(false));
        let blocked_for_cancel = Arc::clone(&write_blocked_at_cancel);
        scheduler.set_before_spawn_cancel_hook(Some(Arc::new(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !write_entered_for_cancel.load(Ordering::Acquire) {
                assert!(
                    Instant::now() < deadline,
                    "bootstrap never entered the blocking write before the deadline cancel"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            blocked_for_cancel.store(
                !write_done_for_cancel.load(Ordering::Acquire),
                Ordering::Release,
            );
        })));

        scheduler.set_spawn_wait_budget(Some(Duration::from_millis(50)));
        scheduler.set_spawn_convergence_budget(Some(Duration::from_millis(150)));

        let manifest = default_manifest(dir.path());
        let admission = default_admission();
        let start_time = Instant::now();
        let err = scheduler
            .submit_and_start_general(&manifest, admission, &|| false)
            .unwrap_err();
        let elapsed = start_time.elapsed();
        assert!(elapsed < Duration::from_secs(5), "timeout took too long: {elapsed:?}");
        assert!(
            write_blocked_at_cancel.load(Ordering::Acquire),
            "bootstrap write must have been in flight and unfinished when the deadline cancelled"
        );
        assert!(
            write_done.load(Ordering::Acquire),
            "the lock-free kill must make the blocking bootstrap write return"
        );
        match err {
            SchedulerError::StartTimeout { agent_id, message } => {
                assert!(message.contains("timed out"));
                let record = store.get_task(&agent_id).unwrap().unwrap();
                assert_eq!(record.phase, TaskPhase::Terminal);
                assert_eq!(record.outcome, Some(TaskOutcome::Cancelled));
            }
            other => panic!("expected StartTimeout, got {other:?}"),
        }
        // The starter consumed the committed cancellation and finished its
        // cleanup: its stop() ran and no active runtime remains registered.
        let cleanup_deadline = Instant::now() + Duration::from_secs(2);
        while !stopped.load(Ordering::Acquire) && Instant::now() < cleanup_deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(stopped.load(Ordering::Acquire), "starter cleanup must stop the runtime");
        assert_eq!(scheduler.active_count(), 0, "starter cleanup must unregister the runtime");
    }

    #[test]
    fn ac3b_background_claim_hangs_timeout_and_cancelled() {
        use std::process::{Command, Stdio};
        use std::os::unix::process::CommandExt;

        // Lock-free write acknowledgement, as in AC3(a): proves the deadline
        // cancelled an in-flight bootstrap write owned by the background claim
        // thread, not a late claim/handle publication.
        let write_entered = Arc::new(AtomicBool::new(false));
        let write_done = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let write_entered_for_factory = Arc::clone(&write_entered);
        let write_done_for_factory = Arc::clone(&write_done);
        let stopped_for_factory = Arc::clone(&stopped);

        let (dir, scheduler, store) = test_harness(move |_| {
            let mut cmd = Command::new("sleep");
            cmd.arg("60")
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut child = cmd.spawn().unwrap();
            let pid = child.id();
            let pgid = pid as i32;
            let stdin = child.stdin.take().unwrap();
            let identity = external_runtime::ProcessIdentity {
                pid,
                pgid,
                uid: 0,
                start_token: "token".into(),
            };
            let child = Arc::new(std::sync::Mutex::new(Some(child)));
            let child_clone = Arc::clone(&child);

            struct StdinHangingRuntime {
                identity: external_runtime::ProcessIdentity,
                stopped: Arc<AtomicBool>,
                child: Arc<std::sync::Mutex<Option<std::process::Child>>>,
                stdin: std::sync::Mutex<Option<std::process::ChildStdin>>,
                write_entered: Arc<AtomicBool>,
                write_done: Arc<AtomicBool>,
            }
            impl ManagedRuntime for StdinHangingRuntime {
                fn identity(&self) -> Option<external_runtime::ProcessIdentity> {
                    Some(self.identity.clone())
                }
                fn stop(&self, _: Duration) -> RuntimeTerminal {
                    self.stopped.store(true, Ordering::Release);
                    if let Some(mut child) = self.child.lock().unwrap().take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    RuntimeTerminal::Stopped(external_runtime::StopOutcome::AlreadyExited(
                        external_runtime::ChildExit::Exited(Some(0)),
                    ))
                }
                fn wait_terminal(&self, _: Duration) -> Option<RuntimeTerminal> {
                    std::thread::sleep(Duration::from_millis(5));
                    if self.stopped.load(Ordering::Acquire) {
                        return Some(self.stop(Duration::ZERO));
                    }
                    if let Ok(mut lock) = self.child.try_lock() {
                        if let Some(ref mut c) = *lock {
                            if let Ok(Some(status)) = c.try_wait() {
                                return Some(RuntimeTerminal::Stopped(external_runtime::StopOutcome::AlreadyExited(
                                    external_runtime::ChildExit::Exited(status.code()),
                                )));
                            }
                        }
                    }
                    None
                }
                fn bootstrap_session(
                    &self,
                    _: &TaskRecord,
                    _: Duration,
                ) -> Result<SessionReady, RuntimeCommandError> {
                    use std::io::Write;
                    let mut writer = self
                        .stdin
                        .lock()
                        .unwrap()
                        .take()
                        .expect("bootstrap stdin must be available once");
                    // Fill the pipe to capacity first and only then publish
                    // `write_entered`: with a full pipe and a `sleep` reader
                    // that never drains stdin, the following write cannot
                    // return on any schedule until the reader dies.
                    prefill_pipe_buffer(&writer);
                    let big_payload = vec![b'a'; 4 * 1024 * 1024];
                    self.write_entered.store(true, Ordering::Release);
                    let _ = writer.write_all(&big_payload);
                    self.write_done.store(true, Ordering::Release);
                    Ok(SessionReady {
                        session_id: "never".into(),
                        initial_turn_id: None,
                        configured_model: Some("test-model".into()),
                    })
                }
                fn turn_snapshot(&self) -> TurnSnapshot {
                    TurnSnapshot { generation: 1, active: true, boundary: None }
                }
                fn inject_turn(&self, _: &str, _: &str, _: Duration) -> Result<Option<String>, RuntimeCommandError> {
                    Ok(None)
                }
            }

            Ok(Arc::new(StdinHangingRuntime {
                identity,
                stopped: Arc::clone(&stopped_for_factory),
                child: child_clone,
                stdin: std::sync::Mutex::new(Some(stdin)),
                write_entered: Arc::clone(&write_entered_for_factory),
                write_done: Arc::clone(&write_done_for_factory),
            }))
        });

        let bg_finished = Arc::new(AtomicBool::new(false));
        let bg_finished_for_hook = Arc::clone(&bg_finished);
        let scheduler_for_bg = scheduler.clone();
        scheduler.set_before_claim_hook(Some(Arc::new(move || {
            if let Ok(Some(claim)) = scheduler_for_bg.inner.store.claim_next("bg-claim-thread", 10, 1) {
                let s = scheduler_for_bg.clone();
                let finished = Arc::clone(&bg_finished_for_hook);
                std::thread::spawn(move || {
                    let _ = s.start_claim(claim);
                    finished.store(true, Ordering::Release);
                });
                std::thread::sleep(Duration::from_millis(50));
            }
        })));

        // Deterministic ordering: the deadline cancellation only runs once the
        // background claim's bootstrap write is confirmed entered and unfinished.
        let write_entered_for_cancel = Arc::clone(&write_entered);
        let write_done_for_cancel = Arc::clone(&write_done);
        let write_blocked_at_cancel = Arc::new(AtomicBool::new(false));
        let blocked_for_cancel = Arc::clone(&write_blocked_at_cancel);
        scheduler.set_before_spawn_cancel_hook(Some(Arc::new(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !write_entered_for_cancel.load(Ordering::Acquire) {
                assert!(
                    Instant::now() < deadline,
                    "background claim never entered the blocking write before the deadline cancel"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            blocked_for_cancel.store(
                !write_done_for_cancel.load(Ordering::Acquire),
                Ordering::Release,
            );
        })));

        scheduler.set_spawn_wait_budget(Some(Duration::from_millis(50)));
        scheduler.set_spawn_convergence_budget(Some(Duration::from_millis(150)));

        let manifest = default_manifest(dir.path());
        let admission = default_admission();
        let start = Instant::now();
        let err = scheduler
            .submit_and_start_general(&manifest, admission, &|| false)
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(5), "must be bounded");
        assert!(
            write_blocked_at_cancel.load(Ordering::Acquire),
            "background bootstrap write must have been in flight and unfinished at cancellation"
        );
        assert!(
            write_done.load(Ordering::Acquire),
            "the lock-free kill must make the background claim's blocking write return"
        );
        match err {
            SchedulerError::StartTimeout { agent_id, .. } => {
                let record = store.get_task(&agent_id).unwrap().unwrap();
                assert_eq!(record.phase, TaskPhase::Terminal);
                assert_eq!(record.outcome, Some(TaskOutcome::Cancelled));
            }
            other => panic!("expected StartTimeout, got {other:?}"),
        }
        // The background claim consumed the committed cancellation and exited
        // its cleanup (stop() ran, runtime unregistered).
        let cleanup_deadline = Instant::now() + Duration::from_secs(2);
        while (!bg_finished.load(Ordering::Acquire) || !stopped.load(Ordering::Acquire))
            && Instant::now() < cleanup_deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(bg_finished.load(Ordering::Acquire), "background claim thread must exit");
        assert!(stopped.load(Ordering::Acquire), "background claim cleanup must stop the runtime");
        assert_eq!(scheduler.active_count(), 0, "background claim cleanup must unregister the runtime");
    }

    #[test]
    fn ac3c_cancel_before_starting_handle_published_starter_recheck_consumes() {
        let pause_gate = Arc::new(AtomicBool::new(false));
        let pause_gate_clone = Arc::clone(&pause_gate);
        let starter_reached = Arc::new(AtomicBool::new(false));
        let starter_reached_clone = Arc::clone(&starter_reached);
        let bootstrap_entered = Arc::new(AtomicBool::new(false));
        let bootstrap_entered_clone = Arc::clone(&bootstrap_entered);

        let (dir, scheduler, store) = test_harness(move |_| {
            let bootstrap_clone = Arc::clone(&bootstrap_entered_clone);
            Ok(Arc::new(TestRuntime {
                session_id: "session-never".into(),
                model: Some("test-model".into()),
                stopped: Arc::new(AtomicBool::new(false)),
                identity: None,
                bootstrap_hook: Some(Arc::new(move || {
                    bootstrap_clone.store(true, Ordering::Release);
                })),
                fail_bootstrap: None,
            }))
        });

        scheduler.set_before_starting_handle_hook(Some(Arc::new(move || {
            starter_reached_clone.store(true, Ordering::Release);
            while !pause_gate_clone.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(2));
            }
        })));

        scheduler.set_spawn_wait_budget(Some(Duration::from_millis(50)));
        scheduler.set_spawn_convergence_budget(Some(Duration::from_millis(100)));

        let manifest = default_manifest(dir.path());
        let admission = default_admission();

        let scheduler_clone = scheduler.clone();
        let join_handle = std::thread::spawn(move || {
            scheduler_clone.submit_and_start_general(&manifest, admission, &|| false)
        });

        while !starter_reached.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(2));
        }

        std::thread::sleep(Duration::from_millis(80));
        pause_gate.store(true, Ordering::Release);

        let err = join_handle.join().unwrap().unwrap_err();
        match err {
            SchedulerError::StartTimeout { agent_id, .. } => {
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline {
                    let record = store.get_task(&agent_id).unwrap().unwrap();
                    if record.phase == TaskPhase::Terminal {
                        assert_eq!(record.outcome, Some(TaskOutcome::Cancelled));
                        assert!(
                            !bootstrap_entered.load(Ordering::Acquire),
                            "starter must not enter bootstrap when cancelled in fresh check"
                        );
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                panic!("starter did not terminate task as cancelled");
            }
            other => panic!("expected StartTimeout, got {other:?}"),
        }
    }

    #[test]
    fn ac3d_queued_never_claimed_rpc_in_place_finishes_and_frees_workspace() {
        let spawned_anything = Arc::new(AtomicBool::new(false));
        let spawned_clone = Arc::clone(&spawned_anything);

        let (dir, scheduler, store) = test_harness(move |_| {
            spawned_clone.store(true, Ordering::Release);
            Ok(Arc::new(TestRuntime {
                session_id: "never".into(),
                model: Some("test-model".into()),
                stopped: Arc::new(AtomicBool::new(false)),
                identity: None,
                bootstrap_hook: None,
                fail_bootstrap: None,
            }))
        });

        scheduler.set_before_claim_hook(Some(Arc::new(|| {
            std::thread::sleep(Duration::from_millis(300));
        })));

        scheduler.set_spawn_wait_budget(Some(Duration::from_millis(50)));
        scheduler.set_spawn_convergence_budget(Some(Duration::from_millis(50)));

        let manifest = default_manifest(dir.path());
        let admission = default_admission();
        let start = Instant::now();
        let err = scheduler
            .submit_and_start_general(&manifest, admission.clone(), &|| false)
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(3), "must be bounded");
        assert!(!spawned_anything.load(Ordering::Acquire), "must never spawn any runtime");

        match err {
            SchedulerError::StartTimeout { agent_id, .. } => {
                let record = store.get_task(&agent_id).unwrap().unwrap();
                assert_eq!(record.phase, TaskPhase::Terminal);
                assert_eq!(record.outcome, Some(TaskOutcome::Cancelled));
                assert!(record.reaped_at.is_some(), "must be reaped in-place");
            }
            other => panic!("expected StartTimeout, got {other:?}"),
        }

        scheduler.set_before_claim_hook(None);

        let second_record = scheduler
            .submit_and_start_general(&manifest, admission, &|| false)
            .expect("workspace must be freed for new submission");
        assert_eq!(second_record.phase, TaskPhase::Running);
    }

    #[test]
    fn ac3e_interrupt_after_cancel_commit_returns_cancelled_timeout_not_interrupted() {
        // B-F01: once the conditional cancellation transaction has committed
        // (stop_requested/CANCELLING persisted) the task is converging to
        // cancelled. A request interrupt racing that convergence must not be
        // reported as `Interrupted` ("session establishment continues"); it
        // must surface the committed cancellation through the cancelled
        // StartTimeout variant and stay responsive.
        let starter_paused = Arc::new(AtomicBool::new(false));
        let starter_paused_clone = Arc::clone(&starter_paused);
        let pause_gate = Arc::new(AtomicBool::new(false));
        let pause_gate_clone = Arc::clone(&pause_gate);

        let (dir, scheduler, store) = test_harness(|_| {
            Ok(Arc::new(TestRuntime {
                session_id: "session-never-published".into(),
                model: Some("test-model".into()),
                stopped: Arc::new(AtomicBool::new(false)),
                identity: None,
                bootstrap_hook: None,
                fail_bootstrap: None,
            }))
        });

        // Pause the starter after the factory returned but before it publishes
        // the starting handle or enters bootstrap: the row stays PREPARING
        // with no killable handle, so the only terminal path is the committed
        // cancellation reaching the starter's fresh recheck.
        scheduler.set_before_starting_handle_hook(Some(Arc::new(move || {
            starter_paused_clone.store(true, Ordering::Release);
            while !pause_gate_clone.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(2));
            }
        })));

        scheduler.set_spawn_wait_budget(Some(Duration::from_millis(50)));
        scheduler.set_spawn_convergence_budget(Some(Duration::from_millis(1000)));

        let manifest = default_manifest(dir.path());
        let admission = default_admission();
        let interrupted = Arc::new(AtomicBool::new(false));
        let interrupted_clone = Arc::clone(&interrupted);

        let scheduler_clone = scheduler.clone();
        let join_handle = std::thread::spawn(move || {
            scheduler_clone.submit_and_start_general(&manifest, admission, &move || {
                interrupted_clone.load(Ordering::Acquire)
            })
        });

        // Wait until the claim is owned (PREPARING) and the starter is parked.
        let park_deadline = Instant::now() + Duration::from_secs(3);
        while !starter_paused.load(Ordering::Acquire) {
            assert!(Instant::now() < park_deadline, "starter never reached its pause point");
            std::thread::sleep(Duration::from_millis(1));
        }

        // Wait for the cancellation transaction to commit (durable stop fact),
        // then assert the interrupt is observed before the next terminal read.
        let commit_deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let record = store.get_task("10000000").unwrap().unwrap();
            if record.stop_requested {
                assert_eq!(record.phase, TaskPhase::Cancelling);
                break;
            }
            assert!(
                Instant::now() < commit_deadline,
                "conditional cancellation never committed"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        interrupted.store(true, Ordering::Release);

        let err = join_handle.join().unwrap().unwrap_err();
        match err {
            SchedulerError::StartTimeout { agent_id, message } => {
                assert!(
                    message.contains("cancelled"),
                    "committed cancellation must be reported truthfully: {message}"
                );
                let record = store.get_task(&agent_id).unwrap().unwrap();
                assert!(record.stop_requested, "durable stop fact must hold");
                assert_eq!(record.phase, TaskPhase::Cancelling);
            }
            SchedulerError::Interrupted { .. } => panic!(
                "interrupt after the cancellation commit must not claim establishment continues"
            ),
            other => panic!("expected StartTimeout, got {other:?}"),
        }

        // The starter consumes the committed cancellation and terminates.
        pause_gate.store(true, Ordering::Release);
        let terminal_deadline = Instant::now() + Duration::from_secs(2);
        let mut terminal = false;
        while Instant::now() < terminal_deadline {
            let record = store.get_task("10000000").unwrap().unwrap();
            if record.phase == TaskPhase::Terminal {
                assert_eq!(record.outcome, Some(TaskOutcome::Cancelled));
                terminal = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(terminal, "starter must converge the cancelled task to terminal");
    }

    #[test]
    fn ac3f_interrupt_on_mismatched_cancel_returns_cancelled_timeout_not_interrupted() {
        // A-slot remaining gap: the mismatch convergence loop must not report
        // `Interrupted` ("session establishment continues") for a row that
        // already carries a durable stop (external cancel/close or a drain
        // fence). Entering that loop requires the conditional cancellation
        // transaction to mismatch while session_id is None and the phase is
        // non-terminal, i.e. a stop that someone else already persisted.
        let claim_gate = Arc::new(AtomicBool::new(false));
        let claim_gate_clone = Arc::clone(&claim_gate);

        let (dir, scheduler, store) = test_harness(|_| {
            Ok(Arc::new(TestRuntime {
                session_id: "never".into(),
                model: Some("test-model".into()),
                stopped: Arc::new(AtomicBool::new(false)),
                identity: None,
                bootstrap_hook: None,
                fail_bootstrap: None,
            }))
        });

        // Keep the starter parked so the row stays fresh in QUEUED across the
        // whole wait: it is another actor's stop, not this spawn, that lands.
        scheduler.set_before_claim_hook(Some(Arc::new(move || {
            while !claim_gate_clone.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(2));
            }
        })));

        // Persist a durable stop on the still-QUEUED row (external cancel /
        // close), so the later conditional cancellation transaction mismatches
        // with Err(current_task) and session_id None.
        let stop_persisted = Arc::new(AtomicBool::new(false));
        let store_for_poll = Arc::clone(&store);
        scheduler.set_spawn_poll_hook(Some(Arc::new(move |task: &TaskRecord| {
            if !stop_persisted.swap(true, Ordering::AcqRel) {
                let prior = store_for_poll
                    .cancel_unstarted_if_still_fresh(&task.agent_id)
                    .unwrap();
                assert!(prior.is_ok(), "row must be fresh for the external cancel");
            }
        })));

        // The interrupt is raised only once the deadline cancel is about to
        // run, i.e. while the mismatch convergence loop is entered.
        let interrupted = Arc::new(AtomicBool::new(false));
        let interrupted_for_hook = Arc::clone(&interrupted);
        scheduler.set_before_spawn_cancel_hook(Some(Arc::new(move || {
            interrupted_for_hook.store(true, Ordering::Release);
        })));
        let interrupted_for_admission = Arc::clone(&interrupted);

        scheduler.set_spawn_wait_budget(Some(Duration::from_millis(50)));
        scheduler.set_spawn_convergence_budget(Some(Duration::from_millis(200)));

        let manifest = default_manifest(dir.path());
        let admission = default_admission();
        let err = scheduler
            .submit_and_start_general(&manifest, admission, &move || {
                interrupted_for_admission.load(Ordering::Acquire)
            })
            .unwrap_err();
        assert!(interrupted.load(Ordering::Acquire));

        match err {
            SchedulerError::StartTimeout { agent_id, message } => {
                assert!(
                    message.contains("cancelled"),
                    "a mismatched durable stop must be reported as cancelled: {message}"
                );
                let record = store.get_task(&agent_id).unwrap().unwrap();
                assert!(record.stop_requested, "durable stop fact must hold");
                assert_eq!(record.phase, TaskPhase::Cancelling);
            }
            SchedulerError::Interrupted { .. } => panic!(
                "interrupt on a row with a durable stop must not claim establishment continues"
            ),
            other => panic!("expected StartTimeout, got {other:?}"),
        }

        claim_gate.store(true, Ordering::Release);
    }

    #[test]
    fn ac4_interrupted_while_starter_runs_returns_interrupted_task_continues() {
        let (dir, scheduler, store) = test_harness(|_| {
            Ok(Arc::new(TestRuntime {
                session_id: "session-ac4".into(),
                model: Some("test-model".into()),
                stopped: Arc::new(AtomicBool::new(false)),
                identity: None,
                bootstrap_hook: Some(Arc::new(|| {
                    std::thread::sleep(Duration::from_millis(30));
                })),
                fail_bootstrap: None,
            }))
        });

        let interrupted_flag = Arc::new(AtomicBool::new(false));
        let flag_clone = Arc::clone(&interrupted_flag);

        let manifest = default_manifest(dir.path());
        let admission = default_admission();

        let flag_setter = Arc::clone(&interrupted_flag);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            flag_setter.store(true, Ordering::Release);
        });

        let err = scheduler
            .submit_and_start_general(&manifest, admission, &move || flag_clone.load(Ordering::Acquire))
            .unwrap_err();

        match err {
            SchedulerError::Interrupted { agent_id } => {
                let deadline = Instant::now() + Duration::from_secs(3);
                while Instant::now() < deadline {
                    let record = store.get_task(&agent_id).unwrap().unwrap();
                    if record.phase == TaskPhase::Running {
                        assert_eq!(record.session_id.as_deref(), Some("session-ac4"));
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                panic!("task was cancelled or failed instead of reaching Running");
            }
            other => panic!("expected Interrupted, got {other:?}"),
        }
    }

    #[test]
    fn ac4b_resume_transient_waits_for_running_and_does_not_cancel() {
        let (dir, scheduler, store) = test_harness(|_| {
            Ok(Arc::new(TestRuntime {
                session_id: "session-resume".into(),
                model: Some("test-model".into()),
                stopped: Arc::new(AtomicBool::new(false)),
                identity: None,
                bootstrap_hook: None,
                fail_bootstrap: None,
            }))
        });

        let manifest = default_manifest(dir.path());
        let record = scheduler
            .submit_and_start_general(&manifest, default_admission(), &|| false)
            .unwrap();
        let agent_id = record.agent_id.clone();
        store
            .transition_terminal(
                &agent_id,
                1,
                &external_store::TerminalUpdate {
                    outcome: TaskOutcome::Completed,
                    failure_code: None,
                    failure_message: None,
                },
            )
            .unwrap();

        store.reap_task(&agent_id).unwrap();
        store
            .requeue_task_for_resume_with_message(&agent_id, "msg-1", "queue", "test-prompt")
            .unwrap();

        let mismatch_queued = store.cancel_unstarted_if_still_fresh(&agent_id).unwrap();
        assert!(matches!(mismatch_queued, Err(t) if t.phase == TaskPhase::Queued && t.session_id.is_some() && !t.stop_requested));

        store
            .claim_specific(&agent_id, "test-owner", 1)
            .unwrap();
        let prep = store.get_task(&agent_id).unwrap().unwrap();
        assert_eq!(prep.phase, TaskPhase::Preparing);
        assert!(prep.session_id.is_some());

        let fresh_check = store.cancel_unstarted_if_still_fresh(&agent_id).unwrap();
        assert!(fresh_check.is_err(), "must mismatch because session_id is non-null");

        let intact = store.get_task(&agent_id).unwrap().unwrap();
        assert_eq!(intact.phase, TaskPhase::Preparing);
        assert!(!intact.stop_requested);
    }

    #[test]
    fn ac4b_scheduler_poll_reads_queued_and_preparing_with_session_then_projects_running() {
        let gate_claim = Arc::new(AtomicBool::new(false));
        let gate_claim_clone = Arc::clone(&gate_claim);
        let gate_bootstrap = Arc::new(AtomicBool::new(false));
        let gate_bootstrap_clone = Arc::clone(&gate_bootstrap);

        let saw_queued = Arc::new(AtomicBool::new(false));
        let saw_queued_clone = Arc::clone(&saw_queued);
        let saw_preparing = Arc::new(AtomicBool::new(false));
        let saw_preparing_clone = Arc::clone(&saw_preparing);

        let (dir, scheduler, store) = test_harness(|_| {
            Ok(Arc::new(TestRuntime {
                session_id: "session-preserved-interleaving".into(),
                model: Some("test-model".into()),
                stopped: Arc::new(AtomicBool::new(false)),
                identity: None,
                bootstrap_hook: None,
                fail_bootstrap: None,
            }))
        });

        // 1. Starter thread pauses before claim so task starts in QUEUED
        let store_path = store.database_path().to_path_buf();
        scheduler.set_before_claim_hook(Some(Arc::new(move || {
            // Simulate resumed task row having preserved session_id while still in QUEUED
            let conn = rusqlite::Connection::open(&store_path).unwrap();
            conn.execute(
                "UPDATE tasks SET session_id='session-preserved-interleaving' WHERE phase='QUEUED'",
                [],
            )
            .unwrap();
            while !gate_claim_clone.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(2));
            }
        })));

        // 2. Starter thread pauses before starting handle/bootstrap so task stays in PREPARING
        scheduler.set_before_starting_handle_hook(Some(Arc::new(move || {
            while !gate_bootstrap_clone.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(2));
            }
        })));

        // 3. spawn_poll_hook observes the read confirmation of both QUEUED and PREPARING with session_id
        let gate_claim_for_hook = Arc::clone(&gate_claim);
        let gate_bootstrap_for_hook = Arc::clone(&gate_bootstrap);
        scheduler.set_spawn_poll_hook(Some(Arc::new(move |task: &TaskRecord| {
            if task.phase == TaskPhase::Queued && task.session_id.as_deref() == Some("session-preserved-interleaving") {
                saw_queued_clone.store(true, Ordering::Release);
                gate_claim_for_hook.store(true, Ordering::Release);
            } else if task.phase == TaskPhase::Preparing && task.session_id.as_deref() == Some("session-preserved-interleaving") {
                saw_preparing_clone.store(true, Ordering::Release);
                gate_bootstrap_for_hook.store(true, Ordering::Release);
            }
        })));

        let manifest = default_manifest(dir.path());
        let admission = default_admission();

        let record = scheduler
            .submit_and_start_general(&manifest, admission, &|| false)
            .expect("must successfully project running once non-transient");

        assert!(
            saw_queued.load(Ordering::Acquire),
            "poll hook must have confirmed reading QUEUED with session_id"
        );
        assert!(
            saw_preparing.load(Ordering::Acquire),
            "poll hook must have confirmed reading PREPARING with session_id"
        );
        assert_eq!(record.phase, TaskPhase::Running);
        assert_eq!(
            record.session_id.as_deref(),
            Some("session-preserved-interleaving")
        );
    }

    #[test]
    fn ac4b_scheduler_resume_wait_shares_remaining_budget_without_renewal() {
        // AC 4b(d): the resume-transient wait must consume the *remaining*
        // part of the single total deadline. This fixture first burns most of
        // the budget on the fresh (unestablished) wait, then performs a real
        // establish -> terminal -> reap -> resume requeue (session preserved)
        // and confirms the poll read the resume-transient row. A wrong
        // implementation that granted a fresh full budget on entering resume
        // would return at roughly consume + budget; the bound below is
        // strictly earlier than that while leaving poll/scheduling slack.
        let budget = Duration::from_millis(500);
        let consume_target = Duration::from_millis(375);
        let renewal_bound = Duration::from_millis(700);

        let claim_gate = Arc::new(AtomicBool::new(false));
        let claim_gate_clone = Arc::clone(&claim_gate);
        let runtime_stopped = Arc::new(AtomicBool::new(false));
        let runtime_stopped_clone = Arc::clone(&runtime_stopped);
        let saw_resume_transient = Arc::new(AtomicBool::new(false));
        let saw_resume_transient_clone = Arc::clone(&saw_resume_transient);
        let transition_applied = Arc::new(AtomicBool::new(false));
        let transition_applied_clone = Arc::clone(&transition_applied);

        let (dir, scheduler, store) = test_harness({
            let stopped = Arc::clone(&runtime_stopped_clone);
            move |_| {
                Ok(Arc::new(TestRuntime {
                    session_id: "session-budget-shared".into(),
                    model: Some("test-model".into()),
                    stopped: Arc::clone(&stopped),
                    identity: None,
                    bootstrap_hook: None,
                    fail_bootstrap: None,
                }))
            }
        });

        // The starter never claims during the wait, so the row is only made
        // resume-transient by the deterministic poll hook below; the gate is
        // held until after the timeout so the resume driver belongs to the
        // requeueing client, exactly like a real follow-up.
        scheduler.set_before_claim_hook(Some(Arc::new(move || {
            while !claim_gate_clone.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(2));
            }
        })));

        let store_for_poll = Arc::clone(&store);
        let start = Instant::now();
        scheduler.set_spawn_poll_hook(Some(Arc::new(move |task: &TaskRecord| {
            if !transition_applied_clone.load(Ordering::Acquire)
                && start.elapsed() >= consume_target
            {
                // Persist the establishment fact on the still-QUEUED row,
                // then complete it, reap it, and requeue for resume. The
                // session id survives the requeue while stop/outcome clear.
                let conn = rusqlite::Connection::open(store_for_poll.database_path()).unwrap();
                conn.execute(
                    "UPDATE tasks SET session_id='session-budget-shared' WHERE phase='QUEUED'",
                    [],
                )
                .unwrap();
                drop(conn);
                store_for_poll
                    .transition_terminal(
                        &task.agent_id,
                        task.owner_epoch,
                        &external_store::TerminalUpdate {
                            outcome: TaskOutcome::Completed,
                            failure_code: None,
                            failure_message: None,
                        },
                    )
                    .unwrap();
                store_for_poll.reap_task(&task.agent_id).unwrap();
                assert!(
                    store_for_poll
                        .requeue_task_for_resume_with_message(
                            &task.agent_id,
                            "msg-budget-shared",
                            "queue",
                            "continue",
                        )
                        .unwrap(),
                    "resume requeue must be admitted"
                );
                transition_applied_clone.store(true, Ordering::Release);
            }
            if transition_applied_clone.load(Ordering::Acquire)
                && task.phase == TaskPhase::Queued
                && task.session_id.as_deref() == Some("session-budget-shared")
            {
                saw_resume_transient_clone.store(true, Ordering::Release);
            }
        })));

        scheduler.set_spawn_wait_budget(Some(budget));

        let manifest = default_manifest(dir.path());
        let admission = default_admission();

        let err = scheduler
            .submit_and_start_general(&manifest, admission, &|| false)
            .unwrap_err();
        let elapsed = start.elapsed();
        // AC 4b(d): application-level error before the MCP 125s / CLI 150s
        // transport deadlines, and before a renewed full budget could expire.
        assert!(
            elapsed < renewal_bound,
            "resume wait must share the remaining budget, not renew it: {elapsed:?}"
        );
        assert!(
            elapsed >= consume_target,
            "the fresh wait must consume most of the budget before resume: {elapsed:?}"
        );
        assert!(
            transition_applied.load(Ordering::Acquire),
            "fixture must have completed establish->terminal->reap->requeue"
        );
        assert!(
            saw_resume_transient.load(Ordering::Acquire),
            "poll hook must have confirmed reading the resume-transient row"
        );

        let timed_out_agent_id = match err {
            SchedulerError::StartTimeout { agent_id, message } => {
                assert!(message.contains("session established"));
                assert!(message.contains("resume"));
                assert!(message.contains("uncancelled"));
                agent_id
            }
            other => panic!("expected StartTimeout with resume variant message, got {other:?}"),
        };

        let task_during_timeout = store.get_task(&timed_out_agent_id).unwrap().unwrap();
        assert!(!task_during_timeout.stop_requested, "must not write stop_requested");
        assert_eq!(task_during_timeout.phase, TaskPhase::Queued);
        assert_eq!(
            task_during_timeout.session_id.as_deref(),
            Some("session-budget-shared")
        );
        assert!(!runtime_stopped.load(Ordering::Acquire), "must not send kill on resume timeout");

        // Follow-up proceeds normally: release the gate, the starter claims
        // the requeued row and resumes it to Running.
        claim_gate.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut reached_running = false;
        while Instant::now() < deadline {
            let record = store.get_task(&timed_out_agent_id).unwrap().unwrap();
            if record.phase == TaskPhase::Running {
                assert_eq!(record.session_id.as_deref(), Some("session-budget-shared"));
                assert!(!record.stop_requested);
                reached_running = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(reached_running, "follow-up must proceed to Running");
    }

    #[test]
    fn ac4b_scheduler_cancel_race_after_last_poll_returns_resume_mismatch_without_stop() {
        let claim_gate = Arc::new(AtomicBool::new(false));
        let claim_gate_clone = Arc::clone(&claim_gate);
        let runtime_stopped = Arc::new(AtomicBool::new(false));
        let runtime_stopped_clone = Arc::clone(&runtime_stopped);

        let (dir, scheduler, store) = test_harness(move |_| {
            Ok(Arc::new(TestRuntime {
                session_id: "session-raced".into(),
                model: Some("test-model".into()),
                stopped: Arc::clone(&runtime_stopped_clone),
                identity: None,
                bootstrap_hook: None,
                fail_bootstrap: None,
            }))
        });

        // Hold the starter thread from claiming so the row stays fresh in
        // QUEUED with session_id: None across every poll read.
        scheduler.set_before_claim_hook(Some(Arc::new(move || {
            while !claim_gate_clone.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(2));
            }
        })));

        // AC 4b(c): after the final fresh poll read and before the deadline
        // cancel transaction, another client's terminal send requeues the row
        // (session preserved, epoch unchanged). The conditional cancellation
        // must mismatch and the call must reclassify from the latest row.
        let store_path = store.database_path().to_path_buf();
        let race_applied = Arc::new(AtomicBool::new(false));
        let race_applied_clone = Arc::clone(&race_applied);
        scheduler.set_before_spawn_cancel_hook(Some(Arc::new(move || {
            race_applied_clone.store(true, Ordering::Release);
            let conn = rusqlite::Connection::open(&store_path).unwrap();
            conn.execute(
                "UPDATE tasks SET phase='QUEUED', session_id='session-raced-requeued', stop_requested=0",
                [],
            )
            .unwrap();
        })));

        scheduler.set_spawn_wait_budget(Some(Duration::from_millis(50)));

        let manifest = default_manifest(dir.path());
        let admission = default_admission();

        let start = Instant::now();
        let err = scheduler
            .submit_and_start_general(&manifest, admission, &|| false)
            .unwrap_err();
        assert!(
            race_applied.load(Ordering::Acquire),
            "cancel-boundary requeue hook must have fired"
        );
        // AC 4b(d): the application-level error must return well before the
        // MCP 125s / CLI 150s transport deadlines.
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "resume mismatch must return an application error promptly"
        );

        match err {
            SchedulerError::StartTimeout { agent_id, message } => {
                assert!(message.contains("session established"));
                assert!(message.contains("resume"));
                assert!(message.contains("uncancelled"));

                let task = store.get_task(&agent_id).unwrap().unwrap();
                assert!(
                    !task.stop_requested,
                    "conditional cancellation must mismatch and not write stop"
                );
                assert_eq!(task.phase, TaskPhase::Queued);
                assert_eq!(task.session_id.as_deref(), Some("session-raced-requeued"));
                assert!(
                    !runtime_stopped.load(Ordering::Acquire),
                    "must not kill the runtime on a resume mismatch"
                );
            }
            other => panic!("expected StartTimeout with resume mismatch message, got {other:?}"),
        }

        // Follow-up proceeds: release the claim gate, the starter claims the
        // requeued row and drives it to Running.
        claim_gate.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut reached_running = false;
        while Instant::now() < deadline {
            let record = store.get_task("10000000").unwrap().unwrap();
            if record.phase == TaskPhase::Running {
                reached_running = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(reached_running, "resume follow-up must proceed to Running");
    }

    #[test]
    fn ac5_workspace_busy_and_drain_unavailable() {
        let (dir, scheduler, _) = test_harness(|_| {
            Ok(Arc::new(TestRuntime {
                session_id: "session-ac5".into(),
                model: Some("test-model".into()),
                stopped: Arc::new(AtomicBool::new(false)),
                identity: None,
                bootstrap_hook: None,
                fail_bootstrap: None,
            }))
        });

        let manifest = default_manifest(dir.path());
        let _first = scheduler
            .submit_and_start_general(&manifest, default_admission(), &|| false)
            .unwrap();

        let second_err = scheduler
            .submit_and_start_general(&manifest, default_admission(), &|| false)
            .unwrap_err();
        match second_err {
            SchedulerError::Store(external_store::StoreError::Conflict(msg)) => {
                assert!(msg.starts_with("WORKSPACE_BUSY"), "expected WORKSPACE_BUSY, got {msg}");
            }
            other => panic!("expected Store Conflict WORKSPACE_BUSY, got {other:?}"),
        }

        let dir2 = tempfile::tempdir().unwrap();
        let manifest2 = default_manifest(dir2.path());
        scheduler.begin_drain();
        let drain_err = scheduler
            .submit_and_start_general(&manifest2, default_admission(), &|| false)
            .unwrap_err();
        match drain_err {
            SchedulerError::StartFailed { reason, .. } => {
                assert!(reason == "DRAIN_CANCELLED" || reason == "daemon_draining");
            }
            SchedulerError::InvalidConfig(reason) => {
                assert_eq!(reason, "daemon_draining");
            }
            other => panic!("expected StartFailed DRAIN_CANCELLED, got {other:?}"),
        }
    }

    #[test]
    fn ac6_head_of_line_blocking_avoidance() {
        let a_started = Arc::new(AtomicBool::new(false));
        let a_started_clone = Arc::clone(&a_started);
        let a_release = Arc::new(AtomicBool::new(false));
        let a_release_clone = Arc::clone(&a_release);

        let (dir, scheduler, _) = test_harness(move |record| {
            if record.repository.contains("ws-a") {
                a_started_clone.store(true, Ordering::Release);
                while !a_release_clone.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            Ok(Arc::new(TestRuntime {
                session_id: "session-ok".into(),
                model: Some("test-model".into()),
                stopped: Arc::new(AtomicBool::new(false)),
                identity: None,
                bootstrap_hook: None,
                fail_bootstrap: None,
            }))
        });

        let ws_a = dir.path().join("ws-a");
        let ws_b = dir.path().join("ws-b");
        std::fs::create_dir_all(&ws_a).unwrap();
        std::fs::create_dir_all(&ws_b).unwrap();

        let manifest_a = default_manifest(&ws_a);
        let manifest_b = default_manifest(&ws_b);

        let sched_a = scheduler.clone();
        let handle_a = std::thread::spawn(move || {
            sched_a.submit_and_start_general(&manifest_a, default_admission(), &|| false)
        });

        while !a_started.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(2));
        }

        let start_b = Instant::now();
        let record_b = scheduler
            .submit_and_start_general(&manifest_b, default_admission(), &|| false)
            .expect("Task B must succeed");
        assert!(start_b.elapsed() < Duration::from_secs(2), "Task B was blocked behind A");
        assert_eq!(record_b.phase, TaskPhase::Running);

        scheduler.begin_drain();
        let ws_c = dir.path().join("ws-c");
        std::fs::create_dir_all(&ws_c).unwrap();
        let manifest_c = default_manifest(&ws_c);
        let start_c = Instant::now();
        let err_c = scheduler
            .submit_and_start_general(&manifest_c, default_admission(), &|| false)
            .unwrap_err();
        assert!(start_c.elapsed() < Duration::from_millis(500), "admission was blocked by A's wait loop");
        match err_c {
            SchedulerError::StartFailed { reason, .. } => {
                assert!(reason == "DRAIN_CANCELLED" || reason == "daemon_draining");
            }
            SchedulerError::InvalidConfig(reason) => {
                assert_eq!(reason, "daemon_draining");
            }
            other => panic!("expected drain refusal, got {other:?}"),
        }

        a_release.store(true, Ordering::Release);
        let _ = handle_a.join().unwrap();
    }
}
