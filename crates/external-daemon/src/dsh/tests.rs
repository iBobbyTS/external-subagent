#![cfg(unix)]
//! Scripted ACP contract tests for the DSH adapter: gates, routing,
//! bootstrap ordering, permission/input correlation, settlement folding,
//! cancellation, and drain lifecycle through the shared scheduler.

use super::*;
use crate::{
    task_agent, terminal_proves_process_group_reaped, CommandRuntimeFactory, LifecycleSink,
    ManagedRuntime, MessageDisposition, ResponseDisposition, RuntimeCommandError, RuntimeFactory,
    RuntimeTerminal, Scheduler, SchedulerConfig, SchedulerError, SessionReady, TurnSnapshot,
    RuntimeState,
};
use external_contract::{EventEnvelope, WireMessage};
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
        developer_instructions: None,
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
/// windows instead of a tight wall-clock guess. Degraded macOS CI runners
/// have measured this sync beyond 30s (v0.3.1 release runs), so the budget
/// must absorb runner slowdown, not just local load.
const SCRIPTED_SYNC_WAIT: Duration = Duration::from_secs(120);

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
// scheduler and lifecycle contract is shared across providers.
fn scheduler_over(workspace: &std::path::Path, factory: Arc<dyn RuntimeFactory>) -> Scheduler {
    let store = Arc::new(external_store::Store::open(workspace.join("state.sqlite")).unwrap());
    let config = SchedulerConfig {
        bootstrap_timeout: Duration::from_secs(30),
        control_timeout: Duration::from_secs(10),

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
        .prefix("dsh-")
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
        permission_mode: Some(PermissionMode::Build),
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
        permission_mode: Some(PermissionMode::Build),
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
/// `--dump-config` for the manifest-build preflight and then speaks the
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
/// `--patch` TempDir so the dump validation sees the guard the daemon just
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
- id: workflow-ptc
  disabled: true
- id: ptc-runtime
  disabled: true
- id: tool-goal
  disabled: true
- id: tool-ralph
  disabled: true
- id: skill-filesystem
  disabled: true
- id: config-editor
  disabled: true
- id: plugin-manager
  disabled: true
- id: mcp-resources
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
    fn emit(&self, _record: crate::RuntimeEnvelope) {}
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
  '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"SESSION","update":{"type":"tool_call","toolCallId":"tool-7","kind":"edit","title":"Edit fixture","rawInput":{"path":"a.ts","content":"中"}}}}' \
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
    // identity (X06): the toolCallId came from a prior tool update, and the
    // name is the ACP title, not the `edit` category.
    let request = await_pending_permission(&scheduler, &agent_id);
    let payload: serde_json::Value = serde_json::from_str(&request.payload_json).unwrap();
    assert_eq!(payload["toolName"], "Edit fixture");
    assert_eq!(payload["toolCallId"], "tool-7");
    let activity = scheduler.passive_activity_snapshot(&agent_id).unwrap();
    assert_eq!(activity.window_60s.tool_calls_started, 1);
    // The daemon attaches the ACP rawInput to the started frame, so observe
    // sees the real arguments instead of the empty-input fallback.
    let observation = scheduler.observation_snapshot(&agent_id);
    assert_eq!(observation.tools[0].tool_name, "Edit fixture");
    assert_eq!(observation.tools[0].call_count, 1);
    assert_eq!(
        observation.tools[0].recent_calls[0].arguments,
        serde_json::json!({"content":"中","path":"a.ts"})
            .as_object()
            .unwrap()
            .clone()
    );
    assert!(!observation.tools[0].recent_calls[0].arguments_truncated);
    assert!(observation.coverage.tool_history_complete);

    let service = crate::rpc::RpcService::new(scheduler.clone(), scheduler.store()).unwrap();
    let error = service
        .dispatch(crate::rpc::RpcMethod::TaskMessage(
            crate::rpc::MessageInput {
                agent_id: agent_id.clone(),
                message_id: Some("unsupported-steer".into()),
                mode: "steer".into(),
                content: "new direction".into(),
            },
        ))
        .unwrap_err();
    assert_eq!(error.code, crate::rpc::RpcErrorCode::SteerUnsupported);
    assert!(scheduler
        .store()
        .message("unsupported-steer")
        .unwrap()
        .is_none());

    // Queue the follow-up while the turn is still blocked on permission;
    // delivery may only happen after settlement.
    assert_eq!(
        scheduler
            .send_message(&agent_id, "follow-up", "queue", "follow-up prompt")
            .unwrap(),
        MessageDisposition::Queued
    );

    assert_eq!(
        scheduler
            .send_message(
                &agent_id,
                "second-follow-up",
                "queue",
                "second follow-up prompt"
            )
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
            .send_message(&agent_id, "follow-up", "queue", "follow-up prompt")
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
    use crate::rpc::{RpcMethod, RpcService, RpcSuccess, TaskWaitBody, TaskWaitQuery, WaitTarget};
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
    let RpcSuccess::TaskWait(TaskWaitBody {
        pending_requests,
        timed_out,
        ..
    }) = service
        .dispatch(RpcMethod::TaskWait(TaskWaitQuery {
            wait_mode: crate::rpc::WaitMode::All,
            agent_id: WaitTarget::One(agent_id.clone()),
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
    assert_eq!(pending_requests[0].summary, None);
    assert_eq!(
        pending_requests[0].question.as_ref().unwrap().text,
        "pick a scope"
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
    use crate::rpc::{RpcMethod, RpcService, RpcSuccess, TaskWaitBody, TaskWaitQuery, WaitTarget};
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
    let RpcSuccess::TaskWait(TaskWaitBody {
        pending_requests,
        timed_out,
        ..
    }) = service
        .dispatch(RpcMethod::TaskWait(TaskWaitQuery {
            wait_mode: crate::rpc::WaitMode::All,
            agent_id: WaitTarget::One(agent_id.clone()),
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
    use crate::rpc::{RpcMethod, RpcService, RpcSuccess, TaskWaitBody, TaskWaitQuery, WaitTarget};
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
    let RpcSuccess::TaskWait(TaskWaitBody {
        pending_requests,
        timed_out,
        ..
    }) = service
        .dispatch(RpcMethod::TaskWait(TaskWaitQuery {
            wait_mode: crate::rpc::WaitMode::All,
            agent_id: WaitTarget::One(agent_id.clone()),
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
    assert_eq!(wake.summary, None);
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

/// The peer's JSON-RPC error is delivery evidence: a prompt rejected with an
/// application error must surface that whole object, never the transport
/// layer's generic label. The peer sends `message` only, so no code/status
/// field upstream removed is fabricated; whatever it did send travels intact.
#[test]
fn remote_jsonrpc_error_settlement_preserves_the_complete_error() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    const REMOTE_ERROR_SETTLEMENT: &str = r#"
printf '%s\n' '{"jsonrpc":"2.0","id":4,"error":{"code":-32000,"message":"quota exhausted for the fixture account","data":{"retryAfter":30,"limit":"daily"}}}'
"#;
    let script = format!("{BOOTSTRAP_PREFIX}{REMOTE_ERROR_SETTLEMENT}").replace("SESSION", SESSION_ID);
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
    let delivered = &stored.result.final_text;
    assert_ne!(
        delivered, "runtime returned an application error",
        "the generic transport label must not replace the native error"
    );
    let native: serde_json::Value = serde_json::from_str(delivered)
        .unwrap_or_else(|_| panic!("delivered text is not the native error object: {delivered}"));
    assert_eq!(native["code"], -32000);
    assert_eq!(native["message"], "quota exhausted for the fixture account");
    assert_eq!(native["data"]["retryAfter"], 30);
    assert_eq!(native["data"]["limit"], "daily");
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Failed));
    // The diagnostic record keeps the runtime-terminal stage around the peer's
    // own message instead of the generic transport label.
    let failure = scheduler.last_error(&agent_id).expect("failure record");
    assert!(
        failure.contains("quota exhausted for the fixture account"),
        "{failure}"
    );
    assert!(
        !failure.contains("runtime returned an application error"),
        "{failure}"
    );
}

/// The upstream standard settlement carries nothing but `stopReason`: no
/// committed flag and no message id. The adapter must still complete the turn
/// from its prompt-scoped text aggregation.
#[test]
fn standard_chunks_and_stop_reason_settle_without_a_message_id() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    const STANDARD_SETTLEMENT: &str = r#"
printf '%s\n' \
  '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"SESSION","update":{"sessionUpdate":"agent_message_chunk","messageId":"native-message","content":{"type":"text","text":"standard "}}}}' \
  '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"SESSION","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answer"}}}}' \
  '{"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}'
"#;
    let script =
        format!("{BOOTSTRAP_PREFIX}{STANDARD_SETTLEMENT}").replace("SESSION", SESSION_ID);
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
    assert_eq!(stored.result.outcome, TaskOutcome::Completed);
    assert_eq!(stored.result.final_text, "standard answer");
    assert!(!stored.result.partial);
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Completed));
}

/// Synthetic native frames: a turn aggregate must never be replayed under m2.
#[test]
fn repair_same_text_messages_have_no_fabricated_terminal_wait_suffix() {
    use crate::rpc::{RpcMethod, RpcService, RpcSuccess, TaskWaitQuery, WaitTarget};
    let _guard = scripted_test_guard();
    // Fresh tasks keep scalar/list checks independent of take-on-delivery.
    for many in [false, true] {
        let workspace = dsh_workspace();
        let script = format!(r#"{BOOTSTRAP_PREFIX}
printf '%s\n' \
 '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"SESSION","update":{{"sessionUpdate":"agent_message_chunk","messageId":"m1","content":{{"type":"text","text":"A"}}}}}}}}' \
 '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"SESSION","update":{{"sessionUpdate":"agent_message_chunk","messageId":"m2","content":{{"type":"text","text":"A"}}}}}}}}' \
 '{{"jsonrpc":"2.0","id":4,"result":{{"stopReason":"end_turn"}}}}'
"#).replace("SESSION", SESSION_ID);
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
        assert_eq!(stored.result.final_text, "AA");
        assert_eq!(stored.result.outcome, TaskOutcome::Completed);
        let activity = scheduler.passive_activity_snapshot(&agent_id).unwrap();
        assert_eq!(activity.latest_text_tail, stored.result.final_text);
        assert_eq!(activity.window_60s.text_delta_events, 2);
        let service = RpcService::new(scheduler.clone(), scheduler.store()).unwrap();
        let response = service
            .dispatch(RpcMethod::TaskWait(TaskWaitQuery {
                wait_mode: crate::rpc::WaitMode::All,
                agent_id: if many {
                    WaitTarget::Many(vec![agent_id.clone()])
                } else {
                    WaitTarget::One(agent_id.clone())
                },
                wait_time: 0,
                message_id: None,
            }))
            .unwrap();
        let body = match response {
            RpcSuccess::TaskWait(body) => body,
            RpcSuccess::TaskWaitMany { mut waits } => waits.bodies.remove(0).1,
            _ => panic!("expected wait body"),
        };
        assert_eq!(body.result.unwrap().final_text, stored.result.final_text);
        assert!(
            body.activity.latest_text_tail.is_empty(),
            "fabricated wait tail: {:?}",
            body.activity.latest_text_tail
        );
    }
}

/// A native `status: failed` is a failed call, not a completed one. The tool
/// keeps its real name (the ACP title) even though the generic category is
/// `other`, and ending the call does not fail the turn by itself.
#[test]
fn failed_tool_status_is_not_a_success_and_is_named_by_its_title() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    const FAILED_TOOL_SETTLEMENT: &str = r#"
printf '%s\n' \
  '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"SESSION","update":{"sessionUpdate":"tool_call","toolCallId":"tool-9","title":"probe the workspace","kind":"other","status":"in_progress","rawInput":{"path":"."}}}}' \
  '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"SESSION","update":{"sessionUpdate":"tool_call_update","toolCallId":"tool-9","status":"failed","content":[{"type":"text","text":"permission denied"}]}}}' \
  '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"SESSION","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"tool failure noted"}}}}' \
  '{"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}'
"#;
    let script = format!("{BOOTSTRAP_PREFIX}{FAILED_TOOL_SETTLEMENT}").replace("SESSION", SESSION_ID);
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
    assert_eq!(stored.result.outcome, TaskOutcome::Completed);
    assert_eq!(stored.result.final_text, "tool failure noted");

    let activity = scheduler
        .passive_activity_snapshot(&agent_id)
        .expect("activity snapshot");
    // Bootstrap announced tool-7; this turn announced tool-9. The failed call
    // counts as failed, never as completed.
    assert_eq!(activity.window_60s.tool_calls_started, 2);
    assert_eq!(activity.window_60s.tool_calls_completed, 0);
    assert_eq!(activity.window_60s.tool_calls_failed, 1);
    // The authoritative turn settlement clears this turn's activity residue, so
    // nothing is left in flight once the turn completes.
    assert!(activity.active_tools.is_empty(), "{activity:?}");

    let observation = scheduler.observation_snapshot(&agent_id);
    let names: Vec<&str> = observation
        .tools
        .iter()
        .map(|tool| tool.tool_name.as_str())
        .collect();
    assert!(names.contains(&"probe the workspace"), "{names:?}");
    assert!(
        !names.contains(&"other"),
        "the generic category must not name a tool: {names:?}"
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
    assert!(failure.contains("MODEL_REJECTED"), "{failure}");
    assert!(!failure.contains("SESSION_START_FAILED"), "{failure}");
    assert!(failure.contains("unknown model option"), "{failure}");

    let stored = await_result(&scheduler, &agent_id);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored.result.final_text).unwrap(),
        serde_json::json!({"code":-32602,"message":"unknown model option: nope"})
    );
    assert_eq!(
        scheduler
            .store()
            .terminal_reason_code(&agent_id)
            .unwrap()
            .as_deref(),
        Some("MODEL_REJECTED")
    );

    let frames = wire_frames(workspace.path());
    assert_eq!(
        request_methods(&frames),
        vec!["initialize", "session/new", "session/set_config_option"],
        "no prompt may follow a refused model selection"
    );
}

#[test]
fn model_rejection_without_a_server_message_falls_back_to_the_whole_object() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    let script = format!(
        r#"
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"capabilities":{{"models":true,"cancel":true,"permission":true}}}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"{SESSION_ID}","configOptions":[{{"configId":"model"}}]}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"error":{{"code":-32603}}}}'
"#
    );
    let child = scripted_child(workspace.path(), &script);
    let scheduler = dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(child)),
    );
    let agent_id = enqueue_dsh(&scheduler, workspace.path(), Some("fixture-provider:nope"));
    scheduler.start_ready().unwrap_err();

    let stored = await_result(&scheduler, &agent_id);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored.result.final_text).unwrap(),
        serde_json::json!({"code":-32603})
    );
    assert_eq!(
        scheduler
            .store()
            .terminal_reason_code(&agent_id)
            .unwrap()
            .as_deref(),
        Some("MODEL_REJECTED")
    );
}

#[test]
fn model_rejection_uses_the_not_offered_session_display() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    let script = format!(
        r#"
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"capabilities":{{"models":true,"cancel":true,"permission":true}}}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"{SESSION_ID}","configOptions":[{{"configId":"reasoning_effort"}}]}}}}'
"#
    );
    let child = scripted_child(workspace.path(), &script);
    let scheduler = dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(child)),
    );
    let agent_id = enqueue_dsh(&scheduler, workspace.path(), Some("fixture-provider:nope"));
    scheduler.start_ready().unwrap_err();

    let stored = await_result(&scheduler, &agent_id);
    assert_eq!(
        stored.result.final_text,
        "model selection was rejected: dsh session advertised no model config option"
    );
    assert_eq!(
        scheduler
            .store()
            .terminal_reason_code(&agent_id)
            .unwrap()
            .as_deref(),
        Some("MODEL_REJECTED")
    );
    let frames = wire_frames(workspace.path());
    assert_eq!(
        request_methods(&frames),
        vec!["initialize", "session/new"],
        "a not-offered model must never emit a config frame"
    );
}

#[test]
fn non_model_bootstrap_failures_keep_session_start_failed_and_the_legacy_message() {
    let _guard = scripted_test_guard();
    // Initialize shape error: the session never reaches set_model.
    let shape_workspace = dsh_workspace();
    let shape_script = r#"
read_frame
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"bogus"}}'
"#;
    let shape_child = scripted_child(shape_workspace.path(), shape_script);
    let shape_scheduler = dsh_scheduler(
        shape_workspace.path(),
        DshRuntimeFactory::test_harness(Some(shape_child)),
    );
    let shape_agent = enqueue_dsh(
        &shape_scheduler,
        shape_workspace.path(),
        Some("fixture-provider:fixture-model"),
    );
    shape_scheduler.start_ready().unwrap_err();
    let shape_stored = await_result(&shape_scheduler, &shape_agent);
    assert_eq!(
        shape_stored.result.final_text,
        "invalid session response: dsh acp shape error: initialize result is missing protocolVersion"
    );
    assert_eq!(
        shape_scheduler
            .store()
            .terminal_reason_code(&shape_agent)
            .unwrap()
            .as_deref(),
        Some("SESSION_START_FAILED")
    );

    // Transport failure during set_model: the child exits before answering,
    // which must stay an invalid-session failure and never MODEL_REJECTED.
    let transport_workspace = dsh_workspace();
    let transport_script = format!(
        r#"
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"capabilities":{{"models":true,"cancel":true,"permission":true}}}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"{SESSION_ID}","configOptions":[{{"configId":"model"}}]}}}}'
read_frame
exit 0
"#
    );
    let transport_child = scripted_child(transport_workspace.path(), &transport_script);
    let transport_scheduler = dsh_scheduler(
        transport_workspace.path(),
        DshRuntimeFactory::test_harness(Some(transport_child)),
    );
    let transport_agent = enqueue_dsh(
        &transport_scheduler,
        transport_workspace.path(),
        Some("fixture-provider:fixture-model"),
    );
    transport_scheduler.start_ready().unwrap_err();
    let transport_stored = await_result(&transport_scheduler, &transport_agent);
    assert!(
        transport_stored
            .result
            .final_text
            .starts_with("invalid session response: "),
        "{}",
        transport_stored.result.final_text
    );
    assert_eq!(
        transport_scheduler
            .store()
            .terminal_reason_code(&transport_agent)
            .unwrap()
            .as_deref(),
        Some("SESSION_START_FAILED")
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
    tracker: Arc<RuntimeState>,
    terminal: Mutex<Option<RuntimeTerminal>>,
    terminal_changed: Condvar,
}

impl FakeProviderRuntime {
    fn new() -> Self {
        Self {
            session_id: "fake-provider-session".into(),
            tracker: Arc::new(RuntimeState::new()),
            terminal: Mutex::new(None),
            terminal_changed: Condvar::new(),
        }
    }

    fn turn_event(&self, kind: &str) {
        self.tracker.test_turn(match kind {
            "turn.started" => None,
            "turn.completed" => Some(crate::TurnBoundary::Completed),
            "turn.failed" => Some(crate::TurnBoundary::Failed),
            _ => panic!("unsupported fixture boundary"),
        });
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
        let current = self.tracker.turn_snapshot();
        if !current.active {
            return Ok(current);
        }
        self.turn_event("turn.completed");
        self.tracker
            .wait_boundary_after(current.generation, timeout)
    }

    fn turn_snapshot(&self) -> TurnSnapshot {
        self.tracker.turn_snapshot()
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
    // admitted and stays active in the shared workspace.
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

    // A second provider is admitted while the first remains active.
    let second = enqueue_dsh(
        &scheduler,
        workspace.path(),
        Some("fixture-provider:fixture-model"),
    );
    assert_eq!(scheduler.start_ready().unwrap(), vec![second.clone()]);
    let _request = await_pending_permission(&scheduler, &second);
    assert_eq!(scheduler.active_count(), 2);

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
    // Cancelling one actor preserves the other actor's live turn.
    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    while scheduler.active_count() != 1 {
        assert!(Instant::now() < deadline, "first runtime was not reaped");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(scheduler.store().get_task(&second).unwrap().unwrap().phase, TaskPhase::WaitingInput);
    assert_eq!(scheduler.cancel_task(&first).unwrap(), TaskPhase::Terminal);
    assert_eq!(fake.spawns.load(Ordering::Acquire), 1);

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
    use crate::rpc::{
        MessageInput, RpcMethod, RpcOutcome, RpcService, RpcSuccess, TaskWaitQuery, WaitTarget,
    };
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
            mode: "queue".into(),
        }))
        .unwrap_err();
    assert_eq!(send.message, "daemon_draining");
    assert!(scheduler.store().message("after-drain").unwrap().is_none());
    service
        .dispatch(RpcMethod::TaskWait(TaskWaitQuery {
            wait_mode: crate::rpc::WaitMode::All,
            agent_id: WaitTarget::One(agent_id.clone()),
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
            wait_mode: crate::rpc::WaitMode::All,
            agent_id: WaitTarget::One(agent_id.clone()),
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
        TaskWaitBody, TaskWaitQuery, WaitTarget,
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
            wait_mode: crate::rpc::WaitMode::All,
            agent_id: WaitTarget::One(agent_id.clone()),
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
    let RpcSuccess::TaskWait(TaskWaitBody { task, .. }) = service
        .dispatch(RpcMethod::TaskWait(TaskWaitQuery {
            wait_mode: crate::rpc::WaitMode::All,
            agent_id: WaitTarget::One(agent_id.clone()),
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
            mode: "queue".into(),
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
        .claim_next("default-drain")
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

    // The patch content is the manifest-build constructor output: a guard insert line
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

const DSH_TRANSPORT_REASON: &str = "RUNTIME_TRANSPORT_FRAME_LIMIT";

fn dsh_event_count(workspace: &std::path::Path, agent_id: &str, event_type: &str) -> i64 {
    let connection = rusqlite::Connection::open(workspace.join("state.sqlite")).unwrap();
    connection
        .query_row(
            "SELECT COUNT(*) FROM events WHERE agent_id=?1 AND event_type=?2",
            rusqlite::params![agent_id, event_type],
            |row| row.get(0),
        )
        .unwrap()
}

/// The oversized-frame contract: a DSH provider that emits an oversized ACP frame and keeps
/// running fails explicitly instead of hanging RUNNING.
#[test]
fn dsh_oversized_frame_fails_explicitly_and_reaps_the_provider() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    let script = format!(
        r#"
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"capabilities":{{"models":true,"cancel":true,"permission":true}}}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"{SESSION_ID}","configOptions":[{{"configId":"model"}}]}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"configOptions":[]}}}}'
read_frame
head -c {bytes} /dev/zero
while [ ! -f test-release ]; do sleep 0.05; done
"#,
        bytes = external_runtime::MAX_NDJSON_LINE_BYTES + 1,
    );
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
    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    let pgid = loop {
        let task = scheduler.store().get_task(&agent_id).unwrap().unwrap();
        if task.phase == TaskPhase::Running {
            if let Some(identity) = task.process_identity.clone() {
                break identity.process_group_id;
            }
        }
        assert!(Instant::now() < deadline, "task never reached RUNNING");
        thread::sleep(Duration::from_millis(10));
    };
    let result = await_result(&scheduler, &agent_id);
    assert_eq!(result.result.outcome, TaskOutcome::Failed);
    assert_eq!(
        scheduler
            .store()
            .terminal_reason_code(&agent_id)
            .unwrap()
            .as_deref(),
        Some(DSH_TRANSPORT_REASON)
    );
    assert_eq!(
        dsh_event_count(workspace.path(), &agent_id, "driver.oversized_line"),
        1
    );
    // The provider blocked after its oversized write; no self-exit boundary
    // can explain the terminal.
    assert_eq!(
        dsh_event_count(workspace.path(), &agent_id, "driver.child_exited"),
        0
    );
    let task = await_terminal_task(&scheduler, &agent_id);
    assert_eq!(task.outcome, Some(TaskOutcome::Failed));
    assert!(task.reaped_at.is_some());
    assert!(external_runtime::observe_process_group(pgid)
        .unwrap()
        .is_empty());
    let record: serde_json::Value =
        serde_json::from_str(&scheduler.last_error(&agent_id).unwrap()).unwrap();
    assert_eq!(record["stage"], "transport");
    assert_eq!(record["error_code"], DSH_TRANSPORT_REASON);
    assert!(
        record["cleanup_result"]
            .as_str()
            .unwrap()
            .contains("Stopped"),
        "{record}"
    );
    assert_eq!(
        scheduler.store().task_result(&agent_id).unwrap().unwrap(),
        result
    );
}

#[test]
fn repeated_acp_tool_call_notification_keeps_captured_raw_input() {
    let (shared, tracker) = tool_projection_tracker();
    let update = serde_json::json!({"sessionUpdate":"tool_call","toolCallId":"tool-7","title":"edit a.ts","kind":"other",
        "status":"in_progress","rawInput":{"path":"a.ts","content":"中"}});
    deliver_tool_update(&shared, update.clone());
    let before = tracker.observation_snapshot();
    deliver_tool_update(&shared, update);
    let after = tracker.observation_snapshot();
    assert_eq!(after, before);
    assert_eq!(after.tools[0].tool_name, "edit a.ts");
    assert_eq!(after.tools[0].call_count, 1);
    assert_eq!(after.tools[0].recent_calls[0].arguments["path"], "a.ts");
    assert!(!after.tools[0].recent_calls[0].arguments_truncated);
}

#[test]
fn terminal_send_keeps_rejecting_dsh_even_with_a_persisted_session_id() {
    // dsh owns no ACP session re-entry (its owner re-runs the full bootstrap
    // on every claim), so it stays in the terminal-send exclusion set: a
    // terminal dsh task must keep the clean admission rejection even when a
    // session id is persisted, instead of requeueing into a resume that
    // replays the initial prompt on a fresh session.
    let workspace = dsh_workspace();
    let scheduler = dsh_scheduler(workspace.path(), DshRuntimeFactory::closed());
    let agent_id = enqueue_dsh(
        &scheduler,
        workspace.path(),
        Some("fixture-provider:fixture-model"),
    );
    let store = scheduler.store();
    let claim = store
        .claim_next("terminal-dsh")
        .unwrap()
        .unwrap();
    assert_eq!(claim.task.agent_id, agent_id);
    store
        .mark_session_running(
            &agent_id,
            claim.owner_epoch,
            "runtime",
            None,
            Some(SESSION_ID),
            Some(external_store::TurnState::Idle),
        )
        .unwrap();
    store
        .store_task_result(
            &agent_id,
            &external_store::TaskResult {
                outcome: TaskOutcome::Completed,
                final_text: "done".into(),
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
    let error = scheduler
        .send_message(&agent_id, "dsh-terminal-msg", "queue", "continue")
        .unwrap_err();
    assert!(error.to_string().contains("TERMINAL_SEND_UNSUPPORTED"));
    assert!(store.message("dsh-terminal-msg").unwrap().is_none());
    // The terminal row was not requeued.
    let task = store.get_task(&agent_id).unwrap().unwrap();
    assert_eq!(task.phase, TaskPhase::Terminal);
}

/// Exercise the real adapter projector and observation/activity consumers.
fn tool_projection_tracker() -> (
    super::events::DshRuntimeShared,
    Arc<crate::RuntimeState>,
) {
    struct TrackerSink(Arc<crate::RuntimeState>);
    impl LifecycleSink for TrackerSink {
        fn runtime_state(&self) -> Option<Arc<crate::RuntimeState>> { Some(self.0.clone()) }
        fn emit(&self, record: crate::RuntimeEnvelope) {
        }
    }
    let tracker = Arc::new(crate::RuntimeState::new());
    let publisher = Arc::new(crate::Publisher::new(Arc::new(TrackerSink(Arc::clone(
        &tracker,
    )))));
    let (task, launch) = publisher.event_scope();
    let shared = super::events::DshRuntimeShared {
        publisher: Arc::clone(&publisher),
        runtime_state: Arc::clone(&publisher.runtime_state),
        task,
        launch,
        offers: Mutex::new(Default::default()),
        tool_names: Mutex::new(Default::default()),
        messages: Mutex::new(Default::default()),
        session_id: Mutex::new(Some("s".into())),
        current_prompt: Mutex::new(None),
        last_prompt: Mutex::new(None),
        settled_turn: Mutex::new(None),
        stop_boundaries: std::sync::atomic::AtomicU64::new(0),
    };
    shared.begin_turn(1);
    (shared, tracker)
}

fn deliver_tool_update(shared: &super::events::DshRuntimeShared, update: serde_json::Value) {
    let event = Inbound::Message(WireMessage::UnknownEvent {
        method: "session/update".into(),
        raw: serde_json::json!({"params":{"sessionId":"s","update":update}}),
    });
    assert!(super::events::project_dsh_inbound(shared, &event).is_none());
}

fn assert_pending_tool_counters(tool_name: &str, input: serde_json::Value) {
    let (shared, tracker) = tool_projection_tracker();
    let pending = serde_json::json!({"sessionUpdate":"tool_call","toolCallId":"c",
        "status":"pending","title":tool_name,"kind":"other","rawInput":input});
    deliver_tool_update(&shared, pending.clone());
    deliver_tool_update(&shared, pending); // Fresh envelopes, same call.
    let scheduled = tracker.snapshot();
    assert_eq!(scheduled.window_60s.tool_calls_started, 0);
    assert_eq!(scheduled.window_60s.read_calls, 0);
    assert_eq!(scheduled.window_60s.bash_calls, 0);
    assert_eq!(scheduled.window_60s.other_tool_calls, 0);
    let observed = tracker.observation_snapshot();
    assert_eq!(observed.tools.len(), 1, "pending descriptor was lost");
    assert_eq!(observed.tools[0].tool_name, tool_name);
    assert_eq!(observed.tools[0].call_count, 1);
    assert_eq!(&observed.tools[0].recent_calls[0].arguments, input.as_object().unwrap());
    let started = serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"c","status":"in_progress"});
    deliver_tool_update(&shared, started.clone());
    deliver_tool_update(&shared, started);
    let running = tracker.snapshot();
    assert_eq!(running.window_60s.tool_calls_started, 1);
    assert_eq!(running.window_60s.tool_calls_completed, 0);
    assert_eq!(running.window_60s.read_calls, u64::from(tool_name == "Read"), "{tool_name}");
    assert_eq!(running.window_60s.bash_calls, u64::from(tool_name == "Bash"), "{tool_name}");
    assert_eq!(running.window_60s.other_tool_calls, 0, "{tool_name}");
    let completed = serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"c","status":"completed"});
    deliver_tool_update(&shared, completed.clone());
    deliver_tool_update(&shared, completed);
    assert_eq!(tracker.snapshot().window_60s.tool_calls_completed, 1);
    assert_eq!(tracker.snapshot().window_60s.tool_calls_failed, 0);
    assert!(tracker.snapshot().active_tools.is_empty());
    let finished = tracker.observation_snapshot();
    assert_eq!(finished.tools[0].recent_calls[0].arguments, observed.tools[0].recent_calls[0].arguments);
    assert_eq!(finished.tools[0].call_count, observed.tools[0].call_count);
    assert_eq!(finished.tools[0].recent_calls[0].status, crate::observation::ObservedCallStatus::Succeeded);
}

#[test]
fn repair1_pending_tool_preserves_descriptor_without_premature_execution() {
    assert_pending_tool_counters("Read", serde_json::json!({"path":"a.ts"}));
}

#[test]
fn repair2_pending_bash_preserves_descriptor_and_activity_counters() {
    assert_pending_tool_counters("Bash", serde_json::json!({"command":"pwd"}));
}

fn assert_terminal_tool_announcement(status: &str) {
    let (shared, tracker) = tool_projection_tracker();
    let terminal = serde_json::json!({"sessionUpdate":"tool_call","toolCallId":"terminal",
        "status":status,"title":"Read","kind":"other","rawInput":{"path":"a.ts"}});
    deliver_tool_update(&shared, terminal.clone());
    let observed = tracker.observation_snapshot();
    assert_eq!(observed.tools.len(), 1, "{status} announcement descriptor was lost");
    assert_eq!(observed.tools[0].tool_name, "Read");
    assert_eq!(observed.tools[0].call_count, 1);
    assert_eq!(&observed.tools[0].recent_calls[0].arguments, serde_json::json!({"path":"a.ts"}).as_object().unwrap());
    assert!(!observed.tools[0].recent_calls[0].arguments_truncated);
    deliver_tool_update(&shared, terminal);
    let activity = tracker.snapshot();
    assert_eq!(activity.window_60s.tool_calls_started, 1);
    assert_eq!(activity.window_60s.tool_calls_completed, u64::from(status == "completed"));
    assert_eq!(activity.window_60s.tool_calls_failed, u64::from(status == "failed"));
    assert!(activity.active_tools.is_empty());
    assert_eq!(tracker.observation_snapshot(), observed, "replay must not recount");
}

#[test]
fn repair2_completed_announcement_preserves_observation_without_start() {
    assert_terminal_tool_announcement("completed");
}

#[test]
fn repair2_failed_announcement_preserves_observation_without_start() {
    assert_terminal_tool_announcement("failed");
}

fn projected_permission_tool_name(
    shared: &super::events::DshRuntimeShared,
    call_id: &str,
) -> String {
    let request = Inbound::Message(WireMessage::Request(
        external_contract::RequestEnvelope::new(
            external_contract::WireId::String(format!("permission-{call_id}")),
            "session/request_permission",
            serde_json::json!({"sessionId":"s","toolCallId":call_id,
            "options":[{"optionId":"allow-once","kind":"allow_once"}]}),
        ),
    ));
    let Some(Inbound::Message(WireMessage::Request(projected))) =
        super::events::project_dsh_inbound(shared, &request)
    else {
        panic!("permission request must be projected");
    };
    assert_eq!(
        projected.method,
        external_contract::INTERACTION_REQUEST_PERMISSION
    );
    projected.params["toolName"].as_str().unwrap().to_owned()
}

fn assert_finished_history_preserves_current_tool(tool_name: &str, input: serde_json::Value) {
    let (shared, tracker) = tool_projection_tracker();
    for index in 0..128 {
        let warmup = serde_json::json!({"sessionUpdate":"tool_call",
            "toolCallId":format!("warmup-{index}"),"status":"completed",
            "title":"Warmup","kind":"other","rawInput":{}});
        deliver_tool_update(&shared, warmup.clone());
        deliver_tool_update(&shared, warmup);
    }
    let warm = tracker.snapshot();
    assert_eq!(warm.window_60s.tool_calls_started, 128);
    assert_eq!(warm.window_60s.tool_calls_completed, 128);
    assert!(warm.active_tools.is_empty());

    let pending = serde_json::json!({"sessionUpdate":"tool_call","toolCallId":"current",
        "status":"pending","title":tool_name,"kind":"other","rawInput":input});
    deliver_tool_update(&shared, pending.clone());
    deliver_tool_update(&shared, pending);
    assert_eq!(tracker.snapshot().window_60s.tool_calls_started, 128);
    // Capture the actual permission correlation while still pending, without
    // echoing a title in the permission offer itself.
    let pending_name = projected_permission_tool_name(&shared, "current");
    let observed = tracker.observation_snapshot();
    let current = observed
        .tools
        .iter()
        .find(|tool| tool.tool_name == tool_name)
        .unwrap();
    assert_eq!(current.call_count, 1);
    assert_eq!(
        &current.recent_calls[0].arguments,
        input.as_object().unwrap()
    );
    assert!(!current.recent_calls[0].arguments_truncated);

    let started = serde_json::json!({"sessionUpdate":"tool_call_update",
        "toolCallId":"current","status":"in_progress"});
    deliver_tool_update(&shared, started.clone());
    deliver_tool_update(&shared, started);
    let running = tracker.snapshot();
    assert_eq!(running.window_60s.tool_calls_started, 129);
    assert_eq!(
        running.window_60s.read_calls,
        u64::from(tool_name == "Read"),
        "{tool_name}"
    );
    assert_eq!(
        running.window_60s.bash_calls,
        u64::from(tool_name == "Bash"),
        "{tool_name}"
    );
    assert_eq!(running.window_60s.other_tool_calls, 128);
    assert_eq!(running.active_tools.len(), 1);
    assert_eq!(running.active_tools[0].tool_call_id, "current");
    assert_eq!(
        running.active_tools[0].kind,
        if tool_name == "Read" {
            crate::PassiveToolKind::Read
        } else {
            crate::PassiveToolKind::Bash
        }
    );
    assert_eq!(pending_name, tool_name);
    assert_eq!(
        projected_permission_tool_name(&shared, "current"),
        tool_name
    );
    assert_eq!(shared.tool_names.lock().unwrap().len(), 1);
    let running_observation = tracker.observation_snapshot();
    let active = running_observation.tools.iter().find(|tool| tool.tool_name == tool_name).unwrap();
    assert_eq!(active.recent_calls[0].arguments, current.recent_calls[0].arguments);
    assert_eq!(active.call_count, current.call_count);
    assert_eq!(active.recent_calls[0].status, crate::observation::ObservedCallStatus::Running);

    let terminal = serde_json::json!({"sessionUpdate":"tool_call_update",
        "toolCallId":"current","status":"completed"});
    deliver_tool_update(&shared, terminal.clone());
    deliver_tool_update(&shared, terminal);
    let settled = tracker.snapshot();
    assert_eq!(settled.window_60s.tool_calls_started, 129);
    assert_eq!(settled.window_60s.tool_calls_completed, 129);
    assert_eq!(settled.window_60s.tool_calls_failed, 0);
    assert!(settled.active_tools.is_empty());
    let running_observation = tracker.observation_snapshot();
    let active = running_observation.tools.iter().find(|tool| tool.tool_name == tool_name).unwrap();
    assert_eq!(active.recent_calls[0].arguments, current.recent_calls[0].arguments);
    assert_eq!(active.call_count, current.call_count);
    assert_eq!(active.recent_calls[0].status, crate::observation::ObservedCallStatus::Succeeded);
    assert!(shared.tool_names.lock().unwrap().is_empty());
}

#[test]
fn repair3_completed_history_preserves_new_pending_read() {
    assert_finished_history_preserves_current_tool("Read", serde_json::json!({"path":"a.ts"}));
}

#[test]
fn repair3_completed_history_preserves_new_pending_bash() {
    assert_finished_history_preserves_current_tool("Bash", serde_json::json!({"command":"pwd"}));
}

#[test]
fn repair3_terminal_phases_release_only_finished_names() {
    let (shared, tracker) = tool_projection_tracker();
    deliver_tool_update(
        &shared,
        serde_json::json!({"sessionUpdate":"tool_call",
        "toolCallId":"keep-pending","status":"pending","title":"Read","rawInput":{"path":"a.ts"}}),
    );
    for kind in ["tool_call", "tool_call_update"] {
        for status in ["completed", "failed"] {
            let id = format!("{kind}-{status}");
            if kind == "tool_call_update" {
                deliver_tool_update(
                    &shared,
                    serde_json::json!({"sessionUpdate":"tool_call",
                    "toolCallId":id,"status":"pending","title":"Bash","rawInput":{"command":"pwd"}}),
                );
                assert_eq!(projected_permission_tool_name(&shared, &id), "Bash");
                deliver_tool_update(
                    &shared,
                    serde_json::json!({"sessionUpdate":"tool_call_update",
                    "toolCallId":id,"status":"in_progress"}),
                );
                assert_eq!(projected_permission_tool_name(&shared, &id), "Bash");
            }
            let terminal = serde_json::json!({"sessionUpdate":kind,"toolCallId":id,
                "status":status,"title":"Bash","rawInput":{"command":"pwd"}});
            deliver_tool_update(&shared, terminal.clone());
            let before_replay = tracker.snapshot().window_60s;
            deliver_tool_update(&shared, terminal);
            assert_eq!(tracker.snapshot().window_60s, before_replay);
            let names = shared.tool_names.lock().unwrap();
            assert!(
                !names.contains_key(&id),
                "{kind}/{status} retained finished history"
            );
            assert_eq!(names.get("keep-pending").map(String::as_str), Some("Read"));
            assert_eq!(names.len(), 1);
            drop(names);
            assert_eq!(
                projected_permission_tool_name(&shared, "keep-pending"),
                "Read"
            );
        }
    }
}

#[test]
fn repair1_large_prompt_result_has_explicit_limit_semantics() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    let updates = workspace.path().join("large-updates.jsonl");
    let chunk = |id: &str, text: String| {
        serde_json::json!({"jsonrpc":"2.0","method":"session/update",
        "params":{"sessionId":SESSION_ID,"update":{"sessionUpdate":"agent_message_chunk",
        "messageId":id,"content":{"type":"text","text":text}}}})
        .to_string()
    };
    // Synthetic sustained output: each frame is legal and below the 16 MiB
    // wire limit, while the stream exceeds the 2 MiB result cap sixteenfold.
    let mut frames = (0..32)
        .map(|_| chunk("first", "x".repeat(1024 * 1024)))
        .collect::<Vec<_>>();
    frames.push(chunk("last", "done".into()));
    frames.push(r#"{"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}"#.into());
    std::fs::write(&updates, frames.join("\n") + "\n").unwrap();
    drop(frames);
    let script = format!(
        "{}\ncat {:?}\n",
        BOOTSTRAP_PREFIX.replace("SESSION", SESSION_ID),
        updates
    );
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
    assert_eq!(
        stored.result.outcome,
        TaskOutcome::Failed,
        "capped text must not be completed"
    );
    assert!(
        stored.result.partial,
        "the text limit must be visible as a partial result"
    );
    assert!(
        stored.result.final_text.contains("ACP_RESULT_TEXT_LIMIT"),
        "missing limit reason"
    );
    let activity = scheduler.passive_activity_snapshot(&agent_id).unwrap();
    assert!(
        activity.latest_text_tail.ends_with("done"),
        "post-limit deltas must stay live"
    );
    assert!(activity.latest_text_tail.len() <= crate::MAX_LATEST_TEXT_BYTES);
    assert!(activity.latest_text_truncated);
}

/// Backpressure holds the prompt writer in send while the peer emits its first
/// chunk. The peer drains the request only after the tracker has seen that
/// chunk, forcing the write-before-begin interleaving without timing guesses.
#[test]
fn repair1_fast_peer_first_chunk_precedes_prompt_write_completion() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    let release = workspace.path().join("release-prompt-reader");
    let script = format!(
        r#"
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"capabilities":{{"models":true,"cancel":true,"permission":true}}}}}}'
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"{SESSION_ID}"}}}}'
# Read a single byte, leaving a legal prompt larger than the pipe capacity.
dd bs=1 count=1 >/dev/null 2>/dev/null
printf '%s\n' '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"{SESSION_ID}","update":{{"sessionUpdate":"agent_message_chunk","messageId":"first","content":{{"type":"text","text":"immediate "}}}}}}}}'
while [ ! -f {release:?} ]; do sleep 0.01; done
read_frame
printf '%s\n' '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"{SESSION_ID}","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"answer"}}}}}}}}' '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}'
"#
    );
    let child = scripted_child(workspace.path(), &script);
    let scheduler = Arc::new(dsh_scheduler(
        workspace.path(),
        DshRuntimeFactory::test_harness(Some(child)),
    ));
    let agent_id = scheduler
        .enqueue_general_with_admission(
            &manifest_for(workspace.path(), &"p".repeat(240 * 1024)),
            Some(dsh_admission(None)),
        )
        .unwrap()
        .agent_id;
    let starter = {
        let scheduler = Arc::clone(&scheduler);
        thread::spawn(move || scheduler.start_ready())
    };
    let deadline = Instant::now() + SCRIPTED_SYNC_WAIT;
    let saw_first_chunk = loop {
        let connection = rusqlite::Connection::open(workspace.path().join("state.sqlite")).unwrap();
        // Typed text deltas settle as `message` facts; the store keeps one
        // lifecycle summary row per fact, never the canonical JSON envelope.
        let projected: i64 = connection.query_row(
            "SELECT COUNT(*) FROM events WHERE agent_id=?1 AND event_type='runtime.fact' \
             AND json_extract(payload_json, '$.category')='message'",
            [&agent_id], |row| row.get(0)).unwrap();
        if projected == 1 {
            break true;
        }
        if Instant::now() >= deadline || starter.is_finished() {
            break false;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let writer_was_blocked = !starter.is_finished();
    // Always release the peer before asserting, including a failed barrier.
    std::fs::write(release, "release").unwrap();
    let started = starter.join().unwrap().unwrap();
    assert!(saw_first_chunk && writer_was_blocked,
        "first chunk must be projected while the prompt writer is blocked");
    assert_eq!(started, vec![agent_id.clone()]);
    let stored = await_result(&scheduler, &agent_id);
    assert_eq!(stored.result.outcome, TaskOutcome::Completed);
    assert_eq!(stored.result.final_text, "immediate answer");
    assert!(!stored.result.partial);
}

#[test]
fn repair1_failed_prompt_write_closes_the_reserved_turn() {
    let _guard = scripted_test_guard();
    let mut command = Command::new("sh");
    command.args(["-c", "cat >/dev/null"]);
    let owner = DshRuntimeOwner::spawn(command, Arc::new(NoopSink)).unwrap();
    // This is only the test-owned child. No installed daemon is touched.
    owner.session.lock().unwrap().driver().stop_and_reap(Duration::from_secs(1)).unwrap();
    assert!(owner.send_prompt("cannot be written").is_err());
    let turn = owner.shared.runtime_state.turn_snapshot();
    assert!(!turn.active, "failed write left a phantom prompt active");
    assert_eq!(turn.boundary, Some(crate::TurnBoundary::Failed));
    assert!(owner.shared.current_prompt.lock().unwrap().is_none());
    let messages = owner.shared.messages.lock().unwrap();
    assert!(messages.is_empty());
    assert_eq!(messages.turn_text("dsh-prompt-1"), None);
}

#[test]
fn repair4_nul_chunk_is_preserved_in_persisted_and_paged_result() {
    let _guard = scripted_test_guard();
    let workspace = dsh_workspace();
    let updates = workspace.path().join("nul-updates.jsonl");
    let chunk = |text: &str| {
        serde_json::json!({"jsonrpc":"2.0","method":"session/update",
        "params":{"sessionId":SESSION_ID,"update":{"sessionUpdate":"agent_message_chunk",
            "messageId":"native-nul","content":{"type":"text","text":text}}}})
        .to_string()
    };
    std::fs::write(
        &updates,
        format!(
            "{}\n{}\n{}\n",
            chunk("prefix "),
            chunk("important\0suffix"),
            r#"{"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}"#
        ),
    )
    .unwrap();
    let script = format!(
        "{}\ncat {:?}\n",
        BOOTSTRAP_PREFIX.replace("SESSION", SESSION_ID),
        updates
    );
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
    let expected = "prefix important\0suffix";
    assert_eq!(
        stored.result.final_text, expected,
        "complete success must retain the entire received text"
    );
    assert_eq!(stored.result.outcome, TaskOutcome::Completed);
    assert!(!stored.result.partial);

    let service = crate::rpc::RpcService::new(scheduler.clone(), scheduler.store()).unwrap();
    let mut offset = 0;
    let mut reconstructed = String::new();
    let mut saw_nul_escape = false;
    loop {
        let response = service
            .dispatch(crate::rpc::RpcMethod::TaskResult {
                agent_id: agent_id.clone(),
                offset,
                limit: 8,
            })
            .unwrap();
        let crate::rpc::RpcSuccess::TaskResult {
            result: Some(page), ..
        } = response
        else {
            panic!("result page");
        };
        assert_eq!(page.outcome, TaskOutcome::Completed);
        assert!(!page.partial);
        assert_eq!(page.total_bytes, expected.len());
        assert_eq!(page.offset, offset);
        let encoded = serde_json::to_string(&page).unwrap();
        saw_nul_escape |= encoded.contains("\\u0000");
        let decoded: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        reconstructed.push_str(decoded["final_text"].as_str().unwrap());
        if let Some(next) = page.next_offset {
            assert!(next > offset);
            offset = next;
        } else {
            break;
        }
    }
    assert!(
        saw_nul_escape,
        "RPC must safely encode NUL as a JSON escape"
    );
    assert_eq!(reconstructed, expected);
}

// ---------------------------------------------------------------------------
// Typed adapter facts (S03 `external_contract::events`). These assert the
// adapter's own output: one native `session/update` is explained once, with no
// canonical `session/event` JSON and no invented native identity.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CapturedFacts {
    events: Mutex<Vec<crate::RuntimeEnvelope>>,
}

struct CaptureSink(Arc<CapturedFacts>);
impl LifecycleSink for CaptureSink {
    fn emit(&self, record: crate::RuntimeEnvelope) {
        self.0.events.lock().unwrap().push(record);
    }
}

fn typed_adapter_shared(
    session: Option<&str>,
) -> (super::events::DshRuntimeShared, Arc<CapturedFacts>) {
    use external_agent_dsh::acp::{permission::OfferCache, result::MessageAggregation};
    let captured = Arc::new(CapturedFacts::default());
    let publisher = Arc::new(crate::Publisher::new(Arc::new(CaptureSink(Arc::clone(
        &captured,
    )))));
    let (task, launch) = publisher.event_scope();
    let shared = super::events::DshRuntimeShared {
        publisher: Arc::clone(&publisher),
        runtime_state: Arc::clone(&publisher.runtime_state),
        task,
        launch,
        offers: Mutex::new(OfferCache::default()),
        tool_names: Mutex::new(Default::default()),
        messages: Mutex::new(MessageAggregation::default()),
        session_id: Mutex::new(session.map(str::to_owned)),
        current_prompt: Mutex::new(None),
        last_prompt: Mutex::new(None),
        settled_turn: Mutex::new(None),
        stop_boundaries: std::sync::atomic::AtomicU64::new(0),
    };
    (shared, captured)
}

fn captured_facts(captured: &CapturedFacts) -> Vec<external_contract::events::EventFact> {
    use external_contract::events::NormalizedEvent;
    captured
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            crate::RuntimeEnvelope::Event { event: NormalizedEvent { fact, .. }, .. } => Some(fact.clone()),
            _ => None,
        })
        .collect()
}

fn session_update(update: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"sessionId": "s", "update": update})
}

#[test]
fn text_thought_and_committed_message_decode_to_typed_facts() {
    use external_contract::events::*;
    let (shared, _captured) = typed_adapter_shared(Some("s"));
    shared.begin_turn(7);

    let chunk = shared
        .decode_session_update(&session_update(serde_json::json!({
            "sessionUpdate": "agent_message_chunk", "messageId": "m1",
            "content": {"type": "text", "text": "hello"}
        })))
        .unwrap();
    assert_eq!(chunk.disposition().kind(), DispositionKind::Consumed);
    assert_eq!(chunk.attribution(), Attribution::Current);
    assert_eq!(chunk.input().source.protocol, SourceProtocol::DshAcpV1);
    assert_eq!(
        chunk
            .input()
            .source
            .discriminator
            .as_ref()
            .map(SafeToken::as_str),
        Some("agent_message_chunk")
    );
    // DSH ACP has no native frame identity: none is invented.
    assert_eq!(chunk.input().source.event_id, None);
    assert_eq!(chunk.input().source.turn_id, None);
    assert_eq!(chunk.input().turn_id, Some(TurnId::Local(7)));
    assert_eq!(chunk.events().len(), 1);
    assert!(matches!(
        &chunk.events()[0].fact,
        EventFact::MessageUpdated(MessageUpdate { message_id, text })
            if *message_id == MessageId("m1".into()) && *text == TextUpdate::Delta("hello".into())
    ));

    let committed = shared
        .decode_session_update(&session_update(serde_json::json!({
            "type": "agent_message", "messageId": "m2",
            "content": [{"type": "text", "text": "done"}]
        })))
        .unwrap();
    assert_eq!(committed.disposition().kind(), DispositionKind::Consumed);
    let facts: Vec<_> = committed.events().iter().map(|e| e.fact.clone()).collect();
    assert_eq!(facts.len(), 2);
    assert!(matches!(
        &facts[0],
        EventFact::MessageUpdated(MessageUpdate { text: TextUpdate::Delta(text), .. })
            if text == "done"
    ));
    assert!(matches!(
        &facts[1],
        EventFact::MessageFinished { message_id } if *message_id == MessageId("m2".into())
    ));

    let thought = shared
        .decode_session_update(&session_update(serde_json::json!({
            "sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}
        })))
        .unwrap();
    assert!(matches!(
        &thought.events()[0].fact,
        EventFact::PublicReasoningDelta(reasoning)
            if reasoning.channel == PublicReasoningChannel::Thought && reasoning.delta == "hmm"
    ));

    // A legal empty chunk has no consumer, and no malformed coverage is
    // claimed for it.
    let empty = shared
        .decode_session_update(&session_update(serde_json::json!({
            "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": ""}
        })))
        .unwrap();
    assert_eq!(
        empty.disposition().kind(),
        DispositionKind::RecognizedIgnored
    );
    assert!(empty.events().is_empty());

    // A NUL-bearing public thought is not deliverable reasoning: the known
    // shape is malformed and the affected dimension is declared.
    let nul_thought = shared
        .decode_session_update(&session_update(serde_json::json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": "bad\0text"}
        })))
        .unwrap();
    assert_eq!(
        nul_thought.disposition().kind(),
        DispositionKind::InvalidLost
    );
    assert!(matches!(
        &nul_thought.events()[0].fact,
        EventFact::CoverageGap(gap)
            if gap.reason == LossReason::Malformed
                && gap.impact == Impact::Known(vec![CoverageDimension::PublicReasoning])
    ));
}

#[test]
fn repair_malformed_message_text_is_lost_but_legal_empty_content_is_ignored() {
    use external_contract::events::*;
    let (shared, captured) = typed_adapter_shared(Some("s"));
    shared.begin_turn(1);
    for malformed in [
        serde_json::json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":42}}),
        serde_json::json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text"}}),
        serde_json::json!({"sessionUpdate":"agent_message_chunk"}),
        serde_json::json!({"sessionUpdate":"agent_message","content":[{"type":"text","text":"valid"},{"type":"text","text":false}]}),
    ] {
        let batch = shared
            .decode_session_update(&session_update(malformed))
            .unwrap();
        assert_eq!(batch.disposition().kind(), DispositionKind::InvalidLost);
        assert!(batch.events().iter().any(|event| matches!(&event.fact,
            EventFact::CoverageGap(gap) if gap.reason == LossReason::Malformed
                && gap.impact == Impact::Known(vec![CoverageDimension::Text]))));
        shared.publisher.emit_typed(batch);
    }
    for content in [
        serde_json::json!({"type":"text","text":""}),
        serde_json::json!({"type":"image","data":"synthetic","mimeType":"image/png"}),
    ] {
        let batch = shared
            .decode_session_update(&session_update(serde_json::json!({
                "sessionUpdate":"agent_message_chunk", "content":content
            })))
            .unwrap();
        assert_eq!(
            batch.disposition(),
            &DecodeDisposition::RecognizedIgnored(IgnoreReason::NoConsumer)
        );
    }
    for event in captured.events.lock().unwrap().iter() {
        if matches!(
            event,
            crate::RuntimeEnvelope::Event { event: NormalizedEvent {
                fact: EventFact::CoverageGap(_), ..
            }, .. }
        ) {
            let projection = crate::projection::lifecycle_projection(event, None);
            let summary: LifecycleSummary = serde_json::from_str(&projection.payload_json).unwrap();
            assert_eq!(summary.disposition, DispositionKind::InvalidLost);
            assert_eq!(summary.reason, Some(DiagnosticReason::Malformed));
            assert_eq!(
                summary.impact,
                Some(Impact::Known(vec![CoverageDimension::Text]))
            );
            assert!(!projection.payload_json.contains("content"));
        }
    }
    let facts = captured_facts(&captured);
    assert_eq!(
        facts
            .iter()
            .filter(|fact| matches!(fact, EventFact::CoverageGap(g)
        if g.reason == LossReason::Malformed
            && g.impact == Impact::Known(vec![CoverageDimension::Text])))
            .count(),
        4
    );
}

#[test]
fn tool_updates_name_from_title_and_derive_phases_from_status() {
    use external_contract::events::*;
    let (shared, _captured) = typed_adapter_shared(Some("s"));
    shared.begin_turn(1);

    let announced = shared
        .decode_session_update(&session_update(serde_json::json!({
            "sessionUpdate": "tool_call", "toolCallId": "c1",
            "title": "probe the workspace", "kind": "other",
            "status": "in_progress", "rawInput": {"path": "."}
        })))
        .unwrap();
    assert_eq!(announced.disposition().kind(), DispositionKind::Consumed);
    assert_eq!(announced.events().len(), 1);
    assert!(matches!(
        &announced.events()[0].fact,
        EventFact::ToolChanged(tool)
            if tool.call_id == CallId("c1".into())
                && tool.name.as_deref() == Some("probe the workspace")
                && tool.category.as_ref().map(SafeToken::as_str) == Some("other")
                && tool.phase == Some(ToolPhase::Started)
                && tool.outcome.is_none()
                && tool.arguments == Some(ArgumentUpdate::Snapshot(serde_json::json!({"path": "."})))
    ));
    assert_eq!(announced.input().source.call_id.as_deref(), Some("c1"));

    // The category is never the name; a failed native status is a failed call.
    let failed = shared
        .decode_session_update(&session_update(serde_json::json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "failed"
        })))
        .unwrap();
    assert_eq!(failed.disposition().kind(), DispositionKind::Consumed);
    assert!(matches!(
        &failed.events()[0].fact,
        EventFact::ToolChanged(tool)
            if tool.name.as_deref() == Some("probe the workspace")
                && tool.phase == Some(ToolPhase::Finished)
                && tool.outcome == Some(ToolOutcome::Failed)
    ));
    // The bounded correlation slot is released once the call finished.
    assert!(shared.tool_names.lock().unwrap().is_empty());

    // A terminal announcement is one fact, never an invented start, and it
    // carries the explicit native success evidence.
    let terminal = shared
        .decode_session_update(&session_update(serde_json::json!({
            "sessionUpdate": "tool_call", "toolCallId": "c2",
            "title": "Read", "kind": "other", "status": "completed", "rawInput": {"path": "a.ts"}
        })))
        .unwrap();
    assert_eq!(terminal.events().len(), 1);
    assert!(matches!(
        &terminal.events()[0].fact,
        EventFact::ToolChanged(tool)
            if tool.phase == Some(ToolPhase::Finished) && tool.outcome == Some(ToolOutcome::Succeeded)
    ));

    // No recognized status proves nothing about execution.
    let unknown_status = shared
        .decode_session_update(&session_update(serde_json::json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "c3", "status": "queued"
        })))
        .unwrap();
    assert_eq!(
        unknown_status.disposition().kind(),
        DispositionKind::RecognizedIgnored
    );
    assert!(unknown_status.events().is_empty());
}

#[test]
fn update_dispositions_separate_ignored_unknown_and_lost_inputs() {
    use external_contract::events::*;
    let (shared, _captured) = typed_adapter_shared(Some("s"));
    shared.begin_turn(1);

    // Recognized normal traffic with no consumer here is not loss.
    for (update, expected) in [
        (
            serde_json::json!({"sessionUpdate": "usage_update"}),
            IgnoreReason::Usage,
        ),
        (
            serde_json::json!({"sessionUpdate": "plan"}),
            IgnoreReason::PlanOrDiff,
        ),
        (
            serde_json::json!({"sessionUpdate": "config_option_update"}),
            IgnoreReason::Configuration,
        ),
        (
            serde_json::json!({"sessionUpdate": "available_commands_update"}),
            IgnoreReason::Configuration,
        ),
    ] {
        let batch = shared
            .decode_session_update(&session_update(update))
            .unwrap();
        assert_eq!(
            batch.disposition(),
            &DecodeDisposition::RecognizedIgnored(expected)
        );
        assert!(batch.events().is_empty());
    }

    // A discriminator this adapter does not know is uncertainty, retained as a
    // safe diagnostic instead of disappearing.
    let unknown = shared
        .decode_session_update(&session_update(serde_json::json!({
            "sessionUpdate": "permission_stream_v2", "payload": {"secret": "x"}
        })))
        .unwrap();
    assert_eq!(unknown.disposition().kind(), DispositionKind::Unknown);
    assert!(matches!(
        &unknown.events()[0].fact,
        EventFact::Diagnostic(diagnostic)
            if diagnostic.reason == DiagnosticReason::UnknownSemantics
                && diagnostic.discriminator.as_ref().map(SafeToken::as_str)
                    == Some("permission_stream_v2")
    ));

    // A known tool shape without its identity is damaged input with a known
    // affected dimension.
    let malformed = shared
        .decode_session_update(&session_update(serde_json::json!({
            "sessionUpdate": "tool_call", "title": "no identity"
        })))
        .unwrap();
    assert_eq!(malformed.disposition().kind(), DispositionKind::InvalidLost);
    assert!(matches!(
        &malformed.events()[0].fact,
        EventFact::CoverageGap(gap)
            if gap.reason == LossReason::Malformed
                && gap.impact == Impact::Known(vec![CoverageDimension::Tools])
    ));
}

#[test]
fn foreign_and_post_settlement_frames_never_touch_the_current_turn() {
    use external_contract::events::*;
    let (shared, _captured) = typed_adapter_shared(Some("s"));
    shared.begin_turn(1);

    let foreign = shared
        .decode_session_update(&serde_json::json!({
            "sessionId": "other-session",
            "update": {"sessionUpdate": "agent_message_chunk",
                       "content": {"type": "text", "text": "foreign"}}
        }))
        .unwrap();
    assert_eq!(foreign.attribution(), Attribution::ForeignSession);
    assert_eq!(
        foreign.disposition(),
        &DecodeDisposition::RecognizedIgnored(IgnoreReason::ForeignSession)
    );
    assert!(foreign.events().is_empty());
    assert!(shared.messages.lock().unwrap().is_empty());

    let chunk = shared
        .decode_session_update(&session_update(serde_json::json!({
            "sessionUpdate": "agent_message_chunk", "messageId": "m1",
            "content": {"type": "text", "text": "answer"}
        })))
        .unwrap();
    assert_eq!(chunk.disposition().kind(), DispositionKind::Consumed);
    shared.apply_settled_turn(super::events::SettledTurn::Settled(
        external_agent_dsh::acp::transport::PromptSettlement {
            stop_reason: "end_turn".into(),
            message_id: None,
        },
    ));

    let late = shared
        .decode_session_update(&session_update(serde_json::json!({
            "sessionUpdate": "agent_message_chunk", "messageId": "m1",
            "content": {"type": "text", "text": "more"}
        })))
        .unwrap();
    assert_eq!(late.attribution(), Attribution::Late);
    assert_eq!(
        late.disposition(),
        &DecodeDisposition::RecognizedIgnored(IgnoreReason::Late)
    );
    assert!(late.events().is_empty());
}

#[test]
fn settlement_reduces_text_before_the_boundary_and_keeps_the_native_stop_reason() {
    use external_contract::events::*;
    let (shared, captured) = typed_adapter_shared(Some("s"));
    shared.begin_turn(1);
    for text in ["build ", "answer"] {
        let batch = shared
            .decode_session_update(&session_update(serde_json::json!({
                "sessionUpdate": "agent_message_chunk", "messageId": "m1",
                "content": {"type": "text", "text": text}
            })))
            .unwrap();
        assert_eq!(batch.disposition().kind(), DispositionKind::Consumed);
    }
    // The standard flow settles with nothing but a stop reason.
    shared.apply_settled_turn(super::events::SettledTurn::Settled(
        external_agent_dsh::acp::transport::PromptSettlement {
            stop_reason: "end_turn".into(),
            message_id: None,
        },
    ));

    let facts = captured_facts(&captured);
    assert!(
        !facts.iter().any(|fact| matches!(
            fact,
            EventFact::MessageUpdated(MessageUpdate {
                text: TextUpdate::Snapshot(_),
                ..
            })
        )),
        "turn-wide text must only travel on TurnSettled"
    );
    let settled = facts
        .iter()
        .position(|fact| matches!(fact, EventFact::TurnSettled(_)))
        .expect("the turn boundary was projected");
    let EventFact::TurnSettled(settlement) = &facts[settled] else {
        unreachable!()
    };
    assert_eq!(settlement.turn_id, TurnId::Local(1));
    assert_eq!(settlement.outcome, TurnOutcome::Completed);
    assert_eq!(settlement.final_text.as_deref(), Some("build answer"));
    assert_eq!(
        settlement.stop_reason.as_ref().map(SafeToken::as_str),
        Some("end_turn")
    );
    assert!(settlement.failure.is_none());

    let turn = shared.runtime_state.turn_snapshot();
    assert!(!turn.active);
    assert_eq!(turn.boundary, Some(crate::TurnBoundary::Completed));
}

#[test]
fn failed_settlements_keep_the_complete_redacted_native_error() {
    use external_contract::events::*;
    let (shared, captured) = typed_adapter_shared(Some("s"));
    shared.begin_turn(3);
    let native = serde_json::json!({
        "code": -32000,
        "message": "quota exhausted for the fixture account",
        "data": {"retryAfter": 30, "limit": "daily", "apiKey": "secret"}
    });
    shared.apply_failed_settlement(super::events::REMOTE_ERROR_REASON, Some(&native));

    let facts = captured_facts(&captured);
    let EventFact::TurnSettled(settlement) = facts
        .iter()
        .find(|fact| matches!(fact, EventFact::TurnSettled(_)))
        .expect("failed boundary")
    else {
        unreachable!()
    };
    assert_eq!(settlement.outcome, TurnOutcome::Failed);
    let failure = settlement.failure.as_ref().expect("native failure");
    assert_eq!(failure.original["code"], -32000);
    assert_eq!(
        failure.original["message"],
        "quota exhausted for the fixture account"
    );
    assert_eq!(failure.original["data"]["retryAfter"], 30);
    assert_eq!(failure.original["data"]["limit"], "daily");
    assert_eq!(failure.original["data"]["apiKey"], "[REDACTED]");
    assert_eq!(
        settlement.stop_reason.as_ref().map(SafeToken::as_str),
        Some(super::events::REMOTE_ERROR_REASON)
    );
    let detail = shared
        .runtime_state
        .native_failure()
        .expect("the complete native failure is retained");
    assert!(detail.original.to_string().contains("quota exhausted for the fixture account"));
    assert_eq!(detail.original, failure.original);
}

#[test]
fn a_failed_stop_reason_is_a_failed_turn_with_the_reason_retained() {
    use external_contract::events::*;
    let (shared, captured) = typed_adapter_shared(Some("s"));
    shared.begin_turn(1);
    let batch = shared
        .decode_session_update(&session_update(serde_json::json!({
            "sessionUpdate": "tool_call", "toolCallId": "c1",
            "title": "Read", "kind": "other", "status": "failed",
            "rawInput": {"path": "a.ts"}
        })))
        .unwrap();
    assert_eq!(batch.disposition().kind(), DispositionKind::Consumed);

    shared.apply_settled_turn(super::events::SettledTurn::Settled(
        external_agent_dsh::acp::transport::PromptSettlement {
            stop_reason: "max_tokens".into(),
            message_id: None,
        },
    ));
    let facts = captured_facts(&captured);
    let EventFact::TurnSettled(settlement) = facts
        .iter()
        .find(|fact| matches!(fact, EventFact::TurnSettled(_)))
        .expect("failed boundary")
    else {
        unreachable!()
    };
    assert_eq!(settlement.outcome, TurnOutcome::Failed);
    assert_eq!(
        settlement.stop_reason.as_ref().map(SafeToken::as_str),
        Some("max_tokens")
    );
    // The bounded ES reason keeps the delivered diagnostic stable.
    assert_eq!(
        settlement
            .failure
            .as_ref()
            .map(|failure| failure.original.clone()),
        Some(serde_json::json!("MAX_TOKENS"))
    );
    assert!(!shared.runtime_state.turn_snapshot().active);
    assert_eq!(
        shared.runtime_state.turn_snapshot().boundary,
        Some(crate::TurnBoundary::Failed)
    );
}

#[test]
fn session_update_projection_emits_typed_facts_only() {
    use external_contract::events::*;
    let (shared, captured) = typed_adapter_shared(Some("s"));
    shared.begin_turn(1);
    let event = Inbound::Message(WireMessage::UnknownEvent {
        method: "session/update".into(),
        raw: serde_json::json!({"params": {"sessionId": "s", "update": {
            "sessionUpdate": "agent_message_chunk", "messageId": "m1",
            "content": {"type": "text", "text": "typed"}
        }}}),
    });
    assert!(super::events::project_dsh_inbound(&shared, &event).is_none());
    let events = captured.events.lock().unwrap();
    assert!(events
        .iter()
        .all(|event| matches!(event, crate::RuntimeEnvelope::Event { .. })));
    assert!(events.iter().any(|event| matches!(
        event,
        crate::RuntimeEnvelope::Event { event: normalized, .. }
            if matches!(
                &normalized.fact,
                EventFact::MessageUpdated(MessageUpdate {
                    text: TextUpdate::Delta(text), ..
                }) if text == "typed"
            )
    )));
}
