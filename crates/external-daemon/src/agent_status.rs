use crate::{LifecycleRecord, LifecycleSink, RuntimeCommandError, RuntimeOwner, TurnBoundary};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
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
        let scope = input.scope.clone();
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
    let owner = match RuntimeOwner::spawn(command, Arc::new(DiscardLifecycle)) {
        Ok(owner) => owner,
        Err(_) => {
            let evidence = unavailable(scope, version, checked_at_ms, "transport");
            return (evidence.clone(), evidence);
        }
    };
    let ready = owner.bootstrap_session(workspace, "Reply with exactly hi.", RUNTIME_PROBE_TIMEOUT);
    let (auth, hi) = match ready {
        Ok(session) => {
            let auth = ScopeEvidence {
                state: EvidenceState::Ready,
                scope: scope.clone(),
                version: version.clone(),
                checked_at_ms,
                reason: None,
            };
            let deadline = Instant::now() + RUNTIME_PROBE_TIMEOUT;
            let hi = loop {
                let snapshot = owner.turn_snapshot();
                match snapshot.boundary {
                    Some(TurnBoundary::Completed) => {
                        let _ = owner.close_session(&session.session_id, RUNTIME_STOP_GRACE);
                        break ScopeEvidence {
                            state: EvidenceState::Ready,
                            scope: scope.clone(),
                            version: version.clone(),
                            checked_at_ms,
                            reason: None,
                        };
                    }
                    Some(TurnBoundary::Failed) => {
                        break unavailable(scope, version.clone(), checked_at_ms, "transport")
                    }
                    None if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                    None => break unavailable(scope, version.clone(), checked_at_ms, "network"),
                }
            };
            (auth, hi)
        }
        Err(error) => {
            let reason = classify_runtime_error(&error);
            let evidence = unavailable(scope, version, checked_at_ms, reason);
            (evidence.clone(), evidence)
        }
    };
    let _ = owner.stop(RUNTIME_STOP_GRACE);
    (auth, hi)
}

fn classify_runtime_error(error: &RuntimeCommandError) -> &'static str {
    match error {
        RuntimeCommandError::Timeout => "network",
        RuntimeCommandError::Transport(_) => "transport",
        RuntimeCommandError::Remote(value) => {
            let code = value.get("code").and_then(Value::as_i64);
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if code == Some(401) || message.contains("401") || message.contains("unauthorized") {
                "auth_401"
            } else if code == Some(429) || message.contains("429") || message.contains("rate limit")
            {
                "rate_limit"
            } else if message.contains("network")
                || message.contains("dns")
                || message.contains("connect")
            {
                "network"
            } else {
                "transport"
            }
        }
        RuntimeCommandError::Unsupported | RuntimeCommandError::InvalidSession(_) => "transport",
    }
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

struct DiscardLifecycle;

impl LifecycleSink for DiscardLifecycle {
    fn emit(&self, _record: LifecycleRecord) {}
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
            classify_runtime_error(&RuntimeCommandError::Remote(
                serde_json::json!({"code":401,"message":"unauthorized"})
            )),
            "auth_401"
        );
        assert_eq!(
            classify_runtime_error(&RuntimeCommandError::Remote(
                serde_json::json!({"code":429,"message":"rate limit"})
            )),
            "rate_limit"
        );
        assert_eq!(
            classify_runtime_error(&RuntimeCommandError::Timeout),
            "network"
        );
        assert_eq!(
            classify_runtime_error(&RuntimeCommandError::Transport("closed".into())),
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
}
