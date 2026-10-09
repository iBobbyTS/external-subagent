# external-subagent

A local service that manages bounded, repository-scoped external subagent
tasks and exposes them over MCP. Hosts (the applications that call the MCP
tools) and subagents (the execution targets that run the tasks) are two
independent axes:

- **Hosts** — Codex (`install-plugin codex` or `install-mcp`), ZCode
  (`install-plugin zcode`), and any custom local MCP client that connects
  directly to the facade without registration.
- **Subagents** — ZCode, DeepSeek Harness (DSH), Codex, and Google
  Antigravity (`agy`), each routed through an internal adapter and subject to
  the per-subagent limitations listed in [docs/product.md](docs/product.md)
  and [docs/mcp-api.md](docs/mcp-api.md).

## Status

The package is published to the public npm registry as
`external-subagent@0.1.0` for macOS arm64 and Linux x86_64. `os`/`cpu` in
`package.json` are independent npm arrays, so npm admits any darwin/linux +
arm64/x64 tuple at install time; the CLI then enforces the real support at
first use — a host outside macOS/Linux is rejected with `UNSUPPORTED_PLATFORM`
before any HOME write, and `darwin-x64`/`linux-arm64` (no native payload) are
rejected at `init` with `UNSUPPORTED_PAYLOAD_PLATFORM`.
`init` installs the standalone daemon service only — no host is bound
implicitly; hosts are bound through the explicit `install-plugin` (Codex or
ZCode) / `install-mcp` (Codex-only TOML binding) commands. Installing from a
packed artifact and explicitly initializing to a service is live-verified on
macOS arm64 (launchd, real GUI session) and on Linux x86_64 (systemd user
service, Ubuntu; idempotent repeat init/start); active-task upgrade and
standalone initialization are implemented and reviewed. On Linux x86_64 the
`codex` and `zcode` subagents are live-verified (OBSERVED), while `dsh` and
`agy` remain runtime-gated there: their runtimes are not installed on the
verification host, so they stay disabled by default with an explicit status,
exactly as on macOS.

All four subagents are **disabled by default** and are enabled per name with
`external-subagent agents enable <zcode|dsh|codex|agy>`: a successful local probe
writes the configuration (no write on failure), and `dsh`/`codex`/`agy` need a
daemon restart to take effect. ZCode admits all four permission modes
(build/edit/plan/yolo) and rejects an explicit spawn `model`; DSH admits
`build` and strict `plan`, with model selection as explicit spawn model,
configured default, then the native default, and its `build` mode admits a
non-empty caller `write_manifest` through the guarded manifest-build
composition (workspace-write sandbox, `tool-fs`-only writes, out-of-manifest
paths rejected as `FS_WRITE_MANIFEST_DENIED`; an explicit `["."]` keeps the
legacy build composition and `plan` still requires an empty manifest); Codex
admits all four modes
(build/edit → workspace-write, plan → read-only, yolo → danger-full-access,
all pinning `approvalPolicy=never`) and rejects a non-empty `write_manifest`
with `codex_write_manifest_unsupported`; Antigravity (`agy`) spawns only when
its `enabled + spawn_supported` gate and an absolute `AGY_RUNTIME_PATH`
executable admit it, runs with `build` (`--mode accept-edits`) or `yolo`
(`--dangerously-skip-permissions`), selects a catalog-validated bare model
slug, admits effort from `low|medium|high|max`, and rejects a non-empty
`write_manifest` with `agy_write_manifest_unsupported` (it has no
permission-respond interaction — tools are soft-denied and denied actions
surface on failure diagnostics). `observe` works on all four: ZCode
and DSH expose the public reasoning tail (at most 200 characters) and Codex
and agy report `reasoning: null`.

The daemon socket environment variable is now the neutral
`EXTERNAL_SUBAGENT_SOCKET`, with no fallback to the retired name. An existing
installation upgrading to this version must run the public
`external-subagent update` (or `reconcile`) once to rewrite the host
bindings, then restart the service; see
[docs/operations.md](docs/operations.md). The historical productization
acceptance matrix is recorded in
[docs/acceptance/productization.md](docs/acceptance/productization.md).

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

## Install

```
npm install -g external-subagent     # stages package + payload only (macOS arm64, Linux x64)
external-subagent init               # explicit: standalone daemon service only
external-subagent install-plugin zcode   # optional host binding (or: install-plugin codex / install-mcp)
```

Packing and publishing are gated by npm lifecycle scripts: `prepack` always
rebuilds the native payload from source (so a tarball can never silently ship
stale or missing binaries) and `postpack` runs the static tarball checks. A
development checkout packs the same way (`npm pack` after `cargo` is
available); see [docs/operations.md](docs/operations.md) for release checks.

## Spawn profiles

A spawn profile is a named JSON preset for the spawn inputs, stored as one
JSON file per profile in the `profiles/` directory next to `config.json`
(`~/Library/Application Support/external-subagent/profiles/` by default;
`profile list` reports the exact path in use). Every `*.json` file in the
directory is discovered by its `name` field; the same name defined in two
files is rejected, and invalid files are reported as warnings and excluded
from the available set.

```json
{
  "name": "codex-yolo",
  "subagent": "codex",
  "model": "gpt-5",
  "permission_mode": "yolo",
  "developer_instructions": "Always reply in English."
}
```

| Field | Meaning |
|---|---|
| `name` | Required, unique across files; trimmed, non-empty, at most 128 bytes |
| `subagent` | `zcode`, `dsh`, `codex`, or `agy` |
| `model` | Model token, named per target: `dsh` wants `provider:model`, `zcode` accepts `provider/model` or a bare token, `codex` and `agy` take a bare slug |
| `effort` | Effort token; some runtime models require one (for example `agy` rejects `gemini-3.8-flash` without `--effort`) and the spawn error surfaces the runtime's message verbatim |
| `permission_mode` | `build`, `edit`, `plan`, or `yolo`; defaults to `build` when unset |
| `developer_instructions` | Free-form instructions, delivered as described below |

`--profile` cannot be combined with `--subagent`, `--model`, `--effort`, or
`--permission_mode`; put those in the profile JSON or omit `profile`. Fields
left out of the profile fall back to the same defaults as an unprofiled
spawn. `developer_instructions` rides the codex runtime's native
`developerInstructions` channel when the effective subagent is `codex`; every
other subagent receives it silently concatenated in front of the prompt:

```
Developer Instructions: {developer_instructions}
----------
{prompt}
```

Inspect what is loaded with `external-subagent profile list` and
`external-subagent profile show <name>`; an empty `profile list` result
carries a `hint` field with this format summary.

## Publishing

Releases publish from CI via npm Trusted Publishing (OIDC): pushing a `vX.Y.Z`
tag runs [.github/workflows/npm-publish.yml](.github/workflows/npm-publish.yml).
One tarball carries both platform payloads, so the workflow builds them on two
runners: the `build-linux` job builds the `linux-x64` payload on Ubuntu and
uploads it as an artifact, and the `publish` job runs on an Apple Silicon
runner (where `prepack` rebuilds the `darwin-arm64` payload), merges the Linux
payload into `npm/native/` before the pack gates and release-path test suites
run, and publishes with no npm token in the environment — the runner's OIDC
identity is the credential. npm does not support trusted publishing for a
package's first release, so the initial version was published manually once
and the trusted publisher was then linked on npmjs.com; the workflow filename
is part of that link and must not be renamed.

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
data. Supported platforms: macOS arm64 and Linux x86_64. npm's independent
`os`/`cpu` arrays only bound the tuple to darwin/linux + arm64/x64 (so
`darwin-x64` and `linux-arm64` install as well); the CLI keeps `help`/`version`
working everywhere, rejects a host outside macOS/Linux with
`UNSUPPORTED_PLATFORM`, and rejects `darwin-x64`/`linux-arm64` at `init` with
`UNSUPPORTED_PAYLOAD_PLATFORM`, never writing HOME on a rejected host. macOS arm64 is live-verified
(launchd). Linux x86_64 (Ubuntu, systemd user service) is verified for the
install/init/service/CLI path and the codex and zcode subagents; `dsh` and
`agy` remain runtime-gated there (their runtimes are not present on the
verification host).

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

The JavaScript install/upgrade suites build the native facade through
`scripts/release/build-native-payload.mjs`; set
`EXTERNAL_SUBAGENT_CARGO_PROFILE=debug` to make that entry build from
`target/debug` (no `--release`) and record `profile: debug` in the staged
manifest. The default stays a release build for `prepack`/publishing, and
`npm run check` is the JavaScript syntax gate.
