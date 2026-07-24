//! Circular import dependency detection via strongly-connected components.
//!
//! In a directed file graph where `a → b` means "file a imports file b", a
//! *circular dependency* is exactly a strongly-connected component (SCC) of two
//! or more files. (A single file importing itself is excluded by the caller; an
//! intra-file edge is not a cross-file cycle.)
//!
//! Cycles are detected over `imports` edges only, NOT `calls`: mutual recursion
//! (a call cycle) is normal and expected, whereas circular *imports* are the
//! architectural smell this surfaces. Most actionable for languages where
//! circular imports are problematic (JS/TS/Python/Go/C/C++).
//!
//! Rust `.rs`↔`.rs` import edges are dropped before detection (see
//! [`is_rust_intra_crate_edge`]): a Rust crate compiles as a unit, so `use`
//! cycles between its modules — a parent module and its submodules, or sibling
//! modules sharing types — are idiomatic and carry none of the load-order hazard
//! this detector exists to surface, and Cargo forbids cross-crate cycles
//! outright. Reporting a crate's own module tree as "4 circular dependencies"
//! is noise that buries the actionable cross-language cycles.
//!
//! Algorithm: iterative Tarjan SCC (O(V+E), no recursion so deep graphs can't
//! overflow the stack), then for each SCC of size ≥ 2 a shortest representative
//! cycle is recovered by BFS so the report can show a concrete `a → b → … → a`
//! path. Deterministic for a fixed input: nodes are indexed in sorted order,
//! adjacency is sorted, and outputs are sorted (size desc, then first file).

use std::collections::{BTreeSet, VecDeque};

/// One circular import dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyCycle {
    /// Every file in the strongly-connected component, sorted lexically.
    pub files: Vec<String>,
    /// A representative shortest cycle through the component as a closed path
    /// `[a, b, …, a]` (the first file is repeated at the end). Starts at the
    /// lexically smallest file in the component for determinism.
    pub path: Vec<String>,
    /// Number of files in the component (`== files.len()`).
    pub size: usize,
}

impl DependencyCycle {
    /// Human-readable headline for one cycle.
    ///
    /// When the representative loop visits every member (`size` distinct files,
    /// i.e. `path.len() == size + 1`) the count and the arrows agree, so it reads
    /// as a plain `N-file cycle: a → b → … → a`. When the strongly-connected
    /// component is larger than its shortest loop (e.g. a 12-file SCC whose
    /// shortest back-edge is just `a → b → a`), labelling that "12-file cycle"
    /// next to a 2-file arrow path is contradictory — so it reads as a
    /// `N-file cyclic group (shortest loop: …)` instead, and callers should list
    /// the full member set separately.
    pub fn headline(&self) -> String {
        let loop_files = self.path.len().saturating_sub(1); // distinct files in the loop
        let arrows = self.path.join(" → ");
        if loop_files >= self.size {
            format!("{}-file cycle: {}", self.size, arrows)
        } else {
            format!(
                "{}-file cyclic group (shortest loop: {})",
                self.size, arrows
            )
        }
    }
}

/// True when both endpoints are Rust source files. Rust compiles a crate as a
/// unit, so intra-crate `use` cycles between modules (a parent module and its
/// submodules, or sibling modules sharing types) are idiomatic and never the
/// load-order hazard this detector targets — and Cargo forbids cross-crate
/// cycles outright. Such edges are dropped before SCC detection so the report
/// stays focused on languages where circular imports are actually problematic.
/// Cross-language edges (e.g. `.rs`↔`.py`) are kept: only same-`.rs` pairs go.
fn is_rust_intra_crate_edge(from: &str, to: &str) -> bool {
    from.ends_with(".rs") && to.ends_with(".rs")
}

/// Detect circular import dependencies in a directed file graph.
///
/// `edges` are `(from, to)` pairs meaning *from imports to*. Returns one
/// [`DependencyCycle`] per strongly-connected component of ≥ 2 files, sorted by
/// size descending then by first file lexically. Self-edges (`from == to`) and
/// Rust intra-crate edges (see [`is_rust_intra_crate_edge`]) are ignored.
/// Deterministic for a fixed input.
pub fn find_cycles(edges: &[(String, String)]) -> Vec<DependencyCycle> {
    // 1. Distinct file names in sorted order → dense indices (deterministic:
    //    smallest index == lexically smallest file). Self-edges and benign Rust
    //    intra-crate module edges are dropped here.
    let mut names_set: BTreeSet<&str> = BTreeSet::new();
    for (from, to) in edges {
        if from == to || is_rust_intra_crate_edge(from, to) {
            continue;
        }
        names_set.insert(from.as_str());
        names_set.insert(to.as_str());
    }
    let names: Vec<&str> = names_set.into_iter().collect();
    let n = names.len();
    if n == 0 {
        return Vec::new();
    }
    let index_of: std::collections::HashMap<&str, usize> =
        names.iter().enumerate().map(|(i, &s)| (s, i)).collect();

    // 2. Sorted, deduped adjacency over dense indices (sorted enables the
    //    binary_search in shortest_cycle and keeps traversal deterministic).
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (from, to) in edges {
        if from == to || is_rust_intra_crate_edge(from, to) {
            continue;
        }
        adj[index_of[from.as_str()]].push(index_of[to.as_str()]);
    }
    for succ in &mut adj {
        succ.sort_unstable();
        succ.dedup();
    }

    // 3. SCCs of size ≥ 2 are the dependency cycles.
    let mut cycles: Vec<DependencyCycle> = tarjan_scc(&adj)
        .into_iter()
        .filter(|comp| comp.len() >= 2)
        .map(|mut comp| {
            comp.sort_unstable();
            let start = comp[0]; // lex-smallest file in the component
            let in_scc: BTreeSet<usize> = comp.iter().copied().collect();
            let path = shortest_cycle(&adj, &in_scc, start);
            DependencyCycle {
                files: comp.iter().map(|&i| names[i].to_string()).collect(),
                path: path.iter().map(|&i| names[i].to_string()).collect(),
                size: comp.len(),
            }
        })
        .collect();

    // 4. Deterministic order: largest component first, ties by first file name.
    cycles.sort_by(|a, b| {
        b.size
            .cmp(&a.size)
            .then_with(|| a.files[0].cmp(&b.files[0]))
    });
    cycles
}

/// Iterative Tarjan strongly-connected-components (no recursion, so a deep import
/// chain cannot overflow the stack). Returns each SCC as a list of node indices.
fn tarjan_scc(adj: &[Vec<usize>]) -> Vec<Vec<usize>> {
    const UNVISITED: usize = usize::MAX;
    let n = adj.len();
    let mut index = vec![UNVISITED; n];
    let mut lowlink = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut scc_stack: Vec<usize> = Vec::new();
    let mut sccs: Vec<Vec<usize>> = Vec::new();
    let mut next_index = 0usize;

    for root in 0..n {
        if index[root] != UNVISITED {
            continue;
        }
        // Explicit DFS stack of (node, next-child pointer into adj[node]).
        let mut call_stack: Vec<(usize, usize)> = vec![(root, 0)];
        while let Some(&(v, ci)) = call_stack.last() {
            if ci == 0 {
                index[v] = next_index;
                lowlink[v] = next_index;
                next_index += 1;
                scc_stack.push(v);
                on_stack[v] = true;
            }
            if ci < adj[v].len() {
                call_stack.last_mut().unwrap().1 += 1;
                let w = adj[v][ci];
                if index[w] == UNVISITED {
                    call_stack.push((w, 0));
                } else if on_stack[w] {
                    lowlink[v] = lowlink[v].min(index[w]);
                }
            } else {
                call_stack.pop();
                if let Some(&(parent, _)) = call_stack.last() {
                    lowlink[parent] = lowlink[parent].min(lowlink[v]);
                }
                if lowlink[v] == index[v] {
                    let mut comp = Vec::new();
                    loop {
                        let w = scc_stack.pop().expect("scc_stack non-empty at SCC root");
                        on_stack[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    sccs.push(comp);
                }
            }
        }
    }
    sccs
}

/// Shortest cycle through `start` within the SCC `in_scc`, as a closed index path
/// `[start, …, start]`. An SCC of size ≥ 2 always has one. Deterministic: the
/// closest back-edge node wins, ties broken by smallest index.
fn shortest_cycle(adj: &[Vec<usize>], in_scc: &BTreeSet<usize>, start: usize) -> Vec<usize> {
    use std::collections::HashMap;
    let mut dist: HashMap<usize, usize> = HashMap::new();
    let mut parent: HashMap<usize, usize> = HashMap::new();
    let mut queue: VecDeque<usize> = VecDeque::new();
    dist.insert(start, 0);
    queue.push_back(start);
    while let Some(u) = queue.pop_front() {
        let du = dist[&u];
        for &w in &adj[u] {
            // Stay inside the SCC and never re-enter `start` as an interior node.
            if w == start || !in_scc.contains(&w) || dist.contains_key(&w) {
                continue;
            }
            dist.insert(w, du + 1);
            parent.insert(w, u);
            queue.push_back(w);
        }
    }
    // Pick the reachable node with an edge back to `start` that closes the
    // shortest loop (ties: smallest index → deterministic).
    let mut best: Option<usize> = None;
    for (&node, &d) in &dist {
        if node != start && adj[node].binary_search(&start).is_ok() {
            best = Some(match best {
                Some(b) if (dist[&b], b) <= (d, node) => b,
                _ => node,
            });
        }
    }
    let Some(mut u) = best else {
        return Vec::new(); // unreachable for a genuine SCC ≥ 2
    };
    let mut rev = vec![u];
    while u != start {
        u = parent[&u];
        rev.push(u);
    }
    rev.reverse();
    rev.push(start);
    rev
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(from: &str, to: &str) -> (String, String) {
        (from.to_string(), to.to_string())
    }

    fn files_of(c: &DependencyCycle) -> Vec<&str> {
        c.files.iter().map(String::as_str).collect()
    }

    fn path_of(c: &DependencyCycle) -> Vec<&str> {
        c.path.iter().map(String::as_str).collect()
    }

    #[test]
    fn two_node_cycle_is_detected() {
        let cycles = find_cycles(&[edge("a", "b"), edge("b", "a")]);
        assert_eq!(cycles.len(), 1);
        assert_eq!(files_of(&cycles[0]), ["a", "b"]);
        assert_eq!(cycles[0].size, 2);
        // Representative path starts at the lex-smallest node and is closed.
        assert_eq!(path_of(&cycles[0]), ["a", "b", "a"]);
    }

    #[test]
    fn headline_matches_arrows_when_loop_visits_all_members() {
        // 2-node SCC: shortest loop visits both members → plain "N-file cycle".
        let cycles = find_cycles(&[edge("a", "b"), edge("b", "a")]);
        assert_eq!(cycles[0].headline(), "2-file cycle: a → b → a");
    }

    #[test]
    fn headline_calls_it_a_group_when_scc_exceeds_shortest_loop() {
        // 3 files all mutually reachable via the hub `b`, but the shortest loop
        // through the lex-smallest member is just `a → b → a` (2 files), so the
        // 3-file count must NOT be presented as a 3-file arrow path.
        let cycles = find_cycles(&[
            edge("a", "b"),
            edge("b", "a"),
            edge("b", "c"),
            edge("c", "b"),
        ]);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].size, 3);
        assert_eq!(path_of(&cycles[0]), ["a", "b", "a"]);
        assert_eq!(
            cycles[0].headline(),
            "3-file cyclic group (shortest loop: a → b → a)",
            "a 3-file SCC whose shortest loop is 2 files must not read as a 3-file cycle"
        );
    }

    #[test]
    fn acyclic_graph_yields_no_cycles() {
        let cycles = find_cycles(&[edge("a", "b"), edge("b", "c")]);
        assert!(cycles.is_empty());
    }

    #[test]
    fn self_edge_is_not_a_cross_file_cycle() {
        let cycles = find_cycles(&[edge("a", "a")]);
        assert!(
            cycles.is_empty(),
            "a file importing itself is not a dependency cycle"
        );
    }

    #[test]
    fn three_node_cycle_path_is_full_loop() {
        let cycles = find_cycles(&[edge("a", "b"), edge("b", "c"), edge("c", "a")]);
        assert_eq!(cycles.len(), 1);
        assert_eq!(files_of(&cycles[0]), ["a", "b", "c"]);
        assert_eq!(path_of(&cycles[0]), ["a", "b", "c", "a"]);
    }

    #[test]
    fn only_the_scc_nodes_are_included() {
        // x imports a, but nothing imports x back — x is not in the cycle.
        let cycles = find_cycles(&[edge("a", "b"), edge("b", "a"), edge("x", "a")]);
        assert_eq!(cycles.len(), 1);
        assert_eq!(
            files_of(&cycles[0]),
            ["a", "b"],
            "x must not be part of the SCC"
        );
    }

    #[test]
    fn larger_scc_groups_all_mutually_reachable_files() {
        // a↔b and b↔c ⇒ {a,b,c} is one SCC even though a and c aren't directly linked.
        let cycles = find_cycles(&[
            edge("a", "b"),
            edge("b", "a"),
            edge("b", "c"),
            edge("c", "b"),
        ]);
        assert_eq!(cycles.len(), 1);
        assert_eq!(files_of(&cycles[0]), ["a", "b", "c"]);
        assert_eq!(cycles[0].size, 3);
        // Shortest cycle through the smallest node "a" is a→b→a.
        assert_eq!(path_of(&cycles[0]), ["a", "b", "a"]);
    }

    #[test]
    fn disjoint_cycles_sorted_by_size_then_name() {
        // One 3-cycle {b,c,d}, one 2-cycle {a,e}. Bigger first; equal size by name.
        let cycles = find_cycles(&[
            edge("a", "e"),
            edge("e", "a"),
            edge("b", "c"),
            edge("c", "d"),
            edge("d", "b"),
        ]);
        assert_eq!(cycles.len(), 2);
        assert_eq!(files_of(&cycles[0]), ["b", "c", "d"], "larger SCC first");
        assert_eq!(files_of(&cycles[1]), ["a", "e"]);
    }

    #[test]
    fn rust_intra_crate_module_cycle_is_suppressed() {
        // Parent module ↔ submodule (mod.rs uses `use sub::f`, sub uses `super::T`)
        // and sibling ↔ sibling — both are idiomatic Rust, not load-order hazards.
        let cycles = find_cycles(&[
            edge("src/parser/relations/mod.rs", "src/parser/relations/cpp.rs"),
            edge("src/parser/relations/cpp.rs", "src/parser/relations/mod.rs"),
            edge(
                "src/storage/queries/edges.rs",
                "src/storage/queries/nodes.rs",
            ),
            edge(
                "src/storage/queries/nodes.rs",
                "src/storage/queries/edges.rs",
            ),
        ]);
        assert!(
            cycles.is_empty(),
            "Rust intra-crate import cycles must not be reported"
        );
    }

    #[test]
    fn non_rust_cycle_is_still_detected_alongside_suppressed_rust() {
        // A genuine JS require cycle survives even when Rust edges are present.
        let cycles = find_cycles(&[
            edge("a/mod.rs", "a/sub.rs"),
            edge("a/sub.rs", "a/mod.rs"),
            edge("scripts/doctor.js", "scripts/lifecycle.js"),
            edge("scripts/lifecycle.js", "scripts/doctor.js"),
        ]);
        assert_eq!(cycles.len(), 1, "only the JS cycle should remain");
        assert_eq!(
            files_of(&cycles[0]),
            ["scripts/doctor.js", "scripts/lifecycle.js"]
        );
    }

    #[test]
    fn cross_language_cycle_with_rust_endpoint_is_kept() {
        // Only same-`.rs` edges are dropped; a `.rs`↔`.py` cycle is cross-language
        // and genuinely worth surfacing, so it must still be reported.
        let cycles = find_cycles(&[edge("src/a.rs", "src/b.py"), edge("src/b.py", "src/a.rs")]);
        assert_eq!(
            cycles.len(),
            1,
            "a cross-language cycle is not a benign intra-crate cycle"
        );
        assert_eq!(files_of(&cycles[0]), ["src/a.rs", "src/b.py"]);
    }

    #[test]
    fn output_is_deterministic_regardless_of_edge_order() {
        let forward = find_cycles(&[
            edge("a", "b"),
            edge("b", "a"),
            edge("c", "d"),
            edge("d", "c"),
        ]);
        let shuffled = find_cycles(&[
            edge("d", "c"),
            edge("b", "a"),
            edge("c", "d"),
            edge("a", "b"),
        ]);
        assert_eq!(forward, shuffled);
    }
}
