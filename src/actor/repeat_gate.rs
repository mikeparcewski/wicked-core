//! The actor's half of a BOUNDED LOOP ([`crate::repeat`] holds the decision line's grammar): when
//! a Tool unit that declares `repeat_from` passes, its command's decision re-arms the range and runs
//! it again, pauses at a `repeat` ask, or lets the run move on; and the ask's answers.

use super::*;

/// The decision a loop unit's stored output carries (its last `wicked-repeat` line), when the unit
/// at `ord` decides a loop. `None` for any other unit, or when the command printed no decision.
pub(super) fn signal_for(
    store: &dyn GraphStore,
    run_id: &str,
    ord: u32,
) -> anyhow::Result<Option<crate::repeat::Signal>> {
    let units = crate::domain::session_units(store, run_id)?;
    let Some(u) = units.iter().find(|u| u.ord == ord) else {
        return Ok(None);
    };
    if u.repeat_from.is_none() || u.tool_cmd.is_none() {
        return Ok(None);
    }
    Ok(crate::domain::get_work_output(store, &u.id)
        .as_deref()
        .and_then(crate::repeat::parse_signal))
}

/// A loop unit at `unit_ix` PASSED its gate with `output`: act on its command's decision.
/// `Ok(None)` = no loop decision applies (the caller advances as it always did); otherwise the
/// run was re-armed and resumed, or paused at the ask, and the caller returns what this returns.
#[allow(clippy::too_many_arguments)]
pub(super) fn after_pass(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    session: &mut AgentSession,
    unit_ix: usize,
    output: &str,
    lifecycle_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    actor_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    process_gen: uuid::Uuid,
    is_acp: bool,
) -> anyhow::Result<Option<StepApplied>> {
    use crate::repeat::Next;
    let run_id = session.id.clone();
    let Some(signal) = crate::repeat::parse_signal(output) else {
        return Ok(None);
    };
    if signal.next == Next::Done {
        return Ok(None);
    }
    let units = crate::domain::session_units(store, &run_id)?;
    let Some(unit) = units.get(unit_ix) else {
        return Ok(None);
    };
    let Some(from) = unit.repeat_from.as_deref() else {
        return Ok(None);
    };
    let ord = unit.ord;
    let Some(from_ix) = units
        .iter()
        .position(|u| u.phase_id() == Some(from))
        .filter(|&i| i < unit_ix)
    else {
        // The def's validation refuses this; a revision that moved the target is said, and the
        // run moves on rather than looping somewhere nobody declared.
        emit_run_error(
            subscribers,
            &run_id,
            anyhow::anyhow!(
                "step {ord} asked for another pass from {from}, which is not a step before it in \
                 this run; the run goes on"
            ),
        );
        return Ok(None);
    };
    if unit.repeat_passes >= crate::repeat::MAX_PASSES {
        emit_run_error(
            subscribers,
            &run_id,
            anyhow::anyhow!(
                "step {ord} asked for pass {} of its loop; the engine stops a loop at {} passes, \
                 so the run goes on",
                unit.repeat_passes.saturating_add(2),
                crate::repeat::MAX_PASSES
            ),
        );
        return Ok(None);
    }
    match signal.next {
        Next::Done => Ok(None),
        Next::Ask => {
            // The cursor STAYS on the loop unit (it ran and passed): a choice is recorded on it and
            // the gate's approve dispatches it again at a fresh attempt, so its command decides
            // again with the operator's choice in hand. Reject cancels the run; what it did is kept.
            // Re-armed like a rewind target (a Done unit's late result is stale by rule): the
            // approve dispatches it at its own next attempt, a fresh dispatch key.
            let mut u = unit.clone();
            u.status = crate::domain::UnitStatus::Distributed;
            u.worktree_baseline = None;
            u.worktree_mutation = None;
            put_node(store, u.to_node())?;
            session.unit_ix = unit_ix;
            session.attempt = next_attempt(unit);
            pause_for_human(
                store,
                subscribers,
                self_tx,
                session,
                ord,
                Some(ord),
                crate::repeat::GATE_KIND,
                crate::repeat::ask_prompt(&signal, ord),
            )?;
            Ok(Some(StepApplied::Paused))
        }
        Next::Continue => {
            let from_ord = units[from_ix].ord;
            let pass = unit.repeat_passes.saturating_add(1);
            // ONE pass of writes over the range, as a rewind re-arms its range: every unit from the
            // target through the loop unit runs again at its own next attempt (a fresh dispatch
            // key), with nothing of the previous pass's tree baseline or denial left on it.
            for (ix, u) in units.iter().enumerate().take(unit_ix + 1).skip(from_ix) {
                let mut u = u.clone();
                u.status = if ix == from_ix {
                    crate::domain::UnitStatus::Distributed
                } else {
                    crate::domain::UnitStatus::Pending
                };
                u.worktree_baseline = None;
                u.worktree_mutation = None;
                u.denial = None;
                u.denial_reason = None;
                u.requested_rejudge = None;
                u.rework_of = None;
                u.rework_amendment = None;
                if ix == unit_ix {
                    u.repeat_passes = pass;
                    // A choice answers ONE ask: the next pass decides without it.
                    u.repeat_choice = None;
                }
                put_node(store, u.to_node())?;
            }
            session.unit_ix = from_ix;
            session.attempt = next_attempt(&units[from_ix]);
            session.verified_tree = None;
            session.status = SessionStatus::Executing;
            put_node(store, session.to_node())?;
            emit(
                subscribers,
                CoreEvent::StepRepeated {
                    session: run_id.clone(),
                    ord,
                    from_ord,
                    pass,
                    line: signal.line.clone(),
                },
            );
            match advance_or_pause(
                store,
                subscribers,
                runner,
                self_tx,
                &run_id,
                from_ix,
                lifecycle_maps,
                actor_maps,
                process_gen,
                is_acp,
            )? {
                Progress::Dispatched | Progress::Deferred => Ok(Some(StepApplied::Continuing)),
                Progress::Paused => Ok(Some(StepApplied::Paused)),
                Progress::Done => {
                    finalize_run(store, subscribers, runner, self_tx, &run_id)?;
                    Ok(Some(StepApplied::Finished))
                }
            }
        }
    }
}

/// An answer at an open `repeat` ask on unit `ord`, turned into the ordinary approve / reject: a
/// `consent:<id>` the ask offers (or a plain approve = its recommended choice) is recorded on the
/// unit, whose command re-runs with it; reject stops the run. Every other answer is refused here,
/// before the gate row resolves, so the ask stays open and re-answerable.
pub(super) fn resolve_answer(
    store: &mut dyn GraphStore,
    run_id: &str,
    ord: u32,
    decision: crate::workflow::HumanDecision,
) -> anyhow::Result<crate::workflow::HumanDecision> {
    use crate::workflow::HumanDecision;
    let signal = signal_for(&*store, run_id, ord)?;
    let offered = |s: Option<&crate::repeat::Signal>| {
        let mut v: Vec<String> = s
            .map(|s| {
                s.choices
                    .iter()
                    .map(|c| crate::repeat::token(&c.id))
                    .collect()
            })
            .unwrap_or_default();
        v.push("reject".into());
        v.join(", ")
    };
    let chosen = match &decision {
        HumanDecision::ConsentChoice { choice } => {
            let Some(c) = signal
                .as_ref()
                .and_then(|s| crate::repeat::choice_for_token(s, choice))
            else {
                anyhow::bail!(
                    "`{choice}` is not a choice of the ask on unit {ord} (offered: {})",
                    offered(signal.as_ref())
                );
            };
            Some(c.id.clone())
        }
        HumanDecision::Approve { amend, .. }
            if amend.as_deref().is_none_or(|a| a.trim().is_empty()) =>
        {
            signal
                .as_ref()
                .and_then(crate::repeat::default_choice)
                .map(|c| c.id.clone())
        }
        HumanDecision::Reject => return Ok(decision),
        _ => anyhow::bail!(
            "the ask on unit {ord} takes one of its choices or reject (offered: {})",
            offered(signal.as_ref())
        ),
    };
    let mut unit = crate::domain::session_units(&*store, run_id)?
        .into_iter()
        .find(|u| u.ord == ord)
        .ok_or_else(|| anyhow::anyhow!("run {run_id} has no unit {ord}"))?;
    // EVERY answer rewrites the record: a choice from an earlier ask never rides one that chose
    // nothing.
    if unit.repeat_choice != chosen {
        unit.repeat_choice = chosen;
        put_node(store, unit.to_node())?;
    }
    Ok(match decision {
        HumanDecision::ConsentChoice { .. } => decision,
        _ => HumanDecision::Approve {
            amend: None,
            amend_scope: crate::workflow::AmendScope::Cursor,
        },
    })
}
