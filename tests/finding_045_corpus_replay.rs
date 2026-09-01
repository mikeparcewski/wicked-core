//! FINDING-045, replayed against its own corpus.
//!
//! The finding was produced by measuring real unit transcripts. The fix is measured the same way,
//! so the numbers are directly comparable rather than a fresh benchmark chosen to flatter it.
//!
//! Fixture: every discrete-path tool call (`Write`/`Edit`/`Read`/`NotebookEdit`) extracted from the
//! surviving transcripts, each labelled `inside` / `legit` / `hostile` by where it points. Shell
//! (`Bash`) calls are deliberately absent — the policy layer cannot see paths inside a shell string,
//! and pretending otherwise is the presence-shaped gate this codebase keeps finding.
use std::path::PathBuf;
use wicked_core::path_policy::{check, AllowedRoots};

#[derive(serde::Deserialize)]
struct Call {
    tool: String,
    path: String,
    write: bool,
    class: String,
}

fn corpus() -> Vec<Call> {
    let raw = include_str!("fixtures/finding_045_corpus.json");
    serde_json::from_str(raw).expect("corpus fixture parses")
}

/// The headline claim, asserted rather than described: every hostile write is refused.
///
/// This is the class that matters most. FINDING-067 is a governed worker that wrote outside its
/// boundary and deleted the platform's entire operational state.
#[test]
fn every_hostile_write_in_the_corpus_is_denied() {
    let wt = PathBuf::from("/Users/michael.parcewski/Projects/wicked-e2e/agent-frameworks/AutoGPT");
    let roots = AllowedRoots {
        write: vec![wt.clone()],
        read: vec![],
    };
    let home = PathBuf::from("/Users/michael.parcewski");

    let mut hostile_writes = 0;
    let mut allowed_through = Vec::new();
    for c in corpus().iter().filter(|c| c.class == "hostile" && c.write) {
        hostile_writes += 1;
        if check(&c.path, &roots, true, &wt, Some(&home)).is_ok() {
            allowed_through.push(format!("{} {}", c.tool, c.path));
        }
    }
    assert!(
        hostile_writes > 0,
        "fixture has no hostile writes — it would pass vacuously"
    );
    assert!(
        allowed_through.is_empty(),
        "{} of {hostile_writes} hostile writes were ALLOWED: {allowed_through:?}",
        allowed_through.len()
    );
}

/// C4, on real data: the boundary must not break ordinary work. A boundary that does gets turned
/// off, and one that is off is worse than none because it is believed.
#[test]
fn in_worktree_work_in_the_corpus_is_not_disturbed() {
    let wt = PathBuf::from("/Users/michael.parcewski/Projects/wicked-e2e/agent-frameworks/AutoGPT");
    let roots = AllowedRoots {
        write: vec![wt.clone()],
        read: vec![],
    };
    let home = PathBuf::from("/Users/michael.parcewski");

    let mut inside = 0;
    let mut wrongly_denied = Vec::new();
    for c in corpus().iter().filter(|c| c.class == "inside") {
        // Only calls that actually resolve under this worktree are in scope; the corpus spans
        // several worktrees and the others are simply not this unit's business.
        if !c.path.starts_with(wt.to_str().unwrap()) && c.path.starts_with('/') {
            continue;
        }
        inside += 1;
        if check(&c.path, &roots, c.write, &wt, Some(&home)).is_err() {
            wrongly_denied.push(c.path.clone());
        }
    }
    assert!(inside > 0, "no in-worktree calls exercised — vacuous");
    assert!(
        wrongly_denied.is_empty(),
        "{} of {inside} legitimate in-worktree calls were DENIED: {:?}",
        wrongly_denied.len(),
        &wrongly_denied[..wrongly_denied.len().min(5)]
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
