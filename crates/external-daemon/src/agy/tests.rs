//! Scripted NDJSON contract tests for the `agy` adapter: gate, launch
//! admission threading, bootstrap on `init`, turn settlement, multi-turn
//! message delivery, tool activity, cancellation, the idle-signal stream-noise
//! trap, malformed-line tolerance, interleaved child processes, and the
//! process-group reap proof.

use super::*;
use crate::{
    CommandRuntimeFactory, LifecycleSink, MessageDisposition, RuntimeFactory, Scheduler,
    SchedulerConfig,
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
        .prefix("s02-agy-")
        .tempdir_in(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/live-agent/workspace"),
        )
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
        permission_mode,
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
            per_workspace_max_agents: 1,
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
    let scheduler = agy_scheduler(&directory, harness_factory(&HAPPY.replace("__CONV__", CONV), &directory));
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
fn tool_step_pair_feeds_the_wait_window_and_observe_stays_exempt() {
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

    // observe is exempt for agy like codex: available with no tool history and
    // empty reasoning (the runtime source is not verified).
    let (observation, verified) = scheduler.observation_snapshot(&agent_id);
    assert!(!verified);
    let expected = crate::observation::ObservationState::for_adapter("agy").snapshot();
    assert_eq!(observation, expected);
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
/// at resolution time (S03 admission is expected to reject it upstream).
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
        error.to_string().contains("build and yolo permission modes"),
        "{error}"
    );
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Failed));
}

struct NoopSink;
impl crate::LifecycleSink for NoopSink {
    fn emit(&self, _record: crate::LifecycleRecord) {}
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
        Arc::new(crate::Publisher::new(Arc::new(NoopSink))),
        Arc::new(crate::TurnTracker::new()),
    ));
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
        // 与 watchdog 相同：持 publisher latch 读取 snapshot。
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
        fn emit(&self, record: crate::LifecycleRecord) {
            let crate::RuntimeEvent::Driver(external_runtime::Inbound::Message(
                external_contract::WireMessage::Event(event),
            )) = record.event
            else {
                return;
            };
            if event.params["type"] == "turn.completed" {
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
        }))),
        Arc::new(crate::TurnTracker::new()),
    ));
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
    let tracker = Arc::new(crate::TurnTracker::new());
    let shared = AgyRuntimeShared::new(
        Arc::new(crate::Publisher::new(Arc::new(NoopSink))),
        tracker.clone(),
    );
    shared.begin_turn();
    shared.buffer_input(|| Ok(())).unwrap();
    let result = |count| {
        external_agent_agy::event::parse_line(&format!(r#"{{"event":"result","result":{{"conversation_id":"{CONV}","status":"SUCCESS","response":"turn {count}","num_turns":{count}}}}}"#)).unwrap()
    };
    shared.project_event(&result(1));
    assert!(shared.turn_snapshot().active);
    let first = tracker.snapshot();
    assert_eq!(first.boundary, Some(crate::TurnBoundary::Completed));
    shared.project_event(&result(1));
    assert_eq!(tracker.snapshot(), first);
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
