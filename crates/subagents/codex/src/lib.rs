//! Pure Codex app-server protocol face for the external-subagent daemon.
//!
//! This crate owns the Codex provider surface only: the `thread/start`,
//! `thread/resume`, and `turn/start` parameter shapes, posture admission
//! (`codex_posture`), the fail-closed echo validation of thread results,
//! and the folding of Codex `item/*`/`turn/*` notifications into the
//! canonical internal `session/event` envelope. Task lifecycle, the gated
//! factory, the stdio transport, and scheduling stay with the daemon
//! (`external-daemon`), which maps this crate's [`session::CodexError`]
//! onto its `RuntimeCommandError` and its four-value permission modes onto
//! the binary [`session::CodexPermissionMode`] at the admitted-thread
//! boundary.

pub mod session;
pub mod update;
