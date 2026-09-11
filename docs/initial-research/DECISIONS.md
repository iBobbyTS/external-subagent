# external-subagent — 决策清单

状态：**建议，尚未经用户批准**。固定要求见 `REQUIREMENTS.md` 的 U 系列；下列推荐是本计划采用的单一设计假设，不是伪造的 Owner approval。实际执行前，主控须把用户的采纳／修改记录在本文件；只重审受影响的未实施范围。

## 推荐方案总览

保留 Rust daemon / durable store / MCP 与 Node CLI 的成熟路径；把 ZCode 变成一个适配器。DSH 首选官方 `dsh --profile acp`，不使用 SDK 的有限会话协议来模拟完整控制，也不 fork GUI 插件充当服务端。扩展点是内部 `AgentAdapter`，不是新的插件市场。生命周期、权限请求关联、结果分页和工作区独占仍各有一个权威 owner。

## 必须定下来的产品／部署选择

| ID | 需要决定什么 | 本计划推荐值 | 不接受时的影响 |
|---|---|---|---|
| D01 | 新旧项目与公共名称 | 新包／CLI `external-subagent`，MCP 前缀 `external_subagent_`；新数据目录、socket、LaunchAgent、Codex plugin identity。旧 ZAS 保留，不自动迁移会话、不设旧命令别名、不自动卸载旧插件。 | 要兼容／迁移必须另写具体转换与回滚合同，不能顺手复制数据库。 |
| D02 | 首发平台与发布身份 | v1 正式支持 **macOS arm64**，继承已有可验收平台；架构不绑定 macOS，其他平台明确 unsupported。npm 发布名若不可用采用用户所属 scope，CLI 名保持不变；Node 运行范围须按实际发布依赖核实，DSH 的 Node 要求另行检测。 | 要同时支持 Linux、Intel Mac 或 Windows，会新增进程回收、服务注册、权限执行与原生包验收，须扩展计划后再开工。包名所有权／发布凭据只能由用户授权。 |
| D03 | 扩展的含义与技术栈 | 先做**编译期适配器**：独立 crate + 一个 composition-root 注册点 + 同一 conformance suite；核心不得到处判断 zcode/dsh。维持 Rust/Node，不全量重写 TypeScript，不做动态插件下载／热加载／市场。 | 要用户安装任意第三方适配器，应增加独立进程插件协议及信任／版本合同；不把任意 executable 自动视为已支持 agent。 |
| D04 | DSH 权限是否必须与 ZCode 四模式／精确 write_manifest 完全同级 | **能力显式化，不伪装同级**：ZCode 保留现有保证；DSH 首发必需 build 与严格 plan。build 使用受限 workspace-write + 可响应权限请求；plan 必须真实只读且禁止 shell／可执行代码绕过。非空精确 write_manifest、edit/yolo 只在对应保证已证明时启用，否则 spawn 明确拒绝。优先受管 profile/patch；需要专门策略插件时先提交有证据的窄 scope delta。 | 若要求 DSH 四模式＋精确文件白名单首发全覆盖，须把 DSH 策略插件纳入强制范围，不能在 S04 末尾把 unsupported 当完成。未证实严格 plan 是发布阻断，不是普通降级。 |
| D05 | 默认模型与凭据／真实探测 | 默认模型按 agent 配置，不设跨 agent 全局模型；ZCode 禁止设置默认模型或 spawn model。DSH 复用显式选择的本地 DSH_HOME 与原生凭据，建独立受管 profile，不复制密钥。status 默认无模型请求；真实 hi 由显式 probe 或用户授权的 init 选项触发，并注明使用同一生产路由。 | 单一全局模型容易落到不支持的 agent；每次 status 自动 hi 会产生费用／延迟。隔离 DSH_HOME 可以选，但必须另解决原生登录与设置，不偷偷搬密钥。 |
| D06 | 终态续聊、崩溃恢复与“完成”的含义 | v1 延续**终态不能 send**；进程丢失不自动重放有副作用的 prompt。DSH 原生 resume 仅记录为上游能力，不自动升级产品承诺。completed 表示可验证的协议完成，不代表代码通过业务验收。DSH 文本按已验证 messageId 聚合最后文本消息，保留 upstream stop reason。 | 要恢复／续聊，需补请求投递不确定性、原生 session ownership、历史结果与新轮次的独立身份，不应只加一个 resume 调用。若要区分 ACP 被折叠的所有 stop reason，需增加桥接证据接口。 |
| D07 | npm 更新遇到活跃任务怎么办 | **默认排空后自动激活，不杀任务**。npm hook 将候选版本完整暂存；已有 daemon 进入可见 draining，保留 wait/respond/cancel/result/close，拒绝新 spawn 和新 send；既已入队消息按原合同处理；空闲且已回收后启动一次性 updater 完成切换／重启／插件同步。排空超时显示 pending，旧版本继续可用；强制取消只能显式 `--cancel-active --yes`。 | “立即更新并杀掉所有任务”会改变用户任务保证。不能用 npm hook 返回 0 表示全链路已激活，也不能承诺跨 npm／launchd／Codex 的全局原子事务。 |
| D08 | 哪些 Codex 安装可被自动更新，是否强退调用方 | 只更新用户已登记／明确认领的 Codex homes 与本产品绑定；记录自定义 CODEX_HOME、marketplace、插件身份、受管源路径。保存原 enabled 状态。更新安装副本后报告 `reload_required`；不擅自结束正在使用的 Codex。 | 扫描并改写整个 HOME、更新所有相似名字插件，或自动重启用户会话都需要额外授权。安装副本版本与当前进程已加载版本必须区分。 |
| D09 | 文件规模约束 | 按行为 owner 拆分；生产文件目标 150–400 行，超过 600 行必须解释，超过 1000 行须明确例外，否则拆分后才接受该 owner。测试移入独立测试文件；generated/fixture 不计此阈值。先用简单统计＋review，不新增通用代码治理平台。 | 阈值可调整，但不能只把 lib.rs 改叫 scheduler.rs 后留下 6000 行。不要为满足行数把同一个不变量分散成几十个薄转发文件。 |
| D10 | DSH 受管子进程的附加能力与数据边界 | 只启用经过验收的工具组合；不自动启用嵌套 subagent／自修改 profile／任意 MCP。受管子进程默认关闭可关闭的 telemetry／日志上传，不改变用户全局 DSH 设置；模型调用仍按用户原生 provider 路由。 | 开启外部插件、额外 MCP、自动上传或嵌套 agent，需要重新核验权限覆盖与回收边界。此项目不声称防御恶意同 UID 程序或可信宿主插件。 |

## 工程决策建议（无需用户选择实现细节，但不能伪称已批准）

E01：一 external Agent 对应一个受管理的 provider 进程。DSH 原生支持单连接多 session，但 v1 不做池化，避免共享进程故障、权限、模型和取消之间的新耦合。

E02：保留现有单 workspace 活跃任务约束，并扩展到**跨 provider**；同一物理 workspace 的 ZCode 与 DSH 不能各自拿一把独立锁。旧 ZAS 与新产品共存不共享锁，因此不能同时操作同一 workspace；迁移试用按人工切换，不偷偷引入跨产品锁服务。

E03：更新使用版本化不可变 payload 与稳定 MCP 入口。daemon 的真实执行路径、运行版本、目标版本、plugin 安装版本分别记录。旧 payload 在新版本验证前不删除。只做已知受管文件的补偿回滚；数据 schema 不兼容时拒绝伪回滚。

E04：新增能力状态是小型数据结构，不能把 `available=true` 当作已验证认证、模型可用、权限可执行的全部保证。未知项为 unknown，错误不得自动切换 agent/model。

E05：继续使用已有 ZAS 作为 SFD 外部独立 review 通道，开发中的 external-subagent 不是它自己的验收裁判；本计划不修改 SFD 4.5.1 本体。

## S01 后可能触发的窄工程决定

仅当实测证明以下缺口时，才决定新增 `plugins/dsh-policy/`：原生组合无法可靠禁止 shell／间接执行、无法兑现已批准的精确文件 scope，或必须恢复 ACP 未公开的终态语义。插件只负责策略／必要元数据，不拥有任务队列、数据库、进程 supervisor、wait/result API 或第二套 session 生命周期。

S01 结果不能自动把插件范围放大；先记录实际失败 fixture、缺失的官方 hook／服务接口与最小可修补 owner，作有界 PLAN delta。若官方 profile 已足够，完全不创建该插件。这不是“实现不了就静默降低安全性”的后门。
