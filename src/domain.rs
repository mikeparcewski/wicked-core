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
    Completed,
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
    /// The CLI the council assigned (set in distribute).
    #[serde(default)]
    pub assigned_cli: Option<String>,
    /// The council task id whose verdict produced the assignment (provenance).
    #[serde(default)]
    pub council_task_ref: Option<String>,
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

impl WorkUnit {
    /// Build a fresh `Pending` unit for the plan.
    pub fn pending(
        id: impl Into<String>,
        session_id: impl Into<String>,
        ord: u32,
        description: impl Into<String>,
    ) -> Self {
        WorkUnit {
            id: id.into(),
            session_id: session_id.into(),
            ord,
            description: description.into(),
            assigned_cli: None,
            council_task_ref: None,
            phase_ref: None,
            conformance_ref: None,
            phase_status: None,
            collection_scope: None,
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
