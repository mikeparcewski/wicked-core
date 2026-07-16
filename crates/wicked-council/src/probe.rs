//! Two-stage detection: PATH scan + optional version probe, with error-signature
//! classification.
//!
//! Stage 1 (`which`-equivalent over `binary` ∪ `alt_binaries`) is pure — no subprocess —
//! and yields `detected`. Stage 2 runs the `version_probe` argv bounded + isolated and
//! classifies stdout+stderr into `usable` or `unusable: <reason>`. A binary on PATH is
//! **not** a usable seat until stage 2.
//!
//! Full headless probing of every agentic CLI ("reply with the single word: pong") needs
//! a real LLM CLI logged in; the version probe is the testable stand-in that still
//! exercises subprocess + classification on a real binary (`sh`, `echo`). The seam
//! ([`crate::types::Prober`]) is what tests inject.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::types::{AgenticCli, ProbeOutcome, Prober, UnusableReason};

/// The real, subprocess-backed prober.
#[derive(Debug, Clone)]
pub struct RealProber {
    /// Per-CLI version-probe timeout.
    pub timeout: Duration,
}

impl Default for RealProber {
    fn default() -> Self {
        RealProber {
            timeout: Duration::from_secs(10),
        }
    }
}

/// Stage 1: resolve a binary on `PATH` by scanning `$PATH` entries directly (a
/// `shutil.which` equivalent, no `which` crate). On Windows also tries the `PATHEXT`
/// extensions. Returns the first hit.
pub fn which(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
            .split(';')
            .map(|s| s.to_string())
            .collect()
    } else {
        vec![String::new()]
    };

    for dir in std::env::split_paths(&path_var) {
        for ext in &exts {
            let candidate = if ext.is_empty() {
                dir.join(binary)
            } else {
                dir.join(format!("{binary}{ext}"))
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Resolve `binary` then any `alt_binaries`, returning the first that exists.
fn resolve_any(cli: &AgenticCli) -> Option<PathBuf> {
    which(&cli.binary).or_else(|| cli.alt_binaries.iter().find_map(|b| which(b)))
}

/// Classify combined process output into an [`UnusableReason`], in priority order.
/// Returns `None` when the output shows no failure signature (i.e. looks usable).
pub fn classify(exit_ok: bool, combined_lower: &str) -> Option<UnusableReason> {
    // Ordered signatures. Most-specific auth/provider first.
    if combined_lower.contains("401")
        || combined_lower.contains("403")
        || combined_lower.contains("not logged in")
        || combined_lower.contains("invalid api key")
        || combined_lower.contains("re-authenticate")
        || combined_lower.contains("unauthorized")
    {
        return Some(UnusableReason::Auth);
    }
    if combined_lower.contains("no provider configured")
        || combined_lower.contains("set api key")
        || combined_lower.contains("_api_key")
        || combined_lower.contains("run configure")
        || combined_lower.contains("no api key")
    {
        return Some(UnusableReason::NoProvider);
    }
    if combined_lower.contains("connection refused")
        || combined_lower.contains("is the server running")
        || combined_lower.contains("no such model")
        || combined_lower.contains("could not connect")
    {
        return Some(UnusableReason::DaemonDown);
    }
    if combined_lower.contains("rate limit")
        || combined_lower.contains("429")
        || combined_lower.contains("insufficient credits")
        || combined_lower.contains("402")
        || combined_lower.contains("quota")
    {
        return Some(UnusableReason::Quota);
    }
    // No recognised signature: usable iff the process exited cleanly.
    if exit_ok {
        None
    } else {
        Some(UnusableReason::Error)
    }
}

impl Prober for RealProber {
    fn probe(&self, cli: &AgenticCli) -> ProbeOutcome {
        // Stage 1: PATH scan.
        let resolved = resolve_any(cli);
        let Some(path) = resolved else {
            return ProbeOutcome {
                cli: cli.key.clone(),
                usable: false,
                reason: Some(UnusableReason::NotFound),
                resolved_path: None,
                version: None,
            };
        };
        let resolved_path = Some(path.display().to_string());

        // No version-probe argv → detected but unprobed; treat as usable on PATH
        // presence alone (the record asserts nothing else to verify).
        if cli.version_probe.is_empty() {
            return ProbeOutcome {
                cli: cli.key.clone(),
                usable: true,
                reason: None,
                resolved_path,
                version: None,
            };
        }

        // Stage 2: run the version-probe argv, bounded + isolated.
        let (program, args) = cli
            .version_probe
            .split_first()
            .expect("non-empty checked above");

        match run_bounded(program, args, self.timeout) {
            Ok((exit_ok, combined)) => {
                let lower = combined.to_lowercase();
                let reason = classify(exit_ok, &lower);
                let version = combined.lines().next().map(|l| l.trim().to_string());
                ProbeOutcome {
                    cli: cli.key.clone(),
                    usable: reason.is_none(),
                    reason,
                    resolved_path,
                    version,
                }
            }
            Err(ProbeError::Timeout) => ProbeOutcome {
                cli: cli.key.clone(),
                usable: false,
                reason: Some(UnusableReason::Timeout),
                resolved_path,
                version: None,
            },
            Err(ProbeError::Spawn) => ProbeOutcome {
                cli: cli.key.clone(),
                usable: false,
                reason: Some(UnusableReason::Error),
                resolved_path,
                version: None,
            },
        }
    }
}

enum ProbeError {
    Timeout,
    Spawn,
}

/// Run `program args…` with stdin from null, capturing stdout+stderr, bounded by
/// `timeout`. Returns `(exit_success, combined_output)`.
///
/// The bound is enforced by a watcher loop that kills the child if it overruns;
/// std-only (no tokio).
fn run_bounded(
    program: &str,
    args: &[String],
    timeout: Duration,
) -> Result<(bool, String), ProbeError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| ProbeError::Spawn)?;

    // Watcher: poll for completion until the deadline, then kill.
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ProbeError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return Err(ProbeError::Spawn),
        }
    }

    let output = child.wait_with_output().map_err(|_| ProbeError::Spawn)?;
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok((output.status.success(), combined))
}

/// `probe` payload: usable/unusable/collisions partition over the registry.
pub fn probe_all_json(prober: &dyn Prober, clis: &[AgenticCli]) -> serde_json::Value {
    let outcomes: Vec<ProbeOutcome> = clis.iter().map(|c| prober.probe(c)).collect();

    let usable: Vec<&str> = outcomes
        .iter()
        .filter(|o| o.usable)
        .map(|o| o.cli.as_str())
        .collect();
    let unusable: Vec<serde_json::Value> = outcomes
        .iter()
        .filter(|o| !o.usable)
        .map(|o| serde_json::json!({ "cli": o.cli, "reason": o.reason }))
        .collect();

    serde_json::json!({
        "detected": outcomes,
        "usable": usable,
        "unusable": unusable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Category;

    fn cli(key: &str, binary: &str, version_probe: Vec<String>) -> AgenticCli {
        AgenticCli {
            key: key.into(),
            display_name: key.into(),
            binary: binary.into(),
            headless_invocation: format!("{binary} \"{{PROMPT}}\""),
            category: Category::AgenticCoder,
            input_mode: crate::types::InputMode::PromptArg,
            version_probe,
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: crate::types::Confidence::Verified,
            enabled_for_council: true,
            acp: None,
        }
    }

    #[test]
    fn probe_detects_real_binary_as_usable() {
        // `sh` exists on every unix; `cmd` on Windows. Probe with a harmless arg.
        let prober = RealProber::default();
        let (bin, probe_argv) = if cfg!(windows) {
            ("cmd", vec!["cmd".to_string(), "/C".into(), "ver".into()])
        } else {
            ("sh", vec!["sh".to_string(), "-c".into(), "echo ok".into()])
        };
        let outcome = prober.probe(&cli("shell", bin, probe_argv));
        assert!(
            outcome.usable,
            "a real shell binary must probe usable, got {outcome:?}"
        );
        assert!(outcome.resolved_path.is_some(), "PATH must resolve {bin}");
        assert!(outcome.reason.is_none());
    }

    #[test]
    fn probe_reports_not_found_for_absent_binary() {
        let prober = RealProber::default();
        let outcome = prober.probe(&cli(
            "ghost",
            "wicked-council-no-such-binary-xyzzy",
            vec!["wicked-council-no-such-binary-xyzzy".into()],
        ));
        assert!(!outcome.usable);
        assert_eq!(outcome.reason, Some(UnusableReason::NotFound));
    }

    #[test]
    fn classify_flags_auth_and_quota() {
        assert_eq!(
            classify(false, "error: not logged in"),
            Some(UnusableReason::Auth)
        );
        assert_eq!(
            classify(false, "http 429 rate limit"),
            Some(UnusableReason::Quota)
        );
        assert_eq!(classify(true, "v1.2.3"), None);
        assert_eq!(classify(false, "boom"), Some(UnusableReason::Error));
    }
}
