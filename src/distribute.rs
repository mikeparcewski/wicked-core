//! DISTRIBUTE — convene `wicked_council` IN-PROCESS to pick the CLI assigned to each unit.
//! Ported into COE from the retired wicked-agent. Each unit: convene the council over the roster,
//! read the verdict; the winner names the seat, else gracefully degrade to the first seat
//! (distribution ALWAYS yields an assignment — never fails a unit).

use std::sync::Arc;
use std::time::Duration;

use wicked_council::dispatch::RealDispatcher;
use wicked_council::{
    ids, work_kind_for, AgenticCli, CouncilTask, EstateHandle, EstateRankStore, Ledger,
    NoopEventSink, PollStatus, RankStore, TaskState, Worker,
};

use crate::domain::WorkUnit;

/// The distribution decision for one unit (positionally aligned with the input units).
#[derive(Debug, Clone)]
pub struct Distribution {
    pub assigned_cli: String,
    pub council_task_ref: Option<String>,
}

const DISTRIBUTE_CRITERIA: &[&str] = &["general"];
const MIN_RANKED_OBS: u32 = 5;
const RANKED_SCORE_THRESHOLD: f32 = 0.80;

/// Convene the council (in-process) for every unit, persisting its task/verdict on the SHARED store
/// at `db_path` so council nodes land on the same file as the rest (R6). Sequential — each
/// `queue_blocking` joins its worker before the next, so the council never writes concurrently.
pub fn distribute_units_on(
    units: &[WorkUnit],
    clis: &[AgenticCli],
    session_id: &str,
    db_path: Option<&str>,
) -> anyhow::Result<Vec<Distribution>> {
    let roster_keys: Vec<String> = clis.iter().map(|c| c.key.clone()).collect();
    units
        .iter()
        .map(|unit| distribute_one(unit, clis, &roster_keys, session_id, db_path))
        .collect()
}

fn distribute_one(
    unit: &WorkUnit,
    clis: &[AgenticCli],
    roster_keys: &[String],
    session_id: &str,
    db_path: Option<&str>,
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

    // Ranked fast path: skip convening if historical evidence is strong enough. Governance still
    // fires in execute — ranking bypasses distribution, NOT governance (ADR-0003).
    let criteria: Vec<String> = DISTRIBUTE_CRITERIA.iter().map(|s| s.to_string()).collect();
    let work_kind = work_kind_for(&criteria);
    if let Some(best) = rank_store.best_for(&work_kind, 1).into_iter().next() {
        if best.n >= MIN_RANKED_OBS
            && best.score >= RANKED_SCORE_THRESHOLD
            && roster_keys.contains(&best.cli)
        {
            return Ok(Distribution {
                assigned_cli: best.cli,
                council_task_ref: None,
            });
        }
    }
    let dispatcher = Arc::new(RealDispatcher {
        timeout: Duration::from_secs(30),
        local_runner_timeout: Duration::from_secs(30),
    });
    let worker = Worker::new(
        ledger,
        dispatcher,
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
    let (assigned_cli, _degraded) = pick_assignment(status.as_ref(), roster_keys);

    Ok(Distribution {
        assigned_cli,
        council_task_ref: Some(task_id),
    })
}

/// Pick the assigned CLI from the council's poll status: the verdict winner if it names a roster
/// seat, else gracefully degrade to the first seat.
fn pick_assignment(status: Option<&PollStatus>, roster_keys: &[String]) -> (String, bool) {
    let fallback = || {
        roster_keys
            .first()
            .cloned()
            .unwrap_or_else(|| "claude".to_string())
    };

    let Some(status) = status else {
        return (fallback(), true);
    };
    if status.state != TaskState::Voted {
        return (fallback(), true);
    }
    let Some(verdict) = &status.verdict else {
        return (fallback(), true);
    };
    let Some(winner) = &verdict.winning_recommendation else {
        return (fallback(), true);
    };

    let winner_norm = winner.to_lowercase();
    if let Some(seat) = roster_keys
        .iter()
        .find(|k| winner_norm == k.to_lowercase() || winner_norm.contains(&k.to_lowercase()))
    {
        (seat.clone(), false)
    } else {
        (fallback(), true)
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
    fn winner_matching_a_seat_is_assigned() {
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let st = status_with_winner(Some("fake-b"), TaskState::Voted);
        let (cli, degraded) = pick_assignment(Some(&st), &roster);
        assert_eq!(cli, "fake-b");
        assert!(!degraded);
    }

    #[test]
    fn no_match_degrades_to_first_seat() {
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let (cli, degraded) = pick_assignment(None, &roster);
        assert_eq!(cli, "fake-a");
        assert!(degraded);
        let st = status_with_winner(Some("Option Z"), TaskState::Voted);
        let (cli, degraded) = pick_assignment(Some(&st), &roster);
        assert_eq!(cli, "fake-a");
        assert!(degraded);
    }
}
