use super::*;
use crate::validator::SandboxLevel;

pub(crate) fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "wicked-repochecks-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The tree's check-scratch leaf in this test binary's (hermetic) check cache.
fn leaf_of(wt: &Path) -> PathBuf {
    scratch_dir_for(&check_cache_root().unwrap(), wt, None)
}

/// The floor created nothing named `tmp/wicked-checks` (the pre-fix scratch) in the tree.
fn assert_no_worktree_scratch(wt: &Path) {
    let old = wt
        .join(crate::worktree_guard::ENGINE_SCRATCH_DIR)
        .join("wicked-checks");
    assert!(
        std::fs::symlink_metadata(&old).is_err(),
        "the floor wrote its scratch into the worktree: {}",
        old.display()
    );
}

/// The boundary a test floor arms: the worktree and the tree's check-scratch leaf (which
/// lives in the engine's check cache, outside the worktree).
pub(crate) fn sandbox_for(wt: &Path) -> WorkerSandbox {
    let leaf = scratch_dir_for(&check_cache_root().unwrap(), wt, None);
    std::fs::create_dir_all(&leaf).unwrap();
    crate::validator::detect_floor_sandbox(&[wt.to_path_buf(), leaf])
}

/// core#1627: deciding runs must stop a floor with reused checks, including when a red
/// base cannot be separated. Exclusion keeps these retries inside `decide`, not early.
#[cfg(unix)]
#[test]
fn core_1627_deciding_enospc_is_terminal_after_reuse() {
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let _load = test_seam::pin(8.0, 4);
    let _exclude = test_seam::exclude(true);
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_FREE_BYTES.with(|c| c.set(u64::MAX));
            match &self.0 {
                Some(v) => std::env::set_var("WICKED_FLOOR_MIN_FREE_GB", v),
                None => std::env::remove_var("WICKED_FLOOR_MIN_FREE_GB"),
            }
        }
    }
    let _restore = Restore(std::env::var_os("WICKED_FLOOR_MIN_FREE_GB"));
    std::env::set_var("WICKED_FLOOR_MIN_FREE_GB", "15");
    // (tag, the base inventory is precise, the spawn of `decide` low space starts at, a retry
    // runs, the head's results run is the one refused — core#1767)
    for (tag, precise, low_at, on_retry, results_refused) in [
        ("unseparated-retry", false, 1, true, false),
        ("excluded-retry", true, 5, true, false),
        ("excluded-head", true, 4, false, false),
        ("results-head", true, 3, false, true),
    ] {
        let wt = scratch(tag);
        std::fs::write(
                wt.join("check.sh"),
                "case \"$*\" in *--list*)\n\
                   echo 'new: test'; case \"$*\" in *--skip=old*) ;; *) echo 'old: test';; esac; exit 0;;\n\
                 esac\n\
                 echo run >> attempts\n\
                 echo 'running 2 tests'\n\
                 echo 'No space left on device (os error 28)' >&2\n\
                 exit 101\n",
            )
            .unwrap();
        let scratch = CheckScratch::prepare(&wt).unwrap();
        let sandbox = WorkerSandbox {
            wrapper: Vec::new(),
            level: SandboxLevel::BestEffort,
            downgrade_reason: None,
        };
        let check = RepoCheck {
            name: "test".into(),
            argv: s(&["sh", "check.sh"]),
            source: "fixture".into(),
            timeout_s: Some(30),
            dir: String::new(),
        };
        TEST_FREE_BYTES.with(|c| c.set(40 << 30));
        let mut head = run_one(&wt, &check, &sandbox, &scratch, Tree::Head);
        assert!(head.spawn_error.is_none(), "{head:?}");
        let old = TestId {
            id: "old".into(),
            q: String::new(),
        };
        let side = BaseSide {
            base: BaseRun {
                head: "base".into(),
                cached: false,
                run: Some(head.clone()),
                error: None,
            },
            toc: Some(Toc {
                ids: vec![old.clone()],
                failing: vec![old.clone()],
                alone: vec![(old, true)],
                incomplete: (!precise).then(|| "the base list was incomplete".into()),
                ..Toc::default()
            }),
        };
        let guide: GuideCheck = serde_json::from_value(serde_json::json!({
            "name": "test", "run": check.argv, "failedExit": [101],
            "results": {"kind": "text"},
            "parser": {"failed": ["^FAIL (?P<id>.+)$"], "count": "^running (?P<n>\\d+) tests$"},
            "list": {"run": ["sh", "check.sh", "--list"],
                     "item": "^(?P<id>.+): test$"},
            "alone": {"run": ["sh", "check.sh", "{id}"]},
            "deselect": {"append": ["--skip={id}"]}
        }))
        .unwrap();
        let seed = CreatorSeed {
            record: CreatorFloorRecord {
                floor_policy: FLOOR_POLICY_VERSION,
                tree: "tree".into(),
                base: "base".into(),
                ord: 1,
                attempt: 0,
                phase: Some("fix".into()),
                sandbox_level: "best_effort".into(),
                exclude: true,
                checks: Vec::new(),
            },
            reused: vec!["lint".into()],
            fresh: Vec::new(),
        };
        assert!(seed.set_aside_by(&check, &head).is_some());
        // Low space from the `n`th spawn of `decide` on: the head inventory (1), the deselected
        // head inventory (2), the head's results run (3, core#1767), the excluded run (4) and
        // its retry (5). Inventory probes also use the existing spawn seam.
        fn low_from(n: u32) {
            if n <= 1 {
                test_seam::before_spawn(|| TEST_FREE_BYTES.with(|c| c.set(1 << 30)));
            } else {
                test_seam::before_spawn(move || low_from(n - 1));
            }
        }
        low_from(low_at);
        decide(
            &wt,
            None,
            &check,
            &mut head,
            Some(side),
            None,
            &Ok(&guide),
            true,
            &sandbox,
            &scratch,
            None,
            false,
        );
        assert!(head.environment_failure(), "{head:?}");
        if results_refused {
            // The refusal is the check's, though no deciding run ran after it.
            assert!(head.reruns.is_empty(), "{head:?}");
            assert!(
                head.earlier_passes.iter().any(|(l, _)| l == "head results"),
                "{head:?}"
            );
        } else {
            let deciding = head.reruns.last().expect("a deciding run ran");
            assert!(deciding.environment_failure(), "{head:?}");
            assert_eq!(head.spawn_error, deciding.spawn_error, "{head:?}");
        }
        assert_eq!(head.outcome(), OUTCOME_COULD_NOT_RUN, "{head:?}");
        let decision = head.decision.as_ref().unwrap();
        assert_eq!(decision.outcome, OUTCOME_COULD_NOT_RUN, "{head:?}");
        assert_eq!(head.classification, None, "{head:?}");
        assert!(
            seed.set_aside_by(&check, &head).is_none(),
            "no restart: {head:?}"
        );
        assert_eq!(head.retry.is_some(), on_retry, "{head:?}");
        assert_eq!(
            head.reruns.len(),
            usize::from(precise && !results_refused) + usize::from(on_retry)
        );
        for rerun in &head.reruns {
            for log in &rerun.logs {
                assert!(decision.log_refs.contains(log), "{decision:?}");
            }
        }
        assert_eq!(
            std::fs::read_to_string(wt.join("attempts"))
                .unwrap()
                .lines()
                .count(),
            // The head, (precise) its results run, the excluded run unless the results run was
            // refused (it ran and met the low space), and the retry.
            1 + usize::from(precise)
                + usize::from(precise && !results_refused)
                + usize::from(on_retry)
        );
        drop(scratch);
        let _ = std::fs::remove_dir_all(&wt);
    }
}

/// core#1627: an ENOSPC from the engine's log writer is itself corroboration, even for
/// exit zero and with ample disk space. Preserve the original log error for diagnosis.
#[cfg(unix)]
#[test]
fn core_1627_log_write_enospc_is_an_environment_failure() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let wt = scratch("log-write-enospc");
    let scratch = CheckScratch::prepare(&wt).unwrap();
    let sandbox = WorkerSandbox {
        wrapper: Vec::new(),
        level: SandboxLevel::BestEffort,
        downgrade_reason: None,
    };
    for exit in [0, 101] {
        for (write_error, no_space) in [
            (std::io::Error::from_raw_os_error(28), true),
            (
                std::io::Error::new(std::io::ErrorKind::StorageFull, "injected full log device"),
                true,
            ),
            (std::io::Error::other("No space left on device"), false),
        ] {
            let check = RepoCheck {
                name: "test".into(),
                argv: s(&["sh", "-c", &format!("echo ordinary-output; exit {exit}")]),
                source: "fixture".into(),
                timeout_s: Some(30),
                dir: String::new(),
            };
            let detail = write_error.to_string();
            crate::floor_log::fail_next_write(write_error);
            let mut run = run_one(&wt, &check, &sandbox, &scratch, Tree::Head);
            assert_eq!(run.exit_code, Some(exit), "{run:?}");
            assert_eq!(run.environment_failure(), no_space, "{run:?}");
            let error = run.spawn_error.as_deref().unwrap();
            if no_space {
                assert!(error.contains("the disk filled during `test`"), "{error}");
                assert!(error.contains("Free disk space, then retry"), "{error}");
            }
            assert!(
                error.ends_with(&format!("the floor log could not be written: {detail}")),
                "{error}"
            );
            assert!(run.logs[0].truncated, "{run:?}");
            decide_alone(&mut run);
            assert_eq!(run.decision.unwrap().outcome, OUTCOME_COULD_NOT_RUN);
        }
    }
    drop(scratch);
    let _ = std::fs::remove_dir_all(&wt);
}

#[test]
fn core_1616_description_shows_waiting_then_admission_with_the_pass() {
    let waiting = "waiting for a disk slot (1 other floor(s) running, 34.0 GiB free)";
    let admitted = "admitted after waiting 2 s for a disk slot";
    let mut passes = FloorPasses {
        waiting: Some(waiting.into()),
        ..FloorPasses::default()
    };
    assert_eq!(passes.describe().as_deref(), Some(waiting));
    passes.waiting = None;
    passes.admitted = Some(admitted.into());
    assert_eq!(passes.describe().as_deref(), Some(admitted));
    passes
        .started
        .push(("test".into(), Tree::Head, Instant::now()));
    assert_eq!(
        passes.describe().unwrap(),
        format!("now `test` on the change (pass 1 of `test`), 0 min in; {admitted}")
    );
    passes.waiting = Some(waiting.into());
    assert_eq!(passes.describe().as_deref(), Some(waiting));
}

#[test]
fn core_1616_a_waiting_admission_updates_its_floor_now() {
    static COUNT: std::sync::Mutex<FloorSet> = std::sync::Mutex::new(FloorSet::new());
    let _admission = test_seam::floor_admission(&COUNT, 15, 20);
    let first = FloorSlot::admit(&|| Some(34 << 30), None, None).unwrap();
    let now = FloorNow::default();
    let waiting_now = std::sync::Arc::clone(&now);
    let (probing, probed) = std::sync::mpsc::channel();
    let (done, result) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _admission = test_seam::floor_admission(&COUNT, 15, 20);
        let free = || {
            probing.send(()).unwrap();
            Some(34 << 30)
        };
        let admitted = with_floor_now(&waiting_now, || {
            FloorSlot::admit_with_wait(&free, None, None, Duration::from_secs(30))
        });
        done.send(admitted.map(drop)).unwrap();
    });
    probed.recv_timeout(Duration::from_secs(10)).unwrap();
    // The probe holds the admission lock; reacquiring it observes the published wait note.
    let count = COUNT.lock().unwrap();
    assert_eq!(count.count, 1, "a waiting floor has no reservation");
    assert_eq!(
        now.lock().unwrap().describe().as_deref(),
        Some("waiting for a disk slot (1 other floor(s) running, 34.0 GiB free)")
    );
    assert!(matches!(
        result.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    drop(count);
    drop(first);
    result
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    waiter.join().unwrap();
    let passes = now.lock().unwrap();
    assert_eq!(passes.waiting, None);
    let admitted = passes
        .admitted
        .as_deref()
        .expect("admission records the wait");
    assert!(
        admitted.starts_with("admitted after waiting "),
        "{admitted}"
    );
    assert!(admitted.ends_with(" s for a disk slot"), "{admitted}");
    assert_eq!(passes.describe().as_deref(), Some(admitted));
}

#[test]
fn core_1616_the_wait_cap_is_visible_on_the_floor_now() {
    static COUNT: std::sync::Mutex<FloorSet> = std::sync::Mutex::new(FloorSet::new());
    let _admission = test_seam::floor_admission(&COUNT, 15, 20);
    let first = FloorSlot::admit(&|| Some(34 << 30), None, None).unwrap();
    let now = FloorNow::default();
    let second = with_floor_now(&now, || {
        FloorSlot::admit_with_wait(&|| Some(34 << 30), None, None, Duration::from_millis(100))
    })
    .unwrap();
    let passes = now.lock().unwrap();
    assert_eq!(passes.waiting, None);
    let admitted = passes
        .admitted
        .as_deref()
        .expect("the cap records admission");
    assert!(admitted.starts_with("admitted at the "), "{admitted}");
    assert!(
        admitted.ends_with(" s wait cap with 1 other floor(s) still running (34.0 GiB free)"),
        "{admitted}"
    );
    assert_eq!(passes.describe().as_deref(), Some(admitted));
    drop(passes);
    drop(second);
    drop(first);
}

#[test]
fn core_1616_admission_without_a_wait_records_nothing() {
    static COUNT: std::sync::Mutex<FloorSet> = std::sync::Mutex::new(FloorSet::new());
    let _admission = test_seam::floor_admission(&COUNT, 15, 20);
    let now = FloorNow::default();
    let slot = with_floor_now(&now, || FloorSlot::admit(&|| Some(40 << 30), None, None)).unwrap();
    let passes = now.lock().unwrap();
    assert_eq!(passes.waiting, None);
    assert_eq!(passes.admitted, None);
    assert_eq!(passes.describe(), None);
    drop(passes);
    drop(slot);
}

#[test]
fn core_1617_running_floors_disk_use_reduces_the_reservation_once_per_set() {
    static COUNT: std::sync::Mutex<FloorSet> = std::sync::Mutex::new(FloorSet::new());
    let _admission = test_seam::floor_admission(&COUNT, 15, 20);
    let first = FloorSlot::admit(&|| Some(60 << 30), None, None).unwrap();
    let second = FloorSlot::admit(&|| Some(60 << 30), None, None).unwrap();
    let now = FloorNow::default();
    // Of the two floors' 40 GiB reservation, 35 GiB is already reflected in free space.
    let third = with_floor_now(&now, || {
        FloorSlot::admit_with_wait(&|| Some(25 << 30), None, None, Duration::from_millis(100))
    })
    .unwrap();
    assert_eq!(COUNT.lock().unwrap().count, 3);
    assert_eq!(
        now.lock().unwrap().describe(),
        None,
        "25 GiB covers the 15 GiB minimum plus 5 GiB still reserved; no wait is needed"
    );
    drop(third);
    drop(second);
    drop(first);
}

#[test]
fn core_1617_unused_reservations_still_require_a_slot() {
    static COUNT: std::sync::Mutex<FloorSet> = std::sync::Mutex::new(FloorSet::new());
    let _admission = test_seam::floor_admission(&COUNT, 15, 20);
    let first = FloorSlot::admit(&|| Some(34 << 30), None, None).unwrap();
    let now = FloorNow::default();
    let second = with_floor_now(&now, || {
        FloorSlot::admit_with_wait(&|| Some(34 << 30), None, None, Duration::from_millis(100))
    })
    .unwrap();
    let passes = now.lock().unwrap();
    assert!(
        passes.admitted.as_deref().unwrap().contains("wait cap"),
        "with no disk use, 34 GiB cannot cover the 15 + 20 GiB requirement"
    );
    drop(passes);
    drop(second);
    drop(first);
}

#[test]
fn core_1617_a_new_set_uses_a_fresh_baseline() {
    static COUNT: std::sync::Mutex<FloorSet> = std::sync::Mutex::new(FloorSet::new());
    let _admission = test_seam::floor_admission(&COUNT, 15, 20);
    let first = FloorSlot::admit(&|| Some(60 << 30), None, None).unwrap();
    let second = FloorSlot::admit(&|| Some(60 << 30), None, None).unwrap();
    drop(first);
    assert_eq!(COUNT.lock().unwrap().baseline, Some(60 << 30));
    drop(second);
    assert_eq!(COUNT.lock().unwrap().count, 0);
    assert_eq!(COUNT.lock().unwrap().baseline, None);

    let first = FloorSlot::admit(&|| Some(30 << 30), None, None).unwrap();
    assert_eq!(COUNT.lock().unwrap().baseline, Some(30 << 30));
    let now = FloorNow::default();
    let second = with_floor_now(&now, || {
        FloorSlot::admit_with_wait(&|| Some(34 << 30), None, None, Duration::from_millis(100))
    })
    .unwrap();
    let passes = now.lock().unwrap();
    assert!(
        passes.admitted.as_deref().unwrap().contains("wait cap"),
        "free space above the new 30 GiB baseline must not consume the reservation"
    );
    drop(passes);
    drop(second);
    drop(first);
}

#[test]
fn core_1560_disk_guard_refuses_only_below_the_minimum() {
    const GIB: u64 = 1 << 30;
    let why = disk_guard(Some(14 * GIB), 15).expect("below the minimum");
    assert!(
        why.starts_with("environment:")
            && why.contains("14.0 GiB free")
            && why.contains("minimum of 15 GiB")
            && why.contains("not a failure of the change"),
        "{why}"
    );
    assert_eq!(disk_guard(Some(15 * GIB), 15), None);
    assert_eq!(disk_guard(Some(34 * GIB), 15), None);
    assert_eq!(disk_guard(Some(0), 0), None, "0 turns the guard off");
    assert_eq!(disk_guard(None, 15), None);
    assert!(disk_guard(Some(34 * GIB), u64::MAX).is_some());
}

#[test]
fn core_1604_a_second_floor_on_the_same_repo_waits_for_the_first() {
    static COUNT: std::sync::Mutex<FloorSet> = std::sync::Mutex::new(FloorSet::new());
    let _admission = test_seam::floor_admission(&COUNT, 15, 20);
    let repo = Path::new("/core-1604/r/.git");
    let first = FloorSlot::admit(&|| Some(1000 << 30), None, Some(repo)).unwrap();
    let now = FloorNow::default();
    let waiting_now = std::sync::Arc::clone(&now);
    let (probing, probed) = std::sync::mpsc::channel();
    let (done, result) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _admission = test_seam::floor_admission(&COUNT, 15, 20);
        let free = || {
            probing.send(()).unwrap();
            Some(1000 << 30)
        };
        let admitted = with_floor_now(&waiting_now, || {
            FloorSlot::admit_with_wait(&free, None, Some(repo), Duration::ZERO)
        });
        done.send(admitted.map(|slot| {
            assert_eq!(COUNT.lock().unwrap().count, 1);
            drop(slot);
        }))
        .unwrap();
    });
    probed.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(
        result.recv_timeout(Duration::from_millis(100)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    // Reacquiring the admission lock observes the wait note published after the probe.
    let count = COUNT.lock().unwrap();
    assert_eq!(count.count, 1, "a waiter holds no slot");
    assert_eq!(count.repos, vec![repo.to_path_buf()]);
    assert_eq!(
        now.lock().unwrap().describe().as_deref(),
        Some("waiting for another floor on this repository to finish (1 floor(s) running)")
    );
    drop(count);
    drop(first);
    result
        .recv_timeout(Duration::from_secs(2))
        .expect("dropping a slot wakes the waiter before its 5-second poll")
        .expect("the waiting floor must be admitted");
    waiter.join().unwrap();
    let passes = now.lock().unwrap();
    assert_eq!(passes.waiting, None);
    let admitted = passes
        .admitted
        .as_deref()
        .expect("admission records the repo wait");
    assert!(
        admitted.starts_with("admitted after waiting "),
        "{admitted}"
    );
    assert!(
        admitted.ends_with(" s for another floor on this repository"),
        "{admitted}"
    );
    assert_eq!(passes.describe().as_deref(), Some(admitted));
    assert_eq!(COUNT.lock().unwrap().count, 0);
    assert!(COUNT.lock().unwrap().repos.is_empty());
}

#[test]
fn core_1604_a_repo_wait_does_not_use_up_the_disk_slot_wait() {
    static COUNT: std::sync::Mutex<FloorSet> = std::sync::Mutex::new(FloorSet::new());
    let _admission = test_seam::floor_admission(&COUNT, 15, 20);
    let repo = Path::new("/core-1604/r/.git");
    let first = FloorSlot::admit(&|| Some(34 << 30), None, Some(repo)).unwrap();
    let other = FloorSlot::admit_with_wait(
        &|| Some(34 << 30),
        None,
        Some(Path::new("/core-1604/s/.git")),
        Duration::ZERO,
    )
    .unwrap();
    assert_eq!(COUNT.lock().unwrap().count, 2);
    let now = FloorNow::default();
    let waiting_now = std::sync::Arc::clone(&now);
    let (probing, probed) = std::sync::mpsc::channel();
    let (done, result) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _admission = test_seam::floor_admission(&COUNT, 15, 20);
        let free = || {
            probing.send(()).unwrap();
            Some(34 << 30)
        };
        let admitted = with_floor_now(&waiting_now, || {
            FloorSlot::admit_with_wait(&free, None, Some(repo), Duration::from_secs(1))
        });
        done.send(admitted.map(|slot| (slot, Instant::now())))
            .unwrap();
    });
    probed.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(
        result.recv_timeout(Duration::from_millis(1500)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    let count = COUNT.lock().unwrap();
    assert_eq!(
        count.count, 2,
        "a repo waiter holds no slot even after its disk cap"
    );
    assert_eq!(
        now.lock().unwrap().describe().as_deref(),
        Some("waiting for another floor on this repository to finish (2 floor(s) running)")
    );
    drop(count);
    let released = Instant::now();
    drop(first);
    let (slot, admitted_at) = result
        .recv_timeout(Duration::from_secs(10))
        .expect("the disk-slot wait must finish after the repo is released")
        .expect("the waiting floor must be admitted");
    let disk_wait = admitted_at.duration_since(released);
    assert!(disk_wait >= Duration::from_millis(900), "{disk_wait:?}");
    assert!(disk_wait < Duration::from_secs(10), "{disk_wait:?}");
    waiter.join().unwrap();
    let passes = now.lock().unwrap();
    assert_eq!(passes.waiting, None);
    let admitted = passes
        .admitted
        .as_deref()
        .expect("the cap records admission");
    let seconds: u64 = admitted
        .strip_prefix("admitted at the ")
        .and_then(|rest| {
            rest.strip_suffix(" s wait cap with 1 other floor(s) still running (34.0 GiB free)")
        })
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{admitted}"));
    // The cap text counts the disk-slot wait only: the total, with the 1.5 s repo wait,
    // would read at least one second more than the wait measured from the release.
    assert!(
        seconds <= disk_wait.as_secs(),
        "{admitted} after {disk_wait:?}"
    );
    drop(passes);
    drop(slot);
    drop(other);
    assert_eq!(COUNT.lock().unwrap().count, 0);
    assert!(COUNT.lock().unwrap().repos.is_empty());
}

#[test]
fn core_1604_floors_on_different_repos_do_not_wait() {
    static COUNT: std::sync::Mutex<FloorSet> = std::sync::Mutex::new(FloorSet::new());
    let _admission = test_seam::floor_admission(&COUNT, 15, 20);
    let repo = Path::new("/core-1604/r/.git");
    let other = Path::new("/core-1604/s/.git");
    let first = FloorSlot::admit(&|| Some(1000 << 30), None, Some(repo)).unwrap();
    let now = FloorNow::default();
    let second = with_floor_now(&now, || {
        FloorSlot::admit(&|| Some(1000 << 30), None, Some(other))
    })
    .unwrap();
    assert_eq!(now.lock().unwrap().describe(), None);
    assert_eq!(COUNT.lock().unwrap().count, 2);
    assert_eq!(
        COUNT.lock().unwrap().repos,
        vec![repo.to_path_buf(), other.to_path_buf()]
    );
    drop(first);
    assert_eq!(COUNT.lock().unwrap().repos, vec![other.to_path_buf()]);
    drop(second);
    assert_eq!(COUNT.lock().unwrap().count, 0);
    assert!(COUNT.lock().unwrap().repos.is_empty());
}

#[test]
fn core_1604_a_cancelled_run_stops_waiting_for_its_repo() {
    static COUNT: std::sync::Mutex<FloorSet> = std::sync::Mutex::new(FloorSet::new());
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = cancelled.clone();
    let stop = FloorStop::new(move || {
        flag.load(std::sync::atomic::Ordering::SeqCst)
            .then_some("cancelled")
    });
    let _admission = test_seam::floor_admission(&COUNT, 15, 20);
    let repo = Path::new("/core-1604/r/.git");
    let first = FloorSlot::admit(&|| Some(1000 << 30), None, Some(repo)).unwrap();
    let (probing, probed) = std::sync::mpsc::channel();
    let (done, result) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _admission = test_seam::floor_admission(&COUNT, 15, 20);
        let free = || {
            probing.send(()).unwrap();
            Some(1000 << 30)
        };
        done.send(
            FloorSlot::admit_with_wait(&free, Some(&stop), Some(repo), Duration::from_secs(30))
                .map(drop),
        )
        .unwrap();
    });
    probed.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(
        result.recv_timeout(Duration::from_millis(100)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    let refused = result.recv_timeout(Duration::from_secs(10));
    let why = refused
        .expect("cancellation is polled without releasing the held repo slot")
        .expect_err("a cancelled floor must not be admitted");
    assert_eq!(why, "the run was cancelled: its floor does not start");
    waiter.join().unwrap();
    assert_eq!(COUNT.lock().unwrap().count, 1, "a refusal holds no slot");
    assert_eq!(COUNT.lock().unwrap().repos, vec![repo.to_path_buf()]);
    drop(first);
    assert_eq!(COUNT.lock().unwrap().count, 0);
    assert!(COUNT.lock().unwrap().repos.is_empty());
}

#[test]
fn core_1604_repo_key_is_the_common_git_dir() {
    let dir = scratch("core-1604-key");
    let git_dir = dir.join("r/.git");
    let worktree_git_dir = git_dir.join("worktrees/x");
    std::fs::create_dir_all(&worktree_git_dir).unwrap();
    let expected = Some(git_dir.canonicalize().unwrap());
    assert_eq!(
        floor_repo_key(&FloorContext {
            git_dir: Some(worktree_git_dir),
            ..FloorContext::default()
        }),
        expected
    );
    assert_eq!(
        floor_repo_key(&FloorContext {
            git_dir: Some(git_dir),
            ..FloorContext::default()
        }),
        expected
    );
    assert_eq!(floor_repo_key(&FloorContext::default()), None);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn core_1560_a_floor_waits_for_a_slot_instead_of_refusing() {
    static COUNT: std::sync::Mutex<FloorSet> = std::sync::Mutex::new(FloorSet::new());
    let _admission = test_seam::floor_admission(&COUNT, 15, 20);
    let first = FloorSlot::admit(&|| Some(34 << 30), None, None).expect("the first floor fits");
    let (probing, probed) = std::sync::mpsc::channel();
    let (done, result) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _admission = test_seam::floor_admission(&COUNT, 15, 20);
        let free = || {
            probing.send(()).unwrap();
            Some(34 << 30)
        };
        let admitted = FloorSlot::admit(&free, None, None).map(|slot| {
            assert_eq!(COUNT.lock().unwrap().count, 1);
            drop(slot);
        });
        done.send(admitted).unwrap();
    });
    probed.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(
        result.recv_timeout(Duration::from_millis(100)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    assert_eq!(COUNT.lock().unwrap().count, 1, "a waiter holds no slot");
    drop(first);
    result
        .recv_timeout(Duration::from_secs(2))
        .expect("dropping a slot wakes the waiter before its 5-second poll")
        .expect("the waiting floor must be admitted, never refused");
    waiter.join().unwrap();
    assert_eq!(COUNT.lock().unwrap().count, 0);
}

#[test]
fn core_1560_a_floor_is_admitted_at_the_wait_deadline() {
    static COUNT: std::sync::Mutex<FloorSet> = std::sync::Mutex::new(FloorSet::new());
    let _admission = test_seam::floor_admission(&COUNT, 15, 20);
    let first = FloorSlot::admit(&|| Some(34 << 30), None, None).expect("the first floor fits");
    let started = Instant::now();
    let second = FloorSlot::admit_with_wait(&|| Some(34 << 30), None, None, Duration::from_secs(1))
        .expect("the deadline admits even while the other floor still holds its slot");
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(COUNT.lock().unwrap().count, 2);
    drop(second);
    assert_eq!(COUNT.lock().unwrap().count, 1);
    drop(first);
    assert_eq!(COUNT.lock().unwrap().count, 0);
}

#[test]
fn core_1560_below_minimum_refuses_regardless_of_in_flight_floors() {
    static COUNT: std::sync::Mutex<FloorSet> = std::sync::Mutex::new(FloorSet::new());
    let _admission = test_seam::floor_admission(&COUNT, 15, 20);
    for held in [false, true] {
        let first = held.then(|| FloorSlot::admit(&|| Some(34 << 30), None, None).unwrap());
        let refused = FloorSlot::admit_with_wait(&|| Some(14 << 30), None, None, Duration::ZERO);
        let why = refused.err().expect("below the minimum always refuses");
        assert!(why.starts_with("environment:"), "{why}");
        assert!(why.contains("minimum of 15 GiB"), "{why}");
        assert_eq!(COUNT.lock().unwrap().count, usize::from(held));
        drop(first);
    }
    assert_eq!(COUNT.lock().unwrap().count, 0);
}

#[test]
fn core_1560_a_cancelled_run_stops_waiting_for_a_slot() {
    static COUNT: std::sync::Mutex<FloorSet> = std::sync::Mutex::new(FloorSet::new());
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = cancelled.clone();
    let stop = FloorStop::new(move || {
        flag.load(std::sync::atomic::Ordering::SeqCst)
            .then_some("cancelled")
    });
    let _admission = test_seam::floor_admission(&COUNT, 15, 20);
    let first = FloorSlot::admit(&|| Some(34 << 30), None, None).unwrap();
    let (probing, probed) = std::sync::mpsc::channel();
    let (done, result) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _admission = test_seam::floor_admission(&COUNT, 15, 20);
        let free = || {
            probing.send(()).unwrap();
            Some(34 << 30)
        };
        done.send(
            FloorSlot::admit_with_wait(&free, Some(&stop), None, Duration::from_secs(30)).map(drop),
        )
        .unwrap();
    });
    probed.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(
        result.recv_timeout(Duration::from_millis(100)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    let refused = result.recv_timeout(Duration::from_secs(10));
    let why = refused
        .expect("cancellation is polled without releasing the held slot")
        .expect_err("a cancelled floor must not be admitted");
    assert_eq!(why, "the run was cancelled: its floor does not start");
    waiter.join().unwrap();
    assert_eq!(
        COUNT.lock().unwrap().count,
        1,
        "a refusal must not take a slot"
    );
    drop(first);
    assert_eq!(COUNT.lock().unwrap().count, 0);
}

#[test]
fn detects_node_scripts_with_an_ignore_scripts_install_when_node_modules_is_absent() {
    let wt = scratch("detect-node");
    std::fs::write(
        wt.join("package.json"),
        r#"{"scripts":{"build":"x","test":"vitest run","typecheck":"tsc --noEmit","dev":"y"}}"#,
    )
    .unwrap();
    std::fs::write(wt.join("package-lock.json"), "{}").unwrap();
    let names: Vec<(String, Vec<String>)> = detect(&wt)
        .unwrap()
        .into_iter()
        .map(|c| (c.name, c.argv))
        .collect();
    assert_eq!(
        names,
        vec![
            (
                "install".to_string(),
                s(&["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"])
            ),
            ("typecheck".to_string(), s(&["npm", "run", "typecheck"])),
            ("test".to_string(), s(&["npm", "run", "test"])),
        ],
        "install first (lockfile ⇒ ci, lifecycle scripts NEVER run), then typecheck/lint/test \
             in that order, only those present"
    );
    // No lockfile ⇒ an `npm install` that writes NO lockfile into the reviewed tree.
    std::fs::remove_file(wt.join("package-lock.json")).unwrap();
    assert_eq!(
        detect(&wt).unwrap()[0].argv,
        s(&[
            "npm",
            "install",
            "--ignore-scripts",
            "--no-package-lock",
            "--no-audit",
            "--no-fund"
        ])
    );
    // node_modules present ⇒ no install step — unless FORCED (F-433-003: a lift moved the
    // lockfile, the installed modules are stale), which re-installs frozen + scripts-off.
    std::fs::create_dir_all(wt.join("node_modules")).unwrap();
    std::fs::write(wt.join("package-lock.json"), "{}").unwrap();
    let forced = detect_with(
        &wt,
        &FloorContext {
            force_install: true,
            ..FloorContext::default()
        },
    )
    .unwrap();
    assert_eq!(forced[0].name, "install", "{forced:?}");
    assert_eq!(
        forced[0].argv,
        vec!["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"]
    );
    assert!(
        forced[0].source.contains("forced: lockfile drift"),
        "{}",
        forced[0].source
    );
    std::fs::remove_file(wt.join("package-lock.json")).unwrap();
    assert_eq!(
        detect(&wt)
            .unwrap()
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["typecheck", "test"]
    );
}

/// core#1393: an allowlisted `native_rebuild` adds a `native-build` check right after the
/// (unchanged, scripts-off) install, whatever the tree's install state; it is provisioning
/// (never compared with the base, never reused from a journal, the install's bound). A name off
/// the allowlist, a non-npm install or no package.json is a detection error, on every checkout.
#[test]
fn a_declared_native_addon_gets_a_native_build_check_after_the_install() {
    let wt = scratch("native-build");
    std::fs::write(
        wt.join("package.json"),
        r#"{"scripts":{"test":"node --test"},"dependencies":{"better-sqlite3":"12.0.0"}}"#,
    )
    .unwrap();
    std::fs::write(wt.join("package-lock.json"), "{}").unwrap();
    std::fs::create_dir_all(wt.join(".wicked")).unwrap();
    std::fs::write(
        wt.join(CONFIG_PATH),
        r#"{"native_rebuild":["better-sqlite3","better-sqlite3"]}"#,
    )
    .unwrap();
    let detected = detect(&wt).unwrap();
    let names: Vec<&str> = detected.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["install", NATIVE_BUILD_CHECK, "test"],
        "{detected:?}"
    );
    assert_eq!(
        detected[0].argv,
        s(&["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"]),
        "the install is unchanged (the container floor fills it from its cache)"
    );
    assert_eq!(detected[1].argv, native_build_argv(&["better-sqlite3"]));
    assert_eq!(
        detected[1].source,
        ".wicked/checks.json native_rebuild (better-sqlite3)"
    );
    assert!(is_provisioning(NATIVE_BUILD_CHECK) && is_provisioning("install"));
    assert!(!is_provisioning("test"));
    assert_eq!(detected[1].base_timeout(), INSTALL_TIMEOUT);
    // Provisioned already: no install, the build check still runs (it skips a built binding).
    std::fs::create_dir_all(wt.join("node_modules/better-sqlite3")).unwrap();
    std::fs::write(
        wt.join("node_modules/better-sqlite3/package.json"),
        r#"{"name":"better-sqlite3","version":"12.0.0"}"#,
    )
    .unwrap();
    let detected = detect(&wt).unwrap();
    assert_eq!(detected[0].name, NATIVE_BUILD_CHECK, "{detected:?}");
    // Off the allowlist: fail-closed, never "skip it".
    std::fs::write(wt.join(CONFIG_PATH), r#"{"native_rebuild":["left-pad"]}"#).unwrap();
    let err = detect(&wt).expect_err("off the allowlist");
    assert!(
        err.contains("not on the floor's native-rebuild allowlist"),
        "{err}"
    );
    assert!(native_rebuild_in(r#"{"native_rebuild":["x; rm -rf /"]}"#).is_err());
    // A non-npm install cannot honour it — refused with node_modules present or not.
    std::fs::write(
        wt.join(CONFIG_PATH),
        r#"{"native_rebuild":["better-sqlite3"]}"#,
    )
    .unwrap();
    std::fs::remove_file(wt.join("package-lock.json")).unwrap();
    std::fs::write(wt.join("pnpm-lock.yaml"), "").unwrap();
    let err = detect(&wt).expect_err("pnpm, provisioned");
    assert!(
        err.contains("npm install from package-lock.json only"),
        "{err}"
    );
    std::fs::remove_dir_all(wt.join("node_modules")).unwrap();
    let err = detect(&wt).expect_err("pnpm, unprovisioned");
    assert!(
        err.contains("npm install from package-lock.json only"),
        "{err}"
    );
    // A declaration alone emits the build, with no check scripts (it fails visibly when
    // nothing is installed, never silently skipped).
    std::fs::remove_file(wt.join("pnpm-lock.yaml")).unwrap();
    std::fs::write(wt.join("package-lock.json"), "{}").unwrap();
    std::fs::write(
        wt.join("package.json"),
        r#"{"dependencies":{"better-sqlite3":"12.0.0"}}"#,
    )
    .unwrap();
    let names: Vec<String> = detect(&wt).unwrap().into_iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        vec!["install".to_string(), NATIVE_BUILD_CHECK.to_string()],
        "a declaration alone provisions and builds"
    );
    std::fs::remove_file(wt.join("package.json")).unwrap();
    let err = detect(&wt).expect_err("no package.json");
    assert!(err.contains("needs a package.json"), "{err}");
    let _ = std::fs::remove_dir_all(&wt);
}

/// core#1393 (review r3): the generated script, executed against stub `npm` / `node` /
/// node-gyp: it builds an unbuilt addon (configure, then build), skips a built one without
/// looking for node-gyp, and fails — visibly — on a build that leaves no binding, a failed
/// node-gyp, a linked addon, a missing addon and a missing node-gyp.
#[cfg(unix)]
#[test]
fn the_native_build_script_builds_skips_and_fails_visibly() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let root = scratch("native-build-exec");
    let bin = root.join("bin");
    let npm_dir = root.join("lib/node_modules/npm");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(npm_dir.join("bin")).unwrap();
    std::fs::create_dir_all(npm_dir.join("node_modules/node-gyp/bin")).unwrap();
    std::fs::write(npm_dir.join("bin/npm-cli.js"), "").unwrap();
    // npm's CLI is executable, as every install ships it (`command -v npm` needs that).
    std::fs::set_permissions(
        npm_dir.join("bin/npm-cli.js"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let gyp = npm_dir.join("node_modules/node-gyp/bin/node-gyp.js");
    std::fs::write(&gyp, "").unwrap();
    symlink(npm_dir.join("bin/npm-cli.js"), bin.join("npm")).unwrap();
    let log = root.join("node.log");
    // The stub node: logs its argv; `build` writes a binding unless NO_BINDING, and any call
    // fails under GYP_FAILS.
    let node = bin.join("node");
    std::fs::write(
        &node,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\n[ -n \"$GYP_FAILS\" ] && exit 7\n\
                 if [ \"$2\" = build ] && [ -z \"$NO_BINDING\" ]; then mkdir -p build/Release && \
                 : > build/Release/x.node; fi\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
    let argv = native_build_argv(&["better-sqlite3"]);
    let tree = root.join("tree");
    let addon = tree.join("node_modules/better-sqlite3");
    let run = |env: &[(&str, &str)]| -> (i32, String) {
        let _ = std::fs::remove_file(&log);
        let mut c = std::process::Command::new(&argv[0]);
        c.hardened()
            .args(&argv[1..])
            .current_dir(&tree)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        for (k, v) in env {
            c.env(k, v);
        }
        let out = c.output().unwrap();
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        (
            out.status.code().unwrap_or(-1),
            format!(
                "{}{}{calls}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    };
    // Not installed.
    std::fs::create_dir_all(tree.join("node_modules")).unwrap();
    let (rc, out) = run(&[]);
    assert!(rc != 0 && out.contains("is not installed"), "{rc} {out}");
    // Installed, unbuilt: configure then build, then the binding is required and present.
    std::fs::create_dir_all(&addon).unwrap();
    std::fs::write(addon.join("package.json"), "{}").unwrap();
    let (rc, out) = run(&[]);
    assert_eq!(rc, 0, "{out}");
    assert!(
        out.contains("configure --release --nodedir=") && out.contains(" build --release"),
        "{out}"
    );
    assert!(out.contains(&gyp.display().to_string()), "{out}");
    assert!(addon.join("build/Release/x.node").is_file());
    // Built: skipped — node (and node-gyp) never run.
    let (rc, out) = run(&[]);
    assert_eq!(rc, 0, "{out}");
    assert!(!out.contains("configure"), "{out}");
    // A build that leaves no binding, and a failing node-gyp: both fail.
    std::fs::remove_dir_all(addon.join("build")).unwrap();
    let (rc, out) = run(&[("NO_BINDING", "1")]);
    assert!(rc != 0 && out.contains("built no binding"), "{rc} {out}");
    let (rc, _) = run(&[("GYP_FAILS", "1")]);
    assert_ne!(rc, 0);
    // No node-gyp beside npm (nor beside its directory): fails.
    std::fs::remove_file(&gyp).unwrap();
    let (rc, out) = run(&[]);
    assert_ne!(rc, 0, "{out}");
    std::fs::write(&gyp, "").unwrap();
    // A linked addon (a workspace or file: package under the name): refused.
    std::fs::remove_dir_all(&addon).unwrap();
    std::fs::create_dir_all(tree.join("packages/x")).unwrap();
    std::fs::write(tree.join("packages/x/package.json"), "{}").unwrap();
    symlink(tree.join("packages/x"), &addon).unwrap();
    let (rc, out) = run(&[]);
    assert!(rc != 0 && out.contains("is a link"), "{rc} {out}");
    let _ = std::fs::remove_dir_all(&root);
}

/// core#1393: the build runs the node-gyp beside the floor's own npm against its own node's
/// headers — no npm lifecycle (so no `.npmrc`, root or package scripts, or `.bin` shims) —
/// and fails unless a binding was built.
#[test]
fn the_native_build_runs_node_gyp_directly_and_requires_a_binding() {
    let argv = native_build_argv(&["better-sqlite3"]);
    assert_eq!(&argv[..2], &["sh", "-c"]);
    let script = &argv[2];
    for want in [
        "command -v npm",
        "readlink -f",
        "/node_modules/node-gyp/bin/node-gyp.js",
        "configure --release --nodedir=",
        "build --release",
        "node_modules/better-sqlite3/build/Release/*.node",
        "built no binding",
        // Debian packages node-gyp beside npm, not inside it.
        "/node-gyp/bin/node-gyp.js",
        "$(dirname \"$d\")",
        // A workspace or local package under the name is refused.
        "test -L node_modules/better-sqlite3",
    ] {
        assert!(script.contains(want), "{want}: {script}");
    }
    for never in [
        "npm rebuild",
        "npm run",
        "npm install",
        "npx",
        ".bin/",
        "prebuild-install",
    ] {
        assert!(!script.contains(never), "{never}: {script}");
    }
}

#[test]
fn a_hollow_node_modules_does_not_count_as_provisioned_and_the_install_names_the_gap() {
    // F-E2E-029: the nested worktree's `node_modules/` held only vitest's cache dir; the floor
    // must install, and say which declared dependency was missing.
    let wt = scratch("hollow-node-modules");
    std::fs::write(
            wt.join("package.json"),
            r#"{"scripts":{"test":"vitest run"},"dependencies":{"wicked-crew-api-types":"1.0.0"},"devDependencies":{"vitest":"3.0.0","@types/node":"22.0.0"},"optionalDependencies":{"fsevents":"2.0.0"}}"#,
        )
        .unwrap();
    std::fs::write(wt.join("package-lock.json"), "{}").unwrap();
    std::fs::create_dir_all(wt.join("node_modules/.vite/vitest")).unwrap();
    let detected = detect(&wt).unwrap();
    assert_eq!(detected[0].name, "install", "{detected:?}");
    assert_eq!(
        detected[0].argv,
        s(&["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"])
    );
    assert!(
        detected[0]
            .source
            .contains("`wicked-crew-api-types` is not installed"),
        "the install's source names the first missing declared dependency: {}",
        detected[0].source
    );
    // A partial install — some dependencies present — is still a gap, named by the missing one
    // (declared names are visited in sorted order: `@types/node` before `vitest`).
    for dep in ["wicked-crew-api-types", "@types/node"] {
        std::fs::create_dir_all(wt.join("node_modules").join(dep)).unwrap();
        std::fs::write(wt.join("node_modules").join(dep).join("package.json"), "{}").unwrap();
    }
    let detected = detect(&wt).unwrap();
    assert_eq!(detected[0].name, "install");
    assert!(
        detected[0].source.contains("`vitest` is not installed"),
        "{}",
        detected[0].source
    );
    // Every declared dependency present (a scoped one included; the OPTIONAL one absent)
    // ⇒ provisioned ⇒ no install step.
    std::fs::create_dir_all(wt.join("node_modules/vitest")).unwrap();
    std::fs::write(wt.join("node_modules/vitest/package.json"), "{}").unwrap();
    let names: Vec<String> = detect(&wt).unwrap().into_iter().map(|c| c.name).collect();
    assert_eq!(names, vec!["test".to_string()], "provisioned ⇒ no install");
}

/// core#1155: a live worktree whose node_modules the engine installed for the SEATS' platform
/// (a container seat's Linux) does not provision this host's checks: the floor installs.
#[test]
fn a_node_modules_installed_for_the_seats_platform_is_a_gap_on_the_host() {
    let wt = scratch("seat-platform-deps");
    std::fs::write(
        wt.join("package.json"),
        r#"{"scripts":{"test":"vitest run"},"devDependencies":{"vitest":"3.0.0"}}"#,
    )
    .unwrap();
    std::fs::write(wt.join("package-lock.json"), "{}").unwrap();
    std::fs::create_dir_all(wt.join("node_modules/vitest")).unwrap();
    std::fs::write(wt.join("node_modules/vitest/package.json"), "{}").unwrap();
    let names =
        |wt: &Path| -> Vec<String> { detect(wt).unwrap().into_iter().map(|c| c.name).collect() };
    assert_eq!(
        names(&wt),
        vec!["test".to_string()],
        "provisioned for the host"
    );
    // The marker names the host's own platform: still provisioned.
    let host = crate::seat_deps::SeatPlatform::host().label();
    let marker = wt.join("node_modules").join(crate::seat_deps::MARKER);
    std::fs::write(&marker, format!(r#"{{"platform":"{host}","lock":"x"}}"#)).unwrap();
    assert_eq!(names(&wt), vec!["test".to_string()]);
    // Another platform's install: the floor installs for the host first.
    std::fs::write(&marker, r#"{"platform":"plan9-mips","lock":"x"}"#).unwrap();
    let detected = detect(&wt).unwrap();
    assert_eq!(detected[0].name, "install", "{detected:?}");
    assert!(
        detected[0].source.contains("plan9-mips"),
        "{}",
        detected[0].source
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_dependency_counts_as_installed() {
    // A workspace member or `npm link` target is a symlink in `node_modules/`; its presence is
    // the install, and nothing under it is followed or read.
    let wt = scratch("symlinked-dep");
    std::fs::write(
        wt.join("package.json"),
        r#"{"scripts":{"test":"vitest run"},"devDependencies":{"linked":"1.0.0"}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(wt.join("node_modules")).unwrap();
    std::os::unix::fs::symlink("/nonexistent/elsewhere", wt.join("node_modules/linked")).unwrap();
    let names: Vec<String> = detect(&wt).unwrap().into_iter().map(|c| c.name).collect();
    assert_eq!(names, vec!["test".to_string()]);
}

#[test]
fn a_failed_install_is_reported_as_a_provisioning_finding_not_a_verdict() {
    let report = RepoChecksReport {
        targeted_test: None,
        detected: vec![RepoCheck {
            name: "install".into(),
            argv: s(&["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"]),
            source: "package-lock.json (node_modules absent)".into(),
            timeout_s: None,
            dir: String::new(),
        }],
        checks: vec![CheckRun {
            failures_complete_in: None,
            name: "install".into(),
            argv: s(&["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"]),
            source: "package-lock.json (node_modules absent)".into(),
            exit_code: Some(1),
            timed_out: false,
            spawn_error: None,
            duration_ms: 1200,
            stdout_tail: String::new(),
            stderr_tail: "npm ERR! code ENOTFOUND\nnpm ERR! network request failed".into(),
            bound_s: 0,
            bound_note: None,
            failure_ids: Vec::new(),
            flaky_ids: Vec::new(),
            scope: None,
            retry: None,
            self_retry: false,
            classification: None,
            pre_existing: Vec::new(),
            regressions: Vec::new(),
            base: None,
            env_cannot_run: Vec::new(),
            reruns: Vec::new(),
            earlier_passes: Vec::new(),
            selection: None,
            decision: None,
            logs: Vec::new(),
            load_band: None,
            reused_from: None,
        }],
        skipped: vec!["typecheck".into(), "test".into()],
        passed: false,
        detect_error: None,
        sandbox_level: "sandboxed".into(),
        sandbox_note: None,
        sandbox_error: None,
        engine_writes_removed: Vec::new(),
        claim: None,
        env: None,
        tree: None,
        rerun: None,
        waived: Vec::new(),
        requested_rerun: None,
        recheck: None,
        coverage_note: None,
        key: None,
        resumed: None,
        reused_from: None,
        runner: None,
    };
    let reason = report.denial_reason();
    assert!(
        reason.contains("dependency provisioning failed"),
        "{reason}"
    );
    assert!(
        reason.contains("package-lock.json (node_modules absent)"),
        "{reason}"
    );
    assert!(
        reason.contains("environment finding, not a verdict"),
        "{reason}"
    );
    assert!(
        reason.contains("ENOTFOUND"),
        "the install's own stderr rides along: {reason}"
    );
    assert!(
        reason.contains("not run after the failure: typecheck, test"),
        "{reason}"
    );
}

#[test]
fn detects_the_package_manager_from_the_lockfile_and_cargo_from_the_manifest() {
    let wt = scratch("detect-pm");
    std::fs::write(
        wt.join("package.json"),
        r#"{"scripts":{"lint":"eslint ."}}"#,
    )
    .unwrap();
    std::fs::write(wt.join("pnpm-lock.yaml"), "").unwrap();
    std::fs::create_dir_all(wt.join("node_modules")).unwrap();
    std::fs::write(wt.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
    let checks = detect(&wt).unwrap();
    assert_eq!(
        checks[0].name, "cargo-fmt-check",
        "the formatter precedes every other check"
    );
    assert_eq!(checks[0].argv, s(&["cargo", "fmt", "--all", "--check"]));
    assert_eq!(checks[1].argv, s(&["pnpm", "run", "lint"]));
    assert_eq!(checks[2].name, "cargo-test");
    assert_eq!(checks[2].argv, s(&["cargo", "test", "--no-fail-fast"]));
    // No manifests at all ⇒ nothing detected, and a run of it is a vacuous pass that SAYS so.
    let empty = scratch("detect-empty");
    assert!(detect(&empty).unwrap().is_empty());
    let report = run(&empty);
    if report.sandbox_error.is_none() {
        assert!(report.passed && report.checks.is_empty() && report.detect_error.is_none());
        assert!(report.summary().contains("no repository checks detected"));
    } else {
        assert!(
            !report.passed,
            "no boundary ⇒ fail-closed even with nothing to run"
        );
    }
    assert!(
        !report.sandbox_level.is_empty(),
        "the containment level is always disclosed"
    );
}

/// Codex review on #414: an unreadable or malformed manifest, or a probe that is a symlink,
/// is NOT "no checks" — the floor FAILS with the reason.
#[cfg(unix)]
#[test]
fn a_malformed_or_symlinked_manifest_fails_the_floor_by_name() {
    if crate::validator::skip_in_container_floor(
        "a_malformed_or_symlinked_manifest_fails_the_floor_by_name",
    ) {
        return;
    }
    if crate::validator::skip_when_nested(
        "a_malformed_or_symlinked_manifest_fails_the_floor_by_name",
    ) {
        return;
    }
    let wt = scratch("detect-bad");
    std::fs::write(wt.join("package.json"), "not a manifest").unwrap();
    let err = detect(&wt).expect_err("malformed package.json must not read as no checks");
    assert!(err.contains("not valid JSON"), "{err}");
    let report = run_with_sandbox(
        &wt,
        WorkerSandbox {
            wrapper: vec!["true".to_string()],
            level: SandboxLevel::Sandboxed,
            downgrade_reason: None,
        },
    );
    assert!(!report.passed && report.checks.is_empty());
    assert!(
        report.denial_reason().contains("not valid JSON")
            && report.denial_reason().contains("fail-closed"),
        "{}",
        report.denial_reason()
    );

    // `scripts` of the wrong shape.
    std::fs::write(wt.join("package.json"), r#"{"scripts":"nope"}"#).unwrap();
    assert!(detect(&wt)
        .expect_err("scripts must be an object")
        .contains("not an object"));

    // A symlinked manifest / lockfile / node_modules is refused, never followed.
    let outside = scratch("detect-outside");
    std::fs::write(
        outside.join("package.json"),
        r#"{"scripts":{"test":"true"}}"#,
    )
    .unwrap();
    std::fs::remove_file(wt.join("package.json")).unwrap();
    std::os::unix::fs::symlink(outside.join("package.json"), wt.join("package.json")).unwrap();
    let err = detect(&wt).expect_err("a symlinked manifest is refused");
    assert!(
        err.contains("symlink") && err.contains("package.json"),
        "{err}"
    );
    std::fs::remove_file(wt.join("package.json")).unwrap();
    std::fs::write(wt.join("package.json"), r#"{"scripts":{"test":"true"}}"#).unwrap();
    std::os::unix::fs::symlink(&outside, wt.join("node_modules")).unwrap();
    let err = detect(&wt).expect_err("a symlinked node_modules is refused");
    assert!(err.contains("node_modules"), "{err}");
    std::fs::remove_file(wt.join("node_modules")).unwrap();
    std::fs::write(wt.join("Cargo.toml"), "").unwrap();
    std::fs::remove_file(wt.join("Cargo.toml")).unwrap();
    std::os::unix::fs::symlink(outside.join("package.json"), wt.join("Cargo.toml")).unwrap();
    assert!(detect(&wt)
        .expect_err("a symlinked Cargo.toml is refused")
        .contains("Cargo.toml"));
}

/// The brief's test, in the medium every `cargo test` host has: a fixture crate whose one test
/// fails. The floor must capture the non-zero exit AND the assertion text in the tail, and
/// report the failure as a denial — running with the isolated `CARGO_HOME`/`HOME` and inside
/// whatever OS boundary the host offers.
#[test]
fn captures_a_failing_cargo_test_as_evidence_and_denies() {
    let wt = scratch("cargo-fail");
    std::fs::write(
        wt.join("Cargo.toml"),
        "[package]\nname = \"floor_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\npath = \"lib.rs\"\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        wt.join("lib.rs"),
        "#[cfg(test)]\nmod t {\n    #[test]\n    fn boom() {\n        assert!(false, \
             \"BOOM the floor must see this\");\n    }\n}\n",
    )
    .unwrap();
    let report = run(&wt);
    assert!(
        !report.passed,
        "a failing test must fail the floor: {report:?}"
    );
    if report.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the floor failed closed instead of running");
        return;
    }
    // Item 7 (adversarial review on #414): the fixture ships no `Cargo.lock`, so cargo wrote
    // one — the engine's own side effect — and the floor removed it again, disclosed.
    assert_eq!(
        report.engine_writes_removed,
        vec!["Cargo.lock".to_string()],
        "{report:?}"
    );
    assert!(
        !wt.join("Cargo.lock").exists(),
        "the engine-written lockfile must not be left for the worktree guard to deny"
    );
    assert_eq!(
        report.checks[0].name, "cargo-fmt-check",
        "formatter runs before tests"
    );
    assert!(
        report.checks[0].exit_code == Some(0),
        "properly formatted fixture passes fmt check: {:?}",
        report.checks[0]
    );
    let c = &report.checks[1];
    assert_eq!(c.name, "cargo-test");
    assert!(
        c.exit_code.is_some_and(|e| e != 0),
        "the non-zero exit is captured: {:?} ({} / {})",
        c.exit_code,
        c.stdout_tail,
        c.stderr_tail
    );
    assert!(!c.timed_out && c.spawn_error.is_none());
    let combined = format!("{}\n{}", c.stdout_tail, c.stderr_tail);
    assert!(
        combined.contains("BOOM the floor must see this"),
        "the assertion text rides the captured tail: {combined}"
    );
    let denial = report.denial_reason();
    assert!(
        denial.contains("cargo-test: exit") && denial.contains(CRITERION),
        "the denial names the check and the criterion: {denial}"
    );
    // The isolated homes AND the build artifacts live in the engine's check cache, never in
    // the reviewed tree (Copilot on #414: `./target` would trip the worktree guard; the
    // 2026-10-20 disk incident: `tmp/wicked-checks` held 16 GB in each kept worktree).
    let scratch = leaf_of(&wt);
    assert!(!scratch.starts_with(&wt), "{}", scratch.display());
    assert_no_worktree_scratch(&wt);
    assert!(scratch.join("cargo-home").is_dir());
    assert!(
        scratch
            .join("cargo-target")
            .join("head")
            .join("debug")
            .is_dir(),
        "cargo built into the scratch target dir's HEAD leaf (core#480)"
    );
    assert!(
        !wt.join("target").exists(),
        "no ./target in the reviewed tree"
    );
}

/// core#551: a worktree with an unformatted Rust file must fail the floor — the formatter check
/// runs before tests and a formatting diff is a deterministic denial.
#[test]
fn unformatted_rust_fails_fmt_floor() {
    let wt = scratch("fmt-fail");
    std::fs::write(
        wt.join("Cargo.toml"),
        "[package]\nname = \"fmt_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\npath = \"lib.rs\"\n[workspace]\n",
    )
    .unwrap();
    // Deliberately unformatted: rustfmt would expand `{ x }` to a multi-line body.
    std::fs::write(wt.join("lib.rs"), "pub fn add(a: i32,b: i32)->i32{a+b}\n").unwrap();
    let report = run(&wt);
    assert!(
        !report.passed,
        "an unformatted Rust file must fail the floor: {report:?}"
    );
    if report.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the floor failed closed instead of running");
        return;
    }
    let fmt = report
        .checks
        .iter()
        .find(|c| c.name == "cargo-fmt-check")
        .expect("cargo-fmt-check must appear in the checks");
    assert!(
        fmt.exit_code.is_some_and(|e| e != 0),
        "cargo-fmt-check exits non-zero on a formatting diff: {fmt:?}"
    );
    assert_eq!(
        report.detected[0].name, "cargo-fmt-check",
        "formatter must be listed FIRST in detected checks"
    );
    let denial = report.denial_reason();
    assert!(
        denial.contains("cargo-fmt-check") && denial.contains(CRITERION),
        "the denial names the formatter check: {denial}"
    );
}

/// core#551 control: a properly formatted Rust file must produce no formatter diff — the floor
/// must not deny a worktree solely because of formatting when the code IS already formatted.
#[test]
fn formatted_rust_worktree_passes_fmt_floor() {
    let wt = scratch("fmt-pass");
    std::fs::write(
        wt.join("Cargo.toml"),
        "[package]\nname = \"fmt_pass_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\npath = \"lib.rs\"\n[workspace]\n",
    )
    .unwrap();
    // Properly formatted Rust — rustfmt produces no diff.
    std::fs::write(
        wt.join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    )
    .unwrap();
    // Detection: cargo-fmt-check must be first in the detected set.
    let checks = detect(&wt).unwrap();
    assert_eq!(
        checks[0].name, "cargo-fmt-check",
        "formatter is detected first for a Rust workspace: {checks:?}"
    );
    // Execution: cargo-fmt-check exits 0 — no diff in the formatted fixture.
    // Other checks (cargo-test, lockfile writes) may fail in restricted environments;
    // the control claim is narrowly about the formatter exit code.
    let report = run(&wt);
    if report.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the floor failed closed instead of running");
        return;
    }
    // No escape hatch past the sandbox check: the formatter was detected above, so it MUST
    // have run — a missing or unspawnable check is a floor defect, not an environment fact.
    let fmt = report
        .checks
        .iter()
        .find(|c| c.name == "cargo-fmt-check")
        .expect("cargo-fmt-check must run when detected");
    assert!(
        fmt.spawn_error.is_none(),
        "cargo-fmt-check must be invocable where cargo is: {:?}",
        fmt.spawn_error
    );
    assert!(
        fmt.exit_code == Some(0),
        "cargo-fmt-check exits 0 on formatted code — no false positive: {fmt:?}"
    );
}

/// The brief's literal case — a failing `npm test`. Runs where npm is on PATH (every hosted
/// CI runner; developer machines); otherwise says so and returns, because a host without npm
/// is a fact about the host, not about the floor.
#[test]
fn captures_a_failing_npm_test_as_evidence() {
    if crate::validator::find_on_path("npm").is_none() {
        eprintln!("repo_checks: npm not on PATH — the npm-shaped floor test cannot run here");
        return;
    }
    let wt = scratch("npm-fail");
    std::fs::write(
            wt.join("package.json"),
            r#"{"name":"floor-fixture","version":"0.0.0","scripts":{"test":"node -e \"process.stdout.write('NPM BOOM\\n'); process.exit(3)\""}}"#,
        )
        .unwrap();
    // node_modules present ⇒ no install step (nothing to install, no network).
    std::fs::create_dir_all(wt.join("node_modules")).unwrap();
    let report = run(&wt);
    assert!(!report.passed, "{report:?}");
    if report.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the floor failed closed instead of running");
        return;
    }
    let c = &report.checks[0];
    assert_eq!(c.name, "test");
    assert_eq!(c.argv, s(&["npm", "run", "test"]));
    assert_eq!(
        c.exit_code,
        Some(3),
        "npm propagates the script's exit code ({} / {})",
        c.stdout_tail,
        c.stderr_tail
    );
    assert!(c.stdout_tail.contains("NPM BOOM"), "{}", c.stdout_tail);
}

/// The J4 shape (rig run 62c4f0ff, crew's `scripts/test-related.mjs`): the first pass fails
/// two files, the check's own retry re-runs them serially and only the shared one fails again.
/// The marker line goes to both streams (vitest prints its failures on stderr).
#[cfg(unix)]
const RETRY_SHAPE_SH: &str = "\
echo ' RUN  v3.2.4'
echo ' FAIL  tests/a.test.ts > first pass only' >&2
echo ' FAIL  tests/s.test.ts > shared' >&2
M='WICKED-RETRY a worker died — re-running the 2 named file(s) ONCE, serially, before recording any failure: tests/a.test.ts, tests/s.test.ts'
echo \"$M\"
echo \"$M\" >&2
echo ' FAIL  tests/s.test.ts > shared' >&2
echo 'WICKED-RETRY the re-run FAILED too — a reproducing failure is this change'\\''s, recorded as exit 1.'
exit 1
";

/// (core#1172) A check whose own retry cleared some first-pass failures: those are flaky
/// (passed on retry), kept out of `failure_ids` (so out of the change-only list), and the
/// check still fails exactly as before — its exit decides, not the ids.
#[cfg(unix)]
#[test]
fn a_failure_the_checks_own_retry_cleared_is_flaky_not_the_changes() {
    let wt = scratch("retry-cleared");
    std::fs::write(wt.join("retry.sh"), RETRY_SHAPE_SH).unwrap();
    let scratch = CheckScratch::prepare(&wt).unwrap();
    let check = RepoCheck {
        name: "test_targeted".into(),
        argv: s(&["sh", "retry.sh"]),
        source: "fixture".into(),
        timeout_s: None,
        dir: String::new(),
    };
    let no_wrapper = WorkerSandbox {
        wrapper: Vec::new(),
        level: crate::validator::SandboxLevel::BestEffort,
        downgrade_reason: None,
    };
    let r = run_one(&wt, &check, &no_wrapper, &scratch, Tree::Head);
    assert_eq!(r.exit_code, Some(1), "{r:?}");
    assert!(!r.passed(), "the check's verdict is its exit: {r:?}");
    assert_eq!(
        r.failure_ids,
        s(&["FAIL tests/s.test.ts > shared"]),
        "{r:?}"
    );
    assert_eq!(
        r.flaky_ids,
        s(&["FAIL tests/a.test.ts > first pass only"]),
        "{r:?}"
    );
}

/// (core#1172) The split itself: no retry block keeps every id; a stream that carries no
/// marker cannot be split (old scripts printed it on stdout only, failures on stderr); a final
/// pass that names nothing tells nothing apart; the LAST marker starts the final pass; the
/// marker's other lines start nothing.
#[test]
fn the_retry_split_reads_each_stream_after_its_last_re_run_marker() {
    let start = "WICKED-RETRY deadline — re-running the 1 named file(s) ONCE: tests/a.test.ts";
    let a = " FAIL  tests/a.test.ts > a";
    let b = " FAIL  tests/b.test.ts > b";
    let ids = |v: &[&str]| s(v);
    // No block.
    assert_eq!(
        split_retry_ids("", &format!("{a}\n{b}\n")),
        (
            ids(&["FAIL tests/a.test.ts > a", "FAIL tests/b.test.ts > b"]),
            Vec::new()
        )
    );
    // A marker on stdout only, failures on stderr: nothing split.
    assert_eq!(
        split_retry_ids(&format!("{start}\n"), &format!("{a}\n{b}\n")),
        (
            ids(&["FAIL tests/a.test.ts > a", "FAIL tests/b.test.ts > b"]),
            Vec::new()
        )
    );
    // Both streams marked: b reproduced, a cleared.
    assert_eq!(
        split_retry_ids(&format!("{start}\n"), &format!("{a}\n{b}\n{start}\n{b}\n")),
        (
            ids(&["FAIL tests/b.test.ts > b"]),
            ids(&["FAIL tests/a.test.ts > a"])
        )
    );
    // The final pass named nothing: every id stays a failure.
    assert_eq!(
        split_retry_ids("", &format!("{a}\n{start}\n")),
        (ids(&["FAIL tests/a.test.ts > a"]), Vec::new())
    );
    // (xreview on #1176) …even when another, unmarked stream names a failure: the marked
    // stream's pre-marker id is not cleared by ids that were never in a final pass.
    assert_eq!(
        split_retry_ids(&format!("{a}\n{start}\n"), &format!("{b}\n")),
        (
            ids(&["FAIL tests/a.test.ts > a", "FAIL tests/b.test.ts > b"]),
            Vec::new()
        )
    );
    // (xreview on #1176) A failure the final pass names past the id cap reproduced: it is a
    // failure, never flaky.
    let many: String = (0..MAX_FAILURE_IDS + 5)
        .map(|n| format!(" FAIL  tests/m{n}.test.ts > m\n"))
        .collect();
    let (failing, flaky) = split_retry_ids("", &format!("{a}\n{start}\n{many}{a}\n"));
    assert_eq!(failing.len(), MAX_FAILURE_IDS);
    assert!(flaky.is_empty(), "{flaky:?}");
    // A "none —" line or the verdict line starts no block.
    for other in [
        "WICKED-RETRY none — the failure carries no deadline marker",
        "WICKED-RETRY the re-run FAILED too — recorded as exit 1.",
    ] {
        assert_eq!(
            split_retry_ids("", &format!("{a}\n{other}\n{b}\n")).1,
            Vec::<String>::new(),
            "{other}"
        );
    }
}

/// Codex review on #414: a check is repo-controlled code and NEVER runs without an OS write
/// boundary. Deterministic by injection: a best-effort probe (what a host with no
/// `sandbox-exec`/`bwrap` yields) makes the floor FAIL with the reason, and nothing runs —
/// even a check that would have passed.
/// (core#678, codex r1) A launcher whose execution probe failed is no boundary: the floor
/// downgrades the detection (so the opt-in applies) instead of running a bwrap that cannot arm.
#[test]
fn the_floor_downgrades_a_launcher_whose_probe_failed() {
    let armed = || WorkerSandbox {
        wrapper: vec!["bwrap".to_string()],
        level: crate::validator::SandboxLevel::Sandboxed,
        downgrade_reason: None,
    };
    let kept = arm_probed(armed(), Ok("bwrap"));
    assert_eq!(kept.level, crate::validator::SandboxLevel::Sandboxed);
    assert_eq!(kept.wrapper, vec!["bwrap".to_string()]);
    let down = arm_probed(armed(), Err(crate::worker_sandbox::FloorUnarmed::CannotArm));
    assert_eq!(down.level, crate::validator::SandboxLevel::BestEffort);
    assert!(down.wrapper.is_empty());
    assert!(down.downgrade_reason.unwrap().starts_with("cannot_arm"));
}

#[test]
fn without_a_write_boundary_the_floor_fails_closed_and_runs_nothing() {
    // Reads an environment variable another test MUTATES (the core#416 opt-in below), so it
    // holds the crate-wide lock shared: fail-closed is only the default while nobody opted in.
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let wt = scratch("no-boundary");
    std::fs::write(wt.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
    let probe_marker = wt.join("ran.txt");
    let no_boundary = WorkerSandbox {
        wrapper: Vec::new(),
        level: SandboxLevel::BestEffort,
        downgrade_reason: Some("no OS-sandbox tool on PATH".to_string()),
    };
    let report = run_with_sandbox(&wt, no_boundary);
    assert!(!report.passed, "{report:?}");
    assert!(
        report.checks.is_empty(),
        "nothing may run unsandboxed: {report:?}"
    );
    assert_eq!(report.sandbox_level, "best-effort");
    assert!(
            report.sandbox_error.as_deref().is_some_and(
                |e| e.contains("no OS write boundary") && e.contains("no OS-sandbox tool")
            ),
            "{report:?}"
        );
    assert_eq!(
        report
            .detected
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["cargo-fmt-check", "cargo-test"],
        "what WOULD have run is still on the record"
    );
    let denial = report.denial_reason();
    assert!(
        denial.contains("NOT run") && denial.contains("fail-closed") && denial.contains(CRITERION),
        "{denial}"
    );
    assert!(!probe_marker.exists());
    assert!(
        report.denial_reason().contains(UNSANDBOXED_OPT_IN_ENV),
        "the denial names the opt-in, so an operator on a boundary-less host is not stuck: {}",
        report.denial_reason()
    );
}

/// core#416 item 3(b) — the disclosed opt-in, and the only thing an operator on Windows can
/// do about the floor. The default above denies every `repo_checks_floor` phase on a host
/// where no boundary can arm; with `WICKED_REPO_CHECKS_UNSANDBOXED=1` the checks RUN, and the
/// report, the gate's own note and the recorded `FloorEnv` all say that nothing contained
/// them (`sandbox_level: "none"`) — so a pass is never read as containment evidence.
///
/// Deterministic by injection, like the fail-closed test: a best-effort probe is what a host
/// with no `sandbox-exec`/`bwrap` yields, whatever this host has.
#[test]
fn the_disclosed_opt_in_runs_the_checks_with_no_boundary() {
    if crate::validator::find_on_path("npm").is_none() {
        eprintln!("repo_checks: npm not on PATH — the opt-in floor test cannot run here");
        return;
    }
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let prior = std::env::var_os(UNSANDBOXED_OPT_IN_ENV);
    let wt = scratch("unsandboxed-opt-in");
    std::fs::write(
            wt.join("package.json"),
            r#"{"name":"floor-fixture","version":"0.0.0","scripts":{"test":"node -e \"process.stdout.write('RAN UNSANDBOXED\\n')\""}}"#,
        )
        .unwrap();
    // node_modules present ⇒ no install step (nothing to install, no network).
    std::fs::create_dir_all(wt.join("node_modules")).unwrap();
    let no_boundary = || WorkerSandbox {
        wrapper: Vec::new(),
        level: SandboxLevel::BestEffort,
        downgrade_reason: Some("no OS-sandbox tool on PATH".to_string()),
    };

    std::env::set_var(UNSANDBOXED_OPT_IN_ENV, "1");
    let report = run_with_sandbox(&wt, no_boundary());
    match &prior {
        Some(v) => std::env::set_var(UNSANDBOXED_OPT_IN_ENV, v),
        None => std::env::remove_var(UNSANDBOXED_OPT_IN_ENV),
    }

    assert!(
        report.sandbox_error.is_none(),
        "the opt-in is not a refusal: {report:?}"
    );
    assert_eq!(report.sandbox_level, UNSANDBOXED_LEVEL);
    assert!(report.unsandboxed());
    let c = report
        .checks
        .iter()
        .find(|c| c.name == "test")
        .expect("the repository's own check RAN");
    assert!(
        c.stdout_tail.contains("RAN UNSANDBOXED"),
        "{}",
        c.stdout_tail
    );
    assert!(report.passed, "{report:?}");
    // Disclosed on every surface a reader can reach.
    let summary = report.summary();
    assert!(summary.contains("no OS write boundary"), "{summary}");
    let note = report
        .classification_note()
        .expect("the gate's floorNote discloses the posture");
    assert!(
        note.contains(UNSANDBOXED_OPT_IN_ENV) && note.contains("NOT evidence"),
        "{note}"
    );
    assert_eq!(
        report.env.as_ref().map(|e| e.sandbox_level.as_str()),
        Some(UNSANDBOXED_LEVEL),
        "the recorded environment carries the level CI parity is read against"
    );
}

/// The boundary itself, where the host has one: a check script that writes outside the
/// worktree fails the floor and the write never lands; the operator's real HOME is not the
/// check's HOME. On a host without a sandbox tool this asserts the fail-closed path instead
/// (never a skip that reads as a pass).
#[cfg(unix)]
#[test]
fn a_check_that_writes_outside_the_worktree_fails_the_floor() {
    // Its no-tool branch asserts the fail-closed refusal, which the core#416 opt-in can turn
    // off process-wide — shared lock, so that mutation cannot land mid-assert.
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let base = scratch("contain");
    let wt = base.join("wt");
    let outside = base.join("outside");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let sandbox = sandbox_for(&wt);
    if sandbox.level != SandboxLevel::Sandboxed {
        // No tool here: the floor must FAIL, not run. Assert that and stop.
        let report = run(&wt);
        assert!(
            !report.passed && report.sandbox_error.is_some(),
            "{report:?}"
        );
        return;
    }
    let scratch = CheckScratch::prepare(&wt).unwrap();
    // Premise: an INSIDE write under the same wrapper works (bwrap present but unusable — no
    // user namespaces — is an environmental skip of the KERNEL claim only).
    let inside = RepoCheck {
        name: "test".into(),
        argv: s(&[
            "sh",
            "-c",
            "printf y > \"$1\"",
            "_",
            &wt.join("ok").to_string_lossy(),
        ]),
        source: "fixture".into(),
        timeout_s: None,
        dir: String::new(),
    };
    if !run_one(&wt, &inside, &sandbox, &scratch, Tree::Head).passed() {
        eprintln!(
            "repo_checks: the sandbox wrapper cannot run on this host — skipping the kernel claim"
        );
        return;
    }
    let pwned = outside.join("pwned");
    let escaping = RepoCheck {
        name: "test".into(),
        argv: s(&[
            "sh",
            "-c",
            "printf x > \"$1\"",
            "_",
            &pwned.to_string_lossy(),
        ]),
        source: "fixture".into(),
        timeout_s: None,
        dir: String::new(),
    };
    let r = run_one(&wt, &escaping, &sandbox, &scratch, Tree::Head);
    assert!(!r.passed(), "an outside write must fail the check: {r:?}");
    assert!(!pwned.exists(), "the outside write must never land on disk");
    // macOS `sandbox-exec` says EPERM/EACCES; Linux `bwrap` surfaces its `--ro-bind` as EROFS.
    assert!(
        r.stderr_tail.contains("Permission denied")
            || r.stderr_tail.contains("Operation not permitted")
            || r.stderr_tail.contains("Read-only file system"),
        "the check observed the OS denial: {}",
        r.stderr_tail
    );
    // And the operator's real HOME is not the check's HOME.
    let home_probe = RepoCheck {
        name: "test".into(),
        argv: s(&["sh", "-c", "printf %s \"$HOME\""]),
        source: "fixture".into(),
        timeout_s: None,
        dir: String::new(),
    };
    let r = run_one(&wt, &home_probe, &sandbox, &scratch, Tree::Head);
    assert!(r.passed());
    assert!(
        r.stdout_tail
            .starts_with(&scratch.home().to_string_lossy().to_string()),
        "HOME is the isolated scratch home, got {}",
        r.stdout_tail
    );
}

/// Adversarial review on #414: a check is repo-controlled code with the network open, so it
/// must never see the daemon's credentials. The environment is cleared to an allow-list — a
/// secret-looking variable planted on the daemon side is absent from the check's `env`, while
/// the isolation overrides and `PATH` are present.
#[cfg(unix)]
#[test]
fn a_check_sees_a_minimal_environment_never_the_daemons_secrets() {
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    const PLANTED: &str = "WICKED_TEST_PLANTED_GH_TOKEN";
    std::env::set_var(PLANTED, "hunter2");
    let wt = scratch("min-env");
    let sandbox = sandbox_for(&wt);
    let scratch = CheckScratch::prepare(&wt).unwrap();
    let env_dump = RepoCheck {
        name: "test".into(),
        argv: s(&["sh", "-c", "env"]),
        source: "fixture".into(),
        timeout_s: None,
        dir: String::new(),
    };
    let r = run_one(&wt, &env_dump, &sandbox, &scratch, Tree::Head);
    std::env::remove_var(PLANTED);
    assert!(r.passed(), "{r:?}");
    let out = &r.stdout_tail;
    assert!(
        !out.contains(PLANTED),
        "the planted daemon-side secret reached the check:\n{out}"
    );
    // Nothing secret-looking from the daemon's environment either — by name, whatever is set.
    let leaked: Vec<String> = std::env::vars_os()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .filter(|k| {
            let u = k.to_ascii_uppercase();
            u.contains("TOKEN")
                || u.contains("SECRET")
                || u.contains("API_KEY")
                || u.contains("PASSWORD")
        })
        .filter(|k| out.lines().any(|l| l.starts_with(&format!("{k}="))))
        .collect();
    assert!(leaked.is_empty(), "leaked into the check: {leaked:?}");
    assert!(
        !out.lines().any(|l| l.starts_with("WICKED_")),
        "no WICKED_* variable reaches a check:\n{out}"
    );
    // The allow-list and the isolation overrides ARE there — all in the tree's leaf of the
    // engine's check cache, outside the worktree.
    assert_eq!(scratch.root(), leaf_of(&wt));
    assert!(!scratch.root().starts_with(&wt));
    let home_line = format!("HOME={}", scratch.home().display());
    assert!(out.lines().any(|l| l == home_line), "{out}");
    assert!(out.lines().any(|l| l == "CI=1"), "{out}");
    assert!(out.lines().any(|l| l.starts_with("PATH=")), "{out}");
    assert!(
        out.lines().any(|l| l.starts_with("CARGO_TARGET_DIR=")),
        "{out}"
    );
    // core#480: the head run builds into its own target leaf, the base run into another;
    // core#489: `TMPDIR` is the scratch's private dir under the system temp dir, not a leaf
    // of the worktree.
    let head_target = format!(
        "CARGO_TARGET_DIR={}",
        scratch.root.join("cargo-target").join("head").display()
    );
    assert!(out.lines().any(|l| l == head_target), "{out}");
    let tmp_line = format!("TMPDIR={}", scratch.tmp.path().display());
    assert!(out.lines().any(|l| l == tmp_line), "{out}");
    assert!(
        !scratch.tmp.path().starts_with(&wt),
        "TMPDIR must leave the worktree: {}",
        scratch.tmp.path().display()
    );
    let base = run_one(&wt, &env_dump, &sandbox, &scratch, Tree::Base);
    assert!(base.passed(), "{base:?}");
    let base_target = format!(
        "CARGO_TARGET_DIR={}",
        scratch.root.join("cargo-target").join("base").display()
    );
    assert!(
        base.stdout_tail.lines().any(|l| l == base_target),
        "{}",
        base.stdout_tail
    );
}

/// core#489: the checks' `TMPDIR` is a short private dir under the system temp dir — a socket
/// bound under it stays inside `sun_path` however deep the worktree is — recorded on the env
/// payload, inside the armed boundary, and gone when the floor is. Skips without python3 (the
/// socket binder) or a sandbox tool.
#[cfg(unix)]
#[test]
fn the_checks_tmpdir_is_short_private_outside_the_worktree_and_reaped_489() {
    if crate::validator::find_on_path("python3").is_none() {
        eprintln!("repo_checks: python3 not on PATH — the socket binder cannot run here");
        return;
    }
    // A worktree nested past the socket limit (104 bytes on macOS): a `TMPDIR` under it would
    // put `x.sock` well past that.
    let mut wt = scratch("deep-tmpdir");
    while wt.to_string_lossy().len() < 120 {
        wt = wt.join("a-deeper-directory-level");
    }
    std::fs::create_dir_all(wt.join(".wicked")).unwrap();
    // Check access bits, as in the private-dir creation test below: a setgid system temp
    // directory passes that special bit to children without granting group/other access.
    let binder = "import os, socket, stat\n\
                      t = os.environ['TMPDIR']\n\
                      assert os.environ['TMP'] == t and os.environ['TEMP'] == t\n\
                      print('TMPDIR=' + t)\n\
                      print('MODE=' + oct(stat.S_IMODE(os.stat(t).st_mode) & 0o777))\n\
                      s = socket.socket(socket.AF_UNIX)\n\
                      s.bind(os.path.join(t, 'x.sock'))\n\
                      s.listen(1)\n\
                      print('BOUND')\n";
    std::fs::write(
        wt.join(CONFIG_PATH),
        serde_json::json!({ "test": ["python3", "-c", binder] }).to_string(),
    )
    .unwrap();
    let report = run_floor(&wt, &FloorContext::default());
    if report.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return;
    }
    assert!(report.passed, "{report:?}");
    let env = report
        .env
        .as_ref()
        .expect("the env record rides a floor that ran");
    let tmpdir = Path::new(&env.tmpdir);
    // One short level under the system temp dir (`wc-` + 6). Measured against that dir, not
    // as a fixed 60 bytes (core#1194): nested inside a repo-checks floor, the system temp dir
    // is that floor's own `…/T/wc-xxxxxx`, and the macOS per-user one alone is 49 bytes.
    let base = std::env::temp_dir();
    let base_len = base.to_string_lossy().trim_end_matches('/').len();
    assert!(
        env.tmpdir.trim_end_matches('/').len() <= base_len + "/wc-123456".len(),
        "TMPDIR must stay short: {} ({} bytes over a {base_len}-byte temp dir)",
        env.tmpdir,
        env.tmpdir.len()
    );
    assert!(tmpdir.starts_with(std::env::temp_dir()), "{}", env.tmpdir);
    assert!(
        !tmpdir.starts_with(&wt),
        "TMPDIR left the worktree: {}",
        env.tmpdir
    );
    assert!(
        tmpdir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.len() == 9 && n.starts_with("wc-")),
        "{}",
        env.tmpdir
    );
    let out = &report.checks[0].stdout_tail;
    assert!(out.contains(&format!("TMPDIR={}", env.tmpdir)), "{out}");
    assert!(out.contains("MODE=0o700"), "{out}");
    assert!(out.contains("BOUND"), "{out}");
    assert!(
        !tmpdir.exists(),
        "the private TMPDIR is reaped with the floor: {}",
        env.tmpdir
    );
    // And the check scratch carries no `tmp` leaf any more — nor is it in the worktree.
    assert!(!leaf_of(&wt).join("tmp").exists());
    assert_no_worktree_scratch(&wt);
}

/// The private `TMPDIR` refuses an existing path at the drawn name — a directory or a SYMLINK
/// another local user planted on a shared sticky temp dir — and simply draws again: nothing
/// is followed, nothing fail-closes, and the result is mode 0700. Every draw taken is an
/// error, never a follow.
#[cfg(unix)]
#[test]
fn the_private_tmpdir_refuses_a_planted_name_and_draws_again() {
    use std::os::unix::fs::PermissionsExt;
    let base = scratch("tmp-draw");
    let outside = base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, base.join("wc-taken1")).unwrap();
    std::fs::create_dir_all(base.join("wc-taken2")).unwrap();
    let mut draws = ["wc-taken1", "wc-taken2", "wc-fresh0"].into_iter();
    let tmp =
        create_private_tmp(&base, || draws.next().expect("bounded draws").to_string()).unwrap();
    assert_eq!(tmp, base.join("wc-fresh0"));
    assert_eq!(
        std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(
        std::fs::symlink_metadata(base.join("wc-taken1"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the planted link is untouched"
    );
    assert!(
        std::fs::read_dir(&outside).unwrap().next().is_none(),
        "nothing was created through the link"
    );
    let err = create_private_tmp(&base, || "wc-taken1".to_string())
        .expect_err("every draw taken is an error");
    assert!(err.to_string().contains("no unused"), "{err}");
}

/// core#493: a launcher that dies before exec (`bwrap: Can't mkdir <HOME>/.aws: Read-only
/// file system`, exit 1) never ran the check — the record is `could_not_run` carrying the
/// launcher's line, not the check's `failed`. A check that RAN and exited 1 under the same
/// wrapper is still `failed`; a passing exit is never reclassified whatever it printed.
#[cfg(unix)]
#[test]
fn a_launcher_that_fails_to_arm_is_could_not_run_never_the_checks_failure_493() {
    use std::os::unix::fs::PermissionsExt;
    let wt = scratch("launcher-fail");
    let fake = wt.join("fake-bwrap");
    std::fs::write(
        &fake,
        "#!/bin/sh\n\
             if [ \"$1\" = \"--fail\" ]; then\n\
               echo \"bwrap: Can't mkdir /nonexistent-home/.aws: Read-only file system\" >&2\n\
               exit 1\n\
             fi\n\
             shift\n\
             exec \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let armed = |flag: &str| WorkerSandbox {
        wrapper: vec![fake.to_string_lossy().into_owned(), flag.to_string()],
        level: SandboxLevel::Sandboxed,
        downgrade_reason: None,
    };
    let scratch = CheckScratch::prepare(&wt).unwrap();
    let check = |script: &str| RepoCheck {
        name: "test".into(),
        argv: s(&["sh", "-c", script]),
        source: "fixture".into(),
        timeout_s: None,
        dir: String::new(),
    };
    let dead = run_one(
        &wt,
        &check("exit 0"),
        &armed("--fail"),
        &scratch,
        Tree::Head,
    );
    assert_eq!(dead.outcome(), "could_not_run", "{dead:?}");
    assert_eq!(dead.exit_code, Some(1));
    assert!(
        dead.spawn_error.as_deref().is_some_and(|e| {
            e.starts_with("the OS sandbox launcher exited before the check ran: bwrap: Can't mkdir")
        }),
        "{dead:?}"
    );
    assert!(
        dead.denies(),
        "fail-closed: a floor that could not run still denies"
    );
    assert!(
        dead.summary().contains("exited before the check ran"),
        "{}",
        dead.summary()
    );
    let red = run_one(
        &wt,
        &check("echo 'error: test failed' >&2; exit 1"),
        &armed("--"),
        &scratch,
        Tree::Head,
    );
    assert_eq!(red.outcome(), "failed", "{red:?}");
    assert!(red.spawn_error.is_none());
    let green = run_one(
        &wt,
        &check("echo 'bwrap: only printed' >&2; exit 0"),
        &armed("--"),
        &scratch,
        Tree::Head,
    );
    assert_eq!(green.outcome(), "passed", "{green:?}");
}

/// Adversarial review on #414: a descendant that escapes the process group (`setsid`) and
/// keeps the check's stdout open must not wedge the verify unit — the drain is bounded and
/// the tail read so far is reported.
#[cfg(unix)]
#[test]
fn a_detached_pipe_holder_cannot_wedge_the_check() {
    // spawn-audit: test-only — probes whether perl exists to build the detached pipe holder.
    if Command::new("perl").arg("-e").arg("1").output().is_err() {
        eprintln!("perl not on PATH — the detached pipe-holder test cannot run here");
        return;
    }
    let wt = scratch("pipe-holder");
    let sandbox = sandbox_for(&wt);
    let scratch = CheckScratch::prepare(&wt).unwrap();
    // The check prints, then leaves a setsid'd perl holding stdout, and exits 0. (core#1194)
    // The holder is a CONDITION, not a clock: it holds until the test releases it (a 600 s
    // guard ends it if the test never does), and it marks `held-to-end` only when it lets go.
    // The check returning while that mark is absent is the proof the drain did not wait for
    // the holder; a time limit on the check said nothing at load 80, where it took 192 s.
    let holder = RepoCheck {
        name: "test".into(),
        argv: s(&[
            "sh",
            "-c",
            "echo before-holder; perl -e 'use POSIX; POSIX::setsid(); \
                 for (1..6000) { last if -e \"release\"; select(undef, undef, undef, 0.1) } \
                 open(my $f, \">\", \"held-to-end\"); close($f)' & exit 0",
        ]),
        source: "fixture".into(),
        timeout_s: None,
        dir: String::new(),
    };
    let r = run_one(&wt, &holder, &sandbox, &scratch, Tree::Head);
    let held = !wt.join("held-to-end").exists();
    std::fs::write(wt.join("release"), "go").unwrap();
    assert!(
        held,
        "the check returned only once the holder let go of its stdout"
    );
    assert!(r.passed(), "{r:?}");
    assert!(r.stdout_tail.contains("before-holder"), "{r:?}");
}

/// Adversarial review on #414: a checkout that ships `tmp` as a SYMLINK must not have the
/// checks' scratch created through it — the floor is refused (core#1050: still, though most
/// of the scratch moved to the engine's check cache; the base export lives under `tmp`). A
/// link at a scratch LEAF (an earlier floor's check can write the leaf) is refused too.
#[cfg(unix)]
#[test]
fn a_symlinked_scratch_root_is_refused_not_followed() {
    let base = scratch("symlink-tmp");
    let wt = base.join("wt");
    let outside = base.join("outside");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, wt.join(crate::worktree_guard::ENGINE_SCRATCH_DIR))
        .unwrap();
    let err = CheckScratch::prepare(&wt).expect_err("a symlinked tmp is refused");
    assert!(err.to_string().contains("symlink"), "{err}");
    assert!(
        std::fs::read_dir(&outside).unwrap().next().is_none(),
        "nothing was created through the worktree's `tmp` link"
    );
    std::fs::remove_file(wt.join(crate::worktree_guard::ENGINE_SCRATCH_DIR)).unwrap();
    drop(CheckScratch::prepare(&wt).unwrap());
    // A link planted at a leaf of the check scratch itself.
    let leaf = leaf_of(&wt);
    std::fs::remove_dir_all(leaf.join("home")).unwrap();
    std::os::unix::fs::symlink(&outside, leaf.join("home")).unwrap();
    let err = CheckScratch::prepare(&wt).expect_err("a symlinked leaf is refused");
    assert!(err.to_string().contains("symlink"), "{err}");
    assert!(
        std::fs::read_dir(&outside).unwrap().next().is_none(),
        "nothing was created through the leaf's link"
    );
    // The floor reports the refusal as a detection error, fail-closed — with a boundary
    // injected so the scratch step is reached on every host (no tool ⇒ the earlier
    // fail-closed branch would answer first).
    let report = run_with_sandbox(
        &wt,
        WorkerSandbox {
            wrapper: vec!["true".to_string()],
            level: SandboxLevel::Sandboxed,
            downgrade_reason: None,
        },
    );
    assert!(!report.passed);
    assert!(
        report
            .detect_error
            .as_deref()
            .is_some_and(|e| e.contains("symlink")),
        "{report:?}"
    );
    std::fs::remove_file(leaf.join("home")).unwrap();
    assert!(remove_check_scratch_in(
        &check_cache_root().unwrap(),
        &wt,
        None
    ));
}

/// (2026-10-20 disk incident) The check scratch lives OUTSIDE the worktree, in the engine's
/// check cache: grouped by repository (the directory its run worktrees sit in, labelled with
/// the repository's name), one leaf per run tree, and the same leaf is derived after the tree
/// is gone (the reaper's view).
#[test]
fn the_check_scratch_lives_outside_the_worktree_keyed_by_repository() {
    let base = scratch("cache-key");
    let cache = base.join("cache");
    let a = base.join("myrepo").join("wicked-worktrees").join("run-a");
    let b = base.join("myrepo").join("wicked-worktrees").join("run-b");
    let other = base.join("other").join("wicked-worktrees").join("run-a");
    for d in [&a, &b, &other] {
        std::fs::create_dir_all(d).unwrap();
    }
    let (la, lb, lo) = (
        scratch_dir_for(&cache, &a, None),
        scratch_dir_for(&cache, &b, None),
        scratch_dir_for(&cache, &other, None),
    );
    assert!(la.starts_with(&cache), "{}", la.display());
    assert!(!la.starts_with(base.join("myrepo")), "{}", la.display());
    assert_eq!(la.parent(), lb.parent(), "one group per repository");
    assert_ne!(la, lb, "one leaf per run tree");
    assert_ne!(
        la.parent(),
        lo.parent(),
        "another repository is another group"
    );
    let name = |p: &Path| p.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name(la.parent().unwrap()).starts_with("myrepo-"),
        "{}",
        la.display()
    );
    assert!(name(&la).starts_with("run-a-"), "{}", la.display());
    std::fs::remove_dir_all(&a).unwrap();
    assert_eq!(
        scratch_dir_for(&cache, &a, None),
        la,
        "derivable once the tree is gone"
    );
    // Preparing makes every leaf there and writes nothing into the worktree.
    let prepared = CheckScratch::prepare_in(&cache, &b, None, u64::MAX).unwrap();
    assert_eq!(prepared.root(), lb);
    for sub in SCRATCH_LEAVES {
        assert!(lb.join(sub).is_dir(), "{sub}");
    }
    assert!(
        std::fs::read_dir(&b).unwrap().next().is_none(),
        "nothing is written into the worktree"
    );
    drop(prepared);
    assert!(remove_check_scratch_in(&cache, &b, None));
    assert!(!lb.exists());
    assert!(lease_path(&lb).is_file(), "the 0-byte lock file is kept");
    assert!(b.is_dir(), "the worktree itself is never touched");
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn pr_c2_last_scratch_retires_both_target_claims_without_ending_the_run() {
    let base = scratch("retire-targets");
    let cache = base.join("cache");
    let wt = base.join("worktree");
    std::fs::create_dir_all(&wt).unwrap();
    let scratch = CheckScratch::prepare_in(&cache, &wt, Some("live-run"), u64::MAX).unwrap();
    let leaf = scratch.root().to_path_buf();
    let kubectl = base.join("kubectl");
    crate::test_env::install_stub(&kubectl, &format!(
            "#!/bin/sh\necho \"$*\" >>'{0}/calls'\ncase \" $* \" in\n*' apply '*) cat >/dev/null;;\n*' get pvc '*) echo '{{\"metadata\":{{\"labels\":{{\"wicked.dev/owner\":\"me\",\"app.kubernetes.io/managed-by\":\"wicked-core\"}}}}}}';;\nesac\n",
            base.display()
        ));
    let kube = crate::k8s::Kube {
        kubectl,
        kubeconfig: None,
        context: "test".into(),
        owner: "me".into(),
    };
    let place = crate::k8s::floor::Place {
        kube,
        ns: "live-ns".into(),
        run: "live-run".into(),
        owned: false,
    };
    for tree in [Tree::Head, Tree::Base] {
        let claim = crate::checks_container::target_volume(&leaf, tree);
        crate::checks_container::admit_target_claim(
            &leaf,
            &place.kube,
            &place.ns,
            &claim,
            crate::checks_container::MAX_TARGET_LEAVES,
        )
        .unwrap();
        place.claim(&claim, "64Gi").unwrap();
    }
    assert!(!retire_leaf(&leaf), "a leased leaf is only marked orphaned");
    assert!(
        !std::fs::read_to_string(base.join("calls"))
            .unwrap()
            .contains("delete pvc"),
        "no active claim is deleted"
    );
    drop(scratch);
    assert!(!leaf.exists());
    let calls = std::fs::read_to_string(base.join("calls")).unwrap();
    for tree in [Tree::Head, Tree::Base] {
        assert!(
            calls.contains(&format!(
                "delete pvc {}",
                crate::checks_container::target_volume(&leaf, tree)
            )),
            "{calls}"
        );
    }
    assert!(!calls.contains("delete namespace"));
    std::fs::remove_dir_all(base).unwrap();
}

/// (core#1542) The run's last floor out of a leaf deletes its BASE target claim and keeps the
/// head claim warm (and registered, so the leaf's retirement still deletes it).
#[cfg(unix)]
#[test]
fn core_1542_last_floor_out_drops_the_base_target_claim_and_keeps_the_head() {
    let base = scratch("retire-base-claim");
    let cache = base.join("cache");
    let wt = base.join("worktree");
    std::fs::create_dir_all(&wt).unwrap();
    let scratch = CheckScratch::prepare_in(&cache, &wt, Some("base-run"), u64::MAX).unwrap();
    let leaf = scratch.root().to_path_buf();
    let kubectl = base.join("kubectl");
    crate::test_env::install_stub(&kubectl, &format!(
            "#!/bin/sh\necho \"$*\" >>'{0}/calls'\ncase \" $* \" in\n*' apply '*) cat >/dev/null;;\n*' get pvc '*) echo '{{\"metadata\":{{\"labels\":{{\"wicked.dev/owner\":\"me\",\"app.kubernetes.io/managed-by\":\"wicked-core\"}}}}}}';;\nesac\n",
            base.display()
        ));
    let kube = crate::k8s::Kube {
        kubectl,
        kubeconfig: None,
        context: "test".into(),
        owner: "me".into(),
    };
    let ns = format!("base-claim-{}", uuid::Uuid::new_v4());
    let [head, base_claim] =
        [Tree::Head, Tree::Base].map(|tree| crate::checks_container::target_volume(&leaf, tree));
    for claim in [&head, &base_claim] {
        crate::checks_container::admit_target_claim(
            &leaf,
            &kube,
            &ns,
            claim,
            crate::checks_container::MAX_TARGET_LEAVES,
        )
        .unwrap();
    }
    drop(scratch);
    assert!(leaf.exists(), "the leaf stays for the run's next floor");
    let calls = std::fs::read_to_string(base.join("calls")).unwrap();
    assert!(
        calls.contains(&format!("delete pvc {base_claim}")),
        "{calls}"
    );
    // (core#1714) Only marked under the leaf's lock: the next floor's claim waits it out.
    let delete = calls
        .lines()
        .find(|l| l.contains(&format!("delete pvc {base_claim}")))
        .unwrap();
    assert!(delete.contains("--wait=false"), "{delete}");
    assert!(!calls.contains(&format!("delete pvc {head}")), "{calls}");
    // The head claim is still the leaf's: retiring the leaf deletes it.
    assert!(retire_leaf(&leaf));
    let calls = std::fs::read_to_string(base.join("calls")).unwrap();
    assert!(calls.contains(&format!("delete pvc {head}")), "{calls}");
    assert_eq!(
        calls.matches(&format!("delete pvc {base_claim}")).count(),
        1
    );
    std::fs::remove_dir_all(base).unwrap();
}

#[cfg(unix)]
#[test]
fn pr_c2_target_budget_evicts_idle_leaves_and_refuses_when_all_are_leased() {
    let base = scratch("target-budget");
    let cache = base.join("cache");
    let wt = base.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    let kubectl = base.join("kubectl");
    crate::test_env::install_stub(&kubectl, &format!(
            "#!/bin/sh\necho \"$*\" >>'{0}/calls'\ncase \" $* \" in\n*' get pvc '*) echo '{{\"metadata\":{{\"labels\":{{\"wicked.dev/owner\":\"me\",\"app.kubernetes.io/managed-by\":\"wicked-core\"}}}}}}';;\nesac\n", base.display()
        ));
    let kube = crate::k8s::Kube {
        kubectl,
        kubeconfig: None,
        context: "test".into(),
        owner: "me".into(),
    };
    let ns = format!("budget-{}", uuid::Uuid::new_v4());
    let allocate = |leaf: &Path| {
        crate::checks_container::admit_target_claim(
            leaf,
            &kube,
            &ns,
            &crate::checks_container::target_volume(leaf, Tree::Head),
            2,
        )
    };
    let idle = CheckScratch::prepare_in(&cache, &wt, Some("idle"), u64::MAX).unwrap();
    let idle_leaf = idle.root().to_path_buf();
    allocate(&idle_leaf).unwrap();
    drop(idle);
    let active = CheckScratch::prepare_in(&cache, &wt, Some("active"), u64::MAX).unwrap();
    allocate(active.root()).unwrap();
    let own = CheckScratch::prepare_in(&cache, &wt, Some("own"), u64::MAX).unwrap();
    allocate(own.root()).unwrap();
    assert!(!idle_leaf.exists(), "the idle leaf was evicted");
    assert!(active.root().exists());
    let blocked = CheckScratch::prepare_in(&cache, &wt, Some("blocked"), u64::MAX).unwrap();
    let err = allocate(blocked.root()).unwrap_err();
    assert!(err.starts_with("admission_pending:"), "{err}");
    assert!(active.root().exists() && own.root().exists());
    drop(blocked);
    drop(own);
    drop(active);
    for run in ["own", "active"] {
        assert!(remove_check_scratch_in(&cache, &wt, Some(run)));
    }
    std::fs::remove_dir_all(base).unwrap();
}

/// The per-repository cap: over it, the least recently used OTHER run leaves go first, a
/// leaf a floor holds a lease on is never taken (two floors starting together never evict
/// each other), and the floor's own leaf is cleaned only when it alone is over the cap —
/// then re-made empty for the floor about to run.
#[test]
fn the_cap_removes_least_recently_used_leaves_first_and_never_a_leased_one() {
    let base = scratch("cache-cap");
    let cache = base.join("cache");
    let wts = base.join("repo").join("wicked-worktrees");
    let ago = |s: u64| std::time::SystemTime::now() - std::time::Duration::from_secs(s);
    let fill = |name: &str, bytes: usize, age_s: u64| {
        let wt = wts.join(name);
        std::fs::create_dir_all(&wt).unwrap();
        let leaf = scratch_dir_for(&cache, &wt, None);
        std::fs::create_dir_all(leaf.join("cargo-target").join("head")).unwrap();
        std::fs::write(leaf.join("cargo-target/head/blob"), vec![0u8; bytes]).unwrap();
        let lock = std::fs::File::create(lease_path(&leaf)).unwrap();
        lock.set_modified(ago(age_s)).unwrap();
        (wt, leaf)
    };
    let (_, old) = fill("old", 600, 300);
    let (_, newer) = fill("newer", 600, 100);
    let (_, busy) = fill("busy", 600, 50);
    let (own_wt, own) = fill("own", 0, 10);
    // A floor of the `busy` run is mid-build; it is also the least recently used.
    let held = lock_leaf(&busy, false).unwrap().unwrap();
    held.set_modified(ago(400)).unwrap();
    assert_eq!(
        enforce_check_cache_cap(&cache, &own, CacheCaps::uniform(10_000)),
        (vec![], false),
        "under the cap"
    );
    // Two active floors (`busy` and this one) over the cap together: neither leaf is taken
    // for the other, and this floor's own leaf (under the cap alone) is not cleaned.
    let own_lease = lock_leaf(&own, false).unwrap().unwrap();
    own_lease.set_modified(ago(10)).unwrap();
    let (removed, own_over) = enforce_check_cache_cap(&cache, &own, CacheCaps::uniform(1000));
    assert!(!own_over);
    assert_eq!(
        removed,
        vec![old.clone(), newer.clone()],
        "LRU first, the leased one skipped"
    );
    assert!(
        busy.join("cargo-target/head/blob").is_file(),
        "a leased leaf is never removed"
    );
    assert!(own.is_dir(), "the group fits once the others are gone");
    drop(own_lease);
    drop(held);
    // The floor's own leaf alone is over the cap: the others go, then its own — and the
    // floor gets a fresh, empty scratch.
    std::fs::write(own.join("cargo-target/head/blob"), vec![0u8; 1500]).unwrap();
    let prepared = CheckScratch::prepare_in(&cache, &own_wt, None, 1000).unwrap();
    assert!(
        !busy.exists(),
        "an unleased leaf goes before the floor's own"
    );
    assert!(
        !own.join("cargo-target/head/blob").exists(),
        "the own leaf was cleaned"
    );
    assert!(
        own.join("cargo-target/head").is_dir(),
        "and re-made for this floor"
    );
    assert_eq!(prepared.root(), own);
    let _ = std::fs::remove_dir_all(&base);
}

/// A leaf of run `name` over `<base>/<repo>/wicked-worktrees/<name>` holding `bytes` of build
/// output, last used `age_s` seconds ago.
fn cap_leaf(base: &Path, repo: &str, name: &str, bytes: usize, age_s: u64) -> (PathBuf, PathBuf) {
    let wt = base.join(repo).join("wicked-worktrees").join(name);
    std::fs::create_dir_all(&wt).unwrap();
    let leaf = scratch_dir_for(&base.join("cache"), &wt, Some(name));
    std::fs::create_dir_all(leaf.join("cargo-target").join("head")).unwrap();
    std::fs::write(leaf.join("cargo-target/head/blob"), vec![0u8; bytes]).unwrap();
    let lock = std::fs::File::create(lease_path(&leaf)).unwrap();
    lock.set_modified(std::time::SystemTime::now() - Duration::from_secs(age_s))
        .unwrap();
    (wt, leaf)
}

/// (core#1122) The TOTAL cap evicts across repositories: every repository is under its own
/// cap, the cache as a whole is not, and the least recently used leaves of ANY repository go
/// first until it fits. A leaf a floor holds (here the oldest of all) is never taken, and the
/// floor's own leaf is never taken for the others.
#[test]
fn the_total_cap_evicts_least_recently_used_across_repositories_never_a_leased_leaf() {
    let base = scratch("cache-total-cap");
    let cache = base.join("cache");
    let (_, running) = cap_leaf(&base, "core", "running", 600, 900);
    let (_, core_old) = cap_leaf(&base, "core", "old", 600, 500);
    let (_, crew_old) = cap_leaf(&base, "crew", "older", 600, 700);
    let (_, crew_new) = cap_leaf(&base, "crew", "new", 600, 100);
    let (own_wt, own) = cap_leaf(&base, "garden", "own", 600, 10);
    assert_ne!(running.parent(), crew_old.parent(), "two repositories");
    // A floor of `running` is mid-build.
    let held = lock_leaf(&running, false).unwrap().unwrap();
    held.set_modified(std::time::SystemTime::now() - Duration::from_secs(900))
        .unwrap();
    let own_lease = lock_leaf(&own, false).unwrap().unwrap();
    let caps = CacheCaps {
        repo: 10_000,
        total: 2_000,
        idle: CHECK_CACHE_IDLE_TTL,
        ..CacheCaps::uniform(0)
    };
    let (removed, own_over) = enforce_check_cache_cap(&cache, &own, caps);
    assert!(!own_over);
    assert_eq!(
        removed,
        vec![crew_old.clone(), core_old.clone()],
        "LRU across repositories, the leased leaf skipped, then stop once it fits"
    );
    assert!(
        running.join("cargo-target/head/blob").is_file(),
        "never a leased leaf"
    );
    assert!(
        crew_new.join("cargo-target/head/blob").is_file(),
        "the cache fits"
    );
    assert!(
        own.join("cargo-target/head/blob").is_file(),
        "never the floor's own"
    );
    drop(own_lease);
    // The same caps through the floor's own prepare: under them now, nothing more goes.
    let prepared = CheckScratch::prepare_with(&cache, &own_wt, Some("own"), caps).unwrap();
    assert!(crew_new.is_dir() && running.is_dir());
    drop(prepared);
    drop(held);
    let _ = std::fs::remove_dir_all(&base);
}

/// (codex on #1124) An orphaned leaf a floor still holds cannot be removed, but its bytes are on
/// disk: they count against the total cap, so an unleased leaf of another repository goes.
/// A leaf gone since the scan is never reported as this pass's eviction.
#[test]
fn a_held_orphan_counts_against_the_total_cap_and_a_vanished_leaf_is_not_an_eviction() {
    let base = scratch("cache-held-orphan");
    let cache = base.join("cache");
    let (_, orphan) = cap_leaf(&base, "core", "orphan", 800, 50);
    let (_, parked) = cap_leaf(&base, "crew", "parked", 600, 100);
    let (_, own) = cap_leaf(&base, "garden", "own", 400, 0);
    let held = lock_leaf(&orphan, false).unwrap().unwrap();
    std::fs::write(orphan_path(&orphan), b"").unwrap();
    let caps = CacheCaps {
        repo: u64::MAX,
        total: 1_200,
        idle: CHECK_CACHE_IDLE_TTL,
        ..CacheCaps::uniform(0)
    };
    let (removed, own_over) = enforce_check_cache_cap(&cache, &own, caps);
    assert!(!own_over);
    assert_eq!(
        removed,
        vec![parked.clone()],
        "800 + 600 + 400 is over 1200"
    );
    assert!(
        orphan.join("cargo-target/head/blob").is_file(),
        "the held orphan stays"
    );
    drop(held);
    // A leaf whose directory is gone (a reap between scan and lock) is not this pass's removal.
    assert_eq!(evict_leaf(&parked), Evicted::AlreadyGone);
    let _ = std::fs::remove_dir_all(&base);
}

/// (core#1122) The cache's own retention: a leaf unused past [`CacheCaps::idle`] goes whatever
/// the caps (a run whose terminal reap never came), unless a floor holds it; a recent leaf stays.
#[test]
fn an_idle_leaf_goes_whatever_the_caps_unless_a_floor_holds_it() {
    let base = scratch("cache-idle");
    let cache = base.join("cache");
    let day = 86_400;
    let (_, stale) = cap_leaf(&base, "core", "stale", 10, 9 * day);
    let (_, stale_held) = cap_leaf(&base, "crew", "stale-held", 10, 9 * day);
    let (_, recent) = cap_leaf(&base, "crew", "recent", 10, day);
    let (_, own) = cap_leaf(&base, "core", "own", 10, 0);
    let held = lock_leaf(&stale_held, false).unwrap().unwrap();
    held.set_modified(std::time::SystemTime::now() - Duration::from_secs(9 * day))
        .unwrap();
    let caps = CacheCaps {
        repo: u64::MAX,
        total: u64::MAX,
        idle: Duration::from_secs(7 * day),
        ..CacheCaps::uniform(0)
    };
    let (removed, own_over) = enforce_check_cache_cap(&cache, &own, caps);
    assert_eq!(removed, vec![stale.clone()]);
    assert!(!own_over);
    assert!(!stale.exists());
    assert!(stale_held.is_dir(), "a leaf a floor holds is never taken");
    assert!(recent.is_dir() && own.is_dir());
    drop(held);
    let _ = std::fs::remove_dir_all(&base);
}

/// (core#1122) The cap pass only ever removes run leaves: real directories two levels under the
/// cache. A LINK planted as a group or as a leaf (pointing outside the cache) is never
/// followed, so nothing outside the cache is ever measured or removed, even at a 0-byte cap.
#[cfg(unix)]
#[test]
fn the_cap_pass_never_removes_anything_outside_the_cache() {
    let base = scratch("cache-outside");
    let cache = base.join("cache");
    let outside = base.join("outside");
    std::fs::create_dir_all(outside.join("leafish").join("cargo-target")).unwrap();
    std::fs::write(outside.join("leafish/cargo-target/blob"), vec![0u8; 600]).unwrap();
    let (_, own) = cap_leaf(&base, "core", "own", 10, 0);
    let (_, victim) = cap_leaf(&base, "core", "victim", 600, 100);
    std::os::unix::fs::symlink(&outside, cache.join("linked-group")).unwrap();
    std::os::unix::fs::symlink(
        outside.join("leafish"),
        own.parent().unwrap().join("linked-leaf"),
    )
    .unwrap();
    let (removed, _) = enforce_check_cache_cap(&cache, &own, CacheCaps::uniform(0));
    assert_eq!(removed, vec![victim], "only the real run leaf");
    assert!(outside.join("leafish/cargo-target/blob").is_file());
    assert!(own
        .parent()
        .unwrap()
        .join("linked-leaf")
        .symlink_metadata()
        .is_ok());
    let _ = std::fs::remove_dir_all(&base);
}

/// Caps whose fixed limits are out of reach, so only the free-disk floor can bind: `floor`
/// bytes kept free on a simulated volume of `volume` bytes with `free` free.
fn floor_caps(floor: u64, volume: u64, free: u64) -> CacheCaps {
    CacheCaps {
        free_floor: FreeFloor { pct: 0, min: floor },
        disk: DiskProbe::Simulated {
            space: VolumeSpace {
                total: volume,
                free,
            },
            reclaim_pct: 100,
        },
        ..CacheCaps::uniform(u64::MAX)
    }
}

/// (core#1138) A volume below its free-disk floor: the cache is far under its fixed caps, yet
/// the least recently used leaves no floor holds go until the floor holds again (here 1000
/// bytes short: two 600-byte leaves), then the pass stops. The oldest leaf of all is leased
/// by a running floor and is never taken; with the floor off, the same cache keeps everything.
#[test]
fn a_volume_below_its_free_floor_evicts_below_the_fixed_cap_never_a_leased_leaf() {
    let base = scratch("cache-free-floor");
    let cache = base.join("cache");
    let (_, running) = cap_leaf(&base, "core", "running", 600, 900);
    let (_, old) = cap_leaf(&base, "core", "old", 600, 500);
    let (_, mid) = cap_leaf(&base, "crew", "mid", 600, 300);
    let (_, recent) = cap_leaf(&base, "crew", "recent", 600, 100);
    let (_, own) = cap_leaf(&base, "garden", "own", 600, 10);
    let held = lock_leaf(&running, false).unwrap().unwrap();
    held.set_modified(std::time::SystemTime::now() - Duration::from_secs(900))
        .unwrap();
    let own_lease = lock_leaf(&own, false).unwrap().unwrap();
    let off = CacheCaps {
        free_floor: FreeFloor::OFF,
        ..floor_caps(10_000, 100_000, 9_000)
    };
    assert_eq!(
        enforce_check_cache_cap(&cache, &own, off),
        (vec![], false),
        "the fixed caps alone keep every leaf"
    );
    // 3000 bytes cached, 9000 free, a 10000-byte floor: the cache may hold 2000.
    let caps = floor_caps(10_000, 100_000, 9_000);
    assert_eq!(
        effective_total_cap(caps, 3_000, caps.disk.space(&cache)),
        2_000
    );
    let (removed, own_over) = enforce_check_cache_cap(&cache, &own, caps);
    assert!(!own_over, "the floor never cleans the floor's own leaf");
    assert_eq!(
        removed,
        vec![old.clone(), mid.clone()],
        "LRU first, the leased leaf skipped, then stop once the floor holds"
    );
    assert!(
        running.join("cargo-target/head/blob").is_file(),
        "never a leased leaf"
    );
    assert!(recent.join("cargo-target/head/blob").is_file());
    assert!(own.join("cargo-target/head/blob").is_file());
    drop(own_lease);
    drop(held);
    let _ = std::fs::remove_dir_all(&base);
}

/// (codex on #1140) A removal can give back less than its leaf's apparent bytes (cargo
/// hard-links its outputs): the floor is held on fresh readings of the volume, so the pass
/// keeps going until a reading shows the floor holds. Here each 600-byte leaf gives back 300,
/// so the 1000-byte shortfall takes four leaves, not the two apparent bytes would credit; the
/// leased leaf is still never taken.
#[test]
fn the_free_floor_is_held_on_fresh_readings_not_on_apparent_bytes() {
    let base = scratch("cache-free-floor-reclaim");
    let cache = base.join("cache");
    let (_, running) = cap_leaf(&base, "core", "running", 600, 900);
    let (_, old) = cap_leaf(&base, "core", "old", 600, 500);
    let (_, mid) = cap_leaf(&base, "crew", "mid", 600, 300);
    let (_, recent) = cap_leaf(&base, "crew", "recent", 600, 100);
    let (_, newest) = cap_leaf(&base, "crew", "newest", 600, 50);
    let (_, own) = cap_leaf(&base, "garden", "own", 600, 10);
    let held = lock_leaf(&running, false).unwrap().unwrap();
    held.set_modified(std::time::SystemTime::now() - Duration::from_secs(900))
        .unwrap();
    let caps = CacheCaps {
        disk: DiskProbe::Simulated {
            space: VolumeSpace {
                total: 100_000,
                free: 9_000,
            },
            reclaim_pct: 50,
        },
        ..floor_caps(10_000, 100_000, 9_000)
    };
    let (removed, own_over) = enforce_check_cache_cap(&cache, &own, caps);
    assert!(!own_over);
    assert_eq!(
        removed,
        vec![old, mid, recent, newest],
        "three give back 900 of the 1000 short, so a fourth goes"
    );
    assert!(
        running.join("cargo-target/head/blob").is_file(),
        "never a leased leaf"
    );
    assert!(own.join("cargo-target/head/blob").is_file());
    drop(held);
    let _ = std::fs::remove_dir_all(&base);
}

/// (codex on #1140) The closing shortfall line fires whenever the latest reading is below the
/// floor, even when the cache is empty, and never blames an empty cache on running floors.
#[test]
fn the_shortfall_line_fires_on_the_reading_even_for_an_empty_cache() {
    let cache = Path::new("cache");
    assert_eq!(
        floor_shortfall(cache, Some(5_000), Some(5_000), 1_000),
        None
    );
    assert_eq!(
        floor_shortfall(cache, None, None, 1_000),
        None,
        "no reading, no floor"
    );
    let empty = floor_shortfall(cache, Some(1_000_000), Some(100), 0).unwrap();
    assert!(empty.contains("holds nothing more to remove"), "{empty}");
    assert!(!empty.contains("running"), "{empty}");
    let held = floor_shortfall(cache, Some(1_000_000_000), Some(0), 2_000_000_000).unwrap();
    assert!(
        held.contains("held by runs whose floors are running"),
        "{held}"
    );
    assert!(held.contains("1.0 GB short of its 1.0 GB floor"), "{held}");
}

/// (core#1138) A volume so full no eviction restores the floor: every leaf no floor holds goes,
/// and the leases running floors hold (and this floor's own) are never taken — the shortfall
/// is left for the next pass and for [`floor_cache_usage`] to report. Through the floor's own
/// prepare as well: it neither cleans its own leaf nor fails.
#[test]
fn a_floor_that_cannot_be_restored_leaves_only_leased_leaves_and_never_takes_one() {
    let base = scratch("cache-free-floor-short");
    let cache = base.join("cache");
    let (_, held_a) = cap_leaf(&base, "core", "held-a", 500, 700);
    let (_, held_b) = cap_leaf(&base, "crew", "held-b", 500, 600);
    let (_, idle_a) = cap_leaf(&base, "core", "idle-a", 500, 400);
    let (_, idle_b) = cap_leaf(&base, "crew", "idle-b", 500, 200);
    let (own_wt, own) = cap_leaf(&base, "garden", "own", 500, 10);
    let a = lock_leaf(&held_a, false).unwrap().unwrap();
    let b = lock_leaf(&held_b, false).unwrap().unwrap();
    let caps = floor_caps(1_000_000, 10_000_000, 0);
    let prepared = CheckScratch::prepare_with(&cache, &own_wt, Some("own"), caps).unwrap();
    assert!(
        !idle_a.exists() && !idle_b.exists(),
        "every unleased leaf went"
    );
    for leaf in [&held_a, &held_b, &own] {
        assert!(
            leaf.join("cargo-target/head/blob").is_file(),
            "a leased leaf is never evicted: {}",
            leaf.display()
        );
    }
    // Nothing left to take: a second pass removes nothing.
    let (removed, own_over) = enforce_check_cache_cap(&cache, &own, caps);
    assert_eq!((removed, own_over), (vec![], false));
    drop(prepared);
    drop((a, b));
    let _ = std::fs::remove_dir_all(&base);
}

/// (core#1138) The free-disk floor at its most aggressive (no free byte, a floor larger than
/// the volume) still only ever removes run leaves under the cache: a directory beside the
/// cache root, a LINKED group and a LINKED leaf (both pointing outside), the cache's own lock
/// and every lease file are left as they were.
#[cfg(unix)]
#[test]
fn the_free_floor_never_touches_anything_outside_the_cache_root() {
    let base = scratch("cache-free-floor-outside");
    let cache = base.join("cache");
    let outside = base.join("outside");
    std::fs::create_dir_all(outside.join("leafish").join("cargo-target")).unwrap();
    std::fs::write(outside.join("leafish/cargo-target/blob"), vec![0u8; 600]).unwrap();
    // A sibling of the cache root that looks like a group with a leaf.
    let beside = base.join("beside").join("core-x").join("r-y");
    std::fs::create_dir_all(&beside).unwrap();
    std::fs::write(beside.join("blob"), vec![0u8; 600]).unwrap();
    let (_, own) = cap_leaf(&base, "core", "own", 10, 0);
    let (_, victim) = cap_leaf(&base, "core", "victim", 600, 100);
    std::os::unix::fs::symlink(&outside, cache.join("linked-group")).unwrap();
    std::os::unix::fs::symlink(
        outside.join("leafish"),
        own.parent().unwrap().join("linked-leaf"),
    )
    .unwrap();
    let caps = floor_caps(u64::MAX, 1_000, 0);
    let (removed, _) = enforce_check_cache_cap(&cache, &own, caps);
    assert_eq!(removed, vec![victim.clone()], "only the real run leaf");
    assert!(outside.join("leafish/cargo-target/blob").is_file());
    assert!(beside.join("blob").is_file(), "never beside the cache root");
    assert!(cache.join("linked-group").symlink_metadata().is_ok());
    assert!(own
        .parent()
        .unwrap()
        .join("linked-leaf")
        .symlink_metadata()
        .is_ok());
    assert!(cache.join(CAP_PASS_LOCK).is_file());
    assert!(
        lease_path(&victim).is_file(),
        "the 0-byte lock file is kept"
    );
    assert!(lease_path(&own).is_file());
    let _ = std::fs::remove_dir_all(&base);
}

/// (core#1138) The floor is the larger of its share of the volume and its GB minimum, both
/// configurable; a value that does not parse keeps the default, and over 100% reads as 100%.
#[test]
fn the_free_floor_is_the_larger_of_its_share_and_its_minimum() {
    let gb = 1_000_000_000u64;
    let d = FreeFloor::parse(None, None);
    assert_eq!(
        d,
        FreeFloor {
            pct: 15,
            min: 20 * gb
        }
    );
    assert_eq!(d.bytes(500 * gb), 75 * gb, "15% of a 500 GB volume");
    assert_eq!(
        d.bytes(100 * gb),
        20 * gb,
        "the 20 GB minimum on a small one"
    );
    assert_eq!(
        FreeFloor::parse(Some(" 5 "), Some("x")),
        FreeFloor {
            pct: 5,
            min: 20 * gb
        }
    );
    assert_eq!(FreeFloor::parse(Some("250"), Some("0")).pct, 100);
    assert_eq!(FreeFloor::parse(Some("0"), Some("0")), FreeFloor::OFF);
    assert_eq!(FreeFloor::OFF.bytes(u64::MAX), 0);
    assert_eq!(
        FreeFloor { pct: 100, min: 0 }.bytes(u64::MAX),
        u64::MAX,
        "no overflow"
    );
    // An unreadable volume: the fixed cap alone.
    let caps = CacheCaps {
        free_floor: d,
        ..CacheCaps::uniform(40 * gb)
    };
    assert_eq!(effective_total_cap(caps, 25 * gb, None), 40 * gb);
    // The rig's reading (core#1138): 25 GB cached, 23 GB free on a 460 GB volume. The floor
    // is 69 GB, more than the cache could ever give back: every unleased leaf goes.
    let rig = VolumeSpace {
        total: 460 * gb,
        free: 23 * gb,
    };
    assert_eq!(effective_total_cap(caps, 25 * gb, Some(rig)), 0);
    // Plenty free: the fixed cap binds.
    let roomy = VolumeSpace {
        total: 460 * gb,
        free: 300 * gb,
    };
    assert_eq!(effective_total_cap(caps, 25 * gb, Some(roomy)), 40 * gb);
}

/// The real probe reads the volume a directory is on.
#[cfg(unix)]
#[test]
fn the_volume_probe_reads_the_cache_volume() {
    let base = scratch("cache-volume-probe");
    let s = DiskProbe::Volume
        .space(&base)
        .expect("statvfs reads the volume");
    assert!(s.total > 0 && s.free <= s.total, "{s:?}");
    assert_eq!(volume_space(&base.join("missing")), None);
    let _ = std::fs::remove_dir_all(&base);
}

/// (core#1138) The disk view reports the floor: a simulated volume short of its floor gives a
/// shortfall, an effective cap below the fixed one, and a note naming the floor cache; a
/// volume over it has no shortfall and no note; an unreadable one has `null` readings.
#[test]
fn the_floor_cache_usage_reports_a_free_disk_shortfall_naming_the_floor_cache() {
    let base = scratch("cache-usage-floor");
    let cache = base.join("cache");
    cap_leaf(&base, "core", "a", 700, 0);
    cap_leaf(&base, "crew", "c", 300, 0);
    let u = floor_cache_usage_in(&cache, floor_caps(5_000, 100_000, 3_000));
    let f = &u["freeDisk"];
    assert_eq!(f["volumeBytes"], 100_000);
    assert_eq!(f["freeBytes"], 3_000);
    assert_eq!(f["floorBytes"], 5_000);
    assert_eq!(f["shortfallBytes"], 2_000);
    assert_eq!(
        f["effectiveCapBytes"], 0,
        "1000 cached + 3000 free - 5000 floor"
    );
    let note = f["note"].as_str().expect("a note when short");
    assert!(note.contains("floor cache"), "{note}");
    assert!(note.contains(&*cache.to_string_lossy()), "{note}");
    let ok = floor_cache_usage_in(&cache, floor_caps(5_000, 100_000, 50_000));
    assert_eq!(ok["freeDisk"]["shortfallBytes"], 0);
    assert!(ok["freeDisk"]["note"].is_null());
    assert_eq!(ok["freeDisk"]["effectiveCapBytes"], 46_000);
    let unknown = floor_cache_usage_in(&cache, CacheCaps::uniform(5_000));
    assert!(unknown["freeDisk"]["freeBytes"].is_null());
    assert!(unknown["freeDisk"]["shortfallBytes"].is_null());
    assert_eq!(unknown["freeDisk"]["effectiveCapBytes"], 5_000);
    let _ = std::fs::remove_dir_all(&base);
}

/// (core#1122) The disk view crew's diagnostics reads: the cache's path and total, each
/// repository's bytes (largest first) and leaf count, the caps, and the log store beside it.
#[test]
fn the_floor_cache_usage_names_the_path_the_total_and_each_repository() {
    let base = scratch("cache-usage");
    let cache = base.join("cache");
    cap_leaf(&base, "core", "a", 700, 0);
    cap_leaf(&base, "core", "b", 300, 0);
    cap_leaf(&base, "crew", "c", 200, 0);
    std::fs::create_dir_all(base.join(crate::floor_log::LOG_SUBDIR).join("r-x")).unwrap();
    std::fs::write(
        base.join(crate::floor_log::LOG_SUBDIR).join("r-x/blob"),
        [0u8; 50],
    )
    .unwrap();
    let u = floor_cache_usage_in(&cache, CacheCaps::uniform(5_000));
    assert_eq!(u["path"], cache.to_string_lossy().as_ref());
    assert_eq!(u["bytes"], 1200);
    assert_eq!(u["totalCapBytes"], 5000);
    assert_eq!(u["repos"][0]["bytes"], 1000);
    assert_eq!(u["repos"][0]["leaves"], 2);
    assert!(u["repos"][0]["name"].as_str().unwrap().starts_with("core-"));
    assert_eq!(u["repos"][1]["bytes"], 200);
    assert_eq!(u["logs"]["bytes"], 50);
    let _ = std::fs::remove_dir_all(&base);
}

/// (core#1050) A checkout whose `tmp` is a SYMLINK or a regular FILE refuses the floor —
/// `passed: false` with main's reason — even though every check would pass; it is never a
/// logged warning beside a green floor.
#[cfg(unix)]
#[test]
fn a_linked_or_file_tmp_refuses_the_floor_even_when_every_check_passes() {
    for (kind, plant) in [("symlink", true), ("not a directory", false)] {
        let base = scratch(&format!("tmp-refuse-{}", plant));
        let wt = base.join("wt");
        let outside = base.join("outside");
        std::fs::create_dir_all(wt.join(".wicked")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            wt.join(".wicked/checks.json"),
            serde_json::json!({ "test": ["sh", "-c", "exit 0"] }).to_string(),
        )
        .unwrap();
        let tmp = wt.join(crate::worktree_guard::ENGINE_SCRATCH_DIR);
        if plant {
            std::os::unix::fs::symlink(&outside, &tmp).unwrap();
        } else {
            std::fs::write(&tmp, "a file").unwrap();
        }
        let report = run_floor(&wt, &FloorContext::default());
        assert!(!report.passed, "{kind}: {report:?}");
        assert!(report.checks.is_empty(), "{kind}: no check ran: {report:?}");
        let why = report.detect_error.as_deref().unwrap_or_default();
        assert!(why.contains(kind), "{kind}: {why}");
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// (codex on #1050) Only "not found" is absence: a stale base export the floor cannot even
/// inspect (its `tmp` has no search permission) refuses the floor, never a pass beside it.
#[cfg(unix)]
#[test]
fn an_uninspectable_base_export_refuses_the_floor() {
    if crate::validator::skip_in_container_floor("an_uninspectable_base_export_refuses_the_floor") {
        return;
    }
    if crate::validator::skip_when_nested("an_uninspectable_base_export_refuses_the_floor") {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let base = scratch("tmp-noperm");
    let wt = base.join("wt");
    std::fs::create_dir_all(wt.join(".wicked")).unwrap();
    std::fs::write(
        wt.join(".wicked/checks.json"),
        serde_json::json!({ "test": ["sh", "-c", "exit 0"] }).to_string(),
    )
    .unwrap();
    let tmp = wt.join(crate::worktree_guard::ENGINE_SCRATCH_DIR);
    std::fs::create_dir_all(tmp.join(BASE_EXPORT_SUBDIR)).unwrap();
    std::fs::write(tmp.join(BASE_EXPORT_SUBDIR).join("stale.ts"), "old").unwrap();
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::symlink_metadata(tmp.join(BASE_EXPORT_SUBDIR)).is_ok() {
        // Running as root: permissions do not bind, nothing to prove here.
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    // A boundary injected, so the refusal is reached whatever sandbox tool the host has.
    let report = run_with_sandbox(
        &wt,
        WorkerSandbox {
            wrapper: vec!["true".to_string()],
            level: SandboxLevel::Sandboxed,
            downgrade_reason: None,
        },
    );
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!report.passed, "{report:?}");
    assert!(
        report
            .detect_error
            .as_deref()
            .is_some_and(|e| e.contains("cannot be inspected")),
        "{report:?}"
    );
    assert!(report.checks.is_empty(), "no check ran: {report:?}");
    let _ = remove_check_scratch_in(&check_cache_root().unwrap(), &wt, None);
    let _ = std::fs::remove_dir_all(&base);
}

/// (codex on #1050) The orphan mark is checked again UNDER the lease: a reap that marks the
/// leaf between a floor's first look and its lease never hands that floor the orphan — it is
/// refused while another floor holds the orphan, and gets a fresh leaf once it is free.
#[test]
fn an_orphan_marked_before_the_lease_is_never_reused() {
    let base = scratch("cache-orphan-race");
    let leaf = base.join("cache").join("repo-x").join("run-x");
    std::fs::create_dir_all(leaf.join("cargo-target")).unwrap();
    std::fs::write(leaf.join("cargo-target/old-binary"), "old").unwrap();
    let holder = lock_leaf(&leaf, false).unwrap().unwrap();
    std::fs::write(orphan_path(&leaf), b"").unwrap();
    let err = take_orphan_free_lease(&leaf).expect_err("held orphan: refused");
    assert!(err.to_string().contains("orphaned"), "{err}");
    assert!(leaf.join("cargo-target/old-binary").is_file());
    drop(holder);
    let lease = take_orphan_free_lease(&leaf).expect("a free orphan is retired, then leased");
    assert!(
        !leaf.join("cargo-target/old-binary").exists(),
        "never reused"
    );
    assert!(!orphan_path(&leaf).exists());
    drop(lease);
    let _ = std::fs::remove_dir_all(&base);
}

/// (core#1482) Each floor's base build output goes when the run's last floor on the leaf ends;
/// the head target stays. Adversarial (SILENT-OK / data loss): a second floor still building
/// keeps it, and a check that turned `base` or `cargo-target` into a symlink never gets the
/// removal followed out of the scratch.
#[test]
fn core_1482_the_last_floor_out_drops_its_base_output_and_follows_no_link() {
    let base = scratch("cache-drop-base");
    let cache = base.join("cache");
    let wt = base.join("repo").join("wicked-worktrees").join("r1");
    std::fs::create_dir_all(&wt).unwrap();
    let fill = |leaf: &Path| {
        std::fs::write(leaf.join("cargo-target/head/h.bin"), "h").unwrap();
        std::fs::write(leaf.join("cargo-target/base/b.bin"), "b").unwrap();
    };
    // Two floors of one run share the leaf: the first out leaves the other's base alone.
    let a = CheckScratch::prepare_in(&cache, &wt, Some("r1"), u64::MAX).unwrap();
    let leaf = a.root().to_path_buf();
    let b = CheckScratch::prepare_in(&cache, &wt, Some("r1"), u64::MAX).unwrap();
    fill(&leaf);
    drop(a);
    assert!(
        leaf.join("cargo-target/base/b.bin").is_file(),
        "b still builds"
    );
    drop(b);
    assert!(
        !leaf.join("cargo-target/base").exists(),
        "the last out drops base"
    );
    let aside: Vec<_> = std::fs::read_dir(leaf.join("cargo-target"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(".base-drop-"))
        .collect();
    assert!(aside.is_empty(), "set aside, then removed");
    assert!(leaf.join("cargo-target/head/h.bin").is_file(), "head stays");
    // A link a check left: unlinked, its target untouched.
    #[cfg(unix)]
    {
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), "k").unwrap();
        let c = CheckScratch::prepare_in(&cache, &wt, Some("r1"), u64::MAX).unwrap();
        let _ = std::fs::remove_dir_all(leaf.join("cargo-target/base"));
        std::os::unix::fs::symlink(&outside, leaf.join("cargo-target/base")).unwrap();
        drop(c);
        assert!(outside.join("keep").is_file(), "never followed");
        assert!(std::fs::symlink_metadata(leaf.join("cargo-target/base")).is_err());
        let d = CheckScratch::prepare_in(&cache, &wt, Some("r1"), u64::MAX).unwrap();
        std::fs::remove_dir_all(leaf.join("cargo-target")).unwrap();
        std::os::unix::fs::symlink(&outside, leaf.join("cargo-target")).unwrap();
        drop(d);
        assert!(outside.join("keep").is_file(), "never followed");
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// (core#1050) Two runs whose ids sanitize to ONE worktree path never share a check-scratch
/// leaf: the first run's leaf, held by its floor when its worktree is removed, is marked
/// orphaned rather than left reusable, the second run starts on its own empty leaf, and the
/// orphan goes when the first floor ends (or, failing that, at the next cap pass).
#[test]
fn colliding_run_ids_never_share_a_check_scratch() {
    let base = scratch("cache-collide");
    let cache = base.join("cache");
    let wt = base.join("repo").join("wicked-worktrees").join("a-b_c_a0");
    std::fs::create_dir_all(&wt).unwrap();
    let (run_a, run_b) = ("a-b:c:a0", "a:b-c:a0");
    let leaf_a = scratch_dir_for(&cache, &wt, Some(run_a));
    let leaf_b = scratch_dir_for(&cache, &wt, Some(run_b));
    assert_ne!(leaf_a, leaf_b, "one tree, two runs, two leaves");
    assert_eq!(leaf_a.parent(), leaf_b.parent(), "one repository group");
    // Run A's floor is building when A is cancelled and its worktree removed.
    let floor_a = CheckScratch::prepare_in(&cache, &wt, Some(run_a), u64::MAX).unwrap();
    std::fs::write(leaf_a.join("cargo-target/head/a-binary"), "A").unwrap();
    assert!(
        !remove_check_scratch_in(&cache, &wt, Some(run_a)),
        "held: not removed"
    );
    assert!(orphan_path(&leaf_a).exists(), "marked orphaned instead");
    // Run B, a distinct run on the same path, never sees A's output.
    let floor_b = CheckScratch::prepare_in(&cache, &wt, Some(run_b), u64::MAX).unwrap();
    assert_eq!(floor_b.root(), leaf_b);
    assert!(!leaf_b.join("cargo-target/head/a-binary").exists());
    // A's floor ends: its orphaned leaf goes with it.
    drop(floor_a);
    assert!(!leaf_a.exists() && !orphan_path(&leaf_a).exists());
    // An orphan no floor could retire is removed by the next cap pass, whatever the cap.
    std::fs::create_dir_all(leaf_a.join("home")).unwrap();
    std::fs::write(orphan_path(&leaf_a), b"").unwrap();
    let (removed, _) = enforce_check_cache_cap(&cache, &leaf_b, CacheCaps::uniform(u64::MAX));
    assert_eq!(removed, vec![leaf_a.clone()]);
    assert!(!leaf_a.exists() && leaf_b.is_dir());
    drop(floor_b);
    let _ = std::fs::remove_dir_all(&base);
}

/// (codex on #1050) The run's terminal reap keeps the leaf's lock file: a floor of a resumed
/// run that already opened it and then takes its lease holds a lock the next eviction sees,
/// so the eviction never removes the scratch that floor is building into.
#[test]
fn a_reap_overlapping_a_floors_lease_never_lets_the_cap_take_its_scratch() {
    let base = scratch("cache-reap-race");
    let cache = base.join("cache");
    let wts = base.join("repo").join("wicked-worktrees");
    let (wt, other) = (wts.join("run-a"), wts.join("run-b"));
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    drop(CheckScratch::prepare_in(&cache, &wt, None, u64::MAX).unwrap());
    let leaf = scratch_dir_for(&cache, &wt, None);
    // The resumed run's floor has the lock file open, about to take its lease.
    let waiting = std::fs::OpenOptions::new()
        .write(true)
        .open(lease_path(&leaf))
        .unwrap();
    assert!(
        remove_check_scratch_in(&cache, &wt, None),
        "the terminal reap"
    );
    assert!(!leaf.exists());
    waiting.lock_shared().unwrap();
    std::fs::create_dir_all(leaf.join("cargo-target").join("head")).unwrap();
    std::fs::write(leaf.join("cargo-target/head/blob"), vec![0u8; 64]).unwrap();
    let own = scratch_dir_for(&cache, &other, None);
    let (removed, _) = enforce_check_cache_cap(&cache, &own, CacheCaps::uniform(0));
    assert!(removed.is_empty(), "{removed:?}");
    assert!(
        leaf.join("cargo-target/head/blob").is_file(),
        "the leased scratch survives"
    );
    drop(waiting);
    let _ = std::fs::remove_dir_all(&base);
}

/// (codex on #1050) The base export stays in the worktree's engine scratch, so a base check
/// that calls git still finds the enclosing repository (as from `tmp/wicked-checks/base`).
/// From the check cache it would find none, fail before printing its failure ids, and turn
/// the head's pre-existing failure into a "regression".
#[cfg(unix)]
#[test]
fn a_git_dependent_base_check_still_finds_its_repository() {
    let repo = scratch("base-git");
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    std::fs::write(repo.join(".gitignore"), "tmp/\n").unwrap();
    let script = "git rev-parse --is-inside-work-tree >/dev/null 2>&1 || { echo NOGIT; \
                      exit 2; }; echo 'test t::shared ... FAILED'; exit 101";
    std::fs::write(
        repo.join(".wicked/checks.json"),
        serde_json::json!({ "test": ["sh", "-c", script] }).to_string(),
    )
    .unwrap();
    let base = git_repo_with_commit(&repo);
    let ctx = FloorContext {
        stage: FloorStage::Verify,
        base_head: Some(base),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    let report = run_floor(&repo, &ctx);
    if report.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the baseline diff cannot run");
        return;
    }
    let c = report
        .checks
        .iter()
        .find(|c| c.name == "test")
        .expect("test ran");
    let b = c
        .base
        .as_ref()
        .and_then(|b| b.run.as_ref())
        .expect("the base ran");
    assert!(!b.stdout_tail.contains("NOGIT"), "{b:?}");
    assert_eq!(b.failure_ids, s(&["test t::shared"]), "{b:?}");
    // DES-FLOOR-V2: an `sh` runner has no precise deselect, so the shared red escalates.
    assert_eq!(
        c.classification.as_deref(),
        Some(BASE_RED_UNSEPARATED),
        "{c:#?}"
    );
    assert!(!report.passed, "{report:?}");
    assert_no_worktree_scratch(&repo);
    assert!(!repo.join("tmp").join(BASE_EXPORT_SUBDIR).exists());
    assert!(remove_check_scratch_in(
        &check_cache_root().unwrap(),
        &repo,
        None
    ));
    let _ = std::fs::remove_dir_all(&repo);
}

/// (codex on #1050) A base export an older engine left at `tmp/wicked-checks/base` (it died
/// mid-baseline) is removed before HEAD checks, as that engine itself did — and only that: the
/// rest of the legacy dir stays, and a linked `wicked-checks` is never followed.
#[test]
fn a_legacy_base_export_is_removed_before_head_checks() {
    let base = scratch("legacy-base");
    let wt = base.join("wt");
    let legacy = wt.join("tmp").join("wicked-checks");
    std::fs::create_dir_all(legacy.join("base").join("src")).unwrap();
    std::fs::write(legacy.join("base/src/old.test.ts"), "fails").unwrap();
    std::fs::create_dir_all(legacy.join("home")).unwrap();
    std::fs::write(legacy.join("home/keep"), "x").unwrap();
    let scratch_dir = CheckScratch::prepare(&wt).unwrap();
    let held = HeldDir::open(&wt).unwrap();
    remove_base_export(&held).unwrap();
    assert!(!legacy.join("base").exists(), "the legacy export is gone");
    assert!(
        legacy.join("home/keep").is_file(),
        "the rest of the legacy dir is not touched"
    );
    // A linked `wicked-checks` is not followed.
    #[cfg(unix)]
    {
        let outside = base.join("outside");
        std::fs::create_dir_all(outside.join("base")).unwrap();
        std::fs::remove_dir_all(&legacy).unwrap();
        std::os::unix::fs::symlink(&outside, &legacy).unwrap();
        remove_base_export(&held).unwrap();
        assert!(
            outside.join("base").is_dir(),
            "nothing removed through the link"
        );
    }
    drop(scratch_dir);
    assert!(remove_check_scratch_in(
        &check_cache_root().unwrap(),
        &wt,
        None
    ));
    let _ = std::fs::remove_dir_all(&base);
}

/// Two consecutive floors over one run's tree reuse its leaf (warm, as when the scratch lived
/// in the worktree); another run of the SAME repository gets its own leaf and never sees
/// the first run's build output (a shared target dir hands one run another's binaries by
/// mtime — a false pass); neither floor creates `tmp/wicked-checks` in its tree; the reap
/// removes the leaf and leaves the tree's files alone.
#[test]
fn consecutive_floors_on_one_run_reuse_its_scratch_and_another_run_never_sees_it() {
    let base = scratch("cache-reuse");
    let wts = base.join("repo").join("wicked-worktrees");
    let probe = "if test -f \"$CARGO_TARGET_DIR/warm\"; then echo SCRATCH=WARM; else echo \
                     SCRATCH=COLD; fi; touch \"$CARGO_TARGET_DIR/warm\"";
    let tree = |name: &str| {
        let wt = wts.join(name);
        std::fs::create_dir_all(wt.join(".wicked")).unwrap();
        std::fs::write(
            wt.join(".wicked/checks.json"),
            serde_json::json!({ "test": ["sh", "-c", probe] }).to_string(),
        )
        .unwrap();
        wt
    };
    let (a, b) = (tree("run-a"), tree("run-b"));
    let said = |wt: &Path| {
        let r = run_floor(wt, &FloorContext::default());
        assert_no_worktree_scratch(wt);
        if r.sandbox_error.is_some() {
            return None;
        }
        let c = r
            .checks
            .iter()
            .find(|c| c.name == "test")
            .expect("test ran");
        assert!(c.passed(), "{c:?}");
        Some(c.stdout_tail.contains("SCRATCH=WARM"))
    };
    let Some(first) = said(&a) else {
        eprintln!("repo_checks: no sandbox tool here — the reuse test cannot run the floor");
        return;
    };
    assert!(!first, "a run's first floor starts cold");
    assert_eq!(
        said(&a),
        Some(true),
        "the run's next floor reuses its scratch"
    );
    assert_eq!(
        said(&b),
        Some(false),
        "another run never sees the first run's output"
    );
    let cache = check_cache_root().unwrap();
    assert!(remove_check_scratch_in(&cache, &a, None));
    assert!(!leaf_of(&a).exists());
    assert!(
        a.join(".wicked/checks.json").is_file(),
        "the tree's files are untouched"
    );
    assert!(remove_check_scratch_in(&cache, &b, None));
    let _ = std::fs::remove_dir_all(&base);
}

/// `cargo test --locked` when the repo ships a lockfile (the check must not rewrite it);
/// plain `cargo test` when it does not — the engine-written `Cargo.lock` is then removed
/// after the checks (see `a_failing_cargo_test_is_captured…`).
#[test]
fn cargo_runs_locked_when_a_lockfile_ships() {
    let wt = scratch("cargo-locked");
    std::fs::write(wt.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
    std::fs::write(wt.join("Cargo.lock"), "# lock\n").unwrap();
    let checks = detect(&wt).unwrap();
    assert_eq!(checks[0].name, "cargo-fmt-check");
    assert_eq!(
        checks[1].argv,
        s(&["cargo", "test", "--locked", "--no-fail-fast"])
    );
    assert!(engine_generated_candidates(&wt, &checks).is_empty());
    std::fs::remove_file(wt.join("Cargo.lock")).unwrap();
    let checks = detect(&wt).unwrap();
    assert_eq!(checks[1].argv, s(&["cargo", "test", "--no-fail-fast"]));
    assert_eq!(
        engine_generated_candidates(&wt, &checks),
        vec!["Cargo.lock".to_string()]
    );
}

#[test]
fn stops_at_the_first_failure_and_lists_what_it_skipped() {
    let wt = scratch("stop");
    let sandbox = sandbox_for(&wt);
    let scratch = CheckScratch::prepare(&wt).unwrap();
    // A check that cannot even spawn fails the floor and skips everything after it.
    let checks = vec![
        RepoCheck {
            name: "typecheck".into(),
            argv: s(&["definitely-not-a-binary-wicked-xyz", "run"]),
            source: "fixture".into(),
            timeout_s: None,
            dir: String::new(),
        },
        RepoCheck {
            name: "test".into(),
            argv: s(&["definitely-not-a-binary-wicked-xyz", "run"]),
            source: "fixture".into(),
            timeout_s: None,
            dir: String::new(),
        },
    ];
    let mut runs = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = false;
    for c in &checks {
        if failed {
            skipped.push(c.name.clone());
            continue;
        }
        let r = run_one(&wt, c, &sandbox, &scratch, Tree::Head);
        failed = !r.passed();
        runs.push(r);
    }
    assert_eq!(runs.len(), 1);
    assert!(runs[0]
        .spawn_error
        .as_deref()
        .is_some_and(|e| e.contains("not on PATH")));
    assert_eq!(skipped, vec!["test"]);
    let report = RepoChecksReport {
        targeted_test: None,
        detected: checks,
        checks: runs,
        skipped,
        passed: false,
        detect_error: None,
        sandbox_level: sandbox.level.as_wire().to_string(),
        sandbox_note: None,
        sandbox_error: None,
        engine_writes_removed: Vec::new(),
        claim: None,
        env: None,
        tree: None,
        rerun: None,
        waived: Vec::new(),
        requested_rerun: None,
        recheck: None,
        coverage_note: None,
        key: None,
        resumed: None,
        reused_from: None,
        runner: None,
    };
    let denial = report.denial_reason();
    assert!(denial.contains("could not run") && denial.contains("not run after the failure: test"));
}

#[test]
fn a_passing_check_passes_and_a_tail_is_bounded() {
    let wt = scratch("pass");
    let echo = RepoCheck {
        name: "test".into(),
        argv: s(&[
            "sh",
            "-c",
            "yes long-line-of-output | head -c 100000; exit 0",
        ]),
        source: "fixture".into(),
        timeout_s: None,
        dir: String::new(),
    };
    if crate::validator::find_on_path("sh").is_none() {
        eprintln!("repo_checks: sh not on PATH — skipping the bounded-tail check");
        return;
    }
    let sandbox = sandbox_for(&wt);
    let scratch = CheckScratch::prepare(&wt).unwrap();
    let r = run_one(&wt, &echo, &sandbox, &scratch, Tree::Head);
    assert!(r.passed(), "{r:?}");
    assert!(
        r.stdout_tail.len() <= TAIL_BYTES,
        "the tail is bounded to {TAIL_BYTES} bytes, got {}",
        r.stdout_tail.len()
    );
    assert!(r.stdout_tail.contains("long-line-of-output"));
}

pub(crate) fn git(repo: &Path, args: &[&str]) -> String {
    // spawn-audit: test-only — builds the baseline-diff fixture repository.
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// core#417 (operator ruling (a)): a guarded verify floor runs on a clean checkout of the
/// tree the worktree guard hashed. An IGNORED file the seat planted in the live worktree is
/// absent there and cannot change the verdict: the same check FAILS on the live worktree (it
/// sees the plant) and PASSES on the guarded checkout. Tracked and untracked-not-ignored
/// content is there, with git metadata, and the checkout is removed when dropped.
#[test]
fn a_guarded_verify_floor_runs_on_the_guards_tree_where_an_ignored_plant_cannot_reach() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let wt = scratch("guarded-checkout");
    std::fs::write(wt.join("app.txt"), "v1\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("app.txt", wt.join("link")).unwrap();
    std::fs::write(wt.join(".gitignore"), ".env.local\n").unwrap();
    std::fs::create_dir_all(wt.join(".wicked")).unwrap();
    std::fs::write(
            wt.join(CONFIG_PATH),
            r#"{"typecheck": false, "lint": false, "test": ["sh", "-c", "test ! -e .env.local && test -e new.txt"]}"#,
        )
        .unwrap();
    git_repo_with_commit(&wt);
    // The creator's uncommitted work, and the plant: ignored, so the guard's hash skips it.
    std::fs::write(wt.join("new.txt"), "new\n").unwrap();
    std::fs::write(wt.join(".env.local"), "STEER=1\n").unwrap();
    let git_dir = PathBuf::from(git(&wt, &["rev-parse", "--absolute-git-dir"]));
    let snap = crate::worktree_guard::snapshot_through(&wt, &git_dir).expect("snapshot");

    // No repository hook may run while the engine builds the checkout (codex r2).
    let hooks = wt.join("repo-hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let marker = wt.join("hook-ran");
    for h in ["post-checkout", "post-index-change"] {
        let hook = hooks.join(h);
        std::fs::write(
            &hook,
            format!("#!/bin/sh\necho {h} >> '{}'\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    git(&wt, &["config", "core.hooksPath", &hooks.to_string_lossy()]);
    let checkout =
        checkout_guarded_tree(&wt, &git_dir, &snap.tree, "t-417").expect("guarded checkout");
    assert!(
        !marker.exists(),
        "a repository hook ran while the checkout was built"
    );
    let dir = checkout.dir.clone();
    assert!(dir.join("app.txt").is_file() && dir.join("new.txt").is_file());
    assert!(
        !dir.join(".env.local").exists(),
        "the ignored plant never reaches the floor"
    );
    assert!(dir.join(".git").exists(), "the checks keep git metadata");

    let live = run_floor_rerun(&wt, &FloorContext::default(), None);
    let guarded = run_floor_rerun(&dir, &FloorContext::default(), None);
    if !live.checks.is_empty() {
        // A host with an OS write boundary ran the checks: the verdicts must differ.
        assert!(!live.passed, "the live worktree sees the plant: {live:?}");
        assert!(guarded.passed, "the guarded tree does not: {guarded:?}");
    }
    // A check that rewrites tracked content in the checkout cannot pass on it (codex r2):
    // the floor certifies only the guarded tree, untouched. An untracked artifact is fine.
    let mut ok = guarded.clone();
    ok.passed = true;
    std::fs::write(dir.join("coverage.out"), "artifact\n").unwrap();
    checkout.hold_to(&snap.tree, &mut ok);
    assert!(
        ok.passed,
        "an untracked artifact is not a change to what ships: {ok:?}"
    );
    // (codex r3) A mode flip or a retargeted symlink is a change to what ships, too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut flipped = ok.clone();
        let app = dir.join("app.txt");
        std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o755)).unwrap();
        checkout.hold_to(&snap.tree, &mut flipped);
        assert!(!flipped.passed, "a chmod +x is a change: {flipped:?}");
        std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut relinked = ok.clone();
        let link = dir.join("link");
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("new.txt", &link).unwrap();
        checkout.hold_to(&snap.tree, &mut relinked);
        assert!(
            !relinked.passed,
            "a retargeted symlink is a change: {relinked:?}"
        );
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("app.txt", &link).unwrap();
        // (sweep review on #1066) A directory of the tree swapped for a link to an outside
        // copy with the SAME content is a change too: every entry is re-read through the
        // checkout held before the checks ran, never through the link.
        let mut swapped = ok.clone();
        let copy = scratch("guarded-checkout-outside-copy");
        std::fs::copy(dir.join(CONFIG_PATH), copy.join("checks.json")).unwrap();
        std::fs::rename(dir.join(".wicked"), dir.join(".wicked-moved")).unwrap();
        std::os::unix::fs::symlink(&copy, dir.join(".wicked")).unwrap();
        checkout.hold_to(&snap.tree, &mut swapped);
        assert!(
            !swapped.passed,
            "a linked-away directory is a change: {swapped:?}"
        );
        assert!(
            swapped
                .detect_error
                .as_deref()
                .is_some_and(|e| e.contains(CONFIG_PATH)),
            "{swapped:?}"
        );
        std::fs::remove_file(dir.join(".wicked")).unwrap();
        std::fs::rename(dir.join(".wicked-moved"), dir.join(".wicked")).unwrap();
        let _ = std::fs::remove_dir_all(&copy);
    }
    std::fs::write(dir.join("app.txt"), "rewritten by a check\n").unwrap();
    checkout.hold_to(&snap.tree, &mut ok);
    assert!(!ok.passed, "{ok:?}");
    assert!(
        ok.detect_error
            .as_deref()
            .is_some_and(|e| e.contains("app.txt")),
        "{ok:?}"
    );
    // The guarded checkout's check scratch is its own leaf, never the live tree's
    // (core#417), and it goes with the checkout; the live tree's stays for the run.
    let (live_leaf, guarded_leaf) = (leaf_of(&wt), leaf_of(&dir));
    assert_ne!(live_leaf, guarded_leaf);
    assert!(guarded_leaf.is_dir() && live_leaf.is_dir());
    assert_no_worktree_scratch(&dir);
    drop(checkout);
    assert!(!dir.exists(), "the checkout is removed when dropped");
    assert!(
        !guarded_leaf.exists(),
        "the checkout's check scratch is removed with it: {}",
        guarded_leaf.display()
    );
    assert!(live_leaf.is_dir());
    assert!(remove_check_scratch_in(
        &check_cache_root().unwrap(),
        &wt,
        None
    ));
    let listed = git(&wt, &["worktree", "list", "--porcelain"]);
    assert!(!listed.contains("wicked-core-floor"), "{listed}");
    let _ = std::fs::remove_dir_all(&wt);
}

pub(crate) fn git_repo_with_commit(repo: &Path) -> String {
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@example.invalid"]);
    git(repo, &["config", "user.name", "t"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
    git(repo, &["config", "core.autocrlf", "false"]);
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "base"]);
    git(repo, &["rev-parse", "HEAD"])
}

pub(crate) fn names(checks: &[RepoCheck]) -> Vec<&str> {
    checks.iter().map(|c| c.name.as_str()).collect()
}

/// core#469: a check that hits its bound is `timed_out`, never `failed` — the classification
/// rides the run, the report and the denial source, and the denial says what the gate can do.
#[test]
fn a_check_that_hits_its_bound_is_timed_out_not_failed() {
    if crate::validator::find_on_path("sh").is_none() {
        eprintln!("repo_checks: sh not on PATH — skipping the timeout classification test");
        return;
    }
    let wt = scratch("timeout");
    let sandbox = sandbox_for(&wt);
    let scratch = CheckScratch::prepare(&wt).unwrap();
    let slow = RepoCheck {
        name: "test".into(),
        argv: s(&["sh", "-c", "sleep 30"]),
        source: "fixture".into(),
        timeout_s: Some(1),
        dir: String::new(),
    };
    let started = Instant::now();
    let r = run_one(&wt, &slow, &sandbox, &scratch, Tree::Head);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "killed at the bound, not after the sleep: {:?}",
        started.elapsed()
    );
    assert!(r.timed_out && !r.passed() && r.denies(), "{r:?}");
    assert_eq!(r.outcome(), "timed_out");
    assert!(
        (1..=3).contains(&r.bound_s),
        "a 1s base × a host-load factor capped at 3: {} ({:?})",
        r.bound_s,
        r.bound_note
    );
    assert!(r.summary().contains("TIMED OUT"), "{}", r.summary());
    let report = RepoChecksReport {
        targeted_test: None,
        detected: vec![slow.clone()],
        checks: vec![r],
        skipped: vec!["cargo-test".into()],
        passed: false,
        detect_error: None,
        sandbox_level: sandbox.level.as_wire().to_string(),
        sandbox_note: None,
        sandbox_error: None,
        engine_writes_removed: Vec::new(),
        claim: None,
        env: None,
        tree: None,
        rerun: None,
        waived: Vec::new(),
        requested_rerun: None,
        recheck: None,
        coverage_note: None,
        key: None,
        resumed: None,
        reused_from: None,
        runner: None,
    };
    assert!(report.timed_out());
    assert_eq!(report.outcome(), "timed_out");
    assert_eq!(report.denial_source(), DENIAL_SOURCE_TIMEOUT);
    let denial = report.denial_reason();
    assert!(
        denial.contains("did not FINISH")
            && denial.contains("unverified by it, not refuted")
            && denial.contains("timeout_s")
            && denial.contains("test_targeted")
            && denial.contains("not run after the failure: cargo-test"),
        "{denial}"
    );
    assert!(
        !denial.contains("repo checks floor failed:"),
        "a timeout is never worded as a failure: {denial}"
    );
    // (core#1392) The denial says whether a targeted re-run is offered, and what it runs.
    assert!(
        denial.contains("none is declared, so a targeted re-run is not offered"),
        "{denial}"
    );
    let with = RepoChecksReport {
        targeted_test: Some(format!("{CONFIG_PATH} test_targeted")),
        ..report
    };
    assert!(
        with.denial_reason()
            .contains("re-run with the targeted tests (`.wicked/checks.json test_targeted`)"),
        "{}",
        with.denial_reason()
    );
}

/// (power-speed) A power run at high rigor or above hands off on the FULL suite at the creator
/// floor too, with the same argv the verify floor runs, so verify reuses it rather than
/// running the suite after a targeted pass. Below high, and outside power mode, the creator
/// keeps its targeted set.
#[test]
fn a_power_run_at_high_rigor_runs_the_full_suite_at_the_creator_floor_too() {
    let wt = scratch("power-full");
    std::fs::write(
        wt.join("package.json"),
        r#"{"scripts":{"typecheck":"tsc --noEmit","test":"vitest run"}}"#,
    )
    .unwrap();
    std::fs::write(wt.join(".gitignore"), "node_modules/\ntmp/\n").unwrap();
    std::fs::create_dir_all(wt.join("node_modules")).unwrap();
    std::fs::create_dir_all(wt.join(".wicked")).unwrap();
    std::fs::write(
        wt.join(CONFIG_PATH),
        r#"{"test_targeted":["npx","vitest","related","--run","{files}"],"lint":false}"#,
    )
    .unwrap();
    let base = git_repo_with_commit(&wt);
    std::fs::write(wt.join("src.ts"), "changed\n").unwrap();
    let run = "power-full-creator-run";
    let ctx = FloorContext {
        stage: FloorStage::Creator,
        base_head: Some(base),
        git_dir: Some(wt.join(".git")),
        run_id: Some(run.to_string()),
        ..FloorContext::default()
    };
    let verify_ctx = FloorContext {
        stage: FloorStage::Verify,
        ..ctx.clone()
    };
    crate::power::set_full_suite(run, false);
    assert_eq!(
        names(&detect_with(&wt, &ctx).unwrap()),
        vec!["typecheck", "test_targeted"],
        "below high the creator floor stays targeted"
    );
    crate::power::set_full_suite(run, true);
    let creator = detect_with(&wt, &ctx).unwrap();
    let verify = detect_with(&wt, &verify_ctx).unwrap();
    crate::power::set_full_suite(run, false);
    assert_eq!(names(&creator), vec!["typecheck", "test"]);
    assert_eq!(names(&verify), vec!["typecheck", "test"]);
    assert_eq!(
        creator[1].argv, verify[1].argv,
        "the same command, so the verify floor can reuse the creator's run"
    );
    // (xreview on #1184) A repo whose ONLY test is its targeted one keeps it at both floors.
    std::fs::write(
        wt.join("package.json"),
        r#"{"scripts":{"typecheck":"tsc --noEmit"}}"#,
    )
    .unwrap();
    crate::power::set_full_suite(run, true);
    let creator = detect_with(&wt, &ctx).unwrap();
    let verify = detect_with(&wt, &verify_ctx).unwrap();
    crate::power::set_full_suite(run, false);
    assert_eq!(names(&creator), vec!["typecheck", "test_targeted"]);
    assert_eq!(names(&verify), vec!["typecheck", "test_targeted"]);
    let _ = std::fs::remove_dir_all(&wt);
}

/// core#469: `.wicked/checks.json` speaks over the manifests — a targeted test command is
/// preferred at the creator stage (and at verify unless `full: true`), `{files}` / `{base}`
/// are substituted from the run's base, `timeout_s` rides every non-install check, `false`
/// disables a check, and a malformed file fails detection CLOSED.
#[test]
fn per_repo_config_prefers_targeted_tests_and_fails_closed_on_a_bad_file() {
    let wt = scratch("config");
    std::fs::write(
        wt.join("package.json"),
        r#"{"scripts":{"typecheck":"tsc --noEmit","lint":"eslint .","test":"vitest run"}}"#,
    )
    .unwrap();
    std::fs::write(wt.join(".gitignore"), "node_modules/\ntmp/\n").unwrap();
    std::fs::create_dir_all(wt.join("node_modules")).unwrap();
    std::fs::create_dir_all(wt.join(".wicked")).unwrap();
    std::fs::write(
            wt.join(CONFIG_PATH),
            r#"{"lint":false,"test_targeted":["npx","vitest","run","--changed","{base}","{files}"],"timeout_s":90}"#,
        )
        .unwrap();
    let base = git_repo_with_commit(&wt);
    // No base known ⇒ an anchored targeted command cannot run: the full set stands.
    let full = detect(&wt).unwrap();
    assert_eq!(
        names(&full),
        vec!["typecheck", "test"],
        "lint disabled by config"
    );
    assert!(
        full.iter().all(|c| c.timeout_s == Some(90)),
        "timeout_s rides every check: {full:?}"
    );
    // The creator, with a known base ⇒ targeted, `{base}` and `{files}` substituted (the
    // untracked file the change added is a touched path; the engine scratch never is).
    std::fs::write(wt.join("src.ts"), "changed\n").unwrap();
    std::fs::create_dir_all(wt.join("tmp/worker")).unwrap();
    std::fs::write(wt.join("tmp/worker/x"), "scratch").unwrap();
    let ctx = FloorContext {
        stage: FloorStage::Creator,
        base_head: Some(base.clone()),
        git_dir: Some(wt.join(".git")),
        ..FloorContext::default()
    };
    let creator = detect_with(&wt, &ctx).unwrap();
    assert_eq!(names(&creator), vec!["typecheck", "test_targeted"]);
    let targeted = &creator[1];
    assert_eq!(
        targeted.argv,
        vec!["npx", "vitest", "run", "--changed", base.as_str(), "src.ts"]
    );
    assert!(
        targeted.source.contains("test_targeted"),
        "{}",
        targeted.source
    );
    assert_eq!(targeted.timeout_s, Some(90));
    // Verify prefers the targeted set too — until the repo says `full: true`.
    let verify_ctx = FloorContext {
        stage: FloorStage::Verify,
        ..ctx.clone()
    };
    assert_eq!(
        names(&detect_with(&wt, &verify_ctx).unwrap()),
        vec!["typecheck", "test_targeted"]
    );
    std::fs::write(
            wt.join(CONFIG_PATH),
            r#"{"lint":false,"test_targeted":"npx vitest run --changed {base}","timeout_s":90,"full":true}"#,
        )
        .unwrap();
    let verify_full = detect_with(&wt, &verify_ctx).unwrap();
    assert_eq!(names(&verify_full), vec!["typecheck", "test"]);
    assert_eq!(verify_full[1].argv, s(&["npm", "run", "test"]));
    let creator_again = detect_with(&wt, &ctx).unwrap();
    assert_eq!(
        names(&creator_again),
        vec!["typecheck", "test_targeted"],
        "`full` speaks at verify only"
    );
    assert_eq!(
        creator_again[1].argv,
        vec!["npx", "vitest", "run", "--changed", base.as_str()],
        "a string command is whitespace-split, no shell"
    );
    // A configured `test` replaces the auto-detected one; a configured typecheck is added
    // even when package.json names none.
    std::fs::write(
        wt.join("package.json"),
        r#"{"scripts":{"test":"vitest run"}}"#,
    )
    .unwrap();
    std::fs::write(
        wt.join(CONFIG_PATH),
        r#"{"typecheck":["npx","tsc","-p","."],"test":"npm run test:ci"}"#,
    )
    .unwrap();
    let cfgd = detect(&wt).unwrap();
    assert_eq!(names(&cfgd), vec!["typecheck", "test"]);
    assert_eq!(cfgd[0].argv, s(&["npx", "tsc", "-p", "."]));
    assert_eq!(cfgd[1].argv, s(&["npm", "run", "test:ci"]));
    assert!(cfgd.iter().all(|c| c.timeout_s.is_none()));
    // Fail-closed: an unknown key, a bad value, an out-of-range bound.
    for (bad, why) in [
        (r#"{"tests":"x"}"#, "unknown field"),
        (r#"{"test":true}"#, "not a command"),
        (r#"{"test":[]}"#, "empty argv"),
        (r#"{"timeout_s":0}"#, "timeout_s"),
        ("not json", "not valid"),
    ] {
        std::fs::write(wt.join(CONFIG_PATH), bad).unwrap();
        let err = detect(&wt).expect_err(bad);
        assert!(err.contains(why), "{bad}: {err}");
    }
}

/// The host-load factor is bounded on both sides and never governs the install step.
#[test]
fn the_load_factor_is_bounded() {
    assert_eq!(load_factor(None, 8), 1.0, "unknown load ⇒ the base bound");
    assert_eq!(
        load_factor(Some(0.5), 8),
        1.0,
        "an idle host keeps the base bound"
    );
    assert_eq!(load_factor(Some(16.0), 8), 2.0);
    assert_eq!(load_factor(Some(1000.0), 8), LOAD_FACTOR_CAP);
    assert_eq!(load_factor(Some(f64::NAN), 8), 1.0);
    assert_eq!(load_factor(Some(10.0), 0), 1.0);
    let check = RepoCheck {
        name: "test".into(),
        argv: s(&["true"]),
        source: "fixture".into(),
        timeout_s: Some(100),
        dir: String::new(),
    };
    let (bound, note) = effective_bound(&check);
    assert!(
        bound >= Duration::from_secs(100) && bound <= Duration::from_secs(300),
        "{bound:?} ({note:?})"
    );
    let install = RepoCheck {
        name: "install".into(),
        argv: s(&["true"]),
        source: "fixture".into(),
        timeout_s: Some(100),
        dir: String::new(),
    };
    assert_eq!(
        install.base_timeout(),
        INSTALL_TIMEOUT,
        "timeout_s never governs the install step"
    );
    let default = RepoCheck {
        name: "lint".into(),
        argv: s(&["true"]),
        source: "fixture".into(),
        timeout_s: None,
        dir: String::new(),
    };
    assert_eq!(default.base_timeout(), CHECK_TIMEOUT);
}

/// The streaming failure-identifier scanner knows the runners the floor meets, and drops the
/// line/column positions so an unchanged failure keeps its identity across a shifted file.
#[test]
fn failure_identifiers_are_scanned_off_runner_output() {
    let mut f = None;
    let id = |line: &str, f: &mut Option<String>| failure_id_of_line(line, f);
    assert_eq!(
        id("test actor::tests::a_run ... FAILED", &mut f).as_deref(),
        Some("test actor::tests::a_run")
    );
    assert_eq!(id("test actor::tests::ok ... ok", &mut f), None);
    assert_eq!(
        id("test result: FAILED. 908 passed; 27 failed", &mut f),
        None
    );
    assert_eq!(
        id(" FAIL  tests/x.test.ts > suite > name", &mut f).as_deref(),
        Some("FAIL tests/x.test.ts > suite > name")
    );
    assert_eq!(
        id("● suite › name", &mut f).as_deref(),
        Some("● suite › name")
    );
    assert_eq!(
        id("src/a.ts(1203,20): error TS2375: Argument of type", &mut f).as_deref(),
        Some("src/a.ts: error TS2375: Argument of type")
    );
    assert_eq!(
        id("FAILED tests/x.py::test_y - AssertionError", &mut f).as_deref(),
        Some("FAILED tests/x.py::test_y")
    );
    assert_eq!(
        id("--- FAIL: TestX (0.00s)", &mut f).as_deref(),
        Some("FAIL TestX")
    );
    // eslint (stylish): the file header sets the context, the problem lines carry it.
    assert_eq!(id("/w/src/components/CenterDashboard.tsx", &mut f), None);
    assert_eq!(f.as_deref(), Some("/w/src/components/CenterDashboard.tsx"));
    assert_eq!(
        id(
            "  1203:7  error  'NO_UNITS' is assigned a value but never used  no-unused-vars",
            &mut f
        )
        .as_deref(),
        Some(
            "/w/src/components/CenterDashboard.tsx: error 'NO_UNITS' is assigned a value \
                 but never used no-unused-vars"
        )
    );
    assert_eq!(id("✖ 1 problem (1 error, 0 warnings)", &mut f), None);
    assert_eq!(id("", &mut f), None);
    // node:test SPEC reporter (Node ≥ 23) — `✖ name (Nms)` with a timing suffix is a
    // failure; the `✖ failing tests:` summary and eslint `✖ N problems` lines are not (#538).
    assert_eq!(
        id("✖ should pass the assertion (3ms)", &mut f).as_deref(),
        Some("✖ should pass the assertion"),
        "SPEC failure line with ms timing"
    );
    assert_eq!(
        id("  ✖ nested > suite > test name (1.234s)", &mut f).as_deref(),
        Some("✖ nested > suite > test name"),
        "indented SPEC line (nested suite) with s timing"
    );
    assert_eq!(
        id("✖ failing tests:", &mut f),
        None,
        "✖ failing tests: summary has no timing suffix"
    );
    // node:test TAP output (#538): `not ok N - name` is a failure; `# Subtest:` is not.
    assert_eq!(
        id("not ok 1 - passes the assertion", &mut f).as_deref(),
        Some("not ok passes the assertion")
    );
    assert_eq!(
        id("not ok 42 - deeply/nested > suite > test name", &mut f).as_deref(),
        Some("not ok deeply/nested > suite > test name")
    );
    assert_eq!(
        id("not ok 1", &mut f).as_deref(),
        Some("not ok 1"),
        "no separator: the whole rest is the id"
    );
    assert_eq!(
        id("# Subtest: some suite", &mut f),
        None,
        "# Subtest is not failure-only"
    );
    assert_eq!(
        id("ok 1 - passes", &mut f),
        None,
        "ok lines are not failures"
    );
    assert_eq!(id("TAP version 14", &mut f), None);
}

/// core#467: the claim scanner is conservative — a claim phrase AND a check-shaped word in
/// one sentence; a bug described as "pre-existing" in a fix summary is not a claim.
#[test]
fn a_pre_existing_claim_is_detected_conservatively() {
    assert_eq!(
        detect_claim("Note: the typecheck error in CenterDashboard.tsx is pre-existing on main.")
            .as_deref(),
        Some("pre-existing")
    );
    assert_eq!(
        detect_claim("All green.\nThe lint failure was already failing before my change")
            .as_deref(),
        Some("already failing")
    );
    assert_eq!(
        detect_claim("Fixed a pre-existing bug in the routing table"),
        None,
        "no check-shaped word in the sentence"
    );
    assert_eq!(detect_claim("typecheck 0, lint 0, tests 3226 passed"), None);
    assert_eq!(
        detect_claim("src/app.ts is fine. The tsc error in a.b.ts was already broken"),
        Some("already broken".to_string()),
        "a dotted file name is not a sentence boundary"
    );
}

/// (F-RC2-009) BASELINE-DIFF on the medium every `cargo test` host has. A base whose `shared`
/// and `other` tests fail: (1) a head that ALSO breaks `stable` is a REGRESSION — denied,
/// naming the head-only failure and the shared ones, and the creator's "pre-existing" claim is
/// REJECTED; (2) a head that fails exactly what the base fails is a `floor_env_mismatch` —
/// recorded, never denying, the floor PASSES and the base run is paid for once (cached);
/// (3) a head that fixes `other` and breaks nothing is `pre_existing_in_sandbox` — passes;
/// (4) `baseline_diff: false` restores the plain denial, with the opt-out on the record.
/// core#551 (review): a configured Node formatter is a Node check like any other, so a
/// formatter-only `checks.json` still schedules `install` first — it must not run `npx`/`npm`
/// against an unprovisioned tree.
#[test]
fn a_formatter_only_node_config_schedules_install_first() {
    let wt = scratch("fmt-node-install");
    std::fs::write(
        wt.join("package.json"),
        "{\"name\":\"x\",\"version\":\"0.0.0\"}\n",
    )
    .unwrap();
    std::fs::create_dir_all(wt.join(".wicked")).unwrap();
    std::fs::write(
        wt.join(CONFIG_PATH),
        "{\"formatter\": [\"npm\", \"run\", \"format:check\"]}\n",
    )
    .unwrap();
    let names: Vec<String> = detect(&wt).unwrap().into_iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        vec!["install".to_string(), "formatter".to_string()],
        "{names:?}"
    );
}

/// core#551 (review): on a Node repo with a `test` script AND a configured formatter, the
/// formatter runs before the tests — a failing test must not skip the "format first" denial.
#[test]
fn a_configured_node_formatter_runs_before_the_test_script() {
    let wt = scratch("fmt-node-order");
    std::fs::write(
        wt.join("package.json"),
        "{\"name\":\"x\",\"version\":\"0.0.0\",\"scripts\":{\"test\":\"vitest run\"}}\n",
    )
    .unwrap();
    std::fs::create_dir_all(wt.join(".wicked")).unwrap();
    std::fs::write(
        wt.join(CONFIG_PATH),
        "{\"formatter\": [\"npm\", \"run\", \"format:check\"]}\n",
    )
    .unwrap();
    let names: Vec<String> = detect(&wt).unwrap().into_iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        vec![
            "install".to_string(),
            "formatter".to_string(),
            "test".to_string()
        ],
        "{names:?}"
    );
}

/// core#551 (review): a formatting diff is deterministic, never environmental, so a formatter
/// failure the base ALSO has is still a denial — the baseline diff must not excuse it the way
/// it excuses a pre-existing test failure.
#[test]
fn a_formatter_failure_the_base_shares_still_denies() {
    let repo = scratch("fmt-basediff");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"fmt_basediff_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\npath = \"src/lib.rs\"\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(repo.join(".gitignore"), "Cargo.lock\ntmp/\n").unwrap();
    // Unformatted on the BASE and unchanged on the head: the base fails fmt identically.
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn add(a: i32,b: i32)->i32{a+b}\n",
    )
    .unwrap();
    let base = git_repo_with_commit(&repo);
    let ctx = FloorContext {
        stage: FloorStage::Creator,
        base_head: Some(base),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    let report = run_floor(&repo, &ctx);
    if report.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the floor failed closed instead of running");
        return;
    }
    assert!(
        !report.passed,
        "a shared formatting failure must still deny: {report:?}"
    );
    let fmt = &report.checks[0];
    assert_eq!(fmt.name, "cargo-fmt-check");
    assert!(fmt.exit_code.is_some_and(|e| e != 0), "{fmt:?}");
    assert!(
        fmt.base.is_none(),
        "the formatter is never baseline-compared: {:?}",
        fmt.base
    );
    assert!(fmt.denies(), "{fmt:?}");
    let denial = report.denial_reason();
    assert!(denial.contains("cargo-fmt-check"), "{denial}");
}

/// core#482 / F-3R2-023 (DES-L2 2D): `e2e` in `.wicked/checks.json` runs at the VERIFY stage
/// only, after the test set; never at the creator; `false` and absence mean no `e2e` check;
/// `timeout_s` rides it; and a base that lacks the key fails the baseline diff CLOSED ("the
/// change introduced it") — a declared e2e suite is never excused by a base that had none.
#[cfg(unix)]
#[test]
fn e2e_runs_at_verify_only_after_the_test_set_and_a_base_lacking_it_fails_closed() {
    let repo = scratch("e2e");
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    std::fs::write(repo.join(".gitignore"), "tmp/\n").unwrap();
    // The base: a `test` only, no `e2e`.
    std::fs::write(
        repo.join(CONFIG_PATH),
        r#"{"test":["true"],"timeout_s":45}"#,
    )
    .unwrap();
    let base = git_repo_with_commit(&repo);
    // The change declares an e2e suite that fails.
    std::fs::write(
        repo.join(CONFIG_PATH),
        r#"{"test":["true"],"e2e":["sh","-c","echo e2e-red >&2; exit 1"],"timeout_s":45}"#,
    )
    .unwrap();
    let creator = detect_with(
        &repo,
        &FloorContext {
            stage: FloorStage::Creator,
            ..FloorContext::default()
        },
    )
    .unwrap();
    assert_eq!(
        names(&creator),
        vec!["test"],
        "never at the creator: {creator:?}"
    );
    let verify = detect_with(&repo, &FloorContext::default()).unwrap();
    assert_eq!(
        names(&verify),
        vec!["test", "e2e"],
        "after the test set: {verify:?}"
    );
    assert_eq!(verify[1].argv, s(&["sh", "-c", "echo e2e-red >&2; exit 1"]));
    assert_eq!(verify[1].source, format!("{CONFIG_PATH} e2e"));
    assert_eq!(verify[1].timeout_s, Some(45), "timeout_s rides e2e too");
    // `false` ⇒ no e2e; unknown keys still refused.
    std::fs::write(repo.join(CONFIG_PATH), r#"{"test":["true"],"e2e":false}"#).unwrap();
    assert_eq!(
        names(&detect_with(&repo, &FloorContext::default()).unwrap()),
        vec!["test"]
    );
    let bad = detect_with_bad_key(&repo);
    assert!(bad.contains("unknown field"), "{bad}");
    // The floor at verify with the base known: `e2e` fails, the base declares none ⇒ the
    // baseline diff cannot excuse it (fail-closed), the check denies with the base's error.
    std::fs::write(
        repo.join(CONFIG_PATH),
        r#"{"test":["true"],"e2e":["sh","-c","echo e2e-red >&2; exit 1"],"timeout_s":45}"#,
    )
    .unwrap();
    let ctx = FloorContext {
        stage: FloorStage::Verify,
        base_head: Some(base),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    let report = run_floor(&repo, &ctx);
    if report.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the e2e floor cannot run");
        return;
    }
    assert!(!report.passed, "{report:?}");
    assert_eq!(names_run(&report), vec!["test", "e2e"]);
    let e2e = &report.checks[1];
    assert_eq!(e2e.outcome(), "failed");
    assert!(e2e.denies(), "{e2e:?}");
    assert!(
        e2e.classification.is_none(),
        "no base verdict ⇒ no classification: {e2e:?}"
    );
    let base_err = e2e
        .base
        .as_ref()
        .and_then(|b| b.error.clone())
        .unwrap_or_default();
    assert!(
        base_err.contains("declares no `e2e` check") && base_err.contains("introduced it"),
        "{base_err}"
    );
}

/// core#817: a change confined to a NEW subdirectory package runs that package's checks, in
/// its directory — not the unrelated root package's; a change touching both runs both; a
/// workspace root that covers its members keeps the root's checks; a package with no checks
/// is named on the note.
#[cfg(unix)]
#[test]
fn a_change_in_a_subdirectory_package_runs_that_packages_checks_817() {
    let repo = scratch("subdir-pkg");
    std::fs::write(repo.join(".gitignore"), "tmp/\nnode_modules/\n").unwrap();
    std::fs::write(
        repo.join("package.json"),
        r#"{"name":"petstore","scripts":{"test":"vitest run"}}"#,
    )
    .unwrap();
    std::fs::write(repo.join("index.ts"), "export {}\n").unwrap();
    let base = git_repo_with_commit(&repo);
    std::fs::create_dir_all(repo.join("giphy/src")).unwrap();
    std::fs::write(
        repo.join("giphy/package.json"),
        r#"{"name":"giphy","scripts":{"test":"vitest run","typecheck":"tsc --noEmit"}}"#,
    )
    .unwrap();
    std::fs::write(repo.join("giphy/src/server.ts"), "export {}\n").unwrap();
    let ctx = FloorContext {
        stage: FloorStage::Verify,
        base_head: Some(base.clone()),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    assert_eq!(
        subdir_packages(&repo, &ctx).unwrap(),
        ChangePackages {
            dirs: vec!["giphy".into()],
            root_touched: false
        }
    );
    let checks = detect_with(&repo, &ctx).unwrap();
    assert!(!checks.is_empty(), "{checks:?}");
    assert!(
        checks
            .iter()
            .all(|c| c.dir == "giphy" && c.source.starts_with("giphy/")),
        "only giphy's checks, in giphy/: {checks:?}"
    );
    assert!(checks.iter().any(|c| c.name == "typecheck"), "{checks:?}");
    // A root-owned path too: both packages' checks.
    std::fs::write(repo.join("index.ts"), "export const x = 1\n").unwrap();
    let checks = detect_with(&repo, &ctx).unwrap();
    assert!(checks.iter().any(|c| c.dir.is_empty()), "{checks:?}");
    assert!(checks.iter().any(|c| c.dir == "giphy"), "{checks:?}");
    // A Node workspace root covers its members: the root's checks only, as before.
    std::fs::write(
        repo.join("package.json"),
        r#"{"name":"petstore","workspaces":["giphy"],"scripts":{"test":"vitest run"}}"#,
    )
    .unwrap();
    assert!(subdir_packages(&repo, &ctx).unwrap().dirs.is_empty());
    assert!(detect_with(&repo, &ctx)
        .unwrap()
        .iter()
        .all(|c| c.dir.is_empty()));
    // ...but only the members its globs name (review: `packages/*` does not cover giphy/).
    std::fs::write(
            repo.join("package.json"),
            r#"{"name":"petstore","workspaces":["packages/*","!packages/x"],"scripts":{"test":"vitest run"}}"#,
        )
        .unwrap();
    assert_eq!(subdir_packages(&repo, &ctx).unwrap().dirs, vec!["giphy"]);
    assert!(workspace_glob_matches("packages/*", "packages/a"));
    assert!(workspace_glob_matches("apps/**", "apps/web/ui"));
    assert!(workspace_glob_matches("libs/ui-*", "libs/ui-kit"));
    assert!(!workspace_glob_matches("packages/*", "packages/a/b"));
    assert!(!workspace_glob_matches("packages/*", "giphy"));
    // No base known: the root, as before.
    assert!(subdir_packages(&repo, &FloorContext::default())
        .unwrap()
        .dirs
        .is_empty());
    // A package with no scripts is named, not silently passed.
    std::fs::write(
        repo.join("package.json"),
        r#"{"name":"petstore","scripts":{"test":"vitest run"}}"#,
    )
    .unwrap();
    std::fs::write(repo.join("giphy/package.json"), r#"{"name":"giphy"}"#).unwrap();
    let checks = detect_with(&repo, &ctx).unwrap();
    let note = package_coverage_note(&repo, &ctx, &checks).expect("the bare package is named");
    assert!(note.contains("touches package giphy/"), "{checks:?} {note}");
    // A package-local targeted command sees the change's files RELATIVE to the package
    // (review r2: the subpackage context kept no base, so `{files}` was never detected).
    std::fs::create_dir_all(repo.join("giphy/.wicked")).unwrap();
    std::fs::write(
        repo.join("giphy").join(CONFIG_PATH),
        r#"{"test_targeted":["sh","-c","echo","{files}"],"baseline_diff":false}"#,
    )
    .unwrap();
    let checks = detect_with(
        &repo,
        &FloorContext {
            stage: FloorStage::Creator,
            ..ctx.clone()
        },
    )
    .unwrap();
    let targeted = checks
        .iter()
        .find(|c| c.name == "test_targeted" && c.dir == "giphy")
        .unwrap_or_else(|| panic!("{checks:?}"));
    assert!(
        targeted.argv.iter().any(|a| a == "src/server.ts")
            && !targeted.argv.iter().any(|a| a.starts_with("giphy/")),
        "{targeted:?}"
    );
    assert!(!baseline_diff_enabled(&repo.join("giphy")) && baseline_diff_enabled(&repo));
    // The check runs IN its package directory.
    let sandbox = sandbox_for(&repo);
    if sandbox.level != SandboxLevel::Sandboxed {
        return;
    }
    let scratch_dir = CheckScratch::prepare(&repo).unwrap();
    let pwd = RepoCheck {
        name: "test".into(),
        argv: s(&["sh", "-c", "pwd"]),
        source: "fixture".into(),
        timeout_s: None,
        dir: "giphy".into(),
    };
    let r = run_one(&repo, &pwd, &sandbox, &scratch_dir, Tree::Head);
    if r.passed() {
        assert!(r.stdout_tail.trim_end().ends_with("/giphy"), "{r:?}");
    }
}

/// (F24) A red package fixture: `dir/package.json` and a `.wicked/checks.json` whose `test`
/// prints the libtest failure line for `id` and exits 101.
#[cfg_attr(not(unix), allow(dead_code))]
fn f24_red_package(repo: &Path, dir: &str, id: &str) {
    std::fs::create_dir_all(repo.join(dir).join(".wicked")).unwrap();
    std::fs::write(
        repo.join(dir).join("package.json"),
        format!(r#"{{"name":"{}"}}"#, dir.replace('/', "-")),
    )
    .unwrap();
    let script = format!("echo 'test {id} ... FAILED'; exit 101");
    std::fs::write(
        repo.join(dir).join(CONFIG_PATH),
        serde_json::json!({ "test": ["sh", "-c", script] }).to_string(),
    )
    .unwrap();
}

/// (F24) A repo with a green root (`test` = `true`) and a `.gitignore` for the engine scratch.
fn f24_repo(name: &str) -> PathBuf {
    let repo = scratch(name);
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    std::fs::write(repo.join(".gitignore"), "tmp/\n").unwrap();
    std::fs::write(repo.join(CONFIG_PATH), r#"{"test":["true"]}"#).unwrap();
    std::fs::write(repo.join("index.txt"), "root\n").unwrap();
    repo
}

#[cfg_attr(not(unix), allow(dead_code))]
fn f24_ctx(repo: &Path, base: &str) -> FloorContext {
    FloorContext {
        stage: FloorStage::Verify,
        base_head: Some(base.to_string()),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    }
}

/// The `test` check the floor ran in `dir`.
#[cfg_attr(not(unix), allow(dead_code))]
fn f24_check<'a>(report: &'a RepoChecksReport, dir: &str) -> &'a CheckRun {
    report
        .checks
        .iter()
        .find(|c| c.name == "test" && c.source.starts_with(&format!("{dir}/")))
        .unwrap_or_else(|| panic!("no `test` ran in `{dir}`: {report:#?}"))
}

#[cfg_attr(not(unix), allow(dead_code))]
fn f24_base_error(c: &CheckRun) -> String {
    c.base
        .as_ref()
        .and_then(|b| b.error.clone())
        .unwrap_or_default()
}

/// (F24, W10 57bafff0 unit 7) A failing test in a package directory the change ADDED is the
/// change's own failure, as a red test on a comparable green base is: `new_failure`, the
/// failing test named as the change's (so the card leads with Send back), never "base red,
/// cannot separate" with the operator's override as the path through.
#[cfg(unix)]
#[test]
fn a_failing_check_in_a_package_the_change_added_is_the_changes_own_f24() {
    let repo = f24_repo("f24-new-pkg");
    let base = git_repo_with_commit(&repo);
    f24_red_package(&repo, "pkg", "pkg::adds");
    let git_dir = repo.join(".git");
    assert_eq!(
        introduced_dir(&repo, &git_dir, &base, "pkg").unwrap(),
        NewDir::Introduced
    );
    // No claim on a path that is not a plain relative one, or on what the base has.
    for odd in ["", ".", "./pkg", "pkg/", "../pkg", "/pkg", "pkg//x"] {
        assert_eq!(
            introduced_dir(&repo, &git_dir, &base, odd).unwrap(),
            NewDir::No,
            "{odd:?}"
        );
    }
    assert_eq!(
        introduced_dir(&repo, &git_dir, &base, "index.txt").unwrap(),
        NewDir::No
    );
    // The head has no directory there: no claim.
    assert_eq!(
        introduced_dir(&repo, &git_dir, &base, "nowhere").unwrap(),
        NewDir::No
    );
    let report = run_floor(&repo, &f24_ctx(&repo, &base));
    if report.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the F24 floor cannot run");
        return;
    }
    assert!(!report.passed, "{report:?}");
    let c = f24_check(&report, "pkg");
    let d = c.decision.as_ref().expect("decided");
    // The change's own either way: failed, or failed twice (its identical retry failed too).
    assert!(
        matches!(
            d.outcome.as_str(),
            OUTCOME_NEW_FAILURE | OUTCOME_FLAKY_TWICE
        ),
        "{c:#?}"
    );
    assert_ne!(c.classification.as_deref(), Some(BASE_RED_UNSEPARATED));
    assert_eq!(c.regressions, s(&["test pkg::adds"]), "{c:#?}");
    let err = f24_base_error(c);
    assert!(
        err.starts_with("the change introduced `pkg`") && err.contains("no such directory"),
        "{err}"
    );
    let why = report.denial_reason();
    assert!(!why.contains("floor override is the path through"), "{why}");
    eprintln!("repo_checks: F24 new-package floor asserted");
    let _ = std::fs::remove_dir_all(&repo);
}

/// (F24) The negative control: the package is in the base and red there too (a comparable
/// red base) — the head's failure cannot be told apart, so it stays `base_red_unseparated`.
#[cfg(unix)]
#[test]
fn a_red_package_the_base_already_had_stays_unseparated_f24() {
    let repo = f24_repo("f24-old-pkg");
    f24_red_package(&repo, "pkg", "pkg::shared");
    std::fs::write(repo.join("pkg/notes.txt"), "v1\n").unwrap();
    let base = git_repo_with_commit(&repo);
    std::fs::write(repo.join("pkg/notes.txt"), "v2\n").unwrap();
    assert_eq!(
        introduced_dir(&repo, &repo.join(".git"), &base, "pkg").unwrap(),
        NewDir::No
    );
    let report = run_floor(&repo, &f24_ctx(&repo, &base));
    if report.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the F24 floor cannot run");
        return;
    }
    let c = f24_check(&report, "pkg");
    assert_eq!(
        c.classification.as_deref(),
        Some(BASE_RED_UNSEPARATED),
        "{c:#?}"
    );
    assert!(c.regressions.is_empty(), "{c:#?}");
    eprintln!("repo_checks: F24 comparable-red floor asserted");
    let _ = std::fs::remove_dir_all(&repo);
}

/// (core#1314) A package force-added (`git add -f`) under an ignored path is one the change
/// introduced: pre-fix, the head's snapshot (`add -A`) left it out and `introduced_dir` made
/// no claim.
#[test]
fn a_force_added_package_under_an_ignored_path_is_introduced_1314() {
    let repo = f24_repo("f24-forced");
    std::fs::write(repo.join(".gitignore"), "tmp/\nvendor/\n").unwrap();
    let base = git_repo_with_commit(&repo);
    f24_red_package(&repo, "vendor/pkg", "pkg::adds");
    git(&repo, &["add", "-f", "vendor/pkg"]);
    assert_eq!(
        introduced_dir(&repo, &repo.join(".git"), &base, "vendor/pkg").unwrap(),
        NewDir::Introduced
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// (core#1110) A process that swaps the base export's package directory while base
/// detection reads it, and restores it after, cannot change what the base runs: detection
/// reads the directory held before it began. Pre-fix, detection read the impostor (which
/// declares no `test`) by path, and the restored path passed the identity check after it.
/// A swap left in place fails the base closed.
#[cfg(unix)]
#[test]
fn a_base_detection_swap_and_restore_cannot_change_the_base_check_1110() {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    for restore in [true, false] {
        let repo = f24_repo(&format!("held-detect-{restore}"));
        f24_red_package(&repo, "pkg", "pkg::shared");
        std::fs::write(repo.join("pkg/notes.txt"), "v1\n").unwrap();
        let base = git_repo_with_commit(&repo);
        std::fs::write(repo.join("pkg/notes.txt"), "v2\n").unwrap();
        let parked: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
        let fired = Rc::new(Cell::new(false));
        let (p_in, p_out, f_out) = (parked.clone(), parked.clone(), fired.clone());
        test_seam::around_base_detect(
            move |pkg| {
                // Park the real package; an impostor that declares no `test` takes its path.
                let aside = pkg.with_file_name("pkg-parked");
                std::fs::rename(pkg, &aside).unwrap();
                std::fs::create_dir_all(pkg.join(".wicked")).unwrap();
                std::fs::write(pkg.join("package.json"), r#"{"name":"pkg"}"#).unwrap();
                std::fs::write(pkg.join(CONFIG_PATH), r#"{"test":false}"#).unwrap();
                *p_in.borrow_mut() = Some(aside);
            },
            move |pkg| {
                f_out.set(true);
                if restore {
                    let aside = p_out.borrow_mut().take().expect("parked");
                    std::fs::remove_dir_all(pkg).unwrap();
                    std::fs::rename(&aside, pkg).unwrap();
                }
            },
        );
        let report = run_floor(&repo, &f24_ctx(&repo, &base));
        let _ = test_seam::take_base_detect(true);
        let _ = test_seam::take_base_detect(false);
        if report.sandbox_error.is_some() {
            eprintln!("repo_checks: no sandbox tool here — the core#1110 floor cannot run");
            return;
        }
        assert!(fired.get(), "the base detection hook ran: {report:#?}");
        let c = f24_check(&report, "pkg");
        let err = f24_base_error(c);
        assert!(!err.starts_with(DECLARES_NO_PREFIX), "{err}");
        if restore {
            // The base ran its OWN red `test` — not an error, not a failed install.
            let run = c
                .base
                .as_ref()
                .filter(|b| b.error.is_none())
                .and_then(|b| b.run.as_ref())
                .unwrap_or_else(|| panic!("the base ran: {c:#?}"));
            assert_eq!(run.exit_code, Some(101), "{run:#?}");
            assert!(run.stdout_tail.contains("pkg::shared"), "{run:#?}");
            assert_eq!(
                c.classification.as_deref(),
                Some(BASE_RED_UNSEPARATED),
                "{c:#?}"
            );
        } else {
            assert!(
                err.contains("could not be determined") && err.contains("no longer the directory"),
                "a swap left in place fails the base closed: {err}"
            );
        }
        let _ = std::fs::remove_dir_all(&repo);
    }
}

/// (core#1110) Under [`held_reads`], detection reads the held directory, not whatever is at
/// its path now; a link is refused, never followed; a missing parent is absent.
#[cfg(unix)]
#[test]
fn held_reads_read_the_held_directory_not_the_path_1110() {
    let root = scratch("held-reads");
    let config = |dir: &Path, word: &str| {
        std::fs::create_dir_all(dir.join(".wicked")).unwrap();
        std::fs::write(dir.join("package.json"), r#"{"name":"pkg"}"#).unwrap();
        std::fs::write(
            dir.join(CONFIG_PATH),
            serde_json::json!({ "test": ["echo", word] }).to_string(),
        )
        .unwrap();
    };
    let path = root.join("pkg");
    config(&path, "original");
    let held = HeldDir::open(&root).unwrap();
    let pkg = held.at("pkg").unwrap();
    assert!(pkg.still_at(&path).is_ok());
    std::fs::rename(&path, root.join("parked")).unwrap();
    config(&path, "impostor");
    let test_argv = |d: Result<Vec<RepoCheck>, String>| {
        d.unwrap()
            .into_iter()
            .find(|c| c.name == "test")
            .map(|c| c.argv)
    };
    assert_eq!(test_argv(detect(&path)), Some(s(&["echo", "impostor"])));
    let read = held_reads(&path, &pkg, || detect(&path)).unwrap();
    assert_eq!(test_argv(read), Some(s(&["echo", "original"])));
    assert!(pkg.still_at(&path).is_err(), "the swap is still in place");
    // Outside the anchor, by path again.
    assert_eq!(test_argv(detect(&path)), Some(s(&["echo", "impostor"])));
    // A link in the held directory is refused; a missing parent is absent.
    std::fs::remove_file(root.join("parked/package.json")).unwrap();
    std::os::unix::fs::symlink("/etc/hosts", root.join("parked/package.json")).unwrap();
    let link = held_reads(&path, &pkg, || {
        probe(&path, "package.json").map(|p| p.is_some())
    })
    .unwrap();
    assert!(
        link.as_ref().is_err_and(|e| e.contains("symlink")),
        "{link:?}"
    );
    let missing = held_reads(&path, &pkg, || {
        probe(&path, "nowhere/package.json").map(|p| p.is_some())
    })
    .unwrap();
    assert_eq!(missing, Ok(false));
    let _ = std::fs::remove_dir_all(&root);
}

/// (core#1408) A root that declares `native_rebuild` is provisioned on the head before any
/// member's checks; the base comparison of a member's check runs the root's install and
/// native build too, in the root. A root that declares none adds nothing; a root config that
/// does not parse leaves the base unknown (`Err`), as the head's root detection fails too.
#[cfg(unix)]
#[test]
fn the_base_provisions_the_roots_native_build_for_a_member_1408() {
    for (tag, cfg) in [
        ("declares", r#"{"native_rebuild":["better-sqlite3"]}"#),
        ("none", "{}"),
        ("broken", "{"),
    ] {
        let repo = scratch(&format!("root-native-{tag}"));
        std::fs::write(repo.join(".gitignore"), "tmp/\nnode_modules/\n").unwrap();
        std::fs::write(
            repo.join("package.json"),
            r#"{"name":"root","dependencies":{"better-sqlite3":"12.0.0"}}"#,
        )
        .unwrap();
        std::fs::write(repo.join("package-lock.json"), "{}").unwrap();
        std::fs::create_dir_all(repo.join(".wicked")).unwrap();
        std::fs::write(repo.join(CONFIG_PATH), cfg).unwrap();
        std::fs::create_dir_all(repo.join("packages/a")).unwrap();
        std::fs::write(
            repo.join("packages/a/package.json"),
            r#"{"name":"a","scripts":{"test":"node --test"}}"#,
        )
        .unwrap();
        let head = git_repo_with_commit(&repo);
        let _scratch = CheckScratch::prepare(&repo).unwrap();
        let root = HeldDir::open(&repo).unwrap();
        let base = BaseTree::export(&repo, &root, &repo.join(".git"), &head).unwrap();
        let ctx = FloorContext::default();
        let steps = base.root_native("packages/a", &ctx);
        match tag {
            "declares" => {
                let steps = steps.unwrap();
                let got: Vec<(&str, &str)> = steps
                    .iter()
                    .map(|c| (c.name.as_str(), c.dir.as_str()))
                    .collect();
                assert_eq!(
                    got,
                    vec![("install", ""), (NATIVE_BUILD_CHECK, "")],
                    "{steps:?}"
                );
                assert_eq!(steps[1].argv, native_build_argv(&["better-sqlite3"]));
            }
            "none" => assert!(steps.unwrap().is_empty()),
            _ => assert!(steps.is_err(), "{steps:?}"),
        }
        // A root check is the root's own: nothing extra.
        assert!(base.root_native("", &ctx).unwrap().is_empty());
        drop(base);
        let _ = remove_base_export(&root);
        let _ = std::fs::remove_dir_all(&repo);
    }
}

/// (F24, pi review) The rename guard holds past git's rename limit: a moved package whose files
/// changed names and content is still a move with `diff.renameLimit = 1` in the repository's
/// config (git would otherwise list it as deletes + adds, exit 0).
#[test]
fn a_rename_past_the_rename_limit_is_still_a_move_f24() {
    let repo = f24_repo("f24-rename-limit");
    std::fs::create_dir_all(repo.join("old")).unwrap();
    let body = |i: usize| {
        (1..=20)
            .map(|n| format!("f{i} line {n}\n"))
            .collect::<String>()
    };
    for i in 1..=3 {
        std::fs::write(repo.join(format!("old/a{i}.txt")), body(i)).unwrap();
    }
    let base = git_repo_with_commit(&repo);
    git(&repo, &["config", "diff.renameLimit", "1"]);
    std::fs::create_dir_all(repo.join("new")).unwrap();
    for i in 1..=3 {
        std::fs::remove_file(repo.join(format!("old/a{i}.txt"))).unwrap();
        std::fs::write(
            repo.join(format!("new/b{i}.txt")),
            format!("{}extra\n", body(i)),
        )
        .unwrap();
    }
    match introduced_dir(&repo, &repo.join(".git"), &base, "new").unwrap() {
        NewDir::Moved(m) => assert!(m.iter().any(|x| x == "old/a1.txt -> new/b1.txt"), "{m:?}"),
        other => panic!("a move past the rename limit read as {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&repo);
}

/// (F24, codex review) Only the engine's own leading words exempt a base error; the same words
/// quoted inside another error (a repo path in an ENOENT) leave the base unknown.
#[test]
fn only_the_engines_leading_words_exempt_a_base_error_f24() {
    for ok in [
        "the change introduced `pkg`: the run base has no such directory",
        "the run base declares no `e2e` check (the change introduced it)",
        "baseline diff disabled by `.wicked/checks.json` (baseline_diff: false)",
    ] {
        assert!(base_error_leaves_a_new_failure(ok), "{ok}");
    }
    for laundered in [
        "the base's checks could not be determined: `the change introduced `x``: No such file",
        "the base's checks could not be determined: `a/declares no `b`/c`: No such file",
        "the base's dependency install did not pass (baseline_diff: false)",
        "the run base has no `new`, but the change moved base files into it (old/a -> new/a)",
        " the change introduced `pkg`",
    ] {
        assert!(!base_error_leaves_a_new_failure(laundered), "{laundered}");
    }
}

/// (F24, codex review) A base LINK at an ancestor (`alias -> ../shared`, dangling in the
/// export) reached a package the change then made a real directory: not one the change added.
#[cfg(unix)]
#[test]
fn a_package_behind_a_base_link_is_not_one_the_change_added_f24() {
    let repo = f24_repo("f24-link-ancestor");
    f24_red_package(&repo, "shared/pkg", "shared::pkg");
    std::os::unix::fs::symlink("shared", repo.join("alias")).unwrap();
    let base = git_repo_with_commit(&repo);
    std::fs::remove_file(repo.join("alias")).unwrap();
    f24_red_package(&repo, "alias/pkg", "shared::pkg");
    assert_eq!(
        introduced_dir(&repo, &repo.join(".git"), &base, "alias/pkg").unwrap(),
        NewDir::No
    );
    // The export walk refuses a link on the way, dangling or not.
    let export = scratch("f24-link-export");
    std::os::unix::fs::symlink("../nowhere", export.join("alias")).unwrap();
    assert!(!absent_in_export(&export, "alias/pkg"));
    assert!(absent_in_export(&export, "fresh/pkg"));
    std::fs::write(export.join("file"), "x").unwrap();
    assert!(!absent_in_export(&export, "file/pkg"));
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&export);
}

/// (F24) One change, both: a new red package and an old red one it touched. The floor stops at
/// its first failure (`fresh` runs first): that is the change's own, naming only its test —
/// the old package's base red is neither laundered into it nor hidden behind it. Once the new
/// package is green, the old one is still the base's, unseparated.
#[cfg(unix)]
#[test]
fn a_new_red_package_beside_an_old_red_one_is_told_apart_f24() {
    let repo = f24_repo("f24-mixed");
    f24_red_package(&repo, "old", "old::shared");
    std::fs::write(repo.join("old/notes.txt"), "v1\n").unwrap();
    let base = git_repo_with_commit(&repo);
    std::fs::write(repo.join("old/notes.txt"), "v2\n").unwrap();
    f24_red_package(&repo, "fresh", "fresh::adds");
    let ctx = f24_ctx(&repo, &base);
    let report = run_floor(&repo, &ctx);
    if report.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the F24 floor cannot run");
        return;
    }
    let fresh = f24_check(&report, "fresh");
    assert!(
        matches!(
            fresh.decision.as_ref().map(|d| d.outcome.as_str()),
            Some(OUTCOME_NEW_FAILURE | OUTCOME_FLAKY_TWICE)
        ),
        "{fresh:#?}"
    );
    assert_ne!(fresh.classification.as_deref(), Some(BASE_RED_UNSEPARATED));
    assert_eq!(fresh.regressions, s(&["test fresh::adds"]), "{fresh:#?}");
    assert!(f24_base_error(fresh).starts_with("the change introduced `fresh`"));
    assert!(
        !report.checks.iter().any(|c| c.source.starts_with("old/")),
        "the floor stopped at its first failure: {report:#?}"
    );
    // The new package fixed: the old package's red is the base's, as before.
    std::fs::write(repo.join("fresh").join(CONFIG_PATH), r#"{"test":["true"]}"#).unwrap();
    let report = run_floor(&repo, &ctx);
    let old = f24_check(&report, "old");
    assert_eq!(
        old.classification.as_deref(),
        Some(BASE_RED_UNSEPARATED),
        "{old:#?}"
    );
    assert!(old.regressions.is_empty(), "{old:#?}");
    assert!(f24_check(&report, "fresh").passed());
    eprintln!("repo_checks: F24 mixed floor asserted");
    let _ = std::fs::remove_dir_all(&repo);
}

/// (F24, path tricks) A RENAMED red package is not one the change added: the base's files
/// under a new name keep the comparison unknown (`base_red_unseparated`, the move named) —
/// untracked, staged (`git mv`), and with an edit git still pairs.
#[cfg(unix)]
#[test]
fn a_renamed_red_package_is_not_one_the_change_added_f24() {
    let repo = f24_repo("f24-renamed");
    f24_red_package(&repo, "old", "old::shared");
    std::fs::write(
        repo.join("old/lib.txt"),
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n",
    )
    .unwrap();
    let base = git_repo_with_commit(&repo);
    let git_dir = repo.join(".git");
    std::fs::rename(repo.join("old"), repo.join("new")).unwrap();
    let moved = |r: NewDir| match r {
        NewDir::Moved(m) => m,
        other => panic!("a rename read as {other:?}"),
    };
    let m = moved(introduced_dir(&repo, &git_dir, &base, "new").unwrap());
    assert!(m.iter().any(|x| x == "old/lib.txt -> new/lib.txt"), "{m:?}");
    let report = run_floor(&repo, &f24_ctx(&repo, &base));
    if report.sandbox_error.is_none() {
        let c = f24_check(&report, "new");
        assert_eq!(
            c.classification.as_deref(),
            Some(BASE_RED_UNSEPARATED),
            "{c:#?}"
        );
        let err = f24_base_error(c);
        assert!(
            err.starts_with("the run base has no `new`, but the change moved base files"),
            "{err}"
        );
        eprintln!("repo_checks: F24 renamed floor asserted");
    }
    // Staged, and with an edit git still pairs as a rename.
    git(&repo, &["add", "-A"]);
    assert!(!moved(introduced_dir(&repo, &git_dir, &base, "new").unwrap()).is_empty());
    std::fs::write(
        repo.join("new/lib.txt"),
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\nEIGHT\n",
    )
    .unwrap();
    assert!(!moved(introduced_dir(&repo, &git_dir, &base, "new").unwrap()).is_empty());
    let _ = std::fs::remove_dir_all(&repo);
}

/// core#432 F-3R2-023: a verify floor that ran no end-to-end check over a change touching e2e
/// files says so on the gate's note — a pass included — and stays silent once one ran.
#[cfg(unix)]
#[test]
fn an_e2e_change_the_floor_did_not_run_is_disclosed_432() {
    let repo = scratch("e2e-cover");
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    std::fs::create_dir_all(repo.join("e2e")).unwrap();
    std::fs::write(repo.join(".gitignore"), "tmp/\n").unwrap();
    std::fs::write(
        repo.join(CONFIG_PATH),
        r#"{"test":["true"],"timeout_s":45}"#,
    )
    .unwrap();
    std::fs::write(repo.join("e2e/carve.py"), "XFAIL = True\n").unwrap();
    std::fs::write(repo.join("e2e/old.py"), "pass\n").unwrap();
    let base = git_repo_with_commit(&repo);
    std::fs::write(repo.join("e2e/carve.py"), "XFAIL = False\n").unwrap();
    std::fs::remove_file(repo.join("e2e/old.py")).unwrap();
    let ctx = FloorContext {
        stage: FloorStage::Verify,
        base_head: Some(base),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    let report = run_floor(&repo, &ctx);
    if report.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return;
    }
    assert!(report.passed, "{report:?}");
    let note = report
        .classification_note()
        .expect("the gap is on the note");
    assert!(
        note.contains("touches end-to-end files e2e/carve.py, e2e/old.py")
            && note.contains("did NOT run them")
            && note.contains("no `e2e` or `journeys` check is declared"),
        "{note}"
    );
    // The creator floor never runs e2e, so it never says this.
    let creator = run_floor(
        &repo,
        &FloorContext {
            stage: FloorStage::Creator,
            ..ctx.clone()
        },
    );
    assert!(creator.coverage_note.is_none(), "{creator:?}");
    // A declared e2e that runs answers it.
    std::fs::write(
        repo.join(CONFIG_PATH),
        r#"{"test":["true"],"e2e":["true"],"timeout_s":45}"#,
    )
    .unwrap();
    let report = run_floor(&repo, &ctx);
    assert_eq!(names_run(&report), vec!["test", "e2e"]);
    assert!(report.coverage_note.is_none(), "{report:?}");
    // One that could not start did not run them.
    std::fs::write(
        repo.join(CONFIG_PATH),
        r#"{"test":["true"],"e2e":["wicked-no-such-e2e-binary"],"timeout_s":45}"#,
    )
    .unwrap();
    let report = run_floor(&repo, &ctx);
    assert!(report
        .checks
        .iter()
        .any(|c| c.name == "e2e" && c.spawn_error.is_some()));
    assert!(
        report
            .coverage_note
            .as_deref()
            .is_some_and(|n| n.contains("the declared end-to-end check could not start")),
        "{report:?}"
    );
    assert!(is_e2e_path("web/tests/checkout.e2e.ts") && is_e2e_path("Playwright/x.ts"));
    assert!(!is_e2e_path("src/app.ts") && !is_e2e_path("src/e2e_helpers/x.rs"));
}

#[cfg(unix)]
fn detect_with_bad_key(repo: &Path) -> String {
    std::fs::write(
        repo.join(CONFIG_PATH),
        r#"{"test":["true"],"e2e_suite":["x"]}"#,
    )
    .unwrap();
    detect_with(repo, &FloorContext::default()).expect_err("unknown key is refused")
}

#[cfg(unix)]
fn names_run(report: &RepoChecksReport) -> Vec<&str> {
    report.checks.iter().map(|c| c.name.as_str()).collect()
}

fn run_named(name: &str, exit_code: Option<i32>, timed_out: bool) -> CheckRun {
    CheckRun {
        failures_complete_in: None,
        name: name.into(),
        argv: vec!["sh".into()],
        source: "t".into(),
        exit_code,
        timed_out,
        spawn_error: None,
        duration_ms: 1,
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        bound_s: 1,
        bound_note: None,
        failure_ids: Vec::new(),
        flaky_ids: Vec::new(),
        scope: None,
        retry: None,
        self_retry: false,
        classification: None,
        pre_existing: Vec::new(),
        regressions: Vec::new(),
        base: None,
        env_cannot_run: Vec::new(),
        reruns: Vec::new(),
        earlier_passes: Vec::new(),
        selection: None,
        decision: None,
        logs: Vec::new(),
        load_band: None,
        reused_from: None,
    }
}

/// (power-speed-2, rig run 5a88dcb6) A verify floor that reused the creator floor's checks
/// says so on each reused row, not only at the frame's top: the row's duration and retries
/// are the creator floor's. The mark rides the run itself, so a check that ran here carries
/// none even when it shares its name with a reused one (a root `test` run fresh beside a
/// subpackage's reused `test`, xreview on #1224).
#[test]
fn each_reused_check_row_names_the_floor_it_was_reused_from() {
    let reused = |mut r: CheckRun, dir: &str| {
        r.reused_from = Some(CheckReuse {
            phase: Some("fix".into()),
            ord: 2,
            attempt: 0,
        });
        r.argv = vec!["sh".into(), dir.into()];
        r
    };
    let frame = crate::event::CoreEvent::RepoChecksEvaluated {
        session: "s".into(),
        ord: 4,
        attempt: 0,
        passed: true,
        criterion: "c".into(),
        checks: vec![
            run_named("install", Some(0), false),
            reused(run_named("typecheck", Some(0), false), "."),
            run_named("test", Some(0), false),
            reused(run_named("test", Some(0), false), "packages/a"),
        ],
        skipped: Vec::new(),
        sandbox_level: "sandboxed".into(),
        sandbox_error: None,
        detect_error: None,
        outcome: "passed".into(),
        floor: "verify".into(),
        claim: None,
        env: None,
        changed: Vec::new(),
        changed_truncated: false,
        reused_from: Some(FloorReuse {
            phase: Some("fix".into()),
            ord: 2,
            attempt: 0,
            reused: vec!["typecheck".into(), "test".into()],
            fresh: vec!["install".into(), "test".into()],
            voided: None,
        }),
        floor_runner: None,
    }
    .to_json();
    let rows = frame["checks"].as_array().unwrap();
    let from = serde_json::json!({"phase": "fix", "ord": 2, "attempt": 0});
    let marks: Vec<Option<&serde_json::Value>> = rows.iter().map(|r| r.get("reusedFrom")).collect();
    assert_eq!(
        marks,
        vec![None, Some(&from), None, Some(&from)],
        "only the reused runs are marked: {frame:#}"
    );
    // A stored run from before the field reads as not reused.
    let mut stored = serde_json::to_value(run_named("lint", Some(0), false)).unwrap();
    stored.as_object_mut().unwrap().remove("reused_from");
    let back: CheckRun = serde_json::from_value(stored).unwrap();
    assert_eq!(back.reused_from, None);
}

/// (power-speed-2) A verify floor's reused failure is the SAME result as the creator floor's
/// it came from: the fingerprint an operator's override is keyed on ignores the reuse mark,
/// so the override carries (core#1186).
#[test]
fn a_reused_failure_keeps_the_fingerprint_of_the_floor_it_came_from() {
    let report = |run: CheckRun| -> RepoChecksReport {
        serde_json::from_value(serde_json::json!({
            "detected": [], "checks": [run], "skipped": [], "passed": false,
            "sandbox_level": "sandboxed",
        }))
        .unwrap()
    };
    let failed = run_named("test", Some(1), false);
    let mut reused = failed.clone();
    reused.reused_from = Some(CheckReuse {
        phase: Some("fix".into()),
        ord: 2,
        attempt: 0,
    });
    let creator = report(failed).result_fingerprint();
    assert!(creator.is_some());
    assert_eq!(report(reused).result_fingerprint(), creator);
}

/// (deliver honors an override) A floor result's key: only the ordinary floor that RAN is keyed —
/// never one that did not run, an operator's re-run or a floor with waived checks — and the
/// digest moves with the stage, the package dir and every check's identity (a forced install
/// moves it through the `install` check it adds).
#[test]
fn a_floor_key_is_the_ordinary_floor_that_ran_and_its_exact_check_set() {
    let check = |name: &str, argv: &[&str]| RepoCheck {
        name: name.into(),
        argv: argv.iter().map(|a| a.to_string()).collect(),
        source: "test".into(),
        timeout_s: None,
        dir: String::new(),
    };
    let mut r = report_of(Vec::new(), &[]);
    r.passed = false;
    r.detected = vec![
        check("lint", &["npm", "run", "lint"]),
        check("test", &["npm", "test"]),
    ];
    let ctx = FloorContext {
        stage: FloorStage::Verify,
        base_head: Some("base".into()),
        ..Default::default()
    };
    let key = r
        .key_for(Some("tree"), &ctx)
        .expect("the ordinary floor is keyed");
    assert_eq!(key.tree, "tree");
    assert_eq!(key.base.as_deref(), Some("base"));
    assert!(r.key_for(None, &ctx).is_none(), "no tree, no key");
    assert!(r.key_for(Some(""), &ctx).is_none());
    let mut not_run = r.clone();
    not_run.sandbox_error = Some("no boundary".into());
    assert!(not_run.key_for(Some("tree"), &ctx).is_none(), "did not run");
    let mut rerun = r.clone();
    rerun.rerun = Some(FloorRerunMode::Targeted);
    assert!(
        rerun.key_for(Some("tree"), &ctx).is_none(),
        "an operator's re-run"
    );
    let mut waived = r.clone();
    waived.waived = vec!["lint".into()];
    assert!(
        waived.key_for(Some("tree"), &ctx).is_none(),
        "waived checks"
    );
    // The digest: any change to the context or the set is another key.
    let digest = |c: &FloorContext, d: &[RepoCheck]| check_set_digest(c, d);
    let base = digest(&ctx, &r.detected);
    let pkg = FloorContext {
        package_dir: Some("pkg".into()),
        ..ctx.clone()
    };
    let creator = FloorContext {
        stage: FloorStage::Creator,
        ..ctx.clone()
    };
    let reordered = vec![r.detected[1].clone(), r.detected[0].clone()];
    let mut bounded = r.detected.clone();
    bounded[1].timeout_s = Some(60);
    let mut moved = r.detected.clone();
    moved[1].dir = "pkg".into();
    for (tag, other) in [
        (
            "an added install",
            digest(
                &ctx,
                &[vec![check("install", &["npm", "ci"])], r.detected.clone()].concat(),
            ),
        ),
        ("package dir", digest(&pkg, &r.detected)),
        ("stage", digest(&creator, &r.detected)),
        ("order", digest(&ctx, &reordered)),
        ("bound", digest(&ctx, &bounded)),
        ("dir", digest(&ctx, &moved)),
        ("subset", digest(&ctx, &r.detected[..1])),
    ] {
        assert_ne!(other, base, "{tag}");
    }
    assert_eq!(digest(&ctx, &r.detected), base, "deterministic");
}

fn report_of(checks: Vec<CheckRun>, skipped: &[&str]) -> RepoChecksReport {
    let passed = !checks.iter().any(|c| c.denies());
    RepoChecksReport {
        targeted_test: None,
        detected: Vec::new(),
        checks,
        skipped: skipped.iter().map(|s| s.to_string()).collect(),
        passed,
        detect_error: None,
        sandbox_level: "sandboxed".into(),
        sandbox_note: None,
        sandbox_error: None,
        engine_writes_removed: Vec::new(),
        claim: None,
        env: None,
        tree: None,
        rerun: None,
        waived: Vec::new(),
        requested_rerun: None,
        recheck: None,
        coverage_note: None,
        key: None,
        resumed: None,
        reused_from: None,
        runner: None,
    }
}

/// core#469: `accept_partial` waives exactly what the timed-out floor did not verify — the
/// check that hit its bound and every one that never ran after it — and nothing on a floor
/// that FAILED (a failure is not "unfinished").
#[test]
fn the_partial_waiver_is_what_the_timed_out_floor_did_not_verify() {
    let timed_out = report_of(
        vec![
            run_named("lint", Some(0), false),
            run_named("test", None, true),
        ],
        &["e2e"],
    );
    assert!(timed_out.timed_out());
    assert_eq!(partial_waiver(&timed_out), vec!["test", "e2e"]);
    let failed = report_of(
        vec![
            run_named("lint", Some(0), false),
            run_named("test", Some(1), false),
        ],
        &[],
    );
    assert!(partial_waiver(&failed).is_empty());
    let passed = report_of(vec![run_named("lint", Some(0), false)], &[]);
    assert!(partial_waiver(&passed).is_empty());
}

/// core#469: what each operator re-run runs. `extend` doubles every non-install bound;
/// `targeted` swaps the full test set for the declared `test_targeted` (disclosing the
/// replaced full set) or, with none declared, is refused (core#1392: never a waiver of the
/// tests); `accept_partial` drops the waived checks and runs the rest.
#[test]
fn an_operator_rerun_selects_its_checks_and_names_what_it_waives() {
    let wt = scratch("rerun-select");
    std::fs::create_dir_all(wt.join(".wicked")).unwrap();
    std::fs::write(
        wt.join(CONFIG_PATH),
        r#"{"lint":["sh","-c","exit 0"],"test":["sh","-c","exit 0"],"full":true,"timeout_s":10}"#,
    )
    .unwrap();
    let ctx = FloorContext::default(); // verify stage
    let detected = detect_with(&wt, &ctx).unwrap();
    assert_eq!(names(&detected), vec!["lint", "test"]);
    let rerun = |mode, waive: &[&str]| FloorRerun {
        mode,
        waive: waive.iter().map(|w| w.to_string()).collect(),
        output: String::new(),
        fix: None,
    };

    let (run, waived) = apply_rerun(
        &wt,
        &ctx,
        &rerun(FloorRerunMode::Extend, &[]),
        detected.clone(),
    )
    .unwrap();
    assert!(waived.is_empty());
    assert!(
        run.iter().all(|c| c.timeout_s == Some(10 * EXTEND_FACTOR)),
        "{run:?}"
    );

    // (core#1392) No targeted command: the re-run is refused (fail-closed), never a waiver of
    // every test, and the floor names no targeted command for the gate to offer.
    let err = apply_rerun(
        &wt,
        &ctx,
        &rerun(FloorRerunMode::Targeted, &[]),
        detected.clone(),
    )
    .expect_err("no targeted command: not run");
    assert_eq!(err, NO_TARGETED_COMMAND);
    assert_eq!(targeted_selection(&wt, &ctx).unwrap(), None);

    let (run, waived) = apply_rerun(
        &wt,
        &ctx,
        &rerun(FloorRerunMode::AcceptPartial, &["test"]),
        detected.clone(),
    )
    .unwrap();
    assert_eq!(names(&run), vec!["lint"]);
    assert_eq!(waived, vec!["test"]);

    // A declared targeted command stands in for the full set, even under `full: true`.
    std::fs::write(
            wt.join(CONFIG_PATH),
            r#"{"lint":["sh","-c","exit 0"],"test":["sh","-c","exit 0"],"test_targeted":["sh","-c","exit 0"],"full":true}"#,
        )
        .unwrap();
    let detected = detect_with(&wt, &ctx).unwrap();
    assert_eq!(names(&detected), vec!["lint", "test"], "verify, full: true");
    assert_eq!(
        targeted_selection(&wt, &ctx).unwrap().map(|c| c.source),
        Some(format!("{CONFIG_PATH} test_targeted")),
        "the declared targeted command is what the gate offers"
    );
    let (run, waived) = apply_rerun(
        &wt,
        &ctx,
        &rerun(FloorRerunMode::Targeted, &[]),
        detected.clone(),
    )
    .unwrap();
    assert_eq!(names(&run), vec!["lint", "test_targeted"]);
    assert_eq!(waived, vec!["test"], "the replaced full set is disclosed");
    // (xreview on #1184) A power run at high rigor: the operator's targeted rerun still runs
    // the targeted command, never the full set waived down to nothing.
    let power = FloorContext {
        run_id: Some("power-rerun-targeted".into()),
        ..ctx.clone()
    };
    crate::power::set_full_suite("power-rerun-targeted", true);
    let got = apply_rerun(&wt, &power, &rerun(FloorRerunMode::Targeted, &[]), detected);
    crate::power::set_full_suite("power-rerun-targeted", false);
    let (run, waived) = got.unwrap();
    assert_eq!(names(&run), vec!["lint", "test_targeted"]);
    assert_eq!(waived, vec!["test"]);
    let _ = std::fs::remove_dir_all(&wt);
}

// ─── core#481 / core#546 / core#553 / core#482 ──────────────────────────────────────────────

fn failed_run(ids: &[&str], env: &[&str]) -> CheckRun {
    CheckRun {
        failures_complete_in: None,
        name: "cargo-test".to_string(),
        argv: s(&["cargo", "test", "--locked", "--workspace"]),
        source: "Cargo.toml".to_string(),
        exit_code: Some(101),
        timed_out: false,
        spawn_error: None,
        duration_ms: 10,
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        bound_s: 1200,
        bound_note: None,
        failure_ids: ids.iter().map(|x| x.to_string()).collect(),
        flaky_ids: Vec::new(),
        scope: None,
        retry: None,
        self_retry: false,
        classification: None,
        pre_existing: Vec::new(),
        regressions: Vec::new(),
        base: None,
        env_cannot_run: env.iter().map(|x| x.to_string()).collect(),
        reruns: Vec::new(),
        earlier_passes: Vec::new(),
        selection: None,
        decision: None,
        logs: Vec::new(),
        load_band: None,
        reused_from: None,
    }
}

fn base_of(run: CheckRun) -> BaseRun {
    BaseRun {
        head: "0123456789abcdef".to_string(),
        cached: false,
        run: Some(run),
        error: None,
    }
}

/// core#781: a regression read against a base CACHED from another load band says so in the
/// denial; a fresh base, or a cached one from the same band, does not.
#[test]
fn a_cached_base_from_another_load_band_is_disclosed_781() {
    let at = |band: u32, note: &str| CheckRun {
        load_band: Some(band),
        bound_note: Some(note.to_string()),
        ..failed_run(&[], &[])
    };
    let passing_base = |band: u32, cached: bool| BaseRun {
        cached,
        ..base_of(CheckRun {
            exit_code: Some(0),
            ..at(band, "1200s × 3.00 (1-min load 63.1 / 14 cpus)")
        })
    };
    let denial = |head_band: u32, base: BaseRun| {
        let mut head = CheckRun {
            failure_ids: vec!["FAIL tests/chord.test.ts > scans".to_string()],
            flaky_ids: Vec::new(),
            scope: None,
            retry: None,
            self_retry: false,
            ..at(head_band, "1200s × 3.00 (1-min load 100.9 / 14 cpus)")
        };
        head.base = Some(Box::new(base));
        head.classification = Some(REGRESSION.to_string());
        report_of(vec![head], &[]).denial_reason()
    };
    let r = denial(7, passing_base(4, true));
    assert!(
        r.contains(
            "the base result is CACHED from an earlier floor of this run, under 1200s × \
                 3.00 (1-min load 63.1 / 14 cpus); this check ran under 1200s × 3.00 (1-min load \
                 100.9 / 14 cpus) — a different load band"
        ),
        "{r}"
    );
    for (head_band, base) in [(7, passing_base(4, false)), (4, passing_base(4, true))] {
        let r = denial(head_band, base);
        assert!(!r.contains("CACHED"), "{r}");
    }
    assert_eq!(load_band(Some(100.9), 14), Some(7));
    assert_eq!(load_band(Some(13.9), 14), Some(0));
    assert_eq!(load_band(None, 14), None);
    assert_eq!(load_band(Some(f64::NAN), 14), None);
}

/// core#482: a repo with no configured test commands gets a derived change-scoped set at the
/// CREATOR floor — the crates owning the touched files, or `vitest related` over them — and
/// the full set wherever the change can reach every test (manifests, lockfiles, configs) and
/// at verify.
#[test]
fn an_unconfigured_repo_derives_its_targeted_set_from_the_touched_files_482() {
    let repo = scratch("derive482");
    for c in ["a", "b"] {
        std::fs::create_dir_all(repo.join(c).join("src")).unwrap();
        std::fs::write(
            repo.join(c).join("Cargo.toml"),
            format!("[package]\nname = \"{c}\" # the crate\nversion = \"0.0.0\"\n"),
        )
        .unwrap();
        std::fs::write(repo.join(c).join("src/lib.rs"), "\n").unwrap();
    }
    std::fs::write(
        repo.join("Cargo.toml"),
        "[workspace]\nmembers = [\"a\", \"b\"]\n",
    )
    .unwrap();
    std::fs::write(repo.join("README.md"), "x\n").unwrap();
    let base = git_repo_with_commit(&repo);
    let ctx = |stage| FloorContext {
        stage,
        base_head: Some(base.clone()),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    let test_of = |d: &[RepoCheck]| {
        d.iter()
            .find(|c| c.name == "cargo-test" || c.name == "test_targeted")
            .cloned()
            .expect("a test set")
    };
    std::fs::write(repo.join("b/src/lib.rs"), "pub fn x() {}\n").unwrap();
    let t = test_of(&detect_with(&repo, &ctx(FloorStage::Creator)).unwrap());
    assert_eq!(t.name, "test_targeted");
    assert_eq!(t.source, DERIVED_TARGETED_SOURCE);
    assert_eq!(t.argv, s(&["cargo", "test", "--no-fail-fast", "-p", "b"]));
    let t = test_of(&detect_with(&repo, &ctx(FloorStage::Verify)).unwrap());
    assert_eq!(t.name, "cargo-test", "verify keeps the full suite");
    // The workspace manifest can change every member ⇒ full.
    std::fs::write(
        repo.join("Cargo.toml"),
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    let t = test_of(&detect_with(&repo, &ctx(FloorStage::Creator)).unwrap());
    assert_eq!(t.name, "cargo-test");
    git(&repo, &["checkout", "-q", "--", "Cargo.toml"]);
    // A deleted file elsewhere (crate `a`) could break what it left ⇒ full.
    std::fs::remove_file(repo.join("a/src/lib.rs")).unwrap();
    let t = test_of(&detect_with(&repo, &ctx(FloorStage::Creator)).unwrap());
    assert_eq!(t.name, "cargo-test", "a deletion never narrows the set");
    git(
        &repo,
        &["checkout", "-q", "--", "a/src/lib.rs", "b/src/lib.rs"],
    );
    // A path no package owns beside a crate change (a shared file any crate may
    // `include!`) ⇒ full.
    std::fs::write(repo.join("README.md"), "y\n").unwrap();
    std::fs::write(repo.join("b/src/lib.rs"), "pub fn x() {}\n").unwrap();
    let t = test_of(&detect_with(&repo, &ctx(FloorStage::Creator)).unwrap());
    assert_eq!(t.name, "cargo-test");
    // A configured test command is the repo's own choice — never overridden.
    std::fs::write(repo.join("a/src/lib.rs"), "pub fn y() {}\n").unwrap();
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    std::fs::write(
        repo.join(CONFIG_PATH),
        r#"{"test":["cargo","test","--workspace"]}"#,
    )
    .unwrap();
    let t = detect_with(&repo, &ctx(FloorStage::Creator)).unwrap();
    assert!(names(&t).contains(&"test") && !names(&t).contains(&"test_targeted"));
    let _ = std::fs::remove_dir_all(&repo);

    // vitest: `vitest related` over the touched files; a touched lockfile ⇒ full.
    let repo = scratch("derive482-node");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("package.json"),
        r#"{"scripts":{"test":"vitest run","lint":"eslint ."}}"#,
    )
    .unwrap();
    std::fs::write(repo.join("package-lock.json"), "{}").unwrap();
    std::fs::write(repo.join("src/x.ts"), "export {}\n").unwrap();
    let base = git_repo_with_commit(&repo);
    let ctx = FloorContext {
        stage: FloorStage::Creator,
        base_head: Some(base),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    std::fs::write(repo.join("src/x.ts"), "export const x = 1\n").unwrap();
    let d = detect_with(&repo, &ctx).unwrap();
    let t = d
        .iter()
        .find(|c| c.name == "test_targeted")
        .expect("derived");
    assert_eq!(
        t.argv,
        s(&[
            "npx",
            "--no-install",
            "vitest",
            "related",
            "--run",
            "--passWithNoTests",
            "src/x.ts"
        ])
    );
    std::fs::create_dir_all(repo.join("tests")).unwrap();
    std::fs::write(repo.join("tests/setup.ts"), "export {}\n").unwrap();
    let d = detect_with(&repo, &ctx).unwrap();
    assert!(
        names(&d).contains(&"test") && !names(&d).contains(&"test_targeted"),
        "a config-loaded setup file reaches every test"
    );
    std::fs::remove_file(repo.join("tests/setup.ts")).unwrap();
    std::fs::write(repo.join("package-lock.json"), "{ }").unwrap();
    let d = detect_with(&repo, &ctx).unwrap();
    assert!(names(&d).contains(&"test") && !names(&d).contains(&"test_targeted"));
    let _ = std::fs::remove_dir_all(&repo);
}

/// A cached base run of a targeted command answers only the SAME argv: a rework that touches
/// another crate derives another `-p`, and must not be compared against the first one's base.
/// A change that edits a configured command (fail-fast → `--no-fail-fast`) is compared on
/// the base with the SAME command: the base's shorter failure list is not read as the head's
/// regressions.
#[test]
fn the_base_runs_the_heads_command_when_the_change_edits_it() {
    let repo = scratch("basecmd");
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    let cfg = |script: &str| serde_json::json!({ "test": ["sh", "-c", script] }).to_string();
    std::fs::write(
        repo.join(".wicked/checks.json"),
        cfg("echo 'test t::one ... FAILED'; exit 101"),
    )
    .unwrap();
    let base = git_repo_with_commit(&repo);
    std::fs::write(
        repo.join(".wicked/checks.json"),
        cfg("echo 'test t::one ... FAILED'; echo 'test t::two ... FAILED'; exit 101"),
    )
    .unwrap();
    let ctx = FloorContext {
        stage: FloorStage::Verify,
        base_head: Some(base),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    let report = run_floor(&repo, &ctx);
    if report.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the baseline diff cannot run");
        return;
    }
    let c = report
        .checks
        .iter()
        .find(|c| c.name == "test")
        .expect("test ran");
    assert!(c.regressions.is_empty(), "{c:#?}");
    let b = c
        .base
        .as_deref()
        .and_then(|b| b.run.as_ref())
        .expect("base ran");
    assert_eq!(b.argv, c.argv, "the base ran the head's command");
    // DES-FLOOR-V2: `sh` has no precise deselect — the base red escalates, never excused.
    assert_eq!(
        c.decision.as_ref().map(|d| d.outcome.as_str()),
        Some(OUTCOME_BASE_RED_UNSEPARATED),
        "{c:#?}"
    );
    assert!(!report.passed);
    let _ = std::fs::remove_dir_all(&repo);
}

/// (codex on #1066) [`HeldDir`] never follows a link: not a linked directory component, not a
/// linked leaf, not `..`; it never opens a FIFO for a blocking write; and once the ROOT it
/// holds is swapped for a link to an outside directory, its writes land in the original
/// (detached) directory — never outside — and a fresh hold of the linked path is refused.
#[cfg(unix)]
#[test]
fn a_held_dir_never_follows_a_linked_component_nor_a_swapped_root() {
    let base = scratch("held-dir");
    let outside = scratch("held-dir-outside");
    std::fs::write(outside.join("victim"), "v").unwrap();
    let root = base.join("root");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let held = HeldDir::open(&root).unwrap();
    // A linked directory component: no write, no directory, no fresh directory through it,
    // and a removal through it removes nothing.
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
    let e = held.write("link/x", "x").unwrap_err();
    assert!(e.contains("passes through a symlink"), "{e}");
    let e = held.subdir("link", false).expect_err("refused");
    assert!(e.contains("passes through a symlink"), "{e}");
    assert!(held.create_fresh_dir("link/fresh").is_err());
    assert!(held.write_bytes("sub/../link/y", b"y", false).is_err());
    assert_eq!(held.remove_file("link/victim"), Ok(false));
    // A linked leaf: never written through, never removed as a file.
    std::os::unix::fs::symlink(outside.join("victim"), root.join("leaf")).unwrap();
    let e = held.write("leaf", "x").unwrap_err();
    assert!(e.contains("passes through a symlink"), "{e}");
    assert_eq!(held.remove_file("leaf"), Ok(false));
    assert!(std::fs::symlink_metadata(root.join("leaf")).is_ok());
    assert!(
        held.symlink("leaf", b"elsewhere").is_err(),
        "never replaced"
    );
    for rel in ["../x", "/abs", "sub/../../x", "", "."] {
        assert!(held.write(rel, "x").is_err(), "{rel:?}");
    }
    // A FIFO at the path: refused at once, never waited on.
    let fifo = std::ffi::CString::new(root.join("fifo").as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    assert!(held.write("fifo", "x").is_err());
    // A plain write creates its directories; an executable is 0755; a fresh directory is
    // refused where one already exists.
    held.write_bytes("sub/new/f", b"ok", true).unwrap();
    assert_eq!(std::fs::read(root.join("sub/new/f")).unwrap(), b"ok");
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(root.join("sub/new/f"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }
    assert!(held.create_fresh_dir("sub/new").is_err());
    held.create_fresh_dir("sub/fresh")
        .unwrap()
        .write("in", "1")
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("sub/fresh/in")).unwrap(),
        "1"
    );
    assert_eq!(held.remove_file("sub/fresh/in"), Ok(true));
    // The ROOT swapped for a link to the outside after it was held.
    let moved = base.join("moved");
    std::fs::rename(&root, &moved).unwrap();
    std::os::unix::fs::symlink(&outside, &root).unwrap();
    held.write("after-swap", "x").unwrap();
    held.create_fresh_dir("tmp/export").unwrap();
    assert!(moved.join("after-swap").is_file());
    assert!(moved.join("tmp/export").is_dir());
    assert!(HeldDir::open(&root).is_err(), "a linked root is never held");
    let mut seen: Vec<String> = std::fs::read_dir(&outside)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    seen.sort();
    assert_eq!(seen, ["victim"], "nothing landed outside");
    assert_eq!(
        std::fs::read_to_string(outside.join("victim")).unwrap(),
        "v"
    );
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&outside);
}

/// (confirmation round on d32b2b2, codex P1) The base cache record is written by the engine,
/// outside the sandbox: whatever a check's `name` or `dir` say — a traversal included — its
/// path is ONE file directly in the run's log store (outside every sandbox write root), and
/// distinct (dir, name) pairs never share one.
#[test]
fn a_traversal_check_name_never_writes_outside_the_base_cache() {
    let wt = scratch("cache-name");
    let scratch_dir = CheckScratch::prepare(&wt).expect("scratch");
    let cache = scratch_dir.logs.clone();
    assert!(
        !cache.starts_with(&scratch_dir.root),
        "outside the checks' scratch"
    );
    let head = "0123456789abcdef0123";
    let at = |dir: &str, name: &str| {
        BaseTree::cache_path(
            &scratch_dir,
            head,
            &RepoCheck {
                name: name.into(),
                argv: Vec::new(),
                source: String::new(),
                timeout_s: None,
                dir: dir.into(),
            },
        )
    };
    let mut seen = Vec::new();
    for (dir, name) in [
        ("", "../../../escaped"),
        ("", "/tmp/escaped"),
        ("", "a/b"),
        ("a", "b"),
        ("a/b", ""),
        ("../..", ".."),
        ("", &"x".repeat(400)),
    ] {
        let path = at(dir, name);
        assert_eq!(path.parent(), Some(cache.as_path()), "{dir:?} {name:?}");
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            file.len() == 5 + 64 + 5
                && file.starts_with("base-")
                && file.ends_with(".json")
                && file[5..69].bytes().all(|b| b.is_ascii_hexdigit()),
            "{file}"
        );
        assert!(!seen.contains(&path), "{dir:?} {name:?} shares an entry");
        seen.push(path.clone());
        // The record lands where the floor writes it: inside the cache, nowhere else.
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(&path, "{}").unwrap();
    }
    let records = std::fs::read_dir(&cache)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("base-"))
        .count();
    assert_eq!(records, seen.len());
    assert!(!scratch_dir.root.join("escaped").exists());
    assert!(!wt.join("escaped").exists());
    let _ = std::fs::remove_dir_all(&wt);
}

/// core#1217: a failing set's base run is kept per set on a base, not only the latest set's:
/// a later floor whose failures match an earlier set reads that set's base back instead of
/// running the base again. The whole check's record is keyed as before.
#[test]
fn core_1217_each_failing_sets_base_run_is_kept_per_set() {
    let wt = scratch("cache-per-set");
    let scratch_dir = CheckScratch::prepare(&wt).expect("scratch");
    let head = "0123456789abcdef0123";
    let set = |ids: &[&str]| {
        let mut argv = s(&["cargo", "test", "--workspace", "--", "--exact"]);
        argv.extend(s(ids));
        RepoCheck {
            name: "cargo-test".into(),
            argv,
            source: "Cargo.toml".into(),
            timeout_s: None,
            dir: String::new(),
        }
    };
    let (a, b) = (set(&["t::a"]), set(&["t::b"]));
    assert_ne!(
        BaseTree::cache_path_of(&scratch_dir, head, &a, true),
        BaseTree::cache_path_of(&scratch_dir, head, &b, true),
        "one record per set"
    );
    assert_eq!(
        BaseTree::cache_path_of(&scratch_dir, head, &a, false),
        BaseTree::cache_path_of(&scratch_dir, head, &b, false),
        "the whole check's record is keyed by the check, not its argv"
    );
    for check in [&a, &b] {
        let mut ran = failed_run(&[check.argv.last().unwrap().as_str()], &[]);
        ran.argv = check.argv.clone();
        let rec = BaseRecord {
            floor_policy: FLOOR_POLICY_VERSION,
            head_argv: check.argv.clone(),
            guide: None,
            run: ran,
            toc: None,
            posture: scratch_dir.posture(),
        };
        let path = BaseTree::cache_path_of(&scratch_dir, head, check, true);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string(&rec).unwrap()).unwrap();
    }
    // Set `b` was cached after `a`: `a`'s base run is still read back, and each is its own.
    for check in [&a, &b] {
        let side = BaseTree::cached_of(&scratch_dir, head, check, None, true)
            .unwrap_or_else(|| panic!("{:?} is cached", check.argv));
        assert!(side.base.cached);
        assert_eq!(side.base.run.unwrap().argv, check.argv);
    }
    assert!(BaseTree::cached_of(&scratch_dir, head, &set(&["t::c"]), None, true).is_none());
    let _ = std::fs::remove_dir_all(&wt);
}

#[test]
fn a_cached_targeted_base_run_answers_only_its_own_argv() {
    let wt = scratch("cache-argv");
    let scratch_dir = CheckScratch::prepare(&wt).expect("scratch");
    let head = "0123456789abcdef0123";
    let mut ran = failed_run(&["test t::x [-p b --lib]"], &[]);
    ran.name = "test_targeted".into();
    ran.argv = s(&["cargo", "test", "-p", "b"]);
    let path = BaseTree::cache_path(
        &scratch_dir,
        head,
        &RepoCheck {
            name: "test_targeted".into(),
            argv: Vec::new(),
            source: String::new(),
            timeout_s: None,
            dir: String::new(),
        },
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // A record from before the policy version (a bare check run) is a miss.
    std::fs::write(&path, serde_json::to_string(&ran).unwrap()).unwrap();
    let bare = RepoCheck {
        name: "test_targeted".into(),
        argv: s(&["cargo", "test", "-p", "b"]),
        source: DERIVED_TARGETED_SOURCE.into(),
        timeout_s: None,
        dir: String::new(),
    };
    assert!(BaseTree::cached(&scratch_dir, head, &bare, None).is_none());
    let rec = BaseRecord {
        floor_policy: FLOOR_POLICY_VERSION,
        head_argv: s(&["cargo", "test", "-p", "b"]),
        guide: None,
        run: ran,
        toc: None,
        posture: String::new(),
    };
    std::fs::write(&path, serde_json::to_string(&rec).unwrap()).unwrap();
    // (review round 15, item 17) A record taken without a guide is no answer under one.
    let g: crate::floor_guide::GuideCheck = serde_json::from_value(serde_json::json!({
            "name": "test", "run": ["cargo", "test", "-p", "b"]}))
    .unwrap();
    let b = RepoCheck {
        name: "test_targeted".into(),
        argv: s(&["cargo", "test", "-p", "b"]),
        source: DERIVED_TARGETED_SOURCE.into(),
        timeout_s: None,
        dir: String::new(),
    };
    assert!(BaseTree::cached(&scratch_dir, head, &b, Some(&g)).is_none());
    let check = |argv: &[&str]| RepoCheck {
        name: "test_targeted".into(),
        argv: s(argv),
        source: DERIVED_TARGETED_SOURCE.into(),
        timeout_s: None,
        dir: String::new(),
    };
    assert!(BaseTree::cached(
        &scratch_dir,
        head,
        &check(&["cargo", "test", "-p", "b"]),
        None
    )
    .is_some());
    assert!(BaseTree::cached(
        &scratch_dir,
        head,
        &check(&["cargo", "test", "-p", "a"]),
        None
    )
    .is_none());
    // (codex r12) A record whose blob is gone is no evidence: a miss, so the base re-runs.
    let mut rec: BaseRecord =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    rec.run.logs = vec![crate::floor_log::LogRef {
        attempt: "base".into(),
        stream: "stdout".into(),
        sha256: "0".repeat(64),
        bytes: 1,
        truncated: false,
    }];
    std::fs::write(&path, serde_json::to_string(&rec).unwrap()).unwrap();
    assert!(BaseTree::cached(
        &scratch_dir,
        head,
        &check(&["cargo", "test", "-p", "b"]),
        None
    )
    .is_none());
    drop(scratch_dir);
    let _ = std::fs::remove_dir_all(&wt);
}
/// core#767: `journeys` are VERIFY-stage checks named `journey-<name>`, in name order after
/// `e2e`; a bad name fails detection; `false` drops one; a journey is a test-set check.
#[test]
fn journeys_are_verify_stage_checks_in_name_order_767() {
    let repo = scratch("journeys767");
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    std::fs::write(
            repo.join(CONFIG_PATH),
            r#"{"test":["true"],"e2e":["true"],"journeys":{"desk_projects":["python3","e2e/desk_projects_test.py"],"cmdk":["python3","e2e/cmdk.py"],"off":false},"timeout_s":60}"#,
        )
        .unwrap();
    let verify = detect_with(&repo, &FloorContext::default()).unwrap();
    assert_eq!(
        names(&verify),
        vec!["test", "e2e", "journey-cmdk", "journey-desk_projects"]
    );
    assert_eq!(verify[3].argv, s(&["python3", "e2e/desk_projects_test.py"]));
    assert_eq!(verify[3].timeout_s, Some(60));
    assert!(is_test_set_check("journey-cmdk"));
    let creator = detect_with(
        &repo,
        &FloorContext {
            stage: FloorStage::Creator,
            ..FloorContext::default()
        },
    )
    .unwrap();
    assert_eq!(names(&creator), vec!["test"]);
    std::fs::write(
        repo.join(CONFIG_PATH),
        r#"{"test":["true"],"journeys":{"bad name":["true"]}}"#,
    )
    .unwrap();
    let err = detect_with(&repo, &FloorContext::default()).unwrap_err();
    assert!(err.contains("not a journey name"), "{err}");
    let _ = std::fs::remove_dir_all(&repo);
}
