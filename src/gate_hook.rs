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
//!    Now uses `open_store_ro` (P4b, wicked-core#36 + wicked-estate#63): the hook opens the SQLite
//!    file with `SQLITE_OPEN_READONLY` — no WAL pragma, no `SCHEMA`/`migrate_schema` DDL — so the
//!    hook subprocess never races the single-writer actor on schema or WAL operations. The read is
//!    tuning-only (busy_timeout + cache). Fails CLOSED throughout: exit 2 = deny ⇒ Claude aborts.
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
    open_store_ro, ConformanceClaim, Decision, GraphRead, GraphStore, NodeKind, ToNode,
    CONFORMANCE_CLAIM,
};
use wicked_governance::{conform, decide, recall_rules, select_any, RuleQuery};
use wicked_orchestration::{apply_gate, get_phase, Phase};

use crate::domain::put_node;
use crate::execute::advance_to_gate_running;

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

/// The WORKFLOW phase id backing the unit (e.g. `review`), carried alongside [`GATE_PHASE_ENV`]'s
/// synthetic `unit-{ord}`. Policy `select` matches either, so an operator's `applies_to: ["review"]`
/// fires in the subprocess hook exactly as it does in-process (FINDING-021). Unset/empty ⇒ the
/// synthetic token alone, which is the pre-fix behaviour.
pub const GATE_PHASE_ID_ENV: &str = "WICKED_GATE_PHASE_ID";

/// Environment variable carrying the operational store path to the gate-hook subprocess (the injected
/// command drops `--db`). One exported const so the launcher setter + the bin resolver never drift on
/// the name.
///
/// DELIBERATELY NOT `WICKED_ESTATE_DB` (FINDING-067). The hook is a grandchild of the worker CLI, so
/// the only way to reach it is through the worker's own environment — which means every tool the
/// worker spawns sees this variable too. Under the old name, `wicked-estate`, `wicked-estate-mcp` and
/// `wicked-core` all resolve `--db` ELSE `$WICKED_ESTATE_DB`, so a worker running a bare
/// `wicked-estate index .` in a Bash call silently pointed the indexer at the platform's operational
/// store and its delete-sweep took all 833 operational nodes with it. A name no estate tool consumes
/// keeps the hook working while removing the accident: the worker's tools now resolve their own
/// default instead of inheriting the engine's. The launcher additionally `env_remove`s the old name so
/// an inherited value cannot re-open the channel.
pub const GATE_DB_ENV: &str = "WICKED_GATE_DB";

/// The variable [`GATE_DB_ENV`] replaced — still named here because the launcher must actively STRIP it
/// from a worker's environment, not merely stop setting it. Every estate binary reads it as the `--db`
/// fallback, so a daemon started with it exported would hand every worker the operator's store without
/// the engine ever setting a thing.
pub const ESTATE_DB_ENV: &str = "WICKED_ESTATE_DB";

/// The store a VALIDATOR script may reach, carried under its own name.
///
/// Validator scripts used to be handed `WICKED_ESTATE_DB` — the OPERATIONAL store — so that
/// `COVERAGE_SCRIPT` could shell out to `wicked-core coverage`. Those scripts are approval-gated,
/// denylist-screened and run with a minimal env, which is a genuinely different threat model from a
/// worker CLI running agent-authored Bash. But all three are AUTHORIZATION controls: none of them
/// constrain what an approved script does with the handle once it holds it, and FINDING-067 showed
/// the failure needs no malice — just a tool that defaults to `$WICKED_ESTATE_DB`. An approved
/// script shelling out to any estate tool without `--db` inherits the operational store exactly as
/// the worker did (core#166).
///
/// Same remedy the hook got in #165: a dedicated name, so the operational one can be removed from
/// the environment rather than merely not-used.
pub const COVERAGE_DB_ENV: &str = "WICKED_COVERAGE_DB";

/// The name a validator script uses to reach the engine's own CLI: `${WICKED_CORE_EXE:-wicked-core}`.
///
/// Named here rather than spelled at each site because three places have to agree on it — the
/// injection in [`crate::validator`], the shipped `COVERAGE_SCRIPT` in [`crate::domain_extraction`],
/// and the diagnostic that fires when no binary can be located (FINDING-093). It is also read as an
/// operator override by `resolve_wicked_core_exe_opt`.
pub const WICKED_CORE_EXE_ENV: &str = "WICKED_CORE_EXE";

/// Absolute roots this unit may WRITE inside, `PATH`-separator joined. In practice its worktree.
///
/// The boundary travels by env for the same reason [`DECISIONS_PATH_ENV`] does: the hook runs as a
/// subprocess of an agent that may have changed directory, so `cwd` is not a trustworthy statement
/// of where the unit was scoped.
pub const WRITE_ROOTS_ENV: &str = "WICKED_WRITE_ROOTS";

/// Absolute roots this unit may READ but not write, `PATH`-separator joined. Evidence-derived (skill
/// definitions, language runtimes, package caches) — see [`crate::path_policy`]. A boundary that
/// breaks every real run gets switched off, and one that is off is worse than none because it is
/// believed.
pub const READ_ROOTS_ENV: &str = "WICKED_READ_ROOTS";

/// The unit's filesystem boundary, or `None` when the launcher armed no roots.
///
/// `None` is NOT "allow everything" — it is "no boundary was configured", which is the honest state
/// for a standalone or ungoverned invocation. [`crate::path_policy::check`] itself allows nothing
/// against an empty root set, so the two cases stay distinguishable rather than collapsing into a
/// silent fail-open.
fn allowed_roots_from_env() -> Option<crate::path_policy::AllowedRoots> {
    // `var_os`, NOT `var`: a non-UTF-8 worktree path comes back as `VarError::NotUnicode` from
    // `var`, which would make the boundary look UNCONFIGURED and apply to nothing — a fail-OPEN in
    // the one control that must fail closed. `execute_wrapped` sets this from an `OsStr`, so the
    // round trip has to stay OsString-clean end to end (review).
    let split = |k: &str| -> Vec<std::path::PathBuf> {
        std::env::var_os(k)
            .map(|v| {
                std::env::split_paths(&v)
                    .filter(|p| !p.as_os_str().is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };
    let write = split(WRITE_ROOTS_ENV);
    let read = split(READ_ROOTS_ENV);
    (!write.is_empty() || !read.is_empty())
        .then_some(crate::path_policy::AllowedRoots { write, read })
}

/// Tools whose call WRITES through its path argument. Anything else carrying a path is checked as a
/// read, which is the conservative direction: mislabelling a write as a read would test it against
/// the wider root set and let it through.
const WRITE_TOOLS: [&str; 3] = ["Write", "Edit", "NotebookEdit"];

/// Refuse a path-bearing tool call that reaches outside the unit's boundary (FINDING-045/098).
///
/// The motivating case is not a generic escape. A governed worker in the campaign located
/// `~/.config/wicked-core/workflows/domain-extraction.json` — the pin binding its OWN gate — and
/// began authoring a replacement, including vaulting and approving it. Nothing objected; an
/// unrelated network failure stopped it. `evaluator != creator` is this platform's headline
/// structural claim, and it was enforced by a file the creator could rewrite.
///
/// Returns `(reason, fatal)`, or `None` when the call is inside the boundary, carries nothing
/// path-shaped, or no boundary was configured. `fatal` drives whether the deny ABORTS the unit (a
/// write/escape) or is ADVISORY (blocked but the worker continues) — see [`append_boundary_deny`] /
/// core#219. A blocked READ is always advisory; a blocked WRITE is fatal EXCEPT into the worker's
/// own Claude Code state tree (`~/.claude/**`) — see the carve-out below (core#235).
fn boundary_denial(context: &serde_json::Value, tool: &str) -> Option<(String, bool)> {
    let roots = allowed_roots_from_env()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    boundary_denial_with(&roots, &cwd, home.as_deref(), context, tool)
}

/// The unit's filesystem boundary as EXPLICIT state (core#260) — for the carrier that evaluates
/// IN-PROCESS (the ACP permission bridge), where env vars would read the DAEMON's environment
/// (never armed → no boundary) and `current_dir()` the daemon's cwd (wrong base for resolving a
/// tool call's relative paths). The wrapped path's hook subprocess keeps the env carrier:
/// [`boundary_denial`] resolves env + process cwd + `$HOME` and calls the same pure check.
pub(crate) struct BoundaryCtx {
    pub roots: crate::path_policy::AllowedRoots,
    /// The unit's working directory — the base for resolving relative tool-call paths.
    pub cwd: std::path::PathBuf,
    /// The `$HOME` used for `~` expansion and the `~/.claude` advisory carve-out — captured at
    /// gate construction so the in-process carrier judges with the SAME home the worker's own
    /// subprocess would inherit, not whatever the evaluating thread happens to see (Copilot).
    pub home: Option<std::path::PathBuf>,
}

/// The pure boundary judgement both carriers share: roots, cwd AND home are PARAMETERS, never
/// ambient process state, so the wrapped subprocess (env-armed) and the in-process ACP bridge
/// (context-armed, core#260) cannot diverge on what "outside the boundary" means.
fn boundary_denial_with(
    roots: &crate::path_policy::AllowedRoots,
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    context: &serde_json::Value,
    tool: &str,
) -> Option<(String, bool)> {
    // Path-bearing tools (Write/Edit/NotebookEdit/Read): the direct path check.
    if let Some(path) = context
        .get("path")
        .and_then(serde_json::Value::as_str)
        .filter(|p| !p.is_empty())
    {
        let is_write = WRITE_TOOLS.contains(&tool);
        if let Err(d) = crate::path_policy::check(path, roots, is_write, cwd, home) {
            // A blocked WRITE is unit-FATAL — EXCEPT into the worker's OWN Claude Code state tree
            // (`~/.claude/**`, e.g. `~/.claude/projects/<slug>/memory/*.md`). A governed claude
            // worker routinely writes its own project-memory there; the write is STILL blocked
            // (exit 2, nothing lands outside the worktree), but a benign agent-memory write must
            // not ABORT the run (core#235 — infigraph died here). Downgrade it to advisory, exactly
            // as a blocked read is (core#219). The carve-out is scoped to `~/.claude`: the gate pin
            // (`~/.config/wicked-core/**`) and the operational store are NOT under it and stay
            // fatal, so the FINDING-098 pin-rewrite escape is untouched. Reads are already advisory,
            // so this relaxes ONLY writes, and ONLY into the agent's own state dir.
            let fatal = is_write
                && !home.is_some_and(|h| {
                    crate::path_policy::resolved_is_within(&d.resolved, &h.join(".claude"))
                });
            return Some((d.to_string(), fatal));
        }
    }

    // Bash: inspect the command for WRITE targets that leave the boundary (FINDING-045). A path
    // gate is blind to `Bash{echo x > ~/outside}` / cp / mv / dd — the command carries no `path`, so
    // the check above never sees it, and a shell write out of the worktree was permitted. This is a
    // WRITE escape → unit-fatal (is_write = true). DEFENSE-IN-DEPTH, not a hermetic sandbox: the
    // shell is Turing-complete, so a determined escape (via `$(...)`, a variable, `base64|sh`) can
    // still evade a scan of the literal command — the honest containment guarantee remains OS-level,
    // which this codebase does not yet have. This closes the DIRECT, common escapes the finding names.
    if tool == "Bash" {
        if let Some(command) = context.get("command").and_then(serde_json::Value::as_str) {
            for target in bash_write_targets(command) {
                if let Err(d) = crate::path_policy::check(&target, roots, true, cwd, home) {
                    return Some((format!("Bash write leaves the unit boundary: {d}"), true));
                }
            }
        }
    }

    None
}

/// Tokenize a Bash command on whitespace AND the control operators `;`, `(`, `)` — but ONLY when those
/// operators are UNQUOTED and UNESCAPED, i.e. acting as operators rather than as literal path bytes.
///
/// Whitespace-only tokenizing captured `> /dev/null; next` as the target `/dev/null;` (trailing `;`
/// glued), which is not a safe sink, so `is_safe_write_sink` missed it and the governed PageIndex
/// domain-graph unit was wrongly DENIED (run 4c63ba17). Splitting an operator `;` off fixes that and
/// keeps a glued second redirect (`>/dev/null;>/outside`) visible as its own token, so real escapes
/// still surface.
///
/// QUOTING/ESCAPING is why this is a real lexer and not a naive `char`-split: in `> foo\;../../etc/evil`
/// the `\;` is a LITERAL `;`, so the runtime redirect target is the single relative path
/// `foo;../../etc/evil` — which `../..` resolves to OUTSIDE the worktree. A naive split at that `;`
/// would truncate the target to `foo\` (resolves INSIDE) and silently drop the traversal, WEAKENING the
/// boundary versus the old whole-token tokenizer (Copilot review on #228). So a `\`-escaped operator and
/// any operator inside `'…'`/`"…"` are kept verbatim in the current token — split only the bare ones.
fn shell_tokens(command: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = command.chars();
    let mut in_single = false;
    let mut in_double = false;
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if !cur.is_empty() {
            out.push(std::mem::take(cur));
        }
    };
    while let Some(ch) = chars.next() {
        if in_single {
            // Single quotes are fully literal — not even `\` escapes; only a closing `'` ends them.
            cur.push(ch);
            if ch == '\'' {
                in_single = false;
            }
            continue;
        }
        if ch == '\\' {
            // Backslash escapes the next char: keep BOTH verbatim so an escaped `;`/`(`/`)` stays a
            // literal path byte, exactly as the pre-split whole-token tokenizer delivered it.
            cur.push(ch);
            if let Some(next) = chars.next() {
                cur.push(next);
            }
            continue;
        }
        if in_double {
            // Inside `"…"`, `;`/`(`/`)` are literal; only a closing `"` (handled here) ends the string.
            cur.push(ch);
            if ch == '"' {
                in_double = false;
            }
            continue;
        }
        match ch {
            '\'' => {
                in_single = true;
                cur.push(ch);
            }
            '"' => {
                in_double = true;
                cur.push(ch);
            }
            c if c.is_whitespace() => flush(&mut cur, &mut out),
            ';' | '(' | ')' => {
                flush(&mut cur, &mut out);
                out.push(ch.to_string());
            }
            _ => cur.push(ch),
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// Best-effort extraction of the filesystem WRITE targets from a Bash command line (FINDING-045).
/// Covers the direct escapes: `>`/`>>`/`N>` redirects (spaced or glued), `tee [-a] FILE...`, and the
/// destination of `cp`/`mv`/`install` (last non-flag arg) and `dd of=FILE`. Deliberately NOT a shell
/// parser — see the caller's note on why this is defense-in-depth rather than a sandbox.
fn bash_write_targets(command: &str) -> Vec<String> {
    let owned = shell_tokens(command);
    let toks: Vec<&str> = owned.iter().map(String::as_str).collect();
    let mut targets: Vec<String> = Vec::new();

    // Redirection targets. `redirect_glob(tok)` returns Some(glued-filename-or-empty) for a write
    // redirect operator; an empty string means the filename is the NEXT token.
    let mut i = 0;
    while i < toks.len() {
        if let Some(glued) = redirect_glob(toks[i]) {
            if !glued.is_empty() {
                targets.push(glued.to_string());
            } else if i + 1 < toks.len() {
                targets.push(toks[i + 1].to_string());
                i += 1;
            }
        }
        i += 1;
    }

    // Command-shaped destinations. Split into pipeline/sequence SEGMENTS on shell separators so a
    // write command after a pipe (`echo x | tee FILE`) or `;`/`&&` is checked as its own program —
    // not missed because the first word of the whole line was something else.
    const SEPS: [&str; 8] = ["|", "||", "&&", ";", "&", "|&", "(", ")"];
    let mut segments: Vec<Vec<&str>> = Vec::new();
    let mut seg: Vec<&str> = Vec::new();
    for &t in &toks {
        if SEPS.contains(&t) {
            if !seg.is_empty() {
                segments.push(std::mem::take(&mut seg));
            }
        } else if redirect_glob(t).is_none() {
            seg.push(t); // drop redirect operators/targets — handled above
        }
    }
    if !seg.is_empty() {
        segments.push(seg);
    }
    for words in &segments {
        let Some(prog) = words.first() else { continue };
        let base = prog.rsplit(['/', '\\']).next().unwrap_or(prog);
        match base {
            "cp" | "mv" | "install" => {
                if let Some(dest) = words[1..].iter().rev().find(|w| !w.starts_with('-')) {
                    targets.push((*dest).to_string());
                }
            }
            "tee" => {
                for w in &words[1..] {
                    if !w.starts_with('-') {
                        targets.push((*w).to_string());
                    }
                }
            }
            "dd" => {
                for w in &words[1..] {
                    if let Some(f) = w.strip_prefix("of=") {
                        targets.push(f.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    // Drop standard shell write SINKS — writing to them discards or streams bytes, it does not place
    // a file outside the worktree, so they are not escapes. `> /dev/null` is in ~every real command
    // (the governed PageIndex pass failed on an `analyze` unit's `… > /dev/null` before this — a false
    // positive that would fail essentially every workflow). FINDING-045 is a fence against files
    // leaving the worktree, not a ban on discarding output.
    targets.retain(|t| !is_safe_write_sink(t));
    targets
}

/// A standard character-device write sink (not a filesystem location that can hold an escaped file).
/// `> /dev/null` / `2>/dev/stderr` / `>/dev/fd/3` are ordinary output plumbing, never an escape.
fn is_safe_write_sink(target: &str) -> bool {
    matches!(
        target,
        "/dev/null" | "/dev/stdout" | "/dev/stderr" | "/dev/tty" | "/dev/zero"
    ) || target.starts_with("/dev/fd/")
}

/// If `tok` is a WRITE-redirect operator to a FILE (`>`, `>>`, `>|`, `N>`, `&>`, optionally glued to a
/// filename), return the glued filename (`""` when the filename is the next token). `None` for a
/// non-redirect token, a READ redirect (`<`), or an fd DUPLICATION (`2>&1`, `>&2`) — the latter
/// redirects a descriptor to another descriptor, it writes no file, so it is not a boundary target.
fn redirect_glob(tok: &str) -> Option<&str> {
    let t = tok.trim_start_matches(|c: char| c.is_ascii_digit());
    let t = t.strip_prefix('&').unwrap_or(t); // `&>` = redirect stdout+stderr to a file
                                              // `>>` must be tried before `>` (the latter is a prefix of the former).
    let rest = t.strip_prefix(">>").or_else(|| t.strip_prefix('>'))?;
    let rest = rest.trim_start_matches('|');
    // `2>&1` / `>&2`: after the operator the target is `&N` — a descriptor dup, not a file.
    if rest.starts_with('&') {
        return None;
    }
    Some(rest)
}

/// Body of the `wicked-core gate-hook` subcommand. Returns the process exit code (2 = DENY).
///
/// `scope`/`phase` are resolved by the caller (`bin/wicked-core`) from argv (standalone) ELSE the
/// `WICKED_GATE_SCOPE`/`WICKED_GATE_PHASE` env the launcher sets — pinned to the unit's real
/// `resolve_scope(...)` / `unit-{ord}`. They ride env (NOT the shell hook command) so caller-controlled
/// ids can't inject the command. `phase_alias` is the workflow phase id ([`GATE_PHASE_ID_ENV`]) and
/// widens policy selection only — the recorded `claim.phase` stays `phase`. `db` is the shared estate
/// store, used only to *read* policies (we never write governance/claim/domain data — see the
/// module-level note about the open path).
/// Fails CLOSED (returns 2) if the decisions path is unset, the store can't be opened, or governance
/// can't decide — an un-evaluable tool-call is never silently allowed.
pub fn run_gate_hook(scope: &str, phase: &str, phase_alias: Option<&str>, db: Option<&str>) -> i32 {
    // A store-unavailable DENY leaves no synthetic claim (there may be no resolvable decisions path yet),
    // unlike the store-open/select infra failures below. That is fine: in a GOVERNED run the launcher only
    // ever arms a file-backed store (`in_process_governance` filters `:memory:`/`postgres://`), so this
    // arm is unreachable in-run — it only fires for a mis-invoked STANDALONE `gate-hook`, where no fold
    // consumes the log. So there is no in-run audit hole (Copilot).
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

    // Everything from here on is CARRIER-INDEPENDENT: the sentinel, the policy evaluation, the
    // durable claim, and the allow/deny answer. Only the step above — turning a wire payload into
    // `(context, tool)` — differs between carriers. Split out so the ACP path can enforce the SAME
    // policy and write the SAME audit trail instead of running ungoverned (FINDING-062).
    evaluate_tool_call(
        scope,
        phase,
        phase_alias,
        db,
        &decisions_path,
        &context,
        &tool,
        // Hook-subprocess carrier: the launcher armed the boundary on OUR env (core#260).
        None,
    )
}

/// Evaluate one tool call against the run's policies, record it durably, and answer allow/deny.
///
/// Returns the gate-hook exit convention: `0` = allow, `2` = deny.
///
/// # Why this is separate from [`run_gate_hook`]
///
/// Two carriers reach the same gate. Claude's wrapped path invokes `wicked-core gate-hook` as a
/// PreToolUse hook and hands it a `{tool_name, tool_input}` payload on stdin. The ACP path has no
/// subprocess to hook — the bridge drives the agent SDK in-process and asks the CLIENT for
/// permission over `session/request_permission`. Before this split there was no way for that path
/// to reach the policy, so governed units were rerouted to single-shot execution instead
/// (FINDING-060/062), which is what made `domain-extraction` unable to finish on a real repo
/// (FINDING-100).
///
/// The audit trail is not incidental to the answer. `fold_input_denial` requires the hook-fired
/// sentinel for the phase; a carrier that returned allow/deny WITHOUT writing it would be denied
/// downstream for looking bypassed. Sharing this function is what makes the two carriers
/// indistinguishable to the fold, which is the property that matters.
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_tool_call(
    scope: &str,
    phase: &str,
    phase_alias: Option<&str>,
    db: Option<&str>,
    decisions_path: &str,
    context: &serde_json::Value,
    tool: &str,
    // The carrier's boundary (core#260): `None` ⇒ the hook-SUBPROCESS carrier, which resolves
    // roots from the env the launcher armed (`WICKED_WRITE_ROOTS`/`WICKED_READ_ROOTS`) and the
    // process cwd. `Some` ⇒ an IN-PROCESS carrier (the ACP permission bridge), whose ambient env/
    // cwd belong to the DAEMON, not the unit — its boundary must arrive as explicit state.
    boundary: Option<&BoundaryCtx>,
) -> i32 {
    // No clones: this runs once per tool call on both carriers, and `context` carries the tool's
    // whole input — file contents included (review).
    // Write the hook-fired liveness sentinel for `phase` BEFORE any policy evaluation or early-returns
    // below. This proves the hook BINARY was invoked for this phase (not just that the launcher
    // configured it). `fold_input_denial` checks for this sentinel; its absence alongside real claim
    // lines means the hook was bypassed (hook process suppressed while tool calls still ran) → DENY.
    {
        let sentinel_line = serde_json::json!({ HOOK_FIRED_KEY: phase }).to_string() + "\n";
        let sentinel_path = Path::new(&decisions_path);
        // In a launcher-managed run the dir already exists (write_armed_marker ran first); in a
        // standalone / test invocation it may not. Create it here so the sentinel write never fails
        // with a spurious DENY on a missing parent directory.
        if let Some(parent) = sentinel_path.parent() {
            if let Err(e) = create_dir_all_private(parent) {
                eprintln!("wicked-governance: DENY (could not create governance dir: {e})");
                return 2;
            }
        }
        if let Err(e) = with_append_lock(sentinel_path, || {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(decisions_path)?;
            f.write_all(sentinel_line.as_bytes())
        }) {
            eprintln!("wicked-governance: DENY (could not write hook-fired sentinel: {e})");
            return 2;
        }
    }

    // Read-only use of the store: select reads policies, decide is pure. NO store write here.
    // Use open_store_ro (SQLITE_OPEN_READONLY, no DDL) so the hook subprocess never races the
    // single-writer actor on schema or WAL operations (P4b).
    // BOUNDARY FIRST — before the store is even OPENED. A call reaching outside the unit's
    // worktree is refused before any policy is consulted: policies answer "is this action allowed
    // HERE", and a path outside the boundary has already left here. It is also refused before the
    // store-open below, because a boundary escape needs NO policy store to judge — an unreachable
    // store used to mask a boundary escape as `infra-deny`, losing the claim the fold and the
    // operator diagnose by (caught by CI on the #260 proof test, which runs storeless).
    let boundary_verdict = match boundary {
        Some(b) => boundary_denial_with(&b.roots, &b.cwd, b.home.as_deref(), context, tool),
        None => boundary_denial(context, tool),
    };
    if let Some((reason, fatal)) = boundary_verdict {
        // The tool-call is BLOCKED either way (return 2 below). A WRITE outside the sandbox is an
        // escape attempt and stays unit-FATAL; a READ probe — and a benign write into the worker's
        // own `~/.claude` state tree (core#235) — is ADVISORY: blocked, audited, but not unit-fatal,
        // so a worker probing an out-of-bounds file or persisting its own memory (then adapting) is
        // not failed for it (P8 #10 / core#219). `fatal` comes from the boundary check itself so a
        // Bash write-escape (FINDING-045) is fatal even though "Bash" is not in WRITE_TOOLS. See
        // `boundary_denial` / `append_boundary_deny`.
        append_boundary_deny(decisions_path, scope, phase, &reason, fatal);
        eprintln!("wicked-governance: DENY ({reason})");
        return 2;
    }

    // On an INFRA failure below we still exit 2 (the tool IS blocked), but we ALSO best-effort append a
    // synthetic Deny so the block leaves durable evidence — otherwise the fold would see no claim and the
    // run could Complete despite a governance-infra block (council blocker, infra-exit-2 arm).
    let store = match open_store_ro(db.filter(|s| !s.is_empty())) {
        Ok(s) => s,
        Err(e) => {
            append_infra_deny(
                decisions_path,
                scope,
                phase,
                &crate::diagnostic::with_cause("store open failed", &e),
            );
            eprintln!(
                "wicked-governance: DENY ({})",
                crate::diagnostic::with_cause("open store failed", &e)
            );
            return 2;
        }
    };

    let phases = crate::scope::phase_aliases(phase, phase_alias);
    let selected = match select_any(&store, scope, &phases, context) {
        Ok(s) => s,
        Err(e) => {
            append_infra_deny(
                decisions_path,
                scope,
                phase,
                &crate::diagnostic::with_cause("policy select failed", &e),
            );
            eprintln!(
                "wicked-governance: DENY ({})",
                crate::diagnostic::with_cause("policy select failed", &e)
            );
            return 2;
        }
    };
    let claim = decide(&selected, scope, phase, context, crate::clock::eval_now());

    // Write the tool-call annotation AND the claim as a SINGLE buffer under the advisory lock.
    // Using one buffer means that even if `with_append_lock` degrades to running without the lock
    // (e.g., the lockfile cannot be created), a single `write_all` of a small buffer is still
    // atomic on both POSIX (`O_APPEND`) and Windows (`FILE_APPEND_DATA`) — no concurrent hook
    // subprocess can interleave between the annotation and the claim (Copilot).
    {
        let annotation_json = serde_json::json!({
            TOOL_CALL_KEY: if tool.is_empty() { "tool-call" } else { tool },
            TOOL_CALL_PHASE_KEY: phase,
        })
        .to_string()
            + "\n";
        let claim_line = match serde_json::to_string(&claim) {
            Ok(mut s) => {
                s.push('\n');
                s
            }
            Err(e) => {
                eprintln!("wicked-governance: DENY (could not serialise claim: {e})");
                return 2;
            }
        };
        // Concatenate into one buffer so the single `write_all` is atomic even in degraded mode.
        let combined = annotation_json + &claim_line;
        if let Err(e) = with_append_lock(Path::new(&decisions_path), || {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(decisions_path)?;
            f.write_all(combined.as_bytes())
        }) {
            eprintln!("wicked-governance: DENY (could not append decision: {e})");
            return 2;
        }
    }

    match claim.decision {
        Decision::Deny => {
            let t = if tool.is_empty() { "tool-call" } else { tool };
            eprintln!("wicked-governance: DENY `{t}` (claim {})", claim.claim_id);
            2
        }
        _ => 0,
    }
}

/// A fail-closed reason the hook must DENY on rather than proceed, or `None` if the store is usable:
///  - No resolvable store (`--db`/`WICKED_GATE_DB` both unset): `open_store(None)` would fall back to a
///    default `.wicked-estate/graph.db` (and may CREATE an empty one), evaluating against ZERO policies —
///    a silent fail-OPEN. A governed hook MUST have the run's store; deny loudly instead.
///  - A `postgres://` spec: governance-in-run is SQLite-only for now (the read-only spec-dispatch opener
///    is core#30); deny loudly instead of silently creating a garbage SQLite file (findings #13/#18).
fn store_unavailable(db: Option<&str>) -> Option<String> {
    match db.filter(|s| !s.is_empty()) {
        // The variable NAME is interpolated from the const, not typed out. This message is the only
        // instruction an operator gets for a hook that is denying every tool call, so a message that
        // still names the previous variable after a rename prescribes an inert remedy — the exact
        // failure FINDING-066 was filed for, in a place where the symptom (total deny) is maximally
        // alarming and the wrong fix is maximally plausible.
        None => Some(format!(
            "no estate store resolvable (set --db or {GATE_DB_ENV}) — refusing to evaluate against \
             a default/empty store (fail-closed)"
        )),
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
    // Never resolve to the bare `wicked-core-gov` ROOT: an empty (or fully-escaped-away) run_id would
    // otherwise make callers like `run_session`'s fresh-launch `remove_dir_all` wipe EVERY run's gov
    // artifacts (Copilot). A non-empty placeholder keeps each run under its own subdir.
    let enc = encode_run_id(run_id);
    let enc = if enc.is_empty() {
        "_empty".to_string()
    } else {
        enc
    };
    std::env::temp_dir().join("wicked-core-gov").join(enc)
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
                // Reclaim a STALE lock (a crashed holder never unlinked it): if the lockfile is old
                // enough, remove it and retry immediately, so the mechanism self-heals instead of
                // degrading to permanently-unlocked for the rest of the attempt (council [11]). The
                // threshold is deliberately generous (30s) — a tiny claim append never takes that long,
                // so a legitimate-but-IO-stalled writer is not falsely reclaimed into a concurrent-write
                // race (Copilot), while a truly crashed holder is still recovered promptly.
                let stale = std::fs::metadata(&lock)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|age| age.as_secs() >= 30)
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

/// Written by the hook process itself (not the launcher) as the first entry after `ARMED_MARKER_KEY`.
/// Proves the hook BINARY was actually invoked and ran to the policy-evaluation point — not just that
/// the launcher configured it. `fold_input_denial` treats its absence alongside real claim lines as a
/// tamper signal: hook process was suppressed while tool calls still happened.
const HOOK_FIRED_KEY: &str = "_wicked_hook_fired";

/// Key of the tool-call annotation line the hook writes BEFORE each conformance claim. Carries the
/// tool name (e.g. `"Bash"`, `"Edit"`) and the phase so `collect_hook_decisions` can surface the
/// tool name in `GovernanceHookFired` events without re-running the evaluation. Written in the same
/// single buffer as the claim (both under the advisory lock) — a write failure returns exit 2 (fail
/// closed) and no decision is appended.
const TOOL_CALL_KEY: &str = "_wicked_tool_call";
/// Companion phase key on the tool-call annotation (pairs with `TOOL_CALL_KEY`).
const TOOL_CALL_PHASE_KEY: &str = "_wicked_tool_phase";

/// `create_dir_all` + restrict the leaf dir to owner-only (0700) on Unix, so another local user on a
/// shared host cannot traverse in to read a run's policy scope/phase, tool-call context, or denial
/// reasons (council [9]). The sensitive settings/decisions files live under this dir, so blocking
/// traversal protects them regardless of individual file mode.
pub(crate) fn create_dir_all_private(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        // Create with 0700 from the START (DirBuilder::mode) — dirs it CREATES have no create-then-chmod
        // window where they are briefly world-traversable.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // `DirBuilder::mode` does NOT re-chmod an ALREADY-EXISTING leaf (a prior run's dir, or one an
        // attacker pre-created loose after the fresh-launch clear), so tighten the leaf explicitly and
        // PROPAGATE any failure — never silently leave governance artifacts world-readable (gemini/Copilot).
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
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
        // Keyed on `phase` only — NOT the scope, which embeds `/` (`wicked-agent/<sess>/unit/<id>`) and
        // would make an unsafe/unbounded claim symbol (Copilot). One infra-deny node per phase is enough
        // (any infra failure denies the phase); the real scope still rides `claim.scope` below.
        claim_id: format!("infra-deny:{phase}"),
        scope: scope.to_string(),
        phase: phase.to_string(),
        policy_ids: vec![],
        decision: Decision::Deny,
        obligations: vec![reason.to_string()],
        evaluated_context_ref: "sha256:infra".to_string(),
        criteria: format!("governance infra failure: {reason}"),
        evaluator_identity: "wicked-governance-infra".to_string(),
        evaluated_at: crate::clock::eval_now(),
    };
    let _ = append_decision(Path::new(decisions_path), &claim);
}

/// Record a BOUNDARY refusal — a tool call that reached outside the unit's worktree.
///
/// Deliberately not [`append_infra_deny`]. An infra deny says "governance could not be evaluated";
/// a boundary deny says "governance evaluated this and the call left its scope". Filing the second
/// under the first mislabels the evaluator and criteria in the append-only log — the record audits
/// and alerts read — so a real escape would surface as an infrastructure wobble (review).
/// The evaluator identity stamped on every filesystem-boundary deny (read OR write). Distinguishes a
/// containment block from an operator POLICY deny (which carries a policy evaluator identity), so the
/// fold/drain can treat the two differently.
const BOUNDARY_EVALUATOR: &str = "wicked-governance-boundary";
/// Claim-id prefix for a boundary deny that STAYS unit-fatal: a WRITE outside the sandbox (an escape
/// attempt — e.g. the FINDING-098 pin-rewrite). This is the DEFAULT-FATAL prefix, and it is fatal by
/// omission rather than by enumeration: `is_advisory_boundary_read_deny` is an allowlist that fires
/// ONLY for [`BOUNDARY_READ_DENY_PREFIX`], so a claim carrying this prefix — or any other deny the
/// fold/drain ever sees — is treated as fatal. `append_boundary_deny` reaches here whenever
/// `is_write` is set; there is no third writer-side category.
const BOUNDARY_WRITE_DENY_PREFIX: &str = "boundary-deny:";
/// Claim-id prefix for an ADVISORY boundary deny: a READ outside the sandbox. The tool-call is STILL
/// blocked (the worker never reads the file), but a blocked read leaks nothing and the worker adapts,
/// so it is recorded for audit and does NOT fail the unit (P8 #10 / core#219). Whether the blocked
/// read MATTERED is decided by the unit's own output gate, not by the containment event.
const BOUNDARY_READ_DENY_PREFIX: &str = "boundary-read-deny:";

/// Record a filesystem-boundary block. `fatal` picks whether it ABORTS the unit (a write/escape) or
/// is ADVISORY (a read probe, or a benign write into the worker's own `~/.claude` tree — core#235).
/// Either way the caller has already exited 2 — the tool-call is blocked. The `reason` carries the
/// accurate `(write)`/`(read)` from the boundary `Denial`, so an advisory WRITE is still honestly
/// described even though it shares the advisory prefix a read uses (the fold keys on that prefix).
fn append_boundary_deny(decisions_path: &str, scope: &str, phase: &str, reason: &str, fatal: bool) {
    let (prefix, criteria) = if fatal {
        (
            BOUNDARY_WRITE_DENY_PREFIX,
            format!("filesystem boundary: {reason}"),
        )
    } else {
        (
            BOUNDARY_READ_DENY_PREFIX,
            format!("filesystem boundary (advisory: blocked, worker continues): {reason}"),
        )
    };
    let claim = ConformanceClaim {
        // Keyed on `phase` only, for the same reason `append_infra_deny` is: `scope` embeds `/`
        // and would make an unbounded claim symbol.
        claim_id: format!("{prefix}{phase}"),
        scope: scope.to_string(),
        phase: phase.to_string(),
        policy_ids: vec![],
        decision: Decision::Deny,
        obligations: vec![reason.to_string()],
        evaluated_context_ref: "sha256:boundary".to_string(),
        criteria,
        evaluator_identity: BOUNDARY_EVALUATOR.to_string(),
        evaluated_at: crate::clock::eval_now(),
    };
    let _ = append_decision(Path::new(decisions_path), &claim);
}

/// Whether a Deny claim is an ADVISORY boundary READ block — recorded for audit but NOT unit-fatal.
/// A blocked read is containment SUCCEEDING (the read was prevented, nothing leaked, the worker
/// adapts); failing the whole unit for it conflates prevention with violation (P8 #10 / core#219).
/// Requires BOTH the boundary evaluator identity AND the read prefix, so a policy deny or a write
/// escape can never be mistaken for advisory. The worker cannot forge this: the decisions log lives
/// outside its write boundary and is written only by the gate-hook.
fn is_advisory_boundary_read_deny(claim: &ConformanceClaim) -> bool {
    claim.decision == Decision::Deny
        && claim.evaluator_identity == BOUNDARY_EVALUATOR
        && claim.claim_id.starts_with(BOUNDARY_READ_DENY_PREFIX)
}

/// If `v` is an armed-marker object, the phase it marks; else `None`. Checks the ROOT key
/// (`v.get(ARMED_MARKER_KEY)`), NOT a substring — a substring match would let a crafted claim whose
/// `criteria`/`obligations` merely CONTAIN the marker string be silently skipped by the fold, bypassing
/// its Deny (gemini/Copilot security-critical). A real `ConformanceClaim` never carries this root key.
fn marker_phase(v: &serde_json::Value) -> Option<&str> {
    v.get(ARMED_MARKER_KEY).and_then(|x| x.as_str())
}

/// If `v` is a hook-fired sentinel, the phase it covers; else `None`. Root-key check for the same
/// reason as `marker_phase` — substring matching would let a crafted claim sneak past the fold.
fn fired_phase(v: &serde_json::Value) -> Option<&str> {
    v.get(HOOK_FIRED_KEY).and_then(|x| x.as_str())
}

/// If `v` is a tool-call annotation (written by the hook before each claim), return `(tool_name,
/// phase)`; else `None`. Root-key check — the same security rationale as `marker_phase`.
fn tool_call_entry(v: &serde_json::Value) -> Option<(&str, &str)> {
    let tool = v.get(TOOL_CALL_KEY).and_then(|x| x.as_str())?;
    let phase = v.get(TOOL_CALL_PHASE_KEY).and_then(|x| x.as_str())?;
    Some((tool, phase))
}

/// One hook decision record for `GovernanceHookFired` — the structured view of a single tool-call
/// intercepted by the governance hook subprocess and recorded in the decisions NDJSON.
#[derive(Debug, Clone)]
pub struct HookDecisionRecord {
    /// The tool the hook intercepted (e.g. `"Bash"`, `"Edit"`). `"(unknown)"` when the
    /// tool-call annotation was not present in the log (older hook versions, or write failure).
    pub tool_name: String,
    /// The hook's decision for this tool call: `"allow"`, `"allow_with_conditions"`, or `"deny"`.
    pub decision: String,
    /// The first policy id that denied, when `decision == "deny"`. `None` when allowed (or when
    /// the deny came from an infra/corruption path with no policy ids).
    pub denying_policy: Option<String>,
}

/// Collect the per-tool-call hook decisions for `(run_id, attempt, phase)` from the decisions
/// log, for emitting [`crate::event::CoreEvent::GovernanceHookFired`] events. Returns an empty
/// `Vec` when the log is absent (ungoverned unit or log not yet written). Does NOT fail closed —
/// this is observability-only; governance enforcement is `fold_input_denial`'s job.
///
/// Correlates each tool-call annotation (`TOOL_CALL_KEY`) with the immediately-following claim
/// for the same phase, so the tool name rides the event even though `ConformanceClaim` does not
/// store it. Logs written before the annotation was added gracefully degrade to `"(unknown)"` for
/// the tool name.
pub fn collect_hook_decisions(run_id: &str, attempt: u32, phase: &str) -> Vec<HookDecisionRecord> {
    let path = decisions_path_for(run_id, attempt);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Vec::new(), // log absent or unreadable — no events to emit
    };
    let mut records: Vec<HookDecisionRecord> = Vec::new();
    let mut pending_tool: Option<String> = None; // tool name from the last annotation
    for line in raw.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                pending_tool = None;
                continue;
            }
        };
        // Skip armed-marker and hook-fired sentinel lines.
        if marker_phase(&v).is_some() || fired_phase(&v).is_some() {
            continue;
        }
        // Tool-call annotation — note the tool name for the next claim on this phase.
        // Clear `pending_tool` when the annotation is for a DIFFERENT phase so a stale tool
        // name from another phase is never incorrectly attached to a later claim (Copilot).
        if let Some((tool, ann_phase)) = tool_call_entry(&v) {
            if ann_phase == phase {
                pending_tool = Some(tool.to_string());
            } else {
                pending_tool = None;
            }
            continue;
        }
        // Try to deserialize as a ConformanceClaim.
        let claim: wicked_apps_core::ConformanceClaim = match serde_json::from_value(v) {
            Ok(c) => c,
            Err(_) => {
                pending_tool = None;
                continue;
            }
        };
        if claim.phase != phase {
            pending_tool = None;
            continue;
        }
        // Map each Decision variant explicitly so consumers of `GovernanceHookFired` see full
        // fidelity — collapsing AllowWithConditions into "allow" loses the conditional signal
        // and can mislead operators inspecting hook decisions (Copilot).
        let decision_str = match claim.decision {
            wicked_apps_core::Decision::Deny => "deny",
            wicked_apps_core::Decision::AllowWithConditions => "allow_with_conditions",
            _ => "allow",
        };
        let denying_policy = if claim.decision == wicked_apps_core::Decision::Deny {
            claim.policy_ids.into_iter().next()
        } else {
            None
        };
        records.push(HookDecisionRecord {
            tool_name: pending_tool
                .take()
                .unwrap_or_else(|| "(unknown)".to_string()),
            decision: decision_str.to_string(),
            denying_policy,
        });
    }
    records
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
    let mut saw_hook_fired = false;
    let mut has_claim_lines = false; // any ConformanceClaim present for `phase`
    for line in raw.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue; // blank / non-claim line — not corruption
        }
        // Parse each line ONCE as a JSON value; a `{`-prefixed line that won't parse is un-evaluable
        // governance evidence ⇒ deny-dominant (fail closed) via the normal terminal path, not a
        // run-wedging Err.
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                if denial.is_none() {
                    denial = Some(format!(
                        "input governance denied {phase} (fail-closed): corrupted decision line: {e}"
                    ));
                }
                continue;
            }
        };
        // Armed marker (root-key check, not substring): note it for THIS phase, skip it as a claim.
        if let Some(mp) = marker_phase(&v) {
            if mp == phase {
                saw_marker = true;
            }
            continue;
        }
        // Hook-fired sentinel (root-key check): proves the hook BINARY ran for this phase.
        if let Some(fp) = fired_phase(&v) {
            if fp == phase {
                saw_hook_fired = true;
            }
            continue;
        }
        // Tool-call annotation written by `run_gate_hook` immediately before each claim so that
        // `collect_hook_decisions` can recover the tool name. NOT a `ConformanceClaim` — skip it
        // here with the same root-key check used for the other sentinel types.
        if tool_call_entry(&v).is_some() {
            continue;
        }
        let claim: ConformanceClaim = match serde_json::from_value(v) {
            Ok(c) => c,
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
        has_claim_lines = true;
        conform(store, &claim)?;
        // An ADVISORY boundary READ deny is recorded (conform above, for audit) but does NOT fail the
        // unit: the read was blocked, nothing leaked, and the worker adapts — whether the missing file
        // mattered is judged by the unit's OUTPUT gate, not this containment event (P8 #10 / core#219).
        if denial.is_none()
            && claim.decision == Decision::Deny
            && !is_advisory_boundary_read_deny(&claim)
        {
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
    // Hook-liveness check: if there are claim lines for this phase but no hook-fired sentinel, the
    // hook process was suppressed while tool calls still executed — deny immediately. The sentinel is
    // written BEFORE any claim evaluation in `run_gate_hook`, so its absence with claims present is
    // impossible in normal operation and indicates hook bypass.
    if governed && saw_marker && has_claim_lines && !saw_hook_fired && denial.is_none() {
        denial = Some(format!(
            "input governance denied {phase} (fail-closed): hook-fired sentinel missing with \
             claim lines present — hook process may have been suppressed (core#34)"
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
        // Parse once as a value. FAIL CLOSED on a corrupted `{`-prefixed line: un-evaluable governance
        // evidence must never be silently skipped into an allow (finding #10). A blank / non-`{` line was
        // already `continue`d.
        let v: serde_json::Value = serde_json::from_str(line).map_err(|e| {
            anyhow::anyhow!("hook-decision drain DENY (fail-closed): corrupted claim line: {e}")
        })?;
        // Skip sentinel and annotation lines (root-key check) — they are metadata, not claims, and
        // would otherwise fail the drain closed on the `ConformanceClaim` deserialisation step.
        if marker_phase(&v).is_some() || fired_phase(&v).is_some() || tool_call_entry(&v).is_some()
        {
            continue;
        }
        let claim: ConformanceClaim = serde_json::from_value(v).map_err(|e| {
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
        // Advisory boundary READ denies are audit-only (conform()'d in Pass 1) and never veto the
        // input-governance gate (P8 #10 / core#219) — exclude them from the deny-dominating verdict at
        // every tier. A phase whose ONLY claims are advisory has nothing gate-affecting to resolve, so
        // it is skipped (no spurious veto).
        let verdict = match claims
            .iter()
            .filter(|c| !is_advisory_boundary_read_deny(c))
            .find(|c| c.decision == Decision::Deny)
            .or_else(|| {
                claims
                    .iter()
                    .filter(|c| !is_advisory_boundary_read_deny(c))
                    .find(|c| c.decision == Decision::AllowWithConditions)
            })
            .or_else(|| claims.iter().find(|c| !is_advisory_boundary_read_deny(c)))
        {
            Some(v) => v,
            None => continue,
        };
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
        // gate_hook only opens a phase that doesn't yet exist → always attempt 0 here.
        advance_to_gate_running(store, phase_id, 0)?;
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
pub(crate) fn claude_pretool_context(
    raw: &str,
    scope: &str,
    phase: &str,
) -> (serde_json::Value, String) {
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
pub fn run_output_gate_hook(
    scope: &str,
    phase: &str,
    phase_alias: Option<&str>,
    db: Option<&str>,
) -> i32 {
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
    let store = match open_store_ro(db.filter(|s| !s.is_empty())) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "wicked-governance: DENY ({})",
                crate::diagnostic::with_cause("open store failed", &e)
            );
            return 2;
        }
    };
    let phases = crate::scope::phase_aliases(phase, phase_alias);
    let selected = match select_any(&store, scope, &phases, &context) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "wicked-governance: DENY ({})",
                crate::diagnostic::with_cause("policy select failed", &e)
            );
            return 2;
        }
    };
    let mut claim = decide(&selected, scope, phase, &context, crate::clock::eval_now());

    // Wire recall INTO the output gate (M6/M7): the conformance rules applicable to the output's
    // facets become obligations on the claim. A recall failure is a governance failure (fail
    // closed) — never silently drop the ruleset.
    if let Err(e) = attach_recalled_rules(&store, &output_rule_query(), &mut claim) {
        eprintln!(
            "wicked-governance: DENY ({})",
            crate::diagnostic::with_cause("conformance-rule recall failed", &e)
        );
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
pub(crate) fn attach_recalled_rules(
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
/// The subprocess `output-gate-hook` uses this (the launcher scopes `WICKED_OUTPUT_*` per run); the
/// in-process `apply_unit` recall deliberately uses a wildcard instead (see `execute::apply_unit`).
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
    use wicked_apps_core::open_store;

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
            evaluated_at: crate::clock::eval_now(),
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
                retired: false,
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
            evaluated_at: crate::clock::eval_now(),
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
            retired: false,
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

        // (f) SECURITY: a Deny claim whose CRITERIA merely CONTAINS the marker key string must STILL be
        // detected — a substring match (the pre-fix bug) would misclassify it as a marker and skip it,
        // bypassing the Deny. Key-based detection parses it as a claim → the Deny fires.
        let path3 = decisions_path_for(&run_id, 4);
        write_armed_marker(&path3, "unit-1").unwrap();
        let mut evil = allow_claim("ev-evil", "unit-1");
        evil.decision = Decision::Deny;
        evil.criteria = format!("crafted to evade the fold: {ARMED_MARKER_KEY}");
        append_decision(&path3, &evil).unwrap();
        assert!(
            fold_input_denial(&mut store, &run_id, 4, "unit-1", true)
                .unwrap()
                .is_some(),
            "a Deny whose criteria contains the marker string is NOT skipped (no substring bypass)"
        );
        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));
    }

    #[test]
    fn advisory_boundary_read_deny_is_recorded_but_not_unit_fatal() {
        // core#219 / P8 #10. A governed unit whose worker probes a READ outside its boundary must NOT
        // fail: the read is BLOCKED (containment succeeded, nothing leaked), the worker adapts, and the
        // block is recorded for audit. A blocked WRITE (escape attempt) and operator POLICY denies stay
        // unit-fatal. This is the last blocker to a fully-green unattended governed clean pass.
        let mut store = open_store(Some(":memory:")).unwrap();
        let run_id = format!("advisory-{}", std::process::id());
        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));

        // --- fold_input_denial (unit verdict) ---------------------------------------------------

        // (a) GOVERNED unit, marker + ONLY an advisory boundary READ deny → NOT denied (Ok(None)).
        let p0 = decisions_path_for(&run_id, 0);
        write_armed_marker(&p0, "unit-5").unwrap();
        write_hook_fired(&p0, "unit-5");
        // Written by the real production path — exercises the actual prefix/criteria/evaluator wiring,
        // not a hand-forged claim. is_write=false ⇒ `boundary-read-deny:` + BOUNDARY_EVALUATOR.
        append_boundary_deny(
            p0.to_str().unwrap(),
            "wf/unit-5",
            "unit-5",
            "path outside this unit's boundary: /other/repo/domain-modeler.md (read)",
            false,
        );
        assert_eq!(
            fold_input_denial(&mut store, &run_id, 0, "unit-5", true).unwrap(),
            None,
            "an advisory boundary READ deny does not fail the unit"
        );
        // ...but it IS durable evidence — the block was conformed to the store, not dropped.
        assert_eq!(
            count_claims(&store, "boundary-read-deny:unit-5").unwrap(),
            1,
            "the blocked read is recorded for audit even though it is non-fatal"
        );

        // (b) GOVERNED unit, marker + a boundary WRITE deny → DENIED (an escape attempt stays fatal).
        let p1 = decisions_path_for(&run_id, 1);
        write_armed_marker(&p1, "unit-5").unwrap();
        append_boundary_deny(
            p1.to_str().unwrap(),
            "wf/unit-5",
            "unit-5",
            "path outside this unit's boundary: /etc/evil (write)",
            true,
        );
        let write_denial = fold_input_denial(&mut store, &run_id, 1, "unit-5", true).unwrap();
        assert!(
            write_denial
                .as_deref()
                .is_some_and(|d| d.contains("boundary-deny:unit-5")),
            "a boundary WRITE deny fails the unit and names the claim: {write_denial:?}"
        );

        // (c) MIXED: an advisory read deny AND a real POLICY deny in the same unit → still DENIED. The
        // advisory exclusion must not mask a co-occurring fatal deny (a policy deny carries a policy
        // evaluator identity, so it is never mistaken for advisory).
        let p2 = decisions_path_for(&run_id, 2);
        write_armed_marker(&p2, "unit-5").unwrap();
        append_boundary_deny(
            p2.to_str().unwrap(),
            "wf/unit-5",
            "unit-5",
            "path outside this unit's boundary: /other/probe (read)",
            false,
        );
        let mut policy_deny = allow_claim("POL-042", "unit-5");
        policy_deny.decision = Decision::Deny;
        append_decision(&p2, &policy_deny).unwrap();
        assert!(
            fold_input_denial(&mut store, &run_id, 2, "unit-5", true)
                .unwrap()
                .is_some(),
            "an advisory read deny does not mask a co-occurring policy deny"
        );

        // --- apply_hook_decisions (phase-gate drain) --------------------------------------------

        // (d) A phase whose ONLY Deny is an advisory read block must NOT veto the gate.
        let p3 = decisions_path_for(&run_id, 3);
        append_decision(&p3, &allow_claim("drain-allow", "exec")).unwrap();
        append_boundary_deny(
            p3.to_str().unwrap(),
            "wf/exec",
            "exec",
            "path outside this unit's boundary: /other/read (read)",
            false,
        );
        let summary = apply_hook_decisions(&mut store, "advisory-drain", &p3).unwrap();
        assert_eq!(
            summary.denied, 0,
            "an advisory read deny does not veto the phase gate on drain"
        );
        assert_eq!(
            summary.applied, 2,
            "both the allow and the advisory deny still conform as durable evidence"
        );

        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));
        let _ = std::fs::remove_dir_all(gov_run_dir("advisory-drain"));
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
        assert_eq!(
            run_gate_hook("s", "unit-1", None, Some("postgres://h/db")),
            2
        );
        assert_eq!(run_gate_hook("s", "unit-1", None, None), 2);
        assert_eq!(run_gate_hook("s", "unit-1", None, Some(":memory:")), 2);
        assert_eq!(
            run_output_gate_hook("s", "unit-1", None, Some("postgres://h/db")),
            2
        );
        assert_eq!(run_output_gate_hook("s", "unit-1", None, None), 2);
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

    /// Verify `collect_hook_decisions` correlation logic: annotation→claim pairing, graceful
    /// degradation when an annotation is absent, and phase-isolation of stale annotations.
    #[test]
    fn collect_hook_decisions_correlates_tool_names_and_handles_edge_cases() {
        let run_id = format!("chd-test-{}", std::process::id());
        let path = decisions_path_for(&run_id, 0);
        let _ = std::fs::remove_file(&path);

        // Write an armed marker, a hook-fired sentinel, then three claim groups:
        //   A) annotation(Bash, unit-1) + claim(Allow, unit-1) → tool name "Bash"
        //   B) claim(Deny, unit-1) with NO annotation → tool name "(unknown)"
        //   C) annotation(Write, unit-2) + claim(Allow, unit-2) → different phase, must NOT
        //      leak into unit-1 results; a subsequent Allow on unit-1 also gets "(unknown)"
        write_armed_marker(&path, "unit-1").unwrap();

        // Sentinel (group A)
        let sentinel = serde_json::json!({ HOOK_FIRED_KEY: "unit-1" }).to_string() + "\n";
        {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(sentinel.as_bytes()).unwrap();
        }

        // Group A: annotation + allow claim for unit-1
        {
            let ann = serde_json::json!({ TOOL_CALL_KEY: "Bash", TOOL_CALL_PHASE_KEY: "unit-1" })
                .to_string()
                + "\n";
            let claim = allow_claim("a1", "unit-1");
            let claim_line = serde_json::to_string(&claim).unwrap() + "\n";
            let combined = ann + &claim_line;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(combined.as_bytes()).unwrap();
        }

        // Group B: deny claim for unit-1 with NO annotation — tool must degrade to "(unknown)"
        {
            let mut deny = allow_claim("d1", "unit-1");
            deny.decision = Decision::Deny;
            append_decision(&path, &deny).unwrap();
        }

        // Group C: annotation for unit-2 then allow for unit-1 — annotation MUST NOT leak
        {
            let ann = serde_json::json!({ TOOL_CALL_KEY: "Write", TOOL_CALL_PHASE_KEY: "unit-2" })
                .to_string()
                + "\n";
            let claim = allow_claim("a2", "unit-1");
            let claim_line = serde_json::to_string(&claim).unwrap() + "\n";
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(ann.as_bytes()).unwrap();
            f.write_all(claim_line.as_bytes()).unwrap();
        }

        let records = collect_hook_decisions(&run_id, 0, "unit-1");
        // A) annotation + allow → "Bash"
        assert_eq!(records.len(), 3, "three claims for unit-1");
        assert_eq!(
            records[0].tool_name, "Bash",
            "annotated claim gets tool name"
        );
        assert_eq!(records[0].decision, "allow");
        assert!(records[0].denying_policy.is_none());
        // B) deny without annotation → "(unknown)"
        assert_eq!(
            records[1].tool_name, "(unknown)",
            "unannotated claim degrades to (unknown)"
        );
        assert_eq!(records[1].decision, "deny");
        // C) annotation for unit-2 must not leak into the unit-1 claim that follows
        assert_eq!(
            records[2].tool_name, "(unknown)",
            "annotation for a different phase must not attach to a unit-1 claim"
        );

        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));
    }

    /// A minimal Allow [`ConformanceClaim`] on `phase` for the drain/recall tests.
    /// Append a hook-fired liveness sentinel — the production hook writes one per phase before any
    /// claim, so a governed fold that reaches its liveness check does not fail closed (core#34).
    fn write_hook_fired(path: &Path, phase: &str) {
        use std::io::Write;
        let line = serde_json::json!({ HOOK_FIRED_KEY: phase }).to_string() + "\n";
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(line.as_bytes()).unwrap();
    }

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
            evaluated_at: crate::clock::eval_now(),
        }
    }
}

/// The contract between the two artifacts that implement governance, as ONE number.
///
/// # Why this exists
///
/// The launcher lives in the engine (the napi `.node` module); the hook is a separately installed
/// `wicked-core` CLI found on PATH. They are two build artifacts that must agree on a set of
/// environment-variable NAMES, because the injected hook command carries no arguments — everything
/// travels by env so caller-controlled ids cannot inject shell metacharacters.
///
/// Nothing verified that agreement. #165 renamed the store carrier `WICKED_ESTATE_DB` →
/// `WICKED_GATE_DB`; deploy that engine against an un-rebuilt CLI and the launcher sets the new name,
/// the old CLI reads only the old one, finds nothing, and fails closed — correctly — on EVERY tool
/// call of EVERY governed run. The resulting error ("no estate store resolvable, set --db or
/// WICKED_GATE_DB") is accurate and leads nowhere: the launcher already sets that variable, and the
/// operator setting it by hand changes nothing, because the old binary cannot read it. The fault is
/// version skew and nothing named version skew.
///
/// # Bump this
///
/// Whenever a carrier NAME changes, an argument is added or removed, or an exit code changes meaning.
/// Not for behaviour changes behind a stable interface.
pub const GATE_PROTOCOL_VERSION: u32 = 1;

/// The line `gate-hook --protocol-version` prints. Parsed by the launcher; keep it one stable line.
#[must_use]
pub fn protocol_version_line() -> String {
    format!("wicked-core gate-hook protocol {GATE_PROTOCOL_VERSION}")
}

/// Parse [`protocol_version_line`] back out of a probe's stdout.
///
/// Tolerant of surrounding whitespace and trailing output, strict about the shape: anything it does
/// not recognise is `None`, which the caller must treat as skew rather than as "probably fine".
#[must_use]
pub fn parse_protocol_version(stdout: &str) -> Option<u32> {
    stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("wicked-core gate-hook protocol "))
        .and_then(|v| v.trim().parse().ok())
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    /// The line the launcher parses is the line the CLI prints. Two artifacts, one shape — asserted
    /// here rather than left to matching string literals in two files (core#167).
    #[test]
    fn the_printed_line_round_trips_through_the_parser() {
        assert_eq!(
            parse_protocol_version(&protocol_version_line()),
            Some(GATE_PROTOCOL_VERSION)
        );
    }

    #[test]
    fn a_different_version_parses_as_that_version_not_as_ours() {
        // The mismatch case must be DETECTED, not normalised away.
        assert_eq!(
            parse_protocol_version("wicked-core gate-hook protocol 99"),
            Some(99)
        );
        assert_ne!(Some(GATE_PROTOCOL_VERSION), Some(99));
    }

    #[test]
    fn unparseable_output_is_none_so_the_caller_must_treat_it_as_skew() {
        // An old CLI prints something else, or nothing. None must never read as "probably current".
        for junk in [
            "",
            "wicked-core 0.3.1",
            "error: unknown flag --protocol-version",
            "wicked-core gate-hook protocol",
            "wicked-core gate-hook protocol vNext",
        ] {
            assert_eq!(parse_protocol_version(junk), None, "junk parsed: {junk:?}");
        }
    }

    #[test]
    fn the_version_survives_surrounding_noise() {
        // Real stdout may carry a warning line; the probe should still find the contract.
        let out = format!("warning: something\n{}\n", protocol_version_line());
        assert_eq!(parse_protocol_version(&out), Some(GATE_PROTOCOL_VERSION));
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    use serde_json::json;

    /// Env is process-global and Rust runs tests in threads, so these serialize on one mutex.
    /// Without it, two tests setting WICKED_WRITE_ROOTS race and the failure looks like a logic bug.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_roots<T>(write: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        match write {
            Some(w) => std::env::set_var(WRITE_ROOTS_ENV, w),
            None => std::env::remove_var(WRITE_ROOTS_ENV),
        }
        std::env::remove_var(READ_ROOTS_ENV);
        let out = f();
        std::env::remove_var(WRITE_ROOTS_ENV);
        out
    }

    fn ctx(path: &str) -> serde_json::Value {
        json!({ "path": path })
    }

    /// THE case. A governed worker located the pin binding its own gate and began authoring a
    /// replacement. With the worktree armed as the only write root, that write is refused.
    #[test]
    fn the_governance_pin_is_outside_the_boundary() {
        let wt = std::env::temp_dir().join("wicked-boundary-wt");
        std::fs::create_dir_all(&wt).unwrap();
        let pin = dirs_config_workflow();
        with_roots(Some(wt.to_str().unwrap()), || {
            let (denial, is_write) = boundary_denial(&ctx(&pin), "Write")
                .expect("writing the gate's own pin must be refused");
            assert!(is_write, "writing the pin is a WRITE escape (unit-fatal)");
            assert!(
                denial.contains(&wt.to_string_lossy().to_string()),
                "the denial must name where the call WOULD have been allowed, or the agent \
                 retries blind: {denial}"
            );
        });
    }

    /// core#235. The governed claude worker routinely writes its OWN Claude Code project-memory
    /// (`~/.claude/projects/<slug>/memory/*.md`), which is outside the worktree. The write is STILL
    /// blocked (nothing lands), but it must be ADVISORY (`fatal == false`) — not abort the run.
    /// infigraph's domain-extraction died exactly here. The pin control below proves the carve-out
    /// is scoped to `~/.claude` and does not reopen the FINDING-098 pin-rewrite escape.
    ///
    /// Falsified by dropping the carve-out in `boundary_denial` (returning the raw `is_write`): the
    /// memory write is then reported fatal and the first assert fails. The pin control catches the
    /// opposite mutation (a blanket `fatal = false`, which would also un-gate the pin).
    #[cfg(unix)]
    #[test]
    fn a_write_to_the_workers_own_claude_memory_is_advisory_not_fatal() {
        let wt = std::env::temp_dir().join("wicked-boundary-wt-mem");
        std::fs::create_dir_all(&wt).unwrap();
        let home = std::env::var("HOME").expect("HOME set in the unix test env");
        let mem = format!(
            "{home}/.claude/projects/-tmp-wicked-boundary-wt-mem/memory/project_x_domain.md"
        );
        with_roots(Some(wt.to_str().unwrap()), || {
            let (_, fatal) = boundary_denial(&ctx(&mem), "Write")
                .expect("a write outside the worktree is STILL blocked");
            assert!(
                !fatal,
                "a write into the worker's own ~/.claude memory must be ADVISORY, not unit-fatal (core#235)"
            );
            // Control: an escape to a DIFFERENT out-of-boundary path (the gate pin) stays FATAL —
            // the carve-out is scoped to ~/.claude, it does not relax the pin.
            let (_, pin_fatal) = boundary_denial(&ctx(&dirs_config_workflow()), "Write")
                .expect("writing the gate pin is still refused");
            assert!(
                pin_fatal,
                "the gate pin (~/.config/**) write must stay unit-fatal"
            );
        });
    }

    #[test]
    fn a_write_inside_the_worktree_is_allowed() {
        let wt = std::env::temp_dir().join("wicked-boundary-wt2");
        std::fs::create_dir_all(&wt).unwrap();
        let inside = wt.join("src").join("main.rs");
        with_roots(Some(wt.to_str().unwrap()), || {
            assert!(boundary_denial(&ctx(inside.to_str().unwrap()), "Write").is_none());
        });
    }

    /// `..` must not walk out. The policy normalizes before comparing, so a traversal resolves to
    /// its real target and is judged there.
    #[test]
    fn traversal_out_of_the_worktree_is_refused() {
        let wt = std::env::temp_dir().join("wicked-boundary-wt3");
        std::fs::create_dir_all(&wt).unwrap();
        let escape = wt.join("..").join("elsewhere.json");
        with_roots(Some(wt.to_str().unwrap()), || {
            assert!(boundary_denial(&ctx(escape.to_str().unwrap()), "Write").is_some());
        });
    }

    /// A read of the operational store is still outside the boundary. FINDING-067 was a worker that
    /// reached the platform's own state; reads of it are reconnaissance for exactly that.
    #[test]
    fn reads_outside_the_boundary_are_refused_too() {
        let wt = std::env::temp_dir().join("wicked-boundary-wt4");
        std::fs::create_dir_all(&wt).unwrap();
        with_roots(Some(wt.to_str().unwrap()), || {
            assert!(boundary_denial(&ctx("/etc/passwd"), "Read").is_some());
        });
    }

    /// No roots armed means no boundary was CONFIGURED — the honest state for a standalone or
    /// ungoverned invocation. It must not silently become "deny everything" and break those, nor
    /// be mistaken for a boundary that passed.
    #[test]
    fn an_unarmed_boundary_is_absent_not_permissive_and_not_denying() {
        with_roots(None, || {
            assert!(boundary_denial(&ctx("/etc/passwd"), "Write").is_none());
        });
    }

    /// A Bash command with no write target is not a boundary question — a read/list is not an escape.
    #[test]
    fn a_bash_command_with_no_write_target_is_not_judged() {
        let wt = std::env::temp_dir().join("wicked-boundary-wt5");
        std::fs::create_dir_all(&wt).unwrap();
        with_roots(Some(wt.to_str().unwrap()), || {
            assert!(boundary_denial(&json!({"command": "ls /"}), "Bash").is_none());
        });
    }

    /// FINDING-045: a Bash WRITE that leaves the worktree is refused and unit-FATAL, even though
    /// "Bash" is not a path-bearing WRITE_TOOL. A write INSIDE the worktree is allowed. Covers the
    /// direct escape shapes the finding names (redirect, cp, tee). Mutation: delete the Bash arm of
    /// `boundary_denial` and the out-of-boundary writes return None — these asserts fail.
    #[test]
    fn a_bash_write_outside_the_worktree_is_refused_and_fatal() {
        let wt = std::env::temp_dir().join("wicked-boundary-wt-bash");
        std::fs::create_dir_all(&wt).unwrap();
        let inside = wt.join("out.txt");
        let inside = inside.to_str().unwrap();
        with_roots(Some(wt.to_str().unwrap()), || {
            // Redirect to an absolute path outside the boundary → DENY, is_write=true (fatal).
            let d = boundary_denial(&json!({"command": "echo pwned > /etc/evil-marker"}), "Bash");
            assert!(
                d.as_ref().is_some_and(|(_, is_write)| *is_write),
                "a Bash redirect out of the worktree must be a FATAL boundary deny: {d:?}"
            );
            // cp and tee destinations outside → also denied.
            assert!(
                boundary_denial(&json!({"command": "cp ./a.txt /etc/evil-marker"}), "Bash")
                    .is_some(),
                "cp to an outside destination must be denied"
            );
            assert!(
                boundary_denial(&json!({"command": "echo x | tee /etc/evil-marker"}), "Bash")
                    .is_some(),
                "tee to an outside file must be denied"
            );
            // A write INSIDE the worktree is fine — the boundary is a fence, not a Bash ban.
            assert!(
                boundary_denial(&json!({ "command": format!("echo ok > {inside}") }), "Bash")
                    .is_none(),
                "a Bash write inside the worktree must be allowed"
            );
            // Standard write SINKS are not escapes — the governed PageIndex pass failed on an
            // `analyze` unit's `… > /dev/null` before this (a false positive that fails ~every
            // workflow). These must be allowed.
            for sink in [
                "echo x > /dev/null",
                "cmd 2> /dev/null",
                "cmd > /dev/null 2>&1",
                "cmd 2>/dev/stderr",
                "echo hi | tee /dev/null",
                // A sequence separator glued to the sink — the real run-4c63ba17 false positive.
                // Whitespace-only tokenizing captured the target as `/dev/null;`, which is not a safe
                // sink, so the governed domain-graph unit was denied. `shell_tokens` splits the `;`.
                "echo x > /dev/null; echo done",
                "cmd >/dev/null;ls",
                "cmd 2>/dev/null; true",
                // Subshell parens glued to a sink.
                "(echo x > /dev/null)",
            ] {
                assert!(
                    boundary_denial(&json!({ "command": sink }), "Bash").is_none(),
                    "a Bash write to a standard sink must be allowed: {sink}"
                );
            }
            // NON-MASKING: splitting the glued `;` must not hide a second redirect that DOES escape.
            // `>/dev/null;>/etc/evil` is a safe sink followed by a glued escape — the escape must win.
            assert!(
                boundary_denial(
                    &json!({"command": "echo x >/dev/null;>/etc/evil-marker"}),
                    "Bash"
                )
                .is_some(),
                "a glued second redirect out of the worktree must still be denied"
            );
            // REGRESSION (Copilot review on #228): a BACKSLASH-ESCAPED `;` is a literal path byte, not a
            // separator, so the whole token is ONE redirect target. Rooted INSIDE the worktree with a
            // `..` climb that leaves it, the correct target resolves OUTSIDE → DENY. A naive split at the
            // `;` truncates the target to the in-worktree prefix (`<wt>/sub\`) and drops the traversal —
            // the boundary weakening the old whole-token tokenizer did not have. Discriminating because
            // the prefix is genuinely inside the allowed root (unlike a bare relative path, which
            // resolves against the process cwd and would be denied either way). Mutation: remove the
            // escape handling in `shell_tokens` → the split truncates to `<wt>/sub\` and this fails.
            let escaped = format!(r"echo x > {}/sub\;/../../../../../etc/evil", wt.display());
            assert!(
                boundary_denial(&json!({ "command": escaped }), "Bash").is_some(),
                "an escaped ; must keep the whole target so its ../.. escape is still denied: {escaped}"
            );
            // A QUOTED separator is likewise literal — the `;` must not split. (These deny via the
            // quote-naive relative fallback rather than absolute resolution; see the shell_tokens SCOPE
            // note. What is guarded here is that quote tracking keeps `;` inside the token, not split.)
            for q in ['"', '\''] {
                let quoted = format!(
                    "echo x > {q}{}/sub;/../../../../../etc/evil{q}",
                    wt.display()
                );
                assert!(
                    boundary_denial(&json!({ "command": quoted }), "Bash").is_some(),
                    "a quoted ; must not split the target and hide a ../.. escape: {quoted}"
                );
            }
        });
    }

    /// CALL-SITE AUDIT. Every test above calls `boundary_denial` DIRECTLY, so all of them stay
    /// green if someone deletes the call from `run_gate_hook` — I verified that by deleting it, and
    /// nothing failed. That is the third time this campaign has hit the same gap (FINDING-091's
    /// first guard, FINDING-093's), so assert the wiring, not just the helper.
    #[test]
    fn run_gate_hook_actually_consults_the_boundary() {
        let src = include_str!("gate_hook.rs");
        let body = src
            .split("pub fn run_gate_hook")
            .nth(1)
            .and_then(|b| b.split("\npub ").next())
            .expect("run_gate_hook is still a top-level fn");
        assert!(
            body.contains("boundary_denial("),
            "run_gate_hook no longer consults the filesystem boundary — the helper is live and \
             unreachable, which is indistinguishable from having no boundary at all (FINDING-098)"
        );
    }

    /// The other half nothing detected: the launcher must ARM the roots. Without this the boundary
    /// is configured nowhere, `allowed_roots_from_env` returns None, and every path is unjudged —
    /// silently, because "no boundary configured" is a legitimate state for standalone runs.
    #[test]
    fn the_launcher_arms_the_write_root() {
        let launcher = include_str!("execute_wrapped.rs");
        assert!(
            launcher.contains("WRITE_ROOTS_ENV"),
            "execute_wrapped no longer sets {WRITE_ROOTS_ENV} on the governed child, so no \
             governed unit has a filesystem boundary (FINDING-045/098)"
        );
        // Armed from the WORKTREE first, widened ONLY by the launcher-declared roots on the
        // governance context (core#259, validated at launch against the pin tree). Pointing the
        // base anywhere wider — the repo root, the home dir — or sourcing extras from anything
        // the WORKER controls would permit the escape this exists to stop.
        assert!(
            launcher.contains("vec![cwd.as_os_str().to_os_string()]"),
            "the write-root list must START from the unit's worktree; a wider base passes the \
             presence check and still allows the governance pin to be rewritten"
        );
        assert!(
            launcher.contains("armed_write_roots(&cwd, &g.extra_write_roots)"),
            "the ONLY widening must be the launch-validated extra_write_roots riding the \
             governance context — not env, not the unit, not the workflow def"
        );
        // The launch side must actually judge those extras — remove the validation and the
        // widening becomes an unvetted door straight past FINDING-098.
        let launch = include_str!("actor.rs");
        assert!(
            launch.contains("validate_extra_write_roots"),
            "the launch path no longer validates extra write roots against the pin tree"
        );
    }

    /// Review caught a FAIL-OPEN here: `std::env::var` returns `NotUnicode` for a non-UTF-8 path,
    /// which made `allowed_roots_from_env` answer "no boundary configured" — so the control silently
    /// applied to nothing on exactly the paths an attacker would choose. The launcher sets the root
    /// from an `OsStr`, so the round trip has to be OsString-clean.
    ///
    /// Falsified by restoring `var`: on unix this fails, because the non-UTF-8 root vanishes and the
    /// escape is then permitted.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_worktree_still_has_a_boundary() {
        use std::os::unix::ffi::OsStrExt;
        let raw = std::ffi::OsStr::from_bytes(b"/tmp/wicked-\xff-wt");
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(WRITE_ROOTS_ENV, raw);
        std::env::remove_var(READ_ROOTS_ENV);
        let roots = allowed_roots_from_env();
        std::env::remove_var(WRITE_ROOTS_ENV);
        let roots = roots.expect("a non-UTF-8 root must still configure a boundary, not vanish");
        assert_eq!(roots.write.len(), 1, "the root must survive the round trip");
        assert_eq!(
            roots.write[0].as_os_str().as_bytes(),
            b"/tmp/wicked-\xff-wt"
        );
    }

    fn dirs_config_workflow() -> String {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        format!("{home}/.config/wicked-core/workflows/domain-extraction.json")
    }
}
