//! DURABLE INTERACTION REQUESTS (DES-PROJECT-001 §5.3) — a human prompt as ADDRESSABLE STATE, not
//! a transient event. When a run pauses at a human gate the actor writes an [`InteractionRequest`]
//! row in the SAME batch as the session's `AwaitingHuman` write, and resolves it on the same
//! command that resolves the gate — so the skin that renders the prompt need not be the skin (or
//! even the process) that was connected when it fired. This subsumes the ephemeral-GateCache fix
//! (FINDING-051): the daemon's caches demote to latency layers over these rows.
//!
//! `kind` is deliberately extensible (`gate` today; `elicitation` reserved — the engine grows an
//! elicitation surface later, ADR §9.2). The row id is DERIVED from `(session, kind, ord)` so a
//! re-pause on the same unit reopens the same logical request instead of minting a duplicate.

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, FromNode, GraphRead, GraphStore, Language, Location, Node, NodeKind, Span,
    ToNode, SYMBOL_SCHEME,
};
use wicked_estate_core::SymbolQuery;

use crate::domain::put_node;

/// Node-kind for a durable interaction request.
pub const INTERACTION_REQUEST: &str = "interaction_request";

/// What kind of human input the run is waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InteractionKind {
    Gate,
    Elicitation,
}

/// Lifecycle of a request (ADR §5.3 — the states DES-STUDIO-001 §3.3 deferred).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InteractionStatus {
    Open,
    Answered,
    Expired,
    Cancelled,
}

impl InteractionStatus {
    /// Parse the wire token — fail closed on anything else.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "open" => Ok(Self::Open),
            "answered" => Ok(Self::Answered),
            "expired" => Ok(Self::Expired),
            "cancelled" => Ok(Self::Cancelled),
            other => anyhow::bail!(
                "unrecognised interaction status '{other}' (expected open|answered|expired|cancelled)"
            ),
        }
    }
}

/// One durable human prompt. Persisted as `Node(Other("interaction_request"))`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionRequest {
    /// Derived id: `ir_` + hash16(session_id, kind, ord) — a re-pause reopens the same row.
    pub id: String,
    /// The owning run.
    pub session_id: String,
    pub kind: InteractionKind,
    /// The unit ordinal the gate pauses BEFORE (mirrors `CoreEvent::AwaitingHuman.ord`).
    #[serde(default)]
    pub ord: Option<u32>,
    /// The already-run unit the gate reviews, when attributable (mirrors `AwaitingHuman.reviewing_ord`).
    #[serde(default)]
    pub reviewing_ord: Option<u32>,
    /// The full prompt text shown to the human.
    pub prompt: String,
    pub status: InteractionStatus,
    /// The decision payload (JSON text, e.g. `{"approve":true,"amend":null}`) once resolved.
    #[serde(default)]
    pub answer: Option<String>,
    /// Creation timestamp (unix millis).
    pub created_at: i64,
    /// Resolution timestamp (unix millis).
    #[serde(default)]
    pub resolved_at: Option<i64>,
}

impl ToNode for InteractionRequest {
    fn node_kind() -> &'static str {
        INTERACTION_REQUEST
    }
    fn to_node(&self) -> Node {
        let mut node = Node::new(
            synthetic_symbol(INTERACTION_REQUEST, &self.id),
            NodeKind::Other(INTERACTION_REQUEST.to_string()),
            self.id.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{INTERACTION_REQUEST}/{}", self.id), Span::ZERO),
        );
        if let serde_json::Value::Object(map) =
            serde_json::to_value(self).expect("InteractionRequest serializes to JSON")
        {
            node.metadata = map;
        }
        node
    }
}

impl FromNode for InteractionRequest {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == INTERACTION_REQUEST => {}
            other => {
                anyhow::bail!("expected NodeKind::Other({INTERACTION_REQUEST:?}), got {other:?}")
            }
        }
        serde_json::from_value(serde_json::Value::Object(node.metadata.clone())).map_err(|e| {
            anyhow::anyhow!("node {} is not a valid InteractionRequest: {e}", node.name)
        })
    }
}

/// Wall-clock now in unix millis (for request timestamps — actor-side, like `memory::now_secs`).
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Build an OPEN gate request for a pausing run (does not persist — the caller batches it with
/// the session's `AwaitingHuman` write so the two commit together, ADR §5.3).
pub fn open_gate(
    session_id: &str,
    ord: u32,
    reviewing_ord: Option<u32>,
    prompt: &str,
    now_ms: i64,
) -> InteractionRequest {
    let id = format!(
        "ir_{}",
        crate::pipeline::deterministic_id(&[session_id, "gate", &ord.to_string()])
    );
    InteractionRequest {
        id,
        session_id: session_id.to_string(),
        kind: InteractionKind::Gate,
        ord: Some(ord),
        reviewing_ord,
        prompt: prompt.to_string(),
        status: InteractionStatus::Open,
        answer: None,
        created_at: now_ms,
        resolved_at: None,
    }
}

/// All interaction requests, optionally filtered by session and/or status, newest first.
pub fn list_interactions(
    store: &dyn GraphRead,
    session_id: Option<&str>,
    status: Option<InteractionStatus>,
) -> anyhow::Result<Vec<InteractionRequest>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(INTERACTION_REQUEST.to_string())],
        ..Default::default()
    };
    let mut requests: Vec<InteractionRequest> = store
        .find_symbols(&query)?
        .iter()
        .filter_map(|n| InteractionRequest::from_node(n).ok())
        .filter(|r| session_id.is_none_or(|s| r.session_id == s))
        .filter(|r| status.is_none_or(|st| r.status == st))
        .collect();
    requests.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    Ok(requests)
}

/// Resolve every OPEN request on `session_id` to `status` with `answer` — called by the actor in
/// the command that resolves the gate (`confirm_gate` → `answered`) or abandons it (`cancel_run`
/// → `cancelled`). Returns how many rows were resolved (0 is normal — most runs never pause).
pub fn resolve_open_for_session(
    store: &mut dyn GraphStore,
    session_id: &str,
    status: InteractionStatus,
    answer: Option<String>,
    now_ms: i64,
) -> anyhow::Result<usize> {
    debug_assert!(
        status != InteractionStatus::Open,
        "resolving TO open is a bug"
    );
    let open = list_interactions(store, Some(session_id), Some(InteractionStatus::Open))?;
    let n = open.len();
    for mut req in open {
        req.status = status;
        req.answer = answer.clone();
        req.resolved_at = Some(now_ms);
        put_node(store, req.to_node())?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wicked_apps_core::open_store;

    #[test]
    fn request_round_trips_through_node() {
        let r = open_gate("run-1", 2, Some(1), "Approve unit 2?", 99);
        assert_eq!(InteractionRequest::from_node(&r.to_node()).unwrap(), r);
        assert_eq!(r.status, InteractionStatus::Open);
    }

    #[test]
    fn same_gate_reopens_same_row_and_resolve_closes_it() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let r = open_gate("run-1", 1, None, "Approve unit 1?", 10);
        put_node(&mut store, r.to_node()).unwrap();
        // Re-pause on the same (session, kind, ord) — same id, still ONE row.
        let again = open_gate("run-1", 1, None, "Approve unit 1 (retry)?", 20);
        assert_eq!(again.id, r.id);
        put_node(&mut store, again.to_node()).unwrap();
        assert_eq!(
            list_interactions(&store, Some("run-1"), None)
                .unwrap()
                .len(),
            1
        );

        let resolved = resolve_open_for_session(
            &mut store,
            "run-1",
            InteractionStatus::Answered,
            Some(r#"{"approve":true,"amend":null}"#.into()),
            30,
        )
        .unwrap();
        assert_eq!(resolved, 1);
        let rows = list_interactions(&store, Some("run-1"), None).unwrap();
        assert_eq!(rows[0].status, InteractionStatus::Answered);
        assert_eq!(rows[0].resolved_at, Some(30));
        assert!(
            list_interactions(&store, Some("run-1"), Some(InteractionStatus::Open))
                .unwrap()
                .is_empty()
        );
        // Resolving again is a no-op (nothing open).
        assert_eq!(
            resolve_open_for_session(&mut store, "run-1", InteractionStatus::Cancelled, None, 40)
                .unwrap(),
            0
        );
    }

    #[test]
    fn filters_by_session_and_status() {
        let mut store = open_store(Some(":memory:")).unwrap();
        put_node(&mut store, open_gate("run-a", 1, None, "a?", 1).to_node()).unwrap();
        put_node(&mut store, open_gate("run-b", 1, None, "b?", 2).to_node()).unwrap();
        assert_eq!(list_interactions(&store, None, None).unwrap().len(), 2);
        assert_eq!(
            list_interactions(&store, Some("run-a"), None)
                .unwrap()
                .len(),
            1
        );
        resolve_open_for_session(&mut store, "run-a", InteractionStatus::Cancelled, None, 3)
            .unwrap();
        let open = list_interactions(&store, None, Some(InteractionStatus::Open)).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].session_id, "run-b");
    }
}
