//! Drift detection — the residue that idempotent re-ingest cannot self-heal (AW-10 / arch-R7).
//!
//! The mechanism is deliberately simple. Markdown IS digest-tracked by estate's incremental index,
//! but re-extraction refreshes doc-STRUCTURE symbols, never the adapter-minted `Rule` nodes — so
//! rule-level drift needs its own ingest-side detector. On merge to HEAD an idempotent
//! `wicked-core rules ingest` re-run refreshes both projections ("doc changed" is a self-healing
//! non-event: the node upserts, the [`crate::provenance::stamp_provenance`] row refreshes); this
//! module reports what a re-ingest can NOT heal on its own:
//!
//! - **orphaned** — a store rule whose source doc is gone from disk, or whose id the doc no longer
//!   mints (deleting a doc must never silently orphan its derived rules — arch-R21's semantics
//!   start from this report feeding an explicit retire action);
//! - **uningested** — a rule doc on disk whose rules are absent from the store (`not_ingested`) or
//!   ingested at an OLDER content digest (`stale` — the on-merge re-ingest was missed);
//! - **unresolvable** — `symbol_ref`s that fail to parse or resolve at the current epoch (the same
//!   findings `rules relink` reports, re-derived read-only here);
//! - **unlinked** — a ref that RESOLVES but whose `Governs` edges are missing (the post-re-index
//!   state; `rules relink` heals it);
//! - **extraneous** — `Governs` edges from a rule to targets OUTSIDE its current resolution (a
//!   stale projection; reported because the store trait has no per-edge delete — surfacing beats
//!   silently trusting a stale link).
//!
//! Everything here is READ-ONLY (`&dyn GraphRead` + a filesystem scan); pair it with
//! `open_store_ro` so a drift check can run beside a live single-writer daemon. Doc comparison
//! uses the digest embedded in each rule's provenance ref (`<path>@<sha>#<id>` —
//! [`crate::provenance`]); a legacy pre-digest ref reports as `stale` (re-ingest heals it by
//! stamping the digest), never a crash. Run `rules drift --dir` with the SAME root as
//! `rules ingest --dir` — the refs are root-relative.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;
use wicked_apps_core::{synthetic_symbol, EdgeKind, GraphRead};
use wicked_estate_core::Direction;

use crate::conformance::CONFORMANCE_RULE;
use crate::ingest::SourceAdapter;
use crate::markdown::MarkdownAdapter;
use crate::provenance::parse_provenance_ref;
use crate::relink::{
    load_conformance_rules, parse_symbol_ref, resolve_ref, RefFailure, RelinkFinding,
};

/// Why a store rule is orphaned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrphanReason {
    /// The provenance doc path no longer exists under the scanned root.
    DocMissing,
    /// The doc exists but no longer mints this rule id.
    IdNotMinted,
}

/// A store rule whose source doc no longer backs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrphanedRule {
    pub rule_id: String,
    pub doc_path: String,
    pub reason: OrphanReason,
}

/// Why a doc (or one of its rules) is not current in the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UningestReason {
    /// The rule id is absent from the store entirely.
    NotIngested,
    /// The rule exists but was ingested at a different (or missing) content digest.
    Stale {
        recorded_sha: Option<String>,
        current_sha: String,
    },
}

/// One doc rule the store does not currently reflect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UningestedRule {
    pub doc_path: String,
    pub rule_id: String,
    pub reason: UningestReason,
}

/// A ref that resolves, but whose derived edges are missing (relink heals this).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UnlinkedRule {
    pub rule_id: String,
    pub symbol_ref: String,
    /// The resolved targets that have no `Governs` edge from the rule.
    pub missing_targets: Vec<String>,
}

/// A `Governs` edge from a rule to a target outside its current resolution.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExtraneousEdge {
    pub rule_id: String,
    pub target: String,
}

/// The full drift report. [`DriftReport::has_residue`] gates a CI exit code.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct DriftReport {
    pub orphaned: Vec<OrphanedRule>,
    pub uningested: Vec<UningestedRule>,
    pub unresolvable: Vec<RelinkFinding>,
    pub unlinked: Vec<UnlinkedRule>,
    pub extraneous: Vec<ExtraneousEdge>,
    pub rules_checked: usize,
    pub docs_scanned: usize,
    pub skipped_retired: usize,
    /// Doc checks ran (a `--dir` was supplied); without it only the ref checks run.
    pub docs_checked: bool,
}

impl DriftReport {
    /// Any residue an operator (or relink / re-ingest) must act on?
    pub fn has_residue(&self) -> bool {
        !self.orphaned.is_empty()
            || !self.uningested.is_empty()
            || !self.unresolvable.is_empty()
            || !self.unlinked.is_empty()
            || !self.extraneous.is_empty()
    }
}

/// One scanned doc: its current digest and the rule ids it mints right now.
struct ScannedDoc {
    sha: String,
    rule_ids: BTreeSet<String>,
}

/// Scan `root` with the ONE parse convention (the [`MarkdownAdapter`] — no second parse path) and
/// index the result per doc path. Fail-loud inherits from the adapter: a malformed doc aborts the
/// drift run with its path + reason, because a doc that cannot parse cannot be drift-compared.
fn scan_docs(root: &Path) -> anyhow::Result<BTreeMap<String, ScannedDoc>> {
    let mut docs = BTreeMap::new();
    for doc in MarkdownAdapter::new(root).fetch()? {
        let path = doc["doc"]["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("markdown adapter emitted a doc without a path"))?
            .to_string();
        let sha = doc["doc"]["sha"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("markdown adapter emitted a doc without a sha"))?
            .to_string();
        let mut rule_ids = BTreeSet::new();
        if let Some(rules) = doc["rules"].as_array() {
            for r in rules {
                if let Some(id) = r["id"].as_str() {
                    rule_ids.insert(id.to_string());
                }
            }
        }
        docs.insert(path, ScannedDoc { sha, rule_ids });
    }
    Ok(docs)
}

/// Run drift detection over `store`, optionally comparing against the rule docs under `docs_root`
/// (the same root `rules ingest --dir` used). Read-only: reports, never mutates, never drops.
pub fn drift(
    store: &dyn GraphRead,
    docs_root: Option<&Path>,
    ambiguity_cap: usize,
) -> anyhow::Result<DriftReport> {
    let rules = load_conformance_rules(store)?;
    let mut report = DriftReport {
        rules_checked: rules.len(),
        docs_checked: docs_root.is_some(),
        ..Default::default()
    };

    // ── Doc-side checks (markdown lane only — JSON-lane refs are author-supplied free-form). ──
    if let Some(root) = docs_root {
        let scanned = scan_docs(root)?;
        report.docs_scanned = scanned.len();

        // Store → docs: orphaned rules.
        for rule in &rules {
            if rule.retired {
                continue; // already withdrawn from recall — retirement IS the healed state.
            }
            // Doc comparison only makes sense for the markdown lane — JSON-lane refs are
            // author-supplied free-form strings, not paths under this root.
            if rule.provenance.source != "markdown" {
                continue;
            }
            let Some(reference) = rule.provenance.reference.as_deref() else {
                continue;
            };
            let parsed = parse_provenance_ref(reference);
            match scanned.get(&parsed.path) {
                None => report.orphaned.push(OrphanedRule {
                    rule_id: rule.id.clone(),
                    doc_path: parsed.path,
                    reason: OrphanReason::DocMissing,
                }),
                Some(doc) if !doc.rule_ids.contains(&rule.id) => {
                    report.orphaned.push(OrphanedRule {
                        rule_id: rule.id.clone(),
                        doc_path: parsed.path,
                        reason: OrphanReason::IdNotMinted,
                    })
                }
                Some(_) => {}
            }
        }

        // Docs → store: uningested / stale rules.
        let by_id: BTreeMap<&str, &crate::ConformanceRule> =
            rules.iter().map(|r| (r.id.as_str(), r)).collect();
        for (path, doc) in &scanned {
            for id in &doc.rule_ids {
                match by_id.get(id.as_str()) {
                    None => report.uningested.push(UningestedRule {
                        doc_path: path.clone(),
                        rule_id: id.clone(),
                        reason: UningestReason::NotIngested,
                    }),
                    Some(rule) => {
                        let recorded = rule
                            .provenance
                            .reference
                            .as_deref()
                            .and_then(|r| parse_provenance_ref(r).sha);
                        if recorded.as_deref() != Some(doc.sha.as_str()) {
                            report.uningested.push(UningestedRule {
                                doc_path: path.clone(),
                                rule_id: id.clone(),
                                reason: UningestReason::Stale {
                                    recorded_sha: recorded,
                                    current_sha: doc.sha.clone(),
                                },
                            });
                        }
                    }
                }
            }
        }
    }

    // ── Ref-side checks: unresolvable / unlinked / extraneous (relink's read-only twin). ──
    for rule in &rules {
        let Some(symbol_ref) = rule.symbol_ref.as_deref() else {
            continue;
        };
        if rule.retired {
            report.skipped_retired += 1;
            continue;
        }
        let qref = match parse_symbol_ref(symbol_ref) {
            Ok(q) => q,
            Err(reason) => {
                report.unresolvable.push(RelinkFinding {
                    rule_id: rule.id.clone(),
                    symbol_ref: symbol_ref.to_string(),
                    failure: RefFailure::Unqualified { reason },
                });
                continue;
            }
        };
        let matches = resolve_ref(store, &qref)?;
        if matches.is_empty() {
            report.unresolvable.push(RelinkFinding {
                rule_id: rule.id.clone(),
                symbol_ref: symbol_ref.to_string(),
                failure: RefFailure::Unresolved,
            });
            continue;
        }
        if matches.len() > ambiguity_cap {
            report.unresolvable.push(RelinkFinding {
                rule_id: rule.id.clone(),
                symbol_ref: symbol_ref.to_string(),
                failure: RefFailure::Ambiguous {
                    matches: matches.len(),
                    cap: ambiguity_cap,
                    sample: matches
                        .iter()
                        .take(ambiguity_cap)
                        .map(|n| n.symbol.as_str().to_string())
                        .collect(),
                },
            });
            continue;
        }

        let rule_sym = synthetic_symbol(CONFORMANCE_RULE, &rule.id);
        let linked: BTreeSet<String> = store
            .neighbors(&rule_sym, Direction::Dependencies)?
            .into_iter()
            .filter(|e| e.kind == EdgeKind::Governs)
            .map(|e| e.target.as_str().to_string())
            .collect();
        let expected: BTreeSet<String> = matches
            .iter()
            .map(|n| n.symbol.as_str().to_string())
            .collect();

        let missing: Vec<String> = expected.difference(&linked).cloned().collect();
        if !missing.is_empty() {
            report.unlinked.push(UnlinkedRule {
                rule_id: rule.id.clone(),
                symbol_ref: symbol_ref.to_string(),
                missing_targets: missing,
            });
        }
        for target in linked.difference(&expected) {
            report.extraneous.push(ExtraneousEdge {
                rule_id: rule.id.clone(),
                target: target.clone(),
            });
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{ConfSeverity, ConformanceRule, RuleProvenance, RuleType, Targets};
    use crate::provenance::stamp_provenance;
    use crate::relink::{relink, DEFAULT_AMBIGUITY_CAP};
    use crate::{ingest_from, register_rule};
    use std::path::PathBuf;
    use wicked_apps_core::{
        open_store, GraphStore, GraphWrite, Language, Location, Node, NodeKind, Span, Symbol,
    };

    /// A fresh per-test doc root with the given files.
    fn doc_root(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wicked-gov-drift-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, content) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        dir
    }

    /// Ingest every rule doc under `root` into `store` — the exact `rules ingest` re-ingest step
    /// (register + provenance stamp), so these tests exercise the real on-merge semantics.
    fn reingest(store: &mut dyn GraphStore, root: &Path, now: i64) {
        for rule in ingest_from(&MarkdownAdapter::new(root)).unwrap() {
            register_rule(store, &rule).unwrap();
            stamp_provenance(store, &rule, now).unwrap();
        }
    }

    const DOC_V1: &str = "---\nid: doctrine\ntitle: Doctrine\n---\n\n## Rules\n\n\
        - PAT-001 (error): never use printf without %s.\n\
        - POL-002 (critical): single-writer only.\n";
    const DOC_V2: &str = "---\nid: doctrine\ntitle: Doctrine\n---\n\n## Rules\n\n\
        - PAT-001 (error): never use printf without %s, revised wording.\n\
        - POL-002 (critical): single-writer only.\n";
    /// v3 drops POL-002 entirely.
    const DOC_V3: &str = "---\nid: doctrine\ntitle: Doctrine\n---\n\n## Rules\n\n\
        - PAT-001 (error): never use printf without %s, revised wording.\n";

    #[test]
    fn a_freshly_ingested_tree_has_no_residue() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let root = doc_root("clean", &[("doctrine.md", DOC_V1)]);
        reingest(&mut store, &root, 1_000);

        let report = drift(&store, Some(&root), DEFAULT_AMBIGUITY_CAP).unwrap();
        assert!(
            !report.has_residue(),
            "clean state must be clean: {report:?}"
        );
        assert_eq!(report.docs_scanned, 1);
        assert_eq!(report.rules_checked, 2);
    }

    #[test]
    fn doc_change_is_a_self_healing_non_event_via_reingest() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let root = doc_root("selfheal", &[("doctrine.md", DOC_V1)]);
        reingest(&mut store, &root, 1_000);

        // The doc changes on disk (a merge landed) — drift flags the stale ingest.
        std::fs::write(root.join("doctrine.md"), DOC_V2).unwrap();
        let report = drift(&store, Some(&root), DEFAULT_AMBIGUITY_CAP).unwrap();
        assert!(report.has_residue());
        // BOTH rules are stale — the digest is per-doc, so any doc edit re-verifies every rule
        // minted from it.
        assert_eq!(report.uningested.len(), 2);
        assert!(report
            .uningested
            .iter()
            .all(|u| matches!(u.reason, UningestReason::Stale { .. })));

        // The on-merge re-ingest step heals it — a non-event, no manual reconciliation.
        reingest(&mut store, &root, 2_000);
        let report = drift(&store, Some(&root), DEFAULT_AMBIGUITY_CAP).unwrap();
        assert!(
            !report.has_residue(),
            "re-ingest must fully self-heal a doc change: {report:?}"
        );
    }

    #[test]
    fn deleted_docs_and_dropped_ids_report_orphans_and_are_never_auto_dropped() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let root = doc_root(
            "orphan",
            &[
                ("doctrine.md", DOC_V1),
                (
                    "other.md",
                    "---\nid: o\ntitle: O\n---\n\n## Rules\n\n- PAT-100 (info): keep.\n",
                ),
            ],
        );
        reingest(&mut store, &root, 1_000);

        // The whole doc disappears → BOTH its rules orphan (doc-missing).
        std::fs::remove_file(root.join("doctrine.md")).unwrap();
        let report = drift(&store, Some(&root), DEFAULT_AMBIGUITY_CAP).unwrap();
        assert_eq!(report.orphaned.len(), 2);
        assert!(report
            .orphaned
            .iter()
            .all(|o| o.reason == OrphanReason::DocMissing));
        // The rules are still in the store — drift REPORTS, it never deletes (retirement is an
        // explicit operator action fed by this report).
        assert_eq!(load_conformance_rules(&store).unwrap().len(), 3);

        // A doc that survives but drops one id → id-not-minted for that id only.
        let root2 = doc_root("orphan2", &[("doctrine.md", DOC_V2)]);
        let mut store2 = open_store(Some(":memory:")).unwrap();
        reingest(&mut store2, &root2, 1_000);
        std::fs::write(root2.join("doctrine.md"), DOC_V3).unwrap();
        reingest(&mut store2, &root2, 2_000); // re-ingest heals PAT-001's digest…
        let report = drift(&store2, Some(&root2), DEFAULT_AMBIGUITY_CAP).unwrap();
        assert_eq!(
            report.orphaned,
            vec![OrphanedRule {
                rule_id: "POL-002".into(),
                doc_path: "doctrine.md".into(),
                reason: OrphanReason::IdNotMinted,
            }],
            "…but the dropped id is residue a re-ingest cannot heal: {report:?}"
        );
    }

    #[test]
    fn new_docs_report_uningested_until_ingested() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let root = doc_root("newdoc", &[("doctrine.md", DOC_V1)]);
        let report = drift(&store, Some(&root), DEFAULT_AMBIGUITY_CAP).unwrap();
        assert_eq!(report.uningested.len(), 2);
        assert!(report
            .uningested
            .iter()
            .all(|u| u.reason == UningestReason::NotIngested));

        reingest(&mut store, &root, 1_000);
        assert!(!drift(&store, Some(&root), DEFAULT_AMBIGUITY_CAP)
            .unwrap()
            .has_residue());
    }

    #[test]
    fn legacy_pre_digest_refs_report_stale_not_crash() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let root = doc_root("legacy", &[("doctrine.md", DOC_V1)]);
        // Simulate a rule ingested by the PRE-DIGEST build: same id, ref without `@sha`.
        let legacy = ConformanceRule {
            id: "PAT-001".into(),
            rule_type: RuleType::Pattern,
            statement: "never use printf without %s.".into(),
            severity: ConfSeverity::Error,
            confidence: 1.0,
            targets: Targets::default(),
            symbol_ref: None,
            compliance: None,
            provenance: RuleProvenance {
                source: "markdown".into(),
                reference: Some("doctrine.md#PAT-001".into()),
                source_kinds: vec!["doc".into()],
            },
            retired: false,
        };
        register_rule(&mut store, &legacy).unwrap();

        let report = drift(&store, Some(&root), DEFAULT_AMBIGUITY_CAP).unwrap();
        let stale: Vec<_> = report
            .uningested
            .iter()
            .filter(|u| u.rule_id == "PAT-001")
            .collect();
        assert_eq!(stale.len(), 1);
        assert!(
            matches!(
                &stale[0].reason,
                UningestReason::Stale {
                    recorded_sha: None,
                    ..
                }
            ),
            "a digest-less legacy ref is stale (re-ingest stamps it), never a parse crash"
        );
        // And the legacy rule is NOT orphaned — the doc still mints its id.
        assert!(report.orphaned.is_empty());
    }

    #[test]
    fn unlinked_refs_surface_and_relink_heals_them() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let node = Node::new(
            Symbol::synthetic("scip-sim", "src/a.rs::f::v1").id(),
            NodeKind::Function,
            "f".to_string(),
            Language::new("rust"),
            Location::new("src/a.rs".to_string(), Span::ZERO),
        );
        store.begin_batch().unwrap();
        store.upsert_nodes(&[node]).unwrap();
        store.commit_batch().unwrap();

        let mut rule = ConformanceRule {
            id: "PAT-300".into(),
            rule_type: RuleType::Pattern,
            statement: "s".into(),
            severity: ConfSeverity::Warn,
            confidence: 0.9,
            targets: Targets::default(),
            symbol_ref: Some("src/a.rs::f".into()),
            compliance: None,
            provenance: RuleProvenance {
                source: "markdown".into(),
                reference: Some("docs/x.md#PAT-300".into()),
                source_kinds: vec!["doc".into()],
            },
            retired: false,
        };
        register_rule(&mut store, &rule).unwrap();

        // Registered but never relinked: the ref resolves, the edge is missing → unlinked.
        let report = drift(&store, None, DEFAULT_AMBIGUITY_CAP).unwrap();
        assert_eq!(report.unlinked.len(), 1);
        assert_eq!(report.unlinked[0].rule_id, "PAT-300");
        assert!(report.unresolvable.is_empty());

        // relink heals; drift goes quiet.
        relink(&mut store, DEFAULT_AMBIGUITY_CAP, 1_000).unwrap();
        let report = drift(&store, None, DEFAULT_AMBIGUITY_CAP).unwrap();
        assert!(!report.has_residue(), "{report:?}");

        // An unresolvable ref stays reported (drift's read-only twin of relink's findings).
        rule.symbol_ref = Some("src/gone.rs::vanished".into());
        rule.id = "PAT-301".into();
        register_rule(&mut store, &rule).unwrap();
        let report = drift(&store, None, DEFAULT_AMBIGUITY_CAP).unwrap();
        assert_eq!(report.unresolvable.len(), 1);
        assert!(matches!(
            report.unresolvable[0].failure,
            RefFailure::Unresolved
        ));
    }
}
