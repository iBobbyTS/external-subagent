# Windows compatibility

Live-verification evidence for the Windows host. The S04 gate removal makes
Windows a first-class native platform (`win32-x64`) alongside macOS arm64 and
Linux x86-64: the CLI platform gate, the native payload tuple, and the npm
`os` field all include it. This document records what was actually observed on
a real Windows 11 machine. It is an evidence report, not a promise of every
third-party CLI version above the pins below.

**DSH is explicitly DEFERRED.** The installed DSH is `0.2.0-rc.2` while the
product pins `PINNED_DSH_VERSION = 0.1.5-rc.1`
(`crates/subagents/dsh/src/profile.rs`), so the version gate stays closed by
design. DSH is not part of the Windows compatibility claim in this round;
`agents.dsh` admission behavior is unchanged. The Owner will decide the
version upgrade separately.

## Test machine

| Item | Value |
|---|---|
| CPU | AMD Ryzen 9 7900X 12-Core Processor |
| RAM | 50,576,478,208 bytes (~47.1 GiB) |
| OS | Microsoft Windows 11 Pro for Workstations, version 10.0.26200, build 26200, x64 |
| Node | v24.13.0 |
| npm | 11.6.2 |
| Rust | rustc 1.96.0 (ac68faa20 2026-05-25), `x86_64-pc-windows-msvc` |
| Toolchain | Visual Studio 2022 (MSVC) |
| Shell | Git Bash; service control via `C:\Windows\System32\sc.exe` |

## Agent runtimes and pins

| Agent | Runtime | Version | Discovery |
|---|---|---|---|
| Codex | `C:\Users\iBobb\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe` | `codex-cli 0.162.1`, logged in | PATH (`codex.exe`) and persisted `runtime_path` |
| ZCode | `C:\Users\iBobb\AppData\Local\Programs\ZCode\resources\glm\zcode.cjs` | `0.16.9`, logged in | daemon `--runtime` (`node <zcode.cjs> app-server`) |
| Antigravity (`agy`) | `C:\Users\iBobb\AppData\Local\agy\bin\agy.exe` | `1.3.5`, logged in | **not on PATH**; persisted `runtime_path` (`AGY_RUNTIME_PATH`) |
| DSH (deferred) | `C:\Users\iBobb\AppData\Roaming\npm\dsh.cmd` | `0.2.0-rc.2` | not enabled: pin is `0.1.5-rc.1` |

The three enabled agents were persisted with `enabled + spawn_supported` (and
`runtime_path` for codex/agy). The daemon was started in the development
configuration:

```text
target/debug/external-subagentd.exe \
  --database <temp>\state.sqlite3 \
  --socket \\.\pipe\external-subagent-s04-probe-<n> \
  --runtime C:\Users\iBobb\AppData\Local\Programs\ZCode\resources\glm\zcode.cjs \
  --agent-config <temp>\config.json
```

`--runtime` is canonicalized and projected back from the Windows verbatim
spelling (`\\?\C:\...` -> `C:\...`), which Node requires; the daemon starts and
reports `runtime=READY`.

## Probe evidence (native JSON, 2026-10-16)

Probes were run through the public command layer (`subagents probe <agent>
--hi`) over the Named Pipe wire method `agent_probe`. Each command exited 0 and
produced a complete JSON document. The three evidence packets (abridged from
the full `evidence`/`status` objects) are:

### Codex

```json
{
  "subagent": "codex",
  "config_revision": 1,
  "local": {
    "state": "READY",
    "runtime_path": "C:\\Users\\iBobb\\AppData\\Local\\Programs\\OpenAI\\Codex\\bin\\codex.exe",
    "version": "0.162.1"
  },
  "auth": { "state": "UNKNOWN", "reason": "auth_not_probed" },
  "hi": { "state": "UNKNOWN", "reason": "hi_not_probed" },
  "spawn_supported": true,
  "transport": "codex_app_server",
  "permission_modes": ["build", "edit", "plan", "yolo"]
}
```

Codex local evidence is executable/version evidence. The product deliberately
does not claim an in-band auth/hi layer for Codex (`auth_not_probed`,
`hi_not_probed`); the protocol handshake is exercised by a real spawn/wait in
the final acceptance run. **Full `create --subagent codex --model
gpt-6-luna` + `wait` evidence is owned by the final main acceptance step.**

### ZCode

```json
{
  "subagent": "zcode",
  "config_revision": 1,
  "local": {
    "state": "READY",
    "runtime_path": "C:\\Users\\iBobb\\AppData\\Local\\Programs\\ZCode\\resources\\glm\\zcode.cjs",
    "version": "0.16.9"
  },
  "auth": { "state": "READY", "reason": null },
  "hi": { "state": "READY", "reason": null },
  "spawn_supported": true,
  "transport": "zcode_app_server",
  "permission_modes": ["build", "edit", "plan", "yolo"]
}
```

The `hi` probe runs the same app-server protocol as production with an explicit
`plan` session, an empty write manifest, and the daemon policy environment.
`auth` becomes READY only after the hi session succeeds (ZCode exposes no
independent credential endpoint). The probe required the staged policy
verifier: on Windows `install-agent-hooks.mjs` writes it to
`%LOCALAPPDATA%\external-subagent\external-subagent-policy-verifier`, and both
the CLI staging and the daemon lookup use the `%LOCALAPPDATA%` root (falling
back to `%USERPROFILE%\AppData\Local`). The verifier itself runs under `node`
on Windows because it is an extensionless Node script.

### Antigravity (`agy`)

```json
{
  "subagent": "agy",
  "config_revision": 1,
  "local": {
    "state": "READY",
    "runtime_path": "C:\\Users\\iBobb\\AppData\\Local\\agy\\bin\\agy.exe",
    "version": "1.3.5"
  },
  "auth": { "state": "UNKNOWN", "reason": "auth_not_probed" },
  "hi": { "state": "READY", "reason": null },
  "spawn_supported": true,
  "transport": "agy_stream_json",
  "permission_modes": ["build", "yolo"]
}
```

The hi probe launches the bare stream-json session with no permission flags
(agy's default soft-deny posture), sends a bounded no-tool prompt, and only
concludes READY on a `SUCCESS` result with no tool step. agy has no independent
auth endpoint; `auth` stays UNKNOWN by design.

## PATHEXT executable resolution

Windows has no POSIX execute bit. The daemon now validates configured runtime
paths and PATH discoveries through a single executable owner
(`crates/external-daemon/src/executable.rs`):

- `PATHEXT` is parsed as a semicolon-separated, case-insensitive, dot-prefixed
  set; entries without a dot are normalized and duplicates removed.
- An unset or blank `PATHEXT` uses the documented default
  `.COM;.EXE;.BAT;.CMD`.
- A configured `runtime_path` must be a regular file whose extension is in the
  set (an extensionless stem is rejected).
- PATH discovery tries the exact name first, then `name + each PATHEXT
  extension` in directory order, mirroring the Windows loader.
- `.cmd`/`.bat` artifacts are executed through `cmd.exe /D /C` in the probe
  layer; Codex and agy are `.exe`, so this path is reserved for future/DSH
  runtimes.

Unit coverage (run on this machine): default set, custom set normalization and
deduplication, case-insensitive membership, extension requirement, PATH
discovery with `.exe`/`.cmd` and rejection of `.txt`/missing names, custom
`PATHEXT` driving gate membership, and batch wrapping in `cmd.exe`.

## Permission interception matrix

The ZCode probe and production hook surface use the same policy:

| Surface | Rule | Failure mode |
|---|---|---|
| `PreToolUse` matcher `^(Read\|Grep\|Glob\|Write\|Edit\|Delete\|Move)$` | staged file-policy wrapper + provenance hashes | probe: `policy_violation`; production: hook denies |
| `PreToolUse` matcher `Bash` | guard wrapper (read-only probe session) | any permission request fails the probe |
| `PostToolUse` / `PostToolUseFailure` matcher `Bash` | audit wrapper records outcome | audit only; never grants |
| ZCode `hi` probe policy | explicit `plan` session, empty write manifest, no tool calls requested | `policy_unverified` before spawn; `policy_violation` on any tool/permission event |
| agy `hi` probe | bare stream-json, no permission flags | any tool `step_update` (even soft-denied) fails the probe |
| Codex | no in-band hi layer | local executable/version only |

On Windows the staged wrappers are invoked with `process.execPath` and the
verifier path is resolved through the same `%LOCALAPPDATA%` rules as the daemon.

## Session reconnect

Measured on the real Named Pipe transport:

1. First daemon (PID 2653) served `status`: `components.daemon = READY`,
   `payload = verified (win32-x64)`, enabled agents `zcode/codex/agy`.
2. The daemon was terminated; the same pipe name was re-bound by a second
   daemon (PID 14396).
3. `status` over the same `\\.\pipe\external-subagent-s04-probe-<n>` succeeded
   again: `ok: true`, `components.daemon = READY`, payload `win32-x64`, agents
   `zcode:true, dsh:false, codex:true, agy:true`.

This verifies transport reconnection after a daemon restart (client
`net.createConnection` on the pipe plus a fresh server instance). Per-task
session resume semantics are owned by the agent protocols and are recorded in
the respective `codex.md` / `zcode.md` / `antigravity.md` documents; this round
did not re-run a live resume of an interrupted task.

## Termination signals

- `process.kill(pid, 'SIGTERM')` from Node terminates the daemon on Windows
  (the OS maps it to process termination); the process disappears from
  `tasklist` immediately after the call.
- The daemon also installs a Windows console control handler; the unit test
  `console_control_tests::console_events_flip_the_shutdown_flag_and_are_acknowledged`
  verifies that the handler only flips the shared shutdown flag and
  acknowledges the event, leaving the graceful shutdown to the main loop.
- Deep process-tree termination (Job Object, `KILL_ON_JOB_CLOSE`, PID-reuse
  `start_token`) is the S02 owner; see
  `.agent-work/handoffs/windows-real-machine-completion/S02-HANDOFF.md`.
  This section did not re-run the S02 tree-kill matrix.

## CLI command surface on win32

- `status` exits 0 without a daemon and reports `daemon_error.code =
  SOCKET_UNAVAILABLE`; the SCM view degrades to `query: available,
  registered: false` for an unregistered service.
- `init --dry-run` plans the win32 install; a full `init` writes only
  `%LOCALAPPDATA%\external-subagent` and creates no launchd/systemd tree.
- `create/wait/list/send/respond/cancel/result/close` pass the platform gate
  and answer `SOCKET_UNAVAILABLE` when no daemon is listening.
- `subagents probe <agent> --hi` reaches `agent_probe` (the error is a daemon
  connection error, never `UNSUPPORTED_PLATFORM`).
- Service commands (`start/stop/uninstall/purge`) dispatch to the SCM backend;
  lifecycle is exercised through the injectable `sc.exe` seam because this
  shell is not elevated. Elevated SCM lifecycle and Event Viewer evidence from
  S03 remain the authoritative service evidence.

## Known gaps

1. **npm publishing is not activated for Windows in this round.** The publish
   workflow still builds the darwin-arm64 + linux-x64 tarball; the Windows host
   can build, pack, and verify its own `win32-x64` payload locally/CI, but a
   package installed from the currently published tarball on Windows has no
   win32 payload and fails closed at `verify-payload`. This is the deliberate
   "win32 npm 发布激活" exclusion; the local/CI packaging path is ready.
2. **SCM startup does not forward runtime environment variables.** The
   service binPath carries `--database` and `--socket` only (S03 decision).
   Starting the daemon through SCM therefore does not pass `--runtime`, so a
   ZCode task served by an SCM-started daemon has no pinned runtime. Direct
   daemon startup and the final acceptance run pass `--runtime` explicitly.
   Forwarding `ZCODE_RUNTIME_PATH`/agent homes through the service registry
   environment is an S03-owned follow-up.
3. **DSH deferred** (version pin mismatch; see above).
4. **CI not triggered.** The `windows-check` workflow exists and was validated
   locally (YAML parse + command-level runs); no GitHub Actions run was started
   in this worktree.
5. **Elevated SCM / Event Viewer** was not re-run in this section; S03's
   non-elevated evidence (structured `SERVICE_ACCESS_DENIED`, read-only
   `registered: false`) stands.
6. **Full live codex create/wait** is owned by the final main acceptance step;
   this section provides the probe/wire path and local runtime evidence.

## Tests used for this evidence

```text
node --test tests/platform/*.test.mjs tests/cli/*.test.mjs tests/install/*.test.mjs
  tests 308 / pass 194 / fail 0 / skipped 114

cargo check --workspace --all-targets      # 0 errors
cargo test -p external-daemon --lib        # 306 passed, 0 failed
cargo test -p external-daemon --bin external-subagentd  # 7 passed
```

The skipped JS tests are host-coupled POSIX fixtures (filesystem `*.sock`
binds, systemd/launchd definitions, `chmod`-based failure injection, POSIX
shebang fake CLIs) with explicit win32 skip reasons; darwin/linux CI keeps
running them unchanged.
