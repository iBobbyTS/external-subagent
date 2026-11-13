use crate::{RuntimeEnvelope, RuntimeLoss, RuntimeTerminal};
use external_contract::events::*;
use external_runtime::{ChildExit, StopOutcome};

pub(crate) struct LifecycleProjection {
    pub(crate) event_type: &'static str,
    pub(crate) payload_json: String,
    pub(crate) redaction_level: &'static str,
}

pub(crate) fn lifecycle_projection(
    envelope: &RuntimeEnvelope,
    pending_request_id: Option<&str>,
) -> LifecycleProjection {
    let (event_type, payload, redaction_level) = match envelope {
        RuntimeEnvelope::Event { event, diagnostic } => {
            let mut payload = serde_json::json!({"category":event.fact.lifecycle_category(), "source":event.context.source.protocol,
                "discriminator":crate::runtime_observation::safe_discriminator(event.context.source.discriminator.as_ref()),
                "disposition":if diagnostic.as_ref().is_some_and(|d| d.lost) { "invalid_lost" } else if diagnostic.is_some() { "unknown" } else { "consumed" }});
            if let Some(diagnostic) = diagnostic {
                payload["reason"] = serde_json::to_value(diagnostic.reason).expect("reason");
                payload["impact"] = serde_json::to_value(&diagnostic.impact).expect("impact");
                payload["discriminator"] = diagnostic.discriminator.clone().into();
            }
            match &event.fact {
                EventFact::InputRequested(_) => payload["request_id"] = pending_request_id.into(),
                EventFact::TransportFault { kind, .. } => {
                    payload["fault"] = serde_json::to_value(kind).expect("fault")
                }
                EventFact::RuntimeExited {
                    exit_code, signal, ..
                } => {
                    payload["exit_code"] = (*exit_code).into();
                    payload["signal"] = (*signal).into();
                }
                _ => {}
            }
            (
                if matches!(
                    event.fact,
                    EventFact::TransportFault {
                        kind: TransportFaultKind::FrameLimit,
                        ..
                    }
                ) {
                    "driver.oversized_line"
                } else {
                    "runtime.fact"
                },
                payload,
                "allowlisted",
            )
        }
        RuntimeEnvelope::Terminal { terminal, .. } => match terminal {
            RuntimeTerminal::Stopped(outcome) => (
                "runtime.stopped",
                serde_json::json!({"kind":"stopped", "outcome":stop_outcome_name(outcome)}),
                "allowlisted",
            ),
            RuntimeTerminal::Completed(outcome) => (
                "runtime.completed",
                serde_json::json!({"kind":"completed", "outcome":stop_outcome_name(outcome)}),
                "allowlisted",
            ),
            RuntimeTerminal::FailedTurn(outcome) => (
                "runtime.turn_failed",
                serde_json::json!({"kind":"turn_failed", "outcome":stop_outcome_name(outcome)}),
                "allowlisted",
            ),
            RuntimeTerminal::Exited(exit) => (
                "runtime.exited",
                serde_json::json!({"kind":"exited", "outcome":child_exit_name(exit)}),
                "allowlisted",
            ),
            RuntimeTerminal::FailedRuntimeLost(loss) => (
                "runtime.failed_runtime_lost",
                serde_json::json!({"kind":"failed_runtime_lost", "reason":runtime_loss_name(loss)}),
                runtime_loss_redaction(loss),
            ),
            RuntimeTerminal::Orphaned(loss) => (
                "runtime.orphaned",
                serde_json::json!({"kind":"orphaned", "reason":runtime_loss_name(loss)}),
                runtime_loss_redaction(loss),
            ),
        },
    };
    LifecycleProjection {
        event_type,
        payload_json: payload.to_string(),
        redaction_level,
    }
}

fn child_exit_name(exit: &ChildExit) -> &'static str {
    match exit {
        ChildExit::Exited(Some(0)) => "exited_success",
        ChildExit::Exited(Some(_)) => "exited_failure",
        ChildExit::Exited(None) => "exited_unknown",
        ChildExit::Signaled(_) => "signaled",
        ChildExit::Unknown => "unknown",
    }
}

fn stop_outcome_name(outcome: &StopOutcome) -> &'static str {
    match outcome {
        StopOutcome::AlreadyExited(_) => "already_exited",
        StopOutcome::Terminated(_) => "terminated",
    }
}

fn runtime_loss_name(loss: &RuntimeLoss) -> &'static str {
    match loss {
        RuntimeLoss::InvalidIdentity => "invalid_identity",
        RuntimeLoss::UnsupportedIdentity => "unsupported_identity",
        RuntimeLoss::MissingLeader => "missing_leader",
        RuntimeLoss::IdentityMismatch => "identity_mismatch",
        RuntimeLoss::UnknownMembership => "unknown_membership",
        RuntimeLoss::SessionLost => "session_lost",
        RuntimeLoss::StopFailed(_) => "stop_failed",
        RuntimeLoss::EventStreamLost => "event_stream_lost",
        RuntimeLoss::PostureViolation(_) => "posture_violation",
    }
}

fn runtime_loss_redaction(loss: &RuntimeLoss) -> &'static str {
    if matches!(loss, RuntimeLoss::StopFailed(_)) {
        "redacted"
    } else {
        "allowlisted"
    }
}
