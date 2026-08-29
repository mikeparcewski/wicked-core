//! The `knowledge.relate_code` half of the relink pass (AW-9 / arch-R6, second clause).
//!
//! When the rule's source doc has been ingested into the knowledge domain (garden mem-ingest with
//! source = the doc's repo-relative path or URI — arch-R5), the relink pass links those knowledge
//! chunks to the SAME resolved code symbols, so a gate denial's obligation leads back to the wiki
//! rationale and `answer` can name the enforcing rule. The write is the exact seam the estate MCP's
//! `knowledge.relate_code` tool uses: an `about` cross-edge in the xedge overlay
//! (`knowledge node → estate code symbol`), target-epoch-stamped, read back by
//! `knowledge.recall_about_code` (which filters on the TARGET endpoint + rel only).
//!
//! This half runs ONLY where the seam exists: both a knowledge store and an xedge overlay must be
//! supplied (CLI `--knowledge`/`--xedge`, or `WICKED_KNOWLEDGE_DB`/`WICKED_XEDGE_DB`). Nothing
//! here is required for the graph half — a repo without a knowledge store still relinks fully; the
//! report says the knowledge half was skipped and why. Chunks are matched by their `source`
//! metadata equalling the rule's doc path (or the full provenance ref) VERBATIM — no fuzzy
//! matching: a wrong back-link is worse than a reported miss.

use std::collections::BTreeMap;

use serde::Serialize;
use wicked_estate_knowledge::{KClass, KnowledgeEngine, XEdge, XedgeStore};
use wicked_estate_overlay::Endpoint;

use crate::relink::LinkedRule;

/// One written knowledge→code link.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KnowledgeLink {
    pub rule_id: String,
    pub knowledge_id: String,
    pub code_symbol: String,
}

/// The knowledge half's outcome, embedded in the relink report.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct KnowledgeLinkReport {
    pub links_written: Vec<KnowledgeLink>,
    /// Linked rules whose doc path matched NO knowledge node — the doc was never ingested into
    /// the knowledge domain (arch-R5's mem-ingest step), reported so the miss is visible.
    pub unmatched_docs: Vec<String>,
}

/// Relate every linked rule's knowledge chunks to its resolved code symbols. `linked` is the graph
/// half's output ([`crate::relink::relink`]); matching is by knowledge-node `source` == the rule's
/// doc path. Idempotent: the xedge overlay upserts on its full primary key.
pub fn relate_linked_rules(
    knowledge: &KnowledgeEngine,
    xedge: &XedgeStore,
    linked: &[LinkedRule],
) -> anyhow::Result<KnowledgeLinkReport> {
    let mut report = KnowledgeLinkReport::default();
    if linked.is_empty() {
        return Ok(report);
    }

    // Index knowledge nodes by their `source` (one scan, not one per rule). Chunks are the unit
    // recall returns, so they are preferred; the kdoc is the fallback for a source ingested with
    // no chunk bodies (linking BOTH would double every back-link — `ingest` stamps the same
    // source on the doc and each of its chunks).
    let mut chunks_by_source: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut docs_by_source: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (class, map) in [
        (KClass::Chunk, &mut chunks_by_source),
        (KClass::Doc, &mut docs_by_source),
    ] {
        for k in knowledge
            .all_nodes(Some(class))
            .map_err(|e| anyhow::anyhow!("knowledge store scan failed: {e}"))?
        {
            map.entry(k.source.clone())
                .or_default()
                .push(k.symbol().as_str().to_string());
        }
    }

    for rule in linked {
        let Some(doc_path) = rule.doc_path.as_deref() else {
            continue;
        };
        let Some(kids) = chunks_by_source
            .get(doc_path)
            .or_else(|| docs_by_source.get(doc_path))
        else {
            report.unmatched_docs.push(doc_path.to_string());
            continue;
        };
        for kid in kids {
            for target in &rule.targets {
                // The SAME shape the MCP's knowledge.relate_code writes, with two deliberate
                // upgrades: the source endpoint is tagged with its REAL engine ("knowledge", not
                // the tool's legacy "memory" tag — readers filter on the target endpoint + rel, so
                // both are found), and the target carries the epoch the graph half resolved at
                // (DEC-X6-SEQ: an epoch-stamped endpoint can fail-closed on reuse-after-delete).
                xedge
                    .put_edge(&XEdge {
                        source: Endpoint::new("knowledge", kid.clone(), 0),
                        target: Endpoint::new(
                            "estate",
                            target.symbol.clone(),
                            target.epoch.unwrap_or(0),
                        ),
                        rel: "about".to_string(),
                        confidence: 1.0,
                        provenance: "wicked-core rules relink".to_string(),
                    })
                    .map_err(|e| anyhow::anyhow!("xedge write failed: {e}"))?;
                report.links_written.push(KnowledgeLink {
                    rule_id: rule.rule_id.clone(),
                    knowledge_id: kid.clone(),
                    code_symbol: target.symbol.clone(),
                });
            }
        }
    }
    report.unmatched_docs.sort();
    report.unmatched_docs.dedup();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relink::LinkedTarget;

    fn linked_rule(rule_id: &str, doc_path: Option<&str>, symbols: &[&str]) -> LinkedRule {
        LinkedRule {
            rule_id: rule_id.to_string(),
            symbol_ref: "src/a.rs::f".to_string(),
            doc_path: doc_path.map(str::to_string),
            targets: symbols
                .iter()
                .map(|s| LinkedTarget {
                    symbol: s.to_string(),
                    epoch: Some(0),
                })
                .collect(),
        }
    }

    #[test]
    fn matched_chunks_get_about_xedges_to_the_same_resolved_symbols() {
        let mut knowledge = KnowledgeEngine::in_memory().unwrap();
        let (_, chunk_syms) = knowledge
            .ingest(
                "Agent behavior rules",
                &["Never use printf without %s.".to_string()],
                "wiki:architecture",
                "docs/agent-behavior.md",
                1_000,
            )
            .unwrap();
        let xedge = XedgeStore::in_memory().unwrap();

        let linked = vec![
            linked_rule("PAT-001", Some("docs/agent-behavior.md"), &["sim code `f`"]),
            linked_rule("PAT-002", Some("docs/never-ingested.md"), &["sim code `g`"]),
        ];
        let report = relate_linked_rules(&knowledge, &xedge, &linked).unwrap();

        // The ingested doc's chunk linked; the never-ingested doc reported, never guessed.
        assert_eq!(report.links_written.len(), 1);
        assert_eq!(report.links_written[0].rule_id, "PAT-001");
        assert_eq!(
            report.links_written[0].knowledge_id,
            chunk_syms[0].as_str().to_string()
        );
        assert_eq!(
            report.unmatched_docs,
            vec!["docs/never-ingested.md".to_string()]
        );

        // Read back through the SAME query knowledge.recall_about_code uses: in-edges on the
        // estate target endpoint with rel "about".
        let reader = xedge.reader();
        let edges = reader
            .in_edges("estate", "sim code `f`", &["about"])
            .unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(
            edges[0].source.stable_id,
            chunk_syms[0].as_str().to_string()
        );
        assert_eq!(edges[0].source.engine, "knowledge");

        // Idempotent: a second relate pass upserts, never duplicates.
        relate_linked_rules(&knowledge, &xedge, &linked).unwrap();
        assert_eq!(
            xedge.len().unwrap(),
            1,
            "the xedge overlay upserts on its full primary key"
        );
    }
}
