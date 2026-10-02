//! BUILT-IN FLOORS — the deterministic evidence floor the shipped workflows pin onto their
//! Evaluator phases (FINDING-025 fix item 1).
//!
//! ## The defect this closes
//!
//! Every built-in def shipped with `validator_pin = null` on every phase. That makes all three gate
//! layers inert at once, because two of them are keyed off the pin:
//!
//! | layer | gate condition | state with no pin |
//! |---|---|---|
//! | 1 — deterministic floor | `unit.validator.is_some()` | `has_deterministic_floor: false`, `deterministic_pass` **vacuously true** |
//! | 2 — agent semantic judge | `unit.validator.filter(\|v\| v.approved)` | judge never runs, `agent_verdict: null` |
//! | 3 — evaluator≠creator pass | policy engine `select` + `decide_as` | runs, selects nothing, **default-allow** |
//!
//! So a shipped `feature`/`bug`/`migration` run passed every gate without anything ever
//! being checked against a criterion. Pinning a floor onto the Evaluator phases engages layers 1
//! and 2 together — layer 2 is gated on the same `Option` layer 1 is.
//!
//! ## What the floor asserts
//!
//! [`EVIDENCE_CRITERION`]: the run left a change in its worktree. This is the product thesis stated
//! as a check — "done" is re-derived from the diff, never asserted by the worker that claims it. It
//! is repo-agnostic and needs no per-project configuration, which is what lets it ship pinned.
//!
//! The floor is sound *because of how the run's worktree is made*
//! ([`crate::repo::create_worktree`]): a fresh `git worktree add -b wicked/<run-id>`, so the tree
//! starts CLEAN, and nothing in the engine commits. The check therefore reports exactly the changes
//! THIS RUN produced, in either of the two places a worker can legitimately leave them:
//!
//! - **uncommitted** — `git status --porcelain` (tracked modification, deletion, untracked file);
//! - **committed** — commits reachable from `HEAD` but from no non-`wicked/*` local branch
//!   (`--not --exclude='wicked/*' --branches`: every non-run-branch is subtracted; sibling run
//!   branches are exempt but unreachable from this `HEAD` unless deliberately merged, so what
//!   remains is the commits the run itself made on its `wicked/<run-id>` branch).
//!
//! The second clause is core#280's fix. The first shipped alone, with a soundness note claiming "a
//! creator's work cannot be hidden from it by a commit" — false, and proven false by the first run
//! whose worker was told to commit incrementally (a liveness contract): 838 committed lines of
//! deliverable, porcelain clean, gate DENIED. A floor that punishes committing teaches workers to
//! leave work uncommitted, which is the opposite of the evidence discipline the product wants.
//! An operator's pre-existing dirt still cannot satisfy the floor vacuously: the worktree starts
//! clean and its branch starts at the base tip.
//!
//! ## Honest limits
//!
//! - It is a floor, not a review. It proves a change EXISTS; it says nothing about whether the
//!   change is correct, or even related to the task. Layer 2 (the agent judge the pin now also
//!   switches on) and layer 3 (policy) are what reason about content.
//! - A repo-less run has no worktree, and `pinned_validator_denial` is fail-closed on that by
//!   design — so a repo-less run cannot satisfy a pinned phase. Only Evaluator phases in
//!   repo-targeting workflows carry the pin, and the Evaluator phases that carry it sit behind
//!   `HumanConfirmIf(VerdictNotPass)` / `HumanConfirm`, so a denial routes to a human rather than
//!   silently killing a run.
//! - A worker that touches a file for the sake of touching it passes. The floor raises the bar from
//!   "assert done" to "produce something"; it does not close it.
//!
//! ## Why it can ship pinned at all
//!
//! `attach_pinned_validators` is fail-closed on a pin that is not in the vault: an unseeded pin
//! would bail every run of every built-in. [`seed_builtin_floors`] is therefore called on the PLAN
//! path — `pipeline::pre_distribute`, immediately before the attach — which is the one choke point
//! every entry crosses. That placement is the guarantee, and it was learned the hard way: seeding at
//! actor boot alone was correct for the daemon and broken for `run_session`, which is public, takes
//! a store directly, and never constructs an actor. A floor that depends on which entry point you
//! came through is not a floor.
//!
//! The actor still seeds at boot, but only as an early warning and to make the floor visible in the
//! vault before a first run — not as the thing that makes a plan resolve.
//!
//! Seeding per plan is affordable because it is idempotent (content-addressed) and cheap (six
//! `put_node`s that collapse onto themselves: an unapproved + approved copy of the evidence floor
//! and of each of the two walkthrough pins, WT-C1).

use crate::validator::DeterministicValidator;

/// The acceptance criterion of the evidence floor. Phrased as the property being asserted, because
/// it is what an operator sees in a denial (`pinned validator failed: <criterion>`) and in the
/// Decisions ledger.
pub const EVIDENCE_CRITERION: &str =
    "the run left a change in its worktree (done is re-derived from the diff, never asserted)";

/// The deterministic re-verify: exit 0 IFF the run's worktree carries any change, committed or not.
/// Clause 1 catches uncommitted work: tracked modification, deletion, or untracked new file
/// (`--porcelain` reports all three; `grep -q .` turns "at least one line" into the exit status).
/// Clause 2 catches committed work (core#280): any commit reachable from `HEAD` but from no
/// NON-`wicked/*` local branch. Precisely: `--not --exclude='wicked/*' --branches` subtracts every
/// local branch whose name does not match `wicked/*` — the base branch's history never counts. Run
/// branches (this run's and siblings') are exempt from subtraction; that is safe because a sibling
/// run's commits are only reachable from THIS run's `HEAD` if this run deliberately merged them,
/// so in practice the surviving set is exactly the commits this run authored on its own branch.
///
/// Fails closed by construction in every degenerate case. A non-git workdir makes both `git`
/// invocations exit 128 with their error on STDERR, so each `grep` sees empty stdin and the script
/// exits non-zero — a DENY, consistent with the module-level rule that "can't re-verify" is treated
/// as NOT-passed.
///
/// Built only from `git`/`grep`/`|` so it passes the [`looks_dangerous`](crate::validator) denylist.
/// That denylist rejects the substrings `>`, `/dev/`, `:(){`, `$(` and a backtick, plus a table of
/// whole-word tokens (`rm`, `curl`, `sudo`, `eval`, `exec`, …) — this script carries none of them.
/// A single `|` is deliberately NOT denied (denying it would also flag every legitimate `||`), and
/// pipe-plus-or is what lets this express "any line in either place" without command substitution.
/// The `'wicked/*'` quoting keeps `sh -c` from globbing the pattern against the run dir.
pub const EVIDENCE_SCRIPT: &str = "git status --porcelain | grep -q . || git log --oneline HEAD --not --exclude='wicked/*' --branches | grep -q .";

/// The APPROVED content-address pin the built-in Evaluator phases carry. Content-hash over
/// `(EVIDENCE_CRITERION, EVIDENCE_SCRIPT, approved=true)` — see [`crate::validator_vault::pin`].
/// Re-derived and asserted equal to the vaulted approved copy by
/// [`tests::seeded_pin_matches_the_constant_the_builtins_carry`]; if the criterion or the script
/// ever changes, that test fails loudly and this const must be regenerated.
pub const EVIDENCE_FLOOR_PIN: &str = "e2e7af1db9e48454";

/// The authored (UNAPPROVED) evidence floor — the artifact a human/council reviews before it can
/// gate. Authoring never authorizes running: `approved == false` (rev0.4 fork 3). Route it through
/// [`seed_builtin_floors`] to obtain the gate-ready approved pin.
#[must_use]
pub fn evidence_floor_validator() -> DeterministicValidator {
    DeterministicValidator {
        criterion: EVIDENCE_CRITERION.to_string(),
        script: EVIDENCE_SCRIPT.to_string(),
        approved: false,
    }
}

/// WT-C1 (DES-walkthrough-proof §4.3, B1): the criterion of the `walkthrough_plan` entry's pin.
pub const WALKTHROUGH_LINT_CRITERION: &str =
    "the walkthrough storyline passes garden's deterministic lint (a weak proof is never recorded)";

/// The `walkthrough_plan` pin's script: garden's walkthrough lint over the run's evidence root.
/// Only `${VAR}` expansion (no `>`, no `$(`, no backtick), so it passes `looks_dangerous`. Both
/// variables are injected for this entry's units only (WT-C2); unset, `test -n` denies.
pub const WALKTHROUGH_LINT_SCRIPT: &str = "test -n \"${WICKED_GARDEN_ROOT}\" && test -n \"${WICKED_EVIDENCE_ROOT}\" && \"${WICKED_GARDEN_ROOT}/scripts/wicked-garden\" run scripts/demo/walkthrough.mjs lint --root \"${WICKED_EVIDENCE_ROOT}\"";

/// The APPROVED pin the `walkthrough_plan` catalog entry carries (content hash over
/// `(WALKTHROUGH_LINT_CRITERION, WALKTHROUGH_LINT_SCRIPT, approved=true)`), re-derived by
/// [`tests::seeded_walkthrough_pins_match_the_constants_the_catalog_carries`].
pub const WALKTHROUGH_LINT_PIN: &str = "1aa3f15487018f68";

/// WT-C1: the criterion of the `walkthrough_review` entry's pin.
pub const WALKTHROUGH_RESULT_CRITERION: &str =
    "the walkthrough's sealed result is PASS, with no FAIL or INCONCLUSIVE chapter";

/// The `walkthrough_review` pin's script: reads one small result file, so the validator bound holds
/// whatever the chapter count. A missing file, a non-PASS overall or any FAIL/INCONCLUSIVE verdict
/// denies.
///
/// Hardened from the DES's verbatim script (codex review on WT-C1): whitespace around `:` is
/// tolerated in both directions, and a FAIL/INCONCLUSIVE `overall` anywhere denies too, so a
/// pretty-printed result cannot hide a failing verdict and a stray `"overall":"PASS"` beside a
/// failing one cannot pass. Deny dominates.
pub const WALKTHROUGH_RESULT_SCRIPT: &str = "test -n \"${WICKED_EVIDENCE_ROOT}\" && test -f \"${WICKED_EVIDENCE_ROOT}/result.json\" && grep -Eq '\"overall\"[[:space:]]*:[[:space:]]*\"PASS\"' \"${WICKED_EVIDENCE_ROOT}/result.json\" && ! grep -Eq '\"(overall|verdict)\"[[:space:]]*:[[:space:]]*\"(FAIL|INCONCLUSIVE)\"' \"${WICKED_EVIDENCE_ROOT}/result.json\"";

/// The APPROVED pin the `walkthrough_review` catalog entry carries.
pub const WALKTHROUGH_RESULT_PIN: &str = "cd95e6e0acdb4d8b";

/// The two authored (UNAPPROVED) walkthrough validators, lint then result.
#[must_use]
pub fn walkthrough_validators() -> [DeterministicValidator; 2] {
    [
        DeterministicValidator {
            criterion: WALKTHROUGH_LINT_CRITERION.to_string(),
            script: WALKTHROUGH_LINT_SCRIPT.to_string(),
            approved: false,
        },
        DeterministicValidator {
            criterion: WALKTHROUGH_RESULT_CRITERION.to_string(),
            script: WALKTHROUGH_RESULT_SCRIPT.to_string(),
            approved: false,
        },
    ]
}

/// Vault + approve every floor the built-in defs pin, returning the approved evidence-floor pin
/// (== [`EVIDENCE_FLOOR_PIN`]).
///
/// Called by the actor right after the store opens, on the single-writer thread. That placement is
/// load-bearing, not incidental: `attach_pinned_validators` BAILS a run whose phase pins a validator
/// the vault does not hold, so shipping a pin in a built-in def is only safe if the seed provably
/// runs before any plan. Idempotent — the vault is content-addressed, so re-seeding an already
/// seeded store rewrites the same six nodes.
///
/// Goes through the same author → vault-unapproved → APPROVE path an operator's
/// `provision-validator` / `approve-validator` pair does, rather than writing an approved node
/// directly: the approval is a distinct, audited step, and the floor should not get to skip it just
/// because we ship it.
///
/// Also seeds the two walkthrough pins (WT-C1): the catalog carries them as data, and
/// `attach_pinned_validators` bails a plan whose entry pins an unvaulted validator — so seeding
/// them here, on the same choke point, keeps a PA-added walkthrough step from bailing its run.
pub fn seed_builtin_floors(store: &mut dyn wicked_apps_core::GraphStore) -> anyhow::Result<String> {
    for v in walkthrough_validators() {
        let unapproved = crate::validator_vault::store_validator(store, &v)?;
        crate::validator_vault::approve_and_store(store, &unapproved)?.ok_or_else(|| {
            anyhow::anyhow!("a walkthrough pin vanished from the vault between store and approve")
        })?;
    }
    let unapproved = crate::validator_vault::store_validator(store, &evidence_floor_validator())?;
    crate::validator_vault::approve_and_store(store, &unapproved)?.ok_or_else(|| {
        anyhow::anyhow!("evidence floor vanished from the vault between store and approve")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validator::run_validator;
    use crate::validator_vault::load_validator;
    use crate::workflow::{PhaseRole, WorkflowRegistry};
    use std::process::Command;
    use wicked_apps_core::open_store;

    /// A fresh, empty scratch dir. Matches the codebase idiom (`domain_extraction`,
    /// `validator_vault`): no `tempfile` dev-dep, pid- AND name-scoped so concurrent test threads
    /// never share one.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wicked-floors-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build the real thing the floor runs against: a repo with a clean linked worktree on a
    /// `wicked/<run>` branch, exactly as `repo::create_worktree` makes one.
    fn repo_with_worktree(base: &std::path::Path) -> std::path::PathBuf {
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            // spawn-audit: test-only — a git fixture building the worktree layout under test; it reads no engine state.
            let out = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q", "."]);
        git(&["config", "user.email", "t@example.invalid"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "base\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);
        let wt = repo.join(".wicked").join("worktrees").join("run1");
        git(&[
            "worktree",
            "add",
            "-q",
            wt.to_str().unwrap(),
            "-b",
            "wicked/run1",
        ]);
        wt
    }

    #[test]
    fn seeded_pin_matches_the_constant_the_builtins_carry() {
        // The built-in defs embed EVIDENCE_FLOOR_PIN as data. If the criterion or script drifts, the
        // content-address moves and every built-in run would bail fail-closed at plan time with an
        // unresolvable pin. Catch that here, at the source, instead of in a live run.
        let dir = scratch("pin");
        let mut store = open_store(Some(dir.join("v.db").to_str().unwrap())).unwrap();
        let approved = seed_builtin_floors(&mut store).unwrap();
        assert_eq!(
            approved, EVIDENCE_FLOOR_PIN,
            "approved pin drifted from the const the built-in defs embed — regenerate \
             EVIDENCE_FLOOR_PIN"
        );
        let loaded = load_validator(&store, EVIDENCE_FLOOR_PIN).unwrap().unwrap();
        assert!(loaded.approved, "the pin the defs carry must be APPROVED");
        assert_eq!(loaded.script, EVIDENCE_SCRIPT);
    }

    #[test]
    fn seeding_twice_is_idempotent() {
        // The actor seeds on EVERY store open. If that were not idempotent it would fork the vault
        // on the second launch and the pin the defs carry would stop resolving.
        let dir = scratch("idem");
        let mut store = open_store(Some(dir.join("v.db").to_str().unwrap())).unwrap();
        let first = seed_builtin_floors(&mut store).unwrap();
        let second = seed_builtin_floors(&mut store).unwrap();
        assert_eq!(first, second);
        assert!(load_validator(&store, &second).unwrap().unwrap().approved);
    }

    #[test]
    fn floor_denies_a_run_that_changed_nothing_and_passes_one_that_did() {
        // The whole point, measured through the REAL gate path (`run_validator`: approval check,
        // denylist, cleared env, pinned cwd, OS sandbox where available) against a REAL linked
        // worktree — not against a hand-rolled temp dir that would not exercise git's worktree
        // indirection (`.git` is a FILE pointing outside the run dir; the sandbox restricts writes
        // to the run dir, so this is exactly where the script could break in production).
        let dir = scratch("worktree");
        let wt = repo_with_worktree(&dir);
        let v = evidence_floor_validator().approve();

        assert!(
            !run_validator(&v, &wt).unwrap(),
            "a worker that asserted done without touching the tree must be DENIED"
        );

        std::fs::write(wt.join("new.txt"), "work\n").unwrap();
        assert!(
            run_validator(&v, &wt).unwrap(),
            "an untracked new file is evidence and must PASS"
        );

        std::fs::remove_file(wt.join("new.txt")).unwrap();
        std::fs::write(wt.join("a.txt"), "modified\n").unwrap();
        assert!(
            run_validator(&v, &wt).unwrap(),
            "a tracked modification is evidence and must PASS"
        );
    }

    #[test]
    fn floor_passes_a_run_that_committed_its_work() {
        // core#280: a worker under an incremental-commit contract leaves porcelain CLEAN — its work
        // is in commits on the run's `wicked/<run>` branch. The first shipped floor read porcelain
        // only and DENIED such a run (838 committed lines of deliverable, gate: "no change in its
        // worktree"). Clause 2 must see the commits; and after the commit, clause 1 must genuinely
        // be the one that failed (asserted by construction: `git status` is clean post-commit).
        let dir = scratch("committed");
        let wt = repo_with_worktree(&dir);
        let v = evidence_floor_validator().approve();

        std::fs::write(wt.join("deliverable.md"), "the work\n").unwrap();
        let git = |args: &[&str]| {
            // spawn-audit: test-only — commits the fixture worker's work in the worktree under test.
            let out = Command::new("git")
                .args(args)
                .current_dir(&wt)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["add", "-A"]);
        git(&["commit", "-qm", "docs: the run's committed deliverable"]);

        // Premise guard: the tree really is clean now, so only clause 2 can pass this.
        // spawn-audit: test-only — asserts the premise (clean porcelain) that makes this test mean something.
        let porcelain = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&wt)
            .output()
            .expect("git runs");
        assert!(
            porcelain.stdout.iter().all(|b| b.is_ascii_whitespace()),
            "premise broken: porcelain not clean after commit, clause 1 would mask clause 2"
        );

        assert!(
            run_validator(&v, &wt).unwrap(),
            "committed work IS evidence — a worker must not be punished for committing (core#280)"
        );
    }

    #[test]
    fn floor_fails_closed_outside_a_git_repo() {
        // `git status` exits 128 and writes to stderr, so grep sees empty stdin. Asserted rather
        // than assumed: the module claims this is a DENY, and a silent PASS here would be a hole in
        // every non-git workdir.
        let dir = scratch("nongit");
        std::fs::write(dir.join("stray.txt"), "not a repo\n").unwrap();

        // Guard the premise instead of assuming it. The scratch dir is under the system temp dir,
        // which is not inside a repo on any platform we build on — but if some host ever made that
        // false, `git status` would SUCCEED and this test would fail while pointing at the wrong
        // thing. Check the premise directly so a violation reads as a violation.
        // spawn-audit: test-only — checks the premise that the scratch dir is outside a repo — plain `git status`.
        let outside = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&dir)
            .output()
            .expect("git runs");
        assert!(
            !outside.status.success(),
            "premise broken: the scratch dir at {} is INSIDE a git repo, so this test cannot \
             exercise the non-repo path",
            dir.display()
        );

        let v = evidence_floor_validator().approve();
        assert!(
            !run_validator(&v, &dir).unwrap(),
            "a workdir that is not a git repo must DENY, never vacuously pass"
        );
    }

    /// FINDING-025 item 1 as an executable invariant, in BOTH directions.
    ///
    /// The floor asserts over a worktree DIFF, so it is the right instrument exactly when the
    /// workflow is expected to produce one — i.e. when a code-writing (`executes_code`) Creator runs
    /// before the Evaluator. Pinning it more widely than that would not be stricter governance, it
    /// would be a FALSE gate: an Evaluator that judges prose (no code-writing Creator upstream)
    /// would be denied on every run for the wrong reason. No compiled built-in has one since
    /// `collab` was deleted (DES-TEAMING-002, 2026-09-26), and the exempt list is pinned empty.
    ///
    /// Both halves are asserted because each catches a different regression: a new code workflow
    /// that ships an ungated Evaluator re-opens the finding, and a floor pinned onto a non-code
    /// Evaluator breaks that workflow outright.
    #[test]
    fn the_floor_is_pinned_exactly_where_a_diff_is_the_evidence() {
        let reg = WorkflowRegistry::with_defaults();
        let (mut gated, mut exempt) = (Vec::new(), Vec::new());
        let mut code_creators = Vec::new();

        for id in reg.ids() {
            let def = reg.get(&id).unwrap();
            for (i, phase) in def.phases.iter().enumerate() {
                // F-039: the code-writing Creator's OWN gate must evaluate something too — the
                // same floor, at the phase that was supposed to make the change, so its gate
                // never folds `combined: true` over nothing (registration refuses otherwise).
                if phase.role == PhaseRole::Creator && phase.executes_code {
                    assert_eq!(
                        phase.validator_pin.as_deref(),
                        Some(EVIDENCE_FLOOR_PIN),
                        "`{id}/{}` writes code but carries no floor of its own — its gate would \
                         evaluate nothing (F-039)",
                        phase.id
                    );
                    code_creators.push(format!("{id}/{}", phase.id));
                    continue;
                }
                if phase.role != PhaseRole::Evaluator {
                    continue;
                }
                let writes_code_upstream = def.phases[..i]
                    .iter()
                    .any(|p| p.executes_code && p.role == PhaseRole::Creator);
                let target = format!("{id}/{}", phase.id);
                if writes_code_upstream {
                    assert_eq!(
                        phase.validator_pin.as_deref(),
                        Some(EVIDENCE_FLOOR_PIN),
                        "`{target}` evaluates a code-writing Creator but carries no deterministic \
                         floor — gate layers 1 AND 2 are inert for it (FINDING-025 item 1)"
                    );
                    gated.push(target);
                } else {
                    assert_eq!(
                        phase.validator_pin, None,
                        "`{target}` has no code-writing Creator upstream, so a worktree-DIFF floor \
                         would deny every run of `{id}`. This phase needs a floor suited to its own \
                         evidence, not this one."
                    );
                    exempt.push(target);
                }
            }
        }

        assert_eq!(
            gated,
            vec![
                "bug/verify",
                "feature/adversarial-review",
                "migration/verify"
            ],
            "the code-writing built-ins are the ones that must be gated"
        );
        assert!(
            exempt.is_empty(),
            "no compiled built-in has an Evaluator that judges prose rather than a diff; one that \
             appears ships ungated and needs a floor suited to its own evidence: {exempt:?}"
        );
        assert_eq!(
            code_creators,
            vec!["bug/fix", "feature/build", "migration/execute"],
            "the code-writing Creators of the built-ins, each carrying its own floor (F-039)"
        );
    }

    /// The same invariant, for the workflows shipped as DROP-IN JSON rather than compiled in.
    ///
    /// The test above reads `WorkflowRegistry::with_defaults()` — the COMPILED defs. `workflows/`
    /// ships JSON, and a same-id file replaces the compiled def wholesale (`load_dir` runs after
    /// `with_defaults`), so those files are what actually reach the engine. Some are copies of a
    /// compiled built-in (`feature`, `bug`, `migration`); `domain-extraction` exists only as JSON,
    /// and the compiled test never saw it. An Evaluator could ship there with no floor and nothing would notice
    /// (FINDING-074, #176).
    ///
    /// The rule differs from the built-in one in the negative branch. A built-in Evaluator with no
    /// code-writing Creator upstream must carry NO pin, because the only floor those have is the
    /// worktree-DIFF one and pinning it would deny every run. A drop-in may legitimately carry its
    /// OWN floor instead — `domain-extraction/coverage` does, `COVERAGE_VALIDATOR_PIN` over a
    /// `coverage-report.json` deliverable. So the assertion here is "must not carry the DIFF floor",
    /// plus an exact classification of which drop-in Evaluators are floored and which are not.
    ///
    /// `ungated` is asserted empty. Its one entry, `domain-graph-slice/validate` (an Evaluator with
    /// no floor, no deliverable and an `auto` gate — nothing it could deny on), left with the
    /// workflow when it was deleted (DES-TEAMING-002, 2026-09-26). An ungated Evaluator that
    /// appears fails here.
    #[test]
    fn no_shipped_drop_in_ships_an_evaluator_nobody_checked() {
        let workflows_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("workflows");
        let (mut diff_floored, mut own_floored, mut ungated) = (Vec::new(), Vec::new(), Vec::new());
        let mut code_creators = Vec::new();
        let mut files = 0;

        for entry in std::fs::read_dir(&workflows_dir).expect("workflows/ is readable") {
            let path = entry.expect("readable dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            files += 1;
            let def = WorkflowRegistry::def_from_file(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            for (i, phase) in def.phases.iter().enumerate() {
                // F-039: the shipped JSON copies must pin the code-writing Creators exactly as the
                // compiled defs do — a JSON that lost this pin would be REFUSED at registration
                // (`GateEvaluatesNothing`), so this check is what keeps the shipped files loadable.
                if phase.role == PhaseRole::Creator && phase.executes_code {
                    assert_eq!(
                        phase.validator_pin.as_deref(),
                        Some(EVIDENCE_FLOOR_PIN),
                        "`{}/{}` writes code but its shipped JSON carries no floor (F-039)",
                        def.id,
                        phase.id
                    );
                    code_creators.push(format!("{}/{}", def.id, phase.id));
                    continue;
                }
                if phase.role != PhaseRole::Evaluator {
                    continue;
                }
                let writes_code_upstream = def.phases[..i]
                    .iter()
                    .any(|p| p.executes_code && p.role == PhaseRole::Creator);
                let target = format!("{}/{}", def.id, phase.id);
                match phase.validator_pin.as_deref() {
                    Some(EVIDENCE_FLOOR_PIN) => {
                        assert!(
                            writes_code_upstream,
                            "`{target}` pins the worktree-DIFF floor but no code-writing Creator \
                             runs before it, so the floor would deny every run of `{}`",
                            def.id
                        );
                        diff_floored.push(target);
                    }
                    Some(_) => own_floored.push(target),
                    None => ungated.push(target),
                }
            }
        }

        // Guards against the whole test passing vacuously if the directory moves or empties — the
        // failure mode the sibling drop-in test in `workflow.rs` also had to close (#175).
        assert!(files > 0, "workflows/ shipped no drop-in defs to check");

        diff_floored.sort();
        own_floored.sort();
        ungated.sort();
        code_creators.sort();
        assert_eq!(
            code_creators,
            vec!["bug/fix", "feature/build", "migration/execute"],
            "the shipped JSON copies of the code-writing workflows must pin their Creator phases"
        );

        assert_eq!(
            diff_floored,
            vec![
                "bug/verify",
                "feature/adversarial-review",
                "migration/verify"
            ],
            "the shipped copies of the code-writing workflows must carry the DIFF floor, exactly as \
             their compiled counterparts do — if a JSON here lost the pin it would silently replace \
             a floored built-in with an unfloored one (FINDING-049's shape)"
        );
        assert_eq!(
            own_floored,
            vec!["domain-extraction/coverage"],
            "the drop-in Evaluators that carry a floor suited to their own evidence"
        );
        assert!(
            ungated.is_empty(),
            "an Evaluator with no floor has nothing it can deny on (#176): a shipped workflow \
             carries an unfalsifiable review step — give it a floor: {ungated:?}"
        );
    }

    /// WT-C1: each walkthrough pin the catalog carries is the content address of its seeded,
    /// APPROVED validator, and both scripts pass the denylist that gates every pinned script.
    #[test]
    fn seeded_walkthrough_pins_match_the_constants_the_catalog_carries() {
        let dir = scratch("walkthrough-pins");
        let mut store = open_store(Some(dir.join("v.db").to_str().unwrap())).unwrap();
        seed_builtin_floors(&mut store).unwrap();
        for (v, want) in walkthrough_validators()
            .iter()
            .zip([WALKTHROUGH_LINT_PIN, WALKTHROUGH_RESULT_PIN])
        {
            let approved = DeterministicValidator {
                approved: true,
                ..v.clone()
            };
            assert_eq!(
                crate::validator_vault::pin(&approved),
                want,
                "{}",
                v.criterion
            );
            let loaded = load_validator(&store, want).unwrap().expect("seeded");
            assert!(loaded.approved);
            assert_eq!(
                crate::validator::looks_dangerous(&v.script),
                None,
                "denylist-dirty: {}",
                v.script
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WT-C1: the result script's verdict table, run through `sh -c` against fixture roots (the
    /// engine-side `WICKED_EVIDENCE_ROOT` injection and the run under `run_validator_reporting`
    /// are WT-C2). PASS passes; FAIL, INCONCLUSIVE, a missing file, an unset root, a
    /// pretty-printed failing verdict and a contradictory `overall` all deny.
    #[cfg(unix)]
    #[test]
    fn the_walkthrough_result_script_passes_only_a_clean_pass() {
        let dir = scratch("walkthrough-result");
        let cases: [(&str, Option<&str>, bool); 8] = [
            (
                "pass",
                Some(r#"{"overall":"PASS","chapters":[{"verdict":"PASS"}]}"#),
                true,
            ),
            (
                "pass-pretty",
                Some("{\n  \"overall\": \"PASS\",\n  \"chapters\": [{ \"verdict\": \"PASS\" }]\n}"),
                true,
            ),
            (
                "fail",
                Some(r#"{"overall":"FAIL","chapters":[{"verdict":"FAIL"}]}"#),
                false,
            ),
            (
                "inconclusive",
                Some(r#"{"overall":"PASS","chapters":[{"verdict":"INCONCLUSIVE"}]}"#),
                false,
            ),
            (
                "pretty-fail",
                Some("{\"overall\":\"PASS\",\"chapters\":[{\"verdict\": \"FAIL\"}]}"),
                false,
            ),
            (
                "contradictory",
                Some(r#"{"overall":"FAIL","meta":{"overall":"PASS"}}"#),
                false,
            ),
            ("missing", None, false),
            ("unset", None, false),
        ];
        for (name, body, want) in cases {
            let root = dir.join(name);
            std::fs::create_dir_all(&root).unwrap();
            if let Some(b) = body {
                std::fs::write(root.join("result.json"), b).unwrap();
            }
            use wicked_apps_core::spawn::HardenedCommand;
            let mut cmd = Command::new("sh");
            cmd.hardened()
                .arg("-c")
                .arg(WALKTHROUGH_RESULT_SCRIPT)
                .env_clear();
            if name != "unset" {
                cmd.env("WICKED_EVIDENCE_ROOT", &root);
            }
            let ok = cmd.status().unwrap().success();
            assert_eq!(ok, want, "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
