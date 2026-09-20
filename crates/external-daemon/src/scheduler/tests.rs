use super::diagnostics::{
    runtime_failure_record, update_latest_failure, DiagnosticLogger, RotatingDiagnosticWriter,
    DIAGNOSTIC_FILE_BYTES, DIAGNOSTIC_QUEUE_CAPACITY, DIAGNOSTIC_RECORD_BYTES,
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
            assert_eq!(
                scheduler
                    .store()
                    .get_task(&agent_id)
                    .unwrap()
                    .unwrap()
                    .outcome,
                Some(TaskOutcome::Failed)
            );
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
                    .queue_message(&agent_id, "counted", "follow-up")
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
                .queue_message(&agent_id, "counted", "follow-up")
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
                .queue_message(&agent_id, "unknown", "never replay")
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
        assert_eq!(
            scheduler
                .store()
                .get_task(&agent_id)
                .unwrap()
                .unwrap()
                .outcome,
            Some(TaskOutcome::RuntimeLost)
        );
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
                .queue_message(agent, "existing", content)
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
                .queue_message(&agent_id, "existing", "original")
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
}
