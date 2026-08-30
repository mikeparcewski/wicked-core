//! Edge-vocabulary pin — AW-19 / arch-R17 (estate ADR-011 §edge-vocabulary).
//!
//! "Governs" has exactly two legal spellings on the estate graph, and they are NOT
//! interchangeable:
//!
//! * **native [`EdgeKind::Governs`]** — MANDATORY for any edge whose target is a code-graph
//!   symbol (or a `wicked-apps` synthetic node). This is what this crate emits everywhere it
//!   mints a governs relationship ([`crate::ConformanceRule::governs_edge`], `conform`'s
//!   policy→claim edge).
//! * **stringly `EdgeKind::Other("governs")`** — legal ONLY between knowledge-store nodes,
//!   minted by wicked-estate-knowledge's DEC-2 relation grammar (`engine.rs` `relate`/`ingest`
//!   — a brain-migration legacy kept for store compatibility). It never appears inside the
//!   governance/code lane, so inside THIS crate's world any `Other("governs")` edge is a
//!   split-brain defect: a native-only `TraverseGraph`/recall filter would miss it, and a
//!   string-only filter would miss the native edges.
//!
//! Two enforcement layers keep the split-brain from ever growing:
//!
//! 1. **Runtime** — [`edge_vocabulary_violation`] / [`assert_edge_vocabulary`] reject the
//!    stringly spelling (any casing) before an edge batch is persisted through a governance
//!    code path.
//! 2. **Source scan** — `tests/edge_vocab_lint.rs` walks the whole wicked-core workspace and
//!    fails on any source line that constructs `EdgeKind::Other(…governs…)`, so a NEW stringly
//!    mint site cannot land unnoticed (this file is the single allowlisted exception — it names
//!    the spelling in order to ban it).
//!
//! During the deprecation window, recall/traversal surfaces that must see EVERY governs
//! relationship (estate `TraverseGraph`, knowledge-lane queries) match BOTH spellings; the
//! window and its exit criteria are documented in estate ADR-011 §edge-vocabulary.

use wicked_apps_core::{Edge, EdgeKind};

/// The banned stringly spelling (compared case-insensitively).
const STRINGLY_GOVERNS: &str = "governs";

/// Returns the violation message when `edge` carries the stringly spelling of governs
/// (`EdgeKind::Other("governs")`, any casing). Inside the governance/code lane every governs
/// relationship MUST be the native [`EdgeKind::Governs`]; the stringly spelling belongs to
/// knowledge-store relations only (wicked-estate-knowledge), which never flow through this
/// crate. `None` = the edge is vocabulary-clean (native variants and unrelated `Other` kinds
/// both pass).
pub fn edge_vocabulary_violation(edge: &Edge) -> Option<String> {
    match &edge.kind {
        EdgeKind::Other(kind) if kind.eq_ignore_ascii_case(STRINGLY_GOVERNS) => Some(format!(
            "edge {:?} -> {:?} spells governs as EdgeKind::Other({kind:?}) — inside the \
             governance/code lane every governs edge MUST be the native EdgeKind::Governs; the \
             stringly spelling is legal only between knowledge-store nodes (AW-19 / arch-R17; \
             estate ADR-011 §edge-vocabulary)",
            edge.source, edge.target
        )),
        _ => None,
    }
}

/// Fail-closed batch check: errors on the FIRST vocabulary violation (deny-dominates — one
/// mis-spelled edge poisons recall for every consumer, so the batch never lands partially).
pub fn assert_edge_vocabulary(edges: &[Edge]) -> anyhow::Result<()> {
    for edge in edges {
        if let Some(violation) = edge_vocabulary_violation(edge) {
            anyhow::bail!(violation);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::CONFORMANCE_RULE;
    use wicked_apps_core::{synthetic_symbol, Metadata};
    use wicked_estate_core::{Confidence, Provenance};

    fn edge(kind: EdgeKind) -> Edge {
        Edge {
            source: synthetic_symbol(CONFORMANCE_RULE, "PAT-100"),
            target: synthetic_symbol(CONFORMANCE_RULE, "PAT-200"),
            kind,
            confidence: Confidence::new(0.9),
            provenance: Provenance::Extractor("edge-vocab-test".to_string()),
            resolved_by: "edge-vocab-test".to_string(),
            location: None,
            metadata: Metadata::new(),
            evidence_count: 0,
        }
    }

    #[test]
    fn native_governs_is_clean() {
        assert_eq!(edge_vocabulary_violation(&edge(EdgeKind::Governs)), None);
    }

    #[test]
    fn unrelated_other_kinds_are_clean() {
        let e = edge(EdgeKind::Other("evidences".to_string()));
        assert_eq!(edge_vocabulary_violation(&e), None);
        assert!(assert_edge_vocabulary(&[e]).is_ok());
    }

    #[test]
    fn stringly_governs_is_rejected_any_casing() {
        for spelling in ["governs", "GOVERNS", "Governs"] {
            let e = edge(EdgeKind::Other(spelling.to_string()));
            let violation = edge_vocabulary_violation(&e)
                .unwrap_or_else(|| panic!("{spelling:?} must be flagged"));
            assert!(violation.contains("EdgeKind::Governs"), "{violation}");
        }
    }

    #[test]
    fn batch_assert_denies_on_first_violation() {
        let batch = vec![
            edge(EdgeKind::Governs),
            edge(EdgeKind::Other("governs".to_string())),
        ];
        let err = assert_edge_vocabulary(&batch).unwrap_err().to_string();
        assert!(err.contains("estate ADR-011"), "{err}");
    }
}
