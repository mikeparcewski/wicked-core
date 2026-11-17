//! (crew#972) Whether a launch's seats can cover an AUTHOR plus an independent JUDGE of its work,
//! asked before the launch so the launcher can say so up front instead of the run holding at a
//! `judge_unavailable` gate mid-run.
//!
//! Not a second rule: the answer is the runtime's own. A work seat's judges are the seats the
//! judge search walks for its work ([`crate::validator::distinct_judge_keys`]: usable, eligible for
//! work, a different CLI identity), over the roster minus the bench the run would start with
//! ([`crate::distribute::launcher_benched`] and [`crate::distribute::launch_benched`]; a judge is an
//! ungoverned call, so a governed-only bench keeps its seat as a judge, as at the gate), minus the
//! seats KNOWN by observation to call a provider the author was seen calling
//! ([`crate::observed_provider::shared_with`], core#1547: such a seat's approval is not independent,
//! and the search passes over it). A seat never observed is never called shared: nothing is
//! skipped on a guess.
//!
//! Not predicted here (named on the launch's own notices): a pinned gate's exclusion of the
//! validator's author and its re-admission (core#1548, decided on the observed launch at the gate),
//! the team monitors a step's gate drops (core#1165), and the authors a run's plan picks.

use serde::Serialize;

use crate::domain::BenchedSeat;
use crate::AgenticCli;

/// Why a seat of the roster is not a judge of one author's work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotJudge {
    pub seat: String,
    /// `same_identity` (it runs the author's CLI), `shared_provider` (its latest observed launch
    /// called a provider the author's did), `benched` (the run starts with it benched for every
    /// call) or `not_eligible` (no invocation, or not eligible for work).
    pub reason: String,
    /// The shared provider, for `shared_provider`; the bench's words, for `benched`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One seat that can author governed work, and who could judge it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorCoverage {
    pub seat: String,
    /// The judges in the order the runtime search tries them.
    pub judges: Vec<String>,
    /// Every other roster seat, with why it does not judge this author's work.
    pub not_judges: Vec<NotJudge>,
}

/// [`judge_coverage`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JudgeCoverage {
    /// Some work seat's work has a judge.
    pub covered: bool,
    /// Every work seat runs one CLI identity (the one-CLI quickstart).
    pub single_identity: bool,
    /// The seats that can author governed work, in roster order.
    pub authors: Vec<AuthorCoverage>,
    /// The seats the run would start with benched, with the bench's words.
    pub benched: Vec<NotJudge>,
}

/// The bench the run would start with: the launcher's word, then the engine's host-floor and
/// worktree benches, assembled as the launch does (`actor.rs`, the `NoEligibleSeat` check).
fn launch_bench(clis: &[AgenticCli], bound: bool) -> Vec<BenchedSeat> {
    let mut bench: Vec<BenchedSeat> = crate::distribute::launcher_benched(clis);
    for seat in crate::distribute::launch_benched(clis, bound, true) {
        crate::domain::bench_seat(&mut bench, seat);
    }
    bench
}

/// `clis` minus the `bench`ed seats: the roster the judge search walks.
fn without(clis: &[AgenticCli], bench: &[BenchedSeat]) -> Vec<AgenticCli> {
    clis.iter()
        .filter(|c| !bench.iter().any(|b| b.cli == c.key))
        .cloned()
        .collect()
}

/// See the module doc. `bound`: the run is bound to a repository (the launch's worktree bench).
pub fn judge_coverage(clis: &[AgenticCli], bound: bool) -> JudgeCoverage {
    // The launch's bench, assembled as the launch does: the launcher's word, then the engine's
    // host-floor and worktree benches (a consumer's question is about a file-backed daemon, whose
    // governed units are armed: `contained`, as `worktree_benched` answers).
    let bench = launch_bench(clis, bound);
    let benched_any = |k: &str| bench.iter().any(|b| b.cli == k);
    let judge_bench = crate::domain::ungoverned_bench(&bench);
    let judge_roster = without(clis, &judge_bench);
    let can_work =
        |c: &AgenticCli| c.seat_eligible_for_work && !c.headless_invocation.trim().is_empty();

    let authors: Vec<AuthorCoverage> = clis
        .iter()
        .filter(|c| can_work(c) && !benched_any(&c.key))
        .map(|author| {
            let avoid = crate::observed_provider::last_seen(&author.key).unwrap_or_default();
            let mut judges = Vec::new();
            let mut not_judges = Vec::new();
            let distinct =
                crate::validator::distinct_judge_keys(&[author.key.as_str()], &judge_roster);
            for k in distinct {
                match crate::observed_provider::shared_with(&k, &avoid) {
                    Some(p) => not_judges.push(NotJudge {
                        seat: k,
                        reason: "shared_provider".into(),
                        detail: Some(p),
                    }),
                    None => judges.push(k),
                }
            }
            for c in clis.iter().filter(|c| c.key != author.key) {
                if judges.contains(&c.key) || not_judges.iter().any(|n| n.seat == c.key) {
                    continue;
                }
                let (reason, detail) = if let Some(b) = judge_bench.iter().find(|b| b.cli == c.key)
                {
                    ("benched", Some(b.reason.clone()))
                } else if !can_work(c) {
                    ("not_eligible", None)
                } else {
                    ("same_identity", None)
                };
                not_judges.push(NotJudge {
                    seat: c.key.clone(),
                    reason: reason.into(),
                    detail,
                });
            }
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
        covered: authors.iter().any(|a| !a.judges.is_empty()),
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
    use crate::observed_provider::{record_receipt, take_receipts, JudgingWork, Observed};
    use crate::workflow::{StepInput, StepOutput, StepRunner, StepStatus};

    const ANTHROPIC: &str = "api.anthropic.com:443";
    const OPENAI: &str = "chatgpt.com:443";

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

    /// A judge that approves everything and is observed calling its seat's provider, as the
    /// container floor's gateway records it (`None`: unobserved, the host floor).
    struct Approves(Vec<(String, Option<&'static str>)>);
    impl StepRunner for Approves {
        fn run_unit(&self, input: &StepInput) -> StepOutput {
            let seat = input.unit.assigned_cli.clone().unwrap_or_default();
            let provider = self
                .0
                .iter()
                .find(|(k, _)| *k == seat)
                .and_then(|(_, p)| *p);
            record_receipt(&seat, provider.map(|p| vec![p.to_string()]));
            StepOutput {
                run_id: input.run_id.clone(),
                unit_ix: input.unit_ix,
                attempt: input.attempt,
                output: "PASS\nchecked\nPASS".into(),
                status: StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }
    }

    /// The roster and its observed providers. Keys are this module's own (`jc<n>-…`): the
    /// observation record is process-wide and other tests write `claude`, `codex` and `pi`.
    fn roster(
        tag: &str,
        seats: &[(&str, &str, Option<&'static str>)],
    ) -> (Vec<AgenticCli>, Approves) {
        let mut clis = Vec::new();
        let mut providers = Vec::new();
        for (name, binary, provider) in seats {
            let key = format!("{tag}-{name}");
            clis.push(seat(&key, &format!("{binary} -p {{PROMPT}}")));
            if let Some(p) = provider {
                record_receipt(&key, Some(vec![p.to_string()]));
            }
            providers.push((key, *provider));
        }
        let _ = take_receipts();
        (clis, Approves(providers))
    }

    /// SAYS == DOES: for every work seat, what the launch says (its judges) is what the runtime's
    /// judge search does with that seat's work — the first judge named is the seat whose approval
    /// stands, and no judge named means the search finds no independent judge.
    fn assert_agrees(clis: &[AgenticCli], runner: &Approves) -> JudgeCoverage {
        let cov = judge_coverage(clis, false);
        // The roster the gate's search walks: the run's, minus what the launch benched for every call.
        let clis = &without(
            clis,
            &crate::domain::ungoverned_bench(&launch_bench(clis, false)),
        );
        for a in &cov.authors {
            let work = Observed {
                providers: crate::observed_provider::last_seen(&a.seat).unwrap_or_default(),
                complete: true,
            };
            let _judging = JudgingWork::of(&work);
            let runtime_has = crate::validator::distinct_judge_available(&[a.seat.as_str()], clis);
            let (verdict, _) = crate::validator::agent_validate_with_refusals(
                "the work is correct",
                "work",
                &[a.seat.as_str()],
                clis,
                runner,
            );
            let _ = take_receipts();
            match a.judges.first() {
                Some(first) => {
                    let v = verdict.unwrap_or_else(|e| {
                        panic!("{}: launch names {first}, runtime: {e}", a.seat)
                    });
                    assert!(v.pass);
                    assert_eq!(
                        v.judge_cli.as_deref(),
                        Some(first.as_str()),
                        "{}'s work",
                        a.seat
                    );
                    assert_eq!(v.judge_distinct, Some(true));
                }
                None => assert!(
                    !runtime_has || verdict.is_err(),
                    "{}: launch says no judge, runtime judged on {:?}",
                    a.seat,
                    verdict.ok().and_then(|v| v.judge_cli)
                ),
            }
        }
        cov
    }

    fn judges_of<'a>(cov: &'a JudgeCoverage, seat: &str) -> &'a [String] {
        &cov.authors.iter().find(|a| a.seat == seat).unwrap().judges
    }

    #[test]
    fn claude_and_codex_cover_each_other() {
        let (clis, r) = roster(
            "jc1",
            &[
                ("claude", "claude", Some(ANTHROPIC)),
                ("codex", "codex", Some(OPENAI)),
            ],
        );
        let cov = assert_agrees(&clis, &r);
        assert!(cov.covered);
        assert!(!cov.single_identity);
        assert_eq!(judges_of(&cov, "jc1-claude"), ["jc1-codex"]);
        assert_eq!(judges_of(&cov, "jc1-codex"), ["jc1-claude"]);
    }

    #[test]
    fn claude_alone_is_not_covered() {
        let (clis, r) = roster("jc2", &[("claude", "claude", Some(ANTHROPIC))]);
        let cov = assert_agrees(&clis, &r);
        assert!(!cov.covered);
        assert!(cov.single_identity);
        assert!(judges_of(&cov, "jc2-claude").is_empty());
    }

    #[test]
    fn claude_pi_codex_in_both_orders_with_pi_seen_on_claudes_provider() {
        for (tag, order) in [
            (
                "jc3",
                [("claude", "claude"), ("pi", "pi"), ("codex", "codex")],
            ),
            (
                "jc4",
                [("codex", "codex"), ("pi", "pi"), ("claude", "claude")],
            ),
        ] {
            let seats: Vec<(&str, &str, Option<&'static str>)> = order
                .iter()
                .map(|(n, b)| (*n, *b, Some(if *n == "codex" { OPENAI } else { ANTHROPIC })))
                .collect();
            let (clis, r) = roster(tag, &seats);
            let cov = assert_agrees(&clis, &r);
            assert!(cov.covered, "{tag}");
            let k = |n: &str| format!("{tag}-{n}");
            // Claude's work: pi was seen on Anthropic too, so only codex judges it.
            assert_eq!(judges_of(&cov, &k("claude")), [k("codex")], "{tag}");
            let pi = cov.authors.iter().find(|a| a.seat == k("claude")).unwrap();
            assert!(pi.not_judges.iter().any(|n| n.seat == k("pi")
                && n.reason == "shared_provider"
                && n.detail.as_deref() == Some(ANTHROPIC)));
            assert_eq!(judges_of(&cov, &k("pi")), [k("codex")], "{tag}");
            assert_eq!(judges_of(&cov, &k("codex")).len(), 2, "{tag}");
        }
    }

    #[test]
    fn an_unobserved_seat_is_never_called_shared() {
        let (clis, r) = roster(
            "jc5",
            &[("claude", "claude", Some(ANTHROPIC)), ("agy", "agy", None)],
        );
        let cov = assert_agrees(&clis, &r);
        assert!(cov.covered);
        assert_eq!(judges_of(&cov, "jc5-claude"), ["jc5-agy"]);
    }

    #[test]
    fn two_seats_seen_on_one_provider_are_not_covered() {
        let (clis, r) = roster(
            "jc6",
            &[
                ("claude", "claude", Some(ANTHROPIC)),
                ("pi", "pi", Some(ANTHROPIC)),
            ],
        );
        let cov = assert_agrees(&clis, &r);
        assert!(!cov.covered);
        assert!(!cov.single_identity);
    }

    #[test]
    fn two_keys_running_one_binary_are_one_identity() {
        let (clis, r) = roster(
            "jc7",
            &[("claude", "claude", None), ("sonnet", "claude", None)],
        );
        let cov = assert_agrees(&clis, &r);
        assert!(!cov.covered);
        assert!(cov.single_identity);
        let a = &cov.authors[0];
        assert_eq!(a.not_judges[0].reason, "same_identity");
    }

    #[test]
    fn a_launcher_benched_or_ballot_only_seat_neither_authors_nor_judges() {
        let (mut clis, r) = roster(
            "jc8",
            &[
                ("claude", "claude", None),
                ("codex", "codex", None),
                ("pi", "pi", None),
            ],
        );
        clis[1].health = Some(wicked_council::types::SeatHealth::unusable("signed out"));
        clis[2].seat_eligible_for_work = false;
        let cov = assert_agrees(&clis, &r);
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
