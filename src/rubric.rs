//! Gate rubrics as data (DES-CATALOG-002 §5.2): the judging contracts several presets share —
//! parity, cited findings and fidelity to source — written once, in `src/rubrics/<id>.json`, and
//! named by a plan step (`"rubric": "cited-findings"`, one id per step).
//!
//! [`crate::plan::compose`] expands the field, and every rule only makes the step stricter: an
//! unknown id is refused (`unknown_rubric`), a rubric on an entry outside its `applies_to` is refused
//! (`rubric_wrong_entry`: cited findings on a `build` step would have a creator judge itself), and
//! the rubric's instructions are appended after the step's own on the same line. A rubric never
//! changes the step's gate: each preset sets its own.
//!
//! A rubric whose `recheck_excludes_gatherers` is set (cited findings, fidelity to source) also
//! changes who may sit the step: the seats of the plan's earlier agent `understand` steps are its
//! authors, guarded read steps included, so a report's gatherer never rechecks its own findings
//! ([`crate::pipeline::work_authors`], the evaluator fence in [`crate::distribute`]).

use std::sync::OnceLock;

/// The parity rubric's id (a `test` step).
pub const PARITY: &str = "parity";
/// The cited-findings rubric's id (a `critique` or `review` step).
pub const CITED_FINDINGS: &str = "cited-findings";
/// The fidelity-to-source rubric's id (a `critique` or `review` step).
pub const FIDELITY_TO_SOURCE: &str = "fidelity-to-source";

/// One rubric, as its data file declares it. `deny_unknown_fields`: a misspelled key is refused.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rubric {
    pub id: String,
    pub title: String,
    /// The catalog ids a step carrying this rubric may instantiate.
    pub applies_to: Vec<String>,
    /// The judging contract, appended to the step's own instructions. Single-line.
    pub instructions: String,
    /// The seats of the plan's earlier agent `understand` steps are this step's authors.
    #[serde(default)]
    pub recheck_excludes_gatherers: bool,
}

/// Every rubric, in id order. Parsed once; a malformed data file is a build-time defect the
/// tests below catch.
pub fn rubrics() -> &'static [Rubric] {
    static RUBRICS: OnceLock<Vec<Rubric>> = OnceLock::new();
    RUBRICS.get_or_init(|| {
        [
            include_str!("rubrics/cited-findings.json"),
            include_str!("rubrics/fidelity-to-source.json"),
            include_str!("rubrics/parity.json"),
        ]
        .iter()
        .map(|raw| serde_json::from_str(raw).expect("src/rubrics/*.json is a valid rubric"))
        .collect()
    })
}

/// The rubric `id` names (`None` for an id no data file defines).
pub fn rubric(id: &str) -> Option<&'static Rubric> {
    rubrics().iter().find(|r| r.id == id)
}

/// Whether a step carrying rubric `id` excludes its plan's gatherers from its seat.
pub fn excludes_gatherers(id: Option<&str>) -> bool {
    id.and_then(rubric)
        .is_some_and(|r| r.recheck_excludes_gatherers)
}

/// `own` with rubric `r`'s instructions appended on the same line: the step's own criteria lead,
/// the shared contract follows.
pub(crate) fn expand(own: Option<&str>, r: &Rubric) -> String {
    match own.map(str::trim_end).filter(|s| !s.is_empty()) {
        Some(own) => format!("{own} {}", r.instructions),
        None => r.instructions.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_rubrics_are_data_with_their_entries_and_one_line_each() {
        let ids: Vec<&str> = rubrics().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, [CITED_FINDINGS, FIDELITY_TO_SOURCE, PARITY]);
        for r in rubrics() {
            assert!(!r.title.trim().is_empty(), "{}", r.id);
            assert!(!r.instructions.contains('\n'), "{}: single-line", r.id);
            assert!(
                r.instructions
                    .trim_end()
                    .ends_with("never quote another VERDICT line."),
                "{}: the VERDICT contract closes every rubric",
                r.id
            );
            for c in &r.applies_to {
                let e = crate::catalog::catalog_entry(c)
                    .unwrap_or_else(|| panic!("{}: {c} is a catalog id", r.id));
                // A rubric judges: it never applies to a creator entry.
                assert_eq!(
                    e.role,
                    crate::workflow::PhaseRole::Evaluator,
                    "{}: {c}",
                    r.id
                );
            }
        }
        assert_eq!(rubric(PARITY).unwrap().applies_to, ["test"]);
        assert!(excludes_gatherers(Some(CITED_FINDINGS)));
        assert!(excludes_gatherers(Some(FIDELITY_TO_SOURCE)));
        assert!(!excludes_gatherers(Some(PARITY)));
        assert!(!excludes_gatherers(Some("no-such-rubric")));
        assert!(!excludes_gatherers(None));
    }
}
