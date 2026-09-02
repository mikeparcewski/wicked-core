//! PLAN — deterministic decomposition of a problem into ordered work units.
//! Two planners, both pure and deterministic (no randomness, no model):
//!   * [`plan_units`] — free-text: splits a prose problem on newlines / sentence terminators /
//!     semicolons and *classifies* each piece's stage by keyword. The legacy path.
//!   * [`plan_from_def`] — data-driven: derives one unit per [`WorkflowDef`] phase, taking each
//!     unit's [`StageKind`] from the phase's declared `kind` (never a keyword guess); the backing
//!     phase is encoded in the unit id (`<session>:<phase_id>`). The plan is a function of workflow
//!     *data* (Law 2), so a new workflow changes the plan without touching this module.

use crate::domain::WorkUnit;
use crate::workflow::WorkflowDef;

/// Separator folding a phase's own instructions onto its unit description (FINDING-011).
///
/// SINGLE-LINE by contract. The description IS the worker's prompt, and the PTY session runner
/// submits a turn on the FIRST newline (`session_runner` writes `{prompt}\n` to an interactive,
/// line-based PTY). A `\n`-joined description would submit only the `<phase> — <intent>` head — the
/// near-identical prompt this finding set out to kill — and strand the instructions as a stray
/// follow-up line that desyncs the reused session's result sentinel. So the fold uses the same
/// ` ||| ` segment marker `execute_wrapped`'s `LAYOUT_PREFIX` and `assumptions::PROMPT_CONVENTION`
/// already use for exactly this reason (both documented single-line-by-contract). Two guards keep
/// it honest: `folded_instructions_never_introduce_a_newline_into_the_prompt` here, and the
/// call-site `pty_unit_prompt` refusal in `execute_wrapped`.
const INSTRUCTION_SEP: &str = " ||| ";

/// The recognizable head of the engine-side scope preamble (core#283) — a const so the tests that
/// assert its presence/absence and any operator grepping a prompt share one spelling.
///
/// It used to read `PHASE SCOPE (enforced):` and that word was FALSE (core#296): a prompt cannot
/// enforce itself, and run `d1bc72c2` proved it — a `design` unit carrying this exact preamble wrote
/// `src/board/attentionReason.ts` before the build phase ran, and the governance hook watched both
/// writes go by with `decision=allow`. A prompt string that claims enforcement is worse than one
/// that doesn't, because the claim is what stops anyone looking for the missing gate. The word is
/// gone from the prompt and the enforcement now lives where enforcement can live: the gate refuses
/// a pre-build phase's non-documentation Write/Edit before the tool call runs — see
/// [`crate::gate_hook::phase_scope_denial`], reached from `evaluate_tool_call` on both carriers.
pub(crate) const PHASE_SCOPE_PREFIX: &str = "PHASE SCOPE:";

/// The scope preamble injected into the UNIT PROMPT of every PRE-BUILD, non-creator phase
/// (core#283). Phase-role scope was a suggestion: a Neutral pre-build phase (e.g. `feature`'s
/// `design`) received the same problem statement as every phase and routinely implemented the
/// entire deliverable, collapsing the design-before-build ladder — proven twice, including against
/// an explicit prompt-level discipline paragraph, so this is injected ENGINE-SIDE from def data
/// (role + `executes_code` + declaration order), never hardcoded per workflow id and never left to
/// workflow prose. Single-line by construction: the description IS the PTY prompt
/// (see [`INSTRUCTION_SEP`]).
///
/// This is the PROMPT half and nothing more — it TELLS the worker the scope, it does not hold it to
/// it (core#296). The holding is [`crate::gate_hook::phase_scope_denial`], which refuses the
/// non-documentation write itself; the deny message names the same rule this sentence states, so a
/// worker that ignores the prompt still gets a legible reason at the tool call.
fn phase_scope_preamble(phase_id: &str) -> String {
    format!(
        "{PHASE_SCOPE_PREFIX} this is the {phase_id} phase. Produce this phase's deliverable only \
         (analysis/design/plan as applicable). Do NOT write or commit production code; \
         implementation belongs to a later phase."
    )
}

/// Decompose `problem` into ordered [`WorkUnit`]s owned by `session_id`. Unit ids are
/// `<session_id>:u<ord>` (1-based, stable).
pub fn plan_units(problem: &str, session_id: &str) -> Vec<WorkUnit> {
    let pieces = split_problem(problem);
    let descriptions: Vec<String> = if pieces.is_empty() {
        let trimmed = problem.trim();
        vec![if trimmed.is_empty() {
            "unit".to_string()
        } else {
            trimmed.to_string()
        }]
    } else {
        pieces
    };

    descriptions
        .into_iter()
        .enumerate()
        .map(|(i, description)| {
            let ord = (i + 1) as u32;
            WorkUnit::pending(format!("{session_id}:u{ord}"), session_id, ord, description)
        })
        .collect()
}

/// Decompose a run into ordered [`WorkUnit`]s from a [`WorkflowDef`] — one unit per phase, in the
/// def's phase order. Unlike [`plan_units`], the stage is taken from each phase's declared `kind`
/// (data-driven, not keyword-classified). `intent` is the run's problem statement; each unit's
/// description scopes that intent to its phase so the gate gets meaningful `work` context. Unit ids
/// are `<session_id>:<phase_id>` (stable across resumes) — that id is the backing-phase linkage;
/// `phase_ref` is left untouched (the execute path owns it).
pub fn plan_from_def(def: &WorkflowDef, intent: &str, session_id: &str) -> Vec<WorkUnit> {
    // Precondition: `def` is validated — phase ids are unique, so `<session>:<phase_id>` unit ids
    // are collision-free. The registry only ever hands out validated defs (`register` validates),
    // so the runtime path upholds this; the assert catches a raw unvalidated def in dev.
    debug_assert!(
        {
            let mut seen = std::collections::HashSet::new();
            def.phases.iter().all(|p| seen.insert(p.id.as_str()))
        },
        "plan_from_def requires a validated def (unique phase ids); call WorkflowDef::validate first"
    );
    let intent = intent.trim();
    // core#283: the index where implementation legitimately begins — the def's first
    // `executes_code` Creator phase. Phases BEFORE it that play neither creator nor evaluator are
    // the pre-build ladder (clarify/design/plan …): their prompts gain the scope preamble and
    // their units are marked `pre_build_scope` so the completion path can WARN when one implements
    // anyway. `None` (a def with no code-executing creator, e.g. `collab`, `survey-repo`) ⇒ there
    // is no ladder to protect and no phase gets the preamble.
    let first_code_creator = def
        .phases
        .iter()
        .position(|p| p.executes_code && p.role == crate::workflow::PhaseRole::Creator);
    def.phases
        .iter()
        .enumerate()
        .map(|(i, phase)| {
            let ord = (i + 1) as u32;
            let mut description = if intent.is_empty() {
                phase.id.clone()
            } else {
                format!("{} — {intent}", phase.id)
            };
            // FINDING-011: fold the phase's own INSTRUCTIONS into the description. The description
            // IS the worker's prompt (`execute_wrapped::skill_prompt` sends it bare on the authored
            // path), so without this every phase of a multi-phase workflow gets a prompt that
            // differs only by the phase-id token — N recon phases each re-survey the whole intent.
            // Appended after the intent so the shared goal still leads and the phase's slice of it
            // follows; a phase with no instructions keeps the historical prompt byte-exact.
            // Joined with a SINGLE-LINE separator (`INSTRUCTION_SEP`): a `\n` here would be submitted
            // by the line-based PTY runner as an early turn end, sending only the head and stranding
            // the instructions — the very failure this fold exists to remove, reintroduced.
            if let Some(instr) = phase
                .instructions
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                description.push_str(INSTRUCTION_SEP);
                description.push_str(instr);
            }
            // core#283 PHASE-ROLE SCOPE, the PROMPT half (core#296 — it was called "the enforced
            // half" here, and it enforced nothing; the enforcement is the gate, below). Prompt-level
            // discipline proved insufficient twice, so the planner itself injects the scope preamble
            // into the unit prompt of every phase that (a) plays neither Creator nor Evaluator,
            // (b) runs BEFORE the first `executes_code` Creator phase, and (c) is agent-executed (a
            // Tool phase's description is argv context, never a prompt). Threaded ALONGSIDE the
            // instructions fold above (same single-line separator, appended after — the shared
            // intent and the phase's own slice still lead), so authored instructions are never
            // clobbered.
            let pre_build_scope = first_code_creator.is_some_and(|b| i < b)
                && !matches!(
                    phase.role,
                    crate::workflow::PhaseRole::Creator | crate::workflow::PhaseRole::Evaluator
                )
                && !matches!(phase.executor, crate::workflow::PhaseExecutor::Tool { .. });
            if pre_build_scope {
                description.push_str(INSTRUCTION_SEP);
                description.push_str(&phase_scope_preamble(&phase.id));
            }
            let mut unit = WorkUnit::pending(
                format!("{session_id}:{}", phase.id),
                session_id,
                ord,
                description,
            );
            // Stage is DATA from the def, not a keyword guess over the description. The phase linkage
            // lives in the unit id (`<session>:<phase_id>`) — we do NOT touch `phase_ref`, which the
            // execute path owns (it records the orchestration phase, set at execute time).
            unit.stage = phase.kind;
            // Carry the phase's skill + runtime allowlist (DES-EXEC-001 §4.1/§4.2) onto the unit so the
            // step runner invokes the right skill under least-privilege — pure data from the def.
            unit.skill_ref = phase.skill_ref.clone();
            unit.allowed_skills = phase.allowed_skills.clone();
            // Carry the phase's declared human-confirm gate (§3) so the DEF drives when the run pauses
            // for a human, not just the run-level --confirm flag.
            unit.gate = phase.gate;
            // Carry the evaluator≠creator role (§4) so the gate can do real artifact-passing (an
            // Evaluator unit reviews the prior Creator's cold output).
            unit.role = phase.role;
            // Carry the DECLARED dependency graph (FINDING-024). The def states which phases this one
            // consumes; the engine honored that for ordering and dropped it for context, so an
            // Evaluator phase declared `.after("build")` still ran blind to the build. Carrying it
            // onto the unit is what lets the dispatch site inject the right priors — and keeps the
            // bound author-controlled rather than a guessed "last N units".
            unit.depends_on = phase.depends_on.clone();
            // Carry the phase's DECLARED deliverables (FINDING-101). PhaseDef parsed this and
            // nothing read it, so a workflow could list required outputs the engine never verified.
            // The completion path checks them, mirroring how validator/gate/role flow from def to unit.
            unit.required_deliverables = phase.required_deliverables.clone();
            // Carry the phase's `executes_code` declaration (crew#311 / core#297 §2) so the
            // completion path's CODE-EVIDENCE floor can re-derive a build phase's "done" from the
            // worktree diff — the fold has no route back to the def, so like role/gate/deps the
            // declaration must ride the unit.
            unit.executes_code = phase.executes_code;
            // Carry the tool command for Tool-executor phases so the actor can run it directly.
            if let crate::workflow::PhaseExecutor::Tool { cmd } = &phase.executor {
                unit.tool_cmd = Some(cmd.clone());
            }
            // The marker BOTH other halves read. core#283 gave it one consumer — the completion
            // path's after-the-fact warning (`actor::phase_scope_warning`, still live: it catches
            // what the gate cannot see, e.g. a `Bash` heredoc). core#296 gave it the one that
            // actually holds the scope: the launcher rides it to the governance gate
            // (`gate_hook::PRE_BUILD_SCOPE_ENV` on the hook-subprocess carrier,
            // `BoundaryCtx::pre_build_scope` in-process), which REFUSES a non-documentation
            // Write/Edit before it lands. One field, so the prompt, the gate and the warning can
            // never disagree about which phases are pre-build.
            unit.pre_build_scope = pre_build_scope;
            unit
        })
        .collect()
}

/// Bind a run's repo into the placeholders its Tool phases declare.
///
/// A Tool phase's argv is DATA from the workflow def, which is shared by every run of that id. The
/// paths a run actually needs are not: they belong to the repo the run targets. Crew used to close
/// that gap by rewriting the def with one repo's absolute paths and writing it to a single shared
/// overlay file per launch — so three concurrent registrations raced on one file and two of them
/// indexed a third repo's tree into a third repo's database, reported under their own names
/// (FINDING-075, wicked-crew#196). The lock contention that exposed it was luck; the general case is
/// a run that silently does another run's work.
///
/// Substituting here removes the shared artifact entirely. The def stays constant and shared; the
/// per-run values reach the unit, which is already per-run and already persisted.
///
/// Unresolved tokens are an ERROR, never a passthrough. A command carrying a literal `{repo_root}`
/// would be handed to a shell as a path that cannot exist — a confusing failure at best, and at
/// worst (for a tool that treats an unknown path as "index the cwd") the FINDING-067 shape.
pub fn bind_repo_paths(units: &mut [WorkUnit], repo: &crate::repo::RepoEntry) {
    for unit in units.iter_mut() {
        let Some(cmd) = unit.tool_cmd.as_mut() else {
            continue;
        };
        for arg in cmd.iter_mut() {
            if arg == crate::workflow::REPO_ROOT_TOKEN {
                *arg = repo.root_path.clone();
            } else if arg == crate::workflow::CODE_GRAPH_DB_TOKEN {
                *arg = repo.code_graph_db.clone();
            }
        }
    }
}

/// Every placeholder a Tool phase may declare — the set [`bind_repo_paths`] can satisfy.
const REPO_TOKENS: &[&str] = &[
    crate::workflow::REPO_ROOT_TOKEN,
    crate::workflow::CODE_GRAPH_DB_TOKEN,
];

/// The `<phase>: <token>` pairs a def declares that no repo was bound for.
///
/// Separate from [`bind_repo_paths`] so the caller can refuse the launch BEFORE anything is
/// persisted: a run whose def wants a repo but was launched without one must fail at the door, not
/// dispatch a command with a brace-wrapped literal in it.
pub fn unbound_repo_tokens(units: &[WorkUnit]) -> Vec<String> {
    let mut out = Vec::new();
    for unit in units {
        let Some(cmd) = &unit.tool_cmd else { continue };
        for arg in cmd {
            if REPO_TOKENS.contains(&arg.as_str()) {
                // `phase_id()` and not `unit.id`: the id is `<session>:<phase>`, so naming the unit
                // repeats the session on every line of a message that is already about one run.
                // Falls back to the full id for a hand-built unit that carries no session prefix.
                out.push(format!("{}: {arg}", unit.phase_id().unwrap_or(&unit.id)));
            }
        }
    }
    out
}

/// Split on newlines, sentence terminators (`.`/`!`/`?` followed by whitespace), or semicolons.
fn split_problem(problem: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = problem.chars().collect();

    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\n' => {
                push_trimmed(&mut pieces, &mut current);
                while i + 1 < chars.len() && chars[i + 1] == '\n' {
                    i += 1;
                }
            }
            ';' => {
                push_trimmed(&mut pieces, &mut current);
                while i + 1 < chars.len() && chars[i + 1].is_whitespace() {
                    i += 1;
                }
            }
            '.' | '!' | '?' => {
                current.push(c);
                if i + 1 < chars.len() && chars[i + 1].is_whitespace() {
                    push_trimmed(&mut pieces, &mut current);
                    while i + 1 < chars.len() && chars[i + 1].is_whitespace() {
                        i += 1;
                    }
                }
            }
            _ => current.push(c),
        }
        i += 1;
    }
    push_trimmed(&mut pieces, &mut current);
    pieces
}

fn push_trimmed(pieces: &mut Vec<String>, current: &mut String) {
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        pieces.push(trimmed.to_string());
    }
    current.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::UnitStatus;

    #[test]
    fn splits_on_newlines_and_terminators_and_semicolons() {
        let units = plan_units("First task.\nSecond task; third task", "s1");
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].description, "First task.");
        assert_eq!(units[1].description, "Second task");
        assert_eq!(units[2].description, "third task");
        assert_eq!(units[0].id, "s1:u1");
        assert_eq!(units[2].ord, 3);
        assert!(units.iter().all(|u| u.status == UnitStatus::Pending));
    }

    #[test]
    fn deterministic_same_input_same_units() {
        assert_eq!(plan_units("Do X; do Y", "s"), plan_units("Do X; do Y", "s"));
    }

    #[test]
    fn empty_problem_falls_back_to_one_unit() {
        let units = plan_units("   ", "s");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].description, "unit");
    }

    #[test]
    fn decimal_point_does_not_split() {
        assert_eq!(plan_units("Upgrade to version 3.5 now", "s").len(), 1);
    }

    // ---- plan_from_def: the data-driven planner (Law 2) ----
    use crate::workflow::{bug_def, feature_def, migration_def};

    #[test]
    fn plan_from_def_yields_one_unit_per_phase_in_order() {
        let def = feature_def();
        let units = plan_from_def(&def, "add SSO login", "s1");
        assert_eq!(units.len(), def.phases.len());
        // 1:1, same order, unit id derived from the phase id — not from prose splitting. The unit id
        // IS the backing-phase linkage; phase_ref is left for the execute path.
        for (unit, phase) in units.iter().zip(def.phases.iter()) {
            assert_eq!(unit.id, format!("s1:{}", phase.id));
            assert!(unit.phase_ref.is_none(), "plan must not pre-set phase_ref");
        }
        assert_eq!(units[0].ord, 1);
        assert_eq!(units.last().unwrap().ord, units.len() as u32);
        assert!(units.iter().all(|u| u.status == UnitStatus::Pending));
    }

    /// FINDING-024: the DECLARED dependency graph reaches the unit, so the dispatch site can inject
    /// the priors a phase actually consumes. Asserted against the SHIPPED `feature` def rather than a
    /// fixture — the whole finding was that real workflows already declare the edges the engine
    /// dropped, so a synthetic def would prove nothing about them.
    #[test]
    fn plan_from_def_carries_the_declared_dependency_graph_onto_the_unit() {
        let def = feature_def();
        let units = plan_from_def(&def, "add SSO login", "s1");
        for (unit, phase) in units.iter().zip(def.phases.iter()) {
            assert_eq!(
                unit.depends_on, phase.depends_on,
                "phase `{}` must carry its own depends_on verbatim",
                phase.id
            );
        }

        let dep = |id: &str| {
            units
                .iter()
                .find(|u| u.id == format!("s1:{id}"))
                .unwrap_or_else(|| panic!("feature has a `{id}` phase"))
                .depends_on
                .clone()
        };
        // The Evaluator phase declares the Creator phase it reviews — the exact edge whose loss made
        // `adversarial-review` re-solve the original task against a different file.
        assert_eq!(dep("adversarial-review"), vec!["build".to_string()]);
        assert_eq!(dep("test"), vec!["build".to_string()]);
        assert_eq!(dep("review"), vec!["test".to_string()]);
        // The first phase depends on nothing; an empty list must stay empty (not a defaulted guess).
        assert!(dep("clarify").is_empty());
    }

    /// crew#311 / core#297 §2: the phase's `executes_code` declaration rides the unit — the fold's
    /// CODE-EVIDENCE floor has no route back to the def, so without the carry every build unit
    /// would read `false` and the floor would be armed nowhere.
    #[test]
    fn plan_from_def_carries_executes_code_onto_the_unit() {
        let def = feature_def();
        let units = plan_from_def(&def, "add SSO login", "s1");
        for (unit, phase) in units.iter().zip(def.phases.iter()) {
            assert_eq!(
                unit.executes_code, phase.executes_code,
                "phase `{}` must carry its own executes_code verbatim",
                phase.id
            );
        }
        // The concrete shape this exists for: `build` is marked, the prose phases are not.
        let marked = |id: &str| {
            units
                .iter()
                .find(|u| u.id == format!("s1:{id}"))
                .unwrap_or_else(|| panic!("feature has a `{id}` phase"))
                .executes_code
        };
        assert!(marked("build"), "feature/build is the executes_code phase");
        assert!(!marked("clarify") && !marked("design") && !marked("adversarial-review"));
    }

    /// FINDING-024, the join that makes the fix work at all. `prior_context_label` matches a prior's
    /// `phase_id()` (the unit-id suffix) against this unit's `depends_on` (phase ids copied from the
    /// def). Those are two different vocabularies meeting at a string compare, which is exactly the
    /// shape of FINDING-021 — there the phase token the policy engine selected on and the token the
    /// public API accepted diverged, and every gate silently no-op'd while looking correct.
    ///
    /// Nothing above proves they agree: the plan test proves the list is COPIED, and the actor tests
    /// construct units by hand, so both would still pass if real defs named their dependencies in a
    /// vocabulary `phase_id()` never produces — and the fix would inject nothing, silently, on every
    /// shipped workflow. This asserts the join RESOLVES across every builtin: each declared id must
    /// name a real phase that is planned EARLIER, since `prior_context_label` only offers priors with
    /// a lower ord. A forward or dangling edge is unreachable context, not a handoff.
    #[test]
    fn every_builtin_declares_dependencies_that_actually_resolve_to_earlier_units() {
        let registry = crate::workflow::WorkflowRegistry::with_defaults();
        let mut edges = 0usize;
        for id in registry.ids() {
            let def = registry.get(&id).expect("registry returned its own id");
            let units = plan_from_def(def, "some intent", "s1");
            // The lookup `prior_context_label` performs, built from the same `phase_id()` accessor.
            let by_phase: Vec<(Option<&str>, u32)> =
                units.iter().map(|u| (u.phase_id(), u.ord)).collect();
            for unit in &units {
                for dep in &unit.depends_on {
                    let target = by_phase
                        .iter()
                        .find(|(phase, _)| *phase == Some(dep.as_str()))
                        .unwrap_or_else(|| {
                            panic!(
                                "workflow `{id}`: phase `{}` declares depends_on `{dep}`, which no \
                                 unit's phase_id() yields — the declared graph and the unit-id \
                                 vocabulary have diverged, so injection silently no-ops",
                                unit.phase_id().unwrap_or("<none>")
                            )
                        });
                    assert!(
                        target.1 < unit.ord,
                        "workflow `{id}`: phase `{}` (ord {}) depends on `{dep}` (ord {}), which is \
                         not EARLIER — a forward edge is never offered to the dispatch site",
                        unit.phase_id().unwrap_or("<none>"),
                        unit.ord,
                        target.1
                    );
                    edges += 1;
                }
            }
        }
        // Guard the guard: if the builtins ever stop declaring dependencies this test would pass
        // vacuously while asserting nothing at all.
        assert!(
            edges >= 10,
            "expected the builtin defs to declare a real dependency graph, found {edges} edges"
        );
    }

    #[test]
    fn plan_from_def_takes_stage_from_the_phase_not_the_words() {
        // Every unit shares the SAME prose ("build ..."), which the keyword classifier would
        // stamp Build for all of them. plan_from_def must instead carry each phase's declared
        // kind — proving the stage is data from the def, not a guess over the description.
        let def = feature_def();
        let units = plan_from_def(&def, "build the thing", "s");
        for (unit, phase) in units.iter().zip(def.phases.iter()) {
            assert_eq!(unit.stage, phase.kind, "stage must come from phase.kind");
        }
        // And the def genuinely spans more than one kind (otherwise the test is vacuous).
        let first = units[0].stage;
        assert!(
            units.iter().any(|u| u.stage != first),
            "feature def should span multiple stages"
        );
    }

    #[test]
    fn plan_from_def_scopes_the_intent_into_each_phase() {
        let units = plan_from_def(&bug_def(), "500 on empty cart", "s");
        assert!(units
            .iter()
            .all(|u| u.description.contains("500 on empty cart")));
        assert!(units[0].description.starts_with(&bug_def().phases[0].id));
    }

    #[test]
    fn plan_from_def_is_deterministic() {
        let a = plan_from_def(&migration_def(), "move to pg", "s");
        let b = plan_from_def(&migration_def(), "move to pg", "s");
        assert_eq!(a, b);
    }

    #[test]
    fn plan_from_def_handles_empty_intent() {
        let units = plan_from_def(&feature_def(), "   ", "s");
        // Falls back to the bare phase id (plus, on a pre-build phase like `clarify`, the core#283
        // scope preamble) — never an empty description (gate needs work context).
        assert!(
            units[0]
                .description
                .starts_with(&feature_def().phases[0].id),
            "{}",
            units[0].description
        );
        assert!(units.iter().all(|u| !u.description.is_empty()));
    }

    /// core#283, the PROMPT half, asserted against the SHIPPED `feature` def (the workflow the
    /// collapse was proven on — its `design` phase received the same problem statement as `build`
    /// and implemented the entire deliverable, twice, once THROUGH an explicit prompt-level
    /// discipline paragraph). The scope preamble must land on exactly the pre-build non-creator
    /// phases (`clarify`, `design`) and on no build/evaluator/post-build phase — a preamble on
    /// `build` would scope the creator away from building, which is the inverse failure.
    #[test]
    fn scope_preamble_lands_exactly_on_pre_build_non_creator_phases_of_the_shipped_feature_def() {
        let def = feature_def();
        let units = plan_from_def(&def, "add SSO login", "s1");
        let by_phase = |id: &str| {
            units
                .iter()
                .find(|u| u.id == format!("s1:{id}"))
                .unwrap_or_else(|| panic!("feature has a `{id}` phase"))
        };

        for id in ["clarify", "design"] {
            let u = by_phase(id);
            assert!(
                u.description.contains(PHASE_SCOPE_PREFIX),
                "pre-build phase `{id}` must carry the scope preamble: {}",
                u.description
            );
            assert!(
                u.description.contains(&format!("this is the {id} phase")),
                "the preamble is composed from THIS phase's id, not a workflow-wide blurb: {}",
                u.description
            );
            assert!(
                u.description.contains("add SSO login"),
                "the shared intent still leads — the preamble threads alongside, not instead: {}",
                u.description
            );
            assert!(
                !u.description.contains('\n'),
                "the preamble must stay single-line (the PTY runner submits on the first newline): {}",
                u.description
            );
            // core#296. The preamble used to announce itself as `PHASE SCOPE (enforced):` while
            // enforcing nothing — run d1bc72c2's `design` unit wrote `src/board/attentionReason.ts`
            // under that exact header and the hook allowed it. A prompt cannot enforce itself; the
            // word is what stopped anyone from looking for the gate that was missing.
            assert!(
                !u.description.to_ascii_lowercase().contains("enforced"),
                "the prompt must not CLAIM enforcement — enforcement is the gate \
                 (`gate_hook::phase_scope_denial`), not this sentence: {}",
                u.description
            );
            assert!(
                u.pre_build_scope,
                "`{id}` must carry the marker the GATE reads (core#296) and the completion-path \
                 warning reads (core#283) — an unmarked phase is scoped in prose only"
            );
        }
        // build (Creator), adversarial-review (Evaluator), and the POST-build neutral phases: no
        // preamble, no marker.
        for id in ["build", "adversarial-review", "test", "review"] {
            let u = by_phase(id);
            assert!(
                !u.description.contains(PHASE_SCOPE_PREFIX),
                "`{id}` must NOT carry the preamble: {}",
                u.description
            );
            assert!(!u.pre_build_scope, "`{id}` must not be marked pre-build");
        }
    }

    /// core#283 across EVERY builtin: the preamble is a function of def DATA — role +
    /// `executes_code` + declaration order — never of a workflow id. Three invariants: (1) the
    /// prompt carries the preamble IFF the unit carries the marker (the two halves never diverge);
    /// (2) no creator/evaluator-role unit and no unit at-or-after the first code-executing Creator
    /// is ever scoped; (3) a def with NO code-executing Creator (`collab`, `survey-repo`,
    /// `onboarding`…) gets no preamble anywhere — there is no later build rung to defer to, so the
    /// preamble's promise would be a lie. Vacuity-guarded: the shipped defs must actually produce
    /// marked phases (feature: clarify+design, bug: triage+reproduce, migration: plan).
    #[test]
    fn scope_preamble_is_derived_from_def_data_across_every_builtin() {
        use crate::workflow::{PhaseExecutor, PhaseRole};
        let registry = crate::workflow::WorkflowRegistry::with_defaults();
        let mut marked = 0usize;
        for id in registry.ids() {
            let def = registry.get(&id).expect("registry returned its own id");
            let first_code_creator = def
                .phases
                .iter()
                .position(|p| p.executes_code && p.role == PhaseRole::Creator);
            let units = plan_from_def(def, "some intent", "s1");
            for (ix, (unit, phase)) in units.iter().zip(def.phases.iter()).enumerate() {
                let has_preamble = unit.description.contains(PHASE_SCOPE_PREFIX);
                assert_eq!(
                    has_preamble, unit.pre_build_scope,
                    "workflow `{id}` phase `{}`: prompt preamble and completion marker diverged",
                    phase.id
                );
                if matches!(phase.role, PhaseRole::Creator | PhaseRole::Evaluator) {
                    assert!(
                        !has_preamble,
                        "workflow `{id}` phase `{}` plays {:?} and must never be scoped away \
                         from its role",
                        phase.id, phase.role
                    );
                }
                if first_code_creator.is_none_or(|b| ix >= b) {
                    assert!(
                        !has_preamble,
                        "workflow `{id}` phase `{}` is not PRE-build (no code-executing creator \
                         after it) and must not carry the preamble",
                        phase.id
                    );
                }
                if let PhaseExecutor::Tool { .. } = phase.executor {
                    assert!(
                        !has_preamble,
                        "workflow `{id}` phase `{}` is Tool-executed — its description is argv \
                         context, not a prompt",
                        phase.id
                    );
                }
                marked += has_preamble as usize;
            }
        }
        assert!(
            marked >= 5,
            "the shipped defs must produce pre-build scoped phases (feature ×2, bug ×2, \
             migration ×1) or this guard is vacuous; found {marked}"
        );
    }

    /// core#283 + FINDING-011 interplay: a pre-build phase that ALSO authors `instructions` keeps
    /// them — the preamble threads alongside via the same single-line fold, never clobbers.
    #[test]
    fn scope_preamble_threads_alongside_instructions_without_clobbering_them() {
        use crate::domain::StageKind;
        use crate::workflow::{PhaseDef, PhaseRole};
        let def = WorkflowDef {
            id: "ladder".to_string(),
            phases: vec![
                PhaseDef {
                    instructions: Some("write the design doc and nothing else".to_string()),
                    ..PhaseDef::new("design", StageKind::Recon)
                },
                PhaseDef {
                    executes_code: true,
                    role: PhaseRole::Creator,
                    depends_on: vec!["design".to_string()],
                    ..PhaseDef::new("build", StageKind::Build)
                },
            ],
        };
        let units = plan_from_def(&def, "add SSO", "s");
        let d = &units[0].description;
        assert!(d.contains("add SSO"), "intent survives: {d}");
        assert!(
            d.contains("write the design doc and nothing else"),
            "authored instructions survive: {d}"
        );
        assert!(d.contains(PHASE_SCOPE_PREFIX), "preamble joins them: {d}");
        assert!(
            d.find("write the design doc") < d.find(PHASE_SCOPE_PREFIX),
            "instructions lead, the scope preamble follows: {d}"
        );
        assert!(!d.contains('\n'), "single-line contract holds: {d}");
        assert!(
            !units[1].description.contains(PHASE_SCOPE_PREFIX),
            "the creator phase is never scoped away from building: {}",
            units[1].description
        );
        assert!(!units[1].pre_build_scope);
    }

    /// FINDING-011: a phase's own `instructions` reach ITS unit's description — the worker prompt —
    /// and no other unit's. Without the threading, every unit of an N-phase workflow carries a
    /// prompt that differs only by the phase-id token, so N recon phases run N near-identical
    /// surveys (survey-repo: $3.09 / 1.74M tokens to answer one question three times).
    #[test]
    fn plan_from_def_threads_each_phases_instructions_into_its_own_unit_only() {
        use crate::domain::StageKind;
        use crate::workflow::PhaseDef;
        let instr_a = "map the directory layout and nothing else";
        let instr_b = "identify the language stack and nothing else";
        let def = WorkflowDef {
            id: "instructed".to_string(),
            phases: vec![
                PhaseDef {
                    instructions: Some(instr_a.to_string()),
                    ..PhaseDef::new("a", StageKind::Recon)
                },
                PhaseDef {
                    instructions: Some(instr_b.to_string()),
                    depends_on: vec!["a".to_string()],
                    ..PhaseDef::new("b", StageKind::Recon)
                },
                PhaseDef::new("c", StageKind::Recon),
            ],
        };
        let units = plan_from_def(&def, "survey the repo", "s");

        // Each unit carries the shared intent AND its own phase's instructions…
        assert!(units[0].description.contains("survey the repo"));
        assert!(
            units[0].description.contains(instr_a),
            "{}",
            units[0].description
        );
        assert!(
            units[1].description.contains(instr_b),
            "{}",
            units[1].description
        );
        // …and never a sibling's (the whole point is that the prompts stop being interchangeable).
        assert!(
            !units[0].description.contains(instr_b),
            "unit a leaked unit b's instructions: {}",
            units[0].description
        );
        assert!(
            !units[1].description.contains(instr_a),
            "unit b leaked unit a's instructions: {}",
            units[1].description
        );
        // A phase with no instructions keeps the historical prompt byte-exact (no trailing junk).
        assert_eq!(units[2].description, "c — survey the repo");
    }

    /// The degenerate authoring cases: an empty intent still gets the instructions (bare phase id
    /// first), and whitespace-only instructions are treated as absent rather than appending blank
    /// lines to the prompt.
    #[test]
    fn instructions_survive_an_empty_intent_and_blank_instructions_are_ignored() {
        use crate::domain::StageKind;
        use crate::workflow::PhaseDef;
        let def = WorkflowDef {
            id: "instructed".to_string(),
            phases: vec![
                PhaseDef {
                    instructions: Some("do the one thing".to_string()),
                    ..PhaseDef::new("a", StageKind::Recon)
                },
                PhaseDef {
                    instructions: Some("   \n ".to_string()),
                    ..PhaseDef::new("b", StageKind::Recon)
                },
            ],
        };
        let units = plan_from_def(&def, "  ", "s");
        assert_eq!(
            units[0].description,
            format!("a{INSTRUCTION_SEP}do the one thing")
        );
        assert!(
            !units[0].description.contains('\n'),
            "the instruction fold must stay single-line (PTY submits on the first newline): {}",
            units[0].description
        );
        assert_eq!(
            units[1].description, "b",
            "blank instructions must not append"
        );
    }

    /// FINDING-011 (remediation): the instruction fold MUST stay single-line, because the
    /// description is the worker prompt and the PTY session runner submits a turn on the first
    /// newline — a `\n`-joined description would send only `<phase> — <intent>` (the near-identical
    /// prompt the fold exists to kill) and strand the instructions as a stray follow-up that desyncs
    /// the reused session's result sentinel. Asserted against the SHIPPED `survey-repo` def (the one
    /// carrying real multi-sentence instructions), not a fixture, so the guard tracks what ships.
    ///
    /// Falsifier: restore the `\n\n` join in `plan_from_def` — the folded descriptions regain a
    /// newline and the `contains('\n')` assert fires.
    #[test]
    fn folded_instructions_never_introduce_a_newline_into_the_prompt() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("workflows/survey-repo.json");
        let def = crate::workflow::WorkflowRegistry::def_from_file(&path)
            .expect("shipped survey-repo parses");
        // Vacuity guard: the def must actually carry instructions on multiple phases, or a
        // single-line join proves nothing.
        let carrying = def
            .phases
            .iter()
            .filter(|p| {
                p.instructions
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|s| !s.is_empty())
            })
            .count();
        assert!(
            carrying >= 3,
            "survey-repo must carry instructions on multiple phases or this guard is vacuous; \
             found {carrying}"
        );

        let units = plan_from_def(&def, "what is this repo and how do I work in it", "s");
        for (unit, phase) in units.iter().zip(def.phases.iter()) {
            assert!(
                !unit.description.contains('\n'),
                "phase `{}` planned a multi-line description; the PTY runner submits the turn at \
                 the first newline and strands the rest (FINDING-011): {:?}",
                phase.id,
                unit.description
            );
            // …and the instructions genuinely reached the description — the single-line join must
            // FOLD them in, not drop them (a fix that silently discarded them would also pass the
            // newline assert above).
            if let Some(instr) = phase
                .instructions
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                assert!(
                    unit.description.contains(instr),
                    "phase `{}` instructions did not reach its description: {:?}",
                    phase.id,
                    unit.description
                );
            }
        }
    }

    fn repo_at(id: &str, root: &str) -> crate::repo::RepoEntry {
        crate::repo::RepoEntry {
            id: id.to_string(),
            name: id.to_string(),
            root_path: root.to_string(),
            default_branch: "main".to_string(),
            registered_at: 0,
            // Through the engine's ONE resolver, never a hand-join — a second spelling here is
            // exactly the FINDING-069 drift this fixture exists to test against.
            code_graph_db: crate::code_graph::resolved_code_graph_db(std::path::Path::new(root))
                .to_string_lossy()
                .into_owned(),
        }
    }

    /// FINDING-075 (wicked-crew#196): the run's OWN repo reaches its units.
    ///
    /// Two runs planned from the SAME shared def must end up with different argv. That is the whole
    /// property: crew rewrote one shared overlay file per launch instead, so three concurrent
    /// registrations resolved whichever write landed last and two of them indexed a third repo.
    #[test]
    fn two_runs_of_one_def_bind_their_own_repos() {
        let def = crate::workflow::onboarding_def();
        let mut a = plan_from_def(&def, "onboard a", "sa");
        let mut b = plan_from_def(&def, "onboard b", "sb");
        let alpha = repo_at("alpha", "/repos/alpha");
        let beta = repo_at("beta", "/repos/beta");
        bind_repo_paths(&mut a, &alpha);
        bind_repo_paths(&mut b, &beta);

        let index_a = a[0].tool_cmd.as_ref().expect("index is a tool phase");
        let index_b = b[0].tool_cmd.as_ref().expect("index is a tool phase");
        assert!(index_a.contains(&"/repos/alpha".to_string()), "{index_a:?}");
        assert!(index_b.contains(&"/repos/beta".to_string()), "{index_b:?}");
        assert!(
            !index_a.iter().any(|s| s.contains("beta")),
            "run `sa` carries run `sb`'s repo — the cross-repo contamination this guards: {index_a:?}"
        );
        assert!(
            !index_b.iter().any(|s| s.contains("alpha")),
            "run `sb` carries run `sa`'s repo: {index_b:?}"
        );

        // Both phases target the graph the ENGINE resolved — the exact `code_graph_db` the
        // record publishes, never a re-derived spelling (FINDING-069). Asserted against the
        // fixture's resolver-produced value rather than a shape suffix, so this cannot drift
        // into a second spelling of either home.
        for (units, repo) in [(&a, &alpha), (&b, &beta)] {
            for u in units.iter() {
                let cmd = u
                    .tool_cmd
                    .as_ref()
                    .expect("onboarding phases are tool phases");
                let db =
                    cmd[cmd.iter().position(|s| s == "--db").expect("carries --db") + 1].clone();
                assert_eq!(db, repo.code_graph_db, "{db}");
            }
        }
    }

    #[test]
    fn binding_leaves_no_placeholder_behind() {
        let def = crate::workflow::onboarding_def();
        let mut units = plan_from_def(&def, "onboard", "s1");
        assert!(
            !unbound_repo_tokens(&units).is_empty(),
            "the def must DECLARE placeholders, or this guard is vacuous"
        );
        bind_repo_paths(&mut units, &repo_at("alpha", "/repos/alpha"));
        assert_eq!(
            unbound_repo_tokens(&units),
            Vec::<String>::new(),
            "a bound run must carry no `{{...}}` literal into a spawned command"
        );
    }

    /// A phase with no repo placeholders is untouched — binding is not a blanket rewrite.
    #[test]
    fn binding_does_not_touch_commands_that_declare_nothing() {
        let mut units = plan_from_def(&crate::workflow::onboarding_def(), "x", "s1");
        units[0].tool_cmd = Some(vec![
            "wicked-estate".into(),
            "index".into(),
            "/literal".into(),
        ]);
        bind_repo_paths(&mut units, &repo_at("alpha", "/repos/alpha"));
        assert_eq!(
            units[0].tool_cmd.as_deref(),
            Some(
                &[
                    "wicked-estate".to_string(),
                    "index".to_string(),
                    "/literal".to_string()
                ][..]
            )
        );
    }
}
