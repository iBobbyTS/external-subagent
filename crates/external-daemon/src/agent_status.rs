use external_agent_dsh::{
    acp::session::AcpSession,
    profile::{preflight, resolve_launch, DshLaunch, STRICT_PLAN_PATCH_YAML},
};
use external_contract::{
    event_type, CreateSessionParams, RuntimePreferences, SendParams, SessionCreateProjection,
    SessionParams, SubscribeParams, WireMessage, WorkspaceRef, INTERACTION_REQUEST_PERMISSION,
    SESSION_CLOSE, SESSION_CREATE, SESSION_EVENT, SESSION_REQUEST_RUNTIME_PREFERENCES,
    SESSION_SEND, SESSION_SUBSCRIBE,
};
use external_runtime::{Driver, Inbound, RequestError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    collections::BTreeMap,
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const LOCAL_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const RUNTIME_PROBE_TIMEOUT: Duration = Duration::from_secs(90);
const RUNTIME_STOP_GRACE: Duration = Duration::from_secs(1);
const DSH_CATALOG_TIMEOUT: Duration = Duration::from_secs(5);
const DSH_MAX_FRAME_BYTES: usize = 1024 * 1024;
const DSH_MAX_MODELS: usize = 256;
const DSH_MAX_MODEL_TOKEN_BYTES: usize = 512;
const VERSION_STREAM_CAP: usize = 64 * 1024;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
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
    /// Configuration identity used when this probe was admitted. Evidence is
    /// only current while the daemon projects the same revision.
    pub config_revision: u64,
    pub local: ScopeEvidence,
    pub auth: ScopeEvidence,
    pub hi: ScopeEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentModelsInput {
    pub agent: String,
    #[serde(default)]
    pub scope: ProbeScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogEvidence {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub scope: ProbeScope,
    pub checked_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentModelsOutput {
    pub agent: String,
    pub config_revision: u64,
    pub supported: bool,
    pub models: Vec<String>,
    pub evidence: ModelCatalogEvidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

pub trait AgentProbeBackend: Send + Sync + 'static {
    fn probe(&self, input: &AgentProbeInput) -> AgentProbeEvidence;

    fn models(&self, input: &AgentModelsInput) -> AgentModelsOutput {
        unsupported_models(input, "native_only")
    }
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

    pub fn probe(&self, input: &AgentProbeInput, config_revision: u64) -> AgentProbeEvidence {
        let mut evidence = self.backend.probe(input);
        evidence.config_revision = config_revision;
        self.latest
            .lock()
            .unwrap()
            .insert(input.agent.clone(), evidence.clone());
        evidence
    }

    pub fn latest(&self, agent: &str) -> Option<AgentProbeEvidence> {
        self.latest.lock().unwrap().get(agent).cloned()
    }

    pub fn models(&self, input: &AgentModelsInput, config_revision: u64) -> AgentModelsOutput {
        let mut output = self.backend.models(input);
        output.config_revision = config_revision;
        output
    }
}

struct ProcessProbeBackend {
    runtime_source: Option<PathBuf>,
}

impl AgentProbeBackend for ProcessProbeBackend {
    fn probe(&self, input: &AgentProbeInput) -> AgentProbeEvidence {
        let checked_at_ms = wall_now_millis();
        let mut scope = input.scope.clone();
        if scope.home.is_none() {
            let variable = match input.agent.as_str() {
                "dsh" => "DSH_HOME",
                // Codex never falls back to ~/.codex: only an explicit home
                // (persisted configuration or exported CODEX_HOME) counts.
                "codex" if env::var_os("CODEX_HOME").is_some() => "CODEX_HOME",
                _ => "ZCODE_HOME",
            };
            scope.home = if variable == "CODEX_HOME" {
                env::var_os(variable)
            } else {
                env::var_os(variable).or_else(|| env::var_os("HOME"))
            }
            .map(|value| value.to_string_lossy().into_owned());
        }
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
            "codex" => env::var_os("CODEX_RUNTIME_PATH").map(PathBuf::from),
            _ => None,
        };
        let local = probe_local(executable.as_deref(), scope.clone(), checked_at_ms);
        let mut auth = ScopeEvidence::unknown(scope.clone(), checked_at_ms, "auth_not_probed");
        let mut hi = ScopeEvidence::unknown(scope.clone(), checked_at_ms, "hi_not_probed");

        if input.agent == "dsh" && input.through == ProbeLayer::Hi {
            let (a, h) = probe_dsh_hi(
                executable.as_deref(),
                &scope,
                local.version.clone(),
                checked_at_ms,
            );
            auth = a;
            hi = h;
        } else if input.agent == "dsh" && input.through == ProbeLayer::Auth {
            // DSH has no independent credential endpoint in the ACP dialect
            // we support. Auth-only must remain side-effect free: do not
            // create a session or send a prompt merely to infer auth.
            auth = ScopeEvidence::unknown(scope.clone(), checked_at_ms, "auth_not_probed");
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
            // The store replaces this sentinel with the revision read by the
            // RPC owner immediately before the probe starts.
            config_revision: 0,
            local,
            auth,
            hi,
        }
    }

    fn models(&self, input: &AgentModelsInput) -> AgentModelsOutput {
        if input.agent == "zcode" {
            return unsupported_models(input, "native_only");
        }
        if input.agent == "codex" {
            // The Codex catalog was observed only through the controlled live
            // probe; no in-band catalog claim is made here.
            return unsupported_models(input, "codex_models_probed_live_only");
        }
        probe_dsh_models(
            env::var_os("DSH_RUNTIME_PATH")
                .map(PathBuf::from)
                .as_deref(),
            input,
        )
    }
}

fn probe_dsh_hi(
    path: Option<&Path>,
    scope: &ProbeScope,
    version: Option<String>,
    checked: u64,
) -> (ScopeEvidence, ScopeEvidence) {
    let unavailable = |reason: &str| {
        let e = ScopeEvidence {
            state: EvidenceState::Unavailable,
            scope: scope.clone(),
            version: version.clone(),
            checked_at_ms: checked,
            reason: Some(reason.into()),
        };
        (
            ScopeEvidence::unknown(scope.clone(), checked, "auth_not_probed"),
            e,
        )
    };
    let Some(workspace) = scope.workspace.as_deref() else {
        return unavailable("workspace_missing");
    };
    let mut launch = DshLaunch::new(
        path.map(PathBuf::from),
        workspace,
        scope.home.as_ref().map(PathBuf::from),
    );
    launch.profile = scope
        .profile
        .clone()
        .or_else(|| env::var("DSH_PROFILE").ok());
    launch.version = scope
        .version
        .clone()
        .or_else(|| env::var("DSH_VERSION").ok());
    launch.permission_mode = Some("read-only".into());
    // The embedded resource survives native/npm installation without the build
    // source tree. The private directory lives until the ACP child is reaped.
    let patch_directory = match tempfile::Builder::new()
        .prefix("external-dsh-hi-")
        .tempdir()
    {
        Ok(directory) => directory,
        Err(_) => return unavailable("auth_hi_unavailable"),
    };
    let patch = patch_directory.path().join("strict-plan.patch.yml");
    if fs::write(&patch, STRICT_PLAN_PATCH_YAML).is_err() {
        return unavailable("auth_hi_unavailable");
    }
    launch.patch = Some(patch);
    if preflight(&launch).is_err() {
        return unavailable("strict_preflight_failed");
    }
    let mut command = match resolve_launch(&launch) {
        Ok(c) => c,
        Err(_) => return unavailable("auth_hi_unavailable"),
    };
    command.current_dir(workspace);
    let driver = match Driver::spawn_with_codec(command, external_runtime::FrameCodec::JsonRpc2) {
        Ok(d) => Arc::new(d),
        Err(_) => return unavailable("network"),
    };
    let mut session = AcpSession::new(Arc::clone(&driver));
    let result = (|| {
        session.initialize(LOCAL_PROBE_TIMEOUT)?;
        session.new_session(Path::new(workspace), LOCAL_PROBE_TIMEOUT)?;
        let (_, pending) = session.prompt("Reply with exactly hi. Do not call tools.")?;
        pending
            .wait(LOCAL_PROBE_TIMEOUT)
            .map_err(external_agent_dsh::acp::session::SessionError::from)?;
        Ok::<(), external_agent_dsh::acp::session::SessionError>(())
    })();
    let _ = session.close(LOCAL_PROBE_TIMEOUT);
    let _ = driver.stop_and_reap(RUNTIME_STOP_GRACE);
    match result {
        Ok(()) => {
            let hi = ScopeEvidence {
                state: EvidenceState::Ready,
                scope: scope.clone(),
                version,
                checked_at_ms: checked,
                reason: None,
            };
            let auth = ScopeEvidence::unknown(scope.clone(), checked, "auth_not_probed");
            (auth, hi)
        }
        Err(e) => {
            let reason = classify_dsh_session_error(&e);
            let hi = ScopeEvidence {
                state: EvidenceState::Unavailable,
                scope: scope.clone(),
                version,
                checked_at_ms: checked,
                reason: Some(reason.into()),
            };
            let auth = ScopeEvidence::unknown(scope.clone(), checked, "auth_not_probed");
            (auth, hi)
        }
    }
}

fn unsupported_models(input: &AgentModelsInput, reason: &str) -> AgentModelsOutput {
    AgentModelsOutput {
        agent: input.agent.clone(),
        config_revision: 0,
        supported: false,
        models: Vec::new(),
        evidence: ModelCatalogEvidence {
            source: match input.agent.as_str() {
                "zcode" => "zcode_native_model".into(),
                "codex" => "codex_app_server_model_list".into(),
                _ => "dsh_acp_models_list".into(),
            },
            version: None,
            scope: input.scope.clone(),
            checked_at_ms: wall_now_millis(),
        },
        reason: Some(reason.into()),
    }
}

fn probe_dsh_models(path: Option<&Path>, input: &AgentModelsInput) -> AgentModelsOutput {
    let checked_at_ms = wall_now_millis();
    let mut scope = input.scope.clone();
    if scope.home.is_none() {
        scope.home = env::var_os("DSH_HOME")
            .or_else(|| env::var_os("HOME"))
            .map(|value| value.to_string_lossy().into_owned());
    }
    let disposable_workspace = if scope.workspace.is_none() {
        tempfile::Builder::new()
            .prefix("external-subagent-dsh-catalog-")
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
    let local = probe_local(path, scope.clone(), checked_at_ms);
    let evidence = ModelCatalogEvidence {
        source: "dsh_acp_models_list".into(),
        version: local.version.clone(),
        scope: scope.clone(),
        checked_at_ms,
    };
    if local.state != EvidenceState::Ready {
        return AgentModelsOutput {
            agent: input.agent.clone(),
            config_revision: 0,
            supported: false,
            models: Vec::new(),
            evidence,
            reason: local.reason,
        };
    }
    let Some(workspace) = scope.workspace.as_deref() else {
        return AgentModelsOutput {
            agent: input.agent.clone(),
            config_revision: 0,
            supported: false,
            models: Vec::new(),
            evidence,
            reason: Some("workspace_required".into()),
        };
    };
    if !Path::new(workspace).is_absolute() || !Path::new(workspace).is_dir() {
        return AgentModelsOutput {
            agent: input.agent.clone(),
            config_revision: 0,
            supported: false,
            models: Vec::new(),
            evidence,
            reason: Some("workspace_missing".into()),
        };
    }
    let result = run_dsh_catalog(
        path.expect("ready local evidence has a runtime path"),
        &scope,
        DSH_CATALOG_TIMEOUT,
    );
    match result {
        Ok(models) if !models.is_empty() => AgentModelsOutput {
            agent: input.agent.clone(),
            config_revision: 0,
            supported: true,
            models,
            evidence,
            reason: None,
        },
        Ok(_) => AgentModelsOutput {
            agent: input.agent.clone(),
            config_revision: 0,
            supported: false,
            models: Vec::new(),
            evidence,
            reason: Some("model_catalog_empty".into()),
        },
        Err(reason) => AgentModelsOutput {
            agent: input.agent.clone(),
            config_revision: 0,
            supported: false,
            models: Vec::new(),
            evidence,
            reason: Some(reason),
        },
    }
}

fn run_dsh_catalog(
    executable: &Path,
    scope: &ProbeScope,
    timeout: Duration,
) -> Result<Vec<String>, String> {
    let workspace = scope
        .workspace
        .as_deref()
        .ok_or_else(|| "workspace_required".to_owned())?;
    let mut launch = DshLaunch::new(
        Some(executable.to_path_buf()),
        workspace,
        scope.home.clone().map(PathBuf::from),
    );
    launch.profile = scope
        .profile
        .clone()
        .or_else(|| env::var("DSH_PROFILE").ok());
    launch.version = scope
        .version
        .clone()
        .or_else(|| env::var("DSH_VERSION").ok());
    let mut command =
        resolve_launch(&launch).map_err(|_| "unsupported_profile_or_version".to_owned())?;
    command
        .env("DSH_ACP_PROBE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(home) = scope.home.as_deref() {
        command.env("DSH_HOME", home);
    }
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|_| "transport".to_owned())?;
    let process_group = child.id() as i32;
    let (Some(mut input), Some(output), Some(diagnostic)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        cleanup_catalog_process(&mut child, process_group, Duration::from_millis(500));
        return Err("transport".into());
    };
    let (frames_tx, frames_rx) = mpsc::channel();
    thread::spawn(move || read_dsh_frames(output, frames_tx));
    let (diagnostic_tx, diagnostic_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = diagnostic.take(64 * 1024).read_to_end(&mut bytes);
        let _ = diagnostic_tx.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    let deadline = Instant::now() + timeout;
    let result = (|| {
        let initialized = dsh_call(
            &mut input,
            &frames_rx,
            1,
            "initialize",
            serde_json::json!({
                "protocolVersion": 1,
                "clientInfo": {"name":"external-subagent-dsh-probe","version":env!("CARGO_PKG_VERSION")}
            }),
            deadline,
        )?;
        if initialized.get("protocolVersion").and_then(Value::as_u64) != Some(1)
            && initialized.get("agentCapabilities").is_none()
        {
            return Err("protocol_version".into());
        }
        let session = dsh_call(
            &mut input,
            &frames_rx,
            2,
            "session/new",
            serde_json::json!({"cwd": workspace, "mcpServers": []}),
            deadline,
        )?;
        if session
            .get("sessionId")
            .and_then(Value::as_str)
            .is_none_or(|value| value.is_empty() || value.len() > 512)
        {
            return Err("protocol".into());
        }
        let session_catalog = parse_opaque_model_tokens(&session).unwrap_or_default();
        match dsh_call(
            &mut input,
            &frames_rx,
            3,
            "models/list",
            serde_json::json!({}),
            deadline,
        ) {
            Ok(catalog) => match parse_opaque_model_tokens(&catalog) {
                Ok(tokens) if !tokens.is_empty() => Ok(tokens),
                _ if !session_catalog.is_empty() => Ok(session_catalog),
                // Ok must imply a non-empty catalog: when neither models/list
                // nor session/new exposed a usable token, fail closed with a
                // diagnosable reason instead of an empty Ok catalog.
                Ok(_) => Err("model_catalog_empty".into()),
                Err(error) => Err(error),
            },
            Err(_) if !session_catalog.is_empty() => Ok(session_catalog),
            Err(error) => Err(error),
        }
    })();
    drop(input);
    cleanup_catalog_process(&mut child, process_group, Duration::from_millis(500));
    let diagnostic_tail = diagnostic_rx
        .recv_timeout(Duration::from_millis(200))
        .unwrap_or_default();
    if let Err(reason) = result {
        if reason == "transport" {
            let classified = classify_provider_failure(&Value::Null, &diagnostic_tail);
            return Err(classified.into());
        }
        return Err(reason);
    }
    result
}

fn cleanup_catalog_process(child: &mut Child, process_group: i32, grace: Duration) {
    #[cfg(unix)]
    unsafe {
        let _ = libc::kill(-process_group, libc::SIGTERM);
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(_) => break,
        }
    }
    // Kill the whole group even when the leader already exited: descendants
    // may still own inherited stdout/stderr descriptors.
    #[cfg(unix)]
    unsafe {
        let _ = libc::kill(-process_group, libc::SIGKILL);
    }
    let _ = child.wait();
}

fn read_dsh_frames(output: impl Read, sender: mpsc::Sender<Result<Value, String>>) {
    let mut output = output;
    let mut frame = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 4096];
    loop {
        match output.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => {
                for byte in &chunk[..read] {
                    if *byte == b'\n' {
                        if matches!(frame.last(), Some(b'\r')) {
                            frame.pop();
                        }
                        if !frame.is_empty() {
                            let parsed =
                                serde_json::from_slice(&frame).map_err(|_| "protocol".into());
                            if sender.send(parsed).is_err() {
                                return;
                            }
                            frame.clear();
                        }
                    } else {
                        if frame.len() == DSH_MAX_FRAME_BYTES {
                            let _ = sender.send(Err("oversized".into()));
                            return;
                        }
                        frame.push(*byte);
                    }
                }
            }
            Err(_) => {
                let _ = sender.send(Err("transport".into()));
                return;
            }
        }
    }
}

fn dsh_call(
    input: &mut impl Write,
    frames: &mpsc::Receiver<Result<Value, String>>,
    id: u64,
    method: &str,
    params: Value,
    deadline: Instant,
) -> Result<Value, String> {
    let mut request = serde_json::to_vec(&serde_json::json!({
        "jsonrpc":"2.0", "id":id, "method":method, "params":params
    }))
    .map_err(|_| "protocol".to_owned())?;
    request.push(b'\n');
    if request.len() > DSH_MAX_FRAME_BYTES {
        return Err("oversized".into());
    }
    input
        .write_all(&request)
        .map_err(|_| "transport".to_owned())?;
    input.flush().map_err(|_| "transport".to_owned())?;
    for _ in 0..256 {
        let frame = frames
            .recv_timeout(remaining(deadline)?)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => "network".to_owned(),
                mpsc::RecvTimeoutError::Disconnected => "transport".to_owned(),
            })??;
        if frame.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if frame.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err("protocol".into());
        }
        if let Some(error) = frame.get("error") {
            return Err(classify_provider_failure(error, "").into());
        }
        return frame
            .get("result")
            .filter(|result| !result.is_null())
            .cloned()
            .ok_or_else(|| "protocol".into());
    }
    Err("protocol".into())
}

fn parse_opaque_model_tokens(catalog: &Value) -> Result<Vec<String>, String> {
    let models = catalog
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut tokens = Vec::new();
    if let Some(options) = catalog.get("configOptions").and_then(Value::as_array) {
        for option in options {
            let id = option
                .get("id")
                .or_else(|| option.get("configId"))
                .and_then(Value::as_str);
            if id != Some("model") {
                continue;
            }
            let mut add = |v: Option<&Value>| {
                if let Some(t) = v.and_then(Value::as_str) {
                    if !t.is_empty()
                        && t.len() <= DSH_MAX_MODEL_TOKEN_BYTES
                        && !t.contains('\0')
                        && !tokens.iter().any(|x| x == t)
                    {
                        tokens.push(t.to_owned());
                    }
                }
            };
            add(option.get("currentValue"));
            fn walk(value: &Value, add: &mut dyn FnMut(Option<&Value>)) {
                add(value.get("value"));
                if let Some(vals) = value.get("options").and_then(Value::as_array) {
                    for nested in vals {
                        walk(nested, add);
                    }
                }
            }
            if let Some(vals) = option.get("options").and_then(Value::as_array) {
                for v in vals {
                    walk(v, &mut add);
                }
            }
        }
    }
    if !tokens.is_empty() {
        return Ok(tokens);
    }
    if models.len() > DSH_MAX_MODELS {
        return Err("oversized".into());
    }
    tokens = Vec::with_capacity(models.len());
    for model in models {
        let token = model
            .get("id")
            .and_then(Value::as_str)
            .filter(|token| {
                !token.is_empty()
                    && token.len() <= DSH_MAX_MODEL_TOKEN_BYTES
                    && !token.contains('\0')
            })
            .ok_or_else(|| "protocol".to_owned())?;
        if !tokens.iter().any(|existing| existing == token) {
            tokens.push(token.to_owned());
        }
    }
    Ok(tokens)
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
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|_| "transport".to_owned())?;
    let process_group = child.id() as i32;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        cleanup_catalog_process(&mut child, process_group, Duration::from_millis(250));
        return Err("transport".into());
    };
    let (stdout_tx, stdout_rx) = mpsc::channel();
    let (stderr_tx, stderr_rx) = mpsc::channel();
    thread::spawn(move || read_version_stream(stdout, stdout_tx));
    thread::spawn(move || read_version_stream(stderr, stderr_tx));
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let deadline = Instant::now() + LOCAL_PROBE_TIMEOUT;
    let status = loop {
        if drain_version_stream(&stdout_rx, &mut stdout_bytes).is_err()
            || drain_version_stream(&stderr_rx, &mut stderr_bytes).is_err()
        {
            cleanup_catalog_process(&mut child, process_group, Duration::from_millis(250));
            return Err("oversized".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                cleanup_catalog_process(&mut child, process_group, Duration::from_millis(250));
                return Err("transport".into());
            }
            Err(_) => {
                cleanup_catalog_process(&mut child, process_group, Duration::from_millis(250));
                return Err("transport".into());
            }
        }
    };
    cleanup_catalog_process(&mut child, process_group, Duration::from_millis(250));
    finish_version_streams(
        &stdout_rx,
        &stderr_rx,
        &mut stdout_bytes,
        &mut stderr_bytes,
        Instant::now() + Duration::from_millis(250),
    )?;
    if !status.success() {
        return Err("version".into());
    }
    let text = String::from_utf8_lossy(&stdout_bytes);
    let diagnostic = String::from_utf8_lossy(&stderr_bytes);
    let version = text
        .lines()
        .chain(diagnostic.lines())
        .map(str::trim)
        .find(|line| !line.is_empty());
    version
        .filter(|value| valid_version(value) && !value.contains('\0'))
        .map(str::to_owned)
        .or_else(|| package_version(path))
        .ok_or_else(|| "version".into())
}

fn finish_version_streams(
    stdout: &mpsc::Receiver<Result<Vec<u8>, String>>,
    stderr: &mpsc::Receiver<Result<Vec<u8>, String>>,
    stdout_bytes: &mut Vec<u8>,
    stderr_bytes: &mut Vec<u8>,
    deadline: Instant,
) -> Result<(), String> {
    let mut stdout_closed = false;
    let mut stderr_closed = false;
    while !(stdout_closed && stderr_closed) && Instant::now() < deadline {
        stdout_closed |= drain_version_stream_until_empty(stdout, stdout_bytes)?;
        stderr_closed |= drain_version_stream_until_empty(stderr, stderr_bytes)?;
        if !(stdout_closed && stderr_closed) {
            thread::sleep(Duration::from_millis(2));
        }
    }
    Ok(())
}

fn drain_version_stream_until_empty(
    receiver: &mpsc::Receiver<Result<Vec<u8>, String>>,
    output: &mut Vec<u8>,
) -> Result<bool, String> {
    loop {
        match receiver.try_recv() {
            Ok(Ok(chunk)) => {
                if output.len().saturating_add(chunk.len()) > VERSION_STREAM_CAP {
                    return Err("oversized".into());
                }
                output.extend_from_slice(&chunk);
            }
            Ok(Err(reason)) => return Err(reason),
            Err(mpsc::TryRecvError::Disconnected) => return Ok(true),
            Err(mpsc::TryRecvError::Empty) => return Ok(false),
        }
    }
}

fn read_version_stream(mut stream: impl Read, sender: mpsc::Sender<Result<Vec<u8>, String>>) {
    let mut buffer = [0_u8; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => {
                if sender.send(Ok(buffer[..read].to_vec())).is_err() {
                    return;
                }
            }
            Err(_) => {
                let _ = sender.send(Err("transport".into()));
                return;
            }
        }
    }
}

fn drain_version_stream(
    receiver: &mpsc::Receiver<Result<Vec<u8>, String>>,
    output: &mut Vec<u8>,
) -> Result<(), String> {
    loop {
        match receiver.try_recv() {
            Ok(Ok(chunk)) => {
                if output.len().saturating_add(chunk.len()) > VERSION_STREAM_CAP {
                    return Err("oversized".into());
                }
                output.extend_from_slice(&chunk);
            }
            Ok(Err(reason)) => return Err(reason),
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => return Ok(()),
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
            if valid_version(version) {
                return Some(version.to_owned());
            }
        }
    }
    None
}

fn valid_version(version: &str) -> bool {
    #[cfg(test)]
    if version.starts_with("fixture-") || version.starts_with("dsh-fixture-") {
        return true;
    }
    let core = version.split(['-', '+']).next().unwrap_or_default();
    let parts = core.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        && version.len() <= 128
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
    if !verified_read_only_policy(scope, workspace) {
        let evidence = unavailable(scope, version, checked_at_ms, "policy_unverified");
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

fn verified_read_only_policy(scope: &ProbeScope, workspace: &str) -> bool {
    let Some(home) = scope.home.as_deref() else {
        return false;
    };
    let verifier = env::var_os("EXTERNAL_SUBAGENT_POLICY_VERIFIER")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(home).join(
                "Library/Application Support/external-subagent/external-subagent-policy-verifier",
            )
        });
    if !verifier.is_file() {
        return false;
    }
    let mut command = Command::new(&verifier);
    command
        .args([
            "--workspace",
            workspace,
            "--home",
            home,
            "--permission-mode",
            "plan",
            "--write-manifest",
            "[]",
        ])
        .env("ZCODE_HOME", home)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let group = child.id() as i32;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => {
                cleanup_catalog_process(&mut child, group, Duration::from_millis(100));
                return false;
            }
        }
    }
}

fn run_read_only_hi(driver: Arc<Driver>, workspace: &str) -> Result<String, String> {
    let deadline = Instant::now() + RUNTIME_PROBE_TIMEOUT;
    let mut events = ProbeEventCache::default();
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
        &mut events,
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
        &mut events,
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
        &mut events,
    )?;

    loop {
        if let Some(terminal) = events.terminal.take() {
            return match terminal {
                ProbeTerminal::Completed => Ok(session_id),
                ProbeTerminal::Failed(reason) => Err(reason),
            };
        }
        let wait = remaining(deadline)?.min(Duration::from_millis(50));
        match driver.recv_timeout(wait) {
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
            Ok(inbound) => events.observe(&inbound, &driver.diagnostic_tail())?,
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
    events: &mut ProbeEventCache,
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
        drain_probe_events(&driver, events)?;
        if let Some(ProbeTerminal::Failed(reason)) = events.terminal.as_ref() {
            return Err(reason.clone());
        }
        match receiver.try_recv() {
            Ok(result) => {
                if result.is_err() {
                    drain_probe_events(&driver, events)?;
                }
                return result.map_err(|error| {
                    let fallback = classify_request_error(&error, &driver.diagnostic_tail());
                    events.failure_reason_or(fallback)
                });
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(events.failure_reason_or("transport".into()))
            }
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
            Ok(inbound) => events.observe(&inbound, &driver.diagnostic_tail())?,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("transport".into()),
        }
    }
}

fn drain_probe_events(driver: &Driver, events: &mut ProbeEventCache) -> Result<(), String> {
    loop {
        match driver.recv_timeout(Duration::ZERO) {
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
            Ok(inbound) => events.observe(&inbound, &driver.diagnostic_tail())?,
            Err(mpsc::RecvTimeoutError::Timeout) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(events.failure_reason_or("transport".into()))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeTerminal {
    Completed,
    Failed(String),
}

#[derive(Debug, Default)]
struct ProbeEventCache {
    terminal: Option<ProbeTerminal>,
    diagnostic_reason: Option<String>,
}

impl ProbeEventCache {
    fn observe(&mut self, inbound: &Inbound, diagnostic: &str) -> Result<(), String> {
        match inbound {
            Inbound::Message(WireMessage::Event(event)) => {
                if event
                    .params
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind.starts_with("tool.") || kind == "permission.requested")
                {
                    return Err("policy_violation".into());
                }
                if event.method == SESSION_EVENT {
                    match event_type(event) {
                        Some("turn.completed") => {
                            if self.terminal.is_none() {
                                self.terminal = Some(ProbeTerminal::Completed);
                            }
                        }
                        Some("turn.failed") => {
                            let reason =
                                classify_provider_failure(&event.params, diagnostic).to_owned();
                            self.diagnostic_reason = Some(reason.clone());
                            self.terminal = Some(ProbeTerminal::Failed(reason));
                        }
                        _ => self.capture_diagnostic(&event.params, diagnostic),
                    }
                }
            }
            Inbound::Message(WireMessage::UnknownEvent { raw, .. }) => {
                if contains_tool_signal(raw) {
                    return Err("policy_violation".into());
                }
                self.capture_diagnostic(raw, diagnostic);
            }
            Inbound::Lifecycle { method, .. } if method == "turn.completed" => {
                if self.terminal.is_none() {
                    self.terminal = Some(ProbeTerminal::Completed);
                }
            }
            Inbound::Lifecycle { method, .. } if method == "turn.failed" => {
                // The driver publishes the lifecycle marker before the matching
                // event body. Keep the boundary, but wait for that body so an
                // asynchronous 401/429/network reason is not collapsed.
            }
            Inbound::Malformed(_) | Inbound::OversizedLine { .. } => return Err("transport".into()),
            Inbound::ChildExited(_) => {
                self.terminal = Some(ProbeTerminal::Failed(
                    self.diagnostic_reason.clone().unwrap_or_else(|| {
                        classify_provider_failure(&Value::Null, diagnostic).into()
                    }),
                ));
            }
            Inbound::UnmatchedResponse { .. } => return Err("transport".into()),
            Inbound::Message(WireMessage::Request(_) | WireMessage::Response(_))
            | Inbound::Lifecycle { .. } => {}
        }
        Ok(())
    }

    fn capture_diagnostic(&mut self, value: &Value, diagnostic: &str) {
        let reason = classify_provider_failure(value, diagnostic);
        if reason != "transport" {
            self.diagnostic_reason = Some(reason.into());
        }
    }

    fn failure_reason_or(&self, fallback: String) -> String {
        match self.terminal.as_ref() {
            Some(ProbeTerminal::Failed(reason)) => reason.clone(),
            _ => self.diagnostic_reason.clone().unwrap_or(fallback),
        }
    }
}

fn remaining(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| "network".into())
}

fn classify_dsh_session_error(
    error: &external_agent_dsh::acp::session::SessionError,
) -> &'static str {
    use external_agent_dsh::acp::session::SessionError;
    match error {
        SessionError::Timeout => "timeout",
        SessionError::Shape(_) => "shape",
        SessionError::Remote(value) => {
            let code = value.get("code").and_then(|code| {
                code.as_i64().or_else(|| {
                    code.as_str()
                        .filter(|code| code.len() <= 16)
                        .and_then(|code| code.parse::<i64>().ok())
                })
            });
            match code {
                Some(-32001) | Some(401) | Some(403) => "auth",
                Some(-32002) | Some(429) => "rate_limit",
                _ => {
                    // Inspect only a bounded message, never arbitrary remote data
                    // or numeric substrings that may merely be request IDs.
                    let message = value
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .chars()
                        .take(512)
                        .collect::<String>()
                        .to_ascii_lowercase();
                    if message.contains("unauthorized") || message.contains("unauthenticated") {
                        "auth"
                    } else if message.contains("rate limit") || message.contains("rate_limit") {
                        "rate_limit"
                    } else {
                        "remote"
                    }
                }
            }
        }
        SessionError::Transport(_) => "network",
        SessionError::Model(_) => "shape",
    }
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
            config_revision: 0,
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
            profile: None,
            version: None,
        };
        let store = AgentEvidenceStore::with_backend(Arc::new(FixtureBackend {
            evidence: evidence(scope.clone()),
        }));
        assert_eq!(store.latest("zcode"), None);
        let observed = store.probe(
            &AgentProbeInput {
                agent: "zcode".into(),
                through: ProbeLayer::Auth,
                scope: scope.clone(),
            },
            7,
        );
        assert_eq!(observed.config_revision, 7);
        assert_eq!(observed.local.scope, scope);
        assert_eq!(store.latest("zcode"), Some(observed));
        assert_eq!(store.latest("dsh"), None);
    }

    #[test]
    fn dsh_session_errors_classify_codes_and_bounded_messages() {
        use external_agent_dsh::acp::session::SessionError;
        for (code, expected) in [
            (401, "auth"),
            (403, "auth"),
            (429, "rate_limit"),
            (-32001, "auth"),
            (-32002, "rate_limit"),
        ] {
            for code in [serde_json::json!(code), serde_json::json!(code.to_string())] {
                assert_eq!(
                    classify_dsh_session_error(&SessionError::Remote(
                        serde_json::json!({"code": code, "message": "provider failed"})
                    )),
                    expected
                );
            }
        }
        for (value, expected) in [
            (serde_json::json!({"message": "Unauthorized"}), "auth"),
            (
                serde_json::json!({"code": -32603, "message": "Unauthenticated"}),
                "auth",
            ),
            (
                serde_json::json!({"message": "Rate limit exceeded"}),
                "rate_limit",
            ),
            (serde_json::json!({"message": "RATE_LIMIT"}), "rate_limit"),
            (
                serde_json::json!({"code": "401x", "message": "request 429 failed"}),
                "remote",
            ),
            (serde_json::json!({"data": "unauthorized"}), "remote"),
            (
                serde_json::json!({"message": format!("{}unauthorized", "x".repeat(512))}),
                "remote",
            ),
            (serde_json::json!({"message": "未知错误"}), "remote"),
            (serde_json::json!({"message": 401}), "remote"),
            (
                serde_json::json!({"code": 429, "message": "unauthorized"}),
                "rate_limit",
            ),
        ] {
            assert_eq!(
                classify_dsh_session_error(&SessionError::Remote(value)),
                expected
            );
        }
        assert_eq!(
            classify_dsh_session_error(&SessionError::Timeout),
            "timeout"
        );
        assert_eq!(
            classify_dsh_session_error(&SessionError::Transport("closed".into())),
            "network"
        );
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
        assert_eq!(result.hi.reason.as_deref(), Some("workspace_missing"));
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

    #[test]
    fn version_probe_reaps_descendant_inheriting_output_pipes_after_leader_exit() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("version-descendant");
        let descendant_pid = directory.path().join("descendant.pid");
        let source = format!(
            "#!/bin/sh\n(sleep 20) &\nprintf '%s\\n' $! > '{}'\nprintf 'fixture-9.9.9\\n'\nexit 0\n",
            descendant_pid.display()
        );
        fs::write(&executable, source).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let started = Instant::now();
        let version = executable_version(&executable).unwrap();
        assert_eq!(version, "fixture-9.9.9");
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid = fs::read_to_string(descendant_pid)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert!(wait_process_gone(
            pid,
            Instant::now() + Duration::from_secs(1)
        ));
    }

    #[test]
    fn version_probe_rejects_unterminated_oversized_stream_without_waiting_for_eof() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("version-oversized");
        let source = "#!/bin/sh\nprintf '%*s' 70000 x\nsleep 20\n";
        fs::write(&executable, source).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let started = Instant::now();
        let error = executable_version(&executable).unwrap_err();
        assert_eq!(error, "oversized");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    fn fake_hi_runtime(
        directory: &Path,
        terminal: &str,
        failure: Value,
        emit_tool: bool,
        terminal_before_response: bool,
        after_terminal: &str,
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
const terminalBeforeResponse = __TERMINAL_BEFORE_RESPONSE__;
const afterTerminal = __AFTER_TERMINAL__;
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
      const response = { id: value.id, result: { turnId: 'probe-turn' } };
      const started = { method: 'session/event', params: { eventId: 'start', sessionId: 'probe-session', seq: 1, timestamp: 1, type: 'turn.started', payload: {} } };
      const tool = { method: 'session/event', params: { eventId: 'tool', sessionId: 'probe-session', seq: 2, timestamp: 2, type: 'tool.updated', payload: { toolName: 'Bash' } } };
      const ended = { method: 'session/event', params: { eventId: 'end', sessionId: 'probe-session', seq: 3, timestamp: 3, type: terminal, payload: failure } };
      if (terminalBeforeResponse) {
        write(started); if (emitTool) write(tool); write(ended);
        if (afterTerminal === 'rpc_error') setTimeout(() => write({ id: value.id, error: { code: -32000, message: 'generic transport error' } }), 0);
        else if (afterTerminal === 'exit') setTimeout(() => process.exit(7), 0);
        else setTimeout(() => write(response), 0);
      } else {
        write(response); write(started); if (emitTool) write(tool); write(ended);
      }
    } else if (value.method === 'session/close') {
      write({ id: value.id, result: { closed: true } });
    }
  }
});
"#
        .replace("__LOG__", &serde_json::to_string(&log).unwrap())
        .replace("__TERMINAL__", &serde_json::to_string(terminal).unwrap())
        .replace("__FAILURE__", &failure.to_string())
        .replace("__EMIT_TOOL__", if emit_tool { "true" } else { "false" })
        .replace(
            "__TERMINAL_BEFORE_RESPONSE__",
            if terminal_before_response { "true" } else { "false" },
        )
        .replace(
            "__AFTER_TERMINAL__",
            &serde_json::to_string(after_terminal).unwrap(),
        );
        fs::write(&executable, source).unwrap();
        (executable, log)
    }

    fn run_fixture_probe(
        terminal: &str,
        failure: Value,
        emit_tool: bool,
    ) -> (AgentProbeEvidence, Vec<Value>) {
        run_fixture_probe_ordered(terminal, failure, emit_tool, false)
    }

    fn run_fixture_probe_ordered(
        terminal: &str,
        failure: Value,
        emit_tool: bool,
        terminal_before_response: bool,
    ) -> (AgentProbeEvidence, Vec<Value>) {
        run_fixture_probe_after_terminal(
            terminal,
            failure,
            emit_tool,
            terminal_before_response,
            "success",
        )
    }

    fn run_fixture_probe_after_terminal(
        terminal: &str,
        failure: Value,
        emit_tool: bool,
        terminal_before_response: bool,
        after_terminal: &str,
    ) -> (AgentProbeEvidence, Vec<Value>) {
        let directory = tempfile::tempdir().unwrap();
        let verifier = directory.path().join(
            "Library/Application Support/external-subagent/external-subagent-policy-verifier",
        );
        fs::create_dir_all(verifier.parent().unwrap()).unwrap();
        fs::write(
            &verifier,
            b"#!/bin/sh\n[ \"$1\" = \"--workspace\" ] && [ -d \"$2\" ] && [ \"$3\" = \"--home\" ] && [ -d \"$4\" ] && [ \"$5\" = \"--permission-mode\" ] && [ \"$6\" = \"plan\" ] && [ \"$7\" = \"--write-manifest\" ] && [ \"$8\" = \"[]\" ]\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&verifier, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let (runtime, log) = fake_hi_runtime(
            directory.path(),
            terminal,
            failure,
            emit_tool,
            terminal_before_response,
            after_terminal,
        );
        let backend = ProcessProbeBackend {
            runtime_source: Some(runtime),
        };
        let evidence = backend.probe(&AgentProbeInput {
            agent: "zcode".into(),
            through: ProbeLayer::Hi,
            scope: ProbeScope {
                home: Some(directory.path().to_string_lossy().into_owned()),
                ..ProbeScope::default()
            },
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
    fn hi_probe_refuses_prompt_without_scope_policy_verifier() {
        let directory = tempfile::tempdir().unwrap();
        let (runtime, log) = fake_hi_runtime(
            directory.path(),
            "turn.completed",
            serde_json::json!({}),
            false,
            false,
            "success",
        );
        let evidence = ProcessProbeBackend {
            runtime_source: Some(runtime),
        }
        .probe(&AgentProbeInput {
            agent: "zcode".into(),
            through: ProbeLayer::Hi,
            scope: ProbeScope::default(),
        });
        assert_eq!(evidence.hi.reason.as_deref(), Some("policy_unverified"));
        assert!(!log.exists());
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

    #[test]
    fn terminal_events_seen_before_send_response_are_cached_without_timeout() {
        for (terminal, failure, reason, state) in [
            (
                "turn.completed",
                serde_json::json!({}),
                None,
                EvidenceState::Ready,
            ),
            (
                "turn.failed",
                serde_json::json!({"error":{"code":401,"message":"unauthorized"}}),
                Some("auth_401"),
                EvidenceState::Unavailable,
            ),
            (
                "turn.failed",
                serde_json::json!({"error":{"code":429,"message":"rate limit"}}),
                Some("rate_limit"),
                EvidenceState::Degraded,
            ),
        ] {
            let started = Instant::now();
            let (evidence, _) = run_fixture_probe_ordered(terminal, failure, false, true);
            assert!(started.elapsed() < Duration::from_secs(5));
            assert_eq!(evidence.hi.state, state);
            assert_eq!(evidence.auth.state, state);
            assert_eq!(evidence.hi.reason.as_deref(), reason);
        }
    }

    #[test]
    fn cached_failure_reason_beats_later_rpc_error_or_process_exit() {
        for (failure, after_terminal, reason, state) in [
            (
                serde_json::json!({"error":{"code":401,"message":"unauthorized"}}),
                "rpc_error",
                "auth_401",
                EvidenceState::Unavailable,
            ),
            (
                serde_json::json!({"error":{"code":429,"message":"rate limit"}}),
                "exit",
                "rate_limit",
                EvidenceState::Degraded,
            ),
            (
                serde_json::json!({"error":{"message":"network connection failed"}}),
                "rpc_error",
                "network",
                EvidenceState::Unavailable,
            ),
        ] {
            let started = Instant::now();
            let (evidence, _) = run_fixture_probe_after_terminal(
                "turn.failed",
                failure,
                false,
                true,
                after_terminal,
            );
            assert!(started.elapsed() < Duration::from_secs(5));
            assert_eq!(evidence.auth.reason.as_deref(), Some(reason));
            assert_eq!(evidence.hi.reason.as_deref(), Some(reason));
            assert_eq!(evidence.auth.state, state);
            assert_eq!(evidence.hi.state, state);
        }
    }

    #[test]
    fn dsh_hi_preflights_actual_scope_and_reaps_embedded_patch() {
        // The drift scenario is the R0 counterexample: controls present but
        // the enabled sandbox-policy carries no config and approval is
        // disabled, so the strict preflight must keep the probe prompt-free.
        for (unknown, drift) in [(false, false), (true, false), (false, true)] {
            let refused = unknown || drift;
            let directory = tempfile::tempdir().unwrap();
            let workspace = directory.path().join("workspace");
            let home = directory.path().join("home");
            fs::create_dir(&workspace).unwrap();
            fs::create_dir(&home).unwrap();
            let runtime = directory.path().join("runtime.mjs");
            fs::write(
                &runtime,
                include_str!("../../../tests/fixtures/dsh-hi-probe.mjs"),
            )
            .unwrap();
            if unknown {
                fs::write(home.join("unknown-tool"), "").unwrap();
            }
            if drift {
                fs::write(home.join("policy-drift"), "").unwrap();
            }
            let scope = ProbeScope {
                workspace: Some(workspace.to_string_lossy().into_owned()),
                home: Some(home.to_string_lossy().into_owned()),
                profile: None,
                version: None,
            };
            let (auth, hi) = probe_dsh_hi(Some(&runtime), &scope, None, 0);
            assert_eq!(auth.state, EvidenceState::Unknown);
            assert_eq!(
                hi.state,
                if refused {
                    EvidenceState::Unavailable
                } else {
                    EvidenceState::Ready
                }
            );
            let events: Vec<Value> = fs::read_to_string(home.join("probe.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(events[0]["kind"], "dump");
            let canonical_workspace = fs::canonicalize(&workspace)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let canonical_home = fs::canonicalize(&home)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            for event in events.iter().filter(|event| event.get("kind").is_some()) {
                assert_eq!(
                    fs::canonicalize(event["cwd"].as_str().unwrap())
                        .unwrap()
                        .to_string_lossy(),
                    canonical_workspace
                );
                assert_eq!(
                    fs::canonicalize(event["home"].as_str().unwrap())
                        .unwrap()
                        .to_string_lossy(),
                    canonical_home
                );
                assert_eq!(event["mode"], "read-only");
                assert!(!Path::new(event["patch"].as_str().unwrap()).exists());
            }
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["method"] == "session/prompt")
                    .count(),
                usize::from(!refused)
            );
            assert_eq!(fs::read_dir(workspace).unwrap().count(), 0);
            if refused {
                assert_eq!(hi.reason.as_deref(), Some("strict_preflight_failed"));
                assert_eq!(events.len(), 1, "ACP must not start after refused dump");
            }
        }
    }

    #[test]
    fn dsh_catalog_uses_bounded_acp_sequence_and_keeps_tokens_opaque() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("fake-dsh");
        let log = directory.path().join("requests.jsonl");
        let opaque = "provider://future:model@2027?variant=a/b+c";
        let source = r#"#!/usr/bin/env node
import fs from 'node:fs';
if (process.argv.includes('--version')) { process.stdout.write('dsh-fixture-1.2.3\n'); process.exit(0); }
const log = __LOG__;
let buffer = '';
const write = (value) => process.stdout.write(`${JSON.stringify(value)}\n`);
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  buffer += chunk;
  for (;;) {
    const newline = buffer.indexOf('\n');
    if (newline < 0) break;
    const line = buffer.slice(0, newline); buffer = buffer.slice(newline + 1);
    if (!line) continue;
    const request = JSON.parse(line);
    fs.appendFileSync(log, JSON.stringify(request) + '\n');
    if (request.method === 'initialize') write({ jsonrpc:'2.0', id:request.id, result:{ protocolVersion:1, capabilities:{ models:true } } });
    else if (request.method === 'session/new') write({ jsonrpc:'2.0', id:request.id, result:{ sessionId:'catalog-session' } });
    else if (request.method === 'models/list') write({ jsonrpc:'2.0', id:request.id, result:{ models:[{ id:__OPAQUE__, name:'Future Model' }, { id:__OPAQUE__, name:'duplicate' }] } });
  }
});
"#
        .replace("__LOG__", &serde_json::to_string(&log).unwrap())
        .replace("__OPAQUE__", &serde_json::to_string(opaque).unwrap());
        fs::write(&runtime, source).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();

        let output = probe_dsh_models(
            Some(&runtime),
            &AgentModelsInput {
                agent: "dsh".into(),
                scope: ProbeScope::default(),
            },
        );
        assert!(output.supported);
        assert_eq!(output.models, vec![opaque]);
        assert_eq!(output.evidence.source, "dsh_acp_models_list");
        assert_eq!(
            output.evidence.version.as_deref(),
            Some("dsh-fixture-1.2.3")
        );
        let workspace = output.evidence.scope.workspace.as_deref().unwrap();
        assert!(workspace.contains("external-subagent-dsh-catalog-"));
        assert!(!Path::new(workspace).exists());
        let requests = fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            requests
                .iter()
                .map(|request| request["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["initialize", "session/new", "models/list"]
        );
        assert!(requests.iter().all(|request| request["jsonrpc"] == "2.0"));
    }

    #[test]
    fn dsh_catalog_fails_closed_when_catalog_sources_lack_model_tokens() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("fake-dsh-empty-catalog");
        let log = directory.path().join("requests.jsonl");
        let source = r#"#!/usr/bin/env node
import fs from 'node:fs';
if (process.argv.includes('--version')) { process.stdout.write('dsh-fixture-1.2.3\n'); process.exit(0); }
const log = __LOG__;
let buffer = '';
const write = (value) => process.stdout.write(`${JSON.stringify(value)}\n`);
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  buffer += chunk;
  for (;;) {
    const newline = buffer.indexOf('\n');
    if (newline < 0) break;
    const line = buffer.slice(0, newline); buffer = buffer.slice(newline + 1);
    if (!line) continue;
    const request = JSON.parse(line);
    fs.appendFileSync(log, JSON.stringify(request) + '\n');
    if (request.method === 'initialize') write({ jsonrpc:'2.0', id:request.id, result:{ protocolVersion:1, capabilities:{ models:true } } });
    else if (request.method === 'session/new') write({ jsonrpc:'2.0', id:request.id, result:{ sessionId:'empty-catalog-session' } });
    else if (request.method === 'models/list') write({ jsonrpc:'2.0', id:request.id, result:{ models: [] } });
  }
});
"#
        .replace("__LOG__", &serde_json::to_string(&log).unwrap());
        fs::write(&runtime, source).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();

        let scope = ProbeScope {
            workspace: Some(directory.path().to_string_lossy().into_owned()),
            home: Some(directory.path().to_string_lossy().into_owned()),
            profile: None,
            version: None,
        };
        // Counterexample: models/list succeeds with zero tokens and session/new
        // exposes none, so the catalog must fail closed, not return an empty Ok.
        assert_eq!(
            run_dsh_catalog(&runtime, &scope, DSH_CATALOG_TIMEOUT),
            Err("model_catalog_empty".to_owned())
        );
        let output = probe_dsh_models(
            Some(&runtime),
            &AgentModelsInput {
                agent: "dsh".into(),
                scope,
            },
        );
        assert!(!output.supported);
        assert!(output.models.is_empty());
        assert_eq!(output.reason.as_deref(), Some("model_catalog_empty"));
        let requests = fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            requests
                .iter()
                .map(|request| request["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["initialize", "session/new", "models/list", "initialize", "session/new", "models/list"]
        );
    }

    #[test]
    fn zcode_catalog_is_explicitly_native_only_without_runtime_start() {
        let backend = ProcessProbeBackend {
            runtime_source: Some(PathBuf::from("/must-not-run")),
        };
        let output = backend.models(&AgentModelsInput {
            agent: "zcode".into(),
            scope: ProbeScope::default(),
        });
        assert!(!output.supported);
        assert!(output.models.is_empty());
        assert_eq!(output.reason.as_deref(), Some("native_only"));
        assert_eq!(output.evidence.source, "zcode_native_model");
    }

    fn wait_process_gone(pid: i32, deadline: Instant) -> bool {
        loop {
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn dsh_catalog_reaps_descendant_that_inherits_stderr() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("dsh-descendant-fixture");
        let descendant_pid = directory.path().join("descendant.pid");
        let source = r#"#!/usr/bin/env node
import fs from 'node:fs';
import { spawn } from 'node:child_process';
if (process.argv.includes('--version')) { process.stdout.write('dsh-fixture-1\n'); process.exit(0); }
const descendant = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio:['ignore','ignore','inherit'] });
fs.writeFileSync(__PID__, String(descendant.pid));
let buffer = '';
const write = (value) => process.stdout.write(`${JSON.stringify(value)}\n`);
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  buffer += chunk;
  for (;;) {
    const newline = buffer.indexOf('\n'); if (newline < 0) break;
    const line = buffer.slice(0, newline); buffer = buffer.slice(newline + 1); if (!line) continue;
    const request = JSON.parse(line);
    if (request.method === 'initialize') write({jsonrpc:'2.0',id:request.id,result:{protocolVersion:1}});
    else if (request.method === 'session/new') write({jsonrpc:'2.0',id:request.id,result:{sessionId:'s'}});
    else if (request.method === 'models/list') write({jsonrpc:'2.0',id:request.id,result:{models:[{id:'opaque'}]}});
  }
});
"#
        .replace("__PID__", &serde_json::to_string(&descendant_pid).unwrap());
        fs::write(&runtime, source).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let started = Instant::now();
        let output = probe_dsh_models(
            Some(&runtime),
            &AgentModelsInput {
                agent: "dsh".into(),
                scope: ProbeScope::default(),
            },
        );
        assert!(output.supported);
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid = fs::read_to_string(descendant_pid)
            .unwrap()
            .parse::<i32>()
            .unwrap();
        assert!(wait_process_gone(
            pid,
            Instant::now() + Duration::from_secs(1)
        ));
    }

    #[test]
    fn dsh_catalog_rejects_unterminated_oversized_frame_and_reaps_process() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("dsh-oversized-fixture");
        let runtime_pid = directory.path().join("runtime.pid");
        let source = r#"#!/usr/bin/env node
import fs from 'node:fs';
if (process.argv.includes('--version')) { process.stdout.write('dsh-fixture-1\n'); process.exit(0); }
fs.writeFileSync(__PID__, String(process.pid));
process.stdout.write('x'.repeat(1024 * 1024 + 1));
setInterval(() => {}, 1000);
"#
        .replace("__PID__", &serde_json::to_string(&runtime_pid).unwrap());
        fs::write(&runtime, source).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let started = Instant::now();
        let output = probe_dsh_models(
            Some(&runtime),
            &AgentModelsInput {
                agent: "dsh".into(),
                scope: ProbeScope::default(),
            },
        );
        assert!(!output.supported);
        assert_eq!(output.reason.as_deref(), Some("oversized"));
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid = fs::read_to_string(runtime_pid)
            .unwrap()
            .parse::<i32>()
            .unwrap();
        assert!(wait_process_gone(
            pid,
            Instant::now() + Duration::from_secs(1)
        ));
    }
}
