//! WRAPPED-CLI execute backend (P4a) — the real [`StepRunner`] that runs an actual agentic CLI as a
//! subprocess in the run's worktree and captures its output. This is the organ that makes the
//! orchestrator *do real work* instead of returning a stub string.
//!
//! It implements ONLY the worker half (work production); the actor still owns the per-unit governance
//! gate + cursor + evidence (single-writer). The CLI is invoked **augment mode** (its own tools, no
//! hermetic lockdown). Per-tool-call PreToolUse governance (the gate-hook drain) is P4b — until then a
//! unit's output is governed at the unit level by the existing gate.
//!
//! Security: the prompt is passed as its OWN argv element with no shell (no command injection), with a
//! POSIX `--` end-of-options guard so a flag-shaped prompt can't smuggle a flag. Output is drained
//! CONCURRENTLY on threads while the child runs, so a verbose CLI exceeding the ~64KB pipe buffer can't
//! deadlock (the bug the P2 review flagged for this phase). The run is bounded by a timeout.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::domain::WorkUnit;
use crate::workflow::{DeltaSink, StepInput, StepOutput, StepRunner, StepStatus, Usage};

/// The structured signals an [`OutputAdapter`] extracts from ONE raw stdout line (DES-STUDIO-COCKPIT-001
/// §3 B-runner). `text` is 0..n readable deltas to stream through the [`DeltaSink`] as `CliOutputDelta`
/// (never raw JSON — FR-2 live output stays prose); `usage` is the end-of-run token/cost total when the
/// line carried it; `files` are data-file paths the CLI touched (`tool_use.input.file_path`).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AdapterOut {
    pub text: Vec<String>,
    pub usage: Option<Usage>,
    pub files: Vec<String>,
}

/// Per-CLI stdout adapter: turns a binary's raw stdout into readable deltas plus optional structured
/// signals (usage, files). Selected by the resolved binary — `claude` → [`ClaudeStreamJson`], everything
/// else → [`Passthrough`]. The default (passthrough) path is byte-identical to the pre-adapter behavior
/// (every line is one delta, no usage/files), so a non-claude run is unchanged.
pub trait OutputAdapter: Send {
    /// Consume one raw stdout line; return the readable deltas + any structured signals it carried.
    fn on_line(&mut self, line: &str) -> AdapterOut;
    /// Flush any buffered state when stdout closes (both current adapters are stateless → empty).
    fn finish(&mut self) -> AdapterOut {
        AdapterOut::default()
    }
}

/// Default adapter for every non-claude binary: each raw line is exactly one readable delta, no usage or
/// files. Byte-identical to the original raw-line streaming.
struct Passthrough;

impl OutputAdapter for Passthrough {
    fn on_line(&mut self, line: &str) -> AdapterOut {
        AdapterOut {
            text: vec![line.to_string()],
            usage: None,
            files: Vec::new(),
        }
    }
}

/// The claude `--output-format stream-json --verbose` NDJSON adapter (DES §6b, empirically grounded).
/// Per line: `assistant` `content[].type=="text"` → readable deltas; an `assistant` message whose
/// `content` is a bare STRING (not an array) → one text delta; `content[].type=="tool_use"` with
/// `input.file_path` → data files; `type=="result"` → `Usage` from `usage.input_tokens`/`output_tokens`
/// plus `cost_usd = total_cost_usd` (only when a `usage` object is present — no fabricated 0-token row).
/// FALLBACK (S3): if NO assistant text was emitted during the run, the terminal `result`'s `result`
/// string (the final answer) becomes the text delta, so `StepOutput.output` (the artifact the
/// creator≠evaluator judge reads) is never empty when the answer only arrives in the result envelope.
/// FAIL-SAFE: any line that is not valid JSON (version drift) degrades to a single passthrough text
/// delta, so it never panics and never blocks the run.
#[derive(Default)]
pub(crate) struct ClaudeStreamJson {
    /// Whether any assistant text delta was emitted this run — gates the terminal `result` fallback.
    emitted_text: bool,
}

impl OutputAdapter for ClaudeStreamJson {
    fn on_line(&mut self, line: &str) -> AdapterOut {
        if line.trim().is_empty() {
            return AdapterOut::default();
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            // version drift / non-JSON line → degrade to passthrough (fail-safe, never panic/block).
            Err(_) => {
                return AdapterOut {
                    text: vec![line.to_string()],
                    usage: None,
                    files: Vec::new(),
                };
            }
        };
        let mut out = AdapterOut::default();
        match v.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                let content = v.get("message").and_then(|m| m.get("content"));
                match content {
                    // (S3a) `content` is a bare STRING (a valid-JSON shape that `as_array()` misses, so
                    // the text was silently dropped and — being valid JSON — never hit the passthrough
                    // fallback). Treat it as one readable text delta.
                    Some(serde_json::Value::String(s)) => {
                        if !s.is_empty() {
                            out.text.push(s.clone());
                        }
                    }
                    Some(serde_json::Value::Array(blocks)) => {
                        for block in blocks {
                            match block.get("type").and_then(|t| t.as_str()) {
                                // Readable prose → live-output delta (FR-2). Skip empty text blocks.
                                Some("text") => {
                                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                        if !t.is_empty() {
                                            out.text.push(t.to_string());
                                        }
                                    }
                                }
                                // A tool call touching a file (Read/Edit/Write/…) → a data-in-use signal (B4).
                                Some("tool_use") => {
                                    if let Some(fp) = block
                                        .get("input")
                                        .and_then(|i| i.get("file_path"))
                                        .and_then(|f| f.as_str())
                                    {
                                        if !fp.is_empty() {
                                            out.files.push(fp.to_string());
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                if !out.text.is_empty() {
                    self.emitted_text = true;
                }
            }
            // The terminal result carries the run totals + cost directly (B3).
            Some("result") => {
                // (M8) Only synthesize `Usage` when a `usage` object is actually present. A missing
                // `usage` must leave `usage = None` so NO `CliUsage` row is emitted for the unit — never
                // a fabricated "$0.00, 0 tokens" total.
                if let Some(usage) = v.get("usage") {
                    let input_tokens = usage
                        .get("input_tokens")
                        .and_then(|n| n.as_u64())
                        .unwrap_or(0);
                    let output_tokens = usage
                        .get("output_tokens")
                        .and_then(|n| n.as_u64())
                        .unwrap_or(0);
                    let cost_usd = v.get("total_cost_usd").and_then(|c| c.as_f64());
                    out.usage = Some(Usage {
                        input_tokens,
                        output_tokens,
                        cost_usd,
                    });
                }
                // (S3b) FALLBACK: no assistant text streamed this run ⇒ the final answer lives only in the
                // result envelope. Emit `result.result` as text so `StepOutput.output` is non-empty.
                if !self.emitted_text {
                    if let Some(answer) = v.get("result").and_then(|r| r.as_str()) {
                        if !answer.is_empty() {
                            out.text.push(answer.to_string());
                            self.emitted_text = true;
                        }
                    }
                }
            }
            // system / user / rate_limit_event / anything else → no readable output, no signals.
            _ => {}
        }
        out
    }
}

/// Whether the resolved binary is `claude` (selects the stream-json adapter + flag injection). Matches on
/// the file stem so `claude`, `/usr/local/bin/claude`, and `claude.exe` (Windows) all resolve.
///
/// CLAUDE-ADAPTER CONTRACT (known boundary): the stream-json adapter is selected purely by binary stem
/// (`stem == "claude"`); a claude-compatible binary under a different name is NOT recognized (M7). The
/// operator template MUST run claude in print/headless mode (`-p`/`--print`) — that is the mode under
/// which `--output-format stream-json --verbose` emits the NDJSON this adapter parses; without it claude
/// runs interactively and the adapter degrades to passthrough. (M9: a raw stdout line containing invalid
/// UTF-8 is dropped by the `map_while(Result::ok)` line reader — a pre-existing, accepted boundary.)
pub(crate) fn binary_is_claude(bin: &str) -> bool {
    std::path::Path::new(bin)
        .file_stem()
        .map(|s| s == "claude")
        .unwrap_or(false)
}

/// Set to any value to let workers run under the operator's own CLI configuration again.
///
/// The escape hatch for the one legitimate case: an operator deliberately testing their own hooks
/// or skills through a run. It is opt-IN because the safe default has to be the one you get by not
/// knowing this exists.
pub(crate) const INHERIT_OPERATOR_CONFIG_ENV: &str = "WICKED_WORKER_INHERIT_OPERATOR_CONFIG";

/// Directories a worker has no business reading: the operator's agent-tooling state and their
/// credentials. Relative to `$HOME` (or `$USERPROFILE` on Windows).
///
/// `.claude` is listed even though `$CLAUDE_CONFIG_DIR` usually supersedes it — both are denied,
/// because which one is live depends on the daemon's environment and a boundary that depends on
/// environment is not a boundary.
const DENIED_HOME_SUBDIRS: &[&str] = &[
    ".claude",           // operator CLAUDE.md, hooks, memory, plugins, transcripts
    ".wicked",           // this engine's own run state, decisions logs, settings files
    ".wicked-brain",     // an index of a DIFFERENT repo than the one under test
    ".something-wicked", // ecosystem app state (event outbox, app dbs)
    ".config/wicked-core",
    ".config/wicked-council",
    ".ssh",
    ".gnupg",
    ".aws",
    ".config/gcloud",
];

/// Bash verbs that leave the worktree by construction, so no path rule can catch them.
///
/// `find /` and `pkill` are not hypotheticals: transcript forensics caught two whole-filesystem
/// scans and one `pkill` from campaign workers (FINDING-045).
const DENIED_BASH: &[&str] = &[
    "Bash(sudo:*)",
    "Bash(pkill:*)",
    "Bash(killall:*)",
    "Bash(find /:*)",
    "Bash(shutdown:*)",
    "Bash(reboot:*)",
];

/// Keep a worker session out of the operator's own machine state.
///
/// Two separate leaks, one seam (FINDING-047 + FINDING-045):
///
///  1. **Config inheritance.** A worker loaded the operator's user-scope settings — their hooks,
///     their permission defaults, their plugins. Observed consequences: 8 blocked writes from an
///     operator memory hook, 4 skill calls into a brain indexed on an unrelated repo, and 2 writes
///     refused because the operator's settings put the session in `dontAsk`. A run whose behaviour
///     depends on whose laptop it is on is not reproducible. `--setting-sources project,local`
///     drops user scope while leaving auth and the repo's own settings alone.
///
///  2. **No filesystem boundary.** 41 of 331 path-bearing tool calls left the worktree.
///     `--disallowedTools` denies the file tools a path into the directories above.
///
/// LIMITS, stated plainly: this is a deny-list, not a sandbox. `Read(...)` rules govern the file
/// TOOLS — a `Bash(cat …)` of a denied path still gets through, and `DENIED_BASH` only names verbs
/// that are unsalvageable rather than every command that could escape. The rules are also inert
/// under `bypassPermissions` / `--dangerously-skip-permissions`, which is why the mode is pinned
/// below; note the council's seat dispatch DOES pass that trust flag ([`wicked_council::dispatch`]),
/// so seat votes are outside this boundary. The real boundary belongs in the PreToolUse gate-hook,
/// which already sees every call and can reject on the resolved path; this closes the observed leaks
/// in the meantime and does not pretend to close the class.
///
/// The engine's own governance is unaffected: the gate-hook rides a wicked-written `--settings`
/// file ([`arm_input_governance`]), which is a separate source from the three scopes named here.
pub(crate) fn inject_isolation_flags(argv: &mut Vec<String>, invocation: &str) {
    if std::env::var_os(INHERIT_OPERATOR_CONFIG_ENV).is_some() {
        return;
    }
    // Deference is decided against the TEMPLATE, not against the built argv. The argv also holds
    // the prompt, which is workflow- and model-authored, and `build_argv` may place it as a bare
    // token (`-p {PROMPT}` puts it before any `--` guard). Scanning the argv therefore let a prompt
    // whose text began `--setting-sources=…` read as "the operator already pinned this" and suppress
    // the whole injection — untrusted text switching off a boundary. The template is the only place
    // an operator's intent is actually expressed, and `{PROMPT}` tokenizes to the literal
    // placeholder, so prompt content cannot appear here at all.
    let stated = tokenize(invocation);
    let mut flags: Vec<String> = Vec::new();
    // An operator template that already pins its own scopes wins — the same deference
    // `inject_claude_stream_flags` shows `--output-format`.
    if !argv_states(&stated, &["--setting-sources"]) {
        flags.push("--setting-sources".into());
        flags.push("project,local".into());
    }
    // Dropping user scope also drops whatever permission mode lived there, and a `-p` session with
    // no mode denies its own Write calls — verified: the same probe that wrote `probe.txt` under
    // `acceptEdits` got "Claude requested permissions to write to …" with the mode left unset. So
    // the mode has to be stated, not inherited.
    //
    // `acceptEdits` and not `bypassPermissions`/`auto`: both of those make the deny rules below
    // inert. Measured on the live CLI with an identical probe — under `acceptEdits` the read of the
    // operator's config was refused by the rule, under `auto` it went straight through, and under
    // `--dangerously-skip-permissions` likewise. `acceptEdits` is the only mode where a worker can
    // do its job AND stay inside the boundary.
    if !argv_states(&stated, &["--permission-mode"]) {
        flags.push("--permission-mode".into());
        flags.push("acceptEdits".into());
    }
    if !argv_states(&stated, &["--disallowedTools", "--disallowed-tools"]) {
        let rules = deny_rules();
        if !rules.is_empty() {
            flags.push("--disallowedTools".into());
            // Comma-joined into a SINGLE argv entry rather than spread across several: the flag is
            // variadic, and a bare sequence of values invites a parser to keep swallowing until the
            // next `-`-prefixed token — which is exactly where `--settings` lands.
            flags.push(rules.join(","));
        }
    }
    if flags.is_empty() {
        return;
    }
    match argv.iter().position(|a| a == "--") {
        Some(i) => {
            for (k, f) in flags.into_iter().enumerate() {
                argv.insert(i + k, f);
            }
        }
        None => argv.extend(flags),
    }
}

/// Does `argv` already state any of `names`, in EITHER accepted spelling — `--flag value` or
/// `--flag=value`?
///
/// Exists so the three guards in [`inject_isolation_flags`] cannot drift apart. Written out inline,
/// they did: the first two checked both forms and the third checked only the separate-token one, so
/// a template using `--disallowedTools=…` would have had a second copy injected next to it. One
/// helper makes "both forms, every flag" true by construction instead of by three-way vigilance.
///
/// `names` is a slice because some flags have more than one accepted spelling (`--disallowedTools`
/// and `--disallowed-tools` are the same flag); a template using either one must suppress injection.
fn argv_states(argv: &[String], names: &[&str]) -> bool {
    argv.iter().any(|a| {
        names
            .iter()
            .any(|n| a == n || (a.starts_with(n) && a.as_bytes().get(n.len()) == Some(&b'=')))
    })
}

/// The deny rules, in Claude's permission-rule syntax. Used two ways: comma-joined as the
/// `--disallowedTools` value on the wrapped-CLI path, and as `permissions.deny` inside the settings
/// file both the wrapped and ACP paths write (the ACP bridge forwards settings but has its own flag
/// surface, so the file is the only carrier that reaches both).
///
/// Nothing is ever dropped silently. Two things can go wrong, and each degrades to the largest
/// boundary still expressible rather than to none:
///
///  - **No home directory resolves** (HOME and USERPROFILE both unset — plausible for a daemon under
///    launchd/systemd). Only the PATH rules need a home; [`DENIED_BASH`] does not. Returning nothing
///    here would have taken `Bash(sudo:*)` and `Bash(find /:*)` down with the path rules, silently,
///    in exactly the unattended environment where that matters most. So the path rules are skipped
///    with a warning and the verb rules still ship.
///  - **An individual directory cannot be expressed** — non-UTF8, or containing the comma the list is
///    joined on. Skipped with a warning; the remaining rules still ship. Dropping the other dozen
///    because one path is unrepresentable would trade a small hole for a total one.
///
/// This is documented as a deny-list rather than a sandbox (see [`inject_isolation_flags`]), so a
/// partial list is a real if reduced boundary. What must never happen is a gap being SILENT — an
/// operator who reads "the worker is fenced off from `~/.ssh`" needs to hear when it isn't.
///
/// The return is a plain `Vec` because it can never be empty: `DENIED_BASH` is unconditional.
pub(crate) fn deny_rules() -> Vec<String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    if home.is_none() {
        eprintln!(
            "wicked-core: neither HOME nor USERPROFILE is set, so worker isolation cannot build \
             the path deny rules; operator config, credentials and brain state are NOT fenced off \
             from workers (the Bash verb rules still apply)"
        );
    }
    let mut rules: Vec<String> = Vec::new();
    // `$CLAUDE_CONFIG_DIR` first: when the daemon inherits one it is the live config dir, and it is
    // frequently NOT `~/.claude` (that redirection is how the operator's own tooling stays separate).
    // It is also home-independent, so it still contributes when no home resolves.
    let dirs = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .into_iter()
        .chain(
            home.iter()
                .flat_map(|h| DENIED_HOME_SUBDIRS.iter().map(|d| h.join(d))),
        );
    for dir in dirs {
        // A rule carrying a comma would split the joined list into two malformed rules; a non-UTF8
        // path cannot be written into one at all. Skip the entry, but SAY SO — see the doc comment:
        // the hole is acceptable, hiding it is not.
        let representable = dir.to_str().filter(|p| !p.contains(','));
        let Some(p) = representable else {
            eprintln!(
                "wicked-core: worker isolation cannot express a deny rule for {} (non-UTF8 or \
                 contains a comma); this path is NOT fenced off from workers",
                dir.display()
            );
            continue;
        };
        for tool in ["Read", "Edit", "Write"] {
            rules.push(format!("{tool}({p}/**)"));
        }
    }
    rules.extend(DENIED_BASH.iter().map(|s| s.to_string()));
    rules
}

/// Append claude's `--output-format stream-json --verbose` flags to an already-built argv, INSERTED
/// before any `--` end-of-options guard so they are parsed as flags (never demoted to positional args
/// after the prompt). Per-binary rule — only applied when the resolved binary is `claude`; no other
/// seat's template is touched.
pub(crate) fn inject_claude_stream_flags(argv: &mut Vec<String>) {
    // (M6) Skip injection when the operator template already sets `--output-format` (e.g.
    // `--output-format json`): injecting a SECOND `--output-format stream-json` produces conflicting
    // flags that claude rejects, failing the run. Honor the template's choice as-is.
    if argv.iter().any(|a| a == "--output-format") {
        return;
    }
    let flags = ["--output-format", "stream-json", "--verbose"];
    match argv.iter().position(|a| a == "--") {
        Some(i) => {
            for (k, f) in flags.iter().enumerate() {
                argv.insert(i + k, f.to_string());
            }
        }
        None => argv.extend(flags.iter().map(|f| f.to_string())),
    }
}

/// The real wrapped-CLI runner. Resolves each unit's assigned CLI to its invocation template, runs it
/// in the unit's worktree, and maps the exit code to a [`StepStatus`].
pub struct WrappedCliStepRunner {
    /// Per-unit wall-clock bound. A CLI exceeding it is killed and the step reports `Cancelled`.
    timeout: Duration,
    /// Back-channel to the actor's single emit point (relay via `Command::EmitEvent`). `None` for
    /// the `Default` path (no-tx contexts such as standalone tests); `Some` when constructed via
    /// [`WrappedCliStepRunner::with_tx`] (the actor path, seeded from `AcpStepRunner::new`).
    tx: Option<std::sync::mpsc::Sender<crate::command::Command>>,
}

/// Per-unit wall-clock timeout. Default 2 h — agentic CLIs doing real multi-file extraction
/// commonly exceed 15 min. Override with `WICKED_UNIT_TIMEOUT_SECS` (e.g. 900 for conservative
/// environments). Extracted into a helper so both `Default` and `with_tx` stay DRY (Gemini).
fn unit_timeout() -> Duration {
    let secs = std::env::var("WICKED_UNIT_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(7200);
    Duration::from_secs(secs)
}

impl Default for WrappedCliStepRunner {
    fn default() -> Self {
        WrappedCliStepRunner {
            timeout: unit_timeout(),
            tx: None,
        }
    }
}

impl WrappedCliStepRunner {
    /// Construct with a back-channel to the actor so the runner can relay events via
    /// `Command::EmitEvent`. Used by `AcpStepRunner::new` to give the fallback runner a tx.
    pub(crate) fn with_tx(tx: std::sync::mpsc::Sender<crate::command::Command>) -> Self {
        WrappedCliStepRunner {
            timeout: unit_timeout(),
            tx: Some(tx),
        }
    }

    /// Relay a [`CoreEvent`] through the actor's single emit point. No-op when no tx was set.
    fn emit_event(&self, ev: crate::event::CoreEvent) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(crate::command::Command::EmitEvent(ev));
        }
    }
}

impl StepRunner for WrappedCliStepRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        // No live sink → a no-op emit (non-streaming callers).
        let noop = |_: &str| {};
        self.exec(input, &noop)
    }

    fn run_unit_streaming(&self, input: &StepInput, emit: &DeltaSink) -> StepOutput {
        self.exec(input, emit)
    }
}

impl WrappedCliStepRunner {
    /// Run one unit's CLI, streaming stdout lines through `emit` as they arrive.
    fn exec(&self, input: &StepInput, emit: &DeltaSink) -> StepOutput {
        let cli_key = input
            .unit
            .assigned_cli
            .as_deref()
            .unwrap_or("claude")
            .to_string();
        // Prefer the unit's own invocation template (an ad-hoc launch CLI not in the registry); else
        // resolve the key via the council registry.
        let invocation = input
            .unit
            .assigned_invocation
            .clone()
            .unwrap_or_else(|| resolve_invocation(&cli_key));
        let mut argv = build_argv(
            &invocation,
            &skill_prompt(&input.unit),
            &input.unit.allowed_skills,
        );

        // Per-binary output adapter (B-runner). claude → stream-json (+ the two flags, injected before the
        // `--` guard); every other binary → passthrough (byte-identical to the pre-adapter raw-line stream).
        let is_claude = argv.first().map(|a| binary_is_claude(a)).unwrap_or(false);
        if is_claude {
            inject_claude_stream_flags(&mut argv);
            // Before governance arms: isolation applies to EVERY claude unit, governed or not. An
            // ungoverned unit reading the operator's config is the same defect as a governed one
            // doing it (FINDING-047/045).
            inject_isolation_flags(&mut argv, &invocation);
        }

        // GOVERNED unit + claude → arm INPUT governance (DES-OUTGOV-003 §2): write a per-run settings
        // file declaring a PreToolUse gate-hook (every tool; exit 2 = deny ⇒ claude aborts the call),
        // insert `--settings <file>`, and return the child env (decisions log + absolute store path).
        // `--settings` MERGES (the user's own settings stay intact) and lives OUTSIDE the worktree.
        // Non-claude CLIs + ungoverned internal calls (`governance: None`) are untouched.
        let gov_env: Option<GovLaunch> = match (&input.governance, is_claude) {
            (Some(gov), true) => match arm_input_governance(input, gov, &mut argv) {
                Ok(env) => {
                    // (EVT-016) GovernanceContextArmed — wrapped-CLI path successfully armed
                    // governance. Fires before the subprocess starts so the operator can confirm
                    // governance is ON for this unit (distinct from GateEvaluated's signals).
                    self.emit_event(crate::event::CoreEvent::GovernanceContextArmed {
                        session: input.run_id.clone(),
                        ord: input.unit.ord,
                        attempt: input.attempt,
                        path: "wrapped_cli".to_string(),
                        db_path: gov.db_path.clone(),
                    });
                    Some(env)
                }
                // A governed unit whose governance cannot be armed must NOT run ungoverned — fail it.
                Err(e) => {
                    return StepOutput {
                        run_id: input.run_id.clone(),
                        unit_ix: input.unit_ix,
                        attempt: input.attempt,
                        output: format!("(could not arm input governance: {e})"),
                        status: StepStatus::Failed,
                        usage: None,
                        files: Vec::new(),
                        governed: false, // arming failed → not governed (and the unit fails anyway)
                    };
                }
            },
            _ => None,
        };

        // Run in the worktree if the run targets a repo; else a per-run temp sandbox (never the
        // orchestrator's own cwd).
        let cwd = input.workdir.clone().unwrap_or_else(|| sandbox_for(input));
        let _ = std::fs::create_dir_all(&cwd);

        let (status, output, usage, files) = if argv.is_empty() {
            (
                StepStatus::Failed,
                format!("(no invocation configured for cli `{cli_key}`)"),
                None,
                Vec::new(),
            )
        } else {
            let adapter: Box<dyn OutputAdapter> = if is_claude {
                Box::<ClaudeStreamJson>::default()
            } else {
                Box::new(Passthrough)
            };
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]).current_dir(&cwd);
            // The gate-hook subprocess (spawned by claude) reads these: the append-only decisions log,
            // the absolute estate store path, and the unit's scope/phase. Scope/phase travel via ENV
            // (NOT interpolated into the shell hook command) so caller-controlled ids can never inject
            // shell metacharacters — the command string carries only the trusted exe (DES-OUTGOV-003 §8).
            if let Some(g) = &gov_env {
                cmd.env(crate::gate_hook::DECISIONS_PATH_ENV, &g.decisions_path);
                cmd.env(crate::gate_hook::ESTATE_DB_ENV, &g.db_path);
                cmd.env(crate::gate_hook::GATE_SCOPE_ENV, &g.scope);
                cmd.env(crate::gate_hook::GATE_PHASE_ENV, &g.phase);
                if !g.phase_id.is_empty() {
                    cmd.env(crate::gate_hook::GATE_PHASE_ID_ENV, &g.phase_id);
                }
            }
            match run_bounded(cmd, self.timeout, emit, adapter) {
                Ok((0, out, _, usage, files)) => (StepStatus::Ok, out, usage, files),
                Ok((-1, _, err, _, _)) if err == TIMED_OUT => (
                    StepStatus::Cancelled,
                    format!("(cli `{cli_key}` exceeded the timeout and was killed)"),
                    None,
                    Vec::new(),
                ),
                Ok((code, out, err, _, _)) => {
                    let detail = if !out.trim().is_empty() { out } else { err };
                    (
                        StepStatus::Failed,
                        format!("(cli `{cli_key}` exited {code}) {detail}"),
                        None,
                        Vec::new(),
                    )
                }
                Err(e) => (
                    StepStatus::Failed,
                    format!("(could not run `{}`: {e})", argv[0]),
                    None,
                    Vec::new(),
                ),
            }
        };

        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output,
            status,
            usage,
            files,
            // The wrapped-CLI runner is the ONLY authority on whether input governance was armed (it wrote
            // the armed marker). The fold trusts this, not unit properties, so a stub/test runner never
            // false-denies a claude-assigned unit for a marker it never wrote.
            governed: gov_env.is_some(),
        }
    }
}

/// A per-run temp sandbox for repo-less runs (so a real CLI never edits the orchestrator's own tree).
fn sandbox_for(input: &StepInput) -> PathBuf {
    std::env::temp_dir()
        .join("wicked-core-sandbox")
        .join(&input.run_id)
}

/// The `PreToolUse` hook command — the exe path quoted for the platform's shell so a `$`/backtick/space
/// in the install path can't be expanded or split. POSIX single-quotes disable all expansion (with the
/// standard `'\''` escape for an embedded quote); Windows cmd double-quotes (it does not expand `$`).
fn quote_exe_command(exe: &str) -> String {
    #[cfg(unix)]
    {
        format!("'{}' gate-hook", exe.replace('\'', "'\\''"))
    }
    #[cfg(not(unix))]
    {
        format!("\"{exe}\" gate-hook")
    }
}

/// Resolve the absolute path to the `wicked-core` binary for the gate-hook command.
///
/// When wicked-core is loaded as a napi-rs native addon inside Node.js (the wicked-crew TS
/// layer), `std::env::current_exe()` returns the Node.js binary path. Using that would produce
/// a hook command like `'/opt/homebrew/bin/node' gate-hook` that Node.js would try to
/// `require('gate-hook')` — always failing. Resolution order:
///
/// 1. `$WICKED_CORE_EXE` — explicit daemon override (the daemon sets this to its own path).
/// 2. `current_exe()` if its filename is not `node` or `node.exe` (native non-addon launch).
/// 3. PATH lookup of `wicked-core` — fallback for installs that put the binary on PATH.
/// 4. Bare `"wicked-core"` string — last resort; will fail at hook time if not on PATH.
fn resolve_wicked_core_exe() -> String {
    // 1. Operator override — trim so trailing whitespace/newlines don't break the hook command.
    if let Ok(v) = std::env::var("WICKED_CORE_EXE") {
        let trimmed = v.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    // 2. current_exe() — valid unless it is the Node.js interpreter.
    if let Ok(path) = std::env::current_exe() {
        let is_node = path
            .file_name()
            .and_then(|n| n.to_str())
            // Case-insensitive: Windows may report Node.exe / NODE.EXE.
            .map(|n| n.eq_ignore_ascii_case("node") || n.eq_ignore_ascii_case("node.exe"))
            .unwrap_or(false);
        if !is_node {
            return path.to_string_lossy().into_owned();
        }
    }
    // 3. PATH lookup.
    if let Ok(found) = which_binary("wicked-core") {
        return found;
    }
    // 4. Bare name — last resort; works if wicked-core is on PATH at hook-execution time.
    "wicked-core".to_string()
}

/// Locate `wicked-estate-mcp` binary for injecting estate tools into governed workers.
/// Priority: sibling of wicked-core binary → user-local install → cargo → PATH.
fn resolve_estate_mcp_exe() -> String {
    // 1. Sibling of the wicked-core binary (monorepo dev: target/release/).
    if let Ok(path) = std::env::current_exe() {
        let is_node = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.eq_ignore_ascii_case("node") || n.eq_ignore_ascii_case("node.exe"))
            .unwrap_or(false);
        if !is_node {
            if let Some(parent) = path.parent() {
                let sibling = parent.join(if cfg!(windows) {
                    "wicked-estate-mcp.exe"
                } else {
                    "wicked-estate-mcp"
                });
                if sibling.exists() {
                    return sibling.to_string_lossy().into_owned();
                }
            }
        }
    }
    // 2. User-local install / cargo / PATH.
    if let Ok(found) = which_binary("wicked-estate-mcp") {
        return found;
    }
    "wicked-estate-mcp".to_string()
}

/// Locate a binary on PATH using the same search the shell would do.
fn which_binary(name: &str) -> Result<String, ()> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let exe_name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(&exe_name);
        if candidate.is_file() {
            if let Some(s) = candidate.to_str().map(|s| s.to_string()) {
                return Ok(s);
            }
        }
    }
    Err(())
}

/// What the launcher sets on the wrapped CLI's `Command` to arm INPUT governance. Scope/phase are set as
/// ENV (never interpolated into the shell hook command) so caller-controlled ids cannot inject shell
/// metacharacters.
struct GovLaunch {
    decisions_path: PathBuf,
    db_path: String,
    scope: String,
    phase: String,
    /// The unit's WORKFLOW phase id (e.g. `review`) — set as `WICKED_GATE_PHASE_ID` so the hook's
    /// policy `select` matches an operator-authored `applies_to` (FINDING-021). Empty ⇒ unset.
    phase_id: String,
}

/// Arm INPUT governance for a governed claude unit (DES-OUTGOV-003 §2): derive the unit's REAL
/// `resolve_scope(...)` / `unit-{ord}` (so the hook's policy `select` + the recorded `claim.phase` match
/// the run engine's own per-unit gate, findings #1/#7), write a per-(unit,attempt) `--settings` file
/// declaring the `PreToolUse` gate-hook, insert `--settings <file>` into `argv` (before the prompt / any
/// `--` guard), and return the env the launcher sets on the child. Settings + decisions live under a
/// per-run/attempt dir OUTSIDE the worktree (no repo pollution).
///
/// SECURITY: the hook command is a CONSTANT (`"<exe>" gate-hook`) — only the trusted `current_exe()` is
/// interpolated (double-quoted for spaces; it contains no shell metacharacters). Scope/phase (which
/// embed the caller-controlled `session_id`/`unit.id`) travel via `WICKED_GATE_SCOPE`/`WICKED_GATE_PHASE`
/// env, so no attacker-controlled data ever reaches the shell-executed command string — closing the
/// injection / fail-open hole a naive double-quoted argv would open.
fn arm_input_governance(
    input: &StepInput,
    gov: &crate::workflow::GovernanceContext,
    argv: &mut Vec<String>,
) -> std::io::Result<GovLaunch> {
    let scope = crate::scope::resolve_scope(input.entity_mode, &input.run_id, &input.unit.id);
    let phase = crate::scope::unit_phase(input.unit.ord);
    let decisions_path = crate::gate_hook::decisions_path_for(&input.run_id, input.attempt);
    // Resolve the wicked-core binary path for the gate-hook command.
    //
    // When wicked-core runs as a napi-rs addon loaded by Node.js (the wicked-crew TS layer),
    // `std::env::current_exe()` returns the Node.js binary — which would produce a hook command
    // like `'/opt/homebrew/bin/node' gate-hook` that Node then tries to require() as a module.
    // Resolution order:
    //   1. $WICKED_CORE_EXE — explicit override from the daemon's process environment
    //   2. current_exe() — if it does not end with "node"/"node.exe" (i.e., not the napi addon path)
    //   3. PATH lookup of "wicked-core" — covers cargo-install and wicked-crew npm-install scenarios
    //   4. Bare "wicked-core" — last resort; works if the binary is on PATH at hook-execution time
    let exe = resolve_wicked_core_exe();
    // exit 2 = deny ⇒ claude aborts the tool-call; matcher "*" governs EVERY tool. Only the exe is
    // interpolated (scope/phase go via env). Quote it per-platform so a `$`/backtick in the install path
    // can't be shell-expanded (POSIX single-quote disables ALL expansion; on Windows cmd `$`/backtick are
    // not special, so double-quote for spaces — a `"` is illegal in a Windows path anyway).
    let command = quote_exe_command(&exe);
    // Include the wicked-estate MCP server so governed workers have access to the 23 estate tools
    // (graph-view, code-graph, memory recall, etc.). The db_path comes from GovernanceContext —
    // the same store the gate-hook uses — so no separate lookup is needed. Using the resolved exe
    // directory to find wicked-estate-mcp avoids hardcoding a PATH dependency.
    let estate_mcp_exe = resolve_estate_mcp_exe();
    let settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [
                { "matcher": "*", "hooks": [ { "type": "command", "command": command } ] }
            ]
        },
        // The same boundary `inject_isolation_flags` puts on argv, restated in the file. Belt and
        // braces on purpose: the flag is the one that survives if this file fails to arm, and the
        // file is the one that survives an operator template that pins its own `--disallowedTools`.
        "permissions": { "deny": deny_rules() },
        "mcpServers": {
            "wicked-estate": {
                "command": estate_mcp_exe,
                "args": ["--db", &gov.db_path]
            }
        }
    });
    let dir = decisions_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("wicked-core-gov"));
    crate::gate_hook::create_dir_all_private(&dir)?;
    // Per-unit settings file. Written with `create_new` (O_EXCL) so a local attacker who predicts the
    // deterministic temp path can't pre-place a symlink and redirect the write (council [6] TOCTOU). On a
    // clash we UNLINK the existing entry (removing a symlink itself, not its target) and re-create fresh
    // with O_EXCL — tolerating a legitimate re-arm without ever writing through a pre-placed symlink.
    let settings_path = dir.join(format!("settings-{phase}.json"));
    let bytes = serde_json::to_vec(&settings).map_err(std::io::Error::other)?;
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&settings_path)
            .or_else(|e| {
                // On a clash, UNLINK the existing entry (remove_file removes a SYMLINK itself, never its
                // target) then re-create fresh with O_EXCL. A truncate-reopen would FOLLOW a pre-placed
                // symlink and overwrite an arbitrary file (gemini/Copilot security-critical); unlink+
                // create_new tolerates a legitimate re-arm without ever writing through a symlink.
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    let _ = std::fs::remove_file(&settings_path);
                    std::fs::OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(&settings_path)
                } else {
                    Err(e)
                }
            })?;
        f.write_all(&bytes)?;
    }
    // Write the ARMED marker BEFORE the CLI runs: its presence lets the actor-side fold distinguish a
    // governed unit that legitimately made no tool-calls (marker only) from one whose evidence was erased
    // or whose hook never fired (marker absent → fail closed). Closes the council evidence-integrity blocker.
    crate::gate_hook::write_armed_marker(&decisions_path, &phase)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    // Insert `--settings <path>` right after the binary so it parses as a flag (never demoted past the
    // prompt / a `--` guard).
    argv.insert(1, settings_path.to_string_lossy().into_owned());
    argv.insert(1, "--settings".to_string());
    Ok(GovLaunch {
        decisions_path,
        db_path: gov.db_path.clone(),
        scope,
        phase,
        phase_id: input.unit.phase_id().unwrap_or_default().to_string(),
    })
}

/// Resolve a CLI key to its headless invocation template. Reads the council registry (built-ins +
/// the user's `~/.config/wicked-council/clis.toml`); if the key isn't registered, treats the key
/// itself as the binary (`<key> {PROMPT}`) so an ad-hoc binary still runs.
pub(crate) fn resolve_invocation(cli_key: &str) -> String {
    let user =
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/wicked-council/clis.toml"));
    if let Ok(clis) = wicked_council::registry::load(user.as_deref()) {
        if let Some(c) = clis.iter().find(|c| c.key == cli_key) {
            if !c.headless_invocation.trim().is_empty() {
                return c.headless_invocation.clone();
            }
        }
    }
    format!("{cli_key} {{PROMPT}}")
}

const TIMED_OUT: &str = "__wicked_timed_out__";

/// The outcome of a bounded run: `(exit_code, stdout, stderr, usage, files)`.
type BoundedRun = (i32, String, String, Option<Usage>, Vec<String>);

/// Run `cmd` bounded by `timeout`, draining stdout+stderr CONCURRENTLY (no pipe-buffer deadlock). Each
/// raw stdout line is routed through `adapter`, whose READABLE text deltas are streamed through `emit`
/// (live output) exactly as raw lines were before (for passthrough) while its structured signals (usage,
/// files) are accumulated. Returns `(exit_code, stdout, stderr, usage, files)`; a timeout returns
/// `(-1, "", TIMED_OUT, None, [])` after killing. Uses a scoped thread so the stdout drain can borrow
/// `emit` (which lives on the worker stack); the adapter is MOVED into that thread.
fn run_bounded(
    mut cmd: Command,
    timeout: Duration,
    emit: &DeltaSink,
    mut adapter: Box<dyn OutputAdapter>,
) -> std::io::Result<BoundedRun> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let so = child.stdout.take().expect("piped stdout");
    let se = child.stderr.take().expect("piped stderr");
    let child_ref = &mut child;

    // Cap the ACCUMULATED buffers so a runaway/verbose CLI can't OOM the orchestrator. Streaming via
    // `emit` is unaffected (every delta still streams); only the retained string is bounded.
    const MAX_OUT: usize = 8 * 1024 * 1024;

    let (code, timed_out, out, usage, files, err) = std::thread::scope(|scope| {
        // Stdout: read line-by-line, route through `adapter`, stream each readable delta through `emit`,
        // accumulate the readable text (bounded) + the structured signals (usage/files).
        let out_h = scope.spawn(move || {
            use std::io::BufRead;
            let mut s = String::new();
            let mut capped = false;
            let mut usage: Option<Usage> = None;
            let mut files: Vec<String> = Vec::new();
            let mut absorb = |ao: AdapterOut, s: &mut String, capped: &mut bool| {
                for t in ao.text {
                    emit(&t);
                    if s.len() < MAX_OUT {
                        s.push_str(&t);
                        s.push('\n');
                    } else if !*capped {
                        s.push_str("\n… (output truncated)\n");
                        *capped = true;
                    }
                }
                if ao.usage.is_some() {
                    usage = ao.usage;
                }
                files.extend(ao.files);
            };
            for line in std::io::BufReader::new(so).lines().map_while(Result::ok) {
                let ao = adapter.on_line(&line);
                absorb(ao, &mut s, &mut capped);
            }
            let fin = adapter.finish();
            absorb(fin, &mut s, &mut capped);
            (s, usage, files)
        });
        let err_h = scope.spawn(move || {
            let mut s = String::new();
            // Bounded read so a verbose stderr can't OOM either.
            let _ = se.take(MAX_OUT as u64).read_to_string(&mut s);
            s
        });

        let start = Instant::now();
        let (code, timed_out) = loop {
            match child_ref.try_wait() {
                Ok(Some(status)) => break (status.code().unwrap_or(-1), false),
                Ok(None) => {
                    if start.elapsed() > timeout {
                        let _ = child_ref.kill();
                        let _ = child_ref.wait();
                        break (-1, true);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break (-1, false),
            }
        };
        let (out, usage, files) = out_h.join().unwrap_or_default();
        let err = err_h.join().unwrap_or_default();
        (code, timed_out, out, usage, files, err)
    });

    if timed_out {
        // Preserve what the CLI produced before the kill (debugging context on a hang).
        Ok((-1, out, TIMED_OUT.to_string(), usage, files))
    } else {
        Ok((code, out, err, usage, files))
    }
}

// ── argv building (ported from the proven UI logic — no shell, `--` guard) ──────────────────────

/// Whitespace tokenizer that keeps double-quoted spans together and strips the quotes.
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut any = false;
    for ch in s.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                any = true;
            }
            c if c.is_whitespace() && !in_quote => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

/// Build a no-shell argv from an invocation template, placing the untrusted `prompt` as its OWN argv
/// element. A POSIX `--` end-of-options guard is inserted before a positional prompt so a flag-shaped
/// prompt can't smuggle a flag; when `{PROMPT}` is a flag's value (preceding token is an option) the
/// binding is preserved and no `--` is added.
/// The prompt for a unit's CLI invocation. When the unit is **skill-driven** (DES-EXEC-001 §4.1), the
/// prompt LEADS with the leading-slash form `/{skill_ref} {description}` so the harness expands the
/// named skill (spike-verified for `claude` **given the skill is installed** in `~/.claude/skills/` —
/// see brain `headless-skill-invocation-recipe`); otherwise it's the bare description (authored path).
///
/// LIMITATION (Law-2): `/{skill}` is the **Claude-Code** slash-command form. Other CLIs express "run
/// this skill" differently, so the per-CLI skill form should become template data (like `{SKILLS}`)
/// rather than this hard-coded prefix — tracked as a follow-up. Today only the claude form is grounded.
/// Pure + testable without a subprocess.
///
/// The runtime skill allowlist (`unit.allowed_skills`, §4.2) rides the invocation template via a
/// `{SKILLS}` placeholder — see [`build_argv`]. The template author picks the per-CLI flag (e.g.
/// `claude … --allowedTools {SKILLS}`), so the engine never hard-codes one CLI's semantics.
/// Real work units additionally carry the structured-assumptions conventions appendix
/// ([`crate::assumptions::PROMPT_CONVENTION`]); engine-internal `validator`/`triage`
/// sessions return the authored prompt byte-exact.
pub(crate) fn skill_prompt(unit: &WorkUnit) -> String {
    let base = match unit.skill_ref.as_deref() {
        Some(skill) if !skill.is_empty() => {
            // NOT a slash line: plugin SKILLS are not slash commands — a "/name" prompt hits the
            // CLI's command parser and dies as "Unknown command" in ANY name form (core#126,
            // probed live both ways). The grounded mechanic is the Skill tool: instruct the
            // session to invoke the named skill and do the unit's work under it.
            format!(
                "Invoke your skill \"{}\" (via the Skill tool) and complete this task under its instructions: {}",
                plugin_skill_invocation(skill),
                unit.description
            )
        }
        _ => unit.description.clone(),
    };
    // Engine-internal judge/triage prompts are fully authored — no conventions appendix
    // (their verdict contracts must stay byte-exact).
    if matches!(unit.session_id.as_str(), "validator" | "triage") {
        return base;
    }
    format!("{base}{}", crate::assumptions::PROMPT_CONVENTION)
}

/// Map a dash-form `skill_ref` onto the CLI's invocable name. Claude Code invokes PLUGIN skills
/// as `/plugin:skill` — a dash-form ref like `wicked-garden-domain-extractor` is literally
/// "Unknown command" to it (core#126: three no-op units, caught only by the coverage validator).
/// Refs under a known wicked plugin family are rewritten `wicked-<plugin>-<skill>` →
/// `wicked-<plugin>:<skill>`; anything else passes through untouched.
pub(crate) fn plugin_skill_invocation(skill_ref: &str) -> String {
    for plugin in ["wicked-garden", "wicked-testing", "wicked-brain"] {
        if let Some(rest) = skill_ref.strip_prefix(&format!("{plugin}-")) {
            if !rest.is_empty() {
                return format!("{plugin}:{rest}");
            }
        }
    }
    skill_ref.to_string()
}

/// Build the argv from an invocation template, substituting `{PROMPT}` (the skill-led prompt, guarded
/// as its own arg) and `{SKILLS}` (the runtime allowlist, §4.2).
///
/// The allowlist rides a **glued** token — e.g. `--allowedTools={SKILLS}`. When the allowlist is
/// non-empty the placeholder is replaced with the comma-joined skills; when EMPTY the whole token is
/// dropped (the flag disappears with no dangling empty value). The substitution is inserted **before**
/// any `--` end-of-options guard, so the flag can never be demoted to a positional arg even if the
/// template places it after `{PROMPT}`. Unlike the earlier heuristic, nothing pops a *preceding* token,
/// so an unrelated flag can never be silently deleted. (A bare `{SKILLS}` token also works — it expands
/// in place — but only the glued form elides its flag cleanly when the allowlist is empty.)
pub(crate) fn build_argv(invocation: &str, prompt: &str, skills: &[String]) -> Vec<String> {
    let toks = tokenize(invocation);
    let mut argv: Vec<String> = Vec::new();
    let mut placed = false;
    let joined = skills.join(",");
    let ensure_guard = |argv: &mut Vec<String>| {
        // A bare flag (`-p`, `--foo`) may take the prompt as its value ⇒ no guard. A GLUED flag
        // (`--foo=bar`) is self-contained ⇒ the prompt is NOT its value, so it still needs the guard.
        let prev_is_flag = argv
            .last()
            .map(|p| p.starts_with('-') && !p.contains('='))
            .unwrap_or(false);
        if !prev_is_flag && !argv.iter().any(|a| a == "--") {
            argv.push("--".to_string());
        }
    };
    // Insert a skills arg BEFORE any already-pushed `--` guard (keeps flags out of positional land).
    let insert_pre_guard =
        |argv: &mut Vec<String>, arg: String| match argv.iter().position(|a| a == "--") {
            Some(i) => argv.insert(i, arg),
            None => argv.push(arg),
        };
    for t in &toks {
        if t.contains("{SKILLS}") {
            // Empty allowlist ⇒ drop the whole token (flag + value vanish). Non-empty ⇒ substitute.
            if !skills.is_empty() {
                insert_pre_guard(&mut argv, t.replace("{SKILLS}", &joined));
            }
        } else if t == "{PROMPT}" {
            ensure_guard(&mut argv);
            argv.push(prompt.to_string());
            placed = true;
        } else if t.contains("{PROMPT}") {
            argv.push(t.replace("{PROMPT}", prompt));
            placed = true;
        } else {
            argv.push(t.clone());
        }
    }
    if !placed {
        ensure_guard(&mut argv);
        argv.push(prompt.to_string());
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn run_bounded_streams_each_stdout_line_live() {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = lines.clone();
        let emit = move |line: &str| sink.lock().unwrap().push(line.to_string());
        let mut cmd = Command::new("printf");
        cmd.arg("alpha\nbeta\ngamma\n");
        let (code, out, _err, _usage, _files) =
            run_bounded(cmd, Duration::from_secs(5), &emit, Box::new(Passthrough)).unwrap();
        assert_eq!(code, 0);
        assert_eq!(
            *lines.lock().unwrap(),
            vec!["alpha", "beta", "gamma"],
            "each stdout line is streamed through emit as it arrives"
        );
        assert!(
            out.contains("alpha") && out.contains("gamma"),
            "the full output is still accumulated alongside streaming"
        );
    }

    #[test]
    fn arm_input_governance_writes_a_pretool_settings_file_and_returns_env() {
        let mut u = WorkUnit::pending("s:u1", "s", 3, "do it");
        u.assigned_cli = Some("claude".to_string());
        let gov = crate::workflow::GovernanceContext {
            db_path: "/abs/estate.db".to_string(),
        };
        let input = StepInput {
            run_id: format!("armtest-{}", std::process::id()),
            unit_ix: 0,
            attempt: 0,
            unit: u,
            workflow_id: "wf-x".to_string(),
            entity_mode: crate::scope::EntityMode::Isolated,
            workdir: None,
            governance: Some(gov.clone()),
            prior_outputs: vec![],
        };
        let mut argv = vec!["claude".to_string(), "-p".to_string(), "hi".to_string()];
        let g = arm_input_governance(&input, &gov, &mut argv).unwrap();

        assert_eq!(g.db_path, "/abs/estate.db", "the child gets the store path");
        // scope/phase ride the RETURNED struct (→ env), pinned to the unit's real values.
        assert_eq!(g.phase, "unit-3", "phase pinned to the unit's real ord");
        assert!(
            g.scope.starts_with("wicked-agent/"),
            "scope pinned to resolve_scope: {}",
            g.scope
        );
        // `--settings <file>` inserted right after the binary (parses as a flag, before the prompt).
        assert_eq!(argv[0], "claude");
        assert_eq!(argv[1], "--settings");
        let settings_path = std::path::PathBuf::from(&argv[2]);
        assert!(settings_path.exists(), "the settings file was written");
        assert!(
            settings_path.starts_with(std::env::temp_dir()),
            "settings live OUTSIDE any worktree (no repo pollution): {settings_path:?}"
        );
        assert!(
            g.decisions_path.starts_with(std::env::temp_dir()),
            "the decisions log lives outside any worktree"
        );

        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
        assert_eq!(
            json["hooks"]["PreToolUse"][0]["matcher"], "*",
            "the hook governs EVERY tool"
        );
        let cmd = json["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            cmd.contains("gate-hook"),
            "runs the gate-hook subcommand: {cmd}"
        );
        // SECURITY: the command carries NO caller-controlled data — scope/phase go via env, not the shell
        // string. Only the (trusted, double-quoted) exe is interpolated.
        assert!(
            !cmd.contains("--scope") && !cmd.contains("--phase") && !cmd.contains("--db"),
            "no scope/phase/db interpolated into the shell-executed hook command: {cmd}"
        );
        assert!(
            !cmd.contains("wicked-agent/"),
            "the caller-controlled scope must NOT appear in the shell command: {cmd}"
        );
        let q = if cfg!(unix) { '\'' } else { '"' };
        assert!(
            cmd.trim_start().starts_with(q),
            "the exe path is quoted per-platform ({q}) so $/backtick/space can't be expanded: {cmd}"
        );
        let _ = std::fs::remove_dir_all(gov_run_dir_for_test(&input.run_id));
    }

    // The gov run dir for cleanup — mirrors gate_hook::gov_run_dir without exposing it beyond the crate.
    fn gov_run_dir_for_test(run_id: &str) -> std::path::PathBuf {
        crate::gate_hook::gov_run_dir(run_id)
    }

    #[test]
    fn skill_prompt_leads_with_the_headless_slash_form() {
        let appendix = crate::assumptions::PROMPT_CONVENTION;
        let mut u = WorkUnit::pending("s:build", "s", 1, "add SSO login");
        // authored path: no skill → bare description + the conventions appendix.
        assert_eq!(skill_prompt(&u), format!("add SSO login{appendix}"));
        // skill-driven: leads with /<skill> so the harness expands the named skill deterministically.
        u.skill_ref = Some("wicked-testing-semantic-reviewer".to_string());
        assert_eq!(
            skill_prompt(&u),
            format!("Invoke your skill \"wicked-testing:semantic-reviewer\" (via the Skill tool) and complete this task under its instructions: add SSO login{appendix}")
        );
        // an empty skill_ref is treated as no skill (authored path), never a bare "/ ...".
        u.skill_ref = Some(String::new());
        assert_eq!(skill_prompt(&u), format!("add SSO login{appendix}"));
        // Engine-internal judge/triage prompts stay byte-exact — no appendix.
        let judge = WorkUnit::pending("validator-agent", "validator", 1, "judge this");
        assert_eq!(skill_prompt(&judge), "judge this");
        let triage = WorkUnit::pending("triage-agent", "triage", 1, "triage this");
        assert_eq!(skill_prompt(&triage), "triage this");
    }

    #[test]
    fn a_skill_prompt_flows_through_build_argv_as_one_guarded_arg() {
        let mut u = WorkUnit::pending("s:build", "s", 1, "do it");
        u.skill_ref = Some("wicked-testing-plan".to_string());
        let argv = build_argv("claude -p {PROMPT}", &skill_prompt(&u), &[]);
        assert_eq!(argv.len(), 3, "one guarded prompt arg");
        assert_eq!(argv[0], "claude");
        assert_eq!(argv[1], "-p");
        assert!(
            argv[2].starts_with("Invoke your skill \"wicked-testing:plan\""),
            "the skill-led prompt binds as -p's value, one argv element (no shell, no flag smuggling)"
        );
        assert!(
            argv[2].contains("ASSUMPTION[external-transform]"),
            "the conventions appendix rides the same single arg"
        );
    }

    #[test]
    fn skills_placeholder_expands_the_glued_allowlist_flag() {
        let skills = vec![
            "wicked-testing-execution".to_string(),
            "wicked-testing-authoring".to_string(),
        ];
        let argv = build_argv(
            "claude -p --allowedTools={SKILLS} {PROMPT}",
            "do it",
            &skills,
        );
        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "-p".to_string(),
                "--allowedTools=wicked-testing-execution,wicked-testing-authoring".to_string(),
                "--".to_string(),
                "do it".to_string(),
            ],
            "the glued flag carries the comma-joined allowlist; the prompt still gets its -- guard"
        );
    }

    #[test]
    fn empty_skills_drop_the_whole_glued_flag_token() {
        // No allowlist ⇒ the entire `--allowedTools={SKILLS}` token vanishes (no dangling flag).
        let argv = build_argv("claude -p --allowedTools={SKILLS} {PROMPT}", "do it", &[]);
        assert_eq!(
            argv,
            vec!["claude".to_string(), "-p".to_string(), "do it".to_string()]
        );
    }

    #[test]
    fn skills_after_prompt_still_land_before_the_guard() {
        // Even a misordered template ({SKILLS} after {PROMPT}) must not demote the flag past `--`.
        // `run {PROMPT}` gives the prompt a `--` guard (prev token isn't a value-taking flag); the
        // later skills flag must be inserted BEFORE that guard.
        let skills = vec!["a".to_string()];
        let argv = build_argv(
            "claude run {PROMPT} --allowedTools={SKILLS}",
            "do it",
            &skills,
        );
        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "run".to_string(),
                "--allowedTools=a".to_string(),
                "--".to_string(),
                "do it".to_string(),
            ],
            "the allowlist flag is inserted before the -- guard, never left in positional territory"
        );
    }

    #[test]
    fn no_unrelated_preceding_flag_is_ever_deleted() {
        // Regression for the old pop-heuristic: an empty allowlist must NOT delete an adjacent flag
        // that isn't the allowlist flag. With the glued form there is no preceding-token pop at all.
        let argv = build_argv(
            "claude --verbose --allowedTools={SKILLS} -p {PROMPT}",
            "go",
            &[],
        );
        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "--verbose".to_string(),
                "-p".to_string(),
                "go".to_string(),
            ],
            "--verbose survives; only the glued allowlist token is dropped"
        );
    }

    #[test]
    fn prompt_is_a_guarded_standalone_arg() {
        assert_eq!(
            build_argv("echo {PROMPT}", "--help", &[]),
            vec!["echo".to_string(), "--".to_string(), "--help".to_string()]
        );
    }

    #[test]
    fn flag_value_position_keeps_binding() {
        assert_eq!(
            build_argv("claude -p {PROMPT}", "hi", &[]),
            vec!["claude".to_string(), "-p".to_string(), "hi".to_string()]
        );
    }

    #[test]
    fn unknown_cli_falls_back_to_key_as_binary() {
        // A key not in the registry becomes `<key> {PROMPT}`.
        let inv = resolve_invocation("definitely-not-a-registered-cli-xyz");
        assert_eq!(inv, "definitely-not-a-registered-cli-xyz {PROMPT}");
    }

    // ── B-runner adapters (DES-STUDIO-COCKPIT-001 §3 / §6b) ──────────────────────────────────────────

    /// Drive an adapter over a slice of raw lines the way `run_bounded`'s stdout drain does: collect the
    /// readable deltas in order, keep the LAST usage seen, and accumulate every file path.
    fn drive(adapter: &mut dyn OutputAdapter, lines: &[&str]) -> AdapterOut {
        let mut acc = AdapterOut::default();
        let mut absorb = |ao: AdapterOut| {
            acc.text.extend(ao.text);
            if ao.usage.is_some() {
                acc.usage = ao.usage;
            }
            acc.files.extend(ao.files);
        };
        for l in lines {
            absorb(adapter.on_line(l));
        }
        absorb(adapter.finish());
        acc
    }

    /// Faithful structural slice of the empirical `/tmp/cj.ndjson` capture (DES §6b): a system init, an
    /// assistant `thinking` block (no readable text), an assistant `tool_use` Read carrying `file_path`,
    /// a `rate_limit_event`, an assistant `text` block, and the terminal `result` with `usage` +
    /// `total_cost_usd`. Values (tokens 25789/83, cost 0.409099, path, text) are the measured ones.
    const CLAUDE_FIXTURE: &[&str] = &[
        r#"{"type":"system","subtype":"init","session_id":"d2a386ef-958b-4f5f-984c-3bce7238bb30"}"#,
        r#"{"type":"assistant","message":{"model":"claude-opus-4-8","role":"assistant","content":[{"type":"thinking","thinking":""}]}}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01VsZS6YPmvMhjD4TD82Lh2T","name":"Read","input":{"file_path":"/tmp/wc-probe.txt"}}]}}"#,
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"#,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"hello","total_cost_usd":0.409099,"usage":{"input_tokens":25789,"cache_creation_input_tokens":26103,"cache_read_input_tokens":34098,"output_tokens":83}}"#,
    ];

    #[test]
    fn t_d1_claude_adapter_extracts_text_usage_and_files() {
        let mut adapter = ClaudeStreamJson::default();
        let out = drive(&mut adapter, CLAUDE_FIXTURE);
        // Readable prose only — no raw JSON leaked to FR-2 live output; thinking/system/result yield none.
        assert_eq!(
            out.text,
            vec!["hello".to_string()],
            "only assistant text blocks become readable deltas"
        );
        // Usage from the terminal result: tokens + cost DIRECTLY from claude (no price table needed).
        assert_eq!(
            out.usage,
            Some(Usage {
                input_tokens: 25789,
                output_tokens: 83,
                cost_usd: Some(0.409099),
            })
        );
        // Files from the tool_use `input.file_path`.
        assert_eq!(out.files, vec!["/tmp/wc-probe.txt".to_string()]);
    }

    #[test]
    fn t_d1_claude_adapter_degrades_malformed_line_to_passthrough_no_panic() {
        let mut adapter = ClaudeStreamJson::default();
        // A non-JSON line (version drift) must degrade to a single passthrough text delta, never panic.
        let out = adapter.on_line("not json at all {oops");
        assert_eq!(out.text, vec!["not json at all {oops".to_string()]);
        assert!(out.usage.is_none());
        assert!(out.files.is_empty());
        // And a run mixing a garbage line with good lines still recovers the real usage/files.
        let mut adapter = ClaudeStreamJson::default();
        let mut lines = vec!["}{ broken"];
        lines.extend_from_slice(CLAUDE_FIXTURE);
        let out = drive(&mut adapter, &lines);
        assert_eq!(out.files, vec!["/tmp/wc-probe.txt".to_string()]);
        assert!(out.usage.is_some());
        assert!(out.text.contains(&"}{ broken".to_string()));
        assert!(out.text.contains(&"hello".to_string()));
    }

    #[test]
    fn t_d1_claude_adapter_string_content_becomes_a_text_delta() {
        // (S3a) An `assistant` message whose `content` is a bare STRING (not an array) is valid JSON, so
        // it never hit the passthrough fallback — and `as_array()` returned None, silently DROPPING the
        // text. It must now surface as one readable delta.
        let mut adapter = ClaudeStreamJson::default();
        let out = drive(
            &mut adapter,
            &[
                r#"{"type":"assistant","message":{"role":"assistant","content":"the answer is 42"}}"#,
            ],
        );
        assert_eq!(
            out.text,
            vec!["the answer is 42".to_string()],
            "string content is emitted as a text delta (no longer dropped)"
        );
    }

    #[test]
    fn t_d1_claude_result_only_answer_yields_nonempty_output() {
        // (S3b) When NO assistant text streamed (the answer arrives only in the terminal `result`
        // envelope), the `result.result` string is emitted as text so `StepOutput.output` — the artifact
        // the creator≠evaluator judge reads — is never empty (an empty artifact → spurious reject).
        let mut adapter = ClaudeStreamJson::default();
        let out = drive(
            &mut adapter,
            &[
                r#"{"type":"system","subtype":"init"}"#,
                r#"{"type":"result","subtype":"success","is_error":false,"result":"FINAL ANSWER","total_cost_usd":0.01,"usage":{"input_tokens":10,"output_tokens":5}}"#,
            ],
        );
        assert_eq!(
            out.text,
            vec!["FINAL ANSWER".to_string()],
            "the result envelope's answer becomes the output when no assistant text streamed"
        );

        // Mirror: when assistant text DID stream, the result fallback does NOT double-emit it.
        let mut adapter = ClaudeStreamJson::default();
        let out = drive(&mut adapter, CLAUDE_FIXTURE);
        assert_eq!(
            out.text,
            vec!["hello".to_string()],
            "the result fallback stays silent when assistant text already streamed (no duplicate)"
        );
    }

    #[test]
    fn t_d1_claude_result_without_usage_reports_no_usage() {
        // (M8) A `result` line with NO `usage` object must leave `usage = None` — never a fabricated
        // `Usage{0,0}` that would surface as a "$0.00, 0 tokens" CliUsage row.
        let mut adapter = ClaudeStreamJson::default();
        let out = drive(
            &mut adapter,
            &[r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#],
        );
        assert!(
            out.usage.is_none(),
            "a result without a usage object yields no usage (no zero-token CliUsage row)"
        );
        // The answer still surfaces via the S3b fallback even with no usage.
        assert_eq!(out.text, vec!["done".to_string()]);
    }

    #[test]
    fn claude_stream_flags_not_injected_when_output_format_already_set() {
        // (M6) An operator template that already sets `--output-format json` must NOT get a second,
        // conflicting `--output-format stream-json` injected (claude would error and fail the run).
        let mut argv = build_argv("claude -p --output-format json {PROMPT}", "hi", &[]);
        let before = argv.clone();
        inject_claude_stream_flags(&mut argv);
        assert_eq!(
            argv, before,
            "no stream-json flags injected when the template already sets --output-format"
        );
    }

    #[test]
    fn t_d2_passthrough_adapter_is_one_delta_per_line_no_usage_no_files() {
        let mut adapter = Passthrough;
        let out = drive(&mut adapter, &["line one", "line two", "line three"]);
        assert_eq!(
            out.text,
            vec![
                "line one".to_string(),
                "line two".to_string(),
                "line three".to_string()
            ],
            "each raw line is exactly one delta (byte-identical to pre-adapter behavior)"
        );
        assert!(out.usage.is_none(), "passthrough never reports usage");
        assert!(out.files.is_empty(), "passthrough never reports files");
    }

    #[test]
    fn claude_binary_detection_matches_stem_only() {
        assert!(binary_is_claude("claude"));
        assert!(binary_is_claude("/usr/local/bin/claude"));
        assert!(binary_is_claude("claude.exe"));
        assert!(!binary_is_claude("agy"));
        assert!(!binary_is_claude("claude-code-wrapper"));
    }

    #[test]
    fn claude_stream_flags_inject_before_the_guard() {
        // No `--` guard (the default `claude -p {PROMPT}` shape): flags append after the prompt value —
        // the empirically-verified form (`-p <prompt> --output-format stream-json --verbose`).
        let mut argv = build_argv("claude -p {PROMPT}", "hi", &[]);
        inject_claude_stream_flags(&mut argv);
        assert_eq!(
            argv,
            vec![
                "claude",
                "-p",
                "hi",
                "--output-format",
                "stream-json",
                "--verbose"
            ]
        );
        // With a `--` guard: the flags must land BEFORE it, never demoted to positional args.
        let mut argv = build_argv("claude {PROMPT}", "hi", &[]);
        inject_claude_stream_flags(&mut argv);
        assert_eq!(
            argv,
            vec![
                "claude",
                "--output-format",
                "stream-json",
                "--verbose",
                "--",
                "hi"
            ]
        );
    }

    /// Serializes the test that unsets HOME against the tests whose assertions depend on it.
    ///
    /// Environment variables are process-global and cargo runs this module's tests on many threads,
    /// so an unsynchronized `remove_var("HOME")` would intermittently strip the path rules out from
    /// under a concurrent reader — a flake that reads exactly like a real isolation defect. Every
    /// test that asserts on a HOME-DERIVED rule must take this lock; the ones that only assert which
    /// flags got injected do not, because `DENIED_BASH` is unconditional and so the deny flag is
    /// present either way. Poison-tolerant on purpose: one panicking test must not cascade.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Read the value that follows `flag` in an argv.
    fn flag_value<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
        let i = argv.iter().position(|a| a == flag)?;
        argv.get(i + 1).map(String::as_str)
    }

    /// Does `argv` carry `flag` IMMEDIATELY followed by `value`, anywhere?
    ///
    /// [`flag_value`] reads the first token matching `flag`, which is the wrong answer when the
    /// PROMPT is itself that string: the prompt lands in argv as an ordinary token, so the first
    /// match is the prompt and the "value" read back is the injected flag name. That is a property
    /// of the assertion, not of the injection — this pair form asks the question the hostile-prompt
    /// test actually means.
    fn states_pair(argv: &[String], flag: &str, value: &str) -> bool {
        argv.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    /// FINDING-047: a worker that loads the operator's user-scope settings inherits their hooks,
    /// their permission defaults, and their plugins — and behaves differently because of it. The
    /// run stops being a property of the repo and becomes a property of the laptop.
    #[test]
    fn isolation_drops_user_scope_settings_and_lands_before_the_guard() {
        let inv = "claude {PROMPT}";
        let mut argv = build_argv(inv, "hi", &[]);
        inject_isolation_flags(&mut argv, inv);

        assert_eq!(
            flag_value(&argv, "--setting-sources"),
            Some("project,local"),
            "project and local scope stay; the operator's user scope does not"
        );
        let guard = argv.iter().position(|a| a == "--").expect("the -- guard");
        let flag = argv
            .iter()
            .position(|a| a == "--setting-sources")
            .expect("the flag");
        assert!(
            flag < guard,
            "past the guard it would be read as prompt text, not as a flag: {argv:?}"
        );
    }

    /// FINDING-045: 41 of 331 path-bearing tool calls left the worktree — into the operator's
    /// brain index, their CLI config, and twice into whole-filesystem scans.
    #[test]
    fn isolation_denies_the_operator_state_the_campaign_saw_workers_reach_into() {
        // Asserts on HOME-derived rules, so it must not run while the no-home test has HOME unset.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let inv = "claude -p {PROMPT}";
        let mut argv = build_argv(inv, "hi", &[]);
        inject_isolation_flags(&mut argv, inv);

        let denied = flag_value(&argv, "--disallowedTools").expect("a deny list");
        // One argv entry, not several: `--disallowedTools` is variadic, and a bare sequence of
        // values would keep swallowing tokens up to the next flag.
        assert!(
            denied.contains(','),
            "the rules ride one comma-joined value: {denied}"
        );
        for dir in [".wicked-brain", ".claude", ".ssh"] {
            assert!(
                denied.contains(&format!("{dir}/**)")),
                "{dir} must be denied to the file tools: {denied}"
            );
        }
        assert!(
            denied.contains("Bash(find /:*)") && denied.contains("Bash(pkill:*)"),
            "the two verbs no path rule can catch, both observed in campaign transcripts: {denied}"
        );
    }

    /// Dropping user scope drops the permission mode that lived there, and a `-p` session with no
    /// mode denies its own Write calls. Verified against the live CLI: same probe, mode unset ⇒
    /// "Claude requested permissions to write to …" and no file; mode `acceptEdits` ⇒ file written.
    /// The mode must be stated or the isolation fix breaks every unit that writes anything.
    #[test]
    fn isolation_states_a_permission_mode_that_still_honours_the_deny_rules() {
        let inv = "claude -p {PROMPT}";
        let mut argv = build_argv(inv, "hi", &[]);
        inject_isolation_flags(&mut argv, inv);
        assert_eq!(
            flag_value(&argv, "--permission-mode"),
            Some("acceptEdits"),
            "`auto` and `bypassPermissions` both make --disallowedTools inert (measured); \
             acceptEdits is the only mode that lets a worker write AND stay inside the boundary"
        );
    }

    /// An operator template that pins its own scopes or deny list is making a deliberate choice.
    /// Injecting a second copy of either flag is how you get a CLI that refuses to start.
    #[test]
    fn isolation_defers_to_a_template_that_already_pins_these_flags() {
        let inv =
            "claude --setting-sources user --permission-mode plan --disallowedTools Edit -p {PROMPT}";
        let mut argv = build_argv(inv, "hi", &[]);
        let before = argv.clone();
        inject_isolation_flags(&mut argv, inv);
        assert_eq!(argv, before, "nothing injected over an explicit choice");
        assert_eq!(argv.iter().filter(|a| *a == "--setting-sources").count(), 1);
    }

    /// The `--flag=value` form defers exactly like the `--flag value` form, for EVERY flag and both
    /// spellings of the deny flag.
    ///
    /// This is a regression test with a known origin: the three guards were written out inline and
    /// the `--disallowedTools` one checked only the separate-token form, so a template written as
    /// `--disallowedTools=Edit` got a SECOND `--disallowedTools` injected beside it. Table-driven so
    /// a flag added later without an `argv_states` guard shows up here as a failure rather than as a
    /// duplicated flag in a live worker's argv.
    #[test]
    fn isolation_defers_to_the_equals_form_of_every_flag_it_would_inject() {
        for stated in [
            "--setting-sources=user",
            "--permission-mode=plan",
            "--disallowedTools=Edit",
            "--disallowed-tools=Edit",
        ] {
            let inv = format!("claude {stated} -p {{PROMPT}}");
            let mut argv = build_argv(&inv, "hi", &[]);
            inject_isolation_flags(&mut argv, &inv);
            let flag = stated.split('=').next().unwrap();
            assert!(
                !argv.iter().any(|a| a == flag),
                "`{stated}` already states this flag, but a separate `{flag}` was injected \
                 alongside it: {argv:?}"
            );
        }
    }

    /// `argv_states` must key off the whole flag name, not a prefix of it. `--permission-mode-foo`
    /// and `--setting-sourcesX` are different flags; treating them as a match would silently
    /// suppress the isolation this whole module exists to apply.
    #[test]
    fn a_longer_flag_that_merely_starts_the_same_is_not_a_match() {
        assert!(argv_states(
            &["--permission-mode=plan".to_string()],
            &["--permission-mode"]
        ));
        assert!(argv_states(
            &["--permission-mode".to_string()],
            &["--permission-mode"]
        ));
        assert!(!argv_states(
            &["--permission-mode-extra".to_string()],
            &["--permission-mode"]
        ));
        assert!(!argv_states(
            &["--permission-modes".to_string()],
            &["--permission-mode"]
        ));
    }

    /// The deny list is a `--disallowedTools` value AND a `permissions.deny` array in the settings
    /// file. A rule carrying a comma would split into two malformed rules in the first of those, so
    /// no rule may contain one.
    #[test]
    fn no_deny_rule_contains_the_character_that_joins_them() {
        let rules = deny_rules();
        assert!(!rules.is_empty());
        for r in &rules {
            assert!(!r.contains(','), "rule would split when joined: {r}");
        }
    }

    /// A PROMPT that looks like a flag must not switch the boundary off.
    ///
    /// Deference used to be decided by scanning the built argv — which contains the prompt. Prompt
    /// text is workflow- and model-authored, and `build_argv` places it as a bare token when the
    /// template ends in a value-taking flag (`-p {PROMPT}` puts it BEFORE any `--` guard), so a
    /// prompt reading exactly `--setting-sources=user` was indistinguishable from an operator
    /// pinning that flag: the injection was suppressed and the worker ran with the operator's
    /// config, their deny rules absent, on untrusted text alone. Deference now reads the template,
    /// where `{PROMPT}` is a literal placeholder and prompt content cannot appear.
    #[test]
    fn a_prompt_that_looks_like_a_flag_cannot_suppress_the_isolation() {
        for hostile in [
            "--setting-sources=user",
            "--setting-sources",
            "--permission-mode=bypassPermissions",
            "--disallowedTools=",
            "--disallowed-tools=nothing",
        ] {
            // `-p {PROMPT}` on purpose: that is the shape that leaves the prompt un-guarded.
            let inv = "claude -p {PROMPT}";
            let mut argv = build_argv(inv, hostile, &[]);
            inject_isolation_flags(&mut argv, inv);
            assert!(
                states_pair(&argv, "--setting-sources", "project,local"),
                "prompt `{hostile}` suppressed the scope isolation: {argv:?}"
            );
            assert!(
                states_pair(&argv, "--permission-mode", "acceptEdits"),
                "prompt `{hostile}` suppressed the permission mode: {argv:?}"
            );
            assert!(
                argv.windows(2)
                    .any(|w| w[0] == "--disallowedTools" && w[1].contains("Bash(sudo:*)")),
                "prompt `{hostile}` suppressed the deny rules: {argv:?}"
            );
            // The prompt is still handed to the CLI verbatim — this hardens the boundary without
            // rewriting what the worker is asked to do. Scoped claim: verbatim IN ARGV. A prompt
            // that opens with `--` may still be read as a flag by the CLI's own parser, because
            // `build_argv` omits the `--` guard when the template ends in a bare flag (`-p
            // {PROMPT}`). That is a pre-existing argv-construction question and a correctness one;
            // what this test pins is that it is no longer a SECURITY one.
            assert!(
                argv.iter().any(|a| a == hostile),
                "the prompt must still reach the worker: {argv:?}"
            );
        }
    }

    /// Losing the home directory must not take the home-INDEPENDENT rules with it.
    ///
    /// `deny_rules` used to return `None` when neither HOME nor USERPROFILE resolved, which dropped
    /// `Bash(sudo:*)` / `Bash(find /:*)` / `Bash(pkill:*)` as collateral — none of which need a home
    /// path — and did it silently, in precisely the unattended daemon environment (launchd, systemd)
    /// where an unset HOME is plausible and nobody is watching.
    ///
    /// Serialized against the other env-mutating tests in this module via [`ENV_LOCK`]; the vars are
    /// restored before the assertions run so a failure cannot leave the process without a home.
    #[test]
    fn no_home_still_denies_the_verbs_that_never_needed_one() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            ["HOME", "USERPROFILE", "CLAUDE_CONFIG_DIR"]
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect();
        for (k, _) in &saved {
            std::env::remove_var(k);
        }
        let rules = deny_rules();
        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }

        for verb in DENIED_BASH {
            assert!(
                rules.iter().any(|r| r == verb),
                "`{verb}` needs no home directory and must survive one going missing: {rules:?}"
            );
        }
        assert!(
            !rules.iter().any(|r| r.starts_with("Read(")),
            "with no home there is no path to fence, and a rule claiming otherwise would be a lie: {rules:?}"
        );
    }
}
