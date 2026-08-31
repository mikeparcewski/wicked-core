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
//! `framework` are WILDCARD facets (an ABSENT facet applies to all), `severity`/`rule_type`/
//! `steering_type` are exact, results ordered severity-first (critical→info) then weight (desc)
//! then id — deterministic, enforcement-ready. Retired and DECIDE-lane (effect-bearing) rules
//! never recall; `list_rules` is the management view that shows them. (Wiring recall INTO the
//! per-output gate is PR-C; this module is the population + query half.)

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, ConformanceClaim, Decision, Edge, EdgeKind, FromNode, GraphRead, GraphStore,
    Language, Location, Metadata, Node, NodeKind, Span, SymbolId, ToNode, SYMBOL_SCHEME,
};
use wicked_estate_core::{Confidence, Direction, Provenance, SymbolQuery};

use crate::domain::{Effect, Trigger};

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
pub(crate) const VALID_SOURCE_KINDS: [&str; 4] = ["code-body", "type-def", "comment", "doc"];

/// The steering-type vocabulary (STEERING program) — one sub-page per type in studio's Steering
/// surface. Enum-as-STRING on the wire (a new type is a vocabulary bump, not a serde break),
/// validated fail-closed at the write boundary (INV-S1).
pub const STEERING_TYPES: [&str; 7] = [
    "architecture",
    "development",
    "security",
    "testing",
    "operations",
    "compliance",
    "design-ux",
];

/// The `steering_type` every pre-steering row reads back as (serde default — the additive
/// metadata-key migration: existing rows carry no key and default here on every read).
pub const DEFAULT_STEERING_TYPE: &str = "architecture";

fn default_steering_type() -> String {
    DEFAULT_STEERING_TYPE.to_string()
}

fn is_default_steering_type(s: &str) -> bool {
    s == DEFAULT_STEERING_TYPE
}

/// The `weight` every pre-steering row reads back as (serde default, same migration story).
pub const DEFAULT_RULE_WEIGHT: f32 = 1.0;

fn default_weight() -> f32 {
    DEFAULT_RULE_WEIGHT
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if signature
fn is_default_weight(w: &f32) -> bool {
    *w == DEFAULT_RULE_WEIGHT
}

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

/// A prescriptive conformance rule. Field set mirrors the `conformance-rules` schema this crate
/// now OWNS (`schemas/conformance-rules.schema.json`, re-homed from the retired wicked-brain repo,
/// RET-BRAIN-DOMAIN-001 / AW-2) — the wire contract downstream consumers still speak.
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
    /// Withdrawn from recall. See [`crate::domain::Policy::retired`] — same contract, same reason
    /// for the `serde(default)`: rules written before the field existed read back as active.
    #[serde(default)]
    pub retired: bool,

    // ── STEERING unification (the wiki/rules model + the standalone Policy model merged into ONE
    //    steering-rule model). Every field below is optional/defaulted AND skipped at its default,
    //    so every pre-steering row parses (the additive metadata-key migration happens on READ) and
    //    a rule that uses none of them serializes byte-identical to the 2.x wire shape.
    /// Which steering page this rule belongs to — one of [`STEERING_TYPES`] (enum-as-string,
    /// INV-S1 fail-closed). Pre-steering rows default to [`DEFAULT_STEERING_TYPE`].
    #[serde(
        default = "default_steering_type",
        skip_serializing_if = "is_default_steering_type"
    )]
    pub steering_type: String,
    /// Phase/tool INCLUSION — the same exact-token-match semantics the merged standalone
    /// [`crate::domain::Policy::applies_to`] had in SELECT. Empty = not phase-scoped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applies_to: Vec<String>,
    /// The EXCLUSION twin of `applies_to`: a phase token listed here withdraws the rule from that
    /// phase even when `applies_to` (or a wildcard) would include it — exclusion dominates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<String>,
    /// Ordering weight within a severity band (recall/list order severity → weight DESC → id) and
    /// the stored gate-priority signal. Default 1.0 (INV-S2: finite, ≥ 0). NOTE: `decide()` keeps
    /// the merged Policy model's precedence (severity → id) so a migrated policy's decisions stay
    /// byte-equal; threading weight into gate precedence is the follow-up that moves `decide` onto
    /// steering rules natively.
    #[serde(default = "default_weight", skip_serializing_if = "is_default_weight")]
    pub weight: f32,
    /// The enforcement effect, from the merged standalone Policy model. `None` ⇒ the rule is
    /// RECALL-ONLY, exactly as every wiki rule was before the merge; `Some` ⇒ the rule is
    /// decide-lane (SELECT/DECIDE pick it up) and is therefore NOT attached again by recall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<Effect>,
    /// The condition under which an effect-bearing rule fires (merged `Policy.trigger`;
    /// `None` ⇔ the old `Trigger::default()` — fires whenever phase-selected).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<Trigger>,
    /// Obligations collected when a triggered `AllowWithConditions` effect fires (merged
    /// `Policy.obligations`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub obligations: Vec<String>,
    /// Frozen acceptance-criteria text (merged `Policy.criteria` — becomes the claim's criteria).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub criteria: String,
}

/// The all-defaults rule — INVALID as-is (empty id), useful as `..Default::default()` filler so
/// the additive steering fields never force every construction site to spell them out.
impl Default for ConformanceRule {
    fn default() -> Self {
        Self {
            id: String::new(),
            rule_type: RuleType::Pattern,
            statement: String::new(),
            severity: ConfSeverity::Info,
            confidence: 1.0,
            targets: Targets::default(),
            symbol_ref: None,
            compliance: None,
            provenance: RuleProvenance::default(),
            retired: false,
            steering_type: default_steering_type(),
            applies_to: Vec::new(),
            excludes: Vec::new(),
            weight: DEFAULT_RULE_WEIGHT,
            effect: None,
            trigger: None,
            obligations: Vec::new(),
            criteria: String::new(),
        }
    }
}

impl ConformanceRule {
    /// Fail-closed write-time invariants (ported from `conformance-store` INV-C1/INV-C2, extended
    /// by the STEERING unification INV-S1..S3). INV-C3 (bundle-unique ids) is enforced at ingest,
    /// where the whole bundle is visible.
    pub fn validate(&self) -> anyhow::Result<()> {
        // INV-C1 (steering-scoped): `PAT-`/`POL-` is the RESERVED doc-ingest namespace — an id in
        // it must match the wire contract `^(PAT|POL)-[0-9]{3,6}$` AND its prefix must agree with
        // rule_type (PAT-⇔pattern, POL-⇔policy), exactly as before the merge. Ids OUTSIDE the
        // reserved namespace (migrated policies keep their original ids unchanged — audit
        // resolvability; UI/chat-authored steering rules mint their own) need only be non-blank.
        if self.id.starts_with("PAT-") || self.id.starts_with("POL-") {
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
        } else if self.id.trim().is_empty() {
            anyhow::bail!("INV-C1: rule id must not be blank");
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
        // INV-S1: steering_type is enum-as-string — an out-of-vocabulary value fails closed so a
        // typo'd type can never mint a rule no Steering sub-page lists.
        if !STEERING_TYPES.contains(&self.steering_type.as_str()) {
            anyhow::bail!(
                "INV-S1: steering_type {:?} is not one of {STEERING_TYPES:?}",
                self.steering_type
            );
        }
        // INV-S2: weight orders recall and carries gate priority — NaN/∞/negative would make the
        // ordering non-deterministic (NaN is unordered) or nonsensical.
        if !self.weight.is_finite() || self.weight < 0.0 {
            anyhow::bail!(
                "INV-S2: weight must be a finite number ≥ 0, got {}",
                self.weight
            );
        }
        // INV-S3 (mirrors the merged Policy::validate, same fail-open reasons): an EFFECT-bearing
        // rule with no non-blank applies_to entry is selected for NO phase and enforces nothing —
        // a silent fail-open on the primary safety control; and a malformed trigger regex fails
        // CLOSED in the engine (never fires), so an invalid regex is a silent dead Deny.
        if self.effect.is_some() && self.applies_to.iter().all(|p| p.trim().is_empty()) {
            anyhow::bail!(
                "INV-S3: steering rule {:?} carries an effect but no non-blank applies_to entry — \
                 it is selected for no phase and enforces nothing (fail-loud: a non-enforcing \
                 enforcement rule must not silently register)",
                self.id
            );
        }
        if let Some(pattern) = self.trigger.as_ref().and_then(|t| t.contains.as_deref()) {
            if let Err(e) = regex::Regex::new(pattern) {
                anyhow::bail!(
                    "INV-S3: steering rule {:?} trigger.contains {:?} is not a valid regex — it \
                     would never fire (a silent fail-open): {e}",
                    self.id,
                    pattern
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
            evidence_count: 0,
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
///
/// After the commit, a COARSE fire-and-forget [`crate::events::EV_RULE_INGESTED`] event goes out
/// through the shared emit seam (AW-22 / arch-R24) — a bus failure must NOT fail registration;
/// the durable record is the rule node just committed.
pub fn register_rule(store: &mut dyn GraphStore, rule: &ConformanceRule) -> anyhow::Result<()> {
    rule.validate()?;
    store.begin_batch()?;
    store.upsert_nodes(&[rule.to_node()])?;
    store.commit_batch()?;
    let _ = crate::events::emit_rule_ingested(rule);
    Ok(())
}

/// Withdraw `id` from recall. Returns `false` if no such rule exists.
///
/// Retire, not delete — see [`crate::engine::retire_policy`] for why the node has to survive.
///
/// On an ACTUAL state change (active → retired), a COARSE fire-and-forget
/// [`crate::events::EV_RULE_RETIRED`] event goes out after the commit (AW-22 / arch-R24) — the
/// wave-5 propagation trigger. Retiring an already-retired rule reports success but emits
/// nothing: no state change, no event.
pub fn retire_rule(store: &mut dyn GraphStore, id: &str) -> anyhow::Result<bool> {
    // O(1) by synthetic symbol rather than a scan-and-filter over every rule node — see
    // [`crate::engine::retire_policy`].
    let symbol = synthetic_symbol(CONFORMANCE_RULE, id);
    let Some(node) = store.get_node(&symbol)? else {
        return Ok(false);
    };
    if node.kind != NodeKind::Rule {
        return Ok(false);
    }
    let mut rule = ConformanceRule::from_node(&node)?;
    // Same reasoning as [`crate::engine::retire_policy`]: the write recomputes the symbol from
    // `rule.id`, so a node filed under a symbol that disagrees with its own metadata would retire
    // something else and report success.
    if rule.id != id {
        anyhow::bail!(
            "conformance graph is inconsistent: node at {symbol} carries id {:?}, not {id:?}",
            rule.id
        );
    }
    if rule.retired {
        return Ok(true);
    }
    rule.retired = true;
    store.begin_batch()?;
    store.upsert_nodes(std::slice::from_ref(&rule.to_node()))?;
    store.commit_batch()?;
    let _ = crate::events::emit_rule_retired(&rule);
    Ok(true)
}

// ─────────────────────────────────────────────────────────────────────────────
// Enforcement evidence — evidenced_by edges + the Governs evidence_count bump (AW-23 / arch-R23)
// ─────────────────────────────────────────────────────────────────────────────

/// Edge-kind spelling for the claim → rule enforcement-evidence edge. Stringly (`EdgeKind::Other`)
/// by design: both endpoints are wicked-apps synthetic nodes, so the native code-lane vocabulary
/// pin (AW-19 — native `Governs` for code targets) does not claim it, and estate has no native
/// variant for "this claim is the enforcement evidence for that rule". The spelling is the
/// program-wide vocabulary from arch-R13a/R23 (`evidenced_by`).
pub const EVIDENCED_BY: &str = "evidenced_by";

/// What [`record_rule_evidence`] did for one claim — every cited rule is accounted for, nothing
/// is silently dropped (a missing rule node is REPORTED, never an error: a claim replayed onto a
/// store that never held the rule must not fail conformance recording).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RuleEvidenceReport {
    /// Rule ids the claim's `conform:<severity>:<id>:<statement>` obligations cite (deduped,
    /// citation order).
    pub cited: Vec<String>,
    /// Rules that got a NEW `evidenced_by` edge (and whose Governs edges were bumped).
    pub evidenced: Vec<String>,
    /// Rules this claim had already evidenced (same claim re-conformed — idempotent, no
    /// double-count).
    pub already_recorded: Vec<String>,
    /// Cited ids with no conformance-rule node in this store (reported, never an error).
    pub missing: Vec<String>,
    /// How many rule → code `Governs` edges had their `evidence_count` incremented.
    pub governs_bumped: usize,
}

/// Does `id` have the conformance-rule wire shape (`^(PAT|POL)-[0-9]{3,6}$`, INV-C1)? Used to pick
/// rule citations out of obligation text without ever treating free-form obligation prose as an id.
fn is_rule_id_shape(id: &str) -> bool {
    ["PAT-", "POL-"].iter().any(|p| {
        id.strip_prefix(p).is_some_and(|ord| {
            (3..=6).contains(&ord.len()) && ord.bytes().all(|b| b.is_ascii_digit())
        })
    })
}

/// The conformance-rule ids a claim cites, parsed from the `conform:<severity>:<id>:<statement>`
/// obligations the recall→gate wiring attaches (`attach_recalled_rules` in wicked-core). Deduped,
/// citation order; anything not shaped like a rule id is skipped (an obligation is free text —
/// only the exact wire shape counts as a citation).
fn cited_rule_ids(claim: &ConformanceClaim) -> Vec<String> {
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for obligation in &claim.obligations {
        let Some(rest) = obligation.strip_prefix("conform:") else {
            continue;
        };
        // <severity>:<id>:<statement> — the statement may itself contain ':', so split at most 3.
        let mut parts = rest.splitn(3, ':');
        let _severity = parts.next();
        let Some(id) = parts.next() else { continue };
        if is_rule_id_shape(id) && seen.insert(id) {
            out.push(id.to_string());
        }
    }
    out
}

/// Record the enforcement evidence a recorded DENIAL carries back onto the rules it cites — the
/// aw14 verifier's flagged follow-up (arch-R13a → arch-R23): `evidence_count` was initialized to 0
/// by [`ConformanceRule::governs_edge`] and never incremented, so an enforced rule and a decaying
/// one looked identical on the graph.
///
/// For each rule the deny claim's `conform:` obligations cite (and that exists in this store):
///
/// 1. write ONE `claim → rule` [`EVIDENCED_BY`] edge (`EdgeKind::Other("evidenced_by")`,
///    `evidence_count = 1` — the claim IS one independent confirmation that the rule enforces);
/// 2. increment `evidence_count` on every existing `rule → code` [`EdgeKind::Governs`] edge the
///    relink pass derived — the audit counter estate defines as "how many times this relationship
///    has been independently confirmed".
///
/// Idempotent per claim: claim ids are content-addressed (same context ⇒ same claim), so a
/// re-conformed claim finds its own `evidenced_by` edge and bumps nothing twice. Distinct denials
/// citing the same rule each count once. Non-deny claims are a no-op — the metric is DENIALS
/// citing wiki rules (arch-R23), not recall breadth.
///
/// Call AFTER the claim node is committed ([`crate::conform`] does): both edge endpoints must
/// exist or the edges would dangle and be pruned by estate's `compact`. A cited rule with no node
/// in this store is reported in [`RuleEvidenceReport::missing`], never an error.
pub fn record_rule_evidence(
    store: &mut dyn GraphStore,
    claim: &ConformanceClaim,
) -> anyhow::Result<RuleEvidenceReport> {
    let mut report = RuleEvidenceReport {
        cited: cited_rule_ids(claim),
        ..Default::default()
    };
    if claim.decision != Decision::Deny || report.cited.is_empty() {
        return Ok(report);
    }

    let claim_sym = crate::engine::claim_symbol(&claim.claim_id);
    // The rules this claim already evidenced (same claim re-conformed) — the idempotence read.
    let already: std::collections::BTreeSet<SymbolId> = store
        .neighbors(&claim_sym, Direction::Dependencies)?
        .into_iter()
        .filter(|e| matches!(&e.kind, EdgeKind::Other(k) if k.as_str() == EVIDENCED_BY))
        .map(|e| e.target)
        .collect();

    let mut edges: Vec<Edge> = Vec::new();
    for id in report.cited.clone() {
        let rule_sym = synthetic_symbol(CONFORMANCE_RULE, &id);
        // Same guard as recall: only OUR conformance rules (native Rule kind at the synthetic
        // symbol) count — a foreign node at that address is "missing", never a write target.
        let is_ours = store
            .get_node(&rule_sym)?
            .is_some_and(|n| n.kind == NodeKind::Rule);
        if !is_ours {
            report.missing.push(id);
            continue;
        }
        if already.contains(&rule_sym) {
            report.already_recorded.push(id);
            continue;
        }
        edges.push(Edge {
            source: claim_sym.clone(),
            target: rule_sym.clone(),
            kind: EdgeKind::Other(EVIDENCED_BY.to_string()),
            // The citation is exact (the claim literally names the rule id) — not a heuristic.
            confidence: Confidence::new(1.0),
            provenance: Provenance::Extractor(OUTGOV_EXTRACTOR.to_string()),
            resolved_by: CONFORMANCE_RESOLVED_BY.to_string(),
            location: None,
            metadata: Metadata::new(),
            evidence_count: 1,
        });
        // Bump the audit counter on the rule's derived Governs edges. The store's upsert keeps
        // the incoming row when `evidence_count` grows (estate 0.14.5 merge rule), so the bump
        // lands even at equal confidence.
        for mut governs in store
            .neighbors(&rule_sym, Direction::Dependencies)?
            .into_iter()
            .filter(|e| e.kind == EdgeKind::Governs)
        {
            governs.evidence_count += 1;
            edges.push(governs);
            report.governs_bumped += 1;
        }
        report.evidenced.push(id);
    }

    if !edges.is_empty() {
        store.begin_batch()?;
        store.upsert_edges(&edges)?;
        store.commit_batch()?;
    }
    Ok(report)
}

/// A recall query slice. Any `None` field matches every value of that facet.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleQuery {
    pub language: Option<String>,
    pub layer: Option<String>,
    pub framework: Option<String>,
    pub severity: Option<ConfSeverity>,
    pub rule_type: Option<RuleType>,
    /// STEERING facet — exact match against [`ConformanceRule::steering_type`] (a pre-steering row
    /// reads back as [`DEFAULT_STEERING_TYPE`] and matches that). Absent field on the 2.x wire
    /// parses as `None` (serde's built-in Option handling), so old query payloads stay valid.
    #[serde(default)]
    pub steering_type: Option<String>,
}

/// The shared scan behind [`recall_rules`] (the enforcement funnel) and [`list_rules`] (the
/// operator/management view). Facet semantics (ported from `conformance-store.recallRules`):
/// `language`/`layer`/`framework` match when the rule's facet is ABSENT (wildcard — applies
/// broadly) OR equals the query; `severity`/`rule_type`/`steering_type` are exact. Ordered
/// severity (critical→info) → weight DESC → rule id — deterministic + enforcement-ready.
fn scan_rules(
    store: &dyn GraphRead,
    query: &RuleQuery,
    include_retired: bool,
    include_enforcing: bool,
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
        if rule.retired && !include_retired {
            continue;
        }
        // Effect-bearing steering rules are DECIDE-lane (SELECT/DECIDE fire them; a triggered
        // effect denies or collects its own obligations). Recall attaching them AGAIN would
        // double-govern one rule in a single gate pass, so the enforcement funnel keeps them out;
        // the listing view includes them.
        if rule.effect.is_some() && !include_enforcing {
            continue;
        }
        if facet_matches(&rule.targets.language, &query.language)
            && facet_matches(&rule.targets.layer, &query.layer)
            && facet_matches(&rule.targets.framework, &query.framework)
            && query.severity.is_none_or(|s| s == rule.severity)
            && query.rule_type.is_none_or(|t| t == rule.rule_type)
            && query
                .steering_type
                .as_deref()
                .is_none_or(|t| t == rule.steering_type)
        {
            matched.push(rule);
        }
    }

    matched.sort_by(|a, b| {
        b.severity
            .rank()
            .cmp(&a.severity.rank())
            // Within a severity band the heavier rule orders first (STEERING weight; ties keep
            // the id order pre-steering rows had, since every default weight is 1.0).
            .then_with(|| b.weight.total_cmp(&a.weight))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(matched)
}

/// Recall the conformance rules that APPLY to `query` — the enforcement funnel: retired rules and
/// effect-bearing (decide-lane) rules are excluded, exactly as before the steering unification
/// (rules without an effect are recall-only, exactly as today; rules WITH an effect reach the gate
/// through SELECT/DECIDE instead). See [`scan_rules`] for facet + ordering semantics.
pub fn recall_rules(
    store: &dyn GraphRead,
    query: &RuleQuery,
) -> anyhow::Result<Vec<ConformanceRule>> {
    scan_rules(store, query, false, false)
}

/// List steering rules for MANAGEMENT (the studio Steering surface / `wicked-core rules list`):
/// the whole unified store — recall-only AND effect-bearing rules — with retired rows included
/// when `include_retired` (the recall-skips-retired listing gap: an operator auditing the corpus
/// must be able to see what was withdrawn, which `recall_rules` rightly never returns). Same
/// facets and ordering as recall ([`scan_rules`]).
pub fn list_rules(
    store: &dyn GraphRead,
    query: &RuleQuery,
    include_retired: bool,
) -> anyhow::Result<Vec<ConformanceRule>> {
    scan_rules(store, query, include_retired, true)
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
            ..Default::default()
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
        crate::events::hermetic_test_spool();
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
        crate::events::hermetic_test_spool();
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
        crate::events::hermetic_test_spool();
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

    // ── retire (FINDING-038) ────────────────────────────────────────────────

    #[test]
    fn a_retired_rule_is_no_longer_recalled() {
        crate::events::hermetic_test_spool();
        let mut store = open_store(Some(":memory:")).unwrap();
        register_rule(
            &mut store,
            &rule(
                "PAT-001",
                RuleType::Pattern,
                ConfSeverity::Warn,
                Targets::default(),
            ),
        )
        .unwrap();
        assert_eq!(
            recall_rules(&store, &RuleQuery::default()).unwrap().len(),
            1,
            "precondition: the rule is recalled"
        );

        assert!(retire_rule(&mut store, "PAT-001").unwrap());
        assert!(
            recall_rules(&store, &RuleQuery::default())
                .unwrap()
                .is_empty(),
            "a retired rule must not be recalled for enforcement"
        );
    }

    #[test]
    fn retiring_a_rule_reports_absence_and_keeps_the_node() {
        crate::events::hermetic_test_spool();
        let mut store = open_store(Some(":memory:")).unwrap();
        assert!(
            !retire_rule(&mut store, "PAT-999").unwrap(),
            "an unknown id must report false"
        );

        let original = rule(
            "PAT-002",
            RuleType::Pattern,
            ConfSeverity::Error,
            Targets::default(),
        );
        register_rule(&mut store, &original).unwrap();
        assert!(retire_rule(&mut store, "PAT-002").unwrap());

        // Retire, not delete — a past claim citing PAT-002 must still resolve.
        let node = store
            .get_node(&synthetic_symbol(CONFORMANCE_RULE, "PAT-002"))
            .unwrap()
            .expect("the node must survive retirement");
        let recovered = ConformanceRule::from_node(&node).unwrap();
        assert!(recovered.retired);
        assert_eq!(recovered.statement, original.statement);
    }

    /// `retire_rule` reads by synthetic symbol but writes back through `to_node()`, which
    /// recomputes that symbol from `rule.id`. A node whose metadata id disagrees with the symbol it
    /// is filed under would therefore retire a DIFFERENT rule and report success — see the twin
    /// test on `retire_policy` (review on #149).
    #[test]
    fn retiring_a_misfiled_rule_errors_instead_of_retiring_a_different_one() {
        crate::events::hermetic_test_spool();
        let mut store = open_store(Some(":memory:")).unwrap();

        let victim = rule(
            "PAT-800",
            RuleType::Pattern,
            ConfSeverity::Error,
            Targets::default(),
        );
        register_rule(&mut store, &victim).unwrap();

        // Metadata says PAT-800; the node is filed under PAT-801.
        let mut node = victim.to_node();
        node.symbol = synthetic_symbol(CONFORMANCE_RULE, "PAT-801");
        store.begin_batch().unwrap();
        store.upsert_nodes(std::slice::from_ref(&node)).unwrap();
        store.commit_batch().unwrap();

        let err = retire_rule(&mut store, "PAT-801")
            .expect_err("a symbol/id mismatch must be an error, not a silent cross-write");
        let msg = err.to_string();
        assert!(
            msg.contains("PAT-801") && msg.contains("PAT-800"),
            "the error must name both ids so the inconsistency is diagnosable, got: {msg}"
        );

        let recalled = recall_rules(&store, &RuleQuery::default()).unwrap();
        assert!(
            recalled.iter().any(|r| r.id == "PAT-800"),
            "the rule nobody asked to retire must still be recalled for enforcement"
        );
    }

    /// The `serde(default)` on `retired` is the back-compat hinge: rules registered before the
    /// field existed have no `retired` key in their metadata bag. Without the default that read is
    /// a hard parse error and every pre-existing rule becomes unrecallable.
    #[test]
    fn a_rule_persisted_before_the_field_existed_reads_back_active() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let mut node = rule(
            "PAT-003",
            RuleType::Pattern,
            ConfSeverity::Info,
            Targets::default(),
        )
        .to_node();
        // Exactly what a node written by the previous release looks like.
        node.metadata.remove("retired");
        store.begin_batch().unwrap();
        store.upsert_nodes(std::slice::from_ref(&node)).unwrap();
        store.commit_batch().unwrap();

        let recalled = recall_rules(&store, &RuleQuery::default()).unwrap();
        assert_eq!(
            recalled.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["PAT-003"],
            "a rule with no `retired` key must read back as active and stay enforceable"
        );
        assert!(!recalled[0].retired);
    }

    fn claim(id: &str, decision: Decision, obligations: Vec<String>) -> ConformanceClaim {
        ConformanceClaim {
            claim_id: id.to_string(),
            scope: "repo:test".into(),
            phase: "build".into(),
            policy_ids: vec![],
            decision,
            obligations,
            evaluated_context_ref: "sha256:test".into(),
            criteria: String::new(),
            evaluator_identity: "wicked-governance@test".into(),
            evaluated_at: 1_750_000_000,
        }
    }

    /// Only the exact `conform:<severity>:<id>:<statement>` wire shape counts as a citation —
    /// free-form obligation prose, non-conform prefixes, and id-shaped text in the statement
    /// position must never be treated as rule ids.
    #[test]
    fn record_rule_evidence_only_counts_denials_and_real_citations() {
        let mut store = open_store(Some(":memory:")).unwrap();
        register_rule(
            &mut store,
            &rule(
                "PAT-100",
                RuleType::Pattern,
                ConfSeverity::Error,
                Targets::default(),
            ),
        )
        .unwrap();
        let obligations = vec![
            "conform:Error:PAT-100:statement for PAT-100".to_string(), // real citation
            "conform:Error:PAT-100:statement for PAT-100".to_string(), // duplicate — deduped
            "conform:Info:PAT-999:never registered".to_string(),       // missing, reported
            "conform:Info:not-an-id:free text".to_string(),            // malformed id — skipped
            "advise:soft obligation POL-777".to_string(),              // not conform: — skipped
            "conform:oops".to_string(),                                // too few parts — skipped
        ];

        // A non-deny claim is a NO-OP: the metric is denials citing wiki rules, not recall breadth.
        let allowed = record_rule_evidence(
            &mut store,
            &claim("c-allow", Decision::Allow, obligations.clone()),
        )
        .unwrap();
        assert_eq!(allowed.evidenced, Vec::<String>::new());
        assert_eq!(allowed.governs_bumped, 0);

        let denied =
            record_rule_evidence(&mut store, &claim("c-deny", Decision::Deny, obligations))
                .unwrap();
        assert_eq!(
            denied.cited,
            vec!["PAT-100", "PAT-999"],
            "deduped, id-shaped only"
        );
        assert_eq!(denied.evidenced, vec!["PAT-100"]);
        assert_eq!(denied.missing, vec!["PAT-999"], "reported, never an error");
        assert_eq!(denied.already_recorded, Vec::<String>::new());
        assert_eq!(
            denied.governs_bumped, 0,
            "no Governs edges without a resolved target"
        );

        // Same claim replayed: its evidenced_by edge already exists — nothing double-counts.
        let replay = record_rule_evidence(
            &mut store,
            &claim(
                "c-deny",
                Decision::Deny,
                vec!["conform:Error:PAT-100:statement for PAT-100".to_string()],
            ),
        )
        .unwrap();
        assert_eq!(replay.already_recorded, vec!["PAT-100"]);
        assert_eq!(replay.evidenced, Vec::<String>::new());
    }

    // ── STEERING unification ────────────────────────────────────────────────

    /// The unified model round-trips: every steering field survives Node + wire JSON, losslessly.
    #[test]
    fn steering_fields_round_trip_through_node_and_wire() {
        let r = ConformanceRule {
            id: "sec-input-validation".into(), // a non-reserved (UI-authored shape) id
            rule_type: RuleType::Policy,
            statement: "validate every external input".into(),
            severity: ConfSeverity::Error,
            confidence: 0.9,
            steering_type: "security".into(),
            applies_to: vec!["build".into(), "review".into()],
            excludes: vec!["clarify".into()],
            weight: 2.5,
            effect: Some(crate::domain::Effect::AllowWithConditions),
            trigger: Some(crate::domain::Trigger {
                contains: Some("input".into()),
            }),
            obligations: vec!["sanitize".into()],
            criteria: "inputs are sanitized".into(),
            provenance: RuleProvenance {
                source: "ui".into(), // UI-authored provenance is first-class
                reference: Some("steering/security#sec-input-validation".into()),
                source_kinds: vec![],
            },
            ..Default::default()
        };
        r.validate().expect("a well-formed steering rule validates");

        // Node round-trip.
        let back = ConformanceRule::from_node(&r.to_node()).unwrap();
        assert_eq!(back, r, "lossless steering round-trip through the node");

        // Wire round-trip.
        let json = serde_json::to_string(&r).unwrap();
        let wire: ConformanceRule = serde_json::from_str(&json).unwrap();
        assert_eq!(wire, r, "lossless steering round-trip over the wire");
    }

    /// The additive migration on READ: a node written before the steering fields existed (no new
    /// keys in its metadata bag) parses and defaults to steering_type=architecture / weight=1.0 —
    /// AND a rule that never sets the new fields serializes byte-identical to the 2.x wire shape.
    #[test]
    fn pre_steering_rows_default_and_default_rows_keep_the_2x_wire_shape() {
        let legacy = rule(
            "PAT-500",
            RuleType::Pattern,
            ConfSeverity::Warn,
            Targets::default(),
        );
        let mut node = legacy.to_node();
        for k in [
            "steering_type",
            "applies_to",
            "excludes",
            "weight",
            "effect",
            "trigger",
            "obligations",
            "criteria",
        ] {
            node.metadata.remove(k);
            // The 2.x node never carried the key in the first place — prove serialization skipped it.
            assert!(
                !legacy.to_node().metadata.contains_key(k),
                "a defaulted `{k}` must not serialize (2.x wire shape preserved)"
            );
        }
        let read = ConformanceRule::from_node(&node).expect("a pre-steering node must parse");
        assert_eq!(read.steering_type, DEFAULT_STEERING_TYPE);
        assert_eq!(read.weight, DEFAULT_RULE_WEIGHT);
        assert!(read.applies_to.is_empty() && read.excludes.is_empty());
        assert!(read.effect.is_none() && read.trigger.is_none());
        assert_eq!(
            read, legacy,
            "defaults reconstruct the exact pre-steering rule"
        );
    }

    #[test]
    fn steering_invariants_fail_closed() {
        // INV-S1: out-of-vocabulary steering_type.
        let mut r = rule(
            "PAT-001",
            RuleType::Pattern,
            ConfSeverity::Info,
            Targets::default(),
        );
        r.steering_type = "vibes".into();
        assert!(r.validate().unwrap_err().to_string().contains("INV-S1"));

        // INV-S2: NaN / negative weight.
        let mut r = rule(
            "PAT-001",
            RuleType::Pattern,
            ConfSeverity::Info,
            Targets::default(),
        );
        r.weight = f32::NAN;
        assert!(r.validate().unwrap_err().to_string().contains("INV-S2"));
        r.weight = -1.0;
        assert!(r.validate().unwrap_err().to_string().contains("INV-S2"));

        // INV-S3: an effect with no applies_to enforces nothing — refuse the silent fail-open.
        let mut r = rule(
            "PAT-001",
            RuleType::Pattern,
            ConfSeverity::Info,
            Targets::default(),
        );
        r.effect = Some(crate::domain::Effect::Deny);
        assert!(r.validate().unwrap_err().to_string().contains("INV-S3"));
        r.applies_to = vec!["build".into()];
        assert!(r.validate().is_ok());

        // INV-S3: a malformed trigger regex would never fire — refuse the silent dead Deny.
        r.trigger = Some(crate::domain::Trigger {
            contains: Some("[unclosed".into()),
        });
        assert!(r.validate().unwrap_err().to_string().contains("INV-S3"));

        // INV-C1 outside the reserved namespace: blank ids refused, free-form ids accepted.
        let mut r = rule(
            "steer-1",
            RuleType::Policy,
            ConfSeverity::Info,
            Targets::default(),
        );
        assert!(
            r.validate().is_ok(),
            "non-reserved ids need only be non-blank"
        );
        r.id = "  ".into();
        assert!(r.validate().unwrap_err().to_string().contains("INV-C1"));
    }

    /// The steering_type facet works end-to-end through recall, and weight orders within a
    /// severity band (severity → weight desc → id).
    #[test]
    fn recall_filters_by_steering_type_and_orders_by_weight_within_severity() {
        crate::events::hermetic_test_spool();
        let mut store = open_store(Some(":memory:")).unwrap();
        let mut sec_heavy = rule(
            "PAT-601",
            RuleType::Pattern,
            ConfSeverity::Error,
            Targets::default(),
        );
        sec_heavy.steering_type = "security".into();
        sec_heavy.weight = 5.0;
        let mut sec_light = rule(
            "PAT-600",
            RuleType::Pattern,
            ConfSeverity::Error,
            Targets::default(),
        );
        sec_light.steering_type = "security".into();
        // default weight 1.0 — must order AFTER the heavier PAT-601 despite the smaller id
        let arch = rule(
            "PAT-599",
            RuleType::Pattern,
            ConfSeverity::Critical,
            Targets::default(),
        ); // defaults to architecture
        for r in [&sec_heavy, &sec_light, &arch] {
            register_rule(&mut store, r).unwrap();
        }

        let sec = recall_rules(
            &store,
            &RuleQuery {
                steering_type: Some("security".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            sec.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["PAT-601", "PAT-600"],
            "type facet exact; weight desc breaks the tie within the error band"
        );

        let arch_only = recall_rules(
            &store,
            &RuleQuery {
                steering_type: Some("architecture".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            arch_only.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["PAT-599"],
            "a pre-steering/defaulted rule is the architecture page's"
        );

        let all = recall_rules(&store, &RuleQuery::default()).unwrap();
        assert_eq!(
            all.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["PAT-599", "PAT-601", "PAT-600"],
            "severity first, then weight desc, then id"
        );
    }

    /// list_rules is the management view: retired rows appear when asked (the recall-skips-retired
    /// listing gap), effect-bearing (decide-lane) rules appear always, and recall keeps excluding
    /// both — the enforcement funnel is unchanged.
    #[test]
    fn list_rules_includes_retired_and_enforcing_rows_recall_never_does() {
        crate::events::hermetic_test_spool();
        let mut store = open_store(Some(":memory:")).unwrap();
        register_rule(
            &mut store,
            &rule(
                "PAT-700",
                RuleType::Pattern,
                ConfSeverity::Warn,
                Targets::default(),
            ),
        )
        .unwrap();
        register_rule(
            &mut store,
            &rule(
                "PAT-701",
                RuleType::Pattern,
                ConfSeverity::Warn,
                Targets::default(),
            ),
        )
        .unwrap();
        retire_rule(&mut store, "PAT-701").unwrap();
        let mut enforcing = rule(
            "pol-block-x",
            RuleType::Policy,
            ConfSeverity::Critical,
            Targets::default(),
        );
        enforcing.effect = Some(crate::domain::Effect::Deny);
        enforcing.applies_to = vec!["build".into()];
        enforcing.steering_type = "operations".into();
        register_rule(&mut store, &enforcing).unwrap();

        let recalled = recall_rules(&store, &RuleQuery::default()).unwrap();
        assert_eq!(
            recalled.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["PAT-700"],
            "recall: no retired rows, no decide-lane rows"
        );

        let active = list_rules(&store, &RuleQuery::default(), false).unwrap();
        assert_eq!(
            active.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["pol-block-x", "PAT-700"],
            "listing includes the enforcing rule (critical orders first)"
        );

        let all = list_rules(&store, &RuleQuery::default(), true).unwrap();
        assert_eq!(
            all.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["pol-block-x", "PAT-700", "PAT-701"],
            "--include-retired: the withdrawn row is listable"
        );
        assert!(
            all.iter().find(|r| r.id == "PAT-701").unwrap().retired,
            "the listed row still says it is retired"
        );
    }
}
