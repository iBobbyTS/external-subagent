# Codex compatibility

Verified interface evidence for the Codex CLI this product binds to. All
commands below were executed against codex-cli **0.153.4**
(`/opt/homebrew/bin/codex`, `codex --version` → `codex-cli 0.153.4`) on
macOS arm64, with `CODEX_HOME` pointed at throwaway directories; no real
user Codex home was modified while gathering this evidence.

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

## CODEX_HOME semantics (verified)

- `CODEX_HOME` confines plugin cache/config writes; it must already exist or
  codex fails (`failed to resolve CODEX_HOME`). The product creates the
  configured home (mode 0700) when claiming it.
- The personal marketplace named `personal` is the default the product
  registers; it does not conflict with an already-registered root (re-adds
  return `alreadyAdded: true`).
- `plugin add` materializes the installed cache from a **machine-global
  content store keyed by `plugin@marketplace@version`**. When another home on
  the machine (e.g. the real `~/.codex`) already caches the same identity, a
  fresh `CODEX_HOME` receives the store's bytes, not the staged tree's —
  observed live: the cached `.mcp.json` carried the real-home socket while
  the staged tree carried the throwaway socket. Consequence: the plugin
  manifest version **is the cache identity** and must be distinct per
  released candidate. The managed plugin is versioned `0.1.1` from the
  productization closeout onward (identity `0.1.0` is already cached by the
  historical installation); verified live that an isolated home's
  `external-subagent@personal@0.1.1` cache was byte-identical to the staged
  binding (see
  [docs/acceptance/productization.md](../acceptance/productization.md)).

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

- Tool discovery **and real tool calls inside a running Codex host**: fresh
  `codex exec` processes loaded the managed plugin (`0.1.1`, enabled) from an
  isolated `CODEX_HOME` and completed real `external_subagent_spawn` →
  `wait` → `result` → `close` MCP calls for both providers — recorded in
  [docs/acceptance/productization.md](../acceptance/productization.md).

## NOT_RUN

- Installation into a real user `~/.codex` (requires explicit authorization).
- Live two-agent tasks driven by a real Codex conversation.
- Registry publication and any `npm publish` flow.
