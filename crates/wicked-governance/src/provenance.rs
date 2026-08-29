//! Rule provenance — the digest-bearing `ref` format and its per-run freshness stamp (AW-10 /
//! arch-R7).
//!
//! DES-OUTGOV-001 §3 specifies a provenance carriage the store never populated: the source digest
//! in `RuleProvenance.ref` and a `last_verified` freshness clock. This module is that carriage.
//!
//! ## The `ref` format
//!
//! A markdown-lane rule's `provenance.ref` is
//!
//! ```text
//! <repo-relative path>@<git blob sha>#<RULE-ID>
//! ```
//!
//! - the **path** is root-relative with forward slashes on every platform (orphan detection:
//!   "does the doc still exist, and does it still mint this rule?");
//! - the **sha** is the git BLOB sha-1 of the doc's bytes (`git hash-object <file>`), computed
//!   in-process by [`git_blob_sha1`] — no subprocess, no git checkout required, and it equals what
//!   estate's `file_git_sha` records for the same bytes, so the two correlate (change detection:
//!   "was the rule ingested from the doc's CURRENT content?");
//! - the **anchor** is the rule id within the doc (back-link: a gate denial's obligation leads to
//!   the exact rule in the exact doc).
//!
//! [`parse_provenance_ref`] accepts every historical shape — `path#id` (pre-digest ingests),
//! `path@sha#id`, `path@sha`, bare `path`, and author-supplied free-form refs from the JSON lane —
//! a legacy ref parses to `sha: None` (reported by drift as needing re-ingest, never a crash).
//!
//! ## The freshness stamp
//!
//! [`stamp_provenance`] writes the §3 typed annotation on the Rule node: type `provenance`, key
//! `source_ref`, value = the full ref, evidence envelope `source_type: documentation` +
//! `extraction_method: wicked-governance@<version>` + `last_verified: <now>`. It is delete-then-
//! insert on the (type, key) pair, so re-ingest is IDEMPOTENT: one row per rule, `last_verified`
//! refreshed per run — which is exactly what makes the store's `annotations_stale_since(cutoff)`
//! read answer "which rules has no ingest re-verified since `cutoff`?". A doc merge re-runs
//! `rules ingest` and the whole carriage self-heals (arch-R7's non-event).

use sha1::{Digest, Sha1};
use wicked_apps_core::{synthetic_symbol, GraphStore};
use wicked_estate_core::Annotation;

use crate::conformance::{ConformanceRule, CONFORMANCE_RULE};

/// Annotation `type` for the provenance freshness stamp (DES-OUTGOV-001 §3 carriage).
pub const PROVENANCE_ANNOTATION_TYPE: &str = "provenance";
/// Annotation `key` for the provenance freshness stamp.
pub const PROVENANCE_ANNOTATION_KEY: &str = "source_ref";
/// The evidence-envelope `extraction_method` this crate stamps.
pub const EXTRACTION_METHOD: &str = concat!("wicked-governance@", env!("CARGO_PKG_VERSION"));

/// The git BLOB sha-1 of `bytes` — identical to `git hash-object <file>` on the same bytes
/// (`sha1("blob <len>\0" + bytes)`, lowercase hex). Deterministic, in-process, works outside a git
/// checkout, and equals estate's recorded `file_git_sha` for the same content.
pub fn git_blob_sha1(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(40);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Build the digest-bearing ref: `<path>@<sha>#<anchor>`.
pub fn format_provenance_ref(path: &str, sha: &str, anchor: &str) -> String {
    format!("{path}@{sha}#{anchor}")
}

/// A parsed provenance ref. Every field but `path` is optional because every HISTORICAL ref shape
/// must keep parsing (a legacy `path#id` ref is drift residue, never a crash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRef {
    /// The root-relative doc path (or the whole ref, for free-form JSON-lane refs).
    pub path: String,
    /// The git blob sha the rule was ingested at, when the ref carries one.
    pub sha: Option<String>,
    /// The `#`-anchor (the rule id within the doc), when present.
    pub anchor: Option<String>,
}

/// Parse a provenance ref of any historical shape. The sha is recognized ONLY as a trailing
/// `@<40 lowercase hex>` before the anchor — an `@` inside an ordinary path or free-form ref does
/// not false-positive (the right-hand side must be exactly a 40-hex blob sha).
pub fn parse_provenance_ref(r: &str) -> ParsedRef {
    let (left, anchor) = match r.split_once('#') {
        Some((l, a)) => (l, Some(a.to_string())),
        None => (r, None),
    };
    let (path, sha) = match left.rsplit_once('@') {
        Some((p, s)) if is_blob_sha(s) => (p.to_string(), Some(s.to_string())),
        _ => (left.to_string(), None),
    };
    ParsedRef { path, sha, anchor }
}

fn is_blob_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Stamp the DES-OUTGOV-001 §3 freshness annotation on `rule`'s node: ONE `provenance/source_ref`
/// row whose value is the current ref and whose `last_verified` is `now`. Delete-then-insert keyed
/// on (type, key), so calling this every ingest run is idempotent — the row count stays 1 and only
/// the freshness clock (and, after a doc change, the ref value) moves. The rule node must already
/// be registered ([`crate::register_rule`]); estate's `annotate` is a no-op on an absent symbol,
/// which would silently drop the stamp, so that precondition is checked here fail-loud.
pub fn stamp_provenance(
    store: &mut dyn GraphStore,
    rule: &ConformanceRule,
    now: i64,
) -> anyhow::Result<()> {
    let symbol = synthetic_symbol(CONFORMANCE_RULE, &rule.id);
    if store.get_node(&symbol)?.is_none() {
        anyhow::bail!(
            "stamp_provenance: rule {} has no registered node — register_rule must run first \
             (estate's annotate silently no-ops on an absent symbol)",
            rule.id
        );
    }
    let value = rule.provenance.reference.clone().unwrap_or_default();
    store.delete_annotations(
        &symbol,
        Some(PROVENANCE_ANNOTATION_TYPE),
        PROVENANCE_ANNOTATION_KEY,
    )?;
    store.annotate(
        &symbol,
        Annotation::new(PROVENANCE_ANNOTATION_TYPE, PROVENANCE_ANNOTATION_KEY, value)
            .with_confidence(rule.confidence as f64)
            .with_provenance(rule.provenance.source.clone())
            .with_author("wicked-core rules ingest")
            .with_source_type("documentation")
            .with_extraction_method(EXTRACTION_METHOD)
            .with_last_verified(now),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{ConfSeverity, RuleProvenance, RuleType, Targets};
    use wicked_apps_core::{open_store, GraphRead};

    #[test]
    fn git_blob_sha1_matches_git_hash_object() {
        // The canonical git example: `echo 'test content' | git hash-object --stdin`.
        assert_eq!(
            git_blob_sha1(b"test content\n"),
            "d670460b4b4aece5915caf5c68d12f560a9fe3e4"
        );
        // Empty blob — another well-known git constant.
        assert_eq!(
            git_blob_sha1(b""),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
    }

    #[test]
    fn provenance_ref_round_trips_and_legacy_shapes_parse() {
        let sha = "d670460b4b4aece5915caf5c68d12f560a9fe3e4";
        let full = format_provenance_ref("docs/agent-behavior.md", sha, "PAT-001");
        assert_eq!(
            parse_provenance_ref(&full),
            ParsedRef {
                path: "docs/agent-behavior.md".into(),
                sha: Some(sha.into()),
                anchor: Some("PAT-001".into()),
            }
        );
        // Legacy pre-digest shape (`path#id`): parses with sha None — drift residue, not a crash.
        assert_eq!(
            parse_provenance_ref("docs/agent-behavior.md#PAT-001"),
            ParsedRef {
                path: "docs/agent-behavior.md".into(),
                sha: None,
                anchor: Some("PAT-001".into()),
            }
        );
        // A path containing `@` must not false-positive as a digest.
        assert_eq!(
            parse_provenance_ref("docs/v@2/rules.md#POL-002").path,
            "docs/v@2/rules.md"
        );
        // Free-form JSON-lane ref: everything lands in `path`, nothing fabricated.
        assert_eq!(
            parse_provenance_ref("handbook"),
            ParsedRef {
                path: "handbook".into(),
                sha: None,
                anchor: None,
            }
        );
        // Uppercase hex is NOT a git blob sha (git prints lowercase) — stays part of the path.
        let upper = "docs/x.md@D670460B4B4AECE5915CAF5C68D12F560A9FE3E4";
        assert_eq!(parse_provenance_ref(upper).sha, None);
    }

    fn rule_with_ref(id: &str, reference: &str) -> ConformanceRule {
        ConformanceRule {
            id: id.to_string(),
            rule_type: RuleType::Pattern,
            statement: "s".into(),
            severity: ConfSeverity::Warn,
            confidence: 0.8,
            targets: Targets::default(),
            symbol_ref: None,
            compliance: None,
            provenance: RuleProvenance {
                source: "markdown".into(),
                reference: Some(reference.to_string()),
                source_kinds: vec!["doc".into()],
            },
            retired: false,
        }
    }

    #[test]
    fn stamp_is_idempotent_and_refreshes_last_verified() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let rule = rule_with_ref("PAT-001", "docs/a.md#PAT-001");
        crate::register_rule(&mut store, &rule).unwrap();

        stamp_provenance(&mut store, &rule, 1_000).unwrap();
        stamp_provenance(&mut store, &rule, 2_000).unwrap();

        let symbol = synthetic_symbol(CONFORMANCE_RULE, "PAT-001");
        let rows: Vec<_> = store
            .annotations(&symbol)
            .unwrap()
            .into_iter()
            .filter(|a| a.r#type == PROVENANCE_ANNOTATION_TYPE)
            .collect();
        assert_eq!(rows.len(), 1, "re-stamping must not accumulate rows");
        assert_eq!(rows[0].key, PROVENANCE_ANNOTATION_KEY);
        assert_eq!(rows[0].value, "docs/a.md#PAT-001");
        assert_eq!(
            rows[0].last_verified, 2_000,
            "the freshness clock moves per run"
        );
        assert_eq!(rows[0].source_type, "documentation");

        // The annotations_stale_since surface answers the re-verification question.
        let stale = store.annotations_stale_since(3_000).unwrap();
        assert!(
            stale
                .iter()
                .any(|(s, a)| s == &symbol && a.r#type == PROVENANCE_ANNOTATION_TYPE),
            "a stamp older than the cutoff is reported stale"
        );
        let fresh = store.annotations_stale_since(1_500).unwrap();
        assert!(
            !fresh
                .iter()
                .any(|(s, a)| s == &symbol && a.r#type == PROVENANCE_ANNOTATION_TYPE),
            "a stamp newer than the cutoff is not stale"
        );
    }

    #[test]
    fn stamping_an_unregistered_rule_fails_loud() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let rule = rule_with_ref("PAT-002", "docs/a.md#PAT-002");
        let err = stamp_provenance(&mut store, &rule, 1_000)
            .expect_err("annotate on an absent symbol silently no-ops — must fail loud instead");
        assert!(err.to_string().contains("register_rule"), "{err}");
    }
}
