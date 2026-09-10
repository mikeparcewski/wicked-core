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
//!   install` without; the pnpm/yarn frozen-lockfile equivalents);
//! * `Cargo.toml` → `cargo test`.
//!
//! Each command's exit code, duration and the TAIL of its stdout/stderr are captured as a
//! [`CheckRun`] and attached to the gate: the fold persists the [`RepoChecksReport`] on the unit,
//! emits `repoChecksEvaluated`, and DENIES the unit when any check exits non-zero, times out, or
//! cannot be spawned (fail-closed — a check that cannot run has re-derived nothing). Checks stop at
//! the first failure: the evidence of the failure is what the gate needs, and a failing typecheck
//! makes the suite behind it moot.
//!
//! A repository that declares NO detectable checks yields a report with no runs and `passed:
//! true` — disclosed as such on the event (`checks: []`), never silently. That is "nothing to
//! re-verify", which is a different statement from "could not re-verify" and is reported as one.
//!
//! ## Why the engine runs them rather than trusting the seat
//!
//! The seat may well have run the same commands — the acceptance transcript says it did. The
//! point is WHO the record belongs to: an exit code the engine observed is evidence; a sentence
//! in a transcript is a claim. Running them again costs minutes of machine time per verify phase
//! and buys the one property the gate exists for.
//!
//! ## Environment
//!
//! The checks inherit the daemon's environment through the spawn chokepoint (`hardened()` strips
//! every engine-internal variable), plus `CI=1` so watch-mode test runners run once, and colour
//! disabled so the captured tails are legible. They run in the worktree with the worktree as cwd.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use wicked_apps_core::spawn::HardenedCommand;

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
    /// What was detected, in run order.
    pub detected: Vec<RepoCheck>,
    /// What actually ran (a prefix of `detected` — the floor stops at the first failure).
    pub checks: Vec<CheckRun>,
    /// Detected checks that never ran because an earlier one failed.
    pub skipped: Vec<String>,
    /// True iff every detected check ran and exited 0 (vacuously true when nothing was detected —
    /// see the module doc; the event discloses `checks: []`).
    pub passed: bool,
}

impl RepoChecksReport {
    /// The operator-facing denial when `!passed`.
    pub fn denial_reason(&self) -> String {
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

/// Which package manager a Node repo uses, read off its lockfile (npm when none says otherwise).
fn package_manager(worktree: &Path) -> &'static str {
    if worktree.join("pnpm-lock.yaml").is_file() {
        "pnpm"
    } else if worktree.join("yarn.lock").is_file() {
        "yarn"
    } else {
        "npm"
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Detect the repository's own checks in `worktree`. Pure over the filesystem — runs nothing.
pub fn detect(worktree: &Path) -> Vec<RepoCheck> {
    let mut out = Vec::new();
    let pkg = worktree.join("package.json");
    if let Ok(raw) = std::fs::read_to_string(&pkg) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) {
            let scripts = json.get("scripts").and_then(|v| v.as_object());
            let pm = package_manager(worktree);
            let wanted: Vec<&str> = ["typecheck", "lint", "test"]
                .into_iter()
                .filter(|k| scripts.is_some_and(|m| m.get(*k).and_then(|v| v.as_str()).is_some()))
                .collect();
            if !wanted.is_empty() {
                if !worktree.join("node_modules").is_dir() {
                    let (argv, source) = match pm {
                        "pnpm" => (
                            s(&["pnpm", "install", "--frozen-lockfile"]),
                            "pnpm-lock.yaml (node_modules absent)",
                        ),
                        "yarn" => (
                            s(&["yarn", "install", "--frozen-lockfile"]),
                            "yarn.lock (node_modules absent)",
                        ),
                        _ if worktree.join("package-lock.json").is_file() => (
                            s(&["npm", "ci", "--no-audit", "--no-fund"]),
                            "package-lock.json (node_modules absent)",
                        ),
                        _ => (
                            s(&["npm", "install", "--no-audit", "--no-fund"]),
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
    }
    if worktree.join("Cargo.toml").is_file() {
        out.push(RepoCheck {
            name: "cargo-test".into(),
            argv: s(&["cargo", "test"]),
            source: "Cargo.toml".into(),
        });
    }
    out
}

/// Detect and run the checks in `worktree`, stopping at the first failure.
pub fn run(worktree: &Path) -> RepoChecksReport {
    let detected = detect(worktree);
    let mut checks = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = false;
    for check in &detected {
        if failed {
            skipped.push(check.name.clone());
            continue;
        }
        let run = run_one(worktree, check);
        failed = !run.passed();
        checks.push(run);
    }
    RepoChecksReport {
        passed: !failed,
        detected,
        checks,
        skipped,
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

/// Run one check in `worktree` under its timeout, capturing exit code + stream tails.
pub fn run_one(worktree: &Path, check: &RepoCheck) -> CheckRun {
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
    // spawn-audit: hardened — the repository's own check command, run in the run's worktree.
    let mut cmd = Command::new(exe);
    cmd.hardened()
        .args(&check.argv[1..])
        .current_dir(worktree)
        .env("CI", "1")
        .env("NO_COLOR", "1")
        .env("FORCE_COLOR", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
    use std::path::PathBuf;

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

    #[test]
    fn detects_node_scripts_with_install_when_node_modules_is_absent() {
        let wt = scratch("detect-node");
        std::fs::write(
            wt.join("package.json"),
            r#"{"scripts":{"build":"x","test":"vitest run","typecheck":"tsc --noEmit","dev":"y"}}"#,
        )
        .unwrap();
        std::fs::write(wt.join("package-lock.json"), "{}").unwrap();
        let names: Vec<(String, Vec<String>)> =
            detect(&wt).into_iter().map(|c| (c.name, c.argv)).collect();
        assert_eq!(
            names,
            vec![
                (
                    "install".to_string(),
                    s(&["npm", "ci", "--no-audit", "--no-fund"])
                ),
                ("typecheck".to_string(), s(&["npm", "run", "typecheck"])),
                ("test".to_string(), s(&["npm", "run", "test"])),
            ],
            "install first (lockfile ⇒ ci), then typecheck/lint/test in that order, only those present"
        );
        // node_modules present ⇒ no install step.
        std::fs::create_dir_all(wt.join("node_modules")).unwrap();
        assert_eq!(
            detect(&wt)
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
        let checks = detect(&wt);
        assert_eq!(checks[0].argv, s(&["pnpm", "run", "lint"]));
        assert_eq!(checks[1].name, "cargo-test");
        assert_eq!(checks[1].argv, s(&["cargo", "test"]));
        // No manifests at all ⇒ nothing detected, and a run of it is a vacuous pass that SAYS so.
        let empty = scratch("detect-empty");
        assert!(detect(&empty).is_empty());
        let report = run(&empty);
        assert!(report.passed && report.checks.is_empty());
        assert!(report.summary().contains("no repository checks detected"));
    }

    /// The brief's test, in the medium every `cargo test` host has: a fixture crate whose one test
    /// fails. The floor must capture the non-zero exit AND the assertion text in the tail, and
    /// report the failure as a denial.
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
        // Keep the fixture's build artifacts under the fixture, not the developer's target dir.
        // (`CARGO_TARGET_DIR` is inherited through the spawn chokepoint; it is not engine-internal.)
        let prior = std::env::var_os("CARGO_TARGET_DIR");
        std::env::set_var("CARGO_TARGET_DIR", wt.join("target"));
        let report = run(&wt);
        match prior {
            Some(v) => std::env::set_var("CARGO_TARGET_DIR", v),
            None => std::env::remove_var("CARGO_TARGET_DIR"),
        }
        assert!(
            !report.passed,
            "a failing test must fail the floor: {report:?}"
        );
        let c = &report.checks[0];
        assert_eq!(c.name, "cargo-test");
        assert!(
            c.exit_code.is_some_and(|e| e != 0),
            "the non-zero exit is captured: {:?}",
            c.exit_code
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
        let c = &report.checks[0];
        assert_eq!(c.name, "test");
        assert_eq!(c.argv, s(&["npm", "run", "test"]));
        assert_eq!(
            c.exit_code,
            Some(3),
            "npm propagates the script's exit code"
        );
        assert!(c.stdout_tail.contains("NPM BOOM"), "{}", c.stdout_tail);
    }

    #[test]
    fn stops_at_the_first_failure_and_lists_what_it_skipped() {
        let wt = scratch("stop");
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
            let r = run_one(&wt, c);
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
        let r = run_one(&wt, &echo);
        assert!(r.passed(), "{r:?}");
        assert!(
            r.stdout_tail.len() <= TAIL_BYTES,
            "the tail is bounded to {TAIL_BYTES} bytes, got {}",
            r.stdout_tail.len()
        );
        assert!(r.stdout_tail.contains("long-line-of-output"));
    }
}
