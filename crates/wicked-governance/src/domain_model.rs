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
    /// How much of the code this model maps (domain-model 1.1.0, bundle 1.2.3, optional). A graph is built once the
    /// mapped share reaches the configured minimum ([`MIN_COVERAGE_ENV`], default
    /// [`DEFAULT_MIN_COVERAGE_PCT`]), so a reader needs the share to read the graph honestly:
    /// "82% of behaviours mapped".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<ModelCoverage>,
}

/// The coverage a domain model was built at (`metadata.coverage`, domain-model 1.1.0, schema
/// bundle 1.3.0). The minimum counts REAL requirements only (operator decision 2026-11-01,
/// core#1233): a behaviour resolved to a stated, sourced business rule. A risk-flagged placeholder
/// (an empty statement, a rule under the confidence threshold, or one written unconfirmed) still
/// shows in the graph as unconfirmed, but never counts toward the minimum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCoverage {
    /// `floor(real * 100 / behavior_bearing)`; 100 on an empty denominator. The one number the
    /// minimum is checked against.
    pub real_pct: u8,
    /// The minimum the build was held to.
    pub min_pct: u8,
    /// Behavior-bearing nodes in product code (the denominator; tests, data, manifests and
    /// documents are not in it, see [`is_product_file`]).
    pub behavior_bearing: u64,
    /// Behavior-bearing nodes resolved to a stated, sourced business rule.
    pub real: u64,
    /// Behavior-bearing nodes carrying only a risk-flagged placeholder: shown, never counted.
    pub placeholders: u64,
    /// Behavior-bearing nodes with nothing recorded yet.
    pub unmapped: u64,
    /// Behaviour-kind nodes left out of the denominator because their file is not product code.
    pub not_product: u64,
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
    /// How many independent sources support this requirement (domain-model 1.1.0, bundle 1.2.3, optional):
    /// `validated` with its count, or `unconfirmed` with why. See [`crate::corroboration`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corroboration: Option<crate::corroboration::Corroboration>,
    /// The estate SymbolId of the code this requirement covers, when the builder knows it. Not part
    /// of the document (the schema is closed); the store carries it as the requirement's
    /// `SatisfiedBy` edge and its `code_symbol` metadata. `legacy_components` names code by
    /// `file#name`, which two same-named symbols in one file (`A::new`, `B::new`) share.
    #[serde(skip)]
    pub code_symbol: Option<String>,
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
/// (vacuously 1.0 when `behavior_bearing == 0`) is the ACCOUNTED ledger only: the builder's minimum
/// counts RESOLVED behaviours (real requirements; placeholders never count, core#1233) in exact
/// integers, never the rounded `coverage` float — see [`meets_minimum`]. `deny_unknown_fields` makes
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
    /// How many DISTINCT non-blank `requirement` strings back the behavior-bearing set.
    ///
    /// Reported, never gated — see [`RequirementReuse`]. Optional on the wire so a report written
    /// before this field existed still deserializes (`deny_unknown_fields` rejects unknown keys, it
    /// does not require known ones).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirement_reuse: Option<RequirementReuse>,
    /// Behaviour-kind nodes left OUT of the denominator because their file is not product code (a
    /// test, a data file, a manifest or a document: [`is_product_file`]). Optional on the wire for
    /// reports written before the field.
    #[serde(default)]
    pub not_product: u64,
    /// The risk-flagged placeholders (accounted, not real requirements), sorted by SymbolId: what
    /// the next extraction page may revisit. Absent when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub placeholder_nodes: Vec<UnaccountedNode>,
}

/// How concentrated the `requirement` strings are across the nodes claiming them.
///
/// # Why this is here
///
/// `coverage == 1.0` says every behavior-bearing node is ACCOUNTED. It does not say the accounting
/// means anything. Observed live (#131): an agent wrote 46 distinct strings — `NFR-76`, `NFR-77`, … —
/// as the requirement of **34,897** nodes, all `requirement_validated=1`. Coverage computed 1.0
/// legitimately, the pinned validator passed, `domain-graph` translated, and the resulting
/// "requirements" were file-name titles over reference lists. Every gate green, zero rule content.
///
/// The RESOLVED predicate cannot catch that: it truthy-checks a string. Concentration can — 758 nodes
/// per requirement is bulk-mapping, not extraction.
///
/// # Why it does NOT gate
///
/// Deliberately a signal, not a denial. A real system does have cross-cutting requirements covering
/// many symbols, so the honest threshold is an empirical question, and inventing one now would either
/// miss the observed case or deny legitimate reuse. (The observed case averages 2.2% of nodes per
/// string — an intuitive "5% of the graph" rule would have sailed past it.) Emitting the numbers makes
/// bulk-mapping visible immediately and gives a basis for choosing the bar from measurement instead of
/// taste. Gating on it is tracked in #131.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequirementReuse {
    /// Behavior-bearing nodes carrying a non-blank `requirement`.
    pub nodes_with_requirement: u64,
    /// Distinct non-blank `requirement` strings among them.
    pub distinct_requirements: u64,
    /// Nodes sharing the single most-reused `requirement` string.
    pub max_nodes_per_requirement: u64,
    /// The most-reused string itself, truncated — so a reader sees WHAT was bulk-mapped, not just
    /// that something was. Truncated because a requirement may legitimately be a paragraph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub most_reused_requirement: Option<String>,
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
        // `SatisfiedBy` is a requirement→code TRACEABILITY edge — its source is always a synthetic
        // requirement node, never a code module, so it can never contribute to a code module's
        // behavior-out set. Structural for this test (like `Contains`), not a code-behavior edge.
        Contains | Defines | Imports | Instantiates | Implements | Extends | Overrides
        | HasType | Returns | InvokedBy | SatisfiedBy => false,
    }
}

/// TypeScript-family languages. A TS interface is erased at runtime: its own declaration holds only
/// types, and an extracted interface method (`interface_body (method_signature)`) never has a body.
const TS_FAMILY: &[&str] = &["typescript", "tsx", "vue", "svelte"];

/// The type a member's symbol names as its owner: the symbol minus the member's own name and
/// descriptor (`…/TokenVerifier#verify().` → `…/TokenVerifier#`, `Pay#NoOp#run().` → `Pay#NoOp#`,
/// `T#N#` → `T#`). Split on the node's NAME, never on the last `#`: estate renders a name raw, so it
/// may itself hold `#` (a private `#data`, `Live##data.`) or `.`; a backtick-escaped rendering is
/// tried too. `None` when the symbol does not end in the name (nothing is attributed) or the rest is
/// not a type.
fn owner_symbol<'a>(sym: &'a str, name: &str) -> Option<&'a str> {
    if name.is_empty() {
        return None;
    }
    let escaped = format!("`{name}`");
    let prefix = [name, escaped.as_str()].iter().find_map(|n| {
        ["().", ".", "#"]
            .iter()
            .find_map(|d| sym.strip_suffix(*d)?.strip_suffix(*n))
    })?;
    // A raw private name (`#data`) whose node name drops the `#` leaves `Live##`: a type
    // descriptor never has an empty name, so the second `#` belongs to the member.
    let prefix = prefix
        .strip_suffix("##")
        .map_or(prefix, |p| &prefix[..p.len() + 1]);
    prefix.ends_with('#').then_some(prefix)
}

/// What the per-node behaviour test needs from the rest of the graph, computed once per pass
/// ([`BehaviorIndex::build`]) and shared by the coverage recompute and the domain-graph build.
struct BehaviorIndex {
    /// Every symbol with ≥1 outgoing behaviour edge: the Module dead-shell test, and a TS interface
    /// that does something.
    has_behavior_out: std::collections::HashSet<String>,
    /// TS interface method signatures (proven bodiless): structural.
    bodiless: std::collections::HashSet<String>,
    /// TS interfaces with evidence of runtime content: a member outside the declaration (a class or
    /// namespace merged into the same symbol), or a member with a behaviour-out edge.
    carries_body: std::collections::HashSet<String>,
}

impl BehaviorIndex {
    fn build(store: &dyn GraphRead, nodes: &[wicked_apps_core::Node]) -> anyhow::Result<Self> {
        use std::collections::{HashMap, HashSet};
        use wicked_apps_core::NodeKind;
        let mut has_behavior_out: HashSet<String> = HashSet::new();
        for e in store.all_edges()? {
            if is_behavior_out_edge(&e.kind) {
                has_behavior_out.insert(e.source.as_str().to_string());
            }
        }
        let ts_interfaces: HashMap<&str, &wicked_apps_core::Node> = nodes
            .iter()
            .filter(|n| is_ts_interface(n))
            .map(|n| (n.symbol.as_str(), n))
            .collect();
        let mut bodiless: HashSet<String> = HashSet::new();
        let mut carries_body: HashSet<String> = HashSet::new();
        for n in nodes {
            let sym = n.symbol.as_str();
            let Some(owner) =
                owner_symbol(sym, &n.name).and_then(|o| ts_interfaces.get(o).copied())
            else {
                continue;
            };
            // A member is part of the interface's own declaration only when both have a span, they
            // share a file, the member lies inside it, and the member does nothing (no behaviour edge).
            let (sp, osp) = (&n.location.span, &owner.location.span);
            let in_declaration = n.location.file == owner.location.file
                && sp.end_byte > sp.start_byte
                && osp.end_byte > osp.start_byte
                && osp.start_byte <= sp.start_byte
                && sp.end_byte <= osp.end_byte
                && !has_behavior_out.contains(sym);
            if !in_declaration {
                carries_body.insert(owner.symbol.as_str().to_string());
            } else if matches!(n.kind, NodeKind::Method) {
                bodiless.insert(sym.to_string());
            }
        }
        Ok(BehaviorIndex {
            has_behavior_out,
            bodiless,
            carries_body,
        })
    }
}

/// A TypeScript-family `interface` node.
fn is_ts_interface(node: &wicked_apps_core::Node) -> bool {
    matches!(node.kind, wicked_apps_core::NodeKind::Interface)
        && TS_FAMILY.contains(&node.language.as_str())
}

/// Whether a node counts in the coverage DENOMINATOR (ports `coverage.py::is_behavior_bearing`). The
/// native `NodeKind` match is EXHAUSTIVE with NO wildcard arm — adding a variant to the enum breaks this
/// build until it is explicitly classified (a dropped behavior kind is fail-OPEN, so silence is the
/// hazard). `Module` is behavior-bearing only when it is NOT a dead shell (has ≥1 behavior-out edge).
/// A TypeScript `interface` likewise counts only when the graph shows it carries behaviour (its own
/// behaviour-out edge, a member with one, or a member outside its declaration: a merged class or
/// namespace): a pure shape is the same thing as a `TypeAlias` and can never carry a rule. Its
/// method signatures are structural for the same reason. Interfaces and traits in other languages
/// keep counting: Java interface constants, C# property bodies, Rust trait items and Scala trait
/// initializers can execute, and extraction does not prove their absence. Known residuals, both
/// estate extraction gaps: a TS class merged with an interface whose every member the interface
/// redeclares collapses (estate gives both one symbol, last declaration wins) and leaves no evidence
/// of the class; and a namespace merged with an interface has no namespace node (its members are
/// module-level), so only its members' own classification counts it (a `Constant` initializer is
/// structural before and after this rule).
/// `unknown_other` collects any unrecognized `Other` tag so the caller can WARN (release-safe surfacing
/// of a new behavior extractor's tag that currently defaults structural).
fn is_behavior_bearing(
    node: &wicked_apps_core::Node,
    idx: &BehaviorIndex,
    cfg: &CoverageConfig,
    unknown_other: &mut std::collections::BTreeSet<String>,
) -> bool {
    use wicked_apps_core::NodeKind::*;
    match &node.kind {
        // Behavior-bearing: the atomic units of logic. Namespace≈Module.
        Namespace | Class | Struct => true,
        // A callable counts unless it is a TS interface's method signature.
        Function | Method | Constructor => !idx.bodiless.contains(node.symbol.as_str()),
        // A TS interface counts only when the graph shows behaviour; other interfaces and traits count.
        Interface | Trait => {
            !is_ts_interface(node)
                || idx.has_behavior_out.contains(node.symbol.as_str())
                || idx.carries_body.contains(node.symbol.as_str())
        }
        // Rule is behavior-bearing when EXTRACTED — a rules-engine extractor's output, bare until
        // annotated (extractors mint per-language schemes: `odm`, `dmn`, `drl`, …). But a Rule
        // persisted by `persist_domain_model` is a `wicked-apps` SYNTHETIC node — the domain graph's
        // OWN business rule, annotation OUTPUT, not source. Counting it would let a domain-graph
        // persist pin the NEXT coverage run below 1.0 (its own rules read as unaccounted behavior
        // nodes) — the measurement polluting the measurand. So a synthetic domain-model Rule is
        // excluded; an extracted Rule still counts.
        Rule => !wicked_apps_core::is_apps_synthetic_symbol(&node.symbol),
        // Module counts unless it is a dead shell (a pure container with no outgoing behavior edge).
        Module => idx.has_behavior_out.contains(node.symbol.as_str()),
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

/// Test, spec and fixture files, by directory (any case: `Tests/` in .NET, `Specs/`) or by file
/// name (case kept: `*Tests.cs` is a test, `Contest.py` is not). The same patterns as
/// wicked-garden's `extract_loop.is_test_path`, which frames pages and writes their rules.
static TEST_DIR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)(^|/)(tests?|__tests__|specs?|e2e|fixtures?|testdata)/")
        .expect("test-dir pattern compiles")
});
static TEST_FILE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r"\.(test|spec)\.[^/]+$|(^|/)test_[^/]+\.py$|(^|/)tests?\.py$|(^|/)conftest\.py$|_test\.(py|go|rs|c|cc|cpp|cxx|h|hpp)$|_spec\.rb$|Tests?\.(cs|java|kt|swift|scala|php)$",
    )
    .expect("test-file pattern compiles")
});
/// Data, manifest and document files (by extension, any case): the indexer counts their keys and
/// headings as nodes (a JSON object is a `Struct`), but what they hold is not behaviour the
/// product's code implements. wicked-garden's `extract_loop._DATA_EXTS`.
const DATA_EXTS: &[&str] = &[
    ".json",
    ".jsonc",
    ".json5",
    ".jsonl",
    ".yaml",
    ".yml",
    ".toml",
    ".lock",
    ".ini",
    ".cfg",
    ".conf",
    ".properties",
    ".csv",
    ".tsv",
    ".txt",
    ".md",
    ".mdx",
    ".rst",
    ".xml",
];

/// Whether `file` (repo-relative, either slash) holds product code: not a test, spec or fixture
/// file and not a data, manifest or document file. A behaviour-kind node outside product code is
/// not behaviour-bearing (operator decision 2026-11-01): on pi-governance 109 of 120 "behaviours"
/// were JSON evidence keys and package.json fields. A node with no file stays in (it can only
/// make the share harder to reach, never easier). An EXTRACTED rule stays in whatever its file
/// (`is_product_node`): a Salesforce Flow (`.flow-meta.xml`) or a cloud policy (`.json`) is
/// executable behaviour, not data. Classified by file only: a test module inside a product file
/// (Rust's `#[cfg(test)] mod tests`) still counts, in the denominator and, once a page resolves it,
/// in the real count, so it can bias the share either way (codex r3 on #1234; post-mvp).
#[must_use]
pub fn is_product_file(file: &str) -> bool {
    !is_test_file(file) && !is_data_file(file)
}

/// A test, spec or fixture file (garden's `is_test_path`).
fn is_test_file(file: &str) -> bool {
    let norm = file.replace('\\', "/");
    TEST_DIR.is_match(&norm) || TEST_FILE.is_match(&norm)
}

/// A data, manifest or document file, by extension (garden's `is_data_path`).
fn is_data_file(file: &str) -> bool {
    let lower = file.to_ascii_lowercase();
    DATA_EXTS.iter().any(|ext| lower.ends_with(ext))
}

/// Whether a behaviour-kind node is product behaviour: in a product file ([`is_product_file`]), or an
/// extracted rule (a rules-engine extractor's `Rule` node, executable whatever its file's
/// extension) outside test and fixture files. The one predicate the coverage recompute and the build share.
fn is_product_node(node: &wicked_apps_core::Node) -> bool {
    let file = &node.location.file;
    // An extracted rule passes the data-extension test only; a rule in a test or fixture file is
    // still a test (codex r2 on #1234).
    !is_test_file(file)
        && (matches!(node.kind, wicked_apps_core::NodeKind::Rule) || !is_data_file(file))
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
    // Passed in, not re-read. The caller needs the same record to tally requirement reuse, and on a
    // DB-backed store a second `node_semantics` per node is a second round trip across the whole
    // graph — 34,897 of them on the run that motivated the tally.
    semantics: Option<&wicked_estate_core::NodeSemantics>,
) -> anyhow::Result<Bucket> {
    // A requirement counts as present ONLY when non-blank — an empty/whitespace `requirement` is NOT
    // real accounting (coverage.py truthy-checks `req`); counting it would be a vacuous-gate fail-open.
    let has_requirement = semantics
        .and_then(|s| s.requirement.as_deref())
        .is_some_and(|r| !r.trim().is_empty());
    let requirement_validated = semantics.is_some_and(|s| s.requirement_validated);
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
    recompute_observing(store, cfg, &mut |_, _| {})
}

/// Where the coverage share puts one node: counted in its denominator, or left out and why. The
/// recompute decides it for every node it visits ([`recompute_observing`]), so a reader that asks it
/// for a listed requirement's unit gets the very decision behind the header's numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnitClass {
    Counted,
    Uncounted(Uncounted),
}

/// Why the coverage share leaves a listed requirement out (`uncounted` on the requirements read).
/// The graph keeps every requirement a build emitted, and a build emits one for every product unit an
/// extraction page annotated, whether or not the unit counts as behaviour; a later classification
/// change (core#1238: a TypeScript shape is not a behaviour) leaves such rows in the list while the
/// share stops counting them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Uncounted {
    /// Not behaviour-bearing: a type shape, a signature, a constant or another structural unit.
    Structural,
    /// A behaviour-kind unit outside product code (a test, data, manifest or document file).
    NotProduct,
    /// The unit is no longer in the index (its code was removed or renamed since the build).
    Unindexed,
}

/// The recompute, telling `observe` where each visited node falls (by symbol).
fn recompute_observing(
    store: &dyn GraphRead,
    cfg: &CoverageConfig,
    observe: &mut dyn FnMut(&str, UnitClass),
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

    // Precompute ONCE: behaviour-out edges (Module dead shells, interfaces that act), bodiless
    // callables and the interfaces/traits that carry a body.
    let idx = BehaviorIndex::build(store, &nodes)?;

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
    let mut unaccounted: u64 = 0;
    let mut confidences: Vec<f64> = Vec::new();
    let mut unaccounted_nodes: Vec<UnaccountedNode> = Vec::new();
    let mut placeholder_nodes: Vec<UnaccountedNode> = Vec::new();
    let mut not_product: u64 = 0;
    // requirement string → how many behavior-bearing nodes claim it (the #131 bulk-mapping signal).
    let mut requirement_counts: std::collections::BTreeMap<String, u64> =
        std::collections::BTreeMap::new();

    for node in &nodes {
        if !is_behavior_bearing(node, &idx, cfg, &mut unknown_other) {
            observe(
                node.symbol.as_str(),
                UnitClass::Uncounted(Uncounted::Structural),
            );
            continue;
        }
        if !is_product_node(node) {
            observe(
                node.symbol.as_str(),
                UnitClass::Uncounted(Uncounted::NotProduct),
            );
            not_product += 1;
            continue;
        }
        observe(node.symbol.as_str(), UnitClass::Counted);
        behavior_bearing += 1;
        // ONE read, used twice: the reuse tally and the bucket classification below.
        let semantics = store.node_semantics(&node.symbol)?;
        // Counted on the same non-blank rule `classify_node` uses, so the signal describes exactly
        // the strings that can carry a node to ACCOUNTED.
        if let Some(req) = semantics
            .as_ref()
            .and_then(|sem| sem.requirement.as_deref())
            .map(str::trim)
            .filter(|r| !r.is_empty())
        {
            *requirement_counts.entry(req.to_string()).or_insert(0) += 1;
        }
        let app = package_dir(&node.location.file);
        let acc = apps.entry(app.clone()).or_default();
        acc.behavior_bearing += 1;
        match classify_node(store, node, cfg.resolve_threshold, semantics.as_ref())? {
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
                placeholder_nodes.push(listed_node(node, app));
            }
            Bucket::Unaccounted => {
                unaccounted += 1;
                acc.unaccounted += 1;
                unaccounted_nodes.push(listed_node(node, app));
            }
        }
    }
    // `unaccounted` is COUNTED directly (not derived by subtraction) so it can never u64-underflow-wrap
    // in release; the three buckets partition the behavior-bearing set by construction.
    debug_assert_eq!(resolved + risk_flagged + unaccounted, behavior_bearing);

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
    placeholder_nodes.sort_by(|a, b| a.symbol_id.cmp(&b.symbol_id));
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

    // `max_by_key` on a BTreeMap iterates in key order and keeps the LAST maximum, so ties resolve to
    // the lexicographically greatest string — deterministic, which this report is required to be.
    let top = requirement_counts.iter().max_by_key(|(_, n)| **n);
    // Absent entirely when no node carries a requirement, rather than an object of zeroes with no
    // string in it. That keeps `most_reused_requirement` REQUIRED whenever the object is present,
    // so a consumer never has to handle a half-populated reuse record.
    let requirement_reuse = top.map(|_| RequirementReuse {
        nodes_with_requirement: requirement_counts.values().sum(),
        distinct_requirements: requirement_counts.len() as u64,
        max_nodes_per_requirement: top.map(|(_, n)| *n).unwrap_or(0),
        most_reused_requirement: top.map(|(r, _)| {
            // Bounded: a requirement may legitimately be a paragraph, and this rides in a report that
            // ends up in denial messages and logs.
            // 120 INCLUDING the ellipsis, so the schema's `maxLength: 120` is exactly true.
            const MAX: usize = 120;
            if r.chars().count() <= MAX {
                r.clone()
            } else {
                format!("{}…", r.chars().take(MAX - 1).collect::<String>())
            }
        }),
    });

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
        requirement_reuse,
        not_product,
        placeholder_nodes,
    })
}

/// A behaviour-bearing node as the report lists it (`unaccounted_nodes`, `placeholder_nodes`).
fn listed_node(node: &wicked_apps_core::Node, app: String) -> UnaccountedNode {
    UnaccountedNode {
        symbol_id: node.symbol.as_str().to_string(),
        name: (!node.name.is_empty()).then(|| node.name.clone()),
        kind: Some(kind_label(&node.kind)),
        file: (!node.location.file.is_empty()).then(|| node.location.file.clone()),
        app: Some(app),
    }
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

/// Below this many requirement-bearing nodes, concentration is NOT judged: a small graph cannot be
/// told apart from a small-but-legitimate catalog, so denying it would be guesswork. Operator-tunable
/// via `WICKED_COVERAGE_MIN_JUDGEABLE`.
const COVERAGE_MIN_JUDGEABLE_NODES: u64 = 100;
/// Average behavior-bearing nodes per distinct `requirement` string above which the accounting reads
/// as bulk-mapping rather than extraction (#131). The observed abuse averaged ~758 nodes/string (46
/// strings over 34,897 nodes); a real catalog for a graph that size runs to hundreds/thousands of
/// distinct requirements, i.e. tens of nodes each. 100 sits well above generous-legitimate and far
/// below the abuse. Operator-tunable via `WICKED_COVERAGE_MAX_AVG_NODES_PER_REQ`.
const COVERAGE_MAX_AVG_NODES_PER_REQ: f64 = 100.0;

/// Read a `u64` tunable from the environment, falling back to `default` on absent-or-unparseable.
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}
/// Read an `f64` tunable from the environment, falling back to `default` on absent-or-unparseable.
fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|v: &f64| v.is_finite())
        .unwrap_or(default)
}

/// Fail-closed front-half precondition (port of `domain_graph.py::assert_front_half_coverage`, §I5) at
/// 100%: every behavior-bearing node must be a REAL requirement (resolved; a risk-flagged placeholder
/// is not one, core#1233); otherwise bail, surfacing the nodes that are not. The configured minimum
/// is [`assert_front_half_coverage_min`].
///
/// It ALSO refuses a graph whose accounting is content-free (#131): `coverage == 1.0` says every node
/// is ACCOUNTED, not that the accounting means anything. A handful of `requirement` strings smeared
/// across tens of thousands of nodes (observed: 46 over 34,897, all `requirement_validated=1`) reaches
/// 1.0 legitimately with zero rule content, because the RESOLVED predicate truthy-checks a string.
/// Concentration catches it where the predicate cannot — this turns the [`RequirementReuse`] SIGNAL
/// into a gate.
pub fn assert_front_half_coverage(report: &CoverageReport) -> anyhow::Result<()> {
    assert_front_half_coverage_min(report, 100)
}

/// The default minimum share of behavior-bearing nodes a requirements graph must map before it is
/// built (operator ruling 2026-10-31). Adjustable per daemon: crew's
/// `requirementsGraph.minCoveragePct` setting, exported as [`MIN_COVERAGE_ENV`].
pub const DEFAULT_MIN_COVERAGE_PCT: u8 = 80;

/// The variable carrying the configured minimum (an integer percentage, 1..=100). The daemon
/// exports it at boot and on every settings change; `wicked-core domain-graph` and `wicked-core
/// coverage --min-coverage-pct` read it, and the coverage validator is handed it explicitly.
pub const MIN_COVERAGE_ENV: &str = "WICKED_REQUIREMENTS_MIN_COVERAGE_PCT";

/// Parse a minimum: an integer 1..=100 (surrounding whitespace and one trailing `%` allowed).
#[must_use]
pub fn parse_min_coverage_pct(raw: &str) -> Option<u8> {
    let t = raw.trim();
    let t = t.strip_suffix('%').unwrap_or(t).trim();
    t.parse::<u8>().ok().filter(|v| (1..=100).contains(v))
}

/// The configured minimum from [`MIN_COVERAGE_ENV`], or [`DEFAULT_MIN_COVERAGE_PCT`] when it is
/// absent or not a valid percentage.
///
/// Absent (or empty) is the default. A value that is SET but not a whole percentage 1..=100 is an
/// error, never the default: a typo'd `95.5` silently becoming 80 would approve a graph below the
/// bar the operator set. Callers fail closed on it.
pub fn min_coverage_pct_from_env() -> Result<u8, String> {
    match std::env::var(MIN_COVERAGE_ENV) {
        Ok(v) if !v.trim().is_empty() => parse_min_coverage_pct(&v)
            .ok_or_else(|| format!("{MIN_COVERAGE_ENV}={v:?} is not a whole percentage 1..100")),
        _ => Ok(DEFAULT_MIN_COVERAGE_PCT),
    }
}

/// `floor(real * 100 / behavior_bearing)`, 100 on an empty denominator, where `real` is the
/// behaviours RESOLVED to a stated, sourced business rule. Risk-flagged placeholders never count
/// (operator decision 2026-11-01, core#1233). Floored so the number shown never rounds up past the
/// minimum it is compared to.
#[must_use]
pub fn real_pct(report: &CoverageReport) -> u8 {
    let den = denominator(report);
    if den == 0 {
        return 100;
    }
    let pct = u128::from(report.resolved) * 100 / den;
    pct.min(100) as u8
}

/// The denominator a report is judged on: `behavior_bearing`, or the bucket sum when a hand-built
/// or inconsistent report claims fewer behaviours than its buckets hold — never a smaller number
/// than the holes it lists, so an inconsistent report can only read WORSE, not better.
fn denominator(report: &CoverageReport) -> u128 {
    // u128: a hand-built report's buckets cannot overflow their sum (codex on #1234).
    u128::from(report.behavior_bearing).max(
        u128::from(report.resolved)
            + u128::from(report.risk_flagged)
            + u128::from(report.unaccounted),
    )
}

/// Whether REAL requirements (resolved behaviours) reach `min_pct` percent of the behaviour-bearing
/// nodes. Placeholders do not count. Exact integer arithmetic, never the rounded `coverage` float:
/// on a large graph a single hole rounds up to 1.0, so a float test would fail open. `min_pct ==
/// 100` is exactly "every behaviour is a real requirement".
#[must_use]
pub fn meets_minimum(report: &CoverageReport, min_pct: u8) -> bool {
    let den = denominator(report);
    if min_pct >= 100 {
        return report.risk_flagged == 0
            && report.unaccounted == 0
            && u128::from(report.resolved) >= den;
    }
    u128::from(report.resolved) * 100 >= u128::from(min_pct) * den
}

/// The one-line verdict, e.g. `"23% real requirements, minimum 80% (1362 of 5786 behaviours;
/// 4397 placeholders, 27 unmapped)"`.
#[must_use]
pub fn coverage_verdict(report: &CoverageReport, min_pct: u8) -> String {
    format!(
        "{}% real requirements, minimum {}% ({} of {} behaviours; {} placeholders, {} unmapped)",
        real_pct(report),
        min_pct,
        report.resolved,
        denominator(report),
        report.risk_flagged,
        report.unaccounted
    )
}

/// [`assert_front_half_coverage`] against a configured minimum: the builder REFUSES to translate a
/// graph that maps less than `min_pct` percent of its behavior-bearing nodes, and says how far it
/// got. The mapping already done stays in the estate store (annotations are never rolled back), so
/// the next extraction run continues from it rather than starting over.
pub fn assert_front_half_coverage_min(report: &CoverageReport, min_pct: u8) -> anyhow::Result<()> {
    if !meets_minimum(report, min_pct) {
        let shown: Vec<&str> = report
            .unaccounted_nodes
            .iter()
            .chain(&report.placeholder_nodes)
            .take(20)
            .map(|n| n.symbol_id.as_str())
            .collect();
        // FATAL-LAST: the diagnostic sample precedes the summary+action line, because downstream
        // consumers (unit denial_reason, logs) keep the TAIL of long output — the line a human
        // must read has to be the last one, not buried above a 20-item symbol list.
        anyhow::bail!(
            "first not yet real (sample of {}): {:?}\n{} — {} behavior-bearing node(s) are not \
             real requirements yet. The mapping so far is kept in the estate store; the next \
             domain-extraction run continues from it (refusing to build a requirements graph \
             below the minimum).",
            shown.len(),
            shown,
            coverage_verdict(report, min_pct),
            u128::from(report.risk_flagged) + u128::from(report.unaccounted)
        );
    }
    // #131: content-free accounting. Gated on the AVERAGE (nodes / distinct), not any single string's
    // max — a real system can have ONE legitimately broad cross-cutting requirement, but a whole
    // catalog that thin is bulk-mapping. Only applied once there are enough requirement-bearing nodes
    // to judge (a small graph is indistinguishable from a small-but-real catalog, so it is never
    // denied here — this is why the 12-node miniature test still passes). Both bounds are
    // operator-tunable so a genuinely broad-but-real catalog can raise them rather than be blocked.
    if let Some(reuse) = &report.requirement_reuse {
        let min_judgeable = env_u64(
            "WICKED_COVERAGE_MIN_JUDGEABLE",
            COVERAGE_MIN_JUDGEABLE_NODES,
        );
        let max_avg = env_f64(
            "WICKED_COVERAGE_MAX_AVG_NODES_PER_REQ",
            COVERAGE_MAX_AVG_NODES_PER_REQ,
        );
        if reuse.nodes_with_requirement >= min_judgeable && reuse.distinct_requirements > 0 {
            let avg = reuse.nodes_with_requirement as f64 / reuse.distinct_requirements as f64;
            if avg > max_avg {
                // FATAL-LAST: the action line is last, because consumers keep the TAIL of long output.
                anyhow::bail!(
                    "requirement concentration: {} node(s) over {} distinct string(s) = {:.1} nodes \
                     per requirement (most-reused backs {} node(s): {:?}); coverage is {:.4} but the \
                     accounting is content-free (#131) — a catalog this thin is bulk-mapping, not \
                     extraction. Extract real requirement content, or raise \
                     WICKED_COVERAGE_MAX_AVG_NODES_PER_REQ (currently {:.0}) if this catalog is \
                     genuinely this broad.",
                    reuse.nodes_with_requirement,
                    reuse.distinct_requirements,
                    avg,
                    reuse.max_nodes_per_requirement,
                    reuse.most_reused_requirement.as_deref().unwrap_or("<none>"),
                    report.coverage,
                    max_avg,
                );
            }
        }
    }
    Ok(())
}

/// The `business_rule` annotation type — the estate annotations the builder lifts into
/// [`Requirement::business_rules`].
const BUSINESS_RULE_ANN: &str = "business_rule";
/// A custom `risk` annotation type — a risk-flagged placeholder: accounted in the `coverage` ledger
/// and kept in the graph as a `review` requirement, but never counted toward the minimum.
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
    build_domain_model_with(
        store,
        coverage,
        schema_version,
        &DomainBuildOptions::default(),
    )
}

/// How [`build_domain_model_with`] builds: the coverage minimum and, optionally, the repository
/// root (to name tracked files the index skipped when an item is unconfirmed).
#[derive(Debug, Clone)]
pub struct DomainBuildOptions<'a> {
    /// The share of behavior-bearing nodes that must be mapped, 1..=100.
    pub min_coverage_pct: u8,
    pub repo_root: Option<&'a std::path::Path>,
}

impl Default for DomainBuildOptions<'_> {
    /// Full coverage, no repository root: [`build_domain_model`]'s historical contract.
    fn default() -> Self {
        DomainBuildOptions {
            min_coverage_pct: 100,
            repo_root: None,
        }
    }
}

/// [`build_domain_model`] at a configured minimum (operator ruling 2026-10-31): the graph is built
/// once `min_coverage_pct` percent of the behavior-bearing nodes are mapped, carries that share on
/// `metadata.coverage`, and every requirement carries its corroboration.
pub fn build_domain_model_with(
    store: &dyn GraphRead,
    coverage: &CoverageReport,
    schema_version: &str,
    opts: &DomainBuildOptions<'_>,
) -> anyhow::Result<DomainModel> {
    if !(1..=100).contains(&opts.min_coverage_pct) {
        anyhow::bail!("min_coverage_pct {} out of 1..=100", opts.min_coverage_pct);
    }
    let min_pct = opts.min_coverage_pct;
    // 1.1.0 carries `metadata.coverage` + `requirement.corroboration`; a 1.0.0 document must not
    // (its schema's objects are closed), so a caller asking for 1.0.0 gets the 1.0.0 shape.
    let rich = match schema_version {
        "1.1.0" => true,
        "1.0.0" => false,
        other => anyhow::bail!(
            "schema_version {other:?} has no domain-model schema here (accepted: 1.0.0, 1.1.0)"
        ),
    };
    // Gate on the PASSED report (back-compat) AND, defense-in-depth, on a fresh STORE recompute — so a
    // hand-fed `coverage:1.0` can never translate a graph that actually has an unaccounted behavior node
    // (DES-OUTGOV-005 decision #4). The store is the source of truth; the file is a cross-check. The extra
    // store traversal here is a DELIBERATE correctness-over-perf choice for a fail-closed governance gate:
    // trusting the caller's report would reintroduce the trust-boundary hole this milestone closes. It is
    // a single O(nodes) pass, run once per build (not hot-path).
    assert_front_half_coverage_min(coverage, min_pct)?;
    let recomputed = recompute_front_half_coverage(store)?;
    assert_front_half_coverage_min(&recomputed, min_pct)?;
    let corroboration = if rich {
        Some(crate::corroboration::CorroborationIndex::build(
            store,
            opts.repo_root,
        )?)
    } else {
        None
    };

    let mut domains: std::collections::BTreeMap<String, Domain> = std::collections::BTreeMap::new();
    let mut nodes = store.all_nodes()?;
    nodes.sort_by(|a, b| a.symbol.cmp(&b.symbol)); // stable, UNIQUE visitation order

    // STABLE requirement ids across a growing mapping. A requirement's number is its node's slot
    // among the domain's behaviour-bearing (or mapped) nodes in SymbolId order — whether or not it
    // is mapped yet. Numbering only the EMITTED requirements (the old `len() + 1`) was stable only
    // while every build was complete: once a graph is built at a partial minimum, mapping one more
    // node would renumber every later requirement, so `REQ-001` would mean another node, its
    // persisted `SatisfiedBy` edge would point at the old one, and crew's overrides (keyed
    // `domain::REQ-NNN`) would land on the wrong item. Gaps in the numbering are unmapped nodes.
    let cfg = CoverageConfig::default();
    let idx = BehaviorIndex::build(store, &nodes)?;
    let mut unknown_other = std::collections::BTreeSet::new();
    let mut slots: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();

    for node in &nodes {
        // Not product behaviour (a test, data, manifest or document file): no requirement, even
        // when an earlier page annotated it, so the graph matches the denominator (agy on #1234).
        // First, before any annotation is read or validated (codex r2 on #1234).
        if !is_product_node(node) {
            continue;
        }
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
        let behavior = is_behavior_bearing(node, &idx, &cfg, &mut unknown_other);
        let accounted = resolved || !risk_notes.is_empty();
        let slot = if behavior || accounted {
            let n = slots.entry(package_dir(&node.location.file)).or_insert(0);
            *n += 1;
            Some(*n)
        } else {
            None
        };
        // Nothing accounted → skip: exactly the coverage gate's predicate. A DESCRIPTION alone is not
        // accounting (coverage counts a merely-described behaviour as a hole), so it never becomes a
        // requirement either — under a partial minimum it would otherwise turn an unmapped node into
        // an item with a synthesized rule.
        let Some(slot) = slot.filter(|_| accounted) else {
            continue;
        };

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
        // Distinct extractor SEATS that stated a real business rule (a RISK flag is not one). The seat
        // is the annotation's `author`, never its `provenance`: provenance says where one extractor's
        // evidence came from, so counting it would let one seat citing two places corroborate itself.
        // A rule with no recorded author is one anonymous seat.
        let extractor_sources = annotations
            .iter()
            .filter(|a| a.r#type == BUSINESS_RULE_ANN)
            .map(|a| a.author.trim())
            .collect::<std::collections::BTreeSet<_>>()
            .len() as u32;
        let requirement = Requirement {
            corroboration: corroboration
                .as_ref()
                .map(|c| c.assess(node, extractor_sources)),
            title: node.name.clone(),
            description: requirement_text
                .or(description_text)
                .unwrap_or_else(|| node.name.clone()),
            legacy_components: vec![node_ref(node)], // the estate symbol this requirement covers
            status: Some(status.to_string()),
            business_rules,
            code_symbol: Some(node.symbol.as_str().to_string()),
            ..Default::default()
        };
        // The node's stable slot in its domain (see `slots` above) — unique, deterministic.
        let req_id = format!("REQ-{slot:03}");
        domain.requirements.insert(req_id, requirement);
    }

    Ok(DomainModel {
        metadata: Metadata {
            schema_version: schema_version.to_string(),
            // M5 package-dir grouping IS the schema's "functional" (capability-grouped) mode.
            migration_mode: "functional".to_string(),
            source: Some("estate-graph".to_string()),
            coverage: rich.then(|| model_coverage(&recomputed, min_pct)),
        },
        domains,
    })
}

/// Persist an assembled [`DomainModel`] into the estate graph store as nodes + edges.
///
/// The domain/requirements graph belongs IN the estate DB, not as a `requirements_graph.json` file
/// on disk (operator directive; the locked "domain data lives in estate's graph" direction). The
/// JSON is retained as an optional export, but THIS is the source of truth: each domain, requirement
/// and business rule becomes a graph node, wired by `Contains`/`References` edges, so the domain
/// graph is queryable and composable like the code graph it derives from — and its presence in the
/// store (not a file's existence) is the phase's real evidence.
///
/// Idempotent: stable synthetic SymbolIds (ADR-002 — never content-hash) mean a re-run upserts the
/// same nodes rather than stacking duplicates. Returns `(nodes_written, edges_written)`.
///
/// Requirement→code satisfaction is wired: each `legacy_components` entry (`file#name`) is resolved
/// to its code SymbolId via `find_symbols` and emitted as a `SatisfiedBy` edge (source = requirement,
/// target = code — the dependent→dependency invariant, so `BlastRadius(code)` surfaces the
/// requirements a change may break). Entries that resolve to no code node in this graph emit NO edge
/// (never a dangling one) and need NO separate field — drift is derivable as the `legacy_components`
/// provenance (kept on the node) minus the SatisfiedBy edges, and is re-resolved on the next persist
/// relink once the code is indexed. The edge is the first-class `EdgeKind::SatisfiedBy` (estate-core
/// 0.16.7), mirroring the `Governs` rule→code precedent so `BlastRadius` and bounded traversal can
/// filter requirement satisfaction by kind.
pub fn persist_domain_model(
    store: &mut dyn wicked_apps_core::GraphStore,
    model: &DomainModel,
) -> anyhow::Result<(usize, usize)> {
    persist_domain_model_expecting(store, model, None)
}

/// [`persist_domain_model`], refusing to write when the store's build is no longer the one
/// `expected_generation` names (the generation [`stabilize_requirement_ids`] numbered the model
/// against): another build committed in between, so this one's ids may collide with it. The
/// check runs inside the write transaction. The caller rebuilds and tries again.
pub fn persist_domain_model_expecting(
    store: &mut dyn wicked_apps_core::GraphStore,
    model: &DomainModel,
    expected_generation: Option<u64>,
) -> anyhow::Result<(usize, usize)> {
    use wicked_apps_core::{
        synthetic_symbol, Edge, EdgeKind, Language, Location, Node, NodeKind, ResolutionTier, Span,
        SymbolQuery, SYMBOL_SCHEME,
    };

    let lang = Language::new(SYMBOL_SCHEME);
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();

    let mk = |scheme: &str, id: &str, kind: NodeKind, name: &str| -> Node {
        Node::new(
            synthetic_symbol(scheme, id),
            kind,
            name.to_string(),
            lang.clone(),
            // Synthetic nodes have no source span; the scheme/id IS their address (emit.rs pattern).
            Location::new(format!("{scheme}/{id}"), Span::ZERO),
        )
    };
    // `Contains` is structural and certain (a domain HAS these requirements); a `dependencies`
    // reference is an inferred cross-link, so it rides the lower Heuristic tier's confidence.
    let contains = ResolutionTier::Parsed;
    let inferred = ResolutionTier::Heuristic;

    for (dname, domain) in &model.domains {
        let dsym = synthetic_symbol("domain", dname);
        let mut dnode = mk(
            "domain",
            dname,
            NodeKind::Other("domain".to_string()),
            dname,
        );
        if let Some(desc) = &domain.description {
            dnode.metadata.insert(
                "description".to_string(),
                serde_json::Value::String(desc.clone()),
            );
        }
        // The document fields the nodes and edges do not carry (entities, cluster id) and the
        // model's metadata, so the store reads back the exact document the build produced.
        let mut ddoc = serde_json::to_value(domain)?;
        if let Some(o) = ddoc.as_object_mut() {
            o.remove("requirements");
        }
        dnode.metadata.insert(DOMAIN_DOC_KEY.to_string(), ddoc);
        dnode.metadata.insert(
            MODEL_METADATA_KEY.to_string(),
            serde_json::to_value(&model.metadata)?,
        );
        nodes.push(dnode);

        for (rkey, req) in &domain.requirements {
            let rid = format!("{dname}/{rkey}");
            let rsym = synthetic_symbol("requirement", &rid);
            let mut rnode = mk(
                "requirement",
                &rid,
                NodeKind::Other("requirement".to_string()),
                &req.title,
            );
            rnode.metadata.insert(
                "description".to_string(),
                serde_json::Value::String(req.description.clone()),
            );
            if let Some(s) = &req.status {
                rnode
                    .metadata
                    .insert("status".to_string(), serde_json::Value::String(s.clone()));
            }
            // The raw code links are kept as provenance regardless of which entries resolve; the
            // resolve-to-SymbolId + SatisfiedBy edge emission happens just below.
            if let Some(code) = &req.code_symbol {
                // The builder knows exactly which code: link it, no name lookup.
                rnode.metadata.insert(
                    "code_symbol".to_string(),
                    serde_json::Value::String(code.clone()),
                );
                edges.push(Edge::new(
                    rsym.clone(),
                    wicked_apps_core::SymbolId::from(code.as_str()),
                    EdgeKind::SatisfiedBy,
                    contains,
                    "domain-graph",
                ));
            }
            if !req.legacy_components.is_empty() {
                rnode.metadata.insert(
                    "legacy_components".to_string(),
                    serde_json::json!(req.legacy_components),
                );
                // Resolve each `file#name` to a code SymbolId and emit requirement --SatisfiedBy--> code
                // as a first-class estate edge. Split from the RIGHT (a symbol name never contains '#').
                // An entry that resolves to no code node in this graph gets NO edge (never a dangling
                // one) and needs no separate record: it stays in the raw `legacy_components` metadata
                // above, so drift is derivable as the entries with no SatisfiedBy edge, and it is
                // re-resolved on the next persist relink once the code is indexed. A store read error
                // fails the whole persist closed — a partial satisfaction graph is worse than none.
                for comp in req
                    .legacy_components
                    .iter()
                    .filter(|_| req.code_symbol.is_none())
                {
                    // BOTH sides must be non-empty: an empty file part (e.g. "#parse") would make
                    // `n.location.file.ends_with(file)` trivially true and wrongly satisfy the
                    // requirement from an arbitrary same-named symbol.
                    let Some((file, name)) = comp
                        .rsplit_once('#')
                        .filter(|(f, n)| !f.is_empty() && !n.is_empty())
                    else {
                        continue;
                    };
                    let hits = store.find_symbols(&SymbolQuery {
                        exact_name: Some(name.to_string()),
                        ..Default::default()
                    })?;
                    // Prefer an exact file match; fall back to a path-suffix match (the extractor's
                    // relative path vs the graph's canonical path).
                    if let Some(code) =
                        hits.iter().find(|n| n.location.file == file).or_else(|| {
                            hits.iter().find(|n| {
                                n.location.file.ends_with(file)
                                    || file.ends_with(n.location.file.as_str())
                            })
                        })
                    {
                        // A first-class `EdgeKind::SatisfiedBy` (estate-core 0.16.7) — the requirements
                        // overlay is a real consumer that filters traversal by this kind, mirroring the
                        // `Governs` rule→code precedent.
                        edges.push(Edge::new(
                            rsym.clone(),
                            code.symbol.clone(),
                            EdgeKind::SatisfiedBy,
                            inferred,
                            "domain-graph",
                        ));
                    }
                }
            }
            // Dependencies are part of the wire contract and can name external services or
            // cross-domain keys that have no node here. Preserve the raw list on the node so the
            // information survives regardless of which entries become `References` edges below.
            if !req.dependencies.is_empty() {
                rnode.metadata.insert(
                    "dependencies".to_string(),
                    serde_json::json!(req.dependencies),
                );
            }
            // Corroboration rides on the node too, so a graph query sees validated vs unconfirmed.
            if let Some(c) = &req.corroboration {
                if let Ok(v) = serde_json::to_value(c) {
                    rnode.metadata.insert("corroboration".to_string(), v);
                }
            }
            // The whole requirement as the build wrote it (rule provenance, validations, error
            // paths …): what `load_domain_graph` returns, so a read is the build's document.
            rnode
                .metadata
                .insert(REQUIREMENT_DOC_KEY.to_string(), serde_json::to_value(req)?);
            nodes.push(rnode);

            // domain CONTAINS requirement
            edges.push(Edge::new(
                dsym.clone(),
                rsym.clone(),
                EdgeKind::Contains,
                contains,
                "domain-graph",
            ));

            // requirement REFERENCES its IN-DOMAIN dependency requirements. A dependency naming an
            // external service or a cross-domain key has no requirement node here; an edge to
            // `{dname}/{dep}` would dangle, so those survive only in the metadata above.
            for dep in &req.dependencies {
                if domain.requirements.contains_key(dep) {
                    edges.push(Edge::new(
                        rsym.clone(),
                        synthetic_symbol("requirement", &format!("{dname}/{dep}")),
                        EdgeKind::References,
                        inferred,
                        "domain-graph",
                    ));
                }
            }

            // business rules become Rule nodes; requirement CONTAINS each. Rule ids are unique only
            // WITHIN a requirement (domain-model schema), so the node id is scoped by its owning
            // requirement — otherwise two requirements' `RULE-001` collide onto a single node.
            for rule in &req.business_rules {
                let ruid = format!("{rid}/{}", rule.id);
                let bsym = synthetic_symbol("rule", &ruid);
                let mut bnode = mk("rule", &ruid, NodeKind::Rule, &rule.statement);
                bnode
                    .metadata
                    .insert("confidence".to_string(), serde_json::json!(rule.confidence));
                bnode.metadata.insert(
                    "provenance".to_string(),
                    serde_json::to_value(&rule.provenance)?,
                );
                nodes.push(bnode);
                edges.push(Edge::new(
                    rsym.clone(),
                    bsym,
                    EdgeKind::Contains,
                    contains,
                    "domain-graph",
                ));
            }
        }
    }

    // The store holds THIS build of the graph and nothing older: every domain, requirement and rule
    // node an earlier build wrote is removed with ALL its edges, then this build is written, in one
    // batch. A kept requirement keeps its id ([`stabilize_requirement_ids`]) but never an edge the
    // new build did not state (a `SatisfiedBy` to code it no longer covers, a dropped dependency).
    let stale: Vec<String> = previous_graph_nodes(&*store)?
        .into_iter()
        .map(|n| n.location.file)
        .collect();
    // The build record: which build this is (a reader compares it before and after), the model's
    // metadata (so an EMPTY build still reads back as built), and every requirement number each
    // domain has ever used (so a retired id is never handed to different code, even after builds
    // that dropped it or the whole domain).
    let previous = read_build_record(&*store)?;
    // Every number any previous build used — the record's high-water marks AND the requirement
    // nodes about to be replaced (a graph persisted before the record existed has only those) —
    // so a number retired by THIS build stays retired.
    let mut high: std::collections::BTreeMap<String, u32> =
        previous_req_high(&*store)?.into_iter().collect();
    for (dname, domain) in &model.domains {
        for key in domain.requirements.keys() {
            if let Some(n) = req_number(key) {
                let h = high.entry(dname.clone()).or_insert(0);
                *h = (*h).max(n);
            }
        }
    }
    let mut record = mk(
        BUILD_SCHEME,
        BUILD_ID,
        NodeKind::Other("requirements_graph".to_string()),
        "requirements graph",
    );
    record.metadata.insert(
        "generation".to_string(),
        serde_json::json!(previous.generation + 1),
    );
    record.metadata.insert(
        MODEL_METADATA_KEY.to_string(),
        serde_json::to_value(&model.metadata)?,
    );
    record
        .metadata
        .insert("req_high".to_string(), serde_json::to_value(&high)?);
    nodes.push(record);

    // Atomic: the nodes and their edges land together or not at all. Without the batch, an
    // `upsert_edges` failure after `upsert_nodes` succeeded would leave the store holding a partial
    // domain graph even though the command fails closed — a persisted-but-incomplete graph is worse
    // than none. Matches the `begin_batch`/`commit_batch` write path every other store write uses.
    store.begin_batch()?;
    if let Some(expected) = expected_generation {
        let now = read_build_record(&*store)?.generation;
        if now != expected {
            anyhow::bail!(
                "another requirements-graph build committed while this one was prepared (store \
                 generation {now}, expected {expected}); nothing was written — run domain-graph again"
            );
        }
    }
    for file in &stale {
        // A synthetic node's location is its own `<scheme>/<id>` address: removing that "file"
        // removes exactly that node and its edges.
        store.remove_file(file)?;
    }
    store.upsert_nodes(&nodes)?;
    store.upsert_edges(&edges)?;
    store.commit_batch()?;
    Ok((nodes.len(), edges.len()))
}

/// The location prefixes [`persist_domain_model`] gives its nodes (`<scheme>/<id>`).
const DOMAIN_FILE_PREFIX: &str = "domain/";
const REQUIREMENT_FILE_PREFIX: &str = "requirement/";
const RULE_FILE_PREFIX: &str = "rule/";

/// Every node a previous [`persist_domain_model`] wrote: the wicked-apps synthetic domain,
/// requirement and business-rule nodes (a conformance rule or an extracted rules-engine `Rule` is
/// another scheme or location and is never touched).
fn previous_graph_nodes(store: &dyn GraphRead) -> anyhow::Result<Vec<wicked_apps_core::Node>> {
    use wicked_apps_core::{NodeKind, SymbolQuery};
    let kinds = vec![
        NodeKind::Other("domain".to_string()),
        NodeKind::Other("requirement".to_string()),
        NodeKind::Rule,
    ];
    let nodes = store.find_symbols(&SymbolQuery {
        kinds,
        ..Default::default()
    })?;
    Ok(nodes
        .into_iter()
        .filter(|n| wicked_apps_core::is_apps_synthetic_symbol(&n.symbol))
        .filter(|n| {
            let f = n.location.file.as_str();
            f.starts_with(DOMAIN_FILE_PREFIX)
                || f.starts_with(REQUIREMENT_FILE_PREFIX)
                || f.starts_with(RULE_FILE_PREFIX)
        })
        .collect())
}

/// The build record [`persist_domain_model`] writes beside the graph (`requirements-graph/build`).
const BUILD_SCHEME: &str = "requirements-graph";
const BUILD_ID: &str = "build";

/// What the build record says: the build's generation (0 = never built), the model's metadata,
/// and the highest requirement number each domain has used.
#[derive(Default)]
struct BuildRecord {
    exists: bool,
    generation: u64,
    metadata: Option<serde_json::Value>,
    req_high: std::collections::BTreeMap<String, u32>,
}

fn read_build_record(store: &dyn GraphRead) -> anyhow::Result<BuildRecord> {
    let sym = wicked_apps_core::synthetic_symbol(BUILD_SCHEME, BUILD_ID);
    let Some(node) = store.get_node(&sym)? else {
        return Ok(BuildRecord::default());
    };
    Ok(BuildRecord {
        exists: true,
        generation: node
            .metadata
            .get("generation")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        metadata: node.metadata.get(MODEL_METADATA_KEY).cloned(),
        req_high: node
            .metadata
            .get("req_high")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default(),
    })
}

/// (domain-graph-3) The repository's OWN coverage target, beside the build record
/// (`requirements-graph/target`): set when the operator lowers the target for this repository at a
/// generation's budget ask, so every reader of this store — `wicked-core coverage`, `domain-graph`
/// and the requirements read — holds the graph to it before the daemon-wide minimum.
const TARGET_ID: &str = "target";
const TARGET_KEY: &str = "min_coverage_pct";

/// The repository's own coverage target, if one is set in its store (a whole percentage 1..=100;
/// anything else stored there is ignored, never trusted).
pub fn repo_min_coverage_pct(store: &dyn GraphRead) -> anyhow::Result<Option<u8>> {
    let sym = wicked_apps_core::synthetic_symbol(BUILD_SCHEME, TARGET_ID);
    Ok(store.get_node(&sym)?.and_then(|n| {
        n.metadata
            .get(TARGET_KEY)
            .and_then(|v| v.as_u64())
            .and_then(|v| u8::try_from(v).ok())
            .filter(|v| (1..=100).contains(v))
    }))
}

/// Set (`Some`) or clear (`None`) the repository's own coverage target in its store. A value
/// outside 1..=100 is refused.
pub fn set_repo_min_coverage_pct(
    store: &mut dyn wicked_apps_core::GraphStore,
    pct: Option<u8>,
) -> anyhow::Result<()> {
    use wicked_apps_core::{
        synthetic_symbol, Language, Location, Node, NodeKind, Span, SYMBOL_SCHEME,
    };
    let file = format!("{BUILD_SCHEME}/{TARGET_ID}");
    let Some(pct) = pct else {
        store.begin_batch()?;
        store.remove_file(&file)?;
        store.commit_batch()?;
        return Ok(());
    };
    if !(1..=100).contains(&pct) {
        anyhow::bail!("a coverage target is a whole percentage 1..100, not {pct}");
    }
    let mut node = Node::new(
        synthetic_symbol(BUILD_SCHEME, TARGET_ID),
        NodeKind::Other("requirements_target".to_string()),
        "requirements graph target".to_string(),
        Language::new(SYMBOL_SCHEME),
        Location::new(file, Span::ZERO),
    );
    node.metadata
        .insert(TARGET_KEY.to_string(), serde_json::json!(pct));
    store.begin_batch()?;
    store.upsert_nodes(&[node])?;
    store.commit_batch()?;
    Ok(())
}

/// The minimum this store's graph is held to: the repository's own target when one is set
/// ([`repo_min_coverage_pct`]), else the daemon-wide setting ([`min_coverage_pct_from_env`]).
pub fn effective_min_coverage_pct(store: &dyn GraphRead) -> Result<u8, String> {
    match repo_min_coverage_pct(store) {
        Ok(Some(pct)) => Ok(pct),
        Ok(None) => min_coverage_pct_from_env(),
        Err(e) => Err(format!(
            "the repository's coverage target could not be read: {e}"
        )),
    }
}

/// The highest requirement number each domain has used: the build record's marks merged with the
/// numbers of the requirement nodes in the store (a graph persisted before the record existed
/// carries only those).
fn previous_req_high(
    store: &dyn GraphRead,
) -> anyhow::Result<std::collections::HashMap<String, u32>> {
    let mut high: std::collections::HashMap<String, u32> =
        read_build_record(store)?.req_high.into_iter().collect();
    let nodes = previous_graph_nodes(store)?;
    for (dname, rkey, _) in requirement_ids(&nodes) {
        if let Some(num) = req_number(rkey) {
            let h = high.entry(dname.to_string()).or_insert(0);
            *h = (*h).max(num);
        }
    }
    Ok(high)
}

/// `(domain, requirement id, node)` for every requirement node among `nodes`, the domain being the
/// longest known domain name its id path starts with (names may hold '/').
fn requirement_ids(nodes: &[wicked_apps_core::Node]) -> Vec<(&str, &str, &wicked_apps_core::Node)> {
    let domains: std::collections::HashSet<&str> = nodes
        .iter()
        .filter(|n| n.location.file.starts_with(DOMAIN_FILE_PREFIX))
        .map(|n| n.name.as_str())
        .collect();
    let mut out = Vec::new();
    for n in nodes
        .iter()
        .filter(|n| n.location.file.starts_with(REQUIREMENT_FILE_PREFIX))
    {
        let rest = &n.location.file[REQUIREMENT_FILE_PREFIX.len()..];
        let Some(dname) = domains
            .iter()
            .filter(|d| rest.starts_with(&format!("{d}/")))
            .max_by_key(|d| d.len())
        else {
            continue;
        };
        out.push((*dname, &rest[dname.len() + 1..], n));
    }
    out
}

/// Node-metadata keys [`persist_domain_model`] writes the document under.
const DOMAIN_DOC_KEY: &str = "domain_document";
const REQUIREMENT_DOC_KEY: &str = "requirement_document";
const MODEL_METADATA_KEY: &str = "model_metadata";

/// Keep each requirement's id across builds. The builder numbers requirements by their code's
/// position in the domain (`REQ-nnn` by slot), so deleting or inserting a behaviour ahead of others
/// would shift every later id onto different code — and crew's overrides, keyed by id, with it.
/// This renames the requirements of `model` to the id the PREVIOUS build in `store` gave the same
/// code (the first `legacy_components` entry, `file#name`, in the same domain); a requirement new
/// to the domain takes a number above every id the domain has EVER used (the build record keeps
/// the high-water mark across builds that dropped an id or the whole domain), so a retired id is
/// never reused for different code. In-domain `dependencies` follow the renames. Deterministic.
pub fn stabilize_requirement_ids(
    store: &dyn GraphRead,
    model: &mut DomainModel,
) -> anyhow::Result<u64> {
    use std::collections::{BTreeMap, HashMap, HashSet};
    let generation = read_build_record(store)?.generation;
    let high = previous_req_high(store)?;
    // (domain, key) -> previous id. Keyed by the code's SymbolId when the build recorded it
    // (unique); else by its first `file#name` (a build persisted before the SymbolId rode on the
    // node) — and a `file#name` two previous requirements shared (`A::new`, `B::new` in one file)
    // proves nothing about which is which, so it keeps NO id: both take fresh ones, and any
    // override on the old ids shows as orphaned instead of moving to the other behaviour.
    let nodes = previous_graph_nodes(store)?;
    let mut previous: HashMap<(String, String), String> = HashMap::new();
    let mut ambiguous: HashSet<(String, String)> = HashSet::new();
    for (dname, rkey, n) in requirement_ids(&nodes) {
        let key = n
            .metadata
            .get("code_symbol")
            .and_then(|v| v.as_str())
            .map(|c| format!("sym:{c}"))
            .or_else(|| {
                n.metadata
                    .get("legacy_components")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                    .map(|c| format!("ref:{c}"))
            });
        if let Some(key) = key {
            let k = (dname.to_string(), key);
            if previous.insert(k.clone(), rkey.to_string()).is_some() {
                ambiguous.insert(k);
            }
        }
    }
    for k in &ambiguous {
        previous.remove(k);
    }
    for (dname, domain) in model.domains.iter_mut() {
        let old = std::mem::take(&mut domain.requirements);
        // A `file#name` two requirements of THIS build share proves nothing either: the old id
        // could be either one's. Only a reference unique on both sides carries an id over.
        let mut refs_now: HashMap<&str, usize> = HashMap::new();
        for req in old.values() {
            if let Some(c) = req.legacy_components.first() {
                *refs_now.entry(c.as_str()).or_insert(0) += 1;
            }
        }
        let shared_now: HashSet<String> = refs_now
            .into_iter()
            .filter(|(_, n)| *n > 1)
            .map(|(c, _)| c.to_string())
            .collect();
        let mut next = high.get(dname.as_str()).copied().unwrap_or(0);
        let mut taken: HashSet<String> = HashSet::new();
        let mut renamed: BTreeMap<String, Requirement> = BTreeMap::new();
        let mut map: HashMap<String, String> = HashMap::new();
        let mut fresh: Vec<(String, Requirement)> = Vec::new();
        for (key, req) in old {
            let by_symbol = req
                .code_symbol
                .as_ref()
                .and_then(|c| previous.get(&(dname.clone(), format!("sym:{c}"))));
            let by_ref = req
                .legacy_components
                .first()
                .filter(|c| !shared_now.contains(*c))
                .and_then(|c| previous.get(&(dname.clone(), format!("ref:{c}"))));
            let kept = by_symbol
                .or(by_ref)
                .filter(|id| !taken.contains(*id))
                .cloned();
            match kept {
                Some(id) => {
                    taken.insert(id.clone());
                    map.insert(key, id.clone());
                    renamed.insert(id, req);
                }
                None => fresh.push((key, req)),
            }
        }
        for (key, req) in fresh {
            let id = loop {
                next += 1;
                let id = format!("REQ-{next:03}");
                if !taken.contains(&id) {
                    break id;
                }
            };
            taken.insert(id.clone());
            map.insert(key, id.clone());
            renamed.insert(id, req);
        }
        for req in renamed.values_mut() {
            for dep in req.dependencies.iter_mut() {
                if let Some(id) = map.get(dep.as_str()) {
                    *dep = id.clone();
                }
            }
        }
        domain.requirements = renamed;
    }
    Ok(generation)
}

/// The number of a `REQ-nnn` id.
fn req_number(key: &str) -> Option<u32> {
    key.strip_prefix("REQ-").and_then(|n| n.parse().ok())
}

/// The requirements graph as the estate store holds it — the source of truth
/// [`persist_domain_model`] writes — read back as the domain-model document that build produced
/// (`metadata` + `domains{ name: { …, requirements{ REQ-id: Requirement }, entities } }`), so it
/// deserializes as [`DomainModel`] and validates against the domain-model schema. The
/// requirement → code links are the store's `SatisfiedBy` edges (graph queries read them there);
/// the document carries them as `legacy_components`, as the schema says. `None` when no graph has
/// been built into this store.
///
/// `coverage`, when given, replaces the build's `metadata.coverage`: the label computed by the
/// caller from the store's OWN counts at read time ([`model_coverage`]), so a later run that maps
/// more of the repository shows without a rebuild (not on a 1.0.0 document, whose schema has no
/// such field). An EMPTY build reads back as a built graph with no domains; `None` means never
/// built. Read-only; deterministic order. A read during which a rebuild committed (the build
/// record's generation moved, or an edge led to a node it had not listed) starts over, twice at
/// most.
pub fn load_domain_graph(
    store: &dyn GraphRead,
    coverage: Option<&ModelCoverage>,
) -> anyhow::Result<Option<serde_json::Value>> {
    let Some((mut graph, _)) = load_domain_graph_with_build(store)? else {
        return Ok(None);
    };
    if let Some(c) = coverage {
        label_domain_graph(&mut graph, c)?;
    }
    Ok(Some(graph))
}

/// Put `coverage` on a read graph's `metadata.coverage`, replacing the build's — except on a 1.0.0
/// document, whose schema has no such field (the caller carries the label beside the graph either way).
pub fn label_domain_graph(
    graph: &mut serde_json::Value,
    coverage: &ModelCoverage,
) -> anyhow::Result<()> {
    let label = serde_json::to_value(coverage)?;
    if let Some(metadata) = graph.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        if metadata.get("schema_version").and_then(|v| v.as_str()) != Some("1.0.0") {
            metadata.insert("coverage".into(), label);
        }
    }
    Ok(())
}

/// Which build a [`load_domain_graph`] read returned: the build record's generation and the coverage
/// label that build persisted, verbatim (`None` when the build carried none, e.g. a 1.0.0 document).
/// Once a caller labels the read with the store's counts now ([`label_domain_graph`]), this is the only
/// place a reader learns what the listed graph was built at, and so whether the header's counts are
/// the rows' (req-list-stale 2026-11-01: a list of the 04:59 build under a header counting today's
/// store). `generation` is 0 for a graph persisted before the build record existed.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DomainGraphBuild {
    pub generation: u64,
    pub coverage: Option<serde_json::Value>,
}

/// [`load_domain_graph`] as the build wrote it (its own `metadata.coverage`), with the build it read:
/// taken from the same build record the read checked for a concurrent rebuild, so the label always
/// belongs to the rows returned beside it.
pub fn load_domain_graph_with_build(
    store: &dyn GraphRead,
) -> anyhow::Result<Option<(serde_json::Value, DomainGraphBuild)>> {
    Ok(load_domain_graph_with_units(store)?.map(|(graph, build, _)| (graph, build)))
}

/// Each listed requirement's code unit, by domain then requirement id: the `code_symbol` its build
/// recorded. A requirement persisted without one (a build from before the field) is absent.
pub type RequirementUnits =
    std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>;

/// [`load_domain_graph_with_build`], with the code unit behind each requirement read
/// ([`RequirementUnits`]), from the same read: what [`recompute_front_half_coverage_labelling`] needs to
/// say which listed rows the share does not count.
pub fn load_domain_graph_with_units(
    store: &dyn GraphRead,
) -> anyhow::Result<Option<(serde_json::Value, DomainGraphBuild, RequirementUnits)>> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match load_domain_graph_once(store)? {
            Loaded::Torn if attempt < 3 => continue,
            Loaded::Torn => anyhow::bail!(
                "the requirements graph changed while it was read (a build was writing it); read again"
            ),
            Loaded::Graph(g) => return Ok(g),
        }
    }
}

/// Listed requirements the coverage share leaves out, by domain then requirement id, with why.
pub type UncountedRequirements =
    std::collections::BTreeMap<String, std::collections::BTreeMap<String, Uncounted>>;

/// [`recompute_front_half_coverage`], plus which of the listed requirements (`units`, from
/// [`load_domain_graph_with_units`]) sit on a unit the share does not count — decided in the same pass
/// that counts the header's numbers, so the rows NOT returned are exactly the requirements on
/// behaviour-bearing product units (req-reconcile 2026-11-01: wicked-crew listed 1,458 rows on
/// TypeScript shapes under a header that, since core#1238, counts none of them). A unit the store no
/// longer holds is [`Uncounted::Unindexed`].
pub fn recompute_front_half_coverage_labelling(
    store: &dyn GraphRead,
    units: &RequirementUnits,
) -> anyhow::Result<(CoverageReport, UncountedRequirements)> {
    let wanted: std::collections::HashSet<&str> = units
        .values()
        .flat_map(|reqs| reqs.values().map(String::as_str))
        .collect();
    let mut seen: std::collections::HashMap<String, UnitClass> = std::collections::HashMap::new();
    let report = recompute_observing(store, &CoverageConfig::default(), &mut |sym, class| {
        if wanted.contains(sym) {
            seen.insert(sym.to_string(), class);
        }
    })?;
    let mut out = UncountedRequirements::new();
    for (domain, reqs) in units {
        for (req, sym) in reqs {
            let why = match seen.get(sym.as_str()) {
                Some(UnitClass::Counted) => continue,
                Some(UnitClass::Uncounted(why)) => *why,
                None => Uncounted::Unindexed,
            };
            out.entry(domain.clone())
                .or_default()
                .insert(req.clone(), why);
        }
    }
    Ok((report, out))
}

enum Loaded {
    Graph(Option<(serde_json::Value, DomainGraphBuild, RequirementUnits)>),
    /// An edge pointed at a node the read had not listed.
    Torn,
}

fn load_domain_graph_once(store: &dyn GraphRead) -> anyhow::Result<Loaded> {
    use serde_json::{json, Map, Value};
    use wicked_apps_core::EdgeKind;
    use wicked_estate_core::Direction;

    let record = read_build_record(store)?;
    let nodes = previous_graph_nodes(store)?;
    let mut domains: Vec<&wicked_apps_core::Node> = nodes
        .iter()
        .filter(|n| n.location.file.starts_with(DOMAIN_FILE_PREFIX))
        .collect();
    if domains.is_empty() && !record.exists {
        return Ok(Loaded::Graph(None));
    }
    domains.sort_by(|a, b| a.name.cmp(&b.name));
    let by_symbol: std::collections::HashMap<&str, &wicked_apps_core::Node> =
        nodes.iter().map(|n| (n.symbol.as_str(), n)).collect();

    let metadata = record
        .metadata
        .clone()
        .or_else(|| {
            domains
                .iter()
                .find_map(|d| d.metadata.get(MODEL_METADATA_KEY).cloned())
        })
        .unwrap_or_else(|| {
            json!({ "schema_version": "1.1.0", "migration_mode": "functional",
                    "source": "estate-graph" })
        });
    let mut out_domains = Map::new();
    let mut units = RequirementUnits::new();
    for d in domains {
        let dname = d.name.clone();
        let req_prefix = format!("{REQUIREMENT_FILE_PREFIX}{dname}/");
        let mut reqs: Vec<(String, &wicked_apps_core::Node)> = Vec::new();
        for e in store.neighbors(&d.symbol, Direction::Dependencies)? {
            if e.kind != EdgeKind::Contains {
                continue;
            }
            let Some(r) = by_symbol.get(e.target.as_str()) else {
                return Ok(Loaded::Torn);
            };
            if let Some(key) = r.location.file.strip_prefix(req_prefix.as_str()) {
                reqs.push((key.to_string(), r));
            }
        }
        reqs.sort_by(|a, b| a.0.cmp(&b.0));
        reqs.dedup_by(|a, b| a.0 == b.0);
        let mut out_reqs = Map::new();
        for (key, r) in reqs {
            if let Some(sym) = r.metadata.get("code_symbol").and_then(Value::as_str) {
                units
                    .entry(dname.clone())
                    .or_default()
                    .insert(key.clone(), sym.to_string());
            }
            let doc = match r.metadata.get(REQUIREMENT_DOC_KEY) {
                Some(doc) => doc.clone(),
                // A graph persisted before the document rode on the node: rebuild what the nodes
                // and edges say.
                None => legacy_requirement(store, r, &by_symbol, &dname, &key)?,
            };
            out_reqs.insert(key, doc);
        }
        let mut dom = match d.metadata.get(DOMAIN_DOC_KEY) {
            Some(Value::Object(o)) => o.clone(),
            _ => {
                let mut o = Map::new();
                if let Some(v) = d.metadata.get("description").filter(|v| !v.is_null()) {
                    o.insert("description".into(), v.clone());
                }
                o.insert("entities".into(), json!({}));
                o
            }
        };
        dom.insert("requirements".into(), Value::Object(out_reqs));
        out_domains.insert(dname, Value::Object(dom));
    }
    // A build that changed under this read (its generation moved) is read again.
    if read_build_record(store)?.generation != record.generation {
        return Ok(Loaded::Torn);
    }
    // The graph as the build wrote it, its own label included; a caller that wants the store's counts
    // now puts them on with `label_domain_graph`.
    let build = DomainGraphBuild {
        generation: record.generation,
        coverage: metadata.get("coverage").filter(|v| !v.is_null()).cloned(),
    };
    Ok(Loaded::Graph(Some((
        json!({ "metadata": metadata, "domains": out_domains }),
        build,
        units,
    ))))
}

/// A requirement of a graph persisted without its document, rebuilt from its node, its rule nodes
/// and its metadata into the schema's shape (required arrays present, possibly empty).
fn legacy_requirement(
    store: &dyn GraphRead,
    r: &wicked_apps_core::Node,
    by_symbol: &std::collections::HashMap<&str, &wicked_apps_core::Node>,
    dname: &str,
    key: &str,
) -> anyhow::Result<serde_json::Value> {
    use serde_json::{json, Value};
    use wicked_apps_core::EdgeKind;
    use wicked_estate_core::Direction;
    let rule_prefix = format!("{RULE_FILE_PREFIX}{dname}/{key}/");
    let mut rules: Vec<(String, Value)> = Vec::new();
    for e in store.neighbors(&r.symbol, Direction::Dependencies)? {
        if e.kind != EdgeKind::Contains {
            continue;
        }
        if let Some(b) = by_symbol.get(e.target.as_str()) {
            if let Some(id) = b.location.file.strip_prefix(rule_prefix.as_str()) {
                let mut rule = json!({
                    "id": id,
                    "statement": b.name,
                    "confidence": b.metadata.get("confidence").cloned().unwrap_or(json!(0.0)),
                });
                // A graph persisted before rule provenance rode on the node: the rule was lifted
                // from the requirement's code, as the builder grounds every rule.
                let reference = r
                    .metadata
                    .get("legacy_components")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                    .unwrap_or(key);
                rule["provenance"] = b.metadata.get("provenance").cloned().unwrap_or_else(|| {
                    json!({ "source": "code-graph", "ref": reference,
                               "source_kinds": ["code-body"] })
                });
                rules.push((id.to_string(), rule));
            }
        }
    }
    rules.sort_by(|a, b| a.0.cmp(&b.0));
    let meta = |k: &str, default: Value| r.metadata.get(k).cloned().unwrap_or(default);
    let mut req = json!({
        "title": r.name,
        "description": meta("description", json!("")),
        "legacy_components": meta("legacy_components", json!([])),
        "data_access": [],
        "dependencies": meta("dependencies", json!([])),
        "business_rules": rules.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        "validations": [],
        "error_paths": [],
        "merged_programs": [],
    });
    for k in ["status", "corroboration"] {
        if let Some(v) = r.metadata.get(k).filter(|v| !v.is_null()) {
            req[k] = v.clone();
        }
    }
    Ok(req)
}

/// The coverage label for `report` against `min_pct`, from the store's own counts — the same
/// numbers the build and the coverage gate use (`metadata.coverage`).
#[must_use]
pub fn model_coverage(report: &CoverageReport, min_pct: u8) -> ModelCoverage {
    ModelCoverage {
        real_pct: real_pct(report),
        min_pct,
        behavior_bearing: report.behavior_bearing,
        real: report.resolved,
        placeholders: report.risk_flagged,
        unmapped: report.unaccounted,
        not_product: report.not_product,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The domain graph lands IN the estate store as queryable nodes + edges — not a JSON file.
    /// Falsifiable: delete the `upsert_edges` line in production and the Contains-edge assertion
    /// fails; delete `upsert_nodes` and the node lookups fail.
    /// domain-graph-3: the repository's own target lives in its store, beside the build record,
    /// and comes before the daemon-wide minimum; clearing it hands the bar back; a value outside
    /// 1..=100 is refused, and a persisted graph build never touches it.
    #[test]
    fn a_repositorys_own_target_is_kept_in_its_store_and_comes_first() {
        use wicked_apps_core::SqliteStore;
        let mut st = SqliteStore::in_memory().unwrap();
        assert_eq!(repo_min_coverage_pct(&st).unwrap(), None);
        set_repo_min_coverage_pct(&mut st, Some(23)).unwrap();
        assert_eq!(repo_min_coverage_pct(&st).unwrap(), Some(23));
        assert_eq!(effective_min_coverage_pct(&st), Ok(23));
        assert!(set_repo_min_coverage_pct(&mut st, Some(0)).is_err());
        assert!(set_repo_min_coverage_pct(&mut st, Some(101)).is_err());
        assert_eq!(
            repo_min_coverage_pct(&st).unwrap(),
            Some(23),
            "a refusal keeps it"
        );
        let model: DomainModel = serde_json::from_value(serde_json::json!({
            "domains": {},
            "metadata": {"schema_version": "1.0.0", "migration_mode": "functional"}
        }))
        .unwrap();
        persist_domain_model(&mut st, &model).unwrap();
        assert_eq!(
            repo_min_coverage_pct(&st).unwrap(),
            Some(23),
            "a build keeps the target"
        );
        set_repo_min_coverage_pct(&mut st, None).unwrap();
        assert_eq!(repo_min_coverage_pct(&st).unwrap(), None);
    }

    #[test]
    fn persist_domain_model_writes_the_graph_into_the_store() {
        use wicked_apps_core::{synthetic_symbol, EdgeKind, GraphRead, NodeKind, SqliteStore};

        // A rule whose id (`RULE-001`) is only locally unique — the schema allows the SAME id under
        // a different requirement. Both REQ-001 and REQ-002 carry `RULE-001`, so a persist that keys
        // the Rule node by `rule.id` alone collapses them onto one node. REQ-001 also depends on
        // REQ-002 (a real in-domain requirement → a live References edge) and on an EXTERNAL service
        // (no node → metadata only, never a dangling edge).
        let rule = |stmt: &str| Rule {
            id: "RULE-001".to_string(),
            statement: stmt.to_string(),
            confidence: 0.9,
            provenance: Provenance {
                source: "code-body".to_string(),
                reference: "a.py#parse".to_string(),
                source_kinds: vec!["code-body".to_string()],
            },
            source_ref: None,
        };
        let mut reqs = std::collections::BTreeMap::new();
        reqs.insert(
            "REQ-001".to_string(),
            Requirement {
                title: "Parse the page into blocks".to_string(),
                description: "The parser groups spans into ordered blocks".to_string(),
                dependencies: vec!["REQ-002".to_string(), "stripe-api".to_string()],
                legacy_components: vec!["a.py#parse".to_string()],
                business_rules: vec![rule("blocks preserve reading order")],
                ..Default::default()
            },
        );
        reqs.insert(
            "REQ-002".to_string(),
            Requirement {
                title: "Render blocks to markdown".to_string(),
                description: "Blocks serialize to a markdown outline".to_string(),
                business_rules: vec![rule("emphasized empty lines are not sections")],
                ..Default::default()
            },
        );
        let mut domains = std::collections::BTreeMap::new();
        domains.insert(
            "pageindex".to_string(),
            Domain {
                description: Some("core".to_string()),
                requirements: reqs,
                ..Default::default()
            },
        );
        let model = DomainModel {
            metadata: Metadata {
                schema_version: "1.0.0".to_string(),
                migration_mode: "functional".to_string(),
                source: None,
                coverage: None,
            },
            domains,
        };

        let mut st = SqliteStore::in_memory().unwrap();
        let (n, e) = persist_domain_model(&mut st, &model).unwrap();
        assert_eq!(
            n, 6,
            "one domain + two requirements + two DISTINCT rule nodes (ids scoped by requirement) + \
             the build record"
        );
        assert_eq!(
            e, 5,
            "2 domain→req Contains, 2 req→rule Contains, 1 in-domain req→dep References \
             (the external `stripe-api` dependency emits NO edge)"
        );

        let dsym = synthetic_symbol("domain", "pageindex");
        let rsym = synthetic_symbol("requirement", "pageindex/REQ-001");
        let rsym2 = synthetic_symbol("requirement", "pageindex/REQ-002");
        assert_eq!(
            st.get_node(&dsym)
                .unwrap()
                .expect("domain node in store")
                .kind,
            NodeKind::Other("domain".to_string())
        );

        // The collision falsifier: each requirement's `RULE-001` is a DISTINCT node under a
        // requirement-scoped id. Revert the scoping to `synthetic_symbol("rule", &rule.id)` and
        // these two lookups resolve to the same node, `n` drops to 4, and this fails.
        let rule1 = synthetic_symbol("rule", "pageindex/REQ-001/RULE-001");
        let rule2 = synthetic_symbol("rule", "pageindex/REQ-002/RULE-001");
        assert_ne!(
            rule1, rule2,
            "same local rule id under different requirements must not collide"
        );
        for rs in [&rule1, &rule2] {
            assert_eq!(
                st.get_node(rs)
                    .unwrap()
                    .expect("business-rule node in store")
                    .kind,
                NodeKind::Rule
            );
        }

        // The raw dependency list is preserved on the node, INCLUDING the external service that
        // never became an edge (finding: persist must not silently drop the wire-contract field).
        let deps = st
            .get_node(&rsym)
            .unwrap()
            .expect("requirement node")
            .metadata
            .get("dependencies")
            .cloned()
            .expect("dependencies preserved in metadata");
        assert_eq!(deps, serde_json::json!(["REQ-002", "stripe-api"]));

        let all = st.all_edges().unwrap();
        // The domain→requirement structural edge is queryable in the store.
        assert!(
            all.iter().any(|edge| edge.source == dsym
                && edge.target == rsym
                && edge.kind == EdgeKind::Contains),
            "domain Contains requirement edge is in the store, got {all:?}"
        );
        // The in-domain dependency became a References edge; the external one did NOT.
        assert!(
            all.iter().any(|edge| edge.source == rsym
                && edge.target == rsym2
                && edge.kind == EdgeKind::References),
            "in-domain REQ-001→REQ-002 References edge present, got {all:?}"
        );
        assert!(
            !all.iter().any(|edge| edge.kind == EdgeKind::References
                && edge.target == synthetic_symbol("requirement", "pageindex/stripe-api")),
            "external dependency must not emit a dangling References edge, got {all:?}"
        );
    }

    #[test]
    fn persist_wires_satisfied_by_edges_to_resolved_code_and_derives_drift() {
        use std::collections::BTreeMap;
        use wicked_apps_core::{
            synthetic_symbol, EdgeKind, GraphRead, GraphWrite, Language, Location, Node, NodeKind,
            Span, SqliteStore,
        };

        let mut st = SqliteStore::in_memory().unwrap();
        // Seed a REAL code node named `parse` at `a.py` so the requirement's `a.py#parse` resolves
        // through `find_symbols` — this is the keystone: a requirement wired to actual code, not an
        // orphan island.
        let code_sym = synthetic_symbol("py", "a.py:parse");
        let code = Node::new(
            code_sym.clone(),
            NodeKind::Other("function".to_string()),
            "parse".to_string(),
            Language::new("py"),
            Location::new("a.py".to_string(), Span::ZERO),
        );
        st.begin_batch().unwrap();
        st.upsert_nodes(&[code]).unwrap();
        st.commit_batch().unwrap();

        // REQ-001 names one RESOLVABLE component (a.py#parse) and one DANGLING one (ghost.py#nope).
        let mut reqs = BTreeMap::new();
        reqs.insert(
            "REQ-001".to_string(),
            Requirement {
                title: "Parse".to_string(),
                description: "parse the page".to_string(),
                legacy_components: vec![
                    "a.py#parse".to_string(),
                    "ghost.py#nope".to_string(),
                    "#parse".to_string(),
                ],
                ..Default::default()
            },
        );
        let mut domains = BTreeMap::new();
        domains.insert(
            "pageindex".to_string(),
            Domain {
                requirements: reqs,
                ..Default::default()
            },
        );
        let model = DomainModel {
            metadata: Metadata {
                schema_version: "1.0.0".to_string(),
                migration_mode: "functional".to_string(),
                source: None,
                coverage: None,
            },
            domains,
        };

        persist_domain_model(&mut st, &model).unwrap();

        let rsym = synthetic_symbol("requirement", "pageindex/REQ-001");
        let all = st.all_edges().unwrap();
        let satisfied = EdgeKind::SatisfiedBy;
        // The resolvable component becomes requirement --SatisfiedBy--> code (source = requirement,
        // target = code: the dependent→dependency invariant, so BlastRadius(code) surfaces the
        // requirement a change may break).
        assert!(
            all.iter()
                .any(|e| e.source == rsym && e.target == code_sym && e.kind == satisfied),
            "requirement --SatisfiedBy--> code edge for the resolvable component, got {all:?}"
        );
        // Exactly one SatisfiedBy edge — the dangling `ghost.py#nope` NEVER emits a dangling edge.
        assert_eq!(
            all.iter().filter(|e| e.kind == satisfied).count(),
            1,
            "the dangling component must NOT emit an edge, got {all:?}"
        );
        // Drift is DERIVABLE — no invented parallel field. The raw provenance stays on the node, and
        // the unresolved entry is simply the one with no SatisfiedBy edge (re-tried next relink).
        let node = st.get_node(&rsym).unwrap().expect("requirement node");
        assert!(
            node.metadata.get("satisfied_by_unresolved").is_none(),
            "no invented drift field — drift derives from legacy_components minus SatisfiedBy edges"
        );
        assert_eq!(
            node.metadata.get("legacy_components").cloned(),
            Some(serde_json::json!(["a.py#parse", "ghost.py#nope", "#parse"])),
            "raw legacy_components provenance is preserved on the node — including the malformed \
             empty-file `#parse`, which is kept as provenance but (see the count above) emits NO edge"
        );
    }

    use wicked_estate_core::ValidationClaim;

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

    /// Operator ruling 2026-10-31: the gate is the CONFIGURED MINIMUM (default 80%), compared in
    /// exact integers, and the refusal says how far the mapping got. Operator decision 2026-11-01
    /// (core#1233): it counts REAL requirements only — risk-flagged placeholders never count.
    #[test]
    fn the_coverage_gate_counts_real_requirements_only() {
        let report = |bb: u64, resolved: u64, risk: u64| CoverageReport {
            behavior_bearing: bb,
            resolved,
            risk_flagged: risk,
            unaccounted: bb - resolved - risk,
            ..Default::default()
        };
        // pi-governance before this change: 3 real + 117 placeholders read "100% mapped". Now it
        // is 2%, and the placeholders do not lift it.
        let r = report(120, 3, 117);
        assert_eq!(real_pct(&r), 2);
        assert!(!meets_minimum(&r, 80));
        // 80 real of 100 meets 80; 60 real + 20 placeholders does not (it did before).
        assert!(meets_minimum(&report(100, 80, 0), 80));
        let r = report(100, 60, 20);
        assert_eq!(real_pct(&r), 60);
        assert!(!meets_minimum(&r, 80));
        assert!(assert_front_half_coverage_min(&r, 60).is_ok());
        let err = assert_front_half_coverage_min(&r, 80)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "60% real requirements, minimum 80% (60 of 100 behaviours; 20 placeholders, 20 unmapped)"
            ) && err.contains("kept in the estate store"),
            "{err}"
        );
        // Floored, never rounded up past the bar: 7999/10000 is 79%, below 80.
        let r = report(10_000, 7_999, 0);
        assert_eq!(real_pct(&r), 79);
        assert!(!meets_minimum(&r, 80));
        // 100 is exactly "every behaviour is a real requirement": one hole or one placeholder in
        // 20k denies it.
        assert!(!meets_minimum(&report(20_000, 19_999, 0), 100));
        assert!(!meets_minimum(&report(20_000, 19_999, 1), 100));
        assert!(assert_front_half_coverage(&report(3, 3, 0)).is_ok());
        assert!(assert_front_half_coverage(&report(3, 2, 1)).is_err());
        // An inconsistent report can only read worse (xreview on core#1215, agy P3): holes beyond
        // a too-small `behavior_bearing` still count, and 100 still means zero holes.
        let lying = CoverageReport {
            behavior_bearing: 0,
            unaccounted: 1,
            ..Default::default()
        };
        assert!(!meets_minimum(&lying, 100) && !meets_minimum(&lying, 80));
        let lying = CoverageReport {
            behavior_bearing: 1,
            resolved: 1,
            risk_flagged: 3,
            ..Default::default()
        };
        assert_eq!(real_pct(&lying), 25);
        // Buckets that overflow u64 when summed still read worse, never wrap past the bar (codex
        // on #1234): a hole at 100% denies.
        let huge = CoverageReport {
            behavior_bearing: u64::MAX,
            resolved: u64::MAX,
            unaccounted: 1,
            ..Default::default()
        };
        assert!(!meets_minimum(&huge, 100));
        assert!(real_pct(&huge) <= 100);
        let err = assert_front_half_coverage_min(
            &CoverageReport {
                risk_flagged: u64::MAX,
                unaccounted: 1,
                ..Default::default()
            },
            80,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("18446744073709551616 behavior-bearing"),
            "{err}"
        );
        // The empty graph stays a vacuous pass.
        assert!(meets_minimum(&report(0, 0, 0), 80));
        assert_eq!(real_pct(&report(0, 0, 0)), 100);
        // The label carries the one gated number and the placeholders as their own count.
        let label = model_coverage(&report(100, 60, 20), 80);
        assert_eq!(
            (
                label.real_pct,
                label.real,
                label.placeholders,
                label.unmapped
            ),
            (60, 60, 20, 20)
        );
        // The setting's parse: whole percentages 1..=100 only.
        assert_eq!(parse_min_coverage_pct(" 80 "), Some(80));
        assert_eq!(parse_min_coverage_pct("75%"), Some(75));
        for bad in ["0", "101", "eighty", "80.5", ""] {
            assert_eq!(parse_min_coverage_pct(bad), None, "{bad}");
        }
    }

    /// Operator decision 2026-11-01: tests, data, manifests and documents are not behaviour-bearing.
    /// pi-governance's 120 "behaviours" were 109 JSON keys (indexed as `Struct`), 8 test functions
    /// and 3 product functions.
    #[test]
    fn only_product_files_are_behaviour_bearing() {
        for file in [
            "extensions/pi-governance.js",
            "src/core/run.ts",
            "crates/x/src/lib.rs",
            "app/Contest.py",
            "src/latest/handler.go",
            "src/Payment.php",
            "src/payment.cpp",
            "",
        ] {
            assert!(is_product_file(file), "{file} is product code");
        }
        for file in [
            "evidence/pi-0.84.2/four-gate-allow/summary.json",
            "package.json",
            "README.md",
            "config/app.YAML",
            "Cargo.lock",
            "test/live/capture.mjs",
            "tests/coverage_cli.rs",
            "packages/crew/tests/requirements.test.ts",
            "src/__tests__/a.tsx",
            "src/api/run.test.ts",
            "src/api/run.spec.js",
            "pkg/test_run.py",
            "pkg/conftest.py",
            "pkg/run_test.go",
            "src/payment_test.cpp",
            "src/payment_test.cc",
            "src/PaymentTest.php",
            "spec/run_spec.rb",
            "Src/Tests/RunTests.cs",
            "src\\Tests\\Run.cs",
            "e2e/journey.ts",
            "fixtures/repo/main.ts",
        ] {
            assert!(!is_product_file(file), "{file} is not product code");
        }
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
            err.contains("2 unmapped") && err.contains("refund"),
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
            err.contains("1 unmapped") && err.contains("hole"),
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
                Some(&ValidationClaim::new(true, "test-fixture").unwrap()),
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
        // One real requirement of three behaviours (two placeholders): built at a 33% minimum,
        // because placeholders never count toward it (core#1233) — and the build still KEEPS them.
        let opts = DomainBuildOptions {
            min_coverage_pct: 33,
            repo_root: None,
        };
        let model = build_domain_model_with(&store, &coverage, "1.0.0", &opts).unwrap();

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
        store.upsert_nodes(std::slice::from_ref(&n)).unwrap();
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
            Symbol, SYMBOL_SCHEME,
        };
        use wicked_estate_core::Annotation;

        fn store() -> SqliteStore {
            SqliteStore::in_memory().unwrap()
        }
        fn node(store: &mut SqliteStore, name: &str, kind: NodeKind, file: &str) -> Node {
            // A SOURCE node carries a source-scheme symbol — real extractors mint per-language schemes
            // (`odm`, `dmn`, scip, …), NEVER the `wicked-apps` synthetic scheme. It matters here because
            // coverage now excludes `wicked-apps` synthetic symbols as domain-model artifacts
            // (persist_domain_model output); faking source with one would drop every fixture below out of
            // the denominator. `"src"` stands in for any extractor scheme.
            let n = Node::new(
                Symbol::synthetic("src", format!("{file}::{name}")).id(),
                kind,
                name.to_string(),
                Language::new("rust"),
                Location::new(file.to_string(), Span::ZERO),
            );
            store.begin_batch().unwrap();
            store.upsert_nodes(std::slice::from_ref(&n)).unwrap();
            store.commit_batch().unwrap();
            n
        }
        fn cov(store: &SqliteStore) -> CoverageReport {
            recompute_front_half_coverage(store).unwrap()
        }

        /// Map node `name` with a validated requirement + a business rule by `author`.
        fn map(store: &mut SqliteStore, n: &Node, author: &str, provenance: &str) {
            store
                .set_node_semantics(
                    &n.symbol,
                    None,
                    Some(&format!("{} holds", n.name)),
                    Some(&ValidationClaim::new(true, "test-fixture").unwrap()),
                )
                .unwrap();
            store
                .annotate(
                    &n.symbol,
                    Annotation::new("business_rule", provenance, format!("{} holds", n.name))
                        .with_confidence(0.9)
                        .with_provenance(provenance)
                        .with_author(author),
                )
                .unwrap();
        }

        fn build_at(store: &SqliteStore, min: u8) -> DomainModel {
            let opts = DomainBuildOptions {
                min_coverage_pct: min,
                repo_root: None,
            };
            build_domain_model_with(store, &cov(store), "1.1.0", &opts).unwrap()
        }

        /// xreview on core#1215 (codex P2): requirement ids are STABLE as the mapping grows. With
        /// a..e and b..e mapped (80%), `b` must keep its id when `a` is mapped next, and the
        /// persisted `SatisfiedBy` edges must still point each requirement at its own node.
        #[test]
        fn requirement_ids_stay_put_as_coverage_grows_and_satisfaction_follows() {
            use wicked_apps_core::{synthetic_symbol, EdgeKind, GraphRead};
            let mut s = store();
            let nodes: Vec<Node> = ["a", "b", "c", "d", "e"]
                .iter()
                .map(|n| node(&mut s, n, NodeKind::Function, "pkg/ops.rs"))
                .collect();
            for n in &nodes[1..] {
                map(&mut s, n, "seat-1", "extract-loop");
            }
            let first = build_at(&s, 80);
            let title_of = |m: &DomainModel, id: &str| {
                m.domains["pkg"]
                    .requirements
                    .get(id)
                    .map(|r| r.title.clone())
            };
            assert_eq!(
                title_of(&first, "REQ-001"),
                None,
                "a's slot is a gap while unmapped"
            );
            assert_eq!(title_of(&first, "REQ-002").as_deref(), Some("b"));
            persist_domain_model(&mut s, &first).unwrap();

            map(&mut s, &nodes[0], "seat-1", "extract-loop");
            let second = build_at(&s, 80);
            assert_eq!(title_of(&second, "REQ-001").as_deref(), Some("a"));
            assert_eq!(
                title_of(&second, "REQ-002").as_deref(),
                Some("b"),
                "b kept its id"
            );
            persist_domain_model(&mut s, &second).unwrap();
            let edges = s.all_edges().unwrap();
            for (id, want) in [("REQ-001", "a"), ("REQ-002", "b")] {
                let rsym = synthetic_symbol("requirement", &format!("pkg/{id}"));
                let targets: Vec<String> = edges
                    .iter()
                    .filter(|e| e.source == rsym && e.kind == EdgeKind::SatisfiedBy)
                    .map(|e| s.get_node(&e.target).unwrap().unwrap().name)
                    .collect();
                assert_eq!(
                    targets,
                    vec![want.to_string()],
                    "{id} is satisfied by {want} alone"
                );
            }
        }

        /// Operator ruling 2026-10-31: the requirements graph lives in estate. What a build saves
        /// reads back out of the store as the SAME document (it deserializes as `DomainModel` and
        /// equals the build's), with the coverage label from the store's own counts when asked.
        #[test]
        fn the_graph_reads_back_from_estate_as_the_builds_document() {
            let mut s = store();
            let nodes: Vec<Node> = ["a", "b", "c", "d", "e"]
                .iter()
                .map(|n| node(&mut s, n, NodeKind::Function, "pkg/ops.rs"))
                .collect();
            for n in &nodes[1..] {
                map(&mut s, n, "seat-1", "extract-loop");
            }
            assert!(
                load_domain_graph(&s, None).unwrap().is_none(),
                "nothing built yet"
            );
            let mut model = build_at(&s, 80);
            stabilize_requirement_ids(&s, &mut model).unwrap();
            persist_domain_model(&mut s, &model).unwrap();
            let read = load_domain_graph(&s, None).unwrap().expect("a graph");
            let back: DomainModel = serde_json::from_value(read.clone()).unwrap();
            assert_eq!(
                read,
                serde_json::to_value(&model).unwrap(),
                "the store reads back the build's document"
            );
            assert_eq!(back.domains.len(), model.domains.len());

            let label = model_coverage(&cov(&s), 80);
            assert_eq!((label.real_pct, label.min_pct, label.real), (80, 80, 4));
            let labelled = load_domain_graph(&s, Some(&label)).unwrap().unwrap();
            assert_eq!(labelled["metadata"]["coverage"]["real_pct"], 80);
            assert!(labelled["metadata"]["coverage"].get("mapped_pct").is_none());
            assert_eq!(labelled["metadata"]["schema_version"], "1.1.0");

            // req-list-stale: the store moves on after the build (the first behaviour is mapped
            // too). The live label replaces the document's, and the build's own label is still
            // there to read, with the build's generation, so a reader can tell the rows are older.
            map(&mut s, &nodes[0], "seat-1", "extract-loop");
            let live = model_coverage(&cov(&s), 80);
            assert_eq!(live.real, 5);
            let (mut read, build) = load_domain_graph_with_build(&s).unwrap().expect("a graph");
            assert_eq!(
                read["metadata"]["coverage"]["real"], 4,
                "as the build wrote it"
            );
            label_domain_graph(&mut read, &live).unwrap();
            assert_eq!(read["metadata"]["coverage"]["real"], 5, "the live label");
            assert_eq!(build.generation, 1);
            assert_eq!(
                build.coverage,
                Some(serde_json::to_value(model.metadata.coverage.as_ref().unwrap()).unwrap()),
                "the label the rows were built at, not the live one"
            );
        }

        /// xreview on core#1223 (codex + agy P2): ids follow the CODE, not its position. Deleting an
        /// early behaviour (its file gone, the others untouched) must not shift later requirements
        /// onto different code, and a kept requirement keeps no `SatisfiedBy` edge the new build did
        /// not state. A new behaviour takes a fresh number; nothing stale is left behind.
        #[test]
        fn requirement_ids_follow_their_code_across_deletions_and_edges_are_replaced() {
            use wicked_apps_core::{EdgeKind, GraphRead};
            use wicked_estate_core::Direction;
            let mut s = store();
            // One behaviour per file, all in package `pkg`.
            let mut nodes: Vec<Node> = ["a", "b", "c"]
                .iter()
                .map(|n| node(&mut s, n, NodeKind::Function, &format!("pkg/{n}.rs")))
                .collect();
            for n in &nodes {
                map(&mut s, n, "seat-1", "extract-loop");
            }
            let mut first = build_at(&s, 80);
            stabilize_requirement_ids(&s, &mut first).unwrap();
            persist_domain_model(&mut s, &first).unwrap();
            let id_of = |m: &DomainModel, name: &str| -> String {
                m.domains["pkg"]
                    .requirements
                    .iter()
                    .find(|(_, r)| r.title == name)
                    .map(|(k, _)| k.clone())
                    .unwrap()
            };
            let (b1, c1) = (id_of(&first, "b"), id_of(&first, "c"));

            // `a` is deleted; `d` is new.
            s.remove_file("pkg/a.rs").unwrap();
            let d = node(&mut s, "d", NodeKind::Function, "pkg/d.rs");
            map(&mut s, &d, "seat-1", "extract-loop");
            nodes.push(d);
            let mut second = build_at(&s, 80);
            stabilize_requirement_ids(&s, &mut second).unwrap();
            persist_domain_model(&mut s, &second).unwrap();
            assert_eq!(id_of(&second, "b"), b1, "b kept its id");
            assert_eq!(id_of(&second, "c"), c1, "c kept its id");
            let d_id = id_of(&second, "d");
            assert!(
                ![id_of(&first, "a"), b1.clone(), c1.clone()].contains(&d_id),
                "a new behaviour never takes a retired id: {d_id}"
            );
            // Every requirement is SatisfiedBy exactly its own code, nothing from the first build.
            for name in ["b", "c", "d"] {
                let id = id_of(&second, name);
                let rsym = synthetic_symbol("requirement", &format!("pkg/{id}"));
                let code: Vec<String> = s
                    .neighbors(&rsym, Direction::Dependencies)
                    .unwrap()
                    .into_iter()
                    .filter(|e| e.kind == EdgeKind::SatisfiedBy)
                    .map(|e| s.get_node(&e.target).unwrap().unwrap().name)
                    .collect();
                assert_eq!(
                    code,
                    vec![name.to_string()],
                    "{id} is satisfied by {name} alone"
                );
            }
            assert_eq!(
                load_domain_graph(&s, None).unwrap().unwrap(),
                serde_json::to_value(&second).unwrap(),
                "the store holds the second build exactly"
            );
        }

        /// xreview round 2 on core#1223/crew#1246 (codex P2): a retired id stays retired across a
        /// build that dropped it — and across a build that dropped the whole domain.
        #[test]
        fn a_retired_id_is_never_reused_even_after_an_intervening_build() {
            let mut s = store();
            let a = node(&mut s, "a", NodeKind::Function, "pkg/a.rs");
            let b = node(&mut s, "b", NodeKind::Function, "pkg/b.rs");
            map(&mut s, &a, "seat-1", "extract-loop");
            map(&mut s, &b, "seat-1", "extract-loop");
            let build = |s: &mut SqliteStore| {
                let mut m = build_at(s, 1);
                stabilize_requirement_ids(s, &mut m).unwrap();
                persist_domain_model(s, &m).unwrap();
                m
            };
            let first = build(&mut s);
            let b_id = first.domains["pkg"]
                .requirements
                .iter()
                .find(|(_, r)| r.title == "b")
                .map(|(k, _)| k.clone())
                .unwrap();
            s.remove_file("pkg/b.rs").unwrap();
            build(&mut s); // b's id retired
            let c = node(&mut s, "c", NodeKind::Function, "pkg/c.rs");
            map(&mut s, &c, "seat-1", "extract-loop");
            let third = build(&mut s);
            let ids: Vec<&String> = third.domains["pkg"].requirements.keys().collect();
            assert!(
                !ids.contains(&&b_id),
                "c must not take b's retired {b_id}: {ids:?}"
            );

            // The whole domain goes, then comes back: numbering continues.
            s.remove_file("pkg/a.rs").unwrap();
            s.remove_file("pkg/c.rs").unwrap();
            build(&mut s);
            let d = node(&mut s, "d", NodeKind::Function, "pkg/d.rs");
            map(&mut s, &d, "seat-1", "extract-loop");
            let fifth = build(&mut s);
            let d_id = fifth.domains["pkg"]
                .requirements
                .keys()
                .next()
                .unwrap()
                .clone();
            assert!(
                req_number(&d_id).unwrap() > 3,
                "a returning domain does not restart at REQ-001: {d_id}"
            );
        }

        /// xreview round 2 (codex P2): a successful EMPTY build reads back as built (no domains),
        /// never as "not generated"; a 1.0.0 build reads back without the 1.1.0 coverage field.
        #[test]
        fn an_empty_build_reads_back_as_built_and_a_1_0_0_build_keeps_its_shape() {
            let mut s = store();
            let empty = build_at(&s, 1);
            assert!(empty.domains.is_empty());
            persist_domain_model(&mut s, &empty).unwrap();
            let label = model_coverage(&cov(&s), 80);
            let read = load_domain_graph(&s, Some(&label))
                .unwrap()
                .expect("built, not absent");
            assert_eq!(read["domains"], serde_json::json!({}));

            let n = node(&mut s, "a", NodeKind::Function, "pkg/a.rs");
            map(&mut s, &n, "seat-1", "extract-loop");
            let v1 = build_domain_model(&s, &cov(&s), "1.0.0").unwrap();
            persist_domain_model(&mut s, &v1).unwrap();
            let label = model_coverage(&cov(&s), 80);
            let read = load_domain_graph(&s, Some(&label)).unwrap().unwrap();
            assert!(read["metadata"].get("coverage").is_none(), "{read}");
            assert_eq!(read, serde_json::to_value(&v1).unwrap());
        }

        /// xreview round 2 (codex P2): two same-named symbols in one file (`A::new`, `B::new`)
        /// share a `file#name`; their requirements keep their own ids on an unchanged rebuild, and
        /// each is SatisfiedBy its own code (the SymbolId the builder knew, not a name lookup).
        #[test]
        fn same_named_symbols_in_one_file_keep_their_own_ids_and_code() {
            use wicked_apps_core::{EdgeKind, GraphRead};
            use wicked_estate_core::Direction;
            let mut s = store();
            let mk = |s: &mut SqliteStore, owner: &str| {
                let n = Node::new(
                    Symbol::synthetic("src", format!("pkg/m.rs::{owner}::new")).id(),
                    NodeKind::Function,
                    "new".to_string(),
                    Language::new("rust"),
                    Location::new("pkg/m.rs".to_string(), Span::ZERO),
                );
                s.begin_batch().unwrap();
                s.upsert_nodes(std::slice::from_ref(&n)).unwrap();
                s.commit_batch().unwrap();
                map(s, &n, "seat-1", "extract-loop");
                n
            };
            let (na, nb) = (mk(&mut s, "A"), mk(&mut s, "B"));
            let ids_by_code = |s: &SqliteStore| -> Vec<(String, String)> {
                let g = load_domain_graph(s, None).unwrap().unwrap();
                let mut v = Vec::new();
                for key in g["domains"]["pkg"]["requirements"]
                    .as_object()
                    .unwrap()
                    .keys()
                {
                    let rsym = synthetic_symbol("requirement", &format!("pkg/{key}"));
                    for e in s.neighbors(&rsym, Direction::Dependencies).unwrap() {
                        if e.kind == EdgeKind::SatisfiedBy {
                            v.push((key.clone(), e.target.as_str().to_string()));
                        }
                    }
                }
                v.sort();
                v
            };
            for _ in 0..3 {
                let mut m = build_at(&s, 1);
                stabilize_requirement_ids(&s, &mut m).unwrap();
                persist_domain_model(&mut s, &m).unwrap();
            }
            let got = ids_by_code(&s);
            assert_eq!(
                got.len(),
                2,
                "each requirement links exactly one code node: {got:?}"
            );
            let codes: std::collections::BTreeSet<&str> =
                got.iter().map(|(_, c)| c.as_str()).collect();
            assert_eq!(
                codes,
                [na.symbol.as_str(), nb.symbol.as_str()]
                    .into_iter()
                    .collect()
            );
            let ids: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
            assert_eq!(
                ids,
                ["REQ-001", "REQ-002"],
                "three rebuilds, ids unchanged: {got:?}"
            );
        }

        /// xreview round 2 (codex P2): a graph persisted by the previous engine (no documents on
        /// the nodes, no rule provenance) still reads back as a valid domain-model document.
        #[test]
        fn a_graph_persisted_before_the_documents_reads_back_valid() {
            let mut s = store();
            let n = node(&mut s, "a", NodeKind::Function, "pkg/a.rs");
            map(&mut s, &n, "seat-1", "extract-loop");
            let m = build_at(&s, 1);
            persist_domain_model(&mut s, &m).unwrap();
            // Strip what the previous engine never wrote.
            let mut old: Vec<Node> = previous_graph_nodes(&s).unwrap();
            for node in old.iter_mut() {
                node.metadata.remove(REQUIREMENT_DOC_KEY);
                node.metadata.remove(DOMAIN_DOC_KEY);
                node.metadata.remove(MODEL_METADATA_KEY);
                node.metadata.remove("provenance");
                node.metadata.remove("code_symbol");
            }
            s.begin_batch().unwrap();
            s.remove_file(&format!("{BUILD_SCHEME}/{BUILD_ID}"))
                .unwrap();
            s.upsert_nodes(&old).unwrap();
            s.commit_batch().unwrap();
            let read = load_domain_graph(&s, None).unwrap().expect("a graph");
            let back: DomainModel = serde_json::from_value(read).expect("a valid document");
            let rule = &back.domains["pkg"]
                .requirements
                .values()
                .next()
                .unwrap()
                .business_rules[0];
            assert_eq!(rule.provenance.reference, "pkg/a.rs#a");
        }

        /// xreview round 3 (codex P2): the FIRST build after an upgrade (a graph with no build
        /// record) keeps the numbers the old graph used, so a requirement it drops stays retired.
        #[test]
        fn an_upgraded_graph_keeps_its_retired_numbers() {
            let mut s = store();
            let a = node(&mut s, "a", NodeKind::Function, "pkg/a.rs");
            let b = node(&mut s, "b", NodeKind::Function, "pkg/b.rs");
            map(&mut s, &a, "seat-1", "extract-loop");
            map(&mut s, &b, "seat-1", "extract-loop");
            let old = build_at(&s, 1); // a = REQ-001, b = REQ-002
            persist_domain_model(&mut s, &old).unwrap();
            // What the previous engine left: no build record.
            s.begin_batch().unwrap();
            s.remove_file(&format!("{BUILD_SCHEME}/{BUILD_ID}"))
                .unwrap();
            s.commit_batch().unwrap();
            let rebuild = |s: &mut SqliteStore| {
                let mut m = build_at(s, 1);
                let g = stabilize_requirement_ids(s, &mut m).unwrap();
                persist_domain_model_expecting(s, &m, Some(g)).unwrap();
                m
            };
            s.remove_file("pkg/b.rs").unwrap();
            rebuild(&mut s); // the first upgraded build drops b's REQ-002
            let c = node(&mut s, "c", NodeKind::Function, "pkg/c.rs");
            map(&mut s, &c, "seat-1", "extract-loop");
            let later = rebuild(&mut s);
            assert!(
                !later.domains["pkg"].requirements.contains_key("REQ-002"),
                "c must not take b's retired REQ-002: {:?}",
                later.domains["pkg"].requirements.keys().collect::<Vec<_>>()
            );
        }

        /// xreview round 3 (codex P2): an old graph (no SymbolIds on its nodes) with two
        /// same-named symbols in one file cannot tell which old id was whose: both take fresh
        /// ids, so no operator override moves to the other behaviour.
        #[test]
        fn an_ambiguous_legacy_reference_keeps_no_id() {
            let mut s = store();
            for owner in ["A", "B"] {
                let n = Node::new(
                    Symbol::synthetic("src", format!("pkg/m.rs::{owner}::new")).id(),
                    NodeKind::Function,
                    "new".to_string(),
                    Language::new("rust"),
                    Location::new("pkg/m.rs".to_string(), Span::ZERO),
                );
                s.begin_batch().unwrap();
                s.upsert_nodes(std::slice::from_ref(&n)).unwrap();
                s.commit_batch().unwrap();
                map(&mut s, &n, "seat-1", "extract-loop");
            }
            let first = build_at(&s, 1);
            persist_domain_model(&mut s, &first).unwrap();
            // The previous engine's format: no code_symbol on the requirement nodes.
            let mut old: Vec<Node> = previous_graph_nodes(&s).unwrap();
            for n in old.iter_mut() {
                n.metadata.remove("code_symbol");
            }
            s.begin_batch().unwrap();
            s.upsert_nodes(&old).unwrap();
            s.commit_batch().unwrap();
            let mut m = build_at(&s, 1);
            stabilize_requirement_ids(&s, &mut m).unwrap();
            let ids: Vec<&String> = m.domains["pkg"].requirements.keys().collect();
            assert_eq!(
                ids,
                ["REQ-003", "REQ-004"],
                "both fresh, neither inherits: {ids:?}"
            );
        }

        /// xreview round 4 (codex P2): an old graph mapped only `B::new`; the first upgraded build
        /// also maps `A::new` in the same file. The shared `file#name` cannot say which is the old
        /// one, so neither inherits B's old id (B's override shows as orphaned, never on A).
        #[test]
        fn a_reference_shared_in_the_new_build_carries_no_legacy_id() {
            let mut s = store();
            let mk = |s: &mut SqliteStore, owner: &str| {
                let n = Node::new(
                    Symbol::synthetic("src", format!("pkg/m.rs::{owner}::new")).id(),
                    NodeKind::Function,
                    "new".to_string(),
                    Language::new("rust"),
                    Location::new("pkg/m.rs".to_string(), Span::ZERO),
                );
                s.begin_batch().unwrap();
                s.upsert_nodes(std::slice::from_ref(&n)).unwrap();
                s.commit_batch().unwrap();
                n
            };
            let b = mk(&mut s, "B");
            map(&mut s, &b, "seat-1", "extract-loop");
            let first = build_at(&s, 1);
            let b_old = first.domains["pkg"]
                .requirements
                .keys()
                .next()
                .unwrap()
                .clone();
            persist_domain_model(&mut s, &first).unwrap();
            let mut old: Vec<Node> = previous_graph_nodes(&s).unwrap();
            for n in old.iter_mut() {
                n.metadata.remove("code_symbol"); // the previous engine's format
            }
            s.begin_batch().unwrap();
            s.upsert_nodes(&old).unwrap();
            s.commit_batch().unwrap();
            let a = mk(&mut s, "A"); // sorts before B
            map(&mut s, &a, "seat-1", "extract-loop");
            let mut m = build_at(&s, 1);
            stabilize_requirement_ids(&s, &mut m).unwrap();
            assert!(
                !m.domains["pkg"].requirements.contains_key(&b_old),
                "neither A nor B inherits {b_old}: {:?}",
                m.domains["pkg"].requirements.keys().collect::<Vec<_>>()
            );
        }

        /// xreview round 3 (codex P2): a build numbered against one generation is refused if
        /// another build committed meanwhile — nothing written.
        #[test]
        fn a_build_prepared_against_an_older_generation_is_refused() {
            let mut s = store();
            let a = node(&mut s, "a", NodeKind::Function, "pkg/a.rs");
            map(&mut s, &a, "seat-1", "extract-loop");
            let mut first = build_at(&s, 1);
            let g = stabilize_requirement_ids(&s, &mut first).unwrap();
            let mut second = build_at(&s, 1);
            stabilize_requirement_ids(&s, &mut second).unwrap();
            persist_domain_model_expecting(&mut s, &second, Some(g)).unwrap(); // commits g+1
            let err = persist_domain_model_expecting(&mut s, &first, Some(g)).unwrap_err();
            assert!(
                err.to_string().contains("another requirements-graph build"),
                "{err}"
            );
        }

        /// xreview round 2 (codex P3): replacing the build drops an edge between nodes that BOTH
        /// survive — a satisfaction target that moved, a dependency that went.
        #[test]
        fn edges_between_surviving_nodes_are_replaced_too() {
            use wicked_apps_core::{EdgeKind, GraphRead};
            use wicked_estate_core::Direction;
            let mut s = store();
            let mut code = std::collections::HashMap::new();
            for n in ["a", "b", "c"] {
                let n = node(&mut s, n, NodeKind::Function, &format!("pkg/{n}.rs"));
                map(&mut s, &n, "seat-1", "extract-loop");
                code.insert(n.name.clone(), n.symbol.as_str().to_string());
            }
            let mut first = build_at(&s, 1);
            stabilize_requirement_ids(&s, &mut first).unwrap();
            let keys: Vec<String> = first.domains["pkg"].requirements.keys().cloned().collect();
            {
                let reqs = &mut first.domains.get_mut("pkg").unwrap().requirements;
                reqs.get_mut(&keys[0]).unwrap().dependencies = vec![keys[1].clone()];
            }
            persist_domain_model(&mut s, &first).unwrap();
            let r0 = synthetic_symbol("requirement", &format!("pkg/{}", keys[0]));
            let out = |s: &SqliteStore, kind: EdgeKind| -> Vec<String> {
                s.neighbors(&r0, Direction::Dependencies)
                    .unwrap()
                    .into_iter()
                    .filter(|e| e.kind == kind)
                    .map(|e| e.target.as_str().to_string())
                    .collect()
            };
            assert_eq!(out(&s, EdgeKind::References).len(), 1);
            // Second build: the first requirement now names c's code instead of its own, and has
            // no dependency. Every code node and every requirement survives.
            let mut second = first.clone();
            {
                let reqs = &mut second.domains.get_mut("pkg").unwrap().requirements;
                let r = reqs.get_mut(&keys[0]).unwrap();
                r.dependencies.clear();
                r.legacy_components = vec!["pkg/c.rs#c".to_string()];
                r.code_symbol = Some(code["c"].clone());
            }
            persist_domain_model(&mut s, &second).unwrap();
            assert!(
                out(&s, EdgeKind::References).is_empty(),
                "the dropped dependency is gone"
            );
            let code: Vec<String> = out(&s, EdgeKind::SatisfiedBy)
                .iter()
                .map(|t| {
                    s.get_node(&wicked_apps_core::SymbolId::from(t.as_str()))
                        .unwrap()
                        .unwrap()
                        .name
                })
                .collect();
            assert_eq!(
                code,
                vec!["c".to_string()],
                "only the new satisfaction target"
            );
        }

        /// xreview on core#1215 (codex P2): a DESCRIPTION-only behaviour is a coverage hole, so a
        /// partial build must not turn it into a requirement with a synthesized rule.
        #[test]
        fn a_description_only_hole_is_not_a_requirement() {
            let mut s = store();
            let nodes: Vec<Node> = ["a", "b", "c", "d", "e"]
                .iter()
                .map(|n| node(&mut s, n, NodeKind::Function, "pkg/ops.rs"))
                .collect();
            for n in &nodes[1..] {
                map(&mut s, n, "seat-1", "extract-loop");
            }
            s.set_node_semantics(&nodes[0].symbol, Some("adds numbers"), None, None)
                .unwrap();
            let m = build_at(&s, 80);
            assert_eq!(m.domains["pkg"].requirements.len(), 4, "{m:?}");
            assert!(m.domains["pkg"]
                .requirements
                .values()
                .all(|r| r.title != "a"));
            assert_eq!(m.metadata.coverage.as_ref().unwrap().real, 4);
        }

        /// agy on #1234: a test function or a JSON key an earlier page annotated (a placeholder,
        /// or a rule) is not a requirement in the graph, so the graph matches the denominator.
        #[test]
        fn annotated_non_product_nodes_are_not_requirements() {
            let mut s = store();
            let real = node(&mut s, "charge", NodeKind::Function, "pkg/ops.rs");
            map(&mut s, &real, "seat-1", "extract-loop");
            let test_fn = node(
                &mut s,
                "capture",
                NodeKind::Function,
                "test/live/capture.mjs",
            );
            let json_key = node(&mut s, "scripts", NodeKind::Struct, "package.json");
            map(&mut s, &test_fn, "seat-1", "extract-loop");
            s.annotate(
                &json_key.symbol,
                Annotation::new("risk", "r", "[RISK] scripts: not product code")
                    .with_confidence(0.0),
            )
            .unwrap();
            // An out-of-range confidence on an excluded node never blocks the build (codex r2).
            s.annotate(
                &test_fn.symbol,
                Annotation::new("business_rule", "bad", "x").with_confidence(1.1),
            )
            .unwrap();
            let m = build_at(&s, 80);
            let titles: Vec<&str> = m
                .domains
                .values()
                .flat_map(|d| d.requirements.values())
                .map(|r| r.title.as_str())
                .collect();
            assert_eq!(titles, vec!["charge"], "{m:?}");
            let label = m.metadata.coverage.as_ref().unwrap();
            assert_eq!(
                (label.behavior_bearing, label.real, label.not_product),
                (1, 1, 2)
            );
        }

        /// xreview on core#1215 (codex P2): extractor SEATS are counted by author, not provenance.
        #[test]
        fn one_seat_citing_two_provenances_is_one_extractor() {
            let mut s = store();
            let a = node(&mut s, "settle", NodeKind::Function, "pkg/ops.rs");
            map(&mut s, &a, "seat-1", "code-body");
            s.annotate(
                &a.symbol,
                Annotation::new("business_rule", "r2", "settle holds")
                    .with_confidence(0.9)
                    .with_provenance("comment")
                    .with_author("seat-1"),
            )
            .unwrap();
            let one = build_at(&s, 80);
            let c = one.domains["pkg"].requirements["REQ-001"]
                .corroboration
                .clone()
                .unwrap();
            assert_eq!((c.status.as_str(), c.extra_extractors), ("unconfirmed", 0));
            // A second seat agreeing is a source; two more make it validated.
            for seat in ["seat-2", "seat-3"] {
                s.annotate(
                    &a.symbol,
                    Annotation::new("business_rule", seat, "settle holds")
                        .with_confidence(0.9)
                        .with_author(seat),
                )
                .unwrap();
            }
            let c = build_at(&s, 80).domains["pkg"].requirements["REQ-001"]
                .corroboration
                .clone()
                .unwrap();
            assert_eq!((c.status.as_str(), c.extra_extractors), ("validated", 2));
        }

        /// xreview on core#1215 (agy P2): a 1.0.0 document keeps the 1.0.0 shape; an unknown
        /// version is refused rather than emitted.
        #[test]
        fn a_1_0_0_build_carries_no_1_1_0_fields_and_unknown_versions_are_refused() {
            let mut s = store();
            let a = node(&mut s, "settle", NodeKind::Function, "pkg/ops.rs");
            map(&mut s, &a, "seat-1", "code-body");
            let opts = DomainBuildOptions {
                min_coverage_pct: 80,
                repo_root: None,
            };
            let old = build_domain_model_with(&s, &cov(&s), "1.0.0", &opts).unwrap();
            assert!(old.metadata.coverage.is_none());
            assert!(old.domains["pkg"].requirements["REQ-001"]
                .corroboration
                .is_none());
            assert!(build_domain_model_with(&s, &cov(&s), "9.9.9", &opts).is_err());
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
        fn persisted_domain_model_rule_is_excluded_from_the_denominator() {
            // A `persist_domain_model` business-rule node is `NodeKind::Rule` under the WICKED-APPS
            // synthetic scheme — annotation OUTPUT (the domain graph), not extracted source to be
            // annotated. It must NOT enter the coverage denominator: it can never be annotated, so
            // counting it would pin coverage below 1.0 on every run AFTER a domain-graph persist —
            // the measurement polluting the measurand (surfaced by the P8 governed PageIndex run,
            // where a single leftover persisted Rule dragged the gate to 0.9987). The sibling
            // `bare_rule_nodes_fail_the_gate_critical_1` proves EXTRACTED rules (a source scheme)
            // still count — the exclusion keys on the scheme, not the kind.
            let mut s = store();
            // one resolved source function → 1.0 on its own.
            let f = node(&mut s, "charge", NodeKind::Function, "billing.rs");
            s.set_node_semantics(
                &f.symbol,
                Some("charges a card"),
                Some("REQ-1"),
                Some(&ValidationClaim::new(true, "test-fixture").unwrap()),
            )
            .unwrap();
            // a persisted domain-model Rule (wicked-apps synthetic) sharing the same store.
            let rule = Node::new(
                synthetic_symbol("rule", "billing/REQ-1/RULE-001"),
                NodeKind::Rule,
                "amount must be positive".to_string(),
                Language::new(SYMBOL_SCHEME),
                Location::new("rule/billing/REQ-1/RULE-001".to_string(), Span::ZERO),
            );
            s.begin_batch().unwrap();
            s.upsert_nodes(std::slice::from_ref(&rule)).unwrap();
            s.commit_batch().unwrap();

            let r = cov(&s);
            // Falsifier: drop the `is_apps_synthetic_symbol` guard in `is_behavior_bearing` and the
            // persisted Rule counts → behavior_bearing 2, unaccounted 1, coverage 0.5 → these fail.
            assert_eq!(
                r.behavior_bearing, 1,
                "only the source function counts; the persisted domain Rule is excluded: {r:?}"
            );
            assert_eq!(
                r.unaccounted, 0,
                "the excluded Rule is not an unaccounted hole: {r:?}"
            );
            assert!(
                (r.coverage - 1.0).abs() < 1e-9,
                "coverage stays 1.0 despite the persisted domain Rule in the store: {r:?}"
            );
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

        /// A source node with an extractor-style symbol, a language, a span (bytes, lines) and the
        /// extractor's one-line signature.
        #[allow(clippy::too_many_arguments)]
        fn shaped(
            store: &mut SqliteStore,
            sym: &str,
            kind: NodeKind,
            file: &str,
            lang: &str,
            bytes: (u32, u32),
            lines: (u32, u32),
            sig: &str,
        ) -> Node {
            let name = sym
                .trim_end_matches(['#', '.', '(', ')'])
                .rsplit(['#', '/'])
                .next()
                .unwrap()
                .to_string();
            let mut n = Node::new(
                wicked_apps_core::SymbolId::from(sym),
                kind,
                name,
                Language::new(lang),
                Location::new(
                    file.to_string(),
                    Span {
                        start_byte: bytes.0,
                        end_byte: bytes.1,
                        start_line: lines.0,
                        start_col: 0,
                        end_line: lines.1,
                        end_col: 1,
                    },
                ),
            );
            n.signature = Some(sig.to_string());
            store.begin_batch().unwrap();
            store.upsert_nodes(std::slice::from_ref(&n)).unwrap();
            store.commit_batch().unwrap();
            n
        }
        fn calls(store: &mut SqliteStore, from: &Node, to: &Node) {
            use wicked_apps_core::{Edge, EdgeKind, ResolutionTier};
            store.begin_batch().unwrap();
            store
                .upsert_edges(&[Edge::new(
                    from.symbol.clone(),
                    to.symbol.clone(),
                    EdgeKind::Calls,
                    ResolutionTier::Heuristic,
                    "test",
                )])
                .unwrap();
            store.commit_batch().unwrap();
        }
        fn listed(r: &CoverageReport) -> Vec<String> {
            let mut v: Vec<String> = r
                .unaccounted_nodes
                .iter()
                .chain(&r.placeholder_nodes)
                .map(|n| n.symbol_id.rsplit(' ').next().unwrap().to_string())
                .collect();
            v.sort();
            v
        }
        const TS: &str = "ts-typescript . . . ";

        #[test]
        fn owner_symbol_names_the_enclosing_type() {
            assert_eq!(
                owner_symbol("t . . . a/T#run().", "run"),
                Some("t . . . a/T#")
            );
            assert_eq!(owner_symbol("t . . . a/T#x.", "x"), Some("t . . . a/T#"));
            assert_eq!(
                owner_symbol("t . . . a/T#N#run().", "run"),
                Some("t . . . a/T#N#")
            );
            assert_eq!(owner_symbol("t . . . a/T#N#", "N"), Some("t . . . a/T#"));
            assert_eq!(owner_symbol("t . . . a/T#", "T"), None);
            assert_eq!(owner_symbol("t . . . a/run().", "run"), None);
            assert_eq!(owner_symbol("t . . . a/T#run().", "other"), None);
            // xreview r2+r3 on core#1238 (codex): estate renders names raw, so a name may hold `#`
            // (a private field `#data`), or be backtick-escaped.
            assert_eq!(
                owner_symbol("t . . . a/Live##data.", "#data"),
                Some("t . . . a/Live#")
            );
            assert_eq!(
                owner_symbol("t . . . a/Live##data.", "data"),
                Some("t . . . a/Live#")
            );
            assert_eq!(
                owner_symbol("t . . . a/Live##run().", "#run"),
                Some("t . . . a/Live#")
            );
            assert_eq!(
                owner_symbol("s . . . a/H#`foo#bar`().", "foo#bar"),
                Some("s . . . a/H#")
            );
            assert_eq!(
                owner_symbol("s . . . a/H#foo#bar().", "foo#bar"),
                Some("s . . . a/H#")
            );
        }

        /// cov-interfaces (2026-11-01): wicked-crew stopped at 61% with 915 of its 2,174
        /// placeholders on TypeScript interfaces (459 of them wire types). A TS interface that only
        /// describes a data shape is the same thing as a type alias, and its extracted method
        /// signatures have no body: neither can carry a business rule. The class method and the
        /// function beside them still count. Falsifier: put `Interface` back in the unconditional
        /// arm of `is_behavior_bearing` (and drop the bodiless test) and `behavior_bearing` reads 5.
        #[test]
        fn ts_shape_interface_and_its_signatures_are_structural() {
            let mut s = store();
            let f = "src/auth.ts";
            shaped(
                &mut s,
                &format!("{TS}src/auth/TokenVerifier#"),
                NodeKind::Interface,
                f,
                "typescript",
                (0, 120),
                (1, 5),
                "export interface TokenVerifier {",
            );
            shaped(
                &mut s,
                &format!("{TS}src/auth/TokenVerifier#verify()."),
                NodeKind::Method,
                f,
                "typescript",
                (60, 110),
                (4, 4),
                "verify(token: string): Promise<Actor | null>",
            );
            shaped(
                &mut s,
                &format!("{TS}src/auth/Wire#"),
                NodeKind::Interface,
                f,
                "typescript",
                (130, 200),
                (7, 9),
                "interface Wire {",
            );
            let m = shaped(
                &mut s,
                &format!("{TS}src/auth/StaticVerifier#verify()."),
                NodeKind::Method,
                f,
                "typescript",
                (260, 400),
                (12, 20),
                "async verify(token: string): Promise<Actor | null> {",
            );
            let g = shaped(
                &mut s,
                &format!("{TS}src/auth/defaultTokensPath()."),
                NodeKind::Function,
                f,
                "typescript",
                (420, 520),
                (22, 24),
                "export function defaultTokensPath(env = process.env): string {",
            );
            let r = cov(&s);
            assert_eq!(r.behavior_bearing, 2, "{r:?}");
            assert_eq!(
                listed(&r),
                vec![
                    "src/auth/StaticVerifier#verify().",
                    "src/auth/defaultTokensPath()."
                ]
            );
            // Mapping the two behaviours is 100%: the unmapped shapes never block a full build, and
            // the build emits exactly the two requirements.
            map(&mut s, &m, "seat", "RULE-1");
            map(&mut s, &g, "seat", "RULE-2");
            assert_eq!(cov(&s).resolved, 2);
            let model = build_at(&s, 100);
            let reqs: usize = model.domains.values().map(|d| d.requirements.len()).sum();
            assert_eq!(reqs, 2);
        }

        /// req-reconcile (2026-11-01): wicked-crew's list held 1,458 rows on TypeScript shapes (an
        /// extraction page had annotated them before core#1238 stopped counting them) under a header
        /// that counts none, so the list showed 441 more "real" rows than the header. The read now says,
        /// from the same pass that counts the header, which listed rows the share leaves out and why;
        /// the rest are exactly the counted ones. Falsifier: observe every node as `Counted` in
        /// `recompute_observing` and the shape's row loses its label (the map has 3 entries, not 4).
        #[test]
        fn the_read_labels_listed_rows_the_share_does_not_count() {
            use wicked_apps_core::GraphRead;
            let mut s = store();
            let f = "src/wire.ts";
            let shape = shaped(
                &mut s,
                &format!("{TS}src/wire/Wire#"),
                NodeKind::Interface,
                f,
                "typescript",
                (0, 60),
                (1, 3),
                "export interface Wire {",
            );
            let run = shaped(
                &mut s,
                &format!("{TS}src/wire/run()."),
                NodeKind::Function,
                f,
                "typescript",
                (80, 200),
                (5, 9),
                "export function run(w: Wire): void {",
            );
            let in_test = shaped(
                &mut s,
                &format!("{TS}src/wire.test/helper()."),
                NodeKind::Function,
                "src/wire.test.ts",
                "typescript",
                (0, 50),
                (1, 3),
                "function helper() {",
            );
            map(&mut s, &shape, "seat", "RULE-1");
            map(&mut s, &run, "seat", "RULE-2");
            let mut model = build_at(&s, 100);
            assert_eq!(
                model.domains["src"].requirements.len(),
                2,
                "an annotated shape still emits a row; the test-file helper does not"
            );
            // Rows a build no longer makes but an older graph may hold: a unit outside product code,
            // a unit since removed from the index, and one from before `code_symbol` (no claim).
            let reqs = &mut model.domains.get_mut("src").unwrap().requirements;
            let row = |code: Option<&str>| Requirement {
                title: "older".into(),
                description: "older".into(),
                business_rules: vec![Rule {
                    id: "RULE-001".into(),
                    statement: "older holds".into(),
                    confidence: 0.9,
                    provenance: Provenance {
                        source: "code-graph".into(),
                        reference: "src/x.ts#older".into(),
                        source_kinds: vec!["code-body".into()],
                    },
                    source_ref: None,
                }],
                code_symbol: code.map(str::to_string),
                ..Default::default()
            };
            reqs.insert("REQ-900".into(), row(Some(in_test.symbol.as_str())));
            reqs.insert("REQ-901".into(), row(Some(&format!("{TS}src/gone/old()."))));
            reqs.insert("REQ-902".into(), row(None));
            persist_domain_model(&mut s, &model).unwrap();

            let (graph, _, units) = load_domain_graph_with_units(&s).unwrap().unwrap();
            assert_eq!(
                units["src"].len(),
                4,
                "every row with a code_symbol: {units:?}"
            );
            let (report, uncounted) = recompute_front_half_coverage_labelling(&s, &units).unwrap();
            assert_eq!(report, cov(&s), "the same report as the plain recompute");
            assert_eq!((report.behavior_bearing, report.resolved), (1, 1));
            let shape_req = model.domains["src"]
                .requirements
                .iter()
                .find(|(_, r)| r.title == "Wire")
                .map(|(k, _)| k.clone())
                .unwrap();
            let want: UncountedRequirements = [(
                "src".to_string(),
                [
                    (shape_req, Uncounted::Structural),
                    ("REQ-900".to_string(), Uncounted::NotProduct),
                    ("REQ-901".to_string(), Uncounted::Unindexed),
                ]
                .into_iter()
                .collect(),
            )]
            .into_iter()
            .collect();
            assert_eq!(uncounted, want);
            // Listed rows minus the labelled ones and the one with no claim = what the share counts.
            let listed = graph["domains"]["src"]["requirements"]
                .as_object()
                .unwrap()
                .len();
            assert_eq!(listed - want["src"].len() - 1, report.resolved as usize);
            assert_eq!(
                serde_json::to_value(&uncounted).unwrap()["src"]["REQ-900"],
                "not_product"
            );
            assert!(s.get_node(&run.symbol).unwrap().is_some());
        }

        /// What keeps a TS interface or its member counted (xreview r1 + r2 on core#1238): the
        /// interface's own behaviour edge; a class merged into the same symbol (its method or field
        /// lies outside the interface's declaration); a member with no span (membership unproven);
        /// an in-declaration member with its own behaviour edge.
        #[test]
        fn ts_interface_counts_when_anything_proves_behaviour() {
            let mut s = store();
            let f = "src/a.ts";
            let ts = |t: &str| format!("{TS}src/a/{t}");
            let i = |s: &mut SqliteStore, t: &str, b: (u32, u32)| {
                shaped(
                    s,
                    &ts(t),
                    NodeKind::Interface,
                    f,
                    "typescript",
                    b,
                    (b.0, b.0 + 1),
                    "interface X {",
                )
            };
            let policy = i(&mut s, "Policy#", (0, 100));
            let decide = shaped(
                &mut s,
                &ts("decide()."),
                NodeKind::Function,
                f,
                "typescript",
                (120, 200),
                (6, 9),
                "function decide() {",
            );
            calls(&mut s, &policy, &decide);
            // Declaration merging: `class Merged { fee() {…} }` + `interface Merged { sig(): void }`.
            i(&mut s, "Merged#", (300, 360));
            shaped(
                &mut s,
                &ts("Merged#sig()."),
                NodeKind::Method,
                f,
                "typescript",
                (320, 340),
                (21, 21),
                "sig(): void",
            );
            shaped(
                &mut s,
                &ts("Merged#fee()."),
                NodeKind::Method,
                f,
                "typescript",
                (220, 280),
                (12, 14),
                "fee() {",
            );
            // A merged class field with an initializer that calls: `class Live { data = { a: charge() } }`.
            i(&mut s, "Live#", (400, 460));
            let data = shaped(
                &mut s,
                &ts("Live#data."),
                NodeKind::Field,
                f,
                "typescript",
                (380, 398),
                (30, 30),
                "data = { a: charge() };",
            );
            calls(&mut s, &data, &decide);
            // A merged class's PRIVATE field (`#data = charge()`): estate renders `Live2##data.`.
            i(&mut s, "Live2#", (600, 640));
            let pdata = shaped(
                &mut s,
                &ts("Live2##data."),
                NodeKind::Field,
                f,
                "typescript",
                (580, 598),
                (50, 50),
                "#data = { a: charge() };",
            );
            calls(&mut s, &pdata, &decide);
            // An in-declaration member with its own behaviour edge.
            i(&mut s, "Edged#", (500, 560));
            let run = shaped(
                &mut s,
                &ts("Edged#run()."),
                NodeKind::Method,
                f,
                "typescript",
                (520, 540),
                (41, 41),
                "run(): void",
            );
            calls(&mut s, &run, &decide);
            // An interface at byte 0 and a zero-span method of it: no span, no proof.
            shaped(
                &mut s,
                &format!("{TS}src/b/Zero#"),
                NodeKind::Interface,
                "src/b.ts",
                "typescript",
                (0, 90),
                (1, 3),
                "interface Zero {",
            );
            shaped(
                &mut s,
                &format!("{TS}src/b/Zero#run()."),
                NodeKind::Method,
                "src/b.ts",
                "typescript",
                (0, 0),
                (0, 0),
                "run() { return 7; }",
            );
            let r = cov(&s);
            assert_eq!(
                listed(&r),
                vec![
                    "src/a/Edged#",
                    "src/a/Edged#run().",
                    "src/a/Live#",
                    "src/a/Live2#",
                    "src/a/Merged#",
                    "src/a/Merged#fee().",
                    "src/a/Policy#",
                    "src/a/decide().",
                    "src/b/Zero#",
                    "src/b/Zero#run().",
                ],
                "{r:?}"
            );
        }

        /// Interfaces and traits outside TypeScript keep counting, members or not: a Java interface
        /// constant (`Object LOCK = new Object();`) or a Scala trait initializer can execute and is
        /// not always extracted, a C# property body is a Field, and estate gives overloads one symbol,
        /// so a `…;` signature does not prove the symbol has no body. A Rust trait's default method
        /// counts as before.
        #[test]
        fn interfaces_and_traits_outside_typescript_keep_counting() {
            let mut s = store();
            let f = "src/main/java/a/Pay.java";
            shaped(
                &mut s,
                "java . . . a/Pay#",
                NodeKind::Interface,
                f,
                "java",
                (0, 300),
                (1, 10),
                "public interface Pay {",
            );
            shaped(
                &mut s,
                "java . . . a/Pay#fee().",
                NodeKind::Method,
                f,
                "java",
                (30, 70),
                (2, 2),
                "long fee(Order order);",
            );
            shaped(
                &mut s,
                "java . . . a/Locks#",
                NodeKind::Interface,
                f,
                "java",
                (310, 400),
                (12, 14),
                "interface Locks { Object LOCK = new Object(); }",
            );
            shaped(
                &mut s,
                "csharp . . . a/Fees#",
                NodeKind::Interface,
                "Fees.cs",
                "csharp",
                (0, 90),
                (1, 3),
                "interface Fees {",
            );
            let r = "rust . . . src/team/vault/";
            shaped(
                &mut s,
                &format!("{r}EvidenceVault#"),
                NodeKind::Trait,
                "src/team/vault.rs",
                "rust",
                (0, 200),
                (1, 8),
                "pub trait EvidenceVault: Send + Sync {",
            );
            shaped(
                &mut s,
                &format!("{r}Hooks#on_shutdown()."),
                NodeKind::Function,
                "src/team/vault.rs",
                "rust",
                (240, 280),
                (11, 11),
                "fn on_shutdown(&self) {}",
            );
            let rep = cov(&s);
            assert_eq!(rep.behavior_bearing, 6, "{rep:?}");
        }

        /// Constructors are unchanged: an empty `{}` body can still run parameter defaults,
        /// destructuring and decorators, so a one-line TS constructor keeps counting.
        #[test]
        fn constructors_keep_counting() {
            let mut s = store();
            let f = "src/cache.ts";
            shaped(
                &mut s,
                &format!("{TS}src/cache/C#constructor()."),
                NodeKind::Method,
                f,
                "typescript",
                (0, 60),
                (3, 3),
                "constructor(private readonly deps: CacheDeps) {}",
            );
            shaped(
                &mut s,
                &format!("{TS}src/cache/D#constructor()."),
                NodeKind::Method,
                f,
                "typescript",
                (100, 160),
                (9, 9),
                "constructor([first]: Iterable<number>) {}",
            );
            assert_eq!(cov(&s).behavior_bearing, 2);
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

        /// Operator decision 2026-11-01, on pi-governance's shape: JSON keys (indexed as `Struct`),
        /// package.json fields and test functions are not behaviour-bearing; the product's own
        /// function is, and a placeholder on it is listed for the next page to revisit but is not
        /// a real requirement. Falsifier: drop the `is_product_file` check in the recompute and
        /// `behavior_bearing` reads 5.
        #[test]
        fn only_product_code_is_counted_and_placeholders_are_listed() {
            let mut s = store();
            let decide = node(&mut s, "decide", NodeKind::Function, "extensions/gov.js");
            let envelope = node(&mut s, "envelope", NodeKind::Function, "extensions/gov.js");
            node(
                &mut s,
                "capture",
                NodeKind::Function,
                "test/live/capture.mjs",
            );
            node(
                &mut s,
                "summary",
                NodeKind::Struct,
                "evidence/a/summary.json",
            );
            node(&mut s, "scripts", NodeKind::Struct, "package.json");
            s.annotate(
                &decide.symbol,
                Annotation::new("business_rule", "RULE-1", "a call proceeds only on true")
                    .with_confidence(0.9),
            )
            .unwrap();
            s.annotate(
                &envelope.symbol,
                Annotation::new("risk", "RULE-2", "[RISK] envelope: empty statement")
                    .with_confidence(0.0),
            )
            .unwrap();
            let r = cov(&s);
            assert_eq!(
                (
                    r.behavior_bearing,
                    r.resolved,
                    r.risk_flagged,
                    r.unaccounted,
                    r.not_product
                ),
                (2, 1, 1, 0, 3),
                "{r:?}"
            );
            assert_eq!(
                r.coverage, 1.0,
                "the accounted ratio is unchanged in meaning"
            );
            assert_eq!(real_pct(&r), 50, "the placeholder does not count");
            let listed: Vec<&str> = r
                .placeholder_nodes
                .iter()
                .map(|n| n.symbol_id.as_str())
                .collect();
            assert_eq!(listed, vec![envelope.symbol.as_str()]);
            assert!(r.unaccounted_nodes.is_empty());
        }

        /// An extracted rule is executable behaviour whatever its file (codex on #1234): a
        /// Salesforce Flow (`.flow-meta.xml`) or a cloud policy (`.json`) Rule stays in the
        /// denominator, while a JSON key beside it does not.
        #[test]
        fn an_extracted_rule_in_a_data_extension_file_still_counts() {
            let mut s = store();
            node(
                &mut s,
                "Approve_Discount",
                NodeKind::Rule,
                "flows/Approve.flow-meta.xml",
            );
            node(
                &mut s,
                "deny-public-buckets",
                NodeKind::Rule,
                "policies/storage.json",
            );
            node(&mut s, "name", NodeKind::Struct, "package.json");
            // …but a rule in a test fixture is a test (codex r2 on #1234).
            node(
                &mut s,
                "fixture-policy",
                NodeKind::Rule,
                "tests/fixtures/policy.json",
            );
            let r = cov(&s);
            assert_eq!(
                (r.behavior_bearing, r.unaccounted, r.not_product),
                (2, 2, 2),
                "{r:?}"
            );
            assert_eq!(
                real_pct(&r),
                0,
                "two unmapped executable rules are not 100%"
            );
        }

        /// #131, in miniature: coverage 1.0 with the accounting carrying no content.
        ///
        /// Live, this was 46 distinct strings across 34,897 nodes, every one
        /// `requirement_validated=1`. The gate passed, `domain-graph` translated, and the output was
        /// file-name titles over reference lists. Nothing in the RESOLVED predicate can see that — it
        /// truthy-checks a string — so the report has to carry the concentration for a reader to.
        #[test]
        fn bulk_mapped_requirements_reach_full_coverage_but_are_visible_in_the_report() {
            let mut st = store();
            for i in 0..12 {
                let n = node(&mut st, &format!("N{i}"), NodeKind::Function, "a.rs");
                // Two strings over twelve nodes — the same shape as 46 over 34,897.
                let req = if i % 2 == 0 { "NFR-76" } else { "NFR-77" };
                st.set_node_semantics(
                    &n.symbol,
                    None,
                    Some(req),
                    Some(&ValidationClaim::new(true, "test-fixture").unwrap()),
                )
                .unwrap();
            }
            let r = recompute_front_half_coverage(&st).unwrap();

            // The gate is satisfied AT THIS SCALE. 12 nodes is below COVERAGE_MIN_JUDGEABLE_NODES
            // (100): a graph this small cannot be told apart from a small-but-legitimate catalog, so
            // the concentration gate deliberately does not fire here. The enforcement at real scale is
            // proved by `bulk_mapping_is_denied_at_scale` below; this test now only asserts that the
            // signal is REPORTED and that small graphs are not falsely denied.
            assert_eq!(r.unaccounted, 0);
            assert_eq!(r.coverage, 1.0);
            assert_eq!(r.resolved, 12);
            assert!(
                assert_front_half_coverage(&r).is_ok(),
                "a 12-node graph is below the judgeable threshold — concentration is not gated at this scale"
            );

            // …and the report now says so out loud.
            let reuse = r
                .requirement_reuse
                .expect("report carries the reuse signal");
            assert_eq!(reuse.nodes_with_requirement, 12);
            assert_eq!(reuse.distinct_requirements, 2);
            assert_eq!(reuse.max_nodes_per_requirement, 6);
            assert_eq!(reuse.most_reused_requirement.as_deref(), Some("NFR-77"));
        }

        /// #131 ENFORCEMENT. The live abuse at scale: many nodes, a handful of requirement strings.
        /// coverage is a legitimate 1.0, but the accounting is content-free — and now the gate DENIES
        /// it instead of merely reporting it. 300 nodes over 2 strings = avg 150, unambiguously past
        /// the default bar of 100 (the check is `avg > max_avg`, so avg exactly 100 would NOT deny).
        #[test]
        fn bulk_mapping_is_denied_at_scale() {
            let mut st = store();
            for i in 0..300 {
                let n = node(&mut st, &format!("B{i}"), NodeKind::Function, "a.rs");
                let req = if i % 2 == 0 { "NFR-76" } else { "NFR-77" };
                st.set_node_semantics(
                    &n.symbol,
                    None,
                    Some(req),
                    Some(&ValidationClaim::new(true, "test-fixture").unwrap()),
                )
                .unwrap();
            }
            let r = recompute_front_half_coverage(&st).unwrap();
            // coverage is genuinely complete — the point is that completeness is not substance.
            assert_eq!(r.unaccounted, 0);
            assert_eq!(r.coverage, 1.0);
            let err = assert_front_half_coverage(&r).expect_err(
                "300 nodes over 2 requirement strings is bulk-mapping — must be DENIED",
            );
            let msg = err.to_string();
            assert!(msg.contains("content-free"), "names the condition: {msg}");
            assert!(
                msg.contains("150.0 nodes per requirement") || msg.contains("per requirement"),
                "reports the concentration: {msg}"
            );
        }

        /// FALSIFIER for the gate above: the SAME node count with a real, diverse catalog (one distinct
        /// requirement per node) is NOT denied. Proves the gate keys on concentration, not on graph
        /// size — flip the `avg > max_avg` comparison in production and this test starts failing.
        #[test]
        fn a_diverse_catalog_at_the_same_scale_is_not_denied() {
            let mut st = store();
            for i in 0..300 {
                let n = node(&mut st, &format!("D{i}"), NodeKind::Function, "a.rs");
                st.set_node_semantics(
                    &n.symbol,
                    None,
                    Some(&format!("REQ-{i}")), // 300 distinct → avg 1.0 node/requirement
                    Some(&ValidationClaim::new(true, "test-fixture").unwrap()),
                )
                .unwrap();
            }
            let r = recompute_front_half_coverage(&st).unwrap();
            assert_eq!(r.unaccounted, 0);
            assert!(
                assert_front_half_coverage(&r).is_ok(),
                "a genuine one-requirement-per-node catalog must pass — the gate keys on \
                 concentration, not on how many nodes there are"
            );
        }

        /// The bound is operator-tunable via the environment. Tested against the `env_f64` helper with
        /// a DEDICATED probe var — NOT by mutating `WICKED_COVERAGE_MAX_AVG_NODES_PER_REQ` at runtime,
        /// which would race the parallel gate tests above that read it (a set-env test is a classic
        /// source of flakiness). The gate's use of this helper is a single direct call, so proving the
        /// helper reads + parses + falls back is what makes the override trustworthy.
        #[test]
        fn env_tunables_read_parse_and_fall_back() {
            // A unique name nothing else reads, so no cross-test interference even in parallel.
            let key = "WICKED_TEST_COVERAGE_TUNABLE_PROBE";
            std::env::remove_var(key);
            assert_eq!(env_f64(key, 100.0), 100.0, "absent → default");
            assert_eq!(env_u64(key, 7), 7, "absent → default");
            std::env::set_var(key, "1000");
            assert_eq!(env_f64(key, 100.0), 1000.0, "present + valid → override");
            assert_eq!(env_u64(key, 7), 1000, "present + valid → override");
            std::env::set_var(key, "not-a-number");
            assert_eq!(
                env_f64(key, 100.0),
                100.0,
                "unparseable → default, not a panic"
            );
            std::env::set_var(key, "inf");
            assert_eq!(
                env_f64(key, 100.0),
                100.0,
                "non-finite → default (a bar of ∞ disables silently)"
            );
            std::env::remove_var(key);
        }

        /// The signal must not fire on ordinary extraction, or it is noise nobody reads.
        #[test]
        fn one_requirement_per_node_reports_no_concentration() {
            let mut st = store();
            for i in 0..5 {
                let n = node(&mut st, &format!("M{i}"), NodeKind::Function, "a.rs");
                st.set_node_semantics(
                    &n.symbol,
                    None,
                    Some(&format!("REQ-{i}")),
                    Some(&ValidationClaim::new(true, "test-fixture").unwrap()),
                )
                .unwrap();
            }
            let reuse = recompute_front_half_coverage(&st)
                .unwrap()
                .requirement_reuse
                .expect("carries the signal");
            assert_eq!(reuse.distinct_requirements, 5);
            assert_eq!(reuse.max_nodes_per_requirement, 1);
        }

        /// The truncation must honour the number the schema enforces (`maxLength: 120`), ellipsis
        /// included — the first version took 120 chars and then appended one more.
        #[test]
        fn a_long_requirement_is_truncated_to_the_length_the_schema_allows() {
            let mut st = store();
            let long = "R".repeat(400);
            let n = node(&mut st, "L", NodeKind::Function, "a.rs");
            st.set_node_semantics(
                &n.symbol,
                None,
                Some(&long),
                Some(&ValidationClaim::new(true, "test-fixture").unwrap()),
            )
            .unwrap();
            let reuse = recompute_front_half_coverage(&st)
                .unwrap()
                .requirement_reuse
                .expect("carries the signal");
            let shown = reuse
                .most_reused_requirement
                .expect("present when the object is");
            assert_eq!(
                shown.chars().count(),
                120,
                "schema pins maxLength 120: {shown}"
            );
            assert!(shown.ends_with('…'));
        }

        /// Absent, not a record of zeroes — so `most_reused_requirement` can be REQUIRED whenever
        /// the object is present and a consumer never meets a half-populated one.
        #[test]
        fn no_requirements_at_all_omits_the_reuse_record() {
            let mut st = store();
            let n = node(&mut st, "Bare", NodeKind::Function, "a.rs");
            st.annotate(&n.symbol, Annotation::new("risk", "r", "unreviewed"))
                .unwrap();
            let r = recompute_front_half_coverage(&st).unwrap();
            assert!(r.requirement_reuse.is_none(), "{:?}", r.requirement_reuse);
        }

        #[test]
        fn resolved_risk_split_and_mean_confidence_resolved_only() {
            let mut s = store();
            // resolved via a validated requirement (no annotation confidence).
            let a = node(&mut s, "A", NodeKind::Function, "a.rs");
            s.set_node_semantics(
                &a.symbol,
                None,
                Some("REQ-1"),
                Some(&ValidationClaim::new(true, "test-fixture").unwrap()),
            )
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
                s.set_node_semantics(
                    &n.symbol,
                    None,
                    Some(req),
                    Some(&ValidationClaim::new(true, "test-fixture").unwrap()),
                )
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
            s.set_node_semantics(
                &a.symbol,
                None,
                Some("REQ"),
                Some(&ValidationClaim::new(true, "test-fixture").unwrap()),
            )
            .unwrap();
            let b = node(&mut s, "B", NodeKind::Function, "top.rs"); // repo root
            s.set_node_semantics(
                &b.symbol,
                None,
                Some("REQ"),
                Some(&ValidationClaim::new(true, "test-fixture").unwrap()),
            )
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
