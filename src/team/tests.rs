//! Pure-logic tests of the team's grammars and ids: `ADVICE` / `HELP:` / `STEP` / `HOLD` parsing,
//! the advice block, the steer answer classification, confirmation, finding ids, line keys and
//! anchors (DES-001 §4.6, DES-002 §8.8). The carrier is tested against a mock bridge in
//! `acp_runner::tests::steer`, the supervisor over a real bus in `team::supervisor::tests`.

use super::*;

const A: &str = "f-3fa9c2e1d0b4a7e6";
const B: &str = "f-0123456789abcdef";

fn f(id: &str, severity: Severity) -> Finding {
    Finding {
        finding_id: id.to_string(),
        monitor_id: "m1".to_string(),
        seat: "claude#2".to_string(),
        severity,
        path: "src/retire.ts".to_string(),
        line: 41,
        evidence: "fetchCoverage(scope).then(setCount)".to_string(),
        claim: "no cancellation".to_string(),
        suggestion: None,
        tree: "t".to_string(),
        in_diff: true,
        checkpoint_seq: 1,
        anchor: String::new(),
        carried_from_attempt: None,
    }
}

/// #602 acceptance 5 (parsing half): DECLINE and ACCEPT lines with their reasons, every separator
/// the DES grammar allows, the last line per id wins, and non-matching lines are ignored.
#[test]
fn advice_lines_parse_to_the_disposition_and_reason() {
    let out = format!(
        "narration\n\
         ADVICE {A}: ACCEPT — added AbortController\n\
         \t ADVICE {B}: DECLINE — campaign.rs:325 documents the exclusion\n\
         ADVICE {A}: DECLINE: the spec says so\n\
         ADVICE f-XYZ: DECLINE — not an id\n\
         ADVICE {B}: DECLINED — a word, not the keyword\n\
         - ADVICE {B}: ACCEPT — not at line start\n"
    );
    let p = parse_advice_lines(&out);
    assert_eq!(p.len(), 2, "{p:?}");
    assert_eq!(
        p[A].disposition,
        Disposition::Declined,
        "last line per id wins"
    );
    assert_eq!(p[A].reason, "the spec says so");
    assert_eq!(p[B].disposition, Disposition::Declined);
    assert_eq!(p[B].reason, "campaign.rs:325 documents the exclusion");

    let p = parse_advice_lines(&format!(
        "ADVICE {A}: DECLINE\nADVICE {B}: ACCEPT - fixed it"
    ));
    assert_eq!(
        p[A].reason, "",
        "a DECLINE with no reason is recorded with reason \"\""
    );
    assert_eq!(p[B].reason, "fixed it");
    assert_eq!(p[B].disposition, Disposition::Accepted);
}

#[test]
fn a_reason_is_capped_at_2kb_on_a_char_boundary() {
    let long = "é".repeat(3000);
    let p = parse_advice_lines(&format!("ADVICE {A}: DECLINE — {long}"));
    assert!(p[A].reason.len() <= REASON_CAP);
    assert!(p[A].reason.chars().all(|c| c == 'é'));
}

#[test]
fn the_steer_answer_is_classified_as_the_des_says() {
    use serde_json::json;
    assert_eq!(
        classify_steer_answer(&json!({"id":5,"result":{"outcome":"injected"}})),
        (events::DeliveryOutcome::Injected, None)
    );
    assert_eq!(
        classify_steer_answer(
            &json!({"id":5,"result":{"outcome":"promptRequired","reason":"noRunningTurn"}})
        ),
        (
            events::DeliveryOutcome::TurnEnded,
            Some("noRunningTurn".to_string())
        )
    );
    let (o, d) = classify_steer_answer(
        &json!({"id":5,"error":{"code":-32602,"message":"Invalid params: unsupported steering idleBehavior"}}),
    );
    assert_eq!(o, events::DeliveryOutcome::Refused);
    assert_eq!(
        d.as_deref(),
        Some("-32602: Invalid params: unsupported steering idleBehavior")
    );
    // A detached turn is exactly what promptRequired prevents: never read as delivered.
    let (o, d) = classify_steer_answer(&json!({"id":5,"result":{"outcome":"startedNewTurn"}}));
    assert_eq!(o, events::DeliveryOutcome::Refused);
    assert!(d.unwrap().contains("startedNewTurn"));
}

#[test]
fn the_steer_params_always_carry_prompt_required() {
    let p = steer_params("sess", "advice");
    assert_eq!(
        p,
        serde_json::json!({"sessionId":"sess","prompt":[{"type":"text","text":"advice"}],
            "_meta":{"steering":{"idleBehavior":"promptRequired"}}})
    );
}

/// One steer stays within 8 KB; what does not fit waits for the next boundary, and a single
/// oversized finding is truncated rather than wedging the queue.
#[test]
fn the_advice_block_is_capped_and_overflow_is_kept() {
    let mut big = f(A, Severity::High);
    big.claim = "x".repeat(5000);
    let mut big2 = f(B, Severity::High);
    big2.claim = "y".repeat(5000);
    let (text, sent, rest) = advice_block(vec![Advice { finding: big }, Advice { finding: big2 }]);
    assert!(text.len() <= ADVICE_TEXT_CAP, "{}", text.len());
    assert_eq!(sent.len(), 1);
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].finding.finding_id, B);

    let mut huge = f(A, Severity::High);
    huge.claim = "z".repeat(20_000);
    let (text, sent, rest) = advice_block(vec![Advice { finding: huge }]);
    assert!(text.len() <= ADVICE_TEXT_CAP);
    assert_eq!(sent.len(), 1);
    assert!(rest.is_empty());
    assert!(text.ends_with("The gate reviews each finding together with your answer.\n"));
}

#[test]
fn confirm_is_exact_line_text_and_normalization_serves_only_the_finding_id() {
    let file = Some("fn a() {}\nfn b() {\n    let x = 2;\n}\n");
    assert!(
        confirm(file, 3, "    let x = 2;"),
        "the exact line confirms"
    );
    assert!(
        !confirm(file, 3, "let x = 2;"),
        "dropped indentation does not"
    );
    assert!(
        !confirm(file, 3, "let   x = 2;"),
        "changed spacing does not"
    );
    assert!(
        !confirm(file, 3, "    let x = 2; "),
        "a trailing space does not"
    );
    assert!(
        !confirm(file, 2, "    let x = 2;"),
        "the wrong line does not"
    );
    assert!(!confirm(file, 3, ""), "empty evidence confirms nothing");
    assert!(!confirm(file, 0, "fn a() {}"), "line 0 confirms nothing");
    assert!(
        !confirm(None, 3, "    let x = 2;"),
        "a missing file confirms nothing"
    );
    assert_eq!(
        finding_id_anchored("src/lib.rs", "", "    let x = 2;"),
        finding_id_anchored("src/lib.rs", "", "let   x = 2;"),
        "the id is keyed on the normalized text"
    );
}

/// DES-001 §4.6 step 4: the id is `path ‖ anchor ‖ normalized text`. The same hazardous line in two
/// functions of one file is two findings with one line key; a file-level finding (no anchor) keeps
/// the pre-anchor spelling, so ids minted before T6 are unchanged.
#[test]
fn the_finding_id_is_path_anchor_and_text_and_the_line_key_is_path_and_text() {
    assert_eq!(
        finding_id_anchored("src/lib.rs", "", "fn b() {"),
        "f-6a4ac002d79dfc6b",
        "the file-level spelling is unchanged"
    );
    assert_eq!(
        finding_id_anchored("src/lib.rs", "", "  fn   b() {  "),
        "f-6a4ac002d79dfc6b"
    );
    assert_ne!(
        finding_id_anchored("src/other.rs", "", "fn b() {"),
        "f-6a4ac002d79dfc6b"
    );
    let line = "    store.erase_scope(scope)?;";
    let retire = finding_id_anchored("src/x.rs", "fn retire() {", line);
    let purge = finding_id_anchored("src/x.rs", "fn purge() {", line);
    assert_ne!(retire, purge);
    assert_ne!(retire, finding_id_anchored("src/x.rs", "", line));
    assert_eq!(
        line_key("src/x.rs", line),
        line_key("src/x.rs", "store.erase_scope(scope)?;")
    );
    assert!(line_key("src/x.rs", line).starts_with("l-"));
    assert_eq!(
        line_key("src/lib.rs", "fn b() {").trim_start_matches("l-"),
        "6a4ac002d79dfc6b",
        "the line key is the file-level id's hash"
    );
}

/// The anchor is git's funcname heuristic: the nearest line ABOVE the finding's that starts with a
/// letter, `_` or `$`; none (a new file of indented statements) is file-level.
#[test]
fn the_anchor_is_the_nearest_funcname_line_above() {
    let file = "use x;\n\nfn retire() {\n    a();\n    b();\n}\n\nimpl S {\n    fn m(&self) {\n        c();\n";
    assert_eq!(anchor_of(Some(file), 5), "fn retire() {");
    assert_eq!(anchor_of(Some(file), 10), "impl S {");
    assert_eq!(anchor_of(Some(file), 1), "", "nothing above line 1");
    assert_eq!(anchor_of(Some("    a();\n    b();\n"), 2), "");
    assert_eq!(anchor_of(None, 3), "");
    let long = format!("fn {}() {{\n    x();\n", "a".repeat(400));
    assert!(anchor_of(Some(&long), 2).len() <= ANCHOR_CAP);
}

/// DES-002 §8.8: `HELP:` lines, in order, capped, empty ones dropped.
#[test]
fn help_lines_parse_in_order_and_empty_ones_are_not_questions() {
    let out = "work\nHELP: which helper retries?\n  HELP:   \nHELP: second one\nnot HELP: this\n";
    assert_eq!(
        parse_help_lines(out),
        vec![
            "which helper retries?".to_string(),
            "second one".to_string()
        ]
    );
    let many: String = (0..20).map(|i| format!("HELP: q{i}\n")).collect();
    assert_eq!(parse_help_lines(&many).len(), HELP_MAX);
}

/// DES-002 §8.8: `STEP <id>: ACCEPT|REJECT to:member|to:pa — <reason>`; the first line per step
/// wins; a REJECT without a target goes back to the member; a malformed line is not a review.
#[test]
fn step_lines_parse_the_verdict_the_target_and_the_reason() {
    let out = "STEP build: REJECT to:pa — the migration is wrong\n\
               STEP build: ACCEPT — second line loses\n\
               STEP test-plan: ACCEPT — covers it\n\
               STEP review: REJECT — no target\n\
               STEP bogus: MAYBE — no\n\
               STEP spaced id: ACCEPT — no\n";
    let l = parse_step_lines(out);
    assert_eq!(l.len(), 3, "{l:?}");
    assert_eq!(l[0].step_id, "build");
    assert_eq!(l[0].verdict, events::StepVerdict::Rejected);
    assert_eq!(l[0].to, Some(events::ReworkBy::Pa));
    assert_eq!(l[0].reason, "the migration is wrong");
    assert_eq!(l[1].verdict, events::StepVerdict::Accepted);
    assert_eq!(l[1].to, None);
    assert_eq!(
        l[2].to,
        Some(events::ReworkBy::Member),
        "no target = the member"
    );
}

/// DES-001 §4.5: `HOLD` / `WITHDRAW <findingId> — <reason>`, last line per id wins; anything else
/// is not an answer (the caller counts silence as HOLD).
#[test]
fn hold_lines_parse_and_the_last_per_id_wins() {
    let r = format!(
        "HOLD {A} — still a bug\nWITHDRAW {B}: the worker is right\nHOLD {B} — changed my mind\n\
         WITHDRAW f-short — no\nDONE"
    );
    let p = parse_hold_lines(&r);
    assert_eq!(p.len(), 2);
    assert_eq!(p[A].kind, ReplyKind::Hold);
    assert_eq!(p[A].reason, "still a bug");
    assert_eq!(p[B].kind, ReplyKind::Hold, "last line wins");
}

/// DES-002 §8.8: a member's answer to a rejected step: `HOLD <step> — why` holds, `ACCEPT <step>`
/// takes the rejection; neither (or another step's line) is no answer.
#[test]
fn a_member_answers_a_rejection_with_hold_or_accept() {
    assert_eq!(
        parse_member_step_answer("HOLD build — the PA misread it\nDONE", "build"),
        Some((true, "the PA misread it".to_string()))
    );
    assert_eq!(
        parse_member_step_answer("ACCEPT build\nDONE", "build"),
        Some((false, String::new()))
    );
    assert_eq!(
        parse_member_step_answer("HOLD build-2 — other", "build"),
        None
    );
    assert_eq!(
        parse_member_step_answer("I think it is fine", "build"),
        None
    );
}

/// A member's help answer and its citations; `CHANGE` lines with steps.
#[test]
fn help_answers_and_change_requests_parse() {
    let (a, e) =
        parse_help_answer("ANSWER: use retry()\nit is in lib\nEVIDENCE: src/lib.rs:2\nDONE");
    assert_eq!(a, "use retry()\nit is in lib");
    assert_eq!(e, vec!["src/lib.rs:2".to_string()]);
    let c = parse_change_lines(
        "CHANGE {\"steps\":[{\"catalog\":\"test_plan\",\"id\":\"tp-2\"}],\"reason\":\"no tests\"}\n\
         CHANGE {\"steps\":[]}\nCHANGE not json\n",
    );
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].0[0].catalog, "test_plan");
    assert_eq!(c[0].1, "no tests");
}

/// A carried finding says so in the advice block (T6 (k)).
#[test]
fn a_carried_finding_is_labelled_in_the_advice_block() {
    let mut c = f(A, Severity::High);
    c.carried_from_attempt = Some(1);
    let (text, sent, _) = advice_block(vec![Advice { finding: c }]);
    assert_eq!(sent.len(), 1);
    assert!(text.contains("carried_from_attempt:1"), "{text}");
}
