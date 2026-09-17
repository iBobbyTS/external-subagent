# MCP 工具与字段设计说明

本文说明本项目对外注册的全部 10 个 MCP 工具，以及每个输入、输出字段的使用时机、存在理由和省略／移除影响。核对日期：2026-09-17。

## 0. 调用宿主、子代理与实例边界

MCP 调用链中的上游应用称为 `host`，被调度执行的目标称为 `subagent`，连接目标协议的内部实现称为 `adapter`。例如，Codex 是一个 host，ZCode、DSH 和 Codex 是 subagent；它们不是同一层的“agent”。

Host 接入不是注册制：本机任意 MCP client 都可以作为 `custom` host 直接连接
facade 并调用公开工具。`codex` 是拥有产品特定安装和自动升级协调能力的内置 host；
Codex 支持 plugin 和直接 MCP 两种安装方式。host 注册只服务于这些宿主集成操作，
不能作为 MCP 调用或 `spawn` 的前置条件。服务端不得因为 client 没有对应的
`hosts` 条目而拒绝公开工具调用。

实例能力目前不对称：

- `host.codex` 支持多个 instance。每个 instance 由一个独立的 Codex `home` 标识，安装绑定使用 `hosts.codex.installations[].home`；状态、升级同步和解绑必须按 home 分开处理。
- `host.custom` 表示未注册的本机 MCP client。它不要求持久化 instance，也没有 Codex home、安装或自动升级绑定；每个连接按 MCP session 处理。
- 每个 `subagent` 名称目前只支持单一 instance。`subagents.zcode`、`subagents.dsh` 和 `subagents.codex` 分别描述一个受管 runtime/home；`spawn` 的 `subagent` 选择的是名称，不是 instance ID。当前协议不承诺同名 subagent 的多实例路由、实例选择或实例级故障隔离。

这意味着 `codex_home` 是 host 安装绑定信息，不是产品顶层运行时配置；不能把多个 Codex home 的能力误解为 subagent 多实例能力。

范围是当前仓库源码定义的公开接口；不代表某台机器正在运行的 daemon 已更新到这些代码。本文依据源码、schema 和测试静态核对，未重新编译或调用已安装服务。

## 1. 阅读约定与权威来源

- **输入必填**：调用时缺少会被反序列化或业务校验拒绝。
- **输入可选**：允许省略，采用下表默认行为；不代表允许显式传 `null`。
- **条件必填**：只有特定调用场景必须提供。
- **输出 `T/null`**：当前实现输出该键，但没有值时为 `null`。部分生成 schema 为兼容旧结果允许键缺失；消费者应兼容缺失。
- **输出 `T/省略`**：没有值时不序列化该键。
- **移除影响**：指从公开接口永久删除字段后的能力或兼容性损失，是基于当前实现的设计分析，不是已经执行的改动。

并非每个字段都是执行任务的硬性前提。标为“诊断”的字段主要用于排障、解释和审计；标为“便利”的字段可能由其他字段推导或额外调用替代。删除任何公开字段前仍需处理依赖它的消费者；下文不会把兼容性要求等同于所有字段都不可精简。

权威来源：

| 内容 | 源码／契约 |
|---|---|
| 工具清单、输入、输出、注册和错误映射 | [mcp.rs](../crates/external-daemon/src/mcp.rs)：`PUBLIC_TOOLS`、`SubagentMcp`、各 `Agent*Input/Output` |
| 分页、等待、消息、请求响应的实际语义 | [rpc.rs](../crates/external-daemon/src/rpc.rs)：`RpcService`、`paged_text_bounds` |
| 仓库和写入范围约束 | [general.rs](../crates/external-core/src/general.rs)：`canonical_general_repository`、`validate_manifest`、`validate_write_scope` |
| 静态公共契约摘要 | [zcode-subagent-public-api.json](../schema/zcode-subagent-public-api.json) |
| observe 的完整输出 schema | [zas-observation-v1.1.schema.json](../schema/zas-observation-v1.1.schema.json) |

`tools/list` 的输入和输出 schema 主要由 Rust 类型生成；输出 schema 接受成功对象或公共错误对象。静态公共契约 JSON 不是全部运行时 schema 的完整副本。

已知需按实现理解的差异：`list.repository` 在 Rust 输入中可省略，但 handler 强制要求提供；静态 JSON 允许 `list.phase/outcome/cursor=null`，实际自定义反序列化拒绝显式 `null`。调用方应省略无过滤条件的键。某些生成 schema 对 `Option<T>` 的可空表达也不能替代实际反序列化约束。

## 2. 协议外层与通用输入约束

典型调用（完成 MCP 初始化之后）：

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "external_subagent_wait",
    "arguments": { "agent_id": 12345678, "wait_time": 30 }
  }
}
```

| 字段 | 何时使用／为什么存在 | 缺少或移除影响 |
|---|---|---|
| `jsonrpc` | 每个 JSON-RPC 消息，值为 `2.0`；声明协议版本 | 消息不符合该协议 |
| `id` | 每次请求的关联 ID，匹配响应及取消通知 | 无法按普通请求可靠关联响应；通知不能替代工具调用 |
| `method` | 工具调用为 `tools/call`，发现工具为 `tools/list` | 无法分派协议操作 |
| `params.name` | 指定完整工具名 | 无法选择工具 |
| `params.arguments` | 承载工具参数；无参数工具传 `{}` | 有必填输入时无法执行；本文统一显式传对象 |

成功结果中的业务对象放在 `result.structuredContent`；SDK 还提供文本 `result.content`。工具执行失败使用 `result.isError=true` 和 `structuredContent.error`。成功时 `isError` 为 false 或由 SDK 省略，不能要求它一定出现。协议、未知工具或参数反序列化错误也可能在进入 handler 前返回 JSON-RPC 顶层 `error`，不保证全部符合业务错误结构。

| 响应外层字段 | 何时使用／为什么存在 | 移除影响 |
|---|---|---|
| `id`、`jsonrpc` | 响应与请求关联、协议标识 | 破坏协议处理 |
| `result` | 成功处理协议请求后的 MCP 工具响应容器 | 无法承载工具返回 |
| `result.structuredContent` | 程序消费结构化业务字段 | 只能解析文本，失去稳定类型入口 |
| `result.content[].type` | 标识内容块类型；本接口通常为 `text` | 内容块无法按 MCP 类型解释 |
| `result.content[].text` | 供文本消费者使用；业务错误保留有界 legacy 文本 | 影响仅支持文本的客户端；错误细节可能减少 |
| `result.isError` | 区分业务成功和工具执行失败 | 仅按协议成功判断的调用方可能把失败当成功 |
| 顶层 `error` | JSON-RPC／框架层失败，含协议定义的 `code/message`，可能有 `data` | 协议故障无法按标准报告；不是本文业务 `error` 的替代字段 |

所有工具输入拒绝未声明字段。所有 `agent_id` 是 **10000000–99999999 的整数**，标识持久化任务，不是 subagent 名、上游会话 ID 或字符串。下文 `integer` 均指 JSON 整数；计数、偏移为非负值，另有说明除外。

除 `wait.message_id` 明确允许 `null`，下文可选输入应通过省略表达未提供，不应传 `null`。

## 3. 工具目录

| 工具 | 用途 | MCP annotations |
|---|---|---|
| `external_subagent_status` | 系统就绪、subagent 能力和身份 | 只读、幂等 |
| `external_subagent_spawn` | 提交任务 | 非只读、非幂等 |
| `external_subagent_wait` | 有界等待可处理请求或终态结果 | 只读、幂等 |
| `external_subagent_observe` | 疑似循环时查看观测事实 | 只读、幂等 |
| `external_subagent_list` | 限定仓库范围列出任务 | 只读、幂等 |
| `external_subagent_send` | 向运行中任务排队发送消息 | 非只读、非幂等 |
| `external_subagent_respond` | 响应权限或用户问题 | 非只读、幂等 |
| `external_subagent_cancel` | 取消任务，保留历史 | 非只读、破坏性、幂等 |
| `external_subagent_result` | 分页取结果或问题正文 | 只读、幂等 |
| `external_subagent_close` | 关闭任务并回收运行资源 | 非只读、破坏性、幂等 |

所有工具的 `open_world_hint=false`；annotations 是接口提示，不能取代实际权限校验。以下各节列出的返回均为成功时的 `structuredContent`。

## 4. external_subagent_status

输入为 `{}`。读取状态不会代替显式 subagent probe 或真实执行验收；`hi` 等信息须结合检查时间和 scope 判断。

### 4.1 顶层输出

| 字段 | 类型 | 何时使用／存在理由 | 移除影响 |
|---|---|---|---|
| `mcp_version` | string | 诊断：记录服务报告的版本 | 降低版本排障能力；它本身不是兼容性协商开关 |
| `components` | map<string, ComponentState> | 判断各组件就绪情况 | 无法定位哪层不可用 |
| `capabilities` | Capabilities | 构造有界调用和了解观测能力 | 客户端只能硬编码限制 |
| `subagents` | SubagentStatus[] | 选择 subagent 前查看启用、启动、权限和模型能力 | 容易提交不受支持的组合 |
| `identity` | DeploymentIdentity | 诊断：核对组件路径及配置模型来源 | 难以解释安装／运行对象不一致 |

`ComponentState = READY | DEGRADED | UNAVAILABLE | UNKNOWN`。

### 4.2 capabilities

| 字段 | 类型 | 何时使用／存在理由 | 移除影响 |
|---|---|---|---|
| `max_rpc_request_frame_bytes` | integer | 发送较长 prompt 时了解内部 RPC 帧上限 | 更容易超过边界；该值不是正文可用字节数 |
| `max_rpc_response_frame_bytes` | integer | 评估有界响应；包含编码和元数据开销 | 只能猜测响应容量 |
| `max_wait_ms` | integer | 设置 wait 前了解最大等待毫秒数 | 需硬编码；注意输入 `wait_time` 是秒 |
| `maturity` | map<string, enum> | 诊断：按能力区分成熟程度 | 无法区分已验证与实验能力 |
| `observation` | object | 汇总 observe 能力说明 | 失去观测配置发现入口 |
| `observation.public_reasoning_default` | boolean | 判断默认是否收集公开推理 | 难以解释推理字段为空；true 也不保证每次有数据 |
| `observation.defaults` | object | 承载默认观测窗口 | 客户端需固定假设窗口 |
| `observation.defaults.top_tools` | integer | 理解工具类别截取数量，当前 3 | 易把截取的类别当作全部工具 |
| `observation.defaults.recent_calls_per_tool` | integer | 理解每类最近调用上限，当前 5 | 易把窗口当成完整历史 |
| `observation.defaults.reasoning_chars` | integer | 理解推理字符窗口，当前 200 | 易把尾部片段当成全文 |

`maturity` 值：`beta_ready | experimental_unverified_runtime`。map 的键为能力名称，不应推断固定键集合。

### 4.3 subagents[]

| 字段 | 类型 | 何时使用／存在理由 | 移除影响 |
|---|---|---|---|
| `subagent` | string | 把状态关联到 subagent 路由，如 zcode、dsh、codex | 无法知道能力属于谁 |
| `config_revision` | integer | 诊断：对照任务创建时配置 | 无法判断配置是否已变更 |
| `configured` | boolean | 判断配置是否存在 | 无法区分未配置与已禁用 |
| `enabled` | boolean | 判断配置是否允许使用 | 客户端只能通过失败获知禁用 |
| `spawn_supported` | boolean | 判断此 subagent 配置是否支持启动 | 容易把可 probe 误当作可 spawn |
| `transport_support` | object | 描述适配器传输与操作支持 | 丢失实现层能力边界 |
| `transport_support.transport` | enum | 诊断：`zcode_app_server / dsh_acp / codex_app_server` | 无法识别适配器协议 |
| `transport_support.probe` | boolean | 判断是否支持探测 | 无法预先判断 probe 支持 |
| `transport_support.spawn` | boolean | 判断适配器是否实现启动 | 与配置 gate 的差别不可见 |
| `permission_modes` | PermissionMode[] | 选择该 subagent 支持的权限模式 | 只能尝试后报错；全局枚举不代表每个 subagent 都支持 |
| `model_selection` | object | 描述模型选择能力 | 无法按 subagent 选择参数策略 |
| `model_selection.supported` | boolean | 是否应传 spawn.model | 更容易触发不支持模型选择的错误 |
| `model_selection.mode` | enum | `native_only / catalog_token`，说明选择方式 | 不清楚应省略还是使用模型 token |
| `local` | AgentScopeStatus | 查看本地 runtime 检查 | 缺少本地可用性依据 |
| `auth` | AgentScopeStatus | 查看认证相关检查 | 无法区分认证与安装故障 |
| `hi` | AgentScopeStatus | 查看最小真实交互检查 | 缺少端到端就绪证据 |

三个 scope 状态共用如下结构；无值的可选字段直接省略。

| 字段 | 类型 | 何时使用／存在理由 | 移除影响 |
|---|---|---|---|
| `state` | ComponentState | 判断本项检查结论 | 无法判断检查是否成功或未知 |
| `scope` | object | 解释检查适用范围 | 可能把其他目录的结果套用于当前任务 |
| `scope.workspace` | string/省略 | 诊断：检查使用的 workspace | 失去工作区适用性依据 |
| `scope.home` | string/省略 | 诊断：检查使用的 subagent home | 难以识别凭据／配置作用域差异 |
| `version` | string/省略 | 诊断：检查发现的版本 | 无法分析 runtime 版本差异 |
| `checked_at_ms` | integer/省略 | 判断检查结果新旧 | 无法识别陈旧状态 |
| `reason` | string/省略 | 解释非就绪等状态 | 只能看到结论，缺少原因 |

### 4.4 identity

| 字段 | 类型 | 何时使用／存在理由 | 移除影响 |
|---|---|---|---|
| `daemon` | ComponentIdentity/省略 | 诊断：服务端运行组件身份 | 难以核对服务来源 |
| `facade` | ComponentIdentity | 诊断：MCP 服务报告的 facade 身份 | 难以核对入口来源；嵌入 daemon 时可能就是 daemon 身份，不保证是独立 bridge |
| `daemon.artifact`、`facade.artifact` | object | 容纳组件制品信息 | 丢失该组件的制品描述入口 |
| `daemon.artifact.path`、`facade.artifact.path` | string/省略 | 诊断：制品路径 | 无法排查路径指向错误；路径本身不证明源码版本 |
| `models` | object | 容纳配置模型事实 | 丢失系统级模型诊断入口 |
| `models.configured` | object/省略 | 配置已知时报告模型事实 | 无法查看配置层模型；不等价于每个任务实际模型 |
| `models.configured.value` | string | 模型事实的值 | 只有来源而没有内容 |
| `models.configured.source` | string | 模型事实的来源 | 容易把配置推断当作 runtime 确认 |

## 5. external_subagent_spawn

### 5.1 输入

| 参数 | 类型／必填与默认 | 什么时候用、为什么有 | 省略行为／移除影响 |
|---|---|---|---|
| `subagent` | string；可选 | 选择 subagent，或避免默认配置变化影响路由 | 省略使用 `default_subagent`，未配置则 `subagent_required`；移除后无法逐任务选 subagent |
| `repository` | string；必填 | 指定存在的绝对 workspace 目录，建立执行和写入范围 | 缺少报错；移除后必须设计另一种明确作用域，不能默认为任意目录 |
| `permission_mode` | PermissionMode；默认 `build` | 区分执行、编辑、只读规划等授权模式 | 省略采用 build，不是自动只读；移除后无法逐任务选权限模式 |
| `prompt` | string；必填 | 给出具体任务 | 缺少报错；移除后没有任务指令 |
| `write_manifest` | string[]；默认 `[]` | 需要明确约束可写相对路径时使用 | 非 plan 空清单会采用受保护 workspace scope，并非禁止写入；移除后失去细粒度调用方写入范围 |
| `model` | string；可选 | subagent 支持时指定任务模型 | 省略采用 subagent 配置／原生默认；移除后失去逐任务模型选择；ZCode 当前显式传入会被拒绝 |

`PermissionMode = build | edit | plan | yolo`，实际可用组合以 subagent 能力和 admission 为准。DSH 当前支持 build 和严格 plan，不能因为公共枚举有 edit/yolo 就假设可用。

`prompt` 必须非空白、无 NUL，最大 262144 字节。`write_manifest` 不允许重复路径、绝对路径、`..`，或包含 `.git`／`.gitmodules` 路径组件；plan 模式必须为空。整个内部 RPC 帧另有上限，因此正文上限不等于完整请求上限。`repository` 名称沿用契约，实际通用准备逻辑要求目录，不应仅因名称就额外假设必须有 `.git`。

### 5.2 输出

| 字段 | 类型 | 什么时候用、为什么有 | 移除影响 |
|---|---|---|---|
| `agent_id` | integer | 保存后用于 wait/send/respond/result/cancel/close/observe | 无法可靠操作刚创建的任务 |
| `submission_disposition` | `created / existing` | 区分新建与匹配已有提交 | 调用方无法判断本次是否新建；不是可任意重放 spawn 的承诺 |
| `phase` | string | 获取提交后阶段 | 必须额外查询才能知道是否排队或运行；属于便利信息 |

spawn 标注非幂等；返回超时不能直接推断未创建任务，不应通过无限重试推断唯一性。

## 6. external_subagent_wait

### 6.1 输入

| 参数 | 类型／必填与默认 | 什么时候用、为什么有 | 省略行为／移除影响 |
|---|---|---|---|
| `agent_id` | integer；必填 | 选择要等待的持久任务 | 缺少报错；无目标无法等待 |
| `wait_time` | integer，0–299 秒；默认 290 | 适配调用方超时预算；0 用于立即取快照 | 省略仍可能等 290 秒；移除后客户端不能调整等待预算 |
| `message_id` | string/null；默认无 | 需要一起查看某条 send 消息回执时使用 | 省略不请求该回执；移除后失去按消息关联反馈的入口 |
| `supports_answer` | boolean；默认 false | 调用方能以 answer+content 回答用户问题时声明 | false 时用户输入不会作为可回答的提前唤醒目标；移除后无法区分支持与不支持回答的客户端 |

普通进度、消息回执、不可响应请求不提前唤醒。权限请求和调用方声明能处理的用户问题、终态结果才构成提前返回条件。`pending_requests` 投影最多 100 条，但唤醒判断扫描完整 pending 集合，并确保触发唤醒的请求在返回中。取消本次 MCP 等待不等于取消持久任务；取消任务应调用 cancel。

### 6.2 输出

| 字段 | 类型 | 什么时候用、为什么有 | 移除影响 |
|---|---|---|---|
| `task` | PublicTask | 检查持久阶段、终态和资源状态 | 无法从等待响应直接判断任务生命周期 |
| `pending_requests` | PublicPendingRequest[] | 找出需要 respond 的权限或用户问题 | 任务等待输入时缺少可操作的请求 ID 和语义 |
| `result_available` | boolean | 快速判断有无终态结果 | 客户端须从结果等字段推导；属于显式便利信号 |
| `activity` | PublicActivity | 诊断当前活动和遥测可信程度 | 难以判断运行中是在工作、等待还是观测失效 |
| `latest_progress` | string/null | 给人看的最新进度摘要 | 降低可读性；不应作为终态判断依据 |
| `result` | PublicResult/null | 内嵌结果，避免再请求首段 | 必须额外调用 result，长结果仍需分页 |
| `instruction` | string/null | 给出下一步响应或取分页的提示 | 客户端需自行从结构化字段实现同一判断；不能只解析该自然语言字段 |
| `timed_out` | boolean | 区分本次等待到期与事件唤醒 | 容易把等待到期误判为任务失败；不等于 outcome=TIMED_OUT |
| `message_receipt` | MessageReceipt/null | 关联输入 message_id 的投递反馈 | 不能获知所发消息的投递状态；不保证模型已经执行消息要求 |

嵌入结果 `complete=true` 时已取得全部结果，不必重复 result；否则沿 `next_offset` 继续。

## 7. external_subagent_observe

仅在怀疑 ZCode 任务无进展循环时使用，不作为健康任务的常规轮询。读取已捕获的公开数据，不启动模型、不执行工具、不自动判定循环或取消。

输入：

| 参数 | 类型／要求 | 使用理由 | 省略／移除影响 |
|---|---|---|---|
| `agent_id` | integer；必填 | 限定读取哪一个任务的观测 | 缺少报错；不能安全地关联观测与任务 |

输出：

| 字段 | 类型／范围 | 什么时候用、为什么有 | 移除影响 |
|---|---|---|---|
| `tools` | array，最多 3 类 | 查看生命周期计数最高的工具类别 | 无工具调用过程可供判断 |
| `tools[].tool_name` | 非空 string | 识别工具类别 | 无法理解调用含义 |
| `tools[].call_count` | integer ≥1 | 理解该任务生命周期内累计调用频度 | 仅见最近窗口，无法判断累计频度；高频本身不证明循环 |
| `tools[].recent_calls` | array，1–5 条 | 查看该类工具最近调用 | 无法检查实际参数或调用差异 |
| `recent_calls[].seq` | integer ≥1 | 比较记录先后 | 缺少事件顺序依据 |
| `recent_calls[].tool_call_id` | 非空 string | 关联同一个工具调用 | 相同参数的不同调用更难区分 |
| `recent_calls[].arguments` | object | 判断是否重复相同输入、是否在探索新路径 | 只有名称无法判断动作是否等价 |
| `recent_calls[].arguments_truncated` | boolean | 判断参数是否完整 | 易把被截断参数误当作完整输入 |
| `recent_calls[].redacted_fields` | integer | 说明字段脱敏数量 | 易把缺失数据误当作没有提供 |
| `reasoning` | object | 公开推理尾部的容器 | 缺少动作上下文 |
| `reasoning.text` | string，最多 200 Unicode 字符 | 理解最近公开思路 | 判断上下文减少；不含加密内容或私有推理 |
| `reasoning.truncated` | boolean | 提醒只看到了尾部片段 | 易把片段当作完整解释 |
| `coverage` | object | 描述采集完整性 | 无法评估观测证据的局限 |
| `coverage.tool_history_complete` | boolean | 判断工具历史采集是否完整 | 易把未观测到当作没发生；不意味着有限窗口返回所有调用 |
| `coverage.reasoning_complete` | boolean | 判断推理采集覆盖 | 易对缺失片段过度推断 |
| `coverage.dropped_events` | integer | 量化丢弃事件 | 无法衡量证据缺口 |

不返回工具结果，因而不能据此证明执行成功、文件没有变化或任务失败。公开描述中的 `PROGRESSING / EXPECTED_WAIT / NEEDS_CLARIFICATION / NO_PROGRESS_LOOP / INSUFFICIENT_OBSERVABILITY` 是调用方判断用语，**不是返回字段或服务端分类结果**。

## 8. external_subagent_list

### 8.1 输入

| 参数 | 类型／要求 | 什么时候用、为什么有 | 省略行为／移除影响 |
|---|---|---|---|
| `repository` | string；业务必填 | 明确查询作用域 | 省略返回 validation；移除会失去当前 daemon 强制的仓库范围契约 |
| `subagent` | string；可选 | 只查询特定 subagent 任务 | 省略不按 subagent 过滤；移除后客户端需自己过滤 |
| `phase` | Phase；可选 | 只找运行中、等待输入、终态等任务 | 省略不按阶段过滤；移除后扩大查询量 |
| `outcome` | Outcome；可选 | 查失败、取消或成功历史 | 省略不按结果过滤；移除后需自行筛选 |
| `cursor` | string；可选 | 使用上一页 next_cursor 获取续页 | 省略取起始页；移除后无法访问超出首批的任务 |
| `limit` | integer，1–100；默认100 | 控制单页大小 | 省略最多100条；移除后只能固定页大小 |

`Phase = QUEUED | PREPARING | RUNNING | WAITING_INPUT | CANCELLING | TERMINAL`。

`Outcome = COMPLETED | FAILED | CANCELLED | TIMED_OUT | RUNTIME_LOST | RESULT_INVALID`。

### 8.2 输出

| 字段 | 类型 | 使用理由 | 移除影响 |
|---|---|---|---|
| `tasks` | PublicTask[] | 返回符合范围与过滤条件的任务 | 无查询内容 |
| `next_cursor` | string/null | 继续分页；null 表示没有续页 | 无法可靠取下一页；不能用条数猜是否结束 |

## 9. external_subagent_send

向运行中任务排队发送消息，不是创建新任务或恢复终态任务的接口。

| 输入参数 | 类型／要求 | 什么时候用、为什么有 | 省略行为／移除影响 |
|---|---|---|---|
| `agent_id` | integer；必填 | 选择消息收件任务 | 缺少报错；无法路由消息 |
| `message_id` | string；可选 | 对同一消息进行重试、关联 wait 回执 | 省略由 daemon 生成；移除后无法在响应丢失时使用调用方已知 ID 去重 |
| `content` | string；必填 | 传递补充指令 | 缺少报错；无法表达消息内容 |

`content` 去掉空白后不能为空、无 NUL、最大16384字节。显式复用 message_id 应保持原任务和正文不变；冲突可能返回 `MESSAGE_ID_CONFLICT`。省略 ID 后重复调用可能产生多条消息，因此 MCP 标注非幂等。

| 输出字段 | 类型 | 使用理由 | 移除影响 |
|---|---|---|---|
| `message_id` | string | 返回实际使用的 ID，供重试或 wait 查询 | 自动生成的消息无法被后续关联 |
| `disposition` | `queued / delivered / already_delivered / failed` | 区分排队、送达、重复确认和失败 | 容易把排队成功当作已送达；送达也不代表指令执行完成 |

## 10. external_subagent_respond

| 输入参数 | 类型／要求 | 什么时候用、为什么有 | 省略行为／移除影响 |
|---|---|---|---|
| `agent_id` | integer；必填 | 限定请求所属任务 | 缺少报错；无法校验任务归属 |
| `request_id` | string；必填 | 指向 wait.pending_requests 中具体交互请求 | 缺少报错；同任务可能有多个请求，不能只靠 agent_id |
| `decision` | `allow / deny / answer`；必填 | 权限请求用 allow/deny，可回答用户问题用 answer | 缺少报错；无明确决定不能响应 |
| `content` | string；answer 时必填，其他情况可省略 | 为用户问题提供实际答案 | answer 省略或仅空白报错；移除后不能表达答案。不要把该字段自行定义成权限审批备注 |

提供的 content 不能为空、无 NUL、最大16384字节；answer 还要求非空白。请求必须可响应，decision 必须符合请求类型；不能凭 `allow` 越过硬性策略。

| 输出字段 | 类型 | 使用理由 | 移除影响 |
|---|---|---|---|
| `disposition` | `responded / already_responded / in_flight` | 处理幂等重试，区分已响应与发送中 | 不清楚是否需要继续等待 |
| `requested_decision` | Decision | 回显调用方决定 | 诊断时缺少策略前输入；调用方本可本地保存 |
| `effective_decision` | Decision | 知道策略后真正采用的决定 | 可能误以为 allow 已实际放行 |
| `policy_overrode` | boolean | 明确告知策略是否覆盖输入 | 需比较 requested/effective 等信息推导；属于便利解释信号 |
| `policy_reason_code` | string/null | 解释覆盖／策略结果 | 无法理解为什么没有采用请求决定 |

此处 request_id 是**待响应交互 ID**；错误结构中的 request_id 通常是 facade 生成的 **RPC 关联 ID**，不可互换。

## 11. external_subagent_cancel 与 external_subagent_close

两个工具均输入 `{ "agent_id": 12345678 }`，成功输出 `{ "task": PublicTask }`。

| 工具／字段 | 要求／类型 | 什么时候用、为什么有 | 省略／移除影响 |
|---|---|---|---|
| cancel 输入 `agent_id` | integer；必填 | 需要中止任务时指定目标 | 缺少报错；无法确定取消哪个任务 |
| cancel 输出 `task` | PublicTask | 判断取消请求是否记录、当前阶段及资源状态 | 无法从本次返回判断取消进展，需额外查询 |
| close 输入 `agent_id` | integer；必填 | 用完任务或需要关闭运行资源时指定目标 | 缺少报错；无法确定回收目标 |
| close 输出 `task` | PublicTask | 查看关闭与回收状态 | 无法确认 close_requested、closed、resources_reaped |

cancel 不删除历史；close 也保留历史。不要把取消请求已记录、任务已终态、任务已关闭和资源已回收视为同一事实。两个工具都标记幂等和破坏性。

## 12. external_subagent_result

| 输入参数 | 类型／要求 | 什么时候用、为什么有 | 省略行为／移除影响 |
|---|---|---|---|
| `agent_id` | integer；必填 | 选择任务结果或任务所属问题 | 缺少报错；无目标 |
| `request_id` | string；可选 | 读取 wait 中被分页的问题正文 | 省略读取任务终态结果；移除后长问题超出首段部分不可恢复 |
| `offset` | integer ≥0；默认0 | 以 next_offset 继续读取 | 省略重取首页；移除后不能连续取全文 |
| `limit` | integer，1–262144字节；默认262144 | 控制响应页大小 | 省略用最大页；移除后客户端失去页大小控制 |

request_id 若提供须非空、无 NUL、最多256字节。offset 是 UTF-8 **字节偏移**，须是合法字符边界；使用服务端 next_offset，不要按字符数或固定 limit 自行累加。limit 太小无法容纳下一个完整字符时可能报校验错误。

| 输出字段 | 类型 | 使用理由 | 移除影响 |
|---|---|---|---|
| `task` | PublicTask | 与读取的文本一起获得任务生命周期 | 单凭 null result 无法判断任务所处阶段 |
| `result` | PublicResult/null | 不带 request_id 时返回终态文本；非终态为 null | 无法取得任务产物正文 |
| `question` | PublicQuestion/null | 带 request_id 时返回问题页 | 无法恢复长问题正文 |

问题分页模式的 result 为 null；任务结果模式的 question 为 null。两者不是要求同时有值。

## 13. 复用输出结构

### 13.1 PublicTask

用于 wait、list.tasks[]、cancel、result、close。

| 字段 | 类型 | 什么时候用、为什么有 | 移除影响 |
|---|---|---|---|
| `agent_id` | integer | 确认快照所属任务，特别是列表与并发返回 | 无法把快照可靠关联到操作目标 |
| `phase` | string，当前值见 Phase | 判断任务执行阶段 | 只能看到结果时才知道是否结束 |
| `outcome` | Outcome/null | 判断终态是成功、失败、取消等 | TERMINAL 无法表达成功与否 |
| `reason_code` | string/null | 诊断：解释终态或异常原因 | 只能获知粗粒度 outcome |
| `cancel_requested` | boolean | 判断取消意图是否已记录 | 无法区分尚未申请与正在取消 |
| `close_requested` | boolean | 判断关闭意图是否已记录 | 无法判断正在关闭的过程 |
| `closed` | boolean | 判断关闭事实是否成立 | 不能把 close_requested 当关闭完成 |
| `resources_reaped` | boolean | 确认运行资源是否回收 | 任务终态不再能说明资源清理进度 |
| `input_identity` | InputIdentity/null | 诊断：追溯任务接纳时配置与执行范围 | 只能看当前系统配置，无法解释历史任务 |

phase 在输出类型中是 string，而非强制枚举。不要把 phase、activity.state、outcome 三者混为同一状态机。

### 13.2 InputIdentity

当前投影通常生成对象，各字段可为 null；旧数据可能缺少 admission 信息。

| 字段 | 类型 | 什么时候用、为什么有 | 移除影响 |
|---|---|---|---|
| `subagent` | string/null | 追溯接纳该任务的 subagent | 无法区分不同上游执行来源 |
| `config_revision` | integer/null | 对照提交时配置版本 | 当前配置变化后无法解释旧任务 |
| `adapter_version` | string/null | 排查适配器版本行为差异 | 缺少适配器证据 |
| `model` | string/null | 查看已记录模型选择；null 不应猜测为某个模型 | 无法追溯已知模型选择 |
| `model_source` | string/null | 区分模型信息来源 | 容易把默认配置当作实测确认 |
| `workspace_path` | string/null | 核对实际工作目录 | 难以确定执行位置 |
| `permission_mode` | string/null | 追溯任务采用权限模式 | 无法解释读写限制来源 |

### 13.3 PublicResult 与 PublicQuestion

| 结构／字段 | 类型 | 什么时候用、为什么有 | 移除影响 |
|---|---|---|---|
| Result.`outcome` | Outcome | 独立消费结果页时判断任务成功与否 | 需依赖外层 task；属于随结果携带的冗余上下文 |
| Result.`final_text` | string | 当前页结果正文 | 没有结果内容 |
| Result.`partial` | boolean | 标识任务产物本身是否不完整 | 可能把中断后的片段当完整任务产物 |
| Question.`text` | string | 当前页问题正文 | 无法作出有上下文的回答 |
| 两者的 `offset` | integer | 识别本页字节起点，避免拼接错位 | 难以检测重复／错页 |
| 两者的 `total_bytes` | integer | 知道全文长度、检查累计读取量 | 失去长度校验和进度信息；仍可用 next_offset 翻页 |
| 两者的 `next_offset` | integer/null | 获取下一页的合法字节起点 | 无法可靠按服务端分页继续，尤其涉及 UTF-8 边界 |
| 两者的 `complete` | boolean | 直接标识全文分页结束 | 可由 next_offset 为 null 推导；是便利字段，不表示任务成功 |

`partial` 与 `complete` 含义不同：可能 `partial=true` 且 `complete=true`，表示已取完一个不完整任务产物的全部文本。wait 内嵌问题首段最多2048字节，超过后按 question.next_offset 调 result 并带上该 request_id。

### 13.4 PublicPendingRequest

| 字段 | 类型 | 什么时候用、为什么有 | 移除影响 |
|---|---|---|---|
| `request_id` | string | 响应／分页特定问题 | 无法精确 respond 或读长问题 |
| `kind` | `permission / user_input / unsupported_input` | 选择审批、回答或标记不支持 | 容易把用户问题当权限决定 |
| `state` | `pending / sending / responded` | 区分响应所处阶段 | 容易重复处理仍在发送中的请求 |
| `respondable` | boolean | 判断当前公开接口能否处理 | 可能对不可响应类型盲目提交 |
| `tool_name` | string/null | 权限审批时识别触发工具 | 缺少具体动作来源 |
| `operation` | `read / write / command / network / git_ref_mutation / user_input / unknown` | 从语义上理解请求操作 | 必须猜工具名或解析摘要 |
| `summary` | string | 快速展示请求概要 | 可读性下降；摘要不能替代完整问题 |
| `question` | PublicQuestion/null | 可回答问题的有界首段 | 需额外请求才能了解题目；没有 request_id 则无法补读 |
| `policy_preview` | `externally_decidable / hard_deny / unknown` | 判断外部决定是否可能生效 | 容易误认为 allow 能越过硬拒绝；preview 不是最终决定 |

### 13.5 PublicActivity

这些是诊断事实，不是自动检测循环、成功或失败的结论。

| 字段 | 类型 | 什么时候用、为什么有 | 移除影响 |
|---|---|---|---|
| `state` | `queued / preparing / active / waiting_input / cancelling / idle / terminal` | 快速理解活动状态 | 需要拼接其他遥测字段推断 |
| `last_runtime_event_at` | integer/null | 对照 runtime 最近事件时点 | 无法与外部日志对齐 |
| `last_activity_age_ms` | integer/null | 判断距最近活动多久 | 需自行依据时钟计算或无法判断 |
| `model_request_active` | boolean | 区分模型请求活跃与其他阶段 | 不清楚是否正在等模型 |
| `model_request_age_ms` | integer/null | 诊断当前模型请求持续时间 | 缺少慢请求依据 |
| `model_last_delta_age_ms` | integer/null | 区分长请求持续输出与长时间无 delta | 只看请求总时长容易误判停滞 |
| `latest_text_tail` | string | 展示近期公开文本尾部 | 缺少人可读活动线索 |
| `latest_text_updated_at` | integer/null | 判断尾部文本新旧 | 易把旧文本当新进展 |
| `latest_text_truncated` | boolean | 判断尾部是否省略了前文 | 易把片段当完整输出 |
| `active_tools` | ActiveTool[] | 查看尚在进行的工具 | 难以区分工具执行与模型计算 |
| `active_tools[].tool_call_id` | string | 关联正在执行的具体调用 | 同类工具无法区分 |
| `active_tools[].kind` | `read / bash / other` | 粗分类活动工具 | 只剩不透明 ID |
| `window_60s` | ActivityWindow | 把近期活动放在固定时间窗口比较 | 累计值不能直接反映近期工作 |
| `telemetry_status` | `healthy / degraded / unavailable` | 判断其他遥测字段是否可靠 | “没有观测”容易被误判为“没有活动” |

时间 age 字段单位为毫秒。`*_at` 用于事件时点，未知值为 null；调用方不能把 null 转换成0后推断已静默很久。

ActivityWindow 的所有字段均为 integer，单位是最近60秒内的事件／调用计数：

| 字段 | 什么时候用、为什么有 | 移除影响 |
|---|---|---|
| `reasoning_delta_events` | 判断公开推理 delta 活动 | 缺少推理流活动线索 |
| `text_delta_events` | 判断文本流活动 | 缺少文本输出频度线索 |
| `tool_calls_started` | 判断工具发起频度 | 看不出近期启动了多少调用 |
| `tool_calls_completed` | 区分启动和完成 | 只有启动不能说明已有完成 |
| `tool_calls_failed` | 识别近期失败频度 | 难以发现失败重试模式 |
| `read_calls` | 判断读取类工具活动 | 失去读取类统计 |
| `bash_calls` | 判断命令类工具活动 | 失去命令类统计 |
| `other_tool_calls` | 保留其他类别统计 | 可能把非 read/bash 工具漏算 |

这些计数不能证明产生了有效任务进展，跨窗口的开始与结束也不能简单相减得出当前活动工具数。

### 13.6 MessageReceipt

| 字段 | 类型 | 什么时候用、为什么有 | 移除影响 |
|---|---|---|---|
| `message_id` | string | 确认回执对应哪条消息 | 无法关联发送行为 |
| `state` | string | 查看持久化投递状态 | 无法判断排队／交付过程；公开类型不是固定 enum |
| `target_turn_id` | string/null | 诊断消息关联的上游 turn | 难以追查投递位置 |
| `failure_code` | string/null | 诊断消息投递失败 | 只能知道失败但不知道原因 |
| `created_at_ms` | integer | 记录消息创建时点 | 无法计算排队时间 |
| `delivered_at_ms` | integer/null | 记录交付时点 | 缺少交付耗时与时间线；null 不能视为已交付 |

## 14. 所有工具的公共错误结构

```json
{
  "error": {
    "code": "not_found",
    "message": "agent task was not found",
    "component": "daemon",
    "operation": "result",
    "request_id": "subagent-mcp-12",
    "agent_id": 12345678
  }
}
```

| 字段 | 类型／是否出现 | 什么时候用、为什么有 | 移除影响 |
|---|---|---|---|
| `error` | object，业务失败时 | 将失败与成功结构分开 | 客户端无法按公共错误契约解析 |
| `error.code` | string，必有 | 程序分类故障、决定是否重试／修正输入 | 只能解析自然语言，行为不稳定 |
| `error.message` | string，必有 | 给人看的公开错误解释 | 可读性和排障能力下降 |
| `error.component` | string/省略 | 诊断 facade、daemon、daemon_transport 等层次 | 难以定位失败边界 |
| `error.operation` | string/省略 | 诊断哪种操作失败 | 多操作日志难以关联 |
| `error.request_id` | string/省略 | 关联 facade 发起的 RPC 请求 | 无法从错误定位具体 RPC；不是待回答问题的 ID |
| `error.agent_id` | integer/省略 | 关联任务或冲突的活动任务 | 难以查找受影响任务；workspace busy 时尤其有用 |
| `error.prompt_count` | integer/省略 | subagent 路由／能力拒绝时记录0，说明未送出 prompt | 缺少拒绝发生在模型调用前的显式证据；不是通用 token 计费统计 |

当前公开映射的错误码：

```text
validation, subagent_required, subagent_unknown, agent_disabled,
agent_unsupported, model_selection_unsupported, oversized,
protocol_error, not_found, conflict, runtime_command_failed,
timeout, runtime_lost, result_invalid, persistence, internal,
unavailable, daemon_unavailable
```

`conflict` 的 message 可为 `WORKSPACE_BUSY`、`MESSAGE_ID_CONFLICT` 或通用 `durable state conflict`。结构化 message 可能是有意收敛后的说明，文本 content 可能包含有界校验细节；不要假设两者完全相同。错误不保证都能附带 operation、request_id、agent_id，例如在请求派发前失败时。

## 15. 调用示例与字段取舍

以下为工具 arguments 示例。路径需替换成真实目录，后续 ID 必须使用实际返回值。

```json
{
  "subagent": "dsh",
  "repository": "/absolute/path/to/project",
  "permission_mode": "plan",
  "prompt": "阅读代码并解释入口，不修改文件。"
}
```

将其交给 `external_subagent_spawn`，保存返回 agent_id，再调用 wait：

```json
{ "agent_id": 12345678, "wait_time": 30, "supports_answer": true }
```

出现可回答问题时，读取 question；有 next_offset 则补读剩余问题：

```json
{ "agent_id": 12345678, "request_id": "request-from-wait", "offset": 2048 }
```

其中2048只是示例，实际必须使用服务端返回的 next_offset。随后 respond：

```json
{
  "agent_id": 12345678,
  "request_id": "request-from-wait",
  "decision": "answer",
  "content": "只分析启动流程。"
}
```

继续 wait，结果完整后调用 close；若结果未完整则用 result 分页。

设计精简时，可以区别以下情况，而不把所有字段都视为不可删除：

| 类别 | 示例 | 精简需要保留的语义 |
|---|---|---|
| 路由／任务必需 | agent_id、repository、prompt、交互 request_id | 必须有等价方式确定目标、范围和任务内容 |
| 有界传输必需 | offset、next_offset、cursor、正文页 | 必须仍能恢复全部长文本和长列表 |
| 权限／正确性解释 | permission_mode、write_manifest、effective_decision、coverage | 必须避免扩大授权或对缺失证据作错误判断 |
| 可推导／便利 | complete、result_available、requested_decision、instruction | 可精简，但需迁移消费者并明确替代计算／调用 |
| 诊断 | identity、config_revision、活动时间、各类计数 | 不一定阻止执行，但会降低排障和可追溯性 |

本文只说明现有接口及其设计代价，不修改协议。后续修改参数时，应同步 Rust 输入／输出及 handler、静态契约、相关回归测试和本文；以实际行为为准处理 schema 与运行时校验差异。
