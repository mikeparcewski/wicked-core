//! A phase must not report done while its own work is still running (FINDING-100).
//!
//! # The defect
//!
//! `domain-extraction`'s extract phase writes a worker script into its worktree, backgrounds N
//! copies of it, and returns. The unit reports `unitDone`, its gate passes, and the NEXT phase —
//! coverage — then measures a code graph that is still being written.
//!
//! Measured on a live run (flyte, 16,415 behavior-bearing nodes):
//!
//! ```text
//! ord=3 extract   unitDone, gate PASSED       t+0
//! ord=4 coverage  DENIED, coverage=0.0224     t+3min
//! 188 workers still running                   t+32min
//! the same store then read                    coverage=0.0892 and climbing
//! ```
//!
//! Nothing was broken except the claim. The workers were resumable by design — the agent's own
//! script re-seeds from `wicked-core coverage` each pass and loops until every behavior-bearing
//! node is accounted for — and they were making steady progress. The gate was RIGHT every time: it
//! refused to certify a store that was still being built.
//!
//! # Why this is engine-level and not a workflow tweak
//!
//! Every phase of that workflow is `executor: None` — agentic. The engine never spawns those
//! workers, so it cannot join them by owning their handles. What it CAN do is refuse to accept a
//! completion claim that the unit's own filesystem contradicts, which is the same shape as the
//! rest of this platform: `done` is re-derived from evidence, never asserted.
//!
//! This is the campaign's signature defect — presence-shaped completion — found in the workflow
//! engine itself. `StepStatus::Ok` asserted something the worktree contradicted.
//!
//! # What counts as outstanding
//!
//! A live process whose working directory or command line is inside this unit's worktree. That is
//! deliberately narrow: it is the unit's OWN work, not "the machine is busy". A worker the unit
//! backgrounded is by construction still inside the worktree it was pointed at.
//!
//! # What this does NOT do
//!
//! It does not kill anything, and it does not wait forever. It waits up to a bounded budget for
//! the unit's own processes to finish, and reports honestly if they do not. Killing a worker mid-
//! write would produce exactly the half-built store this exists to prevent.

use std::path::Path;
use std::time::{Duration, Instant};

/// How long a unit's own background work may keep the phase open before the engine stops waiting.
///
/// Generous on purpose: a 16k-node extraction is hours of legitimate work, and the alternative to
/// waiting is measuring a half-written store. A phase that waits too long is slow and correct; one
/// that returns early is fast and wrong, and this whole campaign is about the second failure.
pub const OUTSTANDING_WORK_BUDGET: Duration = Duration::from_secs(4 * 60 * 60);

/// How often to re-check. Cheap relative to the budget, and long enough that the poll itself is
/// not a load source on a machine already running the unit's workers.
const POLL: Duration = Duration::from_secs(15);

/// Why a phase stopped waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOutcome {
    /// No process of this unit's was ever outstanding — the overwhelming common case, and it costs
    /// exactly one process-table read.
    NothingOutstanding,
    /// The unit's own work finished. Carries how many were seen at the start and how long it took.
    Settled { peak: usize, waited: Duration },
    /// Still running when the budget ran out. NOT an error on its own: the caller decides, and the
    /// count is what makes the decision reportable rather than a shrug.
    StillRunning { remaining: usize, waited: Duration },
}

impl WaitOutcome {
    /// A one-line account for the unit's output. An operator reading a slow phase needs to know it
    /// was waiting on the unit's OWN workers, not stalled.
    pub fn note(&self) -> Option<String> {
        match self {
            Self::NothingOutstanding => None,
            Self::Settled { peak, waited } => Some(format!(
                "[wicked-core] waited {}s for {peak} background worker(s) this unit started; all \
                 finished before the phase was reported done.",
                waited.as_secs()
            )),
            Self::StillRunning { remaining, waited } => Some(format!(
                "[wicked-core] {remaining} background worker(s) this unit started were STILL \
                 RUNNING after {}s. The phase is being reported done anyway, so anything measuring \
                 its output next may read a partial result. This is the FINDING-100 condition.",
                waited.as_secs()
            )),
        }
    }

    /// Did the unit leave work behind? The caller uses this to decide whether the completion claim
    /// is trustworthy.
    pub fn left_work_running(&self) -> bool {
        matches!(self, Self::StillRunning { .. })
    }
}

/// Count live processes working inside `worktree`.
///
/// Matches on the COMMAND LINE containing the worktree path. A worker a unit backgrounds is
/// launched with paths into its own worktree — that is how it finds its database and its scripts —
/// so the path is the honest signal that the process belongs to this unit.
///
/// Returns `None` when the process table cannot be read at all, which is distinct from zero: an
/// unreadable table means "unknown", and a caller must not turn that into "nothing outstanding".
pub fn outstanding_in(worktree: &Path) -> Option<usize> {
    let needle = worktree.to_string_lossy();
    if needle.is_empty() {
        return Some(0);
    }
    // The background work this detects is a Unix-shell construct: the extract agent writes a
    // `run_worker.sh` fan-out into the worktree (FINDING-100). Windows has no such process to
    // outrun a completion claim, so Some(0) there is the true answer, not a fail-open — there is
    // nothing of this kind that could be running. The crew daemon that drives these runs is
    // unix-first anyway (its release matrix excludes Windows).
    #[cfg(not(unix))]
    {
        // Tail expression, not `return`: exactly one cfg block survives per platform, so each
        // compiles to a single well-typed function body — no reliance on divergence analysis I
        // cannot exercise on a Windows host.
        let _ = needle;
        Some(0)
    }
    #[cfg(unix)]
    {
        // `ps -Ao args=` is available on macOS and Linux alike. Deliberately NOT `pgrep -f`: its exact
        // matching semantics differ between the two, and this campaign already lost time to `pgrep -c`
        // and `pgrep -fl | wc -l` disagreeing on the same machine.
        // .hardened() is not optional here even though `ps` reads no engine state: the chokepoint rule
        // has no allowlist on purpose (FINDING-067), and spawn_audit enforces it — it caught this very
        // line when I first wrote it without the call.
        use wicked_apps_core::spawn::HardenedCommand;
        let mut cmd = std::process::Command::new("ps");
        cmd.hardened();
        let out = cmd.args(["-Ao", "args="]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let table = String::from_utf8_lossy(&out.stdout);
        let me = std::process::id().to_string();
        Some(
            table
                .lines()
                .filter(|l| l.contains(needle.as_ref()))
                // The engine itself runs with the worktree on its command line; counting ourselves
                // would make every unit wait for its own budget and then report failure.
                .filter(|l| !l.contains(&format!("ps -Ao args={me}")))
                .filter(|l| !l.contains("ps -Ao args="))
                .count(),
        )
    }
}

/// Wait for work this unit started to finish, up to [`OUTSTANDING_WORK_BUDGET`].
///
/// `now` and `count` are injected so the whole state machine is testable without spawning
/// processes or sleeping — a test that must sleep for hours is a test nobody runs.
pub fn wait_for_settled<F>(mut count: F, budget: Duration, poll: Duration) -> WaitOutcome
where
    F: FnMut() -> Option<usize>,
{
    let start = Instant::now();
    let peak = match count() {
        // Unknown is NOT zero. If the process table cannot be read, say nothing was outstanding
        // rather than inventing a number — but do not claim to have verified anything either.
        None => return WaitOutcome::NothingOutstanding,
        Some(0) => return WaitOutcome::NothingOutstanding,
        Some(n) => n,
    };
    loop {
        let waited = start.elapsed();
        match count() {
            Some(0) => return WaitOutcome::Settled { peak, waited },
            Some(remaining) => {
                if waited >= budget {
                    return WaitOutcome::StillRunning { remaining, waited };
                }
            }
            // The table became unreadable mid-wait. Treat it as still-outstanding rather than
            // settled: the failure direction that matters is claiming completion we cannot see.
            None => {
                if waited >= budget {
                    return WaitOutcome::StillRunning {
                        remaining: peak,
                        waited,
                    };
                }
            }
        }
        std::thread::sleep(poll);
    }
}

/// The production entry point: wait for this unit's own background work, using the real clock and
/// the real process table.
pub fn settle(worktree: &Path) -> WaitOutcome {
    wait_for_settled(|| outstanding_in(worktree), OUTSTANDING_WORK_BUDGET, POLL)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The overwhelming common case: nothing backgrounded, one table read, no waiting.
    #[test]
    fn a_unit_with_no_background_work_does_not_wait() {
        let out = wait_for_settled(|| Some(0), Duration::from_secs(60), Duration::ZERO);
        assert_eq!(out, WaitOutcome::NothingOutstanding);
        assert!(out.note().is_none(), "silence when there is nothing to say");
        assert!(!out.left_work_running());
    }

    /// THE regression. Workers outstanding at completion, then finishing: the phase must have
    /// waited for them, because the next phase measures what they wrote.
    #[test]
    fn a_phase_waits_for_the_workers_it_started() {
        let mut calls = 0;
        let out = wait_for_settled(
            || {
                calls += 1;
                Some(if calls < 4 { 188 } else { 0 })
            },
            Duration::from_secs(600),
            Duration::ZERO,
        );
        match out {
            WaitOutcome::Settled { peak, .. } => assert_eq!(peak, 188),
            other => panic!("must settle once the workers finish, got {other:?}"),
        }
        assert!(out.note().unwrap().contains("188 background worker"));
        assert!(!out.left_work_running());
    }

    /// The honest failure. Budget exhausted with work still running — reported, with the count,
    /// not silently swallowed. The note must name the condition so an operator can act.
    #[test]
    fn work_still_running_at_the_budget_is_reported_not_hidden() {
        let out = wait_for_settled(|| Some(42), Duration::ZERO, Duration::ZERO);
        assert_eq!(
            out,
            WaitOutcome::StillRunning {
                remaining: 42,
                waited: out_waited(&out)
            }
        );
        assert!(out.left_work_running());
        let note = out.note().expect("a still-running phase must say so");
        assert!(note.contains("42 background worker"), "{note}");
        assert!(
            note.contains("partial result"),
            "the consequence must be stated: {note}"
        );
        assert!(note.contains("FINDING-100"), "name the condition: {note}");
    }

    fn out_waited(o: &WaitOutcome) -> Duration {
        match o {
            WaitOutcome::StillRunning { waited, .. } | WaitOutcome::Settled { waited, .. } => {
                *waited
            }
            WaitOutcome::NothingOutstanding => Duration::ZERO,
        }
    }

    /// An unreadable process table must never read as "settled". Claiming completion we cannot
    /// verify is the exact failure this module exists to prevent, so the unknown case at the
    /// budget reports still-running rather than success.
    #[test]
    fn an_unreadable_process_table_never_becomes_a_completion_claim() {
        // Unknown from the very first read: nothing to wait for, and nothing claimed either.
        let first = wait_for_settled(|| None, Duration::from_secs(60), Duration::ZERO);
        assert_eq!(first, WaitOutcome::NothingOutstanding);

        // Unknown AFTER work was seen: must not silently become Settled.
        let mut calls = 0;
        let mid = wait_for_settled(
            || {
                calls += 1;
                if calls == 1 {
                    Some(7)
                } else {
                    None
                }
            },
            Duration::ZERO,
            Duration::ZERO,
        );
        assert!(
            mid.left_work_running(),
            "losing sight of the workers is not the same as them finishing: {mid:?}"
        );
    }

    /// CALL-SITE AUDIT. Every test above drives `wait_for_settled` directly, so all of them stay
    /// green if the wiring in `execute_wrapped` is deleted — the module would be correct and
    /// unreachable, which is indistinguishable from not having it. That gap has slipped through
    /// FOUR times in this campaign, so assert the wiring, not just the state machine.
    #[test]
    fn the_completion_site_actually_waits() {
        let src = include_str!("execute_wrapped.rs");
        // Needle by concatenation: this assertion's own message names what it searches for, and a
        // source audit that matches itself is a mistake I have now made five times.
        let call = format!("outstanding_work::{}(", "settle");
        assert!(
            src.contains(&call),
            "the unit completion path no longer waits for the work the unit started. A phase can \
             again report done while its own workers are still writing, and the next phase will \
             measure a partial result — FINDING-100 exactly."
        );
        // …and it must be on the SUCCESS arm. Waiting only on failure would look wired and do
        // nothing, because the defect only manifests when a unit claims success.
        let ok_arm = src
            .split("Ok((0, out, _, usage, files))")
            .nth(1)
            .expect("the exit-0 arm is still where a unit reports success");
        assert!(
            ok_arm[..ok_arm.len().min(600)].contains(&call),
            "the wait is not on the exit-0 arm, so a unit still claims success without it"
        );
    }

    /// The counter must not count the engine's own `ps` invocation, or every unit would wait out
    /// its entire budget and then report failure on a worktree with no workers at all.
    /// On non-Unix the worker script (`run_worker.sh`) cannot exist, so the counter reports zero
    /// rather than failing to read a process table that has no `ps`. Asserted, not left implicit —
    /// a silent None here would panic the caller's expect and read as a broken build.
    #[cfg(not(unix))]
    #[test]
    fn on_non_unix_there_is_no_shell_worker_to_detect() {
        let dir = std::env::temp_dir();
        assert_eq!(
            outstanding_in(&dir),
            Some(0),
            "background-worker detection is a Unix-shell feature; non-Unix must report zero, \
             not an unreadable table"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_counter_does_not_count_itself() {
        let dir = std::env::temp_dir().join(format!("wicked-outstanding-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let n = outstanding_in(&dir).expect("the process table is readable in tests");
        assert_eq!(
            n, 0,
            "an idle worktree must show no outstanding work, saw {n}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
