//! (crew#1493) The repository's own checks on a finished run's REBASED tree, before crew
//! re-delivers it.
//!
//! A run's delivered pull request can conflict with its base after delivery (another PR landed
//! first). crew rebases the PR branch onto the current base in a throwaway worktree (only the
//! base-only rebase and the CHANGELOG `[Unreleased]` resolver change the tree) and must not push a
//! tree no check ran on. The engine's deliver lift cannot do it (a run is finished, and the lift
//! skips a branch whose commits the remote lacks), so this is the same verify floor the lift's
//! re-verify runs ([`super::lift_and_reverify`]): a clean checkout of exactly the tree the head
//! commit holds ([`crate::repo_checks::checkout_guarded_tree`]), the Verify-stage
//! [`crate::repo_checks::FloorContext`] keyed by the run, measured against the base the tree was
//! rebased onto (a failure the base shares does not deny), and the dependency install forced
//! (the rebase can move a lockfile). Nothing of the floor is copied here; it is
//! [`crate::repo_checks::run_floor`], and the checkout is held to its tree afterwards.

use std::path::Path;

use super::{git_string, is_ancestor, pinned_git_dir, short};
use crate::repo_checks::{FloorContext, FloorStage, RepoChecksReport};

/// What [`check_run_tree`] answers: the head commit and tree the checks ran on, the base they were
/// measured against, and the floor's report (`report.passed` is the verdict).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeRecheck {
    /// The commit `workdir`'s `HEAD` named when the checks started: what crew may push.
    pub head: String,
    /// That commit's tree: what the checks ran on.
    pub tree: String,
    /// The base commit the floor measured against (an ancestor of `head`).
    pub base: String,
    /// True iff the floor passed on `tree` (`report.passed`).
    pub passed: bool,
    /// The verify floor's report, as the run's own `repoChecksEvaluated` carries it.
    pub report: RepoChecksReport,
}

/// Is `s` a full 40- or 64-character hex object id?
fn full_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Run the verify floor of run `run_id` on the tree of `workdir`'s `HEAD`, measured against
/// `base_commit`. `workdir` is a worktree of `repo_root` (the registered repository: its git dir is
/// pinned through it, never through the worktree's own `.git` file). `Err` when the inputs do not
/// name that shape: a run id that is empty, a base that is not a full object id, not a commit, or
/// not an ancestor of `HEAD`, a worktree that is not one of `repo_root`'s, or a tree that cannot be
/// checked out. A floor that refuses (no OS write boundary, a cancelled run, a short disk) is an
/// `Ok` report with `passed: false`, as at verify.
pub fn check_run_tree(
    run_id: &str,
    workdir: &Path,
    repo_root: &Path,
    base_commit: &str,
) -> anyhow::Result<TreeRecheck> {
    if run_id.trim().is_empty() {
        anyhow::bail!("check_run_tree: the run id is empty");
    }
    if !full_object_id(base_commit) {
        anyhow::bail!("check_run_tree: the base must be a full commit id (got {base_commit:?})");
    }
    let git_dir = pinned_git_dir(workdir, repo_root)?;
    let pinned: [(&str, &Path); 1] = [("GIT_DIR", git_dir.as_path())];
    let head = git_string(workdir, &["rev-parse", "--verify", "HEAD^{commit}"], &[])?;
    let base = git_string(
        workdir,
        &[
            "rev-parse",
            "--verify",
            &format!("{base_commit}^{{commit}}"),
        ],
        &pinned,
    )
    .map_err(|e| anyhow::anyhow!("check_run_tree: the base {base_commit} is not a commit: {e}"))?;
    if !is_ancestor(workdir, &pinned, &base, &head) {
        anyhow::bail!(
            "check_run_tree: the base {} is not an ancestor of HEAD {}: the tree was not rebased \
             onto it",
            short(&base),
            short(&head)
        );
    }
    let tree = git_string(workdir, &["rev-parse", "--verify", "HEAD^{tree}"], &[])?;
    let ctx = FloorContext {
        stage: FloorStage::Verify,
        force_install: true,
        base_head: Some(base.clone()),
        git_dir: Some(git_dir.clone()),
        claim_text: None,
        package_dir: None,
        run_id: Some(run_id.to_string()),
        code_graph: None,
    };
    let mut report = match crate::repo_checks::checkout_guarded_tree(
        workdir,
        &git_dir,
        &tree,
        &format!("{run_id}-recheck"),
    ) {
        Ok(checkout) => {
            let mut r = crate::repo_checks::run_floor(&checkout.dir, &ctx);
            checkout.hold_to(&tree, &mut r);
            r
        }
        Err(why) => crate::repo_checks::guarded_checkout_refused(workdir, &ctx, &why),
    };
    report.key = report.key_for(Some(&tree), &ctx);
    Ok(TreeRecheck {
        passed: report.passed,
        head,
        tree,
        base,
        report,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // spawn-audit: test-only — git fixtures building the layout under test.
    fn git(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A registered repository whose base commit carries a check (`test -f ok`) that passes, and a
    /// worktree on `wicked/<tag>` with one commit on top of it. `head_keeps_ok` decides whether the
    /// head still holds `ok` (the check passes) or deletes it (a regression the base does not share).
    fn layout(tag: &str, head_keeps_ok: bool) -> (PathBuf, PathBuf, String) {
        let base = std::env::temp_dir().join(format!(
            "wicked-recheck-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".wicked")).unwrap();
        git(&repo, &["init", "-q", "-b", "main", "."]);
        for (k, v) in [
            ("user.email", "t@example.invalid"),
            ("user.name", "t"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
        ] {
            git(&repo, &["config", k, v]);
        }
        std::fs::write(
            repo.join(crate::repo_checks::CONFIG_PATH),
            serde_json::json!({ "test": ["sh", "-c", "test -f ok"], "timeout_s": 60 }).to_string(),
        )
        .unwrap();
        std::fs::write(repo.join("ok"), "ok\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "base"]);
        let base_commit = git(&repo, &["rev-parse", "HEAD"]);
        let wt = base.join("wt");
        git(
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
        std::fs::write(wt.join("change.txt"), "the run's change\n").unwrap();
        if !head_keeps_ok {
            std::fs::remove_file(wt.join("ok")).unwrap();
        }
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "the run's change"]);
        (repo, wt, base_commit)
    }

    /// No OS write boundary on this host: the floor refuses (fail-closed) and the verdict cannot be
    /// exercised here; the refusal itself must still read `passed: false`.
    fn unboxed(r: &TreeRecheck) -> bool {
        let skip = r.report.sandbox_error.is_some();
        if skip {
            assert!(!r.passed, "a refused floor never passes");
            eprintln!("recheck: no sandbox tool here — the floor refused, as it must");
        }
        skip
    }

    #[test]
    fn a_rebased_tree_whose_checks_pass_reads_passed_on_that_exact_tree() {
        let (repo, wt, base) = layout("pass", true);
        let r = check_run_tree("run-pass", &wt, &repo, &base).unwrap();
        assert_eq!(r.head, git(&wt, &["rev-parse", "HEAD"]));
        assert_eq!(r.tree, git(&wt, &["rev-parse", "HEAD^{tree}"]));
        assert_eq!(r.base, base);
        if unboxed(&r) {
            return;
        }
        assert!(r.passed, "{:?}", r.report);
        assert_eq!(r.passed, r.report.passed);
        assert!(!r.report.checks.is_empty(), "the check ran: {:?}", r.report);
    }

    /// SILENT-OK, from the side of a confused re-delivery: a rebased tree that broke a check the
    /// base passes must read FAILED, so crew pushes nothing.
    #[test]
    fn a_rebased_tree_that_breaks_a_check_the_base_passes_reads_failed() {
        let (repo, wt, base) = layout("fail", false);
        let r = check_run_tree("run-fail", &wt, &repo, &base).unwrap();
        if unboxed(&r) {
            return;
        }
        assert!(!r.passed, "a regression must not pass: {:?}", r.report);
        assert!(!r.report.passed);
    }

    /// KEY: the checks run on the tree HEAD names (what crew pushes), never on the live worktree:
    /// an uncommitted edit that would break the check does not change the verdict, and one that
    /// would fix a broken head does not rescue it.
    #[test]
    fn the_checks_run_on_the_head_commits_tree_not_on_uncommitted_edits() {
        let (repo, wt, base) = layout("dirty-pass", true);
        std::fs::remove_file(wt.join("ok")).unwrap();
        let r = check_run_tree("run-dirty-pass", &wt, &repo, &base).unwrap();
        if !unboxed(&r) {
            assert!(r.passed, "the committed tree holds ok: {:?}", r.report);
        }
        let (repo, wt, base) = layout("dirty-fail", false);
        std::fs::write(wt.join("ok"), "planted\n").unwrap();
        let r = check_run_tree("run-dirty-fail", &wt, &repo, &base).unwrap();
        if !unboxed(&r) {
            assert!(
                !r.passed,
                "an uncommitted file never rescues the head: {:?}",
                r.report
            );
        }
    }

    /// The inputs must name a rebased tree: a base that is not a full commit id, not a commit, or
    /// not an ancestor of HEAD, and an empty run id, are refused before any check runs.
    #[test]
    fn inputs_that_do_not_name_a_rebased_tree_are_refused() {
        let (repo, wt, base) = layout("refuse", true);
        let err = |r: anyhow::Result<TreeRecheck>| r.err().map(|e| e.to_string()).unwrap();
        assert!(err(check_run_tree("", &wt, &repo, &base)).contains("run id"));
        assert!(err(check_run_tree("r", &wt, &repo, &base[..12])).contains("full commit id"));
        assert!(err(check_run_tree("r", &wt, &repo, &"0".repeat(40))).contains("not a commit"));
        // The head is not an ancestor of the base: a later commit on main the branch never saw.
        std::fs::write(repo.join("later.txt"), "later\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "later on main"]);
        let later = git(&repo, &["rev-parse", "HEAD"]);
        assert!(err(check_run_tree("r", &wt, &repo, &later)).contains("not an ancestor"));
        // A worktree of another repository.
        let (other, _, _) = layout("refuse-other", true);
        assert!(check_run_tree("r", &wt, &other, &base).is_err());
    }
}
