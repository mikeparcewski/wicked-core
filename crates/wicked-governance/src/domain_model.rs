//! Domain-model output artifact + the front-half coverage gate (PR-D foundation, DES-OUTGOV-001 §10).
//!
//! The domain-graph builder ports anti-legacy `domain_graph.py`: it translates estate's annotated
//! code graph into a `requirements_graph.json` domain model. These serde types are the OUTPUT wire
//! contract — the builder's output MUST validate against the KEPT `wicked-brain/schemas/
//! domain-model.schema.json` (schema_version **1.0.0**), which garden STEERS on and wicked-testing
//! ASSERTS. Fidelity is proven by `tests/domain_model_schema.rs` validating built output against the
//! real JSON Schema (NOT a self-authored literal) — the review of this PR found that a round-trip
//! literal gives false confidence, so every enum/const/pattern/minItems is now schema-checked.
//!
//! [`assert_front_half_coverage`] is the fail-closed precondition: the builder REFUSES to translate
//! an unannotated graph — the coverage-report MUST show `coverage == 1.0` (every behavior-bearing
//! node accounted for) or the build bails, surfacing the unaccounted SymbolIds (port of
//! `domain_graph.py::assert_front_half_coverage`, §I5).

use serde::{Deserialize, Serialize};
use wicked_apps_core::GraphRead;

/// The primary artifact: `{ metadata, domains }` (schema root).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainModel {
    pub metadata: Metadata,
    /// Capability domains keyed by domain name (a JSON object of `name -> Domain`).
    pub domains: std::collections::BTreeMap<String, Domain>,
}

/// Artifact metadata — `schema_version` + `migration_mode` are required by the schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    /// Schema bundle version — the schema pins this to const `"1.0.0"`.
    pub schema_version: String,
    /// Schema enum: `"functional"` (capability-grouped — what the M5 package-dir grouping emits) or
    /// `"structural"` (1:1). NOT the internal word "modern".
    pub migration_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// A capability domain: its requirements + the entities they operate on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Domain {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The estate Louvain community index this domain derives from (advisory). Schema-typed as an
    /// INTEGER — so modern/functional package-dir grouping (no Louvain) OMITS it; the human package
    /// label rides `description`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_id: Option<i64>,
    /// Requirements keyed by requirement key.
    pub requirements: std::collections::BTreeMap<String, Requirement>,
    /// Entities keyed by entity name.
    pub entities: std::collections::BTreeMap<String, Entity>,
}

/// A requirement. The legacy-only fields (`legacy_components`, `data_access`, `dependencies`,
/// `merged_programs`) are schema-required arrays that modern-code translation leaves EMPTY (never
/// omitted — the wire contract requires the keys present).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Requirement {
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub legacy_components: Vec<String>,
    #[serde(default)]
    pub data_access: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub business_rules: Vec<Rule>,
    #[serde(default)]
    pub validations: Vec<Validation>,
    #[serde(default)]
    pub error_paths: Vec<ErrorPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merged_programs: Vec<String>,
}

/// A business rule (`id`, `statement`, `confidence`, `provenance` required by the schema).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub statement: String,
    pub confidence: f64,
    pub provenance: Provenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
}

/// A field-level validation (`id` + `statement` required).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Validation {
    pub id: String,
    pub statement: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

/// An error path (`id` + `statement` required).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorPath {
    pub id: String,
    pub statement: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

/// A domain entity (`description` + `fields` required).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    pub description: String,
    pub fields: Vec<EntityField>,
}

/// An entity field (`name`, `type`, `description` all required).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityField {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: String,
    pub description: String,
}

/// Extraction provenance (`source`, `ref`, `source_kinds` all required) — the SAME shape the
/// conformance-rules contract uses (RET-BRAIN-DOMAIN-001 noted the two provenance $defs are identical).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    pub source: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub source_kinds: Vec<String>,
}

/// The front-half coverage report the builder gates on (subset — the load-bearing fields).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CoverageReport {
    pub coverage: f64,
    /// The behavior-bearing SymbolIds NOT yet resolved/risk-flagged (the coverage hole).
    #[serde(default)]
    pub unaccounted: Vec<String>,
}

/// Fail-closed front-half precondition (port of `domain_graph.py::assert_front_half_coverage`, §I5):
/// the builder REFUSES to translate an unannotated graph. `coverage` MUST be `1.0` (every
/// behavior-bearing node resolved or risk-flagged); otherwise bail, surfacing the unaccounted
/// SymbolIds so the operator knows exactly what is missing. Never translates a partial graph.
pub fn assert_front_half_coverage(report: &CoverageReport) -> anyhow::Result<()> {
    // Exact 1.0 (floats compared with a tiny epsilon — coverage is a ratio of integer counts, so it
    // is exactly 1.0 when complete, but guard against representation drift).
    if (report.coverage - 1.0).abs() > f64::EPSILON {
        let shown: Vec<&String> = report.unaccounted.iter().take(20).collect();
        anyhow::bail!(
            "front-half coverage {:.4} < 1.0 — {} behavior-bearing node(s) unaccounted; run \
             extraction + coverage first (refusing to translate an unannotated graph). First \
             unaccounted: {:?}",
            report.coverage,
            report.unaccounted.len(),
            shown
        );
    }
    Ok(())
}

/// The `business_rule` annotation type — the estate annotations the builder lifts into
/// [`Requirement::business_rules`].
const BUSINESS_RULE_ANN: &str = "business_rule";
/// A custom `risk` annotation type — a risk-flagged node the coverage gate COUNTS as accounted
/// (numerator = resolved + risk_flagged) but that carries no resolved requirement.
const RISK_ANN: &str = "risk";
/// Confidence stamped on a synthesized fallback rule for a behavior-bearing node that produced no
/// real business rule (the schema's `business_rules` `minItems:1` invariant + `domain_graph.py`'s
/// defensive fallback). Mid-range: it is a review item, neither asserted nor refuted.
const REVIEW_CONFIDENCE: f64 = 0.5;

/// The capability boundary for MODERN code (M5): the PARENT directory of a node's source file — NOT
/// a Louvain community (dense modern code collapses into one near-fully-connected blob). Files at
/// the repo root group under `(root)`. Forward-slash normalized so grouping is cross-platform stable.
fn package_dir(file: &str) -> String {
    let norm = file.replace('\\', "/");
    match norm.rfind('/') {
        Some(i) if i > 0 => norm[..i].to_string(),
        _ => "(root)".to_string(),
    }
}

/// The estate reference a domain fact points at — the node's `file#name` (a REFERENCE, never a copy
/// of the symbol's structure). Used for `provenance.ref`, `source_ref`, and `legacy_components`.
fn node_ref(node: &wicked_apps_core::Node) -> String {
    format!("{}#{}", node.location.file, node.name)
}

/// Provenance for a fact lifted from `node` — grounded in the code body it was read from.
fn node_provenance(node: &wicked_apps_core::Node) -> Provenance {
    Provenance {
        source: "code-graph".to_string(),
        reference: node_ref(node),
        source_kinds: vec!["code-body".to_string()],
    }
}

/// Build the domain model from the annotated estate graph — the port of `domain_graph.py`'s modern
/// path (schema `migration_mode: "functional"` = capability-grouped). Gate on front-half coverage
/// (fail-closed), then group every behavior-bearing node by PACKAGE DIR into a capability domain.
///
/// **Keep-set matches the coverage gate's accounted predicate** (resolved + risk-flagged), so nothing
/// the gate certified is silently dropped: a node counts if it carries a `requirement`/`description`
/// semantic, a `business_rule` annotation, OR a risk-flag (advisory assumption/question, or a `risk`
/// custom type). A risk-only / rule-less node still emits a Requirement — with `status: "review"` and
/// a SYNTHESIZED fallback rule carrying its risk/requirement text — so the schema's
/// `business_rules minItems:1` invariant holds and the HITL-queued behavior survives into the
/// artifact. Confidence is NEVER fabricated or clamped: an out-of-`[0,1]` annotation confidence fails
/// CLOSED. Deterministic: nodes are visited in `SymbolId` order; requirement ids are per-domain
/// sequential `REQ-NNN` (unique — no bare-name collisions).
pub fn build_domain_model(
    store: &dyn GraphRead,
    coverage: &CoverageReport,
    schema_version: &str,
) -> anyhow::Result<DomainModel> {
    assert_front_half_coverage(coverage)?;

    let mut domains: std::collections::BTreeMap<String, Domain> = std::collections::BTreeMap::new();
    let mut nodes = store.all_nodes()?;
    nodes.sort_by(|a, b| a.symbol.cmp(&b.symbol)); // stable, UNIQUE visitation order

    for node in &nodes {
        let semantics = store.node_semantics(&node.symbol)?;
        let requirement_text = semantics.as_ref().and_then(|s| s.requirement.clone());
        let description_text = semantics.as_ref().and_then(|s| s.description.clone());
        let annotations = store.annotations(&node.symbol)?;

        // Real business rules — the annotation's own confidence rides through (fail-closed if out of range).
        let mut business_rules: Vec<Rule> = Vec::new();
        for a in annotations.iter().filter(|a| a.r#type == BUSINESS_RULE_ANN) {
            if !(0.0..=1.0).contains(&a.confidence) {
                anyhow::bail!(
                    "node {}: business_rule confidence {} out of [0,1] — never fabricated or clamped",
                    node.name,
                    a.confidence
                );
            }
            business_rules.push(Rule {
                id: String::new(), // sequential id assigned below
                statement: a.value.clone(),
                confidence: a.confidence,
                provenance: node_provenance(node),
                source_ref: Some(node_ref(node)),
            });
        }

        // Risk-flagged = advisory (assumption/question) or a `risk` custom type — the coverage gate
        // counts these as accounted, so the builder MUST keep them (not drop, not silently lose).
        let risk_notes: Vec<&_> = annotations
            .iter()
            .filter(|a| a.is_advisory() || a.r#type == RISK_ANN)
            .collect();

        let resolved = requirement_text.is_some() || !business_rules.is_empty();
        // Genuinely structural (nothing accounted) → skip. Everything the gate counted survives.
        if !resolved && risk_notes.is_empty() && description_text.is_none() {
            continue;
        }

        // minItems:1 — synthesize ONE fallback rule when there is no real business rule (carries the
        // risk statement if risk-flagged, else the requirement/description text; `domain_graph.py`'s
        // defensive fallback so rule_objects is never empty).
        if business_rules.is_empty() {
            let (statement, confidence) = match risk_notes.first() {
                Some(risk) => {
                    if !(0.0..=1.0).contains(&risk.confidence) {
                        anyhow::bail!(
                            "node {}: risk annotation confidence {} out of [0,1]",
                            node.name,
                            risk.confidence
                        );
                    }
                    (risk.value.clone(), risk.confidence)
                }
                None => (
                    requirement_text
                        .clone()
                        .or_else(|| description_text.clone())
                        .unwrap_or_default(),
                    REVIEW_CONFIDENCE,
                ),
            };
            business_rules.push(Rule {
                id: String::new(),
                statement: if statement.is_empty() {
                    "RISK".to_string()
                } else {
                    statement
                },
                confidence,
                provenance: node_provenance(node),
                source_ref: Some(node_ref(node)),
            });
        }

        // Assign schema-valid, digits-only, per-requirement-unique ids (`^RULE-[0-9]{3,6}$`).
        for (i, rule) in business_rules.iter_mut().enumerate() {
            rule.id = format!("RULE-{:03}", i + 1);
        }

        // status ∈ {active, review, unresolvable}: validated-requirement → active; otherwise (a
        // risk-flagged / unvalidated / description-only node) → review (HITL-queued).
        let status = if semantics.as_ref().is_some_and(|s| s.requirement_validated) {
            "active"
        } else {
            "review"
        };

        let package = package_dir(&node.location.file);
        let domain = domains.entry(package.clone()).or_insert_with(|| Domain {
            // functional/package-dir mode has no Louvain integer → omit cluster_id; label rides description.
            description: Some(package.clone()),
            cluster_id: None,
            ..Default::default()
        });
        let requirement = Requirement {
            title: node.name.clone(),
            description: requirement_text
                .or(description_text)
                .unwrap_or_else(|| node.name.clone()),
            legacy_components: vec![node_ref(node)], // the estate symbol this requirement covers
            status: Some(status.to_string()),
            business_rules,
            ..Default::default()
        };
        // Per-domain sequential requirement id — unique within the domain, deterministic.
        let req_id = format!("REQ-{:03}", domain.requirements.len() + 1);
        domain.requirements.insert(req_id, requirement);
    }

    Ok(DomainModel {
        metadata: Metadata {
            schema_version: schema_version.to_string(),
            // M5 package-dir grouping IS the schema's "functional" (capability-grouped) mode.
            migration_mode: "functional".to_string(),
            source: Some("estate-graph".to_string()),
        },
        domains,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_model_round_trips_in_the_wire_shape() {
        // A minimal FUNCTIONAL-mode model, schema-VALID: metadata const 1.0.0 + enum migration_mode;
        // rule id ^RULE-[0-9]{3,6}$; status enum; business_rules minItems 1; provenance complete.
        let json = serde_json::json!({
            "metadata": { "schema_version": "1.0.0", "migration_mode": "functional" },
            "domains": {
                "billing": {
                    "description": "billing",
                    "requirements": {
                        "REQ-001": {
                            "title": "charge",
                            "description": "process a payment charge",
                            "legacy_components": ["billing/charge.py#charge"],
                            "data_access": [],
                            "dependencies": [],
                            "business_rules": [
                                { "id": "RULE-001", "statement": "amount must be positive",
                                  "confidence": 0.9,
                                  "provenance": { "source": "code-graph", "ref": "billing/charge.py#charge",
                                                  "source_kinds": ["code-body"] } }
                            ],
                            "validations": [],
                            "error_paths": [],
                            "status": "active"
                        }
                    },
                    "entities": {
                        "Charge": { "description": "a payment charge",
                                    "fields": [ { "name": "amount", "type": "int", "description": "cents" } ] }
                    }
                }
            }
        });
        let model: DomainModel = serde_json::from_value(json.clone()).expect("parse wire shape");
        assert_eq!(model.metadata.migration_mode, "functional");
        let req = model.domains["billing"]
            .requirements
            .get("REQ-001")
            .unwrap();
        assert_eq!(req.business_rules[0].confidence, 0.9);
        // Round-trips back to an equal JSON value (serde symmetry; schema conformance is proven by
        // the integration test in tests/domain_model_schema.rs which validates against the schema).
        assert_eq!(serde_json::to_value(&model).unwrap(), json);
    }

    #[test]
    fn coverage_gate_fails_closed_below_one() {
        let partial = CoverageReport {
            coverage: 0.33,
            unaccounted: vec!["sym:refund".into(), "sym:audit".into()],
        };
        let err = assert_front_half_coverage(&partial)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("< 1.0") && err.contains("refund"),
            "got: {err}"
        );

        let complete = CoverageReport {
            coverage: 1.0,
            unaccounted: vec![],
        };
        assert!(
            assert_front_half_coverage(&complete).is_ok(),
            "coverage 1.0 translates"
        );
    }

    #[test]
    fn build_domain_model_groups_behavior_by_package_dir() {
        use wicked_apps_core::{
            synthetic_symbol, GraphWrite, Language, Location, Node, NodeKind, Span, SqliteStore,
            SYMBOL_SCHEME,
        };
        use wicked_estate_core::Annotation;

        let mut store = SqliteStore::in_memory().unwrap();
        let mk = |name: &str, file: &str| {
            Node::new(
                synthetic_symbol("code", name),
                NodeKind::Function,
                name.to_string(),
                Language::new(SYMBOL_SCHEME),
                Location::new(file.to_string(), Span::ZERO),
            )
        };
        let charge = mk("charge", "billing/charge.py");
        let login = mk("login", "auth/login.py");
        // A RISK-flagged node: no requirement, no business rule — but the coverage gate COUNTS it as
        // accounted (risk_flagged), so the builder must KEEP it (regression for the silent-drop bug).
        let refund = mk("refund", "billing/refund.py");
        let scaffold = mk("helper", "billing/util.py"); // truly structural → skipped
        store.begin_batch().unwrap();
        store
            .upsert_nodes(&[
                charge.clone(),
                login.clone(),
                refund.clone(),
                scaffold.clone(),
            ])
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
                Annotation::new("business_rule", "r1", "amount must be positive")
                    .with_confidence(0.9),
            )
            .unwrap();
        store
            .annotate(
                &login.symbol,
                Annotation::new("business_rule", "r1", "rate-limit failed logins")
                    .with_confidence(0.7),
            )
            .unwrap();
        store
            .annotate(
                &refund.symbol,
                Annotation::new(
                    "risk",
                    "r1",
                    "unclear whether partial refunds are allowed — HITL",
                )
                .with_confidence(0.4),
            )
            .unwrap();

        let coverage = CoverageReport {
            coverage: 1.0,
            unaccounted: vec![],
        };
        let model = build_domain_model(&store, &coverage, "1.0.0").unwrap();

        assert_eq!(model.metadata.migration_mode, "functional");
        // Two package-dir domains (auth, billing); scaffold contributed nothing.
        assert_eq!(
            model.domains.keys().collect::<Vec<_>>(),
            vec!["auth", "billing"]
        );
        let billing = &model.domains["billing"];
        assert_eq!(billing.description.as_deref(), Some("billing"));
        assert!(
            billing.cluster_id.is_none(),
            "functional mode omits the Louvain integer"
        );
        // charge (real rule) + refund (risk-flagged, KEPT) — the silent-drop regression.
        assert_eq!(
            billing.requirements.len(),
            2,
            "charge + risk-flagged refund both survive"
        );

        // charge → REQ-001 (visited in SymbolId order): validated requirement, real rule.
        let charge_req = billing
            .requirements
            .values()
            .find(|r| r.title == "charge")
            .unwrap();
        assert_eq!(charge_req.status.as_deref(), Some("active"));
        assert_eq!(charge_req.business_rules[0].id, "RULE-001");
        assert_eq!(charge_req.business_rules[0].confidence, 0.9);

        // refund → risk-flagged: KEPT as a review requirement with a SYNTHESIZED fallback rule
        // carrying the risk statement + confidence (minItems:1 held, behavior not lost).
        let refund_req = billing
            .requirements
            .values()
            .find(|r| r.title == "refund")
            .unwrap();
        assert_eq!(refund_req.status.as_deref(), Some("review"));
        assert_eq!(
            refund_req.business_rules.len(),
            1,
            "synthesized fallback rule"
        );
        assert_eq!(refund_req.business_rules[0].confidence, 0.4);
        assert!(refund_req.business_rules[0]
            .statement
            .contains("partial refunds"));
    }

    #[test]
    fn build_fails_closed_on_out_of_range_confidence() {
        use wicked_apps_core::{
            synthetic_symbol, GraphWrite, Language, Location, Node, NodeKind, Span, SqliteStore,
            SYMBOL_SCHEME,
        };
        use wicked_estate_core::Annotation;
        let mut store = SqliteStore::in_memory().unwrap();
        let n = Node::new(
            synthetic_symbol("code", "bad"),
            NodeKind::Function,
            "bad".to_string(),
            Language::new(SYMBOL_SCHEME),
            Location::new("m/bad.py".to_string(), Span::ZERO),
        );
        store.begin_batch().unwrap();
        store.upsert_nodes(&[n.clone()]).unwrap();
        store.commit_batch().unwrap();
        store
            .annotate(
                &n.symbol,
                Annotation::new("business_rule", "r", "x").with_confidence(1.5),
            )
            .unwrap();
        let coverage = CoverageReport {
            coverage: 1.0,
            unaccounted: vec![],
        };
        let err = build_domain_model(&store, &coverage, "1.0.0")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("[0,1]"),
            "out-of-range confidence must fail closed: {err}"
        );
    }
}
