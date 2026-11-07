use super::*;
use crate::{parse_line, WireMessage};
use serde_json::json;
use std::collections::HashSet;

fn context(sequence: u64) -> EventContext {
    EventContext {
        task_id: TaskId("task-1".into()),
        launch_id: LaunchId("launch-1".into()),
        sequence,
        turn_id: Some(TurnId::Local(1)),
        source: NativeSource {
            protocol: SourceProtocol::AgyStreamJson,
            discriminator: SafeToken::new("step_update"),
            session_id: Some(SessionId("session-1".into())),
            turn_id: None,
            item_id: Some("step-2".into()),
            call_id: None,
            event_id: None,
            timestamp: None,
        },
    }
}

fn input() -> NativeInputContext {
    context(0).input_context()
}

fn event(sequence: u64, fact: EventFact) -> NormalizedEvent {
    NormalizedEvent {
        context: context(sequence),
        fact,
    }
}

fn failure() -> NativeFailure {
    NativeFailure {
        original: json!({"code":-32000,"message":"full native error", "data":{"detail":[1,2,3]}}),
        projection: Some(FailureProjection {
            code: Some(json!(-32000)),
            status: Some(429),
            retry: Some(NativeRetry {
                will_retry: Some(true),
                ..Default::default()
            }),
        }),
    }
}

fn diagnostic() -> Diagnostic {
    Diagnostic {
        reason: DiagnosticReason::UnknownSemantics,
        impact: Impact::Unknown,
        discriminator: SafeToken::new("future.fact"),
    }
}

fn gap() -> CoverageGap {
    CoverageGap {
        reason: LossReason::Dropped,
        impact: Impact::Known(vec![CoverageDimension::Tools]),
        activity: ActivityScope::Tool(CallId("call-1".into())),
    }
}

fn tool(phase: Option<ToolPhase>) -> ToolChange {
    ToolChange {
        call_id: CallId("call-1".into()),
        name: None,
        category: None,
        arguments: None,
        phase,
        outcome: None,
        failure: None,
        denied: None,
    }
}

fn round_trip<T>(value: T)
where
    T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
{
    let encoded = serde_json::to_string(&value).unwrap();
    assert_eq!(serde_json::from_str::<T>(&encoded).unwrap(), value);
}

#[test]
fn every_event_family_round_trips_with_distinct_safe_lifecycle_category() {
    let session = SessionInfo {
        session_id: SessionId("session-1".into()),
        model: Some("observed-model".into()),
        capabilities: vec![CapabilityObservation {
            capability: Capability::PublicReasoning,
            available: false,
        }],
    };
    let facts = vec![
        (
            EventFact::SessionReady(session.clone()),
            LifecycleCategory::Session,
        ),
        (
            EventFact::SessionChanged(session),
            LifecycleCategory::Session,
        ),
        (
            EventFact::TurnStarted {
                turn_id: TurnId::Local(1),
            },
            LifecycleCategory::Turn,
        ),
        (
            EventFact::TurnSettled(TurnSettlement {
                turn_id: TurnId::Native("native-turn".into()),
                outcome: TurnOutcome::Failed,
                stop_reason: SafeToken::new("error"),
                final_text: Some("final text".into()),
                failure: Some(failure()),
            }),
            LifecycleCategory::Turn,
        ),
        (
            EventFact::MessageUpdated(MessageUpdate {
                message_id: MessageId("message".into()),
                text: TextUpdate::Delta("repeat".into()),
            }),
            LifecycleCategory::Message,
        ),
        (
            EventFact::MessageFinished {
                message_id: MessageId("message".into()),
            },
            LifecycleCategory::Message,
        ),
        (
            EventFact::PublicReasoningDelta(PublicReasoning {
                message_id: None,
                part_id: Some("part".into()),
                channel: PublicReasoningChannel::Summary,
                delta: "public only".into(),
            }),
            LifecycleCategory::PublicReasoning,
        ),
        (
            EventFact::ToolChanged(tool(Some(ToolPhase::Finished))),
            LifecycleCategory::Tool,
        ),
        (
            EventFact::ModelActivityChanged(ModelActivity {
                request_id: Some(RequestId("request".into())),
                phase: ModelPhase::RetryScheduled,
                retry: Some(NativeRetry {
                    attempt: Some(1),
                    next_attempt: Some(2),
                    delay_ms: Some(500),
                    ..Default::default()
                }),
                failure: None,
            }),
            LifecycleCategory::ModelActivity,
        ),
        (
            EventFact::InputRequested(InputRequest {
                request_id: crate::WireId::String("request".into()),
                kind: InputKind::Question,
                respondable: true,
                question: json!({"question":"full question", "options":["A","B"]}),
            }),
            LifecycleCategory::Input,
        ),
        (
            EventFact::InputResolved(InputResolution {
                request_id: crate::WireId::String("request".into()),
                outcome: InputOutcome::Denied,
                call_id: Some(CallId("call".into())),
            }),
            LifecycleCategory::Input,
        ),
        (
            EventFact::ErrorObserved {
                operation: Operation::Send,
                failure: failure(),
            },
            LifecycleCategory::Error,
        ),
        (
            EventFact::Diagnostic(diagnostic()),
            LifecycleCategory::Diagnostic,
        ),
        (EventFact::CoverageGap(gap()), LifecycleCategory::Coverage),
        (
            EventFact::RuntimeExited {
                exit_code: Some(1),
                signal: None,
                stderr_ref: Some("stderr-ref".into()),
            },
            LifecycleCategory::Runtime,
        ),
        (
            EventFact::TransportFault {
                kind: TransportFaultKind::FrameLimit,
                failure: Some(failure()),
                stderr_ref: None,
            },
            LifecycleCategory::Transport,
        ),
    ];
    for (index, (fact, expected)) in facts.into_iter().enumerate() {
        assert_eq!(fact.lifecycle_category(), expected);
        let encoded_category = serde_json::to_string(&fact.lifecycle_category()).unwrap();
        assert!(!encoded_category.contains("full native error"));
        round_trip(event(index as u64 + 1, fact));
    }
    round_trip(TextUpdate::Snapshot("replace".into()));
    for update in [
        ArgumentUpdate::Delta("{\"x\":".into()),
        ArgumentUpdate::Snapshot(json!({"x":1})),
        ArgumentUpdate::MergePatch(json!({"y":2})),
    ] {
        round_trip(update);
    }
}

#[test]
fn disposition_truth_table_rejects_wrong_classification_and_attribution() {
    let facts = vec![event(
        1,
        EventFact::TurnStarted {
            turn_id: TurnId::Local(1),
        },
    )];
    // Classification is adapter-owned; these assertions enforce the shared
    // consequences, including normal ignores producing no loss/diagnostic facts.
    for (disposition, empty_ok, populated_ok, wire_kind) in [
        (DecodeDisposition::Consumed, false, true, "consumed"),
        (
            DecodeDisposition::RecognizedIgnored(IgnoreReason::Usage),
            true,
            false,
            "recognized_ignored",
        ),
        (
            DecodeDisposition::Unknown(diagnostic()),
            true,
            true,
            "unknown",
        ),
        (
            DecodeDisposition::InvalidLost(gap()),
            true,
            true,
            "invalid_lost",
        ),
    ] {
        assert_eq!(
            serde_json::to_value(&disposition).unwrap()["kind"],
            wire_kind
        );
        for (events, expected) in [(vec![], empty_ok), (facts.clone(), populated_ok)] {
            let result =
                DecodedInput::new(input(), Attribution::Current, disposition.clone(), events);
            assert_eq!(result.is_ok(), expected, "{wire_kind}");
            if let Ok(batch) = result {
                round_trip(batch);
            }
        }
    }
    for (attribution, reason) in [
        (Attribution::Late, IgnoreReason::Late),
        (Attribution::Replay, IgnoreReason::Replay),
        (Attribution::ForeignSession, IgnoreReason::ForeignSession),
    ] {
        let batch = DecodedInput::new(
            input(),
            attribution,
            DecodeDisposition::RecognizedIgnored(reason),
            vec![],
        )
        .unwrap();
        round_trip(batch);
        assert_eq!(
            DecodedInput::new(
                input(),
                attribution,
                DecodeDisposition::Consumed,
                facts.clone()
            ),
            Err(BatchError::AttributionMismatch)
        );
        assert_eq!(
            DecodedInput::new(
                input(),
                Attribution::Current,
                DecodeDisposition::RecognizedIgnored(reason),
                vec![]
            ),
            Err(BatchError::AttributionMismatch)
        );
    }
    let damaged_as_unknown = Diagnostic {
        reason: DiagnosticReason::Dropped,
        ..diagnostic()
    };
    assert_eq!(
        DecodedInput::new(
            input(),
            Attribution::Current,
            DecodeDisposition::Unknown(damaged_as_unknown),
            vec![]
        ),
        Err(BatchError::DispositionReasonMismatch)
    );
    let unknown_as_loss = CoverageGap {
        reason: LossReason::UnknownSemantics,
        ..gap()
    };
    assert_eq!(
        DecodedInput::new(
            input(),
            Attribution::Current,
            DecodeDisposition::InvalidLost(unknown_as_loss),
            vec![]
        ),
        Err(BatchError::DispositionReasonMismatch)
    );
    let mut native_input = input();
    native_input.source.event_id = Some("unknown-frame".into());
    let unknown = DecodedInput::new(
        native_input.clone(),
        Attribution::Current,
        DecodeDisposition::Unknown(diagnostic()),
        vec![],
    )
    .unwrap();
    assert_eq!(unknown.input().frame_key(), native_input.frame_key());
    round_trip(unknown);
    // The decode path must enforce the same table as constructors.
    assert!(serde_json::from_value::<DecodedInput>(
        json!({"input": input(), "attribution":"current","disposition":{"kind":"consumed"},"events":[]})
    )
    .is_err());
}

#[test]
fn ordered_settlement_batch_is_launch_scoped_and_decode_validated() {
    let mut events = vec![
        event(
            7,
            EventFact::MessageUpdated(MessageUpdate {
                message_id: MessageId("m".into()),
                text: TextUpdate::Snapshot("final".into()),
            }),
        ),
        event(
            8,
            EventFact::TurnSettled(TurnSettlement {
                turn_id: TurnId::Local(1),
                outcome: TurnOutcome::Completed,
                stop_reason: SafeToken::new("end_turn"),
                final_text: None,
                failure: None,
            }),
        ),
    ];
    round_trip(
        DecodedInput::new(
            input(),
            Attribution::Current,
            DecodeDisposition::Consumed,
            events.clone(),
        )
        .unwrap(),
    );
    events[1].context.sequence = 7;
    assert_eq!(
        DecodedInput::new(
            input(),
            Attribution::Current,
            DecodeDisposition::Consumed,
            events.clone()
        ),
        Err(BatchError::Unordered)
    );
    events[1].context.sequence = 8;
    events[1].context.launch_id = LaunchId("other-launch".into());
    assert_eq!(
        DecodedInput::new(
            input(),
            Attribution::Current,
            DecodeDisposition::Consumed,
            events
        ),
        Err(BatchError::MixedLaunch)
    );
}

#[test]
fn replay_call_and_phase_identity_are_independent_without_content_hashes() {
    let key = LogicalCallKey {
        task_id: TaskId("task-1".into()),
        launch_id: LaunchId("launch-1".into()),
        turn_id: TurnId::Local(1),
        call_id: CallId("call-1".into()),
    };
    let mut completion = tool(Some(ToolPhase::Finished));
    completion.outcome = Some(ToolOutcome::Unknown);
    let mut patch = tool(None);
    patch.arguments = Some(ArgumentUpdate::MergePatch(json!({"path":"."})));
    // Consumer identity oracle, not a RuntimeState implementation: completion
    // alone and later arguments address exactly one logical call; no start time
    // or successful outcome can be inferred from these facts.
    let calls: HashSet<_> = [completion.clone(), patch]
        .iter()
        .map(|c| LogicalCallKey {
            call_id: c.call_id.clone(),
            ..key.clone()
        })
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(completion.phase, Some(ToolPhase::Finished));
    assert_eq!(completion.outcome, Some(ToolOutcome::Unknown));
    assert_ne!(
        CallPhaseKey {
            call: key.clone(),
            phase: ToolPhase::Started
        },
        CallPhaseKey {
            call: key.clone(),
            phase: ToolPhase::Finished
        }
    );
    assert_ne!(
        key,
        LogicalCallKey {
            turn_id: TurnId::Local(2),
            ..key.clone()
        }
    );
    let text = EventFact::MessageUpdated(MessageUpdate {
        message_id: MessageId("m".into()),
        text: TextUpdate::Delta("again".into()),
    });
    let first = event(1, text.clone());
    let second = event(2, text);
    assert_eq!(first.context.frame_key(), None);
    assert_eq!(second.context.frame_key(), None);
    assert_eq!(
        DecodedInput::new(
            input(),
            Attribution::Current,
            DecodeDisposition::Consumed,
            vec![first, second]
        )
        .unwrap()
        .events()
        .len(),
        2
    );
    let mut a = context(3);
    a.source.event_id = Some("native-event".into());
    let mut b = a.clone();
    b.sequence = 4;
    assert_eq!(a.frame_key(), b.frame_key());
    b.launch_id = LaunchId("new-launch".into());
    assert_ne!(a.frame_key(), b.frame_key());
    b = a.clone();
    b.source.session_id = Some(SessionId("foreign-session".into()));
    assert_ne!(a.frame_key(), b.frame_key());
    // Local/native turn IDs have distinct serde identities, never session aliases.
    assert_ne!(
        serde_json::to_value(TurnId::Local(1)).unwrap(),
        serde_json::to_value(TurnId::Native("1".into())).unwrap()
    );
}

#[test]
fn native_failure_retains_complete_nested_original_without_projection() {
    let original = json!({"message":"原始错误".repeat(4000),"error":{"data":[{"code":429,"unknownField":true}]},"willRetry":true});
    let value = NativeFailure {
        original: original.clone(),
        projection: None,
    };
    round_trip(value.clone());
    assert_eq!(serde_json::to_value(value).unwrap()["original"], original);
}

#[test]
fn diagnostic_tokens_are_bounded_and_deserialization_cannot_bypass_validation() {
    for token in [
        "model.request.status",
        "item/tool-call",
        "agent_message_chunk",
        "v4/telemetry/event",
    ] {
        round_trip(SafeToken::new(token).unwrap());
    }
    for token in [
        "",
        "Bearer secret",
        "token=secret",
        "password:secret",
        "line\ntext",
        "用户正文",
        "https://user:pass@host",
        &"x".repeat(65),
    ] {
        assert!(SafeToken::new(token).is_none());
        assert!(serde_json::from_value::<SafeToken>(json!(token)).is_err());
    }
    let summary = LifecycleSummary {
        category: Some(LifecycleCategory::Error),
        source: SourceProtocol::CodexAppServer,
        discriminator: SafeToken::new("error"),
        disposition: DispositionKind::Consumed,
        reason: None,
        impact: None,
    };
    round_trip(summary.clone());
    let mut injected = serde_json::to_value(summary).unwrap();
    injected["payload"] = json!({"secret":"must not enter summary"});
    assert!(serde_json::from_value::<LifecycleSummary>(injected).is_err());
    assert!(serde_json::from_value::<Diagnostic>(
        json!({"reason":"provider free text", "impact":{"scope":"unknown"}, "discriminator":null})
    )
    .is_err());
}

#[test]
fn native_input_request_identity_preserves_integer_and_string_ids() {
    let numeric = InputRequest {
        request_id: crate::WireId::Integer(1),
        kind: InputKind::Permission,
        respondable: true,
        question: json!({"question":"Allow?"}),
    };
    let string = InputRequest {
        request_id: crate::WireId::String("1".into()),
        ..numeric.clone()
    };
    assert_ne!(numeric.request_id, string.request_id);
    round_trip(numeric);
    round_trip(string);
}

fn fixture(channel: &str, name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(format!("{channel}-events"))
        .join(name);
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn zcode_unknown_envelope_can_supply_consumed_ignored_unknown_or_invalid_inputs() {
    let cases = fixture("zcode", "cases.json");
    let mut classes = HashSet::new();
    for case in cases.as_array().unwrap() {
        let wire = parse_line(&case["input"].to_string()).unwrap();
        assert!(matches!(wire, WireMessage::UnknownEvent { .. }));
        let (method, params) = wire.notification().unwrap();
        assert_eq!(method, "v4/telemetry/event");
        let kind = params["kind"].as_str().unwrap();
        // Bounded fixture oracle only. Production native interpretation belongs
        // to the adapter; adding a sample requires an explicit expected mapping.
        let expected = match kind {
            "model.request.status" if params.get("requestId").is_none() => ("invalid_lost", vec![]),
            "model.request.status" => {
                assert!(matches!(
                    params["status"].as_str().unwrap(),
                    "model_request_failed" | "model_retry_scheduled"
                ));
                if params["status"] == "model_request_failed" {
                    assert_eq!(params["statusCode"], 429);
                }
                ("consumed", vec!["model_activity_changed"])
            }
            "turn.terminal" => {
                assert!(matches!(
                    params["status"].as_str().unwrap(),
                    "success" | "interrupted" | "failed"
                ));
                ("consumed", vec!["diagnostic"])
            }
            "usage.delta" => ("recognized_ignored", vec![]),
            "future.fact" => ("unknown", vec![]),
            other => panic!("unreviewed sample kind {other}"),
        };
        assert_eq!(case["expected_disposition"], expected.0, "{}", case["name"]);
        assert_eq!(case["expected_facts"], json!(expected.1));
        classes.insert(expected.0);
        assert!(case["source"].is_string());
    }
    assert_eq!(classes.len(), 4);
    let logs = fixture("zcode", "retry-log-observed.json");
    assert_eq!(logs["provenance"], "OBSERVED");
    assert_eq!(logs["records"].as_array().unwrap().len(), 8);
    let started =
        parse_line(r#"{"method":"session/event","params":{"type":"turn.started"}}"#).unwrap();
    assert!(matches!(started, WireMessage::Event(_)));
    assert_eq!(started.notification().unwrap().1["type"], "turn.started");
    assert!(parse_line(r#"{"id":1,"result":{}}"#)
        .unwrap()
        .notification()
        .is_none());
}

#[test]
fn source_inspected_codex_and_dsh_fixture_classifications_keep_native_semantics() {
    for channel in ["codex", "dsh"] {
        for case in fixture(channel, "cases.json").as_array().unwrap() {
            assert_eq!(case["provenance"], "SOURCE_INSPECTED");
            assert_eq!(case["expected_disposition"], "consumed");
            let raw = &case["input"];
            let expected = match raw["method"].as_str() {
                Some("error") => {
                    assert_eq!(raw["params"]["willRetry"], true);
                    assert!(raw["params"]["error"]["codexErrorInfo"].is_object());
                    vec!["error_observed"]
                }
                Some("turn/completed") => {
                    assert_eq!(raw["params"]["turn"]["status"], "failed");
                    vec!["turn_settled"]
                }
                Some("session/update") => {
                    match raw["params"]["update"]["sessionUpdate"].as_str().unwrap() {
                        "agent_message_chunk" => {
                            assert!(raw["params"]["update"]["content"]["text"].is_string());
                            vec!["message_updated"]
                        }
                        "tool_call" => {
                            assert_ne!(
                                raw["params"]["update"]["title"],
                                raw["params"]["update"]["kind"]
                            );
                            vec!["tool_changed"]
                        }
                        "tool_call_update" => {
                            assert_eq!(raw["params"]["update"]["status"], "failed");
                            vec!["tool_changed"]
                        }
                        other => panic!("unreviewed update {other}"),
                    }
                }
                None => {
                    assert_eq!(raw["result"], json!({"stopReason":"end_turn"}));
                    vec!["message_finished", "turn_settled"]
                }
                other => panic!("unreviewed method {other:?}"),
            };
            assert_eq!(case["expected_facts"], json!(expected));
        }
    }
}

#[test]
fn both_observed_agy_captures_keep_protocol_and_completion_evidence_distinct() {
    let stream = fixture("agy", "stream-observed.json");
    let acp = fixture("agy", "acp-observed.json");
    assert_eq!(stream["provenance"], "OBSERVED");
    assert_eq!(acp["provenance"], "OBSERVED");
    assert_eq!(acp["role"], "evidence_only_unsupported_acp");
    assert!(acp["frames"]
        .as_array()
        .unwrap()
        .iter()
        .all(|frame| frame["jsonrpc"] == "2.0"));
    let frames = stream["frames"].as_array().unwrap();
    assert_eq!(stream["expectations_provenance"], "PLANNED");
    let expectations = stream["expectations"].as_array().unwrap();
    assert_eq!(expectations.len(), frames.len());
    for (frame, expectation) in frames.iter().zip(expectations) {
        let expected = match frame["event"].as_str().unwrap() {
            "init" => ("consumed", vec!["session_ready"]),
            "step_update" => match frame["step_update"]["step_type"].as_str().unwrap() {
                "tool" => ("consumed", vec!["tool_changed"]),
                "user_input" => ("recognized_ignored", vec![]),
                "agent_response" if frame["step_update"].get("text_delta").is_none() => {
                    ("recognized_ignored", vec![])
                }
                "agent_response" if frame["step_update"]["state"] == "DONE" => {
                    ("consumed", vec!["message_updated", "message_finished"])
                }
                "agent_response" => ("consumed", vec!["message_updated"]),
                other => panic!("unreviewed observed step {other}"),
            },
            "result" => ("consumed", vec!["turn_settled"]),
            other => panic!("unreviewed observed event {other}"),
        };
        assert_eq!(expectation["disposition"], expected.0);
        assert_eq!(expectation["facts"], json!(expected.1));
    }
    assert_eq!(expectations[4]["tool_outcome"], "unknown");
    assert_eq!(frames.len(), 8);
    assert_eq!(frames[0]["init"]["model"], "gemini-3.8-flash-low");
    assert_eq!(frames[3]["step_update"]["state"], "ACTIVE");
    assert_eq!(frames[4]["step_update"]["state"], "DONE");
    assert_eq!(
        frames[3]["step_update"]["step_index"],
        frames[4]["step_update"]["step_index"]
    );
    assert!(frames[4]["step_update"]["tool_info"].get("error").is_none());
    // DONE has no explicit success evidence. Prose output and token usage are
    // intentionally absent from the planned typed tool fact.
    let done = ToolChange {
        name: Some("run_command".into()),
        outcome: Some(ToolOutcome::Unknown),
        ..tool(Some(ToolPhase::Finished))
    };
    round_trip(EventFact::ToolChanged(done));
    assert_eq!(frames[7]["result"]["status"], "SUCCESS");
    assert_eq!(frames[7]["result"]["response"], "OK\n");
    assert_eq!(frames[5]["step_update"]["text_delta"], "OK");
    assert_eq!(frames[6]["step_update"]["text_delta"], "\n");
}
