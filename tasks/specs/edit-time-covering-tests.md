# Spec: 编辑时"覆盖测试"靶向推送 (symbol-level covering-test PUSH)

Status: IMPLEMENTED + SHIPPED v0.72.0 — Rust (`test_callers` on impact CLI+MCP) + JS
(pre-edit-guide covering-tests consumer + `test_targets` measurement). See CHANGELOG v0.72.0.
Author context: 2026-06-24,承接 9 次 adoption 审计的收敛结论。

## 1. 动机 (为什么是这个,不是再改表面)

9 次 adoption 审计 (最近 2026-06-23) 收敛:调用率已近**结构上限**,
唯一有效的价值通道是 **PUSH** (deny-with-answer / SessionStart 注入),模型几乎
不主动 PULL (≈5 次/10 天,且历史"100 CLI"读数是 hook-delivery 仪器假象,已 FIXED
v0.68.0)。把 MCP 功能转成 skill/CLI 提调用率 = 净零到净负 (CLI 早已存在且常驻主推,
主动 PULL 仍 ≈1/10 天)。

但那个"foldability ≈ 24%"是**只在 grep 上量的**。模型的结构性需求还有一大块用
**read/edit fan-out** 表达 (实测某夜 318 read / 379 edit vs 155 grep),没进 grep 桶。
本 spec 针对其中最高频、最贴本项目 fix-test-iterate 工作流的一类:

> "我改了函数 X —— 哪些测试覆盖它,好让我只跑那几个,而不是猜测试名 / 跑全套?"

这是 PUSH-native (走已在工作的 pre-edit-guide 注入),不是再加一个等模型来调的 PULL 工具。

## 2. 现状 (已验证,§8.V1,2026-06-24)

已存在,不要重建:
- 反向 call 图 `get_call_graph` (`src/graph/query.rs`) 是 **symbol 级**,且**已把 test
  caller 单列** —— callgraph 输出 `(N test callers hidden, use --include-tests to show)`。
- `impact <symbol> --json` 已返回 `tests_affected` = `impact.test_count`
  (`src/graph/impact.rs:35` "Count of distinct test/bench callers")。两 surface 同源:
  `src/cli.rs:3042` (cmd_impact) + `src/mcp/server/tools/advanced.rs:262` (tool_impact_analysis)。
- `pre-edit-guide.js` 编辑时已跑 `impact <symbol> --json`,已显示 `(N tests)`
  (line 177/182),已 `recordRecommendation({hook:'edit',...})` (line 173)。
- `affected [files...]` 是**文件级** "changed files → test files to re-run" (`main.rs:311`)。

缺口 (净新的就这一点):反向 call 图算出了 test-caller **身份 (name+file)**,但 impact
只保留 `test_count`、**丢了身份**。实测 `impact get_call_graph --json`:`callers` 数组只含
14 个 production caller,`tests_affected:10` 是裸计数,test 身份不在 JSON 里。pre-edit-guide
因此只能显示死计数 "(10 tests)" —— 不可行动。

## 3. 设计

### 3.1 增量本质
从"显示 N 计数"→"给出覆盖这些测试的**可跑命令**"。纯 query-time,**无新边 / 无新提取
/ 无 INDEX_VERSION / 无 schema bump** (test-caller 身份本就在反向闭包里走了一遍)。

### 3.2 组件

**C1 — Rust: 让 impact 保留 test-caller 身份**
- `src/graph/impact.rs` `ImpactClassification`:除 `test_count` 外保留
  `test_callers: Vec<TestCaller{name, file}>` (现在算完就丢)。封顶 ~20 防 payload 膨胀。
- JSON 两 surface 加 `test_callers: [{name,file}]`,与 `tests_affected` 并列:
  `src/cli.rs` cmd_impact + `src/mcp/server/tools/advanced.rs` tool_impact_analysis (parity)。
- 注意 `test_count` 是**传递闭包**计数 (impact callers 含 depth 2/3) —— "覆盖"语义正确
  (任何执行能到达 X 的测试都算),但热点函数会爆 (e.g. `conn` 132 test callers)。故 C3 必须
  按规模降级,镜像 `formatRecentImpact` 既有的 ">15 → 不列名、提示跑全套" 逻辑。

**C2 — 可跑命令格式化 (最 fiddly 的部分)**
- 每语言 test runner 不同:Rust `cargo test <name...>` (空格分隔 = OR 过滤);
  JS `vitest run <file...>` / `jest <file>`;Python `pytest <file>::<name>`;Go `go test -run`。
- v1 范围决策见 §6 Open Q2。建议 v1 先 Rust (dogfood),其余语言**降级为只列
  test 文件+函数名 + 通用提示**,不硬造可能错的命令 (错命令比不给命令更坏)。

**C3 — pre-edit-guide.js (published-client,HARD-AUTH)**
- 在现有 impact summary 后,当 `test_callers` 非空:
  - `≤ K` (e.g. 8):`  Covering tests (N): fn1 (file1), fn2 (file2) …`
    + `  → Run after editing: <runnable cmd>`
  - `> K`:`  High test fan-out (N tests) — run the module/suite: <module-scoped or full cmd>`
    (不列名,镜像 blast-size scaling)
- 复用现有 cooldown / CODE_GRAPH_INTERNAL / findBinary 管道,**不新增 hook**。

### 3.3 备选 (为什么不选)
- **Option A:编辑时复用 `affected <editedFile> --json` (文件级,零 Rust)**。更便宜,
  但粒度粗 —— 返回"所有 touch 这个文件的测试",对大文件会过度 over-run,模型对宽列表
  信任度低。symbol 级"覆盖 X 的测试"更紧、更可行动。**推荐 symbol 级 (C1–C3)**,文件级
  作为 test_callers 为空时的 fallback。

## 4. AUTH / ship gate (不可跳)
- `pre-edit-guide.js` + `impact --json` 输出形状 = **published-client surface**
  (CLAUDE.md Autonomy 边界) → **hard-AUTH**。JSON 加字段是 additive (低风险) 但仍要 ASK。
- plugin shell 是 **version-gated tarball**:改完 pushmain 不够,要 ship 一个版本 + 用户
  auto-update 才落地 ([[feedback_full_release_flow]] / [[project_competitive_analyses]] shipping-reality)。
- 本 dogfood repo 的 marketplace hook 是 pre-edit 旧版,本地看不到新行为;真实数据只能来自
  consumer 项目 ([[project_conversion_metric]])。

## 5. 测量 (用现有 infra,别重建;诚实边界)
- `recordRecommendation` 事件加 `test_targets: N` (edit hook 已在记录)。
- **想测的**:收到带 test_targets 的注入后,模型下一条 Bash 是否跑了**靶向** test 命令
  (`cargo test <name>` / `pytest …::`) 而非裸 `cargo test` / grep 猜测试名。
- **诚实边界**:现有 observe 机制不解析后续 test 命令形态。v1 **不能自动**判"是否跑了靶向命令";
  要么 (a) 先用现有 answered/fall-through 粗测,要么 (b) 加一个小 bash-observe matcher 认
  "注入后的靶向 test 命令"。建议 v1 走 (a) + 在 consumer 项目读 forward recommendations.jsonl;
  (b) 作为 gated follow-up,有 forward 数据证明值得再加。
- 别在本 repo baseline (noisy dev-on-tool + marketplace hook off)。

## 6. Open questions (落地前确认)
1. **粒度**:symbol 级 (C1–C3,小 Rust 改,推荐) vs 文件级复用 `affected` (零 Rust,粗)?
2. **v1 runner 语言**:只 Rust 先验证,还是一上来覆盖 6 个 full-extraction 语言
   (TS/JS/Go/Python/Rust/Java)?建议只 Rust + 其余降级列名。
3. **测量**:v1 走粗测 (a),还是同时加 bash-observe 靶向匹配 (b)?

## 7. 验收 (Iron Law #2)
- Rust:`impact <sym> --json` 输出含 `test_callers` 列表 (name+file),计数与 `tests_affected`
  一致;cli_e2e 加 `test_*` 守 + parity 测两 surface 同形。lib + cli_e2e + plugin JS 全绿,
  `cargo +1.95.0 clippy` 0 warn。
- JS:pre-edit-guide 单测覆盖 列名分支 / 降级分支 / test_callers 为空 fallback。
- 无 INDEX_VERSION/schema 改动 (断言:无新增 edge/extraction)。
- ship 后:consumer 项目 recommendations.jsonl 出现 `test_targets` 事件 = 落地证据。
