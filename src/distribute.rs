//! DISTRIBUTE — convene `wicked_council` IN-PROCESS to pick the CLI assigned to each unit.
//! Ported into COE from the retired wicked-agent. Each unit: convene the council over the roster,
//! read the verdict; the winner names the seat, else gracefully degrade to the first seat
//! (distribution ALWAYS yields an assignment — never fails a unit).

use std::sync::Arc;

use wicked_council::dispatch::RealDispatcher;
use wicked_council::types::Dispatcher;
use wicked_council::{
    ids, work_kind_for, AgenticCli, CouncilTask, EstateHandle, EstateRankStore, Ledger,
    NoopEventSink, PollStatus, TaskState, Worker,
};

use crate::domain::{RoutingInfo, WorkUnit};
use crate::event::CoreEvent;

/// The production dispatcher — spawns real CLI subprocesses to collect council votes. Injected so
/// tests can substitute a deterministic stub (no subprocess, no flaky dispatch).
///
/// The budgets come from `RealDispatcher::from_env` rather than being written here. This function
/// used to hardcode 30 s for both, which is half the library's own default and below what any
/// shipped seat needs to answer a ballot — every seat was killed mid-reasoning and the council
/// degraded on 25 of 27 units (FINDING-026). A budget stated in one place cannot silently
/// contradict the one the library documents.
pub fn real_dispatcher() -> Arc<dyn Dispatcher + Send + Sync> {
    Arc::new(RealDispatcher::from_env())
}

/// Fans council lifecycle events back to the actor's single emit point (via
/// `Command::EmitEvent`), making deliberation visible to subscribers while a vote is
/// still in flight. `None` keeps the historical silent behaviour (tests, straight-line
/// pipeline callers).
pub type EventRelay = Arc<dyn Fn(CoreEvent) + Send + Sync>;

/// Adapts the council's string-keyed `EventSink` to run-scoped [`CoreEvent`]s. The council
/// worker emits `EV_COUNCIL_REQUESTED` when voters are polled, `EV_COUNCIL_DELIBERATED`
/// after each below-bar runoff ballot, and `EV_COUNCIL_VOTED` when the verdict lands; all
/// three are translated with the owning (session, ord) attached so the UI can pin them to
/// the unit being distributed. Rank bookkeeping (`EV_CLI_RANKED`) and any future council
/// event types are intentionally dropped — they are not run-scoped.
struct RelaySink {
    relay: EventRelay,
    session: String,
    ord: u32,
}

impl wicked_council::EventSink for RelaySink {
    fn emit(&self, event: &str, payload: &serde_json::Value) {
        let ev = match event {
            wicked_apps_core::EV_COUNCIL_REQUESTED => CoreEvent::CouncilConvened {
                session: self.session.clone(),
                ord: self.ord,
                clis: payload["clis"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            wicked_apps_core::EV_COUNCIL_DELIBERATED => CoreEvent::CouncilDeliberated {
                session: self.session.clone(),
                ord: self.ord,
                round: payload["round"].as_u64().unwrap_or(0) as u32,
                agreement_pct: (payload["agreement_ratio"]
                    .as_f64()
                    .unwrap_or(0.0)
                    .clamp(0.0, 1.0)
                    * 100.0)
                    .round() as u8,
                needed_pct: (payload["threshold"].as_f64().unwrap_or(0.0).clamp(0.0, 1.0) * 100.0)
                    .round() as u8,
                votes: payload["votes"].as_u64().unwrap_or(0) as u32,
            },
            wicked_apps_core::EV_COUNCIL_SEAT_FAILED => CoreEvent::CouncilSeatFailed {
                session: self.session.clone(),
                ord: self.ord,
                round: payload["round"].as_u64().unwrap_or(0) as u32,
                cli: payload["cli"].as_str().unwrap_or_default().to_string(),
                kind: payload["kind"].as_str().unwrap_or("unreported").to_string(),
                // `as_i64` is None for a JSON null (a seat that never reached exit), which is
                // exactly the case `exit_code: None` represents.
                exit_code: payload["exit_code"].as_i64().map(|c| c as i32),
                stderr: payload["stderr"].as_str().unwrap_or_default().to_string(),
                detail: payload["detail"].as_str().unwrap_or_default().to_string(),
                // Separates a seat that never started from one that burned the whole budget —
                // the two look identical without it.
                latency_ms: payload["latency_ms"].as_u64().unwrap_or(0),
            },
            wicked_apps_core::EV_COUNCIL_VOTED => CoreEvent::CouncilVoted {
                session: self.session.clone(),
                ord: self.ord,
                consensus: payload["consensus"].as_bool().unwrap_or(false),
                agreement_pct: (payload["agreement_ratio"]
                    .as_f64()
                    .unwrap_or(0.0)
                    .clamp(0.0, 1.0)
                    * 100.0)
                    .round() as u8,
                votes: payload["votes"].as_u64().unwrap_or(0) as u32,
                // `map`, not `unwrap_or`: an absent key means the emitter reported no seat count,
                // and both candidate sentinels lie — `0` is an impossible denominator a consumer
                // could divide by, and `votes` would state that every seat answered. The live
                // emitter always sends it (same binary), so this only decides how a replayed or
                // hand-built payload reads.
                seated: payload["seated"].as_u64().map(|s| s as u32),
            },
            _ => return,
        };
        (self.relay)(ev);
    }
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
/// at `db_path` so council nodes land on the same file as the rest (R6). Units are dispatched in
/// parallel via `std::thread::scope`; each spawns its own in-memory council estate, so there is no
/// shared SQLite state and no concurrent-write hazard. (If `db_path` is `Some`, multiple threads
/// would open the same file — currently `db_path` is always `None` from the actor; callers passing
/// a file path should be aware of the SQLite single-writer constraint.)
pub fn distribute_units_on(
    units: &[WorkUnit],
    clis: &[AgenticCli],
    session_id: &str,
    db_path: Option<&str>,
    dispatcher: &Arc<dyn Dispatcher + Send + Sync>,
    relay: Option<EventRelay>,
) -> anyhow::Result<Vec<Distribution>> {
    let roster_keys: Vec<String> = clis.iter().map(|c| c.key.clone()).collect();
    let mut dists: Vec<Distribution> = std::thread::scope(|s| {
        // Spawn all units concurrently. Scoped-thread closures borrow from the enclosing
        // scope — `std::thread::scope` guarantees all threads finish before it returns,
        // making the borrows sound without requiring `move`.
        let relay = &relay;
        let handles: Vec<_> = units
            .iter()
            .map(|unit| {
                s.spawn(|| {
                    if unit.tool_cmd.is_some() {
                        Ok(Distribution {
                            assigned_cli: unit
                                .tool_cmd
                                .as_ref()
                                .and_then(|c| c.first())
                                .cloned()
                                .unwrap_or_else(|| "__tool__".to_string()),
                            assigned_invocation: None,
                            council_task_ref: None,
                            routing: RoutingInfo::Tool,
                        })
                    } else {
                        distribute_one(
                            unit,
                            clis,
                            &roster_keys,
                            session_id,
                            db_path,
                            dispatcher,
                            relay.clone(),
                        )
                    }
                })
            })
            .collect();
        // Join ALL handles before inspecting results. Early-returning on the first join error
        // would drop remaining handles, letting the scope re-propagate their panics and
        // bypass the intended `anyhow::Error` mapping.
        let results: Vec<_> = handles.into_iter().map(|h| h.join()).collect();
        results
            .into_iter()
            .map(|r| {
                r.map_err(|e| {
                    let msg = e
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| e.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "council thread panicked".to_string());
                    anyhow::anyhow!(msg)
                })
                .and_then(|r| r)
            })
            .collect::<anyhow::Result<_>>()
    })?;
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
    // Warn when every roster seat is a builder CLI so operators can detect degraded separation.
    // `find` below will return `None` for every Review/Test unit in this configuration, leaving
    // them on their original (builder) CLI with no routing change — silently, unless we speak up.
    let has_evaluator_seat = roster_keys.iter().any(|k| !builder_clis.contains(k));
    if !has_evaluator_seat {
        let review_test_affected = units.iter().zip(dists.iter()).any(|(u, d)| {
            u.tool_cmd.is_none()
                && matches!(u.stage, StageKind::Review | StageKind::Test)
                && builder_clis.contains(&d.assigned_cli)
        });
        if review_test_affected {
            eprintln!(
                "wicked-core: evaluator\u{2260}creator separation cannot be enforced: all roster \
                 seats are assigned Build/Recon phases; Review/Test phases will use builder CLIs. \
                 Consider adding a dedicated evaluator seat."
            );
        }
    }
    for (u, d) in units.iter().zip(dists.iter_mut()) {
        if u.tool_cmd.is_some() {
            continue; // Tool phases have no CLI to distinct
        }
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
    relay: Option<EventRelay>,
) -> anyhow::Result<Distribution> {
    // FINDING-010: a single-seat roster has nothing to elect. Convening a council here still queues a
    // ballot and dispatches it to the sole CLI — a real subprocess turn spent asking one voter to pick
    // the one option, ~30s of dead wall-clock per unit for a foregone conclusion (and a councilConvened
    // the operator must then read past). Short-circuit: assign the only seat directly and record a
    // TRUTHFUL one-seat verdict (1 of 1, 100% agreement, no dissent). No ballot, no dispatch, no
    // council estate — the dispatcher is never touched. `enforce_evaluator_distinct` is a no-op at
    // len < 2, so nothing downstream depends on the council having run here.
    if let [only] = clis {
        return Ok(Distribution {
            assigned_invocation: invocation_of(clis, &only.key),
            assigned_cli: only.key.clone(),
            council_task_ref: None,
            routing: RoutingInfo::Council {
                winner: only.key.clone(),
                agreement_pct: 100,
                returned: 1,
                seated: Some(1),
                dissent: 0,
            },
        });
    }

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
    // Council lifecycle events flow to the actor's emit point when a relay is armed;
    // otherwise deliberation stays silent (the pre-relay behaviour).
    let events: Arc<dyn wicked_council::EventSink + Send + Sync> = match relay {
        Some(relay) => Arc::new(RelaySink {
            relay,
            session: session_id.to_string(),
            ord: unit.ord,
        }),
        None => Arc::new(NoopEventSink),
    };

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
        events,
        clis.to_vec(),
        work_kind,
    );

    // Build numbered capability profiles — CLI names are NEVER exposed to voters.
    // Each voter sees only the capability description and picks a number, preventing
    // self-selection bias (a CLI knowing its own name will recommend itself).
    let cap_map: Vec<(String, String)> = clis
        .iter()
        .map(|c| {
            let label = c
                .capabilities
                .as_deref()
                .unwrap_or(&c.display_name)
                .to_string();
            (label, c.key.clone())
        })
        .collect();
    let option_labels: Vec<String> = cap_map.iter().map(|(label, _)| label.clone()).collect();

    let task = CouncilTask {
        id: ids::new_task_id(),
        topic: format!(
            "A software task needs an agent to execute it.\n\
             Task description: {}\n\
             Which numbered capability profile is the best fit?",
            unit.description
        ),
        options: option_labels,
        criteria,
        session_id: session_id.to_string(),
    };
    let task_id = worker.queue_blocking(task);
    let status: Option<PollStatus> = worker.poll(&task_id);
    let (assigned_cli, routing) = route_from_status(status.as_ref(), roster_keys, &cap_map);

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

/// Why a council in a non-`Voted` state produced nothing, as specifically as the record allows.
///
/// This replaces the single string `"council did not reach a vote"`, which was emitted for
/// `TimedOut`, `Failed`, `Queued` and `Running` alike and named no seat. A campaign of 27
/// convened councils degraded 25 times, every one of them reporting that same sentence — there
/// was nothing in it to act on.
///
/// Three levels, most specific first: the seats' own reported failures; failing that, the
/// council's OWN failure (a panic in synthesis or emission belongs to no seat, so it has no
/// entry in `seat_failures` — without this it reproduced the same undiagnosable string the
/// finding exists to eliminate, FINDING-026 E); failing that, the lifecycle state (which at
/// least distinguishes "ran out of time" from "could not start" from "still running when
/// polled"). The state is always named so the levels are never confused.
fn no_vote_reason(status: &PollStatus) -> String {
    let state = match status.state {
        TaskState::Queued => "never started (still queued when polled)",
        TaskState::Running => "still running when polled",
        TaskState::TimedOut => "no seat returned a vote",
        TaskState::Failed => "could not run",
        // `Voted` does not reach here — the caller checks for it first.
        TaskState::Voted => "reported a vote but was not in the voted state",
    };

    if status.seat_failures.is_empty() {
        return match &status.failure_detail {
            Some(detail) => format!("council {state} — the council itself failed: {detail}"),
            None => format!("council {state} (no per-seat reason recorded)"),
        };
    }

    let seats: Vec<String> = status
        .seat_failures
        .iter()
        .map(|f| format!("{}: {}", f.cli, f.failure.reason()))
        .collect();
    let seats = format!("council {state} — {}", seats.join("; "));
    // Both can be present: seats failed AND the council then unwound. Neither explains the
    // other, so neither is dropped.
    match &status.failure_detail {
        Some(detail) => format!("{seats}; the council itself failed: {detail}"),
        None => seats,
    }
}

/// Resolve the assigned CLI from the council's poll status AND the routing provenance.
///
/// Voters respond with a **capability-profile number** (e.g. "2"), never a CLI name.
/// We parse the leading integer from the winning recommendation, use it as a 1-based
/// index into `cap_map` (ordered `(capability_label, cli_key)`), and fall back
/// gracefully to the first seat if the number is missing or out of range.
fn route_from_status(
    status: Option<&PollStatus>,
    roster_keys: &[String],
    cap_map: &[(String, String)],
) -> (String, RoutingInfo) {
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
        return degrade(&no_vote_reason(status));
    }
    let Some(verdict) = &status.verdict else {
        return degrade("council produced no verdict");
    };
    let Some(winner) = &verdict.winning_recommendation else {
        return degrade("verdict named no winner");
    };

    // Parse the leading integer from the recommendation text (voters are told to lead with
    // the option number). "2 — broad reasoning..." → 2 → index 1.
    let idx_opt = winner
        .trim()
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|tok| tok.parse::<usize>().ok())
        .filter(|&n| n >= 1 && n <= cap_map.len())
        .map(|n| n - 1); // convert to 0-based

    if let Some(idx) = idx_opt {
        let seat = cap_map[idx].1.clone();
        // Confirm the seat exists in the roster (cap_map may be a superset if a CLI was
        // added after roster construction — degrade rather than assign an unknown key).
        if roster_keys.iter().any(|k| k == &seat) {
            return (
                seat.clone(),
                RoutingInfo::Council {
                    winner: seat,
                    agreement_pct: pct(verdict.agreement_ratio),
                    returned: status.returned,
                    // Off the STATUS, not the verdict: the verdict's own `seated` degrades to the
                    // cast count when it was not recorded, while the status counts the seats the
                    // ledger actually convened. They agree on every live path; where they don't,
                    // the ledger is the one that observed the council.
                    seated: Some(status.seated),
                    dissent: verdict.dissent.len() as u32,
                },
            );
        }
    }

    degrade(&format!(
        "recommendation '{winner}' did not resolve to a roster seat"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wicked_council::types::{Category, Confidence, InputMode, Vote};
    use wicked_council::{CouncilTask, Verdict};

    /// Counts every `dispatch` — a ballot dispatched to a CLI is a real subprocess turn. A
    /// short-circuited single-seat roster must never reach it.
    struct SpyDispatcher {
        calls: Arc<AtomicUsize>,
    }
    impl Dispatcher for SpyDispatcher {
        fn dispatch(&self, cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Some(Vote {
                cli: cli.key.clone(),
                // "1 …" resolves to option 1 so the >1-seat control path reaches a real verdict.
                recommendation: "1 — fit".into(),
                top_risk: "none".into(),
                change_my_mind: "no".into(),
                disqualifier: None,
                confidence: Confidence::default(),
                provenance: "spy".into(),
            })
        }
    }

    fn seat(key: &str) -> AgenticCli {
        AgenticCli {
            key: key.into(),
            display_name: key.into(),
            binary: "unused".into(),
            headless_invocation: format!("run-{key} {{PROMPT}}"),
            category: Category::default(),
            input_mode: InputMode::default(),
            version_probe: vec![],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::default(),
            enabled_for_council: true,
            acp: None,
            capabilities: Some(format!("{key} capabilities")),
        }
    }

    /// FINDING-010: a single-seat roster is assigned WITHOUT convening a council — the dispatcher is
    /// never called (no ballot subprocess), and the routing is a truthful 1-of-1 verdict. The 2-seat
    /// control proves the guard is scoped to len==1: there, the council DOES convene (dispatcher hit).
    /// Mutation: delete the `if let [only] = clis` short-circuit and the single-seat case dispatches
    /// (calls > 0), failing the first assertion.
    #[test]
    fn a_single_seat_roster_skips_the_council_and_dispatches_nothing() {
        let unit = WorkUnit::pending("u1", "s1", 0, "Write the parser module");

        // Single seat → short-circuit.
        let calls = Arc::new(AtomicUsize::new(0));
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(SpyDispatcher {
            calls: calls.clone(),
        });
        let dists = distribute_units_on(
            std::slice::from_ref(&unit),
            &[seat("solo")],
            "s1",
            None,
            &dispatcher,
            None,
        )
        .expect("distribute a single-seat roster");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a one-seat roster must NOT dispatch a ballot"
        );
        assert_eq!(dists.len(), 1);
        assert_eq!(dists[0].assigned_cli, "solo");
        assert_eq!(
            dists[0].assigned_invocation.as_deref(),
            Some("run-solo {PROMPT}")
        );
        assert!(
            dists[0].council_task_ref.is_none(),
            "no council task convened"
        );
        assert!(
            matches!(&dists[0].routing, RoutingInfo::Council { winner, agreement_pct, returned, seated, dissent }
                if winner == "solo" && *agreement_pct == 100 && *returned == 1 && *seated == Some(1) && *dissent == 0),
            "single seat records a truthful 1-of-1 verdict, got {:?}",
            dists[0].routing
        );

        // Two seats → the council genuinely convenes (guard is scoped to len==1).
        let calls2 = Arc::new(AtomicUsize::new(0));
        let dispatcher2: Arc<dyn Dispatcher + Send + Sync> = Arc::new(SpyDispatcher {
            calls: calls2.clone(),
        });
        let _ = distribute_units_on(
            &[unit],
            &[seat("alpha"), seat("beta")],
            "s1",
            None,
            &dispatcher2,
            None,
        )
        .expect("distribute a two-seat roster");
        assert!(
            calls2.load(Ordering::SeqCst) >= 1,
            "a multi-seat roster still convenes a council (dispatches ballots)"
        );
    }

    fn status_with_winner(winner: Option<&str>, state: TaskState) -> PollStatus {
        PollStatus {
            task_id: "t".into(),
            state,
            returned: 1,
            seated: 1,
            pending: 0,
            verdict: winner.map(|w| Verdict {
                task_id: "t".into(),
                kind: "Consensus".into(),
                consensus: true,
                seated: 1,
                winning_recommendation: Some(w.to_string()),
                agreement_ratio: 1.0,
                risk_convergence: vec![],
                dissent: vec![],
            }),
            seat_failures: vec![],
            failure_detail: None,
        }
    }

    fn cap_map(keys: &[&str]) -> Vec<(String, String)> {
        keys.iter()
            .map(|k| (format!("{k}-capabilities"), k.to_string()))
            .collect()
    }

    #[test]
    fn option_number_selects_correct_seat() {
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let map = cap_map(&["fake-a", "fake-b"]);
        // "2 — rationale" → index 1 → fake-b
        let st = status_with_winner(Some("2 — best fit for this task"), TaskState::Voted);
        let (cli, routing) = route_from_status(Some(&st), &roster, &map);
        assert_eq!(cli, "fake-b");
        assert!(
            matches!(&routing, RoutingInfo::Council { winner, agreement_pct, .. }
                if winner.as_str() == "fake-b" && *agreement_pct == 100),
            "option-2 winner maps to fake-b with Council provenance, got {routing:?}"
        );
    }

    #[test]
    fn bare_number_also_resolves() {
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let map = cap_map(&["fake-a", "fake-b"]);
        let st = status_with_winner(Some("1"), TaskState::Voted);
        let (cli, _) = route_from_status(Some(&st), &roster, &map);
        assert_eq!(cli, "fake-a");
    }

    #[test]
    fn no_status_degrades_to_first_seat() {
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let map = cap_map(&["fake-a", "fake-b"]);
        let (cli, routing) = route_from_status(None, &roster, &map);
        assert_eq!(cli, "fake-a");
        assert!(matches!(routing, RoutingInfo::Degraded { .. }));
    }

    #[test]
    fn out_of_range_number_degrades() {
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let map = cap_map(&["fake-a", "fake-b"]);
        // 99 is out of range for a 2-option map
        let st = status_with_winner(Some("99 — some rationale"), TaskState::Voted);
        let (cli, routing) = route_from_status(Some(&st), &roster, &map);
        assert_eq!(cli, "fake-a");
        assert!(
            matches!(&routing, RoutingInfo::Degraded { reason } if reason.contains("99")),
            "out-of-range option degrades with the recommendation in the reason, got {routing:?}"
        );
    }

    #[test]
    fn non_numeric_recommendation_degrades() {
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let map = cap_map(&["fake-a", "fake-b"]);
        let st = status_with_winner(Some("Option Z"), TaskState::Voted);
        let (cli, routing) = route_from_status(Some(&st), &roster, &map);
        assert_eq!(cli, "fake-a");
        assert!(
            matches!(&routing, RoutingInfo::Degraded { reason } if reason.contains("Option Z")),
            "non-numeric winner degrades with a reason naming the recommendation, got {routing:?}"
        );
    }

    #[test]
    fn a_council_that_failed_on_its_own_names_that_in_the_degrade_reason() {
        // A panic in synthesis, ranking or emission belongs to no seat, so `seat_failures` is
        // empty and the old text fell through to "(no per-seat reason recorded)" — exactly the
        // undiagnosable string FINDING-026 exists to eliminate, on the one path where the cause
        // WAS captured.
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let map = cap_map(&["fake-a", "fake-b"]);
        let mut st = status_with_winner(None, TaskState::Failed);
        st.failure_detail = Some("rank store exploded".into());

        let (cli, routing) = route_from_status(Some(&st), &roster, &map);
        assert_eq!(cli, "fake-a", "the unit still routes somewhere");
        let RoutingInfo::Degraded { reason } = &routing else {
            panic!("a failed council degrades: {routing:?}");
        };
        assert!(
            reason.contains("rank store exploded"),
            "the council's own failure is the only account of what happened, got {reason:?}"
        );
        assert!(
            !reason.contains("no per-seat reason recorded"),
            "a recorded cause must not be reported as an absent one, got {reason:?}"
        );
    }

    /// RelaySink translates the council's string-keyed events into run-scoped CoreEvents
    /// with the owning (session, ord) attached, and drops non-run-scoped event types.
    #[test]
    fn relay_sink_translates_council_events_and_drops_the_rest() {
        use wicked_council::EventSink;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let relay: EventRelay = Arc::new(move |ev| sink_seen.lock().unwrap().push(ev));
        let sink = RelaySink {
            relay,
            session: "s1".to_string(),
            ord: 3,
        };

        sink.emit(
            wicked_apps_core::EV_COUNCIL_REQUESTED,
            &serde_json::json!({"clis": ["a", "b"], "task_id": "t", "session_id": "s1"}),
        );
        sink.emit(
            wicked_apps_core::EV_COUNCIL_VOTED,
            &serde_json::json!({"consensus": true, "agreement_ratio": 0.5, "votes": 4, "seated": 5}),
        );
        // Not run-scoped — must be dropped, not translated.
        sink.emit(wicked_apps_core::EV_CLI_RANKED, &serde_json::json!({}));

        let events = seen.lock().unwrap();
        assert_eq!(events.len(), 2, "ranked event dropped: {events:?}");
        assert!(
            matches!(&events[0], CoreEvent::CouncilConvened { session, ord, clis }
                if session == "s1" && *ord == 3 && clis == &["a".to_string(), "b".to_string()]),
            "requested → CouncilConvened with run scope, got {:?}",
            events[0]
        );
        assert!(
            matches!(&events[1], CoreEvent::CouncilVoted { session, ord, consensus: true, agreement_pct: 50, votes: 4, seated: Some(5) }
                if session == "s1" && *ord == 3),
            "voted → CouncilVoted with ratio as percent, got {:?}",
            events[1]
        );
    }

    /// Malformed payloads (missing/mistyped fields) degrade to defaults instead of panicking.
    #[test]
    fn relay_sink_survives_malformed_payloads() {
        use wicked_council::EventSink;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let relay: EventRelay = Arc::new(move |ev| sink_seen.lock().unwrap().push(ev));
        let sink = RelaySink {
            relay,
            session: "s1".to_string(),
            ord: 1,
        };

        sink.emit(
            wicked_apps_core::EV_COUNCIL_REQUESTED,
            &serde_json::json!({}),
        );
        sink.emit(
            wicked_apps_core::EV_COUNCIL_VOTED,
            &serde_json::json!({"agreement_ratio": "not a number"}),
        );

        let events = seen.lock().unwrap();
        assert!(
            matches!(&events[0], CoreEvent::CouncilConvened { clis, .. } if clis.is_empty()),
            "missing clis → empty list, got {:?}",
            events[0]
        );
        assert!(
            matches!(
                &events[1],
                CoreEvent::CouncilVoted {
                    consensus: false,
                    agreement_pct: 0,
                    votes: 0,
                    // NOT `Some(0)` and NOT `Some(votes)`. A seat count nobody reported is
                    // unknown: `0` is an impossible denominator a consumer could divide by, and
                    // copying `votes` would assert that every seat answered — the false-complete
                    // reading this field exists to prevent (review on #151).
                    seated: None,
                    ..
                }
            ),
            "mistyped fields → zero defaults, absent seat count → unknown, got {:?}",
            events[1]
        );
    }
}
