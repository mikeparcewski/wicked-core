//! RUN MEMORY — the orchestrator's episodic memory over `wicked_memory`. The actor (the single
//! writer) owns ONE `RunMemory`; it captures run outcomes as episodic memories and recalls them for
//! context (e.g. when briefing a new run). Best-effort: the actor holds it as `Option`, so a memory
//! failure never breaks a run.
//!
//! The store is a SEPARATE single-writer store from the estate graph (its own db + vector sidecar),
//! opened as a sibling of the estate db. By default it uses REAL semantic embeddings (Model2Vec, via
//! [`choose_embedder`]) so recall matches by meaning, falling back to the dependency-free lexical
//! `HashEmbedder` on a fresh store when the model can't load (offline / `WICKED_MEMORY_EMBEDDER=hash`).
//! The embedder choice is recorded per store and honored on reopen (no silent dimension mismatch).
//!
//! It also exposes an in-process MCP tool surface ([`RunMemory::mcp`]) so other agents can call
//! `memory.recall` / `memory.capture` over JSON-RPC.

use wicked_estate_memory::MemoryEngine;
use wicked_estate_memory_core::{MemKind, Memory, Scope, Tier};

/// A recalled memory, flattened for the Core API (egui-free, serde-friendly).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RecalledMemory {
    pub content: String,
    pub score: f64,
    pub tier: String,
}

/// The orchestrator's memory store.
pub struct RunMemory {
    engine: MemoryEngine,
}

impl RunMemory {
    /// Open the memory store as a sibling of the estate db (`<estate>.mem`), with the REALISTIC
    /// default embedder ([`choose_embedder`]).
    pub fn open(estate_path: &str) -> anyhow::Result<Self> {
        let mem_path = mem_path_for(estate_path);
        let engine = MemoryEngine::open(&mem_path)
            .map_err(|e| anyhow::anyhow!("open memory store at {mem_path}: {e}"))?;
        Ok(Self {
            engine: choose_embedder(engine, &mem_path)?,
        })
    }

    /// Capture an episodic memory — a run outcome / decision the orchestrator learned — at `scope`
    /// (e.g. `app:<id>` so the memory belongs to one application; `Scope::root()` for global).
    pub fn capture(
        &mut self,
        content: impl Into<String>,
        scope: Scope,
        now: i64,
    ) -> anyhow::Result<()> {
        let mem = Memory::new(MemKind::Episode, Tier::Episodic, scope, content, now);
        self.engine
            .capture(&mem)
            .map_err(|e| anyhow::anyhow!("capture memory: {e}"))
    }

    /// LIST captured memories within `scope`'s subtree (scope is an ancestor of the memory's scope —
    /// so `app:<id>` returns that app's memories, and `Scope::root()` returns all), newest first.
    /// A direct listing, NOT a similarity search, so stored memories always show.
    pub fn list(&self, scope: &Scope, limit: usize) -> anyhow::Result<Vec<RecalledMemory>> {
        let mut all: Vec<Memory> = self
            .engine
            .all_memories()
            .map_err(|e| anyhow::anyhow!("list memories: {e}"))?
            .into_iter()
            .filter(|m| scope.is_ancestor_of(&m.scope))
            .collect();
        all.sort_by_key(|m| std::cmp::Reverse(m.created_at)); // newest first
        Ok(all
            .into_iter()
            .take(limit)
            .map(|m| RecalledMemory {
                content: m.content,
                score: 1.0,
                tier: format!("{:?}", m.tier),
            })
            .collect())
    }

    /// Dispatch an MCP-style JSON-RPC request against the memory engine — the in-process tool surface
    /// other agents/surfaces call to use the orchestrator's memory. Supports `tools/list` and
    /// `tools/call` for `memory.recall` + `memory.capture`. Returns the JSON-RPC response, or `None`
    /// for a notification (no `id`).
    pub fn mcp(&mut self, req: &serde_json::Value, now: i64) -> Option<serde_json::Value> {
        use serde_json::json;
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

        let result: Result<serde_json::Value, String> = match method {
            "tools/list" => Ok(json!({ "tools": [
                { "name": "memory.recall", "description": "Recall memories relevant to a query",
                  "inputSchema": { "type": "object", "properties": {
                      "query": { "type": "string" }, "k": { "type": "integer" } }, "required": ["query"] } },
                { "name": "memory.capture", "description": "Capture an episodic memory",
                  "inputSchema": { "type": "object", "properties": {
                      "content": { "type": "string" } }, "required": ["content"] } },
            ] })),
            "tools/call" => {
                let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                self.mcp_tool(name, &args, now)
            }
            other => Err(format!("unknown method: {other}")),
        };

        // No `id` ⇒ a notification: no response.
        id.map(|id| match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(e) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": e } })
            }
        })
    }

    /// Execute one MCP tool by name. Returns an MCP `content` result or an error string.
    fn mcp_tool(
        &mut self,
        name: &str,
        args: &serde_json::Value,
        now: i64,
    ) -> Result<serde_json::Value, String> {
        use serde_json::json;
        let text = |s: String| json!({ "content": [{ "type": "text", "text": s }] });
        match name {
            "memory.recall" => {
                let q = args.get("query").and_then(|q| q.as_str()).unwrap_or("");
                let k = args.get("k").and_then(|k| k.as_u64()).unwrap_or(5) as usize;
                let hits = self.recall(q, k, now).map_err(|e| e.to_string())?;
                Ok(text(
                    serde_json::to_string(&hits).unwrap_or_else(|_| "[]".into()),
                ))
            }
            "memory.capture" => {
                let content = args
                    .get("content")
                    .and_then(|c| c.as_str())
                    .ok_or("missing `content`")?;
                // Optional `scope` (e.g. "app:<id>"); root when omitted.
                let scope = args
                    .get("scope")
                    .and_then(|s| s.as_str())
                    .map(Scope::parse)
                    .unwrap_or_else(Scope::root);
                self.capture(content, scope, now)
                    .map_err(|e| e.to_string())?;
                Ok(text("captured".into()))
            }
            other => Err(format!("unknown tool: {other}")),
        }
    }

    /// Recall up to `k` memories relevant to `query` (hybrid FTS+vector+graph, salience-reranked).
    pub fn recall(&self, query: &str, k: usize, now: i64) -> anyhow::Result<Vec<RecalledMemory>> {
        let budget = k.saturating_mul(64).max(64);
        let recalled = self
            .engine
            .recall(query, &Scope::root(), &[], budget, now)
            .map_err(|e| anyhow::anyhow!("recall memory: {e}"))?;
        Ok(recalled
            .into_iter()
            .take(k)
            .map(|r| RecalledMemory {
                content: r.content,
                score: r.score,
                tier: format!("{:?}", r.tier),
            })
            .collect())
    }
}

/// Choose the embedder, enforcing CONSISTENCY across opens. The realistic default is real semantic
/// embeddings (Model2Vec — static, fast, small model cached on first use); it falls back to the
/// dependency-free lexical `HashEmbedder` only on a FRESH store (offline / `WICKED_MEMORY_EMBEDDER=hash`).
///
/// The embedder is fixed per store: a store's vectors are embedded with one model, and querying with a
/// different one silently mismatches dimensions → recall returns nothing. So we record the choice in a
/// sidecar marker (`<mem>.embedder`) on first open and HONOR it thereafter — if a store was built with
/// Model2Vec but the model can't load now, we FAIL LOUD (the actor then runs without recall) rather
/// than silently switch to Hash and return empty results forever.
fn choose_embedder(engine: MemoryEngine, mem_path: &str) -> anyhow::Result<MemoryEngine> {
    let marker = format!("{mem_path}.embedder");
    let recorded = std::fs::read_to_string(&marker)
        .ok()
        .map(|s| s.trim().to_string());
    let forced_hash = std::env::var("WICKED_MEMORY_EMBEDDER").as_deref() == Ok("hash");

    // A store that already recorded "hash" stays lexical; otherwise we want the semantic embedder
    // (unless this is a fresh store and hash is forced).
    let want_semantic = match recorded.as_deref() {
        Some("model2vec") => true,
        Some("hash") => false,
        _ => !forced_hash, // fresh store: semantic unless forced to hash
    };

    if want_semantic {
        match wicked_estate_retrieve::Model2VecEmbedder::new() {
            Ok(e) => {
                let _ = std::fs::write(&marker, "model2vec");
                return Ok(engine.with_embedder(Box::new(e)));
            }
            Err(err) if recorded.as_deref() == Some("model2vec") => {
                // The store's memories ARE Model2Vec-embedded but the model is unavailable now —
                // falling back to Hash would silently break recall. Refuse.
                anyhow::bail!(
                    "memory store at {mem_path} was built with the Model2Vec embedder, which is \
                     unavailable now ({err}); recall would silently return nothing. Restore the model, \
                     or start a fresh store with WICKED_MEMORY_EMBEDDER=hash."
                );
            }
            Err(err) => {
                eprintln!(
                    "wicked-core: semantic embedder unavailable ({err}); using lexical recall"
                );
            }
        }
    }
    // Lexical path (fresh store + forced/fallback hash, or a store recorded as hash).
    let _ = std::fs::write(&marker, "hash");
    Ok(engine) // already on the default HashEmbedder
}

/// The memory db path derived from the estate db path.
fn mem_path_for(estate_path: &str) -> String {
    format!("{estate_path}.mem")
}

/// Wall-clock now in unix seconds — the memory store's time base.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
