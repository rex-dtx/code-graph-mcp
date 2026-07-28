# 独立评审 findings — 2026-07-27(来自 claudemd 会话,交接文件)

**来源**:claudemd 项目的会话误把贴过来的本仓转录当成自己的欠账,对本仓当时的未提交工作树(staged+unstaged)跑了一次独立评审(fresh-subagent,read-only,未改任何文件)。范围错误已向用户澄清;findings 本身经评审 agent 实测验证,按用户指示落到本文件,由本仓的会话处置。

**快照警告**:评审期间工作树在被另一会话并发编辑(`cli.rs` 187→244 added lines 等)。所有 finding 以最终读取状态为准(`cli.rs` blob `a11579c`, `resolve.rs` blob `ba805b1`);若已继续演进,逐条先对源码核实再动手(参 claudemd 侧 `feedback_audit_finding_verify_vs_source.md`:子代理 finding 可假阴/假阳)。评审自跑:`cargo test` 1332 passed / 0 failed / 5 ignored;`node --test` 64 passed / 0 failed。

## Findings(严重度降序)

### 1. HIGH — `/doctor` 静默重建损坏的 `~/.claude/settings.json` 并报告 "auto-repaired"
`claude-plugin/scripts/lifecycle.js:1163-1238`(`scanForBrokenPaths` + `healthCheck`)、`claude-plugin/scripts/doctor.js:217-242`。本次 diff 新造的触发路径:`scanForBrokenPaths()` 对损坏文件返回 `settings-unusable` → `healthCheck()` 对任何非空 issue 列表调 `install()` → `readSettingsForWrite()`(line 120)备份后**从 `{}` 重建** → 重扫返回 `[]` → 报 `repaired: true`。评审用真代码 + 临时 HOME(内容 `{ "model": "opus", }`)复现:用户的 `model`/`env`/`permissions`/`enabledPlugins`/自有 hooks 只剩在 `.corrupt-<stamp>` 备份里,doctor 输出表格不提该备份(备份通知走 `install()` 的 stderr)。三处新注释与实现相反:`lifecycle.js:1169`、`doctor.js:235-237`、`doctor.js:590-594` 都声称 "install() 拒绝碰该文件",实际只拒绝无法备份的子集;`doctor.js:234-241` 的 `unusable` 分支对常见 unparseable 情形不可达(只在备份本身失败时触发);窄分支触发时 `runRepairs` 的 `hooks-invalid`(`doctor.js:560-578`)给出错误补救建议(reinstall)。

### 2. MEDIUM — cfg 谓词守卫漏掉两种常见三方写法
`src/parser/relations/rust.rs:451` + `508-525`(`in_cfg_predicate`)。守卫认 attribute 祖先或 macro 字段文本 == `cfg`,两种活形态绕过:(a) `cfg_if::cfg_if! { if #[cfg(not(windows))] { … } }` — token 树里的 `#[cfg]` 不是解析出的 attribute,最近 `macro_invocation` 是 `cfg_if`;(b) `core::cfg!(...)` / `std::cfg!(...)` — macro 字段是 `scoped_identifier`,文本 `core::cfg` ≠ `cfg`。已在临时 fixture(定义 `fn any` / `fn not`)+ debug 二进制上复现假 caller 边。本仓无 cfg_if(src/tests grep 0 命中),受影响的是被索引的用户项目(libc / rayon / std)。

### 3. MEDIUM — `doctor.js:251` 仍是旧 collapsed-`null` 读法
`const settings = readJson(settingsPath()) || {};` → `surveyHookCoverage({})` → 损坏的 settings 被诊断为 `missing-hooks-in-settings`,与 `lifecycle.js:1171` 刚修的是兄弟位置,漏改。同一次 doctor run 会给出互相矛盾的诊断。

### 4. MEDIUM — 新源码漂移守卫只认一种取值写法、一层目录
`tests/hardening.rs:475-476`(`every_tool_path_arg_read_is_normalized_in_source`)。只匹配字面量 `args["path"]` / `args["file_path"]`,`read_dir` 不递归;`args.get("path")` 写法(同文件 `overview.rs:249` 已用于 `deps_depth`)或 `tools/` 子目录新工具可绕过。当前全仓 0 处此类读法,守卫现在完整,窄的是抗漂移能力;`checked >= 6` 下限只数括号形式。

### 5. LOW — cfg 测试的"负对照"没走它声称保护的路径
`src/parser/relations/tests.rs:5805-5810`。`if cfg!(any(unix)) { compute(); }` 的 `compute()` 在 if 块里,是普通 `call_expression`,到不了 `extract_rust_macro_token_call`。把 `in_cfg_predicate` 换成 `return true` 该断言照过;真正接住的是 `:1436` / `:1462` 两个测试。

### 6. LOW — predicate_parity 的 Python 跳过只在 `--nocapture` 下可见
`tests/predicate_parity.rs:125-132`。`eprintln!` 后 `return` → 测试 PASS,libtest 吞掉通过测试的 stderr → 无 python3 的 runner 只见 `... ok`,跳过不可见。JS 腿(`:70`)无跳过路径直接 panic,不对称。实际风险低(GitHub 三个 runner 镜像都带 python3),但"loud skip"的声称在效果上不成立——同类教训见 claudemd 侧 `feedback_portability_probe_behavior_not_format.md`(loud-skip 不硬失败)。

### 7. LOW — `windows_absolute` 词法守卫误拒一种合法 Unix 相对路径
`src/cli.rs:431-439`。首段形如 `X:` 即拒(如仓根文件 `a:b.rs`,Unix 合法)。现有 near-miss 测试(`src/a:b.rs`、`a/b:c`)冒号都不在首段,该形状无测试钉住。注释里写了取舍,极罕见。

## 裁决记录(评审时点)

- **INDEX_VERSION 52→53**:必需且已就位(`src/domain.rs` 当时 `:152` = 53)。两条独立理由:cfg 谓词 calls 边被移除(旧索引残留);`<external>` std-import 哨兵节点+边为新增(v52 整个丢弃)。
- **`<external>` 泄漏**:全 14 个查询面(show/callgraph/impact/refs/map/overview/centrality/hotspots/cycles/stats/search/dead-code/deps/health-check)扫过,全干净;`resolve.rs:394` SQL 已排除;`prune_import_contradicted_call_edges`(`resolve.rs:448-496`)行为与设计声明一致。
- 两个 commit-gate 脚本 staged `100755`(HEAD 是 `100644`),ci.yml exec-bit 检查可过。

## 处置建议

#1 是数据破坏面(用户 settings.json 自有配置),建议 tag 前修;#3 与 #1 同根同修;#2 影响被索引的下游仓,可与下一次 INDEX_VERSION 变更同车;#4-#7 酌情。


---

## 处置结果(本仓会话,2026-07-27)

全部 7 条已按**当前**源码逐条复核后处置(该文件基于 09:30 快照,期间工作树持续演进)。

| # | 复核结果 | 处置 |
|---|---|---|
| 1 HIGH | **成立**。实测 `doctor` 打印 `Hooks ✅ 1 issue(s) auto-repaired`,同时用户 `model`/`env` 已从活文件消失(grep 计数 0),只存在于 `.corrupt-*`,而表格只字不提备份。 | `readSettingsForWrite` 改为返回 `{settings, backedUpTo}`;`install`/`update` 回传 `settingsRebuiltFrom`;`healthCheck` 回传 `rebuiltFrom`;doctor 该行改为 `⚠️ settings.json was unusable and has been REBUILT — your original is at <path>`。**注释更正记录有误 —— 实际只改了 2/3**(第四轮评审查出):`doctor.js` 两处已准确(其分支只在 install() 真的拒绝时可达),`lifecycle.js:1191` 的「install() correctly refuses this file」当时未改,现已改正并写明「只拒绝无法备份的子集;可写+不可解析时是备份后重建」。 |
| 2 | (a) `cfg_if!` 支已由结构化属性规则(匹配 token 流里的 `#[ … ]`)接住,不再依赖宏名。(b) **成立**:`core::cfg!(any(unix))` 漏 `any`、`std::cfg!(not(test))` 漏 `not`。 | `in_cfg_predicate` 改比较宏路径**末段**(`core::cfg` → `cfg`)。 |
| 3 | **成立**。`doctor.js:251` 仍是 `readJson()||{}` → 对不可用文件报 "missing 6/6 settings.json entries",与同一张表里正确的 "settings.json unusable" 自相矛盾。 | 改用 `readJsonResult`,corrupt 时报 `not determinable`,且**不**挂 `fixId`(否则同一修复被驱动两次、issue 被计两次)。 |
| 4 | 成立但当前全仓 0 命中。 | 扫描式 guard 已扩到 `args.get(...)` 形态(`ignore_paths`)与 CLI 侧同类站点。递归子目录未做,记为已知窄面。 |
| 5 | **成立**。`if cfg!(..) { compute(); }` 里的 `compute()` 是普通 `call_expression`,到不了被测 pass;把整条 pass stub 成 `None` 该断言照过。 | 负控换成 `recovered()`(位于宏**参数**内);实测 stub 整条 pass 后该断言转红。 |
| 6 | 成立。 | 已改:Windows 按平台显式跳过(纯 Python 镜像由 Linux 腿覆盖),非 Windows 的 CI 缺 python3 则硬失败。 |
| 7 | **成立**。`a:b.rs` 确在索引中却被拒,且原 near-miss 负控冒号都不在首段。 | 谓词收紧为冒号后必须跟分隔符或到串尾;负控换成 `a:b.rs`/`z:name`/`a:b/c.rs` 三个冒号确在 byte 1 的形态。 |

**顺带修掉的一处**:`doctor.js` 第 7 项的 `catch { /* probe failed — skip */ }` 会静默吞掉整行 —— 它当场吞了一个 `readJsonResult` 未导入的 `ReferenceError`,表格看起来完整而该检查根本没跑。改为把探针失败本身作为一条 warn 报出。

**新增回归测试**:`lifecycle.e2e.test.js` 两条(重建须报告为破坏性 + 须给出备份路径;不可读时不得声称 hooks 缺失),回退 `doctor.js` 后双双转红。`parser/relations/tests.rs` 覆盖 `core::cfg!`/`std::cfg!`,单独回退末段匹配即红。

**基线**:fmt 干净;clippy 双特性集各 0;Rust no-default 1339/0、embed 1356/0;JS 893 中 891/2(2 例为已定位的 `adopt` 本地环境失败,非本批引入)。


---

## 第四轮评审(batch4)对上述处置的复查 — 又 2 条 HIGH

处置本身经独立复查,6/7 行属实,但**修复面选错了一半**,另牵出一条已发布契约违反:

- **[HIGH,实证] 诚实修在了低频路径,漏了高频路径。** `install()` 新增的 `settingsRebuiltFrom` 只有 `doctor` 消费;`session-init.js` 在 `syncLifecycleConfig` 里调 `install()` **七次**、`update()` 一次,全部丢弃返回值。实测:损坏 settings.json + SessionStart → exit 0、stdout 只有 project map、活文件 3318 B 且 `model`/`permissions` 计数为 0,通知只在 stderr 而 SessionStart hook 丢弃 stderr。**修**:本文件内统一包 `installReporting`/`updateReporting`(不逐点补),通知走 **stdout**(Claude Code 实际呈现的通道,与 `injectProjectMap` 同)。
- **[HIGH,实证] `doctor --check-only` 会重写用户 settings.json。** CHANGELOG v0.82.1 明文承诺「read-only」「never reaches runRepairs」—— 写操作根本不在 `runRepairs`,而在 `runDiagnostics` → `healthCheck()` → `install()`。四种状态实测被重写(36 B → 3318 B,model 消失),报告还说「Run without --check-only to fix」。**修**:`checkOnly` 现在传进 `runDiagnostics`,check-only 时只 `scanForBrokenPaths()`、不走自动修复半边。四态复测全部 UNCHANGED + 零备份。
- [MED] 见上表更正。 [LOW] `in_cfg_predicate` 的 doc 注释挂到了 `in_raw_attribute_tokens` 上,且第二段描述的是已废弃的实现;两段都已重写归位。
- [MED,接受不改] REBUILT 行无 `fixId` → 退出码 0→1、摘要 `0/N addressed`。退出码 1 是**对的**(用户必须手工并回配置),改前的 exit 0 才是缺陷;`--check-only` 修复后,「建议你去跑破坏性模式」那句组合已不可达。措辞偏保守,记为已知。
- [LOW,接受不改] 不再静默的 `catch` 会让编程错误状态永不 exit 0 —— 正是它的目的。

**新增回归测试**:`--check-only` 四态零写入零备份;SessionStart 重建必须在 stdout 报告并给出备份路径。回退 `doctor.js`+`session-init.js` 后,该文件 4 条转红。
