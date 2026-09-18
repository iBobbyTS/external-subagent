//! Agent routing composition: each prepared task dispatches to the factory
//! of its admitted agent (legacy tasks keep the ZCode route).

use std::{io, process::Command, sync::Arc};

use external_store::TaskRecord;

use super::factory::DshRuntimeFactory;
use crate::{task_agent, CommandRuntimeFactory, LifecycleSink, ManagedRuntime, RuntimeFactory};

/// Route each prepared task to its agent factory. Legacy tasks without an
/// admission identity keep the ZCode route; DSH and Codex tasks route to
/// their configured factories, which are closed unless the production gates
/// are satisfied.
pub struct RoutingRuntimeFactory<F> {
    zcode: CommandRuntimeFactory<F>,
    dsh: DshRuntimeFactory,
    codex: crate::codex::CodexRuntimeFactory,
}

impl<F> RoutingRuntimeFactory<F> {
    pub fn new(zcode: CommandRuntimeFactory<F>, dsh: DshRuntimeFactory) -> Self {
        Self {
            zcode,
            dsh,
            codex: crate::codex::CodexRuntimeFactory::closed(),
        }
    }

    pub fn with_codex(
        zcode: CommandRuntimeFactory<F>,
        dsh: DshRuntimeFactory,
        codex: crate::codex::CodexRuntimeFactory,
    ) -> Self {
        Self { zcode, dsh, codex }
    }
}

impl<F> RuntimeFactory for RoutingRuntimeFactory<F>
where
    F: Fn(&TaskRecord) -> io::Result<Command> + Send + Sync + 'static,
{
    fn spawn(
        &self,
        task: &TaskRecord,
        sink: Arc<dyn LifecycleSink>,
    ) -> io::Result<Arc<dyn ManagedRuntime>> {
        match task_agent(task).as_str() {
            "zcode" => self.zcode.spawn(task, sink),
            "dsh" => self.dsh.spawn(task, sink),
            "codex" => self.codex.spawn(task, sink),
            agent => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("task routes to unknown agent {agent:?}"),
            )),
        }
    }
}
