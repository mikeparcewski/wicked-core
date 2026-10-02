//! EP-K1 (DES-artifact-editor-plugins §6.2): the shipped `governance/packs/editor-defaults` pack
//! ingests VERBATIM through `wicked-core rules ingest` (the markdown doc lane AND the `rules/*.json`
//! posture lane): recall lists its ledger and its doctrine twins ED-1..ED-4, and the ingested
//! posture rules decide grants through `evaluate_editor_grants` exactly as the boot seed's do — the
//! pack in the repo is proven loadable, not proofread.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_wicked-core");

/// Pre-main: arm the hermetic emit spool (core#311), like every test binary.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only touches process env vars
/// and the filesystem via the std API.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

#[test]
fn the_editor_defaults_pack_ingests_and_recalls_its_rules() {
    let dir = std::env::temp_dir().join(format!("wc-editor-defaults-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("gov.db").to_string_lossy().into_owned();
    let pack =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("governance/packs/editor-defaults");
    let out = Command::new(BIN)
        .args(["rules", "ingest", pack.to_str().unwrap(), "--db", &db])
        .output()
        .expect("run rules ingest");
    assert!(
        out.status.success(),
        "the shipped editor-defaults pack must ingest cleanly: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = Command::new(BIN)
        .args(["rules", "recall", "--db", &db, "--json"])
        .output()
        .expect("run rules recall");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids: Vec<&str> = report["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    // Recall is the recall-only lane: the ledger (no effect) and the doctrine twins.
    for want in ["EDITOR-GRANTS", "ED-1", "ED-2", "ED-3", "ED-4"] {
        assert!(ids.contains(&want), "{want} must be recallable: {ids:?}");
    }
    // The effect-bearing posture rules are in the decide lane: they decide grants.
    let sha = "a".repeat(64);
    let ask = |id: &str, perm: &str, first_party: bool| -> (String, serde_json::Value) {
        let body = serde_json::json!({
            "editorId": id, "version": "1.0.0", "sha256": sha,
            "permissions": [perm], "firstParty": first_party,
        });
        let v: serde_json::Value = serde_json::from_str(
            &wicked_core::evaluate_editor_grants_json(&db, &body.to_string()).unwrap(),
        )
        .unwrap();
        (
            v["grants"][0]["decision"].as_str().unwrap().to_string(),
            v["grants"][0]["ruleIds"].clone(),
        )
    };
    assert_eq!(
        ask("wicked-page", "artifact.write", true),
        ("allow".into(), serde_json::json!(["EDITOR-BUILTIN"]))
    );
    assert_eq!(
        ask("wicked-page", "network.media", true),
        (
            "allow".into(),
            serde_json::json!(["EDITOR-BUILTIN-PAGE-MEDIA"])
        )
    );
    assert_eq!(
        ask("acme-notes", "artifact.read", false),
        ("allow".into(), serde_json::json!(["EDITOR-OPEN-DEFAULTS"]))
    );
    assert_eq!(ask("acme-notes", "artifact.write", false).0, "ask");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The daemon path (Copilot on #704): a fresh `Core` seeds the pack at boot, and once the actor has
/// acked a ping (what core-ts `evaluateEditorGrants` does first) the grants read finds it.
#[test]
fn a_fresh_core_answers_grants_after_its_actor_acks() {
    let dir = std::env::temp_dir().join(format!("wc-editor-boot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("estate.db").to_string_lossy().into_owned();
    let core = wicked_core::Core::spawn(db.clone());
    core.ping();
    let body = serde_json::json!({
        "editorId": "wicked-page", "version": "1.0.0", "sha256": "b".repeat(64),
        "permissions": ["artifact.write"], "firstParty": true,
    });
    let v: serde_json::Value = serde_json::from_str(
        &wicked_core::evaluate_editor_grants_json(&db, &body.to_string()).unwrap(),
    )
    .unwrap();
    assert_eq!(v["grants"][0]["decision"], "allow");
    drop(core);
    let _ = std::fs::remove_dir_all(&dir);
}
