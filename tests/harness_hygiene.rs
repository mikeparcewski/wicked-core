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

/// Every test binary in this suite must arm the hermetic emit spool BEFORE main (core#311).
///
/// Engine paths under test — gate transitions, conformance recording, rule lifecycle — fire
/// coarse fire-and-forget `wicked.*` emissions as a side effect. With no shared store configured
/// (the normal test condition) those spool to the outbox, and the default outbox is the
/// operator's REAL `~/.something-wicked/wicked-apps/emit-outbox.ndjson` replay queue. Before the
/// fix, one full `cargo test` run appended ~300KB of junk events there — on pristine main.
///
/// Arming must be PRE-MAIN (`#[ctor::ctor(unsafe)]`): a per-test call cannot guarantee order —
/// tests run on parallel threads, so one unarmed test can spool before an armed one sets the
/// override. This scan fails the suite the moment a test source (this file included) lacks the
/// arming block, so the guarantee survives new test files.
#[test]
fn every_test_binary_arms_the_hermetic_emit_spool() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut entries: Vec<_> = fs::read_dir(&dir)
        .expect("the tests directory is readable")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    entries.sort();
    assert!(
        entries.len() > 10,
        "found {} test sources in {} — the scan is not reading the suite",
        entries.len(),
        dir.display()
    );

    let mut missing: Vec<String> = Vec::new();
    for path in entries {
        let src = fs::read_to_string(&path).expect("a test source is readable");
        let armed = src.contains("#[ctor::ctor(unsafe)]")
            && src.contains("wicked_apps_core::emit::hermetic_test_spool()");
        if !armed {
            missing.push(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    assert!(
        missing.is_empty(),
        "these test binaries never arm the hermetic emit spool, so any emission they trigger \
         spools to the operator's real ~/.something-wicked replay queue (core#311). Add the \
         `#[ctor::ctor(unsafe)] fn arm_hermetic_emit_spool()` block: {missing:?}"
    );
}

/// A spool triggered by this suite lands in the armed per-process temp outbox — never in the
/// real home (core#311). The other half of the guarantee: the scan above proves every binary
/// arms; this proves arming actually redirects a REAL emission through the seam.
#[test]
fn a_forced_spool_lands_in_the_armed_temp_outbox_never_in_the_real_home() {
    use wicked_apps_core::emit::{deadletter_path, emit_event, EmitEvent, DEADLETTER_ENV};
    use wicked_apps_core::ESTATE_DB_ENV;

    // Pre-main arming set the override before any test thread started.
    let armed = std::path::PathBuf::from(
        std::env::var_os(DEADLETTER_ENV).expect("the pre-main ctor armed the spool override"),
    );
    let resolved = deadletter_path().expect("the spool path resolves");
    assert_eq!(
        resolved, armed,
        "the seam must resolve the spool to the armed override"
    );
    assert!(
        armed.starts_with(std::env::temp_dir()),
        "the armed spool must live under the system temp dir, got {}",
        armed.display()
    );

    // The real replay queue the arming exists to protect.
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .expect("a home directory resolves on test hosts");
    let real = home
        .join(".something-wicked")
        .join("wicked-apps")
        .join("emit-outbox.ndjson");
    assert_ne!(
        resolved, real,
        "the armed spool must not be the real outbox"
    );
    let real_existed = real.exists();

    // Force a REAL spool through the seam. `ESTATE_DB_ENV` is removed for the duration so the
    // emit cannot write to an operator's live estate store either; no other test in THIS binary
    // emits or reads that var, so the unset window cannot leak a parallel emission.
    let marker = format!(
        "core311-guard-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let prev = std::env::var_os(ESTATE_DB_ENV);
    // SAFETY: process-global env writes; this binary's only env-mutating test (see above).
    unsafe { std::env::remove_var(ESTATE_DB_ENV) };
    let stored = emit_event(&EmitEvent::new(
        "wicked.core.outbox.probed",
        "wicked-core",
        "core.harness",
        serde_json::json!({ "marker": marker }),
    ));
    if let Some(v) = prev {
        // SAFETY: see above.
        unsafe { std::env::set_var(ESTATE_DB_ENV, v) };
    }

    assert!(!stored, "no shared store configured => spooled, not stored");
    let body = fs::read_to_string(&resolved).expect("the armed spool file exists after a spool");
    assert!(
        body.contains(&marker),
        "the forced spool must land in the armed temp outbox {}",
        resolved.display()
    );
    if !real_existed {
        assert!(
            !real.exists(),
            "the suite CREATED the operator's real outbox {} — a test is spooling to the real \
             home (core#311)",
            real.display()
        );
    }
}

/// core#311, adjacent organ: the SAME pre-main arming block also points the engine's persistent
/// worker config home (`WICKED_WORKER_HOME`, resolved by the ACP spawn path) at a per-process
/// temp base — `emit::hermetic_test_spool` arms both. A test binary that reaches a real start
/// without this would REWRITE the operator's real `~/.wicked-worker/claude/settings.json` and
/// DELETE its executable-config entries (hooks/, plugins/, …) on every `cargo test` run. This
/// asserts the arming reached THIS integration binary; the resolution-level proof lives in the
/// lib binary (`worker_home_resolution_lands_in_the_armed_temp_base_never_the_real_home`).
#[test]
fn the_worker_home_override_is_armed_pre_main_too() {
    let armed = std::path::PathBuf::from(
        std::env::var_os(wicked_apps_core::spawn::WORKER_HOME_ENV)
            .expect("the pre-main ctor armed the worker-home override"),
    );
    assert!(
        armed.starts_with(std::env::temp_dir()),
        "the armed worker home must live under the system temp dir, got {}",
        armed.display()
    );
    if let Some(home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
    {
        assert!(
            !armed.starts_with(home.join(".wicked-worker")),
            "the armed worker home must never be the operator's real ~/.wicked-worker"
        );
    }
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
