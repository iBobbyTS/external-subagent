# Windows 真机交接与分阶段演进指引 (HANDOFF)

本文档是面向 Windows 真机环境上的 AI Agent 与工程师的自包含交接指南。阶段 0（`windows-port-prep` 脚手架）已在 macOS 开发机上完成编译阻断清零、抽象解耦、fail-closed 平台骨架搭建与 CI 门禁建设。阶段 1 至阶段 4 将在 Windows 真机上递进落地。

---

## 一、读我与边界 (Read Me and Boundaries)

### 1. 目标定位与读者
- **主要读者**：Windows 真机上的执行 Agent 与系统开发工程师。
- **文档定位**：全自包含行动指引。本文件包含阶段 0 交付资产、开发机环境踩坑记录、阶段 1–4 演进清单（含真实源码入口、验收标准、批准前等价验证命令与证据要求），以及最终门控解除的决策机制。

### 2. 做什么 (Scope of Work)
- 在配备原生 MSVC C/C++ 工具链、Rust 工具链及四大 Subagent 运行时的 Windows 真机（Windows 10/11 或 Windows Server 2022+）上，承接 Windows 支持落地。
- 严格按照 [PORT-PLAN.md](./PORT-PLAN.md) 规划的阶段顺序，将阶段 0 预留的 fail-closed 骨架逐一替换为生产级 Win32 实现：
  - **阶段 1**：Named Pipe 命名管道服务端/客户端 I/O、CLI 管道通信与 MCP stdio bridge；
  - **阶段 2**：Win32 Job Object 进程生命周期全链监督（`gated_spawn` 自动注入、后代进程原子收割、基于 `GetProcessTimes` 的 `start_token` 防 PID 复用、控制台信号处理）；
  - **阶段 3**：Windows 服务控制管理器（SCM）集成（`service-windows.mjs`）、原生 win32 payload 受控测试打包与发布流水线准备；
  - **阶段 4**：四大 Subagent（Codex、ZCode、DSH、Antigravity）真机 live-verification 证据采集，并提请项目 Owner 裁决解除支持门控。

### 3. 不做什么 / 禁止事项（铁律）
- **禁止提前解除支持门控**：在阶段 4 验收全绿并经项目负责人（Project Owner）显式签字批准之前，**严禁**修改以下三处门控：
  1. `cli/main.mjs:23` 中的 `SUPPORTED_PLATFORMS = new Set(['darwin', 'linux'])`；
  2. `cli/install/layout.mjs:19` 中的 `NATIVE_PLATFORMS = Object.freeze(['darwin-arm64', 'linux-x64'])`；
  3. `package.json:23-26` 中的 `"os": ["darwin", "linux"]`。
  严禁向用户宣称 Windows 已经得到正式支持。
- **Unix 生产零回归（绝对不变量）**：任何面向 Windows 的代码变动必须置于严格的平台条件编译（`#[cfg(windows)]` / `#[cfg(not(unix))]`）或平台分支（`platform() === 'win32'`）内。macOS (Darwin) 与 Linux 生产环境行为必须保持零变化，宿主测试套件全绿是每一个交付物的硬性回归锚。
- **严格遵循 PORT-PLAN 阶段序演进**：严禁跳步。必须按照阶段 1 → 阶段 2 → 阶段 3 → 阶段 4 顺序推进。阶段 2 依赖阶段 1 的传输通信闭环，阶段 3 依赖阶段 2 的稳定进程模型，阶段 4 依赖阶段 3 的服务与打包闭环。

---

## 二、阶段 0 已完成清单 (Stage 0 Completed Manifest)

阶段 0 在分支 `feat/windows-port-prep` 上交付了 5 批核心资产，所有改动均经过独立双槽代码审查（CODE A + CODE B）并获得 CLEAN 结论合入。

### 1. 交付节与 Commit 台账

| 交付单元 | Commit Hash | 提交信息与核心产物 | 验证证据指针 (HANDOFF) | 核心审查与验证结论 |
|---|---|---|---|---|
| **S01** | `47c4e47` | `docs(windows-port): 平台差异矩阵与分阶段移植计划`<br>• `docs/windows-port/PLATFORM-MATRIX.md`<br>• `docs/windows-port/PORT-PLAN.md` | `.agent-work/handoffs/windows-port-prep/S01-HANDOFF.md` | 矩阵覆盖 10/10 平台构造族且四列齐备；演进计划分五阶段且四要素齐备；全部 40+ 源码行号与具名符号经只读命令核验证实。 |
| **S02** | `aec2069` | `feat(runtime): Windows fail-closed 平台骨架`<br>• `crates/external-runtime/src/bin/orphan-spawn-test.rs`<br>• `crates/external-runtime/src/lib.rs` | `.agent-work/handoffs/windows-port-prep/S02-HANDOFF.md` | MinGW Windows 交叉编译 check 0 error / 0 warning；宿主 40 单元测试 + 28 detached 测试全绿；`gated_spawn` 前置拒绝断言通过。 |
| **S03** | `d72f65d` | `feat(daemon,mcp): RPC 传输解耦与 Windows 交叉编译清零`<br>• `crates/external-daemon/src/rpc/transport.rs`<br>• `crates/external-daemon/src/rpc/windows.rs`<br>• `crates/external-daemon/src/rpc/unix.rs`<br>• `crates/external-daemon/src/rpc.rs`<br>• `crates/external-daemon/src/lib.rs`<br>• `crates/external-daemon/src/scheduler/diagnostics.rs`<br>• `crates/external-daemon/src/scheduler/admission.rs`<br>• `crates/external-daemon/src/main.rs`<br>• `crates/external-mcp/src/lib.rs`<br>• `crates/external-runtime/src/lib.rs` | `.agent-work/handoffs/windows-port-prep/S03-HANDOFF.md` | 全 Workspace 交叉编译清零（`cargo check --workspace --target x86_64-pc-windows-gnu` 报 0 error）；宿主 daemon 478 项测试全绿，mcp 5 项全绿；Rust `tests/` 目录零改动。 |
| **S04** | `b6a9fce` | `feat(cli): win32 路径分支与服务 fail-closed 骨架`<br>• `cli/paths.mjs`<br>• `cli/install/service-windows.mjs`<br>• `cli/install/service-macos.mjs`<br>• `cli/install/service-activation.mjs`<br>• `tests/platform/windows.test.mjs` | `.agent-work/handoffs/windows-port-prep/S04-HANDOFF.md` | JS 语法 `node --check` 0 error；平台测试 14/14 全绿（含新增测试 4 校验 12 字段与测试 5 校验 win32 服务 seam 拒绝且未调用 getuid/launchctl/systemctl）；工作树仅 5 授权文件。 |
| **S05** | `da1b4a8` | `ci(windows): 新增 windows-latest check workflow`<br>• `.github/workflows/windows-check.yml` | `.agent-work/handoffs/windows-port-prep/S05-HANDOFF.md` | YAML 解析与 schema 校验通过（Ruby（S05 HANDOFF）与 Python/PyYAML（S05 CODE A）均有解析证据）；`npm-publish.yml` 保持零改动；windows-latest runner 原生 MSVC check 门禁搭建完毕。 |

### 2. 已知环境缺口与 Stash 对照证据 (Host Pre-existing Gaps)
在宿主开发机（macOS darwin arm64）上运行完整 Node 测试套件时，可观察到 4 项由于宿主开发环境污染或未提供打包产物引起的假失败。这些失败与 Windows 移植改动完全无关：
1. `tests/cli/diagnose.test.mjs:28` 与 `tests/cli/linux-subagents.test.mjs:64`：本机已安装真实应用程序 `/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs`，导致测试中模拟缺失运行时的 fallback 机制命中真实文件并计算出 sha256，与测试用例假定未安装 ZCode 的 `undefined` 断言冲突；
2. `tests/install/service-linux.test.mjs:89`：本机 Node.js 安装于 Homebrew 目录（`/opt/homebrew/...`），`systemdServicePath()` 按产品设计拼入 Node 解释器真实路径，触发第 112 行的 `assert.doesNotMatch(unit, /homebrew/u)` 假失败（该用例假定运行在非 Homebrew 环境或标准 Linux CI 上）；
3. `tests/install/fresh-install.test.mjs`：测试临时重设的 `HOME` 隔离目录未透传 `RUSTUP_HOME`，且本地源码树未自带 CI 交叉构建生成的预编译 `linux-x64` 发布 tarball 产物。

**Stash 对照判定证据（引自 `.agent-work/FEATURE-STATE.md:54`）**：
在 S04 交付后，系统负责人（main）使用 `git stash` 针对基线状态（base `ff726bd`）与当前候选分支进行了同环境对照复跑：
- 在同一宿主环境下，base 与候选的失败用例集**逐项完全一致**；
- 结论证明 **S04 diff 零新增失败**，上述 4 项失败确认为宿主环境固有特征，代码改动未引入任何功能退化。

---

## 三、环境事实与工具链陷阱 (Environment Facts)

在 Windows 移植开发及交叉验证中，必须知悉以下环境事实与踩坑教训：

### 1. 本机交叉编译工具链陷阱
- **陷阱描述**：macOS 宿主系统 PATH 默认命中的 `cargo` / `rustc` 为 Homebrew 安装的 `1.99.0`，其 sysroot 仅包含原生 `aarch64-apple-darwin`，未包含 Windows target 的标准库（std）。而支持 Windows 目标的工具链位于 rustup `1.97.1-aarch64-apple-darwin` 下。
- **影响**：若直接运行 `cargo check --target x86_64-pc-windows-gnu`，cargo 会根据 PATH 调用 Homebrew 的 rustc，导致抛出 `can't find crate for 'std'` 错误。
- **避坑操作**：在 macOS 宿主上执行任何针对 Windows target 的交叉编译命令时，**必须显式前置 rustup 工具链 bin 目录**：
  ```bash
  env PATH="$HOME/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin:$PATH" cargo check --workspace --target x86_64-pc-windows-gnu --message-format short
  ```

### 2. MinGW-w64 / GNU 本地判错回路 vs CI MSVC 权威门
- **本地 MinGW-w64 判错回路**：宿主机安装了 Homebrew `mingw-w64` 与 Rust target `x86_64-pc-windows-gnu`。因 `cfg(windows)` 的语义在 gnu 与 msvc 目标下等价，该环境为 macOS 宿主提供了极其高效、秒级反馈的语法与类型检查回路。
- **本地 MSVC 交叉局限**：在 macOS 上因缺乏 Windows SDK 头文件与库文件，涉及 C 代码编译的组件（特别是 `rusqlite` bundled 编译的 `libsqlite3-sys`）无法在本地完成 MSVC 交叉编译。
- **CI MSVC 权威门**：真正的原生 Windows 编译门禁由 GitHub Actions `windows-latest` runner 原生环境承载（MSVC + VS Build Tools）。
- **真机开发要求**：真机 Agent 在真实 Windows 设备上开发时，应使用原生 MSVC 工具链（`cargo check --workspace --all-targets`），无需使用 MinGW 前置工具链。

### 3. cargo-clean 合规记录与 Target 目录契约
- **合规记录**：依据 PLAN 冻结决策 B4，在阶段 0 执行 S02 前执行了一次性 `cargo clean`（彻底清理约 2.1 GiB 缓存），遵循合规审计要求，确保后续构建的二进制完全来自最新源码。
- **Target 目录铁律**：此后所有构建与测试**必须统一使用仓库根目录默认的 `target/` 目录**。严禁通过环境变量 `CARGO_TARGET_DIR` 隔离编译产物，因为 Node CLI 的测试套件存在硬编码契约（如 `tests/fixtures/restart-daemon.mjs:30-36`、`tests/cli/linux-subagents.test.mjs:103-113` 固定 spawn `target/debug/external-subagentd`），隔离 target 会导致测试运行找不到二进制或运行到旧二进制假绿。

### 4. CI 触发方式与未验证状态
- **配置现状**：`.github/workflows/windows-check.yml` 配置了 `push: branches: ['**']` 与 `pull_request: {}`。
- **未验证状态（显式 Gap）**：因阶段 0 严格执行受控开发策略（禁止向远程仓库 push 分支、禁止提 PR、禁止合入 main），GitHub-hosted `windows-latest` runner **尚未真正触发运行过**。
- **后续关注重点**：真机或首发 PR 触发该 CI 时，需重点观测：在无 cache 极简配置下，原生 MSVC 冷编译（含 bundled SQLite 编译）是否能在 `timeout-minutes: 30` 阈值内顺利完成，以及 `windows-latest` 虚拟机镜像的依赖稳定性。

---

## 四、阶段 1–4 真机工作清单 (Stage 1–4 Real-Machine Work Items)

本章按 [PORT-PLAN.md](./PORT-PLAN.md) 展开后续四个真机阶段。每个阶段均严格包含：入口 file:line（现有代码带精确行号；计划新增文件不带伪行号）、验收标准 (AC)、验证命令（含批准前等价验证入口）以及遵循 `docs/compatibility` 的 live-verification 证据要求。

> **符号精度声明**：本章出现的每一个源码符号、方法、文件与参数，均已附带核验命令或源码实证引用。

---

### 阶段 1 — 真机传输层（Named Pipe IO + CLI 连接 + MCP Bridge）

- **执行环境**：Windows 真机（Windows 10/11 或 Windows Server 2022+；MSVC 原生工具链）。
- **业务目标**：在 Windows 真机上实现基于 Win32 命名管道（Named Pipe）的服务端与客户端通信协议；使 Node CLI 能够通过命名管道连接 daemon 进程；使 `external-mcp` 的 stdio bridge 对接命名管道；消除 Rust 集成测试目录 `tests/` 中的 Unix 假设并全部编译通过。

#### 1. 入口 file:line
- `crates/external-daemon/src/rpc/windows.rs`（计划新增，已在 S03 落地为骨架桩，行号待阶段 1 产物生成后核证；核验命令：`grep -n "RpcServer" crates/external-daemon/src/rpc/windows.rs`）：
  - 管道路径命名空间必须使用 `\\.\pipe\` 前缀（9 字符，双反斜杠开头，已由 `cli/paths.mjs:99` 固化为 `\\\\.\\pipe\\${PRODUCT_NAME}-daemon`；核验命令：`sed -n '97,100p' cli/paths.mjs`）；
  - 调用 `CreateNamedPipeW` 配置 `PIPE_ACCESS_DUPLEX`、`PIPE_TYPE_BYTE`、`PIPE_READMODE_BYTE`、`PIPE_WAIT`，并指定并发实例上限 `nMaxInstances`（对齐 `ServerOptions::max_connections`，核验命令：`sed -n '18,30p' crates/external-daemon/src/rpc/transport.rs`）；
  - **防劫持保护**：首实例创建管道时**必须**传入标志位 `FILE_FLAG_FIRST_PIPE_INSTANCE`，确保只有服务端能创建主监听端点，防止恶意进程抢占管道端点；
  - **安全描述符**：配置严格的 `SECURITY_ATTRIBUTES`（DACL），限制仅当前用户 SID 拥有读写访问权限；
  - **互斥与 Stale 判定**：Windows 命名管道不具备 POSIX 文件系统的 `dev`/`ino` 节点标识。使用 Win32 具名互斥体（`CreateMutexW`）或文件锁（`LockFileEx`）保证单例，或在 bind 前探测管道连接状态判定是否残留。
- `crates/external-daemon/src/rpc/transport.rs`（计划新增，已在 S03 落地为共享层，行号待阶段 1 产物生成后核证；核验命令：`grep -n "handle_connection" crates/external-daemon/src/rpc/transport.rs`）：
  - 对接 Windows Named Pipe Stream 读写抽象，复用通用的泛型连接处理函数 `handle_connection<S: Read + Write, D: Fn(&S) -> bool>`（核验命令：`sed -n '105,113p' crates/external-daemon/src/rpc/transport.rs`）。
- `crates/external-daemon/src/lib.rs:1676-1723`（Windows `Daemon` 生命周期桩替换，核验命令：`sed -n '1676,1723p' crates/external-daemon/src/lib.rs`；证据来源：S01 矩阵 `PLATFORM-MATRIX.md:64` 与 S03 `S03-HANDOFF.md:19`）：
  - 当前 `#[cfg(not(unix))]` 的 `Daemon` 桩在 `start_with_shutdown` 中无条件返回 `Unsupported("windows: daemon pending real-machine implementation")`，`crates/external-daemon/src/main.rs:123-137` 调用该方法并直接向上传播错误；若不替换该桩为真实生命周期实现，文档中的 daemon 直启验证命令无法运行；
  - **模块边界注意**：Unix 真实实现在整文件受 `#[cfg(unix)]` 门控的 `crates/external-daemon/src/daemon.rs`（核验命令：`sed -n '1,12p' crates/external-daemon/src/daemon.rs`），不能只改该文件便宣称 Windows daemon 已接通；Windows 必须在 `lib.rs:1681-1723` 处提供生产级生命周期实现（或新增 Windows 专属模块并在 `lib.rs` 按平台引入），打通启动、`claim_thread` 调度循环、RPC 监听与优雅停机（`shutdown`）。
- `crates/external-daemon/src/rpc/handlers.rs:75-77, 801-806`（RPC 随机源 Windows 适配，核验命令：`sed -n '75,77p; 801,806p' crates/external-daemon/src/rpc/handlers.rs`；证据来源：S01 矩阵 `PLATFORM-MATRIX.md:85` 与 S03 `S03-HANDOFF.md:22`）：
  - `RpcService::new` 构造服务时调用 `opaque_generation()` 生成 `service_generation`；当前实现硬读 `/dev/urandom`（`:801-806`），在缺少该设备的 Windows 上返回 `RpcServiceConfigError::GenerationUnavailable`，会直接阻断正常的 daemon 启动与 RPC 服务组合；
  - Windows 真机实现须切换为 Windows CNG 接口（`ProcessPrng` 或 `BCryptGenRandom`，S01 矩阵 :85 已记录为真机阶段项），确保在 Windows 原生环境下成功生成 16 字节随机十六进制代际字符串，消除启动阻断。
- `crates/external-mcp/src/lib.rs:72-81`（`serve_stdio` Windows 桩，核验命令：`sed -n '72,81p' crates/external-mcp/src/lib.rs`）：
  - 在 Windows 平台上使用 `tokio::net::windows::named_pipe::ClientOptions` 连接守护进程具名管道端点，替换当前的 fail-closed Unsupported 错误桩。
- `crates/external-daemon/src/daemon.rs:15, 23, 202`（`McpServer` 结构体定义、实现与 bind，核验命令：`sed -n '15p; 23p; 202p' crates/external-daemon/src/daemon.rs`）：
  - 实现 Windows 平台上 `McpServer` 的具名管道绑定与连接循环。
- `crates/external-daemon/tests/` 各集成测试文件：
  - 清理硬编码的 `.sock` 文件后缀与 POSIX 文件属性断言，适配 Windows 平台路径。

#### 2. 验收标准 (AC)
1. **Windows MSVC 原生全量编译**：`cargo check --workspace --all-targets` 报 **0 error**。
2. **守护进程传输单测通过**：`cargo test -p external-daemon -p external-mcp` 全绿。
3. **真实 IPC 通信闭环**：在 Windows 环境下，通过测试脚本直接 `import('./cli/rpc.mjs')` 调用 RPC 或通过 MCP facade 直连 `\\.\pipe\external-subagent-daemon`，成功与后台 daemon 完成 ping / rpc 握手往返（库层绕过 main.mjs 门控）。
4. **防劫持保护生效**：非首实例尝试以 `FILE_FLAG_FIRST_PIPE_INSTANCE` 绑定已存在的管道端点时，操作系统返回明确的占用拒绝错误，严禁静默退化为客户端。
5. **Windows Daemon 生命周期闭环**：`Daemon::start_with_shutdown` 在 Windows 真机上能成功启动后台调度线程与监听循环，不再返回 `Unsupported`，且在收到退出信号或调用 `shutdown()` 时能够正常终止并释放资源。
6. **RPC 随机源正常就绪**：Windows 原生环境下 `opaque_generation()` 成功调用 Win32 随机熵源（如 `ProcessPrng` / `BCryptGenRandom`）生成 16 字节十六进制服务代际字符串，`RpcService::new` 成功初始化，不再抛出 `GenerationUnavailable` 错误。

#### 3. 验证命令
```powershell
# 1. MSVC 原生全量编译与集成测试检查
cargo check --workspace --all-targets
cargo test -p external-daemon -p external-mcp

# 2. 启动 daemon 二进制（无 CLI 门控；注意必须带必填参数 --database 与 --socket）
# 核验命令来源：crates/external-daemon/src/main.rs:406-425（parse_config 强制校验）
# 注意：--database 的父目录须先存在（external-store/src/store.rs:24-25 直接 Connection::open 不建父目录）
$env:LOCALAPPDATA = "$HOME\AppData\Local"
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\external-subagent" | Out-Null
./target/debug/external-subagentd.exe --database "$env:LOCALAPPDATA\external-subagent\external-subagent.sqlite3" --socket "\\.\pipe\external-subagent-daemon"

# 3. 批准前验证入口：通过 Node 库层调用 RPC wire 方法 system_status（绕过 cli/main.mjs 门控）
# 核验方法名来源：cli/rpc.mjs:103 与 crates/external-daemon/src/rpc/types.rs:89
node -e "import('./cli/rpc.mjs').then(r => r.callDaemon('\\\\.\\pipe\\external-subagent-daemon', 'status').then(console.log))"

# 4. 运行阶段 1 传输往返专项测试脚本（计划新增（阶段 1 产物），命令待脚本创建后可用）
node --test tests/fixtures/test-named-pipe-roundtrip.mjs

# 5. 验证 RPC 处理层探针与门控既有测试（按真实具名模块过滤既有测试；Daemon 生命周期桩替换与随机源生成真实验证见第 2、3 步二进制直启与 RPC 往返）
cargo test -p external-daemon --lib rpc::handlers::agent_probe_tests
cargo test -p external-daemon --lib rpc::handlers::observe_gate_tests
```

#### 4. 证据要求
- MSVC 原生环境下 `cargo check --workspace --all-targets` 退出码 0 且 0 error 的终端日志；
- `external-daemon` 与 `external-mcp` 在 Windows 真机上的测试 pass 日志；
- 命名管道并发连接、单例防劫持（`FILE_FLAG_FIRST_PIPE_INSTANCE`）错误拦截测试日志；
- 命名管道单次与并发 RPC 往返通信的结构化 JSON 证据；
- `Daemon::start_with_shutdown` 在 Windows 上启动成功并输出启动日志的终端记录；
- `opaque_generation()` 在 Windows 环境下成功产出 16 字节随机代际字符串且 `RpcService::new` 成功初始化的测试断言证据；
- Daemon 响应优雅退出（`shutdown()`）且无线程泄漏的退出日志。

---

### 阶段 2 — 真机进程监督（Job Object 全链：gated_spawn / observe / 收割 / start_token）

- **执行环境**：Windows 真机。
- **业务目标**：在 Windows 真机上实现基于 Win32 Job Object 的完整进程生命周期管理；`gated_spawn` 自动将进程注入 Job Object；实现真实的进程与进程组观察（`platform_observe_process` 与 `platform_observe_process_group`）、进程组原子销毁（`TerminateJobObject`）、基于 `GetProcessTimes` 的持久化身份凭据 `start_token` 防止 PID 复用；实现有界/异步的子进程响应写通道。

#### 1. 入口 file:line
- `crates/external-runtime/src/lib.rs:36-45`（`gated_spawn` 咽喉点，核验命令：`sed -n '36,45p' crates/external-runtime/src/lib.rs`）：
  - 替换当前的 `#[cfg(windows)]` fail-closed 桩；
  - 创建 Win32 Job Object 句柄，配置 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`（保证母任务异常退出时子进程树随句柄关闭被系统级联收割）；
  - 调用 `CreateProcessW` 传入 `CREATE_SUSPENDED` 挂起启动子进程，通过 `AssignProcessToJobObject` 将其安全加入 Job Object，随后调用 `ResumeThread` 恢复运行，杜绝进程在绑定前产生后代逃逸。
- `crates/external-runtime/src/lib.rs:1104, 1374, 1522`（`observe_process` 与 `platform_observe_process`，核验命令：`sed -n '1104p; 1374p; 1522p' crates/external-runtime/src/lib.rs`）：
  - 通过 `OpenProcess` 与 `GetProcessTimes` 提取进程创建精确时间戳 `CreationTime`；
  - 构造 `ProcessIdentity`（核验命令：`sed -n '142,147p' crates/external-runtime/src/lib.rs`），生成 `windows:creation_time` 格式的持久化身份凭据 `start_token`。
- `crates/external-runtime/src/lib.rs:1114, 1405, 1530`（`observe_process_group` 与 `platform_observe_process_group`，核验命令：`sed -n '1114p; 1405p; 1530p' crates/external-runtime/src/lib.rs`）：
  - 通过 `QueryInformationJobObject` 查询 `JobObjectBasicProcessIdList` 获取 Job Object 内全部活动 PID 列表并采样。
- `crates/external-runtime/src/lib.rs:1253-1303`（`kill_group` 与 `kill_process` 监督助手，核验命令：`sed -n '1253,1303p' crates/external-runtime/src/lib.rs`）：
  - Windows 实现分别映射至 `TerminateJobObject(job_handle, 1)` 与 `TerminateProcess(process_handle, 1)`，严格容忍进程已退出的系统错误（类似 Unix `ESRCH` 容忍语义）。
- `crates/external-runtime/src/lib.rs:587-630`（`respond_before` 与 `write_frame_before`，核验命令：`sed -n '587,630p' crates/external-runtime/src/lib.rs`）：
  - 替换当前的拒绝式回退臂，实现 Windows 命名管道的异步重叠 I/O（Overlapped I/O）或基于独立线程的有界写入。
- `crates/external-daemon/src/agent_status.rs:1362`（`cleanup_catalog_process`，核验命令：`sed -n '1362p' crates/external-daemon/src/agent_status.rs`）：
  - 在 Windows 上关闭 Job Object 句柄以执行确定性资源回收。
- `crates/external-daemon/src/main.rs:55-63`（控制台退出处理，核验命令：`sed -n '55,63p' crates/external-daemon/src/main.rs`）：
  - 使用 `SetConsoleCtrlHandler` 注册控制台事件处理器，响应 `CTRL_C_EVENT` 与 `CTRL_CLOSE_EVENT`，触发全局优雅停机流程。

#### 2. 验收标准 (AC)
1. **运行时单测与进程监督测试通过**：`cargo test -p external-runtime`（含 `--test detached` 进程监督集成测试）在 Windows 真机上全绿。
2. **进程树级联终止保证**：由子进程衍生出的深层后代进程（例如 node.exe 派生 cmd.exe 再派生 powershell.exe），在母任务被 cancel 或崩溃退出时，随 Job Object 关闭被操作系统原子收割，无孤儿进程残留。
3. **PID 复用防御生效**：杀死某进程后启动具有相同 PID 的新进程，`start_token` 校验能准确识别出进程重启（基于 `GetProcessTimes` 精确时间戳），拒绝越权或误伤操作。
4. **控制台事件响应**：向 daemon 发送 Ctrl+C 能触发控制台信号处理器，完成服务优雅下线与资源释放。

#### 3. 验证命令
```powershell
# 1. 运行 runtime 单元测试、进程监督集成测试与调度器测试（按名过滤既有测试）
cargo test -p external-runtime
cargo test -p external-runtime --test detached
cargo test -p external-daemon scheduler

# 2. 批准前验证入口：通过 Node 库层调用 RPC wire 方法 submit_general 创建长耗时子进程
# 核验方法名来源：cli/rpc.mjs:111 与 crates/external-daemon/src/rpc/types.rs:96
# 【必需前置配置与条件链说明（能真实到达 gated_spawn / Job Object）】：
# 新机默认缺少 config.json 时，rpc/config.rs:45-103 中全部 provider 均 enabled: false 且 default_subagent 为 None；
# 此时直接调用 create 会在 rpc/agents.rs:473-484 触发 AgentRequired 拒绝，或在 :493-511 因 disabled / unsupported 被拒。
# 本示例能够成功创建、调度工作负载并真实到达 gated_spawn / Job Object，必须完整满足以下三项前置条件链：
#   (i) Agent 选择（rpc/agents.rs:473-484）：callDaemon 的 input 显式指定 subagent（真实 agent 名：zcode/dsh/codex/agy 四选一，按 rpc/agents.rs admission 接受集，如示例中的 subagent: 'codex'），或在隔离配置文件中显式配置 default_subagent；
#   (ii) Provider 启用与 Spawn 能力门（rpc/agents.rs:485-511）：该 provider 在隔离配置中必须同时满足 enabled: true 且 spawn_supported: true（默认 false），并满足对应 provider 的 spawn 门条件（如 codex 需配置存在且绝对路径的可执行文件 runtime_path；admission 的 spawn 能力门（enabled && spawn_supported && runtime_path，rpc/agents.rs:125-192 effective_spawn_supported）对 codex/agy/dsh 三者都要求隔离配置里的 runtime_path 字段指向绝对可执行文件，环境变量 AGY_RUNTIME_PATH/DSH_RUNTIME_PATH 只参与启动路径解析，不能替代配置项通过 spawn 门；dsh 另需 profile=="acp" 且 version==PINNED_DSH_VERSION；zcode 需满足其能力门）；
#   (iii) Daemon 隔离配置指向（crates/external-daemon/src/main.rs:397, 418-419）：启动 daemon 二进制时，必须通过命令行参数 --agent-config 显式指向包含上述满足条件的 provider 隔离配置（如 --agent-config "$env:LOCALAPPDATA\external-subagent\isolated-config.json"）。
# 【成功判定与说明】：
# - 成功条件为：观察到实际 RUNNING 任务与真实操作系统子进程（如通过 status 接口返回任务处于运行态，且在 Job Object 内采样到活动 PID）；
# - 特别注意：prompt（如下文中的 'cmd.exe /c timeout /t 60'）仅作为封装进任务清单的任务文本（cli/rpc.mjs:68-95 manifest），而非由 daemon 直接经 shell 执行的命令行参数；
# - 确定性深层进程树级联收割的有效性证据，仍由下方已标注「计划新增」的专用测试脚本（test-process-tree-reap.mjs）通过受控进程树生成，不可仅凭 submit_general 返回成功推断。
node -e "import('./cli/rpc.mjs').then(r => r.callDaemon('\\\\.\\pipe\\external-subagent-daemon', 'create', { repository: process.cwd(), prompt: 'cmd.exe /c timeout /t 60', subagent: 'codex' }).then(console.log))"

# 3. 运行深层进程树级联收割专项测试脚本（计划新增（阶段 2 产物），命令待脚本创建后可用）
node --test tests/platform/test-process-tree-reap.mjs
```

#### 4. 证据要求
- `external-runtime` 库及测试在 Windows 真机上全绿的日志；
- 进程泄漏防御测试报告：深层衍生子进程在母进程退出后被完全收割的 Process Explorer / 任务管理器截图或 PowerShell 脚本日志（由已标注「计划新增」的 `tests/platform/test-process-tree-reap.mjs` 或等价 fixture 确定性生成，不可仅凭 submit_general 成功推断）；
- PID 复用冲突防御对比日志。

---

### 阶段 3 — 服务注册（SCM）+ Win32 Payload + 发布矩阵

- **执行环境**：Windows 真机 + GitHub Actions Windows Runner。
- **业务目标**：在 Windows 平台上实现后台服务化常驻管理；Node CLI 的 `service-windows.mjs` 接入 Windows 服务控制管理器（SCM，Service Control Manager）；构建并打包 `win32-x64` / `win32-arm64` 原生二进制 payload 测试产物；准备受控测试发布矩阵与 npm 打包流水线。
- **门控不变量声明**：阶段 3 仅准备受控测试产物与发布矩阵（不激活），三项支持门控（`SUPPORTED_PLATFORMS`、`NATIVE_PLATFORMS`、`package.json:os`）保持原样封闭，留待阶段 4 在 OWNER_DECISION 批准后统一步骤解除。

#### 1. 入口 file:line
- `cli/install/service-windows.mjs`（Windows 服务骨架符号：`windowsServiceRegistrationStatus` / `bootstrapServiceWindows` / `bootoutServiceWindows` / `installServiceDefinitionWindows` / `activateServiceWindows`，核验命令：`sed -n '10,32p' cli/install/service-windows.mjs`）：
  - 基于 `sc.exe` 命令行工具或 Win32 Service API 实现 `bootstrapServiceWindows`、`bootoutServiceWindows`、`installServiceDefinitionWindows`、`windowsServiceRegistrationStatus`；
  - 将 daemon 包装为 Windows SCM 服务或系统级自启动计划任务运行。
- `cli/paths.mjs:137-156`（`rustPathParent` 路径解析函数，核验命令：`sed -n '137,156p' cli/paths.mjs`）：
  - 升级 `rustPathParent` 支持 Windows 反斜杠 `\`、盘符根路径（如 `C:\`）以及 UNC 路径（如 `\\server\share\`），与 Rust `Path::parent()` 在 Windows 下的行为严格对齐。
- `cli/install/payload.mjs`（核验命令：`grep -n "payload" cli/install/payload.mjs`）：
  - 准备 Windows 原生二进制 payload 的解压、SHA-256 哈希校验及受控测试产物校验链。
- `.github/workflows/`：
  - 在 CI/CD 流水线中补充 Windows 原生二进制编译（`win32-x64`、`win32-arm64`）与 npm 发布打包矩阵配置。

#### 2. 验收标准 (AC)
1. **服务生命周期打通**：在 Windows 上通过服务管理接口（如 `sc.exe` 或直接导入 `service-windows.mjs` 库层）能成功创建并启动 SCM 服务；状态报告运行中；能正常停止并注销服务（公共 CLI `external-subagent start/status/stop` 在门控保持期间预期抛出 `UNSUPPORTED_PLATFORM` 平台拒绝）。
2. **路径规范解析无误**：`rustPathParent` 对形如 `C:\Users\admin\AppData\Local\config.json` 的路径正确解析父目录为 `C:\Users\admin\AppData\Local`，对根路径 `C:\` 返回 `null`。
3. **受控安装与打包闭环**：通过本地或 CI 受控生成的 tarball/payload 测试产物，能在 Windows 机器上正确解压出 `external-subagentd.exe` 与 `external-subagent-mcp.exe`，且直接运行二进制 `--version` 正常。

#### 3. 验证命令
```powershell
# 1. 批准前验证入口：通过 Node 库层验证 SCM 服务生命周期（绕过 cli/main.mjs 门控）
# 核验模块来源：cli/install/service-windows.mjs:18-24
node -e "import('./cli/install/service-windows.mjs').then(m => m.bootstrapServiceWindows())"
sc.exe query external-subagentd
node -e "import('./cli/install/service-windows.mjs').then(m => m.bootoutServiceWindows())"

# 2. 验证 daemon 二进制独立启动（必须带必填参数 --database 与 --socket）
# 注意：--database 的父目录须先存在（external-store/src/store.rs:24-25 直接 Connection::open 不建父目录）
$env:LOCALAPPDATA = "$HOME\AppData\Local"
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\external-subagent" | Out-Null
./target/debug/external-subagentd.exe --database "$env:LOCALAPPDATA\external-subagent\external-subagent.sqlite3" --socket "\\.\pipe\external-subagent-daemon"

# 3. 公共 CLI 业务命令预期结构化拒绝（回归锚，tests/platform/windows.test.mjs:40-47）
# 核验命令来源：cli/main.mjs:347
node bin/external-subagent.mjs start  # 预期返回 exit 1 与 UNSUPPORTED_PLATFORM 错误

# 4. 受控测试打包产物验证
npm pack --dry-run
```

#### 4. 证据要求
- Windows 事件查看器（Event Viewer）中由 SCM 记录的服务注册、启动、运行与正常停止的系统事件日志；
- `npm pack --dry-run` 打包产物目录结构及 `external-subagentd.exe` 的哈希记录。

---

### 阶段 4 — Subagent 真机兼容证据与门控解除决策 (OWNER_DECISION)

- **执行环境**：Windows 真机；配置真实可用的 Subagent 运行环境（Codex CLI、ZCode 环境、DSH CLI、Antigravity CLI）。
- **业务目标**：遵循 `docs/compatibility/*.md` 的证据式文化（Live Verification），在 Windows 真机上逐一验证四大 Subagent 的发现、版本提取、auth 鉴权与 hi 探针交互；产出 Windows 平台专属兼容性报告；提交支持承诺解除决议（OWNER_DECISION）。

#### 1. 入口 file:line
- `crates/external-daemon/src/agent_status.rs:340-365`（四大 Subagent 探针执行逻辑与调度，核验命令：`sed -n '340,365p' crates/external-daemon/src/agent_status.rs`）：
  - 接入 Windows 平台上的运行时发现与探针派发。
- `crates/external-daemon/src/rpc/agents.rs:139-148, 166-175, 193-202`（subagent 运行路径的可执行检查，核验命令：`sed -n '139,148p; 166,175p; 193,202p' crates/external-daemon/src/rpc/agents.rs`）：
  - 实现基于 Windows `PATHEXT`（如 `.exe;.cmd;.bat`）环境变量的可执行文件解析与可执行权限匹配（当前非 unix 臂仅返回 `true`，真机阶段需落实真实校验）。
- `docs/compatibility/windows.md`（计划新增）：
  - 新增 Windows 平台真机兼容性证据报告，遵循 `docs/compatibility/` 体例。
- `cli/main.mjs:23`（`SUPPORTED_PLATFORMS` 门控解除点，核验命令：`sed -n '20,25p' cli/main.mjs`）。
- `cli/install/layout.mjs:19`（`NATIVE_PLATFORMS` 增补 win32 决策点，核验命令：`sed -n '18,22p' cli/install/layout.mjs`）。
- `package.json:23-26`（`os` 数组增补 win32 决策点，核验命令：`sed -n '23,26p' package.json`）。
- `tests/platform/windows.test.mjs:40-48`（核验命令：`sed -n '40,48p' tests/platform/windows.test.mjs`）：
  - 将既有 17 命令拒绝测试调整为正常功能支持断言。

#### 2. 验收标准 (AC)
1. **真实探针证据闭环**：在 Windows 真机上运行四大 agent 探针（通过实际 CLI 参数 `--hi` 或明确受限模式），全部产生有效 JSON 输出，无崩溃或挂起：
   - **Codex**：正常发现 `codex.exe` / `codex.cmd`，完成协议握手；
   - **ZCode**：正常连接 app-server 并取得模型列表；
   - **DSH**：正常运行 ACP 协议完成 session 初始化；
   - **Antigravity**：正常拉起 `agy.cmd` / `agy.exe` 流式交互。
2. **兼容性文档齐备**：在 `docs/compatibility/windows.md` 下完整归档测试机硬件规格、OS 版本、Node 版本、协议事件序列及探针原生 JSON 证据。
3. **Owner 批准解除门控（OWNER_DECISION）**：项目负责人审查全部真机证据后显式批准，正式解除三项支持门控。

#### 3. 验证命令
```powershell
# -------------------------------------------------------------
# A. 门控解除前（批准前验证入口）：
# 通过 node -e 直接调用 rpc.mjs 绕过 CLI 门控，使用真实 wire 方法 agent_probe，
# 参数使用真实层级 through: 'hi'（核验命令：sed -n '20,44p' cli/commands/agents.mjs）
# -------------------------------------------------------------
node -e "import('./cli/paths.mjs').then(p => import('./cli/rpc.mjs').then(r => r.callDaemon(p.productPaths().socket, 'agent-probe', { subagent: 'codex', through: 'hi' }).then(console.log)))"
node -e "import('./cli/paths.mjs').then(p => import('./cli/rpc.mjs').then(r => r.callDaemon(p.productPaths().socket, 'agent-probe', { subagent: 'zcode', through: 'hi' }).then(console.log)))"
node -e "import('./cli/paths.mjs').then(p => import('./cli/rpc.mjs').then(r => r.callDaemon(p.productPaths().socket, 'agent-probe', { subagent: 'dsh', through: 'hi' }).then(console.log)))"
node -e "import('./cli/paths.mjs').then(p => import('./cli/rpc.mjs').then(r => r.callDaemon(p.productPaths().socket, 'agent-probe', { subagent: 'agy', through: 'hi' }).then(console.log)))"

# 门控解除前公共 CLI 业务命令仍预期结构化拒绝（回归锚，tests/platform/windows.test.mjs:40-47）
node bin/external-subagent.mjs subagents probe codex --hi  # 预期返回 exit 1 与 UNSUPPORTED_PLATFORM 错误

# -------------------------------------------------------------
# B. 门控解除后（Owner 批准后执行，公共 CLI 业务命令正式激活，使用真实参数 --hi）：
# -------------------------------------------------------------
external-subagent subagents probe codex --hi
external-subagent subagents probe zcode --hi
external-subagent subagents probe dsh --hi
external-subagent subagents probe agy --hi

# 运行全套跨平台测试
node --test tests/platform/*.test.mjs tests/cli/*.test.mjs tests/install/*.test.mjs
```

#### 4. 证据要求
- 4 份在 Windows 真机上运行四大 agent 探针生成的原生 JSON 数据包；
- 遵循 `docs/compatibility/` 文化撰写的 `docs/compatibility/windows.md`，包含事件流采样、权限拦截矩阵、会话重连及终止信号实测数据；
- 项目负责人（Owner）显式签字批准解除门控的审查记录。

---

## 五、验收与门控解除决策点 (OWNER_DECISION)

### 1. 决策机制与权威主体
- **决策主体**：必须由**项目负责人（Project Owner）**单方面行使此决策权。
- **权限边界**：实施者 Agent、外部协作 Subagent、日常 PR 均**无权**擅自解除支持门控。未获 Owner 显式签署批准前，任何试图解除门控的提交将被代码审查自动标记为材料级阻断（MATERIAL_CANDIDATE）。

### 2. 解除门控的先决条件 (Prerequisites)
Owner 签署解除批准前，必须确认以下验收证据全部就绪：
1. **MSVC 编译**：Windows 真机及 CI `windows-latest` runner 上原生 MSVC `cargo check --workspace --all-targets` 报 0 error；
2. **传输层**：Named Pipe I/O 通信、并发处理与防劫持测试全部通过；
3. **监督层**：Win32 Job Object 级联收割机制经深层衍生进程实测证实无孤儿进程泄漏，`start_token` 防 PID 复用有效；
4. **服务与安装**：Windows SCM 服务注册与卸载生命周期打通，受控 native payload 打包产物经解压校验无损；
5. **Subagent 证据**：Codex、ZCode、DSH、Antigravity 四大 Subagent 在 Windows 上的真机 `--hi` 探针全部返回有效数据，兼容性报告 `docs/compatibility/windows.md` 完成归档；
6. **Unix 零回归**：macOS (Darwin) 与 Linux 宿主既有测试套件除已记录宿主环境固有失败外全部通过（以干净 CI/Linux 环境或同环境基线对照为准）。

### 3. 解除门控的四个具体变更动作
经 Owner 显式批准后，一次性按以下顺序执行变更（引用 PORT-PLAN.md 阶段 4）：

| 序号 | 变更代码路径 | 当前状态（门控中） | 解除后状态（已激活） |
|---|---|---|---|
| **1** | `cli/main.mjs:23`<br>（核验：`sed -n '20,25p' cli/main.mjs`） | `const SUPPORTED_PLATFORMS = new Set(['darwin', 'linux']);` | 扩充包含 `'win32'`：<br>`const SUPPORTED_PLATFORMS = new Set(['darwin', 'linux', 'win32']);` |
| **2** | `cli/install/layout.mjs:19`<br>（核验：`sed -n '18,22p' cli/install/layout.mjs`） | `export const NATIVE_PLATFORMS = Object.freeze(['darwin-arm64', 'linux-x64']);` | 增补 `'win32-x64'`（及适用时的 `'win32-arm64'`）：<br>`export const NATIVE_PLATFORMS = Object.freeze(['darwin-arm64', 'linux-x64', 'win32-x64']);` |
| **3** | `package.json:23-26`<br>（核验：`sed -n '23,26p' package.json`） | `"os": [<br>  "darwin",<br>  "linux"<br>]` | 增补 `"win32"`：<br>`"os": [<br>  "darwin",<br>  "linux",<br>  "win32"<br>]` |
| **4** | `tests/platform/windows.test.mjs:40-48`<br>（核验：`sed -n '40,48p' tests/platform/windows.test.mjs`） | 17 个公共业务命令预期返回 `UNSUPPORTED_PLATFORM` 错误断言 | 调整为公共 CLI 业务命令在 win32 上的正常支持与执行断言 |

至此，Windows 正式成为受支持的官方一级平台。
