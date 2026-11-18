//! (crew#1493) The repository's own checks on a finished run's REBASED tree, before crew
//! re-delivers it.
//!
//! A run's delivered pull request can conflict with its base after delivery (another PR landed
//! first). crew rebases the PR branch onto the current base in a throwaway worktree (only the
//! base-only rebase and the CHANGELOG `[Unreleased]` resolver change the tree) and must not push a
//! tree no check ran on. The engine's deliver lift cannot do it (a run is finished, and the lift
//! skips a branch whose commits the remote lacks), so this is the same verify floor the lift's
//! re-verify runs ([`super::lift_and_reverify`]): a clean checkout of exactly the tree the head
//! commit holds ([`crate::repo_checks::checkout_guarded_tree`]), run through the same guarded,
//! tree-pinned entry ([`crate::repo_checks::FloorCheckout::run_floor_journaled`], so a container
//! floor checks that tree, never a re-snapshot), held to it afterwards, with a Verify-stage
//! [`crate::repo_checks::FloorContext`] measured against the base the tree was rebased onto.
//! Nothing of the floor is copied here.
//!
//! Three things differ from a live run's verify floor, each toward MORE checking (codex r1):
//! - The floor runs under its own id (`<run>-recheck-<n>`), not the run's: a finished run's id is
//!   in the floor-cancel set (its completion cancels its workers), which would refuse every
//!   recheck; and a cancel of the run never reaches a recheck it does not own.
//! - The full test command runs where the repository has one (the power full-suite rule set for
//!   that id; a repository whose only test is its targeted command keeps it): a finished run's own
//!   policy is gone with it (`power::forget_run`), and a re-delivery is a delivery checkpoint.
//! - A floor that ran no check (only provisioning, or none detected) does not pass: a re-delivery
//!   is never unverified (coord's ruling on crew#1493).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{git_string, is_ancestor, pinned_git_dir, short};
use crate::repo_checks::{FloorContext, FloorJournal, FloorStage, RepoChecksReport};

/// What [`check_run_tree`] answers: the head commit and tree the checks ran on, the base they were
/// measured against, and the floor's report (`report.passed` is the verdict).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeRecheck {
    /// The commit `workdir`'s `HEAD` named when the checks started: the immutable id crew pushes
    /// (never `HEAD` by name).
    pub head: String,
    /// That commit's tree (`<head>^{tree}`): what the checks ran on.
    pub tree: String,
    /// The base commit the floor measured against (an ancestor of `head`).
    pub base: String,
    /// True iff the floor passed on `tree` AND ran at least one check that is not provisioning
    /// (`report.passed`, which is set false otherwise).
    pub passed: bool,
    /// How many checks the floor attempted that are not provisioning (`install`, `native-build`);
    /// one that could not start still counts here and still denies.
    pub checks_run: usize,
    /// The verify floor's report, as the run's own `repoChecksEvaluated` carries it.
    pub report: RepoChecksReport,
}

/// The journal unit a recheck floor records under (its own floor id's log store: never a run's
/// unit).
const RECHECK_ORD: u32 = u32::MAX;

/// Is `s` a full 40- or 64-character hex object id?
fn full_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The floor id of one recheck of run `run_id`: unique in this process.
fn recheck_floor_id(run_id: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{run_id}-recheck-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// The recheck's floor id for the floor's duration: marked full-suite, and on drop (every path,
/// unwinding included) unmarked and its run namespace released (a container floor arms one under
/// the floor id; no actor ever ends that id, so the recheck does: `k8s::end_run` +
/// `k8s::release_run`, a no-op without a cluster — codex r2).
struct FloorRun<'a>(&'a str);

impl<'a> FloorRun<'a> {
    fn start(id: &'a str) -> Self {
        crate::power::set_full_suite(id, true);
        Self(id)
    }
}

impl Drop for FloorRun<'_> {
    fn drop(&mut self) {
        crate::power::set_full_suite(self.0, false);
        let epoch = crate::k8s::end_run(self.0);
        crate::k8s::release_run(self.0, epoch);
    }
}

/// Run the verify floor for run `run_id` on the tree of `workdir`'s `HEAD`, measured against
/// `base_commit`. `workdir` is a worktree of `repo_root` (the registered repository: its git dir is
/// pinned through it, never through the worktree's own `.git` file, and every read here goes
/// through that pinned dir). `Err` before any check when the inputs do not name that shape: a run
/// id that is empty, a base that is not the full id of a commit object or not an ancestor of
/// `HEAD`, a worktree that is not one of `repo_root`'s. A floor that refuses (no OS write
/// boundary, a checkout that could not be made, a short disk) or runs no check is an `Ok` answer
/// with `passed: false`.
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
    let base = base_commit.to_ascii_lowercase();
    let kind = git_string(workdir, &["cat-file", "-t", &base], &pinned)
        .map_err(|e| anyhow::anyhow!("check_run_tree: the base {base} is not a commit: {e}"))?;
    if kind != "commit" {
        anyhow::bail!("check_run_tree: the base {base} is not a commit (a {kind})");
    }
    // HEAD once, through the pinned dir; the tree is THAT commit's, so the two cannot disagree.
    let head = git_string(
        workdir,
        &["rev-parse", "--verify", "HEAD^{commit}"],
        &pinned,
    )?;
    if !is_ancestor(workdir, &pinned, &base, &head) {
        anyhow::bail!(
            "check_run_tree: the base {} is not an ancestor of HEAD {}: the tree was not rebased \
             onto it",
            short(&base),
            short(&head)
        );
    }
    let tree = git_string(
        workdir,
        &["rev-parse", "--verify", &format!("{head}^{{tree}}")],
        &pinned,
    )?;
    let floor_id = recheck_floor_id(run_id);
    let ctx = FloorContext {
        stage: FloorStage::Verify,
        force_install: true,
        base_head: Some(base.clone()),
        git_dir: Some(git_dir.clone()),
        claim_text: None,
        package_dir: None,
        run_id: Some(floor_id.clone()),
        code_graph: None,
    };
    let mut report = {
        let _floor = FloorRun::start(&floor_id);
        match crate::repo_checks::checkout_guarded_tree(workdir, &git_dir, &tree, &floor_id) {
            Ok(checkout) => {
                let r = checkout.run_floor_journaled(
                    &ctx,
                    None,
                    &FloorJournal {
                        ord: RECHECK_ORD,
                        attempt: 0,
                        tree: tree.clone(),
                        phase: Some("re-delivery recheck".into()),
                    },
                );
                crate::repo_checks::remove_check_scratch(&checkout.dir, &floor_id);
                r
            }
            Err(why) => crate::repo_checks::guarded_checkout_refused(workdir, &ctx, &why),
        }
    };
    let checks_run = report
        .checks
        .iter()
        .filter(|c| !crate::repo_checks::is_provisioning(&c.name))
        .count();
    if checks_run == 0 && report.outcome() != "not_run" {
        report.passed = false;
        report.detect_error = Some(
            "no repository check ran on this tree (only provisioning, or none detected): a \
             re-delivery is never unverified"
                .into(),
        );
    }
    report.key = report.key_for(Some(&tree), &ctx);
    Ok(TreeRecheck {
        passed: report.passed,
        checks_run,
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

    // Hardened like every spawn site (spawn_audit): the fixtures need nothing of the engine's env.
    fn git(cwd: &Path, args: &[&str]) -> String {
        use wicked_apps_core::spawn::HardenedCommand;
        let out = std::process::Command::new("git")
            .hardened()
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// The base commit's files, written before it is committed, and the change the run's one
    /// commit makes on `wicked/<tag>` in a worktree.
    struct Fixture<'a> {
        tag: &'a str,
        /// `.wicked/checks.json`; `None` = no checks config (and no manifest: nothing detected).
        checks: Option<serde_json::Value>,
        base_files: &'a [&'a str],
        head_removes: &'a [&'a str],
    }

    /// A registered repository whose base carries `base_files` (and the checks config), and a
    /// worktree on `wicked/<tag>` whose one commit adds `change.txt` and removes `head_removes`.
    fn layout(f: Fixture<'_>) -> (PathBuf, PathBuf, String) {
        let base = std::env::temp_dir().join(format!(
            "wicked-recheck-{}-{}-{:?}",
            f.tag,
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
        if let Some(checks) = &f.checks {
            std::fs::write(
                repo.join(crate::repo_checks::CONFIG_PATH),
                checks.to_string(),
            )
            .unwrap();
        }
        std::fs::write(repo.join("README"), "fixture\n").unwrap();
        for name in f.base_files {
            std::fs::write(repo.join(name), "present\n").unwrap();
        }
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
                &format!("wicked/{}", f.tag),
            ],
        );
        std::fs::write(wt.join("change.txt"), "the run's change\n").unwrap();
        for name in f.head_removes {
            std::fs::remove_file(wt.join(name)).unwrap();
        }
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "the run's change"]);
        (repo, wt, base_commit)
    }

    /// The one-check layout: `test -f ok` passes on the base; `head_keeps_ok` decides whether the
    /// head still holds `ok` or deletes it (a regression the base does not share).
    fn simple(tag: &str, head_keeps_ok: bool) -> (PathBuf, PathBuf, String) {
        layout(Fixture {
            tag,
            checks: Some(
                serde_json::json!({ "test": ["sh", "-c", "test -f ok"], "timeout_s": 60 }),
            ),
            base_files: &["ok"],
            head_removes: if head_keeps_ok { &[] } else { &["ok"] },
        })
    }

    /// No OS write boundary on this host: the floor refuses (fail-closed) and the verdict cannot be
    /// exercised here; the refusal itself must still read `passed: false`. Only THAT refusal
    /// skips: a cancel or a disk refusal fails the test (codex r1).
    fn unboxed(r: &TreeRecheck) -> bool {
        let skip = r
            .report
            .sandbox_error
            .as_deref()
            .is_some_and(|e| e.contains("no OS write boundary"));
        if skip {
            assert!(!r.passed, "a refused floor never passes");
            eprintln!("recheck: no sandbox tool here — the floor refused, as it must");
        }
        skip
    }

    #[test]
    fn a_rebased_tree_whose_checks_pass_reads_passed_on_that_exact_tree() {
        let (repo, wt, base) = simple("pass", true);
        let r = check_run_tree("run-pass", &wt, &repo, &base).unwrap();
        assert_eq!(r.head, git(&wt, &["rev-parse", "HEAD"]));
        assert_eq!(r.tree, git(&wt, &["rev-parse", "HEAD^{tree}"]));
        assert_eq!(r.base, base);
        if unboxed(&r) {
            return;
        }
        assert!(r.passed, "{:?}", r.report);
        assert_eq!(r.passed, r.report.passed);
        assert!(r.checks_run >= 1, "the check ran: {:?}", r.report);
        // KEY: the report is keyed by exactly the tree checked and the base measured against.
        let key = r.report.key.as_ref().expect("a finished floor is keyed");
        assert_eq!(key.tree, r.tree);
        assert_eq!(key.base.as_deref(), Some(base.as_str()));
    }

    /// SILENT-OK, from the side of a confused re-delivery: a rebased tree that broke a check the
    /// base passes must read FAILED, so crew pushes nothing.
    #[test]
    fn a_rebased_tree_that_breaks_a_check_the_base_passes_reads_failed() {
        let (repo, wt, base) = simple("fail", false);
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
        let (repo, wt, base) = simple("dirty-pass", true);
        std::fs::remove_file(wt.join("ok")).unwrap();
        let r = check_run_tree("run-dirty-pass", &wt, &repo, &base).unwrap();
        if !unboxed(&r) {
            assert!(r.passed, "the committed tree holds ok: {:?}", r.report);
        }
        let (repo, wt, base) = simple("dirty-fail", false);
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

    /// codex r1: a FINISHED run's id is in the floor-cancel set (its completion cancels its
    /// workers). The recheck runs under its own floor id, so it is not refused, and it leaves
    /// that cancel in place.
    #[test]
    fn a_finished_runs_cancel_does_not_refuse_its_recheck() {
        let (repo, wt, base) = simple("finished", true);
        let run = format!("run-finished-{}", std::process::id());
        crate::repo_checks::cancel_floor(&run);
        let r = check_run_tree(&run, &wt, &repo, &base).unwrap();
        let still = crate::repo_checks::floor_cancelled(Some(&run));
        crate::repo_checks::clear_floor_cancel(&run);
        assert!(still, "the finished run's cancel is left as it was");
        if unboxed(&r) {
            return;
        }
        assert!(r.passed, "not refused as cancelled: {:?}", r.report);
    }

    /// codex r1: the full suite always runs. A targeted command that passes never stands in for
    /// a full suite that fails (the finished run's own policy is gone).
    #[test]
    fn the_full_suite_runs_even_where_a_targeted_command_would_stand_in() {
        let (repo, wt, base) = layout(Fixture {
            tag: "full",
            checks: Some(serde_json::json!({
                "test": ["sh", "-c", "test -f full"],
                "test_targeted": ["sh", "-c", "true"],
                "timeout_s": 60,
            })),
            base_files: &["full"],
            head_removes: &["full"],
        });
        let r = check_run_tree("run-full", &wt, &repo, &base).unwrap();
        if unboxed(&r) {
            return;
        }
        assert!(
            r.report.checks.iter().any(|c| c.name == "test"),
            "the full test ran: {:?}",
            r.report
        );
        assert!(!r.passed, "the failing full suite denies: {:?}", r.report);
    }

    /// codex r1 (SILENT-OK): a floor that ran no check is not a pass — a re-delivery is never
    /// unverified — and its report gets no reusable key.
    #[test]
    fn a_floor_that_runs_no_check_does_not_pass() {
        let (repo, wt, base) = layout(Fixture {
            tag: "empty",
            checks: None,
            base_files: &[],
            head_removes: &[],
        });
        let r = check_run_tree("run-empty", &wt, &repo, &base).unwrap();
        if unboxed(&r) {
            return;
        }
        assert_eq!(r.checks_run, 0, "{:?}", r.report);
        assert!(!r.passed && !r.report.passed, "{:?}", r.report);
        assert!(r.report.key.is_none());
    }

    /// The inputs must name a rebased tree: a base that is not a full commit id, not a commit
    /// object, or not an ancestor of HEAD, and an empty run id, are refused before any check runs.
    #[test]
    fn inputs_that_do_not_name_a_rebased_tree_are_refused() {
        let (repo, wt, base) = simple("refuse", true);
        let err = |r: anyhow::Result<TreeRecheck>| r.err().map(|e| e.to_string()).unwrap();
        assert!(err(check_run_tree("", &wt, &repo, &base)).contains("run id"));
        assert!(err(check_run_tree("r", &wt, &repo, &base[..12])).contains("full commit id"));
        assert!(err(check_run_tree("r", &wt, &repo, &"0".repeat(40))).contains("not a commit"));
        // The base commit's tree is a full object id but not a commit.
        let tree = git(&repo, &["rev-parse", &format!("{base}^{{tree}}")]);
        assert!(err(check_run_tree("r", &wt, &repo, &tree)).contains("not a commit"));
        // The head is not an ancestor of the base: a later commit on main the branch never saw.
        std::fs::write(repo.join("later.txt"), "later\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "later on main"]);
        let later = git(&repo, &["rev-parse", "HEAD"]);
        assert!(err(check_run_tree("r", &wt, &repo, &later)).contains("not an ancestor"));
        // A worktree of another repository.
        let (other, _, _) = simple("refuse-other", true);
        assert!(check_run_tree("r", &wt, &other, &base).is_err());
    }
}
