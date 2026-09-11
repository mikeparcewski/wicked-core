//! The pre-deliver LIFT (core#431, F-3R2-013): lift the run's verified work onto the remote
//! default branch's CURRENT tip before the `deliver` tool phase pushes, and RE-VERIFY it when
//! the lift changed the tree.
//!
//! # The defect
//!
//! The acceptance run branched from a clone five commits behind `origin/main`. Its deliver script
//! (crew's `packages/crew/src/core/deliver.ts`) committed the work, ran `git rebase origin/main`,
//! hit a conflict in a generated file (`testid-inventory.json`) and stopped with `LIFT-CONFLICT`;
//! the operator resolved it by hand, and the tree that was finally pushed was NOT the tree the
//! repo-checks floor had verified — nothing re-ran the checks after the rebase.
//!
//! # What the engine now does
//!
//! Two halves. [`crate::repo::create_worktree_based`] takes the base from the remote tip when the
//! clone is behind it, so in the common case there is nothing left to lift. This module is the
//! deliver-time half, run OFF the actor thread right before the deliver command spawns:
//!
//! 1. `git fetch origin` (best-effort, bounded) and resolve the remote default ref the deliver
//!    script itself targets (`origin/HEAD`, else `origin/main`).
//! 2. If that tip is already an ancestor of the worktree's `HEAD` ⇒ **unchanged**: the deliver
//!    rebase is a no-op and the verified tree is the tree that ships.
//! 3. If the worktree's `HEAD` is strictly behind the tip (the stale-base shape — the run's work
//!    is UNCOMMITTED on top of an old base) ⇒ compute the lifted tree WITHOUT touching the
//!    worktree: snapshot the content (tracked + untracked-not-ignored, the worktree guard's own
//!    instrument), wrap it in a dangling probe commit on `HEAD`, and `git merge-tree
//!    --write-tree` it onto the tip. A **conflict** leaves the worktree exactly as verified and
//!    fails the deliver unit with a `LIFT-CONFLICT` remedy naming the files — nothing rebased,
//!    nothing pushed. A clean merge is APPLIED (`read-tree --reset -u <lifted>` + `reset --soft
//!    <tip>`): the branch now sits on the tip with the run's changes as its diff, so the deliver
//!    script's own rebase is a no-op.
//! 4. **Lifted** ⇒ the repository's own checks run again on the lifted tree
//!    ([`crate::repo_checks`]). PASS ⇒ the report rides as the deliver unit's evidence and the
//!    push proceeds; FAIL ⇒ the unit fails with a re-verify remedy and the push never runs.
//!
//! A run whose base has commits the remote lacks (the operator's local unpushed work, or a phase
//! that committed) is **skipped**: replaying that history is the deliver rebase's job, and
//! squashing it into the run's commit would ship it under the run's name. Skipped is disclosed
//! (`deliverLiftEvaluated.outcome = "skipped"`, with the reason), never silent.
//!
//! The deliver gate never pushes a tree that was not verified.

use std::path::{Path, PathBuf};
use std::process::Command;

use wicked_apps_core::spawn::HardenedCommand;

use crate::event::CoreEvent;
use crate::worktree_guard::{git, git_string, pinned_git_dir, snapshot_through};

/// The phase id wicked-crew gives the composed deliver phase (`DELIVER_PHASE_ID` in
/// `packages/crew/src/core/deliver.ts`). The engine recognises the deliver unit by it.
pub(crate) const DELIVER_PHASE_ID: &str = "deliver";

/// Whether `unit` is the run's deliver phase: a Tool-executor unit whose phase id is
/// [`DELIVER_PHASE_ID`]. Only that unit is lifted — a `domain-graph` persist tool phase must
/// never have its worktree rebased under it.
pub(crate) fn is_deliver_unit(unit: &crate::domain::WorkUnit) -> bool {
    unit.tool_cmd.is_some() && unit.phase_id() == Some(DELIVER_PHASE_ID)
}

/// What the lift concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiftOutcome {
    /// The base was already the remote tip — nothing to lift, the verified tree ships.
    Unchanged,
    /// The remote moved; the run's changes were re-applied onto its tip in the worktree.
    Lifted,
    /// The lift would conflict; the worktree was left exactly as verified.
    Conflict,
    /// The lift could not be DECIDED (no remote, fetch failed, git too old, a history the
    /// engine does not lift) — the worktree was never touched, so the deliver script's own
    /// rebase stands, as before.
    Skipped,
    /// The lift was decided and its APPLICATION failed part-way (`read-tree`/`reset` refused, or
    /// the post-lift snapshot is neither the verified nor the lifted tree). The worktree may be
    /// in a partial state, so this FAILS the deliver unit — never a silent proceed (Copilot on
    /// #433). The operator inspects the worktree; nothing was pushed.
    Failed,
}

impl LiftOutcome {
    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            LiftOutcome::Unchanged => "unchanged",
            LiftOutcome::Lifted => "lifted",
            LiftOutcome::Conflict => "conflict",
            LiftOutcome::Skipped => "skipped",
            LiftOutcome::Failed => "failed",
        }
    }
}

/// The lift's record — every field the `deliverLiftEvaluated` event carries.
#[derive(Debug, Clone)]
pub(crate) struct LiftReport {
    pub outcome: LiftOutcome,
    pub base_ref: Option<String>,
    pub base_before: Option<String>,
    pub base_after: Option<String>,
    pub tree_before: Option<String>,
    pub tree_after: Option<String>,
    pub conflicts: Vec<String>,
    pub note: Option<String>,
}

impl LiftReport {
    fn skipped(note: impl Into<String>) -> Self {
        LiftReport {
            outcome: LiftOutcome::Skipped,
            base_ref: None,
            base_before: None,
            base_after: None,
            tree_before: None,
            tree_after: None,
            conflicts: Vec::new(),
            note: Some(note.into()),
        }
    }

    pub(crate) fn to_event(&self, session: &str, ord: u32, attempt: u32) -> CoreEvent {
        CoreEvent::DeliverLiftEvaluated {
            session: session.to_string(),
            ord,
            attempt,
            outcome: self.outcome.as_wire().to_string(),
            base_ref: self.base_ref.clone(),
            base_before: self.base_before.clone(),
            base_after: self.base_after.clone(),
            tree_before: self.tree_before.clone(),
            tree_after: self.tree_after.clone(),
            conflicts: self.conflicts.clone(),
            note: self.note.clone(),
        }
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(10)]
}

/// `git fetch origin`, bounded so a stalled network cannot wedge the run: the HTTP transport
/// aborts when it moves under 1 KiB/s for 30 s (git's `http.lowSpeed*`). Best-effort — the
/// caller discloses a failure and works with whatever `origin/*` the clone already has.
pub(crate) fn fetch_origin(cwd: &Path) -> Result<(), String> {
    fetch_origin_env(cwd, &[])
}

fn fetch_origin_env(cwd: &Path, env: &[(&str, &Path)]) -> Result<(), String> {
    git(
        cwd,
        &[
            "-c",
            "http.lowSpeedLimit=1024",
            "-c",
            "http.lowSpeedTime=30",
            "fetch",
            "--quiet",
            "origin",
        ],
        env,
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// The remote default ref the deliver script rebases onto, resolved the same way it does
/// (`git symbolic-ref -q --short refs/remotes/origin/HEAD || echo origin/main`) — plus
/// `origin/master` as a last resort for a clone whose remote never set `HEAD` and has no `main`.
/// `None` when nothing resolves.
pub(crate) fn resolve_remote_default(cwd: &Path) -> Option<String> {
    resolve_remote_default_env(cwd, &[])
}

fn resolve_remote_default_env(cwd: &Path, env: &[(&str, &Path)]) -> Option<String> {
    let resolves = |name: &str| {
        git(
            cwd,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/remotes/{name}^{{commit}}"),
            ],
            env,
        )
        .is_ok()
    };
    // `origin/HEAD` is a symbolic ref; its TARGET can be gone (the remote renamed or deleted its
    // default branch, and `fetch --prune` dropped the tracking ref) — accept it only when it
    // resolves to a commit, else fall through to the fixed candidates (Copilot on #433).
    if let Ok(s) = git_string(
        cwd,
        &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"],
        env,
    ) {
        if !s.is_empty() && resolves(&s) {
            return Some(s);
        }
    }
    ["origin/main", "origin/master"]
        .into_iter()
        .find(|cand| resolves(cand))
        .map(str::to_string)
}

/// `ancestor` is reachable from `descendant` (`git merge-base --is-ancestor`).
fn is_ancestor(cwd: &Path, env: &[(&str, &Path)], ancestor: &str, descendant: &str) -> bool {
    git(
        cwd,
        &["merge-base", "--is-ancestor", ancestor, descendant],
        env,
    )
    .is_ok()
}

/// The in-memory merge: the run's content (`probe`, a dangling commit whose parent is the old
/// base) onto `tip`. `Ok(Ok(tree))` clean; `Ok(Err(conflicted_paths))` conflict; `Err(why)` when
/// `git merge-tree --write-tree` is unavailable or failed outright.
fn merge_tree(
    cwd: &Path,
    env: &[(&str, &Path)],
    tip: &str,
    probe: &str,
) -> Result<Result<String, Vec<String>>, String> {
    // spawn-audit: hardened — git plumbing over the run's own worktree; reads no engine state.
    let mut cmd = Command::new("git");
    cmd.hardened()
        .args([
            "merge-tree",
            "--write-tree",
            "--no-messages",
            "--name-only",
            tip,
            probe,
        ])
        .current_dir(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("could not spawn `git merge-tree`: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines().map(str::trim).filter(|l| !l.is_empty());
    match out.status.code() {
        Some(0) => lines
            .next()
            .map(|t| Ok(t.to_string()))
            .ok_or_else(|| "`git merge-tree --write-tree` printed no tree id".to_string()),
        Some(1) => {
            // First line: the (conflict-marked) tree id; the rest: conflicted file names.
            let _tree = lines.next();
            let mut files: Vec<String> = lines.map(str::to_string).collect();
            files.sort();
            files.dedup();
            Ok(Err(files))
        }
        other => Err(format!(
            "`git merge-tree --write-tree` exited {} (needs git >= 2.38): {}",
            other
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// Lift the worktree's UNCOMMITTED work onto the remote default branch's current tip. Pure
/// decision + application; emits nothing — see [`lift_and_reverify`] for the events and the
/// re-verify. Through the pinned git dir of the REGISTERED repo, like the worktree guard.
pub(crate) fn lift_onto_remote_default(worktree: &Path, repo_root: &Path) -> LiftReport {
    let git_dir = match pinned_git_dir(worktree, repo_root) {
        Ok(d) => d,
        Err(e) => return LiftReport::skipped(format!("could not pin the worktree's git dir: {e}")),
    };
    let env: [(&str, &Path); 2] = [("GIT_DIR", &git_dir), ("GIT_WORK_TREE", worktree)];
    let fetch_note = fetch_origin_env(worktree, &env).err().map(|e| {
        format!("`git fetch origin` failed ({e}); using the clone's existing origin/* refs")
    });
    let Some(base_ref) = resolve_remote_default_env(worktree, &env) else {
        let mut note = "no remote default ref (origin/HEAD, origin/main) to lift onto".to_string();
        if let Some(f) = &fetch_note {
            note.push_str(&format!("; {f}"));
        }
        return LiftReport::skipped(note);
    };
    let head = match git_string(worktree, &["rev-parse", "--verify", "HEAD"], &env) {
        Ok(h) if !h.is_empty() => h,
        _ => return LiftReport::skipped("the worktree has no HEAD to lift"),
    };
    let tip = match git_string(
        worktree,
        &["rev-parse", &format!("{base_ref}^{{commit}}")],
        &env,
    ) {
        Ok(t) if !t.is_empty() => t,
        Err(e) => return LiftReport::skipped(format!("{base_ref} does not resolve: {e}")),
        Ok(_) => return LiftReport::skipped(format!("{base_ref} does not resolve")),
    };
    if is_ancestor(worktree, &env, &tip, &head) {
        return LiftReport {
            outcome: LiftOutcome::Unchanged,
            base_ref: Some(base_ref),
            base_before: Some(head.clone()),
            base_after: Some(tip),
            tree_before: None,
            tree_after: None,
            conflicts: Vec::new(),
            note: fetch_note,
        };
    }
    if !is_ancestor(worktree, &env, &head, &tip) {
        return LiftReport {
            outcome: LiftOutcome::Skipped,
            base_ref: Some(base_ref.clone()),
            base_before: Some(head),
            base_after: Some(tip),
            tree_before: None,
            tree_after: None,
            conflicts: Vec::new(),
            note: Some(format!(
                "the run branch has commits {base_ref} lacks (a phase committed, or the clone \
                 carried local unpushed work) — the engine lifts only uncommitted work on a stale \
                 base; the deliver rebase replays this history{}",
                fetch_note
                    .as_deref()
                    .map(|f| format!("; {f}"))
                    .unwrap_or_default()
            )),
        };
    }
    // Strictly behind: the stale-base shape. Decide the merge in memory first.
    let before = match snapshot_through(worktree, &git_dir) {
        Ok(s) => s,
        Err(e) => return LiftReport::skipped(format!("could not snapshot the worktree: {e}")),
    };
    let identity = Path::new("wicked-core");
    let probe_env: [(&str, &Path); 6] = [
        ("GIT_DIR", &git_dir),
        ("GIT_WORK_TREE", worktree),
        ("GIT_AUTHOR_NAME", identity),
        ("GIT_AUTHOR_EMAIL", identity),
        ("GIT_COMMITTER_NAME", identity),
        ("GIT_COMMITTER_EMAIL", identity),
    ];
    let probe = match git_string(
        worktree,
        &[
            "commit-tree",
            &before.tree,
            "-p",
            &head,
            "-m",
            "wicked-core lift probe",
        ],
        &probe_env,
    ) {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => return LiftReport::skipped("`git commit-tree` printed no id"),
        Err(e) => return LiftReport::skipped(format!("could not build the lift probe: {e}")),
    };
    let merged = match merge_tree(worktree, &env, &tip, &probe) {
        Ok(m) => m,
        Err(why) => {
            return LiftReport {
                outcome: LiftOutcome::Skipped,
                base_ref: Some(base_ref),
                base_before: Some(head),
                base_after: Some(tip),
                tree_before: Some(before.tree),
                tree_after: None,
                conflicts: Vec::new(),
                note: Some(format!("the lift could not be decided: {why}")),
            }
        }
    };
    let lifted_tree = match merged {
        Ok(tree) => tree,
        Err(conflicts) => {
            return LiftReport {
                outcome: LiftOutcome::Conflict,
                base_ref: Some(base_ref),
                base_before: Some(head),
                base_after: Some(tip),
                tree_before: Some(before.tree),
                tree_after: None,
                conflicts,
                note: fetch_note,
            }
        }
    };
    // Apply: content first (index + working tree = the lifted tree, judged against the OLD
    // base), then move the branch pointer under it. Ignored artifacts (node_modules, dist)
    // are in no tree and are untouched.
    //
    // From here on the worktree is being CHANGED: a failure is `Failed` (the deliver unit fails
    // closed and the operator inspects the tree), never `Skipped` (which lets the script run)
    // — Copilot on #433.
    let failed = |tree_after: Option<String>, note: String| LiftReport {
        outcome: LiftOutcome::Failed,
        base_ref: Some(base_ref.clone()),
        base_before: Some(head.clone()),
        base_after: Some(tip.clone()),
        tree_before: Some(before.tree.clone()),
        tree_after,
        conflicts: Vec::new(),
        note: Some(note),
    };
    if let Err(e) = git(
        worktree,
        &["read-tree", "--reset", "-u", &lifted_tree],
        &env,
    ) {
        return failed(
            Some(lifted_tree),
            format!(
                "the lifted tree could not be checked out into the worktree ({e}); the worktree \
                 may hold a partial checkout — inspect it before delivering"
            ),
        );
    }
    if let Err(e) = git(worktree, &["reset", "--soft", &tip], &env) {
        return failed(
            Some(lifted_tree),
            format!(
                "the worktree holds the lifted content but the run branch could not be moved to \
                 the remote tip ({e}); inspect the branch before delivering"
            ),
        );
    }
    // Prove it: the content is what the merge produced, and HEAD is the tip. Anything else is
    // a tree nobody verified — fail closed.
    match snapshot_through(worktree, &git_dir) {
        Ok(after) if after.tree == lifted_tree && after.head == tip => LiftReport {
            outcome: LiftOutcome::Lifted,
            base_ref: Some(base_ref),
            base_before: Some(head),
            base_after: Some(tip),
            tree_before: Some(before.tree),
            tree_after: Some(after.tree),
            conflicts: Vec::new(),
            note: fetch_note,
        },
        Ok(after) => failed(
            Some(after.tree.clone()),
            format!(
                "post-lift check failed: tree {} (the merge produced {}), HEAD {} (the tip is {}) \
                 — the worktree holds a tree nobody verified",
                short(&after.tree),
                short(&lifted_tree),
                short(&after.head),
                short(&tip),
            ),
        ),
        Err(e) => failed(
            Some(lifted_tree),
            format!("post-lift snapshot failed: {e} — the worktree state cannot be proven"),
        ),
    }
}

/// The deliver unit's lift context: the run's worktree and the registered repo it was linked
/// from. `None` for every unit that is not the bound run's deliver phase.
pub(crate) struct LiftContext {
    pub worktree: PathBuf,
    pub repo_root: PathBuf,
}

/// Resolve the [`LiftContext`] for `unit` on the actor thread (store access).
pub(crate) fn lift_context(
    store: &dyn wicked_apps_core::GraphStore,
    session: &crate::domain::AgentSession,
    unit: &crate::domain::WorkUnit,
) -> Option<LiftContext> {
    if !is_deliver_unit(unit) {
        return None;
    }
    let worktree = PathBuf::from(session.workdir.as_deref()?);
    let repo_ref = session.repo_ref.as_deref()?;
    let repo = crate::repo::get_repo(store, repo_ref).ok().flatten()?;
    Some(LiftContext {
        worktree,
        repo_root: PathBuf::from(repo.root_path),
    })
}

/// The env var the deliver command receives with the remote-tip commit the engine verified
/// against (`WICKED_DELIVER_VERIFIED_BASE`, core#431; Copilot on #433). The engine's lift and
/// re-verify happen BEFORE the deliver script's own `fetch` + `rebase` + `push`; if the remote
/// advances in that window the script's rebase would move the base again, past what was
/// verified. The script can close the window itself: after its fetch, refuse (or re-verify) when
/// `origin/<default>` is no longer this commit. Absent when the lift was skipped (no remote /
/// no default ref / a history the engine does not lift).
pub(crate) const VERIFIED_BASE_ENV: &str = "WICKED_DELIVER_VERIFIED_BASE";

/// What the deliver command may proceed with once the lift cleared it.
pub(crate) struct LiftClearance {
    /// The re-verify report when the tree was LIFTED and the checks passed — the unit's evidence.
    pub checks: Option<crate::repo_checks::RepoChecksReport>,
    /// The remote-tip commit the run's work now sits on and was verified against (`unchanged`
    /// or `lifted`); `None` when the lift was skipped.
    pub verified_base: Option<String>,
}

/// Lift + re-verify for the deliver unit, off the actor thread, emitting the record through
/// `emit`. `Ok(clearance)` ⇒ proceed to the command — `checks` is `Some` only when the tree was
/// lifted and the repository's checks PASSED on it, `verified_base` names the tip the script
/// should still see after its own fetch. `Err(text)` ⇒ do NOT run the command — the unit fails
/// with `text` (a conflict, or a failed re-verify; `repoChecksEvaluated` was emitted for the
/// latter).
pub(crate) fn lift_and_reverify(
    ctx: &LiftContext,
    run_id: &str,
    ord: u32,
    attempt: u32,
    emit: &dyn Fn(CoreEvent),
) -> Result<LiftClearance, String> {
    let report = lift_onto_remote_default(&ctx.worktree, &ctx.repo_root);
    emit(report.to_event(run_id, ord, attempt));
    let base_ref = report
        .base_ref
        .as_deref()
        .unwrap_or("the remote default branch");
    let tip = report.base_after.as_deref().map(short).unwrap_or("?");
    match report.outcome {
        LiftOutcome::Unchanged => {
            eprintln!(
                "wicked-core: deliver lift for unit {ord}: unchanged — the run's base {} is \
                 already at {base_ref} ({tip}); the verified tree ships",
                report.base_before.as_deref().map(short).unwrap_or("?")
            );
            Ok(LiftClearance {
                checks: None,
                verified_base: report.base_after.clone(),
            })
        }
        LiftOutcome::Skipped => {
            eprintln!(
                "wicked-core: deliver lift for unit {ord}: skipped — {}",
                report.note.as_deref().unwrap_or("no reason recorded")
            );
            Ok(LiftClearance {
                checks: None,
                verified_base: None,
            })
        }
        LiftOutcome::Failed => Err(format!(
            "deliver: the lift onto {base_ref} ({tip}) could not be applied cleanly — {}. \
             Nothing was pushed — the deliver gate never pushes a tree that was not verified. \
             Inspect the worktree (it may hold a partial checkout), restore or fix it, and \
             approve to retry the deliver phase.",
            report.note.as_deref().unwrap_or("no reason recorded")
        )),
        LiftOutcome::Conflict => Err(format!(
            "deliver: LIFT-CONFLICT — lifting the run's work onto {base_ref} ({tip}) would \
             conflict in: {}. The worktree was left exactly as verified (base {}); nothing was \
             rebased and nothing was pushed. Resolve on the branch (rebase onto {base_ref}, \
             regenerate any generated files, re-run the repository's checks) and approve to \
             retry the deliver phase.",
            report.conflicts.join(", "),
            report.base_before.as_deref().map(short).unwrap_or("?"),
        )),
        LiftOutcome::Lifted => {
            eprintln!(
                "wicked-core: deliver lift for unit {ord}: lifted onto {base_ref} ({tip}), tree \
                 {} → {} — re-running the repository's own checks on the lifted tree (F-039)",
                report.tree_before.as_deref().map(short).unwrap_or("?"),
                report.tree_after.as_deref().map(short).unwrap_or("?"),
            );
            let checks = crate::repo_checks::run(&ctx.worktree);
            eprintln!(
                "wicked-core: deliver lift re-verify for unit {ord}: {} — {}",
                if checks.passed { "PASS" } else { "FAIL" },
                checks.summary()
            );
            if checks.passed {
                Ok(LiftClearance {
                    checks: Some(checks),
                    verified_base: report.base_after.clone(),
                })
            } else {
                emit(CoreEvent::RepoChecksEvaluated {
                    session: run_id.to_string(),
                    ord,
                    attempt,
                    passed: false,
                    criterion: crate::repo_checks::CRITERION.to_string(),
                    checks: checks.checks.clone(),
                    skipped: checks.skipped.clone(),
                });
                Err(format!(
                    "deliver: the run's work was lifted onto {base_ref} ({tip}) but the \
                     repository's own checks FAILED on the lifted tree: {}. Nothing was pushed — \
                     the deliver gate never pushes a tree that was not verified. The worktree now \
                     holds the lifted tree; fix it there (or reject the run) and approve to retry.",
                    checks.summary()
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wicked-lift-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run_git(cwd: &Path, args: &[&str]) -> String {
        // spawn-audit: test-only — a git fixture building the layout under test; reads no engine state.
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn identity(repo: &Path) {
        run_git(repo, &["config", "user.email", "t@example.invalid"]);
        run_git(repo, &["config", "user.name", "t"]);
        run_git(repo, &["config", "commit.gpgsign", "false"]);
        // The Windows runner checks out with `core.autocrlf=true`; the lift goes through git's
        // checkout, so byte-exact LF assertions need the conversion pinned off.
        run_git(repo, &["config", "core.autocrlf", "false"]);
    }

    /// The acceptance layout: a bare `origin`, the operator's clone (the REGISTERED repo, left
    /// stale), and a run worktree linked from it with the creator's work UNCOMMITTED. Returns
    /// `(clone, worktree)`; `origin` is `<base>/origin.git`.
    fn stale_base_layout(tag: &str) -> (PathBuf, PathBuf) {
        let base = scratch(tag);
        let seed = base.join("seed");
        std::fs::create_dir_all(seed.join("src")).unwrap();
        run_git(&seed, &["init", "-q", "-b", "main", "."]);
        identity(&seed);
        std::fs::write(seed.join("src/a.ts"), "export const a = 1;\n").unwrap();
        std::fs::write(seed.join("README.md"), "hello\n").unwrap();
        std::fs::write(seed.join(".gitignore"), "node_modules/\n").unwrap();
        run_git(&seed, &["add", "-A"]);
        run_git(&seed, &["commit", "-qm", "base"]);
        let origin = base.join("origin.git");
        run_git(
            &base,
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                origin.to_str().unwrap(),
            ],
        );
        run_git(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        let clone = base.join("clone");
        run_git(
            &base,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        identity(&clone);
        // The run's worktree, minted from the clone's HEAD — the pre-fix shape.
        let wt = base.join("wt");
        run_git(
            &clone,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                &format!("wicked/{tag}"),
            ],
        );
        std::fs::write(wt.join("src/a.ts"), "export const a = 2; // fixed\n").unwrap();
        std::fs::write(wt.join("src/fix.ts"), "export const fix = true;\n").unwrap();
        (clone, wt)
    }

    /// Land a commit on `origin`'s main from a second clone, so the operator's clone is behind.
    fn land_on_origin(tag: &str, clone: &Path, edit: impl Fn(&Path)) {
        let base = clone.parent().unwrap();
        let other = base.join(format!("other-{tag}"));
        run_git(
            base,
            &[
                "clone",
                "-q",
                base.join("origin.git").to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        identity(&other);
        edit(&other);
        run_git(&other, &["add", "-A"]);
        run_git(
            &other,
            &["commit", "-qm", "landed while the run was queued"],
        );
        run_git(&other, &["push", "-q", "origin", "main"]);
    }

    #[test]
    fn a_run_whose_base_is_the_remote_tip_is_unchanged() {
        let (clone, wt) = stale_base_layout("unchanged");
        let r = lift_onto_remote_default(&wt, &clone);
        assert_eq!(r.outcome, LiftOutcome::Unchanged, "{r:?}");
        assert_eq!(r.base_ref.as_deref(), Some("origin/main"));
        assert_eq!(r.base_before, r.base_after);
        assert!(r.conflicts.is_empty());
        // Nothing touched.
        assert_eq!(
            std::fs::read_to_string(wt.join("src/a.ts")).unwrap(),
            "export const a = 2; // fixed\n"
        );
    }

    /// The F-3R2-013 shape with a NON-conflicting landing: the remote moved (a new file), the run's
    /// uncommitted work sits on the stale base. The lift puts the branch on the remote tip with the
    /// run's changes as its diff — and the landed file is present — without a rebase ever running.
    #[test]
    fn a_stale_base_is_lifted_onto_the_remote_tip_with_the_work_reapplied() {
        let (clone, wt) = stale_base_layout("lift");
        land_on_origin("lift", &clone, |o| {
            std::fs::write(o.join("src/landed.ts"), "export const landed = 1;\n").unwrap();
        });
        let old_head = run_git(&wt, &["rev-parse", "HEAD"]);
        let r = lift_onto_remote_default(&wt, &clone);
        assert_eq!(r.outcome, LiftOutcome::Lifted, "{r:?}");
        assert_eq!(r.base_before.as_deref(), Some(old_head.as_str()));
        let tip = run_git(&wt, &["rev-parse", "origin/main"]);
        assert_eq!(r.base_after.as_deref(), Some(tip.as_str()));
        assert_ne!(r.tree_before, r.tree_after, "the lift changed the tree");
        assert!(
            r.note.is_none(),
            "a clean lift carries no note: {:?}",
            r.note
        );
        // The branch now sits on the remote tip …
        assert_eq!(run_git(&wt, &["rev-parse", "HEAD"]), tip);
        assert_eq!(
            run_git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "wicked/lift",
            "still on the run branch"
        );
        // … with the landed file present, the creator's work intact and staged as the diff.
        assert!(
            wt.join("src/landed.ts").exists(),
            "the remote's landing is in the tree"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("src/a.ts")).unwrap(),
            "export const a = 2; // fixed\n"
        );
        assert!(wt.join("src/fix.ts").exists());
        let staged = run_git(&wt, &["diff", "--cached", "--name-status"]);
        assert!(
            staged.contains("M\tsrc/a.ts") && staged.contains("A\tsrc/fix.ts"),
            "the run's changes are the diff against the new base: {staged}"
        );
        assert!(
            !staged.contains("landed.ts"),
            "the remote's own change is NOT part of the run's diff: {staged}"
        );
        // The deliver script's rebase is now a no-op: the tip is an ancestor of HEAD.
        assert!(is_ancestor(
            &wt,
            &[],
            &tip,
            &run_git(&wt, &["rev-parse", "HEAD"])
        ));
        // Idempotent: a second lift finds nothing to do.
        let again = lift_onto_remote_default(&wt, &clone);
        assert_eq!(again.outcome, LiftOutcome::Unchanged, "{again:?}");
    }

    /// The acceptance run's exact failure: the landing touches the SAME lines the run changed.
    /// The lift must report the conflict by file and leave the worktree exactly as verified —
    /// nothing rebased, no markers, the base untouched.
    #[test]
    fn a_conflicting_landing_is_reported_and_the_worktree_is_left_as_verified() {
        let (clone, wt) = stale_base_layout("conflict");
        land_on_origin("conflict", &clone, |o| {
            std::fs::write(o.join("src/a.ts"), "export const a = 99; // landed\n").unwrap();
        });
        let old_head = run_git(&wt, &["rev-parse", "HEAD"]);
        let before = crate::worktree_guard::snapshot(&wt, &clone).unwrap();
        let r = lift_onto_remote_default(&wt, &clone);
        assert_eq!(r.outcome, LiftOutcome::Conflict, "{r:?}");
        assert_eq!(r.conflicts, vec!["src/a.ts".to_string()]);
        assert_eq!(r.tree_before.as_deref(), Some(before.tree.as_str()));
        assert!(r.tree_after.is_none());
        // Untouched: same HEAD, same content, no conflict markers, still uncommitted.
        assert_eq!(run_git(&wt, &["rev-parse", "HEAD"]), old_head);
        let after = crate::worktree_guard::snapshot(&wt, &clone).unwrap();
        assert_eq!(
            after.tree, before.tree,
            "the verified tree is exactly as it was"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("src/a.ts")).unwrap(),
            "export const a = 2; // fixed\n"
        );
        assert_eq!(run_git(&wt, &["diff", "--cached", "--name-only"]), "");
    }

    /// A run branch that carries its OWN commits (a phase committed) is not the stale-base shape:
    /// the engine skips — disclosed — and the deliver rebase replays that history as before.
    #[test]
    fn a_branch_with_its_own_commits_is_skipped_not_squashed() {
        let (clone, wt) = stale_base_layout("own-commits");
        identity(&wt);
        run_git(&wt, &["add", "-A"]);
        run_git(&wt, &["commit", "-qm", "the creator committed"]);
        land_on_origin("own-commits", &clone, |o| {
            std::fs::write(o.join("src/landed.ts"), "export const landed = 1;\n").unwrap();
        });
        let head = run_git(&wt, &["rev-parse", "HEAD"]);
        let r = lift_onto_remote_default(&wt, &clone);
        assert_eq!(r.outcome, LiftOutcome::Skipped, "{r:?}");
        assert!(
            r.note
                .as_deref()
                .is_some_and(|n| n.contains("commits origin/main lacks")),
            "{:?}",
            r.note
        );
        assert_eq!(run_git(&wt, &["rev-parse", "HEAD"]), head, "nothing moved");
    }

    /// Copilot on #433: `origin/HEAD` pointing at a tracking ref that no longer exists (the remote
    /// renamed its default branch; `fetch --prune` dropped the old ref) must not be accepted just
    /// because `symbolic-ref` succeeded — the `origin/main` fallback is right there.
    #[test]
    fn a_dangling_origin_head_falls_through_to_the_fixed_candidates() {
        let (clone, wt) = stale_base_layout("dangling-head");
        run_git(
            &clone,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/renamed-away",
            ],
        );
        assert_eq!(
            run_git(
                &clone,
                &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"]
            ),
            "origin/renamed-away",
            "premise: origin/HEAD is dangling"
        );
        assert_eq!(
            resolve_remote_default(&clone).as_deref(),
            Some("origin/main"),
            "the dangling symbolic ref is skipped for a candidate that resolves"
        );
        let r = lift_onto_remote_default(&wt, &clone);
        assert_eq!(r.outcome, LiftOutcome::Unchanged, "{r:?}");
        assert_eq!(r.base_ref.as_deref(), Some("origin/main"));
    }

    #[test]
    fn a_repo_without_a_remote_is_skipped() {
        let base = scratch("no-remote");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q", "-b", "main", "."]);
        identity(&repo);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        run_git(&repo, &["add", "-A"]);
        run_git(&repo, &["commit", "-qm", "base"]);
        let wt = base.join("wt");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "wicked/nr",
            ],
        );
        let r = lift_onto_remote_default(&wt, &repo);
        assert_eq!(r.outcome, LiftOutcome::Skipped, "{r:?}");
        assert!(r
            .note
            .as_deref()
            .is_some_and(|n| n.contains("no remote default ref")));
    }

    #[test]
    fn only_the_deliver_tool_unit_is_lifted() {
        let mut deliver = crate::domain::WorkUnit::pending("s:deliver", "s", 5, "deliver");
        deliver.tool_cmd = Some(vec!["bash".into(), "-lc".into(), "true".into()]);
        assert!(is_deliver_unit(&deliver));
        let mut other_tool = crate::domain::WorkUnit::pending("s:domain-graph", "s", 5, "x");
        other_tool.tool_cmd = Some(vec!["wicked-core".into()]);
        assert!(
            !is_deliver_unit(&other_tool),
            "a persist tool phase is never rebased"
        );
        let agent = crate::domain::WorkUnit::pending("s:deliver", "s", 5, "deliver");
        assert!(agent.tool_cmd.is_none() && !is_deliver_unit(&agent));
    }
}
