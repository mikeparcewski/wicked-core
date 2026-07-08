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

/// Build the matrix (layer b) from raw votes (layer a).
pub fn build_matrix(votes: &[Vote]) -> Matrix {
    let mut rec: BTreeMap<String, (String, u32)> = BTreeMap::new();
    let mut risk: BTreeMap<String, (String, u32)> = BTreeMap::new();

    for v in votes {
        let rk = norm(&v.recommendation);
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

/// Collapse a `key -> (display, count)` map into a `(display, count)` list sorted by count
/// desc, then display asc (deterministic tie-break).
fn sort_counts(map: BTreeMap<String, (String, u32)>) -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = map.into_values().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

/// Synthesize the [`Verdict`] (layer c) from votes.
///
/// - Winner = the recommendation with the most votes (deterministic tie-break by name).
/// - `agreement_ratio` = winning count / total votes.
/// - Consensus = a **strict majority** (winner count * 2 > total). Counts agreement, never
///   confidence.
/// - `risk_convergence` = risks cited by ≥1 CLI, most-cited first (the high-signal axis).
/// - `dissent` = non-winning recommendations.
pub fn synthesize(task_id: &str, votes: &[Vote]) -> Verdict {
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

    // Strict majority of the cast votes converge on the winner.
    let consensus = total > 0 && winning_count * 2 > total;

    let dissent: Vec<String> = matrix
        .recommendation_counts
        .iter()
        .skip(1)
        .map(|(rec, _)| rec.clone())
        .collect();

    let kind = match &winning_recommendation {
        Some(rec) if consensus => format!("Consensus: {rec} ({winning_count}/{total})"),
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
        let v = synthesize("t1", &votes);
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
        let v = synthesize("t2", &votes);
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
        let v = synthesize("t3", &votes);
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
    fn case_insensitive_recommendations_converge() {
        let votes = vec![
            vote("a", "JWT", "revocation"),
            vote("b", "jwt", "revocation"),
        ];
        let v = synthesize("t4", &votes);
        assert!(v.consensus);
        assert_eq!(v.agreement_ratio, 1.0);
    }
}
