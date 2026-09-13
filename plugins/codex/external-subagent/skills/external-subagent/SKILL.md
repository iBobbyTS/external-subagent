---
name: external-subagent
description: Manage bounded repository-scoped tasks through external-subagent.
---

# external-subagent

Use the `external_subagent_*` tools to inspect status, spawn a task with an explicit
agent, wait for progress, retrieve the complete result, and close the task. Always
name the agent. This installation supports `zcode` and `dsh` when status reports
them as enabled and spawnable. DSH accepts the advertised build/plan permission
modes; ZCode supports the modes reported by status. Omitting `agent` returns
`agent_required`, disabled or unsupported agents are rejected before a prompt is
dispatched, and unknown spawn fields are rejected.
