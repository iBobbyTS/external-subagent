use super::*;

#[derive(Default)]
struct PassiveActivityState {
    revision: u64,
    /// Admitted-progress revision for the stall watchdog: every admitted
    /// runtime event except `Malformed`/`OversizedLine` advances it.
    progress_revision: u64,
    last_runtime_event_at: Option<(Instant, u64)>,
    active_model_requests: HashMap<String, Instant>,
    last_model_delta_at: Option<Instant>,
    latest_text_tail: String,
    latest_text_updated_at: Option<u64>,
    latest_text_truncated: bool,
    /// Monotonic total UTF-8 bytes ever appended to the wait text window. Only
    /// `append_latest_text` advances it; draining the window never reduces it.
    appended_bytes: u64,
    /// Bytes of the wait text stream already delivered by take-on-delivery.
    /// Advanced only by `take_wait_tail`; always `<= appended_bytes`.
    delivered_bytes: u64,
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
    /// Which tool-activity vocabularies this task's adapter may emit. Fixed at
    /// construction: `Mixed` for [`Self::new`], inferred from the adapter name
    /// by [`Self::for_adapter`]. See [`ToolActivityMode`].
    mode: ToolActivityMode,
    /// Launch-scoped public-source evidence: true only when the adapter that
    /// actually launched this task has a verified public observation source.
    /// Constructed from the claimed task's route identity, never from a
    /// scheduler-global proof about a different runtime.
    runtime_source_verified: AtomicBool,
}

impl PassiveActivityTracker {
    pub(crate) fn new(runtime_source_verified: bool) -> Self {
        let state = PassiveActivityState::default();
        Self {
            state: Mutex::new(state),
            changed: Condvar::new(),
            // The pre-mode default: accept both vocabularies so an
            // unregistered caller keeps its behavior.
            mode: ToolActivityMode::Mixed,
            runtime_source_verified: AtomicBool::new(runtime_source_verified),
        }
    }

    pub(crate) fn for_adapter(adapter: &str, runtime_source_verified: bool) -> Self {
        let mut tracker = Self::new(runtime_source_verified);
        tracker.state.lock().unwrap().observation =
            observation::ObservationState::for_adapter(adapter);
        tracker.mode = match adapter {
            "codex" => ToolActivityMode::CountOnly,
            "zcode" | "dsh" => ToolActivityMode::Detailed,
            _ => ToolActivityMode::Mixed,
        };
        tracker
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
        // A dropped or oversized frame is a loss, not progress: it must never
        // restart the S02 no-activity window.
        if !matches!(
            event,
            RuntimeEvent::Driver(Inbound::Malformed(_) | Inbound::OversizedLine { .. })
        ) {
            state.progress_revision = state.progress_revision.saturating_add(1);
        }
        let parsed = parse_passive_activity(event, self.mode);
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

    /// Monotonic count of admitted progress events (losses excluded). The
    /// stall watchdog restarts its window whenever this advances.
    pub(crate) fn progress_revision(&self) -> u64 {
        self.state.lock().unwrap().progress_revision
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
                ActivitySampleKind::ToolStarted { kind, count } => {
                    window.tool_calls_started = window.tool_calls_started.saturating_add(count);
                    match kind {
                        PassiveToolKind::Read => {
                            window.read_calls = window.read_calls.saturating_add(count)
                        }
                        PassiveToolKind::Bash => {
                            window.bash_calls = window.bash_calls.saturating_add(count)
                        }
                        PassiveToolKind::Other => {
                            window.other_tool_calls = window.other_tool_calls.saturating_add(count)
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

    /// Take-on-delivery projection of the wait text window: return only the
    /// bytes appended since the previous take (from any caller) and advance the
    /// delivery cursor to the current append point in the same lock. A caller
    /// that fell behind the rolling window (its undelivered bytes were
    /// drained) degrades to the whole current window with `truncated = true`;
    /// the normal path returns the unseen suffix and `truncated = false`, and
    /// an empty suffix when nothing new arrived.
    pub(crate) fn take_wait_tail(&self) -> WaitTail {
        let mut state = self.state.lock().unwrap();
        let appended = state.appended_bytes;
        let window_len = state.latest_text_tail.len() as u64;
        debug_assert!(appended >= window_len);
        let window_start = appended.saturating_sub(window_len);
        let delivered = state.delivered_bytes;
        let (text, truncated) = if delivered < window_start {
            // Undelivered bytes rolled out of the window: the only honest
            // answer is the whole window, flagged as a gap.
            (state.latest_text_tail.clone(), true)
        } else {
            let start = (delivered - window_start) as usize;
            debug_assert!(state.latest_text_tail.is_char_boundary(start));
            (state.latest_text_tail[start..].to_owned(), false)
        };
        state.delivered_bytes = appended;
        WaitTail { text, truncated }
    }

    /// Carry the wait text stream across a tracker replacement (a follow-up or
    /// resume claim). The incoming tracker may already have collected text
    /// (the runtime sink is wired before the claim reaches the map swap), so
    /// this is a MERGE, not an overwrite: the old window is the stream's tail
    /// before the new tracker's bytes, and the merged window is their
    /// concatenation trimmed from the head to the 8 KiB cap on a char boundary.
    /// When both pre-merge windows are within the cap this is exactly the bytes
    /// the full appended stream's last window would hold; once the incoming
    /// tracker has already rolled its own window the concatenation is no
    /// longer a literal contiguous suffix (a window of a window), and the next
    /// take safely degrades to the whole merged window with `truncated = true`.
    /// Cursors merge as `appended = old + new`, `delivered = old` (the new
    /// tracker cannot have delivered: takes need the state lock this
    /// replacement holds). The per-turn terminal text and the sticky window
    /// truncation flag are deliberately NOT inherited. Locks are taken in the
    /// fixed order old-read then new-write, never held together.
    pub(crate) fn inherit_wait_text(&self, old: &PassiveActivityTracker) {
        let (old_tail, old_appended, old_delivered, old_updated) = {
            let old_state = old.state.lock().unwrap();
            (
                old_state.latest_text_tail.clone(),
                old_state.appended_bytes,
                old_state.delivered_bytes,
                old_state.latest_text_updated_at,
            )
        };
        let mut state = self.state.lock().unwrap();
        debug_assert_eq!(
            state.delivered_bytes, 0,
            "the incoming tracker cannot have delivered before the map swap"
        );
        let new_appended = state.appended_bytes;
        let new_updated = state.latest_text_updated_at;
        let mut merged = old_tail;
        merged.push_str(&state.latest_text_tail);
        if merged.len() > MAX_LATEST_TEXT_BYTES {
            let mut split = merged.len() - MAX_LATEST_TEXT_BYTES;
            while !merged.is_char_boundary(split) {
                split += 1;
            }
            merged.drain(..split);
        }
        state.latest_text_tail = merged;
        state.appended_bytes = old_appended.saturating_add(new_appended);
        state.delivered_bytes = old_delivered;
        state.latest_text_updated_at = match (old_updated, new_updated) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        debug_assert!(state.appended_bytes >= state.latest_text_tail.len() as u64);
    }

    pub(crate) fn observation_snapshot(&self) -> observation::ObservationSnapshot {
        self.state.lock().unwrap().observation.snapshot()
    }

    /// Post-spawn re-verification of the launched adapter's public source.
    /// Degrade-only: a changed or vanished runtime file clears trust for the
    /// rest of the run; a later passing re-check can never resurrect it.
    pub(crate) fn confirm_runtime_source(&self, still_verified: bool) {
        if !still_verified {
            self.runtime_source_verified.store(false, Ordering::Release);
        }
    }

    pub(crate) fn runtime_source_verified(&self) -> bool {
        self.runtime_source_verified.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn activity_mode(&self) -> ToolActivityMode {
        self.mode
    }

    #[cfg(test)]
    pub(crate) fn set_wait_fixture(&self, now: Instant) {
        let mut state = self.state.lock().unwrap();
        state.revision = 900;
        state.latest_text_tail = "ordinary text".into();
        state.appended_bytes = state.latest_text_tail.len() as u64;
        state.delivered_bytes = 0;
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
                    count: 1,
                },
            },
        );
        state.sample_order.push_back("tool".into());
    }

    #[cfg(test)]
    pub(crate) fn set_wait_tail_fixture(&self, tail: &str) {
        let mut state = self.state.lock().unwrap();
        state.latest_text_tail = tail.into();
        // The fixture injects a window directly, bypassing append: keep the
        // counters consistent so the first take returns the whole window.
        state.appended_bytes = state.latest_text_tail.len() as u64;
        state.delivered_bytes = 0;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TerminalText {
    Visible(String),
    Missing,
}

/// One take-on-delivery wait projection: the newly delivered `text` plus
/// whether the caller had fallen behind the rolling window (`truncated`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WaitTail {
    pub text: String,
    pub truncated: bool,
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
    state.appended_bytes = state.appended_bytes.saturating_add(delta.len() as u64);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_samples_weight_the_window_and_expire_without_refresh() {
        fn count_event(event_id: &str, count: u64) -> RuntimeEvent {
            RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(
                external_contract::EventEnvelope {
                    method: "session/event".into(),
                    params: serde_json::json!({
                        "type": "tool.updated",
                        "eventId": event_id,
                        "turnId": "t1",
                        "payload": {"kind": "count", "count": count},
                    }),
                },
            )))
        }
        let tracker = PassiveActivityTracker::new(false);
        let base = Instant::now();
        tracker.observe_at(&count_event("a", 3), base, 1_000);
        tracker.observe_at(&count_event("b", 2), base, 1_000);
        let window = tracker.snapshot_at(base).window_60s;
        assert_eq!(window.tool_calls_started, 5);
        // The count-only path carries no tool name, so every unit lands in the
        // internal Other bucket; read/bash stay zero.
        assert_eq!(window.other_tool_calls, 5);
        assert_eq!(window.read_calls, 0);
        assert_eq!(window.bash_calls, 0);

        // A re-delivery of an already-admitted identity is refused: no extra
        // weight and the original sample time is not refreshed.
        tracker.observe_at(&count_event("a", 3), base + Duration::from_secs(30), 2_000);
        assert_eq!(
            tracker
                .snapshot_at(base + Duration::from_secs(30))
                .window_60s
                .tool_calls_started,
            5
        );

        // Past the 60-second window measured from first receipt, both samples
        // fall out even though "a" was re-delivered at t=30.
        assert_eq!(
            tracker
                .snapshot_at(base + Duration::from_secs(61))
                .window_60s
                .tool_calls_started,
            0
        );
    }

    #[test]
    fn for_adapter_infers_the_activity_mode() {
        assert_eq!(
            PassiveActivityTracker::new(false).activity_mode(),
            ToolActivityMode::Mixed,
            "the bare constructor stays on the safe Mixed default"
        );
        assert_eq!(
            PassiveActivityTracker::for_adapter("codex", false).activity_mode(),
            ToolActivityMode::CountOnly
        );
        assert_eq!(
            PassiveActivityTracker::for_adapter("zcode", false).activity_mode(),
            ToolActivityMode::Detailed
        );
        assert_eq!(
            PassiveActivityTracker::for_adapter("dsh", false).activity_mode(),
            ToolActivityMode::Detailed
        );
        assert_eq!(
            PassiveActivityTracker::for_adapter("future-agent", false).activity_mode(),
            ToolActivityMode::Mixed
        );
    }

    #[test]
    fn strict_modes_match_mixed_for_each_adapters_production_shapes() {
        use external_agent_dsh::acp::update::{canonical_event_payloads, parse_update};

        fn typed_session(params: serde_json::Value) -> RuntimeEvent {
            RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(
                external_contract::EventEnvelope {
                    method: "session/event".into(),
                    params,
                },
            )))
        }
        fn unknown_session(params: serde_json::Value) -> RuntimeEvent {
            RuntimeEvent::Driver(Inbound::Message(WireMessage::UnknownEvent {
                method: "session/event".into(),
                raw: serde_json::json!({"method": "session/event", "params": params}),
            }))
        }
        fn assert_strict_matches_mixed(
            strict: &PassiveActivityTracker,
            mixed: &PassiveActivityTracker,
            now: Instant,
        ) {
            let strict_snapshot = strict.snapshot_at(now);
            let mixed_snapshot = mixed.snapshot_at(now);
            assert_eq!(strict_snapshot.window_60s, mixed_snapshot.window_60s);
            assert_eq!(strict_snapshot.active_tools, mixed_snapshot.active_tools);
            assert_eq!(strict_snapshot.revision, mixed_snapshot.revision);
        }

        let now = Instant::now();

        // ZCode native detailed events: named started (with toolName) + result.
        let zcode_events = || {
            vec![
                typed_session(external_contract::activity::tool_started_event(
                    "z1",
                    "t1",
                    "call-1",
                    Some("Read"),
                )),
                typed_session(external_contract::activity::tool_started_event(
                    "z2",
                    "t1",
                    "call-2",
                    Some("Bash"),
                )),
                typed_session(external_contract::activity::tool_result_event(
                    "z3", "t1", "call-1",
                )),
            ]
        };
        let zcode = PassiveActivityTracker::for_adapter("zcode", false);
        let zcode_mixed = PassiveActivityTracker::new(false);
        for event in zcode_events() {
            zcode.observe_at(&event, now, 1_000);
            zcode_mixed.observe_at(&event, now, 1_000);
        }
        assert_strict_matches_mixed(&zcode, &zcode_mixed, now);
        let zcode_window = zcode.snapshot_at(now).window_60s;
        assert_eq!(zcode_window.tool_calls_started, 2);
        assert_eq!(zcode_window.read_calls, 1);
        assert_eq!(zcode_window.bash_calls, 1);
        assert_eq!(zcode_window.tool_calls_completed, 1);

        // DSH canonical projections: named started, unnamed started, result.
        let dsh_events = || {
            let mut events = Vec::new();
            let named = parse_update(&serde_json::json!({
                "sessionId": "s",
                "update": {"sessionUpdate": "tool_call", "toolCallId": "call-3", "kind": "edit"}
            }))
            .unwrap();
            for params in canonical_event_payloads(&named, "d1", "t1") {
                events.push(unknown_session(params));
            }
            let unnamed = parse_update(&serde_json::json!({
                "sessionId": "s",
                "update": {"sessionUpdate": "tool_call_update", "toolCallId": "call-4"}
            }))
            .unwrap();
            for params in canonical_event_payloads(&unnamed, "d2", "t1") {
                events.push(unknown_session(params));
            }
            let result = parse_update(&serde_json::json!({
                "sessionId": "s",
                "update": {
                    "sessionUpdate": "tool_call_update", "toolCallId": "call-3",
                    "content": [{"type": "text", "text": "done"}]
                }
            }))
            .unwrap();
            for params in canonical_event_payloads(&result, "d3", "t1") {
                events.push(unknown_session(params));
            }
            events
        };
        let dsh = PassiveActivityTracker::for_adapter("dsh", false);
        let dsh_mixed = PassiveActivityTracker::new(false);
        for event in dsh_events() {
            dsh.observe_at(&event, now, 1_000);
            dsh_mixed.observe_at(&event, now, 1_000);
        }
        assert_strict_matches_mixed(&dsh, &dsh_mixed, now);
        let dsh_window = dsh.snapshot_at(now).window_60s;
        assert_eq!(dsh_window.tool_calls_started, 2);
        assert_eq!(dsh_window.tool_calls_completed, 1);

        // Codex count-only events.
        let codex_events = || {
            vec![
                typed_session(external_contract::activity::tool_count_event("c1", "t1", 1)),
                typed_session(external_contract::activity::tool_count_event("c2", "t1", 3)),
            ]
        };
        let codex = PassiveActivityTracker::for_adapter("codex", false);
        let codex_mixed = PassiveActivityTracker::new(false);
        for event in codex_events() {
            codex.observe_at(&event, now, 1_000);
            codex_mixed.observe_at(&event, now, 1_000);
        }
        assert_strict_matches_mixed(&codex, &codex_mixed, now);
        assert_eq!(codex.snapshot_at(now).window_60s.tool_calls_started, 4);

        // Both strict modes reject the foreign vocabulary.
        let count_for_detailed =
            typed_session(external_contract::activity::tool_count_event("x1", "t1", 7));
        zcode.observe_at(&count_for_detailed, now, 2_000);
        assert_eq!(
            zcode.snapshot_at(now).window_60s.tool_calls_started,
            2,
            "Detailed must ignore the count vocabulary"
        );
        for event in zcode_events() {
            codex.observe_at(&event, now, 2_000);
        }
        assert_eq!(
            codex.snapshot_at(now).window_60s.tool_calls_started,
            4,
            "CountOnly must ignore the detailed vocabulary"
        );
        assert!(
            codex.snapshot_at(now).active_tools.is_empty(),
            "ignored detailed events must not enter active_tools"
        );
    }

    #[test]
    fn stall_progress_ignores_malformed_and_oversized_frames() {
        let tracker = PassiveActivityTracker::new(false);
        let message = RuntimeEvent::Driver(Inbound::Message(WireMessage::UnknownEvent {
            method: "session/event".into(),
            raw: serde_json::json!({"method": "session/event", "params": {"type": "model.streaming"}}),
        }));
        tracker.observe(&message);
        assert_eq!(tracker.progress_revision(), 1);
        // A loss is not progress: it must never restart the stall window.
        tracker.observe(&RuntimeEvent::Driver(Inbound::Malformed("bad".into())));
        tracker.observe(&RuntimeEvent::Driver(Inbound::OversizedLine { bytes: 42 }));
        assert_eq!(tracker.progress_revision(), 1);
        tracker.observe(&RuntimeEvent::Driver(Inbound::Lifecycle {
            sequence: 2,
            method: "turn.started".into(),
            order: external_contract::LifecycleOrder::InOrder,
        }));
        assert_eq!(tracker.progress_revision(), 2);
    }

    #[test]
    fn dsh_acp_invalid_thoughts_report_loss_but_empty_text_does_not() {
        use external_agent_dsh::acp::update::{canonical_event_payloads, parse_update};

        for content in [
            None,
            Some(serde_json::json!({"type": "text"})),
            Some(serde_json::json!({"type": "text", "text": 42})),
            Some(serde_json::json!({"type": "text", "text": "bad\0text"})),
            Some(serde_json::json!({"type": "text", "text": ""})),
        ] {
            let valid_empty =
                content.as_ref().and_then(|c| c.get("text")) == Some(&serde_json::json!(""));
            let dsh = PassiveActivityTracker::for_adapter("dsh", true);
            let codex = PassiveActivityTracker::for_adapter("codex", false);
            let mut params = serde_json::json!({
                "sessionId": "s1", "update": {"sessionUpdate": "agent_thought_chunk"}
            });
            if let Some(content) = content {
                params["update"]["content"] = content;
            }
            let update = parse_update(&params).unwrap();
            for payload in canonical_event_payloads(&update, "e1", "t1") {
                let event = RuntimeEvent::Driver(Inbound::Message(WireMessage::UnknownEvent {
                    method: "session/event".into(),
                    raw: serde_json::json!({"method": "session/event", "params": payload}),
                }));
                // Re-delivery must not count the same lost chunk twice.
                dsh.observe(&event);
                dsh.observe(&event);
                codex.observe(&event);
            }
            let snapshot = dsh.observation_snapshot();
            assert_eq!(
                snapshot.coverage.reasoning_complete, valid_empty,
                "{params}"
            );
            assert_eq!(snapshot.coverage.dropped_events, u64::from(!valid_empty));
            assert!(snapshot.reasoning.text.is_empty());
            assert!(dsh.snapshot().latest_reasoning.is_empty());
            let hidden = codex.observation_snapshot();
            assert_eq!(hidden.coverage.dropped_events, 0);
            assert_eq!(hidden.snapshot_seq, 0);
        }
    }

    #[test]
    fn dsh_public_acp_thoughts_reach_observe_and_wait_but_codex_never_collects() {
        use external_agent_dsh::acp::update::{canonical_event_payloads, parse_update};
        let dsh = PassiveActivityTracker::for_adapter("dsh", true);
        let codex = PassiveActivityTracker::for_adapter("codex", false);
        let delta = format!("prefix{}", "中🙂".repeat(110));
        let update = parse_update(&serde_json::json!({
            "sessionId": "s1", "update": {
                "sessionUpdate": "agent_thought_chunk",
                "content": {"type": "text", "text": delta, "encrypted_content": "SECRET"}
            }
        }))
        .unwrap();
        for params in canonical_event_payloads(&update, "e1", "t1") {
            let event = RuntimeEvent::Driver(Inbound::Message(WireMessage::UnknownEvent {
                method: "session/event".into(),
                raw: serde_json::json!({"method":"session/event", "params":params}),
            }));
            dsh.observe(&event);
            codex.observe(&event);
        }
        let tool = parse_update(&serde_json::json!({
            "sessionId": "s1", "update": {
                "sessionUpdate": "tool_call", "toolCallId": "call-1", "kind": "read",
                "rawInput": {"path":"not currently projected", "encrypted_content":"SECRET"}
            }
        }))
        .unwrap();
        for params in canonical_event_payloads(&tool, "e2", "t1") {
            dsh.observe(&RuntimeEvent::Driver(Inbound::Message(
                WireMessage::UnknownEvent {
                    method: "session/event".into(),
                    raw: serde_json::json!({"method":"session/event", "params":params}),
                },
            )));
        }
        let snapshot = dsh.observation_snapshot();
        assert_eq!(snapshot.tools.len(), 1);
        assert_eq!(snapshot.tools[0].call_count, 1);
        assert!(snapshot.tools[0].recent_calls[0].arguments.is_empty());
        assert!(snapshot.tools[0].recent_calls[0].arguments_truncated);
        assert!(!serde_json::to_string(&snapshot).unwrap().contains("SECRET"));
        assert_eq!(snapshot.reasoning.text, "中🙂".repeat(100));
        assert!(snapshot.reasoning.truncated);
        assert_eq!(
            snapshot.reasoning.source,
            observation::ReasoningSource::dsh()
        );
        assert!(snapshot.coverage.reasoning_complete);
        assert!(!snapshot.coverage.tool_history_complete);
        assert_eq!(snapshot.coverage.dropped_events, 0);
        assert_eq!(dsh.snapshot().latest_reasoning, snapshot.reasoning.text);
        let hidden = codex.observation_snapshot();
        assert!(hidden.reasoning.text.is_empty());
        assert_eq!(
            hidden.snapshot_seq, 0,
            "hidden reasoning never enters observation state"
        );
        assert!(!hidden.coverage.reasoning_complete);
        assert!(!hidden.coverage.tool_history_complete);
        assert!(codex.snapshot().latest_reasoning.is_empty());
    }

    #[test]
    fn launch_scoped_evidence_only_degrades_across_reverification() {
        let tracker = PassiveActivityTracker::new(true);
        assert!(tracker.runtime_source_verified());
        // A post-spawn re-verification that fails (the launched adapter's
        // runtime file changed or vanished) clears trust for the run...
        tracker.confirm_runtime_source(false);
        assert!(!tracker.runtime_source_verified());
        // ...and a later passing re-check can never resurrect it mid-run.
        tracker.confirm_runtime_source(true);
        assert!(!tracker.runtime_source_verified());
        // An unverified launch (non-ZCode adapter, or absent proof) stays
        // unverified no matter what a later re-check observes.
        let unverified = PassiveActivityTracker::new(false);
        unverified.confirm_runtime_source(true);
        assert!(!unverified.runtime_source_verified());
    }

    fn append_delta(tracker: &PassiveActivityTracker, delta: &str) {
        tracker.observe(&RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(
            external_contract::EventEnvelope {
                method: "session/event".into(),
                params: serde_json::json!({
                    "type": "model.streaming",
                    "eventId": format!("delta-{delta}"),
                    "payload": {
                        "kind": "text_delta",
                        "delta": delta,
                        "assistantMessageId": "m1"
                    }
                }),
            },
        ))));
    }

    #[test]
    fn inherit_wait_text_merges_text_appended_before_the_swap() {
        // Old window "AB" with "A" delivered; the incoming tracker already
        // collected "N" from the runtime sink before the map swap.
        let old = PassiveActivityTracker::new(false);
        append_delta(&old, "A");
        assert_eq!(old.take_wait_tail().text, "A");
        append_delta(&old, "B");
        let new = PassiveActivityTracker::new(false);
        append_delta(&new, "N");

        new.inherit_wait_text(&old);
        assert_eq!(
            new.take_wait_tail(),
            WaitTail {
                text: "BN".into(),
                truncated: false
            },
            "old undelivered text plus the pre-swap append, in stream order"
        );
    }

    #[test]
    fn inherit_wait_text_trims_the_merged_window_to_the_cap() {
        let old = PassiveActivityTracker::new(false);
        // 3000 * 3 = 9000 bytes; append drains to the largest char-aligned
        // suffix <= 8192, i.e. 810 bytes are dropped, leaving 2730 "界" (8190).
        append_delta(&old, &"界".repeat(3000));
        let new = PassiveActivityTracker::new(false);
        append_delta(&new, "尾");

        new.inherit_wait_text(&old);
        // merged = 2730 "界" + "尾" = 8193 bytes; the head is trimmed to the
        // next char boundary (3 bytes = one "界"), leaving 2729 "界" + "尾"
        // (8190 bytes). appended = 9000 + 3 = 9003, delivered = 0, so
        // window_start = 813 > delivered: the take degrades to the whole
        // window with a truncation flag.
        assert_eq!(
            new.take_wait_tail(),
            WaitTail {
                text: format!("{}尾", "界".repeat(2729)),
                truncated: true
            },
            "exactly the last 8190 bytes of the merged stream, no over-trimming"
        );
    }

    #[test]
    fn take_wait_tail_delivers_only_the_new_suffix() {
        let tracker = PassiveActivityTracker::new(false);
        append_delta(&tracker, "hello");
        assert_eq!(
            tracker.take_wait_tail(),
            WaitTail {
                text: "hello".into(),
                truncated: false
            }
        );
        // The second take returns only the appended suffix, with no overlap.
        append_delta(&tracker, " world");
        let second = tracker.take_wait_tail();
        assert_eq!(second.text, " world");
        assert!(!second.text.starts_with("hello"));
        assert!(!second.truncated);
        // Nothing appended: empty suffix, not truncated.
        assert_eq!(
            tracker.take_wait_tail(),
            WaitTail {
                text: String::new(),
                truncated: false
            }
        );
    }

    #[test]
    fn take_wait_tail_rolls_the_window_with_per_call_truncation() {
        // (a) A caller that falls behind the rolling window gets the whole
        // window plus a truncation flag, and the cursor catches up.
        let tracker = PassiveActivityTracker::new(false);
        append_delta(&tracker, "中");
        let first = tracker.take_wait_tail();
        assert_eq!(first.text, "中");
        assert!(!first.truncated);
        append_delta(&tracker, &"🙂中".repeat(3000));
        let degraded = tracker.take_wait_tail();
        assert!(degraded.truncated);
        assert!(
            degraded.text.len() <= MAX_LATEST_TEXT_BYTES
                && degraded.text.len() > MAX_LATEST_TEXT_BYTES - 4,
            "degraded window stays at the 8 KiB cap on a char boundary: {}",
            degraded.text.len()
        );
        // (c) A degraded take followed by no new bytes is empty and false.
        assert_eq!(
            tracker.take_wait_tail(),
            WaitTail {
                text: String::new(),
                truncated: false
            }
        );

        // (b) Delivered up to the full window, then roll it and append a small
        // multibyte delta: only the delta, no truncation.
        let tracker = PassiveActivityTracker::new(false);
        append_delta(&tracker, &"x".repeat(MAX_LATEST_TEXT_BYTES));
        let full = tracker.take_wait_tail();
        assert_eq!(full.text.len(), MAX_LATEST_TEXT_BYTES);
        assert!(!full.truncated);
        append_delta(&tracker, "中🙂");
        assert_eq!(
            tracker.take_wait_tail(),
            WaitTail {
                text: "中🙂".into(),
                truncated: false
            }
        );

        // (c) delivered == window_start exactly is NOT degradation: the whole
        // window is returned without a truncation flag.
        let tracker = PassiveActivityTracker::new(false);
        append_delta(&tracker, &"a".repeat(1808));
        assert_eq!(tracker.take_wait_tail().text.len(), 1808);
        append_delta(&tracker, &"b".repeat(MAX_LATEST_TEXT_BYTES));
        let equal = tracker.take_wait_tail();
        assert_eq!(
            equal,
            WaitTail {
                text: "b".repeat(MAX_LATEST_TEXT_BYTES),
                truncated: false
            }
        );
    }

    #[test]
    fn concurrent_takes_partition_the_appended_stream_exactly_once() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{mpsc, Arc};

        const DELTAS: usize = 200;
        const TOKEN_BYTES: usize = 8;
        const CONSUMERS: usize = 4;
        let tracker = Arc::new(PassiveActivityTracker::new(false));
        let token = |index: usize| format!("d{index:07}");
        let produced: String = (0..DELTAS).map(token).collect();
        assert!(produced.len() < MAX_LATEST_TEXT_BYTES, "no window roll");

        let done = Arc::new(AtomicBool::new(false));
        // Pin a take inside the append phase with a handshake, not a barrier:
        // the appender releases consumer 0 after appending the first token and
        // blocks on the reply before finishing the stream. No other consumer
        // exists yet, so that take cannot be raced away; and a failure is
        // reported by the appender (no peer blocked on a barrier).
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (taken_tx, taken_rx) = mpsc::channel::<(String, bool)>();
        let mut handles = Vec::new();
        {
            let tracker = Arc::clone(&tracker);
            let done = Arc::clone(&done);
            handles.push(std::thread::spawn(move || {
                let mut chunks = Vec::new();
                go_rx.recv().expect("appender must release the pinned consumer");
                let first = tracker.take_wait_tail();
                taken_tx
                    .send((first.text.clone(), first.truncated))
                    .expect("appender must await the pinned take");
                chunks.push(first.text);
                drain_until_done(&tracker, &done, &mut chunks);
                chunks
            }));
        }
        // Append the first token, let the pinned consumer take it, then bring up
        // the remaining consumers and finish the stream.
        append_delta(&tracker, &token(0));
        go_tx.send(()).unwrap();
        let (first_text, first_truncated) = taken_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the pinned consumer must take during the append phase");
        assert!(!first_truncated);
        assert!(
            !first_text.is_empty(),
            "a take must run while appends are still in progress"
        );
        for _ in 1..CONSUMERS {
            let tracker = Arc::clone(&tracker);
            let done = Arc::clone(&done);
            handles.push(std::thread::spawn(move || {
                let mut chunks = Vec::new();
                drain_until_done(&tracker, &done, &mut chunks);
                chunks
            }));
        }
        for index in 1..DELTAS {
            append_delta(&tracker, &token(index));
        }
        done.store(true, Ordering::Release);

        let mut chunks: Vec<String> = Vec::new();
        for handle in handles {
            chunks.extend(handle.join().unwrap());
        }
        // Every chunk is a contiguous token-aligned slice of the stream, so
        // each appears exactly once at a unique offset in `produced`. Sorting
        // by that offset must tile the stream with no gap and no overlap.
        let mut placed: Vec<(usize, String)> = chunks
            .into_iter()
            .map(|chunk| {
                let start = produced
                    .find(&chunk)
                    .unwrap_or_else(|| panic!("chunk {chunk:?} is not in the stream"));
                (start, chunk)
            })
            .collect();
        placed.sort_by_key(|(start, _)| *start);
        let mut reassembled = String::new();
        for (start, chunk) in &placed {
            assert_eq!(
                *start,
                reassembled.len(),
                "chunks must be disjoint and contiguous"
            );
            reassembled.push_str(chunk);
        }
        assert_eq!(reassembled, produced);
        assert_eq!(
            placed.iter().map(|(_, c)| c.len()).sum::<usize>(),
            DELTAS * TOKEN_BYTES
        );
    }

    fn drain_until_done(
        tracker: &PassiveActivityTracker,
        done: &std::sync::atomic::AtomicBool,
        chunks: &mut Vec<String>,
    ) {
        use std::sync::atomic::Ordering;
        loop {
            let tail = tracker.take_wait_tail();
            assert!(!tail.truncated, "the stream never rolls the window");
            if !tail.text.is_empty() {
                chunks.push(tail.text);
            }
            if done.load(Ordering::Acquire) {
                // One final take captures anything appended after the last
                // empty read.
                let tail = tracker.take_wait_tail();
                assert!(!tail.truncated);
                if !tail.text.is_empty() {
                    chunks.push(tail.text);
                }
                break;
            }
            std::thread::yield_now();
        }
    }
}
