//! WRITE POSTURE — what a governed unit's tool boundary lets it WRITE, derived from the unit's
//! ROLE and the write roots the run was granted (F-4R2-004), never from `executes_code` alone.
//!
//! ## The defect this closes
//!
//! F-036 gave every `executes_code: false` agent phase a READ-ONLY posture at the tool boundary —
//! the wrapped carrier's `--sandbox read-only` / `--exclude-tools edit,write`, the gate hook's
//! NO-CODE scope, the ACP carrier's refusal of every write-class `session/request_permission`. The
//! posture was keyed on the one marker the worktree guard reads
//! ([`crate::domain::WorkUnit::worktree_guarded`] = `!executes_code && !tool`), and the guard's
//! marker is a SUPERSET of "evaluator": a CREATOR phase whose deliverable is a document outside
//! the tree also declares `executes_code: false` — every wicked-crew interactive seam
//! (`interactive-draft/edit/chat`: `draft`, `edit`, `revise`), the steering `propose`, repo-learn's
//! `capture`, `memories`' `store`. Those runs are UNBOUND (no repo, no worktree) and launch with
//! `extra_write_roots` = the per-run inbox the deliverable must land in; the launch validated the
//! root, the deliverable floor looked for the file there — and the ACP boundary refused the
//! creator's `Write` of that very file as a "read-only evaluator" (runs 37f020cc / 2c56cea1: the
//! `revise` worker respected the refusal, returned prose, the floor failed, the run failed).
//!
//! ## The rule
//!
//! `executes_code: false` means "this phase does not change the tree under review". It has never
//! meant "this phase writes nothing": the worktree guard, which owns that doctrine, compares the
//! WORKTREE and nothing else. So the posture is decided per unit from three facts the engine
//! already holds — the role, the guard marker, and whether the run has a tree at all:
//!
//! | phase                                     | posture                 |
//! |-------------------------------------------|-------------------------|
//! | `executes_code: true`, Tool, prose-planned | [`WritePosture::Full`]  |
//! | `executes_code: false`, evaluator/neutral  | [`WritePosture::ReadOnly`] |
//! | `executes_code: false`, creator, BOUND     | [`WritePosture::DeliverableRoots`] |
//! | `executes_code: false`, creator, UNBOUND   | [`WritePosture::Full`]  |
//!
//! A creator keeps `Write`/`Edit` INSIDE its granted write roots and is refused everywhere else —
//! the worktree included when there is one, because the guard would deny that change at the fold
//! anyway and a call the boundary allows and the gate then denies is a contradiction, not a
//! policy. An unbound creator has no tree to protect: its cwd is a throwaway sandbox and its
//! deliverable a file in a declared root, so the ordinary filesystem boundary (cwd + the
//! launch-validated extras) is the whole posture. Evaluators and recon rungs stay read-only: their
//! verdict is their output.
//!
//! One derivation, read by every carrier (wrapped argv lever, gate-hook env, ACP permission
//! bridge, PTY session) so they cannot disagree about who may write what — the same reason
//! `pre_build_scope` and `worktree_guarded` are single plan-time markers.

use std::ffi::OsStr;

use crate::domain::WorkUnit;
use crate::workflow::PhaseRole;

/// The write posture a governed unit's tool boundary applies. See the module docs for the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WritePosture {
    /// The ordinary filesystem boundary: the unit cwd plus the launch-validated extra write roots.
    /// No phase-level fence on top.
    Full,
    /// A BOUND creator whose phase declared `executes_code: false`: write-class calls are allowed
    /// inside the run's declared extra write roots and refused everywhere else, the worktree
    /// (the tree under review) included.
    DeliverableRoots,
    /// An `executes_code: false` agent phase that does not play creator — an evaluator, a recon
    /// rung, a review: every write-class call is refused. `bash` stays (the phase must run the
    /// suite); the worktree guard holds the rest.
    ReadOnly,
}

impl WritePosture {
    /// Derive the posture for `unit`. `bound` is whether the run has a worktree (`StepInput::workdir`
    /// / `AgentSession::workdir` is `Some`): the tree the guard protects and the creator fence
    /// excludes. Read off the unit's plan-time markers, never guessed from the prompt.
    pub(crate) fn of(unit: &WorkUnit, bound: bool) -> Self {
        if !crate::worktree_guard::applies_to(unit) {
            return WritePosture::Full;
        }
        match unit.role {
            PhaseRole::Creator if bound => WritePosture::DeliverableRoots,
            PhaseRole::Creator => WritePosture::Full,
            PhaseRole::Evaluator | PhaseRole::Neutral => WritePosture::ReadOnly,
        }
    }

    /// Whether this posture fences writes at all (anything but [`WritePosture::Full`]). This is
    /// what a carrier keys process/session ISOLATION and end-of-unit QUIESCE on (F-036): a fenced
    /// unit's process is never shared with a write-posture one and is killed, group and all, when
    /// the unit ends — so nothing it backgrounded can land after the guard's final snapshot.
    pub(crate) fn fences_writes(self) -> bool {
        self != WritePosture::Full
    }

    /// The short operator-facing label used in log lines and the denial event (`posture`).
    pub(crate) fn label(self) -> &'static str {
        match self {
            WritePosture::Full => "full",
            WritePosture::DeliverableRoots => "deliverable-roots",
            WritePosture::ReadOnly => "read-only",
        }
    }

    /// The value the wrapped launcher sets on [`crate::gate_hook::NO_CODE_SCOPE_ENV`] for the hook
    /// subprocess, or `None` for [`WritePosture::Full`] — UNSET is the honest "no phase fence"
    /// state, exactly as the pre-build scope env behaves.
    pub(crate) fn env_value(self) -> Option<&'static str> {
        match self {
            WritePosture::Full => None,
            WritePosture::DeliverableRoots | WritePosture::ReadOnly => Some(self.label()),
        }
    }

    /// The pure half of the env read. Parsed STRICTLY: `read-only` (and the legacy `1`/`true`
    /// spelling the launcher set before postures existed) ⇒ [`WritePosture::ReadOnly`];
    /// `deliverable-roots` ⇒ [`WritePosture::DeliverableRoots`]; unset or anything else ⇒
    /// [`WritePosture::Full`]. An inherited junk value must never scope a build phase away from
    /// building — the inverse failure, and a louder one than the one the fence closes.
    pub(crate) fn parse_env(raw: Option<&OsStr>) -> Self {
        match raw.and_then(OsStr::to_str).map(str::trim) {
            Some(s)
                if s.eq_ignore_ascii_case("read-only")
                    || s.eq_ignore_ascii_case("1")
                    || s.eq_ignore_ascii_case("true") =>
            {
                WritePosture::ReadOnly
            }
            Some(s) if s.eq_ignore_ascii_case("deliverable-roots") => {
                WritePosture::DeliverableRoots
            }
            _ => WritePosture::Full,
        }
    }
}

/// The role noun for operator-facing wording — the denial names WHO was refused, so a creator is
/// never reported as an evaluator (the F-4R2-004 log read "read-only evaluator" for a creator).
pub(crate) fn role_noun(role: PhaseRole) -> &'static str {
    match role {
        PhaseRole::Creator => "creator",
        PhaseRole::Evaluator => "evaluator",
        PhaseRole::Neutral => "neutral (recon/review)",
    }
}

/// The wire spelling of the role for the denial event (`role`), matching `PhaseRole`'s serde form.
pub(crate) fn role_wire(role: PhaseRole) -> &'static str {
    match role {
        PhaseRole::Creator => "creator",
        PhaseRole::Evaluator => "evaluator",
        PhaseRole::Neutral => "neutral",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(role: PhaseRole, executes_code: bool) -> WorkUnit {
        let mut u = WorkUnit::pending("r:phase", "r", 2, "do it");
        u.role = role;
        u.executes_code = executes_code;
        // The plan-time marker (`plan_from_def`): a def-driven agent phase that said it would not
        // change the tree.
        u.worktree_guarded = !executes_code;
        u
    }

    /// F-4R2-004: the posture follows the ROLE and the tree, not `executes_code` alone. A creator
    /// whose phase declares `executes_code: false` keeps its write roots (fenced to them when a
    /// tree exists, the ordinary boundary when none does); evaluators and recon rungs stay
    /// read-only; a code phase and a tool phase carry no fence.
    #[test]
    fn posture_is_derived_from_role_and_boundness_not_executes_code_alone() {
        let creator = unit(PhaseRole::Creator, false);
        assert_eq!(
            WritePosture::of(&creator, true),
            WritePosture::DeliverableRoots,
            "a BOUND creator that declared no code writes only its deliverable roots"
        );
        assert_eq!(
            WritePosture::of(&creator, false),
            WritePosture::Full,
            "an UNBOUND creator (crew's interactive seams) has no tree to fence — its sandbox and \
             declared roots ARE the boundary"
        );
        for role in [PhaseRole::Evaluator, PhaseRole::Neutral] {
            let u = unit(role, false);
            assert_eq!(
                WritePosture::of(&u, true),
                WritePosture::ReadOnly,
                "{role:?}"
            );
            assert_eq!(
                WritePosture::of(&u, false),
                WritePosture::ReadOnly,
                "{role:?}: an evaluator stays read-only even on an unbound run — its verdict is \
                 its output"
            );
        }
        let code = unit(PhaseRole::Creator, true);
        assert_eq!(WritePosture::of(&code, true), WritePosture::Full);
        let mut tool = unit(PhaseRole::Neutral, false);
        tool.tool_cmd = Some(vec!["true".into()]);
        assert_eq!(
            WritePosture::of(&tool, true),
            WritePosture::Full,
            "a tool unit is the engine's own command"
        );
        let prose = WorkUnit::pending("r:x", "r", 1, "prose-planned");
        assert_eq!(WritePosture::of(&prose, true), WritePosture::Full);
    }

    #[test]
    fn fence_isolation_and_labels_follow_the_posture() {
        assert!(!WritePosture::Full.fences_writes());
        assert!(WritePosture::DeliverableRoots.fences_writes());
        assert!(WritePosture::ReadOnly.fences_writes());
        assert_eq!(WritePosture::ReadOnly.label(), "read-only");
        assert_eq!(WritePosture::DeliverableRoots.label(), "deliverable-roots");
        assert_eq!(
            WritePosture::Full.env_value(),
            None,
            "UNSET is the honest no-fence state"
        );
        assert_eq!(WritePosture::ReadOnly.env_value(), Some("read-only"));
        assert_eq!(
            WritePosture::DeliverableRoots.env_value(),
            Some("deliverable-roots")
        );
        assert_eq!(role_noun(PhaseRole::Creator), "creator");
        assert_eq!(role_noun(PhaseRole::Evaluator), "evaluator");
        assert_eq!(role_wire(PhaseRole::Neutral), "neutral");
    }

    /// The env carrier round-trips every posture, keeps the legacy `1`/`true` spelling as
    /// read-only, and treats unset or junk as NO fence (strict parse, as the pre-build scope).
    #[test]
    fn env_spelling_round_trips_and_junk_is_no_fence() {
        let os = |s: &str| std::ffi::OsString::from(s);
        for p in [WritePosture::ReadOnly, WritePosture::DeliverableRoots] {
            let v = os(p.env_value().unwrap());
            assert_eq!(WritePosture::parse_env(Some(&v)), p);
        }
        for legacy in ["1", "true", "TRUE", " read-only "] {
            let v = os(legacy);
            assert_eq!(
                WritePosture::parse_env(Some(&v)),
                WritePosture::ReadOnly,
                "{legacy}"
            );
        }
        assert_eq!(WritePosture::parse_env(None), WritePosture::Full);
        for junk in ["", "0", "false", "yes", "readonly", "deliverable"] {
            let v = os(junk);
            assert_eq!(
                WritePosture::parse_env(Some(&v)),
                WritePosture::Full,
                "{junk:?} must not scope a build phase away from building"
            );
        }
    }
}
