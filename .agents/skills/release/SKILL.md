---
name: release
description: 发布 external-subagent 新版本到 npm（tag 驱动的 GitHub Actions Trusted Publishing 流程）。用户要求发版、发布新版本、bump 版本、打 release tag、npm publish、排查发布或打包门禁失败、验证发布 tarball 时使用。
---

# external-subagent 发布流程

## 常规发版（tag 驱动，全自动）

1. bump 版本——**四处必须一致**（payload manifest 由 package.json 生成，门禁只校验 manifest 与包版本一致，校验不到二进制内嵌版本）：
   - `package.json` 的 `version`
   - `cli/constants.mjs` 的 `VERSION`
   - 全部 crate 的 `Cargo.toml` `version`（daemon 自报 identity 版本来自 `env!("CARGO_PKG_VERSION")`，漏 bump 会发布出自报旧版本的二进制）
   - 变更后跑一次 `cargo metadata` 刷新 `Cargo.lock`
   - 教训（v0.1.1）：只 bump 前两处时，tarball 里的 daemon 自报 0.1.0，`external-subagent update` 的健康检查因 running daemon identity 与所选 payload 不匹配而 SERVICE_HEALTH_FAILED，安装卡死在旧版；v0.1.1 因此作废，由 v0.1.2 取代
2. 提交并推送 `main`
3. `git tag vX.Y.Z && git push origin vX.Y.Z`
4. GitHub Actions（`.github/workflows/npm-publish.yml`）在 Apple Silicon runner 上构建并发布。环境里没有任何 npm token，凭证是运行器的 OIDC 身份（npm Trusted Publishing）
5. 验证：`npm view external-subagent version` 等于新版本号；Actions run 为绿（registry 传播可滞后数分钟，`npm view external-subagent@X.Y.Z` 先 404 不代表失败——去 run 日志确认 `+ external-subagent@X.Y.Z` 行）
6. 发布后本机升级 + 生产冒烟（见下节清单）

## 发布后本机升级 + 生产冒烟（必做）

tag 转绿只证明"CI 环境里二进制能过门禁"；本机生产环境有 CI 复现不了的维度（真实 home、
PATH 解析出的运行时**路径形态**、已存在的数据/绑定）。v0.6.4 的教训：CI 与 scratch 验证
全绿，但生产 daemon 把 codex 解析成 `.js` 启动器路径（npm 布局），触发 script 分支的参数
重复 bug，目录探测 degraded——scratch 验证恰好用了同二进制的无扩展名符号链接形态，只覆盖
了另一分支。

1. `npm install -g external-subagent@X.Y.Z`（等 registry 传播）
2. `external-subagent version` 报新版本
3. `external-subagent update` → `phase: "active"`、`active.version` = 新版本
4. 确认 daemon 进程从 `payload-store/X.Y.Z/` 起且启动时间是刚刚（`pgrep -f external-subagentd` + `ps -p <pid> -o lstart,command`）
5. **生产 socket 冒烟**本次变更真正改的行为：对 `~/Library/Application Support/external-subagent/external-subagent.sock` 直接发一条真实请求（node net 客户端，帧 = `{"request_id":"<uuid>","method":<m>,"params":{…}}\n`；注意 adjacent-tag 下 unit variant 的 `system_status` **不带** params 键），断言结果非 degraded
6. scratch 验证不替代本步骤——scratch 的配置/解析形态是人为构造的，必须至少一条请求打到生产 socket

## CI publish job 失败处置

publish job（"Run release-path test suites" 步）在 macOS runner 上有**已知的间歇失败家族**，
同签名历史上反复出现（v0.5.3/v0.5.4/v0.6.4）：

- `dsh::tests::abort_drain…` / `aborting a drain reopens real provider admission…`：
  tests/upgrade/*.test.mjs 的 cargo oracle（`cargo test -p external-daemon <name> -- --exact`
  逐个单跑）驱动真实 script 子进程，负载高的 runner 上 I/O 同步超时（150s 死限）。
  代码无关；本地单跑同一 oracle 数秒即绿即可确认。
- `the win32 platform seam …` / 版本 disagree 类（v0.5.3/v0.5.4 曾发）：结构性失败，
  **不是**间歇家族，按真实回归处理。

处置判据与流程：

1. `gh run view <id> --log` 找 `✖` 行，与上面家族比对签名
2. 命中间歇家族 → 先本地复跑对应 oracle（`node --test tests/upgrade/draining.test.mjs`
   或直接 `cargo test -p external-daemon <过滤串> -- --exact`）；本地绿 + 签名匹配 → 判定环境性
3. **rerun 前的安全检查（必做）**：`npm view external-subagent@X.Y.Z version`——404 才允许
   `gh run rerun <id> --failed`；rerun 会重新执行 Publish 步，若版本已在 registry 会撞
   EPUBLISHCONFLICT 并浪费一次运行
4. rerun 后仍失败 → 换新版本号修复发版，不要反复 rerun（同 tag rerun 上限与时间成本都不划算）

## 发布门禁（自动生效，排障先看这里）

npm lifecycle 串起三道门禁，本地 `npm pack` 与 CI publish 都会全部走过：

- `prepack` → `scripts/release/build-native-payload.mjs`：cargo release 构建，重写 `npm/native/darwin-arm64/payload.json`（product=external-subagent）。干净 checkout 不可能打出缺二进制的坏包
- `postpack` → `scripts/release/check-native-tarball.mjs`：必要条目检查；禁止开发材料（`.agent-work`/`target`/`tests`/数据库/日志）与 debug 产物（`npm/native-debug/`、`plugins/codex/external-subagent-debug/`）；tarball 内 payload 版本必须等于包版本；本地 staged 二进制必须是 Mach-O arm64 且 mode 755。`npm publish` 时 tarball 在 npm 私有临时目录，脚本自动降级为 staged 检查（会打 notice，不是失败）
- `prepublishOnly` → 同脚本 `--staged` 模式

`files` 白名单精确到 `npm/native/` 与 `plugins/codex/external-subagent/`，从源头挡住 debug 构建产物混入发布包。

## 硬约束（违反即失败或不可逆）

- **同一版本不可重发**：已发布版本的 tag 永远不要打（`v0.1.0` 已消费）。发版只能前进到新版本号
- workflow 文件名 `npm-publish.yml` 是 npm Trusted Publisher 绑定的组成部分，**不能改名**（绑定的用户名/仓库同理）
- `repository.url` 必须与 GitHub 仓库精确一致（OIDC 发布校验项，fork 里发布会失败）
- 平台是 darwin-arm64：runner 必须 Apple Silicon（`macos-latest`）；`package.json` 的 `os`/`cpu` 让 npm 在安装期直接拒绝其他平台
- 插件 manifest 版本（`plugins/codex/external-subagent/.codex-plugin/plugin.json`，当前 0.1.x 系列）与产品版本**有意分离**：它是 codex 插件缓存身份（`plugin@marketplace@version`），插件内容变更时按 README "Versioning" 规则单独 bump，不要跟产品版本对齐
- 版本号必须四处一致：`package.json` / `cli/constants.mjs` / 全部 crate `Cargo.toml` / payload manifest（构建时生成，不用手改）；门禁只看 manifest，Cargo.toml 漏 bump 门禁不报错但产物自报旧版本

## 本地验证（不触 registry）

- `npm pack` —— 走全部门禁后产出 tarball（留在 cwd），可 `tar -tzf` 检查内容
- `node scripts/release/test-installed-tarball.mjs` —— 隔离 prefix 安装冒烟
- 本地 `npm publish` 不应使用：账号开了 2FA（需要 `--otp=六位码`），且迁移目标是仅 CI 发布。历史上仅 npm 不支持新包 Trusted Publishing 首发时手动发过 0.1.0

## 背景（为什么这么设计）

- npm Trusted Publishing **不支持新包首次发布**——包必须先存在于 registry 才能在 npm 网站绑定 Trusted Publisher。0.1.0 已于 2026-09-19 手动首发，此后所有版本走 CI
- npm 侧绑定（已配置）：npmjs.com → 包 Settings → Trusted publishing → GitHub Actions，iBobbyTS / external-subagent / `npm-publish.yml` / environment 留空
- provenance 公开包自动生成，无需 `--provenance` 标志；`publishConfig` 里**不要**加回 `provenance: true`——本地 publish 会因无 OIDC provider 直接 EUSAGE 失败
- debug 变体（`EXTERNAL_SUBAGENT_VARIANT=debug`）与发布无关：其 payload 与插件源是开发机构建产物（gitignored），且被 postpack 明确拒绝在发布包之外

## 运行时路径形态覆盖（新增子进程/探测路径时的测试义务）

`runtime_command`（agent_status.rs）按解析出的 runtime 路径**扩展名**分派：`.js/.cjs/.mjs`
走 `node <script>` 并**自动追加子命令**；无扩展名/原生二进制走直接执行。同一个 codex/zcode
安装在本机可能以三种形态被解析到——npm 布局的真实 `.js` 启动器、Homebrew 式无扩展名符号
链接、原生二进制——**分支不同，参数拼接也不同**。新增任何"启动 runtime 子进程"的代码时：

- 测试夹具必须同时覆盖 script 形态与原生形态（一个 `.mjs` 夹具 + 一个可执行脚本夹具），
  或至少断言最终 argv 不含重复子命令
- live 验证要核对**生产 daemon 实际解析到的路径**（`agent_probe` local 层的
  `runtime_path`），不要默认 scratch 环境的解析形态与生产一致——v0.6.4 的目录探测 bug
  正是只在无扩展名形态下验证、script 形态带了重复 `app-server`
