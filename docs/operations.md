# Operations

Installation, service control, Codex binding, and removal for
`external-subagent`. The supported baseline is macOS arm64; see
[compatibility/codex.md](compatibility/codex.md) for the verified Codex CLI
interface.

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
  no provider probes, and no shell profile edits. The single lifecycle script
  is a `postinstall` bridge that only reads local state: a never-initialized
  install stays stage-only, and an already-initialized installation whose
  package version drifted from its published active payload coordinates
  through the existing update owner (bounded drain, abortable; never a
  provider probe or a second lifecycle). Installs run with
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

Missing DSH never blocks installation; `agents status` reports it explicitly
(`enabled=false`, `spawn_supported=false`, scope states `UNKNOWN`) and the
product never installs providers itself.

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
external-subagent status  # launchd service view + install + payload + registry + daemon RPC
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

`status` separates the launchd view from the RPC view: the `service` object
reports whether the LaunchAgent is registered in the user's GUI domain and
its running process (`registered`, `state`, `pid`), `daemon_status` reports
what the daemon answers over its socket, and `payload`/`launch_agent`/
`codex_homes` report the installed artifacts and registry — so a
loaded-but-unready or ready-but-unregistered installation reads differently
instead of blurring together.

The plist template is documented in
`launchd/com.external-subagent.daemon.plist.template` and generated by
`cli/install/service-macos.mjs` with `RunAtLoad`/`KeepAlive`.

## Codex binding and the D08 homes registry

`install-plugin` stages the managed plugin under `~/plugins/external-subagent`
(with an absolute MCP entry and the daemon socket) and registers a local
source marketplace before invoking the official codex CLI (see the
compatibility doc). Existing marketplace entries and Codex config are never
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
stranding a running daemon without its definition. Product data, provider
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
