//! Per-unit coalescing throttle for the live-output stream ([`crate::event::CoreEvent::UnitOutputDelta`]).
//!
//! The raw delta stream (`Command::CliOutputDelta` → `CoreEvent::CliOutputDelta`) arrives as
//! thousands of chunks per run — cheap to fan out in-process, but deliberately EXCLUDED from the
//! durable event log ([`crate::event_log`]) and too chatty to relay off-process. This module is the
//! actor-owned coalescer that turns that firehose into a bounded stream a remote consumer can
//! actually subscribe to:
//!
//! - **per unit** (`(run_id, ord)` key), so parallel runs never share a buffer;
//! - **flush at most every [`FLUSH_INTERVAL`]** — OR immediately once **[`FLUSH_BYTES`]** of text
//!   is pending, whichever comes first (a unit's FIRST chunk also flushes immediately, so live
//!   output starts streaming without a startup delay);
//! - **each flushed text is capped at [`FLUSH_BYTES`]** via a head+tail elision ([`elide`]) — the
//!   tail is kept because fatal lines come last by convention (same argument as the
//!   `apply_step_result` denial snippet).
//!
//! State lives on the single-writer actor thread (the actor loop owns the instance); workers only
//! ever send raw chunks. A buffer that stops receiving chunks mid-window is drained by the actor
//! when the unit's result folds ([`UnitOutputThrottle::drain_run`] on `ApplyStepResult`) or
//! discarded on `CancelRun` — so pending text never outlives its run and the map cannot grow
//! unboundedly.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Flush cadence floor: at most one coalesced flush per unit per this interval, unless the size
/// threshold fires first.
pub(crate) const FLUSH_INTERVAL: Duration = Duration::from_millis(500);

/// Dual-purpose 2KB bound: the pending-size threshold that forces an immediate flush, AND the
/// hard cap on any single flushed `text` (enforced by [`elide`]).
pub(crate) const FLUSH_BYTES: usize = 2048;

/// One unit's coalescing state.
struct Accum {
    /// Text received since the last flush.
    pending: String,
    /// When this unit last flushed. `None` until the first flush — which is why a unit's first
    /// chunk streams immediately instead of idling half a second.
    last_flush: Option<Instant>,
}

/// The actor's per-unit output coalescer. See the module docs for the contract.
#[derive(Default)]
pub(crate) struct UnitOutputThrottle {
    units: HashMap<(String, u32), Accum>,
}

impl UnitOutputThrottle {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Append `chunk` to the unit's pending buffer. Returns `Some(text)` — the coalesced, elided
    /// text to emit as one `UnitOutputDelta` — when a flush is due at `now`; `None` while the
    /// buffer is still accumulating inside the window.
    ///
    /// `now` is a parameter (not read internally) so the boundary behavior is testable with
    /// fabricated instants.
    pub(crate) fn push(
        &mut self,
        run_id: &str,
        ord: u32,
        chunk: &str,
        now: Instant,
    ) -> Option<String> {
        let acc = self
            .units
            .entry((run_id.to_string(), ord))
            .or_insert_with(|| Accum {
                pending: String::new(),
                last_flush: None,
            });
        acc.pending.push_str(chunk);
        if acc.pending.is_empty() {
            return None;
        }
        let due = match acc.last_flush {
            None => true, // first output of the unit — stream it immediately
            Some(t) => now.duration_since(t) >= FLUSH_INTERVAL || acc.pending.len() >= FLUSH_BYTES,
        };
        if !due {
            return None;
        }
        acc.last_flush = Some(now);
        Some(elide(std::mem::take(&mut acc.pending)))
    }

    /// Final drain for `run_id`: return every non-empty pending buffer as `(ord, elided text)`
    /// (ord-ascending) and drop ALL of the run's throttle state. Called by the actor when the
    /// worker's stream is over (the step result arrived) — so the live stream never loses its
    /// tail — and on cancel, where the caller discards the result.
    pub(crate) fn drain_run(&mut self, run_id: &str) -> Vec<(u32, String)> {
        let mut out: Vec<(u32, String)> = Vec::new();
        self.units.retain(|(rid, ord), acc| {
            if rid != run_id {
                return true;
            }
            if !acc.pending.is_empty() {
                out.push((*ord, elide(std::mem::take(&mut acc.pending))));
            }
            false
        });
        out.sort_by_key(|(ord, _)| *ord);
        out
    }
}

/// Cap `text` at [`FLUSH_BYTES`] bytes. Short text passes through untouched; anything longer keeps
/// its head and its TAIL around an elision marker (fatal lines come last by convention), always
/// cutting on char boundaries so multi-byte text can never split into invalid UTF-8.
pub(crate) fn elide(text: String) -> String {
    if text.len() <= FLUSH_BYTES {
        return text;
    }
    // Head + tail + marker must stay under FLUSH_BYTES: 1000 + 1000 + a ~40-byte marker.
    const KEEP: usize = 1000;
    let head_end = floor_char_boundary(&text, KEEP);
    let tail_start = ceil_char_boundary(&text, text.len() - KEEP);
    format!(
        "{}\n[… {} bytes elided …]\n{}",
        &text[..head_end],
        tail_start - head_end,
        &text[tail_start..]
    )
}

/// Largest char boundary `<= i` (stable-Rust stand-in for `str::floor_char_boundary`).
fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest char boundary `>= i`.
fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit's first chunk must stream immediately — a fresh buffer has never flushed, so there
    /// is no window to wait out. Otherwise every unit's live view starts half a second late.
    #[test]
    fn first_chunk_flushes_immediately() {
        let mut t = UnitOutputThrottle::new();
        let now = Instant::now();
        assert_eq!(t.push("r", 1, "hello", now).as_deref(), Some("hello"));
    }

    /// Inside the window and under the size threshold, chunks accumulate silently; the flush at
    /// the 500ms boundary carries the COALESCED text, in arrival order.
    #[test]
    fn coalesces_across_the_500ms_boundary() {
        let mut t = UnitOutputThrottle::new();
        let t0 = Instant::now();
        assert!(t.push("r", 1, "a", t0).is_some(), "first chunk flushes");
        // Strictly inside the window: accumulate, no emission.
        assert_eq!(t.push("r", 1, "b", t0 + Duration::from_millis(100)), None);
        assert_eq!(t.push("r", 1, "c", t0 + Duration::from_millis(499)), None);
        // AT the boundary: the flush carries everything pending, coalesced in order.
        assert_eq!(
            t.push("r", 1, "d", t0 + Duration::from_millis(500))
                .as_deref(),
            Some("bcd"),
            "the 500ms flush must carry the coalesced window, not just the last chunk"
        );
        // The window restarts from the flush: the next chunk accumulates again.
        assert_eq!(t.push("r", 1, "e", t0 + Duration::from_millis(600)), None);
    }

    /// Crossing 2KB pending forces a flush NOW — a fast producer must not be able to pile up
    /// half a second of unbounded output between time-based flushes.
    #[test]
    fn size_threshold_flushes_inside_the_window() {
        let mut t = UnitOutputThrottle::new();
        let t0 = Instant::now();
        assert!(t.push("r", 1, "x", t0).is_some());
        let big = "y".repeat(FLUSH_BYTES + 100); // pending crosses the threshold on this push
        let flushed = t
            .push("r", 1, &big, t0 + Duration::from_millis(1))
            .expect("size threshold must flush even 1ms after the last flush");
        assert!(flushed.len() <= FLUSH_BYTES, "flushed text is capped");
        assert!(flushed.contains("bytes elided"), "over-cap text is elided");
    }

    /// The cap is head+TAIL: the end of the text survives (fatal lines come last), the middle is
    /// elided, and the result never splits a multi-byte char.
    #[test]
    fn elide_keeps_head_and_tail_on_char_boundaries() {
        let text = format!("HEAD{}TAIL", "é".repeat(3000)); // multi-byte middle
        let out = elide(text);
        assert!(out.len() <= FLUSH_BYTES, "capped at 2KB, got {}", out.len());
        assert!(out.starts_with("HEAD"), "head survives");
        assert!(
            out.ends_with("TAIL"),
            "tail survives — fatal lines come last"
        );
        assert!(out.contains("bytes elided"));
        // Under the cap: untouched.
        assert_eq!(elide("short".to_string()), "short");
    }

    /// The final drain returns the pending tail (elided, ord-ascending) and drops the run's state
    /// entirely — other runs' buffers are untouched.
    #[test]
    fn drain_run_returns_the_tail_and_clears_only_that_run() {
        let mut t = UnitOutputThrottle::new();
        let t0 = Instant::now();
        assert!(t.push("r", 1, "first", t0).is_some());
        assert_eq!(
            t.push("r", 1, "tail-1", t0 + Duration::from_millis(1)),
            None
        );
        assert!(t.push("r", 2, "tail-2", t0).is_some()); // unit 2: flushed, nothing pending
        assert_eq!(
            t.push("r", 2, "tail-3", t0 + Duration::from_millis(1)),
            None
        );
        assert!(t.push("other", 1, "keep", t0).is_some());
        assert_eq!(
            t.push("other", 1, "kept-pending", t0 + Duration::from_millis(1)),
            None
        );

        let drained = t.drain_run("r");
        assert_eq!(
            drained,
            vec![(1, "tail-1".to_string()), (2, "tail-3".to_string())],
            "every unit's pending tail, ord-ascending"
        );
        assert!(
            t.drain_run("r").is_empty(),
            "state dropped — a second drain is empty"
        );
        assert_eq!(
            t.drain_run("other"),
            vec![(1, "kept-pending".to_string())],
            "an unrelated run's buffer survives another run's drain"
        );
    }

    /// Empty chunks never produce an empty flush.
    #[test]
    fn empty_chunks_do_not_flush() {
        let mut t = UnitOutputThrottle::new();
        assert_eq!(t.push("r", 1, "", Instant::now()), None);
        assert!(t.drain_run("r").is_empty());
    }
}
