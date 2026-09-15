# DES-BC79-001 — Thread the run's studio project into the worker env (`WICKED_RUN_PROJECT`)

**Status:** implemented (core half). **Register:** BC-79 (approved, des-adjudicated §2). **Scope:** wicked-core only.

## Problem

A governed run's studio project id (`LaunchSpec::project_id`, e.g. `proj_…` / `wicked-platform`) was
used ONLY for membership filing (`actor::attach_member`) and repo-graph selection. It was never
persisted on the run's session, never reachable at the per-unit worker-env build, and never stamped
onto the worker env. So a governed worker (e.g. `capture-learnings`) that submits estate proposals
had no signal for which project it belongs to and self-derived `facets.project`, landing on the
state-home default (`something-wicked`) instead of the run's project. Approved memories were then
mis-scoped for later recall.

Provenance channel before: `estate_provenance_env` stamped `WICKED_RUN_ID` / `WICKED_RUN_UNIT` /
`WICKED_RUN_AGENT` only.

## Seams (verified at HEAD 2b136f2)

- `LaunchSpec::project_id` — `src/lib.rs:223` (membership only, `actor.rs` project-member attach).
- `AgentSession` — `src/domain.rs:77` — carried `project_graph` (a code-graph binding) but **no
  project id**.
- Per-unit dispatch builds `StepInput` from the persisted `session` — `src/actor.rs` (`GovernanceContext` closure).
- Worker env stamped by `estate_provenance_env` — `src/execute_wrapped.rs` — RUN_ID/UNIT/AGENT only.
  Called by both carriers: `execute_wrapped.rs` (wrapped `exec`) and `acp_runner.rs` (ACP `build_cmd`).

## Change (one mechanism, deterministic)

1. **Persist** `AgentSession::project_id: Option<String>` (`#[serde(default)]`), beside
   `project_graph`, set at launch from `LaunchSpec::project_id` and threaded through
   `pre_distribute` / `plan_and_distribute`. Persisted for the same reason `project_graph` is: a
   resume/redrive re-enters with no `LaunchSpec`, so an id held only in launcher memory would
   silently unscope a half-finished run's proposals.
2. **Carry to the worker-env build** on `GovernanceContext::project_id` (`#[serde(default)]`) — the
   per-unit governance context that already carries `code_graph_db` (the run's *project* code graph)
   and is a `StepInput` field. It is `Some` exactly for a governed unit (the only kind that submits
   proposals). Set from `session.project_id` in the actor's `StepInput` build.
3. **Stamp** `estate_provenance_env(run, unit, agent, project)` appends `WICKED_RUN_PROJECT = <id>`
   WHEN the project is present and non-blank; both carriers pass
   `input.governance.as_ref().and_then(|g| g.project_id.as_deref())`. Absent/blank ⇒ nothing stamped.

### Why `GovernanceContext` rather than a bare `StepInput` field
`GovernanceContext` is the natural, lower-churn home: it already scopes the worker's estate tools
(`code_graph_db`), it survives the `DispatchedTask` bus round-trip (`Serialize`/`Deserialize` +
`#[serde(default)]`), and it is present precisely for governed proposal-submitting units. Threading
via it still satisfies "reachable at the per-unit `StepInput`/worker-env build" (it is a `StepInput`
field). A repo-only run has no project and any ungoverned unit has no `GovernanceContext`, so both
correctly stamp nothing.

## No-regression contract
- No project (repo-only run) ⇒ no `WICKED_RUN_PROJECT`; the three existing markers are byte-identical.
- Ungoverned engine-internal calls (`governance: None`) ⇒ nothing stamped.
- Additive `#[serde(default)]` fields ⇒ older serialized sessions / `DispatchedTask`s deserialize as
  `None`.

## Inert until the garden reader lands
Nothing reads `WICKED_RUN_PROJECT` yet. Garden's estate shim gains the reader (into `facets.project`)
in a **separate BC-79 garden PR**. Until then this stamp is a behavior no-op — safe to land alone.

## Tests
`estate_provenance_env` present/absent/blank; and a `StepInput` → env seam test exercising the exact
carrier extraction (governed+project ⇒ stamped; governed+no-project ⇒ none; ungoverned ⇒ none).
