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
        elicitation_epoch: 0,
        process_gen: None,
        launch_seq: 0,
        required_skills: Vec::new(),
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

/// core#396 — POSITIVE INVOCATION EVIDENCE ON THE WRAPPED CARRIER (codex round 2 finding 7,
/// tightened in round 3). The argv/frame tests prove the snapshot is DELIVERED (`--plugin-dir
/// <snapshot>` reaches the binary); this is the positive proof that the pinned harness LOADS it
/// AND INVOKES the skill: the REAL `claude` on PATH is launched through the real
/// `WrappedCliStepRunner` with `WICKED_SKILLS_SNAPSHOT` pointing at a fixture snapshot — in the
/// `<state home>/skills/snapshots/<gen>` shape the fence derives the state home from (the one
/// input, v3.4 §2) — holding one skill whose `SKILL.md` instructs printing a unique marker. The unit's `skill_ref` names that skill (so the directive
/// is the real `Invoke your skill "wicked-garden:wicked-probe" (via the Skill tool)…`), and the
/// turn must end `Ok` WITH the marker in the output. Round 2 only asked for the roster and
/// grepped the skill's name; a listed skill is not an invoked one. If the harness ignored
/// `--plugin-dir`, could not load the plugin, or did not invoke the skill, the marker cannot
/// appear. The ACP half — the real `claude-agent-acp` through the real `AcpStepRunner` — lives in
/// `acp_runner::tests::the_real_acp_bridge_loads_the_snapshot_and_invokes_the_fixture_skill`.
///
/// `#[ignore]`d by default: it needs `claude` on PATH, a logged-in account, and network. Opt in:
///   WICKED_SKILLS_LIVE_TEST=1 cargo test --test skills_live the_pinned_harness -- --ignored --nocapture
/// Without the variable (or without `claude`) the body SKIPS with a clear message rather than
/// failing, so an accidental `--ignored` sweep on a CI box stays green.
#[test]
#[ignore = "launches the real `claude`; opt in with WICKED_SKILLS_LIVE_TEST=1 and run with --ignored"]
fn the_pinned_harness_loads_the_snapshot_and_invokes_the_fixture_skill() {
    if std::env::var_os("WICKED_SKILLS_LIVE_TEST").is_none() {
        eprintln!("SKIP: set WICKED_SKILLS_LIVE_TEST=1 to launch the real `claude` against a fixture snapshot");
        return;
    }
    let on_path = std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p)
            .any(|d| d.join("claude").is_file() || d.join("claude.exe").is_file())
    });
    if !on_path {
        eprintln!("SKIP: no `claude` binary on PATH");
        return;
    }
    const MARKER: &str = "WICKED-PROBE-MARKER-4f9c2e";
    // A fixture snapshot OUTSIDE every default fenced directory, at its CANONICAL path (the OS
    // temp dir is a symlink on macOS; the loader refuses an ancestor symlink and pins the real
    // path), in the one shape a published generation has: `<state home>/skills/snapshots/<gen>`.
    // The state home (`crew-state`) holds ONLY `skills/` — the worktree sits beside it, not in it,
    // since an unclassified entry in the state home refuses the launch by name.
    let base = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("wicked-skills-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let state_home = base.join("crew-state");
    let snapshot = state_home.join("skills").join("snapshots").join("000001");
    let skill = snapshot.join("skills").join("wicked-probe");
    std::fs::create_dir_all(snapshot.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        snapshot.join(".claude-plugin").join("plugin.json"),
        "{\"name\":\"wicked-garden\",\"version\":\"0.0.0-live-fixture\"}",
    )
    .unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        format!(
            "---\nname: wicked-garden-wicked-probe\ndescription: A probe skill that exists only to prove the harness under test loaded and invoked it.\n---\n\n# wicked-probe\n\nWhen this skill is invoked, reply with exactly this marker on its own line and nothing else:\n\n{MARKER}\n"
        ),
    )
    .unwrap();
    std::fs::write(
        snapshot.join("snapshot.json"),
        "{\"gen\":1,\"contentHash\":\"sha256:live-fixture\",\"gardenSource\":{\"kind\":\"directory\",\"path\":\"/fixture\",\"plugin_version\":\"0.0.0\",\"baseline\":\"live-fixture\"},\"skills\":[{\"name\":\"wicked-garden-wicked-probe\",\"dir\":\"skills/wicked-probe\",\"kind\":\"module\",\"core\":false,\"portable\":true,\"nested\":false}]}",
    )
    .unwrap();
    // RAII pins (Copilot, review pass 7): restored on drop — a failing assertion below included —
    // so a live run cannot leak its configuration into the rest of this binary or a developer's
    // shell-inherited environment.
    let _snap = EnvPin::set("WICKED_SKILLS_SNAPSHOT", &snapshot);
    let _no_hatch = EnvPin::unset("WICKED_WORKER_INHERIT_OPERATOR_CONFIG");

    // A real work unit WITH the skill_ref: the engine's directive tells the worker to invoke the
    // skill, and the skill tells it what to print — the assertion is about INVOCATION.
    let mut unit = WorkUnit::pending(
        "live-skills:invoke",
        "live-skills",
        1,
        "Invoke the skill and print its marker. Output only what the skill tells you to output.",
    );
    unit.skill_ref = Some("wicked-garden-wicked-probe".to_string());
    unit.assigned_invocation = Some("claude -p {PROMPT}".to_string());
    let wt = base.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    let input = StepInput {
        run_id: "live-skills".to_string(),
        unit_ix: 0,
        attempt: 0,
        unit,
        workflow_id: "wf-live-skills".to_string(),
        entity_mode: EntityMode::Shared,
        workdir: Some(wt),
        governance: None,
        prior_outputs: vec![],
        elicitation_epoch: 0,
        process_gen: None,
        launch_seq: 0,
        required_skills: Vec::new(),
    };
    let out = WrappedCliStepRunner::default().run_unit(&input);
    eprintln!(
        "--- live claude reply (status {:?}) ---\n{}\n---",
        out.status, out.output
    );
    assert_eq!(
        out.status,
        wicked_core::StepStatus::Ok,
        "the invocation must succeed: {:?}",
        out.output
    );
    assert!(
        out.output.contains(MARKER),
        "the pinned harness must load the snapshot handed via --plugin-dir AND invoke the skill; got: {:?}",
        out.output
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// RAII pin of one process-global variable, restored on drop — the `EnvPin` discipline of the lib
/// tests (Copilot, review pass 7): a panicking assertion cannot leak a pin.
struct EnvPin {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}
impl EnvPin {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prev }
    }
    fn unset(key: &'static str) -> Self {
        let prev = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, prev }
    }
}
impl Drop for EnvPin {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

// ── Test-harness hygiene (core#311) — not a test ─────────────────────────────────────────────
/// Arm the hermetic emit spool BEFORE main (pre-main is single-threaded, so no test thread can
/// race it): engine paths under test fire coarse fire-and-forget `wicked.*` emissions, and with
/// no shared store configured those spool — which must land in a per-process temp file, never in
/// the operator's real `~/.something-wicked/wicked-apps/emit-outbox.ndjson` replay queue. Every
/// binary in this suite carries this block; `harness_hygiene.rs` fails the suite if one is missing.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
