//! JS/TS module specifier resolution. `import { x } from '../util/helper'`
//! carries a relative path that must be resolved against the importing file's
//! directory plus extension probing to a concrete indexed file, so the indexer
//! can bind the import edge to the right definition instead of a path-proximity
//! guess (the Python analog is `python_modules::resolve_python_module_targets`).
//!
//! Only RELATIVE specifiers (`./`, `../`) are resolved here. Bare specifiers
//! (`react`, `lodash`) and tsconfig path aliases are left to the existing
//! name-based handling / `<external>` sentinels — resolving them needs
//! node_modules / tsconfig context the indexer does not have.

use std::collections::{HashMap, HashSet};

/// Candidate file extensions probed for a relative specifier, in
/// TypeScript-first resolution order (a `.ts` next to a stale `.js` wins).
const JS_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs"];

/// Resolve a relative ES module specifier to the indexed file it refers to.
/// Returns the normalized repo-relative file path present in `file_set`, trying
/// `<base>`, `<base>.<ext>`, then `<base>/index.<ext>`. None for bare specifiers
/// or when no candidate is indexed.
pub(super) fn resolve_js_specifier_path(
    specifier: &str,
    importer_rel_path: &str,
    file_set: &HashSet<String>,
) -> Option<String> {
    // Only relative specifiers are file-resolvable without node_modules/tsconfig.
    if !(specifier.starts_with("./") || specifier.starts_with("../")) {
        return None;
    }

    // Base = importer's directory joined with the specifier, then normalized
    // (collapse `.` and `..`). importer_rel_path is repo-relative with '/'.
    let importer_dir = match importer_rel_path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "", // importer at repo root
    };
    let joined = if importer_dir.is_empty() {
        specifier.to_string()
    } else {
        format!("{}/{}", importer_dir, specifier)
    };
    let base = normalize_rel_path(&joined)?;

    // Specifier already names an indexed file (explicit extension).
    if file_set.contains(&base) {
        return Some(base);
    }
    // `<base>.<ext>`
    for ext in JS_EXTENSIONS {
        let cand = format!("{}.{}", base, ext);
        if file_set.contains(&cand) {
            return Some(cand);
        }
    }
    // `<base>/index.<ext>`
    for ext in JS_EXTENSIONS {
        let cand = format!("{}/index.{}", base, ext);
        if file_set.contains(&cand) {
            return Some(cand);
        }
    }
    None
}

/// Resolve import targets to nodes named `target_name` in the specifier-resolved
/// file. None if the specifier doesn't resolve to an indexed file or no matching
/// node exists there (caller falls back to default name-based resolution).
pub(super) fn resolve_js_module_targets(
    specifier: &str,
    importer_rel_path: &str,
    target_name: &str,
    file_set: &HashSet<String>,
    name_to_ids: &HashMap<String, Vec<i64>>,
    node_id_to_path: &HashMap<i64, String>,
) -> Option<Vec<i64>> {
    let file = resolve_js_specifier_path(specifier, importer_rel_path, file_set)?;
    let all_ids = name_to_ids.get(target_name)?;
    let targets: Vec<i64> = all_ids
        .iter()
        .filter(|id| node_id_to_path.get(id).map(|p| p == &file).unwrap_or(false))
        .copied()
        .collect();
    if targets.is_empty() { None } else { Some(targets) }
}

/// Normalize a repo-relative path, collapsing `.` and `..` segments. Returns
/// None if it escapes the repo root (a leading `..` with nothing to pop), which
/// cannot correspond to an indexed file.
fn normalize_rel_path(path: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                out.pop()?;
            }
            s => out.push(s),
        }
    }
    Some(out.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn resolves_parent_relative_with_ts_extension() {
        let files = fs(&["src/util/helper.ts", "src/core/caller.ts", "src/core/helper.ts"]);
        assert_eq!(
            resolve_js_specifier_path("../util/helper", "src/core/caller.ts", &files),
            Some("src/util/helper.ts".to_string())
        );
    }

    #[test]
    fn resolves_same_dir_relative() {
        let files = fs(&["src/core/helper.js", "src/core/caller.js"]);
        assert_eq!(
            resolve_js_specifier_path("./helper", "src/core/caller.js", &files),
            Some("src/core/helper.js".to_string())
        );
    }

    #[test]
    fn resolves_index_file() {
        let files = fs(&["src/util/index.ts", "src/core/caller.ts"]);
        assert_eq!(
            resolve_js_specifier_path("../util", "src/core/caller.ts", &files),
            Some("src/util/index.ts".to_string())
        );
    }

    #[test]
    fn bare_specifier_is_unresolved() {
        let files = fs(&["src/core/caller.ts", "node_modules/react/index.js"]);
        assert_eq!(resolve_js_specifier_path("react", "src/core/caller.ts", &files), None);
    }

    #[test]
    fn escaping_root_is_none() {
        let files = fs(&["caller.ts"]);
        assert_eq!(resolve_js_specifier_path("../../x", "caller.ts", &files), None);
    }

    #[test]
    fn ts_preferred_over_js() {
        let files = fs(&["src/util/helper.js", "src/util/helper.ts", "src/core/caller.ts"]);
        assert_eq!(
            resolve_js_specifier_path("../util/helper", "src/core/caller.ts", &files),
            Some("src/util/helper.ts".to_string())
        );
    }
}
