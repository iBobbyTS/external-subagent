//! Current correlations and bounded historical observation evidence. No wire parsing.
use external_contract::events::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};

pub(crate) const MAX_DIAGNOSTICS: usize = 32;
const MAX_DIAGNOSTIC_FRAMES: usize = 4096;
const MAX_CURRENT_GAPS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryStatus {
    Healthy,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeCoverage {
    pub text: Option<bool>,
    pub tools: Option<bool>,
    pub public_reasoning: Option<bool>,
    pub model_activity: Option<bool>,
    pub input: Option<bool>,
    pub turn: Option<bool>,
}
impl RuntimeCoverage {
    fn field(&mut self, dimension: CoverageDimension) -> &mut Option<bool> {
        match dimension {
            CoverageDimension::Text => &mut self.text,
            CoverageDimension::Tools => &mut self.tools,
            CoverageDimension::PublicReasoning => &mut self.public_reasoning,
            CoverageDimension::ModelActivity => &mut self.model_activity,
            CoverageDimension::Input => &mut self.input,
            CoverageDimension::Turn => &mut self.turn,
        }
    }
    pub(crate) fn complete(&self, dimension: CoverageDimension) -> bool {
        (match dimension {
            CoverageDimension::Text => self.text,
            CoverageDimension::Tools => self.tools,
            CoverageDimension::PublicReasoning => self.public_reasoning,
            CoverageDimension::ModelActivity => self.model_activity,
            CoverageDimension::Input => self.input,
            CoverageDimension::Turn => self.turn,
        }) == Some(true)
    }
    fn support(&mut self, dimension: CoverageDimension) {
        self.field(dimension).get_or_insert(true);
    }
    fn gap(&mut self, impact: &Impact) {
        if let Impact::Known(dimensions) = impact {
            for dimension in dimensions {
                *self.field(*dimension) = Some(false);
            }
        }
    }
    fn inherit(&mut self, old: &Self) {
        use CoverageDimension::*;
        for dimension in [Text, Tools, PublicReasoning, ModelActivity, Input, Turn] {
            let older = match dimension {
                Text => old.text,
                Tools => old.tools,
                PublicReasoning => old.public_reasoning,
                ModelActivity => old.model_activity,
                Input => old.input,
                Turn => old.turn,
            };
            let field = self.field(dimension);
            *field = match (*field, older) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), _) | (_, Some(true)) => Some(true),
                _ => None,
            };
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiagnosticSummary {
    pub source: SourceProtocol,
    pub reason: String,
    pub impact: Impact,
    pub discriminator: String,
    pub count: u64,
    pub last_observed_at: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelActivityView {
    pub request_id: Option<RequestId>,
    pub phase: ModelPhase,
    pub retry: Option<NativeRetry>,
}

/// Metadata for exactly one anomalous native input, attached once to its batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDiagnostic {
    pub(crate) source: SourceProtocol,
    pub(crate) reason: DiagnosticReason,
    pub(crate) impact: Impact,
    pub(crate) discriminator: String,
    pub(crate) frame: Option<NativeFrameKey>,
    pub(crate) lost: bool,
}
impl InputDiagnostic {
    pub(crate) fn from_disposition(
        input: &NativeInputContext,
        disposition: &DecodeDisposition,
    ) -> Option<Self> {
        let (reason, impact, token, lost) = match disposition {
            DecodeDisposition::Unknown(d) => {
                (d.reason, d.impact.clone(), d.discriminator.as_ref(), false)
            }
            DecodeDisposition::InvalidLost(g) => (
                loss_reason(g.reason),
                g.impact.clone(),
                input.source.discriminator.as_ref(),
                true,
            ),
            _ => return None,
        };
        Some(Self {
            source: input.source.protocol,
            reason,
            impact: bounded_impact(impact),
            discriminator: safe_discriminator(token).into(),
            frame: input.frame_key(),
            lost,
        })
    }
    pub(crate) fn from_event(event: &NormalizedEvent) -> Option<Self> {
        let disposition = match &event.fact {
            EventFact::Diagnostic(d)
                if matches!(
                    d.reason,
                    DiagnosticReason::UnknownSemantics
                        | DiagnosticReason::Malformed
                        | DiagnosticReason::Truncated
                        | DiagnosticReason::Dropped
                ) =>
            {
                DecodeDisposition::Unknown(d.clone())
            }
            EventFact::CoverageGap(g) => DecodeDisposition::InvalidLost(g.clone()),
            _ => return None,
        };
        Self::from_disposition(&event.context.input_context(), &disposition)
    }
}
fn bounded_impact(impact: Impact) -> Impact {
    match impact {
        Impact::Known(mut dims) => {
            dims.sort_by_key(|d| *d as usize);
            dims.dedup();
            Impact::Known(dims)
        }
        other => other,
    }
}
pub(crate) fn loss_reason(reason: LossReason) -> DiagnosticReason {
    match reason {
        LossReason::UnknownSemantics => DiagnosticReason::UnknownSemantics,
        LossReason::Malformed => DiagnosticReason::Malformed,
        LossReason::Truncated => DiagnosticReason::Truncated,
        LossReason::Dropped => DiagnosticReason::Dropped,
    }
}
fn reason_name(reason: DiagnosticReason) -> &'static str {
    match reason {
        DiagnosticReason::UnknownSemantics => "unknown_semantics",
        DiagnosticReason::Malformed => "malformed",
        DiagnosticReason::Truncated => "truncated",
        DiagnosticReason::Dropped => "dropped",
        DiagnosticReason::PermissionDenied => "permission_denied",
        DiagnosticReason::AuxiliaryTerminal => "auxiliary_terminal",
    }
}
/// Unknown strings are never made safe by their field name or lexical shape.
/// Summary vocabulary is closed; the full typed source remains adapter-owned.
pub(crate) fn safe_discriminator(token: Option<&SafeToken>) -> &'static str {
    match token.map(SafeToken::as_str) {
        Some("turn.started") => "turn.started",
        Some("turn.completed") => "turn.completed",
        Some("turn.failed") => "turn.failed",
        Some("model.streaming") => "model.streaming",
        Some("tool.updated") => "tool.updated",
        Some("session.updated") => "session.updated",
        Some("model.request.status") => "model.request.status",
        Some("tool.call.status") => "tool.call.status",
        Some("permission.requested") => "permission.requested",
        Some("permission.resolved") => "permission.resolved",
        Some("result") => "result",
        Some("step_update") => "step_update",
        Some("init") => "init",
        Some("error") => "error",
        Some("item/started") => "item/started",
        Some("item/completed") => "item/completed",
        Some("turn/completed") => "turn/completed",
        Some("session/update") => "session/update",
        Some("transport") => "transport",
        Some("runtime") => "runtime",
        _ => "unrecognized",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GapScope {
    Turn,
    Model(String),
    Tool(String),
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct CurrentGap {
    launch: String,
    turn: String,
    scope: GapScope,
}
#[derive(Debug, Clone, Default)]
pub(crate) struct RuntimeObservation {
    pub(crate) coverage: RuntimeCoverage,
    pub(crate) diagnostics: Vec<DiagnosticSummary>,
    pub(crate) diagnostics_truncated: bool,
    pub(crate) model_activity: Option<ModelActivityView>,
    observed: bool,
    exited: bool,
    gaps: Vec<CurrentGap>,
    overflow: Option<CurrentGap>,
    // Once the bounded overflow spans scopes, individual settlements cannot
    // prove every discarded correlation closed. Keep uncertainty until exit.
    overflow_unscoped: bool,
    frames: HashSet<[u8; 32]>,
    frame_order: VecDeque<[u8; 32]>,
}
impl RuntimeObservation {
    pub(crate) fn end_launch(&mut self) {
        self.exited = true;
        self.gaps.clear();
        self.overflow = None;
        self.overflow_unscoped = false;
        self.model_activity = None;
    }
    pub(crate) fn status(&self) -> TelemetryStatus {
        if self.exited {
            TelemetryStatus::Unavailable
        } else if !self.gaps.is_empty() || self.overflow.is_some() || self.overflow_unscoped {
            TelemetryStatus::Degraded
        } else if self.observed {
            TelemetryStatus::Healthy
        } else {
            TelemetryStatus::Unavailable
        }
    }
    pub(crate) fn record_input(&mut self, input: &InputDiagnostic, now: u64) -> bool {
        if let Some(frame) = &input.frame {
            use sha2::{Digest, Sha256};
            let frame: [u8; 32] = Sha256::digest(
                serde_json::to_vec(&(
                    &frame.task_id,
                    &frame.launch_id,
                    frame.protocol,
                    &frame.session_id,
                    &frame.event_id,
                ))
                .expect("native identity"),
            )
            .into();
            if self.frames.contains(&frame) {
                return false;
            }
            if self.frame_order.len() >= MAX_DIAGNOSTIC_FRAMES {
                if let Some(old) = self.frame_order.pop_front() {
                    self.frames.remove(&old);
                }
            }
            self.frames.insert(frame);
            self.frame_order.push_back(frame);
        }
        self.coverage.gap(&input.impact);
        self.aggregate(DiagnosticSummary {
            source: input.source,
            reason: reason_name(input.reason).into(),
            impact: input.impact.clone(),
            discriminator: input.discriminator.clone(),
            count: 1,
            last_observed_at: now,
        });
        true
    }
    fn aggregate(&mut self, mut item: DiagnosticSummary) {
        if let Some(existing) = self.diagnostics.iter_mut().find(|old| {
            old.source == item.source
                && old.reason == item.reason
                && old.impact == item.impact
                && old.discriminator == item.discriminator
        }) {
            existing.count = existing.count.saturating_add(item.count);
            existing.last_observed_at = existing.last_observed_at.max(item.last_observed_at);
            return;
        }
        if self.diagnostics.len() >= MAX_DIAGNOSTICS - 1 {
            self.diagnostics_truncated = true;
            item.source = SourceProtocol::Runtime;
            item.reason = "summary_limit".into();
            item.impact = Impact::Unknown;
            item.discriminator = "summary_limit".into();
            if let Some(existing) = self
                .diagnostics
                .iter_mut()
                .find(|old| old.reason == "summary_limit")
            {
                existing.count = existing.count.saturating_add(item.count);
                existing.last_observed_at = existing.last_observed_at.max(item.last_observed_at);
                return;
            }
        }
        self.diagnostics.push(item);
    }
    pub(crate) fn projection_loss(
        &mut self,
        dimension: CoverageDimension,
        source: SourceProtocol,
        now: u64,
    ) {
        if self.coverage.complete(dimension) {
            let impact = Impact::Known(vec![dimension]);
            self.coverage.gap(&impact);
            self.aggregate(DiagnosticSummary {
                source,
                reason: "retention_limit".into(),
                impact,
                discriminator: "projection_limit".into(),
                count: 1,
                last_observed_at: now,
            });
        }
    }

    pub(crate) fn correlation_loss(
        &mut self,
        event: &NormalizedEvent,
        dimension: CoverageDimension,
        activity: ActivityScope,
        now: u64,
    ) {
        self.projection_loss(dimension, event.context.source.protocol, now);
        // Retention of an active correlation failed: this is current
        // uncertainty as well as a historical coverage loss.
        self.observe(&NormalizedEvent {
            context: event.context.clone(),
            fact: EventFact::CoverageGap(CoverageGap {
                reason: LossReason::Dropped,
                impact: Impact::Known(vec![dimension]),
                activity,
            }),
        });
    }

    pub(crate) fn inherit_history(&mut self, old: &Self) {
        self.coverage.inherit(&old.coverage);
        let newer = std::mem::take(&mut self.diagnostics);
        self.diagnostics = old.diagnostics.clone();
        for item in newer {
            self.aggregate(item);
        }
        self.diagnostics_truncated |= old.diagnostics_truncated;
        // Native frame identity remains launch-scoped. Never inherit live gaps,
        // requests, turn state, or availability from an ended claim.
    }
    pub(crate) fn promote_turn(&mut self, old: &str, new: &str) {
        for gap in &mut self.gaps {
            if gap.turn == old {
                gap.turn = new.into();
            }
        }
        if let Some(gap) = &mut self.overflow {
            if gap.turn == old {
                gap.turn = new.into();
            }
        }
    }

    pub(crate) fn observe(&mut self, event: &NormalizedEvent) {
        let turn = turn_key(event);
        let launch = identity_digest(&event.context.launch_id.0);
        let same_turn = |gap: &CurrentGap| gap.launch == launch && gap.turn == turn;
        use CoverageDimension::*;
        match &event.fact {
            EventFact::TurnStarted { .. } => {
                self.observed = true;
                self.exited = false;
                self.coverage.support(Turn);
            }
            EventFact::TurnSettled(_) => {
                self.observed = true;
                self.coverage.support(Turn);
                self.gaps.retain(|gap| !same_turn(gap));
                if self.overflow.as_ref().is_some_and(same_turn) {
                    self.overflow = None;
                }
            }
            EventFact::RuntimeExited { .. } => {
                self.exited = true;
                self.gaps.retain(|gap| gap.launch != launch);
                self.overflow = None;
            }
            EventFact::TransportFault { .. } => {
                self.exited = true;
            }
            EventFact::CoverageGap(gap) => {
                self.coverage.gap(&gap.impact);
                if matches!(gap.activity, ActivityScope::None)
                    || !matches!(&gap.impact, Impact::Known(dimensions) if dimensions.iter().any(|dimension| matches!(dimension,
                        CoverageDimension::Tools | CoverageDimension::ModelActivity | CoverageDimension::Turn)))
                {
                    return;
                }
                let (turn, scope) = match &gap.activity {
                    ActivityScope::Turn(id) => {
                        let mut scoped = event.clone();
                        scoped.context.turn_id = Some(id.clone());
                        (turn_key(&scoped), GapScope::Turn)
                    }
                    ActivityScope::ModelRequest(id) => {
                        (turn.clone(), GapScope::Model(identity_digest(&id.0)))
                    }
                    ActivityScope::Tool(id) => {
                        (turn.clone(), GapScope::Tool(identity_digest(&id.0)))
                    }
                    ActivityScope::None => return,
                };
                let current = CurrentGap {
                    launch: launch.clone(),
                    turn: turn.clone(),
                    scope,
                };
                if !self.gaps.contains(&current) {
                    if self.gaps.len() < MAX_CURRENT_GAPS {
                        self.gaps.push(current);
                    } else {
                        let overflow = CurrentGap {
                            launch: launch.clone(),
                            turn: turn.clone(),
                            scope: GapScope::Turn,
                        };
                        if self.overflow.as_ref().is_some_and(|old| old != &overflow) {
                            self.overflow_unscoped = true;
                        }
                        self.overflow = Some(overflow);
                    }
                }
            }
            EventFact::ModelActivityChanged(model) => {
                self.observed = true;
                self.coverage.support(ModelActivity);
                self.model_activity = Some(ModelActivityView {
                    request_id: model.request_id.clone().filter(|id| id.0.len() <= 512),
                    phase: model.phase,
                    retry: model.retry.clone(),
                });
                if matches!(
                    model.phase,
                    ModelPhase::Completed | ModelPhase::Failed | ModelPhase::RetryScheduled
                ) {
                    if let Some(id) = &model.request_id {
                        self.gaps.retain(|gap| {
                            !(same_turn(gap)
                                && gap.scope == GapScope::Model(identity_digest(&id.0)))
                        });
                    }
                }
            }
            EventFact::ToolChanged(tool) => {
                self.observed = true;
                self.coverage.support(Tools);
                if tool.phase == Some(ToolPhase::Finished) {
                    self.gaps.retain(|gap| {
                        !(same_turn(gap)
                            && gap.scope == GapScope::Tool(identity_digest(&tool.call_id.0)))
                    });
                }
            }
            EventFact::MessageUpdated(_) | EventFact::MessageFinished { .. } => {
                self.observed = true;
                self.coverage.support(Text);
            }
            EventFact::PublicReasoningDelta(_) => {
                self.observed = true;
                self.coverage.support(PublicReasoning);
            }
            EventFact::InputRequested(_) | EventFact::InputResolved(_) => {
                self.observed = true;
                self.coverage.support(Input);
            }
            EventFact::SessionReady(info) | EventFact::SessionChanged(info) => {
                for capability in &info.capabilities {
                    let dimension = match capability.capability {
                        Capability::ToolIdentity
                        | Capability::ToolArguments
                        | Capability::ToolPhase
                        | Capability::ToolOutcome => Tools,
                        Capability::PublicReasoning => PublicReasoning,
                        Capability::ModelActivity => ModelActivity,
                        Capability::RespondToPermission | Capability::RespondToQuestion => Input,
                    };
                    if capability.available {
                        self.coverage.support(dimension);
                    }
                }
            }
            _ => {}
        }
    }
}
/// Hashes only explicit native/control identities, never text, arguments,
/// errors or payload content. This bounds retained correlation keys without
/// inventing replay identity for inputs that do not provide one.
fn identity_digest(id: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(id.as_bytes()))
}
pub(crate) fn turn_key(event: &NormalizedEvent) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for id in [&event.context.task_id.0, &event.context.launch_id.0] {
        digest.update(id.len().to_le_bytes());
        digest.update(id.as_bytes());
    }
    match &event.context.turn_id {
        None => digest.update([0]),
        Some(TurnId::Local(id)) => {
            digest.update([1]);
            digest.update(id.to_le_bytes());
        }
        Some(TurnId::Native(id)) => {
            digest.update([2]);
            digest.update(id.as_bytes());
        }
    }
    format!("{:x}", digest.finalize())
}
pub(crate) fn correlation_key(event: &NormalizedEvent, id: &str) -> String {
    format!("{}\0{}", turn_key(event), identity_digest(id))
}
