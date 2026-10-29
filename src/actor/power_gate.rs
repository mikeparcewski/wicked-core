//! The actor's half of POWER MODE (program DES-POWER-MODE; [`crate::power`] holds the types,
//! the pure rules and the worker's turn loop): a checkpoint's re-score and its rigor-history
//! row, the switch, an operator's raise, the checkpoint pause's answer, the hand-off steps and
//! the automatic send-back of a hand-off NOT-PASS to the same orchestrator session, and the
//! planned-vs-actual outcome when the run ends. Every rigor value is computed here from the
//! engine's own score; no request can lower it.

use super::*;
use crate::power::{Rigor, RigorChange};

fn power_repo_root(store: &dyn GraphStore, session: &AgentSession) -> Option<std::path::PathBuf> {
    session
        .repo_ref
        .as_deref()
        .and_then(|id| crate::repo::get_repo(store, id).ok().flatten())
        .map(|r| std::path::PathBuf::from(r.root_path))
}

fn emit_change(subscribers: &mut crate::event_log::EventSink, run_id: &str, row: RigorChange) {
    emit(
        subscribers,
        CoreEvent::RigorRescored {
            session: run_id.to_string(),
            change: Box::new(row),
        },
    );
}

fn live(session: &AgentSession) -> bool {
    matches!(
        session.status,
        SessionStatus::Executing | SessionStatus::AwaitingHuman | SessionStatus::Planning
    )
}

fn absorbs_design_of(absorbs: &[String]) -> bool {
    absorbs.iter().any(|a| a == "design")
}

/// The run's band `design` is satisfied by the hand-off review (power mode absorbed it).
pub(super) fn absorbs_design(session: &AgentSession) -> bool {
    session
        .team_plan
        .as_ref()
        .is_some_and(|t| t.power_absorbs.iter().any(|a| a == "design"))
}

/// The hand-off requirements follow the current rigor: its test set, and — from high, when power
/// mode absorbed the band's `design` — the review's design check. A lowering keeps every review
/// (fewer tests, never fewer reviewers).
pub(super) fn retarget_handoff(
    h: &mut crate::power::HandoffRequirements,
    rigor: Rigor,
    absorbs_design: bool,
) {
    h.tests = rigor.tests().into();
    if absorbs_design
        && rigor >= Rigor::High
        && h.reviews.iter().any(|r| r.kind == "distinct_seat")
        && !h.reviews.iter().any(|r| r.kind == "design")
    {
        h.reviews.push(crate::power::ReviewRequirement {
            kind: "design".into(),
            why: "the review on another seat also checks the orchestrator's approach; it counts \
                  as this band's design phase (power mode)"
                .into(),
        });
    }
}

/// A checkpoint the orchestrator's worker measured: score it, append the history row, raise
/// (and hold the raise for the plan's next boundary through the team re-score), lower only at
/// the final checkpoint from evidence, and pause on sudden complexity at a turn end.
pub(super) fn on_checkpoint(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    run_id: &str,
    ord: u32,
    attempt: u32,
    report: &crate::power::CheckpointReport,
) -> anyhow::Result<crate::power::CheckpointAnswer> {
    use crate::power::CheckpointAnswer;
    let Some(session) = crate::domain::get_session(&*store, run_id)? else {
        return Ok(CheckpointAnswer::default());
    };
    let Some(power) = session.power.clone() else {
        return Ok(CheckpointAnswer::default());
    };
    if !live(&session) {
        return Ok(CheckpointAnswer {
            stop: true,
            ..Default::default()
        });
    }
    let root = power_repo_root(&*store, &session);
    let scored = if report.paths.is_empty() {
        None
    } else {
        Some(crate::plan_gate::diff_score_for_run(
            &report.paths,
            root.as_deref(),
            session.base_commit.as_deref(),
        ))
    };
    // The team re-score path holds a raise for the plan's next step boundary (the plan grows:
    // a security review, a full verify), with the steering-rule context re-derived from the diff.
    // A rescore_seq of its own (1000 + checkpoint) keeps it apart from the supervisor's.
    if let Some(tree) = report.tree.as_deref().filter(|_| !report.paths.is_empty()) {
        let seq = 1000u32.saturating_add(power.checkpoints).saturating_add(1);
        if let Err(e) =
            team_gate::on_rescored(store, run_id, ord, attempt, seq, tree, &report.paths)
        {
            eprintln!("wicked-core: run {run_id}: power checkpoint re-score not held ({e:#})");
        }
    }
    // Re-read: the team re-score may have written the session.
    let Some(mut session) = crate::domain::get_session(&*store, run_id)? else {
        return Ok(CheckpointAnswer::default());
    };
    let now = crate::interaction::now_millis();
    let absorbs_design = absorbs_design(&session);
    let Some(power) = session.power.as_mut() else {
        return Ok(CheckpointAnswer::default());
    };
    power.checkpoints = power.checkpoints.saturating_add(1);
    power.orchestrator.turns = power.orchestrator.turns.max(report.turn);
    let checkpoint = power.checkpoints;
    let planned = power.planned_score();
    let current = power.rigor.unwrap_or(Rigor::Light);
    let (actual, destructive, signals_in) = match &scored {
        Some(s) => (
            s.assessment.score,
            s.destructive,
            s.assessment.signals.clone(),
        ),
        None => (0, false, None),
    };
    let actual_rigor = Rigor::of(actual, destructive);
    // Time against the estimate is the ORCHESTRATOR's own time: its turns, not the floor's checks,
    // a reviewer's turn or a wait on a person between them (rig run bc2366e1 paused on "80 min
    // against an estimate of 20" after a 10-minute build and a 70-minute floor).
    let elapsed = crate::power::orchestrator_minutes(power, now);
    let estimate = power.plan.as_ref().and_then(|p| p.estimate_minutes);
    let mut struggle = crate::power::Struggle {
        failed_runs: report.failed_runs,
        reverts: report.reverts,
        turns: report.turn,
        elapsed_minutes: elapsed,
        estimate_minutes: estimate,
        score: 0,
    };
    let (struggle_score, struggle_reasons) = crate::power::struggle_score(&struggle);
    struggle.score = struggle_score;
    // A QE-plan gap: a path the change touched that the intake scope never named (a new surface
    // the test strategy did not plan for). Up to five are named.
    let declared: Vec<String> = session
        .team_plan
        .as_ref()
        .and_then(|t| t.accepted.as_ref())
        .map(|a| a.touch.clone())
        .unwrap_or_default();
    let plan_gaps: Vec<String> = if declared.is_empty() {
        Vec::new()
    } else {
        report
            .paths
            .iter()
            .filter(|p| !declared.iter().any(|d| d == *p))
            .take(5)
            .map(|p| format!("{p} was not in the intake scope"))
            .collect()
    };
    let mut sensitive = Vec::new();
    if signals_in.as_ref().is_some_and(|s| s.critical) {
        sensitive.push("a critical path (gate, governance, deliver, credentials, …)".to_string());
    }
    let signals = crate::power::RigorSignals {
        files: u32::try_from(report.paths.len()).unwrap_or(u32::MAX),
        lines: report.lines,
        dependents: signals_in.as_ref().map(|s| s.dependents),
        spread: signals_in.as_ref().map_or(0, |s| s.products),
        new_dependencies: report.new_dependencies,
        sensitive: sensitive.clone(),
        struggle,
        plan_gaps: plan_gaps.clone(),
    };
    let signals_spread = signals.spread;
    let floor = power.floor();
    let mut to = current.max(actual_rigor).max(floor);
    let mut reasons: Vec<String> = scored
        .as_ref()
        .map(|s| s.assessment.reasons.clone())
        .unwrap_or_else(|| vec!["no change yet".into()]);
    let mut capped = Vec::new();
    let is_final = report.trigger == "pre_handoff";
    let mut note = None;
    if to > current {
        note = Some(format!(
            "Rigor raised {} -> {} (score {actual}). Hand-off now runs the {} test set.",
            current.as_str(),
            to.as_str(),
            to.tests()
        ));
    } else if is_final {
        // (operator ruling 2026-10-29) Lowering only here, from evidence, one band, within the
        // hand-off only (fewer tests, never fewer reviewers), never below a floor.
        let floors = power.all_floors();
        match crate::power::lowering(current, actual_rigor, struggle_score, &plan_gaps, &floors) {
            Ok(Some(down)) => {
                to = down;
                reasons.insert(
                    0,
                    format!(
                        "Rigor lowered from evidence: {} -> {} ({} files, {} dependents, no \
                         struggle, no plan gap). Hand-off runs the {} test set; the reviews \
                         and QE acceptance are unchanged.",
                        current.as_str(),
                        down.as_str(),
                        report.paths.len(),
                        signals.dependents.unwrap_or(0),
                        down.tests()
                    ),
                );
                note = reasons.first().cloned();
            }
            Ok(None) => {}
            Err(floors) => {
                reasons.insert(
                    0,
                    "The diff would allow a lower rigor, but a floor holds it".to_string(),
                );
                capped = floors;
            }
        }
    }
    // Sudden complexity pauses only at a turn end (the worker can wait there). The jump is read
    // against the rigor the TURN started at, so a mid-turn checkpoint's raise never hides it; a
    // trigger the operator already answered is never raised again by the cumulative diff.
    let turn_end = matches!(report.trigger.as_str(), "turn" | "pre_handoff");
    let destructive_new = destructive
        && !power.plan.as_ref().is_some_and(|p| {
            p.floors
                .iter()
                .any(|f| f.by == "policy" && f.rigor == Rigor::Highest)
        });
    let turn_from = power.turn_start_rigor.unwrap_or(current).min(current);
    let spread_grew = crate::power::spread_grew(power, signals.spread);
    let pause_reasons: Vec<(&'static str, String)> = if turn_end && power.pause.is_none() {
        crate::power::sudden_complexity(
            turn_from,
            to,
            destructive_new,
            spread_grew,
            report.new_dependencies > 0 && !sensitive.is_empty(),
            &struggle_reasons,
        )
        .into_iter()
        .filter(|(k, _)| !power.acknowledged.iter().any(|a| a == k))
        .collect()
    } else {
        Vec::new()
    };
    let pause_keys: Vec<String> = pause_reasons.iter().map(|(k, _)| k.to_string()).collect();
    let pause_reasons: Vec<String> = pause_reasons.into_iter().map(|(_, r)| r).collect();
    let paused_before = power.history.iter().filter(|r| r.paused).count();
    let pause =
        (!pause_reasons.is_empty() && paused_before < 3).then(|| crate::power::PowerPause {
            at: now,
            checkpoint,
            recommended: if signals.spread >= 2 {
                "split".into()
            } else if !struggle_reasons.is_empty() {
                "switch_phased".into()
            } else {
                "continue_high".into()
            },
            reasons: pause_reasons.clone(),
        });
    let mut row = crate::power::change(
        &report.trigger,
        current,
        to,
        planned,
        actual,
        reasons,
        Some(signals),
        "engine",
        now,
    );
    row.floors = capped;
    row.checkpoint = Some(checkpoint);
    row.turn = Some(report.turn);
    row.paused = pause.is_some();
    power.rigor = Some(to);
    if turn_end {
        power.turn_start_rigor = Some(to);
    }
    if pause.is_some() {
        // Answering this pause acknowledges its triggers — except spread, whose baseline moves to
        // the spread the operator now accepts, so a further product pauses again.
        if pause_keys.iter().any(|k| k == "spread") {
            power.baseline_spread = Some(signals_spread);
        }
        power
            .acknowledged
            .extend(pause_keys.into_iter().filter(|k| k != "spread"));
    }
    if let Some(p) = power.plan.as_mut() {
        retarget_handoff(&mut p.handoff, to, absorbs_design);
    }
    power.pause = pause.clone();
    if is_final {
        let handoff = power.handoff.get_or_insert_with(|| crate::power::Handoff {
            round: 1,
            max_rounds: crate::power::MAX_HANDOFF_ROUNDS,
            steps: Vec::new(),
        });
        let round = handoff.round;
        handoff.steps.push(crate::power::HandoffStep {
            round,
            step: "rescore".into(),
            ord: Some(ord),
            status: "passed".into(),
            seat: None,
            summary: Some(format!("rigor {} at hand-off", to.as_str())),
            at: now,
        });
    }
    crate::power::set_full_suite(run_id, to.tests() == "full");
    let on = power.on;
    let row = power.push_change(row);
    put_node(store, session.to_node())?;
    emit_change(subscribers, run_id, row);
    emit(
        subscribers,
        CoreEvent::PowerCheckpointReached {
            session: run_id.to_string(),
            ord,
            checkpoint,
            turn: report.turn,
            trigger: report.trigger.clone(),
            head: report.head.clone(),
            picked: 0,
            rigor: to.as_str().to_string(),
            paused: pause.is_some(),
            pause: pause.clone().map(Box::new),
            at: now,
        },
    );
    if pause.is_some() {
        // The worker waits at this turn end for `POST /runs/:id/power/pause`; the run stays
        // executing (no engine gate opens under an in-flight unit) and the pause is on the
        // power record and on this checkpoint's frames.
        crate::power::open_pause(run_id);
    }
    Ok(CheckpointAnswer {
        rigor: Some(to),
        note,
        messages: Vec::new(),
        pause,
        stop: !on,
    })
}

/// Flip the switch (`POST /runs/:id/power`). On -> Off always (it adds rigor: the build ends at
/// its next turn boundary and every later hand-off NOT-PASS asks the operator). Off -> On only
/// when available; the phases the plan mandates stay phased (they are plan steps either way).
pub(super) fn set_power(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    run_id: &str,
    on: bool,
    actor: &str,
    reason: Option<String>,
) -> anyhow::Result<crate::power::PowerState> {
    let mut session = crate::domain::get_session(&*store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("not_found: run {run_id}"))?;
    if !live(&session) {
        anyhow::bail!("conflict: run {run_id} has ended");
    }
    let now = crate::interaction::now_millis();
    let power = match session.power.as_mut() {
        Some(p) => p,
        None if !on => anyhow::bail!("conflict: run {run_id} is already phased"),
        None => anyhow::bail!(
            "refused: run {run_id} was launched phased; power mode needs a plan or preset launch \
             with a repo, decided at launch"
        ),
    };
    if on && !power.available {
        anyhow::bail!(
            "refused: {}",
            power
                .refused_reason
                .clone()
                .unwrap_or_else(|| "phased is required for this run".into())
        );
    }
    if power.on == on {
        return Ok(power.clone());
    }
    let stay_phased: Vec<String> = if on {
        power
            .plan
            .as_ref()
            .map(|p| p.mandatory_phases.iter().map(|m| m.phase.clone()).collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    power.on = on;
    let toggle = crate::power::PowerToggle {
        at: now,
        on,
        by: actor.to_string(),
        reason: reason.clone(),
        stay_phased,
    };
    power.toggles.push(toggle.clone());
    let rigor = power.rigor.unwrap_or(Rigor::Light);
    let planned = power.planned_score();
    let mut row = crate::power::change(
        "toggle",
        rigor,
        rigor,
        planned,
        planned,
        vec![if on {
            "power mode switched on for the rest of the run".into()
        } else {
            "power mode switched off: the rest of the run is phased".into()
        }],
        None,
        actor,
        now,
    );
    row.turn = Some(power.orchestrator.turns);
    row.from = power.rigor;
    row.to = power.rigor;
    let row = power.push_change(row);
    let out = power.clone();
    put_node(store, session.to_node())?;
    if !on {
        // A paused worker waiting on its answer hears the switch.
        crate::power::answer_pause(run_id, crate::power::PauseDecision::SwitchPhased);
    }
    emit(
        subscribers,
        CoreEvent::PowerToggled {
            session: run_id.to_string(),
            toggle: Box::new(toggle),
        },
    );
    emit_change(subscribers, run_id, row);
    Ok(out)
}

/// An operator's raise (`POST /runs/:id/rigor`): never a lowering. The raise becomes a floor and
/// is held for the plan's next step boundary, as a checkpoint's raise is.
pub(super) fn raise_rigor(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    run_id: &str,
    at_least: Rigor,
    actor: &str,
    reason: &str,
) -> anyhow::Result<crate::power::PowerState> {
    let mut session = crate::domain::get_session(&*store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("not_found: run {run_id}"))?;
    if !live(&session) {
        anyhow::bail!("conflict: run {run_id} has ended");
    }
    let now = crate::interaction::now_millis();
    let session_absorbs = session
        .team_plan
        .as_ref()
        .map(|t| t.power_absorbs.clone())
        .unwrap_or_default();
    let cursor = (session.unit_ix, session.attempt);
    let Some(power) = session.power.as_mut() else {
        anyhow::bail!("refused: run {run_id} is not a power run");
    };
    let current = power.rigor.unwrap_or(Rigor::Light);
    if at_least <= current {
        anyhow::bail!(
            "conflict: rigor is already {} (a raise must be higher; rigor is never lowered on \
             request)",
            current.as_str()
        );
    }
    power.rigor = Some(at_least);
    // The raise is a floor whether or not intake planned anything (a declared-scope launch has
    // no intake plan; a raise before intake must survive it).
    power.floors.push(crate::power::RigorFloor {
        rigor: at_least,
        by: "operator".into(),
        why: format!("{actor}: {reason}"),
    });
    let absorbs = absorbs_design_of(&session_absorbs);
    if let Some(p) = power.plan.as_mut() {
        retarget_handoff(&mut p.handoff, at_least, absorbs);
    }
    let planned = power.planned_score();
    let row = crate::power::change(
        "operator",
        current,
        at_least,
        planned,
        planned,
        vec![format!("raised by {actor}: {reason}")],
        None,
        actor,
        now,
    );
    let row = power.push_change(row);
    let out = power.clone();
    crate::power::set_full_suite(run_id, at_least.tests() == "full");
    // The plan follows: the raise is HELD for the next step boundary exactly as a diff re-score's
    // is (`plan.revised{reason:"floor_raised"}`), so the band's phases (a security review at
    // highest) join the plan and the ratcheted score moves with them.
    let ord = crate::domain::session_units(&*store, run_id)?
        .get(cursor.0)
        .map_or(0, |u| u.ord);
    if let Some(tp) = session.team_plan.as_mut() {
        if crate::plan_gate::floor_rises(tp, at_least.min_score(), false) {
            let score = at_least.min_score();
            let assessment = crate::review_scale::Assessment {
                deterministic: score,
                score,
                reasons: vec![format!(
                    "rigor raised to {} by {actor}: {reason}",
                    at_least.as_str()
                )],
                model: None,
                signals: None,
                plan: crate::review_scale::plan_for(score),
            };
            let seq = 2000u32.saturating_add(tp.rev);
            let fact = crate::plan_gate::path_scored_diff(
                run_id,
                ord,
                cursor.1,
                seq,
                None,
                &assessment,
                now,
            )?;
            let held = crate::plan_gate::hold_rescore(
                tp.rescored.as_ref(),
                crate::plan_gate::DiffRescore {
                    ord,
                    attempt: cursor.1,
                    rescore_seq: seq,
                    score,
                    destructive: false,
                    fact: crate::plan_gate::queued_facts(&[fact])?
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("no path.scored fact"))?,
                    obligations: Vec::new(),
                    recalled: Vec::new(),
                },
                true,
            );
            tp.rescored = Some(held);
        }
    }
    put_node(store, session.to_node())?;
    emit_change(subscribers, run_id, row);
    Ok(out)
}

/// The operator's answer to a checkpoint pause (`POST /runs/:id/power/pause`).
pub(super) fn answer_pause(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    run_id: &str,
    decision: crate::power::PauseDecision,
    note: Option<String>,
    actor: &str,
) -> anyhow::Result<crate::power::PowerState> {
    use crate::power::PauseDecision;
    let session = crate::domain::get_session(&*store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("not_found: run {run_id}"))?;
    if session.power.as_ref().is_none_or(|p| p.pause.is_none()) {
        anyhow::bail!("conflict: run {run_id} is not paused at a checkpoint");
    }
    match decision {
        PauseDecision::ContinueHigh => {
            let current = session
                .power
                .as_ref()
                .and_then(|p| p.rigor)
                .unwrap_or(Rigor::Light);
            if current < Rigor::High {
                raise_rigor(
                    store,
                    subscribers,
                    run_id,
                    Rigor::High,
                    actor,
                    "continue at high rigor after a checkpoint pause",
                )?;
            }
        }
        PauseDecision::SwitchPhased => {
            set_power(
                store,
                subscribers,
                run_id,
                false,
                actor,
                Some("switched to phased at a checkpoint pause".into()),
            )?;
        }
        PauseDecision::Continue | PauseDecision::Stop => {}
    }
    if let Some(n) = note.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        crate::power::queue_operator_message(run_id, "all", n);
    }
    let mut session = crate::domain::get_session(&*store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("not_found: run {run_id}"))?;
    let out = match session.power.as_mut() {
        Some(p) => {
            p.pause = None;
            p.operator_gates = p.operator_gates.saturating_add(1);
            p.clone()
        }
        None => anyhow::bail!("conflict: run {run_id} is not a power run"),
    };
    put_node(store, session.to_node())?;
    crate::power::answer_pause(run_id, decision);
    Ok(out)
}

fn handoff_step_of(catalog: Option<&str>) -> Option<&'static str> {
    match catalog? {
        "review" | "critique" => Some("review"),
        "security_review" => Some("security"),
        "test" => Some("qe"),
        _ => None,
    }
}

/// A hand-off step changed (a power run only): record it and say so.
pub(super) fn handoff_step(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    run_id: &str,
    unit: &crate::domain::WorkUnit,
    status: &str,
    summary: Option<String>,
) {
    let step = if unit.catalog.as_deref() == Some("build") {
        Some("checks")
    } else {
        handoff_step_of(unit.catalog.as_deref())
    };
    let Some(step) = step else { return };
    let Ok(Some(mut session)) = crate::domain::get_session(&*store, run_id) else {
        return;
    };
    let Some(power) = session.power.as_mut() else {
        return;
    };
    let now = crate::interaction::now_millis();
    let handoff = power.handoff.get_or_insert_with(|| crate::power::Handoff {
        round: 1,
        max_rounds: crate::power::MAX_HANDOFF_ROUNDS,
        steps: Vec::new(),
    });
    let row = crate::power::HandoffStep {
        round: handoff.round,
        step: step.into(),
        ord: Some(unit.ord),
        status: status.into(),
        seat: unit.assigned_cli.clone(),
        summary,
        at: now,
    };
    handoff.steps.push(row.clone());
    let round = handoff.round;
    let max_rounds = handoff.max_rounds;
    if put_node(store, session.to_node()).is_err() {
        return;
    }
    emit(
        subscribers,
        CoreEvent::PowerHandoffStepped {
            session: run_id.to_string(),
            round,
            max_rounds,
            step: Box::new(row),
        },
    );
}

/// At a dispatch on a power run: every hand-off step still `running` on an earlier ord passed
/// (the run moved past it), and this unit's step (if it is one) is `running`.
pub(super) fn on_dispatch(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    run_id: &str,
    unit: &crate::domain::WorkUnit,
) {
    let Ok(Some(session)) = crate::domain::get_session(&*store, run_id) else {
        return;
    };
    let Some(power) = session.power.as_ref() else {
        return;
    };
    let Ok(units) = crate::domain::session_units(&*store, run_id) else {
        return;
    };
    let running: Vec<crate::domain::WorkUnit> = power
        .handoff
        .iter()
        .flat_map(|h| h.steps.iter())
        .filter(|s| s.ord.is_some_and(|o| o < unit.ord))
        .filter_map(|s| s.ord)
        .collect::<std::collections::BTreeSet<u32>>()
        .into_iter()
        .filter(|o| {
            // The latest row of this ord is still `running`.
            power
                .handoff
                .iter()
                .flat_map(|h| h.steps.iter())
                .rev()
                .find(|s| s.ord == Some(*o))
                .is_some_and(|s| s.status == "running")
        })
        .filter_map(|o| units.iter().find(|u| u.ord == o).cloned())
        .collect();
    for u in running {
        handoff_step(store, subscribers, run_id, &u, "passed", None);
    }
    let after_build = power.orchestrator.ord.is_some_and(|b| unit.ord > b);
    if after_build {
        // The build's floor passed when the run moved past it.
        if let Some(b) = power
            .orchestrator
            .ord
            .and_then(|b| units.iter().find(|u| u.ord == b))
        {
            // This round's checks only (a round 1 pass does not stand for round 2).
            let floored = power.handoff.iter().any(|h| {
                h.steps.iter().any(|s| {
                    s.round == h.round
                        && s.step == "checks"
                        && s.ord == Some(b.ord)
                        && s.status == "passed"
                })
            });
            if !floored {
                let b = b.clone();
                handoff_step(store, subscribers, run_id, &b, "passed", None);
            }
        }
    }
    if handoff_step_of(unit.catalog.as_deref()).is_some() && after_build {
        handoff_step(store, subscribers, run_id, unit, "running", None);
    }
}

/// (program DES-POWER-MODE §6) A hand-off NOT-PASS on a power run goes back to the SAME
/// orchestrator session on its own — a `request_changes` rewind to the build, whose seat resumes
/// its session — up to [`crate::power::MAX_HANDOFF_ROUNDS`]; the next asks the operator. `Some`
/// is the note the send-back carries; `None` leaves the gate to the operator.
pub(super) fn auto_send_back(
    subscribers: &mut crate::event_log::EventSink,
    session: &mut AgentSession,
    unit: &crate::domain::WorkUnit,
    class: &str,
    reason: &str,
) -> Option<String> {
    // Mutates `session` only: the caller (`escalate_denied_unit`) persists it with the pause, so
    // nothing written here can be overwritten by the caller's copy.
    let run_id = session.id.clone();
    let power = session.power.as_mut().filter(|p| p.on)?;
    let build = power.orchestrator.ord?;
    if unit.ord < build {
        return None;
    }
    let evaluator_fail = class == "verdict_not_pass"
        && unit.role == crate::workflow::PhaseRole::Evaluator
        && unit.tool_cmd.is_none();
    // The build's own floor at hand-off: a red check goes back too, unless the base itself is
    // red (that needs the operator's judgment, not another turn).
    let lower = reason.to_ascii_lowercase();
    let floor_fail = class == "floor_failed"
        && unit.ord == build
        && !lower.contains("base_red")
        && !lower.contains("base is red")
        && !lower.contains("timed out");
    if !evaluator_fail && !floor_fail {
        return None;
    }
    let step = if unit.ord == build {
        Some("checks")
    } else {
        handoff_step_of(unit.catalog.as_deref())
    };
    let now = crate::interaction::now_millis();
    let handoff = power.handoff.get_or_insert_with(|| crate::power::Handoff {
        round: 1,
        max_rounds: crate::power::MAX_HANDOFF_ROUNDS,
        steps: Vec::new(),
    });
    let round = handoff.round.max(1);
    let max_rounds = handoff.max_rounds;
    if let Some(step) = step {
        let row = crate::power::HandoffStep {
            round,
            step: step.into(),
            ord: Some(unit.ord),
            status: "failed".into(),
            seat: unit.assigned_cli.clone(),
            summary: Some(
                reason
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(300)
                    .collect(),
            ),
            at: now,
        };
        handoff.steps.push(row.clone());
        emit(
            subscribers,
            CoreEvent::PowerHandoffStepped {
                session: run_id,
                round,
                max_rounds,
                step: Box::new(row),
            },
        );
    }
    if round >= max_rounds {
        return None;
    }
    handoff.round = round + 1;
    Some(format!(
        "Power mode hand-off round {round} of {max_rounds}: {} did not pass. The findings go back \
         to the orchestrator's session; fix them, commit, and hand off again.",
        if floor_fail {
            "the repository's checks"
        } else {
            "the review"
        }
    ))
}

/// The planned-vs-actual outcome, when a power run ends (insights' calibration input). Mutates
/// `session`; the caller persists it with the terminal status. `completed` reads `delivered`
/// when the run's deliver step is done.
pub(super) fn set_outcome(store: &dyn GraphStore, session: &mut AgentSession, outcome: &str) {
    let run_id = session.id.clone();
    // Every run's queue goes with it (a phased run's undelivered messages too).
    crate::power::forget_run(&run_id);
    let Some(power) = session.power.as_mut() else {
        return;
    };
    if power.outcome.is_some() {
        return;
    }
    let delivered = outcome == "completed"
        && crate::domain::session_units(store, &run_id).is_ok_and(|us| {
            us.iter().any(|u| {
                u.catalog.as_deref() == Some("deliver")
                    && u.status == crate::domain::UnitStatus::Done
            })
        });
    let now = crate::interaction::now_millis();
    let last = power.history.last();
    power.outcome = Some(crate::power::PowerOutcome {
        planned_rigor: power.plan.as_ref().map_or(Rigor::Light, |p| p.rigor),
        final_rigor: power.rigor.unwrap_or(Rigor::Light),
        planned_score: power.planned_score(),
        final_score: last.map_or(power.planned_score(), |r| r.actual),
        estimate_minutes: power.plan.as_ref().and_then(|p| p.estimate_minutes),
        actual_minutes: power.launched_at.map_or(0, |t| {
            u32::try_from((now - t).max(0) / 60_000).unwrap_or(u32::MAX)
        }),
        turns: power.orchestrator.turns,
        checkpoints: power.checkpoints,
        handoff_rounds: power.handoff.as_ref().map_or(0, |h| h.round),
        operator_gates: power.operator_gates,
        outcome: if delivered { "delivered" } else { outcome }.to_string(),
    });
    power.orchestrator.in_turn = false;
    power.pause = None;
    crate::power::forget_run(&run_id);
}
