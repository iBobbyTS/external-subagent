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

## Product binding

- The staged plugin `.mcp.json` pins `command` to the absolute
  `external-subagent-mcp` facade inside the installed npm package and
  `ZCODE_AGENTD_SOCKET` to the daemon socket — never a shell/GUI PATH
  lookup or an nvm-relative path.
- The direct TOML binding (`install-mcp`) writes the same ten-tool
  `mcp_servers.external_subagent` section non-destructively.
- Codex-side `enabled` state is owned by codex; the product never forces or
  clears it for anything it did not install.

## NOT_RUN (not verified in S05)

- Installation into a real user `~/.codex` (requires explicit authorization).
- Tool discovery **inside a running Codex host** (the "ten external tools"
  acceptance is verified against the daemon's MCP endpoint through the real
  facade binary; a live Codex session listing the tools is not exercised).
- Live two-agent tasks driven by a real Codex conversation.
- Registry publication and any `npm publish` flow.
