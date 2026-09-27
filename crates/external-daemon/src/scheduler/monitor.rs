use super::types::STALLED_NO_ACTIVITY_REASON;
use super::*;
use crate::lifecycle_sink::TransportFrameLimit;
use crate::TRANSPORT_FRAME_LIMIT_REASON;
use external_store::StoreError;
impl Scheduler {
    #[allow(clippy::too_many_arguments)]
    fn finish_locked_monitor_terminal(
        &self,
        agent_id: &str,
        owner_epoch: u64,
        runtime: &Arc<dyn ManagedRuntime>,
        sink: &StoreLifecycleSink,
        route: &TaskRoute,
        _task: Option<&TaskRecord>,
        terminal: RuntimeTerminal,
        natural_completion: bool,
        forced_outcome: Option<(CompletionOutcome, String)>,
        failure_message: Option<String>,
    ) -> Result<TaskPhase, SchedulerError> {
        let current = self.inner.store.get_task(agent_id)?.ok_or_else(|| {
            SchedulerError::Store(StoreError::InvalidState(
                "active monitor task disappeared".into(),
            ))
        })?;
        if current.phase.is_terminal() || current.owner_epoch != owner_epoch {
            return Ok(current.phase);
        }
        let cancellation_wins = current.stop_requested || current.close_requested;
        let (terminal, natural_completion, forced_outcome) = if cancellation_wins {
            sink.runtime_lifecycle
                .request_stop(&runtime.turn_snapshot());
            sink.runtime_lifecycle.force_terminating();
            (
                runtime.stop(self.inner.config.stop_grace),
                false,
                Some((CompletionOutcome::Cancelled, "CANCELLED".into())),
            )
        } else if forced_outcome.is_none() && sink.error().is_some() {
            (
                terminal,
                false,
                Some((
                    CompletionOutcome::RuntimeLost,
                    "LIFECYCLE_SINK_FAILED".into(),
                )),
            )
        } else {
            (terminal, natural_completion, forced_outcome)
        };
        self.finish_routed_terminal(
            TerminalTarget {
                agent_id,
                sink,
                route,
                runtime: &runtime,
            },
            TerminalDecision {
                terminal,
                natural_completion,
                forced_outcome,
                failure_message,
            },
        )
    }

    /// Handle the sink's latched transport fault on the loop paths that do
    /// not run the stall decision. Re-validates ownership before the real
    /// cleanup and the explicit `RUNTIME_TRANSPORT_FRAME_LIMIT` closure.
    #[allow(clippy::too_many_arguments)]
    fn handle_transport_failure(
        &self,
        agent_id: &str,
        owner_epoch: u64,
        runtime: &Arc<dyn ManagedRuntime>,
        sink: &StoreLifecycleSink,
        route: &TaskRoute,
        operation: &Mutex<()>,
        check: &ActiveCheck,
    ) -> FaultDisposition {
        let Some(failure) = sink.transport_failure() else {
            return FaultDisposition::Suppressed;
        };
        let guard = operation.lock().unwrap();
        #[cfg(test)]
        self.run_before_transport_cleanup_hook();
        match self.decision_task(agent_id) {
            Ok(Some(task))
                if !task.phase.is_terminal()
                    && task.owner_epoch == owner_epoch
                    && self.active_instance_matches(agent_id, owner_epoch) => {}
            Ok(_) => {
                // The row is gone, terminal, or owned by a newer epoch.
                self.abandon_active_monitor(agent_id, owner_epoch);
                return FaultDisposition::Abandoned;
            }
            Err(error) => {
                // A transient read failure must not orphan the watchdog:
                // report once and retry on the next tick.
                if sink.runtime_lifecycle.note_decision_read_error() {
                    self.record_failure(agent_id, error.to_string());
                }
                return FaultDisposition::Suppressed;
            }
        }
        // Close ingress before the real cleanup: `finish_routed_terminal`
        // flips the lifecycle phase to Terminal, so records the pump forwards
        // after that point are dropped instead of reopening the owner.
        sink.runtime_lifecycle
            .request_stop(&runtime.turn_snapshot());
        sink.runtime_lifecycle.force_terminating();
        self.commit_fault(
            agent_id,
            owner_epoch,
            runtime,
            sink,
            route,
            TRANSPORT_FRAME_LIMIT_REASON,
            |terminal| transport_failure_message(&failure, terminal),
            check,
            operation,
            guard,
        );
        FaultDisposition::Handled
    }

    /// Decide and execute a scheduler fault closure at the protected stall
    /// decision point.
    ///
    /// The decision is linearized on the publisher latch (no terminal can be
    /// published) and the admission latch (no event or pending input can be
    /// admitted). Inside both latches it re-reads the transport latch, the
    /// admitted-progress revision, the durable phase, pending input, and the
    /// published terminal, then atomically switches the lifecycle to
    /// terminating before releasing everything for the real stop/reap
    /// (B-B03, review R1/R2).
    #[allow(clippy::too_many_arguments)]
    fn handle_stall(
        &self,
        agent_id: &str,
        owner_epoch: u64,
        runtime: &Arc<dyn ManagedRuntime>,
        sink: &StoreLifecycleSink,
        route: &TaskRoute,
        operation: &Mutex<()>,
        check: &ActiveCheck,
    ) -> FaultDisposition {
        let guard = operation.lock().unwrap();
        #[cfg(test)]
        self.run_before_stall_cleanup_hook();
        // Outermost latch: while held, no pump can publish a terminal, and
        // because pumps call the sink under it, no event or pending input can
        // be admitted. The admission latch nests inside it, matching the
        // sink's publisher -> admission order.
        let publisher_latch = runtime.terminal_latch();
        let published_terminal = publisher_latch
            .as_ref()
            .and_then(|state| state.published_terminal());
        let mut admission = sink.runtime_lifecycle.decision_latch();

        let current = match self.decision_task(agent_id) {
            Ok(current) => current,
            Err(error) => {
                if sink.runtime_lifecycle.note_decision_read_error() {
                    self.record_failure(agent_id, error.to_string());
                }
                return FaultDisposition::Suppressed;
            }
        };
        let Some(task) = current.as_ref() else {
            drop(admission);
            drop(publisher_latch);
            self.abandon_active_monitor(agent_id, owner_epoch);
            return FaultDisposition::Abandoned;
        };
        if task.owner_epoch != owner_epoch
            || task.phase.is_terminal()
            || !self.active_instance_matches(agent_id, owner_epoch)
        {
            drop(admission);
            drop(publisher_latch);
            self.abandon_active_monitor(agent_id, owner_epoch);
            return FaultDisposition::Abandoned;
        }

        let now = self.now();
        // Re-read the transport latch inside the decision latch (review R2):
        // a fault latched after the loop-top check always takes the S01
        // closure with its transport reason and stage.
        let transport = sink.transport_failure();
        // Fold every admitted progress event; a revision that advanced after
        // the expiry check restarts the window and suppresses the stall.
        sink.runtime_lifecycle
            .stall_observe_progress(sink.activity.progress_revision(), now);
        let pending = self
            .inner
            .store
            .completion_blockers(agent_id)
            .map(|(pending, _)| pending)
            .unwrap_or(true);

        let stall_status = if transport.is_none()
            && task.phase == TaskPhase::Running
            && !task.stop_requested
            && !task.close_requested
            && published_terminal.is_none()
            && !pending
        {
            sink.runtime_lifecycle
                .stall_poll(now, self.inner.config.stall_timeout)
        } else {
            None
        };

        if transport.is_none() && stall_status.is_none() {
            // Freeze the window while the task waits for user input before
            // handing it back to the normal loop.
            sink.runtime_lifecycle
                .stall_set_waiting(task.phase == TaskPhase::WaitingInput, now);
            drop(admission);
            drop(publisher_latch);
            return FaultDisposition::Suppressed;
        }

        // Atomic commit under both latches: close ingress and record the
        // stall trigger before anything can be admitted or published.
        RuntimeLifecycle::request_stop_locked(&mut admission, &runtime.turn_snapshot());
        RuntimeLifecycle::force_terminating_locked(&mut admission);
        if transport.is_none() {
            sink.runtime_lifecycle.stall_mark_triggered();
        }
        drop(admission);
        drop(publisher_latch);

        match transport {
            Some(failure) => {
                self.commit_fault(
                    agent_id,
                    owner_epoch,
                    runtime,
                    sink,
                    route,
                    TRANSPORT_FRAME_LIMIT_REASON,
                    |terminal| transport_failure_message(&failure, terminal),
                    check,
                    operation,
                    guard,
                );
            }
            None => {
                let status = stall_status
                    .expect("a stall status exists when no transport fault was latched");
                self.commit_fault(
                    agent_id,
                    owner_epoch,
                    runtime,
                    sink,
                    route,
                    STALLED_NO_ACTIVITY_REASON,
                    |_| stall_failure_message(&status),
                    check,
                    operation,
                    guard,
                );
            }
        }
        FaultDisposition::Handled
    }

    /// Commit a scheduler fault closure. The caller holds the operation lock
    /// and has already closed ingress atomically; this releases the lock,
    /// performs the real stop/reap, re-acquires it, persists the forced
    /// outcome, releases the active slot, and advances the queue (B-B03).
    #[allow(clippy::too_many_arguments)]
    fn commit_fault<F>(
        &self,
        agent_id: &str,
        owner_epoch: u64,
        runtime: &Arc<dyn ManagedRuntime>,
        sink: &StoreLifecycleSink,
        route: &TaskRoute,
        forced_reason: &'static str,
        detail: F,
        check: &ActiveCheck,
        operation: &Mutex<()>,
        guard: MutexGuard<'_, ()>,
    ) where
        F: FnOnce(&RuntimeTerminal) -> String,
    {
        check.cancel();
        drop(guard);
        // The cleanup entry never trusts a terminal a late exit boundary may
        // have published (S01 handoff) and reports the real stop/reap.
        let terminal = runtime.cleanup_for_forced_failure(self.inner.config.stop_grace);
        let message = detail(&terminal);
        let _guard = operation.lock().unwrap();
        if let Err(error) = self.finish_locked_monitor_terminal(
            agent_id,
            owner_epoch,
            runtime,
            sink,
            route,
            None,
            terminal,
            false,
            Some((CompletionOutcome::Failed, forced_reason.into())),
            Some(message),
        ) {
            self.record_failure(agent_id, error.to_string());
        }
        self.release_active(agent_id, owner_epoch);
        if let Err(error) = self.start_ready() {
            self.record_failure(agent_id, error.to_string());
        }
    }

    pub(super) fn spawn_monitor(&self, context: MonitorContext) {
        let MonitorContext {
            agent_id,
            owner_epoch,
            runtime,
            sink,
            session_id,
            operation,
            runtime_lifecycle,
            route,
            task,
            check,
        } = context;
        let scheduler = self.clone();
        thread::spawn(move || {
            let mut handled_generation = 0;
            loop {
                if sink.transport_failure().is_some() {
                    match scheduler.handle_transport_failure(
                        &agent_id,
                        owner_epoch,
                        &runtime,
                        &sink,
                        &route,
                        &operation,
                        &check,
                    ) {
                        // Only a transient decision read failure lands here;
                        // back off a tick before retrying instead of spinning.
                        FaultDisposition::Suppressed => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        FaultDisposition::Handled | FaultDisposition::Abandoned => return,
                    }
                }
                #[cfg(test)]
                scheduler.run_before_terminal_wait_hook();
                if let Some(terminal) = runtime.wait_terminal(Duration::from_millis(50)) {
                    // A latched transport fault outranks any terminal a late
                    // child-exit boundary published while this wait slept, so
                    // re-check before normal turn adjudication.
                    if sink.transport_failure().is_some() {
                        let _ = terminal;
                        match scheduler.handle_transport_failure(
                            &agent_id,
                            owner_epoch,
                            &runtime,
                            &sink,
                            &route,
                            &operation,
                            &check,
                        ) {
                            // The confirmed latch keeps this monitor's fault
                            // priority: a transient decision read failure must
                            // retry from the loop top, never fall through into
                            // the normal terminal closure (that would drop the
                            // S01 reason, stage and forced cleanup).
                            FaultDisposition::Suppressed => {
                                thread::sleep(Duration::from_millis(10));
                                continue;
                            }
                            FaultDisposition::Handled | FaultDisposition::Abandoned => return,
                        }
                    }
                    let _guard = operation.lock().unwrap();
                    let natural = matches!(terminal, RuntimeTerminal::Completed(_));
                    if !natural && runtime_lifecycle.ingress_reason() == Some("LATE_AFTER_STOP") {
                        return;
                    }
                    if natural {
                        match sink.begin_natural_completion() {
                            Ok(NaturalCompletionAdmission::Deferred { pending: true, .. })
                            | Err(_) => {
                                drop(_guard);
                                thread::sleep(Duration::from_millis(10));
                                continue;
                            }
                            Ok(NaturalCompletionAdmission::Deferred {
                                pending: false,
                                queued: true,
                            }) => {
                                match scheduler.deliver_next_message(
                                    &agent_id,
                                    &session_id,
                                    &runtime,
                                    &runtime_lifecycle,
                                    scheduler.control_deadline(),
                                ) {
                                    Ok(Some(_)) => {
                                        drop(_guard);
                                        continue;
                                    }
                                    Ok(None) => {
                                        drop(_guard);
                                        continue;
                                    }
                                    Err(error) => {
                                        let detail = match &error {
                                            SchedulerError::RuntimeCommand { message, .. } => {
                                                message.clone()
                                            }
                                            _ => error.to_string(),
                                        };
                                        scheduler.record_runtime_failure(
                                            &agent_id,
                                            Some(&session_id),
                                            "message_delivery",
                                            "SESSION_SEND_FAILED",
                                            &detail,
                                            Some(runtime.as_ref()),
                                        );
                                        drop(_guard);
                                        continue;
                                    }
                                }
                            }
                            Ok(NaturalCompletionAdmission::Ready)
                            | Ok(NaturalCompletionAdmission::Bypass) => {}
                            Ok(NaturalCompletionAdmission::Deferred {
                                pending: false,
                                queued: false,
                            }) => unreachable!("blocked completion has a blocker"),
                        }
                    }
                    if !natural {
                        check.cancel();
                    }
                    if let Err(error) = scheduler.finish_locked_monitor_terminal(
                        &agent_id,
                        owner_epoch,
                        &runtime,
                        &sink,
                        &route,
                        task.as_ref(),
                        terminal,
                        natural,
                        None,
                        None,
                    ) {
                        scheduler.record_failure(&agent_id, error.to_string());
                    }
                    check.cancel();
                    scheduler.release_active(&agent_id, owner_epoch);
                    if let Err(error) = scheduler.start_ready() {
                        scheduler.record_failure(&agent_id, error.to_string());
                    }
                    return;
                }
                if sink.error().is_some() {
                    check.cancel();
                    let _guard = operation.lock().unwrap();
                    let Some(error) = sink.error() else {
                        continue;
                    };
                    runtime_lifecycle.request_stop(&runtime.turn_snapshot());
                    runtime_lifecycle.force_terminating();
                    let terminal = runtime.stop(scheduler.inner.config.stop_grace);
                    if let Err(store_error) = scheduler.finish_locked_monitor_terminal(
                        &agent_id,
                        owner_epoch,
                        &runtime,
                        &sink,
                        &route,
                        task.as_ref(),
                        terminal,
                        false,
                        Some((
                            CompletionOutcome::RuntimeLost,
                            "LIFECYCLE_SINK_FAILED".into(),
                        )),
                        None,
                    ) {
                        scheduler.record_failure(&agent_id, store_error.to_string());
                    }
                    scheduler.record_failure(&agent_id, error);
                    scheduler.release_active(&agent_id, owner_epoch);
                    return;
                }
                // S02: the stall watchdog shares this unified decision point.
                // It runs after the confirmed terminal/sink failures but
                // before turn adjudication. The window only advances on
                // admitted runtime progress; the full decision (owner epoch,
                // durable phase, pending, cancel/close, transport latch, and
                // published terminal) is re-validated under the linearization
                // latches inside `handle_stall`.
                let stall_now = scheduler.now();
                runtime_lifecycle
                    .stall_observe_progress(sink.activity.progress_revision(), stall_now);
                if runtime_lifecycle
                    .stall_poll(stall_now, scheduler.inner.config.stall_timeout)
                    .is_some()
                {
                    match scheduler.handle_stall(
                        &agent_id,
                        owner_epoch,
                        &runtime,
                        &sink,
                        &route,
                        &operation,
                        &check,
                    ) {
                        FaultDisposition::Suppressed => {}
                        FaultDisposition::Handled | FaultDisposition::Abandoned => return,
                    }
                }
                let turn = runtime.turn_snapshot();
                if !turn.active && turn.generation > handled_generation {
                    let Some(boundary) = turn.boundary else {
                        continue;
                    };
                    let _guard = operation.lock().unwrap();
                    if boundary != TurnBoundary::Completed
                        && runtime_lifecycle.ingress_reason().is_some()
                    {
                        return;
                    }
                    let current = runtime.turn_snapshot();
                    if current.active
                        || current.generation != turn.generation
                        || current.boundary != Some(boundary)
                    {
                        continue;
                    }
                    handled_generation = turn.generation;
                    let deadline = scheduler.control_deadline();
                    let delivery = if boundary == TurnBoundary::Completed {
                        match sink.begin_natural_completion() {
                            Ok(NaturalCompletionAdmission::Ready)
                            | Ok(NaturalCompletionAdmission::Bypass) => Ok(None),
                            Ok(NaturalCompletionAdmission::Deferred {
                                pending: false,
                                queued: true,
                            }) => scheduler.deliver_next_message(
                                &agent_id,
                                &session_id,
                                &runtime,
                                &runtime_lifecycle,
                                deadline,
                            ),
                            Ok(NaturalCompletionAdmission::Deferred { .. }) | Err(_) => {
                                handled_generation = handled_generation.saturating_sub(1);
                                continue;
                            }
                        }
                    } else {
                        scheduler.deliver_next_message(
                            &agent_id,
                            &session_id,
                            &runtime,
                            &runtime_lifecycle,
                            deadline,
                        )
                    };
                    match delivery {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            if boundary != TurnBoundary::Completed {
                                check.cancel();
                            }
                            let terminal = runtime.finish_turn(
                                boundary,
                                deadline.cleanup_grace(scheduler.inner.config.stop_grace),
                            );
                            if let Err(error) = scheduler.finish_locked_monitor_terminal(
                                &agent_id,
                                owner_epoch,
                                &runtime,
                                &sink,
                                &route,
                                task.as_ref(),
                                terminal,
                                boundary == TurnBoundary::Completed,
                                None,
                                None,
                            ) {
                                scheduler.record_failure(&agent_id, error.to_string());
                            }
                            check.cancel();
                            scheduler.release_active(&agent_id, owner_epoch);
                            if let Err(error) = scheduler.start_ready() {
                                scheduler.record_failure(&agent_id, error.to_string());
                            }
                            return;
                        }
                        Err(error) => {
                            check.cancel();
                            let cause = match &error {
                                SchedulerError::RuntimeCommand { message, .. } => message.clone(),
                                _ => error.to_string(),
                            };
                            let terminal = runtime.finish_turn(
                                TurnBoundary::Failed,
                                deadline.cleanup_grace(scheduler.inner.config.stop_grace),
                            );
                            let mut detail = serde_json::from_str::<serde_json::Value>(&cause)
                                .ok()
                                .filter(serde_json::Value::is_object)
                                .unwrap_or_else(
                                    || serde_json::json!({"message": bounded_error(&cause)}),
                                );
                            detail["cleanup_result"] = format!("{terminal:?}").into();
                            let detail = detail.to_string();
                            if let Err(finish_error) = scheduler.finish_locked_monitor_terminal(
                                &agent_id,
                                owner_epoch,
                                &runtime,
                                &sink,
                                &route,
                                task.as_ref(),
                                terminal,
                                false,
                                Some((CompletionOutcome::Failed, "MESSAGE_DELIVERY_FAILED".into())),
                                Some(detail.clone()),
                            ) {
                                scheduler.record_failure(&agent_id, finish_error.to_string());
                            }
                            scheduler.record_runtime_failure(
                                &agent_id,
                                Some(&session_id),
                                "message_delivery",
                                "SESSION_SEND_FAILED",
                                &detail,
                                Some(runtime.as_ref()),
                            );
                            scheduler.release_active(&agent_id, owner_epoch);
                            if let Err(start_error) = scheduler.start_ready() {
                                scheduler.record_failure(&agent_id, start_error.to_string());
                            }
                            return;
                        }
                    }
                }
            }
        });
    }
}

/// Bounded diagnostic detail for the transport closure. `bytes` is the
/// runtime's detection lower bound (`cap + 1`), never the true frame length;
/// `cleanup_result` always carries the real cleanup outcome (or its failure).
fn transport_failure_message(failure: &TransportFrameLimit, terminal: &RuntimeTerminal) -> String {
    serde_json::json!({
        "message": format!(
            "{}: oversized NDJSON frame rejected (bytes={} cap={} last_event_seq={})",
            failure.reason, failure.bytes, failure.cap, failure.last_event_seq
        ),
        "bytes": failure.bytes,
        "cap": failure.cap,
        "last_event_seq": failure.last_event_seq,
        "cleanup_result": format!("{terminal:?}"),
    })
    .to_string()
}

/// Bounded diagnostic detail for the stall closure: elapsed window, the
/// configured timeout, and the age of the last admitted progress.
fn stall_failure_message(status: &StallStatus) -> String {
    let millis = |value: Duration| u64::try_from(value.as_millis()).unwrap_or(u64::MAX);
    serde_json::json!({
        "message": format!(
            "no admitted runtime activity for {}ms (stall_timeout={}ms)",
            millis(status.elapsed),
            millis(status.timeout)
        ),
        "stall_elapsed_ms": millis(status.elapsed),
        "stall_timeout_ms": millis(status.timeout),
        "last_progress_age_ms": status.last_progress_age.map(millis),
    })
    .to_string()
}
