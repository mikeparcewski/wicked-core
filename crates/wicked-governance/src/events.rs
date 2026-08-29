//! Wiki-lifecycle events on wicked-bus (AW-22 / arch-R24) — corpus changes observable as events.
//!
//! ## Catalog
//!
//! | event type | trigger (WHERE it fires) | payload fields |
//! |---|---|---|
//! | [`EV_RULE_INGESTED`] `wicked.estate.rule.ingested` | [`crate::register_rule`], after its store commit — every rule the `wicked-core rules ingest` CLI registers (JSON-bundle lane AND markdown-doc lane) fires one | `rule_id`, `rule_type`, `severity`, `retired`, `source`, `ref`, `confidence` |
//! | [`EV_RULE_RETIRED`] `wicked.estate.rule.retired` | [`crate::retire_rule`], after its store commit — only on an ACTUAL state change (retiring an already-retired rule re-reports success but emits nothing: no change, no event) | `rule_id`, `rule_type`, `severity`, `source`, `ref` |
//! | [`EV_DOC_DRIFTED`] `wicked.estate.doc.drifted` | the drift tooling (AW-10 `rules drift`) calls [`emit_doc_drifted`] once per drifted governed doc — this module ships the seam; the detector lands with AW-10 | `doc_path`, `doc_id`, `reason`, `rule_ids`, `rule_count` |
//!
//! ## Grammar
//!
//! Four-segment `wicked.<domain>.<noun>.<verb>` per `wicked-bus/reqs/SPEC.md` §5 (all lowercase,
//! dot-separated, past-tense verb; three-segment names are not constructed). Every name here
//! passes [`wicked_apps_core::validate_event_type`] — the test suite asserts it. The domain
//! segment is `estate` because the governed corpus's system of record is the shared estate store
//! (arch-R24 names these events verbatim); this is deliberately DISTINCT from the `wicked.crew.*`
//! run-lifecycle catalog ([`wicked_apps_core::EVENT_CATALOG`], single crew producer domain per
//! DES-EXEC-001) — a run event describes an execution, a corpus event describes the rule estate.
//!
//! ## Consumers — the documented triggers for the later waves
//!
//! - **AW-21 (generated-view regen, wave 4):** `rule.ingested` + `rule.retired` are the
//!   corpus-dirty signals. A regen subscriber keys on BOTH (an ingest can mint rules already
//!   retired — `retired: true` in the ingested payload — so neither event alone covers the
//!   catalog-affecting set) and rebuilds the event catalogs / CLAUDE.md tables from the graph.
//! - **AW-24 (retirement propagation, wave 5):** `rule.retired` is the propagation trigger —
//!   consumers mark discovery-lane copies retired and the knowledge twin non-normative
//!   (arch-R22's kill-switch semantics). `doc.drifted` with a deletion-shaped `reason` feeds the
//!   EXPLICIT retire action for the doc's `rule_ids` (a deleted/superseded wiki doc propagates as
//!   retirement of its derived rules, never as silent orphaning).
//!
//! ## Mechanism
//!
//! The established wicked-apps-core emit seam ([`emit_event`]) — the same call
//! [`crate::conform`] already makes for `conformance_recorded`: fire-and-forget, never blocks or
//! fails the caller. The event lands as an `EVENT` node on the shared estate store
//! (`WICKED_ESTATE_DB`) or, when no shared store is configured, on the NDJSON outbox spool
//! (`WICKED_APPS_EMIT_DEADLETTER` override) with a loud `EMIT-DEADLETTER:` stderr marker — never
//! a subprocess, never a live bus dependency. Payloads are COARSE: ids and classifications only,
//! NEVER the rule statement text (same doctrine as `conform` — the durable record is the graph
//! node; the event is the notification).

use serde_json::json;
use wicked_apps_core::emit::{emit_event, EmitEvent};

use crate::conformance::ConformanceRule;

/// A conformance rule was registered on the store (ingest or re-ingest). Payload carries
/// `retired` so consumers can tell an active mint from a non-active doc's withdrawn mint.
pub const EV_RULE_INGESTED: &str = "wicked.estate.rule.ingested";
/// A conformance rule was withdrawn from recall (an actual state change — see the module table).
pub const EV_RULE_RETIRED: &str = "wicked.estate.rule.retired";
/// A governed doc drifted from its ingested rules (changed / deleted / unclaimed). Emitted by the
/// AW-10 drift tooling through [`emit_doc_drifted`].
pub const EV_DOC_DRIFTED: &str = "wicked.estate.doc.drifted";

/// The wiki-lifecycle catalog, in declaration order (mirrors the module table).
pub const WIKI_LIFECYCLE_EVENTS: [&str; 3] = [EV_RULE_INGESTED, EV_RULE_RETIRED, EV_DOC_DRIFTED];

/// Producer app stamped on the emit envelope (matches `conform`'s envelope).
const PRODUCER_DOMAIN: &str = "wicked-governance";
/// Envelope subdomain for corpus-lifecycle events (the envelope's producer-side taxonomy;
/// the event TYPE carries the bus-facing `estate` domain segment).
const CORPUS_SUBDOMAIN: &str = "governance.corpus";

/// One drifted governed doc, as the AW-10 drift tooling reports it. The payload names the doc and
/// the rules minted from it so AW-24's propagation can retire them explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocDrift {
    /// Root-relative path of the drifted doc (forward slashes — matches `provenance.ref`).
    pub doc_path: String,
    /// The doc's frontmatter `id`, when the doc still parses far enough to have one.
    pub doc_id: Option<String>,
    /// Why the doc counts as drifted — e.g. `"doc changed since last ingest"`, `"doc deleted"`,
    /// `"rule doc present but never ingested"`. Deletion-shaped reasons are AW-24's cue to retire.
    pub reason: String,
    /// Ids of the rules minted from this doc at last ingest (the propagation targets).
    pub rule_ids: Vec<String>,
}

/// The coarse `severity`/`rule_type` spellings on the wire — serialized through serde so the event
/// payload can never drift from the conformance-rules schema vocabulary.
fn wire<T: serde::Serialize>(v: &T) -> serde_json::Value {
    serde_json::to_value(v).expect("conformance enum serializes to JSON")
}

/// Build the [`EV_RULE_INGESTED`] event for a just-registered rule. COARSE payload — ids and
/// classifications only, never `rule.statement`.
pub fn rule_ingested_event(rule: &ConformanceRule) -> EmitEvent {
    EmitEvent::new(
        EV_RULE_INGESTED,
        PRODUCER_DOMAIN,
        CORPUS_SUBDOMAIN,
        json!({
            "rule_id": rule.id,
            "rule_type": wire(&rule.rule_type),
            "severity": wire(&rule.severity),
            "retired": rule.retired,
            "source": rule.provenance.source,
            "ref": rule.provenance.reference,
            "confidence": rule.confidence,
        }),
    )
}

/// Build the [`EV_RULE_RETIRED`] event for a rule that just transitioned to `retired`.
pub fn rule_retired_event(rule: &ConformanceRule) -> EmitEvent {
    EmitEvent::new(
        EV_RULE_RETIRED,
        PRODUCER_DOMAIN,
        CORPUS_SUBDOMAIN,
        json!({
            "rule_id": rule.id,
            "rule_type": wire(&rule.rule_type),
            "severity": wire(&rule.severity),
            "source": rule.provenance.source,
            "ref": rule.provenance.reference,
        }),
    )
}

/// Build the [`EV_DOC_DRIFTED`] event for one drifted doc.
pub fn doc_drifted_event(drift: &DocDrift) -> EmitEvent {
    EmitEvent::new(
        EV_DOC_DRIFTED,
        PRODUCER_DOMAIN,
        CORPUS_SUBDOMAIN,
        json!({
            "doc_path": drift.doc_path,
            "doc_id": drift.doc_id,
            "reason": drift.reason,
            "rule_ids": drift.rule_ids,
            "rule_count": drift.rule_ids.len(),
        }),
    )
}

/// Emit [`EV_RULE_INGESTED`] through the shared seam. Fire-and-forget: returns whether the event
/// reached the shared store (`false` = spooled to the outbox), and never errors.
pub fn emit_rule_ingested(rule: &ConformanceRule) -> bool {
    emit_event(&rule_ingested_event(rule))
}

/// Emit [`EV_RULE_RETIRED`] through the shared seam (fire-and-forget, see [`emit_rule_ingested`]).
pub fn emit_rule_retired(rule: &ConformanceRule) -> bool {
    emit_event(&rule_retired_event(rule))
}

/// Emit [`EV_DOC_DRIFTED`] through the shared seam — the entry point the AW-10 drift tooling
/// calls once per drifted doc (fire-and-forget, see [`emit_rule_ingested`]).
pub fn emit_doc_drifted(drift: &DocDrift) -> bool {
    emit_event(&doc_drifted_event(drift))
}
