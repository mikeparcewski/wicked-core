//! S3 (#602) pure-logic tests: `ADVICE` parsing, the advice block, the steer answer
//! classification and the end-of-attempt sweep. The carrier itself is tested against a mock
//! bridge in `acp_runner::tests::steer`.

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
    }
}

fn key() -> AdviceKey {
    ("run-1".to_string(), 3, 1)
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

/// #602 acceptance 5 (recording half) + the authority model: per DELIVERED id one
/// `workerAdviceResponse` with the disposition and reason; a delivered id with no line is
/// `unanswered`; an id this attempt never delivered emits nothing.
#[test]
fn finish_attempt_records_each_answer_and_every_unanswered_id() {
    let m = SteerMailbox::default();
    m.record(&key(), A, Delivery::Injected);
    m.record(&key(), B, Delivery::Injected);
    m.record(
        &key(),
        "f-2222222222222222",
        Delivery::NotDelivered { detail: "x".into() },
    );
    let out = format!(
        "ADVICE {A}: DECLINE — campaign.rs:325 documents the exclusion\n\
         ADVICE f-2222222222222222: ACCEPT — never saw it\n"
    );
    let evs = finish_attempt(&m, &key(), Some(&out));
    let j: Vec<serde_json::Value> = evs.iter().map(CoreEvent::to_json).collect();
    assert_eq!(
        j,
        vec![
            serde_json::json!({"type":"workerAdviceResponse","session":"run-1","ord":3,
            "attempt":1,"findingId":A,"disposition":"declined",
            "reason":"campaign.rs:325 documents the exclusion"})
        ]
    );
    let r = m.take_record(&key()).unwrap();
    assert_eq!(r.unanswered, vec![B.to_string()]);
    assert_eq!(r.responses[A].disposition, Disposition::Declined);
}

/// Nothing queued is ever dropped silently: what is left at the end of an attempt is one
/// `adviceDelivered{not_delivered}` — `carrier: "none"` when no steering-capable turn ran,
/// `"acp_steering"` when one did but the turn ended first. A failed turn is not parsed.
#[test]
fn leftover_advice_is_disclosed_not_delivered_with_the_reason() {
    let m = SteerMailbox::default();
    assert!(m.queue(key(), f(A, Severity::High)));
    let evs = finish_attempt(&m, &key(), None);
    assert_eq!(evs.len(), 1);
    let j = evs[0].to_json();
    assert_eq!(j["carrier"], "none");
    assert_eq!(j["outcome"], "not_delivered");
    assert_eq!(j["findingIds"], serde_json::json!([A]));
    assert!(m.take_queued(&key()).is_empty());

    let k2: AdviceKey = ("run-1".to_string(), 3, 2);
    m.mark_steering_channel(&k2);
    assert!(m.queue(k2.clone(), f(B, Severity::High)));
    let j = finish_attempt(&m, &k2, Some("ok"))[0].to_json();
    assert_eq!(j["carrier"], "acp_steering");
    assert!(j["detail"]
        .as_str()
        .unwrap()
        .contains("before another tool-call boundary"));
    assert!(finish_attempt(&m, &("run-1".to_string(), 9, 0), Some("x")).is_empty());
}

#[test]
fn the_mailbox_takes_only_high_and_only_once_per_attempt() {
    let m = SteerMailbox::default();
    assert!(!m.queue(key(), f(A, Severity::Medium)));
    assert!(m.queue(key(), f(A, Severity::High)));
    assert!(!m.queue(key(), f(A, Severity::High)), "queued twice");
    let taken = m.take_queued(&key());
    assert_eq!(taken.len(), 1);
    m.record(&key(), A, Delivery::Injected);
    assert!(
        !m.queue(key(), f(A, Severity::High)),
        "already answered/delivered"
    );
    m.prune_run("run-1");
    assert!(m.record_of(&key()).is_none());
}

#[test]
fn the_steer_answer_is_classified_as_the_des_says() {
    use serde_json::json;
    assert_eq!(
        classify_steer_answer(&json!({"id":5,"result":{"outcome":"injected"}})),
        (SteerOutcome::Injected, None)
    );
    assert_eq!(
        classify_steer_answer(
            &json!({"id":5,"result":{"outcome":"promptRequired","reason":"noRunningTurn"}})
        ),
        (SteerOutcome::TurnEnded, Some("noRunningTurn".to_string()))
    );
    let (o, d) = classify_steer_answer(
        &json!({"id":5,"error":{"code":-32602,"message":"Invalid params: unsupported steering idleBehavior"}}),
    );
    assert_eq!(o, SteerOutcome::Refused);
    assert_eq!(
        d.as_deref(),
        Some("-32602: Invalid params: unsupported steering idleBehavior")
    );
    // A detached turn is exactly what promptRequired prevents: never read as delivered.
    let (o, d) = classify_steer_answer(&json!({"id":5,"result":{"outcome":"startedNewTurn"}}));
    assert_eq!(o, SteerOutcome::Refused);
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
