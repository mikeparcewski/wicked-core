//! wicked-governance — semantic governance on the shared wicked-estate store.
//!
//! The deterministic governance loop (ARCHITECTURE §2), ported from the Node prototype
//! (`lib/{select,decide,store,evidence-port}.mjs`) onto the verified `wicked-apps-core` estate API:
//!
//! ```text
//!   register_policy ─► [Node Other(POLICY)]            (shared estate store)
//!   SELECT ─► by applies_to(phase)  ─► DECIDE ─► ConformanceClaim ─► conform ─► [Node + Governs edge]
//!                                       (deny dominates; NO model)            (+ coarse bus event)
//! ```
//!
//! ## Estate mapping
//! - A [`Policy`] persists as `Node(kind = NodeKind::Other(`[`POLICY`](wicked_apps_core::POLICY)`))`, keyed
//!   by `Symbol::synthetic("wicked-apps", "policy/<id>")`, every field encoded in `Node.metadata`.
//! - A recorded [`wicked_apps_core::ConformanceClaim`] persists as `Node(Other(`[`CONFORMANCE_CLAIM`](wicked_apps_core::CONFORMANCE_CLAIM)`))`
//!   plus one `policy → claim` [`wicked_apps_core::EdgeKind::Governs`] edge per participating policy.
//! - The claim node IS the evidence on the shared graph (the prototype's wicked-vault EvidencePort
//!   is out of scope for this crate; `conform` records the durable node + a coarse fire-and-forget
//!   event, never the evaluated context payload).
//!
//! ## Determinism
//! DECIDE makes NO model call: triggered [`Effect::Deny`] ⇒ [`wicked_apps_core::Decision::Deny`] (deny
//! DOMINATES); triggered [`Effect::AllowWithConditions`] ⇒ union obligations; else
//! [`wicked_apps_core::Decision::Allow`]. Same context ⇒ same claim (re-derivable, attestable — ADR-0003).

mod conformance;
mod domain;
mod engine;
mod ingest;

pub use domain::{Effect, Policy, Severity, Trigger};
pub use engine::{
    claim_from_node, claim_symbol, claim_to_node, conform, decide, decide_as, register_policy,
    select, EVALUATOR_IDENTITY, EV_CONFORMANCE_RECORDED_LITERAL,
};

// Conformance rules — prescriptive pattern/policy rules on native estate `Rule` nodes (PR-B).
pub use conformance::{
    recall_rules, register_rule, Compliance, ConfSeverity, ConformanceRule, RuleProvenance,
    RuleQuery, RuleType, Targets,
};
pub use ingest::{
    ingest_from, normalize_bundle, ComplianceFramework, FilesystemAdapter, FrameworkRegistry,
    NoopFramework, SourceAdapter, StubAdapter,
};

// Re-export the claim wire type so callers program against one path.
pub use wicked_apps_core::{ConformanceClaim, Decision};

/// Crate identity smoke.
pub fn health() -> &'static str {
    "wicked-governance"
}

#[cfg(test)]
mod tests {
    use super::*;
    use wicked_apps_core::{EdgeKind, FromNode, GraphRead, NodeKind, SqliteStore, ToNode};

    fn deny_policy() -> Policy {
        Policy {
            id: "pol-deny-secrets".to_string(),
            kind: "security".to_string(),
            applies_to: vec!["build".to_string(), "deploy".to_string()],
            effect: Effect::Deny,
            trigger: Trigger {
                contains: Some("AKIA[0-9A-Z]{16}".to_string()),
            },
            obligations: vec![],
            criteria: "no aws access keys in the context".to_string(),
            severity: Severity::High,
            rule: "Deny any plan that embeds an AWS access key id.".to_string(),
        }
    }

    fn allow_with_conditions_policy() -> Policy {
        Policy {
            id: "pol-allow-notify".to_string(),
            kind: "ops".to_string(),
            applies_to: vec!["build".to_string()],
            effect: Effect::AllowWithConditions,
            trigger: Trigger {
                // Fires on any context (no contains predicate).
                contains: None,
            },
            obligations: vec!["notify-secops".to_string(), "tag-release".to_string()],
            criteria: "secops must be notified".to_string(),
            severity: Severity::Medium,
            rule: "Builds are allowed but secops must be notified.".to_string(),
        }
    }

    /// Policy → Node → write → get_node → from_node round-trips to an equal Policy.
    #[test]
    fn policy_node_round_trips_through_in_memory_store() {
        let original = deny_policy();
        let node = original.to_node();
        let symbol = node.symbol.clone();

        let mut store = SqliteStore::in_memory().expect("open in-memory store");
        register_policy(&mut store, &original).expect("register policy");

        let fetched = store
            .get_node(&symbol)
            .expect("get_node ok")
            .expect("policy node present after register");
        // The persisted node carries the POLICY kind.
        assert_eq!(
            fetched.kind,
            NodeKind::Other(wicked_apps_core::POLICY.to_string())
        );

        let recovered = Policy::from_node(&fetched).expect("from_node ok");
        assert_eq!(
            original, recovered,
            "Policy must survive the Node round-trip through SqliteStore"
        );
        // The synthetic symbol is the deterministic id we constructed.
        assert_eq!(
            symbol,
            wicked_apps_core::synthetic_symbol(wicked_apps_core::POLICY, "pol-deny-secrets")
        );
    }

    /// deny-dominates: a triggered deny + a competing triggered allow_with_conditions ⇒ Deny.
    #[test]
    fn deny_dominates_over_allow_with_conditions() {
        let selected = vec![allow_with_conditions_policy(), deny_policy()];
        // Context that trips the deny trigger (contains an AKIA key) AND would otherwise collect
        // the allow_with_conditions obligations.
        let context = serde_json::json!({
            "phase": "build",
            "plan": "export AWS_KEY=AKIAIOSFODNN7EXAMPLE then ship",
        });

        let claim = decide(&selected, "repo:acme", "build", &context, 1_750_000_000);

        assert_eq!(claim.decision, Decision::Deny, "deny must dominate");
        // When denied, obligations are NOT collected (prototype semantics).
        assert!(
            claim.obligations.is_empty(),
            "a denied decision carries no obligations"
        );
        // Both fired policies participated; ordered by precedence (high deny first, then medium).
        assert_eq!(
            claim.policy_ids,
            vec![
                "pol-deny-secrets".to_string(),
                "pol-allow-notify".to_string()
            ]
        );
        assert_eq!(claim.evaluator_identity, EVALUATOR_IDENTITY);
        assert!(claim.evaluated_context_ref.starts_with("sha256:"));
    }

    /// SELECT returns a policy when its applies_to includes the phase, and nothing when it doesn't.
    #[test]
    fn select_matches_on_applies_to_phase() {
        let mut store = SqliteStore::in_memory().expect("open in-memory store");
        // pol-allow-notify applies_to == ["build"] only.
        register_policy(&mut store, &allow_with_conditions_policy()).expect("register");

        let ctx = serde_json::json!({});

        let hit = select(&store, "repo:acme", "build", &ctx).expect("select build");
        assert_eq!(hit.len(), 1, "phase match must return the policy");
        assert_eq!(hit[0].id, "pol-allow-notify");

        let miss = select(&store, "repo:acme", "release", &ctx).expect("select release");
        assert!(miss.is_empty(), "phase mismatch must return no policies");
    }

    /// conform: after recording, the claim node is retrievable and carries the decision, and a
    /// Governs edge links policy → claim.
    #[test]
    fn conform_persists_claim_node_and_governs_edge() {
        // Keep emit hermetic: redirect the dead-letter spool to a temp file so conform's
        // fire-and-forget emit (which falls back to the spool when ESTATE_DB_ENV is unset) does
        // not write under HOME during the test.
        let spool =
            std::env::temp_dir().join(format!("wg-conform-emit-{}.ndjson", std::process::id()));
        unsafe {
            std::env::set_var(wicked_apps_core::emit::DEADLETTER_ENV, &spool);
        }

        let mut store = SqliteStore::in_memory().expect("open in-memory store");
        let policy = deny_policy();
        register_policy(&mut store, &policy).expect("register");

        let context = serde_json::json!({ "phase": "build", "plan": "AKIAIOSFODNN7EXAMPLE" });
        let selected = select(&store, "repo:acme", "build", &context).expect("select");
        let claim = decide(&selected, "repo:acme", "build", &context, 1_750_000_000);
        assert_eq!(claim.decision, Decision::Deny);

        conform(&mut store, &claim).expect("conform");

        // The claim node is retrievable and carries the decision.
        let claim_node = store
            .get_node(&claim_symbol(&claim.claim_id))
            .expect("get_node ok")
            .expect("claim node present after conform");
        assert_eq!(
            claim_node.kind,
            NodeKind::Other(wicked_apps_core::CONFORMANCE_CLAIM.to_string())
        );
        let recovered = claim_from_node(&claim_node).expect("claim_from_node ok");
        assert_eq!(recovered.decision, Decision::Deny);
        assert_eq!(recovered, claim, "the recorded claim round-trips");

        // A Governs edge links policy → claim (source = policy, target = claim).
        let policy_sym = wicked_apps_core::synthetic_symbol(wicked_apps_core::POLICY, &policy.id);
        let edges = store
            .neighbors(&policy_sym, wicked_estate_core::Direction::Dependencies)
            .expect("neighbors ok");
        let governs: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Governs && e.target == claim_node.symbol)
            .collect();
        assert_eq!(
            governs.len(),
            1,
            "exactly one policy→claim Governs edge must exist"
        );

        unsafe {
            std::env::remove_var(wicked_apps_core::emit::DEADLETTER_ENV);
        }
        let _ = std::fs::remove_file(&spool);
    }

    /// NEGATIVE: a clean context that trips no deny and no obligation ⇒ Allow.
    #[test]
    fn clean_context_yields_allow() {
        // Only the deny policy is in scope; its trigger does NOT fire on a clean context.
        let selected = vec![deny_policy()];
        let context = serde_json::json!({ "phase": "build", "plan": "build the docs site" });

        let claim = decide(&selected, "repo:acme", "build", &context, 1_750_000_000);

        assert_eq!(claim.decision, Decision::Allow, "clean context ⇒ allow");
        assert!(claim.obligations.is_empty());
        assert!(
            claim.policy_ids.is_empty(),
            "no policy fired, so none participated"
        );
    }

    /// decide_as stamps the custom evaluator identity onto the claim.
    #[test]
    fn decide_as_stamps_custom_evaluator_identity() {
        let selected = vec![deny_policy()];
        let context = serde_json::json!({ "phase": "build", "plan": "build the docs site" });
        let custom_identity = "wicked-evaluator:agy";

        let claim = decide_as(
            &selected,
            "repo:acme",
            "build",
            &context,
            1_750_000_000,
            custom_identity,
        );

        assert_eq!(
            claim.evaluator_identity, custom_identity,
            "decide_as must stamp the supplied evaluator identity"
        );
        assert_ne!(
            claim.evaluator_identity, EVALUATOR_IDENTITY,
            "custom identity must differ from the canonical EVALUATOR_IDENTITY"
        );
    }

    /// decide and decide_as on identical inputs but different evaluator_identity produce different
    /// claim_ids and can both be stored without one overwriting the other.
    #[test]
    fn decide_and_decide_as_produce_different_claim_ids_for_same_context() {
        let spool =
            std::env::temp_dir().join(format!("wg-decide-as-emit-{}.ndjson", std::process::id()));
        unsafe {
            std::env::set_var(wicked_apps_core::emit::DEADLETTER_ENV, &spool);
        }

        let selected = vec![deny_policy()];
        let context = serde_json::json!({ "phase": "build", "plan": "build the docs site" });

        let claim_default = decide(&selected, "repo:acme", "build", &context, 1_750_000_000);
        let claim_custom = decide_as(
            &selected,
            "repo:acme",
            "build",
            &context,
            1_750_000_000,
            "wicked-evaluator:agy",
        );

        // The evaluator_identity is in the seed — different identities must produce different ids.
        assert_ne!(
            claim_default.claim_id, claim_custom.claim_id,
            "different evaluator identities must produce different claim_ids"
        );

        // Both claims can be stored without collision.
        let mut store = SqliteStore::in_memory().expect("open in-memory store");
        conform(&mut store, &claim_default).expect("conform default");
        conform(&mut store, &claim_custom).expect("conform custom");

        // Read both back — they must exist independently.
        let node_default = store
            .get_node(&claim_symbol(&claim_default.claim_id))
            .expect("get_node default ok")
            .expect("default claim node must be present");
        let node_custom = store
            .get_node(&claim_symbol(&claim_custom.claim_id))
            .expect("get_node custom ok")
            .expect("custom claim node must be present");

        let recovered_default = claim_from_node(&node_default).expect("claim_from_node default");
        let recovered_custom = claim_from_node(&node_custom).expect("claim_from_node custom");

        assert_eq!(recovered_default.evaluator_identity, EVALUATOR_IDENTITY);
        assert_eq!(recovered_custom.evaluator_identity, "wicked-evaluator:agy");

        unsafe {
            std::env::remove_var(wicked_apps_core::emit::DEADLETTER_ENV);
        }
        let _ = std::fs::remove_file(&spool);
    }

    /// Adversarial: creator (decide → Allow) and evaluator (decide_as → Deny) on the same context
    /// both persist, with different claim_ids and their respective decisions intact.
    #[test]
    fn evaluator_deny_persists_alongside_creator_allow() {
        let spool = std::env::temp_dir().join(format!(
            "wg-evaluator-deny-emit-{}.ndjson",
            std::process::id()
        ));
        unsafe {
            std::env::set_var(wicked_apps_core::emit::DEADLETTER_ENV, &spool);
        }

        // Creator sees a clean context (deny_policy trigger does not fire) → Allow.
        let creator_selected = vec![deny_policy()];
        let clean_context = serde_json::json!({ "phase": "build", "plan": "build the docs site" });
        let creator_claim = decide(
            &creator_selected,
            "repo:acme",
            "build",
            &clean_context,
            1_750_000_000,
        );
        assert_eq!(
            creator_claim.decision,
            Decision::Allow,
            "creator must see Allow on clean context"
        );

        // Evaluator uses a deny-triggering context with a different identity → Deny.
        let evaluator_selected = vec![deny_policy()];
        let deny_context = serde_json::json!({
            "phase": "build",
            "plan": "AKIAIOSFODNN7EXAMPLE leaked key",
        });
        let evaluator_claim = decide_as(
            &evaluator_selected,
            "repo:acme",
            "build",
            &deny_context,
            1_750_000_000,
            "wicked-evaluator:security",
        );
        assert_eq!(
            evaluator_claim.decision,
            Decision::Deny,
            "evaluator must see Deny on key-containing context"
        );

        // Both claim_ids are different (different context AND different identity).
        assert_ne!(
            creator_claim.claim_id, evaluator_claim.claim_id,
            "creator and evaluator claim_ids must differ"
        );

        // Persist both.
        let mut store = SqliteStore::in_memory().expect("open in-memory store");
        conform(&mut store, &creator_claim).expect("conform creator");
        conform(&mut store, &evaluator_claim).expect("conform evaluator");

        // Both nodes must exist independently.
        let creator_node = store
            .get_node(&claim_symbol(&creator_claim.claim_id))
            .expect("get_node creator ok")
            .expect("creator claim node must be present");
        let evaluator_node = store
            .get_node(&claim_symbol(&evaluator_claim.claim_id))
            .expect("get_node evaluator ok")
            .expect("evaluator claim node must be present");

        let recovered_creator = claim_from_node(&creator_node).expect("claim_from_node creator");
        let recovered_evaluator =
            claim_from_node(&evaluator_node).expect("claim_from_node evaluator");

        assert_eq!(
            recovered_creator.decision,
            Decision::Allow,
            "creator node must carry Allow"
        );
        assert_eq!(
            recovered_evaluator.decision,
            Decision::Deny,
            "evaluator node must carry Deny"
        );
        assert_eq!(recovered_creator.evaluator_identity, EVALUATOR_IDENTITY);
        assert_eq!(
            recovered_evaluator.evaluator_identity,
            "wicked-evaluator:security"
        );

        unsafe {
            std::env::remove_var(wicked_apps_core::emit::DEADLETTER_ENV);
        }
        let _ = std::fs::remove_file(&spool);
    }
}
