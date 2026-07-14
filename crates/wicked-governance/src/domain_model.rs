//! Domain-model output artifact + the front-half coverage gate (PR-D foundation, DES-OUTGOV-001 §10).
//!
//! The domain-graph builder ports anti-legacy `domain_graph.py`: it translates estate's annotated
//! code graph into a `requirements_graph.json` domain model. These serde types are the OUTPUT wire
//! contract — they MUST round-trip byte-compatibly with the KEPT `wicked-brain/schemas/
//! domain-model.schema.json` (VERSION 1.1.0), which garden STEERS on and wicked-testing ASSERTS.
//! (The wrong-severity-vocabulary near-miss in the conformance PR is exactly why this fidelity is
//! modeled + tested against the schema, not assumed.)
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
    pub schema_version: String,
    /// `mainframe` (legacy translation) or `modern` (package-dir capability grouping, M5).
    pub migration_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// A capability domain: its requirements + the entities they operate on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Domain {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_id: Option<String>,
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

/// The capability boundary for MODERN code (M5): the PARENT directory of a node's source file — NOT
/// a Louvain community (dense modern code collapses into one near-fully-connected blob). Files at
/// the repo root group under `(root)`.
fn package_dir(file: &str) -> String {
    std::path::Path::new(file)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("(root)")
        .to_string()
}

/// Build the domain model from the annotated estate graph (MODERN mode, M5) — the port of
/// `domain_graph.py`'s modern path. Gate on front-half coverage (fail-closed), then group every
/// behavior-bearing node by PACKAGE DIR into a capability domain, lifting each node's `requirement`
/// semantic + its `business_rule` annotations (confidence-carrying) into a [`Requirement`].
///
/// "Behavior-bearing" here = the node carries a `requirement` semantic OR ≥1 `business_rule`
/// annotation; a node with neither is structural scaffolding and is skipped (it is a coverage hole
/// only if it was flagged behavior-bearing upstream — that is the coverage gate's job, already
/// asserted above). Confidence-less rules are NOT fabricated: the annotation's own confidence rides
/// through. Deterministic: nodes/domains are keyed in sorted (BTreeMap) order.
pub fn build_domain_model(
    store: &dyn GraphRead,
    coverage: &CoverageReport,
    schema_version: &str,
) -> anyhow::Result<DomainModel> {
    assert_front_half_coverage(coverage)?;

    let mut domains: std::collections::BTreeMap<String, Domain> = std::collections::BTreeMap::new();
    let mut nodes = store.all_nodes()?;
    nodes.sort_by(|a, b| a.name.cmp(&b.name)); // deterministic requirement id order

    for node in &nodes {
        let semantics = store.node_semantics(&node.symbol)?;
        let requirement_text = semantics.as_ref().and_then(|s| s.requirement.clone());

        let business_rules: Vec<Rule> = store
            .annotations(&node.symbol)?
            .into_iter()
            .filter(|a| a.r#type == BUSINESS_RULE_ANN)
            .enumerate()
            .map(|(i, a)| Rule {
                id: format!("RULE-{}-{}", node.name, i + 1),
                statement: a.value,
                confidence: a.confidence,
                provenance: Provenance {
                    source: "code-graph".to_string(),
                    reference: format!("{}#{}", node.location.file, node.name),
                    source_kinds: vec!["code-body".to_string()],
                },
                source_ref: Some(format!("{}#{}", node.location.file, node.name)),
            })
            .collect();

        // Not behavior-bearing → structural; skip (the coverage gate already vouched completeness).
        if requirement_text.is_none() && business_rules.is_empty() {
            continue;
        }

        let package = package_dir(&node.location.file);
        let domain = domains.entry(package.clone()).or_insert_with(|| Domain {
            cluster_id: Some(format!("pkg:{package}")),
            ..Default::default()
        });
        let requirement = Requirement {
            title: node.name.clone(),
            description: requirement_text
                .or_else(|| semantics.as_ref().and_then(|s| s.description.clone()))
                .unwrap_or_else(|| node.name.clone()),
            status: semantics.as_ref().map(|s| {
                if s.requirement_validated {
                    "validated".to_string()
                } else {
                    "unvalidated".to_string()
                }
            }),
            business_rules,
            ..Default::default()
        };
        domain
            .requirements
            .insert(format!("REQ-{}", node.name), requirement);
    }

    Ok(DomainModel {
        metadata: Metadata {
            schema_version: schema_version.to_string(),
            migration_mode: "modern".to_string(),
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
        // A minimal MODERN-mode model: one domain, one requirement with a business rule, one entity.
        let json = serde_json::json!({
            "metadata": { "schema_version": "1.1.0", "migration_mode": "modern" },
            "domains": {
                "billing": {
                    "cluster_id": "pkg:billing",
                    "requirements": {
                        "REQ-1": {
                            "title": "charge a customer",
                            "description": "process a payment charge",
                            "legacy_components": [],
                            "data_access": [],
                            "dependencies": [],
                            "business_rules": [
                                { "id": "RULE-1", "statement": "amount must be positive",
                                  "confidence": 0.9,
                                  "provenance": { "source": "code-graph", "ref": "billing/charge.py#charge",
                                                  "source_kinds": ["code-body"] } }
                            ],
                            "validations": [],
                            "error_paths": []
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
        assert_eq!(model.metadata.migration_mode, "modern");
        let dom = model.domains.get("billing").unwrap();
        let req = dom.requirements.get("REQ-1").unwrap();
        assert_eq!(req.business_rules[0].confidence, 0.9);
        assert_eq!(
            req.business_rules[0].provenance.source_kinds,
            vec!["code-body".to_string()]
        );
        // Round-trips back to an equal JSON value (byte-shape fidelity with the kept schema).
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
        let scaffold = mk("helper", "billing/util.py"); // no requirement, no business rule → skipped
        store.begin_batch().unwrap();
        store
            .upsert_nodes(&[charge.clone(), login.clone(), scaffold.clone()])
            .unwrap();
        store.commit_batch().unwrap();
        // charge: a validated requirement + a business rule. login: a business rule only.
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

        let coverage = CoverageReport {
            coverage: 1.0,
            unaccounted: vec![],
        };
        let model = build_domain_model(&store, &coverage, "1.1.0").unwrap();

        assert_eq!(model.metadata.migration_mode, "modern");
        // Two package-dir domains (auth, billing) — the scaffold node contributed nothing.
        assert_eq!(
            model.domains.keys().collect::<Vec<_>>(),
            vec!["auth", "billing"]
        );
        let billing = &model.domains["billing"];
        assert_eq!(billing.cluster_id.as_deref(), Some("pkg:billing"));
        assert_eq!(
            billing.requirements.len(),
            1,
            "only the behavior-bearing node"
        );
        let req = &billing.requirements["REQ-charge"];
        assert_eq!(req.description, "process a payment charge");
        assert_eq!(req.status.as_deref(), Some("validated"));
        assert_eq!(req.business_rules[0].confidence, 0.9);
        assert_eq!(req.business_rules[0].statement, "amount must be positive");
        // The whole model serializes to the wire shape (schema-valid: required keys present).
        let v = serde_json::to_value(&model).unwrap();
        assert_eq!(
            v["domains"]["auth"]["requirements"]["REQ-login"]["business_rules"][0]["confidence"],
            0.7
        );
        assert!(
            v["domains"]["billing"]["requirements"]["REQ-charge"]["legacy_components"].is_array()
        );
    }
}
