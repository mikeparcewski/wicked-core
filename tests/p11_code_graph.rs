//! P11 CODE GRAPH — graph-native recon: index a REAL repo via the wicked-estate indexer subprocess,
//! then PageRank its code graph for the most central symbols. This is the substrate the methodology
//! spine + memory cross-edges are built on. Skips cleanly if the indexer binary isn't available.

use std::sync::Mutex;

/// Both tests recon the SAME `src` to the same `.wicked/code-graph.db`, so they must not index it
/// concurrently.
static INDEX_GUARD: Mutex<()> = Mutex::new(());

fn indexer() -> Option<String> {
    for cand in [
        "/tmp/wicked-tools/bin/wicked-estate",
        // also honor an installed one
    ] {
        if std::path::Path::new(cand).exists() {
            return Some(cand.to_string());
        }
    }
    None
}

#[test]
fn recon_indexes_and_ranks_a_real_codebase() {
    let Some(bin) = indexer() else {
        eprintln!("skipping p11: wicked-estate indexer not built (cargo install wicked-estate)");
        return;
    };
    let _g = INDEX_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("WICKED_ESTATE_BIN", &bin);

    // Index this crate's OWN src as a small real Rust codebase.
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let symbols = wicked_core::recon_repo(&src, 12).expect("recon a real repo");

    assert!(
        !symbols.is_empty(),
        "graph-native recon surfaces ranked code symbols from a real repo"
    );
    assert_eq!(
        symbols[0].score_pct, 100,
        "the top symbol is the PageRank centrality leader (100% relative)"
    );
    assert!(
        symbols
            .iter()
            .all(|s| !s.name.is_empty() && !s.file.is_empty()),
        "every ranked symbol resolves to a real name + file, got: {symbols:?}"
    );
    // Scores are monotonically non-increasing (ranked).
    assert!(
        symbols.windows(2).all(|w| w[0].score_pct >= w[1].score_pct),
        "symbols are returned in descending centrality order"
    );
    eprintln!("recon top symbols: {symbols:?}");
}

#[test]
fn browse_and_node_detail_on_a_real_graph() {
    let Some(bin) = indexer() else {
        eprintln!("skipping p11 browse: indexer not built");
        return;
    };
    let _g = INDEX_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("WICKED_ESTATE_BIN", &bin);
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let _ = wicked_core::recon_repo(&src, 5).expect("recon indexes the graph");
    let graph = src.join(".wicked/code-graph.db");
    let graph = graph.to_str().unwrap();

    // The graph has node kinds (functions, etc.).
    let kinds = wicked_core::graph_kinds(graph).expect("kinds");
    assert!(!kinds.is_empty(), "the graph reports node kinds + counts");

    // Browse by name search.
    let hits = wicked_core::browse_nodes(graph, None, "recall", 20).expect("browse");
    let recall = hits
        .iter()
        .find(|n| n.name == "recall")
        .unwrap_or_else(|| panic!("browse finds 'recall', got: {hits:?}"));

    // Node detail resolves the node + its neighbor edges.
    let detail = wicked_core::node_detail(graph, &recall.id)
        .expect("detail ok")
        .expect("node present");
    assert_eq!(detail.node.name, "recall");
    eprintln!(
        "kinds={:?} | 'recall' neighbors={}",
        kinds,
        detail.neighbors.len()
    );
}
