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
}
