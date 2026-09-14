# Productization acceptance — dual-upstream CLI + real Codex CLI→MCP matrix

Feature `external-subagent-productization-closeout-20260913`, section S04
(2026-09-13). All four consumer cells below ran against **one** candidate
tarball through the public install/init surface, with real upstream model
tasks. Evidence is redacted to identities, states, and markers; no credential
content was read into any recorded output.

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
- Installed identity (isolated public prefix, removed after the run):
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

## ACCEPTANCE-MATRIX (four independent cells, one tarball)

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

## Boundaries and limitations

- **Codex 0.153.4 global content store**: `plugin add` materializes caches
  from a machine-global store keyed by `plugin@marketplace@version`. With the
  real `~/.codex` already caching `external-subagent@personal@0.1.0`, an
  isolated home binding the *same* identity receives the real installation's
  bytes (observed live in S03 testing). The managed plugin therefore carries
  a distinct release identity (`0.1.1` for this candidate); verified during
  this run: the isolated cache at
  `<CODEX_HOME>/plugins/cache/personal/external-subagent/0.1.1/.mcp.json` was
  byte-identical to the staged binding (absolute installed facade + isolated
  socket), i.e. not masked. Post-review hardening (native review finding):
  `install-plugin` now reads the materialized cache's `.mcp.json`/manifest
  back before reporting success and fails closed
  (`CODEX_CACHE_BINDING_MISMATCH`/`CODEX_CACHE_UNVERIFIABLE`) when the store
  reused another binding's bytes for the same identity; that guard is pinned
  by the controlled store-simulation oracles in
  `tests/install/codex-binding.test.mjs` — the four live cells above were
  not re-run for it.
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
