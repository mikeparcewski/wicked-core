//! core#25 end-to-end: the `coverage` emitter through the REAL binary: `wicked-core coverage` emits a
//! schema-shaped report recomputed FROM the store. core#1924 removed the 1.1.0 `domain-graph` build
//! and every configured minimum (`--min-coverage-pct`, `requirements-target`, the daemon variable);
//! the last test here pins that each is refused by name.

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

/// Operator decision 2026-11-01 (core#1233): the report counts REAL requirements only. One stated
/// rule and four risk-flagged placeholders read 20% real, never "100% mapped". Two JSON keys
/// (indexed as `Struct`) and a test function are not behaviour-bearing at all, and the placeholders
/// are listed.
#[test]
fn placeholders_are_not_real_and_non_product_files_are_not_counted() {
    let dir = scratch("real");
    let db = dir.join("estate.db");
    let dbs = db.to_str().unwrap();
    let mut store = open_store(Some(dbs)).unwrap();
    let mk = |name: &str, kind: NodeKind, file: &str| {
        Node::new(
            synthetic_symbol("code", name),
            kind,
            name.to_string(),
            Language::new(SYMBOL_SCHEME),
            Location::new(file.to_string(), Span::ZERO),
        )
    };
    let product: Vec<Node> = (0..5)
        .map(|i| mk(&format!("op{i}"), NodeKind::Function, "extensions/gov.js"))
        .collect();
    let other = vec![
        mk("summary", NodeKind::Struct, "evidence/a/summary.json"),
        mk("scripts", NodeKind::Struct, "package.json"),
        mk("capture", NodeKind::Function, "test/live/capture.mjs"),
    ];
    store.begin_batch().unwrap();
    store.upsert_nodes(&product).unwrap();
    store.upsert_nodes(&other).unwrap();
    store.commit_batch().unwrap();
    store
        .annotate(
            &product[0].symbol,
            Annotation::new("business_rule", "r", "a call proceeds only on true")
                .with_confidence(0.9),
        )
        .unwrap();
    for n in product.iter().skip(1).chain(&other) {
        store
            .annotate(
                &n.symbol,
                Annotation::new("risk", "r", format!("[RISK] {}: empty statement", n.name)),
            )
            .unwrap();
    }
    drop(store);
    let out_file = dir.join("coverage-report.json");
    let out = Command::new(BIN)
        .args(["coverage", "--db", dbs, "--out", out_file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(
            "20% real requirements (1 of 5 behaviours; 4 placeholders, 0 unmapped; 3 not product code)"
        ),
        "placeholders do not count: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out_file).unwrap()).unwrap();
    assert_eq!(report["behavior_bearing"], 5, "{report}");
    assert_eq!(report["not_product"], 3, "{report}");
    assert_eq!(
        report["placeholder_nodes"].as_array().unwrap().len(),
        4,
        "{report}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// core#1924: the configured minimum is gone, and every way to ask for it is refused by name before
/// anything is opened (a store that does not exist stays absent): the 1.1.0 `domain-graph` build and
/// its `--coverage` / `--min-coverage-pct` in either spelling, a `--schema-version` with no value or
/// given twice, the same on `--retire`, `coverage --min-coverage-pct` (never an exit 0 a caller would
/// read as "met"), and the `requirements-target` and `seed-domain-validators` commands (usage, exit
/// 2, before the engine starts).
#[test]
fn the_removed_minimum_is_refused_by_name() {
    let dir = scratch("removed");
    let db = dir.join("estate.db");
    seed_many(db.to_str().unwrap(), 5, 4);
    let dbs = db.to_str().unwrap();
    let missing = dir.join("never.db");
    let ms = missing.to_str().unwrap();
    let bus = dir.join("bus.db");
    let root = dir.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let roots = root.to_str().unwrap();
    let run = |args: &[&str]| {
        Command::new(BIN)
            .env("WICKED_BUS_DB", &bus)
            .args(args)
            .output()
            .unwrap()
    };
    let dg = |extra: &[&'static str]| {
        let mut v = vec!["domain-graph", "--db", ms, "--repo-root", roots];
        v.extend_from_slice(extra);
        v
    };
    for (args, says) in [
        (
            dg(&["--schema-version", "1.1.0"]),
            "--schema-version 1.1.0 is not built any more",
        ),
        (
            dg(&["--schema-version=1.1.0"]),
            "--schema-version 1.1.0 is not built any more",
        ),
        (dg(&["--schema-version"]), "--schema-version needs a value"),
        (
            dg(&["--schema-version", "1.2.0", "--schema-version", "1.1.0"]),
            "--schema-version is given more than once",
        ),
        (
            dg(&["--min-coverage-pct", "80"]),
            "--min-coverage-pct was a 1.1.0 option and was removed",
        ),
        (
            dg(&["--min-coverage-pct=80"]),
            "--min-coverage-pct was a 1.1.0 option and was removed",
        ),
        (
            dg(&["--coverage", "c.json"]),
            "--coverage was a 1.1.0 option and was removed",
        ),
        (
            dg(&["--coverage=c.json"]),
            "--coverage was a 1.1.0 option and was removed",
        ),
        (
            dg(&["--retire", "billing::REQ-001", "--min-coverage-pct", "80"]),
            "--min-coverage-pct was a 1.1.0 option and was removed",
        ),
        (
            dg(&["--retire", "billing::REQ-001", "--schema-version", "1.1.0"]),
            "--schema-version 1.1.0 is not built any more",
        ),
        (
            vec!["coverage", "--db", ms, "--json", "--min-coverage-pct", "80"],
            "--min-coverage-pct was removed",
        ),
        (
            vec!["coverage", "--db", ms, "--json", "--min-coverage-pct=80"],
            "--min-coverage-pct was removed",
        ),
    ] {
        let o = run(&args);
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {err}");
        assert!(err.contains(says), "{args:?}: {err}");
        assert!(o.stdout.is_empty(), "{args:?} printed nothing to stdout");
        assert!(!missing.exists(), "{args:?} opened no store");
    }
    for cmd in ["requirements-target", "seed-domain-validators"] {
        let o = run(&[cmd, "--db", ms]);
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(2), "{cmd}: {err}");
        assert!(err.contains("was removed (core#1924)"), "{cmd}: {err}");
        assert!(!missing.exists(), "{cmd} opened no store");
        assert!(!bus.exists(), "{cmd} started no engine");
    }
    // The version that is built is still accepted by name: an empty model builds (no v3 rows).
    let ok = run(&[
        "domain-graph",
        "--db",
        dbs,
        "--repo-root",
        roots,
        "--schema-version",
        "1.2.0",
    ]);
    assert!(
        ok.status.success(),
        "--schema-version 1.2.0 is not refused: {}",
        String::from_utf8_lossy(&ok.stderr)
    );
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
