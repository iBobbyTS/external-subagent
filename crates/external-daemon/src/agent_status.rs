use external_contract::{
    event_type, CreateSessionParams, RuntimePreferences, SendParams, SessionCreateProjection,
    SessionParams, SubscribeParams, WireMessage, WorkspaceRef, INTERACTION_REQUEST_PERMISSION,
    SESSION_CLOSE, SESSION_CREATE, SESSION_EVENT, SESSION_REQUEST_RUNTIME_PREFERENCES,
    SESSION_SEND, SESSION_SUBSCRIBE,
};
use external_runtime::{Driver, Inbound, RequestError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const LOCAL_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const RUNTIME_PROBE_TIMEOUT: Duration = Duration::from_secs(90);
const RUNTIME_STOP_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeLayer {
    Local,
    Auth,
    Hi,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProbeScope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProbeInput {
    pub agent: String,
    #[serde(default)]
    pub through: ProbeLayer,
    #[serde(default)]
    pub scope: ProbeScope,
}

impl Default for ProbeLayer {
    fn default() -> Self {
        Self::Local
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EvidenceState {
    Ready,
    Degraded,
    Unavailable,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeEvidence {
    pub state: EvidenceState,
    pub scope: ProbeScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub checked_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ScopeEvidence {
    fn unknown(scope: ProbeScope, checked_at_ms: u64, reason: &str) -> Self {
        Self {
            state: EvidenceState::Unknown,
            scope,
            version: None,
            checked_at_ms,
            reason: Some(reason.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProbeEvidence {
    pub agent: String,
    pub local: ScopeEvidence,
    pub auth: ScopeEvidence,
    pub hi: ScopeEvidence,
}

pub trait AgentProbeBackend: Send + Sync + 'static {
    fn probe(&self, input: &AgentProbeInput) -> AgentProbeEvidence;
}

#[derive(Clone)]
pub struct AgentEvidenceStore {
    backend: Arc<dyn AgentProbeBackend>,
    latest: Arc<Mutex<BTreeMap<String, AgentProbeEvidence>>>,
}

impl AgentEvidenceStore {
    pub fn new(runtime_source: Option<PathBuf>) -> Self {
        Self::with_backend(Arc::new(ProcessProbeBackend { runtime_source }))
    }

    pub(crate) fn with_backend(backend: Arc<dyn AgentProbeBackend>) -> Self {
        Self {
            backend,
            latest: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn probe(&self, input: &AgentProbeInput) -> AgentProbeEvidence {
        let evidence = self.backend.probe(input);
        self.latest
            .lock()
            .unwrap()
            .insert(input.agent.clone(), evidence.clone());
        evidence
    }

    pub fn latest(&self, agent: &str) -> Option<AgentProbeEvidence> {
        self.latest.lock().unwrap().get(agent).cloned()
    }
}

struct ProcessProbeBackend {
    runtime_source: Option<PathBuf>,
}

impl AgentProbeBackend for ProcessProbeBackend {
    fn probe(&self, input: &AgentProbeInput) -> AgentProbeEvidence {
        let checked_at_ms = wall_now_millis();
        let mut scope = input.scope.clone();
        let disposable_workspace = if input.agent == "zcode"
            && input.through == ProbeLayer::Hi
            && scope.workspace.is_none()
        {
            tempfile::Builder::new()
                .prefix("external-subagent-probe-")
                .tempdir()
                .ok()
        } else {
            None
        };
        if let Some(workspace) = disposable_workspace.as_ref() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(workspace.path(), fs::Permissions::from_mode(0o700));
            }
            scope.workspace = Some(workspace.path().to_string_lossy().into_owned());
        }
        let executable = match input.agent.as_str() {
            "zcode" => self.runtime_source.clone(),
            "dsh" => env::var_os("DSH_RUNTIME_PATH").map(PathBuf::from),
            _ => None,
        };
        let local = probe_local(executable.as_deref(), scope.clone(), checked_at_ms);
        let mut auth = ScopeEvidence::unknown(scope.clone(), checked_at_ms, "auth_not_probed");
        let mut hi = ScopeEvidence::unknown(scope.clone(), checked_at_ms, "hi_not_probed");

        if input.agent == "dsh" && input.through != ProbeLayer::Local {
            auth = ScopeEvidence::unknown(
                scope.clone(),
                checked_at_ms,
                "dsh_production_adapter_unavailable",
            );
            hi = ScopeEvidence::unknown(
                scope.clone(),
                checked_at_ms,
                "dsh_production_adapter_unavailable",
            );
        } else if input.agent == "zcode" && input.through != ProbeLayer::Local {
            if local.state != EvidenceState::Ready {
                auth = derived_failure(&local, scope.clone(), checked_at_ms);
                if input.through == ProbeLayer::Hi {
                    hi = derived_failure(&local, scope.clone(), checked_at_ms);
                }
            } else if input.through == ProbeLayer::Auth {
                auth = ScopeEvidence::unknown(scope.clone(), checked_at_ms, "auth_requires_hi");
            } else {
                let (auth_result, hi_result) = probe_zcode_hi(
                    executable
                        .as_deref()
                        .expect("ready evidence has executable"),
                    &scope,
                    local.version.clone(),
                    checked_at_ms,
                );
                auth = auth_result;
                hi = hi_result;
            }
        }

        AgentProbeEvidence {
            agent: input.agent.clone(),
            local,
            auth,
            hi,
        }
    }
}

fn probe_local(path: Option<&Path>, scope: ProbeScope, checked_at_ms: u64) -> ScopeEvidence {
    let Some(path) = path else {
        return ScopeEvidence {
            state: EvidenceState::Unavailable,
            scope,
            version: None,
            checked_at_ms,
            reason: Some("missing".into()),
        };
    };
    if !path.is_file() {
        return ScopeEvidence {
            state: EvidenceState::Unavailable,
            scope,
            version: None,
            checked_at_ms,
            reason: Some("missing".into()),
        };
    }
    match executable_version(path) {
        Ok(version) => ScopeEvidence {
            state: EvidenceState::Ready,
            scope,
            version: Some(version),
            checked_at_ms,
            reason: None,
        },
        Err(reason) => ScopeEvidence {
            state: EvidenceState::Degraded,
            scope,
            version: None,
            checked_at_ms,
            reason: Some(reason),
        },
    }
}

fn executable_version(path: &Path) -> Result<String, String> {
    let mut command = runtime_command(path, true);
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let mut child = command.spawn().map_err(|_| "transport".to_owned())?;
    let deadline = Instant::now() + LOCAL_PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|_| "transport".to_owned())?;
                if !status.success() {
                    return Err("version".into());
                }
                let text = String::from_utf8_lossy(&output.stdout);
                let diagnostic = String::from_utf8_lossy(&output.stderr);
                let version = text
                    .lines()
                    .chain(diagnostic.lines())
                    .map(str::trim)
                    .find(|line| !line.is_empty());
                return version
                    .filter(|value| value.len() <= 128 && !value.contains('\0'))
                    .map(str::to_owned)
                    .or_else(|| package_version(path))
                    .ok_or_else(|| "version".into());
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("transport".into());
            }
            Err(_) => return Err("transport".into()),
        }
    }
}

fn package_version(path: &Path) -> Option<String> {
    for directory in path.ancestors().take(4) {
        let package = directory.join("package.json");
        let Ok(bytes) = fs::read(package) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if let Some(version) = value.get("version").and_then(Value::as_str) {
            if !version.is_empty() && version.len() <= 128 && !version.contains('\0') {
                return Some(version.to_owned());
            }
        }
    }
    None
}

fn runtime_command(path: &Path, version_only: bool) -> Command {
    let script = matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("js" | "cjs" | "mjs")
    );
    let mut command = if script {
        let mut command = Command::new("node");
        command.arg(path);
        command
    } else {
        Command::new(path)
    };
    if version_only {
        command.arg("--version");
    } else if script {
        command.arg("app-server");
    }
    command
}

fn probe_zcode_hi(
    executable: &Path,
    scope: &ProbeScope,
    version: Option<String>,
    checked_at_ms: u64,
) -> (ScopeEvidence, ScopeEvidence) {
    let Some(workspace) = scope.workspace.as_deref() else {
        let evidence = ScopeEvidence::unknown(scope.clone(), checked_at_ms, "workspace_required");
        return (evidence.clone(), evidence);
    };
    if !Path::new(workspace).is_absolute() || !Path::new(workspace).is_dir() {
        let evidence = ScopeEvidence {
            state: EvidenceState::Unavailable,
            scope: scope.clone(),
            version,
            checked_at_ms,
            reason: Some("workspace_missing".into()),
        };
        return (evidence.clone(), evidence);
    }
    let mut command = runtime_command(executable, false);
    if let Some(home) = scope.home.as_deref() {
        command.env("ZCODE_HOME", home);
    }
    command
        .env("ZCODE_AGENT_POLICY", "1")
        .env("ZCODE_AGENT_PERMISSION_MODE", "plan")
        .env("ZCODE_AGENT_WORKSPACE_ROOT", workspace)
        .env("ZCODE_AGENT_BOOTSTRAP_ROOTS", "/Applications/ZCode.app")
        .env("ZCODE_AGENT_WRITE_MANIFEST", "[]");
    let driver = match Driver::spawn(command) {
        Ok(driver) => Arc::new(driver),
        Err(_) => {
            let evidence = unavailable(scope, version, checked_at_ms, "transport");
            return (evidence.clone(), evidence);
        }
    };
    let result = run_read_only_hi(Arc::clone(&driver), workspace);
    let (auth, hi) = match result {
        Ok(session_id) => {
            let _ = driver.request(
                SESSION_CLOSE,
                serde_json::to_value(SessionParams {
                    session_id: &session_id,
                })
                .unwrap_or(Value::Null),
                RUNTIME_STOP_GRACE,
            );
            let evidence = ScopeEvidence {
                state: EvidenceState::Ready,
                scope: scope.clone(),
                version,
                checked_at_ms,
                reason: None,
            };
            (evidence.clone(), evidence)
        }
        Err(reason) => {
            let evidence = unavailable(scope, version, checked_at_ms, &reason);
            (evidence.clone(), evidence)
        }
    };
    let _ = driver.stop_and_reap(RUNTIME_STOP_GRACE);
    (auth, hi)
}

fn run_read_only_hi(driver: Arc<Driver>, workspace: &str) -> Result<String, String> {
    let deadline = Instant::now() + RUNTIME_PROBE_TIMEOUT;
    let workspace_ref = WorkspaceRef {
        workspace_key: workspace,
        workspace_path: workspace,
    };
    let created = request_with_runtime_preferences(
        Arc::clone(&driver),
        SESSION_CREATE,
        serde_json::to_value(CreateSessionParams {
            workspace: workspace_ref,
            mode: Some("plan"),
            mcp_servers: &[],
        })
        .map_err(|_| "transport".to_owned())?,
        deadline,
    )?;
    let projection = created
        .result
        .as_ref()
        .ok_or_else(|| "transport".to_owned())
        .and_then(|result| {
            SessionCreateProjection::from_result(result).map_err(|_| "transport".to_owned())
        })?;
    let session_id = projection.session_id;
    request_with_runtime_preferences(
        Arc::clone(&driver),
        SESSION_SUBSCRIBE,
        serde_json::to_value(SubscribeParams {
            session_id: &session_id,
            delivery_kind: "desktop-continuous",
            include_snapshot: true,
        })
        .map_err(|_| "transport".to_owned())?,
        deadline,
    )?;
    request_with_runtime_preferences(
        Arc::clone(&driver),
        SESSION_SEND,
        serde_json::to_value(SendParams {
            session_id: &session_id,
            content: "Do not call tools. Reply with exactly hi.",
        })
        .map_err(|_| "transport".to_owned())?,
        deadline,
    )?;

    loop {
        let wait = remaining(deadline)?.min(Duration::from_millis(50));
        match driver.recv_timeout(wait) {
            Ok(Inbound::Message(WireMessage::Event(event)))
                if event.method == SESSION_EVENT
                    && event_type(&event) == Some("turn.completed") =>
            {
                return Ok(session_id)
            }
            Ok(Inbound::Message(WireMessage::Event(event)))
                if event.method == SESSION_EVENT && event_type(&event) == Some("turn.failed") =>
            {
                return Err(
                    classify_provider_failure(&event.params, &driver.diagnostic_tail()).into(),
                )
            }
            Ok(Inbound::Message(WireMessage::Event(event)))
                if event
                    .params
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| {
                        kind.starts_with("tool.") || kind == "permission.requested"
                    }) =>
            {
                return Err("policy_violation".into())
            }
            Ok(Inbound::Message(WireMessage::Event(_))) => {}
            Ok(Inbound::Message(WireMessage::Request(request))) => {
                if request.method == INTERACTION_REQUEST_PERMISSION {
                    let _ = driver.respond_error(
                        request.id,
                        serde_json::json!({"code":-32003,"message":"probe policy forbids tools"}),
                    );
                    return Err("policy_violation".into());
                }
                let _ = driver.respond_error(
                    request.id,
                    serde_json::json!({"code":-32601,"message":"unsupported probe request"}),
                );
                return Err("transport".into());
            }
            Ok(Inbound::Message(WireMessage::UnknownEvent { raw, .. })) => {
                if contains_tool_signal(&raw) {
                    return Err("policy_violation".into());
                }
            }
            Ok(Inbound::Message(WireMessage::Response(_))) => {}
            Ok(Inbound::Malformed(_) | Inbound::OversizedLine { .. }) => {
                return Err("transport".into())
            }
            Ok(Inbound::ChildExited(_)) => {
                return Err(
                    classify_provider_failure(&Value::Null, &driver.diagnostic_tail()).into(),
                )
            }
            Ok(Inbound::UnmatchedResponse { .. }) => return Err("transport".into()),
            Ok(Inbound::Lifecycle { .. }) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("transport".into()),
        }
    }
}

fn request_with_runtime_preferences(
    driver: Arc<Driver>,
    method: &str,
    params: Value,
    deadline: Instant,
) -> Result<external_contract::ResponseEnvelope, String> {
    let pending = driver
        .begin_request(method, params)
        .map_err(|error| classify_request_error(&error, &driver.diagnostic_tail()))?;
    let (sender, receiver) = mpsc::channel();
    let budget = remaining(deadline)?;
    thread::spawn(move || {
        let _ = sender.send(pending.wait(budget));
    });
    loop {
        match receiver.try_recv() {
            Ok(result) => {
                return result
                    .map_err(|error| classify_request_error(&error, &driver.diagnostic_tail()))
            }
            Err(mpsc::TryRecvError::Disconnected) => return Err("transport".into()),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let wait = remaining(deadline)?.min(Duration::from_millis(20));
        match driver.recv_timeout(wait) {
            Ok(Inbound::Message(WireMessage::Request(request)))
                if request.method == SESSION_REQUEST_RUNTIME_PREFERENCES =>
            {
                driver
                    .respond(
                        request.id,
                        serde_json::to_value(RuntimePreferences::default())
                            .map_err(|_| "transport".to_owned())?,
                    )
                    .map_err(|_| "transport".to_owned())?;
            }
            Ok(Inbound::Message(WireMessage::Request(request))) => {
                if request.method == INTERACTION_REQUEST_PERMISSION {
                    let _ = driver.respond_error(
                        request.id,
                        serde_json::json!({"code":-32003,"message":"probe policy forbids tools"}),
                    );
                    return Err("policy_violation".into());
                }
                let _ = driver.respond_error(
                    request.id,
                    serde_json::json!({"code":-32601,"message":"unsupported probe request"}),
                );
                return Err("transport".into());
            }
            Ok(Inbound::Message(WireMessage::Event(event)))
                if event
                    .params
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| {
                        kind.starts_with("tool.") || kind == "permission.requested"
                    }) =>
            {
                return Err("policy_violation".into())
            }
            Ok(Inbound::Message(WireMessage::UnknownEvent { raw, .. }))
                if contains_tool_signal(&raw) =>
            {
                return Err("policy_violation".into())
            }
            Ok(Inbound::Malformed(_) | Inbound::OversizedLine { .. }) => {
                return Err("transport".into())
            }
            Ok(Inbound::ChildExited(_)) => {
                return Err(
                    classify_provider_failure(&Value::Null, &driver.diagnostic_tail()).into(),
                )
            }
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("transport".into()),
        }
    }
}

fn remaining(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| "network".into())
}

fn classify_request_error(error: &RequestError, diagnostic: &str) -> String {
    match error {
        RequestError::Timeout => "network".into(),
        RequestError::Remote(value) => classify_provider_failure(value, diagnostic).into(),
        RequestError::Cancelled
        | RequestError::ChildExited(_)
        | RequestError::StreamClosed
        | RequestError::WriteFailed(_) => {
            classify_provider_failure(&Value::Null, diagnostic).into()
        }
    }
}

fn classify_provider_failure(value: &Value, diagnostic: &str) -> &'static str {
    let mut text = value.to_string().to_ascii_lowercase();
    text.push_str(&diagnostic.to_ascii_lowercase());
    if text.contains("401") || text.contains("unauthorized") || text.contains("unauthenticated") {
        "auth_401"
    } else if text.contains("429") || text.contains("rate limit") || text.contains("rate_limit") {
        "rate_limit"
    } else if text.contains("network")
        || text.contains("dns")
        || text.contains("connect")
        || text.contains("timed out")
        || text.contains("timeout")
    {
        "network"
    } else {
        "transport"
    }
}

fn contains_tool_signal(value: &Value) -> bool {
    let text = value.to_string().to_ascii_lowercase();
    text.contains("tool.")
        || text.contains("tool_call")
        || text.contains("permission.requested")
        || text.contains("requestpermission")
}

fn unavailable(
    scope: &ProbeScope,
    version: Option<String>,
    checked_at_ms: u64,
    reason: &str,
) -> ScopeEvidence {
    ScopeEvidence {
        state: if reason == "rate_limit" {
            EvidenceState::Degraded
        } else {
            EvidenceState::Unavailable
        },
        scope: scope.clone(),
        version,
        checked_at_ms,
        reason: Some(reason.into()),
    }
}

fn derived_failure(source: &ScopeEvidence, scope: ProbeScope, checked_at_ms: u64) -> ScopeEvidence {
    ScopeEvidence {
        state: source.state,
        scope,
        version: source.version.clone(),
        checked_at_ms,
        reason: source.reason.clone(),
    }
}

fn wall_now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, os::unix::fs::PermissionsExt};

    #[derive(Clone)]
    struct FixtureBackend {
        evidence: AgentProbeEvidence,
    }

    impl AgentProbeBackend for FixtureBackend {
        fn probe(&self, _input: &AgentProbeInput) -> AgentProbeEvidence {
            self.evidence.clone()
        }
    }

    fn evidence(scope: ProbeScope) -> AgentProbeEvidence {
        AgentProbeEvidence {
            agent: "zcode".into(),
            local: ScopeEvidence {
                state: EvidenceState::Ready,
                scope: scope.clone(),
                version: Some("1.2.3".into()),
                checked_at_ms: 10,
                reason: None,
            },
            auth: ScopeEvidence::unknown(scope.clone(), 10, "auth_requires_hi"),
            hi: ScopeEvidence::unknown(scope, 10, "hi_not_probed"),
        }
    }

    #[test]
    fn status_reads_only_previously_recorded_explicit_probe_evidence() {
        let scope = ProbeScope {
            workspace: Some("/workspace-a".into()),
            home: Some("/home-a".into()),
        };
        let store = AgentEvidenceStore::with_backend(Arc::new(FixtureBackend {
            evidence: evidence(scope.clone()),
        }));
        assert_eq!(store.latest("zcode"), None);
        let observed = store.probe(&AgentProbeInput {
            agent: "zcode".into(),
            through: ProbeLayer::Auth,
            scope: scope.clone(),
        });
        assert_eq!(observed.local.scope, scope);
        assert_eq!(store.latest("zcode"), Some(observed));
        assert_eq!(store.latest("dsh"), None);
    }

    #[test]
    fn failure_classifier_keeps_auth_network_and_rate_limit_distinct() {
        assert_eq!(
            classify_provider_failure(
                &serde_json::json!({"code":401,"message":"unauthorized"}),
                ""
            ),
            "auth_401"
        );
        assert_eq!(
            classify_provider_failure(&serde_json::json!({"code":429,"message":"rate limit"}), ""),
            "rate_limit"
        );
        assert_eq!(
            classify_provider_failure(&Value::Null, "network timeout"),
            "network"
        );
        assert_eq!(
            classify_provider_failure(&Value::Null, "closed"),
            "transport"
        );
    }

    #[test]
    fn dsh_probe_never_promotes_production_spawn_support() {
        let backend = ProcessProbeBackend {
            runtime_source: None,
        };
        let result = backend.probe(&AgentProbeInput {
            agent: "dsh".into(),
            through: ProbeLayer::Hi,
            scope: ProbeScope::default(),
        });
        assert_eq!(result.local.reason.as_deref(), Some("missing"));
        assert_eq!(
            result.hi.reason.as_deref(),
            Some("dsh_production_adapter_unavailable")
        );
    }

    #[test]
    fn local_probe_executes_the_configured_version_command() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("runtime");
        fs::File::create(&executable)
            .unwrap()
            .write_all(b"#!/bin/sh\nprintf 'fixture-9.9.9\\n'\n")
            .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let result = probe_local(Some(&executable), ProbeScope::default(), 42);
        assert_eq!(result.state, EvidenceState::Ready);
        assert_eq!(result.version.as_deref(), Some("fixture-9.9.9"));
        assert_eq!(result.checked_at_ms, 42);
    }

    fn fake_hi_runtime(
        directory: &Path,
        terminal: &str,
        failure: Value,
        emit_tool: bool,
    ) -> (PathBuf, PathBuf) {
        let executable = directory.join(format!("fixture-{terminal}.mjs"));
        let log = directory.join(format!("fixture-{terminal}.jsonl"));
        let source = r#"
import fs from 'node:fs';
if (process.argv.includes('--version')) { process.stdout.write('3.8.1\n'); process.exit(0); }
const log = __LOG__;
const terminal = __TERMINAL__;
const failure = __FAILURE__;
const emitTool = __EMIT_TOOL__;
let pendingCreate = null;
let buffer = '';
function write(value) { process.stdout.write(`${JSON.stringify(value)}\n`); }
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  buffer += chunk;
  for (;;) {
    const newline = buffer.indexOf('\n');
    if (newline < 0) break;
    const line = buffer.slice(0, newline); buffer = buffer.slice(newline + 1);
    if (!line) continue;
    const value = JSON.parse(line);
    const rootMode = process.env.ZCODE_AGENT_WORKSPACE_ROOT ? (fs.statSync(process.env.ZCODE_AGENT_WORKSPACE_ROOT).mode & 0o777).toString(8) : null;
    fs.appendFileSync(log, JSON.stringify({ value, policy: process.env.ZCODE_AGENT_POLICY, mode: process.env.ZCODE_AGENT_PERMISSION_MODE, manifest: process.env.ZCODE_AGENT_WRITE_MANIFEST, root: process.env.ZCODE_AGENT_WORKSPACE_ROOT, rootMode }) + '\n');
    if (value.method === 'session/create') {
      if (value.params.mode !== 'plan' || process.env.ZCODE_AGENT_POLICY !== '1' || process.env.ZCODE_AGENT_PERMISSION_MODE !== 'plan' || process.env.ZCODE_AGENT_WRITE_MANIFEST !== '[]') {
        write({ id: value.id, error: { code: -32602, message: 'unsafe probe policy' } }); continue;
      }
      pendingCreate = value.id;
      write({ id: 'preferences', method: 'session/requestRuntimePreferences', params: { scope: 'session', sessionId: 'probe-session' } });
    } else if (value.id === 'preferences' && value.result) {
      write({ id: pendingCreate, result: { session: { sessionId: 'probe-session' }, settings: { model: { current: { modelId: 'fixture-model' } } } } });
    } else if (value.method === 'session/subscribe') {
      write({ id: value.id, result: { subscribed: true } });
    } else if (value.method === 'session/send') {
      write({ id: value.id, result: { turnId: 'probe-turn' } });
      write({ method: 'session/event', params: { eventId: 'start', sessionId: 'probe-session', seq: 1, timestamp: 1, type: 'turn.started', payload: {} } });
      if (emitTool) write({ method: 'session/event', params: { eventId: 'tool', sessionId: 'probe-session', seq: 2, timestamp: 2, type: 'tool.updated', payload: { toolName: 'Bash' } } });
      write({ method: 'session/event', params: { eventId: 'end', sessionId: 'probe-session', seq: 3, timestamp: 3, type: terminal, payload: failure } });
    } else if (value.method === 'session/close') {
      write({ id: value.id, result: { closed: true } });
    }
  }
});
"#
        .replace("__LOG__", &serde_json::to_string(&log).unwrap())
        .replace("__TERMINAL__", &serde_json::to_string(terminal).unwrap())
        .replace("__FAILURE__", &failure.to_string())
        .replace("__EMIT_TOOL__", if emit_tool { "true" } else { "false" });
        fs::write(&executable, source).unwrap();
        (executable, log)
    }

    fn run_fixture_probe(
        terminal: &str,
        failure: Value,
        emit_tool: bool,
    ) -> (AgentProbeEvidence, Vec<Value>) {
        let directory = tempfile::tempdir().unwrap();
        let (runtime, log) = fake_hi_runtime(directory.path(), terminal, failure, emit_tool);
        let backend = ProcessProbeBackend {
            runtime_source: Some(runtime),
        };
        let evidence = backend.probe(&AgentProbeInput {
            agent: "zcode".into(),
            through: ProbeLayer::Hi,
            scope: ProbeScope::default(),
        });
        let records = fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        (evidence, records)
    }

    #[test]
    fn hi_probe_uses_plan_policy_no_tools_and_a_restricted_disposable_workspace() {
        let (evidence, records) = run_fixture_probe("turn.completed", serde_json::json!({}), false);
        assert_eq!(evidence.auth.state, EvidenceState::Ready);
        assert_eq!(evidence.hi.state, EvidenceState::Ready);
        let workspace = evidence.hi.scope.workspace.as_deref().unwrap();
        assert!(workspace.contains("external-subagent-probe-"));
        assert!(!Path::new(workspace).exists());
        let create = records
            .iter()
            .find(|record| record["value"]["method"] == "session/create")
            .unwrap();
        assert_eq!(create["value"]["params"]["mode"], "plan");
        assert_eq!(create["policy"], "1");
        assert_eq!(create["mode"], "plan");
        assert_eq!(create["manifest"], "[]");
        assert_eq!(create["root"], workspace);
        assert_eq!(create["rootMode"], "700");
        assert!(records.iter().all(|record| {
            let method = record["value"]["method"].as_str().unwrap_or_default();
            !method.contains("tool") && !method.contains("permission")
        }));
    }

    #[test]
    fn async_terminal_failures_classify_auth_rate_limit_and_network() {
        for (failure, reason, state) in [
            (
                serde_json::json!({"error":{"code":401,"message":"unauthorized"}}),
                "auth_401",
                EvidenceState::Unavailable,
            ),
            (
                serde_json::json!({"error":{"code":429,"message":"rate limit"}}),
                "rate_limit",
                EvidenceState::Degraded,
            ),
            (
                serde_json::json!({"error":{"message":"network connection failed"}}),
                "network",
                EvidenceState::Unavailable,
            ),
        ] {
            let (evidence, _) = run_fixture_probe("turn.failed", failure, false);
            assert_eq!(evidence.auth.reason.as_deref(), Some(reason));
            assert_eq!(evidence.hi.reason.as_deref(), Some(reason));
            assert_eq!(evidence.auth.state, state);
            assert_eq!(evidence.hi.state, state);
        }
    }

    #[test]
    fn any_tool_signal_fails_the_read_only_probe() {
        let (evidence, _) = run_fixture_probe("turn.completed", serde_json::json!({}), true);
        assert_eq!(evidence.auth.state, EvidenceState::Unavailable);
        assert_eq!(evidence.hi.reason.as_deref(), Some("policy_violation"));
    }
}
