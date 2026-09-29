---
name: external-subagent
description: Manage bounded repository-scoped tasks through external-subagent.
---

# external-subagent

Use the `external_subagent_*` tools to inspect status, spawn a task with an explicit
`subagent`, wait for progress, retrieve the complete result, and close the task.
Execution targets are `zcode`, `dsh`, `codex`, and `agy`; each name currently has one
runtime instance and no instance selector is supported. Omitting `subagent` uses
`default_subagent` or returns `subagent_required`; unknown targets return
`subagent_unknown`, and unknown spawn fields are rejected before dispatch.

Spawn is intentionally non-idempotent: every successful call creates a fresh task
and returns `{agent_id, status}` (a duplicate workspace or id is a `conflict`
error, never reuse of an existing task). `status` carries only routing,
capability, and readiness facts (`subagents[]` with `configured`, `enabled`,
`spawn_supported`, permission modes, model selection, and per-scope states);
deployment identity, config revisions, adapter transport detail, and probe
evidence are operator diagnostics — read them with the CLI `diagnose` command
instead of the MCP status tool.

Spawn accepts an optional `effort` token that steers the task's reasoning
effort. Omit it to keep each agent's current default (codex and agy keep their
existing wire default; zcode and dsh send no effort field or call at all).
`codex` admits only the closed set
`low | medium | high | xhigh | max` — `minimal` and `ultra` are rejected;
`agy` admits only the measured closed set `low | medium | high | max`;
`zcode` and `dsh` accept any bounded token (1..24 bytes of `[a-z0-9_]`) and
pass it through to the runtime, because the supported set is only known at
runtime. A malformed or unsupported token is rejected before dispatch, so no
task is created. Example: spawn with
`{"subagent": "zcode", "repository": "/absolute/path", "prompt": "...", "effort": "high"}`.

The caller is a `host`. Any unregistered local MCP client can act as `custom` and
call status or spawn without a host record or Codex home. Built-in `host.codex`
exists for installation and automatic-upgrade coordination, supports multiple
Codex home instances, and offers plugin or direct MCP bindings; built-in
`host.zcode` binds this same plugin through the ZCode config's inline plugin
dirs with a single per-user instance. This is separate
from `subagent.codex`. Protocol implementations are adapters.

All four subagents (`zcode`, `dsh`, `codex`, `agy`) are disabled by default; enable one
with `external-subagent agents enable <name>`, which runs a local probe and writes
the configuration only on success (a daemon restart is required for `dsh`/`codex`/`agy`).
Actual per-subagent limitations (from the accepted code):

- `zcode` — disabled until explicitly enabled; all four permission modes
  (build/edit/plan/yolo); an explicit spawn `model` is rejected
  (`model_selection_unsupported`).
- `dsh` — disabled until explicitly enabled and configured
  (`runtime_path`/`home`/`profile`/`version`); admits only `build` and strict
  `plan`.
- `codex` — disabled until explicitly enabled; all four permission modes:
  `build`/`edit` map to sandbox=workspace-write, `plan` to sandbox=read-only,
  and `yolo` to sandbox=danger-full-access, all pinning approvalPolicy=never.
  A non-empty spawn `write_manifest` is rejected before task creation with
  `codex_write_manifest_unsupported`.
- `agy` (Google Antigravity) — disabled until explicitly enabled with an
  absolute executable `AGY_RUNTIME_PATH`; only `build` (`--mode accept-edits`)
  and `yolo` (`--dangerously-skip-permissions`) are admitted (plan/edit are
  rejected with `agy_permission_mode_unsupported`); `model` is a bare slug
  validated against the `agy models` catalog; a non-empty spawn
  `write_manifest` is rejected before task creation with
  `agy_write_manifest_unsupported`; there is no permission-respond interaction
  (tools are soft-denied).
- `observe` — available for all four subagents; `zcode` and `dsh` return the
  public reasoning tail (at most 200 characters), while `codex` and `agy` do not
  collect reasoning and return `reasoning: null`. Tool history and coverage are
  reported truthfully per adapter capability.
