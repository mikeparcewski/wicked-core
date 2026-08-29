//! The ONE ADR frontmatter contract (AW-12 / arch-R12; estate ADR-011 §adr-contract) parses
//! under the MarkdownAdapter with no second parse path:
//!
//! ```yaml
//! id: <repo>-adr-<number>   # required
//! title: <title>            # required
//! status: active            # active | draft | superseded | retired
//! date: YYYY-MM-DD          # required by the contract (optional to the adapter)
//! supersedes: [ids]         # optional — FULL supersession only
//! applies_to: [scopes]      # optional
//! ```
//!
//! These fixtures mirror the normalized ADR corpora (estate `docs/adr/ADR-001..012`, garden
//! `docs/adr/0001..0007`, interactive `docs/adr/0001..0027`): a doc-only ADR ingests with zero
//! rules and no error; a rule-bearing ADR (estate ADR-011 is the exemplar) mints Rule nodes;
//! a superseded ADR's rules mint `retired = true` and carry the supersession lineage.

use std::path::PathBuf;

use wicked_governance::{ingest_from, MarkdownAdapter, SourceAdapter};

/// Write `files` under a fresh per-test temp dir; returns the dir.
fn dir_with(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wicked-gov-adr-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (file, content) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    dir
}

/// A doc-only ADR under the contract — the shape of every normalized historical ADR.
const ADR_DOC_ONLY: &str = "---\n\
id: wicked-estate-adr-001\n\
title: Graph Schema v2\n\
status: active\n\
date: 2026-06-12\n\
---\n\n\
# ADR-001 — Graph Schema v2\n\n\
**Status:** Accepted · **Date:** 2026-06-12\n\n\
## Context\n\nBody prose is untouched by normalization.\n";

/// A rule-bearing ADR under the contract — the estate ADR-011 shape (frontmatter + `## Rules`).
const ADR_WITH_RULES: &str = "---\n\
id: wicked-estate-adr-011\n\
title: Graph-backed architecture wiki on the estate\n\
status: active\n\
date: 2026-08-29\n\
applies_to: [wicked-estate, wicked-core, wicked-garden, wicked-crew]\n\
scope: wiki:architecture\n\
domain: architecture-wiki\n\
---\n\n\
# ADR-011 — Graph-backed architecture wiki\n\nProse.\n\n\
## Rules\n\n\
- `POL-1101` (critical): Promotion to an enforceable Rule happens only via a\n  \
  human-merged doc PR; there is no rules.write MCP tool.\n\
- `PAT-1102` (error): An edge whose target is a code-graph symbol uses the native\n  \
  Governs edge kind; the stringly spelling is knowledge-store-only.\n";

/// A superseded ADR under the contract — rules withdraw from recall, lineage rides as metadata.
const ADR_SUPERSEDED: &str = "---\n\
id: wicked-garden-adr-0004\n\
title: Move the code-relationship graph to wicked-brain\n\
status: superseded\n\
date: 2026-06-10\n\
supersedes: [wicked-garden-adr-0001]\n\
---\n\n\
# ADR 0004\n\nBody.\n\n\
## Rules\n\n\
- `PAT-9904` (warn): Historical statement kept for lineage.\n";

#[test]
fn contract_adrs_parse_and_doc_only_ingest_is_valid() {
    let dir = dir_with(
        "corpus",
        &[
            ("ADR-001-graph-schema.md", ADR_DOC_ONLY),
            ("ADR-011-architecture-wiki.md", ADR_WITH_RULES),
            ("0004-code-graph-moves-to-wicked-brain.md", ADR_SUPERSEDED),
        ],
    );
    let adapter = MarkdownAdapter::new(&dir);

    // Every contract field round-trips as doc metadata.
    let docs = adapter.fetch().expect("all three contract shapes parse");
    assert_eq!(docs.len(), 3);
    let doc_only = docs
        .iter()
        .find(|d| d["doc"]["id"] == "wicked-estate-adr-001")
        .unwrap();
    assert_eq!(doc_only["doc"]["title"], "Graph Schema v2");
    assert_eq!(doc_only["doc"]["status"], "active");
    assert_eq!(doc_only["doc"]["date"], "2026-06-12");
    assert_eq!(doc_only["rules"].as_array().unwrap().len(), 0);

    let superseded = docs
        .iter()
        .find(|d| d["doc"]["id"] == "wicked-garden-adr-0004")
        .unwrap();
    assert_eq!(
        superseded["doc"]["supersedes"][0], "wicked-garden-adr-0001",
        "supersession lineage rides the doc metadata (SUPERSEDES edge material)"
    );

    // The same corpus materializes through the ONE normalize_bundle path.
    let rules = ingest_from(&adapter).expect("contract corpus ingests");
    assert_eq!(rules.len(), 3, "doc-only ADR contributes zero rules");
    let pol = rules.iter().find(|r| r.id == "POL-1101").unwrap();
    assert!(!pol.retired, "active ADR rules are live");
    assert_eq!(
        pol.provenance.reference.as_deref(),
        Some("ADR-011-architecture-wiki.md#POL-1101")
    );
    let hist = rules.iter().find(|r| r.id == "PAT-9904").unwrap();
    assert!(
        hist.retired,
        "status: superseded mints retired rules — parsed, preserved, withdrawn from recall"
    );
}

#[test]
fn contract_rejects_unknown_keys_and_bad_status_loudly() {
    // The contract is closed: an unknown key (here the common `deciders:` extension) must
    // surface, never silently drop.
    let bad_key =
        "---\nid: x-adr-1\ntitle: X\nstatus: active\ndate: 2026-08-29\ndeciders: me\n---\n";
    let dir = dir_with("badkey", &[("adr.md", bad_key)]);
    let err = ingest_from(&MarkdownAdapter::new(&dir))
        .unwrap_err()
        .to_string();
    assert!(err.contains("deciders"), "{err}");

    // `accepted` is NOT in the status vocabulary — the contract spells it `active`.
    let bad_status = "---\nid: x-adr-1\ntitle: X\nstatus: accepted\ndate: 2026-08-29\n---\n";
    let dir = dir_with("badstatus", &[("adr.md", bad_status)]);
    let err = ingest_from(&MarkdownAdapter::new(&dir))
        .unwrap_err()
        .to_string();
    assert!(err.contains("status") && err.contains("accepted"), "{err}");
}
