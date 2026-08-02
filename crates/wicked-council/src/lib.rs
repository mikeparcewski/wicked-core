//! wicked-council — a registry-driven multi-CLI council on the **shared wicked-estate
//! store**.
//!
//! This is the Rust port of the standalone council engine onto the wicked-apps spine
//! (`wicked-apps-core`). The proven logic is preserved:
//!
//! - the [`AgenticCli`](types::AgenticCli) registry (built-in verified seats ∪ a user
//!   TOML), with the real CLIs available in this environment (`claude`, `agy`, `pi`);
//! - a two-stage [`probe`] (PATH scan + bounded version probe with error-signature
//!   classification);
//! - a non-blocking [`worker`] state machine: `queue` persists + spawns a detached
//!   `std::thread` + returns a `task_id` at once; `poll` reads state→verdict;
//! - isolated, timeboxed parallel-safe [`dispatch`] (own tempdir, stdin=null, trust flags);
//! - 3-layer [`synthesis`] by **risk convergence** (NOT averaged confidence).
//!
//! ## What the port changed (storage + bus)
//! - **Storage → estate Nodes.** The old local-JSON ledger and JSON rank projection are
//!   replaced by persistence on the shared [`wicked_apps_core::SqliteStore`] (see [`store`]): a
//!   task is a `COUNCIL_TASK` node, the verdict is a `COUNCIL_VERDICT` node plus a
//!   `task → verdict` (`DECIDES`) edge, and each `(cli, work_kind)` ranking is a
//!   `CLI_RANKING` node. `to_node`/`from_node` round-trip; identity is `Symbol::synthetic`.
//! - **Bus → emit seam.** The old JSONL sink is replaced by [`bus::EmitSink`], which
//!   publishes coarse `wicked.council.requested` / `wicked.council.voted` /
//!   `wicked.cli.ranked` events (counts/ids only) through `wicked_apps_core::emit::emit_event`.
//!
//! The worker keeps its shared `Arc<Mutex<..>>` state for live coordination; the durable
//! task/verdict/ranking records are backed by the estate store.

pub mod bus;
pub mod dispatch;
pub mod ids;
pub mod probe;
pub mod registry;
pub mod store;
pub mod synthesis;
pub mod types;
pub mod worker;

// Re-export the seam-bearing surface at the crate root for ergonomic callers.
pub use bus::EmitSink;
pub use store::{EstateHandle, EstateRankStore, Ledger, TaskRecord};
pub use types::{
    AgenticCli, Category, Confidence, CouncilTask, Dispatcher, EventSink, InputMode, NoopEventSink,
    ProbeOutcome, Prober, RankSignal, RankStore, Ranking, TaskState, UnusableReason, Verdict, Vote,
    COUNCIL_EVENTS,
};
pub use worker::{PollStatus, Worker};

/// Crate identity smoke.
pub fn health() -> &'static str {
    "wicked-council"
}

/// Derive the ranking *work-kind* from a task's criteria — a coarse bucket the council
/// counts a CLI's performance toward. Falls back to `"general"` when no criteria are given.
///
/// Deterministic: the first criterion (normalised) names the bucket. This keeps rankings
/// per *kind of decision* without inventing a taxonomy the caller didn't provide.
pub fn work_kind_for(criteria: &[String]) -> String {
    criteria
        .iter()
        .map(|c| c.trim())
        .find(|c| !c.is_empty())
        .map(|c| c.to_lowercase())
        .unwrap_or_else(|| "general".to_string())
}

/// The default state directory for the council, cross-platform: `<home>/.wicked-council`.
/// Used by the binary (not by hermetic tests, which use `SqliteStore::in_memory`).
pub fn default_state_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".wicked-council")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_kind_buckets_by_first_criterion() {
        assert_eq!(
            work_kind_for(&["Code-Review".into(), "x".into()]),
            "code-review"
        );
        assert_eq!(work_kind_for(&[]), "general");
        assert_eq!(work_kind_for(&["  ".into(), "Arch".into()]), "arch");
    }

    #[test]
    fn council_events_are_in_the_shared_catalog_and_grammar_valid() {
        for ev in COUNCIL_EVENTS {
            assert!(
                wicked_apps_core::EVENT_CATALOG.contains(&ev),
                "{ev} is declared as produced but is not in the shared catalog"
            );
            assert!(
                wicked_apps_core::validate_event_type(ev),
                "{ev} is not a grammar-valid bus event type"
            );
        }
    }

    /// `COUNCIL_EVENTS` is the crate's published contract, so it must list every event the
    /// crate actually emits.
    ///
    /// The previous test asserted `COUNCIL_EVENTS == [the same three literals]`, which is
    /// tautological — it restates the constant rather than checking it, and passes no matter
    /// how far the constant drifts from reality. It did drift: `EV_COUNCIL_DELIBERATED` was
    /// emitted for several releases without ever being declared. Scanning the emitting source
    /// is the only version of this check that can fail.
    #[test]
    fn council_events_are_the_events_the_crate_emits() {
        let sources = [
            include_str!("worker.rs"),
            include_str!("dispatch.rs"),
            include_str!("store.rs"),
        ];
        let mut emitted: Vec<&str> = Vec::new();
        for src in sources {
            for (idx, _) in src.match_indices("events.emit(") {
                // The const path follows the open paren: `wicked_apps_core::EV_FOO,`
                let rest = &src[idx..];
                let Some(start) = rest.find("wicked_apps_core::EV_") else {
                    continue;
                };
                let name: String = rest[start + "wicked_apps_core::".len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_uppercase() || *c == '_' || c.is_ascii_digit())
                    .collect();
                // Map the const NAME back to its value via the declared list.
                let value = COUNCIL_EVENTS.iter().find(|v| match name.as_str() {
                    "EV_COUNCIL_REQUESTED" => **v == wicked_apps_core::EV_COUNCIL_REQUESTED,
                    "EV_COUNCIL_DELIBERATED" => **v == wicked_apps_core::EV_COUNCIL_DELIBERATED,
                    "EV_COUNCIL_SEAT_FAILED" => **v == wicked_apps_core::EV_COUNCIL_SEAT_FAILED,
                    "EV_COUNCIL_VOTED" => **v == wicked_apps_core::EV_COUNCIL_VOTED,
                    "EV_CLI_RANKED" => **v == wicked_apps_core::EV_CLI_RANKED,
                    _ => false,
                });
                assert!(
                    value.is_some(),
                    "{name} is emitted by this crate but is not declared in COUNCIL_EVENTS — \
                     the published event contract is incomplete"
                );
                emitted.push(value.expect("checked above"));
            }
        }
        // The scan must have found something; an empty scan would make the assertion above
        // vacuous and hide the very drift this test exists to catch.
        assert!(
            emitted.len() >= COUNCIL_EVENTS.len(),
            "scanned {} emit sites for {} declared events — the scan is not finding them",
            emitted.len(),
            COUNCIL_EVENTS.len()
        );
        for declared in COUNCIL_EVENTS {
            assert!(
                emitted.contains(&declared),
                "{declared} is declared as produced but nothing in this crate emits it"
            );
        }
    }
}
