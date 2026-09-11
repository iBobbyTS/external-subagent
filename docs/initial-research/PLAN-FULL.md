# PLAN-FULL — external-subagent v1

- Feature: `external-subagent-v1-20260910`
- Skill: **sectioned-feature-development 4.5.1**，来自本次附件；本文件采用其分段、模型路由、边界 handoff、有限修复与双路径独立 review 规则。
- Status: **DRAFT / OWNER_DECISIONS_ACCEPTED / PLAN_REVIEW_NOT_REQUIRED_BY_USER**。
- Invocation: `USER_EXPLICIT`，本次仅要求研究与计划；没有开始产品实现、用户机器安装或 npm 发布。
- Requirements: [REQUIREMENTS.md](REQUIREMENTS.md)；原始请求 [ORIGINAL-REQUEST.md](ORIGINAL-REQUEST.md)；待决策 [DECISIONS.md](DECISIONS.md)。
- Source repository snapshot: `zcode-as-subagent`，`codex/wait-respondable-20260910`，HEAD `bb45d562671ddbd99637c5680449bc75aedb378b`。
- Target repository / feature base / current branch: **NOT_CREATED / UNKNOWN**；实际新目录由用户提供或主控在获准创建时记录，不能写旧项目绝对路径冒充新路径。
- Execution mode: 本次 `PLAN_ONLY`；后续实施／本地验证／commit 权限由真实 invocation 决定。不默认 merge、push、npm publish、删除旧仓库或修改全局 provider 配置。
- Product support baseline proposed: macOS arm64；ZCode app-server 与官方 DSH ACP；Node/Rust；见 D01—D10。
- Independent PLAN review: native `@plan_reviewer` 与第二独立 ZAS/ZCode review 均按用户明确要求不执行；当前没有 reviewer ID 或 verdict。

## 目标、权威与最小充分范围

交付一个独立产品，让 Codex 经 CLI/MCP 管理 ZCode 与 DSH 的真实外部任务；调用方使用同一 wait/result/request 生命周期，agent 差异仅通过已声明能力体现。新项目不牺牲已有 ZCode 合同，不再把所有 owner 堆进 6000 行 lib.rs。

权威优先级：用户原始要求与后续明确决定 → 当前上传源码/其最新需求修正 → 已批准的 REQUIREMENTS → PLAN。公共网页只证明上游能力，SFD 规划知识只帮助组织，不自行创设产品需求。本包的 D/P 项均未冒充已批准。

当前最小设计：一个 daemon、一个 durable store owner、一个跨 agent workspace admission owner、一个通用生命周期、两个 provider adapters、一个 npm/安装协调 owner。DSH 直接用官方 ACP；只有 S01 的可复现安全缺口和明确决策允许小型策略插件。扩展不是动态插件市场；native resume 不自动成为本产品恢复承诺。

排除：GUI、远程多租户、agent/model fallback、provider 自动安装／升级、终态续聊、自动 replay 不确定 prompt、自动杀活跃任务更新、任意自定义 plugin/MCP 的安全保证、旧 ZAS 数据迁移、工作区并行写入、全仓治理框架、未授权发布。本计划不修改 SFD 本体。

## 前置状态与实施准入

D01—D10 已按用户确认记录在 REQUIREMENTS.md；本次不执行额外 PLAN review。主控只更新受影响的合同／未实施 section。后续获准实施时依SFD创建独立feature branch并逐section提交，不在旧仓库修改分支；实际base/head和权限写入FEATURE-STATE。S01 的真实 hi、credentials 使用和指定 workspace 配置读取应有对应授权；缺失环境只能记录阻断，不伪称测试通过。

上传工作树的 97 个现存 tracked 文件与固定 HEAD 一致，但缺少 40 个已跟踪 tests 文件；Git 对象可恢复。新仓库从该固定 HEAD 导出所需源码与测试，保留 LICENSE/来源，不复制旧 `.git`、工作日志、凭据或旧 `.agent-work` 历史。旧仓库 inspect-only；不执行 reset/clean/gc 或分支修复。新项目采用单主工作树、串行 writer，AGENTS 记录新路径及新授权，不照抄旧绝对路径。

当前只完成源码／文档研究与计划文本检查。尚未运行任何新产品命令、真实 DSH/ZCode 测试、Codex plugin 安装、npm 安装／升级或 macOS LaunchAgent 验证。S01 与后续所有 AC 均是待执行；这不影响本次需求确认和计划交付。

## 规划知识路由与版本限制

以下路径相对于附件 skill 的 `references/planning/`，均按本次变更选择；不展开 language × domain 组合，也不为每份资料新增 review/section：

| 轴 | 选中路径 | 源码／需求触发及采用问题 |
|---|---|---|
| 通用 | `router.md`、`universal.md`、`boundary-handoff.md` | 多个 serializer 与明确 Codex consumer；共享一套反例，先证明外部 seam |
| Domain | `domains/cli-automation.md` | agent/config/probe/npm 的可重复 CLI 行为、非交互错误与 PATH |
| Domain | `domains/systems-daemons.md` | durable task、进程回收、launchd、活跃任务排空 |
| Language | `languages/rust.md` | 原有 traits/store/scheduler 与 async 所有权；依赖 owner 拆分 |
| Language | `languages/javascript.md` | npm ESM CLI、安装脚本、参数／JSON 边界 |
| Language | `languages/python.md` | 仅恢复并适配原 `tests/live-agent` 的Python测试harness；不是新增Python产品后端或一律重写成JS |
| Runtime | `adapters/runtimes/nodejs.md` | npm lifecycle、可执行入口、Node 环境与安装差异 |
| Runtime | `adapters/runtimes/tokio.md` | 锁／stdio／响应绝对 deadline、取消安全、进程事实 |
| Platform | `adapters/platforms/macos.md` | 首发 arm64、LaunchAgent、GUI PATH 与原生 payload |
| Concern | `concerns/async-lifecycle.md` | queue、permission、终态、排空、恢复的同一状态 owner |
| Concern | `concerns/external-integration.md` | ZCode/ACP/Codex 版本与已观测协议，不靠模拟接口猜测 |
| Concern | `concerns/data-evolution.md` | 新存储身份／schema version 与后续 upgrade downgrade 限制 |
| 条件分支 | `languages/typescript.md` | 仅 S01 证据导致批准 DSH 策略插件时激活；不是默认 TS 重写 |

同时遵循 skill 的 `section-planning.md`、`model-routing.md`、`external-reviewer-orchestration.md`。未匹配实现依赖用 universal，不自动加载全部语言／领域。

版本证据：报告读取的 DSH master CLI manifest 是 `0.1.5-rc.2`，根 Node 范围 `^22.19.0 || >=24.0.0`，不是已确认 npm latest。官方 ACP/server 与 client README 的 update 口径有差异。S01 固定真实 runtime/version/来源并记录 wire evidence；S05 固定真实 Codex/npm/Node 版本，不能只依赖移动网页。[RESEARCH-REPORT.md](RESEARCH-REPORT.md) R02—R19 是外部证据入口。

## 共享 producer → consumer 边界与反例

状态标注：**SOURCE_INSPECTED** 为源码所见；**OBSERVED** 仅真实运行后可填；**PLANNED** 为新接口／新测试；**UNKNOWN** 为尚无证据。当前本表没有新产品 OBSERVED 样例。

| ID / authority | Producer → serializer → owner → response → named consumer | 同一输入／反例及判错 oracle | 状态／落点 |
|---|---|---|---|
| X01 / U04 | CLI flags 或 MCP arguments → external-contract Spawn DTO → core admission + daemon agent registry → task route/错误 → CLI、MCP、Codex skill | `{repository:"/fixture",prompt:"hi"}` 且无 default_agent → `agent_required`，spawn 计数为0；ZCode+model → `model_selection_unsupported`，不改配置 | PLANNED；S03 同一 fixture 供 CLI/MCP/consumer 使用 |
| X02 / B02/B03 | provider event → adapter request event → store request → bounded100 projection → wait/command_pending_approval → CLI/MCP/Codex skill | Read/other respondable+PENDING 立即醒；nonrespondable/SENDING/RESPONDED 不醒；第101条不承诺醒；after_revision 普通变更不醒 | SOURCE_INSPECTED旧ZAS、PLANNED新名；S02/S04 |
| X03 / B04 | completed text → UTF-8 durable result → serializer frame guard → complete/next_offset → CLI/MCP/skill | 控制字符编码膨胀、emoji跨边界；result<=262144字节；wait允许>256KiB完整内联但不得超过2MiB编码帧，超限才分页 | SOURCE_INSPECTED旧ZAS；S02复用、S04共用 |
| X04 / U03/U05 | executable/env/provider signal → adapter probe evidence → status owner → agents[] → CLI/MCP/Codex skill | ACP authenticate success+真实401 → 不能valid；限流/网络另类；临时workspace的有效证据不能外推含不同.env的workspace | 上游SOURCE_INSPECTED；S01产出真实脱敏fixture，S03消费 |
| X05 / U04/P02 | native catalog → adapter opaque token → task-config snapshot → set_config_option响应后prompt → CLI/MCP result metadata | 不存在token先失败；省略model不覆盖；config更新不改已admitted turn；两个agent不能共享全局default_model | SOURCE_INSPECTED上游、PLANNED本产品；S01→S03→S04 |
| X06 / B07/P03 | tool update+request_permission → correlation/generation → PENDING/SENDING/RESPONDED → wait/respond → calling model | toolCallId缺上下文标unknown；allow只能选择已offer的单次选项；迟到/重复/取消后回应不得复活任务；stdin锁阻塞有界 | PLANNED；S01捕捉形状、S04端到端 |
| X07 / B08/P06 | DSH message/thought/tool updates → messageId聚合/公开字段过滤 → completion/observe → result → CLI/MCP | 进度消息≠最终消息；thought不得混入结果；top3×5/200字符；end_turn不等于业务验收passed | SOURCE_INSPECTED上游；S01核验、S04反例 |
| X08 / U06 | npm candidate/postinstall或update → reconcile → draining/active identity → actual daemon/plugin versions → CLI+Codex host | 包版本已新但daemon仍旧须pending；一个home失败→partial；ignore-scripts不能声称重启；host reload_required不等于已加载 | PLANNED；S05安装消费者、S06完整协调 |
| X09 / B09/P04/P05 | permission_mode/manifest → adapter受管profile+实际tool dispatch →强制策略→ admission/permission结果 → CLI/MCP | strict plan的shell/indirect exec/write拒绝，不因用户allow升级；unsupported精确manifest在prompt前拒绝 | PLANNED；S01验证接缝、S04最终强制 |

显式命名的 downstream consumers：**CLI commands、MCP schema/projection/tool descriptions、Codex plugin manifest/skill/mcp binding 均是 EDIT**；必须与协议在同 section 同步。官方 runtime/Codex 源码、原 ZAS 源码为 INSPECT_ONLY。已有不变 consumer 行为标 VERIFY_UNCHANGED，并用上表同一 fixture 证明；不能留到“最后改文档”再修。

HANDOFF 包只需要真实来源/version、该 section 的输入输出片段与失败反例、消费者路径、实际测试结果；去除凭据。不另建 schema审批JSON／证据哈希链或动态工作流引擎。未运行样例不能从 PLANNED 改成 OBSERVED。

## 调度、模型、review 与有限修复

顺序：**S01 → S02（A→B）→ S03 → S04（A→B）→ S05 → S06**，全程串行。同一 parent 的子项只在前一 checkpoint 关闭后推进；parent fully accepted、集成通过后才可启下个 parent。没有并行授权，不新增开发 worktree。

固定角色（具体实际模型是否可用、是否暴露 observed model，均由本机执行时核验）：已完成的 S01/S02/S03 保留实际 native 执行记录；自本次 owner override 起，所有未来 `impl_nano`、`impl_mini`、`impl_std` 单元改由 [@Zcode As Subagent](plugin://zcode-as-subagent@personal) 执行；`impl_large`=astra/medium、code_reviewer=astra/high、plan_reviewer=astra/xhigh、code_explorer=luna/xhigh 保持不变。ZCode worker 直接 spawn 并保存真实 agent_id，不做例行 status/list 预检，不盲目重试；完成后分页读取完整 result 并 close 终态任务。下方 role 链接不是已 dispatch 的证据。

**PLAN barrier**：机械 validator 后，先 fresh native `@plan_reviewer` 只读审查持久化完整计划；主控读完、持久化并收敛必要改动。该 feature 同时命中公共协议、进程/并发、权限/凭据、schema、多个 owner 与至少3产品节，故再**直接**调用现有 `zcode_subagent_spawn` 做第二独立 PLAN review，不向它提供第一位 reviewer 的结论。两条真实路径都返回并完成 admission，必要 delta 关闭，才解锁 S01；第二ZCode PLAN review后最多一次有界delta验证，不扩成第三次独立设计评审。没有第三个独立 PLAN reviewer。

ZAS 不经 native proxy，不用开发中的 external-subagent 自审。既有 ZAS 的终态不能 send：需要 delta 时仅在原会话仍可操作才续用；否则按已适用授权记录 `TERMINAL_CONTINUATION_UNSUPPORTED` 与同 provider fresh-delta limitation，或在 strict continuity 下阻断。不能编造同会话 continuation。两条独立 PLAN review 当前 **NOT_RUN**。

**Code full-review slots** feature-wide 按 **native Astra → ZCode → native Astra → …** 交替，PLAN不消耗slot。S01预计先native；任何实际新增独立final都会推进全局slot，因此不硬编码后续 section 的provider。FEATURE-STATE记录 planned/effective provider、真实工具、真实ID、base/head；修复复验不冒充新独立session。先读取结果并admit，再允许writer。

S02/S04父级各在首个子项checkpoint建立一个 primary logical slot；后续 SUBSECTION_DELTA/PARENT_RECONCILIATION延续该coverage，不能每个子项重开full review。父级TWO的fresh FINAL使用下一个独立provider；无每子项独立final。所有物理调用仍如实记成本。

ONE 初次干净可按skill接受；若有修复，仍按skill取得必要fresh FINAL；TWO即使初次CLEAN也需修复／reconciliation后fresh independent FINAL。每个parent共享最多5轮ordinary repair；第6轮按skill先根因诊断与有界处置，不无限重试、不换子项清零。环境/鉴权/工具错误不等于模型under-routing；外部review transport失败最多一次有界重试，缺结果绝不算CLEAN。需要改模型只更新未解决unit、做有界PLAN delta并记录实际重新dispatch；不走低到高试遍模型阶梯。

单独审查输出、主控 admission、产品测试与发布授权是不同事实。main 自查不算 independent review。不能以“结构校验PASS”打开产品实现或发布 gate。

## 共用验证与文件规则

源码／文件owner遵循 [FILE-ARCHITECTURE.md](FILE-ARCHITECTURE.md)。生产文件目标150–400行，>600说明，>1000明确例外或拆分；入口只装配，建议<150行；测试独立、fixture/generated另计。这个规则在每节 touched owners 内落实，不建立全仓扫描平台。

验证分层：targeted最小错误oracle → 当前section/parent组合 → 接受后的serial integration → S05/S06真实安装环境。非触及的已有集合只在合理集成点跑，不每次改一行跑全部实时provider。检查命令下面均为**PLANNED**，未来各section先创建自己的test/script再运行；不得把不存在的测试命令记为PASS。

标准基线可用 `cargo fmt --check`、`cargo check --workspace`、`cargo test --workspace`；CLI具体目录使用 `node --test`。native provider测试只在macOS arm64、授权runtime/credentials存在时跑；fake通过不能代替live。仓库权限允许时才写commit，禁止默认merge/push；git diff在提交前、审查失败后、冲突提示后、切换section前使用，不在每次工具调用前高频扫描。

## S01 — 用真实 ACP 证明 DSH 接入边界
- Implementer: [@impl_std](subagent://impl_std)
- Depends on: none

### 业务合同

**Outcome / authority / necessity**：U01/U03/U04 与 D04 要求两种真实agent；目前 moving master／用户安装版本／strict plan 强制接缝未实测。用一个可丢弃但证据可保存的最小 ACP probe 判定官方通道是否足够，避免先建错误插件框架。不是产品第二个server。

**Primary owner**：`tools/probes/dsh-acp/` 的单个probe入口与fixture记录；其生命周期只管理自己的短时探测进程，不进入产品daemon路径。

**Frozen allowed-to-edit**：`tools/probes/dsh-acp/**`；`tests/fixtures/dsh-acp/**`；`docs/compatibility/dsh.md`；`docs/acceptance/S01.md`；新仓库 `UPSTREAM.md`、`AGENTS.md` 仅来源/实际规则；`.agent-work` 当前执行记录。可以新建最小测试运行说明，不提前创建生产adapter/core。

**Inspect-only**：旧source的driver/preparation/runtime/request owner及Git对象；官方ACP `index.ts/session.ts/model-control.ts/updates.ts/codec.ts`、CLI/profile/sandbox文档；受授权的本机runtime版本/配置来源。**Exclusions**：修改旧ZAS、自动装DSH、用户默认profile/credentials写入、GUI fork、生产policy plugin、发布与常驻服务。

**Allowed structural changes**：仅有界stdio probe与脱敏fixture，必要最小开发依赖锁定；配置测试放临时profile/受控home范围，credential通过用户允许的native路线引用。不得添加通用ACP SDK框架／runtime registry／长期worker。

**Safe intermediate state / invariants**：S01不发布“DSH supported”。stdout仅wire、stderr独立；所有probe有deadline与cleanup；错误key仅在隔离测试环境注入，不覆盖用户凭据。临时workspace hi只证明该scope；真实workspace环境差异须单独授权验证。没有工具强制策略证据时不得让模型进入用户仓库试错。

**AC / targeted checks**：
- 固定并记录 executable实路径、版本／来源、Node版本、initialize/capabilities与session方法真实响应，不编造 `dsh app-server` 或 `login status` 命令。
- 记录正常hi、缺key/401、网络错误；证明ACP authenticate成功≠provider认证成功。获取真实catalog token并在prompt前选定；省略时native决定。
- 捕获可关联的tool update/request_permission、单次allow/deny、取消、close、EOF；核实messageId/text/thought与实际prompt settlement，关键响应必须能拒绝错实现。
- 列举受管composition全部实际执行/写入入口，验证strict plan无Bash/间接执行和readonly，而非只检查配置文本。若存在缺口，保存最小反例/可用官方hook，主控提交D04窄决策；不把缺口写成已满足。
- 对等待权限期间的stdio持续读取、stdin阻塞/EOF/坏帧、stderr污染与退出清理建立脱敏fixture。

**Section oracle / handoff**：S01 owner先创建 `tools/probes/dsh-acp/probe.mjs`、`tools/probes/dsh-acp/probe.test.mjs`；计划执行 `node --test tools/probes/dsh-acp/probe.test.mjs` 与有授权的 `node tools/probes/dsh-acp/probe.mjs --scenario <已实现scenario>`。所有scenario由入口help列出，不预造上游命令。`docs/compatibility/dsh.md` 交付真实X04—X07/X09 shapes、支持版本与未证实项。核心lifecycle不满足则BLOCKED；strict plan存在可补缺口时必须先批准窄实现路线并delta计划，才能接受此probe与进入相应后续实现；不要求probe提前完成生产插件。

**Review**：BOUNDED；assurance **ONE**。probe无生产持久状态，但凭据脱敏、观察真实性和策略判定必须独立检查。

**Routing**：profile `impl_std`。analogue=none（本仓库没有同型DSH ACP probe）；参考已从Git恢复的 `tests/live-agent/non-git-based/s01_direct_live_runner.py` 与官方ACP wire，但不把它们当同型实现；ambiguity=bounded unknown，由明确scenario解答；semantic_hops=CLI→stdio→ACP→fixture→compatibility consumer；state_coupling=局部异步process/request；oracle_strength=初始partial，实测fixture后decisive；novel_reasoning=yes，理解文档/实现差异但不设计生产架构；context_scope=probe+5个官方接缝+有限旧driver；false_clean后果=错误承诺认证/权限，阻断后续真实接入。Sol负责足够，若发现结构未知先gate而非让probe扩大成framework。

## S02 — 在新命名空间完整保留 ZCode 生命周期
- Implementer: [@impl_large](subagent://impl_large)
- Depends on: S01

### 父级合同

**Outcome / authority / necessity**：U01/U02、B01—B10、D01/D03/D09。让新CLI/MCP真实管理ZCode，同时拆出可接第二provider而不复制6000行owner的架构。不能仅建抽象层后依赖后节补回旧功能。

**Primary owner**：`external-core` 的task lifecycle；公开DTO唯一，runtime process与store事务各一个owner，daemon composition root选择adapter。

**Parent frozen allowed-to-edit**：`Cargo.toml/Cargo.lock`；`crates/external-{contract,runtime,store,core,agent-zcode,daemon,mcp}/**`；`bin/external-subagent*.mjs`；`cli/main.mjs`、`cli/rpc-client.mjs`、`cli/errors.mjs`、`cli/commands/tasks.mjs`、`cli/commands/daemon.mjs`、`cli/commands/diagnose.mjs`、`cli/commands/maintenance.mjs`；`tests/{fixtures,contract,integration,cli}/**` 中ZCode/base行为，以及原测试保留路径 `tests/live-agent/**`、`tests/platform/**`、`tests/test_fixtures.py`；`plugins/codex/external-subagent/**` 中工具名/schema/skill消费者；`package.json` 仅开发命令与入口；`LICENSE/UPSTREAM.md/AGENTS.md`；`docs/architecture.md`、`docs/protocol.md`、`docs/acceptance/S02.md`。S02不承担安装器实现。

**Inspect-only/exclusions**：旧仓库全体source/Git objects；S01证据与future DSH/installer设计。禁止修改旧仓库、引入产品DSH adapter、全量旧state migration、动态plugin加载、第二store/processsupervisor、把未完成DSH advertise可spawn。

**Allowed structural changes**：按FILE-ARCHITECTURE新建上述crate与最小 `AgentAdapter` trait；由原ManagedRuntime/RuntimeFactory演进而非重写所有逻辑。抽取一个通用process/deadline owner与一个外部task/config身份；新数据库/schema身份不接旧DB；fake transport放tests或明确test-only模块，不做生产fallback。保留MIT来源与必要notice。

**Parent invariants/joint oracle**：ZCode所有旧公开行为除批准的命名/agent路由扩展外保持；数字agent_id不变，原生模型目录workaround与权限留adapter；wait/队列/取消/close/回收在拆分前后同一oracle。源tests缺40个必须恢复并核对，不删测迁就重构。S02.A通过只是walking path checkpoint，不代表整S02可发布；S02.B共同接受后才宣称ZCode parity。

**Parent review**：HIGH_RISK；assurance **TWO**；共享一个primary logical slot、累计repair lineage与fresh parent FINAL。joint checks包括真实MCP和CLI、ZCode plan/build/edit/yolo及已有manifest反例、异常恢复与所有恢复测试的适配结果；未授权高权限实测明确NOT_RUN并阻断相应支持声明。

**Parent routing**：profile `impl_large`；analogue=旧agentd/lib.rs+rpc.rs+mcp.rs、store/lib.rs、driver/lib.rs；ambiguity=structural unknown在拆owner/adapter所有权，不在公共产品语义；semantic_hops=双入口→公共service→scheduler/store/runtime→ZCode→projection→Codex skill；state_coupling=shared async+transactional；oracle_strength=恢复测试较强但抽取组合初始partial；novel_reasoning=yes，同步生命周期边界不能安全分给互不知情的layer writers；context_scope=当前完整ZCode生命周期影响锥；false_clean=丢任务/越权/双执行。Astra medium保留父级合成与修复责任。

### S02.A — 新 CLI/MCP 的 ZCode spawn→wait→result→close 贯通
- Implementer: [@impl_large](subagent://impl_large)
- Depends on: none

**Outcome / authority / owner**：U01/U02、B01/B02/B04/B05/B09；在新命名空间完成一个真实task的请求到资源关闭，primary owner仍是core lifecycle，不按目录拆层。

**Frozen allowed-to-edit**：父manifest内的contract/runtime/store/core/adapters ZCode/daemon/MCP的walking-path必需文件；bin与CLI task基础dispatch；ZCode fixture/test恢复；Codex工具schema/skill的walking-path说明；Cargo/开发package配置与source provenance。**Inspect-only**：旧source与其tests、S01；**exclude**：未触及queue/request/recovery语义的重设计、agent配置UI、安装/DSH。公共面不对尚未迁移的能力谎称完整可用；checkpoint未发布。

**Allowed structural changes**：父级允许的最小crate/trait/新state路径及process ownership；只按真实ZCode路径抽取，不能预造未来provider专属字段或把core写成ZCode内部术语。

**Invariant / safe state**：task admission与workspace锁一起建立，资源回收前不能释放冲突占位；模型不覆盖；终态结果关联准确；close不删除上游用户历史。旧产品保持原状；新产品未正式注册用户Codex。

**AC/tests**：固定HEAD恢复测试逐项归属；`spawn(agent=zcode)`→连续wait→完整最终结果→分页→close经真CLI/MCP一致；无默认agent的中间开发路由要求显式zcode，不能默认第一个。Unicode/控制字符编码frame边界、终态与resources_reaped不同步、空result、runtime启动失败均有判错测试。入口薄化与最大的几个owner先拆到真实归属，不保留6000行改名副本。

**Create-before-run / handoff**：本child创建 `tests/contract/walking-path.test.mjs` 及相关crate tests，未来运行 `node --test tests/contract/walking-path.test.mjs`、受影响cargo tests和授权live ZCode walking path。把X02/X03 actual新DTO/响应片段交给S02.B，同一Codex consumer同步，不在S05才改语义。

**Review**：HIGH_RISK；assurance **TWO（由父级联合满足；child无独立FINAL）**。首checkpoint启动父primary review，未关闭不可推进B。

**Routing**：profile `impl_large`；analogue=旧Scheduler/RuntimeOwner/RPC及现成result tests；ambiguity=结构抽取，公共规则已给；semantic_hops=CLI/MCP→admission→runtime/store→result→close；state_coupling=sharedasync+事务；oracle_strength=旧regression强、组合需新增；novel_reasoning=yes，process与wire分离；context_scope=walking path加其直接serializer/consumer；false_clean=早返回/错结果/泄漏进程。不能因少几个命令降为机械改名。

### S02.B — 队列、审批、取消、恢复与观察的 ZCode 等价行为
- Implementer: [@impl_std](subagent://impl_std)
- Depends on: S02.A

**Outcome / authority / owner**：B02—B10；在已经贯通的core上恢复完整现有task控制。primary owner=core requests/messages/lifecycle；ZCode wire转换仍归adapter。

**Frozen allowed-to-edit**：父manifest内 `core/{messages,requests,scheduler,lifecycle,completion,recovery,activity,observation,diagnostics}.rs`、store对应modules、runtime/process/deadline、ZCode/session/event/permission/preparation与必要子模块；对应RPC/MCP handlers/projections、CLI task/diagnose/maintenance与Codex skill；其tests、protocol/acceptance。其它A已接受owner inspect-only，发生确切因果修复才由main有界接纳，不能自动扩为全仓重构。**Exclude**：DSH、升级器、终态续聊、answer/elicitation。

**Allowed structural changes**：已有owner局部辅助类型、恢复测试分文件；不再创建独立queue/request broker或第二观测数据库。

**Invariant / safe state**：单一PENDING谓词、投影100上限、取消优先/不复活、message_id幂等冲突、终态不能send；原hook只显式安装不升级为globalpolicy。审批write deadline包括mutex/write/flush，不只包住read等待。

**AC/tests**：Read/other可响应PENDING唤醒、nonrespondable/SENDING/RESPONDED/第101条/after_revision反例；queue drain与并发取消、重启后未知投递不replay、重复respond/迟到generation、partial write/EOF/阻塞锁有界回收；observe top3×5/200Unicode无results/encrypted；ZCode模式/manifest验证与原本支持的diagnose/backup/restore/purge能力按受管边界继承。

**Create-before-run / handoff**：child先建 `tests/contract/lifecycle-parity.test.mjs`、crate request/message/recovery tests与所有恢复原tests对应位置，再跑该file及 `cargo test --workspace`、既有CLI集合。live ZCode以旧版合同相同scenario验证；不要求用yolo任意修改用户仓库。父reconciliation覆盖A+B assembled head和X01—X03/X06消费者；必要fresh parent FINAL之后才accept S02。

**Review**：HIGH_RISK；assurance **TWO（父级）**，本child为SUBSECTION_DELTA不是新fullslot。

**Routing**：profile `impl_std`；analogue=旧lib/rpc/store/observation与已恢复tests；ambiguity=bounded/已由A冻结trait；semantic_hops=provider request/message→store→lifecycle→projection→caller；state_coupling=sharedasync/事务；oracle_strength=明确旧regression+竞态注入；novel_reasoning=no新增产品架构，需跨表示推理；context_scope=上述控制owner与直接测试；false_clean=重复执行/越权/取消失败，规则现成但不能降到简单机械tier。

## S03 — 配置、路由与分层 agent 状态成为同一公共能力
- Implementer: [@impl_std](subagent://impl_std)
- Depends on: S02

### 业务合同

**Outcome / authority / necessity**：U03—U05与D05；用户能配置多个agent、没有默认时被强制指定、正确拒绝ZCode model、看到存在/认证/能力/hi的真实区别。不能只给CLI一个配置文件而MCP仍走旧单agent路径。

**Primary owner**：daemon `agents` 的注册/配置快照与service；CLI读写配置但不能另实现模型解析顺序。

**Frozen allowed-to-edit**：`cli/config/**`、`cli/commands/{agents,config,tasks}.mjs`、`cli/main.mjs`；`external-contract`的spawn/status/error字段；`external-daemon/src/agents.rs`、management/status/spawn RPC handlers；`external-core/src/admission.rs`及固定task配置身份；store对应schema/task；runtime/capabilities和adapter discover/probe边界；ZCode discovery/probe；`external-agent-dsh`仅discovery/probe与S01已验收的最小protocol catalog探测（不得启用生产spawn）；MCP spawn/status/list/schema/projection；Codex skill；`tests/cli/agents-config.test.mjs`、`tests/contract/agent-routing.test.mjs`、对应fixtures；`docs/protocol.md`、`docs/compatibility/agents.md`、`docs/acceptance/S03.md`；Cargo注册这些必需依赖。

**Inspect-only/exclusions**：S02已接受scheduler/process/result；官方credential/capability接缝；旧config读取。禁止复制key、global default_model、默认fallback、在status触发付费probe、安装登录浏览器流程、用户全HOME扫描、把DSH启用为生产可spawn。

**Allowed structural changes**：一个已批准compile-time registry composition与小型配置schema/revision；一种probe evidence结构；采用已有store适当事实存储，不建第二数据库。DSH probe复用最终adapter同一launch/profile逻辑；S04接入后重新做production路径hi，不将开发probe冒称产品可用。

**Invariant / safe state**：request→agent-local default→native model；config变更不改active snapshot；空/null/unknown字段前置拒绝；status unknown真实保留且包含scope/version/time；DSH此中间版显示 `spawn_supported=false`/尚未集成，不把installed=true等同产品支持。临时workspace auth evidence只用于该scope；`--workspace`显式选中后按实际原生环境重新验证，不能继承别处valid。

**AC/tests**：flags/JSON/MCP三条X01 fixture同code/defaults；无default→agent_required；未知/disabled agent明确失败；ZCode任何model/default_model拒绝且prompt_count=0；DSH catalog token不解析/不硬编码；401与限流/网络不同；缺executable/版本不符/transport失败与凭据unknown分别展示；hi只在显式调用触发，模型执行次数可计数；daemon环境/PATH与交互shell不同不误判。list filter按agent且数字agent_id不变；Codex工具描述指明必须指定agent的场景。

**Create-before-run / section oracle**：本节先创建上述2个testfiles与fake registry/probe fixtures，再运行 `node --test tests/cli/agents-config.test.mjs tests/contract/agent-routing.test.mjs`、受影响crate tests。真实ZCode hi与DSH协议/catalog probe有授权时运行；DSH完整production hi责任归S04而不虚报。S03交付X01/X04/X05稳定DTO供S04使用，变更需同形consumer更新。

**Review**：BOUNDED；assurance **ONE**。新配置/默认值与真实证据有多个表示，但唯一owner明确、全组合可决断；出现repair按skill取得相应freshFINAL，不降真实性标准。

**Routing**：profile `impl_std`；analogue=旧CLI/schema/MCP spawn/status与S01probe；ambiguity=resolved产品优先级、bounded能力细节；semantic_hops=CLI config→registry snapshot→admission/probe→RPC/MCP→Codex；state_coupling=configuration revision+existingtask，不改调度算法；oracle_strength=decisive错误/计数fixture；novel_reasoning=no新架构但跨序列化/生效范围；context_scope=registry、config、admission与3个consumer；false_clean=错误agent/model/虚假登录，Sol承担跨表示推理。

## S04 — DSH 经同一核心完成安全、可控的真实任务
- Implementer: [@impl_large](subagent://impl_large)
- Depends on: S03

### 父级合同

**Outcome / authority / necessity**：U01—U05、P01—P08/P15与D04/D06/D10；DSH从只可发现升级为真正可spawn、wait、respond、queue/send、cancel、result、observe、close的provider。不能用hi成功取代生产权限/回收/结果保证。

**Primary owner**：`external-agent-dsh`官方ACP adapter；task生命周期仍由既有core唯一决定，不给DSH自建scheduler。

**Parent frozen allowed-to-edit**：`crates/external-agent-dsh/**`、`profiles/dsh/**`；runtime/capabilities/event/adapter仅S01证明必需的通用扩展；daemon composition registration；contract/status/result必要能力元数据与对应MCP/CLI/skill；core/store仅已批准通用event消费/结果关联（不能provider分支）；`tests/provider-conformance/**`、`tests/integration/dsh*`、`tests/fixtures/dsh-acp/**`、DSH相关CLI/contract tests及共用 `tests/live-agent/**` harness的必要调用适配；`docs/compatibility/dsh.md`、`docs/protocol.md`、`docs/acceptance/S04.md`；Cargo依赖。`plugins/dsh-policy/**`只有D04有证据窄delta明确批准后才在manifest中增加。

**Inspect-only/exclusions**：ZCode私有driver/protocol/policy、已接受安装外其他owner、官方DSH源码。禁止fork GUI/Codex-backend插件、动态provider市场、sessionpool、自动resume/replay、绕过认证、provider自动安装/全局profile改写、常驻策略server。

**Allowed structural changes**：DSH crate的ACP transport/session/model/permission/update/result分模块；受管启动profile；最多已批准的窄策略插件及其明确officialhook。一个test-only第三adapter用于同trait同conformance证明扩展，不随production发布；不能增加产品第三provider支持承诺。

**Joint invariant / intermediate**：A checkpoint期间DSH仍受未发布feature gate保护，只在受控测试可用；A不能把缺strict plan推给下一release。B完成严格plan与失败清理，父级joint tests/freshFINAL通过才打开正式DSH支持。ZCode能力/测试保持；同workspace跨provider同锁，不能各自持有自己的并发slot。

**Parent oracle**：两agent×同一公开lifecycle suite、真实DSH build/strictplan、actualMCP/Codex描述形状、生产daemon环境hi、超限Unicode结果、工具审批与取消、process重启故障；test-only第三adapter只改新adapter+composition注册/fixtures，不改core十处handler。DSH与ZCode输出差异在能力/metadata显式表现而非假装等价。

**Review**：HIGH_RISK；assurance **TWO**。A primary slot、B delta、parent reconciliation与fresh FINAL；共享5轮repair budget，不能通过拆child增加重试。

**Parent routing**：profile `impl_large`；analogue=ZCodeadapter/runtime+S01ACP fixtures但非同型权限/结果协议；ambiguity=bounded外部差异，结构关系由parent统一；semantic_hops=agent admission→ACP→permission/message/terminal→corestore→RPC/MCP→caller；state_coupling=sharedasync/requestgeneration/队列终态；oracle_strength=fixtures强但livepolicy初始partial；novel_reasoning=yes，上游stopreason折叠和安全组合不能简单搬ZCode；context_scope=DSH私有模块+同core接缝+所有直连consumer；false_clean=越权/重复任务/虚假completed。

### S04.A — DSH build、模型与审批的贯通任务
- Implementer: [@impl_std](subagent://impl_std)
- Execution provider: [@Zcode As Subagent](plugin://zcode-as-subagent@personal)；`impl_std` 仅为 SFD validator 的复杂度 profile，不得派发 native impl_std。
- Depends on: none

**Outcome / authority / owner**：U01/U04、P01—P03/P06；primary owner DSH ACP session，把S01已观测形状接到S03已冻结route与S02已成熟core。

**Frozen allowed-to-edit**：父manifest内DSH `acp/{transport,session,model,permission,update,result}`、profile的build组合、discovery/probe共用launch；runtimeevent/adapter必要映射、composition gate；对应core通用消费修正与同形contract/CLI/MCP/skill；build/model/approval/text fixtures和tests。**Inspect-only**：ZCode internals、strict-plan未批准hook、安装器；**exclude**：开放正式DSHsupport、绕过B安全gate、自行扩展publicanswer/alwaysallow。

**Allowed structural changes**：已有adapter内必要小模块/typedprivate ACP correlation；不能把upstream requestId直接当公共agent_id；不得新增DB/dispatcher服务。

**Invariant / safe state**：每task一process、stdio reader持续工作；model set响应后才prompt；queue仅在settlement后下一turn；request映射先持久化再PENDING投影；公开allow/deny只能选择upstreamoffer。progress/thought/tool与final文本分离，messageId必须真实支持。

**AC/tests**：S01fixture逐条穿过真实serializer/service；build任务发出工具权限，wait按X02醒，respond只一次；正常结果/lastmessage、多块/空块/stopreasonfold/max_tokens/error；config改动不影响inflight；unknownmodel不发prompt；queued send与duplicate message_id不重执行；toolCallId无context标unknown。先开gate的attempt必须测试拒绝。

**Create-before-run / handoff**：child创建DSH unittests与 `tests/integration/dsh-build.test.mjs`，计划运行该file与adapter/core相关cargo tests；受控livebuild只改fixtureworkspace，保存实际X05—X07。A输出正确buildloop但gate仍关；交B对同一路径做严格policy/失败补全，不复制第二套transport。

**Review**：HIGH_RISK；assurance **TWO（父级联合；本child无独立FINAL）**，A触发parent primary。

**Routing**：provider `Zcode As Subagent`，permission_mode=`build`，不传 `write_manifest`；analogue=S01wirefixtures+S02adaptertrait/requestqueue；ambiguity=boundedmapping，不再选择protocol；semantic_hops=ACP model/prompt/update→existingstore/request→projection；state_coupling=共享请求/队列，owner已给；oracle_strength=decisivewire与countingfake；novel_reasoning=no新生产架构，需多表示映射；context_scope=DSHadapter与少数coreevent接缝；false_clean=错误模型/错结果/权限deadlock。dispatch 后保存 agent_id，wait/respond/result/close 严格按 ZAS lifecycle 执行。

### S04.B — 严格 plan、取消回收与多 provider 同一合同
- Implementer: [@impl_large](subagent://impl_large)
- Depends on: S04.A

**Outcome / authority / owner**：B05—B09、P04—P08/P15；primary owner=DSH preparation/permission与既有runtime cleanup衔接，使DSH达到可公开支持的安全与生命周期边界。

**Frozen allowed-to-edit**：父manifest内DSH/profile/policy的已批准范围；runtime deadline/process仅有证据的通用修正；core admission/requests/lifecycle/observation与store相应通用事实；status/capabilities/consumer说明；provider-conformance第三testadapter与DSH失败测试。若需要新policy plugin但没有D04窄delta，先阻断，不能以父级笼统允许推定。**Inspect-only**：旧ZCode稳定policy与外部userhome；**exclude**：新增dynamic插件/同UID敌手模型/会话恢复/用户history清除。

**Allowed structural changes**：S01批准的profile强制或窄policyhook；真实supportedcapabilities；测试专用adapter与同一conformance suite。未知tool默认关闭，不为每条工具另建权限framework。

**Invariant / safe state**：strictplan readonly并无shell/indirectexec，用户allow不能升级；unsupportedmode/非空manifest先拒绝；latecallbacks不能复活；localcancel优先且资源事实可证；noauto replay；observe只公开bounded资料。B完成前A featuregate不开放。

**AC/tests**：直接dispatch及模型尝试两层覆盖所有启用write/shell/terminal/jobs/PTC/Cordis/nested/MCP入口；不启用入口验证不可调用。覆盖unsafeuserprofile覆盖尝试、inheritedworkspace.env范围、所有已批准sandboxescape反例；仅在本项目约定威胁模型内验收，不宣称防恶意同UID。取消等待permission/blockedstdin/EOF/坏帧/超大帧/daemon丢失，TERM/KILL有界且进程管理范围无残留；max_tokens/transportlost不伪completed。X03/X07大result/200Unicode与源码禁encrypted过滤；两个provider同workspace互斥与testadapter同套通过。

**Create-before-run / parent integration**：child创建 `tests/integration/dsh-policy.test.mjs`、`tests/integration/dsh-failures.test.mjs`、`tests/provider-conformance/provider.test.mjs` 及相关crate tests；未来运行这些tests、 `cargo test --workspace`、CLI/MCP全公开contract集合与授权两provider live。`agents probe deepseek-harness --hi` 在productiondaemon route重新验证，不能沿用S01独立probe作成功凭证。父reconciliation+freshFINAL通过才改spawn_supported并生成真实兼容矩阵。

**Review**：HIGH_RISK；assurance **TWO（父级）**。任何硬策略无证据即NOT_ACCEPTED，不以暂不支持strictplan关闭本section。

**Routing**：profile `impl_large`；analogue=旧ZCodepolicy只供不变量+官方DSHcomposition不同实现；ambiguity=bounded证据/可能已批准窄hook，未知越界须gate；semantic_hops=config→effectiveprofile→tool dispatch/permission→runtimecancel→durablefacts→consumer；state_coupling=sharedasync和跨provider安全约束；oracle_strength=需要negative live+faultfixture；novel_reasoning=yes，策略入口组合与取消优先不能拆给多个独立layerowner；context_scope=DSH安全组合+process/deadline+public能力；false_clean=越权/进程泄漏/重复执行。

## S05 — npm 全新安装后 Codex 能实际调用两种 agent
- Implementer: [@impl_std](subagent://impl_std)
- Execution provider: [@Zcode As Subagent](plugin://zcode-as-subagent@personal)；`impl_std` 仅为 SFD validator 的复杂度 profile，不得派发 native impl_std。
- Depends on: S04

### 业务合同

**Outcome / authority / necessity**：U06/P09/P10/P13；用户从npm artifact安装后，CLI在验证过的PATH可用，daemon启动，真实Codex发现正确plugin/MCP并完成两种agent最小任务。复制skill目录不等于安装成功。

**Primary owner**：`cli/install/`中fresh init/install的受管文件协调；daemon仍负责自己的process与task。

**Frozen allowed-to-edit**：`package.json/package-lock.json`、`bin/**`、`cli/commands/{plugin,daemon,maintenance}.mjs`、`cli/install/{layout,payload,path,codex,service-macos,reconcile,recovery}.mjs`、`cli/main.mjs`；`plugins/codex/external-subagent/**`；`npm/native/**`、`scripts/release/**`及本产品launchd模板；daemon/bootstrap/service和MCP启动/handshake所必需代码；`tests/install/**`、`tests/platform/**` 的平台安装行为与CLI/pluginfixtures；`docs/operations.md`、`docs/compatibility/codex.md`、`docs/acceptance/S05.md`；必要README发布说明。所有写本机安装路径的行为只能针对显式测试prefix/home或已授权正式home。

**Inspect-only/exclusions**：旧installer/pluginownership逻辑、当版官方Codex/npm；core/adapters已验收task行为。禁止registrypublish、自动升级DSH/ZCode/Codex本体、全HOME插件扫描/旧ZAS卸载、强制enabled、盲写Codexcache、sudo或未授权shellprofile修改。忙任务更新责任S06，S05未验证更新路径必须明确拒绝activation而非不安全热换。

**Allowed structural changes**：版本化payload目录/稳定入口、一种受管installation及caller注册记录、已知文件ownership/backup、一套macOS服务模板；依据实际Codex CLI支持的官方安装接口。允许薄Nodefacade或native稳定launcher，选定后不能让MCP依赖随机nvm路径；没有可用Node时明确失败或已授权绝对Node路径。不能建立通用包管理服务/所有插件扫描器。

**Invariant / safe state**：首次普通npm安装只装包与阶段化payload；服务/账号/真实hi/调用方改写由显式init及选项授权。支持平台不依赖用户编译Rust；unsupported平台help/version允许、business命令明确unsupported且不写HOME。stableMCP binding不依赖GUI PATH，所有相似名字非自动ownership。源码与cache/installedversion不同；已disabled的配置保留。

**AC/tests**：全新临时npm prefix执行pack/install后bin定位；shell和精简PATH/launchd分别启动；原生payload版本/架构/权限与package一致；空格/Unicode路径；默认及自定义CODEX_HOME；有无既有marketplace/plugin配置的非破坏合并、重复init幂等、managedsource漂移拒绝。真实Codex工具发现十个external工具，spawn agent/model语义、wait/result说明正确；两agent各hi/最小fixture任务。install时DSH缺失仍允许产品安装但status明确missing，不能自动安装provider。

**Create-before-run / section oracle**：先创建 `tests/install/fresh-install.test.mjs`、`tests/install/codex-binding.test.mjs` 与release pack检查；计划运行 `node --test tests/install/*.test.mjs`、`npm pack`、在受控prefix的 `npm install -g --prefix <fixture-prefix> <本次生成tgz>`；实际Codex命令先help/version验证后记录到compatibility，不凭空写死旧add语法。产品prompt不读取用户真实仓库；无授权真实Codex安装则NOT_RUN，不能假称S05完成。

**Review**：HIGH_RISK；assurance **TWO**。影响用户配置、服务与发布artifact；虽然很多有旧installer范例，错误ownership会损害非本产品配置。

**Routing**：provider `Zcode As Subagent`，permission_mode=`build`，不传 `write_manifest`；analogue=旧cli/installer.mjs、bin、launchd、plugins与已恢复installtests；ambiguity=bounded官方Codex版本接口，通过probe收敛；semantic_hops=npm→payload/layout→service→Codexregistration/cache→MCP→task；state_coupling=本地多文件安装+既有daemon，升级并发另节；oracle_strength=fixtureFS+真实pack/host工具发现；novel_reasoning=no新生命周期架构，需installer跨环境推理；context_scope=installer/manifest/launchfacade及少数bootstrap；false_clean=PATH不可用/覆盖配置/只安装未可调用。dispatch 后保存 agent_id，wait/respond/result/close 严格按 ZAS lifecycle 执行。

## S06 — 更新自动完成安全排空、daemon 激活和受管 Codex 同步
- Implementer: [@impl_large](subagent://impl_large)
- Depends on: S05

### 业务合同

**Outcome / authority / necessity**：U06、P11—P14、D07/D08；npm升级不止文件换了，而是自动把候选payload、实际daemon、已登记Codex安装副本协调到可验证状态，活跃任务默认不丢失。更新未完全成功必须准确显示原因/恢复办法。

**Primary owner**：`cli/install/reconcile.mjs`唯一协调owner；daemon只提供既有task状态、drain gate与一次性updater触发，不成为第二套安装状态机。

**Frozen allowed-to-edit**：S05的`cli/install/**`、`cli/commands/update.mjs`、package lifecycle entry与稳定bin；daemon/service/bootstrap及management RPC中draining/idle事实；core admission/scheduler仅drain拒绝新spawn与现有任务继续所需；contract/status安装版本字段与CLI/MCP状态consumer；受管Codexmanifest/source/refresh owner；`tests/upgrade/**`、安装回归、release兼容fixture；`docs/operations.md`、`docs/acceptance/S06.md`、README。runtime/store schema仅明确的版本兼容metadata，不可趁机迁移旧ZAS数据库。

**Inspect-only/exclusions**：两provider内部协议、已完成权限/结果owner、用户其他插件与host进程。禁止默认kill任务、强退Codex、修改enabled、自动跨不兼容schema回滚、全局原子事务宣传、重复updaterdaemon、自动推registry或provider升级。

**Allowed structural changes**：现有installation记录增加candidate/active/phase/各caller结果；同一安装lock；版本化不可变payload与active pointer；**一次性**reconcile/updater入口，可由daemon在排空且resources_reaped后触发；受管文件有界补偿。旧activeNode脚本/native文件独立于npm即将被替换的package目录，不能让排空中的旧进程读取一半新JS。候选只读取启动/兼容信息直到确认激活，不在旧daemon活跃时迁移DB。

**Invariant / safe state**：normal npm hook与explicit update走同一reconcile；staged→draining→activating→healthy或partial/failed清晰可见。draining拒绝newspawn，但保留wait/respond/cancel/result/close；既有queued消息按已admitted任务合同处理，不额外接新业务send来无限延长排空，具体拒绝code在本节同schema确定并回归。默认不取消任务；超时pending继续保留旧控制路径。只有 `--cancel-active --yes` 同时存在才显式取消后排空。旧payload保留到新health验证与受管补偿安全结束。

**Activation contract**：payload完整校验→拿installation锁→识别实际running identity→设置drain→无active task且resources_reaped→唯一once-updater选择activepayload/服务重启→验证运行版本与新连接RPC兼容→官方方式同步每个注册Codex副本→结果分项。无runningdaemon时按已初始化服务期望启动/重启；未init不擅自创建服务。旧的已连接MCP facade若不能跨版本兼容，返回明确reconnect/reload需要，不错解响应；不强退宿主。daemon失败可回已验证oldpayload/service；DB不兼容则停在recoverable failed并保留数据，不能伪称成功回滚。

**AC / fault matrix**：
- npm两版fixture的正常闲置升级：包/activepayload/实际daemon/每个受管plugin安装版本一致；不是只比较package.json。daemon被重启有新PID+version证据。
- 真实活跃任务升级：普通输出/工具权限等待继续，wait/respond仍可用；新spawn拒绝；任务自然结束且reaped后无需再次人工update即可自动激活。排空超时pending、用户显式forcecancel、升级期间新send的既定拒绝均可判错。
- 并发npm/update/init只一个协调者；candidate损坏/错误架构/不完整文件不切active；launcher或Node路径变化不让MCP永久指向已删文件。
- stop/activate/start各阶段crash、stalephase重入、旧daemon未死、新daemon健康失败、不可降级schema、plugin refresh失败和一个CODEX_HOME冲突；数据/其他配置不损，结果partial/failed真实可恢复。
- npm ignore-scripts不触发hook：首次之后CLI发现版本差异并reconcile；仅装包、从未再执行产品命令的场景明确“未激活”，不能声称已自动重启。Codex从npmsource不跑script时同样处理。
- 多home含disabledplugin保留enabled状态；source新但cache旧须failed/pending；安装副本新但运行宿主未重载显示reload_required/unknown，而非强制关闭host。

**Create-before-run / section oracle**：先创建 `tests/upgrade/reconcile.test.mjs`、`tests/upgrade/drain-live.test.mjs`、`tests/upgrade/codex-refresh.test.mjs`、`tests/upgrade/recovery.test.mjs`；计划运行 `node --test tests/upgrade/*.test.mjs`、fresh-install回归、受影响cargo tests、整合CLI/MCP contract。真实macOS受控prefix/home进行连续两个本地npm pack版本升级、LaunchAgent状态/实际payload验证、真实两provider任务+真实Codex调用；registry发布不是验收必需，只有用户另行授权才publish。

**Review**：HIGH_RISK；assurance **TWO**。跨npm/daemon/launchd/Codex并发失败需要freshindependentFINAL；无需额外“第七节最终修复全部”。

**Routing**：profile `impl_large`；analogue=旧installer提供ownership/S05freshreconcile，但没有完整协调升级范例；ambiguity=structural unknown受D07/D08合同限制；semantic_hops=npmhook→reconcile→drain→processidentity→serviceactivation→Codexcache/host→status；state_coupling=跨进程+多文件补偿+已有task；oracle_strength=必须faultinjection+actualhost，初始partial；novel_reasoning=yes，不能用本地文件原子rename替代端到端成功；context_scope=installation所有owner及受影响admission/facade，不遍历provider内部；false_clean=活跃任务丢失/无法启动/误覆盖用户插件。

## 集成、接受与交付

下面是组装后的验证矩阵，不是额外实现section。测试创建owner已分配到S01—S06，集成只跑真实组合和有因果关系的回归，不新增“证据系统”。

| 组合 | 必须证明 | 最晚owner |
|---|---|---|
| ZCode × CLI/MCP/Codex | 10工具、四模式既有合同、wait/page/queue/request/recovery不退化 | S02；S05真实host |
| DSH × CLI/MCP/Codex | supported build/strictplan、model precedence、显式unsupported、同生命周期与真实结果 | S04；S05真实host |
| 跨provider与test-only第三adapter | 同workspace独占、core无二选一分支、第三adapter不改全部handler | S04 |
| 配置/认证/探测 | local/auth/hi分层、scope/环境正确、status不付费、无agent默认报错 | S03/S04 |
| 全新npm/macOS/GUI | prefixbin+绝对稳定MCP入口、nativepayload、真实plugin发现、多home幂等 | S05 |
| npm更新/活跃任务/故障 | 自动排空激活重启、受管plugin同步、partial/reload真实、回滚有界 | S06 |
| 文件/授权/证据 | owner分离、超大文件例外清晰、旧repo不动、无secret、所有claim对应实际检查 | 每节touchedowner |

完成条件：所有parent ACCEPTED（含sharedchildreconciliation和必要freshFINAL）；所有已批准U/B/P验收达成；源码/测试/CLI/MCP/plugin同一个assembledhead；真实支持矩阵记录版本、平台与actual结果；没有未处理的安全、auth、安装、权限缺口。未运行live、未经过要求的independentreview或未批准需求，**不能写feature DONE**。不把计划validator、npmexit0、model输出“完成”互相替代。

完成交付包含release artifact/tgz、源码与LICENSE/UPSTREAM、公开CLI/MCP/agent-capability文档、安装/升级/错误恢复说明、已验收版本矩阵、remainingunsupported边界和审计记录；当前本包仅包含其计划与研究，不包含这些未制作的产品release。
