//! Managed launch profile for the DSH ACP adapter.
//!
//! The single launch rule set is shared by the S03 discovery/catalog probe and
//! the adapter spawn: an explicit executable (resolved by the caller, normally
//! `DSH_RUNTIME_PATH`), the Node wrapper for JavaScript entrypoints, the task
//! workspace as cwd, and an explicit home override only when the caller
//! supplied one. The profile never installs a provider, never writes user
//! configuration, and never invents a provider command: a missing executable
//! fails closed instead of searching the PATH.

use serde_json::Value;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Managed build composition declared in `profiles/dsh/build.json`.
///
/// The repository file is the declared source; the embedded default must stay
/// byte-equivalent (a drift test pins this) so a missing file never widens the
/// composition at runtime.
pub const BUILD_PROFILE_JSON: &str = include_str!("../../../profiles/dsh/build.json");
pub const STRICT_PLAN_PATCH_YAML: &str =
    include_str!("../../../profiles/dsh/strict-plan.patch.yml");
pub const STRICT_PLAN_PROFILE_JSON: &str = include_str!("../../../profiles/dsh/strict-plan.json");

/// Launch inputs for one DSH child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DshLaunch {
    /// Explicit executable path. `None` means the provider is unavailable and
    /// the caller must fail closed instead of searching the PATH.
    pub executable: Option<PathBuf>,
    /// Absolute workspace directory used as the child cwd and the ACP
    /// `session/new` cwd.
    pub workspace: PathBuf,
    /// Explicit provider home override. Absence inherits the environment.
    pub home: Option<PathBuf>,
    pub permission_mode: Option<String>,
    pub patch: Option<PathBuf>,
}

impl DshLaunch {
    pub fn new(
        executable: Option<PathBuf>,
        workspace: impl Into<PathBuf>,
        home: Option<PathBuf>,
    ) -> Self {
        Self {
            executable,
            workspace: workspace.into(),
            home,
            permission_mode: None,
            patch: None,
        }
    }
}

/// Resolve the managed launch composition into a process command.
///
/// Mirrors the S03 catalog probe launch exactly (Node wrapper for `.js`,
/// `.cjs`, `.mjs` entrypoints) so probe evidence cannot diverge from the
/// adapter path.
pub fn resolve_launch(launch: &DshLaunch) -> io::Result<Command> {
    let executable = launch.executable.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "DSH_RUNTIME_PATH is unavailable; refusing to search the PATH",
        )
    })?;
    if !executable.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "DSH runtime path must be absolute",
        ));
    }
    if !executable.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "DSH runtime path is not a regular file",
        ));
    }
    if !launch.workspace.is_absolute() || !launch.workspace.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "DSH workspace must be an absolute directory",
        ));
    }
    let mut command = if matches!(
        executable.extension().and_then(|value| value.to_str()),
        Some("js" | "cjs" | "mjs")
    ) {
        let mut command = Command::new("node");
        command.arg(executable);
        command
    } else {
        Command::new(executable)
    };
    command.arg("--profile").arg("acp");
    if let Some(patch) = launch.patch.as_deref() {
        if !patch.is_absolute() || !patch.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "DSH patch must be an absolute regular file",
            ));
        }
        command.arg("--patch").arg(patch);
    }
    command.current_dir(&launch.workspace);
    if let Some(mode) = launch.permission_mode.as_deref() {
        command.env("DSH_PERMISSION_MODE", mode);
    }
    if let Some(home) = launch.home.as_deref() {
        if !home.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "DSH home override must be absolute",
            ));
        }
        command.env("DSH_HOME", home);
    }
    Ok(command)
}

/// Parse DSH's tagged YAML dump structurally. DSH emits `!!js` scalar tags;
/// these are deliberately erased before parsing because their payload remains
/// ordinary YAML data and executable tags must never be evaluated.
pub fn validate_dump_config_yaml(input: &str) -> Result<serde_yaml::Value, String> {
    let normalized = input.replace("!!js", "");
    let value: serde_yaml::Value = serde_yaml::from_str(&normalized)
        .map_err(|e| format!("invalid dsh dump-config YAML: {e}"))?;
    if !value.is_sequence() && !value.is_mapping() {
        return Err("dump-config root must be a structured sequence or mapping".into());
    }
    Ok(value)
}

pub const PINNED_DSH_VERSION: &str = "0.1.5-rc.1";

pub fn validate_dsh_version(output: &str) -> Result<(), String> {
    let found = output
        .split_whitespace()
        .find(|s| s.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .ok_or("dsh --version returned no version")?;
    if found != PINNED_DSH_VERSION {
        return Err(format!(
            "unsupported dsh version {found}; expected {PINNED_DSH_VERSION}"
        ));
    }
    Ok(())
}

/// Collect both pipes concurrently, retaining at most `cap` bytes per pipe.
/// The deadline covers pipe EOF as well as process exit; failed probes are
/// always killed and waited, including output-limit and pipe-read failures.
#[cfg(unix)]
fn bounded_output(
    command: &mut Command,
    timeout: Duration,
    cap: usize,
) -> Result<std::process::Output, String> {
    use std::os::unix::process::CommandExt;
    let deadline = Instant::now() + timeout;
    command.process_group(0);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("dsh probe spawn failed: {e}"))?;
    collect_probe(&mut child, deadline, cap)
}

#[cfg(not(unix))]
fn bounded_output(_: &mut Command, _: Duration, _: usize) -> Result<std::process::Output, String> {
    Err("DSH preflight requires supported process-group isolation".into())
}

#[cfg(unix)]
fn collect_probe(
    child: &mut std::process::Child,
    deadline: Instant,
    cap: usize,
) -> Result<std::process::Output, String> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    };
    trait Pipe: Read + AsRawFd + Send {}
    impl<T: Read + AsRawFd + Send> Pipe for T {}
    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    let (sender, receiver) = mpsc::channel();
    let pipes: [(bool, Box<dyn Pipe>); 2] = [
        (false, Box::new(child.stdout.take().expect("piped stdout"))),
        (true, Box::new(child.stderr.take().expect("piped stderr"))),
    ];
    let result = (|| {
        for (stderr, mut pipe) in pipes {
            let sender = sender.clone();
            let stop = stop.clone();
            let fd = pipe.as_raw_fd();
            // Nonblocking reads allow every reader to terminate and join even
            // if an unexpected descendant keeps a pipe open past the deadline.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(format!(
                    "dsh probe pipe setup failed: {}",
                    io::Error::last_os_error()
                ));
            }
            readers.push(
                std::thread::Builder::new()
                    .name("dsh-probe-output".into())
                    .spawn(move || {
                        let result = (|| {
                            let mut bytes = Vec::new();
                            let mut chunk = [0; 8192];
                            loop {
                                if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                                    return Err("dsh probe timed out".into());
                                }
                                let count = match pipe.read(&mut chunk) {
                                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                        std::thread::sleep(Duration::from_millis(5));
                                        continue;
                                    }
                                    other => other
                                        .map_err(|e| format!("dsh probe pipe read failed: {e}"))?,
                                };
                                if count == 0 {
                                    return Ok(bytes);
                                }
                                if count > cap.saturating_sub(bytes.len()) {
                                    return Err(format!(
                                        "dsh probe {} exceeded {cap} byte cap",
                                        if stderr { "stderr" } else { "stdout" }
                                    ));
                                }
                                bytes.extend_from_slice(&chunk[..count]);
                            }
                        })();
                        let _ = sender.send((stderr, result));
                    })
                    .map_err(|e| format!("dsh probe reader spawn failed: {e}"))?,
            );
        }
        drop(sender);
        let (mut stdout, mut stderr) = (None, None);
        loop {
            if Instant::now() >= deadline {
                return Err("dsh probe timed out".into());
            }
            while let Ok((is_stderr, bytes)) = receiver.try_recv() {
                if is_stderr {
                    stderr = Some(bytes?);
                } else {
                    stdout = Some(bytes?);
                }
            }
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                if stdout.is_some() && stderr.is_some() {
                    return Ok(std::process::Output {
                        status,
                        stdout: stdout.unwrap(),
                        stderr: stderr.unwrap(),
                    });
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    // This group was created specifically for this command, never inherited
    // from the daemon. SIGKILL also handles descendants that ignore SIGTERM.
    let pgid = child.id() as i32;
    let killed = unsafe { libc::kill(-pgid, libc::SIGKILL) };
    let kill_error =
        if killed != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            Some(format!(
                "dsh probe group kill failed: {}",
                io::Error::last_os_error()
            ))
        } else {
            None
        };
    let reaped = child
        .wait()
        .map_err(|e| format!("dsh probe reap failed: {e}"));
    stop.store(true, Ordering::Release);
    let mut join_failed = false;
    for reader in readers {
        join_failed |= reader.join().is_err();
    }
    reaped?;
    if let Some(error) = kill_error {
        return Err(error);
    }
    if join_failed {
        return Err("dsh probe reader panicked".into());
    }
    let reap_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match external_runtime::observe_process_group(pgid) {
            Ok(members) if members.is_empty() => break,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(format!("dsh probe group observation failed: {e}")),
        }
        if Instant::now() >= reap_deadline {
            return Err("dsh probe process group did not finish reaping".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    result
}

/// Verify the actual user-composed build profile with the exact launch cwd,
/// home and permission environment. Unsupported overrides fail before ACP starts.
/// Prompt serialization is enforced separately by the daemon runtime owner.
pub fn preflight_build(launch: &DshLaunch) -> Result<(), String> {
    build_profile()?;
    if launch.permission_mode.as_deref() != Some("workspace-write") {
        return Err("build launch must pin workspace-write".into());
    }
    let mut version_command = resolve_launch(launch).map_err(|e| e.to_string())?;
    version_command.arg("--version");
    let version = bounded_output(&mut version_command, Duration::from_secs(10), 4096)?;
    if !version.status.success() {
        return Err("dsh version probe failed".into());
    }
    validate_dsh_version(&String::from_utf8_lossy(&version.stdout))?;
    let mut command = resolve_launch(launch).map_err(|e| e.to_string())?;
    command.arg("--dump-config");
    let output = bounded_output(&mut command, Duration::from_secs(10), 1024 * 1024)?;
    if !output.status.success() {
        return Err("dsh build dump-config failed".into());
    }
    validate_build_dump(
        &validate_dump_config_yaml(&String::from_utf8_lossy(&output.stdout))?,
        &launch.workspace,
    )
}

fn validate_build_dump(
    value: &serde_yaml::Value,
    workspace: &std::path::Path,
) -> Result<(), String> {
    let entries = value
        .as_sequence()
        .ok_or("build dump must contain a plugin sequence")?;
    let entry = |id: &str, name: &str| -> Result<&serde_yaml::Value, String> {
        let matches: Vec<_> = entries
            .iter()
            .filter(|e| e["id"].as_str() == Some(id) || e["name"].as_str() == Some(name))
            .collect();
        if matches.len() != 1 {
            return Err(format!("build requires exactly one {id}"));
        }
        let e = matches[0];
        let disabled = match &e["disabled"] {
            serde_yaml::Value::Null => Some(false),
            serde_yaml::Value::Bool(value) => Some(*value),
            serde_yaml::Value::String(value)
                if id == "bash-sandbox" && value == "process.platform === 'win32'" =>
            {
                Some(cfg!(windows))
            }
            serde_yaml::Value::String(value)
                if id == "pwsh-sandbox" && value == "process.platform !== 'win32'" =>
            {
                Some(!cfg!(windows))
            }
            _ => None,
        };
        let should_disable =
            (id == "pwsh-sandbox" && !cfg!(windows)) || (id == "bash-sandbox" && cfg!(windows));
        if e["id"].as_str() != Some(id)
            || e["name"].as_str() != Some(name)
            || disabled != Some(should_disable)
        {
            return Err(format!(
                "build requires provider-owned {id} with the pinned platform activation"
            ));
        }
        Ok(e)
    };
    let policy = entry("sandbox-policy", "@deepseek-ai/dsh-sandbox-policy")?;
    // These exact shipped expressions are safe because resolve_launch pins the
    // environment and cwd for both the dump and the ACP process. Never evaluate JS.
    if !matches!(
        policy["config"]["mode"].as_str(),
        Some("workspace-write" | "process.env.DSH_PERMISSION_MODE ?? 'workspace-write'")
    ) {
        return Err("build sandbox must resolve to workspace-write".into());
    }
    let root = policy["config"]["workspaceRoot"].as_str();
    if root != Some("process.cwd()") && root != workspace.to_str() {
        return Err("build sandbox root must match the task workspace".into());
    }
    let approval = entry("approval", "@deepseek-ai/dsh-user-approval")?;
    if !matches!(approval["config"]["policy"].as_str(), Some("ask" |
        "(process.env.DSH_PERMISSION_MODE ?? 'workspace-write') === 'danger-full-access' ? 'never' : 'ask'")) {
        return Err("build approval must resolve to ask".into());
    }
    // The installed sandbox-local supports runnerCommand, which bypasses
    // built-in runner selection/probing. Only its unconfigured default is trusted.
    for (id, name, timeout) in [
        ("sandbox", "@deepseek-ai/dsh-sandbox-local", false),
        ("bash-sandbox", "@deepseek-ai/dsh-bash-sandbox", true),
        ("pwsh-sandbox", "@deepseek-ai/dsh-pwsh-sandbox", false),
    ] {
        let e = entry(id, name)?;
        if e.as_mapping()
            .unwrap()
            .keys()
            .any(|key| !matches!(key.as_str(), Some("id" | "name" | "disabled" | "config")))
        {
            return Err(format!("build {id} contains unverified plugin overrides"));
        }
        let config = &e["config"];
        let valid = if config.is_null() {
            !timeout
        } else {
            config.as_mapping().is_some_and(|map| {
                if timeout {
                    map.len() == 1 && config["timeoutMs"].as_u64() == Some(60000)
                } else {
                    map.is_empty()
                }
            })
        };
        if !valid {
            return Err(format!(
                "build {id} contains unverified executor configuration"
            ));
        }
    }
    let permission = entry("permission", "@deepseek-ai/dsh-permission-presets")?;
    let build = &permission["config"]["presets"]["workspace-write"];
    if build["sandbox"].as_str() != Some("workspace-write")
        || build["approval"].as_str() != Some("ask")
    {
        return Err("build permission preset must preserve workspace-write and ask".into());
    }
    if let Some(default) = permission["config"]["defaultPreset"].as_str() {
        if default != "workspace-write" {
            return Err("build permission defaultPreset must be workspace-write".into());
        }
    }
    if let Some(settings) = permission["config"].get("settings") {
        if !settings.is_null() && !settings.as_mapping().is_some_and(|m| m.is_empty()) {
            return Err("build permission settings overrides are not managed".into());
        }
    }
    entry("fs-sandbox", "@deepseek-ai/dsh-fs-sandbox")?;
    entry("acp", "@deepseek-ai/dsh-acp")?;
    entry("acp-app-startup", "@deepseek-ai/dsh-acp-app")?;
    Ok(())
}

/// Run the provider-owned preflight independently from the ACP command.  The
/// dump is treated as untrusted data and is accepted only when every enabled
/// entry is in the managed allowlist and the policy controls are present.
pub fn preflight(launch: &DshLaunch) -> Result<(), String> {
    strict_plan_profile()?;
    if launch.permission_mode.as_deref() != Some("read-only") || launch.patch.is_none() {
        return Err("strict launch must pin read-only and a managed patch".into());
    }
    let mut version_command = resolve_launch(launch).map_err(|e| e.to_string())?;
    version_command.arg("--version");
    let version = bounded_output(&mut version_command, Duration::from_secs(10), 4096)?;
    if !version.status.success() {
        return Err("dsh version probe failed".into());
    }
    validate_dsh_version(&String::from_utf8_lossy(&version.stdout))?;
    let mut command = resolve_launch(launch).map_err(|e| e.to_string())?;
    command.arg("--dump-config");
    let out = bounded_output(&mut command, Duration::from_secs(10), 1024 * 1024)?;
    if !out.status.success() {
        return Err(format!(
            "dsh dump-config exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let value = validate_dump_config_yaml(&String::from_utf8_lossy(&out.stdout))?;
    validate_dump_policy(&value)
}

fn validate_dump_policy(value: &serde_yaml::Value) -> Result<(), String> {
    const DANGEROUS: &[&str] = &[
        "tool-bash",
        "tool-pwsh",
        "tool-jobs",
        "tool-fs",
        "tool-skill",
        "tool-subagent",
        "tool-subagent-fork",
        "tool-subagent-control",
        "tool-subagent-list-agents",
        "subagent",
        "subagent-spawn-in-process",
        "subagent-fork-in-process",
        "tool-workflow",
        "tool-goal",
        "tool-ralph",
        "skill-filesystem",
        "workflow-worker-thread",
        "goal-round-driver",
    ];
    const ALLOWED_ENABLED: &[&str] = &[
        "timer",
        "llm",
        "deepseek-llm-api-extensions",
        "session",
        "session-log-deepseek",
        "typert",
        "typert-loader",
        "typert-gateway",
        "session-title",
        "session-title-llm",
        "user-questions",
        "agent",
        "plugin-package-inventory-deepseek",
        "agent-default-model",
        "jobs",
        "llm-retry",
        "settings",
        "credentials",
        "llm-pi-ai",
        "session-persistence-jsonl",
        "attachment-local",
        "session-query-sqlite",
        "session-projection",
        "storage",
        "storage-json",
        "storage-domain",
        "session-projection-cache",
        "session-telemetry-otel",
        "permission",
        "shell-env",
        "fs-observation-policy",
        "agent-instructions",
        "skill",
        "commands",
        "command-feedback",
        "goal",
        "command-goal",
        "plan-mode",
        "token-meter",
        "compaction-basic",
        "command-compact",
        "timeout-policy",
        "spill-local",
        "spill-policy",
        "session-checkpoint-policy",
        "tool-result-pruner",
        "tool-todo",
        "repeat-tool-reminder",
        "web",
        "web-search-deepseek",
        "web-fetch-http",
        "tool-web",
        "tools",
        "system-prompt",
        "agent-loop",
        "fs-sandbox",
        "llm-deepseek",
        "acp-app-startup",
        "acp",
        "subprocess",
        "sandbox",
        "bash-sandbox",
        "pwsh-sandbox",
        "sandbox-policy",
        "observe",
        "tool-observe",
        "tool-read",
        "tool-fs-search",
        "tool-glob",
        "tool-grep",
        "tool-cordis",
        "telemetry",
        "logging",
    ];
    let mut saw_policy = false;
    fn walk(v: &serde_yaml::Value, saw: &mut bool) -> Result<(), String> {
        match v {
            serde_yaml::Value::Mapping(m) => {
                let id = m
                    .get(serde_yaml::Value::String("id".into()))
                    .and_then(|v| v.as_str());
                let disabled = m
                    .get(serde_yaml::Value::String("disabled".into()))
                    .and_then(|v| v.as_bool());
                if matches!(id, Some("sandbox-policy" | "approval")) {
                    *saw = true;
                }
                if let Some(id) = id {
                    if DANGEROUS.contains(&id) && disabled != Some(true) {
                        return Err(format!("dangerous DSH entry is enabled: {id:?}"));
                    }
                    if disabled != Some(true)
                        && !ALLOWED_ENABLED.contains(&id)
                        && !matches!(id, "sandbox-policy" | "approval")
                    {
                        return Err(format!("unknown enabled DSH entry: {id:?}"));
                    }
                    if matches!(id, "sandbox-policy" | "approval") {
                        let config = m.get(serde_yaml::Value::String("config".into()));
                        let mode = config.and_then(|v| v.get("mode")).and_then(|v| v.as_str());
                        if id == "sandbox-policy" && config.is_some() && mode != Some("read-only") {
                            return Err("sandbox-policy must be read-only".into());
                        }
                    }
                }
                for key in ["runnerCommand", "executor", "executors"] {
                    if m.contains_key(serde_yaml::Value::String(key.into())) {
                        return Err(format!("strict DSH dump contains unmanaged {key}"));
                    }
                }
                if id == Some("settings") && disabled != Some(true) {
                    if let Some(config) = m.get(serde_yaml::Value::String("config".into())) {
                        if !config
                            .as_mapping()
                            .is_some_and(serde_yaml::Mapping::is_empty)
                        {
                            return Err("strict DSH settings drift is not allowed".into());
                        }
                    }
                }
                for val in m.values() {
                    walk(val, saw)?;
                }
            }
            serde_yaml::Value::Sequence(s) => {
                for x in s {
                    walk(x, saw)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    walk(value, &mut saw_policy)?;
    if !saw_policy {
        return Err("dump-config missing sandbox-policy/approval controls".into());
    }
    Ok(())
}

/// Parse and validate the managed build composition. Unknown fields or a
/// non-`workspace-write` sandbox are rejected: the build composition is the
/// only DSH permission surface S04.A is allowed to drive.
pub fn build_profile() -> Result<Value, String> {
    let profile: Value = serde_json::from_str(BUILD_PROFILE_JSON)
        .map_err(|error| format!("managed dsh build profile is invalid: {error}"))?;
    validate_build_profile(&profile)?;
    Ok(profile)
}

pub fn validate_build_profile(profile: &Value) -> Result<(), String> {
    let Some(obj) = profile.as_object() else {
        return Err("managed dsh build profile must be an object".into());
    };
    if obj
        .keys()
        .any(|k| !matches!(k.as_str(), "agent" | "composition" | "notes"))
    {
        return Err("managed dsh build profile contains unknown fields".into());
    }
    if profile.get("agent").and_then(Value::as_str) != Some(crate::DSH_AGENT_NAME) {
        return Err("managed dsh build profile must declare agent=dsh".into());
    }
    let composition = profile
        .get("composition")
        .ok_or_else(|| "managed dsh build profile is missing the composition object".to_string())?;
    let Some(cobj) = composition.as_object() else {
        return Err("managed dsh composition must be an object".into());
    };
    if cobj.keys().any(|k| {
        !matches!(
            k.as_str(),
            "sandbox" | "respondable_permissions" | "prompt_scope"
        )
    }) {
        return Err("managed dsh composition contains unknown fields".into());
    }
    if composition.get("sandbox").and_then(Value::as_str) != Some("workspace-write") {
        return Err("managed dsh build profile must pin sandbox=workspace-write".into());
    }
    if composition
        .get("respondable_permissions")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err("managed dsh build profile must pin respondable_permissions=true".into());
    }
    if composition.get("prompt_scope").and_then(Value::as_str) != Some("one-in-flight") {
        return Err("managed dsh build profile must pin prompt_scope=one-in-flight".into());
    }
    Ok(())
}

pub fn strict_plan_profile() -> Result<Value, String> {
    let profile: Value = serde_json::from_str(STRICT_PLAN_PROFILE_JSON)
        .map_err(|error| format!("managed dsh strict profile is invalid: {error}"))?;
    validate_strict_plan_profile(&profile)?;
    Ok(profile)
}

pub fn validate_strict_plan_profile(profile: &Value) -> Result<(), String> {
    if profile.get("agent").and_then(Value::as_str) != Some(crate::DSH_AGENT_NAME) {
        return Err("strict dsh profile must declare agent=dsh".into());
    }
    let c = profile
        .get("composition")
        .ok_or("strict dsh profile missing composition")?;
    if c.get("sandbox").and_then(Value::as_str) != Some("read-only")
        || c.get("permission_mode").and_then(Value::as_str) != Some("read-only")
        || c.get("unknown_entry_policy").and_then(Value::as_str) != Some("fail-closed")
        || c.get("write_manifest").and_then(Value::as_array).is_none()
    {
        return Err("strict dsh profile must pin read-only fail-closed composition".into());
    }
    if c["write_manifest"]
        .as_array()
        .is_some_and(|v| !v.is_empty())
    {
        return Err("strict dsh profile cannot declare write_manifest entries".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace() -> PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn missing_executable_fails_closed_without_path_search() {
        let error = resolve_launch(&DshLaunch::new(None, temp_workspace(), None)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("refusing to search the PATH"));
    }

    #[test]
    fn non_absolute_or_missing_runtime_is_rejected() {
        let error = resolve_launch(&DshLaunch::new(
            Some(PathBuf::from("dsh")),
            temp_workspace(),
            None,
        ))
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let error = resolve_launch(&DshLaunch::new(
            Some(PathBuf::from("/definitely/not/here")),
            temp_workspace(),
            None,
        ))
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn js_entrypoints_wrap_node_and_set_cwd() {
        let script = std::env::temp_dir().join("dsh-profile-fixture-check.mjs");
        std::fs::write(&script, b"").unwrap();
        let command = resolve_launch(&DshLaunch::new(
            Some(script.clone()),
            temp_workspace(),
            None,
        ))
        .unwrap();
        assert_eq!(command.get_program().to_string_lossy(), "node");
        assert!(command.get_args().any(|arg| arg == script.as_os_str()));
        assert_eq!(command.get_current_dir(), Some(temp_workspace().as_path()));
        std::fs::remove_file(script).unwrap();
    }

    #[test]
    fn home_override_is_forwarded_only_when_explicit_and_absolute() {
        let absolute = PathBuf::from("/usr/bin/true");
        assert!(absolute.is_file(), "platform true binary is required");
        let command = resolve_launch(&DshLaunch::new(
            Some(absolute.clone()),
            temp_workspace(),
            Some(PathBuf::from("/fixture/home")),
        ))
        .unwrap();
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == "DSH_HOME")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("/fixture/home"))
        );
        let command =
            resolve_launch(&DshLaunch::new(Some(absolute), temp_workspace(), None)).unwrap();
        assert!(command.get_envs().all(|(key, _)| key != "DSH_HOME"));
    }

    #[test]
    fn build_profile_pins_the_managed_composition() {
        let profile = build_profile().unwrap();
        assert_eq!(profile["agent"], "dsh");
        assert_eq!(profile["composition"]["sandbox"], "workspace-write");
    }

    #[test]
    fn build_dump_rejects_user_policy_overrides() {
        let baseline = serde_json::json!([
            {"id":"sandbox-policy", "name":"@deepseek-ai/dsh-sandbox-policy", "config":{
                "mode":"process.env.DSH_PERMISSION_MODE ?? 'workspace-write'", "workspaceRoot":"process.cwd()"}},
            {"id":"approval", "name":"@deepseek-ai/dsh-user-approval", "config":{
                "policy":"(process.env.DSH_PERMISSION_MODE ?? 'workspace-write') === 'danger-full-access' ? 'never' : 'ask'"}},
            {"id":"permission", "name":"@deepseek-ai/dsh-permission-presets", "config":{"presets":{
                "workspace-write":{"sandbox":"workspace-write", "approval":"ask"}}}},
            {"id":"sandbox", "name":"@deepseek-ai/dsh-sandbox-local"},
            {"id":"fs-sandbox", "name":"@deepseek-ai/dsh-fs-sandbox"},
            {"id":"acp", "name":"@deepseek-ai/dsh-acp"},
            {"id":"acp-app-startup", "name":"@deepseek-ai/dsh-acp-app"},
            {"id":"bash-sandbox", "name":"@deepseek-ai/dsh-bash-sandbox", "disabled":"process.platform === 'win32'", "config":{"timeoutMs":60000}},
            {"id":"pwsh-sandbox", "name":"@deepseek-ai/dsh-pwsh-sandbox", "disabled":"process.platform !== 'win32'"}
        ]);
        let validate = |v: &Value| {
            validate_build_dump(
                &serde_yaml::to_value(v).unwrap(),
                std::path::Path::new("/workspace"),
            )
        };
        validate(&baseline).unwrap();
        for (key, value) in [
            ("defaultPreset", Value::String("danger-full-access".into())),
            (
                "settings",
                serde_json::json!({"preset":"danger-full-access"}),
            ),
        ] {
            let mut drifted = baseline.clone();
            drifted[2]["config"][key] = value;
            assert!(validate(&drifted).is_err(), "permission {key}");
        }
        for (index, pointer, replacement) in [
            (
                0,
                "/config/mode",
                Value::String("danger-full-access".into()),
            ),
            (0, "/config/workspaceRoot", Value::String("/".into())),
            (1, "/config/policy", Value::String("never".into())),
            (
                2,
                "/config/presets/workspace-write/approval",
                Value::String("never".into()),
            ),
            (5, "/name", Value::String("unmanaged-acp".into())),
        ] {
            let mut drifted = baseline.clone();
            *drifted[index].pointer_mut(pointer).unwrap() = replacement;
            assert!(validate(&drifted).is_err(), "{index}{pointer}");
        }
        for index in 0..7 {
            let mut drifted = baseline.clone();
            drifted[index]["disabled"] = Value::Bool(true);
            assert!(validate(&drifted).is_err(), "disabled {index}");
            let mut missing = baseline.clone();
            missing.as_array_mut().unwrap().remove(index);
            assert!(validate(&missing).is_err(), "missing {index}");
        }
        for (index, key, value) in [
            (
                3,
                "config",
                serde_json::json!({"runnerCommand":["/bin/sh"],"runnerFailureSignatures":["denied"]}),
            ),
            (3, "config", serde_json::json!({"probeTimeoutMs":0})),
            (
                7,
                "config",
                serde_json::json!({"timeoutMs":60000,"shell":"/bin/sh"}),
            ),
            (7, "name", serde_json::json!("@deepseek-ai/dsh-bash-local")),
            (7, "disabled", serde_json::json!(true)),
            (8, "disabled", serde_json::json!(false)),
            (8, "name", serde_json::json!("@deepseek-ai/dsh-pwsh-local")),
            (3, "inject", serde_json::json!(["unmanaged"])),
        ] {
            let mut drifted = baseline.clone();
            drifted[index][key] = value;
            assert!(validate(&drifted).is_err(), "{index}.{key}");
        }
        for index in [3, 7, 8] {
            let mut duplicate = baseline.clone();
            duplicate
                .as_array_mut()
                .unwrap()
                .push(baseline[index].clone());
            assert!(validate(&duplicate).is_err());
            let mut missing = baseline.clone();
            missing.as_array_mut().unwrap().remove(index);
            assert!(validate(&missing).is_err());
        }
        let mut duplicate = baseline.clone();
        duplicate.as_array_mut().unwrap().push(baseline[0].clone());
        assert!(validate(&duplicate).is_err());
    }

    #[test]
    fn probe_drains_both_large_pipes_without_deadlock() {
        let output = bounded_output(
            Command::new("python3").arg("-c").arg(
                "import sys; sys.stdout.buffer.write(b'o'*131072); sys.stderr.buffer.write(b'e'*131072)"
            ), Duration::from_secs(5), 131072,
        ).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, vec![b'o'; 131072]);
        assert_eq!(output.stderr, vec![b'e'; 131072]);
    }

    #[test]
    fn probe_caps_each_pipe_and_reaps_the_child() {
        use std::os::unix::process::CommandExt;
        for stream in ["stdout", "stderr"] {
            let script = format!("import sys,time; sys.{stream}.buffer.write(b'x'*131073); sys.{stream}.flush(); time.sleep(60)");
            let mut child = Command::new("python3")
                .process_group(0)
                .arg("-c")
                .arg(script)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let started = Instant::now();
            let error =
                collect_probe(&mut child, started + Duration::from_secs(5), 131072).unwrap_err();
            assert!(error.contains(&format!("{stream} exceeded")), "{error}");
            assert!(started.elapsed() < Duration::from_secs(5));
            assert!(child.try_wait().unwrap().is_some());
        }
    }

    #[test]
    fn hanging_version_and_open_pipe_probes_time_out_and_reap() {
        use std::os::unix::process::CommandExt;
        for script in [
            "import time; time.sleep(60)",
            "import os,time; os.close(1); os.close(2); time.sleep(60)",
        ] {
            let mut child = Command::new("python3")
                .process_group(0)
                .arg("-c")
                .arg(script)
                .arg("--version")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let started = Instant::now();
            let error =
                collect_probe(&mut child, started + Duration::from_millis(150), 4096).unwrap_err();
            assert!(error.contains("timed out"), "{error}");
            assert!(started.elapsed() < Duration::from_secs(3));
            assert!(child.try_wait().unwrap().is_some());
        }
    }

    #[test]
    #[cfg(unix)]
    fn probe_reaps_term_resistant_descendants_with_inherited_pipes() {
        use std::os::unix::process::CommandExt;
        for overflow in [false, true] {
            // Parent exits only after its child installed SIGTERM ignore.
            // The descendant retains both pipes, with no filesystem fixtures.
            let script = format!(
                r#"
import os,signal,time
r,w=os.pipe()
if os.fork()==0:
    os.close(r)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    os.write(w,b'ready')
    os.close(w)
    {write}
    time.sleep(60)
else:
    os.close(w)
    os.read(r,5)
    os._exit(0)
"#,
                write = if overflow {
                    "os.write(2,b'x'*65536)"
                } else {
                    "pass"
                }
            );
            let mut child = Command::new("python3")
                .process_group(0)
                .arg("-c")
                .arg(script)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let pgid = child.id() as i32;
            let started = Instant::now();
            let error =
                collect_probe(&mut child, started + Duration::from_millis(500), 4096).unwrap_err();
            assert!(
                error.contains(if overflow {
                    "stderr exceeded"
                } else {
                    "timed out"
                }),
                "{error}"
            );
            assert!(started.elapsed() < Duration::from_secs(3));
            assert!(external_runtime::observe_process_group(pgid)
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    #[ignore = "requires explicit DSH_RUNTIME_PATH and installed pinned DSH"]
    fn live_build_preflight_uses_installed_composition() {
        let mut launch = DshLaunch::new(
            Some(PathBuf::from(std::env::var_os("DSH_RUNTIME_PATH").unwrap())),
            std::env::current_dir().unwrap(),
            std::env::var_os("DSH_HOME").map(PathBuf::from),
        );
        launch.permission_mode = Some("workspace-write".into());
        preflight_build(&launch).unwrap();
    }

    #[test]
    fn build_profile_rejects_drifted_compositions() {
        let baseline = build_profile().unwrap();
        let mut widened = baseline.clone();
        widened["composition"]["sandbox"] = Value::String("danger-full-access".into());
        assert!(validate_build_profile(&widened).is_err());
        let mut unresponsive = baseline.clone();
        unresponsive["composition"]["respondable_permissions"] = Value::Bool(false);
        assert!(validate_build_profile(&unresponsive).is_err());
        let mut pooled = baseline.clone();
        pooled["composition"]["prompt_scope"] = Value::String("concurrent".into());
        assert!(validate_build_profile(&pooled).is_err());
        let mut wrong_agent = baseline.clone();
        wrong_agent["agent"] = Value::String("zcode".into());
        assert!(validate_build_profile(&wrong_agent).is_err());
        let mut unknown = baseline.clone();
        unknown["composition"]["unmanaged"] = Value::Bool(true);
        assert!(validate_build_profile(&unknown).is_err());
        assert!(validate_build_profile(&Value::Null).is_err());
    }

    #[test]
    fn dump_policy_rejects_unknown_enabled_tool() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>("- id: sandbox-policy\n  disabled: true\n- id: approval\n  disabled: true\n- id: tool-unknown\n  disabled: false").unwrap();
        assert!(validate_dump_policy(&yaml)
            .unwrap_err()
            .contains("unknown enabled"));
    }

    #[test]
    fn dump_policy_rejects_unknown_enabled_entry() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            "- id: sandbox-policy\n  disabled: false\n- id: approval\n  disabled: true\n- id: unmanaged-entry\n  disabled: false",
        )
        .unwrap();
        assert!(validate_dump_policy(&yaml)
            .unwrap_err()
            .contains("unknown enabled"));
    }

    #[test]
    fn dump_policy_rejects_executor_and_settings_drift() {
        for drift in [
            "- id: sandbox-policy\n  disabled: false\n  config:\n    mode: read-only\n    runnerCommand: /bin/sh\n- id: approval\n  disabled: true",
            "- id: sandbox-policy\n  disabled: false\n  config:\n    mode: read-only\n- id: approval\n  disabled: true\n- id: settings\n  disabled: false\n  config:\n    permission_mode: workspace-write",
        ] {
            let yaml = serde_yaml::from_str::<serde_yaml::Value>(drift).unwrap();
            assert!(validate_dump_policy(&yaml).is_err());
        }
    }

    #[test]
    fn strict_plan_profile_is_read_only_and_fail_closed() {
        let profile = strict_plan_profile().unwrap();
        assert_eq!(profile["composition"]["sandbox"], "read-only");
        assert!(profile["composition"]["write_manifest"]
            .as_array()
            .unwrap()
            .is_empty());
        let mut widened = profile.clone();
        widened["composition"]["write_manifest"] = serde_json::json!(["src/**"]);
        assert!(validate_strict_plan_profile(&widened).is_err());
    }
}
