//! Shared reduction and publication path for native typed batches.
use crate::*;
use external_contract::events::*;

impl Publisher {
    pub(crate) fn event_scope(&self) -> (TaskId, LaunchId) {
        self.sink.event_scope().unwrap_or_else(|| {
            (
                TaskId(format!("runtime-{:p}", self)),
                LaunchId(format!("{}-{:p}", std::process::id(), self)),
            )
        })
    }

    pub(crate) fn emit_typed(&self, batch: DecodedInput) {
        // Lock order: publisher -> sink admission -> RuntimeState. Reduction
        // never calls owners/sinks. Persistence happens after releasing state.
        let mut state = self.state.lock().unwrap();
        if matches!(state.owner, OwnerState::Terminal(_)) {
            return;
        }
        let mut applied = batch.into_events();
        for event in &mut applied {
            event.context.sequence = state.next_sequence;
            state.next_sequence = state.next_sequence.saturating_add(1);
        }
        // Production sinks reduce under their admission latch. Standalone sinks
        // have no admission gate and use the publisher's state directly.
        if self.sink.runtime_state().is_none() {
            self.runtime_state.reduce_batch(&applied);
        }
        self.sink.emit_normalized_batch(&applied);
        self.runtime_state.notify_reduced();
    }
}

impl RuntimeState {
    pub(crate) fn reduce_batch(&self, events: &[NormalizedEvent]) {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        let wall = activity_tracker::activity_wall_now_millis();
        for event in events {
            Self::reduce_typed(&mut state, event, now, wall);
        }
    }

    pub(crate) fn notify_reduced(&self) {
        self.changed.notify_all();
    }

    pub(crate) fn active_turn_id(&self) -> Option<TurnId> {
        let state = self.state.lock().unwrap();
        if state.turn.active {
            state.turn.turn_id.clone()
        } else {
            None
        }
    }

    pub(crate) fn native_failure(&self) -> Option<NativeFailure> {
        let state = self.state.lock().unwrap();
        state
            .command_failure
            .clone()
            .or_else(|| state.turn.failure.clone())
    }

    pub(crate) fn record_command_failure(&self, failure: NativeFailure) {
        self.state.lock().unwrap().command_failure = Some(failure);
    }

    pub(crate) fn clear_command_failure(&self) {
        self.state.lock().unwrap().command_failure = None;
    }

    pub(crate) fn reduce_turn(state: &mut TurnReduction, event: &NormalizedEvent, now: Instant) {
        if state.active {
            state.last_stream_activity_at = Some(now);
            if let Some(turn_id) = &event.context.turn_id {
                state.turn_id = Some(turn_id.clone());
            }
        }
        // Adapters own current/late attribution, including upgrading a local
        // control scope when a native ID first arrives. Do not reinterpret it.
        match &event.fact {
            EventFact::TurnStarted { turn_id } => {
                state.turn_id = Some(turn_id.clone());
                state.generation = state.generation.saturating_add(1);
                state.active = true;
                state.boundary = None;
                state.failure = None;
                state.model_request_started_at = Some(now);
                state.last_stream_activity_at = Some(now);
            }
            EventFact::ErrorObserved {
                operation: Operation::Turn,
                failure,
            } if state.active => {
                state.failure = Some(failure.clone());
            }
            EventFact::TurnSettled(settlement) if state.active => {
                state.active = false;
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
                    state.failure = None;
                } else if let Some(failure) = &settlement.failure {
                    state.failure = Some(failure.clone());
                }
                state.model_request_started_at = None;
                state.last_stream_activity_at = None;
            }
            _ => {}
        }
    }
    pub(crate) fn turn_snapshot(&self) -> TurnSnapshot {
        let state = self.state.lock().unwrap();
        TurnSnapshot {
            generation: state.turn.generation,
            active: state.turn.active,
            boundary: state.turn.boundary,
        }
    }

    pub(crate) fn activity_snapshot(&self) -> RuntimeActivitySnapshot {
        let state = self.state.lock().unwrap();
        let now = Instant::now();
        RuntimeActivitySnapshot {
            turn: TurnSnapshot {
                generation: state.turn.generation,
                active: state.turn.active,
                boundary: state.turn.boundary,
            },
            model_request_elapsed: state
                .turn
                .model_request_started_at
                .and_then(|started| now.checked_duration_since(started)),
            transport_idle_elapsed: state
                .turn
                .last_stream_activity_at
                .and_then(|activity| now.checked_duration_since(activity)),
        }
    }

    pub(crate) fn wait_started_after(
        &self,
        previous_generation: u64,
        timeout: Duration,
    ) -> Result<TurnSnapshot, RuntimeCommandError> {
        self.wait_until(timeout, |state| state.generation > previous_generation)
    }

    pub(crate) fn wait_boundary_after(
        &self,
        generation: u64,
        timeout: Duration,
    ) -> Result<TurnSnapshot, RuntimeCommandError> {
        self.wait_until(timeout, |state| {
            state.generation >= generation && !state.active && state.boundary.is_some()
        })
    }

    fn wait_until(
        &self,
        timeout: Duration,
        predicate: impl Fn(&TurnReduction) -> bool,
    ) -> Result<TurnSnapshot, RuntimeCommandError> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().unwrap();
        loop {
            if predicate(&state.turn) {
                return Ok(TurnSnapshot {
                    generation: state.turn.generation,
                    active: state.turn.active,
                    boundary: state.turn.boundary,
                });
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(RuntimeCommandError::Timeout);
            }
            let (next, result) = self.changed.wait_timeout(state, deadline - now).unwrap();
            state = next;
            if result.timed_out() && !predicate(&state.turn) {
                return Err(RuntimeCommandError::Timeout);
            }
        }
    }
}

#[cfg(test)]
impl RuntimeState {
    pub(crate) fn test_turn(&self, boundary: Option<TurnBoundary>) {
        let fact = match boundary {
            None => EventFact::TurnStarted {
                turn_id: TurnId::Local(1),
            },
            Some(boundary) => EventFact::TurnSettled(TurnSettlement {
                turn_id: TurnId::Local(1),
                outcome: if boundary == TurnBoundary::Completed {
                    TurnOutcome::Completed
                } else {
                    TurnOutcome::Failed
                },
                stop_reason: None,
                final_text: None,
                failure: None,
            }),
        };
        self.reduce_batch(&[NormalizedEvent {
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
        }]);
        self.notify_reduced();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn event(fact: EventFact) -> NormalizedEvent {
        NormalizedEvent {
            context: EventContext {
                task_id: TaskId("task".into()),
                launch_id: LaunchId("launch".into()),
                sequence: 0,
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
    fn batch(facts: Vec<EventFact>) -> DecodedInput {
        let events: Vec<_> = facts
            .into_iter()
            .enumerate()
            .map(|(index, fact)| {
                let mut event = event(fact);
                event.context.sequence = index as u64 + 1;
                event
            })
            .collect();
        let context = &events[0].context;
        DecodedInput::new(
            NativeInputContext {
                task_id: context.task_id.clone(),
                launch_id: context.launch_id.clone(),
                turn_id: context.turn_id.clone(),
                source: context.source.clone(),
            },
            Attribution::Current,
            DecodeDisposition::Consumed,
            events,
        )
        .unwrap()
    }
    fn tool(phase: ToolPhase) -> EventFact {
        EventFact::ToolChanged(ToolChange {
            call_id: CallId("call".into()),
            name: Some("Read".into()),
            category: None,
            arguments: None,
            phase: Some(phase),
            outcome: Some(ToolOutcome::Unknown),
            failure: None,
            denied: None,
        })
    }
    fn settlement(outcome: TurnOutcome, failure: Option<NativeFailure>) -> EventFact {
        EventFact::TurnSettled(TurnSettlement {
            turn_id: TurnId::Local(1),
            outcome,
            stop_reason: None,
            final_text: Some("settled answer".into()),
            failure,
        })
    }

    #[test]
    fn settlement_reduces_all_views_before_callbacks_and_wakes_waiters() {
        struct Sink {
            runtime: Arc<RuntimeState>,
            persisted: mpsc::Sender<()>,
            records: Mutex<Vec<u64>>,
            waiter: Mutex<mpsc::Receiver<()>>,
        }
        impl LifecycleSink for Sink {
            fn runtime_state(&self) -> Option<Arc<RuntimeState>> {
                Some(self.runtime.clone())
            }
            fn emit(&self, record: LifecycleRecord) {
                // Snapshotting from a persistence callback cannot deadlock the
                // reducer and cannot observe a partially applied settlement.
                assert!(!self.runtime.turn_snapshot().active);
                assert_eq!(self.runtime.peek_wait_tail().text, "stream tail");
                assert_eq!(self.runtime.snapshot().window_60s.tool_calls_completed, 1);
                assert!(self.runtime.snapshot().active_tools.is_empty());
                assert_eq!(
                    self.runtime.native_failure().unwrap().original["data"]["complete"],
                    true
                );
                self.records.lock().unwrap().push(record.sequence);
                self.persisted.send(()).unwrap();
            }
        }
        let runtime = Arc::new(RuntimeState::new());
        runtime.reduce_batch(&[
            event(EventFact::TurnStarted {
                turn_id: TurnId::Local(1),
            }),
            event(tool(ToolPhase::Started)),
        ]);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (waiter_tx, waiter_rx) = mpsc::channel();
        let waiter_state = runtime.clone();
        let waiter = thread::spawn(move || {
            let mut state = waiter_state.state.lock().unwrap();
            ready_tx.send(()).unwrap();
            while state.turn.active {
                state = waiter_state.changed.wait(state).unwrap();
            }
            waiter_tx.send(()).unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (persisted_tx, persisted_rx) = mpsc::channel();
        let sink = Arc::new(Sink {
            runtime: runtime.clone(),
            persisted: persisted_tx,
            records: Mutex::new(vec![]),
            waiter: Mutex::new(waiter_rx),
        });
        let publisher = Publisher::new(sink.clone());
        assert!(Arc::ptr_eq(&publisher.runtime_state, &runtime));
        publisher.emit_typed(batch(vec![EventFact::MessageUpdated(MessageUpdate {
            message_id: MessageId("message".into()), text: TextUpdate::Delta("stream tail".into()),
        }), tool(ToolPhase::Finished), settlement(TurnOutcome::Failed, Some(NativeFailure {
            original: serde_json::json!({"message":"native error", "data":{"complete":true}}), projection: None,
        }))]));
        for _ in 0..3 {
            persisted_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        sink.waiter
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        waiter.join().unwrap();
        assert_eq!(*sink.records.lock().unwrap(), [1, 2, 3]);
        assert_eq!(
            runtime.take_terminal_text(),
            TerminalText::Visible("settled answer".into())
        );
    }

    #[test]
    fn successful_turn_retires_turn_error_but_keeps_a_rejected_command_cause() {
        let runtime = RuntimeState::new();
        runtime.test_turn(None);
        let failure = NativeFailure {
            original: serde_json::json!({"message":"native cause", "data":[1,2,3]}),
            projection: None,
        };
        runtime.reduce_batch(&[event(EventFact::ErrorObserved {
            operation: Operation::Turn,
            failure: failure.clone(),
        })]);
        assert_eq!(runtime.native_failure(), Some(failure.clone()));
        runtime.reduce_batch(&[event(settlement(TurnOutcome::Completed, None))]);
        assert!(runtime.native_failure().is_none());
        runtime.test_turn(None);
        runtime.record_command_failure(failure.clone());
        runtime.reduce_batch(&[event(settlement(TurnOutcome::Completed, None))]);
        assert_eq!(runtime.native_failure(), Some(failure));
        runtime.clear_command_failure();
        assert!(runtime.native_failure().is_none());
    }

    #[test]
    fn publisher_drops_transport_lifecycle_mirrors_without_consuming_sequence() {
        struct Sink(Mutex<Vec<LifecycleRecord>>);
        impl LifecycleSink for Sink {
            fn emit(&self, record: LifecycleRecord) {
                self.0.lock().unwrap().push(record);
            }
        }
        let sink = Arc::new(Sink(Mutex::new(vec![])));
        let publisher = Publisher::new(sink.clone());
        publisher.emit_driver(
            Inbound::Lifecycle {
                sequence: 77,
                method: "turn.started".into(),
                order: external_contract::LifecycleOrder::InOrder,
            },
            None,
        );
        publisher.emit_typed(batch(vec![EventFact::TurnStarted {
            turn_id: TurnId::Local(1),
        }]));
        let records = sink.0.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sequence, 1);
        assert!(matches!(records[0].event, RuntimeEvent::Normalized(_)));
    }
}
