//! Governance domain types and their estate-[`Node`] projection.
//!
//! A [`Policy`] is the typed projection of a governance rule (the Node-era prototype authored
//! these as `policies/<id>.json`; here they live on the SHARED estate store). Persistence rides
//! the [`wicked_apps_core::ToNode`]/[`wicked_apps_core::FromNode`] seam: a policy becomes a
//! `Node(kind = NodeKind::Other(`[`POLICY`](wicked_apps_core::POLICY)`))` whose every field is encoded
//! into `Node.metadata`, keyed by the stable synthetic symbol `wicked-apps synthetic policy/<id>:`.
//!
//! Ported from `wicked-governance/lib/{store,decide}.mjs` — same effect/severity vocabulary, same
//! `trigger.contains` regex semantics, same field set.

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, FromNode, Language, Location, Node, NodeKind, Span, ToNode, POLICY,
    SYMBOL_SCHEME,
};

/// The effect a policy asserts when its trigger fires. Mirrors the prototype's
/// `"deny" | "allow_with_conditions" | "allow"` (store.mjs `VALID_EFFECTS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Hard stop. A triggered `Deny` DOMINATES the decision (decide.mjs).
    Deny,
    /// Permit, but the caller must satisfy the policy's `obligations`.
    AllowWithConditions,
    /// Permit unconditionally.
    Allow,
}

/// Policy precedence weight. Mirrors the prototype's `SEVERITY_RANK` (high > medium > low).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    High,
    Medium,
    Low,
}

impl Severity {
    /// Numeric rank for precedence ordering (decide.mjs `SEVERITY_RANK`): high=3, medium=2, low=1.
    pub fn rank(self) -> u8 {
        match self {
            Severity::High => 3,
            Severity::Medium => 2,
            Severity::Low => 1,
        }
    }
}

/// The condition under which a policy fires. A `contains` is a regex tested over the canonical
/// JSON of the evaluated context (decide.mjs `triggers`). `None` ⇒ the policy fires whenever it
/// was phase-selected (a blanket allow / obligation policy).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trigger {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
}

/// A governance policy — the typed projection of a rule. Field set mirrors the prototype's
/// normalized policy (store.mjs `normalizePolicy`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub id: String,
    pub kind: String,
    /// Phases / tools this policy is selected for (exact match in SELECT).
    #[serde(default)]
    pub applies_to: Vec<String>,
    pub effect: Effect,
    #[serde(default)]
    pub trigger: Trigger,
    #[serde(default)]
    pub obligations: Vec<String>,
    /// The frozen acceptance-criteria text (becomes the claim's `criteria`).
    #[serde(default)]
    pub criteria: String,
    pub severity: Severity,
    /// Human-prose statement of the rule.
    #[serde(default)]
    pub rule: String,
}

impl ToNode for Policy {
    fn node_kind() -> &'static str {
        POLICY
    }

    fn to_node(&self) -> Node {
        let symbol = synthetic_symbol(POLICY, &self.id);
        let mut node = Node::new(
            symbol,
            NodeKind::Other(POLICY.to_string()),
            // The node `name` is the policy id — human-addressable, but NOT load-bearing for
            // reconstruction (every field is round-tripped through metadata below).
            self.id.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{POLICY}/{}", self.id), Span::ZERO),
        );
        // Encode the WHOLE policy as metadata so from_node is lossless. One key keeps the encoding
        // trivially total (no per-field plumbing) and matches the prototype's "the file IS the
        // policy" model. serde of `Policy` is infallible into a JSON object.
        let value = serde_json::to_value(self).expect("Policy serializes to JSON");
        if let serde_json::Value::Object(map) = value {
            node.metadata = map;
        }
        node
    }
}

impl FromNode for Policy {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == POLICY => {}
            other => anyhow::bail!("expected NodeKind::Other({POLICY:?}), got {other:?}"),
        }
        // The metadata bag IS the serialized Policy object.
        let value = serde_json::Value::Object(node.metadata.clone());
        let policy: Policy = serde_json::from_value(value)
            .map_err(|e| anyhow::anyhow!("node {} is not a valid Policy: {e}", node.name))?;
        Ok(policy)
    }
}
