//! Fan-out contract across the deliberate store split (AW-5 / arch-R3, decision record
//! `.product/DES-OUTGOV-008-fanout-placement.md`).
//!
//! Enforcement and discovery read DIFFERENT stores in the same governed run (deliberate,
//! post-FINDING-067): the gate hook reads the operational store handed via `WICKED_GATE_DB`, the
//! injected estate MCP reads the run repo's (or verified project's) code graph, and guidance recall
//! reads a knowledge store. An import into one home silently misses the other lanes — so an import
//! is not "done" until ONE manifest, keyed on the stable `PAT-`/`POL-` rule ids, maps every rule to
//! its copy in each lane AND each lane has been smoke-verified through the same read path a
//! governed run uses:
//!
//! 1. **enforcement** — the store the gate hook selects/recalls from. Offline stores are written
//!    here via [`fanout`] (the `wicked-core rules fanout` CLI); a store owned by a live crew daemon
//!    is NEVER written by this CLI (single-writer invariant, `gate_hook.rs`) — the manifest records
//!    the `crew-api` transport instead and the copies travel over
//!    `POST /api/v1/governance/{policies,rules}`.
//! 2. **discovery** — the repo/project code graph(s) a worker's estate MCP binds. Conformance rules
//!    replicate here as native `NodeKind::Rule` nodes (what `rules.recall` / `RulesInventory`
//!    serve). Deny-path [`Policy`] objects do NOT replicate — they are enforcement-lane machinery.
//! 3. **knowledge** — one rationale chunk per rule, id-keyed (`rule-rationale/<ID>`, so re-ingest
//!    UPSERTS instead of duplicating), `source` = the rule's `provenance.ref` (the wiki URI), with
//!    the `PAT-`/`POL-` id embedded in the chunk text so a cited `knowledge.recall` answer can name
//!    the enforceable twin.
//!
//! Smoke verification re-opens every store FRESH after the write handles drop (graph stores
//! READ-ONLY, the gate hook's own open), so what it proves is the durable state the next process
//! (the worker) will actually see. Any missing copy fails the
//! whole fan-out LOUD — a partial import that reads as "governed" is the exact fail-open this
//! contract exists to prevent. Addition only: retirement propagation is AW-24 (arch-R22).
//!
//! ## `scope: workspace` (AW-6 / arch-R20)
//!
//! Cross-repo doctrine (root CLAUDE.md decisions, TARGET-ARCHITECTURE planes, event grammar) has no
//! single-store home a worker in ANY repo would read: graphs and knowledge sidecars are
//! per-repo/per-project and edges do not resolve across repos. Decision (AW-6):
//! **replicate-to-every-repo** — a manifest with [`FanoutScope::Workspace`] carries one discovery
//! copy per live repo graph (the caller enumerates them; this crate refuses a workspace fan-out
//! with zero discovery targets). Zero engine change; the id-keyed idempotent re-ingest is what
//! makes N copies tractable to keep in sync. The alternative (a workspace-root store plus new
//! multi-`--db` resolution machinery in estate and the gate) is documented and PARKED as P-2 in
//! DES-OUTGOV-008 — revisit only if replication cost bites.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use wicked_apps_core::{open_store, synthetic_symbol, FromNode, GraphRead, GraphStore, POLICY};
use wicked_estate_knowledge::{KClass, KNode, KnowledgeEngine};

use crate::conformance::{recall_rules, ConfSeverity, ConformanceRule, RuleQuery, RuleType};
use crate::domain::Policy;
use crate::engine::register_policy;
use crate::ingest::{ingest_from, FilesystemAdapter};
use crate::markdown::MarkdownAdapter;

/// Manifest wire version. Bump on any breaking change to the serialized shape.
pub const FANOUT_MANIFEST_VERSION: &str = "1.0";

/// Stable knowledge-chunk id namespace: chunk id = `rule-rationale/<RULE-ID>`. A stable id makes
/// the knowledge write an UPSERT (the engine keys nodes on `Symbol::synthetic(class, id)`), so
/// re-running a fan-out refreshes the rationale instead of accreting duplicates — the id-keyed
/// idempotence the workspace replication decision (AW-6) leans on.
pub const KNOWLEDGE_CHUNK_PREFIX: &str = "rule-rationale";

/// Where doctrine replicates (AW-6 / arch-R20). Serialized into the manifest as
/// `"repo" | "workspace"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanoutScope {
    /// Single-repo doctrine: the discovery lane is that repo's graph (and optionally its verified
    /// project graph).
    Repo,
    /// Cross-repo doctrine: replicate-to-every-repo. The discovery lane MUST carry one copy per
    /// live repo graph — the caller enumerates them, and zero targets is an error, because a
    /// workspace rule that lands in no repo graph is recallable by no governed worker.
    Workspace,
}

/// How the enforcement copy reaches its store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnforcementTarget {
    /// An OFFLINE store (no daemon holds it): this process writes and smoke-verifies directly.
    Cli { db: String },
    /// A store owned by a live crew daemon: NEVER written from this process (single-writer
    /// invariant). The manifest records the transport + target; the copies travel over
    /// `POST /api/v1/governance/{policies,rules}` and are verified out-of-process via
    /// `GET /api/v1/governance/rules/preview`.
    CrewApi { url: String },
}

/// The lane targets one fan-out run writes/records.
#[derive(Debug, Clone)]
pub struct FanoutTargets {
    pub scope: FanoutScope,
    pub enforcement: EnforcementTarget,
    /// Repo/project code-graph stores (one per live repo under [`FanoutScope::Workspace`]).
    pub discovery_dbs: Vec<String>,
    /// Knowledge stores the workers' guidance recall reads.
    pub knowledge_dbs: Vec<String>,
    /// Knowledge scope stamped on every rationale chunk (arch-R5 convention: `wiki:<area>`).
    pub knowledge_scope: String,
}

/// A ruleset loaded from the canonical `<dir>` layout (`policies/*.json`, `rules/*.json`,
/// frontmattered `**/*.md`) — the same layout `wicked-core rules ingest` consumes.
#[derive(Debug, Clone)]
pub struct RulesetLoad {
    pub policies: Vec<Policy>,
    pub rules: Vec<ConformanceRule>,
}

/// The enforcement lane as recorded in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnforcementLane {
    /// `"cli"` (offline store, written + verified here) or `"crew-api"` (daemon store, pending).
    pub transport: String,
    /// The store path (cli) or API base URL (crew-api).
    pub target: String,
    /// True only after the lane's smoke check passed IN THIS PROCESS. A `crew-api` lane is always
    /// recorded `false` here — its verification lives with the crew daemon (`rules/preview`).
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One verified store target in the discovery or knowledge lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneTarget {
    pub db: String,
    pub verified: bool,
}

/// Where one conformance rule's copies landed — the per-rule row of the manifest, keyed by the
/// rule's stable `PAT-`/`POL-` id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleEntry {
    pub rule_type: RuleType,
    pub severity: ConfSeverity,
    pub statement: String,
    /// The rule's `provenance.ref` — the wiki URI the rationale chunk cites as `source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// (a) the enforcement copy: `cli:<db>` or `crew-api:<url> (pending)`.
    pub enforcement: String,
    /// (b) the discovery graph copies (one db per live repo under `scope: workspace`).
    pub discovery: Vec<String>,
    /// (c) the knowledge rationale chunks: `<db>#kchunk:rule-rationale/<ID>`.
    pub knowledge: Vec<String>,
}

/// Where one deny-path policy's copy landed. Policies are enforcement-lane machinery (regex
/// triggers over evaluated contexts) — they get no discovery or knowledge twin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyEntry {
    pub enforcement: String,
}

/// The fan-out manifest — one import's receipt, keyed on stable rule ids. Serialized to JSON by
/// the `wicked-core rules fanout` CLI; the smoke checks have already passed for every lane marked
/// `verified` (the fan-out FAILS rather than emit a manifest with an unverified cli lane).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FanoutManifest {
    pub manifest_version: String,
    pub scope: FanoutScope,
    pub source_dir: String,
    pub generated_at: i64,
    pub enforcement: EnforcementLane,
    pub discovery: Vec<LaneTarget>,
    pub knowledge: Vec<LaneTarget>,
    /// Conformance rules, keyed on `PAT-`/`POL-` id (BTreeMap ⇒ deterministic serialization).
    pub rules: BTreeMap<String, RuleEntry>,
    /// Deny-path policies, keyed on policy id.
    pub policies: BTreeMap<String, PolicyEntry>,
}

/// Load the canonical ruleset layout under `root`, fail-loud with the same semantics as
/// `wicked-core rules ingest`: a malformed file errors with path + reason; duplicate rule ids
/// across the JSON and markdown lanes error (both map to `conformance_rule/<id>`); duplicate
/// policy ids error (a Deny silently clobbered by a weaker policy is a fail-open); an EMPTY
/// effective load errors (a no-op import that reads as "governed" while enforcing nothing).
pub fn load_ruleset(root: &Path) -> anyhow::Result<RulesetLoad> {
    let mut rules: Vec<ConformanceRule> = Vec::new();
    let mut seen_rule_ids: HashSet<String> = HashSet::new();

    // Conformance rules, JSON lane.
    let rules_dir = root.join("rules");
    if rules_dir.is_dir() {
        for rule in ingest_from(&FilesystemAdapter::new(&rules_dir))
            .map_err(|e| anyhow::anyhow!("reading conformance rules under {rules_dir:?}: {e}"))?
        {
            seen_rule_ids.insert(rule.id.clone());
            rules.push(rule);
        }
    }

    // Conformance rules, markdown lane (AW-3) — same normalize_bundle fail-closed path.
    for rule in ingest_from(&MarkdownAdapter::new(root))
        .map_err(|e| anyhow::anyhow!("reading markdown rule docs under {root:?}: {e}"))?
    {
        if !seen_rule_ids.insert(rule.id.clone()) {
            anyhow::bail!(
                "rule id {:?} appears in BOTH a rules/*.json bundle and a markdown doc ({}) — \
                 the later write would silently overwrite the earlier at conformance_rule/<id>; \
                 refusing (fail-loud)",
                rule.id,
                rule.provenance.reference.as_deref().unwrap_or("?")
            );
        }
        rules.push(rule);
    }

    // Deny-path policies.
    let mut policies: Vec<Policy> = Vec::new();
    let policies_dir = root.join("policies");
    if policies_dir.is_dir() {
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        let rd = std::fs::read_dir(&policies_dir)
            .map_err(|e| anyhow::anyhow!("cannot read {policies_dir:?}: {e}"))?;
        for entry in rd {
            // Propagate a mid-readdir fault — a silent skip would truncate the DENY set (fail-open).
            let entry =
                entry.map_err(|e| anyhow::anyhow!("cannot enumerate {policies_dir:?}: {e}"))?;
            let p = entry.path();
            if p.extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("json"))
            {
                files.push(p);
            }
        }
        files.sort(); // deterministic load order
        let mut seen_policy_ids: HashSet<String> = HashSet::new();
        for p in files {
            let text = std::fs::read_to_string(&p)
                .map_err(|e| anyhow::anyhow!("cannot read {p:?}: {e}"))?;
            // Dispatch on the JSON SHAPE so a malformed object surfaces the SPECIFIC Policy error.
            let is_array = text.trim_start().starts_with('[');
            let parsed: Result<Vec<Policy>, _> = if is_array {
                serde_json::from_str::<Vec<Policy>>(&text)
            } else {
                serde_json::from_str::<Policy>(&text).map(|p| vec![p])
            };
            let file_policies = parsed.map_err(|e| {
                let shape = if is_array { "[Policy]" } else { "Policy" };
                anyhow::anyhow!("{p:?} is not a valid {shape}: {e}")
            })?;
            for pol in file_policies {
                if !seen_policy_ids.insert(pol.id.clone()) {
                    anyhow::bail!(
                        "duplicate policy id {:?} across policies/*.json — a later policy would \
                         silently overwrite an earlier one at policy/<id>; refusing (fail-loud)",
                        pol.id
                    );
                }
                policies.push(pol);
            }
        }
    }

    if policies.is_empty() && rules.is_empty() {
        anyhow::bail!(
            "NO policies or conformance rules found under {root:?} (expected <dir>/policies/*.json, \
             <dir>/rules/*.json, and/or frontmattered markdown rule docs) — refusing an empty \
             population (fail-loud)"
        );
    }
    Ok(RulesetLoad { policies, rules })
}

/// The rationale chunk text for one rule. Embeds the stable `PAT-`/`POL-` id (design spine: one
/// recalled answer surfaces the guidance AND names its machine-enforced twin) plus the statement
/// and the wiki source, so keyword recall on either the id or the statement's terms lands here.
pub fn rationale_chunk(rule: &ConformanceRule) -> String {
    let severity = match rule.severity {
        ConfSeverity::Info => "info",
        ConfSeverity::Warn => "warn",
        ConfSeverity::Error => "error",
        ConfSeverity::Critical => "critical",
    };
    let rule_type = match rule.rule_type {
        RuleType::Pattern => "pattern",
        RuleType::Policy => "policy",
    };
    let source = rule.provenance.reference.as_deref().unwrap_or("unknown");
    format!(
        "{id} ({severity} {rule_type} rule): {statement} — rationale source: {source}",
        id = rule.id,
        statement = rule.statement.trim(),
    )
}

/// The stable knowledge-chunk id for a rule (`rule-rationale/<RULE-ID>`).
pub fn rationale_chunk_id(rule_id: &str) -> String {
    format!("{KNOWLEDGE_CHUNK_PREFIX}/{rule_id}")
}

/// Register every conformance rule (and, when `include_policies`, every deny-path policy) into one
/// graph-lane store. Enforcement gets both; discovery graphs get `NodeKind::Rule` copies only.
fn write_graph_lane(
    store: &mut dyn GraphStore,
    load: &RulesetLoad,
    include_policies: bool,
) -> anyhow::Result<()> {
    for rule in &load.rules {
        crate::conformance::register_rule(store, rule)?;
    }
    if include_policies {
        for policy in &load.policies {
            register_policy(store, policy)?;
        }
    }
    Ok(())
}

/// Smoke a graph-lane store through the SAME read paths a governed run uses: `recall_rules` (what
/// the gate's recall→obligation step and the MCP `rules.recall` serve) must return every expected
/// rule id, and (enforcement only) every policy node must round-trip active. Returns the missing
/// ids — empty means verified.
fn smoke_graph_lane(
    store: &dyn GraphRead,
    expect_rules: &[String],
    expect_policies: &[String],
) -> anyhow::Result<Vec<String>> {
    let recalled = recall_rules(store, &RuleQuery::default())?;
    let present: HashSet<&str> = recalled.iter().map(|r| r.id.as_str()).collect();
    let mut missing: Vec<String> = expect_rules
        .iter()
        .filter(|id| !present.contains(id.as_str()))
        .cloned()
        .collect();
    for id in expect_policies {
        let ok = match store.get_node(&synthetic_symbol(POLICY, id))? {
            Some(node) => Policy::from_node(&node)
                .map(|p| !p.retired)
                .unwrap_or(false),
            None => false,
        };
        if !ok {
            missing.push(format!("policy/{id}"));
        }
    }
    Ok(missing)
}

/// Write every rule's rationale chunk into one knowledge store, id-keyed (idempotent upsert).
fn write_knowledge_lane(
    engine: &mut KnowledgeEngine,
    rules: &[ConformanceRule],
    scope: &str,
    now: i64,
) -> anyhow::Result<()> {
    for rule in rules {
        let kn = KNode {
            id: rationale_chunk_id(&rule.id),
            class: KClass::Chunk,
            content: rationale_chunk(rule),
            scope: scope.to_string(),
            source: rule
                .provenance
                .reference
                .clone()
                .unwrap_or_else(|| rule.provenance.source.clone()),
            created_at: now,
        };
        engine
            .write(&kn)
            .map_err(|e| anyhow::anyhow!("write rationale chunk for {}: {e}", rule.id))?;
    }
    Ok(())
}

/// Smoke a knowledge store through its real read path: `recall` on each rule id must return a
/// chunk whose content carries that id. Returns the missing ids — empty means verified.
fn smoke_knowledge_lane(
    engine: &mut KnowledgeEngine,
    expect_rules: &[String],
    now: i64,
) -> anyhow::Result<Vec<String>> {
    let mut missing = Vec::new();
    for id in expect_rules {
        let hits = engine
            .recall(id, 1024, now)
            .map_err(|e| anyhow::anyhow!("knowledge recall for {id}: {e}"))?;
        if !hits.iter().any(|h| h.content.contains(id.as_str())) {
            missing.push(id.clone());
        }
    }
    Ok(missing)
}

/// Fan a loaded ruleset out across the deliberate store split and smoke-verify every lane this
/// process wrote. Returns the manifest ONLY when every cli-written lane verified — a partial
/// fan-out is an error naming the lane, the target, and the missing ids, never a receipt.
///
/// The smoke re-opens each store FRESH (after the write handle drops), so it proves the durable
/// state the worker-visible `--db` will serve to the next process, not this process's cache.
pub fn fanout(
    load: &RulesetLoad,
    targets: &FanoutTargets,
    source_dir: &str,
    now: i64,
) -> anyhow::Result<FanoutManifest> {
    if targets.discovery_dbs.is_empty() {
        anyhow::bail!(
            "fan-out has no discovery targets: {} doctrine must land in {} (AW-6/arch-R20 — a rule \
             in no repo graph is recallable by no governed worker); pass at least one --discovery-db",
            match targets.scope {
                FanoutScope::Workspace => "scope:workspace",
                FanoutScope::Repo => "scope:repo",
            },
            match targets.scope {
                FanoutScope::Workspace => "EVERY live repo's graph",
                FanoutScope::Repo => "the repo's graph",
            },
        );
    }
    if targets.knowledge_dbs.is_empty() {
        anyhow::bail!(
            "fan-out has no knowledge targets: every rule carries a rationale chunk (arch-R3 lane \
             3); pass at least one --knowledge-db"
        );
    }

    let rule_ids: Vec<String> = load.rules.iter().map(|r| r.id.clone()).collect();
    let policy_ids: Vec<String> = load.policies.iter().map(|p| p.id.clone()).collect();

    // Lane 1: enforcement.
    let enforcement = match &targets.enforcement {
        EnforcementTarget::Cli { db } => {
            {
                let mut store = open_store(Some(db))
                    .map_err(|e| anyhow::anyhow!("enforcement lane: open {db:?}: {e}"))?;
                write_graph_lane(&mut store, load, true)
                    .map_err(|e| anyhow::anyhow!("enforcement lane {db:?}: {e}"))?;
            } // write handle dropped before the smoke re-open
              // Re-open READ-ONLY — the exact open the gate hook itself performs (`open_store_ro`,
              // no WAL pragma, no schema DDL), so the smoke cannot repair what it is verifying.
            let store = wicked_apps_core::open_store_ro(Some(db))
                .map_err(|e| anyhow::anyhow!("enforcement lane: re-open {db:?}: {e}"))?;
            let missing = smoke_graph_lane(&store, &rule_ids, &policy_ids)?;
            if !missing.is_empty() {
                anyhow::bail!(
                    "enforcement lane smoke FAILED against {db:?}: missing {missing:?} — the \
                     import did not land where the gate hook reads"
                );
            }
            EnforcementLane {
                transport: "cli".to_string(),
                target: db.clone(),
                verified: true,
                note: None,
            }
        }
        EnforcementTarget::CrewApi { url } => EnforcementLane {
            transport: "crew-api".to_string(),
            target: url.clone(),
            verified: false,
            note: Some(
                "daemon-held store: single-writer invariant forbids CLI writes. POST the \
                 enforcement payload to /api/v1/governance/policies and /api/v1/governance/rules, \
                 then verify via GET /api/v1/governance/rules/preview"
                    .to_string(),
            ),
        },
    };

    // Lane 2: discovery — rules only, one copy per graph.
    let mut discovery = Vec::with_capacity(targets.discovery_dbs.len());
    for db in &targets.discovery_dbs {
        {
            let mut store = open_store(Some(db))
                .map_err(|e| anyhow::anyhow!("discovery lane: open {db:?}: {e}"))?;
            write_graph_lane(&mut store, load, false)
                .map_err(|e| anyhow::anyhow!("discovery lane {db:?}: {e}"))?;
        }
        // Read-only re-open, same reasoning as the enforcement smoke.
        let store = wicked_apps_core::open_store_ro(Some(db))
            .map_err(|e| anyhow::anyhow!("discovery lane: re-open {db:?}: {e}"))?;
        let missing = smoke_graph_lane(&store, &rule_ids, &[])?;
        if !missing.is_empty() {
            anyhow::bail!(
                "discovery lane smoke FAILED against {db:?}: missing {missing:?} — the import did \
                 not land where the worker's estate MCP reads"
            );
        }
        discovery.push(LaneTarget {
            db: db.clone(),
            verified: true,
        });
    }

    // Lane 3: knowledge — rationale chunks, id-keyed.
    let mut knowledge = Vec::with_capacity(targets.knowledge_dbs.len());
    for db in &targets.knowledge_dbs {
        {
            let mut engine = KnowledgeEngine::open(db)
                .map_err(|e| anyhow::anyhow!("knowledge lane: open {db:?}: {e}"))?;
            write_knowledge_lane(&mut engine, &load.rules, &targets.knowledge_scope, now)?;
        }
        let mut engine = KnowledgeEngine::open(db)
            .map_err(|e| anyhow::anyhow!("knowledge lane: re-open {db:?}: {e}"))?;
        let missing = smoke_knowledge_lane(&mut engine, &rule_ids, now)?;
        if !missing.is_empty() {
            anyhow::bail!(
                "knowledge lane smoke FAILED against {db:?}: no recallable rationale for \
                 {missing:?} — the import did not land where guidance recall reads"
            );
        }
        knowledge.push(LaneTarget {
            db: db.clone(),
            verified: true,
        });
    }

    // The receipt: every rule mapped to its three copies, keyed on the stable id.
    let enforcement_ref = match &targets.enforcement {
        EnforcementTarget::Cli { db } => format!("cli:{db}"),
        EnforcementTarget::CrewApi { url } => format!("crew-api:{url} (pending)"),
    };
    let mut rules = BTreeMap::new();
    for rule in &load.rules {
        rules.insert(
            rule.id.clone(),
            RuleEntry {
                rule_type: rule.rule_type,
                severity: rule.severity,
                statement: rule.statement.clone(),
                source: rule.provenance.reference.clone(),
                enforcement: enforcement_ref.clone(),
                discovery: targets.discovery_dbs.clone(),
                knowledge: targets
                    .knowledge_dbs
                    .iter()
                    .map(|db| format!("{db}#kchunk:{}", rationale_chunk_id(&rule.id)))
                    .collect(),
            },
        );
    }
    let mut policies = BTreeMap::new();
    for policy in &load.policies {
        policies.insert(
            policy.id.clone(),
            PolicyEntry {
                enforcement: enforcement_ref.clone(),
            },
        );
    }

    Ok(FanoutManifest {
        manifest_version: FANOUT_MANIFEST_VERSION.to_string(),
        scope: targets.scope,
        source_dir: source_dir.to_string(),
        generated_at: now,
        enforcement,
        discovery,
        knowledge,
        rules,
        policies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::RuleProvenance;
    use crate::Targets;

    fn rule(id: &str) -> ConformanceRule {
        ConformanceRule {
            id: id.to_string(),
            rule_type: if id.starts_with("PAT-") {
                RuleType::Pattern
            } else {
                RuleType::Policy
            },
            statement: "No printf without %s".to_string(),
            severity: ConfSeverity::Error,
            confidence: 0.9,
            targets: Targets::default(),
            symbol_ref: None,
            compliance: None,
            provenance: RuleProvenance {
                source: "markdown".to_string(),
                reference: Some("docs/cross-platform.md#PAT-001".to_string()),
                source_kinds: vec!["doc".to_string()],
            },
            retired: false,
        }
    }

    #[test]
    fn rationale_chunk_embeds_id_statement_and_source() {
        let text = rationale_chunk(&rule("PAT-001"));
        assert!(
            text.contains("PAT-001"),
            "the enforceable twin's id: {text}"
        );
        assert!(text.contains("No printf without %s"), "statement: {text}");
        assert!(
            text.contains("docs/cross-platform.md#PAT-001"),
            "wiki URI: {text}"
        );
        assert!(text.contains("error pattern rule"), "class: {text}");
    }

    #[test]
    fn chunk_id_is_stable_and_namespaced() {
        assert_eq!(rationale_chunk_id("POL-002"), "rule-rationale/POL-002");
    }

    #[test]
    fn workspace_scope_with_no_discovery_targets_fails_loud() {
        let load = RulesetLoad {
            policies: vec![],
            rules: vec![rule("PAT-001")],
        };
        let targets = FanoutTargets {
            scope: FanoutScope::Workspace,
            enforcement: EnforcementTarget::Cli {
                db: ":memory:".to_string(),
            },
            discovery_dbs: vec![],
            knowledge_dbs: vec![":memory:".to_string()],
            knowledge_scope: "wiki:governance".to_string(),
        };
        let err = fanout(&load, &targets, "ruleset", 1_750_000_000)
            .expect_err("a workspace rule that lands in no repo graph must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("scope:workspace") && msg.contains("EVERY live repo"),
            "the error must teach the AW-6 decision: {msg}"
        );
    }

    #[test]
    fn missing_knowledge_targets_fail_loud() {
        let load = RulesetLoad {
            policies: vec![],
            rules: vec![rule("PAT-001")],
        };
        let targets = FanoutTargets {
            scope: FanoutScope::Repo,
            enforcement: EnforcementTarget::Cli {
                db: ":memory:".to_string(),
            },
            discovery_dbs: vec![":memory:".to_string()],
            knowledge_dbs: vec![],
            knowledge_scope: "wiki:governance".to_string(),
        };
        let err = fanout(&load, &targets, "ruleset", 1_750_000_000).expect_err("lane 3 required");
        assert!(err.to_string().contains("knowledge"), "{err}");
    }

    #[test]
    fn manifest_serializes_with_snake_case_scope() {
        let manifest = FanoutManifest {
            manifest_version: FANOUT_MANIFEST_VERSION.to_string(),
            scope: FanoutScope::Workspace,
            source_dir: "ruleset".to_string(),
            generated_at: 1,
            enforcement: EnforcementLane {
                transport: "cli".to_string(),
                target: "gov.db".to_string(),
                verified: true,
                note: None,
            },
            discovery: vec![],
            knowledge: vec![],
            rules: BTreeMap::new(),
            policies: BTreeMap::new(),
        };
        let json = serde_json::to_value(&manifest).unwrap();
        assert_eq!(json["scope"], "workspace");
        assert_eq!(json["manifest_version"], "1.0");
        // Round-trips.
        let back: FanoutManifest = serde_json::from_value(json).unwrap();
        assert_eq!(back, manifest);
    }
}
