//! Drift guard for the additive `references` axis.
//!
//! Writing a `references` extractor is only half the change — it does nothing
//! until a row in `REFERENCE_PASSES` (src/parser/relations/mod.rs) calls it for
//! some (language, node kind). Before the table existed those rows were
//! hand-written `if config.name == "…" && kind == "…"` blocks, and that shape
//! is the top recurring bug class in this crate: one arm per language per
//! relation, where a forgotten arm is not a compile error but an edge that is
//! silently never emitted. The extractor compiles, its unit tests pass, and the
//! index simply lacks the edges.
//!
//! So: every `extract_*_reference` defined under src/parser/relations/ must
//! appear in the table. Scanning the source is the point — a hand-kept list of
//! expected names here would be the same forgettable arm one layer up.

use std::fs;
use std::path::{Path, PathBuf};

fn relations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/parser/relations")
}

/// Every `pub(super) fn extract_*_reference` defined in the relations module,
/// as (file name, fn name).
fn defined_reference_extractors() -> Vec<(String, String)> {
    let mut found = Vec::new();
    for entry in fs::read_dir(relations_dir()).expect("relations dir must exist") {
        let path = entry.expect("readable dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let file = path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or_default()
            .to_string();
        let src = fs::read_to_string(&path).expect("readable source file");
        for line in src.lines() {
            let line = line.trim_start();
            // Both `pub(super) fn f(` and a same-line signature are covered:
            // the name ends at the paren either way.
            let Some(rest) = line.strip_prefix("pub(super) fn extract_") else {
                continue;
            };
            let Some(name_end) = rest.find('(') else {
                continue;
            };
            let name = format!("extract_{}", &rest[..name_end]);
            if name.ends_with("_reference") {
                found.push((file.clone(), name));
            }
        }
    }
    found.sort();
    found
}

fn table_region(src: &str) -> &str {
    let start = src
        .find("const REFERENCE_PASSES:")
        .expect("REFERENCE_PASSES table must exist in src/parser/relations/mod.rs");
    let rest = &src[start..];
    // The table is a `&[...]` literal terminated by the first `];` at column 0.
    let end = rest
        .find("\n];")
        .expect("REFERENCE_PASSES must be a terminated slice literal");
    &rest[..end]
}

#[test]
fn reference_passes_wire_every_extractor() {
    let mod_rs = fs::read_to_string(relations_dir().join("mod.rs")).expect("mod.rs readable");
    let table = table_region(&mod_rs);

    let defined = defined_reference_extractors();
    assert!(
        defined.len() >= 10,
        "the scanner found only {} reference extractors — it has probably stopped \
         matching the declaration style, which would make this guard vacuous: {:?}",
        defined.len(),
        defined
    );

    let unwired: Vec<&(String, String)> = defined
        .iter()
        .filter(|(_, name)| !table.contains(name.as_str()))
        .collect();

    assert!(
        unwired.is_empty(),
        "these `references` extractors exist but no REFERENCE_PASSES row calls them, \
         so they emit nothing at index time: {:?}\n\
         Add a row to REFERENCE_PASSES in src/parser/relations/mod.rs (language, node \
         kind, extractor) — writing the extractor is only half the change.",
        unwired
    );
}

/// The companion direction: a row naming a function that no longer exists would
/// not compile, so that half is free — but a row whose `langs` list is empty,
/// or whose `kind` is empty, compiles fine and matches nothing.
#[test]
fn reference_passes_have_no_inert_rows() {
    let mod_rs = fs::read_to_string(relations_dir().join("mod.rs")).expect("mod.rs readable");
    let table = table_region(&mod_rs);

    assert!(
        !table.contains("langs: &[]"),
        "a REFERENCE_PASSES row has an empty `langs` list — it can never fire"
    );
    assert!(
        !table.contains("kind: \"\""),
        "a REFERENCE_PASSES row has an empty `kind` — it can never fire"
    );
    assert!(
        !table.contains("extract: &[]"),
        "a REFERENCE_PASSES row has no extractors — it can never emit"
    );
}

#[test]
fn scanner_sees_a_planted_extractor() {
    // Negative control for `reference_passes_wire_every_extractor`: the guard is
    // only meaningful if the scanner would actually notice a new extractor. A
    // planted declaration in a scratch file must be picked up by the same
    // parsing code, otherwise the "no unwired extractors" pass above proves
    // nothing about a real future addition.
    let dir = tempfile::tempdir().expect("tempdir");
    let planted = dir.path().join("planted.rs");
    fs::write(
        &planted,
        "pub(super) fn extract_klingon_type_reference(\n    node: &tree_sitter::Node,\n) {}\n",
    )
    .expect("write planted file");

    let names = scan_file_for_reference_extractors(&planted);
    assert_eq!(
        names,
        vec!["extract_klingon_type_reference".to_string()],
        "the scanner must find a newly declared reference extractor"
    );
}

/// Same parsing rule as [`defined_reference_extractors`], over one file.
fn scan_file_for_reference_extractors(path: &Path) -> Vec<String> {
    let src = fs::read_to_string(path).expect("readable source file");
    src.lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("pub(super) fn extract_")?;
            let name_end = rest.find('(')?;
            let name = format!("extract_{}", &rest[..name_end]);
            name.ends_with("_reference").then_some(name)
        })
        .collect()
}
