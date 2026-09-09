//! TESTING.md's copyable authoring example must PARSE under the parser it documents (PR #398
//! review): an earlier revision carried inline `# comments` inside values, which the strict
//! frontmatter subset rejects (`applies_to: [build] # …` is not a flow list, `effect: deny # …`
//! is not an effect) and which turn a `trigger:` comment into literal regex text that never
//! matches. The example is lifted from the doc AT COMPILE TIME — not a hand-copied fixture — so
//! the doc and this test cannot drift apart: edit the example and this test replays the edit.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use wicked_apps_core::SqliteStore;
use wicked_governance::{
    ingest_from, register_rule, run_evals, select_any, Effect, EvalSample, MarkdownAdapter, Verdict,
};

/// The guide, byte-for-byte as shipped.
const TESTING_MD: &str = include_str!("../TESTING.md");

/// The ONE ```markdown fenced block in TESTING.md that carries the `git-hygiene` example.
fn doc_example() -> String {
    let blocks: Vec<&str> = TESTING_MD
        .split("```markdown\n")
        .skip(1)
        .map(|chunk| {
            chunk
                .split_once("\n```")
                .map(|(body, _)| body)
                .expect("every ```markdown fence in TESTING.md closes")
        })
        .filter(|body| body.contains("id: git-hygiene"))
        .collect();
    assert_eq!(
        blocks.len(),
        1,
        "exactly one git-hygiene example in TESTING.md (found {})",
        blocks.len()
    );
    format!("{}\n", blocks[0])
}

/// A per-test scratch dir: pid + test name + a process-wide counter (tests share one process).
fn scratch_dir(test: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "wicked-gov-testing-md-{}-{test}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn testing_md_example_ingests_verbatim_and_its_trigger_fires_on_the_documented_sample() {
    wicked_apps_core::emit::hermetic_test_spool();
    let example = doc_example();
    // No inline comments anywhere: the grammar knows only FULL-LINE `#` comments, so a `# …` after
    // a value would be value text (the exact defect this test pins).
    for line in example.lines() {
        assert!(
            line.trim_start().starts_with('#') || !line.contains(" #"),
            "inline comment in the example — the parser would read it as value text: {line:?}"
        );
    }

    // ── 1. The exact doc text ingests through the ONE parse path ─────────────────────────
    let dir = scratch_dir("ingest");
    std::fs::write(dir.join("git-hygiene.md"), &example).unwrap();
    let rules = ingest_from(&MarkdownAdapter::new(&dir))
        .unwrap_or_else(|e| panic!("TESTING.md's example must ingest verbatim: {e}\n{example}"));
    assert_eq!(rules.len(), 2, "POL-060 + PAT-061");

    let pol = rules.iter().find(|r| r.id == "POL-060").expect("POL-060");
    assert_eq!(
        pol.effect,
        Some(Effect::Deny),
        "the doc-level `effect: deny` rides onto POL-060"
    );
    assert_eq!(
        pol.applies_to,
        vec!["build".to_string()],
        "`applies_to: [build]` parsed as a flow list"
    );
    assert_eq!(pol.steering_type, "development");
    assert_eq!(
        pol.trigger.as_ref().and_then(|t| t.contains.as_deref()),
        Some(r"push\s+--force"),
        "the trigger is the bare regex — no comment text appended"
    );
    assert_eq!(pol.statement, "Never force-push a shared branch.");

    let pat = rules.iter().find(|r| r.id == "PAT-061").expect("PAT-061");
    assert_eq!(
        pat.effect,
        Some(Effect::AllowWithConditions),
        "the per-rule `effect: warn` overrides the doc key"
    );
    assert!(pat.trigger.is_none(), "PAT-061 carries no trigger");

    // ── 2. Both rules are DECIDE-lane: SELECT picks them up at the applies_to phase ──────
    let mut store = SqliteStore::in_memory().unwrap();
    for r in &rules {
        register_rule(&mut store, r).unwrap();
    }
    let selected = select_any(&store, "s", &["build"], &serde_json::json!({})).unwrap();
    assert_eq!(
        selected
            .iter()
            .map(|p| (p.id.as_str(), p.effect))
            .collect::<Vec<_>>(),
        vec![
            ("PAT-061", Effect::AllowWithConditions),
            ("POL-060", Effect::Deny)
        ],
        "SELECT sees both doc rules as policies at `build` (id order)"
    );

    // ── 3. Replay the doc's own samples through the REAL gate path (select → decide) ─────
    let samples: Vec<EvalSample> = serde_json::from_value(serde_json::json!([
        {"id": "force-push", "description": "force-pushes main", "kind": "bad",
         "steering_type": "development",
         "signals": {"phase": "build", "tool": "Bash", "content": "git push --force origin main"}},
        {"id": "fix-branch", "description": "pushes a fix branch", "kind": "good",
         "steering_type": "development",
         "signals": {"phase": "build", "tool": "Bash", "content": "git push origin fix/x"}}
    ]))
    .unwrap();
    let report = run_evals(&store, &samples, None, None, 1_700_000_000).unwrap();

    assert_eq!(report.summary.total, 2);
    assert_eq!(report.summary.caught, 2);
    assert_eq!(report.summary.gaps, 0);
    assert_eq!(report.summary.false_positives, 0);
    let bad = &report.results[0];
    assert_eq!(bad.sample.id, "force-push");
    assert_eq!(bad.verdict, Verdict::Caught);
    assert_eq!(
        bad.fired,
        vec!["POL-060".to_string()],
        "the documented trigger fires on the documented force-push"
    );
    let good = &report.results[1];
    assert_eq!(good.sample.id, "fix-branch");
    assert_eq!(
        good.verdict,
        Verdict::Caught,
        "the quiet success: no blocking firing"
    );
    assert!(
        good.fired.is_empty(),
        "a plain push does not trip the trigger"
    );

    // The doc's own claim about `warn`: PAT-061 (phase-selected, no trigger) is EXERCISED by the
    // run but never CATCHES — exercised = both decide-lane rules, nothing recall-only.
    assert_eq!(report.rule_coverage.exercised, 2);
    assert!(report.rule_coverage.unexercised.is_empty());
    assert_eq!(report.rule_coverage.recall_only, 0);

    let _ = std::fs::remove_dir_all(&dir);
}
