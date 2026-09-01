//! SCHEMA LIFT-FIDELITY GUARD (AW-2 / arch-R10). The governance schemas were re-homed from the
//! retired `wicked-brain` repo into `crates/wicked-governance/schemas/` — the LIVE OWNER — lifted
//! byte-for-byte at bundle VERSION 1.1.0. The archive is frozen (read-only, never modified), so
//! this guard proves the lift stays faithful: while the owner bundle's `VERSION` still equals the
//! archived bundle's, every schema the archive holds must exist in the owner dir byte-identical.
//!
//! Once the owner legitimately evolves past the archived version (a bundle VERSION bump), the
//! byte-compare no longer applies and the guard skips — the archive is history, not the contract.
//! It also SKIPS (never fails) when the sibling `wicked-brain` checkout is absent, so CI that
//! checks out only this repo still passes, while a local/full checkout catches an unfaithful lift.
//!
//! Cross-repo sync (garden's vendored copy vs this owner dir) is enforced on the garden side:
//! `wicked-garden/tests/domain/test_schema_vendor_pin.py`.

use std::path::Path;

#[test]
fn owner_schemas_match_the_frozen_brain_archive_at_the_lifted_version() {
    let owner_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("crates")
        .join("wicked-governance")
        .join("schemas");
    // The frozen archive, as a sibling of this repo (../wicked-brain/schemas).
    let brain_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("wicked-brain")
        .join("schemas");

    if !brain_dir.is_dir() {
        eprintln!(
            "schema lift-fidelity guard SKIPPED: frozen archive {} not present (full-checkout-only guard)",
            brain_dir.display()
        );
        return;
    }

    let read_version = |dir: &Path| -> String {
        std::fs::read_to_string(dir.join("VERSION"))
            .unwrap_or_else(|e| panic!("read {}/VERSION: {e}", dir.display()))
            .trim()
            .to_string()
    };
    let owner_version = read_version(&owner_dir);
    let archive_version = read_version(&brain_dir);
    if owner_version != archive_version {
        eprintln!(
            "schema lift-fidelity guard SKIPPED: owner bundle {owner_version} has moved past the \
             frozen archive's {archive_version} — the archive is history, not the contract"
        );
        return;
    }

    let mut checked = 0usize;
    for entry in std::fs::read_dir(&brain_dir).expect("read archive schemas dir") {
        let path = entry.expect("dir entry").path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) if n.ends_with(".schema.json") => n.to_string(),
            _ => continue,
        };
        let owner = owner_dir.join(&name);
        assert!(
            owner.is_file(),
            "archived schema {name} has NO owner copy at {} — the lift dropped a file",
            owner.display()
        );
        let own = std::fs::read(&owner).expect("read owner schema");
        let arch = std::fs::read(&path).expect("read archived schema");
        assert_eq!(
            own, arch,
            "owner crates/wicked-governance/schemas/{name} differs from the frozen archive at the \
             SAME bundle version {owner_version} — an edit without a VERSION bump, or an unfaithful lift"
        );
        checked += 1;
    }
    assert!(
        checked == 4,
        "expected the 4 archived governance schemas, found {checked}"
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
