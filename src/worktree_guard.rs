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

/// The worktree's OWN index file (a linked worktree's lives under `.git/worktrees/<name>/index`,
/// never the main checkout's), absolute.
fn index_path(worktree: &Path) -> anyhow::Result<PathBuf> {
    let raw = git_string(worktree, &["rev-parse", "--git-path", "index"], &[])?;
    let p = PathBuf::from(raw);
    Ok(if p.is_absolute() { p } else { worktree.join(p) })
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

/// Take the worktree's content snapshot. Never modifies the real index, any ref, or the worktree.
pub fn snapshot(worktree: &Path) -> anyhow::Result<WorktreeSnapshot> {
    let head = git_string(worktree, &["rev-parse", "--verify", "--quiet", "HEAD"], &[])
        .unwrap_or_default();
    let tmp_index = scratch_index_path();
    // The engine scratch is excluded through git's own exclude mechanism — a scratch excludes
    // file naming `/tmp/` at the repo root — so it never enters the snapshot, exactly like a
    // `.gitignore`d artifact. `-c core.excludesFile` REPLACES the operator's global excludes for
    // this one command, which is the deterministic choice: a snapshot must not depend on the
    // daemon host's `~/.gitignore_global`.
    let excludes = tmp_index.with_extension("exclude");
    std::fs::write(&excludes, format!("/{ENGINE_SCRATCH_DIR}/\n"))?;
    let excludes_arg = format!("core.excludesFile={}", excludes.display());
    // Seed the scratch index from the real one so `add -A` is a stat-cache walk, not a full
    // re-hash. A missing real index (freshly initialised repo) is fine — `add -A` builds it.
    if let Ok(real) = index_path(worktree) {
        let _ = std::fs::copy(&real, &tmp_index);
    }
    let env: [(&str, &Path); 1] = [("GIT_INDEX_FILE", tmp_index.as_path())];
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
    })
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
/// none is exempt.
pub fn compare(
    worktree: &Path,
    before: &WorktreeSnapshot,
) -> anyhow::Result<Option<WorktreeMutation>> {
    let after = snapshot(worktree)?;
    let head_moved = after.head != before.head;
    if after.tree == before.tree && !head_moved {
        return Ok(None);
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
            &[],
        )?)
    };
    Ok(Some(WorktreeMutation {
        before: before.clone(),
        after,
        changed,
        head_moved,
    }))
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
    match compare(wd, before) {
        Ok(Some(m)) => Some(WorktreeGuardOutcome::Mutated(m)),
        Ok(None) => Some(WorktreeGuardOutcome::Clean {
            before: before.clone(),
            after: before.clone(),
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

    fn guarded_unit() -> WorkUnit {
        let mut u = WorkUnit::pending("s:verify", "s", 4, "verify");
        u.worktree_guarded = true;
        u
    }

    #[test]
    fn snapshot_is_stable_and_leaves_the_real_index_untouched() {
        let wt = creator_worktree("stable");
        let a = snapshot(&wt).unwrap();
        let b = snapshot(&wt).unwrap();
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
        let before = snapshot(&wt).unwrap();

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
        let before = snapshot(&wt).unwrap();
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

    #[test]
    fn nothing_is_exempt_documentation_deliverables_and_tool_state_all_deny() {
        // Codex review on #414: an evaluator's write-up belongs in its OUTPUT, not in the tree it
        // is judging; a declared deliverable that must live in the tree makes its phase a code
        // phase (`executes_code: true`, as `domain-extraction/coverage` now declares); an in-tree
        // code graph moving under a recon phase is a defect to surface (core#406). No exemptions.
        let wt = creator_worktree("no-exemptions");
        let mut unit = guarded_unit();
        unit.required_deliverables = vec!["coverage-report.json".to_string()];
        let before = snapshot(&wt).unwrap();
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
        let before = snapshot(&wt).unwrap();
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
        unit.worktree_baseline = Some(snapshot(&wt).unwrap());
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
