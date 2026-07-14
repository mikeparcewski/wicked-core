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
