# ZCode compatibility

Verified interface evidence for the ZCode client this product binds to as
a host (`install-plugin zcode`). All evidence below was gathered on macOS
arm64 against ZCode **25.6.0** (`/Applications/ZCode.app`,
`~/.zcode/cli/`), by inspecting the client's bundled runtime
(`Contents/Resources/glm/zcode.cjs`) and the on-disk state a real GUI
installation produces. No marketplace, cache, or install-record state
owned by the ZCode client is ever written by this product.

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
  (`~/Library/Application Support/external-subagent/zcode-plugin/external-subagent/`).
  If a client update silently drops inline discovery, the install still
  succeeds and `--uninstall` still cleans up; only the binding's effect
  goes inert.
- The binding takes effect in **new** ZCode sessions; already-running
  sessions do not re-read the config.
- `plugins.enabled: false` in the config disables all plugin loading;
  `install-plugin zcode` still installs and reports a `warning` instead
  of silently binding into a disabled subsystem.

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
