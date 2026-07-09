//! VALIDATOR — the deterministic half of the rev0.4 gate (DES-EXEC-001 §rev0.4). A test-strategy skill
//! AUTHORS a grounded, deterministic check for a specific acceptance criterion; the gate later RE-RUNS
//! the pinned check (the deterministic re-verify). The LLM is offline (authoring) — never at the gate.
//!
//! SAFETY: the authored script is LLM-generated, so it is **untrusted until approved**. In the full
//! gate flow it is pinned + human/council-approved before it can gate (rev0.4 fork 3). [`run_validator`]
//! executes it in a caller-provided `cwd` — callers MUST only run an approved script (or an isolated
//! sandbox). This module deliberately keeps authoring and running separate so approval sits between.

use crate::domain::WorkUnit;
use crate::scope::EntityMode;
use crate::workflow::{StepInput, StepRunner, StepStatus};

/// A deterministic validator authored for one acceptance criterion — the phase's evidence evaluator.
/// `script` is a shell command that exits 0 iff the criterion is satisfied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeterministicValidator {
    pub criterion: String,
    pub script: String,
}

/// Author a deterministic validator for `criterion` by invoking the `acceptance-test-writer` skill
/// through `runner` (the live headless recipe). The skill returns a shell check, often wrapped in
/// explanation despite instructions; [`extract_shell_command`] pulls out the bare command. Errors if
/// authoring fails or produces an empty script.
pub fn author_deterministic_validator(
    criterion: &str,
    runner: &dyn StepRunner,
) -> anyhow::Result<DeterministicValidator> {
    let prompt = format!(
        "For the acceptance criterion '{criterion}', output ONLY a single POSIX shell command \
         (no prose, no explanation, no code fences) that exits 0 if the criterion is satisfied and \
         non-zero otherwise. Prefer test/grep over anything destructive."
    );
    let mut unit = WorkUnit::pending("validator-author", "validator", 1, prompt);
    unit.skill_ref = Some("wicked-testing-acceptance-test-writer".to_string());
    // Ad-hoc claude invocation so the caller needs no council registry entry.
    unit.assigned_invocation = Some("claude -p {PROMPT}".to_string());
    let input = StepInput {
        run_id: "validator".to_string(),
        unit_ix: 0,
        attempt: 0,
        unit,
        workflow_id: "wf-validator".to_string(),
        entity_mode: EntityMode::Isolated,
        workdir: None,
    };
    let out = runner.run_unit(&input);
    if out.status != StepStatus::Ok {
        anyhow::bail!(
            "validator authoring failed ({:?}): {}",
            out.status,
            out.output
        );
    }
    let script = extract_shell_command(&out.output);
    if script.is_empty() {
        anyhow::bail!("validator authoring produced an empty script");
    }
    Ok(DeterministicValidator {
        criterion: criterion.to_string(),
        script,
    })
}

/// Extract the single shell command from a writer response that — despite "no prose" instructions —
/// often wraps the command in explanation ("Here's the check:\n\ntest -f x ...", or a trailing note).
/// `strip_fences` alone would leave those prose lines in the script, and `sh -c` would run them (a
/// spurious "command not found" that only doesn't fail the check when the real command happens to land
/// last). This picks the actual command deterministically: among the non-empty, non-fence lines, the
/// LAST one that looks like a shell command (so both a preamble and a trailing note are discarded),
/// falling back to the last non-empty line. A leaked language marker (`bash …`) is then stripped.
fn extract_shell_command(raw: &str) -> String {
    let lines: Vec<&str> = raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with("```"))
        .collect();
    let chosen = lines
        .iter()
        .rev()
        .find(|l| looks_like_shell_command(l))
        .or_else(|| lines.last())
        .copied()
        .unwrap_or("");
    strip_shell_lang_prefix(chosen)
}

/// Heuristic: does this line read as a shell command (vs. an English explanation)? True when it opens
/// with a common check command, a `[`/`[[` test, or contains a shell operator (`&&`/`||`/redirect).
/// Intentionally conservative — it only has to beat prose lines from the same response.
fn looks_like_shell_command(line: &str) -> bool {
    const CMDS: &[&str] = &[
        "test", "grep", "ls", "cat", "find", "stat", "head", "tail", "awk", "sed", "wc", "diff",
        "cmp", "bash", "sh", "[", "[[", "!",
    ];
    let first = line.split_whitespace().next().unwrap_or("");
    CMDS.contains(&first) || first.starts_with('[') || line.contains("&&") || line.contains("||")
}

/// Strip a single leaked shell-language marker from the front of an authored command. LLMs sometimes
/// answer with a code-fence *info string* inlined onto the command itself (e.g. `bash test -f x`)
/// instead of only on a ``` fence line; `strip_fences` can't see that, so `sh -c` would then run
/// `bash` with `test` as a *script path* (→ "cannot execute binary file") and the check spuriously
/// fails. Conservative: only known markers, and never when a flag follows (so a real `sh -c '…'` /
/// `bash -c '…'` command is left intact).
fn strip_shell_lang_prefix(s: &str) -> String {
    const MARKERS: &[&str] = &[
        "bash",
        "sh",
        "shell",
        "zsh",
        "shellscript",
        "console",
        "posix",
    ];
    if let Some((first, rest)) = s.split_once(char::is_whitespace) {
        let rest = rest.trim_start();
        if MARKERS.contains(&first.to_ascii_lowercase().as_str())
            && !rest.is_empty()
            && !rest.starts_with('-')
        {
            return rest.to_string();
        }
    }
    s.to_string()
}

/// The deterministic RE-VERIFY: run the (approved, pinned) validator's script in `cwd`; `true` iff it
/// exits 0. This is what the gate ladder's layer-1 runs — no LLM, fully deterministic.
pub fn run_validator(v: &DeterministicValidator, cwd: &std::path::Path) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&v.script)
        .current_dir(cwd)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The AGENT half of the rev0.4 dual validator: a reviewer skill judges whether `work` satisfies
/// `criterion` — the semantic judgment a deterministic script can't encode. Authored by a DISTINCT
/// seat from the deterministic validator (two-strategist independence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentVerdict {
    pub pass: bool,
    pub reasoning: String,
}

/// Run the agent validator: a reviewer seat judges `work` against `criterion` and returns
/// PASS/REJECT + a reason, reading only the cold `work` (evidence-only isolation). Uses a CONTROLLED
/// reviewer prompt — NOT a Tier-2 skill — because a skill imposes its own output contract (e.g. the
/// semantic-reviewer's aligned/divergent/missing Gap Report) that fights a clean binary verdict; the
/// two-strategist independence (rev0.4) comes from the distinct seat + cold framing, not the skill.
pub fn agent_validate(
    criterion: &str,
    work: &str,
    runner: &dyn StepRunner,
) -> anyhow::Result<AgentVerdict> {
    let prompt = format!(
        "You are a strict reviewer. Decide whether the WORK satisfies the CRITERION. The FIRST line of \
         your reply MUST be exactly one word — `PASS` or `REJECT` — and nothing else on that line; then \
         a brief reason on the next line. Reject if the work diverges from or does not meet the \
         criterion.\n\nCRITERION: {criterion}\n\nWORK:\n{work}"
    );
    // No skill_ref: an authored prompt on a distinct seat, so the verdict format is fully controlled.
    let mut unit = WorkUnit::pending("validator-agent", "validator", 1, prompt);
    unit.assigned_invocation = Some("claude -p {PROMPT}".to_string());
    let input = StepInput {
        run_id: "validator".to_string(),
        unit_ix: 0,
        attempt: 0,
        unit,
        workflow_id: "wf-validator".to_string(),
        entity_mode: EntityMode::Isolated,
        workdir: None,
    };
    let out = runner.run_unit(&input);
    if out.status != StepStatus::Ok {
        anyhow::bail!("agent validation failed ({:?}): {}", out.status, out.output);
    }
    Ok(parse_agent_verdict(&out.output))
}

/// Parse the reviewer's verdict: the first line whose first token is PASS/REJECT decides it. A
/// response with no clear verdict is treated as REJECT (fail-closed — never a lone-model approve).
fn parse_agent_verdict(raw: &str) -> AgentVerdict {
    for line in raw.lines() {
        let t = line.trim();
        let upper = t.to_uppercase();
        if upper.starts_with("PASS") {
            return AgentVerdict {
                pass: true,
                reasoning: t.to_string(),
            };
        }
        if upper.starts_with("REJECT") {
            return AgentVerdict {
                pass: false,
                reasoning: t.to_string(),
            };
        }
    }
    AgentVerdict {
        pass: false,
        reasoning: format!("no PASS/REJECT verdict found (fail-closed): {}", raw.trim()),
    }
}

/// The gate verdict from the rev0.4 combination rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateVerdict {
    Approve,
    Reject,
}

/// rev0.4 combination rule (preserves "a model may never SOLELY approve a gate"): **Approve iff the
/// deterministic validator PASSES and the agent validator does not REJECT.** The agent can fail a
/// gate but is never the sole approver; `None` agent ⇒ deterministic-only (structural phase).
pub fn combine_verdict(deterministic_pass: bool, agent: Option<&AgentVerdict>) -> GateVerdict {
    let agent_rejects = agent.map(|a| !a.pass).unwrap_or(false);
    if deterministic_pass && !agent_rejects {
        GateVerdict::Approve
    } else {
        GateVerdict::Reject
    }
}

/// Gate a phase with the full rev0.4 dual validator, composed: author + re-verify the DETERMINISTIC
/// check against `cwd` (the phase's artifacts/worktree) AND run the AGENT judge over `work` (the phase
/// output text), combined by [`combine_verdict`]. This is the single "gate this phase" entry point.
///
/// NOTE: this authors the deterministic validator fresh each call. The full flow PINS an approved
/// validator (author once → approve → re-verify many) — pinning + vault storage is the integration
/// step. `deterministic_only` skips the agent (structural phases).
pub fn gate_phase(
    criterion: &str,
    work: &str,
    cwd: &std::path::Path,
    deterministic_only: bool,
    runner: &dyn StepRunner,
) -> anyhow::Result<GateVerdict> {
    let det = author_deterministic_validator(criterion, runner)?;
    let det_pass = run_validator(&det, cwd);
    let agent = if deterministic_only {
        None
    } else {
        Some(agent_validate(criterion, work, runner)?)
    };
    Ok(combine_verdict(det_pass, agent.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agent_verdict_reads_the_leading_token() {
        assert!(parse_agent_verdict("PASS looks good").pass);
        assert!(!parse_agent_verdict("REJECT missing X").pass);
        assert!(
            !parse_agent_verdict("hmm, unclear").pass,
            "no verdict ⇒ fail-closed"
        );
        // a verdict on a later line still counts (skips a blank/preamble line)
        assert!(parse_agent_verdict("\nPASS after a blank line").pass);
    }

    #[test]
    fn combine_verdict_enforces_the_rev04_rule() {
        let pass = AgentVerdict {
            pass: true,
            reasoning: "ok".into(),
        };
        let reject = AgentVerdict {
            pass: false,
            reasoning: "no".into(),
        };
        // deterministic PASS is necessary; agent can only reject, never lone-approve.
        assert_eq!(combine_verdict(true, Some(&pass)), GateVerdict::Approve);
        assert_eq!(
            combine_verdict(true, Some(&reject)),
            GateVerdict::Reject,
            "agent rejects"
        );
        assert_eq!(
            combine_verdict(false, Some(&pass)),
            GateVerdict::Reject,
            "det fail dominates"
        );
        assert_eq!(combine_verdict(false, None), GateVerdict::Reject);
        assert_eq!(
            combine_verdict(true, None),
            GateVerdict::Approve,
            "deterministic-only phase"
        );
    }

    #[test]
    fn extract_shell_command_pulls_the_command_out_of_prose() {
        // Bare command.
        assert_eq!(
            extract_shell_command("test -f greeting.txt && grep -qF 'hello world' greeting.txt"),
            "test -f greeting.txt && grep -qF 'hello world' greeting.txt"
        );
        // Leaked code-fence info string inlined as a prefix (the observed `/bin/test` failure).
        assert_eq!(
            extract_shell_command("bash test -f greeting.txt && grep -qF 'hi' greeting.txt"),
            "test -f greeting.txt && grep -qF 'hi' greeting.txt"
        );
        // Preamble prose THEN the command (observed live).
        assert_eq!(
            extract_shell_command(
                "Only the exact command, per the instructions:\n\ntest -f x && grep -q y x"
            ),
            "test -f x && grep -q y x"
        );
        // Command THEN a trailing note — the command-ish line still wins over the note.
        assert_eq!(
            extract_shell_command(
                "grep -q '## Status' README.md\n\nThis checks the status section."
            ),
            "grep -q '## Status' README.md"
        );
        // Fenced with a language tag and prose around it.
        assert_eq!(
            extract_shell_command("Here is the check:\n```bash\ntest -f a.txt\n```"),
            "test -f a.txt"
        );
    }

    #[test]
    fn strip_shell_lang_prefix_leaves_real_flagged_commands_intact() {
        // A genuine `sh -c` / `bash -c` command must NOT be mangled.
        assert_eq!(
            strip_shell_lang_prefix("sh -c 'test -f x'"),
            "sh -c 'test -f x'"
        );
        assert_eq!(
            strip_shell_lang_prefix("bash -c 'grep y x'"),
            "bash -c 'grep y x'"
        );
        // But a leaked language marker is dropped.
        assert_eq!(strip_shell_lang_prefix("bash test -f x"), "test -f x");
        assert_eq!(strip_shell_lang_prefix("test -f x"), "test -f x");
    }

    #[test]
    fn run_validator_discriminates_pass_from_fail() {
        // Deterministic (no LLM): a hand-written check passes in a dir with the file, fails without.
        let dir = std::env::temp_dir().join(format!("wicked-validator-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("README.md"), "# Title\n\n## Status\nok\n").unwrap();
        let v = DeterministicValidator {
            criterion: "README exists with a Status section".to_string(),
            script: "test -f README.md && grep -q '## Status' README.md".to_string(),
        };
        assert!(run_validator(&v, &dir), "passes where the criterion holds");
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(!run_validator(&v, &empty), "fails where it does not");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
