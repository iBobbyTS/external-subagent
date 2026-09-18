//! The DSH stdio pump thread (frame loop, projection handoff, settlement
//! folding at proven stream-drain points, child-exit terminalization) and the
//! process-group reap proof for terminal classification.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use external_runtime::{observe_process_group, ChildExit, Driver, Inbound, StopOutcome};

use super::events::{project_dsh_inbound, DshRuntimeShared, SettledTurn};
use crate::{Publisher, RuntimeLoss, RuntimeTerminal, TurnBoundary};

/// How long an exit path waits for a settlement the watcher already resolved
/// (the driver resolves a pending prompt response before it broadcasts the
/// child exit or closes the stream, so a missing deposit can only be a
/// scheduling delay of the watcher thread).
const SETTLEMENT_EXIT_GRACE: Duration = Duration::from_millis(250);

pub(super) fn spawn_dsh_pump(
    driver: Arc<Driver>,
    publisher: Arc<Publisher>,
    shared: Arc<DshRuntimeShared>,
    shutdown: Arc<AtomicBool>,
) {
    thread::spawn(move || loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        match driver.recv_timeout(Duration::from_millis(20)) {
            Ok(event) => {
                if let Some(projected) = project_dsh_inbound(&shared, &event) {
                    publisher.emit_driver(projected, None);
                }
                if let Inbound::ChildExited(exit) = &event {
                    finish_on_child_exit(&driver, &publisher, &shared, &event, exit);
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                fold_settled_turn(&driver, &publisher, &shared);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                finish_on_lost_stream(&publisher, &shared);
                return;
            }
        }
    });
}

/// Fold a settlement the watcher already deposited, but only after proving
/// the pump projected every inbound frame that preceded the settlement
/// response on the wire.
///
/// The driver resolves a prompt response while frames sent before it may
/// still sit queued for this pump (the dispatcher forwards them on the same
/// stream order but the response skips the queue into the watcher), so the
/// watcher must not fold directly. Taking the deposit first and then draining
/// the inbound channel to a momentary emptiness is the ordering proof: the
/// dispatcher forwarded every pre-response frame before the response was
/// resolved, so once the deposit is taken only post-response frames can
/// arrive, and the fold sees every message the settlement may verify.
fn fold_settled_turn(driver: &Driver, publisher: &Publisher, shared: &DshRuntimeShared) {
    let Some(settled) = shared.take_settled_turn() else {
        return;
    };
    loop {
        match driver.recv_timeout(Duration::ZERO) {
            Ok(event) => {
                if let Some(projected) = project_dsh_inbound(shared, &event) {
                    publisher.emit_driver(projected, None);
                }
                if let Inbound::ChildExited(exit) = &event {
                    shared.apply_settled_turn(settled);
                    finish_on_child_exit(driver, publisher, shared, &event, exit);
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                shared.apply_settled_turn(settled);
                finish_on_lost_stream(publisher, shared);
                return;
            }
        }
    }
    shared.apply_settled_turn(settled);
}

/// Terminalize on the child's exit. A settlement of a still-active turn
/// folds first, so the exit classification sees the turn boundary instead of
/// racing the watcher's deposit; an already-idle turn never blocks here.
fn finish_on_child_exit(
    driver: &Driver,
    publisher: &Publisher,
    shared: &DshRuntimeShared,
    event: &Inbound,
    exit: &ChildExit,
) {
    driver.wait_diagnostics(Duration::from_secs(1));
    if shared.turn_tracker.snapshot().active {
        if let Some(settled) = take_settled_turn_within(shared, SETTLEMENT_EXIT_GRACE) {
            shared.apply_settled_turn(settled);
        }
    }
    let terminal = classify_child_exit(driver.identity().pgid, shared, exit);
    publisher.emit_driver(event.clone(), terminal);
    shared.offers.lock().unwrap().clear();
}

/// Terminalize when the inbound stream closed without a child exit. As on
/// the exit path, a resolved-but-undeposited settlement of an active turn
/// folds before the runtime-loss terminal so its boundary still lands.
fn finish_on_lost_stream(publisher: &Publisher, shared: &DshRuntimeShared) {
    if shared.turn_tracker.snapshot().active {
        if let Some(settled) = take_settled_turn_within(shared, SETTLEMENT_EXIT_GRACE) {
            shared.apply_settled_turn(settled);
        }
    }
    shared.offers.lock().unwrap().clear();
    publisher.publish_terminal(RuntimeTerminal::FailedRuntimeLost(
        RuntimeLoss::EventStreamLost,
    ));
}

/// Take a deposited settlement, waiting out only the watcher's scheduling
/// delay: the driver has already resolved the pending response before the
/// exit paths reach here, so the deposit is at worst a slice away.
fn take_settled_turn_within(shared: &DshRuntimeShared, grace: Duration) -> Option<SettledTurn> {
    let deadline = Instant::now() + grace;
    loop {
        if let Some(settled) = shared.take_settled_turn() {
            return Some(settled);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn classify_child_exit(
    pgid: i32,
    shared: &DshRuntimeShared,
    exit: &ChildExit,
) -> Option<RuntimeTerminal> {
    // Completed is only provable when the whole process group is already
    // reaped; any surviving member keeps the terminal orphaned instead.
    match observe_process_group(pgid) {
        Ok(members) if members.is_empty() => match exit {
            ChildExit::Exited(Some(0)) => {
                let turn = shared.turn_tracker.snapshot();
                if !turn.active && turn.boundary == Some(TurnBoundary::Completed) {
                    Some(RuntimeTerminal::Completed(StopOutcome::AlreadyExited(
                        exit.clone(),
                    )))
                } else {
                    Some(RuntimeTerminal::FailedRuntimeLost(
                        RuntimeLoss::EventStreamLost,
                    ))
                }
            }
            _ => Some(RuntimeTerminal::Exited(exit.clone())),
        },
        Ok(_) | Err(_) => Some(RuntimeTerminal::Orphaned(RuntimeLoss::UnknownMembership)),
    }
}
