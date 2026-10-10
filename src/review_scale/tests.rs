use std::cell::Cell;
use std::collections::BTreeSet;

use super::*;
use wicked_apps_core::{
    Descriptor, Edge, EdgeKind, GraphWrite, Language, Location, Node, NodeKind, ResolutionTier,
    Span, SqliteStore, Symbol,
};
use wicked_estate_core::RepoInfo;

// ── Graph fixtures: a real in-memory estate store, so traversal is the real one ─────────────

fn sym(name: &str) -> SymbolId {
    Symbol::global("test", None, vec![Descriptor::method(name, None)]).id()
}

fn node(name: &str, kind: NodeKind, file: &str, lines: (u32, u32)) -> Node {
    Node::new(
        sym(name),
        kind,
        name,
        Language::new("rust"),
        Location::new(
            file,
            Span {
                start_byte: 0,
                end_byte: 0,
                start_line: lines.0,
                start_col: 0,
                end_line: lines.1,
                end_col: 0,
            },
        ),
    )
}

/// `from` depends on `to` (source = dependent, target = dependency).
fn edge(from: &str, to: &str, kind: EdgeKind) -> Edge {
    Edge::new(sym(from), sym(to), kind, ResolutionTier::Parsed, "fixture")
}

const BASE: &str = "b45e0000000000000000000000000000000000000";
const HEAD: &str = "4ead0000000000000000000000000000000000000";

fn indexed_at(store: &mut SqliteStore, commit: &str) {
    store
        .set_repo_info(&RepoInfo {
            commit: Some(commit.to_string()),
            ..Default::default()
        })
        .expect("set_repo_info");
}

/// A graph indexed at the run base, which is what every score needs.
fn graph(nodes: &[Node], edges: &[Edge]) -> SqliteStore {
    let mut store = wicked_apps_core::open_store(Some(":memory:")).expect("in-memory store");
    store.begin_batch().expect("begin");
    store.upsert_nodes(nodes).expect("nodes");
    store.upsert_edges(edges).expect("edges");
    store.commit_batch().expect("commit");
    indexed_at(&mut store, BASE);
    store
}

fn ready(store: &SqliteStore) -> Graph<'_> {
    Graph::Ready {
        store,
        base_commit: BASE,
    }
}

/// `hot` in `src/core.rs` lines 10-30 with forty callers, and optionally one test that calls it.
fn hot_graph(with_test: bool) -> SqliteStore {
    hot_graph_at(10, with_test)
}

/// [`hot_graph`] with `hot` starting at `start` (its callers are elsewhere).
fn hot_graph_at(start: u32, with_test: bool) -> SqliteStore {
    let mut nodes = vec![node(
        "hot",
        NodeKind::Function,
        "src/core.rs",
        (start, start + 20),
    )];
    let mut edges = Vec::new();
    for i in 0..40u32 {
        let caller = format!("caller{i}");
        nodes.push(node(
            &caller,
            NodeKind::Function,
            "src/callers.rs",
            (i * 5 + 1, i * 5 + 4),
        ));
        edges.push(edge(&caller, "hot", EdgeKind::Calls));
    }
    if with_test {
        nodes.push(node(
            "hot_roundtrip",
            NodeKind::Function,
            "tests/hot.rs",
            (1, 10),
        ));
        edges.push(edge("hot_roundtrip", "hot", EdgeKind::Calls));
    }
    graph(&nodes, &edges)
}

fn file_diff(path: &str, hunk: &str) -> String {
    format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n{hunk}")
}

/// A ten-line change inside `hot` (base lines 12-21).
fn hot_diff() -> String {
    let mut hunk = String::from("@@ -12,5 +12,5 @@\n");
    for i in 0..5 {
        hunk.push_str(&format!("-    old{i}\n+    new{i}\n"));
    }
    file_diff("src/core.rs", &hunk)
}

// ── Diff fixtures ────────────────────────────────────────────────────────────────────────────

const DOCS_ONLY: &str = "\
diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1,2 +1,3 @@
 # wicked-core
+Run `rm -rf target` to DROP TABLE the build cache.
 more
diff --git a/docs/guide.md b/docs/guide.md
--- a/docs/guide.md
+++ b/docs/guide.md
@@ -1 +1 @@
-old
+new
";

const SMALL_CODE: &str = "\
diff --git a/src/plan.rs b/src/plan.rs
--- a/src/plan.rs
+++ b/src/plan.rs
@@ -10,3 +10,4 @@
 fn plan() {
-    let x = 1;
+    let x = 2;
+    let y = x + 1;
 }
";

// The issue's example: a small change on a memory-erase path.
const MEMORY_ERASE: &str = "\
diff --git a/src/memory.rs b/src/memory.rs
--- a/src/memory.rs
+++ b/src/memory.rs
@@ -40,2 +40,3 @@
 fn retire(scope: &str) {
+    store.erase_scope(scope)?;
 }
";

fn plan_at(score: u8) -> ReviewPlan {
    THRESHOLDS
        .bands
        .iter()
        .rev()
        .find(|(min, _)| score >= *min)
        .map(|(_, p)| *p)
        .expect("a band")
}

// ── The fixed expectations (operator decision on #590, 2026-09-23) ──────────────────────────

#[test]
fn ten_line_change_to_a_symbol_with_forty_dependents_scores_60_to_80() {
    let diff = signals_from_diff(&hot_diff());
    assert_eq!((diff.lines_added, diff.lines_removed), (5, 5));

    // No test reaches `hot`: reach 60 + test gap 20.
    let store = hot_graph(false);
    let a = assess(&diff, ready(&store), None);
    let s = a.signals.as_ref().expect("graph was read");
    assert_eq!((s.changed_symbols, s.dependents), (1, 40), "{s:?}");
    assert_eq!(a.score, 80, "{a:?}");
    assert_eq!(a.plan, PLAN_MOST);

    // One test reaches it: reach 60, no gap.
    let store = hot_graph(true);
    let a = assess(&diff, ready(&store), None);
    assert_eq!(a.score, 60, "{a:?}");
    assert_eq!(a.plan, PLAN_DEEP);
    assert!((60..=80).contains(&a.score));
}

#[test]
fn six_hundred_line_new_leaf_file_scores_50_and_still_ranks_below_the_hot_edit() {
    let mut hunk = String::from("@@ -0,0 +1,600 @@\n");
    for i in 0..600 {
        hunk.push_str(&format!("+    let v{i} = {i};\n"));
    }
    let d = format!(
        "diff --git a/src/leaf.rs b/src/leaf.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/leaf.rs\n{hunk}"
    );
    let diff = signals_from_diff(&d);
    assert_eq!(diff.lines_added, 600);
    assert!(diff.touched[0].old_lines.is_empty(), "{:?}", diff.touched);

    let store = hot_graph(true);
    let a = assess(&diff, ready(&store), None);
    let s = a.signals.as_ref().expect("graph was read");
    assert_eq!((s.changed_symbols, s.dependents), (1, 0), "{s:?}");
    // Reach 20, complexity +20 (600 changed lines), novelty +10 (a new file): 50. The ten-line
    // hot edit (80) still outranks it — reach stays the base.
    assert_eq!(a.score, 50, "{a:?}");
    assert_eq!(a.plan, PLAN_DEEP);
}

#[test]
fn event_schema_change_with_three_consumers_crosses_the_contract_band() {
    // Three consumers reach the enum over INJECTED edges (event -> consumer), the kind grep never
    // sees, plus one test so the gap contributes nothing and the contract is the only escalation.
    fn consumers(type_name: &str, file: &str) -> SqliteStore {
        let mut nodes = vec![node(type_name, NodeKind::Enum, file, (60, 120))];
        let mut edges = Vec::new();
        for c in ["feed", "ledger", "narrator"] {
            nodes.push(node(c, NodeKind::Function, "src/consumers.rs", (1, 5)));
            edges.push(edge(c, type_name, EdgeKind::Other("consumes".into())));
        }
        nodes.push(node(
            "roundtrip",
            NodeKind::Function,
            "tests/events.rs",
            (1, 5),
        ));
        edges.push(edge("roundtrip", type_name, EdgeKind::Calls));
        graph(&nodes, &edges)
    }
    let hunk =
        "@@ -70,2 +70,3 @@\n     Heartbeat,\n+    MonitorFinding { unit: u32 },\n     UnitDone,\n";

    let store = consumers("CoreEvent", "src/event.rs");
    let a = assess(
        &signals_from_diff(&file_diff("src/event.rs", hunk)),
        ready(&store),
        None,
    );
    let s = a.signals.as_ref().expect("graph was read");
    assert!(s.contract_change, "{s:?}");
    assert_eq!(s.dependents, 4, "{s:?}");
    assert_eq!(a.score, 40, "{a:?}");
    assert_eq!(a.plan, PLAN_DEEP);

    // The same change to a plain type on a plain path stays one band lower.
    let store = consumers("Plain", "src/plain.rs");
    let a = assess(
        &signals_from_diff(&file_diff("src/plain.rs", hunk)),
        ready(&store),
        None,
    );
    assert!(!a.signals.as_ref().expect("graph was read").contract_change);
    assert_eq!(a.score, 20, "{a:?}");
    assert_eq!(a.plan, PLAN_STANDARD);
}

#[test]
fn no_graph_scores_100_and_docs_only_scores_0_without_one() {
    let a = assess(
        &signals_from_diff(SMALL_CODE),
        Graph::Unavailable("no graph for the repo".into()),
        None,
    );
    assert_eq!((a.deterministic, a.score), (100, 100), "{a:?}");
    assert!(a.signals.is_none());
    assert!(
        a.reasons[0].contains("no graph for the repo"),
        "{:?}",
        a.reasons
    );
    assert_eq!(a.plan, PLAN_MOST);

    let stale = GraphAge::Stale {
        indexed: "aaaa".into(),
        base: "bbbb".into(),
    };
    let a = assess(
        &signals_from_diff(SMALL_CODE),
        Graph::Unavailable(stale.reason().expect("stale has a reason")),
        None,
    );
    assert_eq!(a.score, 100);
    assert!(
        a.reasons[0].contains("aaaa is not the run base bbbb"),
        "{:?}",
        a.reasons
    );

    let a = assess(
        &signals_from_diff(DOCS_ONLY),
        Graph::Unavailable("no graph".into()),
        None,
    );
    assert_eq!(a.score, 0, "docs-only has no symbols: {a:?}");
    assert_eq!(a.plan, PLAN_NONE);
}

#[test]
fn destructive_path_scores_at_least_the_floor() {
    // A leaf with no dependents would score 20; the destructive marker lifts it to 70.
    let store = graph(
        &[node("sweep", NodeKind::Function, "src/sweep.rs", (1, 10))],
        &[],
    );
    let hunk =
        "@@ -1,4 +1,4 @@\n-if confirmed {\n+if true {\n     std::fs::remove_dir(path)?;\n }\n";
    let diff = signals_from_diff(&file_diff("src/sweep.rs", hunk));
    assert!(diff.destructive);
    let a = assess(&diff, ready(&store), None);
    assert_eq!(a.score, THRESHOLDS.destructive_floor as u8, "{a:?}");
    assert_eq!(a.plan, PLAN_MOST);

    // Above the floor the floor adds nothing.
    let mut d = hot_diff();
    d.push_str("+    std::fs::remove_dir_all(&worktree)?;\n");
    let store = hot_graph(false);
    let a = assess(&signals_from_diff(&d), ready(&store), None);
    assert_eq!(a.score, 80, "{a:?}");
}

struct Adds(u8, Cell<u32>);

impl ModelAssessment for Adds {
    fn assess(&self, _: &ImpactSignals, _: u8) -> Option<ModelBonus> {
        self.1.set(self.1.get() + 1);
        Some(ModelBonus {
            add: self.0,
            rationale: "the change rewires a retry loop".into(),
        })
    }
}

#[test]
fn the_model_hook_can_raise_but_never_lower() {
    let store = hot_graph(true); // deterministic 60
    let diff = signals_from_diff(&hot_diff());
    for (add, want) in [(0, 60), (10, 70), (20, 80), (15, 70), (200, 80)] {
        let hook = Adds(add, Cell::new(0));
        let a = assess(&diff, ready(&store), Some(&hook));
        assert_eq!((a.deterministic, a.score), (60, want), "add {add}: {a:?}");
        assert_eq!(hook.1.get(), 1);
        assert_eq!(a.plan, plan_at(want));
        let m = a.model.expect("bonus recorded");
        assert_eq!(m.add, want - 60);
        assert!(!m.rationale.is_empty());
        assert!(a.reasons.last().expect("a reason").contains("retry loop"));
    }

    // Below the consultation floor the hook is never asked.
    let hook = Adds(20, Cell::new(0));
    let a = assess(&signals_from_diff(DOCS_ONLY), ready(&store), Some(&hook));
    assert_eq!((a.score, hook.1.get()), (0, 0), "{a:?}");
    assert!(a.model.is_none());

    // Nor when the graph was unusable: there are no signals to assess.
    let a = assess(&diff, Graph::Unavailable("no graph".into()), Some(&hook));
    assert_eq!((a.score, hook.1.get()), (100, 0), "{a:?}");
}

// ── The table, pure ──────────────────────────────────────────────────────────────────────────

#[test]
fn impact_score_table() {
    fn sig(changed: u32, deps: u32) -> ImpactSignals {
        ImpactSignals {
            changed_symbols: changed,
            dependents: deps,
            products: 1,
            ..Default::default()
        }
    }
    let cases: &[(&str, ImpactSignals, u8)] = &[
        ("nothing", ImpactSignals::default(), 0),
        ("one symbol, no dependents", sig(1, 0), 20),
        ("reach tier 1 top", sig(1, 5), 20),
        ("reach tier 2", sig(1, 6), 40),
        ("reach tier 2 top", sig(1, 20), 40),
        ("reach tier 3", sig(1, 21), 60),
        ("reach tier 3 top", sig(1, 100), 60),
        ("reach tier 4", sig(1, 101), 80),
        (
            "two products",
            ImpactSignals {
                products: 2,
                ..sig(1, 1)
            },
            30,
        ),
        (
            "five products cap at two steps",
            ImpactSignals {
                products: 5,
                ..sig(1, 1)
            },
            40,
        ),
        (
            "contract",
            ImpactSignals {
                contract_change: true,
                ..sig(1, 1)
            },
            40,
        ),
        (
            "critical",
            ImpactSignals {
                critical: true,
                ..sig(1, 1)
            },
            40,
        ),
        (
            "half the symbols untested",
            ImpactSignals {
                test_gap: 0.5,
                ..sig(2, 1)
            },
            30,
        ),
        (
            "untested but no dependents: nothing to regress",
            ImpactSignals {
                test_gap: 1.0,
                ..sig(1, 0)
            },
            20,
        ),
        (
            "destructive leaf floors at 70",
            ImpactSignals {
                destructive: true,
                ..sig(1, 0)
            },
            70,
        ),
        (
            "everything caps at 100",
            ImpactSignals {
                products: 9,
                contract_change: true,
                test_gap: 1.0,
                critical: true,
                destructive: true,
                ..sig(40, 5000)
            },
            100,
        ),
    ];
    for (name, s, want) in cases {
        let got = impact_score(s);
        assert_eq!(got.score, *want, "{name}: {s:?} -> {got:?}");
        assert!(
            !got.reasons.is_empty(),
            "{name}: every score explains itself"
        );
    }
}

/// core#692: the reach reason names WHY a traversal is incomplete. A hop horizon is not a node
/// cap: within the horizon the count is exact, so it must not read "(capped)".
#[test]
fn reach_reason_labels_truncation_by_cause() {
    let reason = |node_cap_reached, depth_horizon_reached| {
        impact_score(&ImpactSignals {
            changed_symbols: 1,
            dependents: 3,
            products: 1,
            node_cap_reached,
            depth_horizon_reached,
            ..Default::default()
        })
        .reasons[0]
            .clone()
    };
    assert!(
        reason(false, false).ends_with("within 3 hops"),
        "{}",
        reason(false, false)
    );
    assert!(reason(true, false).ends_with("within 3 hops (capped)"));
    assert!(reason(false, true).ends_with("within 3 hops (depth-limited)"));
    assert!(reason(true, true).ends_with("within 3 hops (capped, depth-limited)"));
}

/// core#692 through the real traversal: a five-deep caller chain under a 3-hop horizon counts
/// the three callers in reach and reads "depth-limited", never "capped".
#[test]
fn a_chain_past_the_hop_horizon_reads_depth_limited_not_capped() {
    let mut nodes = vec![node("hot", NodeKind::Function, "src/core.rs", (10, 30))];
    let mut edges = Vec::new();
    let mut prev = "hot".to_string();
    for i in 0..5u32 {
        let caller = format!("chain{i}");
        nodes.push(node(
            &caller,
            NodeKind::Function,
            "src/chain.rs",
            (i * 5 + 1, i * 5 + 4),
        ));
        edges.push(edge(&caller, &prev, EdgeKind::Calls));
        prev = caller;
    }
    let store = graph(&nodes, &edges);
    let a = assess(&signals_from_diff(&hot_diff()), ready(&store), None);
    let s = a.signals.as_ref().expect("graph was read");
    assert_eq!(s.dependents, 3, "{s:?}");
    assert!(s.depth_horizon_reached && !s.node_cap_reached, "{s:?}");
    let reach = a
        .reasons
        .iter()
        .find(|r| r.starts_with("reach"))
        .expect("a reach reason");
    assert!(reach.ends_with("(depth-limited)"), "{reach}");
}

#[test]
fn bands_table() {
    for (score, want) in [
        (0, PLAN_NONE),
        (19, PLAN_NONE),
        (20, PLAN_STANDARD),
        (39, PLAN_STANDARD),
        (40, PLAN_DEEP),
        (69, PLAN_DEEP),
        (70, PLAN_MOST),
        (100, PLAN_MOST),
    ] {
        assert_eq!(plan_for(score), want, "score {score}");
    }
    assert_eq!(THRESHOLDS.bands.last().expect("bands").1.monitors, 3);
}

// ── Graph age ────────────────────────────────────────────────────────────────────────────────

#[test]
fn graph_age_requires_the_indexed_commit_to_be_the_base() {
    let mut store = wicked_apps_core::open_store(Some(":memory:")).expect("in-memory store");
    assert!(matches!(graph_age(&store, BASE), GraphAge::Missing(_)));
    indexed_at(&mut store, "");
    assert!(matches!(graph_age(&store, BASE), GraphAge::Missing(_)));

    indexed_at(&mut store, BASE);
    assert_eq!(graph_age(&store, BASE), GraphAge::Current, "same commit");

    // Review on #600: a graph at a DESCENDANT of the base was accepted, but its spans are the
    // head's, not the base's, and the base-side lookup misses a shifted symbol. Newer is stale too.
    for other in [HEAD, "0000000000000000000000000000000000000000"] {
        indexed_at(&mut store, other);
        assert_eq!(
            graph_age(&store, BASE),
            GraphAge::Stale {
                indexed: other.to_string(),
                base: BASE.to_string()
            },
            "any other commit is stale"
        );
    }
}

/// Review on #600: the PR inserts 100 lines above `hot`, so a graph indexed at the PR's head
/// records `hot` at 110-130 while the diff's base-side lookup asks about 12-21. Against the base
/// graph the edit still scores 80; against the head graph it must fail closed at 100, never 20.
#[test]
fn hot_symbol_shifted_by_an_insertion_above_it_still_scores_80() {
    let mut hunk = String::from("@@ -1,0 +1,100 @@\n");
    for i in 0..100 {
        hunk.push_str(&format!("+// inserted {i}\n"));
    }
    hunk.push_str("@@ -12,5 +112,5 @@\n");
    for i in 0..5 {
        hunk.push_str(&format!("-    old{i}\n+    new{i}\n"));
    }
    let diff = signals_from_diff(&file_diff("src/core.rs", &hunk));
    assert_eq!((diff.lines_added, diff.lines_removed), (105, 5));

    // Indexed at the base: `hot` is at 10-30, the touched lines 12-16 hit it.
    let base_graph = hot_graph_at(10, false);
    let a = assess(&diff, ready(&base_graph), None);
    assert_eq!(a.signals.as_ref().expect("read").dependents, 40, "{a:?}");
    // Reach 60 + test gap 20 + complexity 10 (110 changed lines).
    assert_eq!((a.score, a.plan), (90, PLAN_MOST), "{a:?}");

    // Indexed at the head: `hot` is at 110-130. Scoring this would say 20; the rule says stale.
    let mut head_graph = hot_graph_at(110, false);
    indexed_at(&mut head_graph, HEAD);
    let a = assess(&diff, ready(&head_graph), None);
    assert_eq!((a.score, a.plan), (100, PLAN_MOST), "{a:?}");
    assert!(a.signals.is_none(), "never scored: {a:?}");
    assert!(
        a.reasons[0].contains("is not the run base"),
        "{:?}",
        a.reasons
    );
    assert_ne!(a.score, 20);
}

/// Review on #600: a pure rename/move has no hunks, so `old_lines` is empty and the file node was
/// never seeded; a heavily imported module read as one unindexed symbol with no dependents (20).
/// The old path IS in the graph, so its file node is the changed symbol and its importers count.
/// A file node at `path` with forty importers and no other symbol.
fn imported_file_graph(path: &str) -> SqliteStore {
    let mut nodes = vec![node("core_file", NodeKind::File, path, (1, 200))];
    let mut edges = Vec::new();
    for i in 0..40u32 {
        let importer = format!("importer{i}");
        nodes.push(node(
            &importer,
            NodeKind::File,
            &format!("src/user{i}.rs"),
            (1, 50),
        ));
        edges.push(edge(&importer, "core_file", EdgeKind::Imports));
    }
    graph(&nodes, &edges)
}

#[test]
fn rename_only_of_a_heavily_imported_module_counts_its_importers() {
    let store = imported_file_graph("src/core.rs");
    let d = "\
diff --git a/src/core.rs b/src/runtime/core.rs
similarity index 100%
rename from src/core.rs
rename to src/runtime/core.rs
";
    let diff = signals_from_diff(d);
    assert_eq!((diff.code_files, diff.lines_added), (1, 0), "{diff:?}");
    assert!(diff.touched[0].old_lines.is_empty(), "{:?}", diff.touched);

    let a = assess(&diff, ready(&store), None);
    let s = a.signals.as_ref().expect("graph was read");
    assert_eq!((s.changed_symbols, s.dependents), (1, 40), "{s:?}");
    assert!(a.score >= 60, "{a:?}");
    assert_eq!((a.score, a.plan), (80, PLAN_MOST), "{a:?}");

    // A truly new file (its old path absent from the graph) is still one unindexed symbol.
    let new_leaf = signals_from_diff(
        "diff --git a/src/leaf.rs b/src/leaf.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/leaf.rs\n@@ -0,0 +1 @@\n+fn leaf() {}\n",
    );
    let a = assess(&new_leaf, ready(&store), None);
    let s = a.signals.as_ref().expect("graph was read");
    // Reach 20 + novelty 10 (the file is new).
    assert_eq!(
        (s.changed_symbols, s.dependents, a.score),
        (1, 0, 30),
        "{a:?}"
    );
}

/// core#611: a copy's `a/` side is the SOURCE, which did not change. Scoring it as a rename
/// counted every importer of `src/core.rs` for a diff that never touched `src/core.rs`. The copy
/// is a new, unindexed leaf (20); the source still classifies the block and carries its path
/// markers.
#[test]
fn copy_twin_of_a_heavily_imported_module_scores_as_a_new_leaf() {
    let store = imported_file_graph("src/core.rs");
    let d = "\
diff --git a/src/core.rs b/src/runtime/core.rs
similarity index 100%
copy from src/core.rs
copy to src/runtime/core.rs
";
    let diff = signals_from_diff(d);
    assert_eq!((diff.code_files, diff.lines_added), (1, 0), "{diff:?}");
    assert_eq!(
        diff.touched[0].path, "src/runtime/core.rs",
        "{:?}",
        diff.touched
    );
    assert_eq!(
        diff.touched[0].old_path, "",
        "a copy's source is not the changed path"
    );
    let a = assess(&diff, ready(&store), None);
    let s = a.signals.as_ref().expect("graph was read");
    // Reach 20 + novelty 10: the copy is a new, unindexed file.
    assert_eq!(
        (s.changed_symbols, s.unindexed, s.dependents, a.score),
        (1, 1, 0, 30),
        "{a:?}"
    );
    assert_eq!(a.plan, PLAN_STANDARD);

    // A copy with hunks (similarity < 100%): its line offsets are into the unchanged source, so
    // none are looked up; the copy stays a leaf. The source path still carries its markers.
    let edited = "\
diff --git a/src/memory.rs b/src/runtime/memory.rs
similarity index 90%
copy from src/memory.rs
copy to src/runtime/memory.rs
--- a/src/memory.rs
+++ b/src/runtime/memory.rs
@@ -10,3 +10,3 @@
 fn keep() {
-    let x = 1;
+    let x = 2;
 }
";
    let diff = signals_from_diff(edited);
    assert_eq!(diff.touched[0].old_path, "", "{:?}", diff.touched);
    assert!(diff.touched[0].old_lines.is_empty(), "{:?}", diff.touched);
    assert!(
        diff.critical,
        "the source path's marker still applies: {diff:?}"
    );
    assert_eq!((diff.lines_added, diff.lines_removed), (1, 1));

    // The same block as a RENAME still counts the importers (review on #600 holds).
    let renamed = d
        .replace("copy from", "rename from")
        .replace("copy to", "rename to");
    let a = assess(&signals_from_diff(&renamed), ready(&store), None);
    assert_eq!((a.score, a.plan), (80, PLAN_MOST), "{a:?}");
}

/// core#711: a changed path the graph never indexed (a new file) is UNKNOWN to the test gap,
/// not untested — the graph has no edge to read. It still counts as one changed symbol with no
/// dependents (reach), and the reasons name it as unindexed. A hot, tested symbol beside a new
/// file scores its reach and its novelty (80), never reach plus half a test gap.
#[test]
fn an_unindexed_changed_path_reads_unindexed_not_untested() {
    let new_file = "diff --git a/scripts/walk.mjs b/scripts/walk.mjs\nnew file mode 100644\n--- /dev/null\n+++ b/scripts/walk.mjs\n@@ -0,0 +1 @@\n+export const walk = 1;\n";
    let diff = signals_from_diff(&format!("{}{new_file}", hot_diff()));
    assert_eq!(diff.code_files, 2, "{diff:?}");

    let tested = hot_graph(true);
    let a = assess(&diff, ready(&tested), None);
    let s = a.signals.as_ref().expect("graph was read");
    // 40 callers + the test that reaches `hot`.
    assert_eq!(
        (s.changed_symbols, s.unindexed, s.dependents),
        (2, 1, 41),
        "{s:?}"
    );
    assert_eq!(
        s.test_gap, 0.0,
        "the indexed symbol is tested; the new file is unknown"
    );
    // Reach 60, no gap; novelty +10 (the new file) +10 (its new export `walk`).
    assert_eq!(a.score, 80, "{a:?}");
    assert!(
        a.reasons
            .iter()
            .any(|r| r.contains("1 unindexed path(s) with unknown test reach")),
        "{:?}",
        a.reasons
    );
    assert!(
        a.reasons
            .iter()
            .any(|r| r.contains("2 changed symbol(s) (1 unindexed)")),
        "{:?}",
        a.reasons
    );
    assert!(
        !a.reasons.iter().any(|r| r.contains("untested")),
        "{:?}",
        a.reasons
    );

    // With the hot symbol untested, the gap is 100% OF THE INDEXED part (+20), and the note still
    // separates the unindexed path from it.
    let untested = hot_graph(false);
    let a = assess(&diff, ready(&untested), None);
    let s = a.signals.as_ref().expect("graph was read");
    assert_eq!(s.test_gap, 1.0, "{s:?}");
    assert_eq!(a.score, 100, "{a:?}");
    assert!(
        a.reasons.iter().any(
            |r| r.starts_with("test gap +20: 100% of indexed changed symbols")
                && r.contains("1 unindexed path(s) with unknown test reach")
        ),
        "{:?}",
        a.reasons
    );
}

// Review on #600: the `diff --git a/X b/Y` line was split on the first " b/", so an old path
// containing " b/" (`src/a b/core.rs`) recorded `src/a`, the graph lookup missed, and a hot module
// scored as an unindexed leaf. Paths come from the `---`/`+++` headers (git-unquoted, `/dev/null`
// = absent) and from `rename from`/`rename to` for hunk-less renames; `diff --git` only delimits.
#[test]
fn old_path_containing_space_b_slash_is_read_from_the_headers() {
    let path = "src/a b/core.rs";
    let store = imported_file_graph(path);
    let d = format!(
        "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,2 +1,2 @@\n-x\n+y\n line\n"
    );
    let diff = signals_from_diff(&d);
    assert_eq!(
        (
            diff.touched[0].old_path.as_str(),
            diff.touched[0].path.as_str()
        ),
        (path, path),
        "{:?}",
        diff.touched
    );
    let a = assess(&diff, ready(&store), None);
    let s = a.signals.as_ref().expect("graph was read");
    assert_eq!((s.changed_symbols, s.dependents), (1, 40), "{s:?}");
    assert_eq!((a.score, a.plan), (80, PLAN_MOST), "{a:?}");
}

#[test]
fn quoted_header_paths_are_git_unquoted() {
    // git quotes a path with a tab, a quote, a backslash, or (core.quotePath) non-ASCII bytes.
    let d = "\
diff --git \"a/src/caf\\303\\251 dir/core.rs\" \"b/src/caf\\303\\251 dir/core.rs\"
--- \"a/src/caf\\303\\251 dir/core.rs\"
+++ \"b/src/caf\\303\\251 dir/core.rs\"
@@ -1 +1 @@
-x
+y
diff --git \"a/src/we\\\"ird\\\\tab\\t.rs\" \"b/src/we\\\"ird\\\\tab\\t.rs\"
--- \"a/src/we\\\"ird\\\\tab\\t.rs\"
+++ \"b/src/we\\\"ird\\\\tab\\t.rs\"
@@ -1 +1 @@
-x
+y
";
    let diff = signals_from_diff(d);
    assert_eq!(diff.code_files, 2, "{diff:?}");
    assert_eq!(diff.touched[0].old_path, "src/caf\u{e9} dir/core.rs");
    assert_eq!(diff.touched[0].path, "src/caf\u{e9} dir/core.rs");
    assert_eq!(diff.touched[1].old_path, "src/we\"ird\\tab\t.rs");

    let store = imported_file_graph("src/caf\u{e9} dir/core.rs");
    let one = signals_from_diff(
        d.split("diff --git \"a/src/we")
            .next()
            .expect("first block"),
    );
    let a = assess(&one, ready(&store), None);
    assert_eq!(a.signals.as_ref().expect("read").dependents, 40, "{a:?}");
}

#[test]
fn dev_null_on_either_side_is_new_or_deleted() {
    // New: no old path, so nothing is looked up in the graph (unindexed, 20), not destructive.
    let new = signals_from_diff(
        "diff --git a/src/new.rs b/src/new.rs\n--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1 @@\n+fn n() {}\n",
    );
    assert_eq!(
        (
            new.touched[0].old_path.as_str(),
            new.touched[0].path.as_str()
        ),
        ("", "src/new.rs"),
        "{:?}",
        new.touched
    );
    assert!(!new.destructive && new.code_files == 1, "{new:?}");

    // Deleted, with no `deleted file mode` line: `+++ /dev/null` alone says so.
    let gone = signals_from_diff(
        "diff --git a/src/old.rs b/src/old.rs\n--- a/src/old.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-fn o() {}\n",
    );
    assert_eq!(
        (
            gone.touched[0].old_path.as_str(),
            gone.touched[0].path.as_str()
        ),
        ("src/old.rs", "src/old.rs"),
        "{:?}",
        gone.touched
    );
    assert!(gone.destructive, "{gone:?}");

    // A deleted docs file is still docs-only.
    let doc = signals_from_diff(
        "diff --git a/notes.md b/notes.md\n--- a/notes.md\n+++ /dev/null\n@@ -1 +0,0 @@\n-hi\n",
    );
    assert!(!doc.behavioural() && !doc.destructive, "{doc:?}");
}

// ── The diff side ────────────────────────────────────────────────────────────────────────────

#[test]
fn touched_files_carry_base_side_lines() {
    let s = signals_from_diff(SMALL_CODE);
    assert_eq!(s.touched.len(), 1);
    let f = &s.touched[0];
    assert_eq!(
        (f.path.as_str(), f.old_path.as_str()),
        ("src/plan.rs", "src/plan.rs")
    );
    // Removed line 11; the two insertions sit between 11 and 12.
    assert_eq!(f.old_lines, BTreeSet::from([11, 12]), "{f:?}");

    let s = signals_from_diff(MEMORY_ERASE);
    assert_eq!(
        s.touched[0].old_lines,
        BTreeSet::from([40, 41]),
        "{:?}",
        s.touched
    );
}

#[test]
fn diff_signals_classify_fixtures() {
    let docs = signals_from_diff(DOCS_ONLY);
    assert_eq!(
        (docs.lines_added, docs.lines_removed, docs.docs_files),
        (2, 1, 2)
    );
    assert!(
        !docs.behavioural() && !docs.destructive && docs.touched.is_empty(),
        "destructive words in prose are not a destructive path: {docs:?}"
    );

    let small = signals_from_diff(SMALL_CODE);
    assert_eq!(
        (small.lines_added, small.lines_removed, small.code_files),
        (2, 1, 1)
    );
    assert!(small.behavioural() && !small.destructive && !small.critical);

    let erase = signals_from_diff(MEMORY_ERASE);
    assert_eq!((erase.lines_added, erase.code_files), (1, 1));
    assert!(erase.destructive && erase.critical, "{erase:?}");
}

#[test]
fn diff_signals_file_kinds_and_deletes() {
    let d = "\
diff --git a/tests/e2e.rs b/tests/e2e.rs
--- a/tests/e2e.rs
+++ b/tests/e2e.rs
@@ -1 +1 @@
-a
+b
diff --git a/Cargo.toml b/Cargo.toml
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -1 +1 @@
-a
+b
diff --git a/src/old.rs b/src/old.rs
deleted file mode 100644
--- a/src/old.rs
+++ /dev/null
@@ -1 +0,0 @@
-fn gone() {}
";
    let s = signals_from_diff(d);
    assert_eq!(
        (s.test_files, s.config_files, s.code_files),
        (1, 1, 1),
        "{s:?}"
    );
    assert!(
        s.destructive,
        "deleting a code file is a destructive path: {s:?}"
    );
    assert_eq!(s.touched[2].old_lines, BTreeSet::from([1]));
}

// Review on #600: only the `b/` side was classified, so moving code out of a critical subsystem
// into a docs path read as docs-only and summoned nothing (fail-open).
#[test]
fn code_to_docs_rename_is_behavioural_and_critical() {
    let d = "\
diff --git a/src/memory.rs b/docs/memory.md
similarity index 100%
rename from src/memory.rs
rename to docs/memory.md
";
    let s = signals_from_diff(d);
    assert_eq!((s.docs_files, s.code_files), (0, 1), "{s:?}");
    assert!(s.critical, "the a/ side is a critical subsystem: {s:?}");
    assert_eq!(s.touched[0].old_path, "src/memory.rs");
    let store = graph(&[], &[]);
    let a = assess(&s, ready(&store), None);
    // The empty graph does not know the old path: novelty +10 for the unindexed file.
    assert_eq!(a.score, 50, "reach 20 + critical 20 + novelty 10: {a:?}");
}

#[test]
fn deleted_critical_file_is_destructive_and_critical() {
    let d = "\
diff --git a/src/memory.rs b/src/memory.rs
deleted file mode 100644
--- a/src/memory.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-fn recall() {}
-fn store() {}
";
    let s = signals_from_diff(d);
    assert!(s.destructive && s.critical, "{s:?}");
    let store = graph(&[], &[]);
    assert_eq!(assess(&s, ready(&store), None).plan, PLAN_MOST);
}

// Same fail-open class: a hunk with no `diff --git` header was counted as lines but no file, so
// it read as docs-only. An unattributed hunk counts as code.
#[test]
fn headerless_hunk_is_not_docs_only() {
    let s = signals_from_diff("@@ -1 +1 @@\n-a\n+b\n");
    assert_eq!(s.code_files, 1, "{s:?}");
    let store = graph(&[], &[]);
    assert_eq!(assess(&s, ready(&store), None).plan.monitors, 1);
}

// Review on #600: `remove_dir_all` and `remove_file` were listed but not `remove_dir`, so
// `std::fs::remove_dir(path)?;` in a small change got one standard monitor and no reviewer. The
// markers are now grouped by family; each family member has a realistic fixture line here, and the
// last assertion keeps the table and the fixtures in step.
#[test]
fn every_destructive_family_member_summons_the_top_band() {
    let fixtures = [
        // Rust std::fs
        "std::fs::remove_dir(path)?;",
        "fs::remove_dir_all(&worktree)?;",
        "std::fs::remove_file(&lock)?;",
        // Node fs
        "await fs.rm(dir, { recursive: true, force: true });",
        "fs.rmSync(dir, { recursive: true });",
        "fs.rmdir(dir, cb);",
        "fs.unlink(file, cb);",
        "fs.unlinkSync(file);",
        "await rimraf(dist);",
        // Python
        "os.remove(path)",
        "os.unlink(path)",
        "os.rmdir(path)",
        "shutil.rmtree(tmp)",
        // shell
        "rm -r build",
        "rm -rf target",
        "rm -f state.db",
        "rmdir empty",
        // git
        "git clean -fdx",
        "git branch -D feat/old",
        "git push --force origin main",
        "git push -f origin main",
        "git reset --hard origin/main",
        "git worktree remove --force run-1",
        // SQL
        "DROP TABLE runs;",
        "ALTER TABLE runs DROP COLUMN cost;",
        "DROP INDEX idx_runs;",
        "DROP DATABASE wicked;",
        "DROP SCHEMA memory;",
        "TRUNCATE TABLE events;",
        "DELETE FROM memories WHERE scope = ?;",
        // generic verbs
        "store.erase_scope(scope)?;",
        "purge_expired(&conn)?;",
        "wipe_state_home()?;",
    ];
    let store = graph(&[], &[]);
    for line in fixtures {
        let d = file_diff(
            "src/plan.rs",
            &format!("@@ -1 +1,2 @@\n fn f() {{}}\n+{line}\n"),
        );
        let s = signals_from_diff(&d);
        assert!(s.destructive, "not destructive: {line:?} -> {s:?}");
        assert_eq!(assess(&s, ready(&store), None).plan, PLAN_MOST, "{line:?}");
    }
    let lower: Vec<String> = fixtures.iter().map(|l| l.to_ascii_lowercase()).collect();
    for m in THRESHOLDS.destructive_line_markers {
        assert!(
            lower.iter().any(|l| l.contains(m)),
            "marker {m:?} has no fixture line"
        );
    }
}

#[test]
fn destructive_words_in_a_docs_comment_stay_at_zero() {
    let d = "\
diff --git a/docs/ops.md b/docs/ops.md
--- a/docs/ops.md
+++ b/docs/ops.md
@@ -1 +1,2 @@
 # Ops
+<!-- the sweep calls remove_dir and git push --force; see the runbook -->
";
    let s = signals_from_diff(d);
    assert!(!s.destructive, "{s:?}");
    let store = graph(&[], &[]);
    assert_eq!(assess(&s, ready(&store), None).plan, PLAN_NONE);
}

// Review on #600: only `+`/`-` lines were scanned for destructive markers, so a change that only
// loosens the guard around an existing destructive call (the call itself sits on a context line)
// read as a plain small change. Context lines of a behavioural hunk are scanned too; added/removed
// counts still come from `+`/`-` only.
#[test]
fn guard_change_around_context_line_destructive_call_is_destructive() {
    let hunk = "\
@@ -1,4 +1,4 @@
-if confirmed {
+if true {
     std::fs::remove_dir(path)?;
 }
";
    // The exact fixture from the review (headerless, so it counts as code).
    let s = signals_from_diff(hunk);
    assert_eq!((s.lines_added, s.lines_removed), (1, 1), "{s:?}");
    assert!(s.destructive, "remove_dir on a context line: {s:?}");

    // The same hunk as a small one-file code change.
    let s = signals_from_diff(&file_diff("src/sweep.rs", hunk));
    assert_eq!(
        (s.lines_added, s.lines_removed, s.code_files),
        (1, 1, 1),
        "{s:?}"
    );
    assert!(s.destructive, "remove_dir on a context line: {s:?}");
    let store = graph(&[], &[]);
    assert_eq!(assess(&s, ready(&store), None).plan, PLAN_MOST);
}

// ── T2 (DES-TEAMING-002 §8.5): the floor table, the high-risk rule, the intent score ─────────

/// T2 (a): scores 10/30/50/80 land in §8.5's four rows, each with its floor and high-risk flag.
#[test]
fn t2_a_each_band_has_the_section_8_5_floor_and_high_risk() {
    let row = |f: Floor| (f.band, f.phases, f.high_risk);
    assert_eq!(
        row(floor_for(10, false)),
        ("0-19".to_string(), vec!["build", "deliver"], false)
    );
    assert_eq!(
        row(floor_for(30, false)),
        (
            "20-39".to_string(),
            vec!["build", "review", "deliver"],
            false
        )
    );
    assert_eq!(
        row(floor_for(50, false)),
        (
            "40-69".to_string(),
            vec!["test_plan", "design", "build", "review", "deliver"],
            false
        )
    );
    assert_eq!(
        row(floor_for(80, false)),
        (
            "70-100".to_string(),
            vec![
                "test_plan",
                "design",
                "architecture",
                "build",
                "review",
                "security_review",
                "deliver"
            ],
            true
        )
    );
}

/// T2 (a): with the destructive floor tuned to 0, a destructive change can score 10; it keeps
/// the 0-19 floor and is still high risk (the second rule of §8.5's high-risk definition).
#[test]
fn t2_a_a_destructive_signal_at_score_10_is_high_risk_on_the_band_floor() {
    let tuned = Thresholds {
        destructive_floor: 0,
        ..THRESHOLDS
    };
    let signals = ImpactSignals {
        changed_symbols: 0,
        products: 2,
        destructive: true,
        ..Default::default()
    };
    let score = impact_score_in(&tuned, &signals).score;
    assert_eq!(score, 10);
    let f = floor_in(&tuned, score, true);
    assert_eq!(
        (f.band.as_str(), f.phases, f.high_risk),
        ("0-19", vec!["build", "deliver"], true)
    );
    // The same score without the signal is not high risk.
    assert!(!floor_in(&tuned, score, false).high_risk);
}

/// Every band edge reads the right row (the floor table follows the bands table exactly).
#[test]
fn t2_floor_band_edges() {
    for (score, band) in [
        (0, "0-19"),
        (19, "0-19"),
        (20, "20-39"),
        (39, "20-39"),
        (40, "40-69"),
        (69, "40-69"),
        (70, "70-100"),
        (100, "70-100"),
    ] {
        assert_eq!(floor_for(score, false).band, band, "score {score}");
    }
}

/// `signals_from_paths`: each declared path classified as a diff header path would be, touched as
/// a whole file (no lines), with the critical and destructive path markers applied.
#[test]
fn t2_signals_from_paths_classifies_each_declared_path() {
    let s = signals_from_paths(&["src/x.rs", "README.md", "tests/a.rs", "Cargo.toml"]);
    assert_eq!(
        (s.code_files, s.docs_files, s.test_files, s.config_files),
        (1, 1, 1, 1)
    );
    assert_eq!((s.lines_added, s.lines_removed), (0, 0));
    assert!(!s.critical && !s.destructive, "{s:?}");
    let touched: Vec<_> = s
        .touched
        .iter()
        .map(|f| (f.path.as_str(), f.old_path.as_str(), f.old_lines.len()))
        .collect();
    assert_eq!(
        touched,
        [
            ("src/x.rs", "src/x.rs", 0),
            ("tests/a.rs", "tests/a.rs", 0),
            ("Cargo.toml", "Cargo.toml", 0)
        ]
    );
    let critical = signals_from_paths(&["src/memory.rs"]);
    assert!(critical.critical && !critical.destructive, "{critical:?}");
    let destructive = signals_from_paths(&["db/migrations/001_init.sql"]);
    assert!(destructive.destructive, "{destructive:?}");
    assert!(!signals_from_paths(&["docs/guide.md"]).behavioural());
}

/// T2 (g), the scoring half: a creator plan with `touch` omitted, and again with `touch: []`,
/// scores `no_graph_score` (100) with the reason "no declared scope", so it lands in 70-100.
#[test]
fn t2_g_a_creator_plan_with_no_declared_scope_scores_100() {
    let store = imported_file_graph("src/x.rs");
    for touch in [None, Some(&[][..])] {
        let a = assess_intent(true, touch, ready(&store), None);
        assert_eq!((a.deterministic, a.score), (100, 100), "{touch:?}: {a:?}");
        assert_eq!(a.reasons, ["no declared scope"], "{touch:?}");
        assert_eq!(a.plan, PLAN_MOST);
        assert!(a.signals.is_none());
        assert!(floor_for(a.score, false).high_risk);
    }
}

/// T2 (g): the same plan with `touch: ["src/x.rs"]` scores from the graph (40 importers: reach 60,
/// test gap +20), not from the no-scope rule.
#[test]
fn t2_g_a_declared_touch_set_scores_from_the_graph() {
    let store = imported_file_graph("src/x.rs");
    let a = assess_intent(true, Some(&["src/x.rs"]), ready(&store), None);
    assert_eq!(a.score, 80, "{a:?}");
    let s = a.signals.as_ref().expect("graph was read");
    assert_eq!((s.changed_symbols, s.dependents), (1, 40), "{s:?}");
    assert!(a.reasons[0].starts_with("reach 60:"), "{:?}", a.reasons);
    assert!(!a.reasons.iter().any(|r| r == NO_DECLARED_SCOPE));
    // No usable graph still fails closed, with the graph's reason, not "no declared scope".
    let closed = assess_intent(
        true,
        Some(&["src/x.rs"]),
        Graph::Unavailable("no repo".into()),
        None,
    );
    assert_eq!(closed.score, 100);
    assert_eq!(closed.reasons, ["fail closed at 100: no repo"]);
}

/// T2 (g), (d): a plan with no creator step and no touch set scores 0.
#[test]
fn t2_g_a_plan_with_no_creator_and_no_touch_scores_0() {
    for touch in [None, Some(&[][..])] {
        let a = assess_intent(false, touch, Graph::Unavailable("none".into()), None);
        assert_eq!((a.deterministic, a.score), (0, 0), "{a:?}");
        assert_eq!(a.plan, PLAN_NONE);
        assert_eq!(a.reasons, ["no creator step and no declared scope"]);
    }
}

/// T2 (f): the band numbers live in `THRESHOLDS` only. No code line (comments and tests aside)
/// outside the `THRESHOLDS` table, in this module or in plan floor fill, spells a band edge.
#[test]
fn t2_f_band_numbers_live_only_in_thresholds() {
    let edges = ["19", "20", "39", "40", "69", "70"];
    let strip = |src: &'static str| -> Vec<(usize, String)> {
        let body = src.split("#[cfg(test)]").next().unwrap_or(src);
        let mut out = Vec::new();
        let mut in_table = false;
        for (n, line) in body.lines().enumerate() {
            if line.starts_with("pub(crate) const THRESHOLDS") {
                in_table = true;
            }
            if in_table {
                if line == "};" {
                    in_table = false;
                }
                continue;
            }
            let code = line.split("//").next().unwrap_or("");
            out.push((n + 1, code.to_string()));
        }
        out
    };
    let mut hits = Vec::new();
    for (file, src) in [
        ("src/review_scale.rs", include_str!("../review_scale.rs")),
        ("src/plan.rs", include_str!("../plan.rs")),
    ] {
        for (n, code) in strip(src) {
            let tokens = code.split(|c: char| !c.is_ascii_alphanumeric() && c != '_');
            for tok in tokens {
                if edges.contains(&tok) {
                    hits.push(format!("{file}:{n}: {}", code.trim()));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "band numbers outside THRESHOLDS: {hits:#?}"
    );
}

/// T2 (g) through T3's launch scorer (`plan_gate::intent_score`, the one the engine calls at
/// launch): a creator plan with `touch: ["src/x.rs"]` scores from the graph (80, as
/// `t2_g_a_declared_touch_set_scores_from_the_graph`), and the same plan without `touch` scores 100
/// with "no declared scope"; an understand-only plan scores 0.
#[test]
fn t3_the_launch_scorer_reads_the_graph_for_a_declared_touch_set() {
    let store = imported_file_graph("src/x.rs");
    let plan =
        |v: serde_json::Value| -> crate::plan::PlanSteps { serde_json::from_value(v).unwrap() };
    let touched = crate::plan_gate::intent_score(
        &plan(
            serde_json::json!({"steps": [{"catalog": "build", "id": "build"}], "touch": ["src/x.rs"]}),
        ),
        ready(&store),
    );
    assert_eq!(touched.assessment.score, 80, "{:?}", touched.assessment);
    assert!(touched.assessment.signals.is_some(), "the graph was read");
    assert!(!touched.destructive);
    let none = crate::plan_gate::intent_score(
        &plan(serde_json::json!({"steps": [{"catalog": "build", "id": "build"}]})),
        ready(&store),
    );
    assert_eq!(none.assessment.score, 100);
    assert_eq!(none.assessment.reasons, [NO_DECLARED_SCOPE]);
    let read_only = crate::plan_gate::intent_score(
        &plan(serde_json::json!({"steps": [{"catalog": "understand", "id": "u"}]})),
        Graph::Unavailable("none".into()),
    );
    assert_eq!(read_only.assessment.score, 0);
    // A destructive touched path is high risk even when the graph is unusable (the signal is
    // path-derived, positive evidence), so it never rides on the graph being readable.
    let destructive = crate::plan_gate::intent_score(
        &plan(
            serde_json::json!({"steps": [{"catalog": "build", "id": "build"}], "touch": ["db/migrations/001_init.sql"]}),
        ),
        Graph::Unavailable("none".into()),
    );
    assert!(destructive.destructive);
}

/// Rig run 1a22f803 (D9): the PA scoped a docs-only design as
/// `docs/design/studio-redesign/{spec.md,prototype.html}`. `.html` read as CODE, so the touch set
/// was behavioural, read a graph indexed at another commit, and failed closed at 100 (the full
/// floor for a design doc). A design doc or prototype under a docs directory, and an image
/// anywhere, is docs: the graph is not consulted, so a stale one cannot fail it closed.
#[test]
fn d9_a_docs_only_design_scope_never_reads_a_stale_graph() {
    let mut stale = imported_file_graph("src/x.rs");
    indexed_at(&mut stale, HEAD); // not the run base
    let docs: &[&str] = &[
        "docs/design/studio-redesign/spec.md",
        "docs/design/studio-redesign/prototype.html",
        "docs/design/studio-redesign/hero.PNG",
        "docs/design/studio-redesign/flow.svg",
        "assets/logo.png",
    ];
    assert!(!signals_from_paths(docs).behavioural());
    let a = assess_intent(true, Some(docs), ready(&stale), None);
    assert_eq!((a.deterministic, a.score), (0, 0), "{a:?}");
    assert_eq!(a.reasons, ["docs-only: no symbols"]);
    let floor = floor_for(a.score, false);
    assert_eq!(
        (floor.phases, floor.high_risk),
        (vec!["build", "deliver"], false)
    );
    // A behavioural file on the same stale graph still fails closed at 100.
    for code in [&["src/x.rs"][..], &["src/app.ts", "docs/a.md"][..]] {
        let a = assess_intent(true, Some(code), ready(&stale), None);
        assert_eq!(a.score, 100, "{code:?}: {a:?}");
        assert!(
            a.reasons[0].starts_with("fail closed at 100: graph indexed at"),
            "{a:?}"
        );
    }
    // HTML or SVG outside a docs directory is still code (a web app's entry page, an asset).
    for code in [
        "index.html",
        "src/index.html",
        "public/docsy.html",
        "public/logo.svg",
    ] {
        assert!(signals_from_paths(&[code]).behavioural(), "{code}");
    }
}

// ── QE waiver: complexity and novelty (operator correction 2026-10-10) ──────────────────────

/// One term each, through the pure table: every term adds its points AND a reason line naming it.
#[test]
fn complexity_and_novelty_terms_table() {
    let leaf = || ImpactSignals {
        changed_symbols: 1,
        products: 1,
        ..Default::default()
    };
    let cases: &[(&str, ImpactSignals, u8, &str)] = &[
        ("a small leaf edit: reach alone", leaf(), 20, "reach 20"),
        (
            "50 changed lines: lowest band",
            ImpactSignals {
                lines_changed: 50,
                ..leaf()
            },
            20,
            "reach 20",
        ),
        (
            "51 changed lines",
            ImpactSignals {
                lines_changed: 51,
                ..leaf()
            },
            30,
            "complexity +10: 51 changed line(s)",
        ),
        (
            "201 changed lines",
            ImpactSignals {
                lines_changed: 201,
                ..leaf()
            },
            40,
            "complexity +20: 201 changed line(s)",
        ),
        (
            "5 branch lines: lowest band",
            ImpactSignals {
                branch_lines: 5,
                ..leaf()
            },
            20,
            "reach 20",
        ),
        (
            "6 branch lines",
            ImpactSignals {
                branch_lines: 6,
                ..leaf()
            },
            30,
            "complexity +10: 6 changed branch line(s)",
        ),
        (
            "21 branch lines",
            ImpactSignals {
                branch_lines: 21,
                ..leaf()
            },
            40,
            "complexity +20: 21 changed branch line(s)",
        ),
        (
            "4 changed symbols",
            ImpactSignals {
                changed_symbols: 4,
                ..leaf()
            },
            30,
            "complexity +10: 4 changed symbol(s)",
        ),
        (
            "complexity caps at 30",
            ImpactSignals {
                lines_changed: 900,
                branch_lines: 90,
                changed_symbols: 9,
                ..leaf()
            },
            50,
            "complexity +10: 90 changed branch line(s)",
        ),
        (
            "one unindexed file",
            ImpactSignals {
                unindexed: 1,
                ..leaf()
            },
            30,
            "novelty +10: 1 new or unindexed file(s)",
        ),
        (
            "five unindexed files cap at two steps",
            ImpactSignals {
                unindexed: 5,
                ..leaf()
            },
            40,
            "novelty +20: 5 new or unindexed file(s)",
        ),
        (
            "a new dependency",
            ImpactSignals {
                new_dependencies: 1,
                ..leaf()
            },
            40,
            "novelty +20: 1 new dependency(ies)",
        ),
        (
            "a new public symbol",
            ImpactSignals {
                new_public_symbols: 2,
                ..leaf()
            },
            30,
            "novelty +10: 2 new public or wire symbol(s)",
        ),
        (
            "a low-history path",
            ImpactSignals {
                low_history: 1,
                ..leaf()
            },
            30,
            "novelty +10: 1 touched path(s) with under 3 commits",
        ),
        (
            "novelty caps at 40",
            ImpactSignals {
                unindexed: 3,
                new_dependencies: 2,
                new_public_symbols: 1,
                low_history: 1,
                ..leaf()
            },
            60,
            "novelty +20: 2 new dependency(ies)",
        ),
    ];
    for (name, s, want, reason) in cases {
        let got = impact_score(s);
        assert_eq!(got.score, *want, "{name}: {s:?} -> {got:?}");
        assert!(
            got.reasons.iter().any(|r| r.starts_with(reason)),
            "{name}: a reason starting {reason:?} in {:?}",
            got.reasons
        );
    }
}

fn assessed(s: ImpactSignals) -> Assessment {
    let sc = impact_score(&s);
    Assessment {
        deterministic: sc.score,
        score: sc.score,
        reasons: sc.reasons,
        model: None,
        signals: Some(s),
        plan: plan_for(sc.score),
    }
}

/// The waiver needs EVERY dimension in its lowest band: the reach-only score (20) or docs-only
/// (0), with no complexity and no novelty term. Each other term alone makes QE required.
#[test]
fn qe_waiver_needs_every_dimension_in_its_lowest_band() {
    let leaf = ImpactSignals {
        changed_symbols: 1,
        products: 1,
        ..Default::default()
    };
    assert_eq!(qe_waivable(&assessed(leaf.clone())), Ok(()));
    assert_eq!(
        qe_waivable(&assessed(ImpactSignals::default())),
        Ok(()),
        "docs-only"
    );
    let required: &[(&str, ImpactSignals)] = &[
        (
            "6 dependents (reach tier 2)",
            ImpactSignals {
                dependents: 6,
                ..leaf.clone()
            },
        ),
        (
            "two products",
            ImpactSignals {
                products: 2,
                ..leaf.clone()
            },
        ),
        (
            "contract",
            ImpactSignals {
                contract_change: true,
                ..leaf.clone()
            },
        ),
        (
            "test gap",
            ImpactSignals {
                dependents: 1,
                test_gap: 0.1,
                ..leaf.clone()
            },
        ),
        (
            "critical",
            ImpactSignals {
                critical: true,
                ..leaf.clone()
            },
        ),
        (
            "destructive",
            ImpactSignals {
                destructive: true,
                ..leaf.clone()
            },
        ),
        (
            "51 changed lines",
            ImpactSignals {
                lines_changed: 51,
                ..leaf.clone()
            },
        ),
        (
            "6 branch lines",
            ImpactSignals {
                branch_lines: 6,
                ..leaf.clone()
            },
        ),
        (
            "4 changed symbols",
            ImpactSignals {
                changed_symbols: 4,
                ..leaf.clone()
            },
        ),
        (
            "a new file",
            ImpactSignals {
                unindexed: 1,
                ..leaf.clone()
            },
        ),
        (
            "a new dependency",
            ImpactSignals {
                new_dependencies: 1,
                ..leaf.clone()
            },
        ),
        (
            "a new public symbol",
            ImpactSignals {
                new_public_symbols: 1,
                ..leaf.clone()
            },
        ),
        (
            "a low-history path",
            ImpactSignals {
                low_history: 1,
                ..leaf.clone()
            },
        ),
    ];
    for (name, s) in required {
        let why = qe_waivable(&assessed(s.clone())).expect_err(name);
        assert!(
            why.contains("above the waiver line 20"),
            "{name}: the reason names the line: {why}"
        );
    }
    // The fail-closed score (no usable graph) is never a waiver.
    let a = assess(
        &signals_from_diff(SMALL_CODE),
        Graph::Unavailable("no graph".into()),
        None,
    );
    assert!(
        qe_waivable(&a).unwrap_err().contains("could not be read"),
        "{a:?}"
    );
}

/// The operator's floor: a brand-new file that adds a dependency is never waivable, even with a
/// blast radius of zero (nothing depends on it, nothing is critical).
#[test]
fn a_new_file_with_a_new_dependency_is_never_waivable() {
    let d = r#"diff --git a/src/leaf.rs b/src/leaf.rs
new file mode 100644
--- /dev/null
+++ b/src/leaf.rs
@@ -0,0 +1,2 @@
+use serde_json::json;
+fn leaf() {}
diff --git a/Cargo.toml b/Cargo.toml
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -8,2 +8,3 @@
 [dependencies]
 anyhow = "1"
+serde_json = "1"
"#;
    let diff = signals_from_diff(d);
    assert_eq!(
        diff.new_dependencies,
        BTreeSet::from(["serde_json".to_string()])
    );
    let store = graph(
        &[node("other", NodeKind::Function, "src/other.rs", (1, 5))],
        &[],
    );
    let a = assess(&diff, ready(&store), None);
    let s = a.signals.as_ref().expect("graph was read");
    assert_eq!(s.dependents, 0, "zero blast radius: {s:?}");
    assert!(a.score > THRESHOLDS.qe_waiver_max_score, "{a:?}");
    let why = qe_waivable(&a).unwrap_err();
    assert!(
        why.contains("new dependency") && why.contains("new or unindexed file"),
        "{why}"
    );
}

/// A one-line edit inside an existing leaf function with nothing depending on it is waivable;
/// docs-only is too.
#[test]
fn a_small_leaf_edit_and_docs_only_are_waivable() {
    let store = graph(
        &[node("plan", NodeKind::Function, "src/plan.rs", (10, 13))],
        &[],
    );
    let a = assess(&signals_from_diff(SMALL_CODE), ready(&store), None);
    assert_eq!(a.score, 20, "{a:?}");
    assert_eq!(qe_waivable(&a), Ok(()), "{a:?}");
    let a = assess(&signals_from_diff(DOCS_ONLY), ready(&store), None);
    assert_eq!((a.score, qe_waivable(&a)), (0, Ok(())), "{a:?}");
}

/// Dependency adds are read per manifest/lockfile format; a version bump (the key removed and
/// re-added) is not new; a `requirements.txt` is never docs.
#[test]
fn new_dependencies_are_read_from_manifests_and_lockfiles() {
    let cases: &[(&str, &str, &[&str])] = &[
        ("Cargo.toml", "@@ -1,3 +1,4 @@\n [dev-dependencies]\n+tempfile = \"3\"\n-anyhow = \"1.0\"\n+anyhow = \"1.1\"\n", &["tempfile"]),
        ("Cargo.toml", "@@ -1,2 +1,3 @@\n [package]\n+version = \"0.2.0\"\n", &[]),
        ("Cargo.toml", "@@ -9,1 +9,2 @@\n+[dependencies.tokio]\n+version = \"1\"\n", &["tokio"]),
        ("package.json", "@@ -4,2 +4,4 @@\n   \"dependencies\": {\n+    \"left-pad\": \"^1.3.0\",\n+    \"version\": \"1.0.0\",\n+    \"build\": \"tsc -p .\",\n", &["build", "left-pad", "version"]),
        ("go.mod", "@@ -3,1 +3,2 @@\n require (\n+\tgithub.com/pkg/errors v0.9.1\n", &["github.com/pkg/errors"]),
        ("requirements.txt", "@@ -1 +1,2 @@\n flask==2.0\n+requests>=2.31\n", &["requests"]),
        ("Cargo.lock", "@@ -10,0 +11,3 @@\n+[[package]]\n+name = \"itoa\"\n+version = \"1.0.0\"\n", &["itoa"]),
        ("package-lock.json", "@@ -10,0 +11,2 @@\n+    \"node_modules/left-pad\": {\n+      \"version\": \"1.3.0\",\n", &["left-pad"]),
        ("go.sum", "@@ -1,0 +2 @@\n+github.com/pkg/errors v0.9.1 h1:abc=\n", &["github.com/pkg/errors"]),
    ];
    for (file, hunk, want) in cases {
        let d = signals_from_diff(&file_diff(file, hunk));
        let want: BTreeSet<String> = want.iter().map(|s| s.to_string()).collect();
        assert_eq!(d.new_dependencies, want, "{file}: {hunk}");
        assert!(d.behavioural(), "{file} is never docs");
    }
}

/// New public symbols per language; a signature edit (removed and re-added) is not new; a crate-
/// or module-private item, an indented Python def, a lowercase Go func are not public.
#[test]
fn new_public_symbols_are_read_per_language() {
    let cases: &[(&str, &str, &[&str])] = &[
        ("src/a.rs", "@@ -1 +1,4 @@\n+pub fn alpha() {}\n+pub(crate) fn beta() {}\n+pub struct Gamma;\n+    pub delta: u32,\n", &["alpha", "Gamma", ".delta"]),
        ("src/a.rs", "@@ -1 +1 @@\n-pub fn alpha(x: u8) {}\n+pub fn alpha(x: u16) {}\n", &[]),
        ("src/a.ts", "@@ -1 +1,3 @@\n+export async function load() {}\n+export interface Shape {}\n+const local = 1;\n", &["load", "Shape"]),
        ("lib/a.py", "@@ -1 +1,3 @@\n+def public():\n+def _private():\n+    def nested():\n", &["public"]),
        ("pkg/a.go", "@@ -1 +1,3 @@\n+func Exported() {}\n+func local() {}\n+func (s *Svc) Serve() {}\n", &["Exported", "Serve"]),
    ];
    for (file, hunk, want) in cases {
        let d = signals_from_diff(&file_diff(file, hunk));
        let want: BTreeSet<String> = want.iter().map(|s| s.to_string()).collect();
        assert_eq!(d.new_public_symbols, want, "{file}: {hunk}");
    }
}

/// Branch lines are changed lines with a control-flow token, matched as words (so `elsewhere`
/// and `iffy` are not branches); context lines and config files do not count.
#[test]
fn branch_lines_count_changed_control_flow_only() {
    let hunk = "@@ -1,4 +1,5 @@\n if keep {\n-    a()\n+    if b && c { d() }\n+    let elsewhere = iffy;\n+    x?;\n }\n";
    let d = signals_from_diff(&file_diff("src/a.rs", hunk));
    assert_eq!((d.branch_lines, d.behavioural_lines), (2, 4), "{d:?}");
    let d = signals_from_diff(&file_diff(
        "config/a.toml",
        "@@ -1 +1 @@\n-if = 1\n+if = 2\n",
    ));
    assert_eq!(d.branch_lines, 0, "{d:?}");
}

/// codex r1 on the QE PR: a binary or mode-only change has no `---`/`+++` header; it was dropped
/// (docs-only, 0, waivable). Its paths come from the `Binary files` line or the `diff --git` line.
#[test]
fn binary_and_mode_only_changes_are_never_dropped() {
    let added = "diff --git a/assets/app.wasm b/assets/app.wasm\nnew file mode 100644\nindex 0000000..1111111\nBinary files /dev/null and b/assets/app.wasm differ\n";
    let d = signals_from_diff(added);
    assert!(d.behavioural(), "{d:?}");
    assert_eq!(d.touched[0].path, "assets/app.wasm");
    assert_eq!(d.touched[0].old_path, "", "a new binary has no base side");
    let deleted = "diff --git a/bin/tool b/bin/tool\ndeleted file mode 100755\nindex 1111111..0000000\nBinary files a/bin/tool and /dev/null differ\n";
    let d = signals_from_diff(deleted);
    assert!(d.behavioural() && d.destructive, "{d:?}");
    let mode = "diff --git a/scripts/run.sh b/scripts/run.sh\nold mode 100644\nnew mode 100755\n";
    let d = signals_from_diff(mode);
    assert!(d.behavioural(), "{d:?}");
    assert_eq!(d.touched[0].old_path, "scripts/run.sh");
    // An unparseable header still counts (fail closed): code with no path.
    let odd = "diff --git a/x b/y b/z\nold mode 100644\nnew mode 100755\n";
    assert!(signals_from_diff(odd).behavioural());
}

/// codex r1: a TOML section seen in one hunk must not cover a later hunk that does not show its
/// own header; there the key counts when it is dependency-shaped.
#[test]
fn a_toml_section_does_not_leak_into_the_next_hunk() {
    let hunk = "@@ -1,2 +1,2 @@\n [package]\n-version = \"0.1.0\"\n+version = \"0.2.0\"\n@@ -20,1 +20,2 @@\n anyhow = \"1\"\n+tokio = \"1\"\n";
    let d = signals_from_diff(&file_diff("Cargo.toml", hunk));
    assert_eq!(
        d.new_dependencies,
        BTreeSet::from(["tokio".to_string()]),
        "{d:?}"
    );
}

/// codex r1: inside a dependency object every key is a dependency, whatever its version syntax or
/// name; a hunk that never shows its object counts anything but a known top-level field.
#[test]
fn npm_dependencies_are_read_by_object_not_by_version_syntax() {
    let hunk = "@@ -4,3 +4,6 @@\n   \"dependencies\": {\n+    \"left-pad\": \"latest\",\n+    \"node\": \"^20\",\n   },\n   \"scripts\": {\n+    \"build\": \"tsc\",\n";
    let d = signals_from_diff(&file_diff("package.json", hunk));
    assert_eq!(
        d.new_dependencies,
        BTreeSet::from(["left-pad".to_string(), "node".to_string()]),
        "{d:?}"
    );
    let blind = "@@ -9,1 +9,2 @@\n     \"a\": \"1.0.0\",\n+    \"b\": \"next\",\n";
    assert_eq!(
        signals_from_diff(&file_diff("package.json", blind)).new_dependencies,
        BTreeSet::from(["b".to_string()])
    );
}

/// codex r1: a re-export widens the public surface.
#[test]
fn re_exports_are_new_public_symbols() {
    let rs_ = signals_from_diff(&file_diff(
        "src/lib.rs",
        "@@ -1 +1,2 @@\n mod inner;\n+pub use inner::Thing;\n",
    ));
    assert_eq!(rs_.new_public_symbols.len(), 1, "{rs_:?}");
    let ts = signals_from_diff(&file_diff("src/index.ts", "@@ -1 +1,3 @@\n import x from './x';\n+export { a, b } from './ab';\n+export * from './all';\n"));
    assert_eq!(ts.new_public_symbols.len(), 2, "{ts:?}");
}

/// codex r2: a member added inside an existing multi-line re-export group is a new public symbol,
/// though its line repeats neither `pub` nor `export`.
#[test]
fn a_member_added_to_a_multi_line_export_group_is_new() {
    let rs_ = signals_from_diff(&file_diff(
        "src/lib.rs",
        "@@ -1,4 +1,5 @@\n pub use inner::{\n     A,\n+    B,\n };\n fn x() {}\n",
    ));
    assert_eq!(
        rs_.new_public_symbols,
        BTreeSet::from(["use inner::B".to_string()]),
        "{rs_:?}"
    );
    let ts = signals_from_diff(&file_diff(
        "src/index.ts",
        "@@ -1,3 +1,4 @@\n export {\n   a,\n+  b,\n } from './ab';\n",
    ));
    assert_eq!(
        ts.new_public_symbols,
        BTreeSet::from(["export b".to_string()]),
        "{ts:?}"
    );
    // Re-ordering a member within the group is not new.
    let moved = signals_from_diff(&file_diff(
        "src/lib.rs",
        "@@ -1,4 +1,4 @@\n pub use inner::{\n-    A,\n     B,\n+    A,\n };\n",
    ));
    assert!(moved.new_public_symbols.is_empty(), "{moved:?}");
}

/// codex r2: the two sides of a hunk keep their own object state — a removed `"scripts": {` must
/// not make the added dependency lines below it read as scripts.
#[test]
fn each_side_of_a_hunk_keeps_its_own_object() {
    let hunk = "@@ -3,6 +3,4 @@\n   \"dependencies\": {\n-  },\n-  \"scripts\": {\n-    \"build\": \"tsc\"\n+    \"b\": \"^1\"\n   }\n";
    let d = signals_from_diff(&file_diff("package.json", hunk));
    assert_eq!(
        d.new_dependencies,
        BTreeSet::from(["b".to_string()]),
        "{d:?}"
    );
}
