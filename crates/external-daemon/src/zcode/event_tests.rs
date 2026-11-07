//! Synthetic interleavings use source-inspected protocol shapes. Fixture cases
//! retain their OBSERVED/SOURCE_INSPECTED/SYNTHETIC provenance in tests/fixtures.
use super::*;
use crate::{
    activity_tracker::PassiveActivityTracker, LifecycleRecord, LifecycleSink, Publisher,
    RuntimeEvent, TurnTracker,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

struct ViewSink {
    activity: PassiveActivityTracker,
    records: Mutex<Vec<LifecycleRecord>>,
}
impl LifecycleSink for ViewSink {
    fn emit(&self, record: LifecycleRecord) {
        self.activity.observe(&record.event);
        self.records.lock().unwrap().push(record);
    }
}
struct Trace {
    adapter: ZcodeAdapter,
    publisher: Publisher,
    tracker: TurnTracker,
    sink: Arc<ViewSink>,
    next: u64,
}
impl Trace {
    fn new() -> Self {
        let sink = Arc::new(ViewSink {
            activity: PassiveActivityTracker::for_adapter("zcode"),
            records: Mutex::new(vec![]),
        });
        Self {
            adapter: ZcodeAdapter::new(TaskId("task".into()), LaunchId("launch".into())),
            publisher: Publisher::new(sink.clone()),
            tracker: TurnTracker::new(),
            sink,
            next: 0,
        }
    }
    fn send(&mut self, method: &str, mut params: Value) -> DecodedInput {
        self.next += 1;
        params["eventId"] = json!(format!("input-{}", self.next));
        params["sessionId"] = json!("session-1");
        params["turnId"] = json!("turn-1");
        if method == "v4/telemetry/event" {
            params["version"] = json!(1);
        }
        let inbound = Inbound::Message(WireMessage::UnknownEvent {
            method: method.into(),
            raw: json!({"method":method, "params":params}),
        });
        let batch = self.adapter.decode(&inbound).unwrap();
        self.publisher.emit_typed(batch.clone(), &self.tracker);
        batch
    }
    fn model(&mut self, id: &str, status: &str) {
        self.send(
            "v4/telemetry/event",
            json!({"kind":"model.request.status","requestId":id,"status":status}),
        );
    }
    fn active(&self) -> bool {
        self.sink.activity.snapshot().model_request_active
    }
}

#[test]
fn retry_attempts_and_parallel_requests_converge_without_degradation() {
    let mut trace = Trace::new();
    trace.send("session/event", json!({"type":"turn.started"}));
    trace.model("r1", "model_request_started");
    assert!(trace.active());
    trace.model("r1", "model_request_failed");
    assert!(!trace.active(), "failed attempt must leave active set");
    trace.model("r1", "model_retry_scheduled");
    assert!(!trace.active());
    trace.model("r2", "model_request_started");
    trace.model("parallel", "model_request_started");
    trace.model("r2", "model_request_failed");
    assert!(trace.active());
    trace.model("parallel", "model_stream_stalled");
    assert!(trace.active());
    trace.model("parallel", "model_request_completed");
    assert!(!trace.active());
    assert!(!trace.sink.activity.snapshot().telemetry_degraded);
    assert!(
        trace.tracker.snapshot().active,
        "attempt failure is not turn failure"
    );
}

#[test]
fn fixture_dispositions_use_real_adapter_and_keep_retry_payload() {
    let cases: Vec<Value> = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/zcode-events/cases.json"
    ))
    .unwrap();
    for case in cases {
        let input = &case["input"];
        let mut adapter = ZcodeAdapter::new(TaskId("task".into()), LaunchId("launch".into()));
        let batch = adapter
            .decode(&Inbound::Message(WireMessage::UnknownEvent {
                method: input["method"].as_str().unwrap().into(),
                raw: input.clone(),
            }))
            .unwrap();
        let disposition = serde_json::to_value(batch.disposition()).unwrap();
        assert_eq!(
            disposition["kind"], case["expected_disposition"],
            "{}",
            case["name"]
        );
        let facts: Vec<_> = batch
            .events()
            .iter()
            .filter(|event| {
                batch.disposition().kind() == DispositionKind::Consumed
                    || !matches!(
                        event.fact,
                        EventFact::Diagnostic(_) | EventFact::CoverageGap(_)
                    )
            })
            .map(|event| serde_json::to_value(&event.fact).unwrap()["kind"].clone())
            .collect();
        assert_eq!(json!(facts), case["expected_facts"], "{}", case["name"]);
        if let Some(EventFact::ModelActivityChanged(model)) =
            batch.events().first().map(|e| &e.fact)
        {
            if model.phase == ModelPhase::Failed {
                assert_eq!(model.failure.as_ref().unwrap().original["statusCode"], 429);
            }
        }
    }
}

#[test]
fn terminal_telemetry_preserves_activity_until_full_session_settlement() {
    for status in ["success", "failed", "interrupted"] {
        let mut trace = Trace::new();
        trace.send("session/event", json!({"type":"turn.started"}));
        trace.model("r1", "model_request_started");
        trace.send("session/event", json!({"type":"model.streaming","payload":{"kind":"text_delta","delta":"prefix ","assistantMessageId":"m1"}}));
        trace.send(
            "v4/telemetry/event",
            json!({"kind":"turn.terminal","status":status,"errorMessage":"summary"}),
        );
        assert!(trace.active());
        assert!(trace.tracker.snapshot().active);
        let failure = json!({"message":"native\noriginal", "stack":"frame".repeat(3000),"data":{"apiKey":"secret"},"unknownField":true});
        let final_params = if status == "failed" {
            json!({"type":"turn.failed","payload":{"error":failure}})
        } else {
            json!({"type":"turn.completed","payload":{"response":"prefix complete"}})
        };
        trace.send("session/event", final_params.clone());
        assert!(!trace.active());
        assert!(!trace.tracker.snapshot().active);
        if status == "failed" {
            let detail: Value =
                serde_json::from_str(&trace.tracker.failure_detail().unwrap()).unwrap();
            assert_eq!(detail["original_error"]["stack"], failure["stack"]);
            assert_eq!(detail["original_error"]["data"]["apiKey"], "[REDACTED]");
            assert_eq!(detail["original_error"]["unknownField"], true);
        } else {
            assert_eq!(
                trace.sink.activity.take_terminal_text(),
                crate::TerminalText::Visible("prefix complete".into())
            );
            assert_eq!(trace.sink.activity.take_wait_tail().text, "prefix complete");
        }
        let before = trace.sink.records.lock().unwrap().len();
        let duplicate = trace.send("session/event", final_params);
        assert_eq!(duplicate.attribution(), Attribution::Late);
        assert_eq!(trace.sink.records.lock().unwrap().len(), before);
        let records = trace.sink.records.lock().unwrap();
        for (index, record) in records.iter().enumerate() {
            assert_eq!(record.sequence, index as u64 + 1);
            let RuntimeEvent::Normalized(event) = &record.event else {
                panic!("typed only")
            };
            assert_eq!(record.sequence, event.context.sequence);
        }
    }
}

#[test]
fn tools_progress_permission_denial_and_dual_sources_do_not_recount_or_finish_execution() {
    let mut trace = Trace::new();
    trace.send("session/event", json!({"type":"turn.started"}));
    trace.send("session/event", json!({"type":"model.streaming","payload":{"kind":"tool_call","toolCallId":"call","toolName":"Bash","input":{"command":"pwd","encrypted_content":"hidden"}}}));
    trace.send(
        "v4/telemetry/event",
        json!({"kind":"tool.lifecycle","toolCallId":"call","toolName":"Bash","phase":"started"}),
    );
    trace.send("session/event", json!({"type":"tool.updated","payload":{"toolCallId":"call","toolName":"Bash","kind":"started"}}));
    trace.send(
        "v4/telemetry/event",
        json!({"kind":"tool.lifecycle","toolCallId":"call","phase":"progress"}),
    );
    let request = Inbound::Message(WireMessage::Request(
        external_contract::RequestEnvelope::new(
            WireId::Integer(42),
            external_contract::INTERACTION_REQUEST_PERMISSION,
            json!({"toolCallId":"call"}),
        ),
    ));
    assert!(trace.adapter.decode(&request).is_none());
    let denied = trace.send(
        "v4/telemetry/event",
        json!({"kind":"permission.lifecycle","toolCallId":"call","phase":"denied"}),
    );
    assert!(denied.events().iter().any(|e| matches!(
        &e.fact,
        EventFact::InputResolved(InputResolution {
            request_id: WireId::Integer(42),
            outcome: InputOutcome::Denied,
            ..
        })
    )));
    let snapshot = trace.sink.activity.snapshot();
    assert_eq!(snapshot.active_tools.len(), 1);
    assert_eq!(snapshot.window_60s.tool_calls_started, 1);
    assert_eq!(snapshot.window_60s.tool_calls_completed, 0);
    assert!(!snapshot.telemetry_degraded);
    let observed = trace.sink.activity.observation_snapshot();
    assert_eq!(observed.tools[0].call_count, 1);
    assert_eq!(
        observed.tools[0].recent_calls[0].arguments["command"],
        "pwd"
    );
    assert!(!serde_json::to_string(&observed).unwrap().contains("hidden"));
    assert_eq!(observed.tools[0].recent_calls[0].redacted_fields, 1);
    for record in trace.sink.records.lock().unwrap().iter() {
        if let RuntimeEvent::Normalized(event) = &record.event {
            assert!(!serde_json::to_string(event).unwrap().contains("hidden"));
        }
    }
    trace.send(
        "session/event",
        json!({"type":"tool.updated","payload":{"toolCallId":"call","kind":"result","result":{}}}),
    );
    trace.send(
        "v4/telemetry/event",
        json!({"kind":"tool.lifecycle","toolCallId":"call","phase":"completed"}),
    );
    let snapshot = trace.sink.activity.snapshot();
    assert!(snapshot.active_tools.is_empty());
    assert_eq!(snapshot.window_60s.tool_calls_completed, 1);
}

#[test]
fn known_ignored_and_unknown_or_damage_have_distinct_coverage() {
    let mut trace = Trace::new();
    trace.send("session/event", json!({"type":"turn.started"}));
    for p in [
        json!({"kind":"usage.delta"}),
        json!({"kind":"stream.chunk","channel":"thought","chunkLength":42}),
        json!({"kind":"compaction.terminal","operationId":"op","status":"completed"}),
        json!({"kind":"subagent.lifecycle","agentId":"child","childSessionId":"child-session","phase":"spawned"}),
        json!({"kind":"workflow.lifecycle","runId":"run","phase":"run-settled","status":"completed"}),
    ] {
        let batch = trace.send("v4/telemetry/event", p);
        assert!(matches!(
            batch.disposition(),
            DecodeDisposition::RecognizedIgnored(_)
        ));
    }
    for method in [
        "state.updated",
        "v4/conversation/frame",
        "v4/telemetry/local-ttft",
        "v4/cua/permission-observation",
        "v4/plugin-ui/tool-result",
        "v4/offPeak/lifecycle",
    ] {
        let batch = trace.send(method, json!({}));
        assert!(matches!(
            batch.disposition(),
            DecodeDisposition::RecognizedIgnored(_)
        ));
    }
    assert!(!trace.sink.activity.snapshot().telemetry_degraded);
    assert_eq!(
        trace
            .sink
            .activity
            .observation_snapshot()
            .coverage
            .dropped_events,
        0
    );
    let unknown = trace.send(
        "v4/telemetry/event",
        json!({"kind":"future.fact","private":"private payload"}),
    );
    assert!(matches!(
        unknown.disposition(),
        DecodeDisposition::Unknown(_)
    ));
    assert!(!trace.sink.activity.snapshot().telemetry_degraded);
    assert_eq!(
        trace
            .sink
            .activity
            .observation_snapshot()
            .coverage
            .dropped_events,
        0
    );
    let malformed = trace.send(
        "v4/telemetry/event",
        json!({"kind":"model.request.status","status":"model_request_started"}),
    );
    assert!(
        matches!(malformed.disposition(), DecodeDisposition::InvalidLost(CoverageGap { impact: Impact::Known(dimensions), .. }) if dimensions == &vec![CoverageDimension::ModelActivity])
    );
    for record in trace.sink.records.lock().unwrap().iter() {
        let projection = crate::projection::lifecycle_projection(&record.event, None);
        assert!(!projection.payload_json.contains("private payload"));
    }
}

#[test]
fn session_model_statuses_share_mapping_and_admission_is_not_request_execution() {
    let mut trace = Trace::new();
    trace.send("session/event", json!({"type":"turn.started"}));
    for status in ["model_request_queued", "model_request_admitted"] {
        trace.send(
            "session/event",
            json!({"type":"session.updated","payload":{"type":status,"requestId":"r"}}),
        );
        assert!(!trace.active());
    }
    for (status, active) in [
        ("model_request_started", true),
        ("model_stream_stalled", true),
        ("model_request_failed", false),
        ("model_retry_scheduled", false),
    ] {
        trace.send(
            "session/event",
            json!({"type":"session.updated","payload":{"type":status,"requestId":"r"}}),
        );
        assert_eq!(trace.active(), active);
    }
    assert!(!trace.sink.activity.snapshot().telemetry_degraded);
}

#[test]
fn replay_and_foreign_inputs_are_ignored_without_content_hashes() {
    let mut adapter = ZcodeAdapter::new(TaskId("t".into()), LaunchId("l".into()));
    let frame = |session: &str, id: &str| {
        Inbound::Message(WireMessage::UnknownEvent {
            method: "session/event".into(),
            raw: json!({"params":{
                "sessionId":session,"eventId":id,"type":"model.streaming","payload":{"kind":"text_delta","delta":"same text"}
            }}),
        })
    };
    assert_eq!(adapter.decode(&frame("s", "1")).unwrap().events().len(), 1);
    assert_eq!(
        adapter.decode(&frame("s", "1")).unwrap().attribution(),
        Attribution::Replay
    );
    assert_eq!(adapter.decode(&frame("s", "2")).unwrap().events().len(), 1);
    assert_eq!(
        adapter
            .decode(&frame("foreign", "3"))
            .unwrap()
            .attribution(),
        Attribution::ForeignSession
    );
}

#[test]
fn malformed_and_oversized_transport_frames_keep_safe_fault_and_loss() {
    let mut trace = Trace::new();
    for inbound in [
        Inbound::Malformed("private broken data".into()),
        Inbound::OversizedLine {
            bytes: external_runtime::MAX_NDJSON_LINE_BYTES + 1,
        },
    ] {
        let batch = trace.adapter.decode(&inbound).unwrap();
        assert!(matches!(
            batch.disposition(),
            DecodeDisposition::InvalidLost(_)
        ));
        if matches!(inbound, Inbound::OversizedLine { .. }) {
            assert!(batch.events().iter().any(|event| matches!(
                event.fact,
                EventFact::TransportFault {
                    kind: TransportFaultKind::FrameLimit,
                    ..
                }
            )));
        }
        trace.publisher.emit_typed(batch, &trace.tracker);
    }
    assert_eq!(
        trace
            .sink
            .activity
            .observation_snapshot()
            .coverage
            .dropped_events,
        2
    );
    assert!(!trace.sink.activity.snapshot().telemetry_degraded);
    let coverage = trace.sink.activity.observation_snapshot().coverage;
    assert!(coverage.tool_history_complete);
    assert!(coverage.reasoning_complete);
    for record in trace.sink.records.lock().unwrap().iter() {
        assert!(
            !crate::projection::lifecycle_projection(&record.event, None)
                .payload_json
                .contains("private broken data")
        );
    }
}

#[test]
fn empty_final_response_settles_without_inventing_visible_output() {
    let mut trace = Trace::new();
    trace.send("session/event", json!({"type":"turn.started"}));
    let batch = trace.send(
        "session/event",
        json!({"type":"turn.completed","payload":{"response":""}}),
    );
    assert!(matches!(batch.disposition(), DecodeDisposition::Consumed));
    assert_eq!(
        trace.tracker.snapshot().boundary,
        Some(crate::TurnBoundary::Completed)
    );
    assert_eq!(
        trace.sink.activity.take_terminal_text(),
        crate::TerminalText::Missing
    );
}

#[test]
fn malformed_public_reasoning_affects_reasoning_coverage_only() {
    let mut trace = Trace::new();
    trace.send("session/event", json!({"type":"turn.started"}));
    trace.send(
        "session/event",
        json!({"type":"model.streaming","payload":{"kind":"reasoning_delta","delta":42}}),
    );
    let snapshot = trace.sink.activity.observation_snapshot();
    assert!(!snapshot.coverage.reasoning_complete);
    assert!(snapshot.coverage.tool_history_complete);
    assert_eq!(snapshot.coverage.dropped_events, 1);
    assert!(!trace.sink.activity.snapshot().telemetry_degraded);
    let malformed = trace.send(
        "session/event",
        json!({"type":"model.streaming","payload":{"kind":"tool_input_delta","toolCallId":"call"}}),
    );
    assert!(matches!(
        malformed.disposition(),
        DecodeDisposition::InvalidLost(_)
    ));
}

#[test]
fn telemetry_tool_error_preserves_native_detail_and_redacts_credentials() {
    let mut trace = Trace::new();
    trace.send("session/event", json!({"type":"turn.started"}));
    let batch = trace.send("v4/telemetry/event", json!({"kind":"tool.lifecycle","phase":"failed","toolCallId":"call","errorCode":"spawn_error","errorMessage":"full tool failure","futureField":{"token":"secret"}}));
    let EventFact::ToolChanged(tool) = &batch.events()[0].fact else {
        panic!("tool fact")
    };
    assert_eq!(
        tool.failure.as_ref().unwrap().original["errorMessage"],
        "full tool failure"
    );
    assert_eq!(
        tool.failure.as_ref().unwrap().original["futureField"]["token"],
        "[REDACTED]"
    );
    assert_eq!(
        trace.sink.activity.snapshot().window_60s.tool_calls_failed,
        1
    );
    assert!(trace.sink.activity.snapshot().active_tools.is_empty());
}

#[test]
fn native_error_result_type_is_a_failed_settlement_with_complete_original() {
    for result_type in [
        "error_max_turns",
        "error_max_budget",
        "error_during_execution",
        "error_max_tool_calls",
    ] {
        let mut trace = Trace::new();
        trace.send("session/event", json!({"type":"turn.started"}));
        let payload = json!({"response":"native limit reached", "resultType":result_type, "futureField":{"detail":true}});
        let batch = trace.send(
            "session/event",
            json!({"type":"turn.completed","payload":payload}),
        );
        let EventFact::TurnSettled(settled) = &batch.events().last().unwrap().fact else {
            panic!("settlement")
        };
        assert_eq!(settled.outcome, TurnOutcome::Failed);
        assert_eq!(settled.failure.as_ref().unwrap().original, payload);
        assert_eq!(
            trace.tracker.snapshot().boundary,
            Some(crate::TurnBoundary::Failed)
        );
    }
}

#[test]
fn known_session_configuration_and_recovery_do_not_degrade_model_activity() {
    let mut trace = Trace::new();
    trace.send("session/event", json!({"type":"turn.started"}));
    for kind in [
        "model_selected",
        "model_complete",
        "model_error",
        "compact_completed",
        "subagent_stopped",
    ] {
        let batch = trace.send(
            "session/event",
            json!({"type":"session.updated", "payload":{"type":kind}}),
        );
        assert!(
            matches!(batch.disposition(), DecodeDisposition::RecognizedIgnored(_)),
            "{kind}"
        );
    }
    let recovery = trace.send("session/event", json!({"type":"streamRecovery.updated","payload":{"attemptId":"attempt", "failureKind":"stream_interrupted"}}));
    assert!(matches!(
        recovery.disposition(),
        DecodeDisposition::RecognizedIgnored(_)
    ));
    assert!(!trace.sink.activity.snapshot().telemetry_degraded);
    let future = trace.send(
        "session/event",
        json!({"type":"session.updated","payload":{"type":"model_future_status"}}),
    );
    assert!(matches!(
        future.disposition(),
        DecodeDisposition::Unknown(_)
    ));
}

#[test]
fn valid_429_fixture_converges_through_the_shared_entry() {
    let cases: Vec<Value> = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/zcode-events/cases.json"
    ))
    .unwrap();
    let mut trace = Trace::new();
    trace.send("session/event", json!({"type":"turn.started"}));
    let mut started = cases[0]["input"]["params"].clone();
    started["status"] = json!("model_request_started");
    trace.send("v4/telemetry/event", started);
    assert!(trace.active());
    trace.send("v4/telemetry/event", cases[0]["input"]["params"].clone());
    assert!(!trace.active());
    assert!(!trace.sink.activity.snapshot().telemetry_degraded);
    assert!(trace.tracker.snapshot().active);
}

#[test]
fn zcode_set_model_error_keeps_complete_original_and_existing_classification() {
    for (code, rejected, message) in [
        (
            Some("invalid_model_request"),
            true,
            "model selection was rejected: MODEL_REJECTED: invalid_model_request",
        ),
        (
            Some("model_not_found"),
            true,
            "model selection was rejected: MODEL_REJECTED: model_not_found",
        ),
        (
            Some("provider_unavailable"),
            false,
            "invalid session response: session/setModel was rejected: provider_unavailable",
        ),
        (
            None,
            false,
            "invalid session response: session/setModel was rejected: unknown",
        ),
    ] {
        let mut original = json!({"code":-32603,"message":"upstream rejected model", "data":{
            "nested":{"apiKey":"private-key","providerStatus":429},"stack":"frame\n".repeat(20000)
        },"futureField":[1,{"preserved":true}]});
        if let Some(code) = code {
            original["data"]["code"] = json!(code);
        }
        let error = crate::remote_set_model_error(&original);
        assert_eq!(
            matches!(error, crate::RuntimeCommandError::ModelRejected(_)),
            rejected
        );
        assert_eq!(error.to_string(), message);
        assert!(
            !format!("{error:?}").contains("futureField"),
            "native original stays out of Debug diagnostics"
        );
        let failure = error
            .native_failure()
            .expect("setModel must retain the complete native value");
        let mut expected = original;
        expected["data"]["nested"]["apiKey"] = json!("[REDACTED]");
        assert_eq!(failure.original, expected);
    }
}

#[test]
fn zcode_bootstrap_remote_conversion_keeps_complete_sanitized_native_view() {
    let original = json!({"code":-32001,"message":"bootstrap refused","data":{
        "apiKey":"private-bootstrap-key", "nested":{"retryAfter":23},"stack":"bootstrap frame\n".repeat(15000)
    },"futureField":{"preserved":true}});
    let error =
        crate::RuntimeCommandError::from(external_runtime::RequestError::Remote(original.clone()));
    assert!(matches!(error, crate::RuntimeCommandError::Remote(_)));
    assert_eq!(error.to_string(), "runtime command was rejected");
    let mut expected = original;
    expected["data"]["apiKey"] = json!("[REDACTED]");
    assert_eq!(error.native_failure().unwrap().original, expected);
    let diagnostic: Value = serde_json::from_str(&error.diagnostic("session/create")).unwrap();
    assert_eq!(
        diagnostic,
        json!({"operation":"session/create","message":"runtime command was rejected", "remote_code":-32001,"remote_message":"bootstrap refused"})
    );
    for error in [
        crate::RuntimeCommandError::ModelRejected("legacy classification".to_owned().into()),
        crate::RuntimeCommandError::InvalidSession("legacy validation".into()),
    ] {
        assert!(error.native_failure().is_none());
    }
}

#[test]
fn user_input_resolution_uses_wire_identity_after_attribution() {
    for envelope_id in [WireId::String("server-7".into()), WireId::Integer(7)] {
        let mut trace = Trace::new();
        trace.send("session/event", json!({"type":"turn.started"}));
        let request = Inbound::Message(WireMessage::Request(
            external_contract::RequestEnvelope::new(
                envelope_id.clone(),
                external_contract::INTERACTION_REQUEST_USER_INPUT,
                json!({"requestId":"mcp-elicit-xyz","prompt":"Synthetic question"}),
            ),
        ));
        assert!(trace.adapter.decode(&request).is_none());
        for (session, turn, attribution) in [
            ("foreign-session", "turn-1", Attribution::ForeignSession),
            ("session-1", "older-turn", Attribution::Late),
        ] {
            let ignored = trace
                .adapter
                .decode(&Inbound::Message(WireMessage::UnknownEvent {
                    method: "session/event".into(),
                    raw: json!({"params":{
                        "type":"userInput.resolved","sessionId":session,"turnId":turn,
                        "payload":{"requestId":"mcp-elicit-xyz","cancelled":true}
                    }}),
                }))
                .unwrap();
            assert_eq!(ignored.attribution(), attribution);
            assert!(ignored.events().is_empty());
            assert_eq!(
                trace.adapter.user_inputs.get("mcp-elicit-xyz"),
                Some(&envelope_id)
            );
        }
        let resolved = trace.send(
            "session/event",
            json!({"type":"userInput.resolved", "payload":{
                "requestId":"mcp-elicit-xyz","cancelled":true
            }}),
        );
        let EventFact::InputResolved(resolution) = &resolved.events()[0].fact else {
            panic!("resolution")
        };
        assert_eq!(resolution.request_id, envelope_id);
        assert_eq!(resolution.outcome, InputOutcome::Cancelled);
        assert!(trace.adapter.user_inputs.is_empty());
        let replay = trace
            .adapter
            .decode(&Inbound::Message(WireMessage::UnknownEvent {
                method: "session/event".into(),
                raw: json!({"params":{
                    "type":"userInput.resolved","eventId":resolved.input().source.event_id,
                    "sessionId":"session-1","turnId":"turn-1",
                    "payload":{"requestId":"mcp-elicit-xyz","cancelled":true}
                }}),
            }))
            .unwrap();
        assert_eq!(replay.attribution(), Attribution::Replay);
        assert!(replay.events().is_empty());
    }
}

fn session_frame(kind: &str, turn: &str, id: &str, payload: Value) -> Inbound {
    Inbound::Message(WireMessage::UnknownEvent {
        method: "session/event".into(),
        raw: json!({"params":{"type":kind,"turnId":turn,"sessionId":"session-1","eventId":id,"payload":payload}}),
    })
}

#[test]
fn replay_window_eviction_keeps_authoritative_terminals_usable() {
    for failed in [false, true] {
        let mut trace = Trace::new();
        trace.send("session/event", json!({"type":"turn.started"}));
        for _ in 0..65_537 {
            trace.send("state.updated", json!({}));
        }
        let terminal = trace.send("session/event", if failed {
            json!({"type":"turn.failed","payload":{"error":{"message":"complete native failure","futureField":true}}})
        } else { json!({"type":"turn.completed","payload":{"response":"complete final text"}}) });
        assert!(
            terminal
                .events()
                .iter()
                .any(|event| matches!(event.fact, EventFact::TurnSettled(_))),
            "fresh authoritative terminal must survive replay-cache saturation"
        );
        assert!(!trace.tracker.snapshot().active);
        if failed {
            let original: Value =
                serde_json::from_str(&trace.tracker.failure_detail().unwrap()).unwrap();
            assert_eq!(
                original["original_error"],
                json!({"message":"complete native failure","futureField":true})
            );
        } else {
            assert_eq!(
                trace.sink.activity.take_terminal_text(),
                crate::TerminalText::Visible("complete final text".into())
            );
        }
        assert_eq!(
            trace
                .sink
                .activity
                .observation_snapshot()
                .coverage
                .dropped_events,
            0
        );
        assert!(trace.adapter.frames.len() <= 65_536);
        let replay = trace
            .adapter
            .decode(&session_frame(
                if failed {
                    "turn.failed"
                } else {
                    "turn.completed"
                },
                "turn-1",
                terminal.input().source.event_id.as_deref().unwrap(),
                json!({}),
            ))
            .unwrap();
        assert_eq!(replay.attribution(), Attribution::Replay);
        let start = trace
            .adapter
            .decode(&session_frame(
                "turn.started",
                "turn-2",
                "next-start",
                json!({}),
            ))
            .unwrap();
        trace.publisher.emit_typed(start, &trace.tracker);
        assert!(trace.tracker.snapshot().active);
        let late = trace
            .adapter
            .decode(&session_frame(
                "model.streaming",
                "turn-1",
                "evicted-old-turn",
                json!({"kind":"text_delta","delta":"must not leak"}),
            ))
            .unwrap();
        assert_eq!(late.attribution(), Attribution::Late);
        let done = trace
            .adapter
            .decode(&session_frame(
                "turn.completed",
                "turn-2",
                "next-final",
                json!({"response":"next turn complete"}),
            ))
            .unwrap();
        trace.publisher.emit_typed(done, &trace.tracker);
        assert_eq!(trace.tracker.snapshot().generation, 2);
        assert!(!trace.tracker.snapshot().active);
        assert_eq!(
            trace.sink.activity.take_terminal_text(),
            crate::TerminalText::Visible("next turn complete".into())
        );
    }
}

#[test]
fn session_tool_result_failure_retains_nested_error() {
    let mut trace = Trace::new();
    trace.send("session/event", json!({"type":"turn.started"}));
    trace.send("session/event", json!({"type":"tool.updated","payload":{"kind":"started","toolCallId":"call","toolName":"Bash"}}));
    let error = json!({"code":"spawn_error","message":"complete native cause", "stack":"frame\n".repeat(2000),"nested":{"apiKey":"private-key","status":503},"futureField":[1,2]});
    let result = json!({"success":false,"content":"failure output","error":error});
    let batch = trace.send("session/event", json!({"type":"tool.updated","payload":{"kind":"result","toolCallId":"call","result":result}}));
    let EventFact::ToolChanged(tool) = &batch.events()[0].fact else {
        panic!("tool")
    };
    assert_eq!(tool.phase, Some(ToolPhase::Finished));
    assert_eq!(
        tool.outcome,
        Some(ToolOutcome::Failed),
        "native false success cannot become a successful tool fact"
    );
    let mut expected = error;
    expected["nested"]["apiKey"] = json!("[REDACTED]");
    assert_eq!(tool.failure.as_ref().unwrap().original, expected);
    let activity = trace.sink.activity.snapshot();
    assert_eq!(activity.window_60s.tool_calls_failed, 1);
    assert_eq!(activity.window_60s.tool_calls_completed, 0);
    assert!(activity.active_tools.is_empty());
    for (result, outcome) in [
        (
            json!({"success":true,"content":"ok"}),
            ToolOutcome::Succeeded,
        ),
        (
            json!({"content":"finished without outcome evidence"}),
            ToolOutcome::Unknown,
        ),
    ] {
        let mut trace = Trace::new();
        let batch = trace.send("session/event", json!({"type":"tool.updated","payload":{"kind":"result","toolCallId":"call","result":result}}));
        let EventFact::ToolChanged(tool) = &batch.events()[0].fact else {
            panic!("tool")
        };
        assert_eq!(tool.phase, Some(ToolPhase::Finished));
        assert_eq!(tool.outcome, Some(outcome));
    }
    for telemetry_first in [false, true] {
        let mut trace = Trace::new();
        trace.send("session/event", json!({"type":"turn.started"}));
        trace.send("session/event", json!({"type":"tool.updated","payload":{"kind":"started","toolCallId":"call","toolName":"Bash"}}));
        let native = json!({"type":"tool.updated","payload":{"kind":"result","toolCallId":"call","result":{"success":false,"content":"failed","error":{"message":"native failure"}}}});
        let telemetry_payload = json!({"kind":"tool.lifecycle","phase":"failed","toolCallId":"call","errorMessage":"native failure"});
        for telemetry in [telemetry_first, !telemetry_first] {
            let batch = if telemetry {
                trace.send("v4/telemetry/event", telemetry_payload.clone())
            } else {
                trace.send("session/event", native.clone())
            };
            let EventFact::ToolChanged(tool) = &batch.events()[0].fact else {
                panic!("tool")
            };
            assert_eq!(tool.outcome, Some(ToolOutcome::Failed));
        }
        let window = trace.sink.activity.snapshot().window_60s;
        assert_eq!(window.tool_calls_started, 1);
        assert_eq!(window.tool_calls_failed, 1);
        assert_eq!(window.tool_calls_completed, 0);
    }
}

#[test]
fn unknown_impact_preserves_current_and_historical_state() {
    let mut trace = Trace::new();
    trace.send("session/event", json!({"type":"turn.started"}));
    trace.model("request", "model_request_started");
    trace.send("session/event", json!({"type":"model.streaming","payload":{"kind":"reasoning_delta","delta":"public reasoning"}}));
    trace.send("session/event", json!({"type":"tool.updated","payload":{"kind":"started","toolCallId":"call","toolName":"Bash","input":{"command":"pwd"}}}));
    let before = trace.sink.activity.observation_snapshot();
    let unknown = trace.send(
        "session/event",
        json!({"type":"future.fact","payload":{"private":"not a diagnostic"}}),
    );
    assert!(matches!(
        unknown.disposition(),
        DecodeDisposition::Unknown(Diagnostic {
            impact: Impact::Unknown,
            ..
        })
    ));
    assert!(matches!(
        &unknown.events()[0].fact,
        EventFact::Diagnostic(Diagnostic {
            reason: DiagnosticReason::UnknownSemantics,
            impact: Impact::Unknown,
            ..
        })
    ));
    let activity = trace.sink.activity.snapshot();
    let observed = trace.sink.activity.observation_snapshot();
    assert_eq!(
        (
            activity.telemetry_degraded,
            observed.coverage.tool_history_complete,
            observed.coverage.reasoning_complete,
            observed.coverage.dropped_events
        ),
        (
            false,
            before.coverage.tool_history_complete,
            before.coverage.reasoning_complete,
            0
        ),
        "unknown impact is uncertainty, not established current or historical damage"
    );
    assert!(activity.model_request_active);
    assert_eq!(activity.active_tools.len(), 1);
    assert_eq!(observed.tools, before.tools);
    assert_eq!(observed.reasoning, before.reasoning);
    let projection = crate::projection::lifecycle_projection(
        &trace.sink.records.lock().unwrap().last().unwrap().event,
        None,
    );
    assert!(projection.payload_json.contains("unknown"));
    assert!(!projection.payload_json.contains("not a diagnostic"));
    trace.send(
        "v4/telemetry/event",
        json!({"kind":"model.request.status","status":"model_request_started"}),
    );
    assert!(trace.sink.activity.snapshot().telemetry_degraded);
    let damaged = trace.sink.activity.observation_snapshot();
    assert!(damaged.coverage.tool_history_complete);
    assert!(damaged.coverage.reasoning_complete);
    assert_eq!(damaged.coverage.dropped_events, 1);
}

#[test]
fn replay_window_retirement_preserves_uncertainty_without_asserting_event_loss() {
    let mut trace = Trace::new();
    trace.send("session/event", json!({"type":"turn.started"}));
    let first = trace.send("state.updated", json!({}));
    let old_key = first.input().frame_key().unwrap();
    let mut latest = first;
    for _ in 0..65_537 {
        latest = trace.send("state.updated", json!({}));
    }
    assert!(!trace.adapter.frames.contains(&old_key));
    assert_eq!(trace.adapter.frames.len(), MAX_REPLAY_FRAMES);
    assert_eq!(trace.adapter.frame_order.len(), MAX_REPLAY_FRAMES);
    let ignored_frame = |id: &str| {
        Inbound::Message(WireMessage::UnknownEvent {
            method: "state.updated".into(),
            raw: json!({"params":{
                "eventId":id,"sessionId":"session-1","turnId":"turn-1"
            }}),
        })
    };
    let recent = trace
        .adapter
        .decode(&ignored_frame(
            latest.input().source.event_id.as_deref().unwrap(),
        ))
        .unwrap();
    assert_eq!(recent.attribution(), Attribution::Replay);
    let evicted = trace
        .adapter
        .decode(&ignored_frame(&old_key.event_id))
        .unwrap();
    assert_eq!(
        evicted.attribution(),
        Attribution::Current,
        "an evicted key has no remembered replay proof"
    );
    assert!(matches!(
        evicted.disposition(),
        DecodeDisposition::RecognizedIgnored(_)
    ));
    assert!(evicted.events().is_empty());
    assert!(trace.tracker.snapshot().active);
    assert!(!trace.sink.activity.snapshot().telemetry_degraded);
    let coverage = trace.sink.activity.observation_snapshot().coverage;
    assert_eq!(coverage.dropped_events, 0);
    assert!(coverage.tool_history_complete && coverage.reasoning_complete);
}

#[test]
fn unknown_semantics_and_known_damage_keep_their_supported_scope() {
    let mut trace = Trace::new();
    trace.send("session/event", json!({"type":"turn.started"}));
    trace.send(
        "session/event",
        json!({"type":"tool.updated","payload":{"kind":"future_phase","toolCallId":"call"}}),
    );
    assert!(
        !trace.sink.activity.snapshot().telemetry_degraded,
        "an unknown phase alone does not establish a broken correlation"
    );
    let clean = trace.sink.activity.observation_snapshot().coverage;
    assert_eq!(clean.dropped_events, 0);
    assert!(clean.tool_history_complete && clean.reasoning_complete);
    trace.send(
        "session/event",
        json!({"type":"tool.updated","payload":{"kind":"started"}}),
    );
    assert!(trace.sink.activity.snapshot().telemetry_degraded);
    let known_damage = trace.sink.activity.observation_snapshot().coverage;
    assert!(!known_damage.tool_history_complete);
    assert!(known_damage.reasoning_complete);
    assert_eq!(known_damage.dropped_events, 1);
    // A known damaged tool frame outside any observed turn affects tool history,
    // with no evidence of a current correlation to mark degraded.
    let mut trace = Trace::new();
    let no_turn = Inbound::Message(WireMessage::UnknownEvent {
        method: "session/event".into(),
        raw: json!({"params":{
            "type":"tool.updated","payload":{"kind":"started"}
        }}),
    });
    let batch = trace.adapter.decode(&no_turn).unwrap();
    assert!(matches!(
        batch.disposition(),
        DecodeDisposition::InvalidLost(CoverageGap {
            activity: ActivityScope::None,
            ..
        })
    ));
    trace.publisher.emit_typed(batch, &trace.tracker);
    assert!(!trace.sink.activity.snapshot().telemetry_degraded);
    let historical = trace.sink.activity.observation_snapshot().coverage;
    assert!(!historical.tool_history_complete);
    assert!(historical.reasoning_complete);
    assert_eq!(historical.dropped_events, 1);
}
