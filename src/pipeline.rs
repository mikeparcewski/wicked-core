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
    let mut denied_ord: Option<u32> = None;
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
            None, // sync straight-through path runs no off-thread agent judge (stub work, no LLM)
            emit,
        )?;
        let approved = outcome.approved;
        let ord = u.ord;
        outcomes.push(outcome);
        // RUN-LEVEL DENY CONTRACT (seam finding #1): the SYNC driver must NOT complete past a rejection.
        // A governance/validator/evaluator DENY halts the session as `Failed` here — mirroring the
        // interactive lane's `fail_run` and domain.rs's contract ("a Completed run means EVERY unit was
        // approved"). Stop at the first denied unit; do not run or gate any unit after it.
        if !approved {
            denied_ord = Some(ord);
            break;
        }
    }

    // ── finalize: Completed iff every unit approved; else Failed at the denied unit (finding #1). ──
    session.unit_ix = outcomes.len();
    if let Some(ord) = denied_ord {
        session.status = SessionStatus::Failed;
        put_node(store, session.to_node())?;
        emit(CoreEvent::SessionFailed {
            session: session_id.to_string(),
            ord,
        });
    } else {
        session.status = SessionStatus::Completed;
        put_node(store, session.to_node())?;
        emit(CoreEvent::SessionCompleted {
            session: session_id.to_string(),
        });
    }

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

/// Attach each phase's PINNED, already-approved validator to its unit (the producer half that makes the
/// rev0.4 dual-validator gate ENGAGE). Units are 1:1 with `def.phases` in declaration order, so this zips
/// them and, for every phase that declares a `validator_pin`, LOADS that validator from the vault (a pure
/// store read — no LLM, so actor-safe) and pins it onto `unit.validator`. A pin that does NOT resolve in
/// the vault is FAIL-CLOSED: a phase pinning a validator that isn't vaulted is a misconfiguration, so the
/// run BAILS rather than silently executing an ungated phase. A phase with no `validator_pin` leaves the
/// unit's validator `None` (the pre-gate, ungated behavior).
fn attach_pinned_validators(
    store: &wicked_apps_core::SqliteStore,
    units: &mut [crate::domain::WorkUnit],
    def: &crate::workflow::WorkflowDef,
) -> anyhow::Result<()> {
    // NOTE (docs wave): the shipped feature/bug/migration defs all carry `validator_pin = null`, so
    // this loop is a no-op for them and the dual-validator gate stays INERT until an operator provisions
    // + pins a validator. The author→approve→vault→pin path is exposed as `wicked-core
    // provision-validator --criterion "..."` then `wicked-core approve-validator --pin <pin>`; the
    // approved pin goes into a workflow def's `validator_pin`. (Documented in the docs wave.)
    for (unit, phase) in units.iter_mut().zip(def.phases.iter()) {
        let Some(pin) = phase.validator_pin.as_deref() else {
            continue;
        };
        match crate::validator_vault::load_validator(store, pin)? {
            // The pin must resolve to an APPROVED validator (Lane D finding 2). An UNAPPROVED-but-vaulted
            // pin would attach and then DENY EVERY run at gate time (run_validator fails closed on an
            // unapproved validator) — a persistent DoS surfaced only as a late, misleading gate error.
            // Catch it at PLAN time with a message naming the phase + pin and pointing at approval.
            Some(validator) if validator.approved => unit.validator = Some(validator),
            Some(_) => anyhow::bail!(
                "workflow `{}` phase `{}` pins an UNAPPROVED validator `{pin}` — approve it via \
                 approve_and_store (`wicked-core approve-validator --pin {pin}`) and pin the APPROVED \
                 pin instead; refusing to run (an unapproved pin denies every run)",
                def.id,
                phase.id
            ),
            None => anyhow::bail!(
                "workflow `{}` phase `{}` pins validator `{pin}`, which is not in the vault \
                 (author + approve it out of band via provision_validator/approve_and_store before \
                 running this workflow) — refusing to run the phase ungated (fail-closed)",
                def.id,
                phase.id
            ),
        }
    }
    Ok(())
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

    // ENGAGE THE DUAL-VALIDATOR GATE (the producer side of the rev0.4 pin+vault): for a def-driven run,
    // any phase that declares a `validator_pin` gets its ALREADY-APPROVED validator LOADED from the vault
    // and attached to the phase's unit. Units are 1:1 with `def.phases` in declaration order (plan_from_def
    // zips them), so we zip here too. Loading is a PURE store read — no LLM authoring — so it is actor-safe
    // on the single-writer thread; authoring/approval happened out of band. Once attached, the gate reads
    // `unit.validator`: `pinned_validator_denial` re-verifies it deterministically and the off-thread
    // agent judge renders its semantic verdict — the layers that were INERT with nothing pinning the unit.
    if let Some(def) = &selected_def {
        attach_pinned_validators(store, &mut units, def)?;
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
    agent_verdict: Option<&(bool, String)>,
    emit: &mut dyn FnMut(CoreEvent),
) -> anyhow::Result<UnitOutcome> {
    // ── PRE-RESOLVE every deny-dominant signal BEFORE the governance gate resolves the phase, so a
    //    deny drives the phase to Rejected and NO approved phase / work_output can leak past it (seam
    //    finding #2 / ADR-0003). Each signal is pure + actor-safe: the deterministic re-verify runs a
    //    fixed approved script, the agent verdict already ran OFF-THREAD (folded here), and the
    //    evaluator≠creator pass is deterministic governance.

    // (layer-1) PINNED VALIDATOR — the rev0.4 deterministic re-verify against the run's worktree. A
    // FAIL — OR the ABSENCE of a worktree (fail-closed, so the agent LLM can never lone-approve a pinned
    // phase) — denies. Pure, no LLM.
    let workdir = crate::domain::get_session(store, session_id)?.and_then(|s| s.workdir);
    let det_denial = pinned_validator_denial(unit, workdir.as_deref().map(std::path::Path::new));

    // (layer-2) AGENT VALIDATOR — fold the OFF-THREAD semantic verdict (actor::dispatch_unit's closure
    // ran `claude -p`; here we only interpret its `(pass, reasoning)` via `combine_verdict`). An agent
    // REJECT denies; the agent can never be the SOLE approver. `None` ⇒ no pinned validator.
    let agent_denial = agent_verdict_denial(agent_verdict);

    // (evaluator≠creator) a SECOND governance pass with a DISTINCT evaluator identity whose verdict now
    // GATES (finding #9 — previously discarded). For an Evaluator-role unit it reviews the COLD output
    // of the most recent prior Creator (real artifact-passing, finding #8 on the governance claim);
    // Neutral/Creator units keep the generic per-unit second pass. Falls back to own `output` when there
    // is no prior creator/output, so behavior never regresses. Deterministic governance — actor-safe.
    let assigned_cli = unit
        .assigned_cli
        .clone()
        .unwrap_or_else(|| "claude".to_string());
    let collection_scope = resolve_scope(entity_mode, session_id, &unit.id);
    let evaluator_cli = next_cli_in_roster(&assigned_cli, cli_keys);
    let eval_at = execute::EVAL_AT_BASE + unit.ord as i64 + 1_000_000;
    let review_output = if unit.role == crate::workflow::PhaseRole::Evaluator {
        creator_output_for(store, session_id, unit.ord).unwrap_or_else(|| output.to_string())
    } else {
        output.to_string()
    };
    let eval = execute::evaluate_unit(
        store,
        unit,
        &review_output,
        &evaluator_cli,
        &collection_scope,
        &format!("unit-{}", unit.ord),
        eval_at,
    )
    .ok();
    let evaluator_claim_id = eval.as_ref().map(|e| e.claim_id.clone());
    let evaluator_denial = eval.as_ref().and_then(|e| {
        (!e.approved).then(|| {
            format!(
                "evaluator ({evaluator_cli}) rejected unit {} (evaluator≠creator second pass, decision={})",
                unit.ord, e.decision
            )
        })
    });

    // DENY-DOMINATES ordering: deterministic re-verify, then agent judge, then the evaluator pass.
    let validator_denial = det_denial.or(agent_denial).or(evaluator_denial);

    // Resolve the governance gate WITH the pre-computed deny folded in: a validator/evaluator deny
    // drives the phase Rejected + suppresses the work_output write (see `execute::apply_unit`).
    let mut outcome = execute::apply_unit(
        store,
        unit,
        output,
        workflow_id,
        entity_mode,
        session_id,
        validator_denial,
    )?;
    outcome.evaluator_claim_id = evaluator_claim_id;

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
/// `Some(denial_reason)` when the validator fails, errors, OR cannot be re-verified — the gate then
/// denies the unit (deny-dominates). `None` means "no denial": either the unit has NO pinned validator
/// (an ungated, pre-gate phase) or the validator PASSED. Pure + actor-safe (no LLM).
///
/// FAIL-CLOSED on a missing worktree (Lane D finding 1): a unit that carries a pinned validator but has
/// NO workdir to re-verify against is DENIED, not skipped. Skipping would leave `deterministic_pass =
/// true` for the layer-2 fold, making the agent LLM the SOLE approver of the pinned phase — the exact
/// rev0.4 violation ("Approve requires a deterministic PASS"). "Can't re-verify" is treated as
/// NOT-passed, never assumed-pass. (Consequence: a repo-less run cannot satisfy a pinned phase — that
/// is intended; register a repo so the run has a worktree.)
fn pinned_validator_denial(
    unit: &crate::domain::WorkUnit,
    workdir: Option<&std::path::Path>,
) -> Option<String> {
    // No pinned validator ⇒ ungated phase ⇒ no denial (unchanged pre-gate behavior).
    let v = unit.validator.as_ref()?;
    // Pinned but no worktree ⇒ FAIL-CLOSED (see the doc comment): the deterministic floor is REQUIRED
    // for a pinned phase, so an un-re-verifiable pin denies rather than deferring the whole gate to the
    // agent LLM.
    let Some(cwd) = workdir else {
        return Some(format!(
            "pinned validator `{}` cannot be re-verified: this run has no workdir to check it \
             against (fail-closed — a pinned phase REQUIRES the deterministic floor, so an \
             un-re-verifiable pin is treated as NOT-passed; register a repo so the run has a worktree)",
            v.criterion
        ));
    };
    match crate::validator::run_validator(v, cwd) {
        Ok(true) => None,
        Ok(false) => Some(format!("pinned validator failed: {}", v.criterion)),
        Err(e) => Some(format!("pinned validator error: {e}")),
    }
}

/// Fold the OFF-THREAD agent verdict into the gate (rev0.4 dual-validator layer-2) via
/// [`crate::validator::combine_verdict`], deny-dominates. Called only when the unit already PASSED the
/// deterministic layer (`outcome.approved` still true), so `deterministic_pass = true` here; an agent
/// REJECT ⇒ `Some(denial_reason)` (the gate then denies). `None` verdict (no pinned validator /
/// structural phase) OR an agent PASS ⇒ `None` (no denial). PURE + actor-safe: the LLM already ran on
/// the worker thread; this only interprets the `(pass, reasoning)` it produced. `combine_verdict`
/// guarantees the agent can FAIL a gate but is never the sole approver.
fn agent_verdict_denial(agent: Option<&(bool, String)>) -> Option<String> {
    let (pass, reasoning) = agent?;
    let verdict = crate::validator::AgentVerdict {
        pass: *pass,
        reasoning: reasoning.clone(),
    };
    match crate::validator::combine_verdict(true, Some(&verdict)) {
        crate::validator::GateVerdict::Approve => None,
        crate::validator::GateVerdict::Reject => {
            Some(format!("agent validator rejected: {reasoning}"))
        }
    }
}

/// The cold artifact an Evaluator-role unit reviews (rev0.4 §4 artifact-passing): the work-output of
/// the most recent prior Creator-role unit. `None` if there is no prior Creator or it has no output.
/// `pub(crate)` so the actor's off-thread agent-validator path (dispatch_unit) can judge the SAME cold
/// creator output the governance evaluator pass reads (seam finding #8).
pub(crate) fn creator_output_for(
    store: &wicked_apps_core::SqliteStore,
    session_id: &str,
    evaluator_ord: u32,
) -> Option<String> {
    let units = crate::domain::session_units(store, session_id).ok()?;
    let creator = most_recent_prior_creator(&units, evaluator_ord)?;
    crate::domain::get_work_output(store, &creator.id)
}

/// Pure selector: the highest-`ord` unit before `evaluator_ord` whose role is `Creator`.
fn most_recent_prior_creator(
    units: &[crate::domain::WorkUnit],
    evaluator_ord: u32,
) -> Option<&crate::domain::WorkUnit> {
    units
        .iter()
        .filter(|u| u.ord < evaluator_ord && u.role == crate::workflow::PhaseRole::Creator)
        .max_by_key(|u| u.ord)
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
        // Pinned validator but NO worktree ⇒ FAIL-CLOSED denial (Lane D finding 1): "can't re-verify"
        // is NOT-passed, so the agent LLM can never become the sole approver of a pinned phase.
        assert!(
            pinned_validator_denial(&unit, None).is_some(),
            "a pinned validator with no worktree must DENY (fail-closed), not skip"
        );
        // A unit with NO pinned validator and no worktree is simply ungated ⇒ no denial.
        let ungated = WorkUnit::pending("s:u2", "s", 2, "d");
        assert!(pinned_validator_denial(&ungated, None).is_none());
        // UNAPPROVED ⇒ run_validator refuses ⇒ denial (fail-closed).
        unit.validator = Some(mk("test -f ok.txt", false));
        assert!(pinned_validator_denial(&unit, Some(&dir)).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_verdict_denial_folds_the_rev04_combine_rule() {
        // No agent verdict (no pinned validator / structural phase) ⇒ no denial.
        assert!(agent_verdict_denial(None).is_none());
        // Agent PASS ⇒ no denial (deterministic side already passed to reach here).
        assert!(agent_verdict_denial(Some(&(true, "looks good".into()))).is_none());
        // Agent REJECT ⇒ denial (deny-dominates); the reason is carried through for the UI.
        let denial = agent_verdict_denial(Some(&(false, "diverged from criterion".into())));
        assert!(
            denial
                .as_deref()
                .unwrap()
                .contains("diverged from criterion"),
            "agent reject must deny and surface the reason: {denial:?}"
        );
    }

    #[test]
    fn agent_reject_flips_an_approved_unit_to_denied_in_the_gate_fold() {
        // Exercise the EXACT gate-fold shape from `apply_and_finish_unit` (combine_verdict deny path)
        // WITHOUT an LLM or a store: the deterministic layer has approved (`approved == true`); the
        // off-thread agent verdict is REJECT; the fold must flip the unit to denied and record why.
        let mut approved = true;
        let mut denial_reason: Option<String> = None;
        let agent = (
            false,
            "output does not satisfy the acceptance criterion".to_string(),
        );
        if approved {
            if let Some(reason) = agent_verdict_denial(Some(&agent)) {
                approved = false;
                denial_reason = Some(reason);
            }
        }
        assert!(
            !approved,
            "agent REJECT must flip an approved unit to denied"
        );
        assert!(denial_reason.unwrap().contains("does not satisfy"));

        // Mirror: an agent PASS leaves an approved unit approved (the agent never lone-approves, but it
        // also must not spuriously deny a passing unit).
        let mut approved2 = true;
        if approved2 && agent_verdict_denial(Some(&(true, "ok".into()))).is_some() {
            approved2 = false;
        }
        assert!(approved2, "agent PASS must not flip an approved unit");
    }

    #[test]
    fn plan_attaches_an_approved_pinned_validator_so_the_gate_engages() {
        // The PRODUCER half of the inert-gate fix: a def phase that pins an already-approved validator
        // must have that validator LOADED from the vault and attached to its unit — so `unit.validator`
        // is finally non-`None` and the deterministic re-verify + agent judge actually fire. This runs
        // the EXACT sequence `plan_and_distribute` runs (plan_from_def → attach_pinned_validators),
        // deterministically and with NO LLM (the validator is constructed + vaulted directly).
        use crate::validator::DeterministicValidator;
        use crate::validator_vault::{pin, store_validator};
        use wicked_apps_core::open_store;

        let dir = std::env::temp_dir().join(format!("wicked-pin-attach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = open_store(Some(dir.join("v.db").to_str().unwrap())).unwrap();

        // An APPROVED validator, vaulted out of band (authoring is an LLM step; here we build it directly).
        let approved = DeterministicValidator {
            criterion: "README exists".into(),
            script: "test -f README.md".into(),
            approved: true,
        };
        let p = store_validator(&mut store, &approved).unwrap();
        assert_eq!(p, pin(&approved), "store returns the content-hash pin");

        // A 1-phase def whose phase PINS that approved validator (authored as pure JSON data).
        let def: crate::workflow::WorkflowDef = serde_json::from_str(&format!(
            r#"{{ "id": "gated", "phases": [ {{ "id": "build", "kind": "build", "validator_pin": "{p}" }} ] }}"#
        ))
        .unwrap();
        def.validate().unwrap();

        let mut units = crate::plan::plan_from_def(&def, "do it", "s");
        assert!(
            units[0].validator.is_none(),
            "before the producer runs, the unit is UNGATED (the inert-gate state)"
        );
        attach_pinned_validators(&store, &mut units, &def).unwrap();

        assert_eq!(
            units[0].validator.as_ref(),
            Some(&approved),
            "the phase's approved validator is loaded from the vault and pinned onto the unit — the gate ENGAGES"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unresolvable_validator_pin_fails_closed() {
        // A phase that pins a validator missing from the vault is a MISCONFIGURATION; rather than run the
        // phase silently ungated, the producer BAILS (fail-closed) with an error naming the phase + pin.
        use wicked_apps_core::open_store;
        let dir = std::env::temp_dir().join(format!("wicked-pin-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = open_store(Some(dir.join("v.db").to_str().unwrap())).unwrap();

        let def: crate::workflow::WorkflowDef = serde_json::from_str(
            r#"{ "id": "gated", "phases": [ { "id": "build", "kind": "build", "validator_pin": "deadbeefdeadbeef" } ] }"#,
        )
        .unwrap();
        def.validate().unwrap();
        let mut units = crate::plan::plan_from_def(&def, "do it", "s");
        let err = attach_pinned_validators(&store, &mut units, &def)
            .expect_err("an unresolvable pin must bail, not silently run ungated");
        let msg = err.to_string();
        assert!(
            msg.contains("deadbeefdeadbeef") && msg.contains("not in the vault"),
            "the fail-closed error must name the missing pin: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unapproved_pinned_validator_bails_at_plan_time() {
        // Lane D finding 2: a phase that pins an UNAPPROVED-but-vaulted validator must be caught at PLAN
        // time (attach), not attached and then denying every run at gate time. The bail names the phase
        // + pin and points at approval.
        use crate::validator::DeterministicValidator;
        use crate::validator_vault::{pin, store_validator};
        use wicked_apps_core::open_store;

        let dir = std::env::temp_dir().join(format!("wicked-pin-unappr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = open_store(Some(dir.join("v.db").to_str().unwrap())).unwrap();

        // Vault an UNAPPROVED validator and pin THAT (unapproved) pin from a phase.
        let unapproved = DeterministicValidator {
            criterion: "README exists".into(),
            script: "test -f README.md".into(),
            approved: false,
        };
        let p = store_validator(&mut store, &unapproved).unwrap();
        assert_eq!(p, pin(&unapproved), "the unapproved validator's pin");

        let def: crate::workflow::WorkflowDef = serde_json::from_str(&format!(
            r#"{{ "id": "gated", "phases": [ {{ "id": "build", "kind": "build", "validator_pin": "{p}" }} ] }}"#
        ))
        .unwrap();
        def.validate().unwrap();
        let mut units = crate::plan::plan_from_def(&def, "do it", "s");
        let err = attach_pinned_validators(&store, &mut units, &def)
            .expect_err("an unapproved pin must bail at plan time, not attach + DoS every run");
        let msg = err.to_string();
        assert!(
            msg.contains(&p) && msg.contains("UNAPPROVED") && msg.contains("approve-validator"),
            "the bail must name the pin + point at approval: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn work_unit_validator_survives_the_store_round_trip() {
        // Lane D finding 4: dispatch/apply read `unit.validator` back from the store (via session_units),
        // so an attached pinned validator must survive put_node → session_units losslessly. Attach an
        // APPROVED validator, persist the unit, read it back, and assert the validator is byte-identical.
        use crate::domain::{put_node, session_units, WorkUnit};
        use crate::validator::DeterministicValidator;
        use wicked_apps_core::{open_store, ToNode};

        let dir = std::env::temp_dir().join(format!("wicked-unit-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = open_store(Some(dir.join("v.db").to_str().unwrap())).unwrap();

        let approved = DeterministicValidator {
            criterion: "README exists".into(),
            script: "test -f README.md".into(),
            approved: true,
        };
        let mut unit = WorkUnit::pending("rt:u1", "rt", 1, "build the thing");
        unit.validator = Some(approved.clone());
        put_node(&mut store, unit.to_node()).unwrap();

        let read = session_units(&store, "rt").unwrap();
        assert_eq!(read.len(), 1, "one unit persisted for the session");
        assert_eq!(
            read[0].validator.as_ref(),
            Some(&approved),
            "the approved pinned validator survives put_node → session_units intact (dispatch/apply rely on this)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn artifact_passing_picks_the_latest_prior_creator() {
        use crate::domain::WorkUnit;
        use crate::workflow::PhaseRole;
        let mk = |ord: u32, role: PhaseRole| {
            let mut u = WorkUnit::pending(format!("s:u{ord}"), "s", ord, "d");
            u.role = role;
            u
        };
        let units = vec![
            mk(1, PhaseRole::Neutral),
            mk(2, PhaseRole::Creator),
            mk(3, PhaseRole::Creator), // most recent creator before the evaluator
            mk(4, PhaseRole::Evaluator),
        ];
        assert_eq!(most_recent_prior_creator(&units, 4).unwrap().ord, 3);
        assert_eq!(most_recent_prior_creator(&units, 3).unwrap().ord, 2);
        // no creator before ord 2 ⇒ None (the evaluator falls back to its own output)
        assert!(most_recent_prior_creator(&units, 2).is_none());
    }

    #[test]
    fn creator_output_for_reads_the_prior_creators_cold_output_from_the_store() {
        // Seam finding #8: the artifact an Evaluator judges is the most-recent prior Creator's COLD
        // stored output — the SAME source both the governance evaluator pass and (now) the off-thread
        // agent validator read. Persist a Creator unit + run its gate so its work_output is stored,
        // then assert an evaluator at a later ord resolves that exact output.
        use crate::domain::{put_node, WorkUnit};
        use crate::workflow::PhaseRole;
        use wicked_apps_core::{open_store, ToNode};

        let mut store = open_store(Some(":memory:")).unwrap();
        let mut creator = WorkUnit::pending("s:u1", "s", 1, "build it");
        creator.role = PhaseRole::Creator;
        creator.assigned_cli = Some("claude".into());
        put_node(&mut store, creator.to_node()).unwrap();
        // Run the creator's gate (governance allows) so its cold output is persisted as work_output.
        crate::execute::apply_unit(
            &mut store,
            &creator,
            "CREATOR-COLD-OUTPUT",
            "wf-s",
            EntityMode::Shared,
            "s",
            None,
        )
        .unwrap();

        // An evaluator at ord 2 resolves the creator's cold output (not its own).
        assert_eq!(
            creator_output_for(&store, "s", 2).as_deref(),
            Some("CREATOR-COLD-OUTPUT"),
            "the evaluator's artifact is the prior creator's cold stored output"
        );
        // No prior creator before ord 1 ⇒ None (the caller falls back to the unit's own output).
        assert!(creator_output_for(&store, "s", 1).is_none());
    }
}
