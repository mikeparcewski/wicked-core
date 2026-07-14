//! SCHEMA DRIFT-GUARD (DES-OUTGOV-005 §test-strategy). The wire schemas are vendored into
//! `wicked-core/tests/` (so the schema-validation tests run without the sibling `wicked-brain` checkout)
//! but the CANONICAL copies live in `wicked-brain/schemas/`. A silent divergence would let this repo
//! validate emitted output against a stale contract while the real consumer uses the current one. This
//! test fails if ANY vendored byte-copy drifts from its brain canonical.
//!
//! It iterates EVERY vendored schema (never a hardcoded count) and SKIPS (does not fail) when the sibling
//! `wicked-brain` repo is absent — so CI that checks out only this repo still passes, while a local/full
//! checkout catches drift.

use std::path::Path;

#[test]
fn every_vendored_schema_matches_its_brain_canonical() {
    let vendored_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    // The brain canonical dir, as a sibling of this repo (../wicked-brain/schemas).
    let brain_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("wicked-brain")
        .join("schemas");

    if !brain_dir.is_dir() {
        eprintln!(
            "schema drift-guard SKIPPED: sibling {} not present (full-checkout-only guard)",
            brain_dir.display()
        );
        return;
    }

    let mut checked = 0usize;
    for entry in std::fs::read_dir(&vendored_dir).expect("read tests/ dir") {
        let path = entry.expect("dir entry").path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) if n.ends_with(".schema.json") => n.to_string(),
            _ => continue,
        };
        let canonical = brain_dir.join(&name);
        assert!(
            canonical.is_file(),
            "vendored schema {name} has NO brain canonical at {} — either it was removed upstream or \
             vendored by mistake",
            canonical.display()
        );
        let vend = std::fs::read(&path).expect("read vendored schema");
        let canon = std::fs::read(&canonical).expect("read canonical schema");
        assert_eq!(
            vend, canon,
            "vendored tests/{name} has DRIFTED from wicked-brain/schemas/{name} — re-vendor the byte copy"
        );
        checked += 1;
    }
    assert!(
        checked >= 2,
        "expected at least the domain-model + coverage vendored schemas, found {checked}"
    );
}
