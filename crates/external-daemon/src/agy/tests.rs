#![cfg(unix)]
//! Scripted NDJSON contract tests for the `agy` adapter: gate, launch
//! admission threading, bootstrap on `init`, turn settlement, multi-turn
//! message delivery, tool activity, cancellation, the idle-signal stream-noise
//! trap, malformed-line tolerance, interleaved child processes, and the
//! process-group reap proof.

use super::*;
use crate::{
    CommandRuntimeFactory, LifecycleSink, ManagedRuntime, MessageDisposition, RuntimeFactory,
    Scheduler, SchedulerConfig,
};
use external_core::{AdmissionIdentity, GeneralTaskManifest, PermissionMode, GENERAL_TASK_SCHEMA};
use external_store::{TaskOutcome, TaskPhase, TaskRecord, TurnState};
use std::{
    io,
    path::Path,
    process::Command,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const CONV: &str = "2d8c26a0-83b7-457d-9214-6674f6da52bc";
const MODEL: &str = "claude-sonnet-4-6";

/// Serialize tests that drive scripted children through the whole scheduler,
/// mirroring the DSH/Codex suite's scripted-child guard.
fn scripted_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static SCRIPTED_CHILD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SCRIPTED_CHILD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const SCRIPTED_SYNC_WAIT: Duration = Duration::from_secs(30);

fn agy_workspace() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("agy-")
        .tempdir_in(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace"))
        .unwrap()
}

fn agy_admission(model: Option<&str>) -> AdmissionIdentity {
    agy_admission_with_effort(model, None)
}

fn agy_admission_with_effort(model: Option<&str>, effort: Option<&str>) -> AdmissionIdentity {
    AdmissionIdentity {
        agent: "agy".into(),
        config_revision: 1,
        adapter_version: env!("CARGO_PKG_VERSION").into(),
        model: model.map(str::to_owned),
        model_source: "catalog_token".into(),
        effort: effort.map(str::to_owned),
        developer_instructions: None,
    }
}

fn manifest_for_mode(
    workspace: &Path,
    prompt: &str,
    permission_mode: PermissionMode,
) -> GeneralTaskManifest {
    GeneralTaskManifest {
        schema: GENERAL_TASK_SCHEMA.into(),
        agent_id: "agy-test".into(),
        repository: workspace.canonicalize().unwrap(),
        permission_mode: Some(permission_mode),
        prompt: prompt.into(),
        write_manifest: Vec::new(),
    }
}

fn manifest_for(workspace: &Path, prompt: &str) -> GeneralTaskManifest {
    manifest_for_mode(workspace, prompt, PermissionMode::Build)
}

fn agy_scheduler(workspace: &Path, agy: AgyRuntimeFactory) -> Scheduler {
    let zcode = CommandRuntimeFactory::new(|_: &TaskRecord| {
        Err(io::Error::other("zcode route must not spawn in agy tests"))
    });
    let store = Arc::new(external_store::Store::open(workspace.join("state.sqlite")).unwrap());
    Scheduler::new(
        "agy-test",
        store,
        Arc::new(crate::dsh::RoutingRuntimeFactory::with_agy(
            zcode,
            crate::dsh::DshRuntimeFactory::closed(),
            crate::codex::CodexRuntimeFactory::closed(),
            agy,
        )),
        SchedulerConfig {
            bootstrap_timeout: Duration::from_secs(30),
            control_timeout: Duration::from_secs(30),

            ..SchedulerConfig::default()
        },
    )
    .unwrap()
}

/// Write the shared scripted-child harness at `path`. It records its own full
/// argv to `<workspace>/argv.log` (`ARG<value>` per argument) and every stdin
/// line to `<workspace>/deliveries.jsonl`; `script` is injected before the
/// trailing stdin drain.
fn write_scripted_child(path: &Path, workspace: &Path, script: &str) {
    let template = r#"#!/bin/sh
LOG_PATH=__LOG__
ARGV_PATH=__ARGV__
log() { printf '%s\n' "$1" >> "$LOG_PATH"; }
record() { for a in "$@"; do printf 'ARG%s\n' "$a" >> "$ARGV_PATH"; done; }
read_frame() { IFS= read -r line; log "$line"; }
record "$@"
__SCRIPT__
while IFS= read -r line; do log "$line"; done
"#;
    let harness = template
        .replace(
            "__LOG__",
            &format!("{:?}", workspace.join("deliveries.jsonl").to_string_lossy()),
        )
        .replace(
            "__ARGV__",
            &format!("{:?}", workspace.join("argv.log").to_string_lossy()),
        )
        .replace("__SCRIPT__", script);
    std::fs::write(path, harness).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn harness_factory(script: &str, workspace: &Path) -> AgyRuntimeFactory {
    let child = workspace.join("agy-fake.sh");
    write_scripted_child(&child, workspace, script);
    AgyRuntimeFactory::test_harness(Some(child))
}

fn await_terminal_task(scheduler: &Scheduler, agent_id: &str) -> TaskRecord {
    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    loop {
        let task = scheduler.store().get_task(agent_id).unwrap().unwrap();
        if task.phase == TaskPhase::Terminal {
            return task;
        }
        assert!(Instant::now() < deadline, "task never became terminal");
        thread::sleep(Duration::from_millis(10));
    }
}

fn await_result(scheduler: &Scheduler, agent_id: &str) -> external_store::StoredTaskResult {
    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    loop {
        if let Some(result) = scheduler.store().task_result(agent_id).unwrap() {
            return result;
        }
        assert!(Instant::now() < deadline, "result was never persisted");
        thread::sleep(Duration::from_millis(10));
    }
}

fn await_turn_active(scheduler: &Scheduler, agent_id: &str) {
    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    loop {
        let task = scheduler.store().get_task(agent_id).unwrap().unwrap();
        if matches!(task.turn_state, TurnState::Active) {
            return;
        }
        assert!(Instant::now() < deadline, "turn never became active");
        thread::sleep(Duration::from_millis(10));
    }
}

fn deliveries(workspace: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(workspace.join("deliveries.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn argv(workspace: &Path) -> Vec<String> {
    std::fs::read_to_string(workspace.join("argv.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn enqueue(scheduler: &Scheduler, workspace: &Path, permission: PermissionMode) -> String {
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for_mode(workspace, "inspect the repository", permission),
            Some(agy_admission(Some(MODEL))),
        )
        .unwrap();
    submitted.agent_id
}

/// Boundary 1: init -> user_input -> agent_response(ACTIVE/DONE) -> result.
const HAPPY: &str = r#"
read_frame
printf '%s\n' '{"event":"init","conversation_id":"__CONV__","init":{"cwd":".","tools":["run_command"],"permission_mode":"request-review"}}'
printf '%s\n' '{"event":"step_update","step_update":{"conversation_id":"__CONV__","step_index":0,"state":"DONE","step_type":"user_input"}}'
printf '%s\n' '{"event":"step_update","step_update":{"conversation_id":"__CONV__","step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"MANGO"}}'
printf '%s\n' '{"event":"step_update","step_update":{"conversation_id":"__CONV__","step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"\n"}}'
printf '%s\n' '{"event":"result","result":{"conversation_id":"__CONV__","status":"SUCCESS","response":"MANGO\n","duration_seconds":1.0,"num_turns":1}}'
"#;

/// Boundary 2: one ACTIVE/DONE tool step pair plus text.
const TOOL: &str = r#"
read_frame
printf '%s\n' '{"event":"init","conversation_id":"__CONV__","init":{"cwd":".","tools":["run_command"],"permission_mode":"request-review"}}'
printf '%s\n' '{"event":"step_update","step_update":{"conversation_id":"__CONV__","step_index":1,"state":"ACTIVE","step_type":"tool","tool_info":{"name":"run_command","parameters":{"CommandLine":"echo X"}}}}'
printf '%s\n' '{"event":"step_update","step_update":{"conversation_id":"__CONV__","step_index":1,"state":"DONE","step_type":"tool","tool_info":{"name":"run_command","output":"X\r\n"}}}'
printf '%s\n' '{"event":"step_update","step_update":{"conversation_id":"__CONV__","step_index":2,"state":"DONE","step_type":"agent_response","text_delta":"TOOL_OK"}}'
printf '%s\n' '{"event":"result","result":{"conversation_id":"__CONV__","status":"SUCCESS","response":"TOOL_OK","duration_seconds":1.0,"num_turns":1}}'
"#;

/// Multi-turn: the first result is released only after the test queues a
/// message, then the child consumes the follow-up user line.
const MULTI: &str = r#"
read_frame
printf '%s\n' '{"event":"init","conversation_id":"__CONV__","init":{"cwd":".","tools":["run_command"],"permission_mode":"request-review"}}'
while [ ! -f release ]; do sleep 0.01; done
printf '%s\n' '{"event":"result","result":{"conversation_id":"__CONV__","status":"SUCCESS","response":"APPLE\n","duration_seconds":1.0,"num_turns":1}}'
read_frame
printf '%s\n' '{"event":"result","result":{"conversation_id":"__CONV__","status":"SUCCESS","response":"PEAR\n","duration_seconds":1.0,"num_turns":2}}'
"#;

/// Boundary 3: a mid-turn signal yields a single `interrupted` ERROR result
/// carrying the truncated partial text, then exit 1.
const CANCEL: &str = r#"
read_frame
printf '%s\n' '{"event":"init","conversation_id":"__CONV__","init":{"cwd":".","tools":["run_command"],"permission_mode":"request-review"}}'
printf '%s\n' '{"event":"step_update","step_update":{"conversation_id":"__CONV__","step_index":0,"state":"DONE","step_type":"user_input"}}'
printf '%s\n' '{"event":"step_update","step_update":{"conversation_id":"__CONV__","step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"PARTIAL"}}'
trap 'printf "%s\n" "{\"event\":\"result\",\"result\":{\"conversation_id\":\"__CONV__\",\"status\":\"ERROR\",\"response\":\"PARTIAL\",\"error\":\"interrupted\",\"num_turns\":1}}"; exit 1' TERM
"#;

/// Boundary 4: a completed turn settles on its SUCCESS result; the idle-period
/// signal then flushes a second, stream-cancelled ERROR result that must not
/// override the settled terminal.
const IDLE_DUAL_RESULT: &str = r#"
read_frame
printf '%s\n' '{"event":"init","conversation_id":"__CONV__","init":{"cwd":".","tools":["run_command"],"permission_mode":"request-review"}}'
printf '%s\n' '{"event":"step_update","step_update":{"conversation_id":"__CONV__","step_index":0,"state":"DONE","step_type":"agent_response","text_delta":"MANGO\n"}}'
printf '%s\n' '{"event":"result","result":{"conversation_id":"__CONV__","status":"SUCCESS","response":"MANGO\n","duration_seconds":1.0,"num_turns":1}}'
trap 'printf "%s\n" "{\"event\":\"result\",\"result\":{\"conversation_id\":\"__CONV__\",\"status\":\"ERROR\",\"response\":\"\",\"error\":\"stream input cancelled: context canceled\",\"num_turns\":1}}"; exit 1' TERM
"#;

/// A malformed line and an unknown event name are transport noise; the session
/// must keep reading and still settle the following turn.
const NOISE: &str = r#"
read_frame
printf '%s\n' '{"event":"init","conversation_id":"__CONV__","init":{"cwd":".","tools":["run_command"],"permission_mode":"request-review"}}'
printf '%s\n' 'this is not json'
printf '%s\n' '{"event":"banana","payload":{"x":1}}'
printf '%s\n' '{"event":"step_update","step_update":{"conversation_id":"__CONV__","step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"OK"}}'
printf '%s\n' '{"event":"result","result":{"conversation_id":"__CONV__","status":"SUCCESS","response":"OK","duration_seconds":1.0,"num_turns":1}}'
"#;

/// Boundary 5 on the failure path: a failed turn's structured soft-deny
/// entries are appended to the bounded diagnostic tail.
const DENIED: &str = r#"
read_frame
printf '%s\n' '{"event":"init","conversation_id":"__CONV__","init":{"cwd":".","tools":["run_command"],"permission_mode":"request-review"}}'
printf '%s\n' '{"event":"step_update","step_update":{"conversation_id":"__CONV__","step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"PARTIAL"}}'
printf '%s\n' '{"event":"result","result":{"conversation_id":"__CONV__","status":"ERROR","response":"","error":"boom","denied_actions":[{"action":"command","display_name":"RunCommand"}],"num_turns":1}}'
"#;

#[test]
fn agy_admission_routes_to_the_agy_factory_and_completes_the_turn() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let directory = workspace.path().to_owned();
    let scheduler = agy_scheduler(
        &directory,
        harness_factory(&HAPPY.replace("__CONV__", CONV), &directory),
    );
    let agent_id = enqueue(&scheduler, &directory, PermissionMode::Build);
    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Completed);
    assert_eq!(result.result.final_text, "MANGO\n");
    let task = await_terminal_task(&scheduler, &agent_id);
    // The `init` conversation_id is the session identity.
    assert_eq!(task.session_id.as_deref(), Some(CONV));
    // The initial prompt left as one NDJSON user event.
    let frames = deliveries(&directory);
    let first = frames.first().expect("the child saw the initial user line");
    assert_eq!(first["event"], "user");
    assert_eq!(first["message"]["content"], "inspect the repository");
    // The pinned stream framing and admitted model are in the argv.
    let args = argv(&directory);
    assert!(args.contains(&"ARG--input-format".into()), "{args:?}");
    assert!(args.contains(&"ARGstream-json".into()), "{args:?}");
    assert!(args.contains(&"ARG--model".into()), "{args:?}");
    assert!(args.contains(&format!("ARG{MODEL}")), "{args:?}");
    assert!(args.contains(&"ARG--mode".into()), "{args:?}");
    assert!(args.contains(&"ARGaccept-edits".into()), "{args:?}");
    // The process group was stopped and reaped on the normal completion path.
    assert_eq!(scheduler.active_count(), 0);
}

/// A `native` admission (no explicit and no configured default model) is
/// spawnable: the launch omits `--model` and the `agy` CLI supplies its own
/// default model. This is the flip of the former missing-model refusal.
#[test]
fn native_admission_launches_without_a_model_flag() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let directory = workspace.path().to_owned();
    let scheduler = agy_scheduler(
        &directory,
        harness_factory(&HAPPY.replace("__CONV__", CONV), &directory),
    );
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(&directory, "inspect the repository"),
            Some(agy_admission(None)),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Completed);
    assert_eq!(result.result.final_text, "MANGO\n");
    // The native launch carries no --model flag (the CLI default applies), but
    // still pins the stream framing and the admitted build posture.
    let args = argv(&directory);
    assert!(!args.contains(&"ARG--model".into()), "{args:?}");
    assert!(args.contains(&"ARG--input-format".into()), "{args:?}");
    assert!(args.contains(&"ARGstream-json".into()), "{args:?}");
    assert!(args.contains(&"ARG--mode".into()), "{args:?}");
    assert!(args.contains(&"ARGaccept-edits".into()), "{args:?}");
}

#[test]
fn admitted_effort_is_threaded_into_the_child_argv() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let directory = workspace.path().to_owned();
    let scheduler = agy_scheduler(
        &directory,
        harness_factory(&HAPPY.replace("__CONV__", CONV), &directory),
    );
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for_mode(&directory, "run with effort", PermissionMode::Build),
            Some(agy_admission_with_effort(Some(MODEL), Some("high"))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    scheduler.start_ready().unwrap();
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Completed);
    let args = argv(&directory);
    assert!(args.contains(&"ARG--effort".into()), "{args:?}");
    assert!(args.contains(&"ARGhigh".into()), "{args:?}");
}

#[test]
fn yolo_permission_maps_to_the_skip_permissions_flag() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let directory = workspace.path().to_owned();
    let scheduler = agy_scheduler(
        &directory,
        harness_factory(&HAPPY.replace("__CONV__", CONV), &directory),
    );
    let agent_id = enqueue(&scheduler, &directory, PermissionMode::Yolo);
    scheduler.start_ready().unwrap();
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Completed);
    let args = argv(&directory);
    assert!(
        args.contains(&"ARG--dangerously-skip-permissions".into()),
        "{args:?}"
    );
    assert!(!args.contains(&"ARG--mode".into()), "{args:?}");
}

#[test]
fn tool_step_pair_feeds_the_wait_window_and_observe_capture() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let directory = workspace.path().to_owned();
    let scheduler = agy_scheduler(
        &directory,
        harness_factory(&TOOL.replace("__CONV__", CONV), &directory),
    );
    let agent_id = enqueue(&scheduler, &directory, PermissionMode::Build);
    scheduler.start_ready().unwrap();
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Completed);
    assert_eq!(result.result.final_text, "TOOL_OK");
    await_terminal_task(&scheduler, &agent_id);

    let activity = scheduler
        .passive_activity_snapshot(&agent_id)
        .expect("agy activity tracker is present");
    assert_eq!(activity.window_60s.tool_calls_started, 1);
    assert_eq!(activity.window_60s.tool_calls_completed, 1);
    assert_eq!(activity.window_60s.other_tool_calls, 1);

    // The started frame's tool name and parameters are captured by observe;
    // wait no longer carries per-call details. Reasoning stays uncollected.
    let observation = scheduler.observation_snapshot(&agent_id);
    assert_eq!(observation.tools.len(), 1);
    assert_eq!(observation.tools[0].tool_name, "run_command");
    assert_eq!(observation.tools[0].call_count, 1);
    assert_eq!(
        observation.tools[0].recent_calls[0].arguments,
        serde_json::json!({"CommandLine":"echo X"})
            .as_object()
            .unwrap()
            .clone()
    );
    assert!(!observation.tools[0].recent_calls[0].arguments_truncated);
    assert!(observation.reasoning.text.is_empty());
    assert!(observation.coverage.tool_history_complete);
    assert!(!observation.coverage.reasoning_complete);
}

#[test]
fn message_queue_delivers_a_second_turn_after_the_first_result() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let directory = workspace.path().to_owned();
    let scheduler = agy_scheduler(
        &directory,
        harness_factory(&MULTI.replace("__CONV__", CONV), &directory),
    );
    let agent_id = enqueue(&scheduler, &directory, PermissionMode::Build);
    scheduler.start_ready().unwrap();
    await_turn_active(&scheduler, &agent_id);
    assert_eq!(
        scheduler
            .send_message(&agent_id, "queued-1", "queue", "second question")
            .unwrap(),
        MessageDisposition::Delivered
    );
    std::fs::write(directory.join("release"), "").unwrap();
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Completed);
    assert_eq!(result.result.final_text, "PEAR\n");
    await_terminal_task(&scheduler, &agent_id);
    let frames = deliveries(&directory);
    let users: Vec<&serde_json::Value> = frames
        .iter()
        .filter(|frame| frame["event"] == "user")
        .collect();
    assert_eq!(users.len(), 2, "one user line per turn: {users:?}");
    assert_eq!(users[1]["message"]["content"], "second question");
}

#[test]
fn mid_turn_termination_settles_cancelled_with_the_partial_text() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let directory = workspace.path().to_owned();
    let scheduler = agy_scheduler(
        &directory,
        harness_factory(&CANCEL.replace("__CONV__", CONV), &directory),
    );
    let agent_id = enqueue(&scheduler, &directory, PermissionMode::Build);
    scheduler.start_ready().unwrap();
    await_turn_active(&scheduler, &agent_id);
    scheduler.cancel_task(&agent_id).unwrap();
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Cancelled));
    let result = scheduler.store().task_result(&agent_id).unwrap().unwrap();
    assert_eq!(result.result.outcome, TaskOutcome::Cancelled);
    assert!(result.result.partial);
    let activity = scheduler
        .passive_activity_snapshot(&agent_id)
        .expect("agy activity tracker is present");
    assert!(
        activity.latest_text_tail.contains("PARTIAL"),
        "partial text was not retained: {:?}",
        activity.latest_text_tail
    );
}

#[test]
fn idle_signal_result_does_not_override_the_completed_turn() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let directory = workspace.path().to_owned();
    let scheduler = agy_scheduler(
        &directory,
        harness_factory(&IDLE_DUAL_RESULT.replace("__CONV__", CONV), &directory),
    );
    let agent_id = enqueue(&scheduler, &directory, PermissionMode::Build);
    scheduler.start_ready().unwrap();
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Completed);
    assert_eq!(
        result.result.final_text, "MANGO\n",
        "the idle-period ERROR result must not overwrite the settled turn"
    );
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Completed));
}

#[test]
fn failed_turn_appends_denied_actions_to_the_diagnostic_tail() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let directory = workspace.path().to_owned();
    let scheduler = agy_scheduler(
        &directory,
        harness_factory(&DENIED.replace("__CONV__", CONV), &directory),
    );
    let agent_id = enqueue(&scheduler, &directory, PermissionMode::Build);
    scheduler.start_ready().unwrap();
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Failed));
    let failure = task.failure_message.unwrap_or_default();
    assert!(
        failure.contains("agy denied_actions: command (RunCommand)"),
        "denied actions missing from the diagnostic tail: {failure}"
    );
}

#[test]
fn malformed_lines_are_skipped_and_the_session_continues() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let directory = workspace.path().to_owned();
    let scheduler = agy_scheduler(
        &directory,
        harness_factory(&NOISE.replace("__CONV__", CONV), &directory),
    );
    let agent_id = enqueue(&scheduler, &directory, PermissionMode::Build);
    scheduler.start_ready().unwrap();
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Completed);
    assert_eq!(result.result.final_text, "OK");
}

#[test]
fn closed_gate_refuses_spawn_without_a_process() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let directory = workspace.path().to_owned();
    let scheduler = agy_scheduler(&directory, AgyRuntimeFactory::closed());
    let agent_id = enqueue(&scheduler, &directory, PermissionMode::Build);
    let error = scheduler.start_ready().unwrap_err();
    assert!(
        error.to_string().contains("agy spawn gate is closed"),
        "{error}"
    );
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Failed));
}

/// An unsupported permission mode never reaches a child: the factory refuses
/// at resolution time (daemon admission is expected to reject it upstream).
#[test]
fn plan_permission_is_refused_by_the_factory() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let directory = workspace.path().to_owned();
    let scheduler = agy_scheduler(
        &directory,
        harness_factory(&HAPPY.replace("__CONV__", CONV), &directory),
    );
    let agent_id = enqueue(&scheduler, &directory, PermissionMode::Plan);
    let error = scheduler.start_ready().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("build and yolo permission modes"),
        "{error}"
    );
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Failed));
}

struct NoopSink;
impl crate::LifecycleSink for NoopSink {
    fn emit(&self, _record: crate::RuntimeEnvelope) {}
}

fn agy_task_record(directory: &Path) -> TaskRecord {
    let canonical = directory.canonicalize().unwrap();
    let prepared = external_core::GeneralTaskPreparer::new(Vec::new())
        .unwrap()
        .prepare_direct_submission(&manifest_for(&canonical, "launch"))
        .unwrap()
        .with_admission(agy_admission(Some(MODEL)))
        .unwrap();
    TaskRecord {
        agent_id: "agy-launch-check".into(),
        repository: canonical.to_string_lossy().into_owned(),
        phase: TaskPhase::Queued,
        outcome: None,
        workspace_path: canonical.to_string_lossy().into_owned(),
        runtime_hash: None,
        prepared_launch_json: serde_json::to_string(&prepared).unwrap(),
        initial_prompt: "prompt".into(),
        owner_id: None,
        owner_epoch: 0,
        close_requested: false,
        stop_requested: false,
        last_event_seq: 0,
        failure_code: None,
        failure_message: None,
        runtime_agent_id: None,
        session_id: None,
        turn_state: TurnState::Idle,
        process_identity: None,
        closed_at: None,
        reaped_at: None,
        created_at: 0,
    }
}

#[test]
fn routing_dispatches_agy_admissions_to_the_agy_factory() {
    let workspace = agy_workspace();
    let directory = workspace.path().to_owned();
    let zcode = CommandRuntimeFactory::new(|_: &TaskRecord| {
        Err::<Command, _>(io::Error::other("zcode factory must not spawn"))
    });
    let factory: Arc<dyn RuntimeFactory> = Arc::new(crate::dsh::RoutingRuntimeFactory::with_agy(
        zcode,
        crate::dsh::DshRuntimeFactory::closed(),
        crate::codex::CodexRuntimeFactory::closed(),
        AgyRuntimeFactory::closed(),
    ));
    let sink: Arc<dyn LifecycleSink> = Arc::new(NoopSink);
    // An agy admission routes to the agy factory (closed gate).
    let error = factory
        .spawn(&agy_task_record(&directory), sink)
        .err()
        .expect("agy admission must route to the agy factory");
    assert!(
        error.to_string().contains("agy spawn gate is closed"),
        "{error}"
    );
}

#[test]
fn native_buffer_keeps_runtime_running_between_results_and_rejects_steer() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let script = r#"
read_frame
printf '%s\n' '{"event":"init","conversation_id":"__CONV__","init":{}}'
read_frame
printf '%s\n' '{"event":"result","result":{"conversation_id":"__CONV__","status":"SUCCESS","response":"FIRST","num_turns":1}}'
touch first-settled
while [ ! -f consume-native ]; do sleep 0.01; done
printf '%s\n' '{"event":"step_update","step_update":{"conversation_id":"__CONV__","step_index":1,"state":"DONE","step_type":"user_input"}}'
printf '%s\n' '{"event":"result","result":{"conversation_id":"__CONV__","status":"SUCCESS","response":"SECOND","num_turns":2}}'
"#.replace("__CONV__", CONV);
    let scheduler = agy_scheduler(workspace.path(), harness_factory(&script, workspace.path()));
    let agent_id = enqueue(&scheduler, workspace.path(), PermissionMode::Build);
    scheduler.start_ready().unwrap();
    let error = scheduler
        .send_message(&agent_id, "unsupported", "steer", "turn now")
        .unwrap_err();
    assert!(
        matches!(error, crate::SchedulerError::RuntimeCommand { message, .. } if message == "steer_unsupported")
    );
    assert!(scheduler.store().message("unsupported").unwrap().is_none());
    assert_eq!(
        scheduler
            .send_message(&agent_id, "native", "queue", "second question")
            .unwrap(),
        MessageDisposition::Delivered
    );
    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    while !workspace.path().join("first-settled").exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    // 留出多个 monitor 周期，证明首轮 result 后 runtime 仍被保留。
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        scheduler
            .store()
            .get_task(&agent_id)
            .unwrap()
            .unwrap()
            .phase,
        TaskPhase::Running
    );
    assert_eq!(scheduler.active_count(), 1);
    assert!(scheduler.store().task_result(&agent_id).unwrap().is_none());
    assert_eq!(
        scheduler.store().message("native").unwrap().unwrap().state,
        external_store::MessageState::Delivered
    );
    std::fs::write(workspace.path().join("consume-native"), "").unwrap();
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.final_text, "SECOND");
    assert_eq!(result.result.outcome, TaskOutcome::Completed);
    assert_eq!(
        deliveries(workspace.path())
            .iter()
            .filter(|f| f["event"] == "user")
            .count(),
        2
    );
}

#[test]
fn publisher_latched_snapshot_does_not_wait_for_native_input_write() {
    use super::events::AgyRuntimeShared;
    use std::sync::mpsc;

    let shared = Arc::new(AgyRuntimeShared::new(
        Arc::new(crate::Publisher::new(Arc::new(NoopSink)))));
    let (writing, write_started) = mpsc::channel();
    let (release_write, write_released) = mpsc::channel();
    let writer_shared = Arc::clone(&shared);
    let writer = thread::spawn(move || {
        writer_shared.buffer_input(|| {
            // 受控阻塞写持有 turn 锁，登记已完成且失败撤回尚未发生。
            writing.send(()).unwrap();
            write_released.recv().unwrap();
            Err(crate::RuntimeCommandError::Transport(
                "write refused".into(),
            ))
        })
    });
    write_started.recv_timeout(SCRIPTED_SYNC_WAIT).unwrap();
    let (snapshot_sent, snapshot_ready) = mpsc::channel();
    let monitor_shared = Arc::clone(&shared);
    let monitor = thread::spawn(move || {
        // 与 fault 决策相同：持 publisher latch 读取 snapshot。
        // 此时 turn 锁确定由 writer 持有，旧实现必然阻塞。
        let _latch = monitor_shared.publisher.decision_latch();
        snapshot_sent.send(monitor_shared.turn_snapshot()).unwrap();
    });
    let snapshot = snapshot_ready.recv_timeout(Duration::from_secs(2));
    // 先解除受控写再断言，旧实现失败时也能回收两个线程。
    release_write.send(()).unwrap();
    assert!(writer.join().unwrap().is_err());
    monitor.join().unwrap();
    assert!(
        snapshot
            .expect("publisher-latched snapshot waited for turn lock")
            .active
    );
    assert!(
        !shared.turn_snapshot().active,
        "failed write was not withdrawn"
    );
}

#[test]
fn result_publication_keeps_snapshot_nonblocking_and_pending_input_active() {
    use super::events::AgyRuntimeShared;
    use std::sync::{mpsc, Mutex};

    struct PausedSettlementSink {
        publishing: mpsc::Sender<()>,
        release: Mutex<Option<mpsc::Receiver<()>>>,
    }
    impl LifecycleSink for PausedSettlementSink {
        fn emit(&self, record: crate::RuntimeEnvelope) {
            let crate::RuntimeEnvelope::Event { event: event, .. } = record else {
                return;
            };
            if matches!(
                event.fact,
                external_contract::events::EventFact::TurnSettled(_)
            ) {
                if let Some(release) = self.release.lock().unwrap().take() {
                    // 真实 result 发布正持 turn 与 publisher 锁；暂停首轮边界。
                    self.publishing.send(()).unwrap();
                    release.recv().unwrap();
                }
            }
        }
    }

    let (publishing, publication_started) = mpsc::channel();
    let (release, publication_released) = mpsc::channel();
    let shared = Arc::new(AgyRuntimeShared::new(
        Arc::new(crate::Publisher::new(Arc::new(PausedSettlementSink {
            publishing,
            release: Mutex::new(Some(publication_released)),
        })))));
    shared.begin_turn();
    shared.buffer_input(|| Ok(())).unwrap();
    let result = |count| {
        external_agent_agy::event::parse_line(&format!(r#"{{"event":"result","result":{{"conversation_id":"{CONV}","status":"SUCCESS","response":"turn {count}","num_turns":{count}}}}}"#)).unwrap()
    };
    let first_result = result(1);
    let pump_shared = Arc::clone(&shared);
    let pump = thread::spawn(move || pump_shared.project_event(&first_result));
    publication_started
        .recv_timeout(SCRIPTED_SYNC_WAIT)
        .unwrap();
    let (snapshot_sent, snapshot_ready) = mpsc::channel();
    let snapshot_shared = Arc::clone(&shared);
    let reader = thread::spawn(move || {
        snapshot_sent.send(snapshot_shared.turn_snapshot()).unwrap();
    });
    let snapshot = snapshot_ready.recv_timeout(Duration::from_secs(2));
    // 先释放真实发布路径，确保旧实现超时失败也不会遗留阻塞线程。
    release.send(()).unwrap();
    pump.join().unwrap();
    reader.join().unwrap();
    assert!(
        snapshot
            .expect("snapshot waited for result publication's turn lock")
            .active
    );
    let first = shared.turn_snapshot();
    assert!(
        first.active,
        "pending native input lost its completion protection"
    );
    assert_eq!(first.boundary, Some(crate::TurnBoundary::Completed));
    shared.project_event(&result(2));
    let second = shared.turn_snapshot();
    assert!(!second.active);
    assert_eq!(second.generation, first.generation + 1);
    assert_eq!(second.boundary, Some(crate::TurnBoundary::Completed));
}

#[test]
fn pending_native_input_starts_on_result_and_idle_duplicates_stay_noise() {
    use super::events::AgyRuntimeShared;
    let publisher = Arc::new(crate::Publisher::new(Arc::new(NoopSink)));
    let tracker = Arc::clone(&publisher.runtime_state);
    let shared = AgyRuntimeShared::new(publisher);
    shared.begin_turn();
    shared.buffer_input(|| Ok(())).unwrap();
    let result = |count| {
        external_agent_agy::event::parse_line(&format!(r#"{{"event":"result","result":{{"conversation_id":"{CONV}","status":"SUCCESS","response":"turn {count}","num_turns":{count}}}}}"#)).unwrap()
    };
    shared.project_event(&result(1));
    assert!(shared.turn_snapshot().active);
    let first = tracker.turn_snapshot();
    assert_eq!(first.boundary, Some(crate::TurnBoundary::Completed));
    shared.project_event(&result(1));
    assert_eq!(tracker.turn_snapshot(), first);
    shared.project_event(&result(2));
    let second = shared.turn_snapshot();
    assert!(!second.active);
    assert_eq!(second.generation, first.generation + 1);
    assert_eq!(second.boundary, Some(crate::TurnBoundary::Completed));
    shared.project_event(&result(2));
    assert_eq!(shared.turn_snapshot(), second);
    assert!(shared
        .buffer_input(|| Err(crate::RuntimeCommandError::Transport(
            "write refused".into()
        )))
        .is_err());
    assert_eq!(shared.turn_snapshot(), second);
}

/// One scripted child serves both invocations: without `--conversation` it
/// settles a first turn; with it, it echoes the same conversation on `init`
/// and settles a second turn after exactly one follow-up user line.
const RESUME_SCRIPT: &str = r#"
case " $* " in
  *" --conversation "*)
    printf '%s\n' '{"event":"init","conversation_id":"__CONV__","init":{}}'
    read_frame
    printf '%s\n' '{"event":"result","result":{"conversation_id":"__CONV__","status":"SUCCESS","response":"RESUMED_OK","num_turns":2}}'
    ;;
  *)
    printf '%s\n' '{"event":"init","conversation_id":"__CONV__","init":{}}'
    read_frame
    printf '%s\n' '{"event":"result","result":{"conversation_id":"__CONV__","status":"SUCCESS","response":"FIRST_OK","num_turns":1}}'
    ;;
esac
"#;

#[test]
fn terminal_send_resumes_the_conversation_and_delivers_the_follow_up_only() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let script = RESUME_SCRIPT.replace("__CONV__", CONV);
    let scheduler = agy_scheduler(workspace.path(), harness_factory(&script, workspace.path()));
    let agent_id = enqueue(&scheduler, workspace.path(), PermissionMode::Build);
    scheduler.start_ready().unwrap();
    let first = await_result(&scheduler, &agent_id);
    assert_eq!(first.result.final_text, "FIRST_OK");
    assert_eq!(first.result.outcome, TaskOutcome::Completed);
    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    while scheduler.active_count() > 0 {
        assert!(
            Instant::now() < deadline,
            "first runtime was never released"
        );
        thread::sleep(Duration::from_millis(10));
    }

    // The terminal task carries the persisted conversation id, so the public
    // send requeues it for resume instead of rejecting.
    assert_eq!(
        scheduler
            .send_message(&agent_id, "resume-msg", "queue", "follow-up question")
            .unwrap(),
        MessageDisposition::Queued
    );
    scheduler.start_ready().unwrap();
    let resumed = await_result(&scheduler, &agent_id);
    assert_eq!(resumed.result.final_text, "RESUMED_OK");
    assert_eq!(resumed.result.outcome, TaskOutcome::Completed);
    let message = scheduler.store().message("resume-msg").unwrap().unwrap();
    assert_eq!(message.state, external_store::MessageState::Delivered);
    assert_eq!(message.mode, "queue");

    // The factory threaded the persisted session id as --conversation: the
    // flag appears exactly once, in the resumed invocation only.
    let argv = argv(workspace.path());
    let flag_positions: Vec<usize> = argv
        .iter()
        .enumerate()
        .filter_map(|(index, argument)| (argument == "ARG--conversation").then_some(index))
        .collect();
    assert_eq!(flag_positions.len(), 1, "argv: {argv:?}");
    assert_eq!(argv[flag_positions[0] + 1], format!("ARG{CONV}"));

    // The resumed conversation never replays the initial prompt: the second
    // process receives exactly the follow-up as its sole user line.
    let users: Vec<String> = deliveries(workspace.path())
        .iter()
        .filter(|frame| frame["event"] == "user")
        .map(|frame| frame["message"]["content"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(users, ["inspect the repository", "follow-up question"]);
}

#[test]
fn terminal_send_resume_fails_closed_when_the_conversation_echo_diverges() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let other = "00000000-0000-4000-8000-000000000000";
    // Same shape as RESUME_SCRIPT, but the resumed branch echoes a different
    // conversation id on `init`: the resume must fail closed without ever
    // continuing the diverging conversation.
    let script = format!(
        r#"
case " $* " in
  *" --conversation "*)
    printf '%s\n' '{{"event":"init","conversation_id":"{other}","init":{{}}}}'
    ;;
  *)
    printf '%s\n' '{{"event":"init","conversation_id":"{CONV}","init":{{}}}}'
    read_frame
    printf '%s\n' '{{"event":"result","result":{{"conversation_id":"{CONV}","status":"SUCCESS","response":"FIRST_OK","num_turns":1}}}}'
    ;;
esac
"#
    );
    let scheduler = agy_scheduler(workspace.path(), harness_factory(&script, workspace.path()));
    let agent_id = enqueue(&scheduler, workspace.path(), PermissionMode::Build);
    scheduler.start_ready().unwrap();
    let first = await_result(&scheduler, &agent_id);
    assert_eq!(first.result.final_text, "FIRST_OK");

    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    while scheduler.active_count() > 0 {
        assert!(
            Instant::now() < deadline,
            "first runtime was never released"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        scheduler
            .send_message(&agent_id, "resume-mismatch", "queue", "follow-up question")
            .unwrap(),
        MessageDisposition::Queued
    );
    // The failing resume surfaces through start_ready and terminalizes the
    // task with SESSION_START_FAILED.
    let error = scheduler.start_ready().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not match the persisted session id"),
        "{error}"
    );
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.phase, TaskPhase::Terminal);
    assert_eq!(task.outcome, Some(TaskOutcome::Failed));
    assert_eq!(task.failure_code.as_deref(), Some("SESSION_START_FAILED"));
    let message = scheduler
        .store()
        .message("resume-mismatch")
        .unwrap()
        .unwrap();
    assert_eq!(message.state, external_store::MessageState::Failed);
    // The diverging conversation was never continued: no follow-up user line
    // reached any child after the first turn.
    let users = deliveries(workspace.path())
        .iter()
        .filter(|frame| frame["event"] == "user")
        .count();
    assert_eq!(users, 1);
}

/// Typed ingress probes use observed fixtures where available; fault frames
/// below are explicitly synthetic and never require a live model call.
struct AgyFactSink(
    std::sync::Mutex<Vec<crate::RuntimeEnvelope>>,
    Arc<crate::RuntimeState>,
);
impl Default for AgyFactSink {
    fn default() -> Self {
        Self(
            Default::default(),
            Arc::new(crate::RuntimeState::new()),
        )
    }
}
impl LifecycleSink for AgyFactSink {
    fn runtime_state(&self) -> Option<Arc<crate::RuntimeState>> { Some(self.1.clone()) }
    fn emit(&self, record: crate::RuntimeEnvelope) {
        self.0.lock().unwrap().push(record);
    }
}
impl AgyFactSink {
    fn facts(&self) -> Vec<external_contract::events::NormalizedEvent> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                crate::RuntimeEnvelope::Event { event: event, .. } => Some(event.clone()),
                _ => None,
            })
            .collect()
    }
}
fn agy_fact_probe() -> (super::events::AgyRuntimeShared, Arc<AgyFactSink>) {
    let sink = Arc::new(AgyFactSink::default());
    let shared = super::events::AgyRuntimeShared::new(
        Arc::new(crate::Publisher::new(sink.clone())));
    shared.begin_turn();
    (shared, sink)
}
fn feed_agy(shared: &super::events::AgyRuntimeShared, raw: serde_json::Value) {
    assert!(super::events::project_agy_inbound(
        shared,
        &external_runtime::Inbound::Message(external_contract::WireMessage::UnknownEvent {
            method: "ndjson".into(),
            raw
        })
    )
    .is_none());
}

#[test]
fn observed_stream_emits_typed_facts_without_inventing_tool_success() {
    use external_contract::events::*;
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/agy-events/stream-observed.json"
    ))
    .unwrap();
    let (shared, sink) = agy_fact_probe();
    for (index, frame) in fixture["frames"].as_array().unwrap().iter().enumerate() {
        feed_agy(&shared, frame.clone());
        if index == 3 {
            assert_eq!(sink.1.snapshot().active_tools.len(), 1);
        }
        if index == 4 {
            assert_eq!(sink.1.snapshot().active_tools.len(), 0);
        }
    }
    let facts = sink.facts();
    assert!(facts.iter().any(|e| matches!(&e.fact, EventFact::SessionReady(info) if info.model.as_deref() == Some("gemini-3.8-flash-low"))));
    let tools: Vec<_> = facts
        .iter()
        .filter_map(|e| {
            if let EventFact::ToolChanged(t) = &e.fact {
                Some(t)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].call_id, tools[1].call_id);
    assert_eq!(tools[1].phase, Some(ToolPhase::Finished));
    assert_eq!(tools[1].outcome, Some(ToolOutcome::Unknown));
    assert!(facts.iter().all(|e| e.context.source.turn_id.is_none()));
    assert!(facts
        .iter()
        .all(|e| e.context.turn_id == Some(TurnId::Local(1))));
    assert!(facts
        .windows(2)
        .all(|w| w[0].context.sequence < w[1].context.sequence));
    assert!(
        matches!(&facts.last().unwrap().fact, EventFact::TurnSettled(s) if s.final_text.as_deref() == Some("OK\n") && s.outcome == TurnOutcome::Completed)
    );
    assert!(sink.0.lock().unwrap().iter().all(|e| e.normalized().is_some()));
}

#[test]
fn synthetic_tool_error_denial_unknown_and_malformed_are_retained() {
    use external_contract::events::*;
    let (shared, sink) = agy_fact_probe();
    let error = serde_json::json!({"type":"command_error","message":"failed", "data":{"complete":"x".repeat(2000),"api_key":"secret"}});
    feed_agy(
        &shared,
        serde_json::json!({"event":"step_update","step_update":{"conversation_id":CONV,"step_index":7,"state":"DONE","step_type":"tool","tool_info":{"name":"run_command","error":error}}}),
    );
    assert_eq!(sink.1.snapshot().window_60s.tool_calls_failed, 1);
    assert_eq!(sink.1.snapshot().active_tools.len(), 0);
    let facts = sink.facts();
    assert!(facts.iter().any(|e| matches!(&e.fact, EventFact::ToolChanged(t) if t.phase == Some(ToolPhase::Finished) && t.outcome == Some(ToolOutcome::Failed) && t.failure.as_ref().is_some_and(|f| f.original["data"]["complete"] == "x".repeat(2000) && f.original["data"]["api_key"] != "secret"))));
    feed_agy(
        &shared,
        serde_json::json!({"event":"banana","payload":{"secret":"private-body"}}),
    );
    feed_agy(
        &shared,
        serde_json::json!({"event":"step_update","step_update":{"step_index":"bad"}}),
    );
    feed_agy(
        &shared,
        serde_json::json!({"event":"result","result":{"conversation_id":CONV,"status":"SUCCESS","response":"ok","num_turns":1,"denied_actions":[{"action":"command","display_name":"RunCommand"}]}}),
    );
    let facts = sink.facts();
    assert!(facts.iter().any(|e| matches!(&e.fact, EventFact::Diagnostic(d) if d.reason == DiagnosticReason::UnknownSemantics)));
    assert!(facts.iter().any(
        |e| matches!(&e.fact, EventFact::CoverageGap(g) if g.reason == LossReason::Malformed)
    ));
    assert!(facts.iter().any(|e| matches!(&e.fact, EventFact::Diagnostic(d) if d.reason == DiagnosticReason::PermissionDenied)));
    assert!(shared.diagnostic_tail().contains("RunCommand"));
    assert!(
        matches!(&facts.last().unwrap().fact, EventFact::TurnSettled(s) if s.outcome == TurnOutcome::Completed)
    );
    let diagnostics = facts
        .iter()
        .filter(|e| {
            matches!(
                &e.fact,
                EventFact::Diagnostic(_) | EventFact::CoverageGap(_)
            )
        })
        .collect::<Vec<_>>();
    assert!(!serde_json::to_string(&diagnostics)
        .unwrap()
        .contains("private-body"));
}

#[test]
fn observed_result_error_is_preserved_and_acp_remains_unsupported() {
    use external_contract::events::*;
    let (shared, sink) = agy_fact_probe();
    // OBSERVED 1.2.12 rejected control_request, retained by the existing corpus.
    let raw: serde_json::Value = serde_json::from_str(
        include_str!("../../../subagents/agy/tests/fixtures/neg-control.ndjson")
            .lines()
            .nth(1)
            .unwrap(),
    )
    .unwrap();
    let expected = raw["result"]["error"].clone();
    feed_agy(&shared, raw);
    assert!(sink.facts().iter().any(|e| matches!(&e.fact, EventFact::TurnSettled(s) if s.failure.as_ref().is_some_and(|f| f.original == expected))));
    let acp: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/agy-events/acp-observed.json"
    ))
    .unwrap();
    assert_eq!(acp["provenance"], "OBSERVED");
    let before = sink.facts().len();
    // The other observed protocol is evidence only. Sending one of its
    // actual frames to stream-json must diagnose unknown, never settle.
    feed_agy(&shared, acp["frames"].as_array().unwrap()[0].clone());
    assert!(
        matches!(&sink.facts()[before].fact, EventFact::Diagnostic(d) if d.reason == DiagnosticReason::UnknownSemantics)
    );
}

#[test]
fn synthetic_admission_error_retains_complete_redacted_native_failure() {
    let shared = super::events::AgyRuntimeShared::new(
        Arc::new(crate::Publisher::new(Arc::new(NoopSink))));
    let reason = format!(
        "invalid model selection {} api_key=secret-value",
        "detail ".repeat(500)
    );
    feed_agy(
        &shared,
        serde_json::json!({"event":"result","result":{"status":"ERROR","error":reason}}),
    );
    let error = shared.wait_start(Duration::from_secs(1)).unwrap_err();
    let crate::RuntimeCommandError::ModelRejected(detail) = error else {
        panic!("model rejection classification lost")
    };
    assert!(detail.message.len() <= 512);
    let original = detail.native_failure.unwrap().original;
    let original = original.as_str().unwrap();
    assert!(original.len() > 3000);
    assert!(!original.contains("secret-value"));
    assert!(original.contains(&"detail ".repeat(500)));
}

#[test]
fn buffered_start_is_published_before_pending_input_is_consumed() {
    use external_contract::events::*;
    use std::sync::{mpsc, Mutex};
    struct PausedStartSink {
        started: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl LifecycleSink for PausedStartSink {
        fn emit(&self, record: crate::RuntimeEnvelope) {
            if matches!(
                record,
                crate::RuntimeEnvelope::Event { event: NormalizedEvent {
                    fact: EventFact::TurnStarted {
                        turn_id: TurnId::Local(2)
                    },
                    ..
                }, .. }
            ) {
                self.started.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
            }
        }
    }
    let (started, ready) = mpsc::channel();
    let (release, resumed) = mpsc::channel();
    let shared = Arc::new(super::events::AgyRuntimeShared::new(
        Arc::new(crate::Publisher::new(Arc::new(PausedStartSink {
            started,
            release: Mutex::new(resumed),
        })))));
    shared.begin_turn();
    shared.buffer_input(|| Ok(())).unwrap();
    feed_agy(
        &shared,
        serde_json::json!({"event":"result","result":{"conversation_id":CONV,"status":"SUCCESS","response":"A","num_turns":1}}),
    );
    let pump_shared = shared.clone();
    let pump = thread::spawn(move || {
        feed_agy(
            &pump_shared,
            serde_json::json!({"event":"step_update","step_update":{"conversation_id":CONV,"step_index":42,"state":"DONE","step_type":"user_input"}}),
        )
    });
    ready.recv_timeout(SCRIPTED_SYNC_WAIT).unwrap();
    let snapshot = shared.turn_snapshot();
    release.send(()).unwrap();
    pump.join().unwrap();
    assert!(
        snapshot.active,
        "pending input was consumed before started reached the tracker"
    );
    assert_eq!(
        snapshot.generation, 2,
        "the shared reducer applies started before sink callbacks"
    );
    assert_eq!(shared.turn_snapshot().generation, 2);
    assert!(shared.turn_snapshot().active);
    feed_agy(
        &shared,
        serde_json::json!({"event":"result","result":{"conversation_id":CONV,"status":"SUCCESS","response":"B","num_turns":2}}),
    );
    assert!(!shared.turn_snapshot().active);
}

#[test]
fn native_init_model_reaches_the_owner_model_view_without_changing_admission() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let script = r#"
read_frame
printf '%s\n' '{"event":"init","conversation_id":"__CONV__","init":{"model":"native-effective-model"}}'
"#.replace("__CONV__", CONV);
    let child = workspace.path().join("model-view.sh");
    write_scripted_child(&child, workspace.path(), &script);
    let sink = Arc::new(AgyFactSink::default());
    let owner = AgyRuntimeOwner::spawn(Command::new(child), sink.clone()).unwrap();
    let ready = owner
        .bootstrap_session_with_mcp(&agy_task_record(workspace.path()), &[], SCRIPTED_SYNC_WAIT)
        .unwrap();
    assert_eq!(ready.configured_model.as_deref(), Some(MODEL));
    assert!(sink.facts().iter().any(|event| matches!(
        &event.fact,
        external_contract::events::EventFact::SessionReady(info)
            if info.model.as_deref() == Some("native-effective-model")
    )));
}

#[test]
fn synthetic_long_result_error_survives_the_real_scheduler_result_path() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    let error = format!(
        "invalid control_request {} END_OF_NATIVE_ERROR",
        "detail ".repeat(2000)
    );
    let result = serde_json::json!({"event":"result","result":{"conversation_id":CONV,"status":"ERROR","error":error,"num_turns":1}});
    let script = format!("read_frame\nprintf '%s\\n' '{{\"event\":\"init\",\"conversation_id\":\"{CONV}\",\"init\":{{}}}}'\nprintf '%s\\n' '{result}'\n");
    let scheduler = agy_scheduler(workspace.path(), harness_factory(&script, workspace.path()));
    let agent_id = enqueue(&scheduler, workspace.path(), PermissionMode::Build);
    scheduler.start_ready().unwrap();
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Failed);
    let detail: serde_json::Value = serde_json::from_str(&result.result.final_text).unwrap();
    // Existing result contract delivers the original itself; the wrapper
    // named original_error belongs to the intermediate failure detail only.
    assert_eq!(detail, serde_json::Value::String(error));
    let service = crate::rpc::RpcService::new(scheduler.clone(), scheduler.store()).unwrap();
    let mut offset = 0;
    let mut reconstructed = String::new();
    loop {
        let response = service
            .dispatch(crate::rpc::RpcMethod::TaskResult {
                agent_id: agent_id.clone(),
                offset,
                limit: 257,
            })
            .unwrap();
        let crate::rpc::RpcSuccess::TaskResult {
            result: Some(page), ..
        } = response
        else {
            panic!("result page")
        };
        reconstructed.push_str(&page.final_text);
        if let Some(next) = page.next_offset {
            assert!(next > offset);
            offset = next;
        } else {
            break;
        }
    }
    assert_eq!(reconstructed, result.result.final_text);
}

#[test]
fn interruption_only_observed_response_matches_wait_tail_before_boundary() {
    use external_contract::events::*;
    let raw: serde_json::Value = serde_json::from_str(
        include_str!("../../../subagents/agy/tests/fixtures/interrupted.ndjson")
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    let expected = raw["result"]["response"].as_str().unwrap();
    let (shared, sink) = agy_fact_probe();
    feed_agy(&shared, raw.clone());
    assert_eq!(sink.1.take_wait_tail().text, expected);
    assert!(
        sink.1.take_wait_tail().text.is_empty(),
        "wait cursor must consume the appended response once"
    );
    let facts = sink.facts();
    let text = facts.iter().position(|e| matches!(&e.fact, EventFact::MessageUpdated(m) if matches!(&m.text, TextUpdate::Delta(t) | TextUpdate::Snapshot(t) if t == expected))).expect("partial response text fact");
    let boundary = facts.iter().position(|e| matches!(&e.fact, EventFact::TurnSettled(s) if s.outcome == TurnOutcome::Cancelled && s.final_text.as_deref() == Some(expected))).expect("original cancellation settlement");
    assert!(
        text < boundary,
        "text must be published before the settling boundary"
    );
    assert_eq!(facts[text].context.turn_id, facts[boundary].context.turn_id);
    assert_eq!(
        shared.turn_snapshot().boundary,
        Some(crate::TurnBoundary::Failed),
        "typed cancellation must not authorize legacy successful completion"
    );
}

#[test]
fn interruption_only_response_reaches_wait_and_preserves_cancellation_result() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    const PARTIAL: &str = "ONLY_IN_INTERRUPTED_RESPONSE_结束";
    // SYNTHETIC signal harness using the observed interrupted-result shape.
    // Install the trap before init opens the bootstrap gate; no response delta
    // is emitted before cancellation, so only the result can supply the text.
    let script = r#"
read_frame
trap 'printf "%s\n" "{\"event\":\"result\",\"result\":{\"conversation_id\":\"__CONV__\",\"status\":\"ERROR\",\"response\":\"ONLY_IN_INTERRUPTED_RESPONSE_结束\",\"error\":\"interrupted\",\"num_turns\":1}}"; exit 1' TERM
printf '%s\n' '{"event":"init","conversation_id":"__CONV__","init":{}}'
printf '%s\n' '{"event":"step_update","step_update":{"conversation_id":"__CONV__","step_index":0,"state":"DONE","step_type":"user_input"}}'
"#.replace("__CONV__", CONV);
    let scheduler = agy_scheduler(workspace.path(), harness_factory(&script, workspace.path()));
    let agent_id = enqueue(&scheduler, workspace.path(), PermissionMode::Build);
    scheduler.start_ready().unwrap();
    await_turn_active(&scheduler, &agent_id);
    assert!(scheduler
        .passive_activity_snapshot(&agent_id)
        .unwrap()
        .latest_text_tail
        .is_empty());
    scheduler.cancel_task(&agent_id).unwrap();
    assert_eq!(
        await_terminal_task(&scheduler, &agent_id).outcome,
        Some(TaskOutcome::Cancelled)
    );
    let stored = await_result(&scheduler, &agent_id);
    assert_eq!(stored.result.outcome, TaskOutcome::Cancelled);
    assert!(stored.result.partial);
    assert!(
        scheduler.last_error(&agent_id).is_none(),
        "native stop must reach its boundary: {:?}",
        scheduler.last_error(&agent_id)
    );
    let service = crate::rpc::RpcService::new(scheduler.clone(), scheduler.store()).unwrap();
    let query = || {
        crate::rpc::RpcMethod::TaskWait(crate::rpc::TaskWaitQuery {
            wait_mode: crate::rpc::WaitMode::All,
            agent_id: crate::rpc::WaitTarget::One(agent_id.clone()),
            wait_time: 0,
            message_id: None,
        })
    };
    let crate::rpc::RpcSuccess::TaskWait(wait) = service.dispatch(query()).unwrap() else {
        panic!("wait response")
    };
    assert_eq!(wait.activity.latest_text_tail, PARTIAL);
    assert_eq!(
        wait.result.as_ref().unwrap().outcome,
        TaskOutcome::Cancelled
    );
    assert!(wait.result.as_ref().unwrap().partial);
    let crate::rpc::RpcSuccess::TaskWait(second) = service.dispatch(query()).unwrap() else {
        panic!("wait response")
    };
    assert!(second.activity.latest_text_tail.is_empty());
    // Cancellation's existing result format remains independent of its live
    // text tail; result pagination must still reconstruct that stored result.
    let mut offset = 0;
    let mut reconstructed = String::new();
    loop {
        let response = service
            .dispatch(crate::rpc::RpcMethod::TaskResult {
                agent_id: agent_id.clone(),
                offset,
                limit: 7,
            })
            .unwrap();
        let crate::rpc::RpcSuccess::TaskResult {
            result: Some(page), ..
        } = response
        else {
            panic!("result page")
        };
        assert_eq!(page.outcome, TaskOutcome::Cancelled);
        reconstructed.push_str(&page.final_text);
        if let Some(next) = page.next_offset {
            assert!(next > offset);
            offset = next;
        } else {
            break;
        }
    }
    assert_eq!(reconstructed, stored.result.final_text);
    assert_eq!(wait.result.unwrap().final_text, reconstructed);
}

#[test]
fn native_init_model_identity_does_not_reject_the_admitted_scheduler_task() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    // SYNTHETIC valid init whose observed model differs from the admission.
    // Hold settlement until after the real fresh-session model gate has run.
    let script = r#"
read_frame
printf '%s\n' '{"event":"init","conversation_id":"__CONV__","init":{"model":"native-effective-model"}}'
while [ ! -f release-model-response ]; do sleep 0.01; done
printf '%s\n' '{"event":"result","result":{"conversation_id":"__CONV__","status":"SUCCESS","response":"MODEL_IDENTITY_PRESERVED","num_turns":1}}'
"#.replace("__CONV__", CONV);
    struct CapturedFactory {
        inner: AgyRuntimeFactory,
        facts: Arc<AgyFactSink>,
    }
    struct CapturedSink {
        delegate: Arc<dyn LifecycleSink>,
        facts: Arc<AgyFactSink>,
    }
    impl LifecycleSink for CapturedSink {
        fn runtime_state(&self) -> Option<Arc<crate::RuntimeState>> { self.delegate.runtime_state() }
        fn event_scope(
            &self,
        ) -> Option<(
            external_contract::events::TaskId,
            external_contract::events::LaunchId,
        )> {
            self.delegate.event_scope()
        }
        fn emit(&self, record: crate::RuntimeEnvelope) {
            self.facts.emit(record.clone());
            self.delegate.emit(record);
        }
        fn emit_batch(&self, events: &[crate::RuntimeEnvelope]) {
            for event in events {
                self.facts.0.lock().unwrap().push(event.clone());
            }
            self.delegate.emit_batch(events);
        }
    }
    impl RuntimeFactory for CapturedFactory {
        fn spawn(
            &self,
            task: &TaskRecord,
            sink: Arc<dyn LifecycleSink>,
        ) -> io::Result<Arc<dyn ManagedRuntime>> {
            self.inner.spawn(
                task,
                Arc::new(CapturedSink {
                    delegate: sink,
                    facts: self.facts.clone(),
                }),
            )
        }
    }
    let facts = Arc::new(AgyFactSink::default());
    let store =
        Arc::new(external_store::Store::open(workspace.path().join("state.sqlite")).unwrap());
    let scheduler = Scheduler::new(
        "agy-model-identity-test",
        store,
        Arc::new(CapturedFactory {
            inner: harness_factory(&script, workspace.path()),
            facts: facts.clone(),
        }),
        SchedulerConfig {
            bootstrap_timeout: SCRIPTED_SYNC_WAIT,
            control_timeout: SCRIPTED_SYNC_WAIT,
            ..SchedulerConfig::default()
        },
    )
    .unwrap();
    let agent_id = enqueue(&scheduler, workspace.path(), PermissionMode::Build);
    let started = scheduler.start_ready();
    assert!(
        facts.facts().iter().any(|event| matches!(
            &event.fact,
            external_contract::events::EventFact::SessionReady(info)
                if info.model.as_deref() == Some("native-effective-model")
        )),
        "the actual typed identity must preserve init.model"
    );
    assert_eq!(started.unwrap(), vec![agent_id.clone()]);
    let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
    assert_eq!(task.phase, TaskPhase::Running);
    assert_eq!(
        crate::admitted_model_from_task(&task).as_deref(),
        Some(MODEL)
    );
    assert!(scheduler.last_error(&agent_id).is_none());
    std::fs::write(workspace.path().join("release-model-response"), "").unwrap();
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Completed);
    assert_eq!(result.result.final_text, "MODEL_IDENTITY_PRESERVED");
}

#[test]
fn native_interruption_without_es_stop_cannot_succeed_in_the_scheduler() {
    let _guard = scripted_test_guard();
    let workspace = agy_workspace();
    const PARTIAL: &str = "NATIVE_INTERRUPTED_PARTIAL_结束";
    // SYNTHETIC observed interrupted-result shape, with no ES stop/close.
    // Keep the child alive after the result so the real monitor must adjudicate
    // the turn boundary before any child-exit terminal can hide its projection.
    let script = r#"
read_frame
printf '%s\n' '{"event":"init","conversation_id":"__CONV__","init":{}}'
while [ ! -f release-native-interruption ]; do sleep 0.01; done
printf '%s\n' '{"event":"result","result":{"conversation_id":"__CONV__","status":"ERROR","response":"NATIVE_INTERRUPTED_PARTIAL_结束","error":"interrupted","num_turns":1}}'
"#.replace("__CONV__", CONV);
    let scheduler = agy_scheduler(workspace.path(), harness_factory(&script, workspace.path()));
    let agent_id = enqueue(&scheduler, workspace.path(), PermissionMode::Build);
    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
    await_turn_active(&scheduler, &agent_id);
    let before = scheduler.store().get_task(&agent_id).unwrap().unwrap();
    assert!(!before.stop_requested && !before.close_requested);
    std::fs::write(workspace.path().join("release-native-interruption"), "").unwrap();
    let task = await_terminal_task(&scheduler, &agent_id);
    assert!(
        !task.stop_requested && !task.close_requested,
        "no ES cancellation may rescue this oracle"
    );
    assert_eq!(task.outcome, Some(TaskOutcome::Failed));
    let stored = await_result(&scheduler, &agent_id);
    assert_eq!(stored.result.outcome, TaskOutcome::Failed);
    let service = crate::rpc::RpcService::new(scheduler.clone(), scheduler.store()).unwrap();
    let query = || {
        crate::rpc::RpcMethod::TaskWait(crate::rpc::TaskWaitQuery {
            wait_mode: crate::rpc::WaitMode::All,
            agent_id: crate::rpc::WaitTarget::One(agent_id.clone()),
            wait_time: 0,
            message_id: None,
        })
    };
    let crate::rpc::RpcSuccess::TaskWait(wait) = service.dispatch(query()).unwrap() else {
        panic!("wait response")
    };
    assert_eq!(wait.activity.latest_text_tail, PARTIAL);
    assert_eq!(wait.result.unwrap().outcome, TaskOutcome::Failed);
    let crate::rpc::RpcSuccess::TaskWait(second) = service.dispatch(query()).unwrap() else {
        panic!("wait response")
    };
    assert!(second.activity.latest_text_tail.is_empty());
}

fn current_conversation_probe() -> (super::events::AgyRuntimeShared, Arc<AgyFactSink>) {
    let (shared, sink) = agy_fact_probe();
    feed_agy(
        &shared,
        serde_json::json!({"event":"init","conversation_id":"current","init":{}}),
    );
    (shared, sink)
}

#[test]
fn foreign_malformed_conversation_is_excluded_before_payload_validation() {
    // SYNTHETIC known envelopes: nested identity wins; missing/empty nested
    // identity falls back to the top-level field, as in the native parser.
    let cases = [
        serde_json::json!({"event":"step_update","step_update":{"conversation_id":"foreign","step_index":"bad","state":"DONE","step_type":"tool"}}),
        serde_json::json!({"event":"step_update","conversation_id":"foreign","step_update":{"step_index":"bad","state":"DONE","step_type":"tool"}}),
        serde_json::json!({"event":"step_update","conversation_id":"foreign","step_update":{"conversation_id":"","step_index":"bad","state":"DONE","step_type":"tool"}}),
        serde_json::json!({"event":"step_update","conversation_id":"current","step_update":{"conversation_id":"foreign","step_index":"bad","state":"DONE","step_type":"tool"}}),
        serde_json::json!({"event":"result","result":{"conversation_id":"foreign","status":7}}),
        serde_json::json!({"event":"result","conversation_id":"foreign","result":{"status":7}}),
        serde_json::json!({"event":"init","conversation_id":"foreign","init":{"model":7}}),
        // Preserve the existing valid-foreign-frame exclusion too.
        serde_json::json!({"event":"step_update","step_update":{"conversation_id":"foreign","step_index":7,"state":"DONE","step_type":"tool"}}),
    ];
    for raw in cases {
        let (shared, sink) = current_conversation_probe();
        let before = sink.1.observation_snapshot();
        let before_facts = sink.facts();
        let before_turn = shared.turn_snapshot();
        feed_agy(&shared, raw.clone());
        assert_eq!(
            sink.facts(),
            before_facts,
            "foreign frame emitted current facts: {raw}"
        );
        assert_eq!(
            sink.1.observation_snapshot(),
            before,
            "foreign frame damaged current coverage: {raw}"
        );
        assert!(
            !sink.1.snapshot().telemetry_degraded,
            "foreign frame degraded current telemetry: {raw}"
        );
        assert_eq!(shared.turn_snapshot(), before_turn);
        assert_eq!(shared.session_id().as_deref(), Some("current"));
    }
}

#[test]
fn current_malformed_conversation_retains_dimension_damage() {
    use external_contract::events::*;
    let cases = [
        (
            serde_json::json!({"event":"step_update","conversation_id":"foreign","step_update":{"conversation_id":"current","step_index":"bad","state":"DONE","step_type":"tool"}}),
            CoverageDimension::Tools,
        ),
        (
            serde_json::json!({"event":"step_update","conversation_id":"current","step_update":{"conversation_id":"","step_index":"bad","state":"DONE","step_type":"tool"}}),
            CoverageDimension::Tools,
        ),
        (
            serde_json::json!({"event":"step_update","conversation_id":"current","step_update":{"step_index":"bad","state":"DONE","step_type":"agent_response"}}),
            CoverageDimension::Text,
        ),
        (
            serde_json::json!({"event":"result","result":{"conversation_id":"current","status":7}}),
            CoverageDimension::Turn,
        ),
    ];
    for (raw, dimension) in cases {
        let (shared, sink) = current_conversation_probe();
        let dropped = sink.1.observation_snapshot().coverage.dropped_events;
        feed_agy(&shared, raw);
        let facts = sink.facts();
        let event = facts.last().unwrap();
        assert!(matches!(&event.fact, EventFact::CoverageGap(gap)
            if gap.reason == LossReason::Malformed && gap.impact == Impact::Known(vec![dimension])
            && gap.activity == ActivityScope::Turn(TurnId::Local(1))));
        assert_eq!(
            event.context.source.session_id,
            Some(SessionId("current".into()))
        );
        assert_eq!(
            sink.1.observation_snapshot().coverage.dropped_events,
            dropped + 1
        );
        assert_eq!(
            sink.1.snapshot().telemetry_degraded,
            dimension != CoverageDimension::Text
        );
    }
}

#[test]
fn unreadable_or_unknown_conversation_identity_retains_prior_classification() {
    use external_contract::events::*;
    // SYNTHETIC malformed identities must not acquire a foreign attribution
    // from a fallback the parser cannot apply to a non-string nested field.
    for identity in [serde_json::Value::Null, serde_json::json!(7)] {
        let (shared, sink) = current_conversation_probe();
        feed_agy(
            &shared,
            serde_json::json!({"event":"step_update","conversation_id":"foreign","step_update":{"conversation_id":identity,"step_index":"bad","state":"DONE","step_type":"tool"}}),
        );
        assert!(
            matches!(&sink.facts().last().unwrap().fact, EventFact::CoverageGap(gap)
            if gap.reason == LossReason::Malformed && gap.impact == Impact::Known(vec![CoverageDimension::Tools]))
        );
        assert!(sink.1.snapshot().telemetry_degraded);
    }
    let (shared, sink) = current_conversation_probe();
    feed_agy(
        &shared,
        serde_json::json!({"event":"banana","conversation_id":"foreign","payload":{"private":"not diagnostic content"}}),
    );
    assert!(
        matches!(&sink.facts().last().unwrap().fact, EventFact::Diagnostic(diagnostic)
        if diagnostic.reason == DiagnosticReason::UnknownSemantics)
    );
}
