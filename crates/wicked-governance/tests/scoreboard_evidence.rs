//! AW-23 — the `evidence_count` increment, proven on a PERSISTED on-disk store.
//!
//! The unit tests prove the aggregate through [`wicked_governance::scoreboard`]; this test closes
//! the remaining seam: the bump must survive the store's edge-upsert MERGE RULE (an incoming row
//! wins only on higher confidence OR a GROWN `evidence_count` — estate-store 0.14.5,
//! `sqlite.rs:1566-1568`) and the promoted-column round-trip across a real close-and-reopen.
//! `:memory:` cannot prove the reopen half.
//!
//! Falsifiers: a merge rule that keeps the existing row at equal confidence pins the raw edge at
//! 0/1 and the `2` assertions fail; a promoted column that doesn't round-trip through `data` fails
//! after the reopen; a non-idempotent `record_rule_evidence` makes the replay assertion overshoot.
//!
//! This is the ONLY test in this binary (it mutates process env for the emit spool — see
//! `tests/lifecycle_events.rs` for the same pattern).

use wicked_apps_core::{
    open_store, synthetic_symbol, ConformanceClaim, Decision, EdgeKind, GraphWrite, Language,
    Location, Node, NodeKind, Span, Symbol,
};
use wicked_estate_core::Direction;
use wicked_governance::{
    claim_symbol, conform, register_rule, relink, scoreboard, ConfSeverity, ConformanceRule,
    RuleProvenance, RuleType, Targets, CONFORMANCE_RULE, DEFAULT_AMBIGUITY_CAP, EVIDENCED_BY,
};

fn deny_claim(id: &str) -> ConformanceClaim {
    ConformanceClaim {
        claim_id: id.to_string(),
        scope: "repo:test".into(),
        phase: "build".into(),
        policy_ids: vec![],
        decision: Decision::Deny,
        obligations: vec!["conform:Critical:POL-100:writes go through the single writer".into()],
        evaluated_context_ref: "sha256:test".into(),
        criteria: String::new(),
        evaluator_identity: "wicked-governance@test".into(),
        evaluated_at: 1_750_000_000,
    }
}

/// The rule's ONE derived `Governs` edge, read raw from the store.
fn governs_evidence_count(store: &impl wicked_apps_core::GraphRead) -> u32 {
    let governs: Vec<_> = store
        .neighbors(
            &synthetic_symbol(CONFORMANCE_RULE, "POL-100"),
            Direction::Dependencies,
        )
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Governs)
        .collect();
    assert_eq!(governs.len(), 1, "exactly one relink-derived Governs edge");
    governs[0].evidence_count
}

#[test]
fn evidence_count_increments_persist_through_the_merge_rule_and_a_reopen() {
    let dir = std::env::temp_dir().join(format!("wicked-gov-sb-evidence-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Fire-and-forget emissions from `conform` go to a spool in the SAME temp dir, never the
    // operator's real replay queue. Sole test in this binary, so the env write cannot race.
    // SAFETY: single-threaded at this point (first statement of the only test).
    unsafe {
        std::env::set_var(
            wicked_apps_core::emit::DEADLETTER_ENV,
            dir.join("emit-outbox.ndjson"),
        )
    };
    let db = dir.join("store.db");
    let db_path = db.to_str().unwrap();

    // A real indexed code symbol + a rule whose symbol_ref names it; relink derives the
    // rule → code Governs edge with `evidence_count` 0 (`ConformanceRule::governs_edge`).
    let mut store = open_store(Some(db_path)).unwrap();
    let node = Node::new(
        Symbol::synthetic("scip-sim", "src/writer.rs::commit::v1").id(),
        NodeKind::Function,
        "commit".to_string(),
        Language::new("rust"),
        Location::new("src/writer.rs".to_string(), Span::ZERO),
    );
    store.begin_batch().unwrap();
    store.upsert_nodes(&[node]).unwrap();
    store.commit_batch().unwrap();
    register_rule(
        &mut store,
        &ConformanceRule {
            id: "POL-100".into(),
            rule_type: RuleType::Policy,
            statement: "writes go through the single writer".into(),
            severity: ConfSeverity::Critical,
            confidence: 0.9,
            targets: Targets::default(),
            symbol_ref: Some("src/writer.rs::commit".into()),
            compliance: None,
            provenance: RuleProvenance {
                source: "markdown".into(),
                reference: Some("doctrine.md#POL-100".into()),
                source_kinds: vec!["doc".into()],
            },
            retired: false,
        },
    )
    .unwrap();
    relink(&mut store, DEFAULT_AMBIGUITY_CAP, 1_000).unwrap();
    assert_eq!(
        governs_evidence_count(&store),
        0,
        "initialized 0, pre-AW-23 state"
    );

    // Denial #1: 0 → 1. Denial #1 REPLAYED: still 1 (content-addressed claim ⇒ idempotent).
    // Denial #2: 1 → 2 — the growth clause at EQUAL confidence, where a
    // higher-confidence-only merge rule would silently drop the bump.
    conform(&mut store, &deny_claim("claim-1")).unwrap();
    assert_eq!(governs_evidence_count(&store), 1);
    conform(&mut store, &deny_claim("claim-1")).unwrap();
    assert_eq!(
        governs_evidence_count(&store),
        1,
        "replay must not double-count"
    );
    conform(&mut store, &deny_claim("claim-2")).unwrap();
    assert_eq!(governs_evidence_count(&store), 2);
    drop(store);

    // Reopen from disk: the counter and the claim → rule evidence edges PERSISTED.
    let reopened = open_store(Some(db_path)).unwrap();
    assert_eq!(
        governs_evidence_count(&reopened),
        2,
        "survives a close-and-reopen"
    );
    for claim in ["claim-1", "claim-2"] {
        let cites: Vec<_> = wicked_apps_core::GraphRead::neighbors(
            &reopened,
            &claim_symbol(claim),
            Direction::Dependencies,
        )
        .unwrap()
        .into_iter()
        .filter(|e| matches!(&e.kind, EdgeKind::Other(k) if k == EVIDENCED_BY))
        .collect();
        assert_eq!(
            cites.len(),
            1,
            "{claim} carries exactly one evidenced_by edge"
        );
        assert_eq!(
            cites[0].target,
            synthetic_symbol(CONFORMANCE_RULE, "POL-100")
        );
    }

    // And the scoreboard read over the same reopened store agrees with the raw edges.
    let report = scoreboard(&reopened, None, DEFAULT_AMBIGUITY_CAP).unwrap();
    assert_eq!(report.evidence.denial_claims, 2);
    assert_eq!(report.evidence.evidenced_by_edges, 2);
    assert_eq!(report.evidence.governs_evidence_total, 2);

    drop(reopened);
    let _ = std::fs::remove_dir_all(&dir);
}
