//! wicked-estate#595 (HOLD gate b of wicked-estate#584): core's readers on a store the codegraph
//! import wrote. The store is estate's gate fixture (`tests/fixtures/codegraph/coverage`), indexed
//! with `--codegraph` by the estate head under test (`scripts/codegraph-fixture-store.py store`).
//! Estate's CI sets WICKED_CODEGRAPH_FIXTURE_STORE, runs this test from core's main, and fails
//! when its `codegraph gate: core` line is missing. Without the variable there is nothing to
//! read, and the test says so.

use std::collections::BTreeSet;
use wicked_apps_core::{EdgeKind, GraphRead, SymbolId};
use wicked_estate_core::Direction;

use crate::floor_affected::{
    cargo_workspace_test, plan, Inputs, Package, Target, TargetKind, Workspace, MODE_AFFECTED,
};

const STORE_ENV: &str = "WICKED_CODEGRAPH_FIXTURE_STORE";
const BASE: &str = "ts-typescript . . . src/ts/base/Base#";
const DESCRIBE: &str = "ts-typescript . . . src/ts/base/Base#describe().";
const SQUARE: &str = "ts-typescript . . . src/ts/shapes/Square#";

/// One package per language directory of the fixture, so a plan names the ones a change reaches.
fn workspace() -> Workspace {
    let pkg = |name: &str| Package {
        name: name.into(),
        dir: format!("src/{name}"),
        targets: vec![Target {
            kind: TargetKind::Lib,
            name: name.into(),
            src: format!("src/{name}/lib.rs"),
        }],
        member_deps: vec![],
    };
    Workspace {
        packages: ["ts", "java", "c", "php"].iter().map(|n| pkg(n)).collect(),
        outside_path_deps: vec![],
    }
}

#[test]
fn core_reads_the_codegraph_gate_store() {
    let Ok(db) = std::env::var(STORE_ENV) else {
        eprintln!("codegraph gate: {STORE_ENV} is unset, so there is no store to read");
        return;
    };
    let store = wicked_apps_core::open_store_ro(Some(&db)).expect("the gate store opens");

    // Every edge decodes, including the `Other(..)` kinds, with codegraph's provenance.
    let edges = store.all_edges().expect("every edge decodes");
    let bound: BTreeSet<String> = edges
        .iter()
        .filter(|e| e.resolved_by.starts_with("codegraph:"))
        .map(|e| format!("{:?}", e.kind))
        .collect();
    for k in [
        EdgeKind::Calls,
        EdgeKind::Imports,
        EdgeKind::Extends,
        EdgeKind::Implements,
        EdgeKind::References,
        EdgeKind::Instantiates,
        EdgeKind::Other("decorates".into()),
    ] {
        assert!(
            bound.contains(&format!("{k:?}")),
            "a codegraph-bound {k:?}: {bound:?}"
        );
    }

    // code_graph: ranking reads the store, and caller spread counts codegraph-bound callers.
    let ranked = crate::code_graph::rank_symbols(&db, 200).expect("rank_symbols");
    assert!(
        ranked.iter().any(|r| r.name == "Square"),
        "Square ranks: {ranked:?}"
    );
    let dependents = store
        .neighbors(&SymbolId(BASE.into()), Direction::Dependents)
        .expect("dependents of Base");
    assert!(
        dependents.iter().any(|e| e.source.0 == SQUARE
            && e.kind == EdgeKind::Extends
            && e.resolved_by.starts_with("codegraph:")),
        "Square extends Base through codegraph: {dependents:?}"
    );

    // review_scale: a change to Base#describe reaches Square.
    let diff = "diff --git a/src/ts/base.ts b/src/ts/base.ts\n--- a/src/ts/base.ts\n\
                +++ b/src/ts/base.ts\n@@ -7 +7 @@\n-    return \"base\";\n+    return \"BASE\";\n";
    let signals =
        crate::review_scale::impact_signals(&store, &crate::review_scale::signals_from_diff(diff))
            .expect("impact_signals");
    assert!(signals.changed_symbols >= 1, "{signals:?}");
    assert!(signals.dependents >= 1, "{signals:?}");

    // floor_affected: the same change plans the packages the walk reaches, from this graph.
    let check = cargo_workspace_test(&["cargo", "test", "--workspace"].map(String::from))
        .expect("a workspace cargo test");
    let ws = workspace();
    let touched = vec!["src/ts/base.ts".to_string()];
    let files: Vec<String> = [
        "src/ts/base.ts",
        "src/ts/shapes.ts",
        "src/java/cov/Dog.java",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let text = |_: &str| Some(String::new());
    let p = plan(&Inputs {
        check: &check,
        touched: &touched,
        removed: &[],
        diff,
        workspace: Ok(&ws),
        graph: Ok(&store),
        base_text: &text,
        head_text: &text,
        files: &files,
        full_when: &[],
    });
    assert_eq!(p.selection.mode, MODE_AFFECTED, "{:?}", p.selection);
    assert!(
        p.selection.changed_symbols >= 1 && p.selection.reached_symbols >= 1,
        "{:?}",
        p.selection
    );
    assert_eq!(
        p.selection.packages,
        vec!["ts".to_string()],
        "{:?}",
        p.selection
    );
    let _ = DESCRIBE;

    println!(
        "codegraph gate: core read {} edges ({} codegraph kinds), ranked {}, planned {:?}",
        edges.len(),
        bound.len(),
        ranked.len(),
        p.selection.packages
    );
}
