//! Domain types for orchestration — `Workflow` and `Phase` — and their lossless projection onto
//! the shared estate [`Node`] API via wicked-apps-core's [`ToNode`]/[`FromNode`].
//!
//! Ported from the Node prototype (`wicked-orchestration/lib/store.mjs`), but the projection
//! target is the SHARED estate graph store, not JSON files: a `Workflow` is a
//! `Node(kind=Other("workflow"))` and a `Phase` is a `Node(kind=Other("phase"))`, both keyed by an
//! wicked-apps-core synthetic [`Symbol`](wicked_apps_core::Symbol). Every round-trippable field that is NOT
//! recoverable from `Node.name` is stored explicitly in `Node.metadata` (the estate contract:
//! `to_node` MUST be lossless w.r.t. `from_node`).

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, Decision, FromNode, Language, Location, Node, NodeKind, Span, ToNode, PHASE,
    SYMBOL_SCHEME, WORKFLOW,
};

/// Lifecycle status of a `Workflow` — updated by the runner as phases advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    #[default]
    Running,
    /// Current phase is `AwaitingDeliverables` — a human must unblock it.
    AwaitingHuman,
    Complete,
    Failed,
}

/// The phase state machine's states (ARCHITECTURE §4 / `reducer.mjs`). Serialized to/from
/// `Node.metadata` and the bus as the prototype's snake_case strings (`pending`, `in_progress`, …)
/// so the projection stays wire-compatible with the Node-era contract and the `ALLOWED_TRANSITIONS`
/// table keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseStatus {
    Pending,
    InProgress,
    AwaitingDeliverables,
    ReadyForGate,
    GateRunning,
    Approved,
    ApprovedWithConditions,
    Rejected,
    Skipped,
}

impl PhaseStatus {
    /// The exact snake_case token persisted in metadata / used as a state-machine key. Kept in
    /// lock-step with the `serde(rename_all = "snake_case")` derive so the table and the wire agree.
    pub fn as_token(self) -> &'static str {
        match self {
            PhaseStatus::Pending => "pending",
            PhaseStatus::InProgress => "in_progress",
            PhaseStatus::AwaitingDeliverables => "awaiting_deliverables",
            PhaseStatus::ReadyForGate => "ready_for_gate",
            PhaseStatus::GateRunning => "gate_running",
            PhaseStatus::Approved => "approved",
            PhaseStatus::ApprovedWithConditions => "approved_with_conditions",
            PhaseStatus::Rejected => "rejected",
            PhaseStatus::Skipped => "skipped",
        }
    }

    /// Is this an approving terminal status? A persisted veto (`gate_decision = Deny`) forbids any
    /// transition INTO one of these — the structural enforcement of ADR-0003 (`reject ⇒ ¬approved`).
    pub fn is_approving(self) -> bool {
        matches!(
            self,
            PhaseStatus::Approved | PhaseStatus::ApprovedWithConditions
        )
    }
}

/// An orchestration workflow — an ordered sequence of phases on the shared estate store.
///
/// Persisted as `Node(kind=Other("workflow"))`; `id` is the load-bearing identity. The ordered
/// `phases` list (phase id + display name pairs) and the `current_index` cursor drive the
/// runner's advance logic. Backward-compat: nodes without the new fields default to empty/0/Running
/// on read so older persisted workflows still deserialize cleanly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    pub id: String,
    /// Human-readable label (defaults to `id` when constructed via `new`).
    pub name: String,
    /// Ordered `(phase_id, phase_name)` pairs. The runner opens each in sequence.
    pub phases: Vec<(String, String)>,
    /// Index into `phases` of the phase currently being executed.
    pub current_index: usize,
    pub status: WorkflowStatus,
}

impl Workflow {
    /// Minimal constructor — no phases, used by tests and callers that populate phases separately.
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
            phases: Vec::new(),
            current_index: 0,
            status: WorkflowStatus::Running,
        }
    }

    /// The id of the phase currently being driven, or `None` if there are no phases.
    pub fn current_phase_id(&self) -> Option<&str> {
        self.phases
            .get(self.current_index)
            .map(|(id, _)| id.as_str())
    }
}

impl ToNode for Workflow {
    fn node_kind() -> &'static str {
        WORKFLOW
    }

    fn to_node(&self) -> Node {
        let symbol = synthetic_symbol(WORKFLOW, &self.id);
        let mut node = Node::new(
            symbol,
            NodeKind::Other(WORKFLOW.to_string()),
            self.name.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{WORKFLOW}/{}", self.id), Span::ZERO),
        );
        let m = &mut node.metadata;
        m.insert("id".to_string(), serde_json::Value::String(self.id.clone()));
        m.insert(
            "name".to_string(),
            serde_json::Value::String(self.name.clone()),
        );
        m.insert(
            "phases".to_string(),
            serde_json::to_value(&self.phases).expect("Vec<(String,String)> serializes"),
        );
        m.insert(
            "current_index".to_string(),
            serde_json::Value::Number((self.current_index as u64).into()),
        );
        m.insert(
            "status".to_string(),
            serde_json::to_value(self.status).expect("WorkflowStatus serializes"),
        );
        node
    }
}

impl FromNode for Workflow {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == WORKFLOW => {}
            other => anyhow::bail!("expected NodeKind::Other({WORKFLOW:?}), got {other:?}"),
        }
        let m = &node.metadata;
        let id = m
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Workflow node missing string metadata key `id`"))?
            .to_string();
        let name = m
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&id)
            .to_string();
        let phases: Vec<(String, String)> = match m.get("phases") {
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| anyhow::anyhow!("Workflow node `phases` invalid: {e}"))?,
            None => Vec::new(),
        };
        let current_index = m.get("current_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let status: WorkflowStatus = match m.get("status") {
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| anyhow::anyhow!("Workflow node `status` invalid: {e}"))?,
            None => WorkflowStatus::Running,
        };
        Ok(Workflow {
            id,
            name,
            phases,
            current_index,
            status,
        })
    }
}

/// A workflow phase — the unit the state machine and the gate operate on.
///
/// Persisted as `Node(kind=Other("phase"))`. The full set of round-trippable fields lives in
/// `Node.metadata`: `id`, `workflow_id`, `status`, `obligations`, and the optional `gate_decision`
/// (the HARD governance marker the reducer vetoes on, ADR-0003). `name` is the human label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase {
    pub id: String,
    pub workflow_id: String,
    pub name: String,
    pub status: PhaseStatus,
    /// Obligations carried from an `allow_with_conditions` gate (ADR-0003) — enforced downstream.
    pub obligations: Vec<String>,
    /// The governing governance verdict consumed by the gate. Once `Some(Decision::Deny)`, the
    /// reducer structurally refuses every approving transition (`vetoed_by_governance`).
    pub gate_decision: Option<Decision>,
}

impl Phase {
    /// Open a phase at the initial `Pending` status with no obligations and no gate decision —
    /// the state-machine entry state (mirrors `openPhase` in the prototype).
    pub fn open(
        id: impl Into<String>,
        workflow_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            workflow_id: workflow_id.into(),
            name: name.into(),
            status: PhaseStatus::Pending,
            obligations: Vec::new(),
            gate_decision: None,
        }
    }
}

impl ToNode for Phase {
    fn node_kind() -> &'static str {
        PHASE
    }

    fn to_node(&self) -> Node {
        let symbol = synthetic_symbol(PHASE, &self.id);
        let mut node = Node::new(
            symbol,
            NodeKind::Other(PHASE.to_string()),
            self.name.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{PHASE}/{}", self.id), Span::ZERO),
        );
        let m = &mut node.metadata;
        m.insert("id".to_string(), serde_json::Value::String(self.id.clone()));
        m.insert(
            "workflow_id".to_string(),
            serde_json::Value::String(self.workflow_id.clone()),
        );
        // `status` and `gate_decision` serialize as their snake_case tokens via serde; reuse that so
        // the metadata encoding and the wire format never drift from the enum definitions.
        m.insert(
            "status".to_string(),
            serde_json::to_value(self.status).expect("PhaseStatus serializes"),
        );
        m.insert(
            "obligations".to_string(),
            serde_json::to_value(&self.obligations).expect("Vec<String> serializes"),
        );
        m.insert(
            "gate_decision".to_string(),
            serde_json::to_value(&self.gate_decision).expect("Option<Decision> serializes"),
        );
        node
    }
}

impl FromNode for Phase {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == PHASE => {}
            other => anyhow::bail!("expected NodeKind::Other({PHASE:?}), got {other:?}"),
        }
        let m = &node.metadata;
        let str_field = |key: &str| -> anyhow::Result<String> {
            m.get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow::anyhow!("Phase node missing string metadata key `{key}`"))
        };
        let id = str_field("id")?;
        let workflow_id = str_field("workflow_id")?;
        let status: PhaseStatus = serde_json::from_value(
            m.get("status")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Phase node missing metadata key `status`"))?,
        )
        .map_err(|e| anyhow::anyhow!("Phase node `status` not a valid PhaseStatus: {e}"))?;
        // `obligations` and `gate_decision` default to empty/None when absent so older encodings
        // still read back, but a present value must deserialize cleanly.
        let obligations: Vec<String> = match m.get("obligations") {
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| anyhow::anyhow!("Phase node `obligations` invalid: {e}"))?,
            None => Vec::new(),
        };
        let gate_decision: Option<Decision> = match m.get("gate_decision") {
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| anyhow::anyhow!("Phase node `gate_decision` invalid: {e}"))?,
            None => None,
        };
        Ok(Phase {
            id,
            workflow_id,
            name: node.name.clone(),
            status,
            obligations,
            gate_decision,
        })
    }
}
