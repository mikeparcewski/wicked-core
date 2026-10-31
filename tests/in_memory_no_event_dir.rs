//! An in-memory store has no sidecar anchor: it must deliver events live and create NO
//! `:memory:.events/` directory. Its own test binary (one test) so it can own the process cwd.

use std::time::Duration;

use wicked_core::{Core, EntityMode, HumanConfirm, LaunchSpec};

#[test]
fn an_in_memory_core_creates_no_event_log_directory() {
    let cwd = std::env::temp_dir().join(format!("wicked-memdb-noevents-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cwd);
    std::fs::create_dir_all(&cwd).unwrap();
    let prior = std::env::current_dir().unwrap();
    std::env::set_current_dir(&cwd).unwrap();

    let core = Core::spawn(":memory:");
    let events = core.subscribe();
    let _ = core.launch_run(LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Do one thing.".into(),
        clis: Vec::new(),
        entity_mode: EntityMode::Shared,
        session_id: "mem-noevents".into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: None,
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
        power: false,
        voice: None,
        deliverables: Vec::new(),
        qe_acceptance: Default::default(),
    });
    // Live delivery still works without a log.
    events
        .recv_timeout(Duration::from_secs(10))
        .expect("an in-memory core still delivers events to subscribers");
    wicked_core::event_log::flush();

    assert!(core.run_events("mem-noevents").is_empty());
    drop(core);
    let stray: Vec<_> = std::fs::read_dir(&cwd)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    assert!(
        stray.is_empty(),
        "no sidecar directory expected, saw {stray:?}"
    );
    std::env::set_current_dir(prior).unwrap();
    let _ = std::fs::remove_dir_all(&cwd);
}

/// Arm the hermetic emit spool before `main`, as every test binary does (core#311, core#749).
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
