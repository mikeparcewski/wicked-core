//! Who sits on a council (operator ruling 2026-10-11): a SIZE from the system setting, filled AT
//! RANDOM from the eligible seats, one seat per model family first, reproducible from a recorded
//! seed.
//!
//! The caller decides eligibility (the run's roster minus its benches and minus the dispute's
//! parties); this module only draws from what it is handed, so it can never seat a party or a
//! benched seat. The draw is a pure function of `(eligible, size, seed)`: the seed rides the
//! council's ruling onto the gate's receipt, and re-running [`pick_seats`] with it names the same
//! seats.

use std::time::Duration;

use crate::AgenticCli;

/// Env var naming how many seats a council convenes (crew applies `SystemSettings.councilSize`).
pub const ENV_COUNCIL_SIZE: &str = "WICKED_COUNCIL_SIZE";
/// Env var naming the pause between two seat spawns of one ballot, in milliseconds (crew applies
/// `SystemSettings.councilSpawnStaggerMs`).
pub const ENV_SPAWN_STAGGER_MS: &str = "WICKED_COUNCIL_SPAWN_STAGGER_MS";

/// The shipped council size.
pub const DEFAULT_COUNCIL_SIZE: usize = 3;
/// The largest council the setting admits.
pub const MAX_COUNCIL_SIZE: usize = 9;
/// The shipped spawn stagger.
pub const DEFAULT_SPAWN_STAGGER: Duration = Duration::from_millis(3000);
/// The longest stagger the setting admits (a ballot's budget is 40 s; a larger gap would spend it
/// waiting).
pub const MAX_SPAWN_STAGGER: Duration = Duration::from_millis(30_000);

/// The council size a raw setting names: an integer in `1..=MAX_COUNCIL_SIZE`, else the default.
pub fn council_size_from(raw: Option<&str>) -> usize {
    raw.and_then(|r| r.trim().parse::<usize>().ok())
        .filter(|n| (1..=MAX_COUNCIL_SIZE).contains(n))
        .unwrap_or(DEFAULT_COUNCIL_SIZE)
}

/// The spawn stagger a raw setting names: whole milliseconds in `0..=MAX_SPAWN_STAGGER`, else the
/// default. `0` is a valid choice (spawn every seat at once).
pub fn spawn_stagger_from(raw: Option<&str>) -> Duration {
    raw.and_then(|r| r.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .filter(|d| *d <= MAX_SPAWN_STAGGER)
        .unwrap_or(DEFAULT_SPAWN_STAGGER)
}

/// The configured council size, read at call time (a settings change is live on the next council).
pub fn council_size() -> usize {
    council_size_from(std::env::var(ENV_COUNCIL_SIZE).ok().as_deref())
}

/// The configured spawn stagger, read at call time.
pub fn spawn_stagger() -> Duration {
    spawn_stagger_from(std::env::var(ENV_SPAWN_STAGGER_MS).ok().as_deref())
}

/// A seat's model family: its CLI key (instance suffix dropped, `claude#2` is a `claude`) up to
/// the first `-` (`claude-eval` is a `claude`).
pub fn family(key: &str) -> &str {
    let cli = wicked_apps_core::spawn::seat_cli_key(key);
    cli.split('-').next().unwrap_or(cli)
}

/// A fresh seed for one council: wall-clock nanos mixed with a per-process counter, so two
/// councils convened in the same instant still draw differently. Recorded, never re-derived.
pub fn fresh_seed() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut s = nanos ^ n.rotate_left(32) ^ u64::from(std::process::id());
    // 53 bits: the seed rides JSON to JavaScript readers, which hold integers exactly only up
    // to 2^53 — a wider seed would round on the wire and no longer reproduce the draw.
    splitmix64(&mut s) & ((1u64 << 53) - 1)
}

/// SplitMix64: a small, well-mixed generator — enough for an unbiased seat draw, with no crate.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Draw `size` seats from `eligible` with `seed`: a seeded Fisher–Yates shuffle, then one seat
/// per family in shuffled order, then the rest in shuffled order. The drawn seats are returned in
/// `eligible` order. Fewer eligible seats than `size` seats them all. Duplicate keys sit once.
pub fn pick_seats(eligible: &[AgenticCli], size: usize, seed: u64) -> Vec<AgenticCli> {
    let mut pool: Vec<&AgenticCli> = Vec::with_capacity(eligible.len());
    for c in eligible {
        if !pool.iter().any(|p| p.key == c.key) {
            pool.push(c);
        }
    }
    let mut state = seed;
    for i in (1..pool.len()).rev() {
        let j = (splitmix64(&mut state) % (i as u64 + 1)) as usize;
        pool.swap(i, j);
    }
    let mut picked: Vec<&AgenticCli> = Vec::with_capacity(size.min(pool.len()));
    for c in &pool {
        if picked.len() == size {
            break;
        }
        if !picked.iter().any(|p| family(&p.key) == family(&c.key)) {
            picked.push(c);
        }
    }
    for c in &pool {
        if picked.len() == size {
            break;
        }
        if !picked.iter().any(|p| p.key == c.key) {
            picked.push(c);
        }
    }
    // Seat them in ROSTER order: the draw decides who sits, never the order ballots go out in.
    let mut out: Vec<AgenticCli> = Vec::with_capacity(picked.len());
    for c in eligible {
        if picked.iter().any(|p| p.key == c.key) && !out.iter().any(|o| o.key == c.key) {
            out.push(c.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seats(keys: &[&str]) -> Vec<AgenticCli> {
        keys.iter()
            .map(|k| {
                let mut c = crate::registry::builtin()
                    .into_iter()
                    .next()
                    .expect("a builtin seat");
                c.key = (*k).to_string();
                c
            })
            .collect()
    }

    fn keys(v: &[AgenticCli]) -> Vec<String> {
        v.iter().map(|c| c.key.clone()).collect()
    }

    #[test]
    fn the_council_size_honours_the_setting() {
        let pool = seats(&["claude", "codex", "pi", "opencode", "copilot", "agy"]);
        for size in 1..=6 {
            assert_eq!(pick_seats(&pool, size, 7).len(), size, "size {size}");
        }
        assert_eq!(
            pick_seats(&pool, 9, 7).len(),
            6,
            "never more than the eligible pool"
        );
    }

    #[test]
    fn the_draw_is_reproducible_from_the_recorded_seed() {
        let pool = seats(&["claude", "codex", "pi", "opencode", "copilot", "agy"]);
        for seed in [0u64, 1, 42, u64::MAX, fresh_seed()] {
            assert_eq!(
                keys(&pick_seats(&pool, 3, seed)),
                keys(&pick_seats(&pool, 3, seed)),
                "seed {seed}"
            );
        }
        // And it IS random: across seeds more than one council is drawn.
        let drawn: std::collections::BTreeSet<Vec<String>> =
            (0..64u64).map(|s| keys(&pick_seats(&pool, 3, s))).collect();
        assert!(drawn.len() > 1, "{drawn:?}");
    }

    #[test]
    fn distinct_families_come_first_when_the_pool_allows() {
        let pool = seats(&["claude", "claude-eval", "claude#2", "codex", "pi"]);
        for seed in 0..64u64 {
            let p = keys(&pick_seats(&pool, 3, seed));
            let fams: std::collections::BTreeSet<&str> = p.iter().map(|k| family(k)).collect();
            assert_eq!(fams.len(), 3, "seed {seed}: {p:?}");
        }
        // A pool with only two families fills the third chair from a repeated family.
        let two = seats(&["claude", "claude-eval", "codex"]);
        assert_eq!(pick_seats(&two, 3, 5).len(), 3);
    }

    #[test]
    fn only_the_eligible_seats_are_ever_drawn() {
        // The caller removed the creator and the benched seat; the draw cannot bring them back.
        let pool = seats(&["codex", "pi", "opencode"]);
        for seed in 0..64u64 {
            let p = keys(&pick_seats(&pool, 3, seed));
            assert!(!p.contains(&"claude".to_string()), "{p:?}");
        }
        assert!(pick_seats(&[], 3, 1).is_empty());
    }

    #[test]
    fn settings_parse_to_bounded_values() {
        assert_eq!(council_size_from(None), 3);
        assert_eq!(council_size_from(Some("5")), 5);
        assert_eq!(council_size_from(Some("0")), 3);
        assert_eq!(council_size_from(Some("10")), 3);
        assert_eq!(council_size_from(Some("x")), 3);
        assert_eq!(spawn_stagger_from(None), Duration::from_millis(3000));
        assert_eq!(spawn_stagger_from(Some("0")), Duration::ZERO);
        assert_eq!(
            spawn_stagger_from(Some("4500")),
            Duration::from_millis(4500)
        );
        assert_eq!(
            spawn_stagger_from(Some("30001")),
            Duration::from_millis(3000)
        );
        assert_eq!(family("claude-eval"), "claude");
        assert_eq!(family("claude#2"), "claude");
        assert_eq!(family("codex"), "codex");
    }
}
