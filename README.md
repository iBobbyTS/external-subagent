# external-subagent

A tool that exposes Zcode and DeepSeek Harness as subagents for Codex to use
as MCP.

## Status

Feature branch `codex/external-subagent-v1`; sections S01–S04 implemented and
reviewed, S05 (npm fresh install + managed Codex binding) implemented. The
DSH production spawn gate remains closed pending live evidence, and nothing
has been published to a registry.

## Install (from a packed artifact)

```
npm pack                                                # build the tarball (payload must be staged first)
node scripts/release/build-native-payload.mjs           # cargo release build + payload manifest
npm install -g <external-subagent-0.1.0.tgz>            # stages package + payload only
external-subagent init                                  # explicit: service, Codex binding, D08 claim
```

A plain install never starts the daemon, writes Codex, probes providers, or
edits shell profiles — those belong to the explicit `init`. Supported
platform: macOS arm64; other platforms keep `help`/`version` working and
reject business commands without writing HOME.

See [docs/operations.md](docs/operations.md) for service control, PATH
behavior, the Codex homes registry, backup/removal, and release checks; and
[docs/compatibility/codex.md](docs/compatibility/codex.md) for the verified
codex-cli interface.

## Development

```
cargo test --workspace
node --test tests/install/*.test.mjs tests/platform/*.test.mjs tests/cli/*.test.mjs tests/contract/*.test.mjs
node --test tests/integration/*.test.mjs tools/probes/dsh-acp/probe.test.mjs
```
