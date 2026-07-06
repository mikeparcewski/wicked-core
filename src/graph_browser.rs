//! GRAPH BROWSER — read-only exploration of ANY wicked-estate graph. Every wicked store is an estate
//! graph, so one browser reads them all: a repo's CODE graph, the orchestrator's own session/unit
//! DOMAIN graph, and the memory + knowledge graphs. Surfaces node kinds + counts, a searchable /
//! kind-filtered node list, and per-node detail with its neighbor edges (navigable both ways).

use std::collections::BTreeMap;

use wicked_apps_core::{open_store, GraphRead, Node, NodeKind};
use wicked_estate_core::{Direction, SymbolId};

/// A node, flattened for the Core/UI (egui-free, serde-friendly).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct NodeSummary {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
}

/// One neighbor of a node, with the edge relation + direction.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct NeighborEdge {
    pub rel: String,
    /// "→" (this node → neighbor, a dependency) or "←" (neighbor → this node, a dependent).
    pub dir: String,
    pub node: NodeSummary,
}

/// A semantic annotation on a node (e.g. from CLI enrichment — `author` is the CLI key).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SymbolAnnotation {
    pub key: String,
    pub value: String,
    pub author: String,
}

/// Full detail for one node.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct NodeDetail {
    pub node: NodeSummary,
    pub signature: Option<String>,
    pub doc: Option<String>,
    pub metadata: Vec<(String, String)>,
    pub annotations: Vec<SymbolAnnotation>,
    pub neighbors: Vec<NeighborEdge>,
}

fn kind_label(k: &NodeKind) -> String {
    match k {
        NodeKind::Other(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn summarize(n: &Node) -> NodeSummary {
    NodeSummary {
        id: n.symbol.as_str().to_string(),
        name: n.name.clone(),
        kind: kind_label(&n.kind),
        file: n.location.file.clone(),
    }
}

/// A note tied to a graph node (a manual `note` annotation), for the Notes tab.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct NodeNote {
    pub node_id: String,
    pub node_name: String,
    pub file: String,
    pub note: String,
    pub author: String,
}

/// All notes tied to nodes in a graph (manual `note` annotations), newest first.
pub fn list_node_notes(db: &str) -> anyhow::Result<Vec<NodeNote>> {
    let store = open_store(Some(db)).map_err(|e| anyhow::anyhow!("open graph {db}: {e}"))?;
    let mut out = Vec::new();
    for (sym, ann) in store.annotations_by_type("note").unwrap_or_default() {
        if let Ok(Some(n)) = store.get_node(&sym) {
            out.push(NodeNote {
                node_id: sym.as_str().to_string(),
                node_name: n.name,
                file: n.location.file,
                note: ann.value,
                author: ann.author,
            });
        }
    }
    out.reverse();
    Ok(out)
}

/// The node kinds present in a graph + their counts (the graph's shape).
pub fn graph_kinds(db: &str) -> anyhow::Result<Vec<(String, usize)>> {
    let store = open_store(Some(db)).map_err(|e| anyhow::anyhow!("open graph {db}: {e}"))?;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for n in store.all_nodes()? {
        *counts.entry(kind_label(&n.kind)).or_default() += 1;
    }
    Ok(counts.into_iter().collect())
}

/// Browse nodes — optional `kind` filter + case-insensitive `search` over name/file — capped at `limit`.
pub fn browse_nodes(
    db: &str,
    kind: Option<&str>,
    search: &str,
    limit: usize,
) -> anyhow::Result<Vec<NodeSummary>> {
    let store = open_store(Some(db)).map_err(|e| anyhow::anyhow!("open graph {db}: {e}"))?;
    let q = search.to_lowercase();
    let mut out: Vec<NodeSummary> = store
        .all_nodes()?
        .iter()
        .filter(|n| kind.is_none_or(|k| kind_label(&n.kind) == k))
        .filter(|n| {
            q.is_empty()
                || n.name.to_lowercase().contains(&q)
                || n.location.file.to_lowercase().contains(&q)
        })
        .map(summarize)
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.file.cmp(&b.file)));
    out.truncate(limit);
    Ok(out)
}

/// Full detail for one node by id: signature/doc/metadata + neighbor edges (dependencies + dependents).
pub fn node_detail(db: &str, id: &str) -> anyhow::Result<Option<NodeDetail>> {
    let store = open_store(Some(db)).map_err(|e| anyhow::anyhow!("open graph {db}: {e}"))?;
    let sym = SymbolId(id.to_string());
    let Some(n) = store.get_node(&sym)? else {
        return Ok(None);
    };

    let mut neighbors = Vec::new();
    for (dir, arrow, outgoing) in [
        (Direction::Dependencies, "→", true),
        (Direction::Dependents, "←", false),
    ] {
        for e in store.neighbors(&sym, dir).unwrap_or_default() {
            let other = if outgoing { &e.target } else { &e.source };
            if let Ok(Some(on)) = store.get_node(other) {
                neighbors.push(NeighborEdge {
                    rel: format!("{:?}", e.kind),
                    dir: arrow.to_string(),
                    node: summarize(&on),
                });
            }
        }
    }

    let metadata = n
        .metadata
        .iter()
        .take(24)
        .map(|(k, v)| {
            let s = v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string());
            (k.clone(), s.chars().take(200).collect::<String>())
        })
        .collect();

    // Semantic annotations (CLI enrichment etc.) — read via the GraphRead seam, newest first.
    let mut annotations: Vec<SymbolAnnotation> = store
        .annotations(&sym)
        .unwrap_or_default()
        .into_iter()
        .map(|a| SymbolAnnotation {
            key: a.key,
            value: a.value,
            author: a.author,
        })
        .collect();
    annotations.reverse();

    Ok(Some(NodeDetail {
        node: summarize(&n),
        signature: n.signature.clone(),
        doc: n.doc.clone(),
        metadata,
        annotations,
        neighbors,
    }))
}
