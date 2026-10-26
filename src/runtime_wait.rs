//! (PRODUCT-WORKS J4) A container runtime blip is an infrastructure fault, never a seat failure.
//!
//! On the test rig Rancher Desktop restarted mid-run: docker was gone for about ten minutes, the
//! claude and copilot seats each "failed" their unit with `container_runtime_down`, the failover
//! ladder counted both as seat failures, and the run parked at a dead-seat card that offered only
//! a quota-benched seat. Nothing was wrong with either seat.
//!
//! The wrapped runner now reads a failed seat-container unit against a FRESH runtime probe (the
//! cached [`crate::container_floor::runtime`] answer outlives a restart). While the runtime is
//! down it waits, with backoff, up to a bounded window ([`WINDOW_ENV`], default
//! [`DEFAULT_WINDOW`]) and re-runs the unit on the SAME seat once the runtime answers again. Each
//! step is on the wire (`containerRuntimeWait`), so the run says "waiting for the container
//! runtime". When the window runs out the unit's output is the runner's own
//! [`UNAVAILABLE_MARKER`] line, which the actor routes to an infrastructure gate: no bench, no
//! strike, no failover, no dead-seat card.

use std::time::Duration;

use crate::workflow::{StepOutput, StepStatus};

/// How long a unit waits for the container runtime to come back, in seconds (`0` turns the wait
/// off: the unit escalates as an infrastructure problem at once).
pub(crate) const WINDOW_ENV: &str = "WICKED_RUNTIME_WAIT_SECS";
/// The default wait: long enough for a desktop runtime's restart, short enough to be a wait.
pub(crate) const DEFAULT_WINDOW: Duration = Duration::from_secs(180);
/// The first pause between probes; it doubles up to [`MAX_STEP`].
const FIRST_STEP: Duration = Duration::from_secs(2);
const MAX_STEP: Duration = Duration::from_secs(30);
/// Re-runs one unit may take for runtime faults, so an output that keeps naming the runtime while
/// the runtime answers cannot loop.
const MAX_RERUNS: u32 = 3;
/// How the runner's own "the runtime stayed down" output starts. The actor keys on it.
pub(crate) const UNAVAILABLE_MARKER: &str = "(container runtime unavailable:";

/// The wait window in force.
pub(crate) fn window() -> Duration {
    window_from(std::env::var(WINDOW_ENV).ok().as_deref())
}

fn window_from(v: Option<&str>) -> Duration {
    v.and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_WINDOW)
}

/// Whether a failed unit's output names the container runtime as the cause: the floor's own
/// `container_runtime_down` refusal, or the runtime client's words when its engine went away
/// under a running seat.
pub(crate) fn names_runtime_fault(output: &str) -> bool {
    let o = output.to_ascii_lowercase();
    o.contains("container_runtime_down")
        || o.contains("cannot connect to the docker daemon")
        || o.contains("is the docker daemon running")
        || o.contains("cannot connect to podman")
        || o.contains("error during connect")
}

/// Whether `output` is the runner's "the runtime stayed down" line ([`unavailable_output`]).
pub(crate) fn is_runtime_unavailable(output: &str) -> bool {
    output.trim_start().starts_with(UNAVAILABLE_MARKER)
}

/// One step of the wait, as `containerRuntimeWait.phase` spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitPhase {
    Waiting,
    Recovered,
    Unavailable,
}

impl WaitPhase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            WaitPhase::Waiting => "waiting",
            WaitPhase::Recovered => "recovered",
            WaitPhase::Unavailable => "unavailable",
        }
    }
}

/// What the wait needs from the host, injected so the policy is testable without a runtime.
pub(crate) struct Host<'a> {
    /// A fresh probe: the runtime CLI is installed and its engine does not answer.
    pub(crate) down_now: &'a mut dyn FnMut() -> bool,
    /// Pause for the duration; `false` when the unit's launch was cancelled meanwhile.
    pub(crate) sleep: &'a mut dyn FnMut(Duration) -> bool,
    /// Time since an arbitrary fixed start.
    pub(crate) elapsed: &'a dyn Fn() -> Duration,
    /// Report one step of the wait and the time waited so far.
    pub(crate) notify: &'a mut dyn FnMut(WaitPhase, Duration),
}

/// Ride a unit's failure out when the container runtime caused it: wait for the runtime, then
/// re-run the unit (`rerun`) on the same seat. Any other outcome is returned unchanged: a success,
/// a failure while the runtime answers that does not name it, a cancelled wait. A runtime that
/// stays down for the whole `window` (summed over the unit's re-runs) yields
/// [`unavailable_output`].
pub(crate) fn ride_out(
    first: StepOutput,
    cli: &str,
    window: Duration,
    host: Host<'_>,
    rerun: &mut dyn FnMut() -> StepOutput,
) -> StepOutput {
    let mut out = first;
    let mut reruns = 0;
    let mut wait_began: Option<Duration> = None;
    loop {
        if out.status != StepStatus::Failed || reruns >= MAX_RERUNS {
            return out;
        }
        let down = (host.down_now)();
        if !down && !names_runtime_fault(&out.output) {
            return out;
        }
        if down {
            let began = *wait_began.get_or_insert_with(|| (host.elapsed)());
            let mut step = FIRST_STEP;
            loop {
                let waited = (host.elapsed)().saturating_sub(began);
                if waited >= window {
                    (host.notify)(WaitPhase::Unavailable, waited);
                    return unavailable_output(&out, cli, waited);
                }
                (host.notify)(WaitPhase::Waiting, waited);
                if !(host.sleep)(step.min(window - waited)) {
                    return out;
                }
                step = (step * 2).min(MAX_STEP);
                if !(host.down_now)() {
                    break;
                }
            }
        }
        // The runtime answers (again): the unit re-runs on the same seat.
        let waited = wait_began
            .map(|b| (host.elapsed)().saturating_sub(b))
            .unwrap_or_default();
        (host.notify)(WaitPhase::Recovered, waited);
        reruns += 1;
        out = rerun();
    }
}

/// The unit's output once the runtime stayed down for the whole window: the runner's own line
/// (an infrastructure fault, in words that name no seat failure), then the last attempt's tail.
pub(crate) fn unavailable_output(last: &StepOutput, cli: &str, waited: Duration) -> StepOutput {
    let tail: String = {
        let t = last.output.trim();
        let start = t.char_indices().rev().nth(599).map(|(i, _)| i).unwrap_or(0);
        t[start..].to_string()
    };
    StepOutput {
        output: format!(
            "{UNAVAILABLE_MARKER} the container runtime did not answer for {}s, so the unit did \
             not run on seat '{cli}' — an infrastructure fault on this host, not the seat's (the \
             seat is not benched); start the container runtime (Rancher Desktop, Docker Desktop, \
             `podman machine start`, `colima start`), then retry)\n{tail}",
            waited.as_secs()
        ),
        status: StepStatus::Failed,
        usage: None,
        files: Vec::new(),
        tools: Vec::new(),
        governed: false,
        ..last.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn out(status: StepStatus, text: &str) -> StepOutput {
        StepOutput {
            run_id: "r".into(),
            unit_ix: 0,
            attempt: 0,
            output: text.into(),
            status,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }

    const DOWN: &str =
        "(seat 'claude' could not run this governed unit: its seat container did not \
                        arm — container_runtime_down: a container runtime CLI is installed but its \
                        engine did not answer)";

    /// A fake host: the runtime is down for the first `down_probes` probes; sleeping advances the
    /// clock; every notify is recorded.
    struct Fake {
        probes: Cell<u32>,
        down_probes: u32,
        clock: Cell<Duration>,
        notes: RefCell<Vec<(WaitPhase, u64)>>,
        cancel_after_sleeps: Option<u32>,
        sleeps: Cell<u32>,
    }

    impl Fake {
        fn new(down_probes: u32) -> Self {
            Fake {
                probes: Cell::new(0),
                down_probes,
                clock: Cell::new(Duration::ZERO),
                notes: RefCell::new(Vec::new()),
                cancel_after_sleeps: None,
                sleeps: Cell::new(0),
            }
        }

        fn ride(
            &self,
            first: StepOutput,
            window: Duration,
            reruns: &mut Vec<StepOutput>,
        ) -> (StepOutput, u32) {
            let mut down_now = || {
                let n = self.probes.get();
                self.probes.set(n + 1);
                n < self.down_probes
            };
            let mut sleep = |d: Duration| {
                self.clock.set(self.clock.get() + d);
                self.sleeps.set(self.sleeps.get() + 1);
                self.cancel_after_sleeps
                    .is_none_or(|n| self.sleeps.get() < n)
            };
            let elapsed = || self.clock.get();
            let mut notify =
                |p: WaitPhase, w: Duration| self.notes.borrow_mut().push((p, w.as_secs()));
            let mut ran = 0;
            let mut rerun = || {
                ran += 1;
                if reruns.is_empty() {
                    out(StepStatus::Ok, "done")
                } else {
                    reruns.remove(0)
                }
            };
            let got = ride_out(
                first,
                "claude",
                window,
                Host {
                    down_now: &mut down_now,
                    sleep: &mut sleep,
                    elapsed: &elapsed,
                    notify: &mut notify,
                },
                &mut rerun,
            );
            (got, ran)
        }
    }

    #[test]
    fn a_runtime_that_comes_back_re_runs_the_unit_on_the_same_seat() {
        let fake = Fake::new(3);
        let (got, ran) = fake.ride(
            out(StepStatus::Failed, DOWN),
            DEFAULT_WINDOW,
            &mut Vec::new(),
        );
        assert_eq!(got.status, StepStatus::Ok, "{}", got.output);
        assert_eq!(ran, 1, "re-run exactly once");
        let notes = fake.notes.borrow();
        assert_eq!(
            notes
                .iter()
                .filter(|(p, _)| *p == WaitPhase::Waiting)
                .count(),
            3,
            "one waiting frame per probe that found it down: {notes:?}"
        );
        assert_eq!(notes.last().map(|n| n.0), Some(WaitPhase::Recovered));
        // Backoff: 2 + 4 + 8 seconds waited before the fourth probe answered.
        assert_eq!(notes.last().map(|n| n.1), Some(14));
    }

    #[test]
    fn a_runtime_down_for_the_whole_window_escalates_as_infrastructure_not_as_the_seat() {
        let fake = Fake::new(u32::MAX);
        let (got, ran) = fake.ride(
            out(StepStatus::Failed, DOWN),
            DEFAULT_WINDOW,
            &mut Vec::new(),
        );
        assert_eq!(ran, 0, "nothing re-runs while the runtime is down");
        assert_eq!(got.status, StepStatus::Failed);
        assert!(is_runtime_unavailable(&got.output), "{}", got.output);
        assert!(
            got.output.contains("did not answer for 180s"),
            "{}",
            got.output
        );
        assert!(got.output.contains("not the seat's"), "{}", got.output);
        // Bounded: the clock never ran past the window.
        assert_eq!(fake.clock.get(), DEFAULT_WINDOW);
        assert_eq!(
            fake.notes.borrow().last().map(|n| n.0),
            Some(WaitPhase::Unavailable)
        );
        // Backoff capped at 30 s: 2,4,8,16,30,30,30,30,30 = 180.
        assert_eq!(fake.sleeps.get(), 9);
    }

    #[test]
    fn the_runners_unavailable_line_is_never_read_as_a_seat_refusal() {
        let got = unavailable_output(
            &out(StepStatus::Failed, DOWN),
            "copilot",
            Duration::from_secs(180),
        );
        let exited = crate::execute_wrapped::wrapped_exit_code(&got.output).is_some_and(|c| c != 0);
        assert!(
            wicked_council::types::SeatFailureReason::classify_refusal(&got.output, exited)
                .is_none(),
            "{}",
            got.output
        );
        assert!(crate::execute_wrapped::spawn_failure_detail(&got.output).is_none());
    }

    #[test]
    fn a_failure_while_the_runtime_answers_is_the_seats_and_is_returned_unchanged() {
        let fake = Fake::new(0);
        let failed = out(StepStatus::Failed, "(cli `claude` exited 1) boom");
        let (got, ran) = fake.ride(failed.clone(), DEFAULT_WINDOW, &mut Vec::new());
        assert_eq!(ran, 0);
        assert_eq!(got.output, failed.output);
        assert!(fake.notes.borrow().is_empty());
        assert_eq!(fake.probes.get(), 1, "one fresh probe decides");
    }

    #[test]
    fn a_success_is_never_probed() {
        let fake = Fake::new(u32::MAX);
        let (got, _) = fake.ride(out(StepStatus::Ok, "fine"), DEFAULT_WINDOW, &mut Vec::new());
        assert_eq!(got.status, StepStatus::Ok);
        assert_eq!(fake.probes.get(), 0);
    }

    #[test]
    fn a_blip_already_over_re_runs_when_the_output_names_the_runtime() {
        let fake = Fake::new(0);
        let failed = out(
            StepStatus::Failed,
            "docker: Cannot connect to the Docker daemon at unix:///x/docker.sock. Is the docker daemon running?",
        );
        let (got, ran) = fake.ride(failed, DEFAULT_WINDOW, &mut Vec::new());
        assert_eq!((got.status, ran), (StepStatus::Ok, 1));
    }

    #[test]
    fn re_runs_are_bounded_when_the_output_keeps_naming_the_runtime() {
        let fake = Fake::new(0);
        let mut reruns: Vec<StepOutput> = (0..10).map(|_| out(StepStatus::Failed, DOWN)).collect();
        let (got, ran) = fake.ride(out(StepStatus::Failed, DOWN), DEFAULT_WINDOW, &mut reruns);
        assert_eq!(ran, MAX_RERUNS);
        assert_eq!(got.status, StepStatus::Failed);
    }

    #[test]
    fn the_window_is_shared_across_re_runs() {
        // Down 3 probes, back (re-run fails with the runtime down again), then down for good.
        let fake = Fake::new(3);
        let mut reruns = vec![out(StepStatus::Failed, DOWN)];
        let (got, ran) = {
            // Probes 0-2 find it down, probe 3 answers (the unit re-runs), every later one is down.
            let probes = Cell::new(0u32);
            let mut down_now = || {
                let n = probes.get();
                probes.set(n + 1);
                n != 3
            };
            let mut sleep = |d: Duration| {
                fake.clock.set(fake.clock.get() + d);
                true
            };
            let elapsed = || fake.clock.get();
            let mut notify = |_: WaitPhase, _: Duration| {};
            let mut ran = 0;
            let mut rerun = || {
                ran += 1;
                reruns.remove(0)
            };
            let got = ride_out(
                out(StepStatus::Failed, DOWN),
                "claude",
                DEFAULT_WINDOW,
                Host {
                    down_now: &mut down_now,
                    sleep: &mut sleep,
                    elapsed: &elapsed,
                    notify: &mut notify,
                },
                &mut rerun,
            );
            (got, ran)
        };
        assert_eq!(ran, 1);
        assert!(is_runtime_unavailable(&got.output), "{}", got.output);
        assert_eq!(
            fake.clock.get(),
            DEFAULT_WINDOW,
            "one window in total, not one per re-run"
        );
    }

    #[test]
    fn a_cancelled_wait_returns_the_failure_as_it_was() {
        let mut fake = Fake::new(u32::MAX);
        fake.cancel_after_sleeps = Some(2);
        let (got, ran) = fake.ride(
            out(StepStatus::Failed, DOWN),
            DEFAULT_WINDOW,
            &mut Vec::new(),
        );
        assert_eq!(ran, 0);
        assert_eq!(got.output, DOWN);
        assert_eq!(fake.sleeps.get(), 2);
    }

    #[test]
    fn a_zero_window_escalates_at_once() {
        let fake = Fake::new(u32::MAX);
        let (got, _) = fake.ride(
            out(StepStatus::Failed, DOWN),
            Duration::ZERO,
            &mut Vec::new(),
        );
        assert!(is_runtime_unavailable(&got.output));
        assert_eq!(fake.sleeps.get(), 0);
    }

    #[test]
    fn the_window_reads_its_env_and_defaults() {
        assert_eq!(window_from(None), DEFAULT_WINDOW);
        assert_eq!(window_from(Some("45")), Duration::from_secs(45));
        assert_eq!(window_from(Some("0")), Duration::ZERO);
        assert_eq!(window_from(Some("soon")), DEFAULT_WINDOW);
    }

    #[test]
    fn runtime_fault_words_are_recognised_and_other_failures_are_not() {
        assert!(names_runtime_fault(DOWN));
        assert!(names_runtime_fault(
            "error during connect: Get \"http://%2F%2F.%2Fpipe\": EOF"
        ));
        assert!(names_runtime_fault(
            "Cannot connect to Podman. Please verify your connection"
        ));
        assert!(!names_runtime_fault(
            "(cli `claude` exited 1) Not logged in"
        ));
        assert!(!names_runtime_fault(
            "no_seat_image: the seat's container image is not defined"
        ));
    }
}
