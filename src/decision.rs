//! DECISION COUNCIL (core#590 S5) — convene `wicked_council` IN-PROCESS on ONE concrete,
//! disputed decision: a question, the positions, and the evidence in; a ruling with its agreement
//! and dissent out.
//!
//! This is the council's only job now. Per-phase distribution no longer convenes one to route
//! units to seats ([`crate::distribute`] routes deterministically and records
//! `RoutingInfo::Teamed`); a council is summoned only when the team disagrees on something
//! concrete — a worker refuting a monitor's finding, say — and it rules on that one question and
//! is gone. Nothing convenes it yet: gate adjudication of disputes (core#590 S6) is its caller.
//!
//! The machinery is the council's own, reused unchanged: `wicked_council::Worker` (ballots,
//! runoffs, synthesis), the injected [`Dispatcher`] (the production one is
//! `RealDispatcher::from_env`, whose ballot budget scales with host load —
//! `dispatch::ballot_load_factor`), and the claude-ballot deny fence the routing council used.
//!
//! Known seam for S6: the ballot scaffold (`wicked_council::dispatch::render_ballot`) is still
//! worded for routing ("capability profiles"); the positions ride its numbered options and the
//! question + evidence ride its topic. Voters answer with a position NUMBER, as they did there.

use std::sync::Arc;

use wicked_council::dispatch::RealDispatcher;
use wicked_council::types::Dispatcher;
use wicked_council::{
    ids, work_kind_for, AgenticCli, CouncilTask, EstateHandle, EstateRankStore, Ledger,
    NoopEventSink, PollStatus, TaskState, Worker,
};

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
                // F-031: the stdout tail and the classified cause (`null` when unclassified).
                stdout: payload["stdout"].as_str().unwrap_or_default().to_string(),
                detail: payload["detail"].as_str().unwrap_or_default().to_string(),
                reason: payload["reason"].as_str().map(str::to_string),
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

/// The program token of a seat's `headless_invocation` — the SAME carrier identity the council
/// dispatch judges (`run_in_isolation` execs the first token and hands it to
/// `seat_config_for_carrier`): a double-quoted program or the first bare token.
fn ballot_program(invocation: &str) -> &str {
    let s = invocation.trim_start();
    match s.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(""),
        None => s.split_whitespace().next().unwrap_or(""),
    }
}

/// Is this seat a claude BALLOT — judged exactly as the council will exec it: the invocation's
/// program token through the one shared carrier resolver (`binary_is_claude`; case rules follow
/// the OS's own executable lookup), never the roster key or the record's `binary`.
fn is_claude_ballot(cli: &AgenticCli) -> bool {
    wicked_apps_core::spawn::binary_is_claude(ballot_program(&cli.headless_invocation))
}

/// The roster the council convenes with: every claude ballot's `trust_flags` gain
/// `--disallowedTools <state-home rules>` (`execute_wrapped::ballot_deny_rules`) — the half of the
/// fence the shared worker file omits by design; every other seat is handed back untouched.
fn fenced_roster(
    clis: &[AgenticCli],
    operational_home: Option<&std::path::Path>,
) -> anyhow::Result<Vec<AgenticCli>> {
    let rules =
        crate::execute_wrapped::ballot_deny_rules(operational_home).map_err(anyhow::Error::msg)?;
    Ok(clis
        .iter()
        .cloned()
        .map(|mut c| {
            if is_claude_ballot(&c) && !rules.is_empty() {
                c.trust_flags.push("--disallowedTools".into());
                c.trust_flags.push(rules.join(","));
            }
            c
        })
        .collect())
}

/// Fence a council roster before any ballot runs (wicked-crew#524 follow-up / core#595) and
/// return the roster the council convenes with. `scope` names the council in an error.
fn fence_ballot_roster(
    clis: &[AgenticCli],
    operational_home: Option<&std::path::Path>,
    scope: &str,
) -> anyhow::Result<Vec<AgenticCli>> {
    // wicked-crew#524 follow-up (review on core#436): the council's claude seat runs under the
    // worker home with the trust flag appended (`wicked_council::dispatch::run_in_isolation`) and
    // creates no fence of its own — a council convened before any worker had spawned used to run
    // with no deny fence at all. Fence the ROSTER here, before any ballot, in the two halves
    // `execute_wrapped::ballot_deny_rules` documents: the shared worker `settings.json` (the same
    // idempotent writer the ACP spawn uses) and, on the claude seat's own argv as
    // `--disallowedTools` (the council appends `trust_flags` verbatim), the state-home rules that
    // file omits by design. A fence that cannot be written refuses the council — fail closed,
    // exactly as the ACP spawn refuses the worker. A roster with no claude ballot is untouched.
    if !clis.iter().any(is_claude_ballot) {
        return Ok(clis.to_vec());
    }
    {
        crate::acp_runner::ensure_shared_worker_fence().map_err(|e| {
            anyhow::anyhow!(
                "{scope}: the shared worker fence (<worker home>/claude/\
                 settings.json) could not be written ({e}); refusing to convene a claude seat \
                 without its deny fence"
            )
        })?;
        // core#595: secondary claude ballots (`claude#2`) run under `<base>/claude-2`, which
        // `ensure_shared_worker_fence` does not cover. Write a matching fence for every
        // secondary instance before the ballot runs — fail closed on error, as above.
        for cli in clis.iter().filter(|c| is_claude_ballot(c)) {
            if wicked_apps_core::spawn::seat_cli_key(&cli.key) != cli.key.as_str() {
                crate::execute_wrapped::ensure_secondary_instance_fence(&cli.key).map_err(|e| {
                    anyhow::anyhow!(
                        "{scope}: the instance fence for `{}` could not be \
                             written ({e}); refusing to convene without its deny fence",
                        cli.key
                    )
                })?;
            }
        }
        fenced_roster(clis, operational_home).map_err(|e| {
            anyhow::anyhow!(
                "{scope}: the ballot's state-home fence could not be built ({e}); \
                 refusing to convene a claude seat without it"
            )
        })
    }
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
        .map(|f| format!("{}: {}", f.cli, f.failure.summary()))
        .collect();
    let seats = format!("council {state} — {}", seats.join("; "));
    // Both can be present: seats failed AND the council then unwound. Neither explains the
    // other, so neither is dropped.
    match &status.failure_detail {
        Some(detail) => format!("{seats}; the council itself failed: {detail}"),
        None => seats,
    }
}

/// The criteria every decision ballot is judged on.
const DECISION_CRITERIA: &[&str] = &["general"];

/// One concrete, disputed decision put to a council (core#590 S5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRequest {
    /// The run the dispute belongs to — scopes the council's lifecycle events.
    pub session_id: String,
    /// The unit the dispute is about — scopes the council's lifecycle events.
    pub ord: u32,
    /// The question to rule on, stated as one decision.
    pub question: String,
    /// The competing positions, at least two. Voters pick one by NUMBER (1-based on the ballot);
    /// the ruling names it by its 0-based index here.
    pub options: Vec<String>,
    /// The evidence both sides rely on — findings, refutations, file:line citations.
    pub evidence: String,
}

/// The council's ruling on a [`DecisionRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionVerdict {
    /// The council task id (provenance).
    pub task_id: String,
    /// The 0-based index into `options` of the winning position; `None` when the council produced
    /// no ruling (see `no_ruling_reason`).
    pub winner: Option<usize>,
    /// A strict majority of the SEATED council converged on the winner.
    pub consensus: bool,
    /// Winning votes over votes that answered, `0..=100`.
    pub agreement_pct: u8,
    /// Ballots that came back.
    pub returned: u32,
    /// Seats convened — the denominator `returned` is read against.
    pub seated: u32,
    /// The minority recommendations the verdict recorded, verbatim.
    pub dissent: Vec<String>,
    /// WHY there is no ruling, when `winner` is `None` — named as specifically as the council's
    /// record allows (the seats' own failures, the council's own failure, or its state).
    pub no_ruling_reason: Option<String>,
}

/// The 0-based option a recommendation names: its leading integer, 1-based on the ballot, when
/// it is in range. Voters are told to lead with the option number (`"2 — rationale"`).
fn named_option(recommendation: &str, options: usize) -> Option<usize> {
    recommendation
        .trim()
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|tok| tok.parse::<usize>().ok())
        .filter(|&n| n >= 1 && n <= options)
        .map(|n| n - 1)
}

/// Convene a council on ONE disputed decision and return its ruling (core#590 S5) — the single
/// engine entry point for a council, reached through `Core::convene_decision`.
///
/// Refuses (an `Err`, no ballot dispatched) a request with fewer than two positions or an empty
/// question, and an empty roster. Every seat of `clis` is convened; the caller hands only the
/// seats it may use (a run's eligible, unbenched seats). A council that votes but names no
/// position, or cannot reach a vote at all, is NOT an error: it is a verdict with `winner: None`
/// and the reason — the caller (the gate) decides what an absent ruling means.
pub(crate) fn convene_decision(
    req: &DecisionRequest,
    clis: &[AgenticCli],
    dispatcher: &Arc<dyn Dispatcher + Send + Sync>,
    relay: Option<EventRelay>,
    operational_home: Option<&std::path::Path>,
) -> anyhow::Result<DecisionVerdict> {
    if req.question.trim().is_empty() {
        anyhow::bail!("a decision council needs a question to rule on");
    }
    if req.options.len() < 2 {
        anyhow::bail!(
            "a decision council needs at least two positions to choose between (got {})",
            req.options.len()
        );
    }
    if clis.is_empty() {
        anyhow::bail!(
            "no seat to convene a decision council for {} unit {}",
            req.session_id,
            req.ord
        );
    }
    let scope = format!("decision council for {} unit {}", req.session_id, req.ord);
    let seats = fence_ballot_roster(clis, operational_home, &scope)?;

    let estate = EstateHandle::in_memory()
        .map_err(|e| anyhow::anyhow!("open council estate handle: {e}"))?;
    let ledger = Ledger::new(estate.clone());
    let rank_store = Arc::new(EstateRankStore::new(estate));
    let events: Arc<dyn wicked_council::EventSink + Send + Sync> = match relay {
        Some(relay) => Arc::new(RelaySink {
            relay,
            session: req.session_id.clone(),
            ord: req.ord,
        }),
        None => Arc::new(NoopEventSink),
    };
    let criteria: Vec<String> = DECISION_CRITERIA.iter().map(|s| s.to_string()).collect();
    let work_kind = work_kind_for(&criteria);
    let worker = Worker::new(
        ledger,
        dispatcher.clone(),
        rank_store,
        events,
        seats,
        work_kind,
    );
    let task = CouncilTask {
        id: ids::new_task_id(),
        topic: format!(
            "A concrete decision is disputed and needs a ruling.\n\
             Question: {}\n\
             Evidence:\n{}\n\
             Which numbered position does the evidence best support?",
            req.question.trim(),
            req.evidence.trim()
        ),
        options: req.options.clone(),
        criteria,
        session_id: req.session_id.clone(),
    };
    let task_id = worker.queue_blocking(task);
    let status = worker.poll(&task_id);
    Ok(ruling(task_id, status.as_ref(), req.options.len()))
}

/// Read the council's poll status as a ruling over `options` positions.
fn ruling(task_id: String, status: Option<&PollStatus>, options: usize) -> DecisionVerdict {
    let none = |reason: String, returned: u32, seated: u32| DecisionVerdict {
        task_id: task_id.clone(),
        winner: None,
        consensus: false,
        agreement_pct: 0,
        returned,
        seated,
        dissent: Vec::new(),
        no_ruling_reason: Some(reason),
    };
    let Some(status) = status else {
        return none("council returned no status".to_string(), 0, 0);
    };
    if status.state != TaskState::Voted {
        return none(no_vote_reason(status), status.returned, status.seated);
    }
    let Some(verdict) = &status.verdict else {
        return none(
            "council produced no verdict".to_string(),
            status.returned,
            status.seated,
        );
    };
    let winner = verdict
        .winning_recommendation
        .as_deref()
        .and_then(|w| named_option(w, options));
    let no_ruling_reason = match (&verdict.winning_recommendation, winner) {
        (_, Some(_)) => None,
        (None, None) => Some("verdict named no winner".to_string()),
        (Some(w), None) => Some(format!(
            "recommendation '{w}' did not name one of the {options} positions"
        )),
    };
    DecisionVerdict {
        task_id: task_id.clone(),
        winner,
        consensus: verdict.consensus && winner.is_some(),
        agreement_pct: pct(verdict.agreement_ratio),
        returned: status.returned,
        seated: status.seated,
        dissent: verdict.dissent.clone(),
        no_ruling_reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wicked_council::types::{Category, Confidence, InputMode, Vote};

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
            login_invocation: None,
            health: None,
        }
    }

    /// A seat whose INVOCATION execs `claude` — the carrier identity the council judges.
    fn claude_seat(key: &str) -> AgenticCli {
        let mut c = seat(key);
        c.binary = "claude".into();
        c.headless_invocation = "claude -p {PROMPT}".into();
        c.trust_flags = vec!["--dangerously-skip-permissions".into()];
        c
    }

    /// A stub dispute voter: each seat key votes the recommendation mapped to it; every ballot is
    /// counted and every seat it was handed is recorded.
    struct DisputeDispatcher {
        votes: Vec<(&'static str, &'static str)>,
        calls: Arc<AtomicUsize>,
        seen: Arc<std::sync::Mutex<Vec<AgenticCli>>>,
        topics: Arc<std::sync::Mutex<Vec<String>>>,
    }
    impl Dispatcher for DisputeDispatcher {
        fn dispatch(&self, cli: &AgenticCli, task: &CouncilTask) -> Option<Vote> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(cli.clone());
            self.topics.lock().unwrap().push(task.topic.clone());
            let rec = self
                .votes
                .iter()
                .find(|(k, _)| *k == cli.key)
                .map(|(_, r)| *r)?;
            Some(Vote {
                cli: cli.key.clone(),
                recommendation: rec.into(),
                top_risk: "none".into(),
                change_my_mind: "no".into(),
                disqualifier: None,
                confidence: Confidence::default(),
                provenance: "dispute-stub".into(),
            })
        }
    }

    struct Stub {
        dispatcher: Arc<dyn Dispatcher + Send + Sync>,
        calls: Arc<AtomicUsize>,
        seen: Arc<std::sync::Mutex<Vec<AgenticCli>>>,
        topics: Arc<std::sync::Mutex<Vec<String>>>,
    }

    fn stub(votes: Vec<(&'static str, &'static str)>) -> Stub {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let topics = Arc::new(std::sync::Mutex::new(Vec::new()));
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(DisputeDispatcher {
            votes,
            calls: calls.clone(),
            seen: seen.clone(),
            topics: topics.clone(),
        });
        Stub {
            dispatcher,
            calls,
            seen,
            topics,
        }
    }

    /// The stub dispute: a monitor flagged a finding, the worker refuted it with evidence.
    fn dispute() -> DecisionRequest {
        DecisionRequest {
            session_id: "run-1".into(),
            ord: 3,
            question: "Does the retire dialog need a cancellable coverage fetch?".into(),
            options: vec![
                "Monitor: yes, a stale response can show the wrong erase count".into(),
                "Worker: no, the dialog remounts per scope so no stale response can land".into(),
            ],
            evidence: "Finding: src/Retire.tsx:41 fetches in onClick with no AbortController.\n\
                       Refutation: src/Retire.tsx:12 keys the dialog on scope id."
                .into(),
        }
    }

    /// core#590 S5 — the entry point, driven with a stub dispute: every seat is balloted once,
    /// the question, evidence and positions reach the ballot, and the unanimous ruling names the
    /// worker's position by its index with its agreement and (empty) dissent.
    #[test]
    fn a_unanimous_council_rules_on_the_disputed_decision() {
        let s = stub(vec![
            ("a", "2 — the remount is the cancellation"),
            ("b", "2 — refutation holds"),
            ("c", "2 — keyed on scope"),
        ]);
        let v = convene_decision(
            &dispute(),
            &[seat("a"), seat("b"), seat("c")],
            &s.dispatcher,
            None,
            None,
        )
        .expect("the council rules");
        assert_eq!(s.calls.load(Ordering::SeqCst), 3);
        let mut balloted: Vec<String> = s
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.key.clone())
            .collect();
        balloted.sort();
        assert_eq!(
            balloted,
            ["a", "b", "c"],
            "every seat handed is balloted once"
        );
        assert_eq!(v.winner, Some(1));
        assert!(v.consensus);
        assert_eq!(v.agreement_pct, 100);
        assert_eq!(v.returned, 3);
        assert_eq!(v.seated, 3);
        assert_eq!(v.dissent, Vec::<String>::new());
        assert_eq!(v.no_ruling_reason, None);
        let topic = s.topics.lock().unwrap()[0].clone();
        assert!(
            topic.contains("Question: Does the retire dialog need a cancellable coverage fetch?")
                && topic.contains("src/Retire.tsx:41")
                && topic.contains("src/Retire.tsx:12"),
            "{topic}"
        );
    }

    /// A split council still rules, and the minority is on the record as dissent.
    #[test]
    fn a_split_council_rules_with_the_minority_recorded_as_dissent() {
        let s = stub(vec![
            ("a", "1 — stale response risk is real"),
            ("b", "2 — refutation holds"),
            ("c", "2 — keyed on scope"),
            ("d", "2 — remount cancels"),
        ]);
        let v = convene_decision(
            &dispute(),
            &[seat("a"), seat("b"), seat("c"), seat("d")],
            &s.dispatcher,
            None,
            None,
        )
        .expect("the council rules");
        assert_eq!(v.winner, Some(1));
        assert!(v.consensus);
        assert_eq!(v.agreement_pct, 75);
        assert_eq!(v.seated, 4);
        assert_eq!(v.returned, 4);
        assert_eq!(
            v.dissent,
            vec!["1 — stale response risk is real".to_string()]
        );
    }

    /// A council that cannot vote is a verdict WITHOUT a ruling, naming why — not an error.
    #[test]
    fn a_council_with_no_vote_returns_no_ruling_and_names_the_cause() {
        let s = stub(vec![]);
        let v = convene_decision(
            &dispute(),
            &[seat("a"), seat("b")],
            &s.dispatcher,
            None,
            None,
        )
        .expect("an absent ruling is a verdict, not an error");
        assert_eq!(v.winner, None);
        assert!(!v.consensus);
        assert_eq!(v.returned, 0);
        assert_eq!(v.seated, 2);
        assert!(v.no_ruling_reason.is_some(), "{v:?}");
    }

    /// A recommendation that names no position is no ruling, and says so.
    #[test]
    fn a_recommendation_naming_no_position_is_no_ruling() {
        let s = stub(vec![("a", "9 — neither"), ("b", "9 — neither")]);
        let v = convene_decision(
            &dispute(),
            &[seat("a"), seat("b")],
            &s.dispatcher,
            None,
            None,
        )
        .expect("verdict");
        assert_eq!(v.winner, None);
        assert!(!v.consensus);
        assert_eq!(
            v.no_ruling_reason.as_deref(),
            Some("recommendation '9 — neither' did not name one of the 2 positions")
        );
    }

    /// A malformed request dispatches nothing.
    #[test]
    fn a_malformed_request_is_refused_before_any_ballot() {
        let s = stub(vec![("a", "1")]);
        let mut one = dispute();
        one.options.truncate(1);
        let err = convene_decision(&one, &[seat("a")], &s.dispatcher, None, None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "a decision council needs at least two positions to choose between (got 1)"
        );
        let mut blank = dispute();
        blank.question = "  ".into();
        assert!(convene_decision(&blank, &[seat("a")], &s.dispatcher, None, None).is_err());
        let err = convene_decision(&dispute(), &[], &s.dispatcher, None, None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "no seat to convene a decision council for run-1 unit 3"
        );
        assert_eq!(s.calls.load(Ordering::SeqCst), 0);
    }

    /// Council lifecycle events are relayed run-scoped to the dispute's unit.
    #[test]
    fn a_decision_council_relays_its_lifecycle_events_scoped_to_the_dispute() {
        let s = stub(vec![("a", "1"), ("b", "1")]);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let relay: EventRelay = Arc::new(move |ev| sink.lock().unwrap().push(ev));
        convene_decision(
            &dispute(),
            &[seat("a"), seat("b")],
            &s.dispatcher,
            Some(relay),
            None,
        )
        .expect("verdict");
        let events = seen.lock().unwrap();
        assert!(
            events.iter().any(
                |e| matches!(e, CoreEvent::CouncilConvened { session, ord: 3, clis }
                if session == "run-1" && clis == &["a".to_string(), "b".to_string()])
            ),
            "{events:?}"
        );
        assert!(
            events.iter().any(
                |e| matches!(e, CoreEvent::CouncilVoted { session, ord: 3, consensus: true, .. }
                if session == "run-1")
            ),
            "{events:?}"
        );
    }

    #[test]
    fn named_option_reads_the_leading_number_in_range() {
        assert_eq!(named_option("2 — rationale", 3), Some(1));
        assert_eq!(named_option("1", 3), Some(0));
        assert_eq!(named_option("  3. because", 3), Some(2));
        assert_eq!(named_option("4", 3), None);
        assert_eq!(named_option("0", 3), None);
        assert_eq!(named_option("the second", 3), None);
    }

    /// Records every seat it is asked to dispatch — what the council actually convened with.
    struct RecordingDispatcher {
        seen: Arc<std::sync::Mutex<Vec<AgenticCli>>>,
    }
    impl Dispatcher for RecordingDispatcher {
        fn dispatch(&self, cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
            self.seen.lock().unwrap().push(cli.clone());
            Some(Vote {
                cli: cli.key.clone(),
                recommendation: "1 — fit".into(),
                top_risk: "none".into(),
                change_my_mind: "no".into(),
                disqualifier: None,
                confidence: Confidence::default(),
                provenance: "recording".into(),
            })
        }
    }

    /// wicked-crew#524 follow-up (review on core#436): a claude BALLOT is judged by the
    /// invocation's program token through the shared carrier resolver — exactly what the council
    /// execs — never by the roster key or the record's `binary`.
    #[test]
    fn a_claude_ballot_is_judged_by_the_invocations_program_token_like_the_council() {
        let mut custom = seat("custom");
        custom.headless_invocation = "claude -p {PROMPT}".into();
        assert!(
            is_claude_ballot(&custom),
            "a custom key whose template execs claude IS a ballot"
        );
        let mut quoted = seat("q");
        quoted.headless_invocation = "\"/opt/tools/claude\" -p {PROMPT}".into();
        assert!(
            is_claude_ballot(&quoted),
            "a quoted program path is judged by its file stem"
        );
        let by_key_only = seat("claude"); // template `run-claude {PROMPT}`, binary `unused`
        assert!(
            !is_claude_ballot(&by_key_only),
            "the key alone is not the carrier"
        );
        let mut binary_only = seat("x");
        binary_only.binary = "claude".into();
        assert!(
            !is_claude_ballot(&binary_only),
            "the record's binary alone is not the carrier"
        );
        assert_eq!(ballot_program("  codex exec {PROMPT}"), "codex");
        assert_eq!(ballot_program("\"/a b/claude\" -p"), "/a b/claude");
        assert_eq!(ballot_program(""), "");
    }

    /// wicked-crew#524 follow-up (review on core#436): convening a DECISION council that seats a claude
    /// ballot writes the SHARED worker fence first AND hands the claude seat the state-home rules
    /// that file omits — on its own argv (`--disallowedTools`), the operational home included —
    /// so a council-first ballot is fenced like a worker launch. Every other seat is untouched; a
    /// claude-less roster writes nothing. (The test process arms `WICKED_WORKER_HOME` to a
    /// per-process temp home pre-main; this test re-aims it at a fixture and restores it.)
    #[test]
    fn convening_a_claude_ballot_writes_the_shared_fence_and_fences_the_state_home_on_argv() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let hatch = crate::execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV;
        let prev_hatch = std::env::var_os(hatch);
        std::env::remove_var(hatch);
        let base = std::env::temp_dir().join(format!("wdecision-fence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::env::set_var(wicked_apps_core::spawn::WORKER_HOME_ENV, &base);
        let op_home = base.join("state");
        let seen: Arc<std::sync::Mutex<Vec<AgenticCli>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(RecordingDispatcher {
            seen: Arc::clone(&seen),
        });
        // No claude ballot ⇒ nothing is written, no seat is touched.
        convene_decision(
            &dispute(),
            &[seat("codex"), seat("pi")],
            &dispatcher,
            None,
            Some(&op_home),
        )
        .expect("a claude-less council rules");
        assert!(
            !base.join("claude").join("settings.json").exists(),
            "a claude-less roster writes no fence"
        );
        assert!(seen
            .lock()
            .unwrap()
            .iter()
            .all(|c| c.trust_flags.is_empty()));
        seen.lock().unwrap().clear();
        // A claude ballot ⇒ the shared fence exists with the shared rules, and the claude seat's
        // argv carries the state-home rules — the operational home included; codex is untouched.
        convene_decision(
            &dispute(),
            &[claude_seat("claude"), seat("codex")],
            &dispatcher,
            None,
            Some(&op_home),
        )
        .expect("a council with a claude ballot rules");
        let bytes = std::fs::read(base.join("claude").join("settings.json"))
            .expect("the shared fence was written before the ballot");
        let settings: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let deny: Vec<String> = settings["permissions"]["deny"]
            .as_array()
            .expect("permissions.deny present")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        assert_eq!(
            deny,
            crate::execute_wrapped::shared_deny_rules(None).unwrap(),
            "the ballot reads the SAME shared fence a worker spawn writes"
        );
        let convened = seen.lock().unwrap().clone();
        let claude = convened
            .iter()
            .find(|c| c.key == "claude")
            .expect("the claude seat was convened");
        let codex = convened
            .iter()
            .find(|c| c.key == "codex")
            .expect("the codex seat was convened");
        assert!(
            codex.trust_flags.is_empty(),
            "a non-claude seat is handed back untouched"
        );
        let expected = crate::execute_wrapped::ballot_deny_rules(Some(&op_home)).unwrap();
        assert_eq!(
            claude.trust_flags,
            vec![
                "--dangerously-skip-permissions".to_string(),
                "--disallowedTools".to_string(),
                expected.join(","),
            ],
            "the claude ballot carries the state-home rules on its argv, after its own trust flag"
        );
        let opr = crate::execute_wrapped::rule_path(&op_home).unwrap();
        assert!(
            expected.contains(&format!("Read({opr}/**)"))
                && expected.contains(&format!("Edit({opr}/**)")),
            "{expected:?}"
        );
        match prev_hatch {
            Some(v) => std::env::set_var(hatch, v),
            None => std::env::remove_var(hatch),
        }
        std::env::set_var(
            wicked_apps_core::spawn::WORKER_HOME_ENV,
            wicked_apps_core::spawn::hermetic_test_worker_home(),
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// core#595 HIGH-3: convening a council that seats a SECONDARY claude ballot (`claude#2`)
    /// must write the deny fence to the INSTANCE home (`<worker>/claude-2/settings.json`).
    /// `ensure_shared_worker_fence` writes only `<worker>/claude`; before this fix nothing
    /// wrote to `claude-2`, so the ballot started unfenced.
    ///
    /// FAILS on base (c163cf9): `<worker>/claude-2/settings.json` does not exist.
    #[test]
    fn convening_a_secondary_claude_ballot_writes_fence_to_the_instance_home() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let hatch = crate::execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV;
        let prev_hatch = std::env::var_os(hatch);
        std::env::remove_var(hatch);
        let base =
            std::env::temp_dir().join(format!("wdecision-secondary-fence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::env::set_var(wicked_apps_core::spawn::WORKER_HOME_ENV, &base);

        let seen: Arc<std::sync::Mutex<Vec<AgenticCli>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(RecordingDispatcher {
            seen: Arc::clone(&seen),
        });

        convene_decision(
            &dispute(),
            &[claude_seat("claude#2")],
            &dispatcher,
            None,
            None,
        )
        .expect("a council with a secondary claude ballot rules");

        assert!(
            base.join("claude-2").join("settings.json").exists(),
            "the fence must be written at the secondary instance home \
             (<worker>/claude-2/settings.json); it is missing — the ballot ran without a deny fence"
        );
        // Assert specific security-critical deny rules are present.
        // Hardcoded — NOT computed from shared_deny_rules — so a missing rule in that
        // function fails this test rather than silently passing. (core#595 evaluator CRITICAL)
        let bytes = std::fs::read(base.join("claude-2").join("settings.json")).unwrap();
        let settings: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let deny: Vec<String> = settings["permissions"]["deny"]
            .as_array()
            .expect("permissions.deny present")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        // Directory fences: derive the expected path from HOME directly, never from
        // shared_deny_rules — the point is to catch a missing rule in that function.
        // Fence rules spell every path with forward slashes on every OS (the documented Windows
        // form is `C:/Users/me/.ssh/**`, execute_wrapped.rs tests). A raw USERPROFILE is
        // `C:\Users\...`, so normalise the separator HERE with a plain replace — never through
        // `rule_path`, which is the code under test and would make this assertion vacuous again.
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default()
            .replace('\\', "/");
        for must_contain in [
            // Credential and key protection: these are the rules that stop reads of SSH keys,
            // AWS credentials and git credential files by a worker running in the secondary home.
            format!("Read({home}/.ssh/**)"),
            format!("Edit({home}/.ssh/**)"),
            format!("Read({home}/.aws/**)"),
            format!("Edit({home}/.aws/**)"),
            format!("Read({home}/.git-credentials)"),
            format!("Edit({home}/.git-credentials)"),
            // Operator tool-config fences: council config and wicked daemon state.
            format!("Read({home}/.config/wicked-council/**)"),
            format!("Edit({home}/.config/wicked-council/**)"),
            // Privilege escalation and remote-write Bash verbs — static, never HOME-dependent.
            "Bash(sudo:*)".to_string(),
            "Bash(git push:*)".to_string(),
            "Bash(gh pr create:*)".to_string(),
        ] {
            assert!(
                deny.contains(&must_contain),
                "secondary fence at claude-2/settings.json is missing security-critical rule \
                 {must_contain:?}; full deny list: {deny:#?}"
            );
        }

        match prev_hatch {
            Some(v) => std::env::set_var(hatch, v),
            None => std::env::remove_var(hatch),
        }
        std::env::set_var(
            wicked_apps_core::spawn::WORKER_HOME_ENV,
            wicked_apps_core::spawn::hermetic_test_worker_home(),
        );
        let _ = std::fs::remove_dir_all(&base);
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
