//! Shared impact-analysis classification.
//!
//! The single source for the prod/test caller partition, route counting, and
//! risk assessment that both impact surfaces apply to a symbol's callers:
//! `cli::cmd_impact` (the `impact` subcommand) and
//! `mcp::server::tools::tool_impact_analysis` (the `get_ast_node`/impact tool).
//!
//! Before this module the two surfaces reimplemented the partition and had
//! drifted: the MCP path counted routes reachable through *test* callers into the
//! risk input (a test-only endpoint inflated the prod blast radius), and only the
//! CLI deduplicated callers. Both now call [`classify_impact`], so the rule lands
//! once. Resolution (fuzzy / qualified) and rendering stay per-surface; only the
//! classification core is shared.

use crate::domain;
use crate::storage::queries::CallerWithRouteInfo;

/// Classification of a symbol's callers for impact analysis. Borrows from the
/// caller slice — the callers stay owned by the calling surface for rendering.
pub struct ImpactClassification<'a> {
    /// Production callers (test/bench excluded), deduplicated by
    /// `(name, file, depth)`, preserving input order. Surfaces render these: the
    /// CLI lists them; the MCP path splits them into direct (`depth == 1`) and
    /// transitive (`depth > 1`).
    pub prod_callers: Vec<&'a CallerWithRouteInfo>,
    /// Count of distinct production-caller files.
    pub affected_files: usize,
    /// Production callers carrying a PARSEABLE route (HTTP endpoints). Routes
    /// reachable only through test callers are excluded (not a production blast
    /// radius), and so are callers whose `route_info` JSON does not parse — so
    /// `route_callers.len()` is the single count that feeds the risk level AND
    /// that both surfaces display (the MCP `affected_routes` array, its summary,
    /// and the CLI count), with no divergence on corrupt/legacy metadata.
    pub route_callers: Vec<&'a CallerWithRouteInfo>,
    /// Count of distinct test/bench callers (`tests_affected` in both surfaces).
    /// Always equals `test_callers.len()` — same dedup, single source.
    pub test_count: usize,
    /// The distinct test/bench callers themselves — the identities behind
    /// `test_count`, deduped by `(name, file, depth)` in input order. Surfaces use
    /// these for edit-time covering-test targeting (which tests exercise the
    /// symbol → a runnable test command) and MAY cap the rendered list, while
    /// `test_count` keeps the true total.
    pub test_callers: Vec<&'a CallerWithRouteInfo>,
    /// Risk level from [`domain::compute_risk_level`], or `"UNKNOWN"` when the
    /// target is a non-function with zero production callers — its real usage
    /// (imports / field access / instantiation / type annotations) is broader than
    /// the call graph, so a `LOW` would mislead. [`Self::type_warning`] carries the
    /// matching explanation in that case.
    pub risk_level: &'static str,
    /// `Some(explanation)` exactly when `risk_level == "UNKNOWN"`; `None` otherwise.
    pub type_warning: Option<&'static str>,
}

/// Classify a symbol's callers into the prod/test partition, route set, and risk
/// level shared by both impact surfaces.
///
/// `callers` is the raw list from `get_callers_with_route_info`; the queried
/// symbol itself (depth 0) is filtered out here. `is_function_like` reports
/// whether the target symbol is a function/method (drives the `UNKNOWN`-risk path
/// for types and other non-function symbols).
pub fn classify_impact<'a>(
    callers: &'a [CallerWithRouteInfo],
    change_type: &str,
    is_function_like: bool,
) -> ImpactClassification<'a> {
    use std::collections::HashSet;

    // Exclude the queried symbol itself (depth 0), then dedup by (name, file,
    // depth). get_callers_with_route_info returns shortest-path-distinct node_ids,
    // but two distinct same-named nodes in one file at the same depth would
    // otherwise be double-counted — dedup keeps both surfaces' counts identical
    // and correct. Test callers are deduped on the same key.
    let mut seen_prod: HashSet<(&str, &str, i32)> = HashSet::new();
    let mut seen_test: HashSet<(&str, &str, i32)> = HashSet::new();
    let mut prod_callers: Vec<&CallerWithRouteInfo> = Vec::new();
    let mut test_callers: Vec<&CallerWithRouteInfo> = Vec::new();
    for c in callers.iter().filter(|c| c.depth > 0) {
        let key = (c.name.as_str(), c.file_path.as_str(), c.depth);
        if domain::is_test_symbol(&c.name, &c.file_path) {
            if seen_test.insert(key) {
                test_callers.push(c);
            }
        } else if seen_prod.insert(key) {
            prod_callers.push(c);
        }
    }
    // Single source: the count is the deduped identity list's length.
    let test_count = test_callers.len();

    let affected_files = prod_callers
        .iter()
        .map(|c| c.file_path.as_str())
        .collect::<HashSet<_>>()
        .len();

    // Prod callers carrying a PARSEABLE route. Two filters in one: (1) a route
    // reachable solely through a test caller is not a production blast radius (the
    // MCP-path drift this module first fixed); (2) the `route_info` JSON must parse.
    // Without (2) the count diverged across surfaces: the MCP path fed
    // route_callers.len() to risk but displayed only the parseable subset
    // (affected_routes.len()), while the CLI counted the raw set — so a corrupt or
    // legacy route_info made risk, the MCP count, and the CLI count disagree.
    // Gating on parseability makes route_callers.len() the single basis for risk
    // AND both surfaces' displayed counts (a route we cannot render must not
    // silently inflate the blast radius either).
    let route_callers: Vec<&CallerWithRouteInfo> = prod_callers
        .iter()
        .filter(|c| {
            c.route_info
                .as_deref()
                .is_some_and(|m| serde_json::from_str::<serde_json::Value>(m).is_ok())
        })
        .copied()
        .collect();

    let type_warning = if prod_callers.is_empty() && !is_function_like {
        Some(domain::NON_FUNCTION_IMPACT_WARNING)
    } else {
        None
    };

    let risk_level = if type_warning.is_some() {
        "UNKNOWN"
    } else {
        domain::compute_risk_level(
            prod_callers.len(),
            route_callers.len(),
            // Both `remove` and `signature` are *breaking* changes: every call
            // site must change or it won't compile/run. `behavior` keeps callers
            // compiling, so only it scales risk by caller count. Treating
            // `signature` like `behavior` (its prior behaviour) made the option a
            // silent no-op.
            matches!(change_type, "remove" | "signature"),
        )
    };

    ImpactClassification {
        prod_callers,
        affected_files,
        route_callers,
        test_count,
        test_callers,
        risk_level,
        type_warning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(name: &str, file: &str, depth: i32, route: Option<&str>) -> CallerWithRouteInfo {
        CallerWithRouteInfo {
            node_id: 0,
            name: name.to_string(),
            node_type: "function".to_string(),
            file_path: file.to_string(),
            depth,
            route_info: route.map(|s| s.to_string()),
        }
    }

    #[test]
    fn excludes_root_and_partitions_prod_vs_test() {
        let callers = vec![
            caller("target", "src/a.rs", 0, None), // root — excluded
            caller("handler", "src/a.rs", 1, None), // prod direct
            caller("helper", "src/b.rs", 2, None), // prod transitive
            caller("test_thing", "tests/a.rs", 1, None), // test
        ];
        let c = classify_impact(&callers, "behavior", true);
        assert_eq!(c.prod_callers.len(), 2);
        assert_eq!(c.test_count, 1);
        assert_eq!(c.affected_files, 2);
        assert_eq!(c.prod_callers.iter().filter(|c| c.depth == 1).count(), 1);
        assert_eq!(c.prod_callers.iter().filter(|c| c.depth > 1).count(), 1);
    }

    #[test]
    fn dedups_same_name_file_depth() {
        let callers = vec![
            caller("dup", "src/a.rs", 1, None),
            caller("dup", "src/a.rs", 1, None), // exact dup — counted once
            caller("dup", "src/a.rs", 2, None), // different depth — kept
        ];
        let c = classify_impact(&callers, "behavior", true);
        assert_eq!(c.prod_callers.len(), 2);
    }

    #[test]
    fn routes_exclude_test_only_callers() {
        // A route reachable ONLY through a test caller must not inflate the prod
        // route count — the MCP-path drift this module fixes.
        let callers = vec![
            caller("prodCaller", "src/a.rs", 1, None),
            caller(
                "test_route",
                "tests/a.rs",
                1,
                Some(r#"{"method":"GET","path":"/x"}"#),
            ),
        ];
        let c = classify_impact(&callers, "behavior", true);
        assert_eq!(
            c.route_callers.len(),
            0,
            "test-only route excluded from prod blast radius"
        );
        assert_eq!(c.test_count, 1);
    }

    #[test]
    fn prod_routes_feed_risk() {
        let callers: Vec<_> = (0..3)
            .map(|i| {
                caller(
                    &format!("c{i}"),
                    "src/a.rs",
                    1,
                    Some(r#"{"method":"GET","path":"/x"}"#),
                )
            })
            .collect();
        let c = classify_impact(&callers, "behavior", true);
        assert_eq!(c.route_callers.len(), 3);
        // >= 3 routes ⇒ HIGH (compute_risk_level).
        assert_eq!(c.risk_level, "HIGH");
    }

    #[test]
    fn corrupt_route_metadata_excluded_so_count_is_consistent() {
        // A prod caller whose route_info isn't valid JSON must NOT count toward
        // route_callers — otherwise risk (route_callers.len()) and the count both
        // surfaces can render (the parseable subset) diverge on corrupt/legacy
        // metadata. One valid route + one corrupt → exactly one counted route.
        let callers = vec![
            caller("good", "src/a.rs", 1, Some(r#"{"method":"GET","path":"/ok"}"#)),
            caller("bad", "src/b.rs", 1, Some("not json{")),
        ];
        let c = classify_impact(&callers, "behavior", true);
        assert_eq!(c.route_callers.len(), 1, "only the parseable route counts");
        assert_eq!(c.route_callers[0].name, "good", "the corrupt-metadata route is dropped");
    }

    #[test]
    fn non_function_zero_callers_is_unknown() {
        let callers = vec![caller("MyType", "src/a.rs", 0, None)]; // only root
        let c = classify_impact(&callers, "behavior", false);
        assert_eq!(c.risk_level, "UNKNOWN");
        assert!(c.type_warning.is_some());
    }

    #[test]
    fn function_zero_callers_is_low_not_unknown() {
        let callers = vec![caller("orphanFn", "src/a.rs", 0, None)];
        let c = classify_impact(&callers, "behavior", true);
        assert_eq!(c.risk_level, "LOW");
        assert!(c.type_warning.is_none());
    }

    #[test]
    fn removal_forces_high() {
        let callers = vec![caller("one", "src/a.rs", 1, None)];
        let c = classify_impact(&callers, "remove", true);
        assert_eq!(c.risk_level, "HIGH");
    }

    #[test]
    fn signature_change_is_breaking_and_forces_high() {
        // A single prod caller would be LOW for a behaviour change, but a
        // signature change breaks that call site, so it must escalate to HIGH —
        // matching `remove`. Previously `signature` was a silent alias of
        // `behavior` and reported LOW here.
        let callers = vec![caller("one", "src/a.rs", 1, None)];
        assert_eq!(classify_impact(&callers, "behavior", true).risk_level, "LOW");
        assert_eq!(classify_impact(&callers, "signature", true).risk_level, "HIGH");
        assert_eq!(classify_impact(&callers, "remove", true).risk_level, "HIGH");
    }

    #[test]
    fn captures_test_caller_identities_not_just_count() {
        // Edit-time covering-test targeting (a PUSH feature) needs the test
        // callers' identities — name + file — to build a runnable test command,
        // not just a count. classify_impact must retain them, deduped, with the
        // list length equal to test_count (single source of truth).
        let callers = vec![
            caller("target", "src/a.rs", 0, None), // root — excluded
            caller("prod_handler", "src/a.rs", 1, None), // prod caller — not a test
            caller("test_alpha", "tests/a.rs", 1, None), // test caller (direct)
            caller("test_beta", "tests/b.rs", 2, None), // test caller (transitive)
            caller("test_alpha", "tests/a.rs", 1, None), // exact dup — counted once
        ];
        let c = classify_impact(&callers, "behavior", true);
        assert_eq!(c.test_count, 2, "two distinct test callers");
        assert_eq!(
            c.test_callers.len(),
            c.test_count,
            "an identity is retained for every counted test caller"
        );
        let names: Vec<&str> = c.test_callers.iter().map(|tc| tc.name.as_str()).collect();
        assert!(names.contains(&"test_alpha"), "got {names:?}");
        assert!(names.contains(&"test_beta"), "got {names:?}");
        assert!(
            !names.contains(&"prod_handler"),
            "a prod caller must not appear among test callers"
        );
        // The file identity is carried — it's what lets a surface build the
        // per-language test command (e.g. `cargo test`, `pytest <file>`).
        let alpha = c
            .test_callers
            .iter()
            .find(|tc| tc.name == "test_alpha")
            .unwrap();
        assert_eq!(alpha.file_path, "tests/a.rs");
    }
}
