//! Scripted app-server contract tests for the Codex adapter: gate, launch
//! bounds, thread identity, resume posture, turn attribution, and the
//! shared scheduler lifecycle.

use super::*;
use crate::{
    terminal_proves_process_group_reaped, LifecycleSink, ManagedRuntime, RuntimeFactory, Scheduler,
    SchedulerConfig,
};
use external_core::{AdmissionIdentity, GeneralTaskManifest, PermissionMode, GENERAL_TASK_SCHEMA};
use external_store::{TaskOutcome, TaskPhase, TaskRecord};
use std::{
    io,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

fn codex_admission(model: Option<&str>) -> AdmissionIdentity {
    codex_admission_with_effort(model, None)
}

/// [`codex_admission`] with an explicit admitted reasoning effort, mirroring
/// the `prepared_launch_json.admission.effort` persistence shape.
fn codex_admission_with_effort(model: Option<&str>, effort: Option<&str>) -> AdmissionIdentity {
    AdmissionIdentity {
        agent: "codex".into(),
        config_revision: 7,
        adapter_version: "test".into(),
        model: model.map(str::to_owned),
        model_source: "spawn_catalog".into(),
        effort: effort.map(str::to_owned),
    }
}

/// Serialize tests that drive scripted app-server children through the
/// whole scheduler, mirroring the DSH suite's scripted-child guard: the
/// parallel suite already runs timing-sensitive fixture probes.
fn scripted_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static SCRIPTED_CHILD_LOCK: Mutex<()> = Mutex::new(());
    SCRIPTED_CHILD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn codex_workspace() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/live-agent/workspace")
        .canonicalize()
        .unwrap();
    tempfile::Builder::new()
        .prefix("s01-codex-")
        .tempdir_in(root)
        .unwrap()
}

fn manifest_for(workspace: &Path, prompt: &str) -> GeneralTaskManifest {
    manifest_for_mode(workspace, prompt, PermissionMode::Plan)
}

fn manifest_for_mode(
    workspace: &Path,
    prompt: &str,
    permission_mode: PermissionMode,
) -> GeneralTaskManifest {
    GeneralTaskManifest {
        schema: GENERAL_TASK_SCHEMA.into(),
        agent_id: "codex-test".into(),
        repository: workspace.to_path_buf(),
        permission_mode,
        prompt: prompt.into(),
        write_manifest: Vec::new(),
    }
}

fn codex_scheduler(workspace: &Path, factory: CodexRuntimeFactory) -> Scheduler {
    let store = Arc::new(external_store::Store::open(workspace.join("state.sqlite")).unwrap());
    let zcode = crate::CommandRuntimeFactory::new(|_: &TaskRecord| {
        Err::<Command, _>(io::Error::other("zcode factory must not spawn codex tasks"))
    });
    Scheduler::new(
        "codex-test",
        store,
        Arc::new(crate::dsh::RoutingRuntimeFactory::with_codex(
            zcode,
            crate::dsh::DshRuntimeFactory::closed(),
            factory,
        )),
        // Generous deadlines: the scripted children share the machine
        // with the rest of the parallel suite.
        SchedulerConfig {
            bootstrap_timeout: Duration::from_secs(30),
            control_timeout: Duration::from_secs(30),
            ..SchedulerConfig::default()
        },
    )
    .unwrap()
}

fn harness_factory(script: &str, workspace: &Path) -> CodexRuntimeFactory {
    let child = workspace.join("codex-fake.sh");
    std::fs::write(&child, format!("#!/bin/sh\n{script}")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut mode = std::fs::metadata(&child).unwrap().permissions();
        mode.set_mode(0o755);
        std::fs::set_permissions(&child, mode).unwrap();
    }
    let launch = CodexLaunch::new(child, workspace.join("codex-home"));
    CodexRuntimeFactory::test_harness(Some(launch))
}

fn await_terminal_task(scheduler: &Scheduler, agent_id: &str) -> external_store::TaskRecord {
    let deadline = Instant::now() + Duration::from_secs(5);
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
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = scheduler.store().task_result(agent_id).unwrap() {
            return result;
        }
        assert!(Instant::now() < deadline, "result was never persisted");
        thread::sleep(Duration::from_millis(10));
    }
}

const THREAD_ID: &str = "codex-thread-1";
const MODEL: &str = "gpt-5.6-terra";

/// The posture echo a confirmed `thread/start` result carries at the result
/// root for one admitted permission mode, as the real app-server returns it
/// (verified on codex-cli 0.154.0: plan resolves the read-only object, yolo
/// the faithful dangerFullAccess object; both echo `approvalPolicy` and
/// `cwd`).
fn start_echo(directory: &Path, mode: PermissionMode) -> String {
    let sandbox = match mode {
        PermissionMode::Plan => r#"{"type":"readOnly","networkAccess":false}"#,
        PermissionMode::Yolo => r#"{"type":"dangerFullAccess"}"#,
        PermissionMode::Build | PermissionMode::Edit => {
            r#"{"type":"workspaceWrite","networkAccess":false,"writableRoots":[],"excludeSlashTmp":false,"excludeTmpdirEnvVar":false}"#
        }
    };
    format!(
        r#""sandbox":{sandbox},"approvalPolicy":"never","cwd":"{}""#,
        directory.to_string_lossy()
    )
}

/// A scripted app-server speaking the strict frame sequence of a fresh
/// task: initialize(id1), initialized notification, thread/start(id2),
/// turn/start(id3). Every inbound frame is appended to deliveries.jsonl
/// as it arrives. The thread/start result echoes the confirmed posture
/// for `mode` (see [`start_echo`]).
fn happy_turn(directory: &Path, mode: PermissionMode) -> String {
    happy_turn_with_echo(&start_echo(directory, mode))
}

/// [`happy_turn`] with a caller-supplied thread/start posture echo, so a
/// test can pin exactly one divergence from the confirmed posture; an
/// empty echo is a start result that carries no posture fields at all.
fn happy_turn_with_echo(echo: &str) -> String {
    happy_turn_with_effort(echo, None)
}

/// [`happy_turn_with_echo`] plus a controllable `reasoningEffort` echo on
/// the thread/start result: `Some(effort)` pins the echoed token (equal to
/// or diverging from the admitted request), `None` is a result that does
/// not carry the field at all — the three states the start-side effort
/// confirmation has to distinguish.
fn happy_turn_with_effort(echo: &str, effort_echo: Option<&str>) -> String {
    let mut start_fields = if echo.is_empty() {
        r#""model":"gpt-5.6-terra""#.to_owned()
    } else {
        format!(r#""model":"gpt-5.6-terra",{echo}"#)
    };
    if let Some(effort) = effort_echo {
        start_fields.push_str(&format!(r#","reasoningEffort":"{effort}""#));
    }
    format!(
        r#"
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{{"id":1,"result":{{"codexHome":"/tmp/codex-home","userAgent":"fake"}}}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"codex-thread-1","ephemeral":false}},{start_fields}}}}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"codex-turn-1","status":"inProgress"}}}}}}' \
  '{{"method":"turn/started","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-1","status":"inProgress"}}}}}}' \
  '{{"method":"item/agentMessage/delta","params":{{"threadId":"codex-thread-1","turnId":"codex-turn-1","itemId":"msg_1","delta":"CODEX_OK"}}}}' \
  '{{"method":"item/completed","params":{{"threadId":"codex-thread-1","turnId":"codex-turn-1","item":{{"type":"agentMessage","id":"msg_1","text":"CODEX_OK"}}}}}}' \
  '{{"method":"turn/completed","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-1","status":"completed","error":null}}}}}}'
while IFS= read -r line; do printf '%s\n' "$line" >> deliveries.jsonl; done
"#
    )
}

#[test]
fn public_submit_reaches_persistent_thread_and_persists_the_id() {
    let _guard = scripted_test_guard();
    let workspace = codex_workspace();
    let scheduler = codex_scheduler(
        workspace.path(),
        harness_factory(
            &happy_turn(workspace.path(), PermissionMode::Plan),
            workspace.path(),
        ),
    );
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(workspace.path(), "inspect the repository"),
            Some(codex_admission(Some(MODEL))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Completed);
    assert_eq!(result.result.final_text, "CODEX_OK");
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.session_id.as_deref(), Some(THREAD_ID));
    // The exact child contract is visible in the recorded deliveries.
    let deliveries = std::fs::read_to_string(workspace.path().join("deliveries.jsonl")).unwrap();
    let initialize: serde_json::Value =
        serde_json::from_str(deliveries.lines().next().expect("at least one request")).unwrap();
    assert_eq!(initialize["method"], "initialize");
    let mut requests = deliveries
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap());
    let thread_start = requests
        .find(|frame| frame["method"] == "thread/start")
        .expect("thread/start frame");
    assert_eq!(thread_start["params"]["ephemeral"], false);
    assert_eq!(thread_start["params"]["approvalPolicy"], "never");
    assert_eq!(thread_start["params"]["sandbox"], "read-only");
    assert_eq!(thread_start["params"]["model"], MODEL);
    assert_eq!(
        thread_start["params"]["cwd"],
        workspace.path().to_string_lossy().as_ref()
    );
    let turn_start = requests
        .find(|frame| frame["method"] == "turn/start")
        .expect("turn/start frame");
    assert_eq!(turn_start["params"]["threadId"], THREAD_ID);
    assert_eq!(turn_start["params"]["model"], MODEL);
    assert!(turn_start["params"]["input"][0]["text"]
        .as_str()
        .unwrap()
        .contains("inspect the repository"));
}

#[test]
fn yolo_submit_pins_danger_full_access_and_persists_the_id() {
    let _guard = scripted_test_guard();
    let workspace = codex_workspace();
    let scheduler = codex_scheduler(
        workspace.path(),
        harness_factory(
            &happy_turn(workspace.path(), PermissionMode::Yolo),
            workspace.path(),
        ),
    );
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for_mode(workspace.path(), "run freely", PermissionMode::Yolo),
            Some(codex_admission(Some(MODEL))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Completed);
    assert_eq!(result.result.final_text, "CODEX_OK");
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.session_id.as_deref(), Some(THREAD_ID));
    let deliveries = std::fs::read_to_string(workspace.path().join("deliveries.jsonl")).unwrap();
    let thread_start = deliveries
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|frame| frame["method"] == "thread/start")
        .expect("thread/start frame");
    assert_eq!(thread_start["params"]["approvalPolicy"], "never");
    assert_eq!(thread_start["params"]["sandbox"], "danger-full-access");
    assert_eq!(thread_start["params"]["model"], MODEL);
}

#[test]
fn write_modes_pin_workspace_write_and_persist_the_id() {
    let _guard = scripted_test_guard();
    for mode in [PermissionMode::Build, PermissionMode::Edit] {
        let workspace = codex_workspace();
        let scheduler = codex_scheduler(
            workspace.path(),
            harness_factory(&happy_turn(workspace.path(), mode), workspace.path()),
        );
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for_mode(workspace.path(), "run freely", mode),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let agent_id = submitted.agent_id.clone();
        assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
        let result = await_result(&scheduler, &agent_id);
        assert_eq!(result.result.outcome, TaskOutcome::Completed);
        assert_eq!(result.result.final_text, "CODEX_OK");
        let task = await_terminal_task(&scheduler, &agent_id);
        assert_eq!(task.session_id.as_deref(), Some(THREAD_ID));
        let deliveries =
            std::fs::read_to_string(workspace.path().join("deliveries.jsonl")).unwrap();
        let thread_start = deliveries
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|frame| frame["method"] == "thread/start")
            .expect("thread/start frame");
        assert_eq!(thread_start["params"]["approvalPolicy"], "never");
        assert_eq!(thread_start["params"]["sandbox"], "workspace-write");
        assert_eq!(thread_start["params"]["model"], MODEL);
    }
}

#[test]
fn start_fails_closed_without_a_confirmed_posture() {
    let _guard = scripted_test_guard();
    // The thread/start result is the posture the server actually applied,
    // echoed at the result root (verified live on codex-cli 0.154.0 for
    // both admitted presets). Request params alone are not trusted for
    // the first executable turn: each negative pins exactly one divergence
    // from the confirmed echo and must fail closed BEFORE any turn/start
    // is sent, while both confirmed yolo shapes — the faithful
    // dangerFullAccess object a real start echoes and the narrowed
    // workspace-write reconstruction resume also accepts — start the turn.
    let mut cases: Vec<(PermissionMode, Option<String>, &str, Option<&str>, bool, &str)> = vec![
        // No posture fields at all: unverifiable means refused.
        (
            PermissionMode::Plan,
            None,
            "never",
            None,
            false,
            "start sandbox was not confirmed as the read-only object",
        ),
        // A write-capable sandbox object is never accepted for a plan task.
        (
            PermissionMode::Plan,
            Some(r#"{"type":"workspaceWrite","networkAccess":false}"#.into()),
            "never",
            None,
            false,
            "start sandbox was not confirmed as the read-only object",
        ),
        // A network-capable read-only object diverges from the observed
        // plan posture.
        (
            PermissionMode::Plan,
            Some(r#"{"type":"readOnly","networkAccess":true}"#.into()),
            "never",
            None,
            false,
            "start sandbox was not confirmed as the read-only object",
        ),
        // An approval policy other than never.
        (
            PermissionMode::Plan,
            Some(r#"{"type":"readOnly","networkAccess":false}"#.into()),
            "on-request",
            None,
            false,
            "start approvalPolicy was not confirmed as never",
        ),
        // A thread rooted somewhere other than the task workspace.
        (
            PermissionMode::Plan,
            Some(r#"{"type":"readOnly","networkAccess":false}"#.into()),
            "never",
            Some("/elsewhere"),
            false,
            "start cwd does not match the task workspace",
        ),
        // The plan posture is another permission mode's shape and is never
        // accepted for a yolo task.
        (
            PermissionMode::Yolo,
            Some(r#"{"type":"readOnly","networkAccess":false}"#.into()),
            "never",
            None,
            false,
            "start sandbox was not confirmed as the danger-full-access posture",
        ),
        // The request-time string preset is not the resolved posture the
        // live probe returns; it stays unconfirmed.
        (
            PermissionMode::Yolo,
            Some(r#""danger-full-access""#.into()),
            "never",
            None,
            false,
            "start sandbox was not confirmed as the danger-full-access posture",
        ),
        // The faithful yolo echo starts the turn.
        (
            PermissionMode::Yolo,
            Some(r#"{"type":"dangerFullAccess"}"#.into()),
            "never",
            None,
            true,
            "",
        ),
        // The narrowed workspace-write reconstruction (strictly narrower
        // than the requested posture) starts the turn as well.
        (
            PermissionMode::Yolo,
            Some(
                r#"{"type":"workspaceWrite","networkAccess":false,"writableRoots":[],"excludeSlashTmp":false,"excludeTmpdirEnvVar":false}"#
                    .into(),
            ),
            "never",
            None,
            true,
            "",
        ),
    ];
    for mode in [PermissionMode::Build, PermissionMode::Edit] {
        cases.push((
            mode,
            Some(r#"{"type":"dangerFullAccess"}"#.into()),
            "never",
            None,
            false,
            "start sandbox was not confirmed as the workspace-write object",
        ));
        cases.push((mode, Some(r#"{"type":"workspaceWrite","networkAccess":false,"writableRoots":[],"excludeSlashTmp":false,"excludeTmpdirEnvVar":false}"#.into()), "on-request", None, false,
            "start approvalPolicy was not confirmed as never"));
    }
    for (index, (mode, sandbox, approval, cwd_override, accepts, marker)) in
        cases.into_iter().enumerate()
    {
        let workspace = codex_workspace();
        let directory = workspace.path().to_owned();
        let cwd = cwd_override
            .map(str::to_string)
            .unwrap_or_else(|| directory.to_string_lossy().into_owned());
        let sandbox_fields = sandbox
            .map(|value| format!(r#""sandbox":{value},"#))
            .unwrap_or_default();
        let echo = format!(r#"{sandbox_fields}"approvalPolicy":"{approval}","cwd":"{cwd}""#);
        let scheduler = codex_scheduler(
            &directory,
            harness_factory(&happy_turn_with_echo(&echo), &directory),
        );
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for_mode(&directory, &format!("posture case {index}"), mode),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let agent_id = submitted.agent_id.clone();
        if accepts {
            scheduler.start_ready().unwrap();
            let result = await_result(&scheduler, &agent_id);
            assert_eq!(
                result.result.outcome,
                TaskOutcome::Completed,
                "confirmed posture case {index} must start the turn"
            );
            assert_eq!(result.result.final_text, "CODEX_OK");
        } else {
            assert!(
                scheduler.start_ready().is_err(),
                "an unconfirmed start posture must fail closed (case {index})"
            );
            let task = await_terminal_task(&scheduler, &agent_id);
            assert_eq!(task.outcome, Some(TaskOutcome::Failed));
            assert_eq!(
                task.session_id, None,
                "no thread may be persisted on an unconfirmed start (case {index})"
            );
            let record = scheduler.last_error(&agent_id).expect("failure record");
            assert!(record.contains(marker), "case {index} record: {record}");
            let deliveries = std::fs::read_to_string(directory.join("deliveries.jsonl"))
                .expect("the scripted child logs its frames");
            let turn_starts = deliveries
                .lines()
                .filter(|line| line.contains(r#""method":"turn/start""#))
                .count();
            assert_eq!(
                turn_starts, 0,
                "no turn may start on an unconfirmed posture (case {index})"
            );
        }
    }
}

#[test]
fn start_effort_echo_is_diagnostic_only_and_never_blocks_the_turn() {
    let _guard = scripted_test_guard();
    // OBSERVED on codex-cli 0.154.0 (.agent-work/tmp/codex-app-server-probe/
    // result-20260915-persistent.json): a thread/start without an effort
    // echoes reasoningEffort="medium" — the gpt-5.6-terra model default
    // (defaultReasoningEffort="medium"), not an acknowledgement of the
    // admitted selection. The start-side echo is therefore diagnostic-only:
    // whether the echo names the model default (medium), matches the
    // admission (high), or is missing entirely, the first turn must still
    // start and its turn/start frame must name the admitted effort.
    for (index, effort_echo) in [Some("medium"), Some("high"), None].iter().enumerate() {
        let workspace = codex_workspace();
        let directory = workspace.path().to_owned();
        let echo = start_echo(&directory, PermissionMode::Plan);
        let scheduler = codex_scheduler(
            &directory,
            harness_factory(&happy_turn_with_effort(&echo, *effort_echo), &directory),
        );
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for(&directory, &format!("effort case {index}")),
                Some(codex_admission_with_effort(Some(MODEL), Some("high"))),
            )
            .unwrap();
        let agent_id = submitted.agent_id.clone();
        scheduler.start_ready().unwrap();
        let result = await_result(&scheduler, &agent_id);
        assert_eq!(
            result.result.outcome,
            TaskOutcome::Completed,
            "start echo case {index} must never block the turn"
        );
        assert_eq!(result.result.final_text, "CODEX_OK");
        let deliveries = std::fs::read_to_string(directory.join("deliveries.jsonl")).unwrap();
        let turn_start = deliveries
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|frame| frame["method"] == "turn/start")
            .expect("turn/start frame");
        assert_eq!(
            turn_start["params"]["effort"], "high",
            "the admitted effort must reach the turn/start frame (case {index})"
        );
    }
}

#[test]
fn an_omitted_effort_keeps_the_low_default_on_the_turn_start_frame() {
    let _guard = scripted_test_guard();
    // No admitted effort: nothing is compared against the thread result and
    // the turn/start wire keeps its historical `"effort":"low"` default.
    let workspace = codex_workspace();
    let scheduler = codex_scheduler(
        workspace.path(),
        harness_factory(
            &happy_turn(workspace.path(), PermissionMode::Plan),
            workspace.path(),
        ),
    );
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(workspace.path(), "default effort probe"),
            Some(codex_admission(Some(MODEL))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    scheduler.start_ready().unwrap();
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Completed);
    let deliveries = std::fs::read_to_string(workspace.path().join("deliveries.jsonl")).unwrap();
    let turn_start = deliveries
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|frame| frame["method"] == "turn/start")
        .expect("turn/start frame");
    assert_eq!(turn_start["params"]["effort"], "low");
}

#[test]
fn a_queued_followup_turn_in_the_same_process_keeps_the_admitted_effort() {
    let _guard = scripted_test_guard();
    // A message queued while the first turn is still active is delivered
    // through the same live runtime (the first turn's natural completion
    // defers to the queued message instead of terminating): the follow-up
    // turn/start comes from send_turn reading the shared admitted effort,
    // so both frames of the same process must name the admitted effort,
    // never the low default.
    let workspace = codex_workspace();
    let directory = workspace.path().to_owned();
    let echo = start_echo(&directory, PermissionMode::Plan);
    // The scripted child pauses after the first turn/started and only
    // finishes the turn once the follow-up message is queued, so the
    // natural completion of turn 1 always sees the queued message and
    // defers to it inside the same live process. The turn-2 frames are
    // emitted only after the daemon's own follow-up turn/start request
    // arrives: a started notification that precedes the start request in
    // flight is dropped as unsolicited traffic by the attribution gate.
    let script = format!(
        r#"
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{{"id":1,"result":{{"codexHome":"/tmp/codex-home"}}}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"codex-thread-1","ephemeral":false}},"model":"gpt-5.6-terra",{echo}}}}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"codex-turn-1","status":"inProgress"}}}}}}' \
  '{{"method":"turn/started","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-1","status":"inProgress"}}}}}}'
while [ ! -f release ]; do sleep 0.01; done
printf '%s\n' '{{"method":"item/completed","params":{{"threadId":"codex-thread-1","turnId":"codex-turn-1","item":{{"type":"agentMessage","id":"msg_1","text":"FIRST_OK"}}}}}}' \
  '{{"method":"turn/completed","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-1","status":"completed","error":null}}}}}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{{"id":4,"result":{{"turn":{{"id":"codex-turn-2","status":"inProgress"}}}}}}' \
  '{{"method":"turn/started","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-2","status":"inProgress"}}}}}}' \
  '{{"method":"item/completed","params":{{"threadId":"codex-thread-1","turnId":"codex-turn-2","item":{{"type":"agentMessage","id":"msg_2","text":"SECOND_OK"}}}}}}' \
  '{{"method":"turn/completed","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-2","status":"completed","error":null}}}}}}'
while IFS= read -r line; do printf '%s\n' "$line" >> deliveries.jsonl; done
"#
    );
    let scheduler = codex_scheduler(&directory, harness_factory(&script, &directory));
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(&directory, "first turn"),
            Some(codex_admission_with_effort(Some(MODEL), Some("high"))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    scheduler.start_ready().unwrap();
    // With the first turn active the runtime is live: queueing now makes
    // the first turn's natural completion defer to the message and deliver
    // it through send_turn in the same process.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        if matches!(task.turn_state, external_store::TurnState::Active) {
            break;
        }
        assert!(Instant::now() < deadline, "first turn never became active");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        scheduler
            .queue_message(&agent_id, "same-process-msg", "follow-up question")
            .unwrap(),
        crate::MessageDisposition::Queued
    );
    std::fs::write(directory.join("release"), "").unwrap();
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Completed);
    assert_eq!(result.result.final_text, "SECOND_OK");

    let deliveries = std::fs::read_to_string(directory.join("deliveries.jsonl")).unwrap();
    let turn_starts = deliveries
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|frame| frame["method"] == "turn/start")
        .collect::<Vec<_>>();
    assert_eq!(turn_starts.len(), 2, "both turns run in the same process");
    assert_eq!(turn_starts[0]["params"]["effort"], "high");
    assert_eq!(
        turn_starts[1]["params"]["effort"], "high",
        "the same-process follow-up turn must not fall back to low"
    );
    assert!(turn_starts[1]["params"]["input"][0]["text"]
        .as_str()
        .unwrap()
        .contains("follow-up question"));
}

#[test]
fn closed_gate_refuses_spawn_without_a_process() {
    let _guard = scripted_test_guard();
    let workspace = codex_workspace();
    let scheduler = codex_scheduler(workspace.path(), CodexRuntimeFactory::closed());
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(workspace.path(), "never runs"),
            Some(codex_admission(Some(MODEL))),
        )
        .unwrap();
    let error = scheduler.start_ready().unwrap_err();
    assert!(error.to_string().contains("codex spawn gate is closed"));
    let task = await_terminal_task(&scheduler, &submitted.agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Failed));
}

struct NoopSink;
impl crate::LifecycleSink for NoopSink {
    fn emit(&self, _record: crate::LifecycleRecord) {}
}

#[test]
fn terminal_send_resumes_the_same_thread_in_a_new_process_without_replay() {
    let _guard = scripted_test_guard();
    let workspace = codex_workspace();
    let directory = workspace.path().to_owned();
    let resume_script = r#"
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
printf '%s\n' '{"id":1,"result":{"codexHome":"/tmp/codex-home","userAgent":"fake"}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
printf '%s\n' "{\"id\":2,\"result\":{\"thread\":{\"id\":\"codex-thread-1\",\"ephemeral\":false},\"model\":\"gpt-5.6-terra\",\"sandbox\":{\"type\":\"readOnly\",\"networkAccess\":false},\"approvalPolicy\":\"never\",\"cwd\":\"$(pwd)\"}}"
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
printf '%s\n' '{"id":3,"result":{"turn":{"id":"codex-turn-2","status":"inProgress"}}}' \
  '{"method":"turn/started","params":{"threadId":"codex-thread-1","turn":{"id":"codex-turn-2","status":"inProgress"}}}' \
  '{"method":"item/completed","params":{"threadId":"codex-thread-1","turnId":"codex-turn-2","item":{"type":"agentMessage","id":"msg_2","text":"RESUMED_OK"}}}' \
  '{"method":"turn/completed","params":{"threadId":"codex-thread-1","turn":{"id":"codex-turn-2","status":"completed","error":null}}}'
while IFS= read -r line; do printf '%s\n' "$line" >> deliveries-resume.jsonl; done
"#;
    struct TwoPhaseFactory {
        first: Mutex<Option<CodexRuntimeFactory>>,
        second: CodexRuntimeFactory,
    }
    impl RuntimeFactory for TwoPhaseFactory {
        fn spawn(
            &self,
            task: &TaskRecord,
            sink: Arc<dyn LifecycleSink>,
        ) -> io::Result<Arc<dyn ManagedRuntime>> {
            let mut first = self.first.lock().unwrap();
            match first.take() {
                Some(factory) => factory.spawn(task, sink),
                None => self.second.spawn(task, sink),
            }
        }
    }
    let first = harness_factory(&happy_turn(&directory, PermissionMode::Plan), &directory);
    let second = {
        let child = directory.join("codex-resume.sh");
        std::fs::write(&child, format!("#!/bin/sh\n{resume_script}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut mode = std::fs::metadata(&child).unwrap().permissions();
            mode.set_mode(0o755);
            std::fs::set_permissions(&child, mode).unwrap();
        }
        CodexRuntimeFactory::test_harness(Some(CodexLaunch::new(
            child,
            directory.join("codex-home"),
        )))
    };
    let store = Arc::new(external_store::Store::open(directory.join("state.sqlite")).unwrap());
    let scheduler = Scheduler::new(
        "codex-resume-test",
        store,
        Arc::new(TwoPhaseFactory {
            first: Mutex::new(Some(first)),
            second,
        }),
        SchedulerConfig {
            bootstrap_timeout: Duration::from_secs(30),
            control_timeout: Duration::from_secs(30),
            ..SchedulerConfig::default()
        },
    )
    .unwrap();
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(&directory, "first turn"),
            Some(codex_admission(Some(MODEL))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    scheduler.start_ready().unwrap();
    let first_result = await_result(&scheduler, &agent_id);
    assert_eq!(first_result.result.final_text, "CODEX_OK");
    let deadline = Instant::now() + Duration::from_secs(5);
    while scheduler.active_count() > 0 {
        assert!(
            Instant::now() < deadline,
            "first runtime was never released"
        );
        thread::sleep(Duration::from_millis(10));
    }

    // Public terminal send is the explicit recovery trigger.
    assert_eq!(
        scheduler
            .queue_message(&agent_id, "resume-msg-1", "follow-up question")
            .unwrap(),
        crate::MessageDisposition::Queued
    );
    // queue_message only requeues; the daemon claim loop performs the
    // spawn. Either this call or the finishing monitor's trailing claim
    // wins the single resume claim.
    scheduler.start_ready().unwrap();
    let resumed = await_result(&scheduler, &agent_id);
    assert_eq!(resumed.result.outcome, TaskOutcome::Completed);
    assert_eq!(resumed.result.final_text, "RESUMED_OK");

    let deliveries = std::fs::read_to_string(directory.join("deliveries-resume.jsonl"))
        .expect("the resumed process logs its own frames");
    let mut frames = deliveries
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap());
    let resume = frames
        .find(|frame| frame["method"] == "thread/resume")
        .expect("thread/resume frame");
    assert_eq!(resume["params"]["threadId"], THREAD_ID);
    assert_eq!(resume["params"]["excludeTurns"], true);
    let turn_starts = deliveries
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|frame| frame["method"] == "turn/start")
        .collect::<Vec<_>>();
    assert_eq!(turn_starts.len(), 1, "interrupted turn must not replay");
    assert!(turn_starts[0]["params"]["input"][0]["text"]
        .as_str()
        .unwrap()
        .contains("follow-up question"));
    let message = scheduler.store().message("resume-msg-1").unwrap().unwrap();
    assert_eq!(
        message.state,
        external_store::MessageState::Delivered,
        "resume message was not delivered"
    );
    assert_eq!(message.target_turn_id.as_deref(), Some("codex-turn-2"));
}

#[test]
fn terminal_send_rejects_non_codex_cancelled_and_sessionless_tasks() {
    let _guard = scripted_test_guard();
    let workspace = codex_workspace();
    let scheduler = codex_scheduler(
        workspace.path(),
        harness_factory(
            &happy_turn(workspace.path(), PermissionMode::Plan),
            workspace.path(),
        ),
    );
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(workspace.path(), "completed codex task"),
            Some(codex_admission(Some(MODEL))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    scheduler.start_ready().unwrap();
    await_terminal_task(&scheduler, &agent_id);

    // A zcode terminal task keeps the generic rejection.
    let zcode_task = scheduler
        .enqueue_general(&manifest_for(workspace.path(), "zcode terminal task"))
        .unwrap();
    let store = scheduler.store();
    let claim = store
        .claim_next("terminal-reject", usize::MAX, 1)
        .unwrap()
        .unwrap();
    assert_eq!(claim.task.agent_id, zcode_task.agent_id);
    store
        .mark_session_running(
            &claim.task.agent_id,
            claim.owner_epoch,
            "runtime",
            None,
            None,
            None,
        )
        .unwrap();
    store
        .store_task_result(
            &zcode_task.agent_id,
            &external_store::TaskResult {
                outcome: TaskOutcome::Completed,
                final_text: "done".into(),
                partial: false,
            },
        )
        .unwrap();
    store
        .transition_terminal(
            &zcode_task.agent_id,
            claim.owner_epoch,
            &external_store::TerminalUpdate {
                outcome: TaskOutcome::Completed,
                failure_code: None,
                failure_message: None,
            },
        )
        .unwrap();
    let error = scheduler
        .queue_message(&zcode_task.agent_id, "zcode-msg", "nope")
        .unwrap_err();
    assert!(error.to_string().contains("TERMINAL_SEND_UNSUPPORTED"));

    // A cancelled codex terminal task is rejected even with a thread id.
    let cancelled = scheduler
        .enqueue_general_with_admission(
            &manifest_for(workspace.path(), "cancelled codex task"),
            Some(codex_admission(Some(MODEL))),
        )
        .unwrap();
    let store = scheduler.store();
    let claim = store
        .claim_next("terminal-reject", usize::MAX, 1)
        .unwrap()
        .unwrap();
    assert_eq!(claim.task.agent_id, cancelled.agent_id);
    store
        .mark_session_running(
            &claim.task.agent_id,
            claim.owner_epoch,
            "runtime",
            None,
            Some(THREAD_ID),
            Some(external_store::TurnState::Idle),
        )
        .unwrap();
    store.request_stop(&cancelled.agent_id).unwrap();
    store
        .transition_terminal(
            &cancelled.agent_id,
            claim.owner_epoch,
            &external_store::TerminalUpdate {
                outcome: TaskOutcome::Cancelled,
                failure_code: None,
                failure_message: None,
            },
        )
        .unwrap();
    let error = scheduler
        .queue_message(&cancelled.agent_id, "cancel-msg", "nope")
        .unwrap_err();
    assert!(error.to_string().contains("TERMINAL_SEND_UNSUPPORTED"));

    // A codex terminal task without a persisted thread id is rejected.
    let sessionless = scheduler
        .enqueue_general_with_admission(
            &manifest_for(workspace.path(), "sessionless codex task"),
            Some(codex_admission(Some(MODEL))),
        )
        .unwrap();
    let store = scheduler.store();
    let claim = store
        .claim_next("terminal-reject", usize::MAX, 1)
        .unwrap()
        .unwrap();
    assert_eq!(claim.task.agent_id, sessionless.agent_id);
    store
        .mark_session_running(
            &claim.task.agent_id,
            claim.owner_epoch,
            "runtime",
            None,
            None,
            None,
        )
        .unwrap();
    store
        .store_task_result(
            &sessionless.agent_id,
            &external_store::TaskResult {
                outcome: TaskOutcome::Failed,
                final_text: "never started".into(),
                partial: true,
            },
        )
        .unwrap();
    store
        .transition_terminal(
            &sessionless.agent_id,
            claim.owner_epoch,
            &external_store::TerminalUpdate {
                outcome: TaskOutcome::Failed,
                failure_code: Some("SESSION_START_FAILED".into()),
                failure_message: None,
            },
        )
        .unwrap();
    let error = scheduler
        .queue_message(&sessionless.agent_id, "sessionless-msg", "nope")
        .unwrap_err();
    assert!(error.to_string().contains("TERMINAL_SEND_UNSUPPORTED"));
}

#[test]
fn server_overloaded_and_mcp_startup_failures_stay_bounded_and_diagnostic() {
    let _guard = scripted_test_guard();
    let workspace = codex_workspace();
    let echo = start_echo(workspace.path(), PermissionMode::Plan);
    let script = format!(
        r#"
IFS= read -r line
printf '%s\n' '{{"id":1,"result":{{"codexHome":"/tmp/codex-home","userAgent":"fake"}}}}'
IFS= read -r line
printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"codex-thread-1","ephemeral":false}},"model":"gpt-5.6-terra",{echo}}}}}'
IFS= read -r line
IFS= read -r line
printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"codex-turn-1","status":"inProgress"}}}}}}' \
  '{{"method":"turn/started","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-1","status":"inProgress"}}}}}}' \
  '{{"method":"mcpServer/startupStatus/updated","params":{{"threadId":"codex-thread-1","name":"cloudflare-api","status":"failed","error":"requires OAuth reauthentication","failureReason":"reauthenticationRequired"}}}}' \
  '{{"method":"error","params":{{"error":{{"message":"Selected model is at capacity. Please try a different model.","codexErrorInfo":"serverOverloaded"}},"willRetry":false,"threadId":"codex-thread-1","turnId":"codex-turn-1"}}}}' \
  '{{"method":"turn/completed","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-1","status":"failed","error":{{"message":"Selected model is at capacity.","codexErrorInfo":"serverOverloaded"}}}}}}}}'
sleep 1
"#
    );
    let scheduler = codex_scheduler(workspace.path(), harness_factory(&script, workspace.path()));
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(workspace.path(), "overloaded probe"),
            Some(codex_admission(Some(MODEL))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    scheduler.start_ready().unwrap();
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Failed);
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.session_id.as_deref(), Some(THREAD_ID));
    let record = scheduler
        .last_error(&agent_id)
        .expect("correlated failure record");
    assert!(record.contains("serverOverloaded"), "record: {record}");
    assert!(record.contains("cloudflare-api"), "record: {record}");
}

#[test]
fn strict_envelope_missing_thread_and_model_mismatch_fail_the_start_bounded() {
    let _guard = scripted_test_guard();
    for (script, marker) in [
        // Malformed envelope: a jsonrpc frame is rejected by the strict
        // codec, then the child exits without a turn boundary.
        (
            r#"
IFS= read -r line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"codexHome":"/tmp"}}'
exit 7
"#,
            "SESSION_START_FAILED",
        ),
        // thread/start result without a thread id.
        (
            r#"
IFS= read -r line
printf '%s\n' '{"id":1,"result":{"codexHome":"/tmp/codex-home"}}'
IFS= read -r line
printf '%s\n' '{"id":2,"result":{"thread":{"ephemeral":false},"model":"gpt-5.6-terra"}}'
sleep 1
"#,
            "thread id",
        ),
        // Model mismatch between admission and the started thread.
        (
            r#"
IFS= read -r line
printf '%s\n' '{"id":1,"result":{"codexHome":"/tmp/codex-home"}}'
IFS= read -r line
printf '%s\n' '{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-other"}}'
sleep 1
"#,
            "MODEL_MISMATCH",
        ),
        // Auth failure arrives as an error response to initialize.
        (
            r#"
IFS= read -r line
printf '%s\n' '{"id":1,"error":{"code":401,"message":"not logged in"}}'
sleep 1
"#,
            "SESSION_START_FAILED",
        ),
        // EOF while a request is pending.
        (
            r#"
IFS= read -r line
exit 0
"#,
            "SESSION_START_FAILED",
        ),
    ] {
        let workspace = codex_workspace();
        let scheduler =
            codex_scheduler(workspace.path(), harness_factory(&script, workspace.path()));
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for(workspace.path(), "bounded failure"),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let agent_id = submitted.agent_id.clone();
        assert!(
            scheduler.start_ready().is_err(),
            "start must fail closed for {marker}"
        );
        let task = await_terminal_task(&scheduler, &agent_id);
        assert_eq!(task.outcome, Some(TaskOutcome::Failed));
        let record = scheduler
            .last_error(&agent_id)
            .expect("failure record is persisted");
        assert!(
            record.contains(marker) || marker == "SESSION_START_FAILED",
            "record for {marker}: {record}"
        );
        assert_eq!(task.session_id, None);
    }
}

#[test]
fn interrupt_preserves_the_thread_identity_and_cancels_bounded() {
    let _guard = scripted_test_guard();
    let workspace = codex_workspace();
    let directory = workspace.path().to_owned();
    let echo = start_echo(&directory, PermissionMode::Plan);
    let script = format!(
        r#"
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{{"id":1,"result":{{"codexHome":"/tmp/codex-home"}}}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"{THREAD_ID}","ephemeral":false}},"model":"{MODEL}",{echo}}}}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"codex-turn-1","status":"inProgress"}}}}}}' \
  '{{"method":"turn/started","params":{{"threadId":"{THREAD_ID}","turn":{{"id":"codex-turn-1","status":"inProgress"}}}}}}'
while [ ! -f release ]; do sleep 0.01; done
IFS= read -r line
printf '%s\n' "$line" >> deliveries.jsonl
printf '%s\n' '{{"id":4,"result":{{}}}}' '{{"method":"turn/completed","params":{{"threadId":"{THREAD_ID}","turn":{{"id":"codex-turn-1","status":"interrupted","error":null}}}}}}'
while IFS= read -r line; do printf '%s\n' "$line" >> deliveries.jsonl; done
"#
    );
    let scheduler = codex_scheduler(&directory, harness_factory(&script, &directory));
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(&directory, "interrupt me"),
            Some(codex_admission(Some(MODEL))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    scheduler.start_ready().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        if matches!(task.turn_state, external_store::TurnState::Active) {
            break;
        }
        assert!(Instant::now() < deadline, "turn never became active");
        thread::sleep(Duration::from_millis(10));
    }
    std::fs::write(directory.join("release"), "").unwrap();
    scheduler.cancel_task(&agent_id).unwrap();
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Cancelled));
    assert_eq!(task.session_id.as_deref(), Some(THREAD_ID));
    let deliveries = std::fs::read_to_string(directory.join("deliveries.jsonl")).unwrap();
    let interrupt = deliveries
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|frame| frame["method"] == "turn/interrupt")
        .expect("turn/interrupt frame");
    assert_eq!(interrupt["params"]["threadId"], THREAD_ID);
    assert_eq!(interrupt["params"]["turnId"], "codex-turn-1");
}

#[test]
fn home_precedence_and_runtime_path_bounds() {
    // Configured home wins over the inherited environment.
    assert_eq!(
        resolve_codex_home(Some("/cfg/home"), Some("/inherited/home")),
        Some(Ok(PathBuf::from("/cfg/home")))
    );
    assert_eq!(
        resolve_codex_home(None, Some("/inherited/home")),
        Some(Ok(PathBuf::from("/inherited/home")))
    );
    assert_eq!(
        resolve_codex_home(Some("relative"), None),
        Some(Err("agents.codex.home must be absolute"))
    );
    assert_eq!(
        resolve_codex_home(None, Some("relative")),
        Some(Err("inherited CODEX_HOME must be absolute"))
    );
    // Neither source present rejects instead of falling back to ~/.codex.
    assert_eq!(resolve_codex_home(None, None), None);

    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("codex-runtime");
    std::fs::write(&runtime, b"runtime").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut mode = std::fs::metadata(&runtime).unwrap().permissions();
        mode.set_mode(0o755);
        std::fs::set_permissions(&runtime, mode).unwrap();
    }
    let launch = CodexLaunch::new(runtime.clone(), directory.path().join("home"));
    assert_eq!(launch.runtime_path(), runtime.as_path());
    assert_eq!(launch.home(), directory.path().join("home"));
    let script = directory.path().join("probe.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\n' \"$CODEX_HOME $1 $2 $3\" > args.txt\nsleep 5\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut mode = std::fs::metadata(&script).unwrap().permissions();
        mode.set_mode(0o755);
        std::fs::set_permissions(&script, mode).unwrap();
    }
    let launch = CodexLaunch::new(script.clone(), directory.path().join("home"));
    let sink: Arc<dyn LifecycleSink> = Arc::new(NoopSink);
    let owner = CodexRuntimeOwner::spawn(launch.command(directory.path()), sink).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(args) = std::fs::read_to_string(directory.path().join("args.txt")) {
            assert_eq!(
                args.trim(),
                format!(
                    "{} app-server --listen stdio://",
                    directory.path().join("home").display()
                )
            );
            break;
        }
        assert!(Instant::now() < deadline, "child never recorded its argv");
        thread::sleep(Duration::from_millis(10));
    }
    let terminal = owner.stop(Duration::from_secs(2));
    assert!(terminal_proves_process_group_reaped(&terminal));

    // A non-executable or relative runtime path is rejected.
    let plain = directory.path().join("plain.txt");
    std::fs::write(&plain, b"plain").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut mode = std::fs::metadata(&plain).unwrap().permissions();
        mode.set_mode(0o644);
        std::fs::set_permissions(&plain, mode).unwrap();
    }
    let sink: Arc<dyn LifecycleSink> = Arc::new(NoopSink);
    let bad = CodexLaunch::new(plain, directory.path().join("home"));
    let error = CodexRuntimeFactory::test_harness(Some(bad))
        .spawn(&codex_task_record(directory.path()), sink)
        .err()
        .expect("non-executable runtime must refuse the spawn");
    assert!(error.to_string().contains("absolute executable file"));
}

fn codex_task_record(directory: &Path) -> TaskRecord {
    let canonical = directory.canonicalize().unwrap();
    let prepared = external_core::GeneralTaskPreparer::new(Vec::new())
        .unwrap()
        .prepare_direct_submission(&manifest_for(&canonical, "launch"))
        .unwrap()
        .with_admission(codex_admission(Some(MODEL)))
        .unwrap();
    TaskRecord {
        agent_id: "launch-check".into(),
        repository: canonical.to_string_lossy().into_owned(),
        phase: TaskPhase::Queued,
        outcome: None,
        workspace_path: canonical.to_string_lossy().into_owned(),
        runtime_hash: None,
        prepared_launch_json: serde_json::to_string(&prepared).unwrap(),
        prepared_launch_sha256: prepared.prepared_sha256.clone(),
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
        turn_state: external_store::TurnState::Idle,
        process_identity: None,
        closed_at: None,
        reaped_at: None,
        created_at: 0,
    }
}

#[test]
fn routing_keeps_zcode_and_dsh_on_their_factories() {
    let workspace = codex_workspace();
    let zcode = crate::CommandRuntimeFactory::new(|_: &TaskRecord| {
        Err::<Command, _>(io::Error::other("zcode factory must not spawn"))
    });
    let factory = crate::dsh::RoutingRuntimeFactory::with_codex(
        zcode,
        crate::dsh::DshRuntimeFactory::closed(),
        CodexRuntimeFactory::closed(),
    );
    let sink: Arc<dyn LifecycleSink> = Arc::new(NoopSink);
    // No admission → legacy zcode route.
    let prepared = external_core::GeneralTaskPreparer::new(Vec::new())
        .unwrap()
        .prepare_direct_submission(&manifest_for(workspace.path(), "legacy"))
        .unwrap();
    let mut task = codex_task_record(workspace.path());
    task.prepared_launch_json = serde_json::to_string(&prepared).unwrap();
    task.prepared_launch_sha256 = prepared.prepared_sha256.clone();
    let error = factory
        .spawn(&task, Arc::clone(&sink))
        .err()
        .expect("legacy route must stay on the zcode factory");
    assert!(error.to_string().contains("zcode factory must not spawn"));
    // dsh admission → dsh factory (closed gate).
    let dsh_admission = AdmissionIdentity {
        agent: "dsh".into(),
        config_revision: 1,
        adapter_version: "test".into(),
        model: None,
        model_source: "native".into(),
        effort: None,
    };
    let prepared = external_core::GeneralTaskPreparer::new(Vec::new())
        .unwrap()
        .prepare_direct_submission(&manifest_for(workspace.path(), "dsh route"))
        .unwrap()
        .with_admission(dsh_admission)
        .unwrap();
    let mut task = codex_task_record(workspace.path());
    task.prepared_launch_json = serde_json::to_string(&prepared).unwrap();
    task.prepared_launch_sha256 = prepared.prepared_sha256.clone();
    let error = factory
        .spawn(&task, Arc::clone(&sink))
        .err()
        .expect("dsh admission must stay on the dsh factory");
    assert!(error.to_string().contains("dsh spawn gate is closed"));
    // codex admission → codex factory (closed gate).
    let error = factory
        .spawn(&codex_task_record(workspace.path()), sink)
        .err()
        .expect("codex admission must route to the codex factory");
    assert!(error.to_string().contains("codex spawn gate is closed"));
}

/// Serialize tests that mutate process-global environment variables.
fn env_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn executable_script(directory: &Path, name: &str, body: &str) -> PathBuf {
    let script = directory.join(name);
    std::fs::write(&script, format!("#!/bin/sh\n{body}")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut mode = std::fs::metadata(&script).unwrap().permissions();
        mode.set_mode(0o755);
        std::fs::set_permissions(&script, mode).unwrap();
    }
    script
}

#[test]
fn production_enabled_gate_spawns_a_real_executable_environment() {
    let _env = env_test_guard();
    let directory = tempfile::tempdir().unwrap();
    let runtime = executable_script(directory.path(), "codex-runtime", "sleep 5\n");
    let home = directory.path().join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    let previous_runtime = std::env::var_os("CODEX_RUNTIME_PATH");
    let previous_home = std::env::var_os("CODEX_HOME");
    std::env::set_var("CODEX_RUNTIME_PATH", &runtime);
    std::env::set_var("CODEX_HOME", &home);

    // F01 regression: a valid executable runtime passes the production
    // environment predicate and the Enabled gate actually spawns it.
    let sink: Arc<dyn LifecycleSink> = Arc::new(NoopSink);
    let spawned = CodexRuntimeFactory::enabled()
        .spawn(&codex_task_record(directory.path()), Arc::clone(&sink))
        .expect("a valid executable runtime must pass the production gate");
    let terminal = spawned.stop(Duration::from_secs(2));
    assert!(terminal_proves_process_group_reaped(&terminal));

    let plain = directory.path().join("plain.txt");
    std::fs::write(&plain, b"plain").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut mode = std::fs::metadata(&plain).unwrap().permissions();
        mode.set_mode(0o644);
        std::fs::set_permissions(&plain, mode).unwrap();
    }
    let spawn_refusing = |marker: &str| {
        let error = CodexRuntimeFactory::enabled()
            .spawn(&codex_task_record(directory.path()), Arc::clone(&sink))
            .err()
            .unwrap_or_else(|| panic!("{marker} must fail closed"));
        assert!(
            error.to_string().contains(marker),
            "expected {marker} in {error}"
        );
    };
    std::env::set_var("CODEX_RUNTIME_PATH", &plain);
    spawn_refusing("absolute executable file");
    std::env::set_var("CODEX_RUNTIME_PATH", "relative/codex");
    spawn_refusing("absolute executable file");
    std::env::remove_var("CODEX_RUNTIME_PATH");
    spawn_refusing("CODEX_RUNTIME_PATH is unavailable");
    std::env::set_var("CODEX_RUNTIME_PATH", &runtime);
    std::env::remove_var("CODEX_HOME");
    spawn_refusing("CODEX_HOME is unconfigured");
    std::env::set_var("CODEX_HOME", "relative-home");
    spawn_refusing("CODEX_HOME must be absolute");

    match previous_runtime {
        Some(value) => std::env::set_var("CODEX_RUNTIME_PATH", value),
        None => std::env::remove_var("CODEX_RUNTIME_PATH"),
    }
    match previous_home {
        Some(value) => std::env::set_var("CODEX_HOME", value),
        None => std::env::remove_var("CODEX_HOME"),
    }
}

#[test]
fn terminal_send_never_overwrites_a_committed_close_or_cancel() {
    let _guard = scripted_test_guard();
    let workspace = codex_workspace();
    let scheduler = codex_scheduler(
        workspace.path(),
        harness_factory(
            &happy_turn(workspace.path(), PermissionMode::Plan),
            workspace.path(),
        ),
    );
    for close in [true, false] {
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for(
                    workspace.path(),
                    if close { "close me" } else { "cancel me" },
                ),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let agent_id = submitted.agent_id.clone();
        scheduler.start_ready().unwrap();
        await_result(&scheduler, &agent_id);
        let before = await_terminal_task(&scheduler, &agent_id);
        assert_eq!(before.outcome, Some(TaskOutcome::Completed));
        assert!(before.reaped_at.is_some(), "completed task must be reaped");

        let phase = if close {
            scheduler.close_task(&agent_id)
        } else {
            scheduler.cancel_task(&agent_id)
        }
        .unwrap();
        assert_eq!(phase, TaskPhase::Terminal);
        let error = scheduler
            .queue_message(&agent_id, "post-close-msg", "nope")
            .unwrap_err();
        assert!(error.to_string().contains("TERMINAL_SEND_UNSUPPORTED"));
        let after = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        assert_eq!(after.phase, TaskPhase::Terminal);
        assert_eq!(after.outcome, Some(TaskOutcome::Completed));
        if close {
            assert!(after.close_requested);
            assert!(after.closed_at.is_some());
        } else {
            assert!(after.stop_requested);
        }
        assert!(scheduler
            .store()
            .message("post-close-msg")
            .unwrap()
            .is_none());
    }
}

#[test]
fn store_requeue_refuses_a_close_committed_after_the_scheduler_read() {
    let _guard = scripted_test_guard();
    let workspace = codex_workspace();
    let scheduler = codex_scheduler(
        workspace.path(),
        harness_factory(
            &happy_turn(workspace.path(), PermissionMode::Plan),
            workspace.path(),
        ),
    );
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(workspace.path(), "close race"),
            Some(codex_admission(Some(MODEL))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    scheduler.start_ready().unwrap();
    await_result(&scheduler, &agent_id);
    await_terminal_task(&scheduler, &agent_id);
    let store = scheduler.store();

    // The scheduler already read an eligible snapshot; a close commits
    // inside the eligibility/requeue window. Only the shared
    // transaction can still refuse the resume.
    store.request_close(&agent_id).unwrap();
    let requeued = store
        .requeue_task_for_resume_with_message(&agent_id, "race-msg", "content")
        .unwrap();
    assert!(!requeued, "a committed close must refuse the requeue");
    let task = store.get_task(&agent_id).unwrap().unwrap();
    assert_eq!(task.phase, TaskPhase::Terminal);
    assert!(task.close_requested);
    assert!(task.closed_at.is_some());
    assert!(store.message("race-msg").unwrap().is_none());
}

#[test]
fn concurrent_terminal_send_and_close_never_lose_the_close() {
    let _guard = scripted_test_guard();
    let workspace = codex_workspace();
    let scheduler = codex_scheduler(
        workspace.path(),
        harness_factory(
            &happy_turn(workspace.path(), PermissionMode::Plan),
            workspace.path(),
        ),
    );
    const RACES: usize = 4;
    let mut agents = Vec::new();
    for _ in 0..RACES {
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for(workspace.path(), "race base"),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let agent_id = submitted.agent_id.clone();
        scheduler.start_ready().unwrap();
        await_result(&scheduler, &agent_id);
        await_terminal_task(&scheduler, &agent_id);
        agents.push(agent_id);
    }
    for (index, agent_id) in agents.iter().enumerate() {
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let close_barrier = Arc::clone(&barrier);
        let send_scheduler = scheduler.clone();
        let close_scheduler = scheduler.clone();
        let send_agent = agent_id.clone();
        let close_agent = agent_id.clone();
        let message_id = format!("race-msg-{index}");
        let sender_message = message_id.clone();
        let send_handle = thread::spawn(move || {
            barrier.wait();
            send_scheduler.queue_message(&send_agent, &sender_message, "follow-up")
        });
        let close_handle = thread::spawn(move || {
            close_barrier.wait();
            close_scheduler.close_task(&close_agent)
        });
        let send_outcome = send_handle.join().unwrap();
        close_handle.join().unwrap().unwrap();

        // Whichever side wins, the committed close must survive: the
        // requeue transaction may never clear close evidence.
        let task = scheduler.store().get_task(agent_id).unwrap().unwrap();
        let close_survived =
            task.close_requested || task.closed_at.is_some() || task.phase == TaskPhase::Cancelling;
        assert!(
            close_survived,
            "resume overwrote a committed close for {agent_id}: {task:?}"
        );
        match send_outcome {
            Ok(disposition) => {
                assert_eq!(disposition, crate::MessageDisposition::Queued);
                assert!(scheduler.store().message(&message_id).unwrap().is_some());
            }
            Err(error) => {
                assert!(
                    error.to_string().contains("TERMINAL_SEND_UNSUPPORTED"),
                    "unexpected resume error: {error}"
                );
            }
        }
    }
}

#[test]
fn terminal_send_keeps_old_process_identity_when_reap_is_unproven() {
    let workspace = codex_workspace();
    let scheduler = codex_scheduler(
        workspace.path(),
        harness_factory(
            &happy_turn(workspace.path(), PermissionMode::Plan),
            workspace.path(),
        ),
    );
    let store = scheduler.store();
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(workspace.path(), "orphaned codex task"),
            Some(codex_admission(Some(MODEL))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    let claim = store
        .claim_next("orphan-owner", usize::MAX, 1)
        .unwrap()
        .unwrap();
    assert_eq!(claim.task.agent_id, agent_id);
    store
        .mark_session_running(
            &agent_id,
            claim.owner_epoch,
            "runtime-orphan",
            Some(&external_store::StoredProcessIdentity {
                pid: 4242,
                process_group_id: 4242,
                uid: 1000,
                start_token: "start-token-orphan".into(),
            }),
            Some(THREAD_ID),
            Some(external_store::TurnState::Idle),
        )
        .unwrap();
    store
        .store_task_result(
            &agent_id,
            &external_store::TaskResult {
                outcome: TaskOutcome::Completed,
                final_text: "orphaned".into(),
                partial: false,
            },
        )
        .unwrap();
    store
        .transition_terminal(
            &agent_id,
            claim.owner_epoch,
            &external_store::TerminalUpdate {
                outcome: TaskOutcome::Completed,
                failure_code: None,
                failure_message: None,
            },
        )
        .unwrap();
    let task = store.get_task(&agent_id).unwrap().unwrap();
    let identity = task.process_identity.clone().expect("persisted identity");
    assert_eq!(task.reaped_at, None, "fixture models an unproven reap");

    // The scheduler precheck passes (codex, session, completed, no
    // close), so only the transaction's reap proof refuses this.
    let error = scheduler
        .queue_message(&agent_id, "orphan-msg", "nope")
        .unwrap_err();
    assert!(error.to_string().contains("TERMINAL_SEND_UNSUPPORTED"));
    let after = store.get_task(&agent_id).unwrap().unwrap();
    assert_eq!(after.phase, TaskPhase::Terminal);
    let kept = after.process_identity.clone().expect("old identity kept");
    assert_eq!(kept.pid, identity.pid);
    assert_eq!(kept.process_group_id, identity.process_group_id);
    assert_eq!(kept.start_token, identity.start_token);
    assert!(store.message("orphan-msg").unwrap().is_none());

    // Once the reap is proven, the same durable state admits the resume
    // and only then may the requeue clear the old identity.
    store.reap_task(&agent_id).unwrap();
    assert!(store
        .requeue_task_for_resume_with_message(&agent_id, "orphan-msg", "nope")
        .unwrap());
    assert_eq!(
        store.get_task(&agent_id).unwrap().unwrap().phase,
        TaskPhase::Queued
    );
}

#[test]
fn late_turn_traffic_never_pollutes_the_current_turn_result() {
    let _guard = scripted_test_guard();
    let workspace = codex_workspace();
    let echo = start_echo(workspace.path(), PermissionMode::Plan);
    let script = format!(
        r#"
IFS= read -r line
printf '%s\n' '{{"id":1,"result":{{"codexHome":"/tmp/codex-home"}}}}'
IFS= read -r line
printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"codex-thread-1","ephemeral":false}},"model":"gpt-5.6-terra",{echo}}}}}'
IFS= read -r line
IFS= read -r line
printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"codex-turn-1","status":"inProgress"}}}}}}' \
  '{{"method":"turn/started","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-1","status":"inProgress"}}}}}}' \
  '{{"method":"item/agentMessage/delta","params":{{"threadId":"codex-thread-1","turnId":"codex-turn-1","itemId":"msg_1","delta":"GOOD_"}}}}' \
  '{{"method":"turn/started","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-0","status":"inProgress"}}}}}}' \
  '{{"method":"item/agentMessage/delta","params":{{"threadId":"codex-thread-1","turnId":"codex-turn-0","itemId":"stale_1","delta":"STALE_DELTA"}}}}' \
  '{{"method":"item/completed","params":{{"threadId":"codex-thread-1","turnId":"codex-turn-0","item":{{"type":"agentMessage","id":"stale_1","text":"STALE_ITEM"}}}}}}' \
  '{{"method":"error","params":{{"threadId":"codex-thread-1","turnId":"codex-turn-0","error":{{"message":"old capacity failure","codexErrorInfo":"serverOverloaded"}}}}}}' \
  '{{"method":"turn/completed","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-0","status":"completed","error":null}}}}}}' \
  '{{"method":"item/agentMessage/delta","params":{{"threadId":"codex-thread-1","itemId":"msg_1","delta":"MISSING_TURN_ID"}}}}' \
  '{{"method":"item/completed","params":{{"turnId":"codex-turn-1","item":{{"type":"agentMessage","id":"no_thread","text":"NO_THREAD_ID"}}}}}}' \
  '{{"method":"item/agentMessage/delta","params":{{"threadId":"codex-thread-1","turnId":"codex-turn-1","itemId":"msg_1","delta":"OK"}}}}' \
  '{{"method":"item/completed","params":{{"threadId":"codex-thread-1","turnId":"codex-turn-1","item":{{"type":"agentMessage","id":"msg_1"}}}}}}' \
  '{{"method":"turn/completed","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-1","status":"completed","error":null}}}}}}'
while IFS= read -r line; do :; done
"#
    );
    let scheduler = codex_scheduler(workspace.path(), harness_factory(&script, workspace.path()));
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(workspace.path(), "stale traffic probe"),
            Some(codex_admission(Some(MODEL))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    scheduler.start_ready().unwrap();
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Completed);
    assert_eq!(result.result.final_text, "GOOD_OK");
    await_terminal_task(&scheduler, &agent_id);
    assert!(
        scheduler.last_error(&agent_id).is_none(),
        "stale traffic must not label a failure: {:?}",
        scheduler.last_error(&agent_id)
    );
}

#[test]
fn turn_start_response_and_notification_must_name_the_same_turn() {
    let _guard = scripted_test_guard();
    let workspace = codex_workspace();
    let echo = start_echo(workspace.path(), PermissionMode::Plan);
    let script = format!(
        r#"
IFS= read -r line
printf '%s\n' '{{"id":1,"result":{{"codexHome":"/tmp/codex-home"}}}}'
IFS= read -r line
printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"codex-thread-1","ephemeral":false}},"model":"gpt-5.6-terra",{echo}}}}}'
IFS= read -r line
IFS= read -r line
printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"codex-turn-a","status":"inProgress"}}}}}}' \
  '{{"method":"turn/started","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-b","status":"inProgress"}}}}}}'
sleep 1
"#
    );
    let scheduler = codex_scheduler(workspace.path(), harness_factory(&script, workspace.path()));
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(workspace.path(), "mismatched turn ids"),
            Some(codex_admission(Some(MODEL))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    assert!(
        scheduler.start_ready().is_err(),
        "a start response that disagrees with the started turn must fail closed"
    );
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Failed));
    let record = scheduler.last_error(&agent_id).expect("failure record");
    assert!(
        record.contains("does not match the started turn"),
        "record: {record}"
    );
}

#[test]
fn turn_start_response_without_a_turn_id_fails_closed() {
    let _guard = scripted_test_guard();
    let workspace = codex_workspace();
    // The start response omits its turn id; even a well-formed started
    // notification cannot reconcile an unacknowledged turn.
    let echo = start_echo(workspace.path(), PermissionMode::Plan);
    let script = format!(
        r#"
IFS= read -r line
printf '%s\n' '{{"id":1,"result":{{"codexHome":"/tmp/codex-home"}}}}'
IFS= read -r line
printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"codex-thread-1","ephemeral":false}},"model":"gpt-5.6-terra",{echo}}}}}'
IFS= read -r line
IFS= read -r line
printf '%s\n' '{{"id":3,"result":{{"turn":{{"status":"inProgress"}}}}}}' \
  '{{"method":"turn/started","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-1","status":"inProgress"}}}}}}'
sleep 1
"#
    );
    let scheduler = codex_scheduler(workspace.path(), harness_factory(&script, workspace.path()));
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(workspace.path(), "anonymous turn"),
            Some(codex_admission(Some(MODEL))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    assert!(
        scheduler.start_ready().is_err(),
        "a start response without a turn id must fail closed"
    );
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Failed));
    let record = scheduler.last_error(&agent_id).expect("failure record");
    assert!(
        record.contains("missing a bounded turn id"),
        "record: {record}"
    );
}

#[test]
fn completed_turn_cannot_be_reopened_by_late_or_duplicate_started() {
    let _guard = scripted_test_guard();
    let workspace = codex_workspace();
    // After the turn completes, the server replays a duplicate started
    // for the finished turn, a stale started for an older turn, late
    // turn-scoped traffic, and a duplicate completion. None of it may
    // reactivate the retired turn or touch the stored result.
    let echo = start_echo(workspace.path(), PermissionMode::Plan);
    let script = format!(
        r#"
IFS= read -r line
printf '%s\n' '{{"id":1,"result":{{"codexHome":"/tmp/codex-home"}}}}'
IFS= read -r line
printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"codex-thread-1","ephemeral":false}},"model":"gpt-5.6-terra",{echo}}}}}'
IFS= read -r line
IFS= read -r line
printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"codex-turn-1","status":"inProgress"}}}}}}' \
  '{{"method":"turn/started","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-1","status":"inProgress"}}}}}}' \
  '{{"method":"item/agentMessage/delta","params":{{"threadId":"codex-thread-1","turnId":"codex-turn-1","itemId":"msg_1","delta":"CODEX_OK"}}}}' \
  '{{"method":"item/completed","params":{{"threadId":"codex-thread-1","turnId":"codex-turn-1","item":{{"type":"agentMessage","id":"msg_1","text":"CODEX_OK"}}}}}}' \
  '{{"method":"turn/completed","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-1","status":"completed","error":null}}}}}}' \
  '{{"method":"turn/started","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-1","status":"inProgress"}}}}}}' \
  '{{"method":"turn/started","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-0","status":"inProgress"}}}}}}' \
  '{{"method":"item/agentMessage/delta","params":{{"threadId":"codex-thread-1","turnId":"codex-turn-1","itemId":"msg_1","delta":"LATE_DELTA"}}}}' \
  '{{"method":"item/completed","params":{{"threadId":"codex-thread-1","turnId":"codex-turn-1","item":{{"type":"agentMessage","id":"msg_1","text":"LATE_ITEM"}}}}}}' \
  '{{"method":"turn/completed","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-1","status":"completed","error":null}}}}}}'
while IFS= read -r line; do :; done
"#
    );
    let scheduler = codex_scheduler(workspace.path(), harness_factory(&script, workspace.path()));
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(workspace.path(), "late replay probe"),
            Some(codex_admission(Some(MODEL))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    scheduler.start_ready().unwrap();
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Completed);
    assert_eq!(
        result.result.final_text, "CODEX_OK",
        "late traffic must not rewrite the completed turn's result"
    );
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Completed));
    assert_eq!(task.session_id.as_deref(), Some(THREAD_ID));
    assert!(
        scheduler.last_error(&agent_id).is_none(),
        "late replays must not label a failure: {:?}",
        scheduler.last_error(&agent_id)
    );
}

struct TwoPhaseFactory {
    first: Mutex<Option<CodexRuntimeFactory>>,
    second: CodexRuntimeFactory,
}
impl RuntimeFactory for TwoPhaseFactory {
    fn spawn(
        &self,
        task: &TaskRecord,
        sink: Arc<dyn LifecycleSink>,
    ) -> io::Result<Arc<dyn ManagedRuntime>> {
        let mut first = self.first.lock().unwrap();
        match first.take() {
            Some(factory) => factory.spawn(task, sink),
            None => self.second.spawn(task, sink),
        }
    }
}

fn resume_phase_factory(directory: &Path, script: &str, child: &str) -> CodexRuntimeFactory {
    let path = directory.join(child);
    std::fs::write(&path, format!("#!/bin/sh\n{script}")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut mode = std::fs::metadata(&path).unwrap().permissions();
        mode.set_mode(0o755);
        std::fs::set_permissions(&path, mode).unwrap();
    }
    CodexRuntimeFactory::test_harness(Some(CodexLaunch::new(path, directory.join("codex-home"))))
}

#[test]
fn resume_fails_closed_without_a_confirmed_plan_only_posture() {
    let _guard = scripted_test_guard();
    for (resume_result, marker) in [
        // No posture fields at all: unverifiable means refused.
        (
            r#"{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-5.6-terra"}}"#,
            "read-only",
        ),
        // A write-capable sandbox object is never accepted on resume.
        (
            r#"{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-5.6-terra","sandbox":{"type":"workspaceWrite","networkAccess":false},"approvalPolicy":"never","cwd":"/elsewhere"}}"#,
            "read-only",
        ),
        // The request-time string preset is not the resolved posture
        // the live probe returns; it stays unconfirmed.
        (
            r#"{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-5.6-terra","sandbox":"read-only","approvalPolicy":"never","cwd":"/elsewhere"}}"#,
            "read-only",
        ),
        // A network-capable sandbox diverges from the observed
        // read-only posture and fails closed.
        (
            r#"{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-5.6-terra","sandbox":{"type":"readOnly","networkAccess":true},"approvalPolicy":"never","cwd":"/elsewhere"}}"#,
            "read-only",
        ),
        // An approval policy other than never.
        (
            r#"{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-5.6-terra","sandbox":{"type":"readOnly","networkAccess":false},"approvalPolicy":"on-request","cwd":"/elsewhere"}}"#,
            "never",
        ),
        // A thread rooted somewhere other than the task workspace.
        (
            r#"{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-5.6-terra","sandbox":{"type":"readOnly","networkAccess":false},"approvalPolicy":"never","cwd":"/elsewhere"}}"#,
            "task workspace",
        ),
    ] {
        let workspace = codex_workspace();
        let directory = workspace.path().to_owned();
        let resume_script = format!(
            r#"
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
printf '%s\n' '{{"id":1,"result":{{"codexHome":"/tmp/codex-home"}}}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
printf '%s\n' '{resume_result}'
IFS= read -r line
sleep 1
"#
        );
        let store = Arc::new(external_store::Store::open(directory.join("state.sqlite")).unwrap());
        let scheduler = Scheduler::new(
            "codex-resume-f05",
            store,
            Arc::new(TwoPhaseFactory {
                first: Mutex::new(Some(harness_factory(
                    &happy_turn(&directory, PermissionMode::Plan),
                    &directory,
                ))),
                second: resume_phase_factory(&directory, &resume_script, "codex-resume-fail.sh"),
            }),
            SchedulerConfig {
                bootstrap_timeout: Duration::from_secs(30),
                control_timeout: Duration::from_secs(30),
                ..SchedulerConfig::default()
            },
        )
        .unwrap();
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for(&directory, "first turn"),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let agent_id = submitted.agent_id.clone();
        scheduler.start_ready().unwrap();
        let first_result = await_result(&scheduler, &agent_id);
        assert_eq!(first_result.result.final_text, "CODEX_OK");
        let deadline = Instant::now() + Duration::from_secs(5);
        while scheduler.active_count() > 0 {
            assert!(
                Instant::now() < deadline,
                "first runtime was never released"
            );
            thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(
            scheduler
                .queue_message(&agent_id, "fail-msg", "follow-up question")
                .unwrap(),
            crate::MessageDisposition::Queued
        );
        assert!(
            scheduler.start_ready().is_err(),
            "an unconfirmed plan-only posture must fail the resume closed"
        );
        let task = await_terminal_task(&scheduler, &agent_id);
        assert_eq!(task.outcome, Some(TaskOutcome::Failed));
        assert_eq!(
            task.session_id.as_deref(),
            Some(THREAD_ID),
            "the persisted thread identity stays durable for a retry"
        );
        let record = scheduler.last_error(&agent_id).expect("failure record");
        assert!(record.contains(marker), "record: {record}");

        // The resumed process really did attempt thread/resume with the
        // persisted identity before failing closed.
        let deliveries = std::fs::read_to_string(directory.join("deliveries-resume.jsonl"))
            .expect("the resumed process logs its own frames");
        let resume = deliveries
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|frame| frame["method"] == "thread/resume")
            .expect("thread/resume frame");
        assert_eq!(resume["params"]["threadId"], THREAD_ID);
        let turn_starts = deliveries
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|frame| frame["method"] == "turn/start")
            .count();
        assert_eq!(turn_starts, 0, "no turn may start on an unverified resume");
    }
}

/// A scripted resume phase for a yolo task: the daemon resumes the
/// persisted thread and drives one follow-up turn. `resume_result` is the
/// id2 thread/resume result the fake server returns.
fn yolo_resume_case(directory: &Path, resume_result: &str, child: &str) -> CodexRuntimeFactory {
    let script = format!(
        r#"
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
printf '%s\n' '{{"id":1,"result":{{"codexHome":"/tmp/codex-home"}}}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
printf '%s\n' '{resume_result}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"codex-turn-2","status":"inProgress"}}}}}}' \
  '{{"method":"turn/started","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-2","status":"inProgress"}}}}}}' \
  '{{"method":"item/completed","params":{{"threadId":"codex-thread-1","turnId":"codex-turn-2","item":{{"type":"agentMessage","id":"msg_2","text":"RESUMED_OK"}}}}}}' \
  '{{"method":"turn/completed","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-2","status":"completed","error":null}}}}}}'
while IFS= read -r line; do printf '%s\n' "$line" >> deliveries-resume.jsonl; done
"#
    );
    resume_phase_factory(directory, &script, child)
}

#[test]
fn yolo_resume_accepts_both_confirmed_danger_full_access_postures() {
    let _guard = scripted_test_guard();
    // The live probe on codex-cli 0.154.0 resolved a persisted
    // danger-full-access rollout as a narrowed workspace-write object;
    // the faithful dangerFullAccess shape is accepted equally.
    for (index, sandbox) in [
        r#""sandbox":{"type":"dangerFullAccess"}"#,
        r#""sandbox":{"excludeSlashTmp":false,"excludeTmpdirEnvVar":false,"networkAccess":false,"type":"workspaceWrite","writableRoots":[]}"#,
    ]
    .iter()
    .enumerate()
    {
        let workspace = codex_workspace();
        let directory = workspace.path().to_owned();
        let resume_result = format!(
            r#"{{"id":2,"result":{{"thread":{{"id":"codex-thread-1","ephemeral":false}},"model":"gpt-5.6-terra",{sandbox},"approvalPolicy":"never","cwd":"{}"}}}}"#,
            directory.to_string_lossy()
        );
        let store =
            Arc::new(external_store::Store::open(directory.join("state.sqlite")).unwrap());
        let scheduler = Scheduler::new(
            "codex-resume-yolo-ok",
            store,
            Arc::new(TwoPhaseFactory {
                first: Mutex::new(Some(harness_factory(
                    &happy_turn(&directory, PermissionMode::Yolo),
                    &directory,
                ))),
                second: yolo_resume_case(
                    &directory,
                    &resume_result,
                    &format!("codex-resume-yolo-ok-{index}.sh"),
                ),
            }),
            SchedulerConfig {
                bootstrap_timeout: Duration::from_secs(30),
                control_timeout: Duration::from_secs(30),
                ..SchedulerConfig::default()
            },
        )
        .unwrap();
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for_mode(&directory, "first turn", PermissionMode::Yolo),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let agent_id = submitted.agent_id.clone();
        scheduler.start_ready().unwrap();
        let first_result = await_result(&scheduler, &agent_id);
        assert_eq!(first_result.result.final_text, "CODEX_OK");
        let deadline = Instant::now() + Duration::from_secs(5);
        while scheduler.active_count() > 0 {
            assert!(
                Instant::now() < deadline,
                "first runtime was never released"
            );
            thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(
            scheduler
                .queue_message(&agent_id, "yolo-resume-msg", "follow-up question")
                .unwrap(),
            crate::MessageDisposition::Queued
        );
        scheduler.start_ready().unwrap();
        let resumed = await_result(&scheduler, &agent_id);
        assert_eq!(resumed.result.outcome, TaskOutcome::Completed);
        assert_eq!(resumed.result.final_text, "RESUMED_OK");

        let deliveries = std::fs::read_to_string(directory.join("deliveries-resume.jsonl"))
            .expect("the resumed process logs its own frames");
        let resume = deliveries
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|frame| frame["method"] == "thread/resume")
            .expect("thread/resume frame");
        assert_eq!(resume["params"]["threadId"], THREAD_ID);
        let turn_starts = deliveries
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|frame| frame["method"] == "turn/start")
            .count();
        assert_eq!(turn_starts, 1, "the follow-up turn starts exactly once");
    }
}

#[test]
fn write_modes_resume_with_confirmed_workspace_write() {
    let _guard = scripted_test_guard();
    for mode in [PermissionMode::Build, PermissionMode::Edit] {
        let sandbox = r#""sandbox":{"excludeSlashTmp":false,"excludeTmpdirEnvVar":false,"networkAccess":false,"type":"workspaceWrite","writableRoots":[]}"#;
        let workspace = codex_workspace();
        let directory = workspace.path().to_owned();
        let resume_result = format!(
            r#"{{"id":2,"result":{{"thread":{{"id":"codex-thread-1","ephemeral":false}},"model":"gpt-5.6-terra",{sandbox},"approvalPolicy":"never","cwd":"{}"}}}}"#,
            directory.to_string_lossy()
        );
        let store = Arc::new(external_store::Store::open(directory.join("state.sqlite")).unwrap());
        let scheduler = Scheduler::new(
            "codex-resume-write-ok",
            store,
            Arc::new(TwoPhaseFactory {
                first: Mutex::new(Some(harness_factory(
                    &happy_turn(&directory, mode),
                    &directory,
                ))),
                second: yolo_resume_case(&directory, &resume_result, "codex-resume-write-ok.sh"),
            }),
            SchedulerConfig {
                bootstrap_timeout: Duration::from_secs(30),
                control_timeout: Duration::from_secs(30),
                ..SchedulerConfig::default()
            },
        )
        .unwrap();
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for_mode(&directory, "first turn", mode),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let agent_id = submitted.agent_id.clone();
        scheduler.start_ready().unwrap();
        let first_result = await_result(&scheduler, &agent_id);
        assert_eq!(first_result.result.final_text, "CODEX_OK");
        let deadline = Instant::now() + Duration::from_secs(5);
        while scheduler.active_count() > 0 {
            assert!(
                Instant::now() < deadline,
                "first runtime was never released"
            );
            thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(
            scheduler
                .queue_message(&agent_id, "write-resume-msg", "follow-up question")
                .unwrap(),
            crate::MessageDisposition::Queued
        );
        scheduler.start_ready().unwrap();
        let resumed = await_result(&scheduler, &agent_id);
        assert_eq!(resumed.result.outcome, TaskOutcome::Completed);
        assert_eq!(resumed.result.final_text, "RESUMED_OK");

        let deliveries = std::fs::read_to_string(directory.join("deliveries-resume.jsonl"))
            .expect("the resumed process logs its own frames");
        let resume = deliveries
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|frame| frame["method"] == "thread/resume")
            .expect("thread/resume frame");
        assert_eq!(resume["params"]["threadId"], THREAD_ID);
        // Resume confirms the persisted posture from the result; it must not
        // override it through request parameters.
        assert!(resume["params"].get("sandbox").is_none());
        assert!(resume["params"].get("approvalPolicy").is_none());
        let turn_starts = deliveries
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|frame| frame["method"] == "turn/start")
            .count();
        assert_eq!(turn_starts, 1, "the follow-up turn starts exactly once");
    }
}

/// A scripted resume phase with a controllable `reasoningEffort` echo on
/// the id2 thread/resume result: `Some(effort)` pins the echoed token
/// (equal to or diverging from the admitted request), `None` is a resume
/// result that does not carry the field at all — the three states the
/// resume-side effort confirmation has to distinguish.
fn effort_resume_case(
    directory: &Path,
    effort_echo: Option<&str>,
    child: &str,
) -> CodexRuntimeFactory {
    let echo = start_echo(directory, PermissionMode::Plan);
    let effort_fields = effort_echo
        .map(|effort| format!(r#","reasoningEffort":"{effort}""#))
        .unwrap_or_default();
    let script = format!(
        r#"
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
printf '%s\n' '{{"id":1,"result":{{"codexHome":"/tmp/codex-home"}}}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"codex-thread-1","ephemeral":false}},"model":"gpt-5.6-terra",{echo}{effort_fields}}}}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"codex-turn-2","status":"inProgress"}}}}}}' \
  '{{"method":"turn/started","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-2","status":"inProgress"}}}}}}' \
  '{{"method":"item/completed","params":{{"threadId":"codex-thread-1","turnId":"codex-turn-2","item":{{"type":"agentMessage","id":"msg_2","text":"RESUMED_OK"}}}}}}' \
  '{{"method":"turn/completed","params":{{"threadId":"codex-thread-1","turn":{{"id":"codex-turn-2","status":"completed","error":null}}}}}}'
while IFS= read -r line; do printf '%s\n' "$line" >> deliveries-resume.jsonl; done
"#
    );
    resume_phase_factory(directory, &script, child)
}

#[test]
fn resume_keeps_the_admitted_effort_for_followup_turns() {
    let _guard = scripted_test_guard();
    // The admitted effort survives the process boundary. The resume echo is
    // meaningful here unlike the start echo (OBSERVED on codex-cli 0.154.0:
    // a persistent thread resumed after a low turn echoes "low"): it names
    // the effort the previous turn actually ran with, so an equal echo
    // confirms the admission and the follow-up turn keeps naming the
    // admitted effort instead of falling back to low.
    let workspace = codex_workspace();
    let directory = workspace.path().to_owned();
    let store = Arc::new(external_store::Store::open(directory.join("state.sqlite")).unwrap());
    let scheduler = Scheduler::new(
        "codex-resume-effort-ok",
        store,
        Arc::new(TwoPhaseFactory {
            first: Mutex::new(Some(harness_factory(
                &happy_turn_with_effort(
                    &start_echo(&directory, PermissionMode::Plan),
                    Some("high"),
                ),
                &directory,
            ))),
            second: effort_resume_case(&directory, Some("high"), "codex-resume-effort-ok.sh"),
        }),
        SchedulerConfig {
            bootstrap_timeout: Duration::from_secs(30),
            control_timeout: Duration::from_secs(30),
            ..SchedulerConfig::default()
        },
    )
    .unwrap();
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(&directory, "first turn"),
            Some(codex_admission_with_effort(Some(MODEL), Some("high"))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    scheduler.start_ready().unwrap();
    let first_result = await_result(&scheduler, &agent_id);
    assert_eq!(first_result.result.final_text, "CODEX_OK");
    let first_turn_start = std::fs::read_to_string(directory.join("deliveries.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|frame| frame["method"] == "turn/start")
        .expect("first turn/start frame");
    assert_eq!(first_turn_start["params"]["effort"], "high");
    let deadline = Instant::now() + Duration::from_secs(5);
    while scheduler.active_count() > 0 {
        assert!(
            Instant::now() < deadline,
            "first runtime was never released"
        );
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        scheduler
            .queue_message(&agent_id, "effort-resume-msg", "follow-up question")
            .unwrap(),
        crate::MessageDisposition::Queued
    );
    scheduler.start_ready().unwrap();
    let resumed = await_result(&scheduler, &agent_id);
    assert_eq!(resumed.result.outcome, TaskOutcome::Completed);
    assert_eq!(resumed.result.final_text, "RESUMED_OK");

    let deliveries = std::fs::read_to_string(directory.join("deliveries-resume.jsonl"))
        .expect("the resumed process logs its own frames");
    let followup_turn_start = deliveries
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|frame| frame["method"] == "turn/start")
        .expect("follow-up turn/start frame");
    assert_eq!(
        followup_turn_start["params"]["effort"], "high",
        "a follow-up turn must not fall back to low"
    );
}

#[test]
fn resume_without_an_effort_echo_proceeds_with_a_diagnostic() {
    let _guard = scripted_test_guard();
    // A resume result that carries no reasoningEffort echo at all (a thread
    // resumed before any turn ran): nothing is compared against anything
    // and no echo is fabricated — the admission proceeds with a diagnostic
    // note and the follow-up turn still names the admitted effort.
    let workspace = codex_workspace();
    let directory = workspace.path().to_owned();
    let store = Arc::new(external_store::Store::open(directory.join("state.sqlite")).unwrap());
    let scheduler = Scheduler::new(
        "codex-resume-effort-none",
        store,
        Arc::new(TwoPhaseFactory {
            first: Mutex::new(Some(harness_factory(
                &happy_turn_with_effort(
                    &start_echo(&directory, PermissionMode::Plan),
                    Some("high"),
                ),
                &directory,
            ))),
            second: effort_resume_case(&directory, None, "codex-resume-effort-none.sh"),
        }),
        SchedulerConfig {
            bootstrap_timeout: Duration::from_secs(30),
            control_timeout: Duration::from_secs(30),
            ..SchedulerConfig::default()
        },
    )
    .unwrap();
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(&directory, "first turn"),
            Some(codex_admission_with_effort(Some(MODEL), Some("high"))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    scheduler.start_ready().unwrap();
    let first_result = await_result(&scheduler, &agent_id);
    assert_eq!(first_result.result.final_text, "CODEX_OK");
    let deadline = Instant::now() + Duration::from_secs(5);
    while scheduler.active_count() > 0 {
        assert!(
            Instant::now() < deadline,
            "first runtime was never released"
        );
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        scheduler
            .queue_message(&agent_id, "effort-none-msg", "follow-up question")
            .unwrap(),
        crate::MessageDisposition::Queued
    );
    scheduler.start_ready().unwrap();
    let resumed = await_result(&scheduler, &agent_id);
    assert_eq!(resumed.result.outcome, TaskOutcome::Completed);
    assert_eq!(resumed.result.final_text, "RESUMED_OK");

    let deliveries = std::fs::read_to_string(directory.join("deliveries-resume.jsonl"))
        .expect("the resumed process logs its own frames");
    let followup_turn_start = deliveries
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|frame| frame["method"] == "turn/start")
        .expect("follow-up turn/start frame");
    assert_eq!(
        followup_turn_start["params"]["effort"], "high",
        "a missing echo must not demote the admitted effort"
    );
}

#[test]
fn resume_fails_closed_when_the_effort_echo_diverges() {
    let _guard = scripted_test_guard();
    // The resume result echoes a different reasoning effort than the one
    // admitted at submit time: the resume fails closed before any follow-up
    // turn is sent, while the persisted thread identity stays durable for
    // a retry — mirroring the resume posture confirmation semantics.
    let workspace = codex_workspace();
    let directory = workspace.path().to_owned();
    let store = Arc::new(external_store::Store::open(directory.join("state.sqlite")).unwrap());
    let scheduler = Scheduler::new(
        "codex-resume-effort-fail",
        store,
        Arc::new(TwoPhaseFactory {
            first: Mutex::new(Some(harness_factory(
                &happy_turn_with_effort(
                    &start_echo(&directory, PermissionMode::Plan),
                    Some("high"),
                ),
                &directory,
            ))),
            second: effort_resume_case(&directory, Some("low"), "codex-resume-effort-fail.sh"),
        }),
        SchedulerConfig {
            bootstrap_timeout: Duration::from_secs(30),
            control_timeout: Duration::from_secs(30),
            ..SchedulerConfig::default()
        },
    )
    .unwrap();
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(&directory, "first turn"),
            Some(codex_admission_with_effort(Some(MODEL), Some("high"))),
        )
        .unwrap();
    let agent_id = submitted.agent_id.clone();
    scheduler.start_ready().unwrap();
    let first_result = await_result(&scheduler, &agent_id);
    assert_eq!(first_result.result.final_text, "CODEX_OK");
    let deadline = Instant::now() + Duration::from_secs(5);
    while scheduler.active_count() > 0 {
        assert!(
            Instant::now() < deadline,
            "first runtime was never released"
        );
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        scheduler
            .queue_message(&agent_id, "effort-fail-msg", "follow-up question")
            .unwrap(),
        crate::MessageDisposition::Queued
    );
    assert!(
        scheduler.start_ready().is_err(),
        "a divergent resume effort echo must fail closed"
    );
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Failed));
    assert_eq!(
        task.session_id.as_deref(),
        Some(THREAD_ID),
        "the persisted thread identity stays durable for a retry"
    );
    let record = scheduler.last_error(&agent_id).expect("failure record");
    assert!(
        record.contains("resume reasoningEffort was not confirmed as the admitted high"),
        "record: {record}"
    );
    let deliveries = std::fs::read_to_string(directory.join("deliveries-resume.jsonl"))
        .expect("the resumed process logs its own frames");
    let turn_starts = deliveries
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|frame| frame["method"] == "turn/start")
        .count();
    assert_eq!(
        turn_starts, 0,
        "no follow-up turn may start on an unconfirmed resume effort"
    );
}

#[test]
fn yolo_resume_fails_closed_without_a_confirmed_posture() {
    let _guard = scripted_test_guard();
    for (resume_result, marker) in [
        // No posture fields at all: unverifiable means refused.
        (
            r#"{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-5.6-terra"}}"#,
            "danger-full-access",
        ),
        // The plan read-only object is a different permission mode's
        // posture and is never accepted for a yolo task.
        (
            r#"{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-5.6-terra","sandbox":{"type":"readOnly","networkAccess":false},"approvalPolicy":"never","cwd":"/elsewhere"}}"#,
            "danger-full-access",
        ),
        // The request-time string preset is not the resolved posture the
        // live probe returns; it stays unconfirmed.
        (
            r#"{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-5.6-terra","sandbox":"danger-full-access","approvalPolicy":"never","cwd":"/elsewhere"}}"#,
            "danger-full-access",
        ),
        // A network-capable reconstruction diverges from the observed
        // no-network posture and fails closed.
        (
            r#"{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-5.6-terra","sandbox":{"type":"workspaceWrite","networkAccess":true,"writableRoots":[],"excludeSlashTmp":false,"excludeTmpdirEnvVar":false},"approvalPolicy":"never","cwd":"/elsewhere"}}"#,
            "danger-full-access",
        ),
        // Extra writable roots are unconfirmed drift.
        (
            r#"{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-5.6-terra","sandbox":{"type":"workspaceWrite","networkAccess":false,"writableRoots":["/etc"],"excludeSlashTmp":false,"excludeTmpdirEnvVar":false},"approvalPolicy":"never","cwd":"/elsewhere"}}"#,
            "danger-full-access",
        ),
        // An approval policy other than never.
        (
            r#"{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-5.6-terra","sandbox":{"type":"dangerFullAccess"},"approvalPolicy":"on-request","cwd":"/elsewhere"}}"#,
            "never",
        ),
        // A thread rooted somewhere other than the task workspace.
        (
            r#"{"id":2,"result":{"thread":{"id":"codex-thread-1","ephemeral":false},"model":"gpt-5.6-terra","sandbox":{"type":"dangerFullAccess"},"approvalPolicy":"never","cwd":"/elsewhere"}}"#,
            "task workspace",
        ),
    ] {
        let workspace = codex_workspace();
        let directory = workspace.path().to_owned();
        let resume_script = format!(
            r#"
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
printf '%s\n' '{{"id":1,"result":{{"codexHome":"/tmp/codex-home"}}}}'
IFS= read -r line
printf '%s\n' "$line" >> deliveries-resume.jsonl
printf '%s\n' '{resume_result}'
IFS= read -r line
sleep 1
"#
        );
        let store = Arc::new(external_store::Store::open(directory.join("state.sqlite")).unwrap());
        let scheduler = Scheduler::new(
            "codex-resume-yolo-fail",
            store,
            Arc::new(TwoPhaseFactory {
                first: Mutex::new(Some(harness_factory(
                    &happy_turn(&directory, PermissionMode::Yolo),
                    &directory,
                ))),
                second: resume_phase_factory(
                    &directory,
                    &resume_script,
                    "codex-resume-yolo-fail.sh",
                ),
            }),
            SchedulerConfig {
                bootstrap_timeout: Duration::from_secs(30),
                control_timeout: Duration::from_secs(30),
                ..SchedulerConfig::default()
            },
        )
        .unwrap();
        let submitted = scheduler
            .enqueue_general_with_admission(
                &manifest_for_mode(&directory, "first turn", PermissionMode::Yolo),
                Some(codex_admission(Some(MODEL))),
            )
            .unwrap();
        let agent_id = submitted.agent_id.clone();
        scheduler.start_ready().unwrap();
        let first_result = await_result(&scheduler, &agent_id);
        assert_eq!(first_result.result.final_text, "CODEX_OK");
        let deadline = Instant::now() + Duration::from_secs(5);
        while scheduler.active_count() > 0 {
            assert!(
                Instant::now() < deadline,
                "first runtime was never released"
            );
            thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(
            scheduler
                .queue_message(&agent_id, "fail-msg", "follow-up question")
                .unwrap(),
            crate::MessageDisposition::Queued
        );
        assert!(
            scheduler.start_ready().is_err(),
            "an unconfirmed yolo posture must fail the resume closed"
        );
        let task = await_terminal_task(&scheduler, &agent_id);
        assert_eq!(task.outcome, Some(TaskOutcome::Failed));
        assert_eq!(
            task.session_id.as_deref(),
            Some(THREAD_ID),
            "the persisted thread identity stays durable for a retry"
        );
        let record = scheduler.last_error(&agent_id).expect("failure record");
        assert!(record.contains(marker), "record: {record}");

        let deliveries = std::fs::read_to_string(directory.join("deliveries-resume.jsonl"))
            .expect("the resumed process logs its own frames");
        let turn_starts = deliveries
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|frame| frame["method"] == "turn/start")
            .count();
        assert_eq!(turn_starts, 0, "no turn may start on an unverified resume");
    }
}
