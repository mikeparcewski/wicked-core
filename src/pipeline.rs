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
        Vec::new(), // sync path declares no extra write roots (core#259)
        None,       // …and no repo (above) ⇒ no project graph to bind
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
        return reg
            .get(id)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown workflow `{id}` — known workflows: {}",
                    reg.ids().join(", ")
                )
            })
            .map(Some);
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
    // NOTE: several shipped built-in phases now carry a real `validator_pin`, so this loop is NOT a
    // no-op for them — it loads + attaches the pinned validator (the built-in evidence floor,
    // `EVIDENCE_FLOOR_PIN`, which `pre_distribute` seeds so the load resolves). `feature`'s
    // `adversarial-review` pins it directly (FINDING-025 item 1), and registration arms every
    // `verified_evidence` phase that names no pin of its own with the same floor (FINDING-055:
    // `feature`/`test`, `bug`/`verify`, `migration`/`verify`, `domain-extraction`/`coverage`).
    // A phase with no pin still leaves the unit's validator `None` (ungated). Operators author
    // phase-specific criteria via `wicked-core provision-validator --criterion "..."` then
    // `wicked-core approve-validator --pin <pin>`, and put the approved pin in a def's `validator_pin`.
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
            // Name the CLI, not just the Rust API — the UNAPPROVED arm above already does, and an
            // operator who hits this one is strictly worse off for the inconsistency. The shipped
            // coverage pin has a purpose-built one-liner (`seed-domain-validators`); every other pin
            // goes through the generic author→approve pair. Both are commands the operator can run.
            None => anyhow::bail!(
                "workflow `{}` phase `{}` pins validator `{pin}`, which is not in the vault — \
                 refusing to run the phase ungated (fail-closed). {}",
                def.id,
                phase.id,
                missing_pin_remedy(pin)
            ),
        }
    }
    Ok(())
}

/// The operator-runnable remedy named by [`attach_pinned_validators`]'s missing-pin refusal.
///
/// The shipped `domain-extraction` drop-in pins a hand-authored validator that the LLM writer path
/// cannot reproduce (its script is deterministic, so `provision-validator` would author a *different*
/// script and therefore a different pin). `seed-domain-validators` exists for exactly that pin and is
/// idempotent. Pointing an operator at the generic author→approve pair for it would send them down a
/// path that cannot produce the pin they need.
///
/// Every arm carries [`vault_is_per_db`], because naming a command is only half a remedy: the vault
/// lives in whichever database the engine opened, and every one of these commands defaults to a
/// DIFFERENT database than a long-running engine does (FINDING-066). The coverage arm is now
/// belt-and-braces — `pre_distribute` seeds that pin on the plan path, so reaching it means the seed
/// itself failed or the engine predates it — but the generic arm is live, and an operator who runs
/// `provision-validator` in the wrong cwd hits the identical dead end.
fn missing_pin_remedy(pin: &str) -> String {
    if pin == crate::domain_extraction::COVERAGE_VALIDATOR_PIN {
        format!(
            "This is the coverage validator the shipped `domain-extraction` workflow pins, and the \
             plan path seeds it automatically — so seeing this means the seed did not take. Run \
             `wicked-core seed-domain-validators` to vault + approve it (idempotent, and it yields \
             exactly this pin). {}",
            vault_is_per_db()
        )
    } else {
        format!(
            "Author + approve it first: `wicked-core provision-validator --criterion \"...\"` then \
             `wicked-core approve-validator --pin <unapproved pin>`, and pin the APPROVED pin \
             (`{pin}` resolves to nothing today). {}",
            vault_is_per_db()
        )
    }
}

/// The sentence that turns "run this command" into a remedy that actually lands.
///
/// The validator vault is not global state — it is rows in the graph store, so it is scoped to one
/// database. The CLI resolves `--db` ELSE `WICKED_ESTATE_DB` ELSE a cwd-relative default; a daemon
/// embedding the engine resolves its own state home. Those agree only by accident, and when they
/// disagree the seed succeeds, prints the right pin, and changes nothing the engine can see.
///
/// The variable name is interpolated from [`crate::gate_hook::ESTATE_DB_ENV`], not typed out: a remedy
/// that names a variable the CLI no longer reads is the FINDING-066 failure with extra steps. The test
/// that pins this sentence deliberately hard-codes the literal instead — a test sharing the const
/// would rename itself alongside a rename and detect nothing.
fn vault_is_per_db() -> String {
    format!(
        "Pass `--db <the database the engine opened>` (ELSE `{}`, \
         ELSE a cwd-relative default): the vault is rows in that store, so seeding any other database \
         succeeds, prints the right pin, and leaves this refusal exactly as it was.",
        crate::gate_hook::ESTATE_DB_ENV
    )
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
    // Launcher-declared extra write roots (core#259), already validated at launch; persisted on
    // the session so resume/redrive re-arms the boundary the launch declared.
    extra_write_roots: Vec<String>,
    // The launcher's project-graph binding, persisted on the session for the same reason: a resume
    // re-enters with no LaunchSpec, and a run whose tools silently narrow from the whole project to
    // one repo halfway through is worse than one that never had them. VERIFIED at dispatch, never
    // here — see `actor::project_code_graph_db`.
    project_graph: Option<crate::project::ProjectGraphBinding>,
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
    // core#120: a Tool-executor phase with an unresolvable binary must refuse the launch here —
    // before anything is planned or persisted — never degrade to agent improvisation.
    if let Some(def) = &selected_def {
        crate::workflow::preflight_tool_phases(def)?;
    }
    let mut units = match &selected_def {
        Some(def) => plan::plan_from_def(def, problem, session_id),
        None => plan::plan_units(problem, session_id),
    };
    // Bind THIS run's repo into the placeholders its Tool phases declare, before anything is
    // persisted. The def is shared by every run of its id; the paths are not. Rewriting a shared def
    // per launch instead is what made three concurrent registrations index one repo's tree into one
    // repo's database under three different names (FINDING-075, wicked-crew#196).
    if let Some(repo_id) = repo_ref.as_deref() {
        if let Some(repo) = crate::repo::get_repo(store, repo_id)? {
            plan::bind_repo_paths(&mut units, &repo);
        }
    }
    // Refuse rather than dispatch a command carrying a literal `{repo_root}`. Reached when a def
    // declaring repo placeholders is launched with no `repo_ref`, or with one that no longer
    // resolves — both of which would otherwise hand a tool a path that cannot exist, and hand a tool
    // that treats an unknown path as "use the cwd" the FINDING-067 shape.
    let unbound = plan::unbound_repo_tokens(&units);
    if !unbound.is_empty() {
        // The RESOLVED id, not the caller's argument. A run can reach a def without naming one, and
        // an error reading "workflow `<none>` declares placeholders" tells an operator nothing about
        // which def to go look at.
        let named = selected_def
            .as_ref()
            .map(|d| d.id.as_str())
            .or(workflow)
            .unwrap_or("<none>");
        anyhow::bail!(
            "workflow `{named}` declares repo placeholders that this run cannot fill ({}); it must \
             be launched against a registered repo — pass `repoRef`",
            unbound.join(", ")
        );
    }
    if units.len() as u32 > crate::actor::DENY_PHASE_SPAN {
        anyhow::bail!(
            "run has {} units, exceeding the {}-unit governed limit; split the problem into smaller runs",
            units.len(),
            crate::actor::DENY_PHASE_SPAN
        );
    }

    if let Some(def) = &selected_def {
        // The built-in floors are seeded HERE, at the plan, and not only at actor boot.
        //
        // `attach_pinned_validators` is fail-closed on a pin that is not in the vault, and the
        // shipped `feature`/`bug`/`migration` defs now pin the evidence floor. Seeding only at boot
        // made that correct for the daemon and BROKEN for everyone else: `run_session` is public and
        // takes a store directly, so an embedder — or the engine's own `pipeline` test — opened a
        // fresh store, planned a SHIPPED workflow, and got a hard bail naming a pin they never
        // wrote. A built-in floor that depends on which entry point you came through is not a floor.
        //
        // This is the one choke point both paths cross (actor launch and `run_session`), the writes
        // are content-addressed upserts, and the pin is a compile-time constant — so it is idempotent
        // and costs two `put_node`s per plan. The boot-time seed stays as the loud early warning and
        // to make the floor visible in the vault before a first run; this is the invariant.
        crate::builtin_floors::seed_builtin_floors(store)?;
        // The shipped `domain-extraction` drop-in's coverage validator is seeded HERE for the same
        // reason, and it was NOT — which cost an operator a closed loop (FINDING-066).
        //
        // It is the same class of object as the floor above: hand-authored, deterministic,
        // content-addressed, shipped with the product, and pinned by a def we ship. The only
        // difference was where it got vaulted — the floor on the plan path, this one only via an
        // out-of-band `wicked-core seed-domain-validators`. That difference is not survivable,
        // because THE VAULT IS PER-DATABASE and the CLI's default database is not the engine's:
        // crew's daemon opens `~/.wicked-crew/core.db`, the CLI falls back to a cwd-relative
        // `wicked-estate.db`. Measured: the run failed naming this pin, the prescribed command ran
        // and printed the matching pin, and the relaunch failed identically — the seed had landed in
        // a database nothing reads. An error whose remedy is inert is worse than an unclear one; the
        // operator has no signal that they are looping.
        //
        // Seeding it here removes the out-of-band step from the critical path entirely, so no
        // database can be the wrong one. Same cost argument as the floor: two content-addressed
        // `put_node`s that collapse onto themselves, and the pin is a compile-time constant.
        // `seed-domain-validators` survives as a visibility/repair tool, not a prerequisite.
        crate::domain_extraction::provision_and_approve_coverage_validator(store)?;
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
        extra_write_roots,
        project_graph,
        archived_at: None,
        archive_note: None,
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
        let (routing_method, agreement_pct, returned, seated, dissent, degraded_reason) =
            match &dist.routing {
                RoutingInfo::Council {
                    agreement_pct,
                    returned,
                    seated,
                    dissent,
                    ..
                } => (
                    "council".to_string(),
                    Some(*agreement_pct),
                    Some(*returned),
                    // Already an Option — an unknown seat count stays unknown on the wire rather
                    // than being flattened into a number no one measured.
                    *seated,
                    Some(*dissent),
                    None,
                ),
                RoutingInfo::Degraded { reason } => (
                    "degraded".to_string(),
                    None,
                    None,
                    None,
                    None,
                    Some(reason.clone()),
                ),
                RoutingInfo::EvaluatorDistinct { .. } => (
                    "evaluator_distinct".to_string(),
                    None,
                    None,
                    None,
                    None,
                    None,
                ),
                RoutingInfo::Tool => ("tool".to_string(), None, None, None, None, None),
            };
        emit(CoreEvent::UnitDistributed {
            session: pre.session_id.clone(),
            ord: u.ord,
            cli: dist.assigned_cli.clone(),
            routing_method,
            agreement_pct,
            returned,
            seated,
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
    extra_write_roots: Vec<String>,
    project_graph: Option<crate::project::ProjectGraphBinding>,
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
        extra_write_roots,
        project_graph,
        workflow,
        emit,
        workflow_registry,
        session_already_started,
        governed,
    )?;
    let distributions =
        distribute::distribute_units_on(&pre.units, clis, session_id, None, dispatcher, None)?;
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

    // ── PHASE-SCOPE OBSERVABILITY (core#283, the completion-path backstop) ── A pre-build,
    // non-creator phase whose worktree contribution touches NON-documentation files jumped the
    // design-before-build ladder. Enforcement lives in `gate_hook::phase_scope_denial` (core#296),
    // which refuses the path-bearing WRITE tools at call time; the plan-time preamble is only the
    // PROMPT half and enforces nothing. This stays because the gate cannot see a `Bash` heredoc.
    // Record a WARNING onto the unit's persisted gate evidence, visible to operators, and DO NOT
    // deny: the check is a heuristic over file names, and a deny would turn it into a gate. The
    // unit (with the warning) is persisted by the `put_node` below, so the evidence rides the same
    // write as the gate's own resolution. Dedup guards a retried attempt from stacking copies.
    if let Some(warning) = crate::actor::phase_scope_warning(unit, workdir.as_deref()) {
        // Log only when NEWLY recorded — a retried attempt re-derives the same warning and
        // would otherwise spam identical lines while the evidence stays unchanged (Copilot
        // review on #287).
        if !unit.scope_warnings.contains(&warning) {
            eprintln!("wicked-core: {warning}");
            unit.scope_warnings.push(warning);
        }
    }

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
    // Was EVAL_AT_BASE + ord + 1_000_000 — a timestamp field used as a namespace (FINDING-017).
    let eval_at = crate::clock::eval_now();
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
    // (FINDING-025) The policies that second pass actually applied. `evaluator_pass` is vacuously
    // true when none did — the policy engine runs on every unit and default-allows on an empty
    // selection — so this is what lets a consumer tell an ENFORCED pass from an UNGATED one.
    let evaluator_policies = eval
        .as_ref()
        .map(|e| e.policies.clone())
        .unwrap_or_default();
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
        evaluator_policies,
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
        Ok((outcome, _)) => denial_for_outcome(&outcome, &v.criterion, cwd),
        Err(e) => Some(format!("pinned validator error: {e}")),
    }
}

/// Render the operator-facing denial for a re-verify outcome (FINDING-050). Every outcome except
/// [`ValidatorOutcome::Passed`] denies — the fail-closed rule is unchanged and is NOT what this
/// distinguishes. What it distinguishes is the CLAIM: only `Failed` means the script ran and judged the
/// criterion false. `TimedOut` and `Unrunnable` mean no verdict was ever reached, and reporting those as
/// a criterion failure sends the operator to audit a diff when the fault is in their host.
///
/// Split out from [`pinned_validator_denial`] so the wording of each arm is directly testable without
/// having to provoke a real 120s timeout or a real missing shell.
/// The measurement a failed validator left behind, rendered compactly — or `None`.
///
/// FINDING-092: a criterion is a CONJUNCTION ("at least one behavior-bearing node, AND
/// resolved-or-flagged coverage == 1.0 over them"), and restating it says nothing about which
/// conjunct failed. Two runs produced byte-identical denials while measuring completely different
/// things:
///
/// ```text
/// behavior_bearing 0    coverage 1.0     <- the gate read the WRONG STORE (FINDING-091)
/// behavior_bearing 766  coverage 0.171   <- the gate read the right one, 17% covered
/// ```
///
/// The first is "your extraction produced nothing"; the second is "your extraction covered 17%".
/// Different problems, different next actions, same message — and the CRITICAL defect behind the
/// first hid in that ambiguity for three runs.
///
/// The numbers are already on disk when the denial is rendered. This reads them rather than
/// recomputing, so the message reports what the validator ACTUALLY gated on.
fn failing_measurement(cwd: &std::path::Path) -> Option<String> {
    // FINDING-099: this read `.wicked/domain/coverage-report.json`, a path nothing else in the
    // system writes. It therefore never found the report, and every legitimate coverage denial was
    // reported as "no coverage report was produced" — the floor blamed for not measuring when it
    // had measured and the number was bad. Read the one name all four artifacts agree on.
    let raw =
        std::fs::read_to_string(cwd.join(crate::domain_extraction::COVERAGE_REPORT_FILE)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let n = |k: &str| v.get(k).and_then(serde_json::Value::as_u64);
    let f = |k: &str| v.get(k).and_then(serde_json::Value::as_f64);
    // Only the fields the criterion actually turns on. A dump of the whole report would bury the
    // one number the operator needs.
    Some(format!(
        "measured: behavior_bearing={}, resolved={}, risk_flagged={}, unaccounted={}, coverage={}",
        n("behavior_bearing")?,
        n("resolved")?,
        n("risk_flagged")?,
        n("unaccounted")?,
        f("coverage")?
    ))
}

fn denial_for_outcome(
    outcome: &crate::validator::ValidatorOutcome,
    criterion: &str,
    cwd: &std::path::Path,
) -> Option<String> {
    use crate::validator::ValidatorOutcome as O;
    match outcome {
        O::Passed => None,
        O::Failed => Some(match failing_measurement(cwd) {
            // Naming the measurement is the point: without it a wrong-store read and a genuine
            // shortfall are indistinguishable (FINDING-092).
            Some(m) => format!("pinned validator failed: {criterion} — {m}"),
            // No report means the script denied before producing one; say so rather than implying
            // a measurement happened.
            None => format!(
                "pinned validator failed: {criterion} (no coverage report was produced, so the \
                 criterion could not be measured — the script denied before writing one)"
            ),
        }),
        O::TimedOut => Some(format!(
            "pinned validator TIMED OUT before reaching a verdict (fail-closed — a phase that cannot \
             be re-verified is treated as NOT-passed, so this is a DENY, not a criterion failure). \
             The criterion `{}` was never evaluated; the script was killed at the {}s bound. Check \
             the script for a command that waits on input or the network.",
            criterion,
            crate::validator::VALIDATOR_TIMEOUT.as_secs()
        )),
        O::Unrunnable(e) => Some(format!(
            "pinned validator COULD NOT BE RUN on this host (fail-closed — a phase that cannot be \
             re-verified is treated as NOT-passed, so this is a DENY, not a criterion failure). The \
             criterion `{criterion}` was never evaluated: {e}. The usual cause is resolution: the \
             script runs as `sh -c` under a cleared environment that keeps only a fixed allowlist, so \
             `sh` and anything the script calls must be found on the inherited PATH. Read the OS error \
             above first — it is the authority on what actually failed."
        )),
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
mod denial_message_tests {
    use super::*;
    use crate::validator::ValidatorOutcome as O;

    const CRITERION: &str = "the run left a change in its worktree";

    /// FINDING-050. A distinguishable enum is worth nothing if the operator still reads one sentence.
    /// Asserts the property, not the prose: a no-verdict outcome must never be phrased as the criterion
    /// having failed, must say the criterion went unevaluated, and must name its own cause.
    /// FINDING-092: a criterion is a CONJUNCTION, so restating it cannot say which conjunct
    /// failed. Two campaign runs produced BYTE-IDENTICAL denials while measuring completely
    /// different things — one had read the wrong database entirely (FINDING-091), and that
    /// CRITICAL defect hid in the ambiguity for three runs.
    ///
    /// The invariant is DISTINGUISHABILITY, not wording: an operator must be able to tell "your
    /// extraction produced nothing" from "your extraction covered 17%".
    #[test]
    fn a_failed_denial_distinguishes_no_data_from_a_real_shortfall() {
        fn denial_for(report: &str) -> String {
            let dir =
                std::env::temp_dir().join(format!("den_{}_{}", std::process::id(), report.len()));
            // FINDING-099: this fixture used to write `.wicked/domain/coverage-report.json` — the
            // SAME wrong path the implementation read. Test and code shared one mistake, so the
            // test passed while the behaviour it asserted never occurred in production. Writing
            // through the shared constant is what stops a fixture from agreeing with a defect.
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(crate::domain_extraction::COVERAGE_REPORT_FILE),
                report,
            )
            .unwrap();
            let msg = denial_for_outcome(&O::Failed, CRITERION, &dir).expect("denies");
            let _ = std::fs::remove_dir_all(&dir);
            msg
        }

        // The two states the old message could not tell apart.
        let wrong_store = denial_for(
            r#"{"behavior_bearing":0,"resolved":0,"risk_flagged":0,"unaccounted":0,"coverage":1.0}"#,
        );
        let real_shortfall = denial_for(
            r#"{"behavior_bearing":766,"resolved":0,"risk_flagged":131,"unaccounted":635,"coverage":0.171}"#,
        );

        assert_ne!(
            wrong_store, real_shortfall,
            "a nothing-measured denial and a 17%-covered denial must not read identically — this \
             is FINDING-092, and it is how FINDING-091 stayed hidden"
        );
        assert!(
            wrong_store.contains("behavior_bearing=0"),
            "must name the measurement: {wrong_store}"
        );
        assert!(
            real_shortfall.contains("behavior_bearing=766")
                && real_shortfall.contains("coverage=0.171"),
            "must name the measurement: {real_shortfall}"
        );
    }

    #[test]
    fn a_no_verdict_outcome_is_never_worded_as_a_criterion_failure() {
        assert_eq!(
            denial_for_outcome(&O::Passed, CRITERION, std::path::Path::new("/nonexistent")),
            None,
            "pass ⇒ no denial"
        );

        let failed =
            denial_for_outcome(&O::Failed, CRITERION, std::path::Path::new("/nonexistent"))
                .expect("denies");
        assert!(
            failed.starts_with(&format!("pinned validator failed: {CRITERION}")),
            "the genuine criterion failure keeps its established wording: {failed}"
        );

        let timed_out = denial_for_outcome(
            &O::TimedOut,
            CRITERION,
            std::path::Path::new("/nonexistent"),
        )
        .expect("denies");
        let unrunnable = denial_for_outcome(
            &O::Unrunnable("No such file or directory (os error 2)".into()),
            CRITERION,
            std::path::Path::new("/nonexistent"),
        )
        .expect("denies");

        for (name, msg) in [("TimedOut", &timed_out), ("Unrunnable", &unrunnable)] {
            assert_ne!(
                msg, &failed,
                "{name} must not render as the criterion-failure message"
            );
            assert!(
                msg.contains("never evaluated"),
                "{name} must say the criterion went unevaluated, not that it was judged false: {msg}"
            );
        }

        assert!(
            timed_out.contains("TIMED OUT")
                && timed_out.contains(&crate::validator::VALIDATOR_TIMEOUT.as_secs().to_string()),
            "a timeout must name itself and the bound it hit: {timed_out}"
        );
        assert!(
            unrunnable.contains("COULD NOT BE RUN")
                && unrunnable.contains("No such file or directory")
                && unrunnable.contains("PATH"),
            "an unrunnable check must carry the OS cause and point at PATH: {unrunnable}"
        );
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

    // ── FINDING-099: the denial must name the number it measured ────────────────────────────
    //
    // `failing_measurement` read `.wicked/domain/coverage-report.json`, a path nothing else in this
    // system writes. It never found the report, so every LEGITIMATE coverage denial claimed the
    // floor "could not measure" — blaming the engine for a bad number it had measured correctly,
    // and making the denial indistinguishable from FINDING-093's genuinely inert floor.

    /// P1 rule, applied without touching the content-addressed script: the reader and the script
    /// must agree on the filename, and this asserts BOTH artifacts rather than restating either.
    #[test]
    fn the_script_and_the_diagnostic_agree_on_the_report_filename() {
        assert!(
            crate::domain_extraction::COVERAGE_SCRIPT
                .contains(crate::domain_extraction::COVERAGE_REPORT_FILE),
            "the shipped coverage script does not mention {} — the diagnostic would read a file \
             the script never produces",
            crate::domain_extraction::COVERAGE_REPORT_FILE
        );
    }

    /// The behaviour that actually broke. Falsified by restoring the nested path: this fails.
    #[test]
    fn a_failing_report_is_read_from_the_worktree_root() {
        let dir = std::env::temp_dir().join(format!("wicked-cov-{}-root", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join(crate::domain_extraction::COVERAGE_REPORT_FILE),
            r#"{"behavior_bearing":5769,"resolved":128,"risk_flagged":0,"unaccounted":5641,"coverage":0.0222}"#,
        )
        .expect("write report");

        let m = failing_measurement(&dir).expect("the report at the worktree root must be read");
        assert!(m.contains("behavior_bearing=5769"), "{m}");
        assert!(m.contains("unaccounted=5641"), "{m}");
        assert!(m.contains("coverage=0.0222"), "{m}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half: a report ONLY at the old nested path must NOT be found, so a future edit
    /// cannot quietly satisfy the test above by reading both locations. Reading two paths would
    /// reintroduce the ambiguity this fix removes.
    #[test]
    fn the_abandoned_nested_path_is_not_consulted() {
        let dir = std::env::temp_dir().join(format!("wicked-cov-{}-nested", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".wicked/domain")).expect("temp dir");
        std::fs::write(
            dir.join(".wicked/domain").join(crate::domain_extraction::COVERAGE_REPORT_FILE),
            r#"{"behavior_bearing":1,"resolved":1,"risk_flagged":0,"unaccounted":0,"coverage":1.0}"#,
        )
        .expect("write report");

        assert!(
            failing_measurement(&dir).is_none(),
            "the nested path is not where anything writes; consulting it invites two spellings again"
        );
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

    /// A SHIPPED workflow must plan against a store nobody seeded.
    ///
    /// Regression, and a sharp one: the evidence floor was seeded at actor boot only, so the daemon
    /// worked and every other caller broke. `run_session` is public and takes a store directly — an
    /// embedder opening a fresh store and asking for the shipped `feature` workflow got a fail-closed
    /// bail naming a pin they had never heard of. The test above uses a HAND-BUILT def and vaults its
    /// validator by hand, which is precisely why it did not catch this.
    ///
    /// This one drives `pre_distribute` — the actual function that was fixed — rather than replaying
    /// the seed and attach calls itself. Replaying them tests that the two functions compose, which
    /// was never in doubt; the defect was that the plan path did not CALL one of them, and a test
    /// that makes the call itself cannot observe that. Delete the seed from `pre_distribute` and this
    /// test fails; that is the whole point of it.
    #[test]
    fn a_shipped_def_plans_against_a_store_nobody_seeded() {
        use wicked_apps_core::open_store;
        let dir = std::env::temp_dir().join(format!("wicked-unseeded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = open_store(Some(dir.join("v.db").to_str().unwrap())).unwrap();

        let registry = crate::workflow::WorkflowRegistry::with_defaults();
        let pinned = registry
            .get("feature")
            .expect("the shipped feature def")
            .phases
            .iter()
            .filter(|p| p.validator_pin.is_some())
            .count();
        assert!(
            pinned > 0,
            "this test is only meaningful while a shipped def pins a floor"
        );

        // No CLIs: `pre_distribute` only counts them and copies the keys onto the session — it does
        // not distribute (that is the caller's thread). Nothing here needs a seat.
        let pre = pre_distribute(
            &mut store,
            &[],
            "do it",
            EntityMode::Isolated,
            "s-unseeded",
            crate::domain::HumanConfirm::None,
            None,
            None,
            Vec::new(),
            None,
            Some("feature"),
            &mut |_| {},
            Some(&registry),
            false,
            false,
        )
        .expect("a shipped def must never bail on its own built-in floor");

        let gated = pre
            .units
            .iter()
            .filter(|u| u.validator.as_ref().is_some_and(|v| v.approved))
            .count();
        assert_eq!(
            gated, pinned,
            "every pinned phase came back with an APPROVED validator attached"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDING-066. The sibling of the test above, for the shipped DROP-IN rather than the built-ins —
    /// and it was the missing half. `domain-extraction.json` pins the coverage validator, but nothing on
    /// the plan path vaulted it; the operator had to run `wicked-core seed-domain-validators` out of
    /// band. That is not a "one extra step", because the vault is rows in a database and the CLI's
    /// default database is not a daemon's: measured against the crew daemon, the command succeeded,
    /// printed the exact pin the run had asked for, and the relaunch failed identically — the seed had
    /// landed in `$CWD/wicked-estate.db` while the engine read `~/.wicked-crew/core.db`.
    ///
    /// A fresh store here stands in for "whichever database the engine happened to open". Delete the
    /// coverage seed from `pre_distribute` and this fails with the pin-not-in-vault bail.
    #[test]
    fn the_shipped_drop_in_plans_against_a_store_nobody_seeded() {
        use wicked_apps_core::open_store;
        let dir =
            std::env::temp_dir().join(format!("wicked-dropin-unseeded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = open_store(Some(dir.join("v.db").to_str().unwrap())).unwrap();

        // The real shipped JSON, loaded exactly as an operator's overlay dir would load it — not a
        // hand-built def. The pin under test is the one that actually ships.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("workflows")
            .join("domain-extraction.json");
        let def = crate::workflow::WorkflowRegistry::def_from_file(&path)
            .expect("domain-extraction.json parses + validates");
        let coverage_pin = def
            .phases
            .iter()
            .find(|p| p.id == "coverage")
            .and_then(|p| p.validator_pin.clone())
            .expect("this test is only meaningful while the coverage phase carries a pin");
        assert_eq!(
            coverage_pin,
            crate::domain_extraction::COVERAGE_VALIDATOR_PIN,
            "the shipped JSON pins the const the plan path seeds"
        );
        let id = def.id.clone();
        let mut registry = crate::workflow::WorkflowRegistry::with_defaults();
        registry.register(def).expect("drop-in registers");

        // domain-graph is now a deterministic `wicked-core domain-graph --db {code_graph_db}` Tool
        // (core#237 persist fix), so the workflow legitimately requires a bound repo to fill that
        // placeholder — a domain graph cannot persist without a repo store. Register one at the
        // scratch dir; `pre_distribute` only `get_repo`+`bind_repo_paths` at plan time (no worktree/
        // git). The PIN-resolution assertion below — the seed-in-the-wrong-db regression this test
        // actually guards — is unchanged.
        // register_repo validates the root is a git repo WITH ≥1 commit (a worktree needs a base).
        // Two direct `git` spawns (cross-platform; `sh -c` would not run on Windows CI): init, then
        // an empty base commit with a LOCAL identity so no global config or signing is required.
        assert!(
            // spawn-audit: test-only — fixture setup, never spawned in production.
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&dir)
                .status()
                .expect("spawn git init")
                .success(),
            "git init the scratch repo must succeed"
        );
        assert!(
            // spawn-audit: test-only — fixture setup, never spawned in production.
            std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@wicked.test",
                    "-c",
                    "user.name=wicked",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--allow-empty",
                    "-qm",
                    "base",
                ])
                .current_dir(&dir)
                .status()
                .expect("spawn git commit")
                .success(),
            "git base commit must succeed"
        );
        let repo = crate::repo::register_repo(
            &mut store,
            crate::repo::RepoSpec {
                name: "dropin-unseeded-repo".to_string(),
                root_path: dir.to_string_lossy().into_owned(),
                registered_at: 0,
            },
        )
        .expect("register a scratch repo so {code_graph_db} binds");

        let pre = pre_distribute(
            &mut store,
            &[],
            "extract the domain",
            EntityMode::Isolated,
            "s-dropin-unseeded",
            crate::domain::HumanConfirm::None,
            Some(repo.id.clone()),
            None,
            Vec::new(),
            None,
            Some(&id),
            &mut |_| {},
            Some(&registry),
            false,
            false,
        )
        .expect("a shipped drop-in must never require an out-of-band seed to plan");

        // Attached AND approved — a pin that resolved to an unapproved validator would plan fine here
        // and then deny every run at gate time, which is the failure this must not silently become.
        let gated = pre
            .units
            .iter()
            .filter(|u| {
                u.validator
                    .as_ref()
                    .is_some_and(|v| v.approved && crate::validator_vault::pin(v) == coverage_pin)
            })
            .count();
        assert_eq!(
            gated, 1,
            "the coverage phase came back carrying the APPROVED shipped pin"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDING-066. Naming a command is only half a remedy when the vault is per-database, so every
    /// arm of the refusal must say which database to seed. Pinned as a property of the function rather
    /// than of one message, because the generic arm is the live one and it was the arm with no hint at
    /// all — an operator running `provision-validator` in the wrong cwd hits the identical dead end.
    #[test]
    fn every_missing_pin_remedy_names_the_database() {
        for pin in [
            crate::domain_extraction::COVERAGE_VALIDATOR_PIN,
            "deadbeefdeadbeef",
        ] {
            let remedy = missing_pin_remedy(pin);
            assert!(
                remedy.contains("--db"),
                "remedy for `{pin}` must name the db flag, got: {remedy}"
            );
            assert!(
                remedy.contains("WICKED_ESTATE_DB"),
                "remedy for `{pin}` must name the env fallback, got: {remedy}"
            );
        }
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

    /// A fail-closed refusal has to leave the operator with a command to run. The shipped coverage
    /// pin has a purpose-built one, and the generic author→approve pair cannot reproduce it — so
    /// sending them to `provision-validator` for that pin would be actively misleading.
    #[test]
    fn the_shipped_coverage_pin_is_pointed_at_its_own_seed_command() {
        let remedy = missing_pin_remedy(crate::domain_extraction::COVERAGE_VALIDATOR_PIN);
        assert!(
            remedy.contains("wicked-core seed-domain-validators"),
            "the shipped pin must name its purpose-built seed command: {remedy}"
        );
        assert!(
            !remedy.contains("provision-validator"),
            "the LLM writer path authors a different script and so a DIFFERENT pin — pointing at it \
             here sends the operator somewhere that cannot produce this pin: {remedy}"
        );
    }

    /// Any other unresolved pin is an operator-authored one: name the author→approve pair, and name
    /// it as a CLI (the UNAPPROVED sibling arm already does — this arm used to name only the Rust API).
    #[test]
    fn an_unknown_pin_is_pointed_at_the_author_approve_pair() {
        let remedy = missing_pin_remedy("deadbeefdeadbeef");
        assert!(
            remedy.contains("wicked-core provision-validator"),
            "{remedy}"
        );
        assert!(remedy.contains("wicked-core approve-validator"), "{remedy}");
        assert!(
            remedy.contains("deadbeefdeadbeef"),
            "the message must name the pin that failed to resolve: {remedy}"
        );
    }
}
