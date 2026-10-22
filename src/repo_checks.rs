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
//! Each command's exit code, duration and the TAIL of its stdout/stderr are captured as a
//! [`CheckRun`] and attached to the gate: the fold persists the [`RepoChecksReport`] on the unit,
//! emits `repoChecksEvaluated`, and DENIES the unit when any check REGRESSES (exits non-zero on
//! the run's tree while the run base passes it), times out, or cannot be spawned (fail-closed — a
//! check that cannot run has re-derived nothing). Checks stop at the first denying failure: the
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
//! BASELINE-DIFF (F-RC2-009): a check the SANDBOX fails may fail the same way on the run base —
//! on 2026-09-13 the floor reported 27 `cargo test` failures on a change whose 27 tests pass on the
//! base in a normal shell. So when a check fails on the run's tree and the run knows its base
//! commit, the engine exports that base commit into the checks' scratch (`git checkout-index`
//! through the PINNED git dir — no nested worktree), runs the same check there once per run
//! (cached by base sha + check name; the export itself is removed as soon as its result is
//! cached, so a HEAD check that globs from the worktree root never sweeps it up), and compares
//! the two runs' failure identifiers (streamed
//! off the runner's output — `test x ... FAILED`, ` FAIL  file > name`, `path(l,c): error TS…`,
//! `FAILED tests/x.py::y`, `--- FAIL: TestX`, eslint stylish): failures that also fail on the base
//! — a subset or the identical set — are `pre_existing_in_sandbox` and never deny; only head-only
//! failures are `regression`s and deny. A libtest id (`test t::x`) never matches a base id
//! (core#1060): its text names no test binary, so the same test name red in crate A on the base
//! cannot mask a regression in crate B, and no line of output — all of it text the change's tests
//! can print — is read to say otherwise. Any libtest failure on the head is therefore a
//! regression unless its isolated re-run clears it (below); the operator's floor override is the
//! escape for a red the base shares. The auto-detected `cargo test` runs `--no-fail-fast` so every
//! failing binary is reported. A head-only set of
//! at most [`MAX_FLAKE_RERUN`] libtest or vitest ids, read on an oversubscribed host (1-min load
//! above the CPU count — the evidence it was load), is re-run ONCE, each alone and serially,
//! within [`FLAKE_RERUN_BUDGET_S`] (core#553): a
//! libtest id by its `--exact` name, a vitest id as its test FILE alone (core#766 — a 5 s
//! filesystem-walk `it` that hit vitest's default timeout under load 45-80 was denied twice as a
//! regression; vitest was not re-run because only `cargo test` was). A failure that passes alone
//! is "flaky (passed alone)" (`flaky`, never denies); one that fails again, or that the cap or the
//! budget does not reach (`not_rechecked`), stays a `regression`. If every one passes the check is
//! `flaky_under_load` (never denies; both attempts and the load at each are on the record). Other
//! runners' ids (path-qualified) are compared by equality; a libtest id the base fails too is
//! listed as `ambiguous`. A base that cannot be run or compared leaves the
//! check denying as before (fail-closed). `baseline_diff: false` in the repo config opts out. A
//! creator transcript that CLAIMS a failure is pre-existing is annotated against that comparison
//! ([`ClaimCheck`]: `claim_rejected` when the base is green).
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
//! `test` at verify even when `test_targeted` exists) and `baseline_diff` (default `true`). The
//! floor PREFERS `test_targeted` — at the creator stage always, at verify unless `full: true` —
//! substituting `{files}` (the paths the change touched relative to the base commit, one argv
//! element each) and `{base}` (the base commit id) so a runner's own change-aware mode can be used
//! (`vitest run --changed {base}`, `jest --changedSince {base}`, `cargo test -p <crate>`).
//! A repo that configures NO test command gets one derived from the touched files at the
//! creator floor (core#482, [`derive_targeted`]): `cargo test -p <owning crates>`, or
//! `vitest related --run <files>` for a plain `vitest` test script; the verify floor keeps the
//! full suite, so an unconfigured repo pays for one full run per governed run, not three.
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
//! installs need it; the one addition over a worker's is pseudo-terminal allocation, its own
//! ptys only, so PTY and tmux tests run — [`crate::validator::PtyPolicy`]), with an ISOLATED `HOME` and package caches in the floor's CHECK SCRATCH
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
//! - **A per-repository cap.** Before a floor runs, the repository's group is measured; over
//!   [`CHECK_CACHE_CAP_ENV`] GB (default [`DEFAULT_CHECK_CACHE_CAP_GB`]) the least recently used
//!   leaves are deleted until it fits, the floor's own last, and each removal is logged. A leaf a
//!   floor is using is never removed: every floor holds a SHARED lock on `<run key>.lock` for its
//!   life, and the cap removes a leaf only under an EXCLUSIVE one. Concurrent floors of ONE run
//!   share the leaf as they shared the worktree scratch, and cargo's own build-directory lock
//!   serializes their builds.
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
/// The most failure identifiers the streaming scanner keeps per check. Past it the check also
/// carries [`FAILURE_IDS_TRUNCATED`], which nothing matches.
pub const MAX_FAILURE_IDS: usize = 1000;
/// The failure id that stands for every id past [`MAX_FAILURE_IDS`]: a dropped id could have been
/// the head's only regression, so this one is never matched to a base id — the check denies.
pub const FAILURE_IDS_TRUNCATED: &str = "(more failure ids than the floor keeps)";
/// The prefix of a failure id that stands for a failure the output PROVES but does not name: a
/// test process that died (cargo's `process didn't exit successfully`), a crate or doctest that
/// did not compile, a pytest `ERROR`, a test file that failed as a whole (`FAIL <file>` with no
/// ` > `) — `unattributed failure: <kind>`. Like [`FAILURE_IDS_TRUNCATED`] it never matches a
/// base id and is never re-run alone. A line can only ADD one: a forged one costs the change, a
/// real one always denies — unless a clean confirmation run answers for it (core#1060).
pub const UNATTRIBUTED_FAILURE: &str = "unattributed failure: ";
/// A check whose named head-only failures all passed alone but that did not run clean with them
/// skipped (or could not be confirmed within the re-run budget): the denial's own reason.
pub const NO_CLEAN_RUN: &str =
    "unattributed failure: the check did not run clean with its flaky tests skipped";

/// A line with an ESC that mentions a failure token ([`ESCAPED_FAILURE_TOKENS`]) but yielded no
/// id in either reading, raw or stripped (core#1060): an escape may have hidden the failure's name,
/// so it is a failure the floor cannot name. Add-only; a clean confirmation answers for it.
pub const ESCAPED_FAILURE_LINE: &str = "unattributed failure: an escaped failure line";
/// The failure tokens of [`ESCAPED_FAILURE_LINE`], matched anywhere in the line, case-sensitive.
const ESCAPED_FAILURE_TOKENS: &[&str] = &[
    "FAILED",
    "FAIL ",
    "ERROR",
    "--- FAIL",
    "Couldn't compile",
    "could not compile",
    "test result: FAILED",
    "failed, to rerun pass",
];

/// The `source` of a check's confirmation run ([`recheck_alone`], [`rerun_load_class`]).
const CONFIRMATION_SOURCE: &str = "confirmation run (core#1060)";

/// (core#1060) No re-run path may start: the check already re-ran (from ANY path — whichever re-ran
/// first decides, no second path gets to try), or its failure set was truncated
/// ([`FAILURE_IDS_TRUNCATED`]: failures the floor never saw cannot be re-checked or answered for).
fn rerun_refused(run: &CheckRun) -> bool {
    !run.reruns.is_empty() || run.failure_ids.iter().any(|i| i == FAILURE_IDS_TRUNCATED)
}

/// Never matched to a base id, never re-run alone: the truncation marker and every
/// [`UNATTRIBUTED_FAILURE`].
fn never_matched(id: &str) -> bool {
    id == FAILURE_IDS_TRUNCATED || id.starts_with(UNATTRIBUTED_FAILURE)
}
/// The version of the floor's check policy (its sandbox and its failure-id format) a cached base
/// run was recorded under. A record of another — or no — version is a cache miss: core#1060
/// opened `/dev/ptmx` to the checks and stopped matching libtest ids (3), so a base result from
/// before it could make a head failure read as pre-existing. Bump it whenever either changes.
const FLOOR_POLICY_VERSION: u32 = 3;
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
/// signal). The floor no longer produces it — equal sets are [`PRE_EXISTING_IN_SANDBOX`] and the
/// positive-evidence environment case is [`ENV_CANNOT_RUN`]. Kept so a stored record still reads
/// as non-denying.
pub const FLOOR_ENV_MISMATCH: &str = "floor_env_mismatch";
/// The check's tool could not run on this host: it crashed with the same environment error on
/// the head, the base and a re-run (core#789). Never denies; external CI is its gate. (A stored
/// record may carry core#546's nested-sandbox class, which the floor no longer produces.)
pub const ENV_CANNOT_RUN: &str = "env_cannot_run";
/// (core#553) Every head-only failure (at most [`MAX_FLAKE_RERUN`]) of a floor that ran on an
/// OVERSUBSCRIBED host (1-min load above the CPU count) PASSED when re-run once, alone, in the
/// same worktree: a host-load flake, not a regression — never denies, named on the gate's
/// `floorNote`; both attempts (and the load at each) are on the record (`reruns`).
pub const FLAKY_UNDER_LOAD: &str = "flaky_under_load";
/// The largest head-only failure set the floor re-checks, each failure alone (core#553). 50 is
/// about twice the 21-27 head-only load flakes a floor saw at 1-min load 30-45 (run 2ebd9da2,
/// floors 5 and 7, which the old bound of 2 never re-checked). A larger set is not load noise worth
/// isolating: none of it is re-run, and all of it stays a regression.
pub const MAX_FLAKE_RERUN: usize = 50;
/// The wall clock (seconds) one check's isolated re-runs may take in total, run serially. 900 s
/// is about half of a loaded floor (a full `cargo test --workspace` took ~30 min at load 30-45),
/// enough for 50 single-test re-runs of an already-built binary. The budget is checked before
/// each re-run and each re-run is hard-bounded by what is left (the load factor that stretches
/// every other check's bound does not apply past it); failures it does not reach stay
/// regressions, counted in `not_rechecked`.
pub const FLAKE_RERUN_BUDGET_S: u64 = 900;
/// Both base and head fail with NO parseable failure identifiers (unknown runner output format,
/// e.g. node:test without a TAP-aware parser). Cannot determine if pre-existing or a regression —
/// fail-closed: always denies (#538).
pub const UNCLASSIFIED_RED_BASE: &str = "unclassified_red_base";
/// (core#789/#790) The most environment signals ([`CheckRun::env_signals`]) one check records;
/// a scan that hit the cap is marked [`SIGNALS_TRUNCATED`] and the re-run rules that COUNT
/// signals do not apply (fail-closed).
pub const MAX_ENV_SIGNALS: usize = 200;
/// The marker a capped signal list ends with.
pub const SIGNALS_TRUNCATED: &str = "truncated";
/// (core#790) The most test files a load-class re-run runs (in one serial invocation).
pub const MAX_LOAD_RERUN_FILES: usize = 100;

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

fn is_zero(n: &usize) -> bool {
    *n == 0
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
    /// Failure identifiers streamed off the runner's output (`test a::b ... FAILED`, ` FAIL
    /// file > name`, `path: error TS1234: …`, …) — what the baseline diff compares. Empty when the
    /// runner's format is not one the scanner knows (the diff then compares exit codes).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_ids: Vec<String>,
    /// `regression` | `pre_existing_in_sandbox` | `floor_env_mismatch` — set when the check failed
    /// AND the same check was run on the base ([`Self::base`]); `None` when it passed, timed out,
    /// could not run, or no base comparison was possible (the check then denies as before).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<String>,
    /// Head failures that ALSO fail on the base (never this change's doing).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_existing: Vec<String>,
    /// Head failures ABSENT on the base — the regressions that deny.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regressions: Vec<String>,
    /// The same check run on the run base, when the floor ran it (see [`BaseRun`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<Box<BaseRun>>,
    /// (core#546) Failing ids whose captured output carries a nested-sandbox launcher signature —
    /// tests this host cannot run under the floor, whatever the change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_cannot_run: Vec<String>,
    /// (core#553) The isolated re-runs of the head-only failure set, one per id, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reruns: Vec<CheckRun>,
    /// Head-only failures that PASSED their isolated re-run on an oversubscribed host: "flaky
    /// (passed alone)", never denials. When every head-only failure did, the check is
    /// `flaky_under_load` and `regressions` still names them (as before); otherwise
    /// `regressions` holds only the failures that still deny.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flaky: Vec<String>,
    /// Head-only failures the floor did NOT re-check alone — over [`MAX_FLAKE_RERUN`], past
    /// [`FLAKE_RERUN_BUDGET_S`], or a runner it cannot filter to one test. They stay regressions.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub not_rechecked: usize,
    /// Head libtest failures whose test name the base also fails: never matched (a libtest id names
    /// no test binary, core#1060) — kept as regressions, not guessed, and listed here so the denial
    /// says why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ambiguous: Vec<String>,
    /// (core#789/#790) Environment signals the scan read off the check's own output, at most
    /// [`MAX_ENV_SIGNALS`], each prefixed by its class: `load: <line>` (a vitest pool worker that
    /// never started — `Failed to start forks worker` / `Timeout waiting for worker to respond` —
    /// or a per-test `Test timed out in Nms`), `errors: <N>` (vitest's own error count —
    /// `Errors  N errors` / `Vitest caught N unhandled errors`) and `crash: <line>` (the tool itself died opening a dependency file —
    /// `EPERM`/`EACCES` under `node_modules`). Evidence for the re-run rules, never a verdict.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_signals: Vec<String>,
    /// (core#781) The host's load band as the check started: `floor(1-min load / cpus)` — 0 has
    /// headroom, N is N× oversubscribed. `None` where the load is unknown. A head failure compared
    /// against a CACHED base from another band says so ([`cached_base_load_note`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_band: Option<u32>,
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

    /// Does this check deny the unit? A failure the base shares (`pre_existing_in_sandbox`), a
    /// test the host cannot run on either tree (`env_cannot_run`) and a head-only failure that
    /// passed its isolated re-run (`flaky_under_load`) do NOT (nor a stored `floor_env_mismatch`);
    /// a regression, a timeout, a spawn failure and an un-compared failure do.
    pub fn denies(&self) -> bool {
        !self.passed()
            && !matches!(
                self.classification.as_deref(),
                Some(PRE_EXISTING_IN_SANDBOX)
                    | Some(FLOOR_ENV_MISMATCH)
                    | Some(ENV_CANNOT_RUN)
                    | Some(FLAKY_UNDER_LOAD)
            )
    }

    /// (core#789) Is this check's environment class a tool CRASH (`crash: …` entries) rather than
    /// a nested-sandbox launcher signature?
    pub fn env_crash(&self) -> bool {
        self.env_cannot_run
            .first()
            .is_some_and(|e| e.starts_with("crash: "))
            || self
                .regressions
                .first()
                .is_some_and(|e| e.starts_with("crash: "))
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
        let cls = match self.classification.as_deref() {
            Some(ENV_CANNOT_RUN) if !self.passed() && self.env_crash() => format!(
                " [env_cannot_run: the tool crashed on head, base and a re-run — {}]",
                self.env_cannot_run.join(", ")
            ),
            Some(FLAKY_UNDER_LOAD) if !self.passed() => format!(
                " [flaky_under_load: {} passed re-run alone]",
                self.regressions.join(", ")
            ),
            Some(c) if !self.passed() => format!(" [{c}]"),
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
}

impl RepoChecksReport {
    /// Did the floor stop on a check that hit its bound? A timed-out floor did not FINISH — it is
    /// not "checks failed" (core#469), and the fold denies it under [`DENIAL_SOURCE_TIMEOUT`].
    pub fn timed_out(&self) -> bool {
        self.checks.iter().any(|c| c.timed_out && c.denies())
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

    /// Any check whose base and head failures were identical — the floor's environment is the
    /// likely cause (F-RC2-009). Surfaced so the studio can show the recorded env beside it.
    pub fn env_mismatch(&self) -> bool {
        self.checks.iter().any(|c| {
            matches!(
                c.classification.as_deref(),
                Some(FLOOR_ENV_MISMATCH) | Some(ENV_CANNOT_RUN)
            )
        })
    }

    /// (core#546) What the floor could not test on this host, for the gate's `floorNote`: the
    /// failing ids that carry a nested-sandbox launcher signature. A NON-denying set ("the floor
    /// could not run N tests; external CI is the test gate for them"), and — the advisory — a
    /// head-only failure with the same signature, which still denies (fail-closed) but names the
    /// likely cause so a `request_changes` can too; plus (core#553) each `flaky_under_load` check,
    /// named. `None` when there is nothing of either kind to say.
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
        for c in &self.checks {
            // (core#789) A tool that crashed opening a dependency file on both trees and again on
            // its re-run: the check did not run here; external CI is its gate.
            if c.env_crash() {
                notes.push(format!(
                    "`{}`: the tool crashed with an environment error on the head, the base and a \
                     re-run ({}) — env_cannot_run, not denied and not skipping the remaining \
                     checks; external CI is the gate for it",
                    c.name,
                    c.env_cannot_run.join(", ")
                ));
            }
        }
        // (core#553) A failure that passed alone is not a denial, but it is not silent either: an
        // order-dependent failure the change introduced reads the same way.
        for c in &self.checks {
            if c.classification.as_deref() == Some(FLAKY_UNDER_LOAD) && c.env_crash() {
                notes.push(format!(
                    "`{}`: crashed with an environment error on head and base ({}) and passed \
                     its re-run — classed flaky_under_load, not denied",
                    c.name,
                    c.regressions.join(", ")
                ));
            } else if c.classification.as_deref() == Some(FLAKY_UNDER_LOAD) {
                notes.push(format!(
                    "`{}`: {} failed in the full run on an oversubscribed host and passed \
                     re-run alone — classed flaky_under_load (host load), not denied; an \
                     order-dependent failure reads the same way",
                    c.name,
                    c.regressions.join(", ")
                ));
            }
        }
        // What the re-checks and the head/base matching established beyond the verdict.
        for c in &self.checks {
            notes.extend(match_notes(c));
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
                if c.timed_out {
                    return format!(
                        "{line}. This check did not FINISH — the change is unverified by it, not \
                         refuted: extend the bound (`timeout_s` in `{CONFIG_PATH}`), declare a \
                         targeted test command (`test_targeted`), or accept typecheck + lint + \
                         targeted as this unit's floor at the gate"
                    );
                }
                // (F-RC2-009) What the baseline diff established about this failure.
                let base_note = match c.base.as_deref() {
                    Some(BaseRun {
                        run: Some(b), head, ..
                    }) if b.passed() => format!(
                        " — REGRESSION: the run base {} passes this check{}",
                        &head[..head.len().min(10)],
                        if c.regressions.is_empty() {
                            String::new()
                        } else {
                            format!(" (head-only failures: {})", c.regressions.join(", "))
                        }
                    ),
                    // (#538) Both sides red with no parseable failure identifiers — unknown runner
                    // output format; cannot determine pre-existing vs regression. Fail closed.
                    Some(BaseRun {
                        run: Some(_), head, ..
                    }) if c.classification.as_deref() == Some(UNCLASSIFIED_RED_BASE) => format!(
                        " — the run base {} also fails this check; no failure identifiers could \
                         be extracted from either run's output (unknown runner output format — \
                         add a parser or use a runner the floor knows, e.g. node:test in TAP \
                         mode). Cannot determine if pre-existing or a regression \
                         (unclassified_red_base — fail-closed)",
                        &head[..head.len().min(10)]
                    ),
                    Some(BaseRun {
                        run: Some(_), head, ..
                    }) => format!(
                        " — the run base {} fails this check too, but not identically: \
                         head-only failures {}{}",
                        &head[..head.len().min(10)],
                        if c.regressions.is_empty() {
                            "could not be told apart (failure identifiers were extracted on one \
                             side only)"
                                .to_string()
                        } else {
                            c.regressions.join(", ")
                        },
                        if c.pre_existing.is_empty() {
                            String::new()
                        } else {
                            format!("; also failing on the base: {}", c.pre_existing.join(", "))
                        }
                    ),
                    Some(BaseRun { error: Some(e), .. }) => {
                        format!(" — the base could not be compared ({e}); fail-closed")
                    }
                    _ => String::new(),
                };
                let matched = match_notes(c);
                let line = format!(
                    "{line}{base_note}{}{}",
                    cached_base_load_note(c),
                    if matched.is_empty() {
                        String::new()
                    } else {
                        format!("; {}", matched.join("; "))
                    }
                );
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
        // (F-RC2-009) Failures the run base shares are on the record, not in the verdict.
        let tolerated: Vec<String> = self
            .checks
            .iter()
            .filter(|c| !c.passed() && !c.denies())
            .map(CheckRun::summary)
            .collect();
        if !tolerated.is_empty() {
            s.push_str(&format!(
                " (failures the run base shares — recorded, not denying: {})",
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
        s.push_str(
            ". The engine ran these itself in the run's worktree; the full tails are on the \
             unit's `repo_checks` record and the `repoChecksEvaluated` event.",
        );
        s
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
    let cfg: ChecksConfig =
        serde_json::from_str(&raw).map_err(|e| format!("`{CONFIG_PATH}` is not valid: {e}"))?;
    if let Some(t) = cfg.timeout_s {
        if t == 0 || t > MAX_CONFIG_TIMEOUT_S {
            return Err(format!(
                "`{CONFIG_PATH}` `timeout_s` must be 1..={MAX_CONFIG_TIMEOUT_S} seconds, got {t}"
            ));
        }
    }
    Ok(Some(cfg))
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
fn touched_files(worktree: &Path, ctx: &FloorContext) -> Result<Vec<String>, String> {
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
    let tracked = crate::worktree_guard::git_string(
        worktree,
        &["diff", "--name-only", "--diff-filter=ACMR", base],
        &env,
    )
    .map_err(|e| format!("the change's touched files could not be listed: {e}"))?;
    let untracked = crate::worktree_guard::git_string(
        worktree,
        &["ls-files", "--others", "--exclude-standard"],
        &env,
    )
    .map_err(|e| format!("the change's untracked files could not be listed: {e}"))?;
    let scratch_prefix = format!("{}/", crate::worktree_guard::ENGINE_SCRATCH_DIR);
    let mut files: Vec<String> = Vec::new();
    for line in tracked.lines().chain(untracked.lines()) {
        let l = line.trim();
        if l.is_empty() || l.starts_with(&scratch_prefix) || files.iter().any(|f| f == l) {
            continue;
        }
        files.push(l.to_string());
    }
    Ok(files)
}

/// Did the change delete a tracked file relative to the base — renames counted as a deletion of
/// the old path (`--no-renames`)? A derived targeted set cannot see what a missing file broke.
fn deletes_a_file(worktree: &Path, ctx: &FloorContext) -> Result<bool, String> {
    Ok(!deleted_files(worktree, ctx)?.is_empty())
}

/// The tracked paths the change deleted relative to the base (a rename's old path included —
/// `--no-renames`). Empty when the run knows no base.
fn deleted_files(worktree: &Path, ctx: &FloorContext) -> Result<Vec<String>, String> {
    if let Some((root, dir)) = package_root(worktree, ctx) {
        let root_ctx = FloorContext {
            package_dir: None,
            ..ctx.clone()
        };
        return Ok(package_relative(deleted_files(&root, &root_ctx)?, &dir));
    }
    let (Some(base), Some(git_dir)) = (ctx.base_head.as_deref(), ctx.git_dir.as_deref()) else {
        return Ok(Vec::new());
    };
    let env: [(&str, &Path); 2] = [("GIT_DIR", git_dir), ("GIT_WORK_TREE", worktree)];
    let deleted = crate::worktree_guard::git_string(
        worktree,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "--diff-filter=D",
            base,
        ],
        &env,
    )
    .map_err(|e| format!("the change's deleted files could not be listed: {e}"))?;
    Ok(deleted
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
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
fn package_relative(files: Vec<String>, dir: &str) -> Vec<String> {
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
    files.extend(deleted_files(worktree, ctx)?);
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
                Some(_) => node_modules_gap(worktree, &json)?,
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
        // A deleted (or renamed-away) path is not in `files`, and the crate or test it left can
        // break without being named — never narrow past it.
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
    "base-cache",
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
    /// first held to the repository's size cap, a shared lease taken, the leaves made.
    #[cfg(test)]
    pub(crate) fn prepare(worktree: &Path) -> std::io::Result<Self> {
        Self::prepare_for(worktree, None)
    }

    /// [`Self::prepare`] for the run `run_id` (keyed by run and tree, [`scratch_dir_for`]).
    pub(crate) fn prepare_for(worktree: &Path, run_id: Option<&str>) -> std::io::Result<Self> {
        Self::prepare_in(
            &check_cache_root()?,
            worktree,
            run_id,
            check_cache_cap_bytes(),
        )
    }

    /// [`Self::prepare_for`] against an explicit cache root and cap — the seam the cap tests use.
    pub(crate) fn prepare_in(
        cache: &Path,
        worktree: &Path,
        run_id: Option<&str>,
        cap: u64,
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
        if enforce_check_cache_cap(group, &root, cap).1 {
            // This run's own scratch alone outgrew the cap: clean it before the checks, unless
            // another floor of the run is building into it (then it is that floor's, for now).
            drop(lease);
            if let Ok(Some(_held)) = lock_leaf(&root, true) {
                match remove_leaf_dir(&root) {
                    Ok(()) => eprintln!(
                        "wicked-core: repo checks floor — this run's check scratch {} alone was \
                         over the {:.1} GB cap ({CHECK_CACHE_CAP_ENV}); removed it before the checks",
                        root.display(),
                        cap as f64 / 1e9
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
        match std::fs::symlink_metadata(&root) {
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "`{}` is not a real directory — the checks' scratch is never written \
                         through a link (refused, never followed)",
                        root.display()
                    ),
                ))
            }
            _ => {}
        }
        // The leaves too (adversarial review on #414, LOW): `create_dir_all` would follow a
        // symlink-to-directory at a leaf — one an earlier floor's repo-controlled check, which can
        // write the leaf, left behind. Refuse a link at any leaf before creating.
        for sub in SCRATCH_LEAVES {
            if let Ok(m) = std::fs::symlink_metadata(root.join(sub)) {
                if m.file_type().is_symlink() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "`{}` is a symlink — the checks' scratch leaves must be real \
                             directories (refused, never followed)",
                            root.join(sub).display()
                        ),
                    ));
                }
            }
        }
        for sub in SCRATCH_LEAVES {
            std::fs::create_dir_all(root.join(sub))?;
        }
        // The checks' `TMPDIR` lives OUTSIDE the worktree, short (core#489) — drawn last, so a
        // refusal above leaves nothing behind under the system temp dir.
        let tmp = PrivateTmp::create()?;
        Ok(Self {
            root,
            tmp,
            lease: Some(lease),
        })
    }

    /// The scratch's leaf directory — the boundary's third write root.
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
    fn apply_env(&self, cmd: &mut Command, tree: Tree) {
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
    std::env::var(CHECK_CACHE_CAP_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_CHECK_CACHE_CAP_GB)
        .saturating_mul(1_000_000_000)
}

/// 12 hex of `sha256(bytes)`.
fn hex12(bytes: &[u8]) -> String {
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

/// The cap pass's lock, `<group>/.cap.lock`: one pass per repository at a time, so two floors
/// starting together never scan and evict against each other (one would read a leaf the other is
/// mid-way through deleting as "in use" and evict more than the cap needs).
const CAP_PASS_LOCK: &str = ".cap.lock";

/// Hold the repository group `group` to `cap` bytes before a floor runs over `own` (the floor
/// already holds `own`'s lease). Orphaned leaves ([`orphan_path`]) no floor holds go first,
/// whatever the cap. Over the cap, the OTHER run leaves go least recently used first
/// until it fits, skipping any leaf a floor holds a lease on. The floor's own leaf is never
/// removed here; the second value says whether it ALONE is over the cap (one run outgrew it),
/// which [`CheckScratch::prepare_in`] then cleans. A leaf is never removed because another
/// active run is large. Every removal is logged; best-effort, never fails the floor.
pub(crate) fn enforce_check_cache_cap(group: &Path, own: &Path, cap: u64) -> (Vec<PathBuf>, bool) {
    let mut removed = Vec::new();
    let pass = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(group.join(CAP_PASS_LOCK));
    let Ok(_pass) = pass.and_then(|f| f.lock().map(|()| f)) else {
        return (removed, false);
    };
    let Ok(entries) = std::fs::read_dir(group) else {
        return (removed, false);
    };
    let mut own_bytes = 0u64;
    let mut others: Vec<(std::time::SystemTime, PathBuf, u64)> = Vec::new();
    for e in entries.flatten() {
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let leaf = e.path();
        // (core#1050) An orphaned leaf goes whatever the cap, once no floor holds it.
        if leaf != own && orphan_path(&leaf).exists() {
            if retire_leaf(&leaf) {
                eprintln!(
                    "wicked-core: repo checks floor — removed the orphaned check scratch {} \
                     (its run's worktree is gone)",
                    leaf.display()
                );
                removed.push(leaf);
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
        others.push((used, leaf, bytes));
    }
    let mut total: u64 = own_bytes + others.iter().map(|l| l.2).sum::<u64>();
    if total <= cap {
        return (removed, false);
    }
    others.sort_by_key(|l| l.0);
    let gb = |b: u64| b as f64 / 1e9;
    for (_, leaf, bytes) in others {
        if total <= cap {
            break;
        }
        match lock_leaf(&leaf, true) {
            // Gone since the scan (a run's reap): nothing to remove, nothing to log.
            Ok(Some(_held)) if std::fs::symlink_metadata(&leaf).is_err() => {
                total = total.saturating_sub(bytes);
            }
            Ok(Some(_held)) => match remove_leaf_dir(&leaf) {
                Ok(()) => {
                    eprintln!(
                        "wicked-core: repo checks floor — the check cache {} was {:.1} GB, over \
                         its {:.1} GB cap ({CHECK_CACHE_CAP_ENV}); removed {} ({:.1} GB)",
                        group.display(),
                        gb(total),
                        gb(cap),
                        leaf.display(),
                        gb(bytes)
                    );
                    total = total.saturating_sub(bytes);
                    removed.push(leaf);
                }
                Err(e) => eprintln!(
                    "wicked-core: repo checks floor — could not remove the over-cap check \
                     scratch {}: {e}",
                    leaf.display()
                ),
            },
            // A floor is building into it right now: it is not this cap's to take.
            Ok(None) => {}
            Err(e) => eprintln!(
                "wicked-core: repo checks floor — could not lock the check scratch {}: {e}",
                leaf.display()
            ),
        }
    }
    if total > cap && own_bytes <= cap {
        eprintln!(
            "wicked-core: repo checks floor — the check cache {} is {:.1} GB, over its {:.1} GB \
             cap ({CHECK_CACHE_CAP_ENV}), held by runs whose floors are running",
            group.display(),
            gb(total),
            gb(cap)
        );
    }
    (removed, own_bytes > cap)
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
pub fn run_floor(worktree: &Path, ctx: &FloorContext) -> RepoChecksReport {
    run_floor_rerun(worktree, ctx, None)
}

/// [`run_floor`] for an operator's re-run (core#469, [`FloorRerun`]): the same floor with the
/// check set and bounds the gate's choice asks for. `None` is the ordinary floor.
pub fn run_floor_rerun(
    worktree: &Path,
    ctx: &FloorContext,
    rerun: Option<&FloorRerun>,
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
    let sandbox = crate::validator::detect_floor_sandbox(&[
        worktree.to_path_buf(),
        scratch.tmp.path().to_path_buf(),
        scratch.root().to_path_buf(),
    ]);
    let sandbox = arm_probed(sandbox, floor_probe());
    let mut report = run_with_sandbox_ctx(worktree, sandbox, ctx, scratch, rerun);
    report.rerun = rerun.map(|r| r.mode);
    report
}

/// (core#678, codex r1) The floor arms only what the host's cached EXECUTION probe says arms: a
/// launcher on `PATH` whose probe failed here (bwrap with user namespaces disabled) is no boundary,
/// so the detection is downgraded and the operator's unsandboxed opt-in applies — exactly what
/// `hostBoundary().verifyFloor` predicts — instead of bwrap failing every check at run time.
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
        Ok(scratch) => run_with_sandbox_ctx(worktree, sandbox, &ctx, scratch, None),
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
                };
            }
        }
    }
    let detected = match detect_with(worktree, ctx) {
        Ok(d) => d,
        Err(e) => {
            return RepoChecksReport {
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
            }
        }
    };
    let (detected, waived) = match rerun {
        Some(r) => match apply_rerun(worktree, ctx, r, detected) {
            Ok(pair) => pair,
            Err(e) => {
                return RepoChecksReport {
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
                }
            }
        },
        None => (detected, Vec::new()),
    };
    let candidates = engine_generated_candidates(worktree, &detected);
    let env = scratch.env_record(&sandbox_level);
    // (core#817) Per package: a subdirectory package's own `baseline_diff: false` holds for it.
    // The root's opt-out covers the whole repo; a package's own opt-out covers the package.
    let baseline_diff = baseline_diff_enabled(worktree);
    let baseline_diff_for =
        |dir: &str| dir.is_empty() || baseline_diff_enabled(&worktree.join(dir));
    let mut checks = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = false;
    // (F-RC2-009) Never run a HEAD check beside a copy of the base — a crash mid-floor could have
    // left one; the export lives only between its creation and the caching of its result.
    // (core#1050) A base export that cannot be cleared (a linked or non-directory `tmp`, a link
    // at the export path) REFUSES the floor, fail-closed: no check runs, and none may pass beside
    // a scratch the floor could not make safe.
    let mut scratch_error = remove_base_export(worktree, &scratch)
        .err()
        .map(|e| format!("the checks' scratch could not be made safe: {e}"));
    for check in &detected {
        if failed || scratch_error.is_some() {
            skipped.push(check.name.clone());
            continue;
        }
        let mut run = run_one(worktree, check, &sandbox, &scratch, Tree::Head);
        // BASELINE-DIFF (F-RC2-009): a FAILURE (not a timeout, not a spawn failure, not the
        // install — those say nothing about the base) is compared against the run base before it
        // may deny. No known base ⇒ the failure denies as it always did.
        // A formatter check is excluded too (core#551): a formatting diff is deterministic and
        // never environmental, so "the base fails identically" must not excuse it.
        if !run.passed()
            && run.spawn_error.is_none()
            && !run.timed_out
            && check.name != "install"
            && !is_formatter_check(&check.name)
        {
            match (ctx.base_head.as_deref(), ctx.git_dir.as_deref()) {
                (Some(head), _) if !(baseline_diff && baseline_diff_for(&check.dir)) => {
                    run.base = Some(Box::new(BaseRun {
                        head: head.to_string(),
                        cached: false,
                        run: None,
                        error: Some(format!(
                            "baseline diff disabled by `{CONFIG_PATH}` (baseline_diff: false)"
                        )),
                    }));
                }
                (Some(head), Some(git_dir)) => {
                    // This run's cache first; on a miss the base is exported, run, cached, and
                    // the export removed before the next HEAD check can see it.
                    let base_run = match BaseTree::cached(&scratch, head, check) {
                        Some(cached) => cached,
                        None => {
                            let base_run = match BaseTree::export(worktree, &scratch, git_dir, head)
                            {
                                Ok(t) => t.run_check(check, ctx, &sandbox, &scratch),
                                Err(e) => BaseRun {
                                    head: head.to_string(),
                                    cached: false,
                                    run: None,
                                    error: Some(e),
                                },
                            };
                            if let Err(e) = remove_base_export(worktree, &scratch) {
                                scratch_error = Some(format!(
                                    "the base export could not be removed before the next \
                                     check: {e}"
                                ));
                            }
                            base_run
                        }
                    };
                    classify(&mut run, base_run);
                    // (core#789) an identical environment crash, then (core#790) a load-class
                    // failure whatever the base did, then (core#553) a small head-only set.
                    if !rerun_env_crash(worktree, check, &mut run, &sandbox, &scratch)
                        && !rerun_load_class(worktree, check, &mut run, &sandbox, &scratch)
                    {
                        rerun_flakes(worktree, check, &mut run, &sandbox, &scratch);
                    }
                }
                _ => {}
            }
        }
        eprintln!(
            "wicked-core: repo checks floor — `{}` {} in {:.1}s (bound {}s){}",
            run.name,
            run.outcome(),
            run.duration_ms as f64 / 1000.0,
            run.bound_s,
            run.classification
                .as_deref()
                .map(|c| format!(" — {c}"))
                .unwrap_or_default()
        );
        failed = run.denies();
        checks.push(run);
    }
    let claim = judge_claim(ctx, &checks);
    // An engine-written file (absent at detection, present now) is the checks' own side effect —
    // remove it so the guard's final comparison sees the tree the seat left. Never a symlink.
    let mut engine_writes_removed = Vec::new();
    for rel in candidates {
        let p = worktree.join(&rel);
        if let Ok(m) = std::fs::symlink_metadata(&p) {
            if m.is_file() && std::fs::remove_file(&p).is_ok() {
                engine_writes_removed.push(rel);
            }
        }
    }
    let notes: Vec<String> = [
        (ctx.stage == FloorStage::Verify)
            .then(|| e2e_coverage_note(worktree, ctx, &detected, &checks, &waived))
            .flatten(),
        package_coverage_note(worktree, ctx, &detected),
    ]
    .into_iter()
    .flatten()
    .collect();
    let coverage_note = (!notes.is_empty()).then(|| notes.join("; "));
    if let Some(e) = &scratch_error {
        eprintln!("wicked-core: repo checks floor — refused: {e}");
    }
    RepoChecksReport {
        passed: !failed && scratch_error.is_none(),
        detected,
        checks,
        skipped,
        detect_error: scratch_error,
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
    }
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
    worktree: &Path,
    ctx: &FloorContext,
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
    let mut paths = touched_files(worktree, ctx).ok()?;
    paths.extend(deleted_files(worktree, ctx).ok()?);
    let mut e2e: Vec<String> = paths.into_iter().filter(|p| is_e2e_path(p)).collect();
    e2e.sort();
    e2e.dedup();
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
    /// Every blob entry written: `(mode, blob id, path)` — what [`Self::hold_to`] re-checks.
    entries: Vec<(String, String, PathBuf)>,
    /// Every symlink entry's target as written (unix).
    links: Vec<(PathBuf, Vec<u8>)>,
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
        links: Vec::new(),
    };
    // `--no-checkout` (codex r1 on the slice): a checkout runs the repository's `post-checkout`
    // hook and its configured smudge filters OUTSIDE the floor's sandbox. The worktree is made
    // empty, its files are written from the tree's own blobs (no filter, no attribute), and its
    // index is set with `read-tree`, which touches no file. Every git call here runs with an
    // engine-owned EMPTY hooks dir (codex r2): `read-tree` alone fires `post-index-change`, and no
    // repository hook may run outside the floor's sandbox.
    let no_hooks_dir = parent.join("no-hooks");
    std::fs::create_dir_all(&no_hooks_dir)
        .map_err(|e| format!("the floor's empty hooks dir could not be created: {e}"))?;
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
    (checkout.entries, checkout.links) = write_tree_blobs(worktree, git_dir, tree, &dir)
        .map_err(|e| format!("the guarded tree {tree} could not be written for the floor: {e}"))?;
    crate::worktree_guard::git(&dir, &["-c", &no_hooks, "read-tree", &commit], &[])
        .map_err(|e| format!("the floor checkout's index could not be set: {e}"))?;
    Ok(checkout)
}

/// Write every entry of `tree` into `dir` from its blob — `ls-tree -r -z` then ONE
/// `cat-file --batch` — so nothing repository-controlled runs (no checkout, no filter driver, no
/// `export-ignore`). Regular files keep their executable bit; a symlink entry becomes a symlink
/// (unix) or a plain file holding its target; a submodule becomes an empty directory. A path that
/// is absolute or climbs out of `dir` is refused.
#[allow(clippy::type_complexity)]
fn write_tree_blobs(
    worktree: &Path,
    git_dir: &Path,
    tree: &str,
    dir: &Path,
) -> Result<(Vec<(String, String, PathBuf)>, Vec<(PathBuf, Vec<u8>)>), String> {
    use std::io::{BufRead, Read, Write};
    let env: [(&str, &Path); 1] = [("GIT_DIR", git_dir)];
    let listing = crate::worktree_guard::git(worktree, &["ls-tree", "-r", "-z", tree], &env)
        .map_err(|e| e.to_string())?;
    let mut entries: Vec<(String, String, PathBuf)> = Vec::new();
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
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(format!("the tree holds an unsafe path {path:?}"));
        }
        entries.push((mode, sha, dir.join(rel)));
    }
    let blobs: Vec<&(String, String, PathBuf)> =
        entries.iter().filter(|(m, _, _)| m != "160000").collect();
    // spawn-audit: hardened — git plumbing reading the run's own object store.
    let mut cat = Command::new("git");
    cat.hardened()
        .args(["cat-file", "--batch"])
        .current_dir(worktree)
        .env("GIT_DIR", git_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cat.spawn().map_err(|e| format!("git cat-file: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("git cat-file has no stdin")?;
    let shas: Vec<String> = blobs.iter().map(|(_, sha, _)| sha.clone()).collect();
    let feeder = std::thread::spawn(move || {
        for sha in shas {
            if writeln!(stdin, "{sha}").is_err() {
                break;
            }
        }
    });
    let mut out = std::io::BufReader::new(child.stdout.take().ok_or("git cat-file has no stdout")?);
    // (codex r3) A symlink entry's TARGET, as written — what `hold_to` compares it against.
    #[cfg_attr(not(unix), allow(unused_mut))] // only a unix checkout writes symlinks
    let mut links: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    for (mode, sha, path) in &blobs {
        let mut header = String::new();
        out.read_line(&mut header).map_err(|e| e.to_string())?;
        let size: usize = header
            .split_whitespace()
            .nth(2)
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| format!("git cat-file answered {header:?} for {sha}"))?;
        let mut body = vec![0u8; size];
        out.read_exact(&mut body).map_err(|e| e.to_string())?;
        let mut nl = [0u8; 1];
        out.read_exact(&mut nl).map_err(|e| e.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // Never write through a link: no component under `dir`, nor the entry itself, may be one.
        let mut at = dir.to_path_buf();
        for c in path
            .strip_prefix(dir)
            .map_err(|e| e.to_string())?
            .components()
        {
            at.push(c);
            if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(format!("{} is a symlink in the checkout", at.display()));
            }
        }
        match mode.as_str() {
            #[cfg(unix)]
            "120000" => {
                use std::os::unix::ffi::OsStrExt;
                let target = std::ffi::OsStr::from_bytes(&body);
                std::os::unix::fs::symlink(target, path).map_err(|e| e.to_string())?;
                links.push((path.clone(), body.clone()));
            }
            _ => {
                std::fs::write(path, &body).map_err(|e| e.to_string())?;
                #[cfg(unix)]
                if mode == "100755" {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
                        .map_err(|e| e.to_string())?;
                }
            }
        }
    }
    let _ = feeder.join();
    let _ = child.wait();
    for (_, _, path) in entries.iter().filter(|(m, _, _)| m == "160000") {
        std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
    }
    Ok((
        entries
            .into_iter()
            .filter(|(m, _, _)| m != "160000")
            .collect(),
        links,
    ))
}

impl FloorCheckout {
    /// (codex r2) The checks ran IN the checkout: a check that rewrote tracked content there would
    /// otherwise pass against code that is not the tree that ships. After the floor, the checkout
    /// must still hash to the guarded `tree` (the engine scratch excluded); a difference fails the
    /// report, naming it.
    pub(crate) fn hold_to(&self, tree: &str, report: &mut RepoChecksReport) {
        let why = match self.changed_entries() {
            Ok(changed) if changed.is_empty() => return,
            Ok(changed) => format!(
                "the checks changed the tracked content they ran on ({}{}) — a verify floor \
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

    /// The tree entries whose content no longer matches the blob written: re-hashed with
    /// `hash-object --no-filters` (no repository filter runs); a symlink entry must still be a
    /// symlink. Untracked files a check created are not entries and are not judged — only what
    /// ships is.
    fn changed_entries(&self) -> Result<Vec<String>, String> {
        let rel = |p: &Path| p.strip_prefix(&self.dir).unwrap_or(p).display().to_string();
        let mut changed = Vec::new();
        let mut files: Vec<&(String, String, PathBuf)> = Vec::new();
        for e in &self.entries {
            let (mode, _, path) = e;
            match std::fs::symlink_metadata(path) {
                Err(_) => changed.push(rel(path)),
                Ok(m) if mode == "120000" => {
                    // A symlink stays a symlink, to the SAME target (codex r3).
                    #[cfg(unix)]
                    let same = m.file_type().is_symlink()
                        && self.links.iter().any(|(p, want)| {
                            use std::os::unix::ffi::OsStrExt;
                            p == path
                                && std::fs::read_link(path)
                                    .is_ok_and(|t| t.as_os_str().as_bytes() == want.as_slice())
                        });
                    #[cfg(not(unix))]
                    let same = m.is_file();
                    if !same {
                        changed.push(rel(path));
                    }
                }
                Ok(m) if !m.is_file() => changed.push(rel(path)),
                Ok(m) => {
                    // The executable bit is part of what ships (codex r3).
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let exec = m.permissions().mode() & 0o100 != 0; // git keys on the OWNER execute bit
                        if exec != (mode == "100755") {
                            changed.push(rel(path));
                            continue;
                        }
                    }
                    let _ = m;
                    files.push(e);
                }
            }
        }
        // Paths as ARGUMENTS, in chunks (codex r3: a newline in a name splits `--stdin-paths`).
        for chunk in files.chunks(256) {
            let mut args: Vec<String> = vec![
                "hash-object".to_string(),
                "--no-filters".to_string(),
                "--".to_string(),
            ];
            args.extend(
                chunk
                    .iter()
                    .map(|(_, _, p)| p.to_string_lossy().into_owned()),
            );
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let env: [(&str, &Path); 1] = [("GIT_DIR", self.git_dir.as_path())];
            let text = crate::worktree_guard::git_string(&self.worktree, &refs, &env)
                .map_err(|e| e.to_string())?;
            let shas: Vec<&str> = text.lines().collect();
            if shas.len() != chunk.len() {
                return Err(format!(
                    "git hash-object answered {} of {} paths",
                    shas.len(),
                    chunk.len()
                ));
            }
            for ((_, sha, path), now) in chunk.iter().zip(shas) {
                if sha != now {
                    changed.push(rel(path));
                }
            }
        }
        Ok(changed)
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
    }
}

/// The run base, exported into the checks' scratch for the baseline diff — plain files, no git
/// metadata, no nested worktree: `git read-tree` into a scratch index + `git checkout-index
/// --prefix`, both through the PINNED git dir (never the worktree's own `.git` file, which the
/// seat could have redirected). Lives at `<worktree>/tmp/` [`BASE_EXPORT_SUBDIR`] — inside the OS
/// write boundary, outside the guard's snapshot — so the sandbox wrapper armed for the worktree
/// covers the base run too. It stays INSIDE the worktree when the rest of the check scratch moved
/// to the engine's check cache: a base check that calls git (a `build.rs` running `git rev-parse`)
/// discovers the enclosing worktree's repository from there, exactly as it did from
/// `tmp/wicked-checks/base`; from the cache it found none and failed before its tests ran, a head
/// failure the base did not share, read as a regression (codex on #1050). The export is SOURCE
/// only (the base's build output goes to the check scratch's `cargo-target/base`) and it is
/// REMOVED as soon as its check result is cached ([`remove_base_export`]), and at the start of
/// every floor: it is a full copy of the repo tree inside the worktree, and a HEAD check that
/// globs from the worktree root (vitest's default include, a broad `tsconfig`, eslint without
/// ignores) would otherwise run over the base's files too. The run pays for a base check once
/// through the `base-cache` (in the check scratch), never through a persisted export.
struct BaseTree {
    dir: PathBuf,
    head: String,
}

/// The base export's directory name under the worktree's engine scratch (`tmp/`).
pub const BASE_EXPORT_SUBDIR: &str = "wicked-floor-base";
/// Where the whole check scratch lived before it moved to the engine's check cache
/// (`tmp/wicked-checks`). Nothing is written there; [`remove_base_export`] still removes a base
/// export an older engine left in it.
const LEGACY_SCRATCH_SUBDIR: &str = "wicked-checks";

/// `<worktree>/tmp/` [`BASE_EXPORT_SUBDIR`]. The engine scratch root is repo-adjacent territory:
/// a checkout or a seat could make `tmp` a SYMLINK out of the worktree, and the export (written by
/// the engine, outside any sandbox) would follow it — so a `tmp` that is a link, or not a
/// directory, is refused (adversarial review on #414), never followed.
fn base_export_dir(worktree: &Path) -> Result<PathBuf, String> {
    let tmp = worktree.join(crate::worktree_guard::ENGINE_SCRATCH_DIR);
    match std::fs::symlink_metadata(&tmp) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => Err(format!(
            "`{}` is not a real directory — the base export is never written through a link",
            tmp.display()
        )),
        Ok(_) => Ok(tmp.join(BASE_EXPORT_SUBDIR)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(tmp.join(BASE_EXPORT_SUBDIR)),
        Err(e) => Err(format!("`{}` cannot be inspected: {e}", tmp.display())),
    }
}

/// Remove the base export (`<worktree>/tmp/` [`BASE_EXPORT_SUBDIR`] and its scratch index) if
/// present. A symlink at the export path is refused, never followed. Called at the start of every
/// floor, before an export and as soon as a base check's result is cached, so no HEAD check ever
/// runs beside a copy of the base.
fn remove_base_export(worktree: &Path, scratch: &CheckScratch) -> Result<(), String> {
    let _ = std::fs::remove_file(scratch.root.join("base.idx"));
    let dir = base_export_dir(worktree)?;
    // (codex on #1050) An engine from before the scratch moved left its export at
    // `tmp/wicked-checks/base` when it died mid-baseline; it removed that at every floor's start,
    // and so does this one — or a HEAD check globbing from the root runs over the old base. Only
    // that export: the rest of a legacy `tmp/wicked-checks` is not this floor's to judge. Never
    // through a link (`tmp` was checked above; `wicked-checks` here; `base` is unlinked, not
    // followed).
    // Only "not found" is absence (codex on #1050): any other error inspecting a path (a `tmp`
    // without search permission) is an export the floor could not clear, and refuses it.
    let legacy = dir.with_file_name(LEGACY_SCRATCH_SUBDIR);
    match std::fs::symlink_metadata(&legacy) {
        Ok(m) if m.is_dir() => remove_leaf_dir(&legacy.join("base"))
            .map_err(|e| format!("the legacy base export could not be removed: {e}"))?,
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("`{}` cannot be inspected: {e}", legacy.display())),
    }
    match std::fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("`{}` cannot be inspected: {e}", dir.display())),
        Ok(m) if m.file_type().is_symlink() => Err(format!(
            "`{}` is a symlink (the base export is never written through a link)",
            dir.display()
        )),
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(&dir)
            .map_err(|e| format!("the base export could not be removed: {e}")),
        Ok(_) => std::fs::remove_file(&dir)
            .map_err(|e| format!("the base export could not be removed: {e}")),
    }
}

/// A cached base run with the floor policy it ran under ([`BaseTree::cached`]).
#[derive(Debug, Serialize, Deserialize)]
struct BaseCacheRecord {
    floor_policy: u32,
    run: CheckRun,
}

impl BaseTree {
    fn cache_path(scratch: &CheckScratch, head: &str, check: &RepoCheck) -> PathBuf {
        // (core#817) A subdirectory package's check is its own entry (`dir` with `/` flattened).
        // Hex-encoded, so the key is injective (`a/b` and `a_b` never share an entry).
        let dir = if check.dir.is_empty() {
            String::new()
        } else {
            let hex: String = check.dir.bytes().map(|b| format!("{b:02x}")).collect();
            format!("d{hex}--")
        };
        scratch.root.join("base-cache").join(format!(
            "{}-{dir}{}.json",
            &head[..head.len().min(12)],
            check.name
        ))
    }

    /// This run's cached result of `check` on `head`, when an earlier floor of the run paid for it
    /// under the SAME floor policy ([`FLOOR_POLICY_VERSION`]).
    fn cached(scratch: &CheckScratch, head: &str, check: &RepoCheck) -> Option<BaseRun> {
        let raw = std::fs::read_to_string(Self::cache_path(scratch, head, check)).ok()?;
        let record = serde_json::from_str::<BaseCacheRecord>(&raw).ok()?;
        if record.floor_policy != FLOOR_POLICY_VERSION {
            return None;
        }
        let run = record.run;
        // The base runs the head's argv, and it can change within a run (a targeted command with
        // the files a rework touches — `{files}`, the derived `-p <crate>` — or a reworked
        // `.wicked/checks.json`): a cached run of ANOTHER argv is not this comparison.
        if run.argv != check.argv {
            return None;
        }
        Some(BaseRun {
            head: head.to_string(),
            cached: true,
            run: Some(run),
            error: None,
        })
    }

    fn export(
        worktree: &Path,
        scratch: &CheckScratch,
        git_dir: &Path,
        head: &str,
    ) -> Result<Self, String> {
        let dir = base_export_dir(worktree)?;
        // Always a fresh export: a stale one is removed, a link refused — never followed.
        remove_base_export(worktree, scratch)?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("the base export directory could not be created: {e}"))?;
        let idx = scratch.root.join("base.idx");
        let _ = std::fs::remove_file(&idx);
        let env: [(&str, &Path); 3] = [
            ("GIT_DIR", git_dir),
            ("GIT_WORK_TREE", worktree),
            ("GIT_INDEX_FILE", idx.as_path()),
        ];
        let prefix = format!("--prefix={}/", dir.to_string_lossy());
        let result =
            crate::worktree_guard::git(worktree, &["read-tree", head], &env).and_then(|_| {
                crate::worktree_guard::git(worktree, &["checkout-index", "-a", "-f", &prefix], &env)
            });
        let _ = std::fs::remove_file(&idx);
        result.map_err(|e| {
            format!(
                "the run base {} could not be exported for the baseline diff: {e}",
                &head[..head.len().min(10)]
            )
        })?;
        Ok(Self {
            dir,
            head: head.to_string(),
        })
    }

    /// Run `check` on the base with the HEAD's argv, once the base declares a check of that name
    /// (a baseline diff compares one command on two trees: a change that edits the command — say
    /// adds `--no-fail-fast` — would otherwise read the base's shorter failure list as the head's
    /// regressions; a targeted command is the same change-scoped set on both trees) — and cache
    /// the result under `base-cache/<head>-<name>.json` for the rest of the
    /// run ([`Self::cached`]). The base's install step runs first when its tree needs
    /// provisioning (the same frozen, scripts-off install the head got, out of the same scratch
    /// cache).
    fn run_check(
        &self,
        check: &RepoCheck,
        ctx: &FloorContext,
        sandbox: &WorkerSandbox,
        scratch: &CheckScratch,
    ) -> BaseRun {
        let cache = Self::cache_path(scratch, &self.head, check);
        let fail = |e: String| BaseRun {
            head: self.head.clone(),
            cached: false,
            run: None,
            error: Some(e),
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
        if check.name != "test_targeted" && !detected.iter().any(|c| c.name == check.name) {
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
            let r = run_one(&self.dir, &install, sandbox, scratch, Tree::Base);
            if !r.passed() {
                return fail(format!(
                    "the base's dependency install did not pass ({})",
                    r.summary()
                ));
            }
        }
        let run = run_one(&self.dir, &target, sandbox, scratch, Tree::Base);
        let _ = std::fs::create_dir_all(scratch.root.join("base-cache"));
        let record = BaseCacheRecord {
            floor_policy: FLOOR_POLICY_VERSION,
            run,
        };
        if let Ok(json) = serde_json::to_string(&record) {
            let _ = std::fs::write(&cache, json);
        }
        let run = record.run;
        BaseRun {
            head: self.head.clone(),
            cached: false,
            run: Some(run),
            error: None,
        }
    }
}

/// Classify a failed head check against its base run and attach both (F-RC2-009).
///
/// * base passed ⇒ `regression` (every head failure is head-only);
/// * base did not finish or could not run ⇒ no classification (denies, fail-closed);
/// * both failed with identifiers on both sides ⇒ set arithmetic over the ids ([`match_base`]: a
///   libtest id never matches): any head-only identifier ⇒ `regression` (the shared ones listed
///   as `pre_existing`); otherwise (head ⊆ base, equal sets included) ⇒ `pre_existing_in_sandbox`
///   (the fact the comparison establishes);
/// * both failed with NO identifiers on either side ⇒ `unclassified_red_base` (denies, #538);
/// * identifiers on ONE side only ⇒ `regression` (cannot be compared — fail-closed).
fn classify(head: &mut CheckRun, base: BaseRun) {
    use std::collections::BTreeSet;
    let verdict = match &base.run {
        None => None,
        Some(b) if b.passed() => {
            head.regressions = head.failure_ids.clone();
            Some(REGRESSION)
        }
        Some(b) if b.timed_out || b.spawn_error.is_some() => None,
        Some(b) => {
            let head_ids: BTreeSet<&str> = head.failure_ids.iter().map(String::as_str).collect();
            let base_ids: BTreeSet<&str> = b.failure_ids.iter().map(String::as_str).collect();
            match (head_ids.is_empty(), base_ids.is_empty()) {
                // (#538) Both sides red with zero IDs — the runner's output format is unknown;
                // cannot determine if pre-existing or a regression. Fail closed.
                (true, true) => Some(UNCLASSIFIED_RED_BASE),
                (false, false) => {
                    let m = match_base(&head_ids, &base_ids);
                    head.pre_existing = m.pairs.iter().map(|(h, _)| h.to_string()).collect();
                    head.regressions = head_ids
                        .iter()
                        .filter(|h| !m.pairs.iter().any(|(p, _)| p == *h))
                        .map(|s| s.to_string())
                        .collect();
                    head.ambiguous = m.ambiguous.iter().map(|s| s.to_string()).collect();
                    if !head.regressions.is_empty() {
                        Some(REGRESSION)
                    } else {
                        Some(PRE_EXISTING_IN_SANDBOX)
                    }
                }
                _ => Some(REGRESSION),
            }
        }
    };
    head.classification = verdict.map(str::to_string);
    head.base = Some(Box::new(base));
}

/// How the head's failure ids met the base's ([`match_base`]).
#[derive(Debug, Default)]
struct BaseMatch<'a> {
    /// (head id, the base id it is) — the head failures the base shares.
    pairs: Vec<(&'a str, &'a str)>,
    /// Head libtest ids whose test name the base also fails: never matched (a libtest id names no
    /// test binary) — they stay regressions, and say why.
    ambiguous: Vec<&'a str>,
}

/// Match head failure ids to base failure ids. Other runners' ids (path-qualified) match on
/// equality. A libtest id NEVER matches (core#1060): `test shared` red in crate A on the base and
/// in crate B on the head are the same string, and nothing the floor can read — every line of
/// the output is text the change's tests can print — says which binary either came from. The head
/// id stays a regression (fail-closed) and, when the base fails that name too, is listed as
/// ambiguous: said, not guessed. [`FAILURE_IDS_TRUNCATED`] and every [`UNATTRIBUTED_FAILURE`]
/// never match either.
fn match_base<'a>(
    head: &std::collections::BTreeSet<&'a str>,
    base: &std::collections::BTreeSet<&'a str>,
) -> BaseMatch<'a> {
    let mut m = BaseMatch::default();
    for &h in head {
        if never_matched(h) {
            continue;
        }
        match libtest_name(h) {
            Some(name) => {
                if base.iter().any(|b| libtest_name(b) == Some(name)) {
                    m.ambiguous.push(h);
                }
            }
            None if base.contains(h) => m.pairs.push((h, h)),
            None => {}
        }
    }
    m
}

/// (core#553) The argv that re-runs ONE libtest id alone: the check's own `cargo test`, unchanged,
/// plus `-- --exact <name>` (after any test arguments it already passes) — it runs every test of
/// that name in every binary, so the id clears only when ALL of them pass. (core#766) A vitest id (`FAIL <file> > suite > name`)
/// re-runs as its FILE alone — the check's own `vitest` invocation (through `npx`, `pnpm exec`,
/// `yarn`, or the binary) cut back to `vitest run <file>` plus any `--config`/`--root` it carried;
/// a vitest test name is not addressable by exact match, the file is, and the issue asks for
/// "that file only". `None` for any other runner or id shape — the floor then does not re-run and
/// the classification stands.
fn isolated_rerun_argv(argv: &[String], id: &str) -> Option<(Vec<String>, String)> {
    if let Some(file) = vitest_file_of(id) {
        if !rerun_file_ok(file) {
            return None;
        }
        return vitest_file_rerun_argv(argv, file).map(|a| (a, id.to_string()));
    }
    let name = libtest_name(id)?;
    // A name that reads as a libtest flag (`--nocapture` drops the filter), or carries whitespace
    // or a control character, is no `--exact` filter: not re-run, it stays a regression.
    if !rerunnable_name(name) {
        return None;
    }
    let (cargo_args, test_args) = understood_cargo_test(argv)?;
    let mut out: Vec<String> = cargo_args.to_vec();
    out.push("--".into());
    out.extend(test_args.iter().cloned());
    out.push("--exact".into());
    out.push(name.to_string());
    Some((out, name.to_string()))
}

/// (core#1060) A `cargo [+toolchain] test` argv the floor understands element by element, split
/// at `--` into (cargo arguments, test-binary arguments), or `None`. Only such a check is re-run
/// alone or confirmed: the floor appends `--exact` (and `--skip`), and anything it does not know —
/// a positional TESTNAME (`cargo test foo`: `--exact` turns it into a name no test has, and a
/// confirmation of 0 tests exits 0), a `--skip` or `--exact` of its own, `--ignored` /
/// `--include-ignored`, an unknown flag, a second `--` — could make that select something else.
/// The check then stays a regression.
fn understood_cargo_test(argv: &[String]) -> Option<(&[String], &[String])> {
    let sub = cargo_test_sub(argv)?;
    /// Before `--`: package selection, features, profile, target, lockfile, jobs, fail-fast and
    /// colour flags.
    const CARGO_BARE: &[&str] = &[
        "--workspace",
        "--all",
        "--all-features",
        "--no-default-features",
        "--release",
        "--locked",
        "--frozen",
        "--offline",
        "--no-fail-fast",
    ];
    const CARGO_VALUED: &[&str] = &[
        "-p",
        "--package",
        "--exclude",
        "-F",
        "--features",
        "--profile",
        "--target",
        "-j",
        "--jobs",
        "--color",
    ];
    /// After `--`: thread count, output capture and colour. (`-q`/`--quiet` is not on it: a quiet
    /// re-run never prints `test X ... ok`, the witness an isolated re-run needs.)
    const TEST_BARE: &[&str] = &["--nocapture", "--show-output"];
    const TEST_VALUED: &[&str] = &["--test-threads", "--color"];
    fn allowed(args: &[String], bare: &[&str], valued: &[&str]) -> bool {
        let mut i = 0;
        while let Some(a) = args.get(i).map(String::as_str) {
            i += 1;
            if bare.contains(&a) {
                continue;
            }
            if valued.contains(&a) {
                match args.get(i) {
                    Some(v) if !v.is_empty() && !v.starts_with('-') => i += 1,
                    _ => return false,
                }
                continue;
            }
            let joined = a
                .split_once('=')
                .is_some_and(|(k, v)| k.starts_with("--") && valued.contains(&k) && !v.is_empty());
            // `-pfoo`, `-Ffeat`, `-j8`: a short valued flag with its value attached.
            let short = a.len() > 2
                && !a.starts_with("--")
                && valued.iter().any(|f| f.len() == 2 && a.starts_with(f));
            if !joined && !short {
                return false;
            }
        }
        true
    }
    let split = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    let (cargo_args, test_args) = (&argv[..split], argv.get(split + 1..).unwrap_or(&[]));
    (allowed(&cargo_args[sub + 1..], CARGO_BARE, CARGO_VALUED)
        && allowed(test_args, TEST_BARE, TEST_VALUED))
    .then_some((cargo_args, test_args))
}

/// The index of `test` in a `cargo [+toolchain] test …` argv, or `None` when it is not one.
fn cargo_test_sub(argv: &[String]) -> Option<usize> {
    let file = Path::new(argv.first()?).file_name()?.to_str()?;
    if file != "cargo" && file != "cargo.exe" {
        return None;
    }
    let sub = if argv.get(1)?.starts_with('+') { 2 } else { 1 };
    (argv.get(sub)? == "test").then_some(sub)
}

/// (core#781) The runner behind a package-manager script check — `npm test`, `npm run <s>`,
/// `pnpm [run] <s>`, `yarn [run] <s>` — read from the worktree's `package.json`: the script as
/// `npx --no-install <tokens…>` when it is ONE plain `vitest` command (its first token `vitest`,
/// no shell operator, quote, comment, glob, substitution or variable), else `None` (a script the floor cannot
/// see through is not re-run, as before).
fn script_runner_argv(worktree: &Path, argv: &[String]) -> Option<Vec<String>> {
    let pm = Path::new(argv.first()?).file_stem()?.to_str()?;
    if !matches!(pm, "npm" | "pnpm" | "yarn") {
        return None;
    }
    let script = match argv.get(1..)? {
        [t] if t == "test" => "test",
        [r, name] if r == "run" || r == "run-script" => name.as_str(),
        [name] if pm != "npm" => name.as_str(),
        _ => return None,
    };
    let pkg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(worktree.join("package.json")).ok()?).ok()?;
    let body = pkg.get("scripts")?.get(script)?.as_str()?.trim();
    if body.chars().any(|c| "&|;<>`$\"'(){}*?#~!\\\n".contains(c)) {
        return None;
    }
    let tokens: Vec<String> = body.split_whitespace().map(String::from).collect();
    if tokens.first().map(String::as_str) != Some("vitest") {
        return None;
    }
    let mut out = vec!["npx".to_string(), "--no-install".to_string()];
    out.extend(tokens);
    Some(out)
}

/// (core#766) The test file a vitest failure id names: `FAIL tests/x.test.ts > suite > name` →
/// `tests/x.test.ts`. The ` > ` separator is what makes it vitest's (go's `FAIL TestX` and a
/// file-level `FAIL tests/x.test.ts [ tests/x.test.ts ]` import failure have none and are not
/// re-run). The caller checks the runner.
fn vitest_file_of(id: &str) -> Option<&str> {
    let rest = id.strip_prefix("FAIL ")?;
    let (file, _) = rest.split_once(" > ")?;
    let file = file.trim();
    (!file.is_empty() && !file.contains(char::is_whitespace) && file.contains('.')).then_some(file)
}

/// (core#766) The check's own `vitest` invocation cut back to one file: everything up to and
/// including the `vitest` token (`npx --no-install vitest`, `pnpm exec vitest`, `yarn vitest`,
/// `node_modules/.bin/vitest`), then `run <file>`, then the original's environment selection —
/// `--config` / `--root` / `--dir` / `--project` / `--workspace` (both spellings; `--project`
/// may repeat) — so the file re-runs under the SAME config and project, not a different one.
/// `--changed <base>`, `related`, its file list and every other flag are dropped: the re-run is
/// the one file, not the one selection. `None` when the argv has no `vitest` token (`npm run
/// test` hides the runner behind a script).
fn vitest_file_rerun_argv(argv: &[String], file: &str) -> Option<Vec<String>> {
    vitest_files_rerun_argv(argv, &[file])
}

/// [`vitest_file_rerun_argv`] for a SET of files (core#790): `vitest run <f1> <f2> …` plus the
/// original's environment selection.
fn vitest_files_rerun_argv(argv: &[String], files: &[&str]) -> Option<Vec<String>> {
    let at = argv.iter().position(|a| {
        Path::new(a)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "vitest" || n == "vitest.mjs" || n == "vitest.js")
    })?;
    let mut out: Vec<String> = argv[..=at].to_vec();
    out.push("run".into());
    out.extend(files.iter().map(|f| f.to_string()));
    let mut rest = argv[at + 1..].iter();
    while let Some(a) = rest.next() {
        if matches!(
            a.as_str(),
            "--config" | "-c" | "--root" | "-r" | "--dir" | "--project" | "--workspace" | "-w"
        ) {
            if let Some(v) = rest.next() {
                out.push(a.clone());
                out.push(v.clone());
            }
        } else if [
            "--config=",
            "--root=",
            "--dir=",
            "--project=",
            "--workspace=",
        ]
        .iter()
        .any(|k| a.starts_with(k))
        {
            out.push(a.clone());
        }
    }
    Some(out)
}

/// (core#766) Does a vitest output line witness the id `FAIL <file> > suite > name` passing?
/// Either the default reporter's per-file line with EVERY test passed — ` ✓ <file> (N tests)
/// Nms`, a parenthetical with no `|` sub-count (`(3 tests | 1 skipped)` may have skipped the
/// very test and is not a witness) — or the verbose reporter's per-test line for THIS id —
/// ` ✓ <file> > suite > name[ Nms]`, the whole id, not a prefix of it (`> a > b` is not `> a`)
/// and not another test of the same file. Another file's line is never it.
fn vitest_pass_line_witnesses(s: &str, id: &str) -> bool {
    let (Some(rest), Some(body), Some(file)) = (
        s.strip_prefix('✓'),
        id.strip_prefix("FAIL "),
        vitest_file_of(id),
    ) else {
        return false;
    };
    let rest = rest.trim();
    if let Some(after) = rest.strip_prefix(body) {
        // The id ends here: nothing after it, or a SPACE then the duration — `> a1 5ms` is
        // another test, not `> a` with a timing suffix.
        if after.is_empty()
            || (after.starts_with(' ')
                && after.trim_start().starts_with(|c: char| c.is_ascii_digit()))
        {
            return true;
        }
    }
    match rest.strip_prefix(file).map(str::trim_start) {
        Some(after) if after.starts_with('(') => {
            let inner = &after[1..after.find(')').unwrap_or(after.len())];
            !inner.contains('|') && inner.contains("test")
        }
        _ => false,
    }
}

/// (core#553) Is the host oversubscribed — its 1-min load above its logical CPU count? The one
/// condition under which a head-only failure that passes alone is read as a load flake.
pub(crate) fn oversubscribed(load1: Option<f64>, cpus: usize) -> bool {
    matches!(load1, Some(l) if l.is_finite() && cpus > 0 && l > cpus as f64)
}

/// (core#553) Re-check each head-only failure of a `regression` ONCE, alone, serially, in the same
/// worktree, when the host is oversubscribed as the failure is read ([`recheck_alone`]). On a host
/// with headroom a failure that passes alone is as likely an order-dependent regression the
/// change introduced — it stays a regression, and nothing is re-run.
fn rerun_flakes(
    worktree: &Path,
    check: &RepoCheck,
    run: &mut CheckRun,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
) {
    if run.classification.as_deref() != Some(REGRESSION) || run.regressions.is_empty() {
        return;
    }
    let (load1, cpus) = host_load();
    if !oversubscribed(load1, cpus) {
        return;
    }
    // (core#781) `npm run test` hides its runner behind a script: when the script is a plain
    // `vitest …` command the re-run goes through that runner (`npx --no-install vitest …`).
    let runner_argv = script_runner_argv(&worktree.join(&check.dir), &check.argv);
    recheck_alone(
        check,
        run,
        runner_argv.as_deref(),
        Duration::from_secs(FLAKE_RERUN_BUDGET_S),
        &mut |one, names, cap| {
            run_one_capped(worktree, one, sandbox, scratch, Tree::Head, names, cap)
        },
    );
}

/// One re-run: the check to run, the names it must print passing and its HARD bound — what the
/// isolated re-run budget has left, never stretched by load — or `None` for the check's own
/// load-scaled bound (the confirmation run, core#1060) → the run and the watched names it printed
/// passing.
type IsolatedRerun<'a> =
    dyn FnMut(&RepoCheck, &[String], Option<Duration>) -> (CheckRun, Vec<String>) + 'a;

/// The re-checks themselves, with the re-run injected (the seam the tests drive). Each head-only
/// failure is re-run once, alone: a libtest id by its `--exact` name (in every binary), a vitest
/// id as its FILE (two ids of one file share one re-run). A failure PASSES alone only when the
/// re-run exits 0 AND the runner printed it passing (a filter that matched nothing is not a
/// pass): it is "flaky (passed alone)" and does not deny (`flaky`). One that fails again stays a
/// regression. At most [`MAX_FLAKE_RERUN`] failures and [`FLAKE_RERUN_BUDGET_S`] of re-runs (the
/// sum of their recorded durations; each re-run is HARD-bounded by what is left, the load factor
/// cannot stretch it); a set over the cap, failures the budget does not reach and
/// ids the runner cannot narrow stay regressions, counted in `not_rechecked`. When EVERY
/// head-only failure passed alone the check is `flaky_under_load`.
fn recheck_alone(
    check: &RepoCheck,
    run: &mut CheckRun,
    runner_argv: Option<&[String]>,
    budget: Duration,
    rerun: &mut IsolatedRerun<'_>,
) {
    // One re-run attempt per check, from any path; never for a truncated set (agy on #1060).
    if rerun_refused(run) {
        return;
    }
    let head_only = run.regressions.clone();
    // A failure the output proves but does not name cannot be re-run alone: only the
    // confirmation run below answers for it (core#1060).
    let named: Vec<String> = head_only
        .iter()
        .filter(|id| !never_matched(id))
        .cloned()
        .collect();
    if named.len() > MAX_FLAKE_RERUN {
        run.not_rechecked = named.len();
        eprintln!(
            "wicked-core: repo checks floor — {} head-only failures exceed the {MAX_FLAKE_RERUN} \
             the floor re-checks alone; none re-run, all stay regressions",
            named.len()
        );
        return;
    }
    // (argv, the names the runner must print passing, the ids they stand for), in order.
    let mut groups: Vec<(Vec<String>, Vec<String>, Vec<String>)> = Vec::new();
    let mut not_rechecked = 0usize;
    for id in &named {
        let plan = isolated_rerun_argv(&check.argv, id)
            .or_else(|| runner_argv.and_then(|a| isolated_rerun_argv(a, id)));
        match plan {
            Some((argv, name)) => match groups.iter_mut().find(|(a, _, _)| *a == argv) {
                Some((_, names, ids)) => {
                    names.push(name);
                    ids.push(id.clone());
                }
                None => groups.push((argv, vec![name], vec![id.clone()])),
            },
            None => not_rechecked += 1,
        }
    }
    let mut spent = Duration::ZERO;
    let mut flaky: Vec<String> = Vec::new();
    for (argv, names, ids) in groups {
        let Some(left) = budget
            .checked_sub(spent)
            .filter(|d| *d >= Duration::from_secs(1))
        else {
            not_rechecked += ids.len();
            continue;
        };
        let label = names.join("`, `");
        let one = RepoCheck {
            name: check.name.clone(),
            argv,
            source: format!("isolated re-run of `{label}` (core#553)"),
            timeout_s: check.timeout_s,
            dir: check.dir.clone(),
        };
        let (r, passed) = rerun(&one, &names, Some(left));
        spent += Duration::from_millis(r.duration_ms);
        let ok = r.passed() && names.iter().all(|n| passed.iter().any(|p| p == n));
        eprintln!(
            "wicked-core: repo checks floor — isolated re-run of `{label}` {} ({:.1}s{})",
            if ok {
                "passed: flaky (passed alone)"
            } else {
                "failed again: a regression"
            },
            r.duration_ms as f64 / 1000.0,
            r.bound_note
                .as_deref()
                .map(|n| format!(", bound {n}"))
                .unwrap_or_default()
        );
        run.reruns.push(r);
        if ok {
            flaky.extend(ids);
        }
    }
    run.not_rechecked = not_rechecked;
    // A failure that passed alone is no longer kept as a regression, ambiguous or not.
    run.ambiguous.retain(|id| !flaky.contains(id));
    let all_passed = !flaky.is_empty() && flaky.len() == named.len();
    if all_passed {
        // (core#1060) Names passing alone say nothing of what the output never named (a binary
        // that crashed, a doctest the `--exact` filter never compiled): the check itself, its
        // flaky tests skipped, must exit 0 — the ground truth that nothing else failed. It runs
        // under the check's own load-scaled bound, not what the isolated re-runs left.
        let confirmed = match confirmation_argv(&check.argv, &flaky) {
            Some(argv) => {
                let one = RepoCheck {
                    name: check.name.clone(),
                    argv,
                    source: CONFIRMATION_SOURCE.into(),
                    timeout_s: check.timeout_s,
                    dir: check.dir.clone(),
                };
                let (r, _) = rerun(&one, &[], None);
                let clean = r.passed();
                eprintln!(
                    "wicked-core: repo checks floor — confirmation run with the flaky tests \
                     skipped {} ({:.1}s)",
                    if clean {
                        "passed"
                    } else {
                        "did not pass: a regression"
                    },
                    r.duration_ms as f64 / 1000.0
                );
                run.reruns.push(r);
                clean
            }
            None => false,
        };
        if confirmed {
            // The clean run answered for the unattributed ids: the record names the flakes only.
            run.regressions.retain(|id| !never_matched(id));
            run.classification = Some(FLAKY_UNDER_LOAD.to_string());
            run.flaky = flaky;
            return;
        }
    }
    run.regressions.retain(|id| !flaky.contains(id));
    if all_passed && !run.regressions.iter().any(|r| r == NO_CLEAN_RUN) {
        run.regressions.push(NO_CLEAN_RUN.to_string());
    }
    run.flaky = flaky;
}

/// (core#1060) The confirmation run of a check whose named head-only failures all passed alone:
/// the check's own `cargo test` with each libtest name skipped exactly (`-- --skip <n> --exact`;
/// libtest applies `--exact` to `--skip`) — only for an argv the floor understands element by
/// element ([`understood_cargo_test`]) — or, for any other runner, which has no skip form here
/// (vitest, pytest), the check itself, unchanged. `None`: not confirmable, a regression.
fn confirmation_argv(argv: &[String], flaky: &[String]) -> Option<Vec<String>> {
    let names: Vec<&str> = flaky.iter().filter_map(|id| libtest_name(id)).collect();
    if names.len() != flaky.len() || names.is_empty() {
        return Some(argv.to_vec());
    }
    let (cargo_args, test_args) = understood_cargo_test(argv)?;
    let mut out = cargo_args.to_vec();
    out.push("--".into());
    out.extend(test_args.iter().cloned());
    for n in names {
        out.extend(["--skip".to_string(), n.to_string()]);
    }
    out.push("--exact".into());
    Some(out)
}

/// (core#790) The watch prefix that asks [`LineScan`] for a whole FILE's check-mark line.
const FILE_WATCH: &str = "file:";

/// (core#790) Does a vitest output line witness `file` passing as a whole — ` ✓ <file> (N
/// tests[ | M skipped]) Nms`, optionally behind a `|project|` label? A parenthetical that names a
/// failure is never a witness, nor is another file's line.
fn vitest_file_pass_line(s: &str, file: &str) -> bool {
    let Some(rest) = s.strip_prefix('✓') else {
        return false;
    };
    let mut rest = rest.trim_start();
    if let Some(after) = rest.strip_prefix('|') {
        match after.split_once('|') {
            Some((_, r)) => rest = r.trim_start(),
            None => return false,
        }
    }
    match rest.strip_prefix(file).map(str::trim_start) {
        Some(after) if after.starts_with('(') => {
            let inner = &after[1..after.find(')').unwrap_or(after.len())];
            inner.contains("test") && !inner.contains("fail")
        }
        _ => false,
    }
}

/// (core#789/#790) The environment-signal class one output line carries, as recorded on
/// [`CheckRun::env_signals`]: `load: …` for a vitest pool worker that could not start
/// ([`LOAD_SIGNATURES`]) or a per-test/hook timeout (`Test timed out in Nms`), `errors: N` for
/// vitest's own error count (`Errors  N errors`, `Vitest caught N unhandled errors`), `crash: …`
/// for a tool that died opening a dependency file (`EPERM`/`EACCES` naming `node_modules`).
fn env_signal_of_line(s: &str) -> Option<String> {
    let clip = |line: &str| -> String { line.chars().take(300).collect() };
    if LOAD_SIGNATURES.iter().any(|sig| s.contains(sig)) || per_test_timeout(s) {
        return Some(format!("load: {}", clip(s)));
    }
    if let Some(n) = vitest_error_count(s) {
        return Some(format!("errors: {n}"));
    }
    if (s.contains("EPERM: operation not permitted") || s.contains("EACCES: permission denied"))
        && s.contains("node_modules")
    {
        return Some(format!("crash: {}", clip(s)));
    }
    None
}

/// The vitest pool's worker-start failure (core#790): `[vitest-pool]: Failed to start <pool>
/// worker for test files <abs>, <abs>.` with its cause `Timeout waiting for worker to respond`.
const LOAD_SIGNATURES: &[&str] = &[
    "[vitest-pool]: Failed to start ",
    "Timeout waiting for worker to respond",
];

/// The prefix of the worker-start line that names its files.
const WORKER_START_FILES: &str = " worker for test files ";

/// `Test timed out in 30000ms` / `Hook timed out in 10000ms` — vitest's per-test bound.
fn per_test_timeout(s: &str) -> bool {
    ["Test timed out in ", "Hook timed out in "]
        .iter()
        .any(|k| {
            s.split_once(k).is_some_and(|(_, rest)| {
                let n = rest.bytes().take_while(u8::is_ascii_digit).count();
                n > 0 && rest[n..].starts_with("ms")
            })
        })
}

/// vitest's error count: the summary row `Errors  N error(s)` or the banner `Vitest caught N
/// unhandled error(s)`.
fn vitest_error_count(s: &str) -> Option<usize> {
    let rest = if let Some(r) = s.strip_prefix("Errors ") {
        r.trim_start()
    } else {
        s.split_once("Vitest caught ")?.1
    };
    let n = rest.bytes().take_while(u8::is_ascii_digit).count();
    let count = rest[..n].parse().ok()?;
    let tail = rest[n..].trim_start();
    (tail.starts_with("error") || tail.starts_with("unhandled error")).then_some(count)
}

/// (core#790) The files a worker-start signal names, relative to `worktree` (absolute on vitest's
/// line; a path outside the worktree is `Err` — the floor never re-runs a file it does not own).
fn worker_start_files(signal: &str, worktree: &Path) -> Result<Vec<String>, ()> {
    let Some((_, list)) = signal.split_once(WORKER_START_FILES) else {
        return Ok(Vec::new());
    };
    let roots: Vec<String> = [Some(worktree.to_path_buf()), worktree.canonicalize().ok()]
        .into_iter()
        .flatten()
        .map(|p| format!("{}/", p.to_string_lossy().trim_end_matches('/')))
        .collect();
    let list = list.trim_end().trim_end_matches('.');
    let mut out = Vec::new();
    for f in list.split(", ").map(str::trim).filter(|f| !f.is_empty()) {
        let rel = if f.starts_with('/') || Path::new(f).is_absolute() {
            match roots.iter().find_map(|r| f.strip_prefix(r.as_str())) {
                Some(rel) => rel,
                None => return Err(()),
            }
        } else {
            f
        };
        if !rerun_file_ok(rel) {
            return Err(());
        }
        out.push(rel.to_string());
    }
    Ok(out)
}

/// (core#790) May a file name ride a load-class re-run's argv? Relative, inside the tree (no
/// `..`), no whitespace, not option-shaped (a leading `-`), non-empty.
fn rerun_file_ok(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('-')
        && !rel.starts_with('/')
        && !rel.contains('\\')
        && !Path::new(rel).is_absolute()
        && !rel.split('/').any(|c| c == "..")
        && !rel.contains(char::is_whitespace)
}

/// The test file a vitest id names: `FAIL <file> > suite > name` or a file-level failure
/// `FAIL <file> [ <file> ]` (an import error, a worker that never started).
fn vitest_any_file_of(id: &str) -> Option<&str> {
    if let Some(f) = vitest_file_of(id) {
        return Some(f);
    }
    let rest = id.strip_prefix("FAIL ")?;
    let (file, bracket) = rest.split_once(" [ ")?;
    let file = file.trim();
    (bracket.trim_end().strip_suffix(']').map(str::trim) == Some(file)
        && !file.contains(char::is_whitespace)
        && file.contains('.'))
    .then_some(file)
}

/// (core#790) LOAD-CLASS RE-RUN. A vitest check whose output carries a load-class signal — a pool
/// worker that could not start, or a per-test timeout on an oversubscribed host — re-runs the
/// files its head-only failures name, plus every file a worker never started for, ONCE, in one
/// serial invocation (`--no-file-parallelism`), WHATEVER the base did: a red base is not evidence
/// of a regression when the head's red is load. Every file must print its own passing line and
/// the re-run must exit 0, then the check is `flaky_under_load`; anything else leaves the
/// classification as it was. Not applied when a head-only id is not a vitest id (a tsc or lint id
/// is never load), when the error count vitest printed exceeds the worker-start errors (an
/// unhandled error that is not a worker-start one is unexplained), or when the signal list was
/// truncated. Returns whether the re-run ran.
fn rerun_load_class(
    worktree: &Path,
    check: &RepoCheck,
    run: &mut CheckRun,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
) -> bool {
    use std::collections::BTreeSet;
    if rerun_refused(run)
        || !matches!(
            run.classification.as_deref(),
            Some(REGRESSION) | Some(UNCLASSIFIED_RED_BASE)
        )
        || run.env_signals.iter().any(|s| s == SIGNALS_TRUNCATED)
    {
        return false;
    }
    let loads: Vec<&str> = run
        .env_signals
        .iter()
        .filter_map(|s| s.strip_prefix("load: "))
        .collect();
    if loads.is_empty() {
        return false;
    }
    let worker_start = loads
        .iter()
        .filter(|l| l.contains(LOAD_SIGNATURES[0]))
        .count();
    // A per-test timeout alone is load only on an oversubscribed host (core#553's rule); a worker
    // that could not START in vitest's fixed window is load whatever the snapshot reads.
    let (load1, cpus) = host_load();
    if worker_start == 0 && !oversubscribed(load1, cpus) {
        return false;
    }
    let errors = run
        .env_signals
        .iter()
        .filter_map(|s| s.strip_prefix("errors: ")?.parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    if errors > worker_start {
        return false;
    }
    // The head-only failures: the regressions, or — one side without ids, or no ids at all —
    // every head id the base does not share.
    let base_ids: BTreeSet<&str> = run
        .base
        .as_deref()
        .and_then(|b| b.run.as_ref())
        .map(|b| b.failure_ids.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let head_only: Vec<String> = if run.regressions.is_empty() {
        run.failure_ids
            .iter()
            .filter(|id| !base_ids.contains(id.as_str()))
            .cloned()
            .collect()
    } else {
        run.regressions.clone()
    };
    let mut files: Vec<String> = Vec::new();
    for id in head_only.iter().filter(|id| !never_matched(id)) {
        let Some(f) = vitest_any_file_of(id).filter(|f| rerun_file_ok(f)) else {
            return false;
        };
        if !files.iter().any(|x| x == f) {
            files.push(f.to_string());
        }
    }
    for l in &loads {
        match worker_start_files(l, &worktree.join(&check.dir)) {
            Ok(fs) => {
                for f in fs {
                    if !files.contains(&f) {
                        files.push(f);
                    }
                }
            }
            Err(()) => return false,
        }
    }
    if files.is_empty() || files.len() > MAX_LOAD_RERUN_FILES {
        return false;
    }
    let refs: Vec<&str> = files.iter().map(String::as_str).collect();
    let argv = vitest_files_rerun_argv(&check.argv, &refs).or_else(|| {
        script_runner_argv(&worktree.join(&check.dir), &check.argv)
            .and_then(|a| vitest_files_rerun_argv(&a, &refs))
    });
    let Some(mut argv) = argv else {
        return false;
    };
    if files.len() > 1 {
        argv.push("--no-file-parallelism".into());
    }
    let watch: Vec<String> = files.iter().map(|f| format!("{FILE_WATCH}{f}")).collect();
    let one = RepoCheck {
        name: check.name.clone(),
        argv,
        source: format!(
            "load-class re-run of {} file{} (core#790)",
            files.len(),
            if files.len() == 1 { "" } else { "s" }
        ),
        timeout_s: check.timeout_s,
        dir: check.dir.clone(),
    };
    let (r, passed) = run_one_watching(worktree, &one, sandbox, scratch, Tree::Head, &watch);
    let ok = r.passed() && watch.iter().all(|w| passed.contains(w));
    eprintln!(
        "wicked-core: repo checks floor — load-class re-run of {} file(s) {} ({:.1}s)",
        files.len(),
        if ok { "passed" } else { "did not pass" },
        r.duration_ms as f64 / 1000.0
    );
    run.reruns.push(r);
    // (core#1060) The files passing say nothing of a failure the output never named: the check
    // itself (vitest has no skip form) must run clean once more.
    let ok = ok && {
        let confirm = RepoCheck {
            source: CONFIRMATION_SOURCE.into(),
            ..check.clone()
        };
        let (c, _) = run_one_watching(worktree, &confirm, sandbox, scratch, Tree::Head, &[]);
        let clean = c.passed();
        eprintln!(
            "wicked-core: repo checks floor — confirmation run of `{}` {}",
            check.name,
            if clean {
                "passed"
            } else {
                "did not pass: a regression"
            }
        );
        run.reruns.push(c);
        clean
    };
    if ok {
        // The clean run answered for the unattributed ids: the record names the files' failures.
        run.regressions.retain(|id| !never_matched(id));
        if run.regressions.is_empty() {
            let named: Vec<String> = head_only
                .into_iter()
                .filter(|id| !never_matched(id))
                .collect();
            run.regressions = if named.is_empty() {
                files.iter().map(|f| format!("FAIL {f} [ {f} ]")).collect()
            } else {
                named
            };
        }
        run.classification = Some(FLAKY_UNDER_LOAD.to_string());
        run.not_rechecked = 0;
    }
    true
}

/// (core#789) ENVIRONMENT CRASH. Head and base both failed with no failure ids
/// (`unclassified_red_base`) and BOTH outputs carry a `crash:` signal — the tool died opening a
/// dependency file (`EPERM`/`EACCES` under `node_modules`; an endpoint scanner denying opens under
/// load). Head and base must share the crash CLASS (the same errno — the file a scanner denies
/// differs run to run). The head check is re-run ONCE: a pass makes it `flaky_under_load` (the
/// crash is named); the same class again makes it `env_cannot_run` with the crash line on
/// `env_cannot_run` — never denying, so the floor goes on to the remaining checks; any other
/// outcome leaves it denying.
/// Returns whether the re-run ran.
fn rerun_env_crash(
    worktree: &Path,
    check: &RepoCheck,
    run: &mut CheckRun,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
) -> bool {
    let crash_of = |r: &CheckRun| -> Option<String> {
        r.env_signals
            .iter()
            .find(|s| s.starts_with("crash: "))
            .cloned()
    };
    if rerun_refused(run) || run.classification.as_deref() != Some(UNCLASSIFIED_RED_BASE) {
        return false;
    }
    let base_crash = run
        .base
        .as_deref()
        .and_then(|b| b.run.as_ref())
        .and_then(crash_of);
    let (Some(head_crash), Some(base_crash)) = (crash_of(run), base_crash) else {
        return false;
    };
    let class = crash_class(&head_crash);
    if class != crash_class(&base_crash) {
        return false;
    }
    let r = run_one(worktree, check, sandbox, scratch, Tree::Head);
    let again = (!r.passed() && r.failure_ids.is_empty() && !r.timed_out)
        .then(|| crash_of(&r))
        .flatten()
        .filter(|c| crash_class(c) == class);
    eprintln!(
        "wicked-core: repo checks floor — `{}` crashed on head and base ({head_crash}); re-run {}",
        check.name,
        if r.passed() {
            "passed"
        } else if again.is_some() {
            "crashed again"
        } else {
            "failed otherwise"
        }
    );
    if r.passed() {
        run.regressions = vec![head_crash];
        run.classification = Some(FLAKY_UNDER_LOAD.to_string());
    } else if let Some(c) = again {
        run.env_cannot_run = vec![c];
        run.classification = Some(ENV_CANNOT_RUN.to_string());
    }
    run.reruns.push(r);
    true
}

/// (core#789) The errno class of a `crash:` signal (`EPERM` / `EACCES`).
fn crash_class(signal: &str) -> &'static str {
    if signal.contains("EPERM: operation not permitted") {
        "EPERM"
    } else {
        "EACCES"
    }
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
    let failing = checks.iter().find(|c| !c.passed() && c.name != "install")?;
    let verdict = match failing.classification.as_deref() {
        // Every NAMED regression a libtest name the base fails too (core#1060: never matched; the
        // unattributed ids name nothing): the floor can neither confirm the claim nor reject it.
        Some(REGRESSION)
            if failing.regressions.iter().any(|r| !never_matched(r))
                && failing
                    .regressions
                    .iter()
                    .filter(|r| !never_matched(r))
                    .all(|r| failing.ambiguous.contains(r)) =>
        {
            CLAIM_UNVERIFIED
        }
        Some(REGRESSION) => CLAIM_REJECTED,
        Some(PRE_EXISTING_IN_SANDBOX) | Some(FLOOR_ENV_MISMATCH) | Some(ENV_CANNOT_RUN) => {
            CLAIM_CONFIRMED
        }
        // The base passes it: a flake is not a pre-existing failure.
        Some(FLAKY_UNDER_LOAD) => CLAIM_REJECTED,
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

/// What one check's re-checks and head/base matching established beyond its verdict, one line
/// each: head-only failures that passed alone while others still deny, how many were not
/// re-checked, and head libtest ids the base fails by name too (never matched, core#1060).
/// Empty when there is nothing of the kind.
fn match_notes(c: &CheckRun) -> Vec<String> {
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let mut out = Vec::new();
    if !c.flaky.is_empty() && c.classification.as_deref() != Some(FLAKY_UNDER_LOAD) {
        out.push(format!(
            "`{}`: {} head-only failure{} passed re-run alone on an oversubscribed host — flaky \
             (passed alone), not counted: {}",
            c.name,
            c.flaky.len(),
            plural(c.flaky.len()),
            c.flaky.join(", ")
        ));
    }
    if c.not_rechecked > 0 {
        out.push(format!(
            "`{}`: {} head-only failure{} not re-checked alone (more than {MAX_FLAKE_RERUN}, past \
             the {FLAKE_RERUN_BUDGET_S}s re-run budget, or a runner the floor cannot narrow to one \
             test) — still regressions",
            c.name,
            c.not_rechecked,
            plural(c.not_rechecked)
        ));
    }
    if !c.ambiguous.is_empty() {
        out.push(format!(
            "`{}`: {} could not be told apart from a same-named base failure (a libtest \
             failure names no test binary) — kept as regressions, not guessed; the operator's \
             floor override is the escape for a red the base shares",
            c.name,
            c.ambiguous.join(", ")
        ));
    }
    out
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

/// The effective bound for a check right now: `base` × the load factor, with the note that
/// explains it — then held to `cap` when one is given — a HARD ceiling the load
/// factor cannot stretch (the isolated re-runs' remaining budget, [`FLAKE_RERUN_BUDGET_S`]).
fn bound_under(
    base: Duration,
    load1: Option<f64>,
    cpus: usize,
    cap: Option<Duration>,
) -> (Duration, Option<String>) {
    let factor = load_factor(load1, cpus);
    let scaled = Duration::from_secs_f64(base.as_secs_f64() * factor);
    let note = load1.map(|l| {
        format!(
            "{}s × {factor:.2} (1-min load {l:.1} / {cpus} cpus)",
            base.as_secs()
        )
    });
    match cap {
        Some(c) if scaled > c => (
            c,
            Some(format!(
                "{}, held to {}s by the re-run budget",
                note.unwrap_or_else(|| format!("{}s", base.as_secs())),
                c.as_secs()
            )),
        ),
        _ => (scaled, note),
    }
}

/// A bounded tail buffer: keeps the last [`TAIL_BYTES`] bytes of a stream.
/// How long to wait for a check's stdout/stderr to reach EOF after the process group is dead. A
/// detached descendant (`setsid`, a Node `detached` spawn with inherited stdio) can hold the pipe
/// open forever; the drain is DETACHED after this and the tail read so far is what gets reported
/// (adversarial review on #414 — an unbounded join wedged the verify unit).
const DRAIN_CAP: Duration = Duration::from_secs(5);

/// A stdout/stderr drain: the bounded tail accumulates in `buf` (shared, so a drain that never
/// reaches EOF still yields what it saw), the [`LineScan`] of complete lines accumulates in `scan`,
/// `done` fires at EOF.
struct Drain {
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    scan: std::sync::Arc<std::sync::Mutex<LineScan>>,
    done: std::sync::mpsc::Receiver<()>,
}

impl Drain {
    /// The tail read so far and the line scan, waiting at most `cap` for EOF; on timeout the
    /// reader thread is left to die with the pipe (it holds only its own handles).
    fn finish(self, cap: Duration) -> (Vec<u8>, LineScan) {
        let _ = self.done.recv_timeout(cap);
        let mut tail = std::mem::take(&mut *self.buf.lock().unwrap_or_else(|p| p.into_inner()));
        if tail.len() > TAIL_BYTES {
            let cut = tail.len() - TAIL_BYTES;
            tail.drain(..cut);
        }
        let scan = std::mem::take(&mut *self.scan.lock().unwrap_or_else(|p| p.into_inner()));
        (tail, scan)
    }
}

/// The line-level state of one output stream's failure scan: failure ids in order of first sight,
/// the watched names seen passing and the environment signals.
///
/// No line is read as STRUCTURE (core#1060): libtest's framing (`running N tests`, `failures:`,
/// sections, result lines) and cargo's (`Running …`, `to rerun pass …`) are text the creator's
/// tests can print, and 13 review rounds forged them. Every `test x ... FAILED` line anywhere in
/// the output is a failure, recorded as plain `test x`: none is dropped, and none names a binary
/// ([`match_base`] never matches a libtest id).
#[derive(Debug, Default)]
struct LineScan {
    eslint_file: Option<String>,
    /// Failure ids in order of first sight, at most [`MAX_FAILURE_IDS`].
    ids: Vec<String>,
    /// More failure ids than [`MAX_FAILURE_IDS`] were seen: [`qualify`] adds
    /// [`FAILURE_IDS_TRUNCATED`].
    truncated: bool,
    /// libtest names seen PASSING (`test name ... ok`) among `watch` — the isolated re-run's witness.
    passed: Vec<String>,
    watch: Vec<String>,
    /// (core#789/#790) Environment signals in order of sight ([`env_signal_of_line`]).
    signals: Vec<String>,
}

impl LineScan {
    fn watching(watch: &[String]) -> Self {
        Self {
            watch: watch.to_vec(),
            ..Self::default()
        }
    }

    fn line(&mut self, raw: &str) {
        // (codex on #1060) Colored output (`--color=always`, `CARGO_TERM_COLOR`) reads as plain.
        let plain = strip_ansi(raw);
        let line = plain.as_ref();
        let s = line.trim();
        if let Some(sig) = env_signal_of_line(s) {
            let repeat = sig.starts_with("crash: ") && self.signals.contains(&sig);
            if !repeat {
                if self.signals.len() < MAX_ENV_SIGNALS {
                    self.signals.push(sig);
                } else if self.signals.last().map(String::as_str) != Some(SIGNALS_TRUNCATED) {
                    *self.signals.last_mut().expect("cap > 0") = SIGNALS_TRUNCATED.to_string();
                }
            }
        }
        // (core#1060) Ids and markers are read off the raw line as well as the stripped one — a
        // second reading can only ADD ids (`\x1b[31test new ... FAILED`: the stripper takes the
        // `t` as the sequence's final byte). A line carrying an ESC that mentions a failure token
        // yet yielded no id is itself a failure the floor cannot name.
        let escaped = raw.contains('\u{1b}');
        let mut yielded = false;
        if escaped {
            if let Some(kind) = unattributed_failure_of_line(raw.trim()) {
                self.add(format!("{UNATTRIBUTED_FAILURE}{kind}"));
                yielded = true;
            }
            let mut eslint = None;
            if let Some(id) = failure_id_of_line(raw, &mut eslint) {
                self.add(id);
                yielded = true;
            }
        }
        if let Some(kind) = unattributed_failure_of_line(s) {
            self.add(format!("{UNATTRIBUTED_FAILURE}{kind}"));
            yielded = true;
        }
        if let Some(id) = failure_id_of_line(line, &mut self.eslint_file) {
            self.add(id);
            yielded = true;
        }
        if escaped
            && !yielded
            && [raw, line]
                .iter()
                .any(|t| ESCAPED_FAILURE_TOKENS.iter().any(|k| t.contains(k)))
        {
            self.add(ESCAPED_FAILURE_LINE.to_string());
        }
        if !self.watch.is_empty() {
            if let Some(name) = s
                .strip_prefix("test ")
                .and_then(|r| r.strip_suffix(" ... ok"))
            {
                let name = name.trim();
                if self.watch.iter().any(|w| w == name) && !self.passed.iter().any(|p| p == name) {
                    self.passed.push(name.to_string());
                }
            } else if s.starts_with('✓') {
                // (core#766) A watched vitest id passes when its FILE's check-mark line is printed.
                // (core#790) A watched `file:<path>` passes on that file's own check-mark line.
                for w in &self.watch {
                    let hit = match w.strip_prefix(FILE_WATCH) {
                        Some(file) => vitest_file_pass_line(s, file),
                        None => vitest_pass_line_witnesses(s, w),
                    };
                    if hit && !self.passed.iter().any(|p| p == w) {
                        self.passed.push(w.clone());
                    }
                }
            }
        }
    }

    /// Keep a failure id (once), at most [`MAX_FAILURE_IDS`].
    fn add(&mut self, id: String) {
        if !self.ids.contains(&id) {
            if self.ids.len() < MAX_FAILURE_IDS {
                self.ids.push(id);
            } else {
                self.truncated = true;
            }
        }
    }
}

/// `line` without its COMPLETE ANSI escape sequences: CSI (`ESC [`, parameter bytes 0x30–0x3F,
/// intermediate bytes 0x20–0x2F, one final byte 0x40–0x7E), OSC (`ESC ]` … ended by BEL or `ESC \`)
/// and a two-byte `ESC <0x40–0x5F>`. An unterminated or malformed sequence is kept verbatim, with
/// the rest of the line (agy on #1060: swallowing to the end of the line dropped a FAILED line).
fn strip_ansi(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains('\u{1b}') {
        return std::borrow::Cow::Borrowed(line);
    }
    let c: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < c.len() {
        if c[i] == '\u{1b}' {
            if let Some(end) = escape_end(&c, i) {
                i = end;
                continue;
            }
        }
        out.push(c[i]);
        i += 1;
    }
    std::borrow::Cow::Owned(out)
}

/// The index just past the complete escape sequence that starts at `c[i]` (an ESC), or `None`.
fn escape_end(c: &[char], i: usize) -> Option<usize> {
    let within = |j: usize, lo: char, hi: char| c.get(j).is_some_and(|x| (lo..=hi).contains(x));
    match c.get(i + 1)? {
        '[' => {
            let mut j = i + 2;
            while within(j, '\u{30}', '\u{3f}') {
                j += 1;
            }
            while within(j, '\u{20}', '\u{2f}') {
                j += 1;
            }
            within(j, '\u{40}', '\u{7e}').then_some(j + 1)
        }
        ']' => {
            let mut j = i + 2;
            loop {
                match c.get(j)? {
                    '\u{7}' => return Some(j + 1),
                    '\u{1b}' => return (c.get(j + 1) == Some(&'\\')).then_some(j + 2),
                    _ => j += 1,
                }
            }
        }
        x if ('\u{40}'..='\u{5f}').contains(x) => Some(i + 2),
        _ => None,
    }
}

/// A libtest id's test name: `test a::b` → `a::b`. `None` for any other runner's id.
fn libtest_name(id: &str) -> Option<&str> {
    id.strip_prefix("test ")
}

/// A test name an `--exact` / `--skip` argument can carry: not empty, no leading `-` (it would
/// read as a libtest flag), no whitespace or control character.
fn rerunnable_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The kind of failure a line PROVES without naming it ([`UNATTRIBUTED_FAILURE`]), or `None`:
/// cargo's `process didn't exit successfully:` (a test binary killed by a signal or exiting other
/// than 101), cargo's `error: could not compile`, rustdoc's `Couldn't compile the test.`, a pytest
/// `ERROR` summary line (a collection or setup error), and a `FAIL <file>` line with no ` > `
/// (vitest's file-level failure — an import error — and jest's failing-file header).
fn unattributed_failure_of_line(s: &str) -> Option<&'static str> {
    if s.contains("process didn't exit successfully:") {
        return Some("a test process exited abnormally");
    }
    if s.starts_with("error: could not compile ") {
        return Some("a crate did not compile");
    }
    if s.starts_with("Couldn't compile the test.") {
        return Some("a doctest did not compile");
    }
    if s.starts_with("ERROR ") {
        return Some("pytest reported an error");
    }
    // A cargo run inside a wrapper (`sh -c 'cargo test'`): libtest's verdict, cargo's per-target
    // selector, libtest's `--quiet` failure line.
    if s.starts_with("test result: FAILED")
        || s.contains("failed, to rerun pass ")
        || s.ends_with(" --- FAILED")
    {
        return Some("cargo test failed");
    }
    if let Some(rest) = s.strip_prefix("FAIL ") {
        if !rest.trim().is_empty() && !rest.contains(" > ") {
            return Some("a test file failed as a whole");
        }
    }
    None
}

/// The scanned ids of one check run: the failure ids (stdout first, then stderr, at most
/// [`MAX_FAILURE_IDS`] plus [`FAILURE_IDS_TRUNCATED`] past it), the watched names seen passing,
/// and the environment signals.
fn qualify(out: LineScan, err: LineScan) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut truncated = out.truncated || err.truncated;
    let mut ids: Vec<String> = Vec::new();
    for id in out.ids.into_iter().chain(err.ids) {
        if !ids.contains(&id) {
            if ids.len() < MAX_FAILURE_IDS {
                ids.push(id);
            } else {
                truncated = true;
            }
        }
    }
    // A dropped id could have been the only regression: the check carries one nothing matches.
    if truncated {
        ids.push(FAILURE_IDS_TRUNCATED.to_string());
    }
    let mut passed = out.passed;
    for p in err.passed {
        if !passed.contains(&p) {
            passed.push(p);
        }
    }
    let mut signals = out.signals;
    for sig in err.signals {
        if signals.len() >= MAX_ENV_SIGNALS {
            if signals.last().map(String::as_str) != Some(SIGNALS_TRUNCATED) {
                *signals.last_mut().expect("cap > 0") = SIGNALS_TRUNCATED.to_string();
            }
            break;
        }
        if !(sig.starts_with("crash: ") && signals.contains(&sig)) {
            signals.push(sig);
        }
    }
    (ids, passed, signals)
}

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
    // cargo / libtest: `test path::name ... FAILED` — read from the FIRST `test ` on the line, so
    // text a test wrote to the same line without a newline (or an unterminated escape, kept
    // verbatim) can misname the id but never hide it (core#1060).
    if let Some(name) = s
        .strip_suffix(" ... FAILED")
        .and_then(|r| r.find("test ").map(|i| &r[i + 5..]))
    {
        return Some(format!("test {}", name.trim()));
    }
    // libtest `--quiet`: `path::name --- FAILED`
    if let Some(name) = s.strip_suffix(" --- FAILED") {
        let name = name.trim();
        if !name.is_empty() && !name.contains(char::is_whitespace) {
            return Some(format!("test {name}"));
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

fn drain_tail<R: Read + Send + 'static>(mut r: R, watch: &[String]) -> Drain {
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::with_capacity(TAIL_BYTES * 2)));
    let scan = std::sync::Arc::new(std::sync::Mutex::new(LineScan::watching(watch)));
    let (done_tx, done) = std::sync::mpsc::channel();
    let shared = std::sync::Arc::clone(&buf);
    let shared_scan = std::sync::Arc::clone(&scan);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        let mut pending: Vec<u8> = Vec::new();
        let scan = |line: &[u8]| {
            let line = String::from_utf8_lossy(line);
            shared_scan
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .line(&line);
        };
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    {
                        let mut tail = shared.lock().unwrap_or_else(|p| p.into_inner());
                        tail.extend_from_slice(&chunk[..n]);
                        if tail.len() > TAIL_BYTES * 2 {
                            let cut = tail.len() - TAIL_BYTES;
                            tail.drain(..cut);
                        }
                    }
                    // Line scanner: complete lines are judged, the partial one waits. A line
                    // longer than the tail buffer is judged truncated (identifiers are short).
                    pending.extend_from_slice(&chunk[..n]);
                    while let Some(nl) = pending.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = pending.drain(..=nl).collect();
                        scan(&line[..line.len() - 1]);
                    }
                    if pending.len() > TAIL_BYTES {
                        let cut = pending.len() - TAIL_BYTES;
                        pending.drain(..cut);
                    }
                }
            }
        }
        if !pending.is_empty() {
            scan(&pending);
        }
        let _ = done_tx.send(());
    });
    Drain { buf, scan, done }
}

fn lossy(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Run one check in `worktree` under its timeout, inside `sandbox`'s write boundary with the
/// isolated `scratch` homes (`tree` picks the cargo target leaf), capturing exit code + stream
/// tails. A LAUNCHER that fails to arm (`bwrap: …` / `sandbox-exec: …` as the first stderr line,
/// non-zero exit) is `could_not_run`, never the check's own `failed` (core#493).
pub(crate) fn run_one(
    worktree: &Path,
    check: &RepoCheck,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
    tree: Tree,
) -> CheckRun {
    run_one_watching(worktree, check, sandbox, scratch, tree, &[]).0
}

/// [`run_one`], also reporting which of the libtest names in `watch` the run printed as PASSING
/// (`test name ... ok`) — the witness an isolated re-run needs (core#553): exit 0 alone could be a
/// filter that matched nothing.
fn run_one_watching(
    worktree: &Path,
    check: &RepoCheck,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
    tree: Tree,
    watch: &[String],
) -> (CheckRun, Vec<String>) {
    run_one_capped(worktree, check, sandbox, scratch, tree, watch, None)
}

/// [`run_one_watching`] whose bound is held to `cap` whatever the host load ([`bound_under`]).
fn run_one_capped(
    worktree: &Path,
    check: &RepoCheck,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
    tree: Tree,
    watch: &[String],
    cap: Option<Duration>,
) -> (CheckRun, Vec<String>) {
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
        classification: None,
        pre_existing: Vec::new(),
        regressions: Vec::new(),
        base: None,
        env_cannot_run: Vec::new(),
        reruns: Vec::new(),
        flaky: Vec::new(),
        not_rechecked: 0,
        ambiguous: Vec::new(),
        env_signals: Vec::new(),
        load_band: None,
    };
    let (load1, cpus) = host_load();
    result.load_band = load_band(load1, cpus);
    // LOAD-AWARE BOUND (core#469): base × clamp(load1/ncpu, 1, 3), recorded on the run and
    // logged before the check starts so the observed duration can be read against it.
    let (timeout, bound_note) = bound_under(check.base_timeout(), load1, cpus, cap);
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
        return (result, Vec::new());
    };
    // Resolve on PATH the way `Command::new` will, so "not installed" is a legible spawn error
    // rather than a bare OS code.
    let Some(exe) = crate::validator::find_on_path(bin) else {
        result.spawn_error = Some(format!("`{bin}` is not on PATH"));
        result.duration_ms = started.elapsed().as_millis() as u64;
        return (result, Vec::new());
    };
    // `[<sandbox wrapper…>] <exe> <args…>` — an empty wrapper is the disclosed best-effort floor.
    let mut full: Vec<String> = sandbox.wrapper.clone();
    full.push(exe.to_string_lossy().into_owned());
    full.extend(check.argv[1..].iter().cloned());
    // spawn-audit: hardened — the repository's own check command, run in the run's worktree.
    let mut cmd = Command::new(&full[0]);
    cmd.hardened()
        .args(&full[1..])
        .current_dir(worktree.join(&check.dir))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scratch.apply_env(&mut cmd, tree);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group, so a timeout kills the whole tree (a test runner's workers included)
        // and never the daemon.
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            result.spawn_error = Some(e.to_string());
            result.duration_ms = started.elapsed().as_millis() as u64;
            return (result, Vec::new());
        }
    };
    let out_h = child.stdout.take().map(|r| drain_tail(r, watch));
    let err_h = child.stderr.take().map(|r| drain_tail(r, watch));
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
    // BOUNDED: the group is dead, but a detached descendant may still hold the pipe — take what
    // was read and move on rather than wait for an EOF that may never come.
    let (out_tail, out_scan) = out_h.map(|d| d.finish(DRAIN_CAP)).unwrap_or_default();
    let (err_tail, err_scan) = err_h.map(|d| d.finish(DRAIN_CAP)).unwrap_or_default();
    result.stdout_tail = lossy(out_tail);
    result.stderr_tail = lossy(err_tail);
    // The launcher's exit is not the repository's (core#493): bwrap that cannot `mkdir` a `--tmpfs`
    // destination dies BEFORE exec with `bwrap: Can't mkdir …` and exit 1 (so does a launcher
    // whose `execvp` of the check's binary fails) — the check never ran, so recording
    // `exit_code: 1` as its `failed` blames the repo for the jail. The ONE predicate
    // (`validator::launcher_failure`) reads the first stderr line; a hit is `could_not_run`
    // (denies, fail-closed, attribution honest). Never applied to a passing exit or a timeout.
    if !result.timed_out && result.exit_code != Some(0) {
        if let Some(msg) = crate::validator::launcher_failure(
            &sandbox.wrapper,
            result.stderr_tail.lines().next().unwrap_or(""),
        ) {
            result.spawn_error = Some(msg);
        }
    }
    // Identifiers from both streams (cargo prints its per-test lines on stdout, vitest on
    // stderr), de-duplicated, in order of first sight; libtest ids plain (core#1060).
    let (ids, passed, signals) = qualify(out_scan, err_scan);
    result.failure_ids = ids;
    result.env_signals = signals;
    // (core#1060) A failed `cargo test` is a failure whatever its output says — the floor's own
    // argv and exit code, no text: only a clean confirmation run's exit can answer for it.
    if !result.passed() && result.spawn_error.is_none() && cargo_test_sub(&check.argv).is_some() {
        let how = match result.exit_code {
            Some(c) => format!("exited {c}"),
            None if result.timed_out => "timed out".to_string(),
            None => "was killed by a signal".to_string(),
        };
        let id = format!("{UNATTRIBUTED_FAILURE}cargo test {how}");
        if !result.failure_ids.contains(&id) {
            result.failure_ids.push(id);
        }
    }
    result.duration_ms = started.elapsed().as_millis() as u64;
    (result, passed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validator::SandboxLevel;

    fn scratch(name: &str) -> PathBuf {
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
    fn sandbox_for(wt: &Path) -> WorkerSandbox {
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
                classification: None,
                pre_existing: Vec::new(),
                regressions: Vec::new(),
                base: None,
                env_cannot_run: Vec::new(),
                reruns: Vec::new(),
                flaky: Vec::new(),
                not_rechecked: 0,
                ambiguous: Vec::new(),
                env_signals: Vec::new(),
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
        let group = own.parent().unwrap().to_path_buf();
        assert_eq!(
            enforce_check_cache_cap(&group, &own, 10_000),
            (vec![], false),
            "under the cap"
        );
        // Two active floors (`busy` and this one) over the cap together: neither leaf is taken
        // for the other, and this floor's own leaf (under the cap alone) is not cleaned.
        let own_lease = lock_leaf(&own, false).unwrap().unwrap();
        own_lease.set_modified(ago(10)).unwrap();
        let (removed, own_over) = enforce_check_cache_cap(&group, &own, 1000);
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
        let (removed, _) = enforce_check_cache_cap(leaf_b.parent().unwrap(), &leaf_b, u64::MAX);
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
        let (removed, _) = enforce_check_cache_cap(leaf.parent().unwrap(), &own, 0);
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
                      exit 2; }; echo 'FAILED tests/t.py::test_shared - boom'; exit 101";
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
        assert_eq!(
            c.classification.as_deref(),
            Some(PRE_EXISTING_IN_SANDBOX),
            "{c:#?}"
        );
        assert!(report.passed, "{report:?}");
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
        remove_base_export(&wt, &scratch_dir).unwrap();
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
            remove_base_export(&wt, &scratch_dir).unwrap();
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

    fn git(repo: &Path, args: &[&str]) -> String {
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

    fn git_repo_with_commit(repo: &Path) -> String {
        git(repo, &["init", "-q"]);
        git(repo, &["config", "user.email", "t@example.invalid"]);
        git(repo, &["config", "user.name", "t"]);
        git(repo, &["config", "commit.gpgsign", "false"]);
        git(repo, &["config", "core.autocrlf", "false"]);
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-qm", "base"]);
        git(repo, &["rev-parse", "HEAD"])
    }

    fn names(checks: &[RepoCheck]) -> Vec<&str> {
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
        let (load1, cpus) = host_load();
        let (bound, note) = bound_under(check.base_timeout(), load1, cpus, None);
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

    /// (#538) Both base and head fail with no parseable failure identifiers → `unclassified_red_base`,
    /// which must deny (fail-closed) and never be treated as pre-existing.
    #[test]
    fn unclassified_red_base_denies_when_both_sides_produce_zero_ids() {
        let make_check = |exit: i32| CheckRun {
            name: "test".to_string(),
            argv: vec![],
            source: "test".to_string(),
            exit_code: Some(exit),
            timed_out: false,
            spawn_error: None,
            duration_ms: 0,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            bound_s: 0,
            bound_note: None,
            failure_ids: vec![],
            classification: None,
            pre_existing: vec![],
            regressions: vec![],
            base: None,
            env_cannot_run: Vec::new(),
            reruns: Vec::new(),
            flaky: Vec::new(),
            not_rechecked: 0,
            ambiguous: Vec::new(),
            env_signals: Vec::new(),
            load_band: None,
        };
        let make_base = |exit: i32| BaseRun {
            head: "abc1234567".to_string(),
            run: Some(make_check(exit)),
            cached: false,
            error: None,
        };

        // Same exit code on both sides — previously classified FLOOR_ENV_MISMATCH (which passed);
        // must now be UNCLASSIFIED_RED_BASE and must deny.
        let mut head = make_check(1);
        classify(&mut head, make_base(1));
        assert_eq!(
            head.classification.as_deref(),
            Some(UNCLASSIFIED_RED_BASE),
            "zero ids both sides (same exit code) must be unclassified_red_base, not floor_env_mismatch"
        );
        assert!(
            head.denies(),
            "unclassified_red_base must deny (fail-closed)"
        );

        // Different exit codes on both sides — previously classified REGRESSION; still deny.
        let mut head2 = make_check(2);
        classify(&mut head2, make_base(1));
        assert_eq!(
            head2.classification.as_deref(),
            Some(UNCLASSIFIED_RED_BASE),
            "zero ids both sides (different exit codes) must also be unclassified_red_base"
        );
        assert!(head2.denies());
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

    /// The baseline diff with real `cargo test` (core#1060): a libtest failure is never excused by
    /// the base, whatever it shares; the comparison is still run, cached and disclosed.
    #[test]
    fn baseline_diff_never_excuses_a_libtest_failure() {
        let repo = scratch("basediff");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"basediff_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\npath = \"src/lib.rs\"\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(repo.join(".gitignore"), "Cargo.lock\ntmp/\n").unwrap();
        let body = |shared: bool, other: bool, stable: bool| {
            let fn_body = |ok: bool| -> &'static str {
                if ok {
                    "{}"
                } else {
                    "{\n        assert!(false, \"BOOM\");\n    }"
                }
            };
            format!(
                "#[cfg(test)]\nmod t {{\n    #[test]\n    fn shared() {}\n    #[test]\n    \
                 fn other() {}\n    #[test]\n    fn stable() {}\n}}\n",
                fn_body(shared),
                fn_body(other),
                fn_body(stable)
            )
        };
        std::fs::write(repo.join("src/lib.rs"), body(false, false, true)).unwrap();
        let base = git_repo_with_commit(&repo);
        let ctx = FloorContext {
            stage: FloorStage::Creator,
            base_head: Some(base.clone()),
            git_dir: Some(repo.join(".git")),
            claim_text: Some(
                "Done. The failing test is pre-existing on main and unrelated to my change."
                    .to_string(),
            ),
            ..FloorContext::default()
        };

        // (1) `stable` newly fails beside the two failures the base shares. A libtest id never
        // matches the base (core#1060): all three are regressions, the shared two ambiguous.
        std::fs::write(repo.join("src/lib.rs"), body(false, false, false)).unwrap();
        let report = run_floor(&repo, &ctx);
        if report.sandbox_error.is_some() {
            eprintln!("repo_checks: no sandbox tool here — the baseline diff cannot run");
            return;
        }
        assert!(!report.passed, "{report:?}");
        assert_eq!(report.outcome(), "failed");
        assert_eq!(report.denial_source(), DENIAL_SOURCE);
        assert_eq!(
            report.checks[0].name, "cargo-fmt-check",
            "formatter passes on formatted code"
        );
        let c = &report.checks[1];
        // core#482: the creator floor of an unconfigured repo runs the touched crate only.
        assert_eq!(c.name, "test_targeted");
        assert_eq!(c.source, DERIVED_TARGETED_SOURCE);
        assert_eq!(c.classification.as_deref(), Some(REGRESSION), "{c:?}");
        // Every regression named, plus the failed cargo run's own ids (core#1060).
        let named = |v: &[String]| {
            v.iter()
                .filter(|i| !never_matched(i))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            named(&c.regressions),
            vec!["test t::other", "test t::shared", "test t::stable"]
        );
        assert!(c
            .regressions
            .contains(&format!("{UNATTRIBUTED_FAILURE}cargo test exited 101")));
        assert_eq!(c.ambiguous, vec!["test t::other", "test t::shared"]);
        assert!(c.pre_existing.is_empty());
        let b = c.base.as_deref().expect("the base run is attached");
        assert_eq!(b.head, base);
        assert!(!b.cached, "first comparison of this base: run, not cached");
        let br = b.run.as_ref().expect("the base ran");
        assert!(
            !br.passed() && br.exit_code.is_some_and(|e| e != 0),
            "{br:?}"
        );
        // First-seen order: cargo runs tests on several threads, so the two failures may be
        // reported in either order (the macOS CI runner reports `shared` first) — compare sorted.
        let mut base_ids = named(&br.failure_ids);
        base_ids.sort();
        assert_eq!(base_ids, vec!["test t::other", "test t::shared"]);
        assert_eq!(
            report.claim.as_ref().map(|c| c.verdict.as_str()),
            Some(CLAIM_REJECTED),
            "{:?}",
            report.claim
        );
        assert_eq!(
            report.claim.as_ref().map(|c| c.phrase.as_str()),
            Some("pre-existing")
        );
        let denial = report.denial_reason();
        assert!(
            denial.contains("[regression]")
                && denial
                    .contains("head-only failures test t::other, test t::shared, test t::stable")
                && denial.contains("test t::other, test t::shared could not be told apart")
                && denial.contains("REJECTED"),
            "{denial}"
        );
        let env = report
            .env
            .as_ref()
            .expect("the floor's env is on the record");
        assert!(env.home.ends_with("home") && env.network == "open" && !env.path.is_empty());
        let scratch_root = leaf_of(&repo);
        assert_no_worktree_scratch(&repo);
        assert!(
            !repo
                .join(crate::worktree_guard::ENGINE_SCRATCH_DIR)
                .join(BASE_EXPORT_SUBDIR)
                .exists(),
            "the base export is removed as soon as its result is cached — a HEAD check never \
             runs beside a copy of the base"
        );
        assert!(
            scratch_root
                .join("base-cache")
                .join(format!("{}-test_targeted.json", &base[..12]))
                .is_file(),
            "the base run is cached by base sha + check name"
        );

        // (2) Identical libtest failure sets STILL deny (core#1060, the accepted cost: a Rust
        // test the base fails too is never excused — the operator's floor override is the
        // escape). The creator's "pre-existing" claim is neither confirmed nor rejected. The tree
        // equals the base, so the full `cargo-test` runs (no derived set to narrow to).
        std::fs::write(repo.join("src/lib.rs"), body(false, false, true)).unwrap();
        let report = run_floor(&repo, &ctx);
        assert!(!report.passed, "{:?}", report.checks[1]);
        let c = &report.checks[1];
        assert_eq!(c.name, "cargo-test");
        assert_eq!(c.classification.as_deref(), Some(REGRESSION));
        assert_eq!(
            named(&c.regressions),
            vec!["test t::other", "test t::shared"]
        );
        assert_eq!(c.ambiguous, named(&c.regressions));
        assert!(c.pre_existing.is_empty() && c.denies());
        assert!(!report.env_mismatch());
        assert_eq!(
            report.claim.as_ref().map(|c| c.verdict.as_str()),
            Some(CLAIM_UNVERIFIED)
        );
        assert!(
            report.denial_reason().contains("floor override"),
            "{}",
            report.denial_reason()
        );

        // (3) The head fixes `other` and breaks nothing: `shared` still denies, against the base
        // run paid for once per run.
        std::fs::write(repo.join("src/lib.rs"), body(false, true, true)).unwrap();
        let report = run_floor(&repo, &ctx);
        assert!(!report.passed, "{:?}", report.checks[1]);
        let c = &report.checks[1];
        assert_eq!(named(&c.regressions), vec!["test t::shared"]);
        assert_eq!(c.ambiguous, vec!["test t::shared"]);
        assert!(
            c.base.as_deref().is_some_and(|b| b.cached),
            "the base run is paid for once per run"
        );

        // (4) Opt-out: the plain denial, with the reason the base was not compared.
        std::fs::create_dir_all(repo.join(".wicked")).unwrap();
        std::fs::write(repo.join(CONFIG_PATH), r#"{"baseline_diff":false}"#).unwrap();
        let report = run_floor(&repo, &ctx);
        assert!(!report.passed);
        let c = &report.checks[1];
        assert!(c.classification.is_none() && c.denies());
        assert!(
            c.base
                .as_deref()
                .and_then(|b| b.error.as_deref())
                .is_some_and(|e| e.contains("baseline_diff: false")),
            "{:?}",
            c.base
        );
        assert_eq!(
            report.claim.as_ref().map(|c| c.verdict.as_str()),
            Some(CLAIM_UNVERIFIED)
        );
        assert!(
            report.denial_reason().contains("could not be compared"),
            "{}",
            report.denial_reason()
        );
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
            classification: None,
            pre_existing: Vec::new(),
            regressions: Vec::new(),
            base: None,
            env_cannot_run: Vec::new(),
            reruns: Vec::new(),
            flaky: Vec::new(),
            not_rechecked: 0,
            ambiguous: Vec::new(),
            env_signals: Vec::new(),
            load_band: None,
        }
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

    fn scan_of(text: &str) -> LineScan {
        let mut scan = LineScan::default();
        for line in text.lines() {
            scan.line(line);
        }
        scan
    }

    /// core#1060: a libtest id never matches the base — plain, a stored record's qualified form,
    /// or `[?]`: `test shared` red in crate A on the base and in crate B on the head is the same
    /// string. It stays a regression, listed as ambiguous when the base fails that name, and the
    /// denial says so. Other runners' ids (path-qualified) still match by equality: an identical
    /// set is the FACT `pre_existing_in_sandbox`, never the inference `floor_env_mismatch`.
    #[test]
    fn a_libtest_id_never_matches_a_base_id_1060() {
        for (head_id, base_id) in [
            ("test shared", "test shared"),
            ("test shared [-p a --lib]", "test shared [-p a --lib]"),
            ("test shared [?]", "test shared [?]"),
        ] {
            let mut head = failed_run(&[head_id], &[head_id]);
            classify(&mut head, base_of(failed_run(&[base_id], &[base_id])));
            assert_eq!(
                head.classification.as_deref(),
                Some(REGRESSION),
                "{head_id}"
            );
            assert!(head.pre_existing.is_empty(), "{head_id}");
            assert_eq!(head.ambiguous, vec![head_id]);
            assert!(head.denies());
        }
        let mut head = failed_run(&["test a::x", "test a::new"], &[]);
        classify(&mut head, base_of(failed_run(&["test a::x"], &[])));
        assert_eq!(head.regressions, vec!["test a::new", "test a::x"]);
        assert_eq!(
            head.ambiguous,
            vec!["test a::x"],
            "the name-only coincidence is said"
        );
        let denial = report_of(vec![head], &[]).denial_reason();
        assert!(
            denial.contains("test a::x could not be told apart")
                && denial.contains("not guessed")
                && denial.contains("floor override"),
            "{denial}"
        );
        // Path-qualified ids of other runners match: identical sets pass, a shared pytest red
        // beside a libtest red still denies on the libtest one.
        let py = ["FAILED tests/a.py::test_x", "FAILED tests/b.py::test_y"];
        let mut head = failed_run(&py, &[]);
        classify(&mut head, base_of(failed_run(&py, &[])));
        assert_eq!(
            head.classification.as_deref(),
            Some(PRE_EXISTING_IN_SANDBOX)
        );
        assert!(!head.denies());
        assert!(!report_of(vec![head], &[]).env_mismatch());
        let mixed = ["FAILED tests/a.py::test_x", "test shared"];
        let mut head = failed_run(&mixed, &[]);
        classify(&mut head, base_of(failed_run(&mixed, &[])));
        assert_eq!(head.pre_existing, vec!["FAILED tests/a.py::test_x"]);
        assert_eq!(head.regressions, vec!["test shared"]);
        assert!(head.denies());
    }

    /// A `cargo test --workspace` check whose head-only failures are `ids`, as classify leaves it.
    fn regression_run(ids: &[&str]) -> (RepoCheck, CheckRun) {
        let check = RepoCheck {
            name: "cargo-test".to_string(),
            argv: s(&["cargo", "test", "--workspace", "--no-fail-fast"]),
            source: "Cargo.toml".to_string(),
            timeout_s: Some(1200),
            dir: String::new(),
        };
        let mut run = failed_run(ids, &[]);
        run.regressions = run.failure_ids.clone();
        run.classification = Some(REGRESSION.to_string());
        (check, run)
    }

    /// A stand-in re-run: `pass(name)` decides whether the one test passes alone; each takes
    /// `secs`. Records the base bound each re-run was handed.
    fn fake_rerun<'a>(
        pass: &'a dyn Fn(&str) -> bool,
        secs: u64,
        bounds: &'a mut Vec<u64>,
    ) -> impl FnMut(&RepoCheck, &[String], Option<Duration>) -> (CheckRun, Vec<String>) + 'a {
        move |one, names, cap| {
            assert_eq!(
                one.timeout_s,
                Some(1200),
                "the check's own base bound, capped separately"
            );
            // An isolated re-run is hard-bounded by the budget; the confirmation run is not.
            if let Some(cap) = cap {
                bounds.push(cap.as_secs());
            }
            let ok = names.iter().all(|n| pass(n));
            let mut r = failed_run(&[], &[]);
            r.argv = one.argv.clone();
            r.exit_code = Some(if ok { 0 } else { 101 });
            r.duration_ms = secs * 1000;
            (r, if ok { names.to_vec() } else { Vec::new() })
        }
    }

    /// Each head-only failure is re-run once, alone, serially: one that passes alone is "flaky
    /// (passed alone)" and does not deny; when every one does, the check is `flaky_under_load`.
    #[test]
    fn a_failure_that_passes_alone_is_flaky_and_does_not_block() {
        let ids: Vec<String> = (0..27).map(|i| format!("test t::load{i}")).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let (check, mut run) = regression_run(&refs);
        let mut bounds = Vec::new();
        let pass = |_: &str| true;
        recheck_alone(
            &check,
            &mut run,
            None,
            Duration::from_secs(FLAKE_RERUN_BUDGET_S),
            &mut fake_rerun(&pass, 10, &mut bounds),
        );
        assert_eq!(run.classification.as_deref(), Some(FLAKY_UNDER_LOAD));
        assert!(!run.denies());
        assert_eq!(
            run.reruns.len(),
            28,
            "27 head-only failures, 27 re-runs (the old bound was 2), then one confirmation run"
        );
        assert_eq!(run.flaky, ids);
        assert_eq!(run.not_rechecked, 0);
        assert!(run.reruns[0]
            .argv
            .ends_with(&s(&["--no-fail-fast", "--", "--exact", "t::load0"])));

        // A failure kept as ambiguous (one side's binary unknown) that passes alone is flaky,
        // not "kept as a regression" too.
        let (check, mut run) = regression_run(&["test t::x"]);
        run.ambiguous = run.regressions.clone();
        let mut bounds = Vec::new();
        recheck_alone(
            &check,
            &mut run,
            None,
            Duration::from_secs(FLAKE_RERUN_BUDGET_S),
            &mut fake_rerun(&pass, 10, &mut bounds),
        );
        assert_eq!(run.classification.as_deref(), Some(FLAKY_UNDER_LOAD));
        assert!(run.ambiguous.is_empty(), "{run:#?}");
        assert!(match_notes(&run).iter().all(|n| !n.contains("not guessed")));
    }

    /// One that fails again stays a regression (and denies); the ones beside it that passed alone
    /// are flaky, are not counted, and the denial names both.
    #[test]
    fn a_failure_that_fails_again_alone_stays_a_regression() {
        let (check, mut run) =
            regression_run(&["test t::flaky", "test t::broken", "test t::flaky2"]);
        let mut bounds = Vec::new();
        let pass = |n: &str| n != "t::broken";
        recheck_alone(
            &check,
            &mut run,
            None,
            Duration::from_secs(FLAKE_RERUN_BUDGET_S),
            &mut fake_rerun(&pass, 10, &mut bounds),
        );
        assert_eq!(run.classification.as_deref(), Some(REGRESSION));
        assert!(run.denies());
        assert_eq!(
            run.reruns.len(),
            3,
            "a failure again does not stop the others' re-checks"
        );
        assert_eq!(run.regressions, vec!["test t::broken"]);
        assert_eq!(run.flaky, vec!["test t::flaky", "test t::flaky2"]);
        let denial = report_of(vec![run], &[]).denial_reason();
        assert!(
            denial.contains("2 head-only failures passed re-run alone")
                && denial.contains("flaky (passed alone)")
                && !denial.contains("t::broken, test t::flaky"),
            "{denial}"
        );
    }

    /// The re-checks stop at the budget: failures it does not reach stay regressions, and the
    /// result says how many were not re-checked; each re-run's bound is cut to what is left. Over
    /// the cap, nothing is re-run and every failure is counted.
    #[test]
    fn a_spent_rerun_budget_is_reported_and_the_rest_stay_regressions() {
        let (check, mut run) =
            regression_run(&["test t::a", "test t::b", "test t::c", "test t::d"]);
        let mut bounds = Vec::new();
        let pass = |_: &str| true;
        recheck_alone(
            &check,
            &mut run,
            None,
            Duration::from_secs(900),
            &mut fake_rerun(&pass, 400, &mut bounds),
        );
        assert_eq!(
            bounds,
            vec![900, 500, 100],
            "each hard bound is what the budget has left"
        );
        // The hard bound holds under load: 900 s left at load 42 on 14 cpus is 900 s, not the
        // 2700 s the load factor alone would give (codex on #1060).
        let (b, note) = bound_under(
            Duration::from_secs(900),
            Some(42.0),
            14,
            Some(Duration::from_secs(900)),
        );
        assert_eq!(b, Duration::from_secs(900), "{note:?}");
        assert!(note.unwrap().contains("held to 900s by the re-run budget"));
        let (b, _) = bound_under(Duration::from_secs(900), Some(42.0), 14, None);
        assert_eq!(
            b,
            Duration::from_secs(2700),
            "an ordinary check still scales with load"
        );
        let (b, _) = bound_under(
            Duration::from_secs(100),
            Some(42.0),
            14,
            Some(Duration::from_secs(900)),
        );
        assert_eq!(
            b,
            Duration::from_secs(300),
            "under the cap the load factor applies"
        );

        assert_eq!(run.reruns.len(), 3);
        assert_eq!(run.not_rechecked, 1);
        assert_eq!(run.classification.as_deref(), Some(REGRESSION));
        assert_eq!(run.regressions, vec!["test t::d"]);
        assert_eq!(run.flaky.len(), 3);
        let denial = report_of(vec![run], &[]).denial_reason();
        assert!(
            denial.contains("1 head-only failure not re-checked alone")
                && denial.contains("re-run budget"),
            "{denial}"
        );

        let ids: Vec<String> = (0..=MAX_FLAKE_RERUN)
            .map(|i| format!("test t::n{i}"))
            .collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let (check, mut run) = regression_run(&refs);
        let mut bounds = Vec::new();
        recheck_alone(
            &check,
            &mut run,
            None,
            Duration::from_secs(FLAKE_RERUN_BUDGET_S),
            &mut fake_rerun(&pass, 1, &mut bounds),
        );
        assert!(run.reruns.is_empty() && bounds.is_empty());
        assert_eq!(run.not_rechecked, MAX_FLAKE_RERUN + 1);
        assert_eq!(run.regressions.len(), MAX_FLAKE_RERUN + 1);
        assert!(run.denies());
        // Less than a second left is not a re-run: the failure is counted as not re-checked.
        let (check, mut run) = regression_run(&["test t::a", "test t::b"]);
        let mut bounds = Vec::new();
        recheck_alone(
            &check,
            &mut run,
            None,
            Duration::from_millis(1500),
            &mut fake_rerun(&pass, 1, &mut bounds),
        );
        assert_eq!(
            bounds,
            vec![1],
            "one re-run with the 1.5 s budget's whole second"
        );
        assert_eq!(run.not_rechecked, 1);
        assert_eq!(run.regressions, vec!["test t::b"]);
    }

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
            classification: None,
            pre_existing: Vec::new(),
            regressions: Vec::new(),
            base: None,
            env_cannot_run: env.iter().map(|x| x.to_string()).collect(),
            reruns: Vec::new(),
            flaky: Vec::new(),
            not_rechecked: 0,
            ambiguous: Vec::new(),
            env_signals: Vec::new(),
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
                ..at(head_band, "1200s × 3.00 (1-min load 100.9 / 14 cpus)")
            };
            classify(&mut head, base);
            assert_eq!(head.classification.as_deref(), Some(REGRESSION));
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

    /// core#553: the isolated re-run is the check's own `cargo test`, unchanged, plus `--exact`
    /// and the name (core#1060: no binary to narrow to); any other runner is not re-run.
    #[test]
    fn an_isolated_rerun_narrows_cargo_test_to_the_one_id_553() {
        let argv = s(&["cargo", "test", "--locked", "--workspace", "--no-fail-fast"]);
        assert_eq!(
            isolated_rerun_argv(&argv, "test actor::t::a_stop"),
            Some((
                s(&[
                    "cargo",
                    "test",
                    "--locked",
                    "--workspace",
                    "--no-fail-fast",
                    "--",
                    "--exact",
                    "actor::t::a_stop"
                ]),
                "actor::t::a_stop".to_string()
            ))
        );
        assert_eq!(
            isolated_rerun_argv(
                &s(&["cargo", "test", "-p", "x", "--", "--test-threads", "1"]),
                "test t::y"
            )
            .map(|p| p.0),
            Some(s(&[
                "cargo",
                "test",
                "-p",
                "x",
                "--",
                "--test-threads",
                "1",
                "--exact",
                "t::y"
            ])),
            "the command's own selection and test arguments stay"
        );
        assert!(isolated_rerun_argv(&s(&["npm", "run", "test"]), "test t::y").is_none());
        assert!(
            isolated_rerun_argv(&argv, "FAIL tests/x.test.ts > a").is_none(),
            "a vitest id under a cargo check has no runner to re-run with"
        );
        assert!(isolated_rerun_argv(&s(&["cargo", "nextest", "run"]), "test t::y").is_none());
        // Only an oversubscribed host earns the re-run: load above the CPU count.
        assert!(oversubscribed(Some(15.0), 14));
        assert!(!oversubscribed(Some(14.0), 14));
        assert!(!oversubscribed(Some(3.0), 14));
        assert!(!oversubscribed(None, 14), "an unknown load is no evidence");
        assert!(!oversubscribed(Some(f64::NAN), 14));
    }

    /// core#781: `npm run test` / `npm test` hides vitest behind a script — the re-run reads the
    /// script from the worktree's package.json and goes through `npx --no-install vitest`, cut to
    /// the one file as for a direct vitest check; a compound script, a non-vitest script or a
    /// missing package.json is not seen through.
    #[test]
    fn a_script_hidden_vitest_is_rerun_through_its_runner_781() {
        let dir = std::env::temp_dir().join(format!("wicked-rc-781-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(script_runner_argv(&dir, &s(&["npm", "run", "test"])), None);
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"test":"vitest run","ci":"vitest run --config vitest.ci.ts","both":"tsc && vitest run","commented":"vitest run # --config alt.ts","lint":"eslint ."}}"#,
        )
        .unwrap();
        let id = "FAIL tests/chordModifiers.test.ts > chords > a modifier";
        let runner = script_runner_argv(&dir, &s(&["npm", "run", "test"])).unwrap();
        assert_eq!(runner, s(&["npx", "--no-install", "vitest", "run"]));
        assert_eq!(
            isolated_rerun_argv(&runner, id).map(|p| p.0),
            Some(s(&[
                "npx",
                "--no-install",
                "vitest",
                "run",
                "tests/chordModifiers.test.ts"
            ]))
        );
        assert_eq!(
            script_runner_argv(&dir, &s(&["npm", "test"])),
            Some(runner.clone())
        );
        assert_eq!(
            script_runner_argv(&dir, &s(&["pnpm", "ci"]))
                .and_then(|a| isolated_rerun_argv(&a, id))
                .map(|p| p.0),
            Some(s(&[
                "npx",
                "--no-install",
                "vitest",
                "run",
                "tests/chordModifiers.test.ts",
                "--config",
                "vitest.ci.ts"
            ]))
        );
        assert_eq!(script_runner_argv(&dir, &s(&["npm", "run", "both"])), None);
        assert_eq!(
            script_runner_argv(&dir, &s(&["npm", "run", "commented"])),
            None,
            "a comment must not turn into live flags on the re-run"
        );
        assert_eq!(script_runner_argv(&dir, &s(&["npm", "run", "lint"])), None);
        assert_eq!(script_runner_argv(&dir, &s(&["cargo", "test"])), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#766: a vitest head-only failure (`Test timed out in 5000ms` on a 5 s filesystem walk
    /// under load 45-80) is re-run as its FILE alone through the check's own `vitest` — the
    /// `--changed`/`related` selection, its file list and the other flags give way to `run <file>`
    /// (`--config`/`--root` ride along); `npm run test` hides the runner and is not re-run; go's
    /// `FAIL TestX` and a file-level import failure are not vitest ids. The witness is the file's
    /// check-mark line (default or verbose reporter), never another file's.
    #[test]
    fn a_vitest_head_only_failure_is_rerun_as_its_file_alone_766() {
        let id = "FAIL tests/fonts.selfhosted.test.ts > fonts are self-hosted > index.html and \
                  every source file name no font CDN";
        assert_eq!(
            isolated_rerun_argv(&s(&["npx", "vitest", "run"]), id),
            Some((
                s(&["npx", "vitest", "run", "tests/fonts.selfhosted.test.ts"]),
                id.to_string()
            ))
        );
        assert_eq!(
            isolated_rerun_argv(
                &s(&[
                    "npx",
                    "--no-install",
                    "vitest",
                    "related",
                    "--run",
                    "src/a.ts",
                    "src/b.ts",
                    "--config",
                    "vitest.ci.ts",
                    "--coverage",
                    "--changed",
                    "abc123"
                ]),
                "FAIL tests/x.test.ts > a"
            )
            .map(|p| p.0),
            Some(s(&[
                "npx",
                "--no-install",
                "vitest",
                "run",
                "tests/x.test.ts",
                "--config",
                "vitest.ci.ts"
            ]))
        );
        assert_eq!(
            isolated_rerun_argv(
                &s(&["node_modules/.bin/vitest", "run", "--root=packages/a"]),
                "FAIL tests/x.test.ts > a"
            )
            .map(|p| p.0),
            Some(s(&[
                "node_modules/.bin/vitest",
                "run",
                "tests/x.test.ts",
                "--root=packages/a"
            ]))
        );
        // The project/workspace selection rides along: the file re-runs under the SAME project.
        assert_eq!(
            isolated_rerun_argv(
                &s(&[
                    "pnpm",
                    "exec",
                    "vitest",
                    "--project",
                    "unit",
                    "run",
                    "--workspace=ws.ts"
                ]),
                "FAIL tests/x.test.ts > a"
            )
            .map(|p| p.0),
            Some(s(&[
                "pnpm",
                "exec",
                "vitest",
                "run",
                "tests/x.test.ts",
                "--project",
                "unit",
                "--workspace=ws.ts"
            ]))
        );
        assert!(
            isolated_rerun_argv(&s(&["npm", "run", "test"]), "FAIL tests/x.test.ts > a").is_none()
        );
        assert!(isolated_rerun_argv(&s(&["npx", "vitest", "run"]), "FAIL TestX").is_none());
        assert!(
            isolated_rerun_argv(&s(&["npx", "vitest", "run"]), "FAIL --config=x.test.ts > a")
                .is_none(),
            "an option-shaped file never rides the re-run"
        );
        assert!(isolated_rerun_argv(
            &s(&["npx", "vitest", "run"]),
            "FAIL tests/x.test.ts [ tests/x.test.ts ]"
        )
        .is_none());
        assert!(isolated_rerun_argv(&s(&["npx", "vitest", "run"]), "● suite › name").is_none());

        // The witness: the re-run file's check-mark line, in either reporter's shape.
        let watch = vec![id.to_string(), "FAIL tests/x.test.ts > a".to_string()];
        let mut scan = LineScan::watching(&watch);
        scan.line(" ✓ tests/other.test.ts (3 tests) 12ms");
        scan.line(" ✓ tests/fonts.selfhosted.test.tsx (1 test) 1ms");
        assert!(
            scan.passed.is_empty(),
            "another file is not the witness: {:?}",
            scan.passed
        );
        scan.line(" ✓ tests/fonts.selfhosted.test.ts (3 tests | 1 skipped) 9ms");
        scan.line(" ✓ tests/x.test.ts > other test 5ms");
        scan.line(" ✓ tests/x.test.ts > a > nested 5ms");
        scan.line(" ✓ tests/x.test.ts > a1 5ms");
        assert!(
            scan.passed.is_empty(),
            "a file with a skipped test, another test of the file, or a longer id is no witness: {:?}",
            scan.passed
        );
        scan.line(" ✓ tests/fonts.selfhosted.test.ts (2 tests) 1234ms");
        assert_eq!(scan.passed, vec![id.to_string()]);
        scan.line(" ✓ tests/x.test.ts > a 5ms");
        assert_eq!(
            scan.passed, watch,
            "the verbose per-test line for THIS id witnesses too"
        );
        scan.line(" ✓ tests/x.test.ts (1 test) 5ms");
        assert_eq!(scan.passed, watch, "a file is witnessed once");
        let mut none = LineScan::watching(&watch);
        none.line(" ❯ tests/fonts.selfhosted.test.ts (2 tests | 1 failed) 5001ms");
        none.line("   × fonts are self-hosted > index.html and every source file name no font CDN");
        none.line("     → Test timed out in 5000ms.");
        none.line(" ↓ tests/x.test.ts (1 test | 1 skipped)");
        assert!(none.passed.is_empty(), "{:?}", none.passed);
    }

    /// core#553, end to end on a real cargo crate: a test that fails once and passes alone is
    /// `flaky_under_load` (the floor passes, both attempts on the record); a test that fails
    /// every time stays a `regression` after its one re-run.
    #[cfg(unix)]
    #[test]
    fn a_single_head_only_flake_is_rerun_alone_and_classed_flaky_553() {
        let repo = scratch("flake553");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"flake_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [workspace]\n",
        )
        .unwrap();
        std::fs::write(repo.join(".gitignore"), "Cargo.lock\ntmp/\n").unwrap();
        std::fs::write(
            repo.join("src/lib.rs"),
            "#[cfg(test)]\nmod t {\n    #[test]\n    fn ok() {}\n}\n",
        )
        .unwrap();
        let base = git_repo_with_commit(&repo);
        let ctx = FloorContext {
            stage: FloorStage::Verify,
            base_head: Some(base),
            git_dir: Some(repo.join(".git")),
            ..FloorContext::default()
        };
        // Fails the FIRST time in this floor (its private TMPDIR has no marker yet), then passes.
        std::fs::write(
            repo.join("src/lib.rs"),
            "#[cfg(test)]\nmod t {\n    #[test]\n    fn ok() {}\n    #[test]\n    fn flaky() {\n        \
             let m = std::env::temp_dir().join(\"flake-marker\");\n        if !m.exists() {\n            \
             std::fs::write(&m, \"\").unwrap();\n            panic!(\"first attempt\");\n        \
             }\n    }\n}\n",
        )
        .unwrap();
        let report = run_floor(&repo, &ctx);
        if report.sandbox_error.is_some() {
            eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
            return;
        }
        let (load1, cpus) = host_load();
        if !oversubscribed(load1, cpus) {
            // A host with headroom: no evidence of load, so no re-run — the failure stands.
            let c = report
                .checks
                .iter()
                .find(|c| c.name == "cargo-test")
                .expect("cargo-test ran");
            assert_eq!(c.classification.as_deref(), Some(REGRESSION), "{c:#?}");
            assert!(c.reruns.is_empty() && !report.passed);
            eprintln!("repo_checks: host not oversubscribed — the flake re-run is not exercised");
            let _ = std::fs::remove_dir_all(&repo);
            return;
        }
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "cargo-test")
            .expect("cargo-test ran");
        assert_eq!(
            c.classification.as_deref(),
            Some(FLAKY_UNDER_LOAD),
            "{c:#?}"
        );
        assert!(report.passed, "{report:#?}");
        assert_eq!(c.regressions, vec!["test t::flaky".to_string()]);
        assert_eq!(
            c.reruns.len(),
            2,
            "the isolated re-run, then the confirmation run"
        );
        assert!(c.reruns.iter().all(|r| r.passed()), "{:?}", c.reruns);
        assert!(c.reruns[1]
            .argv
            .ends_with(&s(&["--", "--skip", "t::flaky", "--exact"])));
        assert!(c.reruns[0]
            .argv
            .ends_with(&s(&["--no-fail-fast", "--", "--exact", "t::flaky"])));
        let note = report.classification_note().expect("a flake is disclosed");
        assert!(
            note.contains("test t::flaky") && note.contains("flaky_under_load"),
            "{note}"
        );

        // Always red ⇒ one re-run, still a regression, still denies.
        std::fs::write(
            repo.join("src/lib.rs"),
            "#[cfg(test)]\nmod t {\n    #[test]\n    fn ok() {}\n    #[test]\n    fn broken() {\n        \
             panic!(\"always\");\n    }\n}\n",
        )
        .unwrap();
        let report = run_floor(&repo, &ctx);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "cargo-test")
            .expect("cargo-test ran");
        assert_eq!(c.classification.as_deref(), Some(REGRESSION), "{c:#?}");
        assert!(c.reruns.len() <= 1, "at most one re-run: {c:#?}");
        assert!(c.reruns.iter().all(|r| !r.passed()));
        assert!(!report.passed);
        let _ = std::fs::remove_dir_all(&repo);
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
            cfg("echo 'FAILED tests/t.py::test_one - x'; exit 101"),
        )
        .unwrap();
        let base = git_repo_with_commit(&repo);
        std::fs::write(
            repo.join(".wicked/checks.json"),
            cfg("echo 'FAILED tests/t.py::test_one - x'; echo 'FAILED tests/t.py::test_two - x'; exit 101"),
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
        assert!(report.passed, "{report:#?}");
        let _ = std::fs::remove_dir_all(&repo);
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
        let record = |policy: Option<u32>| match policy {
            Some(p) => serde_json::json!({ "floor_policy": p, "run": &ran }).to_string(),
            None => serde_json::to_string(&ran).unwrap(),
        };
        std::fs::write(&path, record(Some(FLOOR_POLICY_VERSION))).unwrap();
        let check = |argv: &[&str]| RepoCheck {
            name: "test_targeted".into(),
            argv: s(argv),
            source: DERIVED_TARGETED_SOURCE.into(),
            timeout_s: None,
            dir: String::new(),
        };
        assert!(
            BaseTree::cached(&scratch_dir, head, &check(&["cargo", "test", "-p", "b"])).is_some()
        );
        assert!(
            BaseTree::cached(&scratch_dir, head, &check(&["cargo", "test", "-p", "a"])).is_none()
        );
        // (codex on #1060) A record of an earlier floor policy (a sandbox that denied
        // `/dev/ptmx`, bare libtest ids) — or of none, the pre-versioned format — is a miss.
        for old in [Some(FLOOR_POLICY_VERSION - 1), None] {
            std::fs::write(&path, record(old)).unwrap();
            assert!(
                BaseTree::cached(&scratch_dir, head, &check(&["cargo", "test", "-p", "b"]))
                    .is_none(),
                "{old:?}"
            );
        }
        drop(scratch_dir);
        let _ = std::fs::remove_dir_all(&wt);
    }
    /// core#789/#790: the environment signals one output line carries, and the helpers the
    /// load-class re-run reads them with.
    #[test]
    fn env_signals_are_read_off_the_output_789_790() {
        assert_eq!(
            env_signal_of_line(
                "[vitest-pool]: Failed to start forks worker for test files /w/a.test.ts, /w/b.test.ts."
            )
            .as_deref()
            .map(|s| s.starts_with("load: ")),
            Some(true)
        );
        assert!(env_signal_of_line("→ Test timed out in 30000ms.").is_some());
        assert!(env_signal_of_line("Hook timed out in 10000ms").is_some());
        assert!(env_signal_of_line("Test timed out in soon").is_none());
        assert_eq!(
            env_signal_of_line("Errors  44 errors").as_deref(),
            Some("errors: 44")
        );
        assert_eq!(
            env_signal_of_line("⎯⎯ Vitest caught 3 unhandled errors during the test run ⎯⎯")
                .as_deref(),
            Some("errors: 3")
        );
        assert_eq!(
            env_signal_of_line(
                "Error: EPERM: operation not permitted, open '/w/node_modules/eslint/lib/x.js'"
            )
            .as_deref()
            .map(|s| s.starts_with("crash: ")),
            Some(true)
        );
        assert!(
            env_signal_of_line("Error: EPERM: operation not permitted, open '/w/src/a.ts'")
                .is_none(),
            "a denied open outside node_modules is not a dependency crash"
        );
        assert!(env_signal_of_line("ok 1 - a test").is_none());

        let wt = Path::new("/w");
        assert_eq!(
            worker_start_files(
                "[vitest-pool]: Failed to start forks worker for test files /w/a.test.ts, /w/src/b.test.ts.",
                wt
            ),
            Ok(s(&["a.test.ts", "src/b.test.ts"]))
        );
        assert_eq!(
            worker_start_files(
                "[vitest-pool]: Failed to start forks worker for test files /elsewhere/a.test.ts.",
                wt
            ),
            Err(()),
            "a file outside the worktree is never re-run"
        );
        assert_eq!(
            worker_start_files("Timeout waiting for worker to respond", wt),
            Ok(vec![])
        );

        assert_eq!(
            vitest_any_file_of("FAIL a.test.ts > s > n"),
            Some("a.test.ts")
        );
        assert_eq!(
            vitest_any_file_of("FAIL a.test.ts [ a.test.ts ]"),
            Some("a.test.ts")
        );
        assert_eq!(vitest_any_file_of("FAIL a.test.ts [ b.test.ts ]"), None);
        assert_eq!(vitest_any_file_of("FAIL TestX"), None);
        assert!(rerun_file_ok("src/a.test.ts"));
        for bad in [
            "/abs/a.test.ts",
            "../a.test.ts",
            "--config=a.test.ts",
            "a b.ts",
            "",
        ] {
            assert!(!rerun_file_ok(bad), "{bad}");
        }
        assert_eq!(
            crash_class("crash: Error: EPERM: operation not permitted, open 'x'"),
            "EPERM"
        );
        assert_eq!(
            crash_class("crash: Error: EACCES: permission denied, open 'x'"),
            "EACCES"
        );

        assert!(vitest_file_pass_line(
            "✓ a.test.ts (3 tests) 12ms",
            "a.test.ts"
        ));
        assert!(vitest_file_pass_line(
            "✓ |unit| a.test.ts (3 tests | 1 skipped) 12ms",
            "a.test.ts"
        ));
        assert!(!vitest_file_pass_line(
            "✓ a.test.tsx (3 tests) 12ms",
            "a.test.ts"
        ));
        assert!(!vitest_file_pass_line(
            "✓ a.test.ts (3 tests | 1 failed) 12ms",
            "a.test.ts"
        ));
        assert!(!vitest_file_pass_line(
            "✓ a.test.ts > one 12ms",
            "a.test.ts"
        ));

        // The signal list is capped and marks the cut.
        let mut scan = LineScan::watching(&[]);
        for _ in 0..(MAX_ENV_SIGNALS + 5) {
            scan.line("Test timed out in 5000ms");
        }
        assert_eq!(scan.signals.len(), MAX_ENV_SIGNALS);
        assert_eq!(
            scan.signals.last().map(String::as_str),
            Some(SIGNALS_TRUNCATED)
        );
        // An identical crash line is recorded once.
        let mut scan = LineScan::watching(&[]);
        for _ in 0..3 {
            scan.line("Error: EACCES: permission denied, open '/w/node_modules/x.js'");
        }
        assert_eq!(scan.signals.len(), 1);
    }

    /// core#789, end to end: a check that crashes with the SAME dependency-file EPERM on head and
    /// base and again on its re-run is `env_cannot_run` — it does not deny and the floor goes on
    /// to the next check (which used to be skipped).
    #[cfg(unix)]
    #[test]
    fn an_identical_env_crash_on_head_and_base_is_env_cannot_run_and_skips_nothing_789() {
        let repo = scratch("crash789");
        std::fs::create_dir_all(repo.join(".wicked")).unwrap();
        std::fs::write(repo.join(".gitignore"), "tmp/\n").unwrap();
        std::fs::write(
            repo.join(CONFIG_PATH),
            r#"{"lint":["sh","-c","echo \"Error: EPERM: operation not permitted, open '$PWD/node_modules/eslint/lib/x.js'\" >&2; exit 2"],"test":["true"],"timeout_s":45}"#,
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
            eprintln!("repo_checks: no sandbox tool here — the crash floor cannot run");
            return;
        }
        assert_eq!(names_run(&report), vec!["lint", "test"], "{report:?}");
        assert!(report.skipped.is_empty(), "{report:?}");
        let lint = &report.checks[0];
        assert_eq!(
            lint.classification.as_deref(),
            Some(ENV_CANNOT_RUN),
            "{lint:?}"
        );
        assert!(lint.env_crash());
        assert!(!lint.denies());
        assert_eq!(lint.reruns.len(), 1);
        assert!(report.passed, "{report:?}");
        assert!(report
            .classification_note()
            .is_some_and(|n| n.contains("crashed with an environment error")));
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// core#790, end to end: head and base BOTH red from load (different files never started),
    /// so the id diff calls the head's failures regressions; the load-class re-run runs the
    /// failing file plus the never-started file once, serially, and a pass is `flaky_under_load`.
    /// An error count above the worker-start errors (an unexplained unhandled error) is not load.
    #[cfg(unix)]
    #[test]
    fn a_load_class_failure_reruns_whatever_the_base_did_790() {
        let repo = scratch("load790");
        std::fs::create_dir_all(repo.join(".wicked")).unwrap();
        std::fs::create_dir_all(repo.join("vt")).unwrap();
        std::fs::write(repo.join(".gitignore"), "tmp/\n").unwrap();
        std::fs::write(
            repo.join(CONFIG_PATH),
            r#"{"test":["sh","vt/vitest","run"],"timeout_s":45}"#,
        )
        .unwrap();
        // `heal`: after its first full run the suite runs clean (a load flake; the confirmation
        // run is that clean run, core#1060) — a marker file in the check's isolated HOME.
        let script = |fail_id: &str, never: &str, errors: usize, heal: &str| {
            format!(
                "if [ \"$1\" = run ] && [ -n \"$2\" ]; then shift; for f in \"$@\"; do case \"$f\" in \
                 *.ts) echo \" ✓ $f (1 test) 1ms\";; esac; done; exit 0; fi\n\
                 if [ -n '{heal}' ]; then if [ -e \"$HOME/{heal}\" ]; then exit 0; fi; \
                 touch \"$HOME/{heal}\"; fi\n\
                 echo ' FAIL  {fail_id} > s > n'\n\
                 echo \"[vitest-pool]: Failed to start forks worker for test files $PWD/{never}.\"\n\
                 echo ' Errors  {errors} error'\n\
                 exit 1\n"
            )
        };
        std::fs::write(
            repo.join("vt/vitest"),
            script("c.test.ts", "d.test.ts", 1, ""),
        )
        .unwrap();
        let base = git_repo_with_commit(&repo);
        let ctx = FloorContext {
            stage: FloorStage::Verify,
            base_head: Some(base),
            git_dir: Some(repo.join(".git")),
            ..FloorContext::default()
        };
        std::fs::write(
            repo.join("vt/vitest"),
            script("a.test.ts", "b.test.ts", 1, "healed790"),
        )
        .unwrap();
        let report = run_floor(&repo, &ctx);
        if report.sandbox_error.is_some() {
            eprintln!("repo_checks: no sandbox tool here — the load floor cannot run");
            return;
        }
        let test = &report.checks[0];
        assert_eq!(
            test.classification.as_deref(),
            Some(FLAKY_UNDER_LOAD),
            "{test:?}"
        );
        assert_eq!(
            test.reruns.len(),
            2,
            "the files, then the confirmation run: {test:?}"
        );
        assert_eq!(test.reruns[1].argv, s(&["sh", "vt/vitest", "run"]));
        assert!(test.reruns[1].passed());
        assert_eq!(
            test.reruns[0].argv,
            s(&[
                "sh",
                "vt/vitest",
                "run",
                "a.test.ts",
                "b.test.ts",
                "--no-file-parallelism"
            ])
        );
        assert!(report.passed, "{report:?}");

        // More errors than worker-start failures: not explained by load — no load-class re-run
        // (the never-started file is not re-run; core#553/#766's own one-file rule may still).
        std::fs::write(
            repo.join("vt/vitest"),
            script("a.test.ts", "b.test.ts", 5, ""),
        )
        .unwrap();
        let report = run_floor(&repo, &ctx);
        let test = &report.checks[0];
        assert!(
            test.reruns
                .iter()
                .all(|r| !r.argv.iter().any(|a| a == "b.test.ts")),
            "{test:?}"
        );
        let _ = std::fs::remove_dir_all(&repo);
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

    // ---- core#1060 round 14: no libtest qualification. Every forgery the review rounds found (8 to
    // 13) and codex's three round-14 fixtures replay here as faked outputs; each one denies.

    fn adj_text(v: &[&str]) -> String {
        let mut t = v.join("\n");
        t.push('\n');
        t
    }

    /// The head's scanned ids classified against the base's, both read the way the floor reads a
    /// check's two streams.
    fn scanned_head(head: (&str, &str), base: (&str, &str)) -> CheckRun {
        fn ids(out: &str, err: &str) -> Vec<String> {
            qualify(scan_of(out), scan_of(err)).0
        }
        let (h, b) = (ids(head.0, head.1), ids(base.0, base.1));
        let v = |x: &[String]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut run = failed_run(&[], &[]);
        run.failure_ids = v(&h);
        let mut base_run = failed_run(&[], &[]);
        base_run.failure_ids = v(&b);
        classify(&mut run, base_of(base_run));
        run
    }

    /// Rounds 8 to 14 of the #1060 reviews, each a way text the change's tests can print (or a
    /// per-target split) hid a real libtest regression behind a same-named base failure. With no
    /// libtest qualification at all, the head's failure is read (never dropped) and never matched:
    /// every case denies.
    #[test]
    fn no_forged_output_lets_a_libtest_failure_read_as_pre_existing_1060() {
        let fail = |name: &str| {
            format!(
                "running 2 tests\ntest {name} ... FAILED\ntest other ... ok\n\nfailures:\n    \
                 {name}\n\ntest result: FAILED. 1 passed; 1 failed\n"
            )
        };
        let mut deep = String::from(
            "running 1 test\ntest shared ... FAILED\n\nfailures:\n\n---- shared stdout ----\n",
        );
        deep.push_str(&"running 1 test\n".repeat(100));
        deep.push_str("\nfailures:\n    shared\n\ntest result: FAILED. 0 passed; 1 failed\n");
        let forged_summary = adj_text(&[
            "running 2 tests",
            "test old ... FAILED",
            "failures:",
            "---- fake stdout ----",
            "running 2 tests",
            "test new ... FAILED",
            "test old ... FAILED",
            "",
            "failures:",
            "    new",
            "    old",
            "",
            "test result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out",
            "",
            "failures:",
            "    old",
            "",
            "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured",
        ]);
        let doc = "running 1 test\ntest src/lib.rs - f (line 1) ... FAILED\n\nfailures:\n    \
            src/lib.rs - f (line 1)\n\ntest result: FAILED. 0 passed; 1 failed\n\n\
            all doctests ran in 0.4s; merged doctests compilation took 0.3s\n";
        // (label, head (stdout, stderr), base (stdout, stderr), the id that must deny)
        type Streams = (String, String);
        let cases: Vec<(&str, Streams, Streams, &str)> = vec![
            (
                "r8 codex: a wrapper printing only failing suites, no cargo lines",
                (fail("shared"), String::new()),
                (fail("shared"), String::new()),
                "test shared",
            ),
            (
                "r8 agy: an outer binary cut off after its FAILED line",
                ("running 2 tests\ntest shared ... FAILED\n".into(), String::new()),
                (fail("shared"), String::new()),
                "test shared",
            ),
            (
                "r9 codex: 100 runs nested in captured output",
                (deep.clone(), String::new()),
                (fail("shared"), String::new()),
                "test shared",
            ),
            (
                "r10: a section header before failures:",
                (
                    "running 2 tests\ntest a ... ok\n---- x stdout ----\ntest y ... FAILED\n\n\
                     failures:\n    y\n\ntest result: FAILED. 1 passed; 1 failed\n"
                        .into(),
                    String::new(),
                ),
                (fail("y"), String::new()),
                "test y",
            ),
            (
                "r11 agy: raw failures: + section framing, beside a shared pytest failure",
                (
                    "FAILED tests/t.py::test_old - red\nrunning 2 tests\ntest a ... ok\nfailures:\n\
                     ---- x stdout ----\ntest y ... FAILED\n\ntest result: FAILED. 1 passed; 1 \
                     failed\n"
                        .into(),
                    String::new(),
                ),
                (
                    "FAILED tests/t.py::test_old - red\n".into(),
                    String::new(),
                ),
                "test y",
            ),
            (
                "r12 codex: raw framing plus a forged closing summary that lines up",
                (forged_summary.clone(), String::new()),
                (fail("old"), String::new()),
                "test new",
            ),
            (
                "r13 codex: a forged cargo selector and section header on raw stderr",
                (
                    fail("shared"),
                    "error: test failed, to rerun pass `-p a --lib`\n---- fake stdout ----\n\
                     test result: FAILED. 0 passed; 99 failed\n"
                        .into(),
                ),
                (
                    fail("shared"),
                    "error: test failed, to rerun pass `-p a --lib`\n".into(),
                ),
                "test shared",
            ),
            (
                "r13 agy: a doctest-footer merge across two Doc-tests targets",
                (
                    format!("running 0 tests\n\ntest result: ok. 0 passed; 0 failed\n\n{doc}"),
                    "   Doc-tests crate_a\n   Doc-tests crate_b\n".into(),
                ),
                (doc.into(), "   Doc-tests crate_a\n".into()),
                "test src/lib.rs - f (line 1)",
            ),
            (
                "r13 agy: a crash after a pre-existing named failure",
                (
                    "running 3 tests\ntest old ... FAILED\n".into(),
                    "error: test failed, to rerun pass `-p c --lib`\n\nCaused by:\n  process \
                     didn't exit successfully: `c-1` (signal: 11, SIGSEGV: invalid memory \
                     reference)\n"
                        .into(),
                ),
                (fail("old"), String::new()),
                "test old",
            ),
            (
                "r14 codex: a test behind a feature only a sibling crate enables (--workspace)",
                (fail("gated::t"), String::new()),
                (fail("gated::t"), String::new()),
                "test gated::t",
            ),
            (
                "r14 codex: a package named with single quotes",
                (fail("quoted::t"), String::new()),
                (fail("quoted::t"), String::new()),
                "test quoted::t",
            ),
            (
                "r13 agy: a nested-sandbox launcher signature (the env class is gone: it denies)",
                (
                    "running 1 test\ntest jail ... FAILED\n\nfailures:\n\n---- jail stdout \
                     ----\nsandbox-exec: sandbox_apply: Operation not permitted\n"
                        .into(),
                    String::new(),
                ),
                (
                    "running 1 test\ntest jail ... FAILED\n\nfailures:\n\n---- jail stdout \
                     ----\nsandbox-exec: sandbox_apply: Operation not permitted\n"
                        .into(),
                    String::new(),
                ),
                "test jail",
            ),
            (
                "r14 codex: a doctest-only library (`[lib] test = false`)",
                (doc.into(), String::new()),
                (doc.into(), String::new()),
                "test src/lib.rs - f (line 1)",
            ),
        ];
        for (label, head, base, must) in cases {
            let run = scanned_head((&head.0, &head.1), (&base.0, &base.1));
            assert!(
                run.failure_ids.iter().any(|i| i == must),
                "{label}: {must} was read: {:?}",
                run.failure_ids
            );
            assert_eq!(
                run.classification.as_deref(),
                Some(REGRESSION),
                "{label}: {run:?}"
            );
            assert!(run.regressions.iter().any(|r| r == must), "{label}");
            assert!(run.pre_existing.iter().all(|p| !p.starts_with("test ")));
            assert!(run.denies(), "{label}");
        }
    }

    /// The isolated flake re-run is the check's own argv plus `-- --exact <name>` (after any test
    /// arguments it already passes): it runs every test of that name in every binary, so the id
    /// clears only when that run exits 0 AND prints the name `ok` ([`recheck_alone`]).
    #[test]
    fn the_flake_rerun_is_the_checks_own_argv_with_exact_1060() {
        let argv = s(&["cargo", "test", "--locked", "--no-fail-fast", "-p", "core"]);
        assert_eq!(
            isolated_rerun_argv(&argv, "test a::b"),
            Some((
                s(&[
                    "cargo",
                    "test",
                    "--locked",
                    "--no-fail-fast",
                    "-p",
                    "core",
                    "--",
                    "--exact",
                    "a::b"
                ]),
                "a::b".to_string()
            ))
        );
        let with_args = s(&["cargo", "+nightly", "test", "--", "--nocapture"]);
        assert_eq!(
            isolated_rerun_argv(&with_args, "test a::b").map(|r| r.0),
            Some(s(&[
                "cargo",
                "+nightly",
                "test",
                "--",
                "--nocapture",
                "--exact",
                "a::b"
            ]))
        );
        // A bracketed name (a stored qualifier, or a parameterised harness's case) is no
        // `--exact` filter: never re-run (agy on #1060 — `foo [bad_case]` would re-run `foo`).
        assert!(isolated_rerun_argv(&argv, "test a::b [-p x --lib]").is_none());
        assert!(isolated_rerun_argv(&s(&["sh", "-c", "cargo test"]), "test a::b").is_none());
        assert!(isolated_rerun_argv(&argv, "FAILED tests/t.py::x").is_none());
    }

    /// Past [`MAX_FAILURE_IDS`] the dropped ids could hide the head's one new failure behind a
    /// base that shares every kept one: the check carries [`FAILURE_IDS_TRUNCATED`], which
    /// nothing matches (not even the base's own marker).
    #[test]
    fn failure_ids_past_the_cap_never_read_as_pre_existing_1060() {
        let shared: String = (0..MAX_FAILURE_IDS)
            .map(|i| format!("FAILED tests/t.py::test_{i} - red\n"))
            .collect();
        let head = format!("{shared}FAILED tests/t.py::test_new - red\n");
        let base = format!("{shared}FAILED tests/t.py::test_other - red\n");
        let (ids, _, _) = qualify(scan_of(&head), LineScan::default());
        assert_eq!(ids.len(), MAX_FAILURE_IDS + 1);
        assert_eq!(ids.last().map(String::as_str), Some(FAILURE_IDS_TRUNCATED));
        let run = scanned_head((&head, ""), (&base, ""));
        assert_eq!(run.regressions, vec![FAILURE_IDS_TRUNCATED]);
        assert!(run.denies());
        // Colored output reads as plain, and an OSC hyperlink is stripped.
        let colored = "\u{1b}[1mtest shared ... \u{1b}[31mFAILED\u{1b}[0m\n";
        assert_eq!(
            qualify(scan_of(colored), LineScan::default()).0,
            vec!["test shared"]
        );
        assert_eq!(
            strip_ansi("\u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7} ok"),
            "link ok"
        );
    }

    /// core#1060 round 15: a failure the output PROVES but does not name adds an
    /// `unattributed failure: <kind>` id (real cargo 1.99 / rustdoc lines; pytest's and vitest's
    /// documented ones). It never matches the base and is never re-run alone, so a shared named
    /// failure beside it can no longer make the check pre-existing.
    #[test]
    fn a_failure_without_a_name_adds_a_marker_that_never_matches_1060() {
        let marked = |line: &str| qualify(scan_of(line), LineScan::default()).0;
        let m = |kind: &str| format!("{UNATTRIBUTED_FAILURE}{kind}");
        assert_eq!(
            marked(
                "  process didn't exit successfully: `/w/target/debug/deps/fx15-0` (signal: 6, \
                 SIGABRT: process abort signal)\n"
            ),
            vec![m("a test process exited abnormally")]
        );
        assert_eq!(
            marked("error: could not compile `fx15` (test \"bad\") due to 1 previous error\n"),
            vec![m("a crate did not compile")]
        );
        assert_eq!(
            marked(
                "test src/lib.rs - f (line 1) ... FAILED\nerror[E0308]: mismatched types\n\
                 Couldn't compile the test.\n"
            ),
            vec![
                "test src/lib.rs - f (line 1)".to_string(),
                m("a doctest did not compile")
            ]
        );
        assert_eq!(
            marked("ERROR tests/test_new.py - ModuleNotFoundError: No module named 'nope'\n"),
            vec![m("pytest reported an error")]
        );
        assert_eq!(
            marked(" FAIL  tests/b.test.ts [ tests/b.test.ts ]\n"),
            vec![
                m("a test file failed as a whole"),
                "FAIL tests/b.test.ts [ tests/b.test.ts ]".to_string()
            ]
        );
        assert_eq!(
            marked(" FAIL  tests/a.test.ts > s > x\n"),
            vec!["FAIL tests/a.test.ts > s > x".to_string()],
            "a named vitest failure is no marker"
        );
        assert!(isolated_rerun_argv(
            &s(&["cargo", "test"]),
            &m("a test process exited abnormally")
        )
        .is_none());
        // Each must deny beside a failure the base shares, on a host with headroom.
        let shared = "FAILED tests/t.py::test_old - red\n";
        for (label, extra) in [
            (
                "pytest collection error",
                "ERROR tests/test_new.py - ModuleNotFoundError: No module named 'nope'\n",
            ),
            (
                "vitest import failure",
                " FAIL  tests/b.test.ts [ tests/b.test.ts ]\n",
            ),
            (
                "a crashed test binary",
                "  process didn't exit successfully: `/w/deps/a-1` (signal: 11, SIGSEGV)\n",
            ),
        ] {
            let head = format!("{shared}{extra}");
            for base in [shared.to_string(), head.clone()] {
                let run = scanned_head((&head, ""), (&base, ""));
                assert_eq!(
                    run.classification.as_deref(),
                    Some(REGRESSION),
                    "{label}: {run:?}"
                );
                assert!(
                    run.regressions
                        .iter()
                        .any(|r| r.starts_with(UNATTRIBUTED_FAILURE)),
                    "{label}"
                );
                assert!(run.denies(), "{label}");
            }
        }
    }

    /// A stand-in re-run for the confirmation tests: an isolated re-run (it names what it
    /// watches) passes; the confirmation run (no names) exits `confirm`.
    fn confirming_rerun<'a>(
        confirm: i32,
        argvs: &'a mut Vec<Vec<String>>,
    ) -> impl FnMut(&RepoCheck, &[String], Option<Duration>) -> (CheckRun, Vec<String>) + 'a {
        move |one, names, cap| {
            // The confirmation (no names) runs under the check's own load-scaled bound; an
            // isolated re-run under what the budget has left.
            assert_eq!(cap.is_none(), names.is_empty(), "{:?}", one.argv);
            argvs.push(one.argv.clone());
            let mut r = failed_run(&[], &[]);
            r.argv = one.argv.clone();
            r.exit_code = Some(if names.is_empty() { confirm } else { 0 });
            r.duration_ms = 1000;
            (r, names.to_vec())
        }
    }

    /// agy round 14: under load a named flake passes alone while the run's REAL failure has no
    /// name — a crashed test binary, a doctest the `--exact` filter never compiled. Flaky needs
    /// the check itself, the flaky tests skipped, to exit 0: here it does not, and the check
    /// denies. A clean confirmation keeps the flaky path alive, and answers for the markers.
    #[test]
    fn flaky_under_load_needs_a_clean_run_with_the_flakes_skipped_1060() {
        let crash = format!("{UNATTRIBUTED_FAILURE}a test process exited abnormally");
        let doc = format!("{UNATTRIBUTED_FAILURE}a doctest did not compile");
        for marker in [&crash, &doc] {
            let (check, mut run) = regression_run(&["test t::flaky", marker]);
            let mut argvs = Vec::new();
            recheck_alone(
                &check,
                &mut run,
                None,
                Duration::from_secs(FLAKE_RERUN_BUDGET_S),
                &mut confirming_rerun(101, &mut argvs),
            );
            assert_eq!(run.classification.as_deref(), Some(REGRESSION), "{run:?}");
            assert_eq!(run.flaky, vec!["test t::flaky"]);
            assert_eq!(
                run.regressions,
                vec![marker.to_string(), NO_CLEAN_RUN.into()]
            );
            assert_eq!(
                run.not_rechecked, 0,
                "a marker is not a name left unchecked"
            );
            assert!(run.denies());
            assert_eq!(
                argvs.last().unwrap(),
                &s(&[
                    "cargo",
                    "test",
                    "--workspace",
                    "--no-fail-fast",
                    "--",
                    "--skip",
                    "t::flaky",
                    "--exact"
                ])
            );
        }
        // A real rustdoc compile failure also names the doctest — which is no `--exact` name.
        let scanned = qualify(
            scan_of(
                "test t::flaky ... FAILED\ntest src/lib.rs - f (line 1) ... FAILED\n\
                 Couldn't compile the test.\n",
            ),
            LineScan::default(),
        )
        .0;
        let refs: Vec<&str> = scanned.iter().map(String::as_str).collect();
        let (check, mut run) = regression_run(&refs);
        let mut argvs = Vec::new();
        recheck_alone(
            &check,
            &mut run,
            None,
            Duration::from_secs(FLAKE_RERUN_BUDGET_S),
            &mut confirming_rerun(0, &mut argvs),
        );
        assert_eq!(run.classification.as_deref(), Some(REGRESSION));
        assert!(run.denies());
        // A real flake, a clean confirmation: still flaky_under_load (and the marker of a test
        // binary that crashed only under load is answered for by the clean run).
        for ids in [vec!["test t::flaky"], vec!["test t::flaky", crash.as_str()]] {
            let (check, mut run) = regression_run(&ids);
            let mut argvs = Vec::new();
            recheck_alone(
                &check,
                &mut run,
                None,
                Duration::from_secs(FLAKE_RERUN_BUDGET_S),
                &mut confirming_rerun(0, &mut argvs),
            );
            assert_eq!(
                run.classification.as_deref(),
                Some(FLAKY_UNDER_LOAD),
                "{run:?}"
            );
            assert!(!run.denies());
            assert_eq!(
                run.reruns.len(),
                2,
                "the isolated re-run and the confirmation"
            );
        }
        // The confirmation's argv: libtest skips exactly; a command with its own test-name
        // filter is not confirmable; any other runner re-runs as itself (vitest has no skip).
        assert_eq!(
            confirmation_argv(
                &s(&["cargo", "test", "--", "--test-threads", "1"]),
                &["test a".to_string(), "test b".to_string()]
            ),
            Some(s(&[
                "cargo",
                "test",
                "--",
                "--test-threads",
                "1",
                "--skip",
                "a",
                "--skip",
                "b",
                "--exact"
            ]))
        );
        assert!(confirmation_argv(
            &s(&["cargo", "test", "--", "some_filter"]),
            &["test a".to_string()]
        )
        .is_none());
        let npm = s(&["npm", "test"]);
        assert_eq!(
            confirmation_argv(&npm, &["FAIL tests/a.test.ts > s > x".to_string()]),
            Some(npm.clone())
        );
    }

    /// agy round 14: `strip_ansi` strips only COMPLETE sequences; an unterminated or malformed one
    /// keeps the rest of the line, and the libtest id is read from the first `test ` on it — so a
    /// test that writes an escape (or any text) onto libtest's line cannot hide its FAILED line.
    /// A forged name that reads as a libtest flag, or carries a bracket suffix, is no `--exact`
    /// filter: not re-run, never matched, a regression.
    #[test]
    fn an_escape_or_a_forged_name_cannot_hide_or_clear_a_failure_1060() {
        assert_eq!(
            strip_ansi("\u{1b}[1;31mFAILED\u{1b}[0m \u{1b}]8;;http://x\u{1b}\\link\u{1b}]8;;\u{7}"),
            "FAILED link"
        );
        for raw in [
            "\u{1b}]8;;test new ... FAILED",
            "\u{1b}[31\u{1}test new ... FAILED",
            "\u{1b}]8;;\u{1b}[1mtest new ... \u{1b}[31mFAILED\u{1b}[0m",
            "progress 40%test new ... FAILED",
        ] {
            let (ids, _, _) = qualify(scan_of(raw), LineScan::default());
            assert_eq!(ids, vec!["test new"], "{raw:?}");
        }
        assert_eq!(
            strip_ansi("\u{1b}]8;;test new ... FAILED"),
            "\u{1b}]8;;test new ... FAILED",
            "an unterminated OSC is kept verbatim"
        );
        // Beside a pytest failure the base shares, the escaped libtest line still denies.
        let shared = "FAILED tests/t.py::test_old - red\n";
        let run = scanned_head(
            (&format!("{shared}\u{1b}]8;;test new ... FAILED\n"), ""),
            (shared, ""),
        );
        assert_eq!(run.regressions, vec!["test new"]);
        assert!(run.denies());
        // Forged names: never re-run, never excused.
        let argv = s(&["cargo", "test", "--workspace"]);
        for id in [
            "test --nocapture",
            "test foo [bad_case]",
            "test a b",
            "test x\u{7}",
        ] {
            assert!(isolated_rerun_argv(&argv, id).is_none(), "{id:?}");
            let mut head = failed_run(&[id], &[]);
            classify(&mut head, base_of(failed_run(&[id], &[])));
            assert_eq!(head.classification.as_deref(), Some(REGRESSION), "{id:?}");
        }
        assert_eq!(libtest_name("test foo [bad_case]"), Some("foo [bad_case]"));
        let (check, mut run) = regression_run(&["test --nocapture"]);
        recheck_alone(
            &check,
            &mut run,
            None,
            Duration::from_secs(FLAKE_RERUN_BUDGET_S),
            &mut confirming_rerun(0, &mut Vec::new()),
        );
        assert_eq!(run.not_rechecked, 1);
        assert!(run.reruns.is_empty() && run.denies());
    }

    /// codex/agy round 16: the floor appends `--exact` (and `--skip`), so it re-runs alone or
    /// confirms ONLY a `cargo test` argv it understands element by element. Every other shape —
    /// a positional TESTNAME (`cargo test foo`: `--exact` makes it match no test, and a
    /// confirmation of 0 tests exits 0), a `--skip` or `--exact` of its own, `--ignored`, an
    /// unknown flag, a second `--` — is neither, and the check stays a regression.
    #[test]
    fn only_an_understood_cargo_argv_is_rerun_or_confirmed_1060() {
        let flaky = ["test a".to_string()];
        for argv in [
            s(&["cargo", "test", "foo", "--", "--test-threads=1"]),
            s(&["cargo", "test", "--no-fail-fast", "foo"]),
            s(&["cargo", "test", "--", "--skip", "x"]),
            s(&["cargo", "test", "--", "--skip=x"]),
            s(&["cargo", "test", "--", "--exact"]),
            s(&["cargo", "test", "--", "--ignored"]),
            s(&["cargo", "test", "--", "--include-ignored"]),
            s(&["cargo", "test", "--", "x"]),
            s(&["cargo", "test", "--unknown-flag"]),
            s(&["cargo", "test", "--", "--nocapture", "--", "x"]),
            s(&["cargo", "test", "--lib"]),
            s(&["cargo", "test", "-q"]),
            s(&["cargo", "test", "--", "-q"]),
            s(&["cargo", "test", "--", "--quiet"]),
            s(&["cargo", "test", "--target-dir", "t"]),
            s(&["cargo", "test", "-p"]),
            s(&["cargo", "test", "-p", "--lib"]),
            s(&["cargo", "test", "--", "--test-threads"]),
            s(&["cargo", "test", "--features="]),
            s(&["cargo", "build"]),
            s(&["sh", "-c", "cargo test"]),
        ] {
            assert!(isolated_rerun_argv(&argv, "test a").is_none(), "{argv:?}");
            assert!(confirmation_argv(&argv, &flaky).is_none(), "{argv:?}");
        }
        let every = s(&[
            "cargo",
            "+nightly",
            "test",
            "--workspace",
            "--exclude",
            "x",
            "-p",
            "a",
            "--package=b",
            "-pc",
            "-F",
            "f",
            "--features=g",
            "--all-features",
            "--no-default-features",
            "--release",
            "--profile",
            "ci",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--locked",
            "--frozen",
            "--offline",
            "-j",
            "8",
            "-j8",
            "--jobs=2",
            "--no-fail-fast",
            "--color",
            "never",
            "--",
            "--test-threads=4",
            "--test-threads",
            "2",
            "--nocapture",
            "--show-output",
            "--color=always",
        ]);
        let mut want = every.clone();
        want.extend(s(&["--exact", "a"]));
        assert_eq!(
            isolated_rerun_argv(&every, "test a").map(|r| r.0),
            Some(want)
        );
        let mut want = every.clone();
        want.extend(s(&["--skip", "a", "--exact"]));
        assert_eq!(confirmation_argv(&every, &flaky), Some(want));
        // Not cargo (vitest, pytest): no `--exact` is appended — the check re-runs as itself.
        let npm = s(&["npm", "test"]);
        assert_eq!(
            confirmation_argv(&npm, &["FAIL tests/a.test.ts > s > x".to_string()]),
            Some(npm.clone())
        );
    }

    /// (core#1060) One re-run attempt per check, from ANY path: once a check re-ran — an isolated
    /// re-run, a load-class re-run, a crash re-run or a confirmation — no other path starts. The
    /// flake path, the load-class path and the crash path each return untouched.
    #[test]
    fn one_rerun_attempt_per_check_from_any_path_1060() {
        let (check, mut run) = regression_run(&["test t::flaky"]);
        let mut earlier = failed_run(&[], &[]);
        earlier.source = "load-class re-run of 1 file (core#790)".into();
        run.reruns.push(earlier);
        let mut argvs = Vec::new();
        recheck_alone(
            &check,
            &mut run,
            None,
            Duration::from_secs(FLAKE_RERUN_BUDGET_S),
            &mut confirming_rerun(0, &mut argvs),
        );
        assert!(argvs.is_empty(), "{argvs:?}");
        assert_eq!(run.reruns.len(), 1);
        assert_eq!(run.classification.as_deref(), Some(REGRESSION));
        assert!(run.denies());
        // The other two paths refuse the same check before touching anything.
        let wt = scratch("one-rerun");
        let sandbox = sandbox_for(&wt);
        let scratch_dir = CheckScratch::prepare(&wt).unwrap();
        run.env_signals = vec![
            "load: [vitest-pool]: Failed to start forks worker for test files /w/a.test.ts.".into(),
        ];
        assert!(!rerun_load_class(
            &wt,
            &check,
            &mut run,
            &sandbox,
            &scratch_dir
        ));
        run.classification = Some(UNCLASSIFIED_RED_BASE.to_string());
        assert!(!rerun_env_crash(
            &wt,
            &check,
            &mut run,
            &sandbox,
            &scratch_dir
        ));
        assert_eq!(run.reruns.len(), 1);
        drop(scratch_dir);
        let _ = std::fs::remove_dir_all(&wt);
    }

    /// (core#1060) A truncated failure set (more ids than [`MAX_FAILURE_IDS`]) is never re-run or
    /// confirmed: failures the floor never saw cannot be answered for. It stays a regression.
    #[test]
    fn a_truncated_failure_set_is_never_rerun_1060() {
        let (check, mut run) = regression_run(&["test t::flaky", FAILURE_IDS_TRUNCATED]);
        let mut argvs = Vec::new();
        recheck_alone(
            &check,
            &mut run,
            None,
            Duration::from_secs(FLAKE_RERUN_BUDGET_S),
            &mut confirming_rerun(0, &mut argvs),
        );
        assert!(argvs.is_empty(), "{argvs:?}");
        assert!(run.reruns.is_empty() && run.flaky.is_empty());
        assert_eq!(run.classification.as_deref(), Some(REGRESSION));
        assert!(run.denies());
    }

    /// agy round 16: the confirmation runs under the check's own load-scaled bound — not what
    /// the isolated re-runs left of their budget (a full workspace run under load needs it). Here
    /// the isolated re-run spends the whole budget and the confirmation still runs; an allowlisted
    /// argv confirms flaky, and the clean run answers for the marker, which leaves the record.
    #[test]
    fn the_confirmation_runs_under_the_checks_own_bound_1060() {
        let crash = format!("{UNATTRIBUTED_FAILURE}a test process exited abnormally");
        let (mut check, mut run) = regression_run(&["test t::flaky", &crash]);
        check.argv = s(&[
            "cargo",
            "test",
            "--workspace",
            "--no-fail-fast",
            "--",
            "--test-threads=4",
        ]);
        let mut argvs = Vec::new();
        recheck_alone(
            &check,
            &mut run,
            None,
            Duration::from_secs(1),
            &mut confirming_rerun(0, &mut argvs),
        );
        assert_eq!(
            run.classification.as_deref(),
            Some(FLAKY_UNDER_LOAD),
            "{run:?}"
        );
        assert_eq!(run.regressions, vec!["test t::flaky"]);
        assert_eq!(
            argvs.last().unwrap(),
            &s(&[
                "cargo",
                "test",
                "--workspace",
                "--no-fail-fast",
                "--",
                "--test-threads=4",
                "--skip",
                "t::flaky",
                "--exact"
            ])
        );
    }

    /// codex round 16, with real cargo: a load flake beside a test that always crashes, under a
    /// check that passes cargo a TESTNAME (`cargo test foo`). The floor does not re-run or
    /// confirm an argv it does not understand, so the check denies. What it no longer does: the
    /// old confirmation argv (`foo` made exact) selects 0 tests and exits 0.
    #[cfg(unix)]
    #[test]
    fn a_flake_beside_a_crash_under_a_cargo_testname_filter_denies_1060() {
        let wt = scratch("testname1060");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(
            wt.join("Cargo.toml"),
            "[package]\nname = \"tn_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\npath = \"src/lib.rs\"\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(
            wt.join("src/lib.rs"),
            "#[cfg(test)]\nmod t {\n    #[test]\n    fn foo_aflaky() {\n        let mark = \
             concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/flaked\");\n        if \
             std::fs::metadata(mark).is_err() {\n            std::fs::write(mark, \"\").unwrap();\
             \n            panic!(\"flaked\");\n        }\n    }\n    #[test]\n    fn \
             foo_zcrash() {\n        std::process::abort();\n    }\n}\n",
        )
        .unwrap();
        let sandbox = sandbox_for(&wt);
        if sandbox.level != SandboxLevel::Sandboxed {
            eprintln!("repo_checks: no sandbox tool here — the cargo fixture cannot run");
            let _ = std::fs::remove_dir_all(&wt);
            return;
        }
        let scratch_dir = CheckScratch::prepare(&wt).unwrap();
        let check = RepoCheck {
            name: "test".into(),
            argv: s(&["cargo", "test", "foo", "--", "--test-threads=1"]),
            source: "fixture".into(),
            timeout_s: Some(600),
            dir: String::new(),
        };
        let mut run = run_one(&wt, &check, &sandbox, &scratch_dir, Tree::Head);
        assert!(!run.passed(), "{run:?}");
        assert!(
            run.failure_ids.iter().any(|i| i == "test t::foo_aflaky")
                && run
                    .failure_ids
                    .iter()
                    .any(|i| i.starts_with(UNATTRIBUTED_FAILURE)),
            "the flake is named, the crash is a marker: {:?} {}",
            run.failure_ids,
            run.stderr_tail
        );
        // The head-only set of a regression (a base where both pass).
        run.regressions = run.failure_ids.clone();
        run.classification = Some(REGRESSION.to_string());
        recheck_alone(
            &check,
            &mut run,
            None,
            Duration::from_secs(FLAKE_RERUN_BUDGET_S),
            &mut |one, names, cap| {
                run_one_capped(&wt, one, &sandbox, &scratch_dir, Tree::Head, names, cap)
            },
        );
        assert_eq!(run.classification.as_deref(), Some(REGRESSION), "{run:?}");
        assert!(run.reruns.is_empty(), "not re-run alone: {:?}", run.reruns);
        assert!(run.denies());
        let zero = RepoCheck {
            argv: s(&[
                "cargo",
                "test",
                "foo",
                "--",
                "--test-threads=1",
                "--skip",
                "t::foo_aflaky",
                "--exact",
            ]),
            ..check.clone()
        };
        let z = run_one(&wt, &zero, &sandbox, &scratch_dir, Tree::Head);
        assert!(
            z.passed() && z.stdout_tail.contains("running 0 tests"),
            "codex's repro: {z:?}"
        );
        drop(scratch_dir);
        let _ = std::fs::remove_dir_all(&wt);
    }

    /// agy round 17: an escape written onto a failure line — `\x1b[31` before `test new`, where
    /// the stripper takes the `t` as the sequence's final byte, or a bare `\x1b` / `\x1b[` before
    /// `FAILED`, `FAIL`, `ERROR`, `Couldn't` — once lost the id. The line is now read raw,
    /// stripped and de-introduced, and the union kept: beside a pytest failure the base shares,
    /// every one denies.
    #[test]
    fn an_escape_before_any_failure_line_cannot_hide_it_1060() {
        let shared = "FAILED tests/t.py::test_old - red\n";
        for line in [
            "\u{1b}[31test new ... FAILED",
            "\u{1b}[test new ... FAILED",
            "\u{1b}FAILED tests/t.py::test_new - red",
            "\u{1b}[FAILED tests/t.py::test_new - red",
            "\u{1b}]FAILED tests/t.py::test_new - red",
            "\u{1b}FAIL a.test.ts > s > new",
            "\u{1b}[FAIL a.test.ts > s > new",
            "\u{1b}ERROR tests/test_new.py - ImportError",
            "\u{1b}[ERROR tests/test_new.py - ImportError",
            "\u{1b}[Couldn't compile the test.",
        ] {
            let run = scanned_head((&format!("{shared}{line}\n"), ""), (shared, ""));
            assert_eq!(
                run.classification.as_deref(),
                Some(REGRESSION),
                "{line:?}: {run:?}"
            );
            assert!(!run.regressions.is_empty() && run.denies(), "{line:?}");
        }
    }

    /// The ruling of round 17, for a cargo run the floor sees only as text (a wrapper:
    /// `sh -c 'cargo test'`): libtest's `test result: FAILED`, cargo's `… failed, to rerun pass`
    /// and libtest's `--quiet` line `name --- FAILED` each add an unattributed id (the quiet line
    /// names its test too) — so a wrapper's failed run is never pre-existing beside a shared
    /// pytest failure.
    #[test]
    fn a_wrapped_cargo_failure_is_never_pre_existing_1060() {
        let shared = "FAILED tests/t.py::test_old - red\n";
        for line in [
            "error: test failed, to rerun pass `--lib`",
            "error: doctest failed, to rerun pass `-p a --doc`",
            "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out",
            "t::new --- FAILED",
        ] {
            let run = scanned_head((shared, &format!("{line}\n")), (shared, ""));
            assert_eq!(
                run.classification.as_deref(),
                Some(REGRESSION),
                "{line:?}: {run:?}"
            );
            assert!(run
                .regressions
                .iter()
                .any(|r| r == &format!("{UNATTRIBUTED_FAILURE}cargo test failed")));
            assert!(run.denies());
        }
        let (ids, _, _) = qualify(scan_of("t::new --- FAILED\n"), LineScan::default());
        assert!(ids.contains(&"test t::new".to_string()), "{ids:?}");
    }

    /// codex round 17, with real cargo: `cargo test -- --quiet` prints `name --- FAILED`, not
    /// `test name ... FAILED`, and exits 101 with no other marker. The base fails `old_failure`,
    /// the head also fails `new_regression`, and both print a pytest-shaped line into their
    /// captured output that the two trees share. The argv-based id (a failed `cargo test` is a
    /// failure whatever its text says) and the quiet line keep the check a regression.
    #[cfg(unix)]
    #[test]
    fn a_quiet_cargo_failure_beside_a_shared_child_id_denies_1060() {
        let root = scratch("quiet1060");
        let crate_at = |dir: &Path, new_regression: bool| {
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(
                dir.join("Cargo.toml"),
                "[package]\nname = \"quiet_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
                 [lib]\npath = \"src/lib.rs\"\n[workspace]\n",
            )
            .unwrap();
            let body = |name: &str, fails: bool| {
                format!(
                    "    #[test]\n    fn {name}() {{\n        println!(\"FAILED \
                     tests/test_child.py::test_shared - child\");\n        assert!(!{fails});\n    \
                     }}\n"
                )
            };
            std::fs::write(
                dir.join("src/lib.rs"),
                format!(
                    "#[cfg(test)]\nmod t {{\n{}{}}}\n",
                    body("old_failure", true),
                    body("new_regression", new_regression)
                ),
            )
            .unwrap();
        };
        let (base_dir, head_dir) = (root.join("base"), root.join("head"));
        crate_at(&base_dir, false);
        crate_at(&head_dir, true);
        let check = RepoCheck {
            name: "test".into(),
            argv: s(&["cargo", "test", "--", "--quiet"]),
            source: "fixture".into(),
            timeout_s: Some(600),
            dir: String::new(),
        };
        let run_in = |dir: &Path| {
            let sandbox = sandbox_for(dir);
            if sandbox.level != SandboxLevel::Sandboxed {
                return None;
            }
            let scratch_dir = CheckScratch::prepare(dir).unwrap();
            Some(run_one(dir, &check, &sandbox, &scratch_dir, Tree::Head))
        };
        let (Some(base), Some(mut head)) = (run_in(&base_dir), run_in(&head_dir)) else {
            eprintln!("repo_checks: no sandbox tool here — the cargo fixture cannot run");
            let _ = std::fs::remove_dir_all(&root);
            return;
        };
        assert!(!base.passed() && !head.passed(), "{base:?} {head:?}");
        assert!(
            head.failure_ids
                .contains(&"FAILED tests/test_child.py::test_shared".to_string()),
            "the shared child id is read on the head: {:?} {}",
            head.failure_ids,
            head.stdout_tail
        );
        assert!(
            head.failure_ids
                .contains(&format!("{UNATTRIBUTED_FAILURE}cargo test exited 101")),
            "{:?}",
            head.failure_ids
        );
        classify(&mut head, base_of(base));
        assert_eq!(head.classification.as_deref(), Some(REGRESSION), "{head:?}");
        assert!(
            head.regressions
                .contains(&"test t::new_regression".to_string()),
            "the quiet line names it: {:?}",
            head.regressions
        );
        assert!(head.denies());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The argv-based id of a failed `cargo test` is answered for by the clean confirmation run
    /// like every other unattributed id: a real flake beside it is still `flaky_under_load`.
    #[test]
    fn a_flake_beside_the_cargo_exit_id_is_still_flaky_1060() {
        let exit = format!("{UNATTRIBUTED_FAILURE}cargo test exited 101");
        let (check, mut run) = regression_run(&["test t::flaky", &exit]);
        let mut argvs = Vec::new();
        recheck_alone(
            &check,
            &mut run,
            None,
            Duration::from_secs(FLAKE_RERUN_BUDGET_S),
            &mut confirming_rerun(0, &mut argvs),
        );
        assert_eq!(
            run.classification.as_deref(),
            Some(FLAKY_UNDER_LOAD),
            "{run:?}"
        );
        assert_eq!(run.regressions, vec!["test t::flaky"]);
        assert!(!run.denies());
        // The same run with a failing confirmation: a regression, the exit id kept.
        let (check, mut run) = regression_run(&["test t::flaky", &exit]);
        let mut argvs = Vec::new();
        recheck_alone(
            &check,
            &mut run,
            None,
            Duration::from_secs(FLAKE_RERUN_BUDGET_S),
            &mut confirming_rerun(101, &mut argvs),
        );
        assert_eq!(run.regressions, vec![exit, NO_CLEAN_RUN.to_string()]);
        assert!(run.denies());
    }

    /// codex / agy round 19: a parameterised partial CSI or OSC before a failure token (`\x1b[31ERROR`,
    /// `\x1b[0m\x1b]8;;FAILED …\x07`) defeats every prefix-anchored reading. A line with an ESC that
    /// mentions a failure token yet yields no id is an unattributed failure: beside a failure the
    /// base shares, every shape denies.
    #[test]
    fn an_escaped_failure_line_becomes_a_sentinel_1060() {
        let shared = "FAILED tests/t.py::test_old - red\n";
        for line in [
            "\u{1b}[31ERROR tests/test_new.py - ImportError",
            "\u{1b}[31FAILED tests/t.py::test_new - red",
            "\u{1b}[31FAIL a.test.ts > s > new",
            "\u{1b}[0m\u{1b}[FAILED tests/t.py::test_new - red",
            "\u{1b}[0m\u{1b}]8;;FAILED tests/t.py::test_new - red\u{7}",
            "\u{1b}[31--- FAIL: TestNew (0.00s)",
            "\u{1b}[1mcould not compile `x`",
        ] {
            let (ids, _, _) = qualify(scan_of(&format!("{line}\n")), LineScan::default());
            assert!(
                ids.contains(&ESCAPED_FAILURE_LINE.to_string())
                    || ids.iter().any(|i| !never_matched(i)),
                "{line:?}: {ids:?}"
            );
            let run = scanned_head((&format!("{shared}{line}\n"), ""), (shared, ""));
            assert_eq!(
                run.classification.as_deref(),
                Some(REGRESSION),
                "{line:?}: {run:?}"
            );
            assert!(run.denies(), "{line:?}");
        }
        // A plain line, or an escaped line with no failure token, adds nothing.
        let (ids, _, _) = qualify(
            scan_of("\u{1b}[32mtest ok ... ok\u{1b}[0m\nall good\n"),
            LineScan::default(),
        );
        assert!(ids.is_empty(), "{ids:?}");
    }

    /// agy round 19: a coloured libtest failure line yields its plain id and no junk (the deleted
    /// de-introduced reading turned SGR into ids like `test new0m`), so a coloured flake still
    /// recovers after a clean confirmation.
    #[test]
    fn a_coloured_flaky_libtest_line_still_recovers_1060() {
        let (ids, _, _) = qualify(
            scan_of(
                "test new ... \u{1b}[31mFAILED\u{1b}[0m\n\u{1b}[1mtest new ... \
                 \u{1b}[31mFAILED\u{1b}[0m\n",
            ),
            LineScan::default(),
        );
        assert_eq!(ids, vec!["test new"]);
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let (check, mut run) = regression_run(&refs);
        let mut argvs = Vec::new();
        recheck_alone(
            &check,
            &mut run,
            None,
            Duration::from_secs(FLAKE_RERUN_BUDGET_S),
            &mut confirming_rerun(0, &mut argvs),
        );
        assert_eq!(
            run.classification.as_deref(),
            Some(FLAKY_UNDER_LOAD),
            "{run:?}"
        );
        assert!(!run.denies());
    }

    /// codex / agy round 19: a `-q`/`--quiet` re-run never prints `test X ... ok`, so its witness
    /// cannot pass — a quiet cargo check is never re-run, and a flake in it stays a regression.
    #[test]
    fn a_quiet_cargo_check_is_never_rerun_1060() {
        for quiet in ["-q", "--quiet"] {
            let (mut check, mut run) = regression_run(&["test t::flaky"]);
            check.argv = s(&["cargo", "test", "--", quiet]);
            let mut argvs = Vec::new();
            recheck_alone(
                &check,
                &mut run,
                None,
                Duration::from_secs(FLAKE_RERUN_BUDGET_S),
                &mut confirming_rerun(0, &mut argvs),
            );
            assert!(argvs.is_empty(), "{argvs:?}");
            assert_eq!(run.not_rechecked, 1);
            assert_eq!(run.classification.as_deref(), Some(REGRESSION));
            assert!(run.denies());
        }
    }
}
