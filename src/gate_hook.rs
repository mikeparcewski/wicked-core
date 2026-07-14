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
//!    HYGIENE NOTE (P4b, de-risked): `open_store` opens the SQLite file READ-WRITE and, on open, runs
//!    the WAL pragma + `execute_batch(SCHEMA)`/`migrate_schema` DDL (a wicked-estate-store detail). The
//!    open already sets `PRAGMA busy_timeout=5000` (estate `sqlite.rs`, since PR#47), so a hook racing
//!    the actor's write BLOCKS up to 5s and absorbs the contention — it does NOT fail-close into a
//!    *spurious* DENY (the earlier worry, now known false). The residual is purity, not correctness: a
//!    subprocess that only `select`s/`decide`s/`recall`s should not run schema DDL against a store the
//!    single-writer actor owns. The pure-reader opener (`SQLITE_OPEN_READ_ONLY`, skip DDL) is tracked as
//!    a follow-up (estate `SqliteStore::open_readonly` + an apps-core delegate), NOT a blocker for
//!    governance-in-run. Fails CLOSED throughout: exit 2 = deny ⇒ Claude aborts the call.
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

/// Environment variables the launcher sets to carry the unit's governance `scope`/`phase` to the
/// gate-hook subprocess. Passing them via env (NOT interpolated into the shell-executed hook command)
/// is what keeps caller-controlled data out of the command string — closing the injection / fail-open
/// hole a naive double-quoted argv would open (`$(…)`, backticks, embedded `"`). Claude propagates its
/// environment to hook subprocesses, so the hook still receives them.
pub const GATE_SCOPE_ENV: &str = "WICKED_GATE_SCOPE";
pub const GATE_PHASE_ENV: &str = "WICKED_GATE_PHASE";

/// Environment variable carrying the estate store path to the gate-hook subprocess (the injected command
/// drops `--db`). One exported const so the launcher setter + the bin resolver never drift on the name.
pub const ESTATE_DB_ENV: &str = "WICKED_ESTATE_DB";

/// Body of the `wicked-core gate-hook` subcommand. Returns the process exit code (2 = DENY).
///
/// `scope`/`phase` are resolved by the caller (`bin/wicked-core`) from argv (standalone) ELSE the
/// `WICKED_GATE_SCOPE`/`WICKED_GATE_PHASE` env the launcher sets — pinned to the unit's real
/// `resolve_scope(...)` / `unit-{ord}`. They ride env (NOT the shell hook command) so caller-controlled
/// ids can't inject the command. `db` is the shared estate store, used only to *read* policies (we never
/// write governance/claim/domain data — see the module-level note about the open path).
/// Fails CLOSED (returns 2) if the decisions path is unset, the store can't be opened, or governance
/// can't decide — an un-evaluable tool-call is never silently allowed.
pub fn run_gate_hook(scope: &str, phase: &str, db: Option<&str>) -> i32 {
    if let Some(reason) = store_unavailable(db) {
        eprintln!("wicked-governance: DENY ({reason})");
        return 2;
    }
    let mut raw = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut raw) {
        // An unreadable (e.g. non-UTF-8) tool call is UN-EVALUABLE — fail closed, never allow.
        eprintln!("wicked-governance: DENY (could not read tool call for evaluation: {e})");
        return 2;
    }
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
    // On an INFRA failure below we still exit 2 (the tool IS blocked), but we ALSO best-effort append a
    // synthetic Deny so the block leaves durable evidence — otherwise the fold would see no claim and the
    // run could Complete despite a governance-infra block (council blocker, infra-exit-2 arm).
    let store = match open_store(db.filter(|s| !s.is_empty())) {
        Ok(s) => s,
        Err(e) => {
            append_infra_deny(
                &decisions_path,
                scope,
                phase,
                &format!("store open failed: {e}"),
            );
            eprintln!("wicked-governance: DENY (open store failed: {e})");
            return 2;
        }
    };
    let selected = match select(&store, scope, phase, &context) {
        Ok(s) => s,
        Err(e) => {
            append_infra_deny(
                &decisions_path,
                scope,
                phase,
                &format!("policy select failed: {e}"),
            );
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

/// A fail-closed reason the hook must DENY on rather than proceed, or `None` if the store is usable:
///  - No resolvable store (`--db`/`WICKED_ESTATE_DB` both unset): `open_store(None)` would fall back to a
///    default `.wicked-estate/graph.db` (and may CREATE an empty one), evaluating against ZERO policies —
///    a silent fail-OPEN. A governed hook MUST have the run's store; deny loudly instead.
///  - A `postgres://` spec: governance-in-run is SQLite-only for now (the read-only spec-dispatch opener
///    is core#30); deny loudly instead of silently creating a garbage SQLite file (findings #13/#18).
fn store_unavailable(db: Option<&str>) -> Option<String> {
    match db.filter(|s| !s.is_empty()) {
        None => Some(
            "no estate store resolvable (set --db or WICKED_ESTATE_DB) — refusing to evaluate against \
             a default/empty store (fail-closed)"
                .to_string(),
        ),
        Some(s) if s.starts_with("postgres://") || s.starts_with("postgresql://") => Some(
            "governance-in-run is SQLite-only; the hook cannot open a postgres:// store (core#30)"
                .to_string(),
        ),
        // An in-memory store cannot cross into the hook SUBPROCESS — it would open its OWN empty store
        // (zero policies) and ALLOW everything: the same fail-open the missing-store arm denies. In-run
        // it's already filtered out (in_process_governance returns None), but deny it here too so a
        // standalone `gate-hook --db :memory:` can never silently allow (council [10]).
        Some(":memory:") => Some(
            "an in-memory store cannot carry the run's policies into the hook subprocess (always the \
             empty-store fail-open)"
                .to_string(),
        ),
        Some(_) => None,
    }
}

/// INJECTIVE, filesystem-safe encoding of a raw `run_id` into a single path segment. Escapes every byte
/// outside `[A-Za-z0-9-]` — INCLUDING `_`, the escape sentinel — as `_<hex>`, so distinct run_ids can
/// NEVER collide onto one governance dir. A lossy char-replace (the prior impl) mapped `a:b`, `a_b`, and
/// `a/b` all to `a_b` → they would share one decisions log (cross-run veto contamination) and one
/// settings file (last-writer-wins fail-open) — a bypass an attacker could aim by choosing a session id.
fn encode_run_id(run_id: &str) -> String {
    run_id
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' {
                (b as char).to_string()
            } else {
                format!("_{b:02x}")
            }
        })
        .collect()
}

/// The per-run governance directory (outside any worktree). Cleared on a FRESH (re-)launch of a run id
/// so a prior terminal run's stale decisions can't fail a new run — see the launcher; resume/redrive
/// deliberately do NOT clear it (they continue the same run's log).
pub fn gov_run_dir(run_id: &str) -> std::path::PathBuf {
    std::env::temp_dir()
        .join("wicked-core-gov")
        .join(encode_run_id(run_id))
}

/// The absolute decisions-log path that BOTH the launcher (which sets `WICKED_DECISIONS_PATH` on the
/// wrapped CLI) and the actor-side fold ([`fold_input_denial`]) derive identically from `(run_id,
/// attempt)`. Partitioned by `attempt` so a bumped-attempt RETRY (a human `confirm_gate` Approve on a
/// `HumanConfirmIf(VerdictNotPass)` deny, resume, or redrive) reads a CLEAN slate — a stale prior-attempt
/// Deny can no longer re-fail an approved retry. A pure function of `(run_id, attempt)` (no threaded
/// state to keep in sync), living OUTSIDE any worktree.
pub fn decisions_path_for(run_id: &str, attempt: u32) -> std::path::PathBuf {
    gov_run_dir(run_id)
        .join(format!("attempt-{attempt}"))
        .join("decisions.ndjson")
}

/// Append one serialized [`ConformanceClaim`] line to the absolute decisions NDJSON path, creating the
/// file (and parent dir) if needed. Append-only so concurrent hook processes never clobber. The
/// complete `json + '\n'` line is written in a SINGLE `write_all`: a lone append write of a small buffer
/// is atomic on both POSIX (`O_APPEND`) and Windows (`FILE_APPEND_DATA`), so parallel per-tool-call hook
/// subprocesses cannot interleave a claim (finding #10 — the prior two-syscall `writeln!` split the JSON
/// body from its newline, which could interleave and corrupt a line the drain then dropped).
fn append_decision(path: &Path, claim: &ConformanceClaim) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        create_dir_all_private(parent)?;
    }
    let mut line = serde_json::to_string(claim)?;
    line.push('\n');
    // Serialize concurrent per-tool-call hook subprocesses with a cross-platform advisory lockfile (an
    // atomic `create_new`), so a claim whose canonical JSON exceeds the OS single-append atomicity bound
    // can never interleave with another appender's (DES-OUTGOV-003 §7). Belt-and-suspenders on top of the
    // single `write_all` + the drain/fold's fail-CLOSED handling of any torn line.
    with_append_lock(path, || {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        f.write_all(line.as_bytes())
    })?;
    Ok(())
}

/// Run `write` while holding an exclusive advisory lock on `<log>.lock` (a cross-platform, dep-free
/// `create_new` lockfile). Bounded spin — if a crashed holder left a STALE lock, proceed after the
/// timeout: the single `write_all` is still atomic for a typical claim, and a torn line fails CLOSED in
/// the drain/fold, so the worst case degrades, never a silent allow. The lock is always removed if held.
fn with_append_lock<T>(
    log: &Path,
    write: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let lock = log.with_extension("lock");
    let mut held = false;
    for _ in 0..100 {
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock)
        {
            Ok(_) => {
                held = true;
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Reclaim a STALE lock (a crashed holder never unlinked it): if the lockfile is older
                // than a few seconds, remove it and retry immediately, so the mechanism self-heals
                // instead of degrading to permanently-unlocked for the rest of the attempt (council [11]).
                let stale = std::fs::metadata(&lock)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|age| age.as_secs() >= 5)
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(&lock);
                    continue;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            // Can't create the lockfile (dir gone, permissions) — proceed unlocked rather than block.
            Err(_) => break,
        }
    }
    let r = write();
    if held {
        let _ = std::fs::remove_file(&lock);
    }
    r
}

/// Key of the ARMED sentinel line the launcher writes to a governed unit's decisions log BEFORE the CLI
/// runs. Its PRESENCE proves governance was armed + the log is intact; its ABSENCE for a unit the engine
/// KNOWS is governed means the log was never written or was erased/truncated — the fold then fails CLOSED.
/// This makes evidence ERASURE self-defeating (`rm`/truncate ⇒ marker gone ⇒ DENY) and closes the
/// "governed-but-unevidenced looks clean → Completed" fail-open the council flagged as the blocker. It
/// does NOT close SELECTIVE deletion of only the Deny lines (marker + Allows kept) — that needs
/// un-forgeable claims over the bus/store (issue #35).
const ARMED_MARKER_KEY: &str = "_wicked_gov_armed";

/// `create_dir_all` + restrict the leaf dir to owner-only (0700) on Unix, so another local user on a
/// shared host cannot traverse in to read a run's policy scope/phase, tool-call context, or denial
/// reasons (council [9]). The sensitive settings/decisions files live under this dir, so blocking
/// traversal protects them regardless of individual file mode.
pub(crate) fn create_dir_all_private(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Append the ARMED sentinel for `phase` to the decisions log (under the same advisory lock as claims).
/// Called by the launcher when it arms input governance for a governed unit, BEFORE the CLI runs.
pub fn write_armed_marker(decisions_path: &Path, phase: &str) -> anyhow::Result<()> {
    if let Some(parent) = decisions_path.parent() {
        create_dir_all_private(parent)?;
    }
    let mut line = serde_json::json!({ ARMED_MARKER_KEY: phase }).to_string();
    line.push('\n');
    with_append_lock(decisions_path, || {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(decisions_path)?;
        f.write_all(line.as_bytes())
    })?;
    Ok(())
}

/// Best-effort append of a synthetic Deny when the hook must block a tool-call due to an INFRA failure
/// (store won't open, policy select failed) — so the block leaves durable evidence the fold will see,
/// rather than a silent exit-2 the run could Complete past. Errors are swallowed (already failing closed).
fn append_infra_deny(decisions_path: &str, scope: &str, phase: &str, reason: &str) {
    let claim = ConformanceClaim {
        claim_id: format!("infra-deny:{phase}:{scope}"),
        scope: scope.to_string(),
        phase: phase.to_string(),
        policy_ids: vec![],
        decision: Decision::Deny,
        obligations: vec![reason.to_string()],
        evaluated_context_ref: "sha256:infra".to_string(),
        criteria: format!("governance infra failure: {reason}"),
        evaluator_identity: "wicked-governance-infra".to_string(),
        evaluated_at: EVAL_AT_BASE,
    };
    let _ = append_decision(Path::new(decisions_path), &claim);
}

/// Whether `line` is the armed marker for exactly `phase`.
fn is_armed_marker(line: &str, phase: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| {
            v.get(ARMED_MARKER_KEY)
                .and_then(|x| x.as_str())
                .map(str::to_string)
        })
        .as_deref()
        == Some(phase)
}

/// Fold a governed unit's INPUT-hook decisions into a single deny-dominant denial, for the run engine's
/// existing per-unit gate (DES-OUTGOV-003 §1). Reads the run's decisions log; for each claim that
/// targets THIS unit's `phase`, `conform`s it as durable evidence (idempotent upsert by claim symbol)
/// and, if it is a `Deny`, records the denial (deny dominates — the FIRST Deny wins, the rest still
/// conform). Runs ON the actor (single writer). The returned `Some(reason)` folds into
/// `apply_and_finish_unit`'s `validator_denial` seam, so a denied tool-call drives the unit gate
/// `Rejected` → the run `Failed` through the UNCHANGED completion path — never a second phase resolver.
///
/// FAILS CLOSED on a corrupted claim line (a `{`-prefixed line that will not parse is un-evaluable
/// governance evidence) by returning it AS A DENIAL (deny dominates), NOT an `Err`: the denial rides the
/// normal `validator_denial` path → unit gate `Rejected` → run terminally `Failed`, whereas a propagated
/// `Err` would leave the session wedged non-terminal and re-executed on every restart (finding #3/#8).
/// `Ok(None)` when the log is absent (an ungoverned or not-yet-written attempt) or holds no `Deny` for
/// `phase`.
/// `governed` = the engine KNOWS this unit had input governance armed (a claude unit on a file-backed
/// store). It gates EVIDENCE-INTEGRITY fail-closure: a governed unit whose armed marker is absent (log
/// missing, erased, or truncated) DENIES — closing the council blocker where an agent could erase its own
/// Deny evidence (or the hook could silently never fire) and flip the run from Failed to Completed. An
/// UNGOVERNED unit legitimately has no log, so `governed=false` ⇒ the fold is inert (`Ok(None)`).
pub fn fold_input_denial(
    store: &mut dyn GraphStore,
    run_id: &str,
    attempt: u32,
    phase: &str,
    governed: bool,
) -> anyhow::Result<Option<String>> {
    let path = decisions_path_for(run_id, attempt);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        // No log at all: a GOVERNED unit MUST have its launcher-written armed marker → its absence means
        // the evidence was never written or the whole gov dir was erased → fail CLOSED. An ungoverned
        // unit legitimately has no log.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(governed.then(|| format!(
                "input governance denied {phase} (fail-closed): governed unit produced NO decisions log \
                 (hook never fired or evidence erased)"
            )))
        }
        // A non-NotFound read error (permission / sharing) is un-evaluable governance evidence ⇒ deny
        // (fail closed) via the normal terminal path, never a run-wedging Err.
        Err(e) => {
            return Ok(Some(format!(
                "input governance denied {phase} (fail-closed): could not read decisions log: {e}"
            )))
        }
    };
    let mut denial: Option<String> = None;
    let mut saw_marker = false;
    for line in raw.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue; // blank / non-claim line — not corruption
        }
        if is_armed_marker(line, phase) {
            saw_marker = true;
            continue;
        }
        if line.contains(ARMED_MARKER_KEY) {
            continue; // an armed marker for ANOTHER unit — not a claim, don't treat as corrupt
        }
        let claim: ConformanceClaim = match serde_json::from_str(line) {
            Ok(c) => c,
            // Un-evaluable governance evidence ⇒ deny-dominant (fail closed), routed through the normal
            // terminal path rather than a run-wedging `Err`.
            Err(e) => {
                if denial.is_none() {
                    denial = Some(format!(
                        "input governance denied {phase} (fail-closed): corrupted decision line: {e}"
                    ));
                }
                continue;
            }
        };
        if claim.phase != phase {
            continue; // another unit's claim — folded when that unit finishes
        }
        conform(store, &claim)?;
        if denial.is_none() && claim.decision == Decision::Deny {
            denial = Some(format!(
                "input governance denied a tool-call in {phase} (claim {})",
                claim.claim_id
            ));
        }
    }
    // A GOVERNED unit whose log is PRESENT but has lost its armed marker was truncated/edited → the
    // evidence stream is untrustworthy → fail CLOSED (even if no surviving Deny remains).
    if governed && !saw_marker && denial.is_none() {
        denial = Some(format!(
            "input governance denied {phase} (fail-closed): armed marker missing \
             (decisions log tampered or truncated)"
        ));
    }
    Ok(denial)
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

    // Pass 1: `conform` every claim (durable per-claim evidence — idempotent, order-independent) and
    // GROUP by the governance phase it targets. Grouping is what makes deny DOMINATE: a phase gate is
    // resolved ONCE from the composed verdict, not first-writer-wins across claims. Without this, an
    // Allow drained before a Deny (the common input-hook-then-output-hook file order) would resolve
    // the phase to a TERMINAL Approved, and the reducer would then refuse the Deny (`from_mismatch`)
    // — silently dropping the veto. (BTreeMap → deterministic phase iteration order.)
    let mut by_phase: std::collections::BTreeMap<String, Vec<ConformanceClaim>> =
        std::collections::BTreeMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        // FAIL CLOSED on a corrupted `{`-prefixed line: un-evaluable governance evidence must never be
        // silently skipped into an allow (finding #10). A blank / non-`{` line was already `continue`d.
        let claim: ConformanceClaim = serde_json::from_str(line).map_err(|e| {
            anyhow::anyhow!("hook-decision drain DENY (fail-closed): corrupted claim line: {e}")
        })?;
        conform(store, &claim)?;
        summary.applied += 1;
        by_phase.entry(claim.phase.clone()).or_default().push(claim);
    }

    // Pass 2: resolve each phase's gate ONCE from the deny-dominating verdict
    // (Deny ≻ AllowWithConditions ≻ Allow). Deny wins regardless of the claims' arrival order.
    for (phase_name, claims) in &by_phase {
        let phase_id = format!("{workflow_id}:{phase_name}");
        ensure_phase_at_gate(store, &phase_id, &workflow_id, phase_name)?;
        let verdict = claims
            .iter()
            .find(|c| c.decision == Decision::Deny)
            .or_else(|| {
                claims
                    .iter()
                    .find(|c| c.decision == Decision::AllowWithConditions)
            })
            .unwrap_or(&claims[0]);
        let gate_event_id = format!("hookgate-{}", verdict.claim_id);
        let outcome = apply_gate(store, &phase_id, Some(verdict), &gate_event_id)?;
        // Count a veto only when the Deny actually resolved the gate (never mask a refused transition).
        if verdict.decision == Decision::Deny && outcome.applied {
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
    if let Some(reason) = store_unavailable(db) {
        eprintln!("wicked-governance: DENY ({reason})");
        return 2;
    }
    let mut raw = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut raw) {
        // An unreadable (e.g. non-UTF-8) output is UN-EVALUABLE — fail closed, never allow.
        eprintln!("wicked-governance: DENY (could not read output for evaluation: {e})");
        return 2;
    }
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
/// event). The extracted output text becomes `work` (the canonical evaluated value); the FULL raw
/// input is ALSO carried as `raw` so a policy trigger can never fail to fire on a violation living in
/// a discarded envelope field — extraction narrows the DISPLAY value, never the governed surface
/// (fail-CLOSED direction: `select`/`decide` scan the whole context object, so scanning more is safe).
///
/// KNOWN LIMITATION (inherited, tracked as a follow-up — affects BOTH hooks): `decide`'s triggers
/// match over the CANONICAL JSON of this context (`serde_json::to_string`), where newlines are
/// escaped to `\n`, so a policy trigger authored with a real-newline / `(?m)^…$` line anchor will not
/// match interior lines of multiline output. Fixing it means decoupling the trigger haystack from the
/// attestation fingerprint in `wicked-governance::decide` (keep the canonical bytes for
/// `evaluated_context_ref` / ADR-0003 re-derivability, match against the raw string) — a governance-
/// engine change out of this per-output entry point's scope.
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
        "raw": trimmed,
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

    #[test]
    fn attach_recalled_rules_narrows_by_facet() {
        use wicked_governance::{
            register_rule, ConfSeverity, ConformanceRule, RuleProvenance, RuleQuery, RuleType,
            Targets,
        };
        let mut store = open_store(Some(":memory:")).unwrap();
        let mk = |id: &str, lang: &str| ConformanceRule {
            id: id.into(),
            rule_type: RuleType::Pattern,
            statement: "s".into(),
            severity: ConfSeverity::Warn,
            confidence: 0.5,
            targets: Targets {
                language: Some(lang.into()),
                ..Default::default()
            },
            symbol_ref: None,
            compliance: None,
            provenance: RuleProvenance::default(),
        };
        register_rule(&mut store, &mk("PAT-001", "python")).unwrap();
        register_rule(&mut store, &mk("PAT-002", "rust")).unwrap();

        let mut claim = allow_claim("c1", "review");
        // A FACETED query attaches ONLY the matching rule — proving narrowing (not "attach all").
        attach_recalled_rules(
            &store,
            &RuleQuery {
                language: Some("python".into()),
                ..Default::default()
            },
            &mut claim,
        )
        .unwrap();
        assert_eq!(claim.obligations.len(), 1, "only the python rule matches");
        assert!(claim.obligations[0].contains("PAT-001"));
    }

    #[test]
    fn drain_deny_dominates_when_two_claims_share_a_phase() {
        use wicked_orchestration::{get_phase, PhaseStatus};
        let mut store = open_store(Some(":memory:")).unwrap();
        let dir =
            std::env::temp_dir().join(format!("wicked-core-drain-deny-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("decisions.ndjson");
        let _ = std::fs::remove_file(&path);

        // Allow drained BEFORE Deny — the common input-hook-then-output-hook file order that used to
        // resolve the phase to a TERMINAL Approved and silently drop the later Deny (from_mismatch).
        append_decision(&path, &allow_claim("allow-1", "exec")).unwrap();
        let mut deny = allow_claim("deny-1", "exec");
        deny.decision = Decision::Deny;
        append_decision(&path, &deny).unwrap();

        let summary = apply_hook_decisions(&mut store, "run1", &path).unwrap();
        assert_eq!(
            summary.applied, 2,
            "both claims conformed as durable evidence"
        );
        assert_eq!(
            summary.denied, 1,
            "the phase's Deny verdict resolved the gate"
        );
        let phase = get_phase(&store, "wf-run1:exec").unwrap().unwrap();
        assert_eq!(
            phase.status,
            PhaseStatus::Rejected,
            "deny DOMINATES the same-phase Allow regardless of arrival order"
        );
    }

    #[test]
    fn fold_input_denial_denies_conforms_by_phase_and_fails_closed() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let run_id = format!("foldtest-{}", std::process::id());
        let path = decisions_path_for(&run_id, 0);
        let _ = std::fs::remove_file(&path);

        // Absent log ⇒ None (ungoverned / not-yet-written attempt — the fold is inert).
        assert_eq!(
            fold_input_denial(&mut store, &run_id, 0, "unit-1", false).unwrap(),
            None
        );

        // unit-1: Allow then Deny (deny dominates). unit-2: an Allow that must NOT be folded here.
        append_decision(&path, &allow_claim("a1", "unit-1")).unwrap();
        let mut deny = allow_claim("d1", "unit-1");
        deny.decision = Decision::Deny;
        append_decision(&path, &deny).unwrap();
        append_decision(&path, &allow_claim("a2", "unit-2")).unwrap();

        let denial = fold_input_denial(&mut store, &run_id, 0, "unit-1", false).unwrap();
        assert!(
            denial.as_deref().is_some_and(|d| d.contains("d1")),
            "a Deny for unit-1 surfaces a denial naming the claim: {denial:?}"
        );
        // Durable evidence: unit-1's claims conformed; unit-2's is filtered out (folded by its own unit).
        assert_eq!(count_claims(&store, "a1").unwrap(), 1);
        assert_eq!(count_claims(&store, "d1").unwrap(), 1);
        assert_eq!(
            count_claims(&store, "a2").unwrap(),
            0,
            "another unit's claim is not conformed when folding unit-1"
        );

        // RETRY-POISON FIX: a bumped attempt reads a CLEAN slate — attempt 0's Deny does NOT leak to
        // attempt 1 (so a human `confirm_gate` Approve / resume / redrive is no longer re-failed forever).
        assert_eq!(
            fold_input_denial(&mut store, &run_id, 1, "unit-1", false).unwrap(),
            None,
            "attempt 1 does not inherit attempt 0's Deny"
        );

        // A corrupted `{`-prefixed line ⇒ fail closed AS A DENIAL (routed through the normal terminal
        // path, not a run-wedging Err). Use a fresh attempt so only the corrupt line is present.
        let path2 = decisions_path_for(&run_id, 1);
        let _ = std::fs::remove_file(&path2);
        std::fs::create_dir_all(path2.parent().unwrap()).unwrap();
        std::fs::write(&path2, b"{ not valid json\n").unwrap();
        let corrupt = fold_input_denial(&mut store, &run_id, 1, "unit-1", false).unwrap();
        assert!(
            corrupt
                .as_deref()
                .is_some_and(|d| d.contains("fail-closed")),
            "a corrupted claim line DENIES (fail-closed), not Err: {corrupt:?}"
        );
        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));
    }

    #[test]
    fn governed_unit_evidence_integrity_fails_closed_on_tamper() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let run_id = format!("evtest-{}", std::process::id());
        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));

        // (a) GOVERNED unit, NO log at all (erased gov dir / hook never fired) → DENY (fail closed).
        let d = fold_input_denial(&mut store, &run_id, 0, "unit-1", true).unwrap();
        assert!(
            d.as_deref().is_some_and(|s| s.contains("NO decisions log")),
            "a governed unit with no evidence fails closed: {d:?}"
        );

        // (b) GOVERNED unit, marker ONLY (legitimate zero-tool-call run) → allowed (Ok(None)).
        let path = decisions_path_for(&run_id, 1);
        write_armed_marker(&path, "unit-1").unwrap();
        assert_eq!(
            fold_input_denial(&mut store, &run_id, 1, "unit-1", true).unwrap(),
            None,
            "a governed unit that made no tool-calls (marker only) is NOT denied"
        );

        // (c) GOVERNED unit, marker + a Deny claim → DENY (the real veto).
        let mut deny = allow_claim("ev-d1", "unit-1");
        deny.decision = Decision::Deny;
        append_decision(&path, &deny).unwrap();
        assert!(
            fold_input_denial(&mut store, &run_id, 1, "unit-1", true)
                .unwrap()
                .is_some(),
            "a governed unit with a recorded Deny is denied"
        );

        // (d) GOVERNED unit, claims present but marker ERASED (tampered) → DENY even with no surviving Deny.
        let path2 = decisions_path_for(&run_id, 2);
        append_decision(&path2, &allow_claim("ev-a1", "unit-1")).unwrap(); // an Allow, but NO marker
        let d = fold_input_denial(&mut store, &run_id, 2, "unit-1", true).unwrap();
        assert!(
            d.as_deref()
                .is_some_and(|s| s.contains("armed marker missing")),
            "a governed unit whose armed marker was stripped fails closed: {d:?}"
        );

        // (e) UNGOVERNED unit with no log → inert (Ok(None)) — the fail-closure is governed-only.
        assert_eq!(
            fold_input_denial(&mut store, &run_id, 3, "unit-1", false).unwrap(),
            None,
            "an ungoverned unit is never denied for missing evidence"
        );
        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));
    }

    #[test]
    fn hook_fails_closed_on_postgres_or_missing_store() {
        // postgres:// → deny (SQLite-only for now).
        assert!(store_unavailable(Some("postgres://h/db")).is_some());
        assert!(store_unavailable(Some("postgresql://h/db")).is_some());
        // No resolvable store → deny (never fall back to a default/empty store — fail-OPEN).
        assert!(store_unavailable(None).is_some());
        assert!(store_unavailable(Some("")).is_some());
        // :memory: → deny (a subprocess opens its OWN empty in-memory store → guaranteed allow).
        assert!(store_unavailable(Some(":memory:")).is_some());
        // A real file store is usable.
        assert!(store_unavailable(Some("/tmp/estate.db")).is_none());
        // The hook denies (exit 2) for each fail-open case BEFORE reading stdin — never mis-creates a store.
        assert_eq!(run_gate_hook("s", "unit-1", Some("postgres://h/db")), 2);
        assert_eq!(run_gate_hook("s", "unit-1", None), 2);
        assert_eq!(run_gate_hook("s", "unit-1", Some(":memory:")), 2);
        assert_eq!(
            run_output_gate_hook("s", "unit-1", Some("postgres://h/db")),
            2
        );
        assert_eq!(run_output_gate_hook("s", "unit-1", None), 2);
    }

    #[test]
    fn decisions_path_is_outside_any_worktree_deterministic_injective_and_attempt_scoped() {
        let a = decisions_path_for("run-abc", 0);
        assert_eq!(
            a,
            decisions_path_for("run-abc", 0),
            "deterministic from (run_id, attempt)"
        );
        assert!(
            a.starts_with(std::env::temp_dir()),
            "the decisions log lives under the temp dir, never a target worktree: {a:?}"
        );
        // A path-hostile run_id is escaped — no traversal / nested dirs escape the gov root.
        let p = decisions_path_for("a/../b:c", 0);
        assert!(p.starts_with(std::env::temp_dir()));
        assert!(
            !p.to_string_lossy().contains(".."),
            "no `..` survives encoding: {p:?}"
        );
        // INJECTIVE: distinct run_ids that a lossy replace would collide must map to DISTINCT dirs.
        assert_ne!(
            decisions_path_for("a:b", 0),
            decisions_path_for("a_b", 0),
            "encode_run_id is injective — `a:b` and `a_b` never share a governance dir"
        );
        // ATTEMPT-SCOPED: a bumped attempt reads a different (clean) log.
        assert_ne!(
            decisions_path_for("run-abc", 0),
            decisions_path_for("run-abc", 1),
            "each attempt gets its own decisions log"
        );
    }

    #[test]
    fn drain_fails_closed_on_a_corrupted_claim_line() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let dir = std::env::temp_dir().join(format!("wc-drain-malformed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("decisions.ndjson");
        let _ = std::fs::remove_file(&path);
        append_decision(&path, &allow_claim("ok-1", "exec")).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"{ broken json here\n").unwrap();
        assert!(
            apply_hook_decisions(&mut store, "run-x", &path).is_err(),
            "a corrupted `{{` line fails the drain CLOSED (never a silent skip→allow)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A minimal Allow [`ConformanceClaim`] on `phase` for the drain/recall tests.
    fn allow_claim(id: &str, phase: &str) -> ConformanceClaim {
        ConformanceClaim {
            claim_id: id.to_string(),
            scope: "s".into(),
            phase: phase.to_string(),
            policy_ids: vec![],
            decision: Decision::Allow,
            obligations: vec![],
            evaluated_context_ref: format!("sha256:{id}"),
            criteria: String::new(),
            evaluator_identity: "wicked-governance".into(),
            evaluated_at: EVAL_AT_BASE,
        }
    }
}
