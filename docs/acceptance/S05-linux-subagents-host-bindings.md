# S05 acceptance — remaining subagents and host bindings on Linux (2026-10-09)

Section S05 of `external-subagent-linux-full-20261009`: the zcode/dsh/agy
subagents and both host bindings verified on Linux x86_64, plus the two
production path fixes the section owned (the scope policy verifier default
path and the zcode runtime constant) and three later fixes expanded into the
section by the coordinator ruling (the staged verifier script's provenance
path, the probe loop's unknown-server-request tolerance, and `diagnose`'s
runtime artifact resolution — §5). Every fact below is **OBSERVED** on the
host in the environment table unless marked otherwise; all product state
lived in an isolated HOME/XDG tree, all Codex interaction used a throwaway
`CODEX_HOME`, and the zcode credentials used for the probe/spawn rounds were
copied into the isolated home and deleted afterwards. Raw command outputs:
`/tmp/es-s05-evidence/`.

## Environment (OBSERVED, 2026-10-09, times UTC)

| Item | Value |
|---|---|
| OS | Ubuntu 26.04 LTS, x86_64, kernel 7.0.0-29-generic |
| Node | v24.17.0 — `/home/ibobby/.nvm/versions/node/v24.17.0/bin/node` |
| Codex CLI | `codex-cli 0.162.0` (nvm bin, Node shim) |
| ZCode Linux runtime | `~/.zcode/server/agents/glm/zcode.cjs`, `--version` → `0.16.9`; bundled `~/.zcode/server/node` v22.16.0; `agents/glm/.version` `0.13.3`; desktop `ZCODE_APP_VERSION=3.14.5`, attached-remote deployment (`ZCODE_SERVER_RUNTIME_ROOT` exported by the desktop, `zcode-agent` launcher defaults to `$HOME/.zcode/server`) |
| dsh / agy | **not installed** — not on PATH, no `DSH_RUNTIME_PATH`/`AGY_RUNTIME_PATH` |
| product HOME / XDG | `/tmp/es-s05-iso/home`, `XDG_{DATA,STATE,CONFIG}_HOME=/tmp/es-s05-iso/xdg-{data,state,config}` |
| CODEX_HOME (throwaway) | `/tmp/es-s05-codex-home` |
| daemon | release payload `npm/native/linux-x64/external-subagentd` (fail-closed round) and `target/debug/external-subagentd` (rebuilt with the S05 verifier fix, enable/models rounds) |

`init --skip-service-start` verified the linux-x64 payload 0.4.1 and wrote the
systemd unit **without** `--runtime` (the conventional Linux runtime location
does not exist under the isolated HOME), with the fixed PATH carrying the
nvm bin (S04 behavior).

## 1. Production path fixes (code)

1. **Scope policy verifier default path**
   (`crates/external-daemon/src/agent_status.rs`, was :1849-1860): the default
   is now derived per platform by `policy_verifier_candidates` — the frozen
   macOS `~/Library/Application Support/<product>` bytes on darwin; on Linux an
   exported absolute `$XDG_DATA_HOME/<product>/external-subagent-policy-verifier`
   is probed first (the same environment rule `cli/paths.mjs` and
   `rpc/profiles.rs` apply), then the `<scope home>/.local/share/<product>`
   fallback. `EXTERNAL_SUBAGENT_POLICY_VERIFIER` remains the explicit override.
   The three `#[cfg(test)]` fixtures (formerly :2779/:2888/:3387) install the
   scope-home fallback candidate through the same function; a new shape test
   pins both arms (pure form, no process-env mutation, so the parallel suite
   stays deterministic). `cargo test -p external-daemon --lib` green; the new
   lines appear nowhere in `cargo fmt --check` (195 pre-existing drifts
   elsewhere, unchanged count).
2. **zcode runtime constant** (`cli/constants.mjs`, was line 17): the macOS
   app-bundle constant is kept as `ZCODE_RUNTIME`; the runtime the service
   generators and the observations report use is `zcodeRuntimePath(home, env)`
   — explicit `ZCODE_RUNTIME_PATH` first, then the platform convention
   (`/Applications/ZCode.app/…/glm/zcode.cjs` on darwin;
   `<product home>/.zcode/server/agents/glm/zcode.cjs` — the observed Linux
   distribution form — on Linux), then the packaged constant reported honestly
   as an absent pin. Forwarding stays presence-gated (`--runtime` only when
   the file exists). `ZCODE_SERVER_RUNTIME_ROOT` is deliberately not read
   (service generation stays shell-independent; rationale in
   docs/compatibility/zcode.md).

New oracles: `tests/cli/linux-subagents.test.mjs` — discovery precedence
(explicit > darwin constant > Linux convention > absent-pin fallback,
`ZCODE_SERVER_RUNTIME_ROOT` ignored), the systemd unit / runtime report
forwarding the discovered runtime or nothing, and a real-daemon fail-closed
test (below).

Two further fixes joined the section by the coordinator ruling after the
initial blockers were reported: the staged verifier script's provenance
candidates (§5.1) and the probe loop's unknown-server-request tolerance
(§5.2), plus `diagnose`'s `runtime.configured_artifact` now resolving through
the same `zcodeRuntimePath` (label `cli_resolved_configuration`; oracle in
`tests/cli/diagnose.test.mjs`).

## 2. Fail-closed negatives (verified first, test + live)

Test (`tests/cli/linux-subagents.test.mjs`, real `target/debug` daemon, no
`--runtime`, PATH without any subagent runtime): `agents enable zcode|dsh|agy`
each refuses with `agent_probe_failed` ("Cannot enable …: missing") and writes
no config; `spawn zcode` before enable refuses `agent_disabled`; `list` shows
zero tasks.

Live (release payload, real CLI, daemon A): identical results —
`{"code":"agent_probe_failed","message":"Cannot enable zcode: missing"}` (same
for dsh/agy), `{"code":"agent_disabled"}` for the spawn, config revision
unchanged with every subagent still disabled.

## 3. zcode enable + probe + spawn smoke (real runtime)

Daemon B (`--runtime ~/.zcode/server/agents/glm/zcode.cjs`):
`agents enable zcode` → `local.state=READY`, `version=0.16.9`,
`runtime_path=/home/ibobby/.zcode/server/agents/glm/zcode.cjs`,
`restart_required=false`, config revision 2 (`enabled=true`,
`spawn_supported=true`, no runtime_path persisted — the D1 rule holds).

Daemon C (debug binary with the S05 verifier fix, XDG env exported):
`subagents models zcode` first isolated two gaps (§5), both fixed in the same
section after the coordinator ruling — the post-fix re-verification (daemon D,
same controlled tree, fixed verifier restaged by `hooks install`, throwaway
credentials re-copied) reads:

| layer | before the fix | after the fix (OBSERVED) |
|---|---|---|
| `subagents models zcode` | `policy_unverified` → `create_failed` | **`supported: true`, catalog `["deepseek/deepseek-flash"]`, reason none** (the catalog reflects the throwaway provider config copied into the isolated home, not a product claim about the host's catalog) |
| `subagents probe zcode --hi` | `policy_unverified` | **local/auth/hi all `READY`** (0.16.9) |

**Spawn smoke** (plan mode, prompt "reply with the single word
pong"; pre-fix on daemon C and post-fix on daemon D — identical outcomes):

| step | result |
|---|---|
| spawn | `agent_id=10000000` (pre-fix) / `10000001` (post-fix), `session_id=sess_805b5cdb…` |
| wait | `completed` |
| result | `outcome=COMPLETED`, `final_text="pong"`, `input_identity={subagent:zcode, adapter_version:0.4.1, model:null, model_source:"native", permission_mode:"plan"}` |
| close | closed |

The runtime booted fully on Linux (its own log under the isolated
`~/.zcode/cli/log/` shows MCP startup, sessions, clean shutdown). Real
`~/.zcode` zero-write oracle: no files outside the desktop's own
exec/artifacts/log state changed; copied v2 credentials were deleted after
the smoke.

## 4. dsh / agy — blockers (recorded honestly)

Neither runtime exists on this host (not on PATH, no env override), so
enable/probe stop exactly at the fail-closed refusal (§2) and no spawn smoke
is claimed. Upstream runtime installation is explicitly out of S05 scope
(exclusions). This is a host-environment blocker, not a product defect: the
same probes against the fixture runtimes remain covered by
`tests/cli/agents-config.test.mjs` (dsh/codex enable paths) and the Rust
probe suites.

## 5. zcode read-only probe layers — two gaps found, fixed, re-verified

Both were pinpointed with controlled experiments first (no product file
modified during the diagnosis), then expanded into S05 by the coordinator
ruling and fixed:

1. **Staged verifier script macOS path**
   (`plugins/codex/external-subagent/scripts/policy-verifier.mjs`): joined
   its provenance as `<home>/Library/Application Support/external-subagent/…`
   unconditionally → exit 2 on Linux → `policy_unverified` on every
   `--hi`/`models` probe. Diagnosis: manual run exit 2 as staged, exit 0
   with a provenance copy at the macOS-style path. Fix: the provenance
   candidates are derived with the same semantics as the daemon's
   `policy_verifier_candidates` (macOS frozen bytes; Linux absolute
   `$XDG_DATA_HOME/external-subagent/` first, then
   `<home>/.local/share/external-subagent/`). Re-verified: `hooks install`
   restages the fixed script, and with the daemon's XDG env it exits 0
   against the isolated tree.
2. **Unknown server request aborted the probe loop**
   (`agent_status.rs`): the 0.16.9 runtime issues
   `interaction/requestOfficialMcpAuthHeaders` during `session/create` and
   proceeds with an anonymous fallback regardless of the answer; the probe
   loops (request wait, turn wait, event drain) answered `-32601` and
   returned `Err("transport")`, so the wait aborted before the create result
   arrived → `create_failed`. Diagnosis: a manual replica of the daemon's
   exact frames (ZcodeStrict envelope, `RuntimePreferences` camelCase
   default reply, no initialize) completed `session/create` with the full
   `settings.model.available` projection. Fix:
   `respond_unsupported_probe_request` answers method-not-found and keeps
   waiting at all three sites (permission requests still fail closed as
   `policy_violation`); pinned by the new
   `zcode_catalog_survives_an_unsupported_server_request_during_create`
   unit test (fixture emits the unknown request mid-create; catalog still
   projected, exactly one -32601 on the wire).

Post-fix live results are in §3 (`models` supported with catalog; `--hi`
all-READY). Protocol observations recorded without product change: no
`initialize` method (`-32601 Method not found`);
`session/requestRuntimePreferences` with scope `runtime-materialization`;
create result echoes `mode` as `build` for a `plan` request (the env-level
plan policy still held for the completed plan-mode turn).

## 6. Host bindings on Linux (real codex 0.162.0, controlled homes)

| flow | result |
|---|---|
| `install-plugin codex --codex-home /tmp/es-s05-codex-home` | `installed=true`, `cache_verified=true`, plugin `0.1.4` at `plugins/cache/personal/external-subagent/0.1.4`, D08 claim `{registered:true, homes:1}` |
| repeat install | idempotent, `cache_verified=true`, claim `{deduplicated:true}` |
| `install-plugin codex --uninstall` | official remove, cache gone, `claim_released=true`, registry `homes: []` |
| `install-mcp --codex-home …` | TOML `[mcp_servers.external_subagent]` written (10 tools, linux-x64 facade, isolated socket); `codex mcp list` → one row, `Status: enabled`; `codex mcp list --json` shape captured |
| `install-mcp --uninstall` | exactly the managed section removed; unrelated `[marketplaces.personal]` preserved; claim released |
| `install-plugin zcode` | staging under `<XDG data>/external-subagent/zcode-plugin/external-subagent`, `plugins.dirs` entry in the isolated `~/.zcode/cli/config.json` (hooks from `hooks install` preserved byte-for-byte in value), bridge binding = installing node + `scripts/mcp-stdio-bridge.mjs` + `timeoutMs=300000` + isolated socket, `config_verified=true` |
| repeat install | idempotent, identical digest, still exactly one `dirs` entry |
| `install-plugin zcode --uninstall` | `staging_removed=true`, `plugins` key gone, hooks intact |

The codex 0.162.0 Linux `--json` shapes are identical to the 0.154.0 macOS
table (diff table in docs/compatibility/codex.md); one environment-dependent
stderr warning (`PATH aliases … temporary dir "/tmp"`) is documented there.
Real-home zero-write: `~/.codex` showed no product-written entries; the real
`~/.zcode/cli/config.json` does not exist and was never created.

## 7. Checks run

- `node --test tests/cli/*.test.mjs tests/contract/*.test.mjs
  tests/install/*.test.mjs tests/platform/*.test.mjs` — **288 tests,
  271 pass, 17 skip (darwin-gated), 0 fail** (baseline before the section:
  cli+contract 160/151/9/0, install+platform 124/116/8/0).
- `cargo test -p external-daemon --lib` — green (458 tests after the
  blocker-2 unit test joined; 457 on the pre-ruling runs), including the
  agent_status module's verifier-shape and unknown-request oracles.
- `cargo check --workspace` — clean.
- `cargo fmt --check` — 195 pre-existing drifts, none in the changed lines.
- `npm run check` — pass.

## NOT_RUN (explicitly not claimed)

- dsh/agy spawn smoke (runtimes absent from the host, §4; upstream
  installation stays out of scope by the coordinator ruling).
- The staged `mcp-stdio-bridge.mjs` spawned from inside a real Linux ZCode
  GUI restricted context (no Linux GUI client here; the bridge is plain node
  stdio piping with no platform branch).
- Real-home bindings (`~/.codex`, real `~/.zcode/cli/config.json`) —
  controlled homes only, per the section contract.
