//! DISTRIBUTE — convene `wicked_council` IN-PROCESS to pick the CLI assigned to each unit.
//! Ported into COE from the retired wicked-agent. Each unit: convene the council over the roster,
//! read the verdict; the winner names the seat, else gracefully degrade to the first seat
//! (distribution ALWAYS yields an assignment — never fails a unit).

use std::sync::Arc;
use std::time::Duration;

use wicked_council::dispatch::RealDispatcher;
use wicked_council::types::Dispatcher;
use wicked_council::{
    ids, work_kind_for, AgenticCli, CouncilTask, EstateHandle, EstateRankStore, Ledger,
    NoopEventSink, PollStatus, TaskState, Worker,
};

use crate::domain::{RoutingInfo, WorkUnit};

/// The production dispatcher — spawns real CLI subprocesses to collect council votes. Injected so
/// tests can substitute a deterministic stub (no subprocess, no flaky dispatch).
pub fn real_dispatcher() -> Arc<dyn Dispatcher + Send + Sync> {
    Arc::new(RealDispatcher {
        timeout: Duration::from_secs(30),
        local_runner_timeout: Duration::from_secs(30),
    })
}

/// The distribution decision for one unit (positionally aligned with the input units).
#[derive(Debug, Clone)]
pub struct Distribution {
    pub assigned_cli: String,
    /// The assigned CLI's invocation template (so the runner can execute an ad-hoc CLI not in the
    /// registry). Resolved from the launch roster.
    pub assigned_invocation: Option<String>,
    pub council_task_ref: Option<String>,
    /// WHY this CLI won — the council verdict / ranking / degrade, made visible for the UI.
    pub routing: RoutingInfo,
}

/// The invocation template for `key` from the launch roster (`None` if not found).
fn invocation_of(clis: &[AgenticCli], key: &str) -> Option<String> {
    clis.iter()
        .find(|c| c.key == key)
        .map(|c| c.headless_invocation.clone())
        .filter(|s| !s.trim().is_empty())
}

const DISTRIBUTE_CRITERIA: &[&str] = &["general"];

/// Convene the council (in-process) for every unit, persisting its task/verdict on the SHARED store
/// at `db_path` so council nodes land on the same file as the rest (R6). Sequential — each
/// `queue_blocking` joins its worker before the next, so the council never writes concurrently.
pub fn distribute_units_on(
    units: &[WorkUnit],
    clis: &[AgenticCli],
    session_id: &str,
    db_path: Option<&str>,
    dispatcher: &Arc<dyn Dispatcher + Send + Sync>,
) -> anyhow::Result<Vec<Distribution>> {
    let roster_keys: Vec<String> = clis.iter().map(|c| c.key.clone()).collect();
    let mut dists: Vec<Distribution> = units
        .iter()
        .map(|unit| distribute_one(unit, clis, &roster_keys, session_id, db_path, dispatcher))
        .collect::<anyhow::Result<_>>()?;
    enforce_evaluator_distinct(units, &mut dists, &roster_keys, clis);
    Ok(dists)
}

/// METHODOLOGY: evaluator ≠ creator. A REVIEW/TEST unit must not run on a CLI that produced the work
/// it checks, so after distribution we reassign any review/test unit whose council-picked CLI matches
/// a build/recon CLI to a roster seat NOT used for building (when the roster has the seats to do so).
fn enforce_evaluator_distinct(
    units: &[WorkUnit],
    dists: &mut [Distribution],
    roster_keys: &[String],
    clis: &[AgenticCli],
) {
    use crate::domain::StageKind;
    let builder_clis: std::collections::HashSet<String> = units
        .iter()
        .zip(dists.iter())
        .filter(|(u, _)| matches!(u.stage, StageKind::Build | StageKind::Recon))
        .map(|(_, d)| d.assigned_cli.clone())
        .collect();
    if roster_keys.len() < 2 || builder_clis.is_empty() {
        return; // can't distinguish with one seat / nothing built
    }
    for (u, d) in units.iter().zip(dists.iter_mut()) {
        if matches!(u.stage, StageKind::Review | StageKind::Test)
            && builder_clis.contains(&d.assigned_cli)
        {
            if let Some(alt) = roster_keys.iter().find(|k| !builder_clis.contains(*k)) {
                let was = std::mem::replace(&mut d.assigned_cli, alt.clone());
                d.assigned_invocation = invocation_of(clis, alt);
                d.routing = RoutingInfo::EvaluatorDistinct {
                    winner: alt.clone(),
                    was,
                };
            }
        }
    }
}

fn distribute_one(
    unit: &WorkUnit,
    clis: &[AgenticCli],
    roster_keys: &[String],
    session_id: &str,
    db_path: Option<&str>,
    dispatcher: &Arc<dyn Dispatcher + Send + Sync>,
) -> anyhow::Result<Distribution> {
    let estate = match db_path {
        Some(path) => EstateHandle::new(
            wicked_apps_core::SqliteStore::open(path)
                .map_err(|e| anyhow::anyhow!("open council estate on {path}: {e}"))?,
        ),
        None => EstateHandle::in_memory()
            .map_err(|e| anyhow::anyhow!("open council estate handle: {e}"))?,
    };
    let ledger = Ledger::new(estate.clone());
    let rank_store = Arc::new(EstateRankStore::new(estate));

    // NOTE: a historical-ranking fast path once lived here, but distribution always runs with an
    // IN-MEMORY council estate — the single-writer actor owns the only shared-store handle, so we
    // cannot open a second writable one here (`db_path` is always `None` from the pipeline). Rankings
    // therefore never persist across runs, so the fast path could never fire; it was removed rather
    // than ship a `RoutingInfo::Ranked` mode the engine can't actually produce. Every unit convenes.
    let criteria: Vec<String> = DISTRIBUTE_CRITERIA.iter().map(|s| s.to_string()).collect();
    let work_kind = work_kind_for(&criteria);
    let worker = Worker::new(
        ledger,
        dispatcher.clone(),
        rank_store,
        Arc::new(NoopEventSink),
        clis.to_vec(),
        work_kind,
    );

    let task = CouncilTask {
        id: ids::new_task_id(),
        topic: format!(
            "which CLI should own work unit {}: {}",
            unit.id, unit.description
        ),
        options: roster_keys.to_vec(),
        criteria,
        session_id: session_id.to_string(),
    };
    let task_id = worker.queue_blocking(task);
    let status: Option<PollStatus> = worker.poll(&task_id);
    let (assigned_cli, routing) = route_from_status(status.as_ref(), roster_keys);

    Ok(Distribution {
        assigned_invocation: invocation_of(clis, &assigned_cli),
        assigned_cli,
        council_task_ref: Some(task_id),
        routing,
    })
}

/// Clamp a `0.0..=1.0` ratio to an integer percent (keeps the domain `Eq`).
fn pct(x: f32) -> u8 {
    (x.clamp(0.0, 1.0) * 100.0).round() as u8
}

/// Resolve the assigned CLI from the council's poll status AND the routing provenance: the verdict
/// winner if it names a roster seat, else gracefully degrade to the first seat (with the reason).
fn route_from_status(status: Option<&PollStatus>, roster_keys: &[String]) -> (String, RoutingInfo) {
    let fallback = || {
        roster_keys
            .first()
            .cloned()
            .unwrap_or_else(|| "claude".to_string())
    };
    let degrade = |reason: &str| {
        (
            fallback(),
            RoutingInfo::Degraded {
                reason: reason.to_string(),
            },
        )
    };

    let Some(status) = status else {
        return degrade("council returned no status");
    };
    if status.state != TaskState::Voted {
        return degrade("council did not reach a vote");
    }
    let Some(verdict) = &status.verdict else {
        return degrade("council produced no verdict");
    };
    let Some(winner) = &verdict.winning_recommendation else {
        return degrade("verdict named no winner");
    };

    let winner_norm = winner.to_lowercase();
    if let Some(seat) = roster_keys
        .iter()
        .find(|k| winner_norm == k.to_lowercase() || winner_norm.contains(&k.to_lowercase()))
    {
        (
            seat.clone(),
            RoutingInfo::Council {
                winner: seat.clone(),
                agreement_pct: pct(verdict.agreement_ratio),
                returned: status.returned,
                dissent: verdict.dissent.len() as u32,
            },
        )
    } else {
        degrade(&format!("winner '{winner}' is not a roster seat"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wicked_council::Verdict;

    fn status_with_winner(winner: Option<&str>, state: TaskState) -> PollStatus {
        PollStatus {
            task_id: "t".into(),
            state,
            returned: 1,
            pending: 0,
            verdict: winner.map(|w| Verdict {
                task_id: "t".into(),
                kind: "Consensus".into(),
                consensus: true,
                winning_recommendation: Some(w.to_string()),
                agreement_ratio: 1.0,
                risk_convergence: vec![],
                dissent: vec![],
            }),
        }
    }

    #[test]
    fn winner_matching_a_seat_is_assigned_with_council_provenance() {
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let st = status_with_winner(Some("fake-b"), TaskState::Voted);
        let (cli, routing) = route_from_status(Some(&st), &roster);
        assert_eq!(cli, "fake-b");
        assert!(
            matches!(&routing, RoutingInfo::Council { winner, agreement_pct, .. } if winner.as_str() == "fake-b" && *agreement_pct == 100),
            "a matched winner records Council provenance, got {routing:?}"
        );
    }

    #[test]
    fn no_match_degrades_to_first_seat_with_reason() {
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let (cli, routing) = route_from_status(None, &roster);
        assert_eq!(cli, "fake-a");
        assert!(matches!(routing, RoutingInfo::Degraded { .. }));
        let st = status_with_winner(Some("Option Z"), TaskState::Voted);
        let (cli, routing) = route_from_status(Some(&st), &roster);
        assert_eq!(cli, "fake-a");
        assert!(
            matches!(&routing, RoutingInfo::Degraded { reason } if reason.contains("Option Z")),
            "a non-roster winner degrades with a reason naming the winner, got {routing:?}"
        );
    }
}
