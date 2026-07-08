//! Persistence on the **shared wicked-estate store** (via `wicked-apps-core`).
//!
//! This replaces the original local-JSON ledger + JSON rank projection. Three domain
//! objects map onto estate [`Node`]s, with `to_node`/`from_node` round-trips:
//!
//! - a council task → `Node(kind = Other(COUNCIL_TASK))`, with `{topic, options, criteria,
//!   state, session_id, convened, votes}` carried in metadata;
//! - the verdict → `Node(kind = Other(COUNCIL_VERDICT))` PLUS an
//!   `Edge(task → verdict, kind = Other(DECIDES))` so the graph links the decision to its
//!   task;
//! - a per-`(cli, work_kind)` ranking tally → `Node(kind = Other(CLI_RANKING))` (this is
//!   what replaces the JSON rank projection).
//!
//! Synthetic identity uses `wicked_apps_core::synthetic_symbol(kind, id)` (`Symbol::synthetic`),
//! so a task `t1`, its verdict, and a ranking never collide.
//!
//! ## Worker shared state
//! The detached worker still coordinates live task state through an in-memory
//! `Arc<Mutex<..>>` ledger ([`Ledger`]) — the worker thread and `poll` see the same map.
//! Every ledger mutation is **mirrored to the estate store** so the task/verdict records
//! are durable and retrievable as estate Nodes. The estate handle is an
//! `Arc<Mutex<SqliteStore>>` (writes take `&mut self`); cloning shares the same DB.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use wicked_apps_core::{
    synthetic_symbol, Edge, EdgeKind, GraphRead, GraphWrite, Language, Location, Node, NodeKind,
    Span, SqliteStore, CLI_RANKING, COUNCIL_TASK, COUNCIL_VERDICT, DECIDES, SYMBOL_SCHEME,
};

use crate::types::{CouncilTask, RankSignal, RankStore, Ranking, TaskState, Verdict, Vote};

// ─────────────────────────────────────────────────────────────────────────────
// A shared, cloneable handle to the estate store.
// ─────────────────────────────────────────────────────────────────────────────

/// A cloneable handle to the shared estate store. Cloning shares the same underlying DB
/// (the worker thread and the main thread hold clones). Writes serialize through the mutex.
#[derive(Clone)]
pub struct EstateHandle {
    inner: Arc<Mutex<SqliteStore>>,
}

impl EstateHandle {
    /// Wrap an already-open [`SqliteStore`].
    pub fn new(store: SqliteStore) -> Self {
        EstateHandle {
            inner: Arc::new(Mutex::new(store)),
        }
    }

    /// A hermetic in-memory estate store (tests).
    pub fn in_memory() -> anyhow::Result<Self> {
        Ok(EstateHandle::new(SqliteStore::in_memory().map_err(
            |e| anyhow::anyhow!("open in-memory estate: {e}"),
        )?))
    }

    /// Upsert nodes through the batch write path.
    pub fn upsert_nodes(&self, nodes: &[Node]) -> anyhow::Result<()> {
        let mut store = self.inner.lock().expect("estate mutex poisoned");
        store
            .begin_batch()
            .map_err(|e| anyhow::anyhow!("begin: {e}"))?;
        store
            .upsert_nodes(nodes)
            .map_err(|e| anyhow::anyhow!("upsert_nodes: {e}"))?;
        store
            .commit_batch()
            .map_err(|e| anyhow::anyhow!("commit: {e}"))?;
        Ok(())
    }

    /// Upsert edges through the batch write path.
    pub fn upsert_edges(&self, edges: &[Edge]) -> anyhow::Result<()> {
        let mut store = self.inner.lock().expect("estate mutex poisoned");
        store
            .begin_batch()
            .map_err(|e| anyhow::anyhow!("begin: {e}"))?;
        store
            .upsert_edges(edges)
            .map_err(|e| anyhow::anyhow!("upsert_edges: {e}"))?;
        store
            .commit_batch()
            .map_err(|e| anyhow::anyhow!("commit: {e}"))?;
        Ok(())
    }

    /// Fetch a node by its synthetic `(kind, id)` identity.
    pub fn get(&self, kind: &str, id: &str) -> anyhow::Result<Option<Node>> {
        let store = self.inner.lock().expect("estate mutex poisoned");
        let sym = synthetic_symbol(kind, id);
        store
            .get_node(&sym)
            .map_err(|e| anyhow::anyhow!("get_node: {e}"))
    }

    /// All nodes of a given `Other(kind)` (local-first scan — fine for the council's scale).
    pub fn nodes_of_kind(&self, kind: &str) -> anyhow::Result<Vec<Node>> {
        let store = self.inner.lock().expect("estate mutex poisoned");
        let all = store
            .all_nodes()
            .map_err(|e| anyhow::anyhow!("all_nodes: {e}"))?;
        Ok(all
            .into_iter()
            .filter(|n| matches!(&n.kind, NodeKind::Other(k) if k == kind))
            .collect())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Node mapping helpers.
// ─────────────────────────────────────────────────────────────────────────────

/// Build a synthetic estate [`Node`] of `Other(kind)` with stable identity `(kind, id)`.
fn make_node(kind: &str, id: &str, name: impl Into<String>) -> Node {
    Node::new(
        synthetic_symbol(kind, id),
        NodeKind::Other(kind.to_string()),
        name,
        Language::new(SYMBOL_SCHEME),
        Location::new(format!("{kind}/{id}"), Span::ZERO),
    )
}

fn str_meta(node: &Node, key: &str) -> Option<String> {
    node.metadata
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn json_meta<T: serde::de::DeserializeOwned>(node: &Node, key: &str) -> Option<T> {
    node.metadata
        .get(key)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

// ─────────────────────────────────────────────────────────────────────────────
// CouncilTask ↔ COUNCIL_TASK node (round-trip).
// ─────────────────────────────────────────────────────────────────────────────

/// Project a task + its live runtime fields into a `COUNCIL_TASK` node.
pub fn task_to_node(rec: &TaskRecord) -> Node {
    let task = &rec.task;
    let mut node = make_node(COUNCIL_TASK, &task.id, task.topic.clone());
    let m = &mut node.metadata;
    m.insert("id".into(), serde_json::Value::String(task.id.clone()));
    m.insert(
        "topic".into(),
        serde_json::Value::String(task.topic.clone()),
    );
    m.insert(
        "session_id".into(),
        serde_json::Value::String(task.session_id.clone()),
    );
    m.insert(
        "state".into(),
        serde_json::Value::String(rec.state.as_str().to_string()),
    );
    m.insert("options".into(), serde_json::json!(task.options));
    m.insert("criteria".into(), serde_json::json!(task.criteria));
    m.insert("convened".into(), serde_json::json!(rec.convened));
    m.insert("votes".into(), serde_json::json!(rec.votes));
    node
}

/// Reconstruct a [`TaskRecord`] (task + state + convened + votes) from a `COUNCIL_TASK`
/// node. The verdict is reconstructed separately (it is its own node).
pub fn task_from_node(node: &Node) -> anyhow::Result<TaskRecord> {
    match &node.kind {
        NodeKind::Other(k) if k == COUNCIL_TASK => {}
        other => anyhow::bail!("expected NodeKind::Other({COUNCIL_TASK:?}), got {other:?}"),
    }
    let id = str_meta(node, "id").ok_or_else(|| anyhow::anyhow!("task node missing `id`"))?;
    let topic = str_meta(node, "topic").unwrap_or_else(|| node.name.clone());
    let session_id = str_meta(node, "session_id").unwrap_or_default();
    let options: Vec<String> = json_meta(node, "options").unwrap_or_default();
    let criteria: Vec<String> = json_meta(node, "criteria").unwrap_or_default();
    let convened: Vec<String> = json_meta(node, "convened").unwrap_or_default();
    let votes: Vec<Vote> = json_meta(node, "votes").unwrap_or_default();
    let state = str_meta(node, "state")
        .and_then(|s| TaskState::from_str_opt(&s))
        .unwrap_or(TaskState::Queued);

    Ok(TaskRecord {
        task: CouncilTask {
            id,
            topic,
            options,
            criteria,
            session_id,
        },
        state,
        convened,
        votes,
        verdict: None,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Verdict ↔ COUNCIL_VERDICT node (round-trip) + task→verdict edge.
// ─────────────────────────────────────────────────────────────────────────────

/// Project a [`Verdict`] into a `COUNCIL_VERDICT` node. The verdict's stable id is its
/// `task_id` (one verdict per task).
pub fn verdict_to_node(verdict: &Verdict) -> Node {
    let mut node = make_node(COUNCIL_VERDICT, &verdict.task_id, verdict.kind.clone());
    let m = &mut node.metadata;
    m.insert(
        "task_id".into(),
        serde_json::Value::String(verdict.task_id.clone()),
    );
    // Store the whole verdict struct so from_node is a lossless decode.
    m.insert(
        "verdict".into(),
        serde_json::to_value(verdict).unwrap_or(serde_json::Value::Null),
    );
    node
}

/// Reconstruct a [`Verdict`] from a `COUNCIL_VERDICT` node.
pub fn verdict_from_node(node: &Node) -> anyhow::Result<Verdict> {
    match &node.kind {
        NodeKind::Other(k) if k == COUNCIL_VERDICT => {}
        other => anyhow::bail!("expected NodeKind::Other({COUNCIL_VERDICT:?}), got {other:?}"),
    }
    json_meta(node, "verdict").ok_or_else(|| anyhow::anyhow!("verdict node missing `verdict`"))
}

/// The `task → verdict` edge (`DECIDES`): the council's decision over its task.
pub fn task_decides_verdict_edge(task_id: &str) -> Edge {
    Edge::new(
        synthetic_symbol(COUNCIL_TASK, task_id),
        synthetic_symbol(COUNCIL_VERDICT, task_id),
        EdgeKind::Other(DECIDES.to_string()),
        wicked_apps_core::ResolutionTier::Parsed,
        "wicked-council",
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Ranking tally ↔ CLI_RANKING node (replaces the JSON rank projection).
// ─────────────────────────────────────────────────────────────────────────────

/// Accumulated observations for one `(cli, work_kind)` pair.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Tally {
    /// Total observations.
    pub n: u32,
    /// How many produced a usable vote.
    pub successes: u32,
    /// How many agreed with the eventual consensus.
    pub agreements: u32,
    /// Sum of latencies (for an average).
    pub latency_ms_sum: u64,
}

/// The stable ranking id for a `(cli, work_kind)` pair (used as the node's synthetic id).
fn ranking_id(cli: &str, work_kind: &str) -> String {
    format!("{cli}\u{1f}{work_kind}")
}

/// Project a `(cli, work_kind)` tally into a `CLI_RANKING` node.
fn ranking_to_node(cli: &str, work_kind: &str, tally: &Tally) -> Node {
    let id = ranking_id(cli, work_kind);
    let mut node = make_node(CLI_RANKING, &id, format!("{cli}/{work_kind}"));
    let m = &mut node.metadata;
    m.insert("cli".into(), serde_json::Value::String(cli.to_string()));
    m.insert(
        "work_kind".into(),
        serde_json::Value::String(work_kind.to_string()),
    );
    m.insert("n".into(), serde_json::json!(tally.n));
    m.insert("successes".into(), serde_json::json!(tally.successes));
    m.insert("agreements".into(), serde_json::json!(tally.agreements));
    m.insert(
        "latency_ms_sum".into(),
        serde_json::json!(tally.latency_ms_sum),
    );
    node
}

/// Decode a `(cli, work_kind, tally)` from a `CLI_RANKING` node.
fn ranking_from_node(node: &Node) -> Option<(String, String, Tally)> {
    if !matches!(&node.kind, NodeKind::Other(k) if k == CLI_RANKING) {
        return None;
    }
    let cli = str_meta(node, "cli")?;
    let work_kind = str_meta(node, "work_kind")?;
    let tally = Tally {
        n: json_meta(node, "n").unwrap_or(0),
        successes: json_meta(node, "successes").unwrap_or(0),
        agreements: json_meta(node, "agreements").unwrap_or(0),
        latency_ms_sum: json_meta(node, "latency_ms_sum").unwrap_or(0),
    };
    Some((cli, work_kind, tally))
}

// ─────────────────────────────────────────────────────────────────────────────
// EstateRankStore — the RankStore impl backed by CLI_RANKING nodes.
// ─────────────────────────────────────────────────────────────────────────────

/// `RankStore` projection backed by the estate store (one `CLI_RANKING` node per
/// `(cli, work_kind)`). The score is a plain success-rate signal, NOT a model confidence;
/// every [`Ranking`] carries provenance.
#[derive(Clone)]
pub struct EstateRankStore {
    estate: EstateHandle,
}

impl EstateRankStore {
    /// Build a rank store over a shared estate handle.
    pub fn new(estate: EstateHandle) -> Self {
        EstateRankStore { estate }
    }
}

impl RankStore for EstateRankStore {
    fn record(&self, cli: &str, work_kind: &str, signal: &RankSignal) {
        // Read-modify-write the tally node. (Council scale is low; the mutex serializes.)
        let id = ranking_id(cli, work_kind);
        let mut tally: Tally = self
            .estate
            .get(CLI_RANKING, &id)
            .ok()
            .flatten()
            .and_then(|n| ranking_from_node(&n).map(|(_, _, t)| t))
            .unwrap_or_default();

        tally.n += 1;
        if signal.success {
            tally.successes += 1;
        }
        if signal.agreement_with_consensus {
            tally.agreements += 1;
        }
        tally.latency_ms_sum = tally.latency_ms_sum.saturating_add(signal.latency_ms);

        let node = ranking_to_node(cli, work_kind, &tally);
        // Fire-and-forget durability; a write failure must not panic the worker thread.
        let _ = self.estate.upsert_nodes(&[node]);
    }

    fn best_for(&self, work_kind: &str, top: usize) -> Vec<Ranking> {
        let nodes = self.estate.nodes_of_kind(CLI_RANKING).unwrap_or_default();
        let mut rankings: Vec<Ranking> = nodes
            .iter()
            .filter_map(ranking_from_node)
            .filter(|(_, wk, _)| wk == work_kind)
            .map(|(cli, wk, tally)| {
                // Score blends success rate and agreement-with-consensus rate. Plain rates
                // — explicitly NOT averaged model confidence.
                let success_rate = ratio(tally.successes, tally.n);
                let agreement_rate = ratio(tally.agreements, tally.n);
                let score = 0.5 * success_rate + 0.5 * agreement_rate;
                let avg_latency = if tally.n > 0 {
                    tally.latency_ms_sum / tally.n as u64
                } else {
                    0
                };
                Ranking {
                    cli,
                    work_kind: wk,
                    score,
                    n: tally.n,
                    provenance: format!(
                        "success_rate={:.2} ({}/{}), agreement_with_consensus={:.2} ({}/{}), avg_latency_ms={} \
                         [estate CLI_RANKING projection]",
                        success_rate, tally.successes, tally.n,
                        agreement_rate, tally.agreements, tally.n,
                        avg_latency,
                    ),
                }
            })
            .collect();

        // Best first: score desc, then more observations, then cli name.
        rankings.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.n.cmp(&a.n))
                .then_with(|| a.cli.cmp(&b.cli))
        });
        rankings.truncate(top);
        rankings
    }
}

fn ratio(num: u32, den: u32) -> f32 {
    if den == 0 {
        0.0
    } else {
        num as f32 / den as f32
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The operational ledger: the worker's shared in-memory state, mirrored to estate.
// ─────────────────────────────────────────────────────────────────────────────

/// One ledger row: the task, its current state, the convened CLIs, the collected votes,
/// and (once synthesized) the verdict.
#[derive(Debug, Clone)]
pub struct TaskRecord {
    /// The original request.
    pub task: CouncilTask,
    /// Current lifecycle state.
    pub state: TaskState,
    /// The keys of the CLIs convened for this task.
    pub convened: Vec<String>,
    /// Votes collected so far.
    pub votes: Vec<Vote>,
    /// The synthesized verdict, once `state == Voted`.
    pub verdict: Option<Verdict>,
}

/// A cloneable handle to the shared ledger. Cloning shares the same underlying in-memory
/// state (the worker thread and the main thread hold clones). Each mutation is mirrored
/// durably to the estate store as Nodes (+ a task→verdict edge when a verdict lands).
#[derive(Clone)]
pub struct Ledger {
    inner: Arc<Mutex<BTreeMap<String, TaskRecord>>>,
    estate: EstateHandle,
}

impl Ledger {
    /// A ledger backed by the shared estate store.
    pub fn new(estate: EstateHandle) -> Self {
        Ledger {
            inner: Arc::new(Mutex::new(BTreeMap::new())),
            estate,
        }
    }

    /// Insert a freshly-queued task and persist its `COUNCIL_TASK` node.
    pub fn insert(&self, record: TaskRecord) {
        {
            let mut guard = self.inner.lock().expect("ledger mutex poisoned");
            guard.insert(record.task.id.clone(), record.clone());
        }
        let _ = self.estate.upsert_nodes(&[task_to_node(&record)]);
    }

    /// Read a task record by id (from the in-memory ledger).
    pub fn get(&self, task_id: &str) -> Option<TaskRecord> {
        let guard = self.inner.lock().expect("ledger mutex poisoned");
        guard.get(task_id).cloned()
    }

    /// Mutate a task record in place via `f`, then mirror the updated state to estate.
    /// No-op if the id is unknown.
    pub fn update<F: FnOnce(&mut TaskRecord)>(&self, task_id: &str, f: F) {
        let updated: Option<TaskRecord> = {
            let mut guard = self.inner.lock().expect("ledger mutex poisoned");
            match guard.get_mut(task_id) {
                Some(rec) => {
                    f(rec);
                    Some(rec.clone())
                }
                None => None,
            }
        };

        if let Some(rec) = updated {
            // Mirror the task node (state/votes change over the lifecycle).
            let _ = self.estate.upsert_nodes(&[task_to_node(&rec)]);
            // When a verdict is present, persist it as its own node + the task→verdict edge.
            if let Some(verdict) = &rec.verdict {
                let _ = self.estate.upsert_nodes(&[verdict_to_node(verdict)]);
                let _ = self
                    .estate
                    .upsert_edges(&[task_decides_verdict_edge(&rec.task.id)]);
            }
        }
    }

    /// Read a durable task record straight back from the estate store (proves persistence
    /// independent of the in-memory map). Reattaches the verdict node if present.
    pub fn get_from_estate(&self, task_id: &str) -> anyhow::Result<Option<TaskRecord>> {
        let Some(task_node) = self.estate.get(COUNCIL_TASK, task_id)? else {
            return Ok(None);
        };
        let mut rec = task_from_node(&task_node)?;
        if let Some(vnode) = self.estate.get(COUNCIL_VERDICT, task_id)? {
            rec.verdict = Some(verdict_from_node(&vnode)?);
        }
        Ok(Some(rec))
    }

    /// The shared estate handle (for tests / direct reads).
    pub fn estate(&self) -> &EstateHandle {
        &self.estate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Confidence;

    fn sample_task(id: &str) -> CouncilTask {
        CouncilTask {
            id: id.into(),
            topic: "auth strategy".into(),
            options: vec!["JWT".into(), "sessions".into()],
            criteria: vec!["security".into(), "latency".into()],
            session_id: "sess-1".into(),
        }
    }

    fn sample_vote(cli: &str) -> Vote {
        Vote {
            cli: cli.into(),
            recommendation: "JWT".into(),
            top_risk: "revocation".into(),
            change_my_mind: "n/a".into(),
            disqualifier: None,
            confidence: Confidence::Verified,
            provenance: "test".into(),
        }
    }

    #[test]
    fn task_node_round_trips() {
        let rec = TaskRecord {
            task: sample_task("t1"),
            state: TaskState::Running,
            convened: vec!["claude".into(), "agy".into()],
            votes: vec![sample_vote("claude")],
            verdict: None,
        };
        let node = task_to_node(&rec);
        let back = task_from_node(&node).expect("round-trip");
        assert_eq!(back.task.id, "t1");
        assert_eq!(back.task.topic, "auth strategy");
        assert_eq!(
            back.task.options,
            vec!["JWT".to_string(), "sessions".to_string()]
        );
        assert_eq!(back.state, TaskState::Running);
        assert_eq!(back.convened, vec!["claude".to_string(), "agy".to_string()]);
        assert_eq!(back.votes.len(), 1);
        assert_eq!(back.votes[0].cli, "claude");
    }

    #[test]
    fn verdict_node_round_trips() {
        let verdict = Verdict {
            task_id: "t1".into(),
            kind: "Consensus: JWT (2/2)".into(),
            consensus: true,
            winning_recommendation: Some("JWT".into()),
            agreement_ratio: 1.0,
            risk_convergence: vec![("revocation".into(), 2)],
            dissent: vec![],
        };
        let node = verdict_to_node(&verdict);
        let back = verdict_from_node(&node).expect("round-trip");
        assert_eq!(back, verdict);
    }

    #[test]
    fn task_and_verdict_persist_as_estate_nodes() {
        let estate = EstateHandle::in_memory().unwrap();
        let ledger = Ledger::new(estate.clone());

        // Queue, then drive to a verdict via update — exactly the worker's path.
        ledger.insert(TaskRecord {
            task: sample_task("t-persist"),
            state: TaskState::Queued,
            convened: vec!["claude".into(), "agy".into()],
            votes: vec![],
            verdict: None,
        });
        ledger.update("t-persist", |rec| {
            rec.state = TaskState::Voted;
            rec.votes = vec![sample_vote("claude"), sample_vote("agy")];
            rec.verdict = Some(Verdict {
                task_id: "t-persist".into(),
                kind: "Consensus: JWT (2/2)".into(),
                consensus: true,
                winning_recommendation: Some("JWT".into()),
                agreement_ratio: 1.0,
                risk_convergence: vec![("revocation".into(), 2)],
                dissent: vec![],
            });
        });

        // The COUNCIL_TASK node is retrievable straight from the store.
        let task_node = estate
            .get(COUNCIL_TASK, "t-persist")
            .unwrap()
            .expect("task node must be persisted");
        assert!(matches!(&task_node.kind, NodeKind::Other(k) if k == COUNCIL_TASK));

        // And the full record (task + verdict) round-trips from estate alone.
        let durable = ledger
            .get_from_estate("t-persist")
            .unwrap()
            .expect("durable record present");
        assert_eq!(durable.state, TaskState::Voted);
        assert_eq!(durable.votes.len(), 2);
        let v = durable
            .verdict
            .expect("verdict reattached from its own node");
        assert!(v.consensus);
        assert_eq!(v.winning_recommendation.as_deref(), Some("JWT"));

        // The COUNCIL_VERDICT node exists as a distinct node.
        let vnode = estate.get(COUNCIL_VERDICT, "t-persist").unwrap();
        assert!(vnode.is_some(), "verdict must be its own estate node");
    }

    #[test]
    fn ranking_persists_as_cli_ranking_node() {
        let estate = EstateHandle::in_memory().unwrap();
        let store = EstateRankStore::new(estate.clone());

        store.record(
            "claude",
            "code-review",
            &RankSignal {
                success: true,
                agreement_with_consensus: true,
                latency_ms: 1200,
            },
        );
        store.record(
            "agy",
            "code-review",
            &RankSignal {
                success: true,
                agreement_with_consensus: false,
                latency_ms: 800,
            },
        );

        // The ranking is durable as a CLI_RANKING node.
        let id = ranking_id("claude", "code-review");
        let node = estate
            .get(CLI_RANKING, &id)
            .unwrap()
            .expect("CLI_RANKING node must be persisted");
        assert!(matches!(&node.kind, NodeKind::Other(k) if k == CLI_RANKING));

        // best_for reads them back, claude (agreed) ranked above agy.
        let top = store.best_for("code-review", 3);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].cli, "claude");
        assert!(top[0].score > top[1].score);
        assert!(top[0].provenance.contains("CLI_RANKING projection"));
        assert_eq!(top[0].n, 1);
    }

    #[test]
    fn record_accumulates_across_calls() {
        let estate = EstateHandle::in_memory().unwrap();
        let store = EstateRankStore::new(estate);
        for _ in 0..3 {
            store.record(
                "claude",
                "arch",
                &RankSignal {
                    success: true,
                    agreement_with_consensus: true,
                    latency_ms: 100,
                },
            );
        }
        let top = store.best_for("arch", 1);
        assert_eq!(top[0].n, 3, "tally must accumulate, not overwrite");
        assert_eq!(top[0].score, 1.0);
    }
}
