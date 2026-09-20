# external-subagent 产品说明

`external-subagent` 是一个本地服务，管理工作区约束的外部会话生命周期并通过 MCP 暴露。调用方（`host`）与执行目标（`subagent`）是两个独立维度：本机任意 MCP client（Codex、ZCode 或自定义 client）都可以作为 host 调用；当前支持 ZCode、DeepSeek Harness（DSH）与 Codex 三个 subagent 上游。产品边界是本地 npm 包、daemon、MCP facade 和受管宿主 plugin（Codex / ZCode），不负责安装或升级上游 provider 本体。各 subagent 的实际能力限制见下文“实际能力限制”与 [mcp-api.md](mcp-api.md)。

## 角色与实例模型

本项目将调用方与执行目标分为两类：

- `host`（调用宿主）：通过 MCP 调用本产品的上游应用，例如 Codex。`host` 是调用侧身份，不是被调度执行的任务。
- `subagent`（子代理）：由本产品路由并管理生命周期的执行目标，例如 ZCode 和 DSH。
- `adapter`（适配器）：连接某个 subagent 的内部协议实现，例如 ZCode app-server adapter 或 DSH ACP adapter；`adapter` 不是面向用户的配置层级。

`host` 不是强制注册制。除内置的 `codex` host 外，产品接受本机任意 MCP client
作为 `custom` host；它可以直接连接 MCP facade 并调用公开工具，不需要先写入
`hosts` 配置或登记一个 home。内置 host 的作用仅是提供宿主特定的安装和自动升级
协调，不承担 MCP 接入控制；host 注册不是 MCP 调用或任务提交的前置条件。

实例支持是两类对象的明确契约：

| 对象 | 当前实例模型 | 配置位置 | 约束 |
|---|---|---|---|
| `host.codex` | 支持多个 instance | `hosts.codex.installations[].home` | 每个 `home` 代表一个独立 Codex 安装绑定；同步、状态和解绑按 home 分别报告 |
| `host.zcode` | 单 instance（单用户配置，无 home 概念） | `~/.zcode/cli/config.json` 的 `plugins.dirs` | 由 `install-plugin zcode` 注册一个受管 inline plugin 目录；绑定状态从 config 无状态推导，不设注册表 |
| `host.custom` | 支持任意未注册本机 MCP client；不要求持久化 instance | 无需配置 | 连接按 MCP session 识别；不提供安装或自动升级绑定，也不要求预先登记 |
| `subagents.zcode`、`subagents.dsh` | 暂不支持多个 instance | `subagents.<name>` | 一个名称只对应一个受管 runtime/home；不承诺按任务选择多个同名实例 |

因此，Codex home 不属于产品顶层配置。它是 `host.codex` 的安装实例属性；产品可以同时管理多个 Codex home，但不能据此推导出 subagent 多实例能力。

## 安装与初始化

```sh
npm install -g external-subagent
external-subagent init
external-subagent install-plugin codex   # 可选：显式绑定 Codex 宿主（或 install-plugin zcode / install-mcp）
```

`npm pack`/`npm publish` 由生命周期脚本门禁：`prepack` 总是从源码重建 native payload（干净 checkout 永远打不出缺二进制的坏包），`postpack` 对产物做静态检查（含拒绝 debug 产物混入发布包），`prepublishOnly` 在发布前校验 staged payload 与包版本一致。包通过 `os`/`cpu` 字段声明仅支持 macOS arm64，npm 会在安装期直接拒绝其他平台。

普通 npm 安装只放置 CLI、MCP facade 和版本化 native payload。首次激活必须显式执行 `init`；它只安装独立的 daemon 服务：校验 payload，报告 PATH 发现与固定 ZCode runtime 的存在性观察（不做探测），写入产品配置和 LaunchAgent，启动服务，并发布 active/retained payload 基线——不安装任何宿主 plugin，不登记 Codex home，不触碰 `~/.codex`。宿主绑定由 `init` 之后的显式命令完成（`install-plugin codex|zcode` 或 `install-mcp` 的直接 TOML binding）。重复执行 `init`/`start` 幂等：已加载的 launchd 服务会以 `already_loaded` 和当前 PID 上报，不会出现第二个 daemon 进程。

### Debug 变体（开发机并行实例）

开发 checkout 可以构建并安装一个与正式安装完全并行的 debug 实例，互不影响：

```sh
node scripts/release/build-native-payload.mjs --variant debug   # 构建到 npm/native-debug/ 并生成 debug 插件源
bin/external-subagent-debug.mjs init                            # 独立 LaunchAgent/状态目录/socket
bin/external-subagent-debug.mjs install-plugin zcode            # 第二个 plugins.dirs 条目，插件名 external-subagent-debug
```

变体身份由 `EXTERNAL_SUBAGENT_VARIANT=debug` 派生：二进制名（`external-subagent-debugd`、`external-subagent-debug-mcp`）、payload 目录（`npm/native-debug/`）、状态目录（`~/Library/Application Support/external-subagent-debug/`）、LaunchAgent label（`com.external-subagent-debug.daemon`）、插件身份与 Codex TOML section 全部独立命名；两个实例可同时运行。debug payload 与 debug 插件源是构建产物（gitignored），且被 tarball 静态检查拒绝进入发布包。

## Provider 配置

ZCode 默认启用并使用本机固定 runtime。DSH 必须通过公开配置命令启用，并提供完整的 runtime、home、profile 和版本（配置键使用 schema-2 的 `subagents.*` 前缀；旧的 `agents.*` 键已被拒绝）：

```sh
external-subagent config set subagents.dsh.enabled true
external-subagent config set subagents.dsh.spawn_supported true
external-subagent config set subagents.dsh.runtime_path /opt/homebrew/bin/dsh
external-subagent config set subagents.dsh.home "$HOME/.dsh"
external-subagent config set subagents.dsh.profile acp
external-subagent config set subagents.dsh.version 0.1.5-rc.1
```

DSH 首发只接受 `build` 和严格 `plan`。DSH model 的选择顺序是 spawn 显式 model、配置的 `default_model`、上游 native default；ZCode 指定 model 会被明确拒绝。

spawn 可选 `effort` 参数指定逐任务推理力度：codex 只接受闭集 `low/medium/high/xhigh`（`minimal`/`max` 被拒绝），zcode 与 dsh 接受 1..24 字节 `[a-z0-9_]` 的有界透传 token；省略时保持各 subagent 现状默认，非法 token 在派发前被拒绝且不产生任务。

## 实际能力限制（来自已验收代码）

- `zcode`：默认启用且支持 spawn；四个权限模式（build/edit/plan/yolo）全部可用；spawn 显式传入 `model` 被拒绝（`model_selection_unsupported`）。
- `dsh`：默认禁用；显式启用并配置 `runtime_path`/`home`/`profile`/`version` 后才可 spawn；仅接受 `build` 和严格 `plan`。
- `codex`（作为 subagent）：默认禁用；接受 `plan`（sandbox=read-only）与 `yolo`（sandbox=danger-full-access），两者都钉死 approvalPolicy=never；`build`/`edit` 在送出 prompt 前即被拒绝。
- `observe`：已验证的公开推理只在 zcode 任务上存在；对非 zcode 任务调用 observe 会以 `unavailable` 如实报错。
- MCP `status` 只携带路由／能力／就绪结论；部署身份、配置版本、适配器传输细节和逐 scope 探测证据属于操作员诊断，经 CLI `diagnose` 读取。

## CLI 生命周期

```sh
external-subagent agents probe zcode --hi
external-subagent agents probe dsh --hi --workspace "$PWD"
external-subagent spawn --agent dsh --repository "$PWD" --permission-mode build --prompt 'Reply with exactly OK'
external-subagent wait --json '{"agent_id":10000000,"wait_time":5}'
external-subagent result --json '{"agent_id":10000000}'
external-subagent close --json '{"agent_id":10000000}'
```

任务必须显式指定 agent、repository、permission mode 和 prompt。`wait`、`result`、`close` 在终态后保持可重复调用；取消和 daemon 重启会保留终态并回收运行资源，不会自动重放不确定 prompt。

## Codex plugin 与 MCP

完整工具与字段说明见 [MCP 工具与字段设计说明](mcp-api.md)，包含每个参数的使用时机、必填条件、默认值和省略／移除影响。

内置 `codex` host 只负责宿主集成：支持两种安装方式。显式执行
`install-plugin`（等价 `install-plugin codex`）会调用官方 Codex CLI，将受管 plugin 安装到
`$CODEX_HOME/plugins/cache`，并通过本地 marketplace 注册（`init` 不做任何宿主绑定）；`install-mcp` 则写入
直接的 TOML MCP binding。两种方式都连接同一个 MCP facade，并由 Codex host 的
home 绑定参与后续自动升级协调。任意 `custom` host 无需执行这些安装步骤，可直接
连接 facade。

内置 `zcode` host 的安装入口是 `install-plugin zcode`：ZCode 没有官方 headless CLI，
该命令把同一受管 plugin 物化到产品自有目录
（`~/Library/Application Support/external-subagent/zcode-plugin/external-subagent/`），
并在 `~/.zcode/cli/config.json` 的 `plugins.dirs` 追加一个 inline 目录条目（插件身份
`external-subagent@inline`，默认启用）。这是纯配置面注册，不触碰 ZCode 自有的
marketplace/cache/安装记录状态；配置合并原子并保留全部无关键，无法识别的结构以
`ZCODE_CONFIG_INVALID` fail-closed，同名外部插件目录以 `ZCODE_PLUGIN_CONFLICT`
拒绝，卸载只摘除本产品写入的条目与目录。绑定不设注册表：状态从 config 无状态
推导，npm 更新的 reconcile 会在检测到绑定时自动刷新 staging。详见
[compatibility/zcode.md](compatibility/zcode.md)。

plugin manifest 的 `version` 是 Codex 全局 content-store 的缓存身份（`plugin@marketplace@version`），每个发布候选必须携带独立版本（最终候选 C2 为 `0.1.2`；修复前的 C1 是 `0.1.1`，两次消费运行已分别把各自身份写入全局 store，后续候选同样需要再提升版本），否则已缓存同一身份的其他 home 会覆盖新候选的字节。C2 已于 2026-09-13 完成自己的 fresh consumer 验收（`C2_FRESH_CONSUMER_VERIFICATION_PASS`）：通过公开安装面装入隔离 prefix，`install-plugin` 回执 `cache_verified: true` 且 cache 与 staged binding 逐字节一致，四格矩阵（DSH/ZCode × 公开 CLI / 真实 Codex CLI→受管 plugin→MCP）全部真实上游任务 COMPLETED；C1 的四格证据保留为该候选的历史记录，不随身份提升继承。安装器在返回成功前会读回实际 cache 的 `.mcp.json`/manifest 并与本次 staged binding 比对：复用了其他 binding 的字节以 `CODEX_CACHE_BINDING_MISMATCH` 显式失败，cache 缺失或不可读以 `CODEX_CACHE_UNVERIFIABLE` 显式失败（均 fail-closed——不存在返回 `installed`/`cache_verified: false` 的路径，cache 缺失本身就使整个安装失败）；只有校验通过的 cache 才会以 `cache_verified: true` 返回并记录 installed/claimed/updated，且安装器从不改动 Codex cache 本身；详见 [compatibility/codex.md](compatibility/codex.md)。

也可以使用 `install-mcp` 写入直接的 TOML MCP binding，适用于不支持 plugin marketplace 的 Codex 版本。两种方式都只管理本产品自己的 binding，并保留其他 marketplace、plugin 和 enabled 状态。

通过 Codex CLI 验证时，Codex 应实际调用 MCP 工具完成 `spawn → wait → result → close`，不能用 `tools/list` 或直接 facade smoke 代替真实任务调用。同一候选的四格验收矩阵（DSH/ZCode × CLI/真实 Codex CLI→MCP，全部真实上游任务）记录于 [acceptance/productization.md](acceptance/productization.md)。

## 更新与限制

初始化后的 npm 更新由 postinstall/reconcile 复用同一个受控 update owner，候选 payload 在排空前验证，旧 payload 保留用于恢复；首次 npm 安装仍保持 stage-only。`--ignore-scripts` 时不会假报已协调，下一次显式 `update`/`reconcile` 可恢复。

Codex homes 的同步按 home 逐项报告：某次更新已完成 payload 激活与服务切换但存在无法重绑的 home（如只读目录）时，命令以 `CODEX_SYNC_PARTIAL` 非零退出，错误信息与 `partial` 回执列出每个未完成 home 的状态与原因；已完成的激活与已成功 home 不会回滚，修复后运行 `external-subagent reconcile` 幂等补齐剩余 home。

## 诊断、恢复与移除

`status`/`diagnose` 只读区分包内 payload、active 状态、launchd 注册、RPC 可用性和日志证据；损坏的 install-state/codex-homes 会保留 `.corrupt-*` 字节并按可恢复错误上报，不会被当作空状态覆盖。`stop` 在返回前确认 launchd 任务确实移除（有界等待），重复 stop 幂等；`uninstall` 先引导退出本产品自有服务再删除 LaunchAgent，并释放全部 Codex home 登记，同时保留任务数据、provider 凭据；`restore` 后需 `stop`+`start` 使 daemon 回到恢复的数据库。受管 plugin/MCP 解绑由 `install-plugin --uninstall` / `install-mcp --uninstall` 单独完成。

已验证的本地能力包括双 provider 的隔离任务、MCP 工具调用、活跃任务排空、取消/重启恢复、版本化 payload 和 Codex plugin 安装。真实用户 GUI 会话中的 launchd bootstrap 与重复 init/start 幂等已实测验证（含 label/PID/socket/RPC 证据）；最终候选 C2（`0.1.2`）的四格 fresh consumer 验收已在真实 GUI 会话通过（真实 launchd 服务、公开 `stop` 清理）；Codex GUI 热重载、npm registry 发布和未覆盖 provider 的完整矩阵仍需单独验收。
