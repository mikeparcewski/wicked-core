//! CAMPAIGN — a dependency-aware parallel scheduler over core Runs (DES-CAMPAIGN-001).
//!
//! A **Campaign** is a DAG of workflow **Runs**: nodes are Runs (each dispatched via the existing
//! [`crate::LaunchSpec`] / `launch_run` machinery), edges are dependencies. Independent nodes start
//! immediately; a dependent node dispatches the instant its deps reach their completion condition.
//! This is a LAYER over the existing Run + [`CoreEvent`] + single-writer actor primitives — NOT a
//! second runtime (REQ NFR1).
//!
//! This module is split cleanly (DES §4):
//!  * **Pure, deterministic scheduling** ([`ready_set`], [`blocked_by_failure`], [`satisfied`],
//!    [`validate`]) — no I/O, no clock, `BTreeSet`/`BTreeMap` working sets so the dispatch decision
//!    is 100-run identical (SC-C9) and unit-testable in isolation.
//!  * **A side-effecting driver** (in the `driver` section below) that runs INSIDE the actor's
//!    single-writer command handler, so "persist then launch" is one atomic write boundary.
//!
//! Campaign state persists like an [`crate::AgentSession`] — one estate node round-trip — so it is
//! durable and crash-resumable (DES §6): a fresh actor re-derives the ready set from the persisted
//! terminal statuses and never re-runs a completed node.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, FromNode, GraphRead, Language, Location, Node, NodeKind, Span, ToNode,
    SYMBOL_SCHEME,
};
use wicked_estate_core::SymbolQuery;

use crate::domain::HumanConfirm;
use crate::scope::EntityMode;
use crate::workflow::HumanDecision;
use crate::LaunchSpec;
use wicked_council::AgenticCli;

/// The estate node-kind token a persisted [`Campaign`] round-trips through (sibling of
/// `AGENT_SESSION` / `WORK_UNIT`). Local to this crate — a Campaign is a wicked-core concept.
pub(crate) const CAMPAIGN: &str = "campaign";

// ── data model (DES §2) ────────────────────────────────────────────────────────

/// What one campaign node runs — the reusable Run specification (mirrors [`LaunchSpec`] minus the
/// `session_id`, which `dispatch()` derives from `(campaign, node, attempt)`, §2.1). A node inherits
/// governance, worktree isolation, HITL gates, live output, and per-Run resume for free.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSpec {
    /// The free-text problem this node's Run decomposes into ordered units.
    pub problem: String,
    /// The council roster (`AgenticCli` seats) convened for this node's Run.
    pub clis: Vec<AgenticCli>,
    /// Shared vs isolated collection scope for the node's Run.
    pub entity_mode: EntityMode,
    /// The node's internal human-confirm gate policy. `#[serde(default)]` (→ `None`) for back-compat.
    #[serde(default)]
    pub human_confirm: HumanConfirm,
    /// The registered repo the node's Run targets, if any (creates an isolated worktree).
    #[serde(default)]
    pub repo_ref: Option<String>,
}

impl RunSpec {
    /// Build the [`LaunchSpec`] for this node's Run under the dispatch-derived `run_id` (§2.1).
    pub(crate) fn to_launch_spec(&self, run_id: String) -> LaunchSpec {
        LaunchSpec {
            problem: self.problem.clone(),
            clis: self.clis.clone(),
            entity_mode: self.entity_mode,
            session_id: run_id,
            human_confirm: self.human_confirm,
            repo_ref: self.repo_ref.clone(),
        }
    }
}

/// A schedulable unit of the DAG — one node = one core Run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignNode {
    /// Stable node id, unique within the campaign (must not contain `:` — it keys the run id).
    pub node_id: String,
    /// The Run this node dispatches.
    pub run_spec: RunSpec,
}

/// When a dependency edge is *satisfied* (DES §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeCondition {
    /// The dep must reach `Completed` (success only) — the default dependency.
    OnSuccess,
    /// The dep must reach any terminal outcome `{Completed, Failed, Cancelled}` (cleanup/report path).
    OnTerminal,
}

impl Default for EdgeCondition {
    fn default() -> Self {
        EdgeCondition::OnSuccess
    }
}

/// A dependency edge `from -> to`: `to` becomes eligible once `from` satisfies `condition`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignEdge {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub condition: EdgeCondition,
}

/// How a node failure propagates through the campaign (DES §5.2, REQ FR5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePolicy {
    /// Any node failure → cancel every non-terminal node (incl. `ReadyToResume`), campaign `Failed`.
    FailFast,
    /// (default) A failed node blocks only its transitive `OnSuccess`-dependents; independent
    /// branches run on. Ends `PartiallyCompleted` if any node Blocked/Failed, else `Completed`.
    ContinueIndependent,
    /// A node failure pauses the campaign at a per-node decision (`Retry | Skip | Abort`).
    HumanGateOnFailure,
}

impl Default for FailurePolicy {
    fn default() -> Self {
        FailurePolicy::ContinueIndependent
    }
}

/// The static definition of a campaign — validated + persisted verbatim inside the live [`Campaign`]
/// so a resume can reconstruct nodes/edges/policy/cap without a second store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignDef {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub nodes: Vec<CampaignNode>,
    #[serde(default)]
    pub edges: Vec<CampaignEdge>,
    #[serde(default)]
    pub policy: FailurePolicy,
    /// The global concurrency cap (>= 1) — a resource guard on parallel worktrees + CLI subprocesses.
    pub max_concurrency: usize,
}

/// Per-node lifecycle status (DES §2). `TERMINAL = {Completed, Failed, Blocked, Cancelled}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// Not yet eligible (a dep is unsatisfied).
    Pending,
    /// Every in-edge satisfied, never launched — `dispatch()` will `LaunchRun` a fresh attempt.
    Ready,
    /// Actively executing in core (consumes a concurrency slot).
    Running,
    /// A HITL gate is open inside the node's Run, waiting on the human — frees the slot (no slot).
    AwaitingHuman,
    /// Human approved; a live (paused) Run exists, queued for a slot before it resumes (no slot).
    ReadyToResume,
    // ── terminal ──
    Completed,
    Failed,
    Blocked,
    Cancelled,
}

impl NodeStatus {
    /// Whether this is an absorbing terminal state (the §4 terminal-skip guard tests this).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            NodeStatus::Completed
                | NodeStatus::Failed
                | NodeStatus::Blocked
                | NodeStatus::Cancelled
        )
    }
}

/// Campaign lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CampaignStatus {
    Running,
    Paused,
    Completed,
    PartiallyCompleted,
    Failed,
    Cancelled,
}

/// A per-Run terminal outcome — the reconciler maps core's `SessionCompleted` / `SessionFailed` /
/// `RunCancelled` events onto these node outcomes (the terminal signal the driver consumes, DES §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeOutcome {
    Completed,
    Failed,
    Cancelled,
}

impl NodeOutcome {
    pub(crate) fn as_node_status(self) -> NodeStatus {
        match self {
            NodeOutcome::Completed => NodeStatus::Completed,
            NodeOutcome::Failed => NodeStatus::Failed,
            NodeOutcome::Cancelled => NodeStatus::Cancelled,
        }
    }
}

/// The operator's resolution of a campaign gate (DES §4 step 4, §5.2). One command surface resolves
/// BOTH the per-node HITL gate (`Approve`/`Reject`, node `AwaitingHuman`) and the
/// `HumanGateOnFailure` policy gate (`Retry`/`Skip`/`Abort`, node `Failed` + queued).
#[derive(Debug, Clone)]
pub enum CampaignGateDecision {
    /// Per-node HITL gate: approve (optionally amending the unit instruction) → slot-gated resume.
    Approve { amend: Option<String> },
    /// Per-node HITL gate: reject → terminate the node's Run immediately.
    Reject,
    /// `HumanGateOnFailure`: re-run the failed node (bumps `node_attempt`).
    Retry,
    /// `HumanGateOnFailure`: treat the node as terminally failed (apply continue-independent blocking).
    Skip,
    /// `HumanGateOnFailure`: escalate to fail-fast (cancel everything).
    Abort,
}

/// The live campaign instance — persisted like an [`crate::AgentSession`] (one node round-trip), so
/// it is durable + crash-resumable. `node_run_id` is written ONLY by `dispatch()` (§2.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Campaign {
    pub id: String,
    pub def_id: String,
    pub status: CampaignStatus,
    /// The full definition (nodes/edges/policy/cap) — embedded so resume needs no second store.
    pub def: CampaignDef,
    pub node_status: BTreeMap<String, NodeStatus>,
    /// node_id -> live Run id. Written ONLY by `dispatch()`, always from the §2.1 attempt-keyed rule.
    #[serde(default)]
    pub node_run_id: BTreeMap<String, String>,
    /// node_id -> attempt counter (0-based); part of the run id. Retry bumps this.
    #[serde(default)]
    pub node_attempt: BTreeMap<String, u32>,
    /// An approved-gate decision awaiting a slot (per-node HITL gate, §6.5). Not serde — carries a
    /// runtime [`HumanDecision`]; persisted separately as `pending_decision_amend` below.
    #[serde(skip)]
    pub pending_decision: BTreeMap<String, HumanDecision>,
    /// Persisted shape of `pending_decision` (a `ReadyToResume` node approved mid-crash): the amend
    /// text (or empty). Rehydrated into `pending_decision` on load so resume re-issues the confirm.
    #[serde(default)]
    pub pending_decision_amend: BTreeMap<String, Option<String>>,
    /// The `HumanGateOnFailure` queue — failed nodes awaiting a decision, surfaced independently and
    /// ordered by node_id. A later failure on an already-paused campaign appends (never overwrites).
    #[serde(default)]
    pub pending_failure_gates: Vec<String>,
    /// Whether a fail-fast (or `Abort`) tripped — makes `finalize()` land on `Failed`.
    #[serde(default)]
    pub fail_fast_tripped: bool,
}

impl Campaign {
    /// A fresh campaign from a validated def — every node `Pending`, campaign `Running`.
    pub(crate) fn new(def: CampaignDef) -> Self {
        let node_status = def
            .nodes
            .iter()
            .map(|n| (n.node_id.clone(), NodeStatus::Pending))
            .collect();
        Campaign {
            id: def.id.clone(),
            def_id: def.id.clone(),
            status: CampaignStatus::Running,
            node_status,
            node_run_id: BTreeMap::new(),
            node_attempt: BTreeMap::new(),
            pending_decision: BTreeMap::new(),
            pending_decision_amend: BTreeMap::new(),
            pending_failure_gates: Vec::new(),
            fail_fast_tripped: false,
            def,
        }
    }

    /// The status of a node (defaults to `Pending` for an unknown id — defensive).
    pub fn status_of(&self, node_id: &str) -> NodeStatus {
        self.node_status
            .get(node_id)
            .copied()
            .unwrap_or(NodeStatus::Pending)
    }

    /// Count of nodes actively executing — ONLY `Running` (DES §2). `AwaitingHuman` and
    /// `ReadyToResume` are excluded: neither holds a slot, which is what makes FR6 true at
    /// `max_concurrency=1` while keeping the cap a true bound on concurrent execution.
    pub fn running_count(&self) -> usize {
        self.node_status
            .values()
            .filter(|s| **s == NodeStatus::Running)
            .count()
    }

    /// The attempt counter for a node (0-based).
    pub fn attempt_of(&self, node_id: &str) -> u32 {
        self.node_attempt.get(node_id).copied().unwrap_or(0)
    }

    /// The dispatch-derived run id for a node at its current attempt (§2.1): `"{c}:{n}:a{attempt}"`.
    pub fn derive_run_id(&self, node_id: &str) -> String {
        format!("{}:{}:a{}", self.id, node_id, self.attempt_of(node_id))
    }

    /// The dispatchable set (DES §4): fresh-start `ready_set` ∪ approved-waiting-for-slot
    /// (`ReadyToResume`), ordered by node_id (`BTreeSet`) so `try_fill` is deterministic.
    pub fn dispatchable(&self) -> BTreeSet<String> {
        let mut out = ready_set(&self.def.nodes, &self.def.edges, &self.node_status);
        for (id, st) in &self.node_status {
            if *st == NodeStatus::ReadyToResume {
                out.insert(id.clone());
            }
        }
        out
    }

    /// Rehydrate the runtime-only `pending_decision` map from its persisted amend shape (on load).
    pub(crate) fn rehydrate(&mut self) {
        self.pending_decision = self
            .pending_decision_amend
            .iter()
            .map(|(k, amend)| (k.clone(), HumanDecision::Approve { amend: amend.clone() }))
            .collect();
    }

    /// Mirror `pending_decision` into its persisted shape (before a store write).
    pub(crate) fn sync_pending(&mut self) {
        self.pending_decision_amend = self
            .pending_decision
            .iter()
            .map(|(k, d)| {
                let amend = match d {
                    HumanDecision::Approve { amend } => amend.clone(),
                    HumanDecision::Reject => None,
                };
                (k.clone(), amend)
            })
            .collect();
    }
}

// ── PURE scheduling (DES §4 — deterministic, no I/O, no clock; SC-C9) ────────────

/// Whether an edge is *satisfied* by its `from` node's current status (DES §5.1):
/// `OnSuccess` ⇔ dep `Completed`; `OnTerminal` ⇔ dep in `{Completed, Failed, Cancelled}`.
pub fn satisfied(status: &BTreeMap<String, NodeStatus>, edge: &CampaignEdge) -> bool {
    match status.get(&edge.from).copied() {
        Some(NodeStatus::Completed) => true,
        Some(NodeStatus::Failed) | Some(NodeStatus::Cancelled) => {
            matches!(edge.condition, EdgeCondition::OnTerminal)
        }
        // Pending / Ready / Running / AwaitingHuman / ReadyToResume / Blocked / missing → not yet.
        _ => false,
    }
}

/// The set of nodes eligible to LAUNCH from scratch: already `Ready`, or `Pending` with EVERY in-edge
/// satisfied. Pure + deterministic (`BTreeSet`, no `HashSet`; SC-C9). A node with no in-edges is
/// ready (in-degree 0 dispatches immediately). A diamond `A→{B,C}→D` marks `D` ready only once BOTH
/// `B` and `C` satisfy — handled by "all in-edges satisfied", no special-casing (SC-C1).
pub fn ready_set(
    nodes: &[CampaignNode],
    edges: &[CampaignEdge],
    status: &BTreeMap<String, NodeStatus>,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for n in nodes {
        match status.get(&n.node_id).copied().unwrap_or(NodeStatus::Pending) {
            NodeStatus::Ready => {
                out.insert(n.node_id.clone());
            }
            NodeStatus::Pending => {
                let all_satisfied = edges
                    .iter()
                    .filter(|e| e.to == n.node_id)
                    .all(|e| satisfied(status, e));
                if all_satisfied {
                    out.insert(n.node_id.clone());
                }
            }
            _ => {}
        }
    }
    out
}

/// The set of nodes transitively blocked by a failure (DES §5.1): a node is `Blocked` iff ANY of its
/// `OnSuccess` in-edges' dep is `Failed` / `Cancelled` / `Blocked`. A fixpoint over the (insertion-
/// ordered) edge list with a `BTreeSet` working set — deterministic (SC-C9). Only ever blocks a node
/// that hasn't started (`Pending`/`Ready`): a `Running`/`Completed` node's `OnSuccess` deps were all
/// `Completed`, so they can't later fail. `OnTerminal` dependents are NOT blocked (cleanup runs).
pub fn blocked_by_failure(
    nodes: &[CampaignNode],
    edges: &[CampaignEdge],
    status: &BTreeMap<String, NodeStatus>,
) -> BTreeSet<String> {
    // Seed with nodes already recorded `Blocked`, so their dependents propagate.
    let mut blocked: BTreeSet<String> = nodes
        .iter()
        .filter(|n| status.get(&n.node_id).copied() == Some(NodeStatus::Blocked))
        .map(|n| n.node_id.clone())
        .collect();

    loop {
        let mut added = false;
        for e in edges {
            if e.condition != EdgeCondition::OnSuccess {
                continue;
            }
            let dep_status = status.get(&e.from).copied().unwrap_or(NodeStatus::Pending);
            let dep_bad = matches!(dep_status, NodeStatus::Failed | NodeStatus::Cancelled)
                || blocked.contains(&e.from);
            if !dep_bad {
                continue;
            }
            let to_status = status.get(&e.to).copied().unwrap_or(NodeStatus::Pending);
            let blockable =
                matches!(to_status, NodeStatus::Pending | NodeStatus::Ready) && !blocked.contains(&e.to);
            if blockable {
                blocked.insert(e.to.clone());
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    blocked
}

/// Validate a campaign def at define time (DES §2.2). Rejects: an EMPTY campaign (0 nodes), a
/// `max_concurrency < 1`, a duplicate node id, a node/campaign id containing `:` (would collide the
/// derived run id), an edge to/from a nonexistent node, a self-edge, a DUPLICATE `(from,to)` pair
/// (ambiguous condition — reject, don't silently merge), and a CYCLE (topo-sort). A single-node,
/// no-edge campaign is valid (dispatches immediately).
pub fn validate(def: &CampaignDef) -> Result<(), String> {
    if def.nodes.is_empty() {
        return Err("campaign has no nodes (an empty campaign is rejected, not vacuously completed)".into());
    }
    if def.max_concurrency < 1 {
        return Err("max_concurrency must be >= 1".into());
    }
    if def.id.contains(':') {
        return Err(format!("campaign id must not contain ':': {}", def.id));
    }

    let mut ids: BTreeSet<&str> = BTreeSet::new();
    for n in &def.nodes {
        if n.node_id.contains(':') {
            return Err(format!("node id must not contain ':': {}", n.node_id));
        }
        if !ids.insert(n.node_id.as_str()) {
            return Err(format!("duplicate node id: {}", n.node_id));
        }
    }

    let mut pairs: BTreeSet<(&str, &str)> = BTreeSet::new();
    for e in &def.edges {
        if !ids.contains(e.from.as_str()) {
            return Err(format!("edge from nonexistent node: {}", e.from));
        }
        if !ids.contains(e.to.as_str()) {
            return Err(format!("edge to nonexistent node: {}", e.to));
        }
        if e.from == e.to {
            return Err(format!("self-edge on node: {}", e.from));
        }
        if !pairs.insert((e.from.as_str(), e.to.as_str())) {
            return Err(format!("duplicate edge: {} -> {}", e.from, e.to));
        }
    }

    detect_cycle(def)?;
    Ok(())
}

/// Kahn's topo-sort cycle detection. `BTreeMap` in-degree table + a sorted seed queue keep it
/// deterministic. If fewer than `|nodes|` are visited, a cycle exists.
fn detect_cycle(def: &CampaignDef) -> Result<(), String> {
    let mut indeg: BTreeMap<&str, usize> =
        def.nodes.iter().map(|n| (n.node_id.as_str(), 0usize)).collect();
    for e in &def.edges {
        if let Some(d) = indeg.get_mut(e.to.as_str()) {
            *d += 1;
        }
    }
    // Sorted seed (BTreeMap iteration) → deterministic visitation.
    let mut queue: Vec<&str> = indeg
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&k, _)| k)
        .collect();
    let mut visited = 0usize;
    while let Some(n) = queue.pop() {
        visited += 1;
        for e in def.edges.iter().filter(|e| e.from == n) {
            if let Some(d) = indeg.get_mut(e.to.as_str()) {
                *d -= 1;
                if *d == 0 {
                    queue.push(e.to.as_str());
                }
            }
        }
    }
    if visited != def.nodes.len() {
        return Err("campaign graph has a cycle".into());
    }
    Ok(())
}

// ── persistence (mirror AgentSession — one estate node round-trip) ───────────────

impl ToNode for Campaign {
    fn node_kind() -> &'static str {
        CAMPAIGN
    }

    fn to_node(&self) -> Node {
        let mut node = Node::new(
            synthetic_symbol(CAMPAIGN, &self.id),
            NodeKind::Other(CAMPAIGN.to_string()),
            self.id.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{CAMPAIGN}/{}", self.id), Span::ZERO),
        );
        if let serde_json::Value::Object(map) =
            serde_json::to_value(self).expect("Campaign serializes to JSON")
        {
            node.metadata = map;
        }
        node
    }
}

impl FromNode for Campaign {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == CAMPAIGN => {}
            other => anyhow::bail!("expected NodeKind::Other({CAMPAIGN:?}), got {other:?}"),
        }
        let mut c: Campaign = serde_json::from_value(serde_json::Value::Object(node.metadata.clone()))
            .map_err(|e| anyhow::anyhow!("node {} is not a valid Campaign: {e}", node.name))?;
        c.rehydrate();
        Ok(c)
    }
}

/// Read a [`Campaign`] back by id.
pub fn get_campaign(store: &dyn GraphRead, id: &str) -> anyhow::Result<Option<Campaign>> {
    match store.get_node(&synthetic_symbol(CAMPAIGN, id))? {
        Some(node) => Ok(Some(Campaign::from_node(&node)?)),
        None => Ok(None),
    }
}

/// Every campaign on the store (unordered).
pub fn all_campaigns(store: &dyn GraphRead) -> anyhow::Result<Vec<Campaign>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(CAMPAIGN.to_string())],
        ..Default::default()
    };
    Ok(store
        .find_symbols(&query)?
        .iter()
        .filter_map(|n| Campaign::from_node(n).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str) -> CampaignNode {
        CampaignNode {
            node_id: id.to_string(),
            run_spec: RunSpec {
                problem: format!("do {id}"),
                clis: vec![],
                entity_mode: EntityMode::Shared,
                human_confirm: HumanConfirm::None,
                repo_ref: None,
            },
        }
    }

    fn edge(from: &str, to: &str, condition: EdgeCondition) -> CampaignEdge {
        CampaignEdge {
            from: from.to_string(),
            to: to.to_string(),
            condition,
        }
    }

    fn status_map(pairs: &[(&str, NodeStatus)]) -> BTreeMap<String, NodeStatus> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn diamond() -> (Vec<CampaignNode>, Vec<CampaignEdge>) {
        let nodes = vec![node("A"), node("B"), node("C"), node("D")];
        let edges = vec![
            edge("A", "B", EdgeCondition::OnSuccess),
            edge("A", "C", EdgeCondition::OnSuccess),
            edge("B", "D", EdgeCondition::OnSuccess),
            edge("C", "D", EdgeCondition::OnSuccess),
        ];
        (nodes, edges)
    }

    // ── SC-C1 — diamond ready-set semantics: D waits for BOTH B and C ──
    #[test]
    fn ready_set_diamond_gates_join_on_all_in_edges() {
        let (nodes, edges) = diamond();

        // Only A (in-degree 0) is ready at the start.
        let s = status_map(&[
            ("A", NodeStatus::Pending),
            ("B", NodeStatus::Pending),
            ("C", NodeStatus::Pending),
            ("D", NodeStatus::Pending),
        ]);
        assert_eq!(
            ready_set(&nodes, &edges, &s),
            BTreeSet::from(["A".to_string()])
        );

        // A completed → B and C become ready; D still waits (neither dep done).
        let s = status_map(&[
            ("A", NodeStatus::Completed),
            ("B", NodeStatus::Pending),
            ("C", NodeStatus::Pending),
            ("D", NodeStatus::Pending),
        ]);
        assert_eq!(
            ready_set(&nodes, &edges, &s),
            BTreeSet::from(["B".to_string(), "C".to_string()])
        );

        // Only B completed → D still NOT ready (C outstanding).
        let s = status_map(&[
            ("A", NodeStatus::Completed),
            ("B", NodeStatus::Completed),
            ("C", NodeStatus::Running),
            ("D", NodeStatus::Pending),
        ]);
        assert_eq!(ready_set(&nodes, &edges, &s), BTreeSet::new());

        // Both B and C completed → D ready.
        let s = status_map(&[
            ("A", NodeStatus::Completed),
            ("B", NodeStatus::Completed),
            ("C", NodeStatus::Completed),
            ("D", NodeStatus::Pending),
        ]);
        assert_eq!(
            ready_set(&nodes, &edges, &s),
            BTreeSet::from(["D".to_string()])
        );
    }

    // ── SC-C9 — the ready-set decision is deterministic over 100 runs ──
    #[test]
    fn ready_set_is_deterministic_over_100_runs() {
        let (nodes, edges) = diamond();
        let s = status_map(&[
            ("A", NodeStatus::Completed),
            ("B", NodeStatus::Pending),
            ("C", NodeStatus::Pending),
            ("D", NodeStatus::Pending),
        ]);
        let first = ready_set(&nodes, &edges, &s);
        for _ in 0..100 {
            assert_eq!(ready_set(&nodes, &edges, &s), first);
        }
        // Ordered by node_id (BTreeSet) — the dispatch order is stable.
        let ordered: Vec<String> = first.into_iter().collect();
        assert_eq!(ordered, vec!["B".to_string(), "C".to_string()]);
    }

    // ── SC-C5 (logic) — blocked_by_failure transitively blocks OnSuccess dependents only ──
    #[test]
    fn blocked_by_failure_is_transitive_over_on_success_edges() {
        // X -> Y -> Z (OnSuccess), plus an independent W. X fails.
        let nodes = vec![node("X"), node("Y"), node("Z"), node("W")];
        let edges = vec![
            edge("X", "Y", EdgeCondition::OnSuccess),
            edge("Y", "Z", EdgeCondition::OnSuccess),
        ];
        let s = status_map(&[
            ("X", NodeStatus::Failed),
            ("Y", NodeStatus::Pending),
            ("Z", NodeStatus::Pending),
            ("W", NodeStatus::Pending),
        ]);
        assert_eq!(
            blocked_by_failure(&nodes, &edges, &s),
            BTreeSet::from(["Y".to_string(), "Z".to_string()]),
            "the failure of X transitively blocks Y then Z; independent W is untouched"
        );
    }

    #[test]
    fn on_terminal_dependents_are_not_blocked_by_a_failure() {
        // cleanup C runs on X's terminal outcome even when X fails.
        let nodes = vec![node("X"), node("C")];
        let edges = vec![edge("X", "C", EdgeCondition::OnTerminal)];
        let s = status_map(&[("X", NodeStatus::Failed), ("C", NodeStatus::Pending)]);
        assert!(
            blocked_by_failure(&nodes, &edges, &s).is_empty(),
            "an OnTerminal dependent is a cleanup path — never blocked"
        );
        // ...and it becomes ready (X's terminal outcome satisfies the OnTerminal edge).
        assert_eq!(
            ready_set(&nodes, &edges, &s),
            BTreeSet::from(["C".to_string()])
        );
    }

    // ── mixed-edge truth table (DES §5.1) ──
    #[test]
    fn mixed_edge_truth_table() {
        // N has OnSuccess(X) + OnTerminal(Y).
        let nodes = vec![node("X"), node("Y"), node("N")];
        let edges = vec![
            edge("X", "N", EdgeCondition::OnSuccess),
            edge("Y", "N", EdgeCondition::OnTerminal),
        ];

        // Row 1: X Completed, Y Failed → Ready (both satisfied).
        let s = status_map(&[
            ("X", NodeStatus::Completed),
            ("Y", NodeStatus::Failed),
            ("N", NodeStatus::Pending),
        ]);
        assert_eq!(ready_set(&nodes, &edges, &s), BTreeSet::from(["N".to_string()]));
        assert!(blocked_by_failure(&nodes, &edges, &s).is_empty());

        // Row 2: X Failed, Y Completed → Blocked (an OnSuccess dep failed), NOT ready.
        let s = status_map(&[
            ("X", NodeStatus::Failed),
            ("Y", NodeStatus::Completed),
            ("N", NodeStatus::Pending),
        ]);
        assert_eq!(ready_set(&nodes, &edges, &s), BTreeSet::new());
        assert_eq!(
            blocked_by_failure(&nodes, &edges, &s),
            BTreeSet::from(["N".to_string()])
        );

        // Row 3: all OnTerminal, dep Failed → Ready (cleanup/report path).
        let edges2 = vec![edge("X", "N", EdgeCondition::OnTerminal)];
        let nodes2 = vec![node("X"), node("N")];
        let s = status_map(&[("X", NodeStatus::Failed), ("N", NodeStatus::Pending)]);
        assert_eq!(ready_set(&nodes2, &edges2, &s), BTreeSet::from(["N".to_string()]));
    }

    // ── SC-C7 — cycle rejected; + empty / duplicate-edge / dangling-edge validation ──
    #[test]
    fn validate_rejects_cycle_empty_and_duplicate_edges() {
        // Valid: single node, no edges.
        let ok = CampaignDef {
            id: "c".into(),
            name: "".into(),
            nodes: vec![node("A")],
            edges: vec![],
            policy: FailurePolicy::ContinueIndependent,
            max_concurrency: 1,
        };
        assert!(validate(&ok).is_ok());

        // Cycle A -> B -> A.
        let cyclic = CampaignDef {
            edges: vec![
                edge("A", "B", EdgeCondition::OnSuccess),
                edge("B", "A", EdgeCondition::OnSuccess),
            ],
            nodes: vec![node("A"), node("B")],
            ..ok.clone()
        };
        assert!(validate(&cyclic).unwrap_err().contains("cycle"));

        // Empty campaign.
        let empty = CampaignDef {
            nodes: vec![],
            edges: vec![],
            ..ok.clone()
        };
        assert!(validate(&empty).is_err());

        // Duplicate (from,to) edge.
        let dup = CampaignDef {
            nodes: vec![node("A"), node("B")],
            edges: vec![
                edge("A", "B", EdgeCondition::OnSuccess),
                edge("A", "B", EdgeCondition::OnTerminal),
            ],
            ..ok.clone()
        };
        assert!(validate(&dup).unwrap_err().contains("duplicate edge"));

        // Edge to a nonexistent node.
        let dangling = CampaignDef {
            nodes: vec![node("A")],
            edges: vec![edge("A", "ghost", EdgeCondition::OnSuccess)],
            ..ok.clone()
        };
        assert!(validate(&dangling).unwrap_err().contains("nonexistent"));

        // Duplicate node id.
        let dupnode = CampaignDef {
            nodes: vec![node("A"), node("A")],
            edges: vec![],
            ..ok.clone()
        };
        assert!(validate(&dupnode).unwrap_err().contains("duplicate node"));
    }

    #[test]
    fn campaign_round_trips_through_a_node() {
        let def = CampaignDef {
            id: "camp1".into(),
            name: "demo".into(),
            nodes: vec![node("A"), node("B")],
            edges: vec![edge("A", "B", EdgeCondition::OnSuccess)],
            policy: FailurePolicy::ContinueIndependent,
            max_concurrency: 2,
        };
        let mut c = Campaign::new(def);
        c.node_status.insert("A".into(), NodeStatus::Completed);
        c.node_run_id.insert("A".into(), "camp1:A:a0".into());
        c.pending_decision
            .insert("B".into(), HumanDecision::Approve { amend: Some("go".into()) });
        c.sync_pending();

        let back = Campaign::from_node(&c.to_node()).expect("from_node");
        assert_eq!(back.id, "camp1");
        assert_eq!(back.status_of("A"), NodeStatus::Completed);
        assert_eq!(back.node_run_id.get("A").unwrap(), "camp1:A:a0");
        // pending_decision rehydrated from its persisted amend shape.
        assert!(matches!(
            back.pending_decision.get("B"),
            Some(HumanDecision::Approve { amend }) if amend.as_deref() == Some("go")
        ));
        assert_eq!(back.def.nodes.len(), 2);
        assert_eq!(back.def.max_concurrency, 2);
    }

    #[test]
    fn running_count_counts_only_running_nodes() {
        let def = CampaignDef {
            id: "c".into(),
            name: "".into(),
            nodes: vec![node("A"), node("B"), node("C"), node("D")],
            edges: vec![],
            policy: FailurePolicy::default(),
            max_concurrency: 4,
        };
        let mut c = Campaign::new(def);
        c.node_status.insert("A".into(), NodeStatus::Running);
        c.node_status.insert("B".into(), NodeStatus::AwaitingHuman);
        c.node_status.insert("C".into(), NodeStatus::ReadyToResume);
        c.node_status.insert("D".into(), NodeStatus::Running);
        assert_eq!(
            c.running_count(),
            2,
            "only Running consumes a slot; AwaitingHuman + ReadyToResume do not"
        );
    }
}
