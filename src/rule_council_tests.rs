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
        arbiter: false,
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
fn a_rejected_arbiter_ballot_reports_the_single_seated_voter() {
    let row = judged("R1", RuleStatus::Broken, "src/add.rs:1 untyped");
    let v = DecisionVerdict {
        seated: 1,
        seats: vec!["pi".into()],
        eligible: vec!["pi".into()],
        no_ruling_reason: Some("council no seat returned a vote — pi: ballot rejected".into()),
        ..verdict(&[])
    };
    let out = settle(
        &row,
        positions_for(&row, RuleStatus::Kept),
        &v,
        &VoteBars {
            arbiter: true,
            ..bars(false, None)
        },
    );
    assert_eq!(out.status, RuleStatus::Disputed);
    assert!(out.reason.contains("0 of 1 answered"), "{}", out.reason);
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

/// (core#1746) A council ballot carries the change as one argument too: a binary hunk's NULs never
/// ride it (a NUL argument reaches the seat as `<string-with-nul>`), and nothing of the hunk is cut.
#[test]
fn a_dispute_over_a_binary_change_carries_no_nul_and_keeps_the_hunk() {
    let r = rule("STACK-1", "Use what the chosen stack already provides.");
    let paths = [
        "scripts/bump.py",
        "scripts/__pycache__/bump.cpython-313.pyc",
    ];
    let c = change_of(super::BINARY_DIFF, &paths);
    let row = judged(
        "STACK-1",
        RuleStatus::Broken,
        "scripts/bump.py rewrites a parser",
    );
    let ev = Evidence::of_change(&row, &c, None);
    let pos = positions_for(&row, RuleStatus::Kept);
    let req = dispute_request("s", 3, &r, "", &ev, None, &pos).unwrap();
    assert!(!req.evidence.contains('\0'), "no NUL in a ballot");
    assert!(
        req.evidence.contains("after-the-nul tomllib"),
        "the hunk, whole"
    );
}

/// A big change: `src/add.rs` (DIFF), optionally `src/b.rs`, and 200 KB of `gen/big.json`.
fn big_change(with_b: bool) -> Change {
    let b = "diff --git a/src/b.rs b/src/b.rs\n--- a/src/b.rs\n+++ b/src/b.rs\n@@ -1 +1 @@\n\
             -fn b() {}\n+fn b(x: f64) {}\n";
    let diff = format!(
        "{DIFF}{}diff --git a/gen/big.json b/gen/big.json\n--- a/gen/big.json\n\
         +++ b/gen/big.json\n@@ -0,0 +1 @@\n{}",
        if with_b { b } else { "" },
        "+\"BIG-ROW\"\n".repeat(20 * 1024)
    );
    let mut paths = vec!["src/add.rs", "gen/big.json"];
    if with_b {
        paths.insert(1, "src/b.rs");
    }
    change_of(&diff, &paths)
}

/// The judge's reading `judge` against the reviewer's `reviewer` (status, cite).
fn split(
    judge: (RuleStatus, &str),
    reviewer: (RuleStatus, &str),
) -> (RuleVerdict, Vec<RulePosition>) {
    let row = judged("R1", judge.0, judge.1);
    let pos = positions_of(
        &row,
        "codex",
        &Answer {
            status: reviewer.0,
            cite: reviewer.1.into(),
        },
    );
    (row, pos)
}

/// (core#1789) A dispute on a change too big for one ballot (a paged change), where the reviewer
/// claims the break the judge did not find, goes to the council on the files the reviewer named:
/// their whole diff, every other changed file listed as not shown, and a KEPT vote counted only
/// when it names a file shown.
#[test]
fn core_1789_a_paged_dispute_reaches_a_council_on_the_files_the_reviewer_named() {
    let r = rule("R1", "add() takes typed numbers");
    let c = big_change(false);
    let (row, pos) = split(
        (
            RuleStatus::Kept,
            "part 1 of 3: src/add.rs:1 types both numbers",
        ),
        (RuleStatus::Broken, "src/add.rs:1 returns i32 for floats"),
    );
    let whole = Evidence::of_change(&row, &c, None);
    assert!(
        dispute_request("s", 2, &r, "", &whole, None, &pos).is_none(),
        "the whole change does not fit a ballot"
    );
    let (scoped, shown) = Evidence::of_named_files(&row, &c, None, &pos).expect("scoped");
    assert_eq!(shown, vec!["src/add.rs".to_string()]);
    let req = dispute_request("s", 2, &r, "", &scoped, None, &pos).expect("a ballot");
    assert!(req.evidence.contains(DIFF), "the named file's diff, whole");
    assert!(
        !req.evidence.contains("BIG-ROW"),
        "the other file is not shown"
    );
    assert!(req.evidence.contains("THE CHANGE, IN PART"));
    let listed = req
        .evidence
        .split("NOTSHOWN-")
        .nth(1)
        .expect("the not-shown list");
    assert!(listed.contains("gen/big.json"), "{listed}");
    // A KEPT vote counts only on a file shown: one naming only the file not shown is not counted.
    let paths: Vec<&str> = shown.iter().map(String::as_str).collect();
    let bars = VoteBars {
        paths: &paths,
        ..bars(false, None)
    };
    let v = verdict(&[
        ("agy", Some(1), "gen/big.json is generated, nothing to type"),
        ("opencode", Some(1), "src/add.rs:1 types both numbers"),
        ("copilot", Some(1), "src/add.rs:1 is typed"),
    ]);
    let settled = settle(&row, pos.clone(), &v, &bars);
    let c0 = settled.council.as_ref().unwrap();
    assert!(!c0.votes[0].counted, "{c0:?}");
    assert!(c0.votes[1].counted && c0.votes[2].counted, "{c0:?}");
}

/// (core#1789, codex r1 and r2) No scope, so the dispute holds as before: when the JUDGE claims
/// the break with no claims the engine checked as it ruled (a row read back from the store; the
/// cite alone, even with its `part N of M` label and a changed file named `part`, is never read
/// for them), when its checked claims name a file git does not list, and when any BROKEN line of
/// the reviewer's names no changed file. Checked claims, and every reviewer line that names a
/// file, are shown.
#[test]
fn core_1789_a_scope_is_drawn_only_on_checked_claims_each_naming_a_file() {
    let c = big_change(true);
    let (mut row, pos) = split(
        (RuleStatus::Broken, "part 2 of 3: src/add.rs:1 untyped"),
        (RuleStatus::Kept, "src/add.rs:1 typed"),
    );
    assert!(Evidence::of_named_files(&row, &c, None, &pos).is_none());
    row.claim_files = Some(vec!["src/add.rs".into()]);
    let (_, shown) = Evidence::of_named_files(&row, &c, None, &pos).expect("checked claims");
    assert_eq!(shown, vec!["src/add.rs".to_string()]);
    row.claim_files = Some(vec!["src/gone.rs".into()]);
    assert!(Evidence::of_named_files(&row, &c, None, &pos).is_none());
    let mut with_part = big_change(true);
    with_part.paths.push(("part".into(), true));
    let (row, pos) = split(
        (RuleStatus::Broken, "part 2 of 3: the rows lack validation"),
        (RuleStatus::Kept, "src/add.rs:1 typed"),
    );
    assert!(Evidence::of_named_files(&row, &with_part, None, &pos).is_none());
    let (row, pos) = split(
        (RuleStatus::NotApplicable, "part 1 of 3: no API here"),
        (
            RuleStatus::Broken,
            &format!("src/add.rs:1 untyped{CLAIM_SEP}the rows lack validation"),
        ),
    );
    assert!(
        Evidence::of_named_files(&row, &c, None, &pos).is_none(),
        "a claim naming no changed file gives no scope"
    );
    let (row, pos) = split(
        (RuleStatus::Kept, "part 1 of 3: src/add.rs:1 typed"),
        (
            RuleStatus::Broken,
            &format!("src/add.rs:1 untyped{CLAIM_SEP}src/b.rs:1 takes floats"),
        ),
    );
    let (_, shown) = Evidence::of_named_files(&row, &c, None, &pos).expect("scoped");
    assert_eq!(
        shown,
        vec!["src/add.rs".to_string(), "src/b.rs".to_string()]
    );
}

/// (core#1789, codex r1 and r2) A rename's section shows both its sides, so a vote naming the new
/// name counts and neither side is listed as not shown; a header that only ENDS with a changed
/// path (a new name holding ` b/`) never marks that other file shown.
#[test]
fn core_1789_a_scoped_rename_shows_both_sides_and_only_them() {
    let big = "+\"BIG-ROW\"\n".repeat(20 * 1024);
    let renamed = format!(
        "diff --git a/src/old.rs b/src/new.rs\nsimilarity index 90%\nrename from src/old.rs\n\
         rename to src/new.rs\n@@ -1 +1 @@\n-fn o() {{}}\n+fn o(x: i32) {{}}\n\
         diff --git a/gen/big.json b/gen/big.json\n--- a/gen/big.json\n+++ b/gen/big.json\n\
         @@ -0,0 +1 @@\n{big}"
    );
    let c = change_of(&renamed, &["src/old.rs", "src/new.rs", "gen/big.json"]);
    let (row, pos) = split(
        (RuleStatus::Kept, "part 1 of 2: src/new.rs:1 typed"),
        (RuleStatus::Broken, "src/old.rs:1 untyped"),
    );
    let (ev, shown) = Evidence::of_named_files(&row, &c, None, &pos).expect("scoped");
    assert_eq!(
        shown,
        vec!["src/new.rs".to_string(), "src/old.rs".to_string()]
    );
    assert_eq!(ev.not_shown, Some(vec!["gen/big.json".to_string()]));
    let decoy = format!(
        "diff --git a/src/old.rs b/decoy b/src/a.rs\nsimilarity index 90%\n\
         rename from src/old.rs\nrename to decoy b/src/a.rs\n@@ -1 +1 @@\n-fn o() {{}}\n\
         +fn o(x: i32) {{}}\ndiff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n\
         @@ -1 +1 @@\n-fn a() {{}}\n+fn a(x: i32) {{}}\n\
         diff --git a/gen/big.json b/gen/big.json\n--- a/gen/big.json\n+++ b/gen/big.json\n\
         @@ -0,0 +1 @@\n{big}"
    );
    let c = change_of(
        &decoy,
        &["src/old.rs", "decoy b/src/a.rs", "src/a.rs", "gen/big.json"],
    );
    let (ev, shown) = Evidence::of_named_files(&row, &c, None, &pos).expect("scoped");
    assert!(!shown.contains(&"src/a.rs".to_string()), "{shown:?}");
    assert!(
        ev.not_shown
            .as_ref()
            .unwrap()
            .contains(&"src/a.rs".to_string()),
        "{:?}",
        ev.not_shown
    );
    // (codex r3) A file whose own header git quoted (a rename to an unusual name) and a decoy
    // whose name ends in ` b/src/a.rs`: the decoy never stands in for it, so there is no scope.
    let quoted = format!(
        "diff --git a/src/a.rs \"b/src/a\\\"new.rs\"\nsimilarity index 90%\n@@ -1 +1 @@\n\
         -fn a() {{}}\n+fn a(x: i32) {{}}\n\
         diff --git a/decoy b/src/a.rs b/decoy b/src/a.rs\n--- a/decoy b/src/a.rs\n\
         +++ b/decoy b/src/a.rs\n@@ -1 +1 @@\n-x\n+y\n\
         diff --git a/gen/big.json b/gen/big.json\n--- a/gen/big.json\n+++ b/gen/big.json\n\
         @@ -0,0 +1 @@\n{big}"
    );
    let c = change_of(
        &quoted,
        &[
            "src/a.rs",
            "src/a\"new.rs",
            "decoy b/src/a.rs",
            "gen/big.json",
        ],
    );
    let (row, pos) = split(
        (RuleStatus::Kept, "part 1 of 2: decoy b/src/a.rs:1 fine"),
        (RuleStatus::Broken, "src/a.rs:1 untyped"),
    );
    assert!(Evidence::of_named_files(&row, &c, None, &pos).is_none());
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
                seat_attempt: None,
                rulings: &[],
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
                carried: &[],
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
    // A diff larger than MAX_PARTS calls can show (core#1474): a BROKEN still counts, but it is
    // not whole.
    let big = change_of(
        &format!("{DIFF}{}", "+// pad\n".repeat(80_000)),
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
