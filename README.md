# external-subagent

A tool that exposes Zcode and DeepSeek Harness as subagents for Codex to use
as MCP.

## Status

Feature branch `codex/external-subagent-productization-closeout`. DSH and
ZCode installed-artifact lifecycles, active-task upgrade, and managed Codex
binding are implemented and reviewed; installing from a packed artifact and
explicitly initializing to a launchd-resident service (idempotent repeat
init/start) is live-verified on a real GUI session. The productization
candidate additionally passed the four-cell consumer matrix — DSH/ZCode ×
public CLI / real Codex CLI via the managed plugin over MCP — against one
tarball (see [docs/acceptance/productization.md](docs/acceptance/productization.md));
the managed plugin manifest is versioned per released candidate because its
version is codex's plugin-cache identity (currently `0.1.1`). DSH admission
currently supports `build` and strict `plan`; its model selection is explicit
spawn model, configured default, then the native default. Nothing has been
published to a registry (`REGISTRY_PUBLICATION_PENDING`).

## Install (from a packed artifact)

```
npm pack                                                # build the tarball (payload must be staged first)
node scripts/release/build-native-payload.mjs           # cargo release build + payload manifest
npm install -g <external-subagent-0.1.0.tgz>            # stages package + payload only
external-subagent init                                  # explicit: service, Codex binding, D08 claim
```

A plain install stages the package and payload only. On an already initialized
installation, npm's postinstall hook detects a local version drift and reuses
the existing reconcile/update owner; it never initializes a fresh home or
probes a provider. `init` remains the explicit first activation step. A
completed update whose registered Codex homes only partially rebind reports
`CODEX_SYNC_PARTIAL` per home and keeps the verified activation; the public
`stop` confirms launchd removal before returning, and `uninstall` boots out
the ES-owned service before removing its registration while retaining all
data. Supported platform: macOS arm64; other platforms keep `help`/`version`
working and reject business commands without writing HOME.

See [docs/operations.md](docs/operations.md) for service control, PATH
behavior, the Codex homes registry, backup/removal, and release checks; and
[docs/compatibility/codex.md](docs/compatibility/codex.md) for the verified
codex-cli interface. See [docs/product.md](docs/product.md) for the product
workflow, provider configuration, and CLI/MCP acceptance boundary.

## Development

```
cargo test --workspace
node --test tests/install/*.test.mjs tests/platform/*.test.mjs tests/cli/*.test.mjs tests/contract/*.test.mjs
node --test tests/integration/*.test.mjs tools/probes/dsh-acp/probe.test.mjs
```
