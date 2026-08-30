//! Bad-rule kill switch — emergency retirement propagated across every fan-out lane in ONE
//! manifest-keyed operation (AW-24 / arch-R22).
//!
//! Under deny-dominates, one mis-authored critical rule bricks every governed run that recalls
//! it. Per-store withdrawal already exists ([`crate::retire_rule`] / [`crate::retire_policy`],
//! FINDING-038) and crew's API is retire-not-delete and audited — but the fan-out contract
//! ([`crate::fanout`]) is ADDITION-only: a rule imported once lives as an enforcement copy, N
//! discovery-graph copies, and a knowledge rationale chunk. Retiring only the copy in front of
//! you leaves the others silently serving a withdrawn rule. This module is the other half of the
//! contract:
//!
//! ```text
//!   wicked-core rules retire --id PAT-X --manifest M
//!     └─► for PAT-X's row in M (keyed on the stable PAT-/POL- id):
//!           enforcement copy  → retired  (cli store: written here; daemon store: PENDING note,
//!                                         retire over DELETE /api/v1/governance/{rules,policies}/:id)
//!           discovery copies  → retired  (every graph db the manifest row names)
//!           knowledge chunk   → content prefixed with the [RETIRED …] marker (non-normative,
//!                                         original rationale preserved for past-decision forensics)
//! ```
//!
//! Every lane this process writes is then RE-OPENED FRESH and verified through the same read path
//! a governed run uses: `recall_rules` must no longer serve the id (graph lanes) and the chunk
//! must carry the marker (knowledge lane). The receipt ([`RetireReceipt`]) records the per-lane
//! outcome — it is the retirement twin of the fan-out manifest.
//!
//! ## Doctrine
//!
//! - **Operator-only.** Retirement is a human emergency action (arch-R22 item 1 — R8's authorship
//!   contract applies in reverse): the CLI is an operator tool, crew's DELETE routes are audited
//!   operator surface, and no agent path calls this module. A worker that dislikes a rule argues
//!   in the run transcript, not the rule store.
//! - **Retire, not delete.** Same contract as [`crate::retire_policy`]: nodes survive so past
//!   decisions citing the id stay explicable; the knowledge chunk keeps its original text after
//!   the marker for the same reason.
//! - **Attempt every lane, then report.** A mid-way bail would leave earlier lanes retired and
//!   later ones unknown — the exact silent partial state a kill switch exists to prevent. Lane
//!   faults are collected into the receipt ([`LaneStatus::Failed`]) and surface through
//!   [`RetireReceipt::all_cli_lanes_verified`]; only PRE-flight faults (unknown id, malformed
//!   manifest row) refuse the whole operation before any store is touched.
//! - **Deleted doc ⇒ explicit retire, never silent orphaning** (arch-R22 item 3). `rules drift`
//!   REPORTS a deleted governed doc (`OrphanReason::DocMissing`) and never auto-drops;
//!   [`select_doc_rules`] turns that report into the retire set (`rules retire --doc <path>`),
//!   and drift then treats the retired rules as the healed state (`skipped_retired`).
//! - **Events.** Each graph-lane state change emits `wicked.estate.rule.retired` through
//!   [`crate::retire_rule`]'s existing seam (AW-22) — one event per store that actually changed,
//!   which is the propagation trail an observer replays. The knowledge rewrite is deliberately
//!   silent: the marker itself is the durable record.
//! - **Cache invalidation is part of the lane.** The estate MCP version-caches `rules.recall`
//!   responses in the store itself (see [`crate::fanout::bump_graph_version`]); every graph-lane
//!   retire bumps the store's graph version so no cached pre-retire response outlives the
//!   withdrawal. A failed bump FAILS the lane — a rule gone from the nodes but alive in a cache
//!   is still recallable.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use wicked_apps_core::{synthetic_symbol, FromNode, GraphRead, POLICY};
use wicked_estate_knowledge::{KClass, KNode, KnowledgeEngine};

use crate::conformance::{recall_rules, retire_rule, RuleQuery, CONFORMANCE_RULE};
use crate::domain::Policy;
use crate::engine::retire_policy;
use crate::fanout::FanoutManifest;
use crate::provenance::parse_provenance_ref;

/// Receipt wire version. Bump on any breaking change to the serialized shape.
pub const RETIRE_RECEIPT_VERSION: &str = "1.0";

/// The knowledge-chunk retirement marker prefix. A chunk whose content starts with this is
/// NON-NORMATIVE: recall still returns it (so a cited answer can explain the withdrawal), but the
/// marker travels with the text, and re-running the kill switch detects it (idempotence).
pub const RETIRED_MARKER_PREFIX: &str = "[RETIRED";

/// The full marker stamped onto a rationale chunk at `now` (unix seconds).
pub fn retired_marker(now: i64) -> String {
    format!(
        "{RETIRED_MARKER_PREFIX} at unix:{now} — non-normative; withdrawn by operator kill-switch]"
    )
}

/// What happened to one lane copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneStatus {
    /// The copy transitioned (or already was) retired/marked in this store.
    Retired,
    /// The copy was already carrying the retirement marker (knowledge lane idempotent re-run).
    AlreadyRetired,
    /// The manifest names this store but the copy is ABSENT there. The end state a kill switch
    /// wants (nothing recallable) holds, but the note flags the manifest/store disagreement.
    Absent,
    /// A daemon-held store this process must never write (single-writer invariant): the receipt
    /// records the operator action instead.
    Pending,
    /// The lane could not be retired or could not be verified — the receipt line names why.
    Failed,
}

/// One lane copy's retirement outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneOutcome {
    /// `"enforcement"` | `"discovery"` | `"knowledge"`.
    pub lane: String,
    /// `"cli"` (written + verified here) or `"crew-api"` (pending operator action).
    pub transport: String,
    /// The store path (cli) or API base URL (crew-api).
    pub target: String,
    pub status: LaneStatus,
    /// True only after the lane re-opened FRESH and the withdrawn state was observed through the
    /// consumer read path. Always false for [`LaneStatus::Pending`] and [`LaneStatus::Failed`].
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One id's retirement across all its manifest lanes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleRetirement {
    pub id: String,
    /// `"conformance_rule"` or `"policy"` — an id present in both manifest maps yields one entry
    /// of each kind.
    pub kind: String,
    pub lanes: Vec<LaneOutcome>,
}

/// The kill switch's receipt — the retirement twin of the fan-out manifest, keyed on the same
/// stable ids. Serialized to JSON by the `wicked-core rules retire` CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetireReceipt {
    pub receipt_version: String,
    /// The manifest the operation was keyed on (path as given to the CLI; empty when driven as a
    /// library with an in-memory manifest).
    pub manifest_path: String,
    /// The ids the operator asked to retire, post-dedup, in request order.
    pub requested: Vec<String>,
    pub generated_at: i64,
    pub retirements: Vec<RuleRetirement>,
    /// True when every cli-written lane verified its withdrawn state on a fresh re-open and no
    /// lane failed. Pending crew-api lanes do NOT clear this — they are counted separately.
    pub all_cli_lanes_verified: bool,
    /// Crew-api lanes awaiting the operator's DELETE (+ `rules/preview` verification).
    pub pending: usize,
}

impl RetireReceipt {
    /// Fully propagated: every lane verified AND nothing awaits an out-of-process operator step.
    pub fn fully_propagated(&self) -> bool {
        self.all_cli_lanes_verified && self.pending == 0
    }
}

/// The rule ids the manifest derives from `doc_path` — the deleted-doc → explicit-retire bridge
/// (arch-R22 item 3). `doc_path` is compared against the PATH component of each rule's recorded
/// source ref (`path[@sha][#anchor]` — the sha/anchor never participate), so the same doc matches
/// across re-ingests at different digests. Returns ids in manifest (BTreeMap) order.
pub fn select_doc_rules(manifest: &FanoutManifest, doc_path: &str) -> Vec<String> {
    manifest
        .rules
        .iter()
        .filter(|(_, entry)| {
            entry
                .source
                .as_deref()
                .is_some_and(|r| parse_provenance_ref(r).path == doc_path)
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// Retire `ids` across every lane their manifest rows name, in one operation. Pre-flight refuses
/// UNKNOWN ids (an id in neither `rules` nor `policies`) before any store is touched; after that,
/// every lane is attempted and its outcome recorded — see the module doctrine.
pub fn retire_from_manifest(
    manifest: &FanoutManifest,
    manifest_path: &str,
    ids: &[String],
    now: i64,
) -> anyhow::Result<RetireReceipt> {
    // Dedup, preserving request order — retiring one id twice in one invocation is one action.
    let mut seen = BTreeSet::new();
    let requested: Vec<String> = ids
        .iter()
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect();
    if requested.is_empty() {
        anyhow::bail!("rules retire: no ids to retire (pass --id and/or --doc)");
    }

    // Pre-flight: every id must be manifest-keyed. An unknown id is a typo or the wrong manifest —
    // refusing the WHOLE op here is what keeps the kill switch one atomic intent, never a partial
    // guess. (Contrast with per-lane faults below, which are collected, not bailed on.)
    let unknown: Vec<&String> = requested
        .iter()
        .filter(|id| !manifest.rules.contains_key(*id) && !manifest.policies.contains_key(*id))
        .collect();
    if !unknown.is_empty() {
        anyhow::bail!(
            "rules retire: {unknown:?} not found in the manifest (neither `rules` nor `policies`) \
             — retirement is manifest-keyed (arch-R22); check the id or pass the manifest that \
             imported it. No store was modified."
        );
    }

    let mut retirements: Vec<RuleRetirement> = Vec::new();
    for id in &requested {
        if let Some(entry) = manifest.rules.get(id) {
            let mut lanes: Vec<LaneOutcome> = Vec::new();
            // Lane 1: enforcement — where the gate hook recalls from.
            match manifest.enforcement.transport.as_str() {
                "cli" => lanes.push(retire_rule_graph_lane(
                    "enforcement",
                    &manifest.enforcement.target,
                    id,
                )),
                "crew-api" => lanes.push(crew_pending_lane(
                    "enforcement",
                    &manifest.enforcement.target,
                    &format!(
                        "daemon-held store: single-writer invariant forbids CLI writes. Retire \
                         over DELETE {}/governance/rules/{id} (audited, retire-not-delete), then \
                         verify via GET {}/governance/rules/preview",
                        manifest.enforcement.target, manifest.enforcement.target
                    ),
                )),
                other => lanes.push(LaneOutcome {
                    lane: "enforcement".to_string(),
                    transport: other.to_string(),
                    target: manifest.enforcement.target.clone(),
                    status: LaneStatus::Failed,
                    verified: false,
                    note: Some(format!(
                        "unknown enforcement transport {other:?} — this binary predates the \
                         manifest that recorded it; upgrade before retiring"
                    )),
                }),
            }
            // Lane 2: discovery — every graph db the manifest ROW names (manifest-keyed, so a
            // workspace-scoped rule retires in every replica).
            for db in &entry.discovery {
                lanes.push(retire_rule_graph_lane("discovery", db, id));
            }
            // Lane 3: knowledge — mark the rationale chunk non-normative.
            for kref in &entry.knowledge {
                lanes.push(retire_knowledge_lane(kref, now));
            }
            retirements.push(RuleRetirement {
                id: id.clone(),
                kind: "conformance_rule".to_string(),
                lanes,
            });
        }
        if manifest.policies.contains_key(id) {
            // Deny-path policies are enforcement-lane machinery: no discovery/knowledge twins.
            let lane = match manifest.enforcement.transport.as_str() {
                "cli" => retire_policy_lane(&manifest.enforcement.target, id),
                "crew-api" => crew_pending_lane(
                    "enforcement",
                    &manifest.enforcement.target,
                    &format!(
                        "daemon-held store: retire over DELETE {}/governance/policies/{id}, then \
                         verify via GET {}/governance/rules/preview",
                        manifest.enforcement.target, manifest.enforcement.target
                    ),
                ),
                other => LaneOutcome {
                    lane: "enforcement".to_string(),
                    transport: other.to_string(),
                    target: manifest.enforcement.target.clone(),
                    status: LaneStatus::Failed,
                    verified: false,
                    note: Some(format!("unknown enforcement transport {other:?}")),
                },
            };
            retirements.push(RuleRetirement {
                id: id.clone(),
                kind: "policy".to_string(),
                lanes: vec![lane],
            });
        }
    }

    let mut pending = 0usize;
    let mut all_cli_lanes_verified = true;
    for r in &retirements {
        for lane in &r.lanes {
            match lane.status {
                LaneStatus::Pending => pending += 1,
                LaneStatus::Failed => all_cli_lanes_verified = false,
                _ if !lane.verified => all_cli_lanes_verified = false,
                _ => {}
            }
        }
    }

    Ok(RetireReceipt {
        receipt_version: RETIRE_RECEIPT_VERSION.to_string(),
        manifest_path: manifest_path.to_string(),
        requested,
        generated_at: now,
        retirements,
        all_cli_lanes_verified,
        pending,
    })
}

/// A crew-api lane: recorded pending, never written from this process.
fn crew_pending_lane(lane: &str, target: &str, note: &str) -> LaneOutcome {
    LaneOutcome {
        lane: lane.to_string(),
        transport: "crew-api".to_string(),
        target: target.to_string(),
        status: LaneStatus::Pending,
        verified: false,
        note: Some(note.to_string()),
    }
}

fn failed_lane(lane: &str, target: &str, note: String) -> LaneOutcome {
    LaneOutcome {
        lane: lane.to_string(),
        transport: "cli".to_string(),
        target: target.to_string(),
        status: LaneStatus::Failed,
        verified: false,
        note: Some(note),
    }
}

/// Retire one conformance-rule copy in one graph store, then RE-OPEN FRESH (read-only — the exact
/// open the gate hook performs) and verify the withdrawn state through the consumer read path:
/// `recall_rules` must not serve the id, and (retire-not-delete) a retired node must still
/// resolve, carrying `retired: true`.
fn retire_rule_graph_lane(lane: &str, db: &str, id: &str) -> LaneOutcome {
    let found = {
        let mut store = match wicked_apps_core::open_store(Some(db)) {
            Ok(s) => s,
            Err(e) => return failed_lane(lane, db, format!("open {db:?}: {e}")),
        };
        let found = match retire_rule(&mut store, id) {
            Ok(found) => found,
            Err(e) => return failed_lane(lane, db, format!("retire {id} in {db:?}: {e}")),
        };
        // Invalidate the estate MCP's versioned response cache (see `fanout::bump_graph_version`):
        // without the bump, a worker's `rules.recall` could keep serving a PRE-retire cached
        // response — the exact "still recallable somewhere" state the kill switch exists to end.
        // Fail-loud: an unbumped cache is a failed lane, not a footnote.
        if found {
            if let Err(e) = crate::fanout::bump_graph_version(&mut store, db) {
                return failed_lane(lane, db, e.to_string());
            }
        }
        found
        // write handle dropped before the verification re-open
    };

    let store = match wicked_apps_core::open_store_ro(Some(db)) {
        Ok(s) => s,
        Err(e) => return failed_lane(lane, db, format!("verify re-open {db:?}: {e}")),
    };
    let recalled = match recall_rules(&store, &RuleQuery::default()) {
        Ok(rs) => rs,
        Err(e) => return failed_lane(lane, db, format!("verify recall on {db:?}: {e}")),
    };
    if recalled.iter().any(|r| r.id == id) {
        return failed_lane(
            lane,
            db,
            format!("{id} is STILL served by recall_rules on {db:?} after retirement"),
        );
    }
    if found {
        // Retire-not-delete: the node must survive, marked retired, so past claims citing the id
        // stay explicable.
        match store.get_node(&synthetic_symbol(CONFORMANCE_RULE, id)) {
            Ok(Some(node)) => match crate::conformance::ConformanceRule::from_node(&node) {
                Ok(rule) if rule.retired => {}
                Ok(_) => {
                    return failed_lane(
                        lane,
                        db,
                        format!("{id} read back ACTIVE from {db:?} after retirement"),
                    )
                }
                Err(e) => return failed_lane(lane, db, format!("verify parse {id}: {e}")),
            },
            Ok(None) => {
                return failed_lane(
                    lane,
                    db,
                    format!(
                        "{id} node VANISHED from {db:?} — retire-not-delete violated; past \
                         decisions citing it are no longer explicable"
                    ),
                )
            }
            Err(e) => return failed_lane(lane, db, format!("verify get_node {id}: {e}")),
        }
    }
    LaneOutcome {
        lane: lane.to_string(),
        transport: "cli".to_string(),
        target: db.to_string(),
        status: if found {
            LaneStatus::Retired
        } else {
            LaneStatus::Absent
        },
        verified: true,
        note: (!found).then(|| {
            format!(
                "manifest names this store but {id} is absent there — nothing recallable (the \
                 kill-switch end state holds), but the manifest/store disagreement is worth an \
                 audit: the live copy may sit in a store this manifest does not know"
            )
        }),
    }
}

/// Retire one deny-path policy in the enforcement store, verify on a fresh read-only open:
/// the node must survive, marked retired (SELECT skips retired policies — `engine::select_any`).
fn retire_policy_lane(db: &str, id: &str) -> LaneOutcome {
    let lane = "enforcement";
    let found = {
        let mut store = match wicked_apps_core::open_store(Some(db)) {
            Ok(s) => s,
            Err(e) => return failed_lane(lane, db, format!("open {db:?}: {e}")),
        };
        let found = match retire_policy(&mut store, id) {
            Ok(found) => found,
            Err(e) => return failed_lane(lane, db, format!("retire policy {id} in {db:?}: {e}")),
        };
        // Same cache-invalidation contract as the rule lane (see `fanout::bump_graph_version`).
        if found {
            if let Err(e) = crate::fanout::bump_graph_version(&mut store, db) {
                return failed_lane(lane, db, e.to_string());
            }
        }
        found
    };

    let store = match wicked_apps_core::open_store_ro(Some(db)) {
        Ok(s) => s,
        Err(e) => return failed_lane(lane, db, format!("verify re-open {db:?}: {e}")),
    };
    if found {
        match store.get_node(&synthetic_symbol(POLICY, id)) {
            Ok(Some(node)) => match Policy::from_node(&node) {
                Ok(p) if p.retired => {}
                Ok(_) => {
                    return failed_lane(
                        lane,
                        db,
                        format!("policy {id} read back ACTIVE from {db:?} after retirement"),
                    )
                }
                Err(e) => return failed_lane(lane, db, format!("verify parse policy {id}: {e}")),
            },
            Ok(None) => {
                return failed_lane(
                    lane,
                    db,
                    format!("policy {id} node VANISHED from {db:?} — retire-not-delete violated"),
                )
            }
            Err(e) => return failed_lane(lane, db, format!("verify get_node policy {id}: {e}")),
        }
    }
    LaneOutcome {
        lane: lane.to_string(),
        transport: "cli".to_string(),
        target: db.to_string(),
        status: if found {
            LaneStatus::Retired
        } else {
            LaneStatus::Absent
        },
        verified: true,
        note: (!found).then(|| {
            format!(
                "manifest names this store but policy {id} is absent there — nothing selectable, \
                 but the manifest/store disagreement is worth an audit"
            )
        }),
    }
}

/// Mark one rationale chunk non-normative: prefix its content with the [RETIRED …] marker,
/// preserving the original text (and `created_at`, so a withdrawn chunk gains no recency boost).
/// The manifest records the chunk as `<db>#kchunk:rule-rationale/<ID>`.
fn retire_knowledge_lane(kref: &str, now: i64) -> LaneOutcome {
    let lane = "knowledge";
    let Some((db, chunk_id)) = kref.split_once("#kchunk:") else {
        return failed_lane(
            lane,
            kref,
            format!(
                "malformed knowledge ref {kref:?} — expected `<db>#kchunk:<chunk-id>` (the shape \
                 `fanout` writes); refusing to guess a store path"
            ),
        );
    };
    // The chunk's stable symbol: same construction the fan-out's write used (KNode::symbol).
    let probe = KNode {
        id: chunk_id.to_string(),
        class: KClass::Chunk,
        content: String::new(),
        scope: String::new(),
        source: String::new(),
        created_at: 0,
    };
    let symbol = probe.symbol();

    let already = {
        let mut engine = match KnowledgeEngine::open(db) {
            Ok(e) => e,
            Err(e) => return failed_lane(lane, db, format!("open knowledge {db:?}: {e}")),
        };
        let node = match engine.node(&symbol) {
            Ok(n) => n,
            Err(e) => return failed_lane(lane, db, format!("read chunk {chunk_id}: {e}")),
        };
        let Some(node) = node else {
            return LaneOutcome {
                lane: lane.to_string(),
                transport: "cli".to_string(),
                target: db.to_string(),
                status: LaneStatus::Absent,
                verified: true,
                note: Some(format!(
                    "manifest names this store but chunk {chunk_id} is absent there — no \
                     rationale is served (the kill-switch end state holds), but the \
                     manifest/store disagreement is worth an audit"
                )),
            };
        };
        let Some(chunk) = KNode::from_node(&node) else {
            return failed_lane(
                lane,
                db,
                format!("node at {chunk_id} is not a knowledge chunk — refusing to overwrite"),
            );
        };
        if chunk.content.starts_with(RETIRED_MARKER_PREFIX) {
            true
        } else {
            let marked = KNode {
                content: format!("{} {}", retired_marker(now), chunk.content),
                ..chunk
            };
            if let Err(e) = engine.write(&marked) {
                return failed_lane(lane, db, format!("mark chunk {chunk_id} retired: {e}"));
            }
            false
        }
        // engine handle dropped before the verification re-open
    };

    // Verify on a FRESH open: the durable chunk must carry the marker.
    let engine = match KnowledgeEngine::open(db) {
        Ok(e) => e,
        Err(e) => return failed_lane(lane, db, format!("verify re-open knowledge {db:?}: {e}")),
    };
    match engine.node(&symbol) {
        Ok(Some(node)) => match KNode::from_node(&node) {
            Some(chunk) if chunk.content.starts_with(RETIRED_MARKER_PREFIX) => {}
            Some(_) => {
                return failed_lane(
                    lane,
                    db,
                    format!("chunk {chunk_id} read back WITHOUT the retirement marker"),
                )
            }
            None => return failed_lane(lane, db, format!("verify parse chunk {chunk_id} failed")),
        },
        Ok(None) => {
            return failed_lane(
                lane,
                db,
                format!("chunk {chunk_id} VANISHED during marking — retire-not-delete violated"),
            )
        }
        Err(e) => return failed_lane(lane, db, format!("verify read chunk {chunk_id}: {e}")),
    }

    LaneOutcome {
        lane: lane.to_string(),
        transport: "cli".to_string(),
        target: db.to_string(),
        status: if already {
            LaneStatus::AlreadyRetired
        } else {
            LaneStatus::Retired
        },
        verified: true,
        note: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use crate::fanout::{fanout, load_ruleset, EnforcementTarget, FanoutScope, FanoutTargets};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wg-retire-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Two markdown conformance rules from one doc + one JSON rule from another source + one deny
    /// policy — enough to prove per-id targeting, doc-derived selection, and the policy path.
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
             - `POL-100` (error): Event types are 4-segment wicked.<domain>.<noun>.<verb>.\n\
             - `PAT-101` (warn): Three-segment event names are never constructed.\n",
        )
        .unwrap();
        ruleset
    }

    fn fan_out(base: &Path) -> (crate::FanoutManifest, String, String, String) {
        let ruleset = write_ruleset(base);
        let enforcement = base.join("gov.db").to_string_lossy().into_owned();
        let discovery = base.join("repo-graph.db").to_string_lossy().into_owned();
        let knowledge = base.join("knowledge.db").to_string_lossy().into_owned();
        let targets = FanoutTargets {
            scope: FanoutScope::Repo,
            enforcement: EnforcementTarget::Cli {
                db: enforcement.clone(),
            },
            discovery_dbs: vec![discovery.clone()],
            knowledge_dbs: vec![knowledge.clone()],
            knowledge_scope: "wiki:governance".to_string(),
        };
        let load = load_ruleset(&ruleset).unwrap();
        let manifest = fanout(&load, &targets, ruleset.to_str().unwrap(), 1_750_000_000).unwrap();
        (manifest, enforcement, discovery, knowledge)
    }

    fn recall_ids(db: &str) -> Vec<String> {
        let store = wicked_apps_core::open_store_ro(Some(db)).unwrap();
        recall_rules(&store, &RuleQuery::default())
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect()
    }

    #[test]
    fn retire_by_id_propagates_all_lanes_in_one_op_and_recall_stops_serving_it() {
        crate::events::hermetic_test_spool();
        let base = scratch("propagate");
        let (manifest, enforcement, discovery, knowledge) = fan_out(&base);
        assert!(
            recall_ids(&enforcement).contains(&"POL-100".to_string()),
            "precondition: the bad rule is recalled for enforcement"
        );

        let receipt = retire_from_manifest(
            &manifest,
            "manifest.json",
            &["POL-100".to_string()],
            1_750_000_100,
        )
        .unwrap();

        assert!(receipt.fully_propagated(), "receipt: {receipt:?}");
        assert_eq!(receipt.pending, 0);
        assert_eq!(receipt.retirements.len(), 1);
        let lanes = &receipt.retirements[0].lanes;
        assert_eq!(
            lanes.len(),
            3,
            "enforcement + one discovery + one knowledge: {lanes:?}"
        );
        assert!(lanes
            .iter()
            .all(|l| l.status == LaneStatus::Retired && l.verified));

        // The next recall no longer serves it — in EVERY graph lane; the sibling rules survive.
        for db in [&enforcement, &discovery] {
            let ids = recall_ids(db);
            assert!(!ids.contains(&"POL-100".to_string()), "{db}: {ids:?}");
            assert!(ids.contains(&"PAT-101".to_string()), "{db}: {ids:?}");
            assert!(ids.contains(&"PAT-001".to_string()), "{db}: {ids:?}");
        }

        // Retire-not-delete: the node survives, marked retired, in both graph lanes.
        for db in [&enforcement, &discovery] {
            let store = wicked_apps_core::open_store_ro(Some(db)).unwrap();
            let node = store
                .get_node(&synthetic_symbol(CONFORMANCE_RULE, "POL-100"))
                .unwrap()
                .expect("node survives retirement");
            assert!(
                crate::conformance::ConformanceRule::from_node(&node)
                    .unwrap()
                    .retired
            );
        }

        // Knowledge twin: marker prefixed, original rationale (and the id) preserved.
        let probe = KNode {
            id: crate::fanout::rationale_chunk_id("POL-100"),
            class: KClass::Chunk,
            content: String::new(),
            scope: String::new(),
            source: String::new(),
            created_at: 0,
        };
        let engine = KnowledgeEngine::open(&knowledge).unwrap();
        let chunk = KNode::from_node(&engine.node(&probe.symbol()).unwrap().unwrap()).unwrap();
        assert!(
            chunk.content.starts_with(RETIRED_MARKER_PREFIX),
            "{}",
            chunk.content
        );
        assert!(
            chunk.content.contains("POL-100"),
            "the enforceable twin's id survives the marking: {}",
            chunk.content
        );

        // Idempotent re-run: same end state, still verified, knowledge reports AlreadyRetired.
        let second = retire_from_manifest(
            &manifest,
            "manifest.json",
            &["POL-100".to_string()],
            1_750_000_200,
        )
        .unwrap();
        assert!(second.fully_propagated(), "{second:?}");
        let kn = second.retirements[0]
            .lanes
            .iter()
            .find(|l| l.lane == "knowledge")
            .unwrap();
        assert_eq!(kn.status, LaneStatus::AlreadyRetired);
        // No double marker.
        let engine = KnowledgeEngine::open(&knowledge).unwrap();
        let chunk = KNode::from_node(&engine.node(&probe.symbol()).unwrap().unwrap()).unwrap();
        assert_eq!(
            chunk.content.matches(RETIRED_MARKER_PREFIX).count(),
            1,
            "{}",
            chunk.content
        );
    }

    #[test]
    fn retire_a_deny_policy_via_the_same_manifest_keyed_op() {
        crate::events::hermetic_test_spool();
        let base = scratch("policy");
        let (manifest, enforcement, _, _) = fan_out(&base);

        let receipt = retire_from_manifest(
            &manifest,
            "manifest.json",
            &["pol-deny-secretleak".to_string()],
            1_750_000_100,
        )
        .unwrap();
        assert!(receipt.fully_propagated(), "{receipt:?}");
        assert_eq!(receipt.retirements.len(), 1);
        assert_eq!(receipt.retirements[0].kind, "policy");
        assert_eq!(
            receipt.retirements[0].lanes.len(),
            1,
            "a deny policy has no discovery/knowledge twin"
        );

        let store = wicked_apps_core::open_store_ro(Some(&enforcement)).unwrap();
        let node = store
            .get_node(&synthetic_symbol(POLICY, "pol-deny-secretleak"))
            .unwrap()
            .expect("retire-not-delete");
        assert!(Policy::from_node(&node).unwrap().retired);
    }

    #[test]
    fn an_unknown_id_refuses_the_whole_op_before_any_store_write() {
        crate::events::hermetic_test_spool();
        let base = scratch("unknown");
        let (manifest, enforcement, _, _) = fan_out(&base);

        let err = retire_from_manifest(
            &manifest,
            "manifest.json",
            &["POL-100".to_string(), "PAT-999".to_string()],
            1_750_000_100,
        )
        .expect_err("an id the manifest never imported must refuse the whole op");
        assert!(err.to_string().contains("PAT-999"), "{err}");

        // The KNOWN id was NOT retired — the op is atomic in intent.
        assert!(
            recall_ids(&enforcement).contains(&"POL-100".to_string()),
            "no store may change when pre-flight refuses"
        );
    }

    #[test]
    fn a_crew_api_enforcement_lane_is_recorded_pending_never_written() {
        crate::events::hermetic_test_spool();
        let base = scratch("crewapi");
        // Fan out cli-everything first, then rewrite the manifest's enforcement lane to crew-api —
        // the shape a daemon-held import records (fanout writes transport: crew-api, verified: false).
        let (mut manifest, _, discovery, _) = fan_out(&base);
        manifest.enforcement = crate::fanout::EnforcementLane {
            transport: "crew-api".to_string(),
            target: "http://127.0.0.1:7901/api/v1".to_string(),
            verified: false,
            note: None,
        };

        let receipt = retire_from_manifest(
            &manifest,
            "manifest.json",
            &["POL-100".to_string()],
            1_750_000_100,
        )
        .unwrap();
        assert_eq!(receipt.pending, 1, "{receipt:?}");
        assert!(
            receipt.all_cli_lanes_verified,
            "cli lanes verified independently of the pending daemon lane: {receipt:?}"
        );
        assert!(!receipt.fully_propagated());

        let enforcement_lane = receipt.retirements[0]
            .lanes
            .iter()
            .find(|l| l.lane == "enforcement")
            .unwrap();
        assert_eq!(enforcement_lane.status, LaneStatus::Pending);
        let note = enforcement_lane.note.as_deref().unwrap();
        assert!(
            note.contains("DELETE") && note.contains("/governance/rules/POL-100"),
            "the receipt must carry the exact operator action: {note}"
        );
        assert!(
            note.contains("rules/preview"),
            "and the verification step: {note}"
        );

        // The cli discovery lane DID retire.
        assert!(!recall_ids(&discovery).contains(&"POL-100".to_string()));
    }

    #[test]
    fn select_doc_rules_bridges_a_deleted_doc_to_its_retire_set() {
        crate::events::hermetic_test_spool();
        let base = scratch("docselect");
        let (manifest, enforcement, _, _) = fan_out(&base);

        // The markdown doc minted POL-100 + PAT-101; the JSON-lane PAT-001 must NOT be selected.
        let ids = select_doc_rules(&manifest, "event-grammar.md");
        assert_eq!(ids, vec!["PAT-101".to_string(), "POL-100".to_string()]);
        assert!(select_doc_rules(&manifest, "no-such-doc.md").is_empty());

        // Deleted doc → explicit retire: drift REPORTS the orphans, retire CLEARS them.
        let ruleset = base.join("ruleset");
        std::fs::remove_file(ruleset.join("event-grammar.md")).unwrap();
        {
            let store = wicked_apps_core::open_store_ro(Some(&enforcement)).unwrap();
            let report = crate::drift(&store, Some(&ruleset), 25).unwrap();
            let orphaned: Vec<&str> = report.orphaned.iter().map(|o| o.rule_id.as_str()).collect();
            assert!(
                orphaned.contains(&"POL-100") && orphaned.contains(&"PAT-101"),
                "drift must report the deleted doc's rules as orphaned: {orphaned:?}"
            );
        }
        let receipt =
            retire_from_manifest(&manifest, "manifest.json", &ids, 1_750_000_100).unwrap();
        assert!(receipt.fully_propagated(), "{receipt:?}");
        {
            let store = wicked_apps_core::open_store_ro(Some(&enforcement)).unwrap();
            let report = crate::drift(&store, Some(&ruleset), 25).unwrap();
            assert!(
                report.orphaned.is_empty(),
                "retirement IS the healed state — drift must stop reporting the orphans: {:?}",
                report.orphaned
            );
        }
    }

    #[test]
    fn a_missing_lane_copy_reports_absent_with_an_audit_note_not_a_failure() {
        crate::events::hermetic_test_spool();
        let base = scratch("absent");
        let (mut manifest, _, _, _) = fan_out(&base);

        // A manifest row pointing at a store the rule never landed in (fresh empty db).
        let ghost = base.join("ghost.db").to_string_lossy().into_owned();
        {
            let _ = wicked_apps_core::open_store(Some(&ghost)).unwrap(); // create empty
        }
        let entry = manifest.rules.get_mut("POL-100").unwrap();
        entry.discovery = vec![ghost.clone()];
        entry.knowledge = vec![format!("{ghost}#kchunk:rule-rationale/POL-100")];

        let receipt = retire_from_manifest(
            &manifest,
            "manifest.json",
            &["POL-100".to_string()],
            1_750_000_100,
        )
        .unwrap();
        assert!(receipt.all_cli_lanes_verified, "{receipt:?}");
        let discovery_lane = receipt.retirements[0]
            .lanes
            .iter()
            .find(|l| l.lane == "discovery")
            .unwrap();
        assert_eq!(discovery_lane.status, LaneStatus::Absent);
        assert!(discovery_lane.verified, "recall-gone still verified");
        assert!(discovery_lane.note.as_deref().unwrap().contains("audit"));
        let knowledge_lane = receipt.retirements[0]
            .lanes
            .iter()
            .find(|l| l.lane == "knowledge")
            .unwrap();
        assert_eq!(knowledge_lane.status, LaneStatus::Absent);
    }

    /// The estate MCP version-caches `rules.recall` responses in the store's own `cache` table
    /// (keyed on the `graph_version` meta row). A retire that leaves that cache serving the
    /// pre-retire response has not actually withdrawn the rule from the worker's read path —
    /// the drill caught exactly this. The retire lane must bump the version so every response
    /// cached before the kill switch goes stale.
    #[test]
    fn retire_invalidates_the_stores_versioned_response_cache() {
        crate::events::hermetic_test_spool();
        let base = scratch("cachebump");
        let (manifest, enforcement, discovery, _) = fan_out(&base);

        // A consumer (the estate MCP) caches a rules.recall response at the CURRENT version.
        for db in [&enforcement, &discovery] {
            let mut store = wicked_apps_core::open_store(Some(db)).unwrap();
            store
                .cache_put("rules.recall/{}", "pre-retire cached response")
                .unwrap();
        }

        let receipt = retire_from_manifest(
            &manifest,
            "manifest.json",
            &["POL-100".to_string()],
            1_750_000_100,
        )
        .unwrap();
        assert!(receipt.fully_propagated(), "{receipt:?}");

        // The cached pre-retire response must now be stale in BOTH graph lanes.
        for db in [&enforcement, &discovery] {
            let store = wicked_apps_core::open_store(Some(db)).unwrap();
            assert_eq!(
                store.cache_get("rules.recall/{}").unwrap(),
                None,
                "{db}: a version-cached pre-retire rules.recall response must not survive the \
                 kill switch"
            );
        }
    }

    #[test]
    fn receipt_serializes_stably_and_round_trips() {
        let receipt = RetireReceipt {
            receipt_version: RETIRE_RECEIPT_VERSION.to_string(),
            manifest_path: "manifest.json".to_string(),
            requested: vec!["PAT-001".to_string()],
            generated_at: 1,
            retirements: vec![RuleRetirement {
                id: "PAT-001".to_string(),
                kind: "conformance_rule".to_string(),
                lanes: vec![LaneOutcome {
                    lane: "enforcement".to_string(),
                    transport: "cli".to_string(),
                    target: "gov.db".to_string(),
                    status: LaneStatus::Retired,
                    verified: true,
                    note: None,
                }],
            }],
            all_cli_lanes_verified: true,
            pending: 0,
        };
        let json = serde_json::to_value(&receipt).unwrap();
        assert_eq!(json["receipt_version"], "1.0");
        assert_eq!(json["retirements"][0]["lanes"][0]["status"], "retired");
        let back: RetireReceipt = serde_json::from_value(json).unwrap();
        assert_eq!(back, receipt);
    }

    #[test]
    fn retirement_marker_text_is_stable_shaped() {
        let m = retired_marker(1_750_000_000);
        assert!(m.starts_with(RETIRED_MARKER_PREFIX));
        assert!(m.contains("non-normative"));
        assert!(m.contains("kill-switch"));
    }
}
