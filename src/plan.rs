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
            unit
        })
        .collect()
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
}
