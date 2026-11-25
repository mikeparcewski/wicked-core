//! Re-probe due benches while a run waits at its dead-seat gate. Only the actor reads and
//! writes the store; the blocking seat call runs on a short-lived worker.

use std::collections::HashSet;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use wicked_apps_core::{GraphRead, GraphStore, ToNode};

use super::emit;
use crate::command::Command;
use crate::domain::{put_node, AgentSession, SessionStatus, WorkUnit, DENIAL_SOURCE_DEAD_SEAT};
use crate::event::CoreEvent;
use crate::event_log::EventSink;
use crate::seat_bench::BenchReprobe;
use crate::workflow::{GateExpect, HumanDecision, StepRunner};

pub(super) const DEAD_SEAT_SWEEP: Duration = Duration::from_secs(30);

fn dead_seat_cursor(
    store: &dyn GraphRead,
    session: &AgentSession,
) -> anyhow::Result<Option<WorkUnit>> {
    if session.status != SessionStatus::AwaitingHuman {
        return Ok(None);
    }
    Ok(crate::domain::session_units(store, &session.id)?
        .into_iter()
        .nth(session.unit_ix)
        .filter(|unit| {
            unit.denial
                .as_ref()
                .is_some_and(|d| d.source == DENIAL_SOURCE_DEAD_SEAT)
        }))
}

pub(super) fn sweep(
    store: &dyn GraphRead,
    runner: &Arc<dyn StepRunner>,
    registry: fn() -> Vec<crate::AgenticCli>,
    self_tx: &Sender<Command>,
    probing: &mut HashSet<String>,
    now: i64,
) -> anyhow::Result<()> {
    for session in crate::domain::all_sessions(store)? {
        if probing.contains(&session.id)
            || !session
                .benched_seats
                .iter()
                .any(|b| crate::seat_bench::due_for_reprobe(b, now))
            || dead_seat_cursor(store, &session)?.is_none()
        {
            continue;
        }
        probing.insert(session.id.clone());
        let runner = Arc::clone(runner);
        let self_tx = self_tx.clone();
        std::thread::spawn(move || {
            // Always complete the probe so a panicking runner cannot leave the run in probing.
            let reprobes = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let (_, reprobes) = crate::cli_runner::reprobe_due_benches(
                    &*runner,
                    &registry(),
                    &session.benched_seats,
                    now,
                    &|_| {},
                    Some(&session.id),
                );
                reprobes
            }))
            .unwrap_or_default();
            let _ = self_tx.send(Command::DeadSeatReprobed {
                run_id: session.id,
                reprobes,
            });
        });
    }
    Ok(())
}

pub(super) fn fold(
    store: &mut dyn GraphStore,
    subscribers: &mut EventSink,
    self_tx: &Sender<Command>,
    probing: &mut HashSet<String>,
    run_id: &str,
    reprobes: &[BenchReprobe],
) -> anyhow::Result<()> {
    probing.remove(run_id);
    let Some(mut session) = crate::domain::get_session(store, run_id)? else {
        return Ok(());
    };
    let Some(unit) = dead_seat_cursor(store, &session)? else {
        return Ok(());
    };
    fold_bench_reprobes(store, subscribers, &mut session, unit.ord, reprobes)?;
    if unit
        .assigned_cli
        .as_ref()
        .is_some_and(|cli| !session.benched_seats.iter().any(|b| &b.cli == cli))
    {
        let (reply, _) = std::sync::mpsc::channel();
        let _ = self_tx.send(Command::ConfirmGate {
            run_id: session.id,
            decision: HumanDecision::Approve {
                amend: None,
                amend_scope: Default::default(),
            },
            actor: Some(crate::power::BENCH_REPROBE_ACTOR.to_string()),
            expect: Some(GateExpect {
                ord: Some(unit.ord),
                plan_rev: None,
                gate_seq: None,
            }),
            reply,
        });
    }
    Ok(())
}

/// The single actor fold for worker and parked-run re-probes. Persist before readers or the
/// queued gate approval can see the returned seat, and publish only outcomes actually applied.
pub(super) fn fold_bench_reprobes(
    store: &mut dyn GraphStore,
    subscribers: &mut EventSink,
    session: &mut AgentSession,
    ord: u32,
    reprobes: &[BenchReprobe],
) -> anyhow::Result<()> {
    let applied = crate::seat_bench::apply_reprobes(&mut session.benched_seats, reprobes);
    if applied.is_empty() {
        return Ok(());
    }
    // A returned launcher bench must also be usable when a plan revision re-reads its roster.
    for a in &applied {
        if let (crate::seat_bench::Applied::Returned { cli, .. }, Some(tp)) =
            (a, session.team_plan.as_mut())
        {
            crate::seat_bench::clear_launcher_health(&mut tp.roster, cli);
        }
    }
    put_node(store, session.to_node())?;
    for a in applied {
        emit(
            subscribers,
            match a {
                crate::seat_bench::Applied::Returned { cli, reason } => {
                    CoreEvent::SeatBenchLifted {
                        session: session.id.clone(),
                        ord,
                        cli,
                        reason,
                    }
                }
                crate::seat_bench::Applied::Rebenched {
                    cli, reason, until, ..
                } => CoreEvent::SeatBenched {
                    session: session.id.clone(),
                    ord,
                    cli,
                    reason,
                    source: "reprobe".to_string(),
                    benched_until_ms: until,
                },
            },
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BenchedSeat, HumanConfirm, UnitDenial, UnitStatus, WorkUnit};
    use crate::workflow::{HumanDecision, StepInput, StepOutput, StepStatus};
    use crate::EntityMode;
    use std::sync::mpsc::{channel, Receiver, TryRecvError};
    use std::thread::{self, ThreadId};
    use wicked_apps_core::open_store;

    const NOW: i64 = 1_800_000_000_000;

    struct ProbeRunner {
        actor_thread: ThreadId,
        answer: &'static str,
    }

    impl StepRunner for ProbeRunner {
        fn run_unit(&self, input: &StepInput) -> StepOutput {
            assert_ne!(thread::current().id(), self.actor_thread);
            assert_eq!(input.unit.id, crate::validator::REPROBE_UNIT);
            assert_eq!(input.unit.assigned_cli.as_deref(), Some("codex"));
            assert!(input.workdir.is_none() && input.governance.is_none());
            StepOutput {
                run_id: input.run_id.clone(),
                unit_ix: input.unit_ix,
                attempt: input.attempt,
                output: self.answer.into(),
                status: StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }
    }

    fn runner(answer: &'static str) -> Arc<dyn StepRunner> {
        Arc::new(ProbeRunner {
            actor_thread: thread::current().id(),
            answer,
        })
    }

    fn registry() -> Vec<crate::AgenticCli> {
        vec![serde_json::from_value(serde_json::json!({
            "key": "codex",
            "display_name": "Codex",
            "binary": "unused",
            "headless_invocation": "unused {PROMPT}"
        }))
        .unwrap()]
    }

    fn seed(store: &mut dyn GraphStore, run_id: &str) {
        let session = AgentSession {
            intent_amendments: Vec::new(),
            id: run_id.into(),
            workflow_id: format!("wf-{run_id}"),
            problem: "p".into(),
            entity_mode: EntityMode::Shared,
            collection_scope: None,
            clis: vec!["codex".into(), "claude".into()],
            status: SessionStatus::AwaitingHuman,
            human_confirm: HumanConfirm::None,
            auto_deliver: false,
            unit_ix: 0,
            attempt: 0,
            workdir: None,
            repo_ref: None,
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            optional_read_roots: Vec::new(),
            project_graph: None,
            project_id: None,
            archived_at: None,
            archive_note: None,
            verified_tree: None,
            run_branch: None,
            base_commit: None,
            finished_at: None,
            seat_failures: Vec::new(),
            benched_seats: vec![BenchedSeat {
                cli: "codex".into(),
                reason: "not_logged_in".into(),
                source: "launcher".into(),
                governed_only: false,
                benched_until: Some(NOW),
                strikes: 1,
                replaced: None,
            }],
            team: None,
            team_plan: Some(crate::plan_gate::TeamPlanState {
                roster: vec![serde_json::json!({
                    "key": "codex",
                    "health": {"usable": false, "reason": "not_logged_in"}
                })],
                ..Default::default()
            }),
            exclude_seats: Vec::new(),
            evidence_root: None,
            assurance: Default::default(),
            power: None,
            run_context: None,
        };
        put_node(store, session.to_node()).unwrap();
        // The cursor is an index, not its ordinal.
        let mut unit = WorkUnit::pending(format!("{run_id}:work"), run_id, 7, "build it");
        unit.assigned_cli = Some("codex".into());
        unit.status = UnitStatus::Rejected;
        unit.denial = Some(UnitDenial::new(
            crate::domain::DENIAL_SOURCE_DEAD_SEAT,
            "signed out",
        ));
        put_node(store, unit.to_node()).unwrap();
    }

    fn take_probe(rx: &Receiver<Command>) -> (String, Vec<crate::seat_bench::BenchReprobe>) {
        match rx
            .recv_timeout(Duration::from_secs(5))
            .expect("probe finished")
        {
            Command::DeadSeatReprobed { run_id, reprobes } => (run_id, reprobes),
            _ => panic!("expected DeadSeatReprobed"),
        }
    }

    #[test]
    fn answering_due_seat_is_lifted_and_approved_on_its_parked_unit() {
        let mut store = open_store(Some(":memory:")).unwrap();
        seed(&mut store, "returned");
        let (tx, rx) = channel();
        let mut probing = HashSet::new();
        sweep(&store, &runner("READY"), registry, &tx, &mut probing, NOW).unwrap();
        assert!(probing.contains("returned"));
        let (run_id, reprobes) = take_probe(&rx);
        assert_eq!(run_id, "returned");
        assert_eq!(reprobes.len(), 1);
        assert!(reprobes[0].returned);
        let mut subs = crate::event_log::EventSink::default();
        let (etx, erx) = channel();
        subs.push(etx);
        fold(&mut store, &mut subs, &tx, &mut probing, &run_id, &reprobes).unwrap();
        assert!(probing.is_empty());
        let session = crate::domain::get_session(&store, &run_id)
            .unwrap()
            .unwrap();
        assert!(session.benched_seats.is_empty());
        assert_eq!(
            session.team_plan.unwrap().roster[0]["health"]["usable"],
            true
        );
        assert!(matches!(
            erx.try_recv().unwrap(),
            CoreEvent::SeatBenchLifted { session, ord: 7, cli, reason }
                if session == run_id && cli == "codex" && reason == "not_logged_in"
        ));
        match rx.try_recv().expect("approve queued") {
            Command::ConfirmGate {
                run_id: id,
                decision,
                actor,
                expect,
                ..
            } => {
                assert_eq!(id, run_id);
                assert!(matches!(
                    decision,
                    HumanDecision::Approve { amend: None, amend_scope }
                        if amend_scope == crate::workflow::AmendScope::Cursor
                ));
                assert_eq!(actor.as_deref(), Some("engine:bench-reprobe"));
                assert_eq!(
                    expect,
                    Some(crate::workflow::GateExpect {
                        ord: Some(7),
                        plan_rev: None,
                        gate_seq: None,
                    })
                );
            }
            _ => panic!("expected ConfirmGate"),
        }
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn panicking_probe_still_posts_completion_and_clears_probing() {
        struct PanickingRunner;

        impl StepRunner for PanickingRunner {
            fn run_unit(&self, _input: &StepInput) -> StepOutput {
                panic!("probe runner panicked");
            }
        }

        let mut store = open_store(Some(":memory:")).unwrap();
        seed(&mut store, "panicked");
        let (tx, rx) = channel();
        let mut probing = HashSet::new();
        let runner: Arc<dyn StepRunner> = Arc::new(PanickingRunner);
        sweep(&store, &runner, registry, &tx, &mut probing, NOW).unwrap();
        assert!(probing.contains("panicked"));
        let (run_id, reprobes) = take_probe(&rx);
        assert_eq!(run_id, "panicked");
        assert!(reprobes.is_empty());

        let mut subs = crate::event_log::EventSink::default();
        fold(&mut store, &mut subs, &tx, &mut probing, &run_id, &reprobes).unwrap();
        assert!(probing.is_empty());
        let session = crate::domain::get_session(&store, &run_id)
            .unwrap()
            .unwrap();
        assert_eq!(session.status, SessionStatus::AwaitingHuman);
        assert_eq!(session.benched_seats.len(), 1);
        assert_eq!(session.benched_seats[0].strikes, 1);
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn failed_probe_increases_backoff_and_immediate_sweep_does_not_spin() {
        let mut store = open_store(Some(":memory:")).unwrap();
        seed(&mut store, "failed");
        let (tx, rx) = channel();
        let mut probing = HashSet::new();
        let runner = runner("not logged in");
        sweep(&store, &runner, registry, &tx, &mut probing, NOW).unwrap();
        let (run_id, reprobes) = take_probe(&rx);
        let mut subs = crate::event_log::EventSink::default();
        let (etx, erx) = channel();
        subs.push(etx);
        fold(&mut store, &mut subs, &tx, &mut probing, &run_id, &reprobes).unwrap();
        let session = crate::domain::get_session(&store, &run_id)
            .unwrap()
            .unwrap();
        assert_eq!(session.status, SessionStatus::AwaitingHuman);
        assert_eq!(session.benched_seats.len(), 1);
        assert_eq!(session.benched_seats[0].strikes, 2);
        assert!(session.benched_seats[0].benched_until.unwrap() > NOW);
        assert!(matches!(
            erx.try_recv().unwrap(),
            CoreEvent::SeatBenched { ord: 7, .. }
        ));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        sweep(&store, &runner, registry, &tx, &mut probing, NOW).unwrap();
        assert!(probing.is_empty());
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn sweep_leaves_not_due_other_gates_other_statuses_and_inflight_probes_alone() {
        let mut store = open_store(Some(":memory:")).unwrap();
        for id in ["future", "other-gate", "executing", "probing", "permanent"] {
            seed(&mut store, id);
            let mut session = crate::domain::get_session(&store, id).unwrap().unwrap();
            match id {
                "future" => session.benched_seats[0].benched_until = Some(NOW + 60_000),
                "executing" => session.status = SessionStatus::Executing,
                "permanent" => session.benched_seats[0].reason = "not_installed".into(),
                "other-gate" => {
                    let mut unit = crate::domain::session_units(&store, id).unwrap().remove(0);
                    unit.denial = Some(UnitDenial::new("agent_validator", "review failed"));
                    put_node(&mut store, unit.to_node()).unwrap();
                }
                _ => {}
            }
            put_node(&mut store, session.to_node()).unwrap();
        }
        let (tx, rx) = channel();
        let mut probing = HashSet::from(["probing".to_string()]);
        sweep(&store, &runner("READY"), registry, &tx, &mut probing, NOW).unwrap();
        assert_eq!(probing, HashSet::from(["probing".to_string()]));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn fold_drops_answers_after_the_run_or_gate_moves_on() {
        for change in ["answered", "cancelled", "other-gate"] {
            let mut store = open_store(Some(":memory:")).unwrap();
            seed(&mut store, change);
            let (tx, rx) = channel();
            let mut probing = HashSet::new();
            sweep(&store, &runner("READY"), registry, &tx, &mut probing, NOW).unwrap();
            let (run_id, reprobes) = take_probe(&rx);
            let mut session = crate::domain::get_session(&store, &run_id)
                .unwrap()
                .unwrap();
            if change == "other-gate" {
                let mut unit = crate::domain::session_units(&store, &run_id)
                    .unwrap()
                    .remove(0);
                unit.denial = Some(UnitDenial::new("agent_validator", "review failed"));
                put_node(&mut store, unit.to_node()).unwrap();
            } else {
                session.status = if change == "answered" {
                    SessionStatus::Executing
                } else {
                    SessionStatus::Cancelled
                };
                put_node(&mut store, session.to_node()).unwrap();
            }
            let mut subs = crate::event_log::EventSink::default();
            let (etx, erx) = channel();
            subs.push(etx);
            fold(&mut store, &mut subs, &tx, &mut probing, &run_id, &reprobes).unwrap();
            assert!(probing.is_empty());
            let after = crate::domain::get_session(&store, &run_id)
                .unwrap()
                .unwrap();
            assert_eq!(after.benched_seats, session.benched_seats);
            assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
            assert!(matches!(erx.try_recv(), Err(TryRecvError::Empty)));
        }
    }

    #[test]
    fn returning_another_seat_does_not_reassign_or_approve_the_parked_unit() {
        let mut store = open_store(Some(":memory:")).unwrap();
        seed(&mut store, "other-seat");
        let mut unit = crate::domain::session_units(&store, "other-seat")
            .unwrap()
            .remove(0);
        unit.assigned_cli = Some("claude".into());
        put_node(&mut store, unit.to_node()).unwrap();
        let mut session = crate::domain::get_session(&store, "other-seat")
            .unwrap()
            .unwrap();
        let mut other = session.benched_seats[0].clone();
        other.cli = "claude".into();
        other.benched_until = Some(NOW + 60_000);
        session.benched_seats.push(other);
        put_node(&mut store, session.to_node()).unwrap();
        let (tx, rx) = channel();
        let mut probing = HashSet::new();
        sweep(&store, &runner("READY"), registry, &tx, &mut probing, NOW).unwrap();
        let (run_id, reprobes) = take_probe(&rx);
        let mut subs = crate::event_log::EventSink::default();
        fold(&mut store, &mut subs, &tx, &mut probing, &run_id, &reprobes).unwrap();
        let after = crate::domain::get_session(&store, &run_id)
            .unwrap()
            .unwrap();
        assert_eq!(after.benched_seats.len(), 1);
        assert_eq!(after.benched_seats[0].cli, "claude");
        assert_eq!(after.status, SessionStatus::AwaitingHuman);
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        let unit = crate::domain::session_units(&store, &run_id)
            .unwrap()
            .remove(0);
        assert_eq!(unit.assigned_cli.as_deref(), Some("claude"));
    }

    #[test]
    fn worker_fold_persists_lift_and_launcher_health_once_without_approving_a_gate() {
        let mut store = open_store(Some(":memory:")).unwrap();
        seed(&mut store, "worker");
        let mut session = crate::domain::get_session(&store, "worker")
            .unwrap()
            .unwrap();
        session.status = SessionStatus::Executing;
        put_node(&mut store, session.to_node()).unwrap();
        let reprobes = vec![crate::seat_bench::reprobe_outcome(
            &session.benched_seats[0],
            Ok("READY"),
            NOW,
        )];
        let mut subs = crate::event_log::EventSink::default();
        let (etx, erx) = channel();
        subs.push(etx);
        fold_bench_reprobes(&mut store, &mut subs, &mut session, 7, &reprobes).unwrap();
        let after = crate::domain::get_session(&store, "worker")
            .unwrap()
            .unwrap();
        assert_eq!(after.status, SessionStatus::Executing);
        assert!(after.benched_seats.is_empty());
        assert_eq!(after.team_plan.unwrap().roster[0]["health"]["usable"], true);
        assert!(matches!(
            erx.try_recv().unwrap(),
            CoreEvent::SeatBenchLifted { ord: 7, .. }
        ));
        fold_bench_reprobes(&mut store, &mut subs, &mut session, 7, &reprobes).unwrap();
        assert!(matches!(erx.try_recv(), Err(TryRecvError::Empty)));
    }
}
