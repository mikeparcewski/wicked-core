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
    let mut units = crate::domain::session_units(store, &run_id)?;
    let Some(unit) = units.get_mut(unit_ix) else {
        return Ok(None);
    };
    if unit.repeat_from.is_none() {
        return Ok(None);
    }
    // A choice answers ONE ask: the run that received it consumed it, whatever it decided
    // (agy r1 on #1226, P1) — a later pass, or a rewind across this step, never sees it again.
    if unit.repeat_choice.take().is_some() {
        put_node(store, unit.to_node())?;
    }
    let unit = &units[unit_ix];
    let Some(signal) = crate::repeat::parse_signal(output) else {
        return Ok(None);
    };
    if signal.next == Next::Done {
        return Ok(None);
    }
    let from = unit.repeat_from.as_deref().unwrap_or_default();
    let ord = unit.ord;
    let refuse = |subscribers: &mut crate::event_log::EventSink, why: String| {
        emit_run_error(
            subscribers,
            &run_id,
            anyhow::anyhow!("step {ord} asked for another pass, but {why}; the run goes on"),
        );
    };
    let Some(from_ix) = units
        .iter()
        .position(|u| u.phase_id() == Some(from))
        .filter(|&i| i < unit_ix)
    else {
        // The def's validation refuses this; a revision that moved the target is said, and the
        // run moves on rather than looping somewhere nobody declared.
        refuse(
            subscribers,
            format!("{from} is not a step before it in this run"),
        );
        return Ok(None);
    };
    // The def's rule, re-judged on the run as it stands (a mid-run revision may have inserted
    // units into the range): nothing that ships, asks consent, belongs to a team member or loops
    // on its own is run again unattended (agy r1 on #1226).
    if let Some(u) = units[from_ix..=unit_ix].iter().find(|u| {
        crate::deliver_lift::is_deliver_unit(u)
            || matches!(u.gate, crate::workflow::GateSpec::ConsentBefore)
            || u.owner == crate::workflow::StepOwner::Team
            || (u.ord != ord && u.repeat_from.is_some())
    }) {
        refuse(
            subscribers,
            format!(
                "its range now holds unit {}, which cannot run again unattended",
                u.ord
            ),
        );
        return Ok(None);
    }
    // `repeat_passes` counts the passes already sent back; this one is pass `repeat_passes + 1`,
    // the last the engine runs is MAX_PASSES (codex/agy r1 on #1226: pass 201 got through).
    if unit.repeat_passes.saturating_add(1) >= crate::repeat::MAX_PASSES {
        refuse(
            subscribers,
            format!(
                "the engine stops a loop at {} passes",
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
            // approve dispatches it at its own next attempt, a fresh dispatch key. Every answer is
            // a person's, so an ask that keeps asking is bounded by the person answering it.
            let mut u = unit.clone();
            u.status = crate::domain::UnitStatus::Distributed;
            u.worktree_baseline = None;
            u.worktree_mutation = None;
            session.unit_ix = unit_ix;
            session.attempt = next_attempt(unit);
            put_node(store, u.to_node())?;
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
            // ONE write over the range, the counter and the cursor (codex r1 on #1226: a crash
            // between per-unit writes left re-armed units behind a cursor still on the loop step).
            // Every unit from the target through the loop unit runs again at its own next attempt
            // (a fresh dispatch key), as a NEW pass: nothing of the previous pass's tree baseline,
            // denial, failed seats, review rounds or rulings is carried onto different work (codex
            // r1: page two's first FAIL reached the capped review's adjudication).
            let mut nodes = Vec::new();
            for (ix, u) in units.iter().enumerate().take(unit_ix + 1).skip(from_ix) {
                let mut u = u.clone();
                // What this pass's review deferred to delivery stays owed after the next pass
                // reviews different work (codex r2 on #1226): kept on the unit, which the
                // deliver gate lists beside the current pass's.
                if u.role == crate::workflow::PhaseRole::Evaluator
                    && u.tool_cmd.is_none()
                    && u.status == crate::domain::UnitStatus::Done
                {
                    let out = crate::domain::get_work_output(&*store, &u.id).unwrap_or_default();
                    for item in crate::review_context::parse_review_items(&out) {
                        if item.status == crate::review_context::DEFERRED
                            && !u.carried_deferred.contains(&item.line)
                        {
                            u.carried_deferred.push(item.line);
                        }
                    }
                }
                // The previous pass's send-back marker is not this pass's instruction (codex r2).
                u.description = crate::actor::strip_rework_marker(&u.description);
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
                u.worker_failed_clis.clear();
                u.review_rounds.clear();
                u.operator_rulings.clear();
                u.floor_auto_retries = 0;
                if ix == unit_ix {
                    u.repeat_passes = pass;
                }
                nodes.push(u.to_node());
            }
            session.unit_ix = from_ix;
            session.attempt = next_attempt(&units[from_ix]);
            session.verified_tree = None;
            session.status = SessionStatus::Executing;
            nodes.push(session.to_node());
            crate::domain::put_nodes(store, &nodes)?;
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

/// Whether `run_id` has an open `repeat` ask: a resume must not run past it (codex r1 on #1226 —
/// `resume_run` re-dispatched the asking step with no answer).
pub(super) fn ask_open(store: &dyn GraphStore, run_id: &str) -> anyhow::Result<bool> {
    Ok(crate::interaction::list_interactions(
        store,
        Some(run_id),
        Some(crate::interaction::InteractionStatus::Open),
    )?
    .iter()
    .any(|r| r.gate_kind.as_deref() == Some(crate::repeat::GATE_KIND)))
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
        if v.is_empty() {
            v.push("approve".into());
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{HumanConfirm, UnitStatus, WorkUnit};
    use crate::scope::EntityMode;
    use std::sync::mpsc::channel;
    use wicked_apps_core::open_store;

    fn session(run_id: &str, unit_ix: usize) -> AgentSession {
        AgentSession {
            intent_amendments: Vec::new(),
            id: run_id.into(),
            workflow_id: format!("wf-{run_id}"),
            problem: "p".into(),
            entity_mode: EntityMode::Shared,
            collection_scope: None,
            clis: vec!["claude".into()],
            status: SessionStatus::Executing,
            human_confirm: HumanConfirm::None,
            auto_deliver: false,
            unit_ix,
            attempt: 0,
            workdir: None,
            repo_ref: None,
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            project_graph: None,
            project_id: None,
            archived_at: None,
            archive_note: None,
            verified_tree: None,
            run_branch: None,
            base_commit: None,
            finished_at: None,
            seat_failures: Vec::new(),
            benched_seats: Vec::new(),
            team: None,
            team_plan: None,
            exclude_seats: Vec::new(),
            evidence_root: None,
            assurance: Default::default(),
            power: None,
        }
    }

    /// `page` (ord 1, done) → `next` (ord 2, the loop step, just passed); `extra` units go between.
    fn seed(
        store: &mut dyn GraphStore,
        run_id: &str,
        extra: Vec<WorkUnit>,
        next: impl FnOnce(&mut WorkUnit),
    ) -> usize {
        let mut page = WorkUnit::pending(format!("{run_id}:page"), run_id, 1, "page");
        page.status = UnitStatus::Done;
        page.last_attempt = Some(0);
        page.assigned_cli = Some("claude".into());
        page.review_rounds.push(crate::domain::ReviewRound {
            attempt: 0,
            findings: "f".into(),
            outcome: Some(crate::domain::REVIEW_SENT_BACK.to_string()),
        });
        page.worker_failed_clis.push("codex".into());
        put_node(store, page.to_node()).unwrap();
        let mut ord = 2;
        for mut u in extra {
            u.ord = ord;
            ord += 1;
            put_node(store, u.to_node()).unwrap();
        }
        let mut n = WorkUnit::pending(format!("{run_id}:next"), run_id, ord, "next");
        n.status = UnitStatus::Done;
        n.last_attempt = Some(0);
        n.tool_cmd = Some(vec!["true".into()]);
        n.repeat_from = Some("page".into());
        next(&mut n);
        put_node(store, n.to_node()).unwrap();
        let ix = (ord - 1) as usize;
        put_node(store, session(run_id, ix).to_node()).unwrap();
        ix
    }

    fn run(
        store: &mut dyn GraphStore,
        run_id: &str,
        ix: usize,
        out: &str,
    ) -> (Option<StepApplied>, Vec<CoreEvent>) {
        let mut s = crate::domain::get_session(&*store, run_id)
            .unwrap()
            .unwrap();
        let (tx, _rx) = channel::<Command>();
        let mut subs = crate::event_log::EventSink::default();
        let (ev_tx, ev_rx) = channel::<CoreEvent>();
        subs.push(ev_tx);
        let runner: Arc<dyn StepRunner> = Arc::new(crate::workflow::StubStepRunner);
        let r = after_pass(
            store,
            &mut subs,
            &runner,
            &tx,
            &mut s,
            ix,
            out,
            &None,
            &None,
            uuid::Uuid::nil(),
            false,
        )
        .unwrap();
        (r, ev_rx.try_iter().collect())
    }

    fn unit(store: &dyn GraphStore, run_id: &str, ord: u32) -> WorkUnit {
        crate::domain::session_units(store, run_id)
            .unwrap()
            .into_iter()
            .find(|u| u.ord == ord)
            .unwrap()
    }

    const CONTINUE: &str = r#"wicked-repeat {"next":"continue","line":"page 2"}"#;

    fn errors(evs: &[CoreEvent]) -> Vec<String> {
        evs.iter()
            .filter_map(|e| match e {
                CoreEvent::Error { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// codex/agy r1 on #1226: pass 200 is the last the engine runs (pass 201 got through).
    #[test]
    fn the_engine_runs_no_pass_past_its_cap() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let ix = seed(&mut store, "cap", Vec::new(), |n| {
            n.repeat_passes = crate::repeat::MAX_PASSES - 1
        });
        let (r, evs) = run(&mut store, "cap", ix, CONTINUE);
        assert!(r.is_none(), "the run goes on past the loop");
        assert!(
            errors(&evs)
                .iter()
                .any(|m| m.contains("stops a loop at 200 passes")),
            "{evs:?}"
        );
        assert!(!evs
            .iter()
            .any(|e| matches!(e, CoreEvent::StepRepeated { .. })));
        assert_eq!(
            unit(&store, "cap", 2).repeat_passes,
            crate::repeat::MAX_PASSES - 1
        );
    }

    /// agy r1 on #1226 (P1): a choice is consumed by the run that received it, whatever it decided.
    #[test]
    fn a_choice_answers_one_ask_and_is_gone_after_its_run() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let ix = seed(&mut store, "choice", Vec::new(), |n| {
            n.repeat_choice = Some("lower".into())
        });
        let (r, _) = run(
            &mut store,
            "choice",
            ix,
            r#"wicked-repeat {"next":"done","line":"lowered"}"#,
        );
        assert!(r.is_none());
        assert_eq!(unit(&store, "choice", 2).repeat_choice, None);
    }

    /// agy r1 on #1226: a range that now holds a deliver step is never run again unattended.
    #[test]
    fn a_range_that_now_holds_a_deliver_step_is_not_looped() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let mut deliver = WorkUnit::pending("dl:deliver", "dl", 0, "deliver");
        deliver.catalog = Some("deliver".into());
        deliver.tool_cmd = Some(vec!["true".into()]);
        let ix = seed(&mut store, "dl", vec![deliver], |_| {});
        let (r, evs) = run(&mut store, "dl", ix, CONTINUE);
        assert!(r.is_none());
        assert!(
            errors(&evs)
                .iter()
                .any(|m| m.contains("cannot run again unattended")),
            "{evs:?}"
        );
        assert_eq!(
            unit(&store, "dl", 1).status,
            UnitStatus::Done,
            "nothing was re-armed"
        );
    }

    /// codex r1/r2 on #1226: a new pass carries nothing of the last pass's reviews, send-back or
    /// failed seats, keeps what its review deferred to delivery, and the range, counter and cursor
    /// move together.
    #[test]
    fn continue_starts_a_fresh_pass_of_the_range() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let mut critique = WorkUnit::pending("fresh:critique", "fresh", 0, "critique");
        critique.role = crate::workflow::PhaseRole::Evaluator;
        let ix = seed(&mut store, "fresh", vec![critique], |_| {});
        let review = unit(&store, "fresh", 2);
        let o = crate::execute::apply_unit(
            &mut store,
            &review,
            "ITEM 1: PASS — rule a\nITEM 2: DEFERRED — confirm page-one CI\nVERDICT: PASS",
            "wf-fresh",
            EntityMode::Shared,
            "fresh",
            None,
            0,
        )
        .unwrap();
        assert!(o.approved);
        let mut review = unit(&store, "fresh", 2);
        review.status = UnitStatus::Done;
        review.last_attempt = Some(0);
        put_node(&mut store, review.to_node()).unwrap();
        let mut page = unit(&store, "fresh", 1);
        page.description =
            crate::actor::apply_rework_marker(&page.description, 1, "replace page-one-symbol");
        put_node(&mut store, page.to_node()).unwrap();
        let (r, evs) = run(&mut store, "fresh", ix, CONTINUE);
        assert!(r.is_some());
        assert!(
            evs.iter().any(|e| matches!(e, CoreEvent::StepRepeated { ord: 3, from_ord: 1, pass: 1, line, .. } if line == "page 2")),
            "{evs:?}"
        );
        let page = unit(&store, "fresh", 1);
        assert!(
            page.review_rounds.is_empty() && page.worker_failed_clis.is_empty(),
            "{page:?}"
        );
        assert_eq!(page.description, "page", "the send-back marker is gone");
        assert_eq!(
            unit(&store, "fresh", 2).carried_deferred,
            ["ITEM 2: DEFERRED — confirm page-one CI"]
        );
        let next = unit(&store, "fresh", 3);
        assert_eq!((next.status, next.repeat_passes), (UnitStatus::Pending, 1));
        let s = crate::domain::get_session(&store, "fresh")
            .unwrap()
            .unwrap();
        assert_eq!(
            (s.unit_ix, s.attempt),
            (0, 1),
            "the cursor is on the range's first step, at a fresh attempt"
        );
    }
}
