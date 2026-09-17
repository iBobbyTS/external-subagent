---
name: external-subagent
description: Manage bounded repository-scoped tasks through external-subagent.
---

# external-subagent

Use the `external_subagent_*` tools to inspect status, spawn a task with an explicit
`subagent`, wait for progress, retrieve the complete result, and close the task.
Execution targets are `zcode`, `dsh`, and `codex`; each name currently has one
runtime instance and no instance selector is supported. Omitting `subagent` uses
`default_subagent` or returns `subagent_required`; unknown targets return
`subagent_unknown`, and unknown spawn fields are rejected before dispatch.

The caller is a `host`. Any unregistered local MCP client can act as `custom` and
call status or spawn without a host record or Codex home. Built-in `host.codex`
exists for installation and automatic-upgrade coordination, supports multiple
Codex home instances, and offers plugin or direct MCP bindings. This is separate
from `subagent.codex`. Protocol implementations are adapters.
