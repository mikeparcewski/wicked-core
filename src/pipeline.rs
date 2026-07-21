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

use crate::domain::{put_node, AgentSession, RoutingInfo, SessionStatus, UnitStatus};
use crate::event::CoreEvent;
use crate::execute::{self, UnitOutcome};
use crate::scope::{resolve_scope, EntityMode};
use crate::workflow::{GateSpec, PhaseRole};
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
    store: &mut dyn wicked_apps_core::GraphStore,
    clis: Vec<AgenticCli>,
    problem: &str,
    entity_mode: EntityMode,
    session_id: &str,
    workflow: Option<&str>,
    dispatcher: Arc<dyn Dispatcher + Send + Sync>,
    emit: &mut dyn FnMut(CoreEvent),
) -> anyhow::Result<SessionResult> {
    // Clear any prior run's per-run governance dir for this session id (the sync driver, like
    // launch_run_inner, must not inherit a stale decisions log — a leftover Deny would spuriously fail
    // this run; council [14]). A brand-new id is a harmless no-op.
    let _ = std::fs::remove_dir_all(crate::gate_hook::gov_run_dir(session_id));
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
        None, // legacy sync path: no actor-owned registry (uses built-ins + overlay dir per-call)
        false, // stub not yet created
        crate::actor::in_process_governance().is_some(), // propagate governance from calling thread
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
            0, // sync straight-through path is ungoverned (stub work) — the fold is inert (no log)
            false, // the stub sync path never arms governance
            &cli_keys,
            None, // sync straight-through path runs no off-thread agent judge (stub work, no LLM)
            emit,
            None, // sync stub path has no estate db path to inject
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

/// The result of [`pre_distribute`] — planning complete, units persisted at `Distributing`, ready
/// for the blocking council call. All fields are owned so this can be moved across a thread boundary.
pub(crate) struct PreDistributed {
    pub session_id: String,
    pub session: AgentSession,
    pub units: Vec<crate::domain::WorkUnit>,
    /// The launch roster — needed by `distribute_units_on` to convene the council.
    pub clis: Vec<AgenticCli>,
    pub workflow_id: String,
    pub cli_keys: Vec<String>,
}

/// Resolve a selected workflow id to its validated [`WorkflowDef`]. When `extra` is provided it is
/// consulted first (runtime-registered workflows take priority over built-ins and the file overlay);
/// otherwise seeds the built-ins and overlays operator drop-in files (`$WICKED_WORKFLOWS_DIR`, else
/// `$HOME/.config/wicked-core/workflows`, best-effort). `None` (no selection) ⇒ `Ok(None)` and the
/// caller uses the free-text planner; a requested-but-**unknown** id ⇒ `Err` (never a silent
/// fallback).
pub(crate) fn resolve_workflow_def(
    workflow: Option<&str>,
    extra: Option<&crate::workflow::WorkflowRegistry>,
) -> anyhow::Result<Option<crate::workflow::WorkflowDef>> {
    // No selection ⇒ the caller uses the free-text planner. Only THIS falls through.
    let Some(id) = workflow else {
        return Ok(None);
    };
    // When the caller provides an actor-owned registry (the interactive LaunchRun path), use it
    // as the sole authoritative source: it already contains built-ins (seeded at actor startup
    // via `with_defaults()`), the overlay directory (loaded at startup), and any runtime-registered
    // workflows. Falling through to a disk re-scan when `extra` is present would be redundant I/O
    // and could surface stale/inconsistent overlay files added after startup.
    if let Some(reg) = extra {
        // A requested-but-unknown id is a loud error here too — never a silent Ok(None) fallback.
        // The actor-owned registry already contains built-ins + overlay workflows, so a miss is a
        // real typo/invalid id, not a "not-yet-loaded" race.
        return reg.get(id).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "unknown workflow `{id}` — known workflows: {}",
                reg.ids().join(", ")
            )
        }).map(Some);
    }
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
    store: &dyn wicked_apps_core::GraphStore,
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

pub(crate) fn workflow_overlay_dir() -> Option<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("WICKED_WORKFLOWS_DIR") {
        return Some(std::path::PathBuf::from(d));
    }
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config/wicked-core/workflows"))
}

/// Plan a session and persist units to the store, stopping SHORT of the blocking council call.
/// Returns a [`PreDistributed`] that carries everything the distribute thread needs (all owned).
/// Emits `SessionStarted` (when `!session_already_started`) + `UnitPlanned×n`. Leaves the session
/// at `Distributing`. Store-writing — runs on the actor (single-writer) thread.
///
/// The caller spawns a thread, calls `distribute::distribute_units_on(&pre.units, &pre.clis, ...)`
/// there, then posts `Command::PlanReady` (or `PlanFailed`) back to the actor. The actor arm calls
/// [`apply_distributions`] to finish the setup and dispatch unit 0.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pre_distribute(
    store: &mut dyn wicked_apps_core::GraphStore,
    clis: &[AgenticCli],
    problem: &str,
    entity_mode: EntityMode,
    session_id: &str,
    human_confirm: crate::domain::HumanConfirm,
    repo_ref: Option<String>,
    workdir: Option<String>,
    workflow: Option<&str>,
    emit: &mut dyn FnMut(CoreEvent),
    workflow_registry: Option<&crate::workflow::WorkflowRegistry>,
    session_already_started: bool,
    // Whether input governance is active for this run. The call site is responsible for
    // evaluating in_process_governance().is_some() and passing the result here; pre_distribute
    // must never read the GOV_DB_PATH thread-local directly, because thread-locals do not
    // propagate to spawned threads (including the sync/test path where it is unset).
    governed: bool,
) -> anyhow::Result<PreDistributed> {
    let workflow_id = format!("wf-{session_id}");
    let cli_keys: Vec<String> = clis.iter().map(|c| c.key.clone()).collect();

    let selected_def = resolve_workflow_def(workflow, workflow_registry)?;
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

    if let Some(def) = &selected_def {
        attach_pinned_validators(store, &mut units, def)?;
        // EVT-009 is emitted AFTER SessionStarted + UnitPlanned×n below — see the comment there.
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
    if !session_already_started {
        put_node(store, session.to_node())?;
        emit(CoreEvent::SessionStarted {
            session: session_id.to_string(),
            problem: problem.to_string(),
            workflow_id: selected_def.as_ref().map(|d| d.id.clone()),
            cli_count: clis.len() as u32,
            governed,
            entity_mode: match entity_mode {
                EntityMode::Shared => "shared".to_string(),
                EntityMode::Isolated => "isolated".to_string(),
            },
        });
    }

    // (EVT-001) WorkflowSelected — the authoritative decomposition signal for structured runs.
    // Fires once per session, after SessionStarted and before the first UnitPlanned, so consumers
    // that initialise per-session state on SessionStarted see it before any unit events arrive.
    if let Some(def) = &selected_def {
        emit(CoreEvent::WorkflowSelected {
            session: session_id.to_string(),
            workflow_id: def.id.clone(),
            unit_count: u32::try_from(units.len()).unwrap_or(u32::MAX),
        });
    }

    for u in &units {
        put_node(store, u.to_node())?;
        emit(CoreEvent::UnitPlanned {
            session: session_id.to_string(),
            ord: u.ord,
            description: u.description.clone(),
            stage: u.stage.label().to_string(),
            role: match u.role {
                PhaseRole::Neutral => "neutral",
                PhaseRole::Creator => "creator",
                PhaseRole::Evaluator => "evaluator",
            }
            .to_string(),
            gate: match &u.gate {
                GateSpec::Auto => "auto",
                GateSpec::HumanConfirm { .. } => "human_confirm",
                GateSpec::HumanConfirmIf(_) => "human_confirm_if",
            }
            .to_string(),
            skill_ref: u.skill_ref.clone(),
            has_validator_pin: u.validator.is_some(),
            executor_type: if u.tool_cmd.is_some() {
                "tool"
            } else {
                "agent"
            }
            .to_string(),
        });
    }
    // (EVT-009) ValidationPinAttached — emitted here, AFTER SessionStarted + UnitPlanned×n, so
    // that consumers initialising per-session state on SessionStarted see events in the natural
    // "session open → units planned → pins attached" order (Copilot).  Emitting before
    // SessionStarted (the original position) created an ordering edge-case where the session was
    // not yet "started" when the first pin event arrived.
    for u in &units {
        if let Some(v) = &u.validator {
            emit(CoreEvent::ValidationPinAttached {
                session: session_id.to_string(),
                ord: u.ord,
                pin: crate::validator_vault::pin(v),
                criterion: v.criterion.clone(),
            });
        }
    }

    session.status = SessionStatus::Distributing;
    put_node(store, session.to_node())?;

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

    Ok(PreDistributed {
        session_id: session_id.to_string(),
        session,
        units,
        clis: clis.to_vec(),
        workflow_id,
        cli_keys,
    })
}

/// Apply council distributions to the pre-distributed units, persist assignments to the store, and
/// advance the session to `Executing`. Emits `UnitDistributed×n`. Store-writing — runs on the actor
/// thread (called from the `PlanReady` command arm).
pub(crate) fn apply_distributions(
    store: &mut dyn wicked_apps_core::GraphStore,
    pre: &mut PreDistributed,
    distributions: Vec<crate::distribute::Distribution>,
    emit: &mut dyn FnMut(CoreEvent),
) -> anyhow::Result<()> {
    for (u, dist) in pre.units.iter_mut().zip(distributions.iter()) {
        u.assigned_cli = Some(dist.assigned_cli.clone());
        u.assigned_invocation = dist.assigned_invocation.clone();
        u.council_task_ref = dist.council_task_ref.clone();
        u.routing = Some(dist.routing.clone());
        u.status = UnitStatus::Distributed;
        put_node(store, u.to_node())?;
        let (routing_method, agreement_pct, returned, dissent, degraded_reason) =
            match &dist.routing {
                RoutingInfo::Council {
                    agreement_pct,
                    returned,
                    dissent,
                    ..
                } => (
                    "council".to_string(),
                    Some(*agreement_pct),
                    Some(*returned),
                    Some(*dissent),
                    None,
                ),
                RoutingInfo::Degraded { reason } => (
                    "degraded".to_string(),
                    None,
                    None,
                    None,
                    Some(reason.clone()),
                ),
                RoutingInfo::EvaluatorDistinct { .. } => {
                    ("evaluator_distinct".to_string(), None, None, None, None)
                }
                RoutingInfo::Tool => ("tool".to_string(), None, None, None, None),
            };
        emit(CoreEvent::UnitDistributed {
            session: pre.session_id.clone(),
            ord: u.ord,
            cli: dist.assigned_cli.clone(),
            routing_method,
            agreement_pct,
            returned,
            dissent,
            degraded_reason,
        });
    }
    pre.session.status = SessionStatus::Executing;
    put_node(store, pre.session.to_node())?;
    Ok(())
}

/// PLAN + DISTRIBUTE (used by the sync operator CLI + tests): the full sequential path — plan,
/// persist, distribute (blocking council), apply assignments. For the interactive actor engine the
/// call is split: [`pre_distribute`] on the actor thread + `distribute_units_on` off-thread +
/// [`apply_distributions`] back on the actor thread via `Command::PlanReady`.
///
/// `workflow_registry`: when `Some`, the actor-owned runtime registry is consulted first for
/// workflow resolution (enables defs registered via `RegisterWorkflow` without a restart).
/// When `session_already_started` is `true` the caller already wrote a Planning stub + emitted
/// `SessionStarted`; we skip the duplicate writes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_and_distribute(
    store: &mut dyn wicked_apps_core::GraphStore,
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
    workflow_registry: Option<&crate::workflow::WorkflowRegistry>,
    session_already_started: bool,
    // Whether input governance is active. See pre_distribute's `governed` parameter — the call
    // site supplies this value so neither pre_distribute nor plan_and_distribute read the
    // GOV_DB_PATH thread-local internally. Pass in_process_governance().is_some() from the
    // calling thread; the sync/test path correctly gets false when GOV_DB_PATH is not set.
    governed: bool,
) -> anyhow::Result<Planned> {
    let mut pre = pre_distribute(
        store,
        clis,
        problem,
        entity_mode,
        session_id,
        human_confirm,
        repo_ref,
        workdir,
        workflow,
        emit,
        workflow_registry,
        session_already_started,
        governed,
    )?;
    let distributions =
        distribute::distribute_units_on(&pre.units, clis, session_id, None, dispatcher)?;
    apply_distributions(store, &mut pre, distributions, emit)?;
    Ok(Planned {
        session: pre.session,
        units: pre.units,
        workflow_id: pre.workflow_id,
        cli_keys: pre.cli_keys,
    })
}

/// Apply one unit's produced `output` (shared by both drivers): run the governance gate (creator
/// pass) + the evaluator≠creator second pass, tick the workflow cursor, persist the unit's resolved
/// status, and emit `GateDecided` + `UnitDone`/`UnitDenied`. The caller emits `UnitExecuting` BEFORE
/// the work runs. Store-writing, so it runs on the actor (single-writer) thread.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_and_finish_unit(
    store: &mut dyn wicked_apps_core::GraphStore,
    unit: &mut crate::domain::WorkUnit,
    output: &str,
    workflow_id: &str,
    entity_mode: EntityMode,
    session_id: &str,
    attempt: u32,
    governed: bool,
    cli_keys: &[String],
    agent_verdict: Option<&(bool, String)>,
    emit: &mut dyn FnMut(CoreEvent),
    db_path: Option<&str>,
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
    let det_denial =
        pinned_validator_denial(unit, workdir.as_deref().map(std::path::Path::new), db_path);
    // (DES-STUDIO-COCKPIT-001 §3 B1) Capture the layer-1 (deterministic) pass NOW, before `det_denial` is
    // moved into the deny-dominance fold below, so `GateEvaluated` can carry the depth.
    let deterministic_pass = det_denial.is_none();

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
        &crate::scope::unit_phase(unit.ord),
        eval_at,
    )
    .ok();
    let evaluator_claim_id = eval.as_ref().map(|e| e.claim_id.clone());
    // (S2) The evaluator≠creator second-pass result, surfaced on `GateEvaluated` so the denying layer is
    // visible: `Some(false)` when this layer denied (det may still have passed + no agent judge ran),
    // `Some(true)` when it approved, `None` when it did not run.
    let evaluator_pass = eval.as_ref().map(|e| e.approved);
    let evaluator_denial = eval.as_ref().and_then(|e| {
        (!e.approved).then(|| {
            format!(
                "evaluator ({evaluator_cli}) rejected unit {} (evaluator≠creator second pass, decision={})",
                unit.ord, e.decision
            )
        })
    });

    // (input governance — DES-OUTGOV-003 §1) Fold this unit's INPUT-hook decisions into the SAME
    // deny-dominant gate rather than a competing phase resolver: read the run's decisions log, conform
    // each of THIS phase's claims as durable evidence, and surface any Deny. Inert (`None`) for
    // ungoverned / sync runs (no decisions log written). A denied tool-call thus drives the unit gate
    // Rejected → the run Failed through the UNCHANGED completion path.
    // `governed` is the RUNNER's authority (it armed the hook + wrote the marker), NOT a derivation from
    // unit properties — so a claude-assigned STUB/test unit (which never armed) is never false-denied for
    // a missing log. It gates evidence-integrity fail-closure: a governed unit whose armed marker is
    // missing (erased/never-fired) DENIES; an ungoverned unit's fold is inert.
    let hook_denial = crate::gate_hook::fold_input_denial(
        store,
        session_id,
        attempt,
        &crate::scope::unit_phase(unit.ord),
        governed,
    )?;
    // Capture whether the hook denied NOW, before `hook_denial` is moved into the deny-dominance
    // fold below and its source identity is lost in `validator_denial`. The actor uses this flag to
    // block HumanConfirmIf routing on hook vetoes (a hook-sourced deny must hard-fail the run, not
    // escalate to human review).
    let hook_denied = hook_denial.is_some();

    // (EVT-008) GovernanceHookFired — replay per-tool-call decisions from the NDJSON log as events.
    // Only runs for governed units (ungoverned units have no log). Reads the log once more (cheap;
    // tiny NDJSON files) so fold_input_denial's signature is unchanged. Emits one event per claim
    // entry for this unit's phase, in log order.
    if governed {
        let phase = crate::scope::unit_phase(unit.ord);
        for rec in crate::gate_hook::collect_hook_decisions(session_id, attempt, &phase) {
            emit(CoreEvent::GovernanceHookFired {
                session: session_id.to_string(),
                ord: unit.ord,
                attempt,
                tool_name: rec.tool_name,
                decision: rec.decision,
                denying_policy: rec.denying_policy,
            });
        }
    }

    // DENY-DOMINATES ordering: deterministic re-verify, agent judge, evaluator pass, input governance.
    let validator_denial = det_denial
        .or(agent_denial)
        .or(evaluator_denial)
        .or(hook_denial);

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
        attempt,
    )?;
    outcome.evaluator_claim_id = evaluator_claim_id;
    outcome.hook_denied = hook_denied;

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

    // (DES-STUDIO-COCKPIT-001 §3 B1) Emit the gate's DEPTH just before the back-compat `GateDecided` bool.
    // The agent (layer-2) verdict/reasoning are `Some` only when the off-thread judge actually ran (an
    // approved validator + a workdir); otherwise honestly `None`. `combined` is the full deny-dominance
    // result (all layers), identical to `GateDecided.allow`.
    let (agent_verdict_str, agent_reasoning) = match agent_verdict {
        Some((pass, reasoning)) => (
            Some(if *pass { "pass" } else { "reject" }.to_string()),
            Some(reasoning.clone()),
        ),
        None => (None, None),
    };
    // (M5) HONEST criterion: `Some` ONLY when a pinned validator gated this unit (its criterion); `None`
    // for an ungated phase — the unit description is never relabeled a "criterion". `has_deterministic_floor`
    // makes the ungated case explicit so `deterministic_pass` (vacuously true with no floor) isn't misread.
    let has_deterministic_floor = unit.validator.is_some();
    let criterion = unit.validator.as_ref().map(|v| v.criterion.clone());
    // (S2) Surface the WINNING denial reason whenever the combined gate denied, so the record is never
    // self-contradictory ("det pass + agent none + combined false" with no visible denying layer).
    let denial_reason = if outcome.approved {
        None
    } else {
        outcome.denial_reason.clone()
    };
    emit(CoreEvent::GateEvaluated {
        session: session_id.to_string(),
        ord: unit.ord,
        criterion,
        has_deterministic_floor,
        deterministic_pass,
        agent_verdict: agent_verdict_str,
        agent_reasoning,
        evaluator_pass,
        denial_reason,
        combined: outcome.approved,
    });
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
    db_path: Option<&str>,
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
    match crate::validator::run_validator_reporting(v, cwd, db_path) {
        Ok((true, _)) => None,
        Ok((false, _)) => Some(format!("pinned validator failed: {}", v.criterion)),
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
    store: &dyn wicked_apps_core::GraphStore,
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
        assert!(resolve_workflow_def(None, None).unwrap().is_none());
        // A known built-in resolves to its def.
        assert_eq!(
            resolve_workflow_def(Some("feature"), None)
                .unwrap()
                .unwrap()
                .id,
            "feature"
        );
        // A requested-but-unknown id is a LOUD error (never a silent fall-through to prose planning).
        let err = resolve_workflow_def(Some("feaure-typo-xyz"), None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unknown workflow") && err.contains("feaure-typo-xyz"),
            "error must name the bad id: {err}"
        );
    }

    #[test]
    fn workflow_selection_with_actor_registry() {
        use crate::workflow::WorkflowRegistry;
        let reg = WorkflowRegistry::with_defaults();

        // Known id in actor registry resolves to def.
        assert_eq!(
            resolve_workflow_def(Some("feature"), Some(&reg))
                .unwrap()
                .unwrap()
                .id,
            "feature"
        );
        // No selection (None) with actor registry still returns None.
        assert!(resolve_workflow_def(None, Some(&reg)).unwrap().is_none());
        // Unknown id with actor registry returns Err (not silent Ok(None)).
        let err = resolve_workflow_def(Some("feaure-typo"), Some(&reg))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unknown workflow") && err.contains("feaure-typo"),
            "error must name the bad id and say 'unknown workflow': {err}"
        );
        // Error message lists known workflow ids.
        assert!(
            err.contains("feature"),
            "error must list known workflows: {err}"
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
        assert!(pinned_validator_denial(&unit, Some(&dir), None).is_none());
        // Approved + PASSES ⇒ no denial.
        unit.validator = Some(mk("test -f ok.txt", true));
        assert!(pinned_validator_denial(&unit, Some(&dir), None).is_none());
        // Approved + FAILS ⇒ denial (deny-dominates).
        unit.validator = Some(mk("test -f missing.txt", true));
        assert!(pinned_validator_denial(&unit, Some(&dir), None).is_some());
        // Pinned validator but NO worktree ⇒ FAIL-CLOSED denial (Lane D finding 1): "can't re-verify"
        // is NOT-passed, so the agent LLM can never become the sole approver of a pinned phase.
        assert!(
            pinned_validator_denial(&unit, None, None).is_some(),
            "a pinned validator with no worktree must DENY (fail-closed), not skip"
        );
        // A unit with NO pinned validator and no worktree is simply ungated ⇒ no denial.
        let ungated = WorkUnit::pending("s:u2", "s", 2, "d");
        assert!(pinned_validator_denial(&ungated, None, None).is_none());
        // UNAPPROVED ⇒ run_validator refuses ⇒ denial (fail-closed).
        unit.validator = Some(mk("test -f ok.txt", false));
        assert!(pinned_validator_denial(&unit, Some(&dir), None).is_some());
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
            0,
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
