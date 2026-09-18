//! DSH ACP runtime owner, gated factory, and agent routing composition.
//!
//! The provider protocol (methods, frames, offers, settlement folding) lives
//! in `external-agent-dsh`; this module composes it into the shared daemon
//! lifecycle: one child process per task, a continuous stdio pump that
//! normalizes ACP traffic into the canonical internal event envelope, prompt
//! settlement watchers that drive turn boundaries, and single-shot public
//! permission responses that only echo offered options.
//!
//! Production DSH spawn is selected only by the explicit configuration and
//! runtime-path gate; the closed factory remains the fail-closed default.

mod events;
mod factory;
mod owner;
mod routing;
mod session;
mod transport;

#[cfg(test)]
mod tests;

pub use factory::{DshRuntimeFactory, DshSpawnGate};
pub use owner::DshRuntimeOwner;
pub use routing::RoutingRuntimeFactory;
