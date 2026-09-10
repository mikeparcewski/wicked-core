//! WORKTREE GUARD — evaluator ≠ creator held STRUCTURALLY, for every seat (F-036).
//!
//! ## The defect this closes
//!
//! A governed `bug` run's `verify` phase (role `evaluator`, `executes_code: false`) landed on a
//! codex seat. Governance is claude-only, so the seat's tool calls were unchecked, and the
//! evaluator REWROTE the fix it was reviewing — removed the `data-testid` hooks, changed the e2e
//! locator, re-filtered a list — then reported "Implemented issue #219" and PASSED its own gate,
//! because the gate's only criterion was "the run left a change in its worktree". Structurally,
//! the evaluator self-graded its own edit. Nothing in the engine could tell.
//!
//! Prompt discipline had already failed twice (core#283/#296), and a per-CLI write posture is only
//! as good as each CLI's own lever. So this is a CONTENT check the engine owns end to end:
//!
//! 1. At dispatch of any unit whose phase declared `executes_code: false` (a def-driven,
//!    agent-executed unit of a BOUND run), the actor takes a [`WorktreeSnapshot`] — the git TREE
//!    HASH of the worktree as it stands (tracked + untracked-but-not-ignored content) plus `HEAD`
//!    — and persists it ON THE UNIT so it survives a daemon restart.
//! 2. When the unit's work is COMPLETELY over — the seat's process tree quiesced, the agent judge
//!    rendered, the repo checks run — the worker thread takes the FINAL snapshot immediately
//!    before the result is posted to the gate fold and [`compare`]s it with the baseline. Any
//!    non-exempt path that differs, or a moved `HEAD`, is a [`WorktreeMutation`] — the fold
//!    DENIES the unit (deny-dominates, source `worktree_guard`) and emits
//!    `evaluatorMutatedWorktree` naming every path. A comparison taken any earlier would miss a
//!    write that lands after the seat returns (a backgrounded process, a "passing" check script
//!    that edits a tracked file) and still fold `combined: true` over it.
//!
//! Independent of the CLI, its governance adapter, and its sandbox: the same git plumbing judges a
//! claude, codex, pi or copilot seat, on the wrapped or the ACP carrier. A seat's read-only
//! posture (where one exists — `execute_wrapped::no_code_posture`) PREVENTS most writes up front;
//! this guard is what makes the rule hold when the posture is absent, bypassed or wrong.
//!
//! ## How the snapshot is taken without touching the worker's state
//!
//! `git write-tree` hashes an INDEX, so the content hash is computed over a TEMPORARY copy of the
//! worktree's index (`GIT_INDEX_FILE`): `git add -A` into the copy (respecting `.gitignore`, so
//! `node_modules/`, `target/`, `dist/` never count), then `write-tree`. The real index is never
//! read-modified, no ref moves, and the only side effect is a few dangling objects in the shared
//! object store (gc'd later). Copying the real index first makes `add -A` a stat-cache walk rather
//! than a full re-hash, so a snapshot is cheap even on a large tree. The diff between two
//! snapshots is `git diff-tree -r --name-status` over the two tree objects — a pure object read.
//!
//! ## What the snapshot does not see: the engine's own scratch
//!
//! The engine points every worker's `TMPDIR` at `<worktree>/tmp` (core#264, crew#427 — so a
//! bounded seat's temp files land inside its own write boundary), and the repo checks' isolated
//! `HOME`/package caches live there too. That directory is the engine's, not the work's, and the
//! snapshot excludes it BY CONSTRUCTION — through git's own exclude mechanism
//! (`core.excludesFile` for the scratch `add -A`), exactly like a `.gitignore`d build output —
//! rather than by filtering paths out of a diff. Nothing else is excluded.
//!
//! ## The snapshot trusts nothing inside the worktree
//!
//! The snapshot is taken THROUGH the git directory pinned from the REGISTERED repository at
//! dispatch ([`pinned_git_dir`]: `<repo>/.git/worktrees/<id>`, found from the repo side), never
//! through the worktree's own `.git` file, and its scratch index starts EMPTY — the real index is
//! never copied, so no `assume-unchanged` bit can tell `add -A` to skip a rewritten file. Both were
//! reproduced escapes (adversarial review on #414): a redirected `.git` file made both snapshots
//! describe a repository the evaluator controlled, and an inherited `assume-unchanged` bit made a
//! rewrite hash as its cached original. Full re-hash, pinned directory — on purpose.
//!
//! ## No exemptions
//!
//! Every path that differs between the two snapshots denies — documentation, a report, a declared
//! deliverable, an in-tree tool database, anything (codex review on #414). An `executes_code:
//! false` phase's outputs belong OUTSIDE the tree it is judging: its verdict is its work output,
//! and a phase that must leave a file in the tree declares `executes_code: true` (the shipped
//! `domain-extraction/coverage`, which writes `coverage-report.json` at the worktree root for its
//! pinned validator, does exactly that). An in-tree `.codegraph/` that moves under a recon phase
//! is a defect to surface (core#406), not to hide.
//!
//! ## Honest limits
//!
//! * A phase that legitimately must change code declares `executes_code: true` — the guard reads
//!   the def, it does not guess. Prose-planned (def-less) runs carry no declaration and are not
//!   guarded.
//! * The guard DENIES; it does not revert. The creator's tree survives as the `before` tree
//!   object, and the denial names the one-line restore. Auto-revert is a deliberate non-goal for
//!   this change: discarding files an operator may still want to inspect is a bigger decision
//!   than refusing to certify them.
//! * A process that escapes the seat's process group (a `setsid` daemon) can still write after
//!   the final snapshot. The wrapped runner kills the seat's whole group when the seat exits,
//!   and the final snapshot is taken as late as the pipeline allows; what lands after that is
//!   the next phase's baseline problem, and its gate.
//! * A human who APPROVES a mutation-denied gate accepts the tree as it stands: the re-dispatch
//!   re-baselines. A restart-driven re-dispatch never does — the persisted baseline is kept.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use wicked_apps_core::spawn::HardenedCommand;

/// The content identity of a worktree at one instant: its `HEAD` and the tree hash of everything
/// git would commit (`add -A`, so `.gitignore` applies; the engine scratch excluded). Persisted on
/// the unit at dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeSnapshot {
    /// `HEAD`'s commit id; empty on an unborn branch.
    pub head: String,
    /// The tree object id over tracked + untracked-not-ignored content.
    pub tree: String,
    /// Wall-clock millis when taken (informational).
    pub taken_at_ms: u64,
    /// The git directory the snapshot was taken THROUGH, pinned from the REGISTERED repo
    /// (`<repo>/.git` for the main checkout, `<repo>/.git/worktrees/<id>` for a linked worktree) —
    /// never discovered through the worktree's own `.git` file, which the evaluator can rewrite
    /// (adversarial review on #414). The final comparison reuses exactly this directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_dir: Option<String>,
}

/// One path the evaluator changed, with git's one-letter status (`A`dded, `M`odified, `D`eleted,
/// `T`ype-changed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedPath {
    pub status: String,
    pub path: String,
}

/// The evidence a guarded unit changed the tree it was supposed to leave alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeMutation {
    pub before: WorktreeSnapshot,
    pub after: WorktreeSnapshot,
    /// Every differing path — each one denies.
    pub changed: Vec<ChangedPath>,
    /// `HEAD` moved between the snapshots — the unit committed, amended or reset the run branch.
    pub head_moved: bool,
}

impl WorktreeMutation {
    /// Whether this mutation denies the unit: any changed path, or a moved `HEAD`. (Always true for
    /// a mutation `compare` returns — kept as the one place the rule is spelled out.)
    pub fn denies(&self) -> bool {
        !self.changed.is_empty() || self.head_moved
    }
}

/// What the guard concluded for one guarded unit — carried from the worker thread to the gate
/// fold on `ApplyStepResult`. `None` on the wire means the guard NEVER RAN, which the fold treats
/// as fail-closed for a guarded unit (a guard whose absence looks like a pass is not a guard).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorktreeGuardOutcome {
    /// Both snapshots agree: the unit left the tree exactly as it found it.
    Clean {
        before: WorktreeSnapshot,
        after: WorktreeSnapshot,
    },
    /// The tree differs — every changed path is named, and the unit is denied.
    Mutated(WorktreeMutation),
    /// The comparison itself could not be made (git failed, no baseline was persisted at
    /// dispatch). Fail-closed: the fold denies with this reason rather than assuming clean.
    Unverifiable(String),
}

/// The engine's in-boundary scratch directory, relative to the worktree root — where every
/// worker's `TMPDIR` points (`execute_wrapped::redirect_scratch_into_boundary`) and where the
/// repo checks keep their isolated `HOME`/caches. Excluded from the snapshot by construction.
pub const ENGINE_SCRATCH_DIR: &str = "tmp";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Run `git <args>` in `cwd`; `Ok(stdout)` on exit 0, `Err` naming the failure otherwise.
fn git(cwd: &Path, args: &[&str], env: &[(&str, &Path)]) -> anyhow::Result<Vec<u8>> {
    // spawn-audit: hardened — git plumbing over the run's own worktree; reads no engine state.
    let mut cmd = Command::new("git");
    cmd.hardened().args(args).current_dir(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("could not spawn `git {}`: {e}", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "`git {}` in {} failed ({}): {}",
            args.join(" "),
            cwd.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn git_string(cwd: &Path, args: &[&str], env: &[(&str, &Path)]) -> anyhow::Result<String> {
    Ok(String::from_utf8_lossy(&git(cwd, args, env)?)
        .trim()
        .to_string())
}

/// A scratch path for the temporary index — pid + thread + counter scoped so concurrent snapshots
/// (parallel runs, parallel tests) never share one, and never inside the worktree (it would count).
fn scratch_index_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "wicked-worktree-guard-{}-{:?}-{}.idx",
        std::process::id(),
        std::thread::current().id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// The git directory for `worktree`, PINNED from the registered repository `repo_root` — never
/// read through the worktree's own `.git` (adversarial review on #414: an evaluator that rewrites
/// `<worktree>/.git` to `gitdir: <worktree>/tmp/fakegit` — inside the guard's own excluded scratch,
/// so a worktree-confined OS boundary allows it — would otherwise redirect both snapshots to a
/// repository it controls, and the after-tree would equal the baseline by construction).
///
/// `<repo_root>/.git` must be a real directory; the main checkout pins to it, and a linked worktree
/// pins to the `<repo_root>/.git/worktrees/<id>` whose repo-side `gitdir` file names this worktree.
/// Anything else — a `.git` that is a file or a link, a directory that is not one of the repo's
/// worktrees — is an error, which the caller treats as fail-closed.
pub fn pinned_git_dir(worktree: &Path, repo_root: &Path) -> anyhow::Result<PathBuf> {
    let root = std::fs::canonicalize(repo_root)
        .map_err(|e| anyhow::anyhow!("registered repo {} unreadable: {e}", repo_root.display()))?;
    let wt = std::fs::canonicalize(worktree)
        .map_err(|e| anyhow::anyhow!("worktree {} unreadable: {e}", worktree.display()))?;
    let main_git = root.join(".git");
    let meta = std::fs::symlink_metadata(&main_git)
        .map_err(|e| anyhow::anyhow!("registered repo {} has no .git: {e}", root.display()))?;
    if !meta.is_dir() {
        anyhow::bail!(
            "registered repo {}'s .git is not a directory (a file or a link) — refusing to pin a \
             git dir through it",
            root.display()
        );
    }
    if wt == root {
        return Ok(plain_path(main_git));
    }
    let worktrees = main_git.join("worktrees");
    let entries = std::fs::read_dir(&worktrees).map_err(|e| {
        anyhow::anyhow!(
            "{} is not a linked worktree of {} (no worktrees dir: {e})",
            wt.display(),
            root.display()
        )
    })?;
    for entry in entries.flatten() {
        let dir = entry.path();
        // The repo-side pointer: `<repo>/.git/worktrees/<id>/gitdir` holds `<worktree>/.git`.
        let pointer = dir.join("gitdir");
        let Ok(pm) = std::fs::symlink_metadata(&pointer) else {
            continue;
        };
        if !pm.is_file() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&pointer) else {
            continue;
        };
        let Some(parent) = Path::new(raw.trim()).parent() else {
            continue;
        };
        if std::fs::canonicalize(parent).ok().as_deref() == Some(wt.as_path()) {
            return Ok(plain_path(dir));
        }
    }
    anyhow::bail!(
        "{} is not a registered worktree of {} — no entry under {} points at it",
        wt.display(),
        root.display(),
        worktrees.display()
    )
}

/// `std::fs::canonicalize` on Windows yields a verbatim (`\\?\`) path, which git refuses as a
/// `GIT_DIR` ("not a git repository"); hand git the plain spelling. A no-op elsewhere.
fn plain_path(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    p
}

/// Take the worktree's content snapshot THROUGH `git_dir` (see [`pinned_git_dir`]). Never modifies
/// the real index, any ref, or the worktree: the scratch index is seeded EMPTY and `add -A` hashes
/// every path — the real index is never copied, so no per-entry `assume-unchanged` bit (which
/// tells `add` to trust the cached hash and skip the file) can hide a rewrite (adversarial review
/// on #414: `git update-index --assume-unchanged src/a.ts` + rewrite used to snapshot as Clean).
/// The full re-hash is the price, paid on purpose.
fn snapshot_through(worktree: &Path, git_dir: &Path) -> anyhow::Result<WorktreeSnapshot> {
    let pinned: [(&str, &Path); 2] = [("GIT_DIR", git_dir), ("GIT_WORK_TREE", worktree)];
    let head = git_string(
        worktree,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        &pinned,
    )
    .unwrap_or_default();
    let tmp_index = scratch_index_path();
    // A stale scratch file from a crashed run would seed the index — refuse to start from one.
    let _ = std::fs::remove_file(&tmp_index);
    // The engine scratch is excluded through git's own exclude mechanism — a scratch excludes
    // file naming `/tmp/` at the repo root — so it never enters the snapshot, exactly like a
    // `.gitignore`d artifact. `-c core.excludesFile` REPLACES the operator's global excludes for
    // this one command, which is the deterministic choice: a snapshot must not depend on the
    // daemon host's `~/.gitignore_global`.
    let excludes = tmp_index.with_extension("exclude");
    std::fs::write(&excludes, format!("/{ENGINE_SCRATCH_DIR}/\n"))?;
    let excludes_arg = format!("core.excludesFile={}", excludes.display());
    let env: [(&str, &Path); 3] = [
        ("GIT_DIR", git_dir),
        ("GIT_WORK_TREE", worktree),
        ("GIT_INDEX_FILE", tmp_index.as_path()),
    ];
    let result = (|| -> anyhow::Result<String> {
        git(
            worktree,
            &["-c", &excludes_arg, "add", "-A", "--", "."],
            &env,
        )?;
        git_string(worktree, &["write-tree"], &env)
    })();
    // Scratch hygiene on every exit — including `add`'s `<index>.lock` if it was left behind.
    let _ = std::fs::remove_file(&tmp_index);
    let _ = std::fs::remove_file(tmp_index.with_extension("idx.lock"));
    let _ = std::fs::remove_file(&excludes);
    let tree = result?;
    Ok(WorktreeSnapshot {
        head,
        tree,
        taken_at_ms: now_ms(),
        git_dir: Some(git_dir.to_string_lossy().into_owned()),
    })
}

/// Take the worktree's content snapshot, pinned to the registered repository `repo_root` (see
/// [`pinned_git_dir`] and [`snapshot_through`]). The baseline the actor persists at dispatch.
pub fn snapshot(worktree: &Path, repo_root: &Path) -> anyhow::Result<WorktreeSnapshot> {
    let git_dir = pinned_git_dir(worktree, repo_root)?;
    snapshot_through(worktree, &git_dir)
}

/// Parse `git diff-tree -r -z --name-status --no-renames` output: `<status>\0<path>\0` pairs.
fn parse_name_status_z(raw: &[u8]) -> Vec<ChangedPath> {
    let mut out = Vec::new();
    let mut fields = raw.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        out.push(ChangedPath {
            status: String::from_utf8_lossy(status).trim().to_string(),
            path: String::from_utf8_lossy(path).to_string(),
        });
    }
    out
}

/// Re-snapshot `worktree` and compare it with `before`. `Ok(None)` when nothing changed;
/// `Ok(Some(mutation))` otherwise — and a mutation always denies: every differing path is named,
/// none is exempt. The production path is [`compare_with_after`] (via `outcome_for_unit`), which
/// also keeps the after snapshot; this is the tests' view of the same comparison.
#[cfg(test)]
pub(crate) fn compare(
    worktree: &Path,
    before: &WorktreeSnapshot,
) -> anyhow::Result<Option<WorktreeMutation>> {
    compare_with_after(worktree, before).map(|(_, m)| m)
}

/// [`compare`], also handing back the AFTER snapshot the comparison was made against — so a clean
/// outcome carries the snapshot that was actually taken (its own tree id and `taken_at_ms`), not a
/// copy of the baseline (Copilot on #414: an "after" stamped with the baseline's time misreads on
/// the bus and in the logs).
fn compare_with_after(
    worktree: &Path,
    before: &WorktreeSnapshot,
) -> anyhow::Result<(WorktreeSnapshot, Option<WorktreeMutation>)> {
    // The SAME pinned git dir the baseline was taken through — never rediscovered, and never
    // through the worktree's own `.git` (adversarial review on #414).
    let Some(git_dir) = before.git_dir.as_deref() else {
        anyhow::bail!(
            "the baseline carries no pinned git dir (taken by an engine without the pin) — the \
             tree cannot be compared through a directory the evaluator could have redirected"
        );
    };
    let git_dir = Path::new(git_dir);
    let after = snapshot_through(worktree, git_dir)?;
    let head_moved = after.head != before.head;
    if after.tree == before.tree && !head_moved {
        return Ok((after, None));
    }
    let changed = if after.tree == before.tree {
        Vec::new()
    } else {
        parse_name_status_z(&git(
            worktree,
            &[
                "diff-tree",
                "-r",
                "-z",
                "--name-status",
                "--no-renames",
                &before.tree,
                &after.tree,
            ],
            &[("GIT_DIR", git_dir), ("GIT_WORK_TREE", worktree)],
        )?)
    };
    Ok((
        after.clone(),
        Some(WorktreeMutation {
            before: before.clone(),
            after,
            changed,
            head_moved,
        }),
    ))
}

/// Run the guard for one finished unit: the worker-thread half. `None` when the unit is not
/// guarded (or unbound) — the fold then expects no outcome. Never panics on git failure: that is
/// [`WorktreeGuardOutcome::Unverifiable`], which the fold denies.
pub(crate) fn outcome_for_unit(
    unit: &crate::domain::WorkUnit,
    workdir: Option<&Path>,
) -> Option<WorktreeGuardOutcome> {
    if !applies_to(unit) {
        return None;
    }
    let wd = workdir?;
    let Some(before) = unit.worktree_baseline.as_ref() else {
        return Some(WorktreeGuardOutcome::Unverifiable(
            "no worktree baseline was persisted at dispatch, so the tree cannot be compared \
             (the unit was dispatched by an engine without the guard, or the snapshot failed — \
             see the daemon log)"
                .to_string(),
        ));
    };
    match compare_with_after(wd, before) {
        Ok((_, Some(m))) => Some(WorktreeGuardOutcome::Mutated(m)),
        Ok((after, None)) => Some(WorktreeGuardOutcome::Clean {
            before: before.clone(),
            after,
        }),
        Err(e) => Some(WorktreeGuardOutcome::Unverifiable(e.to_string())),
    }
}

/// Whether the guard governs `unit` at all: a def-driven, agent-executed unit whose phase declared
/// `executes_code: false` ([`crate::domain::WorkUnit::worktree_guarded`], set at plan time). Tool
/// units are the engine's own deterministic commands (a `deliver` push MOVES `HEAD` on purpose).
pub(crate) fn applies_to(unit: &crate::domain::WorkUnit) -> bool {
    unit.worktree_guarded && unit.tool_cmd.is_none()
}

fn short(id: &str) -> &str {
    &id[..id.len().min(10)]
}

/// The operator-facing denial for a mutation (`UnitDenial.reason` / `GateEvaluated.denialReason`).
/// Names the rule, the phase, every path with its status, both tree ids, and the two ways forward.
pub(crate) fn denial_reason(unit: &crate::domain::WorkUnit, m: &WorktreeMutation) -> String {
    let phase = unit.phase_id().unwrap_or("this phase");
    let mut s = format!(
        "evaluator≠creator: phase `{phase}` declares `executes_code: false` but changed the \
         worktree it was reviewing"
    );
    if !m.changed.is_empty() {
        s.push_str(&format!(" — {} path(s): ", m.changed.len()));
        s.push_str(
            &m.changed
                .iter()
                .map(|c| format!("{} {}", c.status, c.path))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if m.head_moved {
        s.push_str(&format!(
            "; HEAD moved {} → {} (the run branch was committed, amended or reset)",
            short(&m.before.head),
            short(&m.after.head)
        ));
    }
    s.push_str(&format!(
        " (tree {} → {}). The change under review is no longer the creator's, so this phase's \
         verdict cannot certify it. Restore the creator's tree in the worktree with `git read-tree \
         --reset -u {}`, or reject the run; a phase that must change code declares \
         `executes_code: true` in the workflow def.",
        short(&m.before.tree),
        short(&m.after.tree),
        m.before.tree
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::WorkUnit;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wicked-wtguard-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run_git(cwd: &Path, args: &[&str]) -> String {
        // spawn-audit: test-only — a git fixture building the worktree layout under test; it reads no engine state.
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repo with one commit and a linked worktree on `wicked/<tag>` — the engine's real layout —
    /// with the creator's work left UNCOMMITTED in it (one tracked modification, one new file),
    /// exactly the state a `verify` phase inherits.
    fn creator_worktree(tag: &str) -> PathBuf {
        let base = scratch(tag);
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        run_git(&repo, &["init", "-q", "."]);
        run_git(&repo, &["config", "user.email", "t@example.invalid"]);
        run_git(&repo, &["config", "user.name", "t"]);
        run_git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("src/a.ts"), "export const a = 1;\n").unwrap();
        std::fs::write(repo.join(".gitignore"), "node_modules/\n").unwrap();
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
                &format!("wicked/{tag}"),
            ],
        );
        // The creator's (fix phase's) work: uncommitted, one modification + one addition.
        std::fs::write(wt.join("src/a.ts"), "export const a = 2; // fixed\n").unwrap();
        std::fs::write(wt.join("src/b.ts"), "export const b = 1;\n").unwrap();
        wt
    }

    /// The registered repository a [`creator_worktree`] was linked from.
    fn repo_of(wt: &Path) -> PathBuf {
        wt.parent().unwrap().join("repo")
    }

    fn guarded_unit() -> WorkUnit {
        let mut u = WorkUnit::pending("s:verify", "s", 4, "verify");
        u.worktree_guarded = true;
        u
    }

    #[test]
    fn snapshot_is_stable_and_leaves_the_real_index_untouched() {
        let wt = creator_worktree("stable");
        let a = snapshot(&wt, &repo_of(&wt)).unwrap();
        let b = snapshot(&wt, &repo_of(&wt)).unwrap();
        assert_eq!(a.tree, b.tree, "two snapshots of an unchanged tree agree");
        assert_eq!(a.head, b.head);
        assert!(!a.head.is_empty(), "HEAD is recorded");
        // The real index must not have staged anything: `add -A` ran against the scratch copy.
        assert_eq!(
            run_git(&wt, &["diff", "--cached", "--name-only"]),
            "",
            "the guard must never stage into the worktree's real index"
        );
        // And the creator's uncommitted work is still uncommitted, exactly as it was.
        // (`run_git` trims, so the first line loses its leading status space.)
        let porcelain = run_git(&wt, &["status", "--porcelain"]);
        assert!(
            porcelain.contains("M src/a.ts") && porcelain.contains("?? src/b.ts"),
            "the creator's uncommitted work is untouched: {porcelain}"
        );
    }

    #[test]
    fn an_evaluator_edit_is_a_denying_mutation_naming_every_path() {
        let wt = creator_worktree("edit");
        let unit = guarded_unit();
        let before = snapshot(&wt, &repo_of(&wt)).unwrap();

        // The F-036 shape: the evaluator rewrites the fix (modify), deletes a creator file,
        // and adds a new source file.
        std::fs::write(
            wt.join("src/a.ts"),
            "export const a = 3; // evaluator's idea\n",
        )
        .unwrap();
        std::fs::remove_file(wt.join("src/b.ts")).unwrap();
        std::fs::write(wt.join("src/c.ts"), "export const c = 1;\n").unwrap();

        let m = compare(&wt, &before).unwrap().expect("the tree changed");
        assert!(m.denies());
        assert!(!m.head_moved, "no commit was made");
        let mut got: Vec<(String, String)> = m
            .changed
            .iter()
            .map(|c| (c.status.clone(), c.path.clone()))
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("A".to_string(), "src/c.ts".to_string()),
                ("D".to_string(), "src/b.ts".to_string()),
                ("M".to_string(), "src/a.ts".to_string()),
            ],
            "every changed path is named with its status"
        );
        let reason = denial_reason(&unit, &m);
        assert!(
            reason.contains("M src/a.ts")
                && reason.contains("D src/b.ts")
                && reason.contains("A src/c.ts")
                && reason.contains("executes_code: false")
                && reason.contains(&format!("git read-tree --reset -u {}", before.tree)),
            "the denial names the paths, the rule and the restore: {reason}"
        );
    }

    #[test]
    fn an_untouched_tree_is_clean_and_ignored_or_engine_scratch_files_never_count() {
        let wt = creator_worktree("clean");
        let before = snapshot(&wt, &repo_of(&wt)).unwrap();
        // The evaluator installs deps and runs tests — ignored artifacts only …
        std::fs::create_dir_all(wt.join("node_modules/x")).unwrap();
        std::fs::write(wt.join("node_modules/x/index.js"), "x").unwrap();
        // … and its temp files land in the ENGINE's scratch (`TMPDIR=<worktree>/tmp`), which the
        // snapshot excludes by construction, exactly like a gitignored path.
        std::fs::create_dir_all(wt.join(ENGINE_SCRATCH_DIR).join("wicked-checks/home")).unwrap();
        std::fs::write(wt.join(ENGINE_SCRATCH_DIR).join("scratch.txt"), "scratch").unwrap();
        assert!(
            compare(&wt, &before).unwrap().is_none(),
            "gitignored artifacts and the engine scratch are not a mutation"
        );
    }

    /// Adversarial review on #414 (CRITICAL, reproduced): `git update-index --assume-unchanged
    /// src/a.ts` then rewrite the file. With the scratch index seeded from the REAL index the bit
    /// was inherited and `add -A` trusted the cached hash — identical tree, Clean, gate passed. The
    /// scratch index now starts empty, so the rewrite is hashed and DENIES by name.
    #[test]
    fn an_assume_unchanged_rewrite_is_still_a_denying_mutation() {
        let wt = creator_worktree("assume-unchanged");
        let repo = repo_of(&wt);
        let before = snapshot(&wt, &repo).unwrap();
        run_git(&wt, &["update-index", "--assume-unchanged", "src/a.ts"]);
        std::fs::write(
            wt.join("src/a.ts"),
            "export const a = 'rewritten by the evaluator';\n",
        )
        .unwrap();
        let m = compare(&wt, &before)
            .unwrap()
            .expect("the rewrite is a mutation");
        assert_eq!(
            m.changed
                .iter()
                .map(|c| format!("{} {}", c.status, c.path))
                .collect::<Vec<_>>(),
            vec!["M src/a.ts"]
        );
        assert!(m.denies());
        // The skip-worktree spelling too (it already failed closed; keep it that way).
        run_git(&wt, &["update-index", "--no-assume-unchanged", "src/a.ts"]);
        run_git(&wt, &["update-index", "--skip-worktree", "src/a.ts"]);
        std::fs::write(wt.join("src/a.ts"), "export const a = 'rewritten again';\n").unwrap();
        let m = compare(&wt, &before).unwrap().expect("still a mutation");
        assert!(m.changed.iter().any(|c| c.path == "src/a.ts"));
    }

    /// Adversarial review on #414 (CRITICAL, reproduced with in-worktree writes only): rewrite the
    /// worktree's `.git` FILE to `gitdir: <worktree>/tmp/fakegit` — under the guard's own excluded
    /// scratch, so a worktree-confined OS boundary allows it — pointing at a repository the
    /// evaluator controls. The snapshot goes through the git dir PINNED from the registered repo,
    /// so the redirect changes nothing: the evaluator's rewrite is a denying mutation by name.
    #[test]
    fn a_redirected_dot_git_file_cannot_hide_a_rewrite() {
        let wt = creator_worktree("redirected-gitdir");
        let repo = repo_of(&wt);
        let before = snapshot(&wt, &repo).unwrap();
        assert!(
            before
                .git_dir
                .as_deref()
                .is_some_and(|d| d.contains(".git/worktrees/")),
            "pinned from the registered repo: {:?}",
            before.git_dir
        );
        // The evaluator builds a fake repository inside the excluded scratch and points `.git` at it
        // — with the current content already committed there, so an unpinned snapshot would read
        // "nothing changed" whatever it does next.
        let fake = wt.join(ENGINE_SCRATCH_DIR).join("fakegit");
        std::fs::create_dir_all(&fake).unwrap();
        run_git(&fake, &["init", "-q", "--bare", "."]);
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", fake.display())).unwrap();
        std::fs::write(
            wt.join("src/a.ts"),
            "export const a = 'rewritten behind a fake .git';\n",
        )
        .unwrap();
        match compare(&wt, &before) {
            Ok(Some(m)) => {
                assert!(
                    m.changed.iter().any(|c| c.path == "src/a.ts"),
                    "the rewrite is named: {:?}",
                    m.changed
                );
                assert!(m.denies());
            }
            Ok(None) => {
                panic!("the redirected .git hid the rewrite — the guard read the fake repo")
            }
            Err(e) => {
                // Fail-closed is the other acceptable answer: the fold denies an unverifiable guard.
                eprintln!("comparison refused (fail-closed): {e}");
            }
        }
        // And a BASELINE without the pin can never be compared through a rediscovered `.git`.
        let mut unpinned = before.clone();
        unpinned.git_dir = None;
        assert!(
            compare(&wt, &unpinned).is_err(),
            "no pin ⇒ unverifiable, never clean"
        );
    }

    /// A directory that is not one of the registered repo's worktrees cannot be snapshotted at
    /// all — the guard refuses to guess a git dir for it (fail-closed at dispatch).
    #[test]
    fn a_directory_that_is_not_a_registered_worktree_is_refused() {
        let wt = creator_worktree("not-registered");
        let stranger = scratch("stranger-repo");
        run_git(&stranger, &["init", "-q", "."]);
        assert!(snapshot(&wt, &stranger).is_err());
        // The repo's own main checkout pins to its `.git` directory.
        let repo = repo_of(&wt);
        let main = snapshot(&repo, &repo).unwrap();
        assert!(main.git_dir.as_deref().is_some_and(|d| d.ends_with(".git")));
    }

    /// Copilot on #414: a CLEAN outcome's `after` is the snapshot that was actually taken at the
    /// comparison — same tree and HEAD as the baseline, but its own `taken_at_ms` — never a copy of
    /// the baseline dressed up as an after.
    #[test]
    fn a_clean_outcome_carries_the_real_after_snapshot_not_a_copy_of_the_baseline() {
        let wt = creator_worktree("clean-after");
        let before = snapshot(&wt, &repo_of(&wt)).unwrap();
        let mut unit = guarded_unit();
        unit.worktree_baseline = Some(before.clone());
        std::thread::sleep(std::time::Duration::from_millis(5));
        match outcome_for_unit(&unit, Some(&wt)) {
            Some(WorktreeGuardOutcome::Clean {
                before: b,
                after: a,
            }) => {
                assert_eq!(b, before);
                assert_eq!(a.tree, before.tree);
                assert_eq!(a.head, before.head);
                assert!(
                    a.taken_at_ms > before.taken_at_ms,
                    "the after snapshot was taken later: {} vs {}",
                    a.taken_at_ms,
                    before.taken_at_ms
                );
            }
            other => panic!("expected Clean, got {other:?}"),
        }
    }

    #[test]
    fn nothing_is_exempt_documentation_deliverables_and_tool_state_all_deny() {
        // Codex review on #414: an evaluator's write-up belongs in its OUTPUT, not in the tree it
        // is judging; a declared deliverable that must live in the tree makes its phase a code
        // phase (`executes_code: true`, as `domain-extraction/coverage` now declares); an in-tree
        // code graph moving under a recon phase is a defect to surface (core#406). No exemptions.
        let wt = creator_worktree("no-exemptions");
        let mut unit = guarded_unit();
        unit.required_deliverables = vec!["coverage-report.json".to_string()];
        let before = snapshot(&wt, &repo_of(&wt)).unwrap();
        std::fs::create_dir_all(wt.join("docs")).unwrap();
        std::fs::write(wt.join("docs/review.md"), "# findings").unwrap();
        std::fs::write(wt.join("NOTES.txt"), "notes").unwrap();
        std::fs::write(wt.join("coverage-report.json"), "{}").unwrap();
        std::fs::create_dir_all(wt.join(".codegraph")).unwrap();
        std::fs::write(wt.join(".codegraph/estate.db"), "db").unwrap();
        let m = compare(&wt, &before).unwrap().expect("the tree changed");
        assert!(m.denies(), "every changed path denies: {m:?}");
        let mut paths: Vec<&str> = m.changed.iter().map(|c| c.path.as_str()).collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                ".codegraph/estate.db",
                "NOTES.txt",
                "coverage-report.json",
                "docs/review.md"
            ],
            "documentation, a DECLARED deliverable and tool state are all named and all deny"
        );
        assert!(matches!(
            outcome_for_unit(
                &WorkUnit {
                    worktree_baseline: Some(before.clone()),
                    ..unit.clone()
                },
                Some(&wt)
            ),
            Some(WorktreeGuardOutcome::Mutated(_))
        ));
    }

    #[test]
    fn a_commit_moves_head_and_denies_even_with_an_identical_tree() {
        let wt = creator_worktree("commit");
        let unit = guarded_unit();
        let before = snapshot(&wt, &repo_of(&wt)).unwrap();
        // The evaluator commits the creator's work verbatim: same content, different history.
        run_git(&wt, &["add", "-A"]);
        run_git(&wt, &["commit", "-qm", "evaluator commits the fix"]);
        let m = compare(&wt, &before).unwrap().expect("HEAD moved");
        assert!(m.head_moved);
        assert!(m.changed.is_empty(), "the content did not change");
        assert!(
            m.denies(),
            "a moved HEAD denies: the run branch is no longer the creator's"
        );
        assert!(denial_reason(&unit, &m).contains("HEAD moved"));
    }

    #[test]
    fn outcome_for_unit_is_fail_closed_without_a_baseline_and_inert_when_unguarded() {
        let wt = creator_worktree("outcome");
        let mut unit = guarded_unit();
        // Guarded but no baseline persisted ⇒ Unverifiable (never a silent Clean).
        assert!(matches!(
            outcome_for_unit(&unit, Some(&wt)),
            Some(WorktreeGuardOutcome::Unverifiable(_))
        ));
        // With a baseline and no change ⇒ Clean.
        unit.worktree_baseline = Some(snapshot(&wt, &repo_of(&wt)).unwrap());
        assert!(matches!(
            outcome_for_unit(&unit, Some(&wt)),
            Some(WorktreeGuardOutcome::Clean { .. })
        ));
        // Unbound ⇒ nothing to guard.
        assert!(outcome_for_unit(&unit, None).is_none());
        // A Tool unit is the engine's own command ⇒ never guarded.
        unit.tool_cmd = Some(vec!["true".into()]);
        assert!(outcome_for_unit(&unit, Some(&wt)).is_none());
        // An executes_code phase (creator) is never guarded.
        let mut creator = WorkUnit::pending("s:fix", "s", 3, "fix");
        creator.executes_code = true;
        assert!(!applies_to(&creator));
    }

    #[test]
    fn a_non_git_workdir_is_unverifiable_not_clean() {
        let dir = scratch("nongit");
        let mut unit = guarded_unit();
        unit.worktree_baseline = Some(WorktreeSnapshot {
            head: String::new(),
            tree: "0".repeat(40),
            taken_at_ms: 0,
            git_dir: None,
        });
        // Premise: the scratch dir is outside any repo.
        // spawn-audit: test-only — checks the premise that the scratch dir is outside a repo.
        let outside = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&dir)
            .output()
            .expect("git runs");
        assert!(
            !outside.status.success(),
            "premise: scratch is not in a repo"
        );
        assert!(matches!(
            outcome_for_unit(&unit, Some(&dir)),
            Some(WorktreeGuardOutcome::Unverifiable(_))
        ));
    }
}
