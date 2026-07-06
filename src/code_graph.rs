//! CODE GRAPH — operate GRAPH-NATIVELY on a repo's code/domain graph, the wicked-estate substrate the
//! whole methodology spine (recon → review → test), memory cross-edges, and routing are built on.
//!
//! ARCHITECTURE: indexing (the heavy 150+ tree-sitter language extractors) is delegated to the
//! `wicked-estate` indexer as a SUBPROCESS, so the grammars stay OUT of this engine/UI binary. The
//! engine then READS + RANKS the resulting graph with the lean `estate-core` (GraphRead) +
//! `estate-rank` (PageRank) crates it already links. Indexing is a build step; operating on the graph
//! is the runtime — keeping them separate is what lets us be graph-native without bloat.

use std::path::Path;
use std::process::Command;

use std::collections::HashSet;

use wicked_apps_core::{open_store, GraphRead, NodeKind};
use wicked_estate_core::{Direction, SymbolId};

/// CALL-SPREAD — the number of DISTINCT files that reference `id`. A LANGUAGE-AGNOSTIC, data-driven
/// ubiquity signal: generic utilities (`as_str`, `default`, `new`, `map`, `join`) are called from a
/// large FRACTION of all files, so PageRank over-ranks them; domain symbols (`recall`, `base_dir`,
/// `from_node`) are called from a few. No hardcoded word list — measured from the parsed edges. (We
/// use spread, not raw in-degree, because shallow indexes collapse out-degree but keep edge files.)
fn caller_spread<S: GraphRead>(store: &S, id: &SymbolId) -> usize {
    store
        .neighbors(id, Direction::Dependents)
        .unwrap_or_default()
        .iter()
        .filter_map(|e| e.location.as_ref().map(|l| l.file.clone()))
        .filter(|f| !f.is_empty())
        .collect::<HashSet<_>>()
        .len()
}

/// A ranked code symbol — the orchestrator's recon view of a repo (PageRank centrality over the
/// CALLS/IMPORTS graph). `score_pct` is relative to the top symbol (100 = most central).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RankedSymbol {
    pub name: String,
    pub file: String,
    pub kind: String,
    pub score_pct: u8,
}

/// Definition kinds that can be a hotspot — substantive, human-authored declarations. Excludes
/// imports/files/modules/namespaces (structural) AND fields/constants/variables/parameters (data,
/// not "load-bearing code") + synthetic nodes. (Mirrors command_iq's function/method/class/interface
/// type filter, generalized.)
fn is_def_kind(k: &NodeKind) -> bool {
    matches!(
        k,
        NodeKind::Class
            | NodeKind::Struct
            | NodeKind::Enum
            | NodeKind::Interface
            | NodeKind::Trait
            | NodeKind::Function
            | NodeKind::Method
            | NodeKind::Constructor
            | NodeKind::TypeAlias
            | NodeKind::Macro
    )
}

/// Files that "sit outside" the human-authored source — tests, generated code, vendored deps. A
/// hotspot must come from the original source, so these are excluded (language-agnostic path match).
fn is_excluded_path(file: &str) -> bool {
    let f = file.to_lowercase();
    const PATS: &[&str] = &[
        "/test/", "/tests/", "/__tests__/", "/spec/", "/specs/", ".test.", ".spec.", "_test.",
        "/test_", "/node_modules/", "/vendor/", "/third_party/", "/dist/", "/build/", "/.next/",
        "/__generated__/", "/generated/", "/migrations/", "/target/", ".min.", ".bundle.", "_pb.",
        ".generated.", ".g.dart", "/gen/",
    ];
    PATS.iter().any(|p| f.contains(p))
}

/// A NodeKind's display label (e.g. "Function", or the inner string for `Other`).
fn kind_str(k: &NodeKind) -> String {
    match k {
        NodeKind::Other(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

/// Resolve the `wicked-estate` indexer binary: `$WICKED_ESTATE_BIN`, then `~/.cargo/bin`, else the
/// bare name (PATH lookup).
pub(crate) fn indexer_bin() -> String {
    if let Ok(b) = std::env::var("WICKED_ESTATE_BIN") {
        if !b.is_empty() {
            return b;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = Path::new(&home).join(".cargo/bin/wicked-estate");
        if p.exists() {
            return p.display().to_string();
        }
    }
    "wicked-estate".to_string()
}

/// Index `repo` into a code graph at `<repo>/.wicked/code-graph.db` via the wicked-estate indexer
/// subprocess. Returns the graph db path.
pub fn index_repo(repo: &Path) -> anyhow::Result<String> {
    let graph = repo.join(".wicked").join("code-graph.db");
    if let Some(parent) = graph.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let graph_str = graph.to_string_lossy().to_string();
    let bin = indexer_bin();
    let out = Command::new(&bin)
        .arg("index")
        .arg(repo)
        .arg("--db")
        .arg(&graph_str)
        .output()
        .map_err(|e| {
            anyhow::anyhow!(
                "could not run the `{bin}` indexer ({e}); install it (cargo install wicked-estate) \
                 or set WICKED_ESTATE_BIN to enable code-graph recon/ranking"
            )
        })?;
    if !out.status.success() {
        anyhow::bail!(
            "code indexer failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(graph_str)
}

/// Rank the top-`n` most central code symbols in an indexed graph (global PageRank over CALLS/IMPORTS).
pub fn rank_symbols(graph_path: &str, n: usize) -> anyhow::Result<Vec<RankedSymbol>> {
    let store = open_store(Some(graph_path))
        .map_err(|e| anyhow::anyhow!("open code graph {graph_path}: {e}"))?;
    // Total distinct source files — the denominator for the call-spread ubiquity test.
    let total_files = store
        .all_nodes()
        .map(|ns| {
            ns.iter()
                .map(|n| n.location.file.clone())
                .filter(|f| !f.is_empty())
                .collect::<HashSet<_>>()
                .len()
        })
        .unwrap_or(0)
        .max(1);
    // Scale-adaptive ubiquity cutoff: a symbol referenced from this FRACTION of files (or more) is a
    // generic/library utility. The fraction shrinks as the repo grows (in a 3000-file repo even a
    // ubiquitous symbol touches a smaller % of files than in a 20-file one) — fit 0.45/ln(files).
    let ubiq_frac = 0.45 / (total_files as f32).max(3.0).ln();
    // Over-fetch generously (filters are aggressive), then keep only MEANINGFUL source definitions.
    let ranked = wicked_estate_rank::ranked_symbols(&store, &[], n.saturating_mul(12).max(96))
        .map_err(|e| anyhow::anyhow!("rank code graph: {e}"))?;
    let mut out: Vec<(RankedSymbol, f32)> = ranked
        .into_iter()
        .filter_map(|(id, score)| {
            let node = store.get_node(&id).ok().flatten()?;
            // (1) definition kinds only; (2) from the original source (not tests/generated/vendor).
            if !is_def_kind(&node.kind) || is_excluded_path(&node.location.file) {
                return None;
            }
            // (3) ubiquity: referenced from ≥ the adaptive fraction of files (≥4 absolute) ⇒ generic
            // built-in / common-lib, not a domain hotspot.
            let spread = caller_spread(&store, &id);
            if total_files >= 6 && spread >= 4 && (spread as f32 / total_files as f32) >= ubiq_frac {
                return None;
            }
            Some((
                RankedSymbol {
                    name: node.name,
                    file: node.location.file,
                    kind: kind_str(&node.kind),
                    score_pct: 0,
                },
                score,
            ))
        })
        .collect();
    let top = out.first().map(|(_, s)| *s).unwrap_or(1.0).max(1e-9);
    for (sym, score) in &mut out {
        sym.score_pct = ((*score / top) * 100.0).round().clamp(0.0, 100.0) as u8;
    }
    out.truncate(n);
    Ok(out.into_iter().map(|(s, _)| s).collect())
}

/// Recon a repo end-to-end: index it, then return its `n` most central symbols. This is the
/// graph-native recon view — "what matters in this codebase" — fed to the CLIs + shown in the UI.
pub fn recon_repo(repo: &Path, n: usize) -> anyhow::Result<Vec<RankedSymbol>> {
    let graph = index_repo(repo)?;
    rank_symbols(&graph, n)
}
