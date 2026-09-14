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
//! * `Cargo.toml` → `cargo test`.
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
//! are `pre_existing_in_sandbox` and never deny; identical base and head failure sets are a
//! `floor_env_mismatch` (the floor's environment, not the change, is the likely cause — recorded
//! with the floor's env so a CI mismatch is visible; the sandbox itself is NOT widened here); only
//! head-only failures are `regression`s and deny. A base that cannot be run or compared leaves the
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
//! ([`crate::validator::detect_worker_sandbox`] — macOS `sandbox-exec`, Linux `bwrap`; writes
//! confined to the worktree, the curated secret directories unreadable, network open because
//! installs need it), with an ISOLATED `HOME` and package caches under the worktree's engine
//! scratch (`<worktree>/tmp/wicked-checks/…` — `HOME`, `npm_config_cache`, `CARGO_HOME`,
//! `CARGO_TARGET_DIR`, `XDG_*`), so no check reads the operator's `~/.npmrc`,
//! `~/.cargo/config.toml` or credentials, nothing it writes lands outside the tree, and its build
//! artifacts (`target/`, a generated `package-lock.json` — `--no-package-lock` when the repo ships
//! none) never land in the reviewed tree either, where the worktree guard would deny them. `RUSTUP_HOME` is preserved so the toolchain
//! proxies still resolve. When NO write boundary can be armed — no `sandbox-exec`/`bwrap` on the
//! host (all of Windows), or the worktree root failing to canonicalize — the floor does NOT run
//! the checks: repo-controlled scripts never execute unsandboxed (codex review on #414). The
//! report FAILS with the probe's reason (`sandbox_error`, `sandbox_level: "best-effort"`), which
//! the gate turns into a denial an operator can read and act on (install the sandbox tool, or run
//! the verify phase where one exists).
//!
//! Two leaves are special. `TMPDIR` (with `TMP`/`TEMP`) is NOT under the worktree: it is a short
//! random per-floor directory (`<system temp>/wc-<6 hex>`, mode 0700), armed as the boundary's
//! second write root and reaped with the floor — a `TMPDIR` under a deep worktree overflowed the
//! Unix-socket `sun_path` limit (104 bytes on macOS) and manufactured `listen EINVAL` failures the
//! code did not have (core#489). `CARGO_TARGET_DIR` is split `cargo-target/head` vs
//! `cargo-target/base`, so cargo's freshness check can never hand a head check the base run's test
//! binaries through one shared target dir (core#480).
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
/// The most failure identifiers the streaming scanner keeps per check.
pub const MAX_FAILURE_IDS: usize = 1000;
/// Where the checks' isolated `HOME` and caches live: under the worktree's engine scratch, which
/// the worktree guard excludes from its snapshot by construction and the OS boundary contains.
pub const SCRATCH_SUBDIR: &str = "wicked-checks";
/// The optional per-repo check configuration, relative to the worktree root.
pub const CONFIG_PATH: &str = ".wicked/checks.json";

/// Check classifications on the wire (`CheckRun::classification`).
/// The head failure is absent on the base: the change broke it — denies.
pub const REGRESSION: &str = "regression";
/// Every head failure also fails on the base (and the base fails more): not this change's doing —
/// never denies.
pub const PRE_EXISTING_IN_SANDBOX: &str = "pre_existing_in_sandbox";
/// Base and head fail IDENTICALLY in the floor's sandbox: the floor's environment, not the
/// change, is the likely cause — never denies; recorded with the floor's env.
pub const FLOOR_ENV_MISMATCH: &str = "floor_env_mismatch";

/// Claim verdicts on the wire (`ClaimCheck::verdict`).
pub const CLAIM_REJECTED: &str = "claim_rejected";
pub const CLAIM_CONFIRMED: &str = "claim_confirmed";
pub const CLAIM_UNVERIFIED: &str = "unverified";

/// Denial sources the fold uses for this floor.
pub const DENIAL_SOURCE: &str = "repo_checks";
/// A floor that did not FINISH (a check hit its bound) — never "checks failed".
pub const DENIAL_SOURCE_TIMEOUT: &str = "repo_checks_timeout";

/// One check the floor detected: a name, the exact argv, and where it was read from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCheck {
    /// `install` | `typecheck` | `lint` | `test` | `test_targeted` | `cargo-test`.
    pub name: String,
    pub argv: Vec<String>,
    /// Provenance an operator can verify: `package.json scripts.test`, `Cargo.toml`,
    /// `.wicked/checks.json test_targeted`, …
    pub source: String,
    /// The repo-configured BASE bound (`timeout_s` in [`CONFIG_PATH`]); `None` ⇒ the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
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

    /// Does this check deny the unit? A failure the base shares (`pre_existing_in_sandbox`) or
    /// that the floor's environment produced on both trees (`floor_env_mismatch`) does NOT; a
    /// regression, a timeout, a spawn failure and an un-compared failure do.
    pub fn denies(&self) -> bool {
        !self.passed()
            && !matches!(
                self.classification.as_deref(),
                Some(PRE_EXISTING_IN_SANDBOX) | Some(FLOOR_ENV_MISMATCH)
            )
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
        self.checks
            .iter()
            .any(|c| c.classification.as_deref() == Some(FLOOR_ENV_MISMATCH))
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
                let line = format!("{line}{base_note}");
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
        self.checks
            .iter()
            .map(CheckRun::summary)
            .collect::<Vec<_>>()
            .join(", ")
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
            })
        }
    }
}

/// The paths the unit's change touched relative to the base commit — tracked changes (added,
/// copied, modified, renamed) plus untracked-not-ignored files, through the PINNED git dir; the
/// engine scratch is never a touched path. Empty when the run knows no base.
fn touched_files(worktree: &Path, ctx: &FloorContext) -> Result<Vec<String>, String> {
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
    let cfg = read_config(worktree)?.unwrap_or_default();
    let r_typecheck = resolve(&cfg.typecheck, "typecheck")?;
    let r_lint = resolve(&cfg.lint, "lint")?;
    let r_test = resolve(&cfg.test, "test")?;
    let r_targeted = resolve(&cfg.test_targeted, "test_targeted")?;
    let r_e2e = resolve(&cfg.e2e, "e2e")?;
    // `e2e` is a VERIFY-stage check only (the base run copies the stage, so never at the creator).
    let e2e = match r_e2e {
        Resolved::Command(argv) if ctx.stage == FloorStage::Verify => Some(RepoCheck {
            name: "e2e".into(),
            argv,
            source: format!("{CONFIG_PATH} e2e"),
            timeout_s: None,
        }),
        _ => None,
    };
    // A configured command (Node-shaped or not — the rule is the same for every configured slot)
    // needs the tree provisioned when a `package.json` is present and `node_modules` is missing:
    // `npx vitest …` resolves from node_modules.
    let configured_node = [&r_typecheck, &r_lint, &r_test, &r_targeted]
        .iter()
        .any(|r| matches!(r, Resolved::Command(_)))
        || e2e.is_some();
    let mut install: Option<RepoCheck> = None;
    let mut typecheck: Option<RepoCheck> = None;
    let mut lint: Option<RepoCheck> = None;
    let mut test: Option<RepoCheck> = None;
    let mut cargo: Option<RepoCheck> = None;
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
                });
            }
            for k in wanted {
                let check = RepoCheck {
                    name: k.to_string(),
                    argv: s(&[pm, "run", k]),
                    source: format!("package.json scripts.{k}"),
                    timeout_s: None,
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
        cargo = Some(RepoCheck {
            name: "cargo-test".into(),
            argv: if has_lock {
                s(&["cargo", "test", "--locked"])
            } else {
                s(&["cargo", "test"])
            },
            source: "Cargo.toml".into(),
            timeout_s: None,
        });
    }
    // The repo config speaks over the manifests.
    apply(&mut typecheck, r_typecheck, "typecheck");
    apply(&mut lint, r_lint, "lint");
    // A configured `test` replaces EVERY auto-detected test check (`test` and `cargo-test`);
    // `false` removes them.
    match r_test {
        Resolved::Default => {}
        Resolved::Disabled => {
            test = None;
            cargo = None;
        }
        Resolved::Command(argv) => {
            test = Some(RepoCheck {
                name: "test".into(),
                argv,
                source: format!("{CONFIG_PATH} test"),
                timeout_s: None,
            });
            cargo = None;
        }
    }
    // TARGETED FIRST (core#469): the declared change-scoped command stands in for the full test
    // set at the creator stage always, and at verify unless the repo says `full: true`. A command
    // anchored on `{files}` / `{base}` needs a known base; without one the full set runs.
    if let Resolved::Command(argv) = r_targeted {
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
            });
            cargo = None;
        }
    }
    // `e2e` LAST: after the test set, only where the stage admits it (see above).
    let mut out: Vec<RepoCheck> = [install, typecheck, lint, test, cargo, e2e]
        .into_iter()
        .flatten()
        .collect();
    if let Some(t) = cfg.timeout_s {
        for c in out.iter_mut().filter(|c| c.name != "install") {
            c.timeout_s = Some(t);
        }
    }
    Ok(out)
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
fn engine_generated_candidates(worktree: &Path, detected: &[RepoCheck]) -> Vec<&'static str> {
    let mut out = Vec::new();
    if detected
        .iter()
        .any(|c| c.name == "cargo-test" && !c.argv.iter().any(|a| a == "--locked"))
        && !worktree.join("Cargo.lock").exists()
    {
        out.push("Cargo.lock");
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

/// The isolated homes the checks run with, all under the worktree's engine scratch.
#[derive(Debug)]
pub(crate) struct CheckScratch {
    root: PathBuf,
    /// The checks' `TMPDIR`: `<system temp>/wc-<6 hex>`, private (0700), drawn fresh per floor and
    /// reaped with it (core#489 — a socket path must stay short; the worktree may not be).
    tmp: PrivateTmp,
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

/// The scratch leaves under `<worktree>/tmp/wicked-checks/`. No `tmp` leaf: the checks' `TMPDIR`
/// lives under the system temp dir (see [`CheckScratch::tmp`]).
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
    pub(crate) fn prepare(worktree: &Path) -> std::io::Result<Self> {
        // The engine scratch root is repo-adjacent territory: a checkout could ship `tmp` as a
        // SYMLINK pointing outside the worktree, and `create_dir_all` would follow it. Refuse a
        // link at either level (adversarial review on #414) — lstat, never follow.
        let scratch_root = worktree.join(crate::worktree_guard::ENGINE_SCRATCH_DIR);
        for dir in [&scratch_root, &scratch_root.join(SCRATCH_SUBDIR)] {
            match std::fs::symlink_metadata(dir) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "`{}` is a symlink — the checks' scratch must be a real directory \
                             inside the worktree (refused, never followed)",
                            dir.display()
                        ),
                    ))
                }
                Ok(m) if !m.is_dir() => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("`{}` exists and is not a directory", dir.display()),
                    ))
                }
                _ => {}
            }
        }
        let root = scratch_root.join(SCRATCH_SUBDIR);
        // The leaves too (adversarial review on #414, LOW): `create_dir_all` would follow a
        // committed symlink-to-directory at a leaf. Refuse a link at any leaf before creating.
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
        Ok(Self { root, tmp })
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
            .env("FORCE_COLOR", "0");
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
    // The scratch FIRST: its short `TMPDIR` under the system temp dir is the boundary's SECOND
    // write root (core#489), and the launcher needs it to exist before the probe — bwrap `--bind`s
    // a directory, the SBPL profile canonicalizes it. A scratch that cannot be prepared is the
    // same fail-closed detection error as before, reported against a worktree-only probe.
    let scratch = match CheckScratch::prepare(worktree) {
        Ok(s) => s,
        Err(e) => {
            let sandbox = crate::validator::detect_worker_sandbox(&[worktree.to_path_buf()]);
            return scratch_refused(worktree, ctx, &sandbox, &e);
        }
    };
    let sandbox = crate::validator::detect_worker_sandbox(&[
        worktree.to_path_buf(),
        scratch.tmp.path().to_path_buf(),
    ]);
    run_with_sandbox_ctx(worktree, sandbox, ctx, scratch)
}

/// [`run_floor`] against an explicit sandbox probe — the injectable seam, so the fail-closed branch
/// is testable on a host that HAS a sandbox tool by handing it a best-effort probe.
#[cfg(test)]
pub(crate) fn run_with_sandbox(worktree: &Path, sandbox: WorkerSandbox) -> RepoChecksReport {
    let ctx = FloorContext::default();
    match CheckScratch::prepare(worktree) {
        Ok(scratch) => run_with_sandbox_ctx(worktree, sandbox, &ctx, scratch),
        Err(e) => scratch_refused(worktree, &ctx, &sandbox, &e),
    }
}

/// The fail-closed report for a scratch that could not be prepared (a symlinked `tmp`, an
/// unwritable temp dir): detection is still reported so the record says what WOULD have run.
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
            "the checks' isolated scratch under `{}/{SCRATCH_SUBDIR}` could not be created: {e}",
            crate::worktree_guard::ENGINE_SCRATCH_DIR
        )),
        sandbox_level: sandbox.level.as_wire().to_string(),
        sandbox_note: sandbox.downgrade_reason.clone(),
        sandbox_error: None,
        engine_writes_removed: Vec::new(),
        claim: None,
        env: None,
    }
}

pub(crate) fn run_with_sandbox_ctx(
    worktree: &Path,
    sandbox: WorkerSandbox,
    ctx: &FloorContext,
    scratch: CheckScratch,
) -> RepoChecksReport {
    let sandbox_level = sandbox.level.as_wire().to_string();
    let sandbox_note = sandbox.downgrade_reason.clone();
    if sandbox.level != crate::validator::SandboxLevel::Sandboxed || sandbox.wrapper.is_empty() {
        // NEVER run repo-controlled scripts unsandboxed (codex review on #414): the floor fails
        // with the probe's own reason, and the gate turns that into a denial the operator can act
        // on. Detection is still reported so the record says what WOULD have run — and a
        // detection FAILURE is reported as such, never as "no checks" (Copilot on #414).
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
                "no OS write boundary could be armed for the checks ({})",
                sandbox_note.unwrap_or_else(|| "no OS-sandbox tool on PATH".to_string())
            )),
            engine_writes_removed: Vec::new(),
            claim: None,
            env: None,
        };
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
            }
        }
    };
    let candidates = engine_generated_candidates(worktree, &detected);
    let env = scratch.env_record(&sandbox_level);
    let baseline_diff = baseline_diff_enabled(worktree);
    let mut checks = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = false;
    // (F-RC2-009) Never run a HEAD check beside a copy of the base — a crash mid-floor could have
    // left one; the export lives only between its creation and the caching of its result.
    if let Err(e) = remove_base_export(&scratch) {
        eprintln!("wicked-core: repo checks floor — {e}");
    }
    for check in &detected {
        if failed {
            skipped.push(check.name.clone());
            continue;
        }
        let mut run = run_one(worktree, check, &sandbox, &scratch, Tree::Head);
        // BASELINE-DIFF (F-RC2-009): a FAILURE (not a timeout, not a spawn failure, not the
        // install — those say nothing about the base) is compared against the run base before it
        // may deny. No known base ⇒ the failure denies as it always did.
        if !run.passed() && run.spawn_error.is_none() && !run.timed_out && check.name != "install" {
            match (ctx.base_head.as_deref(), ctx.git_dir.as_deref()) {
                (Some(head), _) if !baseline_diff => {
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
                            if let Err(e) = remove_base_export(&scratch) {
                                eprintln!("wicked-core: repo checks floor — {e}");
                            }
                            base_run
                        }
                    };
                    classify(&mut run, base_run);
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
        let p = worktree.join(rel);
        if let Ok(m) = std::fs::symlink_metadata(&p) {
            if m.is_file() && std::fs::remove_file(&p).is_ok() {
                engine_writes_removed.push(rel.to_string());
            }
        }
    }
    RepoChecksReport {
        passed: !failed,
        detected,
        checks,
        skipped,
        detect_error: None,
        sandbox_level,
        sandbox_note,
        sandbox_error: None,
        engine_writes_removed,
        claim,
        env: Some(env),
    }
}

/// The run base, exported into the checks' scratch for the baseline diff — plain files, no git
/// metadata, no nested worktree: `git read-tree` into a scratch index + `git checkout-index
/// --prefix`, both through the PINNED git dir (never the worktree's own `.git` file, which the
/// seat could have redirected). Lives under `<worktree>/tmp/wicked-checks/base` — inside the OS
/// write boundary, outside the guard's snapshot — so the sandbox wrapper armed for the worktree
/// covers the base run too. REMOVED as soon as its check result is cached ([`remove_base_export`]):
/// the export is a full copy of the repo tree inside the worktree, and a HEAD check that globs from
/// the worktree root (vitest's default include, a broad `tsconfig`, eslint without ignores) would
/// otherwise run over the base's files too. The run pays for a base check once through the
/// `base-cache`, never through a persisted export.
struct BaseTree {
    dir: PathBuf,
    head: String,
}

/// Remove the base export (`<scratch>/base` and its scratch index) if present. A symlink at the
/// export path is refused, never followed. Called before an export and as soon as a base check's
/// result is cached, so no HEAD check ever runs beside a copy of the base.
fn remove_base_export(scratch: &CheckScratch) -> Result<(), String> {
    let dir = scratch.root.join("base");
    let _ = std::fs::remove_file(scratch.root.join("base.idx"));
    match std::fs::symlink_metadata(&dir) {
        Err(_) => Ok(()),
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

impl BaseTree {
    fn cache_path(scratch: &CheckScratch, head: &str, name: &str) -> PathBuf {
        scratch
            .root
            .join("base-cache")
            .join(format!("{}-{name}.json", &head[..head.len().min(12)]))
    }

    /// This run's cached result of `check` on `head`, when an earlier floor of the run paid for it.
    fn cached(scratch: &CheckScratch, head: &str, check: &RepoCheck) -> Option<BaseRun> {
        let raw = std::fs::read_to_string(Self::cache_path(scratch, head, &check.name)).ok()?;
        let run = serde_json::from_str::<CheckRun>(&raw).ok()?;
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
        let dir = scratch.root.join("base");
        // Always a fresh export: a stale one is removed, a link refused — never followed.
        remove_base_export(scratch)?;
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

    /// Run `check` on the base — by NAME, as the base's own detection spells it (a targeted
    /// command keeps the head's substituted argv: it is the same change-scoped set on both
    /// trees) — and cache the result under `base-cache/<head>-<name>.json` for the rest of the
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
        let cache = Self::cache_path(scratch, &self.head, &check.name);
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
        let detected = match detect_with(&self.dir, &base_ctx) {
            Ok(d) => d,
            Err(e) => return fail(format!("the base's checks could not be determined: {e}")),
        };
        let target = if check.name == "test_targeted" {
            check.clone()
        } else {
            match detected.iter().find(|c| c.name == check.name) {
                Some(c) => c.clone(),
                None => {
                    return fail(format!(
                        "the run base declares no `{}` check (the change introduced it)",
                        check.name
                    ))
                }
            }
        };
        if let Some(install) = detected.iter().find(|c| c.name == "install") {
            let r = run_one(&self.dir, install, sandbox, scratch, Tree::Base);
            if !r.passed() {
                return fail(format!(
                    "the base's dependency install did not pass ({})",
                    r.summary()
                ));
            }
        }
        let run = run_one(&self.dir, &target, sandbox, scratch, Tree::Base);
        let _ = std::fs::create_dir_all(scratch.root.join("base-cache"));
        if let Ok(json) = serde_json::to_string(&run) {
            let _ = std::fs::write(&cache, json);
        }
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
/// * both failed with identifiers on both sides ⇒ set arithmetic: any head-only identifier ⇒
///   `regression` (the shared ones listed as `pre_existing`); equal sets ⇒ `floor_env_mismatch`;
///   head ⊂ base ⇒ `pre_existing_in_sandbox`;
/// * both failed with NO identifiers on either side ⇒ compared by exit code: equal ⇒
///   `floor_env_mismatch` (the observable failure is identical), else `regression`;
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
                (true, true) => {
                    if head.exit_code == b.exit_code {
                        Some(FLOOR_ENV_MISMATCH)
                    } else {
                        Some(REGRESSION)
                    }
                }
                (false, false) => {
                    head.pre_existing = head_ids
                        .intersection(&base_ids)
                        .map(|s| s.to_string())
                        .collect();
                    head.regressions = head_ids
                        .difference(&base_ids)
                        .map(|s| s.to_string())
                        .collect();
                    if !head.regressions.is_empty() {
                        Some(REGRESSION)
                    } else if head_ids == base_ids {
                        Some(FLOOR_ENV_MISMATCH)
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
        Some(REGRESSION) => CLAIM_REJECTED,
        Some(PRE_EXISTING_IN_SANDBOX) | Some(FLOOR_ENV_MISMATCH) => CLAIM_CONFIRMED,
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

/// A bounded tail buffer: keeps the last [`TAIL_BYTES`] bytes of a stream.
/// How long to wait for a check's stdout/stderr to reach EOF after the process group is dead. A
/// detached descendant (`setsid`, a Node `detached` spawn with inherited stdio) can hold the pipe
/// open forever; the drain is DETACHED after this and the tail read so far is what gets reported
/// (adversarial review on #414 — an unbounded join wedged the verify unit).
const DRAIN_CAP: Duration = Duration::from_secs(5);

/// A stdout/stderr drain: the bounded tail accumulates in `buf` (shared, so a drain that never
/// reaches EOF still yields what it saw), failure identifiers scanned off complete lines
/// accumulate in `ids`, `done` fires at EOF.
struct Drain {
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    ids: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    done: std::sync::mpsc::Receiver<()>,
}

impl Drain {
    /// The tail read so far and the failure identifiers seen, waiting at most `cap` for EOF; on
    /// timeout the reader thread is left to die with the pipe (it holds only its own handles).
    fn finish(self, cap: Duration) -> (Vec<u8>, Vec<String>) {
        let _ = self.done.recv_timeout(cap);
        let mut tail = std::mem::take(&mut *self.buf.lock().unwrap_or_else(|p| p.into_inner()));
        if tail.len() > TAIL_BYTES {
            let cut = tail.len() - TAIL_BYTES;
            tail.drain(..cut);
        }
        let ids = std::mem::take(&mut *self.ids.lock().unwrap_or_else(|p| p.into_inner()));
        (tail, ids)
    }
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
    None
}

fn drain_tail<R: Read + Send + 'static>(mut r: R) -> Drain {
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::with_capacity(TAIL_BYTES * 2)));
    let ids = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (done_tx, done) = std::sync::mpsc::channel();
    let shared = std::sync::Arc::clone(&buf);
    let shared_ids = std::sync::Arc::clone(&ids);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        let mut pending: Vec<u8> = Vec::new();
        let mut eslint_file: Option<String> = None;
        let scan = |line: &[u8], eslint_file: &mut Option<String>| {
            let line = String::from_utf8_lossy(line);
            if let Some(id) = failure_id_of_line(&line, eslint_file) {
                let mut ids = shared_ids.lock().unwrap_or_else(|p| p.into_inner());
                if ids.len() < MAX_FAILURE_IDS && !ids.contains(&id) {
                    ids.push(id);
                }
            }
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
                        scan(&line[..line.len() - 1], &mut eslint_file);
                    }
                    if pending.len() > TAIL_BYTES {
                        let cut = pending.len() - TAIL_BYTES;
                        pending.drain(..cut);
                    }
                }
            }
        }
        if !pending.is_empty() {
            scan(&pending, &mut eslint_file);
        }
        let _ = done_tx.send(());
    });
    Drain { buf, ids, done }
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
    };
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
    // Resolve on PATH the way `Command::new` will, so "not installed" is a legible spawn error
    // rather than a bare OS code.
    let Some(exe) = crate::validator::find_on_path(bin) else {
        result.spawn_error = Some(format!("`{bin}` is not on PATH"));
        result.duration_ms = started.elapsed().as_millis() as u64;
        return result;
    };
    // `[<sandbox wrapper…>] <exe> <args…>` — an empty wrapper is the disclosed best-effort floor.
    let mut full: Vec<String> = sandbox.wrapper.clone();
    full.push(exe.to_string_lossy().into_owned());
    full.extend(check.argv[1..].iter().cloned());
    // spawn-audit: hardened — the repository's own check command, run in the run's worktree.
    let mut cmd = Command::new(&full[0]);
    cmd.hardened()
        .args(&full[1..])
        .current_dir(worktree)
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
            return result;
        }
    };
    let out_h = child.stdout.take().map(drain_tail);
    let err_h = child.stderr.take().map(drain_tail);
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
    let (out_tail, out_ids) = out_h.map(|d| d.finish(DRAIN_CAP)).unwrap_or_default();
    let (err_tail, err_ids) = err_h.map(|d| d.finish(DRAIN_CAP)).unwrap_or_default();
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
    // stderr), de-duplicated, in order of first sight.
    let mut ids = out_ids;
    for id in err_ids {
        if !ids.contains(&id) && ids.len() < MAX_FAILURE_IDS {
            ids.push(id);
        }
    }
    result.failure_ids = ids;
    result.duration_ms = started.elapsed().as_millis() as u64;
    result
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

    fn sandbox_for(wt: &Path) -> WorkerSandbox {
        crate::validator::detect_worker_sandbox(&[wt.to_path_buf()])
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
        assert_eq!(checks[0].argv, s(&["pnpm", "run", "lint"]));
        assert_eq!(checks[1].name, "cargo-test");
        assert_eq!(checks[1].argv, s(&["cargo", "test"]));
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
        let c = &report.checks[0];
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
        // The isolated homes AND the build artifacts live under the engine scratch, never in the
        // reviewed tree (Copilot on #414: `./target` would trip the worktree guard).
        let scratch = wt
            .join(crate::worktree_guard::ENGINE_SCRATCH_DIR)
            .join(SCRATCH_SUBDIR);
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
    #[test]
    fn without_a_write_boundary_the_floor_fails_closed_and_runs_nothing() {
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
            vec!["cargo-test"],
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
    }

    /// The boundary itself, where the host has one: a check script that writes outside the
    /// worktree fails the floor and the write never lands; the operator's real HOME is not the
    /// check's HOME. On a host without a sandbox tool this asserts the fail-closed path instead
    /// (never a skip that reads as a pass).
    #[cfg(unix)]
    #[test]
    fn a_check_that_writes_outside_the_worktree_fails_the_floor() {
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
        // The allow-list and the isolation overrides ARE there.
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
        // And the worktree scratch carries no `tmp` leaf any more.
        assert!(!wt
            .join(crate::worktree_guard::ENGINE_SCRATCH_DIR)
            .join(SCRATCH_SUBDIR)
            .join("tmp")
            .exists());
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
    /// checks' scratch created through it.
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
            "nothing was created through the link"
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
        assert_eq!(checks[0].argv, s(&["cargo", "test", "--locked"]));
        assert!(engine_generated_candidates(&wt, &checks).is_empty());
        std::fs::remove_file(wt.join("Cargo.lock")).unwrap();
        let checks = detect(&wt).unwrap();
        assert_eq!(checks[0].argv, s(&["cargo", "test"]));
        assert_eq!(
            engine_generated_candidates(&wt, &checks),
            vec!["Cargo.lock"]
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
            },
            RepoCheck {
                name: "test".into(),
                argv: s(&["definitely-not-a-binary-wicked-xyz", "run"]),
                source: "fixture".into(),
                timeout_s: None,
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
        std::fs::create_dir_all(wt.join("tmp/wicked-checks")).unwrap();
        std::fs::write(wt.join("tmp/wicked-checks/x"), "scratch").unwrap();
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
    #[test]
    fn baseline_diff_denies_only_regressions() {
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
            let t = |ok: bool| if ok { "" } else { "assert!(false, \"BOOM\");" };
            format!(
                "#[cfg(test)]\nmod t {{\n    #[test]\n    fn shared() {{ {} }}\n    #[test]\n    \
                 fn other() {{ {} }}\n    #[test]\n    fn stable() {{ {} }}\n}}\n",
                t(shared),
                t(other),
                t(stable)
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

        // (1) REGRESSION: `stable` newly fails beside the two shared failures.
        std::fs::write(repo.join("src/lib.rs"), body(false, false, false)).unwrap();
        let report = run_floor(&repo, &ctx);
        if report.sandbox_error.is_some() {
            eprintln!("repo_checks: no sandbox tool here — the baseline diff cannot run");
            return;
        }
        assert!(!report.passed, "{report:?}");
        assert_eq!(report.outcome(), "failed");
        assert_eq!(report.denial_source(), DENIAL_SOURCE);
        let c = &report.checks[0];
        assert_eq!(c.name, "cargo-test");
        assert_eq!(c.classification.as_deref(), Some(REGRESSION), "{c:?}");
        assert_eq!(c.regressions, vec!["test t::stable".to_string()]);
        assert_eq!(
            c.pre_existing,
            vec!["test t::other".to_string(), "test t::shared".to_string()]
        );
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
        let mut base_ids = br.failure_ids.clone();
        base_ids.sort();
        assert_eq!(
            base_ids,
            vec!["test t::other".to_string(), "test t::shared".to_string()]
        );
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
                && denial.contains("head-only failures test t::stable")
                && denial.contains("also failing on the base: test t::other, test t::shared")
                && denial.contains("REJECTED"),
            "{denial}"
        );
        let env = report
            .env
            .as_ref()
            .expect("the floor's env is on the record");
        assert!(env.home.ends_with("home") && env.network == "open" && !env.path.is_empty());
        let scratch_root = repo
            .join(crate::worktree_guard::ENGINE_SCRATCH_DIR)
            .join(SCRATCH_SUBDIR);
        assert!(
            !scratch_root.join("base").exists(),
            "the base export is removed as soon as its result is cached — a HEAD check never \
             runs beside a copy of the base"
        );
        assert!(
            scratch_root
                .join("base-cache")
                .join(format!("{}-cargo-test.json", &base[..12]))
                .is_file(),
            "the base run is cached by base sha + check name"
        );

        // (2) FLOOR_ENV_MISMATCH: the head fails exactly what the base fails ⇒ the floor passes,
        // the shared failures are listed, the base run comes back from the cache.
        std::fs::write(repo.join("src/lib.rs"), body(false, false, true)).unwrap();
        let report = run_floor(&repo, &ctx);
        assert!(report.passed, "{:?}", report.checks[0]);
        assert_eq!(report.outcome(), "passed");
        let c = &report.checks[0];
        assert!(!c.passed() && !c.denies());
        assert_eq!(c.outcome(), "failed");
        assert_eq!(c.classification.as_deref(), Some(FLOOR_ENV_MISMATCH));
        assert_eq!(
            c.pre_existing,
            vec!["test t::other".to_string(), "test t::shared".to_string()]
        );
        assert!(c.regressions.is_empty());
        assert!(
            c.base.as_deref().is_some_and(|b| b.cached),
            "the base run is paid for once per run"
        );
        assert!(report.env_mismatch());
        assert_eq!(
            report.claim.as_ref().map(|c| c.verdict.as_str()),
            Some(CLAIM_CONFIRMED)
        );
        assert!(
            c.summary().contains("[floor_env_mismatch]"),
            "{}",
            c.summary()
        );

        // (3) PRE_EXISTING_IN_SANDBOX: the head fixes `other`, breaks nothing ⇒ passes.
        std::fs::write(repo.join("src/lib.rs"), body(false, true, true)).unwrap();
        let report = run_floor(&repo, &ctx);
        assert!(report.passed, "{:?}", report.checks[0]);
        let c = &report.checks[0];
        assert_eq!(c.classification.as_deref(), Some(PRE_EXISTING_IN_SANDBOX));
        assert_eq!(c.pre_existing, vec!["test t::shared".to_string()]);
        assert!(c.regressions.is_empty() && !c.denies());
        assert!(!report.env_mismatch());

        // (4) Opt-out: the plain denial, with the reason the base was not compared.
        std::fs::create_dir_all(repo.join(".wicked")).unwrap();
        std::fs::write(repo.join(CONFIG_PATH), r#"{"baseline_diff":false}"#).unwrap();
        let report = run_floor(&repo, &ctx);
        assert!(!report.passed);
        let c = &report.checks[0];
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
}
