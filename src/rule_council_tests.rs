//! DES-RULE-CHECK-001 rev 2, Part A: the dispute machinery's own tests (pure: no seats, no store,
//! a council verdict built by hand). Each fails without the change it names.

use super::*;
use crate::decision::{DecisionVerdict, DecisionVote};

/// A fresh judge row on `id`.
fn judged(id: &str, status: RuleStatus, cite: &str) -> RuleVerdict {
    RuleVerdict {
        rule_id: id.into(),
        status,
        how: RuleHow::Judge,
        reason: cite.into(),
        cite: Some(cite.into()),
        judge: Some("pi".into()),
        rule_key: "k".into(),
        evidence_key: Some("e".into()),
        origin: Some(RuleOrigin::Judge),
        judge_ids: vec!["pi".into()],
        evidence_complete: Some(true),
        ..Default::default()
    }
}

fn carried_dispute_ix(r: &RuleVerdict) -> usize {
    carried(&report(vec![r.clone()])).len()
}

fn report(rows: Vec<RuleVerdict>) -> RuleCheckReport {
    RuleCheckReport {
        rows,
        ..Default::default()
    }
}

fn reviewer(text: &str, ids: &[&str]) -> SeatPositions {
    seat_positions(text, ids)
}

/// A council verdict whose deciding ballot cast `votes` (seat, 0-based option, words).
fn verdict(votes: &[(&str, Option<usize>, &str)]) -> DecisionVerdict {
    DecisionVerdict {
        task_id: "task-1".into(),
        winner: None,
        consensus: false,
        agreement_pct: 0,
        returned: votes.len() as u32,
        seated: 3,
        dissent: Vec::new(),
        seats: vec!["agy".into(), "opencode".into(), "copilot".into()],
        seed: 7,
        eligible: vec!["agy".into(), "opencode".into(), "copilot".into()],
        no_ruling_reason: None,
        ballots_in: Some("host".into()),
        votes: votes
            .iter()
            .map(|(s, o, w)| DecisionVote {
                seat: s.to_string(),
                option: *o,
                rationale: w.to_string(),
            })
            .collect(),
    }
}

const PATHS: &[&str] = &["src/add.rs"];

fn bars<'a>(established: bool, instructions: Option<&'a str>) -> VoteBars<'a> {
    VoteBars {
        paths: PATHS,
        rule_text: "add() takes typed numbers when it is a public API\n",
        established,
        instructions,
    }
}

fn positions_for(row: &RuleVerdict, rev: RuleStatus) -> Vec<RulePosition> {
    positions_of(
        row,
        "codex",
        &Answer {
            status: rev,
            cite: "src/add.rs:1 the reviewer's reading".into(),
        },
    )
}

// ── A.1 the reviewer's positions ────────────────────────────────────────────────────────────────

#[test]
fn a_reviewer_broken_line_is_never_hidden_by_an_echoed_kept() {
    // A.7.7: BROKEN, then an echoed KEPT for the same rule: the position is BROKEN.
    let p = reviewer(
        "RULE R1: BROKEN — src/add.rs:1 untyped\n> quoted: RULE R1: KEPT — fine\nRULE R1: KEPT — \
         src/add.rs ok\nVERDICT: PASS",
        &["R1"],
    );
    assert_eq!(p.by_id["R1"].status, RuleStatus::Broken);
    // KEPT then N/A: the last not-broken line.
    let p = reviewer("RULE R2: KEPT — a\nRULE R2: N/A — not a fix", &["R2"]);
    assert_eq!(p.by_id["R2"].status, RuleStatus::NotApplicable);
    assert_eq!(p.by_id["R2"].cite, "not a fix");
    // DEFERRED alone is DEFERRED; beside a KEPT, the KEPT stands.
    let p = reviewer(
        "RULE R3: DEFERRED — at delivery\nRULE R4: DEFERRED — x\nRULE R4: KEPT — y",
        &["R3", "R4"],
    );
    assert_eq!(p.by_id["R3"].status, RuleStatus::Deferred);
    assert_eq!(p.by_id["R4"].status, RuleStatus::Kept);
    assert!(!p.cut);
}

#[test]
fn a_cut_capture_is_marked() {
    let p = reviewer("VERDICT: PASS\n… (output truncated)\n", &["R1"]);
    assert!(p.cut);
}

#[test]
fn only_a_judges_reading_with_exactly_one_broken_side_is_disputed() {
    let machine = RuleVerdict {
        how: RuleHow::Cache,
        origin: Some(RuleOrigin::Machine),
        ..judged("M", RuleStatus::Broken, "no test")
    };
    let settled = RuleVerdict {
        how: RuleHow::Cache,
        origin: Some(RuleOrigin::Council),
        ..judged("C", RuleStatus::Kept, "src/add.rs")
    };
    let process = RuleVerdict {
        blocking: Some(false),
        ..judged("P", RuleStatus::Broken, "x")
    };
    let rows = vec![
        judged("A", RuleStatus::Broken, "src/add.rs untyped"),
        judged("B", RuleStatus::Kept, "src/add.rs typed"),
        judged("NA", RuleStatus::Kept, "src/add.rs"),
        machine,
        settled,
        judged("D", RuleStatus::Broken, "src/add.rs"),
        process,
        RuleVerdict {
            status: RuleStatus::Skipped,
            origin: Some(RuleOrigin::Operator),
            ..judged("S", RuleStatus::Skipped, "x")
        },
    ];
    let p = reviewer(
        "RULE A: KEPT — fine\nRULE B: BROKEN — src/add.rs no\nRULE NA: N/A — x\nRULE M: KEPT — \
         src/add.rs\nRULE C: BROKEN — x\nRULE D: DEFERRED — delivery\nRULE P: KEPT — x\nRULE S: \
         BROKEN — x",
        &["A", "B", "NA", "M", "C", "D", "P", "S"],
    );
    let ds = disputes(&report(rows.clone()), &p);
    let ids: Vec<&str> = ds.iter().map(|i| rows[*i].rule_id.as_str()).collect();
    // A.7.8: a machine BROKEN (cached) is never voted on; A.7.7: a DEFERRED never disputes.
    assert_eq!(ids, vec!["A", "B"]);
}

// ── A.4 the vote bars and the ruling ────────────────────────────────────────────────────────────

#[test]
fn a_3_0_broken_ruling_denies_labelled_settled_by_council() {
    let row = judged("R1", RuleStatus::Broken, "src/add.rs:1 untyped");
    let pos = positions_for(&row, RuleStatus::Kept);
    let v = verdict(&[
        ("agy", Some(0), "1 src/add.rs:1 takes untyped numbers"),
        ("opencode", Some(0), "1 broken"),
        ("copilot", Some(0), "1 broken"),
    ]);
    let out = settle(&row, pos, &v, &bars(false, None));
    assert_eq!(out.status, RuleStatus::Broken);
    assert_eq!(out.origin, Some(RuleOrigin::Council));
    assert_eq!(out.how, RuleHow::Council);
    let r = report(vec![out]);
    let d = r.denial().expect("a broken ruling denies");
    assert!(
        d.reason
            .contains("R1 — settled by council 3-0 (agy, opencode, copilot): broken"),
        "{}",
        d.reason
    );
    assert!(r.hold().is_none());
    assert!(
        r.summary().contains("1 settled by council"),
        "{}",
        r.summary()
    );
}

#[test]
fn a_kept_ruling_with_cites_passes_and_a_2_1_stands() {
    let row = judged("R1", RuleStatus::Broken, "src/add.rs:1 untyped");
    for (votes, tally) in [
        (
            vec![
                ("agy", Some(1), "2 src/add.rs:1 types both numbers"),
                ("opencode", Some(1), "2 src/add.rs keeps it"),
                ("copilot", Some(1), "2 see src/add.rs"),
            ],
            "3-0",
        ),
        (
            vec![
                ("agy", Some(1), "2 src/add.rs:1 types both numbers"),
                ("opencode", Some(1), "2 src/add.rs keeps it"),
                ("copilot", Some(0), "1 broken"),
            ],
            "2-1",
        ),
    ] {
        let v = verdict(&votes);
        let out = settle(
            &row,
            positions_for(&row, RuleStatus::Kept),
            &v,
            &bars(false, None),
        );
        assert_eq!(out.status, RuleStatus::Kept, "{out:?}");
        assert_eq!(out.council.as_ref().unwrap().tally(), tally);
        let r = report(vec![out]);
        assert!(r.denial().is_none() && r.hold().is_none());
    }
}

#[test]
fn kept_votes_without_a_changed_file_cite_do_not_count() {
    // A.7.3: two KEPT with no changed-file cite and one BROKEN: no ruling, a hold.
    let row = judged("R1", RuleStatus::Broken, "src/add.rs:1 untyped");
    let v = verdict(&[
        ("agy", Some(1), "2 looks fine to me"),
        ("opencode", Some(1), "2 add.rs is fine"),
        ("copilot", Some(0), "1 broken"),
    ]);
    let out = settle(
        &row,
        positions_for(&row, RuleStatus::Kept),
        &v,
        &bars(false, None),
    );
    assert_eq!(out.status, RuleStatus::Disputed);
    let c = out.council.as_ref().unwrap();
    assert_eq!(c.votes.iter().filter(|v| v.counted).count(), 1);
    assert!(
        c.reason.as_deref().unwrap().starts_with("no_quorum"),
        "{c:?}"
    );
    let r = report(vec![out]);
    assert!(r.hold().is_some(), "disputed holds");
    assert!(r.denial().is_none());
}

#[test]
fn not_applicable_votes_need_the_rules_own_words_and_no_established_fact() {
    let row = judged("R1", RuleStatus::Broken, "src/add.rs:1 untyped");
    let pos = positions_for(&row, RuleStatus::NotApplicable);
    assert_eq!(
        dispute_options(&pos),
        vec![RuleStatus::Broken, RuleStatus::NotApplicable]
    );
    let quoted = verdict(&[
        (
            "agy",
            Some(1),
            "2 the rule binds only \"when it is a public API\"; add is private",
        ),
        (
            "opencode",
            Some(1),
            "2 \"when it is a public API\" does not hold",
        ),
        ("copilot", Some(0), "1 broken"),
    ]);
    // On an `applies` rule whose match established a fact: no N/A counts.
    let out = settle(&row, pos.clone(), &quoted, &bars(true, None));
    assert_eq!(out.status, RuleStatus::Disputed, "{out:?}");
    // With no established fact, a quoted scope condition counts.
    let out = settle(&row, pos.clone(), &quoted, &bars(false, None));
    assert_eq!(out.status, RuleStatus::NotApplicable, "{out:?}");
    // A quote of the engine's header around the rule is not the rule's words.
    let header = verdict(&[
        (
            "agy",
            Some(1),
            "2 \"RULE R1 (Warn\" says nothing of this change",
        ),
        ("opencode", Some(1), "2 \"RULE R1 (Warn\" again"),
        ("copilot", Some(0), "1 broken"),
    ]);
    assert_eq!(
        settle(&row, pos.clone(), &header, &bars(false, None)).status,
        RuleStatus::Disputed
    );
    // An N/A that quotes nothing of the rule does not.
    let bare = verdict(&[
        ("agy", Some(1), "2 it does not apply"),
        ("opencode", Some(1), "2 out of scope"),
        ("copilot", Some(0), "1 broken"),
    ]);
    assert_eq!(
        settle(&row, pos, &bare, &bars(false, None)).status,
        RuleStatus::Disputed
    );
}

#[test]
fn a_kept_ruling_resting_on_the_instruction_is_waived_and_holds() {
    let row = judged("R1", RuleStatus::Broken, "src/add.rs:1 no test");
    let v = verdict(&[
        (
            "agy",
            Some(1),
            "2 src/add.rs: the instruction says touch no test",
        ),
        (
            "opencode",
            Some(1),
            "2 src/add.rs as requested by the person",
        ),
        ("copilot", Some(0), "1 broken"),
    ]);
    let out = settle(
        &row,
        positions_for(&row, RuleStatus::Kept),
        &v,
        &bars(false, Some("fix it, touch no test")),
    );
    assert_eq!(out.status, RuleStatus::Waived);
    assert!(report(vec![out]).hold().is_some());
}

#[test]
fn a_council_with_no_ruling_is_disputed_and_holds() {
    // A.7.2: 1 of 3 answered; a timeout; an error — each a disputed row that holds.
    let row = judged("R1", RuleStatus::Kept, "src/add.rs fine");
    let mut one = verdict(&[("agy", Some(1), "2 src/add.rs")]);
    one.returned = 1;
    let mut timed = verdict(&[]);
    timed.no_ruling_reason = Some("council no seat returned a vote — agy: timed_out".into());
    for v in [one, timed] {
        let out = settle(
            &row,
            positions_for(&row, RuleStatus::Broken),
            &v,
            &bars(false, None),
        );
        assert_eq!(out.status, RuleStatus::Disputed, "{out:?}");
        let r = report(vec![out]);
        let h = r.hold().expect("holds");
        assert_eq!(h.source, DENIAL_SOURCE_UNCHECKED);
        assert!(h.rule_ids == ["R1"]);
        assert!(
            r.summary().contains("1 disputed (you decide)"),
            "{}",
            r.summary()
        );
    }
}

// ── A.2 the request ─────────────────────────────────────────────────────────────────────────────

#[test]
fn the_request_carries_both_readings_bounded_inside_a_fresh_fence() {
    let r = rule("R1", "add() takes typed numbers");
    let mut row = judged(
        "R1",
        RuleStatus::Broken,
        &format!("src/add.rs {}", "x\n".repeat(900)),
    );
    let c = change();
    let ev = Evidence::of_change(&row, &c, Some("repo checks passed"));
    let pos = positions_for(&row, RuleStatus::Kept);
    let a = dispute_request(
        "s",
        2,
        &r,
        "  Engine: may apply — unscoped\n",
        &ev,
        None,
        &pos,
    )
    .unwrap();
    let b = dispute_request(
        "s",
        2,
        &r,
        "  Engine: may apply — unscoped\n",
        &ev,
        None,
        &pos,
    )
    .unwrap();
    assert_eq!(a.options, vec!["BROKEN", "KEPT"]);
    assert!(a.question.contains("BREAK steering rule `R1`"));
    assert!(a.evidence.contains(DIFF), "the judge's diff, whole");
    assert!(a
        .evidence
        .contains("the rule judge said BROKEN: src/add.rs"));
    assert!(a.evidence.contains("the reviewer said KEPT:"));
    let judge_line = a
        .evidence
        .lines()
        .find(|l| l.contains("the rule judge said"))
        .unwrap();
    assert!(
        judge_line.len() < 700,
        "a reading is at most 600 bytes, one line"
    );
    let fence = |e: &str| e.split_whitespace().next().unwrap().to_string();
    assert_ne!(
        fence(&a.evidence),
        fence(&b.evidence),
        "a fresh fence per request"
    );
    // Incomplete evidence (the judge's diff was cut) is never put to a council.
    row.evidence_complete = Some(false);
    let ev = Evidence::of_change(&row, &c, None);
    assert!(dispute_request("s", 2, &r, "", &ev, None, &pos).is_none());
    // Neither is evidence too large for a ballot whole.
    row.evidence_complete = Some(true);
    let big = change_of(&"+x\n".repeat(60 * 1024), &["src/add.rs"]);
    let ev = Evidence::of_change(&row, &big, None);
    assert!(dispute_request("s", 2, &r, "", &ev, None, &pos).is_none());
}

// ── A.5 reuse ───────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_council_ruling_is_reused_but_a_disputed_row_only_lends_its_judge_reading() {
    // A.7.9: a council row is reused as settled; a disputed one never is as a ruling — it is
    // carried open with both readings, so a retry convenes the council again without the judge.
    let row = judged("R1", RuleStatus::Broken, "src/add.rs:1 untyped");
    let v = verdict(&[
        ("agy", Some(1), "2 src/add.rs typed"),
        ("opencode", Some(1), "2 src/add.rs ok"),
        ("copilot", Some(1), "2 src/add.rs ok"),
    ]);
    let settled = settle(
        &row,
        positions_for(&row, RuleStatus::Kept),
        &v,
        &bars(false, None),
    );
    let again = reuse(&settled).expect("a ruling is reused");
    assert_eq!(
        (again.how, again.origin),
        (RuleHow::Cache, Some(RuleOrigin::Council))
    );
    assert_eq!(reuse(&again).unwrap().origin, Some(RuleOrigin::Council));
    let open = disputed_row(
        &row,
        RuleCouncil {
            positions: positions_for(&row, RuleStatus::Kept),
            reason: Some("timeout: x".into()),
            ..Default::default()
        },
    );
    let carried = reuse(&open).expect("carried open");
    assert_eq!(
        carried.status,
        RuleStatus::Disputed,
        "never reused as a ruling"
    );
    assert_eq!(carried.how, RuleHow::Cache);
    assert_eq!(carried.origin, Some(RuleOrigin::Judge));
    assert_eq!(carried.judge_ids, vec!["pi".to_string()]);
    assert_eq!(carried.council.as_ref().unwrap().positions.len(), 2);
    assert_eq!(carried_dispute_ix(&carried), 1);
    assert_eq!(judge_reading(&carried).unwrap().status, RuleStatus::Broken);
    // A legacy row reused once (origin unknown) is ruled again, never guessed.
    let legacy = RuleVerdict {
        how: RuleHow::Cache,
        origin: None,
        ..row.clone()
    };
    assert!(reuse(&legacy).is_none());
}

#[test]
fn an_earlier_gates_council_ruling_is_reused_only_on_equal_keys() {
    let rules = [rule("R1", "add() takes typed numbers")];
    let c = change();
    // A first check gives the row its real keys.
    let judge = Fake::new(&[("R1", "BROKEN — src/add.rs:1 untyped")]);
    let first = run_on(&rules, &c, &judge, &NoMachine, None, &[]);
    let v = verdict(&[
        ("agy", Some(1), "2 src/add.rs typed"),
        ("opencode", Some(1), "2 src/add.rs ok"),
        ("copilot", Some(1), "2 src/add.rs ok"),
    ]);
    let ruled = settle(
        &first.rows[0],
        positions_for(&first.rows[0], RuleStatus::Kept),
        &v,
        &bars(false, None),
    );
    let f = job_facts(&[], None, None, &c);
    let seats = vec!["codex".to_string()];
    let qe = |change: &Change, judge: &Fake| {
        let f = job_facts(&[], None, None, change);
        check(
            &CheckInput {
                rules: &rules,
                facts: &f,
                change,
                floor: Some("repo checks passed; test: passed"),
                prior: None,
                skips: &[],
                seats: &seats,
                no_seat: None,
                base: "b0",
                instructions: None,
                engine_record: None,
                creator_records: None,
                reviewer_record: None,
                no_agent_code: false,
                stage: CheckStage::Review,
                deliver_tokens: &[],
                deferred: &[],
                settled: std::slice::from_ref(&ruled),
            },
            judge,
            &NoMachine,
        )
    };
    let _ = f;
    let j2 = Fake::new(&[("R1", "BROKEN — src/add.rs:1 untyped")]);
    let same = qe(&c, &j2);
    assert_eq!(
        j2.calls.load(Ordering::SeqCst),
        0,
        "reused: no judge, no council"
    );
    assert_eq!(same.rows[0].status, RuleStatus::Kept);
    assert_eq!(same.rows[0].origin, Some(RuleOrigin::Council));
    // A changed tree: ruled again.
    let changed = change_of(&format!("{DIFF}+// more\n"), &["src/add.rs"]);
    let j3 = Fake::new(&[("R1", "BROKEN — src/add.rs:1 untyped")]);
    let other = qe(&changed, &j3);
    assert_eq!(j3.calls.load(Ordering::SeqCst), 1);
    assert_eq!(other.rows[0].origin, Some(RuleOrigin::Judge));
}

#[test]
fn a_fresh_judge_row_says_whether_its_evidence_was_whole() {
    let rules = [rule("R1", "add() takes typed numbers")];
    let judge = Fake::new(&[("R1", "BROKEN — src/add.rs:1 untyped")]);
    let r = run(&rules, &judge, None);
    assert_eq!(r.rows[0].origin, Some(RuleOrigin::Judge));
    assert_eq!(r.rows[0].evidence_complete, Some(true));
    // A diff larger than one call can show: a BROKEN still counts, but it is not whole.
    let big = change_of(
        &format!("{DIFF}{}", "+// pad\n".repeat(20_000)),
        &["src/add.rs"],
    );
    let judge = Fake::new(&[("R1", "BROKEN — src/add.rs:1 untyped")]);
    let r = run_on(&rules, &big, &judge, &NoMachine, None, &[]);
    assert_eq!(r.rows[0].status, RuleStatus::Broken);
    assert_eq!(r.rows[0].evidence_complete, Some(false));
}

/// Q2: the rework brief names a rule the reviewer called BROKEN and a council kept.
#[test]
fn the_rework_brief_names_a_rule_the_council_kept_over_the_reviewer() {
    let row = judged("R1", RuleStatus::Kept, "src/add.rs:1 typed");
    let v = verdict(&[
        ("agy", Some(1), "2 src/add.rs:1 types both numbers"),
        ("opencode", Some(1), "2 src/add.rs ok"),
        ("copilot", Some(0), "1 broken"),
    ]);
    let kept = settle(
        &row,
        positions_for(&row, RuleStatus::Broken),
        &v,
        &bars(false, None),
    );
    let note = council_kept_notes(Some(&report(vec![kept]))).expect("a line");
    assert!(
        note.contains("the council kept R1 2-1: 2 src/add.rs:1 types both numbers"),
        "{note}"
    );
    // A kept rule the reviewer did not call broken needs no line.
    let other = settle(
        &row,
        positions_for(&row, RuleStatus::Broken),
        &v,
        &bars(false, None),
    );
    let mut quiet = other.clone();
    quiet.council.as_mut().unwrap().positions[1].status = RuleStatus::Kept;
    assert!(council_kept_notes(Some(&report(vec![quiet]))).is_none());
}
