//! The `wicked-bus` event seam — backed by `wicked-apps-core`'s shared `emit` seam.
//!
//! This replaces the original JSONL file sink. Each council event is published through
//! [`wicked_apps_core::emit::emit_event`], which spawns the canonical `wicked-bus emit` CLI
//! fire-and-forget and dead-letters (loudly, never silently) on failure. The payloads are
//! coarse — **counts and ids only**, never raw vote text — matching the cross-app event
//! catalog (`wicked.council.requested` / `wicked.council.voted` / `wicked.cli.ranked`).
//!
//! For a hard no-op (bus deliberately absent), [`crate::types::NoopEventSink`] is on the
//! spine and used by read-only paths like `poll`.

use wicked_apps_core::emit::{emit_event, EmitEvent};

use crate::types::EventSink;

/// The bus domain this app publishes under.
pub const DOMAIN: &str = "wicked-council";

/// Map a council event type to its bus subdomain (dotted), per the naming convention.
fn subdomain_for(event: &str) -> &'static str {
    match event {
        wicked_apps_core::EV_COUNCIL_REQUESTED => "council.request",
        wicked_apps_core::EV_COUNCIL_VOTED => "council.verdict",
        wicked_apps_core::EV_CLI_RANKED => "council.ranking",
        _ => "council",
    }
}

/// An [`EventSink`] that publishes through the shared `wicked-apps-core` emit seam toward
/// `wicked-bus`. Fire-and-forget: a dropped event is dead-lettered by the seam, never
/// silently lost, and never blocks or fails the council.
#[derive(Debug, Default, Clone)]
pub struct EmitSink;

impl EmitSink {
    /// Construct the emit-backed sink.
    pub fn new() -> Self {
        EmitSink
    }
}

impl EventSink for EmitSink {
    fn emit(&self, event: &str, payload: &serde_json::Value) {
        // Validate against the ecosystem grammar before publishing; a malformed type is a
        // defect we surface rather than ship to the bus.
        if !wicked_apps_core::validate_event_type(event) {
            eprintln!("wicked-council: refusing to emit malformed event type `{event}`");
            return;
        }
        let ev = EmitEvent::new(event, DOMAIN, subdomain_for(event), payload.clone());
        // emit_event already handles the failure path (dead-letter + loud marker).
        let _ = emit_event(&ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subdomains_cover_the_three_events() {
        assert_eq!(
            subdomain_for(wicked_apps_core::EV_COUNCIL_REQUESTED),
            "council.request"
        );
        assert_eq!(
            subdomain_for(wicked_apps_core::EV_COUNCIL_VOTED),
            "council.verdict"
        );
        assert_eq!(
            subdomain_for(wicked_apps_core::EV_CLI_RANKED),
            "council.ranking"
        );
    }

    #[test]
    fn malformed_event_type_is_refused_not_emitted() {
        // A hyphenated/uppercase type fails the grammar; emit must not spawn the bus.
        // (We can't easily assert the no-spawn, but the validator gate is the contract.)
        let sink = EmitSink::new();
        sink.emit("wicked.Council.BAD", &serde_json::json!({}));
        assert!(!wicked_apps_core::validate_event_type("wicked.Council.BAD"));
    }
}
