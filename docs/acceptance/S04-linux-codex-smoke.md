# S04 acceptance — Codex subagent Linux smoke (gpt-6-luna)

Section S04 of `external-subagent-linux-full-20261009`: the first real Codex
`spawn → wait → result → close` chain driven by the external-subagent CLI on
Linux, plus the Linux/codex-cli 0.162.0 wire observations. Every fact below is
**OBSERVED** on the host recorded in the environment table; all Codex
interaction used a throwaway `CODEX_HOME` and an isolated product HOME/XDG tree.
No credential value was read, printed, or committed.

## Environment (OBSERVED, 2026-10-09, times UTC)

| Item | Value |
|---|---|
| OS | Ubuntu 26.04 LTS, x86_64, kernel 7.0.0-29-generic |
| Node | v24.17.0 — `/home/ibobby/.nvm/versions/node/v24.17.0/bin/node` |
| Codex CLI | `codex-cli 0.162.0`, `/home/ibobby/.nvm/versions/node/v24.17.0/bin/codex` → `../lib/node_modules/@openai/codex/bin/codex.js` (Node launcher) |
| product HOME | `/tmp/es-s04-iso/home` |
| product XDG | `XDG_DATA_HOME=/tmp/es-s04-iso/xdg-data`, `XDG_STATE_HOME=/tmp/es-s04-iso/xdg-state`, `XDG_CONFIG_HOME=/tmp/es-s04-iso/xdg-config` |
| CODEX_HOME (throwaway) | `/tmp/es-s04-codex-home` |
| daemon binary | `/home/ibobby/external-subagent/npm/native/linux-x64/external-subagentd`, sha256 `bfcf7862…e64b`, payload version `0.4.1` (linux-x64) |

`init --skip-service-start` verified the linux-x64 payload, created the XDG data
and log directories, wrote `config.json`, and wrote the systemd unit at
`/tmp/es-s04-iso/xdg-config/systemd/user/external-subagent.service`. Service
start was skipped so the daemon ran attached under fully controlled env. The
real user `~/.codex` was only read (to copy `auth.json`/`config.toml` into the
throwaway home with mode 0600) and was never modified by the product.

## 1. Fail-closed reverse example (verified first)

Config: `subagents.codex.runtime_path` = the absolute nvm Codex path,
`enabled=true`, `spawn_supported=true`, **`home` unset**. Daemon A environment:
isolated HOME/XDG, `PATH` including the nvm bin, **no `CODEX_HOME`**.

```
$ external-subagent spawn --json '{"subagent":"codex","repository":"/tmp/es-s04-iso/ws","prompt":"reply with the single word pong","permission_mode":"build","model":"gpt-6-luna"}'
{"ok":false,"error":{"code":"agent_unsupported","message":"codex home is unconfigured"}}
```

The refusal is synchronous at admission **before any task row is created**:

```
$ external-subagent list --json '{"repository":"/tmp/es-s04-iso/ws"}'
{"tasks":[],"next_cursor":null}
```

Real `~/.codex` zero-write oracle around this step: `auth.json` and
`config.toml` sha256 identical before/after; the only inventory deltas were the
concurrently-running host Codex's own `logs_2.sqlite` WAL mtimes (the daemon ran
with isolated HOME and an isolated store, and refused before spawning any child).

## 2. PATH discovery and absolute-path persistence (`agents enable codex`)

Daemon B environment: `CODEX_HOME=/tmp/es-s04-codex-home`, `PATH` including the
nvm bin. `agents enable codex` ran the real local probe through the daemon:

- `local.state = READY`, `local.version = 0.162.0`,
  `local.runtime_path = /home/ibobby/.nvm/versions/node/v24.17.0/bin/codex`
  (absolute, discovered on PATH), `local.scope.home = /tmp/es-s04-codex-home`.
- Persisted config (revision 5): codex `enabled=true`, `spawn_supported=true`,
  absolute `runtime_path`, `home=/tmp/es-s04-codex-home`, `version=0.162.0`;
  `restart_required=true`.

After restarting the daemon (C1) with **no `CODEX_HOME`/`CODEX_RUNTIME_PATH` in
its process environment** (the persisted config supplies both via
`configure_codex_environment`), `subagents probe codex` reported `READY`
version 0.162.0 with the same home — the persisted-config launch contract works.

## 3. Live full chain (spawn → wait → result → close)

Model `gpt-6-luna`, workspace `/tmp/es-s04-iso/ws`, prompt
`reply with the single word pong`.

| permission_mode | agent_id | thread/session_id | outcome | final_text | close |
|---|---|---|---|---|---|
| build | 10000000 | `01a12235-7b56-7e22-9970-c79be985b42e` | COMPLETED | `pong` | closed |
| plan | 10000001 | `01a12235-b274-72c3-8279-3b6675f3c39d` | COMPLETED | `pong` | closed |
| yolo | 10000002 | `01a12235-d1aa-7070-84b0-74c1cf40a309` | COMPLETED | `pong` | closed |

`result.input_identity` for each: `subagent=codex`, `adapter_version=0.4.1`,
`model=gpt-6-luna`, `model_source=spawn_catalog`, `effort=null`,
`workspace_path=/tmp/es-s04-iso/ws`, `permission_mode=<mode>`.

`observe` (build and plan) reported `"reasoning": null` per the README contract,
`count_scope=agent_lifetime`, empty tool history, `dropped_events=0`.

## 4. Permission-mode mapping (source + wire + live)

Source: `crates/subagents/codex/src/session.rs` — `Plan→"read-only"`,
`Build|Edit→"workspace-write"`, `Yolo→"danger-full-access"`; every mode pins
`approvalPolicy="never"`; the start/resume result must confirm the resolved
sandbox object or the adapter fails closed.

Wire (`tools/probes/codex-app-server/probe.mjs`, scenario `posture`, codex-cli
0.162.0, throwaway home): requested preset → resolved echo, all matching the
objects `sandbox_confirmed` accepts:

| requested `sandbox` | resolved echo in `thread/start` result |
|---|---|
| `workspace-write` (build/edit) | `{"type":"workspaceWrite","writableRoots":[],"networkAccess":false,"excludeTmpdirEnvVar":false,"excludeSlashTmp":false}` |
| `read-only` (plan) | `{"type":"readOnly","networkAccess":false}` |
| `danger-full-access` (yolo) | `{"type":"dangerFullAccess"}` |

`approvalPolicy` echoed `"never"`, `cwd` and `model` (`gpt-6-luna`) echoed at
the result root for all three. The three live tasks above completed, which means
the daemon's fail-closed posture confirmation passed on 0.162.0 for
workspace-write, read-only, and danger-full-access end-to-end.

## 5. `subagents models codex` 口径

```
$ external-subagent subagents models codex
{"ok":true,...,"supported":false,"models":[],"evidence":{"source":"codex_app_server_model_list",...},"reason":"codex_models_probed_live_only"}
```

No in-band catalog claim is made; the model token `gpt-6-luna` was accepted by
`spawn` (`model_source=spawn_catalog`) and used for all three live turns.

## 6. codex-cli 0.162.0 protocol observations

Detailed in [docs/compatibility/codex.md](../compatibility/codex.md) ("Linux
x86_64, codex-cli 0.162.0"). Summary vs the 0.154.0 macOS baseline: the
`thread/start` sandbox/approvalPolicy/cwd/model echo contract, the `turn/start`
response `/turn/id`, and the `item/*` + `turn/*` folding the daemon consumes are
unchanged; `initialize` gained `userAgent`/`platformFamily`/`platformOs`, and
several new notifications (`thread/status/changed`, `thread/tokenUsage/updated`,
`account/updated`, `account/rateLimits/updated`, `remoteControl/status/changed`,
`mcpServer/startupStatus/updated`) are emitted and safely ignored by the daemon.

## 7. FINDING (resolved in S04) — systemd fixed PATH could not execute an nvm-installed Codex shim

**Pre-fix observation.** The S03 systemd unit pinned
`PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` (no Node).
The nvm `codex` is a Node launcher (`codex.js`, shebang `#!/usr/bin/env node`).
Running the unit's environment verbatim:

```
$ env -i PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    /home/ibobby/.nvm/versions/node/v24.17.0/bin/codex --version
env: 'node': No such file or directory
exit 127
```

Through the product (daemon started with the unit's fixed PATH and persisted
config, no `CODEX_*` in env):

```
$ external-subagent subagents probe codex      # local.state = DEGRADED, reason "version"
$ external-subagent spawn --json '{...codex...}'
{"ok":false,"error":{"code":"runtime_lost","message":"SESSION_START_FAILED: {...,\"stderr_tail\":\"env: 'node': No such file or directory\\nenv: use -[v]S to pass options in shebang lines\\n\",...}"}}
```

This was a runtime-resolution/environment gap, not an adapter protocol drift.

**Fix (bounded, S04 owner).** `cli/install/path.mjs` gains
`systemdServicePath()` = `SYSTEMD_FIXED_PATH` + `:` + `path.dirname(process.execPath)`
(the interpreter directory of the Node that rendered the unit); both
`cli/install/service-linux.mjs` (unit `Environment="PATH=…"`) and
`pathReport().systemd.path` use it. The darwin plist is byte-unchanged. The
appended directory is absolute/newline-free so `assertUnitLineSafe` admits it.
This mirrors the macOS fixed PATH carrying the Homebrew bin that holds its own
`node`.

## 8. Systemd-hosted re-verification (post-fix, OBSERVED)

Fresh isolated install (`HOME=/tmp/es-s04-sysd/home`, XDG under
`/tmp/es-s04-sysd`, throwaway `CODEX_HOME=/tmp/es-s04-codex-home2`). `init
--skip-service-start` wrote the unit with the fixed PATH; its exact byte line:

```
Environment="PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/home/ibobby/.nvm/versions/node/v24.17.0/bin"
```

Boundary note: the real user manager's unit search path is the real
`~/.config/systemd/user` (manager HOME `/home/ibobby`, no `XDG_CONFIG_HOME`), so
an isolated `XDG_CONFIG_HOME` unit is invisible to it and writing the real HOME
is out of boundary. The daemon was therefore hosted as a genuine systemd user
service via `systemd-run --user` (transient unit `es-s04-codex-systemd.service`)
with this exact unit `PATH` and the isolated HOME/XDG/CODEX_HOME — a real
`systemctl --user` service, not an attached process:

```
$ systemctl --user show es-s04-codex-systemd -p ActiveState -p MainPID
ActiveState=active
MainPID=117990
$ systemctl --user status es-s04-codex-systemd   # CGroup during a turn:
  /user.slice/user-1000.slice/user@1000.service/app.slice/es-s04-codex-systemd.service
   ├─117990 external-subagentd --database … --socket … --diagnostic-log …
   ├─118403 node /home/ibobby/.nvm/versions/node/v24.17.0/bin/codex app-server --listen stdio://
   └─118415 …/codex-linux-x64/vendor/x86_64-unknown-linux-musl/bin/codex app-server --listen stdio://
```

The daemon spawned the Node shim and the vendored native codex inside the
systemd unit cgroup — the fix's target behavior. `agents enable codex` (probe
READY 0.162.0) then `spawn`/`wait`/`result`/`close`:

| mode | agent_id | outcome | final_text | close |
|---|---|---|---|---|
| build | 10000000 | COMPLETED | `pong` | closed |
| plan | 10000001 | COMPLETED | `pong` | closed |

`observe` reported `"reasoning": null`. Cleanup: product `stop` →
`already_stopped`; transient unit stopped/collected; product `uninstall` →
`removed_service_definition=true`, `data_retained=true`. No real
`~/.config/systemd/user/external-subagent.service` was ever created (that
directory still holds only the pre-existing `codex-app-server.service`), and the
real `~/.codex` `auth.json`/`config.toml` sha256 are unchanged from the session
start. The copied credential was deleted from the throwaway home.

## Reproduction

```
/tmp/es-s04/cli.sh init --skip-service-start
/tmp/es-s04/cli.sh config set subagents.codex.runtime_path <abs codex>
/tmp/es-s04/cli.sh config set subagents.codex.enabled true
/tmp/es-s04/cli.sh config set subagents.codex.spawn_supported true
# daemon A (no CODEX_HOME): spawn → agent_unsupported
# daemon B (CODEX_HOME=/tmp/es-s04-codex-home): agents enable codex → READY + persist
# daemon C1 (config-supplied CODEX_HOME/RUNTIME_PATH): spawn/wait/result/close build|plan|yolo
# systemd-hosted (unit PATH): systemd-run --user --unit=es-s04-codex-systemd \
#   --setenv=PATH=<unit PATH> --setenv=HOME=<iso> --setenv=CODEX_HOME=<throwaway> <daemon> --database… --socket…
node tools/probes/codex-app-server/probe.mjs --executable <codex> --codex-home <throwaway> \
  --workspace <ws> --model gpt-6-luna --scenario posture|turn
```

Evidence artifacts (raw command outputs, redacted wire): `/tmp/es-s04-evidence/`
(`init.json`, `failclosed-spawn.json`, `enable-codex.json`, `spawn-build.json`,
`wait-build.json`, `result-build.json`, `observe-build.json`,
`spawn-plan.json`, `probe-posture.json`, `probe-turn.json`,
`probe-codex-fixedpath.json`, `spawn-fixedpath.json`, `sysd-init.json`,
`sysd-unit.txt`, `sysd-enable-codex.json`, `sysd-probe-codex.json`,
`sysd-spawn-build.json`, `sysd-wait-build.json`, `sysd-result-build.json`,
`sysd-close-build.json`, `sysd-spawn-plan.json`, `sysd-wait-plan.json`,
`sysd-result-plan.json`, `sysd-close-plan.json`, `sysd-stop.json`,
`sysd-uninstall.json`,
`real-codex-home.*.tsv`).
