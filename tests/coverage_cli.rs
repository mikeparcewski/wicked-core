//! core#25 end-to-end: the `coverage` emitter + store-bound `domain-graph` through the REAL binary.
//! Proves (a) `wicked-core coverage` emits a schema-shaped report recomputed FROM the store, and (b)
//! `domain-graph` recomputes coverage from the store as PRIMARY — a lying external `--coverage 1.0` file
//! cannot green-light a graph that actually has an unaccounted behavior node (the trust-boundary fix).

use std::process::Command;
use wicked_apps_core::{
    open_store, synthetic_symbol, GraphWrite, Language, Location, Node, NodeKind, Span,
    SYMBOL_SCHEME,
};
use wicked_estate_core::Annotation;

const BIN: &str = env!("CARGO_BIN_EXE_wicked-core");

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wc-cov-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn seed(db: &str, accounted: bool) {
    let mut store = open_store(Some(db)).unwrap();
    let n = Node::new(
        synthetic_symbol("code", "charge"),
        NodeKind::Function,
        "charge".to_string(),
        Language::new(SYMBOL_SCHEME),
        Location::new("billing/charge.rs".to_string(), Span::ZERO),
    );
    store.begin_batch().unwrap();
    store.upsert_nodes(std::slice::from_ref(&n)).unwrap();
    store.commit_batch().unwrap();
    if accounted {
        // a validated requirement → resolved → coverage 1.0.
        store
            .set_node_semantics(&n.symbol, None, Some("REQ-1"), Some(true))
            .unwrap();
        store
            .annotate(
                &n.symbol,
                Annotation::new("business_rule", "r", "amount > 0").with_confidence(0.9),
            )
            .unwrap();
    }
    // else: a BARE Function → an unaccounted hole → coverage < 1.0.
}

#[test]
fn coverage_emitter_writes_a_store_recomputed_report() {
    let dir = scratch("emit");
    let db = dir.join("estate.db");
    let out = dir.join("coverage-report.json");
    seed(db.to_str().unwrap(), false); // a hole

    let status = Command::new(BIN)
        .args([
            "coverage",
            "--db",
            db.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "coverage emit exits 0");

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    // Schema-shaped integer fields + a real hole.
    assert_eq!(report["behavior_bearing"], 1);
    assert_eq!(report["unaccounted"], 1);
    assert!(
        report["coverage"].as_f64().unwrap() < 1.0,
        "the bare node is a hole"
    );
    assert!(report["unaccounted_nodes"].as_array().unwrap().len() == 1);
    assert!(report["per_app"].is_array(), "per_app is an ARRAY");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn domain_graph_recompute_beats_a_lying_coverage_file() {
    let dir = scratch("lie");
    let db = dir.join("estate.db");
    seed(db.to_str().unwrap(), false); // a hole → real coverage < 1.0

    // A LYING external report claiming full coverage.
    let lie = dir.join("lie.json");
    std::fs::write(
        &lie,
        serde_json::json!({
            "total": 1, "behavior_bearing": 1, "resolved": 1, "risk_flagged": 0,
            "unaccounted": 0, "coverage": 1.0, "resolved_rate": 1.0, "mean_confidence": 1.0,
            "resolve_threshold": 0.75, "per_app": [], "unaccounted_nodes": []
        })
        .to_string(),
    )
    .unwrap();

    let out = Command::new(BIN)
        .args([
            "domain-graph",
            "--db",
            db.to_str().unwrap(),
            "--coverage",
            lie.to_str().unwrap(),
            "--out",
            dir.join("rg.json").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a lying --coverage 1.0 must NOT green-light a graph with a store hole"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("DISAGREES") || err.contains("< 1.0"),
        "fails closed on the store recompute, not the file: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn domain_graph_translates_a_fully_accounted_store_with_no_coverage_file() {
    let dir = scratch("ok");
    let db = dir.join("estate.db");
    let rg = dir.join("rg.json");
    seed(db.to_str().unwrap(), true); // coverage 1.0

    // No --coverage supplied: recompute-from-store stands (not a hard error).
    let out = Command::new(BIN)
        .args([
            "domain-graph",
            "--db",
            db.to_str().unwrap(),
            "--out",
            rg.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "a fully-accounted store translates with NO --coverage file: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(rg.exists(), "requirements_graph.json was written");
    let _ = std::fs::remove_dir_all(&dir);
}
