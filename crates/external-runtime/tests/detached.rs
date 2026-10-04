#![cfg(all(target_os = "macos", debug_assertions))]
use external_contract::WireMessage;
use external_runtime::{
    gated_spawn, observe_process, observe_process_group, stop_and_reap_persisted_process_group,
    ChildExit, DetachedTestOptions, Driver, FrameCodec, Inbound, SpawnModel, StopOutcome, TestGate,
};
use std::{
    fs, io,
    os::unix::ffi::OsStringExt,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};
const BIN: &str = env!("CARGO_BIN_EXE_orphan-spawn-test");
const BOUND: Duration = Duration::from_secs(10);
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "es-detached-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn options() -> DetachedTestOptions {
    DetachedTestOptions {
        helper: Some(BIN.into()),
        ..Default::default()
    }
}
fn fixture(mode: &str) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.arg(mode);
    cmd
}
fn spawn(cmd: Command, model: SpawnModel, hooks: DetachedTestOptions) -> io::Result<Driver> {
    Driver::spawn_for_test(cmd, FrameCodec::Ndjson, model, hooks)
}
fn detached(cmd: Command) -> Driver {
    spawn(cmd, SpawnModel::Detached, options()).unwrap()
}
fn error(cmd: Command, hooks: DetachedTestOptions) -> String {
    match spawn(cmd, SpawnModel::Detached, hooks) {
        Ok(_) => panic!("expected spawn failure"),
        Err(e) => e.to_string(),
    }
}
fn event(driver: &Driver) -> Inbound {
    driver.recv_timeout(BOUND).unwrap()
}
fn exit_once(driver: &Driver, expected: ChildExit) {
    loop {
        if let Inbound::ChildExited(exit) = event(driver) {
            assert_eq!(exit, expected);
            break;
        }
    }
    assert_eq!(
        driver.wait().unwrap(),
        match expected {
            ChildExit::Exited(c) => c,
            _ => None,
        }
    );
    assert!(
        driver.recv_timeout(Duration::from_millis(80)).is_err(),
        "duplicate exit event"
    );
}
fn assert_gate_reusable() {
    let d = detached(fixture("__fixture"));
    d.send(&serde_json::json!({})).unwrap();
    exit_once(&d, ChildExit::Exited(Some(0)));
}
fn trace_pid(path: &Path, label: &str) -> u32 {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix(label).and_then(|p| p.parse().ok()))
        .unwrap()
}
fn kill_residual(path: &Path) {
    let pid = trace_pid(path, "runtime:");
    if let Ok(identity) = observe_process(pid) {
        stop_and_reap_persisted_process_group(&identity, Duration::from_secs(1)).unwrap();
    }
}
fn wait_until(mut test: impl FnMut() -> bool) {
    let deadline = Instant::now() + BOUND;
    while !test() {
        assert!(Instant::now() < deadline, "bounded condition timed out");
        thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn orphan_identity_fd_eof_and_diagnostic_tail() {
    let mut cmd = fixture("__terminal");
    cmd.arg("7");
    let driver = detached(cmd);
    let id = driver.identity();
    assert_eq!(id.pgid, id.pid as i32);
    wait_until(|| unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        libc::proc_pidinfo(
            id.pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            std::mem::size_of_val(&info) as i32,
        ) > 0
            && info.pbi_ppid == 1
    });
    let members = observe_process_group(id.pgid).unwrap();
    assert_eq!(members, vec![id]);
    driver.send(&serde_json::json!({})).unwrap();
    exit_once(&driver, ChildExit::Exited(Some(7)));
    let tail = driver.diagnostic_tail();
    assert!(tail.len() <= 16 * 1024);
    assert!(tail.ends_with("TAIL"));
    let d = detached(fixture("__fixture"));
    d.send(&serde_json::json!({})).unwrap();
    match event(&d) {
        Inbound::Message(WireMessage::UnknownEvent { raw, .. }) => {
            assert_eq!(raw["fds"], serde_json::json!([]))
        }
        other => panic!("{other:?}"),
    }
    exit_once(&d, ChildExit::Exited(Some(0)));
}
#[test]
fn observe_notfound_reap_reports_exact_fast_exit() {
    let mut hooks = options();
    hooks.before_observe_delay = Duration::from_millis(150);
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "exit 37"]);
    assert!(error(cmd, hooks).contains("Exited(Some(37))"));
    assert_gate_reusable();
}
#[test]
fn live_observe_then_esrch_reap_reports_exact_exit() {
    let temp = Temp::new();
    let release = temp.path("release");
    let trace = temp.path("trace");
    let mut hooks = options();
    hooks.trace = Some(trace.clone());
    hooks.before_register_delay = Duration::from_millis(350);
    let mut cmd = Command::new("/bin/sh");
    cmd.env("RELEASE", &release).args([
        "-c",
        "while [ ! -f \"$RELEASE\" ]; do sleep 0.005; done; exit 29",
    ]);
    let worker = thread::spawn(move || error(cmd, hooks));
    wait_until(|| {
        fs::read_to_string(&trace)
            .unwrap_or_default()
            .contains("observed")
    });
    fs::write(release, "").unwrap();
    assert!(worker.join().unwrap().contains("Exited(Some(29))"));
    assert!(fs::read_to_string(trace)
        .unwrap()
        .contains("directive:REAP"));
}
#[test]
fn nack_success_cleans_only_validated_identity() {
    let temp = Temp::new();
    let trace = temp.path("trace");
    let mut hooks = options();
    hooks.force_nack = true;
    hooks.trace = Some(trace.clone());
    let diagnostic = error(fixture("__linger"), hooks);
    assert!(diagnostic.contains("identity cleanup=Ok"), "{diagnostic}");
    let pid = trace_pid(&trace, "runtime:");
    assert!(observe_process(pid).is_err());
    assert!(fs::read_to_string(trace)
        .unwrap()
        .contains("directive:NACK"));
    assert_gate_reusable();
}
#[test]
fn parent_close_pause_reap_is_directive_write_barrier() {
    let temp = Temp::new();
    let trace = temp.path("trace");
    let mut hooks = options();
    hooks.trace = Some(trace.clone());
    hooks
        .helper_env
        .insert("ES_HELPER_TEST_PAUSE".into(), "400".into());
    let start = Instant::now();
    let d = spawn(fixture("__fixture"), SpawnModel::Detached, hooks).unwrap();
    assert!(start.elapsed() >= Duration::from_millis(400));
    let log = fs::read_to_string(&trace).unwrap();
    assert!(log.find("parent_closing").unwrap() < log.find("parent_reaped").unwrap());
    assert!(log.find("parent_reaped").unwrap() < log.find("directive:ACK").unwrap());
    d.send(&serde_json::json!({})).unwrap();
    exit_once(&d, ChildExit::Exited(Some(0)));
    assert_gate_reusable();
}
#[test]
fn parent_timeout_sigkill_rewait_error_and_gate_reuse() {
    let temp = Temp::new();
    let trace = temp.path("trace");
    let mut hooks = options();
    hooks.trace = Some(trace.clone());
    hooks
        .helper_env
        .insert("ES_HELPER_TEST_PAUSE".into(), "3000".into());
    let diagnostic = error(fixture("__linger"), hooks);
    assert!(
        diagnostic.contains("SIGKILL=Ok(())") && diagnostic.contains("re-wait=Ok(true)"),
        "{diagnostic}"
    );
    let parent = trace_pid(&trace, "parent:");
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(parent as i32, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    let log = fs::read_to_string(&trace).unwrap();
    assert!(log.contains("parent_SIGKILL"));
    assert!(!log.contains("directive:"));
    wait_until(|| fs::read_to_string(&trace).unwrap().contains("helper_eof"));
    kill_residual(&trace);
    assert_gate_reusable();
}
#[test]
fn ack_deadline_after_parent_reap_epipe_never_authorizes_cleanup() {
    let temp = Temp::new();
    let trace = temp.path("trace");
    let mut hooks = options();
    hooks.trace = Some(trace.clone());
    hooks.after_parent_reap_delay = Duration::from_millis(10_300);
    let diagnostic = error(fixture("__linger"), hooks);
    assert!(
        diagnostic.contains("helper lost") && diagnostic.contains("no cleanup authorization"),
        "{diagnostic}"
    );
    assert!(
        observe_process(trace_pid(&trace, "runtime:")).is_ok(),
        "EPIPE must not kill runtime"
    );
    let log = fs::read_to_string(&trace).unwrap();
    assert!(log.contains("helper_timeout"));
    assert!(!log.contains("directive:"));
    kill_residual(&trace);
    assert_gate_reusable();
}
#[test]
fn first_frame_timeout_late_frame_and_crash_fail_without_hanging() {
    for crash in [false, true] {
        let temp = Temp::new();
        let trace = temp.path("trace");
        let mut hooks = options();
        hooks.trace = Some(trace.clone());
        if crash {
            hooks
                .helper_env
                .insert("ES_HELPER_TEST_CRASH_BEFORE_FRAME".into(), "1".into());
        } else {
            hooks.handshake_timeout = Some(Duration::from_millis(80));
            hooks
                .helper_env
                .insert("ES_HELPER_TEST_FRAME_DELAY".into(), "250".into());
        }
        let diagnostic = error(fixture("__linger"), hooks);
        assert!(
            diagnostic.contains("helper lost") && diagnostic.contains("runtime may remain"),
            "{diagnostic}"
        );
        assert!(observe_process(trace_pid(&trace, "runtime:")).is_ok());
        thread::sleep(Duration::from_millis(300));
        assert!(!fs::read_to_string(&trace).unwrap().contains("directive:"));
        kill_residual(&trace);
        assert_gate_reusable();
    }
}
#[test]
fn reap_reply_timeout_late_frame_and_eof_have_no_exit_code() {
    for eof in [false, true] {
        let temp = Temp::new();
        let trace = temp.path("trace");
        let mut hooks = options();
        hooks.trace = Some(trace.clone());
        hooks.before_observe_delay = Duration::from_millis(100);
        hooks.reap_reply_timeout = Some(Duration::from_millis(80));
        hooks.helper_env.insert(
            if eof {
                "ES_HELPER_TEST_REAP_EOF"
            } else {
                "ES_HELPER_TEST_REAP_DELAY"
            }
            .into(),
            if eof { "1" } else { "250" }.into(),
        );
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exit 23"]);
        let diagnostic = error(cmd, hooks);
        assert!(
            diagnostic.contains("REAP reply failed") && diagnostic.contains("no exit code"),
            "{diagnostic}"
        );
        assert_gate_reusable();
    }
}
#[test]
fn command_spec_rejects_non_utf8_oversize_nul_and_invalid_cwd() {
    let mut cmd = fixture("__fixture");
    cmd.arg(std::ffi::OsString::from_vec(vec![0xff]));
    assert!(error(cmd, options()).contains("non-UTF-8"));
    let mut cmd = fixture("__fixture");
    cmd.arg("x".repeat(256 * 1024));
    assert!(error(cmd, options()).contains("256KB"));
    let mut cmd = fixture("__fixture");
    cmd.current_dir("/nonexistent/es-cwd");
    assert!(error(cmd, options()).contains("helper spawn"));
    let mut cmd = fixture("__fixture");
    cmd.arg("a\0b");
    assert!(error(cmd, options()).contains("NUL"));
    assert_gate_reusable();
}
fn run_fixture(mut cmd: Command, model: SpawnModel) -> serde_json::Value {
    cmd.arg("__fixture")
        .args(["space arg", "你好", "{\"policy\":true}"]);
    let d = spawn(cmd, model, options()).unwrap();
    d.send(&serde_json::json!({})).unwrap();
    let value = match event(&d) {
        Inbound::Message(WireMessage::UnknownEvent { raw, .. }) => raw,
        other => panic!("{other:?}"),
    };
    exit_once(&d, ChildExit::Exited(Some(0)));
    value
}
#[test]
fn command_environment_argv_cwd_json_delta_matches_attached() {
    let temp = Temp::new();
    let policy = "{\"sandbox\":\"workspace-write\",\"approval\":\"never\",\"items\":[1,2]}";
    let build = || {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(&temp.0)
            .env("ES_TEST_OVERRIDE", "overridden")
            .env_remove("OPENAI_API_KEY")
            .env("ES_TEST_EMPTY", "")
            .env("ES_TEST_POLICY", policy);
        cmd
    };
    let a = run_fixture(build(), SpawnModel::Attached);
    let b = run_fixture(build(), SpawnModel::Detached);
    assert_eq!(a, b);
    assert_eq!(b["env"]["HOME"], std::env::var("HOME").unwrap());
    assert_eq!(b["env"]["ES_TEST_OVERRIDE"], "overridden");
    assert!(b["env"]["OPENAI_API_KEY"].is_null());
    assert_eq!(b["env"]["ES_TEST_EMPTY"], "");
    assert_eq!(b["env"]["ES_TEST_POLICY"], policy);
}
#[test]
fn path_six_cases_match_attached() {
    let temp = Temp::new();
    let bin = temp.path("bin");
    fs::create_dir(&bin).unwrap();
    symlink(BIN, bin.join("spec-probe")).unwrap();
    // Absolute, cwd, spec PATH conflicting with inherited caller PATH, relative PATH + cwd.
    for case in 0..4 {
        let build = || {
            let mut cmd = Command::new(if case < 2 { BIN } else { "spec-probe" });
            if case == 1 || case == 3 {
                cmd.current_dir(&temp.0);
            }
            if case == 2 {
                cmd.env("PATH", &bin);
            }
            if case == 3 {
                cmd.env("PATH", "bin");
            }
            cmd
        };
        assert_eq!(
            run_fixture(build(), SpawnModel::Attached),
            run_fixture(build(), SpawnModel::Detached)
        );
    }
    // PATH deletion uses execvp/spawnp default search; empty PATH searches cwd.
    for empty in [false, true] {
        for model in [SpawnModel::Attached, SpawnModel::Detached] {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "read line; exit 9"]);
            if empty {
                cmd.env("PATH", "").current_dir(&temp.0);
            } else {
                cmd.env_remove("PATH");
            }
            let result = spawn(cmd, model, options());
            if empty {
                assert!(result.is_err());
            } else {
                let d = result.unwrap();
                d.send(&serde_json::json!({})).unwrap();
                exit_once(&d, ChildExit::Exited(Some(9)));
            }
        }
    }
    // Actual bare node through inherited PATH, as used by production builders.
    for model in [SpawnModel::Attached, SpawnModel::Detached] {
        let mut cmd = Command::new("node");
        cmd.args(["-e","process.stdin.once('data',()=>{console.log(JSON.stringify({event:'node'}));process.exit(0)})"]);
        let d = spawn(cmd, model, options()).unwrap();
        d.send(&serde_json::json!({})).unwrap();
        exit_once(&d, ChildExit::Exited(Some(0)));
    }
}
#[test]
fn sigpipe_default_matches_attached() {
    for model in [SpawnModel::Attached, SpawnModel::Detached] {
        let d = spawn(fixture("__fixture"), model, options()).unwrap();
        assert_eq!(
            unsafe { libc::kill(d.identity().pid as i32, libc::SIGPIPE) },
            0
        );
        exit_once(&d, ChildExit::Signaled(13));
    }
}
#[test]
fn terminal_frame_before_exit_under_reader_and_monitor_interleavings() {
    for model in [SpawnModel::Attached, SpawnModel::Detached] {
        for reader_first in [false, true] {
            let reader = Arc::new(TestGate::default());
            let monitor = Arc::new(TestGate::default());
            let mut hooks = options();
            hooks.reader_start_gate = Some(reader.clone());
            hooks.monitor_gate = Some(monitor.clone());
            let mut cmd = fixture("__terminal");
            cmd.arg("7");
            let d = spawn(cmd, model, hooks).unwrap();
            assert!(reader.wait_arrived(BOUND));
            d.send(&serde_json::json!({})).unwrap();
            assert!(monitor.wait_arrived(BOUND));
            if reader_first {
                reader.release();
                assert!(matches!(event(&d), Inbound::Message(_)));
                monitor.release();
            } else {
                monitor.release();
                thread::sleep(Duration::from_millis(50));
                assert!(d.recv_timeout(Duration::from_millis(30)).is_err());
                reader.release();
                assert!(matches!(event(&d), Inbound::Message(_)));
            }
            exit_once(&d, ChildExit::Exited(Some(7)));
        }
    }
}
#[test]
fn terminal_boundary_is_bounded_when_reader_never_finishes() {
    for model in [SpawnModel::Attached, SpawnModel::Detached] {
        let reader = Arc::new(TestGate::default());
        let mut hooks = options();
        hooks.reader_gate = Some(reader.clone());
        let mut cmd = fixture("__terminal");
        cmd.arg("7");
        let d = spawn(cmd, model, hooks).unwrap();
        d.send(&serde_json::json!({})).unwrap();
        assert!(reader.wait_arrived(BOUND));
        let start = Instant::now();
        exit_once(&d, ChildExit::Exited(Some(7)));
        assert!(start.elapsed() < Duration::from_millis(1300));
        reader.release();
    }
}
#[test]
fn concurrent_stop_exact_signal_once_and_identity_refusal() {
    for model in [SpawnModel::Attached, SpawnModel::Detached] {
        let mut d = spawn(fixture("__ignore_term"), model, options()).unwrap();
        assert!(matches!(event(&d), Inbound::Message(_)));
        let actual = d.identity();
        let mut reused = actual.clone();
        reused.start_token.push_str(":reused");
        d.replace_identity_for_test(reused);
        assert!(d
            .stop_and_reap(Duration::from_millis(80))
            .unwrap_err()
            .to_string()
            .contains("identity changed"));
        assert!(observe_process(actual.pid).is_ok());
        d.replace_identity_for_test(actual);
        let d = Arc::new(d);
        let subscribed = d.subscribe();
        let mut workers = Vec::new();
        for _ in 0..4 {
            let d = d.clone();
            workers.push(thread::spawn(move || {
                d.stop_and_reap(Duration::from_millis(80)).unwrap()
            }));
        }
        for w in workers {
            assert_eq!(
                w.join().unwrap(),
                StopOutcome::Terminated(ChildExit::Signaled(9))
            );
        }
        assert_eq!(
            subscribed.recv_timeout(BOUND).unwrap(),
            Inbound::ChildExited(ChildExit::Signaled(9))
        );
        assert!(subscribed.recv_timeout(Duration::from_millis(50)).is_err());
        exit_once(&d, ChildExit::Signaled(9));
    }
}
#[test]
fn detached_stop_unknown_does_not_steal_or_duplicate_late_monitor_event() {
    let monitor = Arc::new(TestGate::default());
    let mut hooks = options();
    hooks.monitor_gate = Some(monitor.clone());
    let d = Arc::new(spawn(fixture("__ignore_term"), SpawnModel::Detached, hooks).unwrap());
    assert!(matches!(event(&d), Inbound::Message(_)));
    let stop = d.clone();
    let worker = thread::spawn(move || stop.stop_and_reap(Duration::from_millis(80)).unwrap());
    assert!(monitor.wait_arrived(BOUND));
    assert_eq!(
        worker.join().unwrap(),
        StopOutcome::Terminated(ChildExit::Unknown)
    );
    assert!(d.diagnostic_tail().contains("Unknown"));
    monitor.release();
    exit_once(&d, ChildExit::Signaled(9));
}
#[test]
fn natural_exit_concurrent_stop_and_drop_stop_keep_one_terminal_publication() {
    for model in [SpawnModel::Attached, SpawnModel::Detached] {
        // Stop observes group death while monitor is deliberately delayed.
        let monitor = Arc::new(TestGate::default());
        let mut hooks = options();
        hooks.monitor_gate = Some(monitor.clone());
        let mut cmd = fixture("__terminal");
        cmd.arg("7");
        let d = Arc::new(spawn(cmd, model, hooks).unwrap());
        d.send(&serde_json::json!({})).unwrap();
        assert!(monitor.wait_arrived(BOUND));
        let waiter = d.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            tx.send(waiter.wait()).unwrap();
        });
        let stop = d.clone();
        let worker = thread::spawn(move || stop.stop_and_reap(Duration::from_millis(80)).unwrap());
        thread::sleep(Duration::from_millis(30));
        monitor.release();
        assert_eq!(rx.recv_timeout(BOUND).unwrap().unwrap(), Some(7));
        assert_eq!(
            worker.join().unwrap(),
            StopOutcome::AlreadyExited(ChildExit::Exited(Some(7)))
        );
        exit_once(&d, ChildExit::Exited(Some(7)));
        // Drop has no Driver left to inspect: subscribers are the publication oracle.
        let d = spawn(fixture("__ignore_term"), model, options()).unwrap();
        assert!(matches!(event(&d), Inbound::Message(_)));
        let rx = d.subscribe();
        drop(d);
        assert_eq!(
            rx.recv_timeout(BOUND).unwrap(),
            Inbound::ChildExited(ChildExit::Signaled(9))
        );
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    }
}
#[test]
fn sixteen_parallel_detached_attached_and_gated_probes_preserve_eof() {
    let workers: Vec<_> = (0..16)
        .map(|i| {
            thread::spawn(move || {
                for _ in 0..3 {
                    let model = if i % 2 == 0 {
                        SpawnModel::Detached
                    } else {
                        SpawnModel::Attached
                    };
                    let d = spawn(fixture("__fixture"), model, options()).unwrap();
                    d.send(&serde_json::json!({})).unwrap();
                    exit_once(&d, ChildExit::Exited(Some(0)));
                    let mut cmd = Command::new("/bin/sh");
                    cmd.args(["-c", "exit 0"]);
                    assert!(gated_spawn(&mut cmd).unwrap().wait().unwrap().success());
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_gate_reusable();
}
