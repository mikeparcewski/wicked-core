//! WT-C4 (DES-walkthrough-proof §4.12 S5): the shipped testing starter
//! (`crates/wicked-governance/seed/testing/rules/testing-starter.json`) ingests VERBATIM through
//! `wicked-core rules ingest` (the JSON lane), yields exactly TST-1001..1003 on the `testing`
//! steering page with no `effect` (advisory: recall-only), and recall by type returns them. STEERING.md
//! documents the testing rules — held = `allow_with_conditions` + obligations — and its starter table
//! is pinned to the seed file so the doc and the data cannot drift apart.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_wicked-core");

/// The guide, byte-for-byte as shipped (line endings normalized for a CRLF checkout).
fn steering_md() -> String {
    include_str!("../crates/wicked-governance/STEERING.md").replace("\r\n", "\n")
}

/// The starter, byte-for-byte as shipped.
fn seed() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../crates/wicked-governance/seed/testing/rules/testing-starter.json"
    ))
    .expect("the testing starter is valid JSON")
}

/// Pre-main: arm the hermetic emit spool (core#311), like every test binary.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only touches process env vars
/// and the filesystem via the std API.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

fn ids_of(report: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<String> = report["rules"]
        .as_array()
        .expect("a rules array")
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

#[test]
fn the_testing_starter_ingests_advisory_and_recalls_by_type() {
    let dir = std::env::temp_dir().join(format!("wc-testing-starter-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("gov.db").to_string_lossy().into_owned();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("crates/wicked-governance/seed/testing");
    let run = |args: &[&str]| {
        Command::new(BIN)
            .args(args)
            .output()
            .expect("run wicked-core")
    };

    // Ingest twice: the second is a non-event (id-keyed).
    for pass in 0..2 {
        let out = run(&["rules", "ingest", root.to_str().unwrap(), "--db", &db]);
        assert!(
            out.status.success(),
            "pass {pass}: the shipped testing starter must ingest cleanly: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let out = run(&[
        "rules", "recall", "--db", &db, "--type", "testing", "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(ids_of(&report), ["TST-1001", "TST-1002", "TST-1003"]);
    for r in report["rules"].as_array().unwrap() {
        assert_eq!(r["steering_type"], "testing", "{r}");
        assert!(
            r.get("effect").is_none(),
            "the starter is advisory (no effect): {r}"
        );
        assert_eq!(r["applies_to"], serde_json::json!(["plan.compose"]), "{r}");
    }

    // Another page does not list them; the unfiltered list carries all three, all recall-only.
    let out = run(&[
        "rules", "recall", "--db", &db, "--type", "security", "--json",
    ]);
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(ids_of(&report).is_empty(), "{report}");
    let out = run(&["rules", "list", "--db", &db, "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(ids_of(&report), ["TST-1001", "TST-1002", "TST-1003"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// STEERING.md's testing section names the held encoding and the closed vocabulary, and its
/// starter table lists every seed rule with the obligations its hold binds — read from the seed.
#[test]
fn steering_md_documents_the_testing_rules_and_matches_the_starter() {
    let doc = steering_md();
    let start = doc
        .find("\n## Testing rules")
        .expect("STEERING.md has a `## Testing rules` section");
    let section = &doc[start + 1..];
    let section = &section[..section[3..].find("\n## ").map_or(section.len(), |i| i + 3)];
    for needle in [
        "plan.compose",
        "allow_with_conditions",
        "`step:walkthrough`",
        "`step:test`",
        "`step:security_review`",
        "floor_raised",
        "wicked-core rules ingest crates/wicked-governance/seed/testing",
        "Obligations on a rule without `effect` are inert",
        "no `effect` (recall-only)",
        "`effect: allow_with_conditions` + non-empty `obligations`",
    ] {
        assert!(
            section.contains(needle),
            "the testing section names {needle:?}"
        );
    }
    let seed = seed();
    let rules = seed["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 3);
    for r in rules {
        let id = r["id"].as_str().unwrap();
        let row = section
            .lines()
            .find(|l| l.starts_with(&format!("| `{id}` |")))
            .unwrap_or_else(|| panic!("the starter table has a row for {id}"));
        assert!(
            row.contains(r["statement"].as_str().unwrap()),
            "{id}: the row carries the statement: {row}"
        );
        let mut obligations: Vec<&str> = r["obligations"]
            .as_array()
            .map(|a| a.iter().map(|o| o.as_str().unwrap()).collect())
            .unwrap_or_default();
        obligations.sort();
        // The last cell is "Obligations when held": exactly the seed's tokens, no more.
        let cells: Vec<&str> = row.trim_matches('|').split(" | ").collect();
        let last = cells.last().unwrap();
        let mut documented: Vec<&str> = last.split('`').skip(1).step_by(2).collect();
        documented.sort();
        assert_eq!(
            documented, obligations,
            "{id}: the row's obligations: {row}"
        );
        if obligations.is_empty() {
            assert!(
                row.contains("none"),
                "{id}: an obligation-free row says so: {row}"
            );
        }
        if id == "TST-1003" {
            let trigger = r["trigger"]["contains"].as_str().unwrap();
            let alt = trigger
                .split("(?i:")
                .nth(1)
                .and_then(|t| t.strip_suffix(')'))
                .expect("TST-1003's trigger ends in one (?i:...) alternation");
            let mut words: Vec<&str> = alt.split('|').collect();
            words.sort();
            let trigger_cell = cells[cells.len() - 2];
            let mut documented: Vec<&str> = trigger_cell
                .split('`')
                .skip(1)
                .step_by(2)
                .filter(|t| t.chars().all(|c| c.is_ascii_lowercase()))
                .collect();
            documented.sort();
            assert_eq!(documented, words, "TST-1003's documented risk words: {row}");
        }
        assert_eq!(r["steering_type"], "testing", "{id}");
        assert!(r.get("effect").is_none(), "{id} is shipped advisory");
    }
}
