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

    /// Handle the sink's latched transport fault. Caller must not hold the
    /// per-task operation lock: this takes it and re-validates that the active
    /// instance and the durable task row still belong to this monitor before
    /// performing the real cleanup and the explicit
    /// `RUNTIME_TRANSPORT_FRAME_LIMIT` closure.
    fn handle_transport_failure(
        &self,
        agent_id: &str,
        owner_epoch: u64,
        runtime: &Arc<dyn ManagedRuntime>,
        sink: &StoreLifecycleSink,
        route: &TaskRoute,
        operation: &Mutex<()>,
        check: &ActiveCheck,
    ) {
        let Some(failure) = sink.transport_failure() else {
            return;
        };
        check.cancel();
        let _guard = operation.lock().unwrap();
        let current = match self.inner.store.get_task(agent_id) {
            Ok(current) => current,
            Err(error) => {
                self.record_failure(agent_id, error.to_string());
                return;
            }
        };
        let owned = current
            .as_ref()
            .is_some_and(|task| !task.phase.is_terminal() && task.owner_epoch == owner_epoch);
        if !owned || !self.active_instance_matches(agent_id, owner_epoch) {
            // Another control path already released or finished this instance.
            return;
        }
        #[cfg(test)]
        self.run_before_transport_cleanup_hook();
        // Close ingress before the real cleanup: `finish_routed_terminal`
        // flips the lifecycle phase to Terminal, so records the pump forwards
        // after that point are dropped instead of reopening the owner.
        sink.runtime_lifecycle
            .request_stop(&runtime.turn_snapshot());
        sink.runtime_lifecycle.force_terminating();
        let terminal = runtime.cleanup_for_transport_failure(self.inner.config.stop_grace);
        let message = transport_failure_message(&failure, &terminal);
        if let Err(error) = self.finish_locked_monitor_terminal(
            agent_id,
            owner_epoch,
            runtime,
            sink,
            route,
            current.as_ref(),
            terminal,
            false,
            Some((
                CompletionOutcome::Failed,
                TRANSPORT_FRAME_LIMIT_REASON.into(),
            )),
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
                    scheduler.handle_transport_failure(
                        &agent_id,
                        owner_epoch,
                        &runtime,
                        &sink,
                        &route,
                        &operation,
                        &check,
                    );
                    return;
                }
                if let Some(terminal) = runtime.wait_terminal(Duration::from_millis(50)) {
                    // A latched transport fault outranks any terminal a late
                    // child-exit boundary published while this wait slept, so
                    // re-check before normal turn adjudication.
                    if sink.transport_failure().is_some() {
                        let _ = terminal;
                        scheduler.handle_transport_failure(
                            &agent_id,
                            owner_epoch,
                            &runtime,
                            &sink,
                            &route,
                            &operation,
                            &check,
                        );
                        return;
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
                                None,
                            ) {
                                scheduler.record_failure(&agent_id, finish_error.to_string());
                            }
                            scheduler.record_runtime_failure(
                                &agent_id,
                                Some(&session_id),
                                "message_delivery",
                                "SESSION_SEND_FAILED",
                                &detail.to_string(),
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
