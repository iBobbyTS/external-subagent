# Google Antigravity (`agy`) stream-json compatibility probe

本文记录 2026-09-28 对 Google Antigravity CLI（`agy` 1.2.12，长驻 `--input-format stream-json` 模式）的真实机器探测结果，评估其作为本产品 subagent 的接入面。**Antigravity (`agy`) 现已作为第四个 subagent 注册，但全部 spawn 入口默认关闭、由 `enabled + spawn_supported + AGY_RUNTIME_PATH` 门控；live smoke 证据仍待「集成与完成」阶段回填（见 §10「接入状态」）。** 全部原始证据（stdout/stderr/事件流）保存在 `.agent-work/tmp/antigravity-probe/logs/`（未跟踪目录）。

探测环境：macOS darwin 25.6.0 arm64；`agy 1.2.12`（`~/.local/bin/agy`，home `~/.gemini/antigravity-cli/`）；已认证（缓存凭据有效，未登录时 headless 直接报错不挂起）；settings.json 仅含 `trustedWorkspaces: ["/Users/ibobby"]`（探测期间曾临时注入 permissions 规则，已恢复原样并核对）。官方 headless 文档：`antigravity.google/docs/cli/headless/`。

## 1. 协议事实（实测事件形状）

长驻模式：`agy --input-format stream-json --output-format stream-json`，stdin 每行一个 NDJSON `user` 事件跑一个 turn，共享 `conversation_id`；关 stdin 后当前 turn 完成并 exit 0。`text` 是唯一 content block 类型。

**输入**（唯一文档化事件）：`{"event":"user","message":{"content":"..."}}`。

**输出**（payload 嵌在内层对象，官方文档描述的"顶层字段"实为内层字段）：

```jsonc
// init 恰好一个
{"event":"init","conversation_id":"<uuid>","init":{"cwd":"...","tools":[/* 57 个 */],"permission_mode":"request-review"}}
// step_update 0..N 个；step_index 全局递增，user_input 也占一步
{"event":"step_update","step_update":{"conversation_id":"...","step_index":2,"state":"ACTIVE","step_type":"tool","tool_info":{"name":"run_command","parameters":{"CommandLine":"echo X"}}}}
{"event":"step_update","step_update":{...,"state":"DONE","step_type":"tool","tool_info":{...,"output":"X\r\n"}}}
{"event":"step_update","step_update":{...,"step_type":"agent_response","state":"ACTIVE","text_delta":"..."}}
// result 每个 turn 一个（例外见 §4 双 result 陷阱）
{"event":"result","result":{"conversation_id":"...","status":"SUCCESS","response":"...","denied_actions":[{"action":"write_file","display_name":"WriteToFile"}],"structured_output":{...},"error":"...","duration_seconds":1.5,"num_turns":2,"usage":{"input_tokens":0,"output_tokens":0,"thinking_tokens":0,"cache_read_tokens":0,"total_tokens":0}}}
```

实测见过的 `step_type`：`user_input`、`agent_response`、`tool`、`system_message`（resume 时出现）。文档另列 `checkpoint` 与 `subagent_info`（未采样）。`status` 枚举：`SUCCESS|ERROR|CANCELED|INTERRUPTED|INVALID|WAITING|RUNNING`（实测只出现过 SUCCESS/ERROR）。`num_turns`/`usage`/`duration_seconds` 按**会话累计**，`response` 按 turn。`thinking_tokens` 只在 usage 中，无 thought 文本流。

stderr 与协议分离：进度、soft-deny 通知、错误原因走 stderr，stdout 恒为 NDJSON。

## 2. 权限矩阵（核心探测）

headless 默认姿态是**全工具拒绝**（不是官方文档所称"文件 I/O 自动放行"）：默认 `request-review` 下 `run_command` 与 `write_to_file` 均被 soft-deny——任务继续、exit 0、response 可能为空，但 result 附带结构化 `denied_actions`（含 action/display_name），stderr 给人类可读提示。

| 配置 | shell | 文件写 | 证据 |
|---|---|---|---|
| 默认 `request-review` | soft-deny | soft-deny | `denied_actions:[{command},{write_file}]`，exit 0，无文件产生 |
| `--mode plan` | 拒（denied_actions 报 command） | 无文件产生 | 模型层引导，非工具禁用 |
| `--mode accept-edits` | 未测 | **成功**（edit-write.txt=EDITPROBE） | |
| settings `permissions.allow:["command(echo)"]` | **放行**，模型转述真实输出 | 同机制推断 | `ALLOW_RULE_OK` |
| settings `permissions.deny:["command(echo hi)"]` | **强制拒**（工具内错误 "Matches user-configured deny rule"，非 soft-deny） | 同机制推断 | |
| `--dangerously-skip-permissions` | **放行** | 推断同 | `YOLO_OK` |
| `--sandbox`（默认权限下） | 仍拒 | 未测 | 不构成放行；错误措辞区分 sandboxed/unsandboxed 命令 |

对项目 permission_mode 的映射结论：

- **yolo** → `--dangerously-skip-permissions`，直接可用。
- **build（legacy `["."]`）** → `--mode accept-edits` 或注入 `permissions.allow` 规则（`command(...)`/`write_file(path)` 模式）。allowlist 是预授权不是拦截守卫，**write-manifest composition 无等价物**：allow 之外的写会被 soft-deny 但任务继续跑（denied_actions 可上报），达不到 `FS_WRITE_MANIFEST_DENIED` 的失败语义。
- **plan** → 可构造的 fail-closed 姿态：`--mode plan`（模型层引导）+ `permissions.deny` 兜底（强制层）+ hi probe 的 any-tool-step 检测（tool step 全可见，含被自动拒绝的调用）。注意其语义是"工具被强制拒绝"而非"工具被禁用"，与 codex 的 confirmed plan posture 语义有差距，admission 是否接受该姿态是一个产品决策。

## 3. 会话与 resume（全部原生可用）

- print 单发：`-p` + `--output-format text|json|stream-json` 全通；新会话单 turn 1.5–8s。
- print resume：`--conversation <id>` 与 `--continue` 均接续原上下文（正确记得先前 turn 内容），`num_turns` 接续累计；**单轮耗时 53–58s**（上下文重放，远高于新会话，restart recovery 的延迟预期需按此标注）。
- **streaming resume（官方文档未记载，实测可用）**：`--input-format stream-json --conversation <id>` 组合被接受——init 保持原 `conversation_id`，`step_index` 接续（实测 10→12），期间出现 `system_message` step。daemon 重启恢复**不需要降级到 print 单发**。
- 多轮：第二个 user 事件在首个 result 之后被消费（按 turn 投递），与 daemon 的 message-queue 模型匹配。

## 4. 终止与信号语义（pump 实现铁律）

- **mid-turn SIGINT/SIGTERM**：单个 result，`status=ERROR, error="interrupted"`，**response 保留截断的部分文本**，exit 1，stderr `error: interrupted`。daemon 的 cancel 路径可映射此 outcome 且不丢部分输出。未观测到 `CANCELED` status（应保留给未公开的 control 面）。
- **空闲等待期被信号打断**：同一会话产出**两个 result**——先 `SUCCESS`（已完成 turn 的真实结果），再 `ERROR`（`error="stream input cancelled: context canceled"`，流层面取消），exit 1。**违反"每 turn 恰好一个 result"**：终态判定必须以进程退出为准，不可见第一个 result 就结算。
- EOF（关 stdin）：当前 turn 完成后干净 exit 0。

## 5. model / effort admission

- `agy models` 列出 14 个 slug，**多供应商**（gemini-3.x-*/claude-sonnet-4-6/claude-opus-4-6-thinking/gpt-oss-120b-medium）；实测 `claude-sonnet-4-6` 路由成功（1.5s）。`agent_models` 有两个来源：该子命令，以及未知 slug 的 **stderr 错误信息自带完整目录**（`error: invalid model selection ... Available models: ...`）。
- 未知 slug：stderr 报错 + stdout 一个 `conversation_id:""` 的 ERROR result + **exit 1**（loud fail，符合 fail-closed 偏好）。
- `--effort` 合法集：`low|medium|high|max`（**含 max，比官方文档多一档**）；非法值 exit 1 并列明合法集。
- **model 与 effort 非正交**：`claude-sonnet-4-6` 拒绝 `--effort max`（`--effort is not supported for model`）；Gemini slug 自身内嵌 effort 档位（`gemini-3.8-flash-low`），slug+同值显式 effort 可共存。admission 不能像 codex 那样全局 closed_set，需 per-model 校验或 spawn 前 dry 校验。

## 6. 负面输入（streaming 模式）

| 输入 | 行为 | exit |
|---|---|---|
| `control_request` 事件 | ERROR result（"not supported **yet**"——control 面在路线图上）+ 会话终结 | 2 |
| slash 命令（`/model`） | ERROR result + 提示可用 `agy -p "/model"` 单发替代 | 2 |
| 坏 JSON / 空 prompt | ERROR result + 会话终结 | 1 |
| 未知事件名 | 跳过 + stderr `warning: ignoring unsupported stream input message event`，会话继续 | — |

## 7. 其他实测能力与陷阱

- `--json-schema`：完整可用，result 附带独立的 `structured_output` 对象（与 `response` 文本分离，schema 回显）。
- `--print-timeout` 默认 **0（无限等）**，非文档的 5m；超时触发时 result 是 **`status=SUCCESS` + 空 response + exit 0**，仅 stderr 提示 `print timeout ... returning partial output`——超时被伪装成成功，daemon **不要依赖该 flag**，应自管超时并走 §4 的信号路径。
- `--disable-slash-commands`：print 模式默认会展开 slash/skill（流模式禁用），host 注入的 prompt 若含 `/` 前缀文本需注意。
- help 面新增（相对文档）：`--mode accept-edits|plan`、`--add-dir`（多目录 workspace）、`--new-project`/`--project`、`--remote-control`、`--log-file` 覆盖、`-i/--prompt-interactive`。
- **无原生 ACP/app-server 模式**（1.2.12 无 `--experimental-acp` 类 flag）。
- `.agents/` workspace 配置仅在 trusted workspace 加载；本机 settings 将整个 `/Users/ibobby` 标记 trusted，意味着任意任务 workspace 默认 trusted——**任务仓库内容可影响 agy 运行时行为（MCP/plugin 注入面）**，接入设计需显式处理（收紧 trusted 集合或接受该面并文档化）。

## 8. 对照产品 MCP 面的结论（以实测为准）

**可直接实现**：spawn（prompt+cwd）；长驻多轮 send/queue_message；result 的 final_text+outcome 映射（SUCCESS/ERROR+error/interrupted）；wait 增量 text tail（text_delta）；tool activity（tool step 含 name/parameters/output，粒度超需求）；cancel 硬杀路径（`interrupted` result + 部分文本 + exit 1，进程组 TERM/KILL 语义匹配）；close/list/durable history；probe local（`--version`）；probe hi（any tool step 即 policy_violation，检测面完整）；auth 层无独立检查 → 按惯例 UNKNOWN；`agent_models`（`agy models`/错误目录）；yolo 权限；effort（closed set low/medium/high/max）；model 启动时选择+会话 sticky；**restart recovery resume（streaming + `--conversation` 原生可用）**；`denied_actions` 上报（respond 缺失的部分代偿——调用方至少能看到任务被拒了什么）；structured_output（超出项目现有面的免费加分）。

**降级/变通**：build/edit 权限（allow 规则注入 + `--mode accept-edits`；预授权 ≠ 拦截守卫）；plan 姿态（`--mode plan` + `deny` 兜底可构造，但语义是"强制拒绝"非"工具禁用"，需产品决策是否承认为 plan）；reasoning tail（无 thought 流 → 不采集，照 codex 先例）；resume 单轮延迟 53–58s（上下文重放成本）。

**真缺失**：permission request/respond 交互（流中无请求事件，soft-deny 自动决）；write-manifest 守卫（无 `FS_WRITE_MANIFEST_DENIED` 等价物，build 只能开放 legacy `["."]`）；运行时 set-model/set-effort/cancel 控制面（`control_request` 明示 not supported yet；slash 单发替代仅 print 模式）。

**实现铁律（pump/admission）**：同一 turn 可出现多个 result，终态以进程退出为准；`--print-timeout` 的 SUCCESS 不可信；`print-timeout` 默认无限等；未知事件名静默跳过意味着输入面宽松（注入侧仍应只发严格 user 事件）；stderr 是诊断与 soft-deny 通知的唯一通道，按 64 KiB cap 惯例收集。

## 9. 与官方文档的偏差清单

1. 默认 headless 姿态是全工具 soft-deny，非"文件 I/O 自动放行"（文档错误，实测推翻）。
2. `--effort` 含 `max` 共四档（文档只写三档）。
3. `--print-timeout` 默认 0（无限等），非 5m；且超时伪装 SUCCESS。
4. streaming + `--conversation` resume 可用（文档未记载）。
5. result 可附带 `denied_actions`、`structured_output` 字段（文档未记载）。
6. "每 turn 恰好一个 result" 在空闲期信号打断时不成立（双 result）。
7. 事件 payload 嵌套在 `step_update`/`result` 内层对象（文档的字段描述位置有歧义）。

## 10. 接入状态

> 本节由外围合同节（S03）追加；live smoke 证据待「集成与完成」阶段回填，当前仅登记实现面与已知缺口。

**注册与门控**：`agy` 已作为第 4 个 subagent 注册（`zcode`/`dsh`/`codex`/`agy`），传输为 `agy_stream_json`。生产 spawn 与顶层 `spawn_supported` 默认关闭，需要 `enabled + spawn_supported` 加绝对可执行的 `AGY_RUNTIME_PATH`；未满足时 status 如实报告 closed。

**admission 面**：permission 仅 `build`（`--mode accept-edits`）与 `yolo`（`--dangerously-skip-permissions`），`plan`/`edit` 在 admission 拒绝；任何非空 `write_manifest` 拒绝（`agy_write_manifest_unsupported`，只接受空 manifest）；effort 为闭集 `low | medium | high | max`；model 为裸 slug（无 `:`/`/`/空白，≤512B），优先级为显式值 > `agents.agy.default_model` > native，native 时不传 `--model`、由 `agy` CLI 采用自带默认模型。

**probe 面**：`local` 执行 `agy --version`；`auth` 固定 `auth_not_probed`（无独立认证检查）；`hi` 为只读 streaming（不跳过权限、bounded prompt、任何 tool step 即 `policy_violation`）；`models` 来自 `agy models` 的一次性目录发现（source `agy_models_list`，去重 ≤256）。

**已登记缺口（documented gaps）**：

- permission request/respond 交互缺失：流中无请求事件，工具被 soft-deny 自动决；被拒动作仅经失败/取消路径拼入 bounded 诊断尾部。
- 成功路径的 `denied_actions` 无公共投影面：`TaskResult` 无该字段、Completed 不携带 diagnostics，成功 turn 的 soft-deny 明细不上报。
- 无 write-manifest 守卫：无 `FS_WRITE_MANIFEST_DENIED` 等价物，故 admission 只接受空 manifest。
- 无 daemon 重启 resume：探测虽确认 streaming + `--conversation` 原生可用，适配器当前不实现重启恢复。
- 运行时 set-model/set-effort/cancel 控制面缺失（`control_request` 明示 not supported yet）。
- model×effort 非正交：admission 不做 per-model 校验，交给 `agy` CLI 在 session 启动时 loud-fail。

**live smoke 证据**（2026-09-28，集成阶段回填）：隔离 daemon（`target/release/external-subagentd` 以 `--database/--socket/--agent-config` 独立启动，未触碰生产 LaunchAgent）+ 本机真实 `agy`（探测时 1.2.12，smoke 时已自动升级 1.2.13，版本探测如实反映）完成五用例，原始响应存 `.agent-work/tmp/agy-subagent-live/0*.json`：

1. `agent_probe agy local` → `READY`，version 1.2.13，runtime_path 解析正确（01）。
2. `agent_models agy` → `supported:true`，14 个多供应商 slug，source `agy_models_list`（02）。
3. build spawn（`gemini-3.8-flash-low`，映射 `--mode accept-edits`）→ 真实 agy 长驻进程完成文件写：outcome `COMPLETED`、final_text `DONE`、workspace 内 `smoke-build.txt` 内容 `AGY_BUILD_OK`（03/04）。
4. yolo spawn（`--dangerously-skip-permissions`，prompt 要求以 `run_command` 执行 echo）→ outcome `COMPLETED`、final_text `done`、**`tool_calls_last_60s: 1`**（Detailed 词汇对真实 tool step 的计数验证，05/06）。
5. plan 模式 spawn → admission fail-closed 拒绝：`agent_unsupported / AGY_PERMISSION_MODE_UNSUPPORTED`（07）。

## send 双模式投递

`external_subagent_send` 的 mode 必填，仅接受 queue/steer；缺失或非法值以 validation 拒绝。

活跃 queue 向 stdin 直写 user 事件，不 begin_turn，由原生缓冲按序成为后续 turn。daemon 记录已接收、尚未消费的输入，下一 user_input 或递增 num_turns 的 result 开启并结算对应 turn；首轮 result 与后续输入消费之间保持 RUNNING，monitor 不回收 runtime。无待消费输入时的空闲重复 result 仍视为噪声。空闲 queue 沿用 send_turn；steer 返回 steer_unsupported，不使用破坏性的信号中断。

接入新 subagent 时原生 mid-turn 投递优先于 es 暂存，es 暂存是文档化的标准兜底。queued 仅表示 es 暂存；原生直写／注入完成即 delivered，执行结果仍由 wait/result 查询。同 message_id、同 mode、同 content 重试幂等，改变任一绑定字段会冲突。
