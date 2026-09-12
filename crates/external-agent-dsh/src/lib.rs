//! DeepSeek Harness (DSH) ACP adapter.
//!
//! This crate owns the DSH provider surface only: the managed launch profile
//! and the official ACP JSON-RPC 2.0 stdio protocol (transport shapes, session
//! sequence, model selection, permission offers, session updates, and result
//! folding). Task lifecycle, queueing, persistence, and process reaping stay
//! with the daemon scheduler and the shared runtime driver.
//!
//! Production DSH spawn stays behind the S04 feature gate; nothing in this
//! crate installs a provider, writes user credentials, or mutates global
//! provider configuration.

pub mod acp;
pub mod profile;

pub const DSH_AGENT_NAME: &str = "dsh";
