//! (core#1390) The DISPATCH PROBE: the reads of a run's worktree a unit's dispatch needs, run off
//! the actor.
//!
//! A dispatch used to read the worktree on the actor thread, up to four times: the unit's baseline
//! snapshot, the tree a new author starts on, the tree a QE unit's floor note is about and the tree
//! an editor-conformance record must match. Each snapshot re-hashes every tracked file (a fresh
//! scratch index has no stat cache), 30–45 s for a 1,300-file worktree on a loaded host, and every
//! read and gate answer of every run waited behind it. A QE unit's diff score (the code graph and
//! git history) and a reviewer's requirements record (the code graph and git) ran there too.
//!
//! Now [`super::dispatch_unit`] takes no tree read on the actor. When the unit needs one, it starts
//! a probe thread and returns. The thread takes ONE snapshot (all four uses read the same tree: no
//! worker of the run is live between the probe and the dispatch, and a tree id is the content's,
//! whichever pinned git dir it was read through), scores the QE unit and reads the requirements
//! record on it, then posts [`crate::command::Command::DispatchProbed`]. The actor runs the dispatch
//! again with the probe in hand. Everything a dispatch decides is still decided there, on the
//! store as it stands then: a run that ended or moved on while the probe ran, or a newer dispatch
//! of the same run, drops the probe. A probe result is used only when the inputs it was computed on
//! are the ones the dispatch has; otherwise the dispatch reads it itself, as before.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use crate::assurance::QeAcceptance;
use crate::command::Command;
use crate::domain::{AgentSession, WorkUnit};
use crate::repo::RepoEntry;
use crate::worktree_guard::WorktreeSnapshot;

/// TEST-ONLY: a probe of a run named here parks before it reads, until released, and says so
/// first — a probe that takes as long as the test needs.
#[cfg(test)]
pub(crate) type Hold = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
#[cfg(test)]
pub(crate) static HOLD: std::sync::Mutex<Option<HashMap<String, Hold>>> =
    std::sync::Mutex::new(None);

thread_local! {
    /// The actor's runs whose dispatch waits on a probe, by probe number: a newer dispatch of the
    /// run supersedes the older probe, whose answer is then dropped. Actor-thread state only.
    static PROBING: RefCell<HashMap<String, u64>> = RefCell::new(HashMap::new());
    static NEXT: Cell<u64> = const { Cell::new(1) };
}

/// The probe's QE score and the inputs it was computed on.
pub(crate) struct ScoredQe {
    pub ord: u32,
    pub base_commit: Option<String>,
    pub tree: String,
    pub git_dir: String,
    pub score: QeAcceptance,
}

/// The probe's requirements record and the inputs it was read on.
pub(crate) struct Requirements {
    pub tree: String,
    pub base_commit: String,
    pub record: Result<Option<String>, String>,
}

/// What the probe thread read for one dispatch.
pub(crate) struct DispatchProbe {
    pub(crate) seq: u64,
    pub(crate) unit_id: String,
    pub(crate) attempt: u32,
    /// The run's status and cursor when the probe started: a run that moved on since (paused,
    /// ended, relaunched, advanced) is not this dispatch's any more.
    pub(crate) status: crate::domain::SessionStatus,
    pub(crate) cursor: usize,
    /// What the probe read FROM: the run's worktree and its repository's registration (id, root,
    /// code graph). The dispatch takes the probe only on these same inputs, and probes again when
    /// they changed (a repository re-registered meanwhile).
    pub(crate) inputs: ProbeInputs,
    /// The one snapshot of the worktree, pinned to the registered repository.
    pub(crate) snapshot: Result<WorktreeSnapshot, String>,
    pub(crate) qe: Option<ScoredQe>,
    pub(crate) requirements: Option<Requirements>,
}

impl DispatchProbe {
    /// The fresh tree, when the snapshot succeeded.
    pub(crate) fn tree(&self) -> Option<&str> {
        self.snapshot.as_ref().ok().map(|s| s.tree.as_str())
    }

    /// The probe's QE score, if it was computed on exactly these inputs.
    pub(crate) fn qe_score_for(
        &self,
        ord: u32,
        base_commit: Option<&str>,
        baseline: Option<(&str, &Path)>,
    ) -> Option<QeAcceptance> {
        let q = self.qe.as_ref()?;
        let (tree, git_dir) = baseline?;
        (q.ord == ord
            && q.base_commit.as_deref() == base_commit
            && q.tree == tree
            && Path::new(&q.git_dir) == git_dir)
            .then(|| q.score.clone())
    }

    /// The probe's requirements record, if it was read on exactly this tree and base.
    pub(crate) fn requirements_for(
        &self,
        tree: &str,
        base_commit: &str,
    ) -> Option<Result<Option<String>, String>> {
        let r = self.requirements.as_ref()?;
        (r.tree == tree && r.base_commit == base_commit).then(|| r.record.clone())
    }
}

/// The inputs a probe read from ([`DispatchProbe::inputs`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeInputs {
    pub(crate) workdir: PathBuf,
    pub(crate) repo_id: String,
    pub(crate) repo_root: String,
    pub(crate) code_graph_db: String,
}

impl ProbeInputs {
    pub(crate) fn of(workdir: &Path, repo: &RepoEntry) -> Self {
        Self {
            workdir: workdir.to_path_buf(),
            repo_id: repo.id.clone(),
            repo_root: repo.root_path.clone(),
            code_graph_db: repo.code_graph_db.clone(),
        }
    }
}

/// What a dispatch needs read off the actor: decided on the actor from the store, carried to the
/// probe thread.
pub(crate) struct ProbeJob {
    pub(crate) run_id: String,
    pub(crate) unit_ix: usize,
    pub(crate) attempt: u32,
    pub(crate) unit: WorkUnit,
    pub(crate) session: AgentSession,
    pub(crate) workdir: PathBuf,
    pub(crate) repo: RepoEntry,
    /// The state home the actor's code-graph resolver is bound to: the probe thread resolves the
    /// QE score's graph through the same one (a thread-local binding does not follow a thread).
    pub(crate) state_home: Option<PathBuf>,
    /// Score the QE unit on the tree it starts on.
    pub(crate) score_qe: bool,
    /// Read the reviewer's requirements record.
    pub(crate) requirements: bool,
}

/// Start `job`'s probe: it supersedes any probe of the same run still out.
pub(crate) fn start(self_tx: &Sender<Command>, job: ProbeJob) {
    let seq = NEXT.with(|n| {
        let s = n.get();
        n.set(s + 1);
        s
    });
    PROBING.with(|p| p.borrow_mut().insert(job.run_id.clone(), seq));
    let tx = self_tx.clone();
    std::thread::spawn(move || {
        let run_id = job.run_id.clone();
        let unit_ix = job.unit_ix;
        #[cfg(test)]
        {
            let held = HOLD
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_mut()
                .and_then(|m| m.remove(&run_id));
            if let Some((entered, release)) = held {
                let _ = entered.send(());
                let _ = release.recv();
            }
        }
        let (unit_id, attempt, status, cursor) = (
            job.unit.id.clone(),
            job.attempt,
            job.session.status,
            job.session.unit_ix,
        );
        let inputs = ProbeInputs::of(&job.workdir, &job.repo);
        // A probe that panics still answers — with no tree, so the dispatch goes on fail-closed
        // (no baseline: a guarded gate denies) rather than leaving the run waiting for ever.
        let probe = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(seq, job)))
            .unwrap_or_else(|_| DispatchProbe {
                seq,
                unit_id,
                attempt,
                status,
                cursor,
                inputs,
                snapshot: Err("the worktree probe panicked".into()),
                qe: None,
                requirements: None,
            });
        let _ = tx.send(Command::DispatchProbed {
            run_id,
            unit_ix,
            probe: Box::new(probe),
        });
    });
}

/// Whether `probe` is the run's live probe; it is taken off the list either way.
pub(crate) fn take_current(run_id: &str, probe: &DispatchProbe) -> bool {
    PROBING.with(|p| {
        let mut p = p.borrow_mut();
        if p.get(run_id) == Some(&probe.seq) {
            p.remove(run_id);
            true
        } else {
            false
        }
    })
}

/// Forget `run_id`'s probe: the run ended or was launched afresh, and the probe's answer, when it
/// lands, is dropped (it is no longer the run's live one).
pub(crate) fn forget(run_id: &str) {
    PROBING.with(|p| {
        p.borrow_mut().remove(run_id);
    });
}

fn run(seq: u64, job: ProbeJob) -> DispatchProbe {
    let _state_home = crate::code_graph::StateHomeScope::bind(job.state_home.clone());
    let root = PathBuf::from(&job.repo.root_path);
    let snapshot = crate::worktree_guard::snapshot(&job.workdir, &root).map_err(|e| e.to_string());
    // The baseline the dispatch will hold: the unit's own, or this snapshot when it has none.
    let baseline = job
        .unit
        .worktree_baseline
        .clone()
        .or_else(|| snapshot.as_ref().ok().cloned());
    let qe = match (&baseline, job.score_qe) {
        (Some(b), true) => b.git_dir.as_deref().map(|g| ScoredQe {
            ord: job.unit.ord,
            base_commit: job.session.base_commit.clone(),
            tree: b.tree.clone(),
            git_dir: g.to_string(),
            score: crate::qe_acceptance::score_ahead(
                &job.session,
                job.unit.ord,
                Some(&root),
                Some((b.tree.as_str(), Path::new(g))),
            ),
        }),
        _ => None,
    };
    let requirements = match (
        &baseline,
        job.requirements,
        job.session.base_commit.as_deref(),
    ) {
        (Some(b), true, Some(base)) => Some(Requirements {
            tree: b.tree.clone(),
            base_commit: base.to_string(),
            record: crate::worktree_guard::paths_since_commit(&job.workdir, b, base)
                .and_then(|files| {
                    crate::repo::requirements_evidence_for(&job.repo, &files, Some(base))
                })
                .map_err(|e| e.to_string()),
        }),
        _ => None,
    };
    DispatchProbe {
        seq,
        unit_id: job.unit.id.clone(),
        attempt: job.attempt,
        status: job.session.status,
        cursor: job.session.unit_ix,
        inputs: ProbeInputs::of(&job.workdir, &job.repo),
        snapshot,
        qe,
        requirements,
    }
}

#[cfg(test)]
#[path = "dispatch_probe_tests.rs"]
mod tests;
