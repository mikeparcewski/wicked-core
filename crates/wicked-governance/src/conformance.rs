//! Conformance rules — prescriptive pattern/policy rules on the shared estate graph.
//!
//! Ported from the retired `wicked-brain` JS `conformance-store` (RET-BRAIN-DOMAIN-001) onto
//! estate's NATIVE rules-engine vocabulary: a [`ConformanceRule`] persists as a
//! `Node(kind = NodeKind::Rule)` (not an `Other(...)` string kind), every field encoded in
//! `Node.metadata`, keyed by the stable synthetic symbol `conformance_rule/<id>`. A rule's
//! `symbol_ref` (an unresolved code-symbol name) rides in that metadata; the `rule → symbol`
//! [`EdgeKind::Governs`] edge is NOT emitted here — [`ConformanceRule::governs_edge`] builds it (a
//! struct-literal [`Edge`] carrying the rule's OWN confidence via `Confidence::new` +
//! `Provenance::Extractor("outgov-v1")`, since a fixed `ResolutionTier` cannot carry an arbitrary
//! `0.72`), but only PR-C's recall→gate step, once `symbol_ref` resolves to a REAL indexed symbol —
//! an edge to a synthetic placeholder would dangle and be pruned by `compact`.
//!
//! Recall (`recall_rules`) returns the rules that apply to a query slice: `language`/`layer`/
//! `framework` are WILDCARD facets (an ABSENT facet applies to all), `severity`/`rule_type` are
//! exact, results ordered severity-first (critical→info) then id — deterministic, enforcement-ready.
//! (Wiring recall INTO the per-output gate is PR-C; this module is the population + query half.)

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, Edge, EdgeKind, FromNode, GraphRead, GraphStore, Language, Location,
    Metadata, Node, NodeKind, Span, SymbolId, ToNode, SYMBOL_SCHEME,
};
use wicked_estate_core::{Confidence, Provenance, SymbolQuery};

/// Symbol-namespace prefix for conformance-rule symbols (the synthetic id; the NODE kind is the
/// native [`NodeKind::Rule`]).
pub const CONFORMANCE_RULE: &str = "conformance_rule";
/// Provenance tag stamped on the Governs edges this module emits (M4: the rule's arbitrary
/// confidence rides a struct-literal `Edge`, NOT a fixed `ResolutionTier`).
const OUTGOV_EXTRACTOR: &str = "outgov-v1";
/// The concrete `resolved_by` id estate requires on every edge.
const CONFORMANCE_RESOLVED_BY: &str = "wicked-governance-conformance";
/// The shared `provenance.source_kinds` wire enum — identical in the conformance-rules AND
/// domain-model schemas ($defs/provenance). Enforced at the fail-closed write boundary (INV-C4).
const VALID_SOURCE_KINDS: [&str; 4] = ["code-body", "type-def", "comment", "doc"];

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

/// Enforcement precedence — mirrors the `conformance-rules` WIRE SCHEMA severity vocabulary
/// (`info | warn | error | critical`), NOT governance's internal Policy `Severity`. This is the
/// cross-product contract garden STEERS on and wicked-testing ASSERTS. Recall orders critical→info.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfSeverity {
    Info,
    Warn,
    Error,
    Critical,
}

impl ConfSeverity {
    /// Descending rank for recall ordering (critical highest).
    pub fn rank(self) -> u8 {
        match self {
            ConfSeverity::Critical => 4,
            ConfSeverity::Error => 3,
            ConfSeverity::Warn => 2,
            ConfSeverity::Info => 1,
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
    /// The source connector. A raw ingest doc may omit it (the ingest STAMPS the adapter name); it
    /// defaults to empty so `normalize_bundle` can fill it before the completeness check.
    #[serde(default)]
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
        // INV-C1: the id must match the wire contract `^(PAT|POL)-[0-9]{3,6}$` AND its prefix must
        // agree with rule_type (PAT-⇔pattern, POL-⇔policy).
        let prefix = self.rule_type.id_prefix();
        let ordinal_ok = self.id.strip_prefix(prefix).is_some_and(|ord| {
            (3..=6).contains(&ord.len()) && ord.bytes().all(|b| b.is_ascii_digit())
        });
        if !ordinal_ok {
            anyhow::bail!(
                "INV-C1: rule id {:?} must match `{prefix}<3-6 digits>` for rule_type {:?}",
                self.id,
                self.rule_type
            );
        }
        if !(0.0..=1.0).contains(&self.confidence) {
            anyhow::bail!(
                "INV-C2: confidence must be a number in [0,1], got {}",
                self.confidence
            );
        }
        // INV-C4: provenance.source_kinds must be drawn from the shared wire enum — the conformance
        // AND domain-model schemas both constrain it. Fail closed here (the write-time boundary all
        // persist paths route through) so a wicked-core producer can never emit an out-of-enum
        // source_kind its cross-product consumers' schema would reject.
        for sk in &self.provenance.source_kinds {
            if !VALID_SOURCE_KINDS.contains(&sk.as_str()) {
                anyhow::bail!(
                    "INV-C4: provenance.source_kinds contains {sk:?}, not one of {VALID_SOURCE_KINDS:?}"
                );
            }
        }
        Ok(())
    }

    /// Build the `rule → governed-symbol` Governs edge for an ALREADY-RESOLVED target symbol. M4: a
    /// struct-literal `Edge` carries the rule's ARBITRARY confidence (a fixed `ResolutionTier`
    /// cannot) and tags provenance as an extractor output (`outgov-v1`), never a code resolver.
    ///
    /// PR-B does NOT emit this at register time: a rule's `symbol_ref` is an unresolved NAME, and an
    /// edge to a synthetic placeholder symbol would DANGLE (deleted by estate's `compact` /
    /// `prune_dangling_edges`) and never reach the real code symbol. PR-C's recall→gate step
    /// resolves `symbol_ref` to the REAL indexed [`SymbolId`] and calls this to link it durably.
    pub fn governs_edge(&self, target: SymbolId) -> Edge {
        Edge {
            source: synthetic_symbol(CONFORMANCE_RULE, &self.id),
            target,
            kind: EdgeKind::Governs,
            confidence: Confidence::new(self.confidence),
            provenance: Provenance::Extractor(OUTGOV_EXTRACTOR.to_string()),
            resolved_by: CONFORMANCE_RESOLVED_BY.to_string(),
            location: None,
            metadata: Metadata::new(),
        }
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

/// Persist one rule: validate (fail-closed), then upsert its native `Rule` node through the
/// single-writer batch path. The rule's `symbol_ref` (an unresolved name) rides in the node
/// metadata; the `rule → symbol` Governs edge is emitted later by PR-C's recall→gate step, once
/// `symbol_ref` resolves to a REAL indexed symbol — an edge to a synthetic placeholder here would
/// dangle and be pruned by `compact` (review finding), so PR-B persists the node only.
pub fn register_rule(store: &mut dyn GraphStore, rule: &ConformanceRule) -> anyhow::Result<()> {
    rule.validate()?;
    store.begin_batch()?;
    store.upsert_nodes(&[rule.to_node()])?;
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
/// Results are ordered severity-first (critical→info) then rule id — deterministic + enforcement-ready.
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
        // A SHARED estate store may hold other `NodeKind::Rule` nodes (e.g. estate's W15 rules
        // engine). Only OUR conformance rules carry the `conformance_rule/<id>` synthetic symbol —
        // identify by that round-trip and skip foreign Rule nodes, so recall never fails on someone
        // else's node (from_node still surfaces corruption in OUR own nodes below).
        if node.symbol != synthetic_symbol(CONFORMANCE_RULE, &node.name) {
            continue;
        }
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
    use wicked_apps_core::{open_store, GraphWrite};

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

    fn lang(l: &str) -> Targets {
        Targets {
            language: Some(l.into()),
            ..Default::default()
        }
    }

    #[test]
    fn to_node_is_native_rule_kind_and_round_trips() {
        let r = rule(
            "PAT-100",
            RuleType::Pattern,
            ConfSeverity::Error,
            Targets::default(),
        );
        let node = r.to_node();
        assert_eq!(node.kind, NodeKind::Rule, "M4: native Rule kind, not Other");
        let back = ConformanceRule::from_node(&node).unwrap();
        assert_eq!(back, r, "lossless metadata round-trip");
    }

    #[test]
    fn from_node_rejects_non_rule_kind() {
        // A non-Rule node (here an Other("policy")) must never deserialize into a ConformanceRule.
        let node = Node::new(
            synthetic_symbol("policy", "POL-001"),
            NodeKind::Other("policy".to_string()),
            "POL-001".to_string(),
            Language::new(SYMBOL_SCHEME),
            Location::new("policy/POL-001".to_string(), Span::ZERO),
        );
        let err = ConformanceRule::from_node(&node).unwrap_err().to_string();
        assert!(err.contains("NodeKind::Rule"), "got: {err}");
    }

    #[test]
    fn governs_edge_carries_rule_confidence_via_struct_literal() {
        // M4: a ResolutionTier's confidence is FIXED; the rule's 0.72 must ride a struct-literal edge
        // built for an ALREADY-RESOLVED target (PR-C resolves symbol_ref → real SymbolId).
        let r = rule(
            "POL-009",
            RuleType::Policy,
            ConfSeverity::Critical,
            Targets::default(),
        );
        let target = synthetic_symbol("symbol", "charge");
        let edge = r.governs_edge(target.clone());
        assert_eq!(edge.kind, EdgeKind::Governs);
        assert_eq!(edge.source, synthetic_symbol(CONFORMANCE_RULE, "POL-009"));
        assert_eq!(
            edge.target, target,
            "targets the RESOLVED symbol, not a placeholder"
        );
        assert_eq!(edge.confidence.get(), 0.72);
        assert_eq!(
            edge.provenance,
            Provenance::Extractor(OUTGOV_EXTRACTOR.to_string())
        );
    }

    #[test]
    fn invariants_fail_closed() {
        // INV-C1 prefix: a POL- id declared as a pattern.
        let mut r = rule(
            "POL-001",
            RuleType::Pattern,
            ConfSeverity::Info,
            Targets::default(),
        );
        assert!(r.validate().unwrap_err().to_string().contains("INV-C1"));
        // INV-C1 ordinal shape: too-short and non-numeric ordinals both fail (wire `[0-9]{3,6}`).
        r = rule(
            "PAT-1",
            RuleType::Pattern,
            ConfSeverity::Info,
            Targets::default(),
        );
        assert!(r.validate().unwrap_err().to_string().contains("INV-C1"));
        r.id = "PAT-abcd".to_string();
        assert!(r.validate().unwrap_err().to_string().contains("INV-C1"));
        // INV-C2: confidence out of [0,1].
        r = rule(
            "PAT-001",
            RuleType::Pattern,
            ConfSeverity::Info,
            Targets::default(),
        );
        r.confidence = 1.5;
        assert!(r.validate().unwrap_err().to_string().contains("INV-C2"));
        // INV-C4: an out-of-enum source_kind (the shared wire enum) fails closed.
        r = rule(
            "PAT-001",
            RuleType::Pattern,
            ConfSeverity::Info,
            Targets::default(),
        );
        r.provenance.source_kinds = vec!["banana".to_string()];
        assert!(r.validate().unwrap_err().to_string().contains("INV-C4"));
        // A valid source_kind passes.
        r.provenance.source_kinds = vec!["code-body".to_string()];
        assert!(r.validate().is_ok());
    }

    #[test]
    fn register_persists_and_recall_filters_by_facet_and_severity() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let py = rule(
            "PAT-100",
            RuleType::Pattern,
            ConfSeverity::Error,
            lang("python"),
        );
        let wild = rule(
            "POL-200",
            RuleType::Policy,
            ConfSeverity::Critical,
            Targets::default(),
        );
        let rust = rule(
            "PAT-300",
            RuleType::Pattern,
            ConfSeverity::Info,
            lang("rust"),
        );
        for r in [&py, &wild, &rust] {
            register_rule(&mut store, r).unwrap();
        }

        // Query python: the python rule + the wildcard-language rule apply; the rust rule does NOT.
        let got = recall_rules(
            &store,
            &RuleQuery {
                language: Some("python".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            got.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["POL-200", "PAT-100"],
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
            vec!["POL-200"]
        );

        // Exact rule_type filter — only the policy rule.
        let pols = recall_rules(
            &store,
            &RuleQuery {
                rule_type: Some(RuleType::Policy),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            pols.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["POL-200"]
        );

        // Empty query recalls all three, ordered critical→info then id.
        let all = recall_rules(&store, &RuleQuery::default()).unwrap();
        assert_eq!(
            all.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["POL-200", "PAT-100", "PAT-300"]
        );
    }

    #[test]
    fn recall_filters_by_layer_and_framework_wildcards() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let svc = rule(
            "PAT-401",
            RuleType::Pattern,
            ConfSeverity::Warn,
            Targets {
                layer: Some("service".into()),
                ..Default::default()
            },
        );
        let django = rule(
            "PAT-402",
            RuleType::Pattern,
            ConfSeverity::Warn,
            Targets {
                framework: Some("django".into()),
                ..Default::default()
            },
        );
        let wild = rule(
            "POL-403",
            RuleType::Policy,
            ConfSeverity::Warn,
            Targets::default(),
        );
        for r in [&svc, &django, &wild] {
            register_rule(&mut store, r).unwrap();
        }

        // Layer facet: service-layer rule + wildcard apply; the django-framework rule is excluded
        // (its framework facet is set but the query omits framework, so it's unconstrained — it's
        // the LAYER mismatch that excludes... actually django has no layer, so it's a layer-wildcard).
        let by_layer = recall_rules(
            &store,
            &RuleQuery {
                layer: Some("service".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            by_layer.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["PAT-401", "PAT-402", "POL-403"],
            "service-layer + layer-wildcards (django+wild)"
        );

        // Framework facet exact: django + wildcards; the service-layer rule's framework is absent
        // (wildcard) so it ALSO matches — only a rule with a DIFFERENT framework would be excluded.
        let by_fw = recall_rules(
            &store,
            &RuleQuery {
                framework: Some("rails".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            by_fw.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["PAT-401", "POL-403"],
            "rails query: django rule EXCLUDED (framework mismatch), wildcards kept"
        );
    }

    #[test]
    fn register_rejects_an_invalid_rule_fail_closed() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let bad = rule(
            "POL-x",
            RuleType::Pattern,
            ConfSeverity::Info,
            Targets::default(),
        );
        assert!(
            register_rule(&mut store, &bad).is_err(),
            "INV-C1 blocks the write"
        );
    }

    #[test]
    fn recall_skips_foreign_rule_nodes() {
        let mut store = open_store(Some(":memory:")).unwrap();
        register_rule(
            &mut store,
            &rule(
                "PAT-001",
                RuleType::Pattern,
                ConfSeverity::Info,
                Targets::default(),
            ),
        )
        .unwrap();
        // A foreign NodeKind::Rule node (NOT a conformance rule — e.g. estate's W15 rules engine).
        let foreign = Node::new(
            synthetic_symbol("w15_rule", "R-42"),
            NodeKind::Rule,
            "R-42".to_string(),
            Language::new(SYMBOL_SCHEME),
            Location::new("w15_rule/R-42".to_string(), Span::ZERO),
        );
        store.begin_batch().unwrap();
        store.upsert_nodes(&[foreign]).unwrap();
        store.commit_batch().unwrap();
        // recall must SUCCEED (not error on the foreign node) and return only our conformance rule.
        let got = recall_rules(&store, &RuleQuery::default()).unwrap();
        assert_eq!(
            got.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["PAT-001"]
        );
    }
}
