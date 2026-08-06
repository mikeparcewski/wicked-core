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
    def.phases
        .iter()
        .enumerate()
        .map(|(i, phase)| {
            let ord = (i + 1) as u32;
            let description = if intent.is_empty() {
                phase.id.clone()
            } else {
                format!("{} — {intent}", phase.id)
            };
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
            // Carry the tool command for Tool-executor phases so the actor can run it directly.
            if let crate::workflow::PhaseExecutor::Tool { cmd } = &phase.executor {
                unit.tool_cmd = Some(cmd.clone());
            }
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
        // Falls back to the bare phase id — never an empty description (gate needs work context).
        assert_eq!(units[0].description, feature_def().phases[0].id);
        assert!(units.iter().all(|u| !u.description.is_empty()));
    }

    fn repo_at(id: &str, root: &str) -> crate::repo::RepoEntry {
        crate::repo::RepoEntry {
            id: id.to_string(),
            name: id.to_string(),
            root_path: root.to_string(),
            default_branch: "main".to_string(),
            registered_at: 0,
            code_graph_db: format!("{root}/.codegraph/estate.db"),
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
        bind_repo_paths(&mut a, &repo_at("alpha", "/repos/alpha"));
        bind_repo_paths(&mut b, &repo_at("beta", "/repos/beta"));

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

        // Both phases target the graph the ENGINE resolved, never a re-derived spelling (FINDING-069).
        for units in [&a, &b] {
            for u in units.iter() {
                let cmd = u
                    .tool_cmd
                    .as_ref()
                    .expect("onboarding phases are tool phases");
                let db =
                    cmd[cmd.iter().position(|s| s == "--db").expect("carries --db") + 1].clone();
                assert!(db.ends_with("/.codegraph/estate.db"), "{db}");
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
