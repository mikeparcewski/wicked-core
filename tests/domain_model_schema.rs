//! PR-D wire-fidelity proof: the domain-graph builder's OUTPUT validates against the KEPT
//! `domain-model.schema.json` (VERSION 1.0.0) — the contract garden STEERS on and wicked-testing
//! ASSERTS. This is the meta-fix for the wire-fidelity class of bug: a self-authored round-trip
//! literal proves only serde symmetry; a real JSON-Schema validator catches every enum / const /
//! pattern (`^RULE-[0-9]{3,6}$`) / `minItems` / type drift automatically, forever.
//!
//! The schema is VENDORED (`tests/domain-model.schema.json`, a byte copy of `wicked-brain/schemas/`)
//! so the test is self-contained + CI-safe. If the canonical schema moves, this copy must be
//! refreshed — a drift-guard belongs with the workflow-retarget follow-on.

use wicked_apps_core::{
    synthetic_symbol, GraphWrite, Language, Location, Node, NodeKind, Span, SqliteStore,
    SYMBOL_SCHEME,
};
use wicked_estate_core::Annotation;
use wicked_governance::{build_domain_model, CoverageReport};

const SCHEMA: &str = include_str!("domain-model.schema.json");

fn node(name: &str, file: &str) -> Node {
    Node::new(
        synthetic_symbol("code", name),
        NodeKind::Function,
        name.to_string(),
        Language::new(SYMBOL_SCHEME),
        Location::new(file.to_string(), Span::ZERO),
    )
}

#[test]
fn built_domain_model_validates_against_the_kept_schema() {
    let mut store = SqliteStore::in_memory().unwrap();
    // A resolved-with-rule node, a rule-less-but-requirement-bearing node (fallback rule path), and a
    // risk-flagged node (kept) — exercising every branch that could emit a schema-invalid shape.
    let charge = node("charge", "billing/charge.py");
    let audit = node("audit", "billing/audit.py");
    let refund = node("refund", "billing/refund.py");
    store.begin_batch().unwrap();
    store
        .upsert_nodes(&[charge.clone(), audit.clone(), refund.clone()])
        .unwrap();
    store.commit_batch().unwrap();
    store
        .set_node_semantics(
            &charge.symbol,
            None,
            Some("process a payment charge"),
            Some(true),
        )
        .unwrap();
    store
        .annotate(
            &charge.symbol,
            Annotation::new("business_rule", "r", "amount must be positive").with_confidence(0.9),
        )
        .unwrap();
    // audit: a requirement semantic but NO business rule → the fallback-rule path (minItems:1).
    store
        .set_node_semantics(
            &audit.symbol,
            None,
            Some("record an audit trail"),
            Some(false),
        )
        .unwrap();
    // refund: risk-flagged only → kept as a review requirement with a synthesized rule.
    store
        .annotate(
            &refund.symbol,
            Annotation::new("risk", "r", "partial refunds unclear — HITL").with_confidence(0.4),
        )
        .unwrap();

    let coverage = CoverageReport {
        coverage: 1.0,
        ..Default::default()
    };
    let model = build_domain_model(&store, &coverage, "1.0.0").expect("build");
    let instance = serde_json::to_value(&model).unwrap();

    let schema: serde_json::Value = serde_json::from_str(SCHEMA).expect("schema parses");
    let compiled = jsonschema::JSONSchema::compile(&schema).expect("schema compiles");
    let pretty = serde_json::to_string_pretty(&instance).unwrap();

    let violations: Vec<String> = match compiled.validate(&instance) {
        Ok(()) => Vec::new(),
        Err(errors) => errors
            .map(|e| format!("{} @ {}", e, e.instance_path))
            .collect(),
    };
    assert!(
        violations.is_empty(),
        "builder output must validate against domain-model.schema.json; violations:\n{}\n\ninstance:\n{}",
        violations.join("\n"),
        pretty
    );
}
