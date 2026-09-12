//! Persistence projection records emitted by the lifecycle sink.
//!
//! Keeping this wire-facing value separate from the scheduler makes the
//! projection boundary explicit without changing lifecycle ordering.

/// Redacted lifecycle payload ready for persistence.
pub(crate) struct LifecycleProjection {
    pub(crate) event_type: &'static str,
    pub(crate) payload_json: String,
    pub(crate) redaction_level: &'static str,
}
