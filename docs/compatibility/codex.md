# Codex compatibility

Verified interface evidence for the Codex CLI this product binds to. The
current tested baseline is codex-cli **0.154.0**
(`/opt/homebrew/bin/codex`, `codex --version` → `codex-cli 0.154.0`) on
macOS arm64. The command surface below was first verified against 0.153.4
(2026-09-12) and the plugin add/marketplace add paths were re-verified live
against 0.154.0 (2026-09-18); cache-resolution behavior changed between the
two versions and is recorded per version below. All evidence was gathered
with `CODEX_HOME` pointed at throwaway directories; no real user Codex home
was modified while gathering this evidence.

A second baseline — codex-cli **0.162.0** on Linux x86_64 (Ubuntu 26.04, the
nvm launcher `/home/ibobby/.nvm/versions/node/v24.17.0/bin/codex`, a Node shim)
— was verified live on 2026-10-09 through the external-subagent CLI; the
app-server deltas from 0.154.0 and the live full-chain evidence are recorded in
"Linux x86_64, codex-cli 0.162.0 (observed 2026-10-09)" below and in
[docs/acceptance/S04-linux-codex-smoke.md](../acceptance/S04-linux-codex-smoke.md).

## Implementation layout

The Codex protocol face lives in the `external-agent-codex` crate
(`crates/subagents/codex`): the `thread/start`, `thread/resume`, and
`turn/start` parameter shapes, the binary plan/yolo posture admission, the
fail-closed result echo validation, and the `item/*`/`turn/*` event folding.
The daemon side (`crates/external-daemon/src/codex`) is lifecycle
composition only — spawn gate, runtime owner, stdio pump, and control-plane
glue.

## Plugin interface (verified 2026-09-12)

| Command | Result shape | Notes |
|---|---|---|
| `codex plugin add --help` | help text, exit 0 | availability probe used before any mutation |
| `codex plugin marketplace add <root> --json` | `{marketplaceName, installedRoot, alreadyAdded}` | idempotent; reads `<root>/.agents/plugins/marketplace.json` |
| `codex plugin add <name> --marketplace <marketplace> --json` | `{pluginId, name, marketplaceName, version, installedPath, authPolicy}` | idempotent; installs into `$CODEX_HOME/plugins/cache/<marketplace>/<name>/<version>` |
| `codex plugin remove <name>@<marketplace> --json` | `{pluginId, name, marketplaceName}` | **the bare `plugin remove <name>` form is rejected** (`plugin requires --marketplace unless passed as <plugin>@<marketplace>`); removes the plugin and its cache, keeps marketplace registration |
| `codex plugin list --json` | `{installed: [...], available: [...]}` | per-plugin `enabled`, `source.path`, `marketplaceSource` |

Side effects observed inside `CODEX_HOME`: `config.toml` gains
`[marketplaces.<name>]` (`source_type = "local"`, `source = <root>`) and
`[plugins."<name>@<marketplace>"]` (`enabled = true`); the plugin tree is
copied to the cache path. The product never writes these itself — it stages
the source marketplace and calls the official commands.

## Local marketplace layout (verified)

```
<root>/.agents/plugins/marketplace.json   # {"name": "personal", "plugins": [...]}
<root>/plugins/<plugin-name>/             # source tree with .codex-plugin/, .mcp.json, skills/
```

Marketplace entries reference plugins by relative `source.path`
(`./plugins/<name>`). A marketplace root without
`.agents/plugins/marketplace.json` is rejected (`marketplace root does not
contain a supported manifest`).

## CODEX_HOME semantics (verified; cache resolution re-verified on 0.154.0)

- `CODEX_HOME` confines plugin cache/config writes; it must already exist or
  codex fails (`failed to resolve CODEX_HOME`). The product creates the
  configured home (mode 0700) when claiming it.
- The personal marketplace named `personal` is the default the product
  registers; it does not conflict with an already-registered root (re-adds
  return `alreadyAdded: true`).
- On codex-cli 0.154.0, `plugin add` cache resolution has two verified
  shapes:
  - **Non-reserved marketplace names** resolve to the root registered inside
    the running `CODEX_HOME`, and every `plugin add` **re-materializes the
    cache from that registered root** — verified live by tampering a cached
    file and re-adding: the CLI exited 0 and restored the registered root's
    original bytes, so a tampered or stale cache alone can neither fail an
    add nor serve stale bytes. The 0.153.4-observed model — a frozen
    machine-global content store keyed by `plugin@marketplace@version`
    handing every later home whichever binding first cached the identity —
    no longer holds on 0.154.0 for non-reserved names.
  - **The reserved marketplace name `personal`** (the product's default) is
    resolved machine-globally to the real user root (`~/plugins/<plugin>`
    behind the real `~/.agents/plugins/marketplace.json` registration),
    **ignoring any `personal` registration inside the running `CODEX_HOME`**
    — verified live with a decoy: an in-home `personal` registration
    pointing at a 9.9.9 root was ignored and the real root's 0.1.2 bytes
    were installed byte-identical. `CODEX_HOME` therefore does **not**
    isolate installs made under the reserved name: they read — and cache —
    the REAL root's content. The earlier "store reuse" observation (an
    isolated home's `external-subagent@personal@<version>` cache carrying
    the real home's socket while the staged tree carried the throwaway
    socket) is this reserved-name resolution, not a version-keyed store.

Consequence: the cache path is still
`<codex-home>/plugins/cache/<marketplace>/<name>/<version>`, so the plugin
manifest version **is the cache identity** and stays distinct per released
candidate. Identity `0.1.0` is already cached by the historical
installation, identity `0.1.1` — candidate C1 of the productization
closeout — was itself materialized by the C1 consumer runs, and the final
candidate C2 (`0.1.2`) burned `0.1.2` the same way, so every later release
candidate bumps its own identity again. Verified live during both the C1
and the C2 runs that an isolated home's `external-subagent@personal@<version>`
cache was byte-identical to the staged binding (see
[docs/acceptance/productization.md](../acceptance/productization.md)).
Because versioning alone is a release discipline rather than a runtime
guarantee, `install-plugin` also **reads the materialized cache back before
reporting success** and compares its `.mcp.json` (the full managed MCP
document — facade command, `EXTERNAL_SUBAGENT_SOCKET`, and every other server
field such as args), its `.codex-plugin/plugin.json` identity, and its
full managed content (file set and bytes) with this run's staged binding;
foreign or unreadable bytes fail closed with
`CODEX_CACHE_BINDING_MISMATCH` / `CODEX_CACHE_CONTENT_MISMATCH`, and a
`plugin add` success whose cache cannot be located at all fails closed the
same way with `CODEX_CACHE_UNVERIFIABLE` — there is no
`installed`/`cache_verified: false` outcome; only a cache verified against
this run's staged binding is ever reported (`cache_verified: true`) or
recorded as installed/claimed/updated. The content check is retained as a
fail-closed defense against freeze-behavior CLIs (the 0.153.4 shape) and
any future regression, but on 0.154.0 non-reserved names it is unlikely to
fire live because codex re-materializes the cache from the registered root
before the verifier reads it; the live reserved-name failure shape on
0.154.0 is `CODEX_CACHE_BINDING_MISMATCH` (the cache resolved to the real
root's different version). The product only ever reads the cache — the
remediation for reused bytes is a distinct release identity (freeze-behavior
CLIs) or refreshing/re-registering the resolved root, and for the reserved
name an isolated home should use a non-reserved marketplace name; codex-owned
state is never edited. The fail-closed paths are pinned by the
store-simulation oracles in `tests/install/codex-binding.test.mjs`.

## Product binding

- The staged plugin `.mcp.json` pins `command` to the absolute
  `external-subagent-mcp` facade inside the installed npm package and
  `EXTERNAL_SUBAGENT_SOCKET` to the daemon socket — never a shell/GUI PATH
  lookup or an nvm-relative path.
- The direct TOML binding (`install-mcp`) writes the same ten-tool
  `mcp_servers.external_subagent` section non-destructively.
- Codex-side `enabled` state is owned by codex; the product never forces or
  clears it for anything it did not install.

## Verified since the productization closeout (2026-09-13)

- Tool discovery **and real tool calls inside a running Codex host**, for
  both candidate generations: fresh `codex exec` processes loaded the
  managed plugin from an isolated `CODEX_HOME` and completed real
  `external_subagent_spawn` → `wait` → `result` → `close` MCP calls for
  both providers — first for C1 (`0.1.1`), then for the post-installer-fix
  final candidate C2 (`0.1.2`), whose own fresh consumer verification
  passed (`C2_FRESH_CONSUMER_VERIFICATION_PASS`: `install-plugin` receipt
  `cache_verified: true`, cache byte-identical to the staged binding, all
  four cells COMPLETED) — recorded in
  [docs/acceptance/productization.md](../acceptance/productization.md).

## Cache resolution re-verified on 0.154.0 (2026-09-18)

Two controlled experiments against the real CLI, both confined to
throwaway `CODEX_HOME` directories under `/tmp` (the real user root was
only read; byte-identical before/after):

- **Re-materialization (non-reserved name)**: a marketplace `d2probe`
  pointing at a `/tmp` root whose plugin copy carried version 9.9.9 was
  added; the cache materialized at
  `plugins/cache/d2probe/external-subagent/9.9.9`. After the cached
  `plugin.json` was tampered, a second `codex plugin add` exited 0 and the
  cache held the registered root's original bytes again — the CLI
  re-materialized from the registered root instead of trusting the cache.
- **Reserved-name resolution (`personal`)**: the same 9.9.9 root was also
  registered inside that throwaway home as the marketplace `personal`, then
  `codex plugin add external-subagent --marketplace personal` installed
  **0.1.2** — the real user root's bytes, byte-identical to
  `~/plugins/external-subagent` — ignoring the in-home registration and its
  target entirely. When the same reserved-name install is driven through
  `install-plugin`, this surfaces as a fail-closed
  `CODEX_CACHE_BINDING_MISMATCH` (the materialized cache carries the real
  root's different version/binding) with full staging/marketplace rollback.

## App-server thread posture (verified 2026-09-18, codex-cli 0.154.0)

Probed against `codex app-server --listen stdio://` with throwaway
`CODEX_HOME` directories; no authenticated turn was driven (the one probe
turn failed 401 as expected), so these are transport/posture facts, not
model-behavior facts:

- `thread/start` accepts the string sandbox presets `read-only` and
  `danger-full-access` alongside `approvalPolicy: "never"`. The start
  result resolves the sandbox as an object:
  `{"type":"readOnly","networkAccess":false}` and
  `{"type":"dangerFullAccess"}` respectively. Re-probed 2026-09-19 on
  0.154.0 with a throwaway `CODEX_HOME` and no turn driven: both presets
  echo `approvalPolicy` (`"never"`), the resolved sandbox object, and
  `cwd` (the requested workspace) at the result root, so the daemon
  confirms all three from the start result before sending any
  `turn/start` — the same fail-closed confirmation it applies to resume.
- `thread/resume` of a read-only thread returns the same read-only object
  (re-verified on 0.154.0; first observed on 0.153.4).
- `thread/resume` of a danger-full-access thread returns the **narrowed
  workspace-write reconstruction**
  `{"type":"workspaceWrite","networkAccess":false,"writableRoots":[],"excludeSlashTmp":false,"excludeTmpdirEnvVar":false}`
  even though the rollout's `session_meta` persists
  `"sandbox_policy":{"type":"danger-full-access"}`. The daemon therefore
  confirms a yolo resume against either the faithful `dangerFullAccess`
  object or that exact narrowed object (never wider: no network, no extra
  writable roots); any other shape fails closed.
- A thread is only resumable after a turn persists its rollout;
  `thread/resume` before that errors with `no rollout found for thread id`.

## Subagent permission modes and write manifest

`codex` runs as a first-class subagent with all four public permission modes.
Each mode is driven as an app-server sandbox preset and must be confirmed at
the start/resume result before any turn is sent:

| Mode | Requested preset | Confirmed sandbox posture |
|---|---|---|
| `build` / `edit` | `workspace-write` | `{"type":"workspaceWrite","networkAccess":false,"writableRoots":[],"excludeSlashTmp":false,"excludeTmpdirEnvVar":false}` |
| `plan` | `read-only` | `{"type":"readOnly","networkAccess":false}` |
| `yolo` | `danger-full-access` | `{"type":"dangerFullAccess"}` |

Every mode pins `approvalPolicy: "never"`. `thread/start` must echo that
approval policy, the resolved sandbox object above, and the requested `cwd`
at the result root or inside the embedded `thread` object (either location is
accepted, mirroring how the results carry the model); `thread/resume`
re-confirms the posture (a `danger-full-access` thread reconstructs as the
narrowed `workspaceWrite` object) and any wider or mismatched shape fails
closed before a turn starts.

A non-empty spawn `write_manifest` is unsupported for `codex`. The daemon
rejects it during admission, before any task row or prompt is created, with the
public MCP error code `codex_write_manifest_unsupported` (internal RPC
sentinel `CODEX_WRITE_MANIFEST_UNSUPPORTED`). `zcode`/`dsh` manifest handling
and the global "plan must be empty" rule are unchanged.

## App-server reasoning effort (SOURCE_INSPECTED + probe OBSERVED + live max turn RUN)

The spawn `effort` parameter (admission closed set
`low | medium | high | xhigh | max`; `minimal` and `ultra` are rejected) is
driven as the top-level `effort` of every `turn/start`; an omitted spawn
effort keeps the pre-existing wire default. Evidence layers:

- **SOURCE_INSPECTED** (codex-cli 0.154.0 binary strings): the
  `none/minimal/low/medium/high/xhigh/max/ultra` variant run sits next to
  `effort`, `reasoningEffort` / `model_reasoning_effort` and the
  `ThreadStart`/`ResumeResponse` types, so all eight tokens serialize on the
  wire enum; admission still admits only the five values below.
- **OBSERVED** (`models/list` probes 2026-09-15 and 2026-09-22): the models
  advertise one of three `supportedReasoningEfforts` shapes —
  `{low,medium,high,xhigh}`, `{low,medium,high,xhigh,max}`,
  `{low,medium,high,xhigh,max,ultra}` — none contains `minimal`; `max` is
  advertised by gpt-6-astra, gpt-reserve, the gpt-5.6 family and
  codex-auto-review.
- **OBSERVED + RUN** (2026-09-22 live probe,
  `.agent-work/tmp/codex-effort-max-20260922/`): an effort=`zzz` turn fails
  with the backend oracle `[ReasoningEffortParam] … Supported values are:
  'none', 'minimal', 'low', 'medium', 'high', 'xhigh', and 'max'` — the wire
  mapping of `max` is verified and `ultra` is excluded by the same oracle.
  A live `turn/start` effort=`max` on gpt-5.6-terra (ephemeral read-only
  thread) ran to `completed` (2.7 s, exact expected output). `minimal`
  remains out because no observed model advertises it; `ultra` remains out
  because the API oracle rejects it.
- **OBSERVED** (2026-09-15 probe): the `thread/start` result's `reasoningEffort`
  echo is the **model default, not an acknowledgement of any requested
  effort** — gpt-5.6-terra echoed `medium`, exactly its
  `defaultReasoningEffort`. The daemon treats the start echo as
  diagnostic-only and never compares against it.
- **OBSERVED** (same probe): `thread/resume` (and `thread/read`) echo the
  **last turn's effective effort** (`low` after a `low` turn). The daemon
  fail-closes with `InvalidSession` when a resume echo exists, an explicit
  effort was admitted, and the two differ. The `max` resume echo was not
  separately re-probed; the comparison is byte-equality on the admitted
  token, so a divergent echo stays a safe fail-closed `InvalidSession`.
- **OBSERVED** (`start-posture-20260919.json`, same directory): a resume
  with no leading turn fails outright — the response is the JSON-RPC error
  branch (keys `code`/`message`, the no-rollout shape recorded above), not
  a success result that merely lacks the echo. The daemon's "an absent
  resume echo is diagnostic-only and passes through" branch is therefore a
  defensive implementation pinned by the fixture tests, not a live-observed
  success shape.
- The `max` tier has a live direct app-server run (ephemeral probe thread
  above); an effort driven end-to-end through the daemon's production spawn
  path is **NOT_RUN**, and the non-`max` tiers keep their static-binary and
  unauthenticated-probe evidence only.

## Developer instructions native channel (SOURCE_INSPECTED, tag verified)

Upstream Codex app-server supports native `developer_instructions` on `thread/start`:
- **SOURCE_INSPECTED**: Verified in upstream GitHub source repository tags `rust-v0.154.0` and `rust-v0.160.0`. `ThreadStartParams.developer_instructions: Option<String>` is a standard, non-experimental field.
- **Wire serialization**: CamelCase `developerInstructions` in `thread/start` payload (`{"model": ..., "cwd": ..., "approvalPolicy": "never", "sandbox": ..., "ephemeral": false, "developerInstructions": "..."}`).
- **Echo behavior**: `thread/start` response does not echo an inline instruction text field (`instruction_sources` lists loaded instruction files). Validation is performed at outbound request frame composition; turn/start receives the caller prompt verbatim without developer instruction splicing.

## Linux x86_64, codex-cli 0.162.0 (observed 2026-10-09)

Evidence gathered with `CODEX_HOME=/tmp/es-s04-codex-home` (throwaway) and a
probe under `tools/probes/codex-app-server/probe.mjs`; the real user Codex home
was only read. The daemon drove three live turns (build/plan/yolo) that all
settled `COMPLETED` with `final_text = "pong"` for `gpt-6-luna`
([acceptance record](../acceptance/S04-linux-codex-smoke.md)).

Deltas from the 0.154.0 baseline, all additive — nothing the daemon reads moved:

- `initialize` result gained `userAgent` (`external-subagent-probe/0.162.0
  (Ubuntu 26.4.0; x86_64) dumb (external-subagent-probe; 0)`),
  `platformFamily` (`"unix"`), and `platformOs` (`"linux"`) alongside the
  existing `codexHome`.
- New server notifications observed on the wire and safely ignored by the
  daemon's `item/*`/`turn/*` folding: `remoteControl/status/changed`,
  `mcpServer/startupStatus/updated`, `thread/status/changed`,
  `thread/tokenUsage/updated`, `account/updated`,
  `account/rateLimits/updated`.
- `thread/start` result root gained `approvalsReviewer` (`"user"`),
  `activePermissionProfile`, `multiAgentMode` (`"explicitRequestOnly"`),
  `runtimeWorkspaceRoots`, `serviceTier`, `disabledPluginIds`; the embedded
  `thread` object gained `environments`, `sessionId`, `path`, `cliVersion`,
  `source` (`"vscode"`), `canAcceptDirectInput`, `historyMode`, etc. The
  pointers the daemon reads (`/thread/id`, `ephemeral`, root/thread `model`,
  `approvalPolicy`, `sandbox`, `cwd`) are unchanged.
- `turn/start` response `turn` gained `rootTurnId`, `itemsView`, `startedAt`,
  `completedAt`, `durationMs`; `/turn/id` is unchanged.
- Request-preset → resolved-sandbox echoes are byte-identical to 0.154.0:
  `workspace-write` → `{"type":"workspaceWrite","writableRoots":[],
  "networkAccess":false,"excludeTmpdirEnvVar":false,"excludeSlashTmp":false}`;
  `read-only` → `{"type":"readOnly","networkAccess":false}`;
  `danger-full-access` → `{"type":"dangerFullAccess"}`. `approvalPolicy`
  echoed `"never"`, `model`/`cwd` echoed at the result root. A live
  `item/agentMessage/delta` + `item/completed` + `turn/completed` sequence
  folded to `pong` exactly as on 0.154.0.
- Linux runtime resolution (fixed 2026-10-09, not a protocol drift): the nvm
  `codex` is a Node launcher (`#! /usr/bin/env node`), so a daemon whose `PATH`
  lacked that interpreter failed closed — the S03 systemd unit's fixed `PATH`
  had no Node, and `spawn` returned `SESSION_START_FAILED`/
  `stderr_tail: env: 'node': No such file or directory`. The Linux service PATH
  now appends `path.dirname(process.execPath)` (the directory of the Node that
  rendered the unit) after the fixed system set, exactly as the macOS fixed PATH
  carries the Homebrew bin holding its own `node`; the darwin plist is
  unchanged. Verified under a genuine `systemctl --user` service (transient
  `systemd-run` unit with the generated unit PATH): the daemon spawned
  `node …/bin/codex app-server --listen stdio://` → the vendored native codex
  binary in the unit cgroup, and live build/plan chains completed
  ([acceptance record](../acceptance/S04-linux-codex-smoke.md)).

### Plugin/MCP host-binding surface on Linux 0.162.0 (observed 2026-10-09)

The full managed binding surface was exercised live on Linux through the
product CLI with a throwaway `CODEX_HOME` and an isolated product HOME/XDG
tree ([S05 acceptance](../acceptance/S05-linux-subagents-host-bindings.md)):
`install-plugin codex` (staging → official `marketplace add` + `plugin add`,
cache verified, D08 claim), a repeat install (idempotent, claim deduplicated),
`install-plugin codex --uninstall` (official `plugin remove`, cache cleared,
claim released), `install-mcp` (TOML section written, listed by the CLI) and
`install-mcp --uninstall` (exactly the managed section removed, unrelated
sections preserved). The `--json` result shapes are **identical to the 0.154.0
macOS table above** — the diff table:

| Command | 0.154.0 macOS | 0.162.0 Linux x86_64 | Delta |
|---|---|---|---|
| `plugin marketplace add <root> --json` | `{marketplaceName, installedRoot, alreadyAdded}` | same | none |
| `plugin add <name> --marketplace <m> --json` | `{pluginId, name, marketplaceName, version, installedPath, authPolicy}` | same | none |
| `plugin remove <name>@<m> --json` | `{pluginId, name, marketplaceName}` | same | none; cache directory removed, marketplace registration kept |
| `plugin list --json` | `{installed: [...], available: [...]}` | same per-plugin fields (`pluginId`, `enabled`, `source.path`, `marketplaceSource`) | none |
| `mcp list` | (not in the macOS table) | human table: `Name / Command / Args / Env / Cwd / Status / Auth`, one row per server, exit 0 | surface present on 0.162.0 |
| `mcp list --json` | (not in the macOS table) | array of `{name, enabled, disabled_reason, transport{type,command,args,env,env_vars,cwd}, startup_timeout_sec, tool_timeout_sec, auth_status}` | surface present on 0.162.0 |

`config.toml` side effects inside `CODEX_HOME` are the same as macOS
(`[marketplaces.<name>]` with `source_type`/`source`, `[plugins."<n>@<m>"]`
with `enabled = true`, plugin tree copied into
`plugins/cache/<marketplace>/<name>/<version>`), and the managed
`[mcp_servers.external_subagent]` TOML written by `install-mcp` is listed by
`codex mcp list` with `Status: enabled`.

One environment-dependent stderr note: with `CODEX_HOME` under `/tmp`, every
codex invocation prints `WARNING: proceeding, even though we could not create
PATH aliases: Refusing to create helper binaries under temporary dir "/tmp" …`
before the JSON. It is a warning only (exit 0, JSON intact) and does not
affect any binding read-back; a `CODEX_HOME` outside temporary directories
does not produce it.

## NOT_RUN

- Installation into a real user `~/.codex` (requires explicit authorization).
- Live two-agent tasks driven by a real Codex conversation.
- Registry publication and any `npm publish` flow.

## send 双模式投递

`external_subagent_send` 的 mode 必填，仅接受 queue/steer；缺失或非法值以 validation 拒绝。

活跃 queue 使用原生并发 `turn/start{threadId,model,effort,input}` 注入当前 turn，在下一步骤边界生效，不中断执行中的命令、不建立新 turn、不打开 start_in_flight、不触碰 tracker/retirement、不等待新 turn.started。响应必须回显当前 turn id；回显不匹配时消息以 SESSION_SEND_FAILED 记为 Failed。若检查与 wire 请求之间 turn 恰好完成，provider 可能开出新 turn，消息内容可能已被消费而回执仍失败，重发存在重复风险。活跃 steer 则等待 turn/interrupt 边界落定，再 turn/start 新 turn。空闲时两种 mode 均普通 send_turn；符合现有条件的终态经 resume 接续，落库保留请求的 mode。

接入新 subagent 时原生 mid-turn 投递优先于 es 暂存，es 暂存是文档化的标准兜底。queued 仅表示 es 暂存；原生直写／注入完成即 delivered，执行结果仍由 wait/result 查询。同 message_id、同 mode、同 content 重试幂等，改变任一绑定字段会冲突。
