//! The single-entity-vs-separate toggle (§6) — ported into COE from the retired wicked-agent.
//!
//! What makes N CLIs ONE entity is not a harness — it is that their outputs read and write the SAME
//! collection scope on the shared store:
//!   - [`EntityMode::Shared`]   → every unit's output goes to ONE session scope (one entity).
//!   - [`EntityMode::Isolated`] → every unit gets its OWN scope (independent mini-sessions).
//!
//! NOTE: the scope strings keep the `wicked-agent/...` prefix on purpose — it is a STABLE persisted
//! identifier that existing sessions + governance policies key on, not a dependency on the agent
//! binary. Changing it would orphan every prior session's scope.

use serde::{Deserialize, Serialize};

/// The collection-scope mode for a session (§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityMode {
    /// One collection scope for ALL units' outputs — N hands, one entity.
    Shared,
    /// Per-unit collection scope — genuinely independent outputs on the same store.
    Isolated,
}

impl EntityMode {
    /// Parse the CLI/JSON token (`shared` | `isolated`); anything else defaults to `shared`.
    pub fn parse(s: &str) -> EntityMode {
        match s.trim().to_ascii_lowercase().as_str() {
            "isolated" => EntityMode::Isolated,
            _ => EntityMode::Shared,
        }
    }

    /// The wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            EntityMode::Shared => "shared",
            EntityMode::Isolated => "isolated",
        }
    }
}

/// The orchestration PHASE id for a unit — the single source of truth. Both the input-governance hook
/// (via `--phase`) and the actor-side fold key on this exact string, so any drift would land claims at a
/// phase the fold never queries (a silent allow). Derive it here, never re-`format!` it ad hoc.
pub fn unit_phase(ord: u32) -> String {
    format!("unit-{ord}")
}

/// Every phase token a governance policy may legitimately name for one unit, in match order.
///
/// [`unit_phase`] is an EXECUTION detail: `unit-3` is derived from a unit's ordinal and is nowhere
/// in the workflow an operator authored. The token they see — in the def, in `GET /runs/:id`, in
/// the unit id (`<session>:<phase_id>`) — is the workflow phase id, e.g. `review`. Selecting on the
/// synthetic token alone meant a policy authored as `applies_to: ["review"]` registered fine and
/// then never fired: a silent fail-open on the primary safety control (FINDING-021). Accept both.
///
/// `alias` is [`crate::domain::WorkUnit::phase_id`]; it is dropped when absent or already equal to
/// `phase` so the result is duplicate-free.
pub fn phase_aliases<'a>(phase: &'a str, alias: Option<&'a str>) -> Vec<&'a str> {
    let mut tokens = vec![phase];
    if let Some(alias) = alias.filter(|a| *a != phase) {
        tokens.push(alias);
    }
    tokens
}

/// Resolve the collection scope for `discriminator` under `mode`.
///
/// - `Shared`   → `wicked-agent/<session>/shared` (the discriminator is ignored — all share it).
/// - `Isolated` → `wicked-agent/<session>/unit/<discriminator>` (each unit/CLI its own scope).
pub fn resolve_scope(mode: EntityMode, session_id: &str, discriminator: &str) -> String {
    match mode {
        EntityMode::Shared => format!("wicked-agent/{session_id}/shared"),
        EntityMode::Isolated => format!("wicked-agent/{session_id}/unit/{discriminator}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The alias list is the two tokens a policy may name, deduplicated, `phase` first.
    #[test]
    fn phase_aliases_pairs_synthetic_and_workflow_tokens() {
        assert_eq!(
            phase_aliases("unit-3", Some("review")),
            vec!["unit-3", "review"]
        );
        // No alias (prose-planned or hand-built unit) ⇒ the synthetic token alone.
        assert_eq!(phase_aliases("unit-3", None), vec!["unit-3"]);
        // A workflow phase literally named `unit-3` must not be listed twice.
        assert_eq!(phase_aliases("unit-3", Some("unit-3")), vec!["unit-3"]);
    }

    /// `phase_id` recovers the workflow phase from the unit id, and refuses to guess when the id
    /// does not carry the session prefix (a wrong guess would select the wrong policies).
    #[test]
    fn work_unit_phase_id_is_the_suffix_after_the_session_prefix() {
        use crate::domain::WorkUnit;
        let def_driven = WorkUnit::pending("sess-1:review", "sess-1", 3, "d");
        assert_eq!(def_driven.phase_id(), Some("review"));

        // Prose-planned runs carry the `u<ord>` token — still the id's own suffix.
        let prose = WorkUnit::pending("sess-1:u3", "sess-1", 3, "d");
        assert_eq!(prose.phase_id(), Some("u3"));

        // A session id containing a colon must not be mis-split (strip the prefix, not the first ':').
        let colonated = WorkUnit::pending("run:2026:build", "run:2026", 1, "d");
        assert_eq!(colonated.phase_id(), Some("build"));

        // No session prefix, and a bare `<session>:` with an empty suffix, both yield None.
        assert_eq!(
            WorkUnit::pending("bare-id", "sess-1", 1, "d").phase_id(),
            None
        );
        assert_eq!(
            WorkUnit::pending("sess-1:", "sess-1", 1, "d").phase_id(),
            None
        );
    }

    #[test]
    fn shared_pins_all_to_one_scope() {
        let a = resolve_scope(EntityMode::Shared, "s1", "u1");
        let b = resolve_scope(EntityMode::Shared, "s1", "u2");
        assert_eq!(
            a, b,
            "shared mode: every unit shares ONE scope (one entity)"
        );
        assert_eq!(a, "wicked-agent/s1/shared");
    }

    #[test]
    fn isolated_gives_each_its_own_scope() {
        let a = resolve_scope(EntityMode::Isolated, "s1", "u1");
        let b = resolve_scope(EntityMode::Isolated, "s1", "u2");
        assert_ne!(a, b, "isolated mode: each unit gets its OWN scope");
        assert_eq!(a, "wicked-agent/s1/unit/u1");
    }

    #[test]
    fn parse_defaults_to_shared() {
        assert_eq!(EntityMode::parse("isolated"), EntityMode::Isolated);
        assert_eq!(EntityMode::parse("shared"), EntityMode::Shared);
        assert_eq!(EntityMode::parse("garbage"), EntityMode::Shared);
        assert_eq!(EntityMode::parse("ISOLATED"), EntityMode::Isolated);
    }
}
