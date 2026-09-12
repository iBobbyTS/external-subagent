use crate::{RuntimeEvent, RuntimeLoss, RuntimeTerminal};
use external_contract::{event_type, LifecycleOrder, WireMessage};
use external_runtime::{ChildExit, Inbound, StopOutcome};

pub(crate) struct LifecycleProjection {
    pub(crate) event_type: &'static str,
    pub(crate) payload_json: String,
    pub(crate) redaction_level: &'static str,
}

pub(crate) fn lifecycle_projection(
    event: &RuntimeEvent,
    pending_request_id: Option<&str>,
) -> LifecycleProjection {
    let (event_type, payload, redaction_level) = match event {
        RuntimeEvent::Driver(Inbound::Message(WireMessage::Request(request))) => (
            "driver.message",
            serde_json::json!({
                "kind": "request",
                "method": request.method,
                "request_id": pending_request_id,
            }),
            "redacted",
        ),
        RuntimeEvent::Driver(Inbound::Message(WireMessage::Response(response))) => (
            "driver.message",
            serde_json::json!({
                "kind": "response",
                "outcome": if response.error.is_some() { "error" } else { "result" },
            }),
            "redacted",
        ),
        RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(message))) => (
            "driver.message",
            serde_json::json!({
                "kind": "event",
                "method": message.method,
                "type": event_type(message),
            }),
            "redacted",
        ),
        RuntimeEvent::Driver(Inbound::Message(WireMessage::UnknownEvent { .. })) => (
            "raw.unknown",
            serde_json::json!({"kind": "unknown_event", "raw": "[REDACTED]"}),
            "redacted",
        ),
        RuntimeEvent::Driver(Inbound::Lifecycle {
            sequence,
            method,
            order,
        }) => (
            "driver.lifecycle",
            serde_json::json!({
                "kind": "lifecycle",
                "sequence": sequence,
                "method": method,
                "order": lifecycle_order_name(order),
            }),
            "allowlisted",
        ),
        RuntimeEvent::Driver(Inbound::Malformed(_)) => (
            "driver.malformed",
            serde_json::json!({"kind": "malformed", "detail": "[REDACTED]"}),
            "redacted",
        ),
        RuntimeEvent::Driver(Inbound::OversizedLine { bytes }) => (
            "driver.oversized_line",
            serde_json::json!({"kind": "oversized_line", "bytes": bytes}),
            "allowlisted",
        ),
        RuntimeEvent::Driver(Inbound::ChildExited(exit)) => (
            "driver.child_exited",
            serde_json::json!({"kind": "child_exited", "outcome": child_exit_name(exit)}),
            "allowlisted",
        ),
        RuntimeEvent::Driver(Inbound::UnmatchedResponse { id: _, outcome }) => (
            "driver.unmatched_response",
            serde_json::json!({"kind": "unmatched_response", "outcome": outcome}),
            "redacted",
        ),
        RuntimeEvent::Terminal(RuntimeTerminal::Stopped(outcome)) => (
            "runtime.stopped",
            serde_json::json!({"kind": "stopped", "outcome": stop_outcome_name(outcome)}),
            "allowlisted",
        ),
        RuntimeEvent::Terminal(RuntimeTerminal::Completed(outcome)) => (
            "runtime.completed",
            serde_json::json!({"kind": "completed", "outcome": stop_outcome_name(outcome)}),
            "allowlisted",
        ),
        RuntimeEvent::Terminal(RuntimeTerminal::FailedTurn(outcome)) => (
            "runtime.turn_failed",
            serde_json::json!({"kind": "turn_failed", "outcome": stop_outcome_name(outcome)}),
            "allowlisted",
        ),
        RuntimeEvent::Terminal(RuntimeTerminal::Exited(exit)) => (
            "runtime.exited",
            serde_json::json!({"kind": "exited", "outcome": child_exit_name(exit)}),
            "allowlisted",
        ),
        RuntimeEvent::Terminal(RuntimeTerminal::FailedRuntimeLost(loss)) => (
            "runtime.failed_runtime_lost",
            serde_json::json!({"kind": "failed_runtime_lost", "reason": runtime_loss_name(loss)}),
            runtime_loss_redaction(loss),
        ),
        RuntimeEvent::Terminal(RuntimeTerminal::Orphaned(loss)) => (
            "runtime.orphaned",
            serde_json::json!({"kind": "orphaned", "reason": runtime_loss_name(loss)}),
            runtime_loss_redaction(loss),
        ),
    };
    LifecycleProjection {
        event_type,
        payload_json: payload.to_string(),
        redaction_level,
    }
}

fn lifecycle_order_name(order: &LifecycleOrder) -> &'static str {
    match order {
        LifecycleOrder::NotLifecycle => "not_lifecycle",
        LifecycleOrder::InOrder => "in_order",
        LifecycleOrder::OutOfOrder { .. } => "out_of_order",
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
    }
}

fn runtime_loss_redaction(loss: &RuntimeLoss) -> &'static str {
    if matches!(loss, RuntimeLoss::StopFailed(_)) {
        "redacted"
    } else {
        "allowlisted"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use external_contract::{RequestEnvelope, WireId};

    #[test]
    fn lifecycle_mapping_preserves_sequence_and_order() {
        let event = RuntimeEvent::Driver(Inbound::Lifecycle {
            sequence: 7,
            method: "turn.started".into(),
            order: LifecycleOrder::InOrder,
        });
        let projection = lifecycle_projection(&event, None);
        assert_eq!(projection.event_type, "driver.lifecycle");
        assert_eq!(projection.redaction_level, "allowlisted");
        let payload: serde_json::Value = serde_json::from_str(&projection.payload_json).unwrap();
        assert_eq!(payload["sequence"], 7);
        assert_eq!(payload["order"], "in_order");
    }

    #[test]
    fn request_projection_redacts_params_and_keeps_pending_id() {
        let request = RequestEnvelope::new(
            WireId::String("secret-id".into()),
            "session/send",
            serde_json::json!({"secret": "value"}),
        );
        let projection = lifecycle_projection(
            &RuntimeEvent::Driver(Inbound::Message(WireMessage::Request(request))),
            Some("pending-42"),
        );
        assert_eq!(projection.redaction_level, "redacted");
        assert!(!projection.payload_json.contains("secret"));
        assert!(projection.payload_json.contains("pending-42"));
    }

    #[test]
    fn stop_failure_is_redacted_but_safe_losses_are_allowlisted() {
        let failed = lifecycle_projection(
            &RuntimeEvent::Terminal(RuntimeTerminal::Orphaned(RuntimeLoss::StopFailed(
                "private detail".into(),
            ))),
            None,
        );
        assert_eq!(failed.redaction_level, "redacted");
        assert!(!failed.payload_json.contains("private detail"));
        let safe = lifecycle_projection(
            &RuntimeEvent::Terminal(RuntimeTerminal::Orphaned(RuntimeLoss::SessionLost)),
            None,
        );
        assert_eq!(safe.redaction_level, "allowlisted");
        assert!(safe.payload_json.contains("session_lost"));
    }
}
