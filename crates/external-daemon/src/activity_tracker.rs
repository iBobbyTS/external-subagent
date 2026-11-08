use super::*;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ActivitySampleKind {
    ReasoningDelta,
    TextDelta,
    ToolStarted { kind: PassiveToolKind, count: u64 },
    ToolCompleted,
    ToolFailed,
}
#[derive(Debug, Clone)]
pub(crate) struct ActivitySample {
    pub(crate) observed_at: Instant,
    pub(crate) kind: ActivitySampleKind,
}

#[derive(Debug, Clone)]
pub(crate) struct ActiveTool {
    call_id: String,
    kind: PassiveToolKind,
    started_at: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct RuntimeStateData {
    pub(crate) turn: TurnReduction,
    pub(crate) command_failure: Option<external_contract::events::NativeFailure>,
    revision: u64,
    last_runtime_event_at: Option<(Instant, u64)>,
    pub(crate) active_model_requests: HashMap<String, Option<Instant>>,
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
    pub(crate) active_tools: HashMap<String, ActiveTool>,
    samples: HashMap<String, ActivitySample>,
    sample_order: VecDeque<String>,
    pub(crate) health: runtime_observation::RuntimeObservation,
    typed_messages: HashMap<String, String>,
    typed_message_bytes: usize,
    /// Once a prefix is no longer fully tracked, snapshots cannot safely be
    /// compared with it. Deltas still feed the bounded live tail unchanged.
    typed_messages_limited: bool,
    typed_finished_tools: std::collections::HashSet<String>,
    counted_tool_calls: std::collections::HashSet<String>,
    pub(crate) observation: observation::ObservationState,
}

impl RuntimeState {
    pub(crate) fn new() -> Self {
        let state = RuntimeStateData::default();
        Self {
            state: Mutex::new(state),
            changed: Condvar::new(),
        }
    }

    pub(crate) fn observe(&self, event: &RuntimeEnvelope) {
        self.reduce_envelopes(std::slice::from_ref(event));
        self.notify_reduced();
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
            .map(|(_, tool)| PassiveActiveTool {
                tool_call_id: tool.call_id.clone(),
                kind: tool.kind,
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
                .filter_map(Option::as_ref)
                .min()
                .map(|at| duration_millis(now.saturating_duration_since(*at))),
            model_last_delta_age_ms: state
                .last_model_delta_at
                .map(|at| duration_millis(now.saturating_duration_since(at))),
            latest_text_tail: state.latest_text_tail.clone(),
            latest_text_updated_at: state.latest_text_updated_at,
            latest_text_truncated: state.latest_text_truncated,
            active_tools,
            oldest_active_tool_age_ms: state
                .active_tools
                .values()
                .filter_map(|tool| {
                    tool.started_at
                        .map(|at| duration_millis(now.saturating_duration_since(at)))
                })
                .max(),
            window_60s: window,
            telemetry_degraded: state.health.status() == TelemetryStatus::Degraded,
            telemetry_status: state.health.status(),
            coverage: state.health.coverage.clone(),
            diagnostics: state.health.diagnostics.clone(),
            model_activity: state.health.model_activity.clone(),
            diagnostics_truncated: state.health.diagnostics_truncated,
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
        let tail = Self::wait_tail_locked(&state);
        state.delivered_bytes = state.appended_bytes;
        tail
    }

    /// Read-only variant of [`take_wait_tail`]: returns the same
    /// undelivered suffix without advancing the delivery cursor. List-mode
    /// waits peek during their scan rounds and only consume (take) once the
    /// whole response is certain to be delivered, so an interrupted or
    /// cancelled multi-agent wait never loses activity text.
    pub(crate) fn peek_wait_tail(&self) -> WaitTail {
        let state = self.state.lock().unwrap();
        Self::wait_tail_locked(&state)
    }

    fn wait_tail_locked(state: &RuntimeStateData) -> WaitTail {
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
    /// 工具观测历史与文本流一样跨 claim 保留：旧 tracker 的 ObservationState
    /// 作为基底，新 tracker 在替换前已采集的调用按序重放（身份去重）。
    /// terminal text 仍只属于当前 turn。
    pub(crate) fn inherit_wait_text(&self, old: &RuntimeState) {
        let (old_tail, old_appended, old_delivered, old_updated, old_observation, old_health) = {
            let old_state = old.state.lock().unwrap();
            (
                old_state.latest_text_tail.clone(),
                old_state.appended_bytes,
                old_state.delivered_bytes,
                old_state.latest_text_updated_at,
                // Clone instead of take: an in-flight reader may still hold
                // this outgoing tracker's Arc until the map swap, and must
                // keep seeing its real observation state.
                old_state.observation.clone(),
                old_state.health.clone(),
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
        let newer_observation = std::mem::replace(&mut state.observation, old_observation);
        state.observation.merge_newer(newer_observation);
        state.health.inherit_history(&old_health);
        let tools = state
            .health
            .coverage
            .complete(external_contract::events::CoverageDimension::Tools);
        let reasoning = state
            .health
            .coverage
            .complete(external_contract::events::CoverageDimension::PublicReasoning);
        state.observation.set_completeness(tools, reasoning);
    }

    pub(crate) fn observation_snapshot(&self) -> observation::ObservationSnapshot {
        self.state.lock().unwrap().observation.snapshot()
    }

    #[cfg(test)]
    pub(crate) fn set_wait_fixture(&self, now: Instant) {
        let mut state = self.state.lock().unwrap();
        state.revision = 900;
        state.latest_text_tail = "ordinary text".into();
        state.appended_bytes = state.latest_text_tail.len() as u64;
        state.delivered_bytes = 0;
        state.last_model_delta_at = Some(now);
        state.active_tools.insert(
            "tool".into(),
            ActiveTool {
                call_id: "tool".into(),
                kind: PassiveToolKind::Bash,
                started_at: Some(now),
            },
        );
        state.samples.insert(
            "tool".into(),
            ActivitySample {
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

pub(crate) fn activity_wall_now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn append_latest_text(state: &mut RuntimeStateData, delta: &str, wall_now_ms: u64) {
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

    fn typed_text(tracker: &RuntimeState, id: &str, text: external_contract::events::TextUpdate) {
        use external_contract::events::*;
        typed_fact(
            tracker,
            EventFact::MessageUpdated(MessageUpdate {
                message_id: MessageId(id.into()),
                text,
            }),
        );
    }

    fn typed_fact(tracker: &RuntimeState, fact: external_contract::events::EventFact) {
        use external_contract::events::*;
        let sequence = tracker.state.lock().unwrap().revision + 1;
        tracker.observe(&RuntimeEnvelope::from(NormalizedEvent {
            context: EventContext {
                task_id: TaskId("task".into()),
                launch_id: LaunchId("launch".into()),
                sequence,
                turn_id: Some(TurnId::Local(1)),
                source: NativeSource {
                    protocol: SourceProtocol::DshAcpV1,
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
        }));
    }

    #[test]
    fn repair_typed_message_cache_is_bounded_while_live_deltas_continue() {
        use external_contract::events::TextUpdate;
        const BOUND: usize = external_agent_dsh::acp::result::MAX_MESSAGE_BYTES;
        let tracker = RuntimeState::new();
        let chunk = "x".repeat(1024 * 1024);
        // 32 MiB of legal chunks, sixteen times the aggregate retention cap.
        for _ in 0..32 {
            typed_text(&tracker, "m", TextUpdate::Delta(chunk.clone()));
            let state = tracker.state.lock().unwrap();
            assert!(
                state
                    .typed_messages
                    .values()
                    .map(String::len)
                    .sum::<usize>()
                    <= BOUND
            );
        }
        let (ids, bytes) = {
            let state = tracker.state.lock().unwrap();
            (
                state.typed_messages.len(),
                state
                    .typed_messages
                    .values()
                    .map(String::len)
                    .sum::<usize>(),
            )
        };
        assert!(bytes <= BOUND, "retained {bytes} message bytes");
        assert!(ids <= 128);
        typed_text(&tracker, "m", TextUpdate::Delta("live end".into()));
        let tail = tracker.peek_wait_tail();
        assert!(tail.truncated);
        assert!(tail.text.ends_with("live end"));
        assert!(tail.text.len() <= MAX_LATEST_TEXT_BYTES);

        let tracker = RuntimeState::new();
        for n in 0..1024 {
            typed_text(&tracker, &format!("m{n}"), TextUpdate::Delta("x".into()));
        }
        assert!(tracker.state.lock().unwrap().typed_messages.len() <= 128);
    }

    #[test]
    fn repair_typed_snapshot_bookkeeping_is_bounded_and_resets_with_the_turn() {
        use external_contract::events::*;
        let tracker = RuntimeState::new();
        typed_text(&tracker, "m1", TextUpdate::Delta("A".into()));
        typed_text(&tracker, "m1", TextUpdate::Snapshot("AB".into()));
        typed_text(&tracker, "m1", TextUpdate::Snapshot("AB".into()));
        assert_eq!(tracker.take_wait_tail().text, "AB");
        // The aggregate cap applies across identities, including snapshots.
        let large = "é".repeat(MAX_TYPED_MESSAGE_BYTES / 2 - 1);
        typed_text(&tracker, "m2", TextUpdate::Snapshot(large.clone()));
        typed_text(&tracker, "m3", TextUpdate::Snapshot("overflow".into()));
        {
            let state = tracker.state.lock().unwrap();
            assert_eq!(state.typed_message_bytes, MAX_TYPED_MESSAGE_BYTES);
            assert_eq!(
                state
                    .typed_messages
                    .values()
                    .map(String::len)
                    .sum::<usize>(),
                MAX_TYPED_MESSAGE_BYTES
            );
            assert!(state.typed_messages_limited);
        }
        tracker.take_wait_tail();
        // Once tracking loses a prefix, snapshots must not replay that prefix
        // as new text. Explicit deltas continue through the live tail.
        typed_text(
            &tracker,
            "m2",
            TextUpdate::Snapshot(format!("{large}suffix")),
        );
        assert!(tracker.take_wait_tail().text.is_empty());
        typed_text(&tracker, "m2", TextUpdate::Delta("live".into()));
        assert_eq!(tracker.take_wait_tail().text, "live");
        typed_fact(
            &tracker,
            EventFact::TurnStarted {
                turn_id: TurnId::Local(2),
            },
        );
        {
            let state = tracker.state.lock().unwrap();
            assert_eq!(state.typed_message_bytes, 0);
            assert!(!state.typed_messages_limited);
            assert!(state.typed_messages.is_empty());
        }
        typed_text(&tracker, "m1", TextUpdate::Delta("C".into()));
        typed_text(&tracker, "m1", TextUpdate::Snapshot("CD".into()));
        assert_eq!(tracker.take_wait_tail().text, "CD");

        let tracker = RuntimeState::new();
        typed_text(
            &tracker,
            "m",
            TextUpdate::Snapshot("x".repeat(MAX_TYPED_MESSAGE_BYTES * 4)),
        );
        assert!(tracker.state.lock().unwrap().typed_messages.is_empty());
        assert_eq!(tracker.peek_wait_tail().text.len(), MAX_LATEST_TEXT_BYTES);
    }

    #[test]
    fn active_tool_retention_loss_is_atomic_and_scoped_to_its_terminal() {
        use external_contract::events::*;
        let tracker = RuntimeState::new();
        typed_fact(
            &tracker,
            EventFact::TurnStarted {
                turn_id: TurnId::Local(1),
            },
        );
        // Synthetic occupancy isolates the common state's retention boundary.
        {
            let mut state = tracker.state.lock().unwrap();
            for index in 0..MAX_ACTIVITY_IDENTITIES {
                state.active_tools.insert(
                    index.to_string(),
                    ActiveTool {
                        call_id: index.to_string(),
                        kind: PassiveToolKind::Other,
                        started_at: None,
                    },
                );
            }
        }
        let change = |id: &str, phase| {
            EventFact::ToolChanged(ToolChange {
                call_id: CallId(id.into()),
                name: Some("Bash".into()),
                category: None,
                arguments: None,
                phase: Some(phase),
                outcome: None,
                failure: None,
                denied: None,
            })
        };
        typed_fact(&tracker, change("over-capacity", ToolPhase::Started));
        tracker.state.lock().unwrap().active_tools.clear();
        assert_eq!(
            tracker.snapshot().telemetry_status,
            TelemetryStatus::Degraded
        );
        assert!(
            !tracker
                .observation_snapshot()
                .coverage
                .tool_history_complete,
            "the same reduction exposes retention loss in the public coverage projection"
        );
        typed_fact(&tracker, change("unrelated", ToolPhase::Finished));
        assert_eq!(
            tracker.snapshot().telemetry_status,
            TelemetryStatus::Degraded
        );
        typed_fact(&tracker, change("over-capacity", ToolPhase::Finished));
        assert_eq!(
            tracker.snapshot().telemetry_status,
            TelemetryStatus::Healthy
        );
        assert!(
            !tracker
                .observation_snapshot()
                .coverage
                .tool_history_complete
        );
    }

    fn append_delta(tracker: &RuntimeState, delta: &str) {
        typed_text(
            tracker,
            "m1",
            external_contract::events::TextUpdate::Delta(delta.into()),
        );
    }

    #[test]
    fn inherit_wait_text_merges_text_appended_before_the_swap() {
        // Old window "AB" with "A" delivered; the incoming tracker already
        // collected "N" from the runtime sink before the map swap.
        let old = RuntimeState::new();
        append_delta(&old, "A");
        assert_eq!(old.take_wait_tail().text, "A");
        append_delta(&old, "B");
        let new = RuntimeState::new();
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
        let old = RuntimeState::new();
        // 3000 * 3 = 9000 bytes; append drains to the largest char-aligned
        // suffix <= 8192, i.e. 810 bytes are dropped, leaving 2730 "界" (8190).
        append_delta(&old, &"界".repeat(3000));
        let new = RuntimeState::new();
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
        let tracker = RuntimeState::new();
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
        let tracker = RuntimeState::new();
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
        let tracker = RuntimeState::new();
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
        let tracker = RuntimeState::new();
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
        let tracker = Arc::new(RuntimeState::new());
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
                go_rx
                    .recv()
                    .expect("appender must release the pinned consumer");
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
        tracker: &RuntimeState,
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

// Match the DSH aggregation family: 2 MiB total text, hence also at most
// 2 MiB per message, and 128 bounded native identities per open turn.
const MAX_TYPED_MESSAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_TYPED_MESSAGE_IDS: usize = 128;

fn typed_message_delta<'a>(
    state: &mut RuntimeStateData,
    message: &'a external_contract::events::MessageUpdate,
) -> &'a str {
    use external_contract::events::TextUpdate;
    let id = &message.message_id.0;
    let is_delta = matches!(message.text, TextUpdate::Delta(_));
    let text = match &message.text {
        TextUpdate::Delta(text) | TextUpdate::Snapshot(text) => text.as_str(),
    };
    if state.typed_messages_limited {
        return if is_delta { text } else { "" };
    }
    if id.is_empty()
        || id.len() > 512
        || id.contains('\0')
        || (!state.typed_messages.contains_key(id)
            && state.typed_messages.len() >= MAX_TYPED_MESSAGE_IDS)
    {
        state.typed_messages_limited = true;
        return if is_delta { text } else { "" };
    }
    let previous = state
        .typed_messages
        .get(id)
        .map(String::as_str)
        .unwrap_or("");
    let delta = if is_delta {
        text
    } else {
        text.strip_prefix(previous).unwrap_or("")
    };
    let retained_bytes = if is_delta {
        state.typed_message_bytes.saturating_add(text.len())
    } else {
        state.typed_message_bytes - previous.len() + text.len()
    };
    if retained_bytes > MAX_TYPED_MESSAGE_BYTES {
        // Keep the known prefix bounded; never compare a later snapshot to
        // an incomplete prefix and fabricate a suffix from missing history.
        state.typed_messages_limited = true;
    } else {
        let previous = state.typed_messages.entry(id.clone()).or_default();
        if is_delta {
            previous.push_str(text);
        } else {
            *previous = text.to_owned();
        }
        state.typed_message_bytes = retained_bytes;
    }
    delta
}

impl RuntimeState {
    pub(crate) fn reduce_typed(
        mut state: &mut RuntimeStateData,
        event: &external_contract::events::NormalizedEvent,
        now: Instant,
        wall_now_ms: u64,
    ) {
        use external_contract::events::*;
        if state.turn.active
            && matches!(state.turn.turn_id, Some(TurnId::Local(_)))
            && matches!(event.context.turn_id, Some(TurnId::Native(_)))
        {
            let mut old_event = event.clone();
            old_event.context.turn_id = state.turn.turn_id.clone();
            let old = runtime_observation::turn_key(&old_event);
            let new = runtime_observation::turn_key(event);
            let prefix = format!("{old}\0");
            let models = std::mem::take(&mut state.active_model_requests);
            for (key, value) in models {
                let key = key
                    .strip_prefix(&prefix)
                    .map(|suffix| format!("{new}\0{suffix}"))
                    .unwrap_or(key);
                state.active_model_requests.insert(key, value);
            }
            let tools = std::mem::take(&mut state.active_tools);
            for (key, value) in tools {
                let key = key
                    .strip_prefix(&prefix)
                    .map(|suffix| format!("{new}\0{suffix}"))
                    .unwrap_or(key);
                state.active_tools.insert(key, value);
            }
            let finished = std::mem::take(&mut state.typed_finished_tools);
            state.typed_finished_tools = finished
                .into_iter()
                .map(|key| {
                    key.strip_prefix(&prefix)
                        .map(|suffix| format!("{new}\0{suffix}"))
                        .unwrap_or(key)
                })
                .collect();
            let counted = std::mem::take(&mut state.counted_tool_calls);
            state.counted_tool_calls = counted
                .into_iter()
                .map(|key| {
                    key.strip_prefix(&prefix)
                        .map(|suffix| format!("{new}\0{suffix}"))
                        .unwrap_or(key)
                })
                .collect();
            state.health.promote_turn(&old, &new);
            state.observation.promote_turn(&old, &new);
        }
        Self::reduce_turn(&mut state.turn, event, now);
        state.revision = state.revision.saturating_add(1);
        state.last_runtime_event_at = Some((now, wall_now_ms));
        state.health.observe(event);
        state.observation.observe_fact(event);
        for dimension in state.observation.projection_losses() {
            state
                .health
                .projection_loss(dimension, event.context.source.protocol, wall_now_ms);
        }
        let turn = runtime_observation::turn_key(event);
        let mut sample = None;
        match &event.fact {
            EventFact::TurnStarted { .. } => {
                state.terminal_text.clear();
                state.terminal_text_settled = false;
                state.typed_messages.clear();
                state.typed_message_bytes = 0;
                state.typed_messages_limited = false;
            }
            EventFact::TurnSettled(settlement) => {
                if let Some(text) = &settlement.final_text {
                    state.terminal_text = text.clone();
                }
                state.terminal_text_settled = true;
                let prefix = format!("{turn}\0");
                state
                    .active_model_requests
                    .retain(|key, _| !key.starts_with(&prefix));
                state
                    .active_tools
                    .retain(|key, _| !key.starts_with(&prefix));
            }
            EventFact::MessageUpdated(message) if !state.terminal_text_settled => {
                let delta = typed_message_delta(&mut state, message);
                if !delta.is_empty() {
                    append_latest_text(&mut state, delta, wall_now_ms);
                    state.last_model_delta_at = Some(now);
                    sample = Some((
                        format!("text:{}", event.context.sequence),
                        ActivitySampleKind::TextDelta,
                    ));
                }
            }
            EventFact::PublicReasoningDelta(_) => {
                state.last_model_delta_at = Some(now);
                sample = Some((
                    format!("reasoning:{}", event.context.sequence),
                    ActivitySampleKind::ReasoningDelta,
                ));
            }
            EventFact::ModelActivityChanged(model) => {
                if let Some(request) = &model.request_id {
                    let key = runtime_observation::correlation_key(event, &request.0);
                    match model.phase {
                        ModelPhase::Started => {
                            if state.active_model_requests.len() < MAX_ACTIVITY_IDENTITIES
                                || state.active_model_requests.contains_key(&key)
                            {
                                state
                                    .active_model_requests
                                    .entry(key)
                                    .or_insert(None)
                                    .get_or_insert(now);
                            } else {
                                state.health.correlation_loss(
                                    event,
                                    CoverageDimension::ModelActivity,
                                    ActivityScope::ModelRequest(request.clone()),
                                    wall_now_ms,
                                );
                            }
                        }
                        ModelPhase::StreamStalled => {
                            if state.active_model_requests.len() < MAX_ACTIVITY_IDENTITIES
                                || state.active_model_requests.contains_key(&key)
                            {
                                state.active_model_requests.entry(key).or_insert(None);
                            } else {
                                state.health.correlation_loss(
                                    event,
                                    CoverageDimension::ModelActivity,
                                    ActivityScope::ModelRequest(request.clone()),
                                    wall_now_ms,
                                );
                            }
                        }
                        ModelPhase::Completed | ModelPhase::Failed | ModelPhase::RetryScheduled => {
                            state.active_model_requests.remove(&key);
                        }
                    }
                }
            }
            EventFact::ToolChanged(tool) => {
                let key = runtime_observation::correlation_key(event, &tool.call_id.0);
                let kind = match tool.name.as_deref() {
                    Some("Read" | "read") => PassiveToolKind::Read,
                    Some("Bash" | "bash") => PassiveToolKind::Bash,
                    _ => PassiveToolKind::Other,
                };
                if matches!(tool.phase, Some(ToolPhase::Started | ToolPhase::Finished))
                    && !state.counted_tool_calls.contains(&key)
                {
                    if state.counted_tool_calls.len() < MAX_ACTIVITY_IDENTITIES {
                        state.counted_tool_calls.insert(key.clone());
                        let sample_key = format!("{key}:started");
                        state.sample_order.push_back(sample_key.clone());
                        state.samples.insert(
                            sample_key,
                            ActivitySample {
                                observed_at: now,
                                kind: ActivitySampleKind::ToolStarted { kind, count: 1 },
                            },
                        );
                    } else {
                        state.health.projection_loss(
                            CoverageDimension::Tools,
                            event.context.source.protocol,
                            wall_now_ms,
                        );
                    }
                }
                match tool.phase {
                    Some(ToolPhase::Scheduled | ToolPhase::Started) => {
                        if !state.typed_finished_tools.contains(&key)
                            && tool.call_id.0.len() <= MAX_ACTIVITY_ID_BYTES
                            && (state.active_tools.len() < MAX_ACTIVITY_IDENTITIES
                                || state.active_tools.contains_key(&key))
                        {
                            state.active_tools.entry(key.clone()).or_insert(ActiveTool {
                                call_id: tool.call_id.0.clone(),
                                kind,
                                started_at: None,
                            });
                            if tool.phase == Some(ToolPhase::Started) {
                                state
                                    .active_tools
                                    .get_mut(&key)
                                    .expect("tool")
                                    .started_at
                                    .get_or_insert(now);
                            }
                        } else if !state.typed_finished_tools.contains(&key) {
                            state.health.correlation_loss(
                                event,
                                CoverageDimension::Tools,
                                ActivityScope::Tool(tool.call_id.clone()),
                                wall_now_ms,
                            );
                        }
                    }
                    Some(ToolPhase::Finished) => {
                        state.active_tools.remove(&key);
                        if state.typed_finished_tools.len() < MAX_ACTIVITY_IDENTITIES {
                            state.typed_finished_tools.insert(key.clone());
                        }
                        sample = Some((
                            format!("{key}:finished"),
                            if tool.outcome == Some(ToolOutcome::Failed) {
                                ActivitySampleKind::ToolFailed
                            } else {
                                ActivitySampleKind::ToolCompleted
                            },
                        ));
                    }
                    _ => {} // progress/permission denial do not end execution
                }
            }
            _ => {}
        }
        if let Some((key, kind)) = sample {
            if !state.samples.contains_key(&key) {
                state.sample_order.push_back(key.clone());
                state.samples.insert(
                    key,
                    ActivitySample {
                        observed_at: now,
                        kind,
                    },
                );
            }
        }
        while state.sample_order.len() > MAX_ACTIVITY_IDENTITIES {
            if let Some(key) = state.sample_order.pop_front() {
                state.samples.remove(&key);
            }
        }
        state.observation.set_completeness(
            state
                .health
                .coverage
                .complete(external_contract::events::CoverageDimension::Tools),
            state
                .health
                .coverage
                .complete(external_contract::events::CoverageDimension::PublicReasoning),
        );
    }
}
