use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, VecDeque};

pub const OBSERVATION_SCHEMA: &str = "external-subagent-observation/1.1";
const MAX_IDENTITIES: usize = 65_536;
const MAX_TOOL_TYPES: usize = 64;
const MAX_RECENT_CALLS: usize = 5;
const MAX_REASONING_CHARS: usize = 200;
const MAX_REASONING_BYTES: usize = 64 * 1024;
const MAX_ID_BYTES: usize = 512;
const MAX_ARGUMENT_BYTES: usize = 4 * 1024;
// The serialized argument JSON is embedded as one JSON string. Quotes and
// backslashes can at most double this already-escaped prefix, leaving room for
// the projection key and object envelope below the 4 KiB public cap.
const MAX_ARGUMENT_PREVIEW_BYTES: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationSnapshot {
    pub snapshot_seq: u64,
    pub tools: Vec<ObservedTool>,
    pub reasoning: ObservedReasoning,
    pub coverage: ObservationCoverage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedTool {
    pub tool_name: String,
    pub call_count: u64,
    pub recent_calls: Vec<ObservedCall>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedCall {
    /// Retained typed outcome; the existing public serialization remains compatible.
    #[serde(skip)]
    pub status: ObservedCallStatus,
    pub seq: u64,
    pub tool_call_id: String,
    pub arguments: Map<String, Value>,
    pub arguments_truncated: bool,
    pub redacted_fields: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ObservedCallStatus {
    #[default]
    Unknown,
    Pending,
    Running,
    Finished,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedReasoning {
    /// Protocol/channel evidence independent of the legacy wire representation.
    #[serde(skip)]
    pub native_source: Option<PublicReasoningSource>,
    pub text: String,
    pub truncated: bool,
    pub source: ReasoningSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicReasoningSource {
    pub protocol: external_contract::events::SourceProtocol,
    pub channel: external_contract::events::PublicReasoningChannel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningSource {
    pub status: String,
    pub runtime_version: String,
    pub event_type: String,
    pub delta_pointer: String,
}

impl ReasoningSource {
    pub fn legacy_session_projection() -> Self {
        Self {
            status: "PUBLIC_RUNTIME_EVENT".into(),
            runtime_version: "unreported".into(),
            event_type: "model.streaming".into(),
            delta_pointer: "/params/payload/delta".into(),
        }
    }

    /// DSH's public ACP thought contract.
    pub fn legacy_acp_projection() -> Self {
        Self {
            status: "VERIFIED_PROTOCOL_PUBLIC".into(),
            runtime_version: "acp/1".into(),
            event_type: "agent_thought_chunk".into(),
            delta_pointer: "/params/update/content/text".into(),
        }
    }

    pub fn is_supported_public(&self) -> bool {
        *self == Self::legacy_session_projection() || *self == Self::legacy_acp_projection()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationCoverage {
    pub tool_history_complete: bool,
    pub reasoning_complete: bool,
    pub dropped_events: u64,
}

impl ObservationSnapshot {
    pub fn unavailable() -> Self {
        Self {
            snapshot_seq: 0,
            tools: Vec::new(),
            reasoning: ObservedReasoning {
                native_source: None,
                text: String::new(),
                truncated: false,
                source: ReasoningSource::legacy_session_projection(),
            },
            coverage: ObservationCoverage {
                tool_history_complete: false,
                reasoning_complete: false,
                dropped_events: 0,
            },
        }
    }
}

#[derive(Default, Clone)]
struct ToolGroup {
    call_count: u64,
    latest_seq: u64,
    recent_calls: VecDeque<ObservedCall>,
}

#[derive(Clone)]
pub struct ObservationState {
    reasoning_source: ReasoningSource,
    native_reasoning_source: Option<PublicReasoningSource>,
    snapshot_seq: u64,
    next_call_seq: u64,
    seen_calls: HashMap<String, (String, u64)>,
    tools: HashMap<String, ToolGroup>,
    reasoning_raw: String,
    reasoning_text: String,
    reasoning_truncated: bool,
    tool_history_complete: bool,
    reasoning_complete: bool,
    dropped_events: u64,
    tool_retention_gap: bool,
    reasoning_retention_gap: bool,
    argument_fragments: HashMap<String, String>,
    limited_fragments: std::collections::HashSet<String>,
}

impl Default for ObservationState {
    fn default() -> Self {
        Self {
            reasoning_source: ReasoningSource::legacy_session_projection(),
            native_reasoning_source: None,
            snapshot_seq: 0,
            next_call_seq: 0,
            seen_calls: HashMap::new(),
            tools: HashMap::new(),
            reasoning_raw: String::new(),
            reasoning_text: String::new(),
            reasoning_truncated: false,
            tool_history_complete: false,
            reasoning_complete: false,
            dropped_events: 0,
            tool_retention_gap: false,
            reasoning_retention_gap: false,
            argument_fragments: HashMap::new(),
            limited_fragments: std::collections::HashSet::new(),
        }
    }
}

impl ObservationState {
    fn observe_reasoning(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let mut combined = String::with_capacity(self.reasoning_raw.len() + delta.len());
        combined.push_str(&self.reasoning_raw);
        combined.push_str(delta);
        if combined.len() > MAX_REASONING_BYTES {
            let mut start = combined.len() - MAX_REASONING_BYTES;
            while start < combined.len() && !combined.is_char_boundary(start) {
                start += 1;
            }
            self.reasoning_raw = combined[start..].to_owned();
            self.reasoning_truncated = true;
            self.reasoning_complete = false;
            self.reasoning_retention_gap = true;
            self.dropped_events = self.dropped_events.saturating_add(1);
        } else {
            self.reasoning_raw = combined;
        }
        self.reasoning_text = reasoning_tail(&self.reasoning_raw);
        self.reasoning_truncated |= self.reasoning_raw.chars().count() > MAX_REASONING_CHARS;
    }

    fn record_tool_call(
        &mut self,
        turn_id: &str,
        call_id: &str,
        tool_name: &str,
        arguments: Map<String, Value>,
        arguments_truncated: bool,
        redacted_fields: u64,
    ) {
        // Single source of the cross-path call identity: every producer and
        // the replacement merge feed this path, so the same logical call can
        // never be counted under two shapes.
        let identity = format!("{turn_id}\0{call_id}");
        if let Some((original_name, seq)) = self.seen_calls.get(&identity).cloned() {
            if let Some(call) = self
                .tools
                .get_mut(&original_name)
                .and_then(|group| group.recent_calls.iter_mut().find(|call| call.seq == seq))
            {
                // An input-absent update (unknown arguments) must never erase
                // known non-empty arguments of the same call. Known→known
                // refinements and unknown-creates-first-record stay allowed.
                let unknown_downgrade =
                    arguments.is_empty() && arguments_truncated && !call.arguments.is_empty();
                if !unknown_downgrade
                    && (call.arguments != arguments
                        || call.arguments_truncated != arguments_truncated
                        || call.redacted_fields != redacted_fields)
                {
                    call.arguments = arguments;
                    call.arguments_truncated = arguments_truncated;
                    call.redacted_fields = redacted_fields;
                    self.bump_snapshot();
                }
            }
            return;
        }
        if self.seen_calls.len() >= MAX_IDENTITIES {
            self.drop_event(true);
            return;
        }
        if !self.tools.contains_key(tool_name) && self.tools.len() >= MAX_TOOL_TYPES {
            self.drop_event(true);
            return;
        }
        self.next_call_seq = self.next_call_seq.saturating_add(1);
        let seq = self.next_call_seq;
        self.seen_calls
            .insert(identity, (tool_name.to_owned(), seq));
        let group = self.tools.entry(tool_name.to_owned()).or_default();
        group.call_count = group.call_count.saturating_add(1);
        group.latest_seq = seq;
        group.recent_calls.push_front(ObservedCall {
            seq,
            tool_call_id: call_id.to_owned(),
            arguments,
            arguments_truncated,
            redacted_fields,
            status: ObservedCallStatus::Unknown,
        });
        group.recent_calls.truncate(MAX_RECENT_CALLS);
        self.bump_snapshot();
    }

    /// Fold a tracker's pre-replacement observation history into this one.
    /// The receiver keeps its own (older) history as the base; the incoming
    /// calls are replayed in receipt order through [`Self::record_tool_call`].
    /// A call identity already present in the base is a replay or a
    /// pathological id reuse, so the base's captured arguments win and the
    /// count stays deduplicated. The merged state adopts the incoming launch
    /// scope: inherited replay keys keep the old launch's namespace, while
    /// fresh events from the new launch are compared in its own.
    /// Coverage stays honest: incomplete wins and dropped counts add up.
    pub(crate) fn merge_newer(&mut self, newer: ObservationState) {
        if !newer.reasoning_raw.is_empty() {
            self.native_reasoning_source = newer.native_reasoning_source.clone();
            self.reasoning_source = newer.reasoning_source.clone();
            // The base's raw stream is older than the incoming tracker's
            // bytes; appending applies the same rolling cap and tail.
            self.observe_reasoning(&newer.reasoning_raw);
        }
        self.reasoning_truncated |= newer.reasoning_truncated;
        self.tool_retention_gap |= newer.tool_retention_gap;
        self.reasoning_retention_gap |= newer.reasoning_retention_gap;
        self.reasoning_complete = self.reasoning_complete && newer.reasoning_complete;
        self.tool_history_complete = self.tool_history_complete && newer.tool_history_complete;
        self.dropped_events = self.dropped_events.saturating_add(newer.dropped_events);

        let mut calls = newer
            .seen_calls
            .iter()
            .map(|(identity, (tool_name, seq))| (*seq, identity, tool_name))
            .collect::<Vec<_>>();
        calls.sort_by_key(|(seq, _, _)| *seq);
        for (seq, identity, tool_name) in calls {
            if self.seen_calls.contains_key(identity) {
                // Already counted in the base: never replace its arguments
                // with the incoming (possibly stale/replayed) record.
                continue;
            }
            // The identity format has a single NUL separator and both sides
            // are NUL-free by validation.
            let (turn_id, call_id) = identity.split_once('\0').unwrap_or(("", identity.as_str()));
            let detail = newer
                .tools
                .get(tool_name)
                .and_then(|group| group.recent_calls.iter().find(|call| call.seq == seq));
            let (arguments, arguments_truncated, redacted_fields) = match detail {
                Some(call) => (
                    call.arguments.clone(),
                    call.arguments_truncated,
                    call.redacted_fields,
                ),
                // A call older than the incoming tracker's bounded recent
                // window has lost its arguments; keep the count and mark the
                // detail unknown. It is inserted before the retained recent
                // calls, so the bounded window still ends with the newest
                // calls that do carry detail.
                None => (Map::new(), true, 0),
            };
            self.record_tool_call(
                turn_id,
                call_id,
                tool_name,
                arguments,
                arguments_truncated,
                redacted_fields,
            );
            if let Some(old) = detail {
                if let Some((name, new_seq)) = self.seen_calls.get(identity).cloned() {
                    if let Some(call) = self.tools.get_mut(&name).and_then(|group| {
                        group
                            .recent_calls
                            .iter_mut()
                            .find(|call| call.seq == new_seq)
                    }) {
                        call.status = old.status;
                    }
                }
            }
        }

        self.snapshot_seq = self.snapshot_seq.saturating_add(newer.snapshot_seq);
    }

    fn drop_event(&mut self, tool: bool) {
        if tool {
            self.tool_history_complete = false;
            self.tool_retention_gap = true;
        } else {
            self.reasoning_complete = false;
            self.reasoning_retention_gap = true;
        }
        self.dropped_events = self.dropped_events.saturating_add(1);
        self.bump_snapshot();
    }

    pub(crate) fn set_completeness(&mut self, tools: bool, reasoning: bool) {
        self.tool_history_complete = tools && !self.tool_retention_gap;
        self.reasoning_complete = reasoning && !self.reasoning_retention_gap;
    }

    pub(crate) fn observe_gap(&mut self, impact: &external_contract::events::Impact, lost: bool) {
        use external_contract::events::*;
        if let Impact::Known(dimensions) = impact {
            if dimensions.contains(&CoverageDimension::Tools) {
                self.tool_history_complete = false;
            }
            if dimensions.contains(&CoverageDimension::PublicReasoning) {
                self.reasoning_complete = false;
            }
        }
        if lost {
            self.dropped_events = self.dropped_events.saturating_add(1);
        }
        self.bump_snapshot();
    }

    fn bump_snapshot(&mut self) {
        self.snapshot_seq = self.snapshot_seq.saturating_add(1);
    }

    pub fn snapshot(&self) -> ObservationSnapshot {
        let mut ranked = self.tools.iter().collect::<Vec<_>>();
        ranked.sort_by(|(left_name, left), (right_name, right)| {
            right
                .call_count
                .cmp(&left.call_count)
                .then_with(|| right.latest_seq.cmp(&left.latest_seq))
                .then_with(|| left_name.cmp(right_name))
        });
        let tools = ranked
            .into_iter()
            .take(3)
            .map(|(name, group)| ObservedTool {
                tool_name: name.clone(),
                call_count: group.call_count,
                recent_calls: group.recent_calls.iter().cloned().collect(),
            })
            .collect();
        let text = self.reasoning_text.clone();
        ObservationSnapshot {
            snapshot_seq: self.snapshot_seq,
            tools,
            reasoning: ObservedReasoning {
                native_source: self.native_reasoning_source.clone(),
                text,
                truncated: self.reasoning_truncated,
                source: self.reasoning_source.clone(),
            },
            coverage: ObservationCoverage {
                tool_history_complete: self.tool_history_complete,
                reasoning_complete: self.reasoning_complete,
                dropped_events: self.dropped_events,
            },
        }
    }
}

fn reasoning_tail(value: &str) -> String {
    value
        .chars()
        .rev()
        .take(MAX_REASONING_CHARS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn exclude_protocol_fields(value: &Value, excluded: &mut u64) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter_map(|(key, value)| {
                    if key == "encrypted_content" {
                        *excluded = excluded.saturating_add(1);
                        None
                    } else {
                        Some((key.clone(), exclude_protocol_fields(value, excluded)))
                    }
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| exclude_protocol_fields(value, excluded))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn project_arguments(input: Option<&Value>) -> (Map<String, Value>, bool, u64) {
    let mut excluded_fields: u64 = 0;
    let arguments = input
        .and_then(Value::as_object)
        .map(|object| exclude_protocol_fields(&Value::Object(object.clone()), &mut excluded_fields))
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let serialized = serde_json::to_string(&arguments).unwrap_or_default();
    if serialized.len() <= MAX_ARGUMENT_BYTES {
        return (arguments, false, excluded_fields);
    }
    let mut preview_end = serialized.len().min(MAX_ARGUMENT_PREVIEW_BYTES);
    while !serialized.is_char_boundary(preview_end) {
        preview_end -= 1;
    }
    let mut projected = Map::new();
    projected.insert(
        "_truncated_preview".into(),
        Value::String(serialized[..preview_end].to_owned()),
    );
    if serde_json::to_vec(&projected).map_or(true, |encoded| encoded.len() > MAX_ARGUMENT_BYTES) {
        projected.clear();
    }
    (projected, true, excluded_fields)
}

impl ObservationState {
    /// Typed facts already passed the native adapter's replay/shape checks.
    pub(crate) fn observe_fact(&mut self, event: &external_contract::events::NormalizedEvent) {
        use external_contract::events::*;
        match &event.fact {
            EventFact::PublicReasoningDelta(reasoning) => {
                self.native_reasoning_source = Some(PublicReasoningSource {
                    protocol: event.context.source.protocol,
                    channel: reasoning.channel,
                });
                self.reasoning_source = match event.context.source.protocol {
                    SourceProtocol::DshAcpV1 => ReasoningSource::legacy_acp_projection(),
                    _ => ReasoningSource::legacy_session_projection(),
                };
                self.observe_reasoning(&reasoning.delta);
                self.bump_snapshot();
            }
            EventFact::ToolChanged(tool) => self.observe_tool_change(event, tool),
            _ => {}
        }
    }
}

impl ObservationState {
    fn observe_tool_change(
        &mut self,
        event: &external_contract::events::NormalizedEvent,
        tool: &external_contract::events::ToolChange,
    ) {
        use external_contract::events::*;
        let turn = crate::runtime_observation::turn_key(event);
        let identity = format!("{turn}\0{}", tool.call_id.0);
        let known = self.seen_calls.get(&identity).cloned();
        if known.is_none()
            && (tool.phase == Some(ToolPhase::Progress)
                || (tool.phase.is_none() && tool.arguments.is_none()))
        {
            return;
        }
        // Reserve identity/group capacity before retaining argument fragments.
        // Rejected calls cannot grow the bounded fragment-loss ledger.
        if known.is_none() && self.seen_calls.len() >= MAX_IDENTITIES {
            self.drop_event(true);
            return;
        }
        let name = tool
            .name
            .clone()
            .or_else(|| known.as_ref().map(|(name, _)| name.clone()))
            .unwrap_or_else(|| "unknown".into());
        if tool.call_id.0.len() > MAX_ID_BYTES
            || name.len() > MAX_ID_BYTES
            || tool.call_id.0.contains('\0')
            || name.contains('\0')
        {
            self.drop_event(true);
            return;
        }
        if known.is_none() && !self.tools.contains_key(&name) && self.tools.len() >= MAX_TOOL_TYPES
        {
            self.drop_event(true);
            return;
        }
        if let Some((old_name, seq)) = &known {
            if old_name == "unknown" && name != *old_name {
                if self.tools.contains_key(&name) || self.tools.len() < MAX_TOOL_TYPES {
                    if let Some(group) = self.tools.get_mut(old_name) {
                        group.call_count = group.call_count.saturating_sub(1);
                        let call = group
                            .recent_calls
                            .iter()
                            .position(|call| call.seq == *seq)
                            .and_then(|index| group.recent_calls.remove(index));
                        let empty = group.call_count == 0;
                        if empty {
                            self.tools.remove(old_name);
                        }
                        let group = self.tools.entry(name.clone()).or_default();
                        group.call_count = group.call_count.saturating_add(1);
                        group.latest_seq = group.latest_seq.max(*seq);
                        if let Some(call) = call {
                            group.recent_calls.push_front(call);
                            group
                                .recent_calls
                                .make_contiguous()
                                .sort_by_key(|call| std::cmp::Reverse(call.seq));
                            group.recent_calls.truncate(MAX_RECENT_CALLS);
                        }
                        self.seen_calls
                            .insert(identity.clone(), (name.clone(), *seq));
                    }
                } else {
                    self.drop_event(true);
                }
            }
        }
        let known = self.seen_calls.get(&identity).cloned();
        let previous = known
            .as_ref()
            .and_then(|(name, seq)| {
                self.tools
                    .get(name)?
                    .recent_calls
                    .iter()
                    .find(|call| call.seq == *seq)
            })
            .cloned();
        let (arguments, truncated, redacted) = match &tool.arguments {
            Some(ArgumentUpdate::Snapshot(value)) => {
                self.argument_fragments.remove(&identity);
                self.limited_fragments.remove(&identity);
                let (args, limited, count) = project_arguments(Some(value));
                (args, limited || !value.is_object(), count)
            }
            Some(ArgumentUpdate::MergePatch(patch)) => {
                let mut value = Value::Object(
                    previous
                        .as_ref()
                        .map(|call| call.arguments.clone())
                        .unwrap_or_default(),
                );
                merge_patch(&mut value, patch);
                let (args, limited, count) = project_arguments(Some(&value));
                (
                    args,
                    limited
                        || previous
                            .as_ref()
                            .is_none_or(|call| call.arguments_truncated),
                    count + previous.as_ref().map_or(0, |call| call.redacted_fields),
                )
            }
            Some(ArgumentUpdate::Delta(delta)) => {
                if self.limited_fragments.contains(&identity) {
                    (
                        previous
                            .as_ref()
                            .map(|call| call.arguments.clone())
                            .unwrap_or_default(),
                        true,
                        previous.as_ref().map_or(0, |call| call.redacted_fields),
                    )
                } else if self.argument_fragments.len() < MAX_TOOL_TYPES * MAX_RECENT_CALLS
                    || self.argument_fragments.contains_key(&identity)
                {
                    let fragment = self.argument_fragments.entry(identity.clone()).or_default();
                    if fragment.len().saturating_add(delta.len()) <= MAX_ARGUMENT_BYTES {
                        fragment.push_str(delta);
                    } else {
                        fragment.clear();
                        self.limited_fragments.insert(identity.clone());
                    }
                    match serde_json::from_str::<Value>(fragment) {
                        Ok(value) if value.is_object() => project_arguments(Some(&value)),
                        _ => (
                            previous
                                .as_ref()
                                .map(|call| call.arguments.clone())
                                .unwrap_or_default(),
                            true,
                            previous.as_ref().map_or(0, |call| call.redacted_fields),
                        ),
                    }
                } else {
                    self.limited_fragments.insert(identity.clone());
                    (Map::new(), true, 0)
                }
            }
            None => previous
                .as_ref()
                .map(|call| {
                    (
                        call.arguments.clone(),
                        call.arguments_truncated,
                        call.redacted_fields,
                    )
                })
                .unwrap_or((Map::new(), true, 0)),
        };
        self.record_tool_call(
            &turn,
            &tool.call_id.0,
            &name,
            arguments,
            truncated,
            redacted,
        );
        let Some((name, seq)) = self.seen_calls.get(&identity).cloned() else {
            return;
        };
        if let Some(call) = self
            .tools
            .get_mut(&name)
            .and_then(|group| group.recent_calls.iter_mut().find(|call| call.seq == seq))
        {
            let terminal = matches!(
                call.status,
                ObservedCallStatus::Finished
                    | ObservedCallStatus::Succeeded
                    | ObservedCallStatus::Failed
                    | ObservedCallStatus::Cancelled
            );
            let status = match tool.phase {
                Some(ToolPhase::Finished) => match tool.outcome {
                    Some(ToolOutcome::Succeeded) => ObservedCallStatus::Succeeded,
                    Some(ToolOutcome::Failed) => ObservedCallStatus::Failed,
                    Some(ToolOutcome::Cancelled) => ObservedCallStatus::Cancelled,
                    _ => ObservedCallStatus::Finished,
                },
                Some(ToolPhase::Scheduled) if !terminal => ObservedCallStatus::Pending,
                Some(ToolPhase::Started | ToolPhase::Progress) if !terminal => {
                    ObservedCallStatus::Running
                }
                Some(_) => call.status,
                None => call.status,
            };
            if status != call.status {
                call.status = status;
                self.bump_snapshot();
            }
        }
        if tool.phase == Some(ToolPhase::Finished) {
            self.argument_fragments.remove(&identity);
        }
    }

    pub(crate) fn promote_turn(&mut self, old: &str, new: &str) {
        let prefix = format!("{old}\0");
        let calls = std::mem::take(&mut self.seen_calls);
        for (key, value) in calls {
            let key = key
                .strip_prefix(&prefix)
                .map(|suffix| format!("{new}\0{suffix}"))
                .unwrap_or(key);
            self.seen_calls.insert(key, value);
        }
        let fragments = std::mem::take(&mut self.argument_fragments);
        for (key, value) in fragments {
            let key = key
                .strip_prefix(&prefix)
                .map(|suffix| format!("{new}\0{suffix}"))
                .unwrap_or(key);
            self.argument_fragments.insert(key, value);
        }
        self.limited_fragments = std::mem::take(&mut self.limited_fragments)
            .into_iter()
            .map(|key| {
                key.strip_prefix(&prefix)
                    .map(|suffix| format!("{new}\0{suffix}"))
                    .unwrap_or(key)
            })
            .collect();
    }

    pub(crate) fn projection_losses(&self) -> Vec<external_contract::events::CoverageDimension> {
        use external_contract::events::CoverageDimension;
        let mut losses = Vec::new();
        if self.tool_retention_gap {
            losses.push(CoverageDimension::Tools);
        }
        if self.reasoning_retention_gap {
            losses.push(CoverageDimension::PublicReasoning);
        }
        losses
    }
}
fn merge_patch(target: &mut Value, patch: &Value) {
    if let Value::Object(fields) = patch {
        if !target.is_object() {
            *target = Value::Object(Map::new());
        }
        let object = target.as_object_mut().expect("object");
        for (key, value) in fields {
            if value.is_null() {
                object.remove(key);
            } else {
                merge_patch(object.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
    } else {
        *target = patch.clone();
    }
}

#[cfg(test)]
mod typed_projection_tests {
    use super::*;
    use external_contract::events::*;
    struct ProjectionTrace {
        inner: ObservationState,
        adapter: crate::zcode::events::ZcodeAdapter,
    }
    impl Default for ProjectionTrace {
        fn default() -> Self {
            Self {
                inner: ObservationState {
                    tool_history_complete: true,
                    reasoning_complete: true,
                    ..ObservationState::default()
                },
                adapter: crate::zcode::events::ZcodeAdapter::new(
                    TaskId("projection".into()),
                    LaunchId("projection".into()),
                ),
            }
        }
    }
    impl std::ops::Deref for ProjectionTrace {
        type Target = ObservationState;
        fn deref(&self) -> &Self::Target {
            &self.inner
        }
    }
    impl std::ops::DerefMut for ProjectionTrace {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.inner
        }
    }
    impl ProjectionTrace {
        fn ingest_fixture(&mut self, method: &str, params: &Value) {
            let input =
                external_runtime::Inbound::Message(external_contract::WireMessage::UnknownEvent {
                    method: method.into(),
                    raw: serde_json::json!({"params":params}),
                });
            let batch = self.adapter.decode(&input).expect("fixture notification");
            for event in batch.events() {
                self.inner.observe_fact(event);
            }
        }
    }
    fn event(id: &str, kind: &str, payload: Value) -> Value {
        serde_json::json!({"type":"model.streaming","eventId":id,"turnId":"turn",
        "payload":{"kind":kind,"toolCallId":payload.get("toolCallId"),"toolName":payload.get("toolName"),"input":payload.get("input"),"delta":payload.get("delta")}})
    }
    #[test]
    fn ordinary_reasoning_remains_visible_during_streaming() {
        let mut state = ProjectionTrace::default();
        for (index, (chunk, expected)) in [
            ("ordinary ", "ordinary "),
            ("streaming text", "ordinary streaming text"),
        ]
        .into_iter()
        .enumerate()
        {
            state.ingest_fixture(
                "session/event",
                &event(
                    &index.to_string(),
                    "reasoning_delta",
                    serde_json::json!({"delta":chunk}),
                ),
            );
            assert_eq!(state.snapshot().reasoning.text, expected);
        }
    }

    #[test]
    fn reasoning_budget_recovers_latest_tail_after_coverage_loss() {
        let mut state = ProjectionTrace::default();
        state.ingest_fixture(
            "session/event",
            &event(
                "1",
                "reasoning_delta",
                serde_json::json!({"delta":"x".repeat(MAX_REASONING_BYTES)}),
            ),
        );
        state.ingest_fixture(
            "session/event",
            &event(
                "2",
                "reasoning_delta",
                serde_json::json!({"delta":" post-overflow-unique-marker"}),
            ),
        );
        let snapshot = state.snapshot();
        assert!(snapshot
            .reasoning
            .text
            .ends_with("post-overflow-unique-marker"));
        assert!(state.reasoning_raw.len() <= MAX_REASONING_BYTES);
        assert!(snapshot.reasoning.text.chars().count() <= MAX_REASONING_CHARS);
        assert!(snapshot.reasoning.truncated);
        assert!(!snapshot.coverage.reasoning_complete);
        assert_eq!(snapshot.coverage.dropped_events, 1);
    }

    #[test]
    fn reasoning_overflow_tail_counts_chinese_and_emoji_as_unicode_scalars() {
        let mut state = ProjectionTrace::default();
        state.ingest_fixture(
            "session/event",
            &event(
                "1",
                "reasoning_delta",
                serde_json::json!({"delta":"x".repeat(MAX_REASONING_BYTES)}),
            ),
        );
        state.ingest_fixture(
            "session/event",
            &event(
                "2",
                "reasoning_delta",
                serde_json::json!({"delta":format!(" {}", "中🙂".repeat(110))}),
            ),
        );
        let snapshot = state.snapshot();
        assert_eq!(snapshot.reasoning.text.chars().count(), MAX_REASONING_CHARS);
        assert_eq!(snapshot.reasoning.text, "中🙂".repeat(100));
        assert!(state.reasoning_raw.len() <= MAX_REASONING_BYTES);
        assert!(!snapshot.coverage.reasoning_complete);
    }

    #[test]
    fn rolling_reasoning_window_bounds_capacity_after_repeated_oversized_deltas() {
        let mut state = ProjectionTrace::default();
        for index in 0..4 {
            state.ingest_fixture(
                "session/event",
                &event(
                    &index.to_string(),
                    "reasoning_delta",
                    serde_json::json!({"delta":"x".repeat(MAX_REASONING_BYTES + 1)}),
                ),
            );
            assert!(state.reasoning_raw.len() <= MAX_REASONING_BYTES);
            assert!(state.reasoning_raw.capacity() <= MAX_REASONING_BYTES);
        }
    }

    #[test]
    fn gradual_reasoning_window_growth_never_keeps_an_oversized_allocation() {
        let mut state = ProjectionTrace::default();
        for (index, delta) in [
            "x".repeat(40 * 1024),
            "y".repeat(20 * 1024),
            "z".repeat(4 * 1024),
        ]
        .into_iter()
        .enumerate()
        {
            state.ingest_fixture(
                "session/event",
                &event(
                    &index.to_string(),
                    "reasoning_delta",
                    serde_json::json!({"delta":delta}),
                ),
            );
            assert!(state.reasoning_raw.len() <= MAX_REASONING_BYTES);
            assert!(state.reasoning_raw.capacity() <= MAX_REASONING_BYTES);
        }
    }

    #[test]
    fn oversized_append_never_places_more_than_the_bound_in_observation_state() {
        let mut state = ProjectionTrace::default();
        state.ingest_fixture(
            "session/event",
            &event(
                "1",
                "reasoning_delta",
                serde_json::json!({"delta":"seed".repeat(16 * 1024)}),
            ),
        );
        state.ingest_fixture(
            "session/event",
            &event(
                "2",
                "reasoning_delta",
                serde_json::json!({"delta":"oversized".repeat(MAX_REASONING_BYTES)}),
            ),
        );
        assert!(state.reasoning_raw.len() <= MAX_REASONING_BYTES);
        assert!(state.reasoning_raw.capacity() <= MAX_REASONING_BYTES);
    }

    #[test]
    fn tools_rank_by_count_then_recency_and_keep_five_recent_calls() {
        let mut state = ProjectionTrace::default();
        let mut id = 0;
        for (name, count) in [("Read", 8), ("Bash", 6), ("Search", 4), ("Edit", 1)] {
            for call in 0..count {
                id += 1;
                let payload = serde_json::json!({"toolCallId": format!("call-{id}"), "toolName": name, "input": {"n": call}});
                state.ingest_fixture(
                    "session/event",
                    &event(&id.to_string(), "tool_call", payload),
                );
            }
        }
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot
                .tools
                .iter()
                .map(|tool| tool.tool_name.as_str())
                .collect::<Vec<_>>(),
            ["Read", "Bash", "Search"]
        );
        assert_eq!(snapshot.tools[0].call_count, 8);
        assert_eq!(snapshot.tools[0].recent_calls.len(), 5);
        assert_eq!(
            snapshot.tools[0]
                .recent_calls
                .iter()
                .map(|call| call.seq)
                .collect::<Vec<_>>(),
            [8, 7, 6, 5, 4]
        );
    }

    #[test]
    fn call_updates_do_not_recount_and_arguments_preserve_values_except_encrypted_content() {
        let mut state = ProjectionTrace::default();
        let initial = serde_json::json!({"toolCallId":"call", "toolName":"Bash", "input": {}});
        let update = serde_json::json!({"toolCallId":"call", "toolName":"Bash", "input": {"command":"echo secret-value", "nested":{"encrypted_content":"NEVER"}, "token":"NEVER"}});
        state.ingest_fixture("session/event", &event("1", "tool_call", initial));
        state.ingest_fixture("session/event", &event("2", "tool_call", update));
        let snapshot = state.snapshot();
        let tool = &snapshot.tools[0];
        assert_eq!(tool.call_count, 1);
        assert_eq!(tool.recent_calls[0].seq, 1);
        let encoded = serde_json::to_string(&tool.recent_calls[0].arguments).unwrap();
        assert!(!encoded.contains("NEVER"));
        assert!(!encoded.contains("encrypted_content"));
        assert_eq!(tool.recent_calls[0].redacted_fields, 1);
    }

    #[test]
    fn identical_arguments_with_distinct_call_ids_are_counted() {
        let mut state = ProjectionTrace::default();
        for id in 1..=3 {
            let payload = serde_json::json!({"toolCallId":id.to_string(), "toolName":"Bash", "input":{"command":"true"}});
            state.ingest_fixture(
                "session/event",
                &event(&id.to_string(), "tool_call", payload),
            );
        }
        assert_eq!(state.snapshot().tools[0].call_count, 3);
    }

    #[test]
    fn long_arguments_are_bounded_and_marked_truncated() {
        let mut state = ProjectionTrace::default();
        let payload = serde_json::json!({"toolCallId":"1", "toolName":"Read", "input":{"path":"x".repeat(10_000)}});
        state.ingest_fixture("session/event", &event("1", "tool_call", payload));
        let call = &state.snapshot().tools[0].recent_calls[0];
        assert!(call.arguments_truncated);
        assert!(serde_json::to_vec(&call.arguments).unwrap().len() <= MAX_ARGUMENT_BYTES);
    }

    #[test]
    fn near_driver_limit_arguments_use_bounded_linear_projection_work() {
        for size in [256 * 1024, 900 * 1024] {
            let mut state = ProjectionTrace::default();
            let payload = serde_json::json!({
                "toolCallId":"1",
                "toolName":"Bash",
                "input":{"command":"x".repeat(size), "token":"must-not-copy"}
            });
            let started = std::time::Instant::now();
            state.ingest_fixture("session/event", &event("1", "tool_call", payload));
            let elapsed = started.elapsed();
            let snapshot = state.snapshot();
            let call = &snapshot.tools[0].recent_calls[0];
            assert!(call.arguments_truncated);
            assert!(serde_json::to_vec(&call.arguments).unwrap().len() <= MAX_ARGUMENT_BYTES);
            assert!(!serde_json::to_string(&call.arguments)
                .unwrap()
                .contains("must-not-copy"));
            assert!(
                elapsed < std::time::Duration::from_secs(5),
                "{size} bytes took {elapsed:?}"
            );
        }
    }
}
