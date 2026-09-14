# external-subagent 产品说明

`external-subagent` 为 Codex 提供受工作区约束的外部会话生命周期管理，当前支持 ZCode 与 DeepSeek Harness（DSH）两个上游。产品边界是本地 npm 包、daemon、MCP facade 和受管 Codex plugin，不负责安装或升级上游 provider 本体。

## 安装与初始化

```sh
node scripts/release/build-native-payload.mjs
npm pack
npm install -g external-subagent-0.1.0.tgz
external-subagent init --codex-home "$HOME/.codex"
```

普通 npm 安装只放置 CLI、MCP facade 和版本化 native payload。首次激活必须显式执行 `init`；它会校验 payload，写入产品配置和 LaunchAgent，安装并启用 `external-subagent@personal` Codex plugin，登记 Codex home，并发布 active/retained payload 基线。重复执行 `init`/`start` 幂等：已加载的 launchd 服务会以 `already_loaded` 和当前 PID 上报，不会出现第二个 daemon 进程。

## Provider 配置

ZCode 默认使用本机固定 runtime。DSH 必须通过公开配置命令启用，并提供完整的 runtime、home、profile 和版本：

```sh
external-subagent config set agents.zcode.enabled true
external-subagent config set agents.zcode.spawn_supported true
external-subagent config set agents.dsh.enabled true
external-subagent config set agents.dsh.spawn_supported true
external-subagent config set agents.dsh.runtime_path /opt/homebrew/bin/dsh
external-subagent config set agents.dsh.home "$HOME/.dsh"
external-subagent config set agents.dsh.profile acp
external-subagent config set agents.dsh.version 0.1.5-rc.1
```

DSH 首发只接受 `build` 和严格 `plan`。DSH model 的选择顺序是 spawn 显式 model、配置的 `default_model`、上游 native default；ZCode 指定 model 会被明确拒绝。

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

初始化或显式执行 `install-plugin` 会调用官方 Codex CLI，将 plugin 安装到 `$CODEX_HOME/plugins/cache`，并通过本地 marketplace 注册。plugin 的 MCP server 使用绝对路径连接产品 facade；当前公开工具包括 `external_subagent_spawn`、`wait`、`result`、`close`、`cancel`、`send`、`respond`、`observe`、`list` 和 `status`。

plugin manifest 的 `version` 是 Codex 全局 content-store 的缓存身份（`plugin@marketplace@version`），每个发布候选必须携带独立版本（最终候选 C2 为 `0.1.2`；修复前的 C1 是 `0.1.1`，两次消费运行已分别把各自身份写入全局 store，后续候选同样需要再提升版本），否则已缓存同一身份的其他 home 会覆盖新候选的字节。C2 已于 2026-09-13 完成自己的 fresh consumer 验收（`C2_FRESH_CONSUMER_VERIFICATION_PASS`）：通过公开安装面装入隔离 prefix，`install-plugin` 回执 `cache_verified: true` 且 cache 与 staged binding 逐字节一致，四格矩阵（DSH/ZCode × 公开 CLI / 真实 Codex CLI→受管 plugin→MCP）全部真实上游任务 COMPLETED；C1 的四格证据保留为该候选的历史记录，不随身份提升继承。安装器在返回成功前会读回实际 cache 的 `.mcp.json`/manifest 并与本次 staged binding 比对：复用了其他 binding 的字节以 `CODEX_CACHE_BINDING_MISMATCH` 显式失败，cache 缺失或不可读以 `CODEX_CACHE_UNVERIFIABLE` 显式失败（均 fail-closed——不存在返回 `installed`/`cache_verified: false` 的路径，cache 缺失本身就使整个安装失败）；只有校验通过的 cache 才会以 `cache_verified: true` 返回并记录 installed/claimed/updated，且安装器从不改动 Codex cache 本身；详见 [compatibility/codex.md](compatibility/codex.md)。

也可以使用 `install-mcp` 写入直接的 TOML MCP binding，适用于不支持 plugin marketplace 的 Codex 版本。两种方式都只管理本产品自己的 binding，并保留其他 marketplace、plugin 和 enabled 状态。

通过 Codex CLI 验证时，Codex 应实际调用 MCP 工具完成 `spawn → wait → result → close`，不能用 `tools/list` 或直接 facade smoke 代替真实任务调用。同一候选的四格验收矩阵（DSH/ZCode × CLI/真实 Codex CLI→MCP，全部真实上游任务）记录于 [acceptance/productization.md](acceptance/productization.md)。

## 更新与限制

初始化后的 npm 更新由 postinstall/reconcile 复用同一个受控 update owner，候选 payload 在排空前验证，旧 payload 保留用于恢复；首次 npm 安装仍保持 stage-only。`--ignore-scripts` 时不会假报已协调，下一次显式 `update`/`reconcile` 可恢复。

Codex homes 的同步按 home 逐项报告：某次更新已完成 payload 激活与服务切换但存在无法重绑的 home（如只读目录）时，命令以 `CODEX_SYNC_PARTIAL` 非零退出，错误信息与 `partial` 回执列出每个未完成 home 的状态与原因；已完成的激活与已成功 home 不会回滚，修复后运行 `external-subagent reconcile` 幂等补齐剩余 home。

## 诊断、恢复与移除

`status`/`diagnose` 只读区分包内 payload、active 状态、launchd 注册、RPC 可用性和日志证据；损坏的 install-state/codex-homes 会保留 `.corrupt-*` 字节并按可恢复错误上报，不会被当作空状态覆盖。`stop` 在返回前确认 launchd 任务确实移除（有界等待），重复 stop 幂等；`uninstall` 先引导退出本产品自有服务再删除 LaunchAgent，并释放全部 Codex home 登记，同时保留任务数据、provider 凭据与旧 ZAS；`restore` 后需 `stop`+`start` 使 daemon 回到恢复的数据库。受管 plugin/MCP 解绑由 `install-plugin --uninstall` / `install-mcp --uninstall` 单独完成。

已验证的本地能力包括双 provider 的隔离任务、MCP 工具调用、活跃任务排空、取消/重启恢复、版本化 payload 和 Codex plugin 安装。真实用户 GUI 会话中的 launchd bootstrap 与重复 init/start 幂等已实测验证（含 label/PID/socket/RPC 证据）；最终候选 C2（`0.1.2`）的四格 fresh consumer 验收已在真实 GUI 会话通过（真实 launchd 服务、公开 `stop` 清理）；Codex GUI 热重载、npm registry 发布和未覆盖 provider 的完整矩阵仍需单独验收。
