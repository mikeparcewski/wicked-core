//! (core#959, operator decision 2026-10-17) A team finding held for the operator is EVIDENCE: the
//! original finding and what put it on hold (its monitor's withdrawal request, or its cited text
//! disappearing) are written to wicked-vault through its own CLI — the one mechanism the platform
//! records evidence with (`wicked-vault record --artifact`, as the walkthrough proof does): an
//! append-only, content-addressed entry under the run's evidence root, never only a bus row.
//!
//! The vault root is `<evidence root>/team/vault`, a vault of its own (`init` before the first
//! record, so its walk-up never lands in an ancestor's vault). The artifact is a JSON file beside
//! it. `WICKED_VAULT_BIN` names the CLI (empty = switched off); default `wicked-vault` on PATH.
//! A record that cannot be made is not a pass: the finding stays held for the operator either way,
//! and the settled row says why the vault has no entry.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use wicked_apps_core::spawn::HardenedCommand;

/// One evidence record: the vault's required fields (`record --scope --phase --claim --kind
/// --criteria --artifact --actor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultRecord {
    pub scope: String,
    pub phase: String,
    pub claim: String,
    pub kind: String,
    pub criteria: String,
    pub artifact: PathBuf,
    pub actor: String,
}

/// Where operator-held team evidence goes. Production: [`CliVault`]; tests inject a fake.
pub trait EvidenceVault: Send + Sync + std::fmt::Debug {
    /// Record `rec` in the vault rooted at `vault_dir` within `budget`: the entry id, or why there
    /// is none.
    fn record(
        &self,
        vault_dir: &Path,
        rec: &VaultRecord,
        budget: Duration,
    ) -> Result<String, String>;
}

/// The wicked-vault CLI: `bin`, else `WICKED_VAULT_BIN` (read per record; empty = switched off),
/// else `wicked-vault` on PATH.
#[derive(Debug, Default, Clone)]
pub struct CliVault {
    pub bin: Option<String>,
}

/// How long one vault command may run.
pub(crate) const VAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Below this, no vault command is started: the hold is published unvaulted, and says why.
const VAULT_MIN_BUDGET: Duration = Duration::from_secs(1);

fn run_vault(bin: &str, args: &[String], deadline: Instant) -> Result<String, String> {
    let limit = deadline
        .saturating_duration_since(Instant::now())
        .min(VAULT_TIMEOUT);
    if limit < VAULT_MIN_BUDGET {
        return Err("no time left in the final pass to vault it".into());
    }
    let mut child = std::process::Command::new(bin)
        .hardened()
        .args(args)
        .env("WICKED_VAULT_NO_BUS", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("`{bin}` could not start: {e}"))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < limit => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("`{bin}` timed out after {limit:?}"));
            }
            Err(e) => return Err(format!("`{bin}` could not be waited on: {e}")),
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("`{bin}` output could not be read: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = String::from_utf8_lossy(&out.stdout);
        return Err(format!(
            "`{bin} {}` exited {}: {}",
            args.first().map(String::as_str).unwrap_or(""),
            out.status,
            crate::team::cap_utf8(format!("{}{}", msg.trim(), err.trim()).trim(), 512)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

impl EvidenceVault for CliVault {
    fn record(
        &self,
        vault_dir: &Path,
        rec: &VaultRecord,
        budget: Duration,
    ) -> Result<String, String> {
        // One deadline for `init` and `record` together.
        let deadline = Instant::now() + budget;
        let bin = match self
            .bin
            .clone()
            .map(Ok)
            .unwrap_or_else(|| std::env::var("WICKED_VAULT_BIN"))
        {
            Ok(b) if b.trim().is_empty() => {
                return Err("the vault is switched off (WICKED_VAULT_BIN is empty)".into())
            }
            Ok(b) => b,
            Err(_) => "wicked-vault".to_string(),
        };
        // A bare name resolves on PATH the PATHEXT-aware way (npm's `wicked-vault.cmd` on Windows).
        let bin = if Path::new(&bin).components().count() == 1 {
            crate::validator::find_on_path(&bin)
                .map(|p| p.to_string_lossy().into_owned())
                .ok_or_else(|| format!("`{bin}` is not on PATH (wicked-vault is not installed)"))?
        } else {
            bin
        };
        std::fs::create_dir_all(vault_dir).map_err(|e| {
            format!(
                "the vault dir {} could not be made: {e}",
                vault_dir.display()
            )
        })?;
        let dir = vault_dir.to_string_lossy().into_owned();
        if !vault_dir.join(".wicked-vault").is_dir() {
            run_vault(
                &bin,
                &["init".into(), "--cwd".into(), dir.clone()],
                deadline,
            )?;
        }
        let out = run_vault(
            &bin,
            &[
                "record".into(),
                "--cwd".into(),
                dir,
                "--scope".into(),
                rec.scope.clone(),
                "--phase".into(),
                rec.phase.clone(),
                "--claim".into(),
                rec.claim.clone(),
                "--kind".into(),
                rec.kind.clone(),
                "--criteria".into(),
                rec.criteria.clone(),
                "--artifact".into(),
                rec.artifact.to_string_lossy().into_owned(),
                "--actor".into(),
                rec.actor.clone(),
            ],
            deadline,
        )?;
        serde_json::from_str::<serde_json::Value>(out.trim())
            .ok()
            .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_string))
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                format!(
                    "the vault answered no entry id: {}",
                    crate::team::cap_utf8(out.trim(), 256)
                )
            })
    }
}

/// The team vault under a run's evidence root.
pub fn team_vault_dir(evidence_root: &Path) -> PathBuf {
    evidence_root.join("team").join("vault")
}

/// Write `artifact` (the finding and what holds it) beside the run's team vault and record it.
/// `Ok(entry id)` or `Err(why there is none)`; `None` evidence root is an `Err` (an unbound run
/// keeps no evidence root, so its hold is on the bus and the ledger only, and says so).
pub fn record_hold(
    vault: &dyn EvidenceVault,
    evidence_root: Option<&str>,
    file_stem: &str,
    artifact: &serde_json::Value,
    budget: Duration,
    rec: impl FnOnce(PathBuf) -> VaultRecord,
) -> Result<String, String> {
    let root = evidence_root
        .filter(|r| !r.trim().is_empty())
        .ok_or_else(|| "the run has no evidence root to vault into".to_string())?;
    let dir = Path::new(root).join("team").join("holds");
    std::fs::create_dir_all(&dir).map_err(|e| {
        format!(
            "the hold evidence dir {} could not be made: {e}",
            dir.display()
        )
    })?;
    let safe: String = file_stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("{safe}.json"));
    let bytes = serde_json::to_vec_pretty(artifact)
        .map_err(|e| format!("the hold evidence could not be serialized: {e}"))?;
    std::fs::write(&path, bytes).map_err(|e| {
        format!(
            "the hold evidence {} could not be written: {e}",
            path.display()
        )
    })?;
    vault.record(&team_vault_dir(Path::new(root)), &rec(path), budget)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("wicked-core-vault-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// core#959: an empty binary switches the vault off — an `Err`, never a pass; no root is no
    /// record either.
    #[test]
    fn core_959_a_switched_off_vault_or_no_root_is_an_error_not_a_pass() {
        let root = tmp("off");
        let v = CliVault {
            bin: Some(String::new()),
        };
        let art = serde_json::json!({"finding": "x"});
        let r = record_hold(
            &v,
            Some(&root.to_string_lossy()),
            "a",
            &art,
            VAULT_TIMEOUT,
            |p| VaultRecord {
                scope: "s".into(),
                phase: "p".into(),
                claim: "c".into(),
                kind: "k".into(),
                criteria: "c".into(),
                artifact: p,
                actor: "a".into(),
            },
        );
        assert!(r.unwrap_err().contains("switched off"));
        let r = record_hold(&v, None, "a", &art, VAULT_TIMEOUT, |p| VaultRecord {
            scope: "s".into(),
            phase: "p".into(),
            claim: "c".into(),
            kind: "k".into(),
            criteria: "c".into(),
            artifact: p,
            actor: "a".into(),
        });
        assert!(r.unwrap_err().contains("no evidence root"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// core#959 (codex r1): a vault record never outlives its budget — with none left, no command
    /// starts and the hold is published unvaulted, saying why.
    #[test]
    fn core_959_a_spent_budget_starts_no_vault_command() {
        let root = tmp("budget");
        let r = CliVault {
            bin: Some("/nonexistent/wicked-vault".into()),
        }
        .record(
            &root,
            &VaultRecord {
                scope: "s".into(),
                phase: "p".into(),
                claim: "c".into(),
                kind: "k".into(),
                criteria: "c".into(),
                artifact: root.join("a.json"),
                actor: "a".into(),
            },
            Duration::from_millis(10),
        );
        assert!(r.unwrap_err().contains("no time left"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// core#959: the real wicked-vault CLI records the hold in the run's own vault (its walk-up
    /// stops there) and the entry re-reads. Skipped where the CLI is not installed (the CI
    /// runners); the fake-vault supervisor tests cover the engine side everywhere.
    #[test]
    fn core_959_the_wicked_vault_cli_records_a_hold() {
        let installed = std::process::Command::new("wicked-vault")
            .hardened()
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !installed {
            eprintln!("wicked-vault is not installed: skipped");
            return;
        }
        let root = tmp("cli");
        let art = serde_json::json!({"finding": {"claim": "bypassed the sandbox"},
                                     "withdrawal_requested": "harmless"});
        let id = record_hold(
            &CliVault {
                bin: Some("wicked-vault".into()),
            },
            Some(&root.to_string_lossy()),
            "run-1-u3-a1-r1-f-x",
            &art,
            VAULT_TIMEOUT,
            |p| VaultRecord {
                scope: "run:run-1".into(),
                phase: "team:3".into(),
                claim: "finding:f-x".into(),
                kind: "team-finding-withdrawal-request".into(),
                criteria: "held for the operator".into(),
                artifact: p,
                actor: "claude#2".into(),
            },
        )
        .expect("recorded");
        assert!(!id.is_empty());
        let vault = team_vault_dir(&root);
        assert!(vault.join(".wicked-vault").is_dir(), "its own vault");
        let out = std::process::Command::new("wicked-vault")
            .hardened()
            .args(["inspect", &id, "--cwd"])
            .arg(&vault)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
