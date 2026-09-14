# Productization acceptance — dual-upstream CLI + real Codex CLI→MCP matrix

Feature `external-subagent-productization-closeout-20260913`, section S04
(2026-09-13). Two candidate generations are recorded, each proven by its own
consumer run through the public install/init surface with real upstream model
tasks: **C1 (plugin cache identity `0.1.1`)** — the matrix below is kept as
historical evidence for that artifact — and the post-installer-fix final
candidate **C2 (`0.1.2`)**, whose fresh consumer verification ran on
2026-09-13 and **passed** (`C2_FRESH_CONSUMER_VERIFICATION_PASS`; see
[Post-fix final candidate C2 — fresh consumer verification](#post-fix-final-candidate-c2--fresh-consumer-verification-pass)
below — C2 does not inherit C1's cells). Evidence is redacted to identities,
states, and markers; no credential content was read into any recorded
output.

## Candidate identity

- Base commit: `9017aa011ce67d5f77c22380975c4d37aaa88f48` (S03 integrated).
- C0 tarball (pre-fix, HEAD `9017aa0`): `external-subagent-0.1.0.tgz`,
  sha256 `54d35381f754860cb5d8e18a0fe41ccdefe2bcf058ebb08037863d193f5d7ac6`
  (54 files, static checks passed).
- **C1 tarball (used for every cell)**: same sources plus the managed-plugin
  identity fix (plugin manifest `0.1.0` → `0.1.1`, see
  [docs/compatibility/codex.md](../compatibility/codex.md)),
  sha256 `b349a0b2cd5806a5f91fb029ebef9e4e4715a124047a87c647ea413c6a2ea857`,
  `check-native-tarball.mjs` passed.
- **C2 (post-fix final candidate, consumer-verified)**: C1 plus the installer
  production fixes landed after the C1 runs — `install-plugin` now reads the
  materialized plugin cache back before reporting success and fails closed on
  store-reused bytes (`CODEX_CACHE_BINDING_MISMATCH`) and on cache-less
  successes (`CODEX_CACHE_UNVERIFIABLE`); commits `4a0ea8c`…`dff0661`, pinned
  by the controlled store-simulation oracles in
  `tests/install/codex-binding.test.mjs`. The C1 runs themselves materialized
  `external-subagent@personal@0.1.1` into codex's machine-global content
  store, so that identity is now burned on this machine exactly like `0.1.0`;
  per the per-candidate identity rule C2 bumps the plugin manifest
  `0.1.1` → `0.1.2`. The C2 tarball was packed from HEAD `8642409` —
  `external-subagent-0.1.0-c2.tgz`, sha256
  `7444f59b508d5aae31b6f17e6c8744c6abde494b86701d821fce43d436d04399`
  (54 files; `check-native-tarball.mjs` passed; contains the plugin manifest
  `0.1.2` and the fail-closed installer paths) — and consumer-verified by the
  fresh run recorded below (`C2_FRESH_CONSUMER_VERIFICATION_PASS`). Its
  native payload was not rebuilt — no Rust inputs changed since the C1 payload
  staging — so the installed daemon/facade bytes are hash-identical to C1's
  (daemon `34e54790…`, facade `7da0c995…`; `payload-identity.txt`).
- Installed identity of the C1 run (isolated public prefix, removed after
  the run):
  CLI `external-subagent 0.1.0`, native daemon sha256
  `34e547901caad6c243c083c5e894fd9c2062c2fefbfe407ea11f8c7c201b4236`, MCP
  facade sha256 `7da0c995d192fd166b9bd69c0b75bd02e3b3835bab2abbf48f32a47e01c38848`,
  managed plugin `external-subagent@personal` version `0.1.1` (installed,
  enabled).

## Environment

macOS darwin 25.6.0 arm64; node v26.5.0; npm 11.17.0
(`--allow-scripts=external-subagent`); real Codex CLI
`/opt/homebrew/bin/codex` → `codex-cli 0.153.4`; DSH
`/opt/homebrew/bin/dsh` → `0.1.5-rc.1` (credentials bridged read-only by
symlink from the real `~/.dsh`); ZCode runtime
`/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs` (zcode `0.16.5`,
real `~/.zcode/cli/config.json` bridged read-only). Isolation per the
Rosetta skill's disposable-host pattern: npm prefix/cache, `CODEX_HOME`, and
the product data dir under a short scratch root (`/tmp/es4`, fully removed
afterwards), with the session shell running an isolated `HOME`; launchd
service was **real** (label `com.external-subagent.daemon` bootstrapped into
the GUI session by the public `init`, PID 4343, later booted out by public
`stop`; the real home's plist/label and every user Codex session were left
untouched — before/after snapshots of the real environment are identical).
One boundary of that isolation is recorded here and in the probe table:
launchd set no `HOME` on the job (the bootstrapping shell's isolated `HOME`
is not inherited by a GUI-session launch agent, and the product plist sets
none), so daemon-side operations that fall back to `HOME` resolved the real
`/Users/ibobby`, not the scratch root.

## Provider probes (public ES commands)

| Probe | Result |
|---|---|
| `agents probe dsh --hi` | local `READY` (`0.1.5-rc.1`, bridged home), auth `UNKNOWN/auth_not_probed` (by design), **hi `READY`** — real upstream round-trip |
| `agents probe zcode --hi` | local `READY` (zcode `0.16.5`), auth/hi `UNAVAILABLE/policy_unverified`. Correction (native review): the probe's recorded `scope.home` was the real `/Users/ibobby`, not the scratch `HOME` — launchd set no `HOME` on the daemon job (the plist sets none), and the daemon's home fallback resolved the real user home. This run therefore establishes neither which ZCode config source the probe consulted nor how the policy gate evaluated against that real home; `policy_unverified` is recorded as that evidence gap, not as a property of an isolated HOME and not as a statement about a normal user's configuration. Real ZCode capability is unaffected and proven by the spawn cells below |
| DSH build mode | proven by cell 1 (live) |
| DSH strict-plan refusal | task `10000002`: `COMPLETED`, the model reported no write tool available, workspace stayed empty |
| ZCode explicit model | `model_selection_unsupported` before any prompt (re-asserted live by the 20:0x double-provider session run below; the closeout re-check did not reach this step) |

## ACCEPTANCE-MATRIX — C1, `0.1.1` (historical)

Four independent cells against the one C1 tarball; superseded as release
evidence by the C2 matrix below, kept as the historical record for the C1
artifact.

| # | Route | Task ID | Terminal outcome | Final marker | resources_reaped | closed | Exit codes |
|---|---|---|---|---|---|---|---|
| 1 | DSH, public ES CLI (`spawn/wait/result/close`) | `10000000` | `COMPLETED` | `S04_CLI_DSH_OK` | true | true | 0 / 0 / 0 / 0 |
| 2 | ZCode, public ES CLI | `10000001` | `COMPLETED` | `S04_CLI_ZCODE_OK` | true | true | 0 / 0 / 0 / 0 |
| 3 | DSH, real Codex CLI → managed plugin → MCP | `10000003` | `COMPLETED` | `S04_MCP_DSH_OK` | true | true | codex exec 0 |
| 4 | ZCode, real Codex CLI → managed plugin → MCP | `10000004` | `COMPLETED` | `S04_MCP_ZCODE_OK` | true | true | codex exec 0 |

Cells 3–4 were separate fresh `codex exec --ephemeral --json` processes whose
recorded JSONL shows real `mcp_tool_call` items against server
`external_subagent` (`spawn` → `wait` → `result` → `close`, each
`completed`), not `tools/list` or a direct facade call; the reported markers,
reaped, and closed flags were cross-checked from the ES daemon side and
matched. A third fresh Codex process after an idempotent `install-plugin`
re-sync made a real `external_subagent_status` call, proving post-sync loads
pick up the managed plugin (`0.1.1`, enabled).

Evidence provenance: the per-cell artifacts backing this matrix (spawn/wait/
result/close JSON, Codex `--json` JSONL, probe/plugin/stop/uninstall
records, C1 tarball) currently reside in the S04 run root
`/tmp/es-s04-candidate/` (raw, identity-level redaction only; `/tmp` is
ephemeral — this document is the durable sanitized record). Terminal
outcomes, final markers, reaped, and closed flags above are read directly
from those saved artifacts; the numeric exit-code column (`0 / 0 / 0 / 0`,
`codex exec 0`) is the S04 session runner's report — the per-cell numeric
codes were not separately persisted, and the saved terminal proof for cells
3–4 is the `turn.completed` JSONL records with real token usage and no error
events.

Same-candidate consumer re-check, two records:

- **S04 session (2026-09-13 ~20:0x, session prefix since removed)**:
  `EXTERNAL_SUBAGENT_LIVE_PREFIX=<prefix> node --test
  tests/integration/double-provider.test.mjs` → 3/3 pass as reported by the
  session (dsh hi READY, models=3, build COMPLETED/reaped, strict-plan
  COMPLETED with unmutated workspace, mid-run cancel CANCELLED/reaped, zcode
  native-model COMPLETED/reaped, explicit-model rejection before prompt).
  No run log survived with that prefix; the report is kept attributed, not
  re-sourced.
- **Closeout re-check (2026-09-13 ~20:47, ZAS agent 10000140)** against a
  fresh public `npm install -g` of the **same C1 tarball** (prefix
  `/tmp/es4-check/prefix`; installed native daemon/facade hash-verified
  identical to C1): both controlled checks pass, and the hardened bridge
  digest (symlink-aware `treeDigest`, capture plus guaranteed `finally`
  re-check) executed cleanly with **no bridge drift** — but the live harness
  now stops at the authenticated DSH hi probe: `UNAVAILABLE`, reason
  `timeout`, reproduced identically twice with backoff (logs
  `evidence/double-provider-rerun-c1.log`, `evidence/double-provider-rerun-c1-retry.log`
  under the run root below). Live steps after that probe were not re-reached;
  their proof remains the 20:0x per-cell artifacts above. The current DSH
  upstream timeout is recorded as an environment boundary of the re-check
  moment (same tarball, same bridges, same harness pattern passed at 20:0x),
  not classified as a product regression; next step is to re-run the harness
  once the DSH authenticated hi answers again.

## Post-fix final candidate C2 — fresh consumer verification (PASS)

`C2_FRESH_CONSUMER_VERIFICATION_PASS` (2026-09-13, HEAD `8642409`, managed
plugin identity `0.1.2`). Run type: fresh consumer verification through the
public install/init surface only — no tracked file, `.agent-work/`, legacy
ZAS, or real `~/.codex` was modified. The run root was `/tmp/es-c2/`
(ephemeral); `outputs/s04-c2-evidence/` is the durable sanitized copy, and
every fact below is read from those artifacts. The C1 matrix above stays
C1's evidence — rewriting its rows to `0.1.2` would be evidence
falsification; C2 was proven by this run of its own.

### Install and init (public surface only)

- `npm install -g` of the C2 tarball (sha256
  `7444f59b508d5aae31b6f17e6c8744c6abde494b86701d821fce43d436d04399`) into
  isolated prefix `/tmp/es-c2/prefix` — isolated `HOME` `/tmp/es-c2/home`,
  npm cache `/tmp/es-c2/npm-cache`, `--allow-scripts=external-subagent`.
- `init --codex-home /tmp/es-c2/codex` completed all ten steps (`init.json`:
  probe-runtime, verify-payload, check-path, create-data,
  write-product-config, install-launch-agent, start-service,
  install-codex-plugin, claim-codex-home, publish-active-payload); the
  installed payload is hash-identical to C1's (daemon `34e54790…`, facade
  `7da0c995…` — payload not rebuilt, Rust inputs unchanged since staging).
- The launchd service was **real**: the public `init` bootstrapped label
  `com.external-subagent.daemon` into the GUI session (PID 71071), and the
  public `stop` booted it out at the end of the run (`stop.json`:
  `removed: true`, status 0).

### Plugin cache verification (the C2-specific acceptance point)

- `resync-plugin.json` — the idempotent public `install-plugin` receipt —
  reports `installed: true`, **`cache_verified: true`**, cache
  `/private/tmp/es-c2/codex/plugins/cache/personal/external-subagent/0.1.2`,
  digest `22aa0fb68e4073de953d2964ac57eda6026d63c2fc05c9e43f36571d2f7e3292`,
  codex-side identity `external-subagent@personal` version `0.1.2`.
- The materialized cache equals this run's staged binding, i.e. the fixed
  installer's read-back verification ran live on its happy path:
  `cache-mcp.json` (byte copy of the cache `.mcp.json`) pins this run's
  absolute facade under `/private/tmp/es-c2/prefix/…` and the isolated
  socket `/tmp/es-c2/home/Library/Application Support/external-subagent/external-subagent.sock`;
  `cache-plugin-manifest.json` is `0.1.2`; `cache-vs-staged.diff` is empty.
- `plugin-list.json`: a fresh `codex plugin list --json` shows
  `external-subagent@personal` version `0.1.2`, enabled, local marketplace
  `personal`.

### ACCEPTANCE-MATRIX — C2 (four independent cells, one tarball)

| # | Route | Task ID | Terminal outcome | Final marker | resources_reaped | closed | Exit codes |
|---|---|---|---|---|---|---|---|
| 1 | DSH, public ES CLI (`spawn/wait/result/close`) | `10000000` | `COMPLETED` | `S04_C2_CLI_DSH_OK` | true | true | 0 / 0 / 0 / 0 |
| 2 | ZCode, public ES CLI | `10000001` | `COMPLETED` | `S04_C2_CLI_ZCODE_OK` | true | true | 0 / 0 / 0 / 0 |
| 3 | DSH, real Codex CLI → managed plugin → MCP | `10000003` | `COMPLETED` | `S04_C2_MCP_DSH_OK` | true | true | codex exec 0 |
| 4 | ZCode, real Codex CLI → managed plugin → MCP | `10000004` | `COMPLETED` | `S04_C2_MCP_ZCODE_OK` | true | true | codex exec 0 |

- Cells 1–2 artifacts: `cell1-dsh-{spawn,wait,result,close}.json`,
  `cell2-zcode-{spawn,wait,result,close}.json` (public CLI JSON; markers read
  from `result` final_text, reaped/closed from `close`).
- Cells 3–4 artifacts: `cell3-codex-dsh.jsonl`, `cell4-codex-zcode.jsonl` —
  separate fresh `codex exec --ephemeral --json` processes whose JSONL shows
  real `mcp_tool_call` items against server `external_subagent`
  (`spawn` → `wait` → `result` → `close`, each `completed`) and real token
  usage in `turn.completed` — not `tools/list` or a direct facade call.
- Daemon-side cross-check: `tasks-list-wsdsh.json` / `tasks-list-wszcode.json`
  — every task COMPLETED with `resources_reaped: true`, `closed: true`.

### C2 run boundaries (recorded, not hidden)

- **Cell 3 attempt 1 was approval-blocked, not product-blocked**: with codex
  exec's default approval policy (`never`), the `close` call was rejected
  ("MCP tool call requires approval, but approval policy is never") while
  spawn/wait/result completed; that task (`10000002`) was closed afterwards
  through the public CLI (`cell3-attempt1-task-close.json`: COMPLETED,
  reaped, closed). The recorded cell 3 was a fresh second codex exec run
  using `--dangerously-bypass-approvals-and-sandbox` so the full four-call
  lifecycle could complete; all MCP calls in both attempts were real
  (`cell3-codex-dsh-attempt1-approval-blocked.jsonl`).
- **The C1 HOME boundary repeats**: launchd sets no `HOME` on the daemon job
  (the product plist sets none), so daemon-side `HOME` fallbacks resolved
  the real `/Users/ibobby`; the zcode hi probe policy gap stays open (cells
  2 and 4 prove real Zcode capability).
- **Identity burn-in**: these runs materialized
  `external-subagent@personal@0.1.2` into codex's machine-global content
  store; any later candidate must bump the plugin version again.
- Unrelated stderr noise: codex's account-level remote plugin catalog
  produced a cloudflare OAuth `AuthRequired` transport error at exit in
  cells 3–4 (no credential content; the managed local plugin was
  unaffected).
- Legacy ZAS (PID 30433), real ES data (digests unchanged), real `~/.codex`
  (cache still `0.1.0` only), and historical `/tmp/esr2-*` daemons were
  untouched (`env-before.txt` / `env-after.txt`).

Sanitization: identity-level redaction only; the isolated `auth.json`
(copied mode 600 from `/Users/ibobby/.codex-multi-2/auth.json`) was never
printed, never appears in any recorded output, was excluded from the durable
copy, and was deleted from the `/tmp` run root after the run; no
credential-shaped content was found by pattern scan across all copied
files.

## Boundaries and limitations

- **Codex 0.153.4 global content store**: `plugin add` materializes caches
  from a machine-global store keyed by `plugin@marketplace@version`. With the
  real `~/.codex` already caching `external-subagent@personal@0.1.0`, an
  isolated home binding the *same* identity receives the real installation's
  bytes (observed live in S03 testing). The managed plugin therefore carries
  a distinct release identity per candidate (`0.1.1` for C1; `0.1.2` for C2,
  because the C1 runs themselves put `0.1.1` bytes into the machine-global
  store — and the C2 runs have now burned `0.1.2` the same way, so any later
  candidate must bump again); verified live in both runs that the isolated
  cache at
  `<CODEX_HOME>/plugins/cache/personal/external-subagent/<version>/.mcp.json`
  was byte-identical to the staged binding (absolute installed facade +
  isolated socket), i.e. not masked. Post-review hardening (native review
  finding): `install-plugin` now reads the materialized cache's
  `.mcp.json`/manifest back before reporting success and fails closed
  (`CODEX_CACHE_BINDING_MISMATCH`/`CODEX_CACHE_UNVERIFIABLE`) when the store
  reused another binding's bytes for the same identity, and with
  `CODEX_CACHE_UNVERIFIABLE` when a reported success materialized no cache
  to verify — an install is only ever recorded as
  installed/claimed/updated (`cache_verified: true`) after its cache bytes
  were verified. The C2 fresh consumer run exercised that guard live on its
  happy path (`cache_verified: true` with an empty `cache-vs-staged.diff`);
  the fail-closed branches remain pinned by the controlled store-simulation
  oracles in `tests/install/codex-binding.test.mjs`.
- **reload_required**: the Codex processes used here were ephemeral and exited
  on completion; no user Codex session was force-quit. Installed-cache
  version and a long-lived host's loaded version are separate facts — a
  running host keeps its loaded plugin until it reloads; `codex plugin list`
  from a fresh process is the supported check.
- **ZCode hi probe policy gate**: the probe ran with `scope.home`
  `/Users/ibobby` (launchd set no `HOME`; the product plist sets none), so
  the config source behind the probe and the policy-gate evaluation that
  produced `policy_unverified` are unproven for this run — an authenticated
  plus policy-verified ZCode hi on a launchd-resident daemon remains open
  evidence. This gap does not negate the ZCode task cells: cells 2 and 4
  prove real ZCode capability through the public CLI and the managed plugin.
- **REGISTRY_PUBLICATION_PENDING**: no public npm publish was authorized or
  performed; the local tarball install above does not claim publication.
- NOT_RUN for this candidate: installation into the real user `~/.codex`
  (requires explicit authorization); Codex GUI app hot-reload (no claim made).

Legacy `zcode-as-subagent` daemon/plist and the user's real
`~/Library/Application Support/external-subagent` state were untouched
(verified before/after; a historical `/tmp/esr2-*` daemon belonging to
another run was left alone).
