//! WT-C2 (DES-walkthrough-proof §4.2–§4.4): the engine's half of a walkthrough — the run's
//! evidence root, the roots and environment each walkthrough step is handed, and the jail the
//! `walkthrough_review` Tool runs in.
//!
//! ## Roots
//!
//! The launcher mints one EVIDENCE ROOT per repo-bound run (`LaunchSpec::evidence_root`, persisted
//! on the session). Two kinds of directory hang off it:
//!
//! | Path | Written by |
//! |---|---|
//! | `<root>/author/<plan step>/` | the `walkthrough_plan` seat (the launcher lists `<root>/author` in `extra_write_roots`) |
//! | `<root>/<review step>/` — the PROOF ROOT | only the jailed `walkthrough_review` Tool |
//!
//! A step id is used as a path segment, so one that is not a plain name (`[A-Za-z0-9._-]`, not
//! starting with `.`) has no root at all: the step fails closed at its pinned validator, never
//! reaching outside the evidence root.
//!
//! ## What each step is handed
//!
//! - The `walkthrough_plan` unit's pinned validator (the lint) gets `WICKED_EVIDENCE_ROOT` = its
//!   author dir and `WICKED_GARDEN_ROOT` = the skills generation (A10).
//! - The `walkthrough_review` unit's pinned validator (the result check) gets
//!   `WICKED_EVIDENCE_ROOT` = its proof root.
//! - The `walkthrough_review` Tool process gets the validator allowlist, a private temp dir, and
//!   `WICKED_RUN_ID`, `WICKED_RUN_UNIT`, `WICKED_EVIDENCE_ROOT` (its proof root), `WICKED_TREE` (the
//!   worktree's tree id, snapshotted at dispatch OUTSIDE the jail, since computing it writes git
//!   objects), `WICKED_WALKTHROUGH_AUTHOR` (the author dir of the walkthrough_plan step it
//!   records, when the plan has one) and the skills generation's launcher env
//!   (`WICKED_GARDEN_ROOT` + the `PATH` prefix). Nothing else: no key or token reaches the
//!   recorder, the app or the probes.
//!
//! ## The jail
//!
//! The Tool runs under the validator's OS launcher with `NetworkPolicy::LoopbackOnly`: writes only
//! in the proof root and the private temp dir, loopback connect/bind only. A host with no
//! `Sandboxed` launcher (Windows; Linux without bwrap) runs NO walkthrough: the engine writes the
//! proof root's `result.json` as `INCONCLUSIVE` / `unjailed_host` and the pinned result validator
//! denies, saying why (O4).

use std::path::{Path, PathBuf};

use crate::domain::{AgentSession, WorkUnit};

/// The catalog id of the walkthrough author (evaluator, agent).
pub(crate) const PLAN_CATALOG: &str = "walkthrough_plan";
/// The catalog id of the walkthrough recorder (neutral, engine-run Tool).
pub(crate) const REVIEW_CATALOG: &str = "walkthrough_review";
/// The step's root, handed to its validator and to the record Tool.
pub(crate) const EVIDENCE_ROOT_ENV: &str = "WICKED_EVIDENCE_ROOT";
/// The worktree's tree id at the record Tool's dispatch.
pub(crate) const TREE_ENV: &str = "WICKED_TREE";
/// The author dir of the `walkthrough_plan` step the record Tool records.
pub(crate) const AUTHOR_DIR_ENV: &str = "WICKED_WALKTHROUGH_AUTHOR";
/// The subdirectory of the evidence root the walkthrough author writes under.
pub(crate) const AUTHOR_SUBDIR: &str = "author";
/// The record Tool's verdict file, read by the pinned result validator.
pub(crate) const RESULT_FILE: &str = "result.json";
/// `result.json`'s `cause` on a host that cannot jail the recorder.
pub(crate) const UNJAILED_HOST: &str = "unjailed_host";

/// Validate a launch's evidence root by the rules every launch-declared write root obeys
/// (absolute, outside the engine's config/pin tree, `path_policy`). `None` is valid: such a run
/// simply cannot pass a walkthrough.
pub fn validate_evidence_root(root: Option<&str>, home: Option<&Path>) -> Result<(), String> {
    let Some(root) = root else {
        return Ok(());
    };
    if root.trim().is_empty() {
        return Err("evidence_root is empty: omit it, or name an absolute directory".to_string());
    }
    crate::path_policy::validate_extra_write_roots(&[root.to_string()], home)
        .map_err(|e| format!("evidence_root {root:?} is refused: {e}"))
}

/// The unit's step id (the composed phase id: `<session>:<step>`), when it is a plain name that is
/// safe as one path segment. `None` otherwise — the step then has no root.
pub(crate) fn step_id(unit: &WorkUnit) -> Option<&str> {
    let prefix = format!("{}:", unit.session_id);
    let id = unit.id.strip_prefix(&prefix)?;
    let plain = !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    plain.then_some(id)
}

fn is(unit: &WorkUnit, catalog: &str) -> bool {
    unit.catalog.as_deref() == Some(catalog)
}

/// `<root>/author/<step>`.
pub(crate) fn author_dir(evidence_root: &Path, plan_step: &str) -> PathBuf {
    evidence_root.join(AUTHOR_SUBDIR).join(plan_step)
}

/// The step's own root: the author dir for a `walkthrough_plan` unit, the proof root for a
/// `walkthrough_review` unit, `None` for any other unit, a run with no evidence root, or a step id
/// that is not a plain name.
pub(crate) fn step_root(session: &AgentSession, unit: &WorkUnit) -> Option<PathBuf> {
    let root = Path::new(session.evidence_root.as_deref()?);
    let step = step_id(unit)?;
    if is(unit, PLAN_CATALOG) {
        Some(author_dir(root, step))
    } else if is(unit, REVIEW_CATALOG) {
        // `author` is the author subtree's name; a review step spelled that way would record into
        // the authors' write root.
        // Case-insensitively: on a case-insensitive filesystem (macOS by default) `AUTHOR` is the
        // same directory (codex review).
        (!step.eq_ignore_ascii_case(AUTHOR_SUBDIR)).then(|| root.join(step))
    } else {
        None
    }
}

/// The variables a walkthrough unit's PINNED VALIDATOR is handed on top of the cleared
/// environment (§4.3 B1): `WICKED_EVIDENCE_ROOT` = the step's root for both catalog ids, plus
/// `WICKED_GARDEN_ROOT` = `garden_root` for the author's lint. Empty for every other unit, and when
/// the step has no root — the scripts' own `test -n` then denies.
pub(crate) fn validator_env(
    session: &AgentSession,
    unit: &WorkUnit,
    garden_root: Option<&Path>,
) -> Vec<(String, String)> {
    let mut env = Vec::new();
    let Some(root) = step_root(session, unit) else {
        return env;
    };
    env.push((
        EVIDENCE_ROOT_ENV.to_string(),
        root.to_string_lossy().into_owned(),
    ));
    if is(unit, PLAN_CATALOG) {
        if let Some(g) = garden_root {
            env.push((
                crate::skills_snapshot::GARDEN_ROOT_ENV.to_string(),
                g.to_string_lossy().into_owned(),
            ));
        }
    }
    env
}

/// The skills generation's root for a walkthrough validator (A10), resolved the way the workers'
/// ladder resolves it. `None` when no generation resolves — the lint then denies on its `test -n`.
pub(crate) fn garden_root_for(unit: &WorkUnit) -> Option<PathBuf> {
    if !is(unit, PLAN_CATALOG) {
        return None;
    }
    match crate::skills_snapshot::resolve_ladder() {
        Ok(crate::skills_snapshot::Ladder::Root(s)) => Some(s.root),
        _ => None,
    }
}

/// The `walkthrough_plan` step a `walkthrough_review` unit records: the one its `depends_on` names,
/// else the nearest one before it. `None` when the plan has none (the tool then refuses the
/// storyline as missing).
pub(crate) fn author_step_for<'a>(review: &WorkUnit, units: &'a [WorkUnit]) -> Option<&'a str> {
    let plans = || {
        units
            .iter()
            .filter(|u| is(u, PLAN_CATALOG) && u.ord < review.ord)
    };
    let named =
        plans().find(|u| step_id(u).is_some_and(|s| review.depends_on.iter().any(|d| d == s)));
    named
        .or_else(|| plans().max_by_key(|u| u.ord))
        .and_then(step_id)
}

/// Everything the actor resolves (on its own thread, with the store) for one `walkthrough_review`
/// dispatch. The worker thread adds the skills generation's launcher env and runs the jail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordLaunch {
    /// The proof root, or why there is none.
    pub(crate) proof_root: Result<PathBuf, String>,
    /// The declared variables, in a fixed order (see the module docs).
    pub(crate) env: Vec<(String, String)>,
}

/// Resolve a `walkthrough_review` dispatch. `None` for every other unit. `tree` is the worktree's
/// tree id snapshotted at dispatch (`Err` = why it could not be taken).
pub(crate) fn record_launch(
    run_id: &str,
    session: &AgentSession,
    unit: &WorkUnit,
    units: &[WorkUnit],
    tree: Result<String, String>,
) -> Option<RecordLaunch> {
    if !is(unit, REVIEW_CATALOG) {
        return None;
    }
    let proof_root = if session.workdir.is_none() {
        Err("the run has no worktree (a repo-less run records no walkthrough)".to_string())
    } else if session.evidence_root.is_none() {
        Err("the launcher minted no evidence root for this run".to_string())
    } else {
        step_root(session, unit).ok_or_else(|| {
            format!(
                "the step id of unit {} is not a plain name, so it has no proof root",
                unit.ord
            )
        })
    };
    let proof_root = match (proof_root, tree.as_ref()) {
        (Ok(_), Err(why)) => Err(format!(
            "the worktree's tree could not be snapshotted at dispatch: {why}"
        )),
        (other, _) => other,
    };
    let mut env = vec![
        ("WICKED_RUN_ID".to_string(), run_id.to_string()),
        ("WICKED_RUN_UNIT".to_string(), unit.ord.to_string()),
    ];
    if let (Ok(root), Ok(tree)) = (&proof_root, &tree) {
        env.push((
            EVIDENCE_ROOT_ENV.to_string(),
            root.to_string_lossy().into_owned(),
        ));
        env.push((TREE_ENV.to_string(), tree.clone()));
        if let (Some(evidence), Some(step)) = (
            session.evidence_root.as_deref(),
            author_step_for(unit, units),
        ) {
            env.push((
                AUTHOR_DIR_ENV.to_string(),
                author_dir(Path::new(evidence), step)
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
    }
    Some(RecordLaunch { proof_root, env })
}

/// Check, at use time, that a proof root is a real directory directly under its evidence root —
/// not a symlink, and resolving to exactly `<canonical parent>/<name>` — so a link planted at
/// `<evidence_root>/<step>` cannot aim the jail's writable root, the unjailed-host result or the
/// previous take's retirement outside the evidence root (codex review). What a same-uid process
/// could still do between this check and the jail arming is the honest limit of §4.6.
pub(crate) fn check_proof_root(proof_root: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(proof_root)
        .map_err(|e| format!("the proof root {} is unreadable: {e}", proof_root.display()))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(format!(
            "the proof root {} is not a plain directory (a link or a file was planted there)",
            proof_root.display()
        ));
    }
    let (Some(parent), Some(name)) = (proof_root.parent(), proof_root.file_name()) else {
        return Err(format!(
            "the proof root {} has no parent",
            proof_root.display()
        ));
    };
    let want = parent
        .canonicalize()
        .map_err(|e| format!("the evidence root {} is unreadable: {e}", parent.display()))?
        .join(name);
    let got = proof_root
        .canonicalize()
        .map_err(|e| format!("the proof root {} is unreadable: {e}", proof_root.display()))?;
    if got != want {
        return Err(format!(
            "the proof root {} resolves to {}, outside its evidence root",
            proof_root.display(),
            got.display()
        ));
    }
    Ok(())
}

/// The `result.json` the engine writes when it cannot run the recorder (§4.4 O4): every chapter
/// INCONCLUSIVE, with the cause and a sentence the operator can act on.
pub(crate) fn inconclusive_result(cause: &str, reason: &str) -> String {
    serde_json::json!({
        "overall": "INCONCLUSIVE",
        "cause": cause,
        "reason": reason,
        "chapters": [],
    })
    .to_string()
}

/// Before a record attempt, move the previous attempt's `result.json` aside
/// (`result.before-attempt-<n>.json`), so the pinned validator can only ever read THIS attempt's
/// verdict — a refused or crashed re-take must not pass on the last take's PASS. The take's own
/// evidence (segments, vault, ledger) is the tool's and is left as it is.
pub(crate) fn retire_previous_result(proof_root: &Path, attempt: u32) -> std::io::Result<()> {
    let current = proof_root.join(RESULT_FILE);
    match std::fs::symlink_metadata(&current) {
        Ok(_) => std::fs::rename(
            &current,
            proof_root.join(format!("result.before-attempt-{attempt}.json")),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(evidence: Option<&str>, workdir: Option<&str>) -> AgentSession {
        let mut s: AgentSession = serde_json::from_value(serde_json::json!({
            "id": "run-w", "workflow_id": "wf-run-w", "problem": "p",
            "entity_mode": "shared", "clis": [], "status": "executing"
        }))
        .expect("a minimal session");
        s.evidence_root = evidence.map(str::to_string);
        s.workdir = workdir.map(str::to_string);
        s
    }

    fn unit(step: &str, ord: u32, catalog: Option<&str>) -> WorkUnit {
        let mut u = WorkUnit::pending(format!("run-w:{step}"), "run-w", ord, "d".to_string());
        u.catalog = catalog.map(str::to_string);
        u
    }

    fn ev() -> String {
        std::env::temp_dir()
            .join("wt-c2-evidence")
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn a_step_id_is_a_path_segment_only_when_it_is_a_plain_name() {
        assert_eq!(
            step_id(&unit("walkthrough_review", 1, None)),
            Some("walkthrough_review")
        );
        assert_eq!(step_id(&unit("wr-2.b", 1, None)), Some("wr-2.b"));
        for bad in ["../x", "a/b", ".hidden", "", "a b", "a\\b"] {
            assert_eq!(
                step_id(&unit(bad, 1, None)),
                None,
                "{bad:?} must have no root"
            );
        }
    }

    #[test]
    fn each_walkthrough_step_gets_its_own_root_and_no_other_unit_gets_one() {
        let ev = ev();
        let s = session(Some(&ev), Some("/wt"));
        let plan = unit("wp", 1, Some(PLAN_CATALOG));
        let review = unit("wr", 2, Some(REVIEW_CATALOG));
        let build = unit("b", 0, Some("build"));
        assert_eq!(
            step_root(&s, &plan),
            Some(Path::new(&ev).join("author").join("wp"))
        );
        assert_eq!(step_root(&s, &review), Some(Path::new(&ev).join("wr")));
        assert_eq!(step_root(&s, &build), None);
        // A review step named like the author subtree would record into the authors' root.
        assert_eq!(
            step_root(&s, &unit("author", 2, Some(REVIEW_CATALOG))),
            None
        );
        // No evidence root: no root for anyone.
        assert_eq!(step_root(&session(None, Some("/wt")), &review), None);
    }

    #[test]
    fn the_validators_are_handed_the_step_root_and_the_lint_the_garden_root() {
        let ev = ev();
        let s = session(Some(&ev), Some("/wt"));
        let garden = Path::new("/garden/gen-7");
        let plan = unit("wp", 1, Some(PLAN_CATALOG));
        let review = unit("wr", 2, Some(REVIEW_CATALOG));
        let author = Path::new(&ev).join("author").join("wp");
        assert_eq!(
            validator_env(&s, &plan, Some(garden)),
            vec![
                (
                    "WICKED_EVIDENCE_ROOT".to_string(),
                    author.to_string_lossy().into_owned()
                ),
                (
                    "WICKED_GARDEN_ROOT".to_string(),
                    "/garden/gen-7".to_string()
                ),
            ]
        );
        assert_eq!(
            validator_env(&s, &review, Some(garden)),
            vec![(
                "WICKED_EVIDENCE_ROOT".to_string(),
                Path::new(&ev).join("wr").to_string_lossy().into_owned()
            )]
        );
        assert!(validator_env(&s, &unit("b", 0, Some("build")), Some(garden)).is_empty());
        assert!(validator_env(&session(None, Some("/wt")), &review, Some(garden)).is_empty());
    }

    #[test]
    fn the_record_tool_is_handed_exactly_the_declared_variables() {
        let ev = ev();
        let s = session(Some(&ev), Some("/wt"));
        let units = vec![
            unit("b", 1, Some("build")),
            unit("wp", 2, Some(PLAN_CATALOG)),
            unit("wr", 3, Some(REVIEW_CATALOG)),
        ];
        let got = record_launch("run-w", &s, &units[2], &units, Ok("t123".into())).unwrap();
        assert_eq!(got.proof_root, Ok(Path::new(&ev).join("wr")));
        let keys: Vec<&str> = got.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "WICKED_RUN_ID",
                "WICKED_RUN_UNIT",
                "WICKED_EVIDENCE_ROOT",
                "WICKED_TREE",
                "WICKED_WALKTHROUGH_AUTHOR"
            ]
        );
        assert_eq!(got.env[1].1, "3");
        assert_eq!(got.env[3].1, "t123");
        assert_eq!(
            got.env[4].1,
            Path::new(&ev).join("author").join("wp").to_string_lossy()
        );
        // Not a walkthrough_review unit: nothing.
        assert!(record_launch("run-w", &s, &units[1], &units, Ok("t".into())).is_none());
    }

    #[test]
    fn the_author_is_the_plan_step_the_review_depends_on_else_the_nearest_before_it() {
        let units = vec![
            unit("wp1", 1, Some(PLAN_CATALOG)),
            unit("wp2", 2, Some(PLAN_CATALOG)),
            unit("wr", 3, Some(REVIEW_CATALOG)),
            unit("wp3", 4, Some(PLAN_CATALOG)),
        ];
        assert_eq!(author_step_for(&units[2], &units), Some("wp2"));
        let mut named = units[2].clone();
        named.depends_on = vec!["wp1".into()];
        assert_eq!(author_step_for(&named, &units), Some("wp1"));
        // A plan step AFTER the review is never its author.
        let lone = vec![
            unit("wr", 1, Some(REVIEW_CATALOG)),
            unit("wp", 2, Some(PLAN_CATALOG)),
        ];
        assert_eq!(author_step_for(&lone[0], &lone), None);
    }

    #[test]
    fn a_review_with_no_worktree_root_or_tree_has_no_proof_root_and_says_why() {
        let ev = ev();
        let units = vec![unit("wr", 1, Some(REVIEW_CATALOG))];
        let no_wt = record_launch(
            "r",
            &session(Some(&ev), None),
            &units[0],
            &units,
            Ok("t".into()),
        )
        .unwrap();
        assert!(no_wt.proof_root.unwrap_err().contains("no worktree"));
        let no_root = record_launch(
            "r",
            &session(None, Some("/wt")),
            &units[0],
            &units,
            Ok("t".into()),
        )
        .unwrap();
        assert!(no_root.proof_root.unwrap_err().contains("no evidence root"));
        let no_tree = record_launch(
            "r",
            &session(Some(&ev), Some("/wt")),
            &units[0],
            &units,
            Err("git failed".into()),
        )
        .unwrap();
        assert!(no_tree.proof_root.unwrap_err().contains("git failed"));
        // Without a proof root only the run identity is handed over.
        assert_eq!(no_tree.env.len(), 2);
    }

    #[test]
    fn an_evidence_root_is_judged_like_a_write_root() {
        let home_dir = std::env::temp_dir().join("wt-c2-home");
        let home = Some(home_dir.as_path());
        assert!(validate_evidence_root(None, home).is_ok());
        assert!(validate_evidence_root(Some("  "), home).is_err());
        assert!(validate_evidence_root(Some("relative/dir"), home)
            .unwrap_err()
            .contains("evidence_root"));
        let abs = std::env::temp_dir().join("wt-c2-ok");
        assert!(validate_evidence_root(Some(abs.to_str().unwrap()), home).is_ok());
        // Inside the engine's config/pin tree: refused, as a write root there would be.
        let pins = home_dir.join(".config").join("wicked-core").join("x");
        assert!(validate_evidence_root(Some(pins.to_str().unwrap()), home).is_err());
    }

    #[test]
    fn a_previous_result_is_moved_aside_before_a_new_take() {
        let dir = std::env::temp_dir().join(format!("wt-c2-retire-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        retire_previous_result(&dir, 1).unwrap(); // nothing there: fine
        std::fs::write(dir.join(RESULT_FILE), r#"{"overall":"PASS"}"#).unwrap();
        retire_previous_result(&dir, 2).unwrap();
        assert!(!dir.join(RESULT_FILE).exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("result.before-attempt-2.json")).unwrap(),
            r#"{"overall":"PASS"}"#
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_review_step_spelled_like_the_author_subtree_in_any_case_has_no_root() {
        let ev = ev();
        let s = session(Some(&ev), Some("/wt"));
        for name in ["author", "AUTHOR", "Author"] {
            assert_eq!(
                step_root(&s, &unit(name, 2, Some(REVIEW_CATALOG))),
                None,
                "{name}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_link_at_the_proof_root_is_refused() {
        let dir = std::env::temp_dir().join(format!("wt-c2-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let evidence = dir.join("evidence");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&evidence).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(evidence.join("ok")).unwrap();
        assert_eq!(check_proof_root(&evidence.join("ok")), Ok(()));
        std::os::unix::fs::symlink(&outside, evidence.join("wr")).unwrap();
        assert!(check_proof_root(&evidence.join("wr"))
            .unwrap_err()
            .contains("not a plain directory"));
        std::fs::write(evidence.join("file"), "x").unwrap();
        assert!(check_proof_root(&evidence.join("file")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_unjailed_result_is_inconclusive_with_its_cause() {
        let v: serde_json::Value =
            serde_json::from_str(&inconclusive_result(UNJAILED_HOST, "no jail")).unwrap();
        assert_eq!(v["overall"], "INCONCLUSIVE");
        assert_eq!(v["cause"], "unjailed_host");
    }
}
