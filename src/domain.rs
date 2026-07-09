//! The session/unit domain — ported into COE from the retired wicked-agent. These are the entities
//! the pipeline plans, distributes, executes, and the UI reads. Each round-trips losslessly through
//! one estate `Node.metadata` object (serde), so adding a field needs no per-field plumbing.

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, FromNode, GraphRead, GraphWrite, Language, Location, Node, NodeKind, Span,
    SqliteStore, ToNode, AGENT_SESSION, SYMBOL_SCHEME, WORK_UNIT,
};
use wicked_estate_core::SymbolQuery;

use crate::scope::EntityMode;

/// Lifecycle status of an [`AgentSession`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Planning,
    Distributing,
    Executing,
    /// Paused BEFORE a not-yet-done unit, awaiting a human to resume.
    AwaitingHuman,
    /// Terminal: every unit was governance-approved and ran without worker failure.
    Completed,
    /// Terminated by the operator (or a rejected gate) before completing. Terminal.
    Cancelled,
    /// Terminal: stopped because a unit was governance-DENIED or its worker reported failure. The
    /// RUN-LEVEL DENY CONTRACT (decided in P2): a `Completed` run means EVERY unit was approved; a
    /// governance `Deny` (or a `StepStatus::Failed` worker) halts the run here, never silently
    /// completing past a rejection.
    Failed,
}

/// The human-confirm gate policy for a run — whether to pause BEFORE executing a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HumanConfirm {
    /// Never pause (default).
    #[default]
    None,
    /// Pause before EVERY not-yet-done unit.
    All,
    /// Pause before the unit whose `ord` equals the value.
    Before(u32),
}

/// A session — the owned interactive flow, persisted as `Node(Other(AGENT_SESSION))`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    /// Stable session id (the node identity).
    pub id: String,
    /// The orchestration workflow id backing this session.
    pub workflow_id: String,
    /// The free-text problem this session decomposes.
    pub problem: String,
    /// Shared (one scope for all units) vs isolated (per-unit scope) — §6 toggle.
    pub entity_mode: EntityMode,
    /// The collection scope under shared mode (`None` under isolated).
    pub collection_scope: Option<String>,
    /// The CLI seats convened for this session (council options).
    pub clis: Vec<String>,
    /// Lifecycle status.
    pub status: SessionStatus,
    /// The human-confirm gate policy. `#[serde(default)]` so older sessions still deserialize.
    #[serde(default)]
    pub human_confirm: HumanConfirm,
    /// Resume cursor: the index of the NEXT unit to execute (0-based into the ordered units). The
    /// interactive engine advances this as each unit's outcome is applied; `ResumeRun` re-enters
    /// here. `#[serde(default)]` so older sessions deserialize at 0.
    #[serde(default)]
    pub unit_ix: usize,
    /// Retry attempt for the unit at `unit_ix` — folded into event ids so a retried step is not
    /// deduped as a no-op (P2). `#[serde(default)]` for back-compat.
    #[serde(default)]
    pub attempt: u32,
    /// The git worktree this run executes in (set when the run targets a registered repo, P3).
    /// `None` for a repo-less run. `#[serde(default)]` for back-compat.
    #[serde(default)]
    pub workdir: Option<String>,
    /// The registered repo this run targets, if any (P3).
    #[serde(default)]
    pub repo_ref: Option<String>,
}

impl ToNode for AgentSession {
    fn node_kind() -> &'static str {
        AGENT_SESSION
    }

    fn to_node(&self) -> Node {
        let mut node = Node::new(
            synthetic_symbol(AGENT_SESSION, &self.id),
            NodeKind::Other(AGENT_SESSION.to_string()),
            self.id.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{AGENT_SESSION}/{}", self.id), Span::ZERO),
        );
        if let serde_json::Value::Object(map) =
            serde_json::to_value(self).expect("AgentSession serializes to JSON")
        {
            node.metadata = map;
        }
        node
    }
}

impl FromNode for AgentSession {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == AGENT_SESSION => {}
            other => anyhow::bail!("expected NodeKind::Other({AGENT_SESSION:?}), got {other:?}"),
        }
        serde_json::from_value(serde_json::Value::Object(node.metadata.clone()))
            .map_err(|e| anyhow::anyhow!("node {} is not a valid AgentSession: {e}", node.name))
    }
}

/// A unit of distributed work, persisted as `Node(Other(WORK_UNIT))`. Plan creates it `Pending`;
/// distribute records the assignment; execute records the outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkUnit {
    /// Stable unit id (the node identity), e.g. `<session>:u1`.
    pub id: String,
    /// The owning session id.
    pub session_id: String,
    /// 1-based order in the plan.
    pub ord: u32,
    /// The unit's description (becomes the gate's governance `work` context).
    pub description: String,
    /// The methodology stage this unit belongs to (recon/build/review/test), classified at plan time.
    /// `#[serde(default)]` (→ `Build`) so older units deserialize.
    #[serde(default)]
    pub stage: StageKind,
    /// The CLI the council assigned (set in distribute).
    #[serde(default)]
    pub assigned_cli: Option<String>,
    /// The assigned CLI's invocation template (from the launch roster) — lets the runner execute an
    /// AD-HOC CLI not in the council registry. `None` ⇒ the runner resolves the key via the registry.
    #[serde(default)]
    pub assigned_invocation: Option<String>,
    /// The council task id whose verdict produced the assignment (provenance).
    #[serde(default)]
    pub council_task_ref: Option<String>,
    /// WHY the assigned CLI won — the council verdict/ranking made visible (set in distribute). `None`
    /// for units distributed before this field existed. `#[serde(default)]` for back-compat.
    #[serde(default)]
    pub routing: Option<RoutingInfo>,
    /// WHY the unit was rejected — a governance deny (which policies) or a worker failure. Set only
    /// when the run halts on this unit. `#[serde(default)]` for back-compat.
    #[serde(default)]
    pub denial_reason: Option<String>,
    /// The orchestration phase id backing this unit (set in execute).
    #[serde(default)]
    pub phase_ref: Option<String>,
    /// The ConformanceClaim id the gate consumed (set in execute).
    #[serde(default)]
    pub conformance_ref: Option<String>,
    /// The phase status token the gate resolved to, e.g. `approved` / `rejected`.
    #[serde(default)]
    pub phase_status: Option<String>,
    /// The collection scope this unit's output is written to.
    #[serde(default)]
    pub collection_scope: Option<String>,
    /// The skill that drives this unit's work (DES-EXEC-001 §4.1) — carried from the backing phase's
    /// `skill_ref` at plan time (def-driven runs). `None` ⇒ the authored-prompt path. `#[serde(default)]`
    /// for back-compat with units persisted before the skills seam.
    #[serde(default)]
    pub skill_ref: Option<String>,
    /// The runtime skill ALLOWLIST for this unit's agent (DES-EXEC-001 §4.2) — carried from the phase's
    /// `allowed_skills`. The runner passes it as the invocation's skill/tool scope. Empty ⇒ unscoped.
    #[serde(default)]
    pub allowed_skills: Vec<String>,
    /// The backing phase's declared human-confirm gate (DES-EXEC-001 §3) — carried from the phase's
    /// `GateSpec` so the def, not just the run-level `--confirm` flag, drives when a run pauses for a
    /// human. A phase's gate fires AFTER its work (before the next unit). `Auto` (the default) ⇒ defer
    /// to the run-level policy. `#[serde(default)]` for back-compat with pre-gate-wiring units.
    #[serde(default)]
    pub gate: crate::workflow::GateSpec,
    /// The APPROVED, pinned deterministic validator for this unit's phase (rev0.4 gate layer-1). When
    /// present, the gate RE-VERIFIES it against the worktree after the governance pass — a fail denies
    /// the unit (deny-dominates). Authored + approved out of band; `None` ⇒ no validator (the pre-gate
    /// behavior). `#[serde(default)]` for back-compat.
    #[serde(default)]
    pub validator: Option<crate::validator::DeterministicValidator>,
    /// The final unit status: `pending` → `distributed` → `done` | `rejected`.
    pub status: UnitStatus,
}

/// Lifecycle status of a [`WorkUnit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitStatus {
    Pending,
    Distributed,
    Done,
    Rejected,
}

/// The methodology stage a unit belongs to (the recon → build → adversarial-review → functional-test
/// spine). Classified from the unit's description in [`crate::plan`]; surfaced as a per-unit badge so
/// the methodology is legible (you can tell a Recon unit from a Build unit from a Review unit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    /// Decompose / explore / map the problem before building.
    Recon,
    /// The main implementation work (the default).
    #[default]
    Build,
    /// Adversarial review — a distinct critic checks the build (evaluator ≠ creator).
    Review,
    /// Functional testing — verify the build actually works.
    Test,
}

impl StageKind {
    /// Classify a unit's stage from its description (a v1 keyword heuristic — the spine made visible
    /// without changing how units are planned).
    pub fn classify(description: &str) -> StageKind {
        let d = description.to_lowercase();
        let has = |words: &[&str]| words.iter().any(|w| d.contains(w));
        if has(&["test", "verify", "validate", "functional", "qa "]) {
            StageKind::Test
        } else if has(&[
            "review",
            "audit",
            "adversarial",
            "critique",
            "evaluate",
            "inspect",
        ]) {
            StageKind::Review
        } else if has(&[
            "recon",
            "research",
            "explore",
            "investigate",
            "decompose",
            "map the",
            "scope ",
        ]) {
            StageKind::Recon
        } else {
            StageKind::Build
        }
    }

    /// Short label for the UI badge.
    pub fn label(self) -> &'static str {
        match self {
            StageKind::Recon => "recon",
            StageKind::Build => "build",
            StageKind::Review => "review",
            StageKind::Test => "test",
        }
    }
}

/// WHY a particular CLI was assigned to a unit — the council's decision made visible. The verdict is
/// otherwise computed in [`crate::distribute`] and thrown away; capturing it here is what lets the UI
/// answer "why *this* CLI". Percentages are `0..=100` (not `f32`) so `WorkUnit` stays `Eq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum RoutingInfo {
    /// The council convened and its verdict named the winning seat.
    Council {
        winner: String,
        /// Council agreement ratio, `0..=100`.
        agreement_pct: u8,
        /// How many seats returned a vote.
        returned: u32,
        /// How many dissenting voices the verdict recorded.
        dissent: u32,
    },
    /// No usable verdict (no quorum, or the winner named no roster seat) — degraded to the first seat.
    Degraded { reason: String },
    /// A review/test unit was REASSIGNED off the council's pick to enforce evaluator ≠ creator (the
    /// critic must differ from the CLI that produced the work it checks). `was` is the council's pick.
    EvaluatorDistinct { winner: String, was: String },
}

impl WorkUnit {
    /// Build a fresh `Pending` unit for the plan.
    pub fn pending(
        id: impl Into<String>,
        session_id: impl Into<String>,
        ord: u32,
        description: impl Into<String>,
    ) -> Self {
        let description = description.into();
        WorkUnit {
            id: id.into(),
            session_id: session_id.into(),
            ord,
            stage: StageKind::classify(&description),
            description,
            assigned_cli: None,
            assigned_invocation: None,
            council_task_ref: None,
            routing: None,
            denial_reason: None,
            phase_ref: None,
            conformance_ref: None,
            phase_status: None,
            collection_scope: None,
            skill_ref: None,
            allowed_skills: Vec::new(),
            gate: crate::workflow::GateSpec::default(),
            validator: None,
            status: UnitStatus::Pending,
        }
    }
}

impl ToNode for WorkUnit {
    fn node_kind() -> &'static str {
        WORK_UNIT
    }

    fn to_node(&self) -> Node {
        let mut node = Node::new(
            synthetic_symbol(WORK_UNIT, &self.id),
            NodeKind::Other(WORK_UNIT.to_string()),
            self.id.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{WORK_UNIT}/{}", self.id), Span::ZERO),
        );
        if let serde_json::Value::Object(map) =
            serde_json::to_value(self).expect("WorkUnit serializes to JSON")
        {
            node.metadata = map;
        }
        node
    }
}

impl FromNode for WorkUnit {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == WORK_UNIT => {}
            other => anyhow::bail!("expected NodeKind::Other({WORK_UNIT:?}), got {other:?}"),
        }
        serde_json::from_value(serde_json::Value::Object(node.metadata.clone()))
            .map_err(|e| anyhow::anyhow!("node {} is not a valid WorkUnit: {e}", node.name))
    }
}

// ── Store primitives (the single shared-store read/write the pipeline uses) ──────

/// Upsert a node onto the store via the batch write path. Called only from the actor thread (the
/// single writer for `shared_writers=false` backends).
pub fn put_node(store: &mut SqliteStore, node: Node) -> anyhow::Result<()> {
    store.begin_batch()?;
    store.upsert_nodes(&[node])?;
    store.commit_batch()?;
    Ok(())
}

/// Read an [`AgentSession`] back by id.
pub fn get_session(
    store: &dyn GraphRead,
    session_id: &str,
) -> anyhow::Result<Option<AgentSession>> {
    match store.get_node(&synthetic_symbol(AGENT_SESSION, session_id))? {
        Some(node) => Ok(Some(AgentSession::from_node(&node)?)),
        None => Ok(None),
    }
}

/// Read every [`WorkUnit`] belonging to `session_id`, ordered by `ord`.
pub fn session_units(store: &dyn GraphRead, session_id: &str) -> anyhow::Result<Vec<WorkUnit>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(WORK_UNIT.to_string())],
        ..Default::default()
    };
    let mut units: Vec<WorkUnit> = store
        .find_symbols(&query)?
        .iter()
        .filter_map(|n| WorkUnit::from_node(n).ok())
        .filter(|u| u.session_id == session_id)
        .collect();
    units.sort_by_key(|u| u.ord);
    Ok(units)
}

/// Every session on the store (unordered).
pub fn all_sessions(store: &dyn GraphRead) -> anyhow::Result<Vec<AgentSession>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(AGENT_SESSION.to_string())],
        ..Default::default()
    };
    Ok(store
        .find_symbols(&query)?
        .iter()
        .filter_map(|n| AgentSession::from_node(n).ok())
        .collect())
}

/// A unit's captured work output (the transcript the UI shows), if the unit ran + was approved.
pub fn get_work_output(store: &dyn GraphRead, unit_id: &str) -> Option<String> {
    let node = store
        .get_node(&synthetic_symbol(crate::execute::WORK_OUTPUT, unit_id))
        .ok()??;
    node.metadata
        .get("output")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// A session plus its ordered units — the read the UI builds its project list from.
#[derive(Debug, Clone)]
pub struct SessionView {
    pub session: AgentSession,
    pub units: Vec<WorkUnit>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_session() -> AgentSession {
        AgentSession {
            id: "s-demo".to_string(),
            workflow_id: "wf-s-demo".to_string(),
            problem: "Build a thing".to_string(),
            entity_mode: EntityMode::Shared,
            collection_scope: Some("wicked-agent/s-demo/shared".to_string()),
            clis: vec!["claude".to_string(), "agy".to_string()],
            status: SessionStatus::Planning,
            human_confirm: HumanConfirm::Before(2),
            unit_ix: 0,
            attempt: 0,
            workdir: None,
            repo_ref: None,
        }
    }

    #[test]
    fn session_round_trips_through_node() {
        let s = sample_session();
        let back = AgentSession::from_node(&s.to_node()).expect("from_node");
        assert_eq!(
            s, back,
            "AgentSession must survive a node round-trip losslessly"
        );
    }

    #[test]
    fn unit_round_trips_through_node() {
        let mut u = WorkUnit::pending("s-demo:u1", "s-demo", 1, "Do step one");
        u.assigned_cli = Some("claude".to_string());
        u.status = UnitStatus::Distributed;
        let back = WorkUnit::from_node(&u.to_node()).expect("from_node");
        assert_eq!(
            u, back,
            "WorkUnit must survive a node round-trip losslessly"
        );
    }

    #[test]
    fn units_are_stage_classified_from_their_description() {
        assert_eq!(StageKind::classify("Add JWT auth"), StageKind::Build);
        assert_eq!(StageKind::classify("Then review it"), StageKind::Review);
        assert_eq!(
            StageKind::classify("Write functional tests"),
            StageKind::Test
        );
        assert_eq!(
            StageKind::classify("Research the codebase"),
            StageKind::Recon
        );
        // The classification rides through `pending` + the node round-trip.
        let u = WorkUnit::pending("s:u1", "s", 1, "Adversarial review of the change");
        assert_eq!(u.stage, StageKind::Review);
        assert_eq!(
            WorkUnit::from_node(&u.to_node()).unwrap().stage,
            StageKind::Review
        );
    }

    #[test]
    fn session_and_units_persist_and_read_back_from_the_store() {
        use wicked_apps_core::open_store;
        let dir = std::env::temp_dir().join("wicked-core-domain-test");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("domain.db");
        let _ = std::fs::remove_file(&db);
        let mut store = open_store(Some(db.to_str().unwrap())).expect("open_store");

        let s = sample_session();
        put_node(&mut store, s.to_node()).expect("put session");
        put_node(
            &mut store,
            WorkUnit::pending("s-demo:u1", "s-demo", 1, "step one").to_node(),
        )
        .expect("put unit 1");
        put_node(
            &mut store,
            WorkUnit::pending("s-demo:u2", "s-demo", 2, "step two").to_node(),
        )
        .expect("put unit 2");

        let read = get_session(&store, "s-demo")
            .expect("get_session")
            .expect("present");
        assert_eq!(read, s);
        let units = session_units(&store, "s-demo").expect("session_units");
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].ord, 1);
        assert_eq!(units[1].description, "step two");
    }
}
