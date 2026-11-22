//! (core#1418) The fold's slow reads, run off the actor before the fold.
//!
//! A unit's fold (`pipeline::apply_and_finish_unit`) re-verified the unit's PINNED VALIDATOR on the
//! actor thread: a subprocess waited for up to `VALIDATOR_TIMEOUT` (120 s), and its denial text
//! reopened the repository's code graph. A creator floor's `git diff-tree` ran there too. Every
//! run's reads, gate answers and step results waited behind them.
//!
//! Now, when a step result arrives whose fold would run either, the actor decides the inputs the
//! fold would hand them (from the store, as the fold does), runs them on a thread, and posts the
//! same result back with the reads in its evidence ([`UnitEvidence::pinned_validator`],
//! [`UnitEvidence::creator_paths`]). The fold applies a read only when its own inputs equal the
//! read's; otherwise it runs the validator or the diff itself, as before. Every guard of the fold
//! (a run that ended, a superseded attempt, a moved cursor) runs on the result that comes back, so
//! a read that took long is applied only to the run it was read for. A result is handed off at most
//! once: one that carries a read is folded as it is.
//!
//! The hand-off breaks the actor's first-in, first-out order for that one result: a cancel and a
//! relaunch of the same run id can be processed while its reads run, and the relaunch resets the
//! cursor and attempt the fold's guards compare. So each hand-off is registered on the actor
//! thread (`HANDED`) under its run, unit and attempt; a relaunch forgets the run's hand-offs
//! ([`forget_run`]), and a result that comes back from a forgotten hand-off is dropped
//! ([`returned`]). (A cancel needs no forget of its own: a result that comes back to a cancelled
//! run meets the fold's terminal guard, and only a relaunch can make the run live again.) A second
//! copy of a result whose hand-off is still out (a duplicate delivery) is dropped too, so two
//! validators never run in one worktree for one attempt. Reads only count
//! when this actor handed them off: `UnitEvidence::fold_handoff` never crosses the wire, and reads
//! that arrive without it are stripped, so the fold runs them itself.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use wicked_apps_core::GraphStore;

use crate::command::Command;
use crate::domain::{SessionStatus, UnitStatus, WorkUnit};
use crate::workflow::{
    CreatorPathsRead, PinnedValidatorRead, StepOutput, StepStatus, UnitEvidence,
};
use crate::worktree_guard::WorktreeSnapshot;

thread_local! {
    /// The hand-offs this actor has out, by id, with the run, unit and attempt of each.
    /// Thread-local: the actor's own (every launch runs on the actor thread), never another
    /// actor's in the process.
    static HANDED: RefCell<HashMap<u64, Handed>> = RefCell::new(HashMap::new());
}

/// What a hand-off was for: `(run id, unit index, attempt)`.
type Handed = (String, usize, u32);

fn handed_for(output: &StepOutput) -> Handed {
    (output.run_id.clone(), output.unit_ix, output.attempt)
}

static NEXT_HANDOFF: AtomicU64 = AtomicU64::new(1);

/// On the actor: `run_id` is relaunched. A result still out for its reads belongs to a life of the
/// run that is over, and is dropped when it comes back.
pub(crate) fn forget_run(run_id: &str) {
    HANDED.with(|h| h.borrow_mut().retain(|_, (r, _, _)| r != run_id));
}

/// What to do with a step result that arrives on the actor, before its fold.
pub(crate) enum Arrival {
    /// Fold it (it was never handed off, or its hand-off is still this run's).
    Fold,
    /// Drop it (acked): it comes back from a hand-off the run outlived, or it is a second copy of
    /// a result whose hand-off is still out.
    Drop,
}

/// On the actor: account for a step result's hand-off. A result back from a live hand-off is
/// folded with its reads; one from a forgotten hand-off is dropped, and so is a copy of a result
/// whose hand-off is still out. Reads a result carries without a hand-off of this actor are
/// stripped, so the fold runs them itself (fail-closed).
pub(crate) fn returned(output: &StepOutput, evidence: &mut UnitEvidence) -> Arrival {
    match evidence.fold_handoff {
        Some(id) => {
            if HANDED.with(|h| h.borrow_mut().remove(&id)).is_some() {
                Arrival::Fold
            } else {
                Arrival::Drop
            }
        }
        None => {
            evidence.pinned_validator = None;
            evidence.creator_paths = None;
            let key = handed_for(output);
            if HANDED.with(|h| h.borrow().values().any(|k| *k == key)) {
                Arrival::Drop
            } else {
                Arrival::Fold
            }
        }
    }
}

/// The step result as the worker posted it, carried to the reads' thread and posted back.
pub(crate) struct Posted {
    pub output: StepOutput,
    pub agent_verdict: Option<crate::validator::AgentVerdict>,
    pub evidence: Box<UnitEvidence>,
    pub process_gen: Option<uuid::Uuid>,
    pub launch_seq: u64,
    pub ack: Option<std::sync::mpsc::SyncSender<()>>,
}

/// What the fold of a result would run on the actor, with the inputs it would run on.
pub(crate) struct Reads {
    unit: WorkUnit,
    /// The validator and its inputs; `denial` is filled on the thread.
    pinned: Option<PinnedValidatorRead>,
    /// The creator floor's diff: the worktree, the unit's baseline and the tree its checks ran on.
    creator: Option<(String, WorktreeSnapshot, String)>,
}

/// The reads the fold of `output` would run on the actor, decided from the store now; `None` when
/// it would run none, or `output` is not a result the fold applies as it stands (a failed step,
/// a run that ended, a superseded attempt or a moved cursor: those paths run no validator, or
/// drop the result).
pub(crate) fn plan(
    store: &dyn GraphStore,
    output: &StepOutput,
    evidence: &UnitEvidence,
) -> Option<Reads> {
    if !matches!(output.status, StepStatus::Ok)
        || evidence.fold_handoff.is_some()
        || evidence.pinned_validator.is_some()
        || evidence.creator_paths.is_some()
    {
        return None;
    }
    let session = crate::domain::get_session(store, &output.run_id)
        .ok()
        .flatten()?;
    if matches!(
        session.status,
        SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
    ) || session.unit_ix != output.unit_ix
        || session.attempt != output.attempt
    {
        return None;
    }
    let unit = crate::domain::session_unit_at(store, &output.run_id, output.unit_ix)
        .ok()
        .flatten()?;
    if unit.status == UnitStatus::Done {
        return None;
    }
    // The same inputs `apply_step_result` and the fold compute: the session's worktree, the
    // repository's own code graph (`coverage_db`) and the walkthrough variables.
    let pinned = unit.validator.clone().map(|validator| PinnedValidatorRead {
        validator,
        workdir: session.workdir.clone(),
        db_path: super::repo_code_graph_db(store, session.repo_ref.as_deref()),
        env: crate::walkthrough::validator_env(
            &session,
            &unit,
            crate::walkthrough::garden_root_for(&unit).as_deref(),
        ),
        denial: None,
    });
    let verify_floor = unit.repo_checks_floor || crate::deliver_lift::is_deliver_unit(&unit);
    let creator = match (
        evidence.repo_checks.as_ref().and_then(|r| r.tree.clone()),
        unit.worktree_baseline.clone(),
        session.workdir.clone(),
    ) {
        (Some(after), Some(baseline), Some(wd)) if !verify_floor => Some((wd, baseline, after)),
        _ => None,
    };
    (pinned.is_some() || creator.is_some()).then_some(Reads {
        unit,
        pinned,
        creator,
    })
}

/// The run's lifecycle maps, where a run's live child process groups are registered.
pub(crate) type Maps = Arc<Mutex<crate::acp_runner::ElicitationMaps>>;

/// (core#1742) The validator's process group is registered under its run, as a tool child is, so
/// the run's reassign or cancel stops it before a replacement can be dispatched into the same
/// worktree. `seq` is the run's launch identity at the hand-off: a reassign, a cancel and a
/// relaunch each advance it. A group spawned after that (the reassign landed between the hand-off
/// and the spawn) is stopped at once, since the reassign has already taken the groups it kills.
fn validator_watch(maps: Maps, run_id: String, seq: u64) -> crate::validator::SpawnWatch {
    let (m, run) = (maps.clone(), run_id.clone());
    crate::validator::SpawnWatch {
        on_spawn: Box::new(move |pgid| {
            let stale = {
                let mut g = m.lock().unwrap_or_else(|p| p.into_inner());
                if g.current_launch_seq(&run) == seq {
                    // A tool child of the same run cannot be live here: the step's result, which
                    // ends it, is what this read was handed off for.
                    let _ = g.register_tool_child(&run, pgid);
                    false
                } else {
                    true
                }
            };
            if stale {
                #[cfg(unix)]
                crate::validator::kill_pgroup_graceful(pgid);
            }
        }),
        on_done: Box::new(move |pgid| {
            maps.lock()
                .unwrap_or_else(|p| p.into_inner())
                .deregister_tool_child(&run_id, pgid);
        }),
    }
}

/// On the actor: register the hand-off, then run `reads` on a thread of their own and post `posted`
/// back to the actor with them. `maps` (the actor's lifecycle maps, when it has them) is where the
/// validator's process group is registered while it runs ([`validator_watch`]).
pub(crate) fn start(self_tx: &Sender<Command>, maps: Option<&Maps>, reads: Reads, posted: Posted) {
    let id = NEXT_HANDOFF.fetch_add(1, Ordering::Relaxed);
    HANDED.with(|h| h.borrow_mut().insert(id, handed_for(&posted.output)));
    let tx = self_tx.clone();
    let run_id = posted.output.run_id.clone();
    let watch = maps.map(|m| {
        let seq = m
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .current_launch_seq(&run_id);
        (m.clone(), seq)
    });
    std::thread::spawn(move || {
        let Posted {
            output,
            agent_verdict,
            mut evidence,
            process_gen,
            launch_seq,
            ack,
        } = posted;
        let Reads {
            unit,
            pinned,
            creator,
        } = reads;
        if let Some(mut read) = pinned {
            // A validator run that panics still answers, fail-closed: a pinned phase that could not
            // be re-verified is NOT passed.
            let run = || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::pipeline::pinned_validator_denial_with_env(
                        &unit,
                        read.workdir.as_deref().map(Path::new),
                        read.db_path.as_deref(),
                        &read.env,
                    )
                }))
                .unwrap_or_else(|_| {
                    Some(format!(
                        "pinned validator `{}` could not be re-verified: its run panicked \
                         (fail-closed — an un-re-verifiable pin is treated as NOT-passed)",
                        read.validator.criterion
                    ))
                })
            };
            read.denial = match &watch {
                Some((maps, seq)) => crate::validator::with_spawn_watch(
                    validator_watch(maps.clone(), run_id.clone(), *seq),
                    run,
                ),
                None => run(),
            };
            // (core#1742) A validator the run's reassign or cancel stopped has no verdict: its read
            // is not carried, so a fold that still applies runs the validator itself.
            let superseded = watch.as_ref().is_some_and(|(maps, seq)| {
                maps.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .current_launch_seq(&run_id)
                    != *seq
            });
            if !superseded {
                evidence.pinned_validator = Some(read);
            }
        }
        if let Some((wd, baseline, after)) = creator {
            let paths = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::pipeline::creator_paths_between(&wd, &baseline, &after, unit.ord)
            }))
            .unwrap_or(None);
            evidence.creator_paths = Some(CreatorPathsRead {
                baseline_tree: baseline.tree.clone(),
                after_tree: after,
                paths,
            });
        }
        evidence.fold_handoff = Some(id);
        let _ = tx.send(Command::ApplyStepResult {
            output,
            agent_verdict,
            evidence,
            process_gen,
            launch_seq,
            ack,
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::put_nodes;
    use std::time::Duration;
    use wicked_apps_core::{open_store, ToNode};

    /// core#1418: a step result whose fold would run the unit's pinned validator is handed to a
    /// thread with the fold's own inputs, and posted back with the validator's answer on its
    /// evidence; a result that already carries a read, a failed step or a superseded attempt is
    /// folded as it is.
    #[test]
    fn a_result_whose_fold_runs_a_pinned_validator_is_read_off_the_actor_once() {
        let wd = std::env::temp_dir().join(format!(
            "wicked-fold-reads-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&wd);
        std::fs::create_dir_all(&wd).unwrap();
        let mut store = open_store(Some(":memory:")).unwrap();
        let mut session = crate::actor::sessions_detail_fold_tests::session("r-fold");
        session.status = SessionStatus::Executing;
        session.workdir = Some(wd.to_string_lossy().into_owned());
        let mut unit = WorkUnit::pending("r-fold:u1", "r-fold", 1, "d");
        unit.validator = Some(crate::validator::DeterministicValidator {
            criterion: "c".into(),
            script: "exit 0".into(),
            approved: true,
        });
        put_nodes(&mut store, &[session.to_node(), unit.to_node()]).unwrap();
        let output = StepOutput {
            run_id: "r-fold".into(),
            unit_ix: 0,
            attempt: 0,
            output: "done".into(),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        };

        let reads = plan(&store, &output, &UnitEvidence::default())
            .expect("the fold's validator is read off the actor");
        let read = reads.pinned.clone().expect("the pinned validator");
        assert_eq!(read.workdir, session.workdir, "the fold's own worktree");
        assert_eq!(
            read.db_path, None,
            "no registered repository, no code graph"
        );
        assert!(reads.creator.is_none(), "no floor report, no diff");

        let carried = UnitEvidence {
            pinned_validator: Some(read),
            ..Default::default()
        };
        assert!(
            plan(&store, &output, &carried).is_none(),
            "a result that carries a read is folded as it is"
        );
        let failed = StepOutput {
            status: StepStatus::Failed,
            ..output.clone()
        };
        assert!(plan(&store, &failed, &UnitEvidence::default()).is_none());
        let superseded = StepOutput {
            attempt: 7,
            ..output.clone()
        };
        assert!(plan(&store, &superseded, &UnitEvidence::default()).is_none());

        let posted = |output: &StepOutput| Posted {
            output: output.clone(),
            agent_verdict: None,
            evidence: Box::default(),
            process_gen: None,
            launch_seq: 0,
            ack: None,
        };
        let back = |rx: &std::sync::mpsc::Receiver<Command>| -> Box<UnitEvidence> {
            match rx.recv_timeout(Duration::from_secs(120)) {
                Ok(Command::ApplyStepResult {
                    output, evidence, ..
                }) => {
                    assert_eq!(output.run_id, "r-fold");
                    evidence
                }
                Ok(_) => panic!("only the result is posted back"),
                Err(e) => panic!("the result is posted back: {e}"),
            }
        };
        let (tx, rx) = std::sync::mpsc::channel();
        start(&tx, None, reads, posted(&output));
        let mut evidence = back(&rx);
        let read = evidence
            .pinned_validator
            .clone()
            .expect("the result comes back with the read");
        assert_eq!(read.denial, None, "the validator passed, off the actor");
        assert!(
            matches!(returned(&output, &mut evidence), Arrival::Fold),
            "a result back from a live hand-off is folded"
        );
        assert!(evidence.pinned_validator.is_some(), "with its read");

        // A failing validator's denial comes back on the result (fail-closed off the actor too).
        unit.validator = Some(crate::validator::DeterministicValidator {
            criterion: "c".into(),
            script: "exit 1".into(),
            approved: true,
        });
        put_nodes(&mut store, &[unit.to_node()]).unwrap();
        let reads = plan(&store, &output, &UnitEvidence::default()).expect("planned again");
        start(&tx, None, reads, posted(&output));
        let mut evidence = back(&rx);
        assert!(
            evidence
                .pinned_validator
                .as_ref()
                .and_then(|r| r.denial.as_deref())
                .is_some_and(|d| d.starts_with("pinned validator failed")),
            "the failing validator's denial comes back: {:?}",
            evidence.pinned_validator
        );
        assert!(matches!(returned(&output, &mut evidence), Arrival::Fold));

        // A second copy of the result (a duplicate delivery) while its hand-off is out is dropped:
        // two validators never run in one worktree for one attempt. The hand-off still folds.
        let reads = plan(&store, &output, &UnitEvidence::default()).expect("planned again");
        start(&tx, None, reads, posted(&output));
        assert!(
            matches!(
                returned(&output, &mut UnitEvidence::default()),
                Arrival::Drop
            ),
            "a copy of a result whose hand-off is out is dropped"
        );
        let mut evidence = back(&rx);
        assert!(matches!(returned(&output, &mut evidence), Arrival::Fold));

        // The run is relaunched while its reads run: the result is dropped on return.
        let reads = plan(&store, &output, &UnitEvidence::default()).expect("planned again");
        start(&tx, None, reads, posted(&output));
        forget_run("r-fold");
        let mut evidence = back(&rx);
        assert!(
            matches!(returned(&output, &mut evidence), Arrival::Drop),
            "a result its run outlived never folds"
        );

        // Reads this actor did not hand off are stripped: the fold runs them itself.
        let mut forged = UnitEvidence {
            pinned_validator: evidence.pinned_validator.clone(),
            ..Default::default()
        };
        assert!(matches!(returned(&output, &mut forged), Arrival::Fold));
        assert!(
            forged.pinned_validator.is_none(),
            "a read that was not handed off is not applied"
        );
        let _ = std::fs::remove_dir_all(&wd);
    }

    /// (core#1742) A validator handed off the actor is registered under its run while it runs: the
    /// run's reassign (advance the launch identity, take the run's groups, stop each) stops it
    /// before it writes into the worktree a replacement will use, and the result comes back with
    /// no verdict, so a fold that still applies runs the validator itself. A group spawned after the
    /// reassign took the run's groups is stopped at its spawn.
    #[cfg(unix)]
    #[test]
    fn a_handed_off_validator_is_stopped_when_its_attempt_is_superseded() {
        let wd = std::env::temp_dir().join(format!(
            "wicked-fold-reads-stop-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&wd).unwrap();
        let mut store = open_store(Some(":memory:")).unwrap();
        let mut session = crate::actor::sessions_detail_fold_tests::session("r-stop");
        session.status = SessionStatus::Executing;
        session.workdir = Some(wd.to_string_lossy().into_owned());
        let mut unit = WorkUnit::pending("r-stop:u1", "r-stop", 1, "d");
        // A validator that writes into the worktree after a while (the coverage validator rewrites
        // its report; an approved script may write anywhere in the tree).
        unit.validator = Some(crate::validator::DeterministicValidator {
            criterion: "c".into(),
            script: "sleep 3; touch late-write.txt".into(),
            approved: true,
        });
        put_nodes(&mut store, &[session.to_node(), unit.to_node()]).unwrap();
        // The control: run to its end, this validator does write into the tree.
        assert_eq!(
            crate::pipeline::pinned_validator_denial_with_env(&unit, Some(wd.as_path()), None, &[]),
            None
        );
        assert!(wd.join("late-write.txt").exists(), "the control run writes");
        std::fs::remove_file(wd.join("late-write.txt")).unwrap();
        let output = StepOutput {
            run_id: "r-stop".into(),
            unit_ix: 0,
            attempt: 0,
            output: "done".into(),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        };
        let maps: Maps = Arc::new(Mutex::new(crate::acp_runner::ElicitationMaps::new()));
        maps.lock().unwrap().advance_launch_seq("r-stop");
        let posted = Posted {
            output: output.clone(),
            agent_verdict: None,
            evidence: Box::default(),
            process_gen: None,
            launch_seq: 1,
            ack: None,
        };
        let reads = plan(&store, &output, &UnitEvidence::default()).expect("handed off");
        let (tx, rx) = std::sync::mpsc::channel();
        let began = std::time::Instant::now();
        start(&tx, Some(&maps), reads, posted);
        // The validator's group shows up under the run (bounded wait: it spawns on the thread).
        let pgroups = loop {
            let mut g = maps.lock().unwrap();
            let seen = g.take_tool_child_pgroups("r-stop");
            if !seen.is_empty() {
                break seen;
            }
            drop(g);
            assert!(
                began.elapsed() < Duration::from_secs(20),
                "the validator's group is registered under its run"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        // `ReassignUnit`'s order: advance the launch identity, then stop the run's groups. (The
        // groups were taken above; put back, the reassign takes them itself.)
        {
            let mut g = maps.lock().unwrap();
            for p in &pgroups {
                let _ = g.register_tool_child("r-stop", *p);
            }
            g.advance_launch_seq("r-stop");
        }
        let taken = maps.lock().unwrap().take_tool_child_pgroups("r-stop");
        assert_eq!(taken, pgroups);
        for pgid in taken {
            crate::validator::kill_pgroup_graceful(pgid);
        }
        let evidence = match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(Command::ApplyStepResult { evidence, .. }) => evidence,
            Ok(_) => panic!("only the result is posted back"),
            Err(e) => panic!("the result is posted back: {e}"),
        };
        std::thread::sleep(Duration::from_millis(3500));
        assert!(
            !wd.join("late-write.txt").exists(),
            "the stopped validator never wrote into the worktree"
        );
        assert!(
            evidence.pinned_validator.is_none(),
            "a stopped validator's read is not carried: {:?}",
            evidence.pinned_validator
        );
        assert!(
            evidence.fold_handoff.is_some(),
            "it is still this hand-off's result"
        );
        assert!(
            maps.lock()
                .unwrap()
                .take_tool_child_pgroups("r-stop")
                .is_empty(),
            "nothing is left registered"
        );

        // A group spawned after the reassign took the run's groups is stopped at its spawn.
        let late = validator_watch(maps.clone(), "r-stop".into(), 1);
        let t = std::time::Instant::now();
        let denial = crate::validator::with_spawn_watch(late, || {
            crate::pipeline::pinned_validator_denial_with_env(&unit, Some(wd.as_path()), None, &[])
        });
        assert!(denial.is_some(), "a stopped validator never passes");
        assert!(
            t.elapsed() < Duration::from_secs(3),
            "stopped at spawn, not run out ({:?})",
            t.elapsed()
        );
        std::thread::sleep(Duration::from_millis(3500));
        assert!(!wd.join("late-write.txt").exists());
        assert!(maps
            .lock()
            .unwrap()
            .take_tool_child_pgroups("r-stop")
            .is_empty());
        let _ = std::fs::remove_dir_all(&wd);
    }

    /// (coord review of #1739, F3) Both launch entry points forget an earlier life's hand-offs:
    /// the interactive `LaunchRun` fast path (pinned end to end by `tests/fold_reads_handoff.rs`)
    /// and `launch_run_inner`, the campaign's node launch. A launch that skipped it would let the
    /// earlier life's result fold into the new one.
    #[test]
    fn both_launch_paths_forget_an_earlier_lifes_hand_offs() {
        let src = include_str!("../actor.rs");
        // Built by concatenation so this test cannot satisfy itself out of its own text.
        let call = concat!("fold_reads::", "forget_run(&run_id);");
        let arm = src
            .find("Command::LaunchRun { spec, reply } => {")
            .expect("the LaunchRun arm");
        let arm_end = src[arm..]
            .find("Command::ContinueLaunch {")
            .map(|e| arm + e)
            .unwrap_or(src.len());
        assert!(
            src[arm..arm_end].contains(call),
            "the interactive launch no longer forgets an earlier life's hand-offs"
        );
        let inner = src
            .find("pub(crate) fn launch_run_inner(")
            .expect("launch_run_inner");
        let inner_end = src[inner + 1..]
            .find("\npub(crate) fn ")
            .map(|e| inner + 1 + e)
            .unwrap_or(src.len());
        assert!(
            src[inner..inner_end].contains(call),
            "launch_run_inner (a campaign node launch) no longer forgets an earlier life's hand-offs"
        );
    }
}
