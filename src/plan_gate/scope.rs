//! Seam X1 (DES-TEAMING-002 §8.2, §8.4, rev 13): the PA scopes a plan that declares no scope.
//!
//! A launch plan with a creator step and no declared `touch` (every preset, and a user plan that
//! left `touch` out) is not scored at launch. The run's plan rev 1 is ONE read-only step,
//! [`SCOPE_STEP_ID`] (`pa-scope`: the catalog `understand` entry on the PA seat; a plan that
//! authors the id is refused at launch as a duplicate step id), accepted by the engine
//! without the approval matrix: it has no creator step, so nothing it does can change the tree.
//! Its output answers in one line of a fixed grammar:
//!
//! - a run on a repo: `SCOPE {"touch":["src/auth/login.rs", …]}` — the files the plan will
//!   change, found through the repo's estate graph. The touch set is scored by the SAME scorer
//!   and blast-radius rules as a user plan's declared touch ([`super::intent_score_for_run`]); no
//!   usable graph still fails closed at 100.
//! - a run with no repo: `RISK {"score":N,"reasons":["…"]}` — the PA's judgement of the content
//!   and its audience. No graph is involved. The deterministic baseline is the lowest band
//!   (`THRESHOLDS.repo_less_baseline`) and the PA's rating can only RAISE it (S4's model rule):
//!   it rides `path.scored.model.add`, never the deterministic part.
//!
//! A missing or malformed answer fails closed: 100, reason [`PA_DECLARED_NO_SCOPE`]. At the step
//! boundary after the scope step, the launch plan (the scope step first, then its own steps) goes
//! through [`super::decide`] as the run's INITIAL plan — floor fill, compose, the approval matrix —
//! before any creator step dispatches. The diff re-score (§8.7) still ratchets from there.

use serde::{Deserialize, Serialize};

use super::{Decided, Proposal, Scored, TeamPlanState};
use crate::domain::HumanConfirm;
use crate::plan::{PlanStep, PlanSteps};
use crate::review_scale::{Assessment, ModelBonus, THRESHOLDS};
use crate::team::events::{ProposalKind, ProposalSource};
use crate::workflow::WorkflowDef;

/// The phase id of the PA's scope step (catalog `understand`).
pub(crate) const SCOPE_STEP_ID: &str = "pa-scope";

/// The first reason of a fail-closed scope score (a missing or malformed answer).
pub(crate) const PA_DECLARED_NO_SCOPE: &str = "the PA declared no scope";

/// `PlanPreview.graph` for a plan the PA will scope: the score does not exist yet.
pub(crate) const PENDING_PA_SCOPE: &str = "pending_pa_scope";

/// The line prefix of a repo run's answer.
const SCOPE_PREFIX: &str = "SCOPE";
/// The line prefix of a repo-less run's answer.
const RISK_PREFIX: &str = "RISK";

/// A launch plan the PA scopes, held on the plan state from launch until the scope step's
/// boundary applies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeHold {
    /// The launch plan as authored (a preset's steps, or the user's plan), without the scope step.
    pub plan: PlanSteps,
    /// The run has no repo: the PA rates the risk (`RISK`) instead of declaring a touch set.
    pub unbound: bool,
    /// The scope step's answer lines, recorded when its turn folds (`None` until then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<ScopeAnswer>,
}

/// The `SCOPE` / `RISK` lines of the scope step's finished turn (the rest is not kept).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeAnswer {
    pub ord: u32,
    pub attempt: u32,
    /// The seat that answered (the PA).
    pub by: String,
    pub lines: String,
}

/// The PA scores this launch plan: it has a creator step and declares no touch set.
pub(crate) fn needs_pa_scope(plan: &PlanSteps) -> bool {
    plan.has_creator() && plan.touch.as_ref().is_none_or(|t| t.is_empty())
}

/// The scope step for `plan`: catalog `understand` (read-only, the PA's by default), id
/// [`SCOPE_STEP_ID`], with the instructions that ask for the answer line.
pub(crate) fn scope_step(plan: &PlanSteps, unbound: bool) -> PlanStep {
    let steps: Vec<&str> = plan.steps.iter().map(|s| s.catalog.as_str()).collect();
    let instructions = if unbound {
        format!(
            "Rate this run's risk before any work starts (READ ONLY: change nothing). The plan: \
             {}. There is no repo: judge the content and its audience; client-facing work (a \
             customer deliverable, an RFP answer, a demo shown outside) rates higher than \
             internal work. End with exactly one line:\n\
             RISK {{\"score\":N,\"reasons\":[\"...\"]}}\n\
             N is 0-100: 0-19 internal and routine, 20-39 shared internally, 40-69 \
             client-facing, 70-100 high stakes (pauses for approval). A missing or malformed \
             RISK line scores 100.",
            steps.join(" -> ")
        )
    } else {
        format!(
            "Scope this run before anything changes (READ ONLY: edit nothing). The plan: {}. \
             Find every file it will create, edit or delete, grounding in the repo's estate code \
             graph through the wicked-garden search skill (blast radius, lineage). End with \
             exactly one line:\n\
             SCOPE {{\"touch\":[\"repo/relative/path\", ...]}}\n\
             The engine scores the run's risk from that list against the code graph. A missing \
             or malformed SCOPE line scores 100 (high risk).",
            steps.join(" -> ")
        )
    };
    PlanStep {
        catalog: "understand".into(),
        id: SCOPE_STEP_ID.into(),
        instructions: Some(instructions),
        ..PlanStep::default()
    }
}

/// `plan` as the launch runs it: the scope step first when the PA scopes it, else unchanged.
pub(crate) fn with_scope_step(plan: &PlanSteps, unbound: bool) -> PlanSteps {
    if !needs_pa_scope(plan) {
        return plan.clone();
    }
    let mut out = plan.clone();
    out.steps.insert(0, scope_step(plan, unbound));
    out
}

/// The run's plan rev 1 for a launch the PA scopes: the scope step alone, accepted by the engine
/// (no creator step, so no floor and never high risk; the approval matrix judges the scoped plan
/// at the step boundary). `prior` carries the roster and the deliver step; the launch plan waits
/// on the state ([`TeamPlanState::scope`]).
pub(crate) fn scope_rev(
    run_id: &str,
    plan: &PlanSteps,
    preset: Option<String>,
    unbound: bool,
    prior: TeamPlanState,
    human_confirm: &HumanConfirm,
) -> anyhow::Result<(TeamPlanState, WorkflowDef)> {
    let steps = PlanSteps {
        steps: vec![scope_step(plan, unbound)],
        touch: None,
        floor_override: None,
    };
    let mut def = crate::plan::compose(crate::catalog::catalog(), &steps)
        .map_err(|r| anyhow::anyhow!("the scope step does not compose: {r}"))?;
    def.id = super::per_run_def_id(run_id, 1);
    let floor = crate::review_scale::floor_for(0, false);
    let mut state = prior;
    state.rev = 1;
    state.accepted_rev = 1;
    state.accepted_high_risk = false;
    state.preset = preset;
    state.accepted = Some(super::AcceptedPlan {
        rev: 1,
        by: "engine".into(),
        band: floor.band,
        high_risk: false,
        auto: super::is_auto(human_confirm),
        steps,
        floor_override: None,
        proposal_id: String::new(),
    });
    state.scope = Some(ScopeHold {
        plan: plan.clone(),
        unbound,
        answer: None,
    });
    Ok((state, def))
}

/// The `SCOPE` / `RISK` lines of a step output.
pub(crate) fn scope_lines_of(output: &str) -> String {
    output
        .lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with(SCOPE_PREFIX) || l.starts_with(RISK_PREFIX))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the PA declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Declared {
    /// A repo run: the files the plan will change (deduplicated, in order).
    Touch(Vec<String>),
    /// A repo-less run: the PA's rating and why.
    Risk { score: u8, reasons: Vec<String> },
}

/// Parse the PA's answer: every line of the run's kind (`SCOPE` on a repo run, `RISK` on a
/// repo-less one) must parse; several are merged so the answer can only get riskier (the union of
/// the touch sets, the highest rating). `Err` names why there is no usable answer.
pub(crate) fn parse_answer(lines: &str, unbound: bool) -> Result<Declared, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ScopeLine {
        touch: Vec<String>,
        #[serde(default)]
        #[allow(dead_code)]
        reasons: Vec<String>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RiskLine {
        score: u8,
        reasons: Vec<String>,
    }
    let prefix = if unbound { RISK_PREFIX } else { SCOPE_PREFIX };
    let mut touch: Vec<String> = Vec::new();
    let mut risk: Option<(u8, Vec<String>)> = None;
    let mut seen = 0usize;
    for line in lines.lines().map(str::trim) {
        let Some(json) = line.strip_prefix(prefix) else {
            continue;
        };
        // `SCOPE {…}` / `SCOPE{…}`, never a longer word (`SCOPED …`).
        if !(json.starts_with(char::is_whitespace) || json.starts_with('{')) {
            continue;
        }
        seen += 1;
        let json = json.trim();
        if unbound {
            let r: RiskLine = serde_json::from_str(json)
                .map_err(|e| format!("a malformed {prefix} line: {e}"))?;
            if r.score > 100 {
                return Err(format!("a {prefix} score above 100: {}", r.score));
            }
            let reasons: Vec<String> = r
                .reasons
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if reasons.is_empty() {
                return Err(format!("a {prefix} line with no reasons"));
            }
            risk = Some(match risk.take() {
                Some((s, mut rs)) => {
                    rs.extend(reasons);
                    (s.max(r.score), rs)
                }
                None => (r.score, reasons),
            });
        } else {
            let s: ScopeLine = serde_json::from_str(json)
                .map_err(|e| format!("a malformed {prefix} line: {e}"))?;
            if s.touch.is_empty() {
                return Err(format!("a {prefix} line with an empty touch set"));
            }
            for p in s.touch {
                let p = p.trim();
                let p = p.strip_prefix("./").unwrap_or(p);
                if p.is_empty()
                    || p.starts_with('/')
                    || p.contains('\\')
                    || p.split('/').any(|c| c == "..")
                    || p.chars().nth(1) == Some(':')
                {
                    return Err(format!("a {prefix} path that is not repo-relative: `{p}`"));
                }
                if !touch.iter().any(|t| t == p) {
                    touch.push(p.to_string());
                }
            }
        }
    }
    if seen == 0 {
        return Err(format!("no {prefix} line in the scope step's answer"));
    }
    Ok(match risk {
        Some((score, reasons)) => Declared::Risk { score, reasons },
        None => Declared::Touch(touch),
    })
}

/// The fail-closed score of a missing or malformed answer.
fn no_scope(detail: &str) -> Scored {
    let t = &THRESHOLDS;
    Scored {
        assessment: Assessment {
            deterministic: t.no_graph_score,
            score: t.no_graph_score,
            reasons: vec![PA_DECLARED_NO_SCOPE.to_string(), detail.to_string()],
            model: None,
            signals: None,
            plan: crate::review_scale::plan_for(t.no_graph_score),
        },
        destructive: false,
    }
}

/// A repo-less run's score: the deterministic `baseline`, raised (never lowered) by the PA's
/// rating, which rides the model part (`path.scored.model.add`).
pub(crate) fn judged(baseline: u8, rating: u8, reasons: &[String]) -> Scored {
    let add = rating.saturating_sub(baseline);
    let score = baseline.saturating_add(add).min(100);
    let mut all = vec![format!(
        "no repo: the deterministic baseline is {baseline}, the lowest band; no graph is read"
    )];
    let model = (add > 0).then(|| {
        let rationale = reasons.join("; ");
        all.push(format!("PA judgement +{add}: {rationale}"));
        ModelBonus { add, rationale }
    });
    Scored {
        assessment: Assessment {
            deterministic: baseline,
            score,
            reasons: all,
            model,
            signals: None,
            plan: crate::review_scale::plan_for(score),
        },
        destructive: false,
    }
}

/// Score the held plan from the PA's answer, and the touch set it declared (a repo run).
pub(crate) fn scope_score(
    hold: &ScopeHold,
    repo_root: Option<&std::path::Path>,
    base_commit: Option<&str>,
) -> (Scored, Option<Vec<String>>) {
    let Some(answer) = hold.answer.as_ref() else {
        return (no_scope("the scope step recorded no answer"), None);
    };
    match parse_answer(&answer.lines, hold.unbound) {
        Err(why) => (no_scope(&why), None),
        Ok(Declared::Risk { score, reasons }) => {
            (judged(THRESHOLDS.repo_less_baseline, score, &reasons), None)
        }
        Ok(Declared::Touch(touch)) => {
            let plan = PlanSteps {
                steps: hold.plan.steps.clone(),
                touch: Some(touch.clone()),
                floor_override: None,
            };
            (
                super::intent_score_for_run(&plan, repo_root, base_commit),
                Some(touch),
            )
        }
    }
}

/// The score a preview shows for a plan the PA will scope: nothing is known yet, so the floor is
/// the baseline's and the reason says the score is pending (never 100 as if final).
pub(crate) fn pending_scored(unbound: bool) -> Scored {
    let what = if unbound {
        "its RISK rating (no repo)"
    } else {
        "its SCOPE touch set, scored against the repo's code graph"
    };
    let baseline = THRESHOLDS.repo_less_baseline;
    Scored {
        assessment: Assessment {
            deterministic: baseline,
            score: baseline,
            reasons: vec![format!(
                "pending the PA's scope: the run's first step ({SCOPE_STEP_ID}, read-only) \
                 declares {what}; the score, band and approval are computed from it before any \
                 creator step"
            )],
            model: None,
            signals: None,
            plan: crate::review_scale::plan_for(baseline),
        },
        destructive: false,
    }
}

/// The scoped plan (the scope rev's steps, then the launch plan's) decided as the run's INITIAL
/// plan at the scope step's boundary, through [`super::decide`]: proposed by the PA seat that
/// answered (source: its understand turn), scored from its answer. `prior` is the run's state
/// with the hold still on it; the decided state drops it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decide_scoped(
    run_id: &str,
    prior: &TeamPlanState,
    pa_seat: &str,
    scope_ord: u32,
    scope_attempt: u32,
    repo_root: Option<&std::path::Path>,
    base_commit: Option<&str>,
    human_confirm: &HumanConfirm,
    now: i64,
) -> anyhow::Result<Decided> {
    let hold = prior
        .scope
        .clone()
        .ok_or_else(|| anyhow::anyhow!("run {run_id} holds no plan for its PA to scope"))?;
    let accepted = prior
        .accepted
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("run {run_id} has no scope rev"))?;
    let (scored, touch) = scope_score(&hold, repo_root, base_commit);
    let (by, ord, attempt) = match &hold.answer {
        Some(a) => (a.by.clone(), a.ord, a.attempt),
        None => (pa_seat.to_string(), scope_ord, scope_attempt),
    };
    let mut steps = accepted.steps.steps.clone();
    steps.extend(hold.plan.steps.iter().cloned());
    let plan = PlanSteps {
        steps,
        touch,
        floor_override: hold.plan.floor_override.clone(),
    };
    let mut base = prior.clone();
    base.scope = None;
    super::decide(
        run_id,
        Proposal {
            by,
            source: ProposalSource::Understand { ord, attempt },
            kind: ProposalKind::Initial,
            preset: base.preset.clone(),
            plan,
            reviewing_ord: Some(ord),
            approved_by_human: false,
        },
        &base,
        human_confirm,
        &scored,
        now,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan(v: serde_json::Value) -> PlanSteps {
        serde_json::from_value(v).unwrap()
    }

    /// Which launch plans the PA scopes: a creator step and no declared touch set.
    #[test]
    fn a_creator_plan_without_a_touch_set_is_scoped_by_the_pa() {
        assert!(needs_pa_scope(&plan(
            json!({"steps": [{"catalog": "build"}]})
        )));
        assert!(needs_pa_scope(&plan(
            json!({"steps": [{"catalog": "produce"}], "touch": []})
        )));
        assert!(!needs_pa_scope(&plan(
            json!({"steps": [{"catalog": "build"}], "touch": ["src/a.rs"]})
        )));
        assert!(!needs_pa_scope(&plan(
            json!({"steps": [{"catalog": "understand"}]})
        )));
        let p = with_scope_step(&plan(json!({"steps": [{"catalog": "build"}]})), false);
        assert_eq!(
            p.steps
                .iter()
                .map(|s| (s.catalog.as_str(), s.id.as_str()))
                .collect::<Vec<_>>(),
            [("understand", "pa-scope"), ("build", "")]
        );
        assert!(p.steps[0]
            .instructions
            .as_deref()
            .unwrap()
            .contains("SCOPE {"));
        let u = with_scope_step(&plan(json!({"steps": [{"catalog": "produce"}]})), true);
        assert!(u.steps[0]
            .instructions
            .as_deref()
            .unwrap()
            .contains("RISK {"));
    }

    /// The grammar: a repo run's `SCOPE`, a repo-less run's `RISK`; merged so it only gets
    /// riskier; anything malformed is no answer.
    #[test]
    fn the_scope_grammar() {
        let out = "prose\n  SCOPE {\"touch\":[\"./docs/a.md\",\"src/b.rs\"]}\nSCOPE {\"touch\":[\"src/b.rs\",\"src/c.rs\"]}\nRISK {\"score\":5,\"reasons\":[\"x\"]}\n";
        let lines = scope_lines_of(out);
        assert_eq!(lines.lines().count(), 3);
        assert_eq!(
            parse_answer(&lines, false),
            Ok(Declared::Touch(vec![
                "docs/a.md".into(),
                "src/b.rs".into(),
                "src/c.rs".into()
            ]))
        );
        assert_eq!(
            parse_answer(&lines, true),
            Ok(Declared::Risk {
                score: 5,
                reasons: vec!["x".into()]
            })
        );
        let risk =
            "RISK {\"score\":20,\"reasons\":[\"a\"]}\nRISK {\"score\":45,\"reasons\":[\"b\"]}";
        assert_eq!(
            parse_answer(risk, true),
            Ok(Declared::Risk {
                score: 45,
                reasons: vec!["a".into(), "b".into()]
            })
        );
        for bad in [
            "",
            "SCOPED {\"touch\":[\"a\"]}",
            "SCOPE {\"touch\":[]}",
            "SCOPE {\"touch\":[\"/etc/passwd\"]}",
            "SCOPE {\"touch\":[\"../x.rs\"]}",
            "SCOPE {\"touch\":[\"a.rs\"], \"extra\": 1}",
            "SCOPE touch: a.rs",
            "SCOPE {\"touch\":[\"a.rs\"]}\nSCOPE {oops}",
        ] {
            assert!(parse_answer(bad, false).is_err(), "{bad:?}");
        }
        for bad in [
            "RISK {\"score\":101,\"reasons\":[\"a\"]}",
            "RISK {\"score\":-1,\"reasons\":[\"a\"]}",
            "RISK {\"score\":30,\"reasons\":[]}",
            "RISK {\"score\":30}",
            "SCOPE {\"touch\":[\"a.rs\"]}",
        ] {
            assert!(parse_answer(bad, true).is_err(), "{bad:?}");
        }
    }

    /// A missing or malformed answer fails closed at 100 with the reason; a repo-less rating
    /// raises the lowest-band baseline through the model part and can never lower it.
    #[test]
    fn a_missing_answer_fails_closed_and_a_rating_only_raises() {
        let hold = |lines: Option<&str>, unbound: bool| ScopeHold {
            plan: plan(json!({"steps": [{"catalog": "produce"}]})),
            unbound,
            answer: lines.map(|l| ScopeAnswer {
                ord: 1,
                attempt: 0,
                by: "a".into(),
                lines: l.into(),
            }),
        };
        for h in [
            hold(None, false),
            hold(Some(""), false),
            hold(Some("SCOPE nope"), false),
            hold(None, true),
        ] {
            let (s, touch) = scope_score(&h, None, None);
            assert_eq!(s.assessment.score, 100);
            assert_eq!(s.assessment.reasons[0], PA_DECLARED_NO_SCOPE);
            assert!(touch.is_none());
        }
        let (s, _) = scope_score(
            &hold(
                Some("RISK {\"score\":45,\"reasons\":[\"client-facing RFP answer\"]}"),
                true,
            ),
            None,
            None,
        );
        assert_eq!(
            (s.assessment.deterministic, s.assessment.score),
            (0, 45),
            "the rating rides the model part over the lowest-band baseline"
        );
        assert_eq!(s.assessment.model.as_ref().map(|m| m.add), Some(45));
        // The rating can never take the score below the deterministic baseline.
        let lower = judged(30, 10, &["internal".into()]);
        assert_eq!((lower.assessment.score, lower.assessment.model), (30, None));
        // A repo run's docs-only touch set scores 0 with no graph (docs have no symbols).
        let (s, touch) = scope_score(
            &hold(Some("SCOPE {\"touch\":[\"docs/guide.md\"]}"), false),
            None,
            None,
        );
        assert_eq!(s.assessment.score, 0);
        assert_eq!(touch, Some(vec!["docs/guide.md".to_string()]));
        // A behavioural one with no graph fails closed as a user plan's declared touch does.
        let (s, _) = scope_score(
            &hold(Some("SCOPE {\"touch\":[\"src/auth/login.rs\"]}"), false),
            None,
            None,
        );
        assert_eq!(s.assessment.score, 100);
        assert_ne!(s.assessment.reasons[0], PA_DECLARED_NO_SCOPE);
    }
}
