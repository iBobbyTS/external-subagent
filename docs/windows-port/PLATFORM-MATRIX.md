# 平台差异矩阵 — Windows 移植 (PLATFORM-MATRIX)

## 背景与权威

本矩阵根据系统决策清单 [DECISIONS.md](../../docs/initial-research/DECISIONS.md) 的 D02 项要求制定（「要同时支持 Linux、Intel Mac 或 Windows，会新增进程回收、服务注册、权限执行与原生包验收，须扩展计划后再开工」）。作为 Windows 移植工作的开工前置契约，本文档梳理全仓对 Unix（macOS / Linux）的底层依赖，明确每一处构造的 Windows 真机对应方案、阶段 0 处置策略与验收标准。

### 阶段 0 处置分类定义（全计划冻结）

全仓 Unix 依赖在阶段 0（准备脚手架）统一归为以下五种处置方式之一，后续各阶段的实现严格遵循此分类：
1. **前置拒绝骨架**：编译通过；进入需真实实现的路径时返回结构化 fail-closed 错误（`io::ErrorKind::Unsupported` / 既有错误通道，消息统一前缀 `windows:` 并含 `pending real-machine implementation`），源码标注 `TODO(windows-real-machine)`。
2. **已有回退保持**：源码中已存在非 Unix / 跨平台回退分支，语义完备且编译通过，原样保持。
3. **已有 cfg 门控零改动**：源码中该构造已置于 `#[cfg(unix)]`、`#[cfg(target_os = "macos")]` 等平台条件编译块内，在 Windows target 下不参与编译，零改动。注：2026-10-14 源码复核已证伪原先勘查（EXPL）中认为 agent_status / provider / cleanup_catalog「需迁移」的假设，核证其均已有精确门控。
4. **运行时容忍回退**：构造在 Windows target 下能够编译，运行时若资源不存在则安全退化或容忍，不影响骨架行为。
5. **无需处理**：属于 Unix 专属模块（整文件已 cfg 门控）或受顶层平台门控阻断不可达的路径，阶段 0 无需修改。

> [!NOTE]
> Windows 对应方案属于未来真机阶段的技术实现，本文档均标注「真机阶段查证」，不得视为当前已验证实现。

---

## 逐构造映射矩阵（10/10 构造族覆盖）

### 1. 进程组隔离（setpgid / process_group → Job Object）

| Unix 构造（file:line，以 2026-10-14 源码复核为准） | Windows 对应方案（真机阶段技术，标注真机阶段查证） | 本仓库阶段 0 处置 | 验收标准 |
|---|---|---|---|
| `crates/external-runtime/src/lib.rs:34-37`<br>`pub fn gated_spawn(command: &mut Command) -> io::Result<Child>`<br>全仓进程创建法定咽喉点（probes / Driver / dsh 均经此） | `CreateJobObjectW` 创建具名/匿名 Job Object，配置 `JobObjectExtendedLimitInformation`（含 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`），通过 `AssignProcessToJobObject` 将子进程加入 Job Object（真机阶段查证） | **前置拒绝骨架**（fail-closed 骨架阶段）：函数首部增加 `#[cfg(windows)]` 早退 `Err(io::ErrorKind::Unsupported, "windows: runtime spawn pending real-machine implementation (Job Object)")`，早退严格位于 `command.spawn()` 之前 | Windows 上任何子进程创建在 spawn 前被安全拒绝；源断言确认早退位于 spawn 之前；交叉编译 `cargo check --target x86_64-pc-windows-gnu` 0 error；Unix 行为零变化 |
| `crates/external-runtime/src/lib.rs:356-368`<br>`start_attached` 中的 `command.pre_exec` 调用 `libc::setpgid(0, 0)` | Job Object 进程组绑定（真机阶段查证） | **已有 cfg 门控零改动**：整段调用已位于 `#[cfg(unix)]` 块内 | 交叉 check 0 error；host macOS/Linux 原样保留 |
| `crates/external-daemon/src/agent_status.rs:588-596`<br>`probe_agy_hi`（:554 起，agy hi 探针）启动前 `command.pre_exec(|| libc::setpgid(0, 0))` | Job Object 进程隔离（真机阶段查证） | **已有 cfg 门控零改动**：源码复核核证已位于 `#[cfg(unix)]` 内；且后续调用 `gated_spawn`（:597）在 Windows 上自动前置拒绝 | 交叉 check 0 error；host agy probe 测试全绿 |
| `crates/external-daemon/src/agent_status.rs:1120-1128`<br>`run_agy_catalog`（:1101 起，agy catalog 目录读取）启动前 `command.pre_exec(|| libc::setpgid(0, 0))` | Job Object 进程隔离（真机阶段查证） | **已有 cfg 门控零改动**：源码复核核证已位于 `#[cfg(unix)]` 内；后续调用 `gated_spawn`（:1129）在 Windows 上自动前置拒绝 | 交叉 check 0 error；host agy catalog 测试全绿 |
| `crates/external-daemon/src/agent_status.rs:1267-1275`<br>dsh 探针启动前 `command.pre_exec(|| libc::setpgid(0, 0))` | Job Object 进程隔离（真机阶段查证） | **已有 cfg 门控零改动**：源码复核核证已位于 `#[cfg(unix)]` 内；后续调用 `gated_spawn`（:1276）在 Windows 上自动前置拒绝 | 交叉 check 0 error；host dsh probe 测试全绿 |
| `crates/external-daemon/src/agent_status.rs:1607-1615`<br>通用 `executable_version`（:1603 起，通用可执行文件版本探测）启动前 `command.pre_exec(|| libc::setpgid(0, 0))` | Job Object 进程隔离（真机阶段查证） | **已有 cfg 门控零改动**：源码复核核证已位于 `#[cfg(unix)]` 内；后续调用 `gated_spawn`（:1616）在 Windows 上自动前置拒绝 | 交叉 check 0 error；host 版本探测测试全绿 |
| `crates/external-daemon/src/agent_status.rs:1954-1962`<br>zcode 验证脚本启动前 `command.pre_exec(|| libc::setpgid(0, 0))` | Job Object 进程隔离（真机阶段查证） | **已有 cfg 门控零改动**：源码复核核证已位于 `#[cfg(unix)]` 内；后续调用 `gated_spawn`（:1963）在 Windows 上自动前置拒绝 | 交叉 check 0 error；host zcode probe 测试全绿 |
| `crates/subagents/dsh/src/profile.rs:231-240`<br>`bounded_output` 中 `command.process_group(0)` 与 `gated_spawn(command)` | Job Object 进程隔离（真机阶段查证） | **已有回退保持**：源码第 225 行已对函数加 `#[cfg(unix)]`，第 245–248 行已存在 `#[cfg(not(unix))]` 实现并返回 `Err("DSH preflight requires supported process-group isolation".into())` | 交叉 check 0 error；host unix 测试全绿 |

### 2. 组收割（kill -pgid TERM → KILL → Job close / TerminateJobObject）

| Unix 构造（file:line，以 2026-10-14 源码复核为准） | Windows 对应方案（真机阶段技术，标注真机阶段查证） | 本仓库阶段 0 处置 | 验收标准 |
|---|---|---|---|
| `crates/external-runtime/src/lib.rs:1191-1213`<br>`signal_group(pgid, signal)` 调用 `libc::kill(target, signal)` 向进程组发信号 | 调用 `TerminateJobObject` 或释放/关闭 Job Object 句柄触发级联销毁（真机阶段查证） | **已有回退保持**：源码第 1205–1212 行已存在 `#[cfg(not(unix))]` 分支，返回 `Err(Unsupported "process-group signalling is unsupported on this platform")` | 交叉 check 0 error；host unix 信号测试全绿 |
| `crates/external-runtime/src/lib.rs:1056-1064`<br>`TERM_SIGNAL` 与 `KILL_SIGNAL` 信号常量定义（libc::SIGTERM / libc::SIGKILL） | Windows 无 POSIX 信号，对应为 Job Object 销毁或 `TerminateProcess` 退出码（真机阶段查证） | **已有回退保持**：源码第 1061–1064 行已有 `#[cfg(not(unix))]` 对应常量（15 与 9） | 交叉 check 0 error |
| `crates/external-runtime/src/lib.rs`（既有 platform 区，signal_group 旁，新增）<br>`pub fn kill_group(pgid: i32) -> io::Result<()>`<br>`pub fn kill_process(pid: u32) -> io::Result<()>` | `TerminateJobObject` / `TerminateProcess`（真机阶段查证） | **前置拒绝骨架**（fail-closed 骨架阶段）：收敛监督助手面；unix 臂调 `libc::kill(-pgid, SIGKILL)` / `libc::kill(pid, SIGKILL)`（ESRCH 容差与 `crates/subagents/dsh/src/profile.rs:351-355` 一致）；windows 臂返回 `Err(Unsupported "windows: process-group termination pending real-machine implementation (Job Object)")` | 供 daemon admission 消费；交叉 check 0 error；host unix 行为等价 |
| `crates/external-daemon/src/scheduler/admission.rs:184-196`<br>取消任务时直接调用 `unsafe { libc::kill(-pgid, libc::SIGKILL) }` 与 `libc::kill(pid as i32, libc::SIGKILL)`（全仓唯一未门控 kill 站点） | 调用公共进程监督助手（真机阶段查证） | **前置拒绝骨架**（RPC 解耦阶段）：改调共享的 `kill_group`/`kill_process` 助手（`let _ = kill_group(pgid);` 与 `let _ = kill_process(pid);`），保留 best-effort 忽略错误语义 | 消除直接 libc 调用编译阻断；交叉 check 0 error；host 既有任务取消测试全绿 |
| `crates/external-daemon/src/agent_status.rs:1362-1382`<br>`cleanup_catalog_process` 中两处 `libc::kill(-process_group, SIGTERM)` 与 `libc::kill(-process_group, SIGKILL)` | Job Object 关闭或终止（真机阶段查证） | **已有 cfg 门控零改动**：源码复核核证第 1363–1366 与 1377–1380 行均已由 `#[cfg(unix)] unsafe` 包裹 | 交叉 check 0 error；host catalog 进程回收测试全绿 |
| `crates/subagents/dsh/src/profile.rs:351-360`<br>`collect_probe` 中探测进程组收割 `libc::kill(-pgid, libc::SIGKILL)` | Job Object 终止（真机阶段查证） | **已有 cfg 门控零改动**：整段位于 `#[cfg(unix)] fn collect_probe` 内，非 unix 无法编译进二进制 | 交叉 check 0 error；host unix 测试全绿 |
| `crates/external-runtime/src/lib.rs:561-604`<br>`respond_before` 与 `write_frame_before`（非阻塞/超时向 runtime 输入管道写入响应） | 命名管道或重叠 I/O（Overlapped I/O / IOCP / 有界异步写入）（真机阶段查证） | **前置拒绝骨架**（fail-closed 骨架阶段）：加 `#[cfg(not(unix))]` 臂；帧序列化保留，写通道不执行，直接经既有 `RequestError` 通道返回错误，消息前缀 `windows:` 并含 `pending real-machine implementation`；不引入阻塞写回退 | 交叉 check 0 error；host macOS/Linux 原样保留；Windows 失败走既有 RequestError 变体 |

### 3. 进程身份（proc_pidinfo / /proc start_token → ProcessCreationTime + Toolhelp32）

| Unix 构造（file:line，以 2026-10-14 源码复核为准） | Windows 对应方案（真机阶段技术，标注真机阶段查证） | 本仓库阶段 0 处置 | 验收标准 |
|---|---|---|---|
| `crates/external-runtime/src/lib.rs:1283-1312`<br>`platform_observe_process` 基于 Darwin `libc::proc_pidinfo` 提取 `pbi_start_tvsec`/`tvusec` 生成 `darwin:sec:usec` start_token | `OpenProcess` 获取句柄后调用 `GetProcessTimes` 提取 `lpCreationTime`（FILETIME 64位无符号整型），格式化为 `windows:creation_time` start_token（真机阶段查证） | **已有 cfg 门控零改动**：整段函数已声明 `#[cfg(target_os = "macos")]` | 交叉 check 0 error；macOS host 身份观察测试全绿 |
| `crates/external-runtime/src/lib.rs:1380-1406`<br>`platform_observe_process` 基于 Linux `/proc/{pid}/stat` 解析 starttime 字段生成 `linux:starttime` start_token | 调用 `GetProcessTimes` 提取进程创建时间（真机阶段查证） | **已有 cfg 门控零改动**：整段函数已声明 `#[cfg(target_os = "linux")]` | 交叉 check 0 error；Linux 行为零改动 |
| `crates/external-runtime/src/lib.rs:1314-1378 & 1408-1429`<br>Darwin `libc::proc_listpgrppids` 与 Linux `/proc` 目录枚举观察进程组成员 | `QueryInformationJobObject`（传入 `JobObjectBasicProcessIdList`）获取 Job 内 PID 列表，或通过 `CreateToolhelp32Snapshot` 快照遍历（真机阶段查证） | **已有 cfg 门控零改动**：分别位于 `target_os = "macos"` 与 `target_os = "linux"` 条件编译块内 | 交叉 check 0 error |
| `crates/external-runtime/src/lib.rs:1431-1445`<br>非 macOS/Linux 平台的 `platform_observe_process` 与 `platform_observe_process_group` 桩函数 | Windows 对应真机实现（真机阶段查证） | **已有回退保持**：源码第 1431–1445 行已存在 `#[cfg(not(any(target_os = "macos", target_os = "linux")))]` 桩，返回 `Err(io::ErrorKind::Unsupported, "native process identity observation is unsupported on this platform")` | 交叉 check 0 error；Windows 上身份观察前置拒绝 |
| `crates/external-runtime/src/detached.rs`<br>Detached 守护进程隔离模式（依赖 Darwin kqueue / proc_pidinfo 等） | Windows 独立服务模式或隐藏控制台子进程（真机阶段查证） | **已有 cfg 门控零改动**：整文件已声明 `#[cfg(target_os = "macos")]`；且 `lib.rs:319-325` 在非 macOS 下明确返回 `Err(io::ErrorKind::Unsupported, "detached requires macOS")` | 交叉 check 0 error；macOS detached 模式不变 |

### 4. UDS（bind / stale / flock 单例 → named pipe + LockFileEx）

| Unix 构造（file:line，以 2026-10-14 源码复核为准） | Windows 对应方案（真机阶段技术，标注真机阶段查证） | 本仓库阶段 0 处置 | 验收标准 |
|---|---|---|---|
| `crates/external-daemon/src/rpc.rs:18-24`<br>RPC 传输导出面（现仅 `#[cfg(unix)] mod unix;` 及 `pub use unix::{...}`） | Windows 命名管道服务端与客户端 `\\.\pipe\external-subagent-daemon`，采用 `CreateNamedPipeW`（设置 `FILE_FLAG_FIRST_PIPE_INSTANCE` 防管道劫持），客户端用 `CreateFileW`（真机阶段查证） | **前置拒绝骨架**（RPC 解耦阶段）：rpc 三分——`rpc/transport.rs`（共享传输无关帧处理/busy写/回收/handle_connection 泛型化 `S: Read+Write`）+ `#[cfg(unix)] mod unix`（UDS 实现保持）+ `#[cfg(windows)] mod windows`（骨架桩：`RpcServer::bind`/`RpcClient::call_interruptible` 返回 `Err(Unsupported "windows: named pipe transport pending real-machine implementation")`；并提供同签名 `RpcClient::new` 构造占位 Self；模块文档记录真机设计要点）；`rpc.rs` 保持 unix 消费者路径不变 | 交叉 check 0 error；host `cargo test -p external-daemon` 全绿；unix 帧协议字节与错误映射不变 |
| `crates/external-daemon/src/daemon.rs:108-123`<br>`SingletonLock::acquire` 中的 `libc::flock` 单例互斥锁（flock 在 :118） | 命名管道无文件系统 dev/ino，改用具名互斥量 `CreateMutexW` / `LockFileEx` 实现单例互斥（真机阶段查证） | **已有 cfg 门控零改动**：整段逻辑位于 `daemon.rs` 内，且 `daemon.rs` 整文件已置于 `#[cfg(unix)] mod daemon;`（lib.rs:1675-1678）门控下，Windows target 下不参与编译 | 交叉 check 0 error；host unix 测试全绿 |
| `crates/external-daemon/src/daemon.rs:22-40, 200`<br>`McpServer::bind` 与 `RpcServer::bind` 监听 UDS | Named Pipe 监听（真机阶段查证） | **已有 cfg 门控零改动**：整文件已置于 `#[cfg(unix)] mod daemon;`（lib.rs:1675-1678）内；在 RPC 解耦阶段于 lib.rs 补同签名 `#[cfg(not(unix))]` Daemon 桩（`start_with_shutdown` 返回 Unsupported） | 交叉 check 0 error；Windows 上 main 启动即 unsupported 退出 |
| `crates/external-mcp/src/lib.rs:6, 12`<br>无条件 `tokio::net::UnixStream::connect(socket)` 连接 daemon MCP UDS | 基于 Windows Named Pipe 客户端（`tokio::net::windows::named_pipe::ClientOptions`）连接服务端（真机阶段查证） | **前置拒绝骨架**（RPC 解耦阶段）：`UnixStream::connect` 进行 cfg 拆分，windows 臂返回 `Err(Unsupported "windows: named pipe MCP bridge pending real-machine implementation")` | 交叉 check 0 error；host `cargo test -p external-mcp` 全绿 |
| `cli/paths.mjs:24, 61`<br>CLI 中的 socket 字段（`PRODUCT_NAME.sock`） | Windows 命名管道命名规范 `\\.\pipe\external-subagent-daemon`（真机阶段查证） | **前置拒绝骨架**（CLI/win32 路径阶段）：`windowsProductPaths` 中 `socket` 输出 JS 模板串 `\\\\.\\pipe\\${PRODUCT_NAME}-daemon`，运行时字面值 `\\.\pipe\external-subagent-daemon`（前缀 9 字符，两个开头反斜杠），doc 注明 pending 阶段 1 | seam 测试断言通过；测试 4 覆盖字段 |

### 5. 权限与可执行位（0600 / 0700 / 0o111 → ACL / PATHEXT）

| Unix 构造（file:line，以 2026-10-14 源码复核为准） | Windows 对应方案（真机阶段技术，标注真机阶段查证） | 本仓库阶段 0 处置 | 验收标准 |
|---|---|---|---|
| `crates/external-daemon/src/lib.rs:23`<br>无条件引用 `use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};`（全仓编译阻断点） | Windows NT 访问控制列表（ACL）/ 安全描述符 Security Descriptors（真机阶段查证） | **前置拒绝骨架**（RPC 解耦阶段）：将该 import 改为 `#[cfg(unix)] use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};`，保留 trait 在 unix 侧对 `daemon.rs:31` 的供给 | 消除 Windows 交叉 check 阻断；交叉 check 0 error；daemon 模块编译不受损 |
| `crates/external-daemon/src/scheduler/diagnostics.rs:540-545, 559-563`<br>两处 OpenOptions 链上无条件调用 `.mode(0o600)`（全仓编译阻断点） | Windows ACL（Windows 默认仅当前用户及管理员拥有写权限）（真机阶段查证） | **前置拒绝骨架**（RPC 解耦阶段）：两处 OpenOptions 调用链重排为「构建 options → `#[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }` → `open()`」 | 消除 Windows 交叉 check 阻断；交叉 check 0 error；unix 下字节等价且保持 0600 权限 |
| `crates/external-daemon/src/daemon.rs:31`<br>`McpServer::bind` 中 `std::fs::set_permissions(&path, Permissions::from_mode(0o600))` | Named Pipe 安全描述符 SECURITY_ATTRIBUTES 限制当前用户（真机阶段查证） | **已有 cfg 门控零改动**：daemon.rs 整文件已在 `#[cfg(unix)]` 门控下 | 交叉 check 0 error |
| `crates/external-daemon/src/agent_status.rs:333-337, 778-782, 948-952, 1051-1055`<br>4 处 disposable workspace 设置 `Permissions::from_mode(0o700)` | Windows ACL 限制当前用户独占（真机阶段查证） | **已有 cfg 门控零改动**：源码复核核证 4 处均已精确包裹在 `#[cfg(unix)]` 块内 | 交叉 check 0 error；host probe 行为无损 |
| `crates/external-daemon/src/agent_status.rs:1551-1557`<br>`discover_on_path`（:1543 起）检查 `metadata.permissions().mode() & 0o111 == 0` 过滤非可执行文件 | Windows 无 POSIX 权限位，需通过 `PATHEXT` 环境变量（`.com;.exe;.bat;.cmd` 等）判定扩展名匹配（真机阶段查证） | **已有 cfg 门控零改动**：源码复核核证已位于 `#[cfg(unix)]` 块内，非 unix 默认返回 Some(candidate) | 交叉 check 0 error |
| `crates/external-daemon/src/rpc/agents.rs:139-148, 166-175, 193-202`<br>subagent 运行路径的可执行权限检查（`m.permissions().mode() & 0o111 != 0`） | 基于 `PATHEXT` 环境变量校验（真机阶段查证） | **已有回退保持**：源码在每处均已包含 `#[cfg(unix)]` 与 `#[cfg(not(unix))] { true }` 双臂 | 交叉 check 0 error |
| `crates/external-daemon/src/zcode/provider.rs:10-11, 459-460, 477-480`<br>私有目录 0700 与临时文件 0600 权限设置 | Windows ACL 设置（真机阶段查证） | **已有 cfg 门控零改动**：源码复核核证 PermissionsExt 引用与 set_permissions 均已置于 `#[cfg(unix)]` 门控下 | 交叉 check 0 error |

### 6. 随机熵源（/dev/urandom → ProcessPrng）

| Unix 构造（file:line，以 2026-10-14 源码复核为准） | Windows 对应方案（真机阶段技术，标注真机阶段查证） | 本仓库阶段 0 处置 | 验收标准 |
|---|---|---|---|
| `crates/external-core/src/general.rs:227-229`<br>提交临时工作区生成随机熵源 `if let Ok(mut source) = fs::File::open("/dev/urandom")` | Windows CNG（Cryptography Next Generation）接口 `ProcessPrng` 或 `BCryptGenRandom`（真机阶段查证） | **运行时容忍回退**：源码使用 `if let Ok(...)`，在 Windows 上若无该文件则静默跳过读取，熵数组保持 0，依赖同时参与哈希的 timestamp 与原子自增 nonce 提供唯一性保证；编译无阻断 | 交叉 check 0 error；macOS/Linux host 行为零变化 |
| `crates/external-daemon/src/rpc/handlers.rs:801-807`<br>`opaque_generation()` 中 `File::open("/dev/urandom")` | `ProcessPrng`（真机阶段查证） | **运行时容忍回退**：Windows 编译无阻断；Windows 运行时若无此文件则返回 `RpcServiceConfigError::GenerationUnavailable`；阶段 0 下 Windows daemon 在启动即 unsupported 退出，不会进入业务路径 | 交叉 check 0 error |

### 7. 信号处理与控制台事件（signal_hook → Console Ctrl Handler）

| Unix 构造（file:line，以 2026-10-14 源码复核为准） | Windows 对应方案（真机阶段技术，标注真机阶段查证） | 本仓库阶段 0 处置 | 验收标准 |
|---|---|---|---|
| `crates/external-daemon/src/main.rs:12, 55-56`<br>`signal_hook::flag::register(SIGINT/SIGTERM, Arc::clone(&shutdown_requested))` | `SetConsoleCtrlHandler` 注册控制台 Ctrl 事件处理回调函数（处理 `CTRL_C_EVENT`、`CTRL_BREAK_EVENT`、`CTRL_CLOSE_EVENT`、`CTRL_SHUTDOWN_EVENT`）（真机阶段查证） | **前置拒绝骨架**（RPC 解耦阶段）：signal_hook import 及注册调用拆分至 `#[cfg(unix)]`；windows 臂不注册，并标记 `TODO(windows-real-machine: console ctrl handler)` | 消除 Windows 交叉 check 阻断；交叉 check 0 error；Unix 信号退出语义不变 |
| `crates/external-daemon/src/main.rs:28-32, 343-365`<br>调试模式启动门 `wait_for_startup_test_gate` 中连接 `UnixStream` 监听退出信号 | 命名管道或 Windows 同步事件对象（Event Object）（真机阶段查证） | **前置拒绝骨架**（RPC 解耦阶段）：`wait_for_startup_test_gate` 拆分为 `#[cfg(all(debug_assertions, unix))]`，并补 `#[cfg(not(all(debug_assertions, unix)))]` 空实现桩 | 消除 debug 模式下 Windows 交叉 check 阻断；交叉 check 0 error |

### 8. 服务系统与账户身份（launchd / systemd / launchctl / systemctl / getuid → SCM / sc.exe / 无对应）

| Unix 构造（file:line，以 2026-10-14 源码复核为准） | Windows 对应方案（真机阶段技术，标注真机阶段查证） | 本仓库阶段 0 处置 | 验收标准 |
|---|---|---|---|
| `cli/install/service-macos.mjs:179-195`<br>三大分发函数默认参数 `uid = process.getuid()` | Windows 平台无 POSIX UID 概念，SCM 服务运行于 LocalSystem、NetworkService 或用户账户下（真机阶段查证） | **前置拒绝骨架**（CLI/win32 路径阶段）：将三大分发函数（`serviceRegistrationStatus`、`bootstrapService`、`bootoutService`）的默认参 `uid = process.getuid()` 移入 darwin 臂内惰性求值；win32 臂完全不执行 getuid | 消除 Windows 环境下调用 `process.getuid()` 抛出的 TypeError；darwin/linux 行为零变化；node --test 全绿 |
| `cli/install/service-macos.mjs:179-202`<br>四大服务分发函数（`serviceRegistrationStatus`、`bootstrapService`、`bootoutService`、`installServiceDefinition`） | 基于 Windows 服务控制管理器 SCM（`sc.exe` / Win32 Service API）进行服务的注册、启动、停止与查询（真机阶段查证） | **前置拒绝骨架**（CLI/win32 路径阶段）：四大分发函数增加 `platform() === 'win32'` 转发臂；变更类（bootstrap/bootout/installDefinition）统一抛出 `throw new CliError('UNSUPPORTED_PLATFORM_SERVICE', 'windows: service integration pending real-machine implementation')`；注册查询（registrationStatus）返回 unavailable view 形态 | node --test windows.test.mjs 测试 5 直接断言抛出 `UNSUPPORTED_PLATFORM_SERVICE`；node --check 通过 |
| `cli/install/service-activation.mjs:22-24`<br>`activateService` 分发入口 | SCM 服务平滑升级与激活（真机阶段查证） | **前置拒绝骨架**（CLI/win32 路径阶段）：`activateService` 增加 win32 分发臂，同样抛出 `CliError('UNSUPPORTED_PLATFORM_SERVICE', ...)` | win32 激活操作安全 fail-closed |
| `cli/main.mjs:23, 347`<br>`SUPPORTED_PLATFORMS = new Set(['darwin', 'linux'])`<br>`cli/install/layout.mjs:19`<br>`NATIVE_PLATFORMS = ['darwin-arm64', 'linux-x64']`<br>`package.json:23-26`<br>`"os": ["darwin", "linux"]` | 待阶段 4 真机全套验收通过后由系统拥有者（Owner）决策解禁（真机阶段查证） | **已有回退保持**：**冻结保持零改动**。维持对 Windows 的结构拒绝契约不变；`tests/platform/windows.test.mjs` 中的 17 命令拒绝测试原样保留 | 既有 `tests/platform/windows.test.mjs` 3 个测试全绿；不提前承诺未验证的平台支持 |
| `cli/install/init.mjs:166, 205`<br>`cli/maintenance.mjs:91`<br>`cli/install/service-activation.mjs:34, 116, 122`<br>其余 CLI 内部 `process.getuid()` 站点 | SCM 服务管理（真机阶段查证） | **无需处理**：此类内部站点仅在业务命令内部调用，而 `main.mjs:347` 顶层门控已在任何业务逻辑前将 Windows 拦截并抛出 `UNSUPPORTED_PLATFORM`，生产环境下完全不可达 | 既有 17 命令拒绝测试覆盖 |

### 9. 解释器 Shebang（#!/usr/bin/env node → cmd 包装）

| Unix 构造（file:line，以 2026-10-14 源码复核为准） | Windows 对应方案（真机阶段技术，标注真机阶段查证） | 本仓库阶段 0 处置 | 验收标准 |
|---|---|---|---|
| `bin/external-subagent.mjs:1`<br>CLI 可执行脚本第一行 `#!/usr/bin/env node` | Windows 原生不支持 POSIX shebang，依赖 npm 在 `npm install -g` 时自动生成的 `external-subagent.cmd` 批处理文件及 PowerShell 启动脚本，或打包为单文件二进制可执行程序（真机阶段查证） | **已有回退保持**：`package.json:31-33` 的 `"bin"` 字段已声明映射；由 npm 包管理器在全局安装或打包时自动处理 cmd 包装；且当前 Windows 受顶层门控拦截 | package.json 结构有效；npm 包打包标准一致 |

### 10. 路径规范与分隔符（rustPathParent 的 `/` → `\`）

| Unix 构造（file:line，以 2026-10-14 源码复核为准） | Windows 对应方案（真机阶段技术，标注真机阶段查证） | 本仓库阶段 0 处置 | 验收标准 |
|---|---|---|---|
| `cli/paths.mjs:95-114`<br>`function rustPathParent(value)` 模拟 Rust `Path::parent()`，当前内部硬编码 `/` 进行字符串切片匹配 | 兼容 Windows 路径分隔符 `\` 与 `/`，并正确处理 Windows 盘符（如 `C:\`）与 UNC 路径（如 `\\server\share`）（真机阶段查证） | **运行时容忍回退**：阶段 0 下 win32 业务命令被门控拦截；`profilesDir` 在未配置环境变量时直接回退至 `path.join(productPaths(home, env).data, 'profiles')`；seam 测试在 macOS 上运行，不传入含 `\` 的路径 | node --test tests/platform/windows.test.mjs 全绿；真机阶段需增补包含 `\` 与盘符的单测 |
| `cli/paths.mjs:75-77`<br>`productPaths` 仅包含 darwin 与 linux 双分支 | 增加 Windows 路径规范，基于 `LOCALAPPDATA` 构建数据目录，无 launchAgent 文件（真机阶段查证） | **前置拒绝骨架**（CLI/win32 路径阶段）：实现 `windowsProductPaths(home, env)`，冻结 12 字段平行结构（`localAppData` 解析防 `xdgDirectory` 语义，socket 输出具名管道名，launchAgent 为 null）；`productPaths` 改为三分支（darwin / win32 / linux） | node --test windows.test.mjs 新增测试 4 验证全字段结构；macOS/Linux 输出字节完全不变 |

---

## 阶段 0 涉及的 crate / 模块处置归总

| 目标 Crate / 模块 | 涉及的构造族 | 阶段 0 修改动作 | 回归保障 |
|---|---|---|---|
| `external-runtime` | 1 (进程组隔离), 2 (组收割), 3 (进程身份) | `gated_spawn` 首部 windows 早退拒绝；`respond_before` 增加 fail-closed RequestError 臂；新增 `kill_group`/`kill_process` 监督助手；`orphan-spawn-test` bin 采用双 main 源码 cfg 门控 | host macOS `cargo test -p external-runtime` 保持全绿；交叉 check 0 error |
| `external-daemon` | 2 (组收割), 4 (UDS), 5 (权限位), 6 (urandom), 7 (信号) | rpc 三分（抽出 transport、增加 windows 骨架桩、unix 保持）；lib.rs:23 及 diagnostics.rs 的 0o600 局部门控；admission.rs 迁移调共享助手；main.rs 信号注册与 debug gate cfg 拆分；lib.rs 补 Daemon 骨架桩 | host macOS `cargo test -p external-daemon` 保持全绿；全 workspace 交叉 check 0 error |
| `external-mcp` | 4 (UDS) | `UnixStream::connect` cfg 拆分，windows 臂返回 fail-closed 骨架错误 | host macOS `cargo test -p external-mcp` 保持全绿；交叉 check 0 error |
| Node CLI (`cli/`) | 4 (UDS), 8 (服务与身份), 10 (路径规范) | `paths.mjs` 增补 win32 第三分支；`service-windows.mjs` 新建 fail-closed 骨架；`service-macos.mjs` getuid 移入 darwin 惰性求值并转发 win32；`service-activation.mjs` 增加 win32 fail-closed 臂 | host macOS `node --test` 全绿；`node --check` 新增/修改脚本通过 |
| CI Workflows | CI 把关 | 新增 `.github/workflows/windows-check.yml`，在 windows-latest 上运行原生 check 与结构拒绝测试 | YAML 语法有效；npm-publish 零改动 |
