//! FLOOR PROBE (DES-BUILD-GUIDE) — `wicked-core floor probe --repo <dir> --guide <file>
//! [--check <name>]`: every piece of a build guide checked against reality, exit codes the referee,
//! in a scratch copy of the repo's HEAD under the real floor sandbox. The results land in each
//! check's `verified`, and the floor relies on a check's exclusion only when every probe passed.
//!
//! The probes, per check:
//! * `list` — the list run exits 0 and names tests;
//! * `run` — the run's exit code agrees with its parsed failures (0 ⇔ none; else a `failedExit`);
//! * `mutation` — with `probes.mutation` written into the copy, the run fails with a `failedExit`
//!   and its parsed failures are the clean run's plus EXACTLY the expected id (and qualifier); the
//!   run deselecting it fails exactly as the clean run did; run alone it fails;
//! * `alone` — with the mutation present, a passing id run alone exits 0 and names no failure (and,
//!   with junit results, names exactly that test): alone runs no more than its id;
//! * `deselect` — the list deselecting that passing id lists exactly the rest;
//! * `affected` (when the check has the piece) — with the mutation file as `{files}`, its run's
//!   failures include the mutated id.

use std::path::Path;

use crate::floor_guide::{BuildGuide, GuideCheck, TestId};
use crate::repo_checks::{
    exec_of, inventory, remove_base_export, results_of, run_one_with, BaseTree, CheckScratch,
    RepoCheck, Tree,
};

/// A results run's parse: every testcase (junit) and the failures.
type Parsed = Result<(Vec<TestId>, Vec<TestId>), String>;

/// The probe results of one check: probe name → `pass` | `fail: <why>`.
pub type ProbeResults = std::collections::BTreeMap<String, String>;

/// Probe every check of the guide at `guide_path` (or only `only`) against `repo`'s HEAD, write
/// each probed check's `verified` back into the guide file, and return the results by check.
pub fn probe_guide(
    repo: &Path,
    guide_path: &Path,
    only: Option<&str>,
) -> Result<Vec<(String, ProbeResults)>, String> {
    // Absolute: the sandbox's write roots and every run's cwd hang off it.
    let repo = &repo
        .canonicalize()
        .map_err(|e| format!("`{}`: {e}", repo.display()))?;
    let raw = std::fs::read_to_string(guide_path)
        .map_err(|e| format!("`{}` could not be read: {e}", guide_path.display()))?;
    let mut guide: BuildGuide =
        serde_json::from_str(&raw).map_err(|e| format!("the guide does not parse: {e}"))?;
    if guide.version != 1 {
        return Err(format!(
            "guide version {} (this probe reads 1)",
            guide.version
        ));
    }
    let git_dir =
        crate::worktree_guard::git_string(repo, &["rev-parse", "--absolute-git-dir"], &[])
            .map(std::path::PathBuf::from)
            .map_err(|e| format!("`{}` is not a git repository: {e}", repo.display()))?;
    let head = crate::worktree_guard::git_string(repo, &["rev-parse", "HEAD"], &[])
        .map_err(|e| format!("the repo's HEAD: {e}"))?;
    let mut out = Vec::new();
    for c in guide.checks.iter_mut() {
        if only.is_some_and(|n| n != c.name) {
            continue;
        }
        let results = probe_check(repo, &git_dir, &head, c)?;
        let v = c.verified.get_or_insert_with(Default::default);
        v.base_sha = head.clone();
        v.at = now_rfc3339();
        v.results = results.clone();
        out.push((c.name.clone(), results));
    }
    if out.is_empty() {
        return Err("no check of the guide matched".into());
    }
    let json = serde_json::to_string_pretty(&guide).map_err(|e| e.to_string())?;
    std::fs::write(guide_path, json + "\n")
        .map_err(|e| format!("`{}` could not be written: {e}", guide_path.display()))?;
    Ok(out)
}

/// The CLI: `floor probe --repo <dir> --guide <file> [--check <name>]`. Prints each probe's result;
/// exit 0 only when every probe of every probed check passed.
pub fn probe_cli(args: &[String]) -> i32 {
    let flag = |f: &str| {
        args.iter()
            .position(|a| a == f)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let (Some(repo), Some(guide)) = (flag("--repo"), flag("--guide")) else {
        eprintln!("usage: wicked-core floor probe --repo <dir> --guide <file> [--check <name>]");
        return 2;
    };
    match probe_guide(
        Path::new(&repo),
        Path::new(&guide),
        flag("--check").as_deref(),
    ) {
        Ok(all) => {
            let mut ok = true;
            for (check, results) in all {
                for (probe, r) in results {
                    ok &= r == "pass";
                    println!("{check}\t{probe}\t{r}");
                }
            }
            i32::from(!ok)
        }
        Err(e) => {
            eprintln!("wicked-core floor probe: {e}");
            2
        }
    }
}

fn probe_check(
    repo: &Path,
    git_dir: &Path,
    head: &str,
    g: &GuideCheck,
) -> Result<ProbeResults, String> {
    g.compile()?;
    let scratch = CheckScratch::prepare_for(repo, Some("floor-probe"))
        .map_err(|e| format!("the probe's scratch: {e}"))?;
    let tree = BaseTree::export(repo, &scratch, git_dir, head)?;
    let sandbox = crate::repo_checks::floor_sandbox(&[
        tree.dir.clone(),
        scratch.tmp_path().to_path_buf(),
        scratch.root().to_path_buf(),
    ]);
    if sandbox.level != crate::validator::SandboxLevel::Sandboxed {
        let _ = remove_base_export(repo, &scratch);
        return Err(format!(
            "the floor sandbox does not arm here ({})",
            sandbox
                .downgrade_reason
                .as_deref()
                .unwrap_or("no boundary tool")
        ));
    }
    let results = Probe {
        dir: &tree.dir,
        g,
        rc: RepoCheck {
            name: g.name.clone(),
            argv: g.run.clone(),
            source: "floor probe (DES-BUILD-GUIDE)".into(),
            timeout_s: Some(3600),
            dir: g.dir().to_string(),
        },
        sandbox: &sandbox,
        scratch: &scratch,
    }
    .all();
    let _ = remove_base_export(repo, &scratch);
    Ok(results)
}

struct Probe<'a> {
    dir: &'a Path,
    g: &'a GuideCheck,
    rc: RepoCheck,
    sandbox: &'a crate::validator::WorkerSandbox,
    scratch: &'a CheckScratch,
}

fn verdict(ok: bool, why: impl FnOnce() -> String) -> String {
    if ok {
        "pass".into()
    } else {
        format!("fail: {}", why())
    }
}

impl Probe<'_> {
    /// A results run of `argv` (with `how`): its exit code, every testcase (junit), its failures.
    fn results(&self, argv: &[String]) -> (Option<i32>, Parsed) {
        let rc = RepoCheck {
            argv: self.g.for_results(argv),
            ..self.rc.clone()
        };
        let mut r = run_one_with(
            self.dir,
            &rc,
            &exec_of(self.g, self.g.results.how.merge),
            self.sandbox,
            self.scratch,
            Tree::Base,
        );
        if r.timed_out || r.spawn_error.is_some() {
            return (None, Err(format!("did not finish ({})", r.summary())));
        }
        let parsed = results_of(self.dir, &self.rc, self.g, &mut r, self.scratch);
        (r.exit_code, parsed)
    }

    fn exit_of(&self, argv: Vec<String>) -> Option<i32> {
        let rc = RepoCheck {
            argv,
            ..self.rc.clone()
        };
        run_one_with(
            self.dir,
            &rc,
            &exec_of(self.g, false),
            self.sandbox,
            self.scratch,
            Tree::Base,
        )
        .exit_code
    }

    /// `failed`'s ids beyond `before` that still fail run alone (a flake that passes alone is set
    /// aside).
    fn steady(&self, failed: &[TestId], before: &[TestId]) -> Vec<TestId> {
        failed
            .iter()
            .filter(|t| !before.contains(t))
            .filter(|t| self.failed_exit(self.exit_of(self.g.alone_argv(t))))
            .cloned()
            .collect()
    }

    fn failed_exit(&self, code: Option<i32>) -> bool {
        code.is_some_and(|c| self.g.failed_exit.contains(&c))
    }

    fn all(&self) -> ProbeResults {
        let mut out = ProbeResults::new();
        let g = self.g;
        let list = inventory(
            self.dir,
            &self.rc,
            g,
            &[],
            self.sandbox,
            self.scratch,
            Tree::Base,
        );
        out.insert(
            "list".into(),
            verdict(list.incomplete.is_none(), || {
                list.incomplete.clone().unwrap_or_default()
            }),
        );
        let (exit, clean) = self.results(&g.run);
        let clean_failed = clean.as_ref().map(|(_, f)| f.clone()).unwrap_or_default();
        out.insert(
            "run".into(),
            match &clean {
                Err(e) => format!("fail: {e}"),
                Ok((_, f)) => verdict(
                    if exit == Some(0) {
                        f.is_empty()
                    } else {
                        !f.is_empty() && self.failed_exit(exit)
                    },
                    || format!("exit {exit:?} with {} parsed failure(s)", f.len()),
                ),
            },
        );
        for (piece, present) in [
            ("alone", g.alone.is_some()),
            ("deselect", g.deselect.is_some()),
        ] {
            if !present {
                for p in ["mutation", piece] {
                    out.insert(p.into(), format!("fail: no `{piece}` piece"));
                }
            }
        }
        if out.len() > 2 {
            return out;
        }
        let Some(m) = g.probes.mutation.as_ref() else {
            for p in ["mutation", "alone", "deselect"] {
                out.insert(p.into(), "fail: no `probes.mutation`".into());
            }
            return out;
        };
        let file = self.dir.join(g.dir()).join(&m.file);
        if let Err(e) = file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&file, &m.content))
        {
            out.insert(
                "mutation".into(),
                format!("fail: the mutation could not be written: {e}"),
            );
            return out;
        }
        // The mutation: its run fails with exactly one new failure, the expected one. A suite's
        // other new failures that PASS when run alone are its flakes, not the parser's mistakes,
        // and are set aside (a red or flaky suite still proves its parser this way).
        let (mexit, mutated) = self.results(&g.run);
        let new: Vec<TestId> = mutated
            .as_ref()
            .map(|(_, f)| self.steady(f, &clean_failed))
            .unwrap_or_default();
        let expected = new
            .first()
            .filter(|t| {
                new.len() == 1
                    && t.id == m.expect_id
                    && m.expect_q.as_ref().is_none_or(|q| *q == t.q)
            })
            .cloned();
        let mutation = match (&mutated, &expected) {
            (Err(e), _) => format!("fail: the mutated run's results: {e}"),
            (_, None) => format!(
                "fail: the mutated run's new failures are {new:?}, not `{}`",
                m.expect_id
            ),
            _ if !self.failed_exit(mexit) => format!("fail: the mutated run exited {mexit:?}"),
            (_, Some(t)) => {
                let (dexit, d) = self.results(&g.deselected(&g.run, std::slice::from_ref(t)));
                let deselected_ok = d
                    .as_ref()
                    .is_ok_and(|(_, f)| !f.contains(t) && self.steady(f, &clean_failed).is_empty())
                    && (!clean_failed.is_empty() || dexit == Some(0));
                if !deselected_ok {
                    format!("fail: deselecting `{}` did not leave the clean run's failures (exit {dexit:?})", t.id)
                } else if !self.failed_exit(self.exit_of(g.alone_argv(t))) {
                    format!("fail: `{}` run alone does not fail", t.id)
                } else {
                    "pass".into()
                }
            }
        };
        out.insert("mutation".into(), mutation);
        // alone + deselect precision, with the mutation present.
        let listed = inventory(
            self.dir,
            &self.rc,
            g,
            &[],
            self.sandbox,
            self.scratch,
            Tree::Base,
        );
        let failing: Vec<TestId> = mutated.as_ref().map(|(_, f)| f.clone()).unwrap_or_default();
        let passing = listed
            .ids
            .iter()
            .find(|t| !failing.iter().any(|f| f.id == t.id))
            .cloned();
        match passing {
            None => {
                out.insert(
                    "alone".into(),
                    "fail: the list names no passing test".into(),
                );
                out.insert(
                    "deselect".into(),
                    "fail: the list names no passing test".into(),
                );
            }
            Some(p) => {
                let (aexit, alone) = self.results(&g.alone_argv(&p));
                out.insert(
                    "alone".into(),
                    match alone {
                        Err(e) => format!("fail: {e}"),
                        Ok((all, f)) => verdict(
                            aexit == Some(0)
                                && f.is_empty()
                                && (g.results.kind != "junit"
                                    || all.iter().all(|t| t.id == p.id) && !all.is_empty()),
                            || format!("`{}` alone exited {aexit:?}, ran {} test(s), {} failure(s)", p.id, all.len(), f.len()),
                        ),
                    },
                );
                let rest = inventory(
                    self.dir,
                    &self.rc,
                    g,
                    std::slice::from_ref(&p),
                    self.sandbox,
                    self.scratch,
                    Tree::Base,
                );
                let mut want: Vec<TestId> =
                    listed.ids.iter().filter(|t| **t != p).cloned().collect();
                let mut got = rest.ids.clone();
                want.sort();
                got.sort();
                out.insert(
                    "deselect".into(),
                    verdict(rest.incomplete.is_none() && got == want, || {
                        format!(
                            "the list deselecting `{}` lists {} test(s), not the {} others{}",
                            p.id,
                            got.len(),
                            want.len(),
                            rest.incomplete
                                .as_deref()
                                .map(|i| format!(" ({i})"))
                                .unwrap_or_default()
                        )
                    }),
                );
            }
        }
        if let (Some(argv), Some(t)) = (g.affected_argv(std::slice::from_ref(&m.file)), &expected) {
            let (_, a) = self.results(&argv);
            out.insert(
                "affected".into(),
                verdict(a.as_ref().is_ok_and(|(_, f)| f.contains(t)), || {
                    format!("the affected run's failures do not include `{}`", t.id)
                }),
            );
        } else if g.affected.is_some() {
            out.insert(
                "affected".into(),
                "fail: the mutation probe did not establish the id".into(),
            );
        }
        out
    }
}

/// Now, as `YYYY-MM-DDTHH:MM:SSZ`.
fn now_rfc3339() -> String {
    let s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let (days, rem) = ((s / 86_400) as i64, s % 86_400);
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}
