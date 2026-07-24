# 全面审核报告（2026-07-24）

> 对象：code-graph-mcp v0.104.1（HEAD `39833eb`）+ 工作区未提交改动（5 文件，+298/-4）。
> 方法：6 个并行审核 agent 分维度深查（架构 / 索引与解析 / 存储与搜索 / MCP·CLI / 插件发布链 / 测试质量门），
> 主会话对关键结论逐条交叉复核（clippy P1 已本地原样命令复现；宏启发式 P1 由 agent 在隔离副本实证复现）。
> 所有引用的测试通过数、行号、命令输出均为本次会话新鲜证据，非记忆推断。
> 历史基线：v0.97.1 全量审计 ~8.4/10（见 `docs/OPTIMIZATION-ROADMAP-2026-07-18.md` §0）。

---

## 一、总体结论

**核心引擎已达到生产级水平；当前存在 2 个合入/发版阻塞项，修复后可正常发版。**

- 存储层、MCP 协议层、发布链是三个最成熟的子系统——大量防御逻辑都能追溯到具名的真实事故
  （orphan-vector "daagu" 事件、mmap SIGBUS CI 抖动、schema 漂移砖机、hook 双授权 ping-pong），
  且每个修复都有钉住的回归测试。这是"实战硬化"而非"白板防御"，是生产级代码库最可靠的特征。
- 测试面健康：Rust 侧 1290 通过 / 0 失败 / 5 忽略（17 个测试二进制，~10s）；JS 侧本地合计 ~936 个测试 0 失败
  （含 CI 排除的 install-e2e 39/39、mcp-launcher 6/6）。测试断言行为而非"不崩溃"，
  且带有层级守卫（storage 禁 import graph）、Cargo.toml 守卫（禁 `panic="abort"`）等漂移防护。
- **但当前状态不能直接发版**：① CI 的 clippy 门在已提交代码上是红的（P1-1）；
  ② 未提交的宏调用提取启发式存在实证假边 bug（P1-2），带病合入会污染 callgraph/impact/dead-code 三个核心输出。

| 维度 | 评级 | P0 | P1 | P2 |
|---|---|---|---|---|
| 架构与模块设计 | B | 0 | 1（cli.rs 巨型文件） | 4 |
| 索引管线与解析算法 | B+（管线 A-，新增启发式拖累） | 0 | 2 | 2 |
| 存储（SQLite）与搜索 | **A** | 0 | 0 | 2 |
| MCP 服务器与 CLI 健壮性 | A- | 0 | 0 | 4 |
| 插件 JS 层与发布链 | A- | 0 | 0 | 4 |
| 测试与质量门 | B+（测试 A，门有洞） | 0 | 1（CI clippy 红） | 3 |
| **合计** | — | **0** | **4** | **~19** |

---

## 二、P1 缺陷（按处置优先级排序）

### P1-1 CI clippy 门当前为红（已提交代码）— 阻塞发版

- 位置：`src/indexer/pipeline/resolve.rs:242`、`:249`，两处 `let filtered = …; filtered`（`let_and_return`）。
- 证据（主会话本地复现）：`cargo clippy -- -D warnings`（与 `ci.yml:52` 逐字节一致）
  → `error: could not compile code-graph-mcp (lib) due to 2 previous errors`，exit 101。
- 该代码在 HEAD 中（非工作区改动引入）。ea0166d "make release gate actually gate" 之后门是真门，红即挡发版。
- 修复：两处内联表达式（`self_filter_candidates(...)?` / `path_filter_candidates(...)?` 直接作为分支值），一行改动 ×2。
- 残余不确定性：本地 clippy 1.91 复现，CI 钉 1.95.0；`let_and_return` 是长期稳定的默认 warn lint，几乎必然同样触发，但未在 1.95.0 下确认。

### P1-2 宏调用提取启发式把"模式匹配"误判为"调用"（未提交改动）— 建议阻止合入，补修后再进

- 位置：`src/parser/relations/rust.rs:311-344`（`extract_rust_macro_token_call`），接线于 `relations/mod.rs:410-416`。
- 根因：tree-sitter-rust 的 `token_tree` 子节点是通用 token，**调用与元组变体/元组结构体模式在语法上不可区分**
  （经 tree-sitter-rust 0.23.3 `node-types.json` 核实）。现有排除表（`.`/`::`/`$`/定义关键字/self/Self/_）不覆盖模式场景。
- 实证复现（agent，隔离副本）：`matches!(x, Some(y) if y > 0)` 产出假边 `("calls", <scope>, "Some")`。
  `matches!` / `assert!(matches!(…))` / `debug_assert!` 是极高频惯用法；本仓库自身源码暂未踩中
  （现存 `matches!` 用法均为 `::` 限定形态，已被排除），属"未咬人但必咬"的真 bug。
- 影响：callgraph/impact 出现枚举变体的幻影调用者；仅被模式匹配的变体会被 dead-code 误判为"活代码"——与该特性要修的问题方向相反。
- 新增的 4 个配套测试均未覆盖此形态。建议：合入前补 `matches!` 假边回归测试 + 排除策略
  （如：前一 token 为 `,` 且处于已知模式宏参数位、或标注 CONF_AMBIGUOUS 降置信度而非硬边）。
- 附带 nit：`rust.rs:328` 的 `"$"` 排除分支实际不可达（`$name` 是 `metavariable` 叶节点，不会以 `identifier` 进入该函数）。

### P1-3 符号链接源文件被静默跳过、永不索引

- 位置：`src/indexer/merkle.rs:86`（`scan_directory` 及 cached 变体）。
- 根因：`ignore::WalkBuilder` 默认 `follow_links=false`，symlink 条目 `file_type().is_file()==false`，
  被 `!ft.is_file()` 守卫跳过；且这是该文件中**唯一无 `tracing::warn!` 的跳过路径**（size/读错/哈希错均有日志）——零可观测性。
- 实证复现（agent）：以同配置 walker + symlink `.rs` 文件验证条目被跳过。
- 影响：monorepo 共享包 symlink、软链配置/schema 文件的用户，符号"消失"且无任何提示。
- 修复分两层：最低限度先补一条 warn 日志 + 统计计数（无行为变化）；行为修复（跟随 symlink 或纳入索引）需评估环路防护，可后置。

### P1-4 `src/cli.rs` 巨型文件（8269 行 / 148 个顶层函数）

- 混合参数解析、业务逻辑、输出格式化与 ~1200 行内联测试。内部有 `// --- <subcommand> ---` 分节标记（~20 个子命令），
  组织尚可，但共享 helper（`resolve_project_root` / `normalize_user_path`）的任何修改都在 8K 行文件内进行，
  本次审核发现的"rebase 逻辑两处重复"（见 P2-7）正是这种结构的直接代价。
- 修复：机械拆分为 `src/cli/<subcommand>.rs` + `cli/mod.rs` 重导出。低风险，分节边界已存在。非紧急，但应排期。

---

## 三、P2 观察项（按维度归组）

### 架构（audit：B）
1. `src/mcp/server/mod.rs` 3445 行，单个 `impl McpServer` 块 ~1700 行 / 31 方法；继续增长则拆索引生命周期与工具分发。
2. 15 个 `CODE_GRAPH_*` 环境变量散读于各处，无中央注册表——"程序响应哪些环境变量"不可枚举。建议 `utils/env_config.rs` 收口。
3. `utils/config.rs` 名不副实（实为语言检测注册表，与 `snapshot/config.rs` 的 .code-graph.toml 解析无重复）；建议改名 `language_registry`。
4. `lib.rs` 无 API 门面（14 个裸 `pub mod`），且 Cargo.toml 无 `publish = false` 防误发布护栏。
5. **副产物发现（产品自身 bug 线索）**：`code-graph-mcp map` 报出 `src/embedding → src/indexer/pipeline (3 imports)` 假边，
   grep 证实零命中——自家 import 计数器有伪影，值得立案排查（影响用户对 map 输出的信任）。

### 索引与解析（audit：B+）
6. 【SUSPECTED】cached 增量路径存在 mtime 同 tick 编辑盲区：`scan_directory_cached`（merkle.rs:143-247）按 mtime 跳过再哈希，
   同秒双编辑/粗粒度 mtime 文件系统下改动不可见；此路径接在 watcher 触发与周期 reindex 上（`mcp/server/mod.rs:854`、`:1665`），是热路径。
   `ensure_file_indexed` 的全量哈希兜底覆盖"编辑后立即查询"主流，但后台新鲜度可静默滞后。至少补一行文档注明该权衡。
7. 无持久化的逐文件 parse-error 记录：`files_with_parse_errors` 仅内存聚合计数，事后无法查询"哪些文件因解析错误符号不全"。

### 存储与搜索（audit：A — 无 P1，正面结论摘录见附录）
8. 9 段迁移块手工枚举（`db.rs:208-252`）而非注册表循环——有 `fresh_schema_matches_fully_migrated_schema` 漂移测试兜底，仅维护性观察。
9. vendored sqlite-vec 的 blake3 重签是人工信任边界（gate 提示如何重签但无法验证重签是否经过审查）——流程性而非代码缺陷。

### MCP·CLI（audit：A-）
10. **rebase 逻辑两处手工重复**：`normalize_user_path_from`（cli.rs:411-433）与 `cmd_grep`（cli.rs:2436-2449）各自实现
    "cwd 缺失 + root 存在 → rebase + stderr 提示"，注释互相引用但未抽公共函数——未来单边修改必然重新引入不对称。建议抽 helper。
11. 【SUSPECTED】rebase 启发式按文件系统存在性裁决，同名碰撞场景可误判：cwd=`<root>/src` 下输入 `utils.rs` 意指 `src/utils.rs`
    但其刚被删除、而无关的 `<root>/utils.rs` 恰好存在 → 静默 rebase 到错误文件。现有测试仅覆盖"倍增"形态，未覆盖"碰撞"形态，建议补一例。
12. 【SUSPECTED】rebase 的存在性检查用原始 `project_root` 而非 `effective_read_root` 的 worktree 映射根——
    linked worktree 无自有索引时，存在性判断看 worktree 磁盘、后续查询打主 checkout 索引，可能分歧。属 D#106 已接受权衡的既有类别，
    但新启发式增加了一处 root/checkout 不一致敏感点；同类既有问题：`cmd_grep` 逐匹配 staleness 检查（cli.rs:2830）在 worktree 下可能标错。
13. 数组形 JSON 输出（search/show/overview 等）的 partial-refresh 披露仅走 stderr，无带内通道——代码自述的已知缺口，
    与 `docs/OPTIMIZATION-ROADMAP-2026-07-18.md` §1 披露修复批重叠，不重复展开。

### 插件与发布链（audit：A-）
14. 版本漂移仅在发布后被捕获：build job 从 tag checkout 编译在 sync-versions 之前，smoke-verify 是 `needs: publish`。
    建议 publish 前加一步"制品 `--version` == tag"断言（便宜且关闭最后一个次序缝）。
15. `mcp-launcher.test.js`（stub/proxy 交接这一承重路径）被 ci.yml 与 release.yml 双双排除（依赖 gitignored 的 dev .mcp.json），
    本地 6/6 通过但零 CI 覆盖。建议做 fixture .mcp.json 使其进 CI。
16. 无 `cargo fmt --check` 门（任何 workflow / pre-commit 均无），且当前已有漂移（本次实测 `cargo fmt --check` exit 1，
    漂移在测试文件）。样式级,但会持续腐化。
17. first-party GitHub Actions 钉可变 major tag（checkout@v6 等），与已对第三方 actions 采用的 SHA-pin 姿态不一致。
18. 结构性提醒（非缺陷）：settings.json hook 层"双授权源互相自愈"设计是全库补丁密度最高区域（RCA 2026-07-24 类 ping-pong 反复出现），
    当前由 `hook-orphan-dedup.test.js` 等钉住收敛性，但它是下一个现场回归最可能的产地。中期值得考虑单授权源重构。

### 测试与质量门（audit：B+）
19. 性能回归无硬门：criterion bench 不进 CI；routing-bench.yml 是独立 workflow、瞬时 API 失败时 skip-and-exit-0 且非 required check——
    没有任何性能回归会实际挡合入。
20. 语言 fixture 覆盖 ~11/19：edge_coverage.rs 对 6 个全量提取语言有逐语言边数基线，其余浅提取语言（c/cpp/bash/html/css 等）无显式集成 fixture。
21. `.unwrap()` 表面计数 1570 具误导性——几乎全部在 `#[cfg(test)]` 区；生产路径抽查纪律良好
    （db.rs 测试区前 0 处；serve loop 用毒化恢复的 `lock_stdout`；受守卫的 unwrap 均核实有前置检查）。风险评估：低。

---

## 四、生产就绪评估（针对"达到生产级使用水平没有"）

**结论：核心达标，两个阻塞项修复前不应发版；修复成本合计约半天。**

达标依据（均有本次新鲜证据）：
- **数据完整性**：外键级联删除（文件删→节点→边零孤儿，有测试）；WAL + busy_timeout + flock 单写者 + 只读副本的多进程模型健全；
  损坏自恢复（0 字节/截断/malformed 各有测试）；orphan-vector 三层防御（AD 触发器 + 插入前存在性检查 + IMMEDIATE 事务兜底扫）。
- **协议健壮性**：逐请求 catch_unwind + 启动任务独立 catch_unwind；10MB 上限用 read_until 规避 UTF-8 边界 panic；
  超长行循环排空防解析失步；tracing 仅 stderr，stdout 全走互斥 SharedStdout——未发现任何协议污染路径。
- **注入安全**：FTS5 token 白名单化（`[A-Za-z0-9_]`）+ 短语引号包裹；LIKE 走 `escape_like` + 显式 ESCAPE；未发现注入面。
- **供应链**：无 npm postinstall；二进制走预构建平台包；运行时下载 sha256 先验后 chmod/exec + 原子改名；
  vendored C 与模型权重双哈希钉（build 期 sha256 + 运行期 blake3）；第三方 actions SHA-pin。
- **发布门（ea0166d 后）**：publish 前跑全量 JS 套件（含 install-e2e 对实建制品）；版本一致性三层独立校验；发布次序 npm 最后。
- **崩溃一致性**：批级 SAVEPOINT（500/批）使索引中途崩溃止于最后已提交批；rebuild 单事务防外部读者见半空索引。

未达标项（即上文 P1-1/P1-2）修复后，本仓库的发版流程（含 smoke-verify 三 OS 端到端）足以支撑持续生产发布。
对照历史基线 v0.97.1 ~8.4/10：存储/协议/发布链较当时进一步硬化（新增 schema 漂移测试、hook 收敛测试、真门 release gate），
新增风险集中于未合入的工作区改动——这正是审核应该拦截的位置。

---

## 五、建议行动清单（按优先级）

| # | 级别 | 行动 | 成本 |
|---|---|---|---|
| 1 | P1 | 修 `resolve.rs:242/249` let_and_return（内联表达式），恢复 CI clippy 绿 | ~5 分钟 |
| 2 | P1 | 宏调用启发式：合入前补 `matches!` 模式假边处理 + 回归测试（或先降为 CONF_AMBIGUOUS 边） | ~1-2 小时 |
| 3 | P1 | merkle symlink 跳过：先补 warn 日志 + 计数（无行为变化），行为修复另立条目评估环路防护 | 日志 ~15 分钟 |
| 4 | P2 | rebase 逻辑抽公共 helper（关闭 cli.rs 两处重复），顺手补"同名碰撞"回归测试 | ~1 小时 |
| 5 | P2 | release.yml publish 前加"制品 --version == tag"断言 | ~15 分钟 |
| 6 | P2 | mcp-launcher 测试做 fixture 进 CI；ci.yml 加 `cargo fmt --check`（先 `cargo fmt` 清存量漂移） | ~1 小时 |
| 7 | P2 | 排查 `map` 的 embedding→pipeline 假 import 边伪影（自家产品输出可信度） | 待估 |
| 8 | P1(排期) | cli.rs 拆分为 `cli/<subcommand>.rs` 模块 | 机械,可分批 |
| 9 | P2(排期) | mtime 同 tick 盲区文档注明；parse-error 持久化列（可搭下次 SCHEMA bump 顺风车） | — |

与既有规划的关系：本报告不重复 `OPTIMIZATION-ROADMAP-2026-07-18.md` 已立案项（披露修复批、Rust 路由提取、
worktree Rust 读侧 D#106、barrel 再导出）。上表 #4/#12 与 D#106 存在交集，修复时应互相对照。

### 处置记录（2026-07-24 同日修复批）

行动清单 #1–#7 与 #9a 已修复并验证（同日落地，见工作区改动）：

| # | 处置 | 验证 |
|---|---|---|
| 1 | `resolve.rs` 两处内联 | `cargo clippy --all-targets -- -D warnings` exit 0（原 exit 101） |
| 2 | 宏启发式加大写首字母排除（模式/构造器名不发 calls 边） | 新增 `test_rust_macro_pattern_match_not_a_call`；4 个宏测试全绿 |
| 3 | symlink 跳过加聚合 warn + 候选识别（行为不变） | 新增 `test_scan_directory_skips_symlinked_file_and_candidate_detection`；行为修复记 defer D#15 |
| 4 | 抽 `is_cwd_anchored` + `note_root_rebase` 共享 helper | 新增同名碰撞固化测试；normalize_user_path 相关 12 测试全绿 |
| 5 | release.yml publish job 加"制品 --version == tag"前置断言 | 步骤位于一切 publish 动作之前 |
| 6 | ci.yml 加 fmt 门；`cargo fmt` 清存量漂移；mcp-launcher 测试重新纳入 ci/release glob（其排除理由已过时——dedup 测试早已自包含） | `cargo fmt --check` 干净；JS 套件（新 glob）894 测试 893 通过 / 0 失败 / 1 有意跳过 |
| 7 | 根因：`use std::fs;` 的裸尾段以 metadata:None 进入全局裸名解析，绑定全库唯一同名符号 `fn fs`（#[cfg(test)] 助手）→ 13 条幻影边污染 4 个模块对。修复：`std`/`core`/`alloc`/`proc_macro` 根的 use 声明整条跳过（parser 层）；INDEX_VERSION 51→52（与宏调用提取共乘一次重建） | rebuild-index 后 `map` 实测 `embedding→pipeline` 边消失；新增 `test_rust_std_root_use_skipped_entirely` |
| 9a | `scan_directory_cached` 文档注明 mtime 同 tick 盲区 | — |

修复后全量验证：Rust 测试 1290 → **1294 通过 / 0 失败**；clippy 全目标 `-D warnings` 通过；fmt 干净。
未动项：#8（cli.rs 拆分,排期）、#9b（parse-error 持久化列,搭下次 SCHEMA bump）、
纵深项"结构性关系候选池排除 is_test 节点"（需给 Phase 2 的 name map 加 is_test 数据管道,记 defer D#14）。

---

## 附录：各维度审计人结论原文摘要

- **存储与搜索**："unusually mature subsystem… I found no P0s [and no P1s]"——迁移真实且幂等、RRF 混合项数学有界性有证明与测试
  （blend 严格小于任意相邻 rank 的 RRF 间隙）、HashMap 平局非确定性已用 node_id tiebreak 修复并以 64 轮循环测试钉住。
- **MCP·CLI**："defensively engineered well past what I'd expect… reads as code that has actually been in an incident and been
  hardened afterward"；对未提交 rebase 修复的评价："net improvement that introduces a small amount of new, mostly-narrow risk"。
- **索引与解析**："nearly every non-obvious decision is backed by a named regression test tied to a specific past incident…
  the one place that maturity gap shows is exactly the brand-new, uncommitted heuristic"。
- **插件与发布链**："I would ship on this chain"（在 F1 次序缝之外）。
- **架构**："A-level foundation… held to a B by cli.rs at 8269 lines"。
- **测试与质量门**：测试成熟度 HIGH（断言行为、带漂移守卫、~10s 全绿），但"fast/cheap gates (fmt, benches) aren't wired into
  required CI, and clippy — which IS wired — is currently failing on committed code"。
