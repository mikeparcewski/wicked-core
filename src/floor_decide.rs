//! (core#1100) Moved verbatim from `repo_checks.rs` (no behaviour change): the floor's per-check decision, retries, claim judging and the one-check runner.

use super::*;

/// (core#1187) The engine's affected set for `check` at the creator floor
/// ([`crate::floor_affected`]), or `None` when `check` is not a cargo workspace test at the root.
#[allow(clippy::too_many_arguments)]
pub(super) fn engine_affected(
    worktree: &Path,
    held: Option<&HeldDir>,
    check: &RepoCheck,
    ctx: &FloorContext,
    touched: &[String],
    removed: &Result<Vec<String>, String>,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
) -> Option<crate::floor_affected::Plan> {
    use crate::floor_affected::Plan;
    let cargo = crate::floor_affected::cargo_workspace_test(&check.argv)?;
    if !(check.dir.is_empty() || check.dir == ".") {
        return None;
    }
    if power_full_suite(ctx) {
        return Some(Plan::full(
            "a power run at high rigor or above hands off on the full suite (DES-POWER-MODE)",
        ));
    }
    let (Some(base), Some(git_dir)) = (ctx.base_head.as_deref(), ctx.git_dir.as_deref()) else {
        return Some(Plan::full(
            "the run has no base to measure the change against",
        ));
    };
    let removed = match removed {
        Ok(r) => r.clone(),
        Err(e) => {
            return Some(Plan::full(format!(
                "the change's removed files could not be listed: {e}"
            )))
        }
    };
    // Cargo's own description of the workspace, run like every check (sandboxed, isolated home).
    let mut metadata = || {
        let meta = RepoCheck {
            name: format!("{} (cargo metadata)", check.name),
            argv: crate::floor_affected::METADATA_ARGV
                .iter()
                .map(|a| a.to_string())
                .collect(),
            source: "the engine's affected set (core#1187)".into(),
            timeout_s: Some(300),
            dir: String::new(),
        };
        let r = as_probe(|| {
            run_one_with(
                worktree,
                held,
                &meta,
                &merged(false),
                sandbox,
                scratch,
                Tree::Head,
            )
        });
        if !r.passed() {
            return Err(format!("`cargo metadata` did not pass ({})", r.summary()));
        }
        r.logs
            .iter()
            .find(|l| l.stream == "stdout")
            .ok_or_else(|| "cargo metadata left no output".to_string())
            .and_then(|l| crate::floor_log::read_blob(&scratch.logs, l))
            .and_then(|b| crate::floor_affected::parse_metadata(&String::from_utf8_lossy(&b)))
    };
    Some(crate::floor_affected::plan_for_tree(
        &crate::floor_affected::Tree {
            worktree,
            git_dir,
            base,
            touched,
            removed: &removed,
            graph: ctx.code_graph.as_deref(),
        },
        &cargo,
        &mut metadata,
    ))
}

/// (DES-BUILD-GUIDE) The base guide's check for `check`, when its pieces may be relied on for
/// this head; `Err` says why not (no guide, no such check, a probe not passed, stale, invalidated).
pub(super) fn guide_for<'g>(
    guide: &'g Result<Option<crate::floor_guide::BuildGuide>, String>,
    check: &RepoCheck,
    ctx: &FloorContext,
    worktree: &Path,
    changed: &Result<Vec<String>, String>,
) -> Result<&'g GuideCheck, String> {
    let g = match guide {
        Err(e) => return Err(e.clone()),
        Ok(None) => {
            return Err(format!(
                "the base has no `{}`",
                crate::floor_guide::GUIDE_PATH
            ))
        }
        Ok(Some(g)) => g,
    };
    let c = g
        .check_for(&check.argv, &check.dir)
        .ok_or("the base's guide has no check with this argv and cwd")?;
    let (Some(base), Some(git_dir)) = (ctx.base_head.as_deref(), ctx.git_dir.as_deref()) else {
        return Err("no base to verify the guide against".into());
    };
    let changed = changed
        .as_ref()
        .map_err(|e| format!("the change's files could not be listed: {e}"))?;
    c.usable(g, worktree, git_dir, base, changed)?;
    Ok(c)
}

/// (DES-BUILD-GUIDE) A failed run whose exit code is neither 0 nor one of the guide's
/// `failedExit` codes did not fail tests — it crashed or did not build: why, or `None`.
pub(super) fn crashed(run: &CheckRun, g: Option<&GuideCheck>) -> Option<String> {
    let g = g.filter(|g| !g.failed_exit.is_empty())?;
    let code = run
        .exit_code
        .filter(|c| *c != 0 && !g.failed_exit.contains(c))?;
    Some(format!(
        "exit {code} is not one of the guide's `failedExit` codes {:?}: a crash or a build \
         failure, not a test failure",
        g.failed_exit
    ))
}

/// (DES-BUILD-GUIDE) A check that declares a `version` command is relied on only while the runner
/// still reports the version its probes ran under (`verified.runnerVersion`).
pub(super) fn runner_current(
    worktree: &Path,
    held: Option<&HeldDir>,
    check: &RepoCheck,
    g: &GuideCheck,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
) -> Result<(), String> {
    if g.version.is_none() {
        return Ok(());
    }
    let now = runner_version(worktree, held, check, g, sandbox, scratch, Tree::Head)?;
    let then = g
        .verified
        .as_ref()
        .map_or("", |v| v.runner_version.as_str());
    if now != then {
        return Err(format!(
            "stale: the runner reports `{now}`, the guide was verified under `{then}`"
        ));
    }
    Ok(())
}

/// The first line the guide's `version` command prints (stdout and stderr merged) on `tree`.
pub(crate) fn runner_version(
    dir: &Path,
    held: Option<&HeldDir>,
    check: &RepoCheck,
    g: &GuideCheck,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
    tree: Tree,
) -> Result<String, String> {
    let v = g.version.as_ref().ok_or("no `version` piece")?;
    let rc = RepoCheck {
        argv: v.run.clone(),
        source: format!("`{}`'s runner version (DES-BUILD-GUIDE)", check.name),
        timeout_s: Some(120),
        ..check.clone()
    };
    let r = as_probe(|| run_one_with(dir, held, &rc, &merged(true), sandbox, scratch, tree));
    if !r.passed() {
        return Err(format!(
            "the version command did not pass ({})",
            r.summary()
        ));
    }
    let text = match r.logs.first() {
        Some(log) => crate::floor_log::read_blob(&scratch.logs, log)?,
        None => Vec::new(),
    };
    String::from_utf8_lossy(&text)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "the version command printed nothing".to_string())
}

pub(super) fn relabel(
    mut logs: Vec<crate::floor_log::LogRef>,
    attempt: &str,
) -> Vec<crate::floor_log::LogRef> {
    for l in &mut logs {
        l.attempt = attempt.to_string();
    }
    logs
}

/// (DES-FLOOR-V2 §4) Was the host oversubscribed when `run` started (its load band)?
pub(super) fn ran_loaded(run: &CheckRun) -> bool {
    // The run's OWN load band, sampled as it started — never the load after other runs (the
    // base's) raised it.
    run.load_band.is_some_and(|b| b >= 1)
}

/// (DES-FLOOR-V2 §4) Bazel's `flaky_test_attempts=2`: when the deciding run FAILED (not a
/// timeout, not a spawn failure) on an oversubscribed host, wait (bounded) for the 1-min load to
/// fall below the CPU count, then run the SAME argv once more. `None` when no retry was due.
pub(super) fn retry_if_loaded(
    worktree: &Path,
    held: Option<&HeldDir>,
    one: &RepoCheck,
    exec: &ExecOpts,
    first: &CheckRun,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
) -> Option<CheckRun> {
    retry_labelled(worktree, held, one, exec, first, sandbox, scratch, None)
}

/// [`retry_if_loaded`], running `one` (the failing set, when `scope` says so) once the HEAD run
/// `first` is due a retry.
#[allow(clippy::too_many_arguments)]
pub(super) fn retry_labelled(
    worktree: &Path,
    held: Option<&HeldDir>,
    one: &RepoCheck,
    exec: &ExecOpts,
    first: &CheckRun,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
    scope: Option<&RunScope>,
) -> Option<CheckRun> {
    if first.passed() || first.timed_out || first.spawn_error.is_some() || !ran_loaded(first) {
        return None;
    }
    let waited = Instant::now();
    let bound = retry_headroom_wait();
    while waited.elapsed() < bound {
        let (load1, cpus) = host_load();
        if !oversubscribed(load1, cpus) {
            break;
        }
        std::thread::sleep(Duration::from_secs(1).min(bound));
    }
    // The wait alone: measured before the retry runs, not after it (rig run b6f7968b's label
    // said "after waiting 1916s" for a 30 s bound — the retry's own run time).
    let waited_s = waited.elapsed().as_secs_f64();
    let mut r = run_one_with(worktree, held, one, exec, sandbox, scratch, Tree::Head);
    r.logs = relabel(r.logs, "retry");
    r.source = match scope {
        None => format!(
            "the identical retry of `{}` after waiting {:.0}s for headroom (DES-FLOOR-V2)",
            one.name, waited_s
        ),
        Some(s) => format!(
            "the retry of `{}`'s {} failing {}{} only, after waiting {:.0}s for headroom (core#1198)",
            one.name,
            s.ids.len(),
            s.by,
            if s.ids.len() == 1 { "" } else { "s" },
            waited_s
        ),
    };
    r.scope = scope.cloned();
    eprintln!(
        "wicked-core: repo checks floor — `{}` failed under load; its {} retry {}",
        one.name,
        if scope.is_some() {
            "failing-set"
        } else {
            "identical"
        },
        if r.passed() { "passed" } else { "failed" }
    );
    Some(r)
}

/// (core#1198) A failed check's failing set, ready to re-run through the repo's declared `rerun`
/// command ([`floor_scoped`]).
pub(super) struct Scoped {
    pub(super) by: floor_scoped::RerunBy,
    /// The directory the set's files are relative to, below the check's own.
    pub(super) root: String,
    /// The check with the declared command over the failing set (same name, dir and bound).
    pub(super) check: RepoCheck,
    pub(super) scope: RunScope,
    /// Does the head's own count account for every failure the set names?
    pub(super) complete: Result<(), String>,
}

/// A run's two streams, read back whole from the log store (`Err`: unreadable or cut at the cap).
pub(super) fn run_text(run: &CheckRun, scratch: &CheckScratch) -> Result<(String, String), String> {
    let (Some(out), Some(err)) = (run.logs.first(), run.logs.get(1)) else {
        return Err("its output was not stored".into());
    };
    if out.truncated || err.truncated {
        return Err("its output was cut at the log cap".into());
    }
    Ok((
        crate::floor_log::text(&scratch.logs, out)?,
        crate::floor_log::text(&scratch.logs, err)?,
    ))
}

impl Scoped {
    /// The failing set of the failed head run `head`, when the repo declares a `rerun` command for
    /// `check`; `Some(Err(why))` when it does but the set cannot be read off the head.
    pub(super) fn of(
        check: &RepoCheck,
        decl: &floor_scoped::RerunDecl,
        head: &CheckRun,
        scratch: &CheckScratch,
    ) -> Option<Result<Self, String>> {
        let (command, by) = decl.command.as_ref()?;
        let by = *by;
        Some((|| {
            let ids = floor_scoped::select(by, &head.failure_ids)?;
            let (out, err) = run_text(head, scratch).map_err(|e| format!("the head: {e}"))?;
            let complete = floor_scoped::head_complete(by, &out, &err, &ids);
            Ok(Scoped {
                by,
                root: decl.root.clone(),
                check: RepoCheck {
                    argv: floor_scoped::scoped_argv(command, by, &ids),
                    source: format!(
                        "{CONFIG_PATH} rerun.{} over the {} failing {}{}",
                        check.name,
                        ids.len(),
                        by.as_wire(),
                        if ids.len() == 1 { "" } else { "s" }
                    ),
                    ..check.clone()
                },
                scope: floor_scoped::scope_of(by, &ids),
                complete,
            })
        })())
    }

    /// Does the PASSING run `run` show each of the set's tests that `present` says the tree has
    /// running and passing?
    pub(super) fn proven_in(
        &self,
        run: &CheckRun,
        scratch: &CheckScratch,
        present: impl Fn(&str) -> bool,
    ) -> Result<(), String> {
        let (out, err) = run_text(run, scratch)?;
        match self
            .scope
            .ids
            .iter()
            .find(|id| present(id) && !floor_scoped::proven(self.by, &out, &err, id))
        {
            Some(id) => Err(format!(
                "its output does not show `{id}` running and passing"
            )),
            None => Ok(()),
        }
    }
}

/// (core#1198) The engine's retry of the failed head run `first` of `check`, when one is due
/// ([`retry_if_loaded`]): first only the failing set when the repo declares how. A failing-set
/// retry that FAILS decides (the failure reproduced). One that PASSES does not: a test that passes
/// alone may still fail beside the others, so the whole identical retry runs at once and decides.
/// Returns the deciding retry, the passes it cost before it, and why it ran the whole check
/// although a `rerun` is declared.
pub(super) fn retry_failing(
    worktree: &Path,
    held: Option<&HeldDir>,
    check: &RepoCheck,
    scoped: Option<&Result<Scoped, String>>,
    first: &CheckRun,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
) -> (Option<CheckRun>, Vec<CheckRun>, Option<String>) {
    let exec = ExecOpts::default();
    let s = match scoped {
        Some(Ok(s)) => s,
        other => {
            let r = retry_if_loaded(worktree, held, check, &exec, first, sandbox, scratch);
            let why = match other {
                Some(Err(e)) if r.is_some() => Some(e.clone()),
                _ => None,
            };
            return (r, Vec::new(), why);
        }
    };
    let Some(mut r) = retry_labelled(
        worktree,
        held,
        &s.check,
        &exec,
        first,
        sandbox,
        scratch,
        Some(&s.scope),
    ) else {
        return (None, Vec::new(), None);
    };
    if !r.passed() {
        return (Some(r), Vec::new(), None);
    }
    let why = "the failing set passed alone, which does not show the whole check passes (a test \
               can fail only beside the others)"
        .to_string();
    eprintln!(
        "wicked-core: repo checks floor — `{}`'s failing-set retry passed: retrying the whole \
         check, which decides",
        check.name
    );
    r.logs = relabel(r.logs, "retry-scoped");
    let mut whole = run_one_with(worktree, held, check, &exec, sandbox, scratch, Tree::Head);
    whole.logs = relabel(whole.logs, "retry");
    whole.source = format!(
        "the identical retry of `{}`: its failing-set retry passed, and only the whole check \
         decides a pass (core#1198)",
        check.name
    );
    (Some(whole), vec![r], Some(why))
}

/// (program DES-POWER-MODE) A power run at high rigor or above hands off on the FULL suite, at the
/// creator floor as at verify: no targeted or affected narrowing for it.
pub(super) fn power_full_suite(ctx: &FloorContext) -> bool {
    ctx.run_id
        .as_deref()
        .is_some_and(crate::power::full_suite_required)
}

pub(super) fn retry_headroom_wait() -> Duration {
    // Unit tests never wait for headroom.
    if cfg!(test) {
        return Duration::ZERO;
    }
    std::env::var(RETRY_WAIT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|s| Duration::from_secs(s.min(MAX_RETRY_WAIT_S)))
        .unwrap_or(RETRY_HEADROOM_WAIT)
}

/// (core#1767) The head's own RESULTS run of `check` (the base's shape: `results.how` argv, env and
/// merge, a `how.path` file read through the held tree) and its results text, for the head's
/// list coverage. A run that did not finish, could not start or exited outside `failedExit` and 0
/// has no results to read.
fn head_results(
    worktree: &Path,
    held: Option<&HeldDir>,
    check: &RepoCheck,
    g: &GuideCheck,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
) -> (CheckRun, Result<String, String>) {
    if let Some(h) = held {
        clear_results(h, check, g);
    }
    let since = std::time::SystemTime::now();
    let mut r = run_one_with(
        worktree,
        held,
        &RepoCheck {
            argv: g.for_results(&check.argv),
            source: format!("`{}`'s results on the head (core#1767)", check.name),
            ..check.clone()
        },
        &results_exec(g),
        sandbox,
        scratch,
        Tree::Head,
    );
    r.logs = relabel(r.logs, "results-head");
    let text = if r.timed_out || r.spawn_error.is_some() {
        Err(format!(
            "the head's results run did not finish ({})",
            r.summary()
        ))
    } else if !r
        .exit_code
        .is_some_and(|c| c == 0 || g.failed_exit.contains(&c))
    {
        Err(format!(
            "the head's results run did not run to a test verdict ({})",
            r.summary()
        ))
    } else {
        results_text(held, check, g, &mut r, scratch, since)
            .map_err(|e| format!("the head's results: {e}"))
    };
    (r, text)
}

/// The base side of a failed head check: the base run (fresh or cached) and, when it failed, its
/// inventory.
pub(super) struct BaseSide {
    pub(super) base: BaseRun,
    pub(super) toc: Option<Toc>,
}

/// (DES-FLOOR-V2 §3) Decide a FAILED head check `head` (its first, unchanged run) against its base
/// side: the head's exit codes decide, the base's TOC-validated failures are the only ones that
/// may be excluded, and head text never decides a run's outcome (core#1767: the head's results run
/// is read only for whether the head's list covers it, which an exclusion requires).
/// wicked-core#1701: a prose-only change may excuse an otherwise unseparated completed base failure.
#[allow(clippy::too_many_arguments)]
pub(super) fn decide(
    worktree: &Path,
    held: Option<&HeldDir>,
    check: &RepoCheck,
    head: &mut CheckRun,
    side: Option<BaseSide>,
    changed: Option<&[String]>,
    guide: &Result<&GuideCheck, String>,
    exclude: bool,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
    early_retry: Option<CheckRun>,
    no_retry: bool,
) {
    let mut logs = head.logs.clone();
    let base_sha = side.as_ref().map(|s| s.base.head.clone());
    let mut decision = CheckDecision {
        outcome: OUTCOME_NEW_FAILURE.to_string(),
        base_sha: base_sha.clone(),
        base_failing: Vec::new(),
        excluded: Vec::new(),
        log_refs: Vec::new(),
        flaky: false,
        reason: None,
        guide: None,
    };
    let base_run = side.as_ref().and_then(|s| s.base.run.as_ref());
    if let Some(b) = base_run {
        logs.extend(b.logs.iter().cloned());
    }
    // The base is red and finished: separate its failures from the head's, if it can be precise.
    let red_base = base_run.filter(|b| !b.passed() && !b.timed_out && b.spawn_error.is_none());
    let mut plan: Result<Vec<TestId>, String> = Err("the base passes this check".to_string());
    if let (Some(_), Some(side)) = (red_base, side.as_ref()) {
        plan = (|| {
            if !exclude {
                return Err(format!("exclusion disabled ({FLOOR_EXCLUDE_ENV})"));
            }
            let g = guide
                .as_ref()
                .map_err(|e| format!("no usable build guide: {e}"))?;
            let toc = side
                .toc
                .as_ref()
                .ok_or_else(|| "the base's list was not taken".to_string())?;
            logs.extend(toc.logs.iter().cloned());
            if let Some(why) = &toc.incomplete {
                return Err(format!("the base: {why}"));
            }
            let failing = toc.failing.clone();
            decision.base_failing = failing.iter().map(|t| t.id.clone()).collect();
            if failing.is_empty() {
                return Err("no failing test of the base appears in the base's own list".into());
            }
            if let Some(t) = failing.iter().find(|t| toc.count(t) != 1) {
                return Err(format!(
                    "`{}` names {} tests in the base's list",
                    t.id,
                    toc.count(t)
                ));
            }
            // `alone`/`deselect` that do not name the qualifier reach the id under every one.
            if let Some(t) = failing
                .iter()
                .find(|t| !g.q_scoped() && toc.qs(&t.id).len() != 1)
            {
                return Err(format!(
                    "`{}` is listed under more than one qualifier, and the guide's `alone` and \
                     `deselect` do not name `{{q}}`",
                    t.id
                ));
            }
            for t in &failing {
                match toc.alone.iter().find(|(a, _)| a == t) {
                    Some((_, true)) => {}
                    Some(_) => {
                        return Err(format!(
                            "`{}` does not fail when the base runs it alone",
                            t.id
                        ))
                    }
                    None => {
                        return Err(format!(
                            "`{}` was not confirmed on the base (more than \
                             {MAX_ALONE_CONFIRMATIONS} failures)",
                            t.id
                        ))
                    }
                }
            }
            let head_toc = inventory(worktree, held, check, g, &[], sandbox, scratch, Tree::Head);
            logs.extend(head_toc.logs.iter().cloned());
            if let Some(why) = &head_toc.incomplete {
                return Err(format!("the head: {why}"));
            }
            for t in &failing {
                if head_toc.count(t) > 1 {
                    return Err(format!(
                        "`{}` names {} tests in the head's list",
                        t.id,
                        head_toc.count(t)
                    ));
                }
                if head_toc.qs(&t.id).iter().any(|q| *q != t.q) {
                    return Err(format!(
                        "`{}` is not under the same qualifier on the base and the head",
                        t.id
                    ));
                }
            }
            let names: Vec<TestId> = failing
                .into_iter()
                .filter(|t| head_toc.count(t) == 1)
                .collect();
            // Precision, by the runner's own selection: the head's list with them deselected
            // lists exactly the rest of the head's tests.
            if !names.is_empty() {
                let rest = inventory(
                    worktree,
                    held,
                    check,
                    g,
                    &names,
                    sandbox,
                    scratch,
                    Tree::Head,
                );
                logs.extend(rest.logs.iter().cloned());
                let mut want: Vec<TestId> = head_toc
                    .ids
                    .iter()
                    .filter(|i| !names.contains(i))
                    .cloned()
                    .collect();
                let mut got = rest.ids.clone();
                want.sort();
                got.sort();
                if rest.incomplete.is_some() || got != want {
                    return Err(
                        "deselecting the base's failures does not list exactly the rest of the \
                         head's tests"
                            .into(),
                    );
                }
                // (core#1767) The head's list names exactly what the head's own results run
                // executed (per qualifier): the probe proved the list on its clean copy only, and
                // a binary added since that the list omits could make a name it shares look
                // unique. Last, so only an exclusion still standing pays for this run; it decides
                // nothing itself (the excluded run below does, by its exit code).
                let (r, text) = head_results(worktree, held, check, g, sandbox, scratch);
                logs.extend(r.logs.iter().cloned());
                g.completeness(&text?, &head_toc.ids)
                    .map_err(|e| format!("the head's list does not cover its run: {e}"))?;
            }
            Ok(names)
        })();
    }
    // The deciding run: the excluded head run when exclusion is precise and has something to
    // deselect, else the unchanged head run itself.
    let (deciding_check, deciding_exec, mut deciding, excluded_run) = match &plan {
        Ok(names) if !names.is_empty() => {
            let g = guide.as_ref().expect("a precise plan has a guide");
            let one = RepoCheck {
                argv: g.deselected(&check.argv, names),
                source: format!(
                    "`{}` with the base's {} failing test{} deselected (DES-FLOOR-V2)",
                    check.name,
                    names.len(),
                    if names.len() == 1 { "" } else { "s" }
                ),
                ..check.clone()
            };
            let exec = ExecOpts::default();
            let mut e = run_one_with(worktree, held, &one, &exec, sandbox, scratch, Tree::Head);
            e.logs = relabel(e.logs, "excluded");
            decision.excluded = names.iter().map(|t| t.id.clone()).collect();
            (one, exec, e.clone(), true)
        }
        _ => (check.clone(), ExecOpts::default(), head.clone(), false),
    };
    if excluded_run {
        logs.extend(deciding.logs.iter().cloned());
        head.reruns.push(deciding.clone());
    }
    let unseparated = red_base.is_some() && plan.is_err();
    // Failing twice is `flaky_twice` only when BOTH attempts ran oversubscribed: a retry that had
    // its headroom and failed again reproduced the failure without the load — a new failure.
    let mut twice = false;
    // A deselected run that crashed is decided as it stands (`could_not_run`): no retry.
    let crashed_out = excluded_run && crashed(&deciding, guide.as_ref().ok().copied()).is_some();
    // The head's identical retry already ran before the base (see the caller): it is this
    // decision's retry. Only when the decision could not exclude, so the deciding run is the head.
    let early_retry = early_retry.filter(|_| !excluded_run);
    // (core#1198) A check that retries its own failures is not retried again.
    let retry = early_retry.or_else(|| {
        (!deciding.environment_failure() && !crashed_out && !no_retry)
            .then(|| {
                retry_if_loaded(
                    worktree,
                    held,
                    &deciding_check,
                    &deciding_exec,
                    &deciding,
                    sandbox,
                    scratch,
                )
            })
            .flatten()
    });
    if let Some(r) = retry {
        logs.extend(r.logs.iter().cloned());
        head.retry = Some(RetrySummary::of(&r));
        head.reruns.push(r.clone());
        decision.flaky = r.passed();
        twice = !r.passed() && !r.timed_out && r.spawn_error.is_none() && ran_loaded(&r);
        deciding = r;
    }
    // (core#1627) The deciding run may be nested in `reruns`: keep its environment cause on
    // the head too, so a floor with reused checks terminates instead of starting over.
    if deciding.environment_failure() {
        head.spawn_error = deciding.spawn_error.clone();
    }
    // Head ids are an ANNOTATION: listed beside the outcome, never deciding it (the deciding
    // run's, when it was the excluded one).
    let annotated: Vec<String> = deciding.failure_ids.clone();
    let base_passed = base_run.is_some_and(CheckRun::passed);
    // A base that could not be established — it timed out, could not start, or could not be
    // exported or provisioned — separates nothing: escalate. (A base that declares no such check,
    // a package directory the change introduced (F24), and a repo that opted out of the
    // comparison, leave the head's failure a new one. Matched by the engine's own prefix: a
    // repo-controlled path quoted inside another error can never carry the exemption.)
    let base_unknown: Option<String> = side.as_ref().and_then(|s| match &s.base.run {
        Some(b) if b.timed_out => Some("the base run did not finish".to_string()),
        Some(b) if b.spawn_error.is_some() => Some(format!(
            "the base run could not start ({})",
            b.spawn_error.as_deref().unwrap_or_default()
        )),
        None => s
            .base
            .error
            .as_deref()
            .filter(|e| !base_error_leaves_a_new_failure(e))
            .map(|e| format!("the base could not be run ({e})")),
        _ => None,
    });
    let cannot_separate: Option<String> = if unseparated {
        plan.as_ref().err().cloned()
    } else {
        base_unknown
    };
    let (outcome, classification) = if deciding.environment_failure() {
        (OUTCOME_COULD_NOT_RUN, None)
    } else if deciding.passed() && excluded_run {
        (OUTCOME_PASS_EXCLUDED, Some(PRE_EXISTING_IN_SANDBOX))
    } else if deciding.passed() {
        (OUTCOME_PASS, Some(FLAKY_UNDER_LOAD))
    } else if let Some(why) = &cannot_separate {
        // wicked-core#1701: excuse prose only where the outcome would be base_red_unseparated.
        // A precise excluded run that still fails is evidence of a new failure, even in prose.
        // The caller supplies the whole repository's run-base paths, withheld above the event's
        // path cap or when deleted, renamed-away or type-changed paths cannot be classified.
        let prose = red_base
            .filter(|b| {
                b.exit_code.is_some_and(|code| code != 0)
                    && !b.environment_failure()
                    && crashed(b, guide.as_ref().ok().copied()).is_none()
                    && !deciding.timed_out
                    && deciding.spawn_error.is_none()
                    && deciding.exit_code.is_some_and(|code| code != 0)
                    && crashed(&deciding, guide.as_ref().ok().copied()).is_none()
            })
            .and(changed)
            .and_then(|paths| {
                crate::rule_check::prose_paths(
                    &paths.iter().map(String::as_str).collect::<Vec<_>>(),
                )
            });
        if let Some(paths) = prose {
            decision.reason = Some(format!(
                "the base fails this check too and the change touches only prose ({paths}): not the change's failure"
            ));
            (OUTCOME_PASS_EXCLUDED, Some(PRE_EXISTING_IN_SANDBOX))
        } else {
            // Before a retry's own timeout or spawn failure: the first run decided this.
            decision.reason = Some(format!("base red, cannot separate: {why}"));
            (OUTCOME_BASE_RED_UNSEPARATED, Some(BASE_RED_UNSEPARATED))
        }
    } else if deciding.timed_out {
        (OUTCOME_TIMED_OUT, None)
    } else if deciding.spawn_error.is_some() {
        (OUTCOME_COULD_NOT_RUN, None)
    } else if let Some(why) = crashed(&deciding, guide.as_ref().ok().copied()) {
        decision.reason = Some(why);
        (OUTCOME_COULD_NOT_RUN, None)
    } else {
        // `regression` (the compatible token) only when a base was compared.
        let compared = (base_passed || plan.is_ok()).then_some(REGRESSION);
        (
            if twice {
                OUTCOME_FLAKY_TWICE
            } else {
                OUTCOME_NEW_FAILURE
            },
            compared,
        )
    };
    if outcome == OUTCOME_PASS_EXCLUDED {
        let n = decision.excluded.len();
        if decision.reason.is_none() {
            decision.reason = Some(format!(
                "{n} pre-existing excluded: {}",
                decision.excluded.join(", ")
            ));
        }
        head.pre_existing = decision.excluded.clone();
    } else if matches!(outcome, OUTCOME_NEW_FAILURE | OUTCOME_FLAKY_TWICE) {
        head.regressions = annotated;
    }
    if decision.flaky {
        let said = "passed on retry (flaky)";
        decision.reason = Some(match decision.reason.take() {
            Some(r) => format!("{r}; {said}"),
            None => said.to_string(),
        });
    }
    decision.outcome = outcome.to_string();
    decision.log_refs = logs;
    head.classification = classification.map(str::to_string);
    head.base = side.map(|s| Box::new(s.base));
    head.decision = Some(decision);
}

/// The decision of a check whose first run decides alone: it passed, timed out or could not
/// start.
pub(super) fn decide_alone(run: &mut CheckRun) {
    run.decision = Some(CheckDecision {
        outcome: if run.passed() {
            OUTCOME_PASS
        } else if run.timed_out {
            OUTCOME_TIMED_OUT
        } else {
            OUTCOME_COULD_NOT_RUN
        }
        .to_string(),
        base_sha: None,
        base_failing: Vec::new(),
        excluded: Vec::new(),
        log_refs: run.logs.clone(),
        flaky: false,
        reason: None,
        guide: None,
    });
}

/// The decision's outcome of a check, when it has one.
pub(super) fn outcome_of(c: &CheckRun) -> Option<&str> {
    c.decision.as_ref().map(|d| d.outcome.as_str())
}

/// (core#553) Is the host oversubscribed — its 1-min load above its logical CPU count? The one
/// condition under which a head-only failure that passes alone is read as a load flake.
pub(crate) fn oversubscribed(load1: Option<f64>, cpus: usize) -> bool {
    matches!(load1, Some(l) if l.is_finite() && cpus > 0 && l > cpus as f64)
}

/// Phrases a creator uses to wave a failure away, and the check-shaped words one of them must
/// share a sentence with — a bug described as "pre-existing" in a fix's summary is not a claim
/// about the floor.
pub(super) const CLAIM_PHRASES: &[&str] = &[
    "pre-existing",
    "preexisting",
    "pre existing",
    "already failing",
    "already fails",
    "already failed",
    "already broken",
    "fails on main",
    "failing on main",
    "fail on main",
    "broken on main",
    "fails on the base",
    "existing failure",
    "not caused by",
    "unrelated to my change",
    "unrelated to this change",
    "unrelated to the change",
    "not related to my change",
    "not related to this change",
    "was already",
];
pub(super) const CHECK_WORDS: &[&str] = &[
    "typecheck",
    "type check",
    "type-check",
    "tsc",
    "lint",
    "clippy",
    "test",
    "check",
    "error",
    "failure",
    "failing",
    "fails",
    "failed",
    "build",
    "compile",
    "warning",
];

/// Split on newlines, semicolons and a full stop that ENDS a sentence (followed by whitespace or
/// the end of the text) — `CenterDashboard.tsx` is one token, not a sentence boundary. Every split
/// point is ASCII, so the slices are always on char boundaries.
pub(super) fn sentences(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    for (i, b) in bytes.iter().enumerate() {
        let boundary = match b {
            b'\n' | b';' => true,
            b'.' => bytes.get(i + 1).is_none_or(|n| n.is_ascii_whitespace()),
            _ => false,
        };
        if boundary {
            out.push(&text[start..i]);
            start = i + 1;
        }
    }
    out.push(&text[start..]);
    out
}

/// Conservatively detect a "this failure is pre-existing" claim: a claim phrase and a check-shaped
/// word in the same sentence. Returns the phrase that matched.
pub(crate) fn detect_claim(text: &str) -> Option<String> {
    for sentence in sentences(text) {
        let lower = sentence
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        if lower.is_empty() {
            continue;
        }
        if let Some(p) = CLAIM_PHRASES.iter().find(|p| lower.contains(**p)) {
            if CHECK_WORDS.iter().any(|w| lower.contains(w)) {
                return Some((*p).to_string());
            }
        }
    }
    None
}

/// (core#467) Judge a creator transcript's "pre-existing" claim against the baseline diff of the
/// first failing check. `None` when no claim was made, the floor is not the creator's, or the
/// floor is green (the claim is moot).
pub(super) fn judge_claim(ctx: &FloorContext, checks: &[CheckRun]) -> Option<ClaimCheck> {
    if ctx.stage != FloorStage::Creator {
        return None;
    }
    let phrase = detect_claim(ctx.claim_text.as_deref()?)?;
    let failing = checks
        .iter()
        .find(|c| c.denies() && c.name != "install")
        .or_else(|| checks.iter().find(|c| !c.passed() && c.name != "install"))?;
    let base_passed = failing
        .base
        .as_deref()
        .and_then(|b| b.run.as_ref())
        .is_some_and(CheckRun::passed);
    let verdict = match outcome_of(failing) {
        // The head passed once the base's own failures were deselected.
        Some(OUTCOME_PASS_EXCLUDED) => CLAIM_CONFIRMED,
        // A failure that remains with every base failure excluded, or against a green base, is
        // this change's; a pass on retry is a flake, not a pre-existing failure.
        Some(OUTCOME_NEW_FAILURE) | Some(OUTCOME_FLAKY_TWICE) if base_passed => CLAIM_REJECTED,
        // A red base the floor separated (its failures established, even ones the head no longer
        // has): what is left failing is this change's.
        Some(OUTCOME_NEW_FAILURE) | Some(OUTCOME_FLAKY_TWICE)
            if failing
                .decision
                .as_ref()
                .is_some_and(|d| !d.excluded.is_empty() || !d.base_failing.is_empty()) =>
        {
            CLAIM_REJECTED
        }
        Some(OUTCOME_PASS) => CLAIM_REJECTED,
        _ => CLAIM_UNVERIFIED,
    };
    Some(ClaimCheck {
        phrase,
        check: failing.name.clone(),
        verdict: verdict.to_string(),
    })
}

/// The host's 1-min load average and logical CPU count (`None` load where the platform has no
/// `getloadavg` — Windows).
pub(crate) fn host_load() -> (Option<f64>, usize) {
    #[cfg(test)]
    if let Some(l) = test_seam::load() {
        return l;
    }
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    #[cfg(unix)]
    let load1 = {
        let mut avg = [0f64; 3];
        // SAFETY: `avg` is a valid, writable buffer of 3 doubles and `nelem` says so.
        let n = unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) };
        (n >= 1).then_some(avg[0])
    };
    #[cfg(not(unix))]
    let load1: Option<f64> = None;
    (load1, cpus)
}

/// (core#781) `floor(load1 / cpus)`: 0 = headroom, N = N× oversubscribed; `None` when unknown.
pub(crate) fn load_band(load1: Option<f64>, cpus: usize) -> Option<u32> {
    match load1 {
        Some(l) if cpus > 0 && l.is_finite() && l >= 0.0 => {
            Some((l / cpus as f64).floor().min(u32::MAX as f64) as u32)
        }
        _ => None,
    }
}

/// (core#781) The base result was read from this run's cache — run by an earlier floor, under
/// another host load — and its load band differs from the head's: head and base were not compared
/// under like load, so the denial says so (a 5 s vitest timeout at 7× load against a base that ran
/// at 4× read as a plain regression twice). Empty when the base ran fresh, the bands match, or a
/// band is unknown.
pub(super) fn cached_base_load_note(head: &CheckRun) -> String {
    let Some(base) = head.base.as_deref().filter(|b| b.cached) else {
        return String::new();
    };
    let Some(b) = base.run.as_ref() else {
        return String::new();
    };
    match (b.load_band, head.load_band) {
        (Some(bb), Some(hb)) if bb != hb => format!(
            " (the base result is CACHED from an earlier floor of this run, under {}; this check \
             ran under {} — a different load band, so head and base were not compared under like \
             load)",
            b.bound_note.as_deref().unwrap_or("an unrecorded load"),
            head.bound_note.as_deref().unwrap_or("an unrecorded load"),
        ),
        _ => String::new(),
    }
}

/// The bound multiplier: `clamp(load1 / cpus, 1, LOAD_FACTOR_CAP)` — never below 1 (an idle host
/// keeps the base bound), capped so a wedged host cannot hold a unit for hours; 1 when the load is
/// unknown.
pub(crate) fn load_factor(load1: Option<f64>, cpus: usize) -> f64 {
    match load1 {
        Some(l) if cpus > 0 && l.is_finite() => (l / cpus as f64).clamp(1.0, LOAD_FACTOR_CAP),
        _ => 1.0,
    }
}

/// The effective bound for `check` right now, with the note that explains it.
pub(super) fn effective_bound(check: &RepoCheck) -> (Duration, Option<String>) {
    let base = check.base_timeout();
    let (load1, cpus) = host_load();
    let factor = load_factor(load1, cpus);
    let bound = Duration::from_secs_f64(base.as_secs_f64() * factor);
    let note = load1.map(|l| {
        format!(
            "{}s × {factor:.2} (1-min load {l:.1} / {cpus} cpus)",
            base.as_secs()
        )
    });
    (bound, note)
}

/// How long to wait for a check's stdout/stderr to reach EOF after the process group is dead. A
/// detached descendant (`setsid`, a Node `detached` spawn with inherited stdio) can hold the pipe
/// open forever; the capture is DETACHED after this and what was read so far is the (truncated) blob
/// (adversarial review on #414 — an unbounded join wedged the verify unit).
pub(super) const DRAIN_CAP: Duration = Duration::from_secs(5);

/// The failure identifier one complete output line carries, for the runners the floor knows —
/// `None` for any other line. `eslint_file` carries eslint's stylish file header across its
/// indented problem lines. Identifiers drop line/column positions so an unchanged failure keeps
/// its identity across a shifted file.
pub(crate) fn failure_id_of_line(line: &str, eslint_file: &mut Option<String>) -> Option<String> {
    let t = line.trim_end();
    let s = t.trim_start();
    if s.is_empty() {
        return None;
    }
    // cargo / libtest: `test path::name ... FAILED`
    if let Some(rest) = s.strip_prefix("test ") {
        if let Some(name) = rest.strip_suffix(" ... FAILED") {
            return Some(format!("test {}", name.trim()));
        }
    }
    // pytest: `FAILED tests/x.py::test_y - AssertionError: …`
    if let Some(rest) = s.strip_prefix("FAILED ") {
        let id = rest.split(" - ").next().unwrap_or(rest).trim();
        if !id.is_empty() {
            return Some(format!("FAILED {id}"));
        }
    }
    // go: `--- FAIL: TestX (0.00s)`
    if let Some(rest) = s.strip_prefix("--- FAIL: ") {
        let id = rest.split(' ').next().unwrap_or(rest);
        return Some(format!("FAIL {id}"));
    }
    // vitest: ` FAIL  tests/x.test.ts > suite > name`; jest: `● suite › name`
    if let Some(rest) = s.strip_prefix("FAIL ") {
        let id = rest.trim();
        if !id.is_empty() {
            return Some(format!("FAIL {id}"));
        }
    }
    if let Some(rest) = s.strip_prefix("● ") {
        let id = rest.trim();
        if !id.is_empty() {
            return Some(format!("● {id}"));
        }
    }
    // tsc: `src/a.ts(12,5): error TS2322: Type …` → `src/a.ts: error TS2322: Type …`
    if let Some(pos) = s.find("): error TS") {
        if let Some(open) = s[..pos].rfind('(') {
            return Some(format!("{}: {}", &s[..open], &s[pos + 3..]));
        }
    }
    // eslint (stylish): a bare path line, then `  12:5  error  message  rule-id` lines.
    if !t.starts_with(' ') && !s.contains(' ') && (s.contains('/') || s.contains('\\')) {
        *eslint_file = Some(s.to_string());
        return None;
    }
    if t.starts_with(' ') {
        if let Some(file) = eslint_file.as_deref() {
            let mut parts = s.split_whitespace();
            let loc = parts.next()?;
            if loc.contains(':') && loc.chars().all(|c| c.is_ascii_digit() || c == ':') {
                let sev = parts.next()?;
                if sev == "error" || sev == "warning" {
                    let rest: Vec<&str> = parts.collect();
                    let (rule, msg) = rest.split_last()?;
                    return Some(format!("{file}: {sev} {} {rule}", msg.join(" ")));
                }
            }
        }
    }
    // node:test / TAP: `not ok N - test name` — the only provably failure-only TAP line (#538).
    // `# Subtest:` appears for both passing and failing subtests and is intentionally excluded.
    if let Some(rest) = s.strip_prefix("not ok ") {
        let name = rest.split_once(" - ").map(|x| x.1).unwrap_or(rest).trim();
        if !name.is_empty() {
            return Some(format!("not ok {name}"));
        }
    }
    // node:test SPEC reporter (Node ≥ 23): `✖ test name (Nms)` — the timing suffix `(…ms)` is
    // the discriminator that makes this failure-only (the `✖ failing tests:` summary line and
    // eslint `✖ N problems` lines have no numeric-only timing suffix). `trim_start()` above
    // handles indented `  ✖ …` lines from nested suites (#538).
    if let Some(rest) = s.strip_prefix("✖ ") {
        // Require ` (Nms)` or ` (N.Ns)` timing — digits/dots only before the unit, so
        // "1 error, 0 warnings" (ends with 's') and "failing tests:" never match.
        if let Some(name_part) = rest.strip_suffix(')') {
            if let Some(paren) = name_part.rfind(" (") {
                let timing = &name_part[paren + 2..];
                let is_timing = timing
                    .strip_suffix("ms")
                    .or_else(|| timing.strip_suffix('s'))
                    .is_some_and(|n| {
                        !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                    });
                if is_timing {
                    let name = name_part[..paren].trim();
                    if !name.is_empty() {
                        return Some(format!("✖ {name}"));
                    }
                }
            }
        }
    }
    None
}

/// (core#1172) The line a check's own retry prints, in each stream it writes, as it starts
/// re-running its first pass's failures (crew's `scripts/test-related.mjs`, crew#649):
/// `WICKED-RETRY <why> — re-running …`. What a stream prints after its LAST such line is the final
/// pass.
pub(crate) const RETRY_BLOCK_MARKER: &str = "WICKED-RETRY";

/// Does `line` announce a retry's re-run ([`RETRY_BLOCK_MARKER`])? Its other lines (`none — …`,
/// `the re-run passed`, `the re-run FAILED too`) start nothing.
pub(super) fn starts_retry_block(line: &str) -> bool {
    line.trim_start()
        .strip_prefix(RETRY_BLOCK_MARKER)
        .is_some_and(|rest| rest.starts_with(' ') && rest.contains(" re-running "))
}

/// (core#1172) The failure ids a failed check's output names ([`failure_id_of_line`], both streams,
/// first sight order, at most [`MAX_FAILURE_IDS`]) and the ids its own retry cleared. A stream is
/// SPLIT only when it carries the retry-block marker and names a failure after its last one: it
/// then counts only what follows the marker, and what it named only before is flaky (passed on
/// retry). Every other stream (no marker, or a final pass that named nothing there) is read whole.
/// No split stream ⇒ every id is a failure, as before. Membership is decided over every id the
/// output names; the cap applies only to the lists returned, so a failure the final pass names
/// past the cap is never called flaky.
pub(crate) fn split_retry_ids(stdout: &str, stderr: &str) -> (Vec<String>, Vec<String>) {
    fn push(into: &mut Vec<String>, id: &str) {
        if !into.iter().any(|x| x == id) {
            into.push(id.to_string());
        }
    }
    let (mut first, mut last, mut all) = (Vec::new(), Vec::new(), Vec::new());
    let mut split_any = false;
    for stream in [stdout, stderr] {
        let lines: Vec<&str> = stream.lines().collect();
        let start = lines.iter().rposition(|l| starts_retry_block(l));
        let mut eslint_file = None;
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for (i, line) in lines.iter().enumerate() {
            let Some(id) = failure_id_of_line(line, &mut eslint_file) else {
                continue;
            };
            push(&mut all, &id);
            match start {
                Some(s) if i < s => push(&mut before, &id),
                _ => push(&mut after, &id),
            }
        }
        if start.is_some() && !after.is_empty() {
            split_any = true;
            before.iter().for_each(|id| push(&mut first, id));
        } else {
            // Not splittable: everything this stream named is a failure.
            after.splice(0..0, before);
        }
        after.iter().for_each(|id| push(&mut last, id));
    }
    if !split_any {
        all.truncate(MAX_FAILURE_IDS);
        return (all, Vec::new());
    }
    let mut flaky: Vec<String> = first.into_iter().filter(|id| !last.contains(id)).collect();
    last.truncate(MAX_FAILURE_IDS);
    flaky.truncate(MAX_FAILURE_IDS);
    (last, flaky)
}

/// Run one check in `worktree` under its timeout, inside `sandbox`'s write boundary with the
/// isolated `scratch` homes (`tree` picks the cargo target leaf). Both streams go to the run's log
/// store ([`crate::floor_log`]) whole, up to the per-stream cap; the run carries their refs
/// (`logs`, attempt `head`/`base` by `tree` — a caller relabels a re-run), the exit code, and the
/// 4 KB display tails. A LAUNCHER that fails to arm (`bwrap: …` / `sandbox-exec: …` as the first
/// stderr line, non-zero exit) is `could_not_run`, never the check's own `failed` (core#493).
/// Unheld: tests only — the floor spawns through [`run_held`] / [`run_one_with`] with the tree's
/// [`HeldDir`].
#[cfg(test)]
pub(crate) fn run_one(
    worktree: &Path,
    check: &RepoCheck,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
    tree: Tree,
) -> CheckRun {
    run_held(worktree, None, check, sandbox, scratch, tree)
}

/// [`run_one_with`] with the default [`ExecOpts`].
pub(crate) fn run_held(
    worktree: &Path,
    held: Option<&HeldDir>,
    check: &RepoCheck,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
    tree: Tree,
) -> CheckRun {
    run_one_with(
        worktree,
        held,
        check,
        &ExecOpts::default(),
        sandbox,
        scratch,
        tree,
    )
}

/// How a run the floor builds from a guide is spawned: extra environment, and stderr merged into
/// stdout (one stream, in order).
#[derive(Debug, Clone, Default)]
pub(crate) struct ExecOpts {
    pub(crate) env: Vec<(String, String)>,
    pub(crate) merge: bool,
}

/// [`run_one`] with [`ExecOpts`], in `worktree` — held as `held` from before any command ran
/// there: the path must still be that directory when the command is spawned
/// ([`HeldDir::still_at`]), or the run could not start (a command that swapped the tree's root for
/// a link to an outside directory never gets a later command started there).
pub(crate) fn run_one_with(
    worktree: &Path,
    held: Option<&HeldDir>,
    check: &RepoCheck,
    exec: &ExecOpts,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
    tree: Tree,
) -> CheckRun {
    let started = Instant::now();
    let mut result = CheckRun {
        failures_complete_in: None,
        name: check.name.clone(),
        argv: check.argv.clone(),
        source: check.source.clone(),
        exit_code: None,
        timed_out: false,
        spawn_error: None,
        duration_ms: 0,
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        bound_s: 0,
        bound_note: None,
        failure_ids: Vec::new(),
        flaky_ids: Vec::new(),
        scope: None,
        retry: None,
        self_retry: false,
        classification: None,
        pre_existing: Vec::new(),
        regressions: Vec::new(),
        base: None,
        env_cannot_run: Vec::new(),
        reruns: Vec::new(),
        earlier_passes: Vec::new(),
        selection: None,
        load_band: None,
        decision: None,
        logs: Vec::new(),
        reused_from: None,
    };
    let (load1, cpus) = host_load();
    result.load_band = load_band(load1, cpus);
    // LOAD-AWARE BOUND (core#469): base × clamp(load1/ncpu, 1, 3), recorded on the run and
    // logged before the check starts so the observed duration can be read against it.
    let (timeout, bound_note) = effective_bound(check);
    result.bound_s = timeout.as_secs();
    result.bound_note = bound_note;
    eprintln!(
        "wicked-core: repo checks floor — `{}` starts under a {}s bound{} in {}",
        check.name,
        result.bound_s,
        result
            .bound_note
            .as_deref()
            .map(|n| format!(" ({n})"))
            .unwrap_or_default(),
        worktree.display()
    );
    note_pass(&check.name, tree);
    let Some(bin) = check.argv.first() else {
        result.spawn_error = Some("empty argv".into());
        return result;
    };
    // The check's directory must be a real directory of THIS tree, every component (a head that
    // turned it into a link could send the check out of the tree): otherwise it does not run.
    if !check.dir.is_empty() && !plain_dir(worktree, &check.dir) {
        result.spawn_error = Some(format!(
            "`{}` is not a plain directory of the tree (a link, or missing)",
            check.dir
        ));
        return result;
    }
    // Held, too: the tree's root and the check's directory, walked from the held fd through no link,
    // must still be the directories the path names — a command that swapped either for a link to
    // an outside directory gets no later command started there.
    if let Some(e) = held.and_then(|h| h.still_at_rel(worktree, &check.dir).err()) {
        result.spawn_error = Some(e);
        return result;
    }
    // (codex on #1066) …and the child STARTS there by fd, not by pathname: the check directory is
    // opened from the held root through no link and kept open across the spawn, and the child
    // enters it with `fchdir` — a detached descendant of an earlier check that swaps the path for a
    // link between the check above and the child's chdir cannot send this command elsewhere.
    #[cfg(unix)]
    let start_dir = match held
        .filter(|_| scratch.container.is_none())
        .map(|h| h.dir_fd(&check.dir))
        .transpose()
    {
        Ok(fd) => fd,
        Err(e) => {
            result.spawn_error = Some(e);
            return result;
        }
    };
    let mut merged_in: Option<std::io::PipeReader> = None;
    // (DES-FLOOR-CONTAINER) In a container floor the check is a `docker exec` into its side's
    // fresh container: the tree there is a copy, the environment the container's own, and the
    // host's sandbox, PATH and working directory play no part.
    let container = scratch.container.clone();
    let mut cmd = if let Some(floor) = container.as_deref() {
        let _ = bin;
        match floor.command(
            tree,
            worktree,
            &check.name,
            &check.dir,
            &check.argv,
            &exec.env,
        ) {
            Ok(mut c) => {
                c.stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                if exec.merge {
                    match std::io::pipe().and_then(|(r, w)| Ok((r, w.try_clone()?, w))) {
                        Ok((r, w1, w2)) => {
                            c.stdout(w1).stderr(w2);
                            merged_in = Some(r);
                        }
                        Err(e) => {
                            result.spawn_error = Some(format!("the merged output pipe: {e}"));
                            return result;
                        }
                    }
                }
                c
            }
            Err(e) => {
                result.spawn_error = Some(e);
                result.duration_ms = started.elapsed().as_millis() as u64;
                return result;
            }
        }
    } else {
        // Resolve on PATH the way `Command::new` will, so "not installed" is a legible spawn error
        // rather than a bare OS code.
        let Some(exe) = crate::validator::find_on_path(bin) else {
            result.spawn_error = Some(format!("`{bin}` is not on PATH"));
            result.duration_ms = started.elapsed().as_millis() as u64;
            return result;
        };
        // `[<sandbox wrapper…>] [env K=V…] <exe> <args…>` — an empty wrapper is the disclosed
        // best-effort floor. (DES-BUILD-GUIDE) A guide's env is applied INSIDE the boundary only,
        // through `env` after the launcher: never on the launcher's own environment, which stays the
        // floor's sanitized allow-list (a launcher that is not setuid — bwrap — honours `LD_PRELOAD`
        // before it arms anything). With no launcher (no wrapper) the spawned process IS the command,
        // and the env is set on it directly.
        // (The names were checked when the guide loaded: `floor_guide::env_name_ok`.)
        let mut full: Vec<String> = sandbox.wrapper.clone();
        let env_inside = !exec.env.is_empty() && !sandbox.wrapper.is_empty();
        if env_inside {
            let exe_s = exe.to_string_lossy();
            let Some(env_bin) = crate::validator::find_on_path("env") else {
                result.spawn_error = Some(
                    "`env` is not on PATH (it applies the guide's env inside the boundary)".into(),
                );
                return result;
            };
            // `env` reads a leading `NAME=VALUE` as an assignment: an exe path holding `=` would be.
            if exe_s.contains('=') {
                result.spawn_error = Some(format!(
                    "`{exe_s}` holds `=` (it would read as an env assignment)"
                ));
                return result;
            }
            full.push(env_bin.to_string_lossy().into_owned());
            full.extend(exec.env.iter().map(|(k, v)| format!("{k}={v}")));
        }
        full.push(exe.to_string_lossy().into_owned());
        full.extend(check.argv[1..].iter().cloned());
        // spawn-audit: hardened — the repository's own check command, run in the run's worktree.
        let mut cmd = Command::new(&full[0]);
        cmd.hardened()
            .args(&full[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        match &start_dir {
            Some(fd) => {
                use std::os::fd::AsRawFd;
                use std::os::unix::process::CommandExt;
                // Close-on-exec is fine: the hook runs in the child after fork, before exec.
                let raw = fd.as_raw_fd();
                // SAFETY: the hook calls only `fchdir` (async-signal-safe) and reads `errno`; `raw`
                // stays open in the parent until `spawn` returns (`start_dir` outlives it), so the
                // forked child's copy is the held directory.
                unsafe {
                    cmd.pre_exec(move || {
                        if libc::fchdir(raw) == 0 {
                            Ok(())
                        } else {
                            Err(std::io::Error::last_os_error())
                        }
                    });
                }
            }
            None => {
                cmd.current_dir(worktree.join(&check.dir));
            }
        }
        #[cfg(not(unix))]
        cmd.current_dir(worktree.join(&check.dir));
        // The launcher's environment: the floor's sanitized allow-list only (the guide's is in `full`).
        scratch.apply_env(&mut cmd, tree, worktree);
        if !env_inside {
            // No launcher: this process is the command itself.
            for (k, v) in &exec.env {
                cmd.env(k, v);
            }
        }
        cmd
    };
    let merged = if container.is_some() {
        merged_in.take()
    } else if exec.merge {
        match std::io::pipe().and_then(|(r, w)| Ok((r, w.try_clone()?, w))) {
            Ok((r, w1, w2)) => {
                cmd.stdout(w1).stderr(w2);
                Some(r)
            }
            Err(e) => {
                result.spawn_error = Some(format!("the merged output pipe: {e}"));
                return result;
            }
        }
    } else {
        None
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group, so a timeout kills the whole tree (a test runner's workers included)
        // and never the daemon.
        cmd.process_group(0);
    }
    // A test swaps the check directory here: after the checks above, before the child's chdir.
    #[cfg(test)]
    if let Some(f) = test_seam::take_before_spawn() {
        f();
    }
    if let Some(reason) = scratch.stop.as_ref().and_then(FloorStop::reason) {
        result.spawn_error = Some(floor_stop_text(reason).into());
        result.duration_ms = started.elapsed().as_millis() as u64;
        return result;
    }
    let spawned = cmd.spawn();
    // The child holds its own working directory now: the parent's handle goes.
    #[cfg(unix)]
    drop(start_dir);
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            result.spawn_error = Some(e.to_string());
            result.duration_ms = started.elapsed().as_millis() as u64;
            return result;
        }
    };
    // The parent's copies of a merged pipe's write ends close here, so EOF comes with the child's.
    drop(cmd);
    let store = scratch.logs.as_path();
    let cap = scratch.log_cap;
    let (out_h, err_h) = match merged {
        // Merged: one stream, and an empty stderr blob beside it.
        Some(r) => (
            Some(crate::floor_log::capture(r, store, cap)),
            Some(crate::floor_log::capture(std::io::empty(), store, cap)),
        ),
        None => (
            child
                .stdout
                .take()
                .map(|r| crate::floor_log::capture(r, store, cap)),
            child
                .stderr
                .take()
                .map(|r| crate::floor_log::capture(r, store, cap)),
        ),
    };
    // A stopped check keeps the stop as its reason, whatever its container reports after.
    let mut stopped = false;
    // (core#1731) Admission read free space once; a build can then fill the host's disk. The
    // running check re-reads the cache volume and stops under the stop line, as for a cancel.
    let watch_root = check_cache_root().ok();
    let stop_gib = floor_stop_free_gib();
    let mut watched: Option<Instant> = None;
    let status = loop {
        if let Some(reason) = scratch.stop.as_ref().and_then(FloorStop::reason) {
            crate::validator::kill_child_tree(&mut child);
            crate::validator::reap_bounded(&mut child);
            result.spawn_error = Some(floor_stop_text(reason).into());
            stopped = true;
            break None;
        }
        if stop_gib > 0 && watched.is_none_or(|at| at.elapsed() >= DISK_WATCH_EVERY) {
            watched = Some(Instant::now());
            let free = watch_root.as_deref().and_then(watch_free_bytes);
            if let Some(why) = disk_watch(check, free, stop_gib) {
                crate::validator::kill_child_tree(&mut child);
                crate::validator::reap_bounded(&mut child);
                eprintln!("wicked-core: repo checks floor — {why}");
                let _ = scratch.disk_stop.set(why.clone());
                result.spawn_error = Some(why);
                stopped = true;
                break None;
            }
        }
        match crate::validator::has_exited_unreaped(&mut child) {
            Ok(true) => {
                // Quiesce: nothing the check backgrounded may keep running (or writing) into the
                // next stage. The exit was observed WITHOUT reaping (Copilot on #414), so the
                // leader's pid — the group id — is still reserved by the zombie when the group is
                // killed; only then is the status collected (immediate on a zombie).
                crate::validator::kill_child_tree(&mut child);
                break child.wait().ok();
            }
            Ok(false) if started.elapsed() >= timeout => {
                crate::validator::kill_child_tree(&mut child);
                crate::validator::reap_bounded(&mut child);
                result.timed_out = true;
                break None;
            }
            Ok(false) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                // The wait itself failed: the child may still be running — never leave a
                // repo-controlled process alive in the worktree after the floor gave up on it
                // (Copilot on #414). Kill the group, reap bounded, then report the failure.
                crate::validator::kill_child_tree(&mut child);
                crate::validator::reap_bounded(&mut child);
                result.spawn_error = Some(format!("wait failed: {e}"));
                break None;
            }
        }
    };
    result.exit_code = status.and_then(|st| st.code());
    // Ended by a signal (no exit code): it did not run to a verdict.
    if let Some(st) = status.filter(|st| st.code().is_none()) {
        result.spawn_error = Some(format!("the process was ended by a signal ({st})"));
    }
    // (DES-FLOOR-CONTAINER) Everything the check left running in its container is ended, and a
    // check during which the container ran out of memory or stopped answering did not run to a
    // verdict (floor infrastructure, never a failure of the change), nor did one after which the
    // copied tree no longer holds what was copied in — whatever its exit code.
    if let Some(why) = container
        .as_deref()
        .and_then(|f| f.after(tree, result.exit_code))
        .filter(|_| !stopped)
    {
        result.spawn_error = Some(why);
    }
    // BOUNDED: the group is dead, but a detached descendant may still hold the pipe — take what
    // was read (marked truncated) and move on rather than wait for an EOF that may never come.
    let out = out_h.map(|c| c.finish(DRAIN_CAP)).unwrap_or_default();
    let err = err_h.map(|c| c.finish(DRAIN_CAP)).unwrap_or_default();
    let attempt = match tree {
        Tree::Head => "head",
        Tree::Base => "base",
    };
    result.logs = vec![
        out.log_ref(attempt, "stdout"),
        err.log_ref(attempt, "stderr"),
    ];
    result.stdout_tail = String::from_utf8_lossy(&out.tail).into_owned();
    result.stderr_tail = String::from_utf8_lossy(&err.tail).into_owned();
    // A stream the store could not keep is no record: the check could not be run as evidence.
    if let Some(stream) = [&out, &err]
        .into_iter()
        .find(|s| s.no_space)
        .or_else(|| [&out, &err].into_iter().find(|s| s.error.is_some()))
    {
        if let Some(e) = &stream.error {
            result.spawn_error = Some(match result.spawn_error.take().filter(|_| stopped) {
                // (core#1731) A stopped check keeps its stop as the cause; the log error follows.
                Some(cause) => format!("{cause}; {e}"),
                None if stream.no_space => format!("{}; {e}", disk_full_error(check, "")),
                None => e.clone(),
            });
        }
    }
    // Read back only for a failing run: nothing below reads a passing run's streams (agy r6).
    let failed = result.exit_code != Some(0);
    let (stdout, stderr) = if failed {
        match (
            crate::floor_log::text(store, &result.logs[0]),
            crate::floor_log::text(store, &result.logs[1]),
        ) {
            (Ok(o), Ok(e)) => (o, e),
            // An unreadable record is no record: the run is not evidence.
            (Err(e), _) | (_, Err(e)) => {
                result
                    .spawn_error
                    .get_or_insert(format!("its output could not be read back: {e}"));
                Default::default()
            }
        }
    } else {
        Default::default()
    };
    // The launcher's exit is not the repository's (core#493): bwrap that cannot `mkdir` a `--tmpfs`
    // destination dies BEFORE exec with `bwrap: Can't mkdir …` and exit 1 (so does a launcher
    // whose `execvp` of the check's binary fails) — the check never ran, so recording
    // `exit_code: 1` as its `failed` blames the repo for the jail. The ONE predicate
    // (`validator::launcher_failure`) reads the first stderr line (of the one merged stream when
    // stderr was merged); a hit is `could_not_run` (denies, fail-closed, attribution honest).
    // Never applied to a passing exit or a timeout.
    // A stopped check keeps its stop as the cause: its output's first line is not a launcher's.
    if !stopped && !result.timed_out && result.exit_code != Some(0) {
        let first_err = if exec.merge { &stdout } else { &stderr };
        let first = first_err.lines().next().unwrap_or("");
        let launcher = if container.is_some() {
            crate::checks_container::launcher_failure(first, result.exit_code)
        } else {
            crate::validator::launcher_failure(&sandbox.wrapper, first)
        };
        if let Some(msg) = launcher {
            result.spawn_error = Some(msg);
        }
    }
    // (core#1605) ENOSPC on a cache volume below the admission minimum is infrastructure,
    // not a failed check to retry or compare with the base. Output alone is repo-controlled:
    // require the volume reading too, and keep any earlier launcher/log/signal diagnosis.
    if failed
        && !result.timed_out
        && result.spawn_error.is_none()
        && [&stdout, &stderr]
            .iter()
            .any(|text| text.contains("No space left on device") || text.contains("os error 28"))
    {
        let min_gib = floor_min_free_gib();
        if let Some(free) = check_cache_root()
            .ok()
            .and_then(|root| free_bytes(&root))
            .filter(|free| disk_guard(Some(*free), min_gib).is_some())
        {
            result.spawn_error = Some(disk_full_error(
                check,
                &format!(
                    " ({:.1} GiB free, minimum {min_gib} GiB; WICKED_FLOOR_MIN_FREE_GB)",
                    free as f64 / (1u64 << 30) as f64
                ),
            ));
        }
    }
    // ANNOTATION only (DES-FLOOR-V2): the ids a runner printed, both streams, first sight order.
    // They name failures for the reader; no outcome reads them. (core#1172) When the check retried
    // on its own, the final pass names the failures, and what only the first pass named is flaky.
    if failed {
        (result.failure_ids, result.flaky_ids) = split_retry_ids(&stdout, &stderr);
    }
    result.duration_ms = started.elapsed().as_millis() as u64;
    result
}

/// The same environment refusal for corroborated check output and the log store's own ENOSPC.
pub(super) fn disk_full_error(check: &RepoCheck, detail: &str) -> String {
    format!(
        "{ENVIRONMENT_REFUSAL_PREFIX} the disk filled during `{}`{detail}. Free disk space, then \
         retry; not a failure of the change",
        check.name
    )
}

/// The per-stream log cap ([`crate::floor_log::MAX_STREAM_BYTES`]; a test may lower it).
pub(super) fn log_cap() -> u64 {
    #[cfg(test)]
    if let Some(c) = test_seam::log_cap() {
        return c;
    }
    crate::floor_log::MAX_STREAM_BYTES
}
