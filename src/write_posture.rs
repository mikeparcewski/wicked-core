//! WRITE POSTURE — what a governed unit's tool boundary lets it WRITE, derived from the unit's
//! ROLE and the write roots the run was granted (F-4R2-004), never from `executes_code` alone.
//!
//! ## The defect this closes
//!
//! F-036 gave every `executes_code: false` agent phase a READ-ONLY posture at the tool boundary —
//! the wrapped carrier's `--sandbox read-only` / `--exclude-tools edit,write`, the gate hook's
//! NO-CODE scope, the ACP carrier's refusal of every write-class `session/request_permission`. The
//! posture was keyed on the one marker the worktree guard reads
//! ([`crate::domain::WorkUnit::worktree_guarded`], then `!executes_code && !tool`), and the guard's
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
//! | catalog `walkthrough_plan` (evaluator), BOUND | [`WritePosture::DeliverableRoots`] |
//! | `executes_code: false`, evaluator/neutral  | [`WritePosture::ReadOnly`] |
//! | `executes_code: false`, creator, BOUND     | [`WritePosture::DeliverableRoots`] |
//! | `executes_code: false`, creator, UNBOUND   | [`WritePosture::Full`]  |
//!
//! The first row is the one evaluator that writes (WT-C1, DES-walkthrough-proof §4.3 B3): the
//! walkthrough author writes its storyline into `<evidence root>/author/<step>/`, a declared
//! extra write root, never the tree. It is keyed off the catalog id, never the prompt; unbound it
//! stays read-only like every other evaluator.
//!
//! A creator keeps `Write`/`Edit` INSIDE its granted write roots and is refused everywhere else —
//! the worktree included when there is one, because the guard would deny that change at the fold
//! anyway and a call the boundary allows and the gate then denies is a contradiction, not a
//! policy. An unbound creator has no tree to protect: its cwd is a throwaway sandbox and its
//! deliverable a file in a declared root, so the ordinary filesystem boundary (cwd + the
//! launch-validated extras) is the whole posture. Evaluators and recon rungs stay read-only: their
//! verdict is their output (the walkthrough author excepted, see the table).
//!
//! D14: since the plan stopped guarding creators (`worktree_guarded = !executes_code && !tool &&
//! role != Creator`), a creator planned by this engine is never guarded and so gets
//! [`WritePosture::Full`] bound or not — its documents land in the worktree and the later review
//! steps judge them on distinct seats. [`WritePosture::DeliverableRoots`] is still what a creator
//! unit PERSISTED with the old marker (`worktree_guarded: true`) gets, consistently with the guard
//! that still covers it.
//!
//! One derivation, read by every carrier (wrapped argv lever, gate-hook env, ACP permission
//! bridge, PTY session) so they cannot disagree about who may write what — the same reason
//! `pre_build_scope` and `worktree_guarded` are single plan-time markers.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::domain::WorkUnit;
use crate::workflow::PhaseRole;

/// The write posture a governed unit's tool boundary applies. See the module docs for the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WritePosture {
    /// The ordinary filesystem boundary: the unit cwd plus the launch-validated extra write roots.
    /// No phase-level fence on top.
    Full,
    /// A BOUND creator whose phase declared `executes_code: false`, or the bound walkthrough author
    /// (catalog `walkthrough_plan`, an evaluator; WT-C1): write-class calls are allowed inside the
    /// run's declared extra write roots and refused everywhere else, the worktree (the tree under
    /// review) included.
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
        // The PA's review of a member's step (DES-TEAMING-002 §8.8) is an evaluator turn whatever
        // the step's own role or phase: its verdict is its output, the tree is the member's, and
        // it gets no write tools (review round 2 on #628, D2).
        if unit.is_member_step_review() && unit.tool_cmd.is_none() {
            return WritePosture::ReadOnly;
        }
        // WT-C1 (DES-walkthrough-proof §4.3, B3): the walkthrough author is an evaluator that
        // writes exactly one file, its storyline, into the run's declared author dir. Keyed off
        // the catalog entry, never the prompt; on a bound run only (unbound it stays read-only,
        // and a repo-less walkthrough fails closed at its pinned validator anyway). Spelled
        // `deliverable-roots` on the hook env — an EVALUATOR carrying it is why gate protocol 2
        // exists (an older hook would parse it as no fence; a mismatched hook refuses instead).
        // Decided BEFORE the guard marker (Copilot review on #691): a plan step may raise
        // `executes_code` (TightenOnly), which clears the marker — that must never turn the
        // author's fence into `Full`.
        if unit.tool_cmd.is_none()
            && unit.catalog.as_deref() == Some(crate::catalog::WALKTHROUGH_PLAN)
        {
            return if bound {
                WritePosture::DeliverableRoots
            } else {
                WritePosture::ReadOnly
            };
        }
        // (wicked-core#881, wicked-crew#951) The run's QE unit records its acceptance verdict
        // through garden's pipeline — an interpreter writing the run's QE ledger root — so on a
        // bound run it takes the deliverable-roots posture, admitting ONLY its own roots
        // ([`unit_admitted_roots`]): interpreters run, every path write is fenced to those roots,
        // and the worktree guard still restores and escalates any change to the tree. Read-only,
        // its fenced seats (claude, opencode) refused `node` and could never record the verdict.
        // Decided before the guard marker, like the walkthrough author.
        if crate::qe_acceptance::carries_ledger_root(unit) {
            return if bound {
                WritePosture::DeliverableRoots
            } else {
                WritePosture::ReadOnly
            };
        }
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
    ///
    /// The read-only posture is written as `1` — the spelling the hook parsed BEFORE postures
    /// existed (strict `1`/`true`), on purpose (independent review of #444, F-01): the hook is the
    /// STANDALONE `wicked-core` binary the daemon finds on PATH, so a pre-posture hook must still
    /// read an evaluator's fence as ON. `deliverable-roots` is the newer spelling; a protocol-1
    /// hook reads it as "no fence". That was acceptable while only a creator carried it (a bound
    /// creator became guard-only there). WT-C1 gives it to an EVALUATOR (the walkthrough author),
    /// so gate protocol 2 ([`crate::gate_hook::GATE_PROTOCOL_VERSION`]) makes a protocol-1 hook
    /// refuse to arm instead of running that evaluator unfenced.
    pub(crate) fn env_value(self) -> Option<&'static str> {
        match self {
            WritePosture::Full => None,
            WritePosture::ReadOnly => Some("1"),
            WritePosture::DeliverableRoots => Some("deliverable-roots"),
        }
    }

    /// The pure half of the env read. Parsed STRICTLY: `1`/`true` (what the launcher writes, and
    /// what it wrote before postures existed) or the label `read-only` ⇒
    /// [`WritePosture::ReadOnly`]; `deliverable-roots` ⇒ [`WritePosture::DeliverableRoots`]; unset
    /// or anything else ⇒ [`WritePosture::Full`]. An inherited junk value must never scope a build
    /// phase away from building — the inverse failure, and a louder one than the one the fence
    /// closes.
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

/// The DELIVERABLE ROOTS a fenced creator may write: EXACTLY the run's launch-validated
/// `extra_write_roots` ([`crate::workflow::GovernanceContext::extra_write_roots`]) as paths, in
/// declaration order, nothing added and nothing dropped — nothing else. Not the unit cwd (the tree
/// under review, which the fence excludes), not the repo-graph key directory the filesystem
/// boundary also admits (engine scratch, written by the estate MCP — never a seat's deliverable),
/// not the state home. ONE derivation for every carrier (independent review of #444, F-02), reached
/// through [`admitted_roots`]: the ACP fence holds it in-process, the wrapped launcher arms it on
/// [`crate::gate_hook::DELIVERABLE_ROOTS_ENV`] for the hook subprocess, and the gate hook judges
/// the same list — so the two carriers cannot disagree about where a creator's deliverable may
/// land. No declared roots ⇒ every creator write is refused (fail closed), the same rule the
/// boundary applies to an unarmed run. (`deliverable_roots_of(Option<&GovernanceContext>)` was
/// deleted with DES-L4 PR-②: `admitted_roots` is the one entry point.)
pub(crate) fn deliverable_roots_from(extra_write_roots: &[String]) -> Vec<PathBuf> {
    extra_write_roots.iter().map(PathBuf::from).collect()
}

/// The ADMITTED out-of-tree write roots of a posture (DES-L4 PR-②, D-12): the ONE list both
/// carriers judge a fenced unit's writes against — path-bearing tools and Bash write targets alike.
/// `DeliverableRoots` → the creator's launch-validated `extra_write_roots`
/// ([`deliverable_roots_from`]); `ReadOnly` → the unit's NOTES ROOT (`WorkUnit::notes_root`, minted
/// at dispatch for a bound read-only unit, core#464 — the sanctioned place outside the tree an
/// evaluator may write its analysis; `[]` when the unit has none); `Full` → nothing (no fence).
/// The wrapped launcher arms this list on [`crate::gate_hook::DELIVERABLE_ROOTS_ENV`], the ACP
/// carrier holds it in-process (`AcpWritePosture::deliverable_roots`), and the gate hook judges it —
/// the same variable and the same judgement ([`deliverable_write_admitted`]) for both postures.
pub(crate) fn admitted_roots(
    posture: WritePosture,
    notes_root: Option<&str>,
    extra_write_roots: &[String],
) -> Vec<PathBuf> {
    match posture {
        WritePosture::DeliverableRoots => deliverable_roots_from(extra_write_roots),
        WritePosture::ReadOnly => notes_root.into_iter().map(PathBuf::from).collect(),
        WritePosture::Full => Vec::new(),
    }
}

/// [`admitted_roots`] for a dispatched `unit` — what every carrier arms: under the read-only
/// posture the unit's notes root and then (wicked-crew#951) its QE LEDGER ROOT
/// (`WorkUnit::qe_ledger_root`, the run's ledger outside the tree, set at dispatch on the QE unit)
/// — the two engine-minted places outside the tree a read-only unit may write. The notes root
/// stays FIRST: the read-only refusal names the first admitted root as where notes go.
pub(crate) fn unit_admitted_roots(
    posture: WritePosture,
    unit: &WorkUnit,
    extra_write_roots: &[String],
) -> Vec<PathBuf> {
    // (wicked-core#881) A fenced unit carrying a QE ledger root — the QE unit, deliverable-roots
    // on a bound run — admits exactly its own roots, never the launch extras (another phase's
    // deliverables, e.g. the walkthrough author's dir).
    if posture != WritePosture::Full && crate::qe_acceptance::carries_ledger_root(unit) {
        return unit
            .notes_root
            .iter()
            .chain(unit.qe_ledger_root.iter())
            .map(PathBuf::from)
            .collect();
    }
    admitted_roots(posture, unit.notes_root.as_deref(), extra_write_roots)
}

/// The env spelling of [`admitted_roots`] for the hook-subprocess carrier: the roots joined
/// with the platform's PATH separator, exactly as `WICKED_WRITE_ROOTS` is. `None` when a root
/// contains the separator and cannot be joined — the launcher then arms an EMPTY list rather than
/// a partial one, and the hook refuses every creator write (fail closed); `Some("")` for no roots.
pub(crate) fn deliverable_roots_env(roots: &[PathBuf]) -> Option<std::ffi::OsString> {
    if roots.is_empty() {
        return Some(std::ffi::OsString::new());
    }
    std::env::join_paths(roots.iter().map(|r| r.as_os_str())).ok()
}

/// The pure half of the [`crate::gate_hook::DELIVERABLE_ROOTS_ENV`] read: the inverse of
/// [`deliverable_roots_env`]. Unset or empty ⇒ no roots.
pub(crate) fn parse_deliverable_roots_env(raw: Option<&OsStr>) -> Vec<PathBuf> {
    match raw {
        Some(v) if !v.is_empty() => std::env::split_paths(v).collect(),
        _ => Vec::new(),
    }
}

/// THE deliverable-roots judgement, shared by the ACP fence (`acp_runner::AcpWritePosture::judge`)
/// and the gate hook (`gate_hook::phase_scope_denial`): a write-class call's RAW path (as the agent
/// spelled it) is admitted iff it resolves inside one of `deliverable_roots` AND not inside `cwd`
/// (the tree under review). Both containment tests run the boundary's own normalize →
/// symlink-resolve → containment chain ([`crate::path_policy::raw_resolves_within`]), so a relative
/// spelling, a `..` hop, a `/tmp`→`/private/tmp` alias or a Windows verbatim prefix is judged on
/// where it LANDS, identically on both carriers.
pub(crate) fn deliverable_write_admitted(
    raw_path: &str,
    cwd: &Path,
    home: Option<&Path>,
    deliverable_roots: &[PathBuf],
) -> bool {
    !crate::path_policy::raw_resolves_within(raw_path, cwd, home, cwd)
        && deliverable_roots
            .iter()
            .any(|root| crate::path_policy::raw_resolves_within(raw_path, cwd, home, root))
}

/// The operator-facing spelling of the roots a refusal names — the list, or the honest "none".
pub(crate) fn describe_deliverable_roots(roots: &[PathBuf]) -> String {
    if roots.is_empty() {
        "(none declared — the run granted no write root outside the tree)".to_string()
    } else {
        roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
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
        // The marker a unit persisted before D14 carried (`plan_from_def` then guarded every
        // def-driven agent phase that said it would not change the tree, creators included).
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
        assert_eq!(
            WritePosture::ReadOnly.env_value(),
            Some("1"),
            "the read-only spelling is the one a pre-posture hook binary parses (F-01)"
        );
        assert_eq!(
            WritePosture::DeliverableRoots.env_value(),
            Some("deliverable-roots")
        );
        assert_eq!(role_noun(PhaseRole::Creator), "creator");
        assert_eq!(role_noun(PhaseRole::Evaluator), "evaluator");
        assert_eq!(role_wire(PhaseRole::Neutral), "neutral");
    }

    /// The env carrier round-trips every posture, reads `1`/`true` (what the launcher writes now
    /// AND wrote before postures existed) and the `read-only` label as read-only, and treats
    /// unset or junk as NO fence (strict parse, as the pre-build scope).
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

    /// F-02 (independent review of #444): the deliverable roots are EXACTLY the governance
    /// context's `extra_write_roots` — derived once, and the env carrier the wrapped launcher arms
    /// for the hook subprocess round-trips to the identical list the ACP fence holds in-process.
    /// No governance context ⇒ no roots (fail closed).
    #[test]
    fn deliverable_roots_are_exactly_the_extra_write_roots_on_every_carrier() {
        let inbox = std::env::temp_dir().join("wicked-f02-inbox");
        let second = std::env::temp_dir().join("wicked-f02-second");
        let g = crate::workflow::GovernanceContext {
            human_confirm: Default::default(),
            db_path: "/state/core.db".into(),
            code_graph_db: Some("/state/repo-graphs/key/graph.db".into()),
            extra_write_roots: vec![
                inbox.to_string_lossy().into_owned(),
                second.to_string_lossy().into_owned(),
            ],
            extra_read_roots: vec!["/somewhere/readonly".into()],
            project_id: None,
        };
        let in_process = deliverable_roots_from(&g.extra_write_roots);
        assert_eq!(
            in_process,
            vec![inbox.clone(), second.clone()],
            "the roots are the extras and nothing else — not the graph db, not a read root"
        );
        let env = deliverable_roots_env(&in_process).expect("temp paths carry no separator");
        let via_hook = parse_deliverable_roots_env(Some(&env));
        assert_eq!(
            via_hook, in_process,
            "the env carrier round-trips to the same list"
        );
        assert!(deliverable_roots_from(&[]).is_empty());
        assert!(parse_deliverable_roots_env(None).is_empty());
        assert!(parse_deliverable_roots_env(Some(&deliverable_roots_env(&[]).unwrap())).is_empty());
        assert!(describe_deliverable_roots(&[]).contains("none declared"));
        assert!(describe_deliverable_roots(&in_process).contains(&inbox.display().to_string()));
    }

    /// THE shared judgement: inside a root and outside the tree ⇒ admitted; the tree, an
    /// engine-owned dir the filesystem boundary admits (the repo-graph key dir), or anywhere else
    /// ⇒ refused — on the resolved target, not the spelling.
    #[test]
    fn deliverable_write_admitted_is_roots_only_and_never_the_tree() {
        let base = std::env::temp_dir().join(format!(
            "wicked-f02-judge-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let wt = base.join("wt");
        let inbox = base.join("inbox");
        let graph = base.join("repo-graphs").join("key");
        for d in [&wt, &inbox, &graph] {
            std::fs::create_dir_all(d).unwrap();
        }
        let roots = vec![inbox.clone()];
        let ok = |p: &std::path::Path| {
            deliverable_write_admitted(&p.to_string_lossy(), &wt, None, &roots)
        };
        assert!(ok(&inbox.join("revised.html")));
        assert!(ok(&inbox.join("sub").join("fragment-1.html")));
        assert!(
            ok(&wt.join("..").join("inbox").join("revised.html")),
            "a `..` hop that lands in the root is judged where it lands"
        );
        assert!(!ok(&wt.join("src").join("app.ts")), "the tree under review");
        assert!(
            !deliverable_write_admitted("src/app.ts", &wt, None, &roots),
            "a relative spelling lands in the tree"
        );
        assert!(
            !ok(&graph.join("graph.db-wal")),
            "the repo-graph key dir is engine scratch, not a deliverable root (F-02)"
        );
        assert!(!ok(&base.join("elsewhere.html")));
        assert!(!deliverable_write_admitted(
            &inbox.join("x.html").to_string_lossy(),
            &wt,
            None,
            &[]
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// DES-L4 PR-②: ONE admitted-roots derivation per posture — the creator's extras under
    /// deliverable-roots, the notes root (or nothing) under read-only, nothing under full — so the
    /// wrapped env, the ACP fence and the gate hook cannot disagree about where a fenced write may go.
    /// wicked-core#881: the QE unit carrying a QE ledger root is deliverable-roots on a bound run
    /// (it runs garden's pipeline — an interpreter — into its own root), read-only unbound; the
    /// same evaluator without a root keeps the read-only posture. Mutation: drop the rule → the
    /// first assert reads ReadOnly.
    #[test]
    fn the_qe_unit_with_a_ledger_root_takes_the_deliverable_roots_posture() {
        let mut qe = unit(PhaseRole::Evaluator, false);
        qe.repo_checks_floor = true;
        assert_eq!(WritePosture::of(&qe, true), WritePosture::ReadOnly);
        qe.qe_ledger_root = Some("/evidence/run/.wicked-qe".into());
        assert_eq!(WritePosture::of(&qe, true), WritePosture::DeliverableRoots);
        assert_eq!(WritePosture::of(&qe, false), WritePosture::ReadOnly);
        // A floor-fix creator cloned from the QE unit is not the QE unit (codex r1 on #889).
        let fixer = crate::cli_runner::floor_fix_unit(
            &qe,
            &crate::repo_checks::FloorFix {
                note: "fix it".into(),
                seat: "claude".into(),
            },
        );
        assert_eq!(fixer.qe_ledger_root, None);
        assert_eq!(WritePosture::of(&fixer, true), WritePosture::Full);
        qe.tool_cmd = Some(vec!["true".to_string()]);
        assert_ne!(
            WritePosture::of(&qe, true),
            WritePosture::DeliverableRoots,
            "a Tool unit is the engine's own command"
        );
    }

    /// wicked-core#881, the live finding of wicked-crew#951 (rig run 46fb2c04): under the QE unit's
    /// posture the fence the claude/ACP carriers share lets garden's pipeline run — `node -e`
    /// and a write into the QE ledger root are admitted — while a write into the tree is still
    /// refused (and the worktree guard still restores one that escapes). Read-only, the same
    /// `node -e` is refused: that refusal stranded the run. Mutation: drop the posture rule → the
    /// node call is refused.
    #[cfg(unix)]
    #[test]
    fn the_qe_unit_fence_runs_the_pipeline_into_its_root_and_still_refuses_the_tree() {
        let base = std::env::temp_dir().join(format!("wicked-881-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let wt = base.join("wt");
        let qe_root = base.join("evidence").join(".wicked-qe");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&qe_root).unwrap();
        let mut qe = unit(PhaseRole::Evaluator, false);
        qe.repo_checks_floor = true;
        qe.qe_ledger_root = Some(qe_root.to_string_lossy().into_owned());
        let posture = WritePosture::of(&qe, true);
        let roots = unit_admitted_roots(posture, &qe, &["/run/author".to_string()]);
        assert_eq!(roots, vec![qe_root.clone()]);
        let bash = |p: WritePosture, cmd: &str| {
            crate::gate_hook::bash_write_phase_scope(false, p, cmd, &wt, None, &roots)
        };
        let node = "node -e \"import('wicked-ledger').then(m => m.createDomainStore())\"";
        assert!(
            bash(posture, node).is_none(),
            "the pipeline's node call runs"
        );
        assert!(
            bash(WritePosture::ReadOnly, node).is_some(),
            "read-only refuses it (what stranded the run)"
        );
        let into_root = format!("echo x > {}", qe_root.join("verdicts.json").display());
        assert!(
            bash(posture, &into_root).is_none(),
            "a write into the QE root is admitted"
        );
        let into_tree = format!("echo x > {}", wt.join("src.js").display());
        assert!(
            bash(posture, &into_tree).is_some(),
            "a write into the tree is refused"
        );
        let write = |p: &std::path::Path| {
            crate::gate_hook::phase_scope_denial(
                false,
                posture,
                &serde_json::json!({ "path": p.to_string_lossy() }),
                "Write",
                &wt,
                None,
                &roots,
            )
        };
        assert!(write(&qe_root.join("config.json")).is_none());
        assert!(write(&wt.join("src.js")).is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// wicked-crew#951: a dispatched read-only unit admits its notes root and THEN its QE ledger
    /// root (the notes root stays first — the refusal names it as where notes go); a creator's
    /// list is its extras, untouched by either; a unit without a QE root admits the notes root
    /// alone. Mutation: drop the `extend` → the first assert loses the QE root.
    #[test]
    fn a_read_only_unit_admits_its_notes_root_then_its_qe_ledger_root() {
        let mut u = unit(PhaseRole::Evaluator, false);
        u.repo_checks_floor = true; // the QE unit (qe_acceptance::is_qe_unit)
        u.notes_root = Some("/notes/u4".into());
        u.qe_ledger_root = Some("/evidence/run/.wicked-qe".into());
        let extras = vec!["/run/author".to_string()];
        assert_eq!(
            unit_admitted_roots(WritePosture::ReadOnly, &u, &extras),
            vec![
                PathBuf::from("/notes/u4"),
                PathBuf::from("/evidence/run/.wicked-qe")
            ]
        );
        assert_eq!(
            unit_admitted_roots(WritePosture::DeliverableRoots, &u, &extras),
            vec![
                PathBuf::from("/notes/u4"),
                PathBuf::from("/evidence/run/.wicked-qe")
            ],
            "the QE unit (deliverable-roots) admits its own roots, never the extras (core#881)"
        );
        let mut creator = unit(PhaseRole::Creator, false);
        creator.notes_root = None;
        assert_eq!(
            unit_admitted_roots(WritePosture::DeliverableRoots, &creator, &extras),
            vec![PathBuf::from("/run/author")],
            "a creator's admitted roots are exactly its extras"
        );
        assert!(unit_admitted_roots(WritePosture::Full, &u, &extras).is_empty());
        u.qe_ledger_root = None;
        assert_eq!(
            unit_admitted_roots(WritePosture::ReadOnly, &u, &extras),
            vec![PathBuf::from("/notes/u4")]
        );
    }

    #[test]
    fn admitted_roots_are_the_extras_for_a_creator_the_notes_root_for_read_only_and_none_for_full()
    {
        let extras = vec!["/run/inbox".to_string(), "/run/out".to_string()];
        assert_eq!(
            admitted_roots(WritePosture::DeliverableRoots, Some("/notes/u2"), &extras),
            vec![PathBuf::from("/run/inbox"), PathBuf::from("/run/out")],
            "a creator's admitted roots are exactly its extras — never the notes root"
        );
        assert_eq!(
            admitted_roots(WritePosture::ReadOnly, Some("/notes/u2"), &extras),
            vec![PathBuf::from("/notes/u2")],
            "an evaluator's admitted root is its notes root — never the extras"
        );
        assert!(
            admitted_roots(WritePosture::ReadOnly, None, &extras).is_empty(),
            "no notes root ⇒ nothing admitted (fail closed)"
        );
        assert!(admitted_roots(WritePosture::Full, Some("/notes/u2"), &extras).is_empty());
        // The env round-trip the wrapped carrier uses carries the read-only list too.
        let env = deliverable_roots_env(&admitted_roots(
            WritePosture::ReadOnly,
            Some("/notes/u2"),
            &[],
        ))
        .unwrap();
        assert_eq!(
            parse_deliverable_roots_env(Some(&env)),
            vec![PathBuf::from("/notes/u2")]
        );
    }

    /// D2 (review round 2 on #628): the PA's review of a member's step gets no write tools —
    /// read-only on a bound or unbound run, whatever the step's own role and `executes_code`.
    #[test]
    fn a_member_step_review_is_read_only_whatever_the_steps_own_posture() {
        let mut u = crate::domain::WorkUnit::pending("u1", "s1", 1, "Build the thing");
        u.role = PhaseRole::Creator;
        u.worktree_guarded = false;
        assert_eq!(
            WritePosture::of(&u, true),
            WritePosture::Full,
            "the member's own turn"
        );
        u.member_step = Some(crate::domain::MemberStepState {
            member: "codex".into(),
            reviewing: Some(0),
            ..Default::default()
        });
        assert_eq!(WritePosture::of(&u, true), WritePosture::ReadOnly);
        assert_eq!(WritePosture::of(&u, false), WritePosture::ReadOnly);
        assert!(WritePosture::of(&u, true).fences_writes());
    }

    /// WT-C1 (DES-walkthrough-proof §4.3, B3): the walkthrough author is an EVALUATOR that writes
    /// exactly one file, its storyline, into the run's declared author dir. Keyed off the catalog
    /// entry, never the prompt: on a BOUND run it gets `DeliverableRoots` (fenced to the declared
    /// roots, the tree excluded, never `Full`); on an unbound run, and for any other evaluator,
    /// the posture stays read-only.
    #[test]
    fn the_walkthrough_author_gets_deliverable_roots_on_a_bound_run_only() {
        let mut author = unit(PhaseRole::Evaluator, false);
        author.catalog = Some("walkthrough_plan".to_string());
        assert_eq!(
            WritePosture::of(&author, true),
            WritePosture::DeliverableRoots
        );
        assert_ne!(WritePosture::of(&author, true), WritePosture::Full);
        assert_eq!(WritePosture::of(&author, false), WritePosture::ReadOnly);
        let mut other = unit(PhaseRole::Evaluator, false);
        other.catalog = Some("test".to_string());
        assert_eq!(WritePosture::of(&other, true), WritePosture::ReadOnly);
        let mut named = unit(PhaseRole::Evaluator, false);
        named.description = "walkthrough_plan: write the storyline".to_string();
        assert_eq!(
            WritePosture::of(&named, true),
            WritePosture::ReadOnly,
            "the prompt never keys the posture"
        );
        // A step that raised `executes_code` (TightenOnly) clears the guard marker; the author
        // keeps its fence anyway (Copilot review on #691).
        let mut raised = unit(PhaseRole::Evaluator, true);
        raised.catalog = Some("walkthrough_plan".to_string());
        assert!(!raised.worktree_guarded);
        assert_eq!(
            WritePosture::of(&raised, true),
            WritePosture::DeliverableRoots
        );
        assert_eq!(WritePosture::of(&raised, false), WritePosture::ReadOnly);
    }
}
