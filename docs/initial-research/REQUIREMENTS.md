# Requirements Contract — external-subagent

- Feature: `external-subagent-v1-20260910`
- Status: **DRAFT — U 已明确；B 为当前源码继承边界；P 待采纳**。
- 原始权威：`ORIGINAL-REQUEST.md`；代码证据：`SOURCE-EVIDENCE.md`；提议选择：`DECISIONS.md`。
- 本文不是实施／安装／发布授权。涉及网络 hi、用户配置、服务启停、registry 发布须在实际执行时具备相应授权。
- Source snapshot：`bb45d562671ddbd99637c5680449bc75aedb378b`，分支 `codex/wait-respondable-20260910`。不是旧 main，也不是记忆中的 Bash-only wait 版本。

## 1. 目标与成功条件

U01. 建立独立的 `external-subagent` 项目，复用当前 ZAS 的外部会话生命周期管理，通过 CLI 与 MCP 暴露；首批 agent 为 ZCode 与 DeepSeek Harness。

U02. 接入设计可扩展，不把未来 agent 写成对两个现有 provider 的散落特殊判断；文件结构须防止当前超大 owner 再次形成。

U03. CLI 能检查本地 agent 是否存在、认证的已知状态，并能显式发送实际 `hi` 探测；支持 agent 配置、可缺省的默认 agent、可缺省的默认模型。

U04. spawn 可指定 agent/model；没有默认 agent 且请求也未指定时必须报错，不能选“第一个可用的”。ZCode 不支持指定模型；模型未配置则由被选 agent 自己决定。

U05. 状态同时展示各 agent 的可用性，不能只有 daemon 在线／离线。

U06. 经 npm 分发，公开可执行入口进入常用 PATH；更新联动更新程序、重启 daemon、同步已配置的调用方 plugin，初期只支持 Codex。

完成的最小定义：在批准平台、固定实测 provider/Codex 版本上，两种 agent 均能通过真实 CLI 与真实 MCP 客户端走完 spawn → wait → 需要时 respond/send → 终态结果 → close；故障／取消正确回收；配置与更新有真实安装验收；不能只凭 fake runtime 宣布支持。

## 2. 当前必须继承的行为（B）

B01. 保留十种生命周期／诊断能力：status、spawn、wait、list、observe、send、respond、cancel、result、close。新名字提议为 `external_subagent_*`，不存在 `poll`／`timeout_ms` 回归。

B02. wait 的 `wait_time` 为整数秒，默认 290，0 为立即快照，最大 299。普通文本、推理与工具活动不提前返回。只在任务终态，或**返回投影中存在 respondable 且 PENDING 的请求**时提前返回；期限到返回当前状态且不变更任务。`command_pending_approval` 使用同一个谓词。非 respondable、SENDING、RESPONDED 不独立唤醒。

B03. pending 投影仍最多 100 条，保留有序、有限的边界。投影以外的请求不保证提前唤醒；不新增无界扫描。来源是上传仓库最近的用户修正，已取代 Bash-only 规则。

B04. 结果页按 UTF-8 **字节**计，默认／上限 262144；不能切断字符。当前内部 RPC 请求 frame 上限 512 KiB、响应 frame 上限 2 MiB。终态 wait 优先内联完整结果；完整编码超限才退为有 `complete=false`、`next_offset` 的第一页，再超限返回明确 oversized。不得把“每个 result 页最多 256 KiB”误改为“wait 永远只能返回 256 KiB”，也不能把无限完整结果塞进固定帧。

B05. 同一 canonical workspace 在本产品内仅一个活跃 task；跨 agent 也受同一 admission owner 控制。进程退出、协议终态、结果持久化与 resources_reaped 是不同事实，不能混为一谈。

B06. send 延续当前 queue 语义与 message_id 重放／冲突检查；不变成按活动自动 interrupt。终态任务不支持续聊；失败后不能把原来的 completed result 冒充新请求结果。

B07. respond 只处理本 session 当前 generation 的已知可响应请求，保留 allow/deny、重复请求与 in-flight 处理；未知／已过期／非可响应请求拒绝。MCP 不偷偷扩展成任意 answer/elicitation。

B08. observe 是调用方怀疑循环时使用的有限证据，不自动判循环、不自动 cancel；工具 top 3 × 各最近最多 5 次调用，不含结果；推理尾部最多 200 Unicode 字符。只采集公开字段，不读取 encrypted_content。DSH 的 committed thoughts 与 ZCode 的公开 delta 必须标记不同来源／支持度。

B09. 保留 ZCode 已有工作区、permission_mode 与 write_manifest 合同；plan 不能降为只在 prompt 里写“请只读”。ZCode runtime、模型目录 workaround、登录信息、hook 行为留在 ZCode 适配器内。Hook 安装仍是显式管理动作；不可恢复旧的全局 hook 决策架构。

B10. CLI/MCP 错误使用结构化 code，并保持受限可读文本；结果、日志、诊断不泄露凭据。configured/requested/resolved/observed 不同事实分别记录，缺证据填 unknown。

## 3. 新公共协议提案（P；D01、D03—D06）

### 3.1 名称与身份

- npm 包的逻辑产品名／canonical CLI：`external-subagent`；独立 MCP 可执行入口：`external-subagent-mcp`；daemon：`external-subagentd`。实际 npm scope 由发布权限确定。
- `agent`：提供方路由键，例如 `zcode`、`deepseek-harness`，可扩展字符串，不是任务 ID。
- `agent_id`：沿用公共 MCP 的八位数字任务 ID；内部 durable ID 与 upstream session ID 不冒充这个 ID。CLI 新友好命令使用同一公共 DTO；旧 raw RPC envelope 不作为旧产品兼容承诺。
- session 一旦 admission，固定 agent、adapter 版本、配置 revision、workspace 与模型选择来源。配置改动只作用新 task。

### 3.2 spawn 的行为

在已有 repository、permission_mode、prompt、write_manifest 输入上增加可省略的 `agent`、`model`。空字符串／显式 null 拒绝，不混同于省略。

agent 解析：请求值 → 配置 default_agent → `agent_required`。不做可用性 fallback。

model 解析：请求值 → 被选 agent 的 default_model → 不覆盖 provider 原生选择。**不设跨 agent 全局 default_model**。DSH 的 model 参数优先使用 `agents models` 返回的 catalog token；核心不解析 token 编码、硬编码模型清单或自行猜 provider。无效／消失／歧义选择在发送 prompt 前报 `model_unavailable`。

ZCode 收到任何显式 model，或 config 设置 ZCode default_model，报 `model_selection_unsupported`；不忽略、不改全局模型、不临时覆盖账号设置。

示例为 PLANNED，不代表接口已实现：

```json
{"repository":"/absolute/workspace","agent":"deepseek-harness","model":"<catalog token>","permission_mode":"build","prompt":"执行已批准的任务"}
```

```json
{"repository":"/absolute/workspace","agent":"zcode","permission_mode":"plan","prompt":"只读审查"}
```

第二例不含 model。没有默认 agent 时省略 agent 必须失败；设置 zcode 默认后省略 agent 可成功选择 ZCode。

### 3.3 状态与探测

status 返回 daemon/facade/install identity 和 `agents[]`；每项至少含路由键、configured/enabled、executable 的检测结果、runtime version、transport support、auth 状态、支持的 permission modes、model-selection 能力、最近 hi 结果及时间、阻塞原因。任务列表可按 agent 过滤，task 输出携带路由键。

认证状态至少区分 `unknown`、`missing`、`valid`、`invalid`；网络／限流／余额／policy 错误独立，不一概当作登录失效。valid 只表示特定 route/home/workspace/config revision 在标明时间有可支持的有效证据；不是永不过期承诺。

三个探测层分开：
1. local：路径、可执行性、版本、已知配置来源；无模型调用。
2. protocol/auth：显式启动协议检查；认证真实性仅按 provider 有效信号判断。DSH ACP authenticate 返回成功不作为 API 凭据有效证据；models catalog 能读也不代表 hi 可执行。
3. hi：通过**生产 daemon、选定 agent 路由与原生 transport/model 选择**发出真实请求，失败保留阶段与安全错误。默认使用受限临时 workspace，认证证据仅对该 scope 有效，不外推到带不同 `.env` 的用户仓库。显式 `--workspace` 才使用指定 workspace 的原生配置／环境，须有读取该配置的授权与先验可执行的无工具或严格只读策略，不允许探测改写仓库。生产 daemon 环境不同于交互 shell时按生产环境判定；不能拿独立 headless/shell hi 冒充同路由有效。

status 不为每个 agent 自动付费 hi，也不读取并输出 secret。MCP 不新增安装、登录或升级工具；这些管理行为留 CLI。

### 3.4 CLI 命令面（拟定）

```text
external-subagent agents list
external-subagent agents probe <agent> [--auth | --hi] [--workspace <dir>]
external-subagent agents models <agent>
external-subagent config show
external-subagent config set agents.<agent>.<key> <value>
external-subagent config unset <key>
external-subagent config set default_agent <agent>
external-subagent spawn --agent <agent> [--model <catalog-token>] --repository <dir> --prompt <text>
external-subagent wait|list|observe|send|respond|cancel|result|close ...
external-subagent status
external-subagent daemon start|stop|restart|status
external-subagent init [--codex-home <dir>] [--probe-hi <agent>]
external-subagent plugin codex install|status|update|uninstall [--codex-home <dir>]
external-subagent update [--wait <seconds>] [--cancel-active --yes]
external-subagent diagnose|backup|restore|uninstall|purge ...
```

所有公共 task 命令须有等价 JSON 输入；友好 flags 与 JSON 不得产生不同默认值。agent executable 用 executable+argv 数组，不执行拼接 shell 字符串。配置删除被 active task 使用时不修改该 task 已固定的 launch facts；新 spawn 看到禁用／缺失即拒绝。

## 4. DSH 适配合同（D03、D04、D06、D10）

P01. 首选官方 ACP stdio；ZCode app-server 保持其独立协议。SDK/headless/Web/Tauri 不作静默替代。每个外部 task 一个受管理子进程；只有 daemon/runtime owner 负责产品进程回收。

P02. DSH initialize → new session → 校验能力／设置可选 model → prompt。同一 session 仅一个 in-flight prompt；队列消息等待当前 prompt 的已验证 settlement，再进入下一轮。读取 stdio 不得在等待 prompt response 时停止，权限回调必须能并行到达并等待调用方响应。

P03. permission request 持久化公共 request_id 与私有 upstream correlation/generation，再进入 PENDING 投影。只回答原生 offer 中的单次 allow/reject；不构造 allow-always、不把 unknown 当 allow。请求省略工具内容时可按已收到的同 toolCallId 事件关联；关联不到就标 unknown，不猜命令。cancel／close 后的迟到 respond 无效。

P04. DSH 原生 ACP 没有 mode／elicitation 接口。plan 权限用已验证受管组合实现，而不是调用不存在的 ACP mode 方法。**read-only 不等于禁止 Bash**。严格 plan 必须覆盖 fs write/edit、shell、终端、jobs、PTC/Cordis code、嵌套 agent/MCP 等实际启用入口，未知执行入口 fail closed。不能仅隐藏工具说明；须验证不可通过底层 dispatch 绕过。

P05. DSH v1 必需 build（workspace-write＋可响应权限）与严格 plan；未证明的 edit/yolo、非空精确 write_manifest 显式拒绝，并反映在能力状态。ZCode 的这些既有能力不削减。所有 unsupported 必须在发出任务 prompt 前被发现；严格 plan 不通过则不发布 DSH 支持。

P06. 文本／thought／tool update 分开处理。最终文本按本次 prompt 的已验证 messageId 分组，在协议 settlement 后选最后有文本的 committed assistant message；完整保留该消息各文本块顺序，不拼入前面的进度、thought 或工具结果。没有可辨认 messageId 的上游版本不得冒充此保证，须限制版本或另作明确选择。上游 end_turn 不声称“代码已通过验收”，被折叠的内部 aborted/blocked 不凭空还原；结果注明 upstream stop reason。max_tokens、refusal、wire error、transport loss 与本地取消各有明确 reason，不统一映射为成功。

P07. cancel 后取消入队／请求投递，排空必要更新后 close，EOF/TERM/KILL 有界升级并验证本进程管理范围回收。timeout／坏帧／stdin 阻塞／权限响应阻塞都不能无限等；使用原有 absolute deadline 思路覆盖锁获取、write/flush 与响应等待。发生不确定 prompt admission 不自动重放。

P08. DSH 原生 session persistence 与本产品 durable store 各自保留；不直接解析或修改用户 DSH 私有会话文件，不代替官方 resume。普通 close 不等于删除原生历史。v1 不提供跨重启自动续聊，也不自动清理用户原生历史。

## 5. 安装、更新和数据边界（D01、D02、D07、D08）

P09. npm `bin` 提供 CLI/MCP 入口；Unix 的 npm 全局 bin 路径和 GUI/launchd 的环境不同，安装验收必须分别验证。为受管 plugin 写绝对稳定入口，不能假定 GUI 继承终端 PATH。不能 sudo 写系统目录或未授权修改 shell 启动文件；无法确保 PATH 时明确给出检查结果，显式 repair 才写用户路径。

P10. own executable/native payload 可随 npm 更新；ZCode.app、DSH、Codex 本体以及其账号／凭据不在自动升级范围。native payload 在发布时构建／校验，支持平台安装不能依赖用户装 Rust。

P11. 正常 npm 安装脚本开启时，已初始化机器的更新自动进入同一个 reconcile 路径；显式 `update` 也复用该 owner。npm `--ignore-scripts` 或宿主不运行脚本时，无法保证安装时重启，必须显示 pending 并在下次 CLI 启动执行版本协调／提示，不伪称零例外自动更新。

P12. 原子性仅限已知本地文件；升级阶段至少区分 staged、draining、activating、healthy、partial/failed。并发 upgrade/init 用一个安装锁；daemon 的任务生命周期没有第二 owner。候选版本不完整时不切 active；活跃任务默认安全排空；draining 拒绝新 spawn／新 send，但已接收入队消息仍按原合同处理，wait/respond/cancel/result/close 保持可用。只显式强制命令可取消活跃任务。进程实际运行版本、安装包版本、plugin 安装副本版本均须验证。

P13. 只更新已登记的本产品 Codex plugin/MCP 绑定；保留其他 TOML/JSON/marketplace 项与 enabled 状态。源目录相同名字不等于所有权，漂移／手改冲突不覆盖。调用本机已验证的官方 Codex 安装／marketplace refresh 能力，不盲改 cache。多个 CODEX_HOME 分项显示成功／失败；某一项失败不能汇总成全成功。

P14. 候选 daemon 启动失败时尽量恢复已知旧 payload／service 配置。数据库不满足 downgrade 合同时停止并保留数据，不宣称已回滚。plugin 同步失败做受管文件的有界补偿并报告 partial，不能修改用户会话来凑“原子成功”。自动更新安装副本不等于正在运行的 Codex 已热重载；如需要，返回 reload_required。

## 6. 文件、测试与维护

P15. 采用 `FILE-ARCHITECTURE.md` 的 owner 图；新增 provider 的典型改动范围是该 adapter crate、composition root 注册、能力/协议 fixture 与文档，不需改所有 RPC handlers。使用同一 conformance suite 验证一个 test-only 第三 adapter，证明不是二选一特判。

P16. 保留／恢复原源码的回归测试。附件工作树缺少的 40 个 tracked 测试由内含 Git 对象可恢复；先按固定 HEAD 验证再移入新项目。不能删除测试来使重命名通过；历史 `.agent-work`、运行日志、密钥与旧 Git 工作状态不作为新产品内容拷贝。

P17. 实际接受证据覆盖 fake transport、生产序列化边界、真正两种 runtime、本地 npm pack 安装／升级和真正 Codex 工具发现。未运行项明确 NOT_RUN。结构化 PLAN validator 不等于独立 PLAN review 或产品验收。

## 7. 明确非目标

不做 GUI、远程多租户、动态插件市场、自动切换 provider/model、并行 workspace 写入、开发 worktree 管理、云端账号同步、provider 自动安装／升级、任意第三方 MCP 注入、绕过原生认证、原生终态续聊或 DSH UI 全功能复刻。没有“为可扩展性”新增调度服务、通用事件总线或第二数据库。

## 8. 验收追踪

| 要求／反例 | 责任 section | 可判错检查 |
|---|---|---|
| U01/U02，新项目不丢 ZCode 生命周期 | S02 | 老测试复原＋新命名空间真实 MCP/CLI lifecycle；第三 adapter 接缝由 S04 联合验证 |
| U03/U05，安装／认证／hi 分层 | S01/S03 | 缺 executable、坏版本、缺 key、握手成功但401、限流、合法 hi、daemon env 差异 |
| U04，没有默认 agent；ZCode 禁 model | S03/S04 | flags/JSON/MCP 同形 fixture，非法输入未发 prompt、配置修改不影响已有 task |
| B02/B03，非 Bash 可响应请求；100上限 | S02/S04 | Read/other PENDING 提前醒；SENDING/非响应不醒；第101条边界明确 |
| B04，Unicode／编码膨胀／完整 wait | S02/S04 | 262144 字节分页、2MiB frame、控制字符转义、完整内联与分页回退 |
| B05/B06/B07，取消／队列／请求关联 | S02/S04 | 双 agent 同 workspace 互斥、发送重放冲突、迟到 respond、不确定投递不重放 |
| B08/P04/P05，observe与严格plan | S01/S04 | 公开thought字段、200字符；所有已启用写入／执行入口拒绝与越权回复测试 |
| U06/P09—P14，完整npm升级 | S05/S06 | 全新prefix、GUI PATH、多home、忙任务排空、启动失败／plugin冲突／ignore-scripts |
| U02/P15，文件结构 | S02—S06 | owner依赖、文件规模清单、入口薄化、独立测试；不复制巨大旧lib |
