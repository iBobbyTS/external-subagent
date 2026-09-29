//! Parsed shapes of the three `agy` stdout stream-json events and the
//! lenient line parser.
//!
//! The payload of every event is nested inside an inner object keyed by the
//! event name (`init`/`step_update`/`result`); the `init` event additionally
//! carries `conversation_id` at the top level, while the other two carry it
//! inside their payload (measured, `docs/compatibility/antigravity.md` §1/§7).
//! Enum-valued wire fields (`state`, `step_type`, `status`) deserialize into
//! named variants with an `Other(String)` arm, so an unknown value is
//! preserved instead of aborting the parse; unknown object fields and unknown
//! event names are likewise kept or ignored, never an error. Only a
//! syntactically malformed JSON line (or a known event whose required payload
//! is malformed) returns an error, which the caller treats as a transport
//! concern.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// The step lifecycle state reported by a `step_update`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepState {
    Active,
    Done,
    /// An unrecognized state, preserved verbatim.
    Other(String),
}

impl<'de> Deserialize<'de> for StepState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.as_str() {
            "ACTIVE" => Self::Active,
            "DONE" => Self::Done,
            _ => Self::Other(raw),
        })
    }
}

/// The kind of step a `step_update` reports. The five sampled/documented kinds
/// are named; `subagent_info` and any future kind land in `Other`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepType {
    UserInput,
    AgentResponse,
    Tool,
    SystemMessage,
    Checkpoint,
    /// An unrecognized step type, preserved verbatim.
    Other(String),
}

impl StepType {
    pub fn as_str(&self) -> &str {
        match self {
            Self::UserInput => "user_input",
            Self::AgentResponse => "agent_response",
            Self::Tool => "tool",
            Self::SystemMessage => "system_message",
            Self::Checkpoint => "checkpoint",
            Self::Other(name) => name,
        }
    }
}

impl<'de> Deserialize<'de> for StepType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.as_str() {
            "user_input" => Self::UserInput,
            "agent_response" => Self::AgentResponse,
            "tool" => Self::Tool,
            "system_message" => Self::SystemMessage,
            "checkpoint" => Self::Checkpoint,
            _ => Self::Other(raw),
        })
    }
}

/// The terminal status of a `result` event. `SUCCESS` and `ERROR` are the
/// only statuses observed in the probe; the other five are the documented
/// enumeration and are preserved distinctly even though they stay unreachable
/// until the unpublished control plane exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultStatus {
    Success,
    Error,
    Canceled,
    Interrupted,
    Invalid,
    Waiting,
    Running,
    /// An unrecognized status, preserved verbatim.
    Other(String),
}

impl ResultStatus {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Success => "SUCCESS",
            Self::Error => "ERROR",
            Self::Canceled => "CANCELED",
            Self::Interrupted => "INTERRUPTED",
            Self::Invalid => "INVALID",
            Self::Waiting => "WAITING",
            Self::Running => "RUNNING",
            Self::Other(status) => status,
        }
    }
}

impl<'de> Deserialize<'de> for ResultStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.as_str() {
            "SUCCESS" => Self::Success,
            "ERROR" => Self::Error,
            "CANCELED" => Self::Canceled,
            "INTERRUPTED" => Self::Interrupted,
            "INVALID" => Self::Invalid,
            "WAITING" => Self::Waiting,
            "RUNNING" => Self::Running,
            _ => Self::Other(raw),
        })
    }
}

/// The token accounting carried by a `step_update` or `result`. Counts are
/// session-cumulative on a `result`; a missing field counts as zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub thinking_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

/// The tool payload a `tool` step carries. `parameters` and `output` are kept
/// as opaque JSON because their shape is tool-specific; `output` is present
/// only on the `DONE` half of a tool step.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ToolInfo {
    pub name: Option<String>,
    pub parameters: Option<Value>,
    pub output: Option<Value>,
}

/// One structured soft-deny entry attached to a `result` (default permission
/// posture denies tools without failing the task, `docs/compatibility/
/// antigravity.md` §2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct DeniedAction {
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub display_name: String,
}

/// The `init` payload: the child cwd, the advertised tool list, and the
/// effective permission mode.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct InitInfo {
    pub cwd: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    pub permission_mode: Option<String>,
}

/// A parsed `step_update` event.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct StepUpdate {
    /// The shared conversation identity; carried inside the payload (a
    /// top-level fallback is applied by [`parse_line`] for robustness).
    #[serde(default)]
    pub conversation_id: String,
    pub step_index: u64,
    pub state: StepState,
    pub step_type: StepType,
    pub text_delta: Option<String>,
    /// Present alongside `tool_info` on measured tool steps.
    pub tool_name: Option<String>,
    pub tool_info: Option<ToolInfo>,
    pub duration_seconds: Option<f64>,
    pub usage: Option<Usage>,
}

/// A parsed `result` event. The last `result` for a turn is the turn's
/// terminal record (see [`crate::session::TurnClassifier`]).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ResultPayload {
    /// The shared conversation identity; carried inside the payload (a
    /// top-level fallback is applied by [`parse_line`]).
    #[serde(default)]
    pub conversation_id: String,
    pub status: ResultStatus,
    #[serde(default)]
    pub response: String,
    pub error: Option<String>,
    #[serde(default)]
    pub denied_actions: Vec<DeniedAction>,
    /// Parse-only, intentionally retained with no consumer yet
    /// (`docs/compatibility/antigravity.md` §9-5).
    pub structured_output: Option<Value>,
    pub json_schema: Option<Value>,
    pub duration_seconds: Option<f64>,
    #[serde(default)]
    pub num_turns: u64,
    pub usage: Option<Usage>,
}

/// One parsed stdout stream-json event.
#[derive(Debug, Clone, PartialEq)]
pub enum AgyEvent {
    Init {
        conversation_id: String,
        init: InitInfo,
    },
    StepUpdate(StepUpdate),
    Result(ResultPayload),
    /// An event name this crate does not model; the raw value is preserved so
    /// the daemon can log it, and no state is produced from it.
    Unknown {
        event: Option<String>,
        raw: Value,
    },
}

impl AgyEvent {
    /// The conversation identity this event names, if any. An event with no
    /// identity (an `Unknown`, or a `result` from an admission failure with an
    /// empty `conversation_id`) yields `None`.
    pub fn conversation_id(&self) -> Option<&str> {
        let id = match self {
            Self::Init {
                conversation_id, ..
            } => conversation_id,
            Self::StepUpdate(step) => &step.conversation_id,
            Self::Result(result) => &result.conversation_id,
            Self::Unknown { .. } => return None,
        };
        (!id.is_empty()).then_some(id.as_str())
    }
}

/// Parse one stdout NDJSON line into an [`AgyEvent`].
///
/// A known event name whose payload is missing or malformed is an error (the
/// caller treats malformed lines as a transport concern); an unrecognized
/// event name is [`AgyEvent::Unknown`] with its raw value preserved, so a
/// novel event never aborts the stream.
pub fn parse_line(line: &str) -> Result<AgyEvent, serde_json::Error> {
    let raw: Value = serde_json::from_str(line)?;
    let Some(event) = raw.get("event").and_then(Value::as_str) else {
        return Ok(AgyEvent::Unknown { event: None, raw });
    };
    match event {
        "init" => {
            let conversation_id = raw
                .get("conversation_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let init = raw.get("init").cloned().unwrap_or(Value::Null);
            Ok(AgyEvent::Init {
                conversation_id,
                init: serde_json::from_value(init)?,
            })
        }
        "step_update" => {
            let mut step: StepUpdate = serde_json::from_value(payload(&raw, "step_update"))?;
            fallback_conversation_id(&mut step.conversation_id, &raw);
            Ok(AgyEvent::StepUpdate(step))
        }
        "result" => {
            let mut result: ResultPayload = serde_json::from_value(payload(&raw, "result"))?;
            fallback_conversation_id(&mut result.conversation_id, &raw);
            Ok(AgyEvent::Result(result))
        }
        other => Ok(AgyEvent::Unknown {
            event: Some(other.to_owned()),
            raw,
        }),
    }
}

fn payload(raw: &Value, key: &str) -> Value {
    raw.get(key).cloned().unwrap_or(Value::Null)
}

/// When a payload omits `conversation_id` but the event carries it at the top
/// level, adopt the top-level value. The measured shape nests it; this keeps
/// the parser tolerant of the equivalent flat placement without changing the
/// modeled shape.
fn fallback_conversation_id(conversation_id: &mut String, raw: &Value) {
    if conversation_id.is_empty() {
        if let Some(top) = raw.get("conversation_id").and_then(Value::as_str) {
            *conversation_id = top.to_owned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HAPPY: &str = include_str!("../tests/fixtures/happy.ndjson");
    const NEG_CONTROL: &str = include_str!("../tests/fixtures/neg-control.ndjson");

    #[test]
    fn parse_init_reads_the_top_level_id_and_nested_payload() {
        let line = HAPPY.lines().next().unwrap();
        let event = parse_line(line).unwrap();
        match event {
            AgyEvent::Init {
                conversation_id,
                init,
            } => {
                assert_eq!(conversation_id, "e1c47663-9ef1-43ec-a333-dccd868220f1");
                assert_eq!(init.permission_mode.as_deref(), Some("request-review"));
                assert!(init.cwd.as_deref().unwrap().ends_with("ws-1"));
                assert_eq!(init.tools.len(), 57);
                assert!(init.tools.iter().any(|tool| tool == "run_command"));
            }
            other => panic!("expected init, got {other:?}"),
        }
    }

    #[test]
    fn parse_step_update_reads_the_nested_payload() {
        let line = HAPPY.lines().nth(1).unwrap();
        match parse_line(line).unwrap() {
            AgyEvent::StepUpdate(step) => {
                assert_eq!(step.conversation_id, "e1c47663-9ef1-43ec-a333-dccd868220f1");
                assert_eq!(step.step_index, 0);
                assert_eq!(step.state, StepState::Done);
                assert_eq!(step.step_type, StepType::UserInput);
            }
            other => panic!("expected step_update, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_reads_the_nested_payload() {
        let line = HAPPY.lines().last().unwrap();
        match parse_line(line).unwrap() {
            AgyEvent::Result(result) => {
                assert_eq!(result.status, ResultStatus::Success);
                assert_eq!(result.response, "MANGO\n");
                assert_eq!(result.num_turns, 1);
                assert_eq!(
                    result.usage,
                    Some(Usage {
                        input_tokens: 12381,
                        output_tokens: 88,
                        thinking_tokens: 86,
                        cache_read_tokens: 0,
                        total_tokens: 12469,
                    })
                );
                assert!(result.error.is_none());
            }
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[test]
    fn unknown_event_names_are_preserved_without_error() {
        let event = parse_line(r#"{"event":"banana","payload":{"x":1}}"#).unwrap();
        assert_eq!(event.conversation_id(), None);
        match &event {
            AgyEvent::Unknown { event, raw } => {
                assert_eq!(event.as_deref(), Some("banana"));
                assert_eq!(raw["payload"]["x"], 1);
            }
            other => panic!("expected unknown, got {other:?}"),
        }
    }

    #[test]
    fn unknown_step_type_is_preserved_and_does_not_error() {
        let line = concat!(
            r#"{"event":"step_update","step_update":{"conversation_id":"c","#,
            r#""step_index":7,"state":"ACTIVE","step_type":"subagent_info"}}"#
        );
        match parse_line(line).unwrap() {
            AgyEvent::StepUpdate(step) => {
                assert_eq!(step.step_type, StepType::Other("subagent_info".into()));
                assert_eq!(step.step_type.as_str(), "subagent_info");
            }
            other => panic!("expected step_update, got {other:?}"),
        }
        // An unknown state and an unknown status are preserved the same way.
        let unknown_state = concat!(
            r#"{"event":"step_update","step_update":{"conversation_id":"c","#,
            r#""step_index":8,"state":"PAUSED","step_type":"tool"}}"#
        );
        match parse_line(unknown_state).unwrap() {
            AgyEvent::StepUpdate(step) => {
                assert_eq!(step.state, StepState::Other("PAUSED".into()));
            }
            other => panic!("expected step_update, got {other:?}"),
        }
        let unknown_status = concat!(
            r#"{"event":"result","result":{"conversation_id":"c","status":"MYSTERY","#,
            r#""response":"","num_turns":0}}"#
        );
        match parse_line(unknown_status).unwrap() {
            AgyEvent::Result(result) => {
                assert_eq!(result.status, ResultStatus::Other("MYSTERY".into()));
            }
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[test]
    fn unknown_extra_fields_are_ignored() {
        let line = concat!(
            r#"{"event":"result","result":{"conversation_id":"c","status":"SUCCESS","#,
            r#""response":"ok","num_turns":3,"future_field":{"nested":true}}}"#
        );
        match parse_line(line).unwrap() {
            AgyEvent::Result(result) => {
                assert_eq!(result.response, "ok");
                assert_eq!(result.num_turns, 3);
            }
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_is_the_callers_error() {
        assert!(parse_line("not json").is_err());
        assert!(parse_line(r#"{"event":"result","result":"missing fields"}"#).is_err());
    }

    #[test]
    fn parse_negative_input_result_reads_the_cli_error() {
        let line = NEG_CONTROL.lines().nth(1).unwrap();
        match parse_line(line).unwrap() {
            AgyEvent::Result(result) => {
                assert_eq!(result.status, ResultStatus::Error);
                assert!(result.error.as_deref().unwrap().contains("control_request"));
                assert_eq!(result.num_turns, 0);
            }
            other => panic!("expected result, got {other:?}"),
        }
    }
}
