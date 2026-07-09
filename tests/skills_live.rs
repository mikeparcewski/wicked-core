//! LIVE skill-invocation verification — runs the REAL `WrappedCliStepRunner` against a REAL `claude`
//! with the wicked-testing skills installed. Proves the skill-driven invocation end-to-end (not just
//! the unit-tested command construction): a unit with a `skill_ref` actually loads the named skill.
//!
//! `#[ignore]`d because it requires `claude` on PATH + `~/.claude/skills/wicked-testing-*` installed
//! (a fresh CI box won't have them). Run explicitly:
//!   cargo test -p wicked-core --test skills_live -- --ignored --nocapture
//!
//! Verified passing 2026-07-09 against claude v2.1.205 (51 wicked-testing skills installed):
//! the runner's argv `claude -p "/wicked-testing-semantic-reviewer <prompt>"` expands the skill and
//! the model replies in-role at turn 0.

use wicked_core::{
    author_deterministic_validator, run_validator, EntityMode, StepInput, StepRunner, WorkUnit,
    WrappedCliStepRunner,
};

#[test]
#[ignore = "requires real `claude` on PATH + installed wicked-testing skills; run with --ignored"]
fn a_skill_driven_unit_loads_the_named_skill_against_real_claude() {
    // A unit whose backing phase named a skill (as plan_from_def carries it), invoked on claude via
    // the standard headless template. The skill prompt is deterministic ("reply with READY only") so
    // the assertion is stable + cheap.
    let mut unit = WorkUnit::pending(
        "live:review",
        "live",
        1,
        "Reply with only the word READY and nothing else.",
    );
    unit.skill_ref = Some("wicked-testing-semantic-reviewer".to_string());
    // Drive claude explicitly (ad-hoc invocation, so no council registry needed for the test).
    unit.assigned_invocation = Some("claude -p {PROMPT}".to_string());

    let input = StepInput {
        run_id: "live".to_string(),
        unit_ix: 0,
        attempt: 0,
        unit,
        workflow_id: "wf-live".to_string(),
        entity_mode: EntityMode::Shared,
        workdir: None,
    };

    let runner = WrappedCliStepRunner::default();
    let out = runner.run_unit(&input);

    // The runner built `claude -p "/wicked-testing-semantic-reviewer <prompt>"`; claude expanded the
    // skill and answered in-role. If the skill hadn't loaded, the harness would have errored or the
    // model would have ignored the leading slash.
    assert!(
        out.output.to_uppercase().contains("READY"),
        "expected the in-role reply; got: {:?} (status {:?})",
        out.output,
        out.status
    );
}

/// LIVE gate-mechanism slice (DES-EXEC-001 rev0.4): the acceptance-test-writer skill AUTHORS a
/// grounded deterministic validator for a criterion, and the pinned script then discriminates a
/// satisfying dir from a non-satisfying one — the deterministic re-verify, no LLM at run time.
#[test]
#[ignore = "requires real `claude` on PATH + installed wicked-testing skills; run with --ignored"]
fn writer_skill_authors_a_deterministic_validator_that_discriminates() {
    let runner = WrappedCliStepRunner::default();
    let v = author_deterministic_validator(
        "a file named README.md exists in the current directory and contains a line with '## Status'",
        &runner,
    )
    .expect("authoring should succeed against real claude");
    eprintln!("authored validator script: {}", v.script);

    let base = std::env::temp_dir().join(format!("wicked-val-live-{}", std::process::id()));
    let pass = base.join("pass");
    let fail = base.join("fail");
    std::fs::create_dir_all(&pass).unwrap();
    std::fs::create_dir_all(&fail).unwrap();
    std::fs::write(pass.join("README.md"), "# Title\n\n## Status\nok\n").unwrap();

    assert!(
        run_validator(&v, &pass),
        "authored validator must PASS where the criterion holds: {}",
        v.script
    );
    assert!(
        !run_validator(&v, &fail),
        "and FAIL where it does not: {}",
        v.script
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// LIVE full dual-validator gate (rev0.4): the writer authors a deterministic check AND the
/// semantic-reviewer judges the work, combined by the rule "Approve iff deterministic PASS and agent
/// not REJECT". Two distinct skill seats; a model can fail but never lone-approve.
#[test]
#[ignore = "requires real `claude` on PATH + installed wicked-testing skills; run with --ignored"]
fn dual_validator_gate_approves_good_work_and_rejects_bad() {
    use wicked_core::{agent_validate, combine_verdict, GateVerdict};
    let runner = WrappedCliStepRunner::default();
    let criterion = "the greeting says hello to the world";

    // Good work: agent should PASS; combined with a deterministic pass ⇒ Approve.
    let good = agent_validate(criterion, "println!(\"hello world\");", &runner).expect("agent");
    eprintln!("agent(good): {:?}", good);
    assert_eq!(combine_verdict(true, Some(&good)), GateVerdict::Approve);

    // Bad work: agent should REJECT; even with a deterministic pass ⇒ Reject (agent can fail a gate).
    let bad = agent_validate(criterion, "println!(\"goodbye\");", &runner).expect("agent");
    eprintln!("agent(bad): {:?}", bad);
    assert!(
        !bad.pass,
        "reviewer should reject work that doesn't meet the criterion"
    );
    assert_eq!(combine_verdict(true, Some(&bad)), GateVerdict::Reject);
}
