//! Antigravity (`agy`) stream-json runtime owner, gated factory, and agent
//! routing composition.
//!
//! The pure protocol face lives in the `external-agent-agy` crate
//! (`crates/subagents/agy`): the parsed shapes of the `init`/`step_update`/
//! `result` events, the last-result-wins turn classifier, the canonical tool
//! activity derivation, and the launch-argv assembly. This module keeps the
//! daemon lifecycle composition on top of it: the fail-closed spawn gate, the
//! NDJSON pump (the [`external_runtime::FrameCodec::Ndjson`] driver), the
//! `ManagedRuntime` trait surface (bootstrap on the `init` event, stdin `user`
//! events per turn, process-group termination for cancellation, stop/reap),
//! and the normalization of `agy` events into the canonical `session/event`
//! stream.
//!
//! `agy` has no interactive control plane (`control_request` is explicitly
//! unsupported), so a turn cannot be interrupted cooperatively: `stop_turn`
//! terminates the process group, and the child's measured `interrupted` result
//! (`docs/compatibility/antigravity.md` §4) settles the turn as cancelled.
//!
//! Production spawn is selected only by the explicit configuration and
//! runtime-path gate; the closed factory remains the fail-closed default.

mod events;
mod factory;
mod owner;
mod transport;

#[cfg(test)]
mod tests;

pub use factory::{AgyRuntimeFactory, AgySpawnGate};
pub use owner::AgyRuntimeOwner;
