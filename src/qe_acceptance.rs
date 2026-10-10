//! (QE-IN-APP-WORKFLOWS, operator ruling 2026-10-10) The BINDING QE acceptance decision.
//!
//! A run whose contract requires `qe_acceptance` launches with a provisional decision
//! (`basis: plan`, always `required`: a plan has no diff, so it cannot waive) or the operator's
//! (`basis: operator`: an explicit skip with a reason, or a force). When the run's QE phase — its
//! code-verifying unit (`repo_checks_floor`: `verified_evidence` with an `executes_code` creator
//! before it) — dispatches, the engine scores the run's ACTUAL diff (base commit .. the tree the
//! unit starts on) through the one scorer, `review_scale::assess`, with the history of the
//! touched paths, and decides: `waived` only when `review_scale::qe_waivable` holds (every
//! dimension in its lowest band), else `required`. The decision is persisted on the session's
//! contract, published (`qeAcceptanceDecided`), stamped on the unit (its prompt says whether to
//! run garden's acceptance pipeline) and carried on every later receipt.
//!
//! A waiver covers the tree it scored and nothing after it: a creator unit dispatched after a
//! waiver revokes it (`required`), and the QE phase re-scores at its next dispatch; a QE dispatch
//! that carries a floor fix (a seat changes the tree after the score) is `required`. Anything that
//! cannot be read — no repo, no base, no snapshot, a git failure, no current graph — is
//! `required` with the reason (fail-closed). An operator decision is never re-scored.

use std::collections::BTreeSet;
use std::path::Path;

use crate::assurance::{self as a, QeAcceptance};
use crate::domain::{AgentSession, WorkUnit};
use crate::review_scale::{self as rs, ChangeSignals, Graph};

/// At most this many touched paths have their history read (each is one `git rev-list`).
const HISTORY_PATH_CAP: usize = 50;

/// The QE phase of a run: its code-verifying unit.
pub(crate) fn is_qe_unit(unit: &WorkUnit) -> bool {
    unit.repo_checks_floor && unit.tool_cmd.is_none()
}

/// A unit that may change the tree a waiver scored: every creator, whatever its executor or its
/// `executes_code` (a `produce` creator writes too, and so does a creator Tool step), and every
/// `executes_code` unit whatever its role (a neutral cutover) — except the deliver step, which
/// ships the tree and changes nothing the waiver covered.
fn may_change_the_tree(unit: &WorkUnit) -> bool {
    (unit.role == crate::workflow::PhaseRole::Creator || unit.executes_code)
        && !crate::deliver_lift::is_deliver_unit(unit)
}

/// (core#782) The QE unit's dispatch carries a FLOOR FIX: a seat changes the tree after this
/// dispatch scores it, so the scored tree is not the one that will be verified and delivered.
fn floor_fix_pending(unit: &WorkUnit) -> bool {
    unit.repo_checks
        .as_ref()
        .and_then(|r| r.requested_rerun.as_ref())
        .is_some_and(|r| r.fix.is_some())
}

/// What a dispatch did to the run's decision, for the caller to persist and publish.
pub(crate) enum Dispatched {
    /// Nothing changed.
    Unchanged,
    /// The run's decision is now this (persist it on the session and publish it).
    Decided(QeAcceptance),
}

/// The decision a dispatch of `unit` makes, from the session as persisted. `tree` is the tree the
/// unit starts on (its dispatch baseline) and `git_dir` the registered repo's pinned git dir;
/// `repo_root` is the registered repo, whose code graph is read at the run's base commit.
pub(crate) fn on_dispatch(
    session: &AgentSession,
    unit: &WorkUnit,
    repo_root: Option<&Path>,
    baseline: Option<(&str, &Path)>,
) -> Dispatched {
    let Some(current) = session.assurance.qe.as_ref() else {
        return Dispatched::Unchanged;
    };
    if current.by_operator() {
        return Dispatched::Unchanged;
    }
    if may_change_the_tree(unit) && current.status == a::QE_WAIVED {
        return Dispatched::Decided(QeAcceptance {
            status: a::QE_REQUIRED.to_string(),
            basis: a::QE_BASIS_DIFF.to_string(),
            reason: format!(
                "required: creator unit {} runs after the waiver at unit {}, and a waiver covers \
                 only the tree it scored ({})",
                unit.ord,
                current.ord.map_or("?".to_string(), |o| o.to_string()),
                current.reason
            ),
            ord: Some(unit.ord),
            ..current.clone()
        });
    }
    if !is_qe_unit(unit) {
        return Dispatched::Unchanged;
    }
    let decided = if floor_fix_pending(unit) {
        QeAcceptance {
            status: a::QE_REQUIRED.to_string(),
            basis: a::QE_BASIS_DIFF.to_string(),
            score: None,
            threshold: rs::THRESHOLDS.qe_waiver_max_score,
            reason:
                "required: a floor fix changes the tree after this dispatch, so no score of it \
                     can waive the tree that will be delivered"
                    .to_string(),
            reasons: Vec::new(),
            ord: Some(unit.ord),
            tree: None,
        }
    } else {
        decide(session, unit.ord, repo_root, baseline)
    };
    if decided == *current {
        Dispatched::Unchanged
    } else {
        Dispatched::Decided(decided)
    }
}

/// Score the run's diff and decide, against the run repo's code graph at the base. Every
/// unreadable input is `required` with its reason.
fn decide(
    session: &AgentSession,
    ord: u32,
    repo_root: Option<&Path>,
    baseline: Option<(&str, &Path)>,
) -> QeAcceptance {
    decide_with(session, ord, repo_root, baseline, |signals, root, base| {
        match crate::code_graph::existing_code_graph(root) {
            None => rs::assess(
                signals,
                Graph::Unavailable(format!("no code graph is indexed for {}", root.display())),
                None,
            ),
            Some(db) => match wicked_apps_core::open_store_ro(Some(&db.to_string_lossy())) {
                Ok(store) => rs::assess(
                    signals,
                    Graph::Ready {
                        store: &store,
                        base_commit: base,
                    },
                    None,
                ),
                Err(e) => rs::assess(
                    signals,
                    Graph::Unavailable(format!("the code graph could not be opened: {e}")),
                    None,
                ),
            },
        }
    })
}

/// [`decide`] with the scorer injected: `score(signals, repo_root, base_commit)` reads the graph.
fn decide_with(
    session: &AgentSession,
    ord: u32,
    repo_root: Option<&Path>,
    baseline: Option<(&str, &Path)>,
    score: impl FnOnce(&ChangeSignals, &Path, &str) -> rs::Assessment,
) -> QeAcceptance {
    let threshold = rs::THRESHOLDS.qe_waiver_max_score;
    let required = |reason: String, tree: Option<&str>| QeAcceptance {
        status: a::QE_REQUIRED.to_string(),
        basis: a::QE_BASIS_DIFF.to_string(),
        score: None,
        threshold,
        reason: format!("required: {reason}"),
        reasons: Vec::new(),
        ord: Some(ord),
        tree: tree.map(str::to_string),
    };
    let (Some(root), Some(base)) = (repo_root, session.base_commit.as_deref()) else {
        return required(
            "the run has no repository diff to score (no registered repo or base commit)".into(),
            None,
        );
    };
    let Some((tree, git_dir)) = baseline else {
        return required(
            "the tree the QE phase starts on could not be snapshotted".into(),
            None,
        );
    };
    let diff = match run_diff(root, git_dir, base, tree) {
        Ok(d) => d,
        Err(e) => return required(format!("the run's diff could not be read: {e}"), Some(tree)),
    };
    let mut signals = rs::signals_from_diff(&diff);
    with_history(&mut signals, root, git_dir, base);
    with_whole_files(&mut signals, root, git_dir, base, tree);
    from_assessment(&score(&signals, root, base), ord, tree)
}

/// At most this many bytes of a blob are read for its whole-file surface; a larger file keeps the
/// diff's own signal.
const SURFACE_BYTES_CAP: usize = 1 << 20;

/// (codex r3) Widen the diff's novelty with the touched files' WHOLE contents: the public symbols
/// and dependencies the head blob declares that the base blob does not. A diff hunk shows a few
/// context lines, so a member added far below its re-export group's opener (or an import group
/// made public) is invisible to it; the whole files are not. Unioned with the diff's own sets, so
/// it only ever raises. A blob that cannot be read, or is too large, keeps the diff's signal.
pub(crate) fn with_whole_files(
    signals: &mut ChangeSignals,
    root: &Path,
    git_dir: &Path,
    base: &str,
    tree: &str,
) {
    let blob = |rev: &str, path: &str| -> Option<String> {
        if path.is_empty() {
            return Some(String::new());
        }
        let out = crate::worktree_guard::git(
            root,
            &["cat-file", "-p", &format!("{rev}:{path}")],
            &[("GIT_DIR", git_dir)],
        );
        match out {
            Ok(b) if b.len() <= SURFACE_BYTES_CAP => Some(String::from_utf8_lossy(&b).into_owned()),
            Ok(_) => None,
            // A path absent on a side (a new or deleted file) has an empty surface there.
            Err(_) => Some(String::new()),
        }
    };
    let touched: Vec<(String, String)> = signals
        .touched
        .iter()
        .take(HISTORY_PATH_CAP)
        .map(|f| (f.path.clone(), f.old_path.clone()))
        .collect();
    for (path, old_path) in touched {
        let (Some(old), Some(new)) = (blob(base, &old_path), blob(tree, &path)) else {
            continue;
        };
        let (before, after) = (
            rs::public_surface(&old_path, &old),
            rs::public_surface(&path, &new),
        );
        signals
            .new_public_symbols
            .extend(after.difference(&before).cloned());
        if let (Some(before), Some(after)) = (
            rs::dependency_surface(&old_path, &old).or_else(|| rs::dependency_surface(&path, "")),
            rs::dependency_surface(&path, &new),
        ) {
            signals
                .new_dependencies
                .extend(after.difference(&before).cloned());
        }
    }
}

/// The decision an assessment makes (pure).
pub(crate) fn from_assessment(a_: &rs::Assessment, ord: u32, tree: &str) -> QeAcceptance {
    let threshold = rs::THRESHOLDS.qe_waiver_max_score;
    let (status, reason) = match rs::qe_waivable(a_) {
        Ok(()) => (
            a::QE_WAIVED,
            format!(
                "waived: impact score {} at or below the waiver line {threshold}, every dimension \
                 in its lowest band ({})",
                a_.score,
                a_.reasons.join("; ")
            ),
        ),
        Err(why) => (a::QE_REQUIRED, format!("required: {why}")),
    };
    QeAcceptance {
        status: status.to_string(),
        basis: a::QE_BASIS_DIFF.to_string(),
        score: Some(a_.score),
        threshold,
        reason,
        reasons: a_.reasons.clone(),
        ord: Some(ord),
        tree: Some(tree.to_string()),
    }
}

/// The run's unified diff: its base commit against the tree the QE unit starts on, through the
/// registered repo's pinned git dir (renames detected, so a moved module keeps its importers).
fn run_diff(root: &Path, git_dir: &Path, base: &str, tree: &str) -> anyhow::Result<String> {
    let out = crate::worktree_guard::git(
        root,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "-M",
            base,
            tree,
        ],
        &[("GIT_DIR", git_dir)],
    )?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Count the touched pre-existing paths with little history before `base` into
/// [`ChangeSignals::low_history`]. A path whose history git cannot read counts as little history
/// (unknown leans toward "required"); paths past [`HISTORY_PATH_CAP`] count too.
pub(crate) fn with_history(signals: &mut ChangeSignals, root: &Path, git_dir: &Path, base: &str) {
    let min = rs::THRESHOLDS.low_history_commits;
    let paths: BTreeSet<&str> = signals
        .touched
        .iter()
        .map(|f| f.old_path.as_str())
        .filter(|p| !p.is_empty())
        .collect();
    let mut low = 0u32;
    for (i, p) in paths.iter().enumerate() {
        if i >= HISTORY_PATH_CAP {
            low += 1;
            continue;
        }
        let count = crate::worktree_guard::git_string(
            root,
            &["rev-list", "--count", base, "--", p],
            &[("GIT_DIR", git_dir)],
        )
        .ok()
        .and_then(|c| c.parse::<u32>().ok());
        low += u32::from(count.is_none_or(|c| c < min));
    }
    signals.low_history = low;
}

/// The QE line a unit's prompt carries, from the decision stamped on it. Short: it rides the
/// unit prompt (the PTY carrier's line budget). `skill` is the QE skill as the seat invokes it.
pub(crate) fn directive(qe: &QeAcceptance, skill: &str) -> String {
    match qe.status.as_str() {
        a::QE_REQUIRED => format!(
            " QE ACCEPTANCE IS REQUIRED: run \"{skill}\" accept (writer, executor, isolated \
             reviewer) on a scenario written from this run's acceptance list, so its verdict \
             lands in the QE ledger stamped with WICKED_RUN_ID; delivery is refused without a \
             PASS."
        ),
        a::QE_WAIVED => {
            " QE acceptance is waived for this run (impact score in the lowest band); do not run \
             the acceptance pipeline."
                .to_string()
        }
        _ => " QE acceptance was skipped by the operator; do not run the acceptance pipeline."
            .to_string(),
    }
}

#[cfg(test)]
mod tests;
