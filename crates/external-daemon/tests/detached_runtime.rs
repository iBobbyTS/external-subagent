//! Daemon-level detached runtime coverage (AC6).
//!
//! The real `external-subagentd` binary is used as the hidden `__orphan-spawn`
//! helper, so this exercises the production helper entry (the same
//! `current_exe()` the daemon passes in production) rather than the
//! external-runtime test fixture. S01's `spawn_for_test` supplies only the
//! helper path; the spawn protocol itself is the production one.
#![cfg(all(target_os = "macos", debug_assertions))]

use external_runtime::{
    observe_process_group, ChildExit, DetachedTestOptions, Driver, FrameCodec, Inbound, SpawnModel,
};
use std::{
    process::Command,
    time::{Duration, Instant},
};

const DAEMON: &str = env!("CARGO_BIN_EXE_external-subagentd");
const BOUND: Duration = Duration::from_secs(10);

fn spawn_detached(command: Command) -> Driver {
    let options = DetachedTestOptions {
        helper: Some(DAEMON.into()),
        ..Default::default()
    };
    Driver::spawn_for_test(command, FrameCodec::Ndjson, SpawnModel::Detached, options)
        .expect("detached spawn through the daemon helper")
}

fn wait_terminal(driver: &Driver) -> ChildExit {
    let deadline = Instant::now() + BOUND;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("terminal event within bound");
        if let Inbound::ChildExited(exit) = driver.recv_timeout(remaining).expect("terminal event")
        {
            return exit;
        }
    }
}

#[test]
fn daemon_helper_detached_runtime_reports_exact_terminal() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "read line; exit 7"]);
    let driver = spawn_detached(command);
    let identity = driver.identity();
    assert_eq!(identity.pgid, identity.pid as i32);
    driver.send(&serde_json::json!({})).expect("write stdin");
    assert_eq!(wait_terminal(&driver), ChildExit::Exited(Some(7)));
    assert_eq!(driver.wait().expect("wait"), Some(7));
    assert!(
        driver.recv_timeout(Duration::from_millis(100)).is_err(),
        "duplicate terminal event"
    );
}

#[test]
fn daemon_helper_detached_stop_reaps_runtime_group() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "read line; sleep 30"]);
    let driver = spawn_detached(command);
    driver.send(&serde_json::json!({})).expect("write stdin");
    let pgid = driver.identity().pgid;
    driver
        .stop_and_reap(Duration::from_secs(3))
        .expect("detached stop_and_reap");
    let remaining = observe_process_group(pgid);
    assert!(
        remaining.is_err() || remaining.unwrap().is_empty(),
        "runtime group {pgid} survived stop"
    );
}
