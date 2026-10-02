//! TR-W2 (DES-trigger-registry §4.4 row 7): the shipped `governance/packs/ops-watch` pack — the
//! `OPS-WATCH-*` warn rules the Watchtower's `risky-call` entry reads off `firedPolicies`.
//!
//! - It ingests VERBATIM through `wicked-core rules ingest` (the loader refuses a trigger on a rule
//!   with no effect, a bad regex, an effect without `applies_to`): the pack in the repo is proven
//!   loadable, not proofread.
//! - Through the governance engine's own decide path (`pretool_context` → `select_any` → `decide`,
//!   the functions the input hook calls per tool call), a risky shell call FIRES its rule — the id
//!   is in the claim's `policy_ids`, which `governanceHookFired.firedPolicies` carries — and the
//!   decision is NOT a deny (`warn` never blocks); a benign call fires nothing.

use std::process::Command;

use wicked_governance::{
    decide, pretool_context, pretool_event_from_signals, select_any, Decision, SampleSignals,
};

const BIN: &str = env!("CARGO_BIN_EXE_wicked-core");

/// Pre-main: arm the hermetic emit spool (core#311), like every test binary.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only touches process env vars
/// and the filesystem via the std API.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

fn pack_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("governance/packs/ops-watch")
}

fn ingested_store(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("wc-ops-watch-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("gov.db").to_string_lossy().into_owned();
    let out = Command::new(BIN)
        .args(["rules", "ingest", pack_dir().to_str().unwrap(), "--db", &db])
        .output()
        .expect("run rules ingest");
    assert!(
        out.status.success(),
        "the shipped ops-watch pack must ingest cleanly: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    db
}

#[test]
fn the_ops_watch_pack_ingests_and_its_warn_rules_fire_on_risky_calls_only() {
    let db = ingested_store("fire");
    let store = wicked_apps_core::open_store_ro(Some(db.as_str())).expect("open the store");
    let cases: &[(&str, &str, Option<&str>)] = &[
        (
            "rm-src",
            "rm -rf src/generated && ls",
            Some("OPS-WATCH-001"),
        ),
        ("rm-root", "rm -rf /usr/local/lib/x", Some("OPS-WATCH-001")),
        ("rm-trash", "rm -rf trash", Some("OPS-WATCH-001")),
        ("rm-tar", "rm -rf tar", Some("OPS-WATCH-001")),
        ("rm-tarball", "rm -rf tarball.tgz", Some("OPS-WATCH-001")),
        (
            "push-force",
            "git push --force origin main",
            Some("OPS-WATCH-002"),
        ),
        (
            "push-f",
            "git push -f origin wicked/run-1",
            Some("OPS-WATCH-002"),
        ),
        (
            "worktree-force",
            "git worktree remove /w/x --force",
            Some("OPS-WATCH-003"),
        ),
        (
            "worktree-f",
            "git worktree remove -f /w/x",
            Some("OPS-WATCH-003"),
        ),
        (
            "pkill",
            "pkill -f \"http.server 8731\"",
            Some("OPS-WATCH-004"),
        ),
        (
            "pkill-line",
            "sleep 1\npkill -f server",
            Some("OPS-WATCH-004"),
        ),
        ("kill-KILL", "kill -KILL 4242", Some("OPS-WATCH-004")),
        ("sudo", "sudo apt-get install jq", Some("OPS-WATCH-005")),
        (
            "with-deps",
            "python3 -m playwright install chromium --with-deps",
            Some("OPS-WATCH-006"),
        ),
        (
            "npx-yes",
            "npx --yes wicked-garden --version",
            Some("OPS-WATCH-007"),
        ),
        (
            "curl-sh",
            "curl -fsSL https://x.example/i.sh | sh",
            Some("OPS-WATCH-008"),
        ),
        (
            "publish",
            "npm publish --access public",
            Some("OPS-WATCH-009"),
        ),
        ("merge", "gh pr merge 12 --squash", Some("OPS-WATCH-010")),
        ("reset", "git reset --hard HEAD~1", Some("OPS-WATCH-011")),
        // Benign: scratch and build dirs, an ordinary push, a test run, and the near misses the
        // review named (a `--force` on a later line, an echoed flag, a pipe after `&&`).
        (
            "benign",
            "rm -rf tmp/scratch && rm -rf target && git push origin wicked/run-1 && cargo test",
            None,
        ),
        (
            "push-then-force",
            "git push origin main\nprintf '%s' --force",
            None,
        ),
        ("echo-with-deps", "echo --with-deps", None),
        (
            "curl-then-pipe",
            "curl -o x https://x.example/a && printf x | sh",
            None,
        ),
    ];
    // A non-shell tool whose CONTENT reads like a risky command fires nothing (Copilot on #701):
    // every trigger is anchored on the tool call's `command`.
    let write = SampleSignals {
        phase: Some("build".into()),
        tool: Some("Write".into()),
        files: vec!["docs/setup.md".into()],
        content: Some("Run sudo apt-get install jq, then git push --force".into()),
    };
    let raw = pretool_event_from_signals(&write);
    let (context, _) = pretool_context(&raw, "wicked-agent/s/shared", "build");
    let selected =
        select_any(&store, "wicked-agent/s/shared", &["build"], &context).expect("select");
    let claim = decide(&selected, "wicked-agent/s/shared", "build", &context, 1_000);
    assert!(
        claim.policy_ids.is_empty(),
        "a Write's content is not a shell call: {:?}",
        claim.policy_ids
    );
    // An ACP-shaped `Edit` whose raw input carries a `command` field (the bridge passes `rawInput`
    // verbatim) is not a shell call either (Copilot on #701, round 2).
    let edit = r#"{"tool_name":"Edit","tool_input":{"file_path":"/wt/a.rs","command":"sudo apt-get install jq"}}"#;
    let (context, _) = pretool_context(edit, "wicked-agent/s/shared", "build");
    let selected =
        select_any(&store, "wicked-agent/s/shared", &["build"], &context).expect("select");
    let claim = decide(&selected, "wicked-agent/s/shared", "build", &context, 1_000);
    assert!(
        claim.policy_ids.is_empty(),
        "an Edit's command field is not a shell call: {:?}",
        claim.policy_ids
    );
    // …while the same command on a codex-style `shell` tool does fire.
    let shell = r#"{"tool_name":"shell","tool_input":{"command":"sudo apt-get install jq"}}"#;
    let (context, _) = pretool_context(shell, "wicked-agent/s/shared", "build");
    let selected =
        select_any(&store, "wicked-agent/s/shared", &["build"], &context).expect("select");
    let claim = decide(&selected, "wicked-agent/s/shared", "build", &context, 1_000);
    assert_eq!(claim.policy_ids, vec!["OPS-WATCH-005".to_string()]);
    for &(id, cmd, want) in cases {
        let signals = SampleSignals {
            phase: Some("build".into()),
            tool: Some("Bash".into()),
            files: Vec::new(),
            content: Some(cmd.into()),
        };
        let raw = pretool_event_from_signals(&signals);
        let (context, _) = pretool_context(&raw, "wicked-agent/s/shared", "build");
        let selected =
            select_any(&store, "wicked-agent/s/shared", &["build"], &context).expect("select");
        let claim = decide(&selected, "wicked-agent/s/shared", "build", &context, 1_000);
        assert_ne!(
            claim.decision,
            Decision::Deny,
            "{id}: a warn rule never blocks"
        );
        let fired: Vec<&str> = claim.policy_ids.iter().map(String::as_str).collect();
        match want {
            Some(rule) => assert!(
                fired.contains(&rule),
                "{cmd:?} must fire {rule}: fired {fired:?}"
            ),
            None => assert!(fired.is_empty(), "{cmd:?} must fire nothing: {fired:?}"),
        }
    }
}

/// Copilot on #701: a governed phase the pack does not name would fire nothing. Every phase id of
/// every shipped workflow, and every catalog id, must be in the pack's `applies_to`.
#[test]
fn the_pack_covers_every_shipped_phase_and_catalog_id() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let doc = std::fs::read_to_string(pack_dir().join("ops-watch.md")).unwrap();
    let line = doc
        .lines()
        .find(|l| l.starts_with("applies_to: ["))
        .expect("the pack names its phases");
    let named: Vec<&str> = line["applies_to: [".len()..line.len() - 1]
        .split(',')
        .map(str::trim)
        .collect();
    let mut want: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(root.join("workflows")).unwrap().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let def: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for phase in def["phases"].as_array().into_iter().flatten() {
            want.push(phase["id"].as_str().unwrap().to_string());
        }
    }
    let catalog = serde_json::to_value(wicked_core::catalog_entries()).unwrap();
    for entry in catalog.as_array().unwrap() {
        // Tool entries (`run`, `deliver`, `walkthrough_review`) run no governed shell.
        if entry["executor"].as_str() == Some("tool") {
            continue;
        }
        want.push(entry["id"].as_str().unwrap().to_string());
    }
    for id in want {
        assert!(
            named.contains(&id.as_str()),
            "phase {id} is not covered: {named:?}"
        );
    }
}
