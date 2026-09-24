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
fn six_hundred_line_new_leaf_file_scores_20() {
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
    assert_eq!(a.score, 20, "{a:?}");
    assert_eq!(a.plan, PLAN_STANDARD);
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
    assert_eq!((a.score, a.plan), (80, PLAN_MOST), "{a:?}");

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
#[test]
fn rename_only_of_a_heavily_imported_module_counts_its_importers() {
    let mut nodes = vec![node("core_file", NodeKind::File, "src/core.rs", (1, 200))];
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
    let store = graph(&nodes, &edges);
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
    assert_eq!(
        (s.changed_symbols, s.dependents, a.score),
        (1, 0, 20),
        "{a:?}"
    );
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
    assert_eq!(a.score, 40, "reach 20 + critical 20: {a:?}");
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
