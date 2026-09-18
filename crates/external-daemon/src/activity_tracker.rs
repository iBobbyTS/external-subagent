use super::*;

#[derive(Default)]
struct PassiveActivityState {
    revision: u64,
    last_runtime_event_at: Option<(Instant, u64)>,
    active_model_requests: HashMap<String, Instant>,
    last_model_delta_at: Option<Instant>,
    latest_text_tail: String,
    latest_text_updated_at: Option<u64>,
    latest_text_truncated: bool,
    terminal_text: String,
    /// The current turn's terminal text was set by a boundary carrying the
    /// turn's verified final text; later streaming echoes must not pollute it.
    terminal_text_settled: bool,
    active_tools: HashMap<String, (PassiveToolKind, Instant)>,
    samples: HashMap<String, ActivitySample>,
    sample_order: VecDeque<String>,
    telemetry_degraded: bool,
    observation: observation::ObservationState,
}

pub(crate) struct PassiveActivityTracker {
    state: Mutex<PassiveActivityState>,
    changed: Condvar,
    runtime_source_verified: AtomicBool,
}

impl PassiveActivityTracker {
    pub(crate) fn new(runtime_source_verified: bool) -> Self {
        let state = PassiveActivityState::default();
        Self {
            state: Mutex::new(state),
            changed: Condvar::new(),
            runtime_source_verified: AtomicBool::new(runtime_source_verified),
        }
    }

    pub(crate) fn observe(&self, event: &RuntimeEvent) {
        self.observe_at(event, Instant::now(), activity_wall_now_millis());
    }

    fn observe_at(&self, event: &RuntimeEvent, now: Instant, wall_now_ms: u64) {
        let mut state = self.state.lock().unwrap();
        match event {
            RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(event))) => {
                state
                    .observation
                    .observe_message(&event.method, &event.params);
            }
            RuntimeEvent::Driver(Inbound::Message(WireMessage::UnknownEvent { method, raw })) => {
                let params = raw.get("params").unwrap_or(&serde_json::Value::Null);
                state.observation.observe_message(method, params);
            }
            RuntimeEvent::Driver(Inbound::Malformed(_) | Inbound::OversizedLine { .. }) => {
                state.observation.observe_loss();
            }
            _ => {}
        }
        state.revision = state.revision.saturating_add(1);
        state.last_runtime_event_at = Some((now, wall_now_ms));
        let parsed = parse_passive_activity(event);
        if parsed.source == ActivitySource::Telemetry && !parsed.telemetry_known {
            state.telemetry_degraded = true;
        }

        let mut admitted = true;
        if admitted {
            if let (Some(identity), Some(sample)) = (parsed.identity.as_ref(), parsed.sample) {
                let replace = match state.samples.get(identity) {
                    Some(existing) => {
                        existing.source == ActivitySource::Telemetry
                            && parsed.source == ActivitySource::Session
                    }
                    None => true,
                };
                if replace {
                    if !state.samples.contains_key(identity) {
                        state.sample_order.push_back(identity.clone());
                    }
                    state.samples.insert(
                        identity.clone(),
                        ActivitySample {
                            source: parsed.source,
                            observed_at: now,
                            kind: sample,
                        },
                    );
                } else {
                    admitted = false;
                }
            }
        }

        while state.sample_order.len() > MAX_ACTIVITY_IDENTITIES {
            if let Some(identity) = state.sample_order.pop_front() {
                state.samples.remove(&identity);
            }
        }

        if admitted
            && matches!(
                parsed.sample,
                Some(ActivitySampleKind::ReasoningDelta | ActivitySampleKind::TextDelta)
            )
        {
            state.last_model_delta_at = Some(now);
        }
        if admitted {
            if let Some(delta) = parsed.text_delta.as_deref() {
                append_latest_text(&mut state, delta, wall_now_ms);
            }
            if let Some(response) = parsed.terminal_response.as_deref() {
                // A boundary that carries the turn's verified final text
                // settles it: streaming deltas racing past the boundary are
                // echoes of a message the settlement already folded.
                state.terminal_text = response.to_owned();
                state.terminal_text_settled = true;
            }
        }

        match parsed.transition {
            Some(ActivityTransition::ModelStarted) => {
                let request_id = parsed.request_id.unwrap_or_else(|| "model-request".into());
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    state.active_model_requests.entry(request_id)
                {
                    entry.insert(now);
                    state.last_model_delta_at = Some(now);
                }
            }
            Some(ActivityTransition::ModelCompleted) => {
                if let Some(request_id) = parsed.request_id.as_deref() {
                    state.active_model_requests.remove(request_id);
                } else {
                    state.active_model_requests.clear();
                }
            }
            Some(ActivityTransition::ToolScheduled | ActivityTransition::ToolStarted) => {
                if let Some(tool_call_id) = parsed.tool_call_id {
                    state
                        .active_tools
                        .entry(tool_call_id)
                        .or_insert((parsed.tool_kind, now));
                }
            }
            Some(
                ActivityTransition::ToolCompleted
                | ActivityTransition::ToolFailed
                | ActivityTransition::PermissionResolved,
            ) => {
                if let Some(tool_call_id) = parsed.tool_call_id {
                    state.active_tools.remove(&tool_call_id);
                }
            }
            Some(ActivityTransition::TurnCompleted | ActivityTransition::TurnFailed) => {
                state.active_model_requests.clear();
                state.active_tools.clear();
            }
            Some(ActivityTransition::TurnStarted) => {
                // Terminal text is scoped to one turn: a later turn's result
                // must never inherit an earlier turn's text.
                state.terminal_text.clear();
                state.terminal_text_settled = false;
            }
            Some(ActivityTransition::PermissionRequested) | None => {}
        }
        self.changed.notify_all();
    }

    pub(crate) fn snapshot(&self) -> PassiveActivitySnapshot {
        self.snapshot_at(Instant::now())
    }

    fn snapshot_at(&self, now: Instant) -> PassiveActivitySnapshot {
        let state = self.state.lock().unwrap();
        let mut window = PassiveActivityWindow::default();
        for sample in state.samples.values() {
            if now.saturating_duration_since(sample.observed_at) > PASSIVE_ACTIVITY_WINDOW {
                continue;
            }
            match sample.kind {
                ActivitySampleKind::ReasoningDelta => {
                    window.reasoning_delta_events = window.reasoning_delta_events.saturating_add(1);
                }
                ActivitySampleKind::TextDelta => {
                    window.text_delta_events = window.text_delta_events.saturating_add(1);
                }
                ActivitySampleKind::ToolStarted { kind } => {
                    window.tool_calls_started = window.tool_calls_started.saturating_add(1);
                    match kind {
                        PassiveToolKind::Read => {
                            window.read_calls = window.read_calls.saturating_add(1)
                        }
                        PassiveToolKind::Bash => {
                            window.bash_calls = window.bash_calls.saturating_add(1)
                        }
                        PassiveToolKind::Other => {
                            window.other_tool_calls = window.other_tool_calls.saturating_add(1)
                        }
                    }
                }
                ActivitySampleKind::ToolCompleted => {
                    window.tool_calls_completed = window.tool_calls_completed.saturating_add(1)
                }
                ActivitySampleKind::ToolFailed => {
                    window.tool_calls_failed = window.tool_calls_failed.saturating_add(1)
                }
            }
        }
        let mut active_tools = state
            .active_tools
            .iter()
            .map(|(tool_call_id, (kind, _))| PassiveActiveTool {
                tool_call_id: tool_call_id.clone(),
                kind: *kind,
            })
            .collect::<Vec<_>>();
        active_tools.sort_by(|left, right| left.tool_call_id.cmp(&right.tool_call_id));
        PassiveActivitySnapshot {
            revision: state.revision,
            last_runtime_event_at: state.last_runtime_event_at.map(|(_, wall)| wall),
            last_activity_age_ms: state
                .last_runtime_event_at
                .map(|(at, _)| duration_millis(now.saturating_duration_since(at))),
            model_request_active: !state.active_model_requests.is_empty(),
            model_request_age_ms: state
                .active_model_requests
                .values()
                .min()
                .map(|at| duration_millis(now.saturating_duration_since(*at))),
            model_last_delta_age_ms: state
                .last_model_delta_at
                .map(|at| duration_millis(now.saturating_duration_since(at))),
            latest_text_tail: state.latest_text_tail.clone(),
            latest_text_updated_at: state.latest_text_updated_at,
            latest_text_truncated: state.latest_text_truncated,
            latest_reasoning: if self.runtime_source_verified() {
                state.observation.snapshot().reasoning.text
            } else {
                String::new()
            },
            active_tools,
            oldest_active_tool_age_ms: state
                .active_tools
                .values()
                .map(|(_, at)| duration_millis(now.saturating_duration_since(*at)))
                .max(),
            window_60s: window,
            telemetry_degraded: state.telemetry_degraded,
        }
    }

    pub(crate) fn take_terminal_text(&self) -> TerminalText {
        let mut state = self.state.lock().unwrap();
        if state.terminal_text.trim().is_empty() {
            state.terminal_text.clear();
            TerminalText::Missing
        } else {
            TerminalText::Visible(std::mem::take(&mut state.terminal_text))
        }
    }

    pub(crate) fn observation_snapshot(&self) -> observation::ObservationSnapshot {
        self.state.lock().unwrap().observation.snapshot()
    }

    pub(crate) fn confirm_runtime_source(&self, still_verified: bool) {
        if !still_verified {
            self.runtime_source_verified.store(false, Ordering::Release);
        }
    }

    pub(crate) fn runtime_source_verified(&self) -> bool {
        self.runtime_source_verified.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn set_wait_fixture(&self, now: Instant) {
        let mut state = self.state.lock().unwrap();
        state.revision = 900;
        state.latest_text_tail = "ordinary text".into();
        state.last_model_delta_at = Some(now);
        state
            .active_tools
            .insert("tool".into(), (PassiveToolKind::Bash, now));
        state.samples.insert(
            "tool".into(),
            ActivitySample {
                source: ActivitySource::Session,
                observed_at: now,
                kind: ActivitySampleKind::ToolStarted {
                    kind: PassiveToolKind::Bash,
                },
            },
        );
        state.sample_order.push_back("tool".into());
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TerminalText {
    Visible(String),
    Missing,
}

fn duration_millis(value: Duration) -> u64 {
    value.as_millis().try_into().unwrap_or(u64::MAX)
}

fn activity_wall_now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn append_latest_text(state: &mut PassiveActivityState, delta: &str, wall_now_ms: u64) {
    if !state.terminal_text_settled {
        state.terminal_text.push_str(delta);
    }
    state.latest_text_tail.push_str(delta);
    if state.latest_text_tail.len() > MAX_LATEST_TEXT_BYTES {
        let mut split = state.latest_text_tail.len() - MAX_LATEST_TEXT_BYTES;
        while !state.latest_text_tail.is_char_boundary(split) {
            split += 1;
        }
        state.latest_text_tail.drain(..split);
        state.latest_text_truncated = true;
    }
    state.latest_text_updated_at = Some(wall_now_ms);
}
