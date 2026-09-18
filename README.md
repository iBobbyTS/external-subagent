# external-subagent

A local service that manages bounded, repository-scoped external subagent
tasks and exposes them over MCP. Hosts (the applications that call the MCP
tools) and subagents (the execution targets that run the tasks) are two
independent axes:

- **Hosts** — Codex (`install-plugin codex` or `install-mcp`), ZCode
  (`install-plugin zcode`), and any custom local MCP client that connects
  directly to the facade without registration.
- **Subagents** — ZCode, DeepSeek Harness (DSH), and Codex, each routed
  through an internal adapter and subject to the per-subagent limitations
  listed in [docs/product.md](docs/product.md) and
  [docs/mcp-api.md](docs/mcp-api.md).

## Status

Current work happens on the `audit/external-subagent-20260917` branch; the
productization closeout below is the historical record of that stage. DSH and
ZCode installed-artifact lifecycles, active-task upgrade, and standalone
initialization are implemented and reviewed. `init` installs the standalone
daemon service only — no host is bound implicitly; hosts are bound through the
explicit `install-plugin` (Codex or ZCode) / `install-mcp` (Codex-only TOML
binding) commands. Installing from a packed artifact and explicitly
initializing to a launchd-resident service (idempotent repeat init/start) is
live-verified on a real GUI session. Both productization
candidates passed the four-cell consumer matrix — DSH/ZCode × public CLI /
real Codex CLI via the managed plugin over MCP, each against its own tarball:
the pre-fix candidate C1 (`0.1.1`, historical) and the post-installer-fix
final candidate C2 (`0.1.2`), whose fresh consumer verification passed
through the public install surface — all four cells COMPLETED with the
plugin cache verified against the staged binding (`cache_verified: true`;
see [docs/acceptance/productization.md](docs/acceptance/productization.md)).
The managed plugin manifest is versioned per released candidate because its
version is codex's plugin-cache identity, and codex resolves the reserved
`personal` marketplace name machine-globally to the real user root
(regardless of `CODEX_HOME`), so each candidate's consumer runs pin that
identity's bytes there and any later candidate bumps again. The installer
verifies the materialized plugin cache against the staged binding and
managed content (file set and bytes, not just identity) before reporting
success, failing closed on cache-reused or stale bytes. DSH
admission currently supports `build` and strict `plan`; its model
selection is explicit spawn model, configured default, then the native
default. Nothing has been published to a registry
(`REGISTRY_PUBLICATION_PENDING`).

## Versioning

The product version has a single source: `package.json` (`0.1.0`). The CLI
constant (`cli/constants.mjs`), the daemon crates, and the native payload
manifest all carry the same version, and `scripts/release/check-native-tarball.mjs`
enforces payload/package agreement. The managed Codex plugin manifest
(`plugins/codex/external-subagent/.codex-plugin/plugin.json`, currently
`0.1.3`) is deliberately **not** tied to the product version: it is codex's
plugin-cache identity (`plugin@marketplace@version`; under the reserved
`personal` marketplace name codex resolves the real user root
machine-globally, ignoring `CODEX_HOME`), which
must carry a fresh version per released candidate (`0.1.1` for C1, `0.1.2`
for C2, `0.1.3` for the boundary-fixes candidate) so no home silently
receives another installation's cached bytes. The two numbers therefore
intentionally diverge; see
[docs/operations.md](docs/operations.md) for the full rule.

## Install (from a packed artifact)

```
node scripts/release/build-native-payload.mjs           # cargo release build + payload manifest (must run BEFORE npm pack)
npm pack                                                # build the tarball from the freshly staged payload
node scripts/release/check-native-tarball.mjs           # static tarball checks (entries, payload/package version, Mach-O)
npm install -g <external-subagent-0.1.0.tgz>            # stages package + payload only
external-subagent init                                  # explicit: standalone daemon service only
external-subagent install-plugin codex                  # optional: bind the Codex host (or: install-plugin zcode / install-mcp)
```

A plain install stages the package and payload only. `init` never binds a
host or touches Codex state; host binding is a separate explicit step
(`install-plugin codex|zcode`, or `install-mcp` for the direct Codex TOML
binding), and any custom local MCP client may connect without either. On an
already initialized installation, npm's postinstall hook detects a local
version drift and reuses
the existing reconcile/update owner; it never initializes a fresh home or
probes a provider. A same-version reinstall does not drift and therefore
does not trigger that update path — rebind hosts explicitly with
`external-subagent reconcile` and restart the service per
[docs/operations.md](docs/operations.md) instead of expecting npm reinstall
to refresh a running daemon. `init` remains the explicit first activation
step. A
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

See [docs/mcp-api.md](docs/mcp-api.md) for the complete MCP tool reference,
field usage, defaults, design rationale, and omission/removal impacts.

## Development

```
cargo test --workspace
node --test tests/install/*.test.mjs tests/platform/*.test.mjs tests/cli/*.test.mjs tests/contract/*.test.mjs
node --test tests/integration/*.test.mjs tools/probes/dsh-acp/probe.test.mjs
```
