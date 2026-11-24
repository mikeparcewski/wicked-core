//! (core#1100) Moved verbatim from `repo_checks.rs` (no behaviour change): the floor's detected-check run, coverage notes and targeted selection.

use super::*;

/// (core#1152) Everything a floor reads from the tree before its first check runs: the checks, the
/// base's build guide, the change's files and each package's config. The no-reuse re-run
/// ([`run_with_sandbox_ctx`]) takes them exactly as the first pass read them.
#[derive(Clone)]
pub(super) struct FloorInputs {
    pub(super) detected: Vec<RepoCheck>,
    pub(super) waived: Vec<String>,
    pub(super) exclude: bool,
    pub(super) guide: Result<Option<crate::floor_guide::BuildGuide>, String>,
    pub(super) touched: Vec<String>,
    pub(super) changed: Result<Vec<String>, String>,
    pub(super) removed: Result<Vec<String>, String>,
    /// Complete repository-relative change, eligible for the prose exception, even on a package floor.
    pub(super) prose_change: Option<Vec<String>>,
    pub(super) candidates: Vec<String>,
    pub(super) baseline_diff: bool,
    pub(super) package_baseline: std::collections::HashMap<String, bool>,
    pub(super) package_note: Option<String>,
    pub(super) e2e_paths: Option<Vec<String>>,
    /// (core#1198) Each detected check's `rerun` entry, by `(dir, name)`, read before any check ran.
    pub(super) reruns: std::collections::HashMap<(String, String), floor_scoped::RerunDecl>,
    /// (core#1392) What an operator's `targeted` re-run of this floor would run
    /// ([`targeted_selection`]'s source), read before any check ran.
    pub(super) targeted_test: Option<String>,
}

/// One pass of the floor over the detected checks ([`run_with_sandbox_ctx`]). With no `frozen`
/// inputs it reads them from the tree and lets a verify floor take the creator floor's results
/// ([`CreatorSeed`]); the second value is set when a check that ran (or was resumed) after reused
/// ones failed, with why and the inputs it read, and the caller then runs the checks again on
/// those inputs with nothing reused.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_detected(
    worktree: &Path,
    sandbox: &WorkerSandbox,
    sandbox_level: &str,
    sandbox_note: Option<&str>,
    ctx: &FloorContext,
    scratch: &CheckScratch,
    rerun: Option<&FloorRerun>,
    journal: Option<&FloorJournal>,
    untouched: Option<&dyn Fn() -> bool>,
    frozen: Option<FloorInputs>,
) -> (RepoChecksReport, Option<(String, FloorInputs)>) {
    let sandbox_level = sandbox_level.to_string();
    let sandbox_note = sandbox_note.map(str::to_string);
    // (core#1152, xreview) What the floor reads from the tree is read before its first check runs —
    // and, on the no-reuse re-run, taken as the first pass read it: by then repository commands
    // have run in this checkout, and an untracked file one left (a `.wicked/checks.json` turning
    // every check off) must never steer which checks run or how they decide.
    let allow_reuse = frozen.is_none();
    let inputs = match frozen {
        Some(f) => f,
        None => {
            let detected = match detect_with(worktree, ctx) {
                Ok(d) => d,
                Err(e) => {
                    return (
                        RepoChecksReport {
                            targeted_test: None,
                            detected: Vec::new(),
                            checks: Vec::new(),
                            skipped: Vec::new(),
                            passed: false,
                            detect_error: Some(e),
                            sandbox_level,
                            sandbox_note,
                            sandbox_error: None,
                            engine_writes_removed: Vec::new(),
                            claim: None,
                            env: None,
                            tree: None,
                            rerun: None,
                            waived: Vec::new(),
                            requested_rerun: None,
                            recheck: None,
                            coverage_note: None,
                            key: None,
                            resumed: None,
                            reused_from: None,
                            runner: None,
                        },
                        None,
                    )
                }
            };
            let (detected, waived) = match rerun {
                Some(r) => match apply_rerun(worktree, ctx, r, detected) {
                    Ok(pair) => pair,
                    Err(e) => {
                        return (
                            RepoChecksReport {
                                targeted_test: None,
                                detected: Vec::new(),
                                checks: Vec::new(),
                                skipped: Vec::new(),
                                passed: false,
                                detect_error: Some(e),
                                sandbox_level,
                                sandbox_note,
                                sandbox_error: None,
                                engine_writes_removed: Vec::new(),
                                claim: None,
                                env: None,
                                tree: None,
                                rerun: None,
                                waived: Vec::new(),
                                requested_rerun: None,
                                recheck: None,
                                coverage_note: None,
                                key: None,
                                resumed: None,
                                reused_from: None,
                                runner: None,
                            },
                            None,
                        )
                    }
                },
                None => (detected, Vec::new()),
            };
            // (DES-BUILD-GUIDE) The BASE tree's build guide — a head edit to it never applies to its own
            // run — and the files the change touched (for `invalidatedBy` and `affected`).
            // Read once per floor run: every check of it decides under the same switch.
            let exclude = exclusion_enabled();
            // `removed` (deleted, renamed away, type-changed: git's own D/T list) is read directly, never
            // inferred from `changed` minus `touched` — a path removed from the index but still on disk
            // is in both.
            let (guide, touched, changed, removed) =
                match (ctx.base_head.as_deref(), ctx.git_dir.as_deref()) {
                    (Some(base), Some(git_dir)) => (
                        crate::floor_guide::load_base(worktree, git_dir, base),
                        touched_files(worktree, ctx).unwrap_or_default(),
                        changed_files(worktree, ctx),
                        removed_files(worktree, ctx),
                    ),
                    _ => (Ok(None), Vec::new(), Ok(Vec::new()), Ok(Vec::new())),
                };
            // wicked-core#1701: package-relative lists omit changes outside the package. Prose
            // must describe the whole change, including removals, read before any check runs.
            let (repo_changed, repo_removed) = match package_root(worktree, ctx) {
                Some((root, _)) => {
                    let root_ctx = FloorContext {
                        package_dir: None,
                        ..ctx.clone()
                    };
                    (
                        changed_files(&root, &root_ctx),
                        removed_files(&root, &root_ctx),
                    )
                }
                None => (changed.clone(), removed.clone()),
            };
            let prose_change = repo_changed.ok().filter(|paths| {
                paths.len() <= crate::worktree_guard::CHANGED_PATHS_CAP
                    && matches!(&repo_removed, Ok(paths) if paths.is_empty())
            });
            let candidates = engine_generated_candidates(worktree, &detected);
            // (core#817) Per package: a subdirectory package's own `baseline_diff: false` holds for it.
            // The root's opt-out covers the whole repo; a package's own opt-out covers the package.
            // Every package's config is read HERE, before the first check runs: no `.wicked/checks.json`
            // is read from the tree after a command could have rewritten (or re-pointed) it.
            let baseline_diff = baseline_diff_enabled(worktree);
            let package_baseline: std::collections::HashMap<String, bool> = detected
                .iter()
                .filter(|c| !c.dir.is_empty())
                .map(|c| (c.dir.clone(), baseline_diff_enabled(&worktree.join(&c.dir))))
                .collect();
            // The coverage notes' reads of the tree (the change's files, the packages' manifests) happen
            // here too, before any check runs.
            let package_note = package_coverage_note(worktree, ctx, &detected);
            let e2e_paths = (ctx.stage == FloorStage::Verify)
                .then(|| e2e_paths(worktree, ctx))
                .flatten();
            // (core#1198) Read here too, before the first check runs. A build guide's check is the
            // guide's, not the config's: no entry applies to it.
            let mut reruns = std::collections::HashMap::new();
            for c in detected.iter().filter(|c| c.source != GUIDE_SOURCE) {
                if let Some(d) = rerun_decls(&worktree.join(&c.dir)).remove(&c.name) {
                    reruns.insert((c.dir.clone(), c.name.clone()), d);
                }
            }
            // (core#1392) What a `targeted` re-run would run, read here, before the first check
            // runs: the gate offers `targeted` only when this names a command.
            let targeted_test = targeted_selection(worktree, ctx)
                .ok()
                .flatten()
                .map(|c| c.source);
            FloorInputs {
                detected,
                waived,
                exclude,
                guide,
                touched,
                changed,
                removed,
                prose_change,
                candidates,
                baseline_diff,
                package_baseline,
                package_note,
                e2e_paths,
                reruns,
                targeted_test,
            }
        }
    };
    let rerun_inputs = (allow_reuse && ctx.stage == FloorStage::Verify).then(|| inputs.clone());
    let FloorInputs {
        detected,
        waived,
        exclude,
        guide,
        touched,
        changed,
        removed,
        prose_change,
        candidates,
        baseline_diff,
        package_baseline,
        package_note,
        e2e_paths,
        reruns,
        targeted_test,
    } = inputs;
    let baseline_diff_for =
        |dir: &str| dir.is_empty() || package_baseline.get(dir).copied().unwrap_or(false);
    // (F-RC2-009) Never run a HEAD process beside a copy of the base — a crash mid-floor could
    // have left one; the export lives only between its creation and the caching of its result.
    // (core#1050) A base export that cannot be cleared (a linked or non-directory `tmp`, a link
    // at the export path) REFUSES the floor, fail-closed: no check runs, and none may pass beside
    // a scratch the floor could not make safe.
    // Held before the first check runs: every later engine read, write or removal in the worktree
    // goes through it, never by a path a check could have re-pointed ([`HeldDir`]).
    let held_worktree = HeldDir::open(worktree);
    let mut scratch_error = held_worktree
        .as_ref()
        .map_err(String::clone)
        .and_then(remove_base_export)
        .err()
        .map(|e| format!("the checks' scratch could not be made safe: {e}"));
    let held_head = held_worktree.as_ref().ok();
    let env = scratch.env_record(&sandbox_level);
    let mut checks = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = false;
    // (J4) The ordinary floor keeps a journal (a re-run's or a waiver's check set is not the one a
    // later floor would run, so it neither writes nor reads one).
    // (core#1144) The tree this floor runs on, whatever kind of floor it is: a creator floor that
    // does not leave a record for it removes any earlier one (a re-run, a waiver, the live
    // worktree or a check that changed the checkout must never leave a stale record standing).
    let floor_tree = journal.map(|j| j.tree.clone()).filter(|t| !t.is_empty());
    let journal_of = journal.filter(|j| rerun.is_none() && waived.is_empty() && !j.tree.is_empty());
    let mut journal = journal
        .filter(|j| rerun.is_none() && waived.is_empty() && !j.tree.is_empty())
        .filter(|_| scratch_error.is_none())
        .map(|j| {
            OpenJournal::open(
                &scratch.logs,
                j,
                FloorKey::of(&j.tree, ctx, &detected),
                exclude,
                scratch.posture(),
            )
        });
    // (core#1144) The tree an ORDINARY guarded floor runs on (the checkout's probe is present only
    // there): a creator floor leaves its results for it, a verify floor reuses them. A floor on
    // the live worktree neither writes nor reads one — an untracked or ignored file there could
    // have steered it (core#417).
    let guarded_tree = journal
        .as_ref()
        .filter(|_| untouched.is_some())
        .map(|j| j.record.key.tree.clone());
    let mut seed = match (ctx.stage, guarded_tree.as_deref(), ctx.base_head.as_deref()) {
        (FloorStage::Verify, Some(tree), Some(base)) if allow_reuse => CreatorSeed::load(
            &scratch.logs,
            tree,
            base,
            &scratch.posture_level(&sandbox_level),
            exclude,
        ),
        _ => None,
    };
    // (core#1152) Reuse is in ANY order: every check the record stands for is reused, wherever it
    // sits, and only the rest run here. Two guards replace core#1145's suffix-only rule: a check
    // that runs fresh after reused ones and fails sends the floor round again with nothing reused
    // (`again`), and after the fresh checks the checkout must still hold exactly the tree the
    // reused results stand for (below), or they are void and the floor fails.
    let mut again: Option<String> = None;
    // The checks this floor RAN itself, for a creator floor's record, and (core#1152) the ones it
    // took from an interrupted floor's journal that ran them on a guarded clean checkout: never
    // one that may have run in the live worktree (xreview r1).
    let mut ran_here: Vec<CreatorEntry> = Vec::new();
    // (core#1152) The boundary this floor's runs are journaled and published under.
    let posture_level = scratch.posture_level(&sandbox_level);
    for check in detected.iter() {
        // (core#1731) Once the disk watch stopped a run, no further check starts: the floor is
        // that environment refusal ([`disk_stopped`]).
        if ctx.stop.as_ref().and_then(FloorStop::reason).is_some()
            || failed
            || scratch_error.is_some()
            || scratch.disk_stop.get().is_some()
        {
            skipped.push(check.name.clone());
            continue;
        }
        if let Some(run) = seed.as_mut().and_then(|s| s.reuse(check, &scratch.logs)) {
            if let Some(s) = &seed {
                eprintln!("{}", s.log_line(&run));
            }
            failed = run.denies();
            // A verify floor's reuse: never a creator record's entry.
            if let Some(j) = journal.as_mut() {
                j.record(check, &run, untouched, None);
            }
            checks.push(run);
            continue;
        }
        if let Some((run, guarded)) = journal.as_mut().and_then(|j| j.reuse(check, &scratch.logs)) {
            if let Some(resume) = journal.as_ref().and_then(|j| j.resume.as_ref()) {
                eprintln!(
                    "wicked-core: repo checks floor — `{}` {} on this tree in the floor \
                     interrupted ({}); reused, not run again",
                    run.name,
                    run.outcome(),
                    resume.cause
                );
            }
            failed = run.denies();
            // (xreview) A failure the interrupted pass ran after reused checks sends the floor
            // round again just as it would have there.
            if again.is_none() {
                again = seed.as_ref().and_then(|s| s.set_aside_by(check, &run));
            }
            // (core#1152) A resumed creator floor publishes what the interrupted floor ran on a
            // guarded clean checkout of this tree under this floor's boundary, beside what it
            // runs itself, so its record covers the set; an entry that may have run in the live
            // worktree, or under another boundary, stays out.
            let publishable = ctx.stage == FloorStage::Creator
                && untouched.is_some()
                && guarded.as_deref() == Some(posture_level.as_str());
            if let Some(j) = journal.as_mut() {
                j.record(check, &run, untouched, guarded);
            }
            if publishable {
                ran_here.push(CreatorEntry {
                    dir: check.dir.clone(),
                    name: check.name.clone(),
                    argv: check.argv.clone(),
                    timeout_s: check.timeout_s,
                    run: run.clone(),
                });
            }
            checks.push(run);
            continue;
        }
        let present = match &guide {
            Ok(Some(g)) => g.check_for(&check.argv, &check.dir),
            _ => None,
        };
        let gcheck = guide_for(&guide, check, ctx, worktree, &changed).and_then(|g| {
            runner_current(worktree, held_head, check, g, sandbox, scratch).map(|()| g)
        });
        let status = GuideStatus {
            present: present.is_some(),
            usable: gcheck.is_ok(),
            reason: gcheck.as_ref().err().cloned(),
        };
        // (DES-BUILD-GUIDE) At the creator, a usable guide's `affected` piece is a fast pre-pass:
        // its pass decides; its failure runs the check as declared.
        // A deleted, renamed-away or type-changed file (git's D/T list): what it left behind
        // cannot be mapped — the full check. A list that cannot be read: the full check too.
        let deletes = removed.as_ref().map_or(true, |r| !r.is_empty());
        let guided = gcheck
            .as_ref()
            .ok()
            .filter(|_| {
                ctx.stage == FloorStage::Creator
                    && !touched.is_empty()
                    && !deletes
                    && !power_full_suite(ctx)
            })
            .and_then(|g| g.affected_argv(&package_relative(touched.clone(), &check.dir)));
        // (core#1187) Without a guide's piece, the ENGINE computes a cargo workspace's affected
        // set from the diff (cargo's package graph + the code graph at the base): never the
        // creator's seat or the intake model. Recorded on the check either way.
        let engine = (guided.is_none() && ctx.stage == FloorStage::Creator)
            .then(|| {
                engine_affected(
                    worktree, held_head, check, ctx, &touched, &removed, sandbox, scratch,
                )
            })
            .flatten();
        let affected = match guided {
            Some(argv) => Some((
                argv,
                format!("`{}`'s affected set (DES-BUILD-GUIDE)", check.name),
            )),
            None => engine.as_ref().and_then(|p| p.argv.clone()).map(|argv| {
                (
                    argv,
                    format!(
                        "`{}`'s affected set, computed by the engine (core#1187)",
                        check.name
                    ),
                )
            }),
        };
        let mut run = match affected {
            Some((argv, source)) => {
                let a = RepoCheck {
                    argv,
                    source,
                    ..check.clone()
                };
                let r = run_held(worktree, held_head, &a, sandbox, scratch, Tree::Head);
                if r.passed() || r.environment_failure() {
                    r
                } else {
                    let mut full =
                        run_held(worktree, held_head, check, sandbox, scratch, Tree::Head);
                    full.earlier_passes
                        .push(("affected".to_string(), r.duration_ms));
                    full
                }
            }
            None => run_held(worktree, held_head, check, sandbox, scratch, Tree::Head),
        };
        run.selection = engine.map(|p| p.selection);
        // (DES-FLOOR-V2) A check that passed, timed out or could not start decides alone. A
        // FAILURE is compared with the run base (the install and the formatter never are: they
        // say nothing about the base, and a formatting diff is deterministic, core#551); the
        // exit codes of the head's runs decide, the base's TOC-validated failures are the only
        // ones that may be excluded. No known base ⇒ the failure denies as it always did.
        if run.passed() || run.timed_out || run.spawn_error.is_some() {
            decide_alone(&mut run);
        } else if let Some(why) = crashed(&run, gcheck.as_ref().ok().copied()) {
            // An exit the guide does not call a test failure (a crash, a build error): the
            // check could not run, whatever the base does.
            decide_alone(&mut run);
            if let Some(d) = run.decision.as_mut() {
                d.reason = Some(why);
            }
        } else {
            let compare = !is_provisioning(&check.name) && !is_formatter_check(&check.name);
            // With exclusion off the base runs plainly: no results, no list, no alone runs.
            let excluding = gcheck.as_ref().ok().copied().filter(|_| exclude);
            // (power-speed) A decision that cannot exclude is decided by the head's own runs, so
            // its identical retry (DES-FLOOR-V2 §4) goes FIRST. A pass decides alone (`passed on
            // retry (flaky)` reads no base), and the base run it would never have read — a whole
            // suite, 19.5 min on rig run bc2366e1, 30 min on b6f7968b — is not paid for. A retry
            // that fails is the decision's retry; the base then runs as before.
            // (core#1198) The repo's `rerun` entry for this check: retry and compare only its failing
            // set, and do not retry a check that retries its own failures.
            let decl = reruns.get(&(check.dir.clone(), check.name.clone()));
            let self_retry = compare && decl.is_some_and(|d| d.self_retry);
            run.self_retry = self_retry;
            let scoped = decl
                .filter(|_| compare && excluding.is_none())
                .and_then(|d| Scoped::of(check, d, &run, scratch));
            // (core#1186) A complete, file-qualified failing set is recorded on the head run.
            if let Some(Ok(s)) = &scoped {
                if s.by == floor_scoped::RerunBy::File
                    && s.complete.is_ok()
                    && run.failure_ids.len() < MAX_FAILURE_IDS
                {
                    // (core#1709) The directory the ids are relative to: the check's own joined
                    // with the declared `rerun.<check>.root`, so two checks whose runners name
                    // files from different roots never qualify a test alike.
                    run.failures_complete_in = Some(runner_root(&check.dir, &s.root));
                }
            }
            if let Some(Err(why)) = &scoped {
                eprintln!(
                    "wicked-core: repo checks floor — `{}`: the whole check is retried and \
                     compared ({why})",
                    check.name
                );
            }
            let (early_retry, retry_extra, whole_because) =
                if compare && excluding.is_none() && !self_retry {
                    retry_failing(
                        worktree,
                        held_head,
                        check,
                        scoped.as_ref(),
                        &run,
                        sandbox,
                        scratch,
                    )
                } else {
                    (None, Vec::new(), None)
                };
            // A failing-set retry ran and passed, so the whole retry decided.
            let retried_whole = !retry_extra.is_empty();
            run.reruns.extend(retry_extra);
            let retry_passed = early_retry.as_ref().is_some_and(CheckRun::passed);
            let retry_environment_failure = early_retry
                .as_ref()
                .is_some_and(CheckRun::environment_failure);
            // (core#1191) A fresh check that failed after reused ones sends the floor round again
            // with nothing reused (core#1152), and that pass classifies it. Classifying it HERE
            // too (the base run: a whole suite) would be paid twice — rig run b6f7968b spent
            // 2 h 27 min on this pass's classification before the set-aside started the suite
            // over. Only where nothing left could accept the failure: no exclusion can apply and
            // the identical retry (when due) has already failed. A retry that passed, or a base
            // a precise exclusion could still read, is decided below as before (xreview on #1192).
            let set_aside_now =
                (compare && excluding.is_none() && !retry_passed && !retry_environment_failure)
                    .then(|| seed.as_ref().and_then(|s| s.set_aside_by(check, &run)))
                    .flatten();
            if retry_environment_failure {
                let mut r = early_retry.expect("environment failure on the retry");
                decide_alone(&mut r);
                run.spawn_error = r.spawn_error.clone();
                run.retry = Some(RetrySummary {
                    whole_because,
                    ..RetrySummary::of(&r)
                });
                let mut decision = r.decision.clone().expect("decided above");
                decision.log_refs.extend(run.logs.iter().cloned());
                run.decision = Some(decision);
                run.reruns.push(r);
            } else if let Some(why) = set_aside_now {
                if let Some(r) = early_retry {
                    run.retry = Some(RetrySummary {
                        whole_because: whole_because.clone(),
                        ..RetrySummary::of(&r)
                    });
                    run.reruns.push(r);
                }
                decide_alone(&mut run);
                again = Some(why);
            } else {
                let side = match (ctx.base_head.as_deref(), ctx.git_dir.as_deref()) {
                    _ if !compare || retry_passed => None,
                    (Some(head), _) if !(baseline_diff && baseline_diff_for(&check.dir)) => {
                        Some(BaseSide {
                            base: BaseRun {
                                head: head.to_string(),
                                cached: false,
                                run: None,
                                error: Some(format!(
                                "{BASELINE_DIFF_OFF_PREFIX}{CONFIG_PATH}` (baseline_diff: false)"
                            )),
                            },
                            toc: None,
                        })
                    }
                    (Some(head), Some(git_dir)) => {
                        // This run's cache first; on a miss the base is exported, run (and, red,
                        // inventoried), cached, and the export removed before the next HEAD run.
                        // (core#1198) A cached WHOLE base answers a failing set too; then the
                        // set's own record; then the set runs on the base.
                        // Only a set that accounts for every failure of the run that decides: the
                        // head's own count must match it, and a whole retry that decided is the
                        // whole check's failure, not the set's (xreview on #1199).
                        let failing_set =
                            scoped.as_ref().and_then(|r| r.as_ref().ok()).filter(|s| {
                                match (&s.complete, retried_whole) {
                                    (Ok(()), false) => true,
                                    (Err(why), _) => {
                                        eprintln!(
                                        "wicked-core: repo checks floor — `{}`: the whole check \
                                         runs on the base ({why})",
                                        check.name
                                    );
                                        false
                                    }
                                    (Ok(()), true) => false,
                                }
                            });
                        let cached =
                            BaseTree::cached(scratch, head, check, excluding).or_else(|| {
                                failing_set.and_then(|s| {
                                    BaseTree::cached_of(scratch, head, &s.check, None, true)
                                })
                            });
                        Some(match cached {
                            Some(cached) => cached,
                            None => {
                                let export =
                                    held_worktree.as_ref().map_err(String::clone).and_then(
                                        |root| BaseTree::export(worktree, root, git_dir, head),
                                    );
                                let side = match export {
                                    Ok(t) => match failing_set {
                                        Some(s) => {
                                            let (side, cost) =
                                                t.run_failing_set(check, s, ctx, sandbox, scratch);
                                            if let Some(c) = cost {
                                                run.earlier_passes.push((
                                                    "base failing set".to_string(),
                                                    c.duration_ms,
                                                ));
                                            }
                                            side
                                        }
                                        None => {
                                            t.run_check(check, ctx, sandbox, scratch, excluding)
                                        }
                                    },
                                    Err(e) => BaseSide {
                                        base: BaseRun {
                                            head: head.to_string(),
                                            cached: false,
                                            run: None,
                                            error: Some(e),
                                        },
                                        toc: None,
                                    },
                                };
                                let removed = held_worktree
                                    .as_ref()
                                    .map_err(String::clone)
                                    .and_then(remove_base_export);
                                if let Err(e) = removed {
                                    scratch_error = Some(format!(
                                        "the base export could not be removed before the next \
                                     check: {e}"
                                    ));
                                }
                                side
                            }
                        })
                    }
                    _ => None,
                };
                if scratch_error.is_some() {
                    decide_alone(&mut run);
                    run.decision = Some(CheckDecision {
                        outcome: OUTCOME_COULD_NOT_RUN.to_string(),
                        ..run.decision.take().expect("set above")
                    });
                } else {
                    decide(
                        worktree,
                        held_head,
                        check,
                        &mut run,
                        side,
                        prose_change.as_deref(),
                        &gcheck,
                        exclude,
                        sandbox,
                        scratch,
                        early_retry,
                        self_retry,
                    );
                    if let Some(r) = run.retry.as_mut() {
                        r.whole_because = whole_because;
                    }
                }
            }
        }
        if let Some(d) = run.decision.as_mut() {
            d.guide = Some(status);
        }
        eprintln!(
            "wicked-core: repo checks floor — `{}` {} in {:.1}s (bound {}s{}){}",
            run.name,
            run.outcome(),
            run.duration_ms as f64 / 1000.0,
            run.bound_s,
            run.other_passes(),
            outcome_of(&run)
                .map(|c| format!(" — {c}"))
                .unwrap_or_default()
        );
        failed = run.denies();
        if let Some(s) = seed.as_mut() {
            if again.is_none() {
                again = s.set_aside_by(check, &run);
            }
            s.fresh.push(check.name.clone());
        }
        #[cfg(test)]
        if test_seam::take_disk_stop_after(&check.name) {
            let _ = scratch.disk_stop.set(format!(
                "{ENVIRONMENT_REFUSAL_PREFIX} the disk was nearly full during `{}`'s base run \
                 (test stop)",
                check.name
            ));
        }
        // (core#1731) A check decided while the disk watch stopped a run of this floor (its own,
        // its retry's, its base's or an inventory's) is no evidence for a later floor.
        let stopped_for_disk = scratch.disk_stop.get().is_some();
        if let Some(j) = journal.as_mut().filter(|_| !stopped_for_disk) {
            j.record(
                check,
                &run,
                untouched,
                untouched.is_some().then(|| posture_level.clone()),
            );
        }
        if ctx.stage == FloorStage::Creator && !stopped_for_disk {
            ran_here.push(CreatorEntry {
                dir: check.dir.clone(),
                name: check.name.clone(),
                argv: check.argv.clone(),
                timeout_s: check.timeout_s,
                run: run.clone(),
            });
        }
        checks.push(run);
    }
    // A check that changed the checkout's tracked content (even one a later check restored)
    // spoiled the journal: nothing this floor ran may stand for the tree (xreview r1).
    let spoiled = journal.as_ref().is_none_or(|j| j.spoiled);
    let stopped = ctx.stop.as_ref().and_then(FloorStop::reason);
    let resumed = journal.and_then(|j| j.finish(stopped));
    if stopped.is_some() {
        again = None;
    }
    let claim = judge_claim(ctx, &checks);
    // An engine-written file (absent at detection, present now) is the checks' own side effect —
    // remove it so the guard's final comparison sees the tree the seat left. Never a symlink.
    let mut engine_writes_removed = Vec::new();
    if let Ok(held) = &held_worktree {
        for rel in candidates {
            if held.remove_file(&rel) == Ok(true) {
                engine_writes_removed.push(rel);
            }
        }
    }
    // (core#1152) The direct guard for reusing out of order: the reused results stand for the tree
    // the creator floor ran on, so once checks ran fresh beside them this checkout must still hold
    // exactly that tree — after every check (the journal's spoil) and now, with the engine's own
    // writes gone. A change voids them, and the floor fails, as any guarded floor whose check
    // changed its tracked content does ([`FloorCheckout::hold_to`]); they are not run again on
    // content that is not the tree that ships.
    let reuse_error = seed
        .as_ref()
        .filter(|s| !s.reused.is_empty() && !s.fresh.is_empty())
        .filter(|_| spoiled || !untouched.is_some_and(|probe| probe()))
        .map(|s| {
            format!(
                "a check that ran fresh here ({}) changed the checkout's tracked content, so the \
                 results reused from the creator floor ({}) no longer stand for it — void, and \
                 the floor fails",
                s.fresh.join(", "),
                s.reused.join(", ")
            )
        });
    if reuse_error.is_some() {
        again = None;
    }
    let reused_from = seed.and_then(|s| s.finish(reuse_error.clone()));
    if let Some(r) = reused_from
        .as_ref()
        .filter(|r| r.voided.is_none() && again.is_none())
    {
        eprintln!("wicked-core: repo checks floor — {}", r.note());
    }
    // (core#1144) A guarded creator floor leaves the checks it ran for the verify floor of the same
    // tree and base — only an ordinary floor (no re-run, nothing waived), on a clean checkout, whose
    // checks never changed it (per check, through the journal) and that still holds exactly that
    // tree now that the engine's own writes are gone. Any other creator floor of that tree and base
    // removes an earlier record, so verify never stands on a result this floor superseded.
    if let (FloorStage::Creator, Some(tree), Some(base)) =
        (ctx.stage, floor_tree.as_deref(), ctx.base_head.as_deref())
    {
        let publish = match (journal_of, untouched) {
            (Some(j), Some(probe)) if scratch_error.is_none() && !spoiled && probe() => Some(j),
            _ => None,
        };
        match publish {
            Some(j) => write_creator_record(
                &scratch.logs,
                &CreatorFloorRecord {
                    floor_policy: FLOOR_POLICY_VERSION,
                    tree: tree.to_string(),
                    base: base.to_string(),
                    ord: j.ord,
                    attempt: j.attempt,
                    phase: j.phase.clone(),
                    sandbox_level: posture_level,
                    exclude,
                    checks: ran_here,
                },
            ),
            None => {
                let _ = std::fs::remove_file(creator_record_path(&scratch.logs, tree, base));
            }
        }
    }
    let notes: Vec<String> = [
        e2e_paths.and_then(|paths| e2e_coverage_note(paths, &detected, &checks, &waived)),
        package_note,
    ]
    .into_iter()
    .flatten()
    .collect();
    let coverage_note = (!notes.is_empty()).then(|| notes.join("; "));
    if let Some(e) = &scratch_error {
        eprintln!("wicked-core: repo checks floor — refused: {e}");
    }
    if let Some(e) = &reuse_error {
        eprintln!("wicked-core: repo checks floor — {e}");
    }
    let passed = !failed && scratch_error.is_none() && reuse_error.is_none();
    let detect_error = match (scratch_error, reuse_error) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (a, b) => a.or(b),
    };
    // (core#1392) Only a floor that did not FINISH offers `targeted`, so only it says what that runs.
    let targeted_test =
        targeted_test.filter(|_| checks.iter().any(|c| c.did_not_finish() && c.denies()));
    (
        RepoChecksReport {
            targeted_test,
            passed,
            detected,
            checks,
            skipped,
            detect_error,
            sandbox_level,
            sandbox_note,
            sandbox_error: None,
            engine_writes_removed,
            claim,
            env: Some(env),
            tree: None,
            rerun: None,
            waived,
            requested_rerun: None,
            recheck: None,
            coverage_note,
            key: None,
            resumed,
            reused_from,
            runner: None,
        },
        again.zip(rerun_inputs),
    )
}

/// (core#817) A changed subdirectory package the floor detected NO checks for is named, so a
/// green floor is never read as having checked it.
pub(super) fn package_coverage_note(
    worktree: &Path,
    ctx: &FloorContext,
    detected: &[RepoCheck],
) -> Option<String> {
    let pkgs = subdir_packages(worktree, ctx).ok()?;
    let bare: Vec<String> = pkgs
        .dirs
        .into_iter()
        // An install (or a native build) alone checks nothing.
        .filter(|d| {
            !detected
                .iter()
                .any(|c| &c.dir == d && !is_provisioning(&c.name))
        })
        .map(|d| format!("{d}/"))
        .collect();
    (!bare.is_empty()).then(|| {
        format!(
            "the change touches package{} {} and no checks were detected for {} (no test, \
             typecheck or lint script) — the floor did NOT check {}",
            if bare.len() == 1 { "" } else { "s" },
            bare.join(", "),
            if bare.len() == 1 { "it" } else { "them" },
            if bare.len() == 1 { "it" } else { "them" },
        )
    })
}

/// (core#432 F-3R2-023) Does `path` look like an end-to-end file: a path segment `e2e`,
/// `playwright` or `cypress`, or a file name carrying `e2e` (`checkout.e2e.ts`, `test_e2e_x.py`)?
pub(super) fn is_e2e_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let mut segments = lower.split('/').peekable();
    while let Some(seg) = segments.next() {
        let last = segments.peek().is_none();
        if matches!(seg, "e2e" | "playwright" | "cypress") || (last && seg.contains("e2e")) {
            return true;
        }
    }
    false
}

/// (core#432 F-3R2-023) A fix that promotes an e2e scenario (an xfail flipped to expected-pass) was
/// green at verify while the scenario never ran: the floor ran typecheck, lint and the unit tests
/// only. When the change touches end-to-end files (added, modified or deleted relative to the
/// base) and no `e2e` or journey check RAN, say which files and why nothing ran them — undeclared,
/// waived, or skipped after an earlier failure. `None` when an end-to-end check ran, nothing e2e
/// was touched, or the run knows no base.
pub(super) fn e2e_coverage_note(
    e2e: Vec<String>,
    detected: &[RepoCheck],
    checks: &[CheckRun],
    waived: &[String],
) -> Option<String> {
    let is_e2e_check = |name: &str| name == "e2e" || name.starts_with(JOURNEY_PREFIX);
    // A check that could not even start (`spawn_error`) did not run it.
    if checks
        .iter()
        .any(|c| is_e2e_check(&c.name) && c.spawn_error.is_none())
    {
        return None;
    }
    if e2e.is_empty() {
        return None;
    }
    let why = if checks.iter().any(|c| is_e2e_check(&c.name)) {
        "the declared end-to-end check could not start".to_string()
    } else if waived.iter().any(|w| is_e2e_check(w)) {
        "the operator waived the declared end-to-end check".to_string()
    } else if detected.iter().any(|c| is_e2e_check(&c.name)) {
        "the declared end-to-end check was skipped after an earlier check failed".to_string()
    } else {
        format!("no `e2e` or `journeys` check is declared in `{CONFIG_PATH}`")
    };
    let shown: Vec<&str> = e2e.iter().take(5).map(String::as_str).collect();
    let more = e2e.len().saturating_sub(shown.len());
    Some(format!(
        "the change touches end-to-end file{} {}{} and the floor did NOT run them: {why} — the \
         promoted scenario is unproven by this floor; external CI or the reviewer is its gate",
        if e2e.len() == 1 { "" } else { "s" },
        shown.join(", "),
        if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        }
    ))
}

/// The end-to-end files the change touched or removed ([`is_e2e_path`]), sorted; `None` when the
/// change's files cannot be listed. Read before any check runs.
pub(super) fn e2e_paths(worktree: &Path, ctx: &FloorContext) -> Option<Vec<String>> {
    let mut paths = touched_files(worktree, ctx).ok()?;
    paths.extend(removed_files(worktree, ctx).ok()?);
    let mut e2e: Vec<String> = paths.into_iter().filter(|p| is_e2e_path(p)).collect();
    e2e.sort();
    e2e.dedup();
    Some(e2e)
}

/// The check name a targeted test command runs under.
pub(super) const TARGETED_CHECK: &str = "test_targeted";

/// (core#1392) Why a `targeted` re-run cannot run on a tree with no targeted test command.
pub const NO_TARGETED_COMMAND: &str =
    "a `targeted` re-run needs a targeted test command, and this \
     tree has none (no `test_targeted` in `.wicked/checks.json`, and none the floor can derive) — \
     it would run no tests, so it is not run: extend the bound, accept the partial floor (the \
     tests are then waived by you, labelled), give your own judgment, or declare `test_targeted`";

/// The check set a `targeted` re-run runs: the creator-stage selection, without a power run's
/// full-suite policy, which would hand back the full set and waive every test (xreview on #1184).
pub(super) fn targeted_checks(
    worktree: &Path,
    ctx: &FloorContext,
) -> Result<Vec<RepoCheck>, String> {
    detect_with(
        worktree,
        &FloorContext {
            stage: FloorStage::Creator,
            run_id: None,
            ..ctx.clone()
        },
    )
}

/// (core#1392) The targeted test check a `targeted` re-run would run on this tree, if any.
pub(super) fn targeted_selection(
    worktree: &Path,
    ctx: &FloorContext,
) -> Result<Option<RepoCheck>, String> {
    Ok(targeted_checks(worktree, ctx)?
        .into_iter()
        .find(|c| c.name == TARGETED_CHECK))
}

/// Is `name` one of the floor's TEST-set checks (the ones a `targeted` re-run replaces)?
pub(crate) fn is_test_set_check(name: &str) -> bool {
    matches!(name, "test" | "cargo-test" | "e2e") || name.starts_with(JOURNEY_PREFIX)
}

/// (core#469) The check set an operator's re-run runs, and the detected checks it waives.
/// * `extend` — every check (the install excepted) under [`EXTEND_FACTOR`]× its base bound;
/// * `targeted` — the CREATOR-stage selection (the declared or derived `test_targeted` in place of
///   the full test set, no `e2e`); with no targeted command it is an error ([`NO_TARGETED_COMMAND`]):
///   the floor reports `not_run` and denies, never waiving the tests (core#1392);
/// * `accept_partial` — the named checks are waived, the rest run again.
pub(super) fn apply_rerun(
    worktree: &Path,
    ctx: &FloorContext,
    rerun: &FloorRerun,
    detected: Vec<RepoCheck>,
) -> Result<(Vec<RepoCheck>, Vec<String>), String> {
    match rerun.mode {
        FloorRerunMode::Extend => Ok((
            detected
                .into_iter()
                .map(|mut c| {
                    if c.name != "install" {
                        c.timeout_s = Some(c.base_timeout().as_secs() * EXTEND_FACTOR);
                    }
                    c
                })
                .collect(),
            Vec::new(),
        )),
        FloorRerunMode::Targeted => {
            let creator = targeted_checks(worktree, ctx)?;
            if creator.iter().any(|c| c.name == TARGETED_CHECK) {
                // The full test set the targeted command stands in for is disclosed as waived.
                let waived = detected
                    .iter()
                    .filter(|c| {
                        is_test_set_check(&c.name) && !creator.iter().any(|k| k.name == c.name)
                    })
                    .map(|c| c.name.clone())
                    .collect();
                Ok((creator, waived))
            } else {
                // (core#1392) Never "fewer tests" turned into "no tests": with no targeted command
                // the re-run does not run, and the floor denies (fail-closed). The gate refuses
                // this answer first (`targeted_test` names nothing); this holds if it is reached.
                Err(NO_TARGETED_COMMAND.to_string())
            }
        }
        FloorRerunMode::AcceptPartial => {
            let (waived, run): (Vec<RepoCheck>, Vec<RepoCheck>) = detected
                .into_iter()
                .partition(|c| rerun.waive.iter().any(|w| w == &c.name));
            Ok((run, waived.into_iter().map(|c| c.name).collect()))
        }
        // The fix already ran; its floor is the ordinary one, on the fixed tree.
        FloorRerunMode::FloorFix => Ok((detected, Vec::new())),
    }
}

/// (core#469) The checks an `accept_partial` waives, read off the report of the floor that did
/// not finish: every check that did not pass (the one that hit its bound) and every one that
/// never ran after it. Empty when the report is not a timed-out floor — nothing to accept.
pub fn partial_waiver(report: &RepoChecksReport) -> Vec<String> {
    if !report.timed_out() {
        return Vec::new();
    }
    report
        .checks
        .iter()
        .filter(|c| c.denies())
        .map(|c| c.name.clone())
        .chain(report.skipped.iter().cloned())
        .collect()
}
