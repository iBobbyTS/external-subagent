use external_daemon::{
    configure_diagnostic_log,
    dsh::{DshRuntimeFactory, RoutingRuntimeFactory},
    rpc::ServerOptions,
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
    // Production composition registers the DSH adapter behind a closed spawn
    // gate: admission can know about the agent while spawn stays refused until
    // the S04 parent is accepted.
    let runtime_factory: Arc<dyn RuntimeFactory> = Arc::new(RoutingRuntimeFactory::new(
        zcode,
        DshRuntimeFactory::closed(),
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

    #[test]
    fn production_scheduler_allows_the_verified_official_runtime_bootstrap_window() {
        let defaults = SchedulerConfig::default();
        let production = production_scheduler_config(None);

        assert_eq!(production.bootstrap_timeout, Duration::from_secs(90));
        assert_eq!(production.control_timeout, Duration::from_secs(5));
        assert_eq!(production.global_max_agents, defaults.global_max_agents);
        assert_eq!(
            production.per_workspace_max_agents,
            defaults.per_workspace_max_agents
        );
        assert_eq!(production.stop_grace, defaults.stop_grace);
        assert!(production.runtime_source.is_none());
    }
}
