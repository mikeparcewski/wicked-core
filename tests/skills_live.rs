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
        governance: None,
        prior_outputs: vec![],
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
    // Author (untrusted) → APPROVE (out-of-band gate step) → re-verify. run_validator refuses an
    // unapproved validator, so the explicit `.approve()` is what authorizes execution.
    let v = author_deterministic_validator(
        "a file named README.md exists in the current directory and contains a line with '## Status'",
        &runner,
    )
    .expect("authoring should succeed against real claude")
    .approve();
    eprintln!("authored validator script: {}", v.script);

    let base = std::env::temp_dir().join(format!("wicked-val-live-{}", std::process::id()));
    let pass = base.join("pass");
    let fail = base.join("fail");
    std::fs::create_dir_all(&pass).unwrap();
    std::fs::create_dir_all(&fail).unwrap();
    std::fs::write(pass.join("README.md"), "# Title\n\n## Status\nok\n").unwrap();

    assert!(
        run_validator(&v, &pass).expect("approved validator runs"),
        "authored validator must PASS where the criterion holds: {}",
        v.script
    );
    assert!(
        !run_validator(&v, &fail).expect("approved validator runs"),
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
    use wicked_core::{
        agent_validate, combine_verdict, registry_roster, GateVerdict, DETERMINISTIC_VALIDATOR_SEAT,
    };
    let runner = WrappedCliStepRunner::default();
    let criterion = "the greeting says hello to the world";
    // GAP B: the judge runs under a seat distinct from the deterministic author when the live roster
    // offers one, else the single default runner. The live roster drives the real seat pick here.
    let roster = registry_roster();

    // Good work: agent should PASS; combined with a deterministic pass ⇒ Approve.
    let good = agent_validate(
        criterion,
        "println!(\"hello world\");",
        &[DETERMINISTIC_VALIDATOR_SEAT],
        &roster,
        &runner,
    )
    .expect("agent");
    eprintln!("agent(good): {:?}", good);
    assert_eq!(combine_verdict(true, Some(&good)), GateVerdict::Approve);

    // Bad work: agent should REJECT; even with a deterministic pass ⇒ Reject (agent can fail a gate).
    let bad = agent_validate(
        criterion,
        "println!(\"goodbye\");",
        &[DETERMINISTIC_VALIDATOR_SEAT],
        &roster,
        &runner,
    )
    .expect("agent");
    eprintln!("agent(bad): {:?}", bad);
    assert!(
        !bad.pass,
        "reviewer should reject work that doesn't meet the criterion"
    );
    assert_eq!(combine_verdict(true, Some(&bad)), GateVerdict::Reject);
}

/// LIVE composed gate (rev0.4): gate_phase runs the deterministic check against the worktree AND the
/// agent judge over the work, combined. A phase whose artifacts + output satisfy the criterion Approves;
/// a phase whose artifacts do NOT satisfy it Rejects — proving both directions through gate_phase itself.
///
/// The criterion is deliberately CONTENT-framed ("the deliverable greeting.txt contains the text …")
/// rather than existence-framed ("a file … exists in the current directory"). The two gate halves see
/// different worlds: the deterministic check runs in `dir` (which has the artifact), but the agent
/// reviewer runs live `claude` in ITS OWN cwd with tool access — an "exists in the cwd" criterion makes
/// it search its (empty) cwd, find nothing, and REJECT a phase that is actually satisfied. Content
/// framing lets the reviewer judge the supplied `work` on its merits while the deterministic half owns
/// the filesystem existence check in `dir`. See root-cause note in the commit that added this.
#[test]
#[ignore = "requires real `claude` on PATH + installed wicked-testing skills; run with --ignored"]
fn gate_phase_approves_a_satisfying_phase_end_to_end() {
    use wicked_core::{gate_phase, GateVerdict};
    let runner = WrappedCliStepRunner::default();
    let criterion = "the deliverable greeting.txt contains the text 'hello world'";
    let work =
        "The full content of the delivered greeting.txt file is the single line:\nhello world";

    // FINDING-1: gate_phase no longer authors inline. Author ONCE → APPROVE out of band → gate with the
    // approved validator. The same approved validator gates both the good and bad worktrees.
    let validator = author_deterministic_validator(criterion, &runner)
        .expect("authoring should succeed against real claude")
        .approve();
    eprintln!("authored+approved validator script: {}", validator.script);

    // Satisfying phase: artifact present with the right content ⇒ deterministic PASS; the content-framed
    // work satisfies the reviewer ⇒ agent not-REJECT ⇒ Approve.
    let good = std::env::temp_dir().join(format!("wicked-gate-live-good-{}", std::process::id()));
    std::fs::create_dir_all(&good).unwrap();
    std::fs::write(good.join("greeting.txt"), "hello world\n").unwrap();
    let verdict = gate_phase(&validator, work, &good, false, &runner).expect("gate_phase good");
    assert_eq!(
        verdict,
        GateVerdict::Approve,
        "artifacts + output satisfy the criterion"
    );
    let _ = std::fs::remove_dir_all(&good);

    // Non-satisfying phase: artifact has the WRONG content ⇒ deterministic FAIL dominates ⇒ Reject,
    // regardless of the agent. Proves the composed gate does not rubber-stamp.
    let bad = std::env::temp_dir().join(format!("wicked-gate-live-bad-{}", std::process::id()));
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("greeting.txt"), "goodbye\n").unwrap();
    let verdict = gate_phase(&validator, work, &bad, false, &runner).expect("gate_phase bad");
    assert_eq!(
        verdict,
        GateVerdict::Reject,
        "artifacts do not satisfy the criterion ⇒ deterministic fail ⇒ Reject"
    );
    let _ = std::fs::remove_dir_all(&bad);
}
