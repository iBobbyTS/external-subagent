//! Scripted ACP contract tests for the DSH adapter: gates, routing,
//! bootstrap ordering, permission/input correlation, settlement folding,
//! cancellation, and drain lifecycle through the shared scheduler.

use super::*;
use crate::{
    task_agent, terminal_proves_process_group_reaped, CommandRuntimeFactory, LifecycleSink,
    ManagedRuntime, MessageDisposition, ResponseDisposition, RuntimeCommandError, RuntimeFactory,
    RuntimeTerminal, Scheduler, SchedulerConfig, SchedulerError, SessionReady, TurnSnapshot,
    TurnTracker,
};
use external_contract::{EventEnvelope, WireMessage, SESSION_EVENT};
use external_core::{AdmissionIdentity, GeneralTaskManifest, PermissionMode, GENERAL_TASK_SCHEMA};
use external_runtime::{ChildExit, Inbound, ProcessIdentity, StopOutcome};
use external_store::{MessageState, PendingRequestState, TaskOutcome, TaskPhase, TaskRecord};
use std::io;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{atomic::AtomicUsize, Condvar};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SESSION_ID: &str = "dsh-build-session";

fn dsh_admission(model: Option<&str>) -> AdmissionIdentity {
    dsh_admission_with_effort(model, None)
}

fn dsh_admission_with_effort(model: Option<&str>, effort: Option<&str>) -> AdmissionIdentity {
    AdmissionIdentity {
        agent: "dsh".into(),
        config_revision: 1,
        adapter_version: env!("CARGO_PKG_VERSION").into(),
        model: model.map(str::to_owned),
        model_source: "catalog".into(),
        effort: effort.map(str::to_owned),
    }
}

/// Serialize the scripted-children tests so at most one extra provider
/// process runs at a time inside the parallel daemon suite; the older
/// hi-probe fixtures are timing-marginal under concurrent process load.
static SCRIPTED_CHILD_LOCK: Mutex<()> = Mutex::new(());

fn scripted_test_guard() -> std::sync::MutexGuard<'static, ()> {
    SCRIPTED_CHILD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Write the shared scripted-child harness at `path`. Besides recording every
/// inbound frame to `<workspace>/wire.jsonl`, it records its own full argv to
/// `<workspace>/argv.log` (one `ARG<value>` line per argument) and the content
/// of every `--patch` it is launched with to `<workspace>/patch.log` — the
/// observation the manifest-build factory tests need beyond the ACP wire.
/// `script` is injected before the trailing stdin drain.
fn write_scripted_child(path: &std::path::Path, workspace: &std::path::Path, script: &str) {
    let template = r#"#!/bin/sh
LOG_PATH=__WIRE__
ARGV_PATH=__ARGV__
PATCH_LOG=__PATCHES__
log() { printf '%s\n' "$1" >> "$LOG_PATH"; }
record_invocation() {
  for a in "$@"; do printf 'ARG%s\n' "$a" >> "$ARGV_PATH"; done
  patch=""; prev=""
  for a in "$@"; do
    if [ "$prev" = "--patch" ]; then patch="$a"; fi
    prev="$a"
  done
  if [ -n "$patch" ]; then
    printf 'PATCH %s\n' "$patch" >> "$PATCH_LOG"
    cat "$patch" >> "$PATCH_LOG" 2>/dev/null || true
    printf '\n' >> "$PATCH_LOG"
  fi
}
record_invocation "$@"
read_frame() { IFS= read -r line; log "$line"; }
__SCRIPT__
while IFS= read -r line; do log "$line"; done
"#;
    let harness = template
        .replace(
            "__WIRE__",
            &format!("{:?}", workspace.join("wire.jsonl").to_string_lossy()),
        )
        .replace(
            "__ARGV__",
            &format!("{:?}", workspace.join("argv.log").to_string_lossy()),
        )
        .replace(
            "__PATCHES__",
            &format!("{:?}", workspace.join("patch.log").to_string_lossy()),
        )
        .replace("__SCRIPT__", script);
    std::fs::write(path, harness).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Write an executable scripted ACP child at `<workspace>/acp-child.sh`.
fn scripted_child(workspace: &std::path::Path, script: &str) -> std::path::PathBuf {
    let path = workspace.join("acp-child.sh");
    write_scripted_child(&path, workspace, script);
    path
}

fn wire_frames(workspace: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(workspace.join("wire.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Synchronization budget for "eventually" polls over real child I/O.
/// None of the waiters using it asserts deadline behavior; under the
/// parallel suite the scripted children (real processes) can need seconds
/// to become observable, so the budget matches the generous scheduler
/// windows instead of a tight wall-clock guess.
const SCRIPTED_SYNC_WAIT: Duration = Duration::from_secs(30);

fn wait_for_frames(workspace: &std::path::Path, expected: usize) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    loop {
        let frames = wire_frames(workspace);
        if frames.len() >= expected {
            return frames;
        }
        assert!(
            Instant::now() < deadline,
            "scripted child never observed {expected} frames"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn request_methods(frames: &[serde_json::Value]) -> Vec<&str> {
    frames
        .iter()
        .filter_map(|frame| frame.get("method").and_then(|value| value.as_str()))
        .collect()
}

// The scripted children speak real process I/O; under the parallel
// workspace suite the default 2s windows are too tight, so the tests
// budget generously (none of them asserts deadline behavior). The
// per-workspace limit is the contract every provider shares: one agent
// slot per workspace, no provider-private concurrency.
fn scheduler_over(workspace: &std::path::Path, factory: Arc<dyn RuntimeFactory>) -> Scheduler {
    let store = Arc::new(external_store::Store::open(workspace.join("state.sqlite")).unwrap());
    let config = SchedulerConfig {
        bootstrap_timeout: Duration::from_secs(30),
        control_timeout: Duration::from_secs(10),
        per_workspace_max_agents: 1,
        ..SchedulerConfig::default()
    };
    Scheduler::new("dsh-test", store, factory, config).unwrap()
}

fn dsh_scheduler(workspace: &std::path::Path, dsh: DshRuntimeFactory) -> Scheduler {
    // The zcode factory fails loudly if a test accidentally routes a task
    // away from the DSH adapter under test.
    let zcode = CommandRuntimeFactory::new(|_: &TaskRecord| {
        Err(io::Error::other("zcode route must not spawn in dsh tests"))
    });
    scheduler_over(workspace, Arc::new(RoutingRuntimeFactory::new(zcode, dsh)))
}

fn dsh_workspace() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("s04a-dsh-")
        .tempdir_in(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/live-agent/workspace"),
        )
        .unwrap()
}

fn manifest_for(workspace: &std::path::Path, prompt: &str) -> GeneralTaskManifest {
    GeneralTaskManifest {
        schema: GENERAL_TASK_SCHEMA.into(),
        agent_id: "dsh-build".into(),
        repository: workspace.canonicalize().unwrap(),
        permission_mode: PermissionMode::Build,
        prompt: prompt.into(),
        write_manifest: Vec::new(),
    }
}

fn enqueue_dsh(scheduler: &Scheduler, workspace: &std::path::Path, model: Option<&str>) -> String {
    enqueue_dsh_with_effort(scheduler, workspace, model, None)
}

fn enqueue_dsh_with_effort(
    scheduler: &Scheduler,
    workspace: &std::path::Path,
    model: Option<&str>,
    effort: Option<&str>,
) -> String {
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(workspace, "build the fixture"),
            Some(dsh_admission_with_effort(model, effort)),
        )
        .unwrap();
    submitted.agent_id
}

/// Enqueue a build task carrying an explicit caller write manifest.
fn manifest_with_write_scope(
    workspace: &std::path::Path,
    write_manifest: &[&str],
) -> GeneralTaskManifest {
    GeneralTaskManifest {
        schema: GENERAL_TASK_SCHEMA.into(),
        agent_id: "dsh-build".into(),
        repository: workspace.canonicalize().unwrap(),
        permission_mode: PermissionMode::Build,
        prompt: "build the fixture".into(),
        write_manifest: write_manifest
            .iter()
            .map(std::path::PathBuf::from)
            .collect(),
    }
}

fn enqueue_dsh_with_manifest(
    scheduler: &Scheduler,
    workspace: &std::path::Path,
    write_manifest: &[&str],
) -> String {
    let submitted = scheduler
        .enqueue_general_with_admission(
            &manifest_with_write_scope(workspace, write_manifest),
            Some(dsh_admission(Some("fixture-provider:fixture-model"))),
        )
        .unwrap();
    submitted.agent_id
}

/// A throwaway dsh installation tree the pinned derivation can walk: the outer
/// `@deepseek-ai/dsh` package (name pinned) holding a non-`.js` scripted
/// runtime, plus the **nested** `@deepseek-ai/dsh-fs` package the write-guard
/// `node_modules` symlink targets. The runtime answers `--version` and
/// `--dump-config` for the S02 manifest-build preflight and then speaks the
/// scripted ACP bootstrap; the shared harness records its argv and patch.
fn manifest_fake_dsh(workspace: &std::path::Path) -> (tempfile::TempDir, std::path::PathBuf) {
    let root = dsh_workspace();
    let dsh = root.path().join("node_modules/@deepseek-ai/dsh");
    std::fs::create_dir_all(dsh.join("bin")).unwrap();
    std::fs::write(
        dsh.join("package.json"),
        br#"{"name":"@deepseek-ai/dsh","version":"0.1.5-rc.1"}"#,
    )
    .unwrap();
    let fs_pkg = dsh.join("node_modules/@deepseek-ai/dsh-fs");
    std::fs::create_dir_all(fs_pkg.join("lib")).unwrap();
    std::fs::write(
        fs_pkg.join("package.json"),
        br#"{"name":"@deepseek-ai/dsh-fs"}"#,
    )
    .unwrap();
    std::fs::write(fs_pkg.join("lib/index.js"), b"// fake fs\n").unwrap();
    let runtime = dsh.join("bin/dsh-runtime");
    write_scripted_child(&runtime, workspace, MANIFEST_FAKE_SCRIPT);
    (root, runtime)
}

/// The fake runtime body: preflight probes plus a one-turn ACP bootstrap. The
/// `--dump-config` branch builds the write-guard `file://` name from the
/// `--patch` TempDir so the S02 dump validation sees the guard the daemon just
/// materialized, and pins `config.manifest` to the caller manifest the test
/// enqueues (`["src/a.rs", "docs"]`).
const MANIFEST_FAKE_SCRIPT: &str = r#"
case "$*" in
  *"--version"*) printf '%s\n' "0.1.5-rc.1"; exit 0;;
esac
case "$*" in
  *"--dump-config"*)
    patch=""
    prev=""
    for a in "$@"; do
      if [ "$prev" = "--patch" ]; then patch="$a"; fi
      prev="$a"
    done
    guard_dir=$(dirname "$patch")
    cat <<DUMP
- id: sandbox-policy
  name: '@deepseek-ai/dsh-sandbox-policy'
  config:
    mode: workspace-write
- id: approval
  name: '@deepseek-ai/dsh-user-approval'
  config:
    policy: ask
- id: permission
  name: '@deepseek-ai/dsh-permission-presets'
  config:
    presets:
      workspace-write:
        sandbox: workspace-write
        approval: ask
- id: tool-fs
  name: '@deepseek-ai/dsh-tool-fs'
- id: tool-fs-search
  name: '@deepseek-ai/dsh-tool-fs-search'
- id: sandbox
  name: '@deepseek-ai/dsh-sandbox-local'
- id: fs-sandbox
  name: '@deepseek-ai/dsh-fs-sandbox'
- id: acp
  name: '@deepseek-ai/dsh-acp'
- id: acp-app-startup
  name: '@deepseek-ai/dsh-acp-app'
- id: bash-sandbox
  name: '@deepseek-ai/dsh-bash-sandbox'
  disabled: "process.platform === 'win32'"
  config:
    timeoutMs: 60000
- id: pwsh-sandbox
  name: '@deepseek-ai/dsh-pwsh-sandbox'
  disabled: "process.platform !== 'win32'"
- id: tool-bash
  disabled: true
- id: tool-pwsh
  disabled: true
- id: tool-jobs
  disabled: true
- id: tool-skill
  disabled: true
- id: tool-subagent-control
  disabled: true
- id: tool-subagent-list-agents
  disabled: true
- id: tool-subagent
  disabled: true
- id: tool-subagent-fork
  disabled: true
- id: subagent
  disabled: true
- id: tool-workflow
  disabled: true
- id: tool-goal
  disabled: true
- id: tool-ralph
  disabled: true
- id: skill-filesystem
  disabled: true
- id: workflow-worker-thread
  disabled: true
- id: goal-round-driver
  disabled: true
- id: subagent-spawn-in-process
  disabled: true
- id: subagent-fork-in-process
  disabled: true
- name: file://$guard_dir/dsh-write-guard/lib/index.js
  config:
    manifest:
    - src/a.rs
    - docs
DUMP
    exit 0;;
esac
read_frame
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"capabilities":{"models":true,"cancel":true,"permission":true}}}'
read_frame
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"manifest-session","configOptions":[{"configId":"model"}]}}'
read_frame
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"configOptions":[]}}'
read_frame
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"manifest-session","update":{"type":"agent_message","messageId":"manifest-message","content":[{"type":"text","text":"manifest build done"}]}}}' '{"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn","messageId":"manifest-message"}}'
"#;

fn await_terminal_task(scheduler: &Scheduler, agent_id: &str) -> external_store::TaskRecord {
    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    loop {
        let task = scheduler.store().get_task(agent_id).unwrap().unwrap();
        if task.phase.is_terminal() {
            return task;
        }
        assert!(
            Instant::now() < deadline,
            "task never terminalized: {task:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn await_result(scheduler: &Scheduler, agent_id: &str) -> external_store::StoredTaskResult {
    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    loop {
        if let Some(result) = scheduler.store().task_result(agent_id).unwrap() {
            return result;
        }
        assert!(Instant::now() < deadline, "no terminal result");
        thread::sleep(Duration::from_millis(10));
    }
}

fn await_pending_permission(
    scheduler: &Scheduler,
    agent_id: &str,
) -> external_store::StoredPendingRequest {
    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    loop {
        let requests = scheduler.store().pending_requests(agent_id).unwrap();
        if let Some(request) = requests.first() {
            assert_eq!(request.request_type, "permission");
            assert_eq!(request.state, PendingRequestState::Pending);
            return request.clone();
        }
        assert!(Instant::now() < deadline, "permission never became pending");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn closed_gate_refuses_dsh_spawn_without_touching_any_process() {
    let workspace = dsh_workspace();
    let scheduler = dsh_scheduler(workspace.path(), DshRuntimeFactory::closed());
    let agent_id = enqueue_dsh(
        &scheduler,
        workspace.path(),
        Some("fixture-provider:fixture-model"),
    );
    let error = scheduler.start_ready().unwrap_err();
    match &error {
        SchedulerError::RuntimeSpawn { message, .. } => {
            assert!(message.contains("dsh spawn gate is closed"), "{message}");
        }
        other => panic!("expected a runtime spawn refusal, got {other:?}"),
    }
    let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
    assert_eq!(task.phase, TaskPhase::Terminal);
    assert_eq!(task.outcome, Some(TaskOutcome::Failed));
    let failure = scheduler.last_error(&agent_id).expect("failure record");
    assert!(failure.contains("RUNTIME_SPAWN_FAILED"), "{failure}");
    // The workspace saw no provider process at all.
    assert!(!workspace.path().join("wire.jsonl").exists());
}

/// Minimal sink for direct factory probes; records nothing.
struct NoopSink;
impl crate::LifecycleSink for NoopSink {
    fn emit(&self, _record: crate::LifecycleRecord) {}
}

#[test]
fn routing_factory_keeps_the_zcode_route_on_the_zcode_factory() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    let store =
        Arc::new(external_store::Store::open(workspace.path().join("state.sqlite")).unwrap());
    let zcode = CommandRuntimeFactory::new(|_: &TaskRecord| {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5"]);
        Ok(command)
    });
    let factory = RoutingRuntimeFactory::new(zcode, DshRuntimeFactory::closed());
    // A scheduler is used only to persist the prepared task; spawn goes
    // through the routing factory directly.
    let enqueue_only = Scheduler::new(
        "enqueue-only",
        Arc::clone(&store),
        Arc::new(DshRuntimeFactory::closed()),
        SchedulerConfig::default(),
    )
    .unwrap();
    let submitted = enqueue_only
        .enqueue_general(&manifest_for(workspace.path(), "route check"))
        .unwrap();
    // Enqueue with plan mode stores a prepared task whose route lacks an
    // admission identity; such tasks keep the zcode route.
    let task = store.get_task(&submitted.agent_id).unwrap().unwrap();
    assert_eq!(task_agent(&task), "zcode");
    let runtime = RuntimeFactory::spawn(&factory, &task, Arc::new(NoopSink))
        .expect("zcode tasks keep the zcode factory");
    assert!(runtime.identity().is_some());
    let terminal = runtime.stop(Duration::from_millis(200));
    assert!(terminal_proves_process_group_reaped(&terminal));
}

/// Shared bootstrap + one build turn prefix for the scripted children.
const BOOTSTRAP_PREFIX: &str = r#"
read_frame
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"capabilities":{"models":true,"cancel":true,"permission":true}}}'
read_frame
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"SESSION","configOptions":[{"configId":"model"}]}}'
read_frame
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"configOptions":[]}}'
read_frame
printf '%s\n' \
  '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"SESSION","update":{"type":"tool_call","toolCallId":"tool-7","kind":"edit","title":"Edit fixture"}}}' \
  '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"SESSION","update":{"type":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}}' \
  '{"jsonrpc":"2.0","id":"srv-1","method":"session/request_permission","params":{"sessionId":"SESSION","toolCallId":"tool-7","options":[{"optionId":"allow-once","kind":"allow_once"},{"optionId":"reject-once","kind":"reject_once"}]}}'
read_frame
"#;

#[test]
fn build_task_flows_model_permission_and_result_through_the_shared_lifecycle() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    let script = format!(
        "{BOOTSTRAP_PREFIX}\
printf '%s\\n' \
  '{{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{{\"sessionId\":\"{SESSION_ID}\",\"update\":{{\"type\":\"agent_message\",\"messageId\":\"message-final\",\"content\":[{{\"type\":\"text\",\"text\":\"build \"}},{{\"type\":\"text\",\"text\":\"answer\"}}]}}}}}}' \
  '{{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{{\"stopReason\":\"end_turn\",\"messageId\":\"message-final\"}}}}'
read_frame
printf '%s\\n' \
  '{{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{{\"sessionId\":\"{SESSION_ID}\",\"update\":{{\"type\":\"agent_message\",\"messageId\":\"message-2\",\"content\":[{{\"type\":\"text\",\"text\":\"follow-up settled\"}}]}}}}}}' \
  '{{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{{\"stopReason\":\"end_turn\",\"messageId\":\"message-2\"}}}}'
read_frame
printf '%s\\n' \
  '{{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{{\"sessionId\":\"{SESSION_ID}\",\"update\":{{\"type\":\"agent_message\",\"messageId\":\"message-3\",\"content\":[{{\"type\":\"text\",\"text\":\"second follow-up settled\"}}]}}}}}}' \
  '{{\"jsonrpc\":\"2.0\",\"id\":6,\"result\":{{\"stopReason\":\"end_turn\",\"messageId\":\"message-3\"}}}}'
"
    )
    .replace("SESSION", SESSION_ID);
    let child = scripted_child(workspace.path(), &script);
    let scheduler = dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(child)),
    );
    let agent_id = enqueue_dsh(
        &scheduler,
        workspace.path(),
        Some("fixture-provider:fixture-model"),
    );

    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);

    // The permission request reaches the store with the correlated tool
    // identity (X06): the toolCallId came from a prior tool update.
    let request = await_pending_permission(&scheduler, &agent_id);
    let payload: serde_json::Value = serde_json::from_str(&request.payload_json).unwrap();
    assert_eq!(payload["toolName"], "edit");
    assert_eq!(payload["toolCallId"], "tool-7");

    // Queue the follow-up while the turn is still blocked on permission;
    // delivery may only happen after settlement.
    assert_eq!(
        scheduler
            .queue_message(&agent_id, "follow-up", "follow-up prompt")
            .unwrap(),
        MessageDisposition::Queued
    );

    assert_eq!(
        scheduler
            .queue_message(&agent_id, "second-follow-up", "second follow-up prompt")
            .unwrap(),
        MessageDisposition::Queued
    );

    let outcome = scheduler
        .respond_request(&agent_id, &request.request_id, "allow", None)
        .unwrap();
    assert_eq!(outcome.disposition, ResponseDisposition::Responded);
    assert_eq!(outcome.effective_decision, "allow");
    assert!(!outcome.policy_overrode);

    // A duplicate may report its durable receipt but must never reach ACP again.
    let duplicate = scheduler
        .respond_request(&agent_id, &request.request_id, "deny", None)
        .unwrap();
    assert_eq!(duplicate.disposition, ResponseDisposition::AlreadyResponded);
    assert_eq!(duplicate.effective_decision, "allow");

    let stored = await_result(&scheduler, &agent_id);
    assert_eq!(stored.result.outcome, TaskOutcome::Completed);
    assert_eq!(stored.result.final_text, "second follow-up settled");
    assert!(!stored.result.partial);

    // The queued message was delivered exactly once.
    let receipt = scheduler.store().message("follow-up").unwrap().unwrap();
    assert_eq!(receipt.state, MessageState::Delivered);
    assert_eq!(
        scheduler
            .queue_message(&agent_id, "follow-up", "follow-up prompt")
            .unwrap(),
        MessageDisposition::AlreadyDelivered
    );

    // Natural completion released the runtime and terminalized the task.
    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    while scheduler.active_count() != 0 {
        assert!(Instant::now() < deadline, "runtime was not released");
        thread::sleep(Duration::from_millis(10));
    }
    let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
    assert_eq!(task.phase, TaskPhase::Terminal);
    assert_eq!(task.outcome, Some(TaskOutcome::Completed));

    // Wire evidence: bootstrap order, model verified before the prompt,
    // one prompt per turn, and a single-shot allow echoing the offered id.
    // (The allow response sits between the two prompts in arrival order.)
    let frames = wait_for_frames(workspace.path(), 7);
    assert_eq!(
        request_methods(&frames),
        vec![
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/prompt",
            "session/prompt",
            "session/prompt"
        ]
    );
    assert_eq!(frames[2]["params"]["configId"], "model");
    assert_eq!(
        frames[2]["params"]["value"],
        "[\"fixture-provider\",\"fixture-model\"]"
    );
    let prompts: Vec<&serde_json::Value> = frames
        .iter()
        .filter(|frame| {
            frame.get("method").and_then(|value| value.as_str()) == Some("session/prompt")
        })
        .collect();
    assert_eq!(prompts.len(), 3);
    fn prompt_text(frame: &serde_json::Value) -> String {
        frame["params"]["prompt"]
            .as_array()
            .expect("ACP prompt is a content block array")
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect()
    }
    assert!(prompt_text(prompts[0]).contains("build the fixture"));
    assert!(prompt_text(prompts[1]).contains("follow-up prompt"));
    assert!(!prompt_text(prompts[1]).contains("second follow-up prompt"));
    assert!(prompt_text(prompts[2]).contains("second follow-up prompt"));
    assert_eq!(
        scheduler
            .store()
            .message("second-follow-up")
            .unwrap()
            .unwrap()
            .state,
        MessageState::Delivered
    );
    let terminal_before = scheduler.store().get_task(&agent_id).unwrap().unwrap();
    assert_eq!(
        scheduler
            .respond_request(&agent_id, &request.request_id, "allow", None)
            .unwrap()
            .disposition,
        ResponseDisposition::AlreadyResponded
    );
    assert_eq!(
        scheduler.store().get_task(&agent_id).unwrap().unwrap(),
        terminal_before
    );
    let permission_responses: Vec<&serde_json::Value> = frames
        .iter()
        .filter(|frame| frame.get("id").and_then(|id| id.as_str()) == Some("srv-1"))
        .collect();
    assert_eq!(permission_responses.len(), 1, "respond must happen once");
    assert_eq!(
        permission_responses[0]["result"]["outcome"]["optionId"],
        "allow-once"
    );
    for frame in &frames {
        assert_eq!(frame["jsonrpc"], "2.0");
    }
    // The derived `["."]` manifest keeps the legacy build composition: the
    // recorded argv proves the spawn carries no `--patch` at all.
    let argv = std::fs::read_to_string(workspace.path().join("argv.log")).unwrap();
    assert!(
        argv.lines().any(|line| line == "ARG--profile"),
        "legacy build argv must pin the profile: {argv}"
    );
    assert!(
        !argv.lines().any(|line| line == "ARG--patch"),
        "workspace-root manifest must not receive a patch: {argv}"
    );
}

#[test]
fn answerable_server_request_wakes_declared_waits_and_answers_over_acp() {
    use crate::rpc::{RpcMethod, RpcService, RpcSuccess, TaskWaitQuery};
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    // The child parks the turn on a NON-permission server request, which
    // the adapter projects as the answerable interaction/requestUserInput
    // producer instead of a permission offer.
    let script = format!(
        r#"
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"capabilities":{{"models":true,"cancel":true,"permission":true}}}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"{SESSION_ID}","configOptions":[{{"configId":"model"}}]}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"configOptions":[]}}}}'
read_frame
printf '%s\n' \
  '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"{SESSION_ID}","update":{{"type":"agent_message","messageId":"message-q","content":[{{"type":"text","text":"which scope should I use?"}}]}}}}}}' \
  '{{"jsonrpc":"2.0","id":"ask-1","method":"session/request_input","params":{{"prompt":"pick a scope"}}}}'
read_frame
printf '%s\n' \
  '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"{SESSION_ID}","update":{{"type":"agent_message","messageId":"message-final","content":[{{"type":"text","text":"building with the answered scope"}}]}}}}}}' \
  '{{"jsonrpc":"2.0","id":4,"result":{{"stopReason":"end_turn","messageId":"message-final"}}}}'
while IFS= read -r line; do :; done
"#
    )
    .replace("SESSION", SESSION_ID);
    let child = scripted_child(workspace.path(), &script);
    let scheduler = dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(child)),
    );
    let agent_id = enqueue_dsh(
        &scheduler,
        workspace.path(),
        Some("fixture-provider:fixture-model"),
    );
    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);

    let request = {
        let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
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
            assert!(
                Instant::now() < deadline,
                "server request never became pending"
            );
            thread::sleep(Duration::from_millis(10));
        }
    };
    let service = Arc::new(RpcService::new(scheduler.clone(), scheduler.store()).unwrap());
    // Every actionable user-input request wakes without a capability
    // handshake and carries the embedded question.
    let RpcSuccess::TaskWait {
        pending_requests,
        timed_out,
        instruction,
        ..
    } = service
        .dispatch(RpcMethod::TaskWait(TaskWaitQuery {
            agent_id: agent_id.clone(),
            wait_time: 299,
            message_id: None,
        }))
        .unwrap()
    else {
        panic!("expected wait response")
    };
    assert!(!timed_out);
    assert_eq!(pending_requests.len(), 1);
    assert_eq!(pending_requests[0].kind, "user_input");
    assert_eq!(pending_requests[0].summary, "question pick a scope");
    assert_eq!(
        pending_requests[0].question.as_ref().unwrap().text,
        "pick a scope"
    );
    assert_eq!(
        instruction.as_deref(),
        Some("The subagent requested input; answer it now with external_subagent_respond using decision answer and non-empty content.")
    );
    // The caller answers through the request id exactly as the wait
    // response returned it, with no second lookup.
    let returned_id = pending_requests[0].request_id.clone();
    assert_eq!(returned_id, request.request_id);
    // allow/deny against an answerable request fails closed.
    assert!(scheduler
        .respond_request(&agent_id, &returned_id, "allow", None)
        .is_err());
    assert_eq!(
        scheduler
            .respond_request(&agent_id, &returned_id, "answer", Some("release scope"))
            .unwrap()
            .disposition,
        ResponseDisposition::Responded
    );

    let stored = await_result(&scheduler, &agent_id);
    assert_eq!(stored.result.outcome, TaskOutcome::Completed);
    assert_eq!(stored.result.final_text, "building with the answered scope");
    // Wire evidence: the answer reached the ACP server as the plain result
    // frame for its request id.
    let frames = wait_for_frames(workspace.path(), 5);
    let answer = frames
        .iter()
        .find(|frame| frame.get("id").and_then(|id| id.as_str()) == Some("ask-1"))
        .expect("ACP server never observed the answer frame");
    assert_eq!(answer["result"], "release scope");
}

#[test]
fn unsupported_dsh_requests_stay_non_respondable_even_for_answering_callers() {
    use crate::rpc::{RpcMethod, RpcService, RpcSuccess, TaskWaitQuery};
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    // One unknown interaction method and one malformed input request
    // (no usable prompt): both must land as the unsupported sentinel,
    // never as the answerable producer. The child holds them back until
    // the start path has finished marking the session running.
    let script = format!(
        r#"
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"capabilities":{{"models":true,"cancel":true,"permission":true}}}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"{SESSION_ID}","configOptions":[{{"configId":"model"}}]}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"configOptions":[]}}}}'
read_frame
while [ ! -f ask-now ]; do sleep 0.01; done
printf '%s\n' \
  '{{"jsonrpc":"2.0","id":"odd-1","method":"session/requestTail","params":{{"topic":"logs"}}}}' \
  '{{"jsonrpc":"2.0","id":"ask-bad","method":"session/request_input","params":{{"topic":"no prompt here"}}}}'
while IFS= read -r line; do :; done
"#
    )
    .replace("SESSION", SESSION_ID);
    let child = scripted_child(workspace.path(), &script);
    let scheduler = dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(child)),
    );
    let agent_id = enqueue_dsh(
        &scheduler,
        workspace.path(),
        Some("fixture-provider:fixture-model"),
    );
    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
    assert_eq!(
        scheduler
            .store()
            .get_task(&agent_id)
            .unwrap()
            .unwrap()
            .phase,
        TaskPhase::Running
    );
    std::fs::write(workspace.path().join("ask-now"), "").unwrap();
    let requests = {
        let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
        loop {
            let requests = scheduler.store().pending_requests(&agent_id).unwrap();
            if requests.len() == 2 {
                break requests;
            }
            assert!(
                Instant::now() < deadline,
                "unsupported requests never became pending"
            );
            thread::sleep(Duration::from_millis(10));
        }
    };
    assert!(requests
        .iter()
        .all(|request| request.request_type == "unsupported_input"));
    let service = Arc::new(RpcService::new(scheduler.clone(), scheduler.store()).unwrap());
    // The sentinel records are neither woken on nor conveyed: the wait
    // times out with an empty projection, and answering them fails closed.
    let RpcSuccess::TaskWait {
        pending_requests,
        timed_out,
        ..
    } = service
        .dispatch(RpcMethod::TaskWait(TaskWaitQuery {
            agent_id: agent_id.clone(),
            wait_time: 0,
            message_id: None,
        }))
        .unwrap()
    else {
        panic!("expected wait response")
    };
    assert!(timed_out);
    assert!(pending_requests.is_empty());
    assert!(scheduler
        .respond_request(&agent_id, &requests[0].request_id, "answer", Some("guess"))
        .is_err());
    service
        .dispatch(RpcMethod::TaskCancel {
            agent_id: agent_id.clone(),
        })
        .unwrap();
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Cancelled));
}

#[test]
fn answerable_request_among_unsupported_noise_is_returned_and_answerable() {
    use crate::rpc::{RpcMethod, RpcService, RpcSuccess, TaskWaitQuery};
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    // The child holds its input request back until the test has planted
    // one hundred filler records, so the real answerable producer arrives
    // as request 101 — beyond the bounded wait projection.
    let script = format!(
        r#"
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"capabilities":{{"models":true,"cancel":true,"permission":true}}}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"{SESSION_ID}","configOptions":[{{"configId":"model"}}]}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"configOptions":[]}}}}'
read_frame
while [ ! -f ask-now ]; do sleep 0.01; done
printf '%s\n' \
  '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"{SESSION_ID}","update":{{"type":"agent_message","messageId":"message-q","content":[{{"type":"text","text":"which scope should I use?"}}]}}}}}}' \
  '{{"jsonrpc":"2.0","id":"ask-101","method":"session/request_input","params":{{"prompt":"pick a scope"}}}}'
read_frame
printf '%s\n' \
  '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"{SESSION_ID}","update":{{"type":"agent_message","messageId":"message-final","content":[{{"type":"text","text":"answered past the cap"}}]}}}}}}' \
  '{{"jsonrpc":"2.0","id":4,"result":{{"stopReason":"end_turn","messageId":"message-final"}}}}'
while IFS= read -r line; do :; done
"#
    )
    .replace("SESSION", SESSION_ID);
    let child = scripted_child(workspace.path(), &script);
    let scheduler = dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(child)),
    );
    let agent_id = enqueue_dsh(
        &scheduler,
        workspace.path(),
        Some("fixture-provider:fixture-model"),
    );
    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
    for index in 0..crate::rpc::MAX_PENDING_REQUESTS {
        scheduler
            .store()
            .insert_pending_request(
                &format!("filler-{index}"),
                &agent_id,
                &format!("\"filler-correlation-{index}\""),
                "unsupported_input",
                "{}",
            )
            .unwrap();
    }
    std::fs::write(workspace.path().join("ask-now"), "").unwrap();
    let service = Arc::new(RpcService::new(scheduler.clone(), scheduler.store()).unwrap());
    let RpcSuccess::TaskWait {
        pending_requests,
        timed_out,
        ..
    } = service
        .dispatch(RpcMethod::TaskWait(TaskWaitQuery {
            agent_id: agent_id.clone(),
            wait_time: 299,
            message_id: None,
        }))
        .unwrap()
    else {
        panic!("expected wait response")
    };
    // The unsupported fillers stay invisible; the actionable request is
    // the sole projected record and is directly answerable.
    assert!(!timed_out);
    assert_eq!(pending_requests.len(), 1);
    let wake = &pending_requests[0];
    assert!(!wake.request_id.starts_with("filler-"));
    assert_eq!(wake.kind, "user_input");
    assert_eq!(wake.summary, "question pick a scope");
    assert_eq!(wake.question.as_ref().unwrap().text, "pick a scope");
    // Answering through the returned id settles the parked ACP request.
    assert_eq!(
        scheduler
            .respond_request(&agent_id, &wake.request_id, "answer", Some("release scope"))
            .unwrap()
            .disposition,
        ResponseDisposition::Responded
    );
    // The fillers only existed to push the real request past the cap;
    // settle them so natural completion is not blocked.
    for index in 0..crate::rpc::MAX_PENDING_REQUESTS {
        let filler = format!("filler-{index}");
        scheduler
            .store()
            .claim_pending_response_if_accepting(&agent_id, &filler, "answer", None)
            .unwrap();
        scheduler
            .store()
            .complete_pending_response(&agent_id, &filler)
            .unwrap();
    }
    let stored = await_result(&scheduler, &agent_id);
    assert_eq!(stored.result.outcome, TaskOutcome::Completed);
    assert_eq!(stored.result.final_text, "answered past the cap");
    let frames = wait_for_frames(workspace.path(), 5);
    let answer = frames
        .iter()
        .find(|frame| frame.get("id").and_then(|id| id.as_str()) == Some("ask-101"))
        .expect("ACP server never observed the answer frame");
    assert_eq!(answer["result"], "release scope");
}

#[test]
fn max_tokens_settlement_fails_the_task_without_faking_completion() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    let script = format!(
        "{BOOTSTRAP_PREFIX}\
printf '%s\\n' \
  '{{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{{\"sessionId\":\"{SESSION_ID}\",\"update\":{{\"type\":\"agent_message\",\"messageId\":\"message-final\",\"content\":[{{\"type\":\"text\",\"text\":\"truncated\"}}]}}}}}}' \
  '{{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{{\"stopReason\":\"max_tokens\",\"messageId\":\"message-final\"}}}}'
"
    )
    .replace("SESSION", SESSION_ID);
    let child = scripted_child(workspace.path(), &script);
    let scheduler = dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(child)),
    );
    let agent_id = enqueue_dsh(
        &scheduler,
        workspace.path(),
        Some("fixture-provider:fixture-model"),
    );
    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);

    let request = await_pending_permission(&scheduler, &agent_id);
    scheduler
        .respond_request(&agent_id, &request.request_id, "allow", None)
        .unwrap();

    let stored = await_result(&scheduler, &agent_id);
    assert_eq!(stored.result.outcome, TaskOutcome::Failed);
    // The committed message the child emitted before the max_tokens
    // settlement must never become the task result.
    assert!(
        !stored.result.final_text.contains("truncated"),
        "leaked settlement text: {}",
        stored.result.final_text
    );
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Failed));
    // The terminal is the failed turn boundary (never a runtime loss and
    // never a faked completion).
    let failure = scheduler.last_error(&agent_id).expect("failure record");
    assert!(failure.contains("RUNTIME_TERMINAL"), "{failure}");
    assert!(failure.contains("FailedTurn"), "{failure}");

    let frames = wire_frames(workspace.path());
    assert_eq!(
        request_methods(&frames),
        vec![
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/prompt"
        ]
    );
}

#[test]
fn rejected_model_selection_fails_before_any_prompt_is_sent() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    let script = format!(
        r#"
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"capabilities":{{"models":true,"cancel":true,"permission":true}}}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"{SESSION_ID}","configOptions":[{{"configId":"model"}}]}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"error":{{"code":-32602,"message":"unknown model option: nope"}}}}'
"#
    );
    let child = scripted_child(workspace.path(), &script);
    let scheduler = dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(child)),
    );
    let agent_id = enqueue_dsh(&scheduler, workspace.path(), Some("fixture-provider:nope"));
    // The refused model selection fails bootstrap and start_ready surfaces
    // the bounded provider rejection.
    let error = scheduler.start_ready().unwrap_err();
    assert!(
        error.to_string().contains("unknown model option"),
        "{error}"
    );

    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Failed));
    let failure = scheduler.last_error(&agent_id).expect("failure record");
    assert!(failure.contains("SESSION_START_FAILED"), "{failure}");
    assert!(failure.contains("unknown model option"), "{failure}");

    let frames = wire_frames(workspace.path());
    assert_eq!(
        request_methods(&frames),
        vec!["initialize", "session/new", "session/set_config_option"],
        "no prompt may follow a refused model selection"
    );
}

#[test]
fn admitted_effort_is_sent_after_the_model_and_before_the_first_prompt() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    // The scripted session advertises both config options, verifies the
    // model (id3) and the reasoning effort (id4), and only then settles the
    // single prompt turn (id5).
    let script = format!(
        r#"
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"capabilities":{{"models":true,"cancel":true,"permission":true}}}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"{SESSION_ID}","configOptions":[{{"configId":"model"}},{{"configId":"reasoning_effort"}}]}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"configOptions":[]}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":4,"result":{{"configOptions":[]}}}}'
read_frame
printf '%s\n' \
  '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"{SESSION_ID}","update":{{"type":"agent_message","messageId":"message-final","content":[{{"type":"text","text":"effort applied"}}]}}}}}}' \
  '{{"jsonrpc":"2.0","id":5,"result":{{"stopReason":"end_turn","messageId":"message-final"}}}}'
"#
    )
    .replace("SESSION", SESSION_ID);
    let child = scripted_child(workspace.path(), &script);
    let scheduler = dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(child)),
    );
    let agent_id = enqueue_dsh_with_effort(
        &scheduler,
        workspace.path(),
        Some("fixture-provider:fixture-model"),
        Some("high"),
    );
    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
    let stored = await_result(&scheduler, &agent_id);
    assert_eq!(stored.result.outcome, TaskOutcome::Completed);
    assert_eq!(stored.result.final_text, "effort applied");
    let frames = wait_for_frames(workspace.path(), 5);
    assert_eq!(
        request_methods(&frames),
        vec![
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/set_config_option",
            "session/prompt"
        ]
    );
    assert_eq!(frames[2]["params"]["configId"], "model");
    assert_eq!(
        frames[2]["params"]["value"],
        "[\"fixture-provider\",\"fixture-model\"]"
    );
    assert_eq!(frames[3]["params"]["configId"], "reasoning_effort");
    assert_eq!(frames[3]["params"]["value"], "high");
    assert_eq!(frames[4]["method"], "session/prompt");
}

#[test]
fn tasks_without_admitted_effort_emit_no_reasoning_effort_frame() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    // The server offers reasoning_effort, but the admission carries no
    // effort token: the wire must stay exactly at the pre-effort shape.
    let script = format!(
        r#"
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"capabilities":{{"models":true,"cancel":true,"permission":true}}}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"{SESSION_ID}","configOptions":[{{"configId":"model"}},{{"configId":"reasoning_effort"}}]}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"configOptions":[]}}}}'
read_frame
printf '%s\n' \
  '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"{SESSION_ID}","update":{{"type":"agent_message","messageId":"message-final","content":[{{"type":"text","text":"default effort kept"}}]}}}}}}' \
  '{{"jsonrpc":"2.0","id":4,"result":{{"stopReason":"end_turn","messageId":"message-final"}}}}'
"#
    )
    .replace("SESSION", SESSION_ID);
    let child = scripted_child(workspace.path(), &script);
    let scheduler = dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(child)),
    );
    let agent_id = enqueue_dsh(
        &scheduler,
        workspace.path(),
        Some("fixture-provider:fixture-model"),
    );
    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
    let stored = await_result(&scheduler, &agent_id);
    assert_eq!(stored.result.outcome, TaskOutcome::Completed);
    let frames = wait_for_frames(workspace.path(), 4);
    assert_eq!(
        request_methods(&frames),
        vec![
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/prompt"
        ]
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["params"]["configId"] == "reasoning_effort")
            .count(),
        0,
        "no reasoning_effort frame may exist without an admitted effort"
    );
}

#[test]
fn rejected_effort_selection_fails_before_any_prompt_is_sent() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    // The model selection verifies, the reasoning-effort selection is
    // rejected by the server, and the task must fail without any prompt.
    let script = format!(
        r#"
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"capabilities":{{"models":true,"cancel":true,"permission":true}}}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"{SESSION_ID}","configOptions":[{{"configId":"model"}},{{"configId":"reasoning_effort"}}]}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"configOptions":[]}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":4,"error":{{"code":-32602,"message":"unknown reasoning_effort option: high"}}}}'
"#
    )
    .replace("SESSION", SESSION_ID);
    let child = scripted_child(workspace.path(), &script);
    let scheduler = dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(child)),
    );
    let agent_id = enqueue_dsh_with_effort(
        &scheduler,
        workspace.path(),
        Some("fixture-provider:fixture-model"),
        Some("high"),
    );
    let error = scheduler.start_ready().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unknown reasoning_effort option"),
        "{error}"
    );
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Failed));
    let failure = scheduler.last_error(&agent_id).expect("failure record");
    assert!(failure.contains("SESSION_START_FAILED"), "{failure}");
    assert!(
        failure.contains("unknown reasoning_effort option"),
        "{failure}"
    );
    let frames = wire_frames(workspace.path());
    assert_eq!(
        request_methods(&frames),
        vec![
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/set_config_option"
        ],
        "no prompt may follow a refused reasoning-effort selection"
    );
}

/// Minimal test-only provider runtime (the conformance "third adapter"):
/// an in-process `ManagedRuntime` with no child process at all. Bootstrap
/// opens one active turn, `stop_turn` settles it cooperatively, and `stop`
/// publishes a terminal that proves reaping vacuously (no process existed).
/// It exists so a second provider can hold the shared scheduler/workspace
/// contract through the same `ManagedRuntime` seam without launching real
/// ZCode and without touching the production factory composition.
struct FakeProviderRuntime {
    session_id: String,
    tracker: Arc<TurnTracker>,
    terminal: Mutex<Option<RuntimeTerminal>>,
    terminal_changed: Condvar,
}

impl FakeProviderRuntime {
    fn new() -> Self {
        Self {
            session_id: "fake-provider-session".into(),
            tracker: Arc::new(TurnTracker::new()),
            terminal: Mutex::new(None),
            terminal_changed: Condvar::new(),
        }
    }

    fn turn_event(&self, kind: &str) {
        self.tracker
            .observe(&Inbound::Message(WireMessage::Event(EventEnvelope {
                method: SESSION_EVENT.into(),
                params: serde_json::json!({"type": kind}),
            })));
    }
}

struct FakeProviderFactory {
    spawns: AtomicUsize,
}

impl RuntimeFactory for FakeProviderFactory {
    fn spawn(
        &self,
        _task: &TaskRecord,
        _sink: Arc<dyn LifecycleSink>,
    ) -> io::Result<Arc<dyn ManagedRuntime>> {
        self.spawns.fetch_add(1, Ordering::AcqRel);
        Ok(Arc::new(FakeProviderRuntime::new()))
    }
}

impl ManagedRuntime for FakeProviderRuntime {
    fn identity(&self) -> Option<ProcessIdentity> {
        None
    }

    fn stop(&self, _grace: Duration) -> RuntimeTerminal {
        let mut terminal = self.terminal.lock().unwrap();
        let published = terminal
            .get_or_insert(RuntimeTerminal::Stopped(StopOutcome::AlreadyExited(
                ChildExit::Exited(Some(0)),
            )))
            .clone();
        drop(terminal);
        self.terminal_changed.notify_all();
        published
    }

    fn wait_terminal(&self, timeout: Duration) -> Option<RuntimeTerminal> {
        let deadline = Instant::now().checked_add(timeout)?;
        let mut terminal = self.terminal.lock().unwrap();
        loop {
            if let Some(published) = terminal.as_ref() {
                return Some(published.clone());
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (next, waited) = self
                .terminal_changed
                .wait_timeout(terminal, deadline - now)
                .unwrap();
            terminal = next;
            if waited.timed_out() {
                return terminal.clone();
            }
        }
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
        self.turn_event("turn.started");
        Ok(SessionReady {
            session_id: self.session_id.clone(),
            initial_turn_id: None,
            configured_model: None,
        })
    }

    fn stop_turn(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> Result<TurnSnapshot, RuntimeCommandError> {
        if session_id != self.session_id {
            return Err(RuntimeCommandError::InvalidSession(
                "session id does not belong to this runtime".into(),
            ));
        }
        let current = self.tracker.snapshot();
        if !current.active {
            return Ok(current);
        }
        self.turn_event("turn.completed");
        self.tracker
            .wait_boundary_after(current.generation, timeout)
    }

    fn turn_snapshot(&self) -> TurnSnapshot {
        self.tracker.snapshot()
    }

    fn activity_snapshot(&self) -> crate::RuntimeActivitySnapshot {
        self.tracker.activity_snapshot()
    }
}

/// Test-only composition mirroring `RoutingRuntimeFactory`: the zcode
/// route lands on the in-process fake provider, the dsh route on the same
/// test-harness DSH factory the other dsh tests use.
struct JointProviderFactory {
    zcode: Arc<FakeProviderFactory>,
    dsh: DshRuntimeFactory,
}

impl RuntimeFactory for JointProviderFactory {
    fn spawn(
        &self,
        task: &TaskRecord,
        sink: Arc<dyn LifecycleSink>,
    ) -> io::Result<Arc<dyn ManagedRuntime>> {
        match task_agent(task).as_str() {
            "zcode" => self.zcode.spawn(task, sink),
            "dsh" => self.dsh.spawn(task, sink),
            agent => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("task routes to unknown agent {agent:?}"),
            )),
        }
    }
}

#[test]
fn cross_provider_shared_scheduler_contract() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    // The dsh provider runs for real after the workspace is released: the
    // same scripted ACP child the active-cancellation test uses.
    let script = format!(
        "{BOOTSTRAP_PREFIX}printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{{\"stopReason\":\"cancelled\"}}}}'\\nsleep 1\\n"
    )
    .replace("SESSION", SESSION_ID);
    let child = scripted_child(workspace.path(), &script);
    let fake = Arc::new(FakeProviderFactory {
        spawns: AtomicUsize::new(0),
    });
    let scheduler = scheduler_over(
        workspace.path(),
        Arc::new(JointProviderFactory {
            zcode: Arc::clone(&fake),
            dsh: DshRuntimeFactory::test_harness(Some(child)),
        }),
    );

    // The first provider (the fake zcode runtime, one active turn) is
    // admitted and occupies the workspace's single agent slot.
    let first = scheduler
        .enqueue_general(&manifest_for(
            workspace.path(),
            "occupy the shared workspace",
        ))
        .unwrap()
        .agent_id;
    assert_eq!(scheduler.start_ready().unwrap(), vec![first.clone()]);
    let running = scheduler.store().get_task(&first).unwrap().unwrap();
    assert_eq!(running.phase, TaskPhase::Running);
    assert_eq!(scheduler.active_count(), 1);
    assert_eq!(fake.spawns.load(Ordering::Acquire), 1);

    // While the workspace is occupied the second provider's admission is
    // rejected with the workspace conflict naming the active agent;
    // nothing is queued and no dsh provider process is spawned.
    let conflict = scheduler
        .enqueue_general_with_admission(
            &manifest_for(workspace.path(), "second provider must wait"),
            Some(dsh_admission(Some("fixture-provider:fixture-model"))),
        )
        .unwrap_err();
    match &conflict {
        SchedulerError::Store(external_store::StoreError::Conflict(message)) => {
            assert_eq!(message, &format!("WORKSPACE_BUSY active_agent_id={first}"));
        }
        other => panic!("expected a workspace conflict, got {other:?}"),
    }
    assert!(!workspace.path().join("wire.jsonl").exists());
    assert!(scheduler.start_ready().unwrap().is_empty());
    assert_eq!(fake.spawns.load(Ordering::Acquire), 1);

    // Cancelling the occupier terminalizes it exactly once and releases
    // the slot; the cancelled provider never revives.
    let phase = scheduler
        .cancel_task(&first)
        .expect("cancel active occupier");
    assert!(matches!(phase, TaskPhase::Cancelling | TaskPhase::Terminal));
    let cancelled = await_terminal_task(&scheduler, &first);
    assert_eq!(cancelled.outcome, Some(TaskOutcome::Cancelled));
    assert_eq!(
        scheduler
            .store()
            .task_result(&first)
            .unwrap()
            .expect("cancelled occupier keeps its immutable result")
            .result
            .outcome,
        TaskOutcome::Cancelled
    );
    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    while scheduler.active_count() != 0 {
        assert!(Instant::now() < deadline, "occupier runtime was not reaped");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(scheduler.cancel_task(&first).unwrap(), TaskPhase::Terminal);
    assert!(scheduler.start_ready().unwrap().is_empty());
    assert_eq!(fake.spawns.load(Ordering::Acquire), 1);

    // After release the same workspace admits the dsh provider for real:
    // the scripted child performs the ACP bootstrap and blocks on the
    // fixture permission, holding the slot the cancelled provider lost.
    let second = enqueue_dsh(
        &scheduler,
        workspace.path(),
        Some("fixture-provider:fixture-model"),
    );
    assert_eq!(scheduler.start_ready().unwrap(), vec![second.clone()]);
    let _request = await_pending_permission(&scheduler, &second);
    assert_eq!(scheduler.active_count(), 1);

    // Cancelling the second provider reaps it without reviving either
    // provider: one cooperative session/cancel, both tasks terminal.
    let phase = scheduler
        .cancel_task(&second)
        .expect("cancel active dsh provider");
    assert!(matches!(phase, TaskPhase::Cancelling | TaskPhase::Terminal));
    let second_terminal = await_terminal_task(&scheduler, &second);
    assert_eq!(second_terminal.outcome, Some(TaskOutcome::Cancelled));
    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    while scheduler.active_count() != 0 {
        assert!(Instant::now() < deadline, "dsh runtime was not reaped");
        thread::sleep(Duration::from_millis(10));
    }
    let frames = wait_for_frames(workspace.path(), 5);
    assert!(request_methods(&frames).contains(&"session/prompt"));
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.get("method").and_then(|v| v.as_str()) == Some("session/cancel"))
            .count(),
        1
    );
    // The cancelled first provider stayed terminal through the second
    // provider's whole lifecycle and was never respawned.
    let first_final = scheduler.store().get_task(&first).unwrap().unwrap();
    assert_eq!(first_final.phase, TaskPhase::Terminal);
    assert_eq!(first_final.outcome, Some(TaskOutcome::Cancelled));
    assert_eq!(fake.spawns.load(Ordering::Acquire), 1);
}

#[test]
fn dsh_pending_task_cancel_is_terminal_and_non_resurrecting() {
    let workspace = dsh_workspace();
    let scheduler = dsh_scheduler(workspace.path(), DshRuntimeFactory::closed());
    let agent_id = enqueue_dsh(&scheduler, workspace.path(), None);
    let phase = scheduler
        .cancel_task(&agent_id)
        .expect("cancel pending task");
    assert_eq!(phase, TaskPhase::Terminal);
    assert_eq!(
        scheduler.cancel_task(&agent_id).unwrap(),
        TaskPhase::Terminal
    );
    assert_eq!(
        await_terminal_task(&scheduler, &agent_id).phase,
        TaskPhase::Terminal
    );
}

#[test]
fn dsh_active_task_cancel_sends_session_cancel_and_reaps_without_result() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    let script = format!(
        "{BOOTSTRAP_PREFIX}printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{{\"stopReason\":\"cancelled\"}}}}'\\nsleep 1\\n"
    ).replace("SESSION", SESSION_ID);
    let child = scripted_child(workspace.path(), &script);
    let scheduler = dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(child)),
    );
    let agent_id = enqueue_dsh(
        &scheduler,
        workspace.path(),
        Some("fixture-provider:fixture-model"),
    );
    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);

    let request = await_pending_permission(&scheduler, &agent_id);
    let phase = scheduler
        .cancel_task(&agent_id)
        .expect("cancel active task");
    assert!(matches!(phase, TaskPhase::Cancelling | TaskPhase::Terminal));
    let stored = await_result(&scheduler, &agent_id);
    assert_eq!(stored.result.outcome, TaskOutcome::Cancelled);
    assert!(stored.result.partial);

    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    while scheduler.active_count() != 0 {
        assert!(Instant::now() < deadline, "runtime was not reaped");
        thread::sleep(Duration::from_millis(10));
    }
    let terminal_before = await_terminal_task(&scheduler, &agent_id);
    let late = scheduler.respond_request(&agent_id, &request.request_id, "allow", None);
    assert!(
        late.is_err(),
        "late permission response must be refused: {late:?}"
    );
    assert_eq!(
        scheduler.store().get_task(&agent_id).unwrap().unwrap(),
        terminal_before
    );
    assert_eq!(scheduler.active_count(), 0);
    let frames = wait_for_frames(workspace.path(), 5);
    assert!(
        !frames
            .iter()
            .any(|frame| frame.get("id").and_then(|id| id.as_str()) == Some("srv-1")),
        "late response must never reach the provider"
    );
    assert!(request_methods(&frames).contains(&"session/prompt"));
    assert!(request_methods(&frames).contains(&"session/cancel"));
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.get("method").and_then(|v| v.as_str()) == Some("session/cancel"))
            .count(),
        1
    );
    assert!(scheduler.store().task_result(&agent_id).unwrap().is_some());
}
#[test]
fn drain_cancel_active_reaps_dsh_and_preserves_admitted_rpc_lifecycle() {
    use crate::rpc::{MessageInput, RpcMethod, RpcOutcome, RpcService, RpcSuccess, TaskWaitQuery};
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    // This child never finishes a prompt itself. Only the real cancellation
    // and process-group reap path can make activation ready.
    let child = scripted_child(
        workspace.path(),
        &BOOTSTRAP_PREFIX.replace("SESSION", SESSION_ID),
    );
    let scheduler = dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(child)),
    );
    let agent_id = enqueue_dsh(
        &scheduler,
        workspace.path(),
        Some("fixture-provider:fixture-model"),
    );
    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
    let _request = await_pending_permission(&scheduler, &agent_id);
    let queued_workspace = dsh_workspace();
    let queued_id = enqueue_dsh(
        &scheduler,
        queued_workspace.path(),
        Some("fixture-provider:fixture-model"),
    );
    let service = Arc::new(RpcService::new(scheduler.clone(), scheduler.store()).unwrap());
    let passive = service.handle_bytes(
        &serde_json::to_vec(&serde_json::json!({
            "request_id": "passive-upgrade",
            "method": "daemon_begin_drain"
        }))
        .unwrap(),
    );
    let RpcOutcome::Success { result: passive } = passive.outcome else {
        panic!("legacy drain request failed")
    };
    assert!(matches!(
        *passive,
        RpcSuccess::DaemonDrainStatus {
            active_count: 1,
            resources_reaped: false,
            ready_for_activation: false,
            ..
        }
    ));
    assert!(!request_methods(&wire_frames(workspace.path())).contains(&"session/cancel"));
    assert!(matches!(
        service.dispatch(RpcMethod::DaemonActivateReady).unwrap(),
        RpcSuccess::DaemonDrainStatus {
            activation_claim: None,
            ..
        }
    ));
    // The gate is the existing scheduler admission owner, not a provider-
    // specific spawn shortcut. Existing task operations remain real RPCs.
    assert!(matches!(scheduler.enqueue_general_with_admission(
        &manifest_for(workspace.path(), "new spawn rejected"),
        Some(dsh_admission(Some("fixture-provider:fixture-model")))),
        Err(SchedulerError::InvalidConfig(ref message)) if message == "daemon_draining"));
    let send = service
        .dispatch(RpcMethod::TaskMessage(MessageInput {
            agent_id: agent_id.clone(),
            message_id: Some("after-drain".into()),
            content: "must not run".into(),
        }))
        .unwrap_err();
    assert_eq!(send.message, "daemon_draining");
    assert!(scheduler.store().message("after-drain").unwrap().is_none());
    service
        .dispatch(RpcMethod::TaskWait(TaskWaitQuery {
            agent_id: agent_id.clone(),
            wait_time: 0,
            message_id: None,
        }))
        .unwrap();
    service
        .dispatch(RpcMethod::TaskResult {
            agent_id: agent_id.clone(),
            offset: 0,
            limit: 1024,
        })
        .unwrap();

    // Use the wire decoder, so an ignored/unknown cancel_active parameter
    // cannot pass even if a direct scheduler cancel test already passes.
    let began = Instant::now();
    let response = service.handle_bytes(
        &serde_json::to_vec(&serde_json::json!({
            "request_id": "cancel-upgrade",
            "method": "daemon_begin_drain", "params": { "cancel_active": true }
        }))
        .unwrap(),
    );
    assert!(
        matches!(response.outcome, RpcOutcome::Success { .. }),
        "{response:?}"
    );
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "management RPC waited on provider control"
    );
    let mut callers = Vec::new();
    for _ in 0..4 {
        let service = Arc::clone(&service);
        callers.push(thread::spawn(move || {
            service
                .dispatch(RpcMethod::DaemonBeginDrain {
                    cancel_active: true,
                })
                .unwrap()
        }));
    }
    for caller in callers {
        caller.join().unwrap();
    }
    let unreaped = service.dispatch(RpcMethod::DaemonDrainStatus).unwrap();
    assert!(matches!(
        unreaped,
        RpcSuccess::DaemonDrainStatus {
            active_count: 1,
            resources_reaped: false,
            ready_for_activation: false,
            ..
        }
    ));
    let fenced = scheduler.store().get_task(&queued_id).unwrap().unwrap();
    assert_eq!(fenced.phase, TaskPhase::Queued);
    assert!(fenced.stop_requested);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        // Exercise the actual iteration called by Daemon's production
        // claim thread while cancellation is waiting on the first child.
        assert!(
            scheduler.start_ready().unwrap().is_empty(),
            "cancelled queue was claimed"
        );
        let status = service.dispatch(RpcMethod::DaemonDrainStatus).unwrap();
        if matches!(
            status,
            RpcSuccess::DaemonDrainStatus {
                active_count: 0,
                resources_reaped: true,
                ready_for_activation: true,
                ..
            }
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "drain did not wait for runtime reap: {status:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        began.elapsed() > Duration::from_secs(6),
        "fixture must outlast the CLI RPC deadline"
    );
    let queued = scheduler.store().get_task(&queued_id).unwrap().unwrap();
    assert_eq!(queued.phase, TaskPhase::Terminal);
    assert_eq!(queued.outcome, Some(TaskOutcome::Cancelled));
    assert_eq!(
        queued.owner_epoch, 0,
        "queued cancellation must never acquire a runtime claim"
    );
    assert!(!queued_workspace.path().join("wire.jsonl").exists());
    let frames = wait_for_frames(workspace.path(), 5);
    assert_eq!(
        request_methods(&frames)
            .iter()
            .filter(|m| **m == "session/cancel")
            .count(),
        1
    );
    let stored = scheduler.store().get_task(&agent_id).unwrap().unwrap();
    assert_eq!(stored.phase, TaskPhase::Terminal);
    assert_eq!(stored.outcome, Some(TaskOutcome::Cancelled));
    let result = scheduler.store().task_result(&agent_id).unwrap().unwrap();
    service
        .dispatch(RpcMethod::TaskCancel {
            agent_id: agent_id.clone(),
        })
        .unwrap();
    service
        .dispatch(RpcMethod::TaskWait(TaskWaitQuery {
            agent_id: agent_id.clone(),
            wait_time: 0,
            message_id: None,
        }))
        .unwrap();
    service
        .dispatch(RpcMethod::TaskResult {
            agent_id: agent_id.clone(),
            offset: 0,
            limit: 1024,
        })
        .unwrap();
    service
        .dispatch(RpcMethod::TaskClose {
            agent_id: agent_id.clone(),
        })
        .unwrap();
    assert!(scheduler.start_ready().unwrap().is_empty());
    assert_eq!(
        scheduler.store().task_result(&agent_id).unwrap().unwrap(),
        result
    );
    assert_eq!(scheduler.active_count(), 0);
    assert!(matches!(
        service.dispatch(RpcMethod::DaemonDrainStatus).unwrap(),
        RpcSuccess::DaemonDrainStatus {
            resources_reaped: true,
            ready_for_activation: true,
            ..
        }
    ));
}

#[test]
fn aborted_drain_reopens_dsh_admission_while_the_drained_task_keeps_answering() {
    use crate::rpc::{
        MessageInput, RpcError, RpcErrorCode, RpcMethod, RpcOutcome, RpcService, RpcSuccess,
        TaskWaitQuery,
    };
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    // This child parks on a pending permission request, exactly like a
    // live DSH task an update would drain around.
    let child = scripted_child(
        workspace.path(),
        &BOOTSTRAP_PREFIX.replace("SESSION", SESSION_ID),
    );
    let scheduler = dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(child)),
    );
    let agent_id = enqueue_dsh(
        &scheduler,
        workspace.path(),
        Some("fixture-provider:fixture-model"),
    );
    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
    let request = await_pending_permission(&scheduler, &agent_id);
    let service = Arc::new(RpcService::new(scheduler.clone(), scheduler.store()).unwrap());
    let began = service.handle_bytes(
        &serde_json::to_vec(&serde_json::json!({
            "request_id": "passive-drain",
            "method": "daemon_begin_drain"
        }))
        .unwrap(),
    );
    assert!(matches!(began.outcome, RpcOutcome::Success { .. }));

    // During the drain: the existing provider session still answers and a
    // new spawn is refused by the one admission owner.
    service
        .dispatch(RpcMethod::TaskWait(TaskWaitQuery {
            agent_id: agent_id.clone(),
            wait_time: 0,
            message_id: None,
        }))
        .unwrap();
    let fresh = dsh_workspace();
    assert!(matches!(
        scheduler.enqueue_general_with_admission(
            &manifest_for(fresh.path(), "spawn during drain"),
            Some(dsh_admission(Some("fixture-provider:fixture-model")))
        ),
        Err(SchedulerError::InvalidConfig(ref message)) if message == "daemon_draining"
    ));

    // The failed update recovers through the wire: abort reopens
    // admission without a second scheduler and without touching the
    // drained task's facts.
    let aborted = service.handle_bytes(
        &serde_json::to_vec(&serde_json::json!({
            "request_id": "abort-drain",
            "method": "daemon_abort_drain"
        }))
        .unwrap(),
    );
    let RpcOutcome::Success { result } = aborted.outcome else {
        panic!("abort drain failed")
    };
    assert!(matches!(
        *result,
        RpcSuccess::DaemonDrainStatus {
            is_draining: false,
            ready_for_activation: false,
            ..
        }
    ));
    let readmitted = scheduler
        .enqueue_general_with_admission(
            &manifest_for(fresh.path(), "spawn after the aborted drain"),
            Some(dsh_admission(Some("fixture-provider:fixture-model"))),
        )
        .unwrap();
    assert_eq!(readmitted.phase, TaskPhase::Queued);

    // The drained task keeps its evidence: still answerable over RPC,
    // with its pending permission intact and nothing reaped underneath
    // the recovery.
    let RpcSuccess::TaskWait { task, .. } = service
        .dispatch(RpcMethod::TaskWait(TaskWaitQuery {
            agent_id: agent_id.clone(),
            wait_time: 0,
            message_id: None,
        }))
        .unwrap()
    else {
        panic!("wait")
    };
    assert_eq!(task.agent_id, agent_id);
    assert_ne!(task.status, "closed");
    let still = scheduler.store().pending_requests(&agent_id).unwrap();
    assert_eq!(
        still.first().map(|pending| pending.request_id.clone()),
        Some(request.request_id)
    );
    // New business traffic flows again immediately.
    service
        .dispatch(RpcMethod::TaskMessage(MessageInput {
            agent_id: agent_id.clone(),
            message_id: Some("after-abort".into()),
            content: "queued once admission reopened".into(),
        }))
        .unwrap();

    // The unit-variant wire shape holds: explicit params are rejected
    // instead of silently ignored.
    let shaped = service.handle_bytes(
        &serde_json::to_vec(&serde_json::json!({
            "request_id": "abort-params",
            "method": "daemon_abort_drain", "params": {}
        }))
        .unwrap(),
    );
    assert!(matches!(
        shaped.outcome,
        RpcOutcome::Error {
            error: RpcError {
                code: RpcErrorCode::Validation,
                ..
            }
        }
    ));

    // Leave no stray provider process behind.
    service
        .dispatch(RpcMethod::TaskCancel {
            agent_id: agent_id.clone(),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while scheduler.active_count() > 0 {
        assert!(
            Instant::now() < deadline,
            "pending scripted child was never reaped"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn abort_drain_is_refused_while_explicit_cancellation_is_in_flight() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    // The child parks on a pending permission, so the explicit
    // cancellation worker stays busy reaping it for a real interval.
    let child = scripted_child(
        workspace.path(),
        &BOOTSTRAP_PREFIX.replace("SESSION", SESSION_ID),
    );
    let scheduler = dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(child)),
    );
    let agent_id = enqueue_dsh(
        &scheduler,
        workspace.path(),
        Some("fixture-provider:fixture-model"),
    );
    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
    let _request = await_pending_permission(&scheduler, &agent_id);
    scheduler.begin_drain();
    scheduler.cancel_draining_tasks().unwrap();
    // The bounded abort must refuse instead of reopening admission
    // underneath an in-flight `--cancel-active` worker.
    match scheduler.abort_drain() {
        Err(SchedulerError::InvalidConfig(ref message))
            if message == "drain_cancel_in_progress" => {}
        other => panic!("expected an in-flight-cancellation refusal, got {other:?}"),
    }
    assert!(scheduler.is_draining());
    // Once the worker finishes the reap, the same abort succeeds and the
    // daemon reopens admission.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if scheduler.abort_drain().is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "abort never became possible after cancellation settled"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!scheduler.is_draining());
}

#[test]
fn queued_only_drain_cannot_claim_activation_but_default_drain_can_start_admitted_work() {
    let workspace = dsh_workspace();
    let scheduler = dsh_scheduler(workspace.path(), DshRuntimeFactory::closed());
    let id = enqueue_dsh(&scheduler, workspace.path(), None);
    scheduler.begin_drain();
    assert_eq!(scheduler.active_count(), 0);
    assert!(!scheduler.resources_reaped());
    assert!(!scheduler.ready_for_activation());
    assert!(scheduler.claim_activation().is_none());
    // Default drain still permits the admitted queue to run. Using the
    // durable production claim owner isolates this from provider setup.
    let claim = scheduler
        .store()
        .claim_next("default-drain", 10, 1)
        .unwrap()
        .unwrap();
    assert_eq!(claim.task.agent_id, id);
    assert!(!scheduler.ready_for_activation());
}

#[test]
fn manifest_build_spawn_materializes_a_patch_and_observes_the_caller_manifest() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    let (_fake_root, runtime) = manifest_fake_dsh(workspace.path());
    let scheduler = dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(runtime)),
    );
    let agent_id = enqueue_dsh_with_manifest(&scheduler, workspace.path(), &["src/a.rs", "docs"]);
    assert_eq!(scheduler.start_ready().unwrap(), vec![agent_id.clone()]);
    let stored = await_result(&scheduler, &agent_id);
    assert_eq!(stored.result.outcome, TaskOutcome::Completed);
    assert_eq!(stored.result.final_text, "manifest build done");

    // The scripted fake runtime recorded its full argv: the ACP spawn (and the
    // preflight probes) carry an absolute `--patch` under the
    // `external-dsh-manifest-` TempDir prefix.
    let argv = std::fs::read_to_string(workspace.path().join("argv.log")).unwrap();
    let args: Vec<&str> = argv
        .lines()
        .filter_map(|line| line.strip_prefix("ARG"))
        .collect();
    let patch_index = args
        .iter()
        .position(|arg| *arg == "--patch")
        .expect("manifest spawn must carry --patch");
    let patch = args[patch_index + 1];
    assert!(std::path::Path::new(patch).is_absolute(), "{patch}");
    assert!(
        std::path::Path::new(patch)
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("external-dsh-manifest-"),
        "{patch}"
    );

    // The patch content is the S02 constructor output: a guard insert line
    // carrying the caller manifest verbatim, with tool-fs kept enabled and
    // bash disabled. It is the daemon TempDir materialization, never a
    // source-tree file.
    let patch_log = std::fs::read_to_string(workspace.path().join("patch.log")).unwrap();
    assert!(patch_log.contains("- insert:"), "{patch_log}");
    assert!(patch_log.contains("      - src/a.rs"), "{patch_log}");
    assert!(patch_log.contains("      - docs"), "{patch_log}");
    assert!(!patch_log.contains("id: tool-fs"), "{patch_log}");
    assert!(
        patch_log.contains("- id: tool-bash\n  disabled: true"),
        "{patch_log}"
    );
    assert!(!patch.contains("external-subagent/plugins"), "{patch}");
}
