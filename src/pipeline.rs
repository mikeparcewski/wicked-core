//! PIPELINE — the full governed session: plan → distribute → execute → evidence, on ONE shared
//! store. Ported into COE from the retired wicked-agent's `run_session`, with one change: instead of
//! firing the (aspirational) bus catalog, it emits [`CoreEvent`]s through a callback so the actor
//! fans them to live subscribers as the work happens.
//!
//! Runs on the actor thread (the single writer). Stub execute path (deterministic, no subprocess);
//! the wrapped-CLI path is a later phase.

use std::sync::Arc;

use wicked_apps_core::ToNode;
use wicked_council::types::Dispatcher;
use wicked_council::AgenticCli;

use crate::domain::{put_node, AgentSession, SessionStatus, UnitStatus};
use crate::event::CoreEvent;
use crate::execute::{self, UnitOutcome};
use crate::scope::{resolve_scope, EntityMode};
use crate::{distribute, plan};

/// The result of a completed session run.
#[derive(Debug, Clone)]
pub struct SessionResult {
    pub session_id: String,
    pub workflow_id: String,
    pub entity_mode: EntityMode,
    pub collection_scope: Option<String>,
    pub units: Vec<UnitOutcome>,
    pub approved: usize,
    pub rejected: usize,
}

/// Run a full governed session SYNCHRONOUSLY, emitting [`CoreEvent`]s as it progresses. Everything
/// persists on the ONE `store`: the session node, each work-unit node, phase nodes, conformance
/// claims, and each approved unit's work-output node. This is the straight-through driver (used by
/// the operator CLI + tests); the actor's interactive engine reuses the same [`plan_and_distribute`]
/// + [`apply_and_finish_unit`] steps off-thread.
#[allow(clippy::too_many_arguments)]
pub fn run_session(
    store: &mut wicked_apps_core::SqliteStore,
    clis: Vec<AgenticCli>,
    problem: &str,
    entity_mode: EntityMode,
    session_id: &str,
    workflow: Option<&str>,
    dispatcher: Arc<dyn Dispatcher + Send + Sync>,
    emit: &mut dyn FnMut(CoreEvent),
) -> anyhow::Result<SessionResult> {
    let Planned {
        mut session,
        mut units,
        workflow_id,
        cli_keys,
    } = plan_and_distribute(
        store,
        &clis,
        problem,
        entity_mode,
        session_id,
        crate::domain::HumanConfirm::None, // sync path runs straight through (no interactive gates)
        None,                              // sync path has no registered repo
        None,
        workflow,
        &dispatcher,
        emit,
    )?;

    // ── EXECUTE — per unit: produce output (stub, inline here), then gate it. ──
    let mut outcomes: Vec<UnitOutcome> = Vec::with_capacity(units.len());
    for u in &mut units {
        emit(CoreEvent::UnitExecuting {
            session: session_id.to_string(),
            ord: u.ord,
        });
        let output = format!("stub-output for {}", u.description);
        let outcome = apply_and_finish_unit(
            store,
            u,
            &output,
            &workflow_id,
            entity_mode,
            session_id,
            &cli_keys,
            emit,
        )?;
        outcomes.push(outcome);
    }

    // ── complete. ──
    session.status = SessionStatus::Completed;
    session.unit_ix = units.len();
    put_node(store, session.to_node())?;
    emit(CoreEvent::SessionCompleted {
        session: session_id.to_string(),
    });

    let approved = outcomes.iter().filter(|o| o.approved).count();
    let rejected = outcomes.len() - approved;
    Ok(SessionResult {
        session_id: session_id.to_string(),
        workflow_id,
        entity_mode,
        collection_scope: session.collection_scope.clone(),
        units: outcomes,
        approved,
        rejected,
    })
}

/// The result of [`plan_and_distribute`] — a session persisted at `Executing`, its ordered units
/// `Distributed` (each with an assigned CLI), and the registered workflow id + roster.
pub(crate) struct Planned {
    pub session: AgentSession,
    pub units: Vec<crate::domain::WorkUnit>,
    pub workflow_id: String,
    pub cli_keys: Vec<String>,
}

/// Resolve a selected workflow id to its validated [`WorkflowDef`]. Seeds the built-ins and overlays
/// operator drop-in files (`$WICKED_WORKFLOWS_DIR`, else `$HOME/.config/wicked-core/workflows`,
/// best-effort). `None` (no selection) ⇒ `Ok(None)` and the caller uses the free-text planner; a
/// requested-but-**unknown** id ⇒ `Err` (never a silent fallback).
fn resolve_workflow_def(
    workflow: Option<&str>,
) -> anyhow::Result<Option<crate::workflow::WorkflowDef>> {
    // No selection ⇒ the caller uses the free-text planner. Only THIS falls through.
    let Some(id) = workflow else {
        return Ok(None);
    };
    let mut reg = crate::workflow::WorkflowRegistry::with_defaults();
    if let Some(dir) = workflow_overlay_dir() {
        // Best-effort dir read: a broken *overlay dir* must never wedge a built-in run (load_dir
        // itself already skips individual bad files). Warn, don't fail.
        if let Err(e) = reg.load_dir(&dir) {
            eprintln!(
                "wicked-core: workflow overlay {} failed to load ({e}); using built-ins only",
                dir.display()
            );
        }
    }
    // A REQUESTED-but-unknown id is a loud error — never a silent fallback to the prose planner (a
    // `--workflow feaure` typo must not quietly produce a different plan than `--workflow feature`).
    match reg.get(id) {
        Some(def) => Ok(Some(def.clone())),
        None => anyhow::bail!(
            "unknown workflow `{id}` — known workflows: {}",
            reg.ids().join(", ")
        ),
    }
}

fn workflow_overlay_dir() -> Option<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("WICKED_WORKFLOWS_DIR") {
        return Some(std::path::PathBuf::from(d));
    }
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config/wicked-core/workflows"))
}

/// PLAN + DISTRIBUTE (shared by both drivers): persist the session, decompose the problem into
/// units, register the workflow's ordered phase list, and let the council assign a CLI per unit.
/// Emits `SessionStarted` / `UnitPlanned×n` / `UnitDistributed×n` and leaves the session at
/// `Executing`. Store-writing, so it runs on the actor (single-writer) thread.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_and_distribute(
    store: &mut wicked_apps_core::SqliteStore,
    clis: &[AgenticCli],
    problem: &str,
    entity_mode: EntityMode,
    session_id: &str,
    human_confirm: crate::domain::HumanConfirm,
    repo_ref: Option<String>,
    workdir: Option<String>,
    workflow: Option<&str>,
    dispatcher: &Arc<dyn Dispatcher + Send + Sync>,
    emit: &mut dyn FnMut(CoreEvent),
) -> anyhow::Result<Planned> {
    let workflow_id = format!("wf-{session_id}");
    let cli_keys: Vec<String> = clis.iter().map(|c| c.key.clone()).collect();

    // DATA-DRIVEN planning when a workflow is selected (Law 2): units come from the def's phases, each
    // unit's stage from the phase's declared `kind`. No selection ⇒ the legacy free-text planner; a
    // requested-but-unknown id already errored in resolve_workflow_def above.
    // The def is validated (the registry only hands out validated defs), so plan_from_def's unit-id
    // uniqueness precondition holds. The DENY_PHASE_SPAN governed-limit check below applies to BOTH
    // paths — a def with too many phases is rejected exactly like an over-long prose problem.
    let selected_def = resolve_workflow_def(workflow)?;
    let mut units = match &selected_def {
        Some(def) => plan::plan_from_def(def, problem, session_id),
        None => plan::plan_units(problem, session_id),
    };
    if units.len() as u32 > crate::actor::DENY_PHASE_SPAN {
        anyhow::bail!(
            "run has {} units, exceeding the {}-unit governed limit; split the problem into smaller runs",
            units.len(),
            crate::actor::DENY_PHASE_SPAN
        );
    }

    let collection_scope = match entity_mode {
        EntityMode::Shared => Some(resolve_scope(entity_mode, session_id, "shared")),
        EntityMode::Isolated => None,
    };

    let mut session = AgentSession {
        id: session_id.to_string(),
        workflow_id: workflow_id.clone(),
        problem: problem.to_string(),
        entity_mode,
        collection_scope,
        clis: cli_keys.clone(),
        status: SessionStatus::Planning,
        human_confirm,
        unit_ix: 0,
        attempt: 0,
        workdir,
        repo_ref,
    };
    put_node(store, session.to_node())?;
    emit(CoreEvent::SessionStarted {
        session: session_id.to_string(),
        problem: problem.to_string(),
    });

    for u in &units {
        put_node(store, u.to_node())?;
        emit(CoreEvent::UnitPlanned {
            session: session_id.to_string(),
            ord: u.ord,
            description: u.description.clone(),
        });
    }
    session.status = SessionStatus::Distributing;
    put_node(store, session.to_node())?;

    // Register the workflow (ordered phase list + cursor) after planning.
    let phase_specs: Vec<(String, String)> = units
        .iter()
        .map(|u| {
            (
                format!("{workflow_id}:unit-{}", u.ord),
                u.description.clone(),
            )
        })
        .collect();
    wicked_orchestration::register_workflow(store, &workflow_id, problem, &phase_specs)?;

    let distributions =
        distribute::distribute_units_on(&units, clis, session_id, None, dispatcher)?;
    for (u, dist) in units.iter_mut().zip(distributions.iter()) {
        u.assigned_cli = Some(dist.assigned_cli.clone());
        u.assigned_invocation = dist.assigned_invocation.clone();
        u.council_task_ref = dist.council_task_ref.clone();
        u.routing = Some(dist.routing.clone());
        u.status = UnitStatus::Distributed;
        put_node(store, u.to_node())?;
        emit(CoreEvent::UnitDistributed {
            session: session_id.to_string(),
            ord: u.ord,
            cli: dist.assigned_cli.clone(),
        });
    }

    session.status = SessionStatus::Executing;
    put_node(store, session.to_node())?;

    Ok(Planned {
        session,
        units,
        workflow_id,
        cli_keys,
    })
}

/// Apply one unit's produced `output` (shared by both drivers): run the governance gate (creator
/// pass) + the evaluator≠creator second pass, tick the workflow cursor, persist the unit's resolved
/// status, and emit `GateDecided` + `UnitDone`/`UnitDenied`. The caller emits `UnitExecuting` BEFORE
/// the work runs. Store-writing, so it runs on the actor (single-writer) thread.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_and_finish_unit(
    store: &mut wicked_apps_core::SqliteStore,
    unit: &mut crate::domain::WorkUnit,
    output: &str,
    workflow_id: &str,
    entity_mode: EntityMode,
    session_id: &str,
    cli_keys: &[String],
    emit: &mut dyn FnMut(CoreEvent),
) -> anyhow::Result<UnitOutcome> {
    let mut outcome =
        execute::apply_unit(store, unit, output, workflow_id, entity_mode, session_id)?;

    // evaluator≠creator: a second governance pass on approved units, distinct evaluator identity.
    if outcome.approved {
        let evaluator_cli = next_cli_in_roster(&outcome.assigned_cli, cli_keys);
        let eval_at = execute::EVAL_AT_BASE + unit.ord as i64 + 1_000_000;
        if let Ok(eval) = execute::evaluate_unit(
            store,
            unit,
            output,
            &evaluator_cli,
            &outcome.collection_scope,
            &format!("unit-{}", unit.ord),
            eval_at,
        ) {
            outcome.evaluator_claim_id = Some(eval.claim_id);
        }
    }

    // PINNED VALIDATOR — the rev0.4 deterministic gate re-verify (layer-1), deny-dominates. If the unit
    // carries an APPROVED validator and the run has a worktree, re-verify it against that worktree; a
    // FAIL flips the verdict to denied BEFORE the status is persisted. Pure re-verify — no LLM here, so
    // it is safe on the single-writer actor thread. (The agent-validator half is an LLM and belongs on
    // the off-thread worker path — a later slice.)
    if outcome.approved {
        let workdir = crate::domain::get_session(store, session_id)?.and_then(|s| s.workdir);
        if let Some(reason) =
            pinned_validator_denial(unit, workdir.as_deref().map(std::path::Path::new))
        {
            outcome.approved = false;
            outcome.denial_reason = Some(reason);
        }
    }

    wicked_orchestration::tick_workflow(store, workflow_id, outcome.approved)?;

    unit.phase_ref = Some(outcome.phase_id.clone());
    unit.conformance_ref = outcome.claim_id.clone();
    unit.phase_status = Some(outcome.phase_status.clone());
    unit.collection_scope = Some(outcome.collection_scope.clone());
    unit.denial_reason = outcome.denial_reason.clone();
    unit.status = if outcome.approved {
        UnitStatus::Done
    } else {
        UnitStatus::Rejected
    };
    put_node(store, unit.to_node())?;

    emit(CoreEvent::GateDecided {
        session: session_id.to_string(),
        ord: unit.ord,
        allow: outcome.approved,
    });
    emit(if outcome.approved {
        CoreEvent::UnitDone {
            session: session_id.to_string(),
            ord: unit.ord,
        }
    } else {
        CoreEvent::UnitDenied {
            session: session_id.to_string(),
            ord: unit.ord,
        }
    });
    Ok(outcome)
}

/// Re-verify a unit's APPROVED pinned validator against the worktree (rev0.4 gate layer-1). Returns
/// `Some(denial_reason)` when the validator fails or errors — the gate then denies the unit
/// (deny-dominates). `None` means "no denial": no validator, no worktree (an artifact-based validator
/// needs one; repo-less runs skip it for now), or the validator PASSED. Pure + actor-safe (no LLM).
fn pinned_validator_denial(
    unit: &crate::domain::WorkUnit,
    workdir: Option<&std::path::Path>,
) -> Option<String> {
    let v = unit.validator.as_ref()?;
    let cwd = workdir?;
    match crate::validator::run_validator(v, cwd) {
        Ok(true) => None,
        Ok(false) => Some(format!("pinned validator failed: {}", v.criterion)),
        Err(e) => Some(format!("pinned validator error: {e}")),
    }
}

/// A deterministic short id from parts (sha256 prefix).
pub fn deterministic_id(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(parts.join("|").as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

/// The roster seat AFTER `creator` (wrapping), used as the distinct evaluator identity.
fn next_cli_in_roster(creator: &str, roster: &[String]) -> String {
    match roster.iter().position(|k| k == creator) {
        Some(i) => roster
            .get(i + 1)
            .or_else(|| roster.first())
            .filter(|k| k.as_str() != creator)
            .cloned()
            .unwrap_or_else(|| "wicked-evaluator".to_string()),
        None => roster
            .first()
            .cloned()
            .unwrap_or_else(|| "wicked-evaluator".to_string()),
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    #[test]
    fn workflow_selection_resolves_none_known_and_rejects_unknown() {
        // No selection ⇒ None (the caller uses the free-text planner).
        assert!(resolve_workflow_def(None).unwrap().is_none());
        // A known built-in resolves to its def.
        assert_eq!(
            resolve_workflow_def(Some("feature")).unwrap().unwrap().id,
            "feature"
        );
        // A requested-but-unknown id is a LOUD error (never a silent fall-through to prose planning).
        let err = resolve_workflow_def(Some("feaure-typo-xyz"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unknown workflow") && err.contains("feaure-typo-xyz"),
            "error must name the bad id: {err}"
        );
    }

    #[test]
    fn pinned_validator_denial_is_deny_dominates_and_fail_closed() {
        use crate::domain::WorkUnit;
        use crate::validator::DeterministicValidator;
        let dir = std::env::temp_dir().join(format!("wicked-pinned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ok.txt"), "hi").unwrap();
        let mk = |script: &str, approved: bool| DeterministicValidator {
            criterion: "c".into(),
            script: script.into(),
            approved,
        };
        let mut unit = WorkUnit::pending("s:u1", "s", 1, "d");

        // No validator ⇒ no denial.
        assert!(pinned_validator_denial(&unit, Some(&dir)).is_none());
        // Approved + PASSES ⇒ no denial.
        unit.validator = Some(mk("test -f ok.txt", true));
        assert!(pinned_validator_denial(&unit, Some(&dir)).is_none());
        // Approved + FAILS ⇒ denial (deny-dominates).
        unit.validator = Some(mk("test -f missing.txt", true));
        assert!(pinned_validator_denial(&unit, Some(&dir)).is_some());
        // No worktree ⇒ skip (an artifact validator needs one) ⇒ no denial.
        assert!(pinned_validator_denial(&unit, None).is_none());
        // UNAPPROVED ⇒ run_validator refuses ⇒ denial (fail-closed).
        unit.validator = Some(mk("test -f ok.txt", false));
        assert!(pinned_validator_denial(&unit, Some(&dir)).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
