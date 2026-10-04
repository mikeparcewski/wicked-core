//! core#709 — `rules ingest` of a BOOT-SEEDED pack (`mcp-defaults`, `editor-defaults`) keeps the
//! operator's state on the rules the seed owns: an approved token in the pack's approvals ledger
//! (`MCP-FIRST-USE` / `EDITOR-GRANTS` `excludes`) and a retired posture row both survive the
//! re-ingest. Before the fix the JSON lane re-registered every rule through `register_rule`, which
//! keeps only `created_at` — so the ingest revoked every approval and un-retired every posture row.

use std::process::Command;
use wicked_apps_core::{FromNode, GraphRead};

const BIN: &str = env!("CARGO_BIN_EXE_wicked-core");

/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only touches process env vars
/// and the filesystem via the std API.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

fn rule(db: &str, id: &str) -> wicked_governance::ConformanceRule {
    let store = wicked_apps_core::open_store(Some(db)).unwrap();
    let sym = wicked_apps_core::synthetic_symbol(wicked_governance::CONFORMANCE_RULE, id);
    let node = store
        .get_node(&sym)
        .unwrap()
        .unwrap_or_else(|| panic!("{id} is stored"));
    wicked_governance::ConformanceRule::from_node(&node).unwrap()
}

#[test]
fn a_reingested_seeded_pack_keeps_approvals_and_retirements() {
    for (pack, ledger, token, posture) in [
        (
            "mcp-defaults",
            "MCP-FIRST-USE",
            "mcp:jira",
            "MCP-POSTURE-WRITE",
        ),
        (
            "editor-defaults",
            "EDITOR-GRANTS",
            "editor:acme-notes:artifact.write",
            "EDITOR-OPEN-DEFAULTS",
        ),
    ] {
        let dir = std::env::temp_dir().join(format!("wc-reingest-{pack}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("estate.db").to_string_lossy().into_owned();

        // 1. Boot seeds both packs (insert-only).
        let core = wicked_core::Core::spawn(db.clone());
        core.ping();
        drop(core);

        // 2. The operator approves a token and retires a posture row.
        {
            let mut store = wicked_apps_core::open_store(Some(&db)).unwrap();
            let mut l = rule(&db, ledger);
            l.excludes.push(token.to_string());
            wicked_apps_core::GraphWrite::upsert_nodes(
                &mut store,
                &[wicked_apps_core::ToNode::to_node(&l)],
            )
            .unwrap();
            assert!(wicked_governance::retire_rule(&mut store, posture).unwrap());
        }
        assert!(rule(&db, ledger).excludes.iter().any(|e| e == token));
        assert!(rule(&db, posture).retired);

        // 3. The pack is ingested again.
        let pack_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("governance/packs")
            .join(pack);
        let out = Command::new(BIN)
            .args(["rules", "ingest", pack_dir.to_str().unwrap(), "--db", &db])
            .output()
            .expect("run rules ingest");
        assert!(
            out.status.success(),
            "{pack}: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        // 4. The approval and the retirement survive.
        assert!(
            rule(&db, ledger).excludes.iter().any(|e| e == token),
            "{pack}: re-ingest revoked the approval of {token}: {:?}",
            rule(&db, ledger).excludes
        );
        assert!(
            rule(&db, posture).retired,
            "{pack}: re-ingest un-retired {posture}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
