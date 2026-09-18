//! Public MCP facade.
//!
//! The tool catalog and handlers live in `mcp::tools`; the projections in
//! `mcp::views`; input schemas and defaults in `mcp::schemas`; the error
//! envelope in `mcp::errors`; the public envelope types and task-id mapping
//! in `mcp::types`. This facade re-exports every historical path so
//! existing `use crate::mcp::X` consumers keep resolving unchanged.

mod errors;
mod schemas;
mod tools;
mod types;
mod views;

pub use errors::{PublicErrorEnvelope, PublicToolErrorBody, ToolError};
pub use tools::{serve_stdio, SubagentMcp, PUBLIC_TOOLS};
pub use types::{
    PublicDecision, PublicOperation, PublicPendingKind, PublicPendingRequest, PublicQuestion,
    PublicResponseDisposition,
};
