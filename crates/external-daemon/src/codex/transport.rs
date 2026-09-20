//! The Codex stdio pump thread (strict-codec frame loop, unsupported
//! request mirroring, child-exit terminalization) and the process-group
//! reap proof for terminal classification.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use external_contract::{RequestEnvelope, WireMessage};
use external_runtime::{observe_process_group, ChildExit, Driver, Inbound, StopOutcome};

use super::events::CodexShared;
use crate::{Publisher, RuntimeLoss, RuntimeTerminal, TurnBoundary};

pub(super) fn spawn_codex_pump(
    driver: Arc<Driver>,
    publisher: Arc<Publisher>,
    shared: Arc<CodexShared>,
    shutdown: Arc<AtomicBool>,
) {
    thread::spawn(move || loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        match driver.recv_timeout(Duration::from_millis(20)) {
            Ok(event) => {
                if let Inbound::Message(WireMessage::UnknownEvent { method, raw }) = &event {
                    if !shared.project_notification(method, raw) {
                        continue;
                    }
                }
                if let Inbound::Message(WireMessage::Request(request)) = &event {
                    // Every admitted Codex thread pins approvalPolicy=never
                    // (plan read-only, build/edit workspace-write, yolo
                    // danger-full-access) and should not issue interaction
                    // requests; anything it does issue stays an observable,
                    // non-respondable record instead of hanging.
                    let unsupported = RequestEnvelope::new(
                        request.id.clone(),
                        external_contract::INTERACTION_REQUEST_UNSUPPORTED_INPUT,
                        serde_json::json!({
                            "origin": "codex_app_server",
                            "method": request.method,
                        }),
                    );
                    publisher
                        .emit_driver(Inbound::Message(WireMessage::Request(unsupported)), None);
                    continue;
                }
                if let Inbound::ChildExited(exit) = &event {
                    driver.wait_diagnostics(Duration::from_secs(1));
                    let terminal = classify_child_exit(driver.identity().pgid, &shared, exit);
                    publisher.emit_driver(event, terminal);
                    return;
                }
                publisher.emit_driver(event, None);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                publisher.publish_terminal(RuntimeTerminal::FailedRuntimeLost(
                    RuntimeLoss::EventStreamLost,
                ));
                return;
            }
        }
    });
}

fn classify_child_exit(
    pgid: i32,
    shared: &CodexShared,
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
