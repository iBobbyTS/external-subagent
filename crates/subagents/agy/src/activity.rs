//! Canonical tool-activity derivation for `agy` `tool` steps.
//!
//! A measured tool step is an `ACTIVE`/`DONE` pair sharing one `step_index`:
//! the `ACTIVE` half carries `tool_info.name`, the `DONE` half additionally
//! carries `tool_info.output` (`docs/compatibility/antigravity.md` §1). This
//! module turns such a step into plain [`ToolActivity`] data whose identity is
//! `{conversation_id}:{step_index}`, and can serialize it with the shared
//! `external-contract` constructors the daemon's 60-second activity window
//! consumes. Nothing here holds state or emits; the daemon stamps the
//! monotonic `event_id` and the conversation-derived `turn_id`.

use external_contract::activity::{tool_result_event, tool_started_event};
use serde_json::Value;

use crate::event::{AgyEvent, StepState, StepType};

/// Which half of a tool step an activity describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPhase {
    Started,
    Result,
}

/// Plain tool-activity data for one `agy` tool step. `tool_name` is `Some`
/// only for a [`ToolPhase::Started`] record that named a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolActivity {
    pub conversation_id: String,
    pub step_index: u64,
    pub tool_name: Option<String>,
    pub phase: ToolPhase,
}

impl ToolActivity {
    /// The stable call identity `{conversation_id}:{step_index}`.
    pub fn tool_call_id(&self) -> String {
        format!("{}:{}", self.conversation_id, self.step_index)
    }

    /// Serialize this record with the shared detailed activity vocabulary. The
    /// `event_id` is the daemon's monotonic sequence and `turn_id` its
    /// conversation-scoped turn label; both are supplied by the caller.
    pub fn canonical_event(&self, event_id: &str, turn_id: &str) -> Value {
        let tool_call_id = self.tool_call_id();
        match self.phase {
            ToolPhase::Started => {
                tool_started_event(event_id, turn_id, &tool_call_id, self.tool_name.as_deref())
            }
            ToolPhase::Result => tool_result_event(event_id, turn_id, &tool_call_id),
        }
    }
}

/// Derive the activity record for a parsed event, if it is a tool step in a
/// modeled state. A non-tool step, an unknown tool state, or a non-step event
/// yields `None` (no state is invented).
pub fn tool_activity(event: &AgyEvent) -> Option<ToolActivity> {
    let AgyEvent::StepUpdate(step) = event else {
        return None;
    };
    if step.step_type != StepType::Tool {
        return None;
    }
    let phase = match step.state {
        StepState::Active => ToolPhase::Started,
        StepState::Done => ToolPhase::Result,
        StepState::Other(_) => return None,
    };
    let tool_name = step
        .tool_info
        .as_ref()
        .and_then(|info| info.name.clone())
        .or_else(|| step.tool_name.clone());
    Some(ToolActivity {
        conversation_id: step.conversation_id.clone(),
        step_index: step.step_index,
        tool_name,
        phase,
    })
}

/// Derive and serialize one canonical activity event, if the input is a tool
/// step.
pub fn canonical_activity(event: &AgyEvent, event_id: &str, turn_id: &str) -> Option<Value> {
    tool_activity(event).map(|activity| activity.canonical_event(event_id, turn_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::parse_line;

    const TOOLSTEP: &str = include_str!("../tests/fixtures/toolstep.ndjson");

    fn tool_steps() -> Vec<AgyEvent> {
        TOOLSTEP
            .lines()
            .map(|line| parse_line(line).unwrap())
            .filter(|event| {
                matches!(event, AgyEvent::StepUpdate(step) if step.step_type == StepType::Tool)
            })
            .collect()
    }

    #[test]
    fn tool_step_pair_yields_started_and_result_activities() {
        let steps = tool_steps();
        assert_eq!(steps.len(), 2, "the fixture has one ACTIVE/DONE pair");

        let started = tool_activity(&steps[0]).expect("active tool step");
        assert_eq!(started.phase, ToolPhase::Started);
        assert_eq!(started.tool_name.as_deref(), Some("run_command"));
        assert_eq!(started.step_index, 2);
        assert_eq!(
            started.tool_call_id(),
            "7e37ef63-9547-4ae1-b3fd-0e7a8c125f73:2"
        );

        let result = tool_activity(&steps[1]).expect("done tool step");
        assert_eq!(result.phase, ToolPhase::Result);
        assert_eq!(result.tool_call_id(), started.tool_call_id());
        assert_eq!(result.tool_name.as_deref(), Some("run_command"));

        let turn_id = started.conversation_id.as_str();
        assert_eq!(
            started.canonical_event("agy-event-1", turn_id),
            serde_json::json!({
                "type": "tool.updated",
                "eventId": "agy-event-1",
                "turnId": turn_id,
                "payload": {
                    "kind": "started",
                    "toolCallId": "7e37ef63-9547-4ae1-b3fd-0e7a8c125f73:2",
                    "toolName": "run_command",
                },
            })
        );
        assert_eq!(
            result.canonical_event("agy-event-2", turn_id),
            serde_json::json!({
                "type": "tool.updated",
                "eventId": "agy-event-2",
                "turnId": turn_id,
                "payload": {
                    "kind": "result",
                    "toolCallId": "7e37ef63-9547-4ae1-b3fd-0e7a8c125f73:2",
                },
            })
        );
        // The convenience wrapper builds the same record from the raw event.
        assert_eq!(
            canonical_activity(&steps[0], "agy-event-1", turn_id),
            Some(started.canonical_event("agy-event-1", turn_id))
        );
    }

    #[test]
    fn tool_name_prefers_tool_info_then_the_top_level_name() {
        let with_info = concat!(
            r#"{"event":"step_update","step_update":{"conversation_id":"c","#,
            r#""step_index":4,"state":"ACTIVE","step_type":"tool","tool_name":"top","#,
            r#""tool_info":{"name":"from_info"}}}"#
        );
        let activity = tool_activity(&parse_line(with_info).unwrap()).unwrap();
        assert_eq!(activity.tool_name.as_deref(), Some("from_info"));

        let without_info = concat!(
            r#"{"event":"step_update","step_update":{"conversation_id":"c","#,
            r#""step_index":4,"state":"ACTIVE","step_type":"tool","tool_name":"top"}}"#
        );
        let activity = tool_activity(&parse_line(without_info).unwrap()).unwrap();
        assert_eq!(activity.tool_name.as_deref(), Some("top"));

        let unnamed = concat!(
            r#"{"event":"step_update","step_update":{"conversation_id":"c","#,
            r#""step_index":4,"state":"ACTIVE","step_type":"tool"}}"#
        );
        let activity = tool_activity(&parse_line(unnamed).unwrap()).unwrap();
        assert_eq!(activity.tool_name, None);
        // An unnamed started event omits `toolName` rather than emitting null.
        assert!(activity
            .canonical_event("e", "t")
            .pointer("/payload/toolName")
            .is_none());
    }

    #[test]
    fn non_tool_steps_have_no_activity() {
        for line in TOOLSTEP.lines() {
            let event = parse_line(line).unwrap();
            let is_tool =
                matches!(&event, AgyEvent::StepUpdate(step) if step.step_type == StepType::Tool);
            if !is_tool {
                assert_eq!(tool_activity(&event), None);
            }
        }
        let agent_step = concat!(
            r#"{"event":"step_update","step_update":{"conversation_id":"c","#,
            r#""step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"x"}}"#
        );
        assert_eq!(tool_activity(&parse_line(agent_step).unwrap()), None);
        // An unknown tool state does not fabricate an activity.
        let unknown_state = concat!(
            r#"{"event":"step_update","step_update":{"conversation_id":"c","#,
            r#""step_index":2,"state":"PAUSED","step_type":"tool","tool_info":{"name":"n"}}}"#
        );
        assert_eq!(tool_activity(&parse_line(unknown_state).unwrap()), None);
    }
}
