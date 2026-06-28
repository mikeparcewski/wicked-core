//! PLAN — deterministic decomposition of a free-text problem into ordered work units.
//! Ported into COE from the retired wicked-agent. Splits on newlines / sentence terminators /
//! semicolons; the same problem always yields the same ordered units (no randomness, no model).

use crate::domain::WorkUnit;

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
}
