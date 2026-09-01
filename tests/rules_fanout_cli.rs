//! AW-5 CLI end-to-end: `wicked-core rules fanout <dir>` fans one ruleset out across the
//! deliberate store split, smoke-verifies every cli lane against the worker-visible `--db`, and
//! writes the manifest receipt keyed on PAT-/POL- ids. Plus the daemon fence: a lane path under
//! `~/.wicked-crew` is refused (never CLI against a daemon-held store).

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_wicked-core");

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wc-fanout-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One deny policy + one JSON conformance rule + one markdown conformance rule.
fn write_ruleset(base: &std::path::Path) -> std::path::PathBuf {
    let ruleset = base.join("ruleset");
    std::fs::create_dir_all(ruleset.join("policies")).unwrap();
    std::fs::create_dir_all(ruleset.join("rules")).unwrap();
    std::fs::write(
        ruleset.join("policies/deny.json"),
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
    std::fs::write(
        ruleset.join("rules/bundle.json"),
        serde_json::json!({ "rules": [
            { "id": "PAT-001", "rule_type": "pattern", "statement": "no plaintext secrets",
              "severity": "critical", "confidence": 0.95,
              "provenance": { "ref": "wiki://secure-coding#PAT-001", "source_kinds": ["doc"] } }
        ]})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        ruleset.join("event-grammar.md"),
        "---\nid: event-grammar\ntitle: Event grammar\n---\n\n## Rules\n\n\
         - `POL-100` (error): Event types are 4-segment wicked.<domain>.<noun>.<verb>.\n",
    )
    .unwrap();
    ruleset
}

#[test]
fn fanout_lands_in_all_lanes_and_writes_the_manifest_receipt() {
    let base = scratch("e2e");
    let ruleset = write_ruleset(&base);
    let gov = base.join("gov.db");
    let repo_a = base.join("repo-a.db");
    let repo_b = base.join("repo-b.db");
    let know = base.join("knowledge.db");
    let manifest_path = base.join("fanout-manifest.json");

    let out = Command::new(BIN)
        .args([
            "rules",
            "fanout",
            ruleset.to_str().unwrap(),
            "--scope",
            "workspace",
            "--enforcement-db",
            gov.to_str().unwrap(),
            "--discovery-db",
            repo_a.to_str().unwrap(),
            "--discovery-db",
            repo_b.to_str().unwrap(),
            "--knowledge-db",
            know.to_str().unwrap(),
            "--manifest",
            manifest_path.to_str().unwrap(),
        ])
        .output()
        .expect("run rules fanout");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "fanout must succeed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("2 conformance rules + 1 policies"),
        "{stdout}"
    );
    // Per-lane verification is REPORTED, one line per store a governed run reads.
    assert_eq!(
        stdout.matches("VERIFIED").count(),
        4,
        "enforcement + 2 discovery + 1 knowledge: {stdout}"
    );

    // The manifest receipt: keyed on PAT-/POL- ids, every rule mapped to its three lanes.
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["manifest_version"], "1.0");
    assert_eq!(manifest["scope"], "workspace");
    assert_eq!(manifest["enforcement"]["transport"], "cli");
    assert_eq!(manifest["enforcement"]["verified"], true);
    assert_eq!(manifest["discovery"].as_array().unwrap().len(), 2);
    for id in ["PAT-001", "POL-100"] {
        let entry = &manifest["rules"][id];
        assert!(
            entry["enforcement"].as_str().unwrap().starts_with("cli:"),
            "{id}: {entry}"
        );
        assert_eq!(entry["discovery"].as_array().unwrap().len(), 2, "{id}");
        assert!(
            entry["knowledge"][0]
                .as_str()
                .unwrap()
                .ends_with(&format!("#kchunk:rule-rationale/{id}")),
            "{id}: {entry}"
        );
    }
    assert!(manifest["policies"]["pol-deny-secretleak"]["enforcement"]
        .as_str()
        .unwrap()
        .starts_with("cli:"));

    // All three lane stores exist on disk (the smoke already verified their contents through the
    // consumers' read paths — crates/wicked-governance/tests/fanout.rs re-proves that per lane).
    for db in [&gov, &repo_a, &repo_b, &know] {
        assert!(db.exists(), "{db:?} must exist after a verified fan-out");
    }
}

/// The daemon fence: a lane path under `~/.wicked-crew` is refused with the single-writer
/// rationale and the crew-api alternative. HOME is overridden per-process so the test never goes
/// near a real daemon home.
#[test]
fn a_lane_path_under_the_daemon_home_is_refused() {
    let base = scratch("fence");
    let ruleset = write_ruleset(&base);
    let fake_home = base.join("home");
    let daemon_db = fake_home.join(".wicked-crew/core.db");
    std::fs::create_dir_all(daemon_db.parent().unwrap()).unwrap();

    let out = Command::new(BIN)
        .args([
            "rules",
            "fanout",
            ruleset.to_str().unwrap(),
            "--enforcement-db",
            daemon_db.to_str().unwrap(),
            "--discovery-db",
            base.join("repo-a.db").to_str().unwrap(),
            "--knowledge-db",
            base.join("knowledge.db").to_str().unwrap(),
        ])
        .env("HOME", &fake_home)
        .env("USERPROFILE", &fake_home)
        .output()
        .expect("run rules fanout");
    assert!(!out.status.success(), "a daemon-held store must be refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("single-writer") && stderr.contains("--enforcement-crew-api"),
        "the refusal must teach the invariant AND the sanctioned path: {stderr}"
    );
    assert!(
        !base.join("repo-a.db").exists(),
        "the fence must fire BEFORE any lane is written — no partial fan-out"
    );
}

/// A daemon-held enforcement target: the manifest records crew-api PENDING and the POST payload is
/// emitted next to the manifest, while the cli lanes still verify.
#[test]
fn crew_api_enforcement_emits_the_post_payload_and_pending_manifest() {
    let base = scratch("crew-api");
    let ruleset = write_ruleset(&base);
    let manifest_path = base.join("m.json");

    let out = Command::new(BIN)
        .args([
            "rules",
            "fanout",
            ruleset.to_str().unwrap(),
            "--enforcement-crew-api",
            "http://127.0.0.1:7901/api/v1",
            "--discovery-db",
            base.join("repo-a.db").to_str().unwrap(),
            "--knowledge-db",
            base.join("knowledge.db").to_str().unwrap(),
            "--manifest",
            manifest_path.to_str().unwrap(),
        ])
        .output()
        .expect("run rules fanout");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("PENDING"), "{stdout}");
    assert!(
        stdout.contains("rules/preview"),
        "the verify step is named: {stdout}"
    );

    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["enforcement"]["transport"], "crew-api");
    assert_eq!(manifest["enforcement"]["verified"], false);

    // The enforcement copies exist concretely as the POST payload.
    let payload_path = format!("{}.crew-payload.json", manifest_path.display());
    let payload: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&payload_path).unwrap()).unwrap();
    assert_eq!(payload["policies"][0]["id"], "pol-deny-secretleak");
    let rule_ids: Vec<&str> = payload["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(
        rule_ids.contains(&"PAT-001") && rule_ids.contains(&"POL-100"),
        "{rule_ids:?}"
    );
}

/// Argument hygiene mirrors `rules ingest`: a flag-shaped value and a missing enforcement target
/// both refuse loudly instead of writing the wrong store.
#[test]
fn malformed_arguments_refuse_loudly() {
    let base = scratch("args");
    let ruleset = write_ruleset(&base);

    // No enforcement target at all.
    let out = Command::new(BIN)
        .args([
            "rules",
            "fanout",
            ruleset.to_str().unwrap(),
            "--discovery-db",
            base.join("r.db").to_str().unwrap(),
            "--knowledge-db",
            base.join("k.db").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("enforcement target is required"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A value-flag swallowing the next flag (`--enforcement-db --discovery-db …`).
    let out = Command::new(BIN)
        .args([
            "rules",
            "fanout",
            ruleset.to_str().unwrap(),
            "--enforcement-db",
            "--discovery-db",
            base.join("r.db").to_str().unwrap(),
            "--knowledge-db",
            base.join("k.db").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("flag-shaped"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Both enforcement transports at once.
    let out = Command::new(BIN)
        .args([
            "rules",
            "fanout",
            ruleset.to_str().unwrap(),
            "--enforcement-db",
            base.join("gov.db").to_str().unwrap(),
            "--enforcement-crew-api",
            "http://127.0.0.1:7901",
            "--discovery-db",
            base.join("r.db").to_str().unwrap(),
            "--knowledge-db",
            base.join("k.db").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("mutually"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
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
