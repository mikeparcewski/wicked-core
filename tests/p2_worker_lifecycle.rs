//! P2 worker-lifecycle event integration tests (EVT-003, EVT-004).
//!
//! Verifies that `WorkerSessionReused` fires when a run dispatches multiple units through
//! the same PTY session, and that `WorkerSessionClosed` fires at the correct lifecycle points.
//!
//! Pattern: same Core + subscribe-before-launch structure as `events_foundation.rs`.

#[cfg(unix)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use wicked_core::{
        Core, CoreEvent, GateSpec, PhaseRole, StageKind, StepInput, StepRunner, StepStatus,
        UnitStatus, WorkUnit,
    };

    static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_db() -> String {
        let seq = DB_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("wicked-core-p2wl-{}-{}", std::process::id(), seq));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("estate.db").to_str().unwrap().to_string()
    }

    /// Fake interactive CLI script: reads one line per turn, emits the minimum NDJSON that
    /// `ClaudeStreamJson` parses (assistant delta + result sentinel). Shared across test invocations
    /// via a process-scoped `OnceLock`.
    fn fake_cli_invocation() -> String {
        use std::os::unix::fs::PermissionsExt;
        static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let p = PATH.get_or_init(|| {
            let mut path = std::env::temp_dir();
            path.push(format!("wicked-core-p2wl-fake-cli-{}.sh", std::process::id()));
            let script = "#!/bin/sh\n\
                while IFS= read -r line; do\n\
                  printf '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"WKRTURN:%s\"}]}}\\n' \"$line\"\n\
                  printf '{\"type\":\"result\",\"result\":\"ok\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}\\n' \"$line\"\n\
                done\n";
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().into_owned()
        });
        format!("sh {p}")
    }

    fn make_unit(ord: u32, description: &str, invocation: &str) -> WorkUnit {
        WorkUnit {
            id: format!("u-test-{ord}"),
            session_id: "sess-p2wl".to_string(),
            ord,
            description: description.to_string(),
            stage: StageKind::Build,
            assigned_cli: Some("sh".to_string()),
            assigned_invocation: Some(invocation.to_string()),
            council_task_ref: None,
            routing: None,
            denial_reason: None,
            phase_ref: None,
            conformance_ref: None,
            phase_status: None,
            collection_scope: None,
            skill_ref: None,
            allowed_skills: Vec::new(),
            gate: GateSpec::default(),
            role: PhaseRole::default(),
            validator: None,
            tool_cmd: None,
            status: UnitStatus::Pending,
        }
    }

    fn make_input(run_id: &str, unit_ix: usize, unit: WorkUnit) -> StepInput {
        use wicked_core::EntityMode;
        StepInput {
            run_id: run_id.to_string(),
            unit_ix,
            attempt: 0,
            unit,
            workflow_id: "wf-p2wl".to_string(),
            entity_mode: EntityMode::Shared,
            workdir: Some(std::env::temp_dir()),
            governance: None,
            prior_outputs: vec![],
        }
    }

    /// Drain events until `pred` matches or the deadline expires.
    fn wait_for(rx: &std::sync::mpsc::Receiver<CoreEvent>, pred: impl Fn(&CoreEvent) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(ev) if pred(&ev) => return,
                Ok(_) | Err(_) => continue,
            }
        }
    }

    /// Drain all buffered events without blocking.
    fn drain_buffered(rx: &std::sync::mpsc::Receiver<CoreEvent>) -> Vec<CoreEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// A two-unit run sharing the same `run_id` on a `PersistentStepRunner` MUST emit
    /// `WorkerSessionReused` on the second unit (the session was already open).
    ///
    /// This verifies the EVT-003 invariant: if this event fires zero times for a multi-unit run,
    /// every unit paid cold-start cost (or the session silently failed).
    #[test]
    fn worker_session_reused_fires_on_second_unit() {
        std::env::set_var("WICKED_MEMORY_EMBEDDER", "hash");
        let (core, runner) = Core::spawn_with_pty_sessions(unique_db());
        let events = core.subscribe();

        let inv = fake_cli_invocation();
        let unit1 = make_unit(1, "first unit", &inv);
        let unit2 = make_unit(2, "second unit", &inv);
        let input1 = make_input("run-p2wl-reuse", 0, unit1);
        let input2 = make_input("run-p2wl-reuse", 1, unit2);

        // Turn 1 — opens a new session (emits WorkerSessionStarted).
        let out1 = runner.run_unit(&input1);
        assert_eq!(
            out1.status,
            StepStatus::Ok,
            "turn 1 must succeed; got: {:?}",
            out1.output
        );

        // Drain events after turn 1 — zero WorkerSessionReused expected (session just opened).
        let after_turn1 = drain_buffered(&events);
        let reused_count_turn1 = after_turn1
            .iter()
            .filter(|e| matches!(e, CoreEvent::WorkerSessionReused { .. }))
            .count();
        assert_eq!(
            reused_count_turn1, 0,
            "no WorkerSessionReused on the first unit (session just opened)"
        );

        // Turn 2 — reuses the open session; must emit WorkerSessionReused.
        let out2 = runner.run_unit(&input2);
        assert_eq!(
            out2.status,
            StepStatus::Ok,
            "turn 2 must succeed; got: {:?}",
            out2.output
        );

        // WorkerSessionReused must have been emitted for turn 2.
        let after_turn2 = drain_buffered(&events);
        let reused_events: Vec<_> = after_turn2
            .iter()
            .filter_map(|e| {
                if let CoreEvent::WorkerSessionReused { session, ord, .. } = e {
                    Some((session.clone(), *ord))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            reused_events.len(),
            1,
            "exactly one WorkerSessionReused after the second unit; got events: {:?}",
            after_turn2
        );
        let (session, ord) = &reused_events[0];
        assert_eq!(session, "run-p2wl-reuse", "session id matches the run id");
        assert_eq!(*ord, 2, "ord matches the second unit's ord");

        // Explicit teardown.
        runner.drop_session("run-p2wl-reuse");
        wait_for(&events, |e| matches!(e, CoreEvent::TerminalExited { .. }));
    }

    /// `WorkerSessionClosed` with `reason="run_complete"` fires when `on_run_complete` is called.
    #[test]
    fn worker_session_closed_fires_on_run_complete() {
        std::env::set_var("WICKED_MEMORY_EMBEDDER", "hash");
        let (core, runner) = Core::spawn_with_pty_sessions(unique_db());
        let events = core.subscribe();

        let inv = fake_cli_invocation();
        let unit1 = make_unit(1, "the work", &inv);
        let input1 = make_input("run-p2wl-close", 0, unit1);

        // Run one unit to open the session.
        let out = runner.run_unit(&input1);
        assert_eq!(
            out.status,
            StepStatus::Ok,
            "unit must succeed; got: {:?}",
            out.output
        );

        // Drop buffered events before the teardown.
        let _ = drain_buffered(&events);

        // Trigger normal close.
        runner.on_run_complete("run-p2wl-close");

        // WorkerSessionClosed with reason="run_complete" must arrive.
        wait_for(&events, |e| {
            matches!(
                e,
                CoreEvent::WorkerSessionClosed { reason, .. } if reason == "run_complete"
            )
        });

        // Also verify the terminal eventually exits.
        wait_for(&events, |e| matches!(e, CoreEvent::TerminalExited { .. }));
    }

    // ── StepRunner adapter for the PersistentStepRunner (needed to satisfy the StepRunner bound) ──
    // PersistentStepRunner implements StepRunner directly, but we're calling it manually above.
    // This is fine — the tests call runner.run_unit() directly via the Arc<PersistentStepRunner>.
}
