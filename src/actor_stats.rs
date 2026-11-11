//! (core#1390) How long the single-writer actor spends on each command, and how many commands wait
//! behind it.
//!
//! The actor runs one command at a time, so ONE long command delays every read and gate answer
//! queued behind it. These numbers name the command: each one is timed by variant, a command over
//! the slow threshold (`WICKED_ACTOR_SLOW_MS`, default 1000) is logged on stderr with its duration
//! and the backlog behind it, and [`crate::Core::actor_stats`] reports the command running now (and
//! for how long), the backlog, and p50/p99/max per variant over the last [`RING`] runs of each.
//!
//! Observability only: nothing here gates, orders or changes a command.

use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Durations kept per command variant (the window p50/p99 are read over).
pub(crate) const RING: usize = 256;
/// Slow commands kept, newest last.
const SLOW_KEPT: usize = 32;
/// The default slow threshold.
const DEFAULT_SLOW_MS: u64 = 1000;

/// The slow threshold: `WICKED_ACTOR_SLOW_MS` (whole milliseconds) or [`DEFAULT_SLOW_MS`]. An
/// unparsable value keeps the default.
pub(crate) fn slow_threshold_from_env() -> Duration {
    parse_slow_ms(std::env::var("WICKED_ACTOR_SLOW_MS").ok().as_deref())
}

fn parse_slow_ms(raw: Option<&str>) -> Duration {
    Duration::from_millis(
        raw.and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_SLOW_MS),
    )
}

#[derive(Default)]
struct Inner {
    per: HashMap<&'static str, (u64, VecDeque<u64>)>,
    slow: VecDeque<SlowCommand>,
}

/// The actor's shared counters. One per actor; the actor writes them, `Core` reads them.
#[derive(Default)]
pub(crate) struct ActorStats {
    backlog: AtomicUsize,
    max_backlog: AtomicUsize,
    inner: Mutex<Inner>,
    /// The command the actor is running now, and since when.
    running: Mutex<Option<(&'static str, Instant)>>,
    /// Commands begun and ended: equal exactly when the actor is between two commands (every
    /// write a command made has committed and the next has not begun). The read model pins its
    /// snapshot on that (the read path of `Core`'s run reads).
    started: AtomicU64,
    finished: AtomicU64,
    /// The actor's loop has ended (shutdown, an early return, or a panic unwinding it).
    stopped: std::sync::atomic::AtomicBool,
}

/// Marks the actor stopped when dropped: held for the whole of the actor thread's run, so every way
/// out of it (a return, a panic unwinding it) is seen.
pub(crate) struct StopGuard<'a>(pub(crate) &'a ActorStats);
impl Drop for StopGuard<'_> {
    fn drop(&mut self) {
        self.0.stopped.store(true, Ordering::SeqCst);
    }
}

/// The command the actor is running at the moment of the read.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RunningCommand {
    pub command: String,
    /// How long it has run so far.
    pub for_ms: u64,
}

/// One command that ran past the slow threshold.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SlowCommand {
    pub command: String,
    pub ms: u64,
    /// Commands waiting behind it when it started.
    pub backlog: usize,
    /// When it finished (Unix ms).
    pub at_ms: u64,
}

/// One command variant's timing over its last [`RING`] runs.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CommandTiming {
    pub command: String,
    /// Every run since the actor started.
    pub count: u64,
    pub p50_ms: u64,
    pub p99_ms: u64,
    pub max_ms: u64,
}

/// What [`crate::Core::actor_stats`] answers.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ActorStatsView {
    /// The command running now (`None` while the actor waits for work). A long `for_ms` here is
    /// the command every queued read and gate answer is waiting behind.
    pub running: Option<RunningCommand>,
    /// Commands that were waiting when the actor last became free (the next one included).
    pub backlog: usize,
    /// The most that ever waited at once.
    pub max_backlog: usize,
    pub slow_threshold_ms: u64,
    /// Slowest p99 first.
    pub commands: Vec<CommandTiming>,
    pub slow: Vec<SlowCommand>,
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let ix = ((sorted.len() as f64) * p).ceil() as usize;
    sorted[ix.clamp(1, sorted.len()) - 1]
}

impl ActorStats {
    /// The counters of an actor that has not finished starting: it counts as busy (its startup —
    /// redrive, orphan reports — writes the store) until [`ActorStats::started_up`].
    pub(crate) fn starting() -> Self {
        let s = Self::default();
        s.started.store(1, Ordering::SeqCst);
        s
    }

    /// The actor finished its startup work and enters its command loop.
    pub(crate) fn started_up(&self) {
        if self.started.load(Ordering::SeqCst) > self.finished.load(Ordering::SeqCst) {
            self.finished.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The actor took `Shutdown` (or its thread ended): no read is answered off it from now on.
    pub(crate) fn mark_stopped(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    /// The actor's loop has ended or is ending.
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Commands begun so far (startup counts as one).
    pub(crate) fn generation(&self) -> u64 {
        self.started.load(Ordering::SeqCst)
    }

    /// Wait up to `budget` for a moment between two commands and return its generation; `None`
    /// when the actor stayed busy past the budget.
    pub(crate) fn between_commands(&self, budget: Duration) -> Option<u64> {
        let deadline = Instant::now() + budget;
        loop {
            let started = self.started.load(Ordering::SeqCst);
            if self.finished.load(Ordering::SeqCst) == started {
                return Some(started);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The actor dequeued a command with `n` more waiting.
    pub(crate) fn set_backlog(&self, n: usize) {
        self.backlog.store(n, Ordering::Relaxed);
        self.max_backlog.fetch_max(n, Ordering::Relaxed);
    }

    /// Record one command's run; logs it when it took `slow` or longer.
    pub(crate) fn record(
        &self,
        name: &'static str,
        took: Duration,
        backlog: usize,
        slow: Duration,
    ) {
        let ms = took.as_millis().min(u128::from(u64::MAX)) as u64;
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let (count, ring) = inner.per.entry(name).or_default();
        *count += 1;
        if ring.len() == RING {
            ring.pop_front();
        }
        ring.push_back(ms);
        if took >= slow {
            eprintln!("wicked-core: slow actor command {name} took {ms} ms (backlog {backlog})");
            if inner.slow.len() == SLOW_KEPT {
                inner.slow.pop_front();
            }
            inner.slow.push_back(SlowCommand {
                command: name.to_string(),
                ms,
                backlog,
                at_ms: crate::interaction::now_millis().max(0) as u64,
            });
        }
    }

    pub(crate) fn view(&self, slow: Duration) -> ActorStatsView {
        let running =
            self.running
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .map(|(name, since)| RunningCommand {
                    command: name.to_string(),
                    for_ms: since.elapsed().as_millis() as u64,
                });
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let mut commands: Vec<CommandTiming> = inner
            .per
            .iter()
            .map(|(name, (count, ring))| {
                let mut sorted: Vec<u64> = ring.iter().copied().collect();
                sorted.sort_unstable();
                CommandTiming {
                    command: name.to_string(),
                    count: *count,
                    p50_ms: pct(&sorted, 0.50),
                    p99_ms: pct(&sorted, 0.99),
                    max_ms: sorted.last().copied().unwrap_or(0),
                }
            })
            .collect();
        commands.sort_by(|a, b| b.p99_ms.cmp(&a.p99_ms).then(a.command.cmp(&b.command)));
        ActorStatsView {
            running,
            backlog: self.backlog.load(Ordering::Relaxed),
            max_backlog: self.max_backlog.load(Ordering::Relaxed),
            slow_threshold_ms: slow.as_millis() as u64,
            commands,
            slow: inner.slow.iter().cloned().collect(),
        }
    }
}

/// Times one command from construction to drop, so every way out of the actor's match arm — a
/// `continue` included — is counted.
pub(crate) struct Timed<'a> {
    stats: &'a ActorStats,
    name: &'static str,
    backlog: usize,
    slow: Duration,
    start: Instant,
}

impl<'a> Timed<'a> {
    pub(crate) fn start(
        stats: &'a ActorStats,
        name: &'static str,
        backlog: usize,
        slow: Duration,
    ) -> Self {
        stats.started.fetch_add(1, Ordering::SeqCst);
        let start = Instant::now();
        *stats.running.lock().unwrap_or_else(|p| p.into_inner()) = Some((name, start));
        Self {
            stats,
            name,
            backlog,
            slow,
            start,
        }
    }
}

impl Drop for Timed<'_> {
    fn drop(&mut self) {
        *self.stats.running.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.stats
            .record(self.name, self.start.elapsed(), self.backlog, self.slow);
        // Last: the command's writes are all committed by now.
        self.stats.finished.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slow_threshold_reads_whole_milliseconds_and_keeps_the_default_otherwise() {
        assert_eq!(parse_slow_ms(None), Duration::from_millis(1000));
        assert_eq!(parse_slow_ms(Some("250")), Duration::from_millis(250));
        assert_eq!(parse_slow_ms(Some(" 5 ")), Duration::from_millis(5));
        assert_eq!(parse_slow_ms(Some("fast")), Duration::from_millis(1000));
        assert_eq!(parse_slow_ms(Some("-1")), Duration::from_millis(1000));
    }

    #[test]
    fn timings_report_percentiles_per_command_and_keep_only_slow_runs() {
        let s = ActorStats::default();
        let slow = Duration::from_millis(100);
        for ms in 1..=100u64 {
            s.record(
                "Projects",
                Duration::from_millis(ms),
                0,
                Duration::from_secs(3600),
            );
        }
        s.record("ConfirmGate", Duration::from_millis(150), 4, slow);
        s.record("ConfirmGate", Duration::from_millis(20), 0, slow);
        s.set_backlog(7);
        s.set_backlog(2);
        let v = s.view(slow);
        assert_eq!(v.backlog, 2);
        assert_eq!(v.max_backlog, 7);
        assert_eq!(v.slow_threshold_ms, 100);
        let p = v.commands.iter().find(|c| c.command == "Projects").unwrap();
        assert_eq!((p.count, p.p50_ms, p.p99_ms, p.max_ms), (100, 50, 99, 100));
        let g = v
            .commands
            .iter()
            .find(|c| c.command == "ConfirmGate")
            .unwrap();
        assert_eq!((g.count, g.max_ms), (2, 150));
        assert_eq!(v.commands[0].command, "ConfirmGate", "slowest p99 first");
        assert_eq!(
            v.slow,
            vec![SlowCommand {
                command: "ConfirmGate".into(),
                ms: 150,
                backlog: 4,
                at_ms: v.slow[0].at_ms,
            }],
            "only the run at or past the threshold is kept"
        );
    }

    #[test]
    fn the_ring_keeps_the_last_runs_but_the_count_keeps_them_all() {
        let s = ActorStats::default();
        for _ in 0..RING {
            s.record(
                "Ping",
                Duration::from_millis(900),
                0,
                Duration::from_secs(3600),
            );
        }
        for _ in 0..RING {
            s.record(
                "Ping",
                Duration::from_millis(1),
                0,
                Duration::from_secs(3600),
            );
        }
        let v = s.view(Duration::from_secs(3600));
        let p = &v.commands[0];
        assert_eq!((p.count, p.max_ms, p.p99_ms), (2 * RING as u64, 1, 1));
    }

    #[test]
    fn between_commands_is_when_every_begun_command_has_ended() {
        let s = ActorStats::starting();
        assert_eq!(
            s.between_commands(Duration::from_millis(5)),
            None,
            "busy while starting up"
        );
        s.started_up();
        s.started_up();
        assert_eq!(s.between_commands(Duration::ZERO), Some(1), "idempotent");
        {
            let _t = Timed::start(&s, "ConfirmGate", 0, Duration::from_secs(3600));
            assert_eq!(s.between_commands(Duration::from_millis(5)), None);
            assert_eq!(s.generation(), 2);
        }
        assert_eq!(s.between_commands(Duration::ZERO), Some(2));
        let s = std::sync::Arc::new(ActorStats::starting());
        s.started_up();
        let (began_tx, began) = std::sync::mpsc::channel();
        let t = {
            let s = s.clone();
            std::thread::spawn(move || {
                let _t = Timed::start(&s, "Projects", 0, Duration::from_secs(3600));
                began_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(30));
            })
        };
        began.recv().unwrap();
        assert_eq!(
            s.between_commands(Duration::ZERO),
            None,
            "busy while it runs"
        );
        assert_eq!(
            s.between_commands(Duration::from_secs(5)),
            Some(2),
            "a short command is waited out"
        );
        t.join().unwrap();
    }

    #[test]
    fn a_timed_guard_records_on_every_way_out() {
        let s = ActorStats::default();
        for i in 0..3 {
            let _t = Timed::start(&s, "Sessions", 0, Duration::from_secs(3600));
            assert_eq!(
                s.view(Duration::from_secs(1)).running.map(|r| r.command),
                Some("Sessions".to_string()),
                "a running command is visible while it runs"
            );
            if i == 1 {
                continue;
            }
        }
        let v = s.view(Duration::from_secs(1));
        assert_eq!(v.commands[0].count, 3);
        assert_eq!(v.running, None, "and gone once it ends");
    }
}
