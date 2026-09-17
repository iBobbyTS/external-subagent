# Operations

Installation, service control, Codex binding, and removal for
`external-subagent`. The supported baseline is macOS arm64; see
[compatibility/codex.md](compatibility/codex.md) for the verified Codex CLI
interface.

## Host and subagent instances

The product distinguishes the calling application (`host`) from the managed
execution target (`subagent`). Codex is both a host and a subagent; ZCode and DSH are subagents;
their protocol implementations are internal `adapter`s.

Host access is open to local MCP clients. In addition to the built-in `codex`
host, any local MCP client may act as a `custom` host and call the facade
without a prior registration or a `hosts` entry. The built-in host layer exists
only to provide host-specific installation and automatic-upgrade coordination;
it is never an MCP connection or task-submission prerequisite.

`host.codex` supports multiple installations. Each registered Codex `home` is
an independent host instance and is reconciled, upgraded, inspected, and
unbound separately. A subagent name currently supports only one instance:
`subagents.zcode`, `subagents.dsh`, and `subagents.codex` each describe one runtime/home. The
product does not currently provide same-name subagent instance selection or
multi-instance routing.

`host.custom` has no required persistent instance model: the MCP session is
the connection boundary, and an unregistered client receives the same public
tool surface subject to normal tool and task authorization. It has no Codex
home binding and is not included in host-specific install or auto-upgrade
coordination.

For the built-in `host.codex`, installation has two supported forms: the
managed plugin path (`install-plugin`) and the direct TOML MCP binding
(`install-mcp`). Both target the same facade; the selected Codex home is used
only to scope that host integration and subsequent upgrade reconciliation.

## Install model

A plain npm install only stages the package and its native payload:

- `npm install -g external-subagent` places the CLI (`external-subagent`) and
  the MCP facade (`external-subagent-mcp`) on the npm global bin path.
- The versioned payload lives at
  `<package>/npm/native/darwin-arm64/{external-subagentd,external-subagent-mcp}`
  plus a `payload.json` manifest (version, platform, bytes, sha256, mode).
  Release builds are produced and verified by
  `scripts/release/build-native-payload.mjs`; supported platforms never
  compile Rust at install time.
- Nothing else happens on a fresh install: no daemon start, no Codex writes,
  no subagent probes, and no shell profile edits. The single lifecycle script
  is a `postinstall` bridge that only reads local state: a never-initialized
  install stays stage-only, and an already-initialized installation whose
  package version drifted from its published active payload coordinates
  through the existing update owner (bounded drain, abortable; never a
  subagent probe or a second lifecycle). Installs run with
  `--ignore-scripts` skip the bridge; the same coordination stays available
  through the explicit `update`/`reconcile` commands.

Every state-changing action belongs to an explicit `init`:

```
external-subagent init [--dry-run] [--resume] [--install-hooks]
    [--skip-runtime-probe] [--skip-codex-plugin] [--skip-service-start]
    [--codex-home <path>]
```

`init` steps, in order: verify the staged payload (manifest, digest, mode
755, Mach-O arm64, version agreement between payload/package/CLI), probe the
fixed ZCode runtime, report PATH findings (never write profiles), create the
private data/log directories, write the product config, install the
LaunchAgent, bootstrap the service, stage and register the managed Codex
plugin, claim the Codex home in the D08 registry, and — after all of that —
publish the verified active payload with its retained byte-for-byte copy
under product data, so a successful `init` itself establishes the version and
retention baseline for every later upgrade. The publication reuses the locked
update owner (same verification, retention, and lock rules as `update`), so
the standard sequence `npm A → init A → use A → npm B` never depends on an
extra "A update" step. `--resume` continues
after an environmental failure using the step journal; failures roll tracked
files back — including the product-owned Codex artifacts (staging tree,
marketplace manifest, and directories the run created) and the baseline the
same run published — while never undoing the official codex cache — so a
partial install never looks complete.

Missing DSH never blocks installation; `subagents status` reports it explicitly
(`enabled=false`, `spawn_supported=false`, scope states `UNKNOWN`) and the
product never installs subagent runtimes itself.

When DSH is explicitly enabled, configure its `runtime_path`, `home`, `profile`,
and pinned `version` through the public config command. `init` writes those
values into the LaunchAgent environment, and the DSH adapter consumes and
validates the profile/version rather than relying on the interactive shell.

## PATH behavior

The launchd/GUI environment does not inherit the interactive shell PATH, so
every managed entry is absolute: the LaunchAgent pins the daemon binary, and
the staged plugin `.mcp.json` pins the MCP facade inside the installed
package. The daemon itself runs with the fixed PATH
`/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin`.
`init` reports whether `external-subagent` is on the shell PATH; repairing a
user profile stays an explicit user action (the product never edits shell
startup files).

## Service control

```
external-subagent start   # launchctl bootstrap gui/<uid> <plist> — idempotent
external-subagent stop    # launchctl bootout gui/<uid>/com.external-subagent.daemon — waits for removal
external-subagent status          # essential service/install/daemon readiness
external-subagent status --verbose # add payload, registry, and daemon diagnostics
```

`start` (and init's start-service step) is idempotent: launchd answers a
bootstrap whose label is already loaded in the target domain with
`Bootstrap failed: 5: Input/output error`, so the product first looks the
label up with `launchctl print`; an already-loaded job is reported as
`already_loaded` with its current `state`/`pid` — no second bootstrap, no
second daemon process, and the loaded service is left untouched. A bootstrap
that loses a race to another loader resolves through the same lookup; only a
real control failure surfaces as `DAEMON_CONTROL_FAILED`.

`stop` is idempotent the same way: a job that is not registered reports
`already_stopped`, and a job that a concurrent removal took out mid-stop is
settled through the lookup instead of failing. A bootout only succeeds once
the registration probe confirms the job is actually gone — launchd completes
the removal asynchronously, and a `start` issued in that window can otherwise
be swept away by the still-running teardown (observed live). A job that stays
registered past the bounded deadline fails with `SERVICE_UNLOAD_TIMEOUT`
instead of pretending removal.

A failing init rolls its own service work back symmetrically: a daemon the
failed init itself bootstrapped is booted back out together with the plist
and install state, while a service that was already loaded before the init
is never booted out by the rollback.

`status` separates the launchd view from the RPC view. The default output is
an operational summary: install/data presence, payload verification state,
registered service state, daemon component readiness, subagent enablement and
spawn support, and each probe scope's `state` plus `checked_at_ms`.
`status --verbose` adds diagnostic-only details such as payload file hashes,
registry recovery data, daemon capabilities, service generation, transport and
permission metadata, and daemon identity. `diagnose` remains the bounded
failure/incident report. This keeps routine status readable without removing
the underlying RPC fields used by health checks and upgrades.

The plist template is documented in
`launchd/com.external-subagent.daemon.plist.template` and generated by
`cli/install/service-macos.mjs` with `RunAtLoad`/`KeepAlive`.

## Codex binding and the D08 homes registry

`install-plugin` stages the managed plugin under `~/plugins/external-subagent`
(with an absolute MCP entry and the daemon socket) and registers a local
source marketplace before invoking the official codex CLI (see the
compatibility doc). The plugin manifest's `version` is codex's machine-global
cache identity (`plugin@marketplace@version`): every released candidate must
carry its own version (`0.1.1` from the productization closeout onward), or a
home sharing that identity silently receives another installation's cached
bytes. Existing marketplace entries and Codex config are never
overwritten; drift or foreign ownership is rejected (`PLUGIN_STAGING_CONFLICT`,
`PLUGIN_MARKETPLACE_CONFLICT`). `install-mcp` provides the alternative direct
TOML binding with the same ten-tool surface.

Successful claims are recorded in
`~/Library/Application Support/external-subagent/codex-homes.json` — the single
D08 registry. Uninstalling a plugin or the product releases the claim.
Reconciliation only ever writes homes that are both registered and writable,
reports per-home results, and replaces a corrupted registry atomically while
preserving the damaged bytes for inspection.

A partial home sync never reads as success. When an update activates the new
payload and switches the service but one or more registered homes cannot be
rebound (for example a home that is no longer writable), the command exits
non-zero with `CODEX_SYNC_PARTIAL`, names each non-updated home with its
status and reason, and the activation receipt records `status: "partial"`
with the per-home results. The completed activation is kept — payload,
service, install state, and the registry's per-home statuses are not rolled
back — because homes are independently retryable: after fixing the listed
homes (e.g. permissions), `external-subagent reconcile` finishes the
remaining homes idempotently, and `status` shows each home's `last_status`.

`init` establishes the active payload and the retained-byte baseline; a later
npm install that replaces the package coordinates through the postinstall
bridge above (draining active tasks, payload activation, syncing every
registered Codex copy) via the same update owner. An `--ignore-scripts`
machine runs the identical coordination through the explicit
`update`/`reconcile` commands; read-only commands such as `status` never
trigger it.

## Data, backup, and removal

```
external-subagent backup --output <dir>
external-subagent restore --input <dir>
external-subagent uninstall    # releases Codex claims, boots out + deregisters the service, retains data
external-subagent purge --yes  # explicitly deletes product data
```

`uninstall` first boots the ES-owned service out — with the same bounded
removal confirmation `stop` uses — and only then deletes the LaunchAgent
definition; a service that was not registered is not an error
(`service_already_stopped`). Deleting the plist alone would leave a job
launchd already loaded running until the next logout, so a bootout that
cannot complete fails the command (`SERVICE_UNLOAD_TIMEOUT`) rather than
stranding a running daemon without its definition. Product data, subagent
credentials, and the legacy zcode-as-subagent installation are always
retained; the managed Codex plugin and MCP binding are removed separately by
`install-plugin --uninstall` / `install-mcp --uninstall`. After `npm remove`
and a later reinstall, `init` reclaims the Codex home and restores the
service from the retained data (configuration and database survive
byte-for-byte; only the service config revision advances).

A `restore` replaces the product data directory wholesale while a daemon from
the previous data is still running; that daemon keeps serving its old socket
path, so `status` shows the service registered but RPC unavailable. Run
`stop` then `start` after a restore to bring the daemon back onto the
restored database.

## Release checks

```
node scripts/release/build-native-payload.mjs      # cargo release build + manifest
npm pack                                           # build the tarball
node scripts/release/check-native-tarball.mjs      # static pack checks
node scripts/release/test-installed-tarball.mjs    # controlled-prefix install/init check
```
