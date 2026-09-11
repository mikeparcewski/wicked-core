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
use std::time::{Duration, Instant};

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
    /// The remote tip is already an ancestor of `HEAD` — the run's base is current (HEAD may
    /// carry the run's own commits on top; the deliver rebase is a no-op) — nothing to lift.
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

/// Hard wall-clock bound on `git fetch origin` (F-433-004): the fetch sits on the run's critical
/// path (worktree mint) and on the deliver unit; a remote that hangs must degrade to a disclosed
/// "fetch failed", never wedge the run.
pub(crate) const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

/// `git fetch origin`, NON-INTERACTIVE and BOUNDED (F-433-004): no credential prompt
/// (`core.askPass=` + `GIT_TERMINAL_PROMPT=0`), no host-key/passphrase prompt
/// (`ssh -oBatchMode=yes` unless the operator set their own `GIT_SSH_COMMAND`), the HTTP transport
/// aborts under 1 KiB/s for 30 s, and the whole command is killed at [`FETCH_TIMEOUT`]. Best-effort
/// — the caller discloses a failure; a daemon started from a terminal keeps its controlling tty, so
/// without this a missing credential helper would block `resolve_run_base` and the run would never
/// reach `WorktreeReady`.
pub(crate) fn fetch_origin(cwd: &Path) -> Result<(), String> {
    fetch_origin_env(cwd, &[])
}

fn fetch_origin_env(cwd: &Path, env: &[(&str, &Path)]) -> Result<(), String> {
    // spawn-audit: hardened — git plumbing over the run's own worktree; reads no engine state.
    let mut cmd = Command::new("git");
    cmd.hardened()
        .args([
            "-c",
            "core.askPass=",
            "-c",
            "http.lowSpeedLimit=1024",
            "-c",
            "http.lowSpeedTime=30",
            "fetch",
            "--quiet",
            "origin",
        ])
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        cmd.env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes");
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not spawn `git fetch origin`: {e}"))?;
    let stderr = child.stderr.take();
    let deadline = Instant::now() + FETCH_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "`git fetch origin` exceeded {}s and was killed",
                    FETCH_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("`git fetch origin` could not be waited on: {e}")),
        }
    };
    if status.success() {
        return Ok(());
    }
    let mut err = String::new();
    if let Some(mut s) = stderr {
        use std::io::Read;
        let _ = s.read_to_string(&mut err);
    }
    Err(format!(
        "`git fetch origin` failed ({status}): {}",
        err.trim().lines().last().unwrap_or("").trim()
    ))
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
    // No `origin` at all ⇒ nothing to lift onto (the deliver script itself would fail its
    // `git fetch origin`); say that, not "fetch failed".
    if git(worktree, &["remote", "get-url", "origin"], &env).is_err() {
        return LiftReport::skipped("no `origin` remote — nothing to lift onto");
    }
    // A failed fetch means the cached `origin/*` refs are NOT proof of the current tip: an
    // `unchanged` verdict over them would let the script's own fetch + rebase ship a tree nobody
    // verified (Copilot on #433, third pass). Skip — disclosed — and hand the script no
    // verified base; the worktree is untouched.
    if let Err(e) = fetch_origin_env(worktree, &env) {
        return LiftReport::skipped(format!(
            "`git fetch origin` failed ({e}) — the cached origin/* refs cannot stand in for the \
             current tip, so the lift was not decided; the deliver script's own fetch and rebase \
             stand, and no verified base is reported"
        ));
    }
    let Some(base_ref) = resolve_remote_default_env(worktree, &env) else {
        return LiftReport::skipped(
            "no remote default ref (origin/HEAD, origin/main) to lift onto",
        );
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
            note: None,
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
                 base; the deliver rebase replays this history"
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
                note: None,
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
            note: None,
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
    /// The tree id the run last VERIFIED (the verify unit's guard after-tree once its checks
    /// passed, or a previous deliver re-verify) — persisted on the session (F-433-001). `None`
    /// when nothing was recorded: every deliver then re-verifies.
    pub verified_tree: Option<String>,
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
        verified_tree: session.verified_tree.clone(),
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

/// Is the worktree's `HEAD` attached to THIS run's branch? Accepts the engine's spelling
/// (`refs/heads/wicked/<sanitized run id>`, `repo::worktree_branch`) and the raw-id spelling a
/// downstream may have pre-provisioned (crew#390/#391). A detached `HEAD`, or any other ref, is an
/// `Err` naming what was found.
fn require_run_branch(worktree: &Path, repo_root: &Path, run_id: &str) -> Result<(), String> {
    let git_dir = pinned_git_dir(worktree, repo_root)
        .map_err(|e| format!("could not pin the worktree's git dir: {e}"))?;
    let env: [(&str, &Path); 2] = [("GIT_DIR", &git_dir), ("GIT_WORK_TREE", worktree)];
    let head_ref = git_string(worktree, &["symbolic-ref", "-q", "HEAD"], &env)
        .ok()
        .filter(|r| !r.is_empty());
    let expected = [
        format!("refs/heads/{}", crate::repo::worktree_branch(run_id)),
        format!("refs/heads/wicked/{run_id}"),
    ];
    match head_ref {
        Some(r) if expected.contains(&r) => Ok(()),
        Some(r) => Err(format!(
            "the worktree's HEAD is attached to `{r}`, not the run branch `{}`",
            crate::repo::worktree_branch(run_id)
        )),
        None => Err(format!(
            "the worktree's HEAD is detached, not on the run branch `{}`",
            crate::repo::worktree_branch(run_id)
        )),
    }
}

/// What the deliver command may proceed with once the lift cleared it.
pub(crate) struct LiftClearance {
    /// The re-verify report when the repository's checks RAN on the current tree and passed —
    /// the unit's evidence. `None` when the current tree IS the recorded verified tree.
    pub checks: Option<crate::repo_checks::RepoChecksReport>,
    /// The remote-tip commit the run's work now sits on and was verified against (`unchanged`
    /// or `lifted`); `None` when the lift was skipped.
    pub verified_base: Option<String>,
    /// The tree id that is now VERIFIED — the one the command may ship. Persisted on the
    /// session by the fold so the next deliver attempt can tell whether it changed (F-433-001).
    pub verified_tree: String,
}

/// Lockfiles and manifests whose movement between the old base and the tip means the installed
/// dependencies are stale (F-433-003): the checks must re-install (frozen, `--ignore-scripts`)
/// before they can say anything about the lifted tree — the acceptance run's `tsc` failed on
/// `api-types 0.25.0` installed vs `0.30.0` pinned, and the remedy blamed the tree.
const LOCKFILE_PATHS: [&str; 6] = [
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "Cargo.lock",
];

/// The lockfiles/manifests that differ between `from` and `to` (a subset of [`LOCKFILE_PATHS`]).
fn lockfile_drift(worktree: &Path, git_dir: &Path, from: &str, to: &str) -> Vec<String> {
    let env: [(&str, &Path); 2] = [("GIT_DIR", git_dir), ("GIT_WORK_TREE", worktree)];
    let mut args: Vec<&str> = vec!["diff", "--name-only", from, to, "--"];
    args.extend(LOCKFILE_PATHS);
    match git_string(worktree, &args, &env) {
        Ok(out) => out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Lift + re-verify for the deliver unit, off the actor thread, emitting the record through
/// `emit`. THE RULE (F-433-001): whatever the lift concluded, the command may run only when the
/// worktree's CURRENT tree is a tree the repository's own checks have certified — the tree the
/// session recorded as verified, or one the checks pass on right here. `Ok(clearance)` ⇒
/// proceed — `checks` is `Some` when the checks ran now, `verified_base` names the tip the script
/// should still see after its own fetch, `verified_tree` is what the fold persists. `Err(text)`
/// ⇒ do NOT run the command — the unit fails with `text` (a conflict, an apply failure, a failed
/// re-verify, or checks that changed the tree; `repoChecksEvaluated {passed: false}` is emitted
/// for the last two). A retry after a failed re-verify (the branch already on the tip ⇒
/// `unchanged`), an operator's by-hand rebase, or a run resumed onto an edited worktree all land
/// here with a tree ≠ the verified one — and are re-checked, not waved through.
pub(crate) fn lift_and_reverify(
    ctx: &LiftContext,
    run_id: &str,
    ord: u32,
    attempt: u32,
    emit: &dyn Fn(CoreEvent),
) -> Result<LiftClearance, String> {
    // The worktree must be ON THE RUN BRANCH before anything is lifted, reset or cleared
    // (Copilot on #433, fifth pass): a detached HEAD, or another branch switched in by hand at a
    // commit behind the tip, would pass the ancestry test and `reset --soft <tip>` would move
    // THAT branch — or the no-check fast path would clear a tree on the wrong ref. Identify the
    // run by its ref, not by the commit it happens to be at.
    if let Err(why) = require_run_branch(&ctx.worktree, &ctx.repo_root, run_id) {
        return Err(format!(
            "deliver: {why} — nothing was lifted, reset or pushed; the deliver script only pushes \
             the run branch. Switch the worktree back to `{}` and approve to retry.",
            crate::repo::worktree_branch(run_id)
        ));
    }
    let report = lift_onto_remote_default(&ctx.worktree, &ctx.repo_root);
    emit(report.to_event(run_id, ord, attempt));
    let base_ref = report
        .base_ref
        .as_deref()
        .unwrap_or("the remote default branch");
    let tip = report.base_after.as_deref().map(short).unwrap_or("?");
    match report.outcome {
        LiftOutcome::Failed => {
            return Err(format!(
                "deliver: the lift onto {base_ref} ({tip}) could not be applied cleanly — {}. \
                 Nothing was pushed — the deliver gate never pushes a tree that was not verified. \
                 Inspect the worktree (it may hold a partial checkout), restore or fix it, and \
                 approve to retry the deliver phase.",
                report.note.as_deref().unwrap_or("no reason recorded")
            ))
        }
        LiftOutcome::Conflict => {
            return Err(format!(
                "deliver: LIFT-CONFLICT — lifting the run's work onto {base_ref} ({tip}) would \
                 conflict in: {}. The worktree was left exactly as verified (base {}); nothing was \
                 rebased and nothing was pushed. Resolve on the branch (rebase onto {base_ref}, \
                 regenerate any generated files, re-run the repository's checks) and approve to \
                 retry the deliver phase.",
                report.conflicts.join(", "),
                report.base_before.as_deref().map(short).unwrap_or("?"),
            ))
        }
        LiftOutcome::Unchanged => eprintln!(
            "wicked-core: deliver lift for unit {ord}: unchanged — the run's base {} is already \
             at {base_ref} ({tip})",
            report.base_before.as_deref().map(short).unwrap_or("?")
        ),
        LiftOutcome::Skipped => eprintln!(
            "wicked-core: deliver lift for unit {ord}: skipped — {}",
            report.note.as_deref().unwrap_or("no reason recorded")
        ),
        LiftOutcome::Lifted => eprintln!(
            "wicked-core: deliver lift for unit {ord}: lifted onto {base_ref} ({tip}), tree {} → {}",
            report.tree_before.as_deref().map(short).unwrap_or("?"),
            report.tree_after.as_deref().map(short).unwrap_or("?"),
        ),
    }
    // The tree that would ship, as it stands NOW — whatever the lift did or did not do.
    let now = crate::worktree_guard::snapshot(&ctx.worktree, &ctx.repo_root).map_err(|e| {
        format!(
            "deliver: the worktree could not be snapshotted before delivery ({e}); nothing was \
             pushed — the deliver gate never pushes a tree it cannot identify."
        )
    })?;
    let verified_base = match report.outcome {
        LiftOutcome::Unchanged | LiftOutcome::Lifted => report.base_after.clone(),
        _ => None,
    };
    if ctx.verified_tree.as_deref() == Some(now.tree.as_str()) {
        eprintln!(
            "wicked-core: deliver for unit {ord}: the worktree tree {} is the tree the run \
             verified — the checks need not run again",
            short(&now.tree)
        );
        return Ok(LiftClearance {
            checks: None,
            verified_base,
            verified_tree: now.tree,
        });
    }
    // Not the verified tree ⇒ RE-VERIFY, whatever moved it (a lift, a failed earlier re-verify's
    // leftover, an operator's by-hand rebase, an edit after verify, a run with no verify phase).
    let why = match (&ctx.verified_tree, report.outcome) {
        (_, LiftOutcome::Lifted) => "the lift changed the tree".to_string(),
        (Some(v), _) => format!(
            "the worktree's tree {} is not the tree the run verified ({})",
            short(&now.tree),
            short(v)
        ),
        (None, _) => "the run recorded no verified tree".to_string(),
    };
    // F-433-003: a lockfile that moved with the base means the installed dependencies are
    // stale — force a frozen, scripts-off install ahead of the checks and NAME the drift.
    let drift = match (&report.base_before, &report.base_after) {
        (Some(from), Some(to)) if report.outcome == LiftOutcome::Lifted => {
            match pinned_git_dir(&ctx.worktree, &ctx.repo_root) {
                Ok(git_dir) => lockfile_drift(&ctx.worktree, &git_dir, from, to),
                Err(_) => Vec::new(),
            }
        }
        _ => Vec::new(),
    };
    let drift_note = if drift.is_empty() {
        String::new()
    } else {
        format!(
            " Lockfile drift between the old base and the tip ({}): dependencies were \
             re-installed (frozen lockfile, --ignore-scripts) before the checks.",
            drift.join(", ")
        )
    };
    eprintln!(
        "wicked-core: deliver re-verify for unit {ord}: {why}{} — running the repository's own \
         checks on tree {} (F-039 / F-433-001)",
        if drift.is_empty() {
            String::new()
        } else {
            format!("; lockfile drift in {} ⇒ forced install", drift.join(", "))
        },
        short(&now.tree)
    );
    let checks = crate::repo_checks::run_forcing_install(&ctx.worktree, !drift.is_empty());
    eprintln!(
        "wicked-core: deliver re-verify for unit {ord}: {} — {}",
        if checks.passed { "PASS" } else { "FAIL" },
        checks.summary()
    );
    let refuse = |checks: &crate::repo_checks::RepoChecksReport, text: String| {
        emit(CoreEvent::RepoChecksEvaluated {
            session: run_id.to_string(),
            ord,
            attempt,
            passed: false,
            criterion: crate::repo_checks::CRITERION.to_string(),
            checks: checks.checks.clone(),
            skipped: checks.skipped.clone(),
            sandbox_level: checks.sandbox_level.clone(),
            sandbox_error: checks.sandbox_error.clone(),
            detect_error: checks.detect_error.clone(),
        });
        Err(text)
    };
    if !checks.passed {
        return refuse(
            &checks,
            format!(
                "deliver: {why}, and the repository's own checks FAILED on it: {}.{drift_note} \
                 Nothing was pushed — the deliver gate never pushes a tree that was not verified. \
                 Fix the worktree (or reject the run) and approve to retry; the checks run again \
                 until the tree passes.",
                checks.summary()
            ),
        );
    }
    // F-433-002: the checks are REPOSITORY-controlled code — a "passing" script can edit a
    // tracked file or move HEAD, and the Tool path has no final worktree guard. Prove the tree
    // the checks certified is the tree that ships, or fail closed.
    // A failed proof is a refusal WITH the `passed: false` evidence (Copilot, fourth pass) — a
    // consumer must be able to tell "the proof failed" from "the checks never ran".
    let after = match crate::worktree_guard::snapshot(&ctx.worktree, &ctx.repo_root) {
        Ok(a) => a,
        Err(e) => {
            return refuse(
                &checks,
                format!(
                    "deliver: the repository's checks passed but the worktree could not be \
                     re-snapshotted afterwards ({e}); nothing was pushed — the deliver gate never \
                     pushes a tree it cannot prove."
                ),
            )
        }
    };
    // Tree, commit AND the ref HEAD is attached to (Copilot, fourth pass): a check that detaches
    // HEAD or switches branches at the same tip would otherwise pass both comparisons and the
    // deliver script would push from the wrong ref.
    if after.tree != now.tree || after.head != now.head || after.head_ref != now.head_ref {
        return refuse(
            &checks,
            format!(
                "deliver: the repository's checks passed but CHANGED the worktree while running \
                 (tree {} → {}, HEAD {} → {}, HEAD ref {:?} → {:?}) — a check script that edits \
                 tracked files, moves HEAD or switches the branch leaves a tree nobody verified. \
                 Nothing was pushed. Inspect the worktree, fix or ignore the check's writes, and \
                 approve to retry.",
                short(&now.tree),
                short(&after.tree),
                short(&now.head),
                short(&after.head),
                now.head_ref,
                after.head_ref,
            ),
        );
    }
    Ok(LiftClearance {
        checks: Some(checks),
        verified_base,
        verified_tree: after.tree,
    })
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
                "-c",
                "core.autocrlf=false",
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
                "-c",
                "core.autocrlf=false",
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
                "-c",
                "core.autocrlf=false",
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
        assert!(
            r.note
                .as_deref()
                .is_some_and(|n| n.contains("no `origin` remote")),
            "{:?}",
            r.note
        );
    }

    /// Copilot on #433 (third pass): cached `origin/*` refs are not proof of the current tip. A
    /// remote that cannot be fetched (here: the bare origin was deleted after the clone) makes
    /// the lift SKIP before any ancestry test — the worktree untouched, no verified base.
    #[test]
    fn a_failed_fetch_skips_the_lift_instead_of_trusting_cached_refs() {
        let (clone, wt) = stale_base_layout("fetch-fails");
        std::fs::remove_dir_all(clone.parent().unwrap().join("origin.git")).unwrap();
        let head = run_git(&wt, &["rev-parse", "HEAD"]);
        let r = lift_onto_remote_default(&wt, &clone);
        assert_eq!(r.outcome, LiftOutcome::Skipped, "{r:?}");
        assert!(
            r.note
                .as_deref()
                .is_some_and(|n| n.contains("`git fetch origin` failed")
                    && n.contains("no verified base is reported")),
            "{:?}",
            r.note
        );
        assert_eq!(run_git(&wt, &["rev-parse", "HEAD"]), head, "nothing moved");
    }

    /// A worktree with a `test` script that FAILS (`exit 1`), `node_modules/` present so the
    /// floor installs nothing — the repository's own check, always red.
    fn add_failing_npm_check(repo: &Path) {
        std::fs::write(
            repo.join("package.json"),
            r#"{"name":"lift-reverify","version":"0.0.0","scripts":{"test":"exit 1"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(repo.join("node_modules")).unwrap();
        std::fs::write(repo.join("node_modules/.keep"), "").unwrap();
        run_git(repo, &["add", "-A"]);
        run_git(repo, &["commit", "-qm", "failing check"]);
        run_git(repo, &["push", "-q", "origin", "main"]);
    }

    fn ctx_for(clone: &Path, wt: &Path, verified_tree: Option<String>) -> LiftContext {
        LiftContext {
            worktree: wt.to_path_buf(),
            repo_root: clone.to_path_buf(),
            verified_tree,
        }
    }

    /// F-433-001, the load-bearing rule: a lift whose re-verify FAILS leaves the worktree on the
    /// tip with the lifted content; the retry finds nothing to lift (`unchanged`) — and must
    /// still re-run the checks, because the tree is not the one the run verified, and refuse
    /// again. Before this rule the retry pushed the tree that had just failed the checks. Needs
    /// `npm` on PATH; on a host with no OS sandbox the floor fails closed (also a refusal) — either
    /// way the checks are consulted TWICE and the command never gets clearance.
    #[test]
    fn a_failed_reverify_is_re_run_on_retry_and_refuses() {
        // spawn-audit: test-only — probes for `npm` on PATH to decide whether the fixture can run; reads no engine state.
        if Command::new("npm").arg("--version").output().is_err() {
            eprintln!("npm not on PATH — the re-verify test cannot run here");
            return;
        }
        let (clone, wt) = stale_base_layout("reverify-retry");
        // The seed repo had no check; the clone's checkout is the stale base. Land a failing
        // check + a non-conflicting file on origin, so the lift changes the tree.
        land_on_origin("reverify-retry", &clone, |o| {
            std::fs::write(o.join("src/landed.ts"), "export const landed = 1;\n").unwrap();
        });
        let other = clone.parent().unwrap().join("other-reverify-retry");
        add_failing_npm_check(&other);
        // The run's verify unit certified the PRE-lift tree.
        let verified = crate::worktree_guard::snapshot(&wt, &clone).unwrap().tree;
        let ctx = ctx_for(&clone, &wt, Some(verified.clone()));
        let events = std::cell::RefCell::new(Vec::new());
        let emit = |ev: CoreEvent| events.borrow_mut().push(ev);

        // Attempt 0: lifted → checks run → refuse.
        let first = lift_and_reverify(&ctx, "reverify-retry", 5, 0, &emit);
        let err = first.err().expect("a failing check refuses the deliver");
        assert!(err.contains("Nothing was pushed"), "{err}");
        let checks_evaluated = |evs: &[CoreEvent]| {
            evs.iter()
                .filter(|e| matches!(e, CoreEvent::RepoChecksEvaluated { passed: false, .. }))
                .count()
        };
        assert_eq!(
            checks_evaluated(&events.borrow()),
            1,
            "the checks were consulted"
        );
        let lifted = events.borrow().iter().any(
            |e| matches!(e, CoreEvent::DeliverLiftEvaluated { outcome, .. } if outcome == "lifted"),
        );
        assert!(lifted, "{:?}", events.borrow().len());
        let tip = run_git(&wt, &["rev-parse", "origin/main"]);
        assert_eq!(
            run_git(&wt, &["rev-parse", "HEAD"]),
            tip,
            "the worktree sits on the tip"
        );

        // Attempt 1 (the operator approved a retry): nothing to lift — and the tree is NOT the
        // verified one, so the checks run again and refuse again.
        let second = lift_and_reverify(&ctx, "reverify-retry", 5, 1, &emit);
        let err = second.err().expect("the retry must not be waved through");
        assert!(
            err.contains("is not the tree the run verified") || err.contains("FAILED"),
            "{err}"
        );
        let unchanged = events
            .borrow()
            .iter()
            .any(|e| matches!(e, CoreEvent::DeliverLiftEvaluated { attempt: 1, outcome, .. } if outcome == "unchanged"));
        assert!(unchanged, "the retry found nothing to lift");
        assert_eq!(
            checks_evaluated(&events.borrow()),
            2,
            "the checks were consulted AGAIN on the retry"
        );
    }

    /// Copilot on #433 (fifth pass): the lift identifies the run by its BRANCH, not the commit.
    /// A worktree switched to another branch (or detached) at a commit behind the tip must be
    /// refused before anything is lifted or reset — else `reset --soft <tip>` would move that
    /// other branch — and the refusal names what HEAD is on.
    #[test]
    fn a_worktree_not_on_the_run_branch_is_refused_before_any_lift() {
        let (clone, wt) = stale_base_layout("wrong-branch");
        land_on_origin("wrong-branch", &clone, |o| {
            std::fs::write(o.join("src/landed.ts"), "export const landed = 1;\n").unwrap();
        });
        let head = run_git(&wt, &["rev-parse", "HEAD"]);
        let verified = crate::worktree_guard::snapshot(&wt, &clone).unwrap().tree;
        // The layout's run id is "wrong-branch" (branch `wicked/wrong-branch`); a different run
        // id ⇒ HEAD is on the wrong ref for THAT run.
        let ctx = ctx_for(&clone, &wt, Some(verified.clone()));
        let emit = |_: CoreEvent| {};
        let err = lift_and_reverify(&ctx, "some-other-run", 5, 0, &emit)
            .err()
            .expect("another run's branch is refused");
        assert!(
            err.contains("attached to `refs/heads/wicked/wrong-branch`")
                && err.contains("nothing was lifted, reset or pushed"),
            "{err}"
        );
        assert_eq!(
            run_git(&wt, &["rev-parse", "HEAD"]),
            head,
            "the branch was not moved"
        );
        // Detached at the same commit: refused too, even though the tree IS the verified tree.
        run_git(&wt, &["checkout", "-q", "--detach"]);
        let err = lift_and_reverify(&ctx, "wrong-branch", 5, 0, &emit)
            .err()
            .expect("a detached HEAD is refused");
        assert!(err.contains("detached"), "{err}");
        // Back on the run branch, the same run id is accepted (and lifts).
        run_git(&wt, &["checkout", "-q", "wicked/wrong-branch"]);
        match lift_and_reverify(&ctx, "wrong-branch", 5, 0, &emit) {
            Ok(_) => {}
            Err(e) => assert!(
                e.contains("checks"),
                "on the run branch the lift proceeds to the checks: {e}"
            ),
        }
    }

    /// The fast path: the worktree's tree IS the tree the run verified — no checks, clearance.
    #[test]
    fn a_tree_that_matches_the_verified_tree_needs_no_checks() {
        let (clone, wt) = stale_base_layout("verified-match");
        let verified = crate::worktree_guard::snapshot(&wt, &clone).unwrap().tree;
        let ctx = ctx_for(&clone, &wt, Some(verified.clone()));
        let emit = |_: CoreEvent| {};
        let c = lift_and_reverify(&ctx, "verified-match", 5, 0, &emit).expect("cleared");
        assert!(c.checks.is_none(), "no checks ran on the verified tree");
        assert_eq!(c.verified_tree, verified);
        assert_eq!(
            c.verified_base.as_deref(),
            Some(run_git(&wt, &["rev-parse", "origin/main"]).as_str())
        );
    }

    /// `unchanged` is not a pass: an operator's edit after verify (or a by-hand rebase) leaves a
    /// tree ≠ the verified one with nothing to lift — the checks run anyway. Here the repo has
    /// no detectable check, so the floor reports `checks: []`… and on a host with no OS sandbox
    /// it refuses instead — both are "the checks were consulted", never a silent proceed.
    #[test]
    fn an_unchanged_lift_over_a_tree_that_is_not_verified_still_consults_the_checks() {
        let (clone, wt) = stale_base_layout("verified-mismatch");
        let ctx = ctx_for(
            &clone,
            &wt,
            Some("0000000000000000000000000000000000000000".into()),
        );
        let events = std::cell::RefCell::new(Vec::new());
        let emit = |ev: CoreEvent| events.borrow_mut().push(ev);
        let result = lift_and_reverify(&ctx, "verified-mismatch", 5, 0, &emit);
        match result {
            Ok(c) => assert!(
                c.checks.is_some(),
                "unchanged + unverified tree ⇒ the checks ran (and passed vacuously)"
            ),
            Err(e) => assert!(
                e.contains("is not the tree the run verified"),
                "a refusal names why the checks ran: {e}"
            ),
        }
        let unchanged = events
            .borrow()
            .iter()
            .any(|e| matches!(e, CoreEvent::DeliverLiftEvaluated { outcome, .. } if outcome == "unchanged"));
        assert!(unchanged);
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
