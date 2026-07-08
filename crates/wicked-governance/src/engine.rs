//! The governance evaluation loop on the SHARED estate store: register → SELECT → DECIDE → conform.
//!
//! Ported from the Node prototype `lib/{select,decide,store,evidence-port}.mjs` (ARCHITECTURE §2),
//! re-grounded on `wicked-apps-core` + the estate graph:
//!
//! - **register_policy** — upsert the policy [`Node`] via the batch write path.
//! - **SELECT** — the index-only fast lane: load `Other(POLICY)` nodes and keep those whose
//!   `applies_to` includes the phase. No model, bounded, deterministic order.
//! - **DECIDE** — the deterministic engine (NO model): a triggered [`Effect::Deny`] ⇒
//!   [`Decision::Deny`] (deny DOMINATES); triggered [`Effect::AllowWithConditions`] ⇒ collect
//!   obligations ⇒ [`Decision::AllowWithConditions`]; else [`Decision::Allow`].
//! - **conform** — upsert the claim [`Node`] + a policy→claim [`EdgeKind::Governs`] edge, then a
//!   COARSE fire-and-forget `wicked.governance.conformance_recorded` event (counts/ids only).
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
/// (`wicked.governance.conformance_recorded`). NOTE: it is NOT the wicked-apps-core catalog constant
/// `EV_CONFORMANCE_RECORDED` (= `"wicked.conformance.recorded"`); the brief's literal wins here and
/// is grammar-valid per `wicked_apps_core::validate_event_type`. Documented divergence (see crate notes).
pub const EV_CONFORMANCE_RECORDED_LITERAL: &str = "wicked.governance.conformance_recorded";

/// The resolver-id recorded on the policy→claim governance edge (estate requires `resolved_by`).
const GOVERNANCE_RESOLVED_BY: &str = "wicked-governance";

// ─────────────────────────────────────────────────────────────────────────────
// register_policy
// ─────────────────────────────────────────────────────────────────────────────

/// Upsert a policy node into the shared store (begin_batch → upsert_nodes → commit_batch).
/// Idempotent: re-registering the same id overwrites the node (estate upsert on the stable symbol).
pub fn register_policy(store: &mut dyn GraphStore, policy: &Policy) -> anyhow::Result<()> {
    let node = policy.to_node();
    store.begin_batch()?;
    store.upsert_nodes(&[node])?;
    store.commit_batch()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// SELECT — index-only fast lane
// ─────────────────────────────────────────────────────────────────────────────

/// Select candidate policies for `phase`: load every `Other(POLICY)` node and keep those whose
/// `applies_to` includes the phase. Bounded, deterministic (returned in id order). `scope` and
/// `context` are accepted for parity with the prototype/hot-path signature; the fast lane keys on
/// `phase` only (overlay/memory enrichment is the deferred slow lane, ARCHITECTURE §2.1).
pub fn select(
    store: &dyn GraphRead,
    _scope: &str,
    phase: &str,
    _context: &serde_json::Value,
) -> anyhow::Result<Vec<Policy>> {
    // Index-only: restrict to the POLICY kind. find_symbols with no text/exact_name does a single
    // scan then retains by kind — the cheap deterministic lane (no FTS, no traversal).
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(POLICY.to_string())],
        ..Default::default()
    };
    let nodes = store.find_symbols(&query)?;

    let mut selected: Vec<Policy> = Vec::new();
    for node in &nodes {
        let policy = Policy::from_node(node)?;
        if policy.applies_to.iter().any(|p| p == phase) {
            selected.push(policy);
        }
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
/// COARSE fire-and-forget `wicked.governance.conformance_recorded` (counts/ids only, never the
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
