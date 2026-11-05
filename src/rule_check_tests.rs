//! DES-RULE-CHECK-001 S2: the rule check's own tests (pure: a fake judge, no seats, no store).

use super::*;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use wicked_governance::{ConfSeverity, ConformanceRule};

fn rule(id: &str, statement: &str) -> ConformanceRule {
    ConformanceRule {
        id: id.to_string(),
        statement: statement.to_string(),
        severity: ConfSeverity::Warn,
        ..Default::default()
    }
}

const DIFF: &str = "diff --git a/src/add.rs b/src/add.rs\n--- a/src/add.rs\n+++ b/src/add.rs\n@@ -1 +1 @@\n-fn add() {}\n+fn add(a: i32, b: i32) -> i32 { a + b }\n";

/// A judge that answers each asked rule from `verdicts` (id → line tail), after `latency`, and
/// records every prompt it saw.
struct Fake {
    verdicts: BTreeMap<String, String>,
    latency: Duration,
    calls: AtomicU32,
    prompts: Mutex<Vec<String>>,
    /// Seats that fail to run.
    dead: Vec<String>,
    /// When set, a batch of more than one rule omits this id (a single ask answers it).
    omit_in_batch: Option<String>,
}

impl Fake {
    fn new(verdicts: &[(&str, &str)]) -> Self {
        Fake {
            verdicts: verdicts
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            latency: Duration::ZERO,
            calls: AtomicU32::new(0),
            prompts: Mutex::new(Vec::new()),
            dead: Vec::new(),
            omit_in_batch: None,
        }
    }
}

impl RuleJudge for Fake {
    fn ask(&self, seat: &str, prompt: &str) -> JudgeAnswer {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.prompts.lock().unwrap().push(prompt.to_string());
        if self.dead.iter().any(|d| d == seat) {
            return JudgeAnswer::SeatFailed("not signed in".into());
        }
        std::thread::sleep(self.latency);
        let asked: Vec<&str> = prompt
            .lines()
            .skip_while(|l| *l != "RULES:")
            .skip(1)
            .take_while(|l| l.starts_with("RULE "))
            .filter_map(|l| l[5..].split(" (").next())
            .collect();
        let mut out = String::from("Here is my review.\n");
        for id in &asked {
            if asked.len() > 1 && self.omit_in_batch.as_deref() == Some(*id) {
                continue;
            }
            if let Some(v) = self.verdicts.get(*id) {
                out.push_str(&format!("RULE {id}: {v}\n"));
            }
        }
        JudgeAnswer::Answered(out)
    }
}

fn facts() -> JobFacts {
    job_facts(&[], None, DIFF)
}

fn run(
    rules: &[ConformanceRule],
    judge: &Fake,
    prior: Option<&RuleCheckReport>,
) -> RuleCheckReport {
    let f = facts();
    let seats = vec!["codex".to_string(), "agy".to_string(), "pi".to_string()];
    check(
        &CheckInput {
            rules,
            facts: &f,
            diff: DIFF,
            floor: Some("cargo test passed"),
            prior,
            skips: &[],
            seats: &seats,
            no_seat: None,
        },
        judge,
    )
}

#[test]
fn the_plan_keeps_8_per_call_and_grows_only_to_stay_within_two_rounds() {
    assert_eq!(plan(0), (0, 0));
    assert_eq!(plan(5), (8, 1));
    assert_eq!(plan(40), (8, 3), "5 batches of 8, 3 at once: 2 rounds");
    assert_eq!(plan(48), (8, 3));
    assert_eq!(
        plan(100),
        (17, 3),
        "6 batches of 17: 2 rounds, never 13 batches in 5"
    );
    for n in 1..=300 {
        let (size, par) = plan(n);
        assert!(n.div_ceil(size).div_ceil(par) <= MAX_WAVES, "n={n}");
    }
}

#[test]
fn rule_lines_parse_whole_ids_and_void_a_contradiction_or_a_missing_cite() {
    let ids = ["DEV-1", "proposal:a", "proposal:ab", "R2", "R3", "R4", "R5"];
    let text = "RULE DEV-1: KEPT — src/add.rs:1 adds the typed signature\n\
                - **RULE proposal:ab**: BROKEN - no test in tests/\n\
                RULE proposal:a: N/A — no UI in the diff\n\
                RULE R2: KEPT — a\nRULE R2: BROKEN — b\n\
                RULE R3: KEPT\n\
                RULE R4: UNKNOWN — the diff is cut\n\
                RULE R5: KEPTISH — x\n\
                RULE OTHER: KEPT — not asked\n";
    let p = parse_rule_lines(text, &ids);
    assert_eq!(p["DEV-1"].status, RuleStatus::Kept);
    assert_eq!(p["DEV-1"].cite, "src/add.rs:1 adds the typed signature");
    assert_eq!(
        p["proposal:ab"].status,
        RuleStatus::Broken,
        "longest id wins"
    );
    assert_eq!(p["proposal:a"].status, RuleStatus::NotApplicable);
    for gone in ["R2", "R3", "R4", "R5", "OTHER"] {
        assert!(!p.contains_key(gone), "{gone} must count as not answered");
    }
}

/// The 100-rule fixture: every rule comes back with exactly one row, in two rounds, and the timing
/// is measured (fake judge at a fixed latency per call).
#[test]
fn a_hundred_rules_all_get_a_row_in_two_rounds_and_the_time_is_measured() {
    let rules: Vec<ConformanceRule> = (0..100)
        .map(|i| rule(&format!("R{i:03}"), &format!("statement {i} ").repeat(20)))
        .collect();
    let verdicts: Vec<(String, String)> = (0..100)
        .map(|i| (format!("R{i:03}"), "KEPT — src/add.rs:1".to_string()))
        .collect();
    let refs: Vec<(&str, &str)> = verdicts
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let mut judge = Fake::new(&refs);
    let latency = Duration::from_millis(400);
    judge.latency = latency;
    let t = Instant::now();
    let report = run(&rules, &judge, None);
    let wall = t.elapsed();
    assert_eq!(report.rows.len(), 100, "nothing dropped");
    let ids: BTreeSet<&str> = report.rows.iter().map(|r| r.rule_id.as_str()).collect();
    assert_eq!(ids.len(), 100, "each rule once");
    assert!(report.rows.iter().all(|r| r.status == RuleStatus::Kept));
    assert_eq!(
        (
            report.batch_size,
            report.parallel,
            report.waves,
            report.calls
        ),
        (17, 3, 2, 6)
    );
    // Two rounds of one call each, plus overhead; never five rounds.
    assert!(
        wall < latency * 3,
        "wall {wall:?} for 2 rounds of {latency:?}"
    );
    let per_call_s = 45.0;
    eprintln!(
        "RULE-CHECK TIMING: 100 rules, {} calls of up to {} rules, {} at once, {} rounds; wall {:?} at \
         {:?}/call; at {per_call_s} s/call that is {:.0} s (< 120 s)",
        report.calls,
        report.batch_size,
        report.parallel,
        report.waves,
        wall,
        latency,
        report.waves as f64 * per_call_s
    );
}

#[test]
fn a_broken_rule_denies_and_names_the_rule_with_its_cite() {
    let rules = [
        rule("DEV-TINYMATH-2", "add() takes typed numbers"),
        rule("K", "keep"),
    ];
    let judge = Fake::new(&[
        (
            "DEV-TINYMATH-2",
            "BROKEN — src/add.rs:1 takes i32 but the test passes floats",
        ),
        ("K", "KEPT — src/add.rs:1"),
    ]);
    let r = run(&rules, &judge, None);
    let d = r.denial().expect("a broken rule denies");
    assert_eq!(d.source, DENIAL_SOURCE_BROKEN);
    assert_eq!(d.rule_ids, ["DEV-TINYMATH-2"]);
    assert!(d.reason.contains("src/add.rs:1 takes i32"), "{}", d.reason);
    assert!(r.hold().is_none());
    assert_eq!(
        r.summary(),
        "2 rules checked: 1 kept, 1 broken, 0 not applicable"
    );
}

#[test]
fn a_rule_a_batch_skips_is_asked_alone_and_then_answered() {
    let rules = [rule("A", "a"), rule("B", "b")];
    let mut judge = Fake::new(&[("A", "KEPT — x"), ("B", "KEPT — y")]);
    judge.omit_in_batch = Some("B".into());
    let r = run(&rules, &judge, None);
    assert!(r.rows.iter().all(|x| x.status == RuleStatus::Kept));
    assert_eq!(r.calls, 2, "the batch, then B alone");
    let prompts = judge.prompts.lock().unwrap();
    assert!(prompts[1].contains("RULE B (") && !prompts[1].contains("RULE A ("));
}

#[test]
fn a_rule_nobody_answers_is_unchecked_and_holds() {
    let rules = [rule("A", "a"), rule("SILENT", "s")];
    let judge = Fake::new(&[("A", "KEPT — x")]);
    let r = run(&rules, &judge, None);
    let silent = r.rows.iter().find(|x| x.rule_id == "SILENT").unwrap();
    assert_eq!(silent.status, RuleStatus::Unchecked);
    let hold = r.hold().expect("unchecked holds");
    assert_eq!(hold.source, DENIAL_SOURCE_UNCHECKED);
    assert_eq!(hold.rule_ids, ["SILENT"]);
    assert!(r.denial().is_none(), "a hold, not a deny");
}

#[test]
fn every_seat_failing_leaves_the_batch_unchecked_and_says_why() {
    let rules = [rule("A", "a")];
    let mut judge = Fake::new(&[("A", "KEPT — x")]);
    judge.dead = vec!["codex".into(), "agy".into(), "pi".into()];
    let r = run(&rules, &judge, None);
    assert_eq!(r.rows[0].status, RuleStatus::Unchecked);
    assert!(
        r.rows[0].reason.contains("not signed in"),
        "{}",
        r.rows[0].reason
    );
}

#[test]
fn no_distinct_seat_means_every_rule_to_check_is_unchecked_with_no_call() {
    let rules = [rule("A", "a")];
    let judge = Fake::new(&[("A", "KEPT — x")]);
    let f = facts();
    let r = check(
        &CheckInput {
            rules: &rules,
            facts: &f,
            diff: DIFF,
            floor: None,
            prior: None,
            skips: &[],
            seats: &[],
            no_seat: Some("only the creator's seat `claude` is signed in"),
        },
        &judge,
    );
    assert_eq!(judge.calls.load(Ordering::SeqCst), 0);
    assert_eq!(r.rows[0].status, RuleStatus::Unchecked);
    assert!(r.rows[0].reason.contains("creator's seat"));
}

#[test]
fn a_skip_with_its_reason_is_a_labelled_row_and_no_hold() {
    let rules = [rule("SILENT", "s")];
    let judge = Fake::new(&[]);
    let first = run(&rules, &judge, None);
    let ek = first.rows[0].evidence_key.clone();
    let skips = [RuleSkip {
        rule_id: "SILENT".into(),
        reason: "the judge cannot read the binary fixture".into(),
        by: "operator".into(),
        at: 1,
        evidence_key: ek,
    }];
    let f = facts();
    let seats = vec!["codex".to_string()];
    let r = check(
        &CheckInput {
            rules: &rules,
            facts: &f,
            diff: DIFF,
            floor: Some("cargo test passed"),
            prior: Some(&first),
            skips: &skips,
            seats: &seats,
            no_seat: None,
        },
        &judge,
    );
    assert_eq!(r.rows[0].status, RuleStatus::Skipped);
    assert_eq!(r.rows[0].how, RuleHow::Operator);
    assert!(r.rows[0].reason.contains("binary fixture"));
    assert!(r.hold().is_none() && r.denial().is_none());
    // The skip is tied to the evidence it answered: a changed diff checks the rule again.
    let other = format!("{DIFF}+// more\n");
    let f2 = job_facts(&[], None, &other);
    let again = check(
        &CheckInput {
            rules: &rules,
            facts: &f2,
            diff: &other,
            floor: Some("cargo test passed"),
            prior: Some(&r),
            skips: &skips,
            seats: &seats,
            no_seat: None,
        },
        &judge,
    );
    assert_eq!(again.rows[0].status, RuleStatus::Unchecked);
}

#[test]
fn a_re_review_with_the_same_evidence_reuses_every_verdict_and_calls_no_judge() {
    let rules = [rule("A", "a"), rule("B", "b")];
    let judge = Fake::new(&[("A", "KEPT — x"), ("B", "BROKEN — y")]);
    let first = run(&rules, &judge, None);
    let calls = judge.calls.load(Ordering::SeqCst);
    let second = run(&rules, &judge, Some(&first));
    assert_eq!(
        judge.calls.load(Ordering::SeqCst),
        calls,
        "a cache hit asks nobody"
    );
    assert!(second.rows.iter().all(|r| r.how == RuleHow::Cache));
    assert_eq!(
        second.rows[1].status,
        RuleStatus::Broken,
        "a cached broken still denies"
    );
    // An edited rule is checked again; the unchanged one stays cached.
    let edited = [rule("A", "a, edited"), rule("B", "b")];
    let third = run(&edited, &judge, Some(&second));
    assert_eq!(third.rows[0].how, RuleHow::Judge);
    assert_eq!(third.rows[1].how, RuleHow::Cache);
}

#[test]
fn a_rule_out_of_the_runs_steps_is_not_applicable_with_its_fact_and_no_call() {
    let mut mcp = rule("MCPS-1001", "mcp servers ship typed tools");
    mcp.applies_to = vec!["install".into()];
    let steps = vec![JobStep {
        tokens: vec!["unit-0".into(), "fix".into()],
        kind: Some("build".into()),
    }];
    let f = job_facts(&steps, None, DIFF);
    let judge = Fake::new(&[]);
    let seats = vec!["codex".to_string()];
    let r = check(
        &CheckInput {
            rules: &[mcp],
            facts: &f,
            diff: DIFF,
            floor: None,
            prior: None,
            skips: &[],
            seats: &seats,
            no_seat: None,
        },
        &judge,
    );
    assert_eq!(r.rows[0].status, RuleStatus::NotApplicable);
    assert_eq!(r.rows[0].how, RuleHow::Fact);
    assert!(!r.rows[0].reason.is_empty());
    assert_eq!(judge.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn the_prompt_carries_every_statement_whole_and_fences_the_diff() {
    let long = "x".repeat(5000);
    let rules = [rule("LONG", &long)];
    let p = prompt(&rules.iter().collect::<Vec<_>>(), DIFF, Some("ok"));
    assert!(p.contains(&long), "no 600-character cut (core#1273 item 2)");
    assert!(p.contains("fn add(a: i32"));
    let big = format!("diff --git a/a.rs b/a.rs\n{}", "+x\n".repeat(EVIDENCE_CAP));
    let p = prompt(&rules.iter().collect::<Vec<_>>(), &big, None);
    assert!(p.len() < 100 * 1024, "one argv argument");
    assert!(p.contains("answer UNKNOWN"));
}

#[test]
fn the_delivery_keeps_the_worst_row_per_rule() {
    let row = |id: &str, s: RuleStatus| RuleVerdict {
        rule_id: id.into(),
        status: s,
        how: RuleHow::Judge,
        reason: String::new(),
        cite: None,
        judge: None,
        rule_key: String::new(),
        evidence_key: None,
    };
    let a = [row("A", RuleStatus::Kept), row("B", RuleStatus::Skipped)];
    let b = [
        row("A", RuleStatus::Broken),
        row("C", RuleStatus::NotApplicable),
    ];
    let out = aggregate([&a[..], &b[..]]);
    let st: Vec<(&str, RuleStatus)> = out.iter().map(|r| (r.rule_id.as_str(), r.status)).collect();
    assert_eq!(
        st,
        [
            ("A", RuleStatus::Broken),
            ("B", RuleStatus::Skipped),
            ("C", RuleStatus::NotApplicable)
        ]
    );
}

#[test]
fn languages_come_from_the_changed_files() {
    let f = facts();
    assert_eq!(f.languages, Some(vec!["rust".to_string()]));
    assert_eq!(diff_files(DIFF)[0].0, "src/add.rs");
}
