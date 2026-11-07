//! Temporary typed migration entry. S08 replaces these legacy view reducers.
//! No native JSON is reconstructed here. Unmigrated owners keep emit_driver.
use crate::*;
use external_contract::events::*;

impl Publisher {
    pub(crate) fn event_scope(&self) -> (TaskId, LaunchId) {
        self.sink.event_scope().unwrap_or_else(|| {
            // Publisher address is stable for its lifetime; process identity
            // disambiguates independent debug processes. No session/turn alias.
            (
                TaskId(format!("runtime-{:p}", self)),
                LaunchId(format!("{}-{:p}", std::process::id(), self)),
            )
        })
    }

    pub(crate) fn emit_typed(&self, batch: DecodedInput, tracker: &TurnTracker) {
        let mut state = self.state.lock().unwrap();
        if matches!(state.owner, OwnerState::Terminal(_)) {
            return;
        }
        let mut applied = Vec::new();
        for mut event in batch.into_events() {
            event.context.sequence = state.next_sequence;
            state.next_sequence = state.next_sequence.saturating_add(1);
            applied.push(event);
        }
        self.sink.emit_normalized_batch(&applied);
        // The complete batch (including text and failure) reaches the sink
        // before scheduler command waiters can observe its settlement.
        tracker.observe_typed(&applied);
    }
}

impl TurnTracker {
    fn observe_typed(&self, events: &[NormalizedEvent]) {
        let mut state = self.state.lock().unwrap();
        for event in events {
            if state.active {
                state.last_stream_activity_at = Some(Instant::now());
            }
            match &event.fact {
                EventFact::TurnStarted { .. } => {
                    state.generation = state.generation.saturating_add(1);
                    state.active = true;
                    state.boundary = None;
                    state.failure = None;
                    state.model_request_started_at = Some(Instant::now());
                }
                EventFact::ErrorObserved {
                    operation: Operation::Turn,
                    failure,
                } => {
                    state.failure = Some(scheduler::native_failure_detail(&failure.original));
                }
                EventFact::TurnSettled(settlement) if state.active => {
                    state.active = false;
                    // A native cancellation is not successful completion.
                    // A committed ES stop still wins in scheduler arbitration.
                    state.boundary = Some(
                        if matches!(
                            settlement.outcome,
                            TurnOutcome::Failed | TurnOutcome::Cancelled
                        ) {
                            TurnBoundary::Failed
                        } else {
                            TurnBoundary::Completed
                        },
                    );
                    if settlement.outcome == TurnOutcome::Completed {
                        // A successful authoritative boundary retires errors
                        // recovered within this turn, before any later exit.
                        state.failure = None;
                    } else if let Some(failure) = &settlement.failure {
                        state.failure = Some(scheduler::native_failure_detail(&failure.original));
                    }
                    state.model_request_started_at = None;
                    state.last_stream_activity_at = None;
                }
                _ => {}
            }
        }
        self.changed.notify_all();
    }
}
