# DeepSeek Harness ACP compatibility

This document records the bounded discovery probe contract. It is not a claim that DSH is supported by the product.

The probe is `tools/probes/dsh-acp/probe.mjs`. It accepts an explicit executable path and argv, speaks JSON-RPC over stdio, and never installs a provider or writes credentials. `--help` lists the only scenarios. The caller must supply the exact executable and arguments obtained from its installed DSH version; the probe does not invent `dsh app-server`, login, or profile commands.

Observed fixture shapes cover `initialize`, `session/new`, `models/list`, `session/prompt`, `session/update`, `session/request_permission`, `session/cancel`, EOF, and malformed input. `models/list` is catalog evidence only. ACP `initialize` success is not provider authentication evidence; a real auth/hi result must be recorded separately for the exact executable, environment, workspace, and configuration revision.

The product's explicit `agent_models` RPC consumes only the already observed `initialize`, `session/new`, and `models/list` shapes. A `model` option value is the byte-exact JSON tuple `["provider","model"]` (the same bytes JSON.stringify emits); the RPC re-projects such a tuple for display as `provider:model`, while parse failures, non-two-element arrays, and colon-bearing provider sides are returned verbatim. It always cleans up its bounded discovery process. Catalog success does not enable DSH production spawn or imply provider authentication.

Catalog discovery isolates the provider in a process group and cleans the group with TERM/KILL plus leader wait on success and failure. Protocol stdout uses an incremental 1 MiB frame cap, and diagnostic stderr uses a 64 KiB bounded reader with a bounded receive deadline; descendants inheriting stderr cannot hold discovery open.

The shared executable-version check applies the same process-group and incremental-reader bounds, including cleanup when a leader exits while a descendant still owns stdout/stderr.

DSH live status (2026-09-12): **PARTIALLY VERIFIED** against DSH `0.1.5-rc.1` in a disposable workspace. Real ACP `initialize`, `session/new`, and `session/prompt` completed; the default-model prompt returned `LIVE_OK` with `end_turn`. Under the strict patch profile, shell/write probing exposed only the expected `glob`/`grep` operations and created no target file.

These observations validate the ACP wire path and the bounded strict-plan probe. Production daemon routing is implemented behind the explicit `enabled + spawn_supported + DSH_RUNTIME_PATH` gate; Persisted `agents.dsh.runtime_path`, `home`, and `profile` are carried into the LaunchAgent environment and consumed by the DSH launch profile; an unsupported profile fails closed. `agents.dsh.version` is advisory evidence only: a difference from the daemon-tested reference never refuses a launch and is surfaced as `version mismatch, it may not work as expected` (byte-exact) in status and in failed-terminal wait/result instructions. An isolated daemon smoke completed `spawn → wait → result → close` with `DAEMON_LIVE_OK`, `COMPLETED`, and `resources_reaped=true`. Explicit provider auth/hi probes were executed, but a successful authenticated hi remains **UNVERIFIED** (results below). Codex/ZCode host integration remains **NOT_RUN**. Daemon cancellation is live-verified with reaping and terminal-state preservation across wait/result/close; restart recovery is live-verified as `RUNTIME_LOST` with no replay. The default configuration remains closed; this document does not claim that an unconfigured deployment is enabled.

An isolated strict-plan daemon submission completed through ACP. A prompt that
requested shell/write activity received no shell or write tool and the model
declined to claim the write; the task was reaped and the workspace remained
empty. This confirms the safety refusal path for the installed runtime.

## DSH 0.2.0-rc.2 compatibility (2026-10-17)

`PINNED_DSH_VERSION` (the daemon-tested reference, formerly a launch pin) is
now `0.2.0-rc.2`. Version equality is no longer a gate anywhere: the preflight
still probes `--version` to prove the runtime starts and parses a version token,
but never compares it. A difference produces only the advisory string
`version mismatch, it may not work as expected` in MCP status and in
failed-terminal wait/result instructions (expected is the dsh reference;
observed is the bounded local probe).

Captured evidence and fixtures (tests include them verbatim):
`crates/subagents/dsh/fixtures/dsh-0.2.0-rc.2-{build,strict-plan,manifest-build}.dump.yml`.
They were captured with the installed `/opt/homebrew/bin/dsh` 0.2.0-rc.2:
`DSH_PERMISSION_MODE=workspace-write dsh --profile acp --dump-config`, the same
command with `--patch profiles/dsh/strict-plan.patch.yml` and
`DSH_PERMISSION_MODE=read-only`, and the same with a generated
manifest-build patch. The first two produced empty stderr; the updated strict
patch emits no `patch:` error and `validate_dump_policy` passes (unit test).
Live spawn against a provider remains **NOT_RUN** (no provider credentials are
used by this bounded check).

Strict-plan patch changes for 0.2.0-rc.2:

- removed `workflow-worker-thread` (the entry no longer exists upstream);
- added disable rows `workflow-ptc`, `ptc-runtime`, `config-editor`,
  `plugin-manager`, and `mcp-resources`;
- kept `goal-round-driver`, `skill-filesystem`, and the earlier execution,
  write, spawn, and workflow rows.
- `DANGEROUS` now also contains `workflow-ptc`, `ptc-runtime`, `config-editor`,
  `plugin-manager`, and `tool-plugin-manager` (the last is disabled upstream by
  default, so it has no patch row). `ALLOWED_ENABLED` gained `authorization`,
  `deepseek-account`, `llm-deepseek-account`, `image-offload`, `otel`,
  `skill-badge`, and `mcp-resources`; unknown enabled entries still fail
  closed. `mcp-resources` was classified by reading the installed package
  source: it only exposes `resources/list`, `resources/templates/list`, and
  `resources/read`, with no write/execute path, so it is allowlisted; the
  managed strict patch keeps it disabled by default.

Silent patch-failure guard (all three preflights): DSH 0.2.0-rc.2 exits 0 even
when a patch row is missing, printing
`dsh: [<path>] patch: entry "<id>" not found` on stderr. The preflights now
reject any stderr line containing both `patch:` and `not found`; a clean dump
(and a benign non-patch warning line) still passes. This keeps an old runtime
with the strict/manifest patch on the existing fail-closed path: the patch row
is missing, the dump is refused, and the advisory version mismatch is reported.
A legacy no-patch build is not refused for its version: it spawns when
the existing dump controls validate.

Build composition posture (accepted upstream drift): 0.2.0's default build dump
additionally enables `plugin-manager`, `config-editor` (via a JS expression
that does not evaluate to `disabled: true`), `workflow-ptc`, `ptc-runtime`,
and `mcp-resources`. `validate_build_dump` deliberately pins only the managed
control entries (sandbox policy, approval, permission presets, sandbox
executors, fs-sandbox, acp), so these upstream defaults are accepted rather
than adding a new allowlist walk or a new minimal patch to a byte-identical
legacy contract. Residual risk, accepted knowingly: `plugin-manager` and
`config-editor` may persist configuration under `DSH_HOME`, outside
`workspace-write`; the build composition already enables `tool-bash`, so this
is not a new execution surface, and the risk is recorded here for a future
narrowing decision. Manifest-build is unaffected because its generated patch
disables the new entries and keeps the fail-closed allowlist walk.

## Host 绑定（install-plugin dsh）

DSH 除作为 subagent 外也可作为 host 调用本产品。内置绑定命令是
`external-subagent install-plugin dsh --profile <name>`（`--uninstall` 对称移除，
`--dsh-home <path>` 覆盖目标 home，默认 `$DSH_HOME` 环境变量或 `~/.dsh`）：

- **MCP 绑定**：向 `$DSH_HOME/profiles/<name>/cordis.patch.yml` 追加一个带
  哨兵注释的受管块，内容是一条 `@deepseek-ai/dsh-mcp-client` stdio 条目
  （固定 `id: mcp-external-subagent`、`serverName: external_subagent`，command
  为 native MCP facade 绝对路径，`EXTERNAL_SUBAGENT_SOCKET` 钉死 daemon
  socket，`toolCallTimeoutMs: 300000` 覆盖 dsh 默认 60s ——
  `external_subagent_wait` 上限 299s，与 zcode 绑定的 timeoutMs 同理）。
  id 是 dsh 插件管理的前提：insert 行的 `id` 在 dsh 语法里可省，但省略时
  loader 每次启动生成随机 8 位 hex 且不落盘，dsh 自己的 plugin manager 会把
  该行标为 `unaddressable`（只读、无法开关）；钉死 id 后 GUI 插件页与本产品
  都能稳定寻址。追加是**文本级事务**：不 parse/rewrite 用户的 YAML，
  用户注释与条目逐字节保留；全新模板末尾的 `[]` 空根行先摘除再追加
  （OBSERVED 0.2.0-rc.2 模板形态）。重装时哨兵块整体替换（socket/facade
  路径漂移自愈，旧版无 id 块同样被替换补上 id），字节等价则报
  `idempotent`。
- **dump 验证**：写入后执行 `dsh --profile <name> --dump-config`
  （`DSH_PERMISSION_MODE=read-only`，30s 有界），stderr 出现针对**本条目 id**
  的 `patch:` 诊断即 fail-closed（0.2.0-rc.2 对缺失 patch 行也 exit 0，
  与 daemon preflight 同一守卫），dump 必须包含本条目；与本次绑定无关的
  存量诊断（用户自己的 override 指向缺失条目等）降级为回执 warning，不
  阻断——长期使用的 profile 组合里这类噪音常见，且镜像验证下 bundle 缺失
  也会产生同源噪音。失败时 patch 字节与已 staged skill 全部回滚。
  `desktop` profile 的验证走**镜像 dump**：npm dsh CLI 与 Electron 自带 CLI
  都按名字拒绝 desktop 的 `--dump-config`（OBSERVED 0.2.1-rc.3，拒绝仅看
  名字），安装器把 profile 的组合输入（cordis.yml、cordis.patch.yml、
  package.json、pnpm-*.yaml，**不含 node_modules**——symlink 会让 dsh 的
  link-projection 清理穿透写真实 profile）复制进一次性 DSH_HOME 的镜像
  名下，用 Electron 自带 CLI（macOS
  `/Applications/DeepSeek Harness.app/Contents/Resources/runtime/cli/bin/dsh`，
  Windows `%LOCALAPPDATA%\\DeepSeek Harness\\resources\\runtime\\cli\\bin\\dsh.cmd`）
  dump；Electron CLI 缺失时降级为 `config_verified: false` + warning，把
  最终确认留给 GUI 插件页。
- **Skill 装载**：`SKILL.md` 复制到 `$DSH_HOME/skills/external-subagent/`
  （dsh-skill-filesystem 默认扫描的 rank-400 user root，无需改 profile）；
  该位置已有非产品内容时以 `DSH_SKILL_CONFLICT` 拒绝，绝不覆盖。
- **profile 校验**：`--profile` 必填且目录必须已存在（fail-closed 并提示
  `--from-default-profile` 创建）；名字含路径分隔符拒绝。`desktop` 不再被
  拒绝：写入路径与 dump 无关，验证走上面的镜像 dump（Linux 上没有
  Electron 安装，desktop 仍被拒绝）。
- **裸包名解析**：受管条目用裸包名 `@deepseek-ai/dsh-mcp-client`，由 dsh
  loader 从 CLI 自带安装树解析（实测全新 DSH_HOME 无 profile node_modules
  也能干净组合）。安装时探测 CLI 安装树中该包的存在性，缺失时回执附
  warning 并给出 `dsh plugin --profile <name> add ...` 补装指引，不静默。
- **无注册表**：绑定状态从 patch 哨兵无状态推导（zcode 模式）。reconcile
  扫描 `$DSH_HOME/profiles/*/cordis.patch.yml` 的哨兵并逐 profile 重绑（含
  desktop）；`uninstall` 释放所有哨兵 profile 并移除共享 skill（仅当内容仍是
  产品字节），同时剥离携带本条目 id 的顶层 `disabled` 覆盖行（dsh 的
  plugin manager 开关后留下的 `{id, disabled}` 行——块删掉后它指向空目标，
  dsh 每次启动报 `patch: entry not found`，且会被本产品下次安装的 dump
  验证误判为失败；携带 `name`/`config` 的同 id 用户行不属此列，保留）。
  部分失败按 profile 逐项上报（`DSH_SYNC_PARTIAL`），可重跑 reconcile 补齐。
- **边界**：dump 验证只证明组合合法；真实工具注入需用户 boot 该 profile 后
  自行确认。用户手工破坏哨兵块时，卸载走 `serverName: external_subagent`
  兜底搜索并 warning，绝不改动用户条目。

## Launch compositions (implemented)

DSH spawn has three explicit compositions, selected by the daemon factory from
the admitted permission mode and write manifest:

- **strict plan** (`permission_mode: plan`): the strict-plan patch runs DSH with
  `read-only` and every write/execute tool disabled. The write manifest must be
  empty; a non-empty input is refused by admission with `validation` and no
  task is created.
- **build, caller-empty or explicit `["."]`**: the legacy build composition is
  byte-for-byte unchanged — `workspace-write`, no patch, `preflight_build`.
  An explicit `["."]` previously shared the "any non-empty manifest" refusal;
  it is now admitted through this existing composition.
- **build with a non-empty caller manifest**: per task the daemon materializes
  the embedded `dsh-write-guard` package plus a nested
  `node_modules/@deepseek-ai/dsh-fs` symlink into a fresh
  `external-dsh-manifest-` TempDir, writes the managed patch (`sandbox-policy`
  `workspace-write`; every strict-plan tool except `tool-fs` disabled; one
  `insert` row mounting the guard by absolute path with the caller manifest),
  and requires `preflight_build_manifest` before ACP starts. The guard's
  `fs/write-intent`/`fs/edit-intent` listeners are registered `prepend`, so an
  out-of-manifest path becomes a real `FsError` with code
  `FS_WRITE_MANIFEST_DENIED` (message marker `[write-guard]`) before `tool-fs`
  writes anything. The manifest is bounded to 256 entries / 64 KiB serialized;
  the TempDir is owned by the runtime owner and reaped with the task.

Admission never trusts the child dump: `validate_manifest_build_dump` also
checks the materialized patch's `--dump-config` output (guard name normalized
to a `file://` URL, manifest equality, `tool-fs` enabled, the remaining
strict-plan disable set, exactly one `workspace-write` sandbox policy and the
build approval/permission presets; unknown enabled entries fail closed).

## Model selection via ACP set_config_option (implemented; live NOT_RUN)

The spawn `model` token for dsh is `{provider}:{model}`, split at the
**first** `:`; the model side may itself contain further colons and neither
side is restricted to a character set. Admission (the same code path that the
configured `agents.dsh.default_model` uses) refuses a token with no colon, an
empty provider (`:model`), an empty model (`provider:`), a NUL byte, or more
than 512 bytes, with an error naming the `provider:model` format; the token is
trimmed before validation, as before. After `session/new` and before the first
`session/prompt`, `set_model` re-serializes the parsed sides with serde_json as
the byte-exact two-element JSON string `["provider","model"]` and sends that as
the `session/set_config_option` `value`; the colon token itself is never sent
raw. `input_identity.model` persists the trimmed colon token unchanged, and
`model_source` keeps its `spawn_catalog` / `configured_default` / `native`
semantics. Membership of the provider's real catalog is still decided by the
session: an unknown tuple makes `session_start` fail with `-32602` before any
prompt is sent. Codex keeps its own model id and zcode still rejects model
selection; the reasoning-effort channel is unchanged.

## Reasoning effort via ACP set_config_option (implemented; live NOT_RUN)

The spawn `effort` token for dsh is applied through the same
`session/set_config_option` (X05) contract as model selection: one call
after `session/new` and before the first `session/prompt`, omitted entirely
when no effort was admitted. Three branches, mirroring `set_model`: a token
failing the bounded opaque-token validation refuses with no wire traffic; a
server that advertised `configOptions` without a `reasoning_effort` entry
fails closed as `NotOffered` (no set-option frame is sent); a server that
did not advertise `configOptions` at all gets the set option forwarded and
the server's verdict decides. Every failure sets `effort_refused`, which
permanently blocks `prompt` for that session exactly like a refused model
selection. There is no resume re-application point: the dsh runtime owner
has no resume entry and re-runs the full bootstrap sequence on every claim.
Live verification against a real DSH provider is **NOT_RUN** (user
prohibition on production spawns); the branches are pinned by the ACP
fixture tests.

The explicit daemon probe records local discovery as
`READY/<observed version>` (with `0.2.0-rc.2` as the daemon-tested reference),
auth as `UNKNOWN/auth_not_probed`, and the strict no-tool
hi request as `UNAVAILABLE/remote`; no credentials are modified. A successful
authenticated hi remains unverified.
Auth-only probing does not create a session or send a prompt; hi probing is the
separate strict read-only operation.

## send 双模式投递

`external_subagent_send` 的 mode 必填，仅接受 queue/steer；缺失或非法值以 validation 拒绝。

活跃 queue 由 es 暂存，在既有 turn 边界投递；这是对旧版活跃时直接失败的行为改进。空闲 queue 沿用 session/prompt 路径。steer 始终公开返回 steer_unsupported，不降级为 queue，也不尝试硬杀。原生并发 prompt 实测 -32602，session/cancel 实测 -32601。

接入新 subagent 时原生 mid-turn 投递优先于 es 暂存，es 暂存是文档化的标准兜底。queued 仅表示 es 暂存；原生直写／注入完成即 delivered，执行结果仍由 wait/result 查询。同 message_id、同 mode、同 content 重试幂等，改变任一绑定字段会冲突。
