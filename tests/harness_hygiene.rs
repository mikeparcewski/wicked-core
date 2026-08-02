//! Properties the test suite itself must hold for its results to mean anything.
//!
//! A suite that fails for reasons unrelated to the code under test cannot certify anything, and
//! the failures it produces cost more to diagnose than the bugs they hide. The checks here are
//! about the harness, not the product.

use std::fs;
use std::path::Path;

/// How much source to inspect after a `temp_dir()` call before deciding what it built.
/// Comfortably past the longest multi-line `format!` in the suite.
const WINDOW: usize = 260;

/// Fewest fixtures the scan must find before its verdict means anything.
/// Well under the ~30 present, but far above the zero a broken scanner would report.
const MIN_FIXTURES: usize = 20;

/// Every fixture built under the shared system temp dir must be scoped to the process that built
/// it, normally with `std::process::id()`.
///
/// These fixtures open with `remove_dir_all` on a fixed path, so two test processes sharing that
/// path delete each other's git repo and database mid-run. It does not happen inside one
/// `cargo test --all` — each binary runs once — but it happens the moment the suite runs in two
/// terminals, a watch-mode runner re-triggers during a run, or CI shards across jobs. Reproduced
/// before the fix: four concurrent `p3_repo` processes, two failed, each with its run driven to
/// `Failed` because another process had deleted the repo the worktree was to be added to.
///
/// The failure is worth preventing rather than debugging: it surfaces as a product assertion
/// ("the run completes"), pointing at the engine rather than at the harness.
#[test]
fn temp_fixtures_are_scoped_to_the_process() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut fixtures = 0usize;
    let mut unscoped: Vec<String> = Vec::new();

    let mut entries: Vec<_> = fs::read_dir(&dir)
        .expect("the tests directory is readable")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        // This file quotes the very patterns it looks for, so scanning it flags itself.
        .filter(|p| p.file_name().is_some_and(|n| n != "harness_hygiene.rs"))
        .collect();
    entries.sort();
    assert!(
        entries.len() > 10,
        "found {} test sources in {} — the scan is not reading the suite",
        entries.len(),
        dir.display()
    );

    for path in entries {
        let src = fs::read_to_string(&path).expect("a test source is readable");
        let file = path.file_name().unwrap_or_default().to_string_lossy();
        for (idx, _) in src.match_indices("temp_dir()") {
            let end = (idx + WINDOW).min(src.len());
            // Respect char boundaries: these sources contain non-ASCII in comments.
            let mut end = end;
            while end > idx && !src.is_char_boundary(end) {
                end -= 1;
            }
            let window = &src[idx..end];
            // A bare `temp_dir()` passed as an argument builds nothing and cannot collide; only a
            // path assembled from it with `join`/`push` becomes a fixture that gets wiped.
            let builds_a_path = window.starts_with("temp_dir().join(")
                || window.starts_with("temp_dir()\n")
                || window.contains(".push(");
            if !builds_a_path {
                continue;
            }
            fixtures += 1;
            if !window.contains("process::id()") {
                let line = src[..idx].matches('\n').count() + 1;
                unscoped.push(format!("{file}:{line}"));
            }
        }
    }

    assert!(
        fixtures >= MIN_FIXTURES,
        "scanned only {fixtures} temp fixtures — the scan is not finding them, so a pass here \
         would be vacuous"
    );
    assert!(
        unscoped.is_empty(),
        "these temp fixtures are not process-scoped, so two concurrent test processes will wipe \
         each other's state and the failure will look like a product defect: {unscoped:?}"
    );
}
