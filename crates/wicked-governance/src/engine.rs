//! The governance evaluation loop on the SHARED estate store: register → SELECT → DECIDE → conform.
//!
//! Ported from the Node prototype `lib/{select,decide,store,evidence-port}.mjs` (ARCHITECTURE §2),
//! re-grounded on `wicked-apps-core` + the estate graph:
//!
//! - **register_policy** — since the STEERING merge, a thin shim: writes the policy's
//!   effect-bearing steering-rule twin (the enforcement row SELECT reads) plus the legacy policy
//!   [`Node`] as the audit row, in one batch.
//! - **SELECT** — the index-only fast lane: load the UNIFIED effect-bearing steering rules (plus
//!   un-migrated legacy `Other(POLICY)` rows) and keep those whose `applies_to` includes the
//!   phase and whose `excludes` does not. No model, bounded, deterministic order.
//! - **DECIDE** — the deterministic engine (NO model): a triggered [`Effect::Deny`] ⇒
//!   [`Decision::Deny`] (deny DOMINATES); triggered [`Effect::AllowWithConditions`] ⇒ collect
//!   obligations ⇒ [`Decision::AllowWithConditions`]; else [`Decision::Allow`].
//! - **conform** — upsert the claim [`Node`] + a policy→claim [`EdgeKind::Governs`] edge, then a
//!   COARSE fire-and-forget `wicked.crew.governance.conformance_recorded` event (counts/ids only).
//!
//! Divergence from the prototype (faithful, documented): `evaluated_at` is Unix-seconds (`i64`, the
//! `wicked_apps_core::ConformanceClaim` field type) rather than the prototype's ISO-8601 string; the claim
//! id is a sha256 of `(scope, phase, decision, evaluated_context_ref, evaluator_identity)` — the
//! same re-derivable recipe as decide.mjs, but full 64-hex (no slice) for collision headroom, and
//! extended with `evaluator_identity` so different evaluators on the same context produce different
//! claim_ids (evaluator≠creator pattern).

use sha2::{Digest, Sha256};
use wicked_apps_core::{
    emit::{emit_event, EmitEvent},
    synthetic_symbol, ConformanceClaim, Decision, Edge, EdgeKind, FromNode, GraphRead, GraphStore,
    Language, Location, Node, NodeKind, ResolutionTier, Span, SymbolId, ToNode, CONFORMANCE_CLAIM,
    POLICY, SYMBOL_SCHEME,
};
// `SymbolQuery` is not re-exported by wicked-apps-core; pull it straight from estate-core (this crate
// already depends on wicked-estate-core). wicked-apps-core owns the domain seam, not every query type.
use wicked_estate_core::SymbolQuery;

use crate::domain::{Effect, Policy, Trigger};

/// Stable evaluator identity stamped on every claim (matches the prototype `EVALUATOR_IDENTITY`).
pub const EVALUATOR_IDENTITY: &str = "wicked-governance@0.1.0";

/// The coarse bus event emitted by [`conform`]. The build brief specifies this exact literal
/// (`wicked.crew.governance.conformance_recorded`). NOTE: it is NOT the wicked-apps-core catalog constant
/// `EV_CONFORMANCE_RECORDED` (= `"wicked.crew.conformance.recorded"`); the brief's literal wins here and
/// is grammar-valid per `wicked_apps_core::validate_event_type`. Documented divergence (see crate notes).
pub const EV_CONFORMANCE_RECORDED_LITERAL: &str = "wicked.crew.governance.conformance_recorded";

/// The resolver-id recorded on the policy→claim governance edge (estate requires `resolved_by`).
const GOVERNANCE_RESOLVED_BY: &str = "wicked-governance";

// ─────────────────────────────────────────────────────────────────────────────
// register_policy
// ─────────────────────────────────────────────────────────────────────────────

/// Register a policy — since the STEERING unification, a THIN SHIM over the unified steering-rule
/// model: the enforcement row is a steering [`crate::ConformanceRule`] carrying the policy's
/// `effect` (written at `conformance_rule/<id>`, what SELECT/DECIDE read), and the legacy
/// `Other(POLICY)` node is dual-written beside it as the audit row (`policy → claim` Governs
/// edges hang off it; retired-not-deleted invariant). Idempotent on the stable id. Emits nothing
/// (policy lifecycle events stay the governed operator path's concern).
///
/// Fail-closed at the write boundary all persist paths route through: never register a policy
/// that would enforce nothing (empty applies_to — `policy.validate()`), and never let a policy id
/// silently swallow an existing RECALL-ONLY steering rule at the same unified address.
pub fn register_policy(store: &mut dyn GraphStore, policy: &Policy) -> anyhow::Result<()> {
    policy.validate()?;
    let steering = crate::steering::steering_rule_from_policy(policy);
    steering.validate()?;
    // Unified-id collision guard: a recall-only rule (a wiki rule) already at this id is a
    // DIFFERENT rule — overwriting it would silently delete doctrine. An effect-bearing row at
    // the id is this policy's own steering twin (the idempotent re-register), which upsert may
    // replace.
    let rule_symbol = synthetic_symbol(crate::CONFORMANCE_RULE, &policy.id);
    if let Some(existing) = store.get_node(&rule_symbol)? {
        if existing.kind == NodeKind::Rule {
            let prior = crate::ConformanceRule::from_node(&existing)?;
            if prior.effect.is_none() {
                anyhow::bail!(
                    "policy id {:?} collides with an existing recall-only steering rule at \
                     {rule_symbol} — refusing to overwrite doctrine (pick a different policy id \
                     or retire the rule first)",
                    policy.id
                );
            }
        }
    }
    store.begin_batch()?;
    store.upsert_nodes(&[policy.to_node(), steering.to_node()])?;
    store.commit_batch()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// retire_policy
// ─────────────────────────────────────────────────────────────────────────────

/// Withdraw `id` from enforcement. Returns `false` if no such policy exists.
///
/// Retire, not delete. A governance system whose rules cannot be withdrawn has no way to correct a
/// mistake — a policy authored with a too-broad trigger denies every matching unit forever
/// (FINDING-038, where three test policies could not be removed and kept failing live runs). But
/// hard-deleting the node would break the other half of the contract: past decisions record the
/// policy ids that produced them, and a decision citing an id that no longer resolves cannot be
/// explained afterwards. So the node stays and stops being selected.
pub fn retire_policy(store: &mut dyn GraphStore, id: &str) -> anyhow::Result<bool> {
    // ── the legacy audit row ──
    // Addressed directly by its synthetic symbol. The earlier form loaded and JSON-parsed EVERY
    // policy node to find the one already named by `symbol`, so retiring one rule cost the whole
    // policy set (review on #149).
    let symbol = synthetic_symbol(POLICY, id);
    let mut found_legacy = false;
    if let Some(node) = store.get_node(&symbol)? {
        // The symbol namespaces on POLICY, so a kind mismatch means the graph is inconsistent
        // rather than that the caller asked for something absent. Kept as a guard because the scan
        // it replaces filtered on kind, and "not a policy" must stay `false`, not a parse error.
        if node.kind == NodeKind::Other(POLICY.to_string()) {
            let mut policy = Policy::from_node(&node)?;
            // The write below goes through `policy.to_node()`, which recomputes the symbol from
            // `policy.id`. If the stored metadata disagrees with the symbol it is filed under, that
            // write lands somewhere else entirely: the policy the operator asked to retire stays
            // live, an unrelated one is marked retired, and this still returns `true`. An error,
            // not `Ok(false)` — `false` means "no such policy", and saying that about a policy
            // sitting right there in the graph is the least diagnosable outcome available (#149).
            if policy.id != id {
                anyhow::bail!(
                    "policy graph is inconsistent: node at {symbol} carries id {:?}, not {id:?}",
                    policy.id
                );
            }
            found_legacy = true;
            if !policy.retired {
                policy.retired = true;
                store.begin_batch()?;
                store.upsert_nodes(std::slice::from_ref(&policy.to_node()))?;
                store.commit_batch()?;
            }
            // Already withdrawn falls through: retiring twice should be safe for a retrying
            // caller, and the end state the caller asked for already holds.
        }
    }

    // ── the unified steering twin (STEERING merge: SELECT reads this row, so retirement must
    //    land here or the policy keeps deciding gates) ──
    let rule_symbol = synthetic_symbol(crate::CONFORMANCE_RULE, id);
    let mut found_unified = false;
    if let Some(node) = store.get_node(&rule_symbol)? {
        if node.kind == NodeKind::Rule {
            let mut rule = crate::ConformanceRule::from_node(&node)?;
            if rule.id != id {
                anyhow::bail!(
                    "steering graph is inconsistent: node at {rule_symbol} carries id {:?}, \
                     not {id:?}",
                    rule.id
                );
            }
            // Only an effect-bearing row IS this policy's twin — a recall-only rule sharing the
            // id namespace is doctrine `retire_rule` owns, and retiring it here would let a
            // "no such policy" call silence a wiki rule.
            if rule.effect.is_some() {
                found_unified = true;
                if !rule.retired {
                    rule.retired = true;
                    store.begin_batch()?;
                    store.upsert_nodes(std::slice::from_ref(&rule.to_node()))?;
                    store.commit_batch()?;
                }
            }
        }
    }

    Ok(found_legacy || found_unified)
}

// ─────────────────────────────────────────────────────────────────────────────
// SELECT — index-only fast lane
// ─────────────────────────────────────────────────────────────────────────────

/// Select candidate policies for `phase`. Equivalent to [`select_any`] with a single token.
pub fn select(
    store: &dyn GraphRead,
    scope: &str,
    phase: &str,
    context: &serde_json::Value,
) -> anyhow::Result<Vec<Policy>> {
    select_any(store, scope, &[phase], context)
}

/// Select candidate policies matching ANY of `phases` — since the STEERING unification, from the
/// UNIFIED steering-rule store: every effect-bearing `NodeKind::Rule` steering rule whose
/// `applies_to` includes at least one token AND whose `excludes` (the exclusion twin) claims none,
/// projected through [`crate::steering::policy_view`]; legacy `Other(POLICY)` rows without a
/// unified twin are unioned in at read time so an un-migrated store never fails open. Bounded,
/// deterministic (returned in id order). `scope` and `context` are accepted for parity with the
/// prototype/hot-path signature; the fast lane keys on the phase tokens only (overlay/memory
/// enrichment is the deferred slow lane, ARCHITECTURE §2.1).
///
/// Callers pass BOTH the synthetic execution phase (`unit-<ord>`, [`crate`]-external
/// `scope::unit_phase`) and the workflow PHASE ID the operator actually sees in the API
/// (`<session>:<phase_id>` → `phase_id`). Matching only the former made every policy authored
/// against a real phase name register successfully and then never fire — a silent fail-open on the
/// primary safety control. Both tokens are accepted so the obvious authoring choice works.
pub fn select_any(
    store: &dyn GraphRead,
    _scope: &str,
    phases: &[&str],
    _context: &serde_json::Value,
) -> anyhow::Result<Vec<Policy>> {
    let applies = |applies_to: &[String]| {
        applies_to
            .iter()
            .any(|p| phases.iter().any(|phase| p == phase))
    };

    // ── the UNIFIED store (STEERING merge): effect-bearing steering rules ARE the policies ──
    // Index-only: restrict to the native Rule kind — the same cheap deterministic lane as recall
    // (no FTS, no traversal). Recall-only rules (no effect) never decide; foreign Rule nodes
    // (another producer's) are skipped by the synthetic-symbol round-trip, exactly as in recall.
    let rule_query = SymbolQuery {
        kinds: vec![NodeKind::Rule],
        ..Default::default()
    };
    let mut selected: Vec<Policy> = Vec::new();
    let mut unified_ids: std::collections::BTreeSet<String> = Default::default();
    for node in store.find_symbols(&rule_query)? {
        if node.symbol != synthetic_symbol(crate::CONFORMANCE_RULE, &node.name) {
            continue;
        }
        let rule = crate::ConformanceRule::from_node(&node)?;
        let Some(policy) = crate::steering::policy_view(&rule) else {
            continue; // recall-only rule — not decide-lane
        };
        // The unified row CLAIMS the id even when retired or phase-mismatched: its legacy twin
        // (dual-written by `register_policy`, or the pre-migration original) must never be
        // selected as a second copy or resurrect a retired rule.
        unified_ids.insert(rule.id.clone());
        // Retired rules are withdrawn from enforcement. Filtering HERE rather than in `decide`
        // is deliberate: SELECT is the single funnel every enforcement path goes through, so one
        // check covers the gate, the coverage report, and any future caller. A filter in `decide`
        // would leave a retired policy still counted as "applies to this phase" everywhere else.
        if policy.retired || !applies(&policy.applies_to) {
            continue;
        }
        // The `excludes` twin (STEERING): a phase token listed there withdraws the rule from the
        // gate even though `applies_to` includes it — exclusion DOMINATES inclusion, the
        // fail-closed direction (an exclusion can only narrow enforcement scope, never widen it).
        if rule
            .excludes
            .iter()
            .any(|x| phases.iter().any(|phase| x == phase))
        {
            continue;
        }
        selected.push(policy);
    }

    // ── legacy fallback: `Other(POLICY)` rows with no unified twin (an un-migrated store) ──
    // The read-time shim that keeps enforcement alive until `migrate_policies_to_steering` runs:
    // dropping these rows on a store the migration has not touched would fail the primary safety
    // control OPEN. Rows whose id the unified pass claimed are skipped (the unified row wins).
    for policy in crate::steering::legacy_policies(store)? {
        if unified_ids.contains(&policy.id) {
            continue;
        }
        if policy.retired || !applies(&policy.applies_to) {
            continue;
        }
        selected.push(policy);
    }

    // Deterministic order by id (decide() re-orders by precedence; SELECT just stays stable).
    selected.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(selected)
}

// ─────────────────────────────────────────────────────────────────────────────
// DECIDE — the deterministic engine (NO model)
// ─────────────────────────────────────────────────────────────────────────────

/// Canonical JSON of the context, used for trigger matching AND the context fingerprint.
fn canonical_context(context: &serde_json::Value) -> String {
    serde_json::to_string(context).unwrap_or_else(|_| "null".to_string())
}

/// sha256 of the canonical context JSON, rendered `sha256:<hex>` (decide.mjs `contextRef`).
fn context_ref(context_json: &str) -> String {
    let digest = Sha256::digest(context_json.as_bytes());
    format!("sha256:{digest:x}")
}

/// Does this policy's trigger fire for the given (already-canonicalized) context JSON?
/// `contains: None` ⇒ always fires (it was already phase-selected). A malformed regex fails CLOSED
/// (no fire) — same fail-closed posture as the prototype.
fn triggers(trigger: &Trigger, context_json: &str) -> bool {
    match &trigger.contains {
        None => true,
        Some(pattern) => match regex::Regex::new(pattern) {
            Ok(re) => re.is_match(context_json),
            Err(_) => false,
        },
    }
}

/// Precedence comparator: severity DESC, then id ASC (decide.mjs `byPrecedence`). Drives both the
/// claim's `policy_ids` order and obligation collection order.
fn by_precedence(a: &Policy, b: &Policy) -> std::cmp::Ordering {
    b.severity
        .rank()
        .cmp(&a.severity.rank())
        .then_with(|| a.id.cmp(&b.id))
}

/// Derive a [`ConformanceClaim`] from the selected policies + context using a custom evaluator
/// identity. DETERMINISTIC, NO model: same inputs ⇒ same claim (re-derivable, attestable —
/// ADR-0003).
///
/// The `evaluator_identity` is included in the claim_id seed so two evaluators on the same
/// context produce DIFFERENT claim_ids and don't overwrite each other in the store.
///
/// Decision rule (deny DOMINATES): if any FIRED policy is [`Effect::Deny`] ⇒ [`Decision::Deny`];
/// else union the obligations of every fired [`Effect::AllowWithConditions`] ⇒
/// [`Decision::AllowWithConditions`] when non-empty; else [`Decision::Allow`].
pub fn decide_as(
    selected: &[Policy],
    scope: &str,
    phase: &str,
    context: &serde_json::Value,
    evaluated_at: i64,
    evaluator_identity: &str,
) -> ConformanceClaim {
    let context_json = canonical_context(context);

    // Fired policies, ordered by precedence (severity desc, id asc).
    let mut fired: Vec<&Policy> = selected
        .iter()
        .filter(|p| triggers(&p.trigger, &context_json))
        .collect();
    fired.sort_by(|a, b| by_precedence(a, b));

    let denied = fired.iter().any(|p| p.effect == Effect::Deny);

    let mut obligations: Vec<String> = Vec::new();
    let decision = if denied {
        Decision::Deny
    } else {
        // Collect obligations from triggered allow_with_conditions policies (dedup, order-stable).
        for p in &fired {
            if p.effect == Effect::AllowWithConditions {
                for o in &p.obligations {
                    if !obligations.contains(o) {
                        obligations.push(o.clone());
                    }
                }
            }
        }
        if obligations.is_empty() {
            Decision::Allow
        } else {
            Decision::AllowWithConditions
        }
    };

    let policy_ids: Vec<String> = fired.iter().map(|p| p.id.clone()).collect();
    // Concatenated criteria of the fired policies (decide.mjs joins with " ; ").
    let criteria = fired
        .iter()
        .map(|p| p.criteria.as_str())
        .filter(|c| !c.is_empty())
        .collect::<Vec<_>>()
        .join(" ; ");

    let evaluated_context_ref = context_ref(&context_json);

    // Reproducible claim id: sha256 of (scope, phase, decision, evaluated_context_ref,
    // evaluator_identity). Including evaluator_identity ensures two evaluators on the same context
    // produce different claim_ids and don't overwrite each other in the store.
    let id_seed = serde_json::json!({
        "scope": scope,
        "phase": phase,
        "decision": decision,
        "evaluated_context_ref": evaluated_context_ref,
        "evaluator_identity": evaluator_identity,
    });
    let claim_id = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_string(&id_seed)
                .unwrap_or_default()
                .as_bytes()
        )
    );

    ConformanceClaim {
        claim_id,
        scope: scope.to_string(),
        phase: phase.to_string(),
        policy_ids,
        decision,
        obligations,
        evaluated_context_ref,
        criteria,
        evaluator_identity: evaluator_identity.to_string(),
        evaluated_at,
    }
}

/// Derive a [`ConformanceClaim`] from the selected policies + context. DETERMINISTIC, NO model:
/// same inputs ⇒ same claim (re-derivable, attestable — ADR-0003).
///
/// This is a thin wrapper around [`decide_as`] that stamps the canonical [`EVALUATOR_IDENTITY`].
///
/// Decision rule (deny DOMINATES): if any FIRED policy is [`Effect::Deny`] ⇒ [`Decision::Deny`];
/// else union the obligations of every fired [`Effect::AllowWithConditions`] ⇒
/// [`Decision::AllowWithConditions`] when non-empty; else [`Decision::Allow`].
pub fn decide(
    selected: &[Policy],
    scope: &str,
    phase: &str,
    context: &serde_json::Value,
    evaluated_at: i64,
) -> ConformanceClaim {
    decide_as(
        selected,
        scope,
        phase,
        context,
        evaluated_at,
        EVALUATOR_IDENTITY,
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// ConformanceClaim ↔ Node projection (the claim node IS the evidence on the shared store)
// ─────────────────────────────────────────────────────────────────────────────

/// Stable symbol for a recorded conformance claim.
pub fn claim_symbol(claim_id: &str) -> SymbolId {
    synthetic_symbol(CONFORMANCE_CLAIM, claim_id)
}

/// Project a [`ConformanceClaim`] onto an estate [`Node`] (kind `Other(CONFORMANCE_CLAIM)`). The
/// whole claim is encoded into metadata so [`claim_from_node`] is a lossless inverse.
pub fn claim_to_node(claim: &ConformanceClaim) -> Node {
    let mut node = Node::new(
        claim_symbol(&claim.claim_id),
        NodeKind::Other(CONFORMANCE_CLAIM.to_string()),
        claim.claim_id.clone(),
        Language::new(SYMBOL_SCHEME),
        Location::new(
            format!("{CONFORMANCE_CLAIM}/{}", claim.claim_id),
            Span::ZERO,
        ),
    );
    let value = serde_json::to_value(claim).expect("ConformanceClaim serializes to JSON");
    if let serde_json::Value::Object(map) = value {
        node.metadata = map;
    }
    node
}

/// Reconstruct a [`ConformanceClaim`] from a node produced by [`claim_to_node`].
pub fn claim_from_node(node: &Node) -> anyhow::Result<ConformanceClaim> {
    match &node.kind {
        NodeKind::Other(k) if k == CONFORMANCE_CLAIM => {}
        other => anyhow::bail!("expected NodeKind::Other({CONFORMANCE_CLAIM:?}), got {other:?}"),
    }
    let value = serde_json::Value::Object(node.metadata.clone());
    serde_json::from_value(value)
        .map_err(|e| anyhow::anyhow!("node {} is not a valid ConformanceClaim: {e}", node.name))
}

// ─────────────────────────────────────────────────────────────────────────────
// conform — record the claim node + policy→claim edges, then emit (fire-and-forget)
// ─────────────────────────────────────────────────────────────────────────────

/// Record a conformance claim on the shared store: upsert the claim node and, for each policy that
/// participated (`policy_ids`), a `policy → claim` edge with the native [`EdgeKind::Governs`]
/// (a rule governs the thing it was evaluated against — the closest estate-native fit). Then emit a
/// COARSE fire-and-forget `wicked.crew.governance.conformance_recorded` (counts/ids only, never the
/// context payload). The claim node IS the evidence on the shared graph (the prototype's
/// wicked-vault EvidencePort is out of scope here — see the crate note).
pub fn conform(store: &mut dyn GraphStore, claim: &ConformanceClaim) -> anyhow::Result<()> {
    let claim_node = claim_to_node(claim);
    let claim_id = claim_node.symbol.clone();

    // policy → claim edges (source = dependent policy, target = dependency claim; estate invariant).
    let edges: Vec<Edge> = claim
        .policy_ids
        .iter()
        .map(|pid| {
            Edge::new(
                synthetic_symbol(POLICY, pid),
                claim_id.clone(),
                EdgeKind::Governs,
                ResolutionTier::Parsed,
                GOVERNANCE_RESOLVED_BY,
            )
        })
        .collect();

    store.begin_batch()?;
    store.upsert_nodes(&[claim_node])?;
    if !edges.is_empty() {
        store.upsert_edges(&edges)?;
    }
    store.commit_batch()?;

    // AW-23 (the aw14 verifier's flagged follow-up): a recorded DENIAL that cites conformance
    // rules (`conform:<severity>:<id>:<statement>` obligations, attached by the recall→gate
    // wiring) writes one `evidenced_by` edge claim → rule per cited rule and increments
    // `evidence_count` on the rule's derived Governs edges — the enforcement signal
    // `rules scoreboard` aggregates. After the claim commit so both edge endpoints exist;
    // same-store writes, so a failure is a store failure and propagates (fail-closed). Non-deny
    // claims and claims citing nothing are a no-op inside.
    crate::conformance::record_rule_evidence(store, claim)?;

    // COARSE, fire-and-forget: counts/ids only. A bus failure must NOT fail conformance recording —
    // the durable record is the claim node we just committed.
    let payload = serde_json::json!({
        "claim_id": claim.claim_id,
        "scope": claim.scope,
        "phase": claim.phase,
        "decision": claim.decision,
        "policy_count": claim.policy_ids.len(),
        "obligation_count": claim.obligations.len(),
    });
    let event = EmitEvent::new(
        EV_CONFORMANCE_RECORDED_LITERAL,
        "wicked-governance",
        "governance.conformance",
        payload,
    );
    let _ = emit_event(&event);

    Ok(())
}
