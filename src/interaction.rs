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
    /// WHY the run paused (mirrors `AwaitingHuman.gate_kind`; review of #456, F7). `None` on rows
    /// written before the field existed.
    #[serde(default)]
    pub gate_kind: Option<String>,
    pub status: InteractionStatus,
    /// The decision payload (JSON text, e.g. `{"approve":true,"amend":null}`) once resolved.
    #[serde(default)]
    pub answer: Option<String>,
    /// Creation timestamp (unix millis).
    pub created_at: i64,
    /// Resolution timestamp (unix millis).
    #[serde(default)]
    pub resolved_at: Option<i64>,
    /// (core#1808) Which opening of a gate this row is: one more than any gate row of its run
    /// had when it opened ([`next_gate_seq`]), so a gate that closes and re-opens at the same ord
    /// (a retry that lands on a new gate) is a different opening. A gate answer may name it
    /// ([`crate::workflow::GateExpect::gate_seq`]); `0` on a row written before the field existed.
    #[serde(default)]
    pub gate_seq: u32,
}

impl ToNode for InteractionRequest {
    fn node_kind() -> &'static str {
        INTERACTION_REQUEST
    }
    fn to_node(&self) -> Node {
        let mut node = Node::new(
            synthetic_symbol(INTERACTION_REQUEST, &self.id),
            NodeKind::Other(INTERACTION_REQUEST.to_string()),
            row_name(&self.session_id, &self.id),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{INTERACTION_REQUEST}/{}", self.id), Span::ZERO),
        );
        if let serde_json::Value::Object(map) =
            serde_json::to_value(self).expect("InteractionRequest serializes to JSON")
        {
            node.metadata = map;
        }
        crate::tool_activity::redact_fields(&mut node.metadata, &["prompt", "answer"]);
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
    gate_kind: &str,
    now_ms: i64,
) -> InteractionRequest {
    open_gate_keyed(
        session_id,
        ord,
        reviewing_ord,
        prompt,
        gate_kind,
        None,
        now_ms,
    )
}

/// [`open_gate`] for a gate that carries its OWN id (DES-TEAMING-002 T3: a `plan_approval` gate's
/// `gate_id`, minted from the run's gate counter): the id joins the row's derivation, so each
/// opening at the same ord is its own durable row — a re-opened gate never overwrites the answered
/// one (the audit record). `None` is exactly [`open_gate`]'s row: a re-pause of an unkeyed gate
/// still reopens the same row.
pub fn open_gate_keyed(
    session_id: &str,
    ord: u32,
    reviewing_ord: Option<u32>,
    prompt: &str,
    gate_kind: &str,
    gate_key: Option<&str>,
    now_ms: i64,
) -> InteractionRequest {
    let ord_s = ord.to_string();
    let id = match gate_key {
        None => format!(
            "ir_{}",
            crate::pipeline::deterministic_id(&[session_id, "gate", &ord_s])
        ),
        Some(key) => format!(
            "ir_{}",
            crate::pipeline::deterministic_id(&[session_id, "gate", &ord_s, key])
        ),
    };
    InteractionRequest {
        id,
        session_id: session_id.to_string(),
        kind: InteractionKind::Gate,
        ord: Some(ord),
        reviewing_ord,
        prompt: prompt.to_string(),
        gate_kind: Some(gate_kind.to_string()),
        status: InteractionStatus::Open,
        answer: None,
        created_at: now_ms,
        resolved_at: None,
        gate_seq: 0,
    }
}

/// (core#1808) The number the next gate opening of `session_id` carries: one more than the
/// highest any of its gate rows holds (answered ones included), so it never repeats in a run.
pub fn next_gate_seq(store: &dyn GraphRead, session_id: &str) -> anyhow::Result<u32> {
    Ok(list_interactions(store, Some(session_id), None)?
        .iter()
        .filter(|r| r.kind == InteractionKind::Gate)
        .map(|r| r.gate_seq)
        .max()
        .unwrap_or(0)
        .saturating_add(1))
}

/// (core#1795) The node name of request `id` on run `session_id`: `{session_id}:{id}`. The name is
/// what the store indexes, so one run's requests are reached by the name prefix `{session_id}:`
/// without reading every request on the store (as a run's units are, `domain::session_units`).
/// The symbol id stays `synthetic_symbol(INTERACTION_REQUEST, id)`: only the name carries the run.
fn row_name(session_id: &str, id: &str) -> String {
    format!("{session_id}:{id}")
}

/// Every interaction request node, of every run: the whole-store read.
fn all_rows(store: &dyn GraphRead) -> anyhow::Result<Vec<Node>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(INTERACTION_REQUEST.to_string())],
        ..Default::default()
    };
    Ok(store.find_symbols(&query)?)
}

/// (core#1795) One run's request nodes, reached through the store's name index ([`row_name`]), so
/// a one-run read (the gate card's, `gate_actions`) costs the same on a store of 4 runs or 4000.
/// The prefix match is ASCII case-insensitive and also takes the run's units and any run whose id
/// this one prefixes, so a node is kept only when it is a request whose raw `session_id` is this
/// run's.
fn run_rows(store: &dyn GraphRead, session_id: &str) -> anyhow::Result<Vec<Node>> {
    let kind = NodeKind::Other(INTERACTION_REQUEST.to_string());
    Ok(store
        .find_symbols_by_name_prefix(&format!("{session_id}:"), None)?
        .into_iter()
        .filter(|n| n.kind == kind)
        .filter(|n| n.metadata.get("session_id").and_then(|v| v.as_str()) == Some(session_id))
        .collect())
}

/// (core#1795) Name every request row written before [`row_name`] (named by its bare id) under its
/// run, so [`list_interactions`] reaches it by the run's name prefix. Run once by the actor at
/// boot, before its startup work; a row already named under its run is left untouched, so every
/// boot after the first writes nothing. A row whose `session_id` or `id` cannot be read is left as
/// it is: [`list_interactions`] could never decode it either. Returns how many rows were renamed.
pub(crate) fn name_rows_under_their_runs(store: &mut dyn GraphStore) -> anyhow::Result<usize> {
    let mut renamed = Vec::new();
    for mut node in all_rows(store)? {
        let field = |k: &str| node.metadata.get(k).and_then(|v| v.as_str());
        let (Some(session_id), Some(id)) = (field("session_id"), field("id")) else {
            continue;
        };
        let name = row_name(session_id, id);
        if node.name != name {
            node.name = name;
            renamed.push(node);
        }
    }
    if !renamed.is_empty() {
        crate::domain::put_nodes(store, &renamed)?;
    }
    Ok(renamed.len())
}

/// (core#1795) How long the actor waits before it tries [`name_rows_under_their_runs`] again after
/// the pass failed.
pub(crate) const NAMING_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// (core#1795) Run [`name_rows_under_their_runs`] if it is due at `now`, and say when it is next
/// due: `None` once it has succeeded. A pass that fails (a store error at boot) is tried again
/// [`NAMING_RETRY`] after the failed attempt ended (not after it began: an attempt that waited out
/// the store's busy timeout must not be due again at once), so a gate opened before the upgrade is
/// not left unreadable for the daemon's whole life. The actor runs it at boot and, while it is
/// still due, before each command.
pub(crate) fn name_rows_when_due(
    store: &mut dyn GraphStore,
    due: Option<std::time::Instant>,
    now: std::time::Instant,
) -> Option<std::time::Instant> {
    let due = due?;
    if now < due {
        return Some(due);
    }
    match name_rows_under_their_runs(store) {
        Ok(0) => None,
        Ok(n) => {
            eprintln!("wicked-core: named {n} interaction request(s) under their run (core#1795)");
            None
        }
        Err(e) => {
            eprintln!(
                "wicked-core: could not name interaction requests under their run (core#1795): \
                 {e:#}; trying again in {}s (a gate opened before this version is not read until then)",
                NAMING_RETRY.as_secs()
            );
            Some(std::time::Instant::now() + NAMING_RETRY)
        }
    }
}

/// All interaction requests, optionally filtered by session and/or status, newest first. One run's
/// requests are read through the name index ([`run_rows`]); with no run, every request is read.
pub fn list_interactions(
    store: &dyn GraphRead,
    session_id: Option<&str>,
    status: Option<InteractionStatus>,
) -> anyhow::Result<Vec<InteractionRequest>> {
    let rows = match session_id {
        Some(s) => run_rows(store, s)?,
        None => all_rows(store)?,
    };
    let mut requests: Vec<InteractionRequest> = rows
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
    resolve_open_for_session_keeping(store, session_id, status, answer, now_ms).map(|o| o.len())
}

/// (core#2022) [`resolve_open_for_session`], returning the rows as they were open, so a caller whose
/// answer fails before it commits can put them back ([`reopen`]).
pub fn resolve_open_for_session_keeping(
    store: &mut dyn GraphStore,
    session_id: &str,
    status: InteractionStatus,
    answer: Option<String>,
    now_ms: i64,
) -> anyhow::Result<Vec<InteractionRequest>> {
    debug_assert!(
        status != InteractionStatus::Open,
        "resolving TO open is a bug"
    );
    let open = list_interactions(store, Some(session_id), Some(InteractionStatus::Open))?;
    for req in &open {
        let mut req = req.clone();
        req.status = status;
        req.answer = answer.clone();
        req.resolved_at = Some(now_ms);
        put_node(store, req.to_node())?;
    }
    Ok(open)
}

/// (core#2022) Put rows back as [`resolve_open_for_session_keeping`] found them (open, unanswered).
pub fn reopen(store: &mut dyn GraphStore, rows: &[InteractionRequest]) -> anyhow::Result<()> {
    for req in rows {
        put_node(store, req.to_node())?;
    }
    Ok(())
}

/// (core#1319) The key an answered triage row carries for the attempt its approve re-dispatched.
pub const TRIAGE_REDISPATCH_KEY: &str = "redispatch_attempt";

/// (core#1319) Record on the answered `triage` row at `ord` that its answer re-dispatched the unit
/// at `attempt`. The fresh attempt keeps the escalated failure's record apart; this marker keeps
/// that attempt a first try for the automatic remedies, which bound themselves by attempt.
pub fn mark_triage_redispatch(
    store: &mut dyn GraphStore,
    session_id: &str,
    ord: u32,
    attempt: u32,
) -> anyhow::Result<()> {
    let answered = list_interactions(store, Some(session_id), Some(InteractionStatus::Answered))?;
    let Some(mut row) = answered
        .into_iter()
        .find(|r| r.ord == Some(ord) && r.gate_kind.as_deref() == Some("triage"))
    else {
        return Ok(());
    };
    let mut answer = row
        .answer
        .as_deref()
        .and_then(|a| serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(a).ok())
        .unwrap_or_default();
    answer.insert(TRIAGE_REDISPATCH_KEY.to_string(), attempt.into());
    row.answer = Some(serde_json::Value::Object(answer).to_string());
    put_node(store, row.to_node())
}

/// (core#1319) Whether `attempt` of the unit at `ord` is the re-dispatch an answer at its triage
/// gate made ([`mark_triage_redispatch`]). An automatic retry bumps past it, so this holds for one
/// attempt per answer.
pub fn redispatched_by_triage(
    store: &dyn GraphRead,
    session_id: &str,
    ord: u32,
    attempt: u32,
) -> bool {
    attempt > 0
        && list_interactions(store, Some(session_id), Some(InteractionStatus::Answered))
            .unwrap_or_default()
            .iter()
            .filter(|r| r.ord == Some(ord) && r.gate_kind.as_deref() == Some("triage"))
            .filter_map(|r| r.answer.as_deref())
            .filter_map(|a| serde_json::from_str::<serde_json::Value>(a).ok())
            .any(|a| {
                a.get(TRIAGE_REDISPATCH_KEY).and_then(|n| n.as_u64()) == Some(u64::from(attempt))
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wicked_apps_core::open_store;

    #[test]
    fn recorded_prompt_and_answer_redact_tokens_and_preserve_plain_prose() {
        let token = format!(concat!("gh", "p_", "{}"), "Fake7".repeat(4));
        let sensitive = format!("I used {token} to push");
        let plain = "  Reviewed café.\r\n\tKeep  both spaces.\n";
        for (text, expected) in [(sensitive.as_str(), "I used *** to push"), (plain, plain)] {
            let mut request = open_gate("s", 1, None, text, "def", 1);
            request.answer = Some(text.into());
            let recorded = InteractionRequest::from_node(&request.to_node()).unwrap();
            assert_eq!(recorded.prompt.as_bytes(), expected.as_bytes());
            assert_eq!(
                recorded.answer.as_deref().unwrap().as_bytes(),
                expected.as_bytes()
            );
            assert!(!recorded.prompt.contains(&token));
            assert!(!recorded.answer.as_deref().unwrap().contains(&token));
        }
    }

    /// (core#1795) A one-run read reaches the run's requests through the store's name index: it
    /// never asks the store for every request node, so the gate card's read costs the same on a
    /// store of 4 runs or 4000. A run whose id the asked one prefixes and the run's units are not
    /// its requests. A row written before the rows were named under their run (named by its bare
    /// id) is reached once the boot pass has renamed it, and a second pass writes nothing.
    #[test]
    fn a_one_run_interaction_read_never_scans_every_request_on_the_store() {
        use std::cell::Cell;
        use wicked_apps_core::SqliteStore;
        use wicked_estate_core::*;

        struct ScanProbe {
            inner: SqliteStore,
            request_scans: Cell<usize>,
        }
        macro_rules! forward {
            ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
                $(fn $name(&self, $($arg: $ty),*) -> $ret {
                    GraphRead::$name(&self.inner, $($arg),*)
                })*
            };
        }
        impl GraphRead for ScanProbe {
            forward! {
                capabilities() -> StoreCapabilities;
                get_node(id: &SymbolId) -> Result<Option<Node>>;
                find_symbols_by_name_prefix(prefix: &str, limit: Option<usize>) -> Result<Vec<Node>>;
                neighbors(id: &SymbolId, dir: Direction) -> Result<Vec<Edge>>;
                traverse(start: &SymbolId, spec: &TraversalSpec) -> Result<Subgraph>;
                all_nodes() -> Result<Vec<Node>>;
                all_edges() -> Result<Vec<Edge>>;
                unresolved_refs_for_name(name: &str) -> Result<Vec<UnresolvedRef>>;
                file_digest(file: &str) -> Result<Option<String>>;
                indexed_files() -> Result<Vec<String>>;
                file_git_sha(file: &str) -> Result<Option<String>>;
                repo_info() -> Result<Option<RepoInfo>>;
                edge_history(file: &str) -> Result<Vec<HistoricalEdge>>;
                file_content(file: &str) -> Result<Option<String>>;
                symbol_source(node: &Node) -> Result<Option<String>>;
                changes_since(cursor: u64) -> Result<Vec<Change>>;
                node_semantics(symbol: &SymbolId) -> Result<Option<NodeSemantics>>;
                find_by_requirement(requirement: &str) -> Result<Vec<Node>>;
                annotations(symbol: &SymbolId) -> Result<Vec<Annotation>>;
                annotations_by_type(ty: &str) -> Result<Vec<(SymbolId, Annotation)>>;
                annotations_stale_since(cutoff: i64) -> Result<Vec<(SymbolId, Annotation)>>;
                symbol_epoch(id: &SymbolId) -> Result<Option<u64>>;
                edge_supports(source: &SymbolId, target: &SymbolId, kind: &EdgeKind) -> Result<Vec<EdgeSupport>>;
                support_generation(owner: &SupportOwner) -> Result<Option<u64>>;
                support_owners() -> Result<Vec<SupportOwnerState>>;
                stats() -> Result<GraphStats>;
            }
            fn find_symbols(&self, query: &SymbolQuery) -> Result<Vec<Node>> {
                if query
                    .kinds
                    .contains(&NodeKind::Other(INTERACTION_REQUEST.to_string()))
                {
                    self.request_scans.set(self.request_scans.get() + 1);
                }
                self.inner.find_symbols(query)
            }
        }

        let mut store = open_store(Some(":memory:")).unwrap();
        let mut nodes = Vec::new();
        for run in 0..50 {
            let id = format!("run-{run}");
            nodes.push(open_gate(&id, 1, None, "p", "def", run).to_node());
        }
        // The asked run: two gates, an answered one first.
        let mut answered = open_gate("run-x", 1, None, "first", "def", 100);
        answered.status = InteractionStatus::Answered;
        nodes.push(answered.to_node());
        nodes.push(open_gate("run-x", 2, Some(1), "second", "def", 200).to_node());
        // A run whose id the asked one prefixes, and a unit of the asked run.
        nodes.push(open_gate("run-x:2", 1, None, "other run", "def", 300).to_node());
        nodes.push(crate::domain::WorkUnit::pending("run-x:u1", "run-x", 1, "d").to_node());
        // A row as the writer before core#1795 named it: by its bare id (`to_node` used `self.id`).
        let mut legacy = open_gate("run-x", 3, None, "written before", "deliver", 400).to_node();
        legacy.name = legacy
            .metadata
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();
        nodes.push(legacy);
        crate::domain::put_nodes(&mut store, &nodes).unwrap();

        let probe = ScanProbe {
            inner: store,
            request_scans: Cell::new(0),
        };
        let prompts = |status| -> Vec<String> {
            list_interactions(&probe, Some("run-x"), status)
                .unwrap()
                .into_iter()
                .map(|r| r.prompt)
                .collect()
        };
        assert_eq!(
            prompts(None),
            vec!["second", "first"],
            "the run's own rows, newest first; the bare-id row is not reached by the run's name"
        );
        assert_eq!(prompts(Some(InteractionStatus::Open)), vec!["second"]);
        assert_eq!(
            probe.request_scans.get(),
            0,
            "no one-run read asked the store for every request node"
        );

        let mut store = probe.inner;
        assert_eq!(name_rows_under_their_runs(&mut store).unwrap(), 1);
        assert_eq!(
            name_rows_under_their_runs(&mut store).unwrap(),
            0,
            "a boot after the first renames nothing"
        );
        let probe = ScanProbe {
            inner: store,
            request_scans: Cell::new(0),
        };
        let prompts = |status| -> Vec<String> {
            list_interactions(&probe, Some("run-x"), status)
                .unwrap()
                .into_iter()
                .map(|r| r.prompt)
                .collect()
        };
        assert_eq!(prompts(None), vec!["written before", "second", "first"]);
        assert_eq!(
            prompts(Some(InteractionStatus::Open)),
            vec!["written before", "second"]
        );
        assert_eq!(
            list_interactions(&probe, Some("run-x:2"), None)
                .unwrap()
                .len(),
            1,
            "the prefixed run reads its own row"
        );
        assert_eq!(probe.request_scans.get(), 0);
        assert_eq!(
            list_interactions(&probe, None, None).unwrap().len(),
            54,
            "the whole-store read still lists every row"
        );
    }

    /// (core#1795) A naming pass that fails (here: the store refuses writes) is tried again once
    /// it is due, never before, and stops being due once it has succeeded.
    #[test]
    fn a_failed_naming_pass_is_tried_again_when_due() {
        let dir = std::env::temp_dir().join(format!("core-1795-naming-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("estate.db");
        let db = db.to_str().unwrap();
        let mut rw = open_store(Some(db)).unwrap();
        let mut legacy = open_gate("run-r", 1, None, "p", "def", 1).to_node();
        legacy.name = legacy.metadata["id"].as_str().unwrap().to_string();
        put_node(&mut rw, legacy).unwrap();

        let t0 = std::time::Instant::now();
        let mut ro = wicked_apps_core::open_store_ro(Some(db)).unwrap();
        // An attempt that began long before it ended (it waited out the store's busy timeout): the
        // retry is due a full NAMING_RETRY after it ENDED, never at once.
        let began = t0.checked_sub(NAMING_RETRY * 2).unwrap_or(t0);
        let due = name_rows_when_due(&mut ro, Some(began), began);
        let ended = std::time::Instant::now();
        let at = due.expect("a failed pass is due again later");
        assert!(
            at + std::time::Duration::from_millis(1) >= ended + NAMING_RETRY,
            "due a full NAMING_RETRY after the failed attempt ended"
        );
        assert_eq!(
            name_rows_when_due(&mut rw, due, at - std::time::Duration::from_millis(1)),
            due,
            "not before it is due"
        );
        assert!(list_interactions(&rw, Some("run-r"), None)
            .unwrap()
            .is_empty());
        assert_eq!(
            name_rows_when_due(&mut rw, due, at),
            None,
            "done once it succeeds"
        );
        assert_eq!(
            list_interactions(&rw, Some("run-r"), None).unwrap().len(),
            1
        );
        assert_eq!(name_rows_when_due(&mut rw, None, at + NAMING_RETRY), None);
        drop((rw, ro));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn request_round_trips_through_node() {
        let r = open_gate("run-1", 2, Some(1), "Approve unit 2?", "def", 99);
        assert_eq!(InteractionRequest::from_node(&r.to_node()).unwrap(), r);
        assert_eq!(r.status, InteractionStatus::Open);
    }

    #[test]
    fn same_gate_reopens_same_row_and_resolve_closes_it() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let r = open_gate("run-1", 1, None, "Approve unit 1?", "run_level", 10);
        put_node(&mut store, r.to_node()).unwrap();
        // Re-pause on the same (session, kind, ord) — same id, still ONE row.
        let again = open_gate("run-1", 1, None, "Approve unit 1 (retry)?", "run_level", 20);
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
        put_node(
            &mut store,
            open_gate("run-a", 1, None, "a?", "run_level", 1).to_node(),
        )
        .unwrap();
        put_node(
            &mut store,
            open_gate("run-b", 1, None, "b?", "run_level", 2).to_node(),
        )
        .unwrap();
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
