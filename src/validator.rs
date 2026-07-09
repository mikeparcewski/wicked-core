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
/// through `runner` (the live headless recipe). The skill returns a shell check; code fences are
/// stripped. Errors if authoring fails or produces an empty script.
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
    let script = strip_fences(&out.output);
    if script.is_empty() {
        anyhow::bail!("validator authoring produced an empty script");
    }
    Ok(DeterministicValidator {
        criterion: criterion.to_string(),
        script,
    })
}

/// Strip Markdown code fences + surrounding whitespace, returning the inner command(s). LLMs often
/// wrap a shell answer in ``` fences despite instructions; the pinned artifact should be the raw check.
fn strip_fences(raw: &str) -> String {
    raw.lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_fences_unwraps_a_fenced_command() {
        assert_eq!(
            strip_fences("```sh\ntest -f README.md\n```"),
            "test -f README.md"
        );
        assert_eq!(strip_fences("  test -f x  "), "test -f x");
        assert_eq!(strip_fences("```\na\nb\n```"), "a\nb");
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
