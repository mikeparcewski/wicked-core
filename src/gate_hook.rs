//! GATE-HOOK — the out-of-process governance hook + its single-writer reconciliation (P0).
//!
//! Two halves that together preserve COE's **one-writer** invariant across the wrapped-CLI path:
//!
//!  * [`run_gate_hook`] is the body of the `wicked-core gate-hook` subcommand. Claude's real
//!    PreToolUse hook spawns it once per proposed tool-call; it reads the call on stdin, evaluates it
//!    against governance (`select` + `decide`), and APPENDS the resulting [`ConformanceClaim`] to an
//!    append-only NDJSON file at the absolute `WICKED_DECISIONS_PATH`. **It writes no governance,
//!    claim, or domain data to the store** — the actor remains the sole writer of those. The hook
//!    only *reads* policies (`select`).
//!
//!    ⚠️ HONEST CAVEAT (P4b precondition): `open_store` currently opens the SQLite file READ-WRITE and,
//!    on open, writes the WAL pragma + ensures schema (a wicked-estate-store detail), with **no
//!    `busy_timeout`**. As long as the hook is invoked NOT concurrently with an actor write (true in
//!    P0/P1 — it's driven directly by tests), this is harmless. But once the hook is a real
//!    per-tool-call subprocess racing the actor's writes (P4a/P4b), this open path could take the
//!    write lock and, with `busy_timeout = 0`, fail closed → a *spurious* DENY. **P4b must** open with
//!    `SQLITE_OPEN_READ_ONLY` (skipping pragma/DDL — the actor owns schema) or set a `busy_timeout`.
//!    Until then the domain/claim single-writer guarantee holds, but the hook's open is not yet the
//!    pure reader this module aspires to. Fails CLOSED: exit 2 = deny ⇒ Claude aborts the call.
//!
//!  * [`apply_hook_decisions`] is the actor-side drain. It runs ON the single store-owning actor
//!    thread, reads the NDJSON the hook produced, and is the ONLY place those claims hit the store:
//!    each claim is `conform`ed (durable evidence, idempotent upsert by symbol) and, when it is a
//!    `Deny`, driven through the orchestration gate as a veto on the run's phase. Re-draining is a
//!    no-op (idempotent), so a crash mid-drain is safe to retry.
//!
//! This resolves the historical two-writer hazard: the old `wicked-agent` hook called
//! `conform(&mut store)` from the subprocess (`inject.rs:522`) — a SECOND OS-process writer of the
//! same SQLite file. Here the write moves to the actor; the subprocess only appends a file.
//!
//! Phase ownership (locked here, enforced in P1, see [`crate::workflow`]): the orchestration phase a
//! hook decision targets is opened by the engine, not by the hook. The drain only *resolves the gate*
//! on a phase; in the standalone P0 path it opens the phase if absent purely so the veto is
//! observable, but the execute backend remains the phase opener of record.

use std::io::{Read, Write};
use std::path::Path;

use wicked_apps_core::{
    open_store, ConformanceClaim, Decision, GraphRead, GraphStore, NodeKind, ToNode,
    CONFORMANCE_CLAIM,
};
use wicked_governance::{conform, decide, recall_rules, select, RuleQuery};
use wicked_orchestration::{apply_gate, get_phase, Phase};

use crate::domain::put_node;
use crate::execute::advance_to_gate_running;

/// Fixed evaluation-timestamp base for hook-minted claims — deterministic (no wall clock on the
/// decision path), matching `execute.rs`'s convention so a re-derived claim is byte-identical.
const EVAL_AT_BASE: i64 = 1_750_000_000;

/// Environment variable holding the **absolute** path of the run's append-only decisions log. The
/// worker that launches the wrapped CLI sets it; making it absolute (not cwd-relative) is what fixes
/// the old `inject.rs:547` fragility — Claude may change cwd, but the hook still writes the right
/// file.
pub const DECISIONS_PATH_ENV: &str = "WICKED_DECISIONS_PATH";

/// Body of the `wicked-core gate-hook` subcommand. Returns the process exit code (2 = DENY).
///
/// `scope`/`phase` come from the hook's argv (the launcher bakes them into `.claude/settings.json`);
/// `db` is the shared estate store, used only to *read* policies (we never write governance/claim/
/// domain data — see the module-level honest caveat about the open path + the P4b read-only fix).
/// Fails CLOSED (returns 2) if the decisions path is unset, the store can't be opened, or governance
/// can't decide — an un-evaluable tool-call is never silently allowed.
pub fn run_gate_hook(scope: &str, phase: &str, db: Option<&str>) -> i32 {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let (context, tool) = claude_pretool_context(&raw, scope, phase);

    // Fail closed if the launcher didn't wire an absolute decisions path.
    let decisions_path = match std::env::var(DECISIONS_PATH_ENV) {
        Ok(p) if !p.is_empty() => p,
        _ => {
            eprintln!(
                "wicked-governance: DENY ({DECISIONS_PATH_ENV} unset — cannot record decision)"
            );
            return 2;
        }
    };

    // Read-only use of the store: select reads policies, decide is pure. NO store write here.
    let store = match open_store(db.filter(|s| !s.is_empty())) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wicked-governance: DENY (open store failed: {e})");
            return 2;
        }
    };
    let selected = match select(&store, scope, phase, &context) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wicked-governance: DENY (policy select failed: {e})");
            return 2;
        }
    };
    let claim = decide(&selected, scope, phase, &context, EVAL_AT_BASE);

    // The ONLY side effect of the hook: append the claim to the run's decisions log.
    if let Err(e) = append_decision(Path::new(&decisions_path), &claim) {
        // A failure to record is a governance failure — fail closed rather than allow unrecorded.
        eprintln!("wicked-governance: DENY (could not append decision: {e})");
        return 2;
    }

    match claim.decision {
        Decision::Deny => {
            let t = if tool.is_empty() {
                "tool-call"
            } else {
                tool.as_str()
            };
            eprintln!("wicked-governance: DENY `{t}` (claim {})", claim.claim_id);
            2
        }
        _ => 0,
    }
}

/// Append one serialized [`ConformanceClaim`] line to the absolute decisions NDJSON path, creating
/// the file (and parent dir) if needed. Append-only so concurrent hook processes never clobber.
fn append_decision(path: &Path, claim: &ConformanceClaim) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(claim)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{json}")?;
    Ok(())
}

/// Summary of a single drain pass — what the actor applied from the decisions log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HookDrainSummary {
    /// Claims read + `conform`ed onto the store this pass.
    pub applied: usize,
    /// Of those, how many were `Deny` (drove a gate veto).
    pub denied: usize,
}

/// Drain a run's decisions NDJSON into the store. **Runs on the actor thread — the single writer.**
///
/// For each claim: record it durably (`conform`, idempotent upsert by claim symbol) and resolve the
/// run's governance gate — a `Deny` vetoes the phase through orchestration. Idempotent end-to-end:
/// `conform` upserts by symbol and `apply_gate`'s event id is derived from the claim id, so the
/// reducer dedups a re-drained decision. A missing file is not an error (no decisions yet ⇒ nothing
/// to apply).
pub fn apply_hook_decisions(
    store: &mut dyn GraphStore,
    run_id: &str,
    ndjson_path: &Path,
) -> anyhow::Result<HookDrainSummary> {
    let raw = match std::fs::read_to_string(ndjson_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HookDrainSummary::default())
        }
        Err(e) => return Err(e.into()),
    };

    let workflow_id = format!("wf-{run_id}");
    let mut summary = HookDrainSummary::default();
    for line in raw.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let claim: ConformanceClaim = match serde_json::from_str(line) {
            Ok(c) => c,
            Err(_) => continue, // a malformed line never blocks the rest of the drain
        };

        // 1. durable evidence — the single-writer ingest of the out-of-process decision.
        conform(store, &claim)?;
        summary.applied += 1;

        // 2. resolve the gate on the run's phase. Phase id mirrors the engine's convention
        //    (`{workflow_id}:{phase_name}`); claim.phase is the governance phase the hook evaluated.
        let phase_id = format!("{workflow_id}:{}", claim.phase);
        ensure_phase_at_gate(store, &phase_id, &workflow_id, &claim.phase)?;
        let gate_event_id = format!("hookgate-{}", claim.claim_id);
        apply_gate(store, &phase_id, Some(&claim), &gate_event_id)?;

        if claim.decision == Decision::Deny {
            summary.denied += 1;
        }
    }
    Ok(summary)
}

/// Ensure `phase_id` exists and is at `GateRunning` so a gate can resolve on it. If absent, open it
/// and walk it to the gate; if already opened (the run engine owns it in P1+), leave it as is.
/// Idempotent: re-running never illegally re-transitions an already-resolved phase.
fn ensure_phase_at_gate(
    store: &mut dyn GraphStore,
    phase_id: &str,
    workflow_id: &str,
    phase_name: &str,
) -> anyhow::Result<()> {
    if get_phase(store, phase_id)?.is_none() {
        let phase = Phase::open(phase_id, workflow_id, phase_name);
        put_node(store, phase.to_node())?;
        advance_to_gate_running(store, phase_id)?;
    }
    Ok(())
}

/// Count persisted conformance-claim nodes carrying `claim_id` — test/diagnostic helper proving the
/// drain is idempotent (an upsert-by-symbol can only ever yield one).
pub fn count_claims(store: &dyn GraphRead, claim_id: &str) -> anyhow::Result<usize> {
    let query = wicked_estate_core::SymbolQuery {
        kinds: vec![NodeKind::Other(CONFORMANCE_CLAIM.to_string())],
        ..Default::default()
    };
    // The claim node's metadata IS the serialized claim; read `claim_id` straight off it (no
    // FromNode impl exists for ConformanceClaim). Upsert-by-symbol means this can only ever be ≤1.
    Ok(store
        .find_symbols(&query)?
        .iter()
        .filter(|n| n.metadata.get("claim_id").and_then(|v| v.as_str()) == Some(claim_id))
        .count())
}

/// Parse Claude's PreToolUse event `{ "tool_name", "tool_input": { … } }` into the governance
/// evaluation context (ported from `wicked-agent/src/inject.rs`). `tool_input` keys vary by tool:
/// `Bash{command}`, `Write{file_path,content}`, `Edit{file_path,new_string}`, `Read{file_path}`, …
fn claude_pretool_context(raw: &str, scope: &str, phase: &str) -> (serde_json::Value, String) {
    let v: serde_json::Value = serde_json::from_str(raw.trim()).unwrap_or(serde_json::Value::Null);
    let tool = v
        .get("tool_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let input = v
        .get("tool_input")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let get = |k: &str| {
        input
            .get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let command = get("command");
    let path = get("file_path")
        .or_else(|| get("path"))
        .or_else(|| get("notebook_path"));
    let content = get("content")
        .or_else(|| get("new_string"))
        .or_else(|| get("new_str"));
    let work = command
        .clone()
        .or_else(|| content.clone())
        .or_else(|| path.clone())
        .unwrap_or_else(|| tool.clone());
    let context = serde_json::json!({
        "phase": phase,
        "scope": scope,
        "tool": tool,
        "command": command,
        "path": path,
        "content": content,
        "args": input,
        "work": work,
    });
    (context, tool)
}

/// Environment variables the launcher may set to scope OUTPUT-governance recall to the produced
/// artifact's facets. Unset ⇒ a wildcard for that facet (every conformance rule matches — the
/// fail-toward-surfacing default; set them to narrow recall to the artifact's language/layer/framework).
pub const OUTPUT_LANGUAGE_ENV: &str = "WICKED_OUTPUT_LANGUAGE";
pub const OUTPUT_LAYER_ENV: &str = "WICKED_OUTPUT_LAYER";
pub const OUTPUT_FRAMEWORK_ENV: &str = "WICKED_OUTPUT_FRAMEWORK";

/// Body of the `wicked-core output-gate-hook` subcommand — the PER-OUTPUT governance guardrail
/// (DES-OUTGOV-001 PR-C, M2/M6). Where [`run_gate_hook`] governs a proposed tool INPUT, this governs
/// the generated OUTPUT text:
///  1. it evaluates the output through the SAME deterministic `select`+`decide` engine (a policy
///     whose trigger matches the output DENIES it — hard→deny; an allow-with-conditions rides
///     obligations — soft→advise), then
///  2. RECALLS the conformance rules applicable to the output's facets and attaches them as
///     obligations (the applicable ruleset the output must conform to — M6/M7 recall→gate wiring).
///
/// The claim is appended to the SAME decisions NDJSON as the input hook, so [`apply_hook_decisions`]
/// composes its verdict at the phase gate (deny dominates via the reducer) — there is NO separate
/// compose path (M1).
///
/// **Honest seam:** whether the output *violates* a pattern conformance rule is a SEMANTIC check (the
/// rule carries no regex) — that verification is the downstream per-turn checker's job (garden). This
/// entry point is the DETERMINISTIC half: policy-over-output + recall wiring. Fails CLOSED (exit 2)
/// exactly like the input hook — an un-evaluable or un-recordable output is never silently allowed.
pub fn run_output_gate_hook(scope: &str, phase: &str, db: Option<&str>) -> i32 {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let context = claude_output_context(&raw, scope, phase);

    let decisions_path = match std::env::var(DECISIONS_PATH_ENV) {
        Ok(p) if !p.is_empty() => p,
        _ => {
            eprintln!(
                "wicked-governance: DENY ({DECISIONS_PATH_ENV} unset — cannot record output decision)"
            );
            return 2;
        }
    };
    let store = match open_store(db.filter(|s| !s.is_empty())) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wicked-governance: DENY (open store failed: {e})");
            return 2;
        }
    };
    let selected = match select(&store, scope, phase, &context) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wicked-governance: DENY (policy select failed: {e})");
            return 2;
        }
    };
    let mut claim = decide(&selected, scope, phase, &context, EVAL_AT_BASE);

    // Wire recall INTO the output gate (M6/M7): the conformance rules applicable to the output's
    // facets become obligations on the claim. A recall failure is a governance failure (fail
    // closed) — never silently drop the ruleset.
    if let Err(e) = attach_recalled_rules(&store, &output_rule_query(), &mut claim) {
        eprintln!("wicked-governance: DENY (conformance-rule recall failed: {e})");
        return 2;
    }

    if let Err(e) = append_decision(Path::new(&decisions_path), &claim) {
        eprintln!("wicked-governance: DENY (could not append output decision: {e})");
        return 2;
    }

    match claim.decision {
        Decision::Deny => {
            eprintln!("wicked-governance: DENY output (claim {})", claim.claim_id);
            2
        }
        _ => 0,
    }
}

/// Parse the produced OUTPUT into the governance evaluation context. Accepts the wrapped CLI's raw
/// stdout, OR a JSON envelope (`{"output"|"stdout"|"text"|"content": "…"}` — e.g. a Stop/SubagentStop
/// event). The output text becomes `work`, the field `select`/`decide` evaluate over.
fn claude_output_context(raw: &str, scope: &str, phase: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    let output_text = serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|v| {
            ["output", "stdout", "text", "content"]
                .iter()
                .find_map(|k| {
                    v.get(*k)
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
        })
        .unwrap_or_else(|| trimmed.to_string());
    serde_json::json!({
        "phase": phase,
        "scope": scope,
        "output": output_text,
        "work": output_text,
    })
}

/// Attach the conformance rules applicable to `query` as obligations on `claim` — the M6/M7
/// recall→gate wiring. Each obligation is `conform:<Severity>:<id>:<statement>` so a downstream
/// checker/human sees the applicable ruleset (and its severity) that the output must conform to. A
/// recall error propagates so the caller can fail closed.
fn attach_recalled_rules(
    store: &dyn GraphRead,
    query: &RuleQuery,
    claim: &mut ConformanceClaim,
) -> anyhow::Result<()> {
    for r in recall_rules(store, query)? {
        claim
            .obligations
            .push(format!("conform:{:?}:{}:{}", r.severity, r.id, r.statement));
    }
    Ok(())
}

/// Build the conformance-rule recall query from the optional output-facet env vars (unset ⇒ wildcard).
fn output_rule_query() -> RuleQuery {
    let env = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    RuleQuery {
        language: env(OUTPUT_LANGUAGE_ENV),
        layer: env(OUTPUT_LAYER_ENV),
        framework: env(OUTPUT_FRAMEWORK_ENV),
        severity: None,
        rule_type: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretool_context_extracts_bash_command_into_work() {
        let raw = r#"{"tool_name":"Bash","tool_input":{"command":"echo DENYME"}}"#;
        let (ctx, tool) = claude_pretool_context(raw, "scope", "exec");
        assert_eq!(tool, "Bash");
        assert_eq!(ctx["work"], "echo DENYME");
        assert_eq!(ctx["phase"], "exec");
    }

    #[test]
    fn append_decision_is_append_only() {
        let dir = std::env::temp_dir().join("wicked-core-gatehook-append");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("decisions.ndjson");
        let _ = std::fs::remove_file(&path);
        let claim = |id: &str| ConformanceClaim {
            claim_id: id.to_string(),
            scope: "s".into(),
            phase: "exec".into(),
            policy_ids: vec![],
            decision: Decision::Allow,
            obligations: vec![],
            evaluated_context_ref: "sha256:x".into(),
            criteria: String::new(),
            evaluator_identity: "wicked-governance".into(),
            evaluated_at: EVAL_AT_BASE,
        };
        append_decision(&path, &claim("a")).unwrap();
        append_decision(&path, &claim("b")).unwrap();
        let lines: Vec<_> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(lines.len(), 2, "append-only: both claims present");
    }

    #[test]
    fn output_context_extracts_raw_and_enveloped_output() {
        // Raw stdout → work.
        let ctx = claude_output_context("fn main() { unsafe {} }", "s", "review");
        assert_eq!(ctx["work"], "fn main() { unsafe {} }");
        assert_eq!(ctx["phase"], "review");
        // JSON envelope (Stop/SubagentStop-style) → the `output` field becomes work.
        let ctx = claude_output_context(r#"{"output":"SELECT * FROM users"}"#, "s", "review");
        assert_eq!(ctx["work"], "SELECT * FROM users");
    }

    #[test]
    fn attach_recalled_rules_adds_applicable_rules_as_obligations() {
        use wicked_governance::{
            register_rule, ConfSeverity, ConformanceRule, RuleProvenance, RuleQuery, RuleType,
            Targets,
        };
        let mut store = open_store(Some(":memory:")).unwrap();
        register_rule(
            &mut store,
            &ConformanceRule {
                id: "POL-001".into(),
                rule_type: RuleType::Policy,
                statement: "no plaintext secrets in output".into(),
                severity: ConfSeverity::Critical,
                confidence: 0.9,
                targets: Targets::default(),
                symbol_ref: None,
                compliance: None,
                provenance: RuleProvenance::default(),
            },
        )
        .unwrap();

        let mut claim = ConformanceClaim {
            claim_id: "c1".into(),
            scope: "s".into(),
            phase: "review".into(),
            policy_ids: vec![],
            decision: Decision::Allow,
            obligations: vec![],
            evaluated_context_ref: "sha256:x".into(),
            criteria: String::new(),
            evaluator_identity: "wicked-governance".into(),
            evaluated_at: EVAL_AT_BASE,
        };
        // A wildcard query (no facets) recalls the applicable rule and attaches it as an obligation.
        attach_recalled_rules(&store, &RuleQuery::default(), &mut claim).unwrap();
        assert_eq!(
            claim.obligations.len(),
            1,
            "the applicable rule is wired in as an obligation"
        );
        assert!(
            claim.obligations[0].contains("Critical") && claim.obligations[0].contains("POL-001"),
            "obligation carries severity + rule id: {:?}",
            claim.obligations[0]
        );
    }
}
