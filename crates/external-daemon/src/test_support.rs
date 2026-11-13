//! Test-only fixture ingress through the real adapter. Production shared state
//! has no JSON interpreter. A standalone terminal fixture primes its turn.
use crate::*;
use external_contract::events::*;

pub(crate) fn fixture_event(inbound: Inbound) -> RuntimeEnvelope {
    let mut adapter =
        zcode::events::ZcodeAdapter::new(TaskId("fixture".into()), LaunchId("fixture".into()));
    if let Inbound::Message(message) = &inbound {
        if let Some((_, params)) = message.notification() {
            if params.get("type").and_then(serde_json::Value::as_str) != Some("turn.started") {
                let started = Inbound::Message(WireMessage::Event(
                    external_contract::EventEnvelope {
                        method: "session/event".into(),
                        params: serde_json::json!({"type":"turn.started","turnId":params.get("turnId")}),
                    },
                ));
                adapter.decode(&started);
            }
        }
    }
    if let Some(batch) = adapter.decode(&inbound) {
        let diagnostic = runtime_observation::InputDiagnostic::from_disposition(
            batch.input(),
            batch.disposition(),
        );
        if let Some(event) = batch.events().last() {
            return RuntimeEnvelope::Event {
                event: event.clone(),
                diagnostic,
            };
        }
    }
    let fact = if let Inbound::Message(WireMessage::Request(request)) = inbound {
        let kind = match request.method.as_str() {
            INTERACTION_REQUEST_PERMISSION => InputKind::Permission,
            INTERACTION_REQUEST_USER_INPUT => InputKind::Question,
            _ => InputKind::Unsupported,
        };
        EventFact::InputRequested(InputRequest {
            request_id: request.id,
            kind,
            respondable: kind != InputKind::Unsupported,
            question: scheduler::redact_native_error(&request.params),
        })
    } else {
        EventFact::Diagnostic(Diagnostic {
            reason: DiagnosticReason::AuxiliaryTerminal,
            impact: Impact::Unknown,
            discriminator: None,
        })
    };
    RuntimeEnvelope::from(NormalizedEvent {
        context: EventContext {
            task_id: TaskId("fixture".into()),
            launch_id: LaunchId("fixture".into()),
            sequence: 1,
            turn_id: None,
            source: NativeSource {
                protocol: SourceProtocol::Runtime,
                discriminator: None,
                session_id: None,
                turn_id: None,
                item_id: None,
                call_id: None,
                event_id: None,
                timestamp: None,
            },
        },
        fact,
    })
}

pub(crate) fn typed_fact(fact: EventFact) -> NormalizedEvent {
    NormalizedEvent {
        context: EventContext {
            task_id: TaskId("fixture".into()),
            launch_id: LaunchId("fixture".into()),
            sequence: 1,
            turn_id: Some(TurnId::Local(1)),
            source: NativeSource {
                protocol: SourceProtocol::Runtime,
                discriminator: None,
                session_id: None,
                turn_id: None,
                item_id: None,
                call_id: None,
                event_id: None,
                timestamp: None,
            },
        },
        fact,
    }
}
