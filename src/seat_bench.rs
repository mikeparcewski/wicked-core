//! (PRODUCT-WORKS J4, core#1479) A QUOTA or SIGN-IN bench expires: the run re-probes the seat
//! instead of judging without it for the rest of the run.
//!
//! A seat benched for a run ([`crate::domain::AgentSession::benched_seats`]) used to stay benched
//! until the run ended. A quota or rate limit changes mid-run: a copilot seat benched for quota
//! (429) on one day was still benched 27 hours later, and every review in that run had no distinct
//! judge. So does a sign-in (core#1479): a user who signs codex in after the launch, or a seat a
//! misread transcript benched as `not_logged_in`, stayed out of the run's worker, judge and
//! evaluator pools for good. A missing binary and a host-forced approval policy do not change
//! mid-run, and those benches still never lift by themselves.
//!
//! The policy, in three rules:
//! - **When a bench lifts.** A quota-class bench ([`quota_class`]) carries
//!   [`BenchedSeat::benched_until`]: the provider's reset time when its refusal stated one
//!   ([`provider_reset_at`]: `Retry-After`, "try again in 20 minutes", "resets at <time>"), else a
//!   bounded backoff of 15, then 30, then 60 minutes ([`backoff_ms`], keyed by
//!   [`BenchedSeat::strikes`]). A sign-in bench ([`sign_in_class`]) takes 5, then 10, then 15
//!   minutes ([`SIGN_IN_BACKOFF_MINUTES`]): a person signs a seat in on their own clock, and the
//!   re-probe of a seat still signed out is refused by the floor's broker check before any seat
//!   starts. A bench of any other cause never lifts by itself.
//! - **Re-probe before seating.** A bench that is due ([`due_for_reprobe`]) is not simply dropped.
//!   The seat answers one cheap call first ([`crate::validator::reprobe_seat`]: the judge's own
//!   ungoverned one-shot carrier, with no workdir and no tools asked for). Until it has answered,
//!   it stays benched, so routing never hands it work and no judge draws it.
//! - **A failed re-probe re-benches it for longer.** The seat keeps its bench with one more strike
//!   and the next backoff of the cause its refusal names (or the reset time the new refusal states).
//!
//! The re-probe runs on the worker thread before a unit's judge reads the bench
//! (`cli_runner::run_unit_and_judge`), so a gate whose only distinct judge was benched picks the
//! returned seat. Its outcome rides back on [`crate::workflow::UnitEvidence::bench_reprobes`] and
//! the actor folds it ([`apply_reprobes`]) before anything else reads the session: from then on the
//! seat is in the run's worker pool (failover, the dispatch re-seat), its judge rotation and its
//! distinct-evaluator re-seat again.

use crate::domain::BenchedSeat;
use serde::{Deserialize, Serialize};

/// The backoff, in minutes, for the first, second and every later quota bench in a row.
pub const QUOTA_BACKOFF_MINUTES: [i64; 3] = [15, 30, 60];

/// (core#1479) The backoff, in minutes, for the first, second and every later sign-in bench in a
/// row: a seat signed in mid-run is back within 15 minutes, and a seat still signed out costs one
/// refused arm per probe.
pub const SIGN_IN_BACKOFF_MINUTES: [i64; 3] = [5, 10, 15];

/// A provider reset time closer than this is read as this (a "retry after 1s" is not worth a
/// re-probe storm).
pub const MIN_RESET_MS: i64 = 60 * 1000;

/// A provider reset time further away than this is read as this: past it the seat is re-probed
/// anyway, and a new refusal states the time again.
pub const MAX_RESET_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// The longest piece of a re-probe's answer kept as its detail.
const DETAIL_MAX: usize = 240;

/// Now, in unix milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// Whether `reason` is a quota-class bench reason: the engine's `quota_exhausted` token, however
/// the bench worded around it (`quota_exhausted (no success in the run)`, a ballot's
/// `quota_exhausted (3/3 ballots)`, a launcher's `recent quota_exhausted`). Rate limits, usage
/// limits and credit refusals all classify as `quota_exhausted`
/// (`wicked_council::types::SeatFailureReason::QuotaExhausted`).
pub fn quota_class(reason: &str) -> bool {
    reason.contains(wicked_council::types::SeatFailureReason::QuotaExhausted.as_str())
}

/// (core#1479) Whether `reason` is a sign-in bench reason: the engine's `not_logged_in` token,
/// however the bench worded around it (a launcher's `recent not_logged_in`), or the launcher's own
/// words for a seat it read as signed out (crew's `signed out`, `not signed in`).
pub fn sign_in_class(reason: &str) -> bool {
    reason.contains(wicked_council::types::SeatFailureReason::NotLoggedIn.as_str())
        || reason.contains("signed out")
        || reason.contains("not signed in")
}

/// (core#1479) Whether a bench of `reason` lifts by itself, after a re-probe: quota and sign-in.
pub fn lifts(reason: &str) -> bool {
    quota_class(reason) || sign_in_class(reason)
}

/// The backoff for a seat's `strikes`-th quota bench in a row: 15 min, 30 min, then 60 min.
pub fn backoff_ms(strikes: u32) -> i64 {
    let i = (strikes.max(1) as usize - 1).min(QUOTA_BACKOFF_MINUTES.len() - 1);
    QUOTA_BACKOFF_MINUTES[i] * 60 * 1000
}

/// (core#1479) The backoff for a bench of `reason` at its `strikes`-th strike: the sign-in table
/// for a sign-in bench, else the quota table ([`backoff_ms`]).
pub fn backoff_for(reason: &str, strikes: u32) -> i64 {
    if sign_in_class(reason) && !quota_class(reason) {
        let i = (strikes.max(1) as usize - 1).min(SIGN_IN_BACKOFF_MINUTES.len() - 1);
        SIGN_IN_BACKOFF_MINUTES[i] * 60 * 1000
    } else {
        backoff_ms(strikes)
    }
}

/// When a quota bench with `strikes` lifts: the provider's reset time read from `refusal`, when
/// it stated one, else `now + backoff_ms(strikes)`.
pub fn quota_until(refusal: &str, strikes: u32, now: i64) -> i64 {
    provider_reset_at(refusal, now).unwrap_or(now + backoff_ms(strikes))
}

/// Stamp a bench that lifts ([`lifts`]: quota or sign-in) and carries no lift time with the first
/// backoff of its cause (strike 1). A bench of any other cause, a governed-only bench, and one
/// already stamped are returned unchanged.
pub fn stamp(mut seat: BenchedSeat, now: i64) -> BenchedSeat {
    if !seat.governed_only && lifts(&seat.reason) && seat.benched_until.is_none() {
        seat.strikes = seat.strikes.max(1);
        seat.benched_until = Some(now + backoff_for(&seat.reason, seat.strikes));
    }
    seat
}

/// Whether the bench is due for a re-probe at `now`: a bench that lifts ([`lifts`]; never a
/// governed-only one) whose lift time has passed. One with NO lift time (persisted before its rule
/// existed: a quota bench before J4, a sign-in bench before core#1479) is due at once, so a run
/// parked on one recovers on its next unit.
pub fn due_for_reprobe(seat: &BenchedSeat, now: i64) -> bool {
    !seat.governed_only && lifts(&seat.reason) && seat.benched_until.is_none_or(|u| u <= now)
}

/// When `cli`'s bench in `bench` falls due for a re-probe, if it ever does.
pub fn until_of(bench: &[BenchedSeat], cli: &str) -> Option<i64> {
    bench
        .iter()
        .find(|b| b.cli == cli)
        .and_then(|b| b.benched_until)
}

/// `bench` without the seats due for a re-probe: what the judge's eligibility reads when it asks
/// "could this seat judge once it answered?" (the re-judge readiness).
pub fn without_due(bench: &[BenchedSeat], now: i64) -> Vec<BenchedSeat> {
    bench
        .iter()
        .filter(|b| !due_for_reprobe(b, now))
        .cloned()
        .collect()
}

/// One re-probe of a seat whose quota bench was due, as the worker thread ran it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchReprobe {
    /// The roster key (an instance key such as `claude#2` stays one: that instance was asked).
    pub seat: String,
    /// The bench's strikes when it was probed.
    pub prior_strikes: u32,
    /// The bench's lift time when it was probed. With `prior_strikes`, it names the bench the
    /// outcome is about: the fold applies it only while the seat's bench is still that one (a
    /// fresh refusal since, which starts at strike 1 again, has a later lift time and is left alone).
    #[serde(default)]
    pub prior_benched_until: Option<i64>,
    /// `true`: the seat answered, and its bench is dropped.
    pub returned: bool,
    /// The strikes after a failed probe (`prior_strikes + 1`, at least 2; 0 when the refusal names
    /// a cause that does not lift, see `reason`); `prior_strikes` when it returned.
    pub strikes: u32,
    /// After a failed probe, when the new bench lifts (unix millis); `None` when it returned or
    /// when the bench no longer lifts by itself (`reason`).
    pub benched_until: Option<i64>,
    /// Set when the probe's refusal names a cause other than the bench's own, and the bench takes
    /// it: one that does not change mid-run (`not_installed`, `approval_unavailable`: `strikes` 0,
    /// no `benched_until`, the bench stops lifting by itself), or the other cause that lifts (a
    /// quota bench refused as `not_logged_in`, or the reverse: that cause's backoff, core#1479).
    /// `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// When the probe ran (unix millis).
    pub at: i64,
    /// What the seat said, bounded (or why it could not be asked).
    pub detail: String,
}

/// The outcome of re-probing `seat` at `now`: `answer` is `Ok(words)` when the seat answered the
/// readiness call, `Err(output)` when it refused or failed (its output, read for a reset time and
/// for its cause: `not_installed` / `approval_unavailable` bench the seat for good; a sign-in or
/// quota refusal re-benches it with that cause's next backoff, core#1479).
pub fn reprobe_outcome(seat: &BenchedSeat, answer: Result<&str, &str>, now: i64) -> BenchReprobe {
    let base = BenchReprobe {
        seat: seat.cli.clone(),
        prior_strikes: seat.strikes,
        prior_benched_until: seat.benched_until,
        at: now,
        ..BenchReprobe::default()
    };
    match answer {
        Ok(words) => BenchReprobe {
            returned: true,
            strikes: seat.strikes,
            detail: bounded(words),
            ..base
        },
        Err(output) => {
            use wicked_council::types::SeatFailureReason as R;
            let exited_nonzero =
                crate::execute_wrapped::wrapped_exit_code(output).is_some_and(|c| c != 0);
            match R::classify_refusal(output, exited_nonzero) {
                Some(cause @ (R::NotInstalled | R::ApprovalUnavailable)) => BenchReprobe {
                    strikes: 0,
                    reason: Some(cause.as_str().to_string()),
                    detail: bounded(output),
                    ..base
                },
                // (core#1479) A sign-in or quota refusal lifts again later. The bench takes the
                // cause the probe named when it is the other one (a quota bench now signed out).
                Some(R::NotLoggedIn) if !sign_in_class(&seat.reason) => {
                    failed_probe(base, R::NotLoggedIn.as_str(), true, output, now)
                }
                Some(R::QuotaExhausted) if !quota_class(&seat.reason) => {
                    failed_probe(base, R::QuotaExhausted.as_str(), true, output, now)
                }
                _ => failed_probe(base, &seat.reason, false, output, now),
            }
        }
    }
}

/// A failed probe: one more strike, and the next backoff of `cause` (a quota cause honours the reset
/// time `output` states). `retag`: the bench takes `cause` as its reason.
fn failed_probe(
    base: BenchReprobe,
    cause: &str,
    retag: bool,
    output: &str,
    now: i64,
) -> BenchReprobe {
    let strikes = base.prior_strikes.max(1) + 1;
    let until = if quota_class(cause) {
        quota_until(output, strikes, now)
    } else {
        now + backoff_for(cause, strikes)
    };
    BenchReprobe {
        returned: false,
        strikes,
        benched_until: Some(until),
        reason: retag.then(|| cause.to_string()),
        detail: bounded(output),
        ..base
    }
}

/// A seat that answered its re-probe and then refused its real call in the same attempt (the judge
/// call that the probe let it take came back `quota_exhausted`) did not come back: its outcome is
/// read as a failed probe, with the next backoff (or the reset time the refusal stated). Without
/// this, the lift and the judge's fresh strike-1 bench would reset the backoff on every expiry.
/// `refusals` are the attempt's judge refusals: `(seat, cause token, stated reset time)`.
pub fn reconcile_with_refusals(
    reprobes: &[BenchReprobe],
    refusals: &[(String, String, Option<i64>)],
) -> Vec<BenchReprobe> {
    let quota_token = wicked_council::types::SeatFailureReason::QuotaExhausted.as_str();
    reprobes
        .iter()
        .map(|r| {
            let refused = refusals
                .iter()
                .find(|(seat, token, _)| seat == &r.seat && token == quota_token);
            match refused {
                Some((_, _, reset_at)) if r.returned => {
                    let mut failed = failed_probe(r.clone(), quota_token, false, "", r.at);
                    if let Some(at) = reset_at.filter(|at| *at > r.at) {
                        failed.benched_until = Some(at.min(r.at + MAX_RESET_MS));
                    }
                    failed.detail = format!(
                        "answered its re-probe, then refused its judge call for quota ({})",
                        r.detail
                    );
                    failed
                }
                _ => r.clone(),
            }
        })
        .collect()
}

fn bounded(s: &str) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= DETAIL_MAX {
        flat
    } else {
        let mut t: String = flat.chars().take(DETAIL_MAX - 1).collect();
        t.push('…');
        t
    }
}

/// One re-probe outcome the fold applied to the run's bench.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// The seat answered and its quota bench was dropped; `reason` is the bench it had. A seat
    /// whose quota bench had replaced a bench for governed work only (the host-floor bench) keeps
    /// that one: it is back for the calls that bench allows (judging, triage, ballots).
    Returned { cli: String, reason: String },
    /// The seat did not answer and stays benched: until `until` with `strikes`, or (`until:
    /// None`) for good under the cause its refusal named (`reason`).
    Rebenched {
        cli: String,
        reason: String,
        until: Option<i64>,
        strikes: u32,
    },
}

/// Fold re-probe outcomes into a run's bench. Each applies only to the bench it probed: the same
/// seat, still a bench that lifts ([`lifts`]), with the same strikes and lift time (a newer bench
/// is left alone).
pub fn apply_reprobes(bench: &mut Vec<BenchedSeat>, reprobes: &[BenchReprobe]) -> Vec<Applied> {
    let mut applied = Vec::new();
    for r in reprobes {
        let Some(ix) = bench.iter().position(|b| {
            b.cli == r.seat
                && !b.governed_only
                && lifts(&b.reason)
                && b.strikes == r.prior_strikes
                && b.benched_until == r.prior_benched_until
        }) else {
            continue;
        };
        if r.returned {
            let gone = match bench[ix].replaced.take() {
                Some(under) => std::mem::replace(&mut bench[ix], *under),
                None => bench.remove(ix),
            };
            applied.push(Applied::Returned {
                cli: gone.cli,
                reason: gone.reason,
            });
            continue;
        }
        let b = &mut bench[ix];
        if let Some(reason) = &r.reason {
            b.reason = reason.clone();
            b.strikes = r.strikes;
            b.benched_until = r.benched_until;
        } else if let Some(until) = r.benched_until {
            b.strikes = r.strikes;
            b.benched_until = Some(until);
        } else {
            continue;
        }
        applied.push(Applied::Rebenched {
            cli: b.cli.clone(),
            reason: b.reason.clone(),
            until: b.benched_until,
            strikes: b.strikes,
        });
    }
    applied
}

/// A seat that came back from a launcher quota or sign-in bench: mark it usable in the run's
/// persisted launch roster (`team_plan.roster`, the `AgenticCli` records the run was launched with),
/// so a plan revision that re-reads that roster does not bench it again from the launcher's stale
/// verdict. Only a launcher reading that lifts ([`lifts`]) is cleared. Returns whether the roster
/// changed.
pub fn clear_launcher_health(roster: &mut [serde_json::Value], cli: &str) -> bool {
    let mut changed = false;
    for seat in roster.iter_mut() {
        if seat.get("key").and_then(serde_json::Value::as_str) != Some(cli) {
            continue;
        }
        let Some(health) = seat
            .get_mut("health")
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        let unusable = health.get("usable").and_then(serde_json::Value::as_bool) == Some(false);
        let lifting = health
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .is_some_and(lifts);
        if unusable && lifting {
            health.insert("usable".into(), serde_json::Value::Bool(true));
            health.remove("reason");
            changed = true;
        }
    }
    changed
}

/// The provider's reset time stated in a refusal, as unix millis, bounded to
/// [`MIN_RESET_MS`]..=[`MAX_RESET_MS`] from `now`; `None` when the text states none (or one in
/// the past). Read, case-insensitively:
/// - an HTTP `Retry-After: <seconds>` header;
/// - a relative time after "try again", "retry", "reset(s)", "available again" or "wait":
///   `in 20 minutes`, `after 90s`, `in 1h 30m`;
/// - an absolute time after "reset(s)", "try again" or "available again": `at
///   2026-10-25T13:00:00Z`, `on 2026-11-01` (an RFC 3339 instant, or a UTC date).
pub fn provider_reset_at(text: &str, now: i64) -> Option<i64> {
    let lower = text.to_ascii_lowercase();
    let raw = retry_after(&lower, now)
        .or_else(|| relative(&lower).map(|ms| now + ms))
        .or_else(|| absolute(&lower))?;
    (raw > now).then(|| raw.clamp(now + MIN_RESET_MS, now + MAX_RESET_MS))
}

fn retry_after(lower: &str, now: i64) -> Option<i64> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"retry-after:\s*(\d{1,9})\b").expect("valid"));
    let secs: i64 = re.captures(lower)?.get(1)?.as_str().parse().ok()?;
    (secs > 0).then_some(now + secs * 1000)
}

fn relative(lower: &str) -> Option<i64> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(?:try again|retry|resets?|available again|wait)\s+(?:in|after)\s+((?:\d{1,6}(?:\.\d+)?\s*(?:days?|d|hours?|hrs?|h|minutes?|mins?|m|seconds?|secs?|s)\b[\s,]*(?:and\s+)?)+)",
        )
        .expect("valid")
    });
    let span = re.captures(lower)?.get(1)?.as_str();
    static PART: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let part = PART.get_or_init(|| {
        regex::Regex::new(
            r"(\d{1,6}(?:\.\d+)?)\s*(days?|d|hours?|hrs?|h|minutes?|mins?|m|seconds?|secs?|s)\b",
        )
        .expect("valid")
    });
    let mut ms = 0f64;
    for c in part.captures_iter(span) {
        let n: f64 = c.get(1)?.as_str().parse().ok()?;
        let unit = match c.get(2)?.as_str().chars().next()? {
            'd' => 86_400_000.0,
            'h' => 3_600_000.0,
            'm' => 60_000.0,
            _ => 1_000.0,
        };
        ms += n * unit;
    }
    (ms > 0.0).then_some(ms as i64)
}

fn absolute(lower: &str) -> Option<i64> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(?:resets?|try again|available again)\s+(?:at|on|after)\s+(\d{4})-(\d{2})-(\d{2})(?:[t ](\d{2}):(\d{2})(?::(\d{2})(?:\.\d+)?)?\s*(z|utc|[+-]\d{2}:?\d{2})?)?",
        )
        .expect("valid")
    });
    let c = re.captures(lower)?;
    let num = |i: usize| c.get(i).and_then(|m| m.as_str().parse::<i64>().ok());
    let (y, mo, d) = (num(1)?, num(2)?, num(3)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let (h, mi, s) = (
        num(4).unwrap_or(0),
        num(5).unwrap_or(0),
        num(6).unwrap_or(0),
    );
    if h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let offset_min = match c.get(7).map(|m| m.as_str()) {
        None | Some("z") | Some("utc") => 0,
        Some(o) => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            let digits: String = o.chars().filter(char::is_ascii_digit).collect();
            let hh: i64 = digits.get(0..2)?.parse().ok()?;
            let mm: i64 = digits.get(2..4)?.parse().ok()?;
            sign * (hh * 60 + mm)
        }
    };
    let days = days_from_civil(y, mo, d);
    Some(((days * 86_400 + h * 3_600 + mi * 60 + s) - offset_min * 60) * 1000)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// ISO-8601 (UTC, seconds) of unix millis — for the operator's sentence.
pub fn iso(ms: i64) -> String {
    let s = ms.div_euclid(1000);
    let (days, rem) = (s.div_euclid(86_400), s.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60 * 1000;
    const NOW: i64 = 1_761_400_000_000; // 2025-10-25T13:46:40Z

    fn bench(cli: &str, reason: &str) -> BenchedSeat {
        BenchedSeat {
            cli: cli.into(),
            reason: reason.into(),
            source: "worker".into(),
            governed_only: false,
            benched_until: None,
            strikes: 0,
            replaced: None,
        }
    }

    #[test]
    fn the_backoff_is_15_then_30_then_60_minutes_and_stays_bounded() {
        assert_eq!(backoff_ms(0), 15 * MIN, "no strike reads as the first");
        assert_eq!(backoff_ms(1), 15 * MIN);
        assert_eq!(backoff_ms(2), 30 * MIN);
        assert_eq!(backoff_ms(3), 60 * MIN);
        assert_eq!(backoff_ms(40), 60 * MIN);
    }

    #[test]
    fn a_quota_or_sign_in_bench_expires_and_no_other() {
        for r in [
            "quota_exhausted",
            "quota_exhausted (no success in the run)",
            "quota_exhausted (3/3 ballots)",
            "recent quota_exhausted",
        ] {
            assert!(quota_class(r) && lifts(r), "{r}");
        }
        // (core#1479) A sign-in bench lifts too: a person signs the seat in mid-run.
        for r in [
            "not_logged_in",
            "recent not_logged_in",
            "signed out",
            "not signed in (launcher health probe)",
        ] {
            assert!(sign_in_class(r) && lifts(r), "{r}");
            let b = stamp(bench("x", r), NOW);
            assert_eq!(
                (b.strikes, b.benched_until),
                (1, Some(NOW + 5 * MIN)),
                "{r}"
            );
            assert!(!due_for_reprobe(&b, NOW + 5 * MIN - 1));
            assert!(due_for_reprobe(&b, NOW + 5 * MIN), "{r}");
        }
        for r in ["not_installed", "approval_unavailable", "timed_out"] {
            assert!(!lifts(r), "{r}");
            let b = stamp(bench("x", r), NOW);
            assert_eq!(b.benched_until, None, "{r} never lifts by itself");
            assert!(!due_for_reprobe(&b, NOW + 365 * 24 * 60 * MIN));
        }
        let mut host = bench("x", "quota_exhausted");
        host.governed_only = true;
        assert!(!due_for_reprobe(&stamp(host, NOW), NOW + 600 * MIN));
    }

    /// (core#1479) The sign-in backoff: 5, 10, then 15 minutes, bounded.
    #[test]
    fn the_sign_in_backoff_is_5_then_10_then_15_minutes() {
        for (strikes, min) in [(0, 5), (1, 5), (2, 10), (3, 15), (40, 15)] {
            assert_eq!(
                backoff_for("not_logged_in", strikes),
                min * MIN,
                "{strikes}"
            );
            assert_eq!(backoff_for("signed out", strikes), min * MIN, "{strikes}");
        }
        assert_eq!(backoff_for("quota_exhausted", 2), 30 * MIN);
    }

    #[test]
    fn a_quota_bench_is_stamped_with_the_first_backoff_and_falls_due_when_it_passes() {
        let b = stamp(
            bench("copilot", "quota_exhausted (no success in the run)"),
            NOW,
        );
        assert_eq!((b.strikes, b.benched_until), (1, Some(NOW + 15 * MIN)));
        assert!(!due_for_reprobe(&b, NOW + 15 * MIN - 1));
        assert!(due_for_reprobe(&b, NOW + 15 * MIN));
        // A stamp never moves an existing lift time (the provider's reset time wins).
        let mut set = bench("copilot", "quota_exhausted");
        set.benched_until = Some(NOW + 300 * MIN);
        set.strikes = 1;
        assert_eq!(stamp(set.clone(), NOW), set);
    }

    #[test]
    fn a_quota_bench_persisted_before_the_rule_is_due_at_once() {
        // The 10-24 copilot bench on the rig: no lift time on record.
        let legacy: BenchedSeat = serde_json::from_str(
            r#"{"cli":"copilot","reason":"quota_exhausted (no success in the run)","source":"worker"}"#,
        )
        .unwrap();
        assert_eq!((legacy.benched_until, legacy.strikes), (None, 0));
        assert!(due_for_reprobe(&legacy, NOW));
        assert!(without_due(&[legacy], NOW).is_empty());
    }

    #[test]
    fn the_new_fields_stay_off_the_wire_when_unset() {
        let v = serde_json::to_value(bench("pi", "not_installed")).unwrap();
        assert!(
            v.get("benched_until").is_none() && v.get("strikes").is_none(),
            "{v}"
        );
        let v = serde_json::to_value(stamp(bench("pi", "quota_exhausted"), NOW)).unwrap();
        assert_eq!(v["benched_until"], NOW + 15 * MIN);
        assert_eq!(v["strikes"], 1);
    }

    #[test]
    fn a_bench_expires_and_the_seat_returns() {
        let mut run = vec![
            stamp(
                bench("copilot", "quota_exhausted (no success in the run)"),
                NOW,
            ),
            bench("pi", "not_installed"),
        ];
        let later = NOW + 16 * MIN;
        let due: Vec<&BenchedSeat> = run.iter().filter(|b| due_for_reprobe(b, later)).collect();
        assert_eq!(due.len(), 1);
        let r = reprobe_outcome(due[0], Ok("READY"), later);
        assert!(r.returned && r.benched_until.is_none());
        let applied = apply_reprobes(&mut run, &[r]);
        assert_eq!(
            applied,
            vec![Applied::Returned {
                cli: "copilot".into(),
                reason: "quota_exhausted (no success in the run)".into()
            }]
        );
        assert_eq!(run.len(), 1, "only the bench that lifts lifted: {run:?}");
        assert_eq!(run[0].cli, "pi");
    }

    #[test]
    fn a_failed_reprobe_rebenches_with_a_longer_backoff() {
        let mut run = vec![stamp(bench("copilot", "quota_exhausted"), NOW)];
        let t1 = NOW + 15 * MIN;
        let r1 = reprobe_outcome(
            &run[0],
            Err("Error: You have exceeded your monthly quota"),
            t1,
        );
        assert_eq!(
            (r1.returned, r1.strikes, r1.benched_until),
            (false, 2, Some(t1 + 30 * MIN))
        );
        apply_reprobes(&mut run, &[r1]);
        assert_eq!(
            (run[0].strikes, run[0].benched_until),
            (2, Some(t1 + 30 * MIN))
        );
        assert!(!due_for_reprobe(&run[0], t1 + 29 * MIN));
        let t2 = t1 + 30 * MIN;
        let r2 = reprobe_outcome(&run[0], Err("429 Too Many Requests"), t2);
        apply_reprobes(&mut run, &[r2]);
        assert_eq!(
            (run[0].strikes, run[0].benched_until),
            (3, Some(t2 + 60 * MIN))
        );
        let t3 = t2 + 60 * MIN;
        let r3 = reprobe_outcome(&run[0], Err("429 Too Many Requests"), t3);
        apply_reprobes(&mut run, &[r3]);
        assert_eq!(
            (run[0].strikes, run[0].benched_until),
            (4, Some(t3 + 60 * MIN)),
            "the backoff stays bounded at 60 minutes"
        );
        // A legacy bench (no strikes) that fails its first probe takes the second backoff.
        let legacy = bench("copilot", "quota_exhausted");
        let r = reprobe_outcome(&legacy, Err("quota"), NOW);
        assert_eq!((r.strikes, r.benched_until), (2, Some(NOW + 30 * MIN)));
    }

    #[test]
    fn a_failed_reprobe_honours_the_reset_time_the_refusal_states() {
        let b = stamp(bench("codex", "quota_exhausted"), NOW);
        let r = reprobe_outcome(
            &b,
            Err("429 rate limited. Please try again in 2 hours."),
            NOW,
        );
        assert_eq!(r.benched_until, Some(NOW + 120 * MIN));
    }

    #[test]
    fn a_stale_outcome_leaves_a_newer_bench_alone() {
        let mut run = vec![stamp(bench("copilot", "quota_exhausted"), NOW)];
        let r = reprobe_outcome(&run[0], Ok("READY"), NOW + 15 * MIN);
        // Another unit's probe failed it first (strike 2) before this outcome was folded.
        run[0].strikes = 2;
        assert!(apply_reprobes(&mut run, std::slice::from_ref(&r)).is_empty());
        assert_eq!(run.len(), 1);
        // A parallel unit's probe returned it, it then refused again: a FRESH strike-1 bench with
        // a later lift time. The stale "returned" outcome must not drop it (it matched on strikes
        // alone before).
        let mut fresh = vec![stamp(bench("copilot", "quota_exhausted"), NOW + 16 * MIN)];
        assert_eq!(fresh[0].strikes, r.prior_strikes);
        assert!(apply_reprobes(&mut fresh, &[r]).is_empty());
        assert_eq!(fresh.len(), 1);
    }

    #[test]
    fn a_probe_refused_for_a_cause_that_does_not_lift_benches_for_good() {
        let mut run = vec![stamp(bench("codex", "quota_exhausted"), NOW)];
        let r = reprobe_outcome(
            &run[0],
            Err("ERROR codex_core::tools::router: error=exec_command failed: CreateProcess { message: \"Rejected(\\\"approval request failed\\\")\" }"),
            NOW + 15 * MIN,
        );
        assert_eq!(
            (r.returned, r.reason.as_deref(), r.benched_until, r.strikes),
            (false, Some("approval_unavailable"), None, 0)
        );
        let applied = apply_reprobes(&mut run, &[r]);
        assert_eq!(
            applied,
            vec![Applied::Rebenched {
                cli: "codex".into(),
                reason: "approval_unavailable".into(),
                until: None,
                strikes: 0
            }]
        );
        assert!(
            !due_for_reprobe(&run[0], NOW + 365 * 24 * 60 * MIN),
            "never lifts by itself"
        );
    }

    /// (core#1479) A quota bench whose probe answers "Not logged in" becomes a sign-in bench with
    /// the sign-in backoff; it is re-probed again, never benched for good.
    #[test]
    fn a_probe_refused_as_signed_out_rebenches_on_the_sign_in_backoff() {
        let mut run = vec![stamp(bench("claude", "quota_exhausted"), NOW)];
        let t1 = NOW + 15 * MIN;
        let r = reprobe_outcome(&run[0], Err("Not logged in · Please run /login"), t1);
        assert_eq!(
            (r.returned, r.reason.as_deref(), r.benched_until, r.strikes),
            (false, Some("not_logged_in"), Some(t1 + 10 * MIN), 2)
        );
        apply_reprobes(&mut run, &[r]);
        assert_eq!(run[0].reason, "not_logged_in");
        assert!(due_for_reprobe(&run[0], t1 + 10 * MIN));
    }

    /// (core#1479) codex signed out at launch, then signed in: its launcher bench falls due five
    /// minutes in, the probe answers, and the seat is back on the run's roster (and usable in the
    /// launch roster a plan revision re-reads). Before, a sign-in bench never lifted.
    #[test]
    fn a_seat_signed_in_after_the_launch_returns_to_the_run() {
        let mut codex = wicked_council::registry::builtin()
            .into_iter()
            .next()
            .expect("a built-in seat");
        codex.key = "codex".into();
        codex.health = Some(wicked_council::types::SeatHealth {
            usable: false,
            reason: Some("signed out".into()),
        });
        let mut run = crate::distribute::launcher_benched(std::slice::from_ref(&codex));
        let due_at = run[0].benched_until.expect("a sign-in bench is stamped");
        assert!(!due_for_reprobe(&run[0], due_at - 1));
        // Still signed out at the first probe: the floor's own refusal, benched again for longer.
        let still = reprobe_outcome(
            &run[0],
            Err("(codex is signed out: sign it in (Agents → Sign in), then retry — its seat 'codex' could not run this unit: agent_signed_out: this agent is not signed in for container seats)"),
            due_at,
        );
        assert_eq!(
            (still.returned, still.strikes, still.reason.as_deref()),
            (false, 2, None)
        );
        assert_eq!(still.benched_until, Some(due_at + 10 * MIN));
        apply_reprobes(&mut run, &[still]);
        assert_eq!(run[0].reason, "signed out");
        // Signed in since: the next probe answers and the bench is gone.
        let next = run[0].benched_until.unwrap();
        let back = reprobe_outcome(&run[0], Ok("READY"), next);
        assert_eq!(
            apply_reprobes(&mut run, &[back]),
            vec![Applied::Returned {
                cli: "codex".into(),
                reason: "signed out".into()
            }]
        );
        assert!(run.is_empty());
        let mut roster = vec![serde_json::to_value(&codex).unwrap()];
        assert!(clear_launcher_health(&mut roster, "codex"));
        assert_eq!(roster[0]["health"]["usable"], true);
    }

    #[test]
    fn a_quota_bench_over_a_host_floor_bench_lifts_back_to_it() {
        let host = BenchedSeat {
            reason: crate::distribute::HOST_FLOOR_BENCH.into(),
            source: "launcher".into(),
            governed_only: true,
            ..bench("opencode", "")
        };
        let mut run = vec![host.clone()];
        // It judges (a governed-only bench allows that), and refuses for quota while judging.
        assert!(crate::domain::bench_seat(
            &mut run,
            BenchedSeat {
                source: "judge".into(),
                ..bench("opencode", "quota_exhausted (no success in the run)")
            }
        ));
        assert_eq!(run.len(), 1);
        assert!(!run[0].governed_only && run[0].replaced.as_deref() == Some(&host));
        let due_at = run[0].benched_until.expect("stamped");
        let r = reprobe_outcome(&run[0], Ok("READY"), due_at);
        let applied = apply_reprobes(&mut run, &[r]);
        assert!(matches!(&applied[..], [Applied::Returned { .. }]));
        assert_eq!(
            run,
            vec![host],
            "back under the host-floor bench, not off the bench"
        );
    }

    #[test]
    fn answering_then_refusing_the_judge_call_is_a_failed_probe() {
        let b = stamp(bench("copilot", "quota_exhausted"), NOW);
        let r = reprobe_outcome(&b, Ok("READY"), NOW + 15 * MIN);
        let refusals = vec![
            ("copilot".to_string(), "quota_exhausted".to_string(), None),
            ("pi".to_string(), "quota_exhausted".to_string(), None),
        ];
        let out = reconcile_with_refusals(std::slice::from_ref(&r), &refusals);
        assert!(!out[0].returned && out[0].strikes == 2);
        assert_eq!(
            out[0].benched_until,
            Some(NOW + 15 * MIN + 30 * MIN),
            "the next backoff"
        );
        // A stated reset time wins.
        let stated = vec![(
            "copilot".to_string(),
            "quota_exhausted".to_string(),
            Some(NOW + 90 * MIN),
        )];
        assert_eq!(
            reconcile_with_refusals(std::slice::from_ref(&r), &stated)[0].benched_until,
            Some(NOW + 90 * MIN)
        );
        // Another cause, or no refusal: the outcome stands.
        let other = vec![("copilot".to_string(), "not_logged_in".to_string(), None)];
        assert!(reconcile_with_refusals(std::slice::from_ref(&r), &other)[0].returned);
        assert!(reconcile_with_refusals(std::slice::from_ref(&r), &[])[0].returned);
        let mut run = vec![b];
        apply_reprobes(&mut run, &out);
        assert_eq!(
            run[0].strikes, 2,
            "the backoff keeps growing, it is not reset to strike 1"
        );
    }

    #[test]
    fn a_returned_launcher_seat_reads_usable_in_the_launch_roster() {
        let mut roster = vec![
            serde_json::json!({"key": "copilot", "health": {"usable": false, "reason": "recent quota_exhausted"}}),
            serde_json::json!({"key": "pi", "health": {"usable": false, "reason": "not_installed"}}),
            serde_json::json!({"key": "claude"}),
        ];
        assert!(clear_launcher_health(&mut roster, "copilot"));
        assert_eq!(roster[0]["health"], serde_json::json!({"usable": true}));
        assert!(
            !clear_launcher_health(&mut roster, "pi"),
            "a reading that does not lift stays"
        );
        assert!(!clear_launcher_health(&mut roster, "claude"));
        // The launcher reads it usable now: a plan revision benches nothing for it.
        let clis: Vec<crate::AgenticCli> = roster
            .iter()
            .filter_map(|v| {
                let mut c = wicked_council::registry::builtin().into_iter().next()?;
                c.key = v["key"].as_str()?.to_string();
                c.health = serde_json::from_value(v.get("health")?.clone()).ok();
                Some(c)
            })
            .collect();
        let benched: Vec<String> = crate::distribute::launcher_benched(&clis)
            .into_iter()
            .map(|b| b.cli)
            .collect();
        assert_eq!(benched, vec!["pi".to_string()]);
    }

    #[test]
    fn the_provider_reset_time_is_read_in_its_common_spellings() {
        let at = |t: &str| provider_reset_at(t, NOW).map(|ms| ms - NOW);
        assert_eq!(
            at("Rate limit reached. Please try again in 20 minutes."),
            Some(20 * MIN)
        );
        assert_eq!(at("usage limit hit; try again in 1h 30m"), Some(90 * MIN));
        assert_eq!(at("quota exceeded, retry after 90s"), Some(90 * 1000));
        assert_eq!(
            at("Your limit resets in 3 hours and 15 minutes"),
            Some(195 * MIN)
        );
        assert_eq!(
            at("Too many requests, wait 5 seconds"),
            None,
            "no `in`/`after`: not read"
        );
        assert_eq!(
            at("You've hit your limit · resets at 2025-10-25T15:00:00Z"),
            Some((15 * 60 - (13 * 60 + 46)) * MIN - 40 * 1000)
        );
        assert_eq!(
            at("resets at 2025-10-25T17:00:00+02:00"),
            Some((15 * 60 - (13 * 60 + 46)) * MIN - 40 * 1000),
            "an offset is honoured"
        );
        assert_eq!(
            at("monthly quota exhausted; it resets on 2025-11-15"),
            Some(MAX_RESET_MS),
            "a reset beyond a week is read as a week"
        );
        assert_eq!(
            at("try again in 1 second"),
            Some(MIN),
            "a reset under a minute reads as a minute"
        );
        assert_eq!(
            at("resets at 2020-01-01T00:00:00Z"),
            None,
            "a reset in the past is ignored"
        );
        assert_eq!(at("Error: You have exceeded your monthly quota"), None);
        assert_eq!(at("HTTP/1.1 429\r\nRetry-After: 600\r\n"), Some(10 * MIN));
    }

    #[test]
    fn a_launcher_quota_bench_takes_the_first_backoff_not_an_immediate_probe() {
        // crew hands a seat it keeps out on its own clock as `health.usable: false` with
        // `recent quota_exhausted`: the run re-probes it 15 minutes in, not on its first unit.
        let mut copilot = wicked_council::registry::builtin()
            .into_iter()
            .next()
            .expect("a built-in seat");
        copilot.key = "copilot".into();
        copilot.health = Some(wicked_council::types::SeatHealth {
            usable: false,
            reason: Some("recent quota_exhausted".into()),
        });
        let mut pi = copilot.clone();
        pi.key = "pi".into();
        pi.health = Some(wicked_council::types::SeatHealth {
            usable: false,
            reason: Some("signed out".into()),
        });
        let before = now_ms();
        let benched = crate::distribute::launcher_benched(&[copilot, pi]);
        let quota = &benched[0];
        assert_eq!((quota.source.as_str(), quota.strikes), ("launcher", 1));
        let until = quota.benched_until.expect("stamped");
        assert!(until >= before + 15 * MIN && until <= now_ms() + 15 * MIN);
        assert!(!due_for_reprobe(quota, before));
        let sign_in = benched[1]
            .benched_until
            .expect("core#1479: a sign-in bench lifts");
        assert!(sign_in >= before + 5 * MIN && sign_in <= now_ms() + 5 * MIN);
    }

    #[test]
    fn iso_renders_utc_seconds() {
        assert_eq!(iso(NOW), "2025-10-25T13:46:40Z");
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
    }
}
