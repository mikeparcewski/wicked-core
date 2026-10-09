//! The shipped `governance/packs/review-loop` steering pack ingests through the ONE markdown parse
//! path (`MarkdownAdapter` → `normalize_bundle`) as four recall-only guidance rules — the
//! review-loop doctrine (S15e recon) every governed run's review phases recall.

use std::path::PathBuf;

use wicked_governance::{ingest_from, parse_provenance_ref, ConfSeverity, MarkdownAdapter};

fn pack_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../governance/packs/review-loop")
}

#[test]
fn review_loop_pack_ingests_as_four_guidance_rules() {
    let adapter = MarkdownAdapter::new(pack_root());
    let mut rules = ingest_from(&adapter).expect("the review-loop pack ingests");
    rules.sort_by(|a, b| a.id.cmp(&b.id));
    let expected: [(&str, &str, ConfSeverity); 4] = [
        ("RVWL-1001", "testing", ConfSeverity::Error),
        ("RVWL-1002", "development", ConfSeverity::Error),
        ("RVWL-1003", "operations", ConfSeverity::Error),
        ("RVWL-1004", "operations", ConfSeverity::Warn),
    ];
    assert_eq!(
        rules.len(),
        expected.len(),
        "four docs, one rule each: {:?}",
        rules.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
    for (rule, (id, steering, severity)) in rules.iter().zip(expected) {
        assert_eq!(rule.id, id);
        assert_eq!(rule.steering_type, steering, "{id} steering_type");
        assert_eq!(rule.severity, severity, "{id} severity");
        assert!(rule.effect.is_none(), "{id} is guidance: no effect");
        assert!(rule.trigger.is_none(), "{id} is guidance: no trigger");
        let reference = rule
            .provenance
            .reference
            .as_deref()
            .unwrap_or_else(|| panic!("{id} carries a provenance ref"));
        let parsed = parse_provenance_ref(reference);
        assert!(
            parsed.path.starts_with("review-loop-") && parsed.path.ends_with(".md"),
            "{id} ref path: {reference}"
        );
        assert_eq!(parsed.anchor.as_deref(), Some(id), "{id} ref anchor");
        let sha = parsed
            .sha
            .unwrap_or_else(|| panic!("{id} ref carries a blob sha: {reference}"));
        assert_eq!(sha.len(), 40, "{id} ref sha: {reference}");
    }

    let groupings = adapter.groupings().expect("RuleSet groupings read");
    assert_eq!(groupings.len(), 1, "one RuleSet: {groupings:?}");
    assert_eq!(groupings[0].domain, "review-loop");
    let mut members = groupings[0].rule_ids.clone();
    members.sort();
    assert_eq!(
        members,
        expected
            .iter()
            .map(|(id, _, _)| id.to_string())
            .collect::<Vec<_>>(),
        "all four rules sit under RuleSet review-loop"
    );
}
