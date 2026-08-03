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
//! So a shipped `feature`/`bug`/`migration`/`collab` run passed every gate without anything ever
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
//! starts CLEAN, and nothing in the engine commits. `git status --porcelain` at gate time therefore
//! reports exactly the changes THIS RUN produced — an operator's pre-existing dirt cannot satisfy
//! the floor vacuously, and a creator's work cannot be hidden from it by a commit.
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
//! Seeding per plan is affordable because it is idempotent (content-addressed) and cheap (two
//! `put_node`s that collapse onto themselves).

use crate::validator::DeterministicValidator;

/// The acceptance criterion of the evidence floor. Phrased as the property being asserted, because
/// it is what an operator sees in a denial (`pinned validator failed: <criterion>`) and in the
/// Decisions ledger.
pub const EVIDENCE_CRITERION: &str =
    "the run left a change in its worktree (done is re-derived from the diff, never asserted)";

/// The deterministic re-verify: exit 0 IFF the run's worktree carries any change — tracked
/// modification, deletion, or untracked new file (`--porcelain` reports all three; `grep -q .`
/// turns "at least one line" into the exit status).
///
/// Fails closed by construction in every degenerate case. A non-git workdir makes `git status` exit
/// 128 and write its error to STDERR, so `grep` sees empty stdin and the script exits non-zero — a
/// DENY, consistent with the module-level rule that "can't re-verify" is treated as NOT-passed.
///
/// Built only from `git`/`grep`/`|` so it passes the [`looks_dangerous`](crate::validator) denylist.
/// That denylist rejects the substrings `>`, `/dev/`, `:(){`, `$(` and a backtick, plus a table of
/// whole-word tokens (`rm`, `curl`, `sudo`, `eval`, `exec`, …) — this script carries none of them.
/// A single `|` is deliberately NOT denied (denying it would also flag every legitimate `||`), and
/// that pipe is what lets this express "any line at all" without command substitution.
pub const EVIDENCE_SCRIPT: &str = "git status --porcelain | grep -q .";

/// The APPROVED content-address pin the built-in Evaluator phases carry. Content-hash over
/// `(EVIDENCE_CRITERION, EVIDENCE_SCRIPT, approved=true)` — see [`crate::validator_vault::pin`].
/// Re-derived and asserted equal to the vaulted approved copy by
/// [`tests::seeded_pin_matches_the_constant_the_builtins_carry`]; if the criterion or the script
/// ever changes, that test fails loudly and this const must be regenerated.
pub const EVIDENCE_FLOOR_PIN: &str = "2fcde907d57f3ee2";

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

/// Vault + approve every floor the built-in defs pin, returning the approved evidence-floor pin
/// (== [`EVIDENCE_FLOOR_PIN`]).
///
/// Called by the actor right after the store opens, on the single-writer thread. That placement is
/// load-bearing, not incidental: `attach_pinned_validators` BAILS a run whose phase pins a validator
/// the vault does not hold, so shipping a pin in a built-in def is only safe if the seed provably
/// runs before any plan. Idempotent — the vault is content-addressed, so re-seeding an already
/// seeded store rewrites the same two nodes.
///
/// Goes through the same author → vault-unapproved → APPROVE path an operator's
/// `provision-validator` / `approve-validator` pair does, rather than writing an approved node
/// directly: the approval is a distinct, audited step, and the floor should not get to skip it just
/// because we ship it.
pub fn seed_builtin_floors(store: &mut dyn wicked_apps_core::GraphStore) -> anyhow::Result<String> {
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
    /// would be a FALSE gate: `collab` (propose → critique → revise → verdict) never writes code, so
    /// a diff floor would deny every one of its runs for the wrong reason.
    ///
    /// Both halves are asserted because each catches a different regression: a new code workflow
    /// that ships an ungated Evaluator re-opens the finding, and a floor pinned onto a non-code
    /// Evaluator breaks that workflow outright.
    #[test]
    fn the_floor_is_pinned_exactly_where_a_diff_is_the_evidence() {
        let reg = WorkflowRegistry::with_defaults();
        let (mut gated, mut exempt) = (Vec::new(), Vec::new());

        for id in reg.ids() {
            let def = reg.get(&id).unwrap();
            for (i, phase) in def.phases.iter().enumerate() {
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
        assert_eq!(
            exempt,
            vec!["collab/critique", "collab/verdict"],
            "collab is the only built-in whose Evaluators judge prose rather than a diff; if this \
             list grows, those workflows are shipping ungated and need their own floor"
        );
    }
}
