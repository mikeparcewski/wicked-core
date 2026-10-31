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
use wicked_estate_core::ValidationClaim;

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
            .set_node_semantics(
                &n.symbol,
                None,
                Some("REQ-1"),
                Some(&ValidationClaim::new(true, "test-fixture").unwrap()),
            )
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

/// Seed `n` behavior nodes, the first `accounted` of them mapped (validated requirement + rule).
fn seed_many(db: &str, n: usize, accounted: usize) {
    let mut store = open_store(Some(db)).unwrap();
    let nodes: Vec<Node> = (0..n)
        .map(|i| {
            Node::new(
                synthetic_symbol("code", &format!("op{i}")),
                NodeKind::Function,
                format!("op{i}"),
                Language::new(SYMBOL_SCHEME),
                Location::new("billing/ops.rs".to_string(), Span::ZERO),
            )
        })
        .collect();
    store.begin_batch().unwrap();
    store.upsert_nodes(&nodes).unwrap();
    store.commit_batch().unwrap();
    for n in nodes.iter().take(accounted) {
        store
            .set_node_semantics(
                &n.symbol,
                None,
                Some(&format!("REQ for {}", n.name)),
                Some(&ValidationClaim::new(true, "test-fixture").unwrap()),
            )
            .unwrap();
        store
            .annotate(
                &n.symbol,
                Annotation::new("business_rule", "r", format!("{} holds", n.name))
                    .with_confidence(0.9),
            )
            .unwrap();
    }
}

/// Operator ruling 2026-10-31: the graph is built once coverage reaches the configured minimum
/// (default 80%), labelled with its share, and saved INTO the estate store — no JSON file in the
/// checkout (operator ruling 2026-10-31); `--out` is the explicit export.
#[test]
fn domain_graph_builds_at_the_minimum_into_estate_and_writes_no_file() {
    let dir = scratch("min80");
    let db = dir.join("estate.db");
    seed_many(db.to_str().unwrap(), 5, 4); // 80% mapped
    let root = dir.join("repo");
    std::fs::create_dir_all(&root).unwrap();

    let out = Command::new(BIN)
        .env_remove("WICKED_REQUIREMENTS_MIN_COVERAGE_PCT")
        .args([
            "domain-graph",
            "--db",
            db.to_str().unwrap(),
            "--repo-root",
            root.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "80% mapped meets the default minimum: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !root.join(".wicked-estate").exists(),
        "no requirements JSON in the checkout"
    );
    let store = wicked_apps_core::open_store_ro(Some(db.to_str().unwrap())).unwrap();
    let report = wicked_governance::recompute_front_half_coverage(&store).unwrap();
    let label = wicked_governance::model_coverage(&report, 80);
    let graph = wicked_governance::load_domain_graph(&store, Some(&label))
        .unwrap()
        .expect("the graph is in estate");
    let cov = &graph["metadata"]["coverage"];
    assert_eq!(cov["mapped_pct"], 80, "{cov}");
    assert_eq!(cov["min_pct"], 80);
    assert_eq!(cov["behavior_bearing"], 5);
    assert_eq!(cov["mapped"], 4);
    assert_eq!(cov["unmapped"], 1);
    let reqs = graph["domains"]["billing"]["requirements"]
        .as_object()
        .unwrap();
    assert_eq!(
        reqs.len(),
        4,
        "only the mapped behaviours become requirements"
    );
    assert!(
        reqs.values()
            .all(|r| r["corroboration"]["status"] == "unconfirmed"
                && r["corroboration"]["why"]["code"] == "dead_code"),
        "nothing calls these: unconfirmed, and why: {reqs:?}"
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("80% mapped, minimum 80%"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn domain_graph_refuses_below_the_configured_minimum_and_says_how_far_it_got() {
    let dir = scratch("min90");
    let db = dir.join("estate.db");
    seed_many(db.to_str().unwrap(), 5, 4); // 80% mapped
    let rg = dir.join("rg.json");
    for (flag, env) in [(Some("90"), None), (None, Some("90"))] {
        let mut cmd = Command::new(BIN);
        cmd.env_remove("WICKED_REQUIREMENTS_MIN_COVERAGE_PCT");
        if let Some(e) = env {
            cmd.env("WICKED_REQUIREMENTS_MIN_COVERAGE_PCT", e);
        }
        cmd.args([
            "domain-graph",
            "--db",
            db.to_str().unwrap(),
            "--out",
            rg.to_str().unwrap(),
        ]);
        if let Some(f) = flag {
            cmd.args(["--min-coverage-pct", f]);
        }
        let out = cmd.output().unwrap();
        assert!(!out.status.success(), "80% is below a 90% minimum");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("80% mapped, minimum 90% (4 of 5 behaviours)"),
            "names how far it got: {err}"
        );
        assert!(err.contains("kept in the estate store"), "{err}");
        assert!(!rg.exists(), "no graph below the minimum");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn coverage_min_check_exits_3_below_the_minimum_after_writing_the_report() {
    let dir = scratch("check");
    let db = dir.join("estate.db");
    seed_many(db.to_str().unwrap(), 5, 4);
    let out_file = dir.join("coverage-report.json");
    let run = |min: &str| {
        Command::new(BIN)
            .env_remove("WICKED_REQUIREMENTS_MIN_COVERAGE_PCT")
            .args([
                "coverage",
                "--db",
                db.to_str().unwrap(),
                "--out",
                out_file.to_str().unwrap(),
                "--min-coverage-pct",
                min,
            ])
            .output()
            .unwrap()
    };
    let below = run("85");
    assert_eq!(below.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&below.stderr).contains("80% mapped, minimum 85%"));
    assert!(out_file.is_file(), "the report is written either way");
    assert!(run("80").status.success());
    let junk = run("eighty");
    assert_eq!(
        junk.status.code(),
        Some(1),
        "a junk minimum is an error, never a default"
    );
    assert_eq!(run("").status.code(), Some(1), "an empty value is junk too");
    // xreview on core#1215: a SET-but-invalid variable fails closed (exit 1), never becomes 80 —
    // for the bare flag (the validator's call) and for domain-graph alike.
    for cmd in ["coverage", "domain-graph"] {
        let mut c = Command::new(BIN);
        c.env("WICKED_REQUIREMENTS_MIN_COVERAGE_PCT", "95.5")
            .arg(cmd);
        if cmd == "coverage" {
            c.args([
                "--db",
                db.to_str().unwrap(),
                "--out",
                out_file.to_str().unwrap(),
            ]);
            c.arg("--min-coverage-pct");
        } else {
            let rg = dir.join("rg.json");
            c.args(["--db", db.to_str().unwrap(), "--out", rg.to_str().unwrap()]);
        }
        let o = c.output().unwrap();
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(1), "{cmd}: {err}");
        assert!(err.contains("not a whole percentage"), "{cmd}: {err}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ── Test-harness hygiene (core#311) — not a test ─────────────────────────────────────────────
/// Arm the hermetic emit spool BEFORE main (pre-main is single-threaded, so no test thread can
/// race it): engine paths under test fire coarse fire-and-forget `wicked.*` emissions, and with
/// no shared store configured those spool — which must land in a per-process temp file, never in
/// the operator's real `~/.something-wicked/wicked-apps/emit-outbox.ndjson` replay queue. Every
/// binary in this suite carries this block; `harness_hygiene.rs` fails the suite if one is missing.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
