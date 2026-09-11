# 用户原始请求

日期：2026-09-10（America/Edmonton）。以下为本次请求正文，作为需求权威；不把建议误记为用户已批准的要求。

> 我准备基于现在这个zcode-as-subagent项目做一个新的项目，external-subagent，使用当前zcode-as-subagent的cli和mcp的外部会话生命周期管理，外部调用支持扩展，暂定支持zcode和deepseek-harness，zcode已经有官方app-server和本项目的管理能力了，重点调查deepseek-harness官方的app-server/cli能力，有没有第三方的plugin可以fork，或者应该自己写一个plugin。保留其他agent接入能力。
> 另外：本项目有些文件已经到五六千行代码了，新的项目需要规划好文件管理。
> cli管理：
> - 探测本地某个agent是否存在，登录是否有效，hi探测请求
> - 配置agent、默认agent(可不配置，强制spawn时指定)、默认模型（可不配置，由agent自己决定）
> mcp/cli协议
> - spawn时可选指定agent和模型（zcode不支持指定模型）
> - 状态需要列出不同agent的可用性
> 安装和更新：
> 通过npm分发，需要把可执行文件装到常用的PATH，更新需要自动更新可执行文件、重启daemon、自动更新本机已配置好的调用方的plugin(目前只做codex支持)
> 输出：
> - 其他你认为我必要进行的决策
> - Requirements contract
> - SFD 4.5.1标准的PLAN-FULL.md
> - 调研和分析报告

附件：`归档(1).zip`；`sectioned-feature-development(2).zip`。
交付偏好：可下载产物还须保存至用户 Library。
