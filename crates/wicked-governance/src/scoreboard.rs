//! Population/connection scoreboard — coverage metrics for the wiki (AW-23 / arch-R23).
//!
//! The mission is population and connection, yet nothing measured either: a populated wiki and an
//! ingested-once-and-decaying one looked identical. This module aggregates the raw signals the
//! rest of the crate already produces into one report, so the difference is visible at a glance:
//!
//! - **typing** — % of doctrine statements typed into enforcement classes. The class lives in doc
//!   FRONTMATTER (`enforcement_class: policy|validator|guidance`, arch-R4), which never rides the
//!   rule node — so this half needs the same docs root `rules ingest --dir` used, and is reported
//!   `available: false` (with the reason) when no root is supplied. Countable at ingest, counted
//!   here with the ONE parse convention ([`MarkdownAdapter`] — no second parse path).
//! - **connection** — % of active rules whose `symbol_ref` resolves at the CURRENT epoch (the same
//!   read-only resolution `rules drift` runs), plus how many carry live `Governs` edges.
//! - **enforcement evidence** — denials citing wiki rules: distinct deny claims with
//!   [`crate::conformance::EVIDENCED_BY`] edges, the rules they cite, and the accumulated
//!   `evidence_count` on rule → code `Governs` edges ([`crate::record_rule_evidence`] wires both).
//! - **recall volume** — documented UNAVAILABLE: `recall_rules` is a pure read, and while the
//!   store schema DOES carry telemetry tables (`access_log` / `search_misses`), nothing in the
//!   recall funnel writes them — their only writer is estate's one-time brain-consolidation
//!   `import-telemetry` bulk import, so they hold no `rules.recall` signal. An honest "cannot
//!   measure" beats a fabricated zero.
//!
//! Everything here is READ-ONLY (`&dyn GraphRead` + an optional doc scan); pair it with
//! `open_store_ro` so the scoreboard can run beside a live single-writer daemon.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;
use wicked_apps_core::{synthetic_symbol, EdgeKind, GraphRead};
use wicked_estate_core::Direction;

use crate::conformance::{CONFORMANCE_RULE, EVIDENCED_BY};
use crate::ingest::SourceAdapter;
use crate::markdown::MarkdownAdapter;
use crate::relink::{load_conformance_rules, parse_symbol_ref, resolve_ref};

/// `n` of `d` as a percentage, or `None` when the denominator is empty (0/0 must render as
/// "nothing to measure", never as 0% or 100% — both would lie in opposite directions).
fn percent(n: usize, d: usize) -> Option<f64> {
    (d > 0).then(|| (n as f64) * 100.0 / (d as f64))
}

/// Typing coverage — statements typed into enforcement classes (doc-side, arch-R4 frontmatter).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TypingCoverage {
    /// A docs root was supplied and scanned. When `false`, only `reason` is meaningful.
    pub available: bool,
    /// Why typing could not be measured (only when `available` is false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub docs_scanned: usize,
    /// Rule statements minted by the scanned docs.
    pub statements_total: usize,
    /// Statements in docs whose frontmatter declares an `enforcement_class`.
    pub statements_typed: usize,
    /// `statements_typed / statements_total` (absent when the corpus mints no statements).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    /// Typed statements per class (`policy` / `validator` / `guidance`).
    pub by_class: BTreeMap<String, usize>,
    /// Docs that mint statements but declare NO class — the actionable backlog.
    pub docs_untyped: Vec<String>,
}

/// Connection coverage — do the ACTIVE rules' `symbol_ref`s resolve, and are the links live?
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ConnectionCoverage {
    pub rules_with_ref: usize,
    /// Refs that parse AND resolve (within the ambiguity cap) at the current epoch.
    pub refs_resolving: usize,
    /// Refs that fail to parse, match nothing, or exceed the cap (`rules drift` names each one).
    pub refs_unresolvable: usize,
    /// `refs_resolving / rules_with_ref` (absent when no rule carries a ref).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    /// Active rules with at least one live `Governs` edge (the relink pass derived it).
    pub rules_linked: usize,
}

/// One rule's enforcement evidence (only rules with any evidence appear).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleEvidenceRow {
    pub rule_id: String,
    /// Distinct deny claims citing this rule (`evidenced_by` edges in).
    pub denial_claims: usize,
    /// Accumulated `evidence_count` across the rule's `Governs` edges.
    pub governs_evidence: u64,
}

/// Enforcement evidence — gate denials citing wiki rules (over ALL rules, retired included:
/// a past denial stays explicable after its rule retires, so its evidence stays countable).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct EnforcementEvidence {
    /// Distinct deny claims that cite at least one wiki rule — "denials citing wiki".
    pub denial_claims: usize,
    /// Rules cited by at least one denial.
    pub rules_evidenced: usize,
    /// Total `evidenced_by` edges (a claim citing two rules counts twice here, once above).
    pub evidenced_by_edges: usize,
    /// Sum of `evidence_count` across every rule's `Governs` edges.
    pub governs_evidence_total: u64,
    /// Per-rule breakdown, most-evidenced first (then id) — the "which rules actually fire" list.
    pub per_rule: Vec<RuleEvidenceRow>,
}

/// Recall volume — documented unavailable (see the module docs). The struct exists so the report
/// SAYS SO in-band instead of silently omitting the metric arch-R23 asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecallVolume {
    pub available: bool,
    pub reason: String,
}

/// One steering type's population slice (STEERING: the per-sub-page counts).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SteeringTypeCount {
    pub total: usize,
    pub active: usize,
    pub retired: usize,
    /// Decide-lane rows (effect-bearing — the merged Policy model) within this type.
    pub enforcing: usize,
}

/// The population/connection scoreboard (arch-R23).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Scoreboard {
    pub rules_total: usize,
    pub rules_active: usize,
    pub rules_retired: usize,
    /// Population per steering type (STEERING) — one row per type that has any rules, keyed by
    /// the [`crate::STEERING_TYPES`] spelling (pre-steering rows count under the default type).
    pub by_type: BTreeMap<String, SteeringTypeCount>,
    pub typing: TypingCoverage,
    pub connection: ConnectionCoverage,
    pub evidence: EnforcementEvidence,
    pub recall_volume: RecallVolume,
}

/// Compute the scoreboard over `store`, optionally measuring typing coverage against the rule
/// docs under `docs_root` (the same root `rules ingest --dir` used). Read-only.
pub fn scoreboard(
    store: &dyn GraphRead,
    docs_root: Option<&Path>,
    ambiguity_cap: usize,
) -> anyhow::Result<Scoreboard> {
    let rules = load_conformance_rules(store)?;
    let rules_total = rules.len();
    let rules_retired = rules.iter().filter(|r| r.retired).count();

    // ── STEERING by-type breakdown: population per steering sub-page ──
    let mut by_type: BTreeMap<String, SteeringTypeCount> = BTreeMap::new();
    for rule in &rules {
        let row = by_type.entry(rule.steering_type.clone()).or_default();
        row.total += 1;
        if rule.retired {
            row.retired += 1;
        } else {
            row.active += 1;
        }
        if rule.effect.is_some() {
            row.enforcing += 1;
        }
    }

    // ── typing (doc-side — the class lives in frontmatter, not on the rule node) ──
    let typing = match docs_root {
        None => TypingCoverage {
            available: false,
            reason: Some(
                "no docs root supplied — enforcement_class lives in doc frontmatter (never on \
                 the rule node), so typing coverage needs the same --dir `rules ingest` used"
                    .to_string(),
            ),
            ..Default::default()
        },
        Some(root) => {
            let mut typing = TypingCoverage {
                available: true,
                ..Default::default()
            };
            for doc in MarkdownAdapter::new(root).fetch()? {
                typing.docs_scanned += 1;
                let statements = doc["rules"].as_array().map(Vec::len).unwrap_or(0);
                typing.statements_total += statements;
                match doc["doc"]["enforcement_class"].as_str() {
                    Some(class) => {
                        typing.statements_typed += statements;
                        *typing.by_class.entry(class.to_string()).or_insert(0) += statements;
                    }
                    // A doc-only ingest (zero statements) has nothing to type — not backlog.
                    None if statements > 0 => {
                        if let Some(path) = doc["doc"]["path"].as_str() {
                            typing.docs_untyped.push(path.to_string());
                        }
                    }
                    None => {}
                }
            }
            typing.percent = percent(typing.statements_typed, typing.statements_total);
            typing
        }
    };

    // ── connection (active rules) + enforcement evidence (all rules), one pass ──
    let mut connection = ConnectionCoverage::default();
    let mut evidence = EnforcementEvidence::default();
    let mut deny_claims: BTreeSet<String> = BTreeSet::new();
    for rule in &rules {
        let rule_sym = synthetic_symbol(CONFORMANCE_RULE, &rule.id);
        let governs: Vec<_> = store
            .neighbors(&rule_sym, Direction::Dependencies)?
            .into_iter()
            .filter(|e| e.kind == EdgeKind::Governs)
            .collect();

        if !rule.retired {
            if !governs.is_empty() {
                connection.rules_linked += 1;
            }
            if let Some(symbol_ref) = rule.symbol_ref.as_deref() {
                connection.rules_with_ref += 1;
                let resolves = match parse_symbol_ref(symbol_ref) {
                    Err(_) => false,
                    Ok(qref) => {
                        let matches = resolve_ref(store, &qref)?;
                        !matches.is_empty() && matches.len() <= ambiguity_cap
                    }
                };
                if resolves {
                    connection.refs_resolving += 1;
                } else {
                    connection.refs_unresolvable += 1;
                }
            }
        }

        let governs_evidence: u64 = governs.iter().map(|e| u64::from(e.evidence_count)).sum();
        evidence.governs_evidence_total += governs_evidence;
        let citing: Vec<_> = store
            .neighbors(&rule_sym, Direction::Dependents)?
            .into_iter()
            .filter(|e| matches!(&e.kind, EdgeKind::Other(k) if k.as_str() == EVIDENCED_BY))
            .collect();
        if citing.is_empty() && governs_evidence == 0 {
            continue;
        }
        if !citing.is_empty() {
            evidence.rules_evidenced += 1;
        }
        evidence.evidenced_by_edges += citing.len();
        for edge in &citing {
            deny_claims.insert(edge.source.as_str().to_string());
        }
        evidence.per_rule.push(RuleEvidenceRow {
            rule_id: rule.id.clone(),
            denial_claims: citing.len(),
            governs_evidence,
        });
    }
    connection.percent = percent(connection.refs_resolving, connection.rules_with_ref);
    evidence.denial_claims = deny_claims.len();
    evidence.per_rule.sort_by(|a, b| {
        b.denial_claims
            .cmp(&a.denial_claims)
            .then_with(|| a.rule_id.cmp(&b.rule_id))
    });

    Ok(Scoreboard {
        rules_total,
        rules_active: rules_total - rules_retired,
        rules_retired,
        by_type,
        typing,
        connection,
        evidence,
        recall_volume: RecallVolume {
            available: false,
            reason: "not measurable from this store: recall_rules is a pure read, and the \
                     store's telemetry tables (access_log / search_misses) have no live writer \
                     — only estate's one-time brain-consolidation import populates them, so \
                     they carry no rules.recall signal"
                .to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{
        record_rule_evidence, register_rule, ConfSeverity, ConformanceRule, RuleProvenance,
        RuleType, Targets,
    };
    use crate::relink::{relink, DEFAULT_AMBIGUITY_CAP};
    use crate::{conform, ingest_from, register_rule_sets, retire_rule, RuleSetGrouping};
    use std::path::PathBuf;
    use wicked_apps_core::{
        open_store, ConformanceClaim, Decision, GraphWrite, Language, Location, Node, NodeKind,
        Span, Symbol,
    };

    /// A fresh per-test doc root with the given files.
    fn doc_root(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wicked-gov-scoreboard-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, content) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        dir
    }

    fn deny_claim(id: &str, obligations: Vec<String>) -> ConformanceClaim {
        ConformanceClaim {
            claim_id: id.to_string(),
            scope: "repo:test".into(),
            phase: "build".into(),
            policy_ids: vec![],
            decision: Decision::Deny,
            obligations,
            evaluated_context_ref: "sha256:test".into(),
            criteria: String::new(),
            evaluator_identity: "wicked-governance@test".into(),
            evaluated_at: 1_750_000_000,
        }
    }

    const TYPED_DOC: &str = "---\nid: doctrine\ntitle: Doctrine\nenforcement_class: policy\n---\n\
        \n## Rules\n\n\
        - POL-001 (critical): single-writer only.\n\
        - PAT-002 (error): never printf without %s.\n";
    const UNTYPED_DOC: &str = "---\nid: lore\ntitle: Lore\n---\n\n## Rules\n\n\
        - PAT-003 (info): prefer tables in chat.\n";
    const DOC_ONLY: &str = "---\nid: prose\ntitle: Prose only\n---\n\nNo rules section here.\n";

    /// The AC in one test: a POPULATED store (typed docs, resolving ref, live links, an evidenced
    /// denial) and a DECAYING one (untyped, unresolvable, never denied) produce scoreboards an
    /// operator can tell apart at a glance.
    #[test]
    fn populated_and_decaying_stores_are_distinguishable() {
        crate::events::hermetic_test_spool();

        // ── populated ──
        let mut store = open_store(Some(":memory:")).unwrap();
        let root = doc_root(
            "populated",
            &[
                ("doctrine.md", TYPED_DOC),
                ("lore.md", UNTYPED_DOC),
                ("prose.md", DOC_ONLY),
            ],
        );
        for rule in ingest_from(&MarkdownAdapter::new(&root)).unwrap() {
            register_rule(&mut store, &rule).unwrap();
        }
        // A real code symbol + a ref onto it, relinked → a live Governs edge.
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
        let mut linked = ConformanceRule {
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
            ..Default::default()
        };
        register_rule(&mut store, &linked).unwrap();
        relink(&mut store, DEFAULT_AMBIGUITY_CAP, 1_000).unwrap();
        // A recorded denial citing the linked rule — through conform, the real funnel.
        conform(
            &mut store,
            &deny_claim(
                "claim-populated-1",
                vec!["conform:Critical:POL-100:writes go through the single writer".into()],
            ),
        )
        .unwrap();

        let populated = scoreboard(&store, Some(&root), DEFAULT_AMBIGUITY_CAP).unwrap();
        assert_eq!(populated.rules_total, 4);
        assert_eq!(populated.rules_active, 4);
        let typing = &populated.typing;
        assert!(typing.available);
        assert_eq!(typing.docs_scanned, 3);
        assert_eq!(
            (typing.statements_typed, typing.statements_total),
            (2, 3),
            "2 of 3 doc statements are typed (POL-100 is JSON-lane, not in the docs)"
        );
        assert_eq!(typing.by_class.get("policy"), Some(&2));
        assert_eq!(typing.docs_untyped, vec!["lore.md".to_string()]);
        assert!((typing.percent.unwrap() - 66.666).abs() < 0.01);
        assert_eq!(populated.connection.rules_with_ref, 1);
        assert_eq!(populated.connection.refs_resolving, 1);
        assert_eq!(populated.connection.rules_linked, 1);
        assert_eq!(populated.connection.percent, Some(100.0));
        assert_eq!(populated.evidence.denial_claims, 1);
        assert_eq!(populated.evidence.rules_evidenced, 1);
        assert_eq!(
            populated.evidence.governs_evidence_total, 1,
            "the denial bumped the Governs edge's evidence_count"
        );
        assert_eq!(
            populated.evidence.per_rule,
            vec![RuleEvidenceRow {
                rule_id: "POL-100".into(),
                denial_claims: 1,
                governs_evidence: 1,
            }]
        );
        assert!(!populated.recall_volume.available, "documented unavailable");
        assert!(populated.recall_volume.reason.contains("telemetry"));

        // A SECOND distinct denial takes the Governs edge's evidence_count 1 → 2 — this exercises
        // the store's merge-growth clause at EQUAL confidence (an upsert that only kept
        // higher-confidence rows would silently drop the bump; 0 → 1 alone can't tell).
        conform(
            &mut store,
            &deny_claim(
                "claim-populated-2",
                vec!["conform:Critical:POL-100:writes go through the single writer".into()],
            ),
        )
        .unwrap();
        let populated = scoreboard(&store, Some(&root), DEFAULT_AMBIGUITY_CAP).unwrap();
        assert_eq!(populated.evidence.denial_claims, 2);
        assert_eq!(
            populated.evidence.governs_evidence_total, 2,
            "each distinct denial increments the Governs evidence_count (monotonic accumulation)"
        );
        assert_eq!(
            populated.evidence.per_rule,
            vec![RuleEvidenceRow {
                rule_id: "POL-100".into(),
                denial_claims: 2,
                governs_evidence: 2,
            }]
        );

        // ── decaying: ingested once, untyped, ref rotted, never enforced ──
        let mut decayed = open_store(Some(":memory:")).unwrap();
        let decayed_root = doc_root("decaying", &[("lore.md", UNTYPED_DOC)]);
        for rule in ingest_from(&MarkdownAdapter::new(&decayed_root)).unwrap() {
            register_rule(&mut decayed, &rule).unwrap();
        }
        linked.id = "POL-101".into();
        linked.symbol_ref = Some("src/gone.rs::vanished".into());
        register_rule(&mut decayed, &linked).unwrap();

        let decaying = scoreboard(&decayed, Some(&decayed_root), DEFAULT_AMBIGUITY_CAP).unwrap();
        assert_eq!(decaying.typing.statements_typed, 0);
        assert_eq!(decaying.typing.percent, Some(0.0));
        assert_eq!(decaying.connection.refs_resolving, 0);
        assert_eq!(decaying.connection.refs_unresolvable, 1);
        assert_eq!(decaying.connection.percent, Some(0.0));
        assert_eq!(decaying.connection.rules_linked, 0);
        assert_eq!(decaying.evidence.denial_claims, 0);
        assert_eq!(decaying.evidence.governs_evidence_total, 0);
        assert!(decaying.evidence.per_rule.is_empty());
    }

    /// Without a docs root, typing is honestly UNAVAILABLE — never 0% (which would read as "all
    /// untyped") and never silently omitted.
    #[test]
    fn typing_is_unavailable_without_a_docs_root_and_zero_over_zero_is_none() {
        let store = open_store(Some(":memory:")).unwrap();
        let report = scoreboard(&store, None, DEFAULT_AMBIGUITY_CAP).unwrap();
        assert!(!report.typing.available);
        assert!(report.typing.reason.as_deref().unwrap().contains("--dir"));
        assert_eq!(report.typing.percent, None);
        // Empty store: 0/0 connection is "nothing to measure", not a percentage.
        assert_eq!(report.connection.percent, None);
        assert_eq!(report.rules_total, 0);
    }

    /// Distinct denials accumulate; the same claim re-conformed does not double-count; a rule's
    /// retirement keeps its accumulated evidence countable.
    #[test]
    fn evidence_accumulates_per_distinct_denial_and_survives_retirement() {
        crate::events::hermetic_test_spool();
        let mut store = open_store(Some(":memory:")).unwrap();
        let rule = ConformanceRule {
            id: "PAT-200".into(),
            rule_type: RuleType::Pattern,
            statement: "no printf without %s".into(),
            severity: ConfSeverity::Error,
            confidence: 0.8,
            targets: Targets::default(),
            symbol_ref: None,
            compliance: None,
            provenance: RuleProvenance {
                source: "markdown".into(),
                reference: Some("doctrine.md#PAT-200".into()),
                source_kinds: vec!["doc".into()],
            },
            retired: false,
            ..Default::default()
        };
        register_rule(&mut store, &rule).unwrap();
        // RuleSet grouping must not confuse the scoreboard's rule scan (native Contains edges in).
        register_rule_sets(
            &mut store,
            &[RuleSetGrouping {
                domain: "doctrine".to_string(),
                rule_ids: vec!["PAT-200".to_string()],
            }],
        )
        .unwrap();

        let ob = vec!["conform:Error:PAT-200:no printf without %s".to_string()];
        conform(&mut store, &deny_claim("claim-a", ob.clone())).unwrap();
        conform(&mut store, &deny_claim("claim-a", ob.clone())).unwrap(); // re-conformed, same claim
        conform(&mut store, &deny_claim("claim-b", ob.clone())).unwrap();
        retire_rule(&mut store, "PAT-200").unwrap();

        let report = scoreboard(&store, None, DEFAULT_AMBIGUITY_CAP).unwrap();
        assert_eq!(report.rules_retired, 1);
        assert_eq!(
            report.evidence.denial_claims, 2,
            "claim-a counted once, claim-b once"
        );
        assert_eq!(report.evidence.evidenced_by_edges, 2);
        assert_eq!(report.evidence.per_rule.len(), 1);
        assert_eq!(report.evidence.per_rule[0].denial_claims, 2);
        // record_rule_evidence on a retired rule still resolves the node (retire-not-delete).
        let after = record_rule_evidence(&mut store, &deny_claim("claim-c", ob)).unwrap();
        assert_eq!(after.evidenced, vec!["PAT-200".to_string()]);
    }

    /// STEERING: the scoreboard's by-type breakdown counts population per steering sub-page —
    /// pre-steering rows under the default type, retired and decide-lane rows called out.
    #[test]
    fn scoreboard_breaks_population_down_by_steering_type() {
        crate::events::hermetic_test_spool();
        let mut store = open_store(Some(":memory:")).unwrap();
        // A defaulted (pre-steering shape) rule → architecture.
        register_rule(
            &mut store,
            &ConformanceRule {
                id: "PAT-300".into(),
                rule_type: RuleType::Pattern,
                statement: "arch rule".into(),
                severity: ConfSeverity::Info,
                confidence: 0.5,
                ..Default::default()
            },
        )
        .unwrap();
        // Two security rules, one of them retired.
        for id in ["PAT-301", "PAT-302"] {
            register_rule(
                &mut store,
                &ConformanceRule {
                    id: id.into(),
                    rule_type: RuleType::Pattern,
                    statement: "sec rule".into(),
                    severity: ConfSeverity::Warn,
                    confidence: 0.5,
                    steering_type: "security".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        retire_rule(&mut store, "PAT-302").unwrap();
        // A migrated-policy-shaped decide-lane rule → operations, enforcing.
        register_rule(
            &mut store,
            &ConformanceRule {
                id: "pol-ops".into(),
                rule_type: RuleType::Policy,
                statement: "deny x".into(),
                severity: ConfSeverity::Critical,
                confidence: 1.0,
                steering_type: "operations".into(),
                applies_to: vec!["build".into()],
                effect: Some(crate::Effect::Deny),
                ..Default::default()
            },
        )
        .unwrap();

        let report = scoreboard(&store, None, DEFAULT_AMBIGUITY_CAP).unwrap();
        assert_eq!(report.rules_total, 4);
        let arch = &report.by_type["architecture"];
        assert_eq!(
            (arch.total, arch.active, arch.retired, arch.enforcing),
            (1, 1, 0, 0)
        );
        let sec = &report.by_type["security"];
        assert_eq!(
            (sec.total, sec.active, sec.retired, sec.enforcing),
            (2, 1, 1, 0)
        );
        let ops = &report.by_type["operations"];
        assert_eq!(
            (ops.total, ops.active, ops.retired, ops.enforcing),
            (1, 1, 0, 1)
        );
        assert_eq!(report.by_type.len(), 3, "only types that hold rules appear");
    }
}
