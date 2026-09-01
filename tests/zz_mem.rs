#[test]
fn measure_rank_cost() {
    let db = "/Users/michael.parcewski/.wicked/sources/AutoGOT/.wicked/code-graph.db";
    if !std::path::Path::new(db).exists() {
        eprintln!("skip");
        return;
    }
    let t = std::time::Instant::now();
    let top = wicked_core::rank_symbols(db, 14).expect("rank");
    eprintln!(
        "RANKDONE rank_symbols(14) AutoGOT: {} results in {:?}",
        top.len(),
        t.elapsed()
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
