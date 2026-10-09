# ZCode compatibility

Verified interface evidence for the ZCode client this product binds to as
a host (`install-plugin zcode`). All evidence below was gathered on macOS
arm64 against ZCode **25.6.0** (`/Applications/ZCode.app`,
`~/.zcode/cli/`), by inspecting the client's bundled runtime
(`Contents/Resources/glm/zcode.cjs`) and the on-disk state a real GUI
installation produces. No marketplace, cache, or install-record state
owned by the ZCode client is ever written by this product.

Linux evidence (2026-10-09, Ubuntu 26.04 x86_64) is recorded in
"Linux runtime distribution and discovery (observed 2026-10-09)" below and in
[docs/acceptance/S05-linux-subagents-host-bindings.md](../acceptance/S05-linux-subagents-host-bindings.md).

## Restricted spawn environments kill native binaries (verified live)

ZCode starts plugin MCP servers inside a restricted execution context.
Observed against ZCode 25.6.0: the managed plugin was discovered and its
MCP server spawn **attempted** (`mcpServerName:
"plugin:external-subagent:external_subagent"`, transport stdio), but the
pinned native facade binary — an ad-hoc, linker-signed Mach-O
(`codesign -dv` → `Signature=adhoc`, `flags=0x20002(adhoc,linker-signed)`)
— died instantly (client log: `mcp.server.failed` "Connection closed" in
43–49 ms; the settings UI shows 「MCP 进程启动失败」). Spawning the same
binary from ZCode's agent tool sandbox reproduces it exactly: the process
is **SIGKILLed with no stdout/stderr**. Node processes and outbound unix
socket connections are allowed in the very same context (verified: a
node child completed the full MCP handshake against the daemon socket
from inside that sandbox), and official plugins run their MCP servers
through the app-signed `ZCode Helper` executing node scripts.

Consequence: the zcode binding must not point `command` at the native
facade. `install-plugin zcode` pins the **installing node
(`process.execPath`) running the staged
`scripts/mcp-stdio-bridge.mjs`** — a node reimplementation of the native
facade's stdio↔`.mcp`-socket byte pipe (`crates/external-mcp`), with the
same `EXTERNAL_SUBAGENT_SOCKET` → sibling `.mcp` endpoint resolution and
either-side-EOF exit semantics. This mirrors how the product's own hook
installation already pins `process.execPath`. The pinned path follows the
installing interpreter (e.g. a Homebrew Cellar path); `update`/`reconcile`
re-stage the binding with the current interpreter, and a prior binding
pinned to a since-replaced interpreter path refreshes in place — the
staged-bridge args living inside the product-owned staging prove ownership
— instead of failing with `PLUGIN_STAGING_CONFLICT`. The native facade
remains the codex-host entry point, where spawns are unrestricted.

A staging tree carrying the earlier native-command binding from this
product is likewise treated as refreshable (not a conflict) and is
upgraded to the bridge binding in place; foreign commands, or interpreter
bindings whose script lives outside the staging, still fail closed with
`PLUGIN_STAGING_CONFLICT`.

## Why config-based registration (verified)

ZCode has **no headless CLI for plugin management**. The GUI drives
plugin/marketplace operations through internal IPC channels
(`plugins/overview`, `plugins/marketplace/add`,
`plugins/marketplace/remove`, …) exposed only inside the app; there is no
`zcode` binary on `PATH` and no `zcode.cjs plugin …` command surface. A
headless install therefore cannot go through an official CLI the way the
Codex binding shells out to `codex plugin add`.

The client's plugin discovery instead accepts a third source besides
bundled and marketplace plugins: **inline directories** — every path in
`plugins.dirs` (user scope `~/.zcode/cli/config.json`, workspace scope
`.zcode/config.json`) is loaded as a plugin root:

- Discovery pushes each entry as
  `{defaultEnabled: true, marketplace: "inline", rootPath: resolve(entry), source: "inline"}`,
  so the plugin id becomes `external-subagent@inline` and it is enabled
  by default with no `enabledPlugins` entry.
- The config schema (zod, in `zcode.cjs`) validates
  `plugins.dirs` as `array(string().min(1)).optional()`; the same
  `plugins` object holds `enabledPlugins` (`record(string, boolean)`) and
  the master switch `plugins.enabled`.
- This surface is **config-file-driven rather than GUI-driven**: writing
  one directory entry is the entire registration. It touches none of the
  client-owned state under `~/.zcode/cli/plugins/`
  (`known_marketplaces.json`, `marketplaces/`, `cache/`, `data/`).

## Manifest and MCP loading (verified)

- Manifest probe order per plugin root:
  `.zcode-plugin/plugin.json` → `.claude-plugin/plugin.json` →
  `.codex-plugin/plugin.json` → `.cursor-plugin/plugin.json`. The shipped
  plugin tree (`.codex-plugin/plugin.json`) loads as-is.
- Manifest `mcpServers` accepts a **path-string form**
  (`"mcpServers": "./.mcp.json"`): the path is resolved against the
  plugin root and the referenced file's `mcpServers` map is merged
  (array forms merge each entry; inline objects accept either a bare map
  or `{mcpServers: {...}}`). A component path escaping the plugin root is
  rejected as `plugin_component_path_invalid`.
- Plugin-provided MCP servers are trusted and auto-connected at session
  start together with every other scope (user, workspace, environment).
- The manifest declares only `skills` and `mcpServers`; plugin `hooks`
  components are a separate manifest field, so installing this plugin as
  a host binding does not activate the bundled subagent policy hooks
  (those remain the explicit `hooks install` flow).

## Risk envelope

- `plugins.dirs` is a config-schema surface, not a documented public API:
  a ZCode update could stop honoring it. The binding is therefore built
  fail-closed and fully removable: the config merge is atomic and
  preserves every foreign key, unreadable or unrecognized `plugins`
  shapes abort with `ZCODE_CONFIG_INVALID` before any write, and
  `install-plugin zcode --uninstall` removes exactly the one managed
  entry plus the product-owned staging tree
  (`<product data>/zcode-plugin/external-subagent/`: `~/Library/Application
  Support/external-subagent/…` on macOS, `~/.local/share/external-subagent/…`
  on Linux).
  If a client update silently drops inline discovery, the install still
  succeeds and `--uninstall` still cleans up; only the binding's effect
  goes inert.
- The binding takes effect in **new** ZCode sessions; already-running
  sessions do not re-read the config.
- `plugins.enabled: false` in the config disables all plugin loading;
  `install-plugin zcode` still installs and reports a `warning` instead
  of silently binding into a disabled subsystem.
- ZCode model selection materializes a provider environment from four
  undocumented variables (`ZCODE_BUILTIN_PROVIDER_CONFIG_FILE`,
  `ZCODE_BUILTIN_PROVIDER_BUNDLED_CONFIG_FILE`,
  `ZCODE_PERSONAL_PROVIDER_CONFIG_FILE`, `ZCODE_DATA_BASE_DIR`). The
  variable names and the `provider_config` schema they point at are
  internal interfaces, not a public API, and a ZCode update can change
  them. The group is injected atomically and falls back to today's native
  behavior when the builtin template is unreadable. After any ZCode
  upgrade, re-run the model chain: `subagents models zcode` must list the
  expected catalog tokens, and one small task spawned with an explicit
  model must complete with that model. The generated personal file holds
  the API key, so it is written 0600 under the daemon data root and never
  appears in catalog output, evidence, or logs.
- `session/setModel` is the pinned switch protocol: the request always
  sets `persistAsWorkspaceLastUsed: false`, the create result's
  `settings.model.available` is the only whitelist (it shrinks to the
  selected model afterwards), and a remote rejection is classified from
  `error.data.code` (`invalid_model_request` / `model_not_found`) because
  the top-level `code` is always -32603 and the message is only a bounded
  fallback. A resumed session is model-sticky: it is neither re-applied
  nor re-read.

## Session thought level (SOURCE_INSPECTED; live NOT_RUN)

The spawn `effort` token is carried as the optional `thoughtLevel` field of
the session frames (camelCase; the key is omitted entirely when no effort
was admitted), established by inspecting the client's bundled `zcode.cjs`
(25.6.0).

The minified symbol names cited below (`hKe`, `gKe`, `E8e`, `S9t`/`f1s`)
come from the 2026-09-19 08:5x bundle snapshot and **drift with every
client build** — the same-day 09:51 hot update already rebinds them to
unrelated constructs. The behavioral claims were re-confirmed on the new
bundle; when re-verifying, match the stable strings
(`session_create.thought_level_skipped`, the `.strict()` schema shape,
`settings.thoughtLevel.current`) rather than symbol names.

- Both the `session/create` (`hKe`, 08:5x snapshot) and `session/resume`
  (`gKe`, 08:5x snapshot) request
  schemas parse an optional `thoughtLevel` via `.strict()`.
- An unsupported value is **silently skipped**: the client emits a
  `session_create.thought_level_skipped` notice and proceeds. The supported
  set is the selected model's `optionSpecs.reasoningLevel.values`, known
  only at runtime — which is why this product admits effort for zcode as a
  bounded passthrough token rather than a closed set.
- The effective level reads back from
  `result.settings.thoughtLevel.current`; the settings projection includes
  `current` only when the effective value is in the supported list (the
  `E8e` projection, 08:5x snapshot).
- **Resume parses but ignores `thoughtLevel`** — the parsed field
  (`S9t`/`f1s`, 08:5x snapshot) has no consumer on the resume path
  (`setThoughtLevel`'s
  consumers are the create flow, `setModel`/fork, and the `setThoughtLevel`
  command). The daemon still sends the field on resume and still guards the
  read-back fail-closed, because the settings read-back reflects what the
  session actually runs.
- The daemon fail-closes with an `InvalidSession` error carrying the bare
  code `EFFORT_MISMATCH` (mirroring the existing `MODEL_MISMATCH` posture)
  when an explicit effort was admitted, the read-back exists, and the two
  differ. A missing read-back is diagnostic-only and passes through — the
  silent-skip notice above makes a missing `current` the expected shape
  whenever the requested level was not applied.
- Live read-back verification against a real ZCode session is **NOT_RUN**
  (user prohibition on production spawns); the chain above is
  static-inspection evidence pinned by the fake-runtime protocol tests.

## Linux runtime distribution and discovery (observed 2026-10-09)

There is no `/Applications/ZCode.app` on Linux. The observed ZCode Linux
form is the **desktop attached-remote server runtime** the ZCode desktop
deploys to the host it drives over SSH (observed on Ubuntu 26.04 x86_64,
desktop `ZCODE_APP_VERSION=3.14.5`):

- `~/.zcode/server/` is the runtime root; the desktop exports
  `ZCODE_SERVER_RUNTIME_ROOT` pointing at it, and the official
  `~/.zcode/server/agents/glm/zcode-agent` launcher is a POSIX shell script
  that resolves the root as `${ZCODE_SERVER_RUNTIME_ROOT:-$HOME/.zcode/server}`
  and execs `<root>/node <root>/agents/glm/zcode.cjs "$@"`.
- `<root>/node` is a bundled Node (v22.16.0 observed); the runtime script is
  `agents/glm/zcode.cjs` (`zcode.cjs --version` → `0.16.9`;
  `agents/glm/.version` → `0.13.3`). `~/.zcode/cli/` holds only client state
  (config, sessions, plugins), never an executable runtime — probing it for a
  runtime would be inventing a probe point.

**Product discovery** (`zcodeRuntimePath` in `cli/constants.mjs`, consumed by
the service generators and the runtime observations report): explicit
configuration first — `ZCODE_RUNTIME_PATH`, the same variable the daemon
itself resolves — then the platform's conventional location (the app bundle
on darwin, `<product home>/.zcode/server/agents/glm/zcode.cjs` on Linux),
then the packaged macOS constant is reported honestly as an absent pin.
Forwarding to the daemon stays presence-gated exactly as before. The
deployment's `ZCODE_SERVER_RUNTIME_ROOT` is deliberately **not** consulted:
service generation must stay independent of the interactive shell
environment (the same design the fixed unit PATH follows), and the product
home already owns `~/.zcode`. The daemon runs the script with the `node` on
its PATH (the systemd unit PATH carries the unit-rendering Node's bin
directory since S04); `zcode.cjs --version` prints the same `0.16.9` under
the nvm Node v24.17.0 and the bundled v22.16.0.

**Live verification** (isolated product HOME/XDG, real runtime, throwaway
credentials copied then deleted): `agents enable zcode` probes
`local.state=READY`, `version=0.16.9`, and a full
`spawn → wait → result → close` chain with `permission_mode=plan` completed
with `final_text="pong"` (`input_identity.model_source="native"`). The
runtime writes its own logs/state under the spawn environment's `~/.zcode`
(the isolated home), never the real tree.

**Read-only probe layers on this runtime line — two gaps found and fixed
(2026-10-09, both re-verified live end-to-end after the fix):**

1. The staged scope policy verifier
   (`plugins/codex/external-subagent/scripts/policy-verifier.mjs`) joined
   its provenance as the macOS `<home>/Library/Application Support/…` path
   unconditionally, so on Linux it exited 2 and every `--hi`/`models` probe
   reported `policy_unverified` (the daemon-side lookup was already
   platform-aware). The script now derives the provenance candidates with
   the same semantics as the daemon's `policy_verifier_candidates`: the
   frozen macOS bytes on darwin; elsewhere an exported absolute
   `$XDG_DATA_HOME/<product>/` first, then the `<home>/.local/share/<product>`
   XDG fallback. An existing installation picks the fixed script up on the
   next `hooks install`, which restages the verifier beside the provenance.
2. `session/create` on this runtime issues a new server request
   `interaction/requestOfficialMcpAuthHeaders` (official-MCP auth, e.g. for
   the `image-search` plugin) and proceeds with an anonymous fallback no
   matter how the client answers. The daemon's read-only probe loops
   (request wait, turn wait, event drain) answered unrecognized server
   requests with `-32601` and aborted the wait, misclassifying a healthy
   create as `transport`/`create_failed`. They now answer method-not-found
   and keep waiting — the same safe-ignore posture applied to unknown
   notifications — while a genuinely failed request still surfaces through
   its own error response or the probe deadline
   (`respond_unsupported_probe_request`, pinned by the
   `zcode_catalog_survives_an_unsupported_server_request_during_create`
   unit test). After both fixes, the live chain on the real runtime reads:
   `subagents models zcode` → `supported: true` with the projected catalog,
   and `subagents probe zcode --hi` → local/auth/hi all `READY`.

Further protocol observations recorded for this runtime line (no product
change): no `initialize` method (`-32601 Method not found`);
`session/requestRuntimePreferences` arrives with scope
`runtime-materialization`; the create result reports
`session.mode`/`settings.mode.current` as `build` for a `plan` request
(the env-level `ZCODE_AGENT_PERMISSION_MODE=plan` policy still held for the
completed plan-mode turn).

## Oracles

`tests/install/zcode-binding.test.mjs` pins the fail-closed config merge
(foreign keys preserved byte-for-byte in value, broken JSON untouched,
foreign same-name plugin dirs conflicting, foreign staging trees
refused), the idempotent refresh, the native→bridge staging upgrade, the
stateless reconcile detection, and the uninstall behavior. The staged
bridge itself is exercised live by the machine verification: spawning
the exact pinned `command`/`args`/`env` from inside the restricted
context completes initialize → tools/list (all ten tools) → a real
`external_subagent_status` call.

## send 双模式投递

`external_subagent_send` 的 mode 必填，仅接受 queue/steer；缺失或非法值以 validation 拒绝。

活跃 queue 由 es 暂存，在既有 turn 边界经 session/send 投递。活跃 steer 先 session/stop，边界落定后 session/send 新 turn。空闲时两种 mode 均普通发送。原生并发 session/send 的实测 -32010 拒绝是暂存兜底的依据。

接入新 subagent 时原生 mid-turn 投递优先于 es 暂存，es 暂存是文档化的标准兜底。queued 仅表示 es 暂存；原生直写／注入完成即 delivered，执行结果仍由 wait/result 查询。同 message_id、同 mode、同 content 重试幂等，改变任一绑定字段会冲突。
