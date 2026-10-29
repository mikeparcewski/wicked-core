//! REPO CHECKS FLOOR — the `verify` phase re-derives "done" by RUNNING the repository's own checks
//! (F-039).
//!
//! ## The defect this closes
//!
//! A governed `bug` run's `verify` phase was judged on exactly one deterministic criterion: "the
//! run left a change in its worktree". The seat's transcript claimed `npm test: 2,669 tests
//! passed` and `typecheck exit 0`, and nothing re-derived either claim — the run's "verified"
//! state rested on the evaluator's own account (the same evaluator that had rewritten the code,
//! F-036). "Done is re-derived from evidence, never asserted" held only for "a diff exists".
//!
//! ## What this floor does
//!
//! For a unit whose phase is the code-verifying step of a def — `verified_evidence: true` with an
//! `executes_code` Creator upstream ([`crate::domain::WorkUnit::repo_checks_floor`], set at plan
//! time) — the ENGINE, after the seat's work finishes and BEFORE the gate folds, detects the
//! repository's own check commands in the run's worktree and runs them itself, off the actor
//! thread:
//!
//! * `package.json` → its `typecheck`, `lint` and `test` scripts (those present, in that order),
//!   via the package manager the lockfile names (`pnpm-lock.yaml` → pnpm, `yarn.lock` → yarn, else
//!   npm), preceded by an install when `node_modules/` is absent (`npm ci` with a lockfile, `npm
//!   install` without; the pnpm/yarn frozen-lockfile equivalents) — ALWAYS with
//!   `--ignore-scripts`: a dependency's lifecycle script is the one piece of repo-controlled code
//!   the floor has no reason to run;
//! * `Cargo.toml` → `cargo fmt --all --check` (formatting checked first, before tests), then
//!   `cargo test`.
//!
//! Each command's exit code and duration are captured as a [`CheckRun`], and both of its streams
//! are stored whole, content-addressed, in the run's log store ([`crate::floor_log`]); the run is
//! attached to the gate: the fold persists the [`RepoChecksReport`] on the unit, emits
//! `repoChecksEvaluated` (each check with its [`CheckDecision`]), and DENIES the unit when any
//! check's decision is not a pass, times out, or cannot be spawned (fail-closed — a check that
//! cannot run has re-derived nothing). Checks stop at the first denying failure: the
//! evidence of the failure is what the gate needs, and a failing typecheck makes the suite behind
//! it moot.
//!
//! ## The creator owes the floor too (core#467)
//!
//! The floor first ran only at `verify`. On 2026-09-13 a `fix` worker left a tree failing
//! `npm run typecheck` (exit 2) and `npm run lint` (exit 1), called the error "pre-existing"
//! (the base was clean), was judged PASS on "left a change", and the red tree reached the
//! read-only evaluator — which fixed it in place, tripped the worktree guard, and the run was lost
//! with no route back. The planner now marks the def's `executes_code` Creator phase with the
//! DEFAULT floor even when a later phase verifies ([`crate::plan::plan_from_def`]): the same
//! provision + typecheck + lint + tests set runs at the END of the creator's phase, in its
//! worktree, and a red floor denies the creator's unit with the check tails on the record
//! ([`FloorStage::Creator`]). The verify phase keeps its own floor.
//!
//! ## Classification: a timeout is not a failure; a base failure is not a regression (core#469)
//!
//! A check that did not FINISH is `timed_out`, distinct from `failed`: the change is unverified by
//! it, not refuted (a full `npm test` that takes 5 min in CI took 16 under acceptance-host load and
//! hit the fixed 1200 s bound — the correct fix was DENIED). The fold carries the classification on
//! every check (`outcome`) and denies a timeout under its own source (`repo_checks_timeout`) so the
//! gate can offer extend / targeted / accept instead of retry-the-same-tree.
//!
//! ## The decision: the suite's TOC plus exit codes (DES-FLOOR-V2)
//!
//! Two inputs are trusted: the BASE tree (the run base, exported through the PINNED git dir — no
//! nested worktree — run once per run and cached by base sha + check name + argv, the export
//! removed before the next head run) and the EXIT CODES of the processes the floor launches. The
//! head's output text is evidence only: twenty-one review rounds of core#1060 each found a new way
//! for a head's own text to excuse a failure, so no head text decides anything here.
//!
//! * The head check runs unchanged. Exit 0 is a `pass`; a signal, or (for a check of a usable
//!   build guide) an exit outside its `failedExit`, is `could_not_run`.
//! * Otherwise the base runs the same argv. A green base makes the failure a `new_failure`. A red
//!   base, for a check of the base's build guide ([`crate::floor_guide`], DES-BUILD-GUIDE) whose
//!   probes passed, is listed by the guide's `list` piece; its failing tests are read from the
//!   BASE results, kept only when the base list names them, and each is confirmed by its exit
//!   code run `alone` on the base.
//! * When exclusion is precise — each kept test unique in the base list (across every qualifier
//!   unless `alone` and `deselect` name it), at most once in the head's, under the same qualifier,
//!   and the head's own list with them deselected listing exactly the rest — the head runs once
//!   more with them `deselect`ed. Exit 0 is `pass_excluded` with the names listed; non-zero is a
//!   `new_failure`. Anything else, a check with no usable guide included, is
//!   `base_red_unseparated`: escalated with both logs, the operator's floor override the path
//!   through.
//! * A deciding run that fails on an oversubscribed host (1-min load above the CPU count) is
//!   re-run ONCE with the same argv after waiting, bounded, for headroom (Bazel's
//!   `flaky_test_attempts=2`): exit 0 is `passed on retry (flaky)`, a second failure is
//!   `flaky_twice` and denies. Both attempts are on the record.
//!
//! Head failure ids are still read off the output to ANNOTATE the report (`failureIds`,
//! `regressions`); an annotation never changes an outcome. The install and the formatter are never
//! compared with the base (a formatting diff is deterministic, core#551). `baseline_diff: false`
//! in the repo config opts out, and a base that cannot be run leaves the check denying
//! (fail-closed). A creator transcript that CLAIMS a failure is pre-existing is annotated against
//! the decision ([`ClaimCheck`]: `claim_rejected` when the base is green or the failure outlives
//! the exclusion).
//!
//! ## Per-repo configuration: `.wicked/checks.json`
//!
//! Optional, fail-closed on a malformed file (an unknown key or a bad value is a detection error,
//! never a silent default): `typecheck` / `lint` / `test` / `test_targeted` (a command as an argv
//! array or a whitespace-split string — no shell; `false` disables the auto-detected check),
//! `e2e` (an end-to-end suite, run at the VERIFY stage only, after the test set — the creator
//! floor never pays for it; the deliver re-verify is a verify-stage floor and runs it too;
//! nothing is auto-detected, so it runs only where declared),
//! `timeout_s` (the per-check base bound, replacing the 20-minute default), `full` (run the FULL
//! `test` at verify even when `test_targeted` exists), `baseline_diff` (default `true`) and
//! `generated` (core#1173: `[{"paths": [...], "command": [...]}]`, the files the repository
//! generates and the command that regenerates each set; the floor runs none of them, and the
//! deliver lift regenerates a declared file that conflicts beside a changelog, see
//! [`crate::deliver_lift`]). The
//! floor PREFERS `test_targeted` — at the creator stage always, at verify unless `full: true` —
//! substituting `{files}` (the paths the change touched relative to the base commit, one argv
//! element each) and `{base}` (the base commit id) so a runner's own change-aware mode can be used
//! (`vitest run --changed {base}`, `jest --changedSince {base}`, `cargo test -p <crate>`).
//! A repo that configures NO test command gets the AFFECTED set (core#482, DES-FLOOR-V2 §5,
//! [`derive_targeted`]): `cargo test -p a -p b …` for the packages owning a touched file, or
//! `vitest related --run <files>` for a plain `vitest` test script. A repo whose base carries a
//! build guide runs the guide's checks instead, narrowed only by their `affected` piece. At the creator floor only: the verify floor runs
//! the full suite, as does the PR's CI on every OS.
//!
//! ## Load-aware bound
//!
//! Every check's bound is `base × factor`, `factor = clamp(load1 / ncpu, 1, 3)` — the host's 1-min
//! load average over its logical CPUs, never below 1 (an idle host keeps the base bound) and capped
//! at ×3 (a wedged host must not hold a unit for hours). The effective bound rides each check
//! (`bound_s`, `bound_note`) and the observed duration is logged against it.
//!
//! ## Containment: the checks are repo-controlled code
//!
//! A `test` script is arbitrary code the repository chose. It runs the way the engine runs an
//! ungoverned worker: inside the OS write boundary the worker sandbox provides
//! ([`crate::validator::detect_floor_sandbox`] — macOS `sandbox-exec`, Linux `bwrap`; writes
//! confined to the worktree, the curated secret directories unreadable, network open because
//! installs need it), with an ISOLATED `HOME` and package caches in the floor's CHECK SCRATCH
//! (`HOME`, `npm_config_cache`, `CARGO_HOME`, `CARGO_TARGET_DIR`, `XDG_*` — see "Where the check
//! scratch lives" below), so no check reads the operator's `~/.npmrc`, `~/.cargo/config.toml` or
//! credentials, nothing it writes lands outside the boundary's roots, and its build artifacts
//! (`target/`, a generated `package-lock.json` — `--no-package-lock` when the repo ships none)
//! never land in the reviewed tree, where the worktree guard would deny them. `RUSTUP_HOME` is
//! preserved so the toolchain proxies still resolve. When NO write boundary can be armed — no `sandbox-exec`/`bwrap` on the
//! host (all of Windows), or the worktree root failing to canonicalize — the floor does NOT run
//! the checks: repo-controlled scripts never execute unsandboxed (codex review on #414). The
//! report FAILS with the probe's reason (`sandbox_error`, `sandbox_level: "best-effort"`), which
//! the gate turns into a denial an operator can read and act on (install the sandbox tool, or run
//! the verify phase where one exists).
//!
//! ## Platform matrix (core#416)
//!
//! The boundary is the floor's precondition, so the host decides whether the verify gate can pass
//! at all. Written down here because a Windows operator used to learn it only from a gate denial,
//! after the councils and the creator phases had already run and spent:
//!
//! | Host | Boundary | The floor |
//! |---|---|---|
//! | **macOS** (primary) | `sandbox-exec`, in the base system | runs the checks; `sandbox_level: "sandboxed"` |
//! | **Linux** (supported) | `bwrap` (`bubblewrap`), install it | runs the checks; without `bwrap`, denies |
//! | **Windows** | none the engine can arm | **DENIES every `repo_checks_floor` phase**, unless opted in below |
//!
//! Windows is not supported for governed runs that carry a floor phase — `bug/verify`,
//! `feature/test`, `migration/verify` and crew's served mirrors. There is no AppContainer /
//! restricted-token boundary in the engine and none is designed (core#416 item 3, deferred); what
//! exists instead is the disclosed opt-in, so an operator can choose:
//!
//! - `WICKED_REPO_CHECKS_UNSANDBOXED=1` ([`UNSANDBOXED_OPT_IN_ENV`]) runs the repository's own
//!   check scripts with NO OS write boundary. The results are real; the pass is NOT containment
//!   evidence, and every report, `floorNote`, `FloorEnv` and `repoChecksEvaluated` event says so
//!   (`sandbox_level: "none"`). It is per host, never inferred, and never a default.
//! - Otherwise the floor keeps denying, and the denial now names both remedies.
//!
//! A floor that ran here also says nothing about the platforms a repository's own CI gates: the
//! checks ran on the daemon's OS alone.
//!
//! Two leaves are special. `TMPDIR` (with `TMP`/`TEMP`) is NOT a scratch leaf: it is a short
//! random per-floor directory (`<system temp>/wc-<6 hex>`, mode 0700), armed as the boundary's
//! second write root and reaped with the floor — a `TMPDIR` under a deep worktree overflowed the
//! Unix-socket `sun_path` limit (104 bytes on macOS) and manufactured `listen EINVAL` failures the
//! code did not have (core#489). `CARGO_TARGET_DIR` is split `cargo-target/head` vs
//! `cargo-target/base`, so cargo's freshness check can never hand a head check the base run's test
//! binaries through one shared target dir (core#480).
//!
//! ## Where the check scratch lives
//!
//! OUTSIDE the tree the floor checks, in the engine's own cache area, grouped per repository:
//! `<worker home>/repo-checks/<repo key>/<run key>/` ([`scratch_dir_for`]). The repo key names
//! the directory the run worktrees sit in (`<repo>/wicked-worktrees`), the run key the worktree
//! itself. The scratch used to be `<worktree>/tmp/wicked-checks/`, and a run keeps its worktree
//! after it ends (a parked run keeps it for a retry): a cargo repo left about 16 GB of build
//! output in each run's worktree, and two runs filled the daemon's disk. Now:
//!
//! - **One leaf per run, not one per repository.** A target dir SHARED between runs is not safe:
//!   cargo decides freshness by mtime, and two worktrees of one repo build into the same artifact
//!   names, so a run whose files are older than another run's last build is handed that run's
//!   binaries — a false pass (reproduced with two crates and one `CARGO_TARGET_DIR`). It is the
//!   seam core#480 closed for head vs base. A shared dir would also let one run's
//!   repo-controlled build script plant artifacts that another run's floor executes. Inside a run
//!   every floor reuses the leaf, warm, as before; a guarded verify checkout
//!   ([`checkout_guarded_tree`]) is a tree of its own, and its leaf is removed with it.
//! - **A per-repository cap and a total cap.** Before a floor runs, the whole check cache is
//!   measured ([`enforce_check_cache_cap`]); over [`CHECK_CACHE_CAP_ENV`] GB for one repository
//!   (default [`DEFAULT_CHECK_CACHE_CAP_GB`]) or [`CHECK_CACHE_TOTAL_CAP_ENV`] GB for every
//!   repository together (default [`DEFAULT_CHECK_CACHE_TOTAL_CAP_GB`], core#1122: one core run's
//!   head + base targets reached 17.7 GB, and a few runs filled the host disk) the least recently
//!   used leaves are deleted until both fit, the floor's own last, and each removal is logged. A
//!   leaf a floor is using is never removed: every floor holds a SHARED lock on `<run key>.lock`
//!   for its life, and the cap removes a leaf only under an EXCLUSIVE one. Concurrent floors of
//!   ONE run share the leaf as they shared the worktree scratch, and cargo's own build-directory
//!   lock serializes their builds.
//! - **A free-disk floor (core#1138).** The total cap is the smaller of the configured one and
//!   what keeps [`CHECK_CACHE_FREE_FLOOR_PCT_ENV`] percent of the cache's volume (default
//!   [`DEFAULT_CHECK_CACHE_FREE_FLOOR_PCT`]) or [`CHECK_CACHE_FREE_FLOOR_GB_ENV`] GB (default
//!   [`DEFAULT_CHECK_CACHE_FREE_FLOOR_GB`]) free, whichever is larger: the rig's cache reached
//!   25 GB, inside its 40 GB cap, on a volume 95% full. Below the floor the same pass takes the
//!   least recently used leaves no floor holds until the floor is back, and logs (and
//!   [`floor_cache_usage`] reports) what it is still short when only held leaves are left.
//! - **An idle retention.** A leaf no floor has used for [`CHECK_CACHE_IDLE_TTL`] (the floor log
//!   store's TTL) is deleted by the same pass, whatever the caps: the leaf of a run whose terminal
//!   reap never came, or of a run parked for a week (its next floor rebuilds cold).
//! - **Reaped when the run ends.** [`remove_check_scratch`] runs wherever the engine removes,
//!   reaps or cleans a terminal run's worktree (`crate::repo`), so the leaf goes even when the
//!   tree is kept. Its 0-byte `<run key>.lock` is kept, so a floor already waiting on it never
//!   holds a lease the cap cannot see. No `tmp/wicked-checks` is created in the worktree; the
//!   one thing the floor still writes there is the run base's transient SOURCE export
//!   (`tmp/` [`BASE_EXPORT_SUBDIR`], see [`BaseTree`]), removed as soon as its result is cached.
//!
//! ## Minimal environment
//!
//! A check process does NOT inherit the daemon's environment (adversarial review on #414: a
//! repo-controlled test script with the network open could otherwise read the daemon's tokens).
//! The environment is CLEARED and only an allow-list is passed: `PATH`, locale (`LANG`, `LC_*`),
//! `TERM`, `USER`/`LOGNAME`, the Windows shell essentials, the toolchain's `RUSTUP_HOME`, and the
//! isolation overrides above (`HOME`, `TMPDIR`, `XDG_*`, `npm_config_*`, `CARGO_HOME`,
//! `CARGO_TARGET_DIR`, `CI=1`, `NO_COLOR`). No `GH_TOKEN`, no API key, no `WICKED_*` variable
//! reaches a check — the same floor the validator sandbox applies (`validator::apply_minimal_env`).
//!
//! ## Detection is fail-closed
//!
//! A `package.json` that cannot be read or parsed, or a manifest/lockfile/`node_modules` that is a
//! SYMLINK, fails the floor with the reason: an unverifiable manifest is not "no checks", it is
//! "cannot tell what the checks are". Probes never follow links and leave no lstat-then-open
//! window: each entry is `lstat`ed, then opened `O_NOFOLLOW` and the opened descriptor `fstat`ed
//! (a regular file, same device + inode as the lstat) before a single byte is read (codex review
//! on #414). On a host without `O_NOFOLLOW` (Windows) the probe is `lstat` + open + re-`lstat`,
//! documented as the weaker of the two. Only a repository with NO
//! manifest at all yields a report with no runs and `passed: true` — disclosed as such on the
//! event (`checks: []`), never silently.
//!
//! ## Why the engine runs them rather than trusting the seat
//!
//! The seat may well have run the same commands — the acceptance transcript says it did. The
//! point is WHO the record belongs to: an exit code the engine observed is evidence; a sentence
//! in a transcript is a claim. Running them again costs minutes of machine time per verify phase
//! and buys the one property the gate exists for.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use wicked_apps_core::spawn::HardenedCommand;

use crate::floor_guide::{GuideCheck, GuideStatus, TestId};
use crate::validator::WorkerSandbox;

/// The criterion this floor asserts — phrased as the property, because it is what an operator
/// reads on `GateEvaluated.criterion` and in a denial.
pub const CRITERION: &str = "the repository's own checks pass in the run's worktree (every \
                             detected check exits 0 — done is re-derived by running them, never \
                             asserted)";

/// Per-check wall-clock BASE bound (before the host-load factor). A real suite can take minutes; a
/// check still running past its effective bound is killed with its process tree and recorded as
/// `timed_out` — a distinct classification from `failed` (core#469). A repo overrides it with
/// `timeout_s` in [`CONFIG_PATH`].
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Base bound for the dependency install step, when one is needed.
pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// The most a repo may set `timeout_s` to (4 h) — a bound is a bound.
pub const MAX_CONFIG_TIMEOUT_S: u64 = 4 * 60 * 60;
/// The host-load factor's cap: `factor = clamp(load1 / ncpu, 1, LOAD_FACTOR_CAP)`.
pub const LOAD_FACTOR_CAP: f64 = 3.0;
/// How much of each stream's TAIL is kept as evidence.
pub const TAIL_BYTES: usize = 4096;
/// The most failure identifiers the streaming scanner keeps per check.
pub const MAX_FAILURE_IDS: usize = 1000;
/// Where the checks' isolated `HOME`, caches and build output live: `<worker home>/repo-checks/`,
/// the engine's own cache area — never inside the tree a floor checks (see "Where the check
/// scratch lives" in the module docs). The OS boundary arms the floor's leaf as a write root.
pub const CHECK_CACHE_SUBDIR: &str = "repo-checks";
/// The per-repository size cap on the check scratch, in GB (10^9 bytes). Over it, the least
/// recently used run leaves of that repository are deleted before the next floor runs. A value
/// that does not parse as a whole number keeps the default.
pub const CHECK_CACHE_CAP_ENV: &str = "WICKED_REPO_CHECKS_CACHE_CAP_GB";
/// The default of [`CHECK_CACHE_CAP_ENV`]: one large cargo workspace's head + base targets.
pub const DEFAULT_CHECK_CACHE_CAP_GB: u64 = 20;
/// (core#1122) The TOTAL size cap on the whole check cache (`<worker home>/repo-checks`, every
/// repository's group), in GB (10^9 bytes). Over it, the least recently used run leaves of ANY
/// repository are deleted before the next floor runs, never one a floor holds. A value that does
/// not parse as a whole number keeps the default.
pub const CHECK_CACHE_TOTAL_CAP_ENV: &str = "WICKED_REPO_CHECKS_TOTAL_CAP_GB";
/// The default of [`CHECK_CACHE_TOTAL_CAP_ENV`]: two runs of one large cargo workspace (one
/// wicked-core run's head + base targets measured 17.7 GB on the rig, core#1122).
pub const DEFAULT_CHECK_CACHE_TOTAL_CAP_GB: u64 = 40;
/// (core#1138) The free-disk floor on the check cache's volume, as a whole percentage of the
/// volume's size. The cap pass holds the cache to the smaller of [`CHECK_CACHE_TOTAL_CAP_ENV`] and
/// what keeps the larger of this share and [`CHECK_CACHE_FREE_FLOOR_GB_ENV`] free; below it, the
/// least recently used leaves no floor holds go until the floor is back. `0` (with a `0` GB floor)
/// turns the floor off; a value that does not parse as a whole number keeps the default, and a
/// value over 100 is read as 100.
pub const CHECK_CACHE_FREE_FLOOR_PCT_ENV: &str = "WICKED_REPO_CHECKS_FREE_FLOOR_PCT";
/// The default of [`CHECK_CACHE_FREE_FLOOR_PCT_ENV`].
pub const DEFAULT_CHECK_CACHE_FREE_FLOOR_PCT: u64 = 15;
/// (core#1138) The free-disk floor on the check cache's volume in GB (10^9 bytes), the floor's
/// lower bound whatever the volume's size ([`CHECK_CACHE_FREE_FLOOR_PCT_ENV`]). A value that does
/// not parse as a whole number keeps the default.
pub const CHECK_CACHE_FREE_FLOOR_GB_ENV: &str = "WICKED_REPO_CHECKS_FREE_FLOOR_GB";
/// The default of [`CHECK_CACHE_FREE_FLOOR_GB_ENV`]: crew's disk row stops every job below 5 GB
/// free, and one large cargo workspace's floor builds several GB (core#1122).
pub const DEFAULT_CHECK_CACHE_FREE_FLOOR_GB: u64 = 20;
/// (core#1122) The check cache's own retention: a run leaf no floor has used for this long is
/// deleted at the next cap pass, whatever the caps — a leaf whose run's terminal reap never came
/// (a crash, a run with no registered repository) or a run parked for a week. A floor holding the
/// leaf keeps it; a later floor of that run rebuilds cold. The floor log store's TTL, the same
/// retention ([`crate::floor_log::FLOOR_LOG_TTL`]).
pub const CHECK_CACHE_IDLE_TTL: Duration = crate::floor_log::FLOOR_LOG_TTL;
/// The optional per-repo check configuration, relative to the worktree root.
pub const CONFIG_PATH: &str = ".wicked/checks.json";
/// (core#767) The check-name prefix of a configured journey (`journey-<name>`).
pub const JOURNEY_PREFIX: &str = "journey-";

/// Check classifications on the wire (`CheckRun::classification`).
/// The head failure is absent on the base: the change broke it — denies.
pub const REGRESSION: &str = "regression";
/// Every head failure also fails on the base (a subset, or the identical set): not this change's
/// doing — never denies. A FACT: the floor saw the base fail the same ids.
pub const PRE_EXISTING_IN_SANDBOX: &str = "pre_existing_in_sandbox";
/// Retired (core#481): equal base and head failure sets used to be called a floor-environment
/// mismatch, an inference the comparison cannot establish (any genuinely red base yields the same
/// signal). The floor no longer produces it. Kept so a stored record still reads as non-denying.
pub const FLOOR_ENV_MISMATCH: &str = "floor_env_mismatch";
/// Retired (DES-FLOOR-V2): core#546's nested-sandbox env class read the head's captured output.
/// The floor no longer produces it; kept so a stored record still reads as non-denying.
pub const ENV_CANNOT_RUN: &str = "env_cannot_run";
/// (DES-FLOOR-V2) The deciding run failed on an OVERSUBSCRIBED host (1-min load above the CPU
/// count) and passed its identical retry: `passed on retry (flaky)` — never denies, named on the
/// gate's `floorNote`; both attempts are on the record (`reruns`, `decision.logRefs`).
pub const FLAKY_UNDER_LOAD: &str = "flaky_under_load";

/// Claim verdicts on the wire (`ClaimCheck::verdict`).
pub const CLAIM_REJECTED: &str = "claim_rejected";
pub const CLAIM_CONFIRMED: &str = "claim_confirmed";
pub const CLAIM_UNVERIFIED: &str = "unverified";

/// Denial sources the fold uses for this floor.
pub const DENIAL_SOURCE: &str = "repo_checks";

/// The reason an operator retry records when it arms a re-run of a unit's floor on an unchanged
/// tree (`RepoChecksReport::recheck`, the `floorRecheck` event's `reason`).
pub const RECHECK_OPERATOR_RETRY: &str = "operator retry";
/// A floor that did not FINISH (a check hit its bound) — never "checks failed".
pub const DENIAL_SOURCE_TIMEOUT: &str = "repo_checks_timeout";

/// (core#469) What an operator chose at the escalation gate of a floor that did not FINISH
/// (`repo_checks_timeout`). Each re-runs the floor on the tree as it stands — the seat does not
/// run again — and the result goes through the ordinary gate fold:
/// * `extend` — every check again under [`EXTEND_FACTOR`]× its bound;
/// * `targeted` — the creator-stage selection: the repo's declared `test_targeted` stands in for
///   the full test set (and `e2e` does not run); with no targeted command the test set is waived;
/// * `accept_partial` — the checks that passed run again and the ones that did not finish (or
///   never ran) are WAIVED for this unit.
///
/// * `floor_fix` (core#782) — a seat distinct from the read-only phase first makes the fix the
///   operator's note asks for, then the ordinary floor runs on the fixed tree. Reached only
///   through an approve with a note at a read-only phase's floor gate, never as its own answer.
///
/// A waived check is disclosed on `repoChecksEvaluated.waived` and in the gate's `floorNote`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FloorRerunMode {
    Extend,
    Targeted,
    AcceptPartial,
    FloorFix,
}

impl FloorRerunMode {
    /// The wire token — also the gate decision's `action`.
    pub fn as_wire(self) -> &'static str {
        match self {
            FloorRerunMode::Extend => "extend",
            FloorRerunMode::Targeted => "targeted",
            FloorRerunMode::AcceptPartial => "accept_partial",
            FloorRerunMode::FloorFix => "floor_fix",
        }
    }
}

/// The bound multiplier of an `extend` re-run.
pub const EXTEND_FACTOR: u64 = 2;

/// (core#469) A floor-only re-dispatch the gate armed on a unit (`WorkUnit::floor_rerun`): the
/// worker thread skips the seat, hands the fold `output` (the reviewed attempt's transcript) as
/// the unit's output, and runs the floor under `mode`. Cleared by the fold that consumes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloorRerun {
    pub mode: FloorRerunMode,
    /// `accept_partial`: the checks to waive (the ones that did not finish or never ran).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waive: Vec<String>,
    /// The seat's output from the attempt the gate reviewed.
    #[serde(default)]
    pub output: String,
    /// `floor_fix` (core#782): the fix a seat makes before the floor runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<FloorFix>,
}

/// (core#782) The fix a `floor_fix` re-run makes before its floor: the operator's note, handed to
/// `seat` — a seat distinct from the read-only phase whose floor failed — which edits the run's
/// worktree and nothing else. Only the floor judges the result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloorFix {
    pub note: String,
    pub seat: String,
}

/// One check the floor detected: a name, the exact argv, and where it was read from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCheck {
    /// `install` | `typecheck` | `lint` | `test` | `test_targeted` | `cargo-fmt-check` | `cargo-test`.
    pub name: String,
    pub argv: Vec<String>,
    /// Provenance an operator can verify: `package.json scripts.test`, `Cargo.toml`,
    /// `.wicked/checks.json test_targeted`, …
    pub source: String,
    /// The repo-configured BASE bound (`timeout_s` in [`CONFIG_PATH`]); `None` ⇒ the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
    /// (core#817) The package directory the check runs in, relative to the worktree root (`/`
    /// separated); empty = the root. A change confined to a subdirectory package that the root's
    /// checks do not cover runs THAT package's checks ([`subdir_packages`]).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dir: String,
}

impl RepoCheck {
    fn base_timeout(&self) -> Duration {
        match self.timeout_s {
            Some(s) if self.name != "install" => Duration::from_secs(s),
            _ if self.name == "install" => INSTALL_TIMEOUT,
            _ => CHECK_TIMEOUT,
        }
    }
}

/// Which floor is running. Decides the test set (targeted first at the creator; the full suite at
/// verify only when the repo says `full: true`) and whether a transcript's "pre-existing" claim is
/// judged (creator only — the evaluator's transcript makes no claim about its own change).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FloorStage {
    /// The def's `executes_code` Creator phase, or a prose-planned unit — the change's author.
    Creator,
    /// The def's `verified_evidence` phase, and the deliver lift's re-verify. The historical floor.
    #[default]
    Verify,
}

impl FloorStage {
    pub fn as_wire(self) -> &'static str {
        match self {
            FloorStage::Creator => "creator",
            FloorStage::Verify => "verify",
        }
    }
}

/// What the floor knows about the run when it runs — all optional, all degrading to the historical
/// behaviour (no base ⇒ no baseline diff, no `{files}`/`{base}` substitution, no claim judgement).
#[derive(Debug, Clone, Default)]
pub struct FloorContext {
    pub stage: FloorStage,
    /// Force the dependency install step even when `node_modules/` is provisioned (F-433-003).
    pub force_install: bool,
    /// The commit the unit's change is measured against: the dispatch baseline's `HEAD`
    /// ([`crate::worktree_guard::WorktreeSnapshot::head`]) — the run base for a first attempt.
    pub base_head: Option<String>,
    /// The PINNED git dir the baseline was taken through — never the worktree's own `.git` file.
    pub git_dir: Option<PathBuf>,
    /// The seat's transcript, scanned at the creator stage for a "pre-existing failure" claim.
    pub claim_text: Option<String>,
    /// (core#817) Set when detecting a SUBDIRECTORY package: the worktree handed to detection
    /// is `<root>/<package_dir>`, and the change's touched / deleted files are read from the
    /// root through the pinned git dir and made package-relative — so `{files}`, `{base}` and a
    /// derived targeted command work in the package as at the root.
    pub package_dir: Option<String>,
    /// (core#1050) The run this floor belongs to. Its check scratch is keyed by the run AND the
    /// tree, so two runs whose worktree paths collide (two ids that sanitize to one worktree
    /// name) never share a leaf, and never each other's build output. `None` (a floor built
    /// without a run: tests) keys by the tree alone.
    pub run_id: Option<String>,
}

/// The evidence of one check having run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    pub name: String,
    pub argv: Vec<String>,
    pub source: String,
    /// The process exit code; `None` when it produced none (timed out / could not spawn / killed
    /// by a signal).
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// The OS error when the command could not be started (binary not on PATH, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_error: Option<String>,
    pub duration_ms: u64,
    pub stdout_tail: String,
    pub stderr_tail: String,
    /// The EFFECTIVE wall-clock bound this check ran under, in seconds (base × host-load factor).
    #[serde(default)]
    pub bound_s: u64,
    /// How the bound was derived, for the operator: `1200s × 2.40 (1-min load 33.6 / 14 cpus)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_note: Option<String>,
    /// Failure identifiers the runner printed (`test a::b ... FAILED`, ` FAIL file > name`,
    /// `path: error TS1234: …`, …) — an ANNOTATION for the reader (DES-FLOOR-V2): no outcome
    /// reads them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_ids: Vec<String>,
    /// (core#1172) Failure ids the check's OWN retry cleared: the output carried a retry block
    /// ([`RETRY_BLOCK_MARKER`]) and these failed only before it, so they are flaky (passed on
    /// retry), never in [`Self::failure_ids`] nor the change-only list. An annotation like
    /// `failure_ids`: no outcome reads them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flaky_ids: Vec<String>,
    /// The compatible token of [`Self::decision`] for readers of the older field: `regression`
    /// (`new_failure` against a base, `flaky_twice`), `pre_existing_in_sandbox` (`pass_excluded`),
    /// `flaky_under_load` (passed on its identical retry), [`BASE_RED_UNSEPARATED`]; `None` when it
    /// passed first time, did not finish, could not run, or no base was compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<String>,
    /// The base's failing tests the head's excluded run deselected (`decision.excluded`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_existing: Vec<String>,
    /// The head's annotated failure ids that were not excluded (annotation only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regressions: Vec<String>,
    /// The same check run on the run base, when the floor ran it (see [`BaseRun`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<Box<BaseRun>>,
    /// DEPRECATED (DES-FLOOR-V2): the nested-sandbox env class read head text and is gone. Kept so
    /// a stored record and the wire's `envCannotRun` read as before; always empty now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_cannot_run: Vec<String>,
    /// The further runs the decision made, in order: the excluded run (the base's failing tests
    /// deselected), then the identical retry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reruns: Vec<CheckRun>,
    /// (core#781) The host's load band as the check started: `floor(1-min load / cpus)` — 0 has
    /// headroom, N is N× oversubscribed. `None` where the load is unknown. A head failure compared
    /// against a CACHED base from another band says so ([`cached_base_load_note`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_band: Option<u32>,
    /// (DES-FLOOR-V2) The floor's decision on this check; `None` on a nested run (a re-run, a base
    /// run) and on a stored record from before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<CheckDecision>,
    /// This process's two streams in the run's log store ([`crate::floor_log`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<crate::floor_log::LogRef>,
}

impl CheckRun {
    /// Exit 0, within its bound, and it could be started.
    pub fn passed(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && self.spawn_error.is_none()
    }

    /// The check's outcome on the wire: `passed` | `failed` | `timed_out` | `could_not_run`.
    pub fn outcome(&self) -> &'static str {
        if self.spawn_error.is_some() {
            "could_not_run"
        } else if self.timed_out {
            "timed_out"
        } else if self.passed() {
            "passed"
        } else {
            "failed"
        }
    }

    /// Does this check deny the unit? Its decision says: only `pass` and `pass_excluded` do not.
    /// A stored record without one reads as before (a shared, env-class or flaky failure did not
    /// deny).
    pub fn denies(&self) -> bool {
        if let Some(o) = outcome_of(self) {
            return !matches!(o, OUTCOME_PASS | OUTCOME_PASS_EXCLUDED);
        }
        !self.passed()
            && !matches!(
                self.classification.as_deref(),
                Some(PRE_EXISTING_IN_SANDBOX)
                    | Some(FLOOR_ENV_MISMATCH)
                    | Some(ENV_CANNOT_RUN)
                    | Some(FLAKY_UNDER_LOAD)
            )
    }

    /// Did a run the decision needed hit its bound (this one, or the excluded run / retry)?
    pub fn did_not_finish(&self) -> bool {
        self.timed_out || outcome_of(self) == Some(OUTCOME_TIMED_OUT)
    }

    /// One line an operator can read: `test: exit 1 (154.9s)`.
    pub fn summary(&self) -> String {
        let secs = self.duration_ms as f64 / 1000.0;
        if let Some(e) = &self.spawn_error {
            return format!("{}: could not run ({e})", self.name);
        }
        if self.timed_out {
            return format!(
                "{}: TIMED OUT after {secs:.1}s (killed at the {}s bound{})",
                self.name,
                self.bound_s,
                self.bound_note
                    .as_deref()
                    .map(|n| format!(" = {n}"))
                    .unwrap_or_default()
            );
        }
        // `[<classification>: <outcome or reason>]` — the compatible token first, so a reader of
        // the older words (`[regression]`) still finds them.
        let cls = match (&self.decision, self.classification.as_deref()) {
            (Some(d), c) if !self.passed() => {
                let token = c.unwrap_or(d.outcome.as_str());
                match (&d.reason, token == d.outcome) {
                    (Some(r), _) => format!(" [{token}: {r}]"),
                    (None, true) => format!(" [{token}]"),
                    (None, false) => format!(" [{token}: {}]", d.outcome),
                }
            }
            (None, Some(c)) if !self.passed() => format!(" [{c}]"),
            _ => String::new(),
        };
        match self.exit_code {
            Some(c) => format!("{}: exit {c} ({secs:.1}s){cls}", self.name),
            None => format!("{}: no exit status ({secs:.1}s){cls}", self.name),
        }
    }
}

/// The same check, run on the run BASE in the same sandbox — the other half of the baseline diff.
/// Cached under the checks' scratch by base sha + check name, so a run pays for it once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseRun {
    /// The base commit the check ran against.
    pub head: String,
    /// True when the result was read back from this run's cache rather than run again.
    pub cached: bool,
    /// The base run itself; `None` when the base could not be run (see `error`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<CheckRun>,
    /// Why the base could not be run or compared: the export failed, the base declares no such
    /// check, its install failed, the repo opted out — the head check then denies fail-closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A creator transcript's "pre-existing failure" claim, judged against the baseline diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimCheck {
    /// The phrase that matched, verbatim from the transcript line.
    pub phrase: String,
    /// The failing check the claim was judged against (empty when the floor was green).
    pub check: String,
    /// `claim_rejected` (the base is green — the failure is this change's), `claim_confirmed` (the
    /// base fails it too), `unverified` (no base comparison was possible).
    pub verdict: String,
}

/// The environment the checks ran under — recorded on the verdict so a `floor_env_mismatch` can be
/// read against CI's environment. Values here are the floor's own isolation overrides and the
/// non-secret allow-list; no daemon secret can appear (the environment is cleared first).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloorEnv {
    pub home: String,
    pub tmpdir: String,
    /// `LANG=…`, `LC_*=…` as passed through (absent ⇒ the runner's C locale).
    pub locale: Vec<String>,
    /// The floor's network policy — `open` (installs need the registry; there is no egress fence).
    pub network: String,
    pub sandbox_level: String,
    /// The `PATH` the checks resolved their binaries on.
    pub path: String,
    /// NAMES of the daemon variables passed through (values omitted).
    pub passthrough: Vec<String>,
}

/// Everything the floor observed for one unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoChecksReport {
    /// What was detected, in run order (empty when detection failed — see `detect_error`).
    pub detected: Vec<RepoCheck>,
    /// What actually ran (a prefix of `detected` — the floor stops at the first DENYING failure;
    /// a failure the base shares is recorded and the floor moves on).
    pub checks: Vec<CheckRun>,
    /// Detected checks that never ran because an earlier one failed.
    pub skipped: Vec<String>,
    /// True iff detection succeeded and no check DENIES ([`CheckRun::denies`] — exit 0, or a
    /// failure the base shares); vacuously true when nothing was detected (see the module doc;
    /// the event discloses `checks: []`).
    pub passed: bool,
    /// Why detection itself failed (unreadable/malformed manifest, a symlinked probe) — the floor
    /// FAILS with this reason rather than reporting "no checks".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_error: Option<String>,
    /// The OS write-containment level the checks ran under (`sandboxed` / `best-effort`), the
    /// wire spelling of [`SandboxLevel`].
    #[serde(default)]
    pub sandbox_level: String,
    /// Why the level is below `sandboxed`, when it is — verbatim from the worker-sandbox probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_note: Option<String>,
    /// Set when the checks did NOT run because no OS write boundary could be armed: the floor
    /// fails closed rather than run repo-controlled scripts unsandboxed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_error: Option<String>,
    /// Files the checks themselves created beside a manifest that ships none (`Cargo.lock` from a
    /// `cargo test` without a lockfile) and the engine removed afterwards — its own side effect,
    /// never the seat's, so the worktree guard does not deny it. Disclosed, not hidden.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub engine_writes_removed: Vec<String>,
    /// (core#467) A creator transcript's "pre-existing" claim, judged against the baseline diff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<ClaimCheck>,
    /// (F-RC2-009) The environment the checks ran under, for CI-parity reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<FloorEnv>,
    /// (core#544) The worktree tree id the checks ran ON (taken before they ran). The unit's last
    /// report stands for THIS tree: a rework that leaves the tree identical carries it forward
    /// instead of reading as "nothing to check". `None` when unknown (never carried).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    /// (core#469) An operator's floor re-run this report answers (`extend` | `targeted` |
    /// `accept_partial`); `None` for the ordinary floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerun: Option<FloorRerunMode>,
    /// (core#469) Detected checks the operator WAIVED for this unit (never run, never counted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waived: Vec<String>,
    /// (core#469) The operator's re-run of THIS floor, armed at the escalation gate and read by
    /// the unit's next dispatch (`WorkUnit::repo_checks` is where the gate finds the floor it
    /// answers). Consumed there: the re-run's own report replaces this one on the unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_rerun: Option<FloorRerun>,
    /// An OPERATOR RETRY of this unit's escalation gate ("Retry this step with a note" — a plain
    /// approve of the escalated unit) armed ONE re-run of these checks on the unit's next attempt,
    /// even when that attempt leaves the tree unchanged — the single exception to core#544's
    /// carry-forward, so a flaky result is not sticky per tree. The reason
    /// ([`RECHECK_OPERATOR_RETRY`]). Consumed by the attempt's fold (the fresh report replaces this
    /// one); an automatic rework never arms it, so a deterministic failure is not re-run forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recheck: Option<String>,
    /// (core#432 F-3R2-023) A VERIFY floor whose change touches end-to-end files while no `e2e`
    /// or journey check ran says so here — the gate's `floorNote` then states that the floor did
    /// not run them, instead of a green floor implying it did ([`e2e_coverage_note`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage_note: Option<String>,
    /// (deliver honors an override) What this result is a result FOR ([`FloorKey`]): the tree the
    /// checks ran on, the base they were measured against and the exact check set. Recorded only
    /// when the checks ran on a known tree; `None` otherwise (never stood on by a later floor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<FloorKey>,
    /// (J4) This floor RESUMED one a daemon restart interrupted ([`FloorJournal`]): the checks that
    /// had finished there on the same tree, base and check set were reused, the rest ran — or the
    /// interrupted floor could not be resumed and this one ran from its first check, and says why.
    /// `None` for a floor that resumed nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed: Option<FloorResume>,
    /// (core#1144) The checks this VERIFY floor took from the creator floor that already ran them
    /// on the same guarded tree against the same base ([`CreatorFloorRecord`]), instead of running
    /// them again. `None` when it reused nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reused_from: Option<FloorReuse>,
}

/// (J4) A floor's journal identity: the unit whose floor it is, the attempt running it and the tree
/// its checks run on. A floor given one records each check's result in the run's log store as it
/// finishes (`floor-journal-u<ord>.json`) and removes the record when the floor ends; a record left
/// behind is a floor the process died in. The next floor of that unit on the SAME tree, base and
/// check set ([`FloorKey`]) reuses the finished checks and runs only the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloorJournal {
    pub ord: u32,
    pub attempt: u32,
    pub tree: String,
    /// (core#1144) The unit's phase (`fix`, `verify`, …), named when a later floor reuses this
    /// one's results ([`FloorReuse::note`]). `None` when the caller does not know it.
    pub phase: Option<String>,
}

/// The check that provisions the tree's dependencies: never reused from a journal.
const PROVISIONING_CHECK: &str = "install";

/// (J4) What a floor took from the interrupted one ([`RepoChecksReport::resumed`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FloorResume {
    /// The attempt whose floor was interrupted.
    pub from_attempt: u32,
    /// The checks reused from it (finished there on the same tree, base and check set), in run order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reused: Vec<String>,
    /// Set when nothing could be reused and the floor ran from its first check: why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarted: Option<String>,
}

impl FloorResume {
    /// The operator's sentence for the gate's `floorNote`.
    pub fn note(&self) -> String {
        match &self.restarted {
            Some(why) => format!(
                "the floor of attempt {} was interrupted (the daemon restarted) and could not be \
                 resumed ({why}) — the checks ran again from the first one",
                self.from_attempt
            ),
            None if self.reused.is_empty() => format!(
                "the floor of attempt {} was interrupted (the daemon restarted) before any check \
                 finished — the checks ran from the first one",
                self.from_attempt
            ),
            None => {
                format!(
                "resumed the floor of attempt {} that a daemon restart interrupted: {} finished \
                 there on this same tree, base and check set and {} reused, not run again; the \
                 rest ran",
                self.from_attempt,
                self.reused.join(", "),
                if self.reused.len() == 1 { "was" } else { "were" }
            )
            }
        }
    }
}

/// (core#1144) What a verify floor took from the creator floor of the same tree
/// ([`RepoChecksReport::reused_from`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FloorReuse {
    /// The creator unit's phase (`fix`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// The creator unit and the attempt whose floor ran the reused checks.
    pub ord: u32,
    pub attempt: u32,
    /// The checks reused, in run order.
    pub reused: Vec<String>,
    /// (core#1152) The checks this floor ran itself, in run order: the ones the creator floor did
    /// not run (or could not stand for), wherever they sit in the check order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fresh: Vec<String>,
    /// (core#1152) Set when the reused results did not stand, saying why and what the floor did
    /// instead: a check that ran fresh after them failed (the floor ran every check again from
    /// the first one, nothing reused), or a fresh check changed the checkout (the floor fails).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voided: Option<String>,
}

impl FloorReuse {
    /// The operator's sentence for the gate's `floorNote`.
    pub fn note(&self) -> String {
        let from = format!(
            "checks reused from {} (unit {}, attempt {})",
            self.phase.as_deref().unwrap_or("the creator floor"),
            self.ord,
            self.attempt,
        );
        if let Some(why) = &self.voided {
            return format!("{from} were set aside: {why}");
        }
        format!(
            "{from}: {} ran there on this same tree, against the same base, with the same command \
             and bound, on a clean checkout — not run again here{}",
            self.reused.join(", "),
            if self.fresh.is_empty() {
                String::new()
            } else {
                format!("; ran fresh here: {}", self.fresh.join(", "))
            }
        )
    }
}

/// (core#1144) A GUARDED creator floor's finished checks, kept in the run's log store (outside
/// every sandbox write root, so no check can plant one) for the verify floor of the same tree. One
/// record per tree and base: a later creator floor of that tree replaces it. A verify floor reuses
/// a check from it only when the tree, the base, the check's directory, name, argv and bound, the
/// boundary posture and the exclusion switch all match, and the check finished there with its
/// evidence whole — the same rule a resumed journal follows ([`reusable_run`]).
#[derive(Debug, Serialize, Deserialize)]
struct CreatorFloorRecord {
    floor_policy: u32,
    tree: String,
    base: String,
    ord: u32,
    attempt: u32,
    #[serde(default)]
    phase: Option<String>,
    sandbox_level: String,
    exclude: bool,
    #[serde(default)]
    checks: Vec<CreatorEntry>,
}

/// One finished check of a [`CreatorFloorRecord`]: the detected check (with its configured bound)
/// and its run.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CreatorEntry {
    dir: String,
    name: String,
    argv: Vec<String>,
    #[serde(default)]
    timeout_s: Option<u64>,
    run: CheckRun,
}

/// The creator-floor record of `tree` measured against `base` in the run's log store.
fn creator_record_path(logs: &Path, tree: &str, base: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for part in [tree, base] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    let key: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    logs.join(format!("creator-floor-{key}.json"))
}

/// Write `record` to its place in the run's log store (write-then-rename: whole or absent).
fn write_creator_record(logs: &Path, record: &CreatorFloorRecord) {
    let Ok(json) = serde_json::to_string(record) else {
        return;
    };
    let path = creator_record_path(logs, &record.tree, &record.base);
    let _ = std::fs::create_dir_all(logs);
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Did `run` FINISH as the declared `check` (not the guide's `affected` subset, not a timeout,
/// not a spawn or could-not-run failure) with every log it names still whole in `logs`? Only such
/// a result stands in for running the check again. The `install` check never does: its receipt is
/// not its output (`node_modules`, a venv), and a guarded floor runs on a fresh checkout.
fn reusable_run(check: &RepoCheck, run: &CheckRun, logs: &Path) -> bool {
    check.name != PROVISIONING_CHECK && run.argv == check.argv && finished_whole(run, logs)
}

/// Did `run` finish (not a timeout, not a spawn or could-not-run failure, a decision recorded)
/// with every log it names — its own streams, its decision's, its retries' and its base run's —
/// still whole in `logs` (each blob's age restarts)?
fn finished_whole(run: &CheckRun, logs: &Path) -> bool {
    let finished = !run.did_not_finish()
        && run.spawn_error.is_none()
        && run.decision.is_some()
        && outcome_of(run) != Some(OUTCOME_COULD_NOT_RUN);
    let mut refs: Vec<&crate::floor_log::LogRef> = Vec::new();
    fn collect<'a>(r: &'a CheckRun, out: &mut Vec<&'a crate::floor_log::LogRef>) {
        out.extend(r.logs.iter());
        // The decision's own streams (the base inventory, each failing test's confirmation)
        // justify a `pass_excluded`; a decision whose evidence is gone is not reused (xreview).
        if let Some(d) = &r.decision {
            out.extend(d.log_refs.iter());
        }
        for x in &r.reruns {
            collect(x, out);
        }
        if let Some(run) = r.base.as_ref().and_then(|b| b.run.as_ref()) {
            collect(run, out);
        }
    }
    collect(run, &mut refs);
    finished
        && refs
            .iter()
            .all(|l| crate::floor_log::refresh(logs, l).is_ok())
}

/// The creator floor's checks a verify floor on the same tree and base may take
/// ([`CreatorFloorRecord`]); `None` when there is no record, or one of another engine version,
/// boundary posture or exclusion switch.
struct CreatorSeed {
    record: CreatorFloorRecord,
    reused: Vec<String>,
    /// (core#1152) The checks the verify floor ran itself beside the reused ones.
    fresh: Vec<String>,
}

impl CreatorSeed {
    fn load(
        logs: &Path,
        tree: &str,
        base: &str,
        sandbox_level: &str,
        exclude: bool,
    ) -> Option<Self> {
        let raw = std::fs::read_to_string(creator_record_path(logs, tree, base)).ok()?;
        let record: CreatorFloorRecord = serde_json::from_str(&raw).ok()?;
        (record.floor_policy == FLOOR_POLICY_VERSION
            && record.tree == tree
            && record.base == base
            && record.sandbox_level == sandbox_level
            && record.exclude == exclude)
            .then_some(CreatorSeed {
                record,
                reused: Vec::new(),
                fresh: Vec::new(),
            })
    }

    /// The creator floor's result of `check`, when it ran the identical check (directory, name,
    /// argv and bound) and that result may be reused ([`reusable_run`]).
    fn entry_for(&self, check: &RepoCheck, logs: &Path) -> Option<&CreatorEntry> {
        self.record
            .checks
            .iter()
            .find(|e| {
                e.dir == check.dir
                    && e.name == check.name
                    && e.argv == check.argv
                    && e.timeout_s == check.timeout_s
            })
            .filter(|e| reusable_run(check, &e.run, logs))
    }

    /// Take the creator floor's result of `check` ([`Self::entry_for`]).
    fn reuse(&mut self, check: &RepoCheck, logs: &Path) -> Option<CheckRun> {
        let run = self.entry_for(check, logs)?.run.clone();
        self.reused.push(check.name.clone());
        Some(run)
    }

    /// (core#1152) Why the reuse is set aside when `run` — a check that ran here (or in the pass a
    /// daemon restart interrupted) after reused ones — denied: it may have needed something a
    /// reused check leaves in the checkout. Not a timeout: a missing input fails, it does not
    /// hang, and a second hour-long bound would only repeat it.
    fn set_aside_by(&self, check: &RepoCheck, run: &CheckRun) -> Option<String> {
        (run.denies() && !run.timed_out && !self.reused.is_empty()).then(|| {
            format!(
                "`{}` ran fresh after the reused {} and {} — it may need something a reused \
                 check leaves in the checkout",
                check.name,
                self.reused.join(", "),
                run.outcome()
            )
        })
    }

    /// The floor log's line for one reused check.
    fn log_line(&self, run: &CheckRun) -> String {
        format!(
            "wicked-core: repo checks floor — `{}` {} on this same tree and base at {} (unit {}, \
             attempt {}); checks reused from that floor, not run again",
            run.name,
            run.outcome(),
            self.record.phase.as_deref().unwrap_or("the creator floor"),
            self.record.ord,
            self.record.attempt,
        )
    }

    fn finish(self, voided: Option<String>) -> Option<FloorReuse> {
        (!self.reused.is_empty()).then_some(FloorReuse {
            phase: self.record.phase,
            ord: self.record.ord,
            attempt: self.record.attempt,
            reused: self.reused,
            fresh: self.fresh,
            voided,
        })
    }
}

/// The journal file of `ord`'s floor in the run's log store.
fn journal_path(logs: &Path, ord: u32) -> PathBuf {
    logs.join(format!("floor-journal-u{ord}.json"))
}

#[derive(Debug, Serialize, Deserialize)]
struct JournalRecord {
    floor_policy: u32,
    key: FloorKey,
    exclude: bool,
    attempt: u32,
    #[serde(default)]
    checks: Vec<JournalEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalEntry {
    dir: String,
    name: String,
    argv: Vec<String>,
    run: CheckRun,
}

/// An open floor journal ([`FloorJournal`]): the record this floor writes, and what it may reuse
/// from the interrupted one.
struct OpenJournal {
    path: PathBuf,
    record: JournalRecord,
    prior: Vec<JournalEntry>,
    resume: Option<FloorResume>,
    /// The checks changed the tree the journal is keyed by: nothing more is recorded and the record
    /// is gone, so no later floor reuses a result that ran on content other than that tree.
    spoiled: bool,
}

impl OpenJournal {
    /// Open `ord`'s journal for a floor on `key`: a record left behind by an interrupted floor of
    /// the same key (and policy, and exclusion switch) offers its finished checks; any other record
    /// is discarded and disclosed. The fresh record is written at once.
    fn open(logs: &Path, j: &FloorJournal, key: FloorKey, exclude: bool) -> Self {
        let path = journal_path(logs, j.ord);
        let left: Option<JournalRecord> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        let (prior, resume) = match left {
            None => (Vec::new(), None),
            Some(rec) => {
                let why = if rec.floor_policy != FLOOR_POLICY_VERSION {
                    Some("another engine version ran it".to_string())
                } else if rec.key.tree != key.tree {
                    Some(format!(
                        "it ran on tree {}, this floor runs on {}",
                        &rec.key.tree[..rec.key.tree.len().min(10)],
                        &key.tree[..key.tree.len().min(10)]
                    ))
                } else if rec.key.base != key.base {
                    Some("it was measured against another base".to_string())
                } else if rec.key.checks != key.checks || rec.exclude != exclude {
                    Some("it ran another check set".to_string())
                } else {
                    None
                };
                let resume = FloorResume {
                    from_attempt: rec.attempt,
                    reused: Vec::new(),
                    restarted: why.clone(),
                };
                let prior = if why.is_none() {
                    rec.checks
                } else {
                    Vec::new()
                };
                (prior, Some(resume))
            }
        };
        let open = OpenJournal {
            path,
            record: JournalRecord {
                floor_policy: FLOOR_POLICY_VERSION,
                key,
                exclude,
                attempt: j.attempt,
                checks: Vec::new(),
            },
            prior,
            resume,
            spoiled: false,
        };
        open.persist();
        open
    }

    /// The interrupted floor's FINISHED result of `check`, when it has one whose logs are all still
    /// in the store: a check that timed out, could not start or could not run is run again. The
    /// `install` check always runs again: its receipt is not its output (`node_modules`, a venv),
    /// and a guarded floor runs on a fresh checkout that has none of it (xreview).
    fn reuse(&mut self, check: &RepoCheck, logs: &Path) -> Option<CheckRun> {
        if check.name == PROVISIONING_CHECK {
            return None;
        }
        let at = self
            .prior
            .iter()
            .position(|e| e.dir == check.dir && e.name == check.name && e.argv == check.argv)?;
        let entry = self.prior.remove(at);
        let run = entry.run;
        finished_whole(&run, logs).then(|| {
            if let Some(r) = self.resume.as_mut() {
                r.reused.push(check.name.clone());
            }
            run
        })
    }

    /// Record a finished check (written through at once: the process may die before the next).
    /// `untouched` answers whether the checkout still holds exactly the journal's tree; a check
    /// that changed it spoils the journal (its pass ran on other content: xreview P1).
    fn record(&mut self, check: &RepoCheck, run: &CheckRun, untouched: Option<&dyn Fn() -> bool>) {
        if self.spoiled {
            return;
        }
        if untouched.is_some_and(|f| !f()) {
            self.spoiled = true;
            self.record.checks.clear();
            let _ = std::fs::remove_file(&self.path);
            return;
        }
        self.record.checks.push(JournalEntry {
            dir: check.dir.clone(),
            name: check.name.clone(),
            argv: check.argv.clone(),
            run: run.clone(),
        });
        self.persist();
    }

    fn persist(&self) {
        let Ok(json) = serde_json::to_string(&self.record) else {
            return;
        };
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Write-then-rename: a record is whole or absent, never half-written by a dying process.
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &self.path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// The floor ended (whatever its verdict): its record goes, so no later floor of the unit
    /// stands on it — only an interrupted floor leaves one. Returns what this floor resumed.
    fn finish(self) -> Option<FloorResume> {
        let _ = std::fs::remove_file(&self.path);
        self.resume
    }
}

/// (j4-gates) A failed floor's failing tests against the run base ([`RepoChecksReport::failure_split`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailureSplit {
    /// `<check>: <test>` for every failing test the base does not fail — the change's own.
    pub head_only: Vec<String>,
    /// How many failing tests the base fails too.
    pub base_shared: usize,
    /// `<check>: <test>` for failures with no base to compare against.
    pub unknown: Vec<String>,
    /// Every denying check named its failures, the base ran for each, and the base fails every one.
    pub all_base_red: bool,
    /// The base commit compared against, when one ran.
    pub base: Option<String>,
    /// (core#1172) `<check>: <test>` for failures the check's own retry cleared (flaky, passed on
    /// retry): never head-only, never shared.
    pub flaky: Vec<String>,
}

/// The most test names a floor sentence lists before `(+N more)`.
const SPLIT_NAMES_SHOWN: usize = 12;

impl FailureSplit {
    fn names(ids: &[String]) -> String {
        let mut shown: Vec<String> = ids
            .iter()
            .take(SPLIT_NAMES_SHOWN)
            .map(|i| format!("`{i}`"))
            .collect();
        if ids.len() > SPLIT_NAMES_SHOWN {
            shown.push(format!("(+{} more)", ids.len() - SPLIT_NAMES_SHOWN));
        }
        shown.join(", ")
    }

    fn base_words(&self) -> String {
        match &self.base {
            Some(b) => format!("the run base {}", &b[..b.len().min(10)]),
            None => "the run base".to_string(),
        }
    }

    /// The floor's sentence: which failures are the change's, or that none is.
    pub fn sentence(&self) -> String {
        let base = self.base_words();
        let mut parts: Vec<String> = Vec::new();
        if self.all_base_red {
            parts.push(format!(
                "Every failing test also fails on {base} ({} shared): none of them is this \
                 change's",
                self.base_shared
            ));
        } else if !self.head_only.is_empty() {
            parts.push(format!(
                "HEAD-ONLY failures — they fail on this tree and not on {base}, so they are this \
                 change's: {}{}",
                Self::names(&self.head_only),
                if self.base_shared > 0 {
                    format!(
                        " ({} more fail on the base too and are not)",
                        self.base_shared
                    )
                } else {
                    String::new()
                }
            ));
        } else if self.base_shared > 0 {
            parts.push(format!(
                "No named failing test is head-only ({} fail on {base} too)",
                self.base_shared
            ));
        }
        if !self.unknown.is_empty() {
            parts.push(format!(
                "failing with no base to compare: {}",
                Self::names(&self.unknown)
            ));
        }
        if !self.flaky.is_empty() {
            parts.push(format!(
                "flaky (passed on retry), not counted as this change's: {}",
                Self::names(&self.flaky)
            ));
        }
        parts.join("; ")
    }
}

/// (deliver honors an override) The exact identity of one floor result inside a run: the tree the
/// checks ran on, the base commit the baseline diff measured against, and a digest of the check
/// set (the stage, then every detected check's name, directory, argv and bound, in run order). A
/// later floor stands on a recorded result instead of running the checks again ONLY when all three
/// are identical; anything different runs the checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FloorKey {
    pub tree: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Hex SHA-256 of the check set ([`check_set_digest`]).
    pub checks: String,
}

impl FloorKey {
    /// The key of a floor that ran (or would run) `detected` on `tree` for `ctx`.
    pub(crate) fn of(tree: &str, ctx: &FloorContext, detected: &[RepoCheck]) -> Self {
        FloorKey {
            tree: tree.to_string(),
            base: ctx.base_head.clone(),
            checks: check_set_digest(ctx, detected),
        }
    }
}

/// The digest of a floor's check set: the stage and the package directory, then every check's
/// name, directory, argv and configured bound, in run order — a reordered, added, removed,
/// re-scoped or re-bounded check is a different set. A forced install (`force_install`, after a
/// lockfile drift) acts only through detection — it adds the `install` check — so the set itself
/// carries it: an attempt that forced the install and its retry that finds the same `install`
/// check (no `node_modules/` in a fresh checkout) ran the same checks.
pub(crate) fn check_set_digest(ctx: &FloorContext, detected: &[RepoCheck]) -> String {
    use sha2::Digest;
    let set: Vec<(&str, &str, &[String], Option<u64>)> = detected
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.dir.as_str(),
                c.argv.as_slice(),
                c.timeout_s,
            )
        })
        .collect();
    let material = serde_json::json!({
        "stage": ctx.stage.as_wire(),
        "packageDir": ctx.package_dir,
        "checks": set,
    });
    let digest = sha2::Sha256::digest(material.to_string().as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl RepoChecksReport {
    /// The [`FloorKey`] of this report when its checks RAN on `tree` as the ordinary floor (passed,
    /// failed or timed out) — `None` for a floor that did not run (no boundary, a detection or
    /// checkout refusal) and for an operator's re-run (`extend` / `targeted` / `accept_partial` /
    /// a floor fix) or a floor with waived checks: those ran a different set than the ordinary
    /// floor would, so nothing may stand on them in its place (xreview r1).
    pub(crate) fn key_for(&self, tree: Option<&str>, ctx: &FloorContext) -> Option<FloorKey> {
        let tree = tree.filter(|t| !t.is_empty())?;
        (self.outcome() != "not_run" && self.rerun.is_none() && self.waived.is_empty())
            .then(|| FloorKey::of(tree, ctx, &self.detected))
    }

    /// Did the floor stop on a check that hit its bound? A timed-out floor did not FINISH — it is
    /// not "checks failed" (core#469), and the fold denies it under [`DENIAL_SOURCE_TIMEOUT`].
    pub fn timed_out(&self) -> bool {
        self.checks.iter().any(|c| c.did_not_finish() && c.denies())
    }

    /// The floor's outcome on the wire: `passed` | `failed` | `timed_out` | `not_run` (no OS
    /// boundary, or detection failed — nothing was re-derived).
    pub fn outcome(&self) -> &'static str {
        if self.sandbox_error.is_some() || self.detect_error.is_some() {
            "not_run"
        } else if self.passed {
            "passed"
        } else if self.timed_out() {
            "timed_out"
        } else {
            "failed"
        }
    }

    /// The denial source the fold books this floor under when `!passed`.
    pub fn denial_source(&self) -> &'static str {
        if self.timed_out() {
            DENIAL_SOURCE_TIMEOUT
        } else {
            DENIAL_SOURCE
        }
    }

    /// What the floor's decisions say beyond pass/deny, for the gate's `floorNote`: the opted-in
    /// boundary posture first; then, per check, the base failures it excluded, a pass on the
    /// identical retry, and a base red it could not separate; then the e2e coverage note. `None`
    /// when there is nothing of either kind to say.
    pub fn classification_note(&self) -> Option<String> {
        let mut notes: Vec<String> = Vec::new();
        // (core#416 item 3b) The boundary posture rides the gate's own note, first: an operator
        // approving an opted-in floor must see that repo-controlled code ran uncontained on this
        // host whether the checks passed or failed — a disclosure the operator has to step over,
        // not a line in a log.
        if self.unsandboxed() {
            notes.push(self.sandbox_note.clone().unwrap_or_else(|| {
                format!("{UNSANDBOXED_OPT_IN_ENV}=1 — the checks ran with NO OS write boundary")
            }));
        }
        // (J4) A floor that resumed (or had to restart) one a daemon restart interrupted says so.
        if let Some(r) = &self.resumed {
            notes.push(r.note());
        }
        // (core#1144) A verify floor that reused the creator floor's checks says which, and where.
        if let Some(r) = &self.reused_from {
            notes.push(r.note());
        }
        for c in &self.checks {
            let Some(d) = &c.decision else { continue };
            let said = match d.outcome.as_str() {
                OUTCOME_PASS_EXCLUDED => format!(
                    "passed with {} (the base fails them too; the head passed with them \
                     deselected){}",
                    d.reason
                        .as_deref()
                        .unwrap_or("pre-existing failures excluded"),
                    if d.flaky {
                        " — the excluded run failed on an oversubscribed host first; an \
                         order-dependent failure reads the same way"
                    } else {
                        ""
                    }
                ),
                OUTCOME_PASS if d.flaky => "failed on an oversubscribed host and passed on its \
                                            identical retry (flaky) — not denied; an \
                                            order-dependent failure reads the same way"
                    .to_string(),
                OUTCOME_BASE_RED_UNSEPARATED | OUTCOME_FLAKY_TWICE => {
                    d.reason.clone().unwrap_or_else(|| d.outcome.clone())
                }
                _ => continue,
            };
            notes.push(format!("`{}`: {said}", c.name));
        }
        // (core#432 F-3R2-023) An end-to-end change the floor did not run, on a pass too.
        if let Some(n) = &self.coverage_note {
            notes.push(n.clone());
        }
        (!notes.is_empty()).then(|| notes.join("; "))
    }

    /// The operator-facing denial when `!passed`.
    pub fn denial_reason(&self) -> String {
        if let Some(e) = &self.sandbox_error {
            return format!(
                "repo checks floor failed: {CRITERION}. The checks were NOT run: {e} — the floor \
                 never runs repo-controlled scripts without an OS write boundary (fail-closed)."
            );
        }
        if let Some(e) = &self.detect_error {
            return format!(
                "repo checks floor failed: {CRITERION}. The repository's checks could not be \
                 determined: {e} (fail-closed — an unverifiable manifest is not \"no checks\")."
            );
        }
        let failed: Vec<String> = self
            .checks
            .iter()
            .filter(|c| c.denies())
            .map(|c| {
                // Both streams: a test runner puts the failing assertion on stdout and the
                // "error: test failed" line on stderr, and an operator needs to see either.
                let out = last_lines(&c.stdout_tail);
                let err = last_lines(&c.stderr_tail);
                let line = match (out.is_empty(), err.is_empty()) {
                    (true, true) => c.summary(),
                    (false, true) => format!("{} — stdout tail: {out}", c.summary()),
                    (true, false) => format!("{} — stderr tail: {err}", c.summary()),
                    (false, false) => {
                        format!("{} — stdout tail: {out} — stderr tail: {err}", c.summary())
                    }
                };
                // (core#469) A TIMEOUT is a check that did not finish, not one that failed: say
                // so, and say what the gate can do about it.
                if c.did_not_finish() {
                    return format!(
                        "{line}. This check did not FINISH — the change is unverified by it, not \
                         refuted: extend the bound (`timeout_s` in `{CONFIG_PATH}`), declare a \
                         targeted test command (`test_targeted`), or accept typecheck + lint + \
                         targeted as this unit's floor at the gate"
                    );
                }
                // (DES-FLOOR-V2) What the decision established, from exit codes only.
                let short = |h: &str| h[..h.len().min(10)].to_string();
                let failed = |d: &CheckDecision| {
                    matches!(
                        d.outcome.as_str(),
                        OUTCOME_NEW_FAILURE | OUTCOME_FLAKY_TWICE
                    )
                };
                let base_note = match (&c.decision, c.base.as_deref()) {
                    (_, Some(BaseRun { error: Some(e), .. })) => {
                        format!(" — the base could not be compared ({e}); fail-closed")
                    }
                    (Some(d), Some(b)) if d.outcome == OUTCOME_BASE_RED_UNSEPARATED => format!(
                        " — the run base {} fails this check too, and its failures cannot be told \
                         apart from the head's ({}): escalated with both logs — the operator's \
                         floor override is the path through",
                        short(&b.head),
                        d.reason.as_deref().unwrap_or("base red, cannot separate")
                    ),
                    (Some(d), Some(b)) if !d.excluded.is_empty() && failed(d) => format!(
                        " — NEW FAILURE: the head still fails with the base's {} failing test{} \
                         ({}) deselected{}",
                        d.excluded.len(),
                        if d.excluded.len() == 1 { "" } else { "s" },
                        d.excluded.join(", "),
                        if b.head.is_empty() {
                            String::new()
                        } else {
                            format!(" (base {})", short(&b.head))
                        }
                    ),
                    (Some(d), Some(b))
                        if failed(d) && b.run.as_ref().is_some_and(CheckRun::passed) =>
                    {
                        format!(
                            " — NEW FAILURE: the run base {} passes this check{}",
                            short(&b.head),
                            if c.regressions.is_empty() {
                                String::new()
                            } else {
                                format!(" (the head printed: {})", c.regressions.join(", "))
                            }
                        )
                    }
                    _ => String::new(),
                };
                let base_note = match &c.decision {
                    Some(d) if d.outcome == OUTCOME_FLAKY_TWICE => format!(
                        "{base_note} — it failed on an oversubscribed host and again on its \
                         identical retry (flaky_twice)"
                    ),
                    _ => base_note,
                };
                let line = format!("{line}{base_note}{}", cached_base_load_note(c));
                // A failed INSTALL is an environmental finding about provisioning the worktree,
                // not a verdict on the work (F-E2E-029): say so, and say what was being provisioned
                // (`source` = the lockfile and why the install ran), so the operator reads
                // "dependencies could not be installed", never a bare ENOENT.
                if c.name == "install" {
                    format!(
                        "dependency provisioning failed — the worktree's dependencies could not \
                         be installed from {} ({line}); the repository's checks were not run \
                         against an installed tree. This is an environment finding, not a \
                         verdict on the change: fix the install (registry access, lockfile) and \
                         retry the phase",
                        c.source
                    )
                } else {
                    line
                }
            })
            .collect();
        // (core#469) A floor that did not FINISH is worded as such — never "checks failed".
        let head = if self.timed_out() {
            format!(
                "repo checks floor did not FINISH — {CRITERION} was not re-derived (a check hit \
                 its bound): "
            )
        } else {
            format!("repo checks floor failed: {CRITERION}. ")
        };
        let mut s = format!("{head}{}", failed.join("; "));
        if !self.skipped.is_empty() {
            s.push_str(&format!(
                " (not run after the failure: {})",
                self.skipped.join(", ")
            ));
        }
        // Failures the decision cleared (excluded, or passed on retry) are on the record.
        let tolerated: Vec<String> = self
            .checks
            .iter()
            .filter(|c| !c.passed() && !c.denies())
            .map(CheckRun::summary)
            .collect();
        if !tolerated.is_empty() {
            s.push_str(&format!(
                " (failures the floor cleared — recorded, not denying: {})",
                tolerated.join("; ")
            ));
        }
        // (core#467) The transcript's claim, judged.
        if let Some(claim) = &self.claim {
            let judged = match claim.verdict.as_str() {
                CLAIM_REJECTED => {
                    "REJECTED — the run base passes that check; the failure is this change's"
                }
                CLAIM_CONFIRMED => "confirmed — the run base fails it too",
                _ => "unverified — the base could not be compared",
            };
            s.push_str(&format!(
                ". The transcript claimed the `{}` failure was pre-existing (\"{}\"): {judged}",
                claim.check, claim.phrase
            ));
        }
        // (j4-gates) Which failing tests are THIS change's, from the floor's own structured
        // results: a tail can name only the base's failures (rig run 64f05d3d), and a worker told
        // to "fix those" then never sees the test it broke.
        if let Some(split) = self.failure_split() {
            s.push_str(&format!(". {}", split.sentence()));
        }
        s.push_str(
            ". The engine ran these itself in the run's worktree; the full tails are on the \
             unit's `repo_checks` record and the `repoChecksEvaluated` event.",
        );
        s
    }

    /// (j4-gates) The denying checks' failing tests split against the run base, from the floor's
    /// structured results (every [`CheckRun::failure_ids`] the check's head runs named — first
    /// run, excluded run, identical retry — against the base run's failure ids and the base's own
    /// failing inventory). `all_base_red` needs the deciding (last) run of every denying check to
    /// have finished with an exit code and named its failures. `None` when no denying check named
    /// a failing test (a lint, a type error, a timeout: the tail is all there is).
    pub fn failure_split(&self) -> Option<FailureSplit> {
        let mut split = FailureSplit::default();
        let mut named_any = false;
        let mut every_check_known = true;
        for c in self.checks.iter().filter(|c| c.denies()) {
            // The deciding run is the LAST one the decision made, whether or not it printed ids.
            // It is a complete account only when it ran to an exit code and named its failures; a
            // timed-out, crashed or unnamed run may hide a failure of the change's own (xreview).
            let deciding = c.reruns.last().unwrap_or(c);
            let deciding_complete = !deciding.failure_ids.is_empty()
                && deciding.exit_code.is_some()
                && !deciding.timed_out
                && deciding.spawn_error.is_none()
                && !c.did_not_finish();
            if !deciding_complete {
                every_check_known = false;
            }
            // Every failure any head run of this check named (first run, excluded run, retry):
            // a failure the head showed once is never dropped because a later run named nothing.
            let mut head: Vec<&str> = Vec::new();
            for run in std::iter::once(c).chain(c.reruns.iter()) {
                for id in &run.failure_ids {
                    if !head.contains(&id.as_str()) {
                        head.push(id.as_str());
                    }
                }
            }
            for run in std::iter::once(c).chain(c.reruns.iter()) {
                for id in &run.flaky_ids {
                    let named = format!("{}: {id}", c.name);
                    if !head.contains(&id.as_str()) && !split.flaky.contains(&named) {
                        split.flaky.push(named);
                    }
                }
            }
            if head.is_empty() {
                continue;
            }
            named_any = true;
            let base = c.base.as_deref();
            let base_run = base
                .and_then(|b| b.run.as_ref())
                .filter(|b| !b.timed_out && b.spawn_error.is_none());
            if split.base.is_none() {
                split.base = base.map(|b| b.head.clone()).filter(|h| !h.is_empty());
            }
            let Some(base_run) = base_run else {
                // No base to compare: every named failure is listed, and none is shown shared.
                every_check_known = false;
                split
                    .unknown
                    .extend(head.iter().map(|id| format!("{}: {id}", c.name)));
                continue;
            };
            let decision = c.decision.as_ref();
            let on_base = |id: &str| {
                base_run.failure_ids.iter().any(|b| b == id)
                    || decision.is_some_and(|d| {
                        d.base_failing.iter().any(|b| b == id) || d.excluded.iter().any(|b| b == id)
                    })
            };
            for id in head {
                if on_base(id) {
                    split.base_shared += 1;
                } else {
                    split.head_only.push(format!("{}: {id}", c.name));
                }
            }
        }
        if !named_any {
            return None;
        }
        split.all_base_red =
            every_check_known && split.head_only.is_empty() && split.base_shared > 0;
        Some(split)
    }

    /// A one-line account for `GateEvaluated`/logs: `install: exit 0 (31.2s), test: exit 1 (…)`.
    pub fn summary(&self) -> String {
        if let Some(e) = &self.sandbox_error {
            return format!("checks not run: {e}");
        }
        if let Some(e) = &self.detect_error {
            return format!("checks could not be determined: {e}");
        }
        if self.detected.is_empty() {
            return "no repository checks detected (no package.json scripts among typecheck/lint/\
                    test, no Cargo.toml)"
                .to_string();
        }
        let mut s = self
            .checks
            .iter()
            .map(CheckRun::summary)
            .collect::<Vec<_>>()
            .join(", ");
        // (core#416 item 3b) An opted-in run says so on the SAME line as its results, so a `passed`
        // floor is never read as a contained one.
        if self.unsandboxed() {
            s.push_str(&format!(
                " [no OS write boundary — {UNSANDBOXED_OPT_IN_ENV}=1]"
            ));
        }
        s
    }

    /// (core#416 item 3b) Did these checks run with NO OS write boundary, on the operator's
    /// explicit opt-in? The report's results stand; its containment claim does not.
    pub fn unsandboxed(&self) -> bool {
        self.sandbox_level == UNSANDBOXED_LEVEL
    }
}

/// The last three non-empty lines of a tail, joined with ` | ` and clipped — enough for a denial
/// sentence without pasting a whole log into it.
fn last_lines(tail: &str) -> String {
    let lines: Vec<&str> = tail
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let n = lines.len();
    let picked = &lines[n.saturating_sub(3)..];
    let joined = picked.join(" | ");
    if joined.chars().count() > 300 {
        let cut: String = joined.chars().take(297).collect();
        format!("{cut}...")
    } else {
        joined
    }
}

/// A probed worktree entry: present, not a symlink, and — for a regular file — opened without
/// following links (`file` is `None` for a directory: a directory is only ever tested for
/// presence, never read, and Windows refuses to `open` one).
struct Probed {
    meta: std::fs::Metadata,
    file: Option<std::fs::File>,
}

impl Probed {
    fn is_file(&self) -> bool {
        self.meta.is_file()
    }
}

#[cfg(target_os = "macos")]
const O_NOFOLLOW: i32 = 0x0100;
#[cfg(all(unix, not(target_os = "macos")))]
const O_NOFOLLOW: i32 = 0o400000;

/// Open `path` WITHOUT following a final symlink. On unix this is `O_NOFOLLOW` (a link fails with
/// `ELOOP`/`EMLINK`, which is reported as "is a symlink"); elsewhere the plain open, which the
/// caller's lstat-before / lstat-after bracket covers.
fn open_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new().read(true).open(path)
    }
}

/// Probe `rel` under the worktree without ever following a link, and without an lstat-then-open
/// window: `lstat` (a symlink is refused by name), open `O_NOFOLLOW`, `fstat` the OPENED descriptor
/// and require a regular-file/directory that is the same device + inode the lstat saw. `Ok(None)`
/// = absent; `Ok(Some(probed))` = the entry, with the descriptor the caller reads from (never the
/// path again); `Err` = a symlink, an identity mismatch (something swapped the entry between the
/// two syscalls), or an unreadable entry.
fn probe(worktree: &Path, rel: &str) -> Result<Option<Probed>, String> {
    let p = worktree.join(rel);
    let lstat = match std::fs::symlink_metadata(&p) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(format!(
                "`{rel}` is a symlink (the floor never follows links out of the worktree)"
            ))
        }
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("`{rel}` could not be inspected: {e}")),
    };
    if lstat.is_dir() {
        // Presence is all a directory probe answers (`node_modules/`); nothing is read from it,
        // so there is no open to race — the lstat already refused a link.
        return Ok(Some(Probed {
            meta: lstat,
            file: None,
        }));
    }
    let file = match open_nofollow(&p) {
        Ok(f) => f,
        Err(e) => {
            // `O_NOFOLLOW` on a link: ELOOP (Linux) / EMLINK (macOS, "Too many links").
            let raw = e.raw_os_error();
            if raw == Some(40) || raw == Some(31) || raw == Some(62) {
                return Err(format!(
                    "`{rel}` became a symlink between inspection and open (refused)"
                ));
            }
            return Err(format!("`{rel}` could not be opened: {e}"));
        }
    };
    let meta = file
        .metadata()
        .map_err(|e| format!("`{rel}` could not be fstat'ed after open: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.dev() != lstat.dev() || meta.ino() != lstat.ino() {
            return Err(format!(
                "`{rel}` changed identity between inspection and open (refused — something \\
                 swapped the entry)"
            ));
        }
    }
    #[cfg(not(unix))]
    {
        // No O_NOFOLLOW: re-lstat after the open and require the entry is still not a link.
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!(
                    "`{rel}` became a symlink between inspection and open (refused)"
                ))
            }
            Ok(_) => {}
            Err(e) => return Err(format!("`{rel}` vanished after open: {e}")),
        }
        let _ = &lstat;
    }
    if meta.file_type().is_symlink() {
        return Err(format!("`{rel}` is a symlink (refused)"));
    }
    Ok(Some(Probed {
        meta,
        file: Some(file),
    }))
}

/// Which package manager a Node repo uses, read off its lockfile (npm when none says otherwise).
fn package_manager(worktree: &Path) -> Result<&'static str, String> {
    if probe(worktree, "pnpm-lock.yaml")?.is_some() {
        Ok("pnpm")
    } else if probe(worktree, "yarn.lock")?.is_some() {
        Ok("yarn")
    } else {
        Ok("npm")
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// A configured check command in [`CONFIG_PATH`]: an argv array, a whitespace-split line, or
/// `false` to disable the auto-detected check of that name. No shell is involved — quote nothing;
/// use the array form for an argument that contains a space.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum CheckCommand {
    Argv(Vec<String>),
    Line(String),
    Flag(bool),
}

/// What a configured slot resolved to.
enum Resolved {
    /// Not configured — keep the auto-detected check, if any.
    Default,
    /// `false` — drop the auto-detected check.
    Disabled,
    Command(Vec<String>),
}

fn resolve(cmd: &Option<CheckCommand>, key: &str) -> Result<Resolved, String> {
    let bad = |what: &str| {
        Err(format!(
            "`{CONFIG_PATH}` `{key}`: {what} (give an argv array or a string, or `false` to \
             disable the auto-detected check)"
        ))
    };
    match cmd {
        None => Ok(Resolved::Default),
        Some(CheckCommand::Flag(false)) => Ok(Resolved::Disabled),
        Some(CheckCommand::Flag(true)) => bad("`true` is not a command"),
        Some(CheckCommand::Argv(v)) => {
            if v.is_empty() || v.iter().any(|a| a.trim().is_empty()) {
                bad("an empty argv (or an empty element) is not a command")
            } else {
                Ok(Resolved::Command(v.clone()))
            }
        }
        Some(CheckCommand::Line(l)) => {
            let v: Vec<String> = l.split_whitespace().map(String::from).collect();
            if v.is_empty() {
                bad("an empty string is not a command")
            } else {
                Ok(Resolved::Command(v))
            }
        }
    }
}

fn default_true() -> bool {
    true
}

/// The per-repo check configuration ([`CONFIG_PATH`]) — every field optional, unknown keys
/// refused (a typo must not silently mean "default").
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChecksConfig {
    typecheck: Option<CheckCommand>,
    lint: Option<CheckCommand>,
    test: Option<CheckCommand>,
    /// The change-scoped test command the floor PREFERS (`{files}` / `{base}` placeholders).
    test_targeted: Option<CheckCommand>,
    /// An end-to-end suite, run at the VERIFY stage only, after `test`/`test_targeted` — never at
    /// the creator floor (core#482 / F-3R2-023). Nothing is auto-detected: absent ⇒ no `e2e`
    /// check; `false` is accepted and means the same. `timeout_s` and the baseline diff apply
    /// (a base lacking the key fails closed: "the change introduced it").
    e2e: Option<CheckCommand>,
    /// (core#767) Named end-to-end JOURNEYS — `{"desk_projects": ["python3", "e2e/desk_projects_test.py"]}`
    /// — each its own check `journey-<name>`, run at the VERIFY stage only, after `e2e`, in name
    /// order, under the same bound, baseline diff and boundary as every other check. A name is
    /// `[A-Za-z0-9_]{1,64}`; `false` drops one. The engine runs them itself, so a repository whose
    /// proof is a set of scripts (Python journeys) is verified by the floor even when no seat may
    /// run an interpreter.
    #[serde(default)]
    journeys: std::collections::BTreeMap<String, CheckCommand>,
    /// Override or disable the formatter check auto-detected from `Cargo.toml`. Default: when
    /// `Cargo.toml` is present, runs `cargo fmt --all --check`. Set to `false` to disable, or
    /// supply an explicit command. `timeout_s` applies; the baseline diff does not (a formatting
    /// diff is a deterministic regression, never environmental).
    formatter: Option<CheckCommand>,
    /// The per-check BASE bound in seconds (install keeps its own); scaled by the host-load factor.
    timeout_s: Option<u64>,
    /// Run the FULL `test` at verify even when `test_targeted` is declared.
    #[serde(default)]
    full: bool,
    /// Compare a failing check against the run base before denying (F-RC2-009). Default on.
    #[serde(default = "default_true")]
    baseline_diff: bool,
    /// (core#1173) The files the repository GENERATES and the command that regenerates each set
    /// ([`GeneratedFiles`]). The floor runs none of them; the deliver lift reads them from the tip
    /// it lifts onto, so a conflict in a declared file beside a changelog is regenerated, not
    /// stranded.
    #[serde(default)]
    generated: Vec<GeneratedDecl>,
}

/// (core#1173) One `generated` entry of [`CONFIG_PATH`] as written:
/// `{"paths": ["packages/x/manifest.json", "packages/x/tests/generated/**"], "command": [...]}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratedDecl {
    paths: Vec<String>,
    command: CheckCommand,
}

/// (core#1173) A set of generated files and the command that regenerates them, validated: every
/// path is repo-relative (`/`-separated, no `..`, no leading `/`) and may use `*` inside a segment
/// and `**` for any number of segments; the command is an argv (no shell). The deliver lift runs
/// the commands in the order declared (a later one may read an earlier one's output), in the
/// floor's sandbox, after the floor's own dependency install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GeneratedFiles {
    pub paths: Vec<String>,
    pub argv: Vec<String>,
}

impl GeneratedFiles {
    /// Is `path` one of this set's files?
    pub(crate) fn covers(&self, path: &str) -> bool {
        self.paths.iter().any(|p| workspace_glob_matches(p, path))
    }
}

/// (core#1173) The declared generated files of a [`CONFIG_PATH`] body, and its `timeout_s`. The
/// same strict parse the floor applies: a malformed file is an error, never "nothing declared".
pub(crate) fn generated_from_config(
    raw: &str,
) -> Result<(Vec<GeneratedFiles>, Option<u64>), String> {
    let cfg = parse_config(raw)?;
    Ok((generated_of(&cfg)?, cfg.timeout_s))
}

fn generated_of(cfg: &ChecksConfig) -> Result<Vec<GeneratedFiles>, String> {
    cfg.generated
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let key = format!("generated[{i}]");
            if g.paths.is_empty() {
                return Err(format!("`{CONFIG_PATH}` `{key}`: `paths` is empty"));
            }
            for p in &g.paths {
                let bad = p.trim().is_empty()
                    || p != p.trim()
                    || p.starts_with('/')
                    || p.contains('\\')
                    || p.split('/').any(|s| s.is_empty() || s == "." || s == "..");
                if bad {
                    return Err(format!(
                        "`{CONFIG_PATH}` `{key}`: `{p}` is not a repo-relative path \
                         (`/`-separated, no empty, `.` or `..` segment)"
                    ));
                }
            }
            match resolve(&Some(g.command.clone()), &format!("{key}.command"))? {
                Resolved::Command(argv) => Ok(GeneratedFiles {
                    paths: g.paths.clone(),
                    argv,
                }),
                _ => Err(format!(
                    "`{CONFIG_PATH}` `{key}`: `command` must be a command (an argv array or a \
                     string)"
                )),
            }
        })
        .collect()
}

/// Parse and validate a [`CONFIG_PATH`] body (unknown keys, a bad `timeout_s` or a bad
/// `generated` entry are errors).
fn parse_config(raw: &str) -> Result<ChecksConfig, String> {
    let cfg: ChecksConfig =
        serde_json::from_str(raw).map_err(|e| format!("`{CONFIG_PATH}` is not valid: {e}"))?;
    if let Some(t) = cfg.timeout_s {
        if t == 0 || t > MAX_CONFIG_TIMEOUT_S {
            return Err(format!(
                "`{CONFIG_PATH}` `timeout_s` must be 1..={MAX_CONFIG_TIMEOUT_S} seconds, got {t}"
            ));
        }
    }
    generated_of(&cfg)?;
    Ok(cfg)
}

/// Read [`CONFIG_PATH`] when present. Fail-closed: a malformed file, an unknown key, a bad value,
/// an out-of-range `timeout_s`, or a symlinked `.wicked` / `checks.json` is a detection error.
fn read_config(worktree: &Path) -> Result<Option<ChecksConfig>, String> {
    match probe(worktree, ".wicked")? {
        None => return Ok(None),
        Some(p) if !p.meta.is_dir() => return Err("`.wicked` is not a directory".to_string()),
        Some(_) => {}
    }
    let Some(probed) = probe(worktree, CONFIG_PATH)? else {
        return Ok(None);
    };
    let is_file = probed.is_file();
    let Some(mut file) = probed.file.filter(|_| is_file) else {
        return Err(format!("`{CONFIG_PATH}` is not a regular file"));
    };
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|e| format!("`{CONFIG_PATH}` could not be read: {e}"))?;
    parse_config(&raw).map(Some)
}

/// Does the repo opt out of the baseline diff? Read separately from detection so the run loop
/// needs no second detection pass; a config that failed to parse already failed detection.
fn baseline_diff_enabled(worktree: &Path) -> bool {
    read_config(worktree)
        .ok()
        .flatten()
        .map(|c| c.baseline_diff)
        .unwrap_or(true)
}

/// Apply a configured slot over the auto-detected check of the same name.
/// The formatter slot, whether auto-detected (`cargo-fmt-check`) or configured (`formatter`).
fn is_formatter_check(name: &str) -> bool {
    name == "cargo-fmt-check" || name == "formatter"
}

fn apply(slot: &mut Option<RepoCheck>, r: Resolved, key: &str) {
    match r {
        Resolved::Default => {}
        Resolved::Disabled => *slot = None,
        Resolved::Command(argv) => {
            *slot = Some(RepoCheck {
                name: key.to_string(),
                argv,
                source: format!("{CONFIG_PATH} {key}"),
                timeout_s: None,
                dir: String::new(),
            })
        }
    }
}

/// The paths the unit's change touched relative to the base commit — tracked changes (added,
/// copied, modified, renamed) plus untracked-not-ignored files, through the PINNED git dir; the
/// engine scratch is never a touched path. Empty when the run knows no base.
pub(crate) fn touched_files(worktree: &Path, ctx: &FloorContext) -> Result<Vec<String>, String> {
    if let Some((root, dir)) = package_root(worktree, ctx) {
        let root_ctx = FloorContext {
            package_dir: None,
            ..ctx.clone()
        };
        return Ok(package_relative(touched_files(&root, &root_ctx)?, &dir));
    }
    let (Some(base), Some(git_dir)) = (ctx.base_head.as_deref(), ctx.git_dir.as_deref()) else {
        return Ok(Vec::new());
    };
    let env: [(&str, &Path); 2] = [("GIT_DIR", git_dir), ("GIT_WORK_TREE", worktree)];
    // NUL-separated (`-z`): the real names, never quoted, trimmed or split on a newline.
    let tracked = crate::floor_guide::git_paths(
        worktree,
        &["diff", "--name-only", "-z", "--diff-filter=ACMR", base],
        &env,
    )
    .map_err(|e| format!("the change's touched files could not be listed: {e}"))?;
    let untracked = crate::floor_guide::git_paths(
        worktree,
        &["ls-files", "-z", "--others", "--exclude-standard"],
        &env,
    )
    .map_err(|e| format!("the change's untracked files could not be listed: {e}"))?;
    let scratch_prefix = format!("{}/", crate::worktree_guard::ENGINE_SCRATCH_DIR);
    let mut files: Vec<String> = Vec::new();
    for f in tracked.into_iter().chain(untracked) {
        if f.starts_with(&scratch_prefix) || files.contains(&f) {
            continue;
        }
        files.push(f);
    }
    Ok(files)
}

/// (DES-BUILD-GUIDE `invalidatedBy`) Every path the change touched: added, modified and untracked
/// ([`touched_files`]), plus [`removed_files`]: deleted, a rename's old path, and a type change.
fn changed_files(worktree: &Path, ctx: &FloorContext) -> Result<Vec<String>, String> {
    let mut files = touched_files(worktree, ctx)?;
    for d in removed_files(worktree, ctx)? {
        if !files.contains(&d) {
            files.push(d);
        }
    }
    Ok(files)
}

/// Did the change remove a tracked file relative to the base ([`removed_files`]: a deletion, a
/// rename's old path, or a type change)? A derived targeted set cannot see what a missing file
/// broke.
fn deletes_a_file(worktree: &Path, ctx: &FloorContext) -> Result<bool, String> {
    Ok(!removed_files(worktree, ctx)?.is_empty())
}

/// The tracked paths the change removed relative to the base: deleted (a rename's old path
/// included — `--no-renames`) or whose TYPE changed (`T`: a file replaced by a link, or a link by
/// a file — what the path was is gone, and it is in no added/modified list). Empty when the run
/// knows no base.
fn removed_files(worktree: &Path, ctx: &FloorContext) -> Result<Vec<String>, String> {
    if let Some((root, dir)) = package_root(worktree, ctx) {
        let root_ctx = FloorContext {
            package_dir: None,
            ..ctx.clone()
        };
        return Ok(package_relative(removed_files(&root, &root_ctx)?, &dir));
    }
    let (Some(base), Some(git_dir)) = (ctx.base_head.as_deref(), ctx.git_dir.as_deref()) else {
        return Ok(Vec::new());
    };
    let env: [(&str, &Path); 2] = [("GIT_DIR", git_dir), ("GIT_WORK_TREE", worktree)];
    crate::floor_guide::git_paths(
        worktree,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--diff-filter=DT",
            base,
        ],
        &env,
    )
    .map_err(|e| format!("the change's removed files could not be listed: {e}"))
}

/// Substitute `{files}` / `{base}` into a targeted command: a standalone `{files}` element expands
/// to one element per touched path; embedded, the placeholders are replaced in place.
fn substitute_placeholders(
    argv: Vec<String>,
    worktree: &Path,
    ctx: &FloorContext,
) -> Result<Vec<String>, String> {
    let wants_files = argv.iter().any(|a| a.contains("{files}"));
    let files = if wants_files {
        touched_files(worktree, ctx)?
    } else {
        Vec::new()
    };
    let base = ctx.base_head.clone().unwrap_or_default();
    let mut out = Vec::with_capacity(argv.len());
    for a in argv {
        if a == "{files}" {
            out.extend(files.iter().cloned());
        } else if a == "{base}" {
            out.push(base.clone());
        } else {
            out.push(
                a.replace("{files}", &files.join(" "))
                    .replace("{base}", &base),
            );
        }
    }
    Ok(out)
}

/// Detect the repository's own checks in `worktree` as the historical verify floor. Pure over the
/// filesystem — runs nothing. `Err` when a manifest exists but cannot be trusted (unreadable,
/// malformed, a symlink).
#[cfg(test)]
pub(crate) fn detect(worktree: &Path) -> Result<Vec<RepoCheck>, String> {
    detect_with(worktree, &FloorContext::default())
}

/// Detect the repository's own checks in `worktree` for `ctx`: the manifests' checks, overridden
/// by [`CONFIG_PATH`] where it speaks, the test set chosen by stage (targeted first — see the
/// module doc), `ctx.force_install` FORCING the dependency install step even when `node_modules/`
/// is provisioned (F-433-003: after a lift moved a lockfile the installed modules are stale and
/// the checks would fail for the wrong reason; always frozen and `--ignore-scripts`). Pure over
/// the filesystem (one `git diff` when a targeted command asks for `{files}`) — runs nothing.
pub(crate) fn detect_with(worktree: &Path, ctx: &FloorContext) -> Result<Vec<RepoCheck>, String> {
    let detected = detect_manifests(worktree, ctx)?;
    // (DES-BUILD-GUIDE) The base's guide replaces detection only for the directories its checks
    // cover: a package it does not cover keeps its detected checks (exit codes only). A STALE
    // guide (a `derivedFrom` file changed) may name outdated commands: there the floor runs the
    // UNION — the guide's checks and the detected ones (an identical argv in the same directory
    // once) — more checks, never fewer.
    Ok(match guide_checks(worktree, ctx)? {
        Some((mut guided, covered, stale)) => {
            let same = |a: &RepoCheck, b: &RepoCheck| a.argv == b.argv && a.dir == b.dir;
            // In a stale union a covered directory's detected `install` runs FIRST — before every
            // guided check (one that needs the dependencies included); a guided check with the
            // same argv and dir is that install, run once, there.
            let mut first: Vec<RepoCheck> = Vec::new();
            let mut rest: Vec<RepoCheck> = Vec::new();
            for mut c in detected {
                if !covered.contains(&c.dir) {
                    rest.push(c);
                } else if let Some(why) = stale.as_deref() {
                    c.source = format!(
                        "{} — run beside the base's stale build guide ({why})",
                        c.source
                    );
                    if c.name == "install" {
                        guided.retain(|g| !same(g, &c));
                        first.push(c);
                    } else if !guided.iter().any(|g| same(g, &c)) {
                        // A derived targeted set too: once, by argv and dir.
                        rest.push(c);
                    }
                }
            }
            first.into_iter().chain(guided).chain(rest).collect()
        }
        None => detected,
    })
}

/// The manifests' checks (see [`detect_with`]).
fn detect_manifests(worktree: &Path, ctx: &FloorContext) -> Result<Vec<RepoCheck>, String> {
    let pkgs = subdir_packages(worktree, ctx)?;
    if pkgs.dirs.is_empty() {
        return detect_package(worktree, ctx);
    }
    // (core#817) The change lives (at least partly) in a subdirectory package the root's checks
    // do not cover: that package's own checks run, in its directory. The root's checks run as
    // well when the change also touches root-owned paths or the repo DECLARES its checks.
    // The root is detected only then (a malformed root manifest cannot fail a subdir change).
    let mut out = if pkgs.root_touched || read_config(worktree)?.is_some() {
        detect_package(worktree, ctx)?
    } else {
        Vec::new()
    };
    for dir in &pkgs.dirs {
        let sub_ctx = FloorContext {
            package_dir: Some(dir.clone()),
            claim_text: None,
            ..ctx.clone()
        };
        for mut c in detect_package(&worktree.join(dir), &sub_ctx)? {
            c.source = format!("{dir}/{}", c.source);
            c.dir = dir.clone();
            out.push(c);
        }
    }
    Ok(out)
}

/// (core#817) For a subdirectory package's context: the repository root (`worktree` minus
/// `package_dir`) and the dir, or `None` at the root.
fn package_root(worktree: &Path, ctx: &FloorContext) -> Option<(PathBuf, String)> {
    let dir = ctx.package_dir.as_deref().filter(|d| !d.is_empty())?;
    let depth = dir.split('/').filter(|s| !s.is_empty()).count();
    let root = worktree.ancestors().nth(depth)?.to_path_buf();
    (root.join(dir) == worktree).then(|| (root, dir.to_string()))
}

/// Repo-relative paths under `dir/`, made relative to it; others dropped.
pub(crate) fn package_relative(files: Vec<String>, dir: &str) -> Vec<String> {
    let dir = dir.trim_matches('/');
    if dir.is_empty() || dir == "." {
        return files;
    }
    let prefix = format!("{dir}/");
    files
        .into_iter()
        .filter_map(|f| f.strip_prefix(&prefix).map(str::to_string))
        .collect()
}

/// (core#817) Where the change sits relative to the repo's packages.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ChangePackages {
    /// Subdirectory packages (repo-relative) holding a changed path that the root's checks do not
    /// cover, sorted.
    pub dirs: Vec<String>,
    /// A changed path the root package owns (no nearer uncovered manifest).
    pub root_touched: bool,
}

/// The manifests that make a directory a package of its own.
const PACKAGE_MANIFESTS: &[&str] = &["package.json", "Cargo.toml"];

/// Is `rel` (a directory, repo-relative) free of symlinks along the way? A linked directory is
/// never followed into — the probe that reads its manifest would read outside the tree.
fn plain_dir(worktree: &Path, rel: &str) -> bool {
    let mut p = worktree.to_path_buf();
    for seg in rel.split('/') {
        p.push(seg);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => {}
            _ => return false,
        }
    }
    true
}

/// Does the ROOT's own check cover the package at `dir` (repo-relative) of `manifest` kind — a
/// Node workspace member (`dir` matches the root `package.json` `workspaces` globs, or
/// `pnpm-workspace.yaml` `packages`, `!` exclusions honoured), or any root `Cargo.toml` (a
/// workspace, or a crate whose members cargo builds)?
fn root_covers(worktree: &Path, manifest: &str, dir: &str) -> bool {
    if manifest != "package.json" {
        return worktree.join("Cargo.toml").is_file();
    }
    let mut globs: Vec<String> = Vec::new();
    if let Some(v) = std::fs::read_to_string(worktree.join("package.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
    {
        // `"workspaces": [...]` or yarn's `"workspaces": {"packages": [...]}`.
        let ws = v.get("workspaces");
        let list = ws.and_then(|w| w.get("packages")).or(ws);
        if let Some(a) = list.and_then(|l| l.as_array()) {
            globs.extend(a.iter().filter_map(|g| g.as_str().map(str::to_string)));
        }
    }
    if let Some(v) = std::fs::read_to_string(worktree.join("pnpm-workspace.yaml"))
        .ok()
        .and_then(|raw| serde_yaml::from_str::<serde_yaml::Value>(&raw).ok())
    {
        if let Some(a) = v.get("packages").and_then(|p| p.as_sequence()) {
            globs.extend(a.iter().filter_map(|g| g.as_str().map(str::to_string)));
        }
    }
    let norm = |g: &str| g.trim_start_matches("./").trim_end_matches('/').to_string();
    let included = globs
        .iter()
        .filter(|g| !g.starts_with('!'))
        .any(|g| workspace_glob_matches(&norm(g), dir));
    let excluded = globs
        .iter()
        .filter_map(|g| g.strip_prefix('!'))
        .any(|g| workspace_glob_matches(&norm(g), dir));
    included && !excluded
}

/// A workspace glob over `/`-separated segments: `**` matches any number of segments, and inside
/// a segment `*` matches any run of characters (`packages/*`, `apps/**`, `libs/ui-*`).
fn workspace_glob_matches(glob: &str, dir: &str) -> bool {
    fn seg(p: &str, s: &str) -> bool {
        match p.split_once('*') {
            None => p == s,
            Some((pre, rest)) => {
                let Some(tail) = s.strip_prefix(pre) else {
                    return false;
                };
                (0..=tail.len())
                    .filter(|i| tail.is_char_boundary(*i))
                    .any(|i| seg(rest, &tail[i..]))
            }
        }
    }
    fn go(p: &[&str], d: &[&str]) -> bool {
        match (p.first(), d.first()) {
            (None, None) => true,
            (Some(&"**"), _) => go(&p[1..], d) || (!d.is_empty() && go(p, &d[1..])),
            (Some(ps), Some(ds)) => seg(ps, ds) && go(&p[1..], &d[1..]),
            _ => false,
        }
    }
    let p: Vec<&str> = glob.split('/').filter(|s| !s.is_empty()).collect();
    let d: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    go(&p, &d)
}

/// (core#817) The subdirectory packages the change touches: for each changed path (touched or
/// deleted, relative to the base), the NEAREST ancestor directory below the root that holds a
/// manifest ([`PACKAGE_MANIFESTS`]) the root does not cover ([`root_covers`]). Paths under
/// `node_modules/` or the engine scratch never count; a linked directory is never followed. No
/// base ⇒ nothing (the root's checks, as before).
pub(crate) fn subdir_packages(
    worktree: &Path,
    ctx: &FloorContext,
) -> Result<ChangePackages, String> {
    let mut files = touched_files(worktree, ctx)?;
    files.extend(removed_files(worktree, ctx)?);
    let mut out = ChangePackages::default();
    for f in &files {
        let mut owner: Option<String> = None;
        let mut dir = f.rsplit_once('/').map(|(d, _)| d);
        while let Some(d) = dir.filter(|d| !d.is_empty()) {
            if d.split('/').any(|s| s == "node_modules")
                || d == crate::worktree_guard::ENGINE_SCRATCH_DIR
                || d.starts_with(&format!("{}/", crate::worktree_guard::ENGINE_SCRATCH_DIR))
            {
                break;
            }
            if let Some(m) = PACKAGE_MANIFESTS
                .iter()
                .find(|m| worktree.join(d).join(m).is_file())
            {
                if !root_covers(worktree, m, d) && plain_dir(worktree, d) {
                    owner = Some(d.to_string());
                }
                break;
            }
            dir = d.rsplit_once('/').map(|(p, _)| p);
        }
        match owner {
            Some(d) => {
                if !out.dirs.contains(&d) {
                    out.dirs.push(d);
                }
            }
            None => out.root_touched = true,
        }
    }
    out.dirs.sort();
    Ok(out)
}

/// One package's checks — the root's, or (core#817) a subdirectory package's.
fn detect_package(worktree: &Path, ctx: &FloorContext) -> Result<Vec<RepoCheck>, String> {
    let cfg = read_config(worktree)?.unwrap_or_default();
    let r_typecheck = resolve(&cfg.typecheck, "typecheck")?;
    let r_lint = resolve(&cfg.lint, "lint")?;
    let r_test = resolve(&cfg.test, "test")?;
    let r_targeted = resolve(&cfg.test_targeted, "test_targeted")?;
    let r_e2e = resolve(&cfg.e2e, "e2e")?;
    let r_formatter = resolve(&cfg.formatter, "formatter")?;
    // (core#767) Journeys: validated always (a bad entry fails detection at every stage), run at
    // VERIFY only.
    let mut journeys: Vec<RepoCheck> = Vec::new();
    for (name, cmd) in &cfg.journeys {
        if name.is_empty()
            || name.len() > 64
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(format!(
                "`{CONFIG_PATH}` `journeys`: `{name}` is not a journey name ([A-Za-z0-9_]{{1,64}})"
            ));
        }
        let key = format!("journeys.{name}");
        if let Resolved::Command(argv) = resolve(&Some(cmd.clone()), &key)? {
            if ctx.stage == FloorStage::Verify {
                journeys.push(RepoCheck {
                    name: format!("{JOURNEY_PREFIX}{name}"),
                    argv,
                    source: format!("{CONFIG_PATH} {key}"),
                    timeout_s: None,
                    dir: String::new(),
                });
            }
        }
    }
    // `e2e` is a VERIFY-stage check only (the base run copies the stage, so never at the creator).
    let e2e = match r_e2e {
        Resolved::Command(argv) if ctx.stage == FloorStage::Verify => Some(RepoCheck {
            name: "e2e".into(),
            argv,
            source: format!("{CONFIG_PATH} e2e"),
            timeout_s: None,
            dir: String::new(),
        }),
        _ => None,
    };
    // A configured command (Node-shaped or not — the rule is the same for every configured slot)
    // needs the tree provisioned when a `package.json` is present and `node_modules` is missing:
    // `npx vitest …` resolves from node_modules.
    let configured_node = [&r_typecheck, &r_lint, &r_test, &r_targeted, &r_formatter]
        .iter()
        .any(|r| matches!(r, Resolved::Command(_)))
        || e2e.is_some()
        || !journeys.is_empty();
    let mut install: Option<RepoCheck> = None;
    let mut typecheck: Option<RepoCheck> = None;
    let mut lint: Option<RepoCheck> = None;
    let mut test: Option<RepoCheck> = None;
    let mut cargo_fmt: Option<RepoCheck> = None;
    let mut cargo: Option<RepoCheck> = None;
    let mut node_test_script: Option<(&'static str, String)> = None;
    if let Some(probed) = probe(worktree, "package.json")? {
        let is_file = probed.is_file();
        let Some(mut file) = probed.file.filter(|_| is_file) else {
            return Err("`package.json` is not a regular file".to_string());
        };
        // Read from the descriptor the probe opened — never the path again.
        let mut raw = String::new();
        file.read_to_string(&mut raw)
            .map_err(|e| format!("`package.json` could not be read: {e}"))?;
        let json: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| format!("`package.json` is not valid JSON: {e}"))?;
        let scripts = match json.get("scripts") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::Object(m)) => Some(m),
            Some(_) => return Err("`package.json` `scripts` is not an object".to_string()),
        };
        let pm = package_manager(worktree)?;
        node_test_script = scripts
            .and_then(|m| m.get("test"))
            .and_then(|v| v.as_str())
            .map(|v| (pm, v.trim().to_string()));
        let wanted: Vec<&str> = ["typecheck", "lint", "test"]
            .into_iter()
            .filter(|k| scripts.is_some_and(|m| m.get(*k).and_then(|v| v.as_str()).is_some()))
            .collect();
        if !wanted.is_empty() || configured_node {
            // PROVISIONING (F-E2E-029): presence of `node_modules/` is not an install — see
            // `node_modules_gap`. The gap names the first declared dependency that is missing, so
            // the `install` check's `source` says WHY it ran.
            let gap = match probe(worktree, "node_modules")? {
                None => Some("node_modules absent".to_string()),
                // (core#1155) An install the engine made for the SEATS' platform (a container
                // seat's Linux) does not run this host's checks: its native bindings are not the
                // host's.
                Some(_) => match crate::seat_deps::foreign_platform_gap(worktree) {
                    Some(why) => Some(why),
                    None => node_modules_gap(worktree, &json)?,
                },
            };
            if gap.is_some() || ctx.force_install {
                let why = gap.unwrap_or_else(|| "forced: lockfile drift".to_string());
                let has_lock = probe(worktree, "package-lock.json")?.is_some();
                let (argv, source) = match pm {
                    "pnpm" => (
                        s(&["pnpm", "install", "--frozen-lockfile", "--ignore-scripts"]),
                        format!("pnpm-lock.yaml ({why})"),
                    ),
                    "yarn" => (
                        s(&["yarn", "install", "--frozen-lockfile", "--ignore-scripts"]),
                        format!("yarn.lock ({why})"),
                    ),
                    _ if has_lock => (
                        s(&["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"]),
                        format!("package-lock.json ({why})"),
                    ),
                    // No lockfile: `--no-package-lock`, or npm would write one into the reviewed
                    // tree and the worktree guard's final comparison would (correctly) deny it
                    // (Copilot on #414).
                    _ => (
                        s(&[
                            "npm",
                            "install",
                            "--ignore-scripts",
                            "--no-package-lock",
                            "--no-audit",
                            "--no-fund",
                        ]),
                        format!("package.json ({why}, no lockfile)"),
                    ),
                };
                install = Some(RepoCheck {
                    name: "install".into(),
                    argv,
                    source,
                    timeout_s: None,
                    dir: String::new(),
                });
            }
            for k in wanted {
                let check = RepoCheck {
                    name: k.to_string(),
                    argv: s(&[pm, "run", k]),
                    source: format!("package.json scripts.{k}"),
                    timeout_s: None,
                    dir: String::new(),
                };
                match k {
                    "typecheck" => typecheck = Some(check),
                    "lint" => lint = Some(check),
                    _ => test = Some(check),
                }
            }
        }
    }
    if let Some(probed) = probe(worktree, "Cargo.toml")? {
        if !probed.is_file() {
            return Err("`Cargo.toml` is not a regular file".to_string());
        }
        // With a lockfile, `--locked`: the check must not rewrite the repo's own `Cargo.lock`
        // (the worktree guard would deny the engine's side effect). Without one, cargo WILL write
        // a `Cargo.lock` beside the manifest — `run_with_sandbox` removes that engine-written
        // file afterwards, provably ours because it was absent here (adversarial review on #414).
        let has_lock = match probe(worktree, "Cargo.lock")? {
            Some(p) if p.is_file() => true,
            Some(_) => return Err("`Cargo.lock` is not a regular file".to_string()),
            None => false,
        };
        cargo_fmt = Some(RepoCheck {
            name: "cargo-fmt-check".into(),
            argv: s(&["cargo", "fmt", "--all", "--check"]),
            source: "Cargo.toml".into(),
            timeout_s: None,
            dir: String::new(),
        });
        // `--no-fail-fast` (core#481): cargo otherwise stops at the first failing test binary, so
        // a crate whose red test the base shares would hide every later crate from the baseline
        // diff — on both trees, which then compare equal.
        cargo = Some(RepoCheck {
            name: "cargo-test".into(),
            argv: if has_lock {
                s(&["cargo", "test", "--locked", "--no-fail-fast"])
            } else {
                s(&["cargo", "test", "--no-fail-fast"])
            },
            source: "Cargo.toml".into(),
            timeout_s: None,
            dir: String::new(),
        });
    }
    // The repo config speaks over the manifests.
    apply(&mut typecheck, r_typecheck, "typecheck");
    apply(&mut lint, r_lint, "lint");
    apply(&mut cargo_fmt, r_formatter, "formatter");
    // A configured `test` replaces EVERY auto-detected test check (`test` and `cargo-test`);
    // `false` removes them.
    match &r_test {
        Resolved::Default => {}
        Resolved::Disabled => {
            test = None;
            cargo = None;
        }
        Resolved::Command(argv) => {
            test = Some(RepoCheck {
                name: "test".into(),
                argv: argv.clone(),
                source: format!("{CONFIG_PATH} test"),
                timeout_s: None,
                dir: String::new(),
            });
            cargo = None;
        }
    }
    // TARGETED FIRST (core#469): the declared change-scoped command stands in for the full test
    // set at the creator stage always, and at verify unless the repo says `full: true`. A command
    // anchored on `{files}` / `{base}` needs a known base; without one the full set runs.
    if let Resolved::Command(argv) = &r_targeted {
        let argv = argv.clone();
        let full_here = ctx.stage == FloorStage::Verify && cfg.full;
        let anchored = argv
            .iter()
            .any(|a| a.contains("{files}") || a.contains("{base}"));
        let runnable_here = !anchored || ctx.base_head.is_some();
        if !full_here && runnable_here {
            let argv = substitute_placeholders(argv, worktree, ctx)?;
            test = Some(RepoCheck {
                name: "test_targeted".into(),
                argv,
                source: format!("{CONFIG_PATH} test_targeted"),
                timeout_s: None,
                dir: String::new(),
            });
            cargo = None;
        }
    }
    // DERIVED TARGETED (core#482): a repo that declares no test commands of its own gets a
    // change-scoped set derived from the touched files at the CREATOR stage — the verify floor
    // keeps the full suite, so an unconfigured repo pays for one full run, not three.
    if ctx.stage == FloorStage::Creator
        && matches!(r_targeted, Resolved::Default)
        && matches!(r_test, Resolved::Default)
        && ctx.base_head.is_some()
        && ctx.git_dir.is_some()
    {
        if let Some(derived) = derive_targeted(worktree, ctx, &test, &cargo, &node_test_script)? {
            test = Some(derived);
            cargo = None;
        }
    }
    // `e2e` LAST: after the test set, only where the stage admits it (see above).
    // The formatter runs right after `install` and BEFORE every other check (core#551 review): a
    // formatting diff is the cheapest, most deterministic denial, and CI runs it first too. A test
    // that fails earlier would otherwise skip it and hide the "format first" denial.
    let mut out: Vec<RepoCheck> = [install, cargo_fmt, typecheck, lint, test, cargo, e2e]
        .into_iter()
        .flatten()
        .chain(journeys)
        .collect();
    if let Some(t) = cfg.timeout_s {
        for c in out.iter_mut().filter(|c| c.name != "install") {
            c.timeout_s = Some(t);
        }
    }
    Ok(out)
}

/// A base guide's checks for the stage, the directories its checks cover, and why the guide is
/// stale (`None`: it is not — [`crate::floor_guide::BuildGuide::stale`]).
type GuidedChecks = (Vec<RepoCheck>, Vec<String>, Option<String>);

/// The `source` of a check the base's build guide declares.
pub const GUIDE_SOURCE: &str = "the run base's build guide (DES-BUILD-GUIDE)";

/// (DES-BUILD-GUIDE) When the run base carries a build guide, ITS checks for this stage, and the
/// directories its checks cover (any stage): there they replace detection (only [`CONFIG_PATH`]'s
/// `timeout_s` still bounds them) — unless the guide is stale, when detection runs beside them. `None` — detection, the fallback — when the base has no guide,
/// or one that does not parse (each check's `decision.guide` then says why).
fn guide_checks(worktree: &Path, ctx: &FloorContext) -> Result<Option<GuidedChecks>, String> {
    let (Some(base), Some(git_dir), None) = (
        ctx.base_head.as_deref(),
        ctx.git_dir.as_deref(),
        ctx.package_dir.as_deref(),
    ) else {
        return Ok(None);
    };
    let Ok(Some(guide)) = crate::floor_guide::load_base(worktree, git_dir, base) else {
        return Ok(None);
    };
    let timeout_s = read_config(worktree)?.and_then(|c| c.timeout_s);
    let checks = guide
        .checks_at(ctx.stage.as_wire())
        .map(|c| RepoCheck {
            name: c.name.clone(),
            argv: c.run.clone(),
            source: GUIDE_SOURCE.into(),
            timeout_s,
            dir: c.dir().to_string(),
        })
        .collect();
    let covered = guide.checks.iter().map(|c| c.dir().to_string()).collect();
    let stale = guide.stale(worktree, git_dir, base, &changed_files(worktree, ctx));
    Ok(Some((checks, covered, stale)))
}

/// The derived source string on a [`derive_targeted`] check.
pub const DERIVED_TARGETED_SOURCE: &str =
    "derived from the touched files (no `.wicked/checks.json` \
                                           test commands; core#482)";

/// (core#482) The change-scoped test command for a repo that configures none, derived from the
/// touched files, or `None` to keep the full set:
/// * cargo — `cargo test … -p <crate>` for each package owning a touched file (the nearest
///   `Cargo.toml` with a `[package]` name); a touched `Cargo.lock`, `.cargo/…`, `rust-toolchain*`
///   or a path no package owns (a workspace manifest, a shared file, docs beside a virtual
///   workspace) can affect every crate ⇒ full. The narrowing stops at the owning crates — their
///   dependents are the verify floor's full suite to test;
/// * any deleted (or renamed-away) tracked file ⇒ full: what it left behind cannot be mapped;
/// * vitest — a `test` script that is exactly `vitest` / `vitest run` ⇒ `vitest related --run
///   --passWithNoTests <files>` (the touched test files and the tests importing touched modules);
///   a touched manifest, lockfile, vite/vitest/ts config or test setup file ⇒ full.
///
/// Only when exactly ONE auto-detected test set exists (a mixed Node + cargo repo keeps both
/// full), and never with no touched file.
fn derive_targeted(
    worktree: &Path,
    ctx: &FloorContext,
    test: &Option<RepoCheck>,
    cargo: &Option<RepoCheck>,
    node_test_script: &Option<(&'static str, String)>,
) -> Result<Option<RepoCheck>, String> {
    let files = touched_files(worktree, ctx)?;
    if files.is_empty() || deletes_a_file(worktree, ctx)? {
        // A deleted, renamed-away or type-changed path is not in `files`, and the crate or test
        // it left can break without being named — never narrow past it.
        return Ok(None);
    }
    let base_name = |f: &str| f.rsplit('/').next().unwrap_or(f).to_string();
    let argv = match (test, cargo) {
        (None, Some(c)) => {
            let mut crates: Vec<String> = Vec::new();
            for f in &files {
                let name = base_name(f);
                if name == "Cargo.lock"
                    || f.starts_with(".cargo/")
                    || name.starts_with("rust-toolchain")
                {
                    return Ok(None);
                }
                // A path no package owns (a workspace manifest, a shared `include!`d file, docs
                // beside a virtual workspace) can reach any crate: never narrow past it.
                match cargo_package_of(worktree, f)? {
                    Some(p) => {
                        if !crates.contains(&p) {
                            crates.push(p);
                        }
                    }
                    None => return Ok(None),
                }
            }
            if crates.is_empty() {
                return Ok(None);
            }
            let mut argv = c.argv.clone();
            for p in crates {
                argv.push("-p".into());
                argv.push(p);
            }
            argv
        }
        (Some(_), None) => {
            let Some((pm, script)) = node_test_script else {
                return Ok(None);
            };
            if script != "vitest" && script != "vitest run" {
                return Ok(None);
            }
            let wide = |f: &str| {
                let n = base_name(f);
                matches!(
                    n.as_str(),
                    "package.json" | "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock"
                ) || n.starts_with("vitest.config")
                    || n.starts_with("vite.config")
                    || n.starts_with("tsconfig")
                    // A setup file (`tests/setup.ts`) is loaded by the config, never imported, so
                    // `related` cannot see what it reaches.
                    || n.to_ascii_lowercase().contains("setup")
            };
            if files.iter().any(|f| wide(f)) {
                return Ok(None);
            }
            let mut argv = match *pm {
                "pnpm" => s(&["pnpm", "exec", "vitest"]),
                "yarn" => s(&["yarn", "vitest"]),
                _ => s(&["npx", "--no-install", "vitest"]),
            };
            argv.extend(s(&["related", "--run", "--passWithNoTests"]));
            argv.extend(files);
            argv
        }
        _ => return Ok(None),
    };
    Ok(Some(RepoCheck {
        name: "test_targeted".into(),
        argv,
        source: DERIVED_TARGETED_SOURCE.into(),
        timeout_s: None,
        dir: String::new(),
    }))
}

/// The package owning `rel`: the nearest `Cargo.toml` at or above its directory, read without
/// following links; `None` when that manifest declares no `[package]` (a virtual workspace root)
/// or there is none.
fn cargo_package_of(worktree: &Path, rel: &str) -> Result<Option<String>, String> {
    let mut dir: Option<&str> = Some(rel.rsplit_once('/').map(|(d, _)| d).unwrap_or(""));
    while let Some(d) = dir {
        let manifest = if d.is_empty() {
            "Cargo.toml".to_string()
        } else {
            format!("{d}/Cargo.toml")
        };
        if let Some(probed) = probe(worktree, &manifest)? {
            let is_file = probed.is_file();
            let Some(mut file) = probed.file.filter(|_| is_file) else {
                return Err(format!("`{manifest}` is not a regular file"));
            };
            let mut raw = String::new();
            file.read_to_string(&mut raw)
                .map_err(|e| format!("`{manifest}` could not be read: {e}"))?;
            return Ok(cargo_package_name(&raw));
        }
        dir = if d.is_empty() {
            None
        } else {
            Some(d.rsplit_once('/').map(|(p, _)| p).unwrap_or(""))
        };
    }
    Ok(None)
}

/// `[package] name = "…"` off a manifest — a line scan, enough for the one key; a name that is not
/// a plain crate name is refused (it becomes an argv element).
fn cargo_package_name(raw: &str) -> Option<String> {
    let mut in_package = false;
    for line in raw.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_package = l == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some(rest) = l.strip_prefix("name") else {
            continue;
        };
        let Some(v) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let v = v.split('#').next().unwrap_or("").trim().trim_matches('"');
        if !v.is_empty()
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Some(v.to_string());
        }
        return None;
    }
    None
}

/// (core#1155) Whether every dependency `worktree`'s root `package.json` declares is present in
/// its `node_modules/` ([`node_modules_gap`] finds none missing).
pub(crate) fn declared_deps_present(worktree: &Path) -> Result<bool, String> {
    let raw = std::fs::read(worktree.join("package.json"))
        .map_err(|e| format!("`package.json` could not be read: {e}"))?;
    let json: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| format!("`package.json` is not JSON: {e}"))?;
    Ok(node_modules_gap(worktree, &json)?.is_none())
}

/// Why the worktree's `node_modules/` does NOT provision the checks, or `None` when it does.
///
/// Presence alone is not provisioning (F-E2E-029). Run `0ab5ccb8`'s worktree — nested under the
/// customer's clone at `<repo>/wicked-worktrees/<run>` — carried a `node_modules/` holding only a
/// test runner's cache (`node_modules/.vite/…`, written when the creator ran the suite; Node had
/// resolved the runner UPWARD into the clone root's own install), so the floor skipped the install
/// step, `npm run test` started (the runner resolved from the parent again) and three
/// path-relative suites died on ENOENT under `<worktree>/node_modules/wicked-crew-api-types/` — a
/// deterministic denial the operator could only clear by steering the read-only evaluator to run
/// `npm ci` itself. The floor now requires every DECLARED top-level dependency (`dependencies` +
/// `devDependencies`) to be present as `node_modules/<name>/package.json`; a symlinked package (a
/// workspace member, `npm link`) counts — its presence is all that is checked, nothing under it is
/// read or followed. Optional and peer dependencies are not required (they may legitimately be
/// absent). The first missing dependency names the reason; a manifest declaring none is
/// provisioned by definition.
fn node_modules_gap(
    worktree: &Path,
    package_json: &serde_json::Value,
) -> Result<Option<String>, String> {
    let mut declared: Vec<&str> = Vec::new();
    for key in ["dependencies", "devDependencies"] {
        match package_json.get(key) {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::Object(m)) => declared.extend(m.keys().map(String::as_str)),
            Some(_) => return Err(format!("`package.json` `{key}` is not an object")),
        }
    }
    for name in declared {
        // `@scope/name` is two path segments; anything that would leave `node_modules/` is not a
        // dependency name and is reported rather than probed.
        if name.is_empty()
            || name
                .split('/')
                .any(|seg| seg.is_empty() || seg == "." || seg == "..")
        {
            return Err(format!(
                "`package.json` declares a dependency with an invalid name `{name}`"
            ));
        }
        let entry = worktree.join("node_modules").join(name);
        let present = match std::fs::symlink_metadata(&entry) {
            Ok(m) if m.file_type().is_symlink() => true,
            Ok(m) if m.is_dir() => entry.join("package.json").is_file(),
            Ok(_) => false,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(format!("`node_modules/{name}` could not be inspected: {e}")),
        };
        if !present {
            return Ok(Some(format!(
                "node_modules present but `{name}` is not installed — a hollow or partial tree \
                 (e.g. only a tool's cache dir), so the checks would resolve modules outside \
                 the worktree or fail on ENOENT"
            )));
        }
    }
    Ok(None)
}

/// Files the engine's own checks may CREATE beside a manifest that ships none — removed after the
/// checks so the worktree guard never denies the engine's side effect. Only a file that was
/// ABSENT at detection time is a candidate: the seat's work is quiesced before detection, so a
/// file that appears between detection and the end of the checks was written by the checks.
fn engine_generated_candidates(worktree: &Path, detected: &[RepoCheck]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in detected {
        if c.argv.first().is_none_or(|b| b != "cargo") || c.argv.iter().any(|a| a == "--locked") {
            continue;
        }
        // (core#817) Beside the manifest the check ran on — a subdirectory package's own.
        let rel = if c.dir.is_empty() {
            "Cargo.lock".to_string()
        } else {
            format!("{}/Cargo.lock", c.dir)
        };
        if !worktree.join(&rel).exists() && !out.contains(&rel) {
            out.push(rel);
        }
    }
    out
}

/// What a check process may see of the daemon's environment — everything else is dropped
/// (adversarial review on #414). Non-secret by construction: the search path, locale, terminal,
/// the user's name, the Windows shell essentials (so `npm.cmd`/`sh` can start at all), and the
/// rustup toolchain root. The isolation overrides are set on top by `CheckScratch::apply_env`.
const CHECK_ENV_PASSTHROUGH: &[&str] = &[
    "PATH",
    "LANG",
    "TERM",
    "USER",
    "LOGNAME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    // Windows shell/runtime essentials.
    "SystemRoot",
    "windir",
    "ComSpec",
    "PATHEXT",
    "USERPROFILE",
    "SystemDrive",
    "NUMBER_OF_PROCESSORS",
];

/// The isolated homes the checks run with, all in the run's leaf of the engine's check cache
/// (`<worker home>/repo-checks/<repo key>/<run key>/`, [`scratch_dir_for`]).
#[derive(Debug)]
pub(crate) struct CheckScratch {
    root: PathBuf,
    /// The checks' `TMPDIR`: `<system temp>/wc-<6 hex>`, private (0700), drawn fresh per floor and
    /// reaped with it (core#489 — a socket path must stay short; the worktree may not be).
    tmp: PrivateTmp,
    /// The SHARED lock on `<run key>.lock`, held for the floor's life: the size cap removes a leaf
    /// only under an exclusive lock, so it never deletes the scratch a floor is building into.
    /// `Option` only so [`Drop`] can release it before retiring an orphaned leaf.
    lease: Option<std::fs::File>,
    /// (DES-FLOOR-V2) The run's log store ([`crate::floor_log::run_dir`]) — outside every write
    /// root the boundary arms.
    pub(crate) logs: PathBuf,
    /// The per-stream byte cap ([`crate::floor_log::MAX_STREAM_BYTES`]).
    pub(crate) log_cap: u64,
}

impl Drop for CheckScratch {
    /// (core#1050) The run's worktree was removed while this floor held the leaf (the reap
    /// marked it orphaned instead of deleting it): the last floor out deletes it, so it is never
    /// left behind. The cap pass removes any orphan this could not take.
    fn drop(&mut self) {
        if orphan_path(&self.root).exists() {
            drop(self.lease.take());
            retire_leaf(&self.root);
        }
    }
}

/// A private, writable temp dir for ONE floor or ONE validator run: `<system temp>/wc-<6 hex>`
/// (mode 0700, drawn fresh, an existing path at the drawn name refused for a redraw), reaped on
/// drop. One newtype for both users (review of #505, D2): the repo-checks floor's `TMPDIR`
/// (`CheckScratch::tmp`) and the deterministic validator's `TMPDIR`, which the bwrap jail binds
/// read-write as an extra root.
#[derive(Debug)]
pub(crate) struct PrivateTmp(PathBuf);

impl PrivateTmp {
    pub(crate) fn create() -> std::io::Result<Self> {
        create_private_tmp(&std::env::temp_dir(), random_tmp_name).map(Self)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PrivateTmp {
    /// `remove_dir_all` does not follow a symlink at the top, and the path is one this process
    /// `mkdir`ed itself. A crash before `Drop` (SIGKILL) leaves one `wc-*` dir for the OS temp
    /// cleaner — LOW, nothing in it is ever reused.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Which tree a check runs on — selects the `CARGO_TARGET_DIR` leaf (`cargo-target/head` vs
/// `cargo-target/base`, core#480): with ONE shared target dir cargo's mtime freshness check could
/// hand a head check the base's test binaries (the contamination the program saw as a false
/// regression), so each tree builds into its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tree {
    Head,
    Base,
}

impl Tree {
    fn target_leaf(self) -> &'static str {
        match self {
            Tree::Head => "head",
            Tree::Base => "base",
        }
    }
}

/// The scratch leaves under the run's check-cache leaf. No `tmp` leaf: the checks' `TMPDIR` lives
/// under the system temp dir (see [`CheckScratch::tmp`]).
const SCRATCH_LEAVES: &[&str] = &[
    "home",
    "npm-cache",
    "cargo-home",
    "cargo-target",
    "cargo-target/head",
    "cargo-target/base",
    "xdg-config",
    "xdg-cache",
];

/// Bounded attempts at an unused random name under the system temp dir before giving up.
const TMP_NAME_ATTEMPTS: usize = 16;

/// The production name draw for the checks' `TMPDIR`: `wc-` + 6 random hex (24 bits; with
/// refuse-existing a collision only costs a redraw). Six, not eight: the macOS per-user temp dir
/// is 49 bytes and the recorded floor check (`env.tmpdir.len() < 60`) leaves room for exactly
/// `wc-` + 6; the worst realistic socket (`mkdtemp` one level + `/x.sock`) then stays under 104.
fn random_tmp_name() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("wc-{}", &id[..6])
}

/// Create the checks' private `TMPDIR` under `base`: draw a name, `mkdir` it (never `_all`; mode
/// 0700 on unix) and REFUSE an existing path — a pre-created directory or symlink at the drawn
/// name (a shared sticky `/tmp` lets any local user plant one) is skipped for a fresh draw, so a
/// foreign entry is neither followed nor able to fail-close the floor. Only the scratch knows the
/// name; nothing else is told.
fn create_private_tmp(base: &Path, mut draw: impl FnMut() -> String) -> std::io::Result<PathBuf> {
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut b = std::fs::DirBuilder::new();
        b.mode(0o700);
        b
    };
    #[cfg(not(unix))]
    let builder = std::fs::DirBuilder::new();
    for _ in 0..TMP_NAME_ATTEMPTS {
        let candidate = base.join(draw());
        match builder.create(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(format!(
        "no unused `wc-*` name under `{}` after {TMP_NAME_ATTEMPTS} draws",
        base.display()
    )))
}

impl CheckScratch {
    /// The scratch for a floor over `worktree`: its leaf of the engine's check cache, the cache
    /// first held to its caps ([`enforce_check_cache_cap`]), a shared lease taken, the leaves made.
    #[cfg(test)]
    pub(crate) fn prepare(worktree: &Path) -> std::io::Result<Self> {
        Self::prepare_for(worktree, None)
    }

    /// [`Self::prepare`] for the run `run_id` (keyed by run and tree, [`scratch_dir_for`]).
    pub(crate) fn prepare_for(worktree: &Path, run_id: Option<&str>) -> std::io::Result<Self> {
        Self::prepare_with(
            &check_cache_root()?,
            worktree,
            run_id,
            CacheCaps::from_env(),
        )
    }

    /// [`Self::prepare_for`] against an explicit cache root and ONE cap, per repository and total
    /// ([`CacheCaps::uniform`]) — the seam most scratch tests use.
    #[cfg(test)]
    pub(crate) fn prepare_in(
        cache: &Path,
        worktree: &Path,
        run_id: Option<&str>,
        cap: u64,
    ) -> std::io::Result<Self> {
        Self::prepare_with(cache, worktree, run_id, CacheCaps::uniform(cap))
    }

    /// [`Self::prepare_for`] against an explicit cache root and caps — the seam the cap tests use.
    pub(crate) fn prepare_with(
        cache: &Path,
        worktree: &Path,
        run_id: Option<&str>,
        caps: CacheCaps,
    ) -> std::io::Result<Self> {
        // (core#1050) The worktree's engine scratch root is still checked FIRST, exactly as when
        // the whole scratch lived under it: the floor writes the run base's export there, and a
        // checkout that ships `tmp` as a SYMLINK (or as a file) refuses the floor, fail-closed,
        // with the same reason it always had — never a logged warning beside a passing check.
        let engine_tmp = worktree.join(crate::worktree_guard::ENGINE_SCRATCH_DIR);
        match std::fs::symlink_metadata(&engine_tmp) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "`{}` is a symlink — the checks' scratch must be a real directory \
                         inside the worktree (refused, never followed)",
                        engine_tmp.display()
                    ),
                ))
            }
            Ok(m) if !m.is_dir() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("`{}` exists and is not a directory", engine_tmp.display()),
                ))
            }
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(std::io::Error::new(
                    e.kind(),
                    format!("`{}` cannot be inspected: {e}", engine_tmp.display()),
                ))
            }
            _ => {}
        }
        let root = scratch_dir_for(cache, worktree, run_id);
        let group = root
            .parent()
            .ok_or_else(|| std::io::Error::other("the check scratch has no repository group"))?;
        wicked_apps_core::spawn::ensure_private_dir(group).map_err(std::io::Error::other)?;
        // An orphaned leaf (its run's worktree was removed while a floor held it) is never
        // reused: retired first, or the floor is refused while another floor still holds it.
        if orphan_path(&root).exists() && !retire_leaf(&root) {
            return Err(std::io::Error::other(format!(
                "the check scratch `{}` was orphaned (its worktree was removed) and is still in \
                 use — it is never reused",
                root.display()
            )));
        }
        // The lease FIRST: a floor starting beside this one never evicts this run's leaf.
        let mut lease = take_orphan_free_lease(&root)?;
        if enforce_check_cache_cap(cache, &root, caps).1 {
            // This run's own scratch alone outgrew the cap: clean it before the checks, unless
            // another floor of the run is building into it (then it is that floor's, for now).
            drop(lease);
            if let Ok(Some(_held)) = lock_leaf(&root, true) {
                match remove_leaf_dir(&root) {
                    Ok(()) => eprintln!(
                        "wicked-core: repo checks floor — this run's check scratch {} alone was \
                         over the {:.1} GB cap ({CHECK_CACHE_CAP_ENV} / \
                         {CHECK_CACHE_TOTAL_CAP_ENV}); removed it before the checks",
                        root.display(),
                        caps.repo.min(caps.total) as f64 / 1e9
                    ),
                    Err(e) => eprintln!(
                        "wicked-core: repo checks floor — could not remove the over-cap check \
                         scratch {}: {e}",
                        root.display()
                    ),
                }
            }
            lease = take_orphan_free_lease(&root)?;
        }
        // The leaf and every scratch directory under it are made through the leaf HELD
        // (`O_DIRECTORY|O_NOFOLLOW`, then component by component, [`HeldDir`]), never by path:
        // an earlier floor's repo-controlled check could write the leaf, and a link it left (or a
        // process it left running swaps in) at the leaf or any directory below must never have
        // the engine create directories through it (adversarial review on #414, LOW; sweep review
        // on #1066).
        // The leaf itself (missing on a run's first floor): its parent is the engine's cache
        // group, outside every write root; a link already at the leaf's path is refused by the
        // hold below, never followed.
        if std::fs::symlink_metadata(&root).is_err() {
            std::fs::create_dir_all(&root)?;
        }
        let held = HeldDir::open(&root).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "`{}` is not a real directory — the checks' scratch is never written \
                     through a link (refused, never followed): {e}",
                    root.display()
                ),
            )
        })?;
        for sub in SCRATCH_LEAVES {
            held.subdir(sub, true).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "the checks' scratch leaves must be real directories (refused, never \
                         followed): {e}"
                    ),
                )
            })?;
        }
        // The checks' `TMPDIR` lives OUTSIDE the worktree, short (core#489) — drawn last, so a
        // refusal above leaves nothing behind under the system temp dir.
        let tmp = PrivateTmp::create()?;
        let logs = crate::floor_log::run_dir(
            &cache
                .parent()
                .unwrap_or(cache)
                .join(crate::floor_log::LOG_SUBDIR),
            run_id,
            worktree,
        );
        Ok(Self {
            root,
            tmp,
            lease: Some(lease),
            logs,
            log_cap: log_cap(),
        })
    }

    /// The scratch's leaf directory — the boundary's third write root.
    /// The checks' private `TMPDIR`.
    pub(crate) fn tmp_path(&self) -> &Path {
        self.tmp.path()
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// Apply the MINIMAL environment to a check command: the daemon's environment is cleared
    /// (`hardened()` already stripped the engine-internal variables; this drops everything else —
    /// tokens, API keys, `WICKED_*`), only [`CHECK_ENV_PASSTHROUGH`] is copied from the daemon, and
    /// every home-shaped variable is set under the scratch, so the check reads none of the
    /// operator's per-user configuration or credentials and writes nothing outside the worktree
    /// but its private `TMPDIR`. `tree` picks the `CARGO_TARGET_DIR` leaf (core#480).
    fn apply_env(&self, cmd: &mut Command, tree: Tree, root: &Path) {
        let real_home = std::env::var_os("HOME");
        cmd.env_clear();
        for key in CHECK_ENV_PASSTHROUGH {
            if let Some(val) = std::env::var_os(key) {
                cmd.env(key, val);
            }
        }
        // `LC_*` as a family (LC_CTYPE, LC_MESSAGES, …): locale, never a secret.
        for (key, val) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("LC_") {
                cmd.env(key, val);
            }
        }
        cmd.env("HOME", self.home())
            .env("TMPDIR", self.tmp.path())
            .env("TMP", self.tmp.path())
            .env("TEMP", self.tmp.path())
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("XDG_CACHE_HOME", self.root.join("xdg-cache"))
            .env("npm_config_cache", self.root.join("npm-cache"))
            .env("npm_config_update_notifier", "false")
            .env("npm_config_fund", "false")
            .env("npm_config_audit", "false")
            .env("CARGO_HOME", self.root.join("cargo-home"))
            // Build artifacts go under the scratch, not `./target`: a repo that does not ignore
            // `target/` would otherwise fail the worktree guard's final comparison on a PASSING
            // `cargo test` (Copilot on #414). One leaf PER TREE (core#480): the base run's
            // binaries must never satisfy a head check's freshness check, or the reverse.
            .env(
                "CARGO_TARGET_DIR",
                self.root.join("cargo-target").join(tree.target_leaf()),
            )
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("FORCE_COLOR", "0")
            // (core#767) A Python journey must not leave `__pycache__/` in the reviewed tree (the
            // worktree guard would deny the engine's own side effect).
            .env("PYTHONDONTWRITEBYTECODE", "1");
        // The base export is its OWN git repository ([`BaseTree::export`]): a git call a base
        // check makes (`git grep`, `git rev-parse`) discovers `<base>/.git` first, and the
        // export's parent is a discovery ceiling, so it never walks up into the head worktree,
        // whose index and untracked `tmp/` it must not reach. Not `GIT_DIR`/`GIT_WORK_TREE`: every
        // descendant inherits those, and a check's own `git init <fixture>` / `git -C <fixture>`
        // would then act on the base repository instead of its fixture.
        if tree == Tree::Base {
            if let Some(parent) = root.parent() {
                cmd.env("GIT_CEILING_DIRECTORIES", parent);
            }
        }
        // (core#767) Playwright resolves its browsers under the user cache, which moved with
        // HOME: pin the operator's installed browsers (read-only to the check — writes stay
        // confined by the boundary) when the daemon set no location of its own.
        match std::env::var_os("PLAYWRIGHT_BROWSERS_PATH") {
            Some(v) => {
                cmd.env("PLAYWRIGHT_BROWSERS_PATH", v);
            }
            None => {
                if let Some(h) = real_home.as_deref() {
                    let cache = if cfg!(target_os = "macos") {
                        Path::new(h).join("Library/Caches/ms-playwright")
                    } else {
                        Path::new(h).join(".cache/ms-playwright")
                    };
                    if cache.is_dir() {
                        cmd.env("PLAYWRIGHT_BROWSERS_PATH", cache);
                    }
                }
            }
        }
        // The toolchain proxies (`cargo`, `rustc` under rustup) resolve toolchains through
        // `RUSTUP_HOME`, which defaults to `$HOME/.rustup`. Moving HOME must not lose them: pin the
        // real location explicitly when the daemon did not (when it did, the allow-list passed it).
        if std::env::var_os("RUSTUP_HOME").is_none() {
            if let Some(h) = real_home {
                let rustup = Path::new(&h).join(".rustup");
                if rustup.is_dir() {
                    cmd.env("RUSTUP_HOME", rustup);
                }
            }
        }
    }

    /// (F-RC2-009) The environment record for the verdict payload: the isolation overrides, the
    /// locale as passed through (never a secret), the search path, and the NAMES of the daemon
    /// variables the allow-list let through — so a `floor_env_mismatch` can be read against the
    /// environment CI runs the same check in.
    fn env_record(&self, sandbox_level: &str) -> FloorEnv {
        let mut locale: Vec<String> = std::env::vars_os()
            .filter_map(|(k, v)| {
                let k = k.to_string_lossy().into_owned();
                (k == "LANG" || k.starts_with("LC_"))
                    .then(|| format!("{k}={}", v.to_string_lossy()))
            })
            .collect();
        locale.sort();
        let passthrough: Vec<String> = CHECK_ENV_PASSTHROUGH
            .iter()
            .filter(|k| std::env::var_os(k).is_some())
            .map(|k| k.to_string())
            .collect();
        FloorEnv {
            home: self.home().to_string_lossy().into_owned(),
            tmpdir: self.tmp.path().to_string_lossy().into_owned(),
            locale,
            network: "open".to_string(),
            sandbox_level: sandbox_level.to_string(),
            path: std::env::var("PATH").unwrap_or_default(),
            passthrough,
        }
    }
}

/// The engine's check cache: `<worker home>/repo-checks` ([`CHECK_CACHE_SUBDIR`]). In this crate's
/// own tests it is the test binary's hermetic worker home, resolved once — never the
/// operator's, and immune to a test re-aiming `WICKED_WORKER_HOME` mid-run.
pub(crate) fn check_cache_root() -> std::io::Result<PathBuf> {
    #[cfg(test)]
    let base: anyhow::Result<PathBuf> = Ok(wicked_apps_core::spawn::hermetic_test_worker_home());
    #[cfg(not(test))]
    let base = wicked_apps_core::spawn::worker_home_base();
    base.map(|b| b.join(CHECK_CACHE_SUBDIR))
        .map_err(|e| std::io::Error::other(format!("no worker home for the check cache: {e}")))
}

/// [`CHECK_CACHE_CAP_ENV`] in bytes, [`DEFAULT_CHECK_CACHE_CAP_GB`] when unset or unparsable.
fn check_cache_cap_bytes() -> u64 {
    gb_env(CHECK_CACHE_CAP_ENV, DEFAULT_CHECK_CACHE_CAP_GB)
}

/// A whole-GB cap from `var` in bytes, `default_gb` when unset or unparsable.
fn gb_env(var: &str, default_gb: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default_gb)
        .saturating_mul(1_000_000_000)
}

/// 12 hex of `sha256(bytes)`.
pub(crate) fn hex12(bytes: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// `<label>-<12 hex of sha256(path)>`: readable, and injective in the path (two worktrees that
/// share a name never share a leaf).
fn cache_key(label: &std::ffi::OsStr, path: &Path) -> String {
    let label: String = label
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    format!("{label}-{}", hex12(path.as_os_str().as_encoded_bytes()))
}

/// The check-scratch leaf of run `run_id` over the tree at `worktree`:
/// `<cache>/<repo key>/<run key>`. The repo key is the directory the worktree sits in —
/// `<repo>/wicked-worktrees` for every run of one registered repository, so a repository's runs
/// are one group the cap measures — labelled with the repository's own name. The run key is the
/// worktree AND the exact run id (`<tree key>-r<12 hex of sha256(run id)>`, core#1050): two runs
/// whose ids sanitize to one worktree path still get two leaves. Spelled through the canonical
/// PARENT (which outlives the worktree), so the reaper derives the same leaf after the tree is
/// gone.
pub(crate) fn scratch_dir_for(cache: &Path, worktree: &Path, run_id: Option<&str>) -> PathBuf {
    let (group, prefix) = tree_scratch_prefix(cache, worktree);
    group.join(format!(
        "{prefix}{}",
        hex12(run_id.unwrap_or_default().as_bytes())
    ))
}

/// The repository group of the tree at `worktree` and the name prefix every run leaf of that tree
/// carries (`<tree key>-r`).
fn tree_scratch_prefix(cache: &Path, worktree: &Path) -> (PathBuf, String) {
    let name = worktree.file_name().unwrap_or(worktree.as_os_str());
    let parent = worktree
        .parent()
        .map(|p| crate::worker_sandbox::canon_lenient(p).unwrap_or_else(|| p.to_path_buf()))
        .unwrap_or_default();
    let tree = parent.join(name);
    // The repository's name: the first ancestor that is not the worktrees layout itself
    // (`wicked-worktrees`, the pre-crew#276 `.wicked/worktrees`).
    let label = parent
        .ancestors()
        .filter_map(Path::file_name)
        .find(|n| {
            !matches!(
                n.to_str(),
                Some("wicked-worktrees" | "worktrees" | ".wicked")
            )
        })
        .unwrap_or(std::ffi::OsStr::new("root"));
    (
        cache.join(cache_key(label, &parent)),
        format!("{}-r", cache_key(name, &tree)),
    )
}

/// Open `<leaf>.lock` and lock it — SHARED for a floor, EXCLUSIVE (non-blocking) for a removal.
/// `Ok(None)` only for an exclusive lock some floor holds.
fn lock_leaf(leaf: &Path, exclusive: bool) -> std::io::Result<Option<std::fs::File>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lease_path(leaf))?;
    if exclusive {
        // A few short retries: a child another thread is forking holds a copy of a floor's
        // descriptor (and so its `flock`) until its `exec` closes it, so a lease just released can
        // read as held for a moment. A floor that really holds the lease holds it for minutes.
        for attempt in 0..LEASE_ATTEMPTS {
            match file.try_lock() {
                Ok(()) => return Ok(Some(file)),
                Err(std::fs::TryLockError::WouldBlock) if attempt + 1 < LEASE_ATTEMPTS => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
                Err(std::fs::TryLockError::Error(e)) => return Err(e),
            }
        }
        return Ok(None);
    }
    file.lock_shared()?;
    // The lease's mtime is the leaf's last use: the cap evicts the least recently used first.
    let _ = file.set_modified(std::time::SystemTime::now());
    Ok(Some(file))
}

/// How many times a removal tries the exclusive lock on a leaf before leaving it (see
/// [`lock_leaf`]).
const LEASE_ATTEMPTS: u32 = 8;

/// `<leaf>.lock` — a sibling of the leaf, so removing the leaf never removes the lock a waiting
/// floor holds a descriptor to.
fn lease_path(leaf: &Path) -> PathBuf {
    let mut path = leaf.as_os_str().to_owned();
    path.push(".lock");
    PathBuf::from(path)
}

/// A floor's shared lease on `leaf` ([`CheckScratch::lease`]).
fn take_lease(leaf: &Path) -> std::io::Result<std::fs::File> {
    lock_leaf(leaf, false)?.ok_or_else(|| std::io::Error::other("the lease could not be taken"))
}

/// [`take_lease`], then the orphan mark checked AGAIN under the lease (codex on #1050): a reap can
/// mark the leaf between a floor's first look and its lease, and an orphaned leaf is never
/// reused. A marked leaf is released and retired, and the lease taken on the fresh one; while
/// another floor still holds the orphan, the floor is refused.
fn take_orphan_free_lease(leaf: &Path) -> std::io::Result<std::fs::File> {
    for _ in 0..3 {
        let lease = take_lease(leaf)?;
        if !orphan_path(leaf).exists() {
            return Ok(lease);
        }
        drop(lease);
        if !retire_leaf(leaf) {
            break;
        }
    }
    Err(std::io::Error::other(format!(
        "the check scratch `{}` was orphaned (its worktree was removed) and is still in use — it \
         is never reused",
        leaf.display()
    )))
}

/// The apparent size of everything under `dir`, never following a link.
fn tree_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(m) = e.path().symlink_metadata() else {
                continue;
            };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total = total.saturating_add(m.len());
            }
        }
    }
    total
}

/// Remove a leaf directory (a link at its path is removed, never followed).
fn remove_leaf_dir(leaf: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(leaf) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(leaf),
        Ok(_) => std::fs::remove_file(leaf),
    }
}

/// The cap pass's lock, `<cache>/.cap.lock`: one pass over the whole check cache at a time, so two
/// floors starting together never scan and evict against each other (one would read a leaf the
/// other is mid-way through deleting as "in use" and evict more than the cap needs). One lock for
/// the whole cache, not one per repository (core#1122): the total cap evicts across repositories.
const CAP_PASS_LOCK: &str = ".cap.lock";

/// The check cache's limits (core#1122): the per-repository cap ([`CHECK_CACHE_CAP_ENV`]), the
/// total cap over every repository ([`CHECK_CACHE_TOTAL_CAP_ENV`]), and the idle retention
/// ([`CHECK_CACHE_IDLE_TTL`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CacheCaps {
    pub(crate) repo: u64,
    pub(crate) total: u64,
    pub(crate) idle: Duration,
    /// (core#1138) The free-disk floor the total is also held to ([`effective_total_cap`]).
    pub(crate) free_floor: FreeFloor,
    /// Where the floor's free-space reading comes from.
    pub(crate) disk: DiskProbe,
}

impl CacheCaps {
    /// The caps the daemon's environment sets, the defaults where it sets none.
    pub(crate) fn from_env() -> Self {
        Self {
            repo: check_cache_cap_bytes(),
            total: gb_env(CHECK_CACHE_TOTAL_CAP_ENV, DEFAULT_CHECK_CACHE_TOTAL_CAP_GB),
            idle: CHECK_CACHE_IDLE_TTL,
            free_floor: FreeFloor::from_env(),
            #[cfg(not(test))]
            disk: DiskProbe::Volume,
            // The test host's own free disk never decides a fixture's leaves ([`DiskProbe::Off`]).
            #[cfg(test)]
            disk: DiskProbe::Off,
        }
    }

    /// One byte cap as both the repository and the total cap, the default retention, no
    /// free-disk floor.
    #[cfg(test)]
    pub(crate) fn uniform(cap: u64) -> Self {
        Self {
            repo: cap,
            total: cap,
            idle: CHECK_CACHE_IDLE_TTL,
            free_floor: FreeFloor::OFF,
            disk: DiskProbe::Off,
        }
    }
}

/// (core#1138) The free-disk floor on the check cache's volume: the larger of `pct` percent of the
/// volume and `min` bytes ([`CHECK_CACHE_FREE_FLOOR_PCT_ENV`], [`CHECK_CACHE_FREE_FLOOR_GB_ENV`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FreeFloor {
    pub(crate) pct: u64,
    pub(crate) min: u64,
}

impl FreeFloor {
    /// No floor (both settings `0`): the fixed caps alone decide.
    #[cfg(test)]
    pub(crate) const OFF: Self = Self { pct: 0, min: 0 };

    /// The floor the daemon's environment sets, the defaults where it sets none.
    fn from_env() -> Self {
        let pct = std::env::var(CHECK_CACHE_FREE_FLOOR_PCT_ENV).ok();
        let gb = std::env::var(CHECK_CACHE_FREE_FLOOR_GB_ENV).ok();
        Self::parse(pct.as_deref(), gb.as_deref())
    }

    /// The floor from the two settings' raw values (`None` = unset).
    fn parse(pct: Option<&str>, gb: Option<&str>) -> Self {
        let whole = |v: Option<&str>| v.and_then(|v| v.trim().parse::<u64>().ok());
        Self {
            pct: whole(pct)
                .unwrap_or(DEFAULT_CHECK_CACHE_FREE_FLOOR_PCT)
                .min(100),
            min: whole(gb)
                .unwrap_or(DEFAULT_CHECK_CACHE_FREE_FLOOR_GB)
                .saturating_mul(1_000_000_000),
        }
    }

    /// The bytes to keep free on a volume of `volume` bytes.
    pub(crate) fn bytes(self, volume: u64) -> u64 {
        let share = (u128::from(volume) * u128::from(self.pct) / 100) as u64;
        share.max(self.min)
    }
}

/// A volume's size and the bytes free on it for an unprivileged process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VolumeSpace {
    pub(crate) total: u64,
    pub(crate) free: u64,
}

/// Where the cap pass reads the cache volume's free space from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiskProbe {
    /// `statvfs` on the cache root (unix); no reading elsewhere, so no floor. (A test build off
    /// unix never constructs it: its only constructions are the daemon's caps and a unix test.)
    #[cfg_attr(all(test, not(unix)), allow(dead_code))]
    Volume,
    /// No reading, so no floor: this crate's own tests, whose fixtures the HOST's free disk (a
    /// nearly full CI runner) must never decide.
    #[cfg(test)]
    Off,
    /// The floor tests' simulated volume: `space` at the first reading, then a removal of N
    /// apparent bytes gives back `reclaim_pct` percent of them (under 100: hard-linked outputs).
    #[cfg(test)]
    Simulated {
        space: VolumeSpace,
        reclaim_pct: u64,
    },
}

impl DiskProbe {
    /// The free bytes on `cache`'s volume after the cap pass removed `freed` apparent bytes since
    /// the `first` reading: a FRESH reading, since a removal can give back less than its apparent
    /// bytes (hard links, sparse or still-open files); the credited estimate only when the volume
    /// cannot be read again.
    fn free_after(self, cache: &Path, first: VolumeSpace, freed: u64) -> u64 {
        let credited = first.free.saturating_add(freed);
        match self {
            Self::Volume => volume_space(cache).map_or(credited, |s| s.free),
            #[cfg(test)]
            Self::Off => credited,
            #[cfg(test)]
            Self::Simulated { reclaim_pct, .. } => first
                .free
                .saturating_add((u128::from(freed) * u128::from(reclaim_pct) / 100) as u64),
        }
    }

    /// The space on `cache`'s volume, `None` when it cannot be read (the floor is then not held).
    fn space(self, cache: &Path) -> Option<VolumeSpace> {
        match self {
            Self::Volume => volume_space(cache),
            #[cfg(test)]
            Self::Off => None,
            #[cfg(test)]
            Self::Simulated { space, .. } => Some(space),
        }
    }
}

/// `statvfs(at)`: the volume's size and its free bytes an unprivileged process may use.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
fn volume_space(at: &Path) -> Option<VolumeSpace> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(at.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` only writes the zeroed struct it is handed; `c` is NUL-terminated.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let unit = if st.f_frsize as u64 > 0 {
        st.f_frsize as u64
    } else {
        st.f_bsize as u64
    };
    Some(VolumeSpace {
        total: (st.f_blocks as u64).saturating_mul(unit),
        free: (st.f_bavail as u64).saturating_mul(unit),
    })
}

/// No free-space reading off unix: the fixed caps alone decide.
#[cfg(not(unix))]
fn volume_space(_at: &Path) -> Option<VolumeSpace> {
    None
}

/// (core#1138) The total the check cache may hold: the smaller of the fixed total cap and what
/// keeps the free-disk floor free, for a cache of `cache_bytes` on a volume with `space`. The
/// cache's own bytes count as reclaimable, so a cache on a volume already below its floor gets a
/// cap below its size (evict `floor - free` bytes). `space` unknown: the fixed cap.
pub(crate) fn effective_total_cap(
    caps: CacheCaps,
    cache_bytes: u64,
    space: Option<VolumeSpace>,
) -> u64 {
    match space {
        Some(s) => caps.total.min(
            cache_bytes
                .saturating_add(s.free)
                .saturating_sub(caps.free_floor.bytes(s.total)),
        ),
        None => caps.total,
    }
}

/// One run leaf the cap pass may remove.
struct CapCandidate {
    used: std::time::SystemTime,
    leaf: PathBuf,
    group: PathBuf,
    bytes: u64,
}

/// What the cap pass's removal attempt on one leaf came to ([`evict_leaf`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Evicted {
    /// This pass removed it.
    Removed,
    /// Gone since the scan (a run's terminal reap): nothing removed, nothing to log.
    AlreadyGone,
    /// A floor holds its lease (never taken), or the removal failed (logged).
    Kept,
}

/// Remove `leaf` for the cap pass, only under its EXCLUSIVE lock. A floor holds a SHARED lock for
/// its life, so a leaf a floor is building into is never removed.
fn evict_leaf(leaf: &Path) -> Evicted {
    match lock_leaf(leaf, true) {
        Ok(Some(_held)) if std::fs::symlink_metadata(leaf).is_err() => Evicted::AlreadyGone,
        Ok(Some(_held)) => match remove_leaf_dir(leaf) {
            Ok(()) => Evicted::Removed,
            Err(e) => {
                eprintln!(
                    "wicked-core: repo checks floor — could not remove the check scratch {}: {e}",
                    leaf.display()
                );
                Evicted::Kept
            }
        },
        // A floor is building into it right now: it is not this cap's to take.
        Ok(None) => Evicted::Kept,
        Err(e) => {
            eprintln!(
                "wicked-core: repo checks floor — could not lock the check scratch {}: {e}",
                leaf.display()
            );
            Evicted::Kept
        }
    }
}

/// Hold the check cache at `cache` (`<worker home>/repo-checks`) to `caps` before a floor runs over
/// `own` (`<cache>/<repo group>/<run leaf>`; the floor already holds `own`'s lease). Only run
/// leaves (real directories two levels under `cache`, never a link) are ever considered.
///
/// 1. Orphaned leaves ([`orphan_path`]) no floor holds go first, whatever the caps.
/// 2. (core#1122) A leaf no floor has used for [`CacheCaps::idle`] goes, whatever the caps.
/// 3. Over the per-repository cap (that repository's leaves) or the TOTAL cap (every
///    repository's), the OTHER run leaves go least recently used first until both fit.
///    (core#1138) While a reading of the cache's volume is below its free-disk floor
///    ([`CacheCaps::free_floor`]), leaves go the same way, below the fixed cap
///    ([`effective_total_cap`]), until a fresh reading after a removal shows the floor holds
///    again, or only leaves floors hold are left. That shortfall is logged
///    ([`floor_shortfall`]) and reported by [`floor_cache_usage`].
///
/// A leaf a floor holds a lease on is never removed: removal takes the leaf's EXCLUSIVE lock, and
/// every floor holds a SHARED one for its life. The floor's own leaf is never removed here; the
/// second value says whether it ALONE is over a FIXED cap (one run outgrew it), which
/// [`CheckScratch::prepare_in`] then cleans. The free-disk floor never cleans the floor's own leaf:
/// the floor about to run holds its lease, and would rebuild the same bytes cold. Every removal is
/// logged; best-effort, never fails the floor.
pub(crate) fn enforce_check_cache_cap(
    cache: &Path,
    own: &Path,
    caps: CacheCaps,
) -> (Vec<PathBuf>, bool) {
    let mut removed = Vec::new();
    let pass = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cache.join(CAP_PASS_LOCK));
    let Ok(_pass) = pass.and_then(|f| f.lock().map(|()| f)) else {
        return (removed, false);
    };
    let Ok(groups) = std::fs::read_dir(cache) else {
        return (removed, false);
    };
    let own_group = own.parent().unwrap_or(cache).to_path_buf();
    let now = std::time::SystemTime::now();
    let gb = |b: u64| b as f64 / 1e9;
    let mut own_bytes = 0u64;
    let mut others: Vec<CapCandidate> = Vec::new();
    let mut held_orphans: Vec<(PathBuf, u64)> = Vec::new();
    let dirs = |d: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            // `DirEntry::file_type` never follows a link: a link is never a group or a leaf.
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.path())
            .collect()
    };
    for group in groups
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
    {
        for leaf in dirs(&group) {
            // (core#1050) An orphaned leaf goes whatever the cap, once no floor holds it.
            if leaf != own && orphan_path(&leaf).exists() {
                if retire_leaf(&leaf) {
                    eprintln!(
                        "wicked-core: repo checks floor — removed the orphaned check scratch {} \
                         (its run's worktree is gone)",
                        leaf.display()
                    );
                    removed.push(leaf);
                } else {
                    // (codex on #1124) Still held by a floor: not removable now, but its bytes
                    // are on disk and count against both caps.
                    held_orphans.push((group.clone(), tree_bytes(&leaf)));
                }
                continue;
            }
            let bytes = tree_bytes(&leaf);
            if leaf == own {
                own_bytes = bytes;
                continue;
            }
            let used = std::fs::metadata(lease_path(&leaf))
                .or_else(|_| leaf.symlink_metadata())
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            // (core#1122) The cache's own retention: unused this long, it goes.
            let idle = now.duration_since(used).unwrap_or_default();
            if idle > caps.idle && evict_leaf(&leaf) == Evicted::Removed {
                eprintln!(
                    "wicked-core: repo checks floor — removed the check scratch {} ({:.1} GB), \
                     unused for {} day(s)",
                    leaf.display(),
                    gb(bytes),
                    idle.as_secs() / 86_400
                );
                removed.push(leaf);
                continue;
            }
            others.push(CapCandidate {
                used,
                leaf,
                group: group.clone(),
                bytes,
            });
        }
    }
    let mut total: u64 = own_bytes
        + others.iter().map(|c| c.bytes).sum::<u64>()
        + held_orphans.iter().map(|o| o.1).sum::<u64>();
    let mut per_group: std::collections::HashMap<PathBuf, u64> = std::collections::HashMap::new();
    *per_group.entry(own_group.clone()).or_default() += own_bytes;
    for c in &others {
        *per_group.entry(c.group.clone()).or_default() += c.bytes;
    }
    for (g, bytes) in &held_orphans {
        *per_group.entry(g.clone()).or_default() += bytes;
    }
    let own_over = own_bytes > caps.repo.min(caps.total);
    // (core#1138) Read AFTER the scan's own removals, so the free space already counts them. The
    // floor is held on READINGS of the volume, never on the leaves' apparent bytes (codex on
    // #1140): cargo hard-links its outputs, a file may be sparse or still open, so a leaf can give
    // back less than it measures. Re-read after every removal while the volume is short.
    let space = caps.disk.space(cache);
    let floor = space.map(|s| caps.free_floor.bytes(s.total));
    let mut free = space.map(|s| s.free);
    let mut freed = 0u64;
    let short = |free: Option<u64>| matches!((floor, free), (Some(f), Some(fr)) if fr < f);
    if !short(free) && total <= caps.total && per_group.values().all(|&b| b <= caps.repo) {
        return (removed, own_over);
    }
    others.sort_by_key(|c| c.used);
    for c in others {
        let in_group = per_group.get(&c.group).copied().unwrap_or_default();
        let floor_short = short(free);
        let (over_total, over_repo) = (total > caps.total, in_group > caps.repo);
        if !floor_short && !over_total && !over_repo {
            continue;
        }
        match evict_leaf(&c.leaf) {
            Evicted::Removed => {}
            Evicted::AlreadyGone => {
                total = total.saturating_sub(c.bytes);
                if let Some(g) = per_group.get_mut(&c.group) {
                    *g = g.saturating_sub(c.bytes);
                }
                continue;
            }
            Evicted::Kept => continue,
        }
        let why = if floor_short {
            format!(
                "free disk on the check cache's volume was {:.1} GB, under its {:.1} GB floor \
                 ({CHECK_CACHE_FREE_FLOOR_PCT_ENV} / {CHECK_CACHE_FREE_FLOOR_GB_ENV}), with the \
                 check cache {} at {:.1} GB",
                gb(free.unwrap_or_default()),
                gb(floor.unwrap_or_default()),
                cache.display(),
                gb(total)
            )
        } else if over_total {
            format!(
                "the check cache {} was {:.1} GB, over its {:.1} GB total cap \
                 ({CHECK_CACHE_TOTAL_CAP_ENV})",
                cache.display(),
                gb(total),
                gb(caps.total)
            )
        } else {
            format!(
                "the check cache {} was {:.1} GB, over its {:.1} GB cap ({CHECK_CACHE_CAP_ENV})",
                c.group.display(),
                gb(in_group),
                gb(caps.repo)
            )
        };
        eprintln!(
            "wicked-core: repo checks floor — {why}; removed {} ({:.1} GB)",
            c.leaf.display(),
            gb(c.bytes)
        );
        total = total.saturating_sub(c.bytes);
        if let Some(g) = per_group.get_mut(&c.group) {
            *g = g.saturating_sub(c.bytes);
        }
        freed = freed.saturating_add(c.bytes);
        if let Some(first) = space.filter(|_| floor_short) {
            free = Some(caps.disk.free_after(cache, first, freed));
        }
        removed.push(c.leaf);
    }
    let own_group_bytes = per_group.get(&own_group).copied().unwrap_or_default();
    if let Some(why) = floor_shortfall(cache, floor, free, total) {
        // Every leaf no floor holds is gone and the volume is still short (codex on #1140: also
        // when the cache is empty, or was before the pass).
        eprintln!("wicked-core: repo checks floor — {why}");
    } else if !own_over && (total > caps.total || own_group_bytes > caps.repo) {
        eprintln!(
            "wicked-core: repo checks floor — the check cache {} is {:.1} GB (this repository's \
             {:.1} GB), over its caps ({:.1} GB total, {CHECK_CACHE_TOTAL_CAP_ENV}; {:.1} GB per \
             repository, {CHECK_CACHE_CAP_ENV}), held by runs whose floors are running",
            cache.display(),
            gb(total),
            gb(own_group_bytes),
            gb(caps.total),
            gb(caps.repo)
        );
    }
    (removed, own_over)
}

/// (core#1138) The cap pass's closing line when the volume is still below its free-disk floor:
/// `floor` and the latest `free` reading (`None` when unread: no floor), and the `total` bytes the
/// check cache at `cache` still holds. `None` when the floor holds. An empty cache is never blamed
/// on running floors.
fn floor_shortfall(
    cache: &Path,
    floor: Option<u64>,
    free: Option<u64>,
    total: u64,
) -> Option<String> {
    let (floor, free) = (floor?, free?);
    if free >= floor {
        return None;
    }
    let gb = |b: u64| b as f64 / 1e9;
    let left = if total == 0 {
        "holds nothing more to remove".to_string()
    } else {
        format!(
            "is {:.1} GB, held by runs whose floors are running",
            gb(total)
        )
    };
    Some(format!(
        "free disk on the check cache's volume is {:.1} GB, {:.1} GB short of its {:.1} GB floor \
         ({CHECK_CACHE_FREE_FLOOR_PCT_ENV} / {CHECK_CACHE_FREE_FLOOR_GB_ENV}); the check cache {} \
         {left}",
        gb(free),
        gb(floor - free),
        gb(floor),
        cache.display()
    ))
}

/// (core#1122) What the repo-checks floor holds on disk, for an operator's disk view (crew's
/// diagnostics, the Nerve Center's disk row): the build cache at `<worker home>/repo-checks` (every
/// run leaf, by repository, largest first) with its caps, and the floor log store beside it. Sizes
/// are apparent bytes, measured now (a walk of the tree: call it on demand, not on a hot poll).
/// Read-only. `{ path, bytes, totalCapBytes, repoCapBytes, idleTtlSecs, freeDisk, repos: [{ name,
/// bytes, leaves }], logs: { path, bytes, capBytes } }`; `error` instead when there is no worker
/// home. (core#1138) `freeDisk` is the cache volume's free-disk floor: `{ floorPct, floorMinBytes,
/// volumeBytes, freeBytes, floorBytes, effectiveCapBytes, shortfallBytes, note }`, the measured
/// fields `null` where the volume cannot be read. `shortfallBytes` (> 0) is how far free disk is
/// below the floor; then `note` names the floor cache and its size, for crew's disk row.
pub fn floor_cache_usage() -> serde_json::Value {
    match check_cache_root() {
        Ok(cache) => floor_cache_usage_in(&cache, CacheCaps::from_env()),
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    }
}

/// [`floor_cache_usage`] against an explicit cache root and caps.
pub(crate) fn floor_cache_usage_in(cache: &Path, caps: CacheCaps) -> serde_json::Value {
    let mut repos: Vec<(String, u64, usize)> = std::fs::read_dir(cache)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|g| {
            let leaves = std::fs::read_dir(g.path())
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .count();
            (
                g.file_name().to_string_lossy().into_owned(),
                tree_bytes(&g.path()),
                leaves,
            )
        })
        .collect();
    repos.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let logs = cache
        .parent()
        .unwrap_or(cache)
        .join(crate::floor_log::LOG_SUBDIR);
    let bytes = repos.iter().map(|r| r.1).sum::<u64>();
    let space = caps.disk.space(cache);
    let floor = space.map(|s| caps.free_floor.bytes(s.total));
    let shortfall = space.zip(floor).map(|(s, f)| f.saturating_sub(s.free));
    let note = space.zip(floor).filter(|(s, f)| s.free < *f).map(|(s, f)| {
        format!(
            "free disk on the floor cache's volume is {:.1} GB, {:.1} GB short of its {:.1} GB \
             floor ({CHECK_CACHE_FREE_FLOOR_PCT_ENV} / {CHECK_CACHE_FREE_FLOOR_GB_ENV}); the floor \
             cache {} holds {:.1} GB, and its next cap pass keeps only leaves running floors hold",
            s.free as f64 / 1e9,
            (f - s.free) as f64 / 1e9,
            f as f64 / 1e9,
            cache.display(),
            bytes as f64 / 1e9
        )
    });
    serde_json::json!({
        "path": cache.to_string_lossy(),
        "bytes": bytes,
        "totalCapBytes": caps.total,
        "repoCapBytes": caps.repo,
        "idleTtlSecs": caps.idle.as_secs(),
        "freeDisk": {
            "floorPct": caps.free_floor.pct,
            "floorMinBytes": caps.free_floor.min,
            "volumeBytes": space.map(|s| s.total),
            "freeBytes": space.map(|s| s.free),
            "floorBytes": floor,
            "effectiveCapBytes": effective_total_cap(caps, bytes, space),
            "shortfallBytes": shortfall,
            "note": note,
        },
        "repos": repos
            .iter()
            .map(|(name, bytes, leaves)| serde_json::json!({
                "name": name, "bytes": bytes, "leaves": leaves
            }))
            .collect::<Vec<_>>(),
        "logs": {
            "path": logs.to_string_lossy(),
            "bytes": tree_bytes(&logs),
            "capBytes": crate::floor_log::log_cap_bytes(),
        },
    })
}

/// `<leaf>.orphan`: the leaf's run lost its worktree while a floor held the leaf. It is never
/// reused; the floor holding it ([`CheckScratch`]'s `Drop`), the next prepare of that run, or the
/// next cap pass deletes it.
fn orphan_path(leaf: &Path) -> PathBuf {
    let mut path = leaf.as_os_str().to_owned();
    path.push(".orphan");
    PathBuf::from(path)
}

/// Delete `leaf` under its exclusive lock, and its orphan mark. When a floor still holds the
/// lease, the leaf is MARKED orphaned instead (and logged) and `false` returned. The 0-byte lock
/// file is KEPT, as the cap keeps it: a floor may already have it open, waiting in
/// `lock_shared`, and unlinking it would hand that floor a lease on an inode no later lock sees.
fn retire_leaf(leaf: &Path) -> bool {
    let mark = orphan_path(leaf);
    if std::fs::symlink_metadata(lease_path(leaf)).is_err()
        && std::fs::symlink_metadata(leaf).is_err()
    {
        let _ = std::fs::remove_file(&mark);
        return true;
    }
    match lock_leaf(leaf, true) {
        Ok(Some(_held)) => match remove_leaf_dir(leaf) {
            Ok(()) => {
                let _ = std::fs::remove_file(&mark);
                true
            }
            Err(e) => {
                eprintln!(
                    "wicked-core: could not remove the check scratch {}: {e}",
                    leaf.display()
                );
                let _ = std::fs::write(&mark, b"");
                false
            }
        },
        Ok(None) => {
            let _ = std::fs::write(&mark, b"");
            eprintln!(
                "wicked-core: the check scratch {} is in use by a floor; marked orphaned (never \
                 reused, removed when that floor ends or by the next cap pass)",
                leaf.display()
            );
            false
        }
        Err(e) => {
            let _ = std::fs::write(&mark, b"");
            eprintln!(
                "wicked-core: could not lock the check scratch {}: {e}; marked orphaned",
                leaf.display()
            );
            false
        }
    }
}

/// Remove the check-scratch leaf of run `run_id` over the tree at `worktree` — the run ended or
/// its worktree is being removed. Never touches the worktree.
pub(crate) fn remove_check_scratch(worktree: &Path, run_id: &str) {
    let Ok(cache) = check_cache_root() else {
        return;
    };
    remove_check_scratch_in(&cache, worktree, Some(run_id));
}

/// [`remove_check_scratch`] against an explicit cache root. Returns whether the leaf is gone.
pub(crate) fn remove_check_scratch_in(cache: &Path, worktree: &Path, run_id: Option<&str>) -> bool {
    retire_leaf(&scratch_dir_for(cache, worktree, run_id))
}

/// Remove EVERY run's check-scratch leaf of the tree at `dir` — a guarded floor checkout, whose
/// path is unique to it (a nonce), dropped.
pub(crate) fn remove_tree_scratch(dir: &Path) {
    let Ok(cache) = check_cache_root() else {
        return;
    };
    remove_tree_scratch_in(&cache, dir);
}

/// [`remove_tree_scratch`] against an explicit cache root.
pub(crate) fn remove_tree_scratch_in(cache: &Path, dir: &Path) {
    let (group, prefix) = tree_scratch_prefix(cache, dir);
    let Ok(entries) = std::fs::read_dir(&group) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && e.file_type().is_ok_and(|t| t.is_dir()) {
            retire_leaf(&e.path());
        }
    }
}

/// [`run_floor`] as the historical verify floor — no known base (no baseline diff, no
/// `{files}`/`{base}`), no claim. Test-only since core#467: every production caller names its
/// [`FloorContext`].
#[cfg(test)]
pub(crate) fn run(worktree: &Path) -> RepoChecksReport {
    run_floor(worktree, &FloorContext::default())
}

/// Detect and run the checks in `worktree` for `ctx`, stopping at the first DENYING failure
/// ([`CheckRun::denies`]): a failure the run base shares is recorded and the floor moves on
/// (F-RC2-009). Fail-closed on a detection error and when no OS write boundary can be armed.
#[cfg_attr(not(test), allow(dead_code))]
pub fn run_floor(worktree: &Path, ctx: &FloorContext) -> RepoChecksReport {
    run_floor_rerun(worktree, ctx, None)
}

/// [`run_floor`] for an operator's re-run (core#469, [`FloorRerun`]): the same floor with the
/// check set and bounds the gate's choice asks for. `None` is the ordinary floor.
#[cfg_attr(not(test), allow(dead_code))]
pub fn run_floor_rerun(
    worktree: &Path,
    ctx: &FloorContext,
    rerun: Option<&FloorRerun>,
) -> RepoChecksReport {
    run_floor_journaled(worktree, ctx, rerun, None)
}

/// [`run_floor_rerun`] with a [`FloorJournal`]: the ordinary floor (no re-run, nothing waived)
/// records each finished check and resumes one a daemon restart interrupted on the same tree,
/// base and check set. An operator's re-run never reads or writes a journal.
pub fn run_floor_journaled(
    worktree: &Path,
    ctx: &FloorContext,
    rerun: Option<&FloorRerun>,
    journal: Option<&FloorJournal>,
) -> RepoChecksReport {
    run_floor_inner(worktree, ctx, rerun, journal, None)
}

/// [`run_floor_journaled`] with the probe a guarded checkout gives
/// ([`FloorCheckout::run_floor_journaled`]): a check is journaled only while the checkout still
/// holds the journal's tree.
fn run_floor_inner(
    worktree: &Path,
    ctx: &FloorContext,
    rerun: Option<&FloorRerun>,
    journal: Option<&FloorJournal>,
    untouched: Option<&dyn Fn() -> bool>,
) -> RepoChecksReport {
    // The scratch FIRST: its short `TMPDIR` under the system temp dir is the boundary's SECOND
    // write root (core#489), and the launcher needs it to exist before the probe — bwrap `--bind`s
    // a directory, the SBPL profile canonicalizes it. A scratch that cannot be prepared is the
    // same fail-closed detection error as before, reported against a worktree-only probe.
    let scratch = match CheckScratch::prepare_for(worktree, ctx.run_id.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            let sandbox = crate::validator::detect_floor_sandbox(&[worktree.to_path_buf()]);
            return scratch_refused(worktree, ctx, &sandbox, &e);
        }
    };
    // (round 11) The log store's lifetime is its age, collected best-effort at every floor's start.
    if let Some(root) = scratch.logs.parent() {
        crate::floor_log::gc(
            root,
            std::time::SystemTime::now(),
            crate::floor_log::log_cap_bytes(),
        );
    }
    let sandbox = crate::validator::detect_floor_sandbox(&[
        worktree.to_path_buf(),
        scratch.tmp.path().to_path_buf(),
        scratch.root().to_path_buf(),
    ]);
    let sandbox = arm_probed(sandbox, floor_probe());
    let mut report =
        run_with_sandbox_ctx(worktree, sandbox, ctx, scratch, rerun, journal, untouched);
    report.rerun = rerun.map(|r| r.mode);
    report
}

/// (core#678, codex r1) The floor arms only what the host's cached EXECUTION probe says arms: a
/// launcher on `PATH` whose probe failed here (bwrap with user namespaces disabled) is no boundary,
/// so the detection is downgraded and the operator's unsandboxed opt-in applies — exactly what
/// `hostBoundary().verifyFloor` predicts — instead of bwrap failing every check at run time.
/// The floor's own sandbox over `write_roots`, armed only when this host's floor probe passes.
pub(crate) fn floor_sandbox(write_roots: &[PathBuf]) -> WorkerSandbox {
    arm_probed(
        crate::validator::detect_floor_sandbox(write_roots),
        floor_probe(),
    )
}

fn arm_probed(
    sandbox: WorkerSandbox,
    probe: Result<&'static str, crate::worker_sandbox::FloorUnarmed>,
) -> WorkerSandbox {
    match probe {
        Err(why) if sandbox.level == crate::validator::SandboxLevel::Sandboxed => WorkerSandbox {
            wrapper: Vec::new(),
            level: crate::validator::SandboxLevel::BestEffort,
            downgrade_reason: Some(why.describe().to_string()),
        },
        _ => sandbox,
    }
}

/// (core#678, codex r2) Whether THIS floor's launcher arms on this host, probed once by executing
/// the floor's own wrapper (bwrap with `--unshare-pid`, the SBPL deny-writes profile) around
/// `sh -c 'exit 0'` — not the worker boundary's lighter probe, which a host can pass (mount
/// namespaces allowed) while the floor's launch fails (PID namespaces refused). `Err` is why no
/// boundary arms; `hostBoundary()` and [`run_floor`] both read this one cached value.
pub(crate) fn floor_probe() -> Result<&'static str, crate::worker_sandbox::FloorUnarmed> {
    static PROBE: std::sync::OnceLock<Result<&'static str, crate::worker_sandbox::FloorUnarmed>> =
        std::sync::OnceLock::new();
    *PROBE.get_or_init(|| {
        let tool = crate::worker_sandbox::boundary_tool()?;
        let dir = std::env::temp_dir().join(format!("wicked-floor-probe-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let armed = dir.canonicalize().ok().is_some_and(|d| {
            let sandbox = crate::validator::detect_floor_sandbox(std::slice::from_ref(&d));
            if sandbox.level != crate::validator::SandboxLevel::Sandboxed
                || sandbox.wrapper.is_empty()
            {
                return false;
            }
            // spawn-audit: hardened — the floor's own launcher around a no-op, run once per process.
            Command::new(&sandbox.wrapper[0])
                .hardened()
                .args(&sandbox.wrapper[1..])
                .args(["/bin/sh", "-c", "exit 0"])
                .current_dir(&d)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        });
        let _ = std::fs::remove_dir_all(&dir);
        if armed {
            Ok(tool)
        } else {
            Err(crate::worker_sandbox::FloorUnarmed::CannotArm)
        }
    })
}

/// [`run_floor`] against an explicit sandbox probe — the injectable seam, so the fail-closed branch
/// is testable on a host that HAS a sandbox tool by handing it a best-effort probe.
#[cfg(test)]
pub(crate) fn run_with_sandbox(worktree: &Path, sandbox: WorkerSandbox) -> RepoChecksReport {
    let ctx = FloorContext::default();
    match CheckScratch::prepare(worktree) {
        Ok(scratch) => {
            // An armed probe is re-armed over the roots production arms — the worktree, the
            // private TMPDIR (rustdoc's temp dir lives there) and the scratch leaf; a probe that
            // did not arm is handed through, so the fail-closed branches stay testable.
            let sandbox = if sandbox.level == crate::validator::SandboxLevel::Sandboxed
                && !sandbox.wrapper.is_empty()
            {
                crate::validator::detect_floor_sandbox(&[
                    worktree.to_path_buf(),
                    scratch.tmp.path().to_path_buf(),
                    scratch.root().to_path_buf(),
                ])
            } else {
                sandbox
            };
            run_with_sandbox_ctx(worktree, sandbox, &ctx, scratch, None, None, None)
        }
        Err(e) => scratch_refused(worktree, &ctx, &sandbox, &e),
    }
}

/// The fail-closed report for a scratch that could not be prepared (a linked leaf, no worker home,
/// an unwritable temp dir): detection is still reported so the record says what WOULD have run.
fn scratch_refused(
    worktree: &Path,
    ctx: &FloorContext,
    sandbox: &WorkerSandbox,
    e: &std::io::Error,
) -> RepoChecksReport {
    RepoChecksReport {
        detected: detect_with(worktree, ctx).unwrap_or_default(),
        checks: Vec::new(),
        skipped: Vec::new(),
        passed: false,
        detect_error: Some(format!(
            "the checks' isolated scratch could not be prepared: {e}"
        )),
        sandbox_level: sandbox.level.as_wire().to_string(),
        sandbox_note: sandbox.downgrade_reason.clone(),
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
    }
}

/// The operator's explicit, disclosed opt-in to running the repository's own check scripts with NO
/// OS write boundary — core#416 item 3(b), for the hosts where the floor can arm no boundary at
/// all and therefore denies the verify gate on every run by construction (Windows; a Linux host
/// without `bwrap`).
///
/// It is opt-IN, per host, and it is never inferred: absent this variable the floor keeps failing
/// closed, because "we could not contain repo-controlled code, so we ran it anyway" is not a
/// default anyone chooses knowingly. `1` or `true` arms it; anything else does not.
pub(crate) const UNSANDBOXED_OPT_IN_ENV: &str = "WICKED_REPO_CHECKS_UNSANDBOXED";

/// The [`RepoChecksReport::sandbox_level`] wire spelling for the opted-in run: `none`, distinct
/// from `best-effort` (which is the probe's own verdict, and on this path means the checks did NOT
/// run). A consumer folding enforcement (crew's `enforcement.unenforced`) reads `none` as "this
/// pass is test evidence, not containment evidence".
pub(crate) const UNSANDBOXED_LEVEL: &str = "none";

/// The disclosure text when [`UNSANDBOXED_OPT_IN_ENV`] is armed, or `None` when it is not (the
/// fail-closed default). One function so the decision and the words for it cannot drift.
fn unsandboxed_opt_in_note(probe_reason: Option<&str>) -> Option<String> {
    let armed = matches!(
        std::env::var(UNSANDBOXED_OPT_IN_ENV)
            .unwrap_or_default()
            .trim(),
        "1" | "true"
    );
    armed.then(|| {
        format!(
            "{UNSANDBOXED_OPT_IN_ENV}=1 — the repository's own check scripts ran with NO OS write \
             boundary ({}). The operator opted in: the checks' results are real, and this floor's \
             pass is NOT evidence that repo-controlled code was contained.",
            probe_reason.unwrap_or("no OS-sandbox tool on PATH")
        )
    })
}

pub(crate) fn run_with_sandbox_ctx(
    worktree: &Path,
    sandbox: WorkerSandbox,
    ctx: &FloorContext,
    scratch: CheckScratch,
    rerun: Option<&FloorRerun>,
    journal: Option<&FloorJournal>,
    untouched: Option<&dyn Fn() -> bool>,
) -> RepoChecksReport {
    let mut sandbox = sandbox;
    let mut sandbox_level = sandbox.level.as_wire().to_string();
    let mut sandbox_note = sandbox.downgrade_reason.clone();
    if sandbox.level != crate::validator::SandboxLevel::Sandboxed || sandbox.wrapper.is_empty() {
        match unsandboxed_opt_in_note(sandbox_note.as_deref()) {
            // (core#416 item 3b) The operator asked for the checks WITHOUT a boundary, knowing
            // what that means. Run them with no wrapper at all — not the probe's partial jail,
            // which would let the report imply a containment it does not have — and record
            // `sandbox_level: "none"` plus the disclosure on the report, the event and the
            // `FloorEnv`, so nothing downstream can read this pass as contained.
            Some(note) => {
                eprintln!("wicked-core: repo checks floor — {note}");
                sandbox_level = UNSANDBOXED_LEVEL.to_string();
                sandbox_note = Some(note.clone());
                sandbox = crate::validator::WorkerSandbox {
                    wrapper: Vec::new(),
                    level: sandbox.level,
                    downgrade_reason: Some(note),
                };
            }
            // NEVER run repo-controlled scripts unsandboxed (codex review on #414): the floor
            // fails with the probe's own reason, and the gate turns that into a denial the
            // operator can act on. Detection is still reported so the record says what WOULD have
            // run — and a detection FAILURE is reported as such, never as "no checks" (Copilot on
            // #414).
            None => {
                let (detected, detect_error) = match detect_with(worktree, ctx) {
                    Ok(d) => (d, None),
                    Err(e) => (Vec::new(), Some(e)),
                };
                return RepoChecksReport {
                    detected,
                    checks: Vec::new(),
                    skipped: Vec::new(),
                    passed: false,
                    detect_error,
                    sandbox_level,
                    sandbox_note: sandbox_note.clone(),
                    sandbox_error: Some(format!(
                        "no OS write boundary could be armed for the checks ({}). On this host the \
                         verify floor cannot run: install `bwrap` (Linux), or set \
                         {UNSANDBOXED_OPT_IN_ENV}=1 to run the repository's own scripts with NO \
                         write boundary, disclosed on every report",
                        sandbox_note.unwrap_or_else(|| "no OS-sandbox tool on PATH".to_string())
                    )),
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
                };
            }
        }
    }
    // (core#1152) One pass over the checks, reusing what the creator floor ran on this tree in
    // whatever order it sits. When a check that ran fresh AFTER reused ones fails, it may have
    // needed something a reused check leaves in the checkout (an emitted `dist/` an `e2e` reads):
    // the reuse is set aside and the checks run again from the first one with nothing reused —
    // the floor exactly as it ran before reuse existed.
    let (first, again) = run_detected(
        worktree,
        &sandbox,
        &sandbox_level,
        sandbox_note.as_deref(),
        ctx,
        &scratch,
        rerun,
        journal,
        untouched,
        None,
    );
    let Some((why, inputs)) = again else {
        return first;
    };
    eprintln!(
        "wicked-core: repo checks floor — the reused checks were set aside: {why}; running every \
         check again from the first one, nothing reused"
    );
    let (mut report, _) = run_detected(
        worktree,
        &sandbox,
        &sandbox_level,
        sandbox_note.as_deref(),
        ctx,
        &scratch,
        rerun,
        journal,
        untouched,
        Some(inputs),
    );
    report.reused_from = first.reused_from.map(|r| FloorReuse {
        fresh: Vec::new(),
        voided: Some(format!(
            "{why} — every check ran again here from the first one, nothing reused"
        )),
        ..r
    });
    report
}

/// (core#1152) Everything a floor reads from the tree before its first check runs: the checks, the
/// base's build guide, the change's files and each package's config. The no-reuse re-run
/// ([`run_with_sandbox_ctx`]) takes them exactly as the first pass read them.
#[derive(Clone)]
struct FloorInputs {
    detected: Vec<RepoCheck>,
    waived: Vec<String>,
    exclude: bool,
    guide: Result<Option<crate::floor_guide::BuildGuide>, String>,
    touched: Vec<String>,
    changed: Result<Vec<String>, String>,
    removed: Result<Vec<String>, String>,
    candidates: Vec<String>,
    baseline_diff: bool,
    package_baseline: std::collections::HashMap<String, bool>,
    package_note: Option<String>,
    e2e_paths: Option<Vec<String>>,
}

/// One pass of the floor over the detected checks ([`run_with_sandbox_ctx`]). With no `frozen`
/// inputs it reads them from the tree and lets a verify floor take the creator floor's results
/// ([`CreatorSeed`]); the second value is set when a check that ran (or was resumed) after reused
/// ones failed, with why and the inputs it read, and the caller then runs the checks again on
/// those inputs with nothing reused.
#[allow(clippy::too_many_arguments)]
fn run_detected(
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
            FloorInputs {
                detected,
                waived,
                exclude,
                guide,
                touched,
                changed,
                removed,
                candidates,
                baseline_diff,
                package_baseline,
                package_note,
                e2e_paths,
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
        candidates,
        baseline_diff,
        package_baseline,
        package_note,
        e2e_paths,
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
        (FloorStage::Verify, Some(tree), Some(base)) if allow_reuse => {
            CreatorSeed::load(&scratch.logs, tree, base, &sandbox_level, exclude)
        }
        _ => None,
    };
    // (core#1152) Reuse is in ANY order: every check the record stands for is reused, wherever it
    // sits, and only the rest run here. Two guards replace core#1145's suffix-only rule: a check
    // that runs fresh after reused ones and fails sends the floor round again with nothing reused
    // (`again`), and after the fresh checks the checkout must still hold exactly the tree the
    // reused results stand for (below), or they are void and the floor fails.
    let mut again: Option<String> = None;
    // The checks this floor RAN itself, for a creator floor's record: never one reused from an
    // interrupted floor's journal, which may have run in the live worktree (xreview r1).
    let mut ran_here: Vec<CreatorEntry> = Vec::new();
    for check in detected.iter() {
        if failed || scratch_error.is_some() {
            skipped.push(check.name.clone());
            continue;
        }
        if let Some(run) = seed.as_mut().and_then(|s| s.reuse(check, &scratch.logs)) {
            if let Some(s) = &seed {
                eprintln!("{}", s.log_line(&run));
            }
            failed = run.denies();
            if let Some(j) = journal.as_mut() {
                j.record(check, &run, untouched);
            }
            checks.push(run);
            continue;
        }
        if let Some(run) = journal.as_mut().and_then(|j| j.reuse(check, &scratch.logs)) {
            eprintln!(
                "wicked-core: repo checks floor — `{}` {} on this tree in the floor a daemon \
                 restart interrupted; reused, not run again",
                run.name,
                run.outcome()
            );
            failed = run.denies();
            // (xreview) A failure the interrupted pass ran after reused checks sends the floor
            // round again just as it would have there.
            if again.is_none() {
                again = seed.as_ref().and_then(|s| s.set_aside_by(check, &run));
            }
            if let Some(j) = journal.as_mut() {
                j.record(check, &run, untouched);
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
        let affected = gcheck
            .as_ref()
            .ok()
            .filter(|_| ctx.stage == FloorStage::Creator && !touched.is_empty() && !deletes)
            .and_then(|g| g.affected_argv(&package_relative(touched.clone(), &check.dir)));
        let mut run = match affected {
            Some(argv) => {
                let a = RepoCheck {
                    argv,
                    source: format!("`{}`'s affected set (DES-BUILD-GUIDE)", check.name),
                    ..check.clone()
                };
                let r = run_held(worktree, held_head, &a, sandbox, scratch, Tree::Head);
                if r.passed() {
                    r
                } else {
                    run_held(worktree, held_head, check, sandbox, scratch, Tree::Head)
                }
            }
            None => run_held(worktree, held_head, check, sandbox, scratch, Tree::Head),
        };
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
            let compare = check.name != "install" && !is_formatter_check(&check.name);
            // With exclusion off the base runs plainly: no results, no list, no alone runs.
            let excluding = gcheck.as_ref().ok().copied().filter(|_| exclude);
            let side = match (ctx.base_head.as_deref(), ctx.git_dir.as_deref()) {
                _ if !compare => None,
                (Some(head), _) if !(baseline_diff && baseline_diff_for(&check.dir)) => {
                    Some(BaseSide {
                        base: BaseRun {
                            head: head.to_string(),
                            cached: false,
                            run: None,
                            error: Some(format!(
                                "baseline diff disabled by `{CONFIG_PATH}` (baseline_diff: false)"
                            )),
                        },
                        toc: None,
                    })
                }
                (Some(head), Some(git_dir)) => {
                    // This run's cache first; on a miss the base is exported, run (and, red,
                    // inventoried), cached, and the export removed before the next HEAD run.
                    Some(match BaseTree::cached(scratch, head, check, excluding) {
                        Some(cached) => cached,
                        None => {
                            let export = held_worktree
                                .as_ref()
                                .map_err(String::clone)
                                .and_then(|root| BaseTree::export(worktree, root, git_dir, head));
                            let side = match export {
                                Ok(t) => t.run_check(check, ctx, sandbox, scratch, excluding),
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
                    worktree, held_head, check, &mut run, side, &gcheck, exclude, sandbox, scratch,
                );
            }
        }
        if let Some(d) = run.decision.as_mut() {
            d.guide = Some(status);
        }
        eprintln!(
            "wicked-core: repo checks floor — `{}` {} in {:.1}s (bound {}s){}",
            run.name,
            run.outcome(),
            run.duration_ms as f64 / 1000.0,
            run.bound_s,
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
        if let Some(j) = journal.as_mut() {
            j.record(check, &run, untouched);
        }
        if ctx.stage == FloorStage::Creator {
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
    let resumed = journal.and_then(OpenJournal::finish);
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
                    sandbox_level: sandbox_level.clone(),
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
    (
        RepoChecksReport {
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
        },
        again.zip(rerun_inputs),
    )
}

/// (core#817) A changed subdirectory package the floor detected NO checks for is named, so a
/// green floor is never read as having checked it.
fn package_coverage_note(
    worktree: &Path,
    ctx: &FloorContext,
    detected: &[RepoCheck],
) -> Option<String> {
    let pkgs = subdir_packages(worktree, ctx).ok()?;
    let bare: Vec<String> = pkgs
        .dirs
        .into_iter()
        // An install alone checks nothing.
        .filter(|d| !detected.iter().any(|c| &c.dir == d && c.name != "install"))
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
fn is_e2e_path(path: &str) -> bool {
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
fn e2e_coverage_note(
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
fn e2e_paths(worktree: &Path, ctx: &FloorContext) -> Option<Vec<String>> {
    let mut paths = touched_files(worktree, ctx).ok()?;
    paths.extend(removed_files(worktree, ctx).ok()?);
    let mut e2e: Vec<String> = paths.into_iter().filter(|p| is_e2e_path(p)).collect();
    e2e.sort();
    e2e.dedup();
    Some(e2e)
}

/// Is `name` one of the floor's TEST-set checks (the ones a `targeted` re-run replaces)?
fn is_test_set_check(name: &str) -> bool {
    matches!(name, "test" | "cargo-test" | "e2e") || name.starts_with(JOURNEY_PREFIX)
}

/// (core#469) The check set an operator's re-run runs, and the detected checks it waives.
/// * `extend` — every check (the install excepted) under [`EXTEND_FACTOR`]× its base bound;
/// * `targeted` — the CREATOR-stage selection (the declared `test_targeted` in place of the full
///   test set, no `e2e`); a repo with no targeted command has its test set waived;
/// * `accept_partial` — the named checks are waived, the rest run again.
fn apply_rerun(
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
            let creator = detect_with(
                worktree,
                &FloorContext {
                    stage: FloorStage::Creator,
                    ..ctx.clone()
                },
            )?;
            if creator.iter().any(|c| c.name == "test_targeted") {
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
                let (run, waived): (Vec<RepoCheck>, Vec<RepoCheck>) = detected
                    .into_iter()
                    .partition(|c| !is_test_set_check(&c.name));
                Ok((run, waived.into_iter().map(|c| c.name).collect()))
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

/// (core#417, operator ruling option (a)) A guarded VERIFY floor runs on a clean checkout of
/// EXACTLY the tree the worktree guard checked — never the live worktree, where an untracked or
/// ignored file the seat planted (a `.env.local`, a `vitest.config.local.*`, a shim under
/// `node_modules/`, a self-ignoring `.gitignore` beside a payload) could steer the repository's
/// own checks while the guard's tree hash stayed the same.
///
/// The guard's tree becomes a commit object (`commit-tree`, parented on HEAD when there is one; no
/// ref moves) checked out as a DETACHED linked worktree through the pinned git dir, under an
/// engine-owned directory in the system temp — outside the seat's write boundary. So the checks
/// see the tracked and untracked-not-ignored content the guard hashed and nothing else, with real
/// git metadata for checks that call git. The cost (a fresh install and a cold build per verify
/// floor) is the ruling's accepted price. Dropped ⇒ the worktree is removed and pruned.
pub(crate) struct FloorCheckout {
    pub(crate) dir: PathBuf,
    git_dir: PathBuf,
    worktree: PathBuf,
    /// Every blob entry written, with the exact bytes materialized (after eol conversion) — what
    /// [`Self::hold_to`] re-checks.
    entries: Vec<WrittenEntry>,
    /// `dir`, held before anything ran in it: [`Self::hold_to`] re-reads every entry through it.
    held: Option<HeldDir>,
}

impl Drop for FloorCheckout {
    fn drop(&mut self) {
        // The checkout's check scratch is its own (core#417: a guarded floor never builds on the
        // live worktree's artifacts) and dies with it.
        remove_tree_scratch(&self.dir);
        let env: [(&str, &Path); 1] = [("GIT_DIR", self.git_dir.as_path())];
        let dir = self.dir.to_string_lossy().into_owned();
        let _ = crate::worktree_guard::git(
            &self.worktree,
            &["worktree", "remove", "--force", &dir],
            &env,
        );
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = crate::worktree_guard::git(&self.worktree, &["worktree", "prune"], &env);
    }
}

/// Check out `tree` (the guard's snapshot) for a verify floor — see [`FloorCheckout`]. `Err` names
/// what failed; the caller denies the floor on it (fail-closed: a floor that cannot get the guarded
/// tree does not fall back to the live one).
pub(crate) fn checkout_guarded_tree(
    worktree: &Path,
    git_dir: &Path,
    tree: &str,
    label: &str,
) -> Result<FloorCheckout, String> {
    let env: [(&str, &Path); 5] = [
        ("GIT_DIR", git_dir),
        ("GIT_AUTHOR_NAME", Path::new("wicked-core")),
        ("GIT_AUTHOR_EMAIL", Path::new("floor@wicked-core.invalid")),
        ("GIT_COMMITTER_NAME", Path::new("wicked-core")),
        (
            "GIT_COMMITTER_EMAIL",
            Path::new("floor@wicked-core.invalid"),
        ),
    ];
    let msg = "wicked-core: the guarded tree a verify floor runs on (core#417)";
    let commit = crate::worktree_guard::git_string(
        worktree,
        &["commit-tree", tree, "-p", "HEAD", "-m", msg],
        &env,
    )
    .or_else(|_| {
        crate::worktree_guard::git_string(worktree, &["commit-tree", tree, "-m", msg], &env)
    })
    .map_err(|e| format!("the guarded tree {tree} could not be committed for the floor: {e}"))?;
    let label: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let parent = std::env::temp_dir().join("wicked-core-floor");
    std::fs::create_dir_all(&parent)
        .map_err(|e| format!("the floor checkout directory could not be created: {e}"))?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = parent.join(format!("{label}-{}-{nonce}", std::process::id()));
    let dir_s = dir.to_string_lossy().into_owned();
    let mut checkout = FloorCheckout {
        dir: dir.clone(),
        git_dir: git_dir.to_path_buf(),
        worktree: worktree.to_path_buf(),
        entries: Vec::new(),
        held: None,
    };
    // `--no-checkout` (codex r1 on the slice): a checkout runs the repository's `post-checkout`
    // hook and its configured smudge filters OUTSIDE the floor's sandbox. The worktree is made
    // empty, its files are written by git's own eol conversion run against a PRIVATE repository
    // that carries no filter driver ([`write_private_git_dir`]), and its index is set with
    // `read-tree`, which touches no file. Every git call here runs with an engine-owned EMPTY
    // hooks dir (codex r2): `read-tree` alone fires `post-index-change`, and no repository hook
    // may run outside the floor's sandbox.
    let no_hooks_dir = no_hooks_dir()?;
    let no_hooks = format!("core.hooksPath={}", no_hooks_dir.to_string_lossy());
    crate::worktree_guard::git(
        worktree,
        &[
            "-c",
            &no_hooks,
            "worktree",
            "add",
            "--no-checkout",
            "--detach",
            &dir_s,
            &commit,
        ],
        &env[..1],
    )
    .map_err(|e| format!("the guarded tree {tree} could not be checked out for the floor: {e}"))?;
    // Held before anything runs in it; every blob is written through the held fd.
    let held = HeldDir::open(&dir)
        .map_err(|e| format!("the guarded tree {tree} could not be written for the floor: {e}"))?;
    // The conversion repository lives beside the checkout, engine-owned, and dies with the export.
    let conv = parent.join(format!("{label}-conv-{}-{nonce}", std::process::id()));
    let converted = std::fs::create_dir_all(&conv)
        .map_err(|e| e.to_string())
        .and_then(|_| HeldDir::open(&conv))
        .and_then(|conv_held| {
            write_private_git_dir(&conv_held, "git", git_dir, worktree, &commit, true)
        })
        .and_then(|_| write_tree_blobs(worktree, git_dir, tree, &dir, &held, &conv.join("git")));
    let _ = std::fs::remove_dir_all(&conv);
    checkout.entries = converted
        .map_err(|e| format!("the guarded tree {tree} could not be written for the floor: {e}"))?;
    checkout.held = Some(held);
    crate::worktree_guard::git(&dir, &["-c", &no_hooks, "read-tree", &commit], &[])
        .map_err(|e| format!("the floor checkout's index could not be set: {e}"))?;
    Ok(checkout)
}

/// What the export materialized for one tree entry — recorded at write time so the integrity
/// re-check ([`FloorCheckout::hold_to`]) compares against the EXACT bytes the engine wrote, never
/// against bytes recomputed through attributes a check could since have edited.
#[derive(Debug, Clone)]
enum Materialized {
    /// A symlink entry: the raw target (never converted). Read back on unix only: elsewhere a link
    /// is written as a regular file and recorded as [`Materialized::File`].
    #[cfg_attr(not(unix), allow(dead_code))]
    Link(Vec<u8>),
    /// A regular file: the length and sha256 of the bytes written (after eol conversion).
    File { len: u64, digest: [u8; 32] },
}

/// One entry [`write_tree_blobs`] wrote: its git mode, its path, and what was materialized.
#[derive(Debug, Clone)]
pub(crate) struct WrittenEntry {
    mode: String,
    path: PathBuf,
    what: Materialized,
}

/// The "no config file" path for `GIT_CONFIG_GLOBAL`.
const NULL_CONFIG: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// Variables through which an inherited environment could hand git configuration (a
/// `filter.<x>.smudge` driver, say) or redirect a private repository to other metadata. A caller
/// that pins `GIT_DIR`/`GIT_WORK_TREE` sets them itself.
const INHERITED_GIT_ENV: &[&str] = &[
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_ATTR_SOURCE",
    "GIT_GRAFT_FILE",
    "GIT_SHALLOW_FILE",
    "GIT_REPLACE_REF_BASE",
];

/// Pin a git command the engine runs on a private repository to that repository's own
/// configuration: no system file, no global/XDG file, and no configuration or repository
/// redirection handed down through the environment ([`INHERITED_GIT_ENV`]: `GIT_CONFIG_COUNT` /
/// `GIT_CONFIG_KEY_<n>` / `GIT_CONFIG_PARAMETERS` would otherwise reach `cat-file --filters`).
/// `.hardened()` only strips the engine's own variables, not these.
fn isolate_git_config(cmd: &mut Command) {
    for key in INHERITED_GIT_ENV {
        cmd.env_remove(key);
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", NULL_CONFIG);
}

/// Quote a value for a git config file.
fn config_quote(v: &str) -> String {
    let mut s = String::from("\"");
    for c in v.chars() {
        match c {
            '\\' => s.push_str("\\\\"),
            '"' => s.push_str("\\\""),
            '\n' => s.push_str("\\n"),
            '\t' => s.push_str("\\t"),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

/// The engine's empty hooks directory (no repository hook may run outside the floor's sandbox).
fn no_hooks_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir()
        .join("wicked-core-floor")
        .join("no-hooks");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("the floor's empty hooks dir could not be created: {e}"))?;
    Ok(dir)
}

/// Write a minimal PRIVATE git directory at `git_rel` under `dest` (through the held directory):
/// `HEAD` = `commit`, the source repository's objects as a read-only ALTERNATE, an empty `hooks/`
/// and a `config` that carries the source's effective end-of-line settings
/// (`core.autocrlf`/`eol`/`safecrlf`) and NOTHING else — no `filter.*` driver, no `core.hooksPath`,
/// no `include`. So `git cat-file --filters` against it converts exactly as a checkout of the
/// source repository would (built-in text/eol/working-tree-encoding) yet can execute no
/// smudge/process driver; and a git command run in it runs no repository hook. `bare` sets
/// `core.bare` (the base export is a work tree; a conversion-only repository is not).
fn write_private_git_dir(
    dest: &HeldDir,
    git_rel: &str,
    source_git_dir: &Path,
    worktree: &Path,
    commit: &str,
    bare: bool,
) -> Result<(), String> {
    let objects = crate::worktree_guard::git_string(
        worktree,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ],
        &[("GIT_DIR", source_git_dir)],
    )
    .map_err(|e| format!("the object store could not be located: {e}"))?;
    // Unset keys make `git config --get-regexp` exit 1: no settings, the defaults.
    let eol = crate::worktree_guard::git(
        worktree,
        &[
            "config",
            "-z",
            "--get-regexp",
            r"^core\.(autocrlf|eol|safecrlf)$",
        ],
        &[("GIT_DIR", source_git_dir)],
    )
    .unwrap_or_default();
    // The private repository must hash like the source, or its alternate object store and HEAD
    // cannot be read: a SHA-256 source gets format version 1 and `extensions.objectformat`.
    let sha256 = crate::worktree_guard::git_string(
        worktree,
        &["rev-parse", "--show-object-format"],
        &[("GIT_DIR", source_git_dir)],
    )
    .is_ok_and(|f| f == "sha256");
    let mut config = format!(
        "[core]\n\trepositoryformatversion = {}\n\tfilemode = true\n\tbare = {bare}\n\tlogallrefupdates = false\n",
        u8::from(sha256)
    );
    for rec in String::from_utf8_lossy(&eol).split('\0') {
        if let Some((key, value)) = rec.split_once('\n') {
            if let Some(name) = key.strip_prefix("core.") {
                if matches!(name, "autocrlf" | "eol" | "safecrlf") {
                    config.push_str(&format!("\t{name} = {}\n", config_quote(value)));
                }
            }
        }
    }
    // The extension section goes last: the eol keys above belong to `[core]`.
    if sha256 {
        config.push_str("[extensions]\n\tobjectformat = sha256\n");
    }
    dest.write(&format!("{git_rel}/HEAD"), &format!("{commit}\n"))?;
    dest.write(&format!("{git_rel}/config"), &config)?;
    dest.write(
        &format!("{git_rel}/objects/info/alternates"),
        &format!("{objects}\n"),
    )?;
    dest.subdir(&format!("{git_rel}/refs/heads"), true)?;
    dest.subdir(&format!("{git_rel}/hooks"), true)?;
    Ok(())
}

/// The path given to the end-of-body request (any; the object is missing).
const SENTINEL_PATH: &str = "wicked-end";

/// Stream `inputs` (one line each) through ONE `git cat-file --batch[ --filters]` run against the
/// private repository `conv_git_dir` and hand each answered body to `each`. With `filtered` an
/// input is `<blob> <path>` and the body is the blob converted to its working-tree form under the
/// attributes of `tree` (`--attr-source`: the tree's own `.gitattributes`, never a file of any
/// working tree) and the private repository's config.
fn cat_batch(
    conv_git_dir: &Path,
    tree: &str,
    filtered: bool,
    inputs: Vec<String>,
    mut each: impl FnMut(usize, Vec<u8>) -> Result<(), String>,
) -> Result<(), String> {
    use std::io::{BufRead, Read, Write};
    if inputs.is_empty() {
        return Ok(());
    }
    let attr_source = format!("--attr-source={tree}");
    let mut args: Vec<&str> = vec!["-c", "core.bare=true"];
    if filtered {
        args.push(&attr_source);
    }
    args.extend(["cat-file", "--batch"]);
    if filtered {
        args.push("--filters");
    }
    // spawn-audit: hardened — git plumbing over a private repository holding no driver config.
    let mut cat = Command::new("git");
    cat.hardened()
        .args(&args)
        .current_dir(conv_git_dir)
        .env("GIT_DIR", conv_git_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    isolate_git_config(&mut cat);
    let mut child = cat.spawn().map_err(|e| format!("git cat-file: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("git cat-file has no stdin")?;
    let n = inputs.len();
    // With `--filters` the header's size is the BLOB's, not the converted body's (`echo ok\n`
    // under `eol=crlf` is announced as 8 and is 9 bytes), so a converted body cannot be framed by
    // it. Each request is followed by one for a random object id that cannot exist: git answers it
    // `<id> missing`, and the body ends at that line. The id is drawn per call (random bits from two UUIDs), so no committed content can forge it.
    let hex_len = if tree.len() == 64 { 64 } else { 40 };
    let sentinel: String = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )[..hex_len]
        .to_string();
    let end = format!("\n{sentinel} missing\n").into_bytes();
    let feed_sentinel = sentinel.clone();
    let feeder = std::thread::spawn(move || {
        for line in inputs {
            if writeln!(stdin, "{line}").is_err() {
                break;
            }
            if filtered && writeln!(stdin, "{feed_sentinel} {SENTINEL_PATH}").is_err() {
                break;
            }
        }
    });
    let mut out = std::io::BufReader::new(child.stdout.take().ok_or("git cat-file has no stdout")?);
    let mut answer = || -> Result<(), String> {
        for i in 0..n {
            let mut header = String::new();
            out.read_line(&mut header).map_err(|e| e.to_string())?;
            let mut words = header.split_whitespace();
            let (_, kind, size) = (words.next(), words.next(), words.next());
            if kind != Some("blob") {
                return Err(format!("git cat-file answered {header:?}"));
            }
            let body = if filtered {
                let mut acc: Vec<u8> = Vec::new();
                loop {
                    if out.read_until(b'\n', &mut acc).map_err(|e| e.to_string())? == 0 {
                        return Err("git cat-file ended inside a converted body".to_string());
                    }
                    if acc.ends_with(&end) {
                        acc.truncate(acc.len() - end.len());
                        break acc;
                    }
                }
            } else {
                let size: usize = size
                    .and_then(|n| n.parse().ok())
                    .ok_or_else(|| format!("git cat-file answered {header:?}"))?;
                let mut body = vec![0u8; size];
                out.read_exact(&mut body).map_err(|e| e.to_string())?;
                let mut nl = [0u8; 1];
                out.read_exact(&mut nl).map_err(|e| e.to_string())?;
                body
            };
            each(i, body)?;
        }
        Ok(())
    };
    let answered = answer();
    drop(out);
    let _ = feeder.join();
    let _ = child.wait();
    answered
}

/// Whether a path component names the export's `.git` on some filesystem: `.git` in any case,
/// and its NTFS aliases (trailing dots/spaces, an `:` alternate stream, the `git~1` short name),
/// which Windows resolves to the same directory (git's own `is_ntfs_dotgit`).
fn names_git_metadata(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = lower.split(':').next().unwrap_or_default();
    let stem = stem.trim_end_matches(['.', ' ']);
    stem == ".git" || stem == "git~1"
}

/// Write every entry of `tree` into `dir` — `ls-tree -r -z`, then `cat-file --batch --filters`
/// for regular files and `cat-file --batch` for symlink targets — so nothing repository-controlled
/// runs (no checkout, no filter/smudge/process driver, no hook, no `export-ignore`). Regular files
/// are converted exactly as `git checkout` would convert them (`text`/`eol`/`working-tree-encoding`
/// attributes of `tree` itself, `core.autocrlf`/`eol` of the source repository) by git's own
/// conversion run against `conv_git_dir`, a private repository with no driver config
/// ([`write_private_git_dir`]). Regular files keep their executable bit; a symlink entry becomes a
/// symlink (unix) or a plain file holding its raw target; a submodule becomes an empty directory.
/// A path that is absolute, climbs out of `dir`, or names `.git` is refused.
fn write_tree_blobs(
    worktree: &Path,
    git_dir: &Path,
    tree: &str,
    dir: &Path,
    dest: &HeldDir,
    conv_git_dir: &Path,
) -> Result<Vec<WrittenEntry>, String> {
    write_tree_blobs_with(
        worktree,
        git_dir,
        tree,
        dir,
        dest,
        conv_git_dir,
        git_has_attr_source(),
    )
}

/// Whether this host's git has the global `--attr-source` option (git 2.40+), probed once per
/// process. Without it the export cannot convert under the exported tree's own attributes, so it
/// writes the raw blobs, as before eol conversion existed, rather than fail every floor export on
/// an older git (Ubuntu 22.04 ships 2.34).
fn git_has_attr_source() -> bool {
    static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS.get_or_init(|| {
        // spawn-audit: hardened — a capability probe: `git --attr-source=<x> version` exits 0 on a
        // git that knows the option and 129 ("unknown option") on one that does not.
        let mut cmd = Command::new("git");
        cmd.hardened()
            .args(["--attr-source=HEAD", "version"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        isolate_git_config(&mut cmd);
        let has = cmd.status().is_ok_and(|s| s.success());
        if !has {
            eprintln!(
                "wicked-core floor: this git has no --attr-source (git < 2.40); the floor's copies \
                 are written as raw blobs, without .gitattributes eol conversion"
            );
        }
        has
    })
}

/// [`write_tree_blobs`] with the conversion chosen: `convert` = false writes regular files as their
/// raw blobs (the fallback for a git without `--attr-source`).
fn write_tree_blobs_with(
    worktree: &Path,
    git_dir: &Path,
    tree: &str,
    dir: &Path,
    dest: &HeldDir,
    conv_git_dir: &Path,
    convert: bool,
) -> Result<Vec<WrittenEntry>, String> {
    let env: [(&str, &Path); 1] = [("GIT_DIR", git_dir)];
    let listing = crate::worktree_guard::git(worktree, &["ls-tree", "-r", "-z", tree], &env)
        .map_err(|e| e.to_string())?;
    let mut entries: Vec<(String, String, PathBuf, String)> = Vec::new();
    for rec in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        // Strict (codex r2): a lossy decode can fold two distinct paths into one.
        let rec = std::str::from_utf8(rec)
            .map_err(|_| "the tree holds a path that is not UTF-8".to_string())?;
        let (meta, path) = rec
            .split_once('\t')
            .ok_or_else(|| format!("unreadable ls-tree entry {rec:?}"))?;
        let mut parts = meta.split_whitespace();
        let (mode, _kind, sha) = (
            parts.next().unwrap_or_default().to_string(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default().to_string(),
        );
        let rel = Path::new(path);
        // `.git` is the engine's own metadata in a base export: a tree entry may never write it.
        // A newline would split the batch input line.
        if rel.is_absolute()
            || path.contains('\n')
            || rel.components().any(|c| match c {
                std::path::Component::Normal(n) => names_git_metadata(&n.to_string_lossy()),
                _ => true,
            })
        {
            return Err(format!("the tree holds an unsafe path {path:?}"));
        }
        entries.push((mode, sha, dir.join(rel), path.to_string()));
    }
    // An entry's path relative to `dir` (UTF-8: checked above).
    let rel_of = |path: &Path| -> Result<String, String> {
        Ok(path
            .strip_prefix(dir)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned())
    };
    let mut written: Vec<WrittenEntry> = Vec::new();
    // Regular files: converted. Through the held directory ([`HeldDir`]): missing directories are
    // created, and no component under `dir`, nor the entry itself, is ever followed as a link —
    // even one a process still running in the tree swaps in mid-export.
    let files: Vec<&(String, String, PathBuf, String)> = entries
        .iter()
        .filter(|(m, ..)| m != "160000" && m != "120000")
        .collect();
    let inputs = files
        .iter()
        .map(|(_, sha, _, path)| {
            if convert {
                format!("{sha} {path}")
            } else {
                sha.clone()
            }
        })
        .collect();
    cat_batch(conv_git_dir, tree, convert, inputs, |i, body| {
        use sha2::Digest;
        let (mode, _, path, _) = files[i];
        dest.write_bytes(&rel_of(path)?, &body, mode == "100755")?;
        written.push(WrittenEntry {
            mode: mode.clone(),
            path: path.clone(),
            what: Materialized::File {
                len: body.len() as u64,
                digest: sha2::Sha256::digest(&body).into(),
            },
        });
        Ok(())
    })?;
    // Symlinks: the raw target, never converted.
    let links: Vec<&(String, String, PathBuf, String)> =
        entries.iter().filter(|(m, ..)| m == "120000").collect();
    let inputs = links.iter().map(|(_, sha, ..)| sha.clone()).collect();
    cat_batch(conv_git_dir, tree, false, inputs, |i, body| {
        use sha2::Digest;
        let (mode, _, path, _) = links[i];
        let rel = rel_of(path)?;
        #[cfg(unix)]
        dest.symlink(&rel, &body)?;
        #[cfg(not(unix))]
        dest.write_bytes(&rel, &body, false)?;
        written.push(WrittenEntry {
            mode: mode.clone(),
            path: path.clone(),
            what: if cfg!(unix) {
                Materialized::Link(body)
            } else {
                Materialized::File {
                    len: body.len() as u64,
                    digest: sha2::Sha256::digest(&body).into(),
                }
            },
        });
        Ok(())
    })?;
    for (_, _, path, _) in entries.iter().filter(|(m, ..)| m == "160000") {
        dest.subdir(&rel_of(path)?, true)?;
    }
    Ok(written)
}

impl FloorCheckout {
    /// (codex r2) The checks ran IN the checkout: a check that rewrote tracked content there would
    /// otherwise pass against code that is not the tree that ships. After the floor, the checkout
    /// must still hash to the guarded `tree` (the engine scratch excluded); a difference fails the
    /// report, naming it.
    /// (J4) The floor on this checkout, journaled on `journal.tree` and held to it
    /// ([`Self::hold_to`]). A check is journaled only while the checkout still holds that tree, so
    /// a later floor on a FRESH checkout never reuses a pass that ran on content the checks changed.
    pub(crate) fn run_floor_journaled(
        &self,
        ctx: &FloorContext,
        rerun: Option<&FloorRerun>,
        journal: &FloorJournal,
    ) -> RepoChecksReport {
        let untouched = || matches!(self.changed_entries(), Ok(c) if c.is_empty());
        let mut r = run_floor_inner(&self.dir, ctx, rerun, Some(journal), Some(&untouched));
        self.hold_to(&journal.tree, &mut r);
        r
    }

    pub(crate) fn hold_to(&self, tree: &str, report: &mut RepoChecksReport) {
        let why = match self.changed_entries() {
            Ok(changed) if changed.is_empty() => return,
            Ok(changed) => format!(
                "the checks changed the tracked content they ran on ({}{}) — a guarded floor \
                 certifies only the guarded tree {}, untouched",
                changed
                    .iter()
                    .take(5)
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                if changed.len() > 5 { ", …" } else { "" },
                &tree[..tree.len().min(10)]
            ),
            Err(e) => format!("the floor checkout could not be re-checked after the checks: {e}"),
        };
        report.passed = false;
        report.detect_error = Some(match report.detect_error.take() {
            Some(prev) => format!("{prev}; {why}"),
            None => why,
        });
    }

    /// The tree entries whose content no longer matches what the export wrote, every entry read
    /// THROUGH the held checkout ([`HeldDir`]: parents walked through no link, the entry itself
    /// never followed) — a check that swapped a directory of the tree for a link to an outside
    /// copy has every entry under it read as changed, never the copy compared. A regular file
    /// must still be one, with its executable bit, and hold exactly the bytes the export
    /// MATERIALIZED (their length and sha256, recorded at write time — the eol-converted form a
    /// checkout would have, never recomputed through attributes a check could have edited); a
    /// symlink entry must still be a symlink to the SAME target. Untracked files a check created
    /// are not entries and are not judged — only what ships is.
    fn changed_entries(&self) -> Result<Vec<String>, String> {
        let held = self
            .held
            .as_ref()
            .ok_or("the floor checkout was never held")?;
        let rel = |p: &Path| p.strip_prefix(&self.dir).unwrap_or(p).display().to_string();
        let mut changed = Vec::new();
        for e in &self.entries {
            let r = rel(&e.path);
            let same = match &e.what {
                // A symlink stays a symlink, to the SAME target (codex r3).
                #[cfg(unix)]
                Materialized::Link(target) => {
                    held.kind(&r) == Ok(Some(EntryKind::Link))
                        && held.read_link(&r).is_ok_and(|t| &t == target)
                }
                #[cfg(not(unix))]
                Materialized::Link(_) => false,
                Materialized::File { len, digest } => {
                    same_materialized(held, &r, e.mode == "100755", *len, digest)
                }
            };
            if !same {
                changed.push(r);
            }
        }
        Ok(changed)
    }
}

/// Is the entry at `rel` of `held` a regular file (opened through no link) with the executable
/// bit `exec` holding exactly `len` bytes whose sha256 is `digest`?
fn same_materialized(held: &HeldDir, rel: &str, exec: bool, len: u64, digest: &[u8; 32]) -> bool {
    use sha2::Digest;
    use std::io::Read;
    let Ok(mut f) = held.open_read(rel) else {
        return false;
    };
    let Ok(m) = f.metadata() else {
        return false;
    };
    if !m.is_file() || m.len() != len {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if (m.permissions().mode() & 0o100 != 0) != exec {
            return false;
        }
    }
    #[cfg(not(unix))]
    let _ = exec;
    let mut now = Vec::with_capacity(len as usize);
    f.read_to_end(&mut now).is_ok() && sha2::Sha256::digest(&now)[..] == digest[..]
}

/// A symlink entry materialized where there are no symlinks (non-unix: [`write_tree_blobs`]
/// writes a regular file holding the link's target): unchanged only while it is still that regular
/// file with exactly `target` as its content.
#[cfg(test)]
fn same_placeholder(held: &HeldDir, rel: &str, target: &[u8]) -> bool {
    same_file(held, rel, false, target)
}

/// Is the entry at `rel` of `held` a regular file (opened through no link) with the executable
/// bit `exec` (unix; git keys on the OWNER execute bit, codex r3) holding exactly `body`?
#[cfg(test)]
fn same_file(held: &HeldDir, rel: &str, exec: bool, body: &[u8]) -> bool {
    use std::io::Read;
    let Ok(mut f) = held.open_read(rel) else {
        return false;
    };
    let Ok(m) = f.metadata() else {
        return false;
    };
    if !m.is_file() || m.len() != body.len() as u64 {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if (m.permissions().mode() & 0o100 != 0) != exec {
            return false;
        }
    }
    #[cfg(not(unix))]
    let _ = exec;
    let mut now = Vec::with_capacity(body.len());
    f.read_to_end(&mut now).is_ok() && now == body
}

impl FloorCheckout {
    /// (core#1173) Regenerate declared generated files in this checkout, as the floor would run a
    /// check: the floor's write boundary (or the operator's disclosed unsandboxed opt-in, never an
    /// unannounced bare run), the isolated scratch homes, the floor's own dependency install
    /// first, then each set's command in the order declared under the check bound (`timeout_s`).
    /// `Err` says which step failed and why, with the end of its output.
    pub(crate) fn regenerate(
        &self,
        sets: &[GeneratedFiles],
        timeout_s: Option<u64>,
    ) -> Result<(), String> {
        let scratch = CheckScratch::prepare_for(&self.dir, None)
            .map_err(|e| format!("the checks' isolated scratch could not be prepared: {e}"))?;
        let mut sandbox = floor_sandbox(&[
            self.dir.clone(),
            scratch.tmp.path().to_path_buf(),
            scratch.root().to_path_buf(),
        ]);
        if sandbox.level != crate::validator::SandboxLevel::Sandboxed || sandbox.wrapper.is_empty()
        {
            match unsandboxed_opt_in_note(sandbox.downgrade_reason.as_deref()) {
                Some(note) => {
                    eprintln!("wicked-core: deliver lift regenerate — {note}");
                    sandbox = WorkerSandbox {
                        wrapper: Vec::new(),
                        level: sandbox.level,
                        downgrade_reason: Some(note),
                    };
                }
                None => {
                    return Err(format!(
                        "no OS write boundary could be armed for the generator ({}); the \
                         repository's own commands never run unsandboxed unless \
                         {UNSANDBOXED_OPT_IN_ENV}=1",
                        sandbox
                            .downgrade_reason
                            .as_deref()
                            .unwrap_or("no OS-sandbox tool on PATH")
                    ))
                }
            }
        }
        let ctx = FloorContext::default();
        let installs: Vec<RepoCheck> = detect_with(&self.dir, &ctx)
            .map_err(|e| format!("the checkout's checks could not be detected: {e}"))?
            .into_iter()
            .filter(|c| c.name == PROVISIONING_CHECK)
            .collect();
        let commands = sets.iter().map(|g| RepoCheck {
            name: "regenerate".to_string(),
            argv: g.argv.clone(),
            source: format!("{CONFIG_PATH} generated ({})", g.paths.join(", ")),
            timeout_s,
            dir: String::new(),
        });
        for check in installs.into_iter().chain(commands) {
            let run = run_held(
                &self.dir,
                self.held.as_ref(),
                &check,
                &sandbox,
                &scratch,
                Tree::Head,
            );
            if !run.passed() {
                let tail = match last_lines(&run.stderr_tail) {
                    t if t.is_empty() => last_lines(&run.stdout_tail),
                    t => t,
                };
                return Err(format!(
                    "`{}` ({}) — {}{}",
                    check.argv.join(" "),
                    check.source,
                    run.summary(),
                    if tail.is_empty() {
                        String::new()
                    } else {
                        format!(": {tail}")
                    }
                ));
            }
        }
        Ok(())
    }

    /// (core#1173) The regular file at `rel` in this checkout — its bytes and whether it is
    /// executable — read without following a link; `None` when nothing is there. Anything else (a
    /// link, a directory) is an error: a generated file is a file.
    pub(crate) fn read_generated(&self, rel: &str) -> Result<Option<(Vec<u8>, bool)>, String> {
        let held = self
            .held
            .as_ref()
            .ok_or_else(|| "the checkout is not held".to_string())?;
        // A parent the generator removed means the file is gone (codex on #1175); a parent that is
        // not a directory (a link) is refused.
        let names: Vec<&str> = rel.split('/').collect();
        for depth in 1..names.len() {
            match held.kind(&names[..depth].join("/"))? {
                None => return Ok(None),
                Some(EntryKind::Dir) => {}
                Some(_) => {
                    return Err(format!(
                        "`{rel}`: `{}` is not a directory after regenerating",
                        names[..depth].join("/")
                    ))
                }
            }
        }
        match held.kind(rel)? {
            None => Ok(None),
            Some(EntryKind::File) => {
                let mut f = held.open_read(rel)?;
                let exec = {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        f.metadata()
                            .map_err(|e| format!("`{rel}`: {e}"))?
                            .permissions()
                            .mode()
                            & 0o111
                            != 0
                    }
                    #[cfg(not(unix))]
                    {
                        false
                    }
                };
                let mut body = Vec::new();
                f.read_to_end(&mut body)
                    .map_err(|e| format!("`{rel}` could not be read: {e}"))?;
                Ok(Some((body, exec)))
            }
            Some(_) => Err(format!("`{rel}` is not a regular file after regenerating")),
        }
    }
}

/// The fail-closed report for a verify floor whose guarded checkout could not be made: nothing
/// ran, and the floor does not pass.
pub(crate) fn guarded_checkout_refused(
    worktree: &Path,
    ctx: &FloorContext,
    why: &str,
) -> RepoChecksReport {
    let sandbox = crate::validator::detect_floor_sandbox(&[worktree.to_path_buf()]);
    RepoChecksReport {
        detected: detect_with(worktree, ctx).unwrap_or_default(),
        checks: Vec::new(),
        skipped: Vec::new(),
        passed: false,
        detect_error: Some(format!(
            "the verify floor runs on a clean checkout of the tree the worktree guard checked \
             (core#417), and it could not be made: {why}"
        )),
        sandbox_level: sandbox.level.as_wire().to_string(),
        sandbox_note: sandbox.downgrade_reason.clone(),
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
    }
}

/// The run base, exported into the checks' scratch for the baseline diff — the base's own files,
/// written through git's own eol conversion (`ls-tree` + `cat-file --batch --filters`,
/// [`write_tree_blobs`]: no checkout, so no filter driver), plus a PRIVATE git context of its own
/// (`.git`: HEAD the base commit, the source objects as a read-only alternate, an index populated
/// from the base by `read-tree`, an empty hooks dir, no driver config — [`write_private_git_dir`]).
/// A base check's git discovers it first, under a discovery ceiling at the export's parent
/// ([`CheckScratch::apply_env`]), so `git grep` and friends see the base and never the head
/// worktree's index or untracked `tmp/`.
/// Files come from the PINNED git dir (never the worktree's own `.git` file, which the seat could
/// have redirected). Lives at `<worktree>/tmp/` [`BASE_EXPORT_SUBDIR`] — inside the OS
/// write boundary, outside the guard's snapshot — so the sandbox wrapper armed for the worktree
/// covers the base run too. It stays INSIDE the worktree when the rest of the check scratch moved
/// to the engine's check cache (a base check that calls git once discovered the enclosing
/// worktree's repository from there — codex on #1050; the pinned `.git` now makes that context the
/// base's own). The export is SOURCE
/// only (the base's build output goes to the check scratch's `cargo-target/base`) and it is
/// REMOVED as soon as its check result is cached ([`remove_base_export`]), and at the start of
/// every floor: it is a full copy of the repo tree inside the worktree, and a HEAD check that
/// globs from the worktree root (vitest's default include, a broad `tsconfig`, eslint without
/// ignores) would otherwise run over the base's files too. The run pays for a base check once
/// through the base cache (in the run's log store, outside every sandbox write root), never
/// through a persisted export.
pub(crate) struct BaseTree {
    pub(crate) dir: PathBuf,
    /// The export, held from before any command ran in it ([`HeldDir`]): every later parent-side
    /// write or removal in it goes through this.
    pub(crate) held: HeldDir,
    head: String,
}

/// The base export's directory name under the worktree's engine scratch (`tmp/`).
pub const BASE_EXPORT_SUBDIR: &str = "wicked-floor-base";
/// Where the whole check scratch lived before it moved to the engine's check cache
/// (`tmp/wicked-checks`). Nothing is written there; [`remove_base_export`] still removes a base
/// export an older engine left in it.
const LEGACY_SCRATCH_SUBDIR: &str = "wicked-checks";

/// `<worktree>/tmp/` [`BASE_EXPORT_SUBDIR`] — the path the base's commands are spawned in. Only a
/// path: the engine scratch root is repo-adjacent territory (a checkout or a seat could make `tmp`
/// a SYMLINK out of the worktree, adversarial review on #414), so the export is created, written
/// and removed only through the held worktree ([`HeldDir`]), which refuses the link, and every
/// spawn there first checks the path still IS the held export ([`HeldDir::still_at`]).
fn base_export_dir(worktree: &Path) -> PathBuf {
    worktree
        .join(crate::worktree_guard::ENGINE_SCRATCH_DIR)
        .join(BASE_EXPORT_SUBDIR)
}

/// Remove the base export (`<worktree>/tmp/` [`BASE_EXPORT_SUBDIR`]) if present (nothing writes
/// the scratch index an older engine kept; the scratch is the checks' to write, never removed
/// from by name after they ran). Everything goes through `root` — the worktree, held before any
/// check ran there ([`HeldDir`]): the export is removed entry by entry relative to its own
/// directory fds ([`HeldDir::remove_tree`]), so a check (or a process it left running) that swaps
/// `tmp` or the export for a link to an outside directory never has the engine delete through it.
/// A linked or non-directory `tmp`, and a link at the export path, are refused (fail closed).
/// Called at the start of every floor, before an export and as soon as a base check's result is
/// cached, so no HEAD check ever runs beside a copy of the base.
pub(crate) fn remove_base_export(root: &HeldDir) -> Result<(), String> {
    let tmp = crate::worktree_guard::ENGINE_SCRATCH_DIR;
    // Only "not found" is absence (codex on #1050): any other error inspecting a path (a `tmp`
    // without search permission) is an export the floor could not clear, and refuses it.
    match root.kind(tmp)? {
        None => return Ok(()),
        Some(EntryKind::Dir) => {}
        Some(_) => {
            return Err(format!(
                "`{tmp}` is not a real directory — the base export is never written through a link"
            ))
        }
    }
    // (codex on #1050) An engine from before the scratch moved left its export at
    // `tmp/wicked-checks/base` when it died mid-baseline; it removed that at every floor's start,
    // and so does this one — or a HEAD check globbing from the root runs over the old base. Only
    // that export: the rest of a legacy `tmp/wicked-checks` is not this floor's to judge. Never
    // through a link (a linked `wicked-checks` is left alone; `base` is unlinked, not followed).
    let legacy = format!("{tmp}/{LEGACY_SCRATCH_SUBDIR}");
    if root.kind(&legacy)? == Some(EntryKind::Dir) {
        root.remove_tree(&format!("{legacy}/base"))
            .map_err(|e| format!("the legacy base export could not be removed: {e}"))?;
    }
    let export = format!("{tmp}/{BASE_EXPORT_SUBDIR}");
    match root.kind(&export)? {
        None => Ok(()),
        Some(EntryKind::Link) => Err(format!(
            "`{export}` is a symlink (the base export is never written through a link)"
        )),
        Some(_) => root
            .remove_tree(&export)
            .map(drop)
            .map_err(|e| format!("the base export could not be removed: {e}")),
    }
}

impl BaseTree {
    fn cache_path(scratch: &CheckScratch, head: &str, check: &RepoCheck) -> PathBuf {
        // ONE file name, whatever the head, the `dir` (core#817: a subdirectory package's check
        // is its own entry) or the check's `name` (a guide's, repo-controlled) say: a sha256 over
        // the three, NUL-separated (injective, a fixed length, never a path separator). It lives
        // in the run's log store, OUTSIDE every sandbox write root: no check can redirect the
        // engine's write there, or plant a record the next comparison would read. Aged out with
        // the logs ([`crate::floor_log::gc`]).
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for part in [head, check.dir.as_str(), check.name.as_str()] {
            h.update(part.as_bytes());
            h.update([0u8]);
        }
        let key: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        scratch.logs.join(format!("base-{key}.json"))
    }

    /// This run's cached result of `check` on `head`, when an earlier floor of the run paid for it:
    /// a record of THIS policy version ([`FLOOR_POLICY_VERSION`]; an older or bare record is a
    /// miss) for the same argv.
    fn cached(
        scratch: &CheckScratch,
        head: &str,
        check: &RepoCheck,
        guide: Option<&GuideCheck>,
    ) -> Option<BaseSide> {
        let raw = std::fs::read_to_string(Self::cache_path(scratch, head, check)).ok()?;
        let rec = serde_json::from_str::<BaseRecord>(&raw).ok()?;
        // A record taken under another guide (or none: no inventory) is not this comparison.
        if rec.guide != guide_id(guide) {
            return None;
        }
        // The base runs the head's argv, and it can change within a run (a targeted command with
        // the files a rework touches — `{files}`, the derived `-p <crate>` — or a reworked
        // `.wicked/checks.json`): a cached run of ANOTHER argv is not this comparison.
        if rec.floor_policy != FLOOR_POLICY_VERSION || rec.head_argv != check.argv {
            return None;
        }
        // (codex r12) Reused evidence must still be there, whole — and its age restarts, so the
        // log GC keeps what the new receipt names. A blob gone or altered: the base re-runs.
        let toc_logs = rec.toc.iter().flat_map(|t| t.logs.iter());
        if rec
            .run
            .logs
            .iter()
            .chain(toc_logs)
            .any(|l| crate::floor_log::refresh(&scratch.logs, l).is_err())
        {
            return None;
        }
        Some(BaseSide {
            base: BaseRun {
                head: head.to_string(),
                cached: true,
                run: Some(rec.run),
                error: None,
            },
            toc: rec.toc,
        })
    }

    /// Export `head` into a FRESH `<worktree>/tmp/` [`BASE_EXPORT_SUBDIR`], every directory and
    /// file of it created through `root` — `worktree`, held before any check ran there
    /// ([`HeldDir`]): a check (or a process it left running) that re-pointed `tmp` or the export
    /// path at an outside directory refuses the export; it never redirects it.
    pub(crate) fn export(
        worktree: &Path,
        root: &HeldDir,
        git_dir: &Path,
        head: &str,
    ) -> Result<Self, String> {
        let dir = base_export_dir(worktree);
        // Always a fresh export: a stale one is removed, a link refused — never followed.
        remove_base_export(root)?;
        let held = root
            .create_fresh_dir(&format!(
                "{}/{BASE_EXPORT_SUBDIR}",
                crate::worktree_guard::ENGINE_SCRATCH_DIR
            ))
            .map_err(|e| format!("the base export directory could not be created: {e}"))?;
        let why = |e: String| {
            format!(
                "the run base {} could not be exported for the baseline diff: {e}",
                &head[..head.len().min(10)]
            )
        };
        // The base is its own repository (PRODUCT-WORKS J4/J5): a private `.git` — HEAD the base commit,
        // the source objects as a read-only alternate, an empty hooks dir, and no driver config
        // ([`write_private_git_dir`]) — so a git call inside a base check (`git grep`, a
        // `build.rs` running `git rev-parse`) resolves HERE, never to the head worktree and its
        // untracked `tmp/`. Blobs are written through git's own eol conversion against that
        // metadata (no checkout, so no filter driver; the index below touches no file and runs no
        // hook): nothing the repository configures runs outside the floor's sandbox (codex round
        // 17).
        let commit = crate::worktree_guard::git_string(
            worktree,
            &["rev-parse", "--verify", &format!("{head}^{{commit}}")],
            &[("GIT_DIR", git_dir)],
        )
        .map_err(|e| why(e.to_string()))?;
        write_private_git_dir(&held, ".git", git_dir, worktree, &commit, false).map_err(why)?;
        write_tree_blobs(worktree, git_dir, head, &dir, &held, &dir.join(".git")).map_err(why)?;
        // The base's own index, populated from the base tree: nothing of the head's index is
        // ever read. Hooks: the engine's empty directory.
        let hooks = format!("core.hooksPath={}", no_hooks_dir().map_err(why)?.display());
        // spawn-audit: hardened — git plumbing over the private base repository.
        let mut read_tree = Command::new("git");
        read_tree
            .hardened()
            .args(["-c", &hooks, "read-tree", &commit])
            .current_dir(&dir)
            .env("GIT_DIR", dir.join(".git"))
            .env("GIT_WORK_TREE", &dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        isolate_git_config(&mut read_tree);
        let out = read_tree
            .output()
            .map_err(|e| why(format!("git read-tree: {e}")))?;
        if !out.status.success() {
            return Err(why(format!(
                "the base's index could not be set: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(Self {
            dir,
            held,
            head: head.to_string(),
        })
    }

    /// Run `check` on the base with the HEAD's argv, once the base declares a check of that name
    /// (a baseline diff compares one command on two trees: a change that edits the command — say
    /// adds `--no-fail-fast` — would otherwise read the base's shorter failure list as the head's
    /// regressions; a targeted command is the same change-scoped set on both trees) — and cache
    /// the result under the log store's `base-<sha256 of head, dir, name>.json` for the rest of the
    /// run ([`Self::cached`]). The base's install step runs first when its tree needs
    /// provisioning (the same frozen, scripts-off install the head got, out of the same scratch
    /// cache).
    fn run_check(
        &self,
        check: &RepoCheck,
        ctx: &FloorContext,
        sandbox: &WorkerSandbox,
        scratch: &CheckScratch,
        guide: Option<&GuideCheck>,
    ) -> BaseSide {
        let cache = Self::cache_path(scratch, &self.head, check);
        let fail = |e: String| BaseSide {
            base: BaseRun {
                head: self.head.clone(),
                cached: false,
                run: None,
                error: Some(e),
            },
            toc: None,
        };
        // No base of its own: the export is not a worktree, so `{files}`/`{base}` cannot be
        // re-derived there — the head's targeted argv is reused verbatim below.
        let base_ctx = FloorContext {
            stage: ctx.stage,
            ..FloorContext::default()
        };
        // (core#817) A subdirectory package's check is detected in the BASE's copy of that
        // directory; a package the change introduced has none, and fails closed below.
        let detected = match detect_with(&self.dir.join(&check.dir), &base_ctx) {
            Ok(d) => d,
            Err(e) => return fail(format!("the base's checks could not be determined: {e}")),
        };
        // Detection reads the export by path (each file `O_NOFOLLOW`, its identity checked,
        // [`probe`]): what it read was the export only while the package's directory still IS the
        // held one — a process the head left running that swapped it for a link fails the base.
        if let Err(e) = self.held.still_at_rel(&self.dir, &check.dir) {
            return fail(format!("the base's checks could not be determined: {e}"));
        }
        // A guide's check is the base's by construction: the guide is read from the base.
        if check.name != "test_targeted"
            && check.source != GUIDE_SOURCE
            && !detected.iter().any(|c| c.name == check.name)
        {
            return fail(format!(
                "the run base declares no `{}` check (the change introduced it)",
                check.name
            ));
        }
        let target = check.clone();
        if let Some(install) = detected.iter().find(|c| c.name == "install") {
            let install = RepoCheck {
                dir: check.dir.clone(),
                ..install.clone()
            };
            let r = run_held(
                &self.dir,
                Some(&self.held),
                &install,
                sandbox,
                scratch,
                Tree::Base,
            );
            if !r.passed() {
                return fail(format!(
                    "the base's dependency install did not pass ({})",
                    r.summary()
                ));
            }
        }
        // (DES-BUILD-GUIDE) With a usable guide, the base runs with its results on (`how`); a
        // red base is listed by the guide's `list`, its results nominate the failing ids the list
        // holds, and each is confirmed by its EXIT CODE run alone (`failedExit`). A misread result
        // can then only narrow what is excluded.
        if let Some(g) = guide {
            clear_results(&self.held, &target, g);
        }
        let since = std::time::SystemTime::now();
        let mut run = run_one_with(
            &self.dir,
            Some(&self.held),
            &RepoCheck {
                argv: guide.map_or_else(|| target.argv.clone(), |g| g.for_results(&target.argv)),
                ..target.clone()
            },
            &guide.map(results_exec).unwrap_or_default(),
            sandbox,
            scratch,
            Tree::Base,
        );
        let mut toc = None;
        if let Some(g) =
            guide.filter(|_| !run.passed() && !run.timed_out && run.spawn_error.is_none())
        {
            // (DES-BUILD-GUIDE) A base that exited outside the guide's `failedExit` did not run to
            // a test verdict (a crash, a build failure): nothing of it is inventoried, and nothing
            // of it may be excluded.
            let verdict = run
                .exit_code
                .is_some_and(|c| c != 0 && g.failed_exit.contains(&c));
            // The results first: the list must not overwrite them.
            let results =
                verdict.then(|| results_of(&self.held, &target, g, &mut run, scratch, since));
            let mut t = match results {
                Some(_) => inventory(
                    &self.dir,
                    Some(&self.held),
                    &target,
                    g,
                    &[],
                    sandbox,
                    scratch,
                    Tree::Base,
                ),
                None => Toc {
                    incomplete: Some(format!(
                        "the base exited {} — not one of the guide's `failedExit` codes {:?}: it \
                         did not run to a test verdict",
                        run.exit_code
                            .map_or_else(|| "without a code".to_string(), |c| c.to_string()),
                        g.failed_exit
                    )),
                    ..Toc::default()
                },
            };
            match results.filter(|_| t.incomplete.is_none()) {
                Some(Ok((_, failed))) => {
                    t.failing = failed.into_iter().filter(|x| t.count(x) > 0).collect()
                }
                Some(Err(e)) => t.incomplete = Some(format!("the base's results: {e}")),
                None => {}
            }
            let candidates: Vec<TestId> = t
                .failing
                .iter()
                .filter(|x| t.count(x) == 1)
                .cloned()
                .collect();
            if t.incomplete.is_none() && candidates.len() <= MAX_ALONE_CONFIRMATIONS {
                for n in candidates {
                    let one = RepoCheck {
                        argv: g.alone_argv(&n),
                        source: format!("`{}` alone on the base (DES-BUILD-GUIDE)", n.id),
                        ..target.clone()
                    };
                    let r = run_one_with(
                        &self.dir,
                        Some(&self.held),
                        &one,
                        &ExecOpts::default(),
                        sandbox,
                        scratch,
                        Tree::Base,
                    );
                    t.logs.extend(relabel(r.logs.clone(), "alone-base"));
                    // (agy r6) One that did not finish or start confirms nothing: the
                    // inventory is incomplete (escalates) and not cached.
                    if r.timed_out || r.spawn_error.is_some() {
                        t.incomplete = Some(format!(
                            "`{}` run alone on the base did not finish ({})",
                            n.id,
                            r.summary()
                        ));
                        t.transient = true;
                        break;
                    }
                    // A NONZERO exit the guide calls a test failure: a pass confirms nothing.
                    let failed = r
                        .exit_code
                        .is_some_and(|c| c != 0 && g.failed_exit.contains(&c));
                    t.alone.push((n, failed));
                }
            }
            toc = Some(t);
        }
        let rec = BaseRecord {
            floor_policy: FLOOR_POLICY_VERSION,
            head_argv: check.argv.clone(),
            guide: guide_id(guide),
            run,
            toc,
        };
        // A base run (or its inventory, or an alone run) that did not finish or could not start is
        // not cached: it says nothing lasting.
        if !rec.run.timed_out
            && rec.run.spawn_error.is_none()
            && !rec.toc.as_ref().is_some_and(|t| t.transient)
        {
            let _ = std::fs::create_dir_all(&scratch.logs);
            if let Ok(json) = serde_json::to_string(&rec) {
                let _ = std::fs::write(&cache, json);
            }
        }
        BaseSide {
            base: BaseRun {
                head: self.head.clone(),
                cached: false,
                run: Some(rec.run),
                error: None,
            },
            toc: rec.toc,
        }
    }
}

/// A guide check's identity for the base cache: 12 hex of its serialized form.
fn guide_id(g: Option<&GuideCheck>) -> Option<String> {
    g.map(|g| hex12(&serde_json::to_vec(g).unwrap_or_default()))
}

/// The base cache's record: the base run and (a red base) its inventory, under the policy version.
#[derive(Debug, Serialize, Deserialize)]
struct BaseRecord {
    floor_policy: u32,
    /// The head argv the base answered (its run may be narrowed to the base's packages).
    #[serde(default)]
    head_argv: Vec<String>,
    /// The usable guide check the base ran under ([`guide_id`]), or none.
    #[serde(default)]
    guide: Option<String>,
    run: CheckRun,
    #[serde(default)]
    toc: Option<Toc>,
}

// ─── DES-FLOOR-V2: the suite's TOC plus exit codes ─────────────────────────────────────────────

/// (DES-FLOOR-V2) The floor's machine-readable decision for one check (`checks[].decision` on
/// `repoChecksEvaluated`). Only exit codes of processes the floor launched, the base tree and the
/// base's inventory decide it; head output is never read to decide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckDecision {
    /// [`OUTCOME_PASS`] | [`OUTCOME_PASS_EXCLUDED`] | [`OUTCOME_NEW_FAILURE`] |
    /// [`OUTCOME_BASE_RED_UNSEPARATED`] | [`OUTCOME_FLAKY_TWICE`] | [`OUTCOME_TIMED_OUT`] |
    /// [`OUTCOME_COULD_NOT_RUN`]. Only the first two pass.
    pub outcome: String,
    /// The base commit the check was compared with, when the base ran (or was read from cache).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// The base's failing tests that appear VERBATIM in the base's own inventory (its TOC) — the
    /// only ids the floor may exclude.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base_failing: Vec<String>,
    /// The names actually deselected on the head's excluded run (a base failure the head no longer
    /// has is not passed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<String>,
    /// Every stream of every process this decision ran, by hash.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log_refs: Vec<crate::floor_log::LogRef>,
    /// The deciding run failed on an oversubscribed host and passed its identical retry:
    /// `passed on retry (flaky)`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub flaky: bool,
    /// Why, in words: what was excluded, why the base red could not be separated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// (DES-BUILD-GUIDE) The base's build guide for this check: present, usable, and why not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide: Option<GuideStatus>,
}

/// The check exited 0 (on its first run, or on its identical retry — `flaky`).
pub const OUTCOME_PASS: &str = "pass";
/// The head exited 0 once the base's own failing tests were deselected natively.
pub const OUTCOME_PASS_EXCLUDED: &str = "pass_excluded";
/// The deciding run exited non-zero and no base failure explains it — denies.
pub const OUTCOME_NEW_FAILURE: &str = "new_failure";
/// The base is red and its failures cannot be excluded precisely — escalates (denies).
pub const OUTCOME_BASE_RED_UNSEPARATED: &str = "base_red_unseparated";
/// The deciding run failed on an oversubscribed host and failed again on its identical retry, which
/// also ran oversubscribed (the bounded wait found no headroom) — denies; not shown to be a flake,
/// only that load was present both times. A retry WITH headroom that fails is a new failure.
pub const OUTCOME_FLAKY_TWICE: &str = "flaky_twice";
/// A run the decision needed did not finish within its bound — denies under the timeout source.
pub const OUTCOME_TIMED_OUT: &str = "timed_out";
/// The check could not be started (not on PATH, the launcher could not arm) — denies.
pub const OUTCOME_COULD_NOT_RUN: &str = "could_not_run";
/// (DES-FLOOR-V2) The `classification` token of a base-red check whose failures cannot be
/// excluded precisely. Denies.
pub const BASE_RED_UNSEPARATED: &str = "base_red_unseparated";
/// The versioned base-cache record (core#1060 r9 keyed the cache by sandbox policy; v4 adds the
/// base inventory): a record of any other version, or a bare `CheckRun`, is a miss.
const FLOOR_POLICY_VERSION: u32 = 13;
/// The most base failures the floor confirms one by one (each run alone on the base); more than
/// this escalates rather than paying for them.
const MAX_ALONE_CONFIRMATIONS: usize = 16;
/// The longest the identical retry waits for the 1-min load to fall below the CPU count, by
/// default; [`RETRY_WAIT_ENV`] sets it in whole seconds (at most [`MAX_RETRY_WAIT_S`]).
#[cfg_attr(test, allow(dead_code))]
const RETRY_HEADROOM_WAIT: Duration = Duration::from_secs(30);
/// The operator's (or a test harness's) bound on the retry's headroom wait, in seconds.
#[cfg_attr(test, allow(dead_code))]
pub const RETRY_WAIT_ENV: &str = "WICKED_REPO_CHECKS_RETRY_WAIT_S";
/// The most [`RETRY_WAIT_ENV`] may ask for.
#[cfg_attr(test, allow(dead_code))]
pub const MAX_RETRY_WAIT_S: u64 = 600;

/// The inventory of `check` on one tree, from the guide's `list` piece (never from a test's
/// output): every id it lists, with its qualifier.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Toc {
    pub(crate) ids: Vec<TestId>,
    /// Why the inventory is not complete (non-zero exit, unreadable, truncated, empty); `None`
    /// when the list ran clean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) incomplete: Option<String>,
    #[serde(default)]
    logs: Vec<crate::floor_log::LogRef>,
    /// The base's failing ids its results name that its list holds: CANDIDATES only, until
    /// [`Toc::alone`] confirms them.
    #[serde(default)]
    failing: Vec<TestId>,
    /// Each candidate run ALONE on the base: `(id, it failed)` — by exit code (`failedExit`).
    #[serde(default)]
    alone: Vec<(TestId, bool)>,
    /// The inventory or an alone run did not finish or could not start: never cached.
    #[serde(skip)]
    transient: bool,
}

impl Toc {
    fn count(&self, t: &TestId) -> usize {
        self.ids.iter().filter(|i| *i == t).count()
    }

    /// The qualifiers `id` is listed under.
    fn qs(&self, id: &str) -> Vec<&str> {
        self.ids
            .iter()
            .filter(|i| i.id == id)
            .map(|i| i.q.as_str())
            .collect()
    }
}

/// The options of a run whose RESULTS the floor reads: the guide's `results.how` env and merge.
/// No other run (a list, a one-test run, the deselected run, the version) gets that env.
pub(crate) fn results_exec(g: &GuideCheck) -> ExecOpts {
    ExecOpts {
        env: g
            .results
            .how
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        merge: g.results.how.merge,
    }
}

/// A run that only merges its streams (or not), with no guide env.
fn merged(merge: bool) -> ExecOpts {
    ExecOpts {
        env: Vec::new(),
        merge,
    }
}

/// Run the guide's `list` for `check` on one tree (with `ids` deselected, when given) and read
/// its ids.
#[allow(clippy::too_many_arguments)]
pub(crate) fn inventory(
    dir: &Path,
    held: Option<&HeldDir>,
    check: &RepoCheck,
    g: &GuideCheck,
    ids: &[TestId],
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
    tree: Tree,
) -> Toc {
    let (c, list) = match (g.compile(), g.list.as_ref()) {
        (Ok(c), Some(l)) => (c, l),
        (Err(e), _) => {
            return Toc {
                incomplete: Some(e),
                ..Toc::default()
            }
        }
        (Ok(_), None) => unreachable!("compile requires a list"),
    };
    let probe = RepoCheck {
        argv: if ids.is_empty() {
            list.run.clone()
        } else {
            g.deselected(&list.run, ids)
        },
        source: format!("the list of `{}` (DES-BUILD-GUIDE)", check.name),
        ..check.clone()
    };
    let r = run_one_with(
        dir,
        held,
        &probe,
        &merged(list.merge),
        sandbox,
        scratch,
        tree,
    );
    let attempt = match (tree, ids.is_empty()) {
        (Tree::Base, _) => "toc-base",
        (Tree::Head, true) => "toc-head",
        (Tree::Head, false) => "toc-excluded",
    };
    let logs = relabel(r.logs.clone(), attempt);
    let text = logs
        .first()
        .ok_or_else(|| "no stored output".to_string())
        .and_then(|l| crate::floor_log::read_blob(&scratch.logs, l))
        .map(|b| String::from_utf8_lossy(&b).into_owned());
    let deselecting = !ids.is_empty();
    let ids = text.as_deref().map(|t| c.list(t)).unwrap_or_default();
    let incomplete = if let Err(e) = &text {
        Some(format!("its list could not be read back ({e})"))
    } else if !r.passed() {
        Some(format!("its list did not exit 0 ({})", r.summary()))
    } else if logs.iter().any(|l| l.truncated) {
        Some("its list was cut at the log cap".to_string())
    } else if ids.is_empty() && !deselecting {
        Some("its list names no test".to_string())
    } else {
        None
    };
    Toc {
        ids,
        incomplete,
        logs,
        failing: Vec::new(),
        alone: Vec::new(),
        transient: r.timed_out || r.spawn_error.is_some(),
    }
}

/// A results run's ids per the guide: every testcase (junit; text names failures only) and the
/// failing ones. The results file (`how.path`, relative to the check's directory) must be a
/// regular file inside the tree, written since `since` (the run's start), within the log cap; it
/// is stored in the log store beside the run's streams. Anything else is no results.
pub(crate) fn results_of(
    held: &HeldDir,
    check: &RepoCheck,
    g: &GuideCheck,
    run: &mut CheckRun,
    scratch: &CheckScratch,
    since: std::time::SystemTime,
) -> Result<(Vec<TestId>, Vec<TestId>), String> {
    let c = g.compile()?;
    let r = match &g.results.how.path {
        Some(path) => {
            let f = open_results(
                held,
                &Path::new(&check.dir).join(path).to_string_lossy(),
                since,
            )
            .map_err(|e| format!("`{path}`: {e}"))?;
            let cap =
                crate::floor_log::capture(f, &scratch.logs, scratch.log_cap).finish(DRAIN_CAP);
            if let Some(e) = cap.error {
                return Err(e);
            }
            let r = cap.log_ref("results", "file");
            run.logs.push(r.clone());
            r
        }
        None => run
            .logs
            .first()
            .cloned()
            .ok_or_else(|| "the run has no stored output".to_string())?,
    };
    if r.truncated {
        return Err("they were cut at the log cap".into());
    }
    let text =
        String::from_utf8_lossy(&crate::floor_log::read_blob(&scratch.logs, &r)?).into_owned();
    match g.results.kind.as_str() {
        "junit" => crate::floor_guide::junit(
            &text,
            &g.results.id,
            g.results.id_join.as_deref().unwrap_or("::"),
            c.keyed(),
        )
        .ok_or_else(|| "the results hold no testsuite".to_string()),
        _ => Ok((Vec::new(), c.failures(&text, c.keyed()))),
    }
}

/// The operator's switch for excluding a red base's own failures: `on` / `1` enables it; anything
/// else, or unset, leaves it OFF — a red base then escalates as `base_red_unseparated` with both
/// logs (the operator's floor override is the path through), and neither the base's list nor any
/// one-test run, nor a deselected head run, is executed. A usable guide still names the checks.
pub const FLOOR_EXCLUDE_ENV: &str = "WICKED_FLOOR_EXCLUDE";

/// Is exclusion on ([`FLOOR_EXCLUDE_ENV`])? Read once per floor run.
pub(crate) fn exclusion_enabled() -> bool {
    #[cfg(test)]
    if let Some(on) = test_seam::exclusion() {
        return on;
    }
    exclusion_from(std::env::var(FLOOR_EXCLUDE_ENV).ok().as_deref())
}

/// The switch's value: exactly `on` or `1` (never trimmed, never case-folded) enables it.
fn exclusion_from(value: Option<&str>) -> bool {
    matches!(value, Some("on") | Some("1"))
}

/// (DES-BUILD-GUIDE) `rel` under `root`, refused unless it is relative with no `..`, and no
/// component of it that exists is a symlink: a path a guide names never leaves the tree.
// Without directory fds (Windows) the held directory stands on it.
#[cfg_attr(unix, allow(dead_code))]
pub(crate) fn confined(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let mut out = root.to_path_buf();
    for c in Path::new(rel).components() {
        match c {
            std::path::Component::Normal(n) => out.push(n),
            std::path::Component::CurDir => continue,
            _ => return Err(format!("`{rel}` must stay inside the tree")),
        }
        if std::fs::symlink_metadata(&out).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("`{rel}` passes through a symlink"));
        }
    }
    Ok(out)
}

/// Open options that never follow a final symlink, nor block on a FIFO (unix).
// Without directory fds (Windows) the held directory stands on it.
#[cfg_attr(unix, allow(dead_code))]
fn no_follow(o: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    o
}

/// How far a file system's coarse clock may stamp a fresh file before the floor's `now`.
const FS_CLOCK_SLACK: Duration = Duration::from_secs(1);

/// Remove a results file (`results.how.path`) a run is about to write, so whatever is there after
/// the run is that run's.
pub(crate) fn clear_results(held: &HeldDir, check: &RepoCheck, g: &GuideCheck) {
    if let Some(path) = &g.results.how.path {
        let _ = held.remove_file(&Path::new(&check.dir).join(path).to_string_lossy());
    }
}

/// The results file `rel` in the held tree `held` ([`HeldDir::open_read`]: its parents walked
/// through no link, the file opened `O_NOFOLLOW | O_NONBLOCK`), and — on the OPENED file, never
/// the path — a regular file within the log cap, modified since `since` (less
/// [`FS_CLOCK_SLACK`]; the run cleared it first, [`clear_results`]). A command that renames the
/// tree away and leaves a link, plants a FIFO, or swaps the file after a check, gets nothing read
/// from outside the tree.
fn open_results(
    held: &HeldDir,
    rel: &str,
    since: std::time::SystemTime,
) -> Result<std::fs::File, String> {
    let f = held
        .open_read(rel)
        .map_err(|e| format!("no results file ({e})"))?;
    let m = f.metadata().map_err(|e| format!("no results file ({e})"))?;
    if !m.is_file() {
        return Err("not a regular file".into());
    }
    if m.len() > crate::floor_log::MAX_STREAM_BYTES {
        return Err("larger than the log cap".into());
    }
    if m.modified().map_or(true, |t| t + FS_CLOCK_SLACK < since) {
        return Err("not written by this run".into());
    }
    Ok(f)
}

/// A tree the parent process writes into AFTER repository code may have run there, or while a
/// process it started may still be running (a probe copy, the base export, the head worktree, the
/// guarded checkout). The root is opened ONCE, before any command runs in it (`O_DIRECTORY |
/// O_NOFOLLOW`), and every write, directory creation or removal walks from that held fd,
/// component by component with `O_NOFOLLOW`: a command that swaps the root, or any directory
/// below it, for a link to an outside directory cannot redirect the parent — the walk refuses the
/// link, or lands in the original (now detached) directory. A file is opened non-blocking and
/// must be a regular file (a planted FIFO neither stalls nor receives the write). Without
/// directory fds (Windows) the path checks of [`confined`] stand in.
pub(crate) struct HeldDir {
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
    #[cfg(not(unix))]
    path: PathBuf,
}

/// `rel`'s components: plain names only (no root, `..`, or empty name; `.` skipped).
fn rel_names(rel: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for c in Path::new(rel).components() {
        match c {
            std::path::Component::Normal(n) => out.push(
                n.to_str()
                    .ok_or_else(|| format!("`{rel}` is not UTF-8"))?
                    .to_string(),
            ),
            std::path::Component::CurDir => {}
            _ => return Err(format!("`{rel}` must stay inside the tree")),
        }
    }
    if out.is_empty() {
        return Err(format!("`{rel}` names no file"));
    }
    Ok(out)
}

/// A directory opened through no link, close-on-exec.
#[cfg(unix)]
const HELD_DIR_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

#[cfg(unix)]
fn cstr(b: &[u8]) -> Result<std::ffi::CString, String> {
    std::ffi::CString::new(b).map_err(|_| "a path holds a NUL byte".to_string())
}

/// Own a just-returned fd, or the OS error that `-1` reported.
#[cfg(unix)]
fn owned_fd(fd: libc::c_int) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened by the caller and is owned by nothing else.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// Why `name` in `dir` was refused: "a symlink" when that is what is there (never followed),
/// else the OS error.
#[cfg(unix)]
fn held_refusal(
    dir: &std::os::fd::OwnedFd,
    name: &std::ffi::CStr,
    rel: &str,
    err: std::io::Error,
) -> String {
    use std::os::fd::AsRawFd;
    // SAFETY: zeroed `stat` is a valid out-parameter.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `dir` is an open directory fd; `name` is NUL-terminated; `st` is writable.
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFLNK {
        format!("`{rel}` passes through a symlink (never followed)")
    } else {
        format!("`{rel}`: {err}")
    }
}

#[cfg(unix)]
impl HeldDir {
    /// Hold `root` — call it before any command runs there.
    pub(crate) fn open(root: &Path) -> Result<Self, String> {
        use std::os::unix::ffi::OsStrExt;
        let c = cstr(root.as_os_str().as_bytes())?;
        // SAFETY: a valid NUL-terminated path.
        owned_fd(unsafe { libc::open(c.as_ptr(), HELD_DIR_FLAGS) })
            .map(|fd| Self { fd })
            .map_err(|e| format!("`{}` could not be held: {e}", root.display()))
    }

    /// The directory `names` name, walked from the held root through no link; a missing one is
    /// created when `create`.
    fn walk(
        &self,
        names: &[String],
        rel: &str,
        create: bool,
    ) -> Result<std::os::fd::OwnedFd, String> {
        use std::os::fd::AsRawFd;
        let mut cur = self.fd.try_clone().map_err(|e| e.to_string())?;
        for n in names {
            let c = cstr(n.as_bytes())?;
            // SAFETY: `cur` is an open directory fd; `c` is NUL-terminated.
            let mut next =
                owned_fd(unsafe { libc::openat(cur.as_raw_fd(), c.as_ptr(), HELD_DIR_FLAGS) });
            if create && matches!(&next, Err(e) if e.kind() == std::io::ErrorKind::NotFound) {
                // SAFETY: as above. A racing creator is fine: the open below decides.
                unsafe { libc::mkdirat(cur.as_raw_fd(), c.as_ptr(), 0o777) };
                // SAFETY: as above.
                next =
                    owned_fd(unsafe { libc::openat(cur.as_raw_fd(), c.as_ptr(), HELD_DIR_FLAGS) });
            }
            cur = next.map_err(|e| held_refusal(&cur, &c, rel, e))?;
        }
        Ok(cur)
    }

    /// The directory at `rel`, held (missing directories created when `create`), never through a
    /// link.
    pub(crate) fn subdir(&self, rel: &str, create: bool) -> Result<HeldDir, String> {
        let names = rel_names(rel)?;
        self.walk(&names, rel, create).map(|fd| HeldDir { fd })
    }

    /// The directory at `rel` (`rel` empty or `.`: the held root itself) as an open fd, walked
    /// through no link — what a check's child enters with `fchdir`.
    pub(crate) fn dir_fd(&self, rel: &str) -> Result<std::os::fd::OwnedFd, String> {
        if rel.is_empty() || rel == "." {
            return self.fd.try_clone().map_err(|e| e.to_string());
        }
        self.subdir(rel, false).map(|d| d.fd)
    }

    /// A NEW directory at `rel` (its parents created as needed), held: one already there — a
    /// leftover, or one a running command made — is refused, never reused.
    pub(crate) fn create_fresh_dir(&self, rel: &str) -> Result<HeldDir, String> {
        use std::os::fd::AsRawFd;
        let names = rel_names(rel)?;
        let (leaf, parents) = names.split_last().expect("rel_names is never empty");
        let dir = self.walk(parents, rel, true)?;
        let c = cstr(leaf.as_bytes())?;
        // SAFETY: `dir` is an open directory fd; `c` is NUL-terminated.
        if unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o777) } != 0 {
            return Err(format!("`{rel}`: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: as above.
        owned_fd(unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), HELD_DIR_FLAGS) })
            .map(|fd| HeldDir { fd })
            .map_err(|e| held_refusal(&dir, &c, rel, e))
    }

    /// Write `content` at `rel` (missing directories created), never through a link.
    pub(crate) fn write(&self, rel: &str, content: &str) -> Result<(), String> {
        self.write_bytes(rel, content.as_bytes(), false)
    }

    /// Write `body` at `rel` (missing directories created; mode 0755 when `exec`, else the
    /// umask's default), never through a link, and only into a regular file.
    pub(crate) fn write_bytes(&self, rel: &str, body: &[u8], exec: bool) -> Result<(), String> {
        use std::os::fd::AsRawFd;
        let names = rel_names(rel)?;
        let (leaf, parents) = names.split_last().expect("rel_names is never empty");
        let dir = self.walk(parents, rel, true)?;
        let c = cstr(leaf.as_bytes())?;
        let flags = libc::O_WRONLY
            | libc::O_CREAT
            | libc::O_TRUNC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC;
        // SAFETY: `dir` is an open directory fd; `c` is NUL-terminated.
        let fd = owned_fd(unsafe {
            libc::openat(dir.as_raw_fd(), c.as_ptr(), flags, 0o666 as libc::c_uint)
        })
        .map_err(|e| held_refusal(&dir, &c, rel, e))?;
        let mut f = std::fs::File::from(fd);
        if !f.metadata().map_err(|e| e.to_string())?.is_file() {
            return Err(format!("`{rel}` is not a regular file"));
        }
        if exec {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("`{rel}`: {e}"))?;
        }
        std::io::Write::write_all(&mut f, body).map_err(|e| format!("`{rel}`: {e}"))
    }

    /// A symbolic link at `rel` pointing at `target` (written as is, never resolved; missing
    /// directories created).
    pub(crate) fn symlink(&self, rel: &str, target: &[u8]) -> Result<(), String> {
        use std::os::fd::AsRawFd;
        let names = rel_names(rel)?;
        let (leaf, parents) = names.split_last().expect("rel_names is never empty");
        let dir = self.walk(parents, rel, true)?;
        let (c, t) = (cstr(leaf.as_bytes())?, cstr(target)?);
        // SAFETY: `dir` is an open directory fd; both strings are NUL-terminated.
        if unsafe { libc::symlinkat(t.as_ptr(), dir.as_raw_fd(), c.as_ptr()) } != 0 {
            return Err(format!("`{rel}`: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Remove the regular file at `rel` (a link or a directory there is left alone); `true` when
    /// one was removed.
    pub(crate) fn remove_file(&self, rel: &str) -> Result<bool, String> {
        use std::os::fd::AsRawFd;
        let names = rel_names(rel)?;
        let (leaf, parents) = names.split_last().expect("rel_names is never empty");
        let Ok(dir) = self.walk(parents, rel, false) else {
            return Ok(false);
        };
        let c = cstr(leaf.as_bytes())?;
        // SAFETY: zeroed `stat` is a valid out-parameter.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `dir` is an open directory fd; `c` is NUL-terminated; `st` is writable.
        let rc = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                c.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 || (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
            return Ok(false);
        }
        // SAFETY: as above.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), 0) } != 0 {
            return Err(format!("`{rel}`: {}", std::io::Error::last_os_error()));
        }
        Ok(true)
    }

    /// What is at `rel` (its parents walked through no link), never following a link there;
    /// `None` when nothing is.
    pub(crate) fn kind(&self, rel: &str) -> Result<Option<EntryKind>, String> {
        let names = rel_names(rel)?;
        let (leaf, parents) = names.split_last().expect("rel_names is never empty");
        let dir = self
            .walk(parents, rel, false)
            .map_err(|e| format!("cannot be inspected: {e}"))?;
        let c = cstr(leaf.as_bytes())?;
        match stat_at(&dir, &c) {
            Ok(st) => Ok(Some(entry_kind(st.st_mode))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("`{rel}` cannot be inspected: {e}")),
        }
    }

    /// Remove whatever is at `rel`, a directory recursively, every entry relative to its own
    /// directory's fd: a directory below is opened `O_NOFOLLOW` and emptied, a link (or anything
    /// else) is unlinked — never followed, so a command that swaps a directory of the tree for a
    /// link to an outside one has the LINK removed, never the outside directory. `true` when
    /// something was there.
    pub(crate) fn remove_tree(&self, rel: &str) -> Result<bool, String> {
        let names = rel_names(rel)?;
        let (leaf, parents) = names.split_last().expect("rel_names is never empty");
        let dir = self.walk(parents, rel, false)?;
        let c = cstr(leaf.as_bytes())?;
        match stat_at(&dir, &c) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(format!("`{rel}` cannot be inspected: {e}")),
            Ok(_) => {}
        }
        remove_entry(&dir, &c, 0).map_err(|e| format!("`{rel}`: {e}"))?;
        Ok(true)
    }

    /// The regular file at `rel` opened for reading (its parents walked through no link; the file
    /// itself `O_NOFOLLOW | O_NONBLOCK`): what the caller then checks is the OPENED file, never a
    /// path a command could re-point after the check.
    pub(crate) fn open_read(&self, rel: &str) -> Result<std::fs::File, String> {
        use std::os::fd::AsRawFd;
        let names = rel_names(rel)?;
        let (leaf, parents) = names.split_last().expect("rel_names is never empty");
        let dir = self.walk(parents, rel, false)?;
        let c = cstr(leaf.as_bytes())?;
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        // SAFETY: `dir` is an open directory fd; `c` is NUL-terminated.
        owned_fd(unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags) })
            .map(std::fs::File::from)
            .map_err(|e| held_refusal(&dir, &c, rel, e))
    }

    /// The target of the symbolic link at `rel` (its parents walked through no link), as stored.
    pub(crate) fn read_link(&self, rel: &str) -> Result<Vec<u8>, String> {
        use std::os::fd::AsRawFd;
        let names = rel_names(rel)?;
        let (leaf, parents) = names.split_last().expect("rel_names is never empty");
        let dir = self.walk(parents, rel, false)?;
        let c = cstr(leaf.as_bytes())?;
        let mut buf = vec![0u8; 4096];
        // SAFETY: `dir` is an open directory fd; `c` is NUL-terminated; `buf` is writable for
        // its length.
        let n = unsafe {
            libc::readlinkat(
                dir.as_raw_fd(),
                c.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if n < 0 {
            return Err(format!("`{rel}`: {}", std::io::Error::last_os_error()));
        }
        buf.truncate(n as usize);
        Ok(buf)
    }

    /// Is `path` still the held directory — a real directory (not a link) with the held fd's
    /// device and inode? Checked before a command is spawned in the tree by path: a command that
    /// replaced the tree's root with a link to an outside directory gets no later command started
    /// there.
    pub(crate) fn still_at(&self, path: &Path) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(path)
            .map_err(|e| format!("`{}` cannot be inspected: {e}", path.display()))?;
        let held = std::fs::File::from(self.fd.try_clone().map_err(|e| e.to_string())?)
            .metadata()
            .map_err(|e| e.to_string())?;
        if m.file_type().is_symlink()
            || !m.is_dir()
            || (m.dev(), m.ino()) != (held.dev(), held.ino())
        {
            return Err(format!(
                "`{}` is no longer the directory the floor held (replaced after a command ran)",
                path.display()
            ));
        }
        Ok(())
    }
}

impl HeldDir {
    /// `root` — the path this directory was held at — and `root/rel` (`rel` empty: the root
    /// alone) still name the held directory and its `rel`, walked from the held fd through no
    /// link ([`HeldDir::still_at`]).
    pub(crate) fn still_at_rel(&self, root: &Path, rel: &str) -> Result<(), String> {
        self.still_at(root)?;
        if rel.is_empty() || rel == "." {
            return Ok(());
        }
        self.subdir(rel, false)?.still_at(&root.join(rel))
    }
}

/// What a directory entry is, never following a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    Dir,
    File,
    Link,
    Other,
}

#[cfg(unix)]
fn entry_kind(mode: libc::mode_t) -> EntryKind {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => EntryKind::Dir,
        libc::S_IFREG => EntryKind::File,
        libc::S_IFLNK => EntryKind::Link,
        _ => EntryKind::Other,
    }
}

/// `fstatat(dir, name, AT_SYMLINK_NOFOLLOW)`.
#[cfg(unix)]
fn stat_at(dir: &impl std::os::fd::AsRawFd, name: &std::ffi::CStr) -> std::io::Result<libc::stat> {
    // SAFETY: zeroed `stat` is a valid out-parameter.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `dir` is an open directory fd; `name` is NUL-terminated; `st` is writable.
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(st)
}

/// How deep [`HeldDir::remove_tree`] descends before it gives up (a bound on open fds).
#[cfg(unix)]
const MAX_REMOVE_DEPTH: usize = 256;

/// `unlinkat(dir, name, flags)`, a missing entry being gone already.
#[cfg(unix)]
fn unlink_at(
    dir: &impl std::os::fd::AsRawFd,
    name: &std::ffi::CStr,
    flags: libc::c_int,
) -> std::io::Result<()> {
    // SAFETY: `dir` is an open directory fd; `name` is NUL-terminated.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(e);
        }
    }
    Ok(())
}

/// Remove `name` in `dir`: a directory is opened through no link, emptied and removed; anything
/// else (a link included) is unlinked, never followed.
#[cfg(unix)]
fn remove_entry(
    dir: &impl std::os::fd::AsRawFd,
    name: &std::ffi::CStr,
    depth: usize,
) -> std::io::Result<()> {
    let st = match stat_at(dir, name) {
        Ok(st) => st,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if entry_kind(st.st_mode) != EntryKind::Dir {
        return unlink_at(dir, name, 0);
    }
    // SAFETY: `dir` is an open directory fd; `name` is NUL-terminated.
    match owned_fd(unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), HELD_DIR_FLAGS) }) {
        Ok(sub) => {
            empty_dir(&sub, depth + 1)?;
            unlink_at(dir, name, libc::AT_REMOVEDIR)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        // Swapped for a link (or a file) since the stat: unlinked, never followed.
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => {
            unlink_at(dir, name, 0)
        }
        Err(e) => Err(e),
    }
}

/// Remove every entry of the directory `dir` (a few passes: a process still writing there may
/// add one; whatever remains makes the caller's `rmdir` fail, and that is reported).
#[cfg(unix)]
fn empty_dir(dir: &std::os::fd::OwnedFd, depth: usize) -> std::io::Result<()> {
    if depth > MAX_REMOVE_DEPTH {
        return Err(std::io::Error::other("the tree is too deep to remove"));
    }
    for _ in 0..3 {
        let names = dir_names(dir)?;
        if names.is_empty() {
            break;
        }
        for n in &names {
            remove_entry(dir, n, depth)?;
        }
    }
    Ok(())
}

/// The entry names of the directory `dir` (not `.` or `..`), read through a fresh fd of it.
#[cfg(unix)]
fn dir_names(dir: &std::os::fd::OwnedFd) -> std::io::Result<Vec<std::ffi::CString>> {
    use std::os::fd::{AsRawFd, IntoRawFd};
    // SAFETY: `dir` is an open directory fd; the name is NUL-terminated.
    let fresh = owned_fd(unsafe { libc::openat(dir.as_raw_fd(), c".".as_ptr(), HELD_DIR_FLAGS) })?;
    let raw = fresh.into_raw_fd();
    // SAFETY: `raw` is an open directory fd we own; on success the stream owns it.
    let stream = unsafe { libc::fdopendir(raw) };
    if stream.is_null() {
        let e = std::io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so `raw` is still ours to close.
        unsafe { libc::close(raw) };
        return Err(e);
    }
    let mut out = Vec::new();
    loop {
        // SAFETY: `stream` is an open directory stream.
        let ent = unsafe { libc::readdir(stream) };
        if ent.is_null() {
            break;
        }
        // SAFETY: `ent` points at a valid entry whose `d_name` is NUL-terminated.
        let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            out.push(name.to_owned());
        }
    }
    // SAFETY: `stream` is open, and closed exactly once (with the fd it owns).
    unsafe { libc::closedir(stream) };
    Ok(out)
}

/// Without directory fds: the path checks of [`confined`] before every operation.
#[cfg(not(unix))]
impl HeldDir {
    pub(crate) fn open(root: &Path) -> Result<Self, String> {
        match std::fs::symlink_metadata(root) {
            Ok(m) if m.is_dir() => Ok(Self {
                path: root.to_path_buf(),
            }),
            _ => Err(format!("`{}` is not a plain directory", root.display())),
        }
    }

    pub(crate) fn subdir(&self, rel: &str, create: bool) -> Result<HeldDir, String> {
        rel_names(rel)?;
        let p = confined(&self.path, rel)?;
        if create {
            std::fs::create_dir_all(&p).map_err(|e| format!("`{rel}`: {e}"))?;
        }
        HeldDir::open(&confined(&self.path, rel)?)
    }

    pub(crate) fn create_fresh_dir(&self, rel: &str) -> Result<HeldDir, String> {
        rel_names(rel)?;
        let p = confined(&self.path, rel)?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("`{rel}`: {e}"))?;
        }
        std::fs::create_dir(confined(&self.path, rel)?).map_err(|e| format!("`{rel}`: {e}"))?;
        HeldDir::open(&p)
    }

    pub(crate) fn write(&self, rel: &str, content: &str) -> Result<(), String> {
        self.write_bytes(rel, content.as_bytes(), false)
    }

    pub(crate) fn write_bytes(&self, rel: &str, body: &[u8], _exec: bool) -> Result<(), String> {
        rel_names(rel)?;
        let path = confined(&self.path, rel)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // Re-checked once the directories exist: none of them may be a link.
        let path = confined(&self.path, rel)?;
        let mut f = no_follow(
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true),
        )
        .open(&path)
        .map_err(|e| e.to_string())?;
        std::io::Write::write_all(&mut f, body).map_err(|e| e.to_string())
    }

    pub(crate) fn remove_file(&self, rel: &str) -> Result<bool, String> {
        rel_names(rel)?;
        let Ok(p) = confined(&self.path, rel) else {
            return Ok(false);
        };
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.is_file() => std::fs::remove_file(&p)
                .map(|()| true)
                .map_err(|e| e.to_string()),
            _ => Ok(false),
        }
    }

    /// `rel` under the held path, its PARENTS [`confined`] (the leaf itself may be a link: it is
    /// inspected or removed, never followed).
    fn leaf_path(&self, rel: &str) -> Result<PathBuf, String> {
        let names = rel_names(rel)?;
        let (leaf, parents) = names.split_last().expect("rel_names is never empty");
        let mut p = if parents.is_empty() {
            self.path.clone()
        } else {
            confined(&self.path, &parents.join("/"))?
        };
        p.push(leaf);
        Ok(p)
    }

    pub(crate) fn kind(&self, rel: &str) -> Result<Option<EntryKind>, String> {
        let p = self.leaf_path(rel)?;
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => Ok(Some(EntryKind::Link)),
            Ok(m) if m.is_dir() => Ok(Some(EntryKind::Dir)),
            Ok(m) if m.is_file() => Ok(Some(EntryKind::File)),
            Ok(_) => Ok(Some(EntryKind::Other)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("`{rel}` cannot be inspected: {e}")),
        }
    }

    /// `remove_dir_all` removes a link (a junction, a symlink) without following it.
    pub(crate) fn remove_tree(&self, rel: &str) -> Result<bool, String> {
        let p = self.leaf_path(rel)?;
        match std::fs::symlink_metadata(&p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(format!("`{rel}` cannot be inspected: {e}")),
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => std::fs::remove_dir_all(&p)
                .map(|()| true)
                .map_err(|e| format!("`{rel}`: {e}")),
            Ok(_) => std::fs::remove_file(&p)
                .map(|()| true)
                .map_err(|e| format!("`{rel}`: {e}")),
        }
    }

    pub(crate) fn open_read(&self, rel: &str) -> Result<std::fs::File, String> {
        rel_names(rel)?;
        let p = confined(&self.path, rel)?;
        no_follow(std::fs::OpenOptions::new().read(true))
            .open(&p)
            .map_err(|e| format!("`{rel}`: {e}"))
    }

    pub(crate) fn still_at(&self, path: &Path) -> Result<(), String> {
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() && path == self.path => Ok(()),
            _ => Err(format!(
                "`{}` is no longer the directory the floor held (replaced after a command ran)",
                path.display()
            )),
        }
    }
}

/// (DES-BUILD-GUIDE) The base guide's check for `check`, when its pieces may be relied on for
/// this head; `Err` says why not (no guide, no such check, a probe not passed, stale, invalidated).
fn guide_for<'g>(
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
fn crashed(run: &CheckRun, g: Option<&GuideCheck>) -> Option<String> {
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
fn runner_current(
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
    let r = run_one_with(dir, held, &rc, &merged(true), sandbox, scratch, tree);
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

fn relabel(
    mut logs: Vec<crate::floor_log::LogRef>,
    attempt: &str,
) -> Vec<crate::floor_log::LogRef> {
    for l in &mut logs {
        l.attempt = attempt.to_string();
    }
    logs
}

/// (DES-FLOOR-V2 §4) Was the host oversubscribed when `run` started (its load band)?
fn ran_loaded(run: &CheckRun) -> bool {
    // The run's OWN load band, sampled as it started — never the load after other runs (the
    // base's) raised it.
    run.load_band.is_some_and(|b| b >= 1)
}

/// (DES-FLOOR-V2 §4) Bazel's `flaky_test_attempts=2`: when the deciding run FAILED (not a
/// timeout, not a spawn failure) on an oversubscribed host, wait (bounded) for the 1-min load to
/// fall below the CPU count, then run the SAME argv once more. `None` when no retry was due.
fn retry_if_loaded(
    worktree: &Path,
    held: Option<&HeldDir>,
    one: &RepoCheck,
    exec: &ExecOpts,
    first: &CheckRun,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
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
    let mut r = run_one_with(worktree, held, one, exec, sandbox, scratch, Tree::Head);
    r.logs = relabel(r.logs, "retry");
    r.source = format!(
        "the identical retry of `{}` after waiting {:.0}s for headroom (DES-FLOOR-V2)",
        one.name,
        waited.elapsed().as_secs_f64()
    );
    eprintln!(
        "wicked-core: repo checks floor — `{}` failed under load; its identical retry {}",
        one.name,
        if r.passed() { "passed" } else { "failed" }
    );
    Some(r)
}

fn retry_headroom_wait() -> Duration {
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

/// The base side of a failed head check: the base run (fresh or cached) and, when it failed, its
/// inventory.
struct BaseSide {
    base: BaseRun,
    toc: Option<Toc>,
}

/// (DES-FLOOR-V2 §3) Decide a FAILED head check `head` (its first, unchanged run) against its base
/// side: the head's exit codes decide, the base's TOC-validated failures are the only ones that
/// may be excluded, and head text is never read to decide.
#[allow(clippy::too_many_arguments)]
fn decide(
    worktree: &Path,
    held: Option<&HeldDir>,
    check: &RepoCheck,
    head: &mut CheckRun,
    side: Option<BaseSide>,
    guide: &Result<&GuideCheck, String>,
    exclude: bool,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
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
    let retry = (!crashed_out)
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
        .flatten();
    if let Some(r) = retry {
        logs.extend(r.logs.iter().cloned());
        head.reruns.push(r.clone());
        decision.flaky = r.passed();
        twice = !r.passed() && !r.timed_out && r.spawn_error.is_none() && ran_loaded(&r);
        deciding = r;
    }
    // Head ids are an ANNOTATION: listed beside the outcome, never deciding it (the deciding
    // run's, when it was the excluded one).
    let annotated: Vec<String> = deciding.failure_ids.clone();
    let base_passed = base_run.is_some_and(CheckRun::passed);
    // A base that could not be established — it timed out, could not start, or could not be
    // exported or provisioned — separates nothing: escalate. (A base that declares no such check,
    // and a repo that opted out of the comparison, leave the head's failure a new one.)
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
            .filter(|e| !e.contains("declares no `") && !e.contains("baseline_diff: false"))
            .map(|e| format!("the base could not be run ({e})")),
        _ => None,
    });
    let cannot_separate: Option<String> = if unseparated {
        plan.as_ref().err().cloned()
    } else {
        base_unknown
    };
    let (outcome, classification) = if deciding.passed() && excluded_run {
        (OUTCOME_PASS_EXCLUDED, Some(PRE_EXISTING_IN_SANDBOX))
    } else if deciding.passed() {
        (OUTCOME_PASS, Some(FLAKY_UNDER_LOAD))
    } else if let Some(why) = &cannot_separate {
        // Before a retry's own timeout or spawn failure: the first run decided this.
        decision.reason = Some(format!("base red, cannot separate: {why}"));
        (OUTCOME_BASE_RED_UNSEPARATED, Some(BASE_RED_UNSEPARATED))
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
        decision.reason = Some(format!(
            "{n} pre-existing excluded: {}",
            decision.excluded.join(", ")
        ));
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
fn decide_alone(run: &mut CheckRun) {
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
fn outcome_of(c: &CheckRun) -> Option<&str> {
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
const CLAIM_PHRASES: &[&str] = &[
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
const CHECK_WORDS: &[&str] = &[
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
fn sentences(text: &str) -> Vec<&str> {
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
fn judge_claim(ctx: &FloorContext, checks: &[CheckRun]) -> Option<ClaimCheck> {
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
fn cached_base_load_note(head: &CheckRun) -> String {
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
fn effective_bound(check: &RepoCheck) -> (Duration, Option<String>) {
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
const DRAIN_CAP: Duration = Duration::from_secs(5);

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
fn starts_retry_block(line: &str) -> bool {
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
        classification: None,
        pre_existing: Vec::new(),
        regressions: Vec::new(),
        base: None,
        env_cannot_run: Vec::new(),
        reruns: Vec::new(),
        load_band: None,
        decision: None,
        logs: Vec::new(),
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
    let start_dir = match held.map(|h| h.dir_fd(&check.dir)).transpose() {
        Ok(fd) => fd,
        Err(e) => {
            result.spawn_error = Some(e);
            return result;
        }
    };
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
    let merged = if exec.merge {
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
    let status = loop {
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
    if let Some(e) = out.error.or(err.error) {
        result.spawn_error = Some(e);
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
    if !result.timed_out && result.exit_code != Some(0) {
        let first_err = if exec.merge { &stdout } else { &stderr };
        if let Some(msg) = crate::validator::launcher_failure(
            &sandbox.wrapper,
            first_err.lines().next().unwrap_or(""),
        ) {
            result.spawn_error = Some(msg);
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

/// The per-stream log cap ([`crate::floor_log::MAX_STREAM_BYTES`]; a test may lower it).
fn log_cap() -> u64 {
    #[cfg(test)]
    if let Some(c) = test_seam::log_cap() {
        return c;
    }
    crate::floor_log::MAX_STREAM_BYTES
}

/// Per-thread stand-ins for the host's load, the retry's headroom wait and the log cap: a floor
/// runs on its caller's thread, so a test pins what IT sees without touching any other test.
#[cfg(test)]
pub(crate) mod test_seam {
    use std::cell::{Cell, RefCell};
    type Hook = Box<dyn FnOnce()>;
    thread_local! {
        static BEFORE_SPAWN: RefCell<Option<Hook>> = const { RefCell::new(None) };
        static LOAD: Cell<Option<(Option<f64>, usize)>> = const { Cell::new(None) };
        static CAP: Cell<Option<u64>> = const { Cell::new(None) };
        static EXCLUDE: Cell<Option<bool>> = const { Cell::new(None) };
    }
    pub(crate) fn exclusion() -> Option<bool> {
        EXCLUDE.with(Cell::get)
    }
    /// The hook this thread's next check spawn runs once, after its directory checks.
    pub(crate) fn take_before_spawn() -> Option<Hook> {
        BEFORE_SPAWN.with(|h| h.borrow_mut().take())
    }
    /// Run `f` once, just before this thread's next check spawn.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn before_spawn(f: impl FnOnce() + 'static) {
        BEFORE_SPAWN.with(|h| *h.borrow_mut() = Some(Box::new(f)));
    }
    /// Pin this thread's exclusion switch until dropped (the test binary arms it ON).
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) struct Excluding;
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn exclude(on: bool) -> Excluding {
        EXCLUDE.with(|c| c.set(Some(on)));
        Excluding
    }
    impl Drop for Excluding {
        fn drop(&mut self) {
            EXCLUDE.with(|c| c.set(None));
        }
    }
    pub(crate) fn log_cap() -> Option<u64> {
        CAP.with(Cell::get)
    }
    /// Lower this thread's per-stream log cap until dropped.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) struct Capped;
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn cap(bytes: u64) -> Capped {
        CAP.with(|c| c.set(Some(bytes)));
        Capped
    }
    impl Drop for Capped {
        fn drop(&mut self) {
            CAP.with(|c| c.set(None));
        }
    }
    pub(crate) fn load() -> Option<(Option<f64>, usize)> {
        LOAD.with(Cell::get)
    }
    /// Pin the load this thread's floor reads (and a zero headroom wait) until dropped.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) struct Pinned;
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn pin(load1: f64, cpus: usize) -> Pinned {
        LOAD.with(|c| c.set(Some((Some(load1), cpus))));
        Pinned
    }
    impl Drop for Pinned {
        fn drop(&mut self) {
            LOAD.with(|c| c.set(None));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::validator::SandboxLevel;

    pub(crate) fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wicked-repochecks-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The tree's check-scratch leaf in this test binary's (hermetic) check cache.
    fn leaf_of(wt: &Path) -> PathBuf {
        scratch_dir_for(&check_cache_root().unwrap(), wt, None)
    }

    /// The floor created nothing named `tmp/wicked-checks` (the pre-fix scratch) in the tree.
    fn assert_no_worktree_scratch(wt: &Path) {
        let old = wt
            .join(crate::worktree_guard::ENGINE_SCRATCH_DIR)
            .join("wicked-checks");
        assert!(
            std::fs::symlink_metadata(&old).is_err(),
            "the floor wrote its scratch into the worktree: {}",
            old.display()
        );
    }

    /// The boundary a test floor arms: the worktree and the tree's check-scratch leaf (which
    /// lives in the engine's check cache, outside the worktree).
    pub(crate) fn sandbox_for(wt: &Path) -> WorkerSandbox {
        let leaf = scratch_dir_for(&check_cache_root().unwrap(), wt, None);
        std::fs::create_dir_all(&leaf).unwrap();
        crate::validator::detect_floor_sandbox(&[wt.to_path_buf(), leaf])
    }

    #[test]
    fn detects_node_scripts_with_an_ignore_scripts_install_when_node_modules_is_absent() {
        let wt = scratch("detect-node");
        std::fs::write(
            wt.join("package.json"),
            r#"{"scripts":{"build":"x","test":"vitest run","typecheck":"tsc --noEmit","dev":"y"}}"#,
        )
        .unwrap();
        std::fs::write(wt.join("package-lock.json"), "{}").unwrap();
        let names: Vec<(String, Vec<String>)> = detect(&wt)
            .unwrap()
            .into_iter()
            .map(|c| (c.name, c.argv))
            .collect();
        assert_eq!(
            names,
            vec![
                (
                    "install".to_string(),
                    s(&["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"])
                ),
                ("typecheck".to_string(), s(&["npm", "run", "typecheck"])),
                ("test".to_string(), s(&["npm", "run", "test"])),
            ],
            "install first (lockfile ⇒ ci, lifecycle scripts NEVER run), then typecheck/lint/test \
             in that order, only those present"
        );
        // No lockfile ⇒ an `npm install` that writes NO lockfile into the reviewed tree.
        std::fs::remove_file(wt.join("package-lock.json")).unwrap();
        assert_eq!(
            detect(&wt).unwrap()[0].argv,
            s(&[
                "npm",
                "install",
                "--ignore-scripts",
                "--no-package-lock",
                "--no-audit",
                "--no-fund"
            ])
        );
        // node_modules present ⇒ no install step — unless FORCED (F-433-003: a lift moved the
        // lockfile, the installed modules are stale), which re-installs frozen + scripts-off.
        std::fs::create_dir_all(wt.join("node_modules")).unwrap();
        std::fs::write(wt.join("package-lock.json"), "{}").unwrap();
        let forced = detect_with(
            &wt,
            &FloorContext {
                force_install: true,
                ..FloorContext::default()
            },
        )
        .unwrap();
        assert_eq!(forced[0].name, "install", "{forced:?}");
        assert_eq!(
            forced[0].argv,
            vec!["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"]
        );
        assert!(
            forced[0].source.contains("forced: lockfile drift"),
            "{}",
            forced[0].source
        );
        std::fs::remove_file(wt.join("package-lock.json")).unwrap();
        assert_eq!(
            detect(&wt)
                .unwrap()
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["typecheck", "test"]
        );
    }

    #[test]
    fn a_hollow_node_modules_does_not_count_as_provisioned_and_the_install_names_the_gap() {
        // F-E2E-029: the nested worktree's `node_modules/` held only vitest's cache dir; the floor
        // must install, and say which declared dependency was missing.
        let wt = scratch("hollow-node-modules");
        std::fs::write(
            wt.join("package.json"),
            r#"{"scripts":{"test":"vitest run"},"dependencies":{"wicked-crew-api-types":"1.0.0"},"devDependencies":{"vitest":"3.0.0","@types/node":"22.0.0"},"optionalDependencies":{"fsevents":"2.0.0"}}"#,
        )
        .unwrap();
        std::fs::write(wt.join("package-lock.json"), "{}").unwrap();
        std::fs::create_dir_all(wt.join("node_modules/.vite/vitest")).unwrap();
        let detected = detect(&wt).unwrap();
        assert_eq!(detected[0].name, "install", "{detected:?}");
        assert_eq!(
            detected[0].argv,
            s(&["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"])
        );
        assert!(
            detected[0]
                .source
                .contains("`wicked-crew-api-types` is not installed"),
            "the install's source names the first missing declared dependency: {}",
            detected[0].source
        );
        // A partial install — some dependencies present — is still a gap, named by the missing one
        // (declared names are visited in sorted order: `@types/node` before `vitest`).
        for dep in ["wicked-crew-api-types", "@types/node"] {
            std::fs::create_dir_all(wt.join("node_modules").join(dep)).unwrap();
            std::fs::write(wt.join("node_modules").join(dep).join("package.json"), "{}").unwrap();
        }
        let detected = detect(&wt).unwrap();
        assert_eq!(detected[0].name, "install");
        assert!(
            detected[0].source.contains("`vitest` is not installed"),
            "{}",
            detected[0].source
        );
        // Every declared dependency present (a scoped one included; the OPTIONAL one absent)
        // ⇒ provisioned ⇒ no install step.
        std::fs::create_dir_all(wt.join("node_modules/vitest")).unwrap();
        std::fs::write(wt.join("node_modules/vitest/package.json"), "{}").unwrap();
        let names: Vec<String> = detect(&wt).unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["test".to_string()], "provisioned ⇒ no install");
    }

    /// core#1155: a live worktree whose node_modules the engine installed for the SEATS' platform
    /// (a container seat's Linux) does not provision this host's checks: the floor installs.
    #[test]
    fn a_node_modules_installed_for_the_seats_platform_is_a_gap_on_the_host() {
        let wt = scratch("seat-platform-deps");
        std::fs::write(
            wt.join("package.json"),
            r#"{"scripts":{"test":"vitest run"},"devDependencies":{"vitest":"3.0.0"}}"#,
        )
        .unwrap();
        std::fs::write(wt.join("package-lock.json"), "{}").unwrap();
        std::fs::create_dir_all(wt.join("node_modules/vitest")).unwrap();
        std::fs::write(wt.join("node_modules/vitest/package.json"), "{}").unwrap();
        let names = |wt: &Path| -> Vec<String> {
            detect(wt).unwrap().into_iter().map(|c| c.name).collect()
        };
        assert_eq!(
            names(&wt),
            vec!["test".to_string()],
            "provisioned for the host"
        );
        // The marker names the host's own platform: still provisioned.
        let host = crate::seat_deps::SeatPlatform::host().label();
        let marker = wt.join("node_modules").join(crate::seat_deps::MARKER);
        std::fs::write(&marker, format!(r#"{{"platform":"{host}","lock":"x"}}"#)).unwrap();
        assert_eq!(names(&wt), vec!["test".to_string()]);
        // Another platform's install: the floor installs for the host first.
        std::fs::write(&marker, r#"{"platform":"plan9-mips","lock":"x"}"#).unwrap();
        let detected = detect(&wt).unwrap();
        assert_eq!(detected[0].name, "install", "{detected:?}");
        assert!(
            detected[0].source.contains("plan9-mips"),
            "{}",
            detected[0].source
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_dependency_counts_as_installed() {
        // A workspace member or `npm link` target is a symlink in `node_modules/`; its presence is
        // the install, and nothing under it is followed or read.
        let wt = scratch("symlinked-dep");
        std::fs::write(
            wt.join("package.json"),
            r#"{"scripts":{"test":"vitest run"},"devDependencies":{"linked":"1.0.0"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(wt.join("node_modules")).unwrap();
        std::os::unix::fs::symlink("/nonexistent/elsewhere", wt.join("node_modules/linked"))
            .unwrap();
        let names: Vec<String> = detect(&wt).unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["test".to_string()]);
    }

    #[test]
    fn a_failed_install_is_reported_as_a_provisioning_finding_not_a_verdict() {
        let report = RepoChecksReport {
            detected: vec![RepoCheck {
                name: "install".into(),
                argv: s(&["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"]),
                source: "package-lock.json (node_modules absent)".into(),
                timeout_s: None,
                dir: String::new(),
            }],
            checks: vec![CheckRun {
                name: "install".into(),
                argv: s(&["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"]),
                source: "package-lock.json (node_modules absent)".into(),
                exit_code: Some(1),
                timed_out: false,
                spawn_error: None,
                duration_ms: 1200,
                stdout_tail: String::new(),
                stderr_tail: "npm ERR! code ENOTFOUND\nnpm ERR! network request failed".into(),
                bound_s: 0,
                bound_note: None,
                failure_ids: Vec::new(),
                flaky_ids: Vec::new(),
                classification: None,
                pre_existing: Vec::new(),
                regressions: Vec::new(),
                base: None,
                env_cannot_run: Vec::new(),
                reruns: Vec::new(),
                decision: None,
                logs: Vec::new(),
                load_band: None,
            }],
            skipped: vec!["typecheck".into(), "test".into()],
            passed: false,
            detect_error: None,
            sandbox_level: "sandboxed".into(),
            sandbox_note: None,
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
        };
        let reason = report.denial_reason();
        assert!(
            reason.contains("dependency provisioning failed"),
            "{reason}"
        );
        assert!(
            reason.contains("package-lock.json (node_modules absent)"),
            "{reason}"
        );
        assert!(
            reason.contains("environment finding, not a verdict"),
            "{reason}"
        );
        assert!(
            reason.contains("ENOTFOUND"),
            "the install's own stderr rides along: {reason}"
        );
        assert!(
            reason.contains("not run after the failure: typecheck, test"),
            "{reason}"
        );
    }

    #[test]
    fn detects_the_package_manager_from_the_lockfile_and_cargo_from_the_manifest() {
        let wt = scratch("detect-pm");
        std::fs::write(
            wt.join("package.json"),
            r#"{"scripts":{"lint":"eslint ."}}"#,
        )
        .unwrap();
        std::fs::write(wt.join("pnpm-lock.yaml"), "").unwrap();
        std::fs::create_dir_all(wt.join("node_modules")).unwrap();
        std::fs::write(wt.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let checks = detect(&wt).unwrap();
        assert_eq!(
            checks[0].name, "cargo-fmt-check",
            "the formatter precedes every other check"
        );
        assert_eq!(checks[0].argv, s(&["cargo", "fmt", "--all", "--check"]));
        assert_eq!(checks[1].argv, s(&["pnpm", "run", "lint"]));
        assert_eq!(checks[2].name, "cargo-test");
        assert_eq!(checks[2].argv, s(&["cargo", "test", "--no-fail-fast"]));
        // No manifests at all ⇒ nothing detected, and a run of it is a vacuous pass that SAYS so.
        let empty = scratch("detect-empty");
        assert!(detect(&empty).unwrap().is_empty());
        let report = run(&empty);
        if report.sandbox_error.is_none() {
            assert!(report.passed && report.checks.is_empty() && report.detect_error.is_none());
            assert!(report.summary().contains("no repository checks detected"));
        } else {
            assert!(
                !report.passed,
                "no boundary ⇒ fail-closed even with nothing to run"
            );
        }
        assert!(
            !report.sandbox_level.is_empty(),
            "the containment level is always disclosed"
        );
    }

    /// Codex review on #414: an unreadable or malformed manifest, or a probe that is a symlink,
    /// is NOT "no checks" — the floor FAILS with the reason.
    #[cfg(unix)]
    #[test]
    fn a_malformed_or_symlinked_manifest_fails_the_floor_by_name() {
        if crate::validator::skip_when_nested(
            "a_malformed_or_symlinked_manifest_fails_the_floor_by_name",
        ) {
            return;
        }
        let wt = scratch("detect-bad");
        std::fs::write(wt.join("package.json"), "not a manifest").unwrap();
        let err = detect(&wt).expect_err("malformed package.json must not read as no checks");
        assert!(err.contains("not valid JSON"), "{err}");
        let report = run_with_sandbox(
            &wt,
            WorkerSandbox {
                wrapper: vec!["true".to_string()],
                level: SandboxLevel::Sandboxed,
                downgrade_reason: None,
            },
        );
        assert!(!report.passed && report.checks.is_empty());
        assert!(
            report.denial_reason().contains("not valid JSON")
                && report.denial_reason().contains("fail-closed"),
            "{}",
            report.denial_reason()
        );

        // `scripts` of the wrong shape.
        std::fs::write(wt.join("package.json"), r#"{"scripts":"nope"}"#).unwrap();
        assert!(detect(&wt)
            .expect_err("scripts must be an object")
            .contains("not an object"));

        // A symlinked manifest / lockfile / node_modules is refused, never followed.
        let outside = scratch("detect-outside");
        std::fs::write(
            outside.join("package.json"),
            r#"{"scripts":{"test":"true"}}"#,
        )
        .unwrap();
        std::fs::remove_file(wt.join("package.json")).unwrap();
        std::os::unix::fs::symlink(outside.join("package.json"), wt.join("package.json")).unwrap();
        let err = detect(&wt).expect_err("a symlinked manifest is refused");
        assert!(
            err.contains("symlink") && err.contains("package.json"),
            "{err}"
        );
        std::fs::remove_file(wt.join("package.json")).unwrap();
        std::fs::write(wt.join("package.json"), r#"{"scripts":{"test":"true"}}"#).unwrap();
        std::os::unix::fs::symlink(&outside, wt.join("node_modules")).unwrap();
        let err = detect(&wt).expect_err("a symlinked node_modules is refused");
        assert!(err.contains("node_modules"), "{err}");
        std::fs::remove_file(wt.join("node_modules")).unwrap();
        std::fs::write(wt.join("Cargo.toml"), "").unwrap();
        std::fs::remove_file(wt.join("Cargo.toml")).unwrap();
        std::os::unix::fs::symlink(outside.join("package.json"), wt.join("Cargo.toml")).unwrap();
        assert!(detect(&wt)
            .expect_err("a symlinked Cargo.toml is refused")
            .contains("Cargo.toml"));
    }

    /// The brief's test, in the medium every `cargo test` host has: a fixture crate whose one test
    /// fails. The floor must capture the non-zero exit AND the assertion text in the tail, and
    /// report the failure as a denial — running with the isolated `CARGO_HOME`/`HOME` and inside
    /// whatever OS boundary the host offers.
    #[test]
    fn captures_a_failing_cargo_test_as_evidence_and_denies() {
        let wt = scratch("cargo-fail");
        std::fs::write(
            wt.join("Cargo.toml"),
            "[package]\nname = \"floor_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\npath = \"lib.rs\"\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(
            wt.join("lib.rs"),
            "#[cfg(test)]\nmod t {\n    #[test]\n    fn boom() {\n        assert!(false, \
             \"BOOM the floor must see this\");\n    }\n}\n",
        )
        .unwrap();
        let report = run(&wt);
        assert!(
            !report.passed,
            "a failing test must fail the floor: {report:?}"
        );
        if report.sandbox_error.is_some() {
            eprintln!(
                "repo_checks: no sandbox tool here — the floor failed closed instead of running"
            );
            return;
        }
        // Item 7 (adversarial review on #414): the fixture ships no `Cargo.lock`, so cargo wrote
        // one — the engine's own side effect — and the floor removed it again, disclosed.
        assert_eq!(
            report.engine_writes_removed,
            vec!["Cargo.lock".to_string()],
            "{report:?}"
        );
        assert!(
            !wt.join("Cargo.lock").exists(),
            "the engine-written lockfile must not be left for the worktree guard to deny"
        );
        assert_eq!(
            report.checks[0].name, "cargo-fmt-check",
            "formatter runs before tests"
        );
        assert!(
            report.checks[0].exit_code == Some(0),
            "properly formatted fixture passes fmt check: {:?}",
            report.checks[0]
        );
        let c = &report.checks[1];
        assert_eq!(c.name, "cargo-test");
        assert!(
            c.exit_code.is_some_and(|e| e != 0),
            "the non-zero exit is captured: {:?} ({} / {})",
            c.exit_code,
            c.stdout_tail,
            c.stderr_tail
        );
        assert!(!c.timed_out && c.spawn_error.is_none());
        let combined = format!("{}\n{}", c.stdout_tail, c.stderr_tail);
        assert!(
            combined.contains("BOOM the floor must see this"),
            "the assertion text rides the captured tail: {combined}"
        );
        let denial = report.denial_reason();
        assert!(
            denial.contains("cargo-test: exit") && denial.contains(CRITERION),
            "the denial names the check and the criterion: {denial}"
        );
        // The isolated homes AND the build artifacts live in the engine's check cache, never in
        // the reviewed tree (Copilot on #414: `./target` would trip the worktree guard; the
        // 2026-10-20 disk incident: `tmp/wicked-checks` held 16 GB in each kept worktree).
        let scratch = leaf_of(&wt);
        assert!(!scratch.starts_with(&wt), "{}", scratch.display());
        assert_no_worktree_scratch(&wt);
        assert!(scratch.join("cargo-home").is_dir());
        assert!(
            scratch
                .join("cargo-target")
                .join("head")
                .join("debug")
                .is_dir(),
            "cargo built into the scratch target dir's HEAD leaf (core#480)"
        );
        assert!(
            !wt.join("target").exists(),
            "no ./target in the reviewed tree"
        );
    }

    /// core#551: a worktree with an unformatted Rust file must fail the floor — the formatter check
    /// runs before tests and a formatting diff is a deterministic denial.
    #[test]
    fn unformatted_rust_fails_fmt_floor() {
        let wt = scratch("fmt-fail");
        std::fs::write(
            wt.join("Cargo.toml"),
            "[package]\nname = \"fmt_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\npath = \"lib.rs\"\n[workspace]\n",
        )
        .unwrap();
        // Deliberately unformatted: rustfmt would expand `{ x }` to a multi-line body.
        std::fs::write(wt.join("lib.rs"), "pub fn add(a: i32,b: i32)->i32{a+b}\n").unwrap();
        let report = run(&wt);
        assert!(
            !report.passed,
            "an unformatted Rust file must fail the floor: {report:?}"
        );
        if report.sandbox_error.is_some() {
            eprintln!(
                "repo_checks: no sandbox tool here — the floor failed closed instead of running"
            );
            return;
        }
        let fmt = report
            .checks
            .iter()
            .find(|c| c.name == "cargo-fmt-check")
            .expect("cargo-fmt-check must appear in the checks");
        assert!(
            fmt.exit_code.is_some_and(|e| e != 0),
            "cargo-fmt-check exits non-zero on a formatting diff: {fmt:?}"
        );
        assert_eq!(
            report.detected[0].name, "cargo-fmt-check",
            "formatter must be listed FIRST in detected checks"
        );
        let denial = report.denial_reason();
        assert!(
            denial.contains("cargo-fmt-check") && denial.contains(CRITERION),
            "the denial names the formatter check: {denial}"
        );
    }

    /// core#551 control: a properly formatted Rust file must produce no formatter diff — the floor
    /// must not deny a worktree solely because of formatting when the code IS already formatted.
    #[test]
    fn formatted_rust_worktree_passes_fmt_floor() {
        let wt = scratch("fmt-pass");
        std::fs::write(
            wt.join("Cargo.toml"),
            "[package]\nname = \"fmt_pass_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\npath = \"lib.rs\"\n[workspace]\n",
        )
        .unwrap();
        // Properly formatted Rust — rustfmt produces no diff.
        std::fs::write(
            wt.join("lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )
        .unwrap();
        // Detection: cargo-fmt-check must be first in the detected set.
        let checks = detect(&wt).unwrap();
        assert_eq!(
            checks[0].name, "cargo-fmt-check",
            "formatter is detected first for a Rust workspace: {checks:?}"
        );
        // Execution: cargo-fmt-check exits 0 — no diff in the formatted fixture.
        // Other checks (cargo-test, lockfile writes) may fail in restricted environments;
        // the control claim is narrowly about the formatter exit code.
        let report = run(&wt);
        if report.sandbox_error.is_some() {
            eprintln!(
                "repo_checks: no sandbox tool here — the floor failed closed instead of running"
            );
            return;
        }
        // No escape hatch past the sandbox check: the formatter was detected above, so it MUST
        // have run — a missing or unspawnable check is a floor defect, not an environment fact.
        let fmt = report
            .checks
            .iter()
            .find(|c| c.name == "cargo-fmt-check")
            .expect("cargo-fmt-check must run when detected");
        assert!(
            fmt.spawn_error.is_none(),
            "cargo-fmt-check must be invocable where cargo is: {:?}",
            fmt.spawn_error
        );
        assert!(
            fmt.exit_code == Some(0),
            "cargo-fmt-check exits 0 on formatted code — no false positive: {fmt:?}"
        );
    }

    /// The brief's literal case — a failing `npm test`. Runs where npm is on PATH (every hosted
    /// CI runner; developer machines); otherwise says so and returns, because a host without npm
    /// is a fact about the host, not about the floor.
    #[test]
    fn captures_a_failing_npm_test_as_evidence() {
        if crate::validator::find_on_path("npm").is_none() {
            eprintln!("repo_checks: npm not on PATH — the npm-shaped floor test cannot run here");
            return;
        }
        let wt = scratch("npm-fail");
        std::fs::write(
            wt.join("package.json"),
            r#"{"name":"floor-fixture","version":"0.0.0","scripts":{"test":"node -e \"process.stdout.write('NPM BOOM\\n'); process.exit(3)\""}}"#,
        )
        .unwrap();
        // node_modules present ⇒ no install step (nothing to install, no network).
        std::fs::create_dir_all(wt.join("node_modules")).unwrap();
        let report = run(&wt);
        assert!(!report.passed, "{report:?}");
        if report.sandbox_error.is_some() {
            eprintln!(
                "repo_checks: no sandbox tool here — the floor failed closed instead of running"
            );
            return;
        }
        let c = &report.checks[0];
        assert_eq!(c.name, "test");
        assert_eq!(c.argv, s(&["npm", "run", "test"]));
        assert_eq!(
            c.exit_code,
            Some(3),
            "npm propagates the script's exit code ({} / {})",
            c.stdout_tail,
            c.stderr_tail
        );
        assert!(c.stdout_tail.contains("NPM BOOM"), "{}", c.stdout_tail);
    }

    /// The J4 shape (rig run 62c4f0ff, crew's `scripts/test-related.mjs`): the first pass fails
    /// two files, the check's own retry re-runs them serially and only the shared one fails again.
    /// The marker line goes to both streams (vitest prints its failures on stderr).
    #[cfg(unix)]
    const RETRY_SHAPE_SH: &str = "\
echo ' RUN  v3.2.4'
echo ' FAIL  tests/a.test.ts > first pass only' >&2
echo ' FAIL  tests/s.test.ts > shared' >&2
M='WICKED-RETRY a worker died — re-running the 2 named file(s) ONCE, serially, before recording any failure: tests/a.test.ts, tests/s.test.ts'
echo \"$M\"
echo \"$M\" >&2
echo ' FAIL  tests/s.test.ts > shared' >&2
echo 'WICKED-RETRY the re-run FAILED too — a reproducing failure is this change'\\''s, recorded as exit 1.'
exit 1
";

    /// (core#1172) A check whose own retry cleared some first-pass failures: those are flaky
    /// (passed on retry), kept out of `failure_ids` (so out of the change-only list), and the
    /// check still fails exactly as before — its exit decides, not the ids.
    #[cfg(unix)]
    #[test]
    fn a_failure_the_checks_own_retry_cleared_is_flaky_not_the_changes() {
        let wt = scratch("retry-cleared");
        std::fs::write(wt.join("retry.sh"), RETRY_SHAPE_SH).unwrap();
        let scratch = CheckScratch::prepare(&wt).unwrap();
        let check = RepoCheck {
            name: "test_targeted".into(),
            argv: s(&["sh", "retry.sh"]),
            source: "fixture".into(),
            timeout_s: None,
            dir: String::new(),
        };
        let no_wrapper = WorkerSandbox {
            wrapper: Vec::new(),
            level: crate::validator::SandboxLevel::BestEffort,
            downgrade_reason: None,
        };
        let r = run_one(&wt, &check, &no_wrapper, &scratch, Tree::Head);
        assert_eq!(r.exit_code, Some(1), "{r:?}");
        assert!(!r.passed(), "the check's verdict is its exit: {r:?}");
        assert_eq!(
            r.failure_ids,
            s(&["FAIL tests/s.test.ts > shared"]),
            "{r:?}"
        );
        assert_eq!(
            r.flaky_ids,
            s(&["FAIL tests/a.test.ts > first pass only"]),
            "{r:?}"
        );
    }

    /// (core#1172) The split itself: no retry block keeps every id; a stream that carries no
    /// marker cannot be split (old scripts printed it on stdout only, failures on stderr); a final
    /// pass that names nothing tells nothing apart; the LAST marker starts the final pass; the
    /// marker's other lines start nothing.
    #[test]
    fn the_retry_split_reads_each_stream_after_its_last_re_run_marker() {
        let start = "WICKED-RETRY deadline — re-running the 1 named file(s) ONCE: tests/a.test.ts";
        let a = " FAIL  tests/a.test.ts > a";
        let b = " FAIL  tests/b.test.ts > b";
        let ids = |v: &[&str]| s(v);
        // No block.
        assert_eq!(
            split_retry_ids("", &format!("{a}\n{b}\n")),
            (
                ids(&["FAIL tests/a.test.ts > a", "FAIL tests/b.test.ts > b"]),
                Vec::new()
            )
        );
        // A marker on stdout only, failures on stderr: nothing split.
        assert_eq!(
            split_retry_ids(&format!("{start}\n"), &format!("{a}\n{b}\n")),
            (
                ids(&["FAIL tests/a.test.ts > a", "FAIL tests/b.test.ts > b"]),
                Vec::new()
            )
        );
        // Both streams marked: b reproduced, a cleared.
        assert_eq!(
            split_retry_ids(&format!("{start}\n"), &format!("{a}\n{b}\n{start}\n{b}\n")),
            (
                ids(&["FAIL tests/b.test.ts > b"]),
                ids(&["FAIL tests/a.test.ts > a"])
            )
        );
        // The final pass named nothing: every id stays a failure.
        assert_eq!(
            split_retry_ids("", &format!("{a}\n{start}\n")),
            (ids(&["FAIL tests/a.test.ts > a"]), Vec::new())
        );
        // (xreview on #1176) …even when another, unmarked stream names a failure: the marked
        // stream's pre-marker id is not cleared by ids that were never in a final pass.
        assert_eq!(
            split_retry_ids(&format!("{a}\n{start}\n"), &format!("{b}\n")),
            (
                ids(&["FAIL tests/a.test.ts > a", "FAIL tests/b.test.ts > b"]),
                Vec::new()
            )
        );
        // (xreview on #1176) A failure the final pass names past the id cap reproduced: it is a
        // failure, never flaky.
        let many: String = (0..MAX_FAILURE_IDS + 5)
            .map(|n| format!(" FAIL  tests/m{n}.test.ts > m\n"))
            .collect();
        let (failing, flaky) = split_retry_ids("", &format!("{a}\n{start}\n{many}{a}\n"));
        assert_eq!(failing.len(), MAX_FAILURE_IDS);
        assert!(flaky.is_empty(), "{flaky:?}");
        // A "none —" line or the verdict line starts no block.
        for other in [
            "WICKED-RETRY none — the failure carries no deadline marker",
            "WICKED-RETRY the re-run FAILED too — recorded as exit 1.",
        ] {
            assert_eq!(
                split_retry_ids("", &format!("{a}\n{other}\n{b}\n")).1,
                Vec::<String>::new(),
                "{other}"
            );
        }
    }

    /// Codex review on #414: a check is repo-controlled code and NEVER runs without an OS write
    /// boundary. Deterministic by injection: a best-effort probe (what a host with no
    /// `sandbox-exec`/`bwrap` yields) makes the floor FAIL with the reason, and nothing runs —
    /// even a check that would have passed.
    /// (core#678, codex r1) A launcher whose execution probe failed is no boundary: the floor
    /// downgrades the detection (so the opt-in applies) instead of running a bwrap that cannot arm.
    #[test]
    fn the_floor_downgrades_a_launcher_whose_probe_failed() {
        let armed = || WorkerSandbox {
            wrapper: vec!["bwrap".to_string()],
            level: crate::validator::SandboxLevel::Sandboxed,
            downgrade_reason: None,
        };
        let kept = arm_probed(armed(), Ok("bwrap"));
        assert_eq!(kept.level, crate::validator::SandboxLevel::Sandboxed);
        assert_eq!(kept.wrapper, vec!["bwrap".to_string()]);
        let down = arm_probed(armed(), Err(crate::worker_sandbox::FloorUnarmed::CannotArm));
        assert_eq!(down.level, crate::validator::SandboxLevel::BestEffort);
        assert!(down.wrapper.is_empty());
        assert!(down.downgrade_reason.unwrap().starts_with("cannot_arm"));
    }

    #[test]
    fn without_a_write_boundary_the_floor_fails_closed_and_runs_nothing() {
        // Reads an environment variable another test MUTATES (the core#416 opt-in below), so it
        // holds the crate-wide lock shared: fail-closed is only the default while nobody opted in.
        let _env = crate::test_env::ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let wt = scratch("no-boundary");
        std::fs::write(wt.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let probe_marker = wt.join("ran.txt");
        let no_boundary = WorkerSandbox {
            wrapper: Vec::new(),
            level: SandboxLevel::BestEffort,
            downgrade_reason: Some("no OS-sandbox tool on PATH".to_string()),
        };
        let report = run_with_sandbox(&wt, no_boundary);
        assert!(!report.passed, "{report:?}");
        assert!(
            report.checks.is_empty(),
            "nothing may run unsandboxed: {report:?}"
        );
        assert_eq!(report.sandbox_level, "best-effort");
        assert!(
            report.sandbox_error.as_deref().is_some_and(
                |e| e.contains("no OS write boundary") && e.contains("no OS-sandbox tool")
            ),
            "{report:?}"
        );
        assert_eq!(
            report
                .detected
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["cargo-fmt-check", "cargo-test"],
            "what WOULD have run is still on the record"
        );
        let denial = report.denial_reason();
        assert!(
            denial.contains("NOT run")
                && denial.contains("fail-closed")
                && denial.contains(CRITERION),
            "{denial}"
        );
        assert!(!probe_marker.exists());
        assert!(
            report.denial_reason().contains(UNSANDBOXED_OPT_IN_ENV),
            "the denial names the opt-in, so an operator on a boundary-less host is not stuck: {}",
            report.denial_reason()
        );
    }

    /// core#416 item 3(b) — the disclosed opt-in, and the only thing an operator on Windows can
    /// do about the floor. The default above denies every `repo_checks_floor` phase on a host
    /// where no boundary can arm; with `WICKED_REPO_CHECKS_UNSANDBOXED=1` the checks RUN, and the
    /// report, the gate's own note and the recorded `FloorEnv` all say that nothing contained
    /// them (`sandbox_level: "none"`) — so a pass is never read as containment evidence.
    ///
    /// Deterministic by injection, like the fail-closed test: a best-effort probe is what a host
    /// with no `sandbox-exec`/`bwrap` yields, whatever this host has.
    #[test]
    fn the_disclosed_opt_in_runs_the_checks_with_no_boundary() {
        if crate::validator::find_on_path("npm").is_none() {
            eprintln!("repo_checks: npm not on PATH — the opt-in floor test cannot run here");
            return;
        }
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var_os(UNSANDBOXED_OPT_IN_ENV);
        let wt = scratch("unsandboxed-opt-in");
        std::fs::write(
            wt.join("package.json"),
            r#"{"name":"floor-fixture","version":"0.0.0","scripts":{"test":"node -e \"process.stdout.write('RAN UNSANDBOXED\\n')\""}}"#,
        )
        .unwrap();
        // node_modules present ⇒ no install step (nothing to install, no network).
        std::fs::create_dir_all(wt.join("node_modules")).unwrap();
        let no_boundary = || WorkerSandbox {
            wrapper: Vec::new(),
            level: SandboxLevel::BestEffort,
            downgrade_reason: Some("no OS-sandbox tool on PATH".to_string()),
        };

        std::env::set_var(UNSANDBOXED_OPT_IN_ENV, "1");
        let report = run_with_sandbox(&wt, no_boundary());
        match &prior {
            Some(v) => std::env::set_var(UNSANDBOXED_OPT_IN_ENV, v),
            None => std::env::remove_var(UNSANDBOXED_OPT_IN_ENV),
        }

        assert!(
            report.sandbox_error.is_none(),
            "the opt-in is not a refusal: {report:?}"
        );
        assert_eq!(report.sandbox_level, UNSANDBOXED_LEVEL);
        assert!(report.unsandboxed());
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "test")
            .expect("the repository's own check RAN");
        assert!(
            c.stdout_tail.contains("RAN UNSANDBOXED"),
            "{}",
            c.stdout_tail
        );
        assert!(report.passed, "{report:?}");
        // Disclosed on every surface a reader can reach.
        let summary = report.summary();
        assert!(summary.contains("no OS write boundary"), "{summary}");
        let note = report
            .classification_note()
            .expect("the gate's floorNote discloses the posture");
        assert!(
            note.contains(UNSANDBOXED_OPT_IN_ENV) && note.contains("NOT evidence"),
            "{note}"
        );
        assert_eq!(
            report.env.as_ref().map(|e| e.sandbox_level.as_str()),
            Some(UNSANDBOXED_LEVEL),
            "the recorded environment carries the level CI parity is read against"
        );
    }

    /// The boundary itself, where the host has one: a check script that writes outside the
    /// worktree fails the floor and the write never lands; the operator's real HOME is not the
    /// check's HOME. On a host without a sandbox tool this asserts the fail-closed path instead
    /// (never a skip that reads as a pass).
    #[cfg(unix)]
    #[test]
    fn a_check_that_writes_outside_the_worktree_fails_the_floor() {
        // Its no-tool branch asserts the fail-closed refusal, which the core#416 opt-in can turn
        // off process-wide — shared lock, so that mutation cannot land mid-assert.
        let _env = crate::test_env::ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let base = scratch("contain");
        let wt = base.join("wt");
        let outside = base.join("outside");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let sandbox = sandbox_for(&wt);
        if sandbox.level != SandboxLevel::Sandboxed {
            // No tool here: the floor must FAIL, not run. Assert that and stop.
            let report = run(&wt);
            assert!(
                !report.passed && report.sandbox_error.is_some(),
                "{report:?}"
            );
            return;
        }
        let scratch = CheckScratch::prepare(&wt).unwrap();
        // Premise: an INSIDE write under the same wrapper works (bwrap present but unusable — no
        // user namespaces — is an environmental skip of the KERNEL claim only).
        let inside = RepoCheck {
            name: "test".into(),
            argv: s(&[
                "sh",
                "-c",
                "printf y > \"$1\"",
                "_",
                &wt.join("ok").to_string_lossy(),
            ]),
            source: "fixture".into(),
            timeout_s: None,
            dir: String::new(),
        };
        if !run_one(&wt, &inside, &sandbox, &scratch, Tree::Head).passed() {
            eprintln!("repo_checks: the sandbox wrapper cannot run on this host — skipping the kernel claim");
            return;
        }
        let pwned = outside.join("pwned");
        let escaping = RepoCheck {
            name: "test".into(),
            argv: s(&[
                "sh",
                "-c",
                "printf x > \"$1\"",
                "_",
                &pwned.to_string_lossy(),
            ]),
            source: "fixture".into(),
            timeout_s: None,
            dir: String::new(),
        };
        let r = run_one(&wt, &escaping, &sandbox, &scratch, Tree::Head);
        assert!(!r.passed(), "an outside write must fail the check: {r:?}");
        assert!(!pwned.exists(), "the outside write must never land on disk");
        // macOS `sandbox-exec` says EPERM/EACCES; Linux `bwrap` surfaces its `--ro-bind` as EROFS.
        assert!(
            r.stderr_tail.contains("Permission denied")
                || r.stderr_tail.contains("Operation not permitted")
                || r.stderr_tail.contains("Read-only file system"),
            "the check observed the OS denial: {}",
            r.stderr_tail
        );
        // And the operator's real HOME is not the check's HOME.
        let home_probe = RepoCheck {
            name: "test".into(),
            argv: s(&["sh", "-c", "printf %s \"$HOME\""]),
            source: "fixture".into(),
            timeout_s: None,
            dir: String::new(),
        };
        let r = run_one(&wt, &home_probe, &sandbox, &scratch, Tree::Head);
        assert!(r.passed());
        assert!(
            r.stdout_tail
                .starts_with(&scratch.home().to_string_lossy().to_string()),
            "HOME is the isolated scratch home, got {}",
            r.stdout_tail
        );
    }

    /// Adversarial review on #414: a check is repo-controlled code with the network open, so it
    /// must never see the daemon's credentials. The environment is cleared to an allow-list — a
    /// secret-looking variable planted on the daemon side is absent from the check's `env`, while
    /// the isolation overrides and `PATH` are present.
    #[cfg(unix)]
    #[test]
    fn a_check_sees_a_minimal_environment_never_the_daemons_secrets() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        const PLANTED: &str = "WICKED_TEST_PLANTED_GH_TOKEN";
        std::env::set_var(PLANTED, "hunter2");
        let wt = scratch("min-env");
        let sandbox = sandbox_for(&wt);
        let scratch = CheckScratch::prepare(&wt).unwrap();
        let env_dump = RepoCheck {
            name: "test".into(),
            argv: s(&["sh", "-c", "env"]),
            source: "fixture".into(),
            timeout_s: None,
            dir: String::new(),
        };
        let r = run_one(&wt, &env_dump, &sandbox, &scratch, Tree::Head);
        std::env::remove_var(PLANTED);
        assert!(r.passed(), "{r:?}");
        let out = &r.stdout_tail;
        assert!(
            !out.contains(PLANTED),
            "the planted daemon-side secret reached the check:\n{out}"
        );
        // Nothing secret-looking from the daemon's environment either — by name, whatever is set.
        let leaked: Vec<String> = std::env::vars_os()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .filter(|k| {
                let u = k.to_ascii_uppercase();
                u.contains("TOKEN")
                    || u.contains("SECRET")
                    || u.contains("API_KEY")
                    || u.contains("PASSWORD")
            })
            .filter(|k| out.lines().any(|l| l.starts_with(&format!("{k}="))))
            .collect();
        assert!(leaked.is_empty(), "leaked into the check: {leaked:?}");
        assert!(
            !out.lines().any(|l| l.starts_with("WICKED_")),
            "no WICKED_* variable reaches a check:\n{out}"
        );
        // The allow-list and the isolation overrides ARE there — all in the tree's leaf of the
        // engine's check cache, outside the worktree.
        assert_eq!(scratch.root(), leaf_of(&wt));
        assert!(!scratch.root().starts_with(&wt));
        let home_line = format!("HOME={}", scratch.home().display());
        assert!(out.lines().any(|l| l == home_line), "{out}");
        assert!(out.lines().any(|l| l == "CI=1"), "{out}");
        assert!(out.lines().any(|l| l.starts_with("PATH=")), "{out}");
        assert!(
            out.lines().any(|l| l.starts_with("CARGO_TARGET_DIR=")),
            "{out}"
        );
        // core#480: the head run builds into its own target leaf, the base run into another;
        // core#489: `TMPDIR` is the scratch's private dir under the system temp dir, not a leaf
        // of the worktree.
        let head_target = format!(
            "CARGO_TARGET_DIR={}",
            scratch.root.join("cargo-target").join("head").display()
        );
        assert!(out.lines().any(|l| l == head_target), "{out}");
        let tmp_line = format!("TMPDIR={}", scratch.tmp.path().display());
        assert!(out.lines().any(|l| l == tmp_line), "{out}");
        assert!(
            !scratch.tmp.path().starts_with(&wt),
            "TMPDIR must leave the worktree: {}",
            scratch.tmp.path().display()
        );
        let base = run_one(&wt, &env_dump, &sandbox, &scratch, Tree::Base);
        assert!(base.passed(), "{base:?}");
        let base_target = format!(
            "CARGO_TARGET_DIR={}",
            scratch.root.join("cargo-target").join("base").display()
        );
        assert!(
            base.stdout_tail.lines().any(|l| l == base_target),
            "{}",
            base.stdout_tail
        );
    }

    /// core#489: the checks' `TMPDIR` is a short private dir under the system temp dir — a socket
    /// bound under it stays inside `sun_path` however deep the worktree is — recorded on the env
    /// payload, inside the armed boundary, and gone when the floor is. Skips without python3 (the
    /// socket binder) or a sandbox tool.
    #[cfg(unix)]
    #[test]
    fn the_checks_tmpdir_is_short_private_outside_the_worktree_and_reaped_489() {
        if crate::validator::find_on_path("python3").is_none() {
            eprintln!("repo_checks: python3 not on PATH — the socket binder cannot run here");
            return;
        }
        // A worktree nested past the socket limit (104 bytes on macOS): a `TMPDIR` under it would
        // put `x.sock` well past that.
        let mut wt = scratch("deep-tmpdir");
        while wt.to_string_lossy().len() < 120 {
            wt = wt.join("a-deeper-directory-level");
        }
        std::fs::create_dir_all(wt.join(".wicked")).unwrap();
        let binder = "import os, socket, stat\n\
                      t = os.environ['TMPDIR']\n\
                      assert os.environ['TMP'] == t and os.environ['TEMP'] == t\n\
                      print('TMPDIR=' + t)\n\
                      print('MODE=' + oct(stat.S_IMODE(os.stat(t).st_mode)))\n\
                      s = socket.socket(socket.AF_UNIX)\n\
                      s.bind(os.path.join(t, 'x.sock'))\n\
                      s.listen(1)\n\
                      print('BOUND')\n";
        std::fs::write(
            wt.join(CONFIG_PATH),
            serde_json::json!({ "test": ["python3", "-c", binder] }).to_string(),
        )
        .unwrap();
        let report = run_floor(&wt, &FloorContext::default());
        if report.sandbox_error.is_some() {
            eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
            return;
        }
        assert!(report.passed, "{report:?}");
        let env = report
            .env
            .as_ref()
            .expect("the env record rides a floor that ran");
        let tmpdir = Path::new(&env.tmpdir);
        assert!(
            env.tmpdir.len() < 60,
            "TMPDIR must stay short: {} ({} bytes)",
            env.tmpdir,
            env.tmpdir.len()
        );
        assert!(tmpdir.starts_with(std::env::temp_dir()), "{}", env.tmpdir);
        assert!(
            !tmpdir.starts_with(&wt),
            "TMPDIR left the worktree: {}",
            env.tmpdir
        );
        assert!(
            tmpdir
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.len() == 9 && n.starts_with("wc-")),
            "{}",
            env.tmpdir
        );
        let out = &report.checks[0].stdout_tail;
        assert!(out.contains(&format!("TMPDIR={}", env.tmpdir)), "{out}");
        assert!(out.contains("MODE=0o700"), "{out}");
        assert!(out.contains("BOUND"), "{out}");
        assert!(
            !tmpdir.exists(),
            "the private TMPDIR is reaped with the floor: {}",
            env.tmpdir
        );
        // And the check scratch carries no `tmp` leaf any more — nor is it in the worktree.
        assert!(!leaf_of(&wt).join("tmp").exists());
        assert_no_worktree_scratch(&wt);
    }

    /// The private `TMPDIR` refuses an existing path at the drawn name — a directory or a SYMLINK
    /// another local user planted on a shared sticky temp dir — and simply draws again: nothing
    /// is followed, nothing fail-closes, and the result is mode 0700. Every draw taken is an
    /// error, never a follow.
    #[cfg(unix)]
    #[test]
    fn the_private_tmpdir_refuses_a_planted_name_and_draws_again() {
        use std::os::unix::fs::PermissionsExt;
        let base = scratch("tmp-draw");
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, base.join("wc-taken1")).unwrap();
        std::fs::create_dir_all(base.join("wc-taken2")).unwrap();
        let mut draws = ["wc-taken1", "wc-taken2", "wc-fresh0"].into_iter();
        let tmp =
            create_private_tmp(&base, || draws.next().expect("bounded draws").to_string()).unwrap();
        assert_eq!(tmp, base.join("wc-fresh0"));
        assert_eq!(
            std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(
            std::fs::symlink_metadata(base.join("wc-taken1"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the planted link is untouched"
        );
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing was created through the link"
        );
        let err = create_private_tmp(&base, || "wc-taken1".to_string())
            .expect_err("every draw taken is an error");
        assert!(err.to_string().contains("no unused"), "{err}");
    }

    /// core#493: a launcher that dies before exec (`bwrap: Can't mkdir <HOME>/.aws: Read-only
    /// file system`, exit 1) never ran the check — the record is `could_not_run` carrying the
    /// launcher's line, not the check's `failed`. A check that RAN and exited 1 under the same
    /// wrapper is still `failed`; a passing exit is never reclassified whatever it printed.
    #[cfg(unix)]
    #[test]
    fn a_launcher_that_fails_to_arm_is_could_not_run_never_the_checks_failure_493() {
        use std::os::unix::fs::PermissionsExt;
        let wt = scratch("launcher-fail");
        let fake = wt.join("fake-bwrap");
        std::fs::write(
            &fake,
            "#!/bin/sh\n\
             if [ \"$1\" = \"--fail\" ]; then\n\
               echo \"bwrap: Can't mkdir /nonexistent-home/.aws: Read-only file system\" >&2\n\
               exit 1\n\
             fi\n\
             shift\n\
             exec \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let armed = |flag: &str| WorkerSandbox {
            wrapper: vec![fake.to_string_lossy().into_owned(), flag.to_string()],
            level: SandboxLevel::Sandboxed,
            downgrade_reason: None,
        };
        let scratch = CheckScratch::prepare(&wt).unwrap();
        let check = |script: &str| RepoCheck {
            name: "test".into(),
            argv: s(&["sh", "-c", script]),
            source: "fixture".into(),
            timeout_s: None,
            dir: String::new(),
        };
        let dead = run_one(
            &wt,
            &check("exit 0"),
            &armed("--fail"),
            &scratch,
            Tree::Head,
        );
        assert_eq!(dead.outcome(), "could_not_run", "{dead:?}");
        assert_eq!(dead.exit_code, Some(1));
        assert!(
            dead.spawn_error.as_deref().is_some_and(|e| {
                e.starts_with(
                    "the OS sandbox launcher exited before the check ran: bwrap: Can't mkdir",
                )
            }),
            "{dead:?}"
        );
        assert!(
            dead.denies(),
            "fail-closed: a floor that could not run still denies"
        );
        assert!(
            dead.summary().contains("exited before the check ran"),
            "{}",
            dead.summary()
        );
        let red = run_one(
            &wt,
            &check("echo 'error: test failed' >&2; exit 1"),
            &armed("--"),
            &scratch,
            Tree::Head,
        );
        assert_eq!(red.outcome(), "failed", "{red:?}");
        assert!(red.spawn_error.is_none());
        let green = run_one(
            &wt,
            &check("echo 'bwrap: only printed' >&2; exit 0"),
            &armed("--"),
            &scratch,
            Tree::Head,
        );
        assert_eq!(green.outcome(), "passed", "{green:?}");
    }

    /// Adversarial review on #414: a descendant that escapes the process group (`setsid`) and
    /// keeps the check's stdout open must not wedge the verify unit — the drain is bounded and
    /// the tail read so far is reported.
    #[cfg(unix)]
    #[test]
    fn a_detached_pipe_holder_cannot_wedge_the_check() {
        // spawn-audit: test-only — probes whether perl exists to build the detached pipe holder.
        if Command::new("perl").arg("-e").arg("1").output().is_err() {
            eprintln!("perl not on PATH — the detached pipe-holder test cannot run here");
            return;
        }
        let wt = scratch("pipe-holder");
        let sandbox = sandbox_for(&wt);
        let scratch = CheckScratch::prepare(&wt).unwrap();
        // The check prints, then leaves a setsid'd perl holding stdout for 60 s, and exits 0.
        let holder = RepoCheck {
            name: "test".into(),
            argv: s(&[
                "sh",
                "-c",
                "echo before-holder; perl -e 'use POSIX; POSIX::setsid(); sleep 60' & exit 0",
            ]),
            source: "fixture".into(),
            timeout_s: None,
            dir: String::new(),
        };
        let started = Instant::now();
        let r = run_one(&wt, &holder, &sandbox, &scratch, Tree::Head);
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the drain must be bounded (took {:?})",
            started.elapsed()
        );
        assert!(r.passed(), "{r:?}");
        assert!(r.stdout_tail.contains("before-holder"), "{r:?}");
    }

    /// Adversarial review on #414: a checkout that ships `tmp` as a SYMLINK must not have the
    /// checks' scratch created through it — the floor is refused (core#1050: still, though most
    /// of the scratch moved to the engine's check cache; the base export lives under `tmp`). A
    /// link at a scratch LEAF (an earlier floor's check can write the leaf) is refused too.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_scratch_root_is_refused_not_followed() {
        let base = scratch("symlink-tmp");
        let wt = base.join("wt");
        let outside = base.join("outside");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, wt.join(crate::worktree_guard::ENGINE_SCRATCH_DIR))
            .unwrap();
        let err = CheckScratch::prepare(&wt).expect_err("a symlinked tmp is refused");
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing was created through the worktree's `tmp` link"
        );
        std::fs::remove_file(wt.join(crate::worktree_guard::ENGINE_SCRATCH_DIR)).unwrap();
        drop(CheckScratch::prepare(&wt).unwrap());
        // A link planted at a leaf of the check scratch itself.
        let leaf = leaf_of(&wt);
        std::fs::remove_dir_all(leaf.join("home")).unwrap();
        std::os::unix::fs::symlink(&outside, leaf.join("home")).unwrap();
        let err = CheckScratch::prepare(&wt).expect_err("a symlinked leaf is refused");
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing was created through the leaf's link"
        );
        // The floor reports the refusal as a detection error, fail-closed — with a boundary
        // injected so the scratch step is reached on every host (no tool ⇒ the earlier
        // fail-closed branch would answer first).
        let report = run_with_sandbox(
            &wt,
            WorkerSandbox {
                wrapper: vec!["true".to_string()],
                level: SandboxLevel::Sandboxed,
                downgrade_reason: None,
            },
        );
        assert!(!report.passed);
        assert!(
            report
                .detect_error
                .as_deref()
                .is_some_and(|e| e.contains("symlink")),
            "{report:?}"
        );
        std::fs::remove_file(leaf.join("home")).unwrap();
        assert!(remove_check_scratch_in(
            &check_cache_root().unwrap(),
            &wt,
            None
        ));
    }

    /// (2026-10-20 disk incident) The check scratch lives OUTSIDE the worktree, in the engine's
    /// check cache: grouped by repository (the directory its run worktrees sit in, labelled with
    /// the repository's name), one leaf per run tree, and the same leaf is derived after the tree
    /// is gone (the reaper's view).
    #[test]
    fn the_check_scratch_lives_outside_the_worktree_keyed_by_repository() {
        let base = scratch("cache-key");
        let cache = base.join("cache");
        let a = base.join("myrepo").join("wicked-worktrees").join("run-a");
        let b = base.join("myrepo").join("wicked-worktrees").join("run-b");
        let other = base.join("other").join("wicked-worktrees").join("run-a");
        for d in [&a, &b, &other] {
            std::fs::create_dir_all(d).unwrap();
        }
        let (la, lb, lo) = (
            scratch_dir_for(&cache, &a, None),
            scratch_dir_for(&cache, &b, None),
            scratch_dir_for(&cache, &other, None),
        );
        assert!(la.starts_with(&cache), "{}", la.display());
        assert!(!la.starts_with(base.join("myrepo")), "{}", la.display());
        assert_eq!(la.parent(), lb.parent(), "one group per repository");
        assert_ne!(la, lb, "one leaf per run tree");
        assert_ne!(
            la.parent(),
            lo.parent(),
            "another repository is another group"
        );
        let name = |p: &Path| p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name(la.parent().unwrap()).starts_with("myrepo-"),
            "{}",
            la.display()
        );
        assert!(name(&la).starts_with("run-a-"), "{}", la.display());
        std::fs::remove_dir_all(&a).unwrap();
        assert_eq!(
            scratch_dir_for(&cache, &a, None),
            la,
            "derivable once the tree is gone"
        );
        // Preparing makes every leaf there and writes nothing into the worktree.
        let prepared = CheckScratch::prepare_in(&cache, &b, None, u64::MAX).unwrap();
        assert_eq!(prepared.root(), lb);
        for sub in SCRATCH_LEAVES {
            assert!(lb.join(sub).is_dir(), "{sub}");
        }
        assert!(
            std::fs::read_dir(&b).unwrap().next().is_none(),
            "nothing is written into the worktree"
        );
        drop(prepared);
        assert!(remove_check_scratch_in(&cache, &b, None));
        assert!(!lb.exists());
        assert!(lease_path(&lb).is_file(), "the 0-byte lock file is kept");
        assert!(b.is_dir(), "the worktree itself is never touched");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The per-repository cap: over it, the least recently used OTHER run leaves go first, a
    /// leaf a floor holds a lease on is never taken (two floors starting together never evict
    /// each other), and the floor's own leaf is cleaned only when it alone is over the cap —
    /// then re-made empty for the floor about to run.
    #[test]
    fn the_cap_removes_least_recently_used_leaves_first_and_never_a_leased_one() {
        let base = scratch("cache-cap");
        let cache = base.join("cache");
        let wts = base.join("repo").join("wicked-worktrees");
        let ago = |s: u64| std::time::SystemTime::now() - std::time::Duration::from_secs(s);
        let fill = |name: &str, bytes: usize, age_s: u64| {
            let wt = wts.join(name);
            std::fs::create_dir_all(&wt).unwrap();
            let leaf = scratch_dir_for(&cache, &wt, None);
            std::fs::create_dir_all(leaf.join("cargo-target").join("head")).unwrap();
            std::fs::write(leaf.join("cargo-target/head/blob"), vec![0u8; bytes]).unwrap();
            let lock = std::fs::File::create(lease_path(&leaf)).unwrap();
            lock.set_modified(ago(age_s)).unwrap();
            (wt, leaf)
        };
        let (_, old) = fill("old", 600, 300);
        let (_, newer) = fill("newer", 600, 100);
        let (_, busy) = fill("busy", 600, 50);
        let (own_wt, own) = fill("own", 0, 10);
        // A floor of the `busy` run is mid-build; it is also the least recently used.
        let held = lock_leaf(&busy, false).unwrap().unwrap();
        held.set_modified(ago(400)).unwrap();
        assert_eq!(
            enforce_check_cache_cap(&cache, &own, CacheCaps::uniform(10_000)),
            (vec![], false),
            "under the cap"
        );
        // Two active floors (`busy` and this one) over the cap together: neither leaf is taken
        // for the other, and this floor's own leaf (under the cap alone) is not cleaned.
        let own_lease = lock_leaf(&own, false).unwrap().unwrap();
        own_lease.set_modified(ago(10)).unwrap();
        let (removed, own_over) = enforce_check_cache_cap(&cache, &own, CacheCaps::uniform(1000));
        assert!(!own_over);
        assert_eq!(
            removed,
            vec![old.clone(), newer.clone()],
            "LRU first, the leased one skipped"
        );
        assert!(
            busy.join("cargo-target/head/blob").is_file(),
            "a leased leaf is never removed"
        );
        assert!(own.is_dir(), "the group fits once the others are gone");
        drop(own_lease);
        drop(held);
        // The floor's own leaf alone is over the cap: the others go, then its own — and the
        // floor gets a fresh, empty scratch.
        std::fs::write(own.join("cargo-target/head/blob"), vec![0u8; 1500]).unwrap();
        let prepared = CheckScratch::prepare_in(&cache, &own_wt, None, 1000).unwrap();
        assert!(
            !busy.exists(),
            "an unleased leaf goes before the floor's own"
        );
        assert!(
            !own.join("cargo-target/head/blob").exists(),
            "the own leaf was cleaned"
        );
        assert!(
            own.join("cargo-target/head").is_dir(),
            "and re-made for this floor"
        );
        assert_eq!(prepared.root(), own);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A leaf of run `name` over `<base>/<repo>/wicked-worktrees/<name>` holding `bytes` of build
    /// output, last used `age_s` seconds ago.
    fn cap_leaf(
        base: &Path,
        repo: &str,
        name: &str,
        bytes: usize,
        age_s: u64,
    ) -> (PathBuf, PathBuf) {
        let wt = base.join(repo).join("wicked-worktrees").join(name);
        std::fs::create_dir_all(&wt).unwrap();
        let leaf = scratch_dir_for(&base.join("cache"), &wt, Some(name));
        std::fs::create_dir_all(leaf.join("cargo-target").join("head")).unwrap();
        std::fs::write(leaf.join("cargo-target/head/blob"), vec![0u8; bytes]).unwrap();
        let lock = std::fs::File::create(lease_path(&leaf)).unwrap();
        lock.set_modified(std::time::SystemTime::now() - Duration::from_secs(age_s))
            .unwrap();
        (wt, leaf)
    }

    /// (core#1122) The TOTAL cap evicts across repositories: every repository is under its own
    /// cap, the cache as a whole is not, and the least recently used leaves of ANY repository go
    /// first until it fits. A leaf a floor holds (here the oldest of all) is never taken, and the
    /// floor's own leaf is never taken for the others.
    #[test]
    fn the_total_cap_evicts_least_recently_used_across_repositories_never_a_leased_leaf() {
        let base = scratch("cache-total-cap");
        let cache = base.join("cache");
        let (_, running) = cap_leaf(&base, "core", "running", 600, 900);
        let (_, core_old) = cap_leaf(&base, "core", "old", 600, 500);
        let (_, crew_old) = cap_leaf(&base, "crew", "older", 600, 700);
        let (_, crew_new) = cap_leaf(&base, "crew", "new", 600, 100);
        let (own_wt, own) = cap_leaf(&base, "garden", "own", 600, 10);
        assert_ne!(running.parent(), crew_old.parent(), "two repositories");
        // A floor of `running` is mid-build.
        let held = lock_leaf(&running, false).unwrap().unwrap();
        held.set_modified(std::time::SystemTime::now() - Duration::from_secs(900))
            .unwrap();
        let own_lease = lock_leaf(&own, false).unwrap().unwrap();
        let caps = CacheCaps {
            repo: 10_000,
            total: 2_000,
            idle: CHECK_CACHE_IDLE_TTL,
            ..CacheCaps::uniform(0)
        };
        let (removed, own_over) = enforce_check_cache_cap(&cache, &own, caps);
        assert!(!own_over);
        assert_eq!(
            removed,
            vec![crew_old.clone(), core_old.clone()],
            "LRU across repositories, the leased leaf skipped, then stop once it fits"
        );
        assert!(
            running.join("cargo-target/head/blob").is_file(),
            "never a leased leaf"
        );
        assert!(
            crew_new.join("cargo-target/head/blob").is_file(),
            "the cache fits"
        );
        assert!(
            own.join("cargo-target/head/blob").is_file(),
            "never the floor's own"
        );
        drop(own_lease);
        // The same caps through the floor's own prepare: under them now, nothing more goes.
        let prepared = CheckScratch::prepare_with(&cache, &own_wt, Some("own"), caps).unwrap();
        assert!(crew_new.is_dir() && running.is_dir());
        drop(prepared);
        drop(held);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (codex on #1124) An orphaned leaf a floor still holds cannot be removed, but its bytes are on
    /// disk: they count against the total cap, so an unleased leaf of another repository goes.
    /// A leaf gone since the scan is never reported as this pass's eviction.
    #[test]
    fn a_held_orphan_counts_against_the_total_cap_and_a_vanished_leaf_is_not_an_eviction() {
        let base = scratch("cache-held-orphan");
        let cache = base.join("cache");
        let (_, orphan) = cap_leaf(&base, "core", "orphan", 800, 50);
        let (_, parked) = cap_leaf(&base, "crew", "parked", 600, 100);
        let (_, own) = cap_leaf(&base, "garden", "own", 400, 0);
        let held = lock_leaf(&orphan, false).unwrap().unwrap();
        std::fs::write(orphan_path(&orphan), b"").unwrap();
        let caps = CacheCaps {
            repo: u64::MAX,
            total: 1_200,
            idle: CHECK_CACHE_IDLE_TTL,
            ..CacheCaps::uniform(0)
        };
        let (removed, own_over) = enforce_check_cache_cap(&cache, &own, caps);
        assert!(!own_over);
        assert_eq!(
            removed,
            vec![parked.clone()],
            "800 + 600 + 400 is over 1200"
        );
        assert!(
            orphan.join("cargo-target/head/blob").is_file(),
            "the held orphan stays"
        );
        drop(held);
        // A leaf whose directory is gone (a reap between scan and lock) is not this pass's removal.
        assert_eq!(evict_leaf(&parked), Evicted::AlreadyGone);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (core#1122) The cache's own retention: a leaf unused past [`CacheCaps::idle`] goes whatever
    /// the caps (a run whose terminal reap never came), unless a floor holds it; a recent leaf stays.
    #[test]
    fn an_idle_leaf_goes_whatever_the_caps_unless_a_floor_holds_it() {
        let base = scratch("cache-idle");
        let cache = base.join("cache");
        let day = 86_400;
        let (_, stale) = cap_leaf(&base, "core", "stale", 10, 9 * day);
        let (_, stale_held) = cap_leaf(&base, "crew", "stale-held", 10, 9 * day);
        let (_, recent) = cap_leaf(&base, "crew", "recent", 10, day);
        let (_, own) = cap_leaf(&base, "core", "own", 10, 0);
        let held = lock_leaf(&stale_held, false).unwrap().unwrap();
        held.set_modified(std::time::SystemTime::now() - Duration::from_secs(9 * day))
            .unwrap();
        let caps = CacheCaps {
            repo: u64::MAX,
            total: u64::MAX,
            idle: Duration::from_secs(7 * day),
            ..CacheCaps::uniform(0)
        };
        let (removed, own_over) = enforce_check_cache_cap(&cache, &own, caps);
        assert_eq!(removed, vec![stale.clone()]);
        assert!(!own_over);
        assert!(!stale.exists());
        assert!(stale_held.is_dir(), "a leaf a floor holds is never taken");
        assert!(recent.is_dir() && own.is_dir());
        drop(held);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (core#1122) The cap pass only ever removes run leaves: real directories two levels under the
    /// cache. A LINK planted as a group or as a leaf (pointing outside the cache) is never
    /// followed, so nothing outside the cache is ever measured or removed, even at a 0-byte cap.
    #[cfg(unix)]
    #[test]
    fn the_cap_pass_never_removes_anything_outside_the_cache() {
        let base = scratch("cache-outside");
        let cache = base.join("cache");
        let outside = base.join("outside");
        std::fs::create_dir_all(outside.join("leafish").join("cargo-target")).unwrap();
        std::fs::write(outside.join("leafish/cargo-target/blob"), vec![0u8; 600]).unwrap();
        let (_, own) = cap_leaf(&base, "core", "own", 10, 0);
        let (_, victim) = cap_leaf(&base, "core", "victim", 600, 100);
        std::os::unix::fs::symlink(&outside, cache.join("linked-group")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("leafish"),
            own.parent().unwrap().join("linked-leaf"),
        )
        .unwrap();
        let (removed, _) = enforce_check_cache_cap(&cache, &own, CacheCaps::uniform(0));
        assert_eq!(removed, vec![victim], "only the real run leaf");
        assert!(outside.join("leafish/cargo-target/blob").is_file());
        assert!(own
            .parent()
            .unwrap()
            .join("linked-leaf")
            .symlink_metadata()
            .is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Caps whose fixed limits are out of reach, so only the free-disk floor can bind: `floor`
    /// bytes kept free on a simulated volume of `volume` bytes with `free` free.
    fn floor_caps(floor: u64, volume: u64, free: u64) -> CacheCaps {
        CacheCaps {
            free_floor: FreeFloor { pct: 0, min: floor },
            disk: DiskProbe::Simulated {
                space: VolumeSpace {
                    total: volume,
                    free,
                },
                reclaim_pct: 100,
            },
            ..CacheCaps::uniform(u64::MAX)
        }
    }

    /// (core#1138) A volume below its free-disk floor: the cache is far under its fixed caps, yet
    /// the least recently used leaves no floor holds go until the floor holds again (here 1000
    /// bytes short: two 600-byte leaves), then the pass stops. The oldest leaf of all is leased
    /// by a running floor and is never taken; with the floor off, the same cache keeps everything.
    #[test]
    fn a_volume_below_its_free_floor_evicts_below_the_fixed_cap_never_a_leased_leaf() {
        let base = scratch("cache-free-floor");
        let cache = base.join("cache");
        let (_, running) = cap_leaf(&base, "core", "running", 600, 900);
        let (_, old) = cap_leaf(&base, "core", "old", 600, 500);
        let (_, mid) = cap_leaf(&base, "crew", "mid", 600, 300);
        let (_, recent) = cap_leaf(&base, "crew", "recent", 600, 100);
        let (_, own) = cap_leaf(&base, "garden", "own", 600, 10);
        let held = lock_leaf(&running, false).unwrap().unwrap();
        held.set_modified(std::time::SystemTime::now() - Duration::from_secs(900))
            .unwrap();
        let own_lease = lock_leaf(&own, false).unwrap().unwrap();
        let off = CacheCaps {
            free_floor: FreeFloor::OFF,
            ..floor_caps(10_000, 100_000, 9_000)
        };
        assert_eq!(
            enforce_check_cache_cap(&cache, &own, off),
            (vec![], false),
            "the fixed caps alone keep every leaf"
        );
        // 3000 bytes cached, 9000 free, a 10000-byte floor: the cache may hold 2000.
        let caps = floor_caps(10_000, 100_000, 9_000);
        assert_eq!(
            effective_total_cap(caps, 3_000, caps.disk.space(&cache)),
            2_000
        );
        let (removed, own_over) = enforce_check_cache_cap(&cache, &own, caps);
        assert!(!own_over, "the floor never cleans the floor's own leaf");
        assert_eq!(
            removed,
            vec![old.clone(), mid.clone()],
            "LRU first, the leased leaf skipped, then stop once the floor holds"
        );
        assert!(
            running.join("cargo-target/head/blob").is_file(),
            "never a leased leaf"
        );
        assert!(recent.join("cargo-target/head/blob").is_file());
        assert!(own.join("cargo-target/head/blob").is_file());
        drop(own_lease);
        drop(held);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (codex on #1140) A removal can give back less than its leaf's apparent bytes (cargo
    /// hard-links its outputs): the floor is held on fresh readings of the volume, so the pass
    /// keeps going until a reading shows the floor holds. Here each 600-byte leaf gives back 300,
    /// so the 1000-byte shortfall takes four leaves, not the two apparent bytes would credit; the
    /// leased leaf is still never taken.
    #[test]
    fn the_free_floor_is_held_on_fresh_readings_not_on_apparent_bytes() {
        let base = scratch("cache-free-floor-reclaim");
        let cache = base.join("cache");
        let (_, running) = cap_leaf(&base, "core", "running", 600, 900);
        let (_, old) = cap_leaf(&base, "core", "old", 600, 500);
        let (_, mid) = cap_leaf(&base, "crew", "mid", 600, 300);
        let (_, recent) = cap_leaf(&base, "crew", "recent", 600, 100);
        let (_, newest) = cap_leaf(&base, "crew", "newest", 600, 50);
        let (_, own) = cap_leaf(&base, "garden", "own", 600, 10);
        let held = lock_leaf(&running, false).unwrap().unwrap();
        held.set_modified(std::time::SystemTime::now() - Duration::from_secs(900))
            .unwrap();
        let caps = CacheCaps {
            disk: DiskProbe::Simulated {
                space: VolumeSpace {
                    total: 100_000,
                    free: 9_000,
                },
                reclaim_pct: 50,
            },
            ..floor_caps(10_000, 100_000, 9_000)
        };
        let (removed, own_over) = enforce_check_cache_cap(&cache, &own, caps);
        assert!(!own_over);
        assert_eq!(
            removed,
            vec![old, mid, recent, newest],
            "three give back 900 of the 1000 short, so a fourth goes"
        );
        assert!(
            running.join("cargo-target/head/blob").is_file(),
            "never a leased leaf"
        );
        assert!(own.join("cargo-target/head/blob").is_file());
        drop(held);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (codex on #1140) The closing shortfall line fires whenever the latest reading is below the
    /// floor, even when the cache is empty, and never blames an empty cache on running floors.
    #[test]
    fn the_shortfall_line_fires_on_the_reading_even_for_an_empty_cache() {
        let cache = Path::new("cache");
        assert_eq!(
            floor_shortfall(cache, Some(5_000), Some(5_000), 1_000),
            None
        );
        assert_eq!(
            floor_shortfall(cache, None, None, 1_000),
            None,
            "no reading, no floor"
        );
        let empty = floor_shortfall(cache, Some(1_000_000), Some(100), 0).unwrap();
        assert!(empty.contains("holds nothing more to remove"), "{empty}");
        assert!(!empty.contains("running"), "{empty}");
        let held = floor_shortfall(cache, Some(1_000_000_000), Some(0), 2_000_000_000).unwrap();
        assert!(
            held.contains("held by runs whose floors are running"),
            "{held}"
        );
        assert!(held.contains("1.0 GB short of its 1.0 GB floor"), "{held}");
    }

    /// (core#1138) A volume so full no eviction restores the floor: every leaf no floor holds goes,
    /// and the leases running floors hold (and this floor's own) are never taken — the shortfall
    /// is left for the next pass and for [`floor_cache_usage`] to report. Through the floor's own
    /// prepare as well: it neither cleans its own leaf nor fails.
    #[test]
    fn a_floor_that_cannot_be_restored_leaves_only_leased_leaves_and_never_takes_one() {
        let base = scratch("cache-free-floor-short");
        let cache = base.join("cache");
        let (_, held_a) = cap_leaf(&base, "core", "held-a", 500, 700);
        let (_, held_b) = cap_leaf(&base, "crew", "held-b", 500, 600);
        let (_, idle_a) = cap_leaf(&base, "core", "idle-a", 500, 400);
        let (_, idle_b) = cap_leaf(&base, "crew", "idle-b", 500, 200);
        let (own_wt, own) = cap_leaf(&base, "garden", "own", 500, 10);
        let a = lock_leaf(&held_a, false).unwrap().unwrap();
        let b = lock_leaf(&held_b, false).unwrap().unwrap();
        let caps = floor_caps(1_000_000, 10_000_000, 0);
        let prepared = CheckScratch::prepare_with(&cache, &own_wt, Some("own"), caps).unwrap();
        assert!(
            !idle_a.exists() && !idle_b.exists(),
            "every unleased leaf went"
        );
        for leaf in [&held_a, &held_b, &own] {
            assert!(
                leaf.join("cargo-target/head/blob").is_file(),
                "a leased leaf is never evicted: {}",
                leaf.display()
            );
        }
        // Nothing left to take: a second pass removes nothing.
        let (removed, own_over) = enforce_check_cache_cap(&cache, &own, caps);
        assert_eq!((removed, own_over), (vec![], false));
        drop(prepared);
        drop((a, b));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (core#1138) The free-disk floor at its most aggressive (no free byte, a floor larger than
    /// the volume) still only ever removes run leaves under the cache: a directory beside the
    /// cache root, a LINKED group and a LINKED leaf (both pointing outside), the cache's own lock
    /// and every lease file are left as they were.
    #[cfg(unix)]
    #[test]
    fn the_free_floor_never_touches_anything_outside_the_cache_root() {
        let base = scratch("cache-free-floor-outside");
        let cache = base.join("cache");
        let outside = base.join("outside");
        std::fs::create_dir_all(outside.join("leafish").join("cargo-target")).unwrap();
        std::fs::write(outside.join("leafish/cargo-target/blob"), vec![0u8; 600]).unwrap();
        // A sibling of the cache root that looks like a group with a leaf.
        let beside = base.join("beside").join("core-x").join("r-y");
        std::fs::create_dir_all(&beside).unwrap();
        std::fs::write(beside.join("blob"), vec![0u8; 600]).unwrap();
        let (_, own) = cap_leaf(&base, "core", "own", 10, 0);
        let (_, victim) = cap_leaf(&base, "core", "victim", 600, 100);
        std::os::unix::fs::symlink(&outside, cache.join("linked-group")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("leafish"),
            own.parent().unwrap().join("linked-leaf"),
        )
        .unwrap();
        let caps = floor_caps(u64::MAX, 1_000, 0);
        let (removed, _) = enforce_check_cache_cap(&cache, &own, caps);
        assert_eq!(removed, vec![victim.clone()], "only the real run leaf");
        assert!(outside.join("leafish/cargo-target/blob").is_file());
        assert!(beside.join("blob").is_file(), "never beside the cache root");
        assert!(cache.join("linked-group").symlink_metadata().is_ok());
        assert!(own
            .parent()
            .unwrap()
            .join("linked-leaf")
            .symlink_metadata()
            .is_ok());
        assert!(cache.join(CAP_PASS_LOCK).is_file());
        assert!(
            lease_path(&victim).is_file(),
            "the 0-byte lock file is kept"
        );
        assert!(lease_path(&own).is_file());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (core#1138) The floor is the larger of its share of the volume and its GB minimum, both
    /// configurable; a value that does not parse keeps the default, and over 100% reads as 100%.
    #[test]
    fn the_free_floor_is_the_larger_of_its_share_and_its_minimum() {
        let gb = 1_000_000_000u64;
        let d = FreeFloor::parse(None, None);
        assert_eq!(
            d,
            FreeFloor {
                pct: 15,
                min: 20 * gb
            }
        );
        assert_eq!(d.bytes(500 * gb), 75 * gb, "15% of a 500 GB volume");
        assert_eq!(
            d.bytes(100 * gb),
            20 * gb,
            "the 20 GB minimum on a small one"
        );
        assert_eq!(
            FreeFloor::parse(Some(" 5 "), Some("x")),
            FreeFloor {
                pct: 5,
                min: 20 * gb
            }
        );
        assert_eq!(FreeFloor::parse(Some("250"), Some("0")).pct, 100);
        assert_eq!(FreeFloor::parse(Some("0"), Some("0")), FreeFloor::OFF);
        assert_eq!(FreeFloor::OFF.bytes(u64::MAX), 0);
        assert_eq!(
            FreeFloor { pct: 100, min: 0 }.bytes(u64::MAX),
            u64::MAX,
            "no overflow"
        );
        // An unreadable volume: the fixed cap alone.
        let caps = CacheCaps {
            free_floor: d,
            ..CacheCaps::uniform(40 * gb)
        };
        assert_eq!(effective_total_cap(caps, 25 * gb, None), 40 * gb);
        // The rig's reading (core#1138): 25 GB cached, 23 GB free on a 460 GB volume. The floor
        // is 69 GB, more than the cache could ever give back: every unleased leaf goes.
        let rig = VolumeSpace {
            total: 460 * gb,
            free: 23 * gb,
        };
        assert_eq!(effective_total_cap(caps, 25 * gb, Some(rig)), 0);
        // Plenty free: the fixed cap binds.
        let roomy = VolumeSpace {
            total: 460 * gb,
            free: 300 * gb,
        };
        assert_eq!(effective_total_cap(caps, 25 * gb, Some(roomy)), 40 * gb);
    }

    /// The real probe reads the volume a directory is on.
    #[cfg(unix)]
    #[test]
    fn the_volume_probe_reads_the_cache_volume() {
        let base = scratch("cache-volume-probe");
        let s = DiskProbe::Volume
            .space(&base)
            .expect("statvfs reads the volume");
        assert!(s.total > 0 && s.free <= s.total, "{s:?}");
        assert_eq!(volume_space(&base.join("missing")), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (core#1138) The disk view reports the floor: a simulated volume short of its floor gives a
    /// shortfall, an effective cap below the fixed one, and a note naming the floor cache; a
    /// volume over it has no shortfall and no note; an unreadable one has `null` readings.
    #[test]
    fn the_floor_cache_usage_reports_a_free_disk_shortfall_naming_the_floor_cache() {
        let base = scratch("cache-usage-floor");
        let cache = base.join("cache");
        cap_leaf(&base, "core", "a", 700, 0);
        cap_leaf(&base, "crew", "c", 300, 0);
        let u = floor_cache_usage_in(&cache, floor_caps(5_000, 100_000, 3_000));
        let f = &u["freeDisk"];
        assert_eq!(f["volumeBytes"], 100_000);
        assert_eq!(f["freeBytes"], 3_000);
        assert_eq!(f["floorBytes"], 5_000);
        assert_eq!(f["shortfallBytes"], 2_000);
        assert_eq!(
            f["effectiveCapBytes"], 0,
            "1000 cached + 3000 free - 5000 floor"
        );
        let note = f["note"].as_str().expect("a note when short");
        assert!(note.contains("floor cache"), "{note}");
        assert!(note.contains(&*cache.to_string_lossy()), "{note}");
        let ok = floor_cache_usage_in(&cache, floor_caps(5_000, 100_000, 50_000));
        assert_eq!(ok["freeDisk"]["shortfallBytes"], 0);
        assert!(ok["freeDisk"]["note"].is_null());
        assert_eq!(ok["freeDisk"]["effectiveCapBytes"], 46_000);
        let unknown = floor_cache_usage_in(&cache, CacheCaps::uniform(5_000));
        assert!(unknown["freeDisk"]["freeBytes"].is_null());
        assert!(unknown["freeDisk"]["shortfallBytes"].is_null());
        assert_eq!(unknown["freeDisk"]["effectiveCapBytes"], 5_000);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (core#1122) The disk view crew's diagnostics reads: the cache's path and total, each
    /// repository's bytes (largest first) and leaf count, the caps, and the log store beside it.
    #[test]
    fn the_floor_cache_usage_names_the_path_the_total_and_each_repository() {
        let base = scratch("cache-usage");
        let cache = base.join("cache");
        cap_leaf(&base, "core", "a", 700, 0);
        cap_leaf(&base, "core", "b", 300, 0);
        cap_leaf(&base, "crew", "c", 200, 0);
        std::fs::create_dir_all(base.join(crate::floor_log::LOG_SUBDIR).join("r-x")).unwrap();
        std::fs::write(
            base.join(crate::floor_log::LOG_SUBDIR).join("r-x/blob"),
            [0u8; 50],
        )
        .unwrap();
        let u = floor_cache_usage_in(&cache, CacheCaps::uniform(5_000));
        assert_eq!(u["path"], cache.to_string_lossy().as_ref());
        assert_eq!(u["bytes"], 1200);
        assert_eq!(u["totalCapBytes"], 5000);
        assert_eq!(u["repos"][0]["bytes"], 1000);
        assert_eq!(u["repos"][0]["leaves"], 2);
        assert!(u["repos"][0]["name"].as_str().unwrap().starts_with("core-"));
        assert_eq!(u["repos"][1]["bytes"], 200);
        assert_eq!(u["logs"]["bytes"], 50);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (core#1050) A checkout whose `tmp` is a SYMLINK or a regular FILE refuses the floor —
    /// `passed: false` with main's reason — even though every check would pass; it is never a
    /// logged warning beside a green floor.
    #[cfg(unix)]
    #[test]
    fn a_linked_or_file_tmp_refuses_the_floor_even_when_every_check_passes() {
        for (kind, plant) in [("symlink", true), ("not a directory", false)] {
            let base = scratch(&format!("tmp-refuse-{}", plant));
            let wt = base.join("wt");
            let outside = base.join("outside");
            std::fs::create_dir_all(wt.join(".wicked")).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(
                wt.join(".wicked/checks.json"),
                serde_json::json!({ "test": ["sh", "-c", "exit 0"] }).to_string(),
            )
            .unwrap();
            let tmp = wt.join(crate::worktree_guard::ENGINE_SCRATCH_DIR);
            if plant {
                std::os::unix::fs::symlink(&outside, &tmp).unwrap();
            } else {
                std::fs::write(&tmp, "a file").unwrap();
            }
            let report = run_floor(&wt, &FloorContext::default());
            assert!(!report.passed, "{kind}: {report:?}");
            assert!(report.checks.is_empty(), "{kind}: no check ran: {report:?}");
            let why = report.detect_error.as_deref().unwrap_or_default();
            assert!(why.contains(kind), "{kind}: {why}");
            assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    /// (codex on #1050) Only "not found" is absence: a stale base export the floor cannot even
    /// inspect (its `tmp` has no search permission) refuses the floor, never a pass beside it.
    #[cfg(unix)]
    #[test]
    fn an_uninspectable_base_export_refuses_the_floor() {
        if crate::validator::skip_when_nested("an_uninspectable_base_export_refuses_the_floor") {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let base = scratch("tmp-noperm");
        let wt = base.join("wt");
        std::fs::create_dir_all(wt.join(".wicked")).unwrap();
        std::fs::write(
            wt.join(".wicked/checks.json"),
            serde_json::json!({ "test": ["sh", "-c", "exit 0"] }).to_string(),
        )
        .unwrap();
        let tmp = wt.join(crate::worktree_guard::ENGINE_SCRATCH_DIR);
        std::fs::create_dir_all(tmp.join(BASE_EXPORT_SUBDIR)).unwrap();
        std::fs::write(tmp.join(BASE_EXPORT_SUBDIR).join("stale.ts"), "old").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::symlink_metadata(tmp.join(BASE_EXPORT_SUBDIR)).is_ok() {
            // Running as root: permissions do not bind, nothing to prove here.
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        // A boundary injected, so the refusal is reached whatever sandbox tool the host has.
        let report = run_with_sandbox(
            &wt,
            WorkerSandbox {
                wrapper: vec!["true".to_string()],
                level: SandboxLevel::Sandboxed,
                downgrade_reason: None,
            },
        );
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!report.passed, "{report:?}");
        assert!(
            report
                .detect_error
                .as_deref()
                .is_some_and(|e| e.contains("cannot be inspected")),
            "{report:?}"
        );
        assert!(report.checks.is_empty(), "no check ran: {report:?}");
        let _ = remove_check_scratch_in(&check_cache_root().unwrap(), &wt, None);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (codex on #1050) The orphan mark is checked again UNDER the lease: a reap that marks the
    /// leaf between a floor's first look and its lease never hands that floor the orphan — it is
    /// refused while another floor holds the orphan, and gets a fresh leaf once it is free.
    #[test]
    fn an_orphan_marked_before_the_lease_is_never_reused() {
        let base = scratch("cache-orphan-race");
        let leaf = base.join("cache").join("repo-x").join("run-x");
        std::fs::create_dir_all(leaf.join("cargo-target")).unwrap();
        std::fs::write(leaf.join("cargo-target/old-binary"), "old").unwrap();
        let holder = lock_leaf(&leaf, false).unwrap().unwrap();
        std::fs::write(orphan_path(&leaf), b"").unwrap();
        let err = take_orphan_free_lease(&leaf).expect_err("held orphan: refused");
        assert!(err.to_string().contains("orphaned"), "{err}");
        assert!(leaf.join("cargo-target/old-binary").is_file());
        drop(holder);
        let lease = take_orphan_free_lease(&leaf).expect("a free orphan is retired, then leased");
        assert!(
            !leaf.join("cargo-target/old-binary").exists(),
            "never reused"
        );
        assert!(!orphan_path(&leaf).exists());
        drop(lease);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (core#1050) Two runs whose ids sanitize to ONE worktree path never share a check-scratch
    /// leaf: the first run's leaf, held by its floor when its worktree is removed, is marked
    /// orphaned rather than left reusable, the second run starts on its own empty leaf, and the
    /// orphan goes when the first floor ends (or, failing that, at the next cap pass).
    #[test]
    fn colliding_run_ids_never_share_a_check_scratch() {
        let base = scratch("cache-collide");
        let cache = base.join("cache");
        let wt = base.join("repo").join("wicked-worktrees").join("a-b_c_a0");
        std::fs::create_dir_all(&wt).unwrap();
        let (run_a, run_b) = ("a-b:c:a0", "a:b-c:a0");
        let leaf_a = scratch_dir_for(&cache, &wt, Some(run_a));
        let leaf_b = scratch_dir_for(&cache, &wt, Some(run_b));
        assert_ne!(leaf_a, leaf_b, "one tree, two runs, two leaves");
        assert_eq!(leaf_a.parent(), leaf_b.parent(), "one repository group");
        // Run A's floor is building when A is cancelled and its worktree removed.
        let floor_a = CheckScratch::prepare_in(&cache, &wt, Some(run_a), u64::MAX).unwrap();
        std::fs::write(leaf_a.join("cargo-target/head/a-binary"), "A").unwrap();
        assert!(
            !remove_check_scratch_in(&cache, &wt, Some(run_a)),
            "held: not removed"
        );
        assert!(orphan_path(&leaf_a).exists(), "marked orphaned instead");
        // Run B, a distinct run on the same path, never sees A's output.
        let floor_b = CheckScratch::prepare_in(&cache, &wt, Some(run_b), u64::MAX).unwrap();
        assert_eq!(floor_b.root(), leaf_b);
        assert!(!leaf_b.join("cargo-target/head/a-binary").exists());
        // A's floor ends: its orphaned leaf goes with it.
        drop(floor_a);
        assert!(!leaf_a.exists() && !orphan_path(&leaf_a).exists());
        // An orphan no floor could retire is removed by the next cap pass, whatever the cap.
        std::fs::create_dir_all(leaf_a.join("home")).unwrap();
        std::fs::write(orphan_path(&leaf_a), b"").unwrap();
        let (removed, _) = enforce_check_cache_cap(&cache, &leaf_b, CacheCaps::uniform(u64::MAX));
        assert_eq!(removed, vec![leaf_a.clone()]);
        assert!(!leaf_a.exists() && leaf_b.is_dir());
        drop(floor_b);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (codex on #1050) The run's terminal reap keeps the leaf's lock file: a floor of a resumed
    /// run that already opened it and then takes its lease holds a lock the next eviction sees,
    /// so the eviction never removes the scratch that floor is building into.
    #[test]
    fn a_reap_overlapping_a_floors_lease_never_lets_the_cap_take_its_scratch() {
        let base = scratch("cache-reap-race");
        let cache = base.join("cache");
        let wts = base.join("repo").join("wicked-worktrees");
        let (wt, other) = (wts.join("run-a"), wts.join("run-b"));
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        drop(CheckScratch::prepare_in(&cache, &wt, None, u64::MAX).unwrap());
        let leaf = scratch_dir_for(&cache, &wt, None);
        // The resumed run's floor has the lock file open, about to take its lease.
        let waiting = std::fs::OpenOptions::new()
            .write(true)
            .open(lease_path(&leaf))
            .unwrap();
        assert!(
            remove_check_scratch_in(&cache, &wt, None),
            "the terminal reap"
        );
        assert!(!leaf.exists());
        waiting.lock_shared().unwrap();
        std::fs::create_dir_all(leaf.join("cargo-target").join("head")).unwrap();
        std::fs::write(leaf.join("cargo-target/head/blob"), vec![0u8; 64]).unwrap();
        let own = scratch_dir_for(&cache, &other, None);
        let (removed, _) = enforce_check_cache_cap(&cache, &own, CacheCaps::uniform(0));
        assert!(removed.is_empty(), "{removed:?}");
        assert!(
            leaf.join("cargo-target/head/blob").is_file(),
            "the leased scratch survives"
        );
        drop(waiting);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (codex on #1050) The base export stays in the worktree's engine scratch, so a base check
    /// that calls git still finds the enclosing repository (as from `tmp/wicked-checks/base`).
    /// From the check cache it would find none, fail before printing its failure ids, and turn
    /// the head's pre-existing failure into a "regression".
    #[cfg(unix)]
    #[test]
    fn a_git_dependent_base_check_still_finds_its_repository() {
        let repo = scratch("base-git");
        std::fs::create_dir_all(repo.join(".wicked")).unwrap();
        std::fs::write(repo.join(".gitignore"), "tmp/\n").unwrap();
        let script = "git rev-parse --is-inside-work-tree >/dev/null 2>&1 || { echo NOGIT; \
                      exit 2; }; echo 'test t::shared ... FAILED'; exit 101";
        std::fs::write(
            repo.join(".wicked/checks.json"),
            serde_json::json!({ "test": ["sh", "-c", script] }).to_string(),
        )
        .unwrap();
        let base = git_repo_with_commit(&repo);
        let ctx = FloorContext {
            stage: FloorStage::Verify,
            base_head: Some(base),
            git_dir: Some(repo.join(".git")),
            ..FloorContext::default()
        };
        let report = run_floor(&repo, &ctx);
        if report.sandbox_error.is_some() {
            eprintln!("repo_checks: no sandbox tool here — the baseline diff cannot run");
            return;
        }
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "test")
            .expect("test ran");
        let b = c
            .base
            .as_ref()
            .and_then(|b| b.run.as_ref())
            .expect("the base ran");
        assert!(!b.stdout_tail.contains("NOGIT"), "{b:?}");
        assert_eq!(b.failure_ids, s(&["test t::shared"]), "{b:?}");
        // DES-FLOOR-V2: an `sh` runner has no precise deselect, so the shared red escalates.
        assert_eq!(
            c.classification.as_deref(),
            Some(BASE_RED_UNSEPARATED),
            "{c:#?}"
        );
        assert!(!report.passed, "{report:?}");
        assert_no_worktree_scratch(&repo);
        assert!(!repo.join("tmp").join(BASE_EXPORT_SUBDIR).exists());
        assert!(remove_check_scratch_in(
            &check_cache_root().unwrap(),
            &repo,
            None
        ));
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// (codex on #1050) A base export an older engine left at `tmp/wicked-checks/base` (it died
    /// mid-baseline) is removed before HEAD checks, as that engine itself did — and only that: the
    /// rest of the legacy dir stays, and a linked `wicked-checks` is never followed.
    #[test]
    fn a_legacy_base_export_is_removed_before_head_checks() {
        let base = scratch("legacy-base");
        let wt = base.join("wt");
        let legacy = wt.join("tmp").join("wicked-checks");
        std::fs::create_dir_all(legacy.join("base").join("src")).unwrap();
        std::fs::write(legacy.join("base/src/old.test.ts"), "fails").unwrap();
        std::fs::create_dir_all(legacy.join("home")).unwrap();
        std::fs::write(legacy.join("home/keep"), "x").unwrap();
        let scratch_dir = CheckScratch::prepare(&wt).unwrap();
        let held = HeldDir::open(&wt).unwrap();
        remove_base_export(&held).unwrap();
        assert!(!legacy.join("base").exists(), "the legacy export is gone");
        assert!(
            legacy.join("home/keep").is_file(),
            "the rest of the legacy dir is not touched"
        );
        // A linked `wicked-checks` is not followed.
        #[cfg(unix)]
        {
            let outside = base.join("outside");
            std::fs::create_dir_all(outside.join("base")).unwrap();
            std::fs::remove_dir_all(&legacy).unwrap();
            std::os::unix::fs::symlink(&outside, &legacy).unwrap();
            remove_base_export(&held).unwrap();
            assert!(
                outside.join("base").is_dir(),
                "nothing removed through the link"
            );
        }
        drop(scratch_dir);
        assert!(remove_check_scratch_in(
            &check_cache_root().unwrap(),
            &wt,
            None
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Two consecutive floors over one run's tree reuse its leaf (warm, as when the scratch lived
    /// in the worktree); another run of the SAME repository gets its own leaf and never sees
    /// the first run's build output (a shared target dir hands one run another's binaries by
    /// mtime — a false pass); neither floor creates `tmp/wicked-checks` in its tree; the reap
    /// removes the leaf and leaves the tree's files alone.
    #[test]
    fn consecutive_floors_on_one_run_reuse_its_scratch_and_another_run_never_sees_it() {
        let base = scratch("cache-reuse");
        let wts = base.join("repo").join("wicked-worktrees");
        let probe = "if test -f \"$CARGO_TARGET_DIR/warm\"; then echo SCRATCH=WARM; else echo \
                     SCRATCH=COLD; fi; touch \"$CARGO_TARGET_DIR/warm\"";
        let tree = |name: &str| {
            let wt = wts.join(name);
            std::fs::create_dir_all(wt.join(".wicked")).unwrap();
            std::fs::write(
                wt.join(".wicked/checks.json"),
                serde_json::json!({ "test": ["sh", "-c", probe] }).to_string(),
            )
            .unwrap();
            wt
        };
        let (a, b) = (tree("run-a"), tree("run-b"));
        let said = |wt: &Path| {
            let r = run_floor(wt, &FloorContext::default());
            assert_no_worktree_scratch(wt);
            if r.sandbox_error.is_some() {
                return None;
            }
            let c = r
                .checks
                .iter()
                .find(|c| c.name == "test")
                .expect("test ran");
            assert!(c.passed(), "{c:?}");
            Some(c.stdout_tail.contains("SCRATCH=WARM"))
        };
        let Some(first) = said(&a) else {
            eprintln!("repo_checks: no sandbox tool here — the reuse test cannot run the floor");
            return;
        };
        assert!(!first, "a run's first floor starts cold");
        assert_eq!(
            said(&a),
            Some(true),
            "the run's next floor reuses its scratch"
        );
        assert_eq!(
            said(&b),
            Some(false),
            "another run never sees the first run's output"
        );
        let cache = check_cache_root().unwrap();
        assert!(remove_check_scratch_in(&cache, &a, None));
        assert!(!leaf_of(&a).exists());
        assert!(
            a.join(".wicked/checks.json").is_file(),
            "the tree's files are untouched"
        );
        assert!(remove_check_scratch_in(&cache, &b, None));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `cargo test --locked` when the repo ships a lockfile (the check must not rewrite it);
    /// plain `cargo test` when it does not — the engine-written `Cargo.lock` is then removed
    /// after the checks (see `a_failing_cargo_test_is_captured…`).
    #[test]
    fn cargo_runs_locked_when_a_lockfile_ships() {
        let wt = scratch("cargo-locked");
        std::fs::write(wt.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::write(wt.join("Cargo.lock"), "# lock\n").unwrap();
        let checks = detect(&wt).unwrap();
        assert_eq!(checks[0].name, "cargo-fmt-check");
        assert_eq!(
            checks[1].argv,
            s(&["cargo", "test", "--locked", "--no-fail-fast"])
        );
        assert!(engine_generated_candidates(&wt, &checks).is_empty());
        std::fs::remove_file(wt.join("Cargo.lock")).unwrap();
        let checks = detect(&wt).unwrap();
        assert_eq!(checks[1].argv, s(&["cargo", "test", "--no-fail-fast"]));
        assert_eq!(
            engine_generated_candidates(&wt, &checks),
            vec!["Cargo.lock".to_string()]
        );
    }

    #[test]
    fn stops_at_the_first_failure_and_lists_what_it_skipped() {
        let wt = scratch("stop");
        let sandbox = sandbox_for(&wt);
        let scratch = CheckScratch::prepare(&wt).unwrap();
        // A check that cannot even spawn fails the floor and skips everything after it.
        let checks = vec![
            RepoCheck {
                name: "typecheck".into(),
                argv: s(&["definitely-not-a-binary-wicked-xyz", "run"]),
                source: "fixture".into(),
                timeout_s: None,
                dir: String::new(),
            },
            RepoCheck {
                name: "test".into(),
                argv: s(&["definitely-not-a-binary-wicked-xyz", "run"]),
                source: "fixture".into(),
                timeout_s: None,
                dir: String::new(),
            },
        ];
        let mut runs = Vec::new();
        let mut skipped = Vec::new();
        let mut failed = false;
        for c in &checks {
            if failed {
                skipped.push(c.name.clone());
                continue;
            }
            let r = run_one(&wt, c, &sandbox, &scratch, Tree::Head);
            failed = !r.passed();
            runs.push(r);
        }
        assert_eq!(runs.len(), 1);
        assert!(runs[0]
            .spawn_error
            .as_deref()
            .is_some_and(|e| e.contains("not on PATH")));
        assert_eq!(skipped, vec!["test"]);
        let report = RepoChecksReport {
            detected: checks,
            checks: runs,
            skipped,
            passed: false,
            detect_error: None,
            sandbox_level: sandbox.level.as_wire().to_string(),
            sandbox_note: None,
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
        };
        let denial = report.denial_reason();
        assert!(
            denial.contains("could not run") && denial.contains("not run after the failure: test")
        );
    }

    #[test]
    fn a_passing_check_passes_and_a_tail_is_bounded() {
        let wt = scratch("pass");
        let echo = RepoCheck {
            name: "test".into(),
            argv: s(&[
                "sh",
                "-c",
                "yes long-line-of-output | head -c 100000; exit 0",
            ]),
            source: "fixture".into(),
            timeout_s: None,
            dir: String::new(),
        };
        if crate::validator::find_on_path("sh").is_none() {
            eprintln!("repo_checks: sh not on PATH — skipping the bounded-tail check");
            return;
        }
        let sandbox = sandbox_for(&wt);
        let scratch = CheckScratch::prepare(&wt).unwrap();
        let r = run_one(&wt, &echo, &sandbox, &scratch, Tree::Head);
        assert!(r.passed(), "{r:?}");
        assert!(
            r.stdout_tail.len() <= TAIL_BYTES,
            "the tail is bounded to {TAIL_BYTES} bytes, got {}",
            r.stdout_tail.len()
        );
        assert!(r.stdout_tail.contains("long-line-of-output"));
    }

    pub(crate) fn git(repo: &Path, args: &[&str]) -> String {
        // spawn-audit: test-only — builds the baseline-diff fixture repository.
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// core#417 (operator ruling (a)): a guarded verify floor runs on a clean checkout of the
    /// tree the worktree guard hashed. An IGNORED file the seat planted in the live worktree is
    /// absent there and cannot change the verdict: the same check FAILS on the live worktree (it
    /// sees the plant) and PASSES on the guarded checkout. Tracked and untracked-not-ignored
    /// content is there, with git metadata, and the checkout is removed when dropped.
    #[test]
    fn a_guarded_verify_floor_runs_on_the_guards_tree_where_an_ignored_plant_cannot_reach() {
        let _env = crate::test_env::ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let wt = scratch("guarded-checkout");
        std::fs::write(wt.join("app.txt"), "v1\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("app.txt", wt.join("link")).unwrap();
        std::fs::write(wt.join(".gitignore"), ".env.local\n").unwrap();
        std::fs::create_dir_all(wt.join(".wicked")).unwrap();
        std::fs::write(
            wt.join(CONFIG_PATH),
            r#"{"typecheck": false, "lint": false, "test": ["sh", "-c", "test ! -e .env.local && test -e new.txt"]}"#,
        )
        .unwrap();
        git_repo_with_commit(&wt);
        // The creator's uncommitted work, and the plant: ignored, so the guard's hash skips it.
        std::fs::write(wt.join("new.txt"), "new\n").unwrap();
        std::fs::write(wt.join(".env.local"), "STEER=1\n").unwrap();
        let git_dir = PathBuf::from(git(&wt, &["rev-parse", "--absolute-git-dir"]));
        let snap = crate::worktree_guard::snapshot_through(&wt, &git_dir).expect("snapshot");

        // No repository hook may run while the engine builds the checkout (codex r2).
        let hooks = wt.join("repo-hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let marker = wt.join("hook-ran");
        for h in ["post-checkout", "post-index-change"] {
            let hook = hooks.join(h);
            std::fs::write(
                &hook,
                format!("#!/bin/sh\necho {h} >> '{}'\n", marker.display()),
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        git(&wt, &["config", "core.hooksPath", &hooks.to_string_lossy()]);
        let checkout =
            checkout_guarded_tree(&wt, &git_dir, &snap.tree, "t-417").expect("guarded checkout");
        assert!(
            !marker.exists(),
            "a repository hook ran while the checkout was built"
        );
        let dir = checkout.dir.clone();
        assert!(dir.join("app.txt").is_file() && dir.join("new.txt").is_file());
        assert!(
            !dir.join(".env.local").exists(),
            "the ignored plant never reaches the floor"
        );
        assert!(dir.join(".git").exists(), "the checks keep git metadata");

        let live = run_floor_rerun(&wt, &FloorContext::default(), None);
        let guarded = run_floor_rerun(&dir, &FloorContext::default(), None);
        if !live.checks.is_empty() {
            // A host with an OS write boundary ran the checks: the verdicts must differ.
            assert!(!live.passed, "the live worktree sees the plant: {live:?}");
            assert!(guarded.passed, "the guarded tree does not: {guarded:?}");
        }
        // A check that rewrites tracked content in the checkout cannot pass on it (codex r2):
        // the floor certifies only the guarded tree, untouched. An untracked artifact is fine.
        let mut ok = guarded.clone();
        ok.passed = true;
        std::fs::write(dir.join("coverage.out"), "artifact\n").unwrap();
        checkout.hold_to(&snap.tree, &mut ok);
        assert!(
            ok.passed,
            "an untracked artifact is not a change to what ships: {ok:?}"
        );
        // (codex r3) A mode flip or a retargeted symlink is a change to what ships, too.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut flipped = ok.clone();
            let app = dir.join("app.txt");
            std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o755)).unwrap();
            checkout.hold_to(&snap.tree, &mut flipped);
            assert!(!flipped.passed, "a chmod +x is a change: {flipped:?}");
            std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o644)).unwrap();
            let mut relinked = ok.clone();
            let link = dir.join("link");
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink("new.txt", &link).unwrap();
            checkout.hold_to(&snap.tree, &mut relinked);
            assert!(
                !relinked.passed,
                "a retargeted symlink is a change: {relinked:?}"
            );
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink("app.txt", &link).unwrap();
            // (sweep review on #1066) A directory of the tree swapped for a link to an outside
            // copy with the SAME content is a change too: every entry is re-read through the
            // checkout held before the checks ran, never through the link.
            let mut swapped = ok.clone();
            let copy = scratch("guarded-checkout-outside-copy");
            std::fs::copy(dir.join(CONFIG_PATH), copy.join("checks.json")).unwrap();
            std::fs::rename(dir.join(".wicked"), dir.join(".wicked-moved")).unwrap();
            std::os::unix::fs::symlink(&copy, dir.join(".wicked")).unwrap();
            checkout.hold_to(&snap.tree, &mut swapped);
            assert!(
                !swapped.passed,
                "a linked-away directory is a change: {swapped:?}"
            );
            assert!(
                swapped
                    .detect_error
                    .as_deref()
                    .is_some_and(|e| e.contains(CONFIG_PATH)),
                "{swapped:?}"
            );
            std::fs::remove_file(dir.join(".wicked")).unwrap();
            std::fs::rename(dir.join(".wicked-moved"), dir.join(".wicked")).unwrap();
            let _ = std::fs::remove_dir_all(&copy);
        }
        std::fs::write(dir.join("app.txt"), "rewritten by a check\n").unwrap();
        checkout.hold_to(&snap.tree, &mut ok);
        assert!(!ok.passed, "{ok:?}");
        assert!(
            ok.detect_error
                .as_deref()
                .is_some_and(|e| e.contains("app.txt")),
            "{ok:?}"
        );
        // The guarded checkout's check scratch is its own leaf, never the live tree's
        // (core#417), and it goes with the checkout; the live tree's stays for the run.
        let (live_leaf, guarded_leaf) = (leaf_of(&wt), leaf_of(&dir));
        assert_ne!(live_leaf, guarded_leaf);
        assert!(guarded_leaf.is_dir() && live_leaf.is_dir());
        assert_no_worktree_scratch(&dir);
        drop(checkout);
        assert!(!dir.exists(), "the checkout is removed when dropped");
        assert!(
            !guarded_leaf.exists(),
            "the checkout's check scratch is removed with it: {}",
            guarded_leaf.display()
        );
        assert!(live_leaf.is_dir());
        assert!(remove_check_scratch_in(
            &check_cache_root().unwrap(),
            &wt,
            None
        ));
        let listed = git(&wt, &["worktree", "list", "--porcelain"]);
        assert!(!listed.contains("wicked-core-floor"), "{listed}");
        let _ = std::fs::remove_dir_all(&wt);
    }

    pub(crate) fn git_repo_with_commit(repo: &Path) -> String {
        git(repo, &["init", "-q"]);
        git(repo, &["config", "user.email", "t@example.invalid"]);
        git(repo, &["config", "user.name", "t"]);
        git(repo, &["config", "commit.gpgsign", "false"]);
        git(repo, &["config", "core.autocrlf", "false"]);
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-qm", "base"]);
        git(repo, &["rev-parse", "HEAD"])
    }

    pub(crate) fn names(checks: &[RepoCheck]) -> Vec<&str> {
        checks.iter().map(|c| c.name.as_str()).collect()
    }

    /// core#469: a check that hits its bound is `timed_out`, never `failed` — the classification
    /// rides the run, the report and the denial source, and the denial says what the gate can do.
    #[test]
    fn a_check_that_hits_its_bound_is_timed_out_not_failed() {
        if crate::validator::find_on_path("sh").is_none() {
            eprintln!("repo_checks: sh not on PATH — skipping the timeout classification test");
            return;
        }
        let wt = scratch("timeout");
        let sandbox = sandbox_for(&wt);
        let scratch = CheckScratch::prepare(&wt).unwrap();
        let slow = RepoCheck {
            name: "test".into(),
            argv: s(&["sh", "-c", "sleep 30"]),
            source: "fixture".into(),
            timeout_s: Some(1),
            dir: String::new(),
        };
        let started = Instant::now();
        let r = run_one(&wt, &slow, &sandbox, &scratch, Tree::Head);
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "killed at the bound, not after the sleep: {:?}",
            started.elapsed()
        );
        assert!(r.timed_out && !r.passed() && r.denies(), "{r:?}");
        assert_eq!(r.outcome(), "timed_out");
        assert!(
            (1..=3).contains(&r.bound_s),
            "a 1s base × a host-load factor capped at 3: {} ({:?})",
            r.bound_s,
            r.bound_note
        );
        assert!(r.summary().contains("TIMED OUT"), "{}", r.summary());
        let report = RepoChecksReport {
            detected: vec![slow.clone()],
            checks: vec![r],
            skipped: vec!["cargo-test".into()],
            passed: false,
            detect_error: None,
            sandbox_level: sandbox.level.as_wire().to_string(),
            sandbox_note: None,
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
        };
        assert!(report.timed_out());
        assert_eq!(report.outcome(), "timed_out");
        assert_eq!(report.denial_source(), DENIAL_SOURCE_TIMEOUT);
        let denial = report.denial_reason();
        assert!(
            denial.contains("did not FINISH")
                && denial.contains("unverified by it, not refuted")
                && denial.contains("timeout_s")
                && denial.contains("test_targeted")
                && denial.contains("not run after the failure: cargo-test"),
            "{denial}"
        );
        assert!(
            !denial.contains("repo checks floor failed:"),
            "a timeout is never worded as a failure: {denial}"
        );
    }

    /// core#469: `.wicked/checks.json` speaks over the manifests — a targeted test command is
    /// preferred at the creator stage (and at verify unless `full: true`), `{files}` / `{base}`
    /// are substituted from the run's base, `timeout_s` rides every non-install check, `false`
    /// disables a check, and a malformed file fails detection CLOSED.
    #[test]
    fn per_repo_config_prefers_targeted_tests_and_fails_closed_on_a_bad_file() {
        let wt = scratch("config");
        std::fs::write(
            wt.join("package.json"),
            r#"{"scripts":{"typecheck":"tsc --noEmit","lint":"eslint .","test":"vitest run"}}"#,
        )
        .unwrap();
        std::fs::write(wt.join(".gitignore"), "node_modules/\ntmp/\n").unwrap();
        std::fs::create_dir_all(wt.join("node_modules")).unwrap();
        std::fs::create_dir_all(wt.join(".wicked")).unwrap();
        std::fs::write(
            wt.join(CONFIG_PATH),
            r#"{"lint":false,"test_targeted":["npx","vitest","run","--changed","{base}","{files}"],"timeout_s":90}"#,
        )
        .unwrap();
        let base = git_repo_with_commit(&wt);
        // No base known ⇒ an anchored targeted command cannot run: the full set stands.
        let full = detect(&wt).unwrap();
        assert_eq!(
            names(&full),
            vec!["typecheck", "test"],
            "lint disabled by config"
        );
        assert!(
            full.iter().all(|c| c.timeout_s == Some(90)),
            "timeout_s rides every check: {full:?}"
        );
        // The creator, with a known base ⇒ targeted, `{base}` and `{files}` substituted (the
        // untracked file the change added is a touched path; the engine scratch never is).
        std::fs::write(wt.join("src.ts"), "changed\n").unwrap();
        std::fs::create_dir_all(wt.join("tmp/worker")).unwrap();
        std::fs::write(wt.join("tmp/worker/x"), "scratch").unwrap();
        let ctx = FloorContext {
            stage: FloorStage::Creator,
            base_head: Some(base.clone()),
            git_dir: Some(wt.join(".git")),
            ..FloorContext::default()
        };
        let creator = detect_with(&wt, &ctx).unwrap();
        assert_eq!(names(&creator), vec!["typecheck", "test_targeted"]);
        let targeted = &creator[1];
        assert_eq!(
            targeted.argv,
            vec!["npx", "vitest", "run", "--changed", base.as_str(), "src.ts"]
        );
        assert!(
            targeted.source.contains("test_targeted"),
            "{}",
            targeted.source
        );
        assert_eq!(targeted.timeout_s, Some(90));
        // Verify prefers the targeted set too — until the repo says `full: true`.
        let verify_ctx = FloorContext {
            stage: FloorStage::Verify,
            ..ctx.clone()
        };
        assert_eq!(
            names(&detect_with(&wt, &verify_ctx).unwrap()),
            vec!["typecheck", "test_targeted"]
        );
        std::fs::write(
            wt.join(CONFIG_PATH),
            r#"{"lint":false,"test_targeted":"npx vitest run --changed {base}","timeout_s":90,"full":true}"#,
        )
        .unwrap();
        let verify_full = detect_with(&wt, &verify_ctx).unwrap();
        assert_eq!(names(&verify_full), vec!["typecheck", "test"]);
        assert_eq!(verify_full[1].argv, s(&["npm", "run", "test"]));
        let creator_again = detect_with(&wt, &ctx).unwrap();
        assert_eq!(
            names(&creator_again),
            vec!["typecheck", "test_targeted"],
            "`full` speaks at verify only"
        );
        assert_eq!(
            creator_again[1].argv,
            vec!["npx", "vitest", "run", "--changed", base.as_str()],
            "a string command is whitespace-split, no shell"
        );
        // A configured `test` replaces the auto-detected one; a configured typecheck is added
        // even when package.json names none.
        std::fs::write(
            wt.join("package.json"),
            r#"{"scripts":{"test":"vitest run"}}"#,
        )
        .unwrap();
        std::fs::write(
            wt.join(CONFIG_PATH),
            r#"{"typecheck":["npx","tsc","-p","."],"test":"npm run test:ci"}"#,
        )
        .unwrap();
        let cfgd = detect(&wt).unwrap();
        assert_eq!(names(&cfgd), vec!["typecheck", "test"]);
        assert_eq!(cfgd[0].argv, s(&["npx", "tsc", "-p", "."]));
        assert_eq!(cfgd[1].argv, s(&["npm", "run", "test:ci"]));
        assert!(cfgd.iter().all(|c| c.timeout_s.is_none()));
        // Fail-closed: an unknown key, a bad value, an out-of-range bound.
        for (bad, why) in [
            (r#"{"tests":"x"}"#, "unknown field"),
            (r#"{"test":true}"#, "not a command"),
            (r#"{"test":[]}"#, "empty argv"),
            (r#"{"timeout_s":0}"#, "timeout_s"),
            ("not json", "not valid"),
        ] {
            std::fs::write(wt.join(CONFIG_PATH), bad).unwrap();
            let err = detect(&wt).expect_err(bad);
            assert!(err.contains(why), "{bad}: {err}");
        }
    }

    /// The host-load factor is bounded on both sides and never governs the install step.
    #[test]
    fn the_load_factor_is_bounded() {
        assert_eq!(load_factor(None, 8), 1.0, "unknown load ⇒ the base bound");
        assert_eq!(
            load_factor(Some(0.5), 8),
            1.0,
            "an idle host keeps the base bound"
        );
        assert_eq!(load_factor(Some(16.0), 8), 2.0);
        assert_eq!(load_factor(Some(1000.0), 8), LOAD_FACTOR_CAP);
        assert_eq!(load_factor(Some(f64::NAN), 8), 1.0);
        assert_eq!(load_factor(Some(10.0), 0), 1.0);
        let check = RepoCheck {
            name: "test".into(),
            argv: s(&["true"]),
            source: "fixture".into(),
            timeout_s: Some(100),
            dir: String::new(),
        };
        let (bound, note) = effective_bound(&check);
        assert!(
            bound >= Duration::from_secs(100) && bound <= Duration::from_secs(300),
            "{bound:?} ({note:?})"
        );
        let install = RepoCheck {
            name: "install".into(),
            argv: s(&["true"]),
            source: "fixture".into(),
            timeout_s: Some(100),
            dir: String::new(),
        };
        assert_eq!(
            install.base_timeout(),
            INSTALL_TIMEOUT,
            "timeout_s never governs the install step"
        );
        let default = RepoCheck {
            name: "lint".into(),
            argv: s(&["true"]),
            source: "fixture".into(),
            timeout_s: None,
            dir: String::new(),
        };
        assert_eq!(default.base_timeout(), CHECK_TIMEOUT);
    }

    /// The streaming failure-identifier scanner knows the runners the floor meets, and drops the
    /// line/column positions so an unchanged failure keeps its identity across a shifted file.
    #[test]
    fn failure_identifiers_are_scanned_off_runner_output() {
        let mut f = None;
        let id = |line: &str, f: &mut Option<String>| failure_id_of_line(line, f);
        assert_eq!(
            id("test actor::tests::a_run ... FAILED", &mut f).as_deref(),
            Some("test actor::tests::a_run")
        );
        assert_eq!(id("test actor::tests::ok ... ok", &mut f), None);
        assert_eq!(
            id("test result: FAILED. 908 passed; 27 failed", &mut f),
            None
        );
        assert_eq!(
            id(" FAIL  tests/x.test.ts > suite > name", &mut f).as_deref(),
            Some("FAIL tests/x.test.ts > suite > name")
        );
        assert_eq!(
            id("● suite › name", &mut f).as_deref(),
            Some("● suite › name")
        );
        assert_eq!(
            id("src/a.ts(1203,20): error TS2375: Argument of type", &mut f).as_deref(),
            Some("src/a.ts: error TS2375: Argument of type")
        );
        assert_eq!(
            id("FAILED tests/x.py::test_y - AssertionError", &mut f).as_deref(),
            Some("FAILED tests/x.py::test_y")
        );
        assert_eq!(
            id("--- FAIL: TestX (0.00s)", &mut f).as_deref(),
            Some("FAIL TestX")
        );
        // eslint (stylish): the file header sets the context, the problem lines carry it.
        assert_eq!(id("/w/src/components/CenterDashboard.tsx", &mut f), None);
        assert_eq!(f.as_deref(), Some("/w/src/components/CenterDashboard.tsx"));
        assert_eq!(
            id(
                "  1203:7  error  'NO_UNITS' is assigned a value but never used  no-unused-vars",
                &mut f
            )
            .as_deref(),
            Some(
                "/w/src/components/CenterDashboard.tsx: error 'NO_UNITS' is assigned a value \
                 but never used no-unused-vars"
            )
        );
        assert_eq!(id("✖ 1 problem (1 error, 0 warnings)", &mut f), None);
        assert_eq!(id("", &mut f), None);
        // node:test SPEC reporter (Node ≥ 23) — `✖ name (Nms)` with a timing suffix is a
        // failure; the `✖ failing tests:` summary and eslint `✖ N problems` lines are not (#538).
        assert_eq!(
            id("✖ should pass the assertion (3ms)", &mut f).as_deref(),
            Some("✖ should pass the assertion"),
            "SPEC failure line with ms timing"
        );
        assert_eq!(
            id("  ✖ nested > suite > test name (1.234s)", &mut f).as_deref(),
            Some("✖ nested > suite > test name"),
            "indented SPEC line (nested suite) with s timing"
        );
        assert_eq!(
            id("✖ failing tests:", &mut f),
            None,
            "✖ failing tests: summary has no timing suffix"
        );
        // node:test TAP output (#538): `not ok N - name` is a failure; `# Subtest:` is not.
        assert_eq!(
            id("not ok 1 - passes the assertion", &mut f).as_deref(),
            Some("not ok passes the assertion")
        );
        assert_eq!(
            id("not ok 42 - deeply/nested > suite > test name", &mut f).as_deref(),
            Some("not ok deeply/nested > suite > test name")
        );
        assert_eq!(
            id("not ok 1", &mut f).as_deref(),
            Some("not ok 1"),
            "no separator: the whole rest is the id"
        );
        assert_eq!(
            id("# Subtest: some suite", &mut f),
            None,
            "# Subtest is not failure-only"
        );
        assert_eq!(
            id("ok 1 - passes", &mut f),
            None,
            "ok lines are not failures"
        );
        assert_eq!(id("TAP version 14", &mut f), None);
    }

    /// core#467: the claim scanner is conservative — a claim phrase AND a check-shaped word in
    /// one sentence; a bug described as "pre-existing" in a fix summary is not a claim.
    #[test]
    fn a_pre_existing_claim_is_detected_conservatively() {
        assert_eq!(
            detect_claim(
                "Note: the typecheck error in CenterDashboard.tsx is pre-existing on main."
            )
            .as_deref(),
            Some("pre-existing")
        );
        assert_eq!(
            detect_claim("All green.\nThe lint failure was already failing before my change")
                .as_deref(),
            Some("already failing")
        );
        assert_eq!(
            detect_claim("Fixed a pre-existing bug in the routing table"),
            None,
            "no check-shaped word in the sentence"
        );
        assert_eq!(detect_claim("typecheck 0, lint 0, tests 3226 passed"), None);
        assert_eq!(
            detect_claim("src/app.ts is fine. The tsc error in a.b.ts was already broken"),
            Some("already broken".to_string()),
            "a dotted file name is not a sentence boundary"
        );
    }

    /// (F-RC2-009) BASELINE-DIFF on the medium every `cargo test` host has. A base whose `shared`
    /// and `other` tests fail: (1) a head that ALSO breaks `stable` is a REGRESSION — denied,
    /// naming the head-only failure and the shared ones, and the creator's "pre-existing" claim is
    /// REJECTED; (2) a head that fails exactly what the base fails is a `floor_env_mismatch` —
    /// recorded, never denying, the floor PASSES and the base run is paid for once (cached);
    /// (3) a head that fixes `other` and breaks nothing is `pre_existing_in_sandbox` — passes;
    /// (4) `baseline_diff: false` restores the plain denial, with the opt-out on the record.
    /// core#551 (review): a configured Node formatter is a Node check like any other, so a
    /// formatter-only `checks.json` still schedules `install` first — it must not run `npx`/`npm`
    /// against an unprovisioned tree.
    #[test]
    fn a_formatter_only_node_config_schedules_install_first() {
        let wt = scratch("fmt-node-install");
        std::fs::write(
            wt.join("package.json"),
            "{\"name\":\"x\",\"version\":\"0.0.0\"}\n",
        )
        .unwrap();
        std::fs::create_dir_all(wt.join(".wicked")).unwrap();
        std::fs::write(
            wt.join(CONFIG_PATH),
            "{\"formatter\": [\"npm\", \"run\", \"format:check\"]}\n",
        )
        .unwrap();
        let names: Vec<String> = detect(&wt).unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            vec!["install".to_string(), "formatter".to_string()],
            "{names:?}"
        );
    }

    /// core#551 (review): on a Node repo with a `test` script AND a configured formatter, the
    /// formatter runs before the tests — a failing test must not skip the "format first" denial.
    #[test]
    fn a_configured_node_formatter_runs_before_the_test_script() {
        let wt = scratch("fmt-node-order");
        std::fs::write(
            wt.join("package.json"),
            "{\"name\":\"x\",\"version\":\"0.0.0\",\"scripts\":{\"test\":\"vitest run\"}}\n",
        )
        .unwrap();
        std::fs::create_dir_all(wt.join(".wicked")).unwrap();
        std::fs::write(
            wt.join(CONFIG_PATH),
            "{\"formatter\": [\"npm\", \"run\", \"format:check\"]}\n",
        )
        .unwrap();
        let names: Vec<String> = detect(&wt).unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            vec![
                "install".to_string(),
                "formatter".to_string(),
                "test".to_string()
            ],
            "{names:?}"
        );
    }

    /// core#551 (review): a formatting diff is deterministic, never environmental, so a formatter
    /// failure the base ALSO has is still a denial — the baseline diff must not excuse it the way
    /// it excuses a pre-existing test failure.
    #[test]
    fn a_formatter_failure_the_base_shares_still_denies() {
        let repo = scratch("fmt-basediff");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"fmt_basediff_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\npath = \"src/lib.rs\"\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(repo.join(".gitignore"), "Cargo.lock\ntmp/\n").unwrap();
        // Unformatted on the BASE and unchanged on the head: the base fails fmt identically.
        std::fs::write(
            repo.join("src/lib.rs"),
            "pub fn add(a: i32,b: i32)->i32{a+b}\n",
        )
        .unwrap();
        let base = git_repo_with_commit(&repo);
        let ctx = FloorContext {
            stage: FloorStage::Creator,
            base_head: Some(base),
            git_dir: Some(repo.join(".git")),
            ..FloorContext::default()
        };
        let report = run_floor(&repo, &ctx);
        if report.sandbox_error.is_some() {
            eprintln!(
                "repo_checks: no sandbox tool here — the floor failed closed instead of running"
            );
            return;
        }
        assert!(
            !report.passed,
            "a shared formatting failure must still deny: {report:?}"
        );
        let fmt = &report.checks[0];
        assert_eq!(fmt.name, "cargo-fmt-check");
        assert!(fmt.exit_code.is_some_and(|e| e != 0), "{fmt:?}");
        assert!(
            fmt.base.is_none(),
            "the formatter is never baseline-compared: {:?}",
            fmt.base
        );
        assert!(fmt.denies(), "{fmt:?}");
        let denial = report.denial_reason();
        assert!(denial.contains("cargo-fmt-check"), "{denial}");
    }

    /// core#482 / F-3R2-023 (DES-L2 2D): `e2e` in `.wicked/checks.json` runs at the VERIFY stage
    /// only, after the test set; never at the creator; `false` and absence mean no `e2e` check;
    /// `timeout_s` rides it; and a base that lacks the key fails the baseline diff CLOSED ("the
    /// change introduced it") — a declared e2e suite is never excused by a base that had none.
    #[cfg(unix)]
    #[test]
    fn e2e_runs_at_verify_only_after_the_test_set_and_a_base_lacking_it_fails_closed() {
        let repo = scratch("e2e");
        std::fs::create_dir_all(repo.join(".wicked")).unwrap();
        std::fs::write(repo.join(".gitignore"), "tmp/\n").unwrap();
        // The base: a `test` only, no `e2e`.
        std::fs::write(
            repo.join(CONFIG_PATH),
            r#"{"test":["true"],"timeout_s":45}"#,
        )
        .unwrap();
        let base = git_repo_with_commit(&repo);
        // The change declares an e2e suite that fails.
        std::fs::write(
            repo.join(CONFIG_PATH),
            r#"{"test":["true"],"e2e":["sh","-c","echo e2e-red >&2; exit 1"],"timeout_s":45}"#,
        )
        .unwrap();
        let creator = detect_with(
            &repo,
            &FloorContext {
                stage: FloorStage::Creator,
                ..FloorContext::default()
            },
        )
        .unwrap();
        assert_eq!(
            names(&creator),
            vec!["test"],
            "never at the creator: {creator:?}"
        );
        let verify = detect_with(&repo, &FloorContext::default()).unwrap();
        assert_eq!(
            names(&verify),
            vec!["test", "e2e"],
            "after the test set: {verify:?}"
        );
        assert_eq!(verify[1].argv, s(&["sh", "-c", "echo e2e-red >&2; exit 1"]));
        assert_eq!(verify[1].source, format!("{CONFIG_PATH} e2e"));
        assert_eq!(verify[1].timeout_s, Some(45), "timeout_s rides e2e too");
        // `false` ⇒ no e2e; unknown keys still refused.
        std::fs::write(repo.join(CONFIG_PATH), r#"{"test":["true"],"e2e":false}"#).unwrap();
        assert_eq!(
            names(&detect_with(&repo, &FloorContext::default()).unwrap()),
            vec!["test"]
        );
        let bad = detect_with_bad_key(&repo);
        assert!(bad.contains("unknown field"), "{bad}");
        // The floor at verify with the base known: `e2e` fails, the base declares none ⇒ the
        // baseline diff cannot excuse it (fail-closed), the check denies with the base's error.
        std::fs::write(
            repo.join(CONFIG_PATH),
            r#"{"test":["true"],"e2e":["sh","-c","echo e2e-red >&2; exit 1"],"timeout_s":45}"#,
        )
        .unwrap();
        let ctx = FloorContext {
            stage: FloorStage::Verify,
            base_head: Some(base),
            git_dir: Some(repo.join(".git")),
            ..FloorContext::default()
        };
        let report = run_floor(&repo, &ctx);
        if report.sandbox_error.is_some() {
            eprintln!("repo_checks: no sandbox tool here — the e2e floor cannot run");
            return;
        }
        assert!(!report.passed, "{report:?}");
        assert_eq!(names_run(&report), vec!["test", "e2e"]);
        let e2e = &report.checks[1];
        assert_eq!(e2e.outcome(), "failed");
        assert!(e2e.denies(), "{e2e:?}");
        assert!(
            e2e.classification.is_none(),
            "no base verdict ⇒ no classification: {e2e:?}"
        );
        let base_err = e2e
            .base
            .as_ref()
            .and_then(|b| b.error.clone())
            .unwrap_or_default();
        assert!(
            base_err.contains("declares no `e2e` check") && base_err.contains("introduced it"),
            "{base_err}"
        );
    }

    /// core#817: a change confined to a NEW subdirectory package runs that package's checks, in
    /// its directory — not the unrelated root package's; a change touching both runs both; a
    /// workspace root that covers its members keeps the root's checks; a package with no checks
    /// is named on the note.
    #[cfg(unix)]
    #[test]
    fn a_change_in_a_subdirectory_package_runs_that_packages_checks_817() {
        let repo = scratch("subdir-pkg");
        std::fs::write(repo.join(".gitignore"), "tmp/\nnode_modules/\n").unwrap();
        std::fs::write(
            repo.join("package.json"),
            r#"{"name":"petstore","scripts":{"test":"vitest run"}}"#,
        )
        .unwrap();
        std::fs::write(repo.join("index.ts"), "export {}\n").unwrap();
        let base = git_repo_with_commit(&repo);
        std::fs::create_dir_all(repo.join("giphy/src")).unwrap();
        std::fs::write(
            repo.join("giphy/package.json"),
            r#"{"name":"giphy","scripts":{"test":"vitest run","typecheck":"tsc --noEmit"}}"#,
        )
        .unwrap();
        std::fs::write(repo.join("giphy/src/server.ts"), "export {}\n").unwrap();
        let ctx = FloorContext {
            stage: FloorStage::Verify,
            base_head: Some(base.clone()),
            git_dir: Some(repo.join(".git")),
            ..FloorContext::default()
        };
        assert_eq!(
            subdir_packages(&repo, &ctx).unwrap(),
            ChangePackages {
                dirs: vec!["giphy".into()],
                root_touched: false
            }
        );
        let checks = detect_with(&repo, &ctx).unwrap();
        assert!(!checks.is_empty(), "{checks:?}");
        assert!(
            checks
                .iter()
                .all(|c| c.dir == "giphy" && c.source.starts_with("giphy/")),
            "only giphy's checks, in giphy/: {checks:?}"
        );
        assert!(checks.iter().any(|c| c.name == "typecheck"), "{checks:?}");
        // A root-owned path too: both packages' checks.
        std::fs::write(repo.join("index.ts"), "export const x = 1\n").unwrap();
        let checks = detect_with(&repo, &ctx).unwrap();
        assert!(checks.iter().any(|c| c.dir.is_empty()), "{checks:?}");
        assert!(checks.iter().any(|c| c.dir == "giphy"), "{checks:?}");
        // A Node workspace root covers its members: the root's checks only, as before.
        std::fs::write(
            repo.join("package.json"),
            r#"{"name":"petstore","workspaces":["giphy"],"scripts":{"test":"vitest run"}}"#,
        )
        .unwrap();
        assert!(subdir_packages(&repo, &ctx).unwrap().dirs.is_empty());
        assert!(detect_with(&repo, &ctx)
            .unwrap()
            .iter()
            .all(|c| c.dir.is_empty()));
        // ...but only the members its globs name (review: `packages/*` does not cover giphy/).
        std::fs::write(
            repo.join("package.json"),
            r#"{"name":"petstore","workspaces":["packages/*","!packages/x"],"scripts":{"test":"vitest run"}}"#,
        )
        .unwrap();
        assert_eq!(subdir_packages(&repo, &ctx).unwrap().dirs, vec!["giphy"]);
        assert!(workspace_glob_matches("packages/*", "packages/a"));
        assert!(workspace_glob_matches("apps/**", "apps/web/ui"));
        assert!(workspace_glob_matches("libs/ui-*", "libs/ui-kit"));
        assert!(!workspace_glob_matches("packages/*", "packages/a/b"));
        assert!(!workspace_glob_matches("packages/*", "giphy"));
        // No base known: the root, as before.
        assert!(subdir_packages(&repo, &FloorContext::default())
            .unwrap()
            .dirs
            .is_empty());
        // A package with no scripts is named, not silently passed.
        std::fs::write(
            repo.join("package.json"),
            r#"{"name":"petstore","scripts":{"test":"vitest run"}}"#,
        )
        .unwrap();
        std::fs::write(repo.join("giphy/package.json"), r#"{"name":"giphy"}"#).unwrap();
        let checks = detect_with(&repo, &ctx).unwrap();
        let note = package_coverage_note(&repo, &ctx, &checks).expect("the bare package is named");
        assert!(note.contains("touches package giphy/"), "{checks:?} {note}");
        // A package-local targeted command sees the change's files RELATIVE to the package
        // (review r2: the subpackage context kept no base, so `{files}` was never detected).
        std::fs::create_dir_all(repo.join("giphy/.wicked")).unwrap();
        std::fs::write(
            repo.join("giphy").join(CONFIG_PATH),
            r#"{"test_targeted":["sh","-c","echo","{files}"],"baseline_diff":false}"#,
        )
        .unwrap();
        let checks = detect_with(
            &repo,
            &FloorContext {
                stage: FloorStage::Creator,
                ..ctx.clone()
            },
        )
        .unwrap();
        let targeted = checks
            .iter()
            .find(|c| c.name == "test_targeted" && c.dir == "giphy")
            .unwrap_or_else(|| panic!("{checks:?}"));
        assert!(
            targeted.argv.iter().any(|a| a == "src/server.ts")
                && !targeted.argv.iter().any(|a| a.starts_with("giphy/")),
            "{targeted:?}"
        );
        assert!(!baseline_diff_enabled(&repo.join("giphy")) && baseline_diff_enabled(&repo));
        // The check runs IN its package directory.
        let sandbox = sandbox_for(&repo);
        if sandbox.level != SandboxLevel::Sandboxed {
            return;
        }
        let scratch_dir = CheckScratch::prepare(&repo).unwrap();
        let pwd = RepoCheck {
            name: "test".into(),
            argv: s(&["sh", "-c", "pwd"]),
            source: "fixture".into(),
            timeout_s: None,
            dir: "giphy".into(),
        };
        let r = run_one(&repo, &pwd, &sandbox, &scratch_dir, Tree::Head);
        if r.passed() {
            assert!(r.stdout_tail.trim_end().ends_with("/giphy"), "{r:?}");
        }
    }

    /// core#432 F-3R2-023: a verify floor that ran no end-to-end check over a change touching e2e
    /// files says so on the gate's note — a pass included — and stays silent once one ran.
    #[cfg(unix)]
    #[test]
    fn an_e2e_change_the_floor_did_not_run_is_disclosed_432() {
        let repo = scratch("e2e-cover");
        std::fs::create_dir_all(repo.join(".wicked")).unwrap();
        std::fs::create_dir_all(repo.join("e2e")).unwrap();
        std::fs::write(repo.join(".gitignore"), "tmp/\n").unwrap();
        std::fs::write(
            repo.join(CONFIG_PATH),
            r#"{"test":["true"],"timeout_s":45}"#,
        )
        .unwrap();
        std::fs::write(repo.join("e2e/carve.py"), "XFAIL = True\n").unwrap();
        std::fs::write(repo.join("e2e/old.py"), "pass\n").unwrap();
        let base = git_repo_with_commit(&repo);
        std::fs::write(repo.join("e2e/carve.py"), "XFAIL = False\n").unwrap();
        std::fs::remove_file(repo.join("e2e/old.py")).unwrap();
        let ctx = FloorContext {
            stage: FloorStage::Verify,
            base_head: Some(base),
            git_dir: Some(repo.join(".git")),
            ..FloorContext::default()
        };
        let report = run_floor(&repo, &ctx);
        if report.sandbox_error.is_some() {
            eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
            return;
        }
        assert!(report.passed, "{report:?}");
        let note = report
            .classification_note()
            .expect("the gap is on the note");
        assert!(
            note.contains("touches end-to-end files e2e/carve.py, e2e/old.py")
                && note.contains("did NOT run them")
                && note.contains("no `e2e` or `journeys` check is declared"),
            "{note}"
        );
        // The creator floor never runs e2e, so it never says this.
        let creator = run_floor(
            &repo,
            &FloorContext {
                stage: FloorStage::Creator,
                ..ctx.clone()
            },
        );
        assert!(creator.coverage_note.is_none(), "{creator:?}");
        // A declared e2e that runs answers it.
        std::fs::write(
            repo.join(CONFIG_PATH),
            r#"{"test":["true"],"e2e":["true"],"timeout_s":45}"#,
        )
        .unwrap();
        let report = run_floor(&repo, &ctx);
        assert_eq!(names_run(&report), vec!["test", "e2e"]);
        assert!(report.coverage_note.is_none(), "{report:?}");
        // One that could not start did not run them.
        std::fs::write(
            repo.join(CONFIG_PATH),
            r#"{"test":["true"],"e2e":["wicked-no-such-e2e-binary"],"timeout_s":45}"#,
        )
        .unwrap();
        let report = run_floor(&repo, &ctx);
        assert!(report
            .checks
            .iter()
            .any(|c| c.name == "e2e" && c.spawn_error.is_some()));
        assert!(
            report
                .coverage_note
                .as_deref()
                .is_some_and(|n| n.contains("the declared end-to-end check could not start")),
            "{report:?}"
        );
        assert!(is_e2e_path("web/tests/checkout.e2e.ts") && is_e2e_path("Playwright/x.ts"));
        assert!(!is_e2e_path("src/app.ts") && !is_e2e_path("src/e2e_helpers/x.rs"));
    }

    #[cfg(unix)]
    fn detect_with_bad_key(repo: &Path) -> String {
        std::fs::write(
            repo.join(CONFIG_PATH),
            r#"{"test":["true"],"e2e_suite":["x"]}"#,
        )
        .unwrap();
        detect_with(repo, &FloorContext::default()).expect_err("unknown key is refused")
    }

    #[cfg(unix)]
    fn names_run(report: &RepoChecksReport) -> Vec<&str> {
        report.checks.iter().map(|c| c.name.as_str()).collect()
    }

    fn run_named(name: &str, exit_code: Option<i32>, timed_out: bool) -> CheckRun {
        CheckRun {
            name: name.into(),
            argv: vec!["sh".into()],
            source: "t".into(),
            exit_code,
            timed_out,
            spawn_error: None,
            duration_ms: 1,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            bound_s: 1,
            bound_note: None,
            failure_ids: Vec::new(),
            flaky_ids: Vec::new(),
            classification: None,
            pre_existing: Vec::new(),
            regressions: Vec::new(),
            base: None,
            env_cannot_run: Vec::new(),
            reruns: Vec::new(),
            decision: None,
            logs: Vec::new(),
            load_band: None,
        }
    }

    /// (deliver honors an override) A floor result's key: only the ordinary floor that RAN is keyed —
    /// never one that did not run, an operator's re-run or a floor with waived checks — and the
    /// digest moves with the stage, the package dir and every check's identity (a forced install
    /// moves it through the `install` check it adds).
    #[test]
    fn a_floor_key_is_the_ordinary_floor_that_ran_and_its_exact_check_set() {
        let check = |name: &str, argv: &[&str]| RepoCheck {
            name: name.into(),
            argv: argv.iter().map(|a| a.to_string()).collect(),
            source: "test".into(),
            timeout_s: None,
            dir: String::new(),
        };
        let mut r = report_of(Vec::new(), &[]);
        r.passed = false;
        r.detected = vec![
            check("lint", &["npm", "run", "lint"]),
            check("test", &["npm", "test"]),
        ];
        let ctx = FloorContext {
            stage: FloorStage::Verify,
            base_head: Some("base".into()),
            ..Default::default()
        };
        let key = r
            .key_for(Some("tree"), &ctx)
            .expect("the ordinary floor is keyed");
        assert_eq!(key.tree, "tree");
        assert_eq!(key.base.as_deref(), Some("base"));
        assert!(r.key_for(None, &ctx).is_none(), "no tree, no key");
        assert!(r.key_for(Some(""), &ctx).is_none());
        let mut not_run = r.clone();
        not_run.sandbox_error = Some("no boundary".into());
        assert!(not_run.key_for(Some("tree"), &ctx).is_none(), "did not run");
        let mut rerun = r.clone();
        rerun.rerun = Some(FloorRerunMode::Targeted);
        assert!(
            rerun.key_for(Some("tree"), &ctx).is_none(),
            "an operator's re-run"
        );
        let mut waived = r.clone();
        waived.waived = vec!["lint".into()];
        assert!(
            waived.key_for(Some("tree"), &ctx).is_none(),
            "waived checks"
        );
        // The digest: any change to the context or the set is another key.
        let digest = |c: &FloorContext, d: &[RepoCheck]| check_set_digest(c, d);
        let base = digest(&ctx, &r.detected);
        let pkg = FloorContext {
            package_dir: Some("pkg".into()),
            ..ctx.clone()
        };
        let creator = FloorContext {
            stage: FloorStage::Creator,
            ..ctx.clone()
        };
        let reordered = vec![r.detected[1].clone(), r.detected[0].clone()];
        let mut bounded = r.detected.clone();
        bounded[1].timeout_s = Some(60);
        let mut moved = r.detected.clone();
        moved[1].dir = "pkg".into();
        for (tag, other) in [
            (
                "an added install",
                digest(
                    &ctx,
                    &[vec![check("install", &["npm", "ci"])], r.detected.clone()].concat(),
                ),
            ),
            ("package dir", digest(&pkg, &r.detected)),
            ("stage", digest(&creator, &r.detected)),
            ("order", digest(&ctx, &reordered)),
            ("bound", digest(&ctx, &bounded)),
            ("dir", digest(&ctx, &moved)),
            ("subset", digest(&ctx, &r.detected[..1])),
        ] {
            assert_ne!(other, base, "{tag}");
        }
        assert_eq!(digest(&ctx, &r.detected), base, "deterministic");
    }

    fn report_of(checks: Vec<CheckRun>, skipped: &[&str]) -> RepoChecksReport {
        let passed = !checks.iter().any(|c| c.denies());
        RepoChecksReport {
            detected: Vec::new(),
            checks,
            skipped: skipped.iter().map(|s| s.to_string()).collect(),
            passed,
            detect_error: None,
            sandbox_level: "sandboxed".into(),
            sandbox_note: None,
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
        }
    }

    /// core#469: `accept_partial` waives exactly what the timed-out floor did not verify — the
    /// check that hit its bound and every one that never ran after it — and nothing on a floor
    /// that FAILED (a failure is not "unfinished").
    #[test]
    fn the_partial_waiver_is_what_the_timed_out_floor_did_not_verify() {
        let timed_out = report_of(
            vec![
                run_named("lint", Some(0), false),
                run_named("test", None, true),
            ],
            &["e2e"],
        );
        assert!(timed_out.timed_out());
        assert_eq!(partial_waiver(&timed_out), vec!["test", "e2e"]);
        let failed = report_of(
            vec![
                run_named("lint", Some(0), false),
                run_named("test", Some(1), false),
            ],
            &[],
        );
        assert!(partial_waiver(&failed).is_empty());
        let passed = report_of(vec![run_named("lint", Some(0), false)], &[]);
        assert!(partial_waiver(&passed).is_empty());
    }

    /// core#469: what each operator re-run runs. `extend` doubles every non-install bound;
    /// `targeted` swaps the full test set for the declared `test_targeted` (disclosing the
    /// replaced full set) or, with none declared, waives the test set; `accept_partial` drops the
    /// waived checks and runs the rest.
    #[test]
    fn an_operator_rerun_selects_its_checks_and_names_what_it_waives() {
        let wt = scratch("rerun-select");
        std::fs::create_dir_all(wt.join(".wicked")).unwrap();
        std::fs::write(
            wt.join(CONFIG_PATH),
            r#"{"lint":["sh","-c","exit 0"],"test":["sh","-c","exit 0"],"full":true,"timeout_s":10}"#,
        )
        .unwrap();
        let ctx = FloorContext::default(); // verify stage
        let detected = detect_with(&wt, &ctx).unwrap();
        assert_eq!(names(&detected), vec!["lint", "test"]);
        let rerun = |mode, waive: &[&str]| FloorRerun {
            mode,
            waive: waive.iter().map(|w| w.to_string()).collect(),
            output: String::new(),
            fix: None,
        };

        let (run, waived) = apply_rerun(
            &wt,
            &ctx,
            &rerun(FloorRerunMode::Extend, &[]),
            detected.clone(),
        )
        .unwrap();
        assert!(waived.is_empty());
        assert!(
            run.iter().all(|c| c.timeout_s == Some(10 * EXTEND_FACTOR)),
            "{run:?}"
        );

        let (run, waived) = apply_rerun(
            &wt,
            &ctx,
            &rerun(FloorRerunMode::Targeted, &[]),
            detected.clone(),
        )
        .unwrap();
        assert_eq!(names(&run), vec!["lint"]);
        assert_eq!(
            waived,
            vec!["test"],
            "no targeted command: the test set is waived"
        );

        let (run, waived) = apply_rerun(
            &wt,
            &ctx,
            &rerun(FloorRerunMode::AcceptPartial, &["test"]),
            detected.clone(),
        )
        .unwrap();
        assert_eq!(names(&run), vec!["lint"]);
        assert_eq!(waived, vec!["test"]);

        // A declared targeted command stands in for the full set, even under `full: true`.
        std::fs::write(
            wt.join(CONFIG_PATH),
            r#"{"lint":["sh","-c","exit 0"],"test":["sh","-c","exit 0"],"test_targeted":["sh","-c","exit 0"],"full":true}"#,
        )
        .unwrap();
        let detected = detect_with(&wt, &ctx).unwrap();
        assert_eq!(names(&detected), vec!["lint", "test"], "verify, full: true");
        let (run, waived) =
            apply_rerun(&wt, &ctx, &rerun(FloorRerunMode::Targeted, &[]), detected).unwrap();
        assert_eq!(names(&run), vec!["lint", "test_targeted"]);
        assert_eq!(waived, vec!["test"], "the replaced full set is disclosed");
        let _ = std::fs::remove_dir_all(&wt);
    }

    // ─── core#481 / core#546 / core#553 / core#482 ──────────────────────────────────────────────

    fn failed_run(ids: &[&str], env: &[&str]) -> CheckRun {
        CheckRun {
            name: "cargo-test".to_string(),
            argv: s(&["cargo", "test", "--locked", "--workspace"]),
            source: "Cargo.toml".to_string(),
            exit_code: Some(101),
            timed_out: false,
            spawn_error: None,
            duration_ms: 10,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            bound_s: 1200,
            bound_note: None,
            failure_ids: ids.iter().map(|x| x.to_string()).collect(),
            flaky_ids: Vec::new(),
            classification: None,
            pre_existing: Vec::new(),
            regressions: Vec::new(),
            base: None,
            env_cannot_run: env.iter().map(|x| x.to_string()).collect(),
            reruns: Vec::new(),
            decision: None,
            logs: Vec::new(),
            load_band: None,
        }
    }

    fn base_of(run: CheckRun) -> BaseRun {
        BaseRun {
            head: "0123456789abcdef".to_string(),
            cached: false,
            run: Some(run),
            error: None,
        }
    }

    /// core#781: a regression read against a base CACHED from another load band says so in the
    /// denial; a fresh base, or a cached one from the same band, does not.
    #[test]
    fn a_cached_base_from_another_load_band_is_disclosed_781() {
        let at = |band: u32, note: &str| CheckRun {
            load_band: Some(band),
            bound_note: Some(note.to_string()),
            ..failed_run(&[], &[])
        };
        let passing_base = |band: u32, cached: bool| BaseRun {
            cached,
            ..base_of(CheckRun {
                exit_code: Some(0),
                ..at(band, "1200s × 3.00 (1-min load 63.1 / 14 cpus)")
            })
        };
        let denial = |head_band: u32, base: BaseRun| {
            let mut head = CheckRun {
                failure_ids: vec!["FAIL tests/chord.test.ts > scans".to_string()],
                flaky_ids: Vec::new(),
                ..at(head_band, "1200s × 3.00 (1-min load 100.9 / 14 cpus)")
            };
            head.base = Some(Box::new(base));
            head.classification = Some(REGRESSION.to_string());
            report_of(vec![head], &[]).denial_reason()
        };
        let r = denial(7, passing_base(4, true));
        assert!(
            r.contains(
                "the base result is CACHED from an earlier floor of this run, under 1200s × \
                 3.00 (1-min load 63.1 / 14 cpus); this check ran under 1200s × 3.00 (1-min load \
                 100.9 / 14 cpus) — a different load band"
            ),
            "{r}"
        );
        for (head_band, base) in [(7, passing_base(4, false)), (4, passing_base(4, true))] {
            let r = denial(head_band, base);
            assert!(!r.contains("CACHED"), "{r}");
        }
        assert_eq!(load_band(Some(100.9), 14), Some(7));
        assert_eq!(load_band(Some(13.9), 14), Some(0));
        assert_eq!(load_band(None, 14), None);
        assert_eq!(load_band(Some(f64::NAN), 14), None);
    }

    /// core#482: a repo with no configured test commands gets a derived change-scoped set at the
    /// CREATOR floor — the crates owning the touched files, or `vitest related` over them — and
    /// the full set wherever the change can reach every test (manifests, lockfiles, configs) and
    /// at verify.
    #[test]
    fn an_unconfigured_repo_derives_its_targeted_set_from_the_touched_files_482() {
        let repo = scratch("derive482");
        for c in ["a", "b"] {
            std::fs::create_dir_all(repo.join(c).join("src")).unwrap();
            std::fs::write(
                repo.join(c).join("Cargo.toml"),
                format!("[package]\nname = \"{c}\" # the crate\nversion = \"0.0.0\"\n"),
            )
            .unwrap();
            std::fs::write(repo.join(c).join("src/lib.rs"), "\n").unwrap();
        }
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\"]\n",
        )
        .unwrap();
        std::fs::write(repo.join("README.md"), "x\n").unwrap();
        let base = git_repo_with_commit(&repo);
        let ctx = |stage| FloorContext {
            stage,
            base_head: Some(base.clone()),
            git_dir: Some(repo.join(".git")),
            ..FloorContext::default()
        };
        let test_of = |d: &[RepoCheck]| {
            d.iter()
                .find(|c| c.name == "cargo-test" || c.name == "test_targeted")
                .cloned()
                .expect("a test set")
        };
        std::fs::write(repo.join("b/src/lib.rs"), "pub fn x() {}\n").unwrap();
        let t = test_of(&detect_with(&repo, &ctx(FloorStage::Creator)).unwrap());
        assert_eq!(t.name, "test_targeted");
        assert_eq!(t.source, DERIVED_TARGETED_SOURCE);
        assert_eq!(t.argv, s(&["cargo", "test", "--no-fail-fast", "-p", "b"]));
        let t = test_of(&detect_with(&repo, &ctx(FloorStage::Verify)).unwrap());
        assert_eq!(t.name, "cargo-test", "verify keeps the full suite");
        // The workspace manifest can change every member ⇒ full.
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        let t = test_of(&detect_with(&repo, &ctx(FloorStage::Creator)).unwrap());
        assert_eq!(t.name, "cargo-test");
        git(&repo, &["checkout", "-q", "--", "Cargo.toml"]);
        // A deleted file elsewhere (crate `a`) could break what it left ⇒ full.
        std::fs::remove_file(repo.join("a/src/lib.rs")).unwrap();
        let t = test_of(&detect_with(&repo, &ctx(FloorStage::Creator)).unwrap());
        assert_eq!(t.name, "cargo-test", "a deletion never narrows the set");
        git(
            &repo,
            &["checkout", "-q", "--", "a/src/lib.rs", "b/src/lib.rs"],
        );
        // A path no package owns beside a crate change (a shared file any crate may
        // `include!`) ⇒ full.
        std::fs::write(repo.join("README.md"), "y\n").unwrap();
        std::fs::write(repo.join("b/src/lib.rs"), "pub fn x() {}\n").unwrap();
        let t = test_of(&detect_with(&repo, &ctx(FloorStage::Creator)).unwrap());
        assert_eq!(t.name, "cargo-test");
        // A configured test command is the repo's own choice — never overridden.
        std::fs::write(repo.join("a/src/lib.rs"), "pub fn y() {}\n").unwrap();
        std::fs::create_dir_all(repo.join(".wicked")).unwrap();
        std::fs::write(
            repo.join(CONFIG_PATH),
            r#"{"test":["cargo","test","--workspace"]}"#,
        )
        .unwrap();
        let t = detect_with(&repo, &ctx(FloorStage::Creator)).unwrap();
        assert!(names(&t).contains(&"test") && !names(&t).contains(&"test_targeted"));
        let _ = std::fs::remove_dir_all(&repo);

        // vitest: `vitest related` over the touched files; a touched lockfile ⇒ full.
        let repo = scratch("derive482-node");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(
            repo.join("package.json"),
            r#"{"scripts":{"test":"vitest run","lint":"eslint ."}}"#,
        )
        .unwrap();
        std::fs::write(repo.join("package-lock.json"), "{}").unwrap();
        std::fs::write(repo.join("src/x.ts"), "export {}\n").unwrap();
        let base = git_repo_with_commit(&repo);
        let ctx = FloorContext {
            stage: FloorStage::Creator,
            base_head: Some(base),
            git_dir: Some(repo.join(".git")),
            ..FloorContext::default()
        };
        std::fs::write(repo.join("src/x.ts"), "export const x = 1\n").unwrap();
        let d = detect_with(&repo, &ctx).unwrap();
        let t = d
            .iter()
            .find(|c| c.name == "test_targeted")
            .expect("derived");
        assert_eq!(
            t.argv,
            s(&[
                "npx",
                "--no-install",
                "vitest",
                "related",
                "--run",
                "--passWithNoTests",
                "src/x.ts"
            ])
        );
        std::fs::create_dir_all(repo.join("tests")).unwrap();
        std::fs::write(repo.join("tests/setup.ts"), "export {}\n").unwrap();
        let d = detect_with(&repo, &ctx).unwrap();
        assert!(
            names(&d).contains(&"test") && !names(&d).contains(&"test_targeted"),
            "a config-loaded setup file reaches every test"
        );
        std::fs::remove_file(repo.join("tests/setup.ts")).unwrap();
        std::fs::write(repo.join("package-lock.json"), "{ }").unwrap();
        let d = detect_with(&repo, &ctx).unwrap();
        assert!(names(&d).contains(&"test") && !names(&d).contains(&"test_targeted"));
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// A cached base run of a targeted command answers only the SAME argv: a rework that touches
    /// another crate derives another `-p`, and must not be compared against the first one's base.
    /// A change that edits a configured command (fail-fast → `--no-fail-fast`) is compared on
    /// the base with the SAME command: the base's shorter failure list is not read as the head's
    /// regressions.
    #[test]
    fn the_base_runs_the_heads_command_when_the_change_edits_it() {
        let repo = scratch("basecmd");
        std::fs::create_dir_all(repo.join(".wicked")).unwrap();
        let cfg = |script: &str| serde_json::json!({ "test": ["sh", "-c", script] }).to_string();
        std::fs::write(
            repo.join(".wicked/checks.json"),
            cfg("echo 'test t::one ... FAILED'; exit 101"),
        )
        .unwrap();
        let base = git_repo_with_commit(&repo);
        std::fs::write(
            repo.join(".wicked/checks.json"),
            cfg("echo 'test t::one ... FAILED'; echo 'test t::two ... FAILED'; exit 101"),
        )
        .unwrap();
        let ctx = FloorContext {
            stage: FloorStage::Verify,
            base_head: Some(base),
            git_dir: Some(repo.join(".git")),
            ..FloorContext::default()
        };
        let report = run_floor(&repo, &ctx);
        if report.sandbox_error.is_some() {
            eprintln!("repo_checks: no sandbox tool here — the baseline diff cannot run");
            return;
        }
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "test")
            .expect("test ran");
        assert!(c.regressions.is_empty(), "{c:#?}");
        let b = c
            .base
            .as_deref()
            .and_then(|b| b.run.as_ref())
            .expect("base ran");
        assert_eq!(b.argv, c.argv, "the base ran the head's command");
        // DES-FLOOR-V2: `sh` has no precise deselect — the base red escalates, never excused.
        assert_eq!(
            c.decision.as_ref().map(|d| d.outcome.as_str()),
            Some(OUTCOME_BASE_RED_UNSEPARATED),
            "{c:#?}"
        );
        assert!(!report.passed);
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// (codex on #1066) [`HeldDir`] never follows a link: not a linked directory component, not a
    /// linked leaf, not `..`; it never opens a FIFO for a blocking write; and once the ROOT it
    /// holds is swapped for a link to an outside directory, its writes land in the original
    /// (detached) directory — never outside — and a fresh hold of the linked path is refused.
    #[cfg(unix)]
    #[test]
    fn a_held_dir_never_follows_a_linked_component_nor_a_swapped_root() {
        let base = scratch("held-dir");
        let outside = scratch("held-dir-outside");
        std::fs::write(outside.join("victim"), "v").unwrap();
        let root = base.join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let held = HeldDir::open(&root).unwrap();
        // A linked directory component: no write, no directory, no fresh directory through it,
        // and a removal through it removes nothing.
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let e = held.write("link/x", "x").unwrap_err();
        assert!(e.contains("passes through a symlink"), "{e}");
        let e = held.subdir("link", false).err().expect("refused");
        assert!(e.contains("passes through a symlink"), "{e}");
        assert!(held.create_fresh_dir("link/fresh").is_err());
        assert!(held.write_bytes("sub/../link/y", b"y", false).is_err());
        assert_eq!(held.remove_file("link/victim"), Ok(false));
        // A linked leaf: never written through, never removed as a file.
        std::os::unix::fs::symlink(outside.join("victim"), root.join("leaf")).unwrap();
        let e = held.write("leaf", "x").unwrap_err();
        assert!(e.contains("passes through a symlink"), "{e}");
        assert_eq!(held.remove_file("leaf"), Ok(false));
        assert!(std::fs::symlink_metadata(root.join("leaf")).is_ok());
        assert!(
            held.symlink("leaf", b"elsewhere").is_err(),
            "never replaced"
        );
        for rel in ["../x", "/abs", "sub/../../x", "", "."] {
            assert!(held.write(rel, "x").is_err(), "{rel:?}");
        }
        // A FIFO at the path: refused at once, never waited on.
        let fifo =
            std::ffi::CString::new(root.join("fifo").as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        assert!(held.write("fifo", "x").is_err());
        // A plain write creates its directories; an executable is 0755; a fresh directory is
        // refused where one already exists.
        held.write_bytes("sub/new/f", b"ok", true).unwrap();
        assert_eq!(std::fs::read(root.join("sub/new/f")).unwrap(), b"ok");
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join("sub/new/f"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        assert!(held.create_fresh_dir("sub/new").is_err());
        held.create_fresh_dir("sub/fresh")
            .unwrap()
            .write("in", "1")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("sub/fresh/in")).unwrap(),
            "1"
        );
        assert_eq!(held.remove_file("sub/fresh/in"), Ok(true));
        // The ROOT swapped for a link to the outside after it was held.
        let moved = base.join("moved");
        std::fs::rename(&root, &moved).unwrap();
        std::os::unix::fs::symlink(&outside, &root).unwrap();
        held.write("after-swap", "x").unwrap();
        held.create_fresh_dir("tmp/export").unwrap();
        assert!(moved.join("after-swap").is_file());
        assert!(moved.join("tmp/export").is_dir());
        assert!(HeldDir::open(&root).is_err(), "a linked root is never held");
        let mut seen: Vec<String> = std::fs::read_dir(&outside)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        seen.sort();
        assert_eq!(seen, ["victim"], "nothing landed outside");
        assert_eq!(
            std::fs::read_to_string(outside.join("victim")).unwrap(),
            "v"
        );
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// (confirmation round on d32b2b2, codex P1) The base cache record is written by the engine,
    /// outside the sandbox: whatever a check's `name` or `dir` say — a traversal included — its
    /// path is ONE file directly in the run's log store (outside every sandbox write root), and
    /// distinct (dir, name) pairs never share one.
    #[test]
    fn a_traversal_check_name_never_writes_outside_the_base_cache() {
        let wt = scratch("cache-name");
        let scratch_dir = CheckScratch::prepare(&wt).expect("scratch");
        let cache = scratch_dir.logs.clone();
        assert!(
            !cache.starts_with(&scratch_dir.root),
            "outside the checks' scratch"
        );
        let head = "0123456789abcdef0123";
        let at = |dir: &str, name: &str| {
            BaseTree::cache_path(
                &scratch_dir,
                head,
                &RepoCheck {
                    name: name.into(),
                    argv: Vec::new(),
                    source: String::new(),
                    timeout_s: None,
                    dir: dir.into(),
                },
            )
        };
        let mut seen = Vec::new();
        for (dir, name) in [
            ("", "../../../escaped"),
            ("", "/tmp/escaped"),
            ("", "a/b"),
            ("a", "b"),
            ("a/b", ""),
            ("../..", ".."),
            ("", &"x".repeat(400)),
        ] {
            let path = at(dir, name);
            assert_eq!(path.parent(), Some(cache.as_path()), "{dir:?} {name:?}");
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                file.len() == 5 + 64 + 5
                    && file.starts_with("base-")
                    && file.ends_with(".json")
                    && file[5..69].bytes().all(|b| b.is_ascii_hexdigit()),
                "{file}"
            );
            assert!(!seen.contains(&path), "{dir:?} {name:?} shares an entry");
            seen.push(path.clone());
            // The record lands where the floor writes it: inside the cache, nowhere else.
            std::fs::create_dir_all(&cache).unwrap();
            std::fs::write(&path, "{}").unwrap();
        }
        let records = std::fs::read_dir(&cache)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("base-"))
            .count();
        assert_eq!(records, seen.len());
        assert!(!scratch_dir.root.join("escaped").exists());
        assert!(!wt.join("escaped").exists());
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn a_cached_targeted_base_run_answers_only_its_own_argv() {
        let wt = scratch("cache-argv");
        let scratch_dir = CheckScratch::prepare(&wt).expect("scratch");
        let head = "0123456789abcdef0123";
        let mut ran = failed_run(&["test t::x [-p b --lib]"], &[]);
        ran.name = "test_targeted".into();
        ran.argv = s(&["cargo", "test", "-p", "b"]);
        let path = BaseTree::cache_path(
            &scratch_dir,
            head,
            &RepoCheck {
                name: "test_targeted".into(),
                argv: Vec::new(),
                source: String::new(),
                timeout_s: None,
                dir: String::new(),
            },
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A record from before the policy version (a bare check run) is a miss.
        std::fs::write(&path, serde_json::to_string(&ran).unwrap()).unwrap();
        let bare = RepoCheck {
            name: "test_targeted".into(),
            argv: s(&["cargo", "test", "-p", "b"]),
            source: DERIVED_TARGETED_SOURCE.into(),
            timeout_s: None,
            dir: String::new(),
        };
        assert!(BaseTree::cached(&scratch_dir, head, &bare, None).is_none());
        let rec = BaseRecord {
            floor_policy: FLOOR_POLICY_VERSION,
            head_argv: s(&["cargo", "test", "-p", "b"]),
            guide: None,
            run: ran,
            toc: None,
        };
        std::fs::write(&path, serde_json::to_string(&rec).unwrap()).unwrap();
        // (review round 15, item 17) A record taken without a guide is no answer under one.
        let g: crate::floor_guide::GuideCheck = serde_json::from_value(serde_json::json!({
            "name": "test", "run": ["cargo", "test", "-p", "b"]}))
        .unwrap();
        let b = RepoCheck {
            name: "test_targeted".into(),
            argv: s(&["cargo", "test", "-p", "b"]),
            source: DERIVED_TARGETED_SOURCE.into(),
            timeout_s: None,
            dir: String::new(),
        };
        assert!(BaseTree::cached(&scratch_dir, head, &b, Some(&g)).is_none());
        let check = |argv: &[&str]| RepoCheck {
            name: "test_targeted".into(),
            argv: s(argv),
            source: DERIVED_TARGETED_SOURCE.into(),
            timeout_s: None,
            dir: String::new(),
        };
        assert!(BaseTree::cached(
            &scratch_dir,
            head,
            &check(&["cargo", "test", "-p", "b"]),
            None
        )
        .is_some());
        assert!(BaseTree::cached(
            &scratch_dir,
            head,
            &check(&["cargo", "test", "-p", "a"]),
            None
        )
        .is_none());
        // (codex r12) A record whose blob is gone is no evidence: a miss, so the base re-runs.
        let mut rec: BaseRecord =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        rec.run.logs = vec![crate::floor_log::LogRef {
            attempt: "base".into(),
            stream: "stdout".into(),
            sha256: "0".repeat(64),
            bytes: 1,
            truncated: false,
        }];
        std::fs::write(&path, serde_json::to_string(&rec).unwrap()).unwrap();
        assert!(BaseTree::cached(
            &scratch_dir,
            head,
            &check(&["cargo", "test", "-p", "b"]),
            None
        )
        .is_none());
        drop(scratch_dir);
        let _ = std::fs::remove_dir_all(&wt);
    }
    /// core#767: `journeys` are VERIFY-stage checks named `journey-<name>`, in name order after
    /// `e2e`; a bad name fails detection; `false` drops one; a journey is a test-set check.
    #[test]
    fn journeys_are_verify_stage_checks_in_name_order_767() {
        let repo = scratch("journeys767");
        std::fs::create_dir_all(repo.join(".wicked")).unwrap();
        std::fs::write(
            repo.join(CONFIG_PATH),
            r#"{"test":["true"],"e2e":["true"],"journeys":{"desk_projects":["python3","e2e/desk_projects_test.py"],"cmdk":["python3","e2e/cmdk.py"],"off":false},"timeout_s":60}"#,
        )
        .unwrap();
        let verify = detect_with(&repo, &FloorContext::default()).unwrap();
        assert_eq!(
            names(&verify),
            vec!["test", "e2e", "journey-cmdk", "journey-desk_projects"]
        );
        assert_eq!(verify[3].argv, s(&["python3", "e2e/desk_projects_test.py"]));
        assert_eq!(verify[3].timeout_s, Some(60));
        assert!(is_test_set_check("journey-cmdk"));
        let creator = detect_with(
            &repo,
            &FloorContext {
                stage: FloorStage::Creator,
                ..FloorContext::default()
            },
        )
        .unwrap();
        assert_eq!(names(&creator), vec!["test"]);
        std::fs::write(
            repo.join(CONFIG_PATH),
            r#"{"test":["true"],"journeys":{"bad name":["true"]}}"#,
        )
        .unwrap();
        let err = detect_with(&repo, &FloorContext::default()).unwrap_err();
        assert!(err.contains("not a journey name"), "{err}");
        let _ = std::fs::remove_dir_all(&repo);
    }
}

#[cfg(test)]
#[path = "floor_tests.rs"]
pub(crate) mod floor_tests;

/// (j4-gates) A failed floor report for tests: one `test` check that exited 1 naming `head`, with a
/// base run (`base`: `Some(ids)` — red naming those, or green when empty) or none, decided
/// `base_red_unseparated` on a red base and `new_failure` otherwise.
#[cfg(test)]
pub(crate) fn red_report(head: &[&str], base: Option<&[&str]>) -> RepoChecksReport {
    let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let base_json = base.map(|b| {
        serde_json::json!({
            "head": "b1a5e5ha0123456789", "cached": true,
            "run": {
                "name": "test", "argv": ["cargo", "test"], "source": "Cargo.toml",
                "exit_code": if b.is_empty() { 0 } else { 1 }, "timed_out": false,
                "duration_ms": 10, "stdout_tail": "", "stderr_tail": "",
                "failure_ids": ids(b),
            }
        })
    });
    let outcome = match base {
        Some(b) if !b.is_empty() => OUTCOME_BASE_RED_UNSEPARATED,
        _ => OUTCOME_NEW_FAILURE,
    };
    serde_json::from_value(serde_json::json!({
        "detected": [], "skipped": [], "passed": false, "sandbox_level": "sandboxed",
        "checks": [{
            "name": "test", "argv": ["cargo", "test"], "source": "Cargo.toml",
            "exit_code": 1, "timed_out": false, "duration_ms": 10,
            "stdout_tail": "test result: FAILED", "stderr_tail": "error: test failed",
            "failure_ids": ids(head),
            "base": base_json,
            "decision": { "outcome": outcome },
        }],
    }))
    .unwrap()
}

#[cfg(test)]
mod failure_split_tests {
    use super::red_report;

    /// (core#1173) `generated` parses strictly: paths and an argv (array or line), in order; a
    /// path that escapes the repo, an empty set, an unknown key or a non-command is an error, never
    /// "nothing declared". Globs match by segment.
    #[test]
    fn generated_declarations_parse_strictly() {
        let (sets, timeout) = crate::repo_checks::generated_from_config(
            r#"{"timeout_s": 600, "generated": [
                {"paths": ["packages/crew/endpoint-manifest.json"], "command": "npm run manifest"},
                {"paths": ["packages/crew/tests/generated/**"], "command": ["npm", "run", "gen"]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(timeout, Some(600));
        assert_eq!(sets.len(), 2);
        assert_eq!(
            sets[0].argv,
            vec!["npm".to_string(), "run".to_string(), "manifest".to_string()]
        );
        assert!(sets[0].covers("packages/crew/endpoint-manifest.json"));
        assert!(!sets[0].covers("packages/crew/endpoint-manifest.json.bak"));
        assert!(sets[1].covers("packages/crew/tests/generated/api-sample.generated.test.ts"));
        assert!(!sets[1].covers("packages/crew/tests/other.test.ts"));
        assert_eq!(
            crate::repo_checks::generated_from_config("{}").unwrap().0,
            Vec::new()
        );
        for bad in [
            r#"{"generated": [{"paths": ["../x"], "command": "a"}]}"#,
            r#"{"generated": [{"paths": ["/abs"], "command": "a"}]}"#,
            r#"{"generated": [{"paths": ["a//b"], "command": "a"}]}"#,
            r#"{"generated": [{"paths": [], "command": "a"}]}"#,
            r#"{"generated": [{"paths": ["a"], "command": false}]}"#,
            r#"{"generated": [{"paths": ["a"], "command": []}]}"#,
            r#"{"generated": [{"paths": ["a"], "command": "a", "shell": true}]}"#,
        ] {
            assert!(
                crate::repo_checks::generated_from_config(bad).is_err(),
                "{bad}"
            );
        }
    }

    /// (j4-gates) The floor names the change's own failures — head minus base — and says when
    /// every failure is the base's; with no names it says nothing it does not know.
    #[test]
    fn the_floor_names_head_only_failures_and_says_when_all_are_base_red() {
        let r = red_report(
            &["test a::mine", "test b::shared"],
            Some(&["test b::shared", "test c::other"]),
        );
        let s = r.failure_split().unwrap();
        assert_eq!(s.head_only, vec!["test: test a::mine".to_string()]);
        assert_eq!(s.base_shared, 1);
        assert!(!s.all_base_red);
        let why = r.denial_reason();
        assert!(
            why.contains("HEAD-ONLY failures") && why.contains("`test: test a::mine`"),
            "{why}"
        );
        assert!(
            !why.contains("`test: test b::shared`"),
            "a base failure is not named as the change's: {why}"
        );

        let r = red_report(
            &["test b::shared"],
            Some(&["test b::shared", "test c::other"]),
        );
        let s = r.failure_split().unwrap();
        assert!(s.all_base_red && s.head_only.is_empty(), "{s:?}");
        assert!(r.denial_reason().contains("none of them is this change's"));

        // A green base: every named failure is the change's.
        let s = red_report(&["test a::mine"], Some(&[]))
            .failure_split()
            .unwrap();
        assert_eq!(s.head_only.len(), 1);
        // No base at all: listed, never shown shared, never "all base red".
        let s = red_report(&["test a::mine"], None).failure_split().unwrap();
        assert!(
            s.head_only.is_empty() && !s.all_base_red && s.unknown.len() == 1,
            "{s:?}"
        );
        // No names (a lint, a type error): nothing to split.
        assert!(red_report(&[], Some(&["x"])).failure_split().is_none());
    }

    /// (xreview) A run that printed a base failure and then hung is not a complete account: the
    /// split lists the shared failure but never says "none of them is this change's".
    #[test]
    fn a_timed_out_run_with_partial_names_is_never_all_base_red() {
        let mut r = red_report(&["test b::shared"], Some(&["test b::shared"]));
        r.checks[0].timed_out = true;
        r.checks[0].exit_code = None;
        let s = r.failure_split().unwrap();
        assert_eq!(s.base_shared, 1);
        assert!(!s.all_base_red, "{s:?}");
        assert!(!r.denial_reason().contains("none of them is this change's"));
    }

    /// (xreview) An excluded run named a head-only failure and the retry after it named nothing:
    /// the head-only failure is still named, and the split is not "all base red".
    /// (core#1172) The gate's split on the J4 shape: the check's final pass failed only a test the
    /// base fails too, and four first-pass failures cleared on its own retry. They are named as
    /// flaky (passed on retry), never as this change's, and the check still denies as before.
    #[test]
    fn failures_the_checks_retry_cleared_are_named_flaky_not_head_only() {
        let shared = "FAIL tests/system-route.test.ts > gateway";
        let mut r = red_report(&[shared], Some(&[shared]));
        r.checks[0].flaky_ids = vec![
            "FAIL tests/deliver-e2e.test.ts > delivers".to_string(),
            "FAIL tests/run-diff-790.test.ts > 500".to_string(),
        ];
        let denies = r.checks[0].denies();
        let s = r.failure_split().unwrap();
        assert!(s.head_only.is_empty(), "{s:?}");
        assert!(s.all_base_red, "{s:?}");
        assert_eq!(s.flaky.len(), 2, "{s:?}");
        let why = r.denial_reason();
        assert!(
            why.contains("flaky (passed on retry), not counted as this change's")
                && why.contains("`test: FAIL tests/deliver-e2e.test.ts > delivers`"),
            "{why}"
        );
        assert!(!why.contains("HEAD-ONLY"), "{why}");
        assert_eq!(r.checks[0].denies(), denies, "the verdict is untouched");
        assert!(!r.passed);
    }

    #[test]
    fn an_unnamed_retry_never_erases_a_head_only_failure_an_earlier_run_named() {
        let mut r = red_report(&["test b::shared"], Some(&["test b::shared"]));
        let mut excluded = r.checks[0].clone();
        excluded.decision = None;
        excluded.base = None;
        excluded.failure_ids = vec!["test a::mine".to_string()];
        let mut retry = excluded.clone();
        retry.failure_ids = Vec::new();
        r.checks[0].reruns = vec![excluded, retry];
        let s = r.failure_split().unwrap();
        assert_eq!(s.head_only, vec!["test: test a::mine".to_string()], "{s:?}");
        assert!(!s.all_base_red, "{s:?}");
    }
}
