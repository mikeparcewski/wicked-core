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

/// The front-half coverage report — the schema-exact wire shape (`coverage.schema.json`, all 11 fields
/// required, `additionalProperties:false`). `coverage = (resolved + risk_flagged) / behavior_bearing`
/// (vacuously 1.0 when `behavior_bearing == 0`); the builder gates on the exact integer `unaccounted == 0`
/// (NOT the rounded `coverage` float — see [`assert_front_half_coverage`]). `deny_unknown_fields` makes
/// DESERIALIZE fail-closed on an extra key, mirroring the schema's `additionalProperties:false` — a
/// hand-written/older report with a stray field is rejected, not silently accepted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageReport {
    /// Total nodes in the graph (all kinds).
    pub total: u64,
    /// The coverage DENOMINATOR — behavior-bearing nodes only.
    pub behavior_bearing: u64,
    /// Behavior-bearing nodes RESOLVED (validated requirement OR business_rule ann ≥ threshold).
    pub resolved: u64,
    /// Behavior-bearing nodes RISK-flagged (accounted but not resolved — HITL queue).
    pub risk_flagged: u64,
    /// Behavior-bearing nodes still BARE — the coverage hole. MUST be 0 for `coverage == 1.0`.
    pub unaccounted: u64,
    /// `(resolved + risk_flagged) / behavior_bearing`, in [0,1].
    pub coverage: f64,
    /// `resolved / (resolved + risk_flagged)`.
    pub resolved_rate: f64,
    /// Mean confidence across RESOLVED nodes carrying a numeric confidence (annotation-backed).
    pub mean_confidence: f64,
    /// Threshold at/above which a `business_rule` annotation counts as RESOLVED (default 0.75).
    pub resolve_threshold: f64,
    /// Per-app breakdown, sorted by app name.
    pub per_app: Vec<PerApp>,
    /// The bare behavior-bearing nodes, sorted by SymbolId. Empty iff `coverage == 1.0`.
    pub unaccounted_nodes: Vec<UnaccountedNode>,
}

/// Per-app coverage breakdown (`coverage.schema.json` `perApp`; NO `db`/`total` — the schema forbids
/// them, so the WIRE shape is anchored on the schema, not coverage.py).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerApp {
    pub app: String,
    pub behavior_bearing: u64,
    pub resolved: u64,
    pub risk_flagged: u64,
    pub unaccounted: u64,
    pub coverage: f64,
}

/// A behavior-bearing node that reached neither RESOLVED nor RISK — the coverage hole. Only `symbol_id`
/// is schema-required; the rest are omitted-when-absent (never serialized as null).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnaccountedNode {
    pub symbol_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
}

/// Default confidence threshold at/above which a `business_rule` annotation counts as RESOLVED.
pub const DEFAULT_RESOLVE_THRESHOLD: f64 = 0.75;

/// Classification config (DES-OUTGOV-005 §Two-predicate — "config-driven … never hardcoded"). The native
/// `NodeKind` enum is closed, so its classification is a compiler-enforced exhaustive match (see
/// [`is_behavior_bearing`]); only the OPEN `Other(tag)` domain + the resolve threshold are config.
#[derive(Debug, Clone)]
pub struct CoverageConfig {
    /// `Other(tag)` tags that ARE behavior-bearing (coverage.py `DEFAULT_ESTATE_BEHAVIOR_KINDS`).
    pub behavior_other_tags: std::collections::BTreeSet<String>,
    pub resolve_threshold: f64,
}

impl Default for CoverageConfig {
    fn default() -> Self {
        CoverageConfig {
            behavior_other_tags: ["cics_program", "step", "db2_table"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            resolve_threshold: DEFAULT_RESOLVE_THRESHOLD,
        }
    }
}

/// Whether an `EdgeKind` is a BEHAVIOR-OUT edge (the Module dead-shell test). Extends coverage.py's
/// `BEHAVIOR_EDGE_KINDS` with the native rule-engine edges `Evaluates`/`Produces`/`Governs`, so a live
/// rules Module is never falsely dead-shell-excluded (the exception SHRINKS the denominator, so its input
/// set is deliberately generous = fail-closed-safe). Structural/inverse edges are excluded.
fn is_behavior_out_edge(kind: &wicked_apps_core::EdgeKind) -> bool {
    use wicked_apps_core::EdgeKind::*;
    match kind {
        Calls | References | Evaluates | Produces | Governs => true,
        Other(t) => matches!(t.as_str(), "uses" | "accesses" | "invokes"),
        Contains | Defines | Imports | Instantiates | Implements | Extends | Overrides
        | HasType | Returns | InvokedBy => false,
    }
}

/// Whether a node counts in the coverage DENOMINATOR (ports `coverage.py::is_behavior_bearing`). The
/// native `NodeKind` match is EXHAUSTIVE with NO wildcard arm — adding a variant to the enum breaks this
/// build until it is explicitly classified (a dropped behavior kind is fail-OPEN, so silence is the
/// hazard). `Module` is behavior-bearing only when it is NOT a dead shell (has ≥1 behavior-out edge).
/// `unknown_other` collects any unrecognized `Other` tag so the caller can WARN (release-safe surfacing
/// of a new behavior extractor's tag that currently defaults structural).
fn is_behavior_bearing(
    node: &wicked_apps_core::Node,
    has_behavior_out: &std::collections::HashSet<String>,
    cfg: &CoverageConfig,
    unknown_other: &mut std::collections::BTreeSet<String>,
) -> bool {
    use wicked_apps_core::NodeKind::*;
    match &node.kind {
        // Behavior-bearing: the atomic units of logic. Namespace≈Module, Trait≈Interface,
        // Constructor≈Method; Rule is the atomic unit a rules-engine extractor emits (bare until annotated).
        Namespace | Function | Method | Constructor | Class | Struct | Interface | Trait | Rule => {
            true
        }
        // Module counts unless it is a dead shell (a pure container with no outgoing behavior edge).
        Module => has_behavior_out.contains(node.symbol.as_str()),
        // Structural leaves + rule-engine containers/sub-clauses (annotation target is the Rule node).
        File | Import | Field | Constant | Variable | Parameter | TypeAlias | Enum | Macro
        | RuleSet | Condition | Action | Fact | Synthetic => false,
        // Open domain: estate-behavior tags count; every other tag is structural (fail-OPEN default —
        // surfaced via `unknown_other` so a new behavior extractor's tag is not silent).
        Other(tag) => {
            if cfg.behavior_other_tags.contains(tag) {
                true
            } else {
                // Known-structural estate tags are expected; only truly-unrecognized tags warrant a warn.
                const KNOWN_STRUCTURAL: &[&str] = &[
                    "dataset",
                    "cics_map",
                    "ims_database",
                    "ims_segment",
                    "parent",
                ];
                if !KNOWN_STRUCTURAL.contains(&tag.as_str()) {
                    unknown_other.insert(tag.clone());
                }
                false
            }
        }
    }
}

/// The bucket a behavior-bearing node falls into for the resolved/risk/unaccounted split.
enum Bucket {
    /// Validated requirement OR business_rule ≥ threshold. Carries the mean_confidence contribution.
    Resolved(Option<f64>),
    /// Accounted (requirement / business_rule / advisory / risk) but not resolved.
    Risk,
    /// Bare — neither resolved nor risk. The coverage hole.
    Unaccounted,
}

/// Classify ONE behavior-bearing node (ports `coverage.py::classify_node`'s accounted/resolved split).
/// `accounted` EXCLUDES description (a merely-described node is UNACCOUNTED — the vacuous-gate guard).
/// Fails closed on an out-of-range annotation confidence (never clamped — matches `build_domain_model`).
fn classify_node(
    store: &dyn GraphRead,
    node: &wicked_apps_core::Node,
    threshold: f64,
) -> anyhow::Result<Bucket> {
    let semantics = store.node_semantics(&node.symbol)?;
    // A requirement counts as present ONLY when non-blank — an empty/whitespace `requirement` is NOT
    // real accounting (coverage.py truthy-checks `req`); counting it would be a vacuous-gate fail-open.
    let has_requirement = semantics
        .as_ref()
        .and_then(|s| s.requirement.as_deref())
        .is_some_and(|r| !r.trim().is_empty());
    let requirement_validated = semantics.as_ref().is_some_and(|s| s.requirement_validated);
    let annotations = store.annotations(&node.symbol)?;

    // business_rule annotations — confidence rides through (fail-closed if out of range, never clamped).
    let mut best_rule_conf: Option<f64> = None;
    let mut has_business_rule = false;
    for a in annotations.iter().filter(|a| a.r#type == BUSINESS_RULE_ANN) {
        if !(0.0..=1.0).contains(&a.confidence) {
            anyhow::bail!(
                "node {}: business_rule confidence {} out of [0,1] — never fabricated or clamped",
                node.name,
                a.confidence
            );
        }
        has_business_rule = true;
        best_rule_conf = Some(best_rule_conf.map_or(a.confidence, |c: f64| c.max(a.confidence)));
    }
    let has_risk = annotations
        .iter()
        .any(|a| a.is_advisory() || a.r#type == RISK_ANN);

    // accounted = requirement OR business_rule OR advisory/risk — EXCLUDING description.
    let accounted = has_requirement || has_business_rule || has_risk;
    if !accounted {
        return Ok(Bucket::Unaccounted);
    }
    // resolved = validated requirement OR a business_rule at/above threshold.
    let rule_resolved = best_rule_conf.is_some_and(|c| c >= threshold);
    if (has_requirement && requirement_validated) || rule_resolved {
        // mean_confidence contribution: only a resolved-via-rule node carries a numeric confidence
        // (a requirement carries none — NodeSemantics has no confidence field).
        let conf = if rule_resolved { best_rule_conf } else { None };
        Ok(Bucket::Resolved(conf))
    } else {
        Ok(Bucket::Risk)
    }
}

/// Recompute the front-half coverage report DIRECTLY from the store (DES-OUTGOV-005). Store-bound, so the
/// gate no longer trusts an external file. Deterministic: nodes visited in SymbolId order,
/// `unaccounted_nodes` + `per_app` sorted. Convenience wrapper over [`recompute_front_half_coverage_with`]
/// using the default config.
pub fn recompute_front_half_coverage(store: &dyn GraphRead) -> anyhow::Result<CoverageReport> {
    recompute_front_half_coverage_with(store, &CoverageConfig::default())
}

/// [`recompute_front_half_coverage`] with an explicit [`CoverageConfig`] (the Other-tag / threshold seam).
pub fn recompute_front_half_coverage_with(
    store: &dyn GraphRead,
    cfg: &CoverageConfig,
) -> anyhow::Result<CoverageReport> {
    // Fail-closed on an out-of-schema threshold (the schema constrains resolve_threshold to [0,1]) — the
    // public `..._with` entry point could otherwise emit a schema-invalid report. Mirrors the annotation
    // confidence guard below.
    if !(0.0..=1.0).contains(&cfg.resolve_threshold) {
        anyhow::bail!(
            "CoverageConfig.resolve_threshold {} out of [0,1] — the coverage schema constrains it to [0,1]",
            cfg.resolve_threshold
        );
    }
    let mut nodes = store.all_nodes()?;
    nodes.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    let total = nodes.len() as u64;

    // Precompute the Module dead-shell input ONCE: every symbol with ≥1 outgoing behavior edge.
    let mut has_behavior_out: std::collections::HashSet<String> = std::collections::HashSet::new();
    for e in store.all_edges()? {
        if is_behavior_out_edge(&e.kind) {
            has_behavior_out.insert(e.source.as_str().to_string());
        }
    }

    // Per-app accumulators (grouped by package_dir — root sentinel "(root)").
    #[derive(Default)]
    struct Acc {
        behavior_bearing: u64,
        resolved: u64,
        risk_flagged: u64,
        unaccounted: u64,
    }
    let mut apps: std::collections::BTreeMap<String, Acc> = std::collections::BTreeMap::new();
    let mut unknown_other: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut behavior_bearing: u64 = 0;
    let mut resolved: u64 = 0;
    let mut risk_flagged: u64 = 0;
    let mut confidences: Vec<f64> = Vec::new();
    let mut unaccounted_nodes: Vec<UnaccountedNode> = Vec::new();

    for node in &nodes {
        if !is_behavior_bearing(node, &has_behavior_out, cfg, &mut unknown_other) {
            continue;
        }
        behavior_bearing += 1;
        let app = package_dir(&node.location.file);
        let acc = apps.entry(app.clone()).or_default();
        acc.behavior_bearing += 1;
        match classify_node(store, node, cfg.resolve_threshold)? {
            Bucket::Resolved(conf) => {
                resolved += 1;
                acc.resolved += 1;
                if let Some(c) = conf {
                    confidences.push(c);
                }
            }
            Bucket::Risk => {
                risk_flagged += 1;
                acc.risk_flagged += 1;
            }
            Bucket::Unaccounted => {
                acc.unaccounted += 1;
                unaccounted_nodes.push(UnaccountedNode {
                    symbol_id: node.symbol.as_str().to_string(),
                    name: (!node.name.is_empty()).then(|| node.name.clone()),
                    kind: Some(kind_label(&node.kind)),
                    file: (!node.location.file.is_empty()).then(|| node.location.file.clone()),
                    app: Some(app),
                });
            }
        }
    }
    let unaccounted = behavior_bearing - resolved - risk_flagged;

    // Surface any unrecognized Other tag once (release-safe — eprintln!, not debug_assert!).
    for tag in &unknown_other {
        eprintln!(
            "wicked-governance: coverage classification WARN — unrecognized NodeKind::Other(\"{tag}\") \
             treated as STRUCTURAL (not in the behavior-kind set); if it is a behavior kind, add it to \
             CoverageConfig.behavior_other_tags"
        );
    }

    // `coverage` (and per_app coverage) is vacuously 1.0 on an empty denominator (matches coverage.py
    // `_ratio`). But resolved_rate + mean_confidence are 0.0 on the empty case (matches coverage.py — a
    // graph with zero settled/confidence evidence must NOT read as "fully resolved / maximal confidence").
    let ratio = |num: u64, den: u64| {
        if den == 0 {
            1.0
        } else {
            round4(num as f64 / den as f64)
        }
    };
    let coverage = ratio(resolved + risk_flagged, behavior_bearing);
    let settled = resolved + risk_flagged;
    let resolved_rate = if settled == 0 {
        0.0
    } else {
        round4(resolved as f64 / settled as f64)
    };
    let mean_confidence = if confidences.is_empty() {
        0.0
    } else {
        round4(confidences.iter().sum::<f64>() / confidences.len() as f64)
    };

    unaccounted_nodes.sort_by(|a, b| a.symbol_id.cmp(&b.symbol_id));
    let per_app: Vec<PerApp> = apps
        .into_iter()
        .map(|(app, a)| PerApp {
            app,
            behavior_bearing: a.behavior_bearing,
            resolved: a.resolved,
            risk_flagged: a.risk_flagged,
            unaccounted: a.unaccounted,
            coverage: ratio(a.resolved + a.risk_flagged, a.behavior_bearing),
        })
        .collect();

    Ok(CoverageReport {
        total,
        behavior_bearing,
        resolved,
        risk_flagged,
        unaccounted,
        coverage,
        resolved_rate,
        mean_confidence,
        resolve_threshold: cfg.resolve_threshold,
        per_app,
        unaccounted_nodes,
    })
}

/// A stable human label for a `NodeKind` (for `unaccounted_nodes[].kind`).
fn kind_label(kind: &wicked_apps_core::NodeKind) -> String {
    use wicked_apps_core::NodeKind::*;
    match kind {
        Other(t) => t.clone(),
        k => format!("{k:?}"),
    }
}

/// Round to 4 decimal places (the schema's stated precision for the ratio fields).
fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

/// Fail-closed front-half precondition (port of `domain_graph.py::assert_front_half_coverage`, §I5):
/// the builder REFUSES to translate an unannotated graph. Every behavior-bearing node must be resolved
/// or risk-flagged; otherwise bail, surfacing the unaccounted SymbolIds. Never translates a partial graph.
pub fn assert_front_half_coverage(report: &CoverageReport) -> anyhow::Result<()> {
    // Gate on the EXACT integer `unaccounted`, NOT the 4-dp-rounded `coverage` float. On a large graph a
    // single hole (e.g. 1/20000) rounds up to coverage==1.0, so a float-epsilon test would fail OPEN and
    // translate a graph with a real hole. `unaccounted == 0` is the definitive completeness condition and
    // preserves the vacuous pass (empty graph ⇒ behavior_bearing==0 ⇒ unaccounted==0 ⇒ Ok).
    if report.unaccounted != 0 {
        let shown: Vec<&str> = report
            .unaccounted_nodes
            .iter()
            .take(20)
            .map(|n| n.symbol_id.as_str())
            .collect();
        anyhow::bail!(
            "front-half coverage {:.4} — {} behavior-bearing node(s) unaccounted; run extraction + \
             coverage first (refusing to translate an unannotated graph). First unaccounted: {:?}",
            report.coverage,
            report.unaccounted,
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
    // Gate on the PASSED report (back-compat) AND, defense-in-depth, on a fresh STORE recompute — so a
    // hand-fed `coverage:1.0` can never translate a graph that actually has an unaccounted behavior node
    // (DES-OUTGOV-005 decision #4). The store is the source of truth; the file is a cross-check. The extra
    // store traversal here is a DELIBERATE correctness-over-perf choice for a fail-closed governance gate:
    // trusting the caller's report would reintroduce the trust-boundary hole this milestone closes. It is
    // a single O(nodes) pass, run once per build (not hot-path).
    assert_front_half_coverage(coverage)?;
    let recomputed = recompute_front_half_coverage(store)?;
    assert_front_half_coverage(&recomputed)?;

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

        // A blank/whitespace requirement is NOT accounting (consistent with `classify_node` + coverage.py),
        // so it neither keeps a node nor synthesizes a placeholder rule.
        let has_requirement = requirement_text
            .as_deref()
            .is_some_and(|r| !r.trim().is_empty());
        let resolved = has_requirement || !business_rules.is_empty();
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
        let node = |s: &str| UnaccountedNode {
            symbol_id: s.into(),
            ..Default::default()
        };
        let partial = CoverageReport {
            coverage: 0.33,
            behavior_bearing: 3,
            resolved: 1,
            unaccounted: 2,
            unaccounted_nodes: vec![node("sym:refund"), node("sym:audit")],
            ..Default::default()
        };
        let err = assert_front_half_coverage(&partial)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unaccounted") && err.contains("refund"),
            "got: {err}"
        );

        let complete = CoverageReport {
            coverage: 1.0,
            ..Default::default()
        };
        assert!(
            assert_front_half_coverage(&complete).is_ok(),
            "coverage 1.0 translates"
        );

        // ROUNDING FAIL-OPEN GUARD: a large graph with a single hole rounds coverage to 1.0000, but the
        // gate keys on the EXACT integer `unaccounted`, so it still DENIES (the vacuous-gate the review
        // caught). A float-epsilon gate would have passed this.
        let rounds_to_one = CoverageReport {
            behavior_bearing: 20000,
            resolved: 19999,
            unaccounted: 1,
            coverage: 1.0, // round4(19999/20000) == 1.0
            unaccounted_nodes: vec![node("sym:hole")],
            ..Default::default()
        };
        let err = assert_front_half_coverage(&rounds_to_one)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unaccounted") && err.contains("hole"),
            "a hole that rounds coverage to 1.0 must STILL fail closed: {err}"
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
        // A truly STRUCTURAL node (Field, not Function) → excluded from the coverage denominator entirely
        // AND dropped by the builder's keep-set. It proves the builder drops structural nodes WITHOUT
        // being an unaccounted behavior node that would fail the internal store-recompute (decision #5b:
        // re-type, do NOT annotate — annotating it would add a 3rd requirement + break the count assert).
        let scaffold = Node::new(
            synthetic_symbol("code", "helper"),
            NodeKind::Field,
            "helper".to_string(),
            Language::new(SYMBOL_SCHEME),
            Location::new("billing/util.py".to_string(), Span::ZERO),
        );
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
            ..Default::default()
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
            ..Default::default()
        };
        let err = build_domain_model(&store, &coverage, "1.0.0")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("[0,1]"),
            "out-of-range confidence must fail closed: {err}"
        );
    }

    // ── recompute_front_half_coverage regression guards (DES-OUTGOV-005) ─────────────────────────────
    mod recompute {
        use super::*;
        use wicked_apps_core::{
            synthetic_symbol, GraphWrite, Language, Location, Node, NodeKind, Span, SqliteStore,
            SYMBOL_SCHEME,
        };
        use wicked_estate_core::Annotation;

        fn store() -> SqliteStore {
            SqliteStore::in_memory().unwrap()
        }
        fn node(store: &mut SqliteStore, name: &str, kind: NodeKind, file: &str) -> Node {
            let n = Node::new(
                synthetic_symbol("code", name),
                kind,
                name.to_string(),
                Language::new(SYMBOL_SCHEME),
                Location::new(file.to_string(), Span::ZERO),
            );
            store.begin_batch().unwrap();
            store.upsert_nodes(&[n.clone()]).unwrap();
            store.commit_batch().unwrap();
            n
        }
        fn cov(store: &SqliteStore) -> CoverageReport {
            recompute_front_half_coverage(store).unwrap()
        }

        #[test]
        fn bare_function_is_unaccounted() {
            let mut s = store();
            node(&mut s, "helper", NodeKind::Function, "a.rs");
            let r = cov(&s);
            assert_eq!(r.behavior_bearing, 1);
            assert_eq!(r.unaccounted, 1);
            assert!(
                r.coverage < 1.0,
                "a bare behavior node fails the gate: {r:?}"
            );
            assert_eq!(r.unaccounted_nodes.len(), 1);
        }

        #[test]
        fn description_only_is_unaccounted_the_vacuous_gate_guard() {
            // A node with ONLY a description (no requirement/business_rule/risk) is UNACCOUNTED —
            // description is EXCLUDED from the numerator, so a merely-described graph cannot pass 1.0.
            let mut s = store();
            let n = node(&mut s, "widget", NodeKind::Function, "a.rs");
            s.set_node_semantics(&n.symbol, Some("does a thing"), None, None)
                .unwrap();
            let r = cov(&s);
            assert_eq!(r.unaccounted, 1, "described-but-rule-unextracted is a hole");
            assert!(r.coverage < 1.0);
        }

        #[test]
        fn bare_rule_nodes_fail_the_gate_critical_1() {
            // Rules-engine regression guard: a graph of bare NodeKind::Rule nodes (zero Function/Module)
            // must NOT report vacuous 1.0 — Rule is behavior-bearing.
            let mut s = store();
            node(&mut s, "R1", NodeKind::Rule, "rules.dmn");
            node(&mut s, "R2", NodeKind::Rule, "rules.dmn");
            let r = cov(&s);
            assert_eq!(r.behavior_bearing, 2, "bare Rules count in the denominator");
            assert!(r.coverage < 1.0, "unextracted rules DENY the gate: {r:?}");
        }

        #[test]
        fn estate_behavior_other_tags_count_critical_2() {
            // Estate-behavior regression guard: cics_program/db2_table are behavior-bearing; dataset/
            // racf_user are structural (not counted).
            let mut s = store();
            node(
                &mut s,
                "PGM1",
                NodeKind::Other("cics_program".into()),
                "a.cbl",
            );
            node(&mut s, "T1", NodeKind::Other("db2_table".into()), "a.cbl");
            node(&mut s, "DS1", NodeKind::Other("dataset".into()), "a.cbl");
            node(&mut s, "U1", NodeKind::Other("racf_user".into()), "a.cbl");
            let r = cov(&s);
            assert_eq!(r.behavior_bearing, 2, "only cics_program + db2_table count");
            assert!(
                r.coverage < 1.0,
                "bare mainframe behavior nodes DENY: {r:?}"
            );
        }

        #[test]
        fn empty_graph_is_vacuously_one() {
            // Zero behavior-bearing nodes → coverage 1.0 (per contract) — assert it explicitly.
            let mut s = store();
            node(&mut s, "f", NodeKind::Field, "a.rs"); // structural only
            let r = cov(&s);
            assert_eq!(r.behavior_bearing, 0);
            assert_eq!(r.coverage, 1.0, "vacuous 1.0 on no behavior nodes");
        }

        #[test]
        fn resolved_risk_split_and_mean_confidence_resolved_only() {
            let mut s = store();
            // resolved via a validated requirement (no annotation confidence).
            let a = node(&mut s, "A", NodeKind::Function, "a.rs");
            s.set_node_semantics(&a.symbol, None, Some("REQ-1"), Some(true))
                .unwrap();
            // resolved via a business_rule ≥ threshold (contributes 0.9 to mean_confidence).
            let b = node(&mut s, "B", NodeKind::Function, "a.rs");
            s.annotate(
                &b.symbol,
                Annotation::new("business_rule", "r", "x").with_confidence(0.9),
            )
            .unwrap();
            // RISK: a below-threshold business_rule (0.4) — accounted but not resolved; its confidence
            // is NOT in mean_confidence (resolved-only).
            let c = node(&mut s, "C", NodeKind::Function, "a.rs");
            s.annotate(
                &c.symbol,
                Annotation::new("business_rule", "r", "y").with_confidence(0.4),
            )
            .unwrap();
            let r = cov(&s);
            assert_eq!(r.behavior_bearing, 3);
            assert_eq!(r.resolved, 2, "validated-req + rule≥0.75");
            assert_eq!(r.risk_flagged, 1, "below-threshold rule is risk");
            assert_eq!(r.unaccounted, 0);
            assert_eq!(r.coverage, 1.0, "all accounted");
            assert_eq!(
                r.mean_confidence, 0.9,
                "mean_confidence is RESOLVED-only (0.9), excludes the 0.4 risk node and the \
                 confidence-less validated requirement"
            );
        }

        #[test]
        fn out_of_range_confidence_fails_closed() {
            let mut s = store();
            let n = node(&mut s, "A", NodeKind::Function, "a.rs");
            s.annotate(
                &n.symbol,
                Annotation::new("business_rule", "r", "x").with_confidence(1.5),
            )
            .unwrap();
            let err = recompute_front_half_coverage(&s).unwrap_err().to_string();
            assert!(
                err.contains("[0,1]"),
                "emitter path guards confidence: {err}"
            );
        }

        #[test]
        fn blank_requirement_is_unaccounted() {
            // An empty/whitespace requirement is NOT accounting (coverage.py truthy-checks it) — a node
            // carrying only a blank requirement is a HOLE, not covered.
            for req in ["", "   "] {
                let mut s = store();
                let n = node(&mut s, "widget", NodeKind::Function, "a.rs");
                s.set_node_semantics(&n.symbol, None, Some(req), Some(true))
                    .unwrap();
                let r = cov(&s);
                assert_eq!(r.unaccounted, 1, "blank requirement {req:?} is a hole");
                assert!(r.coverage < 1.0);
            }
        }

        #[test]
        fn out_of_range_resolve_threshold_bails() {
            let mut s = store();
            node(&mut s, "A", NodeKind::Function, "a.rs");
            let cfg = CoverageConfig {
                resolve_threshold: 1.5,
                ..Default::default()
            };
            let err = recompute_front_half_coverage_with(&s, &cfg)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("[0,1]"),
                "an out-of-schema threshold fails closed: {err}"
            );
        }

        #[test]
        fn empty_case_rates_are_zero_not_one() {
            // coverage.py parity: on a graph with no resolved/settled/confidence evidence, resolved_rate
            // and mean_confidence are 0.0 (NOT 1.0 — must not read as "fully resolved / maximal confidence").
            let mut s = store();
            node(&mut s, "A", NodeKind::Function, "a.rs"); // bare → unaccounted, none resolved
            let r = cov(&s);
            assert_eq!(r.resolved, 0);
            assert_eq!(r.resolved_rate, 0.0, "no settled node ⇒ resolved_rate 0.0");
            assert_eq!(
                r.mean_confidence, 0.0,
                "no confidence evidence ⇒ mean_confidence 0.0"
            );
        }

        #[test]
        fn per_app_groups_by_package_dir_root_sentinel() {
            let mut s = store();
            let a = node(&mut s, "A", NodeKind::Function, "billing/x.rs");
            s.set_node_semantics(&a.symbol, None, Some("REQ"), Some(true))
                .unwrap();
            let b = node(&mut s, "B", NodeKind::Function, "top.rs"); // repo root
            s.set_node_semantics(&b.symbol, None, Some("REQ"), Some(true))
                .unwrap();
            let r = cov(&s);
            let apps: Vec<&str> = r.per_app.iter().map(|p| p.app.as_str()).collect();
            assert_eq!(
                apps,
                vec!["(root)", "billing"],
                "root sentinel is '(root)', not 'graph'"
            );
        }
    }
}
