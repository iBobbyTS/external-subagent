# 共享入站事件模型（设计 owner 说明）

本文是 ES 对四条运行时通道（ZCode、Codex、DSH ACP v1、AGY stream-json）的共享
入站事件与工具投影的唯一设计说明。各通道的原生协议形状、版本与实测证据边界由
[`compatibility/`](compatibility/) 下对应文档拥有；公共工具的输入/输出字段由
[`mcp-api.md`](mcp-api.md) 与 [`schema/`](../schema/) 拥有。本文不重复定义任何
native wire 协议，也不声称超出已记录证据的外部能力。

## 1. 唯一解释链

```
native stdout / RPC frame / telemetry
  → transport（分帧、解码、匹配；保留）
  → 该通道 adapter（唯一解释者：归属、重放/late/foreign 判定、凭据脱敏）
  → NormalizedEvent 有序批次 + DecodeDisposition
  → 共同 RuntimeState（归约 turn / 正文 / 工具 / 原生失败 / 覆盖）
  → wait / observe / result / list / status / pending input 视图与持久化
```

约束：

- 同一事实只有一个语义解释 owner。transport 只保留传输、控制与清理责任；工具与
  覆盖的语义不再由 transport 反解析。
- 一次原生 settlement 解出的正文、原生失败、工具终态与 turn 边界作为同一个有序
  批次先归约齐全，再唤醒 wait/scheduler。
- 归约锁内只做归约与快照，不执行 stop、RPC 或数据库回调。
- 不是事件总线，也不做事件回放或历史重建；daemon 重启后返回真实不可用的观测。

## 2. 类型化事实与消费者

| 事实 | 用途与边界 | 消费者 |
|---|---|---|
| `SessionReady` / `SessionChanged` | 实际会话身份与已观察能力；不按二进制 hash 或配置名称启用能力 | 启动、任务身份、能力视图 |
| `TurnStarted` / `TurnSettled` | 原生或本地轮次；settlement 保留停止原因、最终正文或原生失败 | scheduler、finalizer、result |
| `MessageUpdated` / `MessageFinished` | message 身份、delta 或权威快照 | wait 文本、最终正文 |
| `PublicReasoningDelta` | 已明确公开的 summary/thought 通道及身份；不采集 `encrypted_content`、隐藏思维或原生内部上下文 | observe 有界推理尾部 |
| `ToolChanged` | call ID、名称/类别、参数补丁、执行 phase 与结果 outcome 分开；字段缺失不清除已知值 | 调用计数、in-flight、observe 参数与覆盖 |
| `ModelActivityChanged` | 原生开始的/完成/失败/重试等待/流停顿；失败结束对应 attempt，不由“turn active”伪造请求状态 | wait 活动、诊断 |
| `InputRequested` / `InputResolved` | permission/用户问题、原生 request ID、实际可答能力 | pending request、wait、respond |
| `ErrorObserved` / `Diagnostic` | 操作、原生错误、原生重试状态；非终态错误不关闭 turn | 错误交付、诊断、最终失败关联 |
| `CoverageGap` | 已知受影响维度或影响范围未知；原因区分未知语义、损坏、截断与实际丢失 | 覆盖、诊断 |
| `RuntimeExited` / `TransportFault` | 真实进程退出、frame-limit、断流与 stderr 诊断引用 | scheduler cleanup/outcome |

解码处置统一为四类：`Consumed`、`RecognizedIgnored`、`Unknown`、
`Invalid/Lost`。正常忽略不计 loss；未知语义保留有界安全 discriminator 且不使
健康任务失败；已知损坏只降低能确定的维度。未知事件与真实 `dropped_events` 分开，
`dropped_events=0` 不证明所有原生能力已覆盖。

## 3. 原生错误合同

- `NativeFailure` 拥有收到的完整原生错误 `Value`，只做凭据脱敏，不摘要代替。
  `code/status/retryable/retryAfter/reset` 等只是可选投影，原件始终权威。
- 有 task 的原生失败经现有 task result 保存，`final_text` 分页交付完整脱敏原件；
  `failure_message` 保留有界 ES 诊断与 `cleanup_result`，不能替代原件。
- 同步控制错误（初始化、模型选择、发送等没有可读 task result 的失败）在结构化
  tool error 上增加可选 `native_error`：

  ```json
  {
    "truncated": false,
    "total_bytes": 812,
    "limit_bytes": 1048576,
    "original": { "...": "完整脱敏原生值" },
    "projection": { "code": "rate_limit", "status": 429, "retry": { "will_retry": true } }
  }
  ```

  `original` 是权威值；`projection` 只包含原生明确表达的语义。同步错误同时携带
  `agent_id`，调用方可据此读取持久失败结果。
- 单个 `native_error` 原生载荷（`original` + `projection`）的序列化上限为 1 MiB（`MAX_NATIVE_ERROR_BYTES`，
  远低于 2 MiB RPC 帧上限）。超过上限时 `original` 为 `null`，`truncated=true`
  并报告 `total_bytes`/`limit_bytes`；本层明确报告限制，不宣称已完整交付，也不
  为此新增持久错误检索服务。
- 原生 `willRetry` 与调用方是否可安全重试是不同问题。ES 不自动重试任务，也不因
  收到一次 retry error 就结算失败。

## 4. 观测状态、覆盖与诊断

- `telemetry_status` 只表达当前请求、调用及 turn 活动关联在已支持能力范围内是否
  可信：`healthy` 无已知未收敛缺口，`degraded` 存在这类缺口，`unavailable` 当前
  没有可用观测。它不证明任务健康、未支持能力不存在，也不保证最近 60 秒窗口完整。
- 合法的原生失败、限流、重试和流停顿本身不降低观测完整性。当前状态只随原生事实
  在其明确范围内收敛；无关正常帧不解除关联缺口。
- 覆盖标志按实际能力与实际缺口生成，不按 subagent 名称永久固定。没有观测到的能力
  保持未知，不伪造成“零工具”或“健康”。
- 有界诊断摘要（≤32 条，含溢出聚合）记录受影响维度、处置原因、去重后的原生输入
  次数、最近发生时间与安全 discriminator。原始 payload、正文、参数、自由文本错误
  与凭据不进入摘要。wait 通过 `activity.diagnostics` 读取同一份保留摘要及其
  `diagnostics_truncated` 标志；查询只读快照，不重算、不重新持久化、不增加计数。
- 历史完整性与当前状态分开表达：turn 权威结算收敛当前关联，但不补造历史缺口；
  跨 turn/claim 的有效摘要保留；daemon 重启不回放持久记录重建内存摘要。

## 5. 工具投影与 presence 语义

`result` 仍读持久结果分页；`list`/`status` 职责不变；wait/observe 读共同状态。
可选字段采用“缺 = 能力/证据未提供，而不是零值”的语义：

| 视图 | 字段 | 语义 |
|---|---|---|
| wait `activity.model_activity` | 省略或对象 | 仅当 native 来源确实提供模型活动事实；携带 `request_id`（可选）、原生 `phase`（`started/completed/failed/retry_scheduled/stream_stalled`）与已提供的 `retry` 详情。不生成等待时间或尝试数；重试等待不是降级，也不触发 ES 自动重试。并发请求时该字段表示**最近一条**原生模型事实（带自己的 request_id），整体活跃度仍由 `model_request_active` 聚合表达；它不宣称其他请求不存在。 |
| observe `tools[].recent_calls[].status` | 枚举 | `unknown`=无原生 phase 证据；`pending/running`=执行中；`finished`=已结束但无成功/失败证据；`succeeded` 仅来自原生明确事实；`failed/cancelled` 按原生终态。 |
| observe `reasoning.source` | 省略或对象 | 实际观察到公开推理时为 `{protocol, channel}`（如 `zcode_session/summary`、`dsh_acp_v1/thought`）；从未观察到公开推理时整个 `reasoning` 为 `null`。不再按 adapter 名称固定能力，也不使用已退役的 wire 指针词汇。 |
| wait `activity.diagnostics` | array | 共同状态保留的有界、去重、已脱敏异常摘要：来源协议、受控处置原因、影响维度（`known` 列表或 `unknown` 范围）、安全 discriminator、去重后的原生输入次数与最近观察时间（epoch 毫秒）。空数组表示没有已知异常，不是完整性证明；`telemetry_status=degraded` 与摘要相互独立，不强制存在条目。 |
| wait `activity.diagnostics_truncated` | boolean | 为 true 表示有界摘要把超出上限的条目折叠进 `summary_limit` 记录。 |
| observe `coverage.*` | boolean/integer | 按实际能力与缺口生成；`dropped_events` 只统计已知丢弃，0 不证明完整。 |
| `error.native_error` | 省略或对象 | 见 §3；只在完整脱敏原件仍可用时出现。 |

wait 的 tail 上限与 take-on-delivery 游标含义保持不变：每次 wait 只交付上次之后
新增的公开文本；中断或取消的多 agent wait 不消费未交付 tail。

## 6. 已退役的共享词汇

以下内部迁移词汇已删除，不再有公共或内部消费者：

- 旧的 per-adapter reasoning source 标签（`status`/`runtime_version`/`event_type`/
  `delta_pointer`）与 `schema/public-reasoning-source.json`；由类型化
  `{protocol, channel}` 证据替代。
- `PassiveActivitySnapshot.telemetry_degraded` 布尔副本；由三态
  `telemetry_status` 单一表达。
- 已无消费者的 `turn_failure_detail` 文本投影辅助；原生失败改由 typed
  `NativeFailure` 与 `native_error` 交付。

## 7. 验证边界

本模型的实现证据来自四通道真实 adapter fixture、共同状态/调度器回归与既有捕获；
外部 seam 的 live 验证范围以任务合同为准。本文不把文档、mock 或单一固件当作未做
实测通道的实测证据。
