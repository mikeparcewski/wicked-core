//! STEERING unification — the standalone [`Policy`] model merged into the [`ConformanceRule`]
//! steering-rule model (the STEERING program: one steering-rule model behind studio's Steering
//! surface; ALL writes go through crew's API, estate MCP stays read-only).
//!
//! ## What merged where
//!
//! | Policy field | steering rule field | notes |
//! |---|---|---|
//! | `id` | `id` | UNCHANGED — audit resolvability: past decisions cite these ids verbatim |
//! | `kind` | `steering_type` | via [`steering_type_for_policy_kind`] (table below) |
//! | `applies_to` | `applies_to` | identical inclusion semantics in SELECT |
//! | `effect` | `effect: Some(_)` | `Some` marks the rule DECIDE-lane; `None` = recall-only |
//! | `trigger` | `trigger` | `Trigger::default()` (no `contains`) stores as `None` — same firing |
//! | `obligations` | `obligations` | verbatim |
//! | `criteria` | `criteria` | verbatim |
//! | `severity` | `severity` | monotone map (below) so decide precedence order is preserved |
//! | `rule` | `statement` | the human-prose statement IS the rule statement |
//! | `retired` | `retired` | honored verbatim |
//!
//! ## kind → steering_type (documented mapping)
//!
//! A `kind` that already names one of the seven [`STEERING_TYPES`] maps to itself; the legacy
//! enforcement kinds `guardrail` and `gate` map to `operations`; EVERYTHING else defaults to
//! `operations` (migrated policies are enforcement machinery — the operations page owns them
//! until an operator re-files them). The original `kind` string stays resolvable on the retained
//! legacy policy node (retired-not-deleted invariant): migration never destroys the old row.
//!
//! ## severity maps (both directions, order-preserving)
//!
//! `High → Critical`, `Medium → Error`, `Low → Warn` — strictly monotone in rank, so
//! [`crate::engine::decide`]'s precedence (severity desc, id asc) sorts a migrated set exactly as
//! it sorted the originals, which is what makes migrated decisions byte-equal (the golden test
//! below proves it). Inverse: `Critical → High`, `Error → Medium`, `Warn → Low`, and `Info → Low`
//! (a steering rule authored at info that carries an effect decides at the lowest precedence).
//!
//! ## Migration ([`migrate_policies_to_steering`])
//!
//! One-time and idempotent: every legacy `Other(POLICY)` node without a unified twin gets a
//! steering rule written at `conformance_rule/<id>`; rows already unified are skipped; an id that
//! collides with an existing RECALL-ONLY rule is reported as a conflict and NOT overwritten
//! (overwriting would silently destroy a wiki rule). Legacy nodes are LEFT IN PLACE — they are
//! the audit rows past claims resolve against (`policy → claim` Governs edges hang off them).
//! Until a store is migrated, [`crate::engine::select_any`] unions in legacy-only rows at read
//! time, so an un-migrated store never fails open.

use serde::Serialize;
use wicked_apps_core::{
    synthetic_symbol, FromNode, GraphRead, GraphStore, NodeKind, ToNode, POLICY,
};
use wicked_estate_core::SymbolQuery;

use crate::conformance::{
    ConfSeverity, ConformanceRule, RuleProvenance, RuleType, CONFORMANCE_RULE, STEERING_TYPES,
};
use crate::domain::{Policy, Severity, Trigger};

/// `provenance.source` stamped on steering rules minted from the merged Policy model (both the
/// [`crate::register_policy`] shim and the one-time migration). Doc-ingested rules keep their
/// `path@sha#id` refs; UI/chat-authored rules carry `"ui"`/`"chat"` — all first-class.
pub const POLICY_PROVENANCE_SOURCE: &str = "policy";

/// The documented `kind → steering_type` mapping (module docs).
pub fn steering_type_for_policy_kind(kind: &str) -> &'static str {
    let k = kind.trim().to_ascii_lowercase();
    if let Some(t) = STEERING_TYPES.iter().find(|t| **t == k) {
        return t;
    }
    // guardrail/gate → operations, and operations is also the documented default for every other
    // legacy kind (security-flavored kinds like "security" already matched above).
    "operations"
}

/// `Policy.severity → ConfSeverity`, strictly monotone in rank (module docs).
pub fn conf_severity_for_policy(severity: Severity) -> ConfSeverity {
    match severity {
        Severity::High => ConfSeverity::Critical,
        Severity::Medium => ConfSeverity::Error,
        Severity::Low => ConfSeverity::Warn,
    }
}

/// The inverse map (module docs): what a decide-lane steering rule's severity means in the merged
/// Policy model's High/Medium/Low precedence vocabulary.
pub fn policy_severity_for(severity: ConfSeverity) -> Severity {
    match severity {
        ConfSeverity::Critical => Severity::High,
        ConfSeverity::Error => Severity::Medium,
        ConfSeverity::Warn | ConfSeverity::Info => Severity::Low,
    }
}

/// Project a [`Policy`] onto the unified steering-rule model (the write half of the merge).
pub fn steering_rule_from_policy(policy: &Policy) -> ConformanceRule {
    ConformanceRule {
        id: policy.id.clone(),
        rule_type: RuleType::Policy,
        statement: policy.rule.clone(),
        severity: conf_severity_for_policy(policy.severity),
        // A registered policy is authoritative doctrine, not an extracted guess.
        confidence: 1.0,
        provenance: RuleProvenance {
            source: POLICY_PROVENANCE_SOURCE.to_string(),
            // The legacy node's address — where the original row (incl. the raw `kind`) lives.
            reference: Some(format!("{POLICY}/{}", policy.id)),
            source_kinds: vec![],
        },
        retired: policy.retired,
        steering_type: steering_type_for_policy_kind(&policy.kind).to_string(),
        applies_to: policy.applies_to.clone(),
        effect: Some(policy.effect),
        // `Trigger::default()` (no `contains`) fires whenever phase-selected — spelled `None`
        // here so a trigger-less policy round-trips without minting an empty object key.
        trigger: (policy.trigger != Trigger::default()).then(|| policy.trigger.clone()),
        obligations: policy.obligations.clone(),
        criteria: policy.criteria.clone(),
        ..Default::default()
    }
}

/// The DECIDE-lane view of a steering rule: `Some(Policy)` iff the rule carries an effect
/// (recall-only rules never decide). This is what [`crate::engine::select_any`] hands to
/// [`crate::engine::decide`], so decide's engine — and every recorded decision — keeps the merged
/// Policy model's exact semantics. `kind` reads back as the steering_type (the raw legacy kind
/// stays on the retained policy node, module docs).
pub fn policy_view(rule: &ConformanceRule) -> Option<Policy> {
    let effect = rule.effect?;
    Some(Policy {
        id: rule.id.clone(),
        kind: rule.steering_type.clone(),
        applies_to: rule.applies_to.clone(),
        effect,
        trigger: rule.trigger.clone().unwrap_or_default(),
        obligations: rule.obligations.clone(),
        criteria: rule.criteria.clone(),
        severity: policy_severity_for(rule.severity),
        rule: rule.statement.clone(),
        retired: rule.retired,
    })
}

/// What one migration pass did — every legacy policy row is accounted for, nothing silent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PolicyMigration {
    /// Legacy policy ids that got a NEW unified steering-rule row this pass.
    pub migrated: Vec<String>,
    /// Ids whose unified (effect-bearing) row already exists — the idempotent re-run case.
    pub already_unified: Vec<String>,
    /// Ids that collide with an existing RECALL-ONLY rule (or a foreign node) at
    /// `conformance_rule/<id>` — reported, never overwritten.
    pub conflicts: Vec<String>,
    /// Legacy nodes skipped because their metadata id disagrees with the symbol they are filed
    /// under (the misfiled-node inconsistency `retire_policy` refuses to write through), or their
    /// projected steering rule fails validation (e.g. a pre-validation-era empty `applies_to`).
    /// Reported with the reason so an operator can repair the row; never migrated blind.
    pub skipped: Vec<String>,
}

/// One-time, idempotent migration of the legacy `Other(POLICY)` rows into unified steering rules
/// (module docs). Legacy nodes are retained (retired-not-deleted / audit resolvability); re-runs
/// converge (`already_unified`). Writes happen in ONE batch after the scan; emits NO events (the
/// rows were already governance state — migration changes their spelling, not the corpus).
pub fn migrate_policies_to_steering(store: &mut dyn GraphStore) -> anyhow::Result<PolicyMigration> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(POLICY.to_string())],
        ..Default::default()
    };
    let mut report = PolicyMigration::default();
    let mut writes = Vec::new();
    for node in store.find_symbols(&query)? {
        let policy = Policy::from_node(&node)?;
        // A misfiled node (metadata id ≠ filed symbol) migrated blind would mint a unified row
        // under the metadata id while select's legacy fallback keys on the symbol — skip + report
        // (same inconsistency retire_policy refuses to write through).
        if node.symbol != synthetic_symbol(POLICY, &policy.id) {
            report.skipped.push(format!(
                "{}: filed under {} (misfiled)",
                policy.id, node.symbol
            ));
            continue;
        }
        let rule_symbol = synthetic_symbol(CONFORMANCE_RULE, &policy.id);
        match store.get_node(&rule_symbol)? {
            Some(existing) if existing.kind == NodeKind::Rule => {
                let unified = ConformanceRule::from_node(&existing)?;
                if unified.effect.is_some() {
                    report.already_unified.push(policy.id.clone());
                } else {
                    report.conflicts.push(policy.id.clone());
                }
                continue;
            }
            Some(_) => {
                // A foreign node already holds the address — never a write target.
                report.conflicts.push(policy.id.clone());
                continue;
            }
            None => {}
        }
        let steering = steering_rule_from_policy(&policy);
        if let Err(e) = steering.validate() {
            report.skipped.push(format!("{}: {e}", policy.id));
            continue;
        }
        writes.push(steering.to_node());
        report.migrated.push(policy.id.clone());
    }
    if !writes.is_empty() {
        store.begin_batch()?;
        store.upsert_nodes(&writes)?;
        store.commit_batch()?;
    }
    // Deterministic report regardless of scan order.
    report.migrated.sort();
    report.already_unified.sort();
    report.conflicts.sort();
    report.skipped.sort();
    Ok(report)
}

/// The legacy `Other(POLICY)` rows on a store, keyed for the read-time shim: `(policy, filed_ok)`.
/// Shared by [`crate::engine::select_any`]'s fallback so an un-migrated store never fails open.
pub(crate) fn legacy_policies(store: &dyn GraphRead) -> anyhow::Result<Vec<Policy>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(POLICY.to_string())],
        ..Default::default()
    };
    let mut out = Vec::new();
    for node in store.find_symbols(&query)? {
        out.push(Policy::from_node(&node)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Effect;
    use crate::engine::{conform, decide, register_policy, select_any};
    use wicked_apps_core::{open_store, GraphWrite};

    fn policy(id: &str, kind: &str, severity: Severity) -> Policy {
        Policy {
            id: id.to_string(),
            kind: kind.to_string(),
            applies_to: vec!["build".to_string()],
            effect: Effect::Deny,
            trigger: Trigger {
                contains: Some("FORBIDDEN".to_string()),
            },
            obligations: vec![],
            criteria: format!("criteria for {id}"),
            severity,
            rule: format!("rule text for {id}"),
            retired: false,
        }
    }

    /// Write a policy the way the PRE-STEERING release did: the legacy node ONLY (no unified
    /// twin) — what an un-migrated store actually holds.
    fn register_old_style(store: &mut dyn GraphStore, policy: &Policy) {
        store.begin_batch().unwrap();
        store.upsert_nodes(&[policy.to_node()]).unwrap();
        store.commit_batch().unwrap();
    }

    #[test]
    fn kind_mapping_is_the_documented_table() {
        // The seven steering types map to themselves.
        for t in STEERING_TYPES {
            assert_eq!(steering_type_for_policy_kind(t), t);
        }
        // guardrail/gate kinds → operations; everything else defaults to operations.
        for legacy in ["guardrail", "gate", "ops", "misc", "SECURITY-ish"] {
            assert_eq!(steering_type_for_policy_kind(legacy), "operations");
        }
        // Case-insensitive on the way in.
        assert_eq!(steering_type_for_policy_kind("Security"), "security");
    }

    #[test]
    fn severity_maps_are_monotone_and_mutually_inverse_on_the_policy_range() {
        let pairs = [
            (Severity::High, ConfSeverity::Critical),
            (Severity::Medium, ConfSeverity::Error),
            (Severity::Low, ConfSeverity::Warn),
        ];
        for (p, c) in pairs {
            assert_eq!(conf_severity_for_policy(p), c);
            assert_eq!(policy_severity_for(c), p, "round-trips on the policy range");
        }
        // Monotone: the mapped ranks order exactly as the source ranks did.
        assert!(
            conf_severity_for_policy(Severity::High).rank()
                > conf_severity_for_policy(Severity::Medium).rank()
        );
        assert!(
            conf_severity_for_policy(Severity::Medium).rank()
                > conf_severity_for_policy(Severity::Low).rank()
        );
        assert_eq!(policy_severity_for(ConfSeverity::Info), Severity::Low);
    }

    #[test]
    fn policy_round_trips_through_the_steering_projection_for_decide() {
        let p = policy("pol-x", "security", Severity::Medium);
        let rule = steering_rule_from_policy(&p);
        rule.validate().expect("a projected policy validates");
        assert_eq!(rule.steering_type, "security");
        assert_eq!(rule.effect, Some(Effect::Deny));
        let view = policy_view(&rule).expect("effect-bearing → Some view");
        // Everything decide() reads round-trips exactly; `kind` reads back as the steering type.
        assert_eq!(
            (
                view.id.as_str(),
                &view.applies_to,
                view.effect,
                &view.trigger,
                &view.obligations,
                view.criteria.as_str(),
                view.severity,
                view.rule.as_str(),
                view.retired,
                view.kind.as_str(),
            ),
            (
                "pol-x",
                &p.applies_to,
                p.effect,
                &p.trigger,
                &p.obligations,
                p.criteria.as_str(),
                p.severity,
                p.rule.as_str(),
                false,
                "security",
            )
        );
        // A recall-only rule never decides.
        assert!(policy_view(&ConformanceRule::default()).is_none());
    }

    /// THE golden test: register a policy OLD-STYLE (legacy node only), record the decision a gate
    /// derives; migrate; replay the same gate. The two claims must be BYTE-equal — the migration
    /// preserves policy semantics exactly.
    #[test]
    fn migrated_policy_decisions_are_byte_equal_to_the_old_rows() {
        crate::events::hermetic_test_spool();
        let mut store = open_store(Some(":memory:")).unwrap();
        // Two policies so precedence ordering (severity desc, id asc) is actually exercised.
        register_old_style(&mut store, &policy("pol-b-low", "guardrail", Severity::Low));
        register_old_style(
            &mut store,
            &policy("pol-a-high", "security", Severity::High),
        );

        let context = serde_json::json!({
            "phase": "build",
            "plan": "this plan says FORBIDDEN twice: FORBIDDEN",
        });
        let evaluated_at = 1_750_000_000;

        let selected_before = select_any(&store, "repo:x", &["build"], &context).unwrap();
        assert_eq!(
            selected_before.len(),
            2,
            "the read-time shim selects un-migrated legacy rows (never fail open)"
        );
        let claim_before = decide(&selected_before, "repo:x", "build", &context, evaluated_at);

        let report = migrate_policies_to_steering(&mut store).unwrap();
        assert_eq!(report.migrated, vec!["pol-a-high", "pol-b-low"]);
        assert!(report.conflicts.is_empty() && report.skipped.is_empty());

        let selected_after = select_any(&store, "repo:x", &["build"], &context).unwrap();
        let claim_after = decide(&selected_after, "repo:x", "build", &context, evaluated_at);

        // Byte-equal: the serialized claims are identical, not merely equivalent.
        assert_eq!(
            serde_json::to_string(&claim_before).unwrap(),
            serde_json::to_string(&claim_after).unwrap(),
            "a migrated policy must decide EXACTLY as its old row did"
        );
        assert_eq!(claim_after.decision, wicked_apps_core::Decision::Deny);
        assert_eq!(
            claim_after.policy_ids,
            vec!["pol-a-high".to_string(), "pol-b-low".to_string()],
            "precedence order (severity desc, id asc) is preserved through the severity map"
        );

        // And the recorded claim still conforms against the unified store.
        conform(&mut store, &claim_after).unwrap();

        // Idempotent: a second pass converges with nothing new to do.
        let again = migrate_policies_to_steering(&mut store).unwrap();
        assert_eq!(again.migrated, Vec::<String>::new());
        assert_eq!(again.already_unified, vec!["pol-a-high", "pol-b-low"]);
    }

    /// AllowWithConditions obligations survive the migration byte-for-byte too (the non-deny
    /// decision shape, where obligations and criteria actually reach the claim).
    #[test]
    fn migrated_obligations_and_criteria_are_byte_equal() {
        crate::events::hermetic_test_spool();
        let mut store = open_store(Some(":memory:")).unwrap();
        let mut p = policy("pol-notify", "ops", Severity::Medium);
        p.effect = Effect::AllowWithConditions;
        p.trigger = Trigger::default(); // fires whenever phase-selected
        p.obligations = vec!["notify-secops".into(), "tag-release".into()];
        register_old_style(&mut store, &p);

        let context = serde_json::json!({ "phase": "build" });
        let before = decide(
            &select_any(&store, "s", &["build"], &context).unwrap(),
            "s",
            "build",
            &context,
            1,
        );
        migrate_policies_to_steering(&mut store).unwrap();
        let after = decide(
            &select_any(&store, "s", &["build"], &context).unwrap(),
            "s",
            "build",
            &context,
            1,
        );
        assert_eq!(
            serde_json::to_string(&before).unwrap(),
            serde_json::to_string(&after).unwrap()
        );
        assert_eq!(
            after.decision,
            wicked_apps_core::Decision::AllowWithConditions
        );
        assert_eq!(after.obligations, vec!["notify-secops", "tag-release"]);
        assert_eq!(after.criteria, "criteria for pol-notify");
    }

    /// Migration honors `retired`, keeps the legacy node in place (audit), reports id conflicts
    /// with recall-only rules instead of overwriting, and skips misfiled rows loudly.
    #[test]
    fn migration_honors_retired_reports_conflicts_and_keeps_legacy_nodes() {
        crate::events::hermetic_test_spool();
        let mut store = open_store(Some(":memory:")).unwrap();

        let mut retired = policy("pol-retired", "gate", Severity::Low);
        retired.retired = true;
        register_old_style(&mut store, &retired);

        // A policy whose id collides with an EXISTING recall-only wiki rule.
        crate::conformance::register_rule(
            &mut store,
            &ConformanceRule {
                id: "POL-333".into(),
                rule_type: RuleType::Policy,
                statement: "a wiki rule".into(),
                severity: ConfSeverity::Info,
                confidence: 0.9,
                provenance: RuleProvenance {
                    reference: Some("wiki.md@sha#POL-333".into()),
                    source_kinds: vec!["doc".into()],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        register_old_style(&mut store, &policy("POL-333", "security", Severity::High));

        // A misfiled legacy node (metadata id ≠ filed symbol).
        let victim = policy("pol-victim", "ops", Severity::Low);
        let mut node = victim.to_node();
        node.symbol = synthetic_symbol(POLICY, "pol-misfiled");
        store.begin_batch().unwrap();
        store.upsert_nodes(&[node]).unwrap();
        store.commit_batch().unwrap();

        let report = migrate_policies_to_steering(&mut store).unwrap();
        assert_eq!(report.migrated, vec!["pol-retired"]);
        assert_eq!(report.conflicts, vec!["POL-333"]);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].contains("pol-victim") && report.skipped[0].contains("misfiled"));

        // Retired flag honored on the unified row; the wiki rule is untouched.
        let unified =
            crate::conformance::list_rules(&store, &crate::conformance::RuleQuery::default(), true)
                .unwrap();
        let migrated = unified.iter().find(|r| r.id == "pol-retired").unwrap();
        assert!(migrated.retired && migrated.effect.is_some());
        let wiki = unified.iter().find(|r| r.id == "POL-333").unwrap();
        assert!(
            wiki.effect.is_none() && wiki.statement == "a wiki rule",
            "the conflicting wiki rule must not be overwritten"
        );

        // Legacy nodes retained (retired-not-deleted: past decisions stay resolvable).
        assert!(store
            .get_node(&synthetic_symbol(POLICY, "pol-retired"))
            .unwrap()
            .is_some());
    }

    /// `register_policy` (the shim) dual-writes; the unified row wins in select without
    /// double-counting, and re-registering stays idempotent on the stable id.
    #[test]
    fn register_policy_shim_dual_writes_without_double_selection() {
        crate::events::hermetic_test_spool();
        let mut store = open_store(Some(":memory:")).unwrap();
        let p = policy("pol-dual", "security", Severity::High);
        register_policy(&mut store, &p).unwrap();
        register_policy(&mut store, &p).unwrap(); // idempotent re-register

        let ctx = serde_json::json!({});
        let selected = select_any(&store, "s", &["build"], &ctx).unwrap();
        assert_eq!(
            selected.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            vec!["pol-dual"],
            "one policy, selected once (unified row wins over its legacy twin)"
        );

        // Both rows exist: the unified steering rule and the legacy audit node.
        assert!(store
            .get_node(&synthetic_symbol(CONFORMANCE_RULE, "pol-dual"))
            .unwrap()
            .is_some());
        assert!(store
            .get_node(&synthetic_symbol(POLICY, "pol-dual"))
            .unwrap()
            .is_some());
    }

    /// The shim refuses a policy id that collides with an existing recall-only steering rule —
    /// registering it would silently swallow a wiki rule at `conformance_rule/<id>`.
    #[test]
    fn register_policy_refuses_a_recall_only_id_collision() {
        crate::events::hermetic_test_spool();
        let mut store = open_store(Some(":memory:")).unwrap();
        crate::conformance::register_rule(
            &mut store,
            &ConformanceRule {
                id: "POL-777".into(),
                rule_type: RuleType::Policy,
                statement: "wiki".into(),
                severity: ConfSeverity::Info,
                confidence: 0.5,
                provenance: RuleProvenance {
                    reference: Some("wiki.md@sha#POL-777".into()),
                    source_kinds: vec!["doc".into()],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        let err = register_policy(&mut store, &policy("POL-777", "security", Severity::High))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("POL-777") && err.contains("recall-only"),
            "{err}"
        );
    }

    /// The `excludes` twin: a phase listed there withdraws the rule from that phase even though
    /// `applies_to` includes it — exclusion dominates inclusion.
    #[test]
    fn excludes_dominates_applies_to_in_select() {
        crate::events::hermetic_test_spool();
        let mut store = open_store(Some(":memory:")).unwrap();
        let mut rule = steering_rule_from_policy(&policy("pol-excl", "ops", Severity::Medium));
        rule.applies_to = vec!["build".into(), "review".into()];
        rule.excludes = vec!["review".into()];
        crate::conformance::register_rule(&mut store, &rule).unwrap();

        let ctx = serde_json::json!({});
        let at_build = select_any(&store, "s", &["build"], &ctx).unwrap();
        assert_eq!(at_build.len(), 1, "included and not excluded → selected");
        let at_review = select_any(&store, "s", &["review"], &ctx).unwrap();
        assert!(at_review.is_empty(), "the excluded phase never selects it");
        let mixed = select_any(&store, "s", &["build", "review"], &ctx).unwrap();
        assert!(
            mixed.is_empty(),
            "exclusion dominates: any excluded token withdraws the rule from the whole gate"
        );
    }
}
