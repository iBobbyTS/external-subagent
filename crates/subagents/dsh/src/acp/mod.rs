//! Official DSH ACP protocol modules. Provider-specific wire semantics live
//! here and nowhere else; the daemon consumes only normalized events.

pub mod model;
pub mod permission;
pub mod result;
pub mod session;
pub mod transport;
pub mod update;
