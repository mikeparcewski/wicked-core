//! STEERING — the unified steering-rule model at the operator CLI seam:
//!
//! - `rules recall --type <t>` / `rules list --type <t>`: the steering_type facet end-to-end
//!   (frontmatter → store → faceted report), typo'd values failing loud;
//! - `rules list [--include-retired]`: the management listing over the unified store — retired
//!   rows listable on request (the 0.7.5 recall-skips-retired gap), decide-lane (effect-bearing /
//!   migrated-policy) rows always listed, while `rules recall` keeps excluding both;
//! - `rules ingest`: legacy `Other(POLICY)` rows migrate one-time/idempotently into steering
//!   rules, ids unchanged, reported on stdout — and freshly ingested policies land as
//!   effect-bearing steering rules directly (the register_policy shim).

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_wicked-core");

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wc-steering-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A steering corpus: a security-typed doc, an architecture (defaulted) doc, a superseded doc
/// (mints RETIRED rules), and one deny policy (the merged Policy model's lane).
fn write_pack(dir: &std::path::Path) {
    std::fs::write(
        dir.join("security.md"),
        "---\nid: sec\ntitle: Security doctrine\nsteering_type: security\nweight: 2\n---\n\n\
         ## Rules\n\n- PAT-100 (error): validate inputs.\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("arch.md"),
        "---\nid: arch\ntitle: Architecture doctrine\n---\n\n\
         ## Rules\n\n- PAT-200 (error): single binary per product.\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("old.md"),
        "---\nid: old\ntitle: Superseded doctrine\nstatus: superseded\n---\n\n\
         ## Rules\n\n- POL-300 (warn): the old way.\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("policies")).unwrap();
    std::fs::write(
        dir.join("policies").join("deny.json"),
        r#"{ "id": "pol-no-leaks", "kind": "security", "applies_to": ["build"],
             "effect": "deny", "trigger": { "contains": "LEAKME" },
             "severity": "high", "rule": "never leak LEAKME" }"#,
    )
    .unwrap();
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("run wicked-core")
}

fn ids_of(report: &serde_json::Value) -> Vec<String> {
    report["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn steering_type_facet_and_listing_work_end_to_end() {
    let dir = scratch("facet");
    let pack = dir.join("pack");
    std::fs::create_dir_all(&pack).unwrap();
    write_pack(&pack);
    let db = dir.join("gov.db");
    let db = db.to_str().unwrap();

    let out = run(&["rules", "ingest", pack.to_str().unwrap(), "--db", db]);
    assert!(
        out.status.success(),
        "ingest: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // ── recall --type: the steering facet end-to-end ──
    let out = run(&[
        "rules", "recall", "--db", db, "--type", "security", "--json",
    ]);
    assert!(out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(ids_of(&report), vec!["PAT-100"], "security page only");
    assert_eq!(
        report["query"]["steering_type"], "security",
        "the echoed query records the steering facet"
    );
    assert_eq!(
        report["rules"][0]["steering_type"], "security",
        "the rule carries its type on the wire"
    );
    assert_eq!(
        report["rules"][0]["weight"], 2.0,
        "frontmatter weight rides the rule"
    );

    let out = run(&[
        "rules",
        "recall",
        "--db",
        db,
        "--type",
        "architecture",
        "--json",
    ]);
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        ids_of(&report),
        vec!["PAT-200"],
        "an untyped doc's rules default onto the architecture page"
    );

    // A typo'd --type fails LOUD (exit 1) — never a silent empty report.
    let out = run(&["rules", "recall", "--db", db, "--type", "vibes"]);
    assert!(
        !out.status.success(),
        "junk --type must be an operational error"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--type"),
        "the error names the flag"
    );

    // ── recall vs list: the enforcement funnel vs the management view ──
    let out = run(&["rules", "recall", "--db", db, "--json"]);
    let recall_report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let recall_ids = ids_of(&recall_report);
    assert!(
        !recall_ids.contains(&"POL-300".to_string()),
        "recall never returns retired rows: {recall_ids:?}"
    );
    assert!(
        !recall_ids.contains(&"pol-no-leaks".to_string()),
        "recall never returns decide-lane (effect-bearing) rows: {recall_ids:?}"
    );

    let out = run(&["rules", "list", "--db", db, "--json"]);
    assert!(out.status.success());
    let list_report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let list_ids = ids_of(&list_report);
    assert!(
        list_ids.contains(&"pol-no-leaks".to_string()),
        "the listing shows the policy's steering twin: {list_ids:?}"
    );
    assert!(
        !list_ids.contains(&"POL-300".to_string()),
        "without --include-retired the withdrawn row stays hidden: {list_ids:?}"
    );
    assert_eq!(list_report["include_retired"], false);
    let pol = list_report["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "pol-no-leaks")
        .unwrap();
    assert_eq!(pol["effect"], "deny", "the merged effect rides the listing");
    assert_eq!(
        pol["steering_type"], "security",
        "kind security → security page"
    );
    assert_eq!(
        pol["provenance"]["source"], "policy",
        "policy-lane provenance is first-class"
    );

    // ── --include-retired: the audit view (the recall-skips-retired listing gap, closed) ──
    let out = run(&["rules", "list", "--db", db, "--include-retired", "--json"]);
    let all_report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let all_ids = ids_of(&all_report);
    assert!(
        all_ids.contains(&"POL-300".to_string()),
        "--include-retired lists the withdrawn row: {all_ids:?}"
    );
    let retired_row = all_report["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "POL-300")
        .unwrap();
    assert_eq!(retired_row["retired"], true, "and says it is retired");
    assert_eq!(all_report["include_retired"], true);

    // Text mode marks the special rows so a human sees what a JSON consumer sees.
    let out = run(&["rules", "list", "--db", db, "--include-retired"]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("[RETIRED]"), "{text}");
    assert!(text.contains("[effect: Deny]"), "{text}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A store populated by the PRE-STEERING release (legacy policy node only, no unified twin)
/// migrates on the next `rules ingest`: one-time, idempotent, ids unchanged, reported on stdout —
/// and the scoreboard's by-type breakdown counts the migrated row.
#[test]
fn legacy_policy_rows_migrate_on_ingest_and_show_in_the_by_type_breakdown() {
    use wicked_apps_core::{GraphWrite, ToNode};

    let dir = scratch("migrate");
    let pack = dir.join("pack");
    std::fs::create_dir_all(&pack).unwrap();
    // A minimal doc so the ingest has a population (empty populations fail loud by design).
    std::fs::write(
        pack.join("doc.md"),
        "---\nid: d\ntitle: D\n---\n\n## Rules\n\n- PAT-001 (info): s.\n",
    )
    .unwrap();
    let db = dir.join("gov.db");
    let db_str = db.to_str().unwrap();

    // Seed the legacy row EXACTLY as the previous release wrote it: the policy node alone.
    {
        let mut store = wicked_apps_core::open_store(Some(db_str)).unwrap();
        let policy = wicked_governance::Policy {
            id: "pol-legacy".into(),
            kind: "guardrail".into(),
            applies_to: vec!["build".into()],
            effect: wicked_governance::Effect::Deny,
            trigger: wicked_governance::Trigger {
                contains: Some("BOOM".into()),
            },
            obligations: vec![],
            criteria: "no booms".into(),
            severity: wicked_governance::Severity::High,
            rule: "never boom".into(),
            retired: false,
        };
        store.begin_batch().unwrap();
        store.upsert_nodes(&[policy.to_node()]).unwrap();
        store.commit_batch().unwrap();
    }

    let out = run(&["rules", "ingest", pack.to_str().unwrap(), "--db", db_str]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        stdout.contains("migrated 1 legacy policy row") && stdout.contains("pol-legacy"),
        "the migration is reported, not silent: {stdout}"
    );

    // The migrated row is listable, id unchanged, filed under operations (guardrail → operations).
    let out = run(&[
        "rules",
        "list",
        "--db",
        db_str,
        "--type",
        "operations",
        "--json",
    ]);
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(ids_of(&report), vec!["pol-legacy"]);
    assert_eq!(report["rules"][0]["effect"], "deny");
    assert_eq!(report["rules"][0]["criteria"], "no booms");

    // Idempotent: a second ingest reports NO migration (nothing left to do).
    let out = run(&["rules", "ingest", pack.to_str().unwrap(), "--db", db_str]);
    assert!(out.status.success());
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("migrated"),
        "a second pass migrates nothing"
    );

    // The scoreboard's STEERING by-type breakdown counts it (JSON + human report).
    let out = run(&["rules", "scoreboard", "--db", db_str, "--json"]);
    assert!(out.status.success());
    let sb: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(sb["by_type"]["operations"]["total"], 1);
    assert_eq!(sb["by_type"]["operations"]["enforcing"], 1);
    assert_eq!(
        sb["by_type"]["architecture"]["total"], 1,
        "the doc rule defaults"
    );
    let out = run(&["rules", "scoreboard", "--db", db_str]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        text.contains("operations: 1 active") && text.contains("1 enforcing"),
        "{text}"
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
