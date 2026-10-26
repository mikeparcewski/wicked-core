//! (PRODUCT-WORKS J4) A container runtime blip is an infrastructure fault, never a seat failure.
//!
//! On the test rig Rancher Desktop restarted mid-run: docker was gone for about ten minutes, the
//! claude and copilot seats each "failed" their unit with `container_runtime_down`, the failover
//! ladder counted both as seat failures, and the run parked at a dead-seat card that offered only
//! a quota-benched seat. Nothing was wrong with either seat.
//!
//! The wrapped runner now reads a failed governed seat-container unit against a FRESH probe of
//! the runtime the seats use (the cached [`crate::container_floor::runtime`] answer outlives a
//! restart). While the runtime is down it waits, with backoff, up to a bounded window ([`WINDOW_ENV`], default
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

/// Whether a failed unit's output is the floor's OWN `container_runtime_down` refusal
/// ([`crate::worker_sandbox::floor_refusal_text`]): the runner's first line, never a substring of
/// a worker's transcript (a unit that greps this file, or logs an unrelated connection error, is
/// not a runtime fault — xreview on #1129).
pub(crate) fn is_floor_runtime_refusal(output: &str) -> bool {
    let first = output.trim_start().lines().next().unwrap_or("");
    first.starts_with("(seat '")
        && first.contains("its seat container did not arm — container_runtime_down")
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
    /// A fresh probe of the runtime the seats use: its CLI is installed and its engine does not
    /// answer.
    pub(crate) down_now: &'a mut dyn FnMut() -> bool,
    /// Pause for the duration; `false` when the unit's launch was cancelled meanwhile.
    pub(crate) sleep: &'a mut dyn FnMut(Duration) -> bool,
    /// Whether the unit's launch was cancelled (its run ended, a reassign superseded it).
    pub(crate) cancelled: &'a dyn Fn() -> bool,
    /// Time since an arbitrary fixed start.
    pub(crate) elapsed: &'a dyn Fn() -> Duration,
    /// Report one step of the wait and the time waited so far.
    pub(crate) notify: &'a mut dyn FnMut(WaitPhase, Duration),
}

/// Ride a unit's failure out when the container runtime caused it: wait for the runtime, then
/// re-run the unit (`rerun`) on the same seat. It is the runtime's fault when a fresh probe finds
/// it down, or when the output is the floor's own `container_runtime_down` refusal (a blip already
/// over by the probe). Any other outcome is returned unchanged: a success, a seat's own failure
/// while the runtime answers, a cancelled launch. The window bounds the time spent WAITING (summed
/// over the unit's re-runs; a re-run's own execution never counts against it). A runtime that
/// stays down for the whole window, or still faults after [`MAX_RERUNS`] re-runs, yields
/// [`unavailable_output`] — never the raw failure, which the actor would read as the seat's.
pub(crate) fn ride_out(
    first: StepOutput,
    cli: &str,
    window: Duration,
    host: Host<'_>,
    rerun: &mut dyn FnMut() -> StepOutput,
) -> StepOutput {
    let mut out = first;
    let mut reruns = 0;
    // Time spent waiting so far, over every wait of this unit.
    let mut waited = Duration::ZERO;
    loop {
        if out.status != StepStatus::Failed {
            return out;
        }
        let down = (host.down_now)();
        if !down && !is_floor_runtime_refusal(&out.output) {
            return out;
        }
        if (host.cancelled)() {
            return out;
        }
        if reruns >= MAX_RERUNS {
            (host.notify)(WaitPhase::Unavailable, waited);
            return unavailable_output(&out, cli, waited);
        }
        if down {
            let began = (host.elapsed)();
            let before = waited;
            let mut step = FIRST_STEP;
            loop {
                waited = before + (host.elapsed)().saturating_sub(began);
                if waited >= window {
                    (host.notify)(WaitPhase::Unavailable, waited);
                    return unavailable_output(&out, cli, waited);
                }
                (host.notify)(WaitPhase::Waiting, waited);
                if !(host.sleep)(step.min(window - waited)) {
                    return out;
                }
                step = (step * 2).min(MAX_STEP);
                let still_down = (host.down_now)();
                // A probe can take its own timeout: the launch may have ended meanwhile.
                if (host.cancelled)() {
                    return out;
                }
                if !still_down {
                    break;
                }
            }
            waited = before + (host.elapsed)().saturating_sub(began);
        }
        // The runtime answers (again): the unit re-runs on the same seat. `recovered` follows a
        // wait only (a floor refusal the probe already finds healthy re-runs without a frame).
        if down {
            (host.notify)(WaitPhase::Recovered, waited);
        }
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

    /// The floor's own refusal, exactly as `worker_sandbox::floor_refusal_text` renders it.
    fn down_text() -> String {
        crate::worker_sandbox::floor_refusal_text(
            "claude",
            crate::worker_sandbox::FloorUnarmed::ContainerRuntimeDown,
        )
    }

    /// A fake host: probe `n` answers `down(n)`; sleeping advances the clock, and so can a probe
    /// (`probe_cost`) and a re-run (`rerun_cost`); every notify is recorded.
    struct Fake {
        probes: Cell<u32>,
        down: Box<dyn Fn(u32) -> bool>,
        clock: Cell<Duration>,
        probe_cost: Duration,
        rerun_cost: Duration,
        notes: RefCell<Vec<(WaitPhase, u64)>>,
        cancel_after_sleeps: Option<u32>,
        cancel_after_probes: Option<u32>,
        sleeps: Cell<u32>,
    }

    impl Fake {
        fn new(down_probes: u32) -> Self {
            Self::with(move |n| n < down_probes)
        }

        fn with(down: impl Fn(u32) -> bool + 'static) -> Self {
            Fake {
                probes: Cell::new(0),
                down: Box::new(down),
                clock: Cell::new(Duration::ZERO),
                probe_cost: Duration::ZERO,
                rerun_cost: Duration::ZERO,
                notes: RefCell::new(Vec::new()),
                cancel_after_sleeps: None,
                cancel_after_probes: None,
                sleeps: Cell::new(0),
            }
        }

        fn cancelled(&self) -> bool {
            self.cancel_after_sleeps
                .is_some_and(|n| self.sleeps.get() >= n)
                || self
                    .cancel_after_probes
                    .is_some_and(|n| self.probes.get() >= n)
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
                self.clock.set(self.clock.get() + self.probe_cost);
                (self.down)(n)
            };
            let mut sleep = |d: Duration| {
                self.clock.set(self.clock.get() + d);
                self.sleeps.set(self.sleeps.get() + 1);
                !self.cancelled()
            };
            let cancelled = || self.cancelled();
            let elapsed = || self.clock.get();
            let mut notify =
                |p: WaitPhase, w: Duration| self.notes.borrow_mut().push((p, w.as_secs()));
            let mut ran = 0;
            let mut rerun = || {
                ran += 1;
                self.clock.set(self.clock.get() + self.rerun_cost);
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
                    cancelled: &cancelled,
                    elapsed: &elapsed,
                    notify: &mut notify,
                },
                &mut rerun,
            );
            (got, ran)
        }

        fn phases(&self) -> Vec<WaitPhase> {
            self.notes.borrow().iter().map(|n| n.0).collect()
        }
    }

    #[test]
    fn a_runtime_that_comes_back_re_runs_the_unit_on_the_same_seat() {
        let fake = Fake::new(3);
        let (got, ran) = fake.ride(
            out(StepStatus::Failed, &down_text()),
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
            out(StepStatus::Failed, &down_text()),
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
        assert_eq!(fake.phases().last(), Some(&WaitPhase::Unavailable));
        // Backoff capped at 30 s: 2,4,8,16,30,30,30,30,30 = 180.
        assert_eq!(fake.sleeps.get(), 9);
    }

    #[test]
    fn the_runners_unavailable_line_is_never_read_as_a_seat_refusal() {
        let got = unavailable_output(
            &out(StepStatus::Failed, &down_text()),
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

    /// xreview on #1129 (codex 4, agy 1): runtime words inside a worker's transcript are data, not
    /// a runtime fault — with the runtime answering, nothing re-runs.
    #[test]
    fn runtime_words_in_a_transcript_never_re_run_a_unit_while_the_runtime_answers() {
        for text in [
            "(cli `claude` exited 1) application service: error during connect: TLS error",
            "(cli `claude` exited 1) docker: Cannot connect to the Docker daemon. Is the docker daemon running?",
            "(cli `claude` exited 1) grep: src/worker_sandbox.rs: container_runtime_down",
            "test failed\n(seat 'claude' could not run this governed unit: its seat container did not arm — container_runtime_down",
        ] {
            let fake = Fake::new(0);
            let (got, ran) = fake.ride(out(StepStatus::Failed, text), DEFAULT_WINDOW, &mut Vec::new());
            assert_eq!((ran, got.output.as_str()), (0, text));
            assert!(fake.notes.borrow().is_empty());
        }
    }

    #[test]
    fn a_success_is_never_probed() {
        let fake = Fake::new(u32::MAX);
        let (got, _) = fake.ride(out(StepStatus::Ok, "fine"), DEFAULT_WINDOW, &mut Vec::new());
        assert_eq!(got.status, StepStatus::Ok);
        assert_eq!(fake.probes.get(), 0);
    }

    #[test]
    fn the_floors_refusal_re_runs_when_the_blip_is_already_over() {
        let fake = Fake::new(0);
        let (got, ran) = fake.ride(
            out(StepStatus::Failed, &down_text()),
            DEFAULT_WINDOW,
            &mut Vec::new(),
        );
        assert_eq!((got.status, ran), (StepStatus::Ok, 1));
        assert!(
            fake.notes.borrow().is_empty(),
            "no wait, no recovered frame"
        );
    }

    /// xreview on #1129 (codex 2, agy 2): a runtime that keeps faulting through every re-run ends
    /// as the runner's infrastructure line, never as the raw refusal the actor would bench for.
    #[test]
    fn re_runs_are_bounded_and_end_as_infrastructure_when_the_runtime_keeps_faulting() {
        let fake = Fake::new(0);
        let mut reruns: Vec<StepOutput> = (0..10)
            .map(|_| out(StepStatus::Failed, &down_text()))
            .collect();
        let (got, ran) = fake.ride(
            out(StepStatus::Failed, &down_text()),
            DEFAULT_WINDOW,
            &mut reruns,
        );
        assert_eq!(ran, MAX_RERUNS);
        assert!(is_runtime_unavailable(&got.output), "{}", got.output);
    }

    /// agy 3 on #1129: a re-run's own execution never counts against the wait window.
    #[test]
    fn the_window_counts_waiting_only_and_is_shared_across_re_runs() {
        // Probes 0-2 down, probe 3 answers (re-run, which takes 10 minutes and faults again),
        // then down for good.
        let mut fake = Fake::with(|n| n != 3);
        fake.rerun_cost = Duration::from_secs(600);
        let mut reruns = vec![out(StepStatus::Failed, &down_text())];
        let (got, ran) = fake.ride(
            out(StepStatus::Failed, &down_text()),
            DEFAULT_WINDOW,
            &mut reruns,
        );
        assert_eq!(ran, 1);
        assert!(is_runtime_unavailable(&got.output), "{}", got.output);
        assert!(
            got.output.contains("did not answer for 180s"),
            "{}",
            got.output
        );
        assert_eq!(
            fake.clock.get(),
            DEFAULT_WINDOW + Duration::from_secs(600),
            "one 180 s window of waiting in total, plus the re-run's own time"
        );
    }

    #[test]
    fn a_cancelled_wait_returns_the_failure_as_it_was() {
        let mut fake = Fake::new(u32::MAX);
        fake.cancel_after_sleeps = Some(2);
        let (got, ran) = fake.ride(
            out(StepStatus::Failed, &down_text()),
            DEFAULT_WINDOW,
            &mut Vec::new(),
        );
        assert_eq!(ran, 0);
        assert_eq!(got.output, down_text());
        assert_eq!(fake.sleeps.get(), 2);
    }

    /// codex 3 on #1129: a launch cancelled while a probe ran does not re-run, even once the
    /// runtime answers.
    #[test]
    fn a_launch_cancelled_during_a_probe_never_re_runs() {
        // Probe 0 finds it down; the launch is cancelled while probe 1 runs, which then answers.
        let mut fake = Fake::new(1);
        fake.probe_cost = Duration::from_secs(15);
        fake.cancel_after_probes = Some(2);
        let (_, ran) = fake.ride(
            out(StepStatus::Failed, &down_text()),
            DEFAULT_WINDOW,
            &mut Vec::new(),
        );
        assert_eq!(ran, 0);
    }

    #[test]
    fn a_zero_window_escalates_at_once() {
        let fake = Fake::new(u32::MAX);
        let (got, _) = fake.ride(
            out(StepStatus::Failed, &down_text()),
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
    fn only_the_floors_own_runtime_refusal_is_a_runtime_fault() {
        assert!(is_floor_runtime_refusal(&down_text()));
        assert!(!is_floor_runtime_refusal(
            &crate::worker_sandbox::floor_refusal_text(
                "claude",
                crate::worker_sandbox::FloorUnarmed::NoSeatImage,
            )
        ));
        assert!(!is_floor_runtime_refusal(
            "(cli `claude` exited 1) Not logged in"
        ));
    }
}
