//! (core#760) THE REVIEWER'S BRIEF — what an Evaluator unit is handed beside the work it reviews,
//! on every round: the intent's *done-when* as a numbered checklist, its own prior verdicts, the
//! operator's rulings on it and on the creator unit it reviews, and the latest floor record.
//!
//! Before this the evaluator's context was the creator's output and the team findings, the same
//! six labels on all thirteen rounds of run ad5a4ca7: no prior verdict, no operator ruling, no
//! `repoChecksEvaluated`. A reviewer with no history re-derives the bar from the intent each round,
//! so the bar moved: rulings were re-raised, items present since the first attempt surfaced ten
//! rounds late, and nine of thirteen verdicts said "floor results not available" over a floor that
//! had passed (program-2026-09 `dogfood/s15e-recon/DECISION.md` §1 P1, §3 row 1).
//!
//! The verdict contract this brief asks for — one `ITEM <n>: PASS | FAIL | RULED` line per
//! checklist item above the `VERDICT:` line — gives a later round (and the operator) stable item
//! numbers to hold a verdict to; [`parse_review_items`] reads them back when the rework cap
//! (core#761, [`MAX_REVIEW_SENDBACKS`]) opens its `review_adjudication` gate. The `VERDICT:` line
//! is still the only thing the gate fold parses ([`crate::validator::parse_evaluator_verdict`]).

use crate::domain::{IntentAmendment, WorkUnit};

/// The most of one prior verdict handed back (its TAIL — the contract puts the findings and the
/// item lines just above the verdict line, which is last).
const ROUND_FINDINGS_CAP: usize = 3000;

/// The `done when` items of an intent, in order: the list under the first heading (or label line)
/// that reads `Done when` / `Done-when` / `Acceptance criteria` / `Definition of done`, markers
/// stripped (`-`, `*`, `+`, `1.`, `1)`, `[ ]`, `[x]`). An indented line continues the item above
/// it; the list ends at the next heading or the next unindented prose line. A label line with
/// text after its colon (`Done when: the build passes`) is itself an item, and a section written
/// as one paragraph is one item. No such section → empty (the brief then asks the reviewer to
/// number the intent's own requirements).
pub(crate) fn done_when_items(intent: &str) -> Vec<String> {
    let mut lines = intent.lines();
    let mut items: Vec<String> = Vec::new();
    // Find the section head.
    loop {
        let Some(line) = lines.next() else {
            return items;
        };
        if let Some(rest) = done_when_head(line) {
            if !rest.is_empty() {
                items.push(rest);
            }
            break;
        }
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            break;
        }
        if let Some(item) = list_item(trimmed) {
            if !item.is_empty() {
                items.push(item.to_string());
            }
            continue;
        }
        let indented = line.starts_with([' ', '\t']);
        match items.last_mut() {
            Some(last) if indented => {
                last.push(' ');
                last.push_str(trimmed);
            }
            None => items.push(trimmed.to_string()),
            // An unindented prose line after the list: the next section began.
            Some(_) => break,
        }
    }
    items
}

/// `Some(rest-after-colon)` when `line` heads a done-when section.
fn done_when_head(line: &str) -> Option<String> {
    let bare = line
        .trim()
        .trim_start_matches(['#', '*', '_', ' ', '\t'])
        .trim_end();
    let (label, rest) = match bare.split_once(':') {
        Some((l, r)) => (l, r),
        None => (bare, ""),
    };
    let norm = label
        .trim_matches(['*', '_', ' ', '\t'])
        .to_lowercase()
        .replace(['-', '_'], " ");
    let heads = ["done when", "acceptance criteria", "definition of done"];
    if !heads.iter().any(|h| norm == *h) {
        return None;
    }
    Some(rest.trim().trim_matches(['*', '_']).trim().to_string())
}

/// The text of a markdown list item (`- x`, `* x`, `+ x`, `1. x`, `1) x`, checkbox stripped).
fn list_item(trimmed: &str) -> Option<&str> {
    let rest = if let Some(r) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        r
    } else {
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let after = &trimmed[digits..];
        after
            .strip_prefix(". ")
            .or_else(|| after.strip_prefix(") "))?
    };
    let rest = rest.trim_start();
    let rest = ["[ ] ", "[x] ", "[X] "]
        .iter()
        .find_map(|b| rest.strip_prefix(b))
        .unwrap_or(rest);
    Some(rest.trim())
}

/// One `ITEM <n>: <STATUS> — …` line a verdict carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewItem {
    pub n: u32,
    /// `PASS` | `FAIL` | `RULED` (uppercased).
    pub status: String,
    /// The line as written (trimmed of decoration).
    pub line: String,
}

/// The `ITEM <n>: PASS | FAIL | RULED` lines of a verdict, in order; the LAST line for an item
/// number wins (a reviewer restating an item). Decoration (`-`, `*`, `#`, `>`, backticks) is
/// tolerated; a line whose status is none of the three is not an item line.
pub(crate) fn parse_review_items(text: &str) -> Vec<ReviewItem> {
    let mut out: Vec<ReviewItem> = Vec::new();
    for line in text.lines() {
        let bare = line
            .trim()
            .trim_start_matches(['#', '*', '-', '>', '`', ' ', '\t'])
            .trim_end();
        let Some(head) = bare.get(..4) else { continue };
        if !head.eq_ignore_ascii_case("item") {
            continue;
        }
        let rest = bare[4..].trim_start();
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        let Ok(n) = rest[..digits].parse::<u32>() else {
            continue;
        };
        let Some((_, after)) = rest[digits..].split_once(':') else {
            continue;
        };
        let status = after
            .split_whitespace()
            .next()
            .map(|w| {
                w.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_uppercase()
            })
            .unwrap_or_default();
        if !matches!(status.as_str(), "PASS" | "FAIL" | "RULED") {
            continue;
        }
        let line = bare.trim_matches(['*', '`']).trim().to_string();
        out.retain(|i| i.n != n);
        out.push(ReviewItem { n, status, line });
    }
    out.sort_by_key(|i| i.n);
    out
}

/// (core#761) THE REWORK CAP: after this many send-backs of one review, the next NOT-PASS verdict
/// opens [`REVIEW_ADJUDICATION_GATE`] instead of the plain escalation (retry / send back /
/// reject). Mirrors the team step's `MAX_STEP_REWORK` (DES-TEAMING-002 §8.8) and the program's
/// two-round cap on PR adjudication. Before it nothing bounded the evaluator↔creator loop: run
/// ad5a4ca7 ran 13 send-backs on one unit, ~10 h of creator time, before a human stopped it.
pub(crate) const MAX_REVIEW_SENDBACKS: usize = 2;

/// The gate kind ([`crate::event::CoreEvent::AwaitingHuman`]`.gate_kind`) of the capped review:
/// approve = LAND WITH CARRIED ITEMS, request changes = ONE MORE ROUND, reject = STOP.
pub(crate) const REVIEW_ADJUDICATION_GATE: &str = "review_adjudication";

/// [`crate::domain::ReviewRound::outcome`] for a verdict the operator landed the work over.
pub(crate) const REVIEW_LANDED: &str = "landed";

/// How many of `rounds` were sent back to the creator.
pub(crate) fn sent_back_count(rounds: &[crate::domain::ReviewRound]) -> usize {
    rounds
        .iter()
        .filter(|r| r.outcome.as_deref() == Some(crate::domain::REVIEW_SENT_BACK))
        .count()
}

/// The current verdict's items, split for the adjudication prompt: a FAIL item is NEW when no
/// earlier round failed the same item number, RE-RAISED when one did; RULED items are listed as
/// such. Each list holds the item lines as written.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ItemTally {
    pub new: Vec<String>,
    pub re_raised: Vec<String>,
    pub ruled: Vec<String>,
}

pub(crate) fn tally_items(
    rounds: &[crate::domain::ReviewRound],
    attempt: u32,
    verdict: &str,
) -> ItemTally {
    let prior_fail: std::collections::HashSet<u32> = rounds
        .iter()
        .filter(|r| r.attempt != attempt)
        .flat_map(|r| parse_review_items(&r.findings))
        .filter(|i| i.status == "FAIL")
        .map(|i| i.n)
        .collect();
    let mut t = ItemTally::default();
    for i in parse_review_items(verdict) {
        match i.status.as_str() {
            "FAIL" if prior_fail.contains(&i.n) => t.re_raised.push(i.line),
            "FAIL" => t.new.push(i.line),
            "RULED" => t.ruled.push(i.line),
            _ => {}
        }
    }
    t
}

/// What a LANDED review carries forward: its FAIL item lines, or — a verdict written without item
/// lines — its own words (bounded tail) as one entry.
pub(crate) fn carried_items(verdict: &str) -> Vec<String> {
    let fails: Vec<String> = parse_review_items(verdict)
        .into_iter()
        .filter(|i| i.status == "FAIL")
        .map(|i| i.line)
        .collect();
    if !fails.is_empty() || verdict.trim().is_empty() {
        return fails;
    }
    vec![tail(verdict, ROUND_FINDINGS_CAP)]
}

/// One bounded list for the prompt: up to five lines, each ≤ 160 chars, then `(+N more)`.
fn prompt_list(lines: &[String]) -> String {
    let mut parts: Vec<String> = lines
        .iter()
        .take(5)
        .map(|l| {
            if l.chars().count() > 160 {
                format!("{}…", l.chars().take(160).collect::<String>())
            } else {
                l.clone()
            }
        })
        .collect();
    if lines.len() > 5 {
        parts.push(format!("(+{} more)", lines.len() - 5));
    }
    parts.join("; ")
}

/// The `review_adjudication` gate's prompt: the round, the send-backs so far against the cap, the
/// items split new / re-raised / ruled, and the three arms in the operator's words.
pub(crate) fn adjudication_prompt(
    ord: u32,
    creator_ord: u32,
    rounds: &[crate::domain::ReviewRound],
    attempt: u32,
    verdict: &str,
    note: &str,
) -> String {
    let t = tally_items(rounds, attempt, verdict);
    let mut items = Vec::new();
    for (name, list) in [
        ("new", &t.new),
        ("re-raised", &t.re_raised),
        ("ruled", &t.ruled),
    ] {
        if !list.is_empty() {
            items.push(format!("{name} ({}): {}", list.len(), prompt_list(list)));
        }
    }
    let items = if items.is_empty() {
        "The verdict carries no ITEM lines; its findings ride the gate's verdict summary."
            .to_string()
    } else {
        format!("Items — {}.", items.join(" · "))
    };
    format!(
        "Unit {ord} verdict is NOT PASS again — round {}, after {} send-backs to unit \
         {creator_ord} (the rework cap is {MAX_REVIEW_SENDBACKS}), so this gate adjudicates \
         instead of sending it back. {items} Approve = LAND WITH CARRIED ITEMS: the creator's \
         tree is accepted as it stands, the FAIL items are recorded as carried on unit {ord} and \
         the run moves on. Request changes = ONE MORE ROUND: the review goes back to unit \
         {creator_ord} once more (the next NOT PASS returns here). Reject = STOP the run{note}",
        rounds.len(),
        sent_back_count(rounds),
    )
}

fn tail(s: &str, cap: usize) -> String {
    let s = s.trim();
    let n = s.chars().count();
    if n <= cap {
        s.to_string()
    } else {
        format!("…{}", s.chars().skip(n - cap).collect::<String>())
    }
}

/// The evaluator's brief, as `(ord, label, text)` context blocks in the order they are handed:
/// `[review checklist — unit N]` always; `[prior verdicts — unit N]` when the unit returned a
/// NOT-PASS verdict before; `[operator rulings — unit N]` when a human note amended it or the
/// creator unit it reviews; `[floor record — unit M]` when an earlier unit carries the engine's
/// repo-checks record (the latest one — the tree under review).
pub(crate) fn evaluator_brief(
    intent: &str,
    amendments: &[IntentAmendment],
    units: &[WorkUnit],
    unit: &WorkUnit,
) -> Vec<(u32, String, String)> {
    let ord = unit.ord;
    let mut out = Vec::new();
    let round = unit.review_rounds.len() + 1;
    let creator = crate::pipeline::most_recent_prior_creator(units, ord);

    // 1. The checklist and the answer contract.
    let items = done_when_items(intent);
    let mut c = format!("Round {round} of this review (unit {ord}).\n");
    if items.is_empty() {
        c.push_str(
            "The intent names no Done-when list. Number the intent's own requirements 1..n in \
             your answer and judge each one.\n",
        );
    } else {
        c.push_str("Done when — the intent's checklist; judge every item:\n");
        for (i, it) in items.iter().enumerate() {
            c.push_str(&format!("  {}. {it}\n", i + 1));
        }
    }
    if !amendments.is_empty() {
        c.push_str(
            "Approved intent amendments (the operator changed the checklist; they win over the \
             text above):\n",
        );
        for a in amendments {
            c.push_str(&format!("  - {}\n", a.text.trim()));
        }
    }
    c.push_str(
        "Answer with one line per item, above your VERDICT line:\n  ITEM <n>: PASS — <the \
         evidence>\n  ITEM <n>: FAIL — <what is wrong, file:line>\n  ITEM <n>: RULED — <the \
         operator ruling that settles it>\nRules:\n- RULED = an [operator rulings] entry settles \
         the item. A RULED item can never fail the unit; do not re-raise it.\n- VERDICT: FAIL \
         only when at least one item is FAIL. A concern that is not a checklist item goes under \
         \"Follow-ups\" and never fails the unit by itself.\n",
    );
    if round >= 2 {
        c.push_str(
            "- This is round 2 or later: re-judge every item a prior verdict failed against the \
             tree as it is now. A NEW FAIL (an item no prior verdict failed) must say why it was \
             invisible before — e.g. a regression the last rework introduced — or it is a \
             follow-up, not a FAIL.\n",
        );
    }
    c.push_str(
        "- [floor record] is the engine's own run of the repository's checks on the tree under \
         review: cite it; do not report the floor results as unavailable.",
    );
    out.push((ord, format!("[review checklist — unit {ord}]"), c));

    // 2. Its own prior verdicts.
    if !unit.review_rounds.is_empty() {
        let mut p = String::new();
        for (i, r) in unit.review_rounds.iter().enumerate() {
            let fate = match r.outcome.as_deref() {
                Some(crate::domain::REVIEW_SENT_BACK) => "sent back to the creator",
                Some(o) => o,
                None => "not sent back",
            };
            p.push_str(&format!(
                "Round {} (attempt {}) — NOT PASS, {fate}:\n{}\n\n",
                i + 1,
                r.attempt,
                tail(&r.findings, ROUND_FINDINGS_CAP)
            ));
        }
        out.push((
            ord,
            format!("[prior verdicts — unit {ord}]"),
            p.trim_end().to_string(),
        ));
    }

    // 3. The operator's rulings on this unit and on the creator unit it reviews.
    let mut rulings: Vec<(u32, &crate::domain::OperatorRuling)> = unit
        .operator_rulings
        .iter()
        .map(|r| (ord, r))
        .chain(
            creator
                .into_iter()
                .flat_map(|c| c.operator_rulings.iter().map(move |r| (c.ord, r))),
        )
        .collect();
    rulings.sort_by_key(|(_, r)| r.at);
    if !rulings.is_empty() {
        let mut r = String::from(
            "The operator's rulings, verbatim — an item a ruling settles is RULED, never FAIL:\n",
        );
        for (on, ruling) in rulings {
            r.push_str(&format!(
                "- [{} on unit {on}, attempt {}] {}\n",
                ruling.action,
                ruling.attempt,
                ruling.text.trim()
            ));
        }
        out.push((
            ord,
            format!("[operator rulings — unit {ord}]"),
            r.trim_end().to_string(),
        ));
    }

    // 4. The latest floor record on the tree under review — from the creator on (an earlier
    // unit's record judged an older tree).
    let from = creator.map_or(0, |c| c.ord);
    if let Some((fo, report)) = units
        .iter()
        .filter(|u| u.ord < ord && u.ord >= from)
        .filter_map(|u| u.repo_checks.as_ref().map(|r| (u.ord, r)))
        .max_by_key(|(o, _)| *o)
    {
        let tree = report
            .tree
            .as_deref()
            .map(|t| format!(", tree {t}"))
            .unwrap_or_default();
        out.push((
            fo,
            format!("[floor record — unit {fo}]"),
            format!(
                "The engine ran the repository's checks itself after unit {fo}{tree}: {} — {}",
                if report.passed { "PASSED" } else { "FAILED" },
                report.summary()
            ),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_when_reads_the_list_under_its_heading_and_stops_at_the_next_section() {
        let intent = "Fix the gate.\n\n## Where things stand\n- old item\n\n## Done when\n1. \
                      the gate opens\n2) the prompt names\n   the arms\n- [ ] tests pin it\n\n## \
                      Out of scope\n- studio\n";
        assert_eq!(
            done_when_items(intent),
            vec![
                "the gate opens".to_string(),
                "the prompt names the arms".to_string(),
                "tests pin it".to_string()
            ]
        );
    }

    #[test]
    fn done_when_tolerates_label_lines_inline_text_and_bold_heads() {
        assert_eq!(
            done_when_items("x\nDone when: the build passes\n"),
            vec!["the build passes".to_string()]
        );
        assert_eq!(
            done_when_items("**Done-when:**\n- a\n- b\nNext paragraph.\n- c"),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            done_when_items("### Acceptance criteria\nThe page loads in one pass.\n"),
            vec!["The page loads in one pass.".to_string()]
        );
        assert!(done_when_items("just prose, nothing named done when here").is_empty());
    }

    #[test]
    fn item_lines_parse_with_decoration_and_the_last_line_for_an_item_wins() {
        let v = "Findings\n- ITEM 1: PASS — ok\n**ITEM 2: FAIL — GateRow.tsx:188 empty**\nItem \
                 3: ruled — gate 6\nITEM 4: maybe\nITEM 2: FAIL — restated\nVERDICT: FAIL";
        let items = parse_review_items(v);
        assert_eq!(
            items
                .iter()
                .map(|i| (i.n, i.status.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "PASS"), (2, "FAIL"), (3, "RULED")]
        );
        assert!(items[1].line.ends_with("restated"));
    }

    fn round(attempt: u32, findings: &str, sent_back: bool) -> crate::domain::ReviewRound {
        crate::domain::ReviewRound {
            attempt,
            findings: findings.into(),
            outcome: sent_back.then(|| crate::domain::REVIEW_SENT_BACK.to_string()),
        }
    }

    /// (core#761) The adjudication prompt splits the current verdict's items against the earlier
    /// rounds — re-raised (an earlier round failed the same item), new, ruled — counts the
    /// send-backs against the cap and names the three arms.
    #[test]
    fn the_adjudication_prompt_splits_new_re_raised_and_ruled_items_and_names_the_arms() {
        let rounds = vec![
            round(0, "ITEM 1: FAIL — a\nITEM 2: PASS\nVERDICT: FAIL", true),
            round(1, "ITEM 2: FAIL — b\nVERDICT: FAIL", true),
            round(2, "", false),
        ];
        let verdict =
            "ITEM 1: FAIL — a again\nITEM 2: RULED — gate 6\nITEM 3: FAIL — c\nVERDICT: FAIL";
        let t = tally_items(&rounds, 2, verdict);
        assert_eq!(t.re_raised, vec!["ITEM 1: FAIL — a again".to_string()]);
        assert_eq!(t.new, vec!["ITEM 3: FAIL — c".to_string()]);
        assert_eq!(t.ruled, vec!["ITEM 2: RULED — gate 6".to_string()]);
        assert_eq!(sent_back_count(&rounds), 2);
        let p = adjudication_prompt(7, 6, &rounds, 2, verdict, "");
        assert!(
            p.starts_with(
                "Unit 7 verdict is NOT PASS again — round 3, after 2 send-backs to unit 6"
            ),
            "{p}"
        );
        for want in [
            "new (1): ITEM 3: FAIL — c",
            "re-raised (1): ITEM 1: FAIL — a again",
            "ruled (1): ITEM 2: RULED — gate 6",
            "Approve = LAND WITH CARRIED ITEMS",
            "Request changes = ONE MORE ROUND",
            "Reject = STOP the run",
        ] {
            assert!(p.contains(want), "{want}: {p}");
        }
        assert_eq!(
            carried_items(verdict),
            vec![
                "ITEM 1: FAIL — a again".to_string(),
                "ITEM 3: FAIL — c".to_string()
            ]
        );
        assert_eq!(carried_items("no items here\nVERDICT: FAIL").len(), 1);
    }

    fn unit(ord: u32, role: crate::workflow::PhaseRole) -> WorkUnit {
        let mut u = WorkUnit::pending(format!("r:{ord}"), "r", ord, format!("unit {ord}"));
        u.role = role;
        u
    }

    fn ruling(text: &str, at: i64) -> crate::domain::OperatorRuling {
        crate::domain::OperatorRuling {
            action: "request_changes".into(),
            text: text.into(),
            attempt: 0,
            at,
        }
    }

    /// The brief's text: the numbered checklist and the PASS / FAIL / RULED contract with its two
    /// rules (RULED never fails; a NEW item in round >= 2 says why it was invisible), each prior
    /// verdict with its fate, the rulings on the evaluator AND its creator in time order, and the
    /// latest floor record from the creator on (not an older one before it).
    #[test]
    fn the_brief_carries_the_checklist_contract_history_rulings_and_latest_floor() {
        use crate::workflow::PhaseRole;
        let floor = |tree: &str, passed: bool| -> crate::repo_checks::RepoChecksReport {
            serde_json::from_value(serde_json::json!({
                "detected": [], "checks": [], "skipped": [], "passed": passed,
                "sandbox_level": "none", "tree": tree
            }))
            .unwrap()
        };
        let mut early = unit(1, PhaseRole::Neutral);
        early.repo_checks = Some(floor("old", false));
        let mut creator = unit(2, PhaseRole::Creator);
        creator.repo_checks = Some(floor("tree-b", true));
        creator
            .operator_rulings
            .push(ruling("keep the step card", 5));
        let mut eval = unit(3, PhaseRole::Evaluator);
        eval.review_rounds.push(crate::domain::ReviewRound {
            attempt: 0,
            findings: "ITEM 2: FAIL — no test".into(),
            outcome: Some(crate::domain::REVIEW_SENT_BACK.into()),
        });
        eval.operator_rulings
            .push(ruling("unknown choices stay disabled", 9));
        let amend = [IntentAmendment {
            text: "item 3 withdrawn".into(),
            ord: 2,
            at: 1,
        }];
        let units = vec![early, creator, eval.clone()];
        let brief = evaluator_brief(
            "Do it.\n## Done when\n- the gate opens\n- tests pin it\n",
            &amend,
            &units,
            &eval,
        );
        let labels: Vec<&str> = brief.iter().map(|(_, l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "[review checklist — unit 3]",
                "[prior verdicts — unit 3]",
                "[operator rulings — unit 3]",
                "[floor record — unit 2]"
            ]
        );
        let c = &brief[0].2;
        assert!(c.starts_with("Round 2 of this review (unit 3)."), "{c}");
        assert!(
            c.contains("  1. the gate opens\n  2. tests pin it\n"),
            "{c}"
        );
        assert!(c.contains("item 3 withdrawn"), "{c}");
        assert!(c.contains("ITEM <n>: RULED"), "{c}");
        assert!(c.contains("A RULED item can never fail the unit"), "{c}");
        assert!(c.contains("must say why it was invisible before"), "{c}");
        assert!(brief[1]
            .2
            .contains("Round 1 (attempt 0) — NOT PASS, sent back to the creator"));
        assert!(brief[1].2.contains("ITEM 2: FAIL — no test"));
        let r = &brief[2].2;
        let (a, b) = (
            r.find("keep the step card").unwrap(),
            r.find("unknown choices stay disabled").unwrap(),
        );
        assert!(a < b, "time order: {r}");
        assert!(r.contains("on unit 2,") && r.contains("on unit 3,"), "{r}");
        assert!(
            brief[3].2.contains("PASSED") && brief[3].2.contains("tree-b"),
            "{}",
            brief[3].2
        );

        // A first round with no history: the checklist and nothing else but the floor; with no
        // done-when list the reviewer is told to number the intent's own requirements.
        // A floor record from BEFORE the creator judged an older tree: not handed.
        let fresh = unit(3, PhaseRole::Evaluator);
        let mut before = unit(1, PhaseRole::Neutral);
        before.repo_checks = Some(floor("old", true));
        let brief = evaluator_brief(
            "just do it",
            &[],
            &[before, unit(2, PhaseRole::Creator)],
            &fresh,
        );
        assert_eq!(brief.len(), 1, "{brief:?}");
        assert!(brief[0].2.contains("The intent names no Done-when list"));
        assert!(!brief[0].2.contains("round 2 or later"));
    }
}
