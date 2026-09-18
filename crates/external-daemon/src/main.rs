use external_daemon::{
    codex::{resolve_codex_home, CodexRuntimeFactory},
    configure_diagnostic_log,
    dsh::{DshRuntimeFactory, RoutingRuntimeFactory},
    rpc::{parse_subagent_config, ServerOptions},
    CommandRuntimeFactory, Daemon, RuntimeFactory, Scheduler, SchedulerConfig,
};
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
}

const PRODUCTION_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(90);
const PRODUCTION_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, Arc::clone(&shutdown_requested))?;
    signal_hook::flag::register(SIGTERM, Arc::clone(&shutdown_requested))?;
    let config = parse_config()?;
    if let Some(path) = config.agent_config.as_ref() {
        env::set_var("EXTERNAL_SUBAGENT_CONFIG", path);
        configure_dsh_environment(Some(path));
        configure_codex_environment(Some(path));
    }
    configure_diagnostic_log(config.diagnostic_log.clone());
    wait_for_startup_test_gate(&shutdown_requested)?;
    if shutdown_requested.load(std::sync::atomic::Ordering::Acquire) {
        return Ok(());
    }
    let store = Arc::new(Store::open(&config.database)?);
    let runtime = config.runtime.clone();
    let zcode = CommandRuntimeFactory::new_prepared(move |_task: &external_store::TaskRecord| {
        runtime_command(runtime.as_deref())
    });
    let dsh_factory = if dsh_production_enabled(config.agent_config.as_deref()) {
        DshRuntimeFactory::enabled()
    } else {
        DshRuntimeFactory::closed()
    };
    let codex_factory = if codex_production_enabled(config.agent_config.as_deref()) {
        CodexRuntimeFactory::enabled()
    } else {
        CodexRuntimeFactory::closed()
    };
    let runtime_factory: Arc<dyn RuntimeFactory> = Arc::new(RoutingRuntimeFactory::with_codex(
        zcode,
        dsh_factory,
        codex_factory,
    ));
    let scheduler = Scheduler::new(
        format!("agentd-{}", std::process::id()),
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
    let Some(path) = env::var_os("ZCODE_AGENTD_TEST_STARTUP_GATE") else {
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
    let mut database = env::var_os("ZCODE_AGENTD_STORE").map(PathBuf::from);
    let mut socket = env::var_os("ZCODE_AGENTD_SOCKET").map(PathBuf::from);
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
            "ZCODE_AGENTD_STORE or --database is required",
        )
    })?)?;
    let agent_config =
        agent_config.or_else(|| database.parent().map(|parent| parent.join("config.json")));
    let socket = absolute_path(socket.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "ZCODE_AGENTD_SOCKET or --socket is required",
        )
    })?)?;
    let runtime = runtime.map(fs::canonicalize).transpose()?;
    if runtime.as_ref().is_some_and(|path| !path.is_file()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "runtime path is not a regular file",
        ));
    }
    Ok(Config {
        database,
        socket,
        runtime,
        diagnostic_log,
        agent_config,
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
}
