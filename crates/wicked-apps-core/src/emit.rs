//! Shared event-emit seam for the four apps — the NATIVE path every app calls to publish a
//! `wicked.*` event onto the shared estate store. NO Node `wicked-bus` subprocess.
//!
//! ## v0.2.0 — native, store-backed eventing
//! The Rust collection's toolbox is Rust-only and the shared estate store IS the integration
//! substrate, so eventing no longer shells out to the Node `wicked-bus` CLI. An event is written as
//! a coarse `Node(NodeKind::Other(`[`EVENT`](crate::EVENT)`))` on the store:
//! - [`emit_event_to`] — the caller passes the store handle it ALREADY holds (no second connection,
//!   no process spawn). PREFER this on any path that already has a store open.
//! - [`emit_event`] — no store handle: resolves the shared store from [`ESTATE_DB_ENV`] and writes
//!   there; if the env is unset (tests, ephemeral/in-memory scope) it appends the event to a local
//!   append-only outbox spool instead. Still never spawns a subprocess.
//!
//! Emit is fire-and-forget by design (it must never block or fail the caller), but never silent: a
//! failed store write falls back to the outbox spool (NDJSON) with a loud [`DEADLETTER_MARKER`] on
//! stderr. A dropped event is a defect, never silent.
//!
//! Events are coarse + off the hot path (counts/ids). They are queryable from the store via
//! `find_symbols(kind = EVENT)` and ordered by the timestamp-prefixed node id (a `changes_since`-
//! style cursor drain can layer on later).
//!
//! ## Cross-platform
//! The spool root resolves via `std::env::var_os("HOME")` / `USERPROFILE` joined with
//! `std::path::Path` segments (never a hardcoded `~`), overridable via [`DEADLETTER_ENV`].

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    open_store, synthetic_symbol, GraphStore, Language, Location, Node, NodeKind, Span,
    ESTATE_DB_ENV, EVENT, SYMBOL_SCHEME,
};

/// Overrides the outbox / dead-letter spool file path. When unset, the spool defaults to
/// `<home>/.something-wicked/wicked-apps/emit-outbox.ndjson`.
pub const DEADLETTER_ENV: &str = "WICKED_APPS_EMIT_DEADLETTER";

/// Loud, greppable marker written to stderr whenever an event is spooled instead of stored.
pub const DEADLETTER_MARKER: &str = "EMIT-DEADLETTER:";

/// Process-local monotonic counter, mixed into the event node id so concurrent emits never collide.
static EMIT_SEQ: AtomicU64 = AtomicU64::new(0);

/// A coarse wicked event ready to publish through the shared seam.
///
/// `event_type` follows the ecosystem convention `wicked.<noun>.<verb>` (validate with
/// [`crate::validate_event_type`] before constructing). `payload` is an already-built JSON object.
#[derive(Debug, Clone)]
pub struct EmitEvent {
    /// `wicked.<noun>.<verb>` — e.g. `wicked.policy.evaluated`.
    pub event_type: String,
    /// Top-level domain — e.g. `wicked-governance`.
    pub domain: String,
    /// Subdomain — e.g. `governance.evaluation`.
    pub subdomain: String,
    /// Structured event payload (a JSON object).
    pub payload: serde_json::Value,
}

impl EmitEvent {
    /// Construct an event. `domain` is the producing app (e.g. `wicked-governance`); `subdomain`
    /// is the dotted subdomain (e.g. `governance.evaluation`); `event_type` is the full
    /// `wicked.<noun>.<verb>` name.
    pub fn new(
        event_type: impl Into<String>,
        domain: impl Into<String>,
        subdomain: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            event_type: event_type.into(),
            domain: domain.into(),
            subdomain: subdomain.into(),
            payload,
        }
    }

    /// The outbox record: the envelope plus the reason it was spooled. Serialized as one NDJSON line.
    fn spool_record(&self, reason: &str) -> serde_json::Value {
        serde_json::json!({
            "type": self.event_type,
            "domain": self.domain,
            "subdomain": self.subdomain,
            "payload": self.payload,
            "deadletter_reason": reason,
        })
    }
}

/// Nanoseconds since the Unix epoch as a `u64` (fits until year ~2262); `0` if the clock predates
/// the epoch (never, in practice).
fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Build the estate [`Node`] for one event. Id = `<zero-padded-nanos>-<pid>-<seq>` — unique across
/// concurrent emitters AND lexically time-ordered (so an id-ordered scan is a chronological drain).
/// The full envelope rides in `metadata`.
fn event_to_node(event: &EmitEvent, ts_nanos: u64, seq: u64) -> Node {
    let id = format!("{ts_nanos:020}-{}-{seq}", std::process::id());
    let mut node = Node::new(
        synthetic_symbol(EVENT, &id),
        NodeKind::Other(EVENT.to_string()),
        event.event_type.clone(),
        Language::new(SYMBOL_SCHEME),
        Location::new(format!("{EVENT}/{id}"), Span::ZERO),
    );
    let m = &mut node.metadata;
    m.insert(
        "event_type".to_string(),
        serde_json::Value::String(event.event_type.clone()),
    );
    m.insert(
        "domain".to_string(),
        serde_json::Value::String(event.domain.clone()),
    );
    m.insert(
        "subdomain".to_string(),
        serde_json::Value::String(event.subdomain.clone()),
    );
    m.insert("payload".to_string(), event.payload.clone());
    m.insert("ts_nanos".to_string(), serde_json::json!(ts_nanos));
    m.insert("seq".to_string(), serde_json::json!(seq));
    node
}

/// Publish `event` onto the shared store through a store handle the caller ALREADY holds — no second
/// connection, no subprocess. PREFER this wherever a store is open.
///
/// Must be called OUTSIDE an open write batch (it opens its own `begin_batch`/`commit_batch`).
/// Fire-and-forget: on a store-write error the event is spooled to the outbox and `false` returned.
pub fn emit_event_to(store: &mut dyn GraphStore, event: &EmitEvent) -> bool {
    let ts = now_nanos();
    let seq = EMIT_SEQ.fetch_add(1, Ordering::Relaxed);
    let node = event_to_node(event, ts, seq);
    match write_event_node(store, node) {
        Ok(()) => true,
        Err(e) => {
            spool(event, &format!("store write failed: {e}"));
            false
        }
    }
}

/// Write one event node through the caller's store via the batch path.
fn write_event_node(store: &mut dyn GraphStore, node: Node) -> anyhow::Result<()> {
    store.begin_batch()?;
    store.upsert_nodes(&[node])?;
    store.commit_batch()?;
    Ok(())
}

/// Publish `event` without a caller-supplied store. Resolves the shared store from [`ESTATE_DB_ENV`]
/// and writes there; if the env is unset/`:memory:` (ephemeral/test scope) the event is appended to
/// the outbox spool instead. NEVER spawns a subprocess.
///
/// Returns `true` if the event was written to the store, `false` if it was spooled. (Callers that
/// already hold a store should use [`emit_event_to`] — it avoids opening a second connection.)
pub fn emit_event(event: &EmitEvent) -> bool {
    match std::env::var(ESTATE_DB_ENV) {
        Ok(p) if !p.is_empty() && p != ":memory:" => match open_store(Some(&p)) {
            Ok(mut store) => emit_event_to(&mut store, event),
            Err(e) => {
                spool(event, &format!("open shared store failed: {e}"));
                false
            }
        },
        _ => {
            spool(event, "no shared store (WICKED_ESTATE_DB unset)");
            false
        }
    }
}

/// Resolve the home directory cross-platform without external deps: `HOME` (unix) or `USERPROFILE`
/// (Windows).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Resolve the outbox spool path: the [`DEADLETTER_ENV`] override if set, else
/// `<home>/.something-wicked/wicked-apps/emit-outbox.ndjson`.
///
/// Returns `None` only when no override is set AND the home directory cannot be resolved.
pub fn deadletter_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(DEADLETTER_ENV) {
        return Some(PathBuf::from(p));
    }
    let home = home_dir()?;
    Some(
        home.join(".something-wicked")
            .join("wicked-apps")
            .join("emit-outbox.ndjson"),
    )
}

/// Append one NDJSON line for `event` to the outbox spool, writing the loud [`DEADLETTER_MARKER`]
/// lines to stderr. Used whenever the event could not be written to the shared store.
fn spool(event: &EmitEvent, reason: &str) {
    eprintln!(
        "{DEADLETTER_MARKER} event `{}` not stored ({reason}); spooling to outbox",
        event.event_type
    );
    match append_spool(event, reason) {
        Ok(path) => eprintln!(
            "{DEADLETTER_MARKER} spooled `{}` to {}",
            event.event_type,
            path.display()
        ),
        Err(e) => eprintln!(
            "{DEADLETTER_MARKER} FAILED to spool `{}` to outbox: {e}",
            event.event_type
        ),
    }
}

/// Append one NDJSON line for `event` to the outbox spool, creating parent dirs as needed.
fn append_spool(event: &EmitEvent, reason: &str) -> std::io::Result<PathBuf> {
    let path = deadletter_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "cannot resolve outbox spool path (no HOME/USERPROFILE and no WICKED_APPS_EMIT_DEADLETTER)",
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(&event.spool_record(reason))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::{deadletter_path, emit_event, emit_event_to, EmitEvent, DEADLETTER_ENV};
    use crate::{GraphRead, NodeKind, SqliteStore, ESTATE_DB_ENV, EVENT, EV_POLICY_EVALUATED};
    use std::sync::{Mutex, MutexGuard};
    use wicked_estate_core::SymbolQuery;

    // `emit_event` reads process-global env vars; serialize the env-mutating tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn read_lines(path: &std::path::Path) -> Vec<serde_json::Value> {
        let body = std::fs::read_to_string(path).expect("spool file must exist");
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("each spool line must be valid JSON"))
            .collect()
    }

    /// `emit_event_to` writes the event as an `EVENT` node on the SAME store, queryable by kind,
    /// payload intact — and no subprocess. Falsifier: a write that lands no event node → empty
    /// `find_symbols` → fail.
    #[test]
    fn emit_event_to_writes_an_event_node() {
        let mut store = SqliteStore::in_memory().expect("open in-memory estate store");
        let ev = EmitEvent::new(
            EV_POLICY_EVALUATED,
            "wicked-governance",
            "governance.evaluation",
            serde_json::json!({ "claim_id": "c1", "decision": "allow" }),
        );
        assert!(
            emit_event_to(&mut store, &ev),
            "store-backed emit must report stored"
        );

        let nodes = store
            .find_symbols(&SymbolQuery {
                kinds: vec![NodeKind::Other(EVENT.to_string())],
                ..Default::default()
            })
            .expect("find_symbols ok");
        assert_eq!(nodes.len(), 1, "exactly one event node must be written");
        let n = &nodes[0];
        assert_eq!(n.metadata.get("event_type").unwrap(), EV_POLICY_EVALUATED);
        assert_eq!(n.metadata.get("domain").unwrap(), "wicked-governance");
        assert_eq!(n.metadata.get("payload").unwrap()["claim_id"], "c1");
        assert!(
            n.metadata.get("ts_nanos").unwrap().is_u64(),
            "ts_nanos must be a u64 for chronological ordering"
        );
    }

    /// With no shared store configured, `emit_event` appends a parseable NDJSON line to the outbox
    /// (and never spawns a subprocess). Falsifier: nothing spooled → `read_lines` empty → fail.
    #[test]
    fn emit_event_without_store_spools_to_outbox() {
        let _guard = lock_env();
        let dir = std::env::temp_dir().join(format!("wicked-apps-emit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spool = dir.join("emit-outbox.ndjson");
        let _ = std::fs::remove_file(&spool);

        // SAFETY: env access is serialized by ENV_LOCK; vars are restored before unlock.
        unsafe {
            std::env::set_var(DEADLETTER_ENV, &spool);
            std::env::remove_var(ESTATE_DB_ENV);
        }

        let ev = EmitEvent::new(
            EV_POLICY_EVALUATED,
            "wicked-governance",
            "governance.evaluation",
            serde_json::json!({ "claim_id": "c1" }),
        );
        let stored = emit_event(&ev);

        let lines = read_lines(&spool);

        unsafe {
            std::env::remove_var(DEADLETTER_ENV);
        }
        let _ = std::fs::remove_file(&spool);

        assert!(!stored, "no WICKED_ESTATE_DB ⇒ spooled, not stored");
        assert_eq!(lines.len(), 1, "exactly one NDJSON line must be spooled");
        assert_eq!(lines[0]["type"], EV_POLICY_EVALUATED);
        assert!(
            lines[0]["deadletter_reason"].is_string(),
            "the spooled record records why it was not stored"
        );
    }

    /// Default outbox path is derived from home (cross-platform) and ends with the documented
    /// suffix — never a hardcoded `~`.
    #[test]
    fn default_outbox_path_is_under_home() {
        let _guard = lock_env();
        unsafe {
            std::env::remove_var(DEADLETTER_ENV);
        }
        if let Some(p) = deadletter_path() {
            let s = p.to_string_lossy().replace('\\', "/");
            assert!(
                s.ends_with(".something-wicked/wicked-apps/emit-outbox.ndjson"),
                "unexpected default spool path: {s}"
            );
            assert!(!s.contains('~'), "path must be expanded, not literal ~");
        }
    }
}
