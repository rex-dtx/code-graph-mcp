//! Cross-language drift guard for the test-path predicate.
//!
//! `is_test_path` is reimplemented in four languages, and every one of them is
//! load-bearing:
//!
//! Checked by THIS file: Rust `domain::is_test_path` (the reference), JS
//! `claude-plugin/scripts/pr-impact-comment.js::isTestPath`, and both Python
//! ports in `scripts/embedding_benchmark/`.
//!
//! NOT checked here: the SQL mirror `domain::is_test_node_sql`. It has its own
//! in-crate differential, `domain::tests::test_is_test_node_sql_matches_rust`,
//! which runs the emitted GLOB against in-memory SQLite.
//!
//! Before this file, only the Rust↔SQL pair had a mechanical differential; the
//! others carried hand-maintained case lists, so widening the Rust side left
//! them behind silently. The 2026-07-27 audit found the drift the other way
//! round too: a2855c2 had to hand-sync four files for two predicate tweaks, and
//! the JS mirror shipped that change with zero test increment.
//!
//! Each mirror is executed for real (spawned interpreter, the actual source
//! file) against the shared corpus in `domain::TEST_PATH_PARITY_CORPUS`, and its
//! verdicts are diffed against Rust's.

use std::path::{Path, PathBuf};
use std::process::Command;

use code_graph_mcp::domain::{is_test_path, TEST_PATH_PARITY_CORPUS};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Rust's verdicts — the reference every mirror is diffed against.
fn rust_verdicts() -> Vec<bool> {
    TEST_PATH_PARITY_CORPUS
        .iter()
        .map(|p| is_test_path(p))
        .collect()
}

/// Compare one mirror's verdicts against Rust's and report every disagreement
/// with the path that caused it — a bare count would send the reader hunting.
fn assert_agrees(mirror: &str, got: &[bool]) {
    let want = rust_verdicts();
    assert_eq!(
        got.len(),
        want.len(),
        "{mirror} returned {} verdicts for {} corpus paths",
        got.len(),
        want.len()
    );
    let mismatches: Vec<String> = TEST_PATH_PARITY_CORPUS
        .iter()
        .zip(want.iter().zip(got.iter()))
        .filter(|(_, (w, g))| w != g)
        .map(|(p, (w, g))| format!("{p}: rust={w} {mirror}={g}"))
        .collect();
    assert!(
        mismatches.is_empty(),
        "{mirror} has drifted from `domain::is_test_path`. Every copy of this \
         predicate must classify identically — a mirror that says \"production\" \
         where Rust says \"test\" reports test files as uncovered production code \
         (and vice versa hides real symbols from search).\n  {}",
        mismatches.join("\n  ")
    );
}

/// Run `interpreter` with `code` and parse the printed `true`/`false` lines.
fn verdicts_from(interpreter: &str, args: &[&str], mirror: &str) -> Vec<bool> {
    let out = Command::new(interpreter)
        .args(args)
        .current_dir(repo_root())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {interpreter} for {mirror}: {e}"));
    assert!(
        out.status.success(),
        "{mirror} harness failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| match l {
            "true" | "True" => true,
            "false" | "False" => false,
            other => panic!("{mirror} printed an unparseable verdict: {other:?}"),
        })
        .collect()
}

fn corpus_json() -> String {
    serde_json::to_string(TEST_PATH_PARITY_CORPUS).unwrap()
}

/// The JS mirror ships inside the plugin, so `node` is already a hard dependency
/// of this repo's test suite — no skip path, a missing interpreter is a real
/// environment failure.
#[test]
fn js_mirror_matches_rust_is_test_path() {
    let script = repo_root().join("claude-plugin/scripts/pr-impact-comment.js");
    assert!(script.exists(), "JS mirror moved: {}", script.display());
    let program = format!(
        "const {{ isTestPath }} = require({});\n\
         for (const p of {}) console.log(isTestPath(p));",
        serde_json::to_string(&script.to_string_lossy()).unwrap(),
        corpus_json()
    );
    let got = verdicts_from("node", &["-e", &program], "pr-impact-comment.js");
    assert_agrees("pr-impact-comment.js", &got);
}

/// The two Python mirrors are offline benchmark tooling, so `python3` is NOT a
/// hard dependency of the suite. When it is absent the legs are skipped LOUDLY —
/// a silent skip would read as coverage this file does not have.
#[test]
fn python_mirrors_match_rust_is_test_path() {
    let mirrors = [
        "scripts/embedding_benchmark/build_tier3_slice.py",
        "scripts/embedding_benchmark/diag_retrieval_drop.py",
    ];
    for m in mirrors {
        assert!(
            repo_root().join(m).exists(),
            "python mirror moved: {m} — update this guard rather than dropping it"
        );
    }

    // The mirrors are pure, platform-independent Python, so checking them on the
    // Linux leg is sufficient coverage. `python3` is not a reliable spelling on
    // windows-latest (App Execution Alias stubs spawn but exit non-zero), and a
    // guard that reddens a job for an environment reason trains people to ignore
    // it. Skip that leg by platform, loudly, rather than by accident.
    if cfg!(windows) {
        eprintln!(
            "[predicate_parity] Python mirrors not checked on Windows by design; \
             the Linux leg covers them."
        );
        return;
    }
    if Command::new("python3").arg("--version").output().is_err() {
        // A skip is only acceptable on a contributor box. In CI it would be a
        // guard that reports success while covering nothing — the exact
        // false-clean shape this file exists to prevent — and `eprintln!` is
        // captured for passing tests, so the notice would be invisible there.
        assert!(
            std::env::var_os("CI").is_none(),
            "no python3 on PATH, so the Python mirrors ({}) went unchecked. \
             CI must be able to run them or this guard covers 2 of 4 copies \
             while claiming 4.",
            mirrors.join(", ")
        );
        eprintln!(
            "[predicate_parity] SKIPPED the two Python mirrors ({}): no python3 on PATH. \
             The Rust and JS legs still ran.",
            mirrors.join(", ")
        );
        return;
    }

    for m in mirrors {
        let module = Path::new(m)
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let program = format!(
            "import importlib.util, sys, json\n\
             spec = importlib.util.spec_from_file_location({m:?}.replace('/', '_'), {m:?})\n\
             mod = importlib.util.module_from_spec(spec)\n\
             sys.modules[spec.name] = mod\n\
             spec.loader.exec_module(mod)\n\
             for p in json.loads({corpus:?}):\n    print(mod.is_test_path(p))\n",
            m = m,
            corpus = corpus_json(),
        );
        let got = verdicts_from("python3", &["-c", &program], &module);
        assert_agrees(&module, &got);
    }
}

/// The corpus is only a guard while it still exercises every leg. Rust's own
/// verdicts must contain both classes — an all-negative corpus would make every
/// mirror "agree" with a stub that returns false.
#[test]
fn corpus_exercises_both_verdicts() {
    let v = rust_verdicts();
    let positives = v.iter().filter(|b| **b).count();
    let negatives = v.len() - positives;
    assert!(
        positives >= 20 && negatives >= 15,
        "corpus lost coverage: {positives} positive / {negatives} negative out of {}",
        v.len()
    );
}
