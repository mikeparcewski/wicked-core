//! The one place a child process's inherited environment is decided.
//!
//! # Why this exists
//!
//! FINDING-067: a governed worker ran `wicked-estate index .` and deleted all 833 nodes of the
//! platform's operational state. The worker needed no `--db` argument — it inherited
//! `WICKED_ESTATE_DB` from the launcher, and every estate binary resolves `--db` ELSE that variable.
//!
//! The fix landed at the two launch paths that were known to matter (the wrapped worker and the ACP
//! worker). Enumerating `Command::new` afterwards found **three more** agent-facing spawn sites that
//! inherited the same environment and had never been considered: council seats
//! (`wicked-council::dispatch`), workflow tool phases (`actor::run_tool_cmd`, which runs an arbitrary
//! argv straight out of a `WorkflowDef`), and source recon (`sources::run_cli`).
//!
//! That is the actual defect. Not "the worker path leaked a variable" — *"hardening is applied at
//! call sites, so it covers exactly the paths someone remembered."* Every new spawn site starts
//! un-hardened and stays that way until an incident finds it.
//!
//! # The rule
//!
//! **Every `Command::new` in this workspace is followed by [`HardenedCommand::hardened`].** No
//! allowlist, no exceptions, not even for `git`.
//!
//! The exception-free form is deliberate. An allowlist ("git is harmless") is a standing invitation
//! to argue a new site onto it, and the argument is always locally reasonable — the incident above
//! happened because passing the operational store to the worker's MCP was locally reasonable too. A
//! rule with no exceptions is one a test can enforce mechanically, and mechanical enforcement is the
//! only kind that survives someone adding a spawn site at 2am. Stripping six variables from a `git`
//! invocation costs nothing.
//!
//! [`enforced_by_test`] is the enforcement: it fails the build when a new spawn site appears without
//! the call.
//!
//! # Ordering
//!
//! `hardened()` clears; it does not decide policy. A path that legitimately needs to hand a child one
//! of these variables sets it **after**:
//!
//! ```ignore
//! let mut cmd = Command::new(exe);
//! cmd.hardened();                                  // start from a known-clean slate
//! cmd.env(GATE_DB_ENV, &gov.db_path);              // then pass exactly what this path intends
//! ```
//!
//! Inverting that order silently restores whatever the parent happened to export, which is the
//! condition this module exists to remove. A boundary that depends on the daemon's environment is not
//! a boundary.

use std::process::Command;

/// Variables the engine uses to talk to *itself* — none of which any child may inherit by accident.
///
/// Membership test: would a process that reads this variable, having inherited rather than been
/// handed it, act on state belonging to the engine instead of state belonging to its own job? If yes,
/// it belongs here. That covers both stores and the gate-hook's argument channel — the hook is a
/// *grandchild* of the worker CLI, so the only way to reach it is through the worker's environment,
/// which means every tool the worker spawns sees those variables too.
///
/// Spelled as literals rather than imported from `wicked-core::gate_hook`, because this crate is
/// *below* that one in the dependency graph — council depends on this crate and cannot see the root.
/// The duplication is pinned by [`enforced_by_test`]'s sibling assertion in the root crate, which
/// compares these strings against the consts. Two spellings with a test between them; not two
/// spellings and a comment asking for discipline.
pub const ENGINE_INTERNAL_ENV: &[&str] = &[
    // The operational store, and the variable that caused FINDING-067.
    "WICKED_ESTATE_DB",
    // The gate hook's store. Only the hook subprocess may resolve this; a worker's own tools seeing
    // it is the same leak wearing a different name.
    "WICKED_GATE_DB",
    // The append-only decisions log. A child that inherits this can forge gate decisions.
    "WICKED_DECISIONS_PATH",
    // The hook's argument channel. Inherited, these silently re-scope another unit's governance onto
    // whatever the child does.
    "WICKED_GATE_SCOPE",
    "WICKED_GATE_PHASE",
    "WICKED_GATE_PHASE_ID",
    // The unit's filesystem boundary. Inherited at an unrelated spawn site these silently re-scope
    // one unit's worktree onto another child, and a child that can merely OBSERVE them learns
    // exactly where the fence is. The launcher sets them deliberately on the governed child.
    "WICKED_WRITE_ROOTS",
    "WICKED_READ_ROOTS",
];

/// Chainable environment hardening for [`Command`].
///
/// Returns `&mut Command` so it composes with the builder style every spawn site already uses:
/// `Command::new(bin).hardened().args(...)`. A free function taking `&mut Command` would have forced
/// each of the ~30 call sites to be restructured, and a migration that requires rewriting the call
/// site is a migration that gets skipped.
pub trait HardenedCommand {
    /// Remove every [`ENGINE_INTERNAL_ENV`] variable from what this child would inherit.
    ///
    /// Unconditional by design — it does not check whether the parent actually has them set. Whether
    /// the daemon's environment is clean today is an accident of how the operator started it, and
    /// hardening that only engages when it is already needed is hardening you cannot test.
    fn hardened(&mut self) -> &mut Command;
}

impl HardenedCommand for Command {
    fn hardened(&mut self) -> &mut Command {
        for key in ENGINE_INTERNAL_ENV {
            self.env_remove(key);
        }
        self
    }
}

/// Marker documenting where the rule is mechanically enforced.
///
/// The enforcement lives in the root crate (`wicked-core`), not here: it must scan the whole
/// workspace, including this crate and `wicked-council`, and only the root sees all of them. See
/// `wicked_core::spawn_audit`.
pub const fn enforced_by_test() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardened_removes_every_engine_internal_variable() {
        // Not a tautology over the const: this asserts the **mechanism**, that `hardened()` actually
        // reaches Command's env map, rather than that the list contains what it contains. The
        // observable proof that the strip works end-to-end (a real child reporting UNSET) lives in
        // `execute_wrapped::tests::no_worker_inherits_an_estate_store_through_the_environment`.
        let mut cmd = Command::new("true");
        for key in ENGINE_INTERNAL_ENV {
            cmd.env(key, "leaked");
        }
        cmd.hardened();

        let surviving: Vec<_> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_some())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert!(
            surviving.is_empty(),
            "hardened() left engine-internal variables set: {surviving:?}"
        );
    }

    #[test]
    fn hardened_does_not_disturb_unrelated_variables() {
        // The strip is targeted, not an env_clear(). A worker still needs PATH, HOME and the
        // operator's own CLI credentials to function; clearing wholesale would trade a leak for a
        // different bug (FINDING-047's neighbourhood) and get reverted the first time a run failed.
        let mut cmd = Command::new("true");
        cmd.env("PATH", "/usr/bin");
        cmd.env("WICKED_ESTATE_DB", "/leak");
        cmd.hardened();

        let kept: Vec<_> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_some())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert_eq!(kept, vec!["PATH".to_string()]);
    }

    #[test]
    fn a_path_may_still_pass_a_variable_deliberately_after_hardening() {
        // The ordering contract from the module docs. `hardened()` clears; it does not forbid. If
        // this ever failed, every governed run would lose its gate-hook store and deny every tool
        // call — the exact skew filed as core#167.
        let mut cmd = Command::new("true");
        cmd.hardened();
        cmd.env("WICKED_GATE_DB", "/run/store.db");

        let found = cmd
            .get_envs()
            .find(|(k, _)| k.to_string_lossy() == "WICKED_GATE_DB")
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().into_owned());
        assert_eq!(found.as_deref(), Some("/run/store.db"));
    }
}
