//! Shared DSH projection state and the normalization of inbound ACP frames
//! (`session/update`, permission offers, input requests) into the canonical
//! internal event envelope, plus prompt-settlement turn boundaries.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use external_agent_dsh::acp::{
    permission::{self, OfferCache},
    result::{fold_settlement, MessageAggregation, SettlementOutcome},
    transport, update,
};
use external_contract::{
    classify_lifecycle, EventEnvelope, RequestEnvelope, WireId, WireMessage,
    INTERACTION_REQUEST_PERMISSION, INTERACTION_REQUEST_UNSUPPORTED_INPUT,
    INTERACTION_REQUEST_USER_INPUT, SESSION_EVENT,
};
use external_runtime::Inbound;

use crate::{Publisher, TurnBoundary, TurnTracker};

const MAX_TRACKED_TOOLS: usize = 128;

pub(super) struct DshRuntimeShared {
    pub(super) publisher: Arc<Publisher>,
    pub(super) turn_tracker: Arc<TurnTracker>,
    pub(super) offers: Mutex<OfferCache>,
    pub(super) tool_names: Mutex<HashMap<String, String>>,
    pub(super) messages: Mutex<MessageAggregation>,
    pub(super) session_id: Mutex<Option<String>>,
    pub(super) current_prompt: Mutex<Option<String>>,
    pub(super) sequence: AtomicU64,
    pub(super) stop_boundaries: AtomicU64,
}

impl DshRuntimeShared {
    fn next_event_id(&self) -> String {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        format!("dsh-event-{sequence}")
    }

    fn canonical_event(&self, params: serde_json::Value) -> Inbound {
        Inbound::Message(WireMessage::Event(EventEnvelope {
            method: SESSION_EVENT.into(),
            params,
        }))
    }

    fn emit_canonical(&self, params: serde_json::Value) {
        self.publisher
            .emit_driver(self.canonical_event(params), None);
    }

    fn emit_lifecycle(&self, method: &str) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        self.publisher.emit_driver(
            Inbound::Lifecycle {
                sequence,
                method: method.into(),
                order: classify_lifecycle(method, false),
            },
            None,
        );
    }

    pub(super) fn begin_turn(&self, prompt_id: u64) {
        *self.current_prompt.lock().unwrap() = Some(format!("dsh-prompt-{prompt_id}"));
        let params = serde_json::json!({"type": "turn.started"});
        let started = self.canonical_event(params.clone());
        // The canonical start event opens the shared tracker's per-turn
        // terminal text before any of the turn's streaming can arrive.
        self.emit_canonical(params);
        self.turn_tracker.observe(&started);
        self.emit_lifecycle("turn.started");
    }

    /// Apply one prompt settlement as the single turn-boundary authority.
    fn apply_boundary(&self, kind: TurnBoundary, final_text: Option<&str>, reason: Option<&str>) {
        let turn_id = self.current_prompt.lock().unwrap().take();
        let event_id = self.next_event_id();
        let mut payload = serde_json::json!({});
        if let Some(text) = final_text {
            payload["response"] = serde_json::Value::String(text.to_owned());
        }
        if let Some(reason) = reason {
            payload["reason_code"] = serde_json::Value::String(reason.to_owned());
        }
        let event_type = match kind {
            TurnBoundary::Completed => "turn.completed",
            TurnBoundary::Failed => "turn.failed",
        };
        let params = serde_json::json!({
            "type": event_type,
            "eventId": event_id,
            "turnId": turn_id,
            "payload": payload,
        });
        let boundary_event = self.canonical_event(params.clone());
        // The sink must observe the settlement's authoritative payload before
        // the tracker exposes the boundary: the scheduler reacts to the
        // tracker (delivering the next prompt or terminalizing the task), and
        // events admitted after terminalization are dropped, which would
        // strand the turn's final text.
        self.emit_canonical(params);
        self.turn_tracker.observe(&boundary_event);
        self.emit_lifecycle(event_type);
    }

    pub(super) fn apply_settlement(&self, settlement: &transport::PromptSettlement) {
        let outcome = {
            let messages = self.messages.lock().unwrap();
            fold_settlement(settlement, &messages)
        };
        match outcome {
            SettlementOutcome::Completed { final_text } => {
                self.apply_boundary(TurnBoundary::Completed, final_text.as_deref(), None);
            }
            SettlementOutcome::Failed { reason_code } => {
                self.apply_boundary(TurnBoundary::Failed, None, Some(&reason_code));
            }
        }
    }

    pub(super) fn apply_failed_settlement(&self, message: &str) {
        let bounded: String = message.chars().take(256).collect();
        self.apply_boundary(TurnBoundary::Failed, None, Some(&bounded));
    }
}

/// Normalize one inbound ACP frame into the canonical internal envelope.
/// Returns `None` when the frame itself is the projection (pass-through).
pub(super) fn project_dsh_inbound(shared: &DshRuntimeShared, event: &Inbound) -> Option<Inbound> {
    let Inbound::Message(message) = event else {
        return Some(event.clone());
    };
    match message {
        WireMessage::UnknownEvent { method, raw } if method == transport::SESSION_UPDATE => {
            let params = raw
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let Some(parsed) = update::parse_update(&params) else {
                return Some(event.clone());
            };
            let expected_session = shared.session_id.lock().unwrap().clone();
            if let (Some(expected), Some(actual)) =
                (expected_session.as_deref(), parsed.session_id.as_deref())
            {
                if expected != actual {
                    // Updates for foreign sessions are never projected.
                    return None;
                }
            }
            if let update::UpdateKind::ToolCall { tool_call_id, .. } = &parsed.kind {
                let name = update::tool_name(&parsed.kind).unwrap_or_else(|| "dsh_tool".into());
                let mut tools = shared.tool_names.lock().unwrap();
                if tools.len() < MAX_TRACKED_TOOLS || tools.contains_key(tool_call_id) {
                    tools.insert(tool_call_id.clone(), name);
                }
            }
            if let update::UpdateKind::AgentMessage {
                message_id: Some(message_id),
                text,
                committed: true,
            } = &parsed.kind
            {
                shared
                    .messages
                    .lock()
                    .unwrap()
                    .observe_committed(message_id, text);
            }
            let event_id = shared.next_event_id();
            let turn_id = shared
                .current_prompt
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "dsh-session".into());
            for payload in update::canonical_event_payloads(&parsed, &event_id, &turn_id) {
                // Streaming events refresh turn-liveness exactly like inbound
                // frames do on the ZCode path.
                let event = shared.canonical_event(payload);
                shared.turn_tracker.observe(&event);
                shared.publisher.emit_driver(event, None);
            }
            None
        }
        WireMessage::Request(request) => {
            if request.method == transport::SESSION_REQUEST_PERMISSION {
                match permission::parse_offer(&request.params) {
                    Some(offer) => {
                        let correlated = offer
                            .tool_call_id
                            .as_deref()
                            .and_then(|id| shared.tool_names.lock().unwrap().get(id).cloned());
                        let normalized =
                            permission::normalize_permission_request(&offer, correlated.as_deref());
                        shared
                            .offers
                            .lock()
                            .unwrap()
                            .observe(permission::correlation_key(&request.id), offer);
                        Some(Inbound::Message(WireMessage::Request(
                            RequestEnvelope::new(
                                request.id.clone(),
                                INTERACTION_REQUEST_PERMISSION,
                                normalized,
                            ),
                        )))
                    }
                    None => Some(unsupported_input_request(
                        request.id.clone(),
                        "dsh permission request carried no usable offer",
                    )),
                }
            } else if request.method == transport::SESSION_REQUEST_INPUT {
                // The only real answerable producer on the DSH path: the
                // question is preserved verbatim and the answer travels back
                // as the plain JSON-RPC result for this request id.
                match transport::request_input_question(&request.params) {
                    Some(question) => Some(Inbound::Message(WireMessage::Request(
                        RequestEnvelope::new(
                            request.id.clone(),
                            INTERACTION_REQUEST_USER_INPUT,
                            serde_json::json!({ "question": question }),
                        ),
                    ))),
                    None => Some(unsupported_input_request(
                        request.id.clone(),
                        "dsh input request carried no usable question",
                    )),
                }
            } else {
                Some(unsupported_input_request(
                    request.id.clone(),
                    "dsh server request is not a supported interaction",
                ))
            }
        }
        _ => Some(event.clone()),
    }
}

fn unsupported_input_request(id: WireId, reason: &str) -> Inbound {
    Inbound::Message(WireMessage::Request(RequestEnvelope::new(
        id,
        INTERACTION_REQUEST_UNSUPPORTED_INPUT,
        serde_json::json!({"origin": "dsh_acp", "reason": reason}),
    )))
}
