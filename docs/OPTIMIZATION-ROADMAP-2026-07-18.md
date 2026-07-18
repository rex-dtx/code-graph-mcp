# 优化方案与路线图（2026-07-18）

> 依据：v0.99.0 发版后当日专项审计（2 个并行探查 agent：Rust CLI/MCP 面实证扫描 + JS hook 面代码核对）、
> 30 天真实使用数据、deferred 清单（D#54/73/77/106）、2026-07-17 全量审计遗留项。
> 主题：本周从 3 起真实误判（worktree statusline 消失、grep 零命中静默、复合命令 deny 折叠）中
> 提炼出的缺陷类 —— **"诚实信息未到达消费者"（disclosure gap）** —— 的全量清点与修复计划，
> 加上 Claude 编程工作流的功能缺口盘点。

## 0. 数据锚点

- 本仓库 30 天工具使用（`outcome --since 30`）：77 次 cg 调用，采纳 22/77=29%。
  `grep_cli` 48 次（19 采纳）为绝对热路径；`show_cli` 15（3）；`callgraph_cli` 7（0）；其余个位数。
- 全量审计基线：v0.97.1 ~8.4/10；姊妹洞（sibling-hole）类连续 4 轮居首要发现来源。
- 本周已修的同类三例：worktree 解析（v0.99.0）、grep -c 零行 + BRE 提示（v0.99.0）、
  deny 头行 compound-tail 标记（Unreleased `ecd54f4`）。

## 1. 披露修复批（Phase 1 — 建议立即做，单批可完成）

同一根因、同一修法：**把只在 stderr 的判别信息落进 stdout/JSON 带内**，
向 `affected`（`not_indexed` 字段）/`trace`（`message` 字段）的最佳实践对齐。
全部为行为修复（恢复"披露"这一已声明设计意图），无 INDEX/SCHEMA bump。

| # | 严重度 | 位置 | 缺陷 | 修法 | 验收 |
|---|---|---|---|---|---|
| 1.1 | HIGH | `cmd_search`/`cmd_ast_search`（cli.rs:3102+/3199+） | filter 清空后 JSON 输出裸 `[]`，与真零命中字节级相同；"N 个候选被过滤器移除"仅 stderr | JSON 空结果时改为自描述对象（如 `{"results":[],"filtered_out":263,"filter":"language: ruby"}`）；text 模式 stdout 同步一行 | `2>/dev/null` 下两种空可区分；`test_cli_json_empty_*` 同形扩展 |
| 1.2 | MED | `dead-code`（cli.rs:6069） | 低于 `--min-lines` 阈值的孤儿静默；JSON `[]` 无标记 | JSON 加 `below_threshold_count`；text stdout 提示 rerun `--min-lines 1` | 实测 3 个真孤儿场景（`--min-lines 1` 对照）有带内提示 |
| 1.3 | MED | `show`/`overview`/`callgraph` 空结果 | 未命中输出裸 `[]`/空容器，无 `error`/`message` 字段（与 impact/refs/trace/affected 不一致） | 统一带内契约：未命中 → `{"error":"Symbol not found","symbol":…}` 类自描述 | 三命令 miss 场景 JSON 自描述；对照 impact 契约测试 |
| 1.4 | MED | 查询时新鲜度（cli.rs:4583 `FreshOutcome::disclose`） | "行号可能过期"仅 eprintln+tracing，`--json` 管道下不可见 | JSON 顶层加 `"freshness_partial":true`（注意 compact 白名单同步——v0.97.1 已在此摔过） | RESYNC_BUDGET=0 负控测试断言字段出现 |
| 1.5 | MED | `cycles`（cli.rs:6256/6278） | `truncate(50)` 后用截断后长度打印 "(50 found)"，无 truncated 标记 | 截断前记总数；text `(showing 50 of N)` + JSON `truncated:true`（对照 callgraph `limit_hit`） | >50 环 fixture 断言两种表面都披露 |
| 1.6 | MED | `post-grep-inject.js:313` | 非 hits 路径不写 recommendations.jsonl，漏斗无法区分"hook dark（binary 缺失）vs 真无结果"（姊妹 hook 都记录 `answer.status`） | 补 `recordRecommendation(… reason: answer.status)` 对齐 pre-grep/pre-read | JS 测试断言 unavailable 路径落记录 |
| 1.7 | LOW | `map` deps cap 30 无 "+N more"；`search` limit 无 "more may exist"；JS 注入文案截断提示仅尾部 | 顺手补标记（map 修、search/JS 视 token 预算裁量） | — |

**明确不修（设计已接受，记录在案）**：
- callgraph/show/impact/overview/trace/refs 的"符号不存在 → exit 1"：复合命令里读作工具故障，
  但六者对真零结果给 exit 0，语义正确；改动会破坏脚本分支习惯。
- 已核对干净的面（审计 clean 清单）：callgraph 截断披露、affected 带内契约、grep 各披露、
  JS deny 三 builder、statusline 状态机、cg-answer 四 runner 部分结果处理。

## 2. 功能缺口（Phase 2 — 真功能，按价值排序）

### 2.1 Rust 路由提取（最大缺口，含 dogfood 盲区）
`trace` 的路由提取仅覆盖 TS/JS（Express/Connect）、Go（net/http）、Python（Flask/FastAPI）；
**axum/actix 零覆盖 —— 本产品自身是 Rust 仓库，trace 对自家代码无用**。
- 范围建议：先 axum（`Router::new().route("/path", get(handler))` 链式 + merge/nest 前缀合成），
  actix 次之；Java/Spring 另立条目（注解式，独立解析器工作量）。
- 影响面：`INDEX_VERSION` bump（产新边）——按 [index-version-seam] 纪律；
  搭 v0.98.0 punch-list #5 "下次 INDEX bump 顺风车批"（Kotlin/Swift implements 区分等）一起 bump，摊薄全量重建成本。
- 验收：本仓库（如未来加 web 面）或 axum fixture 上 `trace` 返回 handler 链；
  route-imported-handler 跨文件场景（IDX v29 同类）有测试。

### 2.2 Worktree 的 Rust 读侧（D#106）
v0.99.0 已做 JS 读侧（statusline/hooks 回落主 checkout 索引）；Rust CLI/MCP 在 worktree 内
仍会冷建全量索引。查询类命令（callgraph/show/search/grep/…）应 mirror `worktreeMainRoot()`
的 gitdir 解析回落主索引；写侧（index/serve）保持建本地索引。
- 约束：metrics-isolation + `$HOME` bound fixtures（v0.75.2）必须保持绿；两侧契约差异注释互指已在位。

### 2.3 `export *` / `import * as` barrel 再导出解析
[const-export-no-edge] 记忆标注的 Meta 级残留：v0.90.0/v0.92.0 修了 destructuring/named 形态，
namespace 与 star re-export 仍断边 —— JS 大仓库 import 图完整性的主要剩余缺口。INDEX bump，
同样可搭 2.1 的顺风车批。

### 2.4 MCP 面补 `centrality`
CLI 有、MCP 无（[features-v053-v054]）。小工作量；注意 MCP `instructions` ~1500 字节预算
（[mcp-instructions-budget] 编译期 assert）与 routing_bench 前后对比（工具描述属 LLM-visible metadata）。

## 3. 仪表与卫生（Phase 3 — 支撑决策质量）

| 项 | 内容 | 来源 |
|---|---|---|
| 3.1 | outcome 两个已知漏计：同 assistant turn 批量 cg 调用漏计（forward-scan 提前 break）；`ADOPTION_WINDOW` 未标定 | [outcome-measurement] open |
| 3.2 | `pending_unresolved_calls` 无界增长：attempt-counter + SCHEMA bump（非 evict-after-full-index） | D#77 |
| 3.3 | callgraph 0/7 采纳率调查：先判读数（callgraph 属信息型使用，采纳定义=后续 Read/Edit 命中返回路径，可能天然低），再决定是否动输出 | 本次 30 天数据 |
| 3.4 | 跨项目 SessionStart 注入成本：收窄激活边界（记忆标注最高杠杆） | [cross-project-interference] |
| 3.5 | release.yml 冷缓存 ~9min 关键路径 | D#73 |

## 4. 路线图

```
Phase 1  披露修复批（1.1–1.7）           ── 1 个会话批量完成；全部无 bump；发一个 patch/minor
Phase 2a Rust 路由提取（axum 先行）       ── INDEX bump 顺风车批主项（+2.3 barrel re-export 同批）
Phase 2b Worktree Rust 读侧（D#106）     ── 独立小批，无 bump
Phase 2c MCP centrality                  ── 独立小批；routing_bench 回归
Phase 3  仪表修正（3.1→3.3）+ 卫生（3.2 SCHEMA bump 单独批）+ 3.4/3.5 择机
```

排序理由：Phase 1 直接服务日常 LLM 消费路径（本周 3 起误判全属此类），修法统一、风险最低；
Phase 2a 是唯一"新能力"级缺口且需要 INDEX bump 编排（顺风车摊薄成本）；仪表（3.1/3.3）决定
我们之后还能不能相信采纳率读数，先于大的排序/输出改动。

## 5. 风险与纪律备忘

- 每一项 JSON 字段新增 → 检查 MCP compact 白名单（[compact-field-allowlist]，两次审计都中招）。
- 每一项新边/新节点 → `INDEX_VERSION` bump + 重嵌入编排（[vec-coverage-orphan-race]）。
- deny/hint/工具描述文案改动 = LLM-visible metadata → routing_bench 前后跑 + 负面引导禁忌
  （[negative-steering-backfire]：「DO NOT for X」反拉低 20pp）。
- 平行路径类改动（fallback/默认臂/多语言 dispatch）→ 枚举全部姊妹路径 + 每类 1 测试 + 实测 RED 负控
  （[v095-arch-lock] META 纪律；姊妹洞连续 4 轮居首）。
