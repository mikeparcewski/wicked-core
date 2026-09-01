//! AW-17 — `wicked-core rules recall`, the CI conformance seam's recall-REPORT, proven against the
//! SHIPPED phase-scope policy pack (`governance/packs/phase-scope`, the core#296 doctrine twin):
//! a scratch ingest of the real pack, then a recall whose report cites rule ids + provenance refs
//! (the wiki URIs a CI comment links to), severity-ordered, read-only, and never exiting nonzero
//! on findings — v1 of the seam reports, it does not block (arch-R15).

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_wicked-core");

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wc-recall-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The shipped pack, ingested VERBATIM — the test would fail on a malformed pack doc, so the pack
/// in the repo is proven ingestable, not just proofread.
fn pack_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("governance/packs/phase-scope")
}

fn ingest_pack(db: &str) {
    let out = Command::new(BIN)
        .args(["rules", "ingest", pack_dir().to_str().unwrap(), "--db", db])
        .output()
        .expect("run rules ingest");
    assert!(
        out.status.success(),
        "the shipped phase-scope pack must ingest cleanly (a malformed pack doc fails loud): {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn recall(db: &str, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["rules", "recall", "--db", db];
    args.extend_from_slice(extra);
    Command::new(BIN)
        .args(&args)
        .output()
        .expect("run rules recall")
}

/// The seam's core contract: the report is severity-ordered, cites rule ids + provenance refs
/// (path@blob-sha#RULE-ID — the wiki URI), and exits 0 EVEN THOUGH rules matched (report ≠ block).
#[test]
fn recall_reports_the_shipped_pack_with_ids_and_wiki_uris() {
    let dir = scratch("pack");
    let db = dir.join("gov.db");
    let db = db.to_str().unwrap();
    ingest_pack(db);

    let out = recall(db, &["--json"]);
    assert!(
        out.status.success(),
        "recall is a REPORT: matched rules must not flip the exit code (v1 never blocks): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("recall --json emits one JSON object");
    let rules = report["rules"].as_array().expect("rules array");
    assert_eq!(
        report["count"].as_u64().unwrap() as usize,
        rules.len(),
        "count mirrors the array so a truncated report is detectable"
    );
    let ids: Vec<&str> = rules.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&"POL-2960") && ids.contains(&"POL-2961"),
        "the phase-scope write-denies must be recallable by id: {ids:?}"
    );
    // Severity-ordered, critical first: POL-2960 (critical) precedes POL-2961 (error).
    let pos = |id: &str| ids.iter().position(|i| *i == id).unwrap();
    assert!(
        pos("POL-2960") < pos("POL-2961"),
        "recall is severity-ordered critical→info: {ids:?}"
    );
    // The wiki URI: `<root-relative doc path>@<git blob sha>#<RULE-ID>` — what a CI comment
    // resolves to a repo link. Root-relative to the INGEST dir, stable across checkouts.
    let p2960 = rules.iter().find(|r| r["id"] == "POL-2960").unwrap();
    let reference = p2960["provenance"]["ref"].as_str().expect("provenance.ref");
    assert!(
        reference.starts_with("phase-scope.md@") && reference.ends_with("#POL-2960"),
        "provenance.ref must be the digest-bearing wiki URI: {reference}"
    );
    // The rule statement names the ENGINE gate that enforces it (core#306's
    // `engine:pre-build-scope`) — the pack is the doctrine twin, never a second enforcement tier.
    assert!(
        p2960["statement"]
            .as_str()
            .unwrap()
            .contains("engine:pre-build-scope"),
        "the doctrine rule and the engine gate must name each other (core#296 closure)"
    );
}

/// Facet filters narrow the report; a filtered-out severity is absent, not demoted.
#[test]
fn recall_severity_facet_filters_exactly() {
    let dir = scratch("facet");
    let db = dir.join("gov.db");
    let db = db.to_str().unwrap();
    ingest_pack(db);

    let out = recall(db, &["--severity", "critical", "--json"]);
    assert!(out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids: Vec<&str> = report["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"POL-2960"), "{ids:?}");
    assert!(
        !ids.contains(&"POL-2961"),
        "--severity critical must exclude the error-severity rule: {ids:?}"
    );

    // A typo'd severity fails LOUD (exit 1), never silently widening to "all severities".
    let out = recall(db, &["--severity", "kritical"]);
    assert!(
        !out.status.success(),
        "a junk --severity value must be an operational error, not a wildcard"
    );
}

/// Text mode carries the same citations (id + source ref) — the human-readable half of the seam.
/// And an EMPTY report is a printed diagnostic, never silence mistaken for conformance.
#[test]
fn recall_text_mode_cites_and_empty_is_a_diagnostic() {
    let dir = scratch("text");
    let db = dir.join("gov.db");
    let db = db.to_str().unwrap();
    ingest_pack(db);

    let out = recall(db, &[]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("POL-2960"), "{text}");
    assert!(
        text.contains("phase-scope.md@"),
        "text mode must cite the source ref too: {text}"
    );

    // A store with nothing ingested: exit 0 (the report ran) + an explicit "nothing to recall
    // against" diagnostic — the fail-open seam's honest degradation, recorded not silent.
    let empty_db = dir.join("empty.db");
    drop(wicked_apps_core::open_store(Some(empty_db.to_str().unwrap())).unwrap());
    let out = recall(empty_db.to_str().unwrap(), &[]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        text.contains("no rules matched"),
        "an empty report must SAY it is empty: {text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ── Test-harness hygiene (core#311) — not a test ─────────────────────────────────────────────
/// Arm the hermetic emit spool BEFORE main (pre-main is single-threaded, so no test thread can
/// race it): engine paths under test fire coarse fire-and-forget `wicked.*` emissions, and with
/// no shared store configured those spool — which must land in a per-process temp file, never in
/// the operator's real `~/.something-wicked/wicked-apps/emit-outbox.ndjson` replay queue. Every
/// binary in this suite carries this block; `harness_hygiene.rs` fails the suite if one is missing.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
