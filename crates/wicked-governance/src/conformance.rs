//! Conformance rules — prescriptive pattern/policy rules on the shared estate graph.
//!
//! Ported from the retired `wicked-brain` JS `conformance-store` (RET-BRAIN-DOMAIN-001) onto
//! estate's NATIVE rules-engine vocabulary: a [`ConformanceRule`] persists as a
//! `Node(kind = NodeKind::Rule)` (not an `Other(...)` string kind), every field encoded in
//! `Node.metadata`, keyed by the stable synthetic symbol `conformance_rule/<id>`. When a rule names
//! a `symbol_ref`, a `rule → symbol` [`EdgeKind::Governs`] edge is emitted carrying the rule's
//! OWN confidence via a struct-literal [`Edge`] (a fixed `ResolutionTier` cannot carry an arbitrary
//! `0.72`, and the rule was not produced by a resolver — its provenance is
//! `Provenance::Extractor("outgov-v1")`).
//!
//! Recall (`recall_rules`) returns the rules that apply to a query slice: `language`/`layer`/
//! `framework` are WILDCARD facets (an ABSENT facet applies to all), `severity`/`rule_type` are
//! exact, results ordered severity-first (critical→low) then id — deterministic, enforcement-ready.
//! (Wiring recall INTO the per-output gate is PR-C; this module is the population + query half.)

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, Edge, EdgeKind, FromNode, GraphRead, GraphStore, Language, Location,
    Metadata, Node, NodeKind, Span, ToNode, SYMBOL_SCHEME,
};
use wicked_estate_core::{Confidence, Provenance, SymbolQuery};

/// Symbol-namespace prefix for conformance-rule symbols (the synthetic id; the NODE kind is the
/// native [`NodeKind::Rule`]).
pub const CONFORMANCE_RULE: &str = "conformance_rule";
/// Symbol-namespace prefix for a rule's declared governance TARGET (`symbol_ref`). PR-C's
/// recall→gate wiring resolves rules to REAL indexed code symbols; this records the declared intent.
const GOVERNED_SYMBOL: &str = "governed_symbol";
/// Provenance tag stamped on the Governs edges this module emits (M4: the rule's arbitrary
/// confidence rides a struct-literal `Edge`, NOT a fixed `ResolutionTier`).
const OUTGOV_EXTRACTOR: &str = "outgov-v1";
/// The concrete `resolved_by` id estate requires on every edge.
const CONFORMANCE_RESOLVED_BY: &str = "wicked-governance-conformance";

/// A conformance rule's kind. The id prefix MUST agree (INV-C1): `PAT-*` ⇔ pattern, `POL-*` ⇔ policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleType {
    Pattern,
    Policy,
}

impl RuleType {
    /// The id prefix this rule type requires (INV-C1).
    pub fn id_prefix(self) -> &'static str {
        match self {
            RuleType::Pattern => "PAT-",
            RuleType::Policy => "POL-",
        }
    }
}

/// Enforcement precedence — recall orders critical→low.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfSeverity {
    Critical,
    High,
    Medium,
    Low,
}

impl ConfSeverity {
    /// Descending rank for recall ordering (critical highest).
    pub fn rank(self) -> u8 {
        match self {
            ConfSeverity::Critical => 4,
            ConfSeverity::High => 3,
            ConfSeverity::Medium => 2,
            ConfSeverity::Low => 1,
        }
    }
}

/// Wildcard facets — an ABSENT facet means the rule applies to ALL values of it (recall matches
/// `facet IS NULL OR facet == query`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Targets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framework: Option<String>,
}

/// Optional mapping to an external compliance control (SOC2/PCI/…). The resolver behind a named
/// framework is the drop-in [`crate::ingest::ComplianceFramework`] seam.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compliance {
    pub framework: String,
    pub control_id: String,
}

/// Where a rule came from (ingest provenance — the source connector + reference + evidence kinds).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleProvenance {
    pub source: String,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default)]
    pub source_kinds: Vec<String>,
}

/// A prescriptive conformance rule. Field set mirrors the retired `conformance-rules` schema
/// (RET-BRAIN-DOMAIN-001), the wire contract garden + wicked-testing still consume.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConformanceRule {
    pub id: String,
    pub rule_type: RuleType,
    pub statement: String,
    pub severity: ConfSeverity,
    /// Rule authority in `[0,1]` (INV-C2). Rides the Governs edge as `Confidence::new`.
    pub confidence: f32,
    #[serde(default)]
    pub targets: Targets,
    /// A specific code symbol this rule governs (optional — most rules are facet-targeted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compliance: Option<Compliance>,
    #[serde(default)]
    pub provenance: RuleProvenance,
}

impl ConformanceRule {
    /// Fail-closed write-time invariants (ported from `conformance-store` INV-C1/INV-C2). INV-C3
    /// (bundle-unique ids) is enforced at ingest, where the whole bundle is visible.
    pub fn validate(&self) -> anyhow::Result<()> {
        let prefix = self.rule_type.id_prefix();
        if !self.id.starts_with(prefix) {
            anyhow::bail!(
                "INV-C1: rule_type {:?} requires an id with prefix {prefix:?}, got {:?}",
                self.rule_type,
                self.id
            );
        }
        if !(0.0..=1.0).contains(&self.confidence) {
            anyhow::bail!(
                "INV-C2: confidence must be a number in [0,1], got {}",
                self.confidence
            );
        }
        Ok(())
    }

    /// The `rule → governed-symbol` Governs edge, when the rule names a `symbol_ref`. M4: a
    /// struct-literal `Edge` carries the rule's ARBITRARY confidence (a `ResolutionTier` cannot) and
    /// tags provenance as an extractor output (`outgov-v1`), never a code resolver.
    pub fn governs_edge(&self) -> Option<Edge> {
        let target = self.symbol_ref.as_ref()?;
        Some(Edge {
            source: synthetic_symbol(CONFORMANCE_RULE, &self.id),
            target: synthetic_symbol(GOVERNED_SYMBOL, target),
            kind: EdgeKind::Governs,
            confidence: Confidence::new(self.confidence),
            provenance: Provenance::Extractor(OUTGOV_EXTRACTOR.to_string()),
            resolved_by: CONFORMANCE_RESOLVED_BY.to_string(),
            location: None,
            metadata: Metadata::new(),
        })
    }
}

impl ToNode for ConformanceRule {
    fn node_kind() -> &'static str {
        CONFORMANCE_RULE
    }

    fn to_node(&self) -> Node {
        let symbol = synthetic_symbol(CONFORMANCE_RULE, &self.id);
        let mut node = Node::new(
            symbol,
            // NATIVE rules-engine kind (M4) — NOT `NodeKind::Other("rule")`.
            NodeKind::Rule,
            self.id.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{CONFORMANCE_RULE}/{}", self.id), Span::ZERO),
        );
        let value = serde_json::to_value(self).expect("ConformanceRule serializes to JSON");
        if let serde_json::Value::Object(map) = value {
            node.metadata = map;
        }
        node
    }
}

impl FromNode for ConformanceRule {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        if node.kind != NodeKind::Rule {
            anyhow::bail!("expected NodeKind::Rule, got {:?}", node.kind);
        }
        let value = serde_json::Value::Object(node.metadata.clone());
        serde_json::from_value(value)
            .map_err(|e| anyhow::anyhow!("node {} is not a valid ConformanceRule: {e}", node.name))
    }
}

/// Persist one rule: validate (fail-closed), then upsert its native `Rule` node and — when it names
/// a `symbol_ref` — its struct-literal Governs edge, through the single-writer batch path.
pub fn register_rule(store: &mut dyn GraphStore, rule: &ConformanceRule) -> anyhow::Result<()> {
    rule.validate()?;
    store.begin_batch()?;
    store.upsert_nodes(&[rule.to_node()])?;
    if let Some(edge) = rule.governs_edge() {
        store.upsert_edges(&[edge])?;
    }
    store.commit_batch()?;
    Ok(())
}

/// A recall query slice. Any `None` field matches every value of that facet.
#[derive(Debug, Clone, Default)]
pub struct RuleQuery {
    pub language: Option<String>,
    pub layer: Option<String>,
    pub framework: Option<String>,
    pub severity: Option<ConfSeverity>,
    pub rule_type: Option<RuleType>,
}

/// Recall the conformance rules that apply to `query`. Facet semantics (ported from
/// `conformance-store.recallRules`): `language`/`layer`/`framework` match when the rule's facet is
/// ABSENT (wildcard — applies broadly) OR equals the query; `severity`/`rule_type` are exact.
/// Results are ordered severity-first (critical→low) then rule id — deterministic + enforcement-ready.
pub fn recall_rules(
    store: &dyn GraphRead,
    query: &RuleQuery,
) -> anyhow::Result<Vec<ConformanceRule>> {
    // Index-only: restrict to native Rule nodes (the cheap deterministic lane — no FTS, no traversal).
    let sym_query = SymbolQuery {
        kinds: vec![NodeKind::Rule],
        ..Default::default()
    };

    let facet_matches = |rule_facet: &Option<String>, q: &Option<String>| -> bool {
        match q {
            None => true, // query omits the facet → matches all
            Some(qv) => match rule_facet {
                None => true, // rule facet absent → wildcard, applies broadly
                Some(rv) => rv == qv,
            },
        }
    };

    let mut matched: Vec<ConformanceRule> = Vec::new();
    for node in store.find_symbols(&sym_query)? {
        let rule = ConformanceRule::from_node(&node)?;
        if facet_matches(&rule.targets.language, &query.language)
            && facet_matches(&rule.targets.layer, &query.layer)
            && facet_matches(&rule.targets.framework, &query.framework)
            && query.severity.is_none_or(|s| s == rule.severity)
            && query.rule_type.is_none_or(|t| t == rule.rule_type)
        {
            matched.push(rule);
        }
    }

    matched.sort_by(|a, b| {
        b.severity
            .rank()
            .cmp(&a.severity.rank())
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(matched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wicked_apps_core::open_store;

    fn rule(id: &str, ty: RuleType, sev: ConfSeverity, targets: Targets) -> ConformanceRule {
        ConformanceRule {
            id: id.to_string(),
            rule_type: ty,
            statement: format!("statement for {id}"),
            severity: sev,
            confidence: 0.72,
            targets,
            symbol_ref: None,
            compliance: None,
            provenance: RuleProvenance::default(),
        }
    }

    #[test]
    fn to_node_is_native_rule_kind_and_round_trips() {
        let r = rule(
            "PAT-1",
            RuleType::Pattern,
            ConfSeverity::High,
            Targets::default(),
        );
        let node = r.to_node();
        assert_eq!(node.kind, NodeKind::Rule, "M4: native Rule kind, not Other");
        let back = ConformanceRule::from_node(&node).unwrap();
        assert_eq!(back, r, "lossless metadata round-trip");
    }

    #[test]
    fn governs_edge_carries_rule_confidence_via_struct_literal() {
        // M4: a ResolutionTier's confidence is FIXED; the rule's 0.72 must ride a struct-literal edge.
        let mut r = rule(
            "POL-9",
            RuleType::Policy,
            ConfSeverity::Critical,
            Targets::default(),
        );
        assert!(r.governs_edge().is_none(), "no symbol_ref → no edge");
        r.symbol_ref = Some("charge".to_string());
        let edge = r.governs_edge().expect("symbol_ref → Governs edge");
        assert_eq!(edge.kind, EdgeKind::Governs);
        assert_eq!(edge.confidence.get(), 0.72);
        assert_eq!(
            edge.provenance,
            Provenance::Extractor(OUTGOV_EXTRACTOR.to_string())
        );
    }

    #[test]
    fn invariants_fail_closed() {
        // INV-C1: a POL- id declared as a pattern.
        let mut r = rule(
            "POL-1",
            RuleType::Pattern,
            ConfSeverity::Low,
            Targets::default(),
        );
        assert!(r.validate().unwrap_err().to_string().contains("INV-C1"));
        // INV-C2: confidence out of [0,1].
        r = rule(
            "PAT-1",
            RuleType::Pattern,
            ConfSeverity::Low,
            Targets::default(),
        );
        r.confidence = 1.5;
        assert!(r.validate().unwrap_err().to_string().contains("INV-C2"));
    }

    #[test]
    fn register_persists_and_recall_filters_by_facet_and_severity() {
        let mut store = open_store(Some(":memory:")).unwrap();
        // A python-only high rule, a wildcard-language critical rule, a rust-only low rule.
        let py = rule(
            "PAT-py",
            RuleType::Pattern,
            ConfSeverity::High,
            Targets {
                language: Some("python".into()),
                ..Default::default()
            },
        );
        let wild = rule(
            "POL-wild",
            RuleType::Policy,
            ConfSeverity::Critical,
            Targets::default(),
        );
        let rust = rule(
            "PAT-rust",
            RuleType::Pattern,
            ConfSeverity::Low,
            Targets {
                language: Some("rust".into()),
                ..Default::default()
            },
        );
        for r in [&py, &wild, &rust] {
            register_rule(&mut store, r).unwrap();
        }

        // Query python: the python rule + the wildcard rule apply; the rust rule does NOT.
        let got = recall_rules(
            &store,
            &RuleQuery {
                language: Some("python".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let ids: Vec<&str> = got.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["POL-wild", "PAT-py"],
            "wildcard+python, critical-first, then id"
        );

        // Exact severity filter.
        let crit = recall_rules(
            &store,
            &RuleQuery {
                severity: Some(ConfSeverity::Critical),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            crit.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["POL-wild"]
        );
    }

    #[test]
    fn register_rejects_an_invalid_rule_fail_closed() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let bad = rule(
            "POL-x",
            RuleType::Pattern,
            ConfSeverity::Low,
            Targets::default(),
        );
        assert!(
            register_rule(&mut store, &bad).is_err(),
            "INV-C1 blocks the write"
        );
    }
}
