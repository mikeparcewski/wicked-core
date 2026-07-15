//! core#26 end-to-end: `wicked-core rules ingest <dir>` populates a store with governance POLICIES
//! (deny) + CONFORMANCE RULES (recall→obligation), and the real `output-gate-hook` binary then DENIES a
//! policy-violating output on that store WITH the recalled conformance rules attached as obligations —
//! i.e. the populated rules actually change the verdict (DES-OUTGOV-006).

use std::io::Write;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_wicked-core");

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wc-rules-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a ruleset dir with one Deny policy (trips on "SECRETLEAK") + one conformance rule (recalled as
/// an obligation on any output). Returns the ruleset dir.
fn write_ruleset(base: &std::path::Path) -> std::path::PathBuf {
    let ruleset = base.join("ruleset");
    std::fs::create_dir_all(ruleset.join("policies")).unwrap();
    std::fs::create_dir_all(ruleset.join("rules")).unwrap();
    // A Deny policy for phase "build" that fires when the output contains SECRETLEAK.
    std::fs::write(
        ruleset.join("policies/deny-secrets.json"),
        serde_json::json!({
            "id": "pol-deny-secretleak",
            "kind": "security",
            "applies_to": ["build"],
            "effect": "deny",
            "trigger": { "contains": "SECRETLEAK" },
            "obligations": [],
            "criteria": "no secret material in generated output",
            "severity": "high",
            "rule": "Deny any output that embeds a SECRETLEAK marker."
        })
        .to_string(),
    )
    .unwrap();
    // A conformance rule (recall→obligation) — wildcard facets so it recalls for any output.
    std::fs::write(
        ruleset.join("rules/conformance.json"),
        serde_json::json!({
            "rules": [
                { "id": "PAT-001", "rule_type": "pattern", "statement": "no plaintext secrets",
                  "severity": "critical", "confidence": 0.95,
                  "provenance": { "ref": "secure-coding-standard#PAT-001", "source_kinds": ["doc"] } }
            ]
        })
        .to_string(),
    )
    .unwrap();
    ruleset
}

fn ingest(db: &str, ruleset: &std::path::Path) -> std::process::Output {
    Command::new(BIN)
        .args(["rules", "ingest", ruleset.to_str().unwrap(), "--db", db])
        .output()
        .expect("run rules ingest")
}

/// Run the real output-gate-hook against `output` on `db`; returns (exit_code, decisions_log_contents).
fn output_gate(db: &str, decisions: &std::path::Path, output: &str) -> (i32, String) {
    let mut child = Command::new(BIN)
        .args([
            "output-gate-hook",
            "--scope",
            "s",
            "--phase",
            "build",
            "--db",
            db,
        ])
        .env("WICKED_DECISIONS_PATH", decisions)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn output-gate-hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(output.as_bytes())
        .unwrap();
    let code = child.wait().unwrap().code().unwrap_or(-1);
    let log = std::fs::read_to_string(decisions).unwrap_or_default();
    (code, log)
}

#[test]
fn ingested_policy_denies_a_violating_output_with_recalled_rule_obligations() {
    let base = scratch("deny");
    let db = base.join("estate.db");
    let db_s = db.to_str().unwrap().to_string();
    let ruleset = write_ruleset(&base);

    let out = ingest(&db_s, &ruleset);
    assert!(
        out.status.success(),
        "rules ingest exits 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("1 policies") && stdout.contains("1 conformance rules"),
        "ingest reports the counts: {stdout}"
    );

    // A VIOLATING output (contains SECRETLEAK) → DENY (exit 2), with the recalled conformance rule
    // attached as an obligation on the claim.
    let (code, log) = output_gate(&db_s, &base.join("d1.ndjson"), "here is a SECRETLEAK value");
    assert_eq!(
        code, 2,
        "the ingested Deny policy denies the violating output"
    );
    assert!(
        log.contains("PAT-001") && log.contains("no plaintext secrets"),
        "the recalled conformance rule rides the claim as an obligation: {log}"
    );

    // A BENIGN output (trips no policy) → ALLOW (exit 0) — but the recalled rules STILL attach as
    // obligations (recall is facet-based, independent of the deny).
    let (code, log) = output_gate(&db_s, &base.join("d2.ndjson"), "a perfectly clean result");
    assert_eq!(code, 0, "a benign output is allowed");
    assert!(
        log.contains("PAT-001"),
        "recall attaches the applicable ruleset even on an allow: {log}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn empty_ruleset_dir_fails_loud() {
    let base = scratch("empty");
    let db = base.join("estate.db");
    let empty = base.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let out = Command::new(BIN)
        .args([
            "rules",
            "ingest",
            empty.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "an empty ruleset (0 policies + 0 rules) must fail loud, not silently populate nothing"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("empty population"),
        "the error names the empty-population refusal"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Write a single policy file with the given JSON object; returns the ruleset dir.
fn ruleset_with_policy(base: &std::path::Path, policy: serde_json::Value) -> std::path::PathBuf {
    let ruleset = base.join("ruleset");
    std::fs::create_dir_all(ruleset.join("policies")).unwrap();
    std::fs::write(ruleset.join("policies/p.json"), policy.to_string()).unwrap();
    ruleset
}

#[test]
fn a_policy_with_empty_applies_to_fails_loud() {
    // CRITICAL fail-open guard: a policy selected for NO phase enforces nothing — registering it would
    // read as "governed" while the gate never fires. Must fail loud, not silently populate.
    let base = scratch("noapply");
    let db = base.join("estate.db");
    let ruleset = ruleset_with_policy(
        &base,
        serde_json::json!({
            "id": "pol-x", "kind": "k", "applies_to": [], "effect": "deny",
            "trigger": { "contains": "BAD" }, "obligations": [], "criteria": "c",
            "severity": "high", "rule": "r"
        }),
    );
    let out = ingest(db.to_str().unwrap(), &ruleset);
    assert!(!out.status.success(), "empty applies_to must fail loud");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("applies_to"),
        "the error names the non-enforcing policy: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_policy_with_a_typoed_field_fails_loud() {
    // deny_unknown_fields: `applies` (typo for applies_to) is a LOUD parse error, not a silently-ignored
    // key that leaves the policy selected for no phase.
    let base = scratch("typo");
    let db = base.join("estate.db");
    let ruleset = ruleset_with_policy(
        &base,
        serde_json::json!({
            "id": "pol-x", "kind": "k", "applies": ["build"], "effect": "deny",
            "trigger": { "contains": "BAD" }, "obligations": [], "criteria": "c",
            "severity": "high", "rule": "r"
        }),
    );
    let out = ingest(db.to_str().unwrap(), &ruleset);
    assert!(
        !out.status.success(),
        "a typo'd policy field must fail loud (deny_unknown_fields)"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_policy_with_an_invalid_regex_trigger_fails_loud() {
    // A malformed trigger.contains regex fails CLOSED in the engine (never fires) — a silent fail-open.
    // The write boundary must reject it so a dead Deny can't populate.
    let base = scratch("badregex");
    let db = base.join("estate.db");
    let ruleset = ruleset_with_policy(
        &base,
        serde_json::json!({
            "id": "pol-x", "kind": "k", "applies_to": ["build"], "effect": "deny",
            "trigger": { "contains": "([unclosed" }, "obligations": [], "criteria": "c",
            "severity": "high", "rule": "r"
        }),
    );
    let out = ingest(db.to_str().unwrap(), &ruleset);
    assert!(
        !out.status.success(),
        "an invalid regex trigger must fail loud"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not a valid regex"),
        "the error names the dead-Deny fail-open: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_policy_with_blank_applies_to_entries_fails_loud() {
    // `applies_to: [""]` matches no real phase — the same non-enforcing fail-open as `[]`.
    let base = scratch("blankapply");
    let db = base.join("estate.db");
    let ruleset = ruleset_with_policy(
        &base,
        serde_json::json!({
            "id": "pol-x", "kind": "k", "applies_to": ["  "], "effect": "deny",
            "trigger": { "contains": "BAD" }, "obligations": [], "criteria": "c",
            "severity": "high", "rule": "r"
        }),
    );
    let out = ingest(db.to_str().unwrap(), &ruleset);
    assert!(
        !out.status.success(),
        "blank applies_to entries must fail loud"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn duplicate_policy_id_fails_loud() {
    let base = scratch("dup");
    let db = base.join("estate.db");
    let ruleset = base.join("ruleset");
    std::fs::create_dir_all(ruleset.join("policies")).unwrap();
    // Two policies with the SAME id (one Deny, one Allow) — a silent overwrite could clobber the Deny.
    std::fs::write(
        ruleset.join("policies/dup.json"),
        serde_json::json!([
            { "id": "pol-dup", "kind": "k", "applies_to": ["build"], "effect": "deny",
              "trigger": { "contains": "X" }, "obligations": [], "criteria": "c", "severity": "high", "rule": "r" },
            { "id": "pol-dup", "kind": "k", "applies_to": ["build"], "effect": "allow",
              "trigger": {}, "obligations": [], "criteria": "c", "severity": "low", "rule": "r" }
        ])
        .to_string(),
    )
    .unwrap();
    let out = ingest(db.to_str().unwrap(), &ruleset);
    assert!(
        !out.status.success(),
        "a duplicate policy id must fail loud"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("duplicate policy id"));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn flags_before_the_dir_still_resolve() {
    // `rules ingest --db x <dir>` (flag before the positional) must resolve the dir.
    let base = scratch("flagorder");
    let db = base.join("estate.db");
    let ruleset = ruleset_with_policy(
        &base,
        serde_json::json!({
            "id": "pol-x", "kind": "k", "applies_to": ["build"], "effect": "deny",
            "trigger": { "contains": "BAD" }, "obligations": [], "criteria": "c",
            "severity": "high", "rule": "r"
        }),
    );
    let out = Command::new(BIN)
        .args([
            "rules",
            "ingest",
            "--db",
            db.to_str().unwrap(),
            ruleset.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "flag-before-dir must resolve: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn policies_only_ruleset_works() {
    // A ruleset with ONLY policies (no rules/ subdir) is tolerated.
    let base = scratch("polonly");
    let db = base.join("estate.db");
    let db_s = db.to_str().unwrap().to_string();
    let ruleset = base.join("ruleset");
    std::fs::create_dir_all(ruleset.join("policies")).unwrap();
    std::fs::write(
        ruleset.join("policies/p.json"),
        serde_json::json!({
            "id": "pol-x", "kind": "k", "applies_to": ["build"], "effect": "deny",
            "trigger": { "contains": "BAD" }, "obligations": [], "criteria": "c",
            "severity": "high", "rule": "r"
        })
        .to_string(),
    )
    .unwrap();
    let out = ingest(&db_s, &ruleset);
    assert!(out.status.success(), "policies-only ingest works");
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 policies + 0 conformance rules"));
    let _ = std::fs::remove_dir_all(&base);
}
