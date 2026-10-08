//! The shipped `governance/packs/mcp-server` steering pack ingests through the ONE markdown parse
//! path (`MarkdownAdapter` → `normalize_bundle`) as seven recall-only guidance rules — the
//! doctrine the `mcp-server` workflow's design and review phases recall.

use std::path::PathBuf;

use wicked_governance::{ingest_from, parse_provenance_ref, ConfSeverity, MarkdownAdapter};

fn pack_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../governance/packs/mcp-server")
}

#[test]
fn mcp_server_pack_ingests_as_seven_guidance_rules() {
    let adapter = MarkdownAdapter::new(pack_root());
    let mut rules = ingest_from(&adapter).expect("the mcp-server pack ingests");
    rules.sort_by(|a, b| a.id.cmp(&b.id));
    let expected: [(&str, &str, ConfSeverity); 7] = [
        ("MCPS-1001", "architecture", ConfSeverity::Error),
        ("MCPS-1002", "operations", ConfSeverity::Error),
        ("MCPS-1003", "operations", ConfSeverity::Error),
        ("MCPS-1004", "security", ConfSeverity::Critical),
        ("MCPS-1005", "security", ConfSeverity::Error),
        ("MCPS-1006", "testing", ConfSeverity::Error),
        ("MCPS-1007", "compliance", ConfSeverity::Warn),
    ];
    assert_eq!(
        rules.len(),
        expected.len(),
        "seven docs, one rule each: {:?}",
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
            parsed.path.starts_with("mcp-server-") && parsed.path.ends_with(".md"),
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
    assert_eq!(groupings[0].domain, "mcp-server");
    let mut members = groupings[0].rule_ids.clone();
    members.sort();
    assert_eq!(
        members,
        expected
            .iter()
            .map(|(id, _, _)| id.to_string())
            .collect::<Vec<_>>(),
        "all seven rules sit under RuleSet mcp-server"
    );
}
