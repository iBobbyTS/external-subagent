# Agent discovery, probe, and status

Agent status is a passive projection of configuration plus the latest evidence produced by an explicit probe. Reading `system_status` does not inspect an executable, start a provider, authenticate, or send a prompt. An agent with no explicit probe therefore reports `UNKNOWN`, `checked_at_ms` omitted, and reason `not_probed` for `local`, `auth`, and `hi`.

Every per-agent status entry also exposes the current configuration revision and capability identity. `configured` says that the provider has a configuration entry; `transport_support` names the wire protocol and separately states whether probe and production spawn are available; `permission_modes` lists the modes currently admitted for spawn; and `model_selection` distinguishes native-only model choice from opaque catalog-token selection. ZCode reports `zcode_app_server`, all four permission modes, and `{supported:false,mode:"native_only"}`. DSH reports `dsh_acp` with probe support, no currently admitted spawn permission modes, and `{supported:false,mode:"catalog_token"}`; its model selection, production `spawn`, and top-level `spawn_supported` remain gated until S04 accepts the adapter.

The daemon RPC method `agent_probe` accepts an agent (`zcode` or `dsh`), a `through` layer (`local`, `auth`, or `hi`), and an exact scope containing optional absolute `workspace` and `home` paths. Each returned layer contains `state`, the exact `scope`, `version`, `checked_at_ms`, and `reason`. Scoped evidence remains attached to the workspace/home that produced it; callers must not treat it as evidence for another workspace or home.

Probe evidence records the configuration revision read immediately before probe execution. Status only projects it as current when that revision still matches the active configuration. After any configuration revision change, the old layers become `UNKNOWN` with reason `stale_config_revision` until the caller explicitly probes again.

The stable failure reasons are:

- `missing`: no configured executable or the configured path is not a file.
- `version`: the executable exists but its version cannot be established.
- `transport`: the runtime cannot start, speak the pinned protocol, or settle a valid session.
- `auth_401`: the provider rejected the production route as unauthorized.
- `network`: the production route timed out or reported a network failure.
- `rate_limit`: the provider reported a rate limit; this is degraded evidence rather than valid authentication.
- `policy_violation`: the probe runtime emitted a tool or permission request despite the read-only probe policy.

For ZCode, a `local` probe establishes executable/version evidence only. An `auth` probe remains `UNKNOWN` with `auth_requires_hi`, because the pinned runtime does not expose an independent credential check. A `hi` probe uses the same app-server protocol as production with an explicit `plan` session mode, an empty write manifest, and the daemon policy environment bound to the probe workspace. The bounded prompt also tells the model not to call tools. Any tool lifecycle or permission request fails the probe rather than being approved.

Auth is not promoted when the turn merely starts. Both auth and hi become `READY` only after the terminal `turn.completed` event. A terminal failure, child exit, or diagnostic tail is classified after settlement, so asynchronous 401, rate-limit, and network failures update both layers. If the caller omits workspace for a hi probe, the daemon creates a mode-0700 disposable workspace, records that generated absolute path as the evidence scope, and removes the directory after the probe. Evidence from that disposable path cannot be reused for another workspace.

Lifecycle and diagnostic events received while a command response is still pending are retained by the probe. After the response arrives, terminal classification consumes those retained events first; a provider that emits `turn.completed` or `turn.failed` before the matching send response therefore settles immediately with the original success or failure reason.

If a failed terminal event is followed by a generic RPC error or process exit, the cached provider classification wins. An observed `auth_401`, `rate_limit`, `network`, or `policy_violation` is never replaced by a later generic `transport` outcome.

For DSH, local discovery may use an explicitly configured `DSH_RUNTIME_PATH`, but this release has no accepted production adapter. Auth and hi remain `UNKNOWN` with `dsh_production_adapter_unavailable`, and `spawn_supported` is always `false`. The S01 fixture is protocol evidence only and is never promoted to live auth or hi evidence.
