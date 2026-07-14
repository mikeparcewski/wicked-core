//! core#25 wire-fidelity proof: the store-bound `recompute_front_half_coverage` OUTPUT validates against
//! the KEPT `coverage.schema.json` (VERSION 1.0.0) — the same meta-fix discipline as
//! `domain_model_schema.rs`. A real JSON-Schema validator catches every type / `additionalProperties` /
//! required-field / range drift automatically, so the emitted `coverage-report.json` can never diverge
//! from the contract the grep validator + garden consume.
//!
//! The schema is VENDORED (`tests/coverage.schema.json`, a byte copy of `wicked-brain/schemas/`) and
//! kept in sync by `tests/schema_vendor_pin.rs`.

use wicked_apps_core::{
    synthetic_symbol, GraphWrite, Language, Location, Node, NodeKind, Span, SqliteStore,
    SYMBOL_SCHEME,
};
use wicked_estate_core::Annotation;
use wicked_governance::recompute_front_half_coverage;

const SCHEMA: &str = include_str!("coverage.schema.json");

fn node(store: &mut SqliteStore, name: &str, kind: NodeKind, file: &str) -> Node {
    let n = Node::new(
        synthetic_symbol("code", name),
        kind,
        name.to_string(),
        Language::new(SYMBOL_SCHEME),
        Location::new(file.to_string(), Span::ZERO),
    );
    store.begin_batch().unwrap();
    store.upsert_nodes(std::slice::from_ref(&n)).unwrap();
    store.commit_batch().unwrap();
    n
}

fn assert_valid(report: &wicked_governance::CoverageReport) {
    let instance = serde_json::to_value(report).unwrap();
    let schema: serde_json::Value = serde_json::from_str(SCHEMA).expect("schema parses");
    let compiled = jsonschema::JSONSchema::compile(&schema).expect("schema compiles");
    let violations: Vec<String> = match compiled.validate(&instance) {
        Ok(()) => Vec::new(),
        Err(errors) => errors
            .map(|e| format!("{} @ {}", e, e.instance_path))
            .collect(),
    };
    assert!(
        violations.is_empty(),
        "coverage report must validate against coverage.schema.json; violations:\n{}\n\ninstance:\n{}",
        violations.join("\n"),
        serde_json::to_string_pretty(&instance).unwrap()
    );
}

#[test]
fn recomputed_coverage_validates_against_the_kept_schema() {
    // Exercise every wire branch: resolved (validated req + rule), risk (below-threshold rule + risk ann),
    // unaccounted (bare behavior node), an estate-behavior Other tag, a structural node (excluded), and
    // two package-dir groups so per_app is a non-trivial array.
    let mut s = SqliteStore::in_memory().unwrap();
    let charge = node(&mut s, "charge", NodeKind::Function, "billing/charge.rs");
    s.set_node_semantics(&charge.symbol, None, Some("REQ-1"), Some(true))
        .unwrap();
    s.annotate(
        &charge.symbol,
        Annotation::new("business_rule", "r", "amount > 0").with_confidence(0.9),
    )
    .unwrap();
    let refund = node(&mut s, "refund", NodeKind::Function, "billing/refund.rs");
    s.annotate(
        &refund.symbol,
        Annotation::new("risk", "r", "partial refunds unclear").with_confidence(0.4),
    )
    .unwrap();
    let bare = node(&mut s, "helper", NodeKind::Function, "auth/util.rs"); // UNACCOUNTED hole
    let _ = bare;
    let pgm = node(
        &mut s,
        "PGM",
        NodeKind::Other("cics_program".into()),
        "mf/a.cbl",
    );
    s.set_node_semantics(&pgm.symbol, None, Some("REQ-2"), Some(true))
        .unwrap();
    node(&mut s, "field", NodeKind::Field, "billing/x.rs"); // structural — excluded

    let report = recompute_front_half_coverage(&s).unwrap();
    // A real hole → coverage < 1.0 (the gate would DENY), and the report is still schema-valid.
    assert!(
        report.coverage < 1.0,
        "the bare helper is a hole: {report:?}"
    );
    assert_eq!(
        report.behavior_bearing, 4,
        "charge+refund+helper+PGM (Field excluded)"
    );
    assert_eq!(report.unaccounted, 1);
    assert_valid(&report);
}

#[test]
fn empty_and_full_reports_are_schema_valid() {
    // Vacuous 1.0 (no behavior nodes) — schema-valid.
    let mut s = SqliteStore::in_memory().unwrap();
    node(&mut s, "f", NodeKind::Field, "a.rs");
    let empty = recompute_front_half_coverage(&s).unwrap();
    assert_eq!(empty.coverage, 1.0);
    assert_valid(&empty);

    // Full 1.0 (every behavior node accounted) — schema-valid, empty unaccounted_nodes.
    let mut s2 = SqliteStore::in_memory().unwrap();
    let a = node(&mut s2, "A", NodeKind::Function, "a.rs");
    s2.set_node_semantics(&a.symbol, None, Some("REQ"), Some(true))
        .unwrap();
    let full = recompute_front_half_coverage(&s2).unwrap();
    assert_eq!(full.coverage, 1.0);
    assert!(full.unaccounted_nodes.is_empty());
    assert_valid(&full);
}
