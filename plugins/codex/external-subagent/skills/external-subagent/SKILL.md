---
name: external-subagent
description: Manage bounded repository-scoped tasks through external-subagent.
---

# external-subagent

Use the `external_subagent_*` tools to inspect status, spawn a task with an explicit
agent, wait for progress, retrieve the complete result, and close the task. Always
name the agent. `zcode` is currently the only spawnable agent; omitting `agent`
returns `agent_required`, and selecting `dsh` returns `agent_unsupported` before
the prompt is dispatched. Unknown spawn fields are rejected.
