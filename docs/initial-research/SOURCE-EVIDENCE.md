# 输入与源码证据

## 1. 检查方式与身份

检查日期：2026-09-10（America/Edmonton；执行环境文件时间为次日 UTC）。读取上传 ZIP、相关 Markdown/Rust/JavaScript 文本与 ZIP 内 Git 对象；没有在用户机器启动 ZCode、DSH、Codex、npm 安装或 launchd 操作。

| 输入 | 大小（bytes） | SHA-256 |
|---|---:|---|
| 归档(1).zip | 196346164 | 386c03a76d9f3ade0a02f2c8bf0feea5b8bd77e676b4c787f8ea61e540a9ecbe |
| sectioned-feature-development(2).zip | 4627063 | 68580d60d9774d4a4be5ca66062a453030621e70f29b16aa97b5989f29c19dd3 |

这些 hash 只用于报告来源识别，不是新增产品审批／审计 hash 流程。

ZIP 根 `.git/HEAD` 指向 `refs/heads/codex/wait-respondable-20260910`，对应 loose ref 为 `bb45d562671ddbd99637c5680449bc75aedb378b`。根 main loose ref 是 `34a5012856a01c92c359baaa43caaea75716dc3b`；不能使用 packed-refs 中更旧的同名值覆盖 loose ref，也不能把测试 fixture 内部 `.git/HEAD` 当产品 HEAD。

用 `git ls-tree -r HEAD` 与现存 tracked 文件的 Git blob SHA-1 对照：**97 个现存 tracked 文件内容全部匹配 HEAD；40 个 tracked 路径在 ZIP 工作区缺失，全部位于根 tests/。** 这40项包含测试代码、fixture、README和报告模板，并不表示40条独立测试用例。Git 对象仍能列出并恢复这些文件。这个对照不等于完整 working tree clean 证明：历史／未跟踪 `.agent-work` 未纳入 Git clean 声明，也没有验证实际用户本机状态。

新项目导入时可从固定提交导出完整 tracked snapshot，恢复 tests；不要用 `git reset --hard` 修复原仓库，不删除原工作树，不复制历史会话日志或原 `.git` 状态到产品包。

## 2. 关键证据地图（源码路径相对上传仓库根）

| 编号 | 路径／位置 | 已检查事实 |
|---|---|---|
| L01 | README.md | 十工具、默认权限、workspace 独占、result 256KiB、终态 continuation 不支持、平台与安装界限 |
| L02 | .agent-work/REQUIREMENTS.md: 1–46 | 最近 wait-respondable 用户合同：respondable+PENDING；100条投影；旧 Bash-only 已被替代 |
| L03 | crates/zcode-agentd/src/rpc.rs: 28–42 | request frame512KiB、response2MiB、result256KiB、wait290/299、pending100 |
| L04 | crates/zcode-agentd/src/rpc.rs: 1058–1169 | task_wait wake 与完整结果内联／超限第一页回退；不是纯 revision 变化唤醒 |
| L05 | crates/zcode-agentd/src/rpc.rs: 1987–2029 | permission 可响应；unsupported_input 不可响应；不能把各种 PENDING 一律唤醒 |
| L06 | crates/zcode-agentd/src/mcp.rs: 829–882、1034–1046、1192–1267 | 公共 spawn/input、八位数字 agent_id、wait 与 send/respond/result DTO；MCP decision 为 allow/deny |
| L07 | crates/zcode-agentd/src/lib.rs: 1644–1688、1894–1982 | 原模型校验与 ZCode permission 映射；ManagedRuntime 已有生命周期接缝但仍携带 ZCode 类型 |
| L08 | crates/zcode-agent-preparation/src/general.rs: 12–149 | 原 manifest、prepared identity、permission modes 与只读/workspace-write；默认 build |
| L09 | cli/installer.mjs: 1–159 | native payload 路径、受管 plugin identity、Codex CLI 调用、staging／marketplace 与 CODEX_HOME 处理 |
| L10 | cli/main.mjs: 296–341；cli/paths.mjs | init、plugin、status、start/stop、维护命令；macOS application-support/socket/service 的原路径 |
| L11 | package.json；Cargo.toml | 包0.1.0、Node CLI＋Rust workspace、npm bin、测试命令；当前未提供完整多 agent upgrade pipeline |
| L12 | AGENTS.md | 原仓库工程/Advisor决策、串行／无 development worktrees、macOS payload与LaunchAgent规则；新项目须移除旧绝对路径 |

源码的 README 能表明维护者已记录的行为，但本次并没有重跑它宣称的 live test。报告将这类内容标为 SOURCE_INSPECTED，不冒充 OBSERVED。

## 3. 文件规模统计

计数为物理行，含内联测试、空行与注释。

| 文件 | 行数 |
|---|---:|
| crates/zcode-agentd/src/lib.rs | 6145 |
| crates/zcode-agentd/src/mcp.rs | 2475 |
| crates/zcode-agentd/src/rpc.rs | 2435 |
| crates/zcode-agent-store/src/lib.rs | 2365 |
| crates/zcode-agent-preparation/src/policy.rs | 2076 |
| crates/zcode-driver/src/lib.rs | 1989 |
| crates/zcode-protocol/src/lib.rs | 907 |
| crates/zcode-agentd/src/observation.rs | 849 |
| crates/zcode-agent-preparation/src/general.rs | 600 |
| crates/zcode-agentd/src/rpc/unix.rs | 585 |
| cli/main.mjs | 341 |
| cli/installer.mjs | 312 |

## 4. SFD 来源

附件版本文件为 **4.5.1**。实际采用其 `skill/sectioned-feature-development/` 下：SKILL.md、references/section-planning.md、references/model-routing.md、references/external-reviewer-orchestration.md、references/planning/router.md、universal.md、boundary-handoff.md、相关 domain/language/runtime/platform/concern 路由（含恢复原测试所需Python）与 PLAN-FULL.template.md。

`section_plan.py` 是只读结构检查器：能检查 section ID、Implementer 链接、Depends on 与图关系；它明确不等于 approval／actor／scheduling gate。实际运行结果另见 `VALIDATION.md`。

## 5. 已核实与尚未核实

SOURCE_INSPECTED：上述 ZIP、Git refs/blob 对照、文件行数、SFD4.5.1 文件；官方 DSH/Codex/npm 公开文档和源码。

OBSERVED（仅本次本地数据检查）：输入 hash、97个 tracked blob 匹配、40个测试缺失、文档结构校验。没有真实 provider 对话、安装升级或用户机器状态观测。

PLANNED：所有 external-subagent DTO、文件树、配置、状态字段、测试命令和六个业务 sections。

UNKNOWN：用户已安装 DSH/Codex 的确切版本、npm dist-tag 当前值、认证状态、严格 plan 在实际组合上的执行效果、新项目 npm 名称归属、macOS真实安装升级结果。上游 master 文档与当前 npm 发布版不能自动画等号。
