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
    open_store_ro, ConformanceClaim, Decision, GraphRead, GraphStore, HardenedCommand, NodeKind,
    ToNode, CONFORMANCE_CLAIM,
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

/// (DES-TEAMING-002 T3) Environment variable carrying the unit's phase-CATALOG id (`review`, …)
/// to the gate-hook subprocess — the third `applies_to` alias for a catalog-composed unit, whose
/// phase id is the plan author's. Unset ⇒ no catalog alias (absence never widens).
pub const GATE_CATALOG_ENV: &str = "WICKED_GATE_CATALOG";

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

/// (D-7, DES-L4 PR-⑦) Garden's EXISTING read-only switch for the estate shim
/// (`scripts/_estate_client.py` `READONLY_ENV`): when set, the shim spawns `wicked-estate-mcp
/// --readonly` whether or not the caller passed `--readonly`. Both carriers set it to `1` on EVERY
/// worker child (units and chats) now that the CLI-registered estate MCP — whose `--readonly` process
/// flag used to be the read-only default — is no longer handed; the estate fence's `--readonly` token
/// check stays the audit. Not in `ENGINE_INTERNAL_ENV`, so it survives `hardened()` to the shim.
pub const ESTATE_READONLY_ENV: &str = "WICKED_ESTATE_READONLY";

/// (issue #463) The environment variables whose presence PINS the store an estate shim / MCP read
/// resolves — the second half of the shim allow rule (`--readonly` AND a pinned store, DES-GROUNDING-001
/// §7.1). BOTH launchers re-set [`ESTATE_DB_ENV`] on a worker whose run has a vouched-for graph
/// (`execute_wrapped::arm_worker_estate_channel`; the ACP `build_cmd`, DES-L4 PR-⑦ — the one graph
/// hand-off since the CLI-registered estate MCP was deleted) after `hardened()` stripped it; `WICKED_HOME` /
/// `WICKED_MEMORY_DB` pin the memory + knowledge stores garden's `mem` backend reads and reach the
/// worker on BOTH carriers by plain inheritance (neither is in `ENGINE_INTERNAL_ENV`).
pub(crate) const ESTATE_STORE_PIN_ENV: [&str; 3] =
    [ESTATE_DB_ENV, "WICKED_HOME", "WICKED_MEMORY_DB"];

/// The env arm of the store-pin fact — read off the hook SUBPROCESS's own environment, which is
/// the worker's (the hook is its grandchild), exactly as [`write_posture_from_env`] reads the posture.
fn estate_store_pinned_from_env() -> bool {
    ESTATE_STORE_PIN_ENV
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

/// The store-pin fact for the IN-PROCESS carrier (the ACP permission bridge): what the AGENT child
/// inherits is the daemon's environment minus `hardened()`'s strip list — so a pin the daemon holds
/// under a stripped name ([`ESTATE_DB_ENV`]) never reaches the child and must not count. Evaluated on
/// the runner at boundary construction, where the daemon env IS the child's parent environment.
pub(crate) fn estate_store_pinned_for_child() -> bool {
    ESTATE_STORE_PIN_ENV
        .iter()
        .filter(|k| !wicked_apps_core::spawn::ENGINE_INTERNAL_ENV.contains(k))
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

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
/// definitions, language runtimes, package caches) — see [`crate::path_policy`] — plus the
/// LAUNCH-declared `extra_read_roots` (core#294, validated at launch like the write extras). A
/// boundary that breaks every real run gets switched off, and one that is off is worse than none
/// because it is believed.
pub const READ_ROOTS_ENV: &str = "WICKED_READ_ROOTS";

/// Set to `1` by the launcher when the governed unit's backing phase is a PRE-BUILD, non-creator
/// rung ([`crate::domain::WorkUnit::pre_build_scope`]) — the flag that turns the phase's declared
/// scope from prose into a gate (core#296). Rides env for exactly the reason
/// [`WRITE_ROOTS_ENV`] does: the hook is a grandchild of the worker CLI, so its own environment is
/// the only channel that reaches it.
///
/// UNSET is the honest "not a pre-build phase / no phase scope declared" state, which is also what
/// a standalone `gate-hook` invocation sees. Parsed STRICTLY (`1`/`true`) rather than
/// "any non-empty value": the launcher sets exactly `1`, and an inherited junk value must not
/// silently scope a build phase away from building — which would be the inverse failure, and a
/// louder one than the one this closes.
pub const PRE_BUILD_SCOPE_ENV: &str = "WICKED_PRE_BUILD_SCOPE";

/// The PURE half of the [`PRE_BUILD_SCOPE_ENV`] read, so the parse can be tested without mutating
/// process-global env (which Rust's threaded test runner shares with every other test, and with any
/// subprocess they spawn).
fn parse_pre_build_scope(raw: Option<&std::ffi::OsStr>) -> bool {
    raw.and_then(std::ffi::OsStr::to_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("1") || s.eq_ignore_ascii_case("true"))
}

/// Read [`PRE_BUILD_SCOPE_ENV`] off the hook subprocess's own environment.
fn pre_build_scope_from_env() -> bool {
    parse_pre_build_scope(std::env::var_os(PRE_BUILD_SCOPE_ENV).as_deref())
}

/// Set by the launcher to the governed unit's WRITE POSTURE when that posture fences writes
/// ([`crate::write_posture::WritePosture::env_value`], F-036 / F-4R2-004): `1` for an
/// `executes_code: false` phase that does not play creator (an evaluator, a recon rung, a review —
/// the SAME spelling the hook parsed before postures existed, so a same-version pre-posture hook
/// binary still reads an evaluator's fence as ON; `true` and the label `read-only` also parse),
/// `deliverable-roots` for a BOUND creator that declared `executes_code: false` (its deliverables
/// live in the run's declared write roots, never in the tree under review; a pre-posture hook
/// reads this as no fence — guard-only — never as a refused deliverable). Same carrier, same
/// strict parse and same UNSET-is-honest rule as [`PRE_BUILD_SCOPE_ENV`]: unset means no phase
/// fence — a build phase must be free to write code, and an inherited junk value must never scope
/// it away from that.
pub const NO_CODE_SCOPE_ENV: &str = "WICKED_NO_CODE_SCOPE";

/// Read [`NO_CODE_SCOPE_ENV`] off the hook subprocess's own environment.
fn write_posture_from_env() -> crate::write_posture::WritePosture {
    crate::write_posture::WritePosture::parse_env(std::env::var_os(NO_CODE_SCOPE_ENV).as_deref())
}

/// Set by the launcher, alongside a fenced [`NO_CODE_SCOPE_ENV`], to the ADMITTED out-of-tree write
/// roots of a FENCED posture ([`crate::write_posture::admitted_roots`], DES-L4 PR-②): the creator's
/// launch-validated `extra_write_roots` under `deliverable-roots`, or the evaluator's NOTES ROOT
/// under the read-only posture — PATH-separator-joined like [`WRITE_ROOTS_ENV`]. Same variable for
/// both postures, no new env. Carried separately from the write roots on purpose (independent
/// review of #444, F-02): the filesystem boundary's write set is cwd + extras + the repo-graph key
/// dir, and a fenced unit's admitted writes are the extras / notes root alone — judging "inside an
/// admitted root" off the write set would admit the graph dir here and refuse it on the ACP
/// carrier. Unset or empty ⇒ no roots ⇒ every fenced write is refused (fail closed).
pub const DELIVERABLE_ROOTS_ENV: &str = "WICKED_DELIVERABLE_ROOTS";

/// Read [`DELIVERABLE_ROOTS_ENV`] off the hook subprocess's own environment.
fn deliverable_roots_from_env() -> Vec<std::path::PathBuf> {
    crate::write_posture::parse_deliverable_roots_env(
        std::env::var_os(DELIVERABLE_ROOTS_ENV).as_deref(),
    )
}

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

/// The hook-subprocess boundary check WITHOUT a shell-cwd sidecar — the spelling the boundary
/// tests exercise (judged from the process cwd, as before the install fence became stateful).
#[cfg(test)]
fn boundary_denial_untracked(context: &serde_json::Value, tool: &str) -> Option<(String, bool)> {
    let roots = allowed_roots_from_env()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let cfg = std::env::var_os("CLAUDE_CONFIG_DIR").and_then(|v| valid_config_home(&v));
    boundary_denial_with(&roots, &cwd, home.as_deref(), cfg.as_deref(), context, tool)
}

/// Refuse a path-bearing tool call that reaches outside the unit's boundary (FINDING-045/098) —
/// the hook-subprocess arm: env-carried roots, the process cwd, `$HOME`, the worker's own
/// `CLAUDE_CONFIG_DIR` — with the install fence's per-attempt shell-cwd sidecar (review of #456,
/// F1). Returns `(reason, fatal)`; see [`boundary_denial_with`] for the pure check and the
/// advisory/fatal rule. The process cwd is the seat's LAUNCH cwd (the worktree) on every
/// call — Claude Code's Bash tool keeps its own shell cwd between calls — so the sidecar, not the
/// process, says where the seat's shell stands.
fn boundary_denial(
    context: &serde_json::Value,
    tool: &str,
    install_state: &Path,
) -> Option<(String, bool)> {
    let roots = allowed_roots_from_env()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let cfg = std::env::var_os("CLAUDE_CONFIG_DIR").and_then(|v| valid_config_home(&v));
    boundary_denial_tracked(
        &roots,
        &cwd,
        home.as_deref(),
        cfg.as_deref(),
        context,
        tool,
        Some(install_state),
        estate_store_pinned_from_env(),
    )
}

/// Validate a raw `CLAUDE_CONFIG_DIR` value before it may steer the agent-state carve-out
/// (core#272, Copilot). The value is trusted input from the worker's environment: empty or
/// relative values would resolve against the judgement cwd, and a filesystem root (`/`, `C:\`)
/// would make the ADVISORY carve-out swallow every out-of-boundary write. Fail closed —
/// an invalid override is ignored and the carve-out falls back to `home/.claude`.
pub(crate) fn valid_config_home(raw: &std::ffi::OsStr) -> Option<std::path::PathBuf> {
    let p = std::path::PathBuf::from(raw);
    // `parent() == None` exactly for roots and prefixes on both families.
    (p.is_absolute() && p.parent().is_some()).then_some(p)
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
    /// The RESOLVED Claude config home for the agent-memory carve-out (core#272): an operator
    /// running with `CLAUDE_CONFIG_DIR=~/alt-configs/.claude` keeps their memory there, and the
    /// core#235 carve-out hardcoded to `home/.claude` turned that same benign write class fatal
    /// again. `None` ⇒ fall back to `home/.claude`.
    pub claude_config_dir: Option<std::path::PathBuf>,
    /// The unit's PHASE SCOPE (core#296): TRUE when this unit's backing phase is a PRE-BUILD,
    /// non-creator rung, carried from [`crate::domain::WorkUnit::pre_build_scope`]. It rides HERE,
    /// alongside the filesystem roots, because it is the same KIND of fact — a boundary the unit
    /// was given, judged before any policy — and because the in-process carrier has no env to read
    /// it from (the daemon's environment belongs to the daemon; that asymmetry is what core#260
    /// closed for the roots and this field closes for the scope).
    ///
    /// FALSE is the honest default for every other phase: a build phase must write code, and a
    /// boundary that scoped the creator away from creating would be a worse bug than the one the
    /// scope exists to stop.
    pub pre_build_scope: bool,
    /// The unit's WRITE POSTURE (F-036 / F-4R2-004): derived at the carrier from the unit's role,
    /// its `worktree_guarded` marker and whether the run has a tree
    /// ([`crate::write_posture::WritePosture::of`]). Rides here for the same reason
    /// `pre_build_scope` does; the subprocess carrier reads [`NO_CODE_SCOPE_ENV`].
    pub write_posture: crate::write_posture::WritePosture,
    /// The roots a `DeliverableRoots` creator may write — EXACTLY the run's `extra_write_roots`
    /// ([`crate::write_posture::admitted_roots`]), the same list the ACP fence judges (F-02).
    /// Empty for every other posture. The subprocess carrier reads [`DELIVERABLE_ROOTS_ENV`].
    pub deliverable_roots: Vec<std::path::PathBuf>,
    /// (issue #463) Whether the worker's environment PINS the estate store its shim / MCP reads
    /// resolve ([`ESTATE_STORE_PIN_ENV`]) — the second half of the shim allow rule. Rides here for
    /// the same reason `pre_build_scope` does: the in-process carrier has no worker env to read it
    /// from ([`estate_store_pinned_for_child`] derives it on the runner); the subprocess carrier
    /// reads its own environment ([`estate_store_pinned_from_env`]).
    pub estate_store_pinned: bool,
}

/// Is `resolved` inside a SYSTEM temp dir? The advisory carve-out set for scratch writes
/// (core#264): `std::env::temp_dir()` — on macOS `$TMPDIR` (`/var/folders/…`) — plus the literal
/// `/tmp`, which macOS symlinks to `/private/tmp` and which is exactly where run fc46a3a1's
/// worker wrote its scratch (temp_dir() alone would have missed it). Same symlink-aware
/// containment as every other boundary comparison.
fn in_system_temp(resolved: &std::path::Path) -> bool {
    // ONE temp_dir() read for both the exclusion and the inclusion set — two reads could
    // diverge under a concurrently-mutated temp env and make the gov-root exclusion judge a
    // different tree than the carve-out (Copilot).
    let sys_temp = std::env::temp_dir();
    // The governance evidence tree ALSO lives under temp (`$TMPDIR/wicked-core-gov/…` — the
    // decisions logs the fold reads). A blocked write there is a tamper attempt against the
    // audit trail, not scratch: it must NOT ride the scratch carve-out. (It is still blocked
    // either way; this keeps it unit-FATAL.)
    if crate::path_policy::resolved_is_within(resolved, &sys_temp.join("wicked-core-gov")) {
        return false;
    }
    let mut temps = vec![sys_temp];
    if cfg!(unix) {
        temps.push(std::path::PathBuf::from("/tmp"));
    }
    temps
        .iter()
        .any(|t| crate::path_policy::resolved_is_within(resolved, t))
}

/// The pure boundary judgement both carriers share: roots, cwd AND home are PARAMETERS, never
/// ambient process state, so the wrapped subprocess (env-armed) and the in-process ACP bridge
/// (context-armed, core#260) cannot diverge on what "outside the boundary" means.
///
/// Judged WITHOUT an ambient estate-store pin (issue #463): a shim / MCP call must carry its pin
/// on argv here — the STRICT spelling the boundary tests exercise. Every production carrier —
/// the governed ones and, since DES-L5 (R16c), the chat boundary — passes its pin fact through
/// [`boundary_denial_tracked`] instead, so this spelling is test-only.
#[cfg(test)]
pub(crate) fn boundary_denial_with(
    roots: &crate::path_policy::AllowedRoots,
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    claude_config_dir: Option<&std::path::Path>,
    context: &serde_json::Value,
    tool: &str,
) -> Option<(String, bool)> {
    boundary_denial_tracked(
        roots,
        cwd,
        home,
        claude_config_dir,
        context,
        tool,
        None,
        false,
    )
}

/// The sidecar of the attempt's decisions log that carries the seat's shell cwd as the install
/// fence tracks it across tool calls (review of #456, F1): `<attempt dir>/install-fence-cwd-<phase>`.
/// Attempt-scoped by its directory, so a retry starts back at the worktree; phase-scoped so two
/// units of one attempt never share a shell.
pub(crate) fn install_fence_cwd_path(decisions_path: &str, phase: &str) -> std::path::PathBuf {
    let safe: String = phase
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Path::new(decisions_path).with_file_name(format!("install-fence-cwd-{safe}"))
}

/// The tracked shell cwd, or `None` when no allowed call has moved it yet (⇒ the worktree).
fn read_install_fence_cwd(state: Option<&Path>) -> Option<std::path::PathBuf> {
    let raw = std::fs::read_to_string(state?).ok()?;
    let t = raw.trim();
    (!t.is_empty()).then(|| std::path::PathBuf::from(t))
}

/// Persist where the seat's shell stands after an ALLOWED Bash call (its trailing `cd`/`pushd`
/// applied to the tracked cwd), so the next call is judged from there. Called by
/// [`evaluate_tool_call`] on its allow exit only — a refused call never ran.
pub(crate) fn track_install_fence_cwd(
    command: &str,
    worktree: &Path,
    home: Option<&Path>,
    state: &Path,
) {
    let here = read_install_fence_cwd(Some(state)).unwrap_or_else(|| worktree.to_path_buf());
    let after = crate::install_fence::judge_from(command, worktree, &here, home).here_after;
    if after == worktree {
        // Back at (or never left) the worktree — the absence of the file says the same.
        let _ = std::fs::remove_file(state);
        return;
    }
    if let Some(parent) = state.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(state, after.to_string_lossy().as_bytes());
}

/// Path of the write-root witness sidecar (issue #541 criterion 3):
/// `<decisions-dir>/write-root-witness-<safe-phase>`. Mirrors `install_fence_cwd_path`.
pub(crate) fn write_root_witness_path(decisions_path: &str, phase: &str) -> std::path::PathBuf {
    let safe: String = phase
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Path::new(decisions_path).with_file_name(format!("write-root-witness-{safe}"))
}

/// FNV-1a fingerprint of the admitted write-root trees: sorted (path, size, mtime-secs) tuples.
/// Detects any file creation, deletion, or content/metadata change a Bash call could cause.
/// `DefaultHasher` is deliberately avoided (it is not stable across Rust versions).
#[cfg(test)]
pub(crate) fn fingerprint_write_roots(roots: &[std::path::PathBuf]) -> u64 {
    const FNV_OFFSET: u64 = 14695981039346656037;
    const FNV_PRIME: u64 = 1099511628211;
    let mut entries: Vec<(String, u64, u64)> = Vec::new();
    for root in roots {
        let _ = collect_dir_entries_for_witness(root, &mut entries);
    }
    entries.sort_unstable();
    let mut hash = FNV_OFFSET;
    for (path, size, mtime) in &entries {
        for b in path.bytes() {
            hash ^= b as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash ^= size;
        hash = hash.wrapping_mul(FNV_PRIME);
        hash ^= mtime;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn collect_dir_entries_for_witness(
    dir: &std::path::Path,
    out: &mut Vec<(String, u64, u64)>,
) -> CollectorKind {
    // For git-managed roots (directories that ARE a git worktree root, identified by the
    // presence of a `.git` entry), use `git ls-files` so .gitignore is respected — target/,
    // .git/, node_modules/, dist/ never appear and an allowed call's own build side-effects
    // cannot read as an escape (INDEPENDENT REVIEW item 2). Only check when `.git` is present
    // so that plain temp directories inside an outer git repo do not accidentally inherit the
    // outer repo's file list via git's upward root search.
    if dir.join(".git").exists() {
        if let Ok(output) = std::process::Command::new("git")
            .hardened()
            .args([
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ])
            .current_dir(dir)
            .output()
        {
            if output.status.success() {
                for rel in output.stdout.split(|&b| b == 0).filter(|s| !s.is_empty()) {
                    let Ok(rel_str) = std::str::from_utf8(rel) else {
                        continue;
                    };
                    let abs = dir.join(rel_str);
                    let Ok(meta) = std::fs::metadata(&abs) else {
                        continue;
                    };
                    if meta.is_file() {
                        let mtime = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        out.push((abs.to_string_lossy().into_owned(), meta.len(), mtime));
                    }
                }
                return CollectorKind::GitLsFiles;
            }
        }
    }
    // Fallback for non-git roots (or when git ls-files fails): raw recursive walk
    // skipping .git directories.
    collect_dir_entries_raw(dir, out);
    CollectorKind::RawWalk
}

fn collect_dir_entries_raw(dir: &std::path::Path, out: &mut Vec<(String, u64, u64)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push((path.to_string_lossy().into_owned(), meta.len(), mtime));
        if meta.is_dir() {
            collect_dir_entries_raw(&path, out);
        }
    }
}

/// Which filesystem enumeration strategy produced a `WitnessSnapshot`.
///
/// Stored in the sidecar so that a snapshot taken with `git ls-files` and re-checked when `.git`
/// is unavailable (forcing a raw walk) can be detected as a collector mismatch and re-snapshotted
/// rather than diffed — the two strategies enumerate different file sets, so a diff would produce
/// false positives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollectorKind {
    /// `git ls-files` succeeded for every root.
    GitLsFiles,
    /// Raw recursive directory walk (`.git` not found or `git ls-files` failed for any root).
    RawWalk,
}

/// Sidecar payload for the post-hoc write-root witness (issue #541, items 1-3 of the review):
/// the watched write roots (write set minus notes root), the collector kind, and the sorted entry
/// list at snapshot time, so a mismatch can name the changed paths rather than only reporting a
/// hash difference.
#[derive(Debug, Clone)]
pub(crate) struct WitnessSnapshot {
    pub roots: Vec<std::path::PathBuf>,
    /// Which strategy enumerated the entries. When the current collector differs from the stored
    /// one, re-snapshot instead of diffing to avoid false positives.
    pub collector: CollectorKind,
    pub entries: Vec<(String, u64, u64)>, // (path, size_bytes, mtime_secs)
}

/// Read the stored write-root witness snapshot (`None` on first call or missing sidecar).
fn read_write_root_witness(path: &std::path::Path) -> Option<WitnessSnapshot> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
    let roots = v
        .get("roots")?
        .as_array()?
        .iter()
        .filter_map(|r| r.as_str().map(std::path::PathBuf::from))
        .collect();
    // Missing "collector" key in old sidecars → RawWalk (conservative: forces re-snapshot,
    // never a false deny).
    let collector = match v.get("collector").and_then(|c| c.as_str()) {
        Some("git_ls_files") => CollectorKind::GitLsFiles,
        _ => CollectorKind::RawWalk,
    };
    let entries = v
        .get("entries")?
        .as_array()?
        .iter()
        .filter_map(|e| {
            let arr = e.as_array()?;
            let path = arr.first()?.as_str()?.to_string();
            let size = arr.get(1)?.as_u64()?;
            let mtime = arr.get(2)?.as_u64()?;
            Some((path, size, mtime))
        })
        .collect();
    Some(WitnessSnapshot {
        roots,
        collector,
        entries,
    })
}

/// Persist the write-root witness snapshot for the next gate-hook invocation to compare against.
fn write_write_root_witness(path: &std::path::Path, snapshot: &WitnessSnapshot) {
    let roots_json: Vec<serde_json::Value> = snapshot
        .roots
        .iter()
        .map(|r| serde_json::Value::String(r.to_string_lossy().into_owned()))
        .collect();
    let entries_json: Vec<serde_json::Value> = snapshot
        .entries
        .iter()
        .map(|(p, s, m)| serde_json::json!([p, s, m]))
        .collect();
    let collector_str = match snapshot.collector {
        CollectorKind::GitLsFiles => "git_ls_files",
        CollectorKind::RawWalk => "raw_walk",
    };
    let payload = serde_json::json!({
        "roots": roots_json,
        "collector": collector_str,
        "entries": entries_json,
    });
    let _ = std::fs::write(path, payload.to_string());
}

/// Compute the write-root witness roots: the unit's write set minus the notes root.
///
/// A ReadOnly evaluator may write to its notes root — that is an EXPECTED write, not a fence
/// violation. The witness fingerprints everything ELSE in the write set (the tree and any extra
/// roots) so a mutation there proves an unwanted write escaped the phase-scope fence (item 1).
fn compute_witness_roots(boundary: Option<&BoundaryCtx>) -> Vec<std::path::PathBuf> {
    let (write_roots, notes_roots) = match boundary {
        Some(b) => (b.roots.write.clone(), b.deliverable_roots.clone()),
        None => (
            allowed_roots_from_env()
                .map(|r| r.write)
                .unwrap_or_default(),
            deliverable_roots_from_env(),
        ),
    };
    let notes_root = notes_roots.first().cloned();
    write_roots
        .into_iter()
        .filter(|r| notes_root.as_ref() != Some(r))
        .collect()
}

/// Diff two sorted entry lists and return the paths that changed (created, deleted, or modified).
fn diff_witness_entries(old: &[(String, u64, u64)], new: &[(String, u64, u64)]) -> Vec<String> {
    use std::collections::HashMap;
    let old_map: HashMap<&str, (u64, u64)> =
        old.iter().map(|(p, s, m)| (p.as_str(), (*s, *m))).collect();
    let new_map: HashMap<&str, (u64, u64)> =
        new.iter().map(|(p, s, m)| (p.as_str(), (*s, *m))).collect();
    let mut changed: Vec<String> = Vec::new();
    for (path, &val) in &new_map {
        if old_map.get(path) != Some(&val) {
            changed.push((*path).to_string());
        }
    }
    for path in old_map.keys() {
        if !new_map.contains_key(path) {
            changed.push((*path).to_string());
        }
    }
    changed.sort_unstable();
    changed
}

/// [`boundary_denial_with`] judging the install fence from the cwd persisted at `install_state`
/// (the wrapped carrier's per-attempt sidecar), or from `cwd` when there is none, and the estate
/// shim rule (issue #463) with `estate_store_pinned` — whether the WORKER's environment pins the
/// store ([`ESTATE_STORE_PIN_ENV`]); a parameter, like the roots, so both carriers judge alike.
#[allow(clippy::too_many_arguments)]
pub(crate) fn boundary_denial_tracked(
    roots: &crate::path_policy::AllowedRoots,
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    claude_config_dir: Option<&std::path::Path>,
    context: &serde_json::Value,
    tool: &str,
    install_state: Option<&std::path::Path>,
    estate_store_pinned: bool,
) -> Option<(String, bool)> {
    // Path-bearing tools (Write/Edit/NotebookEdit/Read): the direct path check.
    if let Some(path) = context
        .get("path")
        .and_then(serde_json::Value::as_str)
        .filter(|p| !p.is_empty())
    {
        let is_write = WRITE_TOOLS.contains(&tool);
        if let Err(d) = crate::path_policy::check(path, roots, is_write, cwd, home) {
            // A blocked WRITE is unit-FATAL — EXCEPT into two benign trees, where it stays
            // BLOCKED and AUDITED but must not ABORT the run (advisory, exactly as a blocked
            // read is, core#219):
            //  - the worker's OWN Claude Code state tree (`~/.claude/**`, e.g. project memory)
            //    — a governed claude worker routinely writes there (core#235; infigraph died
            //    over it);
            //  - the SYSTEM temp dirs (core#264) — `> /tmp/x`, `mktemp`, `tee /tmp/log` are
            //    everyday scratch idioms; run fc46a3a1 produced its deliverable correctly and
            //    was then failed over one blocked `/private/tmp` redirect. Nothing lands (the
            //    block holds), and a scratch file in temp is neither work exfiltration nor a
            //    pin rewrite.
            // The carve-outs are scoped: the gate pin (`~/.config/wicked-core/**`), the home
            // dir, and the operational store are under NEITHER tree and stay fatal, so the
            // FINDING-098 pin-rewrite escape is untouched.
            // The agent-state tree is the RESOLVED config home (core#272): CLAUDE_CONFIG_DIR
            // when the operator runs an alternate home, else `home/.claude` (core#235).
            let state_tree: Option<std::path::PathBuf> = claude_config_dir
                .map(std::path::Path::to_path_buf)
                .or_else(|| home.map(|h| h.join(".claude")));
            let fatal = is_write
                && !state_tree
                    .is_some_and(|t| crate::path_policy::resolved_is_within(&d.resolved, &t))
                && !in_system_temp(&d.resolved);
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
            // REMOTE-WRITE FENCE (F-7R2-012, wave 6): a `git push` / `gh pr create|merge|edit|
            // comment` / `gh api` mutation / `gh release` from a worker seat is refused whatever
            // path it names — delivery is the deliver phase's job. Judged on every segment of the
            // command (`cd x && git -C x push`, `sh -c 'gh pr create …'`), not on its prefix, so
            // the claude deny rules' blind spots are covered on the carriers that see the text.
            // ADVISORY (`fatal: false`): blocked and audited, the seat is told the remedy and
            // continues — the unit is not failed for asking. `evaluate_tool_call` recognises the
            // reason's prefix and records it under its own claim id so the fold can disclose
            // `workerToolCallDenied` with the command and the remedy.
            if let Some(hit) = crate::remote_write_fence::remote_write_command(command) {
                return Some((hit.reason(), false));
            }
            // INSTALL FENCE (F-E2E-029): a package-manager install whose effective directory —
            // after `cd`/`pushd` and the manager's own `--prefix`/`-C`/`--cwd` — leaves the unit's
            // worktree (`cwd`) is refused the same advisory way, and recorded so the fold discloses
            // it as `workerToolCallDenied` with its remedy.
            let here = read_install_fence_cwd(install_state).unwrap_or_else(|| cwd.to_path_buf());
            if let Some(hit) = crate::install_fence::judge_from(command, cwd, &here, home).hit {
                return Some((hit.reason(), false));
            }
            for target in bash_write_targets(command) {
                if let Err(d) = crate::path_policy::check(&target, roots, true, cwd, home) {
                    // A shell write into the SYSTEM temp is the same benign-scratch class as
                    // the direct-tool arm above (core#264): still blocked, still audited,
                    // advisory. Everything else a shell redirect reaches stays unit-fatal.
                    let fatal = !in_system_temp(&d.resolved);
                    return Some((format!("Bash write leaves the unit boundary: {d}"), fatal));
                }
            }
            // issue #540: `cd <outside> && <write>` — the cd moves the shell's runtime cwd, so
            // relative write targets in subsequent segments land in the cd destination, not in the
            // process cwd (the worktree). Check the cd destination against the boundary so the
            // escape is caught before the write segments run.  Unit-fatal like any write escape.
            for target in bash_cd_escape_targets(command) {
                if let Err(d) = crate::path_policy::check(&target, roots, true, cwd, home) {
                    let fatal = !in_system_temp(&d.resolved);
                    return Some((format!("Bash `cd` leaves the unit boundary: {d}"), fatal));
                }
            }
            // ESTATE COMMAND FENCE (DES-GROUNDING-001 §7, issue #463): guard write-path access to
            // the SHARED project graph from a governed unit's Bash. Read-only `wicked-estate`
            // subcommands, and the estate stdio shim / `wicked-estate-mcp` run with `--readonly`
            // AND a pinned store, are ALLOWED — the grounding transport; write subcommands and any
            // shim / MCP invocation missing either half are DENIED, with the reason naming the
            // segment and why. `evaluate_tool_call` inspects the posture (recon/pre-build vs
            // code-executing) to decide advisory vs fatal; the `false` placeholder here is never
            // read for estate denies. Same defense-in-depth limit as bash_write_targets (a renamed
            // binary / raw SQLite still evades a literal scan; the OS sandbox is the hermetic layer).
            if let Some(hit) = classify_estate_command(command, estate_store_pinned) {
                return Some((
                    format!(
                        "{ESTATE_DENY_REASON_PREFIX} `{}` — {}; {ESTATE_DENY_REMEDY}",
                        hit.segment, hit.why
                    ),
                    false, // placeholder — posture-based advisory/fatal decided in evaluate_tool_call
                ));
            }
        }
    }

    None
}

/// Refuse a PRE-BUILD phase's write to a NON-DOCUMENTATION path (core#296).
///
/// # The gap this closes
///
/// A pre-build, non-creator phase (`feature`'s `clarify`/`design`, `bug`'s `triage`/`reproduce`, …)
/// declares a scope: produce the analysis/design/plan, leave implementation to the build phase. That
/// scope shipped as a PROMPT — a preamble that even called itself `(enforced)`. On run `d1bc72c2`
/// the `design` unit wrote `src/board/attentionReason.ts` and `tests/attentionReason.test.ts`
/// *before* the creator phase ran; the hook SAW both calls and returned `allow`, because the only
/// path question it asked was "is this inside the unit's roots" — and the worktree is inside them,
/// so `allow` was the correct answer to the wrong question. Phase scope is a SECOND boundary,
/// orthogonal to the filesystem one: *where* may this unit write is answered by
/// [`boundary_denial_with`]; *what kind of file* may THIS PHASE write is answered here.
///
/// # Why it is shaped this way
///
/// It rides the machinery that already ships rather than growing the `wicked-governance`
/// Policy/Trigger vocabulary, which would need a new trigger kind to express "phase role × path
/// class" and would put an engine invariant into operator-editable policy data. The inputs are both
/// existing: [`crate::domain::WorkUnit::pre_build_scope`] (def-derived at plan time) and
/// [`crate::actor::is_documentation_change`] (the core#283 heuristic).
///
/// # Scope of the claim
///
/// This covers the path-bearing WRITE tools ([`WRITE_TOOLS`]) and nothing else — the same honest
/// limit [`crate::path_policy`] states for itself. A `Bash` heredoc or a `git apply` still gets
/// through, which is why `actor::phase_scope_warning` stays live as the completion-path backstop.
/// A read is never refused: a design phase must read the code it is designing against.
///
/// Returns the operator-facing reason, or `None` when the call is in scope.
/// Reduce a tool-call path to the form [`crate::actor::is_documentation_change`] was written for:
/// a path RELATIVE to the unit's worktree.
///
/// This is load-bearing, not tidiness. That predicate asks whether ANY directory segment is `docs`
/// or `.product`, because git hands it repo-relative paths. The gate hands it the ABSOLUTE
/// tool-call path, so every ancestor of the worktree got judged too — and a checkout living under
/// any directory named `docs` or `.product` silently turned the whole phase-scope gate OFF:
///
///   /Users/me/docs/projects/studio/src/board/attentionReason.ts  -> "documentation", allowed
///   /home/ci/.product/checkout/src/lib.rs                        -> "documentation", allowed
///
/// A gate that is off for some checkout layouts and on for others is worse than no gate, because
/// nothing says which you have. Stripping the worktree prefix first makes the answer depend only
/// on the path INSIDE the repo, which is the only part the phase actually chose.
///
/// A path outside the worktree is judged on its filename alone: the filesystem boundary has
/// already refused it, and borrowing "documentation" from an ancestor we do not own is exactly the
/// bypass above.
fn scope_relative(path: &str, cwd: &std::path::Path) -> String {
    let p = std::path::Path::new(path);
    // `is_relative()` is platform-dependent: on Windows a unix-style "/repo/docs/x.md" has no
    // drive so it counts as relative and would skip the strip/basename containment below —
    // exactly the ancestor-grant hole this gate closes. `has_root()` treats it as anchored on
    // every platform.
    if !p.has_root() {
        return path.to_string();
    }
    match p.strip_prefix(cwd) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        // Outside the worktree: keep the file name only, so no ancestor segment can grant it
        // documentation status.
        Err(_) => p
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string()),
    }
}

pub(crate) fn phase_scope_denial(
    pre_build_scope: bool,
    posture: crate::write_posture::WritePosture,
    context: &serde_json::Value,
    tool: &str,
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    deliverable_roots: &[std::path::PathBuf],
) -> Option<String> {
    use crate::write_posture::WritePosture;
    if !(pre_build_scope || posture.fences_writes()) {
        return None;
    }
    // R7/R7b (DES-L4 PR-②; core #483, F-RC1-080): `Bash` is judged by its WRITE TARGETS — a
    // redirect, heredoc (`cat > f <<EOF`), `tee`, `cp`/`mv`/`install`, `dd of=`, `mkdir` — with the
    // SAME admission a path-bearing write tool gets below (`bash_phase_scope_denial`). Before this
    // the fence saw only `Write`/`Edit`/`NotebookEdit` and a heredoc walked straight through it
    // (the guard then tripped and the unit died).
    if tool == "Bash" {
        return bash_phase_scope_denial(
            pre_build_scope,
            posture,
            context,
            cwd,
            home,
            deliverable_roots,
        );
    }
    if !WRITE_TOOLS.contains(&tool) {
        return None;
    }
    let path = context
        .get("path")
        .and_then(serde_json::Value::as_str)
        .filter(|p| !p.trim().is_empty())?;
    let rel = scope_relative(path, cwd);
    // The PRE-BUILD scope keeps its documentation allowance (core#296: a design/plan rung writes
    // its deliverable as docs). The READ-ONLY posture permits NOTHING (codex review on #414): an
    // evaluator's outputs belong outside the tree it is judging — and the worktree guard, which
    // has no exemption of any kind, would deny the write at the gate anyway; a tool call the hook
    // allows and the gate then denies is a contradiction, not a policy.
    if pre_build_scope && crate::actor::is_documentation_change(&rel) {
        return None;
    }
    // F-4R2-004: the DELIVERABLE-ROOTS posture — a BOUND creator whose phase declared
    // `executes_code: false`. Its deliverables live in EXACTLY the run's declared
    // `extra_write_roots`; a write anywhere else — the tree under review (the unit cwd) first of
    // all, but also an engine-owned directory the filesystem boundary admits (the repo-graph key
    // dir) — is refused. ONE judgement with the ACP fence (`write_posture::
    // deliverable_write_admitted`, F-02), on the boundary's own normalize → symlink-resolve →
    // containment chain, so a relative spelling, a `..` hop or a `/tmp`→`/private/tmp` alias is
    // judged on where it lands, identically on both carriers.
    if posture == WritePosture::DeliverableRoots && !pre_build_scope {
        if crate::write_posture::deliverable_write_admitted(path, cwd, home, deliverable_roots) {
            return None;
        }
        let roots = crate::write_posture::describe_deliverable_roots(deliverable_roots);
        return Some(format!(
            "phase scope: this phase plays creator and declares `executes_code: false` — its \
             deliverables belong in the run's declared write roots ({roots}), not in the tree \
             under review or anywhere else; `{tool}` to `{path}` is outside them, so it is \
             refused. Write the deliverable inside a declared root; a phase that must change the \
             tree itself declares `executes_code: true`."
        ));
    }
    // F-036: the READ-ONLY posture — a phase that declared it would not write code and does not
    // play creator (an evaluator reviewing the build, a recon rung, a review) is refused the
    // path-bearing write tools everywhere. Named as its own rule: the remedy differs (report,
    // don't edit; a phase that must change code declares `executes_code: true`). Pre-build wins
    // the wording when both apply (a pre-build phase is also a no-code one) so its established
    // message is unchanged.
    if !pre_build_scope {
        // core#464 / R7: the ONE sanctioned place a read-only unit may write is its NOTES ROOT
        // (outside the tree, minted at dispatch; `admitted_roots` hands it to both carriers) —
        // admitted by the SAME judgement the creator fence uses, so a `..` hop or a symlink alias
        // is judged where it lands. Everything else stays refused.
        if crate::write_posture::deliverable_write_admitted(path, cwd, home, deliverable_roots) {
            return None;
        }
        let notes = notes_root_remedy(deliverable_roots);
        return Some(format!(
            "phase scope: this phase declares `executes_code: false` (an evaluation/recon/review \
             phase) — `{tool}` to `{path}` would change the tree under review, so it is refused. \
             Nothing in the worktree may be written here: report findings in this phase's output\
             {notes}, and leave any change to the tree — code, docs or a report file — to a phase \
             that declares `executes_code: true`."
        ));
    }
    // A refusal the worker cannot act on is not a refusal, it is a wall (FINDING-066): name the
    // rule, the file, and the two ways forward — write the deliverable as documentation, or leave
    // the implementation to the phase whose job it is.
    Some(format!(
        "phase scope: this is a PRE-BUILD phase, whose deliverable is analysis/design/plan — \
         `{tool}` to `{path}` is production code, not documentation. Allowed here: `.md`/`.txt`/\
         `.rst` files, and anything under a `docs/` or `.product/` directory. Implementation \
         belongs to the later build phase; describe it in this phase's deliverable instead."
    ))
}

/// The remedy the fold and the ACP bridge disclose with a phase-scope Bash refusal (DES-L4 PR-②).
pub(crate) const PHASE_SCOPE_BASH_REMEDY: &str = "write notes only under the unit's notes root; a \
    phase that must change the tree declares executes_code: true";

/// `; write notes only under the unit's notes root (<root>)` when the admitted list names one,
/// empty otherwise — the clause the read-only refusal appends so the seat is told WHERE it may write.
fn notes_root_remedy(admitted: &[std::path::PathBuf]) -> String {
    match admitted.first() {
        Some(root) => format!(
            "; write notes only under the unit's notes root ({})",
            root.display()
        ),
        None => String::new(),
    }
}

/// The `Bash` arm of [`phase_scope_denial`] (R7 / R7b / R8, DES-L4 PR-②): every WRITE TARGET of the
/// command ([`bash_write_targets`]) must be admitted — inside one of the posture's admitted roots
/// (the evaluator's notes root, the creator's deliverable roots;
/// [`crate::write_posture::deliverable_write_admitted`], the same judgement the path-bearing tools
/// get) or, for a PRE-BUILD phase, a documentation path ([`crate::actor::is_documentation_change`],
/// the allowance core#296 grants `Write`). The first unadmitted target refuses the call, naming the
/// target and where a write may go. ADVISORY like every phase-scope refusal: the call is blocked, the
/// unit continues. A command with no resolvable write target, or no `command`, is not judged by the
/// target loop; under ReadOnly posture [`opaque_interpreter_denial`] catches interpreter programs
/// (`python3 -c`, `node -e`, etc.) whose write targets are unresolvable from the command text.
fn bash_phase_scope_denial(
    pre_build_scope: bool,
    posture: crate::write_posture::WritePosture,
    context: &serde_json::Value,
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    admitted_roots: &[std::path::PathBuf],
) -> Option<String> {
    let command = context
        .get("command")
        .and_then(serde_json::Value::as_str)
        .filter(|c| !c.trim().is_empty())?;
    bash_write_phase_scope(pre_build_scope, posture, command, cwd, home, admitted_roots)
}

/// The command-taking core of [`bash_phase_scope_denial`], shared with the ACP permission bridge
/// (`acp_runner::answer_permission_request`, which holds the posture and the admitted roots
/// in-process and has no JSON context to read): ONE rule on both carriers.
pub(crate) fn bash_write_phase_scope(
    pre_build_scope: bool,
    posture: crate::write_posture::WritePosture,
    command: &str,
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    admitted_roots: &[std::path::PathBuf],
) -> Option<String> {
    use crate::write_posture::WritePosture;
    // Full posture without pre-build scope: the phase is a code-executing creator — no fence of
    // any kind applies. Return early before the PRE-BUILD fallthrough in the match below.
    if matches!(posture, WritePosture::Full) && !pre_build_scope {
        return None;
    }
    for target in bash_write_targets(command) {
        if crate::write_posture::deliverable_write_admitted(&target, cwd, home, admitted_roots) {
            continue;
        }
        if pre_build_scope && crate::actor::is_documentation_change(&scope_relative(&target, cwd)) {
            continue;
        }
        let allowed_where = match posture {
            WritePosture::DeliverableRoots if !pre_build_scope => format!(
                "this phase plays creator and declares `executes_code: false`; its deliverables \
                 belong in the run's declared write roots ({})",
                crate::write_posture::describe_deliverable_roots(admitted_roots)
            ),
            WritePosture::ReadOnly if !pre_build_scope => match admitted_roots.first() {
                Some(root) => format!(
                    "this phase declares `executes_code: false` (an evaluation/recon/review \
                     phase); notes may be written ONLY under its notes root ({})",
                    root.display()
                ),
                None => "this phase declares `executes_code: false` (an evaluation/recon/review \
                         phase) and has no notes root — nothing may be written"
                    .to_string(),
            },
            _ => "this is a PRE-BUILD phase; only documentation may be written (`.md`/`.txt`/\
                  `.rst`, or under `docs/` / `.product/`)"
                .to_string(),
        };
        return Some(format!(
            "phase scope: `Bash` would write `{target}` — {allowed_where}. A shell redirect, \
             heredoc, `tee`, `cp`/`mv`/`install`, `dd`, `touch` or `mkdir` counts as a write \
             here, so the call is refused; {PHASE_SCOPE_BASH_REMEDY}."
        ));
    }
    // issue #541: under ReadOnly, also deny interpreter programs whose write targets cannot be
    // extracted from the command text. `bash_write_targets` found nothing to judge, so the loop
    // above returned without denying — but `python3 -c '...'`, `node -e '...'`, `perl -e '...'`,
    // `ruby -e '...'`, and similar shapes can write files at paths invisible to this scan.
    if matches!(posture, WritePosture::ReadOnly) {
        if let Some(reason) = opaque_interpreter_denial(command) {
            return Some(reason);
        }
    }
    if matches!(posture, WritePosture::ReadOnly) {
        if let Some(reason) = explicit_write_program_denial(command) {
            return Some(reason);
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

/// Returns `true` for a code-string token that is an absolute filesystem path.
/// Covers Unix (`/`-rooted), Windows (`C:\`, `\\?\`, UNC `\\server\share\`) via
/// `Path::is_absolute()`, and explicitly excludes `//`-prefixed tokens (URL authority
/// components left after splitting on `:`, e.g. `//127.0.0.1` from `http://127.0.0.1`).
/// INDEPENDENT REVIEW items 1 and 3a. Used only in tests (validates the rule holds).
#[cfg(test)]
fn is_abs_path_token(tok: &str) -> bool {
    !tok.starts_with("//") && std::path::Path::new(tok).is_absolute()
}

/// Best-effort extraction of the filesystem WRITE targets from a Bash command line (FINDING-045).
/// Covers the direct escapes: `>`/`>>`/`N>` redirects (spaced or glued), `tee [-a] FILE...`, the
/// destination of `cp`/`mv`/`install` (last non-flag arg), `dd of=FILE`, `touch FILE...`,
/// `mkdir DIR...`, and `git -C <path>` when paired with a write subcommand (issue #540). Sees
/// through ONE level of the fixed wrapper table ([`unwrap_program`], core #475) — `sh -lc '…'`,
/// `exec`, `xargs`, `env`, `nice`, `timeout`, a quoted program word. Deliberately NOT a shell
/// parser — see the caller's note on why this is defense-in-depth rather than a sandbox, and
/// [`classify_estate_command`]'s doc for the complete list of shapes a literal scan does not model.
pub(crate) fn bash_write_targets(command: &str) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    collect_bash_write_targets(command, true, &mut targets);
    // Drop standard shell write SINKS — writing to them discards or streams bytes, it does not place
    // a file outside the worktree, so they are not escapes. `> /dev/null` is in ~every real command
    // (the governed PageIndex pass failed on an `analyze` unit's `… > /dev/null` before this — a false
    // positive that would fail essentially every workflow). FINDING-045 is a fence against files
    // leaving the worktree, not a ban on discarding output.
    targets.retain(|t| !is_safe_write_sink(t));
    targets
}

/// The scan behind [`bash_write_targets`]. `unwrap_inline` is true for the command line the worker
/// issued and false for the ONE inner rescan of a shell `-c` string — a `-c` wrapper found INSIDE
/// that string is the documented two-level pass, not unwrapped again.
fn collect_bash_write_targets(command: &str, unwrap_inline: bool, targets: &mut Vec<String>) {
    let owned = shell_tokens(command);
    let toks: Vec<&str> = owned.iter().map(String::as_str).collect();

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
        // See through one wrapper level to the real program word (core #475). A shell `-c` string
        // is rescanned as its own command line — its redirects were quoted at this level, so the
        // redirect pass above could not see them.
        let idx = match unwrap_program(words) {
            Unwrapped::Program { idx, .. } => idx,
            Unwrapped::Inline(inner) => {
                // INDEPENDENT REVIEW item 3b: the raw token scan that was here produced FATAL
                // false denials for read commands (`sh -c 'cat /etc/hosts'` → `/etc/hosts` as
                // a write target). `unwrap_program` already re-scans the inner command via the
                // recursive call below, so the raw scan only added spurious read-path targets.
                if unwrap_inline {
                    collect_bash_write_targets(inner, false, targets);
                }
                continue;
            }
        };
        let Some(prog) = words.get(idx) else { continue };
        match program_basename(prog) {
            "cp" | "mv" | "install" => {
                if let Some(dest) = words[idx + 1..].iter().rev().find(|w| !w.starts_with('-')) {
                    targets.push((*dest).to_string());
                }
            }
            "tee" => {
                for w in &words[idx + 1..] {
                    if !w.starts_with('-') {
                        targets.push((*w).to_string());
                    }
                }
            }
            "dd" => {
                for w in &words[idx + 1..] {
                    if let Some(f) = w.strip_prefix("of=") {
                        targets.push(f.to_string());
                    }
                }
            }
            // R8 (DES-L4 PR-②, F-RC1-092): a directory is a write target too — the guard's tree
            // hash cannot see an empty dir, so `mkdir` is judged where it lands like a redirect
            // is. Feeds the filesystem boundary (outside the write roots ⇒ unit-FATAL, as a
            // redirect outside is today) and the phase-scope fence (advisory). Every non-flag
            // word is a target (`mkdir -p a b` ⇒ `a`, `b`); a `-m MODE` value is a benign
            // in-tree false target, stated not modelled.
            "mkdir" => {
                for w in &words[idx + 1..] {
                    if !w.starts_with('-') && !is_fd_dup_operator(w) {
                        targets.push((*w).to_string());
                    }
                }
            }
            // issue #541: `touch` updates or creates a file — every non-flag argument is a write
            // target (like `mkdir`). `touch -t STAMP` / `-d DATE` take values, but those values
            // are timestamps not paths, so the non-flag guard below captures them as false positives
            // only when they look like paths — which is a tolerable over-deny vs. a missed escape.
            "touch" => {
                for w in &words[idx + 1..] {
                    if !w.starts_with('-') && !is_fd_dup_operator(w) {
                        targets.push((*w).to_string());
                    }
                }
            }
            // issue #540: `git -C <path> <write-subcommand>` writes to the repo at <path>, not the
            // process cwd. Extract the -C path as a write target whenever the subcommand is not in
            // the read-only allowlist, so boundary_denial_tracked catches out-of-bounds repos.
            "git" => {
                let args = &words[idx + 1..];
                let mut c_paths: Vec<String> = Vec::new();
                let mut j = 0;
                while j < args.len() {
                    if (args[j] == "-c" || args[j] == "--config") && j + 1 < args.len() {
                        // INDEPENDENT REVIEW item 6 (site 1): skip -c key=value so the value
                        // is not mistaken for the git subcommand verb.
                        j += 2;
                    } else if args[j] == "-C" && j + 1 < args.len() {
                        c_paths.push(args[j + 1].to_string());
                        j += 2;
                    } else if let Some(p) = args[j]
                        .strip_prefix("--git-dir=")
                        .or_else(|| args[j].strip_prefix("--work-tree="))
                    {
                        c_paths.push(p.to_string());
                        j += 1;
                    } else if (args[j] == "--git-dir" || args[j] == "--work-tree")
                        && j + 1 < args.len()
                    {
                        c_paths.push(args[j + 1].to_string());
                        j += 2;
                    } else if args[j].starts_with('-') {
                        j += 1;
                    } else {
                        // First non-flag word is the git subcommand.
                        if !c_paths.is_empty() && !GIT_READ_VERBS.contains(&args[j]) {
                            targets.extend(c_paths);
                        }
                        break;
                    }
                }
            }
            // issue #540: `ln <src> <dest>` — the last non-flag argument is the destination.
            // `ln` can create hard or symbolic links outside the worktree, which counts as a
            // write target for boundary judgement.
            "ln" => {
                if let Some(dest) = words[idx + 1..].iter().rev().find(|w| !w.starts_with('-')) {
                    targets.push((*dest).to_string());
                }
            }
            // INDEPENDENT REVIEW item 5: `sed -i` edits files in-place; `rm` deletes them.
            // Both are write operations that the boundary must judge, exactly like redirects.
            "sed" => {
                let args = &words[idx + 1..];
                let has_inplace = args.iter().any(|w| {
                    *w == "-i" || (w.starts_with("-i") && w.len() > 2) || *w == "--in-place"
                });
                if has_inplace {
                    // If -e or -f supplies the script inline, every non-flag arg is a file.
                    // Otherwise the first non-flag arg is the inline script; rest are files.
                    let mut has_e_or_f = false;
                    let mut skip_next = false;
                    let mut non_flags: Vec<&str> = Vec::new();
                    for w in args {
                        if skip_next {
                            skip_next = false;
                            continue;
                        }
                        if *w == "-e" || *w == "-f" {
                            has_e_or_f = true;
                            skip_next = true;
                            continue;
                        }
                        if w.starts_with('-') {
                            continue;
                        }
                        non_flags.push(w);
                    }
                    let file_args: &[&str] = if has_e_or_f {
                        &non_flags[..]
                    } else {
                        non_flags.get(1..).unwrap_or(&[])
                    };
                    for f in file_args {
                        targets.push((*f).to_string());
                    }
                }
            }
            "rm" => {
                for w in &words[idx + 1..] {
                    if !w.starts_with('-') && !is_fd_dup_operator(w) {
                        targets.push((*w).to_string());
                    }
                }
            }
            _ => {}
        }
    }
}

/// Git subcommands that only READ the repository — `git -C <path>` paired with any of these is
/// NOT a write target (issue #540). Fail-closed: an unrecognised subcommand is treated as a write,
/// so a newly-added write verb is blocked rather than silently admitted.
const GIT_READ_VERBS: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "blame",
    "ls-files",
    "ls-tree",
    "rev-parse",
    "rev-list",
    "for-each-ref",
    "describe",
    "shortlog",
    "grep",
    "cat-file",
    "verify-commit",
    "verify-tag",
    "check-ignore",
    "archive",
    "diff-tree",
    "diff-index",
    "diff-files",
    "name-rev",
    "merge-base",
    "count-objects",
    "fsck",
];

/// Interpreter programs that can write files with unresolvable targets when invoked with inline
/// code (`-c`/`-e`/`-r`), a script argument, or stdin (`-`). Under ReadOnly posture,
/// [`opaque_interpreter_denial`] refuses these when they look like code execution.
const OPAQUE_WRITER_PROGRAMS: &[&str] = &[
    "python", "python2", "python3", "node", "nodejs", "deno", "perl", "ruby", "php", "lua",
    "Rscript",
    // Shells invoked without `-c` (stdin heredoc, `-s`, a script-file argument) are opaque
    // under ReadOnly — the script content cannot be statically analysed (item 6 of the review).
    // `sh -c '...'` is already handled by `unwrap_program` returning `Unwrapped::Inline`.
    "sh", "bash", "zsh", "dash",
];

/// Returns `true` when `command` contains a program that can write files with targets that
/// `bash_write_targets` cannot resolve from the command text — interpreters, shells without
/// `-c`, git non-read verbs, `ln`, `sed`, `rm`, `truncate`, `patch` (item 4 of the review).
/// Used by `bash_cd_escape_targets` to judge `cd` destinations even when there is no
/// statically-resolvable write target.
fn command_contains_write_capable_program(command: &str) -> bool {
    let owned = shell_tokens(command);
    let toks: Vec<&str> = owned.iter().map(String::as_str).collect();
    const SEPS: [&str; 8] = ["|", "||", "&&", ";", "&", "|&", "(", ")"];
    let mut segments: Vec<Vec<&str>> = Vec::new();
    let mut seg: Vec<&str> = Vec::new();
    for &t in &toks {
        if SEPS.contains(&t) {
            if !seg.is_empty() {
                segments.push(std::mem::take(&mut seg));
            }
        } else if redirect_glob(t).is_none() {
            seg.push(t);
        }
    }
    if !seg.is_empty() {
        segments.push(seg);
    }
    for words in &segments {
        match unwrap_program(words) {
            Unwrapped::Inline(_) => return true, // any -c wrapper is write-capable
            Unwrapped::Program { idx, .. } => {
                let Some(prog) = words.get(idx) else { continue };
                let basename = program_basename(prog);
                if OPAQUE_WRITER_PROGRAMS.contains(&basename) {
                    return true;
                }
                if basename == "git" {
                    let args = &words[idx + 1..];
                    let mut j = 0;
                    while j < args.len() {
                        // INDEPENDENT REVIEW item 6 (site 2): skip the value of flags that
                        // take a separate-token argument so the value is never mistaken for
                        // the subcommand verb (over-deny) or causes a verb-position miss.
                        if (args[j] == "-c"
                            || args[j] == "--config"
                            || args[j] == "-C"
                            || args[j] == "--git-dir"
                            || args[j] == "--work-tree")
                            && j + 1 < args.len()
                        {
                            j += 2;
                        } else if args[j].starts_with('-') {
                            j += 1;
                        } else {
                            if !GIT_READ_VERBS.contains(&args[j]) {
                                return true;
                            }
                            break;
                        }
                    }
                }
                if matches!(basename, "ln" | "sed" | "rm" | "truncate" | "patch") {
                    return true;
                }
            }
        }
    }
    false
}

/// Extract the `cd` destinations from a Bash command for the BOUNDARY check (issue #540).
///
/// A `cd <outside> && <write>` moves the shell's runtime cwd; relative write targets in
/// later segments land in the cd destination, not the process cwd (the worktree). Returns
/// the cd destination(s) ONLY when the command also contains at least one write operation
/// (from `bash_write_targets`) — a bare `cd <outside>` with no writes is handled by the
/// install fence, not this boundary check. Called from the boundary check only, NOT from the
/// phase-scope fence, so evaluators may `cd src/` within the worktree without a fence hit.
fn bash_cd_escape_targets(command: &str) -> Vec<String> {
    if bash_write_targets(command).is_empty() && !command_contains_write_capable_program(command) {
        return Vec::new();
    }
    bash_cd_targets_inner(command, true)
}

fn bash_cd_targets_inner(command: &str, unwrap_inline: bool) -> Vec<String> {
    let owned = shell_tokens(command);
    let toks: Vec<&str> = owned.iter().map(String::as_str).collect();
    const SEPS: [&str; 8] = ["|", "||", "&&", ";", "&", "|&", "(", ")"];
    let mut segments: Vec<Vec<&str>> = Vec::new();
    let mut seg: Vec<&str> = Vec::new();
    for &t in &toks {
        if SEPS.contains(&t) {
            if !seg.is_empty() {
                segments.push(std::mem::take(&mut seg));
            }
        } else if redirect_glob(t).is_none() {
            seg.push(t);
        }
    }
    if !seg.is_empty() {
        segments.push(seg);
    }
    let mut targets: Vec<String> = Vec::new();
    for words in &segments {
        match unwrap_program(words) {
            Unwrapped::Inline(inner) => {
                if unwrap_inline {
                    targets.extend(bash_cd_targets_inner(inner, false));
                }
            }
            Unwrapped::Program { idx, .. } => {
                let Some(prog) = words.get(idx) else {
                    continue;
                };
                if program_basename(prog) == "cd" {
                    if let Some(dest) = words[idx + 1..].iter().find(|w| !w.starts_with('-')) {
                        targets.push((*dest).to_string());
                    }
                }
            }
        }
    }
    targets
}

/// Under ReadOnly posture, deny interpreter invocations whose write targets cannot be resolved
/// from the command text (inline code via `-c`/`-e`, a script argument, or stdin via `-`).
///
/// `bash_write_targets` extracts resolvable targets (redirects, `cp`/`mv` destinations, etc.).
/// These interpreter shapes have their write path inside a string argument or a script that this
/// literal scan cannot introspect, so no target is found and the call falls through to allow.
/// This function closes that gap for ReadOnly units: if the program is a known interpreter AND
/// it is invoked in a way that could execute arbitrary code, deny by program word.
///
/// `--version`, `-V`, `--help`, `-h`, and `-?` (read-only info flags) are the only allowed args
/// that suppress the denial — if the invocation consists ONLY of those flags, it is harmless.
/// A bare interpreter name with no args is denied: `echo 'touch x' | bash` tokenises to a
/// segment with an empty arg list, and the vacuous-truth of `[].all(_)` would otherwise admit
/// pipe-fed code execution.
fn opaque_interpreter_denial(command: &str) -> Option<String> {
    opaque_interpreter_denial_inner(command, true)
}

fn opaque_interpreter_denial_inner(command: &str, unwrap_inline: bool) -> Option<String> {
    let owned = shell_tokens(command);
    let toks: Vec<&str> = owned.iter().map(String::as_str).collect();
    const SEPS: [&str; 8] = ["|", "||", "&&", ";", "&", "|&", "(", ")"];
    let mut segments: Vec<Vec<&str>> = Vec::new();
    let mut seg: Vec<&str> = Vec::new();
    for &t in &toks {
        if SEPS.contains(&t) {
            if !seg.is_empty() {
                segments.push(std::mem::take(&mut seg));
            }
        } else if redirect_glob(t).is_none() {
            seg.push(t);
        }
    }
    if !seg.is_empty() {
        segments.push(seg);
    }
    for words in &segments {
        match unwrap_program(words) {
            Unwrapped::Inline(inner) => {
                if unwrap_inline {
                    if let Some(r) = opaque_interpreter_denial_inner(inner, false) {
                        return Some(r);
                    }
                }
            }
            Unwrapped::Program { idx, .. } => {
                let Some(prog) = words.get(idx) else {
                    continue;
                };
                let basename = program_basename(prog);
                if !OPAQUE_WRITER_PROGRAMS.contains(&basename) {
                    continue;
                }
                let args = &words[idx + 1..];
                // Allow invocations whose ONLY args are read-only info flags. Requires non-empty
                // args: a bare interpreter (`bash` alone, `echo x | python3`) has an empty arg
                // list; `[].all(_)` is vacuously true and would admit pipe-fed code execution.
                let only_info = !args.is_empty()
                    && args
                        .iter()
                        .all(|a| matches!(*a, "--version" | "-V" | "--help" | "-h" | "-?"));
                if only_info {
                    continue;
                }
                return Some(format!(
                    "phase scope: `Bash` invokes `{basename}` (an interpreter that can write \
                     files with targets unresolvable from the command text — inline -c/-e code, \
                     a script, or stdin via `-`) — under a read-only evaluation phase every such \
                     invocation is refused; {PHASE_SCOPE_BASH_REMEDY}."
                ));
            }
        }
    }
    None
}

/// Under ReadOnly posture, deny explicit write-program invocations by program word — programs
/// that cannot be safely allowed without analysing their arguments and that are not otherwise
/// caught by the write-target extractor or the opaque-interpreter check. This is separate from
/// `opaque_interpreter_denial`: these programs have predictable write semantics but no
/// resolvable TARGET in the command text under the shapes that matter.
///
/// Covers: `sed -i` (in-place edit), `rm` (deletion), `truncate`, `patch`, `ln` (linking),
/// and git write subcommands (`commit`, `apply`, `checkout`, `stash`, `reset`).
fn explicit_write_program_denial(command: &str) -> Option<String> {
    explicit_write_program_denial_inner(command, true)
}

fn explicit_write_program_denial_inner(command: &str, unwrap_inline: bool) -> Option<String> {
    let owned = shell_tokens(command);
    let toks: Vec<&str> = owned.iter().map(String::as_str).collect();
    const SEPS: [&str; 8] = ["|", "||", "&&", ";", "&", "|&", "(", ")"];
    let mut segments: Vec<Vec<&str>> = Vec::new();
    let mut seg: Vec<&str> = Vec::new();
    for &t in &toks {
        if SEPS.contains(&t) {
            if !seg.is_empty() {
                segments.push(std::mem::take(&mut seg));
            }
        } else if redirect_glob(t).is_none() {
            seg.push(t);
        }
    }
    if !seg.is_empty() {
        segments.push(seg);
    }
    for words in &segments {
        match unwrap_program(words) {
            Unwrapped::Inline(inner) => {
                if unwrap_inline {
                    if let Some(r) = explicit_write_program_denial_inner(inner, false) {
                        return Some(r);
                    }
                }
            }
            Unwrapped::Program { idx, .. } => {
                let Some(prog) = words.get(idx) else {
                    continue;
                };
                let basename = program_basename(prog);
                let args = &words[idx + 1..];
                match basename {
                    "sed" => {
                        // `sed -i` (in-place edit) — deny if any arg is `-i` or starts with `-i`.
                        let has_inplace = args.iter().any(|a| {
                            *a == "-i" || (a.starts_with("-i") && a.len() > 2) || *a == "--in-place"
                        });
                        if has_inplace {
                            return Some(format!(
                                "phase scope: `Bash` invokes `sed -i` (in-place file edit) — \
                                 under a read-only evaluation phase this write is refused; \
                                 {PHASE_SCOPE_BASH_REMEDY}."
                            ));
                        }
                    }
                    "rm" => {
                        // `rm` always deletes — deny by program word.
                        if !args
                            .iter()
                            .all(|a| matches!(*a, "--help" | "-h" | "--version"))
                        {
                            return Some(format!(
                                "phase scope: `Bash` invokes `rm` (file deletion) — \
                                 under a read-only evaluation phase this write is refused; \
                                 {PHASE_SCOPE_BASH_REMEDY}."
                            ));
                        }
                    }
                    "truncate" => {
                        return Some(format!(
                            "phase scope: `Bash` invokes `truncate` (file size modification) — \
                             under a read-only evaluation phase this write is refused; \
                             {PHASE_SCOPE_BASH_REMEDY}."
                        ));
                    }
                    "patch" => {
                        return Some(format!(
                            "phase scope: `Bash` invokes `patch` (file modification) — \
                             under a read-only evaluation phase this write is refused; \
                             {PHASE_SCOPE_BASH_REMEDY}."
                        ));
                    }
                    "ln" => {
                        // `ln` without `--help`/`--version` creates links.
                        if !args
                            .iter()
                            .all(|a| matches!(*a, "--help" | "-h" | "--version"))
                        {
                            return Some(format!(
                                "phase scope: `Bash` invokes `ln` (link creation) — \
                                 under a read-only evaluation phase this write is refused; \
                                 {PHASE_SCOPE_BASH_REMEDY}."
                            ));
                        }
                    }
                    "git" => {
                        // Deny specific git write subcommands by name.
                        const GIT_WRITE_SUBCOMMANDS: &[&str] =
                            &["commit", "apply", "checkout", "stash", "reset"];
                        let mut j = 0;
                        while j < args.len() {
                            // INDEPENDENT REVIEW item 6 (site 3): skip values of flags that
                            // take a separate token so the value is never treated as the verb
                            // (would cause a bypass: `git -c user.name=x commit` → admitted).
                            if (args[j] == "-c"
                                || args[j] == "--config"
                                || args[j] == "-C"
                                || args[j] == "--git-dir"
                                || args[j] == "--work-tree")
                                && j + 1 < args.len()
                            {
                                j += 2;
                            } else if args[j].starts_with('-') {
                                j += 1;
                            } else {
                                if GIT_WRITE_SUBCOMMANDS.contains(&args[j]) {
                                    return Some(format!(
                                        "phase scope: `Bash` invokes `git {}` (a write \
                                         subcommand) — under a read-only evaluation phase \
                                         this is refused; {PHASE_SCOPE_BASH_REMEDY}.",
                                        args[j]
                                    ));
                                }
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    None
}

/// The basename of a program word with ONE layer of shell quotes stripped first, so `"cp"`,
/// `'/usr/bin/tee'` and `C:\tools\tee` name the same family as `cp` / `tee` (core #475: a quoted
/// program word used to match nothing). The one quoting rule both tokenizer consumers share.
fn program_basename(prog: &str) -> &str {
    let p = prog.trim_matches(|c| c == '"' || c == '\'');
    p.rsplit(['/', '\\']).next().unwrap_or(p)
}

/// One layer of shell quotes off a token: `'echo x > f'` → `echo x > f`. Only a MATCHING outer pair
/// is stripped (the string a `-c` wrapper hands the inner shell); anything else is left verbatim.
fn strip_one_quote_layer(tok: &str) -> &str {
    let b = tok.as_bytes();
    if b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"') && b[b.len() - 1] == b[0] {
        &tok[1..tok.len() - 1]
    } else {
        tok
    }
}

/// What ONE level of wrapper-unwrapping found at the head of a pipeline/sequence segment (core #475).
enum Unwrapped<'a> {
    /// `words[idx]` is the program word; every wrapper before it and every leading `NAME=value`
    /// assignment was skipped. The assignments are returned because a `WICKED_HOME=…` prefix is one
    /// way the estate shim rule's store pin is spelled.
    Program {
        idx: usize,
        assignments: Vec<&'a str>,
    },
    /// A shell `-c` family wrapper: the inner command string with one layer of quotes stripped, for
    /// the caller to `shell_tokens` and rescan as its own command line.
    Inline(&'a str),
}

/// See through ONE level of the fixed wrapper table that hides a segment's real program word
/// (core #475 — `sh -lc 'echo x > src/y'`, `exec tee src/y`, `xargs tee src/y` and `"cp" a src/y`
/// all matched nothing before this). The table, exactly:
///
/// * **shell `-c` family** — `sh`/`bash`/`zsh`/`dash` followed by a flag cluster ENDING in `c`
///   (`-c`, `-lc`, `-ec`, `-xc`, …) then `<string>`: the string is handed back as
///   [`Unwrapped::Inline`] for a rescan. ONE level only — the caller does not unwrap a `-c` found
///   INSIDE the inner string (`sh -c 'sh -c "…"'` is the documented pass).
/// * **`exec`** — the word dropped.
/// * **`xargs [flags]`** — the word and its leading `-` flags dropped.
/// * **`env [flags] [NAME=val]…`** — dropped; the assignments are kept as pin prefix, as before
///   (arg-taking flags like `-u VAR` are not modelled).
/// * **`nice [-n N]`** — the word, its flags and `-n`'s value dropped.
/// * **`timeout [flags] <duration>`** — the word, its flags (with `-s`/`-k` values) and the
///   duration dropped.
/// * every wrapper word and the program word itself are matched through [`program_basename`] —
///   one layer of quotes stripped, path prefix removed (the one quoting rule).
///
/// A `sh`/`bash` segment WITHOUT a `-c` cluster is not a wrapper here: it is a script LAUNCHER
/// (`sh "$ROOT/scripts/_python.sh" x.py`), left for [`executed_estate_shim`] to look through.
fn unwrap_program<'a>(words: &[&'a str]) -> Unwrapped<'a> {
    let mut idx = 0;
    let mut assignments: Vec<&'a str> = Vec::new();
    // Every iteration consumes at least one word or returns, so the loop is bounded by `words.len()`.
    loop {
        while idx < words.len() && is_env_assignment(words[idx]) {
            assignments.push(words[idx]);
            idx += 1;
        }
        let Some(prog) = words.get(idx) else {
            return Unwrapped::Program { idx, assignments };
        };
        match program_basename(prog) {
            "exec" => idx += 1,
            "xargs" => {
                idx += 1;
                while idx < words.len() && words[idx].starts_with('-') {
                    idx += 1;
                }
            }
            "env" => {
                idx += 1;
                while idx < words.len()
                    && (words[idx].starts_with('-') || is_env_assignment(words[idx]))
                {
                    if is_env_assignment(words[idx]) {
                        assignments.push(words[idx]);
                    }
                    idx += 1;
                }
            }
            "nice" => {
                idx += 1;
                while idx < words.len() && words[idx].starts_with('-') {
                    let takes_value = matches!(words[idx], "-n" | "--adjustment");
                    idx += 1;
                    if takes_value {
                        idx += 1;
                    }
                }
            }
            "timeout" => {
                idx += 1;
                while idx < words.len() && words[idx].starts_with('-') {
                    let takes_value =
                        matches!(words[idx], "-s" | "-k" | "--signal" | "--kill-after");
                    idx += 1;
                    if takes_value {
                        idx += 1;
                    }
                }
                idx += 1; // the DURATION
            }
            "sh" | "bash" | "zsh" | "dash" => {
                // Walk the shell's own flags; a single-dash cluster ending in `c` means "the next
                // word is the command string".
                let mut j = idx + 1;
                while j < words.len() && words[j].starts_with('-') {
                    let flag = words[j];
                    j += 1;
                    if flag.len() >= 2 && !flag.starts_with("--") && flag.ends_with('c') {
                        return match words.get(j) {
                            Some(&inner) => Unwrapped::Inline(strip_one_quote_layer(inner)),
                            None => Unwrapped::Program { idx, assignments },
                        };
                    }
                }
                return Unwrapped::Program { idx, assignments };
            }
            _ => return Unwrapped::Program { idx, assignments },
        }
    }
}

/// Classify an in-run Bash invocation of the `wicked-estate` CLI, the estate stdio MCP
/// (`wicked-estate-mcp`), or wicked-garden's estate shim / backends (DES-GROUNDING-001 §7,
/// issue #463).
///
/// Replaces the old deny-all rule with a per-command ALLOWLIST:
///
/// * **`wicked-estate` CLI** — the read-only subcommands [`ESTATE_READ_VERBS`] (`query`,
///   `blast-radius`, `rank`, `stats`, `source`, `semantic`, `cross-graph`, `subscribe`) and
///   `clusters` WITHOUT `--annotate` are ALLOWED; the write subcommands (`index`, `scip`,
///   `tfstate`, `import-telemetry`, `compact`, `watch`, `clusters --annotate`) and any
///   unrecognised subcommand are DENIED (fail-closed: a future read verb must be added here).
/// * **`wicked-estate-mcp`** and the **estate shim** — ALLOWED only when the segment carries
///   BOTH `--readonly` AND a pinned store: `--db <path>` / `--db=<path>` on argv, a leading
///   `WICKED_ESTATE_DB=…` / `WICKED_HOME=…` / `WICKED_MEMORY_DB=…` assignment
///   ([`ESTATE_STORE_PIN_ENV`]), or `store_pinned_by_env` — the same variables in the WORKER's
///   environment, handed in by the carrier ([`BoundaryCtx::estate_store_pinned`] /
///   [`estate_store_pinned_from_env`]). `--readonly` alone is not enough: an unpinned shim
///   resolves whatever store the cwd or the operator's defaults happen to name.
///
/// # The shim / backend pattern (the cross-repo contract, DES-GROUNDING-001 §7.3)
///
/// wicked-garden grounds through `scripts/_estate_client.py` — the stdio shim that spawns
/// `wicked-estate-mcp` — and the backends that import it: every `scripts/mem/*.py`
/// (`estate_memory.py`, `auto_memorize.py`, …) and `scripts/_context_backend.py`. Skills run
/// them through a LAUNCHER, so the program word is never `wicked-estate`:
/// `sh "$ROOT/scripts/_python.sh" "$ROOT/scripts/mem/estate_memory.py" recall '{…}'`. The
/// classifier therefore recognises a shim invocation by the SCRIPT IN EXECUTING POSITION — the
/// program word itself, or the first non-flag argument of a launcher (`python*`, `py`, `sh`,
/// `bash`, `zsh`, `dash`, seen through garden's `_python.sh` / `_run.py` resolvers), or the
/// module of `python -m <mod>` ([`ESTATE_SHIM_MODULES`]) — never by a mere mention
/// (`grep readonly scripts/_estate_client.py` is not an invocation). The script is matched by
/// basename ([`ESTATE_SHIM_SCRIPTS`]) or by the `scripts/mem/` path segment ([`ESTATE_SHIM_DIR`]).
/// A new garden backend that spawns the shim from another directory must be added here (or live
/// under `scripts/mem/`) — until then it is invisible to this scan, exactly as before.
///
/// DEFENSE-IN-DEPTH, same honest limit as [`bash_write_targets`] — the complete list of shapes a
/// literal scan does NOT model (core #475; [`unwrap_program`] sees through exactly one level of the
/// wrapper table and nothing else): `$(…)`/backtick substitution, `$VAR`/`${VAR}` program or target
/// words, in-place editors (`sed -i`, `perl -i`), `git apply`/`patch`, `rm`/`ln`, a SECOND
/// level of wrapping (`sh -c 'sh -c "…"'`), a renamed binary, raw SQLite via python, and the
/// no-space glued operator (`a&&wicked-estate`). (`touch`, `git -C`, `cd`, and inline interpreters
/// (`python3 -c`, `node -e`, `perl -e`, `python3 - <<EOF`) are now modelled — issues #541/#540.)
/// OS-level containment is the only hermetic
/// guarantee; this is the secondary layer for sandbox-less hosts.
///
/// Returns the offending pipeline/sequence segment and WHY (so the deny message can NAME both),
/// or `None` when every segment is in the allowlist.
///
/// `is_env_assignment`: a `NAME=value` token with a shell-identifier NAME — a leading env-assignment
/// prefix (`X=1 cmd`) that runs `cmd` with `X` set, so it is not the program word. `=foo`, `1a=b`, or
/// a bare `foo` are NOT assignments (the first is the program).
fn is_env_assignment(tok: &str) -> bool {
    match tok.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// One denied estate invocation: the pipeline/sequence segment (so the message can NAME it) and
/// WHY it was refused (one of the `ESTATE_WHY_*` sentences).
pub(crate) struct EstateDeny {
    pub segment: String,
    pub why: &'static str,
}

/// Why an estate invocation was refused — a `wicked-estate` write subcommand.
pub(crate) const ESTATE_WHY_WRITE_VERB: &str =
    "a write subcommand mutates the shared project graph";
/// Why an estate invocation was refused — a `wicked-estate` subcommand not in the read allowlist.
pub(crate) const ESTATE_WHY_UNKNOWN_VERB: &str =
    "not a known read-only `wicked-estate` subcommand (fail-closed)";
/// Why an estate invocation was refused — the shim / MCP runs without `--readonly`.
pub(crate) const ESTATE_WHY_NO_READONLY: &str =
    "the estate shim / `wicked-estate-mcp` runs without `--readonly`";
/// Why an estate invocation was refused — the shim / MCP names no store.
pub(crate) const ESTATE_WHY_NO_PIN: &str = "the estate shim / `wicked-estate-mcp` names no store \
     (`--db <path>` on argv, or WICKED_ESTATE_DB / WICKED_HOME / WICKED_MEMORY_DB in the worker env)";

/// The read-only `wicked-estate` subcommands a governed unit may run (DES-GROUNDING-001 §7.1).
/// `clusters` joins them only WITHOUT `--annotate` (judged at the call site).
const ESTATE_READ_VERBS: [&str; 8] = [
    "query",
    "blast-radius",
    "rank",
    "stats",
    "source",
    "semantic",
    "cross-graph",
    "subscribe",
];
/// The `wicked-estate` subcommands that WRITE the graph — named so the reason can say "write
/// subcommand" rather than "unknown"; anything else unrecognised is denied fail-closed anyway.
const ESTATE_WRITE_VERBS: [&str; 6] = [
    "index",
    "scip",
    "tfstate",
    "import-telemetry",
    "compact",
    "watch",
];
/// Basenames of wicked-garden scripts that ARE the estate stdio shim or spawn it from outside
/// [`ESTATE_SHIM_DIR`] (DES-GROUNDING-001 §7.3).
const ESTATE_SHIM_SCRIPTS: [&str; 2] = ["_estate_client.py", "_context_backend.py"];
/// The path segment of garden's `mem` domain backends — every `.py` under it grounds through the shim.
const ESTATE_SHIM_DIR: &str = "scripts/mem/";
/// The same shim / backends spelled as `python -m <module>` (last dotted component).
const ESTATE_SHIM_MODULES: [&str; 5] = [
    "_estate_client",
    "_context_backend",
    "estate_memory",
    "auto_memorize",
    "session_fact_extractor",
];

/// A shell-quoted, possibly `\`-separated script token as a bare `/`-separated path — so
/// `"${CLAUDE_PLUGIN_ROOT}/scripts/mem/x.py"` and `scripts\mem\x.py` classify like `scripts/mem/x.py`.
/// Also collapses repeated slashes and strips leading `./` (#474): `./scripts//mem/x.py` and
/// `scripts/mem/x.py` are the same script to the fence.
fn script_path(tok: &str) -> String {
    let mut p = tok
        .trim_matches(|c| c == '"' || c == '\'')
        .replace('\\', "/");
    while p.contains("//") {
        p = p.replace("//", "/");
    }
    while let Some(rest) = p.strip_prefix("./") {
        p = rest.to_string();
    }
    p
}

fn script_basename(tok: &str) -> String {
    let p = script_path(tok);
    p.rsplit('/').next().unwrap_or(p.as_str()).to_string()
}

/// Is `tok` (in executing position) the estate shim or a backend that spawns it?
fn is_estate_shim_script(tok: &str) -> bool {
    let p = script_path(tok);
    let base = p.rsplit('/').next().unwrap_or(p.as_str());
    ESTATE_SHIM_SCRIPTS.contains(&base)
        || (p.contains(ESTATE_SHIM_DIR) && base.ends_with(".py"))
        // #474: a mem backend run by BASENAME after a `cd scripts` (`python3 mem/estate_memory.py`)
        // no longer carries the `scripts/mem/` segment — recognise it by its stem in the module set.
        || ESTATE_SHIM_MODULES.contains(&base.strip_suffix(".py").unwrap_or(base))
}

/// The wicked-garden launcher basenames (#463 §7.3). Skills run the shim through it —
/// `wicked-garden run scripts/_estate_client.py …` / `wicked-garden python <script>` — directly or
/// via `node <…/wicked-garden.mjs>` / `npx wicked-garden`.
const GARDEN_LAUNCHERS: [&str; 3] = ["wicked-garden", "wicked-garden.cmd", "wicked-garden.mjs"];

fn is_garden_launcher(base: &str) -> bool {
    GARDEN_LAUNCHERS.contains(&base)
}

/// If `words[idx..]` opens with a wicked-garden launcher (directly, or through one `node <script>` /
/// `npx <pkg>` indirection), return the index of the first word AFTER its mandatory `run`/`python`
/// verb — where the shim script (or a `python -m`) is scanned for. `None` when the segment is not a
/// garden launcher or names no `run`/`python` verb. The launcher forwards the outer argv
/// (`--readonly`, `--db`) to the `wicked-estate-mcp` it spawns, so the caller still judges the whole
/// segment; this only finds the script in executing position past the launcher.
fn garden_launcher_script_start(words: &[&str], idx: usize) -> Option<usize> {
    let base = script_basename(words[idx]);
    let after_launcher = if matches!(base.as_str(), "node" | "npx") {
        let mut j = idx + 1;
        while j < words.len() && words[j].starts_with('-') {
            j += 1;
        }
        if !is_garden_launcher(&script_basename(words.get(j)?)) {
            return None;
        }
        j + 1
    } else if is_garden_launcher(&base) {
        idx + 1
    } else {
        return None;
    };
    let mut i = after_launcher;
    while i < words.len() && words[i].starts_with('-') {
        i += 1;
    }
    match words.get(i) {
        Some(&"run") | Some(&"python") => Some(i + 1),
        _ => None,
    }
}

/// Is `tok` a `python -m` module spelling of the shim / a backend?
fn is_estate_shim_module(tok: &str) -> bool {
    let m = tok.trim_matches(|c| c == '"' || c == '\'');
    ESTATE_SHIM_MODULES.contains(&m.rsplit('.').next().unwrap_or(m))
}

/// Program words that LAUNCH the script named by their next non-flag argument.
fn is_script_launcher(base: &str) -> bool {
    base == "python"
        || base == "python2"
        || base.starts_with("python3")
        || matches!(
            base,
            "py" | "sh" | "bash" | "zsh" | "dash" | "_python.sh" | "_run.py"
        )
}

/// The estate shim / a backend in EXECUTING position within `words[idx..]` — the program word
/// itself (`./scripts/_estate_client.py health`), or the script a launcher runs (`python3 x.py`,
/// `sh …/_python.sh x.py`, `py -3 x.py`, `python -m mem.estate_memory`). Launcher flags are
/// skipped; garden's `_python.sh` / `_run.py` resolvers are looked through to the script they run;
/// `-c <code>` (inline python) is not modelled — the documented literal-scan limit. A shell `-c`
/// string never reaches here: [`unwrap_program`] hands it back for a rescan first. The
/// `wicked-garden run|python <script>` launcher (#463 §7.3) is looked through the same way — see
/// [`garden_launcher_script_start`].
fn executed_estate_shim(words: &[&str], idx: usize) -> bool {
    let prog = words[idx];
    if is_estate_shim_script(prog) {
        return true;
    }
    // Where the script arguments begin: past a wicked-garden launcher's run|python verb, else
    // straight after a python/sh launcher; anything else is not a launcher.
    let scan_from = if let Some(i) = garden_launcher_script_start(words, idx) {
        i
    } else if is_script_launcher(&script_basename(prog)) {
        idx + 1
    } else {
        return false;
    };
    let mut i = scan_from;
    while i < words.len() {
        let w = words[i];
        if w == "-m" {
            return words.get(i + 1).is_some_and(|m| is_estate_shim_module(m));
        }
        if w == "-c" {
            return false;
        }
        if w.starts_with('-') || matches!(script_basename(w).as_str(), "_python.sh" | "_run.py") {
            i += 1;
            continue;
        }
        return is_estate_shim_script(w);
    }
    false
}

/// Does the segment itself PIN the store the shim / MCP resolves — `--db <path>` / `--db=<path>`
/// on argv, or a leading env-assignment naming one of [`ESTATE_STORE_PIN_ENV`] with a value?
fn argv_pins_store(words: &[&str], prefix_assignments: &[&str]) -> bool {
    let db_on_argv = words.iter().enumerate().any(|(i, &t)| {
        (t.starts_with("--db=") && t.len() > "--db=".len())
            || (t == "--db" && words.get(i + 1).is_some_and(|v| !v.starts_with('-')))
    });
    let pinned_by_assignment = prefix_assignments.iter().any(|a| {
        a.split_once('=')
            .is_some_and(|(name, value)| ESTATE_STORE_PIN_ENV.contains(&name) && !value.is_empty())
    });
    db_on_argv || pinned_by_assignment
}

/// The `wicked-estate` subcommand: the first non-flag token after the program word, skipping the
/// value of a leading `--db <path>` so `wicked-estate --db x stats` classifies as `stats`.
fn estate_subcommand<'a>(rest: &[&'a str]) -> Option<&'a str> {
    let mut i = 0;
    while i < rest.len() {
        let t = rest[i];
        if t == "--db" {
            i += 2;
            continue;
        }
        if t.starts_with('-') {
            i += 1;
            continue;
        }
        return Some(t);
    }
    None
}

fn classify_estate_command(command: &str, store_pinned_by_env: bool) -> Option<EstateDeny> {
    classify_estate_command_in(command, store_pinned_by_env, true)
}

/// The scan behind [`classify_estate_command`]. `unwrap_inline` is true for the command line the
/// worker issued and false for the ONE inner rescan of a shell `-c` string (a nested `-c` is the
/// documented pass — see the limit list above).
fn classify_estate_command_in(
    command: &str,
    store_pinned_by_env: bool,
    unwrap_inline: bool,
) -> Option<EstateDeny> {
    let owned = shell_tokens(command);
    let toks: Vec<&str> = owned.iter().map(String::as_str).collect();

    // Split into pipeline/sequence SEGMENTS on the same shell separators [`bash_write_targets`]
    // uses, so an estate invocation after a pipe / `;` / `&&` is checked as its own program — not
    // missed because the first word of the whole line was something else. Redirect operators (and
    // their glued targets) are dropped so a leading redirect cannot hide the program word.
    const SEPS: [&str; 8] = ["|", "||", "&&", ";", "&", "|&", "(", ")"];
    let mut segments: Vec<Vec<&str>> = Vec::new();
    let mut seg: Vec<&str> = Vec::new();
    let mut skip_redirect_target = false;
    for &t in &toks {
        if SEPS.contains(&t) {
            if !seg.is_empty() {
                segments.push(std::mem::take(&mut seg));
            }
            skip_redirect_target = false;
        } else if skip_redirect_target {
            // The spaced target of a redirect operator (`> file`): drop it too, so a PREFIX redirect
            // (`> /dev/null wicked-estate …`) cannot make the target look like the program.
            skip_redirect_target = false;
        } else if let Some(glued) = redirect_glob(t) {
            // Redirect OPERATOR: drop it; if its filename is not glued on, the NEXT token is the target.
            skip_redirect_target = glued.is_empty();
        } else {
            seg.push(t);
        }
    }
    if !seg.is_empty() {
        segments.push(seg);
    }
    for words in &segments {
        // Find the program word, seeing through the common, LEGITIMATE prefixes that would otherwise
        // hide it (Copilot #385, core #475): leading `NAME=value` env-assignments
        // (`X=1 wicked-estate …`) and ONE level of the wrapper table `unwrap_program` models
        // (`env`, `exec`, `xargs`, `nice`, `timeout`, a quoted program word, and a shell `-c` string,
        // which is rescanned as its own command line). Prefix redirects were already dropped above.
        // The assignments are kept: a `WICKED_HOME=… ` prefix is one way the shim rule's store pin is
        // spelled.
        //
        // BEST-EFFORT BY DESIGN: a literal scan cannot see through every invocation form — the
        // complete unmodelled list is in this function's doc. The HERMETIC containment is Boundary
        // 1's OS sandbox: the shared graph db lives OUTSIDE the worktree, so a kernel write-deny
        // stops EVERY form when the sandbox is armed. This scan is the secondary layer for
        // sandbox-less hosts.
        let (idx, prefix_assignments) = match unwrap_program(words) {
            Unwrapped::Program { idx, assignments } => (idx, assignments),
            Unwrapped::Inline(inner) => {
                if unwrap_inline {
                    let hit = classify_estate_command_in(inner, store_pinned_by_env, false);
                    if hit.is_some() {
                        return hit;
                    }
                }
                continue;
            }
        };
        let Some(prog) = words.get(idx) else { continue };
        // Basename with the SAME rule [`bash_write_targets`] uses (one quote layer off, path prefix
        // removed), so an absolute, quoted or `\`-separated path resolves to the same family name.
        let base = program_basename(prog);
        let deny = |why: &'static str| {
            Some(EstateDeny {
                segment: words.join(" "),
                why,
            })
        };

        if matches!(base, "wicked-estate" | "wicked-estate.exe") {
            let rest = &words[idx + 1..];
            match estate_subcommand(rest) {
                // READ-ONLY subcommands — ALLOW.
                Some(verb) if ESTATE_READ_VERBS.contains(&verb) => {}
                // `clusters` is read-only UNLESS `--annotate` is present (that flag writes).
                Some("clusters") if !rest.contains(&"--annotate") => {}
                Some("clusters") => return deny(ESTATE_WHY_WRITE_VERB),
                Some(verb) if ESTATE_WRITE_VERBS.contains(&verb) => {
                    return deny(ESTATE_WHY_WRITE_VERB)
                }
                // Anything else — a verb this build does not know — DENY (fail-closed).
                _ => return deny(ESTATE_WHY_UNKNOWN_VERB),
            }
            continue;
        }

        // The estate stdio MCP, or garden's shim / a backend that spawns it (§7.3): ALLOWED only
        // read-only AND pinned. Judged on the whole segment: the flags ride the outer argv the
        // launcher hands the backend, which forwards them to the `wicked-estate-mcp` it spawns.
        if matches!(base, "wicked-estate-mcp" | "wicked-estate-mcp.exe")
            || executed_estate_shim(words, idx)
        {
            if !words.contains(&"--readonly") {
                return deny(ESTATE_WHY_NO_READONLY);
            }
            if !(store_pinned_by_env || argv_pins_store(words, &prefix_assignments)) {
                return deny(ESTATE_WHY_NO_PIN);
            }
        }
    }
    None
}

/// A standard character-device write sink (not a filesystem location that can hold an escaped file).
/// `> /dev/null` / `2>/dev/stderr` / `>/dev/fd/3` are ordinary output plumbing, never an escape.
fn is_safe_write_sink(target: &str) -> bool {
    matches!(
        target,
        "/dev/null" | "/dev/stdout" | "/dev/stderr" | "/dev/tty" | "/dev/zero"
    ) || target.starts_with("/dev/fd/")
}

/// Returns `true` for fd-dup redirect operators (`2>&1`, `>&2`, `1>&2`) that appear in the
/// argument stream of `touch`/`mkdir` when the shell tokeniser leaves them in the segment.
/// These are NOT file paths and must be skipped so `touch <notes>/f 2>&1` is not refused.
fn is_fd_dup_operator(tok: &str) -> bool {
    let t = tok.trim_start_matches(|c: char| c.is_ascii_digit());
    let t = t.strip_prefix('&').unwrap_or(t);
    if let Some(rest) = t.strip_prefix('>') {
        return rest.starts_with('&');
    }
    false
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
pub fn run_gate_hook(
    scope: &str,
    phase: &str,
    phase_alias: Option<&str>,
    catalog_alias: Option<&str>,
    db: Option<&str>,
) -> i32 {
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
        catalog_alias,
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
    // (DES-TEAMING-002 T3, #627) The unit's phase-catalog id, passed by EVERY carrier as data: the
    // hook subprocess from its argv/env (resolved in the binary), the in-process ACP bridge from
    // the unit — this function never reads env for it.
    catalog_alias: Option<&str>,
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
    // The wrapped carrier's per-attempt shell-cwd sidecar (review of #456, F1): the install fence
    // judges this call from where the seat's shell stood after its last ALLOWED call.
    let install_state = install_fence_cwd_path(decisions_path, phase);
    let boundary_verdict = match boundary {
        Some(b) => boundary_denial_tracked(
            &b.roots,
            &b.cwd,
            b.home.as_deref(),
            b.claude_config_dir.as_deref(),
            context,
            tool,
            None,
            b.estate_store_pinned,
        ),
        None => boundary_denial(context, tool, &install_state),
    };
    if let Some((reason, fatal)) = boundary_verdict {
        // The tool-call is BLOCKED either way (return 2 below). A WRITE outside the sandbox is an
        // escape attempt and stays unit-FATAL; a READ probe — and a benign write into the worker's
        // own `~/.claude` state tree (core#235) — is ADVISORY: blocked, audited, but not unit-fatal,
        // so a worker probing an out-of-bounds file or persisting its own memory (then adapting) is
        // not failed for it (P8 #10 / core#219). `fatal` comes from the boundary check itself so a
        // Bash write-escape (FINDING-045) is fatal even though "Bash" is not in WRITE_TOOLS. See
        // `boundary_denial` / `append_boundary_deny`.
        // A REMOTE-WRITE refusal (F-7R2-012) is recorded under its own claim id, with the command
        // segment beside the reason, so the gate fold can disclose it as `workerToolCallDenied`
        // (`collect_hook_decisions` surfaces both) — advisory like a blocked read.
        if reason.starts_with(REMOTE_WRITE_REASON_PREFIX)
            || reason.starts_with(crate::install_fence::REASON_PREFIX)
        {
            let command = context
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            append_remote_write_deny(decisions_path, scope, phase, &reason, command);
        } else if reason.starts_with(ESTATE_DENY_REASON_PREFIX) {
            // ESTATE-COMMAND FENCE (issue #463): the POSTURE decides advisory vs fatal, and either
            // arm is a real decision record naming the TOOL and the COMMAND (`append_estate_deny`).
            // Recon / pre-build units (`fences_writes` or `pre_build_scope`): ADVISORY — the
            // write-path call is blocked, the graph is untouched, the seat continues with the
            // remedy, and the fold discloses it as `workerToolCallDenied`. Code-executing units:
            // FATAL — a write escape on the shared graph is not recoverable, so the unit is denied
            // under the same `boundary-deny:` class the fence always emitted.
            let command = context
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let advisory = match boundary {
                Some(b) => b.write_posture.fences_writes() || b.pre_build_scope,
                None => write_posture_from_env().fences_writes() || pre_build_scope_from_env(),
            };
            append_estate_deny(
                decisions_path,
                scope,
                phase,
                tool,
                &reason,
                command,
                !advisory,
            );
        } else {
            append_boundary_deny(decisions_path, scope, phase, &reason, fatal);
        }
        eprintln!("wicked-governance: DENY ({reason})");
        return 2;
    }

    // PHASE SCOPE — the SECOND boundary (core#296), judged next and for the same reasons the
    // filesystem one is judged first: it needs no policy store, and a call that has left its
    // phase's scope has already left "here". The two are orthogonal — the filesystem boundary asks
    // WHERE this unit may write (and the worktree is inside its own roots, which is why run
    // d1bc72c2's recon-phase write to `src/board/attentionReason.ts` was correctly `allow`ed by it);
    // this asks WHAT KIND of file THIS PHASE may write. The flag is per-unit state, so it arrives
    // by the same route the roots do: explicit on the in-process carrier, env on the subprocess one.
    let pre_build_scope = match boundary {
        Some(b) => b.pre_build_scope,
        None => pre_build_scope_from_env(),
    };
    let write_posture = match boundary {
        Some(b) => b.write_posture,
        None => write_posture_from_env(),
    };
    // The worktree the path must be judged against: explicit on the in-process carrier, the
    // process cwd on the subprocess one — the same split the roots use just above. The creator
    // fence (F-4R2-004) also needs the home (for `~` spellings) and the armed write roots (to
    // NAME where the deliverable belongs) — read by the same split.
    let scope_cwd = match boundary {
        Some(b) => b.cwd.clone(),
        None => std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    };
    let scope_home = match boundary {
        Some(b) => b.home.clone(),
        None => std::env::var_os("HOME").map(std::path::PathBuf::from),
    };
    let scope_roots: Vec<std::path::PathBuf> = match boundary {
        Some(b) => b.deliverable_roots.clone(),
        None => deliverable_roots_from_env(),
    };

    // POST-HOC WITNESS (issue #541 items 1-3): for ReadOnly units, compare the entry list of the
    // write roots (write set minus notes root) against the snapshot stored after the LAST allowed
    // Bash call. A mismatch names the changed paths under a typed `witness-deny:` claim.
    // Runs on BOTH carriers: the sidecar is keyed by decisions-dir+phase, which both share.
    let witness_roots = compute_witness_roots(boundary);
    let witness_path_buf = write_root_witness_path(decisions_path, phase);
    if write_posture == crate::write_posture::WritePosture::ReadOnly && !witness_roots.is_empty() {
        if let Some(stored) = read_write_root_witness(&witness_path_buf) {
            let mut current_entries = Vec::new();
            let mut current_collector = CollectorKind::GitLsFiles;
            for root in &witness_roots {
                let kind = collect_dir_entries_for_witness(root, &mut current_entries);
                if kind == CollectorKind::RawWalk {
                    current_collector = CollectorKind::RawWalk;
                }
            }
            current_entries.sort_unstable();
            // If the collector changed (e.g. .git became unavailable since the snapshot was
            // taken), the two entry sets are not comparable — re-snapshot and admit rather than
            // producing a false deny.
            if stored.collector != current_collector {
                let new_snapshot = WitnessSnapshot {
                    roots: witness_roots.clone(),
                    collector: current_collector,
                    entries: current_entries,
                };
                write_write_root_witness(&witness_path_buf, &new_snapshot);
            } else {
                let changed = diff_witness_entries(&stored.entries, &current_entries);
                if !changed.is_empty() {
                    let reason = format!(
                        "write-root-mutated: the admitted write roots changed since the last \
                         allowed Bash call — changed paths: {} (issue #541)",
                        changed.join(", ")
                    );
                    append_witness_deny(decisions_path, scope, phase, &changed);
                    eprintln!("wicked-governance: DENY ({reason})");
                    return 2;
                }
            }
        }
    }

    if let Some(reason) = phase_scope_denial(
        pre_build_scope,
        write_posture,
        context,
        tool,
        &scope_cwd,
        scope_home.as_deref(),
        &scope_roots,
    ) {
        append_phase_scope_deny(
            decisions_path,
            scope,
            phase,
            tool,
            &reason,
            context.get("command").and_then(serde_json::Value::as_str),
        );
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
                tool,
                &crate::diagnostic::with_cause("store open failed", &e),
            );
            eprintln!(
                "wicked-governance: DENY ({})",
                crate::diagnostic::with_cause("open store failed", &e)
            );
            return 2;
        }
    };

    let phases = crate::scope::phase_aliases(phase, phase_alias, catalog_alias);
    let selected = match select_any(&store, scope, &phases, context) {
        Ok(s) => s,
        Err(e) => {
            append_infra_deny(
                decisions_path,
                scope,
                phase,
                tool,
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
        _ => {
            // ALLOWED: the seat's shell runs this call, so its trailing `cd` is where the install
            // fence judges the next one from (review of #456, F1). Only here — every deny above
            // returned 2 before this point, and a refused call never moved the shell.
            if tool == "Bash" {
                // Install-fence cwd tracking is subprocess-only (boundary.is_none()).
                if boundary.is_none() {
                    if let Some(command) =
                        context.get("command").and_then(serde_json::Value::as_str)
                    {
                        let cwd = std::env::current_dir()
                            .unwrap_or_else(|_| std::path::PathBuf::from("."));
                        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
                        track_install_fence_cwd(command, &cwd, home.as_deref(), &install_state);
                    }
                }
                // POST-HOC WITNESS: snapshot the witness roots (write set minus notes root) AFTER
                // an allowed Bash call, on BOTH carriers, so the next invocation can detect
                // mutations and name the changed paths (items 1-3 of the review).
                if write_posture == crate::write_posture::WritePosture::ReadOnly
                    && !witness_roots.is_empty()
                {
                    let mut entries = Vec::new();
                    let mut collector = CollectorKind::GitLsFiles;
                    for root in &witness_roots {
                        let kind = collect_dir_entries_for_witness(root, &mut entries);
                        if kind == CollectorKind::RawWalk {
                            collector = CollectorKind::RawWalk;
                        }
                    }
                    entries.sort_unstable();
                    let snapshot = WitnessSnapshot {
                        roots: witness_roots.clone(),
                        collector,
                        entries,
                    };
                    write_write_root_witness(&witness_path_buf, &snapshot);
                }
            }
            0
        }
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
    std::env::temp_dir()
        .join("wicked-core-gov")
        .join(run_dir_name(run_id))
}

/// The per-run DIRECTORY NAME every engine-owned per-run scratch tree keys on — the governance
/// dir above and the read-only notes root ([`crate::worktree_guard::notes_root`], core#464) —
/// so two runs can never share one and a path-hostile id cannot escape the parent. Never empty:
/// an empty (or fully-escaped-away) run_id would otherwise resolve a caller to the bare ROOT, and
/// `run_session`'s fresh-launch `remove_dir_all` would wipe EVERY run's artifacts (Copilot). A
/// non-empty placeholder keeps each run under its own subdir.
pub(crate) fn run_dir_name(run_id: &str) -> String {
    let enc = encode_run_id(run_id);
    if enc.is_empty() {
        "_empty".to_string()
    } else {
        enc
    }
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

/// (issue #463) Companion key on the ARMED marker naming the CARRIER that armed the unit
/// ([`CARRIER_WRAPPED_CLI`] | [`CARRIER_ACP`]) — the launch-context fact the fold reads back so a
/// refusal replayed from this log is attributed to the carrier that recorded it, instead of the
/// fold assuming the wrapped path. Absent on markers written before this key existed.
const ARMED_CARRIER_KEY: &str = "_wicked_gov_carrier";

/// The carrier label of the wrapped-CLI path (`--settings` gate-hook injection) — the vocabulary
/// `GovernanceContextArmed.path` and `WorkerToolCallDenied.carrier` already use.
pub(crate) const CARRIER_WRAPPED_CLI: &str = "wrapped_cli";
/// The carrier label of the ACP permission-bridge path.
pub(crate) const CARRIER_ACP: &str = "acp";

/// Append the ARMED sentinel for `phase` to the decisions log (under the same advisory lock as claims).
/// The carrier-less spelling, kept for the tests that drive the fold directly (a log an older
/// launcher wrote); both carriers call [`write_armed_marker_for`] in production.
#[cfg(test)]
pub fn write_armed_marker(decisions_path: &Path, phase: &str) -> anyhow::Result<()> {
    write_armed_marker_for(decisions_path, phase, None)
}

/// Append the ARMED sentinel for `phase` to the decisions log (under the same advisory lock as
/// claims), naming the CARRIER that armed the unit under [`ARMED_CARRIER_KEY`] (issue #463).
/// Called by the launcher when it arms input governance for a governed unit, BEFORE the CLI runs;
/// `collect_hook_decisions` stamps the carrier onto every record of the phase.
pub fn write_armed_marker_for(
    decisions_path: &Path,
    phase: &str,
    carrier: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(parent) = decisions_path.parent() {
        create_dir_all_private(parent)?;
    }
    let mut marker = serde_json::json!({ ARMED_MARKER_KEY: phase });
    if let Some(c) = carrier {
        marker[ARMED_CARRIER_KEY] = serde_json::Value::String(c.to_string());
    }
    let mut line = marker.to_string();
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
/// Write a fence claim WITH its tool-call annotation as ONE buffer (F3, DES-L4 PR-②): the fence
/// appenders return before the policy path writes its `TOOL_CALL_KEY` line, so a claim appended
/// alone reads `(unknown)` in `collect_hook_decisions` — and the `workerToolCallDenied` the fold
/// discloses from it would name no tool. Same atomicity argument as `evaluate_tool_call`'s policy
/// path and `append_estate_deny`: one small `write_all` under the advisory lock cannot be
/// interleaved by a concurrent hook subprocess even if the lock degrades.
fn append_annotated_claim(decisions_path: &str, phase: &str, tool: &str, claim: &ConformanceClaim) {
    let annotation = serde_json::json!({
        TOOL_CALL_KEY: if tool.is_empty() { "tool-call" } else { tool },
        TOOL_CALL_PHASE_KEY: phase,
    })
    .to_string()
        + "\n";
    let Ok(mut claim_line) = serde_json::to_string(claim) else {
        return;
    };
    claim_line.push('\n');
    let combined = annotation + &claim_line;
    let path = Path::new(decisions_path);
    if let Some(parent) = path.parent() {
        let _ = create_dir_all_private(parent);
    }
    let _ = with_append_lock(path, || {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        f.write_all(combined.as_bytes())
    });
}

fn append_infra_deny(decisions_path: &str, scope: &str, phase: &str, tool: &str, reason: &str) {
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
    append_annotated_claim(decisions_path, phase, tool, &claim);
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
/// omission rather than by enumeration: `is_advisory_deny` is an allowlist that fires ONLY for
/// [`BOUNDARY_READ_DENY_PREFIX`] and [`PHASE_SCOPE_DENY_PREFIX`], so a claim carrying this prefix —
/// or any other deny the fold/drain ever sees — is treated as fatal. `append_boundary_deny` reaches
/// here whenever `is_write` is set; there is no third writer-side category.
const BOUNDARY_WRITE_DENY_PREFIX: &str = "boundary-deny:";
/// Claim-id prefix for an ADVISORY boundary deny: a READ outside the sandbox. The tool-call is STILL
/// blocked (the worker never reads the file), but a blocked read leaks nothing and the worker adapts,
/// so it is recorded for audit and does NOT fail the unit (P8 #10 / core#219). Whether the blocked
/// read MATTERED is decided by the unit's own output gate, not by the containment event.
const BOUNDARY_READ_DENY_PREFIX: &str = "boundary-read-deny:";

/// The evaluator identity stamped on a PHASE-SCOPE deny (core#296). Distinct from
/// [`BOUNDARY_EVALUATOR`] on purpose: the two answer different questions (WHERE may this unit write
/// vs WHAT KIND of file may this PHASE write), and an operator reading the log has to be able to
/// tell "the recon phase tried to write code" from "something reached outside the worktree"
/// WITHOUT reading the prose — filing the second under the first is the mislabelling
/// [`append_boundary_deny`]'s own doc argues against.
const PHASE_SCOPE_EVALUATOR: &str = "wicked-governance-phase-scope";
/// The rule id carried in the claim's `policy_ids`, so the denying rule has a NAME an operator can
/// grep, alert on, and cite — the concrete thing missing from run d1bc72c2, whose hook records read
/// `decision=allow, denyingPolicy=None`. Engine-owned (`engine:` prefix) rather than a
/// `wicked-governance` policy row: it is a structural property of the workflow def, not something an
/// operator authors or edits per run.
pub(crate) const PHASE_SCOPE_RULE_ID: &str = "engine:pre-build-scope";
/// Claim-id prefix for a phase-scope deny. ADVISORY, for the same reason a blocked out-of-boundary
/// READ is (core#219): the harmful thing — production code appearing before the build phase — was
/// PREVENTED, the worker is told exactly how to proceed, and it adapts. Failing the unit on the
/// first refused `Write` would conflate prevention with violation, and would turn every design
/// phase that reaches for a `.ts` file into a failed run — which is how a control gets switched
/// off, and a control that is off is worse than none because it is believed
/// ([`crate::path_policy`]'s module doc). What the phase actually produced is judged by its own
/// output gate and required deliverables, not by this containment event.
const PHASE_SCOPE_DENY_PREFIX: &str = "phase-scope-deny:";

/// Claim-id prefix for a post-hoc write-root witness deny (issue #541, item 3 of the review).
/// Fatal: the write roots watched by the witness changed between two gate-hook invocations or
/// after the unit's last allowed Bash call, proving an unmodelled write escaped the fence.
/// The changed paths are listed in `obligations` so the fold can name them without parsing prose.
const WITNESS_DENY_PREFIX: &str = "witness-deny:";

fn append_witness_deny(decisions_path: &str, scope: &str, phase: &str, changed_paths: &[String]) {
    let paths_str = changed_paths.join(", ");
    let reason = format!(
        "write-root-mutated: the admitted write roots changed since the \
         last allowed Bash call — changed paths: {paths_str} (issue #541)"
    );
    let claim = ConformanceClaim {
        claim_id: format!("{WITNESS_DENY_PREFIX}{phase}"),
        scope: scope.to_string(),
        phase: phase.to_string(),
        policy_ids: vec!["engine:write-root-witness".to_string()],
        decision: Decision::Deny,
        obligations: changed_paths.to_vec(),
        evaluated_context_ref: "sha256:witness".to_string(),
        criteria: format!("write-root witness: {reason}"),
        evaluator_identity: BOUNDARY_EVALUATOR.to_string(),
        evaluated_at: crate::clock::eval_now(),
    };
    let _ = append_decision(Path::new(decisions_path), &claim);
}

/// Record a PHASE-SCOPE block: a pre-build phase's `Write`/`Edit` to a non-documentation path
/// (core#296). The caller has already exited 2 — the tool call never runs. This is what makes the
/// refusal legible afterwards: `criteria` states the rule, `policy_ids` names it
/// ([`PHASE_SCOPE_RULE_ID`]), `obligations` carries the same actionable sentence the worker saw.
///
/// `policy_ids` is what closes the reported gap most directly: [`collect_hook_decisions`] reports
/// the first id of a Deny as `denying_policy`, so `GovernanceHookFired` for this refusal reads
/// `decision=deny, denyingPolicy=engine:pre-build-scope` where run d1bc72c2's read
/// `decision=allow, denyingPolicy=None`. The event's `tool_name` still shows `(unknown)` here, as
/// it does for every pre-policy block: the tool-call annotation is written further down
/// [`evaluate_tool_call`], after the boundary and scope checks have already returned. The tool and
/// the path are in the `reason` either way, so the record names them — this is a shape shared with
/// [`append_boundary_deny`], not a new hole.
/// Record a phase-scope refusal as a REAL decision record naming the tool (annotation, F3) and —
/// for the `Bash` arm — the offending command at `obligations[1]`, so the fold can disclose
/// `workerToolCallDenied{tool, command}` through [`HookDecisionRecord::phase_scope_refusal`]
/// without re-parsing prose (DES-L4 PR-②). `command` is `None` for a path-bearing tool.
fn append_phase_scope_deny(
    decisions_path: &str,
    scope: &str,
    phase: &str,
    tool: &str,
    reason: &str,
    command: Option<&str>,
) {
    let claim = ConformanceClaim {
        // Keyed on `phase` only, for the same reason `append_infra_deny` is: `scope` embeds `/`
        // and would make an unbounded claim symbol.
        claim_id: format!("{PHASE_SCOPE_DENY_PREFIX}{phase}"),
        scope: scope.to_string(),
        phase: phase.to_string(),
        policy_ids: vec![PHASE_SCOPE_RULE_ID.to_string()],
        decision: Decision::Deny,
        obligations: vec![
            reason.to_string(),
            command.map(str::to_string).unwrap_or_default(),
        ],
        evaluated_context_ref: "sha256:phase-scope".to_string(),
        criteria: format!("phase scope (advisory: blocked, worker continues): {reason}"),
        evaluator_identity: PHASE_SCOPE_EVALUATOR.to_string(),
        evaluated_at: crate::clock::eval_now(),
    };
    append_annotated_claim(decisions_path, phase, tool, &claim);
}

/// Record a filesystem-boundary block. `fatal` picks whether it ABORTS the unit (a write/escape) or
/// is ADVISORY (a read probe, or a benign write into the worker's own `~/.claude` tree — core#235).
/// Either way the caller has already exited 2 — the tool-call is blocked. The `reason` carries the
/// accurate `(write)`/`(read)` from the boundary `Denial`, so an advisory WRITE is still honestly
/// described even though it shares the advisory prefix a read uses (the fold keys on that prefix).
/// (F-7R2-012) Claim-id prefix of a REMOTE-WRITE refusal — a worker seat's `git push` / `gh pr
/// create` / `gh api` mutation refused by the command filter
/// ([`crate::remote_write_fence::remote_write_command`]). Advisory (the call is blocked, the seat
/// continues with the remedy), recorded by [`BOUNDARY_EVALUATOR`] like the read denies, and the
/// one claim shape the fold turns into a `workerToolCallDenied` event.
pub(crate) const REMOTE_WRITE_DENY_PREFIX: &str = "remote-write-deny:";

/// The leading text every remote-write refusal reason carries
/// (`RemoteWriteHit::reason`), by which `evaluate_tool_call` routes it to
/// [`append_remote_write_deny`] instead of the boundary recorder.
pub(crate) const REMOTE_WRITE_REASON_PREFIX: &str = "remote-write fence:";

/// (issue #463) Claim-id prefix of the ADVISORY arm of an ESTATE-DENY refusal — a Bash invocation
/// the estate fence ([`classify_estate_command`]) caught on a unit whose posture fences writes
/// (recon / pre-build): a `wicked-estate` write subcommand, or the shim / MCP without `--readonly`
/// or without a pinned store. Advisory by the allowlist (`is_advisory_deny`) and the one estate
/// shape the fold discloses as `workerToolCallDenied` (`HookDecisionRecord::estate_refusal`). The
/// FATAL arm (a code-executing unit) records the same refusal under `boundary-deny:` — the class the
/// fence always emitted — so the unit is denied; both arms carry the offending command at
/// `obligations[1]` and the tool-call annotation, so no record reads `(unknown)`.
pub(crate) const ESTATE_DENY_PREFIX: &str = "estate-deny:";

/// The leading text every estate-deny reason carries, by which `evaluate_tool_call` routes it to
/// [`append_estate_deny`] instead of the default boundary recorder.
pub(crate) const ESTATE_DENY_REASON_PREFIX: &str = "estate-deny fence:";

/// The remedy every estate-deny refusal carries to the seat and onto the wire
/// (`workerToolCallDenied.remedy`) — the allowed transport, spelled out (DES-GROUNDING-001 §7.1).
pub(crate) const ESTATE_DENY_REMEDY: &str =
    "ground through a read-only `wicked-estate` subcommand, or through the estate stdio shim / \
     `wicked-estate-mcp` with `--readonly` AND a pinned store (`--db <path>`, or \
     WICKED_ESTATE_DB / WICKED_HOME / WICKED_MEMORY_DB in the worker environment); indexing and \
     every other graph write belong to repo onboarding, never to a governed unit";

/// Record a remote-write refusal: `obligations[0]` is the reason (with the remedy), `obligations[1]`
/// the OFFENDING COMMAND, so the fold can name what the seat tried without re-parsing prose.
fn append_remote_write_deny(
    decisions_path: &str,
    scope: &str,
    phase: &str,
    reason: &str,
    command: &str,
) {
    let claim = ConformanceClaim {
        claim_id: format!("{REMOTE_WRITE_DENY_PREFIX}{phase}"),
        scope: scope.to_string(),
        phase: phase.to_string(),
        policy_ids: vec![],
        decision: Decision::Deny,
        obligations: vec![reason.to_string(), command.to_string()],
        evaluated_context_ref: "sha256:remote-write-fence".to_string(),
        criteria: format!(
            "remote-write fence (advisory: blocked, worker continues; delivery is the deliver \
             phase's job): {reason}"
        ),
        evaluator_identity: BOUNDARY_EVALUATOR.to_string(),
        evaluated_at: crate::clock::eval_now(),
    };
    // A remote-write refusal is always a shell command (F3: annotated, never `(unknown)`).
    append_annotated_claim(decisions_path, phase, "Bash", &claim);
}

/// Record an estate-command refusal (issue #463) as a REAL decision record naming the tool and
/// the command: the tool-call annotation ([`TOOL_CALL_KEY`]) rides IN THE SAME BUFFER as the
/// claim — the boundary and scope checks return before the policy path writes its annotation, so
/// without this every pre-policy block reads `(unknown)` in `collect_hook_decisions` — and
/// `obligations[0]` is the reason (with the remedy), `obligations[1]` the OFFENDING COMMAND, so
/// the fold can name what the seat tried without re-parsing prose.
///
/// `fatal` picks the arm: ADVISORY (recon / pre-build posture) records under
/// [`ESTATE_DENY_PREFIX`] — advisory by the allowlist, disclosed as `workerToolCallDenied`; FATAL
/// (a code-executing unit) records under [`BOUNDARY_WRITE_DENY_PREFIX`], the class the fence has
/// always emitted for a write escape, so the fold denies the unit exactly as before — now with the
/// tool named. Mirror of [`append_remote_write_deny`] for the record shape.
fn append_estate_deny(
    decisions_path: &str,
    scope: &str,
    phase: &str,
    tool: &str,
    reason: &str,
    command: &str,
    fatal: bool,
) {
    let (prefix, criteria) = if fatal {
        (
            BOUNDARY_WRITE_DENY_PREFIX,
            format!(
                "estate command fence (fatal: a write path on the shared graph from a \
                 code-executing unit): {reason}"
            ),
        )
    } else {
        (
            ESTATE_DENY_PREFIX,
            format!("estate command fence (advisory: blocked, worker continues): {reason}"),
        )
    };
    let claim = ConformanceClaim {
        // Keyed on `phase` only, for the same reason `append_infra_deny` is.
        claim_id: format!("{prefix}{phase}"),
        scope: scope.to_string(),
        phase: phase.to_string(),
        policy_ids: vec![],
        decision: Decision::Deny,
        obligations: vec![reason.to_string(), command.to_string()],
        evaluated_context_ref: "sha256:estate-deny".to_string(),
        criteria,
        evaluator_identity: BOUNDARY_EVALUATOR.to_string(),
        evaluated_at: crate::clock::eval_now(),
    };
    // Annotation + claim as ONE buffer under the advisory lock — the same atomicity argument the
    // policy path makes in `evaluate_tool_call`: a single `write_all` of a small buffer cannot be
    // interleaved by a concurrent hook subprocess even if the lock degrades.
    let annotation = serde_json::json!({
        TOOL_CALL_KEY: if tool.is_empty() { "tool-call" } else { tool },
        TOOL_CALL_PHASE_KEY: phase,
    })
    .to_string()
        + "\n";
    let Ok(mut claim_line) = serde_json::to_string(&claim) else {
        return;
    };
    claim_line.push('\n');
    let combined = annotation + &claim_line;
    let path = Path::new(decisions_path);
    if let Some(parent) = path.parent() {
        let _ = create_dir_all_private(parent);
    }
    let _ = with_append_lock(path, || {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        f.write_all(combined.as_bytes())
    });
}

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

/// Whether a Deny claim is ADVISORY — recorded for audit but NOT unit-fatal. Two members, both
/// cases of containment SUCCEEDING rather than of a unit misbehaving beyond recovery:
///
/// * a blocked out-of-boundary READ — the read was prevented, nothing leaked, the worker adapts;
///   failing the whole unit for it conflates prevention with violation (P8 #10 / core#219);
/// * a blocked PHASE-SCOPE write (core#296) — the production-code write a pre-build phase had no
///   business making never landed, and the worker was handed the two ways forward.
///
/// An ALLOWLIST keyed on BOTH the evaluator identity AND the claim-id prefix, so a policy deny, a
/// boundary write escape, or an infra deny can never be mistaken for advisory; anything the
/// fold/drain has not been taught here is fatal by omission. The worker cannot forge either: the
/// decisions log lives outside its write boundary and is written only by the gate-hook.
fn is_advisory_deny(claim: &ConformanceClaim) -> bool {
    if claim.decision != Decision::Deny {
        return false;
    }
    // A refused `git push` / `gh pr create` (F-7R2-012) joins the blocked read here: the push
    // never happened, the seat was handed the remedy — prevention, not a violation to fail the
    // unit for.
    // An estate-deny (issue #463, advisory arm only — written only when posture fences writes):
    // the write-path command was prevented, the graph is untouched, and the seat was handed the
    // remedy and can adapt. On a code-executing unit the deny is recorded as `boundary-deny:` (fatal)
    // and never reaches this arm.
    (claim.evaluator_identity == BOUNDARY_EVALUATOR
        && (claim.claim_id.starts_with(BOUNDARY_READ_DENY_PREFIX)
            || claim.claim_id.starts_with(REMOTE_WRITE_DENY_PREFIX)
            || claim.claim_id.starts_with(ESTATE_DENY_PREFIX)))
        || (claim.evaluator_identity == PHASE_SCOPE_EVALUATOR
            && claim.claim_id.starts_with(PHASE_SCOPE_DENY_PREFIX))
}

/// If `v` is an armed-marker object, the phase it marks; else `None`. Checks the ROOT key
/// (`v.get(ARMED_MARKER_KEY)`), NOT a substring — a substring match would let a crafted claim whose
/// `criteria`/`obligations` merely CONTAIN the marker string be silently skipped by the fold, bypassing
/// its Deny (gemini/Copilot security-critical). A real `ConformanceClaim` never carries this root key.
fn marker_phase(v: &serde_json::Value) -> Option<&str> {
    v.get(ARMED_MARKER_KEY).and_then(|x| x.as_str())
}

/// The carrier an ARMED marker names ([`ARMED_CARRIER_KEY`], issue #463), or `None` on an older
/// marker. Only meaningful on a value `marker_phase` already accepted.
fn marker_carrier(v: &serde_json::Value) -> Option<&str> {
    v.get(ARMED_CARRIER_KEY).and_then(|x| x.as_str())
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
    /// The recording claim's id — its PREFIX names the recorder (`boundary-deny:`,
    /// `remote-write-deny:`, `phase-scope-deny:`, a policy claim's own id).
    pub claim_id: String,
    /// The claim's `obligations` — for a deny, `[0]` is the operator-facing reason; a
    /// remote-write refusal (F-7R2-012) and an estate refusal (issue #463, either arm) carry the
    /// offending command at `[1]`.
    pub obligations: Vec<String>,
    /// (issue #463) The carrier that armed this unit's governance — [`CARRIER_WRAPPED_CLI`] |
    /// [`CARRIER_ACP`] — read off the phase's ARMED marker, so a refusal the fold replays from the
    /// log is attributed to the carrier that recorded it. `None` for a log an older launcher wrote.
    pub carrier: Option<String>,
}

impl HookDecisionRecord {
    /// (F-7R2-012) Whether this record is a remote-write refusal the fold discloses as
    /// `workerToolCallDenied`: `(reason, command)` when it is.
    pub fn remote_write_refusal(&self) -> Option<(String, String)> {
        if self.decision != "deny" || !self.claim_id.starts_with(REMOTE_WRITE_DENY_PREFIX) {
            return None;
        }
        Some((
            self.obligations.first().cloned().unwrap_or_default(),
            self.obligations.get(1).cloned().unwrap_or_default(),
        ))
    }

    /// (issue #463) Whether this record is an advisory estate-deny refusal the fold discloses as
    /// `workerToolCallDenied`: `(reason, command)` when it is. Only the advisory (recon-posture)
    /// arm uses `ESTATE_DENY_PREFIX`; fatal denies ride `boundary-deny:` and do not reach here.
    pub fn estate_refusal(&self) -> Option<(String, String)> {
        if self.decision != "deny" || !self.claim_id.starts_with(ESTATE_DENY_PREFIX) {
            return None;
        }
        Some((
            self.obligations.first().cloned().unwrap_or_default(),
            self.obligations.get(1).cloned().unwrap_or_default(),
        ))
    }

    /// (DES-L4 PR-②, R7) Whether this record is a phase-scope refusal the fold discloses as
    /// `workerToolCallDenied`: `(reason, command)` when it is — `command` is the offending shell
    /// command for the `Bash` arm and empty for a path-bearing tool (the reason names the path).
    pub fn phase_scope_refusal(&self) -> Option<(String, String)> {
        if self.decision != "deny" || !self.claim_id.starts_with(PHASE_SCOPE_DENY_PREFIX) {
            return None;
        }
        Some((
            self.obligations.first().cloned().unwrap_or_default(),
            self.obligations.get(1).cloned().unwrap_or_default(),
        ))
    }
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
    let mut carrier: Option<String> = None; // from THIS phase's armed marker (issue #463)
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
        // The armed marker names the carrier that armed THIS phase; a hook-fired sentinel is skipped.
        if let Some(mp) = marker_phase(&v) {
            if mp == phase {
                carrier = marker_carrier(&v).map(str::to_string);
            }
            continue;
        }
        if fired_phase(&v).is_some() {
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
            claim_id: claim.claim_id,
            obligations: claim.obligations,
            carrier: carrier.clone(),
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
///
/// Returns a STRUCTURED [`UnitDenial`] (usability review #1), not just prose: a real Deny claim
/// carries its claim id, its firing policy ids, and — when the log's tool-call annotation names it —
/// the denied tool, so the UI can render "rule X denied tool Y in unit-N" from fields instead of
/// parsing a sentence. The fail-closed arms carry prose only (there is no claim to cite).
pub fn fold_input_denial(
    store: &mut dyn GraphStore,
    run_id: &str,
    attempt: u32,
    phase: &str,
    governed: bool,
) -> anyhow::Result<Option<crate::domain::UnitDenial>> {
    // Every denial this fold produces is an input-governance deny for `phase`.
    let fail_closed = |reason: String| {
        let mut d = crate::domain::UnitDenial::new("input_governance", reason);
        d.phase = Some(phase.to_string());
        d
    };
    let path = decisions_path_for(run_id, attempt);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        // No log at all: a GOVERNED unit MUST have its launcher-written armed marker → its absence means
        // the evidence was never written or the whole gov dir was erased → fail CLOSED. An ungoverned
        // unit legitimately has no log.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(governed.then(|| fail_closed(format!(
                "input governance denied {phase} (fail-closed): governed unit produced NO decisions log \
                 (hook never fired or evidence erased)"
            ))))
        }
        // A non-NotFound read error (permission / sharing) is un-evaluable governance evidence ⇒ deny
        // (fail closed) via the normal terminal path, never a run-wedging Err.
        Err(e) => {
            return Ok(Some(fail_closed(format!(
                "input governance denied {phase} (fail-closed): could not read decisions log: {e}"
            ))))
        }
    };
    let mut denial: Option<crate::domain::UnitDenial> = None;
    let mut saw_marker = false;
    let mut saw_hook_fired = false;
    let mut has_claim_lines = false; // any ConformanceClaim present for `phase`
    let mut pending_tool: Option<String> = None; // tool name from the last annotation (this phase)
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
                    denial = Some(fail_closed(format!(
                        "input governance denied {phase} (fail-closed): corrupted decision line: {e}"
                    )));
                }
                pending_tool = None;
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
        // Tool-call annotation written by `run_gate_hook` immediately before each claim so the tool
        // name can be recovered. NOT a `ConformanceClaim` — note the tool for the NEXT claim on this
        // phase (cleared when the annotation belongs to a different phase, same as
        // `collect_hook_decisions`, so a stale name never attaches to another phase's claim).
        if let Some((tool, ann_phase)) = tool_call_entry(&v) {
            pending_tool = (ann_phase == phase).then(|| tool.to_string());
            continue;
        }
        let claim: ConformanceClaim = match serde_json::from_value(v) {
            Ok(c) => c,
            Err(e) => {
                if denial.is_none() {
                    denial = Some(fail_closed(format!(
                        "input governance denied {phase} (fail-closed): corrupted decision line: {e}"
                    )));
                }
                pending_tool = None;
                continue;
            }
        };
        if claim.phase != phase {
            pending_tool = None;
            continue; // another unit's claim — folded when that unit finishes
        }
        has_claim_lines = true;
        conform(store, &claim)?;
        // Each claim consumes the annotation immediately preceding it — allow or deny — so a tool
        // name can never skip forward past its own claim onto a later one.
        let tool_for_claim = pending_tool.take();
        // An ADVISORY deny — an out-of-boundary READ (P8 #10 / core#219) or a PHASE-SCOPE write
        // (core#296) — is recorded (conform above, for audit) but does NOT fail the unit: the call
        // was blocked, nothing landed or leaked, and the worker adapts. Whether the blocked call
        // MATTERED is judged by the unit's OUTPUT gate and its required deliverables, not by this
        // containment event.
        if denial.is_none() && claim.decision == Decision::Deny && !is_advisory_deny(&claim) {
            denial = Some(crate::domain::UnitDenial {
                source: "input_governance".to_string(),
                reason: format!(
                    "input governance denied a tool-call in {phase} (claim {})",
                    claim.claim_id
                ),
                claim_id: Some(claim.claim_id.clone()),
                rule_ids: claim.policy_ids.clone(),
                denied_tool: tool_for_claim,
                phase: Some(phase.to_string()),
                findings_trimmed: false,
            });
        }
    }
    // POST-HOC WITNESS unit-end check (item 2 of the review): re-scan the witness roots after
    // all tool calls have been evaluated to catch a write that escaped via the last allowed Bash
    // call (no subsequent gate-hook invocation would have detected it).
    if denial.is_none() {
        let witness_path = write_root_witness_path(&path.to_string_lossy(), phase);
        if let Some(stored) = read_write_root_witness(&witness_path) {
            let mut current_entries = Vec::new();
            let mut current_collector = CollectorKind::GitLsFiles;
            for root in &stored.roots {
                let kind = collect_dir_entries_for_witness(root, &mut current_entries);
                if kind == CollectorKind::RawWalk {
                    current_collector = CollectorKind::RawWalk;
                }
            }
            current_entries.sort_unstable();
            // Collector mismatch: re-snapshot and do not deny — the entry sets are not
            // comparable across collector strategies.
            if stored.collector != current_collector {
                let new_snapshot = WitnessSnapshot {
                    roots: stored.roots.clone(),
                    collector: current_collector,
                    entries: current_entries,
                };
                write_write_root_witness(&witness_path, &new_snapshot);
            } else {
                let changed = diff_witness_entries(&stored.entries, &current_entries);
                if !changed.is_empty() {
                    denial = Some(fail_closed(format!(
                        "input governance denied {phase}: write-root mutated after the last \
                         allowed Bash call — changed paths: {} (issue #541 unit-end catch)",
                        changed.join(", ")
                    )));
                }
            }
        }
    }
    // A GOVERNED unit whose log is PRESENT but has lost its armed marker was truncated/edited → the
    // evidence stream is untrustworthy → fail CLOSED (even if no surviving Deny remains).
    if governed && !saw_marker && denial.is_none() {
        denial = Some(fail_closed(format!(
            "input governance denied {phase} (fail-closed): armed marker missing \
             (decisions log tampered or truncated)"
        )));
    }
    // Hook-liveness check: if there are claim lines for this phase but no hook-fired sentinel, the
    // hook process was suppressed while tool calls still executed — deny immediately. The sentinel is
    // written BEFORE any claim evaluation in `run_gate_hook`, so its absence with claims present is
    // impossible in normal operation and indicates hook bypass.
    if governed && saw_marker && has_claim_lines && !saw_hook_fired && denial.is_none() {
        denial = Some(fail_closed(format!(
            "input governance denied {phase} (fail-closed): hook-fired sentinel missing with \
             claim lines present — hook process may have been suppressed (core#34)"
        )));
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
        // Advisory denies — out-of-boundary READs (P8 #10 / core#219) and PHASE-SCOPE writes
        // (core#296) — are audit-only (conform()'d in Pass 1) and never veto the input-governance
        // gate; exclude them from the deny-dominating verdict at every tier. A phase whose ONLY
        // claims are advisory has nothing gate-affecting to resolve, so it is skipped (no spurious
        // veto).
        let verdict = match claims
            .iter()
            .filter(|c| !is_advisory_deny(c))
            .find(|c| c.decision == Decision::Deny)
            .or_else(|| {
                claims
                    .iter()
                    .filter(|c| !is_advisory_deny(c))
                    .find(|c| c.decision == Decision::AllowWithConditions)
            })
            .or_else(|| claims.iter().find(|c| !is_advisory_deny(c)))
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
///
/// A thin delegate: the ONE implementation lives in `wicked_governance::pretool_context`, shared
/// with `rules eval` — the eval replays corpus samples through this exact projection, and a
/// second copy here is how an eval would quietly diverge from the gate it claims to measure.
pub(crate) fn claude_pretool_context(
    raw: &str,
    scope: &str,
    phase: &str,
) -> (serde_json::Value, String) {
    wicked_governance::pretool_context(raw, scope, phase)
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
    catalog_alias: Option<&str>,
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
    let phases = crate::scope::phase_aliases(phase, phase_alias, catalog_alias);
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
        // Severity/rule-type/steering-type are never narrowed for the output gate: the recall
        // report must carry EVERY applicable steering page's rules.
        ..Default::default()
    }
}

/// TEST-ONLY: the pre-posture 5-argument spelling of [`phase_scope_denial`] the PRE-BUILD scope
/// tests (`phase_scope_tests`) were written against — `no_code: bool` maps onto the read-only
/// posture (true) or no fence (false), with no home and no roots, exactly what those tests
/// exercise. Keeps them byte-stable while the production signature carries the F-4R2-004 posture.
#[cfg(test)]
pub(crate) fn legacy_scope(
    pre_build_scope: bool,
    no_code: bool,
    context: &serde_json::Value,
    tool: &str,
    cwd: &std::path::Path,
) -> Option<String> {
    let posture = if no_code {
        crate::write_posture::WritePosture::ReadOnly
    } else {
        crate::write_posture::WritePosture::Full
    };
    phase_scope_denial(pre_build_scope, posture, context, tool, cwd, None, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R7 / R7b / R8 (DES-L4 PR-②; core #483, F-RC1-080, F-RC1-092): `Bash` is judged by its WRITE
    /// TARGETS under a fenced posture with the SAME admission a path-bearing tool gets. Read-only:
    /// a heredoc / redirect / tee / mkdir into the tree is refused (advisory), one under the notes
    /// root is admitted, a command with no write target is not judged; a path-bearing `Write` under
    /// the notes root is admitted too and the refusal names the root. Pre-build: documentation
    /// targets pass, `> src/x` is refused (R7b). Deliverable-roots: inside a declared root passes,
    /// the tree is refused. Full: no fence. Mutation: drop the `tool == "Bash"` arm → every `Some`
    /// here becomes `None`.
    #[test]
    fn bash_write_targets_are_judged_under_a_fenced_posture_and_admitted_under_the_notes_root() {
        use crate::write_posture::WritePosture as P;
        // No `(`/`)` in the scratch path: `shell_tokens` splits bare parens as control operators,
        // and `ThreadId(n)`'s Debug form would truncate every redirect target under it.
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base = std::env::temp_dir().join(format!(
            "wicked-phase-scope-bash-{}-{tid}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let wt = base.join("wt");
        let notes = base.join("notes");
        let inbox = base.join("inbox");
        for d in [&wt.join("src"), &wt.join("docs"), &notes, &inbox] {
            std::fs::create_dir_all(d).unwrap();
        }
        let sh = |c: String| serde_json::json!({ "command": c });
        let notes_roots = vec![notes.clone()];
        let inbox_roots = vec![inbox.clone()];
        let none: &[std::path::PathBuf] = &[];
        let deny =
            |pre: bool, posture: P, ctx: &serde_json::Value, roots: &[std::path::PathBuf]| {
                phase_scope_denial(pre, posture, ctx, "Bash", &wt, None, roots)
            };
        let w = |p: &std::path::Path| p.to_string_lossy().into_owned();

        // READ-ONLY: writes into the tree are refused, naming the target and the notes root.
        for cmd in [
            format!("cat > {} <<'EOF'\nfindings\nEOF", w(&wt.join("notes.md"))),
            format!("echo x > {}", w(&wt.join("src").join("x.rs"))),
            format!("cargo test 2>&1 | tee {}", w(&wt.join("build.log"))),
            format!("mkdir -p {}", w(&wt.join("evidence"))),
            format!("cp {} {}", w(&notes.join("a")), w(&wt.join("a"))),
        ] {
            let d = deny(false, P::ReadOnly, &sh(cmd.clone()), &notes_roots).unwrap_or_else(|| {
                panic!("a read-only Bash write into the tree is refused: {cmd}")
            });
            assert!(
                d.starts_with("phase scope: `Bash` would write")
                    && d.contains(&w(&notes))
                    && d.contains(PHASE_SCOPE_BASH_REMEDY),
                "{d}"
            );
        }
        // READ-ONLY: writes under the notes root are admitted; a command with no write target is
        // not judged; a `/dev/null` sink is not a target.
        for cmd in [
            format!("echo x > {}", w(&notes.join("analysis.md"))),
            format!(
                "cat > {} <<'EOF'\nnotes\nEOF",
                w(&notes.join("sub").join("n.md"))
            ),
            format!(
                "cp {} {}",
                w(&wt.join("src").join("x.rs")),
                w(&notes.join("x.rs"))
            ),
            "ls -la src && cargo test".to_string(),
            "cargo test > /dev/null 2>&1".to_string(),
        ] {
            assert_eq!(
                deny(false, P::ReadOnly, &sh(cmd.clone()), &notes_roots),
                None,
                "admitted / not a write: {cmd}"
            );
        }
        // READ-ONLY with NO notes root: refused, and the refusal says so.
        let d = deny(
            false,
            P::ReadOnly,
            &sh(format!("echo x > {}", w(&wt.join("n.md")))),
            none,
        )
        .expect("no notes root ⇒ nothing may be written");
        assert!(d.contains("has no notes root"), "{d}");
        // The path-bearing tools get the SAME notes-root admission (was: refused everywhere).
        assert_eq!(
            phase_scope_denial(
                false,
                P::ReadOnly,
                &serde_json::json!({ "path": w(&notes.join("x.md")) }),
                "Write",
                &wt,
                None,
                &notes_roots
            ),
            None,
            "a Write under the notes root is admitted"
        );
        let d = phase_scope_denial(
            false,
            P::ReadOnly,
            &serde_json::json!({ "path": w(&wt.join("x.md")) }),
            "Write",
            &wt,
            None,
            &notes_roots,
        )
        .expect("a Write into the tree stays refused");
        assert!(
            d.contains("Nothing in the worktree may be written here")
                && d.contains("write notes only under the unit's notes root")
                && d.contains(&w(&notes)),
            "{d}"
        );

        // PRE-BUILD (R7b): documentation targets pass, production code is refused.
        assert_eq!(
            deny(
                true,
                P::Full,
                &sh(format!(
                    "cat > {} <<'EOF'\n# design\nEOF",
                    w(&wt.join("docs").join("design.md"))
                )),
                none
            ),
            None,
            "pre-build documentation via Bash is allowed"
        );
        let d = deny(
            true,
            P::Full,
            &sh(format!("echo x > {}", w(&wt.join("src").join("x.rs")))),
            none,
        )
        .expect("pre-build production code via Bash is refused");
        assert!(
            d.contains("PRE-BUILD") && d.contains("documentation"),
            "{d}"
        );

        // DELIVERABLE-ROOTS creator: inside a declared root passes, the tree is refused.
        assert_eq!(
            deny(
                false,
                P::DeliverableRoots,
                &sh(format!("echo x > {}", w(&inbox.join("out.html")))),
                &inbox_roots
            ),
            None
        );
        let d = deny(
            false,
            P::DeliverableRoots,
            &sh(format!("echo x > {}", w(&wt.join("index.html")))),
            &inbox_roots,
        )
        .expect("a creator's Bash write into the tree is refused");
        assert!(
            d.contains("declared write roots") && d.contains(&w(&inbox)),
            "{d}"
        );

        // FULL posture, not pre-build: no fence at all.
        assert_eq!(
            deny(
                false,
                P::Full,
                &sh(format!("echo x > {}", w(&wt.join("src").join("x.rs")))),
                none
            ),
            None
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// issue #541 — ReadOnly write fence: `touch`, `python3 -c`, `python3 - <<EOF`, `cat > f`,
    /// `node -e`, `perl -e`, `ruby -e` are all refused; `--version` info invocations and
    /// commands with no write target are admitted; Full posture has no fence.
    #[test]
    fn readonly_fence_denies_touch_and_opaque_interpreters_named_in_541() {
        use crate::write_posture::WritePosture as P;
        // No `(`/`)` in the scratch path: `shell_tokens` splits bare parens as control operators.
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base =
            std::env::temp_dir().join(format!("wicked-541-fence-{}-{tid}", std::process::id(),));
        let _ = std::fs::remove_dir_all(&base);
        let wt = base.join("wt");
        let notes = base.join("notes");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::create_dir_all(&notes).unwrap();
        let notes_roots = vec![notes.clone()];
        let none: &[std::path::PathBuf] = &[];
        let w = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let deny_ro =
            |cmd: String| bash_write_phase_scope(false, P::ReadOnly, &cmd, &wt, None, &notes_roots);
        let deny_ro_noroot =
            |cmd: String| bash_write_phase_scope(false, P::ReadOnly, &cmd, &wt, None, none);
        // touch: denied because the file path is outside the notes root (write target extracted).
        let d = deny_ro(format!("touch {}", w(&wt.join("lockfile"))))
            .expect("touch into the tree is refused for ReadOnly (issue #541)");
        assert!(
            d.contains("would write") && d.contains(PHASE_SCOPE_BASH_REMEDY),
            "{d}"
        );

        // cat > f: redirect target is extracted, denied.
        let d = deny_ro(format!("cat > {}", w(&wt.join("src").join("out.rs"))))
            .expect("cat > f is denied (redirect target)");
        assert!(
            d.contains("would write") && d.contains(PHASE_SCOPE_BASH_REMEDY),
            "{d}"
        );

        // python3 -c: denied by opaque-interpreter word (ReadOnly posture; program-word rule).
        // Both assertion branches ("would write", "interpreter") carry PHASE_SCOPE_BASH_REMEDY.
        let d = deny_ro(format!(
            "python3 -c 'open(\"{}\",\"w\")'",
            w(&wt.join("pwned"))
        ))
        .expect("python3 -c is denied for ReadOnly (issue #541)");
        assert!(
            (d.contains("interpreter") || d.contains("would write"))
                && d.contains(PHASE_SCOPE_BASH_REMEDY),
            "{d}"
        );

        // python3 - <<EOF (stdin heredoc shape): denied by opaque-interpreter word.
        // Heredoc body content is NOT scanned pre-call; the witness catches any out-of-tree
        // writes after the call (#548).
        let d = deny_ro(format!(
            "python3 - <<'EOF'\nopen('{}','w')\nEOF",
            w(&wt.join("pwned"))
        ))
        .expect("python3 - <<EOF is denied for ReadOnly (issue #541)");
        assert!(
            (d.contains("interpreter") || d.contains("would write"))
                && d.contains(PHASE_SCOPE_BASH_REMEDY),
            "{d}"
        );

        // node -e: denied by opaque-interpreter word (ReadOnly posture; program-word rule).
        let d = deny_ro(format!(
            r#"node -e "require('fs').writeFileSync('{}','x')""#,
            w(&wt.join("pwned"))
        ))
        .expect("node -e is denied for ReadOnly (issue #541)");
        assert!(
            (d.contains("interpreter") || d.contains("would write"))
                && d.contains(PHASE_SCOPE_BASH_REMEDY),
            "{d}"
        );

        // perl -e: denied by opaque-interpreter word (ReadOnly posture; program-word rule).
        let d = deny_ro(format!(
            "perl -e 'open(F,\">\",\"{}\")' ",
            w(&wt.join("pwned"))
        ))
        .expect("perl -e is denied for ReadOnly (issue #541)");
        assert!(
            (d.contains("interpreter") || d.contains("would write"))
                && d.contains(PHASE_SCOPE_BASH_REMEDY),
            "{d}"
        );

        // ruby -e: denied by opaque-interpreter word (ReadOnly posture; program-word rule).
        let d = deny_ro(format!(
            "ruby -e 'File.write(\"{}\",\"x\")'",
            w(&wt.join("pwned"))
        ))
        .expect("ruby -e is denied for ReadOnly (issue #541)");
        assert!(
            (d.contains("interpreter") || d.contains("would write"))
                && d.contains(PHASE_SCOPE_BASH_REMEDY),
            "{d}"
        );

        // Controls: --version flags are safe, not denied.
        assert_eq!(
            deny_ro("python3 --version".to_string()),
            None,
            "python3 --version is a read-only info invocation"
        );
        assert_eq!(
            deny_ro("node --version".to_string()),
            None,
            "node --version is read-only"
        );
        assert_eq!(
            deny_ro_noroot("perl -V".to_string()),
            None,
            "perl -V is read-only"
        );

        // A command with no write target and no opaque interpreter is not judged.
        assert_eq!(
            deny_ro("ls -la src && cargo test 2>&1".to_string()),
            None,
            "pure read commands are not judged under ReadOnly"
        );

        // touch into the notes root IS admitted (the target is within admitted_roots).
        assert_eq!(
            deny_ro(format!("touch {}", w(&notes.join("scratch.md")))),
            None,
            "touch under the notes root is admitted"
        );

        // Full posture: no interpreter fence at all.
        assert_eq!(
            bash_write_phase_scope(
                false,
                P::Full,
                &format!("python3 -c 'open(\"{}\",\"w\")'", w(&wt.join("x"))),
                &wt,
                None,
                none
            ),
            None,
            "Full posture has no interpreter fence"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// issue #540 — boundary: `git -C <sibling>` with a write subcommand and
    /// `cd <sibling> && cp …` are denied by `boundary_denial_tracked`; read git subcommands
    /// and in-boundary cds are admitted. Tests Creator posture (boundary is all-posture).
    #[test]
    fn boundary_denies_git_c_and_cd_to_sibling_named_in_540() {
        use crate::path_policy::AllowedRoots;
        // No `(`/`)` in the scratch path: `shell_tokens` splits bare parens as control operators.
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base =
            std::env::temp_dir().join(format!("wicked-540-boundary-{}-{tid}", std::process::id(),));
        let _ = std::fs::remove_dir_all(&base);
        let wt = base.join("run-a").join("wt");
        let sibling = base.join("run-b").join("wt");
        let notes = base.join("notes");
        for d in [&wt.join("src"), &sibling.join("src"), &notes] {
            std::fs::create_dir_all(d).unwrap();
        }
        let roots = AllowedRoots {
            write: vec![wt.clone()],
            read: vec![],
        };
        let ctx = |cmd: &str| serde_json::json!({ "command": cmd });
        let check = |cmd: &str| boundary_denial_with(&roots, &wt, None, None, &ctx(cmd), "Bash");
        let w = |p: &std::path::Path| p.to_string_lossy().into_owned();

        // git -C <sibling> commit: write subcommand → denied (issue #540).
        // Note: paths under system temp are advisory by the core#264 carve-out — the key
        // property is that the call is DENIED (not allowed), not that it is fatal here.
        let (reason, _fatal) = check(&format!("git -C {} commit -m 'x'", w(&sibling)))
            .expect("git -C <sibling> commit is denied");
        assert!(reason.contains("leaves the unit boundary"), "{reason}");

        // git -C <sibling> status: read subcommand → allowed.
        assert_eq!(
            check(&format!("git -C {} status", w(&sibling))),
            None,
            "git -C <sibling> status is a read subcommand — admitted"
        );

        // cd <sibling> && cp: denied (sibling is outside write roots; issue #540).
        // Paths under system temp are advisory by core#264 — the key is DENIED, not fatal here.
        let (reason, _fatal) = check(&format!(
            "cd {} && cp {} .",
            w(&sibling),
            w(&wt.join("README.md"))
        ))
        .expect("cd <sibling> && cp … is denied");
        assert!(
            reason.contains("leaves the unit boundary"),
            "reason names the boundary: {reason}"
        );

        // cd within the worktree: admitted (the sibling check fires only on out-of-boundary cds).
        assert_eq!(
            check(&format!("cd {} && ls", w(&wt.join("src")))),
            None,
            "cd inside the worktree is admitted"
        );

        // git -C <in-wt>: write subcommand but -C lands in the worktree — admitted.
        assert_eq!(
            check(&format!("git -C {} commit -m 'x'", w(&wt.join("src")))),
            None,
            "git -C inside the worktree is admitted even for write subcommands"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// issue #541 criterion 3 — post-hoc witness: `fingerprint_write_roots` detects file creation
    /// and `write_root_witness_path` mirrors `install_fence_cwd_path`.
    #[test]
    fn write_root_witness_fingerprint_detects_mutations() {
        let base = std::env::temp_dir().join(format!("wicked-541-witness-{}", std::process::id(),));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("notes");
        std::fs::create_dir_all(&root).unwrap();
        let roots = vec![root.clone()];

        let fp1 = fingerprint_write_roots(&roots);

        // Create a file → fingerprint changes.
        std::fs::write(root.join("new.md"), "content").unwrap();
        let fp2 = fingerprint_write_roots(&roots);
        assert_ne!(fp1, fp2, "fingerprint detects file creation");

        // Remove the file → back to original.
        std::fs::remove_file(root.join("new.md")).unwrap();
        let fp3 = fingerprint_write_roots(&roots);
        assert_eq!(fp1, fp3, "fingerprint restores when file is removed");

        // Round-trip through sidecar with the new WitnessSnapshot format.
        let sidecar = base.join("decisions").join("witness");
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        assert!(read_write_root_witness(&sidecar).is_none());
        let snap = WitnessSnapshot {
            roots: roots.clone(),
            collector: CollectorKind::RawWalk,
            entries: {
                let mut e = Vec::new();
                let _ = collect_dir_entries_for_witness(&root, &mut e);
                e.sort_unstable();
                e
            },
        };
        write_write_root_witness(&sidecar, &snap);
        let loaded = read_write_root_witness(&sidecar).expect("sidecar loads");
        assert_eq!(loaded.roots, roots, "roots round-trip");
        assert!(
            !loaded.entries.is_empty()
                || std::fs::read_dir(&root)
                    .map(|mut r| r.next().is_none())
                    .unwrap_or(true),
            "entries round-trip (empty dir = empty entries OK)"
        );

        // Sidecar path mirrors install-fence-cwd-path pattern.
        let dp = base.join("gov").join("decisions.ndjson");
        let wp = write_root_witness_path(&dp.to_string_lossy(), "unit-2");
        assert!(
            wp.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("write-root-witness-"))
                .unwrap_or(false),
            "sidecar filename has the expected prefix: {wp:?}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// item 8 — fd-dup operator tokens (`2>&1`, `>&2`) in `touch`/`mkdir` argument lists must
    /// not be treated as write targets; the command must be admitted when the only paths are
    /// within the notes root (F-FIX-S1-01: this run's own triage unit was refused on
    /// `mkdir -p <notes>/… 2>&1`).
    #[test]
    fn touch_and_mkdir_fd_dup_tokens_are_not_targets_item8() {
        use crate::write_posture::WritePosture as P;
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base = std::env::temp_dir().join(format!("wicked-item8-{}-{tid}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let notes = base.join("notes");
        let wt = base.join("wt");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        let notes_roots = vec![notes.clone()];
        let w = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let deny =
            |cmd: &str| bash_write_phase_scope(false, P::ReadOnly, cmd, &wt, None, &notes_roots);
        // touch into notes with a fd-dup suffix: must be ADMITTED (2>&1 is not a path).
        assert_eq!(
            deny(&format!("touch {} 2>&1", w(&notes.join("scratch.md")))),
            None,
            "touch <notes-file> 2>&1 must be admitted — 2>&1 is not a write target"
        );
        // mkdir -p into notes with a fd-dup suffix: must be ADMITTED.
        assert_eq!(
            deny(&format!("mkdir -p {} 2>&1", w(&notes.join("subdir")))),
            None,
            "mkdir -p <notes-dir> 2>&1 must be admitted — 2>&1 is not a write target"
        );
        // touch outside notes still denied (the path IS a real target).
        assert!(
            deny(&format!("touch {}", w(&wt.join("illegal")))).is_some(),
            "touch outside notes must still be denied"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// item 7 — the interpreter fence applies under ReadOnly regardless of `pre_build_scope`.
    /// A pre-build neutral unit must not be able to run `python3 -c` while `touch` is refused.
    #[test]
    fn interpreter_fence_applies_under_pre_build_scope_item7() {
        use crate::write_posture::WritePosture as P;
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base = std::env::temp_dir().join(format!("wicked-item7-{}-{tid}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let wt = base.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        // pre_build_scope=true, ReadOnly posture, no admitted roots.
        let deny = |cmd: &str| bash_write_phase_scope(true, P::ReadOnly, cmd, &wt, None, &[]);
        assert!(
            deny("python3 -c 'open(\"x\",\"w\")'").is_some(),
            "python3 -c must be denied under ReadOnly + pre_build_scope=true"
        );
        assert!(
            deny("node -e 'require(\"fs\").writeFileSync(\"x\",\"y\")'").is_some(),
            "node -e must be denied under ReadOnly + pre_build_scope=true"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// item 5 — `branch`/`tag`/`notes`/`hash-object` removed from `GIT_READ_VERBS`;
    /// `--git-dir=`/`--work-tree=` treated like `-C`; `ln` last-arg is a write target.
    #[test]
    fn git_read_verbs_pruned_and_git_dir_work_tree_ln_item5() {
        use crate::path_policy::AllowedRoots;
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base = std::env::temp_dir().join(format!("wicked-item5-{}-{tid}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let wt = base.join("wt");
        let sibling = base.join("sibling");
        for d in [&wt, &sibling] {
            std::fs::create_dir_all(d).unwrap();
        }
        let roots = AllowedRoots {
            write: vec![wt.clone()],
            read: vec![],
        };
        let ctx = |cmd: &str| serde_json::json!({ "command": cmd });
        let check = |cmd: &str| boundary_denial_with(&roots, &wt, None, None, &ctx(cmd), "Bash");
        let w = |p: &std::path::Path| p.to_string_lossy().into_owned();

        // branch/tag/notes/hash-object are no longer read-only — they are treated as write verbs.
        assert!(
            check(&format!("git -C {} branch my-branch", w(&sibling))).is_some(),
            "git -C <sibling> branch must be denied (branch removed from GIT_READ_VERBS)"
        );
        assert!(
            check(&format!("git -C {} tag v1.0", w(&sibling))).is_some(),
            "git -C <sibling> tag must be denied (tag removed from GIT_READ_VERBS)"
        );
        assert!(
            check(&format!("git -C {} notes add -m x HEAD", w(&sibling))).is_some(),
            "git -C <sibling> notes must be denied (notes removed from GIT_READ_VERBS)"
        );
        assert!(
            check(&format!("git -C {} hash-object -w file", w(&sibling))).is_some(),
            "git -C <sibling> hash-object must be denied (hash-object removed from GIT_READ_VERBS)"
        );

        // --git-dir= treated as write root for non-read verbs.
        assert!(
            check(&format!(
                "git --git-dir={} commit -m x",
                w(&sibling.join(".git"))
            ))
            .is_some(),
            "git --git-dir=<sibling> commit must be denied"
        );
        assert!(
            check(&format!("git --work-tree={} commit -m x", w(&sibling))).is_some(),
            "git --work-tree=<sibling> commit must be denied"
        );

        // --git-dir= in-boundary: admitted for write verbs.
        assert!(
            check(&format!(
                "git --git-dir={} commit -m x",
                w(&wt.join(".git"))
            ))
            .is_none(),
            "git --git-dir=<in-wt> commit must be admitted"
        );

        // ln: last argument is the write target.
        assert!(
            check(&format!(
                "ln -s {} {}",
                w(&wt.join("src")),
                w(&sibling.join("link"))
            ))
            .is_some(),
            "ln to sibling must be denied (last arg is the destination)"
        );
        assert!(
            check(&format!("ln -s something {}", w(&wt.join("link")))).is_none(),
            "ln into the worktree must be admitted"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// item 6 — `sh`/`bash`/`zsh`/`dash` invoked WITHOUT `-c` (script file, stdin, heredoc)
    /// are opaque under ReadOnly. `bash -c '...'` is already handled by the inline rescan.
    #[test]
    fn opaque_shell_invocations_without_c_denied_item6() {
        use crate::write_posture::WritePosture as P;
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base = std::env::temp_dir().join(format!("wicked-item6-{}-{tid}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let wt = base.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let deny = |cmd: &str| bash_write_phase_scope(false, P::ReadOnly, cmd, &wt, None, &[]);

        // bash <<'EOF'\ntouch x\nEOF — heredoc stdin invocation; denied either because
        // heredoc body scanning extracts a write target ("would write") or by the opaque-
        // interpreter check when no static target is found.
        let d = deny("bash <<'EOF'\ntouch x\nEOF").expect("bash heredoc must be denied");
        assert!(
            d.contains("interpreter") || d.contains("opaque") || d.contains("would write"),
            "{d}"
        );

        // sh script.sh — script-file invocation (opaque).
        let d = deny("sh script.sh").expect("sh script.sh must be denied");
        assert!(d.contains("interpreter") || d.contains("opaque"), "{d}");

        // bash -s < f — stdin (-s) invocation (opaque).
        let d = deny("bash -s < f").expect("bash -s < f must be denied");
        assert!(d.contains("interpreter") || d.contains("opaque"), "{d}");

        // zsh script.zsh — script invocation (opaque).
        let d = deny("zsh script.zsh").expect("zsh script.zsh must be denied");
        assert!(d.contains("interpreter") || d.contains("opaque"), "{d}");

        // dash with script — opaque.
        let d = deny("dash -x run.sh").expect("dash -x run.sh must be denied");
        assert!(d.contains("interpreter") || d.contains("opaque"), "{d}");

        // bash alone (no args) — denied: empty arg list is no longer exempt because
        // `echo 'touch x' | bash` tokenises to a segment with empty args, and the
        // vacuous-truth of `[].all(_)` would otherwise admit pipe-fed code execution.
        assert!(
            deny("bash").is_some(),
            "bare bash with no args must be denied under ReadOnly"
        );

        // Pipe-fed interpreter invocations: the right-hand segment has no args (empty arg list),
        // which must be denied — not vacuously admitted as "only info flags".
        let d = deny("echo 'x' | python3").expect("echo 'x' | python3 must be denied");
        assert!(
            d.contains("interpreter") || d.contains("opaque"),
            "echo 'x' | python3 denial must name the interpreter: {d}"
        );
        let d = deny("echo 'touch x' | bash").expect("echo 'touch x' | bash must be denied");
        assert!(
            d.contains("interpreter") || d.contains("opaque"),
            "echo 'touch x' | bash denial must name the interpreter: {d}"
        );
        let d = deny("echo 'touch x' | sh").expect("echo 'touch x' | sh must be denied");
        assert!(
            d.contains("interpreter") || d.contains("opaque"),
            "echo 'touch x' | sh denial must name the interpreter: {d}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// INDEPENDENT REVIEW item 1 / 3a — `is_abs_path_token` covers Unix, Windows and UNC
    /// paths via `Path::is_absolute()`, and rejects `//`-prefixed URL authority tokens and
    /// relative paths. Runs on all platforms without hard-coding path separators.
    #[test]
    fn absolute_path_detection_cross_platform_ir_item1_3a() {
        // Relative paths: never absolute.
        assert!(!is_abs_path_token("relative/x"), "relative path rejected");
        assert!(!is_abs_path_token("./local"), "dot-relative rejected");
        assert!(!is_abs_path_token(""), "empty string rejected");
        // URL authority leftover after splitting on ':' — must be rejected.
        assert!(!is_abs_path_token("//127.0.0.1"), "URL authority rejected");
        assert!(
            !is_abs_path_token("//server/share"),
            "UNC-style // rejected"
        );
        // On Unix, /tmp/x is absolute.
        #[cfg(unix)]
        {
            assert!(is_abs_path_token("/tmp/x"), "Unix abs accepted");
            assert!(is_abs_path_token("/etc/hosts"), "Unix abs accepted");
        }
        // On Windows, drive-letter, extended-length, and UNC paths are absolute.
        #[cfg(windows)]
        {
            assert!(
                is_abs_path_token(r"C:\Users\foo"),
                "Windows drive-letter abs accepted"
            );
            assert!(
                is_abs_path_token(r"\\?\C:\long\path"),
                "Windows extended-length abs accepted"
            );
            assert!(
                is_abs_path_token(r"\\srv\share\dir"),
                "Windows UNC abs accepted"
            );
        }
        // Platform-native temp dir is always absolute and not //-prefixed.
        let td = std::env::temp_dir().to_string_lossy().into_owned();
        assert!(is_abs_path_token(&td), "temp_dir() is absolute: {td}");
        // URL scanner: python3 -c "urlopen('http://127.0.0.1:7701/path')" must produce no
        // abs-path target from the URL — the token after splitting on non-':' separators
        // keeps the full URL form, which is_abs_path_token must reject.
        let url_code = "urlopen('http://127.0.0.1:7701/path')";
        let has_url_hit = url_code
            .split(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '(' | ')' | ',' | ';'))
            .any(is_abs_path_token);
        assert!(
            !has_url_hit,
            "URL token must not be flagged as abs path: {url_code}"
        );
    }

    /// INDEPENDENT REVIEW item 3b — the Inline arm no longer raw-scans the shell `-c` string
    /// for absolute tokens; read commands inside `-c` must not produce write targets.
    #[test]
    fn inline_arm_no_raw_scan_ir_item3b() {
        // sh -c 'cat /etc/hosts' — read-only: must produce no write target.
        let targets = bash_write_targets("sh -c 'cat /etc/hosts'");
        assert!(
            targets.is_empty(),
            "sh -c 'cat /etc/hosts' must yield no write targets: {targets:?}"
        );
        // bash -c 'ls /usr/local' — read-only: must produce no write target.
        let targets = bash_write_targets("bash -c 'ls /usr/local'");
        assert!(
            targets.is_empty(),
            "bash -c 'ls /usr/local' must yield no write targets: {targets:?}"
        );
        // bash -c 'tee /tmp/out' — actual write: must still be captured.
        let targets = bash_write_targets("bash -c 'tee /tmp/out'");
        assert!(
            !targets.is_empty(),
            "bash -c 'tee /tmp/out' must yield a write target"
        );
    }

    /// Regression: 11 realistic own-tree Creator writes are ADMITTED pre-call; the same inputs
    /// are denied for ReadOnly by the program-word rule. Former heredoc-body and interpreter-literal
    /// scanners caused false denials of these; the post-hoc witness catches out-of-tree effects
    /// for ReadOnly units only (wicked-core#548).
    #[test]
    fn own_tree_creator_writes_admitted_readonly_denied_by_program_word() {
        use crate::path_policy::AllowedRoots;
        use crate::write_posture::WritePosture as P;
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base = std::env::temp_dir().join(format!("wicked-rg11-{}-{tid}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let wt = base.join("wt");
        let notes = base.join("notes");
        for d in [
            &wt.join("src"),
            &wt.join("scripts"),
            &wt.join("tests"),
            &notes,
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        let roots = AllowedRoots {
            write: vec![wt.clone()],
            read: vec![],
        };
        let notes_roots = vec![notes.clone()];
        let w = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let ctx = |cmd: &str| serde_json::json!({ "command": cmd });
        // Creator boundary: admitted when target is within write roots.
        let creator_admits =
            |cmd: &str| boundary_denial_with(&roots, &wt, None, None, &ctx(cmd), "Bash");
        // ReadOnly: denied by program-word or redirect-target rule.
        let ro_denies =
            |cmd: String| bash_write_phase_scope(false, P::ReadOnly, &cmd, &wt, None, &notes_roots);

        // --- Category 1: cat > {wt}/* heredocs with text that used to trigger false denies ---
        // Case 1: README body contains "> " and "/usr/local/bin." — false deny before removal.
        let c1 = format!(
            "cat > {} <<'EOF'\nNode > 18 requires /usr/local/bin in your PATH.\nEOF",
            w(&wt.join("README.md"))
        );
        assert!(
            creator_admits(&c1).is_none(),
            "case 1 must be admitted: {c1}"
        );
        assert!(ro_denies(c1).is_some(), "case 1 ReadOnly must be denied");

        // Case 2: Rust source body with ">>" (Vec<String>>) and "/etc/hosts".
        let c2 = format!(
            "cat > {} <<'EOF'\n/// Parses /etc/hosts entries\npub fn parse() -> Option<Vec<String>> {{ None }}\nEOF",
            w(&wt.join("src").join("lib.rs"))
        );
        assert!(
            creator_admits(&c2).is_none(),
            "case 2 must be admitted: {c2}"
        );
        assert!(ro_denies(c2).is_some(), "case 2 ReadOnly must be denied");

        // Case 3: test file body with Vec<String>> and /etc/hosts comment.
        let c3 = format!(
            "cat > {} <<'EOF'\n#[test]\nfn it() {{\n    let _v: Option<Vec<String>> = None;\n    // host = /etc/hosts\n}}\nEOF",
            w(&wt.join("tests").join("parse_test.rs"))
        );
        assert!(
            creator_admits(&c3).is_none(),
            "case 3 must be admitted: {c3}"
        );
        assert!(ro_denies(c3).is_some(), "case 3 ReadOnly must be denied");

        // --- Category 2: deploy.sh ---
        // Case 4: cat > deploy.sh.
        let c4 = format!(
            "cat > {} <<'EOF'\n#!/bin/bash\ncargo build --release\nEOF",
            w(&wt.join("scripts").join("deploy.sh"))
        );
        assert!(
            creator_admits(&c4).is_none(),
            "case 4 must be admitted: {c4}"
        );
        assert!(ro_denies(c4).is_some(), "case 4 ReadOnly must be denied");

        // Case 5: tee to in-tree deploy.sh.
        let c5 = format!("tee {}", w(&wt.join("deploy.sh")));
        assert!(
            creator_admits(&c5).is_none(),
            "case 5 must be admitted: {c5}"
        );
        assert!(ro_denies(c5).is_some(), "case 5 ReadOnly must be denied");

        // --- Category 3: python heredocs ---
        // Case 6: heredoc body has "> " (plain text, no write).
        let c6 = "python3 - <<'EOF'\nprint('Node > 18 required')\nEOF".to_string();
        assert!(
            creator_admits(&c6).is_none(),
            "case 6 must be admitted: {c6}"
        );
        assert!(ro_denies(c6).is_some(), "case 6 ReadOnly must be denied");

        // Case 7: heredoc body has /etc/hosts in a comment — no write.
        let c7 = "python3 - <<'EOF'\n# /etc/hosts format: IP hostname alias\nprint('ok')\nEOF"
            .to_string();
        assert!(
            creator_admits(&c7).is_none(),
            "case 7 must be admitted: {c7}"
        );
        assert!(ro_denies(c7).is_some(), "case 7 ReadOnly must be denied");

        // Case 8: python heredoc writing to own tree (pre-call admitted; witness catches post-call).
        let c8 = format!(
            "python3 - <<'EOF'\nwith open('{}', 'w') as f:\n    f.write('ok')\nEOF",
            w(&wt.join("out.txt"))
        );
        assert!(
            creator_admits(&c8).is_none(),
            "case 8 must be admitted: {c8}"
        );
        assert!(ro_denies(c8).is_some(), "case 8 ReadOnly must be denied");

        // --- Category 4: cat > f <<EOF variations ---
        // Case 9: config.toml body has "Node > 18 required" text.
        let c9 = format!(
            "cat > {} <<'EOF'\n# Node > 18 required\n[deps]\nEOF",
            w(&wt.join("config.toml"))
        );
        assert!(
            creator_admits(&c9).is_none(),
            "case 9 must be admitted: {c9}"
        );
        assert!(ro_denies(c9).is_some(), "case 9 ReadOnly must be denied");

        // Case 10: main.rs body contains ">>" in a Rust expression.
        let c10 = format!(
            "cat > {} <<EOF\nfn main() {{ println!(\">> starting\") }}\nEOF",
            w(&wt.join("src").join("main.rs"))
        );
        assert!(
            creator_admits(&c10).is_none(),
            "case 10 must be admitted: {c10}"
        );
        assert!(ro_denies(c10).is_some(), "case 10 ReadOnly must be denied");

        // Case 11: types.rs body has abs-path-like text in a comment.
        let c11 = format!(
            "cat > {} <<'EOF'\ntype Hosts = Vec<String>; // like /etc/hosts\nEOF",
            w(&wt.join("src").join("types.rs"))
        );
        assert!(
            creator_admits(&c11).is_none(),
            "case 11 must be admitted: {c11}"
        );
        assert!(ro_denies(c11).is_some(), "case 11 ReadOnly must be denied");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// INDEPENDENT REVIEW item 5 — `sed -i` and `rm` are write targets at the Creator boundary;
    /// `sed` without `-i` is not.
    #[test]
    fn sed_inplace_and_rm_creator_boundary_ir_item5() {
        use crate::path_policy::AllowedRoots;
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base = std::env::temp_dir().join(format!("wicked-ir5-{}-{tid}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let wt = base.join("wt");
        let sibling = base.join("sibling");
        for d in [&wt, &sibling] {
            std::fs::create_dir_all(d).unwrap();
        }
        let roots = AllowedRoots {
            write: vec![wt.clone()],
            read: vec![],
        };
        let ctx = |cmd: &str| serde_json::json!({ "command": cmd });
        let check = |cmd: &str| boundary_denial_with(&roots, &wt, None, None, &ctx(cmd), "Bash");
        let w = |p: &std::path::Path| p.to_string_lossy().into_owned();

        // sed -i on sibling file: denied.
        assert!(
            check(&format!("sed -i 's/a/b/' {}/file.rs", w(&sibling))).is_some(),
            "sed -i on sibling must be denied"
        );
        // rm on sibling file: denied.
        assert!(
            check(&format!("rm {}/file.rs", w(&sibling))).is_some(),
            "rm on sibling must be denied"
        );
        // sed without -i: no write target extracted → admitted.
        assert!(
            check(&format!("sed 's/a/b/' {}/file.rs", w(&sibling))).is_none(),
            "sed without -i must be admitted"
        );
        // sed -i on in-wt file: admitted.
        assert!(
            check(&format!("sed -i 's/a/b/' {}/src/main.rs", w(&wt))).is_none(),
            "sed -i on in-wt file must be admitted"
        );
        // rm on in-wt file: admitted.
        assert!(
            check(&format!("rm {}/tmp.rs", w(&wt))).is_none(),
            "rm on in-wt file must be admitted"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// INDEPENDENT REVIEW item 6 — `git -c k=v <write-verb>` bypass is closed at all three
    /// sites; `git -c k=v <read-verb>` is admitted.
    #[test]
    fn git_config_flag_bypass_closed_ir_item6() {
        use crate::write_posture::WritePosture as P;
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base = std::env::temp_dir().join(format!("wicked-ir6-{}-{tid}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let wt = base.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let deny = |cmd: &str| bash_write_phase_scope(false, P::ReadOnly, cmd, &wt, None, &[]);

        // Site 3 (explicit_write_program_denial): git -c k=v commit must be denied.
        assert!(
            deny("git -c user.name=x commit -m y").is_some(),
            "git -c k=v commit must be denied (site 3)"
        );
        // --config long form: also denied.
        assert!(
            deny("git --config user.name=x commit -m y").is_some(),
            "git --config k=v commit must be denied (site 3)"
        );
        // Read verb after -c: must be admitted (site 3 must not over-deny).
        assert!(
            deny("git -c user.name=x status").is_none(),
            "git -c k=v status must be admitted (site 3)"
        );
        assert!(
            deny("git -c user.name=x log --oneline").is_none(),
            "git -c k=v log must be admitted (site 3)"
        );
        // -C value must also be skipped (site 2/3): git -C /path commit denied.
        assert!(
            deny("git -C /some/outside/path commit -m y").is_some(),
            "git -C /path commit must be denied"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// INDEPENDENT REVIEW item 2 — witness skips git-ignored paths so build outputs
    /// (target/, node_modules/) do not appear as escapes.
    #[test]
    fn witness_skips_gitignored_paths_ir_item2() {
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base = std::env::temp_dir().join(format!("wicked-ir2-{}-{tid}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // Init a git repo so git ls-files works.
        let git = |args: &[&str]| {
            std::process::Command::new("git") // spawn-audit: test-only — isolated temp repo
                .args(args)
                .current_dir(&base)
                .output()
        };
        if git(&["init"]).map(|o| o.status.success()).unwrap_or(false) {
            // Write a tracked file and a .gitignore that excludes target/.
            std::fs::write(base.join(".gitignore"), "target/\n").unwrap();
            std::fs::write(base.join("src.rs"), "fn main() {}").unwrap();
            let _ = git(&["add", "."]);
            let _ = git(&[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "init",
            ]);
            // Now create target/debug/binary (git-ignored).
            let target_dir = base.join("target").join("debug");
            std::fs::create_dir_all(&target_dir).unwrap();
            std::fs::write(target_dir.join("binary"), "ELF").unwrap();

            // collect_dir_entries_for_witness must NOT include target/ contents.
            let mut entries = Vec::new();
            collect_dir_entries_for_witness(&base, &mut entries);
            let has_target = entries.iter().any(|(p, _, _)| p.contains("target"));
            assert!(
                !has_target,
                "witness must not include git-ignored target/ paths: {entries:?}"
            );
            // src.rs must be included.
            let has_src = entries.iter().any(|(p, _, _)| p.ends_with("src.rs"));
            assert!(has_src, "witness must include tracked src.rs: {entries:?}");
        } else {
            // git not available in test env: skip rather than fail.
            eprintln!("skipping ir_item2 witness test: git init failed");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Collector mismatch: a snapshot taken with `git ls-files` then re-checked when `.git` is
    /// unavailable (forcing `RawWalk`) must re-snapshot and admit — no false denial even when no
    /// file changed.
    #[test]
    fn ls_files_snapshot_then_git_unavailable_no_change_does_not_fail() {
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base = std::env::temp_dir().join(format!("wicked-coll-{}-{tid}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git") // spawn-audit: test-only — isolated temp repo
                .args(args)
                .current_dir(&base)
                .output()
        };
        if git(&["init"]).map(|o| o.status.success()).unwrap_or(false) {
            std::fs::write(base.join("tracked.rs"), "fn main() {}").unwrap();
            let _ = git(&["add", "."]);
            let _ = git(&[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "init",
            ]);

            // Take a GitLsFiles snapshot.
            let mut entries = Vec::new();
            let kind = collect_dir_entries_for_witness(&base, &mut entries);
            assert_eq!(
                kind,
                CollectorKind::GitLsFiles,
                "first snapshot uses git ls-files"
            );
            entries.sort_unstable();
            let sidecar = base.join("witness-sidecar");
            let snap = WitnessSnapshot {
                roots: vec![base.clone()],
                collector: CollectorKind::GitLsFiles,
                entries: entries.clone(),
            };
            write_write_root_witness(&sidecar, &snap);

            // Rename .git so git ls-files fails → RawWalk.
            let git_dir = base.join(".git");
            let git_bak = base.join(".git-bak");
            std::fs::rename(&git_dir, &git_bak).unwrap();

            // Re-check: collector now RawWalk, no files changed.
            let stored = read_write_root_witness(&sidecar).expect("sidecar present");
            let mut current_entries = Vec::new();
            let mut current_collector = CollectorKind::GitLsFiles;
            for root in &stored.roots {
                let k = collect_dir_entries_for_witness(root, &mut current_entries);
                if k == CollectorKind::RawWalk {
                    current_collector = CollectorKind::RawWalk;
                }
            }
            current_entries.sort_unstable();

            // Collector mismatch → must re-snapshot, NOT deny.
            assert_ne!(
                stored.collector, current_collector,
                "collector changed: git→raw"
            );
            // The re-snapshot path (not a diff) means no denial is produced.
            if stored.collector != current_collector {
                let new_snap = WitnessSnapshot {
                    roots: stored.roots.clone(),
                    collector: current_collector,
                    entries: current_entries.clone(),
                };
                write_write_root_witness(&sidecar, &new_snap);
                // Verify the updated sidecar records RawWalk.
                let reloaded = read_write_root_witness(&sidecar).expect("re-snapshotted sidecar");
                assert_eq!(
                    reloaded.collector,
                    CollectorKind::RawWalk,
                    "sidecar updated to RawWalk"
                );
            } else {
                panic!("expected collector mismatch but got none — test setup wrong");
            }

            // Restore .git for cleanup.
            let _ = std::fs::rename(&git_bak, &git_dir);
        } else {
            eprintln!("skipping collector-mismatch test: git init failed");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// item 4 — `bash_cd_escape_targets` fires when the command contains a write-CAPABLE
    /// program even if `bash_write_targets` finds no resolvable target. Interpreter-literal
    /// code-string scanning is retired; a Creator-posture unit writing outside its tree through
    /// a quoted interpreter string is neither blocked pre-call nor detected by the post-hoc
    /// witness (which runs only for ReadOnly units) — wicked-core#548.
    #[test]
    fn cd_escape_with_write_capable_program_and_abs_path_in_code_item4() {
        use crate::path_policy::AllowedRoots;
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base = std::env::temp_dir().join(format!("wicked-item4-{}-{tid}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let wt = base.join("wt");
        let sibling = base.join("sibling");
        for d in [&wt, &sibling] {
            std::fs::create_dir_all(d).unwrap();
        }
        let roots = AllowedRoots {
            write: vec![wt.clone()],
            read: vec![],
        };
        let ctx = |cmd: &str| serde_json::json!({ "command": cmd });
        let check = |cmd: &str| boundary_denial_with(&roots, &wt, None, None, &ctx(cmd), "Bash");
        let w = |p: &std::path::Path| p.to_string_lossy().into_owned();

        // cd <sibling> && git commit: write-capable program → cd destination judged.
        assert!(
            check(&format!("cd {} && git commit -m x", w(&sibling))).is_some(),
            "cd <sibling> && git commit must be denied"
        );

        // cd <sibling> && python3 -c: interpreter → cd destination judged.
        assert!(
            check(&format!(
                "cd {} && python3 -c 'open(\"x\",\"w\")'",
                w(&sibling)
            ))
            .is_some(),
            "cd <sibling> && python3 -c must be denied"
        );

        // python3 -c with absolute sibling path only in the code string (no cd, no redirect):
        // admitted pre-call — the interpreter-literal scanner is retired. This is a Creator
        // boundary (Full posture); the post-hoc witness runs only for ReadOnly units, so neither
        // pre-call nor post-hoc detection applies here — wicked-core#548.
        assert!(
            check(&format!("python3 -c \"open('{}/x','w')\"", w(&sibling))).is_none(),
            "python3 -c with sibling path in code string is admitted pre-call (Creator posture; witness does not run)"
        );

        // In-worktree cd: admitted.
        assert!(
            check(&format!("cd {} && git commit -m x", w(&wt.join("src")))).is_none(),
            "cd inside worktree && git commit must be admitted"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// items 1, 2, 3 — witness watches write roots minus notes root, runs on both carriers,
    /// names changed paths, and catches a last-call write at fold time.
    #[test]
    fn witness_watches_write_roots_minus_notes_names_paths_items1_2_3() {
        use crate::path_policy::AllowedRoots;
        use crate::write_posture::WritePosture;
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base =
            std::env::temp_dir().join(format!("wicked-item123-{}-{tid}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let wt = base.join("wt");
        let notes = base.join("notes");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&notes).unwrap();

        // --- item 1: witness_roots = write_roots minus notes ---
        let boundary = BoundaryCtx {
            roots: AllowedRoots {
                write: vec![wt.clone(), notes.clone()],
                read: vec![],
            },
            cwd: wt.clone(),
            home: None,
            claude_config_dir: None,
            pre_build_scope: false,
            write_posture: WritePosture::ReadOnly,
            deliverable_roots: vec![notes.clone()], // notes root
            estate_store_pinned: false,
        };
        let wr = compute_witness_roots(Some(&boundary));
        assert_eq!(wr, vec![wt.clone()], "witness roots = write minus notes");
        assert!(!wr.contains(&notes), "notes root excluded from witness");

        // --- item 3: changed paths named in diff ---
        let sidecar = base.join("decisions").join("witness-test-phase");
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        // Snapshot an empty wt.
        let snap1 = WitnessSnapshot {
            roots: vec![wt.clone()],
            collector: CollectorKind::RawWalk,
            entries: vec![],
        };
        write_write_root_witness(&sidecar, &snap1);
        // Create a file in wt.
        let new_file = wt.join("leaked.rs");
        std::fs::write(&new_file, "oops").unwrap();
        // Re-scan and diff.
        let stored = read_write_root_witness(&sidecar).expect("sidecar present");
        let mut current = Vec::new();
        for root in &stored.roots {
            let _ = collect_dir_entries_for_witness(root, &mut current);
        }
        current.sort_unstable();
        let changed = diff_witness_entries(&stored.entries, &current);
        assert!(!changed.is_empty(), "diff detects created file");
        assert!(
            changed.iter().any(|p| p.contains("leaked.rs")),
            "changed path names the file: {changed:?}"
        );

        // --- item 2 (fold-time unit-end catch): simulate a last-call write ---
        // Write a snapshot, then modify the wt (simulating a last-call Bash), then call
        // fold_input_denial with a governed unit that has no deny log yet.
        // We use a temp run with a real decisions file so fold can find the witness.
        let run_id = format!("witness-test-{}", std::process::id());
        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));
        let dpath = decisions_path_for(&run_id, 0);
        write_armed_marker(&dpath, "unit-witness").unwrap();
        // Write a sentinel so fold doesn't fail-closed on missing hook-fired.
        // (We skip the sentinel here to keep the test minimal — the fold closes on missing
        // sentinel only when there ARE claim lines, which there are none here.)
        // Store a snapshot with the leaked file still present.
        let witness_path = write_root_witness_path(&dpath.to_string_lossy(), "unit-witness");
        // Snapshot current state (wt has leaked.rs).
        let snap_current = WitnessSnapshot {
            roots: vec![wt.clone()],
            collector: CollectorKind::RawWalk,
            entries: current.clone(),
        };
        write_write_root_witness(&witness_path, &snap_current);
        // Now add ANOTHER file to simulate a post-snapshot mutation.
        let post_file = wt.join("post-mutation.rs");
        std::fs::write(&post_file, "post").unwrap();
        // fold_input_denial should detect the post-mutation.
        let mut store = open_store(Some(":memory:")).unwrap();
        let denial =
            fold_input_denial(&mut store, &run_id, 0, "unit-witness", true).expect("fold ok");
        assert!(
            denial.is_some(),
            "fold detects last-call write via unit-end witness check"
        );
        let reason = &denial.unwrap().reason;
        assert!(
            reason.contains("post-mutation.rs") || reason.contains("write-root mutated"),
            "fold names the changed path: {reason}"
        );

        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// additional acceptance — under ReadOnly, `sed -i`, `rm`, `truncate`, `patch`, `ln`,
    /// and `git commit|apply|checkout|stash|reset` are denied by program word.
    #[test]
    fn explicit_write_program_words_denied_under_readonly_additional() {
        use crate::write_posture::WritePosture as P;
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base = std::env::temp_dir().join(format!(
            "wicked-explicit-write-{}-{tid}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let wt = base.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let deny = |cmd: &str| bash_write_phase_scope(false, P::ReadOnly, cmd, &wt, None, &[]);

        let cases = [
            ("sed -i 's/a/b/' file.rs", "sed -i"),
            ("rm file.rs", "rm"),
            ("truncate -s 0 file.rs", "truncate"),
            ("patch -p1 < fix.patch", "patch"),
            ("ln -s src dst", "ln"),
            ("git commit -m 'x'", "git commit"),
            ("git apply fix.patch", "git apply"),
            ("git checkout main", "git checkout"),
            ("git stash", "git stash"),
            ("git reset --hard HEAD", "git reset"),
        ];
        for (cmd, label) in &cases {
            assert!(
                deny(cmd).is_some(),
                "`{label}` must be denied under ReadOnly — got None"
            );
        }
        // Controls: pure reads admitted.
        assert_eq!(
            deny("sed 's/a/b/' file.rs"),
            None,
            "sed without -i is admitted"
        );
        assert_eq!(deny("git status"), None, "git status is admitted");
        assert_eq!(deny("git log --oneline"), None, "git log is admitted");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// F-036: the READ-ONLY posture — an `executes_code: false` phase that does not play creator
    /// (an evaluator reviewing a build, a recon rung) is refused the path-bearing write tools on
    /// EVERYTHING in the tree, documentation included (no exemptions — matching the worktree
    /// guard); only the PRE-BUILD scope keeps its documentation allowance (core#296); and both are
    /// inert for a code phase (no fence, no flag). The pre-build wording wins when both apply, so
    /// that gate's established message is unchanged.
    #[test]
    fn no_code_scope_refuses_every_write_and_only_the_pre_build_scope_keeps_documentation() {
        use crate::write_posture::WritePosture as P;
        let ctx = |p: &str| serde_json::json!({ "path": p });
        let wt = std::path::Path::new("/wt");
        let no_roots: &[std::path::PathBuf] = &[];
        let deny = |pre: bool, posture: P, path: &str, tool: &str| {
            phase_scope_denial(pre, posture, &ctx(path), tool, wt, None, no_roots)
        };
        for tool in WRITE_TOOLS {
            let denied = deny(false, P::ReadOnly, "/wt/src/app.ts", tool)
                .expect("a read-only phase may not write production code");
            assert!(
                denied.contains("executes_code: false") && denied.contains("tree under review"),
                "the refusal names its own rule: {denied}"
            );
            assert!(
                !denied.contains("PRE-BUILD"),
                "a post-build evaluator is not a pre-build phase: {denied}"
            );
        }
        assert!(
            deny(false, P::ReadOnly, "/wt/docs/review.md", "Write").is_some(),
            "a read-only phase's write-up belongs in its output, not in the tree — documentation \
             is NOT exempt here (codex review on #414), matching the worktree guard"
        );
        assert!(
            deny(true, P::Full, "/wt/docs/design.md", "Write").is_none(),
            "the PRE-BUILD scope keeps its documentation allowance (core#296)"
        );
        assert!(
            deny(false, P::Full, "/wt/src/app.ts", "Write").is_none(),
            "a code phase (no fence) is free to write code"
        );
        assert!(
            deny(false, P::ReadOnly, "/wt/src/app.ts", "Read").is_none(),
            "reads are never in scope"
        );
        let both = deny(true, P::ReadOnly, "/wt/src/app.ts", "Write").unwrap();
        assert!(
            both.contains("PRE-BUILD"),
            "pre-build keeps its wording: {both}"
        );
        // Codex review on #414: NOTHING is exempt from the read-only posture — not a report, not a
        // declared deliverable (a phase that must write one INTO THE TREE declares
        // `executes_code: true`).
        assert!(
            deny(false, P::ReadOnly, "/wt/coverage-report.json", "Write").is_some(),
            "a report file in the tree is refused for a read-only phase"
        );
        assert!(
            deny(false, P::ReadOnly, "/wt/out/report.json", "Write").is_some(),
            "so is anything under an output directory inside the tree"
        );
        // An evaluator is read-only EVERYWHERE except its NOTES ROOT (F-4R2-004 + DES-L4 PR-②):
        // the roots this fn is handed under the read-only posture are `admitted_roots(ReadOnly,
        // notes_root, extras)` = the notes root alone — a declared CREATOR write root never reaches
        // it, so a declared root does not open the evaluator up. The fn itself admits exactly the
        // list it is handed (the notes-root mechanism); the guarantee that the list is never the
        // creator's extras is `admitted_roots`', asserted here from both sides.
        let inbox = std::path::PathBuf::from("/inbox");
        assert!(
            crate::write_posture::admitted_roots(
                P::ReadOnly,
                Some("/notes/unit-2"),
                &["/inbox".to_string()]
            ) == vec![std::path::PathBuf::from("/notes/unit-2")],
            "an evaluator's admitted roots are its notes root — never a declared creator root"
        );
        assert!(
            phase_scope_denial(
                false,
                P::ReadOnly,
                &ctx("/inbox/review.html"),
                "Write",
                wt,
                None,
                &crate::write_posture::admitted_roots(
                    P::ReadOnly,
                    Some("/notes/unit-2"),
                    &["/inbox".to_string()]
                ),
            )
            .is_some(),
            "an evaluator may not write into a declared creator root"
        );
        assert!(
            phase_scope_denial(
                false,
                P::ReadOnly,
                &ctx("/notes/unit-2/review.md"),
                "Write",
                wt,
                None,
                std::slice::from_ref(&std::path::PathBuf::from("/notes/unit-2")),
            )
            .is_none(),
            "…but it may write under its notes root (core#464)"
        );
        let _ = inbox;
    }

    /// F-01 (independent review of #444): the hook is the standalone `wicked-core` binary the
    /// daemon finds on PATH, and the gate protocol handshake compares crate versions — a hook built
    /// from pre-posture `main` at the SAME version parses [`NO_CODE_SCOPE_ENV`] with the strict
    /// `1`/`true` rule ([`parse_pre_build_scope`], the very parser it used). The spelling the
    /// launcher writes for the read-only posture must therefore still read as ON there, and the
    /// new posture's spelling must read as OFF (no fence, guard-only) rather than anything else.
    #[test]
    fn a_pre_posture_hook_binary_still_reads_the_read_only_spelling_as_its_no_code_scope() {
        use crate::write_posture::WritePosture as P;
        let ro = std::ffi::OsString::from(P::ReadOnly.env_value().unwrap());
        assert!(
            parse_pre_build_scope(Some(&ro)),
            "the read-only spelling the launcher arms is the one a pre-posture hook parsed as ON"
        );
        assert_eq!(
            P::parse_env(Some(&ro)),
            P::ReadOnly,
            "and this hook reads it the same way"
        );
        let dr = std::ffi::OsString::from(P::DeliverableRoots.env_value().unwrap());
        assert!(
            !parse_pre_build_scope(Some(&dr)),
            "an old hook reads the creator posture as no fence — guard-only, never a refused \
             deliverable and never an evaluator fence lost"
        );
        assert_eq!(P::parse_env(Some(&dr)), P::DeliverableRoots);
        assert_eq!(
            P::Full.env_value(),
            None,
            "no fence is spelled by ABSENCE on both"
        );
    }

    /// F-4R2-004: the DELIVERABLE-ROOTS posture — a BOUND creator whose phase declares
    /// `executes_code: false`. Its `Write`/`Edit` INSIDE the run's declared write roots passes the
    /// phase scope (the filesystem boundary already judged containment); a write into the tree
    /// under review — absolute, relative, or via a `..` hop that lands back in it — is refused
    /// with wording that names the CREATOR and the roots where the deliverable belongs, never
    /// "evaluator". Reads are never in scope.
    #[test]
    fn deliverable_roots_posture_keeps_the_creator_out_of_the_tree_and_inside_its_roots() {
        use crate::write_posture::WritePosture as P;
        let base = std::env::temp_dir().join(format!(
            "wicked-deliverable-roots-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let wt = base.join("wt");
        let inbox = base.join("inbox");
        let graph = base.join("repo-graphs").join("key");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::create_dir_all(&graph).unwrap();
        // The DELIVERABLE roots: exactly the run's extra_write_roots (F-02) — never the tree, never
        // the repo-graph key dir the filesystem boundary also admits.
        let roots = vec![inbox.clone()];
        let ctx = |p: &std::path::Path| serde_json::json!({ "path": p.to_string_lossy() });
        let deny = |path: &std::path::Path, tool: &str| {
            phase_scope_denial(
                false,
                P::DeliverableRoots,
                &ctx(path),
                tool,
                &wt,
                None,
                &roots,
            )
        };

        // The chat2 shape: the creator's deliverable in the declared inbox root — ALLOWED.
        assert_eq!(
            deny(&inbox.join("revised.html"), "Write"),
            None,
            "a creator's Write inside its declared write root passes the phase scope"
        );
        assert_eq!(
            deny(&inbox.join("sub").join("fragment-1.html"), "Edit"),
            None
        );
        // A `..` spelling that resolves into the inbox is judged by where it LANDS.
        let via_dots = wt.join("..").join("inbox").join("revised.html");
        assert_eq!(
            phase_scope_denial(
                false,
                P::DeliverableRoots,
                &serde_json::json!({ "path": via_dots.to_string_lossy() }),
                "Write",
                &wt,
                None,
                &roots
            ),
            None,
            "containment is judged on the resolved target, not the spelling"
        );

        // Into the tree under review — REFUSED, naming the creator role and the roots.
        for tool in WRITE_TOOLS {
            let denied = deny(&wt.join("src").join("app.ts"), tool)
                .expect("a creator that declared no code may not write the worktree");
            assert!(
                denied.contains("plays creator") && denied.contains("executes_code: false"),
                "the refusal names the CREATOR role and its rule: {denied}"
            );
            assert!(
                !denied.contains("evaluat"),
                "a creator is never reported as an evaluator: {denied}"
            );
            assert!(
                denied.contains(&inbox.display().to_string())
                    && !denied.contains(&format!("({})", wt.display())),
                "the refusal names where the deliverable belongs (the roots outside the tree, \
                 never the tree itself): {denied}"
            );
        }
        // F-02: the repo-graph key dir is in the filesystem boundary's write set but is NOT a
        // deliverable root — refused here exactly as the ACP fence refuses it.
        let graph_write = deny(&graph.join("graph.db-wal"), "Write")
            .expect("engine scratch the boundary admits is still not a deliverable root");
        assert!(graph_write.contains("plays creator"), "{graph_write}");
        // …and so is anything outside every root.
        assert!(deny(&base.join("elsewhere.html"), "Write").is_some());
        // A RELATIVE spelling resolves against the tree — refused too.
        assert!(
            phase_scope_denial(
                false,
                P::DeliverableRoots,
                &serde_json::json!({ "path": "src/app.ts" }),
                "Write",
                &wt,
                None,
                &roots
            )
            .is_some(),
            "a relative write lands in the worktree"
        );
        // Reads are never in scope; a write with no path is not this rule's to judge.
        assert_eq!(deny(&wt.join("src").join("app.ts"), "Read"), None);
        assert_eq!(
            phase_scope_denial(
                false,
                P::DeliverableRoots,
                &serde_json::json!({}),
                "Write",
                &wt,
                None,
                &roots
            ),
            None
        );
        // With no root declared the wording says so rather than listing nothing.
        let none: Vec<std::path::PathBuf> = vec![];
        let denied = phase_scope_denial(
            false,
            P::DeliverableRoots,
            &ctx(&inbox.join("x.html")),
            "Write",
            &wt,
            None,
            &none,
        )
        .unwrap();
        assert!(denied.contains("none declared"), "{denied}");
        let _ = std::fs::remove_dir_all(&base);
    }
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
                ..Default::default()
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
            ..Default::default()
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
            denial.as_ref().is_some_and(|d| d.reason.contains("d1")),
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
                .as_ref()
                .is_some_and(|d| d.reason.contains("fail-closed")),
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
            d.as_ref()
                .is_some_and(|s| s.reason.contains("NO decisions log")),
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
            d.as_ref()
                .is_some_and(|s| s.reason.contains("armed marker missing")),
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
                .as_ref()
                .is_some_and(|d| d.reason.contains("boundary-deny:unit-5")),
            "a boundary WRITE deny fails the unit and names the claim: {write_denial:?}"
        );
        // Usability review #1: the deny is MACHINE-READABLE — the claim id rides as a field,
        // not only inside the prose, so a UI can render a banner without parsing the sentence.
        let wd = write_denial.as_ref().unwrap();
        assert_eq!(wd.source, "input_governance");
        assert_eq!(wd.claim_id.as_deref(), Some("boundary-deny:unit-5"));
        assert_eq!(wd.phase.as_deref(), Some("unit-5"));

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

    /// core#296, the fold's side of the phase-scope gate. The tool call is already blocked by the
    /// time this runs; what is on trial here is what the block DOES to the unit. Three properties,
    /// all of which the run-level behaviour depends on:
    ///
    /// * the claim survives `conform` — it carries a synthetic `engine:` policy id that has no
    ///   Policy node behind it, and a conform failure would turn a refused write into a hard unit
    ///   error, which is the opposite of "the worker adapts";
    /// * it is DURABLE — the refusal is evidence, not a log line;
    /// * it is ADVISORY — it neither fails the unit nor vetoes the phase gate, so a design phase
    ///   that reached for a `.ts` file gets refused, is told why, and still finishes its own
    ///   deliverable. And it must not MASK a co-occurring fatal deny.
    #[test]
    fn a_phase_scope_deny_is_durable_advisory_and_masks_nothing() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let run_id = format!("phasescope-{}", std::process::id());
        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));

        // (a) marker + hook-fired + ONLY a phase-scope deny → the unit is NOT failed.
        let p0 = decisions_path_for(&run_id, 0);
        write_armed_marker(&p0, "unit-2").unwrap();
        write_hook_fired(&p0, "unit-2");
        // Written by the REAL production path, so the prefix/evaluator/policy-id wiring is what is
        // under test — not a hand-forged claim that happens to match.
        append_phase_scope_deny(
            p0.to_str().unwrap(),
            "wf/unit-2",
            "unit-2",
            "Write",
            "phase scope: this is a PRE-BUILD phase … `Write` to `src/board/attentionReason.ts`",
            None,
        );
        assert_eq!(
            fold_input_denial(&mut store, &run_id, 0, "unit-2", true).unwrap(),
            None,
            "a refused pre-build write blocks the CALL, it does not fail the UNIT"
        );
        assert_eq!(
            count_claims(&store, "phase-scope-deny:unit-2").unwrap(),
            1,
            "the refusal must be durable evidence, not just an exit code"
        );

        // (b) MIXED: a phase-scope deny alongside a real POLICY deny → still DENIED. The advisory
        // downgrade must never swallow a co-occurring fatal claim.
        let p1 = decisions_path_for(&run_id, 1);
        write_armed_marker(&p1, "unit-2").unwrap();
        append_phase_scope_deny(
            p1.to_str().unwrap(),
            "wf/unit-2",
            "unit-2",
            "Write",
            "phase scope: … `Write` to `src/lib.rs`",
            None,
        );
        let mut policy_deny = allow_claim("POL-042", "unit-2");
        policy_deny.decision = Decision::Deny;
        append_decision(&p1, &policy_deny).unwrap();
        assert!(
            fold_input_denial(&mut store, &run_id, 1, "unit-2", true)
                .unwrap()
                .is_some(),
            "a phase-scope deny does not mask a co-occurring policy deny"
        );

        // (c) The phase-gate drain: a phase whose only Deny is a phase-scope block must not veto.
        let p2 = decisions_path_for(&run_id, 2);
        append_decision(&p2, &allow_claim("drain-allow", "design")).unwrap();
        append_phase_scope_deny(
            p2.to_str().unwrap(),
            "wf/design",
            "design",
            "Edit",
            "phase scope: … `Edit` to `src/lib.rs`",
            None,
        );
        let summary = apply_hook_decisions(&mut store, "phasescope-drain", &p2).unwrap();
        assert_eq!(
            summary.denied, 0,
            "a phase-scope deny does not veto the phase gate on drain"
        );
        assert_eq!(
            summary.applied, 2,
            "both the allow and the phase-scope deny conform as durable evidence"
        );

        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));
        let _ = std::fs::remove_dir_all(gov_run_dir("phasescope-drain"));
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
            run_gate_hook("s", "unit-1", None, None, Some("postgres://h/db")),
            2
        );
        assert_eq!(run_gate_hook("s", "unit-1", None, None, None), 2);
        assert_eq!(
            run_gate_hook("s", "unit-1", None, None, Some(":memory:")),
            2
        );
        assert_eq!(
            run_output_gate_hook("s", "unit-1", None, None, Some("postgres://h/db")),
            2
        );
        assert_eq!(run_output_gate_hook("s", "unit-1", None, None, None), 2);
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
///
/// Carries BOTH versions (crew#275): the PROTOCOL version (the carrier interface — args, env,
/// exit codes) and the SEMANTIC version (the crate — what the gate actually enforces). The
/// deployment-skew incident passed the protocol check because a two-day-stale binary still spoke
/// protocol 1 while enforcing pre-core#264 boundary semantics; equal semver is what proves both
/// artifacts came from one source tree.
#[must_use]
pub fn protocol_version_line() -> String {
    format!(
        "wicked-core gate-hook protocol {GATE_PROTOCOL_VERSION} semver {}",
        env!("CARGO_PKG_VERSION")
    )
}

/// Parse the protocol number back out of a probe's stdout.
///
/// Tolerant of surrounding whitespace and trailing output, strict about the shape: anything it does
/// not recognise is `None`, which the caller must treat as skew rather than as "probably fine".
/// Takes the FIRST whitespace token after the prefix so it reads both the pre-semver line
/// (`… protocol 1`) and the current one (`… protocol 1 semver 0.4.0`).
#[must_use]
pub fn parse_protocol_version(stdout: &str) -> Option<u32> {
    stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("wicked-core gate-hook protocol "))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
}

/// Parse the SEMANTIC version out of a probe's stdout (crew#275). `None` for a binary that
/// predates the semantic handshake — the caller must treat that as skew (an old binary is
/// exactly what this detects), never as "probably fine".
#[must_use]
pub fn parse_gate_semver(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("wicked-core gate-hook protocol "))
        .and_then(|rest| {
            let mut toks = rest.split_whitespace();
            toks.next()?; // protocol number
            match (toks.next()?, toks.next()) {
                ("semver", Some(v)) => Some(v.to_string()),
                _ => None,
            }
        })
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

    // Env is process-global and Rust runs tests in threads, so these serialize on the CRATE-WIDE
    // lock (`crate::test_env`). Without it, two tests setting WICKED_WRITE_ROOTS race and the
    // failure looks like a logic bug — and a module-local lock would leave the race open against
    // every other module's env-mutating tests.
    use crate::test_env::ENV_LOCK as ENV;

    fn with_roots<T>(write: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _g = ENV.write().unwrap_or_else(|e| e.into_inner());
        match write {
            Some(w) => std::env::set_var(WRITE_ROOTS_ENV, w),
            None => std::env::remove_var(WRITE_ROOTS_ENV),
        }
        std::env::remove_var(READ_ROOTS_ENV);
        // Pin the agent-state override: an operator shell exporting CLAUDE_CONFIG_DIR would
        // MOVE the core#272 carve-out and flip the `~/.claude` expectations below.
        let cfg = std::env::var_os("CLAUDE_CONFIG_DIR");
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let out = f();
        if let Some(v) = cfg {
            std::env::set_var("CLAUDE_CONFIG_DIR", v);
        }
        std::env::remove_var(WRITE_ROOTS_ENV);
        out
    }

    fn ctx(path: &str) -> serde_json::Value {
        json!({ "path": path })
    }

    /// core#264. Run fc46a3a1 produced its deliverable correctly and was then unit-FAILED over one
    /// blocked Bash redirect to `/private/tmp/out.txt`. A blocked write into the SYSTEM temp is
    /// benign scratch: STILL blocked, STILL audited, but ADVISORY — on the Bash arm and the
    /// direct-tool arm alike. The gov-tree control proves the carve-out excludes the audit trail
    /// (a tamper attempt against the decisions logs stays fatal even though they live under temp),
    /// and the home control proves everything outside temp stays fatal.
    #[test]
    fn a_scratch_write_into_the_system_temp_is_advisory_not_fatal() {
        let wt = std::env::temp_dir().join("wicked-boundary-wt-tmp");
        std::fs::create_dir_all(&wt).unwrap();
        let roots = crate::path_policy::AllowedRoots {
            write: vec![wt.clone()],
            read: vec![],
        };
        let scratch = std::env::temp_dir().join("wicked-scratch-out.txt");

        // Bash redirect into temp (the fc46a3a1 shape) → blocked, advisory.
        let cmd_ctx = json!({ "command": format!("echo x > {}", scratch.display()) });
        let (_, fatal) = boundary_denial_with(&roots, &wt, None, None, &cmd_ctx, "Bash")
            .expect("a Bash write outside the worktree is STILL blocked");
        assert!(
            !fatal,
            "a Bash scratch write into the system temp must be ADVISORY (core#264)"
        );

        // Direct Write tool into temp → same class, same verdict.
        let (_, fatal) = boundary_denial_with(
            &roots,
            &wt,
            None,
            None,
            &ctx(scratch.to_str().unwrap()),
            "Write",
        )
        .expect("blocked");
        assert!(
            !fatal,
            "a Write-tool scratch into the system temp must be ADVISORY"
        );

        // Control 1: the governance evidence tree lives under temp and must NOT ride the
        // carve-out — a write there is a tamper attempt against the audit trail.
        let gov = std::env::temp_dir()
            .join("wicked-core-gov")
            .join("some-run/attempt-0/decisions.ndjson");
        let (_, fatal) = boundary_denial_with(
            &roots,
            &wt,
            None,
            None,
            &ctx(gov.to_str().unwrap()),
            "Write",
        )
        .expect("blocked");
        assert!(
            fatal,
            "a write into the gov evidence tree stays FATAL despite being under temp"
        );

        // Control 2: an escape anywhere else (unix: a home-shaped path) stays fatal.
        #[cfg(unix)]
        {
            let (_, fatal) = boundary_denial_with(
                &roots,
                &wt,
                None,
                None,
                &ctx("/Users/nobody/evil.txt"),
                "Write",
            )
            .expect("blocked");
            assert!(fatal, "a non-temp escape stays unit-FATAL");
        }
    }

    /// DES-GROUNDING-001 §7.1, issue #463 — the `wicked-estate` CLI allowlist. Read-only
    /// subcommands are ALLOWED (the grounding path a recon unit takes — F-RC1-046 died on a
    /// `wicked-estate stats`); the write subcommands and any unrecognised verb are DENIED
    /// fail-closed, and the hit names the segment and WHY.
    #[test]
    fn read_only_estate_subcommands_are_allowed_and_write_subcommands_denied() {
        let shared = "/srv/estate/project.db";
        for allowed in [
            format!("wicked-estate stats --db {shared}"),
            // a value-taking flag before the verb does not hide the verb
            format!("wicked-estate --db {shared} stats"),
            format!("wicked-estate query 'entity:Foo' --db {shared}"),
            format!("wicked-estate blast-radius src/lib.rs --db {shared}"),
            format!("wicked-estate rank --db {shared}"),
            format!("wicked-estate source src/main.rs --json --db {shared}"),
            format!("wicked-estate semantic 'design pattern' --db {shared}"),
            format!("wicked-estate cross-graph --db {shared}"),
            format!("wicked-estate subscribe --db {shared}"),
            // clusters WITHOUT --annotate is read-only
            format!("wicked-estate clusters --json --db {shared}"),
            "wicked-estate.exe stats".to_string(),
            "/usr/local/bin/wicked-estate rank".to_string(),
        ] {
            assert!(
                classify_estate_command(&allowed, false).is_none(),
                "read-only estate command must be ALLOWED: {allowed}"
            );
        }
        for (denied, why) in [
            (
                format!("wicked-estate index . --db {shared}"),
                ESTATE_WHY_WRITE_VERB,
            ),
            (
                format!("wicked-estate scip --db {shared}"),
                ESTATE_WHY_WRITE_VERB,
            ),
            (
                format!("wicked-estate tfstate --db {shared}"),
                ESTATE_WHY_WRITE_VERB,
            ),
            (
                format!("wicked-estate import-telemetry --db {shared}"),
                ESTATE_WHY_WRITE_VERB,
            ),
            (
                format!("wicked-estate compact --db {shared}"),
                ESTATE_WHY_WRITE_VERB,
            ),
            (
                format!("wicked-estate watch --db {shared}"),
                ESTATE_WHY_WRITE_VERB,
            ),
            // clusters WITH --annotate is a write
            (
                format!("wicked-estate clusters --annotate --db {shared}"),
                ESTATE_WHY_WRITE_VERB,
            ),
            // an unrecognised verb — fail-closed
            (
                format!("wicked-estate new-writer-verb --db {shared}"),
                ESTATE_WHY_UNKNOWN_VERB,
            ),
            ("wicked-estate".to_string(), ESTATE_WHY_UNKNOWN_VERB),
        ] {
            let hit = classify_estate_command(&denied, false)
                .unwrap_or_else(|| panic!("estate command must be DENIED: {denied}"));
            assert_eq!(hit.why, why, "{denied}");
            assert_eq!(hit.segment, denied, "the hit names the offending segment");
        }
    }

    /// DES-GROUNDING-001 §7.1 — the estate stdio MCP and garden's shim are ALLOWED only with BOTH
    /// `--readonly` AND a pinned store. `--readonly` alone (the pipeline-produced shape) is not
    /// enough; a pin without `--readonly` is not either. The pin is any of: `--db <path>` /
    /// `--db=<path>` on argv, a leading `WICKED_ESTATE_DB=` / `WICKED_HOME=` / `WICKED_MEMORY_DB=`
    /// assignment (bare or through `env`), or the worker-env fact the carrier hands in.
    #[test]
    fn the_estate_shim_and_mcp_are_allowed_only_read_only_with_a_pinned_store() {
        let shared = "/srv/estate/project.db";
        for allowed in [
            format!("wicked-estate-mcp --readonly --db {shared}"),
            format!("wicked-estate-mcp.exe --db={shared} --readonly"),
            format!("python _estate_client.py --readonly --db {shared} recall '{{}}'"),
            format!("python3 scripts/_estate_client.py --readonly --db {shared} search 'foo'"),
            "WICKED_HOME=/srv/estate python _estate_client.py --readonly recall '{}'".to_string(),
            "WICKED_MEMORY_DB=/srv/estate/memory.db wicked-estate-mcp --readonly".to_string(),
            "env WICKED_ESTATE_DB=/srv/estate/graph.db wicked-estate-mcp --readonly".to_string(),
        ] {
            assert!(
                classify_estate_command(&allowed, false).is_none(),
                "read-only + pinned shim/MCP must be ALLOWED: {allowed}"
            );
        }
        // `--readonly` with no pin on argv: DENIED strictly, ALLOWED once the worker env pins it.
        for unpinned in [
            "wicked-estate-mcp --readonly",
            "python _estate_client.py --readonly recall '{}'",
        ] {
            let hit = classify_estate_command(unpinned, false)
                .unwrap_or_else(|| panic!("an unpinned shim must be DENIED: {unpinned}"));
            assert_eq!(hit.why, ESTATE_WHY_NO_PIN, "{unpinned}");
            assert!(
                classify_estate_command(unpinned, true).is_none(),
                "the worker-env pin satisfies the rule: {unpinned}"
            );
        }
        // An empty assignment pins nothing.
        assert_eq!(
            classify_estate_command("WICKED_HOME= wicked-estate-mcp --readonly", false)
                .map(|h| h.why),
            Some(ESTATE_WHY_NO_PIN)
        );
        // No `--readonly`: DENIED whatever the pin says (the hole the old deny-all missed for the
        // shim: its program word is `python`, invisible to a binary-name scan).
        for rw in [
            format!("wicked-estate-mcp --db {shared}"),
            "wicked-estate-mcp.exe --db x".to_string(),
            format!("python _estate_client.py --db {shared} recall '{{}}'"),
            format!("python3 _estate_client.py --db {shared} search 'foo'"),
        ] {
            for env_pinned in [false, true] {
                let hit = classify_estate_command(&rw, env_pinned)
                    .unwrap_or_else(|| panic!("shim/MCP without --readonly must be DENIED: {rw}"));
                assert_eq!(hit.why, ESTATE_WHY_NO_READONLY, "{rw}");
            }
        }
    }

    /// DES-GROUNDING-001 §7.3 — the backends garden's skills ACTUALLY run: the program word is a
    /// LAUNCHER (`sh …/_python.sh`, `python3`, `py -3`, `python -m`) and the shim / backend is the
    /// script in executing position. Recognised by script path/name under the same `--readonly` +
    /// pin rule; a mere MENTION of the script (grep / cat / an argument to another script) is not
    /// an invocation.
    #[test]
    fn garden_backends_that_spawn_the_shim_are_classified_by_script_path() {
        // the exact shape `skills/mem/SKILL.md` documents
        let mem = "sh \"${CLAUDE_PLUGIN_ROOT}/scripts/_python.sh\" \
                   \"${CLAUDE_PLUGIN_ROOT}/scripts/mem/estate_memory.py\"";
        for backend in [
            format!("{mem} recall '{{\"query\":\"x\"}}'"),
            "python3 scripts/mem/estate_memory.py recall '{}'".to_string(),
            "py -3 scripts\\mem\\auto_memorize.py".to_string(),
            "./scripts/_estate_client.py health".to_string(),
            "\"${CLAUDE_PLUGIN_ROOT}/scripts/_python.sh\" scripts/_context_backend.py stats"
                .to_string(),
            "python -m mem.estate_memory recall '{}'".to_string(),
            "python3 -u \"${CLAUDE_PLUGIN_ROOT}/scripts/_run.py\" \
             scripts/mem/session_fact_extractor.py"
                .to_string(),
        ] {
            let hit = classify_estate_command(&backend, true).unwrap_or_else(|| {
                panic!("a backend that spawns the shim must be classified: {backend}")
            });
            assert_eq!(hit.why, ESTATE_WHY_NO_READONLY, "{backend}");
            let ro = format!("{backend} --readonly");
            assert_eq!(
                classify_estate_command(&ro, false).map(|h| h.why),
                Some(ESTATE_WHY_NO_PIN),
                "{ro}"
            );
            assert!(
                classify_estate_command(&ro, true).is_none(),
                "read-only + worker-env pin: {ro}"
            );
            assert!(
                classify_estate_command(&format!("{ro} --db /srv/estate/graph.db"), false)
                    .is_none(),
                "read-only + argv pin: {ro}"
            );
        }
        for benign in [
            "grep readonly scripts/_estate_client.py",
            "cat scripts/mem/estate_memory.py",
            "ls scripts/mem/",
            // the shim is an ARGUMENT of another script, not the script being run
            "python3 scripts/other/tool.py scripts/mem/estate_memory.py",
            "sed -n '1,10p' \"${CLAUDE_PLUGIN_ROOT}/scripts/_context_backend.py\"",
        ] {
            assert!(
                classify_estate_command(benign, false).is_none(),
                "a mention of the shim must not trip the fence: {benign}"
            );
        }
    }

    /// #463 §7.3 / #474: the `wicked-garden run|python <script>` launcher is recognised as the
    /// shim's executing position (directly, and through `node <…/wicked-garden.mjs>` / `npx`), and
    /// the store-pin rule is enforced on it; the #474 spellings (`//`, leading `./`, a `mem`
    /// backend by basename after a `cd scripts`) all classify as the shim.
    #[test]
    fn the_wicked_garden_launcher_and_474_spellings_are_recognised() {
        let shared = "/srv/estate/project.db";
        // Recognised AND missing --readonly → deny; with --readonly + a pin → allow.
        for run in [
            "wicked-garden run scripts/_estate_client.py call '{}'",
            "wicked-garden python scripts/_estate_client.py call '{}'",
            "npx wicked-garden run scripts/_estate_client.py call '{}'",
            "node /opt/g/scripts/wicked-garden.mjs run scripts/_estate_client.py call '{}'",
            // #474 spellings, still through the launcher
            "wicked-garden run ./scripts//mem/estate_memory.py store '{}'",
            "wicked-garden run mem/estate_memory.py store '{}'",
        ] {
            assert_eq!(
                classify_estate_command(run, false).map(|h| h.why),
                Some(ESTATE_WHY_NO_READONLY),
                "a garden-launched shim without --readonly must be denied: {run}"
            );
            let ok = format!("{run} --readonly --db {shared}");
            assert!(
                classify_estate_command(&ok, false).is_none(),
                "a garden-launched shim, read-only and pinned, is allowed: {ok}"
            );
        }
        // The launcher WITHOUT a run/python verb is not a shim invocation (e.g. `wicked-garden --help`).
        assert!(
            classify_estate_command("wicked-garden --help", false).is_none(),
            "a garden launcher with no run/python verb is not a shim call"
        );
        // #474 direct spellings: a mem backend run by basename after a cd, and a `//`/`./` path.
        for direct in [
            "python3 mem/estate_memory.py store '{}'",
            "python3 ./scripts//_estate_client.py call '{}'",
        ] {
            assert_eq!(
                classify_estate_command(direct, false).map(|h| h.why),
                Some(ESTATE_WHY_NO_READONLY),
                "a #474-spelled shim without --readonly must be denied: {direct}"
            );
        }
    }

    /// Evasion shapes and non-leading segments: env-assignment prefixes, the `env` wrapper, a
    /// prefix redirect, `;` sequences and pipelines do not hide an estate write (Copilot #385);
    /// the ordinary read-only commands around them are untouched.
    #[test]
    fn estate_evasion_shapes_and_later_segments_are_still_denied() {
        let shared = "/srv/estate/project.db";
        for evade in [
            format!("/usr/local/bin/wicked-estate index . --db {shared}"),
            format!("WICKED_X=1 wicked-estate index . --db {shared}"),
            "A=b C=d wicked-estate index .".to_string(),
            "env wicked-estate index .".to_string(),
            "env X=1 wicked-estate index .".to_string(),
            "env -i wicked-estate index .".to_string(),
            format!("> /dev/null wicked-estate index . --db {shared}"),
            format!("cat notes.txt; wicked-estate index . --db {shared}"),
            format!("echo x | wicked-estate index . --db {shared}"),
            "ls && python3 scripts/mem/estate_memory.py store '{}'".to_string(),
        ] {
            assert!(
                classify_estate_command(&evade, true).is_some(),
                "evasion attempt must still be denied: {evade}"
            );
        }
        for benign in [
            "ls",
            "cat file.txt",
            "grep -r wicked-estate .",
            "env ls",
            "env X=1 ls",
            "grep wicked-estate-mcp /etc/hosts",
            "echo wicked-estate index",
        ] {
            assert!(
                classify_estate_command(benign, false).is_none(),
                "benign command must not trip the estate fence: {benign}"
            );
        }
    }

    /// core #475: ONE level of the fixed wrapper table is seen through by BOTH tokenizer consumers.
    /// Every table row × a write target: the target behind `sh -c` / `bash -lc` / `sh -ec` /
    /// `zsh -xc` / `dash -c`, `exec`, `xargs [flags]`, `env [flags] [X=1]`, `nice [-n N]`,
    /// `timeout [flags] <dur>` or a QUOTED program word is found — before this every one of them
    /// matched nothing (`"cp"` is not `cp`, `sh -lc '…'` hid its redirect inside the quotes).
    /// Mutation: delete a row of `unwrap_program` → that row's cases fail.
    #[test]
    fn one_wrapper_level_is_unwrapped_for_write_targets() {
        for (cmd, target) in [
            ("sh -c 'echo x > src/y'", "src/y"),
            ("bash -lc 'echo x > src/y'", "src/y"),
            ("sh -ec \"echo x > src/y\"", "src/y"),
            ("zsh -xc 'cat a | tee src/y'", "src/y"),
            ("dash -c 'cp a src/y'", "src/y"),
            ("/bin/bash -c 'dd if=a of=src/y'", "src/y"),
            ("exec tee src/y", "src/y"),
            ("xargs tee src/y", "src/y"),
            ("xargs -0 -n1 tee src/y", "src/y"),
            ("env tee src/y", "src/y"),
            ("env -i X=1 tee src/y", "src/y"),
            ("nice tee src/y", "src/y"),
            ("nice -n 10 cp a src/y", "src/y"),
            ("timeout 30 tee src/y", "src/y"),
            ("timeout -s KILL 5s cp a src/y", "src/y"),
            ("\"cp\" a src/y", "src/y"),
            ("'/usr/bin/tee' src/y", "src/y"),
            // a wrapper in a LATER segment, and an outer redirect around a `-c` string
            ("echo x | exec tee src/y", "src/y"),
            ("sh -c 'echo x' > src/y", "src/y"),
        ] {
            let targets = bash_write_targets(cmd);
            assert!(
                targets.iter().any(|t| t == target),
                "{cmd}: expected write target {target}, got {targets:?}"
            );
        }
        // The documented limits, unchanged: a SECOND `-c` level is not unwrapped, an inline
        // interpreter is not modelled, and a shell running a SCRIPT (no `-c`) is a launcher, not a
        // wrapper — none of these may invent a target.
        for pass in [
            "sh -c 'sh -c \"echo x > src/y\"'",
            "python3 -c 'open(\"src/y\", \"w\")'",
            "sh run.sh src/y",
        ] {
            assert!(
                bash_write_targets(pass).is_empty(),
                "documented literal-scan pass must yield no target: {pass}"
            );
        }
    }

    /// core #475, the estate half: the same wrapper table hides no estate WRITE from the fence, an
    /// allowed shim call stays allowed through a wrapper (its flags ride the inner argv), and the
    /// nested `-c` remains the documented pass.
    #[test]
    fn one_wrapper_level_is_unwrapped_for_the_estate_fence() {
        for evade in [
            "sh -c 'wicked-estate index .'",
            "bash -lc 'wicked-estate index .'",
            "exec wicked-estate index .",
            "xargs -n1 wicked-estate index .",
            "nice -n 5 wicked-estate index .",
            "timeout 60 wicked-estate index .",
            "\"wicked-estate\" index .",
            "'/usr/local/bin/wicked-estate' index .",
            // the shim without `--readonly`, behind a wrapper
            "sh -c 'python3 scripts/_estate_client.py call x'",
            "timeout 30 python3 scripts/mem/estate_memory.py store '{}'",
            "exec wicked-estate-mcp --db /srv/g.db",
        ] {
            assert!(
                classify_estate_command(evade, true).is_some(),
                "a wrapped estate write must still be denied: {evade}"
            );
        }
        for allowed in [
            "timeout 30 python3 scripts/_estate_client.py --readonly call x",
            "sh -c 'python3 scripts/_estate_client.py --readonly call x'",
            "exec wicked-estate stats",
            "nice -n 5 wicked-estate query 'x'",
        ] {
            assert!(
                classify_estate_command(allowed, true).is_none(),
                "an allowed estate read stays allowed through a wrapper: {allowed}"
            );
        }
        // The inner segment carries its own pin.
        assert!(classify_estate_command(
            "sh -c 'python3 scripts/_estate_client.py --readonly --db /srv/g.db call x'",
            false
        )
        .is_none());
        // A `-c` string INSIDE a `-c` string is the documented pass.
        assert!(
            classify_estate_command("sh -c 'sh -c \"wicked-estate index .\"'", true).is_none(),
            "a second wrapper level is the documented literal-scan limit"
        );
    }

    /// The store pin an ACP child can see is exactly the pin set minus what `hardened()` strips:
    /// `WICKED_ESTATE_DB` never reaches an ACP agent, `WICKED_HOME` / `WICKED_MEMORY_DB` do.
    #[test]
    fn the_acp_child_keeps_only_the_pins_hardening_does_not_strip() {
        let surviving: Vec<&str> = ESTATE_STORE_PIN_ENV
            .iter()
            .copied()
            .filter(|k| !wicked_apps_core::spawn::ENGINE_INTERNAL_ENV.contains(k))
            .collect();
        assert_eq!(surviving, ["WICKED_HOME", "WICKED_MEMORY_DB"]);
        assert!(wicked_apps_core::spawn::ENGINE_INTERNAL_ENV.contains(&ESTATE_DB_ENV));
    }

    /// F3 (DES-L4 PR-②): the three fence appenders that used to call `append_decision` bare —
    /// phase-scope, infra, remote-write — now write the tool-call annotation IN THE SAME BUFFER, so
    /// a replayed record names the tool (never `(unknown)`), and a phase-scope record surfaces
    /// through `phase_scope_refusal()` with the reason AND the offending Bash command. Mutation:
    /// route any of the three back through bare `append_decision` → its `tool_name` reads
    /// `(unknown)` here.
    #[test]
    fn phase_scope_infra_and_remote_write_records_name_the_tool_never_unknown() {
        let run_id = format!("fence-records-{}", std::process::id());
        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));
        let p = decisions_path_for(&run_id, 0);
        write_armed_marker_for(&p, "unit-3", Some(CARRIER_WRAPPED_CLI)).unwrap();
        let sentinel = serde_json::json!({ HOOK_FIRED_KEY: "unit-3" }).to_string() + "\n";
        {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .unwrap();
            f.write_all(sentinel.as_bytes()).unwrap();
        }
        let heredoc = "cat > notes.md <<'EOF'\nfindings\nEOF";
        append_phase_scope_deny(
            p.to_str().unwrap(),
            "wf/unit-3",
            "unit-3",
            "Bash",
            "phase scope: `Bash` would write `notes.md` — …",
            Some(heredoc),
        );
        append_phase_scope_deny(
            p.to_str().unwrap(),
            "wf/unit-3",
            "unit-3",
            "Write",
            "phase scope: … `Write` to `x.md`",
            None,
        );
        append_infra_deny(
            p.to_str().unwrap(),
            "wf/unit-3",
            "unit-3",
            "Edit",
            "store open failed",
        );
        append_remote_write_deny(
            p.to_str().unwrap(),
            "wf/unit-3",
            "unit-3",
            "remote-write fence: …",
            "git push origin main",
        );
        let recs = collect_hook_decisions(&run_id, 0, "unit-3");
        assert_eq!(recs.len(), 4, "{recs:?}");
        let tools: Vec<&str> = recs.iter().map(|r| r.tool_name.as_str()).collect();
        assert_eq!(tools, vec!["Bash", "Write", "Edit", "Bash"], "{recs:?}");
        assert!(
            recs.iter()
                .all(|r| r.tool_name != "(unknown)" && r.decision == "deny"),
            "{recs:?}"
        );
        assert!(
            recs.iter()
                .all(|r| r.carrier.as_deref() == Some(CARRIER_WRAPPED_CLI)),
            "the armed marker's carrier rides every record: {recs:?}"
        );
        // The phase-scope records surface with reason + command (empty for a path-bearing tool).
        assert_eq!(
            recs[0].phase_scope_refusal(),
            Some((
                "phase scope: `Bash` would write `notes.md` — …".to_string(),
                heredoc.to_string()
            ))
        );
        assert_eq!(
            recs[1].phase_scope_refusal(),
            Some((
                "phase scope: … `Write` to `x.md`".to_string(),
                String::new()
            ))
        );
        assert_eq!(
            recs[2].phase_scope_refusal(),
            None,
            "an infra deny is not a phase-scope one"
        );
        assert_eq!(
            recs[3].remote_write_refusal(),
            Some((
                "remote-write fence: …".to_string(),
                "git push origin main".to_string()
            ))
        );
        assert_eq!(recs[3].phase_scope_refusal(), None);
        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));
    }

    /// Issue #463 acceptance: an estate DENY is a real decision record naming the TOOL and the
    /// COMMAND on BOTH arms — never `(unknown)`. The advisory arm (a recon / pre-build posture)
    /// records `estate-deny:` — advisory by the allowlist, so the unit is NOT denied, and
    /// disclosed as `workerToolCallDenied` through `estate_refusal()`; the fatal arm (a
    /// code-executing unit) records the same `boundary-deny:` class the fence always emitted, so
    /// the fold denies the unit with the tool named — the payload a gate over the denial
    /// (issue #463 item 3 / core#464) reads. The carrier rides the armed marker onto every record.
    #[test]
    fn an_estate_deny_record_names_the_tool_and_the_command_on_both_arms() {
        let wt = std::env::temp_dir().join("wicked-boundary-wt-estate-record");
        std::fs::create_dir_all(&wt).unwrap();
        let roots = crate::path_policy::AllowedRoots {
            write: vec![wt.clone()],
            read: vec![],
        };
        let command = "wicked-estate index . --db /srv/estate/project.db";
        // The fence names the segment, why, and the remedy, under the prefix
        // `evaluate_tool_call` routes on.
        let (reason, _) = boundary_denial_tracked(
            &roots,
            &wt,
            None,
            None,
            &json!({ "command": command }),
            "Bash",
            None,
            false,
        )
        .expect("an estate write is denied");
        assert!(reason.starts_with(ESTATE_DENY_REASON_PREFIX), "{reason}");
        assert!(
            reason.contains(command)
                && reason.contains(ESTATE_WHY_WRITE_VERB)
                && reason.contains(ESTATE_DENY_REMEDY),
            "{reason}"
        );

        let run_id = format!("estate-deny-{}", std::process::id());
        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));
        let sentinel = |p: &Path| {
            let line = serde_json::json!({ HOOK_FIRED_KEY: "unit-1" }).to_string() + "\n";
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .unwrap();
            f.write_all(line.as_bytes()).unwrap();
        };
        let mut store = wicked_apps_core::open_store(Some(":memory:")).unwrap();

        // ── advisory arm (recon posture), attempt 0, armed by the ACP carrier ──
        let p0 = decisions_path_for(&run_id, 0);
        write_armed_marker_for(&p0, "unit-1", Some(CARRIER_ACP)).unwrap();
        sentinel(&p0);
        append_estate_deny(
            p0.to_str().unwrap(),
            "wf/unit-1",
            "unit-1",
            "Bash",
            &reason,
            command,
            false,
        );
        let recs = collect_hook_decisions(&run_id, 0, "unit-1");
        assert_eq!(recs.len(), 1);
        assert_eq!(
            recs[0].tool_name, "Bash",
            "the record names the tool, never (unknown)"
        );
        assert_eq!(recs[0].decision, "deny");
        assert_eq!(
            recs[0].carrier.as_deref(),
            Some(CARRIER_ACP),
            "the carrier rides the marker"
        );
        assert!(
            recs[0].claim_id.starts_with(ESTATE_DENY_PREFIX),
            "{}",
            recs[0].claim_id
        );
        let (rec_reason, rec_command) = recs[0]
            .estate_refusal()
            .expect("the advisory arm is disclosed as a refusal");
        assert_eq!(rec_command, command, "obligations[1] names the command");
        assert_eq!(rec_reason, reason);
        assert_eq!(
            fold_input_denial(&mut store, &run_id, 0, "unit-1", true).unwrap(),
            None,
            "advisory: the unit is not denied for a blocked estate write"
        );

        // ── fatal arm (code-executing unit), attempt 1, armed by the wrapped carrier ──
        let p1 = decisions_path_for(&run_id, 1);
        write_armed_marker_for(&p1, "unit-1", Some(CARRIER_WRAPPED_CLI)).unwrap();
        sentinel(&p1);
        append_estate_deny(
            p1.to_str().unwrap(),
            "wf/unit-1",
            "unit-1",
            "Bash",
            &reason,
            command,
            true,
        );
        let recs = collect_hook_decisions(&run_id, 1, "unit-1");
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].tool_name, "Bash");
        assert_eq!(recs[0].carrier.as_deref(), Some(CARRIER_WRAPPED_CLI));
        assert!(
            recs[0].claim_id.starts_with(BOUNDARY_WRITE_DENY_PREFIX),
            "the fatal arm is the fence's own class: {}",
            recs[0].claim_id
        );
        assert!(
            recs[0].estate_refusal().is_none(),
            "the fatal arm is a unit denial, not an advisory refusal"
        );
        assert_eq!(
            recs[0].obligations.get(1).map(String::as_str),
            Some(command),
            "the command rides obligations[1] on the fatal arm too"
        );
        let denial = fold_input_denial(&mut store, &run_id, 1, "unit-1", true)
            .unwrap()
            .expect("fatal: the unit is denied");
        assert_eq!(denial.denied_tool.as_deref(), Some("Bash"));
        assert_eq!(denial.claim_id.as_deref(), Some("boundary-deny:unit-1"));

        // A marker an older launcher wrote (no carrier) leaves the record unattributed, not wrong.
        let p2 = decisions_path_for(&run_id, 2);
        write_armed_marker(&p2, "unit-1").unwrap();
        sentinel(&p2);
        append_estate_deny(
            p2.to_str().unwrap(),
            "wf/unit-1",
            "unit-1",
            "Bash",
            &reason,
            command,
            false,
        );
        assert_eq!(
            collect_hook_decisions(&run_id, 2, "unit-1")[0].carrier,
            None
        );

        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));
        let _ = std::fs::remove_dir_all(&wt);
    }

    /// core#294 — a LAUNCH-DECLARED read root ("ground this run in X without letting it touch X"),
    /// judged exactly as the evidence-derived read roots are. Reads inside it pass; a WRITE into it
    /// stays a fatal escape — a read root never widens write scope; reads outside every root stay
    /// advisory-denied (core#219); and with no declared read roots the boundary is byte-identical
    /// to the pre-#294 one.
    #[test]
    fn a_launch_declared_read_root_grants_reads_and_nothing_else() {
        let wt = std::env::temp_dir().join("wicked-boundary-wt-rr");
        std::fs::create_dir_all(&wt).unwrap();
        // Deliberately NOT under temp_dir (a write-deny there is advisory via the core#264
        // carve-out, which would make the fatality assertions vacuous), and drive-prefixed on
        // Windows (a bare `/srv` has no drive there and is not absolute).
        #[cfg(unix)]
        let grounding = std::path::PathBuf::from("/srv/grounding-repo");
        #[cfg(windows)]
        let grounding = std::path::PathBuf::from("C:\\srv\\grounding-repo");
        let roots = crate::path_policy::AllowedRoots {
            write: vec![wt.clone()],
            read: vec![grounding.clone()],
        };
        let src = grounding.join("src").join("lib.rs");
        let src = src.to_str().unwrap();

        // Read inside the declared root → no denial at all: the widening this issue exists for.
        assert!(
            boundary_denial_with(&roots, &wt, None, None, &ctx(src), "Read").is_none(),
            "a read inside a launch-declared read root must pass the boundary"
        );

        // WRITE into the read root → still blocked, and unit-FATAL: write containment is
        // unchanged — the read grant must never leak into the write list.
        let (reason, fatal) =
            boundary_denial_with(&roots, &wt, None, None, &ctx(src), "Write").expect("blocked");
        assert!(
            fatal,
            "a write into a read-only root is an escape attempt, not a widened grant: {reason}"
        );

        // The Bash arm agrees: a shell redirect into the read root is the same fatal class.
        let cmd = json!({ "command": format!("echo x > {src}") });
        let (_, fatal) =
            boundary_denial_with(&roots, &wt, None, None, &cmd, "Bash").expect("blocked");
        assert!(fatal, "a Bash write into a read-only root stays unit-fatal");

        // Read OUTSIDE every root → still blocked, still advisory (blocked-but-not-fatal).
        #[cfg(unix)]
        let outside = "/srv/other-repo/secret.txt";
        #[cfg(windows)]
        let outside = "C:\\srv\\other-repo\\secret.txt";
        let (_, fatal) = boundary_denial_with(&roots, &wt, None, None, &ctx(outside), "Read")
            .expect("a read outside every root is still blocked");
        assert!(
            !fatal,
            "a blocked read outside every root stays ADVISORY (core#219)"
        );

        // No declared read roots ⇒ today's behavior: the same read is advisory-denied.
        let bare = crate::path_policy::AllowedRoots {
            write: vec![wt.clone()],
            read: vec![],
        };
        let (_, fatal) = boundary_denial_with(&bare, &wt, None, None, &ctx(src), "Read")
            .expect("an undeclared root grants nothing");
        assert!(!fatal, "…and the deny stays advisory, exactly as before");
    }

    /// core#294, the ADVERSARIAL topology: the declared read root CONTAINS the write root. This is
    #[test]
    fn the_install_fence_tracks_the_shell_cwd_across_hook_calls_and_a_cd_back_re_allows() {
        // Review of #456, F1 — the wrapped carrier: `cd <clone>` (call 1, allowed) then `npm ci`
        // (call 2) must be refused from the persisted cwd, not from the process cwd.
        let repo =
            std::env::temp_dir().join(format!("wicked-hook-install-fence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(repo.join("wicked-worktrees/run-1/sub")).unwrap();
        let repo = std::fs::canonicalize(&repo).unwrap();
        let wt = repo.join("wicked-worktrees").join("run-1");
        let roots = crate::path_policy::AllowedRoots {
            write: vec![wt.clone()],
            read: vec![],
        };
        let state =
            install_fence_cwd_path(&decisions_path_for("run-1", 0).to_string_lossy(), "unit-3");
        let _ = std::fs::remove_file(&state);
        let ctx = |cmd: &str| serde_json::json!({ "command": cmd });
        let judge = |cmd: &str| {
            boundary_denial_tracked(
                &roots,
                &wt,
                None,
                None,
                &ctx(cmd),
                "Bash",
                Some(&state),
                false,
            )
        };
        // Fresh attempt: judged from the worktree.
        assert_eq!(judge("npm ci"), None);
        // Call 1 allowed: `cd <clone>` — nothing to refuse, the shell moved; persist it as the
        // allow exit does.
        let cd_clone = format!("cd {}", repo.display());
        assert_eq!(judge(&cd_clone), None);
        track_install_fence_cwd(&cd_clone, &wt, None, &state);
        assert_eq!(
            std::fs::read_to_string(&state).unwrap().trim(),
            repo.to_string_lossy()
        );
        // A benign intermediate call keeps the tracking.
        assert_eq!(judge("git status"), None);
        track_install_fence_cwd("git status", &wt, None, &state);
        // Call 2: refused, advisory, naming the clone root and the install-fence remedy.
        let (reason, fatal) = judge("npm ci").expect("the two-call split is refused");
        assert!(!fatal, "advisory: the call is blocked, the unit continues");
        assert!(
            reason.starts_with(crate::install_fence::REASON_PREFIX),
            "{reason}"
        );
        assert!(reason.contains(&*repo.to_string_lossy()), "{reason}");
        // A refusal never moves the shell: the sidecar still says the clone root.
        assert_eq!(
            std::fs::read_to_string(&state).unwrap().trim(),
            repo.to_string_lossy()
        );
        // A `cd` back into the worktree re-allows — and the sidecar is dropped (= the worktree).
        let cd_back = format!("cd {}", wt.display());
        assert_eq!(judge(&cd_back), None);
        track_install_fence_cwd(&cd_back, &wt, None, &state);
        assert!(!state.exists(), "back at the worktree ⇒ no sidecar");
        assert_eq!(judge("npm ci"), None);
        // A new attempt has its own sidecar path (attempt-scoped directory).
        assert_ne!(
            install_fence_cwd_path(&decisions_path_for("run-1", 1).to_string_lossy(), "unit-3"),
            state
        );
    }

    /// not exotic — it is the PRIMARY use: worktrees live at `<repo>/wicked-worktrees/<run>`
    /// (see [`crate::repo`]), so `extraReadRoots: [repoRoot]` on a run bound to that same repo
    /// puts the unit's own worktree INSIDE the read grant. If the read list leaked into the write
    /// judgement — or the wider root won by prefix-length, order, or any merge — the grant
    /// "read the repo" would silently become "write the repo", the exact weakening core#294's
    /// fail-closed claim rules out. [`crate::path_policy::check`] tests a write against
    /// `roots.write` ALONE, so containment between the lists must be irrelevant; this pins that.
    #[test]
    fn a_read_root_containing_the_write_root_never_admits_a_write() {
        // Nothing here is created on disk, and none of it is under the system temp — a write-deny
        // under temp is ADVISORY via the core#264 carve-out, which would turn every fatality
        // assertion below vacuous. Drive-prefixed on Windows (a bare `/srv` is not absolute there).
        #[cfg(unix)]
        let repo = std::path::PathBuf::from("/srv/monorepo");
        #[cfg(windows)]
        let repo = std::path::PathBuf::from("C:\\srv\\monorepo");
        let wt = repo.join("wicked-worktrees").join("run-1");
        let roots = crate::path_policy::AllowedRoots {
            write: vec![wt.clone()],
            read: vec![repo.clone()],
        };

        // Inside the worktree: both lists admit it, and the write grant is the NARROW one.
        let in_wt = wt.join("src").join("main.rs");
        assert!(
            boundary_denial_with(
                &roots,
                &wt,
                None,
                None,
                &ctx(in_wt.to_str().unwrap()),
                "Write"
            )
            .is_none(),
            "a write inside the worktree stays granted — the read widening must not disturb it"
        );

        // Inside the repo but OUTSIDE the worktree — the parent checkout the read root grounds
        // the run in. Readable by declaration; a WRITE stays blocked and unit-FATAL.
        let sibling = repo.join("src").join("main.rs");
        let sibling = sibling.to_str().unwrap();
        assert!(
            boundary_denial_with(&roots, &wt, None, None, &ctx(sibling), "Read").is_none(),
            "reading the parent checkout is the declared grant"
        );
        let (reason, fatal) = boundary_denial_with(&roots, &wt, None, None, &ctx(sibling), "Write")
            .expect("a write into the read-only remainder of the repo must be blocked");
        assert!(
            fatal,
            "a read root containing the worktree must not soften the write escape: {reason}"
        );

        // Edit and the Bash arm agree — every write path through the boundary, same verdict.
        let (_, fatal) =
            boundary_denial_with(&roots, &wt, None, None, &ctx(sibling), "Edit").expect("blocked");
        assert!(fatal, "an Edit into the repo remainder stays unit-fatal");
        let cmd = json!({ "command": format!("echo x > {sibling}") });
        let (_, fatal) =
            boundary_denial_with(&roots, &wt, None, None, &cmd, "Bash").expect("blocked");
        assert!(
            fatal,
            "a Bash redirect into the repo remainder stays unit-fatal"
        );

        // The repo root itself — the declared read root verbatim as a write target.
        let (_, fatal) = boundary_denial_with(
            &roots,
            &wt,
            None,
            None,
            &ctx(repo.to_str().unwrap()),
            "Write",
        )
        .expect("the read root itself is not writable");
        assert!(
            fatal,
            "writing the read root itself is the same fatal escape"
        );

        // And `..` from the worktree cannot launder the escape into an in-boundary-looking path:
        // normalization collapses it back to the repo remainder before the lists are consulted.
        let dotted = wt.join("..").join("..").join("Cargo.toml");
        let (_, fatal) = boundary_denial_with(
            &roots,
            &wt,
            None,
            None,
            &ctx(dotted.to_str().unwrap()),
            "Write",
        )
        .expect("a dotted escape from the worktree is still a write outside it");
        assert!(
            fatal,
            "`..` out of the worktree into the read root stays unit-fatal"
        );
    }

    /// core#272. Run f09d4331 unit-FAILED at build because the worker wrote its own agent memory
    /// under an ALTERNATE Claude config home (`CLAUDE_CONFIG_DIR=~/alt-configs/.claude`) and the
    /// core#235 carve-out only tested `home/.claude`. The carve-out follows the RESOLVED config
    /// home: the alt tree is advisory, the `home/.claude` fallback still holds when no override
    /// is set, and an override does NOT widen the fallback (with the override set, `home/.claude`
    /// is just another out-of-boundary path — fatal).
    #[test]
    fn the_agent_memory_carveout_follows_the_resolved_config_home() {
        let wt = std::env::temp_dir().join("wicked-boundary-wt-cfg");
        std::fs::create_dir_all(&wt).unwrap();
        let roots = crate::path_policy::AllowedRoots {
            write: vec![wt.clone()],
            read: vec![],
        };
        // A drive-prefixed absolute on Windows (a bare `/Users/op` has no drive there and
        // never matches containment) — and deliberately NOT under temp_dir, which would put
        // every assertion under the core#264 system-temp carve-out and make them vacuous.
        #[cfg(unix)]
        let home = std::path::PathBuf::from("/Users/op");
        #[cfg(windows)]
        let home = std::path::PathBuf::from("C:\\Users\\op");
        let alt = home.join("alt-configs").join(".claude");
        let alt_mem = alt.join("projects/p/memory/MEMORY.md");
        let alt_mem = alt_mem.to_str().unwrap();
        let home_mem = home.join(".claude").join("projects/p/memory/MEMORY.md");
        let home_mem = home_mem.to_str().unwrap();
        let (home, alt) = (home.as_path(), alt.as_path());

        // The f09d4331 shape: memory write under the alt config home → blocked, ADVISORY.
        let (_, fatal) =
            boundary_denial_with(&roots, &wt, Some(home), Some(alt), &ctx(alt_mem), "Write")
                .expect("an agent-memory write outside the worktree is STILL blocked");
        assert!(
            !fatal,
            "a memory write under CLAUDE_CONFIG_DIR must be ADVISORY (core#272)"
        );

        // No override → the core#235 fallback still carves out `home/.claude`.
        let (_, fatal) =
            boundary_denial_with(&roots, &wt, Some(home), None, &ctx(home_mem), "Write")
                .expect("blocked");
        assert!(
            !fatal,
            "without an override, home/.claude stays ADVISORY (core#235)"
        );

        // With the override set, the carve-out MOVES rather than widens: `home/.claude`
        // is no longer the agent-state tree and a write there is an ordinary escape.
        let (_, fatal) =
            boundary_denial_with(&roots, &wt, Some(home), Some(alt), &ctx(home_mem), "Write")
                .expect("blocked");
        assert!(
            fatal,
            "the override replaces the fallback tree; it does not add to it"
        );
    }

    /// core#272 hardening (Copilot). CLAUDE_CONFIG_DIR is trusted input from the worker env:
    /// an empty, relative, or filesystem-root value steering the ADVISORY carve-out would
    /// downgrade every out-of-boundary write. Such values are ignored — fail closed to the
    /// `home/.claude` fallback.
    #[test]
    fn an_over_broad_config_home_override_is_ignored() {
        use std::ffi::OsStr;
        assert_eq!(valid_config_home(OsStr::new("")), None, "empty");
        assert_eq!(
            valid_config_home(OsStr::new("relative/.claude")),
            None,
            "relative"
        );
        #[cfg(unix)]
        {
            assert_eq!(valid_config_home(OsStr::new("/")), None, "filesystem root");
            assert_eq!(
                valid_config_home(OsStr::new("/Users/op/alt-configs/.claude")),
                Some(std::path::PathBuf::from("/Users/op/alt-configs/.claude")),
                "a sane absolute dir passes"
            );
        }
        #[cfg(windows)]
        {
            assert_eq!(valid_config_home(OsStr::new("C:\\")), None, "drive root");
            assert_eq!(
                valid_config_home(OsStr::new("\\")),
                None,
                "rooted but driveless"
            );
            assert_eq!(
                valid_config_home(OsStr::new("C:\\Users\\op\\.claude")),
                Some(std::path::PathBuf::from("C:\\Users\\op\\.claude")),
                "a sane absolute dir passes"
            );
        }
    }

    /// THE case. A governed worker located the pin binding its own gate and began authoring a
    /// replacement. With the worktree armed as the only write root, that write is refused.
    #[test]
    fn the_governance_pin_is_outside_the_boundary() {
        let wt = std::env::temp_dir().join("wicked-boundary-wt");
        std::fs::create_dir_all(&wt).unwrap();
        with_roots(Some(wt.to_str().unwrap()), || {
            // Resolved INSIDE the env lock: the pin path derives from HOME, and a concurrent
            // HOME-pinning test (the skills fixtures pin HOME to a temp scratch) would otherwise
            // hand this test a pin under the system temp, where the core#264 carve-out applies.
            let pin = dirs_config_workflow();
            let (denial, is_write) = boundary_denial_untracked(&ctx(&pin), "Write")
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
            let (_, fatal) = boundary_denial_untracked(&ctx(&mem), "Write")
                .expect("a write outside the worktree is STILL blocked");
            assert!(
                !fatal,
                "a write into the worker's own ~/.claude memory must be ADVISORY, not unit-fatal (core#235)"
            );
            // Control: an escape to a DIFFERENT out-of-boundary path (the gate pin) stays FATAL —
            // the carve-out is scoped to ~/.claude, it does not relax the pin.
            let (_, pin_fatal) = boundary_denial_untracked(&ctx(&dirs_config_workflow()), "Write")
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
            assert!(boundary_denial_untracked(&ctx(inside.to_str().unwrap()), "Write").is_none());
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
            assert!(boundary_denial_untracked(&ctx(escape.to_str().unwrap()), "Write").is_some());
        });
    }

    /// A read of the operational store is still outside the boundary. FINDING-067 was a worker that
    /// reached the platform's own state; reads of it are reconnaissance for exactly that.
    #[test]
    fn reads_outside_the_boundary_are_refused_too() {
        let wt = std::env::temp_dir().join("wicked-boundary-wt4");
        std::fs::create_dir_all(&wt).unwrap();
        with_roots(Some(wt.to_str().unwrap()), || {
            assert!(boundary_denial_untracked(&ctx("/etc/passwd"), "Read").is_some());
        });
    }

    /// No roots armed means no boundary was CONFIGURED — the honest state for a standalone or
    /// ungoverned invocation. It must not silently become "deny everything" and break those, nor
    /// be mistaken for a boundary that passed.
    #[test]
    fn an_unarmed_boundary_is_absent_not_permissive_and_not_denying() {
        with_roots(None, || {
            assert!(boundary_denial_untracked(&ctx("/etc/passwd"), "Write").is_none());
        });
    }

    /// A Bash command with no write target is not a boundary question — a read/list is not an escape.
    #[test]
    fn a_bash_command_with_no_write_target_is_not_judged() {
        let wt = std::env::temp_dir().join("wicked-boundary-wt5");
        std::fs::create_dir_all(&wt).unwrap();
        with_roots(Some(wt.to_str().unwrap()), || {
            assert!(boundary_denial_untracked(&json!({"command": "ls /"}), "Bash").is_none());
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
            let d = boundary_denial_untracked(
                &json!({"command": "echo pwned > /etc/evil-marker"}),
                "Bash",
            );
            assert!(
                d.as_ref().is_some_and(|(_, is_write)| *is_write),
                "a Bash redirect out of the worktree must be a FATAL boundary deny: {d:?}"
            );
            // cp and tee destinations outside → also denied.
            assert!(
                boundary_denial_untracked(
                    &json!({"command": "cp ./a.txt /etc/evil-marker"}),
                    "Bash"
                )
                .is_some(),
                "cp to an outside destination must be denied"
            );
            assert!(
                boundary_denial_untracked(
                    &json!({"command": "echo x | tee /etc/evil-marker"}),
                    "Bash"
                )
                .is_some(),
                "tee to an outside file must be denied"
            );
            // R8 (DES-L4 PR-②, F-RC1-092): `mkdir` is a write target — outside the boundary it is
            // unit-FATAL exactly like a redirect outside; inside the tree it is allowed.
            let d = boundary_denial_untracked(
                &json!({"command": "mkdir -p /etc/evil-dir/sub"}),
                "Bash",
            );
            assert!(
                d.as_ref().is_some_and(|(_, is_write)| *is_write),
                "a Bash mkdir outside the worktree must be a FATAL boundary deny: {d:?}"
            );
            assert!(
                boundary_denial_untracked(
                    &json!({ "command": format!("mkdir -p {}/new/dir", wt.display()) }),
                    "Bash"
                )
                .is_none(),
                "a mkdir inside the worktree must be allowed"
            );
            // A write INSIDE the worktree is fine — the boundary is a fence, not a Bash ban.
            assert!(
                boundary_denial_untracked(
                    &json!({ "command": format!("echo ok > {inside}") }),
                    "Bash"
                )
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
                    boundary_denial_untracked(&json!({ "command": sink }), "Bash").is_none(),
                    "a Bash write to a standard sink must be allowed: {sink}"
                );
            }
            // NON-MASKING: splitting the glued `;` must not hide a second redirect that DOES escape.
            // `>/dev/null;>/etc/evil` is a safe sink followed by a glued escape — the escape must win.
            assert!(
                boundary_denial_untracked(
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
                boundary_denial_untracked(&json!({ "command": escaped }), "Bash").is_some(),
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
                    boundary_denial_untracked(&json!({ "command": quoted }), "Bash").is_some(),
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
            launcher
                .contains("armed_write_roots(&cwd, &g.extra_write_roots, graph_write.as_deref())"),
            "the ONLY widenings must be the launch-validated extra_write_roots riding the \
             governance context and the ENGINE-resolved repo-graph key dir \
             (`graph_write_dir`, never worker-controlled) — not env, not the unit, not the \
             workflow def"
        );
        // The launch side must actually judge those extras — remove the validation and the
        // widening becomes an unvetted door straight past FINDING-098.
        let launch = include_str!("actor.rs");
        assert!(
            launch.contains("validate_extra_write_roots"),
            "the launch path no longer validates extra write roots against the pin tree"
        );
        // The worker's scratch is pointed INSIDE the boundary (core#264) — removing the TMPDIR
        // arming silently reintroduces the advisory-deny noise (and, before #264, the fatal
        // aborts) on every worker that uses platform temp.
        assert!(
            launcher.contains(r#"cmd.env("TMPDIR", &unit_tmp)"#),
            "the launcher no longer points the governed worker's TMPDIR into the unit tree"
        );
    }

    /// The read mirror of the wiring audit above (core#294): the launch-declared extra READ roots
    /// must ride the ONE shared assembly on BOTH carriers, and must be judged at launch. The
    /// helper tests stay green if any of these calls is deleted — that gap has bitten three times
    /// (`run_gate_hook_actually_consults_the_boundary`), so assert the wiring itself.
    #[test]
    fn the_launcher_arms_the_launch_declared_read_roots() {
        // The wrapped carrier: extras enter WICKED_READ_ROOTS through `assemble_read_roots` —
        // never through `armed_write_roots`, whose exact argument list the test above pins.
        // Whitespace-collapsed so the audit pins the CALL, not rustfmt's line breaks — and only
        // its STABLE prefix, through the fourth argument, so rustfmt's choice of a trailing comma
        // before `)` cannot fail the audit either (Copilot, review pass 11). The second argument
        // is the runner's operational state home (core#406): the graph-derived read root is
        // recognised against THIS daemon's repo-graph root, never the default home's. The fourth
        // is the skills snapshot (core#396): the same one assembly read-widens to it.
        let launcher: String = include_str!("execute_wrapped.rs")
            .split_whitespace()
            .collect();
        assert!(
            launcher.contains(
                "assemble_read_roots(g.code_graph_db.as_deref(),self.operational_home.as_deref(),&g.extra_read_roots,g.skills_root.as_deref()"
            ),
            "the wrapped launcher no longer joins the launch-declared extra_read_roots (and the \
             skills snapshot) into WICKED_READ_ROOTS (core#294, core#396)"
        );
        // The ACP carrier builds its BoundaryCtx from the same assembly (core#260's one-assembly
        // rule): dropping the extras there would make the read grant depend on which seat the run
        // resolved to — the exact per-seat divergence core#297 §1 closed for deliverables.
        let acp = include_str!("acp_runner.rs");
        assert!(
            acp.contains("&g.extra_read_roots,"),
            "the ACP carrier no longer hands the launch-declared extra_read_roots to the shared \
             read-root assembly (core#294)"
        );
        // The launch side must actually judge the extras — same rules as the write roots, or the
        // widening becomes an unvetted grant.
        let launch = include_str!("actor.rs");
        assert!(
            launch.contains("validate_extra_read_roots"),
            "the launch path no longer validates extra read roots (core#294)"
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
        let _g = ENV.write().unwrap_or_else(|e| e.into_inner());
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

/// core#296 — PHASE SCOPE, the second boundary. The filesystem boundary above answers WHERE a unit
/// may write; these answer WHAT KIND of file a given PHASE may write. Every case here sits INSIDE
/// the unit's own worktree, which is precisely why the filesystem boundary had nothing to say about
/// the reported failure.
#[cfg(test)]
mod phase_scope_tests {
    use super::*;
    use serde_json::json;

    fn ctx(path: &str) -> serde_json::Value {
        json!({ "path": path })
    }

    /// The rule, judged case by case in BOTH directions. `Some` = the write is refused.
    #[test]
    fn a_pre_build_phase_may_write_documentation_and_nothing_else() {
        // Refused: production code, config, lockfiles, assets — all reachable through a path-bearing
        // write tool, all things a recon/design phase has no business producing.
        for (tool, path) in [
            ("Write", "src/board/attentionReason.ts"), // run d1bc72c2, verbatim
            ("Write", "tests/attentionReason.test.ts"), // its sibling, same run
            ("Edit", "src/lib.rs"),
            ("NotebookEdit", "analysis/run.ipynb"),
            ("Write", "package.json"),
            ("Write", "docs.ts"), // a FILE named docs is code; only a docs/ DIRECTORY is not
        ] {
            assert!(
                legacy_scope(true, false, &ctx(path), tool, std::path::Path::new("/wt")).is_some(),
                "a pre-build phase must not `{tool}` `{path}`"
            );
        }
        // Allowed: the phase's own deliverable. A gate that refused this would stop the phase doing
        // the one job it exists for, which is how a control gets switched off.
        for (tool, path) in [
            ("Write", "DESIGN.md"),
            ("Write", "docs/architecture/overview.html"),
            ("Edit", ".product/DES-EXEC-001.md"),
            ("Write", "./notes.TXT"),
            ("Write", "spec.rst"),
        ] {
            assert_eq!(
                legacy_scope(true, false, &ctx(path), tool, std::path::Path::new("/wt")),
                None,
                "a pre-build phase MUST be able to `{tool}` its deliverable `{path}`"
            );
        }
    }

    /// The bypass this gate shipped with, pinned in both directions.
    ///
    /// `is_documentation_change` asks whether ANY directory segment is `docs` or `.product`,
    /// because git hands it repo-relative paths. The gate hands it the ABSOLUTE tool-call path,
    /// so every ancestor of the worktree was judged too — and a checkout under a directory named
    /// `docs` or `.product` silently disabled the whole gate. Found by adversarial verification,
    /// not by review: the code reads correctly, the predicate is reused as intended, and the CI
    /// suite was green. Only probing the shipped function with a realistic absolute path shows it.
    #[test]
    fn an_ancestor_directory_cannot_grant_documentation_status() {
        let wt = std::path::Path::new("/Users/me/docs/projects/studio");
        // Production code inside a worktree that merely LIVES under a `docs` ancestor is still
        // production code. Before the fix every one of these returned None — allowed.
        for path in [
            "/Users/me/docs/projects/studio/src/board/attentionReason.ts",
            "/Users/me/docs/projects/studio/package.json",
            "/Users/me/docs/projects/studio/src/lib.rs",
        ] {
            assert!(
                legacy_scope(true, false, &ctx(path), "Write", wt).is_some(),
                "`{path}` is production code; the `docs` ANCESTOR must not exempt it"
            );
        }
        // Same shape for `.product`, the other segment the predicate honours.
        let ci = std::path::Path::new("/home/ci/.product/checkout");
        assert!(
            legacy_scope(
                true,
                false,
                &ctx("/home/ci/.product/checkout/src/lib.rs"),
                "Write",
                ci
            )
            .is_some(),
            "a `.product` ANCESTOR must not exempt production code either"
        );
        // The other direction, which matters just as much: a real deliverable INSIDE the worktree
        // is still allowed once the prefix is stripped. A fix that denied these would be worse
        // than the bypass, because it would stop a pre-build phase doing its actual job.
        for path in [
            "/Users/me/docs/projects/studio/docs/architecture/overview.html",
            "/Users/me/docs/projects/studio/.product/DES-EXEC-001.md",
            "/Users/me/docs/projects/studio/DESIGN.md",
        ] {
            assert_eq!(
                legacy_scope(true, false, &ctx(path), "Write", wt),
                None,
                "`{path}` IS the phase's deliverable and must stay writable"
            );
        }
        // Outside the worktree entirely: judged on the filename alone, so an ancestor we do not
        // own cannot lend it documentation status. (The filesystem boundary refuses these first;
        // this is defence in depth, not the primary control.)
        assert!(
            legacy_scope(
                true,
                false,
                &ctx("/somewhere/docs/evil/src/main.rs"),
                "Write",
                wt
            )
            .is_some(),
            "a path outside the worktree must not borrow `docs` from an ancestor"
        );
    }

    /// The scope is a property of the PHASE, not a blanket ban. Off the marker, every write above
    /// is permitted — scoping a Creator away from creating is the inverse failure, and a worse one.
    #[test]
    fn a_phase_that_is_not_pre_build_is_never_scope_denied() {
        for path in ["src/lib.rs", "package.json", "tests/x.test.ts"] {
            assert_eq!(
                legacy_scope(
                    false,
                    false,
                    &ctx(path),
                    "Write",
                    std::path::Path::new("/wt")
                ),
                None
            );
        }
    }

    /// The honest limit, stated as a test so nobody reports this as confinement: a READ is never
    /// refused (a design phase must read the code it designs against), and a write tool with no
    /// usable path is not a judgeable call. Since DES-L4 PR-② (R7b) a `Bash` heredoc IS judged —
    /// by its write targets, not by a `path` — so a pre-build shell write of production code is
    /// refused (advisory) exactly like `Write` would be; `actor::phase_scope_warning` stays live as
    /// the completion backstop for the shapes the target scan cannot see.
    #[test]
    fn reads_are_outside_this_gates_reach_and_shell_writes_are_now_inside_it() {
        assert_eq!(
            legacy_scope(
                true,
                false,
                &ctx("src/lib.rs"),
                "Read",
                std::path::Path::new("/wt")
            ),
            None
        );
        assert_eq!(
            legacy_scope(
                true,
                false,
                &ctx("src/lib.rs"),
                "Grep",
                std::path::Path::new("/wt")
            ),
            None
        );
        let shell = legacy_scope(
            true,
            false,
            &json!({"command": "cat > src/lib.rs <<'EOF'\nx\nEOF", "path": null}),
            "Bash",
            std::path::Path::new("/wt"),
        )
        .expect("R7b: a pre-build shell write of production code is judged by its target");
        assert!(
            shell.contains("PRE-BUILD") && shell.contains("`src/lib.rs`"),
            "{shell}"
        );
        assert_eq!(
            legacy_scope(
                true,
                false,
                &json!({"command": "cat > docs/design.md <<'EOF'\nx\nEOF", "path": null}),
                "Bash",
                std::path::Path::new("/wt")
            ),
            None,
            "a pre-build shell write of DOCUMENTATION keeps the core#296 allowance"
        );
        // A write tool with no usable path is not a judgeable call.
        assert_eq!(
            legacy_scope(
                true,
                false,
                &json!({"path": null}),
                "Write",
                std::path::Path::new("/wt")
            ),
            None
        );
        assert_eq!(
            legacy_scope(
                true,
                false,
                &ctx("   "),
                "Write",
                std::path::Path::new("/wt")
            ),
            None
        );
    }

    /// A refusal the worker cannot act on is a wall, not a gate (FINDING-066). The reason has to
    /// carry the file, the rule, and the way forward.
    #[test]
    fn the_refusal_names_the_file_the_rule_and_the_way_forward() {
        let reason = legacy_scope(
            true,
            false,
            &ctx("src/board/attentionReason.ts"),
            "Write",
            std::path::Path::new("/wt"),
        )
        .expect("this is the reported write");
        assert!(reason.contains("src/board/attentionReason.ts"), "{reason}");
        assert!(reason.contains("PRE-BUILD"), "{reason}");
        assert!(reason.contains(".md"), "names where it MAY write: {reason}");
        assert!(reason.contains("docs/"), "{reason}");
        assert!(reason.contains("build phase"), "{reason}");
    }

    /// The env carrier's PARSE, tested purely — the launcher sets exactly `1`, and an inherited
    /// junk value must never scope a build phase away from building.
    #[test]
    fn the_env_flag_is_parsed_strictly() {
        use std::ffi::OsStr;
        for on in ["1", "true", "TRUE", "True"] {
            assert!(parse_pre_build_scope(Some(OsStr::new(on))), "{on:?}");
        }
        for off in ["", "0", "false", "yes", "on", "2", "1 "] {
            assert!(!parse_pre_build_scope(Some(OsStr::new(off))), "{off:?}");
        }
        assert!(
            !parse_pre_build_scope(None),
            "UNSET is the honest `no phase scope declared` state (a standalone gate-hook, a \
             build phase) and must not read as ON"
        );
    }

    /// An advisory deny, NOT a unit-fatal one: the harmful thing was prevented and the worker was
    /// told how to proceed, so it adapts and the run continues. The allowlist is keyed on BOTH the
    /// evaluator identity and the claim-id prefix, so nothing else can borrow the downgrade.
    #[test]
    fn a_phase_scope_deny_is_advisory_and_only_a_real_one_is() {
        let claim = |id: &str, evaluator: &str, decision: Decision| ConformanceClaim {
            claim_id: id.to_string(),
            scope: "unit".to_string(),
            phase: "unit-2".to_string(),
            policy_ids: vec![PHASE_SCOPE_RULE_ID.to_string()],
            decision,
            obligations: vec![],
            evaluated_context_ref: "sha256:phase-scope".to_string(),
            criteria: String::new(),
            evaluator_identity: evaluator.to_string(),
            evaluated_at: crate::clock::eval_now(),
        };
        assert!(is_advisory_deny(&claim(
            "phase-scope-deny:unit-2",
            PHASE_SCOPE_EVALUATOR,
            Decision::Deny
        )));
        // A POLICY deny that merely borrows the claim-id shape is still fatal — and so is a
        // boundary escape that borrows the evaluator identity.
        assert!(!is_advisory_deny(&claim(
            "phase-scope-deny:unit-2",
            "wicked-governance",
            Decision::Deny
        )));
        assert!(!is_advisory_deny(&claim(
            "boundary-deny:unit-2",
            PHASE_SCOPE_EVALUATOR,
            Decision::Deny
        )));
        assert!(!is_advisory_deny(&claim(
            "phase-scope-deny:unit-2",
            PHASE_SCOPE_EVALUATOR,
            Decision::Allow
        )));
    }

    /// CALL-SITE AUDIT — the lesson this campaign has re-learned three times (FINDING-091/093, and
    /// the boundary's own `run_gate_hook_actually_consults_the_boundary`). Every test above calls
    /// `phase_scope_denial` DIRECTLY, so all of them stay green if the call is deleted from the
    /// evaluator or the launcher stops arming the flag — leaving a live, unreachable helper, which
    /// is indistinguishable from the prompt-only scope this issue is about.
    #[test]
    fn both_carriers_actually_consult_the_phase_scope() {
        let src = include_str!("gate_hook.rs");
        let body = src
            .split("pub(crate) fn evaluate_tool_call")
            .nth(1)
            .and_then(|b| b.split("\n/// ").next())
            .expect("evaluate_tool_call is still a top-level fn");
        assert!(
            body.contains("phase_scope_denial("),
            "evaluate_tool_call no longer consults the phase scope — the helper is live and \
             unreachable, which is the core#296 failure exactly: a scope that exists only as prose"
        );
        assert!(
            body.contains("b.pre_build_scope") && body.contains("pre_build_scope_from_env()"),
            "both carriers must supply the flag: the in-process one from BoundaryCtx, the hook \
             subprocess from its env. A carrier that skipped it would run unscoped while the other \
             enforced — the core#260 asymmetry, reopened on a new axis"
        );
        // The launcher half. Without it the flag is set nowhere, `pre_build_scope_from_env` is
        // always false, and every wrapped-path pre-build unit is unscoped — silently, because
        // "unset" is a legitimate state for a build phase.
        let launcher = include_str!("execute_wrapped.rs");
        assert!(
            launcher.contains("PRE_BUILD_SCOPE_ENV"),
            "execute_wrapped no longer arms {PRE_BUILD_SCOPE_ENV} on the governed child"
        );
        assert!(
            launcher.contains("input.unit.pre_build_scope"),
            "the flag must come from the UNIT (def-derived at plan time), not from a workflow id, \
             an env var, or anything the worker controls"
        );
        // The ACP half — the carrier the reported run actually used.
        let acp = include_str!("acp_runner.rs");
        assert!(
            acp.contains("pre_build_scope: input.unit.pre_build_scope"),
            "the ACP runner no longer carries the unit's phase scope into its BoundaryCtx"
        );
    }

    /// F-7R2-012 (wave 6): a worker seat's `git push` / `gh pr create` is refused by the Bash
    /// command filter — ADVISORY (blocked, the seat continues with the remedy), recorded under
    /// its own claim id so the fold discloses `workerToolCallDenied` with the command, and the
    /// allowlist reads it as advisory (never a unit denial). Reads and local git pass.
    #[test]
    fn a_remote_write_command_is_refused_advisory_with_the_remedy_and_disclosed_to_the_fold() {
        let wt =
            std::env::temp_dir().join(format!("wicked-remote-write-wt-{}", std::process::id()));
        std::fs::create_dir_all(&wt).unwrap();
        let roots = crate::path_policy::AllowedRoots {
            write: vec![wt.clone()],
            read: vec![],
        };
        let judge = |command: &str| {
            boundary_denial_with(
                &roots,
                &wt,
                None,
                None,
                &serde_json::json!({ "command": command }),
                "Bash",
            )
        };
        let (reason, fatal) =
            judge("cd /wt && git push -u origin wicked/run").expect("a push is refused");
        assert!(!fatal, "advisory: the seat continues with the remedy");
        assert!(reason.starts_with(REMOTE_WRITE_REASON_PREFIX), "{reason}");
        assert!(
            reason.contains(crate::remote_write_fence::REMEDY),
            "{reason}"
        );
        assert!(
            judge("gh pr create --fill").is_some(),
            "a PR opened from a seat is refused"
        );
        assert!(
            judge("gh pr view 258 --json state").is_none(),
            "a read passes"
        );
        assert!(
            judge("git add -A && git commit -qm x").is_none(),
            "a local commit passes"
        );

        // The record the hook writes, and what the fold reads back from it.
        let run_id = format!("remote-write-{}", std::process::id());
        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));
        let p = decisions_path_for(&run_id, 0);
        write_armed_marker(&p, "unit-7").unwrap();
        let command = "cd /wt && git push -u origin wicked/run";
        append_remote_write_deny(p.to_str().unwrap(), "wf/unit-7", "unit-7", &reason, command);
        let recs = collect_hook_decisions(&run_id, 0, "unit-7");
        let (rec_reason, rec_command) = recs
            .iter()
            .find_map(|r| r.remote_write_refusal())
            .expect("the fold sees the refusal");
        assert_eq!(rec_command, command);
        assert!(rec_reason.contains("`git push`"), "{rec_reason}");
        assert!(recs[0].claim_id.starts_with(REMOTE_WRITE_DENY_PREFIX));
        // The claim itself is ADVISORY by the allowlist — prevention, not a violation.
        let raw = std::fs::read_to_string(&p).unwrap();
        let claim: ConformanceClaim = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<ConformanceClaim>(l).ok())
            .next()
            .expect("one conformance claim recorded");
        assert!(is_advisory_deny(&claim), "{claim:?}");
        assert_eq!(claim.obligations.get(1).map(String::as_str), Some(command));
        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));
        let _ = std::fs::remove_dir_all(&wt);
    }

    /// Review of #449 (FN-1/FN-2): every bypass spelling the review reproduced is refused by the
    /// WRAPPED carrier's Bash arm — advisory, with the remedy — and nothing in the corpus is
    /// mistaken for a path question.
    #[test]
    fn every_review_bypass_string_is_refused_by_the_gate_hook() {
        let wt =
            std::env::temp_dir().join(format!("wicked-remote-write-corpus-{}", std::process::id()));
        std::fs::create_dir_all(&wt).unwrap();
        let roots = crate::path_policy::AllowedRoots {
            write: vec![wt.clone()],
            read: vec![],
        };
        for cmd in crate::remote_write_fence::REVIEW_BYPASS_STRINGS {
            let verdict = boundary_denial_with(
                &roots,
                &wt,
                None,
                None,
                &serde_json::json!({ "command": cmd }),
                "Bash",
            );
            let (reason, fatal) = verdict.unwrap_or_else(|| panic!("not refused: {cmd}"));
            assert!(!fatal, "advisory, the seat continues: {cmd}");
            assert!(
                reason.starts_with(REMOTE_WRITE_REASON_PREFIX)
                    && reason.contains(crate::remote_write_fence::REMEDY),
                "{cmd}: {reason}"
            );
        }
        let _ = std::fs::remove_dir_all(&wt);
    }
}
