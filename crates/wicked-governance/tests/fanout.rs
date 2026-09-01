//! AW-5 acceptance: a rule ingested once lands in ALL lanes a governed run reads — the enforcement
//! store (gate hook `--db`), every discovery graph (the worker-visible estate MCP `--db`), and the
//! knowledge store (guidance recall) — with the smoke proof running through the same read paths
//! those consumers use, against FRESH store handles (durability, not cache). Plus the AW-6
//! `scope: workspace` semantics: one discovery copy per live repo graph, id-keyed idempotent
//! re-ingest.

use std::path::{Path, PathBuf};

use wicked_apps_core::{synthetic_symbol, FromNode, GraphRead, POLICY};
use wicked_estate_knowledge::{KClass, KnowledgeEngine};
use wicked_governance::{
    fanout, load_ruleset, rationale_chunk_id, recall_rules, EnforcementTarget, FanoutScope,
    FanoutTargets, Policy, RuleQuery, FANOUT_MANIFEST_VERSION,
};

fn scratch(name: &str) -> PathBuf {
    // core#311: `fanout` registers rules, whose fire-and-forget `rule.ingested` emissions would
    // otherwise spool to the operator's real `~/.something-wicked` replay queue. Every test's
    // first act is this fixture, so arming here precedes any emission in every thread.
    wicked_apps_core::emit::hermetic_test_spool();
    let dir = std::env::temp_dir().join(format!("wg-fanout-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A ruleset exercising all three source lanes: one deny policy, one JSON conformance rule, one
/// markdown conformance rule.
fn write_ruleset(base: &Path) -> PathBuf {
    let ruleset = base.join("ruleset");
    std::fs::create_dir_all(ruleset.join("policies")).unwrap();
    std::fs::create_dir_all(ruleset.join("rules")).unwrap();
    std::fs::write(
        ruleset.join("policies/deny.json"),
        serde_json::json!({
            "id": "pol-deny-secretleak",
            "kind": "security",
            "applies_to": ["build"],
            "effect": "deny",
            "trigger": { "contains": "SECRETLEAK" },
            "obligations": [],
            "criteria": "no secret material in generated output",
            "severity": "high",
            "rule": "Deny any output that embeds a SECRETLEAK marker."
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        ruleset.join("rules/bundle.json"),
        serde_json::json!({ "rules": [
            { "id": "PAT-001", "rule_type": "pattern", "statement": "no plaintext secrets",
              "severity": "critical", "confidence": 0.95,
              "provenance": { "ref": "wiki://secure-coding#PAT-001", "source_kinds": ["doc"] } }
        ]})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        ruleset.join("event-grammar.md"),
        "---\n\
         id: event-grammar\n\
         title: Event grammar\n\
         scope: wiki:event-grammar\n\
         ---\n\n\
         ## Rules\n\n\
         - `POL-100` (error): Event types are 4-segment wicked.<domain>.<noun>.<verb>.\n",
    )
    .unwrap();
    ruleset
}

fn targets(base: &Path, scope: FanoutScope, discovery: &[&str]) -> FanoutTargets {
    FanoutTargets {
        scope,
        enforcement: EnforcementTarget::Cli {
            db: base.join("gov.db").to_string_lossy().into_owned(),
        },
        discovery_dbs: discovery
            .iter()
            .map(|n| base.join(n).to_string_lossy().into_owned())
            .collect(),
        knowledge_dbs: vec![base.join("knowledge.db").to_string_lossy().into_owned()],
        knowledge_scope: "wiki:governance".to_string(),
    }
}

/// The AW-5 acceptance path: one import, three lanes, smoke-verified; the manifest maps every
/// PAT-/POL- id to its copies; and each lane's REAL read path serves the rule from a fresh handle.
#[test]
fn one_import_lands_in_every_lane_a_governed_run_reads() {
    let base = scratch("all-lanes");
    let ruleset = write_ruleset(&base);
    let load = load_ruleset(&ruleset).expect("well-formed ruleset loads");
    assert_eq!(load.policies.len(), 1);
    assert_eq!(load.rules.len(), 2, "JSON + markdown lanes both loaded");

    // scope:workspace — TWO live repo graphs, per the AW-6 replicate-to-every-repo decision.
    let t = targets(&base, FanoutScope::Workspace, &["repo-a.db", "repo-b.db"]);
    let manifest =
        fanout(&load, &t, ruleset.to_str().unwrap(), 1_750_000_000).expect("fan-out verifies");

    // The manifest is the receipt: keyed on stable ids, every lane recorded verified.
    assert_eq!(manifest.manifest_version, FANOUT_MANIFEST_VERSION);
    assert!(manifest.enforcement.verified, "cli lane smoke passed");
    assert_eq!(manifest.discovery.len(), 2);
    assert!(manifest.discovery.iter().all(|l| l.verified));
    assert!(manifest.knowledge.iter().all(|l| l.verified));
    let pat = &manifest.rules["PAT-001"];
    assert!(pat.enforcement.starts_with("cli:"), "{}", pat.enforcement);
    assert_eq!(pat.discovery.len(), 2, "one copy per live repo graph");
    assert_eq!(
        pat.source.as_deref(),
        Some("wiki://secure-coding#PAT-001"),
        "the wiki URI rides the manifest"
    );
    assert!(
        pat.knowledge[0].ends_with("#kchunk:rule-rationale/PAT-001"),
        "{}",
        pat.knowledge[0]
    );
    assert!(manifest.rules.contains_key("POL-100"), "markdown rule too");
    assert!(manifest.policies.contains_key("pol-deny-secretleak"));

    // Independent re-verification, fresh handles, the consumers' own read paths:
    // (1) enforcement — what the gate hook selects/recalls from.
    let gov = wicked_apps_core::open_store(Some(&format!("{}", base.join("gov.db").display())))
        .expect("re-open enforcement store");
    let recalled = recall_rules(&gov, &RuleQuery::default()).unwrap();
    let ids: Vec<&str> = recalled.iter().map(|r| r.id.as_str()).collect();
    assert!(
        ids.contains(&"PAT-001") && ids.contains(&"POL-100"),
        "{ids:?}"
    );
    let pol_node = gov
        .get_node(&synthetic_symbol(POLICY, "pol-deny-secretleak"))
        .unwrap()
        .expect("deny policy present in the enforcement store");
    assert!(!Policy::from_node(&pol_node).unwrap().retired);

    // (2) discovery — what the worker-visible estate MCP serves; policies must NOT replicate here.
    for repo in ["repo-a.db", "repo-b.db"] {
        let store =
            wicked_apps_core::open_store(Some(&format!("{}", base.join(repo).display()))).unwrap();
        let recalled = recall_rules(&store, &RuleQuery::default()).unwrap();
        assert_eq!(recalled.len(), 2, "{repo} carries both rule copies");
        assert!(
            store
                .get_node(&synthetic_symbol(POLICY, "pol-deny-secretleak"))
                .unwrap()
                .is_none(),
            "deny-path policies are enforcement-lane machinery, not discovery doctrine"
        );
    }

    // (3) knowledge — guidance recall by the enforceable twin's id, source = the wiki URI.
    let mut know =
        KnowledgeEngine::open(&format!("{}", base.join("knowledge.db").display())).unwrap();
    let hits = know.recall("PAT-001", 1024, 1_750_000_000).unwrap();
    let hit = hits
        .iter()
        .find(|h| h.content.contains("PAT-001"))
        .expect("rationale chunk recallable by rule id");
    assert_eq!(hit.source, "wiki://secure-coding#PAT-001");
    assert!(hit.content.contains("no plaintext secrets"));
}

/// Re-running the same fan-out is an UPSERT everywhere (id-keyed): no duplicate rule nodes, no
/// accreting rationale chunks — the property that makes AW-6's N replicated copies syncable.
#[test]
fn refanning_out_is_idempotent_in_every_lane() {
    let base = scratch("idempotent");
    let ruleset = write_ruleset(&base);
    let load = load_ruleset(&ruleset).unwrap();
    let t = targets(&base, FanoutScope::Workspace, &["repo-a.db"]);

    fanout(&load, &t, ruleset.to_str().unwrap(), 1_750_000_000).expect("first run");
    fanout(&load, &t, ruleset.to_str().unwrap(), 1_750_000_100).expect("second run");

    let store =
        wicked_apps_core::open_store(Some(&format!("{}", base.join("repo-a.db").display())))
            .unwrap();
    assert_eq!(
        recall_rules(&store, &RuleQuery::default()).unwrap().len(),
        2,
        "rule copies upserted, not duplicated"
    );

    let know = KnowledgeEngine::open(&format!("{}", base.join("knowledge.db").display())).unwrap();
    assert_eq!(
        know.count(Some(KClass::Chunk)).unwrap(),
        2,
        "one rationale chunk per rule id, refreshed in place"
    );
    // The stable chunk id is what makes that true.
    assert_eq!(rationale_chunk_id("PAT-001"), "rule-rationale/PAT-001");
}

/// A daemon-held enforcement store is declared, never CLI-written: the manifest records the
/// crew-api transport as pending, no gov.db appears on disk, and the OTHER lanes still verify.
#[test]
fn crew_api_enforcement_is_recorded_pending_and_never_written_locally() {
    let base = scratch("crew-api");
    let ruleset = write_ruleset(&base);
    let load = load_ruleset(&ruleset).unwrap();
    let t = FanoutTargets {
        scope: FanoutScope::Repo,
        enforcement: EnforcementTarget::CrewApi {
            url: "http://127.0.0.1:7901/api/v1".to_string(),
        },
        discovery_dbs: vec![base.join("repo-a.db").to_string_lossy().into_owned()],
        knowledge_dbs: vec![base.join("knowledge.db").to_string_lossy().into_owned()],
        knowledge_scope: "wiki:governance".to_string(),
    };

    let manifest = fanout(&load, &t, ruleset.to_str().unwrap(), 1_750_000_000).unwrap();
    assert_eq!(manifest.enforcement.transport, "crew-api");
    assert!(
        !manifest.enforcement.verified,
        "a daemon lane cannot be verified from this process"
    );
    assert!(
        manifest
            .enforcement
            .note
            .as_deref()
            .unwrap_or("")
            .contains("single-writer"),
        "the note must teach WHY the CLI refused to write"
    );
    assert!(
        manifest.rules["PAT-001"].enforcement.contains("(pending)"),
        "per-rule rows carry the pending state"
    );
    assert!(
        !base.join("gov.db").exists(),
        "no local enforcement store may be created for a daemon target"
    );
    assert!(manifest.discovery[0].verified && manifest.knowledge[0].verified);
}

/// load_ruleset keeps `rules ingest`'s fail-loud contract: empty load and cross-lane duplicate
/// ids refuse.
#[test]
fn load_ruleset_fails_loud_on_empty_and_cross_lane_duplicates() {
    let base = scratch("load-fail");
    let empty = base.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let err = load_ruleset(&empty).expect_err("empty population must refuse");
    assert!(
        err.to_string().contains("refusing an empty population"),
        "{err}"
    );

    // The same id in the JSON and markdown lanes: both map to conformance_rule/<id>.
    let dup = base.join("dup");
    std::fs::create_dir_all(dup.join("rules")).unwrap();
    std::fs::write(
        dup.join("rules/a.json"),
        serde_json::json!({ "rules": [
            { "id": "PAT-001", "rule_type": "pattern", "statement": "a", "severity": "info",
              "confidence": 0.5, "provenance": { "ref": "x", "source_kinds": ["doc"] } }
        ]})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        dup.join("doc.md"),
        "---\nid: doc\ntitle: Doc\n---\n\n## Rules\n\n- `PAT-001` (info): b.\n",
    )
    .unwrap();
    let err = load_ruleset(&dup).expect_err("cross-lane duplicate must refuse");
    assert!(err.to_string().contains("BOTH"), "{err}");
}
