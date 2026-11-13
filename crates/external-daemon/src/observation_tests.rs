//! Synthetic facts exercise common state; native fixture ingress is tested in
//! the adapter suites and the persistence tests in scheduler/tests.rs.
use crate::*;
use external_contract::events::*;

fn event(fact: EventFact) -> NormalizedEvent {
    scoped(fact, "launch", 1)
}
fn scoped(fact: EventFact, launch: &str, turn: u64) -> NormalizedEvent {
    NormalizedEvent {
        context: EventContext {
            task_id: TaskId("task".into()),
            launch_id: LaunchId(launch.into()),
            sequence: 1,
            turn_id: Some(TurnId::Local(turn)),
            source: NativeSource {
                protocol: SourceProtocol::ZcodeSession,
                discriminator: SafeToken::new("api_key.PRIVATE_CREDENTIAL"),
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
fn model(id: Option<&str>, phase: ModelPhase) -> EventFact {
    EventFact::ModelActivityChanged(ModelActivity {
        request_id: id.map(|id| RequestId(id.into())),
        phase,
        retry: None,
        failure: None,
    })
}
fn tool(id: &str, phase: ToolPhase) -> EventFact {
    EventFact::ToolChanged(ToolChange {
        call_id: CallId(id.into()),
        name: Some("RunCommand".into()),
        category: None,
        arguments: None,
        phase: Some(phase),
        outcome: Some(ToolOutcome::Unknown),
        failure: None,
        denied: None,
    })
}
fn gap(scope: ActivityScope, dimension: CoverageDimension) -> EventFact {
    EventFact::CoverageGap(CoverageGap {
        reason: LossReason::Malformed,
        impact: Impact::Known(vec![dimension]),
        activity: scope,
    })
}
fn settled(turn: u64) -> EventFact {
    EventFact::TurnSettled(TurnSettlement {
        turn_id: TurnId::Local(turn),
        outcome: TurnOutcome::Completed,
        stop_reason: None,
        final_text: None,
        failure: None,
    })
}
fn start(state: &RuntimeState) {
    state.reduce_batch(&[event(EventFact::TurnStarted {
        turn_id: TurnId::Local(1),
    })]);
}

#[test]
fn empty_observation_has_unavailable_current_state_and_unknown_capabilities() {
    let state = RuntimeState::new();
    let snapshot = state.snapshot();
    assert_eq!(snapshot.telemetry_status, TelemetryStatus::Unavailable);
    assert_eq!(snapshot.coverage, RuntimeCoverage::default());
    assert_eq!(snapshot.model_activity, None);
    assert!(!state.observation_snapshot().coverage.tool_history_complete);
    assert!(!state.observation_snapshot().coverage.reasoning_complete);
    start(&state);
    assert_eq!(state.snapshot().telemetry_status, TelemetryStatus::Healthy);
    assert_eq!(
        state.snapshot().coverage.model_activity,
        None,
        "turn activity cannot invent model-request capability"
    );
}

#[test]
fn unrelated_frames_and_one_tool_terminal_do_not_repair_other_correlations() {
    let state = RuntimeState::new();
    start(&state);
    for id in ["a", "b"] {
        state.reduce_batch(&[event(gap(
            ActivityScope::Tool(CallId(id.into())),
            CoverageDimension::Tools,
        ))]);
    }
    let diagnostics = state.snapshot().diagnostics;
    assert_eq!(diagnostics.iter().map(|d| d.count).sum::<u64>(), 2);
    state.reduce_batch(&[event(EventFact::MessageUpdated(MessageUpdate {
        message_id: MessageId("m".into()),
        text: TextUpdate::Delta("normal".into()),
    }))]);
    assert_eq!(state.snapshot().telemetry_status, TelemetryStatus::Degraded);
    state.reduce_batch(&[event(tool("a", ToolPhase::Finished))]);
    assert_eq!(state.snapshot().telemetry_status, TelemetryStatus::Degraded);
    state.reduce_batch(&[event(tool("b", ToolPhase::Finished))]);
    let snapshot = state.snapshot();
    assert_eq!(snapshot.telemetry_status, TelemetryStatus::Healthy);
    assert_eq!(snapshot.coverage.tools, Some(false));
    assert_eq!(snapshot.coverage.text, Some(true));
    assert_eq!(snapshot.coverage.public_reasoning, None);
    assert_eq!(snapshot.diagnostics, diagnostics);
}

#[test]
fn settlement_converges_only_its_correlation_gaps_without_repairing_history() {
    let state = RuntimeState::new();
    start(&state);
    for (launch, turn, id) in [("launch", 1, "a"), ("launch", 2, "b"), ("other", 1, "c")] {
        state.reduce_batch(&[scoped(
            gap(
                ActivityScope::ModelRequest(RequestId(id.into())),
                CoverageDimension::ModelActivity,
            ),
            launch,
            turn,
        )]);
    }
    state.reduce_batch(&[event(settled(1))]);
    // Correlation-gap trust and historical coverage retain their own scopes;
    // they do not imply several simultaneously open activity-ledger turns.
    assert_eq!(state.snapshot().telemetry_status, TelemetryStatus::Degraded);
    state.reduce_batch(&[scoped(settled(2), "launch", 2)]);
    assert_eq!(state.snapshot().telemetry_status, TelemetryStatus::Degraded);
    state.reduce_batch(&[scoped(settled(1), "other", 1)]);
    let snapshot = state.snapshot();
    assert!(!snapshot.model_request_active);
    assert_eq!(snapshot.telemetry_status, TelemetryStatus::Healthy);
    assert_eq!(snapshot.coverage.model_activity, Some(false));
    assert_eq!(snapshot.diagnostics.iter().map(|d| d.count).sum::<u64>(), 3);
}

#[test]
fn model_failure_retry_stall_and_unassociated_failure_preserve_parallel_requests() {
    let state = RuntimeState::new();
    start(&state);
    state.reduce_batch(&[
        event(model(Some("a"), ModelPhase::Started)),
        event(model(Some("b"), ModelPhase::Started)),
    ]);
    state.reduce_batch(&[event(model(None, ModelPhase::Failed))]);
    assert_eq!(state.state.lock().unwrap().active_model_requests.len(), 2);
    state.reduce_batch(&[event(model(Some("a"), ModelPhase::Failed))]);
    assert!(state.snapshot().model_request_active);
    state.reduce_batch(&[event(EventFact::ModelActivityChanged(ModelActivity {
        request_id: Some(RequestId("a".into())),
        phase: ModelPhase::RetryScheduled,
        retry: Some(NativeRetry {
            attempt: Some(1),
            next_attempt: Some(2),
            delay_ms: Some(200),
            ..NativeRetry::default()
        }),
        failure: None,
    }))]);
    assert!(
        state.snapshot().model_request_active,
        "waiting on one request cannot hide another running request"
    );
    assert_eq!(
        state
            .snapshot()
            .model_activity
            .unwrap()
            .retry
            .unwrap()
            .delay_ms,
        Some(200)
    );
    state.reduce_batch(&[event(model(Some("b"), ModelPhase::Completed))]);
    assert!(!state.snapshot().model_request_active);
    state.reduce_batch(&[event(model(Some("a"), ModelPhase::Started))]);
    assert!(state.snapshot().model_request_active);
    state.reduce_batch(&[event(model(Some("a"), ModelPhase::Completed))]);
    state.reduce_batch(&[event(model(Some("stalled"), ModelPhase::StreamStalled))]);
    assert!(state.snapshot().model_request_active);
    assert_eq!(
        state.snapshot().model_request_age_ms,
        None,
        "a stall cannot fabricate a start time"
    );
    assert_eq!(state.snapshot().telemetry_status, TelemetryStatus::Healthy);
    assert!(
        state.snapshot().diagnostics.is_empty(),
        "legal native errors/retries/stalls are not observation loss"
    );
}

#[test]
fn local_to_native_identity_promotion_preserves_request_association() {
    let state = RuntimeState::new();
    start(&state);
    state.reduce_batch(&[event(model(Some("request"), ModelPhase::Started))]);
    state.reduce_batch(&[event(gap(
        ActivityScope::ModelRequest(RequestId("request".into())),
        CoverageDimension::ModelActivity,
    ))]);
    let mut completed = event(model(Some("request"), ModelPhase::Completed));
    completed.context.turn_id = Some(TurnId::Native("native-turn".into()));
    state.reduce_batch(&[completed]);
    assert!(!state.snapshot().model_request_active);
    assert_eq!(state.snapshot().telemetry_status, TelemetryStatus::Healthy);
    assert_eq!(state.snapshot().coverage.model_activity, Some(false));
}

#[test]
fn unknown_impact_is_diagnosable_without_claiming_known_dimensional_loss() {
    let state = RuntimeState::new();
    start(&state);
    let diagnostic = Diagnostic {
        reason: DiagnosticReason::UnknownSemantics,
        impact: Impact::Unknown,
        discriminator: SafeToken::new("secret.PRIVATE"),
    };
    state.reduce_batch(&[event(EventFact::Diagnostic(diagnostic))]);
    let snapshot = state.snapshot();
    assert_eq!(snapshot.telemetry_status, TelemetryStatus::Healthy);
    assert_eq!(snapshot.coverage.tools, None);
    assert_eq!(snapshot.coverage.public_reasoning, None);
    assert_eq!(state.observation_snapshot().coverage.dropped_events, 0);
    assert_eq!(snapshot.diagnostics.len(), 1);
    let encoded = serde_json::to_string(&snapshot.diagnostics).unwrap();
    assert!(encoded.contains("unknown_semantics"));
    assert!(!encoded.contains("PRIVATE"));
    assert!(!encoded.contains("api_key"));
}

#[test]
fn diagnostic_counts_are_per_native_input_and_queries_never_mutate_them() {
    let state = RuntimeState::new();
    start(&state);
    let mut first = event(gap(
        ActivityScope::Turn(TurnId::Local(1)),
        CoverageDimension::Tools,
    ));
    first.context.source.event_id = Some("native-input".into());
    let mut second = first.clone();
    second.context.sequence = 2;
    second.fact = EventFact::Diagnostic(Diagnostic {
        reason: DiagnosticReason::Malformed,
        impact: Impact::Known(vec![CoverageDimension::Tools]),
        discriminator: None,
    });
    state.reduce_batch(&[first.clone(), second.clone()]);
    let before = state.snapshot();
    assert_eq!(before.diagnostics[0].count, 1);
    assert_eq!(state.observation_snapshot().coverage.dropped_events, 1);
    state.reduce_batch(&[first, second]);
    for _ in 0..4 {
        assert_eq!(state.snapshot().diagnostics, before.diagnostics);
        state.observation_snapshot();
        state.peek_wait_tail();
    }
    assert_eq!(state.observation_snapshot().coverage.dropped_events, 1);
    assert_eq!(state.snapshot().revision, before.revision);
}

#[test]
fn diagnostics_and_dimensional_history_survive_turn_and_claim_convergence() {
    let old = RuntimeState::new();
    start(&old);
    old.reduce_batch(&[event(gap(
        ActivityScope::Turn(TurnId::Local(1)),
        CoverageDimension::Text,
    ))]);
    let history = old.snapshot().diagnostics;
    old.reduce_batch(&[event(settled(1))]);
    assert_eq!(old.snapshot().telemetry_status, TelemetryStatus::Healthy);
    assert_eq!(old.snapshot().diagnostics, history);
    let next = RuntimeState::new();
    next.inherit_wait_text(&old);
    assert_eq!(
        next.snapshot().telemetry_status,
        TelemetryStatus::Unavailable
    );
    assert_eq!(next.snapshot().diagnostics, history);
    assert_eq!(next.snapshot().coverage.text, Some(false));
    start(&next);
    assert_eq!(next.snapshot().telemetry_status, TelemetryStatus::Healthy);
    assert_eq!(next.snapshot().coverage.text, Some(false));
    assert_eq!(next.snapshot().diagnostics, history);
}

#[test]
fn diagnostic_summary_is_bounded_aggregated_and_uses_only_controlled_words() {
    let state = RuntimeState::new();
    let dimensions = [
        CoverageDimension::Text,
        CoverageDimension::Tools,
        CoverageDimension::PublicReasoning,
        CoverageDimension::ModelActivity,
        CoverageDimension::Input,
        CoverageDimension::Turn,
    ];
    for mask in 0..128 {
        let mut input = event(EventFact::CoverageGap(CoverageGap {
            reason: LossReason::Malformed,
            impact: Impact::Known(
                dimensions
                    .iter()
                    .enumerate()
                    .filter(|(bit, _)| mask & (1 << bit) != 0)
                    .map(|(_, d)| *d)
                    .collect(),
            ),
            activity: ActivityScope::None,
        }));
        input.context.source.protocol = if mask < 64 {
            SourceProtocol::ZcodeSession
        } else {
            SourceProtocol::AgyStreamJson
        };
        state.reduce_batch(&[input]);
    }
    let snapshot = state.snapshot();
    assert!(snapshot.diagnostics_truncated);
    assert!(snapshot.diagnostics.len() <= crate::runtime_observation::MAX_DIAGNOSTICS);
    assert_eq!(
        snapshot.diagnostics.iter().map(|d| d.count).sum::<u64>(),
        128
    );
    let encoded = serde_json::to_string(&snapshot.diagnostics).unwrap();
    assert!(encoded.len() < 16 * 1024);
    assert!(!encoded.contains("PRIVATE"));
    assert!(!encoded.contains("api_key"));
    assert_eq!(
        snapshot.telemetry_status,
        TelemetryStatus::Unavailable,
        "history alone is not current activity evidence"
    );
}

#[test]
fn completion_without_start_counts_a_call_without_inventing_a_start_or_success() {
    let state = RuntimeState::new();
    start(&state);
    let mut finished = tool("call", ToolPhase::Finished);
    if let EventFact::ToolChanged(tool) = &mut finished {
        tool.name = None;
    }
    state.reduce_batch(&[event(finished)]);
    let snapshot = state.snapshot();
    assert_eq!(snapshot.window_60s.tool_calls_started, 1);
    assert_eq!(snapshot.window_60s.tool_calls_completed, 1);
    assert!(snapshot.active_tools.is_empty());
    assert_eq!(snapshot.oldest_active_tool_age_ms, None);
    let observed = state.observation_snapshot();
    assert_eq!(observed.tools[0].call_count, 1);
    assert_eq!(
        observed.tools[0].recent_calls[0].status,
        observation::ObservedCallStatus::Finished
    );
    let mut update = tool("call", ToolPhase::Progress);
    if let EventFact::ToolChanged(tool) = &mut update {
        tool.arguments = Some(ArgumentUpdate::Snapshot(
            serde_json::json!({"command":"true"}),
        ));
    }
    state.reduce_batch(&[event(update)]);
    let observed = state.observation_snapshot();
    assert_eq!(observed.tools.len(), 1);
    assert_eq!(observed.tools[0].tool_name, "RunCommand");
    assert_eq!(observed.tools[0].call_count, 1);
    assert_eq!(
        observed.tools[0].recent_calls[0].status,
        observation::ObservedCallStatus::Finished
    );
}

#[test]
fn argument_fragment_overflow_remains_unknown_until_an_authoritative_snapshot() {
    let state = RuntimeState::new();
    start(&state);
    let mut change = if let EventFact::ToolChanged(change) = tool("args", ToolPhase::Started) {
        change
    } else {
        unreachable!()
    };
    change.arguments = Some(ArgumentUpdate::Delta("x".repeat(4097)));
    state.reduce_batch(&[event(EventFact::ToolChanged(change.clone()))]);
    change.phase = Some(ToolPhase::Progress);
    change.arguments = Some(ArgumentUpdate::Delta("{\"suffix\":true}".into()));
    state.reduce_batch(&[event(EventFact::ToolChanged(change.clone()))]);
    assert!(state.observation_snapshot().tools[0].recent_calls[0].arguments_truncated);
    change.arguments = Some(ArgumentUpdate::Snapshot(
        serde_json::json!({"complete":true}),
    ));
    state.reduce_batch(&[event(EventFact::ToolChanged(change))]);
    let snapshot = state.observation_snapshot();
    assert_eq!(snapshot.tools[0].call_count, 1);
    assert_eq!(
        snapshot.tools[0].recent_calls[0].arguments["complete"],
        true
    );
    assert!(!snapshot.tools[0].recent_calls[0].arguments_truncated);
}

#[test]
fn local_to_native_turn_promotion_does_not_recount_a_tool_or_erase_arguments() {
    let state = RuntimeState::new();
    start(&state);
    let mut started = tool("call", ToolPhase::Started);
    if let EventFact::ToolChanged(change) = &mut started {
        change.arguments = Some(ArgumentUpdate::Snapshot(
            serde_json::json!({"command":"true"}),
        ));
    }
    state.reduce_batch(&[event(started)]);
    let mut finished = event(tool("call", ToolPhase::Finished));
    finished.context.turn_id = Some(TurnId::Native("native".into()));
    state.reduce_batch(&[finished]);
    assert_eq!(state.snapshot().window_60s.tool_calls_started, 1);
    assert!(state.snapshot().active_tools.is_empty());
    let observation = state.observation_snapshot();
    assert_eq!(observation.tools[0].call_count, 1);
    assert_eq!(
        observation.tools[0].recent_calls[0].arguments["command"],
        "true"
    );
}

#[test]
fn correlation_capacity_loss_cannot_be_cleared_by_another_turn_settlement() {
    let state = RuntimeState::new();
    start(&state);
    for index in 0..257 {
        state.reduce_batch(&[event(gap(
            ActivityScope::Tool(CallId(index.to_string())),
            CoverageDimension::Tools,
        ))]);
    }
    state.reduce_batch(&[scoped(
        gap(
            ActivityScope::Tool(CallId("other".into())),
            CoverageDimension::Tools,
        ),
        "launch",
        2,
    )]);
    for index in 0..256 {
        state.reduce_batch(&[event(tool(&index.to_string(), ToolPhase::Finished))]);
    }
    state.reduce_batch(&[scoped(settled(2), "launch", 2)]);
    assert_eq!(
        state.snapshot().telemetry_status,
        TelemetryStatus::Degraded,
        "bounded correlation overflow cannot discard another turn's unresolved gap"
    );
    state.reduce_terminal();
    assert_eq!(
        state.snapshot().telemetry_status,
        TelemetryStatus::Unavailable
    );
    assert_eq!(state.snapshot().coverage.tools, Some(false));
}

#[test]
fn argument_cache_capacity_loss_cannot_turn_a_later_suffix_into_complete_arguments() {
    let state = RuntimeState::new();
    start(&state);
    let change = |id: &str, arguments: Option<ArgumentUpdate>, phase| {
        let EventFact::ToolChanged(mut change) = tool(id, phase) else {
            unreachable!()
        };
        change.arguments = arguments;
        event(EventFact::ToolChanged(change))
    };
    for index in 0..320 {
        state.reduce_batch(&[change(
            &index.to_string(),
            Some(ArgumentUpdate::Delta("{".into())),
            ToolPhase::Started,
        )]);
    }
    state.reduce_batch(&[change(
        "lost-prefix",
        Some(ArgumentUpdate::Delta("{".into())),
        ToolPhase::Started,
    )]);
    state.reduce_batch(&[change("0", None, ToolPhase::Finished)]);
    state.reduce_batch(&[change(
        "lost-prefix",
        Some(ArgumentUpdate::Delta("{\"suffix\":true}".into())),
        ToolPhase::Progress,
    )]);
    let snapshot = state.observation_snapshot();
    let call = snapshot
        .tools
        .iter()
        .flat_map(|tool| &tool.recent_calls)
        .find(|call| call.tool_call_id == "lost-prefix")
        .unwrap();
    assert!(
        call.arguments_truncated,
        "discarded prefixes require an authoritative snapshot to become complete"
    );
}

#[test]
fn model_retention_limit_is_current_uncertainty_until_its_own_terminal() {
    let state = RuntimeState::new();
    start(&state);
    // Synthetic occupancy isolates the documented retention boundary without
    // depending on tens of thousands of unrelated producer notifications.
    for index in 0..MAX_ACTIVITY_IDENTITIES {
        state
            .state
            .lock()
            .unwrap()
            .active_model_requests
            .insert(index.to_string(), None);
    }
    state.reduce_batch(&[event(model(Some("over-capacity"), ModelPhase::Started))]);
    state.state.lock().unwrap().active_model_requests.clear();
    assert_eq!(state.snapshot().telemetry_status, TelemetryStatus::Degraded);
    assert_eq!(state.snapshot().coverage.model_activity, Some(false));
    state.reduce_batch(&[event(model(Some("unrelated"), ModelPhase::Completed))]);
    assert_eq!(state.snapshot().telemetry_status, TelemetryStatus::Degraded);
    state.reduce_batch(&[event(model(Some("over-capacity"), ModelPhase::Completed))]);
    assert_eq!(state.snapshot().telemetry_status, TelemetryStatus::Healthy);
    assert_eq!(state.snapshot().coverage.model_activity, Some(false));
}

// Reviewer R03-F01 interleavings, ported as permanent reducer regressions.
mod settled_scope_residue {
    use crate::*;
    use external_contract::events::*;

    fn ev(turn: Option<TurnId>, fact: EventFact) -> NormalizedEvent {
        NormalizedEvent {
            context: EventContext {
                task_id: TaskId("task".into()),
                launch_id: LaunchId("launch".into()),
                sequence: 0,
                turn_id: turn,
                source: NativeSource {
                    protocol: SourceProtocol::ZcodeTelemetryV4,
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

    fn turn_started(id: &str) -> EventFact {
        EventFact::TurnStarted {
            turn_id: TurnId::Native(id.into()),
        }
    }
    fn settled(id: &str) -> EventFact {
        EventFact::TurnSettled(TurnSettlement {
            turn_id: TurnId::Native(id.into()),
            outcome: TurnOutcome::Completed,
            stop_reason: None,
            final_text: Some("done".into()),
            failure: None,
        })
    }
    fn model(id: &str, phase: ModelPhase) -> EventFact {
        EventFact::ModelActivityChanged(ModelActivity {
            request_id: Some(RequestId(id.into())),
            phase,
            retry: None,
            failure: None,
        })
    }
    fn tool(id: &str, phase: ToolPhase) -> EventFact {
        EventFact::ToolChanged(ToolChange {
            call_id: CallId(id.into()),
            name: Some("Read".into()),
            category: None,
            arguments: None,
            phase: Some(phase),
            outcome: None,
            failure: None,
            denied: None,
        })
    }

    /// Telemetry `started` attributed to the just-settled turn (adapter keeps
    /// `self.turn` after settlement; telemetry has no turnId of its own), then a
    /// later turn starts, the request completes under the NEW turn scope, and the
    /// new turn settles. Does the stale correlation survive every boundary?
    #[test]
    fn settlement_clears_model_residue_from_previously_settled_turn() {
        let rt = RuntimeState::new();
        let t1 = Some(TurnId::Native("t1".into()));
        let t2 = Some(TurnId::Native("t2".into()));
        rt.reduce_batch(&[ev(t1.clone(), turn_started("t1"))]);
        rt.reduce_batch(&[ev(t1.clone(), settled("t1"))]);
        // started lands after t1's settlement, before t2 starts: attributed to t1.
        rt.reduce_batch(&[ev(t1.clone(), model("r", ModelPhase::Started))]);
        rt.reduce_batch(&[ev(t2.clone(), turn_started("t2"))]);
        // the request finishes while the adapter current turn is t2.
        rt.reduce_batch(&[ev(t2.clone(), model("r", ModelPhase::Completed))]);
        rt.reduce_batch(&[ev(t2.clone(), settled("t2"))]);
        let snap = rt.snapshot();
        assert!(rt.state.lock().unwrap().active_model_requests.is_empty());
        assert!(
            !snap.model_request_active,
            "stale model request survived t2 start+settle"
        );
        assert_eq!(snap.model_request_age_ms, None);
    }

    /// Same window for a tool started frame attributed to the settled turn.
    #[test]
    fn settlement_clears_tool_residue_from_previously_settled_turn() {
        let rt = RuntimeState::new();
        let t1 = Some(TurnId::Native("t1".into()));
        let t2 = Some(TurnId::Native("t2".into()));
        rt.reduce_batch(&[ev(t1.clone(), turn_started("t1"))]);
        rt.reduce_batch(&[ev(t1.clone(), settled("t1"))]);
        rt.reduce_batch(&[ev(t1.clone(), tool("c", ToolPhase::Started))]);
        rt.reduce_batch(&[ev(t2.clone(), turn_started("t2"))]);
        rt.reduce_batch(&[ev(t2.clone(), tool("c", ToolPhase::Finished))]);
        rt.reduce_batch(&[ev(t2.clone(), settled("t2"))]);
        let snap = rt.snapshot();
        assert!(rt.state.lock().unwrap().active_tools.is_empty());
        assert!(
            snap.active_tools.is_empty(),
            "stale in-flight tool survived t2 start+settle"
        );
        assert_eq!(snap.oldest_active_tool_age_ms, None);
    }

    /// Control: the same request started INSIDE t1 is cleared by t1's settlement.
    #[test]
    fn settlement_clears_model_started_in_live_scope() {
        let rt = RuntimeState::new();
        let t1 = Some(TurnId::Native("t1".into()));
        rt.reduce_batch(&[ev(t1.clone(), turn_started("t1"))]);
        rt.reduce_batch(&[ev(t1.clone(), model("r", ModelPhase::Started))]);
        rt.reduce_batch(&[ev(t1.clone(), settled("t1"))]);
        assert!(!rt.snapshot().model_request_active);
    }

    #[test]
    fn settlement_clears_correlations_started_before_first_turn() {
        let rt = RuntimeState::new();
        rt.reduce_batch(&[
            ev(None, model("r", ModelPhase::Started)),
            ev(None, tool("c", ToolPhase::Started)),
        ]);
        assert!(rt.snapshot().model_request_active);
        assert_eq!(rt.snapshot().active_tools.len(), 1);
        let turn = Some(TurnId::Native("first".into()));
        rt.reduce_batch(&[ev(turn.clone(), turn_started("first"))]);
        rt.reduce_batch(&[ev(turn.clone(), settled("first"))]);
        let snap = rt.snapshot();
        let state = rt.state.lock().unwrap();
        assert!(state.active_model_requests.is_empty());
        assert!(state.active_tools.is_empty());
        assert!(!snap.model_request_active);
        assert_eq!(snap.model_request_age_ms, None);
        assert!(snap.active_tools.is_empty());
        assert_eq!(snap.oldest_active_tool_age_ms, None);
    }
}
