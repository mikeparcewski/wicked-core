//! `wicked-apps-core` — the shared contract every wicked-estate-universe app programs against.
//!
//! This is the SPINE for the Rust rebuild of the four apps (governance, orchestration, council,
//! agent). It is deliberately thin: it re-exports the `wicked-estate` graph essentials, pins the
//! domain node/edge-kind string vocabulary, mirrors the cross-app event catalog, defines the
//! `ConformanceClaim` wire type, opens the shared estate store, and proves — with a real
//! round-trip test against the local `SqliteStore` — that domain entities map cleanly onto the
//! estate [`Node`] API via [`ToNode`]/[`FromNode`].
//!
//! Per the estate working-style §1 ("Spine before fan-out"), nothing fans out until this compiles
//! against the real estate crates and its round-trip test is green.
//!
//! ## Verified against (not assumed)
//! - `wicked-estate-core` 0.12.0 — `Node::new(SymbolId, NodeKind, name, Language, Location)`,
//!   `Symbol::synthetic(scheme, id).id() -> SymbolId`, `NodeKind::Other(String)`,
//!   `EdgeKind::Other(String)`, `Edge::new(src, tgt, kind, ResolutionTier, resolved_by)`,
//!   `Metadata = serde_json::Map<String, serde_json::Value>`.
//! - `wicked-estate-store` 0.12.0 — `SqliteStore::open(path)` / `SqliteStore::in_memory()`, the
//!   `GraphWrite` batch path (`begin_batch`/`upsert_nodes`/`commit_batch`) and `GraphRead::get_node`.
//!
//! ## Divergences from the build brief (the compiler wins — see the crate-level notes)
//! - The emit seam (`EmitEvent`/`emit_event`) lives in the `wicked-estate` **binary** (`src/emit.rs`
//!   is `mod emit;` in `main.rs`), NOT in its library. A path dependency cannot import it. wicked-apps-core
//!   therefore ships its OWN [`emit`] seam mirroring the estate shape and shelling to `wicked-bus`.
//! - The brief asked for `GOVERNS`/`PRODUCES` as `EdgeKind::Other` strings; estate already has
//!   native `EdgeKind::Governs` / `EdgeKind::Produces` variants. We expose the string constants as
//!   specified (used with `EdgeKind::Other`) AND document the native variants on each constant.
//! - Two openers, both defaulting to estate's `.wicked-estate/graph.db` convention (NOT a
//!   "brain.db" — that belongs to the separate wicked-brain system): [`open_store`] returns the
//!   concrete [`SqliteStore`] (a SQLite-only convenience for tests/tools), while [`open_store_any`]
//!   dispatches on the spec and returns a backend-agnostic [`AnyStore`] (SQLite by default; the
//!   `postgres://` backend under the `postgres` feature, fail-closed otherwise). The engine's
//!   single-writer actor uses `open_store_any`, so the runtime is never pinned to one backend.

// ─────────────────────────────────────────────────────────────────────────────
// 1. Re-export the estate essentials the apps need.
// ─────────────────────────────────────────────────────────────────────────────

pub use wicked_estate_core::{
    // Symbol identity (synthetic-symbol construction for domain entities).
    Descriptor,
    // Node model.
    Edge,
    EdgeKind,
    // Storage traits — apps program against these, never a concrete store where avoidable.
    GraphRead,
    GraphStore,
    GraphWrite,
    Language,
    Location,
    Node,
    NodeKind,
    Package,
    ResolutionTier,
    Span,
    Suffix,
    Symbol,
    SymbolId,
};

pub use wicked_estate_store::SqliteStore;

// The Postgres backend is compiled only under the `postgres` feature (it pulls in sqlx + tokio).
// It implements the SAME sync `GraphRead`/`GraphWrite` (hence `GraphStore`) surface as SqliteStore
// — estate hides the async underneath a one-shot runtime — so the apps program against the store
// traits and never care which backend they hold.
#[cfg(feature = "postgres")]
pub use wicked_estate_store::PostgresStore;

/// The estate metadata bag type (`serde_json::Map<String, serde_json::Value>`), re-exported so
/// apps build `Node.metadata` without depending on `wicked_estate_core` directly.
pub use wicked_estate_core::Metadata;

// ─────────────────────────────────────────────────────────────────────────────
// 2. Node-kind constants — domain entity kinds carried via `NodeKind::Other(&str)`.
//    estate's `NodeKind` is a closed enum + `Other(String)` escape hatch (rules-as-data).
// ─────────────────────────────────────────────────────────────────────────────

/// A governance policy (`wicked-governance`).
pub const POLICY: &str = "policy";
/// A recorded conformance claim / evaluation result (`wicked-governance`).
pub const CONFORMANCE_CLAIM: &str = "conformance_claim";
/// An orchestration workflow (`wicked-orchestration`).
pub const WORKFLOW: &str = "workflow";
/// A workflow phase (`wicked-orchestration`).
pub const PHASE: &str = "phase";
/// A council deliberation task (`wicked-council`).
pub const COUNCIL_TASK: &str = "council_task";
/// A council verdict / vote outcome (`wicked-council`).
pub const COUNCIL_VERDICT: &str = "council_verdict";
/// A CLI ranking produced by the council (`wicked-council`).
pub const CLI_RANKING: &str = "cli_ranking";
/// An agent session (`wicked-agent`).
pub const AGENT_SESSION: &str = "agent_session";
/// A unit of distributed agent work (`wicked-agent`).
pub const WORK_UNIT: &str = "work_unit";
/// A coarse cross-app event, written onto the shared store by the [`emit`] seam (replaces the Node
/// `wicked-bus` subprocess). Queryable via `find_symbols(kind = EVENT)`, ordered by the
/// timestamp-prefixed node id. See [`emit::emit_event_to`].
pub const EVENT: &str = "event";

// ─────────────────────────────────────────────────────────────────────────────
// 3. Edge-kind constants — domain relationships carried via `EdgeKind::Other(&str)`.
//    NOTE: estate already has native `EdgeKind::Governs` and `EdgeKind::Produces`; the
//    `GOVERNS`/`PRODUCES` string constants below are provided per the brief for use with
//    `EdgeKind::Other`. Prefer the native variants where an exact match matters to estate queries.
// ─────────────────────────────────────────────────────────────────────────────

/// A policy governs a scope/phase. (estate native equivalent: `EdgeKind::Governs`.)
pub const GOVERNS: &str = "governs";
/// A council/agent decision over a task or claim.
pub const DECIDES: &str = "decides";
/// A gate guards a phase transition.
pub const GATES: &str = "gates";
/// Agent work is distributed to a worker / sub-task.
pub const DISTRIBUTES_TO: &str = "distributes_to";
/// An actor produces an artifact/outcome. (estate native equivalent: `EdgeKind::Produces`.)
pub const PRODUCES: &str = "produces";
/// Evidence backs a conformance claim or verdict.
pub const EVIDENCES: &str = "evidences";

// ─────────────────────────────────────────────────────────────────────────────
// 4. Cross-app event catalog — mirrors
//    `wicked-governance/contracts/events.json` (the canonical Node-era contract).
//    Convention: `wicked.<domain>.<noun>.<verb>`. Apps validate emitted types with `validate_event_type`.
// ─────────────────────────────────────────────────────────────────────────────

// All events share the single `wicked.crew.*` producer domain (canonical per DES-EXEC-001 /
// DES-OUTGOV-001) — the subsystem is the NOUN, not the domain segment, so one `wicked.crew.*`
// subscriber catches the whole ecosystem.

// wicked.crew.* — governance subsystem (producer: wicked-governance)
pub const EV_POLICY_REGISTERED: &str = "wicked.crew.policy.registered";
pub const EV_POLICY_EVALUATED: &str = "wicked.crew.policy.evaluated";
pub const EV_CONFORMANCE_RECORDED: &str = "wicked.crew.conformance.recorded";
pub const EV_POLICY_VIOLATED: &str = "wicked.crew.policy.violated";

// wicked.crew.* — orchestration subsystem (producer: wicked-orchestration)
pub const EV_WORKFLOW_STARTED: &str = "wicked.crew.workflow.started";
pub const EV_WORKFLOW_COMPLETED: &str = "wicked.crew.workflow.completed";
pub const EV_PHASE_STARTED: &str = "wicked.crew.phase.started";
pub const EV_PHASE_READY_FOR_GATE: &str = "wicked.crew.phase.ready-for-gate";
pub const EV_PHASE_APPROVED: &str = "wicked.crew.phase.approved";
pub const EV_PHASE_REJECTED: &str = "wicked.crew.phase.rejected";

// wicked.crew.* — council subsystem (producer: wicked-council)
pub const EV_COUNCIL_REQUESTED: &str = "wicked.crew.council.requested";
pub const EV_COUNCIL_VOTED: &str = "wicked.crew.council.voted";
pub const EV_CLI_RANKED: &str = "wicked.crew.cli.ranked";

// wicked.crew.* — agent subsystem (producer: wicked-crew). Agent-qualified nouns keep them under
// the crew domain without colliding with the cli-runner `wicked.crew.task.completed` (DES-EXEC-001).
pub const EV_AGENT_SESSION_STARTED: &str = "wicked.crew.agent_session.started";
pub const EV_AGENT_PLAN_CREATED: &str = "wicked.crew.agent_plan.created";
pub const EV_AGENT_WORK_DISTRIBUTED: &str = "wicked.crew.agent_work.distributed";
pub const EV_AGENT_TASK_COMPLETED: &str = "wicked.crew.agent_task.completed";
pub const EV_AGENT_SESSION_COMPLETED: &str = "wicked.crew.agent_session.completed";

/// Every event type in the catalog, in declaration order. The source of truth is
/// `wicked-governance/contracts/events.json`; this array mirrors it for in-process validation
/// and so apps can enumerate the contract.
pub const EVENT_CATALOG: &[&str] = &[
    EV_POLICY_REGISTERED,
    EV_POLICY_EVALUATED,
    EV_CONFORMANCE_RECORDED,
    EV_POLICY_VIOLATED,
    EV_WORKFLOW_STARTED,
    EV_WORKFLOW_COMPLETED,
    EV_PHASE_STARTED,
    EV_PHASE_READY_FOR_GATE,
    EV_PHASE_APPROVED,
    EV_PHASE_REJECTED,
    EV_COUNCIL_REQUESTED,
    EV_COUNCIL_VOTED,
    EV_CLI_RANKED,
    EV_AGENT_SESSION_STARTED,
    EV_AGENT_PLAN_CREATED,
    EV_AGENT_WORK_DISTRIBUTED,
    EV_AGENT_TASK_COMPLETED,
    EV_AGENT_SESSION_COMPLETED,
];

/// Validate a bus event type against the ecosystem grammar.
///
/// Rules (enforced WITHOUT a regex dependency — a hand-rolled scan of the same shape):
/// - matches `^wicked\.[a-z0-9_]+\.[a-z0-9_]+\.[a-z0-9_]+$` — EXACTLY four
///   dot-separated segments (`wicked.<domain>.<noun>.<verb>`)
/// - at most 128 characters
///
/// Note the catalog's `wicked.crew.phase.ready-for-gate` contains a hyphen, which the grammar
/// `[a-z0-9_]` does NOT admit. That is faithful to the brief's stated grammar; the hyphenated
/// name is a known catalog member that this strict validator rejects (see the catalog test, which
/// asserts the grammar-conformant names pass and documents the hyphen exception).
pub fn validate_event_type(s: &str) -> bool {
    if s.is_empty() || s.len() > 128 {
        return false;
    }
    let Some(rest) = s.strip_prefix("wicked.") else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    // The grammar is EXACTLY four segments: `wicked.<domain>.<noun>.<verb>`.
    // After the `wicked.` prefix, `rest` must be exactly three non-empty
    // [a-z0-9_] segments (two dots) — no more (`wicked.a.b.c.d`), no fewer
    // (`wicked.policy`).
    let mut segments = 0usize;
    for segment in rest.split('.') {
        if segment.is_empty() {
            return false;
        }
        if !segment
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return false;
        }
        segments += 1;
    }
    if segments != 3 {
        return false;
    }
    true
}

// ─────────────────────────────────────────────────────────────────────────────
// 5. ConformanceClaim — the governance evaluation wire type.
// ─────────────────────────────────────────────────────────────────────────────

/// The decision an evaluator reached for a conformance claim.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
    AllowWithConditions,
}

/// A recorded conformance evaluation — the artifact `wicked-governance` produces when it evaluates
/// a context against one or more policies. Serializable for bus payloads and store metadata.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConformanceClaim {
    pub claim_id: String,
    pub scope: String,
    pub phase: String,
    pub policy_ids: Vec<String>,
    pub decision: Decision,
    pub obligations: Vec<String>,
    pub evaluated_context_ref: String,
    pub criteria: String,
    pub evaluator_identity: String,
    /// Unix-seconds (or millis — caller's convention) timestamp of evaluation.
    pub evaluated_at: i64,
}

// ─────────────────────────────────────────────────────────────────────────────
// 6. open_store — resolve the shared estate DB and open it.
// ─────────────────────────────────────────────────────────────────────────────

/// Environment variable that points at the shared estate graph DB. Mirrors estate's CLI default
/// path (`.wicked-estate/graph.db`) when unset and no explicit path is given.
pub const ESTATE_DB_ENV: &str = "WICKED_ESTATE_DB";

/// Open the shared estate [`SqliteStore`].
///
/// Resolution order:
/// 1. `path` argument, if `Some`.
/// 2. `WICKED_ESTATE_DB` environment variable, if set.
/// 3. The estate CLI default: `.wicked-estate/graph.db` (relative to the cwd).
///
/// The special path `:memory:` opens an ephemeral in-memory store (matches estate's
/// `open_store` spec parsing).
pub fn open_store(path: Option<&str>) -> anyhow::Result<SqliteStore> {
    let resolved: String = match path {
        Some(p) => p.to_string(),
        None => {
            std::env::var(ESTATE_DB_ENV).unwrap_or_else(|_| ".wicked-estate/graph.db".to_string())
        }
    };

    let store = if resolved == ":memory:" {
        SqliteStore::in_memory()
    } else {
        SqliteStore::open(&resolved)
    }
    .map_err(|e| anyhow::anyhow!("open estate store at {resolved:?}: {e}"))?;

    Ok(store)
}

/// Open the shared estate graph **read-only** (P4b hygiene). Path resolution is identical to
/// [`open_store`], but the underlying connection uses `SQLITE_OPEN_READONLY` — no WAL pragma or
/// schema DDL runs, so this is safe to call from subprocesses (gate-hook, validator scripts) while
/// the single-writer actor holds the store open.
///
/// `:memory:` is not supported here (there is nothing to read from an empty in-memory db and the
/// open would fail anyway). Falls back to the `WICKED_ESTATE_DB` env var then `.wicked-estate/graph.db`.
pub fn open_store_ro(path: Option<&str>) -> anyhow::Result<SqliteStore> {
    let resolved: String = match path {
        Some(p) => p.to_string(),
        None => {
            std::env::var(ESTATE_DB_ENV).unwrap_or_else(|_| ".wicked-estate/graph.db".to_string())
        }
    };
    SqliteStore::open_readonly(&resolved)
        .map_err(|e| anyhow::anyhow!("open estate store read-only at {resolved:?}: {e}"))
}

/// Open the shared estate graph as a backend-agnostic [`AnyStore`], dispatched on the spec. This is
/// the opener the engine's owner (the single-writer actor) uses so the runtime is never pinned to
/// one backend: `AnyStore` is a concrete type, so `&`/`&mut` of it coerce to every store param
/// style the engine uses (`&dyn GraphRead`, `&impl GraphRead`, `&mut dyn GraphStore`).
///
/// Dispatch:
/// - a `postgres://` / `postgresql://` spec (argument or `WICKED_ESTATE_DB`) selects the Postgres
///   backend — compiled ONLY under the `postgres` feature;
/// - anything else is a SQLite path resolved exactly like [`open_store`] (incl. `:memory:`).
///
/// **Fail-closed (deny-dominates):** requesting a `postgres://` spec when this binary was built
/// WITHOUT the `postgres` feature returns a loud error rather than silently opening SQLite. Asking
/// for a backend you did not compile in must never quietly hand you a different one.
pub fn open_store_any(spec: Option<&str>) -> anyhow::Result<AnyStore> {
    let resolved: String = match spec {
        Some(p) => p.to_string(),
        None => {
            std::env::var(ESTATE_DB_ENV).unwrap_or_else(|_| ".wicked-estate/graph.db".to_string())
        }
    };

    if resolved.starts_with("postgres://") || resolved.starts_with("postgresql://") {
        #[cfg(feature = "postgres")]
        {
            let store = PostgresStore::open(&resolved)
                .map_err(|e| anyhow::anyhow!("open estate Postgres store at {resolved:?}: {e}"))?;
            return Ok(AnyStore::postgres(store));
        }
        #[cfg(not(feature = "postgres"))]
        {
            return Err(anyhow::anyhow!(
                "estate spec {resolved:?} requests the Postgres backend, but this binary was built \
                 without the `postgres` feature — rebuild with `--features postgres`. Refusing to \
                 silently fall back to SQLite (deny-dominates)."
            ));
        }
    }

    Ok(AnyStore::sqlite(open_store(Some(&resolved))?))
}

// ─────────────────────────────────────────────────────────────────────────────
// 7. ToNode / FromNode — the domain ↔ estate-Node round-trip pattern.
// ─────────────────────────────────────────────────────────────────────────────

/// The synthetic-symbol scheme apps use so their entities get stable estate `SymbolId`s without a
/// source file. `Symbol::synthetic(SYMBOL_SCHEME, id)` renders to `"<scheme> synthetic <id>:"`.
pub const SYMBOL_SCHEME: &str = "wicked-apps";

/// Build a stable [`SymbolId`] for a synthetic domain entity of `kind` (a node-kind constant such
/// as [`POLICY`]) with the given local `id`. The id namespaces by kind so a policy `p1` and a
/// workflow `p1` never collide.
pub fn synthetic_symbol(kind: &str, id: &str) -> SymbolId {
    Symbol::synthetic(SYMBOL_SCHEME, format!("{kind}/{id}")).id()
}

/// A domain entity that can be projected onto an estate [`Node`].
///
/// The contract: `to_node()` MUST be lossless w.r.t. [`from_node`](FromNode::from_node) — encode
/// everything the entity needs to reconstruct itself into `Node.metadata` (and the stable fields).
/// This is what lets every app persist its domain objects in the shared graph and read them back.
pub trait ToNode {
    /// The node-kind string (a constant such as [`POLICY`]) this entity uses with
    /// `NodeKind::Other`.
    fn node_kind() -> &'static str;
    /// Project `self` into an estate [`Node`].
    fn to_node(&self) -> Node;
}

/// Reconstruct a domain entity from an estate [`Node`] previously produced by [`ToNode::to_node`].
pub trait FromNode: Sized {
    /// Rebuild `Self` from `node`, or report why the node is not a valid encoding of `Self`.
    fn from_node(node: &Node) -> anyhow::Result<Self>;
}

/// A minimal SAMPLE entity that PROVES the estate Node API round-trips. Real domain types
/// (policies, workflows, …) land in the per-app crates later; this exists only so the spine has a
/// non-vacuous round-trip test against the real `SqliteStore`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SamplePolicy {
    pub id: String,
    pub name: String,
}

impl ToNode for SamplePolicy {
    fn node_kind() -> &'static str {
        POLICY
    }

    fn to_node(&self) -> Node {
        let symbol = synthetic_symbol(POLICY, &self.id);
        let mut node = Node::new(
            symbol,
            NodeKind::Other(POLICY.to_string()),
            self.name.clone(),
            Language::new(SYMBOL_SCHEME),
            // Synthetic entities have no source span; estate accepts a zero span.
            Location::new(format!("{POLICY}/{}", self.id), Span::ZERO),
        );
        // Round-trippable state goes into metadata. `id` is load-bearing (it is NOT recoverable
        // from the name), so it must be stored explicitly.
        node.metadata
            .insert("id".to_string(), serde_json::Value::String(self.id.clone()));
        node
    }
}

impl FromNode for SamplePolicy {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == POLICY => {}
            other => anyhow::bail!("expected NodeKind::Other({POLICY:?}), got {other:?}"),
        }
        let id = node
            .metadata
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("SamplePolicy node missing string metadata key `id`"))?
            .to_string();
        Ok(SamplePolicy {
            id,
            name: node.name.clone(),
        })
    }
}

pub mod emit;

/// `AnyStore` — the runtime-selected estate backend as one concrete type (see [`open_store_any`]).
pub mod store_any;
pub use store_any::AnyStore;

#[cfg(test)]
mod tests {
    use super::*;

    // ── 4. validate_event_type: non-vacuous accept + reject ──────────────────

    #[test]
    fn validate_event_type_accepts_catalog_grammar_names() {
        // Every catalog member whose name is grammar-conformant ([a-z0-9_] segments) must pass.
        // `wicked.crew.phase.ready-for-gate` is the documented hyphen exception (rejected below).
        for &ev in EVENT_CATALOG {
            // Domain-consistency guard (cross-product review): the WHOLE catalog shares the single
            // `wicked.crew.*` producer domain — no subsystem gets its own domain segment.
            assert!(
                ev.starts_with("wicked.crew."),
                "catalog event must be under the crew domain: {ev}"
            );
            if ev.contains('-') {
                continue;
            }
            assert!(validate_event_type(ev), "catalog event must validate: {ev}");
        }
        // Spot-check one from each subsystem explicitly.
        assert!(validate_event_type(EV_POLICY_REGISTERED));
        assert!(validate_event_type(EV_WORKFLOW_STARTED));
        assert!(validate_event_type(EV_COUNCIL_VOTED));
        assert!(validate_event_type(EV_AGENT_SESSION_STARTED));
    }

    #[test]
    fn validate_event_type_rejects_bad_names() {
        // No `wicked.` prefix.
        assert!(!validate_event_type("policy.registered"));
        // Uppercase not allowed.
        assert!(!validate_event_type("wicked.Policy.Registered"));
        // Hyphen not in the grammar (and this is the known catalog exception).
        assert!(!validate_event_type("wicked.crew.phase.ready-for-gate"));
        assert!(!validate_event_type(EV_PHASE_READY_FOR_GATE));
        // Empty segment / trailing dot.
        assert!(!validate_event_type("wicked.policy."));
        assert!(!validate_event_type("wicked..registered"));
        // Wrong segment count — the grammar is EXACTLY four segments.
        assert!(!validate_event_type("wicked.policy")); // 2 segments (too few)
        assert!(!validate_event_type("wicked.crew.phase")); // 3 segments (too few)
        assert!(!validate_event_type("wicked.crew.phase.started.extra")); // 5 (too many)
                                                                          // Bare prefix.
        assert!(!validate_event_type("wicked."));
        assert!(!validate_event_type("wicked"));
        // Over length cap (128).
        let too_long = format!("wicked.{}", "a".repeat(130));
        assert!(!validate_event_type(&too_long));
        // Empty string.
        assert!(!validate_event_type(""));
    }

    // ── 7. round-trip: SamplePolicy → Node → SqliteStore → Node → SamplePolicy ──

    #[test]
    fn sample_policy_round_trips_through_in_memory_sqlite_store() {
        // Build the domain entity.
        let original = SamplePolicy {
            id: "pol-001".to_string(),
            name: "no-deploy-on-red".to_string(),
        };

        // Project to a Node and write it to a REAL ephemeral SqliteStore via the batch write path.
        let node = original.to_node();
        let symbol = node.symbol.clone();

        let mut store = SqliteStore::in_memory().expect("open in-memory estate store");
        store.begin_batch().expect("begin batch");
        store.upsert_nodes(&[node]).expect("upsert node");
        store.commit_batch().expect("commit batch");

        // Read it back out and reconstruct the domain entity.
        let fetched = store
            .get_node(&symbol)
            .expect("get_node ok")
            .expect("node must be present after upsert");

        let recovered = SamplePolicy::from_node(&fetched).expect("from_node ok");

        // The full round-trip must be identity-preserving.
        assert_eq!(
            original, recovered,
            "SamplePolicy must survive Node round-trip through SqliteStore"
        );
        // And the synthetic symbol must be the stable, deterministic id we constructed.
        assert_eq!(symbol, synthetic_symbol(POLICY, "pol-001"));
    }

    #[test]
    fn open_store_memory_spec_opens_ephemeral() {
        // `:memory:` resolves to an in-memory store and is writable.
        let mut store = open_store(Some(":memory:")).expect("open :memory: store");
        let node = SamplePolicy {
            id: "x".into(),
            name: "y".into(),
        }
        .to_node();
        store.begin_batch().unwrap();
        store.upsert_nodes(&[node]).unwrap();
        store.commit_batch().unwrap();
        assert!(store
            .get_node(&synthetic_symbol(POLICY, "x"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn conformance_claim_serde_round_trips() {
        let claim = ConformanceClaim {
            claim_id: "c1".into(),
            scope: "repo:acme".into(),
            phase: "build".into(),
            policy_ids: vec!["pol-001".into(), "pol-002".into()],
            decision: Decision::AllowWithConditions,
            obligations: vec!["notify-secops".into()],
            evaluated_context_ref: "ctx://abc".into(),
            criteria: "all gates green".into(),
            evaluator_identity: "governance@v1".into(),
            evaluated_at: 1_750_000_000,
        };
        let json = serde_json::to_string(&claim).unwrap();
        let back: ConformanceClaim = serde_json::from_str(&json).unwrap();
        assert_eq!(claim, back);
        // decision serializes snake_case.
        assert!(json.contains("\"allow_with_conditions\""));
    }
}
