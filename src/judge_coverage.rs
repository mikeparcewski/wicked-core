//! (crew#972) Whether a launch's seats can cover an AUTHOR plus an independent JUDGE of its work,
//! asked before the launch so the launcher can say so up front instead of the run holding at a
//! judge gate mid-run.
//!
//! Not a second rule: the answer walks the runtime's own pool with the runtime's own helpers. For
//! each seat that can author governed work, its judges are the seats the DEFAULT judge rotation of a
//! gate on that seat's work walks (`cli_runner`'s `eligible_run`, then `agent_validate_in`'s order):
//! the registry records of the run's seats ([`crate::cli_runner::judge_pool`], registry order),
//! minus a record the registry marks unusable, minus the bench the run starts with
//! ([`crate::distribute::launcher_benched`] + [`crate::distribute::launch_benched`], assembled as
//! `actor.rs`'s launch check does; the ungoverned bench, as a judge is an ungoverned call), minus
//! seats sharing the author's binary ([`crate::cli_runner::without_author_binaries`]), walked by
//! [`crate::validator::distinct_judge_keys`]; the seats KNOWN by observation to call a provider the
//! author was seen calling go last ([`crate::observed_provider::shared_with`], the rule
//! `known_to_share` sorts the rotation by). Such a seat is still a judge, flagged: its approval
//! stands only if its own launch at the gate is not seen calling that provider again.
//!
//! `covered`: some author has a judge not flagged. What this does not predict, named so a launcher
//! never says more: the fallback ladder's judge-only seats outside the run (operator ruling
//! 2026-10-14), a pinned gate's exclusion of the validator's author and its re-admission on an
//! observed launch (core#1548), team monitors (core#1165), and which seats a plan makes authors.

use serde::Serialize;

use crate::domain::BenchedSeat;
use crate::AgenticCli;

/// A seat that judges one author's work, in rotation order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Judge {
    pub seat: String,
    /// Its latest observed launch called this provider, which the author's did too: tried last,
    /// and its approval stands only if the gate's launch is not seen calling it again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seen_sharing: Option<String>,
}

/// Why a seat of the run does not judge one author's work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotJudge {
    pub seat: String,
    /// `same_identity` (it runs the author's CLI), `benched` (the run starts with it benched for
    /// every call) or `not_eligible` (no registry record, no invocation, not eligible for work, or
    /// marked unusable).
    pub reason: String,
    /// The bench's words, for `benched`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One seat that can author governed work, and who could judge it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorCoverage {
    pub seat: String,
    /// The judges in the order the runtime rotation tries them.
    pub judges: Vec<Judge>,
    /// Every other seat of the run, with why it does not judge this author's work.
    pub not_judges: Vec<NotJudge>,
}

/// [`judge_coverage`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JudgeCoverage {
    /// Some work seat's work has a judge that is not [seen sharing](Judge::seen_sharing) its provider.
    pub covered: bool,
    /// Every work seat runs one CLI identity (the one-CLI quickstart).
    pub single_identity: bool,
    /// The seats that can author governed work, in launch order.
    pub authors: Vec<AuthorCoverage>,
    /// The seats the run would start with benched, with the bench's words.
    pub benched: Vec<NotJudge>,
}

/// See the module doc. `clis`: the launch roster (as `launchRun`'s `clisJson`); `bound`: the run is
/// bound to a repository (the launch's worktree bench). The judges' records are this process's
/// registry, as at the gate.
pub fn judge_coverage(clis: &[AgenticCli], bound: bool) -> JudgeCoverage {
    judge_coverage_in(clis, &crate::registry_roster(), bound)
}

/// The bench the run would start with: the launcher's word, then the engine's host-floor and
/// worktree benches, assembled as the launch does (`actor.rs`, the `NoEligibleSeat` check; a
/// consumer's question is about a file-backed daemon, whose governed units are armed).
pub(crate) fn launch_bench(clis: &[AgenticCli], bound: bool) -> Vec<BenchedSeat> {
    let mut bench: Vec<BenchedSeat> = crate::distribute::launcher_benched(clis);
    for seat in crate::distribute::launch_benched(clis, bound, true) {
        crate::domain::bench_seat(&mut bench, seat);
    }
    bench
}

/// [`judge_coverage`] over an explicit `registry` (the gate's `registry_roster()`).
pub(crate) fn judge_coverage_in(
    clis: &[AgenticCli],
    registry: &[AgenticCli],
    bound: bool,
) -> JudgeCoverage {
    let bench = launch_bench(clis, bound);
    let judge_bench = crate::domain::ungoverned_bench(&bench);
    let run_roster: Vec<String> = clis.iter().map(|c| c.key.clone()).collect();
    // The default judge's pool, as the gate builds it (`eligible_run`).
    let pool = crate::cli_runner::judge_pool(registry, &run_roster);
    let run_has_registry_seat = registry
        .iter()
        .any(|c| crate::cli_runner::in_run_roster(&run_roster, &c.key));
    let eligible_run: Vec<AgenticCli> = pool
        .iter()
        .filter(|c| c.health.as_ref().is_none_or(|h| h.usable))
        .filter(|c| !judge_bench.iter().any(|b| b.cli == c.key))
        .filter(|c| !run_has_registry_seat || crate::cli_runner::in_run_roster(&run_roster, &c.key))
        .map(|c| (*c).clone())
        .collect();
    let benched_any = |k: &str| bench.iter().any(|b| b.cli == k);
    let can_work =
        |c: &AgenticCli| c.seat_eligible_for_work && !c.headless_invocation.trim().is_empty();

    let authors: Vec<AuthorCoverage> = clis
        .iter()
        .filter(|c| can_work(c) && !benched_any(&c.key))
        .map(|author| {
            let ex = [author.key.as_str()];
            let ids = crate::cli_runner::author_identities(&ex, registry);
            let roster = crate::cli_runner::without_author_binaries(&eligible_run, &ex, &ids);
            let avoid = crate::observed_provider::last_seen(&author.key).unwrap_or_default();
            let mut judges: Vec<Judge> = crate::validator::distinct_judge_keys(&ex, &roster)
                .into_iter()
                .map(|seat| Judge {
                    seen_sharing: crate::observed_provider::shared_with(&seat, &avoid),
                    seat,
                })
                .collect();
            // The rotation's own order: a stable sort, seats known to share last.
            judges.sort_by_key(|j| j.seen_sharing.is_some());
            let not_judges = clis
                .iter()
                .filter(|c| c.key != author.key && !judges.iter().any(|j| j.seat == c.key))
                .map(|c| {
                    let (reason, detail) =
                        if let Some(b) = judge_bench.iter().find(|b| b.cli == c.key) {
                            ("benched", Some(b.reason.clone()))
                        } else if !eligible_run.iter().any(|e| e.key == c.key) || !can_work(c) {
                            ("not_eligible", None)
                        } else {
                            ("same_identity", None)
                        };
                    NotJudge {
                        seat: c.key.clone(),
                        reason: reason.into(),
                        detail,
                    }
                })
                .collect();
            AuthorCoverage {
                seat: author.key.clone(),
                judges,
                not_judges,
            }
        })
        .collect();

    let identities: std::collections::HashSet<String> = clis
        .iter()
        .filter(|c| authors.iter().any(|a| a.seat == c.key))
        .map(crate::validator::seat_identity)
        .collect();
    JudgeCoverage {
        covered: authors
            .iter()
            .any(|a| a.judges.iter().any(|j| j.seen_sharing.is_none())),
        single_identity: identities.len() == 1,
        benched: bench
            .iter()
            .map(|b| NotJudge {
                seat: b.cli.clone(),
                reason: "benched".into(),
                detail: Some(b.reason.clone()),
            })
            .collect(),
        authors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observed_provider::{record_receipt, take_receipts};

    fn seat(key: &str, invocation: &str) -> AgenticCli {
        use wicked_council::{Category, Confidence, InputMode};
        AgenticCli {
            key: key.into(),
            display_name: key.into(),
            binary: "unused".into(),
            headless_invocation: invocation.into(),
            category: Category::default(),
            input_mode: InputMode::default(),
            version_probe: vec![],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::default(),
            enabled_for_council: true,
            seat_eligible_for_work: true,
            acp: None,
            capabilities: None,
            login_invocation: None,
            logout_invocation: None,
            governance_class: None,
            credential: None,
            free_tier: None,
            health: None,
        }
    }

    /// Runs `f` with the container floor pinned (the benches read the floor; other tests change it).
    fn on_container_floor<T>(f: impl FnOnce() -> T) -> T {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var_os(crate::container_floor::SEAT_FLOOR_ENV);
        std::env::set_var(crate::container_floor::SEAT_FLOOR_ENV, "container");
        let out = f();
        match prev {
            Some(v) => std::env::set_var(crate::container_floor::SEAT_FLOOR_ENV, v),
            None => std::env::remove_var(crate::container_floor::SEAT_FLOOR_ENV),
        }
        out
    }

    #[test]
    fn a_seat_seen_on_the_authors_provider_judges_last_and_flagged() {
        let clis = [
            seat("jcu1-claude", "claude -p {PROMPT}"),
            seat("jcu1-pi", "pi -p {PROMPT}"),
            seat("jcu1-codex", "codex -p {PROMPT}"),
        ];
        record_receipt("jcu1-claude", Some(vec!["api.anthropic.com:443".into()]));
        record_receipt("jcu1-pi", Some(vec!["api.anthropic.com:443".into()]));
        record_receipt("jcu1-codex", Some(vec!["chatgpt.com:443".into()]));
        let _ = take_receipts();
        let cov = on_container_floor(|| judge_coverage_in(&clis, &clis, false));
        assert!(cov.covered);
        let claude = &cov.authors[0];
        assert_eq!(claude.judges[0].seat, "jcu1-codex");
        assert_eq!(claude.judges[1].seat, "jcu1-pi");
        assert_eq!(
            claude.judges[1].seen_sharing.as_deref(),
            Some("api.anthropic.com:443")
        );
    }

    #[test]
    fn a_launcher_benched_or_ballot_only_seat_neither_authors_nor_judges() {
        let mut clis = [
            seat("jcu2-claude", "claude -p {PROMPT}"),
            seat("jcu2-codex", "codex -p {PROMPT}"),
            seat("jcu2-pi", "pi -p {PROMPT}"),
        ];
        clis[1].health = Some(wicked_council::types::SeatHealth::unusable("signed out"));
        clis[2].seat_eligible_for_work = false;
        let registry = clis.clone();
        let cov = on_container_floor(|| judge_coverage_in(&clis, &registry, false));
        assert!(!cov.covered);
        assert_eq!(cov.authors.len(), 1);
        let reasons: Vec<&str> = cov.authors[0]
            .not_judges
            .iter()
            .map(|n| n.reason.as_str())
            .collect();
        assert_eq!(reasons, ["benched", "not_eligible"]);
        assert_eq!(cov.benched[0].detail.as_deref(), Some("signed out"));
    }
}
