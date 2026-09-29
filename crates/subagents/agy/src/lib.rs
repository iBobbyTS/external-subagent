//! Pure Google Antigravity (`agy`) stream-json protocol face for the
//! external-subagent daemon.
//!
//! This crate owns the `agy` provider surface only: the parsed shapes of the
//! three stdout events (`init`/`step_update`/`result`) emitted by a long-lived
//! `agy --input-format stream-json --output-format stream-json` process, the
//! pure turn-terminal classifier (a turn settles on the last `result` event it
//! saw; the classifier never depends on process liveness), the canonical
//! tool-activity derivation, the launch-argv assembly, and the `agy models` /
//! `agy --version` output parsers. Task lifecycle, the NDJSON pump, process
//! gating/reaping, and scheduling stay with the daemon (`external-daemon`),
//! which feeds parsed events in and consumes the settlements and activity
//! records this crate produces.
//!
//! Protocol authority is `docs/compatibility/antigravity.md` (probe of `agy`
//! 1.2.12); every wire shape here is the measured one, not the official
//! documentation. Malformed JSON lines are the caller's transport concern:
//! [`event::parse_line`] reports them as errors and the daemon decides whether
//! to skip them.

pub mod activity;
pub mod event;
pub mod launch;
pub mod session;

/// The product agent name for the Antigravity adapter.
pub const AGY_AGENT_NAME: &str = "agy";
