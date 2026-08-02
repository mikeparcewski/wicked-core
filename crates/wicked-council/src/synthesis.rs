//! 3-layer synthesis: raw votes → matrix → [`Verdict`].
//!
//! **The cardinal rule:** consensus is measured by RISK CONVERGENCE — how many CLIs
//! independently converge on the same recommendation and cite the same risks — **NOT** by
//! averaging uncalibrated model confidence numbers. `agreement_ratio =
//! winning_vote_count / total_votes`.

use std::collections::BTreeMap;

use crate::types::{Verdict, Vote};

/// Layer (b): the synthesis matrix — counts of each recommendation and each risk.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Matrix {
    /// recommendation → how many CLIs recommended it (most-cited first).
    pub recommendation_counts: Vec<(String, u32)>,
    /// top_risk → how many CLIs cited it (most-cited first).
    pub risk_counts: Vec<(String, u32)>,
    /// Total number of votes.
    pub total: u32,
}

/// Normalise a free-text recommendation/risk to a convergence key: lowercased, trimmed,
/// internal whitespace collapsed. So "JWT (stateless)" and "jwt (stateless)" converge.
/// Empty strings collapse to "" and are dropped from risk convergence.
fn norm(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The convergence key for a **recommendation**, which is not the same question as for a risk.
///
/// A risk is free prose and `norm` is the whole of its identity. A recommendation is a CHOICE
/// AMONG ENUMERATED OPTIONS: `render_ballot` numbers them `1.`, `2.`, … and the ballot asks for
/// `RECOMMENDATION: <option number and rationale>`. The consumer already knows this — the router
/// resolves a winner by parsing exactly that leading integer and indexing the option table
/// (`distribute.rs`, "Parse the leading integer from the recommendation text"). So the option
/// number IS the vote; the rationale trailing it is commentary on the vote.
///
/// Keying the whole line instead made the counter and the consumer disagree about what a vote is,
/// and because the ballot MANDATES a rationale, two seats that picked the same option always
/// disagreed — they had explained themselves differently. At three seats every ratio was therefore
/// 1/3, `APPROVAL_THRESHOLD` (0.75) was unreachable in principle, every council spent all
/// `MAX_BALLOTS` rounds, and the winner fell to the tie-break: the lowest option number any seat
/// named, i.e. position rather than judgement (FINDING-056).
///
/// Padded so the key orders numerically — the tie-break is key-ascending, and "10" sorts before
/// "2" as text. A recommendation that does not lead with a digit falls back to `norm`, which is
/// both the old behaviour and the right one: it is a council choosing among un-numbered options,
/// where the prose is all the identity there is.
/// `0` is rejected because the consumer rejects it: the router accepts `n >= 1 && n <= options`,
/// options being 1-indexed. Treating "0 ..." as a numbered vote would let it win a tally here and
/// then degrade at routing — agreement claimed on a choice that can never be acted on. Falling back
/// to `norm` keys it as the prose it effectively is. The upper bound is deliberately NOT checked:
/// the option count is the router's knowledge, not the matrix's, and an out-of-range high number
/// degrades there exactly as it did before this function existed.
pub(crate) fn rec_key(s: &str) -> String {
    let lead: String = s
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    match lead.parse::<u32>() {
        Ok(n) if n >= 1 => format!("#{n:06}"),
        _ => norm(s),
    }
}

/// Build the matrix (layer b) from raw votes (layer a).
pub fn build_matrix(votes: &[Vote]) -> Matrix {
    let mut rec: BTreeMap<String, (String, u32)> = BTreeMap::new();
    let mut risk: BTreeMap<String, (String, u32)> = BTreeMap::new();

    for v in votes {
        let rk = rec_key(&v.recommendation);
        if !rk.is_empty() {
            let entry = rec.entry(rk).or_insert((v.recommendation.clone(), 0));
            entry.1 += 1;
        }
        let risk_k = norm(&v.top_risk);
        if !risk_k.is_empty() {
            let entry = risk.entry(risk_k).or_insert((v.top_risk.clone(), 0));
            entry.1 += 1;
        }
    }

    let recommendation_counts = sort_counts(rec);
    let risk_counts = sort_counts(risk);

    Matrix {
        recommendation_counts,
        risk_counts,
        total: votes.len() as u32,
    }
}

/// Collapse a `key -> (display, count)` map into a `(display, count)` list sorted by count desc,
/// then **key** asc.
///
/// The tie-break is on the key and not the display because for recommendations they now differ:
/// the key is the option number and the display is whichever seat's rationale was seen first, so
/// ordering by display would rank a dead tie by prose. Risks key on `norm(display)`, so for them
/// this is the same ordering it always was, modulo case.
fn sort_counts(map: BTreeMap<String, (String, u32)>) -> Vec<(String, u32)> {
    let mut v: Vec<(String, String, u32)> = map
        .into_iter()
        .map(|(k, (display, count))| (k, display, count))
        .collect();
    v.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    v.into_iter()
        .map(|(_, display, count)| (display, count))
        .collect()
}

/// Synthesize the [`Verdict`] (layer c) from votes.
///
/// - Winner = the option with the most votes, where two seats picking the same option agree
///   however differently they justified it (see [`rec_key`]). A dead tie breaks to the lowest
///   option number — deterministic, and honestly reported: a 1-1-1 split is exactly the
///   `NoConsensus` that `kind` and `agreement_ratio` then say it is.
/// - `agreement_ratio` = winning count / votes **cast**.
/// - Consensus = a **strict majority of the SEATED council** (winner count * 2 > seated).
///   Counts agreement, never confidence.
/// - `risk_convergence` = risks cited by ≥1 CLI, most-cited first (the high-signal axis).
/// - `dissent` = non-winning recommendations.
///
/// `seated` is how many seats were convened, which is ≥ the number of votes cast — every seat
/// that did not answer is the difference. It is a parameter rather than something derivable
/// from `votes` precisely because a missing vote leaves no trace in the vote list: without it,
/// one seat answering out of three is arithmetically indistinguishable from a one-seat council,
/// and both render as 100% unanimous (FINDING-026 D). Pass 0 only when the seated count is
/// genuinely unknown; it then degrades to the cast count, i.e. the pre-quorum behaviour.
pub fn synthesize(task_id: &str, votes: &[Vote], seated: u32) -> Verdict {
    let matrix = build_matrix(votes);
    let total = matrix.total;

    let (winning_recommendation, winning_count) = match matrix.recommendation_counts.first() {
        Some((rec, count)) => (Some(rec.clone()), *count),
        None => (None, 0),
    };

    let agreement_ratio = if total == 0 {
        0.0
    } else {
        winning_count as f32 / total as f32
    };

    // Strict majority of the SEATED council converges on the winner. `max(total)` is defensive:
    // an under-reported `seated` (including the 0 that legacy records deserialize to) must never
    // be able to manufacture a majority — it can only fail to detect a lost one.
    let seated = seated.max(total);
    let consensus = total > 0 && winning_count * 2 > seated;

    let dissent: Vec<String> = matrix
        .recommendation_counts
        .iter()
        .skip(1)
        .map(|(rec, _)| rec.clone())
        .collect();

    // Among the seats that ANSWERED, did the winner take a majority? This separates the two
    // reasons a council fails to reach consensus, which the summary string must not conflate:
    // the seats disagreed, or the seats never spoke. Only the first is a split.
    let split_among_cast = winning_count * 2 <= total;

    let kind = match &winning_recommendation {
        Some(rec) if consensus => format!("Consensus: {rec} ({winning_count}/{seated} seats)"),
        // Would have carried among those who answered; what stopped it is the seats that didn't.
        Some(rec) if !split_among_cast => format!(
            "NoConsensus (quorum lost): {rec} ({winning_count}/{seated} seats, {total} returned)"
        ),
        Some(rec) => {
            let alt = dissent.first().cloned().unwrap_or_default();
            if alt.is_empty() {
                format!("NoConsensus: {rec} ({winning_count}/{total})")
            } else {
                format!("NoConsensus: {rec} vs {alt}")
            }
        }
        None => "NoConsensus: no votes".to_string(),
    };

    Verdict {
        task_id: task_id.to_string(),
        kind,
        consensus,
        seated,
        winning_recommendation,
        agreement_ratio,
        risk_convergence: matrix.risk_counts,
        dissent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Confidence;

    fn vote(cli: &str, rec: &str, risk: &str) -> Vote {
        Vote {
            cli: cli.into(),
            recommendation: rec.into(),
            top_risk: risk.into(),
            change_my_mind: "n/a".into(),
            disqualifier: None,
            confidence: Confidence::Verified,
            provenance: "test".into(),
        }
    }

    #[test]
    fn two_agree_is_consensus_with_shared_risk() {
        let votes = vec![
            vote("a", "Option A", "latency"),
            vote("b", "Option A", "latency"),
        ];
        let v = synthesize("t1", &votes, 2);
        assert!(v.consensus, "2/2 on A must be consensus");
        assert_eq!(v.winning_recommendation.as_deref(), Some("Option A"));
        assert_eq!(v.agreement_ratio, 1.0);
        // Shared risk surfaces, cited by both.
        assert_eq!(
            v.risk_convergence.first(),
            Some(&("latency".to_string(), 2))
        );
    }

    #[test]
    fn split_vote_is_no_consensus() {
        let votes = vec![
            vote("a", "Option A", "latency"),
            vote("b", "Option B", "cost"),
        ];
        let v = synthesize("t2", &votes, 2);
        assert!(!v.consensus, "1-1 split is not a strict majority");
        assert!((v.agreement_ratio - 0.5).abs() < f32::EPSILON);
        assert!(v.kind.starts_with("NoConsensus"));
    }

    #[test]
    fn majority_of_three_is_consensus() {
        let votes = vec![
            vote("a", "A", "latency"),
            vote("b", "A", "latency"),
            vote("c", "B", "cost"),
        ];
        let v = synthesize("t3", &votes, 3);
        assert!(v.consensus, "2 of 3 is a strict majority");
        assert_eq!(v.winning_recommendation.as_deref(), Some("A"));
        // 2 cite latency, 1 cites cost → latency converges higher.
        assert_eq!(
            v.risk_convergence.first(),
            Some(&("latency".to_string(), 2))
        );
        assert_eq!(v.dissent, vec!["B".to_string()]);
    }

    #[test]
    fn one_seat_of_three_answering_is_not_unanimous() {
        // The FINDING-026 D record, verbatim: a three-seat council where two seats timed out and
        // the survivor's pick was persisted as `agreement=100% dissent=0 degraded=None`. Nothing
        // in the artifact said the quorum was lost, so the audit trail asserted a three-seat
        // consensus that never happened.
        let votes = vec![vote("a", "Option A", "latency")];
        let v = synthesize("t5", &votes, 3);
        assert!(
            !v.consensus,
            "1 of 3 seats is not a majority of the council: {v:?}"
        );
        assert_eq!(
            v.seated, 3,
            "the quorum denominator must survive on the record"
        );
        // Agreement is unchanged and still honest about what it measures: of the seats that
        // answered, all of them agreed. It is `consensus` + `seated` that carry the quorum.
        assert_eq!(v.agreement_ratio, 1.0);
        assert!(
            v.kind.contains("quorum lost") && v.kind.contains("1/3 seats"),
            "the summary must name the lost quorum, got {:?}",
            v.kind
        );
    }

    #[test]
    fn two_seats_of_three_agreeing_still_carries() {
        // Quorum is a majority of the seated council, not unanimity: losing one seat of three
        // must not veto a decision the remaining two genuinely converged on. Otherwise the fix
        // for a false positive would just manufacture false negatives.
        let votes = vec![vote("a", "A", "latency"), vote("b", "A", "latency")];
        let v = synthesize("t6", &votes, 3);
        assert!(v.consensus, "2 of 3 seats is a strict majority: {v:?}");
        assert!(
            v.kind.starts_with("Consensus: A (2/3 seats)"),
            "{:?}",
            v.kind
        );
    }

    #[test]
    fn a_split_among_the_seats_that_answered_is_not_reported_as_lost_quorum() {
        // Two distinct reasons a council fails to converge, and the summary must not conflate
        // them: 1-1 among two of three seats is a genuine disagreement, not silence.
        let votes = vec![vote("a", "A", "latency"), vote("b", "B", "cost")];
        let v = synthesize("t7", &votes, 3);
        assert!(!v.consensus);
        assert!(
            !v.kind.contains("quorum lost"),
            "a real split must not be reported as absence, got {:?}",
            v.kind
        );
        assert_eq!(v.kind, "NoConsensus: A vs B");
    }

    #[test]
    fn an_unreported_seated_count_degrades_to_the_cast_count() {
        // `seated: 0` is what a pre-quorum record deserializes to (`#[serde(default)]`) and what
        // a caller passes when it genuinely does not know. It must never MANUFACTURE a majority
        // by shrinking the denominator below what was actually cast.
        let votes = vec![vote("a", "A", "latency"), vote("b", "A", "latency")];
        let v = synthesize("t8", &votes, 0);
        assert!(v.consensus, "2/2 stands on the cast count alone");
        assert_eq!(v.seated, 2, "0 must widen to the cast count, never stay 0");
    }

    #[test]
    fn case_insensitive_recommendations_converge() {
        let votes = vec![
            vote("a", "JWT", "revocation"),
            vote("b", "jwt", "revocation"),
        ];
        let v = synthesize("t4", &votes, 2);
        assert!(v.consensus);
        assert_eq!(v.agreement_ratio, 1.0);
    }

    // FINDING-056. Shaped like a real ballot, because the defect only appears in that shape: the
    // prompt asks for "<option number and rationale>", so two seats picking the same option in
    // good faith still emit different strings, and keying on the whole string counted them as a
    // disagreement. Every live council sat at exactly this — 33%, dissent 2, three seats.
    #[test]
    fn same_option_with_different_rationales_is_agreement() {
        let votes = vec![
            vote(
                "a",
                "1 — strongest at multi-file refactors",
                "context limits",
            ),
            vote(
                "b",
                "1 because it handles large repos best",
                "context limits",
            ),
            vote("c", "2 — faster iteration on small edits", "shallow review"),
        ];
        let v = synthesize("t9", &votes, 3);
        assert!(
            v.consensus,
            "two seats named option 1; that is a majority however they phrased it"
        );
        assert!(
            (v.agreement_ratio - 2.0 / 3.0).abs() < 1e-6,
            "expected 2/3, got {} — the rationale is not part of the vote",
            v.agreement_ratio
        );
        // The display keeps the first seat's full line, which is what the router parses its
        // leading integer out of, so the winner still resolves to option 1.
        assert!(
            v.winning_recommendation
                .as_deref()
                .expect("winner")
                .starts_with('1'),
            "winner must still lead with its option number for the router to resolve it"
        );
        assert_eq!(v.dissent.len(), 1, "only option 2 dissents");
    }

    // The other half of the same defect: with all three seats on distinct options nothing has a
    // majority, and the tie-break must not be able to pretend otherwise. It resolves low — but
    // `consensus` is false and the ratio says 1/3, so the caller is told what it is buying.
    #[test]
    fn three_distinct_options_tie_break_low_without_claiming_consensus() {
        let votes = vec![
            vote("a", "3 — best at tests", "slow"),
            vote("b", "10 — best at docs", "verbose"),
            vote("c", "2 — best at refactors", "cost"),
        ];
        let v = synthesize("t10", &votes, 3);
        assert!(!v.consensus, "1-1-1 is not consensus");
        assert!((v.agreement_ratio - 1.0 / 3.0).abs() < 1e-6);
        // 2 < 3 < 10 numerically. Keying on the raw text would have ranked "10" first, since it
        // sorts before "2" as a string — which is why the key is padded.
        assert!(
            v.winning_recommendation
                .as_deref()
                .expect("winner")
                .starts_with('2'),
            "tie-break is the lowest option NUMBER, not the lowest digit character"
        );
    }

    // Un-numbered recommendations are a different kind of council — no option table to index, so
    // the prose is the whole identity. Pinned because the fix must not have widened its reach:
    // these two are still two votes, not one.
    #[test]
    fn free_text_recommendations_still_key_on_their_prose() {
        let votes = vec![
            vote("a", "adopt JWT", "revocation"),
            vote("b", "adopt sessions", "sticky routing"),
        ];
        let v = synthesize("t11", &votes, 2);
        assert!(!v.consensus, "two different prose picks are still a split");
        assert!((v.agreement_ratio - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn option_zero_is_not_a_numbered_vote() {
        // Options are 1-indexed and the router accepts only `n >= 1`, so "0" names nothing it can
        // act on. Keying it numerically would let two seats "agree" on option zero here and then
        // degrade at routing — consensus reported on a choice no seat can be assigned.
        assert_eq!(rec_key("0 — none of these"), rec_key("0 — none of these"));
        assert_ne!(
            rec_key("0 — none of these"),
            rec_key("0 — reject them all"),
            "zero falls back to prose keying, so different zero-votes stay distinct"
        );
        assert_eq!(
            rec_key("1 — first"),
            rec_key("1 — worded differently"),
            "a real option number still keys on the number"
        );
    }
}
