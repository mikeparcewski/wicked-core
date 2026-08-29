//! AW-22 / arch-R24 — wiki-lifecycle events on the bus.
//!
//! Asserts (1) the event names conform to the 4-segment `wicked.<domain>.<noun>.<verb>` grammar
//! (`wicked-bus/reqs/SPEC.md` §5, in-process validator `wicked_apps_core::validate_event_type`);
//! (2) the payloads are COARSE and complete (ids/classifications, never the rule statement);
//! (3) corpus changes actually EMIT through the shared outbox/emit seam — `register_rule` fires
//! `wicked.estate.rule.ingested`, `retire_rule` fires `wicked.estate.rule.retired` only on an
//! actual state change, and the exported `emit_doc_drifted` seam fires
//! `wicked.estate.doc.drifted` — asserted against the NDJSON outbox spool, no live bus needed.
//!
//! Falsifiers: a renamed/mis-segmented event type fails the grammar test; a payload that leaks
//! `statement` text fails the coarseness assertion; an emission that never happens leaves the
//! spool empty and fails the seam test.

use wicked_apps_core::validate_event_type;
use wicked_governance::{
    doc_drifted_event, rule_ingested_event, rule_retired_event, ConfSeverity, ConformanceRule,
    DocDrift, RuleProvenance, RuleType, Targets, EV_DOC_DRIFTED, EV_RULE_INGESTED, EV_RULE_RETIRED,
    WIKI_LIFECYCLE_EVENTS,
};

/// The statement text used everywhere below — the string that must NEVER appear in a payload.
const STATEMENT: &str = "Never use printf without %s (coarse-payload sentinel)";

fn sample_rule(id: &str) -> ConformanceRule {
    ConformanceRule {
        id: id.to_string(),
        rule_type: RuleType::Pattern,
        statement: STATEMENT.to_string(),
        severity: ConfSeverity::Error,
        // Exactly representable in binary so the wire number round-trips bit-for-bit.
        confidence: 0.5,
        targets: Targets::default(),
        symbol_ref: None,
        compliance: None,
        provenance: RuleProvenance {
            source: "markdown".to_string(),
            reference: Some("docs/agent-behavior.md#PAT-001".to_string()),
            source_kinds: vec!["doc".to_string()],
        },
        retired: false,
    }
}

/// Every wiki-lifecycle event name passes the ecosystem grammar: exactly four dot-separated
/// segments, `estate` domain, `[a-z0-9_]` alphabet — the validator mirrors the bus SPEC's
/// `wicked.<domain>.<noun>.<verb>` naming convention.
#[test]
fn event_types_conform_to_the_bus_grammar() {
    assert_eq!(
        WIKI_LIFECYCLE_EVENTS,
        [
            "wicked.estate.rule.ingested",
            "wicked.estate.rule.retired",
            "wicked.estate.doc.drifted",
        ],
        "the catalog is the arch-R24 roster, verbatim"
    );
    for ev in WIKI_LIFECYCLE_EVENTS {
        assert!(
            validate_event_type(ev),
            "{ev} must pass the 4-segment wicked.<domain>.<noun>.<verb> grammar"
        );
        let segments: Vec<&str> = ev.split('.').collect();
        assert_eq!(segments.len(), 4, "{ev} must have exactly four segments");
        assert_eq!(segments[0], "wicked");
        assert_eq!(
            segments[1], "estate",
            "{ev}: the corpus's system of record (the estate store) is the domain segment"
        );
        assert!(
            segments[3].ends_with("ed"),
            "{ev}: the verb is past tense per the SPEC convention"
        );
    }
}

/// `rule.ingested` payload: identity + classification + provenance, coarse — never the statement.
#[test]
fn rule_ingested_event_payload_is_coarse_and_complete() {
    let rule = sample_rule("PAT-001");
    let ev = rule_ingested_event(&rule);

    assert_eq!(ev.event_type, EV_RULE_INGESTED);
    assert_eq!(ev.domain, "wicked-governance");
    assert_eq!(ev.subdomain, "governance.corpus");

    let p = &ev.payload;
    assert_eq!(p["rule_id"], "PAT-001");
    assert_eq!(
        p["rule_type"], "pattern",
        "wire spelling via serde, not a hand-rolled string"
    );
    assert_eq!(p["severity"], "error");
    assert_eq!(p["retired"], false);
    assert_eq!(p["source"], "markdown");
    assert_eq!(p["ref"], "docs/agent-behavior.md#PAT-001");
    assert_eq!(p["confidence"], 0.5);

    let raw = serde_json::to_string(p).unwrap();
    assert!(
        !raw.contains("printf") && p.get("statement").is_none(),
        "coarse contract: the payload must never carry the rule statement, got {raw}"
    );
}

/// `rule.retired` payload: identity + classification + provenance — coarse, same contract.
#[test]
fn rule_retired_event_payload_is_coarse_and_complete() {
    let mut rule = sample_rule("POL-002");
    rule.rule_type = RuleType::Policy;
    rule.severity = ConfSeverity::Critical;
    rule.retired = true;
    let ev = rule_retired_event(&rule);

    assert_eq!(ev.event_type, EV_RULE_RETIRED);
    assert_eq!(ev.domain, "wicked-governance");
    assert_eq!(ev.subdomain, "governance.corpus");

    let p = &ev.payload;
    assert_eq!(p["rule_id"], "POL-002");
    assert_eq!(p["rule_type"], "policy");
    assert_eq!(p["severity"], "critical");
    assert_eq!(p["source"], "markdown");
    assert_eq!(p["ref"], "docs/agent-behavior.md#PAT-001");
    assert!(
        p.get("statement").is_none(),
        "coarse contract: no statement text on the retirement event"
    );
}

/// `doc.drifted` payload: the doc + the rules minted from it (AW-24's propagation targets);
/// an unknown frontmatter id rides as `null`, and `rule_count` always agrees with `rule_ids`.
#[test]
fn doc_drifted_event_payload_names_the_doc_and_its_rules() {
    let drift = DocDrift {
        doc_path: "docs/agent-behavior.md".to_string(),
        doc_id: Some("agent-behavior".to_string()),
        reason: "doc changed since last ingest".to_string(),
        rule_ids: vec!["PAT-001".to_string(), "POL-002".to_string()],
    };
    let ev = doc_drifted_event(&drift);

    assert_eq!(ev.event_type, EV_DOC_DRIFTED);
    assert_eq!(ev.domain, "wicked-governance");
    assert_eq!(ev.subdomain, "governance.corpus");

    let p = &ev.payload;
    assert_eq!(p["doc_path"], "docs/agent-behavior.md");
    assert_eq!(p["doc_id"], "agent-behavior");
    assert_eq!(p["reason"], "doc changed since last ingest");
    assert_eq!(p["rule_ids"], serde_json::json!(["PAT-001", "POL-002"]));
    assert_eq!(p["rule_count"], 2);

    // Deletion-shaped drift with no surviving frontmatter id: doc_id is explicit null, never absent.
    let deleted = DocDrift {
        doc_path: "docs/deleted.md".to_string(),
        doc_id: None,
        reason: "doc deleted".to_string(),
        rule_ids: vec![],
    };
    let ev = doc_drifted_event(&deleted);
    assert!(ev.payload["doc_id"].is_null());
    assert_eq!(ev.payload["rule_count"], 0);
}

/// Corpus changes are OBSERVABLE as events through the shared emit seam: with no shared store
/// configured, every emission lands as one NDJSON record on the outbox spool. Register fires
/// `rule.ingested`; retire fires `rule.retired` exactly once (an already-retired rule and a
/// missing rule fire nothing — no state change, no event); the drift seam fires `doc.drifted`.
///
/// This is the ONLY test in this binary that touches process env (the other tests are pure
/// builders), so the env mutation cannot race a parallel test.
#[test]
fn register_retire_and_drift_emit_through_the_outbox_seam() {
    use wicked_apps_core::emit::DEADLETTER_ENV;
    use wicked_apps_core::{SqliteStore, ESTATE_DB_ENV};

    let dir = std::env::temp_dir().join(format!("wicked-gov-lifecycle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let spool = dir.join("emit-outbox.ndjson");
    let _ = std::fs::remove_file(&spool);

    // SAFETY: single env-mutating test in this binary (see the doc comment); vars are cleaned
    // up before the assertions run. Same pattern as wicked-apps-core's own emit tests.
    unsafe {
        std::env::set_var(DEADLETTER_ENV, &spool);
        std::env::remove_var(ESTATE_DB_ENV);
    }

    let mut store = SqliteStore::in_memory().expect("open in-memory estate store");
    let rule = sample_rule("PAT-001");

    wicked_governance::register_rule(&mut store, &rule).expect("register");
    assert!(
        wicked_governance::retire_rule(&mut store, "PAT-001").expect("retire"),
        "retire of an existing rule reports true"
    );
    assert!(
        wicked_governance::retire_rule(&mut store, "PAT-001").expect("re-retire"),
        "re-retire still reports success"
    );
    assert!(
        !wicked_governance::retire_rule(&mut store, "PAT-999").expect("retire missing"),
        "missing rule reports false"
    );
    assert!(
        !wicked_governance::emit_doc_drifted(&DocDrift {
            doc_path: "docs/agent-behavior.md".to_string(),
            doc_id: Some("agent-behavior".to_string()),
            reason: "doc deleted".to_string(),
            rule_ids: vec!["PAT-001".to_string()],
        }),
        "no shared store configured => spooled (false), never an error"
    );

    let body = std::fs::read_to_string(&spool).expect("spool file must exist");

    unsafe {
        std::env::remove_var(DEADLETTER_ENV);
    }
    let _ = std::fs::remove_file(&spool);

    let records: Vec<serde_json::Value> = body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each spool line is valid JSON"))
        .collect();
    let of_type = |t: &str| -> Vec<&serde_json::Value> {
        records.iter().filter(|r| r["type"] == t).collect()
    };

    let ingested = of_type(EV_RULE_INGESTED);
    assert_eq!(
        ingested.len(),
        1,
        "one register => one rule.ingested, got {records:?}"
    );
    assert_eq!(ingested[0]["payload"]["rule_id"], "PAT-001");
    assert_eq!(ingested[0]["payload"]["retired"], false);
    assert_eq!(ingested[0]["domain"], "wicked-governance");
    assert_eq!(ingested[0]["subdomain"], "governance.corpus");

    let retired = of_type(EV_RULE_RETIRED);
    assert_eq!(
        retired.len(),
        1,
        "exactly ONE rule.retired: the re-retire and the missing id are state no-ops, got {records:?}"
    );
    assert_eq!(retired[0]["payload"]["rule_id"], "PAT-001");
    assert_eq!(retired[0]["payload"]["severity"], "error");

    let drifted = of_type(EV_DOC_DRIFTED);
    assert_eq!(
        drifted.len(),
        1,
        "one drift report => one doc.drifted, got {records:?}"
    );
    assert_eq!(drifted[0]["payload"]["doc_path"], "docs/agent-behavior.md");
    assert_eq!(drifted[0]["payload"]["reason"], "doc deleted");
    assert_eq!(
        drifted[0]["payload"]["rule_ids"],
        serde_json::json!(["PAT-001"])
    );

    assert_eq!(
        records.len(),
        3,
        "no other emissions on these paths: {records:?}"
    );
}
