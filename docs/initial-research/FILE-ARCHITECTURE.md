# 文件与模块管理方案

状态：提案；D03、D09 采纳后作为新项目边界。路径是目标结构，不表示文件已经存在。拆分在对应业务 section 内完成，不另建“重构所有文件”的无限 section。

## 1. 当前问题与拆分原则

上传源码 `crates/zcode-agentd/src/lib.rs` 为 6,145 物理行，同时拥有活动解析、RuntimeOwner、Scheduler、生命周期投影、诊断与 daemon 启动；`mcp.rs` 2,475 行、`rpc.rs` 2,435 行也混合 DTO、转换与处理。行数含内联测试，不等同于纯生产代码行数。见 `SOURCE-EVIDENCE.md`。

新项目不要把它们复制后统一改成 external 名字，再往其中追加 DSH 分支。应把**公共状态不变量、provider 语义、进程管理、协议投影、安装协调**分成独立 owner。crate 的数量由依赖边界决定，不为每 400 行创建一个 crate。

## 2. 目标树

```text
external-subagent/
├── README.md
├── AGENTS.md                         # 本项目规则，无旧仓库绝对路径
├── UPSTREAM.md                       # 来源提交、许可证、已确认差异
├── Cargo.toml / Cargo.lock
├── package.json / package-lock.json
├── bin/
│   ├── external-subagent.mjs          # 参数入口；无状态机
│   └── external-subagent-mcp.mjs      # 稳定 facade 启动／兼容性检查
├── cli/
│   ├── main.mjs                      # 路由到 command owner
│   ├── commands/
│   │   ├── agents.mjs / config.mjs
│   │   ├── tasks.mjs / daemon.mjs
│   │   ├── plugin.mjs / update.mjs
│   │   └── diagnose.mjs / maintenance.mjs
│   ├── config/
│   │   ├── schema.mjs / read.mjs / write.mjs
│   ├── install/
│   │   ├── layout.mjs / payload.mjs / path.mjs
│   │   ├── codex.mjs / service-macos.mjs
│   │   └── reconcile.mjs / recovery.mjs
│   └── rpc-client.mjs / errors.mjs
├── crates/
│   ├── external-contract/src/
│   │   ├── lib.rs                    # 只导出模块
│   │   ├── task.rs / request.rs / result.rs / status.rs
│   │   └── error.rs / limits.rs
│   ├── external-runtime/src/
│   │   ├── adapter.rs / capabilities.rs / event.rs
│   │   ├── process.rs / identity.rs / deadline.rs
│   │   └── lib.rs
│   ├── external-store/src/
│   │   ├── schema.rs / task.rs / message.rs / request.rs
│   │   ├── result.rs / recovery.rs / lib.rs
│   │   └── tests/                    # 私有单元测试可用模块，不堆回 lib
│   ├── external-core/src/
│   │   ├── admission.rs / workspace.rs
│   │   ├── scheduler.rs / lifecycle.rs / completion.rs
│   │   ├── messages.rs / requests.rs / recovery.rs
│   │   ├── activity.rs / observation.rs / diagnostics.rs
│   │   └── lib.rs
│   ├── fixtures/zcode/src/
│   │   ├── protocol/                 # 原 zcode-protocol 的 wire 专属部分
│   │   ├── driver.rs / session.rs / event.rs
│   │   ├── preparation.rs / permission.rs
│   │   ├── discovery.rs / probe.rs / lib.rs
│   ├── subagents/dsh/src/
│   │   ├── acp/transport.rs / session.rs / model.rs
│   │   ├── acp/permission.rs / update.rs / result.rs
│   │   ├── profile.rs / discovery.rs / probe.rs / lib.rs
│   ├── external-daemon/src/
│   │   ├── main.rs / bootstrap.rs / agents.rs / service.rs
│   │   ├── rpc/schema.rs / dispatch.rs
│   │   └── rpc/handlers/              # wait、status、tasks、management
│   └── external-mcp/src/
│       ├── main.rs / server.rs / schema.rs / projection.rs
│       └── tools/                     # 每组操作只转到同一公共 service
├── plugins/codex/external-subagent/
│   ├── plugin.json / mcp.json          # 以本机支持的官方格式验收
│   └── skills/external-subagent/SKILL.md
├── profiles/dsh/                       # 受管启动组合，不是第二会话服务器
├── tests/
│   ├── cli/ / integration/ / contract/
│   ├── provider-conformance/ / install/ / upgrade/
│   ├── live-agent/ / platform/ / test_fixtures.py # 恢复的原测试，按实际变更适配
│   └── fixtures/                      # 有来源、已脱敏、受限大小
├── tools/probes/dsh-acp/                # 有界开发探测，不随产品常驻
├── scripts/release/                    # 构建／pack 校验；不含账号
├── npm/native/<target>/                # 已构建平台 payload
├── docs/
│   ├── architecture.md / protocol.md / operations.md
│   ├── compatibility/ / acceptance/
└── .agent-work/                        # 仅当前新 feature 的计划与记录
```

`plugins/dsh-policy/` **不是默认存在的目录**；只有 S01 证明缺口并经窄 scope delta 批准，才加入。届时它是 TypeScript 策略插件，仍不能拥有进程、task store 或 scheduler。

## 3. 依赖与唯一 owner

```text
CLI / MCP → daemon service → core → store + contract + runtime trait
                                      ↑
                     ZCode adapter / DSH adapter 实现 runtime trait
                     daemon composition root 负责选择已注册 adapter
```

- `external-contract` 不依赖 provider、CLI、数据库或 OS 服务；公共字段在一处定义。
- `external-runtime` 拥有 process identity、deadline 与回收；adapter 只控制原生协议并提交规范化事件，不能各建一套进程监控。
- `external-core` 决定 workspace admission、消息排队、请求 PENDING/SENDING/RESPONDED 与终态优先级；不得导入 `external-fixture-zcode` 或 `external-agent-dsh`。
- `external-store` 负责事务与 durable facts；provider 私有附加状态必须带 adapter/schema identity 且无密钥，不是把任意 JSON 当公共 contract。
- provider 的 wire method、model token 解码／映射、认证证据、工具字段和策略差异只能在 adapter 内；`ZCODE_*` 环境变量不能扩散到 core。
- RPC 与 MCP 使用同一业务 service 与错误分类，不各自做默认 agent/model 解析、wait wake 判断或 permission 决策。
- `cli/install/reconcile.mjs` 是唯一升级协调 owner；daemon 仅提供安全排空／激活触发事实，不能再创建另一个 updater daemon。
- caller plugin 是代码消费者：协议、工具名、能力不足、等待／结果页说明变更必须同 section 同步；不是只读旁观者。

## 4. 从当前大文件迁移到哪里

| 当前 owner | 迁移目标 | 必须一起迁移的测试／不变量 |
|---|---|---|
| agentd/lib.rs 活动解析 | ZCode event adapter + core/activity | 原生字段识别、计数支持度、公开 reasoning 过滤 |
| agentd/lib.rs RuntimeOwner/TurnTracker | ZCode session + runtime/process/deadline | generation、stdin deadline、权限关联、回收真值 |
| agentd/lib.rs Scheduler/StoreLifecycleSink | core scheduler/lifecycle/completion | 取消优先、结果只持久化一次、queue 与终态竞态 |
| agentd/lib.rs Daemon/诊断 | daemon bootstrap/service + core diagnostics | singleton、socket identity、结构化受限日志 |
| agentd/rpc.rs | daemon/rpc schema + operation handlers；类型下沉 contract | wait 谓词、100条、frame、UTF-8 分页、旧错误语义 |
| agentd/mcp.rs | external-mcp schema/projection/tools | 数字公共 agent_id、unknown-field、结果／错误与 CLI 一致 |
| agent-store/lib.rs | store 的 schema/task/message/request/result | 原事务、幂等、请求 claim、revision／结果关联 |
| preparation/policy.rs | ZCode preparation/permission；可证通用工作区函数归 core | plan 与 write_manifest、显式 hook、不扩张 trust boundary |
| driver/lib.rs + zcode-protocol | ZCode wire/driver 与 runtime 共用 process/deadline | 先区分 OS 事实和 wire 语义，再抽取，不一次复制两份 |
| cli/installer.mjs | install/payload、codex、service、reconcile | 受管 ownership、多 CODEX_HOME、发布 payload、失败补偿 |

## 5. AgentAdapter 的最小语义面

不是预先承诺具体 Rust 签名；S02 基于现有 `ManagedRuntime` / `RuntimeFactory` 最小演进：

- `capabilities`：可选模型、权限模式、响应种类、telemetry 来源、原生 resume 与产品续聊支持分别列出。
- `discover / probe`：安装与认证证据，纯本地与真实请求分层。
- `prepare_launch`：固定配置、受管 profile 与进程启动 facts；不在 spawn 途中改变全局设置。
- `open / submit / respond / cancel / close`：统一 session/generation 与 bounded deadline；Unsupported 是真实结果，不能 default no-op 后报成功。
- `events`：规范化 message/tool/request/terminal/transport-loss；unknown 原生字段不转成 allow 或 completed。

v1 不建立通用远程协议插件框架。新增第三 agent 的验收是：实现同一 trait 与 provider fixtures，composition root 登记一次；不修改 core scheduler、store 的生命周期分支与十个 MCP handler。必要的新通用能力再通过明确 contract 扩展，而非塞任意 JSON 逃避类型。

## 6. 日常文件规则

生产源文件以 150–400 行为目标，600 行触发解释，1000 行须明确例外或拆分；不是要求每个文件越小越好。`main.rs`、`lib.rs`、CLI entry 只保留装配，建议小于 150 行。独立测试按功能分组；大型 fixture／生成 schema 单独存放并注明生成方式。

禁止兜底 `utils.rs`、`manager.rs`、`helpers.mjs` 持续吞并新职责。一个函数需要改什么状态、谁能调用、哪个测试拒绝错误，都应可从所属 owner 看出。每次已批准的 section 在 touched owner 上更新最小架构图与文件规模表，不执行无关全仓重排。

最小检查是 Cargo 依赖边界、共享 contract fixture 与简单行数统计。暂不新增自制架构 DSL、全仓图分析器、强制审批 JSON 或每次工具调用前的扫描；遵循 SFD 的低流程开销原则。
