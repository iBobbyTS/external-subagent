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
`subagents.zcode`, `subagents.dsh`, `subagents.codex`, and `subagents.agy` each describe one runtime/home. The
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
  `<package>/npm/native/<platform>/{external-subagentd,external-subagent-mcp}`
  plus a `payload.json` manifest (version, platform, bytes, sha256, mode); the
  staged `<platform>` is `darwin-arm64` on macOS arm64 and `linux-x64` on
  Linux x86_64. Release builds are produced and verified by
  `scripts/release/build-native-payload.mjs`, which derives the platform from
  the running host; supported platforms never compile Rust at install time.
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
    [--skip-service-start]
```

`init` installs the standalone daemon service only. Its steps, in order:
verify the staged payload (manifest, digest, mode 755, the platform's image
— Mach-O arm64 or ELF x86-64, version agreement between payload/package/CLI),
report PATH findings (never write profiles) and an honest ZCode-runtime
observation (`runtime: {path, present}` — never a probe that can fail setup;
the pinned runtime resolves per platform: the app-bundle resource on macOS,
`<home>/.zcode/server/agents/glm/zcode.cjs` — the ZCode desktop's
attached-remote server runtime — on Linux, with an explicit
`ZCODE_RUNTIME_PATH` winning on both), create the
private data/log directories, write the product config, install the service
definition (the launchd LaunchAgent plist on macOS; the systemd user unit on
Linux), start the service, and — after all of that — publish the
verified active payload with its retained byte-for-byte copy under product
data, so a successful `init` itself establishes the version and retention
baseline for every later upgrade. The publication reuses the locked update
owner (same verification, retention, and lock rules as `update`), so the
standard sequence `npm A → init A → use A → npm B` never depends on an extra
"A update" step. `--resume` continues after an environmental failure using
the step journal; failures roll tracked files back — including the product
configuration, the service definition, and the baseline the same run
published — so a partial install never looks complete. A service the failed
init itself started is stopped again by the rollback; a service that was
already running before the init is never touched.

`init` binds no host. The retired `--skip-runtime-probe`,
`--skip-codex-plugin`, and `--codex-home` flags are rejected with an
explanatory error (there is no runtime probe to skip, init never touches
Codex state, and a host home is chosen by the explicit install commands):
bind a host afterwards with `install-plugin codex|zcode` or `install-mcp`.

Missing DSH never blocks installation; `subagents status` reports it explicitly
(`enabled=false`, `spawn_supported=false`, scope states `UNKNOWN`) and the
product never installs subagent runtimes itself.

When DSH is explicitly enabled, configure its `runtime_path`, `home`, `profile`,
and pinned `version` through the public config command. `init` renders the
service template once with those values; after a `config set`, the service
environment refreshes on the next service restart (`stop` + `start`, or an
`update` that activates a new payload), because the daemon re-reads the product
config and re-exports the DSH environment at every startup (`update`/`reconcile`
never re-render the definition; on macOS a `config set` refreshes the plist
in place, while a Linux `config set` leaves the unit to the next
init/update). The DSH
adapter consumes and validates the profile/version rather than relying on the
interactive shell.

## PATH behavior

The launchd/GUI and systemd user-manager environments do not inherit the
interactive shell PATH, so every managed entry is absolute: the service
definition pins the daemon binary, and the staged plugin `.mcp.json` pins the
MCP facade inside the installed package. That staged `.mcp.json` also sets
`timeoutMs: 300000` on the zcode-side server entry, because the zcode host
caps MCP tool calls at a default 30000ms — below the product's 299s
`external_subagent_wait` ceiling. The daemon itself runs with a
platform-owned PATH. On macOS it is the fixed
`/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin`. On Linux it is
the fixed system-tool set
`/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` (covering
systemctl and the payload's system-tool dependencies) plus the interpreter
directory of the Node that rendered the unit (`path.dirname(process.execPath)`),
appended by the unit generator. The fixed prefix keeps system commands
deterministic; the appended directory lets a persisted shim-style subagent
runtime — an nvm-installed Codex Node launcher whose shebang resolves `node`
from PATH — run under the service, mirroring how the macOS fixed PATH carries
the Homebrew bin that holds its own `node`. Persisted subagent runtimes are
still forwarded to the service as absolute paths.
`init` reports whether `external-subagent` is on the shell PATH; repairing a
user profile stays an explicit user action (the product never edits shell
startup files).

## Service control

```
external-subagent start   # macOS: launchctl bootstrap gui/<uid> <plist>
                           # Linux: systemctl --user daemon-reload + enable --now
external-subagent stop    # macOS: launchctl bootout gui/<uid>/com.external-subagent.daemon
                           # Linux: systemctl --user disable --now external-subagent.service
external-subagent status          # essential service/install/daemon readiness
external-subagent status --verbose # add payload, registry, and daemon diagnostics
```

Both backends carry the same contract. `start` (and init's start-service step)
is idempotent: launchd answers a bootstrap whose label is already loaded in
the target domain with `Bootstrap failed: 5: Input/output error`, and systemd
reporting is racy around concurrent starters, so the product first looks the
service up (`launchctl print` / `systemctl --user show`); an already-live
service is reported as `already_loaded` with its current `state`/`pid` — no
second start, no second daemon process, and the loaded service is left
untouched. A start that loses a race to another starter resolves through the
same lookup; only a real control failure surfaces as `DAEMON_CONTROL_FAILED`.

`stop` is idempotent the same way: a service that is not registered reports
`already_stopped`, and a service a concurrent removal took down mid-stop is
settled through the lookup instead of failing. A stop only succeeds once the
backend confirms the removal actually completed — launchd completes a bootout
asynchronously, and a systemd `disable --now` returns before the unit has
necessarily finished its teardown, so the stop polls (bounded) until the job
is gone / the unit is `inactive` with its process exited and the daemon socket
removed. A service that stays up past the bounded deadline fails with
`SERVICE_UNLOAD_TIMEOUT` instead of pretending removal.

On Linux the user manager must be reachable: without a login session
(`$XDG_RUNTIME_DIR`/`$DBUS_SESSION_BUS_ADDRESS` absent), start/stop/init fail
with `NO_USER_SYSTEMD_SESSION` and an explicit remedy (log in, or
`loginctl enable-linger <user>`). The product never enables linger, never
modifies unrelated units, and never contacts the system (PID 1) manager; the
read-only `status` degrades its service view to `unavailable` with the same
hint instead of failing.

A failing init rolls its own service work back symmetrically: a daemon the
failed init itself started is stopped again together with the service
definition and install state, while a service that was already running before
the init is never stopped by the rollback.

`status` separates the backend service view from the RPC view. The default
output is an operational summary: install/data presence, payload verification
state, the registered service's real state (a crashed Linux unit reports
`failed`, never a fake healthy), daemon component readiness, subagent
enablement and spawn support, and each probe scope's `state` plus
`checked_at_ms`. `status --verbose` adds diagnostic-only details such as
payload file hashes, registry recovery data, daemon capabilities, service
generation, transport and permission metadata, and daemon identity.
`diagnose` remains the bounded failure/incident report. This keeps routine
status readable without removing the underlying RPC fields used by health
checks and upgrades.

The macOS plist template is documented in
`launchd/com.external-subagent.daemon.plist.template` and generated by
`cli/install/service-macos.mjs` with `RunAtLoad`/`KeepAlive`. The Linux unit
is generated by `cli/install/service-linux.mjs` into
`~/.config/systemd/user/external-subagent.service` (mode 0600) with
`Restart=always` and `WantedBy=default.target`; its environment forwarding is
one-to-one with the plist, the daemon's `--diagnostic-log` writes
`daemon-error.log` under `~/.local/state/external-subagent/`, and the
daemon's stdout/stderr go to the user journal
(`journalctl --user -u external-subagent.service`).

## Codex binding and the D08 homes registry

`install-plugin` stages the managed plugin under `~/plugins/external-subagent`
(with an absolute MCP entry and the daemon socket) and registers a local
source marketplace before invoking the official codex CLI (see the
compatibility doc). The plugin manifest's `version` is codex's plugin-cache
identity (`plugin@marketplace@version`): it is deliberately independent
of the package version (whose single source is `package.json`) and every
released candidate must carry its own version (`0.1.1` from the
productization closeout onward, currently `0.1.3`), or a home sharing that
identity receives another installation's cached bytes — which `install-plugin`
now rejects by comparing the materialized cache's managed content with the
staged tree (`CODEX_CACHE_BINDING_MISMATCH`/`CODEX_CACHE_CONTENT_MISMATCH`)
instead of trusting identity alone. The default marketplace name `personal`
is reserved: codex resolves it machine-globally to the real user root
regardless of `CODEX_HOME`, so installs under that name always read the real
root's content and isolated-home verification needs a non-reserved
marketplace name. Existing
marketplace entries and Codex config are never
overwritten; drift or foreign ownership is rejected (`PLUGIN_STAGING_CONFLICT`,
`PLUGIN_MARKETPLACE_CONFLICT`). `install-mcp` provides the alternative direct
TOML binding with the same ten-tool surface.

Successful claims are recorded in the product data directory's
`codex-homes.json` — the single D08 registry
(`~/Library/Application Support/external-subagent/` on macOS,
`~/.local/share/external-subagent/` — or an absolute `XDG_DATA_HOME` — on
Linux). Uninstalling a plugin or the product releases the claim.
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

`uninstall` first stops the ES-owned service — with the same bounded
removal confirmation `stop` uses, on both backends — and only then deletes
the service definition; a service that is not registered is not an error
(`service_already_stopped`). Deleting the definition alone would leave a job
the service manager already loaded running until the next logout, so a stop
that cannot complete fails the command (`SERVICE_UNLOAD_TIMEOUT`) rather than
stranding a running daemon without its definition. Product data and subagent
credentials are always retained, and installations owned by other products
are never touched; the managed Codex plugin and MCP binding are removed
separately by
`install-plugin --uninstall` / `install-mcp --uninstall`. After `npm remove`
and a later reinstall, `init` restores the service from the retained data
(configuration and database survive byte-for-byte; only the service config
revision advances), and `install-plugin` reclaims the Codex home
explicitly.

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
