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
//! emits `repoChecksEvaluated`, and DENIES the unit when any check exits non-zero, times out, or
//! cannot be spawned (fail-closed — a check that cannot run has re-derived nothing). Checks stop at
//! the first failure: the evidence of the failure is what the gate needs, and a failing typecheck
//! makes the suite behind it moot.
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

/// Per-check wall-clock bound. A real suite can take minutes; a check still running after this
/// long is killed with its process tree and recorded as timed out (a denial).
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Bound for the dependency install step, when one is needed.
pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How much of each stream's TAIL is kept as evidence.
pub const TAIL_BYTES: usize = 4096;
/// Where the checks' isolated `HOME` and caches live: under the worktree's engine scratch, which
/// the worktree guard excludes from its snapshot by construction and the OS boundary contains.
pub const SCRATCH_SUBDIR: &str = "wicked-checks";

/// One check the floor detected: a name, the exact argv, and where it was read from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCheck {
    /// `install` | `typecheck` | `lint` | `test` | `cargo-test`.
    pub name: String,
    pub argv: Vec<String>,
    /// Provenance an operator can verify: `package.json scripts.test`, `Cargo.toml`, …
    pub source: String,
}

impl RepoCheck {
    fn timeout(&self) -> Duration {
        if self.name == "install" {
            INSTALL_TIMEOUT
        } else {
            CHECK_TIMEOUT
        }
    }
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
}

impl CheckRun {
    pub fn passed(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && self.spawn_error.is_none()
    }

    /// One line an operator can read: `test: exit 1 (154.9s)`.
    pub fn summary(&self) -> String {
        let secs = self.duration_ms as f64 / 1000.0;
        if let Some(e) = &self.spawn_error {
            return format!("{}: could not run ({e})", self.name);
        }
        if self.timed_out {
            return format!(
                "{}: TIMED OUT after {secs:.1}s (killed at the {}s bound)",
                self.name,
                self.timeout_secs()
            );
        }
        match self.exit_code {
            Some(c) => format!("{}: exit {c} ({secs:.1}s)", self.name),
            None => format!("{}: no exit status ({secs:.1}s)", self.name),
        }
    }

    fn timeout_secs(&self) -> u64 {
        if self.name == "install" {
            INSTALL_TIMEOUT.as_secs()
        } else {
            CHECK_TIMEOUT.as_secs()
        }
    }
}

/// Everything the floor observed for one unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoChecksReport {
    /// What was detected, in run order (empty when detection failed — see `detect_error`).
    pub detected: Vec<RepoCheck>,
    /// What actually ran (a prefix of `detected` — the floor stops at the first failure).
    pub checks: Vec<CheckRun>,
    /// Detected checks that never ran because an earlier one failed.
    pub skipped: Vec<String>,
    /// True iff detection succeeded and every detected check ran and exited 0 (vacuously true when
    /// nothing was detected — see the module doc; the event discloses `checks: []`).
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
}

impl RepoChecksReport {
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
            .filter(|c| !c.passed())
            .map(|c| {
                // Both streams: a test runner puts the failing assertion on stdout and the
                // "error: test failed" line on stderr, and an operator needs to see either.
                let out = last_lines(&c.stdout_tail);
                let err = last_lines(&c.stderr_tail);
                match (out.is_empty(), err.is_empty()) {
                    (true, true) => c.summary(),
                    (false, true) => format!("{} — stdout tail: {out}", c.summary()),
                    (true, false) => format!("{} — stderr tail: {err}", c.summary()),
                    (false, false) => {
                        format!("{} — stdout tail: {out} — stderr tail: {err}", c.summary())
                    }
                }
            })
            .collect();
        let mut s = format!(
            "repo checks floor failed: {CRITERION}. {}",
            failed.join("; ")
        );
        if !self.skipped.is_empty() {
            s.push_str(&format!(
                " (not run after the failure: {})",
                self.skipped.join(", ")
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

/// A probed worktree entry: present, not a symlink, and opened without following links.
struct Probed {
    meta: std::fs::Metadata,
    file: std::fs::File,
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
    Ok(Some(Probed { meta, file }))
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

/// Detect the repository's own checks in `worktree`. Pure over the filesystem — runs nothing.
/// `Err` when a manifest exists but cannot be trusted (unreadable, malformed, a symlink).
pub fn detect(worktree: &Path) -> Result<Vec<RepoCheck>, String> {
    let mut out = Vec::new();
    if let Some(mut probed) = probe(worktree, "package.json")? {
        if !probed.is_file() {
            return Err("`package.json` is not a regular file".to_string());
        }
        // Read from the descriptor the probe opened — never the path again.
        let mut raw = String::new();
        probed
            .file
            .read_to_string(&mut raw)
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
        if !wanted.is_empty() {
            let node_modules = probe(worktree, "node_modules")?;
            if node_modules.is_none() {
                let has_lock = probe(worktree, "package-lock.json")?.is_some();
                let (argv, source) = match pm {
                    "pnpm" => (
                        s(&["pnpm", "install", "--frozen-lockfile", "--ignore-scripts"]),
                        "pnpm-lock.yaml (node_modules absent)",
                    ),
                    "yarn" => (
                        s(&["yarn", "install", "--frozen-lockfile", "--ignore-scripts"]),
                        "yarn.lock (node_modules absent)",
                    ),
                    _ if has_lock => (
                        s(&["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"]),
                        "package-lock.json (node_modules absent)",
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
                        "package.json (node_modules absent, no lockfile)",
                    ),
                };
                out.push(RepoCheck {
                    name: "install".into(),
                    argv,
                    source: source.into(),
                });
            }
            for k in wanted {
                out.push(RepoCheck {
                    name: k.to_string(),
                    argv: s(&[pm, "run", k]),
                    source: format!("package.json scripts.{k}"),
                });
            }
        }
    }
    if let Some(probed) = probe(worktree, "Cargo.toml")? {
        if !probed.is_file() {
            return Err("`Cargo.toml` is not a regular file".to_string());
        }
        out.push(RepoCheck {
            name: "cargo-test".into(),
            argv: s(&["cargo", "test"]),
            source: "Cargo.toml".into(),
        });
    }
    Ok(out)
}

/// The isolated homes the checks run with, all under the worktree's engine scratch.
#[derive(Debug)]
pub(crate) struct CheckScratch {
    root: PathBuf,
}

impl CheckScratch {
    pub(crate) fn prepare(worktree: &Path) -> std::io::Result<Self> {
        let root = worktree
            .join(crate::worktree_guard::ENGINE_SCRATCH_DIR)
            .join(SCRATCH_SUBDIR);
        for sub in [
            "home",
            "npm-cache",
            "cargo-home",
            "cargo-target",
            "xdg-config",
            "xdg-cache",
            "tmp",
        ] {
            std::fs::create_dir_all(root.join(sub))?;
        }
        Ok(Self { root })
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// Apply the isolation env to a check command. The daemon's environment is inherited through
    /// the spawn chokepoint (`hardened()` strips every engine-internal variable) and then every
    /// home-shaped variable is redirected under the scratch, so the check reads none of the
    /// operator's per-user configuration and writes nothing outside the worktree.
    fn apply_env(&self, cmd: &mut Command) {
        let real_home = std::env::var_os("HOME");
        cmd.env("HOME", self.home())
            .env("TMPDIR", self.root.join("tmp"))
            .env("TMP", self.root.join("tmp"))
            .env("TEMP", self.root.join("tmp"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("XDG_CACHE_HOME", self.root.join("xdg-cache"))
            .env("npm_config_cache", self.root.join("npm-cache"))
            .env("npm_config_update_notifier", "false")
            .env("npm_config_fund", "false")
            .env("npm_config_audit", "false")
            .env("CARGO_HOME", self.root.join("cargo-home"))
            // Build artifacts go under the scratch, not `./target`: a repo that does not ignore
            // `target/` would otherwise fail the worktree guard's final comparison on a PASSING
            // `cargo test` (Copilot on #414).
            .env("CARGO_TARGET_DIR", self.root.join("cargo-target"))
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("FORCE_COLOR", "0");
        // The toolchain proxies (`cargo`, `rustc` under rustup) resolve toolchains through
        // `RUSTUP_HOME`, which defaults to `$HOME/.rustup`. Moving HOME must not lose them: pin the
        // real location explicitly when the daemon did not.
        if std::env::var_os("RUSTUP_HOME").is_none() {
            if let Some(h) = real_home {
                let rustup = Path::new(&h).join(".rustup");
                if rustup.is_dir() {
                    cmd.env("RUSTUP_HOME", rustup);
                }
            }
        }
    }
}

/// Detect and run the checks in `worktree`, stopping at the first failure. Fail-closed on a
/// detection error and when no OS write boundary can be armed (see the module doc).
pub fn run(worktree: &Path) -> RepoChecksReport {
    let sandbox = crate::validator::detect_worker_sandbox(&[worktree.to_path_buf()]);
    run_with_sandbox(worktree, sandbox)
}

/// [`run`] against an explicit sandbox probe — the injectable seam, so the fail-closed branch is
/// testable on a host that HAS a sandbox tool by handing it a best-effort probe.
pub(crate) fn run_with_sandbox(worktree: &Path, sandbox: WorkerSandbox) -> RepoChecksReport {
    let sandbox_level = sandbox.level.as_wire().to_string();
    let sandbox_note = sandbox.downgrade_reason.clone();
    if sandbox.level != crate::validator::SandboxLevel::Sandboxed || sandbox.wrapper.is_empty() {
        // NEVER run repo-controlled scripts unsandboxed (codex review on #414): the floor fails
        // with the probe's own reason, and the gate turns that into a denial the operator can act
        // on. Detection is still reported so the record says what WOULD have run.
        return RepoChecksReport {
            detected: detect(worktree).unwrap_or_default(),
            checks: Vec::new(),
            skipped: Vec::new(),
            passed: false,
            detect_error: None,
            sandbox_level,
            sandbox_note: sandbox_note.clone(),
            sandbox_error: Some(format!(
                "no OS write boundary could be armed for the checks ({})",
                sandbox_note.unwrap_or_else(|| "no OS-sandbox tool on PATH".to_string())
            )),
        };
    }
    let detected = match detect(worktree) {
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
            }
        }
    };
    let scratch = match CheckScratch::prepare(worktree) {
        Ok(s) => s,
        Err(e) => {
            return RepoChecksReport {
                detected,
                checks: Vec::new(),
                skipped: Vec::new(),
                passed: false,
                detect_error: Some(format!(
                    "the checks' isolated scratch under `{}/{SCRATCH_SUBDIR}` could not be \
                     created: {e}",
                    crate::worktree_guard::ENGINE_SCRATCH_DIR
                )),
                sandbox_level,
                sandbox_note,
                sandbox_error: None,
            }
        }
    };
    let mut checks = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = false;
    for check in &detected {
        if failed {
            skipped.push(check.name.clone());
            continue;
        }
        let run = run_one(worktree, check, &sandbox, &scratch);
        failed = !run.passed();
        checks.push(run);
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
    }
}

/// A bounded tail buffer: keeps the last [`TAIL_BYTES`] bytes of a stream.
fn drain_tail<R: Read + Send + 'static>(mut r: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut tail: Vec<u8> = Vec::with_capacity(TAIL_BYTES * 2);
        let mut buf = [0u8; 8192];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > TAIL_BYTES * 2 {
                        let cut = tail.len() - TAIL_BYTES;
                        tail.drain(..cut);
                    }
                }
            }
        }
        if tail.len() > TAIL_BYTES {
            let cut = tail.len() - TAIL_BYTES;
            tail.drain(..cut);
        }
        tail
    })
}

fn lossy(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Run one check in `worktree` under its timeout, inside `sandbox`'s write boundary with the
/// isolated `scratch` homes, capturing exit code + stream tails.
pub(crate) fn run_one(
    worktree: &Path,
    check: &RepoCheck,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
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
    };
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
    scratch.apply_env(&mut cmd);
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
    let timeout = check.timeout();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if started.elapsed() >= timeout => {
                crate::validator::kill_child_tree(&mut child);
                crate::validator::reap_bounded(&mut child);
                result.timed_out = true;
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                result.spawn_error = Some(format!("wait failed: {e}"));
                break None;
            }
        }
    };
    // Quiesce: nothing the check backgrounded may keep running (or writing) into the next stage.
    crate::validator::kill_child_tree(&mut child);
    result.exit_code = status.and_then(|st| st.code());
    result.stdout_tail = out_h
        .and_then(|h| h.join().ok())
        .map(lossy)
        .unwrap_or_default();
    result.stderr_tail = err_h
        .and_then(|h| h.join().ok())
        .map(lossy)
        .unwrap_or_default();
    result.duration_ms = started.elapsed().as_millis() as u64;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
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
        // node_modules present ⇒ no install step.
        std::fs::create_dir_all(wt.join("node_modules")).unwrap();
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
            scratch.join("cargo-target").join("debug").is_dir(),
            "cargo built into the scratch target dir"
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
        };
        if !run_one(&wt, &inside, &sandbox, &scratch).passed() {
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
        };
        let r = run_one(&wt, &escaping, &sandbox, &scratch);
        assert!(!r.passed(), "an outside write must fail the check: {r:?}");
        assert!(!pwned.exists(), "the outside write must never land on disk");
        assert!(
            r.stderr_tail.contains("Permission denied")
                || r.stderr_tail.contains("Operation not permitted"),
            "the check observed the OS denial: {}",
            r.stderr_tail
        );
        // And the operator's real HOME is not the check's HOME.
        let home_probe = RepoCheck {
            name: "test".into(),
            argv: s(&["sh", "-c", "printf %s \"$HOME\""]),
            source: "fixture".into(),
        };
        let r = run_one(&wt, &home_probe, &sandbox, &scratch);
        assert!(r.passed());
        assert!(
            r.stdout_tail
                .starts_with(&scratch.home().to_string_lossy().to_string()),
            "HOME is the isolated scratch home, got {}",
            r.stdout_tail
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
            },
            RepoCheck {
                name: "test".into(),
                argv: s(&["definitely-not-a-binary-wicked-xyz", "run"]),
                source: "fixture".into(),
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
            let r = run_one(&wt, c, &sandbox, &scratch);
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
        };
        if crate::validator::find_on_path("sh").is_none() {
            eprintln!("repo_checks: sh not on PATH — skipping the bounded-tail check");
            return;
        }
        let sandbox = sandbox_for(&wt);
        let scratch = CheckScratch::prepare(&wt).unwrap();
        let r = run_one(&wt, &echo, &sandbox, &scratch);
        assert!(r.passed(), "{r:?}");
        assert!(
            r.stdout_tail.len() <= TAIL_BYTES,
            "the tail is bounded to {TAIL_BYTES} bytes, got {}",
            r.stdout_tail.len()
        );
        assert!(r.stdout_tail.contains("long-line-of-output"));
    }
}
