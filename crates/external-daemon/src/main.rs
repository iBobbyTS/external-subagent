use external_daemon::{
    agy::AgyRuntimeFactory,
    codex::{resolve_codex_home, CodexRuntimeFactory},
    configure_diagnostic_log,
    dsh::{DshRuntimeFactory, RoutingRuntimeFactory},
    rpc::{parse_subagent_config, ServerOptions},
    zcode::{apply_provider_environment, data_root_from_environment},
    CommandRuntimeFactory, Daemon, RuntimeFactory, Scheduler, SchedulerConfig,
};
use external_runtime::SpawnModel;
use external_store::Store;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
    sync::{atomic::AtomicBool, Arc},
    thread,
    time::Duration,
};

fn configured_subagent<'a>(
    value: &'a serde_json::Value,
    name: &str,
) -> Option<&'a serde_json::Value> {
    value.pointer(&format!("/subagents/{name}"))
}
#[cfg(debug_assertions)]
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
};

struct Config {
    database: PathBuf,
    socket: PathBuf,
    runtime: Option<PathBuf>,
    diagnostic_log: Option<PathBuf>,
    agent_config: Option<PathBuf>,
    /// Resolved from the top-level `runtime_process_model` config key (I5):
    /// `auto` (default) picks detached on macOS with kqueue, otherwise
    /// attached with one diagnostic; an explicit `detached` fails startup when
    /// unavailable; an explicit `attached` keeps the pre-feature behavior.
    process_model: SpawnModel,
}

const PRODUCTION_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(90);
const PRODUCTION_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__orphan-spawn")) {
        external_runtime::orphan_spawn_main();
    }
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, Arc::clone(&shutdown_requested))?;
    signal_hook::flag::register(SIGTERM, Arc::clone(&shutdown_requested))?;
    let config = parse_config()?;
    if let Some(path) = config.agent_config.as_ref() {
        env::set_var("EXTERNAL_SUBAGENT_CONFIG", path);
        configure_dsh_environment(Some(path));
        configure_codex_environment(Some(path));
        configure_agy_environment(Some(path));
    }
    configure_diagnostic_log(config.diagnostic_log.clone());
    // The daemon is the authority for the derived zcode data root. The
    // production LaunchAgent passes `--database` as a program argument and never
    // sets EXTERNAL_SUBAGENT_STORE, so export the resolved absolute path here
    // (not a possibly-conflicting inherited value) for both provider
    // injection sites (`data_root_from_environment`).
    env::set_var("EXTERNAL_SUBAGENT_STORE", &config.database);
    wait_for_startup_test_gate(&shutdown_requested)?;
    if shutdown_requested.load(std::sync::atomic::Ordering::Acquire) {
        return Ok(());
    }
    let store = Arc::new(Store::open(&config.database)?);
    let runtime = config.runtime.clone();
    let data_root = data_root_from_environment();
    let zcode = CommandRuntimeFactory::new_prepared_with_model(
        move |_task: &external_store::TaskRecord| {
            let mut command = runtime_command(runtime.as_deref())?;
            if let (Some(runtime), Some(data_root)) = (runtime.as_deref(), data_root.as_deref()) {
                apply_provider_environment(&mut command, runtime, data_root);
            }
            Ok(command)
        },
        config.process_model,
    );
    let dsh_factory = if dsh_production_enabled(config.agent_config.as_deref()) {
        DshRuntimeFactory::enabled_with_model(config.process_model)
    } else {
        DshRuntimeFactory::closed()
    };
    let codex_factory = if codex_production_enabled(config.agent_config.as_deref()) {
        CodexRuntimeFactory::enabled_with_model(config.process_model)
    } else {
        CodexRuntimeFactory::closed()
    };
    let agy_factory = if agy_production_enabled(config.agent_config.as_deref()) {
        AgyRuntimeFactory::enabled_with_model(config.process_model)
    } else {
        AgyRuntimeFactory::closed()
    };
    let runtime_factory: Arc<dyn RuntimeFactory> = Arc::new(RoutingRuntimeFactory::with_agy(
        zcode,
        dsh_factory,
        codex_factory,
        agy_factory,
    ));
    let scheduler = Scheduler::new(
        format!("external-subagentd-{}", std::process::id()),
        store,
        runtime_factory,
        production_scheduler_config(config.runtime.clone()),
    )?;
    let daemon = match Daemon::start_with_shutdown(
        &config.socket,
        scheduler,
        ServerOptions::default(),
        Duration::from_millis(20),
        Arc::clone(&shutdown_requested),
    ) {
        Ok(daemon) => daemon,
        Err(error)
            if error.kind() == io::ErrorKind::Interrupted
                && shutdown_requested.load(std::sync::atomic::Ordering::Acquire) =>
        {
            return Ok(())
        }
        Err(error) => return Err(error.into()),
    };
    while !shutdown_requested.load(std::sync::atomic::Ordering::Acquire) {
        thread::sleep(Duration::from_millis(20));
    }
    daemon.shutdown();
    Ok(())
}

fn dsh_production_enabled(path: Option<&Path>) -> bool {
    let Some(path) = path else { return false };
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    let Ok(value) = parse_subagent_config(&bytes) else {
        return false;
    };
    let configured = configured_subagent(&value, "dsh");
    configured
        .and_then(|entry| entry.get("enabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        && configured
            .and_then(|entry| entry.get("spawn_supported"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        && configured
            .and_then(|entry| entry.get("runtime_path"))
            .and_then(serde_json::Value::as_str)
            .map(Path::new)
            .is_some_and(|runtime| {
                runtime.is_absolute()
                    && runtime.is_file()
                    && {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            fs::metadata(runtime).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
                        }
                        #[cfg(not(unix))]
                        {
                            true
                        }
                    }
                    && configured
                        .and_then(|entry| entry.get("profile"))
                        .and_then(serde_json::Value::as_str)
                        == Some("acp")
                    && configured
                        .and_then(|entry| entry.get("version"))
                        .and_then(serde_json::Value::as_str)
                        == Some(external_agent_dsh::profile::PINNED_DSH_VERSION)
            })
}

fn configure_dsh_environment(path: Option<&Path>) {
    let Some(path) = path else { return };
    let Ok(bytes) = fs::read(path) else { return };
    let Ok(value) = parse_subagent_config(&bytes) else {
        return;
    };
    let Some(entry) = configured_subagent(&value, "dsh") else {
        return;
    };
    for (field, variable) in [
        ("runtime_path", "DSH_RUNTIME_PATH"),
        ("home", "DSH_HOME"),
        ("profile", "DSH_PROFILE"),
        ("version", "DSH_VERSION"),
    ] {
        if let Some(value) = entry.get(field).and_then(serde_json::Value::as_str) {
            env::set_var(variable, value);
        }
    }
}

/// Export the persisted Codex launch contract into the daemon environment.
/// `agents.codex.home` is only exported when configured, so an inherited
/// `CODEX_HOME` stays the deliberate second-priority source and the factory
/// rejects any launch with neither (never `~/.codex`).
fn configure_codex_environment(path: Option<&Path>) {
    let Some(path) = path else { return };
    let Ok(bytes) = fs::read(path) else { return };
    let Ok(value) = parse_subagent_config(&bytes) else {
        return;
    };
    let Some(entry) = configured_subagent(&value, "codex") else {
        return;
    };
    if let Some(runtime) = entry
        .get("runtime_path")
        .and_then(serde_json::Value::as_str)
    {
        env::set_var("CODEX_RUNTIME_PATH", runtime);
    }
    let configured_home = entry.get("home").and_then(serde_json::Value::as_str);
    let inherited_home = env::var_os("CODEX_HOME");
    match resolve_codex_home(
        configured_home,
        inherited_home
            .as_ref()
            .map(|value| value.to_string_lossy().into_owned())
            .as_deref(),
    ) {
        Some(Ok(home)) => env::set_var("CODEX_HOME", home),
        // An invalid configured home fails closed: clear any inherited value
        // so the spawn gate rejects instead of silently downgrading to it.
        Some(Err(_)) => env::remove_var("CODEX_HOME"),
        // An absent home simply leaves nothing exported for the same gate.
        None => {}
    }
}

fn codex_production_enabled(path: Option<&Path>) -> bool {
    let Some(path) = path else { return false };
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    let Ok(value) = parse_subagent_config(&bytes) else {
        return false;
    };
    let configured = configured_subagent(&value, "codex");
    configured
        .and_then(|entry| entry.get("enabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        && configured
            .and_then(|entry| entry.get("spawn_supported"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        && configured
            .and_then(|entry| entry.get("runtime_path"))
            .and_then(serde_json::Value::as_str)
            .map(Path::new)
            .is_some_and(|runtime| {
                runtime.is_absolute() && runtime.is_file() && {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        fs::metadata(runtime).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
                    }
                    #[cfg(not(unix))]
                    {
                        true
                    }
                }
            })
}

/// Export the persisted agy runtime path into the daemon environment. `agy`
/// has no home override; only the absolute executable path is exported, and an
/// absent entry leaves the spawn gate closed.
fn configure_agy_environment(path: Option<&Path>) {
    let Some(path) = path else { return };
    let Ok(bytes) = fs::read(path) else { return };
    let Ok(value) = parse_subagent_config(&bytes) else {
        return;
    };
    let Some(entry) = configured_subagent(&value, "agy") else {
        return;
    };
    if let Some(runtime) = entry
        .get("runtime_path")
        .and_then(serde_json::Value::as_str)
    {
        env::set_var("AGY_RUNTIME_PATH", runtime);
    }
}

fn agy_production_enabled(path: Option<&Path>) -> bool {
    let Some(path) = path else { return false };
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    let Ok(value) = parse_subagent_config(&bytes) else {
        return false;
    };
    let configured = configured_subagent(&value, "agy");
    configured
        .and_then(|entry| entry.get("enabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        && configured
            .and_then(|entry| entry.get("spawn_supported"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        && configured
            .and_then(|entry| entry.get("runtime_path"))
            .and_then(serde_json::Value::as_str)
            .map(Path::new)
            .is_some_and(|runtime| {
                runtime.is_absolute() && runtime.is_file() && {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        fs::metadata(runtime).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
                    }
                    #[cfg(not(unix))]
                    {
                        true
                    }
                }
            })
}

fn production_scheduler_config(runtime_source: Option<PathBuf>) -> SchedulerConfig {
    SchedulerConfig {
        bootstrap_timeout: PRODUCTION_BOOTSTRAP_TIMEOUT,
        control_timeout: PRODUCTION_CONTROL_TIMEOUT,
        runtime_source,
        ..SchedulerConfig::default()
    }
}

#[cfg(debug_assertions)]
fn wait_for_startup_test_gate(shutdown_requested: &AtomicBool) -> io::Result<()> {
    let Some(path) = env::var_os("EXTERNAL_SUBAGENT_TEST_STARTUP_GATE") else {
        return Ok(());
    };
    let mut gate = UnixStream::connect(path)?;
    gate.write_all(&[1])?;
    let mut release = [0u8; 1];
    gate.read_exact(&mut release)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !shutdown_requested.load(std::sync::atomic::Ordering::Acquire) {
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "startup test gate did not observe a shutdown signal",
            ));
        }
        thread::yield_now();
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
fn wait_for_startup_test_gate(_shutdown_requested: &AtomicBool) -> io::Result<()> {
    Ok(())
}

fn parse_config() -> io::Result<Config> {
    let mut database = env::var_os("EXTERNAL_SUBAGENT_STORE").map(PathBuf::from);
    let mut socket = env::var_os("EXTERNAL_SUBAGENT_SOCKET").map(PathBuf::from);
    let mut runtime = env::var_os("ZCODE_RUNTIME_PATH").map(PathBuf::from);
    let mut diagnostic_log = None;
    let mut agent_config = env::var_os("EXTERNAL_SUBAGENT_CONFIG").map(PathBuf::from);
    let mut arguments = env::args_os().skip(1);
    while let Some(argument) = arguments.next() {
        let value = arguments.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "daemon option is missing a value",
            )
        })?;
        match argument.to_string_lossy().as_ref() {
            "--database" => database = Some(PathBuf::from(value)),
            "--socket" => socket = Some(PathBuf::from(value)),
            "--runtime" => runtime = Some(PathBuf::from(value)),
            "--diagnostic-log" => diagnostic_log = Some(absolute_path(PathBuf::from(value))?),
            "--agent-config" => agent_config = Some(absolute_path(PathBuf::from(value))?),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unknown daemon option",
                ))
            }
        }
    }
    let database = absolute_path(database.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "EXTERNAL_SUBAGENT_STORE or --database is required",
        )
    })?)?;
    let agent_config =
        agent_config.or_else(|| database.parent().map(|parent| parent.join("config.json")));
    let socket = absolute_path(socket.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "EXTERNAL_SUBAGENT_SOCKET or --socket is required",
        )
    })?)?;
    let runtime = runtime.map(fs::canonicalize).transpose()?;
    if runtime.as_ref().is_some_and(|path| !path.is_file()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "runtime path is not a regular file",
        ));
    }
    let requested = read_runtime_process_model(agent_config.as_deref())?;
    let (process_model, diagnostic) =
        resolve_runtime_process_model(requested, detached_runtime_available())
            .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    if let Some(diagnostic) = diagnostic {
        eprintln!("external-subagentd: {diagnostic}");
    }
    Ok(Config {
        database,
        socket,
        runtime,
        diagnostic_log,
        agent_config,
        process_model,
    })
}

/// The persisted top-level enum shared with cli/config/schema.mjs and
/// rpc/config.rs. Parsing is separated from resolution so the decision table
/// stays a pure function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeProcessModel {
    Auto,
    Detached,
    Attached,
}

impl RuntimeProcessModel {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "detached" => Some(Self::Detached),
            "attached" => Some(Self::Attached),
            _ => None,
        }
    }
}

const RUNTIME_PROCESS_MODEL_VALUES: &str = "auto, detached, attached";

/// Pure decision table (I5). `detached_available` is injected so both arms are
/// testable: production derives it from the host (macOS with a working
/// kqueue). `auto` degrades to attached with one diagnostic; an explicit
/// `detached` request on an incapable host is a loud startup failure.
fn resolve_runtime_process_model(
    requested: RuntimeProcessModel,
    detached_available: bool,
) -> Result<(SpawnModel, Option<String>), String> {
    match (requested, detached_available) {
        (RuntimeProcessModel::Attached, _) => Ok((SpawnModel::Attached, None)),
        (RuntimeProcessModel::Detached, true) => Ok((SpawnModel::Detached, None)),
        (RuntimeProcessModel::Detached, false) => Err(format!(
            "runtime_process_model=detached is unavailable: this host needs macOS with kqueue"
        )),
        (RuntimeProcessModel::Auto, true) => Ok((SpawnModel::Detached, None)),
        (RuntimeProcessModel::Auto, false) => Ok((
            SpawnModel::Attached,
            Some("runtime_process_model=auto: detached is unavailable; using attached".to_owned()),
        )),
    }
}

/// Host capability for detached spawn: macOS plus a usable kqueue descriptor.
/// On other targets detached is not compiled in, so auto stays attached.
fn detached_runtime_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return false;
        }
        unsafe { libc::close(fd) };
        true
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Read the top-level key from the same config.json main already resolves
/// (database directory, mirroring cli/paths.mjs). Absent/unreadable/malformed
/// configurations keep the `auto` default; the value enum itself is strict so
/// a hand-written invalid value fails startup by key name instead of silently
/// downgrading.
fn read_runtime_process_model(path: Option<&Path>) -> io::Result<RuntimeProcessModel> {
    let Some(path) = path else {
        return Ok(RuntimeProcessModel::Auto);
    };
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RuntimeProcessModel::Auto)
        }
        // Read failures are handled fail-closed by the per-agent gates; do not
        // turn a transient unreadable config into a startup crash here.
        Err(_) => return Ok(RuntimeProcessModel::Auto),
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(RuntimeProcessModel::Auto);
    };
    let Some(raw) = value.get("runtime_process_model") else {
        return Ok(RuntimeProcessModel::Auto);
    };
    raw.as_str()
        .and_then(RuntimeProcessModel::parse)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "config runtime_process_model must be one of {RUNTIME_PROCESS_MODEL_VALUES}"
                ),
            )
        })
}

fn absolute_path(path: PathBuf) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn runtime_command(runtime: Option<&Path>) -> io::Result<Command> {
    let runtime = runtime.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "ZCODE_RUNTIME_PATH is unavailable",
        )
    })?;
    if matches!(
        runtime.extension().and_then(|value| value.to_str()),
        Some("js" | "cjs" | "mjs")
    ) {
        let mut command = Command::new("node");
        command.arg(runtime).arg("app-server");
        Ok(command)
    } else {
        Ok(Command::new(runtime))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn production_scheduler_allows_the_verified_official_runtime_bootstrap_window() {
        let defaults = SchedulerConfig::default();
        let production = production_scheduler_config(None);

        assert_eq!(production.bootstrap_timeout, Duration::from_secs(90));
        assert_eq!(production.control_timeout, Duration::from_secs(5));
        assert_eq!(
            production.per_workspace_max_agents,
            defaults.per_workspace_max_agents
        );
        assert_eq!(production.stop_grace, defaults.stop_grace);
        assert!(production.runtime_source.is_none());
    }

    #[test]
    fn codex_production_gate_requires_enabled_spawn_and_pinned_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("codex-runtime");
        std::fs::File::create(&runtime)
            .unwrap()
            .write_all(b"runtime")
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut mode = std::fs::metadata(&runtime).unwrap().permissions();
            mode.set_mode(0o755);
            std::fs::set_permissions(&runtime, mode).unwrap();
        }
        let config_path = directory.path().join("config.json");
        let base = serde_json::json!({"agents":{"codex":{
            "enabled":true,"spawn_supported":true,
            "runtime_path":runtime,"home":"/persisted/codex-home"}}});
        std::fs::write(&config_path, serde_json::to_vec(&base).unwrap()).unwrap();
        assert!(codex_production_enabled(Some(&config_path)));
        for field in ["enabled", "spawn_supported"] {
            let mut value = base.clone();
            value["agents"]["codex"][field] = serde_json::json!(false);
            std::fs::write(&config_path, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(!codex_production_enabled(Some(&config_path)));
        }
        let mut relative = base.clone();
        relative["agents"]["codex"]["runtime_path"] = serde_json::json!("relative/codex");
        std::fs::write(&config_path, serde_json::to_vec(&relative).unwrap()).unwrap();
        assert!(!codex_production_enabled(Some(&config_path)));
        assert!(!codex_production_enabled(None));
    }

    #[test]
    fn codex_environment_exports_configured_home_over_inherited() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({"agents":{"codex":{
                "runtime_path":"/opt/codex","home":"/persisted/codex-home"}}}))
            .unwrap(),
        )
        .unwrap();
        let previous = env::var_os("CODEX_RUNTIME_PATH");
        let previous_home = env::var_os("CODEX_HOME");
        env::set_var("CODEX_RUNTIME_PATH", "/opt/codex");
        env::set_var("CODEX_HOME", "/inherited/codex-home");
        configure_codex_environment(Some(&config_path));
        assert_eq!(env::var_os("CODEX_HOME").unwrap(), "/persisted/codex-home");
        // An invalid configured home clears the inherited value so the spawn
        // gate fails closed instead of downgrading to it.
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({"agents":{"codex":{
                "runtime_path":"/opt/codex","home":"relative-home"}}}))
            .unwrap(),
        )
        .unwrap();
        configure_codex_environment(Some(&config_path));
        assert_eq!(env::var_os("CODEX_HOME"), None);
        match previous {
            Some(value) => env::set_var("CODEX_RUNTIME_PATH", value),
            None => env::remove_var("CODEX_RUNTIME_PATH"),
        }
        match previous_home {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
    }

    #[test]
    fn dsh_production_gate_requires_complete_persisted_identity() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("dsh-runtime");
        std::fs::File::create(&runtime)
            .unwrap()
            .write_all(b"runtime")
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut mode = std::fs::metadata(&runtime).unwrap().permissions();
            mode.set_mode(0o755);
            std::fs::set_permissions(&runtime, mode).unwrap();
        }
        let config_path = directory.path().join("config.json");
        let base = serde_json::json!({"agents":{"dsh":{"enabled":true,"spawn_supported":true,"runtime_path":runtime,"profile":"acp","version":"0.1.5-rc.1"}}});
        std::fs::write(&config_path, serde_json::to_vec(&base).unwrap()).unwrap();
        assert!(dsh_production_enabled(Some(&config_path)));
        for field in ["profile", "version"] {
            let mut value = base.clone();
            value["agents"]["dsh"]
                .as_object_mut()
                .unwrap()
                .remove(field);
            std::fs::write(&config_path, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(!dsh_production_enabled(Some(&config_path)));
        }
    }

    #[test]
    fn runtime_process_model_decision_table_covers_both_injected_arms() {
        // auto + capable host → detached.
        assert_eq!(
            resolve_runtime_process_model(RuntimeProcessModel::Auto, true).unwrap(),
            (SpawnModel::Detached, None)
        );
        // auto + incapable host → attached with exactly one diagnostic.
        let (model, diagnostic) =
            resolve_runtime_process_model(RuntimeProcessModel::Auto, false).unwrap();
        assert_eq!(model, SpawnModel::Attached);
        let diagnostic = diagnostic.expect("auto fallback must carry a diagnostic");
        assert!(diagnostic.contains("auto") && diagnostic.contains("attached"));
        // explicit detached + capable → detached.
        assert_eq!(
            resolve_runtime_process_model(RuntimeProcessModel::Detached, true).unwrap(),
            (SpawnModel::Detached, None)
        );
        // explicit detached + incapable → loud failure.
        let error = resolve_runtime_process_model(RuntimeProcessModel::Detached, false)
            .expect_err("explicit detached must fail loudly");
        assert!(error.contains("detached"));
        // explicit attached is the pre-feature behavior regardless of host.
        for available in [true, false] {
            assert_eq!(
                resolve_runtime_process_model(RuntimeProcessModel::Attached, available).unwrap(),
                (SpawnModel::Attached, None)
            );
        }
    }

    #[test]
    fn runtime_process_model_config_key_is_read_strictly() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.json");

        // Missing path and absent key both mean auto.
        assert_eq!(
            read_runtime_process_model(None).unwrap(),
            RuntimeProcessModel::Auto
        );
        std::fs::write(&config_path, br#"{"schema_version":2}"#).unwrap();
        assert_eq!(
            read_runtime_process_model(Some(&config_path)).unwrap(),
            RuntimeProcessModel::Auto
        );
        // Malformed JSON does not crash startup; the per-agent gates fail closed.
        std::fs::write(&config_path, b"{not json").unwrap();
        assert_eq!(
            read_runtime_process_model(Some(&config_path)).unwrap(),
            RuntimeProcessModel::Auto
        );
        // Each allowed value round-trips.
        for (raw, expected) in [
            ("auto", RuntimeProcessModel::Auto),
            ("detached", RuntimeProcessModel::Detached),
            ("attached", RuntimeProcessModel::Attached),
        ] {
            std::fs::write(
                &config_path,
                serde_json::to_vec(&serde_json::json!({
                    "schema_version": 2,
                    "runtime_process_model": raw,
                }))
                .unwrap(),
            )
            .unwrap();
            assert_eq!(
                read_runtime_process_model(Some(&config_path)).unwrap(),
                expected
            );
        }
        // Invalid or non-string values fail by key name.
        std::fs::write(
            &config_path,
            br#"{"schema_version":2,"runtime_process_model":"sometimes"}"#,
        )
        .unwrap();
        let error = read_runtime_process_model(Some(&config_path)).unwrap_err();
        assert!(error.to_string().contains("runtime_process_model"));
        std::fs::write(
            &config_path,
            br#"{"schema_version":2,"runtime_process_model":7}"#,
        )
        .unwrap();
        let error = read_runtime_process_model(Some(&config_path)).unwrap_err();
        assert!(error.to_string().contains("runtime_process_model"));
    }
}
