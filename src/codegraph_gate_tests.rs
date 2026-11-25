//! wicked-estate#595 (HOLD gate b of wicked-estate#584): core's readers on a store the codegraph
//! import wrote. The store is estate's gate fixture (`tests/fixtures/codegraph/coverage`), indexed
//! with `--codegraph` by the estate head under test (`scripts/codegraph-fixture-store.py store`).
//! Estate's CI sets WICKED_CODEGRAPH_FIXTURE_STORE, runs this test from core's main with
//! `--nocapture`, and fails when its `codegraph gate: core` line is missing from that output. Without the variable there is nothing to
//! read, and the test says so.

use std::collections::BTreeSet;
use wicked_apps_core::{EdgeKind, GraphRead, SymbolId};
use wicked_estate_core::Direction;

use crate::floor_affected::{cargo_workspace_test, plan, Inputs, Workspace, MODE_FULL};

const STORE_ENV: &str = "WICKED_CODEGRAPH_FIXTURE_STORE";
const BASE: &str = "ts-typescript . . . src/ts/base/Base#";
const SQUARE: &str = "ts-typescript . . . src/ts/shapes/Square#";

#[test]
fn core_reads_the_codegraph_gate_store() {
    let Ok(db) = std::env::var(STORE_ENV) else {
        eprintln!("codegraph gate: {STORE_ENV} is unset, so there is no store to read");
        return;
    };
    let store = wicked_apps_core::open_store_ro(Some(&db)).expect("the gate store opens");

    // Every edge decodes, with codegraph's provenance, for every kind the import maps: the ones
    // codegraph 1.6.4 emits, and (from estate's synthetic-kinds.sql, `codegraph:synthetic`)
    // Overrides, Returns, HasType and Other("exports"), which it never emits.
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
        EdgeKind::Overrides,
        EdgeKind::Returns,
        EdgeKind::HasType,
        EdgeKind::Other("exports".into()),
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

    // review_scale: a change to Base's declaration line reaches its dependents (Square).
    let diff = "diff --git a/src/ts/base.ts b/src/ts/base.ts\n--- a/src/ts/base.ts\n\
                +++ b/src/ts/base.ts\n@@ -4 +4 @@\n-export abstract class Base {\n\
                +export abstract class Base implements Named {\n";
    let signals =
        crate::review_scale::impact_signals(&store, &crate::review_scale::signals_from_diff(diff))
            .expect("impact_signals");
    assert!(signals.changed_symbols >= 1, "{signals:?}");
    assert!(signals.dependents >= 1, "{signals:?}");

    // floor_affected narrows Rust workspaces only, and codegraph never handles Rust, so a
    // codegraph edge never reaches its walk. What it must do with this store: read the graph
    // (no "could not be read") and fall back to the full suite for a file it cannot trace.
    let check = cargo_workspace_test(&["cargo", "test", "--workspace"].map(String::from))
        .expect("a workspace cargo test");
    let ws = Workspace::default();
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
    assert_eq!(p.selection.mode, MODE_FULL, "{:?}", p.selection);
    assert!(
        p.selection.reason.contains("cannot be traced")
            && !p.selection.reason.contains("could not be read"),
        "the graph was read and the TS file is untraceable: {:?}",
        p.selection
    );

    println!(
        "codegraph gate: core read {} edges ({} codegraph kinds), ranked {}, {} dependents",
        edges.len(),
        bound.len(),
        ranked.len(),
        signals.dependents
    );
}
