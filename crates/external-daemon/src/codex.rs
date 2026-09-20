//! Codex upstream app-server runtime owner, gated factory, and launch
//! resolution.
//!
//! The pure Codex protocol face lives in the `external-agent-codex` crate
//! (`crates/subagents/codex`): the `thread/start`, `thread/resume`, and
//! `turn/start` parameter shapes, binary posture admission, fail-closed
//! result echo validation, and the folding of `item/*` and `turn/*`
//! notifications into the canonical internal `session/event` lifecycle.
//! This module keeps the daemon lifecycle composition on top of it: the
//! spawn gate, the runtime owner, the stdio transport (the `ZcodeStrict`-
//! codec [`external_runtime::Driver`] pump and Publisher), the persistent
//! thread and turn control-plane glue, and the normalization entry point
//! the scheduler consumes.
//!
//! Production spawn is selected only by the explicit configuration gate; the
//! closed factory remains the fail-closed default. Only `plan` and `yolo`
//! tasks are admitted, both pinning `approvalPolicy=never` (`plan` maps to
//! `sandbox=read-only`, `yolo` to `sandbox=danger-full-access`); the
//! `thread/start` and `thread/resume` results must confirm the applied
//! posture before any turn runs, and the child always runs with an
//! explicitly resolved `CODEX_HOME` — never a silent `~/.codex` fallback.

mod events;
mod factory;
mod owner;
mod session;
mod transport;

#[cfg(test)]
mod tests;

pub use factory::{resolve_codex_home, CodexLaunch, CodexRuntimeFactory, CodexSpawnGate};
pub use owner::CodexRuntimeOwner;
