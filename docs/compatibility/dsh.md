# DeepSeek Harness ACP compatibility

This document records the bounded S01 probe contract. It is not a claim that DSH is supported by the product.

The probe is `tools/probes/dsh-acp/probe.mjs`. It accepts an explicit executable path and argv, speaks JSON-RPC over stdio, and never installs a provider or writes credentials. `--help` lists the only scenarios. The caller must supply the exact executable and arguments obtained from its installed DSH version; the probe does not invent `dsh app-server`, login, or profile commands.

Observed fixture shapes cover `initialize`, `session/new`, `models/list`, `session/prompt`, `session/update`, `session/request_permission`, `session/cancel`, EOF, and malformed input. `models/list` is catalog evidence only. ACP `initialize` success is not provider authentication evidence; a real auth/hi result must be recorded separately for the exact executable, environment, workspace, and configuration revision.

The product's explicit `agent_models` RPC consumes only the already observed `initialize`, `session/new`, and `models/list` shapes. A `model` option value is the byte-exact JSON tuple `["provider","model"]` (the same bytes JSON.stringify emits); the RPC re-projects such a tuple for display as `provider:model`, while parse failures, non-two-element arrays, and colon-bearing provider sides are returned verbatim. It always cleans up its bounded discovery process. Catalog success does not enable DSH production spawn or imply provider authentication.

Catalog discovery isolates the provider in a process group and cleans the group with TERM/KILL plus leader wait on success and failure. Protocol stdout uses an incremental 1 MiB frame cap, and diagnostic stderr uses a 64 KiB bounded reader with a bounded receive deadline; descendants inheriting stderr cannot hold discovery open.

The shared executable-version check applies the same process-group and incremental-reader bounds, including cleanup when a leader exits while a descendant still owns stdout/stderr.

S04 live status (2026-09-12): **PARTIALLY VERIFIED** against DSH `0.1.5-rc.1` in a disposable workspace. Real ACP `initialize`, `session/new`, and `session/prompt` completed; the default-model prompt returned `LIVE_OK` with `end_turn`. Under the strict patch profile, shell/write probing exposed only the expected `glob`/`grep` operations and created no target file.

These observations validate the ACP wire path and the bounded strict-plan probe. Production daemon routing is implemented behind the explicit `enabled + spawn_supported + DSH_RUNTIME_PATH` gate; Persisted `agents.dsh.runtime_path`, `home`, `profile`, and pinned `version` are carried into the LaunchAgent environment and consumed by the DSH launch profile; unsupported profile/version values fail closed. an isolated daemon smoke completed `spawn → wait → result → close` with `DAEMON_LIVE_OK`, `COMPLETED`, and `resources_reaped=true`. Explicit provider auth/hi probes were executed, but a successful authenticated hi remains **UNVERIFIED** (results below). Codex/ZCode host integration remains **NOT_RUN**. Daemon cancellation is live-verified with reaping and terminal-state preservation across wait/result/close; restart recovery is live-verified as `RUNTIME_LOST` with no replay. The default configuration remains closed; this document does not claim that an unconfigured deployment is enabled.

An isolated strict-plan daemon submission completed through ACP. A prompt that
requested shell/write activity received no shell or write tool and the model
declined to claim the write; the task was reaped and the workspace remained
empty. This confirms the safety refusal path for the installed runtime.

## Launch compositions (implemented)

DSH spawn has three explicit compositions, selected by the daemon factory from
the admitted permission mode and write manifest:

- **strict plan** (`permission_mode: plan`): the strict-plan patch runs DSH with
  `read-only` and every write/execute tool disabled. The write manifest must be
  empty; a non-empty input is refused by admission with `validation` and no
  task is created.
- **build, caller-empty or explicit `["."]`**: the legacy build composition is
  byte-for-byte unchanged — `workspace-write`, no patch, `preflight_build`.
  An explicit `["."]` previously shared the "any non-empty manifest" refusal;
  it is now admitted through this existing composition.
- **build with a non-empty caller manifest**: per task the daemon materializes
  the embedded `dsh-write-guard` package plus a nested
  `node_modules/@deepseek-ai/dsh-fs` symlink into a fresh
  `external-dsh-manifest-` TempDir, writes the S02 patch (`sandbox-policy`
  `workspace-write`; every strict-plan tool except `tool-fs` disabled; one
  `insert` row mounting the guard by absolute path with the caller manifest),
  and requires `preflight_build_manifest` before ACP starts. The guard's
  `fs/write-intent`/`fs/edit-intent` listeners are registered `prepend`, so an
  out-of-manifest path becomes a real `FsError` with code
  `FS_WRITE_MANIFEST_DENIED` (message marker `[write-guard]`) before `tool-fs`
  writes anything. The manifest is bounded to 256 entries / 64 KiB serialized;
  the TempDir is owned by the runtime owner and reaped with the task.

Admission never trusts the child dump: `validate_manifest_build_dump` also
checks the materialized patch's `--dump-config` output (guard name normalized
to a `file://` URL, manifest equality, `tool-fs` enabled, the remaining
strict-plan disable set, exactly one `workspace-write` sandbox policy and the
build approval/permission presets; unknown enabled entries fail closed).

## Model selection via ACP set_config_option (implemented; live NOT_RUN)

The spawn `model` token for dsh is `{provider}:{model}`, split at the
**first** `:`; the model side may itself contain further colons and neither
side is restricted to a character set. Admission (the same code path that the
configured `agents.dsh.default_model` uses) refuses a token with no colon, an
empty provider (`:model`), an empty model (`provider:`), a NUL byte, or more
than 512 bytes, with an error naming the `provider:model` format; the token is
trimmed before validation, as before. After `session/new` and before the first
`session/prompt`, `set_model` re-serializes the parsed sides with serde_json as
the byte-exact two-element JSON string `["provider","model"]` and sends that as
the `session/set_config_option` `value`; the colon token itself is never sent
raw. `input_identity.model` persists the trimmed colon token unchanged, and
`model_source` keeps its `spawn_catalog` / `configured_default` / `native`
semantics. Membership of the provider's real catalog is still decided by the
session: an unknown tuple makes `session_start` fail with `-32602` before any
prompt is sent. Codex keeps its own model id and zcode still rejects model
selection; the reasoning-effort channel is unchanged.

## Reasoning effort via ACP set_config_option (implemented; live NOT_RUN)

The spawn `effort` token for dsh is applied through the same
`session/set_config_option` (X05) contract as model selection: one call
after `session/new` and before the first `session/prompt`, omitted entirely
when no effort was admitted. Three branches, mirroring `set_model`: a token
failing the bounded opaque-token validation refuses with no wire traffic; a
server that advertised `configOptions` without a `reasoning_effort` entry
fails closed as `NotOffered` (no set-option frame is sent); a server that
did not advertise `configOptions` at all gets the set option forwarded and
the server's verdict decides. Every failure sets `effort_refused`, which
permanently blocks `prompt` for that session exactly like a refused model
selection. There is no resume re-application point: the dsh runtime owner
has no resume entry and re-runs the full bootstrap sequence on every claim.
Live verification against a real DSH provider is **NOT_RUN** (user
prohibition on production spawns); the branches are pinned by the ACP
fixture tests.

The explicit daemon probe currently records local discovery as
`READY/0.1.5-rc.1`, auth as `UNKNOWN/auth_not_probed`, and the strict no-tool
hi request as `UNAVAILABLE/remote`; no credentials are modified. A successful
authenticated hi remains unverified.
Auth-only probing does not create a session or send a prompt; hi probing is the
separate strict read-only operation.
