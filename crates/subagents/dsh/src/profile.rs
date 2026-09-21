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
pub const BUILD_PROFILE_JSON: &str = include_str!("../../../../profiles/dsh/build.json");
pub const STRICT_PLAN_PATCH_YAML: &str =
    include_str!("../../../../profiles/dsh/strict-plan.patch.yml");
pub const STRICT_PLAN_PROFILE_JSON: &str =
    include_str!("../../../../profiles/dsh/strict-plan.json");
pub const MANIFEST_BUILD_PROFILE_JSON: &str =
    include_str!("../../../../profiles/dsh/manifest-build.json");

/// Embedded `dsh-write-guard` package artifact. S01 checked in the plugin as
/// the artifact (no build step); embedding the exact bytes here keeps the
/// daemon materialization in S03 independent of the source tree at runtime.
pub const WRITE_GUARD_PACKAGE_JSON: &str =
    include_str!("../../../../plugins/dsh-write-guard/package.json");
pub const WRITE_GUARD_INDEX_JS: &str =
    include_str!("../../../../plugins/dsh-write-guard/lib/index.js");

/// Materialization manifest for the write-guard package: relative path inside
/// the plugin package directory paired with its byte-exact embedded content.
/// S03 writes `<TempDir>/dsh-write-guard/<relative path>` for each pair.
pub const WRITE_GUARD_FILES: &[(&str, &str)] = &[
    ("package.json", WRITE_GUARD_PACKAGE_JSON),
    ("lib/index.js", WRITE_GUARD_INDEX_JS),
];

// The embedded plugin artifact is the S03 materialization source; an empty
// snapshot would silently produce an unloadable plugin tree, so the non-empty
// invariant is enforced at compile time as well as at runtime by a unit test.
const _: () = assert!(!WRITE_GUARD_PACKAGE_JSON.is_empty());
const _: () = assert!(!WRITE_GUARD_INDEX_JS.is_empty());

/// Strict-plan disabled entry ids, copied verbatim from
/// `profiles/dsh/strict-plan.patch.yml` with `tool-fs` removed: manifest-build
/// keeps the filesystem tool enabled on top of a workspace-write sandbox and
/// relies on the embedded write-guard plugin for containment. A unit test pins
/// this list against the embedded strict patch so the two cannot drift.
pub const MANIFEST_BUILD_DISABLED_IDS: &[&str] = &[
    "tool-bash",
    "tool-pwsh",
    "tool-jobs",
    "tool-skill",
    "tool-subagent-control",
    "tool-subagent-list-agents",
    "tool-subagent",
    "tool-subagent-fork",
    "subagent",
    "tool-workflow",
    "tool-goal",
    "tool-ralph",
    "skill-filesystem",
    "workflow-worker-thread",
    "goal-round-driver",
    "subagent-spawn-in-process",
    "subagent-fork-in-process",
];

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
    pub profile: Option<String>,
    pub version: Option<String>,
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
            profile: None,
            version: None,
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
    let profile = launch.profile.as_deref().unwrap_or("acp");
    if profile != "acp" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported DSH profile",
        ));
    }
    command.arg("--profile").arg(profile);
    if let Some(version) = launch.version.as_deref() {
        if version != PINNED_DSH_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported DSH version",
            ));
        }
    }
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

fn validate_dump_policy(value: &serde_yaml::Value) -> Result<(), String> {
    // Both policy controls must appear exactly once, unambiguously enabled,
    // with pinned configs. A missing, duplicated, disabled, or drifted
    // control rejects the whole dump before any prompt is sent: the strict
    // probe never trusts an unconfigured sandbox or a disabled approval gate.
    let mut controls = (0usize, 0usize);
    fn pinned_control_config<'a>(
        m: &'a serde_yaml::Mapping,
        id: &str,
        allowed_keys: &[&str],
    ) -> Result<&'a serde_yaml::Mapping, String> {
        if let Some(disabled) = m.get(serde_yaml::Value::String("disabled".into())) {
            // Activation expressions are never evaluated, so anything except
            // an explicit `disabled: false` fails closed.
            if disabled.as_bool() != Some(false) {
                return Err(format!("strict {id} control must not be disabled"));
            }
        }
        let config = m
            .get(serde_yaml::Value::String("config".into()))
            .ok_or_else(|| format!("strict {id} control is missing its config"))?;
        let map = config
            .as_mapping()
            .ok_or_else(|| format!("strict {id} config must be a mapping"))?;
        if map
            .keys()
            .any(|key| !allowed_keys.contains(&key.as_str().unwrap_or("")))
        {
            return Err(format!("strict {id} config contains unmanaged keys"));
        }
        Ok(map)
    }
    fn walk(v: &serde_yaml::Value, saw: &mut (usize, usize)) -> Result<(), String> {
        match v {
            serde_yaml::Value::Mapping(m) => {
                let id = m
                    .get(serde_yaml::Value::String("id".into()))
                    .and_then(|v| v.as_str());
                let disabled = m
                    .get(serde_yaml::Value::String("disabled".into()))
                    .and_then(|v| v.as_bool());
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
                    match id {
                        "sandbox-policy" => {
                            saw.0 += 1;
                            let config = pinned_control_config(m, id, &["mode", "workspaceRoot"])?;
                            if config.get("mode").and_then(|v| v.as_str()) != Some("read-only") {
                                return Err("sandbox-policy must pin mode read-only".into());
                            }
                            // The managed patch pins only the mode; the root is
                            // the shipped default expression, kept verbatim
                            // because resolve_launch pins the child cwd to the
                            // task workspace. Any other value is drift.
                            if let Some(root) = config.get("workspaceRoot") {
                                if root.as_str() != Some("process.cwd()") {
                                    return Err(
                                        "sandbox-policy workspaceRoot must stay process.cwd()"
                                            .into(),
                                    );
                                }
                            }
                        }
                        "approval" => {
                            saw.1 += 1;
                            let config = pinned_control_config(m, id, &["policy"])?;
                            // Both accepted values resolve to ask inside DSH:
                            // preflight pins DSH_PERMISSION_MODE=read-only, so
                            // the shipped conditional evaluates to ask. The
                            // expression itself is never evaluated here.
                            if !matches!(
                                config.get("policy").and_then(|v| v.as_str()),
                                Some("ask")
                                    | Some(
                                        "(process.env.DSH_PERMISSION_MODE ?? 'workspace-write') \
                                         === 'danger-full-access' ? 'never' : 'ask'",
                                    )
                            ) {
                                return Err("approval must resolve to ask".into());
                            }
                        }
                        _ => {}
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
    walk(value, &mut controls)?;
    if controls.0 != 1 {
        return Err("dump-config requires exactly one enabled sandbox-policy".into());
    }
    if controls.1 != 1 {
        return Err("dump-config requires exactly one enabled approval".into());
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

/// Parse and validate the managed manifest-build composition. The descriptor is
/// static, so it cannot carry the per-task manifest; it pins the sandbox, the
/// write-guard mount, the fs-only write path, and the exact strict-plan
/// disabled set (minus `tool-fs`) instead.
pub fn manifest_build_profile() -> Result<Value, String> {
    let profile: Value = serde_json::from_str(MANIFEST_BUILD_PROFILE_JSON)
        .map_err(|error| format!("managed dsh manifest-build profile is invalid: {error}"))?;
    validate_manifest_build_profile(&profile)?;
    Ok(profile)
}

pub fn validate_manifest_build_profile(profile: &Value) -> Result<(), String> {
    let Some(obj) = profile.as_object() else {
        return Err("managed dsh manifest-build profile must be an object".into());
    };
    if obj
        .keys()
        .any(|k| !matches!(k.as_str(), "agent" | "composition" | "notes"))
    {
        return Err("managed dsh manifest-build profile contains unknown fields".into());
    }
    if profile.get("agent").and_then(Value::as_str) != Some(crate::DSH_AGENT_NAME) {
        return Err("managed dsh manifest-build profile must declare agent=dsh".into());
    }
    let composition = profile.get("composition").ok_or_else(|| {
        "managed dsh manifest-build profile is missing the composition object".to_string()
    })?;
    let Some(c) = composition.as_object() else {
        return Err("managed dsh manifest-build composition must be an object".into());
    };
    if c.keys().any(|k| {
        !matches!(
            k.as_str(),
            "sandbox"
                | "permission_mode"
                | "unknown_entry_policy"
                | "overlay"
                | "write_manifest"
                | "write_guard"
                | "enabled_write_tools"
                | "disabled_entries"
        )
    }) {
        return Err("managed dsh manifest-build composition contains unknown fields".into());
    }
    if c.get("sandbox").and_then(Value::as_str) != Some("workspace-write")
        || c.get("permission_mode").and_then(Value::as_str) != Some("workspace-write")
        || c.get("unknown_entry_policy").and_then(Value::as_str) != Some("fail-closed")
        || c.get("overlay").and_then(Value::as_str) != Some("managed-patch")
    {
        return Err(
            "managed dsh manifest-build profile must pin a workspace-write fail-closed composition"
                .into(),
        );
    }
    // The runtime manifest is per task; the descriptor only pins its source so
    // a static manifest can never be smuggled in through the descriptor.
    if c.get("write_manifest").and_then(Value::as_str) != Some("per-task") {
        return Err("managed dsh manifest-build write_manifest must be per-task".into());
    }
    let Some(guard) = c.get("write_guard").and_then(Value::as_object) else {
        return Err("managed dsh manifest-build profile must mount the write_guard plugin".into());
    };
    if guard
        .keys()
        .any(|k| !matches!(k.as_str(), "plugin" | "entry" | "registration"))
        || guard.get("plugin").and_then(Value::as_str) != Some("dsh-write-guard")
        || guard.get("entry").and_then(Value::as_str) != Some("lib/index.js")
        || guard.get("registration").and_then(Value::as_str) != Some("prepend")
    {
        return Err("managed dsh manifest-build write_guard mount is not pinned".into());
    }
    let Some(write_tools) = c.get("enabled_write_tools").and_then(Value::as_array) else {
        return Err("managed dsh manifest-build profile must pin enabled_write_tools".into());
    };
    if write_tools.len() != 1 || write_tools[0].as_str() != Some("tool-fs") {
        return Err("managed dsh manifest-build profile must enable only tool-fs writes".into());
    }
    let Some(disabled) = c.get("disabled_entries").and_then(Value::as_array) else {
        return Err("managed dsh manifest-build profile must pin disabled_entries".into());
    };
    if disabled.len() != MANIFEST_BUILD_DISABLED_IDS.len() {
        return Err(
            "managed dsh manifest-build disabled_entries must mirror the strict-plan set".into(),
        );
    }
    let mut actual: Vec<&str> = Vec::with_capacity(disabled.len());
    for entry in disabled {
        actual.push(
            entry
                .as_str()
                .ok_or("managed dsh manifest-build disabled_entries must be strings")?,
        );
    }
    let mut expected = MANIFEST_BUILD_DISABLED_IDS.to_vec();
    actual.sort_unstable();
    expected.sort_unstable();
    if actual != expected {
        return Err(
            "managed dsh manifest-build disabled_entries must mirror the strict-plan set minus tool-fs"
                .into(),
        );
    }
    Ok(())
}

fn yaml_string(value: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(value.to_string())
}

fn yaml_mapping(pairs: &[(&str, serde_yaml::Value)]) -> serde_yaml::Value {
    let mut map = serde_yaml::Mapping::new();
    for (key, value) in pairs {
        map.insert(yaml_string(key), value.clone());
    }
    serde_yaml::Value::Mapping(map)
}

/// Build the manifest-build patch applied on top of the user's DSH profile
/// layer.
///
/// The patch pins `sandbox-policy` to `workspace-write` (exactly one key),
/// copies the strict-plan disabled set verbatim except for `tool-fs`, and
/// appends one `insert` row mounting the materialized write-guard plugin by
/// absolute path with the caller's repo-relative manifest. Every value is
/// serialized as YAML data, never concatenated as text, so path components
/// containing `:`, `#`, quotes, backslashes, or surrounding whitespace
/// round-trip losslessly. An empty manifest is rejected before any output.
pub fn manifest_build_patch(
    manifest: &[PathBuf],
    guard_entry: &std::path::Path,
) -> Result<String, String> {
    if manifest.is_empty() {
        return Err("manifest-build patch requires a non-empty write manifest".into());
    }
    if !guard_entry.is_absolute() {
        return Err("manifest-build guard entry must be an absolute path".into());
    }
    let guard_entry = guard_entry
        .to_str()
        .ok_or("manifest-build guard entry must be valid UTF-8")?;

    let mut patch: Vec<serde_yaml::Value> = Vec::new();
    patch.push(yaml_mapping(&[
        ("id", yaml_string("sandbox-policy")),
        (
            "config",
            yaml_mapping(&[("mode", yaml_string("workspace-write"))]),
        ),
    ]));
    for id in MANIFEST_BUILD_DISABLED_IDS {
        patch.push(yaml_mapping(&[
            ("id", yaml_string(id)),
            ("disabled", serde_yaml::Value::Bool(true)),
        ]));
    }

    let mut entries = Vec::with_capacity(manifest.len());
    for entry in manifest {
        let entry = entry
            .to_str()
            .ok_or("manifest-build write manifest entries must be valid UTF-8")?;
        if entry.is_empty() {
            return Err("manifest-build write manifest entries must be non-empty".into());
        }
        entries.push(yaml_string(entry));
    }
    let insert = yaml_mapping(&[
        ("name", yaml_string(guard_entry)),
        (
            "config",
            yaml_mapping(&[("manifest", serde_yaml::Value::Sequence(entries))]),
        ),
    ]);
    patch.push(yaml_mapping(&[(
        "insert",
        serde_yaml::Value::Sequence(vec![insert]),
    )]));

    serde_yaml::to_string(&serde_yaml::Value::Sequence(patch))
        .map_err(|error| format!("failed to serialize manifest-build patch: {error}"))
}

/// Fail-closed walk over every enabled entry of a manifest-build dump. The
/// strict-plan allowlist applies unchanged, with the single exception that
/// `tool-fs` may stay enabled; an unknown or otherwise dangerous enabled entry
/// rejects the whole dump before a prompt is sent. Name-only entries (the
/// `insert` shape, which carries no `id`) are rejected as unmanaged unless they
/// are the one write-guard entry `validate_manifest_build_dump` already pinned.
fn validate_manifest_build_entries(
    value: &serde_yaml::Value,
    guard: &serde_yaml::Value,
) -> Result<(), String> {
    let guard_name = guard["name"].as_str();
    fn walk(v: &serde_yaml::Value, guard_name: Option<&str>) -> Result<(), String> {
        match v {
            serde_yaml::Value::Mapping(m) => {
                let id = m
                    .get(serde_yaml::Value::String("id".into()))
                    .and_then(|v| v.as_str());
                let name = m
                    .get(serde_yaml::Value::String("name".into()))
                    .and_then(|v| v.as_str());
                let disabled = m
                    .get(serde_yaml::Value::String("disabled".into()))
                    .and_then(|v| v.as_bool());
                if id.is_none() && name.is_some() && name != guard_name {
                    return Err(format!(
                        "manifest-build dump contains an unmanaged name-only entry: {name:?}"
                    ));
                }
                if let Some(id) = id {
                    if disabled != Some(true) {
                        if DANGEROUS.contains(&id) && id != "tool-fs" {
                            return Err(format!(
                                "manifest-build enables dangerous DSH entry: {id:?}"
                            ));
                        }
                        if !ALLOWED_ENABLED.contains(&id)
                            && !matches!(id, "tool-fs" | "sandbox-policy" | "approval")
                        {
                            return Err(format!(
                                "manifest-build unknown enabled DSH entry: {id:?}"
                            ));
                        }
                    }
                }
                for key in ["runnerCommand", "executor", "executors"] {
                    if m.contains_key(serde_yaml::Value::String(key.into())) {
                        return Err(format!("manifest-build dump contains unmanaged {key}"));
                    }
                }
                if id == Some("settings") && disabled != Some(true) {
                    if let Some(config) = m.get(serde_yaml::Value::String("config".into())) {
                        if !config
                            .as_mapping()
                            .is_some_and(serde_yaml::Mapping::is_empty)
                        {
                            return Err("manifest-build settings drift is not allowed".into());
                        }
                    }
                }
                for val in m.values() {
                    walk(val, guard_name)?;
                }
            }
            serde_yaml::Value::Sequence(s) => {
                for x in s {
                    walk(x, guard_name)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    walk(value, guard_name)
}

/// Validate a `--dump-config` dump produced by a manifest-build launch.
///
/// The dump is untrusted data. Exactly one write-guard insert entry must be
/// present (the loader normalizes the patch name into a `file://` URL ending in
/// `/dsh-write-guard/lib/index.js`) and its `config.manifest` must equal the
/// caller's repo-relative manifest entry by entry. The composition must keep
/// `tool-fs` enabled, keep every strict-plan tool disabled, pin exactly one
/// `workspace-write` sandbox, and preserve the build approval/permission
/// presets; any other enabled entry fails closed.
pub fn validate_manifest_build_dump(
    value: &serde_yaml::Value,
    manifest: &[PathBuf],
    workspace: &std::path::Path,
) -> Result<(), String> {
    if manifest.is_empty() {
        return Err("manifest-build requires a non-empty write manifest".into());
    }
    let entries = value
        .as_sequence()
        .ok_or("manifest-build dump must contain a plugin sequence")?;
    let guard_entries: Vec<&serde_yaml::Value> = entries
        .iter()
        .filter(|e| {
            e["name"].as_str().is_some_and(|name| {
                name.starts_with("file://") && name.ends_with("/dsh-write-guard/lib/index.js")
            })
        })
        .collect();
    if guard_entries.len() != 1 {
        return Err("manifest-build requires exactly one write-guard entry".into());
    }
    let guard = guard_entries[0];
    // Three-state pin, identical to `pinned_enabled`: only an absent
    // `disabled` or an explicit boolean `false` keeps the guard enabled; any
    // other shape (including a string activation expression) fails closed.
    match &guard["disabled"] {
        serde_yaml::Value::Null | serde_yaml::Value::Bool(false) => {}
        _ => return Err("manifest-build write-guard entry must stay enabled".into()),
    }
    let Some(config) = guard["config"].as_mapping() else {
        return Err("manifest-build write-guard entry must carry a config mapping".into());
    };
    if config.keys().any(|key| key.as_str() != Some("manifest")) {
        return Err("manifest-build write-guard config contains unmanaged keys".into());
    }
    let actual = guard["config"]["manifest"]
        .as_sequence()
        .ok_or("manifest-build write-guard must carry a config.manifest sequence")?;
    if actual.len() != manifest.len() {
        return Err("manifest-build write-guard manifest length mismatch".into());
    }
    for (index, (actual_entry, expected_entry)) in actual.iter().zip(manifest).enumerate() {
        let expected = expected_entry
            .to_str()
            .ok_or("manifest-build expected manifest entries must be valid UTF-8")?;
        if actual_entry.as_str() != Some(expected) {
            return Err(format!(
                "manifest-build write-guard manifest mismatch at entry {index}"
            ));
        }
    }

    let exactly_one = |id: &str| -> Result<&serde_yaml::Value, String> {
        let matches: Vec<&serde_yaml::Value> = entries
            .iter()
            .filter(|e| e["id"].as_str() == Some(id))
            .collect();
        if matches.len() != 1 {
            return Err(format!("manifest-build requires exactly one {id}"));
        }
        Ok(matches[0])
    };
    // Managed controls replicate `validate_build_dump`'s entry closure: a hit is
    // an entry matching *either* the id or the name, so an id-correct entry
    // carrying a drifted name makes the hit count two (a duplicate id) and fails
    // closed instead of hiding behind the name filter. Exactly one hit must then
    // carry both the pinned id and the pinned name.
    let exactly_one_named = |id: &str, name: &str| -> Result<&serde_yaml::Value, String> {
        let matches: Vec<&serde_yaml::Value> = entries
            .iter()
            .filter(|e| e["id"].as_str() == Some(id) || e["name"].as_str() == Some(name))
            .collect();
        if matches.len() != 1 {
            return Err(format!("manifest-build requires exactly one {id}"));
        }
        let entry = matches[0];
        if entry["id"].as_str() != Some(id) || entry["name"].as_str() != Some(name) {
            return Err(format!("manifest-build requires provider-owned {id}"));
        }
        Ok(entry)
    };
    // Provider-owned base entries with no platform toggle, pinned exactly like
    // `validate_build_dump`'s entry closure: exactly one entry matching either
    // the id or the name, and it must carry both plus an explicit enablement.
    // A missing entry and `{disabled: true}` both fail closed.
    let pinned_enabled = |id: &str, name: &str| -> Result<&serde_yaml::Value, String> {
        let matches: Vec<&serde_yaml::Value> = entries
            .iter()
            .filter(|e| e["id"].as_str() == Some(id) || e["name"].as_str() == Some(name))
            .collect();
        if matches.len() != 1 {
            return Err(format!("manifest-build requires exactly one {id}"));
        }
        let entry = matches[0];
        let disabled = match &entry["disabled"] {
            serde_yaml::Value::Null => Some(false),
            serde_yaml::Value::Bool(value) => Some(*value),
            _ => None,
        };
        if entry["id"].as_str() != Some(id)
            || entry["name"].as_str() != Some(name)
            || disabled != Some(false)
        {
            return Err(format!(
                "manifest-build requires provider-owned {id} with the pinned platform activation"
            ));
        }
        Ok(entry)
    };

    let tool_fs = exactly_one("tool-fs")?;
    // Same three-state pin as the guard: a string `disabled` expression must not
    // masquerade as "enabled" through `.as_bool() == Some(true)`.
    match &tool_fs["disabled"] {
        serde_yaml::Value::Null | serde_yaml::Value::Bool(false) => {}
        _ => return Err("manifest-build requires tool-fs enabled".into()),
    }
    for id in MANIFEST_BUILD_DISABLED_IDS {
        let entry = exactly_one(id)?;
        if entry["disabled"].as_bool() != Some(true) {
            return Err(format!("manifest-build requires {id} disabled"));
        }
    }

    let policy = exactly_one_named("sandbox-policy", "@deepseek-ai/dsh-sandbox-policy")?;
    if policy["disabled"].as_bool() == Some(true) {
        return Err("manifest-build sandbox-policy must stay enabled".into());
    }
    let Some(policy_config) = policy["config"].as_mapping() else {
        return Err("manifest-build sandbox-policy must carry a config mapping".into());
    };
    if policy_config
        .keys()
        .any(|key| !matches!(key.as_str(), Some("mode" | "workspaceRoot")))
    {
        return Err("manifest-build sandbox-policy config contains unmanaged keys".into());
    }
    if policy_config.get("mode").and_then(|v| v.as_str()) != Some("workspace-write") {
        return Err("manifest-build sandbox must pin mode workspace-write".into());
    }
    // The generated patch replaces the base sandbox-policy config, so the root
    // is normally absent; the shipped default expression and the launch
    // workspace are the only roots that keep the sandbox over the task
    // workspace.
    if let Some(root) = policy_config.get("workspaceRoot").and_then(|v| v.as_str()) {
        if root != "process.cwd()" && Some(root) != workspace.to_str() {
            return Err("manifest-build sandbox root must match the task workspace".into());
        }
    }

    let approval = exactly_one_named("approval", "@deepseek-ai/dsh-user-approval")?;
    if approval["disabled"].as_bool() == Some(true) {
        return Err("manifest-build approval must stay enabled".into());
    }
    if !matches!(
        approval["config"]["policy"].as_str(),
        Some("ask")
            | Some(
                "(process.env.DSH_PERMISSION_MODE ?? 'workspace-write') \
                 === 'danger-full-access' ? 'never' : 'ask'",
            )
    ) {
        return Err("manifest-build approval must resolve to ask".into());
    }
    let permission = exactly_one_named("permission", "@deepseek-ai/dsh-permission-presets")?;
    let preset = &permission["config"]["presets"]["workspace-write"];
    if preset["sandbox"].as_str() != Some("workspace-write")
        || preset["approval"].as_str() != Some("ask")
    {
        return Err(
            "manifest-build permission preset must preserve workspace-write and ask".into(),
        );
    }
    if let Some(default) = permission["config"]["defaultPreset"].as_str() {
        if default != "workspace-write" {
            return Err("manifest-build permission defaultPreset must be workspace-write".into());
        }
    }
    if let Some(settings) = permission["config"].get("settings") {
        if !settings.is_null() && !settings.as_mapping().is_some_and(|m| m.is_empty()) {
            return Err("manifest-build permission settings overrides are not managed".into());
        }
    }

    for (id, name, timeout) in [
        ("sandbox", "@deepseek-ai/dsh-sandbox-local", false),
        ("bash-sandbox", "@deepseek-ai/dsh-bash-sandbox", true),
        ("pwsh-sandbox", "@deepseek-ai/dsh-pwsh-sandbox", false),
    ] {
        let matches: Vec<&serde_yaml::Value> = entries
            .iter()
            .filter(|e| e["id"].as_str() == Some(id) || e["name"].as_str() == Some(name))
            .collect();
        if matches.len() != 1 {
            return Err(format!("manifest-build requires exactly one {id}"));
        }
        let entry = matches[0];
        let disabled = match &entry["disabled"] {
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
        if entry["id"].as_str() != Some(id)
            || entry["name"].as_str() != Some(name)
            || disabled != Some(should_disable)
        {
            return Err(format!(
                "manifest-build requires provider-owned {id} with the pinned platform activation"
            ));
        }
        if entry.as_mapping().is_some_and(|m| {
            m.keys()
                .any(|key| !matches!(key.as_str(), Some("id" | "name" | "disabled" | "config")))
        }) {
            return Err(format!(
                "manifest-build {id} contains unverified plugin overrides"
            ));
        }
        let config = &entry["config"];
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
                "manifest-build {id} contains unverified executor configuration"
            ));
        }
    }
    pinned_enabled("fs-sandbox", "@deepseek-ai/dsh-fs-sandbox")?;
    pinned_enabled("acp", "@deepseek-ai/dsh-acp")?;
    pinned_enabled("acp-app-startup", "@deepseek-ai/dsh-acp-app")?;

    validate_manifest_build_entries(value, guard)
}

/// Run the manifest-build preflight independently from the ACP command. The
/// launch must pin `workspace-write` and carry the generated patch as an
/// absolute regular file; `--version` and `--dump-config` are probed exactly
/// like the existing build preflight and the dump is validated against the
/// expected manifest. The patch-less build preflight is unchanged.
pub fn preflight_build_manifest(launch: &DshLaunch, manifest: &[PathBuf]) -> Result<(), String> {
    // Fail loud before spawning any probe: an empty manifest can never be
    // described, so it is rejected here as well as defensively in
    // `validate_manifest_build_dump` (same message, earlier ordering).
    if manifest.is_empty() {
        return Err("manifest-build requires a non-empty write manifest".into());
    }
    manifest_build_profile()?;
    if launch.permission_mode.as_deref() != Some("workspace-write") {
        return Err("manifest-build launch must pin workspace-write".into());
    }
    let patch = launch
        .patch
        .as_deref()
        .ok_or("manifest-build launch requires a managed patch")?;
    if !patch.is_absolute() || !patch.is_file() {
        return Err("manifest-build patch must be an absolute regular file".into());
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
        return Err("dsh manifest-build dump-config failed".into());
    }
    validate_manifest_build_dump(
        &validate_dump_config_yaml(&String::from_utf8_lossy(&output.stdout))?,
        manifest,
        &launch.workspace,
    )
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
    fn launch_profile_and_version_are_explicit_and_fail_closed() {
        let runtime = temp_workspace().join("runtime.mjs");
        std::fs::write(&runtime, "").unwrap();
        let mut launch = DshLaunch::new(Some(runtime), temp_workspace(), None);
        launch.profile = Some("acp".into());
        launch.version = Some(PINNED_DSH_VERSION.into());
        let command = resolve_launch(&launch).unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|pair| pair == ["--profile", "acp"]));
        launch.version = Some("0.0.0".into());
        assert!(resolve_launch(&launch).is_err());
        launch.version = None;
        launch.profile = Some("unsafe".into());
        assert!(resolve_launch(&launch).is_err());
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
        let yaml = serde_yaml::from_str::<serde_yaml::Value>("- id: sandbox-policy\n  config:\n    mode: read-only\n- id: approval\n  config:\n    policy: ask\n- id: tool-unknown\n  disabled: false").unwrap();
        assert!(validate_dump_policy(&yaml)
            .unwrap_err()
            .contains("unknown enabled"));
    }

    #[test]
    fn dump_policy_rejects_unknown_enabled_entry() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            "- id: sandbox-policy\n  config:\n    mode: read-only\n- id: approval\n  config:\n    policy: ask\n- id: unmanaged-entry\n  disabled: false",
        )
        .unwrap();
        assert!(validate_dump_policy(&yaml)
            .unwrap_err()
            .contains("unknown enabled"));
    }

    #[test]
    fn dump_policy_rejects_executor_and_settings_drift() {
        for drift in [
            "- id: sandbox-policy\n  disabled: false\n  config:\n    mode: read-only\n    runnerCommand: /bin/sh\n- id: approval\n  config:\n    policy: ask",
            "- id: sandbox-policy\n  config:\n    mode: read-only\n- id: approval\n  config:\n    policy: ask\n- id: settings\n  disabled: false\n  config:\n    permission_mode: workspace-write",
        ] {
            let yaml = serde_yaml::from_str::<serde_yaml::Value>(drift).unwrap();
            assert!(validate_dump_policy(&yaml).is_err());
        }
    }

    #[test]
    fn dump_policy_fails_closed_on_missing_or_drifted_controls() {
        let baseline = "- id: sandbox-policy\n  config:\n    mode: read-only\n- id: approval\n  config:\n    policy: ask";
        validate_dump_policy(&serde_yaml::from_str(baseline).unwrap()).unwrap();
        // R0 counterexample: an enabled sandbox-policy without config and a
        // disabled approval used to pass and send the probe prompt.
        let counterexample =
            "- id: sandbox-policy\n  disabled: false\n- id: approval\n  disabled: true";
        assert!(validate_dump_policy(&serde_yaml::from_str(counterexample).unwrap()).is_err());
        for drift in [
            // missing or duplicated controls
            "- id: sandbox-policy\n  config:\n    mode: read-only".to_string(),
            "- id: approval\n  config:\n    policy: ask".to_string(),
            format!("{baseline}\n- id: sandbox-policy\n  config:\n    mode: read-only"),
            format!("{baseline}\n- id: approval\n  config:\n    policy: ask"),
            // disabled or unresolvable activation
            "- id: sandbox-policy\n  disabled: true\n  config:\n    mode: read-only\n- id: approval\n  config:\n    policy: ask".to_string(),
            "- id: sandbox-policy\n  config:\n    mode: read-only\n- id: approval\n  disabled: true\n  config:\n    policy: ask".to_string(),
            "- id: sandbox-policy\n  disabled: process.platform === 'win32'\n  config:\n    mode: read-only\n- id: approval\n  config:\n    policy: ask".to_string(),
            // missing config
            "- id: sandbox-policy\n- id: approval\n  config:\n    policy: ask".to_string(),
            "- id: sandbox-policy\n  config:\n    mode: read-only\n- id: approval".to_string(),
            // drifted values
            "- id: sandbox-policy\n  config:\n    mode: workspace-write\n- id: approval\n  config:\n    policy: ask".to_string(),
            "- id: sandbox-policy\n  config:\n    mode: read-only\n- id: approval\n  config:\n    policy: never".to_string(),
            // drifted workspaceRoot
            "- id: sandbox-policy\n  config:\n    mode: read-only\n    workspaceRoot: /\n- id: approval\n  config:\n    policy: ask".to_string(),
            // unmanaged config keys
            "- id: sandbox-policy\n  config:\n    mode: read-only\n    autoAllow: true\n- id: approval\n  config:\n    policy: ask".to_string(),
            "- id: sandbox-policy\n  config:\n    mode: read-only\n- id: approval\n  config:\n    policy: ask\n    autoApprove: true".to_string(),
        ] {
            let yaml = serde_yaml::from_str::<serde_yaml::Value>(&drift).unwrap();
            assert!(validate_dump_policy(&yaml).is_err(), "{drift}");
        }
        // The shipped conditional resolves to ask under the pinned read-only
        // permission mode; it is matched textually and never evaluated.
        let conditional = serde_json::json!([
            {"id": "sandbox-policy", "config": {"mode": "read-only"}},
            {"id": "approval", "config": {"policy": "(process.env.DSH_PERMISSION_MODE ?? 'workspace-write') === 'danger-full-access' ? 'never' : 'ask'"}},
        ]);
        validate_dump_policy(&serde_yaml::to_value(&conditional).unwrap()).unwrap();
        // The shipped default root expression is kept verbatim alongside the
        // patch-pinned mode; the drift loop above rejects any other value.
        let shipped_root = serde_json::json!([
            {"id": "sandbox-policy", "config": {"mode": "read-only", "workspaceRoot": "process.cwd()"}},
            {"id": "approval", "config": {"policy": "ask"}},
        ]);
        validate_dump_policy(&serde_yaml::to_value(&shipped_root).unwrap()).unwrap();
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

    fn manifest_dump(manifest: &[&str]) -> serde_json::Value {
        let mut entries = vec![
            serde_json::json!({"id":"sandbox-policy","name":"@deepseek-ai/dsh-sandbox-policy","config":{"mode":"workspace-write"}}),
            serde_json::json!({"id":"approval","name":"@deepseek-ai/dsh-user-approval","config":{"policy":"ask"}}),
            serde_json::json!({"id":"permission","name":"@deepseek-ai/dsh-permission-presets","config":{"presets":{"workspace-write":{"sandbox":"workspace-write","approval":"ask"}}}}),
            serde_json::json!({"id":"sandbox","name":"@deepseek-ai/dsh-sandbox-local"}),
            serde_json::json!({"id":"bash-sandbox","name":"@deepseek-ai/dsh-bash-sandbox","disabled":"process.platform === 'win32'","config":{"timeoutMs":60000}}),
            serde_json::json!({"id":"pwsh-sandbox","name":"@deepseek-ai/dsh-pwsh-sandbox","disabled":"process.platform !== 'win32'"}),
            serde_json::json!({"id":"fs-sandbox","name":"@deepseek-ai/dsh-fs-sandbox"}),
            serde_json::json!({"id":"acp","name":"@deepseek-ai/dsh-acp"}),
            serde_json::json!({"id":"acp-app-startup","name":"@deepseek-ai/dsh-acp-app"}),
            serde_json::json!({"id":"tool-fs","name":"@deepseek-ai/dsh-tool-fs"}),
            serde_json::json!({"id":"tool-fs-search","name":"@deepseek-ai/dsh-tool-fs-search"}),
            serde_json::json!({"name":"file:///tmp/manifest-build/dsh-write-guard/lib/index.js","config":{"manifest": manifest}}),
        ];
        for id in MANIFEST_BUILD_DISABLED_IDS {
            entries.push(serde_json::json!({"id": id, "disabled": true}));
        }
        serde_json::Value::Array(entries)
    }

    fn validate_manifest_dump(
        manifest: &[PathBuf],
        dump: &serde_json::Value,
    ) -> Result<(), String> {
        validate_manifest_build_dump(
            &serde_yaml::to_value(dump).unwrap(),
            manifest,
            std::path::Path::new("/workspace"),
        )
    }

    fn dump_entry_index(dump: &serde_json::Value, id: &str) -> usize {
        dump.as_array()
            .unwrap()
            .iter()
            .position(|entry| entry["id"].as_str() == Some(id))
            .unwrap()
    }

    fn guard_index(dump: &serde_json::Value) -> usize {
        dump.as_array()
            .unwrap()
            .iter()
            .position(|entry| {
                entry["name"]
                    .as_str()
                    .is_some_and(|name| name.starts_with("file://"))
            })
            .unwrap()
    }

    #[test]
    fn write_guard_artifacts_are_embedded_verbatim() {
        assert!(!WRITE_GUARD_PACKAGE_JSON.is_empty());
        assert!(!WRITE_GUARD_INDEX_JS.is_empty());
        assert_eq!(WRITE_GUARD_FILES.len(), 2);
        assert_eq!(WRITE_GUARD_FILES[0].0, "package.json");
        assert_eq!(WRITE_GUARD_FILES[1].0, "lib/index.js");
        assert_eq!(WRITE_GUARD_FILES[0].1, WRITE_GUARD_PACKAGE_JSON);
        assert_eq!(WRITE_GUARD_FILES[1].1, WRITE_GUARD_INDEX_JS);
        assert!(WRITE_GUARD_PACKAGE_JSON.contains("\"dsh-write-guard\""));
        assert!(WRITE_GUARD_PACKAGE_JSON.contains("\"main\": \"lib/index.js\""));
        assert!(WRITE_GUARD_INDEX_JS.contains("FS_WRITE_MANIFEST_DENIED"));
        assert!(WRITE_GUARD_INDEX_JS.contains("fs/write-intent"));
        assert!(WRITE_GUARD_INDEX_JS.contains("fs/edit-intent"));
        let profile: Value = serde_json::from_str(MANIFEST_BUILD_PROFILE_JSON).unwrap();
        assert!(profile.is_object());
    }

    #[test]
    fn manifest_build_disabled_set_copies_strict_plan_minus_tool_fs() {
        let strict: serde_yaml::Value = serde_yaml::from_str(STRICT_PLAN_PATCH_YAML).unwrap();
        let rows = strict.as_sequence().unwrap();
        // The exclusion is only meaningful if strict-plan actually disables the
        // filesystem tool; manifest-build is the one composition that re-enables
        // it under the guard.
        assert!(rows.iter().any(|row| {
            row["id"].as_str() == Some("tool-fs") && row["disabled"].as_bool() == Some(true)
        }));
        let mut expected: Vec<&str> = rows
            .iter()
            .filter(|row| row["disabled"].as_bool() == Some(true))
            .filter_map(|row| row["id"].as_str())
            .filter(|id| *id != "tool-fs")
            .collect();
        let mut actual = MANIFEST_BUILD_DISABLED_IDS.to_vec();
        expected.sort_unstable();
        actual.sort_unstable();
        assert_eq!(actual, expected);
    }

    #[test]
    fn manifest_build_patch_pins_overrides_and_insert() {
        let manifest = vec![PathBuf::from("src/a.rs"), PathBuf::from("docs/")];
        let guard = PathBuf::from("/tmp/manifest-build/dsh-write-guard/lib/index.js");
        let patch = manifest_build_patch(&manifest, &guard).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&patch).unwrap();
        let rows = parsed.as_sequence().unwrap();

        let policies: Vec<_> = rows
            .iter()
            .filter(|row| row["id"].as_str() == Some("sandbox-policy"))
            .collect();
        assert_eq!(policies.len(), 1);
        assert_eq!(
            policies[0]["config"]["mode"].as_str(),
            Some("workspace-write")
        );
        assert_eq!(policies[0]["config"].as_mapping().unwrap().len(), 1);

        // tool-fs keeps its base enablement: no disable override row at all.
        assert!(!rows.iter().any(|row| row["id"].as_str() == Some("tool-fs")));

        for id in MANIFEST_BUILD_DISABLED_IDS {
            let matches: Vec<_> = rows
                .iter()
                .filter(|row| row["id"].as_str() == Some(*id))
                .collect();
            assert_eq!(matches.len(), 1, "{id}");
            assert_eq!(matches[0]["disabled"].as_bool(), Some(true), "{id}");
            assert_eq!(matches[0].as_mapping().unwrap().len(), 2, "{id}");
        }

        let inserts: Vec<_> = rows
            .iter()
            .filter(|row| row["insert"].is_sequence())
            .collect();
        assert_eq!(inserts.len(), 1);
        let guard_row = &inserts[0]["insert"][0];
        assert_eq!(guard_row["name"].as_str(), Some(guard.to_str().unwrap()));
        let manifest_entries: Vec<&str> = guard_row["config"]["manifest"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(manifest_entries, vec!["src/a.rs", "docs/"]);

        let expected_rows = 1 + MANIFEST_BUILD_DISABLED_IDS.len() + 1;
        assert_eq!(rows.len(), expected_rows);

        assert!(manifest_build_patch(&[], &guard).is_err());
        assert!(
            manifest_build_patch(&manifest, std::path::Path::new("relative/index.js")).is_err()
        );
        assert!(manifest_build_patch(&[PathBuf::from("")], &guard).is_err());
    }

    #[test]
    fn manifest_build_patch_round_trips_special_character_paths() {
        let manifest = vec![
            PathBuf::from("src/a.rs"),
            PathBuf::from("with\"double.rs"),
            PathBuf::from("with'single.rs"),
            PathBuf::from("with:colon.rs"),
            PathBuf::from("with#hash.rs"),
            PathBuf::from("with\\backslash.rs"),
            PathBuf::from("  leading and trailing  "),
            PathBuf::from("yaml: value"),
            PathBuf::from("- dash"),
            PathBuf::from("true"),
            PathBuf::from("123"),
            PathBuf::from("null"),
            PathBuf::from("emoji-\u{1f680}.rs"),
        ];
        let guard = PathBuf::from("/tmp/manifest build/gu\"ard: #1/lib/index.js");
        let patch = manifest_build_patch(&manifest, &guard).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&patch).unwrap();
        let rows = parsed.as_sequence().unwrap();
        let insert = rows
            .iter()
            .find(|row| row["insert"].is_sequence())
            .expect("insert row");
        let guard_row = &insert["insert"][0];
        assert_eq!(guard_row["name"].as_str(), Some(guard.to_str().unwrap()));
        let actual: Vec<&str> = guard_row["config"]["manifest"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        let expected: Vec<&str> = manifest.iter().map(|p| p.to_str().unwrap()).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn manifest_build_dump_accepts_the_managed_composition_and_rejects_drift() {
        let manifest = vec![PathBuf::from("src/a.rs"), PathBuf::from("docs/")];
        let baseline = manifest_dump(&["src/a.rs", "docs/"]);
        validate_manifest_dump(&manifest, &baseline).unwrap();
        let validate = |dump: &serde_json::Value| validate_manifest_dump(&manifest, dump);

        // The shipped approval conditional and the workspace root expression
        // stay accepted exactly like the build dump.
        let mut shipped = baseline.clone();
        let approval_index = dump_entry_index(&shipped, "approval");
        shipped[approval_index]["config"]["policy"] = serde_json::json!(
            "(process.env.DSH_PERMISSION_MODE ?? 'workspace-write') === 'danger-full-access' ? 'never' : 'ask'"
        );
        validate(&shipped).unwrap();

        let mut missing_guard = baseline.clone();
        missing_guard.as_array_mut().unwrap().retain(|entry| {
            !entry["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("file://"))
        });
        assert!(validate(&missing_guard).is_err());

        let mut duplicate_guard = baseline.clone();
        let duplicated = baseline[guard_index(&baseline)].clone();
        duplicate_guard.as_array_mut().unwrap().push(duplicated);
        assert!(validate(&duplicate_guard).is_err());

        let mut mismatch = baseline.clone();
        let index = guard_index(&mismatch);
        mismatch[index]["config"]["manifest"] = serde_json::json!(["src/b.rs", "docs/"]);
        assert!(validate(&mismatch).is_err());

        let mut guard_key = baseline.clone();
        let index = guard_index(&guard_key);
        guard_key[index]["config"]["extra"] = serde_json::json!(true);
        assert!(validate(&guard_key).is_err());

        let mut fs_disabled = baseline.clone();
        let index = dump_entry_index(&fs_disabled, "tool-fs");
        fs_disabled[index]["disabled"] = serde_json::json!(true);
        assert!(validate(&fs_disabled).is_err());

        let mut fs_missing = baseline.clone();
        let index = dump_entry_index(&fs_missing, "tool-fs");
        fs_missing.as_array_mut().unwrap().remove(index);
        assert!(validate(&fs_missing).is_err());

        let mut bash_enabled = baseline.clone();
        let index = dump_entry_index(&bash_enabled, "tool-bash");
        bash_enabled[index]["disabled"] = serde_json::json!(false);
        assert!(validate(&bash_enabled).is_err());

        let mut sandbox_drift = baseline.clone();
        let index = dump_entry_index(&sandbox_drift, "sandbox-policy");
        sandbox_drift[index]["config"]["mode"] = serde_json::json!("read-only");
        assert!(validate(&sandbox_drift).is_err());

        let mut unknown = baseline.clone();
        unknown
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"id":"unmanaged-entry","disabled":false}));
        assert!(validate(&unknown).is_err());

        let mut approval_drift = baseline.clone();
        let index = dump_entry_index(&approval_drift, "approval");
        approval_drift[index]["config"]["policy"] = serde_json::json!("never");
        assert!(validate(&approval_drift).is_err());

        let mut preset_drift = baseline.clone();
        let index = dump_entry_index(&preset_drift, "permission");
        preset_drift[index]["config"]["presets"]["workspace-write"]["approval"] =
            serde_json::json!("never");
        assert!(validate(&preset_drift).is_err());

        // A non-sequence root and an empty expected manifest fail closed.
        assert!(validate_manifest_build_dump(
            &serde_yaml::Value::Null,
            &manifest,
            std::path::Path::new("/workspace")
        )
        .is_err());
        assert!(validate_manifest_dump(&[], &baseline).is_err());
    }

    #[test]
    fn manifest_build_dump_requires_fs_sandbox_and_acp_entries() {
        let manifest = vec![PathBuf::from("src/a.rs")];
        let baseline = manifest_dump(&["src/a.rs"]);
        validate_manifest_dump(&manifest, &baseline).unwrap();

        for id in ["fs-sandbox", "acp", "acp-app-startup"] {
            let mut missing = baseline.clone();
            let index = dump_entry_index(&missing, id);
            missing.as_array_mut().unwrap().remove(index);
            assert!(
                validate_manifest_dump(&manifest, &missing).is_err(),
                "{id} missing"
            );

            let mut disabled = baseline.clone();
            let index = dump_entry_index(&disabled, id);
            disabled[index]["disabled"] = serde_json::json!(true);
            assert!(
                validate_manifest_dump(&manifest, &disabled).is_err(),
                "{id} disabled"
            );
        }
    }

    #[test]
    fn manifest_build_dump_pins_control_ids_and_names() {
        let manifest = vec![PathBuf::from("src/a.rs")];
        let baseline = manifest_dump(&["src/a.rs"]);
        validate_manifest_dump(&manifest, &baseline).unwrap();

        for (id, name) in [
            ("sandbox-policy", "@deepseek-ai/dsh-sandbox-policy"),
            ("approval", "@deepseek-ai/dsh-user-approval"),
            ("permission", "@deepseek-ai/dsh-permission-presets"),
        ] {
            let mut drifted = baseline.clone();
            let index = dump_entry_index(&drifted, id);
            assert_eq!(drifted[index]["name"].as_str(), Some(name));
            drifted[index]["name"] = serde_json::json!("@deepseek-ai/dsh-unmanaged");
            assert!(
                validate_manifest_dump(&manifest, &drifted).is_err(),
                "{id} name drift"
            );
        }
    }

    #[test]
    fn manifest_build_dump_rejects_unmanaged_name_only_entries() {
        let manifest = vec![PathBuf::from("src/a.rs")];
        let baseline = manifest_dump(&["src/a.rs"]);
        validate_manifest_dump(&manifest, &baseline).unwrap();

        let mut extra = baseline.clone();
        extra.as_array_mut().unwrap().push(serde_json::json!({
            "name": "@deepseek-ai/dsh-tool-fs",
            "disabled": false
        }));
        assert!(validate_manifest_dump(&manifest, &extra).is_err());
    }

    #[test]
    fn manifest_build_dump_rejects_duplicate_id_with_drifted_name() {
        let manifest = vec![PathBuf::from("src/a.rs")];
        let baseline = manifest_dump(&["src/a.rs"]);
        validate_manifest_dump(&manifest, &baseline).unwrap();

        // The OR-hit filter counts both the correct entry and a second entry
        // reusing the correct id with a drifted name, so the duplicate fails
        // closed even though the drifted name alone would be filtered out by an
        // AND predicate.
        for (id, name) in [
            ("sandbox-policy", "@deepseek-ai/dsh-sandbox-policy"),
            ("approval", "@deepseek-ai/dsh-user-approval"),
            ("permission", "@deepseek-ai/dsh-permission-presets"),
        ] {
            let mut duplicated = baseline.clone();
            let index = dump_entry_index(&duplicated, id);
            assert_eq!(duplicated[index]["name"].as_str(), Some(name));
            duplicated.as_array_mut().unwrap().push(serde_json::json!({
                "id": id,
                "name": "@deepseek-ai/dsh-drifted",
                "disabled": false,
                "config": {"unmanaged": true}
            }));
            assert!(
                validate_manifest_dump(&manifest, &duplicated).is_err(),
                "{id} duplicate id with drifted name"
            );
        }
    }

    #[test]
    fn manifest_build_dump_rejects_string_disabled_guard_and_tool_fs() {
        let manifest = vec![PathBuf::from("src/a.rs")];
        let baseline = manifest_dump(&["src/a.rs"]);
        validate_manifest_dump(&manifest, &baseline).unwrap();

        // A string activation expression must not slip past the enablement pin;
        // only an absent `disabled` or an explicit boolean `false` is enabled.
        let mut guard_string = baseline.clone();
        let index = guard_index(&guard_string);
        guard_string[index]["disabled"] = serde_json::json!("process.platform === 'win32'");
        assert!(
            validate_manifest_dump(&manifest, &guard_string).is_err(),
            "guard must reject a string disabled expression"
        );

        let mut tool_fs_string = baseline.clone();
        let index = dump_entry_index(&tool_fs_string, "tool-fs");
        tool_fs_string[index]["disabled"] = serde_json::json!("process.platform === 'win32'");
        assert!(
            validate_manifest_dump(&manifest, &tool_fs_string).is_err(),
            "tool-fs must reject a string disabled expression"
        );
    }

    #[test]
    fn manifest_build_profile_pins_the_managed_composition() {
        let profile = manifest_build_profile().unwrap();
        assert_eq!(profile["agent"], "dsh");
        assert_eq!(profile["composition"]["sandbox"], "workspace-write");
        assert_eq!(profile["composition"]["permission_mode"], "workspace-write");
        assert_eq!(profile["composition"]["write_manifest"], "per-task");
        assert_eq!(
            profile["composition"]["write_guard"]["plugin"],
            "dsh-write-guard"
        );
        assert_eq!(
            profile["composition"]["enabled_write_tools"],
            serde_json::json!(["tool-fs"])
        );
        assert_eq!(
            profile["composition"]["disabled_entries"]
                .as_array()
                .unwrap()
                .len(),
            MANIFEST_BUILD_DISABLED_IDS.len()
        );

        for (pointer, replacement) in [
            ("/agent", serde_json::json!("zcode")),
            ("/composition/sandbox", serde_json::json!("read-only")),
            (
                "/composition/permission_mode",
                serde_json::json!("read-only"),
            ),
            (
                "/composition/unknown_entry_policy",
                serde_json::json!("allow"),
            ),
            ("/composition/overlay", serde_json::json!("native")),
            (
                "/composition/write_manifest",
                serde_json::json!(["src/a.rs"]),
            ),
            (
                "/composition/write_guard/plugin",
                serde_json::json!("other"),
            ),
            (
                "/composition/write_guard/entry",
                serde_json::json!("index.js"),
            ),
            (
                "/composition/write_guard/registration",
                serde_json::json!("append"),
            ),
            (
                "/composition/enabled_write_tools",
                serde_json::json!(["tool-bash"]),
            ),
            ("/composition/disabled_entries", serde_json::json!([])),
        ] {
            let mut drifted = profile.clone();
            *drifted.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                validate_manifest_build_profile(&drifted).is_err(),
                "{pointer}"
            );
        }

        let mut unknown_top = profile.clone();
        unknown_top["unmanaged"] = serde_json::json!(true);
        assert!(validate_manifest_build_profile(&unknown_top).is_err());

        let mut unknown_composition = profile.clone();
        unknown_composition["composition"]["unmanaged"] = serde_json::json!(true);
        assert!(validate_manifest_build_profile(&unknown_composition).is_err());

        let mut widened = profile.clone();
        widened["composition"]["disabled_entries"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("tool-fs"));
        assert!(validate_manifest_build_profile(&widened).is_err());

        let mut non_string = profile.clone();
        non_string["composition"]["disabled_entries"][0] = serde_json::json!(7);
        assert!(validate_manifest_build_profile(&non_string).is_err());

        assert!(validate_manifest_build_profile(&Value::Null).is_err());
    }

    #[test]
    fn preflight_build_manifest_requires_patch_and_workspace_write() {
        let runtime = std::env::temp_dir().join("dsh-manifest-preflight-runtime.mjs");
        std::fs::write(&runtime, "").unwrap();
        let mut launch = DshLaunch::new(Some(runtime.clone()), temp_workspace(), None);
        let manifest = vec![PathBuf::from("src/a.rs")];

        // Missing patch fails before any probe is spawned.
        launch.permission_mode = Some("workspace-write".into());
        assert!(preflight_build_manifest(&launch, &manifest)
            .unwrap_err()
            .contains("requires a managed patch"));

        // Relative and non-file patches are rejected before any probe.
        launch.patch = Some(PathBuf::from("relative.patch.yml"));
        assert!(preflight_build_manifest(&launch, &manifest).is_err());
        launch.patch = Some(std::env::temp_dir().join("dsh-manifest-missing.patch.yml"));
        assert!(preflight_build_manifest(&launch, &manifest).is_err());

        // A non workspace-write launch is rejected before the patch check.
        launch.permission_mode = Some("read-only".into());
        assert!(preflight_build_manifest(&launch, &manifest)
            .unwrap_err()
            .contains("workspace-write"));

        std::fs::remove_file(runtime).unwrap();
    }

    #[test]
    fn preflight_build_manifest_rejects_empty_manifest_before_probe() {
        // The runtime is an empty placeholder that can never satisfy the
        // `--version`/`--dump-config` probes; reaching the empty-manifest error
        // proves the rejection happens before any probe is spawned.
        let runtime = std::env::temp_dir().join("dsh-manifest-empty-runtime.mjs");
        std::fs::write(&runtime, "").unwrap();
        let patch = std::env::temp_dir().join("dsh-manifest-empty.patch.yml");
        std::fs::write(&patch, "- id: sandbox-policy\n").unwrap();
        let mut launch = DshLaunch::new(Some(runtime.clone()), temp_workspace(), None);
        launch.permission_mode = Some("workspace-write".into());
        launch.patch = Some(patch.clone());

        let error = preflight_build_manifest(&launch, &[]).unwrap_err();
        assert!(
            error.contains("non-empty write manifest"),
            "empty manifest must fail before the probes, got: {error}"
        );

        std::fs::remove_file(runtime).unwrap();
        std::fs::remove_file(patch).unwrap();
    }
}
