# external-subagent 调研与分析报告

研究日期：2026-09-10，America/Edmonton。范围为上传源码、SFD4.5.1 和当前官方公开接口；没有使用用户机器凭据运行实际 agent。引用索引见文末；本报告中的架构选择均是建议，不是上游保证。

## 1. 结论

**不建议 fork DeepSeek Harness 本体，也不建议 fork GUI／Codex-backend 插件来暴露 DSH。首选官方 ACP server，并在 external-subagent 内实现薄 DSH adapter。** DSH 已有 `acp`、`sdk`、`headless` 等入口；ACP 是针对外部程序控制 agent 的完整路径，SDK 的简单消息接口不是等价替代。[R01][R02][R03]

需要自行维护的是 provider adapter、受管启动 profile 和本产品的生命周期投影。只有已批准的权限保证无法用官方组合可靠表达时，才增加小型 DSH 策略插件；不再造 session store、request queue 或第二个 daemon。

当前研究所读源码的 CLI manifest 声明 `@deepseek-ai/dsh` **0.1.5-rc.2**；根 manifest 声明 Node `^22.19.0 || >=24.0.0`。这是 **master 源码状态，不是已核实的 npm latest，也不是用户本机版本**。实际 S01 必须固定可安装版本／来源与真实命令输出再验证，不能把 moving master 的新能力假定为任何旧 DSH 都具备。[R04][R05]

## 2. 官方 app-server／CLI 能力比较

| 路径 | 官方可用用途 | 对本项目的关键限制 | 建议 |
|---|---|---|---|
| `dsh --profile acp` | JSON-RPC stdio 自动化、独立 session、权限回调、模型配置 | 不提供全部 Web UI 语义；必须做版本与能力验证 | **生产主路径** |
| `dsh --profile sdk` / sdk-minimal | 简单 SDK 对话与消息事件 | 协议只有 initialize、session/prompt、shutdown；不能完整替代 per-session cancel/close/approval | 不作为透明 fallback |
| `dsh --profile headless "..."` | 一次任务、最终输出、进程退出 | 不适合保持交互式 respond/send 生命周期 | 可作辅助对照，不作正式 hi 的唯一证据 |
| `dsh web` | 人工 Web UI | 增加 web/browser/session presentation 依赖 | 不抓取 UI 或逆向其私有接口作为首选 |
| 自写 Cordis RPC server | 能自行扩展 DSH | 重做已有协议、兼容／维护面扩大 | 只有明确官方缺口且用户坚持新语义时考虑 |

官方 CLI 支持独立 profile、从 shipped template 初始化自定义 profile、启动时 patch 和 plugin 管理。`dsh plugin --profile <name> ...` 将操作交给 profile 的 pnpm 环境；bundle membership 改动需要重启对应 profile。probe 不应顺手触发插件安装或修改默认 Web profile。[R06]

### 2.1 生命周期适配映射

下表是计划中的映射，不是已完成实现。方法语义以官方 server 源码与实测为准。[R07][R08]

| external-subagent 操作 | DSH ACP 路径 | 本产品还须负责 |
|---|---|---|
| spawn | initialize → session/new → 可选 session/set_config_option → session/prompt | workspace admission、配置固定、daemon task identity、失败不发布伪成功 |
| wait | 消费 session/update 与 correlated prompt settlement | 沿用本地 wait 谓词，不把每个 update 返回给主模型 |
| send | 原生每 session 只允许一个 in-flight prompt | 沿用本产品 durable queue，在安全 turn boundary 派发下一消息 |
| respond | 回答 session/request_permission | request_id／generation／toolCallId 关联；仅单次 allow/deny；迟到拒绝 |
| cancel | session/cancel 或关联请求取消 | 本地取消优先、队列收敛、必要进程回收事实 |
| close | session/close，最后处理进程 EOF／退出 | 本 task 资源释放；不把 close 当删除原生历史 |
| result | 由已完成 prompt 的 message chunks 聚合 | UTF-8／frame 上限、durable 保存、结果关联与分页 |
| observe | 公开的 thought/tool updates | 有界投影、缺失能力标记、不读私有推理字段 |

一个 DSH 连接可以承载多个 session，但 v1 推荐一外部 task 一个进程，先用已成熟的进程隔离与回收模型。池化带来的资源优势尚未在用户环境测量，不应先承担共享模型／权限／故障域的复杂性。

### 2.2 模型选择：不能只给 CLI 拼一个 --model

ACP session 返回 model config choices，选择通过 `session/set_config_option` 完成；模型值是 catalog token，适配器应使用上游给出的值，不在通用层解析其内部编码。源码把模型选择固定到已 admission 的整个 prompt turn。[R09]

因此本产品优先级应为 request model → agent-local default_model → provider default。设置默认值不能改变现有 task。ZCode 收到 model 要明确报不支持，不能为了统一界面改其全局配置。`agents models` 是值得增加的 CLI：让调用方获取实际可用 token，而不是把研究时的模型名写死。

### 2.3 “登录检测”必须拆开

官方 ACP `authenticate` 实现直接成功，并且 advertise 的 authMethods 为空；这只表示 ACP server 本身不要求客户端认证，**不能证明 DeepSeek/provider key 有效**。[R07]

官方 credential 路径包括运行环境和原生 DSH home／project 配置来源；探测还受当前 workspace 的环境影响。读取到 credential 配置只能证明“存在”，真实 hi 才能验证该具体路由此时能发起模型请求。[R06]

建议状态同时表示 installed、transport、auth、last_hi，不给一个欺骗性的 available 布尔值。401、网络超时、限流、余额、模型不可用分别分类；在没有权威证据时为 unknown。daemon 的环境与终端环境不一致时，以真实生产 daemon 路径探测为准。

## 3. 权限：真正可能需要自写插件的地方

DSH sandbox 的 read-only/workspace-write 是**文件效果策略**，不是“是否允许启动 Bash”策略；network/process policy 也不属于这个词汇。官方 ACP 不暴露 session modes／elicitation。不能把 ZCode 的 plan/edit/yolo 枚举原封不动发给 ACP，再声明相同安全性。[R02][R10]

受管 profile 可以配置 sandbox 与工具组合，且最后的 invocation patch 有自己的明确优先级；但 patch 会替换整行 config，不是深合并。使用新 profile 不代表隔离了 home-level 设置与凭据。受管配置必须验证覆盖后的实际组合，而不是只看模板文件。[R06][R11]

建议分两层：

**硬约束**：严格 plan 不允许写仓库，也不允许通过 Bash、terminal、jobs、可执行代码工具、嵌套 subagent／MCP 绕过。用户 allow 回复不能升级掉硬约束。精确 write_manifest 与 root-level workspace-write 不是同一能力。

**provider 能力**：ZCode 原有模式保持；DSH 先证明 build 与 strict plan。不能证明的 edit/yolo／精确 manifest 在 admission 拒绝，不以更宽模式替代。若用户要求首发完全一致，则小型策略插件是必做项，而不只是可选优化。

### 自写插件的合理范围

| 可以做 | 不应该做 |
|---|---|
| 在官方工具／权限接缝执行已批准的 mode 与 scope | 自己重做 spawn/wait/result/close |
| 暴露必要、明确缺失且已批准的 metadata | 复制 DSH 的 session persistence |
| 验证受管组合的实际能力，拒绝未覆盖执行入口 | 另起管理 daemon／再建立 task queue |
| 与特定实测 DSH 版本绑定并做 conformance fixture | 直接依赖大量 Web 内部数据实现完整 GUI parity |

S01 要给出“官方组合足够”或“具体入口存在可复现缺口”的证据。后一种再做窄范围 PLAN delta；不因为“万物皆插件”就预先写一个大 plugin。

## 4. 第三方插件调查与取舍

| 候选 | 实际方向／作用 | 可借鉴内容 | 是否适合直接 fork |
|---|---|---|---|
| 官方 `@deepseek-ai/dsh-acp` | **外部程序 → DSH agent** | 正式服务端协议，最匹配需求 | 直接使用，不 fork |
| 官方 `@deepseek-ai/dsh-subagent-acp` | **DSH → 外部 ACP 子进程** | subprocess admission/cleanup、stop reason、隔离与safe diagnostic | 参考客户端设计；它不是向 Codex 暴露 DSH 的产品插件 |
| `yangbobo2021/relay-dsh-plugin-codex` | 在 DSH Web 中使用 Codex app-server | 身份关联、审批代际、安装与版本固定的经验 | **方向相反**；fork 会带入不需要的 Codex-backend/UI 语义 |
| `anywhere-labs/dsh-desktop` | DSH 插件生态的桌面包装 | 桌面分发／运行环境管理思路 | 不是本任务需要的独立会话自动化层 |

来源：[R12][R13][R14]。以上是实际核查到的候选，不宣称穷尽社区，也不把仓库 README 的 CI 成功声明当本次实测。当前没有找到比官方 ACP 更匹配本需求、值得为生命周期而 fork 的第三方层。

官方 ACP client 的某段限制说明称只收集文本，而当前 server `updates.ts` 已明确生成 thought 与 tool updates；这提示**文档与源码的具体口径存在差异**。应以选定发布版本的 wire capture 验证，不根据其中一句文档宣布所有版本都没有推理／工具信息。[R12][R15]

## 5. ACP 仍不等于“所有原生信息都无损”

### 5.1 正确处理最终文本与公开推理

server 的 update projection 为 assistant 块携带 messageId，thought 与 message 分开；工具开始、结果使用 toolCallId 关联。adapter 可据此避免把中间进度拼成最终答复；200 字符 observe 应明确其来自 **committed thoughts**，不是低延迟 provider reasoning delta。[R15]

这也意味着“等待期间没有新 thought”不能证明模型卡住。主模型仍依据 bounded observe 自行判断，不让产品自动取消。真实最新消息／工具参数只按已公开字段采集，始终排除 encrypted_content。

### 5.2 Stop reason 有语义折叠

官方 codec 将部分 aborted/blocked 路径映射为 end_turn；session owner 又对已识别的 error 做单独失败处理。**end_turn 不能充当业务任务验收通过证据**。本产品应保留上游 stop reason 和明确 completion evidence，不凭空还原没有在 wire 暴露的内部原因。[R08][R16]

若用户需要与 ZCode 完全相同的细粒度失败分类，则要验证官方是否已有别的公开事件；没有时才讨论窄元数据插件，而不是 heuristic 解析自然语言来推测失败。

### 5.3 原生 resume 与产品 resume 分开

DSH 原生有持久会话 list/resume/close，但不 replay 历史 update；本次没有验证跨进程恢复时外部队列与 prompt 去重。现有 ZAS 已明确终态不能继续 send。建议 v1 保持这一边界，禁止不确定 admission 的自动重试，避免“重连成功”导致代码操作执行两次。[R02]

## 6. 现有仓库值得复用什么

值得复用的是 durable task/request/message/result 模型、wait/result 有界输出、回收事实、workspace admission、协议错误、MCP facade 与安装 ownership 思路。不是把所有代码换前缀，也不是重新实现已经调通的 ZCode app-server。

源码细查有三个重要修正：

1. 最新 wait 规则是所有 respondable+PENDING，不限 Bash；100 条之外不保证提前唤醒。
2. 当前 RPC request512KiB／response2MiB；result 单页256KiB；wait 在容许帧内直接给完整结果。
3. ZIP 工作区漏了40个 tracked测试，内含Git对象可恢复；97个现存tracked文件与当前feature HEAD一致。

这三点都已经写入需求与计划，不以旧记忆或旧报告代替当前源码。具体证据、行号、hash 和文件规模见 `SOURCE-EVIDENCE.md`。

## 7. npm、PATH 与 Codex plugin 更新

npm 全局安装会把 bin 链接到 Unix prefix/bin；该目录仍需在调用环境 PATH 中。不能因为终端 `which` 成功，就假定 launchd／GUI 成功。建议插件绑定绝对稳定 MCP 入口，daemon 使用已验证的版本化 payload 路径。[R17]

当前 OpenAI 文档同时说明 portable root manifest 与仍支持的 `.codex-plugin` fallback；本机安装版本可能不同。安装器应先探测支持的官方命令与格式，不假定旧仓库的 `codex plugin add --json` 永远是唯一接口。官方现有 marketplace 管理提供 add/list/upgrade/remove；本地 plugin 还涉及安装 cache，并非只复制源目录。[R18]

因此“自动更新 plugin”必须做真实调用方验收，而不是只更新 npm 包里的 `skills/`。建议登记多 CODEX_HOME、marketplace identity、managed source、安装副本版本，并保留 enable/disable 选择；修改官方不归本产品管理的文件时报告冲突，不静默覆盖。

官方文档要求某些客户端重启后拾取本地更新；本产品不能承诺所有运行中会话热重载。**安装副本同步**与**当前宿主已加载**分开显示，后者没有证据就 unknown/reload_required；不强退用户 Codex。[R18]

更新流程提案：候选包完整校验 → 记录目标版本 → 已有daemon排空 → 一次性updater切换受管入口／服务 → 验证daemon → 官方路径刷新已登记plugin → 分项确认。失败只对自己管理的文件做补偿，不能把跨 npm、服务管理器和 Codex cache 的过程包装成不存在的全局事务。

npm ignore-scripts 会阻止 package scripts；Codex 的 npm plugin source 也有不运行 lifecycle scripts 的安装语义。因此既要有普通安装hook，也必须有显式update／下一次CLI版本协调的恢复路径；脚本被禁用时准确报告未激活，不能假称已经重启。[R19][R18]

## 8. 先验证的最小问题与判定标准

S01 在受控 fixture 与授权环境记录：实际 executable/version、ACP版本与capabilities、native model catalog、model选择先于prompt、错误凭据与正常hi、tool/permission关联、cancel与close退出、messageId/text/thought语义、严格plan对所有启用入口的实际拒绝、stderr不混stdout、未知/超大帧与stdin阻塞。

只有成功才进入正式DSH产品化；S01遇到环境缺失先作一次有界诊断，不能因没有装DSH就声称必须fork。找不到公开能力时保持UNKNOWN或明确unsupported，按决定表处理。性能与多进程资源成本没有本次实测，不在报告里编造吞吐／延迟。

## 9. 一手来源索引

以下为本次实际读取的页面／源码；GitHub master 路径是移动来源。实施时记录发布版或 immutable commit，不能把此表当版本锁文件。

| ID | 来源 |
|---|---|
| R01 | [DSH 官方架构](https://raw.githubusercontent.com/deepseek-ai/deepseek-harness/refs/heads/master/docs/architecture.md) |
| R02 | [官方 ACP server reference](https://raw.githubusercontent.com/deepseek-ai/deepseek-harness/master/packages/acp/acp/README.md) |
| R03 | [官方 SDK protocol](https://raw.githubusercontent.com/deepseek-ai/deepseek-harness/master/packages/sdk/protocol/README.md) |
| R04 | [官方 CLI package manifest](https://raw.githubusercontent.com/deepseek-ai/deepseek-harness/master/apps/cli/package.json) |
| R05 | [官方根 package manifest](https://raw.githubusercontent.com/deepseek-ai/deepseek-harness/master/package.json) |
| R06 | [官方 CLI behavior reference](https://raw.githubusercontent.com/deepseek-ai/deepseek-harness/master/apps/cli/reference/README.md) |
| R07 | [ACP server index.ts](https://raw.githubusercontent.com/deepseek-ai/deepseek-harness/master/packages/acp/acp/src/index.ts) |
| R08 | [ACP session.ts](https://raw.githubusercontent.com/deepseek-ai/deepseek-harness/master/packages/acp/acp/src/session.ts) |
| R09 | [ACP model-control.ts](https://raw.githubusercontent.com/deepseek-ai/deepseek-harness/master/packages/acp/acp/src/model-control.ts) |
| R10 | [官方 sandbox subsystem](https://raw.githubusercontent.com/deepseek-ai/deepseek-harness/master/docs/subsystems/sandbox.md) |
| R11 | [官方 base bundle reference](https://raw.githubusercontent.com/deepseek-ai/deepseek-harness/master/packages/bundle/base/README.md) |
| R12 | [官方 ACP subagent client](https://raw.githubusercontent.com/deepseek-ai/deepseek-harness/master/packages/subagent/subagent-acp/README.md) |
| R13 | [relay-dsh-plugin-codex](https://github.com/yangbobo2021/relay-dsh-plugin-codex) |
| R14 | [dsh-desktop](https://github.com/anywhere-labs/dsh-desktop) |
| R15 | [ACP updates.ts](https://raw.githubusercontent.com/deepseek-ai/deepseek-harness/master/packages/acp/acp/src/updates.ts) |
| R16 | [ACP codec.ts](https://raw.githubusercontent.com/deepseek-ai/deepseek-harness/master/packages/acp/acp/src/codec.ts) |
| R17 | [npm folders / executables](https://docs.npmjs.com/cli/v11/configuring-npm/folders/) |
| R18 | [OpenAI plugin packaging / marketplace / updates](https://developers.openai.com/plugins/build/plugins) |
| R19 | [npm ignore-scripts configuration](https://docs.npmjs.com/cli/v11/using-npm/config/#ignore-scripts) |
