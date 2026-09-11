# external-subagent — 研究、需求与实施计划

**2026-09-10 · America/Edmonton · 规划交付，不是实现完成声明**

推荐：保留Rust生命周期核心，把ZCode降为provider adapter；DSH使用官方 `dsh --profile acp`。先验证受管profile足以提供已批准权限，必要时才补窄策略插件；不fork反向Codex桥或GUI层充当外部会话服务器。

## 文件导览

| 文件 | 用途 |
|---|---|
| [DECISIONS.md](DECISIONS.md) | D01—D10待批准选择、单一推荐值与拒绝该值的影响 |
| [REQUIREMENTS.md](REQUIREMENTS.md) | U显式用户要求、B源码继承、P提案，CLI/MCP/权限/更新完整合同 |
| [PLAN-FULL.md](PLAN-FULL.md) | SFD4.5.1格式；6个业务父section、4个child、每unit模型/范围/测试/review |
| [RESEARCH-REPORT.md](RESEARCH-REPORT.md) | 官方ACP/SDK/CLI、第三方插件、权限/auth/result/npm/Codex分析及一手来源 |
| [FILE-ARCHITECTURE.md](FILE-ARCHITECTURE.md) | 目标目录、owner/依赖边界、旧大文件迁移与规模规则 |
| [SOURCE-EVIDENCE.md](SOURCE-EVIDENCE.md) | 本次附件身份、Git基线、源码定位、97文件比对/40缺失tests证据 |
| [ORIGINAL-REQUEST.md](ORIGINAL-REQUEST.md) | 用户原始请求；计划外的需求权威 |
| [FEATURE-STATE.md](FEATURE-STATE.md) | 真实当前状态、尚未调用的review路径、下一合法动作 |
| [VALIDATION.md](VALIDATION.md) | 本包实际文本/结构检查与明确NOT_RUN项 |

## 建议阅读顺序

先看DECISIONS与报告结论，随后确认REQUIREMENTS，再由本机主控使用PLAN-FULL。文件架构和source evidence用于代码handoff，不需每个implementer重复阅读全部报告。

本计划遵循附件SFD4.5.1的结构与路由；**未执行native和ZAS独立PLANreview**。机械validator通过不等于SFD批准，也不等于产品验收。DSH源码版本是master manifest所见，不是npm latest或本机版本；S01负责真实固定版本与wire验证。

## 在新仓库中的放置方式

获准创建新仓库后，将本包Markdown原样放入新仓库 `.agent-work/` 作为本feature的唯一canonical计划目录；相对链接保持有效，不额外维护另一份根目录PLAN。不要把这些文件覆盖到旧ZAS的 `.agent-work`，也不要复制旧仓库的.git/秘密/历史任务记录。

PLAN中产品源码、tests、docs路径均相对**新仓库根**；其 `.agent-work` 中记录的是本feature状态。执行者先填真实新路径、base/head、owner决定，再做独立review。源码基线从旧repo固定HEAD导出；缺失tests可从附件Git对象恢复，见SOURCE-EVIDENCE。

提供的ZIP仅包含规划Markdown，不包含上传仓库、上游源码、账号密钥或运行日志。本包另保存到用户Library；实际存储结果以对话中的Library文件记录为准。
