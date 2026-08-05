//! When a governance decision was actually made (FINDING-017).
//!
//! # The defect
//!
//! Every conformance claim this engine minted was stamped from a hardcoded constant:
//!
//! ```ignore
//! pub const EVAL_AT_BASE: i64 = 1_750_000_000;      // 2025-06-15T14:13:20Z
//! let evaluated_at = EVAL_AT_BASE + unit.ord as i64;
//! ```
//!
//! So the audit ledger reported mid-June 2025 for every decision ever made, on every host, in
//! every run — and the only variation was `ord`, which made the *unit index* masquerade as a
//! clock. `pipeline.rs` added `+ 1_000_000` on top to keep two claim families from colliding,
//! which is a timestamp field being used as a namespace.
//!
//! For a system whose entire argument is that "done" must be re-derived from evidence, an
//! evidence record that cannot say WHEN a decision was taken is not a small thing. You cannot
//! order two denials, tell a replay from the original, or answer "was this decided before or
//! after the policy changed".
//!
//! It also existed TWICE — `execute.rs` (pub) and `gate_hook.rs` (private) each declared their own
//! copy of the same magic number, free to drift. One clock, one definition.
//!
//! # Why there is no override seam
//!
//! The obvious way to keep tests deterministic is an env var. Deliberately not done: a governance
//! record whose timestamp can be set by the environment is a forgeable audit trail, and the whole
//! point of this record is that it is not forgeable. Tests assert BOUNDS (a stamp taken before the
//! call ≤ the claim ≤ one taken after), which is stronger anyway — it proves a real clock was read
//! rather than that a constant was echoed back.

use std::time::{SystemTime, UNIX_EPOCH};

/// Unix seconds, now. The stamp for a governance claim.
///
/// Saturates at 0 rather than panicking if the host clock predates the epoch: refusing to record a
/// decision because the clock is wrong would turn a cosmetic problem into a governance outage, and
/// a 0 stamp is visibly bogus in a way a panic's absence is not.
pub fn eval_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant, stated as a bound rather than a value — which is what makes it a real test
    /// of "a clock was read" instead of "a constant was returned".
    #[test]
    fn a_claim_is_stamped_from_the_wall_clock() {
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let stamped = eval_now();
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(
            before <= stamped && stamped <= after,
            "eval_now() returned {stamped}, outside [{before}, {after}] — not a real clock read"
        );
    }

    /// The specific regression. 1_750_000_000 is 2025-06-15; any run of this suite happens after
    /// it, so a return of the old constant is detectable without pinning a date of our own.
    #[test]
    fn the_old_hardcoded_base_is_gone() {
        const OLD_EVAL_AT_BASE: i64 = 1_750_000_000;
        // Allow for `+ ord` and pipeline.rs's `+ 1_000_000` offset — the whole synthetic band.
        let synthetic_band = OLD_EVAL_AT_BASE..=OLD_EVAL_AT_BASE + 2_000_000;
        assert!(
            !synthetic_band.contains(&eval_now()),
            "the stamp still falls in the old synthetic band around {OLD_EVAL_AT_BASE}"
        );
    }
}
