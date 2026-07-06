//! RUN KNOWLEDGE — a document knowledge base over `wicked_knowledge_mcp::KnowledgeEngine`. Distinct
//! from [`crate::memory`] (episodic run outcomes): this is a Doc→Chunk store you INGEST reference
//! material into (design notes, ADRs, external docs) and RECALL relevant chunks from. A SEPARATE
//! single-writer store, sibling of the estate db. The actor owns ONE, best-effort (`Option`).
//!
//! NOTE: the engine recalls with its default lexical embedder (no `with_embedder` is exposed), so
//! knowledge recall is keyword-based for now — distinct from memory's semantic (Model2Vec) recall.

use wicked_knowledge_mcp::KnowledgeEngine;

/// A recalled knowledge chunk, flattened for the Core API.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RecalledKnowledge {
    pub content: String,
    pub score: f64,
    pub source: String,
}

/// The orchestrator's knowledge base.
pub struct RunKnowledge {
    engine: KnowledgeEngine,
}

impl RunKnowledge {
    /// Open the knowledge store as a sibling of the estate db (`<estate>.knowledge`).
    pub fn open(estate_path: &str) -> anyhow::Result<Self> {
        let path = format!("{estate_path}.knowledge");
        let engine = KnowledgeEngine::open(&path)
            .map_err(|e| anyhow::anyhow!("open knowledge store at {path}: {e}"))?;
        Ok(Self { engine })
    }

    /// Ingest a document (title + chunks) into the knowledge base. Returns the chunk count.
    pub fn ingest(&mut self, title: &str, chunks: &[String], now: i64) -> anyhow::Result<usize> {
        let (_doc, chunk_syms) = self
            .engine
            .ingest(title, chunks, "wicked", "orchestrator", now)
            .map_err(|e| anyhow::anyhow!("ingest knowledge: {e}"))?;
        Ok(chunk_syms.len())
    }

    /// Recall up to `k` knowledge chunks relevant to `query`.
    pub fn recall(
        &mut self,
        query: &str,
        k: usize,
        now: i64,
    ) -> anyhow::Result<Vec<RecalledKnowledge>> {
        let budget = k.saturating_mul(128).max(128);
        let hits = self
            .engine
            .recall(query, budget, now)
            .map_err(|e| anyhow::anyhow!("recall knowledge: {e}"))?;
        Ok(hits
            .into_iter()
            .take(k)
            .map(|h| RecalledKnowledge {
                content: h.content,
                score: h.score,
                source: h.source,
            })
            .collect())
    }
}
