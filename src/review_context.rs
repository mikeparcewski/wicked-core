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
//! The verdict contract this brief asks for — one `ITEM <n>: PASS | FAIL | RULED | DEFERRED` line
//! per checklist item above the `VERDICT:` line — gives a later round (and the operator) stable
//! item numbers to hold a verdict to. DEFERRED is for a criterion only delivery can show (CI on
//! the opened pull request, the PR body, a merge): it never fails the unit, and the deliver gate
//! lists it ([`deferred_to_delivery`], [`delivery_checks_note`]). [`parse_review_items`] reads
//! the lines back when the rework cap (core#761, [`MAX_REVIEW_SENDBACKS`]) opens its
//! `review_adjudication` gate. The `VERDICT:` line
//! is still the only thing the gate fold parses ([`crate::validator::parse_evaluator_verdict`]).

use crate::domain::{IntentAmendment, WorkUnit};

/// The most of one prior verdict handed back (its TAIL — the contract puts the findings and the
/// item lines just above the verdict line, which is last).
const ROUND_FINDINGS_CAP: usize = 3000;

/// The labels that head the in-run checklist (normalised: lowercase, `-`/`_` as spaces).
const DONE_WHEN_HEADS: &[&str] = &["done when", "acceptance criteria", "definition of done"];

/// The labels that head the intent's AFTER-DELIVERY checks: criteria whose evidence exists only
/// once the deliver phase pushes (CI on the opened pull request, the PR body, a merge).
const AFTER_DELIVERY_HEADS: &[&str] = &["after delivery", "post delivery"];

/// The `done when` items of an intent, in order: the list under the first heading (or label line)
/// that reads `Done when` / `Done-when` / `Acceptance criteria` / `Definition of done`, markers
/// stripped (`-`, `*`, `+`, `1.`, `1)`, `[ ]`, `[x]`). An indented line continues the item above
/// it; the list ends at the next heading, the next section label (`After delivery:`) or the next
/// unindented prose line. A label line with text after its colon (`Done when: the build passes`)
/// is itself an item, and a section written as one paragraph is one item. No such section → empty
/// (the brief then asks the reviewer to number the intent's own requirements).
pub(crate) fn done_when_items(intent: &str) -> Vec<String> {
    section_items(intent, DONE_WHEN_HEADS)
}

/// The intent's `After delivery` items (same list grammar as [`done_when_items`]): the checks the
/// reviewer answers `DEFERRED` and the deliver gate lists for the operator. Empty when the intent
/// has no such section.
pub(crate) fn after_delivery_items(intent: &str) -> Vec<String> {
    section_items(intent, AFTER_DELIVERY_HEADS)
}

fn section_items(intent: &str, heads: &[&str]) -> Vec<String> {
    let mut lines = intent.lines();
    let mut items: Vec<String> = Vec::new();
    // Find the section head.
    loop {
        let Some(line) = lines.next() else {
            return items;
        };
        if let Some(rest) = section_head(line, heads) {
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
        if trimmed.starts_with('#')
            || section_head(line, DONE_WHEN_HEADS).is_some()
            || section_head(line, AFTER_DELIVERY_HEADS).is_some()
        {
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

/// `Some(rest-after-colon)` when `line` heads a section labelled one of `heads`.
fn section_head(line: &str, heads: &[&str]) -> Option<String> {
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
    /// `PASS` | `FAIL` | `RULED` | `DEFERRED` (uppercased).
    pub status: String,
    /// The line as written (trimmed of decoration).
    pub line: String,
}

/// The item status for a criterion whose evidence exists only after delivery — CI on the opened
/// pull request, the PR's title or body, a merge. Neither a pass nor a fail: it never fails the
/// unit, and the deliver gate lists it ([`delivery_checks_note`]) for the operator to check.
pub(crate) const DEFERRED: &str = "DEFERRED";

/// The `ITEM <n>: PASS | FAIL | RULED | DEFERRED` lines of a verdict, in order; the LAST line for
/// an item number wins (a reviewer restating an item). Decoration (`-`, `*`, `#`, `>`, backticks)
/// is tolerated; a line whose status is none of the four is not an item line.
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
        if !matches!(status.as_str(), "PASS" | "FAIL" | "RULED" | DEFERRED) {
            continue;
        }
        // Closing decoration: the `**` of a bold line, and only as many backticks as OPENED it —
        // an item ending in a code span (`PR body \`Closes #7\``) keeps its closing backtick.
        let opened_ticks = line
            .trim()
            .chars()
            .take_while(|c| matches!(c, '#' | '*' | '-' | '>' | '`' | ' ' | '\t'))
            .filter(|c| *c == '`')
            .count();
        let mut end = bare.trim_end_matches('*');
        for _ in 0..opened_ticks {
            end = end.strip_suffix('`').unwrap_or(end);
        }
        let line = end.trim_end_matches('*').trim().to_string();
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

/// The items the run's reviews DEFERRED to delivery, as `(evaluator ord, item line)` in item
/// order: every Evaluator agent unit before `deliver_ord` that counted (`Done`) is read — its
/// approved verdict (`approved_output`), or, for a review the operator landed over a NOT-PASS
/// verdict, that verdict's words. An item number any of them deferred is listed once (the latest
/// deferring line), even when another review judged it: a check named twice costs a glance, a
/// dropped one ships unchecked.
pub(crate) fn deferred_to_delivery(
    units: &[WorkUnit],
    deliver_ord: u32,
    approved_output: impl Fn(&WorkUnit) -> Option<String>,
) -> Vec<(u32, String)> {
    let mut deferred: std::collections::BTreeMap<u32, (u32, String)> = Default::default();
    for u in units.iter().filter(|u| {
        u.ord < deliver_ord
            && u.role == crate::workflow::PhaseRole::Evaluator
            && u.tool_cmd.is_none()
            && u.status == crate::domain::UnitStatus::Done
    }) {
        let verdict = approved_output(u).or_else(|| {
            u.review_rounds
                .iter()
                .rev()
                .find(|r| r.outcome.as_deref() == Some(REVIEW_LANDED))
                .map(|r| r.findings.clone())
        });
        for i in verdict
            .as_deref()
            .map(parse_review_items)
            .unwrap_or_default()
        {
            if i.status == DEFERRED {
                deferred.insert(i.n, (u.ord, i.line));
            }
        }
    }
    deferred.into_values().collect()
}

/// The deliver gate card's post-delivery checks: the [`deferred_to_delivery`] items, said as what
/// they are — checks the engine does NOT run after the push, for the operator to make on the
/// opened pull request before merging. Empty when no review deferred anything.
pub(crate) fn delivery_checks_note(deferred: &[(u32, String)]) -> String {
    if deferred.is_empty() {
        return String::new();
    }
    let lines = deferred
        .iter()
        .map(|(ord, line)| {
            let line = if line.chars().count() > 240 {
                format!("{}…", line.chars().take(240).collect::<String>())
            } else {
                line.clone()
            };
            format!("{line} (review unit {ord})")
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        " After delivery — {} check{} the review deferred to the opened pull request; the engine \
         does not verify {} after the push, so check {} there before you merge: {lines}.",
        deferred.len(),
        if deferred.len() == 1 { "" } else { "s" },
        if deferred.len() == 1 { "it" } else { "them" },
        if deferred.len() == 1 { "it" } else { "each" },
    )
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

/// The deferral rule of the review contract: judge only what the run can show before delivery;
/// a criterion that needs a delivery artifact is DEFERRED (never PASS, never FAIL) and the deliver
/// gate lists it. Deny-dominates: deferral never lifts a failing in-run part of an item.
pub(crate) const DEFERRAL_RULE: &str = "- DEFERRED = the item's evidence can exist only after the \
     deliver phase pushes: CI results on the opened pull request, the pull request's title or \
     body, a merge, a release. Judge only what the run's tree, your own runs in it and the [floor \
     record] show before delivery. An item that mixes the two is judged on its in-run part first: \
     FAIL when that part fails (a deferral never lifts a failing in-run part); DEFERRED, naming \
     what waits, only when every in-run part passes. Never FAIL an item because a delivery \
     artifact does not exist yet, and never PASS one on a promise that it will. A DEFERRED item \
     never fails the unit; the deliver gate lists it for the operator to check on the opened \
     pull request.\n";

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
    let after = after_delivery_items(intent);
    if !after.is_empty() {
        c.push_str(
            "After delivery — the intent's checks on what the deliver phase creates; nothing in \
             this run can show them yet, so answer each DEFERRED:\n",
        );
        for (i, it) in after.iter().enumerate() {
            if items.is_empty() {
                c.push_str(&format!("  - {it} (number it after your own items)\n"));
            } else {
                c.push_str(&format!("  {}. {it}\n", items.len() + i + 1));
            }
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
         operator ruling that settles it>\n  ITEM <n>: DEFERRED — <the delivery artifact it \
         waits for>\nRules:\n- RULED = an [operator rulings] entry settles the item. A RULED \
         item can never fail the unit; do not re-raise it.\n",
    );
    c.push_str(DEFERRAL_RULE);
    c.push_str(
        "- VERDICT: FAIL only when at least one item is FAIL. A concern that is not a checklist \
         item goes under \"Follow-ups\" and never fails the unit by itself.\n",
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
        // Closing decoration goes, a closing code span stays.
        let v =
            "`ITEM 1: PASS — ok`\n**ITEM 2: DEFERRED — PR body `Closes #7`**\nITEM 3: DEFERRED — \
                 `merge`";
        let lines: Vec<String> = parse_review_items(v).into_iter().map(|i| i.line).collect();
        assert_eq!(
            lines,
            vec![
                "ITEM 1: PASS — ok".to_string(),
                "ITEM 2: DEFERRED — PR body `Closes #7`".to_string(),
                "ITEM 3: DEFERRED — `merge`".to_string()
            ]
        );
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

    /// An intent split into `Done when` and `After delivery`: the checklist stops at the second
    /// section's label (even right after its own head), and the after-delivery list reads on its
    /// own with the same grammar.
    #[test]
    fn an_after_delivery_section_ends_the_checklist_and_reads_on_its_own() {
        let intent = "Fix it.\nDONE WHEN:\n1. tests pin it\n2. CHANGELOG bullet\nAFTER DELIVERY:\n1. \
                      CI green on ubuntu/macos/windows\n2. PR body `Closes #840`\nOUT OF SCOPE: x\n";
        assert_eq!(
            done_when_items(intent),
            vec!["tests pin it".to_string(), "CHANGELOG bullet".to_string()]
        );
        assert_eq!(
            after_delivery_items(intent),
            vec![
                "CI green on ubuntu/macos/windows".to_string(),
                "PR body `Closes #840`".to_string()
            ]
        );
        assert!(done_when_items("Done when:\nAfter delivery:\n- CI green\n").is_empty());
        assert_eq!(
            after_delivery_items("## Post-delivery\n- merged\n"),
            vec!["merged".to_string()]
        );
        assert!(after_delivery_items("Done when:\n- a\n").is_empty());
    }

    /// `DEFERRED` is an item status; it is neither a FAIL (the adjudication tally and the carried
    /// items ignore it) nor a PASS.
    #[test]
    fn deferred_item_lines_parse_and_never_count_as_fail() {
        let v =
            "ITEM 1: PASS — ok\n- **ITEM 2: Deferred — CI runs on the opened PR**\nITEM 3: FAIL \
                 — no test\nVERDICT: FAIL";
        let items = parse_review_items(v);
        assert_eq!(
            items
                .iter()
                .map(|i| (i.n, i.status.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "PASS"), (2, DEFERRED), (3, "FAIL")]
        );
        let t = tally_items(&[], 0, v);
        assert_eq!(t.new, vec!["ITEM 3: FAIL — no test".to_string()]);
        assert!(t.re_raised.is_empty() && t.ruled.is_empty());
        assert_eq!(carried_items(v), vec!["ITEM 3: FAIL — no test".to_string()]);
    }

    /// The composed review prompt carries the deferral rule — judge only what the run shows before
    /// delivery, DEFERRED for a delivery artifact, deny-dominates on a mixed item, DEFERRED never
    /// fails the unit — and numbers the intent's after-delivery checks after its checklist.
    #[test]
    fn the_brief_carries_the_deferral_rule_and_numbers_after_delivery_checks() {
        use crate::workflow::PhaseRole;
        let eval = unit(3, PhaseRole::Evaluator);
        let units = vec![unit(2, PhaseRole::Creator), eval.clone()];
        let brief = evaluator_brief(
            "Do it.\nDone when:\n1. tests pin it\n2. CHANGELOG bullet\nAfter delivery:\n1. CI \
             green on the opened PR\n2. PR body `Closes #7`\n",
            &[],
            &units,
            &eval,
        );
        let c = &brief[0].2;
        assert!(
            c.contains("  1. tests pin it\n  2. CHANGELOG bullet\n"),
            "{c}"
        );
        assert!(
            c.contains(
                "answer each DEFERRED:\n  3. CI green on the opened PR\n  4. PR body `Closes #7`\n"
            ),
            "{c}"
        );
        assert!(
            c.contains("ITEM <n>: DEFERRED — <the delivery artifact it waits for>"),
            "{c}"
        );
        assert!(c.contains(DEFERRAL_RULE), "{c}");
        for want in [
            "CI results on the opened pull request",
            "the pull request's title or body",
            "a merge",
            "FAIL when that part fails (a deferral never lifts a failing in-run part)",
            "Never FAIL an item because a delivery artifact does not exist yet",
            "never PASS one on a promise that it will",
            "A DEFERRED item never fails the unit",
            "the deliver gate lists it",
        ] {
            assert!(DEFERRAL_RULE.contains(want), "{want}");
        }
        assert!(
            c.contains("VERDICT: FAIL only when at least one item is FAIL"),
            "{c}"
        );

        // The rule rides every brief, with or without an after-delivery section; with no in-run
        // list the after-delivery checks are bulleted for the reviewer to number after its own.
        let plain = evaluator_brief(
            "just do it\nAfter delivery:\n- merged\n",
            &[],
            &units,
            &eval,
        );
        let c = &plain[0].2;
        assert!(c.contains("The intent names no Done-when list"), "{c}");
        assert!(
            c.contains("  - merged (number it after your own items)\n"),
            "{c}"
        );
        assert!(c.contains(DEFERRAL_RULE), "{c}");
        let none = evaluator_brief("Done when:\n- a\n", &[], &units, &eval);
        assert!(none[0].2.contains(DEFERRAL_RULE) && !none[0].2.contains("After delivery —"));
    }

    /// What the deliver gate lists: the DEFERRED items of every counted Evaluator agent unit before
    /// the deliver unit — an approved verdict, or a landed review's verdict words — one line per
    /// item number; a pending review, a creator, a tool step and a unit after delivery add nothing.
    #[test]
    fn deferred_to_delivery_reads_every_counted_review_and_the_note_names_each_check() {
        use crate::workflow::PhaseRole;
        let done = |ord: u32, role: PhaseRole| {
            let mut u = unit(ord, role);
            u.status = crate::domain::UnitStatus::Done;
            u
        };
        let creator = done(1, PhaseRole::Creator);
        let review = done(2, PhaseRole::Evaluator);
        let mut landed = done(3, PhaseRole::Evaluator);
        landed.review_rounds.push(crate::domain::ReviewRound {
            attempt: 1,
            findings: "ITEM 4: FAIL — flaky\nITEM 6: DEFERRED — merge\nVERDICT: FAIL".into(),
            outcome: Some(REVIEW_LANDED.into()),
        });
        let pending = unit(4, PhaseRole::Evaluator);
        let mut tool = done(5, PhaseRole::Evaluator);
        tool.tool_cmd = Some(vec!["true".into()]);
        let after = done(7, PhaseRole::Evaluator);
        let units = vec![creator, review, landed, pending, tool, after];
        let outputs = |u: &WorkUnit| -> Option<String> {
            match u.ord {
                1 => Some("ITEM 9: DEFERRED — creator words".into()),
                2 => Some(
                    "ITEM 1: PASS — ok\nITEM 5: DEFERRED — CI on the opened PR\nITEM 6: DEFERRED \
                     — PR body\nVERDICT: PASS"
                        .into(),
                ),
                4 | 5 | 7 => Some("ITEM 8: DEFERRED — not counted\nVERDICT: PASS".into()),
                _ => None,
            }
        };
        let d = deferred_to_delivery(&units, 6, outputs);
        assert_eq!(
            d,
            vec![
                (2, "ITEM 5: DEFERRED — CI on the opened PR".to_string()),
                (3, "ITEM 6: DEFERRED — merge".to_string()),
            ]
        );
        let note = delivery_checks_note(&d);
        assert!(
            note.starts_with(
                " After delivery — 2 checks the review deferred to the opened pull request; the \
                 engine does not verify them after the push, so check each there before you merge:"
            ),
            "{note}"
        );
        assert!(note.contains("ITEM 5: DEFERRED — CI on the opened PR (review unit 2); ITEM 6: DEFERRED — merge (review unit 3)."), "{note}");
        assert_eq!(delivery_checks_note(&[]), "");
        let one = delivery_checks_note(&d[..1]);
        assert!(
            one.contains("1 check the review deferred") && one.contains("check it there"),
            "{one}"
        );
    }
}
