//! Codex upstream app-server runtime owner, gated factory, and launch
//! resolution.
//!
//! The Codex app-server speaks NDJSON over stdio with the same strict
//! no-`jsonrpc` envelope the ZCode driver already pins, so this owner reuses
//! [`external_runtime::Driver`] with the `ZcodeStrict` codec. What is
//! Codex-specific lives here: the `initialize`/`initialized` handshake, the
//! persistent `thread/start`/`thread/resume` identity, `turn/start` and
//! `turn/interrupt`, and the normalization of Codex `item/*` and `turn/*`
//! notifications into the canonical internal `session/event` lifecycle so the
//! existing scheduler, result, storage, and observation consumers keep working.
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
