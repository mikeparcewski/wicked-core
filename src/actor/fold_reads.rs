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

use std::path::Path;
use std::sync::mpsc::Sender;

use wicked_apps_core::GraphStore;

use crate::command::Command;
use crate::domain::{SessionStatus, UnitStatus, WorkUnit};
use crate::workflow::{
    CreatorPathsRead, PinnedValidatorRead, StepOutput, StepStatus, UnitEvidence,
};
use crate::worktree_guard::WorktreeSnapshot;

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

/// Run `reads` on a thread of their own and post `posted` back to the actor with them.
pub(crate) fn start(self_tx: &Sender<Command>, reads: Reads, posted: Posted) {
    let tx = self_tx.clone();
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
            read.denial = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
            });
            evidence.pinned_validator = Some(read);
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

        let (tx, rx) = std::sync::mpsc::channel();
        start(
            &tx,
            reads,
            Posted {
                output,
                agent_verdict: None,
                evidence: Box::default(),
                process_gen: None,
                launch_seq: 0,
                ack: None,
            },
        );
        match rx.recv_timeout(Duration::from_secs(120)) {
            Ok(Command::ApplyStepResult {
                output, evidence, ..
            }) => {
                assert_eq!(output.run_id, "r-fold");
                let read = evidence
                    .pinned_validator
                    .expect("the result comes back with the read");
                assert_eq!(read.denial, None, "the validator passed, off the actor");
            }
            Ok(_) => panic!("only the result is posted back"),
            Err(e) => panic!("the result is posted back: {e}"),
        }
        let _ = std::fs::remove_dir_all(&wd);
    }
}
