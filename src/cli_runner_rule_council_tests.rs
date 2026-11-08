//! DES-RULE-CHECK-001 rev 2, Part A: the settle step on the review path — the reviewer's positions,
//! who sits on a rule council, its bounds, and what a ruling or no ruling does to the row. A fake
//! judge runner and a fake council; a real repository and policy store (the parent's fixture).

use super::*;
use crate::decision::{DecisionRequest, DecisionVerdict, DecisionVote};
use crate::rule_check::{RuleOrigin, RuleStatus};
use std::sync::atomic::{AtomicU32, Ordering};

/// The judge: answers every asked rule with `R1`'s word for R1 (and the others' from `rest`).
struct Judging {
    r1: &'static str,
    rest: &'static str,
    calls: AtomicU32,
}

impl Judging {
    fn says(r1: &'static str) -> Self {
        Judging {
            r1,
            rest: "N/A — not a fix",
            calls: AtomicU32::new(0),
        }
    }
}

impl StepRunner for Judging {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let p = &input.unit.description;
        let nonce = crate::rule_check::tests::nonce_of(p);
        let mut body = String::new();
        for l in p.lines().skip_while(|l| *l != "RULES:").skip(1) {
            let Some(id) = l.strip_prefix("RULE ").and_then(|r| r.split(" (").next()) else {
                continue;
            };
            let word = if id == "R1" { self.r1 } else { self.rest };
            body.push_str(&format!("RULE {id}: {word}\n"));
        }
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: 0,
            attempt: 0,
            output: format!("BEGIN VERDICTS {nonce}\n{body}END VERDICTS {nonce}\n"),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
}

type Answer = fn(&DecisionRequest, &[crate::AgenticCli]) -> DisputeOutcome;

/// A council that answers with `answer`, records every request and the seats it was handed, and
/// checks the host slot is held (by the guard it was handed) for as long as it runs.
struct FakeCouncil {
    answer: Answer,
    lock_path: std::path::PathBuf,
    seen: Mutex<Vec<(DecisionRequest, Vec<String>)>>,
    /// The run's councils left (the actor's durable count, faked).
    left: Mutex<u32>,
}

impl FakeCouncil {
    fn new(tag: &str, answer: Answer) -> Self {
        let lock_path =
            std::env::temp_dir().join(format!("rule-council-{tag}-{}.lock", std::process::id()));
        FakeCouncil {
            answer,
            lock_path,
            seen: Mutex::new(Vec::new()),
            left: Mutex::new(crate::rule_check::RUN_COUNCILS),
        }
    }
    fn convened(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
    fn seats(&self, k: usize) -> Vec<String> {
        let mut s = self.seen.lock().unwrap()[k].1.clone();
        s.sort();
        s
    }
}

impl DisputeCouncil for FakeCouncil {
    fn reserve(&self, _run_id: &str, _ord: u32) -> Result<bool, String> {
        let mut left = self.left.lock().unwrap();
        Ok(match left.checked_sub(1) {
            Some(n) => {
                *left = n;
                true
            }
            None => false,
        })
    }

    fn convene(
        &self,
        req: DecisionRequest,
        clis: Vec<crate::AgenticCli>,
        lock: HostCouncilLock,
        deadline: std::time::Instant,
    ) -> DisputeOutcome {
        assert_eq!(
            HostCouncilLock::acquire_until(&self.lock_path, std::time::Instant::now()).err(),
            Some(SlotRefused::Busy),
            "the host slot is held while the council runs"
        );
        assert!(deadline > std::time::Instant::now());
        let out = (self.answer)(&req, &clis);
        self.seen
            .lock()
            .unwrap()
            .push((req, clis.iter().map(|c| c.key.clone()).collect()));
        drop(lock);
        out
    }
}

fn votes(v: &[(&str, usize, &str)]) -> DecisionVerdict {
    DecisionVerdict {
        task_id: "task".into(),
        winner: None,
        consensus: false,
        agreement_pct: 0,
        returned: v.len() as u32,
        seated: 3,
        dissent: Vec::new(),
        seats: v.iter().map(|(s, _, _)| s.to_string()).collect(),
        seed: 1,
        eligible: Vec::new(),
        no_ruling_reason: None,
        ballots_in: Some("host".into()),
        votes: v
            .iter()
            .map(|(s, o, w)| DecisionVote {
                seat: s.to_string(),
                option: Some(*o),
                rationale: w.to_string(),
            })
            .collect(),
    }
}

fn broken_3_0(_: &DecisionRequest, c: &[crate::AgenticCli]) -> DisputeOutcome {
    DisputeOutcome::Ruled(votes(&[
        (&c[0].key, 0, "1 src/a.rs:2 still untyped"),
        (&c[1].key, 0, "1 broken"),
        (&c[2].key, 0, "1 broken"),
    ]))
}

fn kept_3_0(_: &DecisionRequest, c: &[crate::AgenticCli]) -> DisputeOutcome {
    DisputeOutcome::Ruled(votes(&[
        (&c[0].key, 1, "2 src/a.rs:2 types both numbers"),
        (&c[1].key, 1, "2 src/a.rs keeps it"),
        (&c[2].key, 1, "2 src/a.rs ok"),
    ]))
}

/// The registry: the creator (claude), the reviewer (codex), the judge (pi), and three others.
fn registry() -> Vec<crate::AgenticCli> {
    ["claude", "codex", "pi", "agy", "opencode", "copilot"]
        .into_iter()
        .map(seat)
        .collect()
}

/// The review step of the fixture, reviewed by codex as it launched.
fn reviewed(tag: &str) -> StepInput {
    let (repo, base, db) = fixture(tag);
    let mut input = review_input(&repo, &base, &db);
    input.unit.assigned_invocation = Some("codex -p {PROMPT}".into());
    input
}

/// The settle step's run: the creator claude, the judge pool [claude, pi], `reviewer` output.
fn settle_with(
    input: &StepInput,
    judge: &Judging,
    reviewer_output: &str,
    council: &FakeCouncil,
    registry: &[crate::AgenticCli],
) -> crate::rule_check::RuleCheckReport {
    let pool = vec![seat("claude"), seat("pi")];
    let ctx = CouncilCtx {
        council,
        lock_path: &council.lock_path,
    };
    let sink = |_: &str| {};
    run_rule_check_with(
        input,
        &["claude".to_string()],
        Some(&["claude".to_string()][..]),
        &pool,
        registry,
        None,
        judge,
        &sink,
        Some(Settle {
            reviewer_output,
            council: Some(&ctx),
        }),
    )
    .expect("a report")
}

fn row<'a>(
    r: &'a crate::rule_check::RuleCheckReport,
    id: &str,
) -> &'a crate::rule_check::RuleVerdict {
    r.rows.iter().find(|r| r.rule_id == id).expect("a row")
}

const REVIEWER_KEPT: &str =
    "RULE R1: KEPT — src/a.rs:1 typed\nRULE R2: N/A — not a fix\nVERDICT: PASS";

/// A.7.1 (job 9d3e4238): the judge says BROKEN, the reviewer KEPT — exactly one council, on seats
/// distinct from every party. 3-0 BROKEN denies labelled "settled by council"; 3-0 KEPT with cites
/// passes (today it denies).
#[test]
fn a_judge_reviewer_split_goes_to_one_council_and_its_ruling_decides() {
    for (tag, answer, want) in [
        ("split-broken", broken_3_0 as Answer, RuleStatus::Broken),
        ("split-kept", kept_3_0 as Answer, RuleStatus::Kept),
    ] {
        let input = reviewed(tag);
        let judge = Judging::says("BROKEN — src/a.rs:2 add() is untyped");
        let council = FakeCouncil::new(tag, answer);
        let r = settle_with(&input, &judge, REVIEWER_KEPT, &council, &registry());
        assert_eq!(council.convened(), 1, "exactly one council");
        assert_eq!(r.councils, 1);
        assert_eq!(council.seats(0), vec!["agy", "copilot", "opencode"]);
        let (req, _) = &council.seen.lock().unwrap()[0];
        assert!(req.evidence.contains("the rule judge said BROKEN"));
        assert!(req
            .evidence
            .contains("the reviewer said KEPT: src/a.rs:1 typed"));
        assert!(
            !req.evidence.contains("VERDICT: PASS"),
            "the reviewer's transcript is not evidence, only its reading"
        );
        let r1 = row(&r, "R1");
        assert_eq!(r1.status, want);
        assert_eq!(r1.origin, Some(RuleOrigin::Council));
        assert_eq!(r1.judge_ids, vec!["pi".to_string()], "the judge recorded");
        match want {
            RuleStatus::Broken => {
                let d = r.denial().expect("denies");
                assert!(d.reason.contains("settled by council 3-0"), "{}", d.reason);
            }
            _ => assert!(r.denial().is_none() && r.hold().is_none(), "{r:?}"),
        }
        // (e2e-1105 CJ9) R1 is a warning the judge broke (marked waivable by the check): the mark
        // follows the council's ruling, kept by a broken ruling and dropped by a kept one.
        assert_eq!(
            r1.waivable,
            (want == RuleStatus::Broken).then_some(true),
            "{tag}"
        );
        assert_eq!(
            row(&r, "R2").status,
            RuleStatus::NotApplicable,
            "no dispute on R2"
        );
    }
}

/// A.7.2: no ruling — 1 of 3 answered, a timeout, an error — is `disputed`, and holds.
#[test]
fn a_council_with_no_ruling_leaves_the_rule_disputed() {
    fn one(_: &DecisionRequest, c: &[crate::AgenticCli]) -> DisputeOutcome {
        let mut v = votes(&[(&c[0].key, 1, "2 src/a.rs ok")]);
        v.returned = 1;
        DisputeOutcome::Ruled(v)
    }
    fn timeout(_: &DecisionRequest, _: &[crate::AgenticCli]) -> DisputeOutcome {
        DisputeOutcome::TimedOut
    }
    fn error(_: &DecisionRequest, _: &[crate::AgenticCli]) -> DisputeOutcome {
        DisputeOutcome::Failed("the council thread panicked".into())
    }
    for (tag, answer, token) in [
        ("nr-one", one as Answer, "no_quorum"),
        ("nr-timeout", timeout as Answer, "timeout"),
        ("nr-error", error as Answer, "error"),
    ] {
        let input = reviewed(tag);
        let judge = Judging::says("BROKEN — src/a.rs:2 add() is untyped");
        let council = FakeCouncil::new(tag, answer);
        let r = settle_with(&input, &judge, REVIEWER_KEPT, &council, &registry());
        let r1 = row(&r, "R1");
        assert_eq!(r1.status, RuleStatus::Disputed, "{r1:?}");
        let reason = r1.council.as_ref().and_then(|c| c.reason.clone()).unwrap();
        assert!(reason.starts_with(token), "{reason}");
        assert!(
            r.denial().is_none(),
            "the judge's BROKEN no longer denies alone"
        );
        assert!(r.hold().is_some(), "…and the dispute holds instead");
    }
}

/// A.7.4: the creator, its second instance, a seat that execs the creator's binary, an
/// `exclude_seats` alias with the creator's binary, the judge, the reviewer launched as another
/// CLI and a launcher-script seat: none sits. Fewer than three left = no council at all.
#[test]
fn no_party_and_no_unverifiable_seat_sits_on_a_rule_council() {
    let alias = |key: &str, binary: &str, inv: &str| crate::AgenticCli {
        binary: binary.into(),
        headless_invocation: inv.into(),
        ..seat(key)
    };
    let mut reg = vec![
        seat("claude"),
        alias("claude#2", "claude", "claude -p {PROMPT}"),
        alias("mirror", "mirror", "claude -p {PROMPT}"),
        alias("helper", "claude", "claude -p {PROMPT}"),
        seat("pi"),
        seat("codex"),
        seat("opencode"),
        alias("wrapped", "wrapped", "/opt/launch.sh {PROMPT}"),
        // (codex r1) A record that declares its own wrapper as its binary is no shipped CLI.
        alias("selfwrap", "launch.sh", "launch.sh {PROMPT}"),
        alias("shell", "sh", "sh -c \"claude -p {PROMPT}\""),
        seat("agy"),
        seat("copilot"),
    ];
    let seats_for = |reg: &[crate::AgenticCli], rev_inv: &str, bins: &[String]| {
        rule_council_seats(
            &["claude".to_string()],
            Some(bins),
            &["helper".to_string()],
            &[],
            &[],
            (Some("pi"), &["pi".to_string()][..]),
            (Some("codex"), Some(rev_inv)),
            None,
            reg,
            false,
        )
    };
    let claude = ["claude".to_string()];
    // The reviewer `codex` was launched running opencode: opencode is a party too. Two left.
    let err = seats_for(&reg, "opencode run {PROMPT}", &claude).unwrap_err();
    assert!(err.starts_with("seats_benched"), "{err}");
    reg.push(alias("agy#2", "agy", "agy -p {PROMPT}"));
    let mut got: Vec<String> = seats_for(&reg, "opencode run {PROMPT}", &claude)
        .unwrap()
        .into_iter()
        .map(|c| c.key)
        .collect();
    got.sort();
    assert_eq!(got, vec!["agy", "agy#2", "copilot"]);
    // A party launched through a script cannot be seen through…
    let err = seats_for(&reg, "/opt/wrap.sh {PROMPT}", &claude).unwrap_err();
    assert!(err.starts_with("identity_unknown"), "{err}");
    // …nor a creator that ran one (it could be running any CLI underneath).
    let err = seats_for(&reg, "codex -p {PROMPT}", &["wrap.sh".to_string()]).unwrap_err();
    assert!(err.starts_with("identity_unknown"), "{err}");
    // …nor a judge whose identity was not recorded.
    let err = rule_council_seats(
        &["claude".to_string()],
        Some(&["claude".to_string()][..]),
        &[],
        &[],
        &[],
        (Some("pi"), &[]),
        (Some("codex"), Some("codex -p {PROMPT}")),
        None,
        &reg,
        false,
    )
    .unwrap_err();
    assert!(err.starts_with("identity_unknown"), "{err}");
    // (codex r2) …nor a launch-excluded seat the registry cannot name or verify.
    for excluded in ["ghost", "wrapped"] {
        let err = rule_council_seats(
            &["claude".to_string()],
            Some(&["claude".to_string()][..]),
            &[excluded.to_string()],
            &[],
            &[],
            (Some("pi"), &["pi".to_string()][..]),
            (Some("codex"), Some("codex -p {PROMPT}")),
            None,
            &reg,
            false,
        )
        .unwrap_err();
        assert!(err.starts_with("identity_unknown"), "{excluded}: {err}");
    }
    // …nor a carried dispute whose reading has no recorded identity.
    let unrecorded = [crate::rule_check::RulePosition {
        by: crate::rule_check::BY_REVIEWER.into(),
        seat: "codex".into(),
        status: RuleStatus::Broken,
        cite: "x".into(),
        ids: Vec::new(),
    }];
    let err = rule_council_seats(
        &["claude".to_string()],
        Some(&["claude".to_string()][..]),
        &[],
        &[],
        &[],
        (Some("pi"), &["pi".to_string()][..]),
        (Some("codex"), Some("codex -p {PROMPT}")),
        Some(&unrecorded),
        &reg,
        false,
    )
    .unwrap_err();
    assert!(err.starts_with("identity_unknown"), "{err}");
}

/// (codex r2) A carried dispute keeps the reviewer that READ it out of a later council, as it
/// launched then — even when the same seat key now launches another CLI.
#[test]
fn a_carried_dispute_keeps_its_original_reviewer_out_whatever_the_seat_now_runs() {
    fn timeout(_: &DecisionRequest, _: &[crate::AgenticCli]) -> DisputeOutcome {
        DisputeOutcome::TimedOut
    }
    let mut input = reviewed("carried-id");
    // Attempt 1: the reviewer seat `codex` launched running opencode.
    input.unit.assigned_invocation = Some("opencode run {PROMPT}".into());
    let judge = Judging::says("BROKEN — src/a.rs:2 add() is untyped");
    let council = FakeCouncil::new("carried-id", timeout);
    let first = settle_with(&input, &judge, REVIEWER_KEPT, &council, &registry());
    // opencode is a party (the reviewer ran it): two seats left, no council, a recorded dispute.
    assert_eq!(council.convened(), 0);
    let r1 = row(&first, "R1");
    assert_eq!(r1.status, RuleStatus::Disputed);
    let rec = &r1.council.as_ref().unwrap().positions[1];
    assert!(rec.ids.contains(&"opencode".to_string()), "{rec:?}");
    // Attempt 2: the same key now launches codex. opencode read the dispute: it does not sit.
    input.unit.rule_check = Some(first);
    input.unit.assigned_invocation = Some("codex -p {PROMPT}".into());
    let again = FakeCouncil::new("carried-id-2", kept_3_0);
    let r = settle_with(&input, &judge, "VERDICT: PASS", &again, &registry());
    assert_eq!(again.convened(), 0, "two seats left: no council");
    let why = row(&r, "R1")
        .council
        .as_ref()
        .unwrap()
        .reason
        .clone()
        .unwrap();
    assert!(why.starts_with("seats_benched"), "{why}");
}

/// (codex r2) A special file planted at the slot's path (a FIFO) is refused at once: never a
/// blocking open on the unit's worker thread.
#[cfg(unix)]
#[test]
fn a_fifo_at_the_slot_path_is_refused_without_blocking() {
    let path = std::env::temp_dir().join(format!("rule-council-fifo-{}.lock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    let t0 = std::time::Instant::now();
    let got = HostCouncilLock::acquire_until(&path, t0 + Duration::from_secs(30));
    assert!(matches!(got, Err(SlotRefused::Unavailable(_))));
    assert!(t0.elapsed() < Duration::from_secs(5));
    let _ = std::fs::remove_file(&path);
}

/// A.7.4, end to end: too few distinct seats convenes nothing (zero ballots), and the row holds.
#[test]
fn too_few_distinct_seats_convene_nothing_and_the_rule_holds() {
    let input = reviewed("few");
    let judge = Judging::says("BROKEN — src/a.rs:2 add() is untyped");
    let council = FakeCouncil::new("few", broken_3_0);
    let small: Vec<crate::AgenticCli> = ["claude", "codex", "pi", "agy", "opencode"]
        .into_iter()
        .map(seat)
        .collect();
    let r = settle_with(&input, &judge, REVIEWER_KEPT, &council, &small);
    assert_eq!(council.convened(), 0);
    assert_eq!(r.councils, 0);
    assert_eq!(row(&r, "R1").status, RuleStatus::Disputed);
    assert!(r.hold().is_some());
}

/// Two more steering rules in the fixture's store (R3, R4).
fn more_rules(input: &StepInput) {
    let db = &input.governance.as_ref().unwrap().db_path;
    let mut store = wicked_apps_core::open_store(Some(db.as_str())).unwrap();
    for (id, st) in [
        ("R3", "names are snake_case"),
        ("R4", "no panics in library code"),
    ] {
        wicked_governance::register_rule(
            &mut store,
            &wicked_governance::ConformanceRule {
                id: id.into(),
                rule_type: wicked_governance::RuleType::Pattern,
                statement: st.into(),
                severity: wicked_governance::ConfSeverity::Warn,
                ..Default::default()
            },
        )
        .unwrap();
    }
}

/// A.7.5: four disputes in one attempt = three councils and one `cap`; a run that has spent five
/// gets one more and then `cap`.
#[test]
fn councils_are_capped_per_attempt_and_per_run() {
    let all_kept = "RULE R1: KEPT — src/a.rs ok\nRULE R2: KEPT — src/a.rs ok\nRULE R3: KEPT — \
                    src/a.rs ok\nRULE R4: KEPT — src/a.rs ok\nVERDICT: PASS";
    let judge = || Judging {
        r1: "BROKEN — src/a.rs:2 no",
        rest: "BROKEN — src/a.rs:2 no",
        calls: AtomicU32::new(0),
    };
    let input = reviewed("cap-attempt");
    more_rules(&input);
    let council = FakeCouncil::new("cap-attempt", broken_3_0);
    let r = settle_with(&input, &judge(), all_kept, &council, &registry());
    assert_eq!(council.convened(), 3);
    assert_eq!(r.councils, 3);
    let capped: Vec<&str> = r
        .rows
        .iter()
        .filter(|r| r.status == RuleStatus::Disputed)
        .map(|r| r.rule_id.as_str())
        .collect();
    assert_eq!(capped.len(), 1);
    let why = row(&r, capped[0])
        .council
        .as_ref()
        .unwrap()
        .reason
        .clone()
        .unwrap();
    assert!(why.starts_with("cap: this gate attempt"), "{why}");

    let input = reviewed("cap-run");
    more_rules(&input);
    let council = FakeCouncil::new("cap-run", broken_3_0);
    *council.left.lock().unwrap() = 1;
    let r = settle_with(&input, &judge(), all_kept, &council, &registry());
    assert_eq!(council.convened(), 1, "one left in the run's budget");
    let caps = r
        .rows
        .iter()
        .filter(|r| {
            r.council
                .as_ref()
                .and_then(|c| c.reason.as_deref())
                .is_some_and(|w| w.starts_with("cap: the run"))
        })
        .count();
    assert_eq!(caps, 3);
}

/// A.7.7: a reviewer's DEFERRED against a judge's BROKEN convenes nothing, and the BROKEN denies;
/// a reviewer BROKEN with an echoed KEPT after it still disputes a judge's KEPT.
#[test]
fn a_reviewer_deferral_never_disputes_and_an_echo_never_hides_a_broken() {
    let input = reviewed("deferred");
    let judge = Judging::says("BROKEN — src/a.rs:2 add() is untyped");
    let council = FakeCouncil::new("deferred", broken_3_0);
    let r = settle_with(
        &input,
        &judge,
        "RULE R1: DEFERRED — at delivery\nVERDICT: PASS",
        &council,
        &registry(),
    );
    assert_eq!(council.convened(), 0);
    assert_eq!(row(&r, "R1").status, RuleStatus::Broken);
    assert_eq!(
        row(&r, "R1").reviewer.as_ref().unwrap().status,
        RuleStatus::Deferred
    );
    assert!(r.denial().is_some());

    let input = reviewed("echo");
    let judge = Judging::says("KEPT — src/a.rs:2 typed");
    let council = FakeCouncil::new("echo", kept_3_0);
    let _ = settle_with(
        &input,
        &judge,
        "RULE R1: BROKEN — src/a.rs:2 a test is missing\n> RULE R1: KEPT — echoed\nVERDICT: PASS",
        &council,
        &registry(),
    );
    assert_eq!(council.convened(), 1);
}

/// A.7.9: a re-check on an unchanged envelope reuses the ruling (no judge, no council); a disputed
/// row is never reused as a ruling — the re-check convenes again, on the judge's recorded reading.
#[test]
fn a_recheck_reuses_a_ruling_and_reconvenes_on_a_disputed_row() {
    let mut input = reviewed("reuse");
    let judge = Judging::says("BROKEN — src/a.rs:2 add() is untyped");
    let council = FakeCouncil::new("reuse", kept_3_0);
    let first = settle_with(&input, &judge, REVIEWER_KEPT, &council, &registry());
    assert_eq!(row(&first, "R1").status, RuleStatus::Kept);
    input.unit.rule_check = Some(first);
    let judge2 = Judging::says("BROKEN — src/a.rs:2 add() is untyped");
    let council2 = FakeCouncil::new("reuse-2", kept_3_0);
    let again = settle_with(&input, &judge2, REVIEWER_KEPT, &council2, &registry());
    assert_eq!(council2.convened(), 0, "the ruling is reused");
    assert_eq!(judge2.calls.load(Ordering::SeqCst), 0);
    assert_eq!(row(&again, "R1").origin, Some(RuleOrigin::Council));

    fn timeout(_: &DecisionRequest, _: &[crate::AgenticCli]) -> DisputeOutcome {
        DisputeOutcome::TimedOut
    }
    let mut input = reviewed("reuse-open");
    let council = FakeCouncil::new("reuse-open", timeout);
    let open = settle_with(&input, &judge, REVIEWER_KEPT, &council, &registry());
    assert_eq!(row(&open, "R1").status, RuleStatus::Disputed);
    input.unit.rule_check = Some(open);
    let judge3 = Judging::says("KEPT — src/a.rs:1 (a judge asked again would say this)");
    let council3 = FakeCouncil::new("reuse-open-2", broken_3_0);
    let r = settle_with(&input, &judge3, REVIEWER_KEPT, &council3, &registry());
    assert_eq!(council3.convened(), 1, "a disputed row convenes again");
    assert_eq!(
        judge3.calls.load(Ordering::SeqCst),
        0,
        "on the judge's recorded reading"
    );
    assert_eq!(row(&r, "R1").status, RuleStatus::Broken);
}

/// (codex r1, P1) An OPEN dispute on unchanged evidence stands: the judge KEPT, the reviewer said
/// BROKEN, no council ruled. A later review that omits its BROKEN line does not make the rule pass:
/// the carried dispute holds, and is put to a council again on its recorded readings.
#[test]
fn an_open_dispute_stands_on_the_same_evidence_whatever_a_later_review_writes() {
    fn timeout(_: &DecisionRequest, _: &[crate::AgenticCli]) -> DisputeOutcome {
        DisputeOutcome::TimedOut
    }
    let mut input = reviewed("open-kept");
    let judge = Judging::says("KEPT — src/a.rs:2 typed");
    let council = FakeCouncil::new("open-kept", timeout);
    let first = settle_with(
        &input,
        &judge,
        "RULE R1: BROKEN — src/a.rs:2 no test ships with the fix\nVERDICT: FAIL",
        &council,
        &registry(),
    );
    assert_eq!(row(&first, "R1").status, RuleStatus::Disputed);
    input.unit.rule_check = Some(first);
    // No council this time (none left): still disputed, still holding.
    let none = FakeCouncil::new("open-kept-2", kept_3_0);
    *none.left.lock().unwrap() = 0;
    let r = settle_with(&input, &judge, "VERDICT: PASS", &none, &registry());
    assert_eq!(row(&r, "R1").status, RuleStatus::Disputed);
    assert!(r.hold().is_some(), "never a pass on an unsettled dispute");
    // With one: convened again on the recorded readings, and its ruling decides.
    let again = FakeCouncil::new("open-kept-3", kept_3_0);
    let r = settle_with(&input, &judge, "VERDICT: PASS", &again, &registry());
    assert_eq!(again.convened(), 1);
    let (req, _) = &again.seen.lock().unwrap()[0];
    assert!(req
        .evidence
        .contains("the reviewer said BROKEN: src/a.rs:2 no test"));
    assert_eq!(row(&r, "R1").status, RuleStatus::Kept);
}

/// (codex r1) A host where the slot's lock cannot be established convenes no council: no
/// per-process fallback, the dispute holds.
#[test]
fn no_council_runs_where_the_host_slot_cannot_be_established() {
    let input = reviewed("no-slot");
    let judge = Judging::says("BROKEN — src/a.rs:2 add() is untyped");
    let mut council = FakeCouncil::new("no-slot", broken_3_0);
    council.lock_path = std::path::PathBuf::from("/nonexistent-wicked-dir/rule-council.lock");
    let r = settle_with(&input, &judge, REVIEWER_KEPT, &council, &registry());
    assert_eq!(council.convened(), 0);
    let r1 = row(&r, "R1");
    assert_eq!(r1.status, RuleStatus::Disputed);
    let why = r1.council.as_ref().unwrap().reason.clone().unwrap();
    assert!(why.contains("cannot be established"), "{why}");
}

/// A.7.11: a reviewer output the runner cut, with an early PASS, against a judge KEPT: held, never
/// approved, and no council (its positions cannot be read whole).
#[test]
fn a_cut_reviewer_capture_holds_every_rule_it_could_have_disputed() {
    let input = reviewed("cut");
    let judge = Judging::says("KEPT — src/a.rs:2 typed");
    let council = FakeCouncil::new("cut", kept_3_0);
    let r = settle_with(
        &input,
        &judge,
        "VERDICT: PASS\nRULE R1: KEPT — src/a.rs\n… (output truncated)\n",
        &council,
        &registry(),
    );
    assert_eq!(council.convened(), 0);
    assert_eq!(row(&r, "R1").status, RuleStatus::Disputed);
    assert_eq!(row(&r, "R2").status, RuleStatus::Disputed);
    assert!(r.hold().is_some());
}

/// A.7.5: the host slot — two holders never at once, and a caller that stops waiting leaves it
/// held until the council itself ends.
#[test]
fn the_host_slot_is_one_at_a_time_and_outlives_a_caller_that_stops_waiting() {
    let path = std::env::temp_dir().join(format!("rule-council-slot-{}.lock", std::process::id()));
    let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let overlaps = std::sync::Arc::new(AtomicU32::new(0));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (path, busy, overlaps) = (path.clone(), busy.clone(), overlaps.clone());
            std::thread::spawn(move || {
                for _ in 0..3 {
                    let g = HostCouncilLock::acquire_until(
                        &path,
                        std::time::Instant::now() + Duration::from_secs(30),
                    )
                    .expect("the slot comes free");
                    if busy.swap(true, Ordering::SeqCst) {
                        overlaps.fetch_add(1, Ordering::SeqCst);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                    busy.store(false, Ordering::SeqCst);
                    drop(g);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(overlaps.load(Ordering::SeqCst), 0);

    // The production port: the caller gives up at deadline + grace; the "actor" still holds the
    // slot it was handed until its council ends.
    let (tx, rx) = std::sync::mpsc::channel::<crate::command::Command>();
    let actor = std::thread::spawn(move || {
        if let Ok(crate::command::Command::ConveneRuleCouncil { lock, reply, .. }) = rx.recv() {
            std::thread::sleep(Duration::from_millis(800));
            drop(lock);
            let _ = reply.send(Err(anyhow::anyhow!("late")));
        }
    });
    let port = ActorDisputeCouncil { tx };
    // A few seconds, not zero: a child another test forks may hold the lock's open file for an
    // instant before its exec closes it (the production wait retries until its deadline too).
    let soon = || std::time::Instant::now() + Duration::from_secs(5);
    let lock = HostCouncilLock::acquire_until(&path, soon()).expect("the slot is free");
    let past = std::time::Instant::now() - crate::rule_check::COUNCIL_REPLY_GRACE
        + Duration::from_millis(100);
    let req = DecisionRequest {
        session_id: "s".into(),
        ord: 1,
        question: "q".into(),
        options: vec!["BROKEN".into(), "KEPT".into()],
        evidence: "e".into(),
    };
    assert!(matches!(
        port.convene(req, Vec::new(), lock, past),
        DisputeOutcome::TimedOut
    ));
    assert!(
        HostCouncilLock::acquire_until(&path, std::time::Instant::now()).is_err(),
        "still held by the council after the caller stopped waiting"
    );
    actor.join().unwrap();
    assert!(HostCouncilLock::acquire_until(&path, soon()).is_ok());
}
