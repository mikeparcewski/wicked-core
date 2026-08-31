//! Relink pass — rule→code links durable-by-name, derived-by-id (AW-9 / arch-R6).
//!
//! Rule→code linking is fragile-by-design if it is stored as edges alone: a full re-extract
//! deletes and re-mints code nodes, and any `Governs` edge pointing at a superseded id dangles and
//! is pruned. The durable key is therefore the **qualified `symbol_ref` name** in the rule's
//! metadata; the edge is a DERIVED projection this pass re-creates after every `wicked-estate
//! index` (and as a crew workflow step). Links survive re-index by RE-DERIVATION, never by edge
//! persistence.
//!
//! ## Qualified refs only
//!
//! Bare names are NOT unique under scheme-3 type-nested ids, so a `symbol_ref` must be scoped:
//!
//! - **path-scoped** — `<repo-relative path>::<name>` (the prefix before the first `::` contains
//!   `/` or `.`), matched against `node.location.file` (forward slashes on every platform);
//! - **kind-scoped** — `<kind>:<name>` (single colon; `<kind>` is a native estate `NodeKind` token
//!   such as `function`, `struct`, `trait`, `rule`), matched against `node.kind`.
//!
//! Anything else — a bare name, an unknown kind, a `Type::method` shorthand — is REFUSED and
//! reported as a drift finding. Refusing beats guessing: a bare name silently fanning to the wrong
//! symbol is a mis-aimed guardrail.
//!
//! ## Ambiguity cap
//!
//! A ref resolving to more than [`DEFAULT_AMBIGUITY_CAP`] symbols (configurable) is refused and
//! reported — never fanned. Within the cap, ALL matches are linked (a rule may legitimately govern
//! the handful of same-named platform variants of one function).
//!
//! ## Per repo, at current epoch
//!
//! Estate graphs are per-repo; this pass resolves within the ONE store it is pointed at
//! (co-location only — edges never resolve across repos; a ref into another repo reports as
//! unresolved drift HERE and links when relink runs THERE). Each resolved target is stamped with
//! its live `symbol_epoch`, so a later reuse-after-delete of the same id is detectable.
//!
//! Unresolvable refs are reported as [`RelinkFinding`]s and the rule's `symbol_ref` metadata is
//! NEVER modified or dropped — the name outlives any number of failed resolutions.

use serde::Serialize;
use wicked_apps_core::{
    is_apps_synthetic_symbol, synthetic_symbol, Edge, FromNode, GraphRead, GraphStore, Node,
    NodeKind,
};
use wicked_estate_core::{Annotation, SymbolQuery};

use crate::conformance::{ConformanceRule, CONFORMANCE_RULE};

/// Refuse to fan a ref resolving to more than this many symbols (arch-R6's ambiguity cap).
pub const DEFAULT_AMBIGUITY_CAP: usize = 5;
/// Annotation `type` for the relink freshness stamp on a linked rule node.
pub const RELINK_ANNOTATION_TYPE: &str = "relink";
/// Annotation `key` for the relink freshness stamp.
pub const RELINK_ANNOTATION_KEY: &str = "symbol_ref";

/// The scope qualifier of a parsed `symbol_ref`.
#[derive(Debug, Clone, PartialEq)]
pub enum RefScope {
    /// `<repo-relative path>::<name>` — match on `node.location.file`.
    Path(String),
    /// `<kind>:<name>` — match on `node.kind`.
    Kind(NodeKind),
}

/// A successfully parsed, qualified `symbol_ref`.
#[derive(Debug, Clone, PartialEq)]
pub struct QualifiedRef {
    pub raw: String,
    pub scope: RefScope,
    pub name: String,
}

/// Parse a qualified `symbol_ref`. `Err` carries the human reason (it becomes a drift finding —
/// an unparseable ref surfaces, never resolves by guesswork).
pub fn parse_symbol_ref(raw: &str) -> Result<QualifiedRef, String> {
    let raw_trim = raw.trim();
    if let Some((prefix, name)) = raw_trim.split_once("::") {
        let path = prefix.trim().replace('\\', "/");
        let name = name.trim();
        if (path.contains('/') || path.contains('.')) && !path.is_empty() && !name.is_empty() {
            return Ok(QualifiedRef {
                raw: raw.to_string(),
                scope: RefScope::Path(path),
                name: name.to_string(),
            });
        }
        return Err(format!(
            "`{raw_trim}` is not path-scoped (the prefix before `::` must be a repo-relative \
             path containing `/` or `.`) — bare `Type::member` shorthands are not unique under \
             scheme-3 ids; qualify as `<path>::<name>` or `<kind>:<name>`"
        ));
    }
    if let Some((kind_tok, name)) = raw_trim.split_once(':') {
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("`{raw_trim}` has an empty symbol name after `:`"));
        }
        // NodeKind serializes snake_case; a bare string parses back to the native variant.
        // `Other(..)` is a JSON object, so an unknown token FAILS here — exactly what we want.
        match serde_json::from_value::<NodeKind>(serde_json::Value::String(kind_tok.to_string())) {
            Ok(kind) => {
                return Ok(QualifiedRef {
                    raw: raw.to_string(),
                    scope: RefScope::Kind(kind),
                    name: name.to_string(),
                })
            }
            Err(_) => {
                return Err(format!(
                    "`{raw_trim}` uses unknown kind token `{kind_tok}` — use a native estate \
                     NodeKind (e.g. function, method, struct, class, trait, rule) or a \
                     path-scoped `<path>::<name>` ref"
                ))
            }
        }
    }
    Err(format!(
        "`{raw_trim}` is a bare name — bare names are not unique under scheme-3 type-nested ids; \
         qualify as `<repo-relative path>::<name>` or `<kind>:<name>`"
    ))
}

/// Why a ref did not link. Serialized into the relink/drift reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RefFailure {
    /// The ref is not a qualified form this pass accepts.
    Unqualified { reason: String },
    /// The ref parsed but matched no live symbol in this store at the current epoch.
    Unresolved,
    /// The ref matched more symbols than the cap — refused, never fanned.
    Ambiguous {
        matches: usize,
        cap: usize,
        /// Up to `cap` candidate symbol ids, for the operator to disambiguate against.
        sample: Vec<String>,
    },
}

/// One rule whose `symbol_ref` did not produce edges — DRIFT, reported, never dropped.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RelinkFinding {
    pub rule_id: String,
    pub symbol_ref: String,
    pub failure: RefFailure,
}

/// One resolved target of a linked rule, with its epoch at link time.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LinkedTarget {
    pub symbol: String,
    /// The target's live `symbol_epoch` at link time (`None` = the store reports no live epoch,
    /// which for a just-resolved node means a backend without epoch tracking).
    pub epoch: Option<u64>,
}

/// One rule that linked.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LinkedRule {
    pub rule_id: String,
    pub symbol_ref: String,
    /// The doc path parsed from the rule's provenance ref (feeds the knowledge half).
    pub doc_path: Option<String>,
    pub targets: Vec<LinkedTarget>,
}

/// The relink pass output: what linked, what drifted, what was skipped.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct RelinkReport {
    pub linked: Vec<LinkedRule>,
    pub edges_written: usize,
    /// Unresolvable refs — reported as drift, NEVER dropped from the rule metadata.
    pub drift: Vec<RelinkFinding>,
    pub rules_seen: usize,
    pub rules_with_ref: usize,
    pub skipped_retired: usize,
}

/// Load every conformance rule from the store (the same synthetic-symbol round-trip filter recall
/// uses, so foreign `NodeKind::Rule` nodes — estate's W15 rules engine — are never touched).
pub(crate) fn load_conformance_rules(
    store: &dyn GraphRead,
) -> anyhow::Result<Vec<ConformanceRule>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Rule],
        ..Default::default()
    };
    let mut rules = Vec::new();
    for node in store.find_symbols(&query)? {
        if node.symbol != synthetic_symbol(CONFORMANCE_RULE, &node.name) {
            continue;
        }
        rules.push(ConformanceRule::from_node(&node)?);
    }
    Ok(rules)
}

/// Resolve a qualified ref against the store at the current epoch. Returns the matching REAL
/// symbols — wicked-apps synthetic nodes (rules, policies, claims, domain artifacts) are excluded,
/// so a rule can never end up governing another governance artifact through a name collision.
pub(crate) fn resolve_ref(store: &dyn GraphRead, qref: &QualifiedRef) -> anyhow::Result<Vec<Node>> {
    let query = SymbolQuery {
        exact_name: Some(qref.name.clone()),
        kinds: match &qref.scope {
            RefScope::Kind(k) => vec![k.clone()],
            RefScope::Path(_) => vec![],
        },
        ..Default::default()
    };
    let mut matches = Vec::new();
    for node in store.find_symbols(&query)? {
        if is_apps_synthetic_symbol(&node.symbol) {
            continue;
        }
        if let RefScope::Path(path) = &qref.scope {
            if node.location.file.replace('\\', "/") != *path {
                continue;
            }
        }
        matches.push(node);
    }
    Ok(matches)
}

/// Run the relink pass over `store`: for every ACTIVE conformance rule with a `symbol_ref`,
/// resolve the qualified name at the current epoch and re-derive its native `Governs` edges
/// ([`ConformanceRule::governs_edge`] — the edge carries the rule's own confidence). Emission is
/// idempotent (estate's `upsert_edges` dedups on the edge key), and every failure is a reported
/// [`RelinkFinding`] — a ref is never silently dropped.
///
/// Each linked rule also gets ONE `relink/symbol_ref` freshness annotation (delete-then-insert):
/// value = the resolved targets + epochs as JSON, `last_verified = now` — the witness that this
/// link was derived at the current epoch, surfaced through `annotations_stale_since`.
pub fn relink(
    store: &mut dyn GraphStore,
    ambiguity_cap: usize,
    now: i64,
) -> anyhow::Result<RelinkReport> {
    let rules = load_conformance_rules(store)?;
    let mut report = RelinkReport {
        rules_seen: rules.len(),
        ..Default::default()
    };

    let mut edges: Vec<Edge> = Vec::new();
    let mut stamps: Vec<(String, String, serde_json::Value)> = Vec::new();

    for rule in &rules {
        let Some(symbol_ref) = rule.symbol_ref.as_deref() else {
            continue;
        };
        if rule.retired {
            report.skipped_retired += 1;
            continue;
        }
        report.rules_with_ref += 1;

        let qref = match parse_symbol_ref(symbol_ref) {
            Ok(q) => q,
            Err(reason) => {
                report.drift.push(RelinkFinding {
                    rule_id: rule.id.clone(),
                    symbol_ref: symbol_ref.to_string(),
                    failure: RefFailure::Unqualified { reason },
                });
                continue;
            }
        };
        let matches = resolve_ref(store, &qref)?;
        if matches.is_empty() {
            report.drift.push(RelinkFinding {
                rule_id: rule.id.clone(),
                symbol_ref: symbol_ref.to_string(),
                failure: RefFailure::Unresolved,
            });
            continue;
        }
        if matches.len() > ambiguity_cap {
            report.drift.push(RelinkFinding {
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

        let mut targets = Vec::with_capacity(matches.len());
        for node in &matches {
            let epoch = store.symbol_epoch(&node.symbol)?;
            edges.push(rule.governs_edge(node.symbol.clone()));
            targets.push(LinkedTarget {
                symbol: node.symbol.as_str().to_string(),
                epoch,
            });
        }
        stamps.push((
            rule.id.clone(),
            symbol_ref.to_string(),
            serde_json::json!({
                "ref": symbol_ref,
                "targets": targets,
                "linked_at": now,
            }),
        ));
        report.linked.push(LinkedRule {
            rule_id: rule.id.clone(),
            symbol_ref: symbol_ref.to_string(),
            doc_path: rule
                .provenance
                .reference
                .as_deref()
                .map(|r| crate::provenance::parse_provenance_ref(r).path),
            targets,
        });
    }

    if !edges.is_empty() {
        store.begin_batch()?;
        store.upsert_edges(&edges)?;
        store.commit_batch()?;
        report.edges_written = edges.len();
    }

    // Freshness stamps AFTER the edge batch (annotate is not batch-scoped): one row per rule,
    // delete-then-insert, so re-running relink never accumulates.
    for (rule_id, _symbol_ref, value) in stamps {
        let symbol = synthetic_symbol(CONFORMANCE_RULE, &rule_id);
        store.delete_annotations(&symbol, Some(RELINK_ANNOTATION_TYPE), RELINK_ANNOTATION_KEY)?;
        store.annotate(
            &symbol,
            Annotation::new(
                RELINK_ANNOTATION_TYPE,
                RELINK_ANNOTATION_KEY,
                value.to_string(),
            )
            .with_provenance("wicked-core rules relink")
            .with_author("wicked-core rules relink")
            .with_source_type("code")
            .with_extraction_method(crate::provenance::EXTRACTION_METHOD)
            .with_last_verified(now),
        )?;
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{ConfSeverity, RuleProvenance, RuleType, Targets};
    use crate::register_rule;
    use wicked_apps_core::{open_store, EdgeKind, GraphWrite, Language, Location, Span, Symbol};
    use wicked_estate_core::Direction;

    fn code_node(file: &str, name: &str, id_salt: &str, kind: NodeKind) -> Node {
        // A distinct, stable synthetic id per (file, name, salt) — stands in for a scheme-3 id.
        let symbol = Symbol::synthetic("scip-sim", format!("{file}::{name}::{id_salt}")).id();
        Node::new(
            symbol,
            kind,
            name.to_string(),
            Language::new("rust"),
            Location::new(file.to_string(), Span::ZERO),
        )
    }

    fn rule_with_symbol_ref(id: &str, symbol_ref: &str) -> ConformanceRule {
        ConformanceRule {
            id: id.to_string(),
            rule_type: if id.starts_with("POL-") {
                RuleType::Policy
            } else {
                RuleType::Pattern
            },
            statement: format!("statement for {id}"),
            severity: ConfSeverity::Error,
            confidence: 0.72,
            targets: Targets::default(),
            symbol_ref: Some(symbol_ref.to_string()),
            compliance: None,
            provenance: RuleProvenance {
                source: "markdown".into(),
                reference: Some(format!("docs/rules.md#{id}")),
                source_kinds: vec!["doc".into()],
            },
            retired: false,
            ..Default::default()
        }
    }

    fn upsert(store: &mut dyn GraphStore, nodes: &[Node]) {
        store.begin_batch().unwrap();
        store.upsert_nodes(nodes).unwrap();
        store.commit_batch().unwrap();
    }

    fn governs_targets(store: &dyn GraphRead, rule_id: &str) -> Vec<String> {
        let sym = synthetic_symbol(CONFORMANCE_RULE, rule_id);
        let mut targets: Vec<String> = store
            .neighbors(&sym, Direction::Dependencies)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == EdgeKind::Governs)
            .map(|e| e.target.as_str().to_string())
            .collect();
        targets.sort();
        targets
    }

    // ── parsing ──────────────────────────────────────────────────────────────

    #[test]
    fn qualified_forms_parse_and_unqualified_forms_are_refused() {
        // Path-scoped.
        let q = parse_symbol_ref("src/billing/charge.rs::charge").unwrap();
        assert_eq!(q.scope, RefScope::Path("src/billing/charge.rs".into()));
        assert_eq!(q.name, "charge");
        // Windows separators normalize.
        let q = parse_symbol_ref("src\\billing\\charge.rs::charge").unwrap();
        assert_eq!(q.scope, RefScope::Path("src/billing/charge.rs".into()));
        // Kind-scoped, native NodeKind token.
        let q = parse_symbol_ref("function:charge").unwrap();
        assert_eq!(q.scope, RefScope::Kind(NodeKind::Function));
        let q = parse_symbol_ref("type_alias:Money").unwrap();
        assert_eq!(q.scope, RefScope::Kind(NodeKind::TypeAlias));

        // Bare name — refused (not unique under scheme-3 ids).
        assert!(parse_symbol_ref("charge")
            .unwrap_err()
            .contains("bare name"));
        // `Type::method` shorthand — refused (prefix is not a path).
        assert!(parse_symbol_ref("Billing::charge")
            .unwrap_err()
            .contains("not path-scoped"));
        // Unknown kind token — refused, never guessed.
        assert!(parse_symbol_ref("banana:charge")
            .unwrap_err()
            .contains("unknown kind token"));
        // Empty name after the qualifier.
        assert!(parse_symbol_ref("function:").is_err());
    }

    // ── linking ──────────────────────────────────────────────────────────────

    #[test]
    fn relink_emits_native_governs_edges_for_path_and_kind_scoped_refs() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let charge = code_node("src/billing.rs", "charge", "v1", NodeKind::Function);
        let money = code_node("src/money.rs", "Money", "v1", NodeKind::Struct);
        upsert(&mut store, &[charge.clone(), money.clone()]);

        register_rule(
            &mut store,
            &rule_with_symbol_ref("PAT-001", "src/billing.rs::charge"),
        )
        .unwrap();
        register_rule(&mut store, &rule_with_symbol_ref("POL-002", "struct:Money")).unwrap();

        let report = relink(&mut store, DEFAULT_AMBIGUITY_CAP, 1_000).unwrap();
        assert_eq!(report.edges_written, 2);
        assert_eq!(report.linked.len(), 2);
        assert!(report.drift.is_empty());

        // Native Governs edges, targeting the REAL indexed symbols, carrying rule confidence.
        assert_eq!(
            governs_targets(&store, "PAT-001"),
            vec![charge.symbol.as_str().to_string()]
        );
        assert_eq!(
            governs_targets(&store, "POL-002"),
            vec![money.symbol.as_str().to_string()]
        );
        let edge = store
            .neighbors(
                &synthetic_symbol(CONFORMANCE_RULE, "PAT-001"),
                Direction::Dependencies,
            )
            .unwrap()
            .into_iter()
            .find(|e| e.kind == EdgeKind::Governs)
            .unwrap();
        assert_eq!(edge.confidence.get(), 0.72);

        // The freshness stamp: exactly one relink annotation, at the current epoch.
        let rows: Vec<_> = store
            .annotations(&synthetic_symbol(CONFORMANCE_RULE, "PAT-001"))
            .unwrap()
            .into_iter()
            .filter(|a| a.r#type == RELINK_ANNOTATION_TYPE)
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].last_verified, 1_000);
        let value: serde_json::Value = serde_json::from_str(&rows[0].value).unwrap();
        assert_eq!(value["targets"][0]["symbol"], charge.symbol.as_str());
    }

    #[test]
    fn relink_is_idempotent() {
        let mut store = open_store(Some(":memory:")).unwrap();
        upsert(
            &mut store,
            &[code_node("src/a.rs", "f", "v1", NodeKind::Function)],
        );
        register_rule(&mut store, &rule_with_symbol_ref("PAT-010", "src/a.rs::f")).unwrap();

        relink(&mut store, DEFAULT_AMBIGUITY_CAP, 1_000).unwrap();
        relink(&mut store, DEFAULT_AMBIGUITY_CAP, 2_000).unwrap();

        assert_eq!(
            governs_targets(&store, "PAT-010").len(),
            1,
            "re-running relink must not duplicate edges (upsert dedups on the edge key)"
        );
        let stamps: Vec<_> = store
            .annotations(&synthetic_symbol(CONFORMANCE_RULE, "PAT-010"))
            .unwrap()
            .into_iter()
            .filter(|a| a.r#type == RELINK_ANNOTATION_TYPE)
            .collect();
        assert_eq!(stamps.len(), 1, "one freshness row, refreshed per run");
        assert_eq!(stamps[0].last_verified, 2_000);
    }

    #[test]
    fn unresolved_and_unqualified_refs_surface_as_drift_and_are_never_dropped() {
        let mut store = open_store(Some(":memory:")).unwrap();
        register_rule(
            &mut store,
            &rule_with_symbol_ref("PAT-020", "src/gone.rs::vanished"),
        )
        .unwrap();
        register_rule(&mut store, &rule_with_symbol_ref("PAT-021", "vanished")).unwrap();

        let report = relink(&mut store, DEFAULT_AMBIGUITY_CAP, 1_000).unwrap();
        assert_eq!(report.edges_written, 0);
        assert_eq!(report.drift.len(), 2);
        assert!(matches!(
            report
                .drift
                .iter()
                .find(|f| f.rule_id == "PAT-020")
                .unwrap()
                .failure,
            RefFailure::Unresolved
        ));
        assert!(matches!(
            report
                .drift
                .iter()
                .find(|f| f.rule_id == "PAT-021")
                .unwrap()
                .failure,
            RefFailure::Unqualified { .. }
        ));

        // The symbol_ref metadata is untouched — the durable name outlives failed resolutions.
        let rules = load_conformance_rules(&store).unwrap();
        assert_eq!(
            rules
                .iter()
                .find(|r| r.id == "PAT-020")
                .unwrap()
                .symbol_ref
                .as_deref(),
            Some("src/gone.rs::vanished")
        );
    }

    #[test]
    fn ambiguity_over_the_cap_refuses_to_fan_and_reports_instead() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let cap = 2;
        // Three same-named functions in different files — over a cap of 2.
        upsert(
            &mut store,
            &[
                code_node("src/a.rs", "handle", "v1", NodeKind::Function),
                code_node("src/b.rs", "handle", "v1", NodeKind::Function),
                code_node("src/c.rs", "handle", "v1", NodeKind::Function),
            ],
        );
        register_rule(
            &mut store,
            &rule_with_symbol_ref("PAT-030", "function:handle"),
        )
        .unwrap();

        let report = relink(&mut store, cap, 1_000).unwrap();
        assert_eq!(report.edges_written, 0, "over-cap must not fan");
        let finding = &report.drift[0];
        match &finding.failure {
            RefFailure::Ambiguous {
                matches,
                cap: c,
                sample,
            } => {
                assert_eq!(*matches, 3);
                assert_eq!(*c, cap);
                assert_eq!(sample.len(), cap, "sample is capped for the operator");
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }

        // At-or-under the cap, the same ref fans to ALL matches.
        let report = relink(&mut store, 3, 2_000).unwrap();
        assert_eq!(report.edges_written, 3);
        assert_eq!(governs_targets(&store, "PAT-030").len(), 3);
    }

    #[test]
    fn retired_rules_and_governance_artifacts_are_never_linked() {
        let mut store = open_store(Some(":memory:")).unwrap();
        upsert(
            &mut store,
            &[code_node("src/a.rs", "f", "v1", NodeKind::Function)],
        );
        let mut retired = rule_with_symbol_ref("PAT-040", "src/a.rs::f");
        retired.retired = true;
        register_rule(&mut store, &retired).unwrap();
        // A rule whose ref names ANOTHER RULE by its node name: PAT-041's name is apps-synthetic,
        // so even a kind-scoped `rule:` ref must not bind to it.
        register_rule(&mut store, &rule_with_symbol_ref("PAT-041", "rule:PAT-040")).unwrap();

        let report = relink(&mut store, DEFAULT_AMBIGUITY_CAP, 1_000).unwrap();
        assert_eq!(report.skipped_retired, 1);
        assert_eq!(report.edges_written, 0);
        assert!(
            matches!(
                report
                    .drift
                    .iter()
                    .find(|f| f.rule_id == "PAT-041")
                    .unwrap()
                    .failure,
                RefFailure::Unresolved
            ),
            "a governance artifact is not a REAL symbol — the ref reports unresolved"
        );
    }

    // ── the acceptance criterion: links survive re-index by re-derivation ────

    #[test]
    fn links_survive_a_full_re_extract_by_re_derivation() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let old = code_node("src/billing.rs", "charge", "old-id", NodeKind::Function);
        let old_symbol = old.symbol.clone();
        upsert(&mut store, &[old]);
        register_rule(
            &mut store,
            &rule_with_symbol_ref("PAT-050", "src/billing.rs::charge"),
        )
        .unwrap();

        // Index → relink: the edge exists against the old id.
        relink(&mut store, DEFAULT_AMBIGUITY_CAP, 1_000).unwrap();
        assert_eq!(
            governs_targets(&store, "PAT-050"),
            vec![old_symbol.as_str().to_string()]
        );

        // A FULL re-extract: the file's contributions are removed and the symbol is re-minted
        // under a DIFFERENT id (the 0.15 id-scheme scenario), then dangling edges are pruned —
        // exactly what kills a stored-edge-only link.
        store.remove_file("src/billing.rs").unwrap();
        let new = code_node("src/billing.rs", "charge", "new-id", NodeKind::Function);
        let new_symbol = new.symbol.clone();
        upsert(&mut store, &[new]);
        store.prune_dangling_edges().unwrap();
        assert!(
            governs_targets(&store, "PAT-050").is_empty(),
            "precondition: the persisted edge did NOT survive the re-extract"
        );

        // Re-derivation: relink after the index restores the link — onto the NEW id.
        let report = relink(&mut store, DEFAULT_AMBIGUITY_CAP, 2_000).unwrap();
        assert_eq!(report.edges_written, 1);
        assert!(report.drift.is_empty());
        assert_eq!(
            governs_targets(&store, "PAT-050"),
            vec![new_symbol.as_str().to_string()],
            "the durable-by-name ref re-derives the edge onto the re-minted symbol"
        );
        assert_ne!(old_symbol, new_symbol, "the ids really did change");
    }
}
