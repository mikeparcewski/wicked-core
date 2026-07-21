//! Integration tests for the operator APIs added in core#92:
//!   * `Core::inject_worker_message` (+ `WorkerMessageInjected` event)
//!   * `Core::reassign_unit` (+ `UnitReassigned` event)
//!
//! Pattern: follows `p2_worker_lifecycle.rs` — `spawn_with_pty_sessions`, subscribe before act,
//! `wait_for` helper. Both tests are `#[cfg(unix)]` because PTY is unavailable on Windows.

#[cfg(unix)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use wicked_core::{
        Core, CoreEvent, GateSpec, InjectTarget, PhaseRole, StageKind, StepInput, StepRunner,
        StepStatus, UnitStatus, WorkUnit,
    };

    static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_db(tag: &str) -> String {
        let seq = DB_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wicked-core-opapi-{tag}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("estate.db").to_string_lossy().into_owned()
    }

    // ── Fake CLI scripts ─────────────────────────────────────────────────────────

    /// Interactive CLI that reads prompts from stdin, sleeps briefly, then emits a stream-json
    /// result. The sleep creates a window for the inject test to call `inject_worker_message`
    /// while the session is still alive.
    fn slow_cli_invocation() -> String {
        use std::os::unix::fs::PermissionsExt;
        static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let p = PATH.get_or_init(|| {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "wicked-core-opapi-slow-cli-{}.sh",
                std::process::id()
            ));
            let script = "#!/bin/sh\n\
                while IFS= read -r line; do\n\
                  sleep 0.5\n\
                  printf '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"SLOW:%s\"}]}}\\n' \"$line\"\n\
                  printf '{\"type\":\"result\",\"result\":\"ok\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}\\n'\n\
                done\n";
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().into_owned()
        });
        format!("sh {p}")
    }

    // ── Domain helpers ──────────────────────────────────────────────────────────

    fn make_unit(ord: u32, desc: &str, invocation: &str, cli_key: &str) -> WorkUnit {
        WorkUnit {
            id: format!("u-opapi-{ord}"),
            session_id: "s-opapi".to_string(),
            ord,
            description: desc.to_string(),
            stage: StageKind::Build,
            assigned_cli: Some(cli_key.to_string()),
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

    fn make_input(run_id: &str, unit_ix: usize, attempt: u32, unit: WorkUnit) -> StepInput {
        use wicked_core::EntityMode;
        StepInput {
            run_id: run_id.to_string(),
            unit_ix,
            attempt,
            unit,
            workflow_id: "wf-opapi".to_string(),
            entity_mode: EntityMode::Shared,
            workdir: Some(std::env::temp_dir()),
            governance: None,
            prior_outputs: vec![],
        }
    }

    // ── Wait helper ─────────────────────────────────────────────────────────────

    fn wait_for(
        rx: &std::sync::mpsc::Receiver<CoreEvent>,
        label: &str,
        pred: impl Fn(&CoreEvent) -> bool,
    ) -> CoreEvent {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(ev) if pred(&ev) => return ev,
                Ok(_) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
        }
        panic!("timed out waiting for: {label}");
    }

    // ── Tests ───────────────────────────────────────────────────────────────────

    /// Verify that `inject_worker_message` with `InjectTarget::All` writes to the active PTY
    /// session and causes `WorkerMessageInjected` to fire.
    ///
    /// Setup: run one unit against a slow fake CLI; inject while it is sleeping between reading
    /// the prompt and writing the result; verify the event's fields.
    #[test]
    fn inject_all_writes_to_active_pty() {
        std::env::set_var("WICKED_MEMORY_EMBEDDER", "hash");
        let (core, runner) = Core::spawn_with_pty_sessions(unique_db("inject"));
        let events = core.subscribe();

        let inv = slow_cli_invocation();
        let unit = make_unit(1, "inject unit", &inv, "sh");
        let input = make_input("run-inject", 0, 0, unit);
        let core_clone = core.clone();

        // Run the unit in a background thread; the slow CLI gives us a ~500 ms window.
        let handle = std::thread::spawn(move || runner.run_unit(&input));

        // Wait for the session to open — at this point `run_sessions` in the actor is populated.
        wait_for(
            &events,
            "WorkerSessionStarted",
            |e| matches!(e, CoreEvent::WorkerSessionStarted { session, .. } if session == "run-inject"),
        );

        // Inject a message targeting all PTY sessions for this run.
        core_clone
            .inject_worker_message("run-inject", "operator hello", InjectTarget::All)
            .expect("inject_worker_message must not error");

        // The actor processes InjectWorkerMessage synchronously; the event arrives on the
        // subscriber channel before inject_worker_message returns (actor sends reply after emit).
        let ev = wait_for(
            &events,
            "WorkerMessageInjected",
            |e| matches!(e, CoreEvent::WorkerMessageInjected { session, .. } if session == "run-inject"),
        );

        // Verify the event fields.
        if let CoreEvent::WorkerMessageInjected {
            session,
            message,
            target,
        } = ev
        {
            assert_eq!(session, "run-inject");
            assert_eq!(message, "operator hello");
            assert_eq!(target, "all");
        } else {
            unreachable!("matched above");
        }

        // Let the unit finish (slow CLI completes after its sleep).
        let out = handle.join().expect("runner thread must not panic");
        assert_eq!(
            out.status,
            StepStatus::Ok,
            "unit must complete OK: {:?}",
            out.output
        );
    }

    /// Verify that `reassign_unit` returns an error when the run is not executing.
    #[test]
    fn reassign_unit_returns_error_when_not_executing() {
        std::env::set_var("WICKED_MEMORY_EMBEDDER", "hash");
        let (core, _runner) = Core::spawn_with_pty_sessions(unique_db("reassign-err"));

        // Non-existent run must return an error.
        let err = core
            .reassign_unit("run-nonexistent", 1, Some("b".to_string()))
            .expect_err("reassign on non-existent run must fail");
        assert!(
            err.to_string().contains("not found"),
            "unexpected error: {err}"
        );
    }

    /// Verify that `reassign_unit` redispatches the cursor unit, bumps the attempt, and emits
    /// `UnitReassigned` with the correct fields.
    ///
    /// Setup: run one unit via a background `run_unit` call (slow CLI), intercept the
    /// `WorkerSessionStarted` event to confirm the session is live, then reassign immediately.
    /// Because `ReassignUnit` validates against `session.status`, we use a `StubStepRunner`-
    /// based `spawn_with_engine` core (where the actor controls the lifecycle) to avoid the
    /// direct-call path that bypasses the actor's session state.
    ///
    /// The easiest way to get `SessionStatus::Executing` is to launch a run via `launch_run`
    /// against a slow runner that blocks long enough. We use a controllable `BlockingRunner` that
    /// holds a flag and pauses until signalled.
    #[test]
    fn reassign_unit_redispatches() {
        use std::sync::{Arc, Mutex};
        use wicked_core::{
            CoreEvent, EntityMode, HumanConfirm, LaunchSpec, StepInput, StepOutput, StepRunner,
            StepStatus,
        };
        use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
        use wicked_council::{AgenticCli, CouncilTask};

        std::env::set_var("WICKED_MEMORY_EMBEDDER", "hash");

        fn mk_cli(key: &str) -> AgenticCli {
            AgenticCli {
                key: key.into(),
                display_name: key.into(),
                binary: "unused".into(),
                headless_invocation: "unused {PROMPT}".into(),
                category: Category::default(),
                input_mode: InputMode::default(),
                version_probe: vec![],
                trust_flags: vec![],
                alt_binaries: vec![],
                confidence: Confidence::default(),
                enabled_for_council: true,
                acp: None,
                capabilities: None,
            }
        }

        // A dispatcher that always assigns to "a".
        struct AlwaysA;
        impl Dispatcher for AlwaysA {
            fn dispatch(&self, c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
                if c.key == "a" {
                    Some(Vote {
                        cli: "a".into(),
                        recommendation: "1".into(),
                        top_risk: "none".into(),
                        change_my_mind: "no".into(),
                        disqualifier: None,
                        confidence: Confidence::default(),
                        provenance: "test".into(),
                    })
                } else {
                    None
                }
            }
        }

        // A runner that blocks until `released` is set, then succeeds.
        let released: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let released_clone = released.clone();
        let reassign_seen: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let reassign_seen_clone = reassign_seen.clone();

        struct BlockingRunner {
            released: Arc<Mutex<bool>>,
        }
        impl StepRunner for BlockingRunner {
            fn run_unit(&self, i: &StepInput) -> StepOutput {
                // Block until released.
                let deadline = Instant::now() + Duration::from_secs(8);
                while Instant::now() < deadline {
                    {
                        let r = self.released.lock().unwrap();
                        if *r {
                            break;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                StepOutput {
                    run_id: i.run_id.clone(),
                    unit_ix: i.unit_ix,
                    attempt: i.attempt,
                    output: "reassigned-ok".into(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: vec![],
                    governed: false,
                }
            }
        }

        let db = unique_db("reassign-happy");
        let runner = Arc::new(BlockingRunner {
            released: released_clone,
        });
        let core = Core::spawn_with_engine(db, Arc::new(AlwaysA), runner as Arc<dyn StepRunner>);
        let events = core.subscribe();

        // Launch a single-unit run; the blocking runner will hold the session in Executing.
        core.launch_run(LaunchSpec {
            problem: "do step one".to_string(),
            clis: vec![mk_cli("a"), mk_cli("b")],
            entity_mode: EntityMode::Shared,
            session_id: "run-reassign".to_string(),
            human_confirm: HumanConfirm::default(),
            repo_ref: None,
            workflow: None,
        })
        .expect("launch_run must not fail");

        // Wait for the unit to be executing (the actor has dispatched it to the blocking runner).
        wait_for(
            &events,
            "UnitExecuting",
            |e| matches!(e, CoreEvent::UnitExecuting { session, .. } if session == "run-reassign"),
        );

        // Now the session is Executing. Reassign the cursor unit (ord=1) to CLI "b".
        core.reassign_unit("run-reassign", 1, Some("b".to_string()))
            .expect("reassign_unit must not error");

        // UnitReassigned must fire.
        let ev = wait_for(
            &events,
            "UnitReassigned",
            |e| matches!(e, CoreEvent::UnitReassigned { session, .. } if session == "run-reassign"),
        );
        if let CoreEvent::UnitReassigned {
            session,
            ord,
            attempt,
            previous_cli,
            new_cli,
        } = ev
        {
            assert_eq!(session, "run-reassign");
            assert_eq!(ord, 1);
            assert_eq!(attempt, 1, "attempt must be bumped to 1 after reassign");
            assert_eq!(previous_cli, "a");
            assert_eq!(new_cli, Some("b".to_string()));
            *reassign_seen_clone.lock().unwrap() = true;
        } else {
            unreachable!("matched above");
        }

        assert!(
            *reassign_seen.lock().unwrap(),
            "UnitReassigned event must have been observed"
        );

        // Release the blocking runner so the re-dispatched unit can complete and the run
        // finishes (avoiding a zombie thread after the test).
        *released.lock().unwrap() = true;
    }
}
