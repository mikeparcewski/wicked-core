//! MarkdownAdapter over the committed fixtures (AW-3 / arch-R1): a frontmattered doc corpus
//! ingests to native `Rule` nodes through the ONE parse path (`normalize_bundle`), and a
//! malformed file fails LOUD per-file with path + reason.

use std::path::PathBuf;

use wicked_apps_core::{GraphRead, NodeKind, SqliteStore};
use wicked_governance::{
    ingest_from, recall_rules, register_rule, register_schema_nodes, ConfSeverity, MarkdownAdapter,
    RuleQuery,
};

/// Redirect the emit outbox spool to a per-process temp file: `register_rule`'s fire-and-forget
/// `wicked.estate.rule.ingested` emission (AW-22) is a side effect here, not the object under
/// test, and must not append junk to the real `~/.something-wicked` operator replay queue.
/// Set once, never unset (an unset window would leak a parallel test's emission).
fn hermetic_spool() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let path = std::env::temp_dir().join(format!(
            "wg-mdingest-test-outbox-{}.ndjson",
            std::process::id()
        ));
        // SAFETY: process-global env write, serialized by `Once`, never removed.
        unsafe { std::env::set_var(wicked_apps_core::emit::DEADLETTER_ENV, &path) };
    });
}

fn fixture(dir: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
}

#[test]
fn fixture_corpus_ingests_to_rule_nodes_end_to_end() {
    hermetic_spool();
    let adapter = MarkdownAdapter::new(fixture("markdown"));
    let rules = ingest_from(&adapter).expect("well-formed corpus ingests");
    // agent-behavior.md (2) + nested/event-grammar.md (1); doc-only.md mints zero rules and the
    // fence-less nested/README.md is not claimed.
    assert_eq!(
        rules.len(),
        3,
        "recursive discovery, doc-only + README excluded"
    );

    let mut store = SqliteStore::in_memory().unwrap();
    for r in &rules {
        register_rule(&mut store, r).expect("adapter output passes the fail-closed invariants");
    }
    register_schema_nodes(&mut store).expect("schema-document nodes registered");

    // The frontmattered docs became native Rule nodes…
    let query = wicked_estate_core::SymbolQuery {
        kinds: vec![NodeKind::Rule],
        ..Default::default()
    };
    assert_eq!(store.find_symbols(&query).unwrap().len(), 3);

    // …recallable severity-first with the doc's facets and provenance intact.
    let recalled = recall_rules(&store, &RuleQuery::default()).unwrap();
    assert_eq!(recalled.len(), 3);
    assert_eq!(recalled[0].id, "POL-002", "critical first");
    assert_eq!(recalled[0].severity, ConfSeverity::Critical);
    let pat = recalled.iter().find(|r| r.id == "PAT-001").unwrap();
    assert_eq!(pat.targets.language.as_deref(), Some("rust"));
    assert_eq!(pat.provenance.source, "markdown");
    assert_eq!(
        pat.provenance.reference.as_deref(),
        Some("agent-behavior.md#PAT-001")
    );
    let pol100 = recalled.iter().find(|r| r.id == "POL-100").unwrap();
    assert_eq!(
        pol100.provenance.reference.as_deref(),
        Some("nested/event-grammar.md#POL-100"),
        "nested docs keep root-relative, forward-slash refs"
    );
}

#[test]
fn malformed_fixture_fails_loud_with_path_and_reason() {
    let adapter = MarkdownAdapter::new(fixture("markdown-malformed"));
    let err = ingest_from(&adapter).unwrap_err().to_string();
    assert!(
        err.contains("bad-frontmatter.md"),
        "the failing FILE is named: {err}"
    );
    assert!(
        err.contains("key: value"),
        "the reason is named, never a silent skip: {err}"
    );
}
