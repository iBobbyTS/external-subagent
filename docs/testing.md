# 测试策略：按修改面选择测试块（test:affected）

日常验证用 `npm run test:affected`：选择器把「本次修改面」映射到声明式测试块，只运行受影响的块，并明确报告 NOT RUN 的块与原因。集成收口、发版或跨切面改动仍跑全量（`cargo test --workspace` + 逐目录 `node --test`）。

## 为什么全量慢（本机实测样本，2026-10-01）

| 块 | 热态耗时 | 慢的原因 |
|---|---|---|
| node:upgrade | 目录 ≈199s（单测 drain-live 223s 有档；另含 60–130s 用例与树内 cargo build） | vA→vB 场景在复制的源码树里 cargo build + 活体 drain |
| node:install | ≈31s（热）/ 分钟级（冷） | `build-native-payload.mjs --if-stale` + `npm pack`（prepack 仍会增量构建）；测试强制 debug profile |
| rust daemon lib | 105–165s（415 测试，进程密集） | 大量真实子进程收割/协议测试 |
| node:integration | ≈36s | 真实 daemon + fake server |
| 其余 node 目录 | cli 5s / contract 1–2s / platform 1s / provider 7s / acceptance <1s | 便宜 |

历史印象里"node 全量很慢"主要由三件事构成：install/upgrade 的 cargo 构建、drain-live 长测（main 基线即红）、以及**多文件并行运行时的跨文件干扰（挂起/假失败）**。因此选择器的 node 块一律**逐文件串行**且 stdin 置 ignore——这只消除单次选择器内的并行干扰与继承 stdin 挂起，不是"杜绝一切挂起"的承诺。

## 用法

```bash
npm run test:affected                      # 默认 base=main：merge-base..HEAD ∪ 工作树改动 ∪ untracked
node scripts/test/affected.mjs --base main --dry-run   # 只看选择与原因
node scripts/test/affected.mjs --fast      # 跳过慢车道（install/upgrade），NOT RUN 明示
node scripts/test/affected.mjs --only node:contract    # 直接跑指定块
node scripts/test/affected.mjs --list      # 列全部块与成本
```

- `--files a b …` 向采集集合**追加**路径；`--all` 与 `--fast` 互斥；未知块/无效 ref 非零退出。
- 无 main 时需显式 `--base`；base 与 HEAD 相同（如在 main 上干净树）→ 空选择属正常语义。
- 前置构建：node:cli 与 node:integration 依赖 `target/debug/external-subagentd`，选择器会先 `cargo build --bin external-subagentd`（一次、去重）；失败则依赖块记 BLOCKED、总退出码非零，其余独立块继续。
- 覆盖消重：`rust:workspace-full` 覆盖全部 rust 子块、`rust:daemon-full` 覆盖 daemon 子块；被覆盖块列入 NOT RUN（注明被谁覆盖），昂贵测试不会在一次调用里重复跑。
- 失败呈现：任一块失败继续跑完其余块，摘要列 FAIL/BLOCKED 与原始退出码。**已知基线红项不做自动豁免**：productization（版本断言 0.1.3 vs 0.1.4）与 drain-live（live 环境引导）——对照记录判断是否同签名，勿把新失败当既有问题。

## 块表（维护点 1）

见 `scripts/test/affected.mjs` 的 `BLOCKS`（id、命令、成本估计、slow/prebuild/covers 标注）。成本为热态粗估，仅用于排序与预期管理。

## 修改面 → 块映射（维护点 2）

见同文件 `IMPACT_RULES`（顺序即优先级，先命中先赢）。要点：

- Rust 产品/嵌入输入（crates/**、profiles/dsh、plugins/dsh-write-guard、schema/observation.schema.json）→ 对应 rust 块 + `node:integration` + **慢车道**（install/upgrade 会重新构建载荷——诚实映射，用 `--fast` 显式跳过）。
- daemon 子域同覆盖同名根 `.rs`（scheduler.rs/rpc.rs/mcp.rs/agent_status.rs/…）；未列举的 daemon 文件保守选 `rust:daemon-full`。
- `cli/**` 保守映射全部 node 块（contract/integration/provider/upgrade 都真实消费 CLI）；`bin/**` 加 cli/contract/install/platform。
- `docs/acceptance/**` 与 plugin manifest 被 acceptance 测试读取 → `node:acceptance`；其余 docs/README/.agents 白名单免测。
- fixtures 按真实消费者映射（restart-daemon→node:cli、dsh-acp→node:integration、agent-status→node:contract、versioned-daemon→node:upgrade、subagent-config-matrix→node:cli+rust:daemon-rpc、dsh-hi-probe→node:cli+rust:daemon-adapters）；未列举 fixture 保守全量。
- `tests/<dir>/x.test.mjs` → 对应 node 块；`tests/live-agent/**` → rust:daemon-full。
- **未知路径策略**：只有白名单输出"无需运行"；任何未映射路径选择保守全量并显式标注原因。

## 维护规则

- 新增测试目录或 crate：同步 `BLOCKS` + `IMPACT_RULES` + 本文档；`tests/cli/affected.test.mjs` 有表驱动校验（块表一致性、映射、消重、--fast、rename/untracked 采集）。
- 选择器自身改动 → `node:cli`（其测试位于 tests/cli/）。
- 耗时数字更新时注明机器与冷/热状态；不要把成本估计当性能承诺。
