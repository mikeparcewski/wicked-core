//! Node / TypeScript bindings (napi-rs) for **wicked-core** — drive the in-process composition /
//! orchestration runtime from JS/TS.
//!
//! ```js
//! const { Core } = require('wicked-core-ts')     // (or `./index.js` in-tree) — the napi loader
//! const core = Core.spawnStub('/tmp/core.db')   // stub engine: deterministic, no real LLM CLI
//! const sub = core.subscribe((err, json) => console.log(JSON.parse(json)))  // live CoreEvent stream
//! // ... later: sub.close()                      // stop delivery + tear the pump/callback down
//! const runId = await core.launchRun({
//!   problem: 'Do step one. Do step two',
//!   sessionId: 'demo',
//!   clisJson: JSON.stringify([{ key: 'a', display_name: 'A', binary: 'a', headless_invocation: 'a {PROMPT}' }]),
//!   humanConfirm: 'before:1',                    // pause before unit 1
//! })
//! // ... on an `awaitingHuman` event:
//! await core.confirmGate(runId, true)            // Approve → run advances to completion
//! ```
//!
//! ## Async shape
//! `wicked_core::Core` is a **sync-blocking** handle: each method sends a `Command` to the store
//! actor and blocks on a oneshot reply (`std::sync::mpsc`, NOT tokio). `Core` is `Send + Sync` and
//! cheap to `Clone`, so every binding method clones the handle into a napi [`AsyncTask`] whose
//! `compute()` runs on a libuv worker thread — the Node event loop is never blocked on the actor
//! round-trip. The live event stream ([`Core::subscribe`]) moves the `Receiver<CoreEvent>` into a
//! dedicated pump thread that forwards each event (as a JSON string) through a
//! [`ThreadsafeFunction`], preserving emission order.
//!
//! Build: `npm run build` (`napi build --platform --release`) from this directory emits the
//! platform-suffixed addon (`wicked-core-ts.<triple>.node`) plus the generated loader `index.js` and
//! `index.d.ts`. A plain `cargo build -p wicked-core-ts` still links the cdylib too — the
//! `.cargo/config.toml` here injects the macOS `dynamic_lookup` linker flags — for a napi-CLI-free
//! dev/IDE loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use napi::bindgen_prelude::{AsyncTask, Buffer};
use napi::threadsafe_function::{
    ErrorStrategy, ThreadSafeCallContext, ThreadsafeFunction, ThreadsafeFunctionCallMode,
};
use napi::{Env, JsFunction, Task};
use napi_derive::napi;

/// Bound on the live-event [`ThreadsafeFunction`] queue (NIT: backpressure). In napi `0` means an
/// UNLIMITED queue; a positive bound caps buffered events if a subscriber's JS callback stalls
/// (excess events are dropped by the NonBlocking `.call()` rather than growing memory unbounded).
/// Sized well above any single run's event count so a normal stream is never truncated.
const EVENT_QUEUE_BOUND: usize = 1024;

use wicked_core::{
    CampaignDef, CampaignStatus, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec,
    RepoSpec, SessionStatus, StubStepRunner,
};
use wicked_council::types::{Confidence, CouncilTask, Dispatcher, Vote};
use wicked_council::AgenticCli;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Map any displayable error onto a napi error (mirrors wicked-memory-ts's `err`).
fn err<E: std::fmt::Display>(e: E) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// Wall-clock now in unix seconds (for repo registration timestamps).
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The snake_case wire token for a [`SessionStatus`] (matches its serde representation).
fn status_token(s: SessionStatus) -> String {
    match s {
        SessionStatus::Planning => "planning",
        SessionStatus::Distributing => "distributing",
        SessionStatus::Executing => "executing",
        SessionStatus::AwaitingHuman => "awaiting_human",
        SessionStatus::Completed => "completed",
        SessionStatus::Cancelled => "cancelled",
        SessionStatus::Failed => "failed",
    }
    .to_string()
}

/// The snake_case wire token for a [`CampaignStatus`] (matches its serde representation — the
/// test below pins the two together, same discipline as [`status_token`]).
fn campaign_status_token(s: CampaignStatus) -> String {
    match s {
        CampaignStatus::Running => "running",
        CampaignStatus::Paused => "paused",
        CampaignStatus::Completed => "completed",
        CampaignStatus::PartiallyCompleted => "partially_completed",
        CampaignStatus::Failed => "failed",
        CampaignStatus::Cancelled => "cancelled",
    }
    .to_string()
}

// Human-confirm parsing lives in wicked_core::HumanConfirm::parse — the ONE parser every entry point
// shares, so the napi layer cannot disagree with the bus/CLI/HTTP paths (FINDING-019).

/// Serialize one [`CoreEvent`] to its tagged JSON object for the JS callback.
///
/// The mapping itself lives in core ([`CoreEvent::to_json`]) so the `/ws` stream and core's durable
/// event log speak ONE vocabulary (FINDING-014) — when it lived here, core could not name its own
/// events and the daemon invented substitutes. Kept as a named wrapper because the tests below pin
/// the shape through this seam.
fn event_to_json(ev: &CoreEvent) -> serde_json::Value {
    ev.to_json()
}

/// Compute the AW-23 population/connection scoreboard over the store at `db_path` — the SAME JSON
/// document `wicked-core rules scoreboard --json` emits (the pretty-printed
/// [`wicked_governance::Scoreboard`] serde form), so a crew/studio consumer and a CLI operator
/// read ONE report shape. Opens a fresh READ-ONLY connection (`open_store_ro`), the same
/// discipline as the other governance reads — safe beside the single-writer actor.
///
/// A free function (not a `Core` method body) so the test below can drive the binding's exact
/// code path against a temp store without constructing a napi `Core`.
fn scoreboard_json(
    db_path: &str,
    docs_dir: Option<&str>,
    ambiguity_cap: usize,
) -> napi::Result<String> {
    let store = wicked_apps_core::open_store_ro(Some(db_path)).map_err(err)?;
    let report =
        wicked_governance::scoreboard(&store, docs_dir.map(std::path::Path::new), ambiguity_cap)
            .map_err(err)?;
    serde_json::to_string_pretty(&report).map_err(err)
}

/// The steering-type vocabulary the STEERING program pins on `ConformanceRule.steering_type`
/// (enum-as-string; a rule authored before the field existed reads as `"architecture"` — the
/// unified model's serde default). Kept in lockstep with wicked-governance's steering-rule
/// model; collapse into a re-export once the model exposes the canonical list.
const STEERING_TYPES: [&str; 7] = [
    "architecture",
    "development",
    "security",
    "testing",
    "operations",
    "compliance",
    "design-ux",
];

/// List conformance rules as a JSON array, with the two facets crew's Steering management
/// surface reads:
///
/// - `steering_type` — exact match on the rule's `steering_type`; a rule whose serialized form
///   lacks the field (authored before the unified steering model) counts as `"architecture"`,
///   mirroring the model's serde default. An unknown value FAILS CLOSED (a typo silently
///   returning `[]` would read as "no rules of that type").
/// - `include_retired` — `recall_rules` is the enforcement funnel and rightly hides withdrawn
///   rows; a MANAGEMENT listing must be able to show them (retire-not-delete: a retired rule
///   still explains the past decisions that cite it). `false` reproduces the exact pre-0.7.5
///   behavior.
///
/// The rows are the full serialized `ConformanceRule` — whatever fields the model carries
/// (steering_type / applies_to / excludes / weight / effect / …) ride through un-projected, so
/// this binding never strips a field the unified model adds. NOTE the model elides
/// default-valued steering fields on the wire (`skip_serializing_if`): an absent
/// `steering_type` reads as `"architecture"`, an absent `weight` as `1.0`. Ordering is the
/// unified steering order: severity-first (critical→info), then weight DESC within a band,
/// then id — on a pre-steering store every weight is the 1.0 default, so this degrades to the
/// exact severity→id order `recall_rules` ships today.
///
/// A free function (not a `Core` method body) so the tests below drive the binding's exact code
/// path against a temp store — the same seam discipline as [`scoreboard_json`].
fn list_conformance_rules_json(
    db_path: &str,
    steering_type: Option<&str>,
    include_retired: bool,
) -> napi::Result<String> {
    use wicked_apps_core::{open_store_ro, synthetic_symbol, FromNode, GraphRead, NodeKind};
    use wicked_estate_core::SymbolQuery;
    use wicked_governance::{ConformanceRule, CONFORMANCE_RULE};

    if let Some(ty) = steering_type {
        if !STEERING_TYPES.contains(&ty) {
            return Err(err(format!(
                "unknown steering type {ty:?} — expected one of {STEERING_TYPES:?}"
            )));
        }
    }

    let store = open_store_ro(Some(db_path)).map_err(err)?;
    // Mirrors `recall_rules`' identification walk (native Rule nodes, own synthetic-symbol
    // round-trip skips foreign Rule nodes) WITHOUT its unconditional retired skip — see the
    // `include_retired` contract above. Collapse onto the unified store read once the
    // steering-rule model's `RuleQuery` carries these facets natively.
    let sym_query = SymbolQuery {
        kinds: vec![NodeKind::Rule],
        ..Default::default()
    };
    let mut rules: Vec<ConformanceRule> = Vec::new();
    for node in store.find_symbols(&sym_query).map_err(err)? {
        if node.symbol != synthetic_symbol(CONFORMANCE_RULE, &node.name) {
            continue; // foreign Rule node (e.g. estate's rules engine) — not ours
        }
        let rule = ConformanceRule::from_node(&node).map_err(err)?;
        if rule.retired && !include_retired {
            continue;
        }
        rules.push(rule);
    }
    // The steering facets read the SERIALIZED form (`steering_type`/`weight` matched as JSON
    // fields, absent ⇒ the model's defaults: "architecture" / 1.0) so this compiles — and stays
    // correct — on both sides of the unified-model landing, where those fields are elided at
    // their defaults anyway.
    let mut out: Vec<(u8, f64, String, serde_json::Value)> = Vec::with_capacity(rules.len());
    for rule in &rules {
        let v = serde_json::to_value(rule).map_err(err)?;
        if let Some(ty) = steering_type {
            let rule_ty = v
                .get("steering_type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("architecture");
            if rule_ty != ty {
                continue;
            }
        }
        let weight = v
            .get("weight")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(1.0);
        out.push((rule.severity.rank(), weight, rule.id.clone(), v));
    }
    // Unified steering order: severity (critical→info), weight DESC (INV-S2 pins weights finite,
    // so the partial_cmp fallback is unreachable), then id. All-default weights ⇒ the exact
    // severity→id order recall_rules ships.
    out.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.2.cmp(&b.2))
    });
    let rows: Vec<serde_json::Value> = out.into_iter().map(|(_, _, _, v)| v).collect();
    serde_json::to_string(&rows).map_err(err)
}

/// List the doctrine RuleSet parents (AW-13 grouping — one `NodeKind::RuleSet` node per doctrine
/// domain, `Contains` membership edges to its rules) as a JSON array of
/// `{ domain, rule_ids, rule_count }` rows, domain-sorted. The array LENGTH is the
/// `ruleset_count` crew's wiki meta reports (`countRuleSets` — `null`-on-missing-binding was its
/// pre-0.7.5 answer); the rows carry membership so the Steering surface can render grouping
/// without a second round-trip. Membership follows the store's `Contains` edges verbatim —
/// retire-not-delete means a retired rule stays listed in its RuleSet (the grouping is doc
/// structure, not enforcement); a membership edge whose target node is gone is skipped.
///
/// Free-function seam for the same reason as [`list_conformance_rules_json`].
fn list_rule_sets_json(db_path: &str) -> napi::Result<String> {
    use wicked_apps_core::{open_store_ro, EdgeKind, GraphRead, NodeKind};
    use wicked_estate_core::{Direction, SymbolQuery};

    let store = open_store_ro(Some(db_path)).map_err(err)?;
    let sym_query = SymbolQuery {
        kinds: vec![NodeKind::RuleSet],
        ..Default::default()
    };
    let mut sets = store.find_symbols(&sym_query).map_err(err)?;
    sets.sort_by(|a, b| a.name.cmp(&b.name));

    let mut rows: Vec<serde_json::Value> = Vec::with_capacity(sets.len());
    for set in &sets {
        let mut rule_ids: Vec<String> = Vec::new();
        for edge in store
            .neighbors(&set.symbol, Direction::Dependencies)
            .map_err(err)?
        {
            if edge.kind != EdgeKind::Contains {
                continue;
            }
            // The member's rule id is the target NODE's name (`to_node` round-trip) — resolved
            // through the store rather than parsed out of the symbol string.
            if let Some(member) = store.get_node(&edge.target).map_err(err)? {
                rule_ids.push(member.name);
            }
        }
        rule_ids.sort_unstable();
        rows.push(serde_json::json!({
            "domain": set.name,
            "rule_ids": rule_ids,
            "rule_count": rule_ids.len(),
        }));
    }
    serde_json::to_string(&rows).map_err(err)
}

/// Per-entry outcome of a steering import batch — the row shape crew's
/// `SteeringImportResult` wire type reads (`index` into the submitted batch, the doc entry's
/// `name` when one was given, `status`, the minted rule `ids` on `imported`, the reason on
/// `rejected`). `None` fields are elided so the wire spells absence as an absent key.
#[derive(serde::Serialize)]
struct SteeringEntryResult {
    index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Materialize one doc entry's inline markdown through the REAL ingest read path — the
/// [`wicked_governance::MarkdownAdapter`] over a throwaway temp dir — so a steering import and
/// `rules ingest --dir` share one parse convention (frontmatter grammar, `## Rules` item shape,
/// provenance `path@sha#id` refs, git-blob sha). Returns the adapter's raw JSON bundle
/// (`{ doc, rules }`); rule materialization stays in [`wicked_governance::normalize_bundle`].
///
/// Content that does not open with a `---` frontmatter fence is a HARD error here: the adapter
/// (rightly) skips fence-less files when sweeping a directory, but an import entry names its doc
/// explicitly — silently minting zero rules would read as success.
fn fetch_doc_bundle(name: Option<&str>, content: &str) -> anyhow::Result<serde_json::Value> {
    use wicked_governance::SourceAdapter;
    // Process-unique + call-unique dir: imports run concurrently on libuv worker threads.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-ts-steering-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("steering import: cannot create temp dir {dir:?}: {e}"))?;
    let file = dir.join(doc_file_name(name));
    let fetched = std::fs::write(&file, content)
        .map_err(|e| anyhow::anyhow!("steering import: cannot write doc to {file:?}: {e}"))
        .and_then(|()| wicked_governance::MarkdownAdapter::new(&dir).fetch());
    let _ = std::fs::remove_dir_all(&dir); // best-effort cleanup either way
    let mut docs = fetched?;
    if docs.is_empty() {
        anyhow::bail!(
            "not a frontmattered rule doc — the content must open with a `---` frontmatter fence"
        );
    }
    Ok(docs.remove(0))
}

/// The temp-dir filename a doc entry lands under — it becomes the `path` half of every minted
/// rule's provenance ref (`<path>@<blob sha>#<id>`), so the caller's `name` is preserved where
/// it is a safe plain filename. Sanitized (no separators, no leading dot — the adapter skips
/// dot-entries) and forced to `.md` (the adapter only reads `*.md`).
fn doc_file_name(name: Option<&str>) -> String {
    let base = name.unwrap_or("entry").trim();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut cleaned = cleaned.trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        cleaned = "entry".into();
    }
    if !cleaned.to_ascii_lowercase().ends_with(".md") {
        cleaned.push_str(".md");
    }
    cleaned
}

/// Import ONE batch entry; `Ok` carries the minted rule ids, `Err` the rejection reason. All
/// writes go through the single-writer actor (`Core::upsert_conformance_rule` — validate +
/// `register_rule`); this function NEVER opens the store itself.
///
/// `minted` is the batch-scoped id ledger: a rule id an EARLIER entry already minted rejects
/// this entry (INV-C3 translated to per-entry form — within one batch the later write would
/// silently overwrite the earlier at `conformance_rule/<id>`, the exact hazard ingest's
/// cross-document check exists to catch). Re-importing an id that already exists ON THE STORE
/// stays a legitimate idempotent upsert.
fn import_steering_entry(
    core: &wicked_core::Core,
    entry: &serde_json::Value,
    default_type: Option<&str>,
    minted: &mut std::collections::HashSet<String>,
) -> anyhow::Result<Vec<String>> {
    let kind = entry
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("entry has no `kind` (expected \"doc\" or \"rule\")"))?;
    match kind {
        // Doc lane: MarkdownAdapter parse → default steering_type where the doc omitted one →
        // the SAME normalize/validate path `rules ingest --dir` runs → actor upsert per rule.
        "doc" => {
            let content = entry
                .get("content")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("doc entry has no string `content`"))?;
            let name = entry.get("name").and_then(serde_json::Value::as_str);
            let mut doc = fetch_doc_bundle(name, content)?;
            if let Some(ty) = default_type {
                // Doc-level `steering_type:` frontmatter already rode onto each raw rule; only
                // rules the doc left untyped take the batch default.
                if let Some(rules) = doc
                    .get_mut("rules")
                    .and_then(serde_json::Value::as_array_mut)
                {
                    for rule in rules.iter_mut() {
                        if let Some(obj) = rule.as_object_mut() {
                            obj.entry("steering_type")
                                .or_insert_with(|| serde_json::Value::String(ty.to_string()));
                        }
                    }
                }
            }
            let rules = wicked_governance::normalize_bundle(&doc, "markdown")?;
            // Whole-entry checks BEFORE the first write, so a rejected entry mints nothing.
            for rule in &rules {
                if minted.contains(&rule.id) {
                    anyhow::bail!(
                        "rule id {:?} was already minted by an earlier entry in this batch \
                         (INV-C3: the later write would silently overwrite it)",
                        rule.id
                    );
                }
            }
            let mut ids = Vec::with_capacity(rules.len());
            for rule in &rules {
                let json = serde_json::to_string(rule)
                    .map_err(|e| anyhow::anyhow!("serializing rule {:?}: {e}", rule.id))?;
                core.upsert_conformance_rule(json)?;
                minted.insert(rule.id.clone());
                ids.push(rule.id.clone());
            }
            Ok(ids)
        }
        // Rule lane: the ready-rule JSON passes to the actor UN-PROJECTED (the model's own serde
        // is the wire contract — same doctrine as `upsertConformanceRule`), with only the batch
        // default `steering_type` spliced in when the entry omits the key.
        "rule" => {
            let mut raw = entry
                .get("rule")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("rule entry has no `rule` object"))?;
            let obj = raw
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("rule entry's `rule` is not a JSON object"))?;
            if let Some(ty) = default_type {
                obj.entry("steering_type")
                    .or_insert_with(|| serde_json::Value::String(ty.to_string()));
            }
            let id = obj
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("rule entry's `rule` has no string `id`"))?;
            if minted.contains(&id) {
                anyhow::bail!(
                    "rule id {id:?} was already minted by an earlier entry in this batch \
                     (INV-C3: the later write would silently overwrite it)"
                );
            }
            let json = serde_json::to_string(&raw)
                .map_err(|e| anyhow::anyhow!("serializing rule {id:?}: {e}"))?;
            core.upsert_conformance_rule(json)?;
            minted.insert(id.clone());
            Ok(vec![id])
        }
        other => anyhow::bail!("unknown entry kind {other:?} (expected \"doc\" or \"rule\")"),
    }
}

/// Run one STEERING import batch — the exact code path the `steeringImport` binding runs on the
/// libuv worker thread. `batch_json` is `{ default_type: string | null, entries: [...] }` where
/// each entry is `{ kind: "doc", name?, content }` or `{ kind: "rule", rule }`; the reply is a
/// JSON array of per-entry results (same order as the batch). Fail-closed PER ENTRY: a bad entry
/// rejects alone, carrying its reason; the rest still land. Only a malformed BATCH (the envelope
/// itself) rejects the whole call — crew maps that to 400, per-entry failures ride the 200.
///
/// A free function (not a `Core` method body) so the test below drives the binding's exact code
/// path against a temp store — the same seam discipline as [`scoreboard_json`].
fn steering_import_json(core: &wicked_core::Core, batch_json: &str) -> napi::Result<String> {
    #[derive(serde::Deserialize)]
    struct Batch {
        #[serde(default)]
        default_type: Option<String>,
        entries: Vec<serde_json::Value>,
    }
    let batch: Batch = serde_json::from_str(batch_json)
        .map_err(|e| err(format!("invalid steering import batch JSON: {e}")))?;
    let default_type = batch.default_type.as_deref();

    let mut minted: std::collections::HashSet<String> = Default::default();
    let mut results: Vec<SteeringEntryResult> = Vec::with_capacity(batch.entries.len());
    for (index, entry) in batch.entries.iter().enumerate() {
        let name = entry
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        match import_steering_entry(core, entry, default_type, &mut minted) {
            Ok(ids) => results.push(SteeringEntryResult {
                index,
                name,
                status: "imported",
                ids: Some(ids),
                error: None,
            }),
            Err(e) => results.push(SteeringEntryResult {
                index,
                name,
                status: "rejected",
                ids: None,
                error: Some(e.to_string()),
            }),
        }
    }
    serde_json::to_string(&results).map_err(err)
}

// ── the AsyncTask that runs one blocking Core call off the Node loop ──────────

/// A single blocking Core call, run on a libuv worker thread. Holds a boxed closure so one Task type
/// serves every method; every result is marshalled as a `String` (a plain value, a status token, or
/// a JSON document the caller parses).
pub struct CoreTask {
    work: Option<Box<dyn FnOnce() -> napi::Result<String> + Send>>,
}

impl Task for CoreTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> napi::Result<String> {
        let work = self
            .work
            .take()
            .ok_or_else(|| err("wicked-core-ts: task polled twice"))?;
        work()
    }

    fn resolve(&mut self, _env: Env, output: String) -> napi::Result<String> {
        Ok(output)
    }
}

/// Wrap a blocking closure in an [`AsyncTask`] → a JS `Promise<string>`.
fn task<F>(f: F) -> AsyncTask<CoreTask>
where
    F: FnOnce() -> napi::Result<String> + Send + 'static,
{
    AsyncTask::new(CoreTask {
        work: Some(Box::new(f)),
    })
}

// ── the stub engine for deterministic, no-real-LLM runs ───────────────────────

/// A deterministic council dispatcher: every seat votes for the first roster option, so the council
/// reaches a clean consensus without spawning any subprocess. Pairs with [`StubStepRunner`] (which
/// returns fixed text) to drive a full run offline — the engine seams tests inject.
struct StubDispatcher;

impl Dispatcher for StubDispatcher {
    fn dispatch(&self, cli: &AgenticCli, task: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: task
                .options
                .first()
                .cloned()
                .unwrap_or_else(|| cli.key.clone()),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "wicked-core-ts stub".into(),
        })
    }
}

// ── the launch spec, as a JS object ───────────────────────────────────────────

/// WHERE this run's project keeps its co-located code graph, and what this run's repo is called
/// inside it. Set on [`LaunchOptions::project_graph`] by a launcher that owns a project graph.
///
/// The two fields are one fact and travel together: a path with no label cannot be checked against
/// the repo the run targets, and a label with no path names nothing. Sending them as separate
/// optional fields would make "half-specified" expressible, and the half that goes missing is the
/// one that turns the verification off.
#[napi(object)]
pub struct ProjectGraphOptions {
    /// ABSOLUTE path to the project's code graph (crew: `~/.wicked-crew/project-graphs/<id>/code-graph.db`).
    pub db_path: String,
    /// The wicked-estate label this run's repo is indexed under in that graph. REQUIRED whenever
    /// `repoRef` is set — the engine uses it to confirm the graph actually describes the code the
    /// worker will edit, and refuses the binding (falling back to the per-repo graph) when it
    /// cannot. Omit only for a repo-less run.
    pub repo_label: Option<String>,
}

/// Options for [`Core::launch_run`]. `clisJson` is a JSON array of `AgenticCli` seats (the council
/// roster); `Core.registryRoster()` returns the production roster ready to pass here.
#[napi(object)]
pub struct LaunchOptions {
    /// The free-text problem to decompose into ordered work units.
    pub problem: String,
    /// A stable session/run id (empty → the caller must supply one; Core requires an explicit id here).
    pub session_id: String,
    /// JSON array of `AgenticCli` seats — the council roster for this run.
    pub clis_json: String,
    /// `shared` (default) | `isolated` — the collection-scope mode.
    pub entity_mode: Option<String>,
    /// Human-confirm gate policy: `none` (default) | `all` | `before:<ord>`.
    pub human_confirm: Option<String>,
    /// The id of a registered repo to run within (creates an isolated worktree). Omit for a repo-less run.
    pub repo_ref: Option<String>,
    /// A registered `WorkflowDef` id (`feature` | `bug` | `migration` or a drop-in). When set, planning
    /// is data-driven from the def's phases; omit for the free-text planner.
    pub workflow: Option<String>,
    /// The project to file this run into (DES-PROJECT-001). The `crew.run` membership is attached
    /// ATOMICALLY with the launch record (one store batch); an unknown or archived project rejects
    /// the launch with no session persisted. Omit for an unfiled run (the synthesized `default`).
    pub project_id: Option<String>,
    /// ADDITIONAL absolute write roots for the run's deliverables (core#259) — e.g. an inbox dir
    /// the workflow contract names as the output destination. Widens the governed units'
    /// filesystem boundary by exactly these roots, after the unit cwd. Each root must be absolute
    /// and outside the engine's config/pin tree — an invalid root REJECTS the launch with no
    /// session persisted. Omit for runs that deliver inside their own workdir.
    pub extra_write_roots: Option<Vec<String>>,
    /// The PROJECT code graph this run's governed workers should query, instead of the run repo's
    /// own graph — one database holding every member repo of the project, so a worker's
    /// SearchEntity / BlastRadius / TraverseGraph can see the whole project rather than one repo.
    ///
    /// A HINT the engine VERIFIES before any worker sees it: absolute, an existing file, not the
    /// engine's own operational store, non-empty, and actually holding `repoLabel`'s repo. A
    /// binding that fails any of those degrades the run to the per-repo graph (with a reason on
    /// stderr) rather than failing the launch — the graph is a capability, and losing it should
    /// cost tools, not the run. Omit for the per-repo behaviour, unchanged.
    pub project_graph: Option<ProjectGraphOptions>,
}

fn build_spec(o: LaunchOptions) -> napi::Result<LaunchSpec> {
    let clis: Vec<AgenticCli> = serde_json::from_str(&o.clis_json)
        .map_err(|e| err(format!("clisJson is not a valid AgenticCli array: {e}")))?;
    Ok(LaunchSpec {
        problem: o.problem,
        clis,
        entity_mode: o
            .entity_mode
            .as_deref()
            .map(EntityMode::parse)
            .unwrap_or(EntityMode::Shared),
        session_id: o.session_id,
        // The ONE canonical parser (FINDING-019): fail CLOSED on a typo instead of the old
        // silent downgrade to None, so a JS caller that mistypes humanConfirm gets a thrown error,
        // not an unattended run.
        human_confirm: HumanConfirm::parse(o.human_confirm.as_deref()).map_err(err)?,
        repo_ref: o.repo_ref,
        workflow: o.workflow,
        project_id: o.project_id,
        extra_write_roots: o.extra_write_roots.unwrap_or_default(),
        project_graph: o.project_graph.map(|g| wicked_core::ProjectGraphBinding {
            db_path: g.db_path,
            repo_label: g.repo_label,
        }),
    })
}

// ── the binding surface ────────────────────────────────────────────────────────

/// A handle to a wicked-core runtime. Construct with [`Core::spawn`] (production engine: real
/// council + wrapped-CLI subprocesses) or [`Core::spawn_stub`] (deterministic offline engine).
#[napi]
pub struct Core {
    inner: wicked_core::Core,
    /// The estate db path — held so governance read methods can open a read-only connection
    /// without going through the single-writer actor (crew#40).
    db_path: String,
}

#[napi]
impl Core {
    /// Spawn the store actor over the estate db at `path` with the PRODUCTION engine (real council
    /// dispatcher + real wrapped-CLI step runner — runs actual agentic CLIs). The actor lives until
    /// every handle is dropped.
    #[napi(factory)]
    pub fn spawn(path: String) -> Core {
        Core {
            inner: wicked_core::Core::spawn(path.clone()),
            db_path: path,
        }
    }

    /// Spawn the store actor with the STUB engine — a deterministic council dispatcher +
    /// `StubStepRunner`, no subprocesses. For tests / offline runs that must not touch a real LLM.
    #[napi(factory)]
    pub fn spawn_stub(path: String) -> Core {
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(StubDispatcher);
        let runner = Arc::new(StubStepRunner);
        Core {
            inner: wicked_core::Core::spawn_with_engine(path.clone(), dispatcher, runner),
            db_path: path,
        }
    }

    /// The production council roster (built-ins ∪ the user's `~/.config/wicked-council/clis.toml`),
    /// as a JSON array of `AgenticCli` — pass straight into `launchRun`'s `clisJson`.
    #[napi]
    pub fn registry_roster() -> napi::Result<String> {
        serde_json::to_string(&wicked_core::registry_roster()).map_err(err)
    }

    /// Subscribe to the live [`CoreEvent`] stream. `callback` follows the Node error-first
    /// convention — `(err, eventJson)` — and is invoked once per event with the event serialized as
    /// a JSON string (`{ type, ...fields }`); parse it in JS. Events arrive in emission order. Call
    /// this BEFORE `launchRun` to catch the whole sequence, and HOLD the returned [`Subscription`]:
    /// `close()` / `unsubscribe()` stops delivery and tears the pump thread + callback down.
    ///
    /// NOTE (ordering): async methods resolve their Promise off a libuv worker thread, while events
    /// are delivered from a separate pump thread — so an event emitted by a call MAY be observed by
    /// this callback slightly AFTER that call's Promise resolves. Await the event you need (as the
    /// smoke does) rather than assuming it precedes the method's resolution.
    #[napi(ts_args_type = "callback: (err: Error | null, eventJson: string) => void")]
    pub fn subscribe(&self, env: Env, callback: JsFunction) -> napi::Result<Subscription> {
        // SIG-1 containment: in napi-rs 2.16 a *throw* inside a ThreadsafeFunction callback escalates
        // to `napi_fatal_exception` (→ uncaughtException → process death) under BOTH
        // `ErrorStrategy::Fatal` AND `CalleeHandled` — the `.call()` "Direct" variant routes a pending
        // exception through `handle_call_js_cb_status` regardless of strategy. So we wrap the user's
        // callback in a JS try/catch shim: the function we hand napi never throws, so a throwing
        // subscriber is contained (swallowed) instead of killing the process. `CalleeHandled` (used
        // for the tsfn + `.call(Ok(..))` below) additionally routes value-conversion failures to the
        // callback's `err` argument instead of aborting.
        let factory: JsFunction =
            env.run_script("(function(cb){return function(err,v){try{cb(err,v)}catch(_e){}}})")?;
        let wrapped: JsFunction = factory.call(None, &[callback])?.try_into()?;

        let mut tsfn: ThreadsafeFunction<String, ErrorStrategy::CalleeHandled> = wrapped
            .create_threadsafe_function(
                EVENT_QUEUE_BOUND,
                |ctx: ThreadSafeCallContext<String>| Ok(vec![ctx.value]),
            )?;
        // SIG-2: unref the tsfn (via the Env) so the pump does NOT hold the libuv loop open — a normal
        // `main()` return lets Node exit on its own, no `process.exit()` needed. `unref` acts on the
        // shared handle, so the pump thread's clone is unref'd too.
        tsfn.unref(&env)?;

        let stop = Arc::new(AtomicBool::new(false));
        let rx = self.inner.subscribe();
        let pump_tsfn = tsfn.clone();
        let pump_stop = stop.clone();
        // SIG-3: one dedicated FIFO pump thread. `recv_timeout` lets it observe the stop flag and
        // exit cleanly on `close()` (instead of blocking on `recv` forever); it also ends when the
        // actor drops the sender (last Core handle gone). On exit it drops `rx`, so the actor prunes
        // this subscriber on its next emit (retain-on-send) — re-subscribing never leaves a second
        // live pump or a duplicated stream.
        let join = std::thread::spawn(move || loop {
            if pump_stop.load(Ordering::SeqCst) {
                break;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(ev) => {
                    let json = event_to_json(&ev).to_string();
                    let _ = pump_tsfn.call(Ok(json), ThreadsafeFunctionCallMode::NonBlocking);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        });

        Ok(Subscription {
            stop,
            join: Mutex::new(Some(join)),
            tsfn: Mutex::new(Some(tsfn)),
        })
    }

    /// Liveness probe — emits a `Heartbeat` to subscribers and resolves once the actor acks (`"ok"`).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn ping(&self) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.ping();
            Ok("ok".to_string())
        })
    }

    /// Open a chat: eagerly warm one ACP session per seat (crew#165 / core#13). Resolves to a
    /// JSON array of per-seat outcomes `[{cliKey, ok, error?}]`; `chatSessionReady`/`chatSessionFailed`
    /// also stream to subscribers. Blocking handshakes run on the task pool, not the JS thread.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn chat_open(
        &self,
        chat_id: String,
        clis_json: String,
        cwd: Option<String>,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let clis: Vec<String> = serde_json::from_str(&clis_json).map_err(err)?;
            let outcomes = core
                .chat_open(&chat_id, &clis, cwd.map(std::path::PathBuf::from))
                .map_err(err)?;
            let arr: Vec<serde_json::Value> = outcomes
                .into_iter()
                .map(|(cli, r)| match r {
                    Ok(()) => serde_json::json!({ "cliKey": cli, "ok": true }),
                    Err(e) => serde_json::json!({ "cliKey": cli, "ok": false, "error": e }),
                })
                .collect();
            serde_json::to_string(&arr).map_err(err)
        })
    }

    /// Fan a message out to the chat's warm seats (all, or `targets_json` subset). Ack-fast:
    /// resolves to the JSON array of seats targeted; replies stream as `chatDelta`/`chatReply`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn chat_send(
        &self,
        chat_id: String,
        text: String,
        targets_json: Option<String>,
        cwd: Option<String>,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let targets: Option<Vec<String>> = match targets_json {
                Some(t) => Some(serde_json::from_str(&t).map_err(err)?),
                None => None,
            };
            let seats = core
                .chat_send(&chat_id, &text, targets, cwd.map(std::path::PathBuf::from))
                .map_err(err)?;
            serde_json::to_string(&seats).map_err(err)
        })
    }

    /// The seats currently warm for a chat — JSON array of cli keys.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn chat_seats(&self, chat_id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let seats = core.chat_seats(&chat_id).map_err(err)?;
            serde_json::to_string(&seats).map_err(err)
        })
    }

    /// Every chat currently holding pool state — JSON array of
    /// `[{chatId, seats, idleSecs}]`, sorted by id.
    ///
    /// Each warm seat pins an ACP bridge plus an agent child (~520 MB resident) and clients mint
    /// chat ids freely, so without this an accumulation is invisible until the host runs out of
    /// memory (FINDING-027). `idleSecs` is seconds since the chat's last open/ensure/turn, or
    /// `null` when no activity was ever recorded — which the reaper treats as idle-since-forever.
    ///
    /// `null` rather than the `u64::MAX` the Rust side uses for that case: a JS `number` is an
    /// f64, so `u64::MAX` arrives as `18446744073709552000` and no consumer can test for the
    /// sentinel by equality. `null` is checkable, and it stops a caller from doing arithmetic on a
    /// value that never meant a duration.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn chat_list(&self) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let chats = core.chat_list().map_err(err)?;
            let arr: Vec<serde_json::Value> = chats
                .into_iter()
                .map(|c| {
                    serde_json::json!({
                        "chatId": c.chat_id,
                        "seats": c.seats,
                        "idleSecs": (c.idle_secs != u64::MAX).then_some(c.idle_secs),
                    })
                })
                .collect();
            serde_json::to_string(&arr).map_err(err)
        })
    }

    /// Close a chat's warm sessions (idempotent); emits
    /// `chatClosed` with `reason: "requested"`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn chat_close(&self, chat_id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.chat_close(&chat_id).map_err(err)?;
            Ok("ok".to_string())
        })
    }

    /// Launch an interactive, resumable run: plans + distributes, then executes each unit off-thread
    /// (or pauses at a human-confirm gate). Resolves to the run id. Progress arrives as `CoreEvent`s
    /// — `subscribe()` first. Rejects with a busy error if a run with that id is already in flight.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn launch_run(&self, opts: LaunchOptions) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let spec = build_spec(opts)?;
            core.launch_run(spec).map_err(err)
        })
    }

    /// Resume an interactive run from its persisted cursor (after a pause, crash, or fresh process).
    /// Resolves to the resulting status token.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn resume_run(&self, run_id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || core.resume_run(&run_id).map(status_token).map_err(err))
    }

    /// Resolve a human-confirm gate on a PAUSED run. `approve=true` proceeds (optionally applying
    /// `amend` to the next unit's instruction); `approve=false` rejects → cancels the run. Resolves
    /// to the resulting status token. Rejects if the run is not paused at a gate.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn confirm_gate(
        &self,
        run_id: String,
        approve: bool,
        amend: Option<String>,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let decision = if approve {
                HumanDecision::Approve { amend }
            } else {
                HumanDecision::Reject
            };
            core.confirm_gate(&run_id, decision)
                .map(status_token)
                .map_err(err)
        })
    }

    /// Cancel a run — mark it terminally `Cancelled` and stop advancing it. Resolves to the status
    /// token. Safe whether the run is executing or paused.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn cancel_run(&self, run_id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || core.cancel_run(&run_id).map(status_token).map_err(err))
    }

    /// Resolve a pending ACP elicitation. `action` must be `"accept"`, `"decline"`, or `"cancel"`;
    /// `response` is the human's typed/selected value as a JSON-typed value — pass `null` for
    /// `decline`/`cancel`. Resolves to `"ok"` on success, rejects if no matching elicitation exists.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn resolve_elicitation(
        &self,
        run_id: String,
        elicitation_id: String,
        action: String,
        response: Option<serde_json::Value>,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.resolve_elicitation(&run_id, &elicitation_id, action, response)
                .map(|_| "ok".to_string())
                .map_err(err)
        })
    }

    // NOTE: there is intentionally no `pauseRun`. wicked-core has no imperative pause — a run pauses
    // ONLY at a declared human-confirm gate (set `humanConfirm` to `all` / `before:<ord>` at launch).
    // Exposing a fake `pauseRun` would misrepresent the engine, so it is omitted (see the report).

    // ── Campaign DAG scheduler (DES-CAMPAIGN-001; crew TH-9) ─────────────────────
    // The scheduler itself is core's (src/campaign.rs — durable, crash-resumable, single-writer);
    // these bindings only marshal it. The 11 `campaign*` CoreEvents were already serialized by
    // `event_to_json` — a subscriber sees them the moment a campaign runs; nothing here adds a
    // second event path. Marshalling matches the rest of this file: complex inputs/outputs are
    // JSON strings in the ENGINE's wire shape (serde snake_case), parsed/produced by core's own
    // serde derives so this layer cannot drift from the actor's.
    //
    // Deliberately NOT bound in this slice: `pauseCampaign` and `confirmCampaignGate`. They ride
    // the studio-scoreboard slice (TH-14/TH-20), which decides how per-node gate prompts surface;
    // binding them before a consumer exists would freeze a signature nothing exercises.

    /// Validate + launch a campaign — a DAG of Runs (DES-CAMPAIGN-001). `defJson` is a
    /// `CampaignDef` JSON object in the engine wire shape (snake_case): `{ id, name?, nodes:
    /// [{ node_id, run_spec: { problem, clis, entity_mode, human_confirm?, repo_ref?,
    /// workflow_id? } }], edges?: [{ from, to, condition? }], policy?, max_concurrency }`.
    /// Resolves to the campaign id. Fire-and-forget: independent nodes dispatch immediately and
    /// progress arrives as the `campaign*` CoreEvents — `subscribe()` first. Rejects a cycle /
    /// empty / duplicate-edge / unknown-edge-endpoint def at launch, before anything persists.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn launch_campaign(&self, def_json: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let def: CampaignDef = serde_json::from_str(&def_json)
                .map_err(|e| err(format!("defJson is not a valid CampaignDef: {e}")))?;
            core.launch_campaign(def).map_err(err)
        })
    }

    /// Resume a campaign from its persisted state (after a pause, crash, or a fresh process) —
    /// the scheduler re-derives the ready set from the persisted node statuses and re-attaches
    /// any mid-run node, never re-running a completed node. Resolves to the campaign status
    /// token (`running` | `paused` | `completed` | `partially_completed` | `failed` |
    /// `cancelled`); rejects for an unknown id.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn resume_campaign(&self, id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.resume_campaign(&id)
                .map(campaign_status_token)
                .map_err(err)
        })
    }

    /// Cancel a campaign — cancel every in-flight node's Run and mark the rest `Cancelled`.
    /// Resolves to the campaign status token; rejects for an unknown id.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn cancel_campaign(&self, id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.cancel_campaign(&id)
                .map(campaign_status_token)
                .map_err(err)
        })
    }

    /// A campaign's full state — the DAG (embedded def), per-node statuses, per-node run ids and
    /// attempt counters — as a JSON `Campaign` object (engine wire shape, snake_case), or the
    /// JSON literal `null` when the id is unknown. The read a DAG/scoreboard view builds from.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn campaign_detail(&self, id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let detail = core.campaign_detail(&id).map_err(err)?;
            serde_json::to_string(&detail).map_err(err)
        })
    }

    /// Every campaign on the store, as a JSON array of `Campaign` objects. Read-only store
    /// connection (the `project_list` pattern) — the single-writer actor is not involved, so a
    /// long campaign list cannot queue behind dispatch work.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn campaign_list(&self) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            let store = wicked_apps_core::open_store_ro(Some(db_path.as_str())).map_err(err)?;
            let campaigns = wicked_core::all_campaigns(&store).map_err(err)?;
            serde_json::to_string(&campaigns).map_err(err)
        })
    }

    /// The agent session ids currently on the store, as a JSON array of strings.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn sessions(&self) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let ids = core.sessions().map_err(err)?;
            serde_json::to_string(&ids).map_err(err)
        })
    }

    /// Every session + its ordered units, as a JSON array of `{ session, units }` objects (the read
    /// a UI builds its project list from).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn sessions_detail(&self) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let views = core.sessions_detail().map_err(err)?;
            // NIT: surface a serialize failure as a napi error instead of silently substituting
            // `null` (which would hand the UI a malformed row it can't distinguish from real data).
            let mut arr: Vec<serde_json::Value> = Vec::with_capacity(views.len());
            for v in &views {
                arr.push(serde_json::json!({
                    "session": serde_json::to_value(&v.session).map_err(err)?,
                    "units": serde_json::to_value(&v.units).map_err(err)?,
                }));
            }
            serde_json::to_string(&arr).map_err(err)
        })
    }

    /// A unit's captured work output (transcript), as a JSON value — a string, or `null` if none.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn work_output(&self, unit_id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let out = core.work_output(&unit_id);
            serde_json::to_string(&out).map_err(err)
        })
    }

    /// A run's recorded event history, oldest first, as a JSON array. Each entry is the SAME tagged
    /// object the `/ws` stream carries ([`CoreEvent::to_json`]) plus a capture-time `ts` (epoch millis)
    /// and an ordering `seq`.
    ///
    /// The read half of FINDING-014: an evidence bundle assembled after a run must read what actually
    /// happened rather than re-derive pseudo-events from unit records, which cannot recover what it
    /// never saw and invents its own type names doing it. Because the log and the socket serialize
    /// through one mapping, an event named here is the event named live.
    ///
    /// Empty array for an unknown run, one that emitted nothing, or one predating the log — an absent
    /// history is not an error. Streaming chunk events (`cliOutputDelta`, `chatDelta`,
    /// `terminalOutput`, `heartbeat`) are excluded by design.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_events(&self, run_id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let events = core.run_events(&run_id);
            serde_json::to_string(&events).map_err(err)
        })
    }

    /// Register a git repository the orchestrator can run within. Validates it is a git repo with
    /// ≥1 commit; resolves to the persisted `RepoEntry` as a JSON object.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn register_repo(&self, name: String, root_path: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let spec = RepoSpec {
                name,
                root_path,
                registered_at: now_secs(),
            };
            let entry = core.register_repo(spec).map_err(err)?;
            serde_json::to_string(&entry).map_err(err)
        })
    }

    /// List every registered repository, as a JSON array of `RepoEntry` objects.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn list_repos(&self) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let repos = core.list_repos().map_err(err)?;
            serde_json::to_string(&repos).map_err(err)
        })
    }

    // ── Projects (DES-PROJECT-001) ─────────────────────────────────────────────
    // Writes ride the single-writer actor; reads open READ-ONLY connections, exactly
    // like the governance reads below.

    /// Create a project. Resolves to the persisted `Project` as a JSON object
    /// (`{ id, name, description, status, scope, created_at, updated_at }`). Rejects on an
    /// empty/overlong name or a name already used by an ACTIVE project (the API's 409).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn project_create(&self, name: String, description: Option<String>) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let project = core.project_create(&name, description).map_err(err)?;
            serde_json::to_string(&project).map_err(err)
        })
    }

    /// Rename / describe / archive / restore a project (`status`: `active` | `archived`;
    /// `description: ""` clears it). Resolves to the updated `Project` JSON. Rejects for the
    /// synthesized `default` project or an unknown id.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn project_update(
        &self,
        id: String,
        name: Option<String>,
        description: Option<String>,
        status: Option<String>,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let status = match status.as_deref() {
                Some(s) => Some(wicked_core::ProjectStatus::parse(s).map_err(err)?),
                None => None,
            };
            let patch = wicked_core::ProjectPatch {
                name,
                description,
                status,
            };
            let project = core.project_update(&id, patch).map_err(err)?;
            serde_json::to_string(&project).map_err(err)
        })
    }

    /// Every project on the store (all statuses — the caller filters), newest first, as a JSON
    /// array of `Project` objects. The synthesized `default` project is an API-layer concept and
    /// is NOT in this list.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn project_list(&self) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            let store = wicked_apps_core::open_store_ro(Some(db_path.as_str())).map_err(err)?;
            let projects = wicked_core::list_projects(&store).map_err(err)?;
            serde_json::to_string(&projects).map_err(err)
        })
    }

    /// One project by id, as a JSON `Project` object — or the JSON literal `null` when unknown.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn project_get(&self, id: String) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            let store = wicked_apps_core::open_store_ro(Some(db_path.as_str())).map_err(err)?;
            let project = wicked_core::get_project(&store, &id).map_err(err)?;
            serde_json::to_string(&project).map_err(err)
        })
    }

    /// The LIVE members of a project, oldest attach first, as a JSON array of `ProjectMember`
    /// objects (`{ id, project_id, member_kind, member_ref, meta, attached_at, attached_by }`).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn project_members(&self, project_id: String) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            let store = wicked_apps_core::open_store_ro(Some(db_path.as_str())).map_err(err)?;
            let members = wicked_core::list_members(&store, &project_id).map_err(err)?;
            serde_json::to_string(&members).map_err(err)
        })
    }

    /// Attach a member (`memberKind` is the open `<product>.<noun>` grammar, e.g. `crew.run`,
    /// `interactive.doc`; `metaJson` is opaque JSON text; `attachedBy` ∈ studio|interactive|cli|api).
    /// Idempotent on `(project, kind, ref)`. Resolves to `{ "member": ProjectMember, "created":
    /// boolean }` — emit the membership.attached event only when `created` is true.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn project_member_attach(
        &self,
        project_id: String,
        member_kind: String,
        member_ref: String,
        meta_json: Option<String>,
        attached_by: Option<String>,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            // `meta` is opaque to the ENGINE, but the contract says it is JSON text — enforce
            // that here at the boundary so a malformed blob is refused rather than persisted
            // for a downstream consumer to choke on (Copilot, PR #246).
            if let Some(ref meta) = meta_json {
                serde_json::from_str::<serde_json::Value>(meta)
                    .map_err(|e| err(format!("metaJson is not valid JSON: {e}")))?;
            }
            let spec = wicked_core::MemberSpec {
                project_id,
                member_kind,
                member_ref,
                meta: meta_json,
                attached_by: attached_by.unwrap_or_else(|| "api".to_string()),
            };
            let (member, created) = core.project_attach_member(spec).map_err(err)?;
            serde_json::to_string(&serde_json::json!({
                "member": member,
                "created": created,
            }))
            .map_err(err)
        })
    }

    /// Detach a member. Resolves to `"true"` when a live membership was removed, `"false"` when
    /// no such live member exists on that project (the caller answers 404). Detaching never
    /// touches the member's own data (the run, the doc dir).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn project_member_detach(
        &self,
        project_id: String,
        member_id: String,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let removed = core
                .project_detach_member(&project_id, &member_id)
                .map_err(err)?;
            serde_json::to_string(&removed).map_err(err)
        })
    }

    /// The project ids holding a live membership for `(memberKind, memberRef)` — the reverse read
    /// (run → projects) the daemon uses to tag frames and synthesize the `default` project. JSON
    /// array of strings.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn member_projects(&self, member_kind: String, member_ref: String) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            let store = wicked_apps_core::open_store_ro(Some(db_path.as_str())).map_err(err)?;
            let ids =
                wicked_core::member_projects(&store, &member_kind, &member_ref).map_err(err)?;
            serde_json::to_string(&ids).map_err(err)
        })
    }

    /// Durable interaction requests (DES-PROJECT-001 §5.3), newest first, optionally filtered by
    /// run and/or status (`open` | `answered` | `expired` | `cancelled`). JSON array of
    /// `{ id, session_id, kind, ord, reviewing_ord, prompt, status, answer, created_at,
    /// resolved_at }`. This is the durable truth the daemon's gate/elicitation caches demote to
    /// latency layers over — it survives a daemon restart because the actor wrote it in the same
    /// batch as the run's `awaiting_human` transition.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn interaction_requests(
        &self,
        session_id: Option<String>,
        status: Option<String>,
    ) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            let status = match status.as_deref() {
                Some(s) => Some(wicked_core::InteractionStatus::parse(s).map_err(err)?),
                None => None,
            };
            let store = wicked_apps_core::open_store_ro(Some(db_path.as_str())).map_err(err)?;
            let requests = wicked_core::list_interactions(&store, session_id.as_deref(), status)
                .map_err(err)?;
            serde_json::to_string(&requests).map_err(err)
        })
    }

    // ── Memory + knowledge (the foundation record, DES-PROJECT-001 §3.2) ────────
    // Thin wrappers over the actor's existing memory/knowledge commands, so the daemon can write
    // the project charter + probe a project's record without opening the stores itself (they are
    // single-writer sidecars the actor owns).

    /// Capture an episodic memory at `scope` (STRICT `kind:id[/kind:id...]` path; `""` = root —
    /// a malformed segment REJECTS rather than silently re-rooting). Resolves to `"ok"`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn capture_memory(&self, content: String, scope: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            wicked_core::validate_scope_path(&scope).map_err(err)?;
            core.capture_memory(&content, &scope).map_err(err)?;
            Ok("ok".to_string())
        })
    }

    /// LIST memories within `scope`'s subtree (strict path; `""` = all), newest first, up to
    /// `limit`. JSON array of `{ content, score, tier }`. `listMemories("project:<id>", …).length
    /// > 0` is the cheap "does this project have a record?" probe (the ADR's memory.coverage).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn list_memories(&self, scope: String, limit: u32) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            wicked_core::validate_scope_path(&scope).map_err(err)?;
            let memories = core.list_memories(&scope, limit as usize).map_err(err)?;
            serde_json::to_string(&memories).map_err(err)
        })
    }

    /// Ingest a document (title + chunks) into the knowledge store. `chunksJson` is a JSON array
    /// of strings. Resolves to the ingested chunk count as a JSON number.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn ingest_knowledge(&self, title: String, chunks_json: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let chunks: Vec<String> = serde_json::from_str(&chunks_json)
                .map_err(|e| err(format!("chunksJson is not a JSON string array: {e}")))?;
            let n = core.ingest_knowledge(&title, chunks).map_err(err)?;
            serde_json::to_string(&n).map_err(err)
        })
    }

    /// Recall up to `k` knowledge chunks relevant to `query`. JSON array of
    /// `{ content, score, source }`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn recall_knowledge(&self, query: String, k: u32) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let hits = core.recall_knowledge(&query, k as usize).map_err(err)?;
            serde_json::to_string(&hits).map_err(err)
        })
    }

    // ── Governance reads (crew#40) ──────────────────────────────────────────────
    // Each method opens a fresh READ-ONLY connection (open_store_ro) so it never
    // races with the single-writer actor. The actor continues to hold the one writable
    // handle; readonly connections are safe to open concurrently on SQLite WAL mode.

    /// All registered governance policies, as a JSON array of `Policy` objects.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn list_policies(&self) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            use wicked_apps_core::{open_store_ro, FromNode, GraphRead, NodeKind};
            use wicked_estate_core::SymbolQuery;
            use wicked_governance::Policy;
            let store = open_store_ro(Some(db_path.as_str())).map_err(err)?;
            let query = SymbolQuery {
                kinds: vec![NodeKind::Other("policy".to_string())],
                ..Default::default()
            };
            let mut policies: Vec<Policy> = Vec::new();
            for node in store.find_symbols(&query).map_err(err)? {
                match Policy::from_node(&node) {
                    Ok(p) => policies.push(p),
                    Err(e) => eprintln!(
                        "wicked-core-ts: policy node '{}' failed to parse: {e}",
                        node.symbol
                    ),
                }
            }
            policies.sort_by(|a, b| a.id.cmp(&b.id));
            serde_json::to_string(&policies).map_err(err)
        })
    }

    /// All conformance rules on the store (Pattern + Policy types), as a JSON array of
    /// serialized `ConformanceRule` objects — severity-first (critical→info), then weight DESC
    /// within a band, then id. The rows carry the unified steering-rule model's fields
    /// (steering_type / applies_to / excludes / weight / effect / trigger / obligations /
    /// criteria / provenance / …) exactly as the model serializes them — default-valued steering
    /// fields are elided on the wire (absent steering_type ⇒ "architecture", absent weight ⇒ 1).
    ///
    /// Steering facets (the studio Steering surface's list):
    /// - `steeringType` filters on the rule's `steering_type` — one of architecture | development |
    ///   security | testing | operations | compliance | design-ux. A rule authored before the
    ///   field existed counts as `"architecture"` (the model's serde default); an unknown value
    ///   REJECTS (fails closed — a typo must not read as "no rules of that type").
    /// - `includeRetired: true` adds withdrawn rules (retire-not-delete: they still explain the
    ///   past decisions that cite them; recall/enforcement never returns them).
    ///
    /// Both omitted ⇒ the exact pre-0.7.5 behavior (every active rule).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn list_conformance_rules(
        &self,
        steering_type: Option<String>,
        include_retired: Option<bool>,
    ) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            list_conformance_rules_json(
                &db_path,
                steering_type.as_deref(),
                include_retired.unwrap_or(false),
            )
        })
    }

    /// The doctrine RuleSet parents (AW-13 grouping) as a JSON array of
    /// `{ domain, rule_ids, rule_count }` rows, domain-sorted. The array length is the wiki
    /// meta's `ruleset_count` (crew's `countRuleSets` resolved `null` on engine builds without
    /// this binding — "cannot count" must never impersonate "0"); the rows carry `Contains`
    /// membership so grouping renders without a second round-trip. Membership is the store's
    /// edges verbatim — a retired rule stays listed in its RuleSet (grouping is doc structure,
    /// not enforcement). Read-only connection; never blocks the single-writer actor.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn list_rule_sets(&self) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || list_rule_sets_json(&db_path))
    }

    /// All conformance claims (governance decisions) on the store, as a JSON array.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn list_conformance_claims(&self) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            use wicked_apps_core::{
                open_store_ro, ConformanceClaim, GraphRead, NodeKind, CONFORMANCE_CLAIM,
            };
            use wicked_estate_core::SymbolQuery;
            use wicked_governance::claim_from_node;
            let store = open_store_ro(Some(db_path.as_str())).map_err(err)?;
            let query = SymbolQuery {
                kinds: vec![NodeKind::Other(CONFORMANCE_CLAIM.to_string())],
                ..Default::default()
            };
            let mut claims: Vec<ConformanceClaim> = Vec::new();
            for node in store.find_symbols(&query).map_err(err)? {
                match claim_from_node(&node) {
                    Ok(c) => claims.push(c),
                    Err(e) => eprintln!(
                        "wicked-core-ts: claim node '{}' failed to parse: {e}",
                        node.symbol
                    ),
                }
            }
            serde_json::to_string(&claims).map_err(err)
        })
    }

    /// The AW-23 / arch-R23 population/connection scoreboard — the wiki-health report that tells
    /// a POPULATED rule corpus from an ingested-once-and-decaying one: rule counts, typing
    /// coverage, connection (ref-resolution) coverage, enforcement evidence, recall volume.
    /// Resolves to the SAME JSON `wicked-core rules scoreboard --json` emits (a pretty-printed
    /// `Scoreboard` object) — one report shape for CLI operators and crew/studio consumers alike.
    ///
    /// `docsDir` mirrors the CLI's `--dir`: typing coverage is doc-side (`enforcement_class`
    /// lives in frontmatter, never on the rule node), so it needs the SAME docs root
    /// `rules ingest --dir` used; omit it and the report says `typing.available: false`,
    /// honestly, in-band — never a fake 0% or 100%. `ambiguityCap` mirrors `--ambiguity-cap`
    /// (default 5; must be ≥ 1 — fails closed on 0, like the CLI). Strictly a REPORT over a
    /// read-only connection: it never blocks the single-writer actor, and residue gating stays
    /// `rules drift`'s job.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn governance_scoreboard(
        &self,
        docs_dir: Option<String>,
        ambiguity_cap: Option<u32>,
    ) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            let cap = match ambiguity_cap {
                None => wicked_governance::DEFAULT_AMBIGUITY_CAP,
                Some(0) => return Err(err("ambiguityCap must be a positive integer, got 0")),
                Some(n) => n as usize,
            };
            scoreboard_json(&db_path, docs_dir.as_deref(), cap)
        })
    }

    // ── Governance writes (crew#42) ─────────────────────────────────────────────

    /// Upsert a governance policy. `policy_json` is a JSON-serialized `Policy` object
    /// (fields: id, kind, applies_to, effect, trigger, severity, criteria, rule, obligations).
    /// Validates server-side (fails closed on deny_unknown_fields + required fields). Idempotent
    /// on stable id — calling twice with the same id and payload is a no-op.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn upsert_policy(&self, policy_json: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.upsert_policy(policy_json).map_err(err)?;
            Ok(String::new())
        })
    }

    /// Upsert a conformance rule. `rule_json` is a JSON-serialized `ConformanceRule` object
    /// (fields: id, rule_type, statement, severity, confidence, targets, provenance — plus the
    /// unified steering-rule fields: steering_type, applies_to, excludes, weight, and the
    /// optional effect / trigger / obligations / criteria; a rule without `effect` stays
    /// recall-only). The JSON passes through un-projected — the model's own serde is the wire
    /// contract, so new steering fields ride this binding without a rebuild. Provenance is
    /// first-class for UI/chat-authored rules too (`provenance.source: "ui" | "chat"`), not just
    /// doc-ingested `path@sha#id` rows. Validates server-side (INV-C1/C2/C4). Idempotent on
    /// stable id.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn upsert_conformance_rule(&self, rule_json: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.upsert_conformance_rule(rule_json).map_err(err)?;
            Ok(String::new())
        })
    }

    /// STEERING batch import (the unified steering-rule model). `batch_json` is a JSON
    /// `{ default_type: string | null, entries: [...] }` document where each entry is either a
    /// frontmattered markdown doc (`{ kind: "doc", name?, content }` — parsed by the SAME
    /// MarkdownAdapter/normalize path `rules ingest --dir` runs, provenance `path@sha#id` refs
    /// included) or a ready rule object (`{ kind: "rule", rule }` — the rule JSON passes to the
    /// upsert path un-projected, so new model fields ride through without a rebuild).
    /// `default_type` is applied as the `steering_type` of every rule whose entry omits one; a
    /// rule that names its own type keeps it.
    ///
    /// Fail-closed PER ENTRY: a bad entry (unparseable doc, invalid rule, INV violation,
    /// duplicate id within the batch) rejects ALONE with its reason — the rest still land; only
    /// a malformed batch envelope rejects the whole call. Every write goes through the
    /// single-writer actor (validate + `register_rule`). Resolves to a JSON array of per-entry
    /// results, batch order: `{ index, name?, status: "imported" | "rejected", ids?, error? }`
    /// (`ids` = the rule ids the entry minted — a doc can mint several; a rejected entry mints
    /// none). This binding is also crew's PRESENCE SENTINEL for the whole steering seam
    /// (`steeringSupported()`): it ships with the unified model, so its existence tells crew the
    /// engine round-trips the steering fields instead of silently dropping them.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn steering_import(&self, batch_json: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || steering_import_json(&core, &batch_json))
    }

    /// Withdraw a governance policy from enforcement (FINDING-038 — governance state was otherwise
    /// append-only, so a mis-authored policy denied forever).
    ///
    /// Retire, not delete: the node stays readable so a past decision citing this id can still be
    /// explained, but SELECT stops returning it, so it can never decide another gate.
    ///
    /// Resolves to a JSON-encoded boolean: the four characters `true` if a policy with that id
    /// existed, the five characters `false` if none did. Like every method here it hands JS a
    /// `Promise<string>` carrying JSON, so it must be `JSON.parse`d — a bare truthiness test
    /// passes on BOTH values and would read a miss as a hit, losing exactly the 200-vs-404
    /// distinction this return value exists to carry.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn retire_policy(&self, id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let found = core.retire_policy(&id).map_err(err)?;
            serde_json::to_string(&found).map_err(err)
        })
    }

    /// Archive (or unarchive) a TERMINAL run (crew#265) — a write-off, not a delete: the run and
    /// its artifacts stay fully readable, but default run listings exclude it. Resolves to a
    /// JSON-encoded boolean (`true` = session existed, `false` = unknown id → answer 404); REJECTS
    /// when the run is non-terminal (the caller answers 409 — write-off is for finished history
    /// only). Parse the reply; a bare truthiness test reads a miss as a hit.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn archive_run(
        &self,
        run_id: String,
        archived: bool,
        note: Option<String>,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let found = core.archive_run(&run_id, archived, note).map_err(err)?;
            serde_json::to_string(&found).map_err(err)
        })
    }

    /// Withdraw a conformance rule from recall. Same retire-not-delete contract as
    /// [`Core::retire_policy`], and the same JSON-encoded `true`/`false` reply that must be
    /// parsed rather than tested for truthiness.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn retire_conformance_rule(&self, id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let found = core.retire_conformance_rule(&id).map_err(err)?;
            serde_json::to_string(&found).map_err(err)
        })
    }

    /// Register (or replace) a workflow definition in the actor's runtime registry. `json` is a
    /// JSON-serialised `WorkflowDef` object (fields: id, description, phases — see the wicked-core
    /// workflow schema). Validates server-side (id + ≥1 phase required); rejects invalid JSON or a
    /// structurally invalid def. Returns the registered workflow id. Idempotent on id — calling
    /// twice replaces the first registration. The def is immediately visible to the next `launchRun`
    /// call; no process restart required.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn register_workflow(&self, json: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || core.register_workflow(json).map_err(err))
    }

    /// Recall which conformance rules apply to the given `query_json` (a JSON-serialized
    /// `RuleQuery` — fields: language, layer, framework, severity, rule_type, steering_type;
    /// all optional).
    /// An empty or whitespace `query_json` is treated as an all-rules query (no facet filters).
    /// Opens a read-only connection — does not block the single-writer actor. Returns a JSON
    /// array of `ConformanceRule` objects, severity-first then id.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn recall_rules_preview(&self, query_json: String) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            use wicked_apps_core::open_store_ro;
            use wicked_governance::{recall_rules, RuleQuery};
            let store = open_store_ro(Some(db_path.as_str())).map_err(err)?;
            let query: RuleQuery = if query_json.trim().is_empty() {
                RuleQuery::default()
            } else {
                serde_json::from_str(&query_json).map_err(err)?
            };
            let rules = recall_rules(&store, &query).map_err(err)?;
            serde_json::to_string(&rules).map_err(err)
        })
    }

    /// Front-half coverage gate report — JSON-serialized `CoverageReport`, or the JSON literal
    /// `null` when the store has no domain-model nodes yet. Opens a read-only connection so it
    /// never blocks the single-writer actor.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn get_coverage_report(&self) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            use wicked_apps_core::open_store_ro;
            use wicked_governance::recompute_front_half_coverage;
            let store = open_store_ro(Some(db_path.as_str())).map_err(err)?;
            match recompute_front_half_coverage(&store) {
                Ok(report) => serde_json::to_string(&report).map_err(err),
                Err(e) => {
                    eprintln!("wicked-core-ts: getCoverageReport failed: {e}");
                    Ok("null".to_string())
                }
            }
        })
    }

    /// Coverage for ONE registered repo, computed over that repo's OWN code graph — not the daemon's
    /// bookkeeping store (FINDING-009). `get_coverage_report` above reads `self.db_path` (the daemon
    /// `core.db`), which holds run/governance nodes but none of a repo's domain/requirement nodes, so
    /// it reports a vacuous `coverage: 1.0` over an empty denominator and cannot name a repo. This
    /// resolves the repo from the registry, opens its `code_graph_db` (the engine-resolved path
    /// every consumer shares — the legacy in-tree `<root>/.codegraph/estate.db` for a repo that
    /// already has one, else the estate home's `<estate_root>/<key>/estate.db`; see wicked-core's
    /// `code_graph.rs` ADR), and recomputes over it. An unknown `repo_ref` is an
    /// ERROR, never a silent vacuous report — the caller must name a real repo.
    /// Resolves to the coverage report as a JSON string (`ts_return_type` pins it — the crew adapter
    /// used to cast away an `unknown` here; #225 review).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn get_coverage_report_for_repo(&self, repo_ref: String) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            use wicked_apps_core::open_store_ro;
            let daemon = open_store_ro(Some(db_path.as_str())).map_err(err)?;
            // The resolve-repo → open-its-store → recompute logic lives in wicked-core so it is
            // unit-testable there (this napi layer stays thin glue). An unknown repo errors.
            let report = wicked_core::coverage_report_for_repo(&daemon, &repo_ref).map_err(err)?;
            serde_json::to_string(&report).map_err(err)
        })
    }

    /// Node-count-by-kind summary of ONE registered repo's code graph, over that repo's OWN store
    /// (#122). Resolves to a JSON string: an array of `{ "kind": string, "count": number }`,
    /// kind-sorted. An unknown `repo_ref` REJECTS — never a silent empty summary.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn get_graph_kinds_for_repo(&self, repo_ref: String) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            use wicked_apps_core::open_store_ro;
            let daemon = open_store_ro(Some(db_path.as_str())).map_err(err)?;
            let kinds = wicked_core::graph_kinds_for_repo(&daemon, &repo_ref).map_err(err)?;
            let shaped: Vec<_> = kinds
                .into_iter()
                .map(|(kind, count)| serde_json::json!({ "kind": kind, "count": count }))
                .collect();
            serde_json::to_string(&shaped).map_err(err)
        })
    }

    // ── PTY terminal sessions (DES-TERMINAL-001) ────────────────────────────────
    // Each method runs its (potentially blocking) Core call on a libuv worker thread via the SAME
    // `CoreTask`/`AsyncTask` pattern as every other method — the Node event loop is never blocked on
    // PTY open/write/resize/close. Terminal *events* (`terminalOpened` / `terminalOutput` with a
    // base64 `bytesB64` / `terminalExited`) arrive on the `subscribe` stream; `subscribe()` BEFORE
    // `openTerminal` to catch the whole sequence.

    /// Open a PTY terminal session running `cmd` (or the login shell if omitted) in `cwd`, sized
    /// `cols`x`rows`. `governed=false` is a loud, opt-in UNGOVERNED operator shell that bypasses the
    /// gate-hook (DES §7); pass `true` for the governed default. Resolves the new terminal id. Output
    /// arrives as `terminalOutput` events, so `subscribe()` FIRST to catch `terminalOpened` + bytes.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn open_terminal(
        &self,
        cwd: String,
        cmd: Option<Vec<String>>,
        cols: u16,
        rows: u16,
        governed: bool,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.open_terminal(cwd, cmd, cols, rows, governed)
                .map_err(err)
        })
    }

    /// Write raw input bytes (keystrokes) to a terminal. The `bytes` Buffer is copied to an owned
    /// `Vec<u8>` on the Node thread (a cheap memcpy — keystroke payloads are tiny) so the blocking
    /// write can run off-thread without moving a JS-owned Buffer across threads. Resolves `"ok"`;
    /// rejects if the terminal id is unknown or the write fails.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn write_terminal(&self, id: String, bytes: Buffer) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        let data: Vec<u8> = bytes.to_vec();
        task(move || {
            core.write_terminal(&id, &data).map_err(err)?;
            Ok("ok".to_string())
        })
    }

    /// Resize a terminal's PTY to `cols`x`rows`. Resolves `"ok"`; rejects if the terminal id is
    /// unknown or the resize fails.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn resize_terminal(&self, id: String, cols: u16, rows: u16) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.resize_terminal(&id, cols, rows).map_err(err)?;
            Ok("ok".to_string())
        })
    }

    /// Close a terminal: the actor kills the child, joins the reader thread, and drops the registry +
    /// I/O entries (no orphaned process/thread). Resolves `"ok"` once teardown completes; a
    /// `terminalExited` event is emitted. Rejects on an unknown id.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn close_terminal(&self, id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.close_terminal(&id).map_err(err)?;
            Ok("ok".to_string())
        })
    }

    /// Inject an operator message into one or all active PTY workers for a run.
    ///
    /// `target` is either `"all"` (write to every PTY session for the run) or a CLI key string
    /// (write only to that CLI's session). ACP-backed sessions have no PTY and are skipped with a
    /// warning. Fires [`CoreEvent::WorkerMessageInjected`] for each successful write.
    #[napi]
    pub fn inject_worker_message(
        &self,
        run_id: String,
        message: String,
        target: String,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let t = if target == "all" {
                wicked_core::InjectTarget::All
            } else {
                wicked_core::InjectTarget::Cli(target)
            };
            core.inject_worker_message(&run_id, &message, t)
                .map_err(err)?;
            Ok("ok".to_string())
        })
    }

    /// Stop the current worker for `ord` in run `run_id` and re-dispatch it.
    ///
    /// `newCli` is either a CLI key string (re-dispatch immediately to that CLI) or `null` (re-run
    /// the council and let it pick). Returns `"ok"` when the command has been queued; the
    /// [`CoreEvent::UnitReassigned`] event confirms the reassignment.
    #[napi]
    pub fn reassign_unit(
        &self,
        run_id: String,
        ord: u32,
        new_cli: Option<String>,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.reassign_unit(&run_id, ord, new_cli).map_err(err)?;
            Ok("ok".to_string())
        })
    }
}

// ── the live-event subscription handle ─────────────────────────────────────────

/// A live event subscription returned by [`Core::subscribe`]. Owns the FIFO pump thread + its
/// [`ThreadsafeFunction`]. `close()` / `unsubscribe()` tears both down deterministically (set the
/// stop flag → join the pump, which drops the event `Receiver` so the actor prunes its sender →
/// abort the tsfn). Idempotent; dropping the JS handle without an explicit close also stops the pump.
#[napi]
pub struct Subscription {
    stop: Arc<AtomicBool>,
    join: Mutex<Option<JoinHandle<()>>>,
    tsfn: Mutex<Option<ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>>,
}

#[napi]
impl Subscription {
    /// Stop delivering events and release the pump thread + `ThreadsafeFunction`. Idempotent and
    /// safe on a normal shutdown path; after it returns the pump is joined and the tsfn aborted, so
    /// the callback will not fire again. This is the teardown that makes re-subscribe leak-free and
    /// lets a plain `main()` return promptly.
    #[napi]
    pub fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.join.lock().ok().and_then(|mut g| g.take()) {
            let _ = handle.join();
        }
        if let Some(tsfn) = self.tsfn.lock().ok().and_then(|mut g| g.take()) {
            // abort() releases with `abort` mode + flips the shared `aborted` flag, so any call the
            // pump had queued/attempts is a no-op (never a use-after-free on env teardown).
            let _ = tsfn.abort();
        }
    }

    /// Alias for [`Subscription::close`].
    #[napi]
    pub fn unsubscribe(&self) {
        self.close();
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // If the JS handle is GC'd without an explicit close(), still signal the pump to stop so it
        // can't run (and re-deliver) forever. Detach rather than join — Drop may run on the JS
        // thread, and the pump exits within one `recv_timeout` tick, dropping its `Receiver` + tsfn
        // clone (whose Drop then releases the tsfn, as it was never aborted).
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the tests name a failure kind now that the CoreEvent → JSON mapping lives in core.
    use serde_json::Value;
    use wicked_core::StepFailureKind;

    /// Assert one hand-mapped variant: its `type` tag and the EXACT set of JSON keys it emits. This
    /// pins the hand-written `CoreEvent → JSON` mapping (`event_to_json` — the studio's only view of
    /// the stream) so a renamed key or a wrong tag fails CI instead of silently drifting.
    fn check(ev: CoreEvent, expected_type: &str, expected_keys: &[&str]) {
        let v: Value = event_to_json(&ev);
        let obj = v.as_object().expect("mapping emits a JSON object");
        assert_eq!(
            obj.get("type").and_then(Value::as_str),
            Some(expected_type),
            "wrong type tag for {expected_type}"
        );
        let mut got: Vec<&str> = obj.keys().map(String::as_str).collect();
        got.sort_unstable();
        let mut want: Vec<&str> = expected_keys.to_vec();
        want.sort_unstable();
        assert_eq!(got, want, "key set drift for {expected_type}");
    }

    /// Pin EVERY mapped `CoreEvent` variant's tag (camelCase) + exact key set. Because `CoreEvent` is
    /// `#[non_exhaustive]`, a NEW variant no longer breaks the build (the defensive `_` arm catches
    /// it) — so this test is the tripwire: a new variant with no explicit arm falls through to
    /// `{"type":"unknown"}` and any missing pin here signals the arm was never added.
    #[test]
    fn every_mapped_variant_has_stable_tag_and_keys() {
        let s = || "s".to_string();
        check(CoreEvent::Heartbeat, "heartbeat", &["type"]);
        check(
            CoreEvent::ChatSessionReady {
                chat: "c".into(),
                cli_key: "claude".into(),
            },
            "chatSessionReady",
            &["type", "chat", "cliKey"],
        );
        check(
            CoreEvent::ChatSessionFailed {
                chat: "c".into(),
                cli_key: "claude".into(),
                reason: "boom".into(),
            },
            "chatSessionFailed",
            &["type", "chat", "cliKey", "reason"],
        );
        check(
            CoreEvent::ChatDelta {
                chat: "c".into(),
                cli_key: "claude".into(),
                text: "t".into(),
            },
            "chatDelta",
            &["type", "chat", "cliKey", "text"],
        );
        check(
            CoreEvent::ChatReply {
                chat: "c".into(),
                cli_key: "claude".into(),
                text: "t".into(),
                ok: true,
            },
            "chatReply",
            &["type", "chat", "cliKey", "text", "ok"],
        );
        check(
            CoreEvent::ChatClosed {
                chat: "c".into(),
                reason: "idle".into(),
            },
            "chatClosed",
            &["type", "chat", "reason"],
        );
        check(
            CoreEvent::SessionStarted {
                session: s(),
                problem: s(),
                workflow_id: None,
                cli_count: 1,
                governed: false,
                entity_mode: s(),
            },
            "sessionStarted",
            &[
                "type",
                "session",
                "problem",
                "workflowId",
                "cliCount",
                "governed",
                "entityMode",
            ],
        );
        check(
            CoreEvent::UnitPlanned {
                session: s(),
                ord: 1,
                description: s(),
                stage: s(),
                role: s(),
                gate: s(),
                skill_ref: None,
                has_validator_pin: false,
                executor_type: s(),
            },
            "unitPlanned",
            &[
                "type",
                "session",
                "ord",
                "description",
                "stage",
                "role",
                "gate",
                "skillRef",
                "hasValidatorPin",
                "executorType",
            ],
        );
        check(
            CoreEvent::UnitDistributed {
                session: s(),
                ord: 1,
                cli: s(),
                routing_method: s(),
                agreement_pct: None,
                returned: None,
                seated: None,
                dissent: None,
                degraded_reason: None,
            },
            "unitDistributed",
            &[
                "type",
                "session",
                "ord",
                "cli",
                "routingMethod",
                "agreementPct",
                "returned",
                // The quorum denominator `returned` must be read against. Emitted unconditionally
                // (null when unknown) so a consumer never has to guess whether its absence means
                // "one-seat council" or "field not sent" (FINDING-026 D).
                "seated",
                "dissent",
                "degradedReason",
            ],
        );
        check(
            CoreEvent::UnitExecuting {
                session: s(),
                ord: 1,
            },
            "unitExecuting",
            &["type", "session", "ord"],
        );
        check(
            CoreEvent::CliOutputDelta {
                session: s(),
                ord: 1,
                chunk: s(),
            },
            "cliOutputDelta",
            &["type", "session", "ord", "chunk"],
        );
        // The throttled/coalesced live-output stream (cliOutputDelta's off-process sibling);
        // `attempt` rides so a consumer can discard a superseded attempt's output.
        check(
            CoreEvent::UnitOutputDelta {
                session: s(),
                ord: 1,
                attempt: 0,
                text: s(),
            },
            "unitOutputDelta",
            &["type", "session", "ord", "attempt", "text"],
        );
        check(
            CoreEvent::GateDecided {
                session: s(),
                ord: 1,
                allow: true,
            },
            "gateDecided",
            &["type", "session", "ord", "allow"],
        );
        // ── DES-STUDIO-COCKPIT-001 §3 B-events (the 4 new insight variants) ──
        check(
            CoreEvent::GateEvaluated {
                session: s(),
                ord: 1,
                criterion: Some(s()),
                has_deterministic_floor: true,
                deterministic_pass: true,
                agent_verdict: Some(s()),
                agent_reasoning: Some(s()),
                evaluator_pass: Some(true),
                evaluator_policies: vec![s()],
                denial_reason: None,
                combined: true,
            },
            "gateEvaluated",
            &[
                "type",
                "session",
                "ord",
                "criterion",
                "hasDeterministicFloor",
                "deterministicPass",
                "agentVerdict",
                "agentReasoning",
                "evaluatorPass",
                "evaluatorPolicies",
                "denialReason",
                "combined",
            ],
        );
        check(
            CoreEvent::UnitDispatched {
                session: s(),
                ord: 1,
                attempt: 0,
            },
            "unitDispatched",
            &["type", "session", "ord", "attempt"],
        );
        check(
            CoreEvent::CliUsage {
                session: s(),
                ord: 1,
                attempt: 0,
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 6,
                cache_creation_tokens: 2,
                cost_usd: Some(0.4),
            },
            "cliUsage",
            &[
                "type",
                "session",
                "ord",
                "attempt",
                "inputTokens",
                "outputTokens",
                "cacheReadTokens",
                "cacheCreationTokens",
                "costUsd",
            ],
        );
        check(
            CoreEvent::DataUsed {
                session: s(),
                ord: 1,
                files: vec![s()],
            },
            "dataUsed",
            &["type", "session", "ord", "files"],
        );
        check(
            CoreEvent::ToolInvoked {
                session: s(),
                ord: 1,
                attempt: 0,
                tools: vec![s()],
            },
            "toolInvoked",
            &["type", "session", "ord", "attempt", "tools"],
        );
        // ── remaining variants ──
        check(
            CoreEvent::UnitDone {
                session: s(),
                ord: 1,
            },
            "unitDone",
            &["type", "session", "ord"],
        );
        check(
            CoreEvent::UnitDenied {
                session: s(),
                ord: 1,
            },
            "unitDenied",
            &["type", "session", "ord"],
        );
        check(
            CoreEvent::AwaitingHuman {
                session: s(),
                ord: 1,
                reviewing_ord: Some(1),
                prompt: s(),
            },
            "awaitingHuman",
            &["type", "session", "ord", "reviewingOrd", "prompt"],
        );
        check(
            CoreEvent::Resumed {
                session: s(),
                ord: 1,
            },
            "resumed",
            &["type", "session", "ord"],
        );
        check(
            CoreEvent::RunCancelled { session: s() },
            "runCancelled",
            &["type", "session"],
        );
        check(
            CoreEvent::SessionFailed {
                session: s(),
                ord: 1,
            },
            "sessionFailed",
            &["type", "session", "ord"],
        );
        check(
            CoreEvent::RepoRegistered { repo_ref: s() },
            "repoRegistered",
            &["type", "repoRef"],
        );
        check(
            CoreEvent::SessionCompleted { session: s() },
            "sessionCompleted",
            &["type", "session"],
        );
        check(
            CoreEvent::WorkerMessageInjected {
                session: s(),
                message: s(),
                target: s(),
            },
            "workerMessageInjected",
            &["type", "session", "message", "target"],
        );
        check(
            CoreEvent::AssumptionRecorded {
                session: s(),
                ord: 1,
                kind: s(),
                library: s(),
                transform: s(),
                known: true,
                detail: s(),
            },
            "assumptionRecorded",
            &[
                "type",
                "session",
                "ord",
                "kind",
                "library",
                "transform",
                "known",
                "detail",
            ],
        );
        check(
            CoreEvent::WorkerMessageQueued {
                session: s(),
                message: s(),
                target: s(),
            },
            "workerMessageQueued",
            &["type", "session", "message", "target"],
        );
        check(
            CoreEvent::UnitReassigned {
                session: s(),
                ord: 1,
                attempt: 1,
                previous_cli: s(),
                new_cli: Some(s()),
            },
            "unitReassigned",
            &["type", "session", "ord", "attempt", "previousCli", "newCli"],
        );
        check(
            CoreEvent::Error {
                session: Some(s()),
                message: s(),
            },
            "error",
            &["type", "session", "message"],
        );
        check(
            CoreEvent::TerminalOpened { id: s(), cwd: s() },
            "terminalOpened",
            &["type", "id", "cwd"],
        );
        check(
            CoreEvent::TerminalOutput {
                id: s(),
                seq: 7,
                bytes_b64: s(),
            },
            "terminalOutput",
            &["type", "id", "seq", "bytesB64"],
        );
        check(
            CoreEvent::TerminalExited {
                id: s(),
                status: Some(0),
            },
            "terminalExited",
            &["type", "id", "status"],
        );
        check(
            CoreEvent::CampaignLaunched { campaign: s() },
            "campaignLaunched",
            &["type", "campaign"],
        );
        check(
            CoreEvent::CampaignNodeReady {
                campaign: s(),
                node: s(),
            },
            "campaignNodeReady",
            &["type", "campaign", "node"],
        );
        check(
            CoreEvent::CampaignNodeStarted {
                campaign: s(),
                node: s(),
                run_id: s(),
            },
            "campaignNodeStarted",
            &["type", "campaign", "node", "runId"],
        );
        check(
            CoreEvent::CampaignNodeAwaitingHuman {
                campaign: s(),
                node: s(),
                run_id: s(),
                prompt: s(),
            },
            "campaignNodeAwaitingHuman",
            &["type", "campaign", "node", "runId", "prompt"],
        );
        check(
            CoreEvent::CampaignNodeCompleted {
                campaign: s(),
                node: s(),
            },
            "campaignNodeCompleted",
            &["type", "campaign", "node"],
        );
        check(
            CoreEvent::CampaignNodeFailed {
                campaign: s(),
                node: s(),
            },
            "campaignNodeFailed",
            &["type", "campaign", "node"],
        );
        check(
            CoreEvent::CampaignNodeBlocked {
                campaign: s(),
                node: s(),
            },
            "campaignNodeBlocked",
            &["type", "campaign", "node"],
        );
        check(
            CoreEvent::CampaignPaused { campaign: s() },
            "campaignPaused",
            &["type", "campaign"],
        );
        check(
            CoreEvent::CampaignCompleted { campaign: s() },
            "campaignCompleted",
            &["type", "campaign"],
        );
        check(
            CoreEvent::CampaignFailed { campaign: s() },
            "campaignFailed",
            &["type", "campaign"],
        );
        check(
            CoreEvent::CampaignCancelled { campaign: s() },
            "campaignCancelled",
            &["type", "campaign"],
        );
        check(
            CoreEvent::StepFailed {
                session: s(),
                ord: 1,
                attempt: 0,
                detail: s(),
                failure_kind: StepFailureKind::WorkerError,
            },
            "stepFailed",
            &["type", "session", "ord", "attempt", "detail", "failureKind"],
        );
        check(
            CoreEvent::CrashRecoveryRedrive {
                session: s(),
                ord: 1,
                attempt: 1,
            },
            "crashRecoveryRedrive",
            &["type", "session", "ord", "attempt"],
        );
        check(
            CoreEvent::WorkerSessionStarted {
                session: s(),
                terminal_id: s(),
                cli_key: s(),
            },
            "workerSessionStarted",
            &["type", "session", "terminalId", "cliKey"],
        );
        check(
            CoreEvent::AcpSessionStarted {
                session: s(),
                cli_key: s(),
                acp_session_id: s(),
            },
            "acpSessionStarted",
            &["type", "session", "cliKey", "acpSessionId"],
        );
        check(
            CoreEvent::AcpFallback {
                session: s(),
                cli_key: s(),
                reason: s(),
                fallback_kind: s(),
            },
            "acpFallback",
            &["type", "session", "cliKey", "reason", "fallbackKind"],
        );
        // P2 observability events — worker-lifecycle wave (EVT-003, EVT-004, EVT-007).
        check(
            CoreEvent::WorkerSessionReused {
                session: s(),
                terminal_id: s(),
                ord: 2,
            },
            "workerSessionReused",
            &["type", "session", "terminalId", "ord"],
        );
        check(
            CoreEvent::WorkerSessionClosed {
                session: s(),
                terminal_id: s(),
                reason: s(),
            },
            "workerSessionClosed",
            &["type", "session", "terminalId", "reason"],
        );
        check(
            CoreEvent::UnitContextInjected {
                session: s(),
                ord: 2,
                recipient_cli: s(),
                prior_units: vec![wicked_core::InjectedContext {
                    ord: 1,
                    label: s(),
                    output_bytes: 42,
                }],
            },
            "unitContextInjected",
            &["type", "session", "ord", "recipientCli", "priorUnits"],
        );
        // P2 governance-deep wave (EVT-008, EVT-009, EVT-010, EVT-011, EVT-016).
        check(
            CoreEvent::GovernanceHookFired {
                session: s(),
                ord: 1,
                attempt: 0,
                tool_name: s(),
                decision: "allow".to_string(),
                denying_policy: None,
            },
            "governanceHookFired",
            &[
                "type",
                "session",
                "ord",
                "attempt",
                "toolName",
                "decision",
                "denyingPolicy",
            ],
        );
        check(
            CoreEvent::ValidationPinAttached {
                session: s(),
                ord: 1,
                pin: s(),
                criterion: s(),
            },
            "validationPinAttached",
            &["type", "session", "ord", "pin", "criterion"],
        );
        check(
            CoreEvent::GateEscalated {
                session: s(),
                ord: 1,
                condition: "verdict_not_pass".to_string(),
                verdict_summary: s(),
            },
            "gateEscalated",
            &["type", "session", "ord", "condition", "verdictSummary"],
        );
        check(
            CoreEvent::ToolExecutorDispatched {
                session: s(),
                ord: 1,
                cmd: vec!["echo".to_string(), "hello".to_string()],
                workdir: None,
            },
            "toolExecutorDispatched",
            &["type", "session", "ord", "cmd", "workdir"],
        );
        check(
            CoreEvent::GovernanceContextArmed {
                session: s(),
                ord: 1,
                attempt: 0,
                path: "wrapped_cli".to_string(),
                db_path: s(),
            },
            "governanceContextArmed",
            &["type", "session", "ord", "attempt", "path", "dbPath"],
        );
        check(
            CoreEvent::GovernanceUnenforced {
                session: s(),
                ord: 4,
                attempt: 0,
                cli: s(),
                reason: s(),
            },
            "governanceUnenforced",
            &["type", "session", "ord", "attempt", "cli", "reason"],
        );
        // P2 decisions-full wave (EVT-001, EVT-012, EVT-013).
        check(
            CoreEvent::WorkflowSelected {
                session: s(),
                workflow_id: "feature".to_string(),
                unit_count: 2,
            },
            "workflowSelected",
            &["type", "session", "workflowId", "unitCount"],
        );
        check(
            CoreEvent::UnitReworkAmended {
                session: s(),
                ord: 1,
                amendment: "add error handling".to_string(),
                updated_description: s(),
            },
            "unitReworkAmended",
            &["type", "session", "ord", "amendment", "updatedDescription"],
        );
        check(
            CoreEvent::UnitOutputCaptured {
                session: s(),
                ord: 1,
                attempt: 0,
                output_bytes: 512,
                step_status: "ok".to_string(),
                governed: false,
            },
            "unitOutputCaptured",
            &[
                "type",
                "session",
                "ord",
                "attempt",
                "outputBytes",
                "stepStatus",
                "governed",
            ],
        );
    }
    /// Pin [`campaign_status_token`] to serde's own representation of [`CampaignStatus`] — the
    /// hand-written match and the derive must never disagree, because the resume/cancel bindings
    /// answer the former while `campaignDetail`/`campaignList` JSON carries the latter.
    #[test]
    fn campaign_status_token_matches_serde() {
        for status in [
            CampaignStatus::Running,
            CampaignStatus::Paused,
            CampaignStatus::Completed,
            CampaignStatus::PartiallyCompleted,
            CampaignStatus::Failed,
            CampaignStatus::Cancelled,
        ] {
            let via_serde = serde_json::to_value(status).expect("serializes");
            assert_eq!(
                Some(campaign_status_token(status).as_str()),
                via_serde.as_str(),
                "token drift for {status:?}"
            );
        }
    }

    /// Pin the `defJson` wire shape `launchCampaign` accepts: the ENGINE's serde form
    /// (snake_case fields, snake_case enum tokens, defaulted optionals). A crew-side mapper is
    /// written against exactly this JSON — a rename here is a wire break, not a refactor.
    #[test]
    fn campaign_def_json_wire_shape_parses() {
        let def_json = r#"{
            "id": "camp-1",
            "nodes": [
                { "node_id": "a", "run_spec": {
                    "problem": "run scenario file /tmp/s1.spec.json",
                    "clis": [],
                    "entity_mode": "shared",
                    "workflow_id": "campaign-camp-1-a"
                } },
                { "node_id": "b", "run_spec": {
                    "problem": "run scenario file /tmp/s2.spec.json",
                    "clis": [],
                    "entity_mode": "shared"
                } }
            ],
            "edges": [ { "from": "a", "to": "b", "condition": "on_success" } ],
            "policy": "continue_independent",
            "max_concurrency": 2
        }"#;
        let def: CampaignDef = serde_json::from_str(def_json).expect("engine wire shape parses");
        assert_eq!(def.id, "camp-1");
        assert_eq!(def.nodes.len(), 2);
        assert_eq!(def.edges.len(), 1);
        assert_eq!(def.max_concurrency, 2);
        assert_eq!(
            def.nodes[0].run_spec.workflow_id.as_deref(),
            Some("campaign-camp-1-a")
        );
        // Defaulted optionals: `name`, `human_confirm`, `repo_ref`, `workflow_id` may be absent.
        assert_eq!(def.nodes[1].run_spec.workflow_id, None);
        // And the validator core runs at launch accepts this def (no cycle, endpoints exist).
        wicked_core::validate_campaign(&def).expect("a valid def validates");
    }

    /// AC (wiki scoreboard binding): [`scoreboard_json`] — the exact code path
    /// `governanceScoreboard` runs on the libuv worker thread — returns valid scoreboard JSON
    /// over a temp store, in the SAME shape `wicked-core rules scoreboard --json` emits (the
    /// `Scoreboard` serde form). Key drift here is a wire break for every crew/studio consumer
    /// AND a CLI-parity break, so the exact top-level key set is pinned.
    /// Set the hermetic emit spool ONCE per process: `register_rule`'s fire-and-forget emission
    /// must never append to the operator's real `~/.something-wicked/wicked-apps/emit-outbox.ndjson`
    /// (the same set-once-never-unset pattern as wicked-governance's own `hermetic_test_spool`).
    fn hermetic_spool() {
        static SPOOL: std::sync::Once = std::sync::Once::new();
        SPOOL.call_once(|| {
            let path = std::env::temp_dir().join(format!(
                "wicked-core-ts-test-outbox-{}.ndjson",
                std::process::id()
            ));
            std::env::set_var(wicked_apps_core::emit::DEADLETTER_ENV, &path);
        });
    }

    /// A fresh file-backed temp-store path (`:memory:` cannot be shared with the bindings' fresh
    /// read-only connections), with any residue of a previous run of the same test removed.
    fn temp_store_path(name: &str) -> String {
        let db =
            std::env::temp_dir().join(format!("wicked-core-ts-{name}-{}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(db.with_extension(format!("db{suffix}")));
        }
        db.to_str().expect("temp path is utf-8").to_string()
    }

    /// Build a [`wicked_governance::ConformanceRule`] from its minimal WIRE form (the JSON shape
    /// `upsertConformanceRule` accepts) rather than a struct literal — serde defaults fill every
    /// optional field, so this helper keeps compiling as the unified steering-rule model grows
    /// (`steering_type` / `applies_to` / `excludes` / `weight` / … all carry serde defaults).
    fn wire_rule(id: &str, severity: &str) -> wicked_governance::ConformanceRule {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "rule_type": if id.starts_with("POL-") { "policy" } else { "pattern" },
            "statement": format!("statement for {id}"),
            "severity": severity,
            "confidence": 0.9,
        }))
        .expect("the minimal wire-shape rule parses")
    }

    #[test]
    fn governance_scoreboard_reports_over_a_temp_store() {
        hermetic_spool();

        // A file-backed temp store, populated through the real write path.
        let db_path = temp_store_path("scoreboard");
        {
            let mut store = wicked_apps_core::open_store(Some(&db_path)).expect("store opens");
            let rule = wire_rule("POL-100", "critical");
            wicked_governance::register_rule(&mut store, &rule).expect("rule registers");
        }

        let json = scoreboard_json(&db_path, None, wicked_governance::DEFAULT_AMBIGUITY_CAP)
            .expect("scoreboard over a temp store");
        let v: Value = serde_json::from_str(&json).expect("binding emits valid JSON");

        // The CLI report's exact top-level key set (`Scoreboard`'s serde form), order-insensitive
        // (serde_json's default Map is sorted; the pin must not depend on that).
        let mut keys: Vec<&str> = v
            .as_object()
            .expect("a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "by_type",
                "connection",
                "evidence",
                "recall_volume",
                "rules_active",
                "rules_retired",
                "rules_total",
                "typing",
            ]
        );
        assert_eq!(v["rules_total"], 1);
        // STEERING: the by-type breakdown files the (defaulted) rule under architecture.
        assert_eq!(v["by_type"]["architecture"]["total"], 1);
        assert_eq!(v["rules_active"], 1);
        assert_eq!(v["rules_retired"], 0);
        // No docs root supplied → typing coverage reports unavailable IN-BAND (the doc-side
        // metric needs the same `--dir` root `rules ingest` used), never a fabricated percent.
        assert_eq!(v["typing"]["available"], false);
        // Recall volume is documented-unavailable in-band (see wicked-governance's scoreboard docs).
        assert_eq!(v["recall_volume"]["available"], false);
    }

    /// AC (steering facets): [`list_conformance_rules_json`] — the exact code path
    /// `listConformanceRules` runs on the libuv worker thread — honors both new facets over a
    /// temp store populated through the real write path:
    /// - default (no facets) reproduces the pre-0.7.5 behavior: active rules only,
    ///   severity-first then id;
    /// - `include_retired: true` adds the withdrawn row (retire-not-delete);
    /// - `steering_type: "architecture"` matches a rule whose serialized form predates the
    ///   field (the unified model's serde default), any other type excludes it;
    /// - an unknown steering type FAILS CLOSED instead of answering `[]`.
    #[test]
    fn list_conformance_rules_honors_steering_facets() {
        hermetic_spool();

        let db_path = temp_store_path("list-rules");
        {
            let mut store = wicked_apps_core::open_store(Some(&db_path)).expect("store opens");
            for (id, sev) in [("PAT-200", "warn"), ("POL-200", "critical")] {
                wicked_governance::register_rule(&mut store, &wire_rule(id, sev))
                    .expect("rule registers");
            }
            wicked_governance::register_rule(&mut store, &wire_rule("PAT-201", "error"))
                .expect("rule registers");
            assert!(
                wicked_governance::retire_rule(&mut store, "PAT-201").expect("retire runs"),
                "the rule to retire exists"
            );
        }

        let ids = |json: &str| -> Vec<String> {
            let rows: Vec<Value> = serde_json::from_str(json).expect("binding emits valid JSON");
            rows.iter()
                .map(|r| r["id"].as_str().expect("rows carry an id").to_string())
                .collect()
        };

        // Default: active only, severity-first (critical→info) then id — the pre-0.7.5 list.
        let active = list_conformance_rules_json(&db_path, None, false).expect("lists");
        assert_eq!(ids(&active), ["POL-200", "PAT-200"]);

        // include_retired surfaces the withdrawn row in its severity slot, and the row SAYS so.
        let all = list_conformance_rules_json(&db_path, None, true).expect("lists");
        assert_eq!(ids(&all), ["POL-200", "PAT-201", "PAT-200"]);
        let rows: Vec<Value> = serde_json::from_str(&all).unwrap();
        assert_eq!(rows[1]["retired"], true, "the retired row is marked");

        // steering_type facet: a pre-steering row counts as "architecture" (the model default) …
        let arch =
            list_conformance_rules_json(&db_path, Some("architecture"), false).expect("lists");
        assert_eq!(ids(&arch), ["POL-200", "PAT-200"]);
        // … and is excluded by every other type.
        let sec = list_conformance_rules_json(&db_path, Some("security"), false).expect("lists");
        assert_eq!(ids(&sec), Vec::<String>::new());

        // Unknown type fails closed — never an empty array impersonating "no such rules".
        let bad = list_conformance_rules_json(&db_path, Some("archtecture"), false)
            .expect_err("a typo'd steering type must reject");
        assert!(
            bad.reason.contains("unknown steering type"),
            "error names the problem: {}",
            bad.reason
        );
    }

    /// AC (steering import binding): [`steering_import_json`] — the exact code path
    /// `steeringImport` runs on the libuv worker thread — lands a mixed 3-entry batch against a
    /// temp store THROUGH THE SINGLE-WRITER ACTOR, per-entry fail-closed:
    /// - a good frontmattered doc imports, minting BOTH its rules (ids in doc order);
    /// - a good ready-rule JSON imports under its own id;
    /// - a malformed doc (no frontmatter fence) rejects ALONE with its reason — the other two
    ///   entries land anyway (fail-closed per entry, never per batch);
    /// - all three entries omitted `steering_type`, so the batch `default_type` applied — the
    ///   imported rules are listable under that type with the ids they were minted under.
    #[test]
    fn steering_import_lands_per_entry_results_over_a_temp_store() {
        hermetic_spool();

        let db_path = temp_store_path("steering-import");
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(StubDispatcher);
        let core = wicked_core::Core::spawn_with_engine(
            db_path.clone(),
            dispatcher,
            Arc::new(StubStepRunner),
        );

        let good_doc = "---\n\
                        id: DOC-AUTH\n\
                        title: Auth steering rules\n\
                        ---\n\
                        \n\
                        ## Rules\n\
                        \n\
                        - PAT-100 (error): Sessions must expire within 24 hours.\n\
                        - PAT-101 (warn): Login attempts are rate-limited.\n";
        let batch = serde_json::json!({
            "default_type": "security",
            "entries": [
                { "kind": "doc", "name": "auth-rules.md", "content": good_doc },
                { "kind": "rule", "rule": {
                    "id": "SEC-CUSTOM-1",
                    "rule_type": "pattern",
                    "statement": "Secrets never land in logs.",
                    "severity": "critical",
                    "confidence": 0.9,
                } },
                { "kind": "doc", "name": "broken.md", "content": "just prose, no fence" },
            ],
        });

        let json = steering_import_json(&core, &batch.to_string()).expect("the batch resolves");
        drop(core); // writes are committed per upsert; the reads below use fresh RO connections
        let rows: Vec<Value> = serde_json::from_str(&json).expect("binding emits valid JSON");
        assert_eq!(rows.len(), 3, "one result per entry, batch order");

        // Entry 0 — the good doc: imported, minting BOTH rule ids in doc order.
        assert_eq!(rows[0]["index"], 0);
        assert_eq!(rows[0]["name"], "auth-rules.md");
        assert_eq!(rows[0]["status"], "imported");
        assert_eq!(rows[0]["ids"], serde_json::json!(["PAT-100", "PAT-101"]));
        assert!(
            rows[0].get("error").is_none(),
            "an imported entry carries no error"
        );

        // Entry 1 — the ready rule: imported under its own id; a rule entry echoes no name.
        assert_eq!(rows[1]["index"], 1);
        assert_eq!(rows[1]["status"], "imported");
        assert_eq!(rows[1]["ids"], serde_json::json!(["SEC-CUSTOM-1"]));
        assert!(rows[1].get("name").is_none(), "rule entries have no name");

        // Entry 2 — the malformed doc: rejected WITH its reason, minting nothing.
        assert_eq!(rows[2]["index"], 2);
        assert_eq!(rows[2]["name"], "broken.md");
        assert_eq!(rows[2]["status"], "rejected");
        let reason = rows[2]["error"]
            .as_str()
            .expect("rejected names its reason");
        assert!(
            reason.contains("frontmatter"),
            "the reason says what was wrong: {reason}"
        );
        assert!(
            rows[2].get("ids").is_none(),
            "a rejected entry mints nothing"
        );

        // The imported rules are LISTABLE with their steering_type: every entry omitted the
        // field, so the batch default_type ("security") rode onto each minted rule.
        let listed = list_conformance_rules_json(&db_path, Some("security"), false).expect("lists");
        let listed: Vec<Value> = serde_json::from_str(&listed).expect("valid JSON");
        let mut ids: Vec<&str> = listed
            .iter()
            .map(|r| r["id"].as_str().expect("rows carry an id"))
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, ["PAT-100", "PAT-101", "SEC-CUSTOM-1"]);
        for row in &listed {
            assert_eq!(
                row["steering_type"], "security",
                "the batch default_type landed on {}",
                row["id"]
            );
        }
        // And the doc-minted rows kept their ingest provenance (`<name>@<blob sha>#<id>` refs).
        let pat = listed
            .iter()
            .find(|r| r["id"] == "PAT-100")
            .expect("PAT-100 listed");
        let doc_ref = pat["provenance"]["ref"]
            .as_str()
            .expect("doc rules carry a ref");
        assert!(
            doc_ref.starts_with("auth-rules.md@") && doc_ref.ends_with("#PAT-100"),
            "provenance ref is the ingest spelling: {doc_ref}"
        );
    }

    /// AC (rulesets binding): [`list_rule_sets_json`] — the exact code path `listRuleSets` runs —
    /// answers `[]` on an unseeded store (crew's wiki meta reads the array LENGTH as
    /// `ruleset_count`, so an empty store must be an honest 0-length array, valid JSON) and, once
    /// AW-13 groupings are registered, one domain-sorted row per RuleSet parent carrying its
    /// `Contains` membership.
    #[test]
    fn list_rule_sets_reports_grouping_rows() {
        hermetic_spool();

        let db_path = temp_store_path("list-rulesets");
        {
            let store = wicked_apps_core::open_store(Some(&db_path)).expect("store opens");
            // Unseeded: an honest empty array, not an error and not `null`.
            drop(store);
            let empty = list_rule_sets_json(&db_path).expect("lists over an unseeded store");
            let rows: Vec<Value> = serde_json::from_str(&empty).expect("valid JSON");
            assert!(rows.is_empty(), "unseeded store reports zero rulesets");

            let mut store = wicked_apps_core::open_store(Some(&db_path)).expect("store reopens");
            for (id, sev) in [
                ("PAT-300", "warn"),
                ("PAT-301", "error"),
                ("POL-300", "critical"),
            ] {
                wicked_governance::register_rule(&mut store, &wire_rule(id, sev))
                    .expect("rule registers");
            }
            wicked_governance::register_rule_sets(
                &mut store,
                &[
                    wicked_governance::RuleSetGrouping {
                        domain: "event-grammar".to_string(),
                        rule_ids: vec!["PAT-300".to_string(), "PAT-301".to_string()],
                    },
                    wicked_governance::RuleSetGrouping {
                        domain: "agent-behavior".to_string(),
                        rule_ids: vec!["POL-300".to_string()],
                    },
                ],
            )
            .expect("groupings register");
        }

        let json = list_rule_sets_json(&db_path).expect("lists rulesets");
        let rows: Vec<Value> = serde_json::from_str(&json).expect("binding emits valid JSON");
        assert_eq!(rows.len(), 2, "one row per RuleSet parent");
        // Domain-sorted, membership resolved to rule ids through the store.
        assert_eq!(rows[0]["domain"], "agent-behavior");
        assert_eq!(rows[0]["rule_count"], 1);
        assert_eq!(rows[0]["rule_ids"], serde_json::json!(["POL-300"]));
        assert_eq!(rows[1]["domain"], "event-grammar");
        assert_eq!(rows[1]["rule_count"], 2);
        assert_eq!(
            rows[1]["rule_ids"],
            serde_json::json!(["PAT-300", "PAT-301"])
        );
    }
}
