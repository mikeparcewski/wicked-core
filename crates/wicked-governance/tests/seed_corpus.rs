//! The shipped doctrine seed corpus (`seed/corpus/`) ingests through the ONE markdown parse
//! path, mints exactly the rule ids the seed manifest and the runbook driver expect, groups them
//! under the documented RuleSets, and STEERING.md's tour line counts what the corpus really
//! holds — so adding a seed rule cannot drift from the docs that cite it.

use std::collections::BTreeSet;
use std::path::PathBuf;

use wicked_apps_core::SqliteStore;
use wicked_governance::{ingest_from, recall_rules, register_rule, MarkdownAdapter, RuleQuery};

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("seed/corpus")
}

/// Every rule the seed corpus mints (estate ADR-011's POL-1101..POL-1104 are staged at run
/// time by `seed_wiki.py`, not committed here).
const SEED_RULE_IDS: &[&str] = &[
    "POL-1301", "PAT-1302", "PAT-1303", "PAT-1304", "PAT-1305", "PAT-1306", "PAT-1307", "PAT-1308",
    "POL-1401", "PAT-1402", "PAT-1403", "POL-1501", "PAT-1502", "PAT-1601", "PAT-1602", "PAT-1603",
    "PAT-1604", "PAT-1605", "PAT-1606", "PAT-1607", "PAT-1701", "PAT-1702", "PAT-1703", "POL-1704",
    "POL-1801", "POL-1802", "PAT-1901", "PAT-1902", "PAT-1903", "PAT-1904", "POL-2001", "POL-2002",
    "PAT-2003", "PAT-2004", "PAT-2005", "PAT-2006", "POL-2101",
];

#[test]
fn the_seed_corpus_mints_exactly_the_expected_rules_and_rulesets() {
    wicked_apps_core::emit::hermetic_test_spool();
    let adapter = MarkdownAdapter::new(corpus_root());
    let rules = ingest_from(&adapter).expect("the shipped seed corpus ingests");
    let got: BTreeSet<&str> = rules.iter().map(|r| r.id.as_str()).collect();
    let want: BTreeSet<&str> = SEED_RULE_IDS.iter().copied().collect();
    assert_eq!(got, want, "seed corpus rule ids");
    assert_eq!(rules.len(), SEED_RULE_IDS.len(), "no duplicate ids");

    let groupings = adapter.groupings().expect("groupings");
    let domains: Vec<&str> = groupings.iter().map(|g| g.domain.as_str()).collect();
    assert_eq!(
        domains,
        [
            "agent-behavior",
            "cross-platform",
            "engineering-doctrine",
            "event-grammar",
            "mcp-surface",
            "plane-boundaries",
            "storage-doctrine",
        ]
    );
    let grouped: usize = groupings.iter().map(|g| g.rule_ids.len()).sum();
    assert_eq!(grouped, SEED_RULE_IDS.len(), "every seed rule is grouped");

    // The tour line in STEERING.md counts the corpus as it ships.
    let docs = std::fs::read_dir(corpus_root())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "md")
        })
        .count();
    let tour = format!(
        "({docs} doctrine docs → {} rules under {} RuleSets)",
        rules.len(),
        groupings.len()
    );
    let steering = include_str!("../STEERING.md").replace("\r\n", "\n");
    assert!(steering.contains(&tour), "STEERING.md states {tour:?}");
}

/// POL-2101 (operator ruling 2026-10-12): the chosen stack's built-ins come first. It is a
/// development rule under engineering-doctrine, applies to build and review, and its statement
/// carries all four obligations: the stack is chosen for the agent, the check comes before
/// code, the check is recorded in the step output, and reviewers flag unjustified duplicates.
#[test]
fn pol_2101_check_the_chosen_stack_is_a_development_rule() {
    wicked_apps_core::emit::hermetic_test_spool();
    let adapter = MarkdownAdapter::new(corpus_root());
    let mut store = SqliteStore::in_memory().unwrap();
    for r in ingest_from(&adapter).unwrap() {
        register_rule(&mut store, &r).unwrap();
    }
    let dev = recall_rules(
        &store,
        &RuleQuery {
            steering_type: Some("development".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let rule = dev
        .iter()
        .find(|r| r.id == "POL-2101")
        .expect("POL-2101 recalls on the development page");
    assert_eq!(rule.applies_to, ["build", "review"]);
    for needle in [
        "choose the framework or library",
        "check what the chosen stack already",
        "record that check in the step output",
        "duplicates a built-in needs a stated reason",
        "Reviewers flag",
    ] {
        assert!(
            rule.statement.contains(needle),
            "POL-2101 statement carries {needle:?}: {}",
            rule.statement
        );
    }
    let reference = rule.provenance.reference.as_deref().unwrap();
    assert!(
        reference.starts_with("stack-builtins.md@") && reference.ends_with("#POL-2101"),
        "{reference}"
    );
    let groupings = adapter.groupings().unwrap();
    let doctrine = groupings
        .iter()
        .find(|g| g.domain == "engineering-doctrine")
        .unwrap();
    assert!(doctrine.rule_ids.iter().any(|id| id == "POL-2101"));
}
