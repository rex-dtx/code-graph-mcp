/// Languages a file can be detected as — the output set of `detect_language`.
/// Canonical list: when adding a language, add it here AND to `detect_language`.
/// The `every_supported_language_has_consistent_config` test (parser::lang_config)
/// asserts each entry resolves a tree-sitter grammar (parser::languages::
/// get_language) and a round-tripping lang_config static_name, so a forgotten
/// lang_config arm — which silently makes `config.name == "X"` guards never fire
/// (feedback_lang_config_default_name) — fails the build instead of an indexed
/// repo silently extracting nothing for that language.
pub const SUPPORTED_LANGUAGES: &[&str] = &[
    "rust", "typescript", "tsx", "javascript", "go", "python", "java",
    "c", "cpp", "html", "css", "csharp", "kotlin", "ruby", "php", "swift",
    "dart", "markdown", "bash", "json",
];

pub fn detect_language(path: &str) -> Option<&'static str> {
    let p = std::path::Path::new(path);
    // file_stem() returns None for paths without a filename component;
    // dotfiles like ".gitignore" are filtered by extension() returning None.
    let _stem = p.file_stem()?.to_str()?;
    let ext = p.extension()?.to_str()?;
    match ext {
        "rs" => Some("rust"),
        "ts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "go" => Some("go"),
        "py" | "pyi" => Some("python"),
        "java" => Some("java"),
        "c" | "h" => Some("c"),
        // `.hh`/`.hxx` are common C++ header spellings; `.hxx` in particular
        // pairs with the already-supported `.cxx` source — without it those
        // headers were silently never indexed (symbols missing, #includes to
        // them unresolved). `.h` stays C: the C-vs-C++ `.h` ambiguity is
        // unresolvable from the extension alone, and the c/cpp pair is treated
        // as cross-compatible (is_compatible_lang) so C++ code in a `.h` still
        // links to its `.cpp`.
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some("cpp"),
        "html" | "htm" => Some("html"),
        "css" => Some("css"),
        "cs" => Some("csharp"),
        "kt" | "kts" => Some("kotlin"),
        "rb" => Some("ruby"),
        "php" => Some("php"),
        "swift" => Some("swift"),
        "dart" => Some("dart"),
        "md" | "mdx" | "markdown" => Some("markdown"),
        "sh" | "bash" => Some("bash"),
        "json" => Some("json"),
        _ => None,
    }
}

/// True when `dep_path` is a plausible same-language dependent of `root_path`.
/// Drops cross-language bare-name resolution artifacts (e.g. a Rust file "calling"
/// a JS `update` via name-based resolution) and the synthetic `<external>` bucket.
/// Shared by the dependency-graph tool, `deps`, and `affected` so the cross-language
/// filter stays identical across all three reverse-dependency consumers.
pub fn is_compatible_lang(root_path: &str, dep_path: &str) -> bool {
    if dep_path == "<external>" {
        return false;
    }
    let root_lang = detect_language(root_path);
    let dep_lang = detect_language(dep_path);
    match (root_lang, dep_lang) {
        (None, _) | (_, None) => true, // unknown language → keep (conservative)
        (Some(a), Some(b)) if a == b => true,
        // JS/TS family can cross-reference
        (Some(a), Some(b))
            if matches!((a, b),
                ("javascript" | "typescript" | "tsx", "javascript" | "typescript" | "tsx")) =>
        {
            true
        }
        // C/C++ family can cross-reference
        (Some(a), Some(b)) if matches!((a, b), ("c" | "cpp", "c" | "cpp")) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_language_from_extension() {
        assert_eq!(detect_language("src/main.rs"), Some("rust"));
        assert_eq!(detect_language("app.ts"), Some("typescript"));
        assert_eq!(detect_language("app.tsx"), Some("tsx"));
        assert_eq!(detect_language("index.js"), Some("javascript"));
        assert_eq!(detect_language("main.go"), Some("go"));
        assert_eq!(detect_language("app.py"), Some("python"));
        assert_eq!(detect_language("Main.java"), Some("java"));
        assert_eq!(detect_language("main.c"), Some("c"));
        assert_eq!(detect_language("main.cpp"), Some("cpp"));
        assert_eq!(detect_language("shape.hpp"), Some("cpp"));
        assert_eq!(detect_language("shape.hh"), Some("cpp"));
        assert_eq!(detect_language("widget.cxx"), Some("cpp"));
        assert_eq!(detect_language("widget.hxx"), Some("cpp"));
        assert_eq!(detect_language("index.html"), Some("html"));
        assert_eq!(detect_language("style.css"), Some("css"));
        assert_eq!(detect_language("Program.cs"), Some("csharp"));
        assert_eq!(detect_language("install.sh"), Some("bash"));
        assert_eq!(detect_language(".github/release.bash"), Some("bash"));
        assert_eq!(detect_language("package.json"), Some("json"));
        assert_eq!(detect_language("image.png"), None);
    }

    #[test]
    fn test_detect_language_edge_cases() {
        assert_eq!(detect_language("Makefile"), None);
        assert_eq!(detect_language(".gitignore"), None);
        assert_eq!(detect_language("file.test.ts"), Some("typescript"));
        assert_eq!(detect_language("path/to/no_ext"), None);
    }

    #[test]
    fn is_compatible_lang_filters_cross_language() {
        assert!(is_compatible_lang("a.rs", "b.rs"), "same language kept");
        assert!(is_compatible_lang("a.ts", "b.js"), "JS/TS family cross-ok");
        assert!(is_compatible_lang("a.tsx", "b.ts"), "TSX/TS family cross-ok");
        assert!(is_compatible_lang("a.c", "b.cpp"), "C/C++ family cross-ok");
        assert!(!is_compatible_lang("a.rs", "b.py"), "Rust↔Python dropped");
        assert!(!is_compatible_lang("a.ts", "b.rs"), "TS↔Rust dropped");
        assert!(!is_compatible_lang("a.rs", "<external>"), "<external> dropped");
        assert!(is_compatible_lang("a.rs", "data.unknownext"), "unknown ext kept");
    }
}
