//! Smoke test for the operator CLI's validator subcommands (Lane D finding 5). Proves the new
//! `provision-validator` / `approve-validator` subcommands arg-parse and are advertised in the usage
//! string — WITHOUT running the live authoring (`provision-validator` shells out to real `claude`, so
//! only the missing-arg / usage paths, which fail BEFORE any store or CLI call, are exercised here).

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_wicked-core")
}

/// A per-TEST scratch directory under the system temp dir, created empty:
/// `wc-cli-smoke-<pid>-<test>-<n>`. Every `#[test]` in this binary is a thread of ONE process, so a
/// pid-only key is the same string for all of them — two tests sharing a prefix (or one test's
/// future copy) would race on the same path. The test name keeps them apart by construction and the
/// process-wide counter keeps two calls inside one test apart; the pre-clean means a stale dir left
/// by a crashed earlier run (pid reuse) cannot leak state in. Callers remove it when done.
fn scratch_dir(test: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "wc-cli-smoke-{}-{test}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the per-test scratch dir");
    dir
}

#[test]
fn provision_validator_requires_a_criterion() {
    let out = Command::new(bin())
        .arg("provision-validator")
        .output()
        .expect("run wicked-core");
    assert!(
        !out.status.success(),
        "provision-validator with no --criterion must exit non-zero"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--criterion"),
        "the error names the missing flag: {err}"
    );
}

#[test]
fn approve_validator_requires_a_pin() {
    let out = Command::new(bin())
        .arg("approve-validator")
        .output()
        .expect("run wicked-core");
    assert!(
        !out.status.success(),
        "approve-validator with no --pin must exit non-zero"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--pin"),
        "the error names the missing flag: {err}"
    );
}

#[test]
fn usage_advertises_the_validator_subcommands() {
    // An unknown subcommand prints the usage string (exit 2). Point --db at a throwaway path so the
    // actor's store open does not litter the working directory.
    let dir = scratch_dir("usage-advertises-validator-subcommands");
    let out = Command::new(bin())
        .args(["bogus-subcommand", "--db"])
        .arg(dir.join("store.db"))
        .output()
        .expect("run wicked-core");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("provision-validator") && err.contains("approve-validator"),
        "usage advertises the new subcommands: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn usage_labels_launch_as_a_stub_self_test() {
    // Seam finding #6: `launch` fabricates deterministic stub success through a gate-less path — it must
    // be plainly LABELLED as a stub self-test in the usage, never presented as a real `run`.
    let dir = scratch_dir("usage-labels-launch-stub");
    let out = Command::new(bin())
        .args(["bogus-subcommand", "--db"])
        .arg(dir.join("store.db"))
        .output()
        .expect("run wicked-core");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("launch") && err.contains("STUB"),
        "usage must label `launch` as a STUB self-test: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `domain-graph --help` must DOCUMENT, never execute — it previously wrote
/// `.wicked-estate/requirements/requirements_graph.json` into the cwd on --help.
#[test]
fn domain_graph_help_documents_and_writes_nothing() {
    let dir = scratch_dir("domain-graph-help");
    let out = Command::new(bin())
        .args(["domain-graph", "--help"])
        .current_dir(&dir)
        .output()
        .expect("run wicked-core domain-graph --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("domain-graph") && stdout.contains("--out"),
        "help must document the subcommand, got: {stdout}"
    );
    assert!(
        !dir.join(".wicked-estate").exists(),
        "--help must not write any artifact"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `rules eval --corpus <file.json>` — core#395 and core#394 through the binary: a markdown doc
/// carrying `effect: deny` ingests into a scratch store, and a corpus FILE on disk (the documented
/// `{name, samples}` shape, no import) replays against it — the bad sample is caught, the report
/// carries `rule_coverage`, and with no knowledge store the hints degrade honestly. Nothing
/// outside the temp dir is touched: HOME is redirected and the emit spool is hermetic.
#[test]
fn rules_eval_replays_a_corpus_file_against_an_effect_bearing_doc() {
    let dir = scratch_dir("rules-eval-corpus-file");
    let docs = dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(
        docs.join("git-hygiene.md"),
        "---\nid: git-hygiene\ntitle: Git hygiene\nsteering_type: development\n\
         applies_to: [build]\neffect: deny\n---\n\n## Rules\n\n\
         - POL-060 (critical): Never force-push a shared branch.\n  trigger: push\\s+--force\n",
    )
    .unwrap();
    let corpus = dir.join("corpus.json");
    std::fs::write(
        &corpus,
        r#"{"name": "ours", "samples": [
          {"id": "wicked-crew@abc1234", "description": "force-pushes main", "kind": "bad",
           "steering_type": "development",
           "signals": {"phase": "build", "tool": "Bash", "content": "git push --force origin main"}},
          {"id": "wicked-crew@def5678", "description": "pushes a fix branch", "kind": "good",
           "steering_type": "development",
           "signals": {"phase": "build", "tool": "Bash", "content": "git push origin fix/x"}}
        ]}"#,
    )
    .unwrap();
    let db = dir.join("rules.db");
    let db = db.to_str().unwrap();
    let knowledge = dir.join("no-knowledge.db");

    let ingest = Command::new(bin())
        .args(["rules", "ingest", docs.to_str().unwrap(), "--db", db])
        .env("HOME", &dir)
        .current_dir(&dir)
        .output()
        .expect("run wicked-core rules ingest");
    assert!(
        ingest.status.success(),
        "ingest failed: {}{}",
        String::from_utf8_lossy(&ingest.stdout),
        String::from_utf8_lossy(&ingest.stderr)
    );

    let out = Command::new(bin())
        .args([
            "rules",
            "eval",
            "--db",
            db,
            "--corpus",
            corpus.to_str().unwrap(),
            "--knowledge-db",
            knowledge.to_str().unwrap(),
            "--json",
        ])
        .env("HOME", &dir)
        .current_dir(&dir)
        .output()
        .expect("run wicked-core rules eval");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "eval failed: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_str(&stdout).expect("--json prints the report verbatim");
    assert_eq!(
        report["summary"],
        serde_json::json!({ "total": 2, "caught": 2, "gaps": 0, "false_positives": 0 })
    );
    assert_eq!(report["results"][0]["sample"]["id"], "wicked-crew@abc1234");
    assert_eq!(report["results"][0]["verdict"], "caught");
    assert_eq!(
        report["results"][0]["fired"],
        serde_json::json!(["POL-060"])
    );
    assert_eq!(report["results"][1]["verdict"], "caught");
    assert_eq!(report["results"][1]["fired"], serde_json::json!([]));
    assert_eq!(report["rule_coverage"]["exercised"], 1);
    assert_eq!(
        report["rule_coverage"]["unexercised"],
        serde_json::json!([])
    );
    assert_eq!(report["rule_coverage"]["recall_only"], 0);
    assert_eq!(
        report["degraded"], "facet-only",
        "no knowledge store ⇒ the honest degrade"
    );
    assert!(
        !knowledge.exists(),
        "a read path never creates a knowledge store"
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
