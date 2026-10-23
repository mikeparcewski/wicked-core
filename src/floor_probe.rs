//! FLOOR PROBE (DES-BUILD-GUIDE) — `wicked-core floor probe --repo <dir> --guide <file>
//! [--check <name>]`: every piece of a build guide checked against reality, exit codes the referee,
//! in a scratch copy of the repo's HEAD under the real floor sandbox. The results land in each
//! check's `verified`, and the floor relies on a check's exclusion only when every probe passed.
//!
//! The probes, per check (each on a scratch copy; the mutations are written inside the check's
//! directory, never through a link):
//! * `list` — the list run exits 0 and names tests;
//! * `run` — the clean run is GREEN (exit 0, no parsed failure): every later probe needs a green
//!   base, so a mutation's failure is the run's only one and a deselected run that exits 0 does so
//!   because of the deselect;
//! * `mutation` — with `probes.mutation` written, the run fails with a `failedExit` naming EXACTLY
//!   the expected test, and the run deselecting it is green;
//! * `multi` — with `probes.mutation2` written too, the run names exactly both; deselecting both
//!   is green, and the list deselecting both lists exactly the rest;
//! * `ids` — every id the results report is in the list;
//! * `alone` — the mutated test alone fails naming exactly it (with junit, exactly one testcase
//!   ran), and a passing test under its qualifier alone is green (with junit, exactly it ran);
//! * `deselect` — the list deselecting that passing test lists exactly the rest;
//! * `affected` (when the check has the piece) — with the mutation file as `{files}`, its run's
//!   failures include the mutated test;
//! * `version` (when the check has the piece) — the command passes and prints a line, recorded as
//!   `verified.runnerVersion`.
//!
//! A probe that fails re-runs the whole probe once, on a fresh copy, and the retry's results are the
//! result, each marked `(retried)`: a suite too flaky to pass twice is left unverified. A check with
//! no `list` piece (a build, a lint) has nothing to probe and is skipped.
//!
//! Its worker home (check scratch, log store) is `WICKED_WORKER_HOME` when set explicitly, else a
//! fresh temp dir removed at the end — never the operator's default home.

use std::path::Path;

use crate::floor_guide::{passed, BuildGuide, GuideCheck, Mutation, TestId};
use crate::repo_checks::{
    exec_of, inventory, remove_base_export, results_of, run_one_with, runner_version, BaseTree,
    CheckScratch, RepoCheck, Tree,
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
        if only.is_none() && c.list.is_none() {
            eprintln!(
                "wicked-core floor probe: `{}` has no `list` piece (not a test check): skipped",
                c.name
            );
            continue;
        }
        let (results, version) = probe_check(repo, &git_dir, &head, c)?;
        let v = c.verified.get_or_insert_with(Default::default);
        v.base_sha = head.clone();
        v.runner_version = version;
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
    // Held to the end of the probe: dropping it removes the temporary worker home.
    let _home = match probe_worker_home() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("wicked-core floor probe: no temporary worker home: {e}");
            return 2;
        }
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
                    ok &= passed(&r);
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

/// The probe never runs in the operator's worker home: unless `WICKED_WORKER_HOME` is set
/// explicitly, the run gets a fresh private dir under the system temp dir (the variable points at
/// it for this process, before anything resolves the home) and the returned guard removes it.
fn probe_worker_home() -> std::io::Result<Option<crate::repo_checks::PrivateTmp>> {
    use wicked_apps_core::spawn::WORKER_HOME_ENV;
    if std::env::var_os(WORKER_HOME_ENV).is_some() {
        return Ok(None);
    }
    let tmp = crate::repo_checks::PrivateTmp::create()?;
    // Spelled without a symlink (`/var` → `/private/var` on macOS): the home must be normal.
    let dir = tmp.path().canonicalize()?;
    std::env::set_var(WORKER_HOME_ENV, &dir);
    eprintln!(
        "wicked-core floor probe: worker home {} (temporary; removed at the end)",
        dir.display()
    );
    Ok(Some(tmp))
}

fn probe_check(
    repo: &Path,
    git_dir: &Path,
    head: &str,
    g: &GuideCheck,
) -> Result<(ProbeResults, String), String> {
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
    let rc = RepoCheck {
        name: g.name.clone(),
        argv: g.run.clone(),
        source: "floor probe (DES-BUILD-GUIDE)".into(),
        timeout_s: Some(3600),
        dir: g.dir().to_string(),
    };
    // One attempt of every probe on `dir` (a fresh copy): the results and the runner version.
    let attempt = |dir: &Path| {
        let mut out = Probe {
            dir,
            g,
            rc: rc.clone(),
            sandbox: &sandbox,
            scratch: &scratch,
        }
        .all();
        let mut version = String::new();
        if g.version.is_some() {
            let v = runner_version(dir, &rc, g, &sandbox, &scratch, Tree::Base);
            out.insert(
                "version".into(),
                verdict(v.is_ok(), || v.clone().unwrap_err()),
            );
            version = v.unwrap_or_default();
        }
        (out, version)
    };
    let (mut results, mut version) = attempt(&tree.dir);
    // A failed probe re-runs the WHOLE probe once, on a fresh copy, and the retry's results are
    // the result (a missing piece is not retried: it cannot pass).
    if results
        .values()
        .any(|r| !passed(r) && !r.starts_with("fail: no `"))
    {
        remove_base_export(repo, &scratch)?;
        let again = BaseTree::export(repo, &scratch, git_dir, head)?;
        let (second, v2) = attempt(&again.dir);
        results = second
            .into_iter()
            .map(|(p, r)| (p, format!("{r} (retried)")))
            .collect();
        version = v2;
    }
    let _ = remove_base_export(repo, &scratch);
    Ok((results, version))
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

/// The probes that need a green base, both mutations and the `alone`/`deselect` pieces.
const AFTER_RUN: [&str; 5] = ["ids", "alone", "deselect", "mutation", "multi"];

impl Probe<'_> {
    /// A results run of `argv` (with `how`): its exit code, every testcase (junit), its failures.
    fn results(&self, argv: &[String]) -> (Option<i32>, Parsed) {
        let rc = RepoCheck {
            argv: self.g.for_results(argv),
            ..self.rc.clone()
        };
        crate::repo_checks::clear_results(self.dir, &self.rc, self.g);
        let since = std::time::SystemTime::now();
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
        let parsed = results_of(self.dir, &self.rc, self.g, &mut r, self.scratch, since);
        (r.exit_code, parsed)
    }

    /// A results run that exits 0 and names no failure.
    fn green(&self, argv: &[String]) -> bool {
        let (exit, r) = self.results(argv);
        exit == Some(0) && r.is_ok_and(|(_, f)| f.is_empty())
    }

    /// The list (with `ids` deselected).
    fn list(&self, ids: &[TestId]) -> crate::repo_checks::Toc {
        inventory(
            self.dir,
            &self.rc,
            self.g,
            ids,
            self.sandbox,
            self.scratch,
            Tree::Base,
        )
    }

    /// The list deselecting `ids` names exactly `listed` minus them.
    fn deselects_exactly(&self, listed: &[TestId], ids: &[TestId]) -> bool {
        let rest = self.list(ids);
        let mut want: Vec<&TestId> = listed.iter().filter(|t| !ids.contains(t)).collect();
        let mut got: Vec<&TestId> = rest.ids.iter().collect();
        want.sort();
        got.sort();
        rest.incomplete.is_none() && got == want
    }

    /// Write a mutation into the copy, inside the check's directory and through no link.
    fn write(&self, m: &Mutation) -> Result<(), String> {
        let rel = Path::new(self.g.dir()).join(&m.file);
        crate::repo_checks::write_confined(self.dir, &rel.to_string_lossy(), &m.content)
    }

    fn failed_exit(&self, code: Option<i32>) -> bool {
        code.is_some_and(|c| self.g.failed_exit.contains(&c))
    }

    fn all(&self) -> ProbeResults {
        let mut out = ProbeResults::new();
        let fail_rest = |out: &mut ProbeResults, why: &str| {
            for p in AFTER_RUN {
                out.insert(p.into(), format!("fail: {why}"));
            }
        };
        let g = self.g;
        let clean_list = self.list(&[]);
        out.insert(
            "list".into(),
            verdict(clean_list.incomplete.is_none(), || {
                clean_list.incomplete.clone().unwrap_or_default()
            }),
        );
        // A green base: the mutations' failures are then the run's only failures, and a deselected
        // run that exits 0 does so because of the deselect.
        let (exit, clean) = self.results(&g.run);
        let green = exit == Some(0) && clean.as_ref().is_ok_and(|(_, f)| f.is_empty());
        out.insert(
            "run".into(),
            verdict(green, || match &clean {
                Err(e) => e.clone(),
                Ok((_, f)) => format!(
                    "needs a green base: the clean run exited {exit:?} with {} parsed failure(s)",
                    f.len()
                ),
            }),
        );
        for (piece, present) in [
            ("alone", g.alone.is_some()),
            ("deselect", g.deselect.is_some()),
            ("probes.mutation", g.probes.mutation.is_some()),
            ("probes.mutation2", g.probes.mutation2.is_some()),
        ] {
            if !present {
                fail_rest(&mut out, &format!("no `{piece}`"));
                return out;
            }
        }
        if !green {
            fail_rest(&mut out, "needs a green base");
            return out;
        }
        let (m, m2) = (
            g.probes.mutation.as_ref().expect("checked"),
            g.probes.mutation2.as_ref().expect("checked"),
        );
        let is = |t: &TestId, m: &Mutation| {
            t.id == m.expect_id && m.expect_q.as_ref().is_none_or(|q| *q == t.q)
        };
        // mutation: the run fails naming EXACTLY the expected test; deselecting it runs green.
        if let Err(e) = self.write(m) {
            fail_rest(&mut out, &format!("the mutation could not be written: {e}"));
            return out;
        }
        let (mexit, mutated) = self.results(&g.run);
        let t = match &mutated {
            Ok((_, f)) if self.failed_exit(mexit) && f.len() == 1 && is(&f[0], m) => f[0].clone(),
            _ => {
                fail_rest(
                    &mut out,
                    &format!(
                        "the mutated run (exit {mexit:?}) named {:?}, not exactly `{}`",
                        mutated.as_ref().map(|(_, f)| f.clone()).unwrap_or_default(),
                        m.expect_id
                    ),
                );
                return out;
            }
        };
        out.insert(
            "mutation".into(),
            verdict(
                self.green(&g.deselected(&g.run, std::slice::from_ref(&t))),
                || format!("deselecting `{}` did not run green", t.id),
            ),
        );
        // multi: both mutations fail, named exactly; deselecting both runs green and lists exactly
        // the rest.
        if let Err(e) = self.write(m2) {
            fail_rest(
                &mut out,
                &format!("the second mutation could not be written: {e}"),
            );
            return out;
        }
        let listed = self.list(&[]);
        let (bexit, both) = self.results(&g.run);
        let pair: Vec<TestId> = match &both {
            Ok((_, f))
                if self.failed_exit(bexit)
                    && f.len() == 2
                    && f.contains(&t)
                    && f.iter().any(|x| is(x, m2)) =>
            {
                f.clone()
            }
            _ => Vec::new(),
        };
        out.insert(
            "multi".into(),
            verdict(
                !pair.is_empty()
                    && self.green(&g.deselected(&g.run, &pair))
                    && self.deselects_exactly(&listed.ids, &pair),
                || {
                    format!(
                        "with both mutations: {:?} (exit {bexit:?}); both must be named, and \
                         deselecting both must run green and list exactly the rest",
                        both.as_ref().map(|(_, f)| f.clone()).unwrap_or_default()
                    )
                },
            ),
        );
        // ids: every id the results report is the list's.
        let reported: Vec<&TestId> = match (&clean, &both) {
            (Ok((a, _)), Ok((b, f))) => a.iter().chain(b).chain(f).collect(),
            _ => Vec::new(),
        };
        let stray = reported.iter().find(|x| !listed.ids.contains(x)).copied();
        out.insert(
            "ids".into(),
            verdict(
                listed.incomplete.is_none() && stray.is_none(),
                || match stray {
                    Some(x) => format!(
                        "the results name `{}` (q `{}`), which the list does not",
                        x.id, x.q
                    ),
                    None => listed.incomplete.clone().unwrap_or_default(),
                },
            ),
        );
        // alone: the mutated test alone fails naming exactly it (with junit, exactly one testcase
        // ran); a passing test under its qualifier alone runs green (with junit, exactly it).
        let exactly = |all: &[TestId], f: &[TestId], want: &TestId, failing: bool| {
            (if failing {
                f == std::slice::from_ref(want)
            } else {
                f.is_empty()
            }) && (g.results.kind != "junit" || all == std::slice::from_ref(want))
        };
        let (aexit, alone_t) = self.results(&g.alone_argv(&t));
        let passing = listed
            .ids
            .iter()
            .find(|x| x.q == t.q && **x != t && !is(x, m2))
            .cloned();
        out.insert(
            "alone".into(),
            match (&passing, &alone_t) {
                (_, Ok((all, f))) if !(self.failed_exit(aexit) && exactly(all, f, &t, true)) => {
                    format!(
                        "fail: `{}` alone (exit {aexit:?}) did not fail naming exactly it",
                        t.id
                    )
                }
                (_, Err(e)) => format!("fail: `{}` alone: {e}", t.id),
                (None, _) => format!(
                    "fail: no passing test beside `{}` under its qualifier",
                    t.id
                ),
                (Some(p), _) => {
                    let (pexit, alone_p) = self.results(&g.alone_argv(p));
                    verdict(
                        pexit == Some(0)
                            && alone_p
                                .as_ref()
                                .is_ok_and(|(all, f)| exactly(all, f, p, false)),
                        || {
                            format!(
                                "`{}` alone (exit {pexit:?}) did not run exactly it, green",
                                p.id
                            )
                        },
                    )
                }
            },
        );
        // deselect: the list deselecting one passing test lists exactly the rest.
        out.insert(
            "deselect".into(),
            match &passing {
                None => format!(
                    "fail: no passing test beside `{}` under its qualifier",
                    t.id
                ),
                Some(p) => verdict(
                    self.deselects_exactly(&listed.ids, std::slice::from_ref(p)),
                    || {
                        format!(
                            "the list deselecting `{}` does not list exactly the rest",
                            p.id
                        )
                    },
                ),
            },
        );
        if g.affected.is_some() {
            let a = g
                .affected_argv(std::slice::from_ref(&m.file))
                .map(|argv| self.results(&argv).1);
            out.insert(
                "affected".into(),
                verdict(
                    a.is_some_and(|r| r.is_ok_and(|(_, f)| f.contains(&t))),
                    || format!("the affected run's failures do not include `{}`", t.id),
                ),
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
