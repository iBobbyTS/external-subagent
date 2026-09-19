# Codex compatibility

Verified interface evidence for the Codex CLI this product binds to. The
current tested baseline is codex-cli **0.154.0**
(`/opt/homebrew/bin/codex`, `codex --version` → `codex-cli 0.154.0`) on
macOS arm64. The command surface below was first verified against 0.153.4
(2026-09-12) and the plugin add/marketplace add paths were re-verified live
against 0.154.0 (2026-09-18); cache-resolution behavior changed between the
two versions and is recorded per version below. All evidence was gathered
with `CODEX_HOME` pointed at throwaway directories; no real user Codex home
was modified while gathering this evidence.

## Plugin interface (verified 2026-09-12)

| Command | Result shape | Notes |
|---|---|---|
| `codex plugin add --help` | help text, exit 0 | availability probe used before any mutation |
| `codex plugin marketplace add <root> --json` | `{marketplaceName, installedRoot, alreadyAdded}` | idempotent; reads `<root>/.agents/plugins/marketplace.json` |
| `codex plugin add <name> --marketplace <marketplace> --json` | `{pluginId, name, marketplaceName, version, installedPath, authPolicy}` | idempotent; installs into `$CODEX_HOME/plugins/cache/<marketplace>/<name>/<version>` |
| `codex plugin remove <name>@<marketplace> --json` | `{pluginId, name, marketplaceName}` | **the bare `plugin remove <name>` form is rejected** (`plugin requires --marketplace unless passed as <plugin>@<marketplace>`); removes the plugin and its cache, keeps marketplace registration |
| `codex plugin list --json` | `{installed: [...], available: [...]}` | per-plugin `enabled`, `source.path`, `marketplaceSource` |

Side effects observed inside `CODEX_HOME`: `config.toml` gains
`[marketplaces.<name>]` (`source_type = "local"`, `source = <root>`) and
`[plugins."<name>@<marketplace>"]` (`enabled = true`); the plugin tree is
copied to the cache path. The product never writes these itself — it stages
the source marketplace and calls the official commands.

## Local marketplace layout (verified)

```
<root>/.agents/plugins/marketplace.json   # {"name": "personal", "plugins": [...]}
<root>/plugins/<plugin-name>/             # source tree with .codex-plugin/, .mcp.json, skills/
```

Marketplace entries reference plugins by relative `source.path`
(`./plugins/<name>`). A marketplace root without
`.agents/plugins/marketplace.json` is rejected (`marketplace root does not
contain a supported manifest`).

## CODEX_HOME semantics (verified; cache resolution re-verified on 0.154.0)

- `CODEX_HOME` confines plugin cache/config writes; it must already exist or
  codex fails (`failed to resolve CODEX_HOME`). The product creates the
  configured home (mode 0700) when claiming it.
- The personal marketplace named `personal` is the default the product
  registers; it does not conflict with an already-registered root (re-adds
  return `alreadyAdded: true`).
- On codex-cli 0.154.0, `plugin add` cache resolution has two verified
  shapes:
  - **Non-reserved marketplace names** resolve to the root registered inside
    the running `CODEX_HOME`, and every `plugin add` **re-materializes the
    cache from that registered root** — verified live by tampering a cached
    file and re-adding: the CLI exited 0 and restored the registered root's
    original bytes, so a tampered or stale cache alone can neither fail an
    add nor serve stale bytes. The 0.153.4-observed model — a frozen
    machine-global content store keyed by `plugin@marketplace@version`
    handing every later home whichever binding first cached the identity —
    no longer holds on 0.154.0 for non-reserved names.
  - **The reserved marketplace name `personal`** (the product's default) is
    resolved machine-globally to the real user root (`~/plugins/<plugin>`
    behind the real `~/.agents/plugins/marketplace.json` registration),
    **ignoring any `personal` registration inside the running `CODEX_HOME`**
    — verified live with a decoy: an in-home `personal` registration
    pointing at a 9.9.9 root was ignored and the real root's 0.1.2 bytes
    were installed byte-identical. `CODEX_HOME` therefore does **not**
    isolate installs made under the reserved name: they read — and cache —
    the REAL root's content. The earlier "store reuse" observation (an
    isolated home's `external-subagent@personal@<version>` cache carrying
    the real home's socket while the staged tree carried the throwaway
    socket) is this reserved-name resolution, not a version-keyed store.

Consequence: the cache path is still
`<codex-home>/plugins/cache/<marketplace>/<name>/<version>`, so the plugin
manifest version **is the cache identity** and stays distinct per released
candidate. Identity `0.1.0` is already cached by the historical
installation, identity `0.1.1` — candidate C1 of the productization
closeout — was itself materialized by the C1 consumer runs, and the final
candidate C2 (`0.1.2`) burned `0.1.2` the same way, so every later release
candidate bumps its own identity again. Verified live during both the C1
and the C2 runs that an isolated home's `external-subagent@personal@<version>`
cache was byte-identical to the staged binding (see
[docs/acceptance/productization.md](../acceptance/productization.md)).
Because versioning alone is a release discipline rather than a runtime
guarantee, `install-plugin` also **reads the materialized cache back before
reporting success** and compares its `.mcp.json` (facade command +
`ZCODE_AGENTD_SOCKET`), its `.codex-plugin/plugin.json` identity, and its
full managed content (file set and bytes) with this run's staged binding;
foreign or unreadable bytes fail closed with
`CODEX_CACHE_BINDING_MISMATCH` / `CODEX_CACHE_CONTENT_MISMATCH`, and a
`plugin add` success whose cache cannot be located at all fails closed the
same way with `CODEX_CACHE_UNVERIFIABLE` — there is no
`installed`/`cache_verified: false` outcome; only a cache verified against
this run's staged binding is ever reported (`cache_verified: true`) or
recorded as installed/claimed/updated. The content check is retained as a
fail-closed defense against freeze-behavior CLIs (the 0.153.4 shape) and
any future regression, but on 0.154.0 non-reserved names it is unlikely to
fire live because codex re-materializes the cache from the registered root
before the verifier reads it; the live reserved-name failure shape on
0.154.0 is `CODEX_CACHE_BINDING_MISMATCH` (the cache resolved to the real
root's different version). The product only ever reads the cache — the
remediation for reused bytes is a distinct release identity (freeze-behavior
CLIs) or refreshing/re-registering the resolved root, and for the reserved
name an isolated home should use a non-reserved marketplace name; codex-owned
state is never edited. The fail-closed paths are pinned by the
store-simulation oracles in `tests/install/codex-binding.test.mjs`.

## Product binding

- The staged plugin `.mcp.json` pins `command` to the absolute
  `external-subagent-mcp` facade inside the installed npm package and
  `ZCODE_AGENTD_SOCKET` to the daemon socket — never a shell/GUI PATH
  lookup or an nvm-relative path.
- The direct TOML binding (`install-mcp`) writes the same ten-tool
  `mcp_servers.external_subagent` section non-destructively.
- Codex-side `enabled` state is owned by codex; the product never forces or
  clears it for anything it did not install.

## Verified since the productization closeout (2026-09-13)

- Tool discovery **and real tool calls inside a running Codex host**, for
  both candidate generations: fresh `codex exec` processes loaded the
  managed plugin from an isolated `CODEX_HOME` and completed real
  `external_subagent_spawn` → `wait` → `result` → `close` MCP calls for
  both providers — first for C1 (`0.1.1`), then for the post-installer-fix
  final candidate C2 (`0.1.2`), whose own fresh consumer verification
  passed (`C2_FRESH_CONSUMER_VERIFICATION_PASS`: `install-plugin` receipt
  `cache_verified: true`, cache byte-identical to the staged binding, all
  four cells COMPLETED) — recorded in
  [docs/acceptance/productization.md](../acceptance/productization.md).

## Cache resolution re-verified on 0.154.0 (2026-09-18)

Two controlled experiments against the real CLI, both confined to
throwaway `CODEX_HOME` directories under `/tmp` (the real user root was
only read; byte-identical before/after):

- **Re-materialization (non-reserved name)**: a marketplace `d2probe`
  pointing at a `/tmp` root whose plugin copy carried version 9.9.9 was
  added; the cache materialized at
  `plugins/cache/d2probe/external-subagent/9.9.9`. After the cached
  `plugin.json` was tampered, a second `codex plugin add` exited 0 and the
  cache held the registered root's original bytes again — the CLI
  re-materialized from the registered root instead of trusting the cache.
- **Reserved-name resolution (`personal`)**: the same 9.9.9 root was also
  registered inside that throwaway home as the marketplace `personal`, then
  `codex plugin add external-subagent --marketplace personal` installed
  **0.1.2** — the real user root's bytes, byte-identical to
  `~/plugins/external-subagent` — ignoring the in-home registration and its
  target entirely. When the same reserved-name install is driven through
  `install-plugin`, this surfaces as a fail-closed
  `CODEX_CACHE_BINDING_MISMATCH` (the materialized cache carries the real
  root's different version/binding) with full staging/marketplace rollback.

## App-server thread posture (verified 2026-09-18, codex-cli 0.154.0)

Probed against `codex app-server --listen stdio://` with throwaway
`CODEX_HOME` directories; no authenticated turn was driven (the one probe
turn failed 401 as expected), so these are transport/posture facts, not
model-behavior facts:

- `thread/start` accepts the string sandbox presets `read-only` and
  `danger-full-access` alongside `approvalPolicy: "never"`. The start
  result resolves the sandbox as an object:
  `{"type":"readOnly","networkAccess":false}` and
  `{"type":"dangerFullAccess"}` respectively.
- `thread/resume` of a read-only thread returns the same read-only object
  (re-verified on 0.154.0; first observed on 0.153.4).
- `thread/resume` of a danger-full-access thread returns the **narrowed
  workspace-write reconstruction**
  `{"type":"workspaceWrite","networkAccess":false,"writableRoots":[],"excludeSlashTmp":false,"excludeTmpdirEnvVar":false}`
  even though the rollout's `session_meta` persists
  `"sandbox_policy":{"type":"danger-full-access"}`. The daemon therefore
  confirms a yolo resume against either the faithful `dangerFullAccess`
  object or that exact narrowed object (never wider: no network, no extra
  writable roots); any other shape fails closed.
- A thread is only resumable after a turn persists its rollout;
  `thread/resume` before that errors with `no rollout found for thread id`.

## NOT_RUN

- Installation into a real user `~/.codex` (requires explicit authorization).
- Live two-agent tasks driven by a real Codex conversation.
- Registry publication and any `npm publish` flow.
