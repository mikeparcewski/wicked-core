//! POL-2101 is boot-seeded (operator decision 2026-10-12): a FRESH daemon store recalls it after
//! the engine boots, with no `rules ingest`, and the seeded row is the one an ingest of
//! `governance/packs/stack-builtins` writes (same statement, phases and provenance ref).

use wicked_governance::{ingest_from, recall_rules, MarkdownAdapter, RuleQuery};

/// Pre-main: arm the hermetic emit spool (core#311), like every test binary.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only touches process env vars
/// and the filesystem via the std API.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

#[test]
fn a_fresh_store_recalls_pol_2101_after_boot() {
    let dir = std::env::temp_dir().join(format!("wc-pol2101-boot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("estate.db").to_string_lossy().into_owned();

    // Boot: the actor seeds before it serves, so an acked ping means the seed has run.
    let core = wicked_core::Core::spawn(db.clone());
    core.ping();
    drop(core);

    let store = wicked_apps_core::open_store(Some(&db)).unwrap();
    let dev = recall_rules(
        &store,
        &RuleQuery {
            steering_type: Some("development".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let seeded = dev.iter().find(|r| r.id == "POL-2101").unwrap_or_else(|| {
        panic!(
            "a fresh store recalls POL-2101 after boot: {:?}",
            dev.iter().map(|r| &r.id).collect::<Vec<_>>()
        )
    });
    assert_eq!(seeded.applies_to, ["build", "review"]);

    let pack = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("governance/packs/stack-builtins");
    let ingested = ingest_from(&MarkdownAdapter::new(pack)).unwrap();
    let doc = ingested.iter().find(|r| r.id == "POL-2101").unwrap();
    assert_eq!(seeded.statement, doc.statement);
    assert_eq!(seeded.provenance.reference, doc.provenance.reference);
    let _ = std::fs::remove_dir_all(&dir);
}
