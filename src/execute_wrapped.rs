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
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::domain::WorkUnit;
use crate::workflow::{DeltaSink, StepInput, StepOutput, StepRunner, StepStatus, Usage};
use wicked_apps_core::HardenedCommand;

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
                    let field = |k: &str| usage.get(k).and_then(|n| n.as_u64()).unwrap_or(0);
                    // Input is fresh + cached, matching `acp_runner::parse_result_usage` — same
                    // `Usage` struct, same `CliUsage.inputTokens` on the wire, same column in the
                    // studio, so the two paths have to mean the same thing by it (FINDING-058).
                    //
                    // `usage.input_tokens` alone is the NON-CACHED input only; on any real coding
                    // turn the cache fields are the overwhelming majority of the context actually
                    // presented to the model. Counting only the fresh part understated the wrapped
                    // path by orders of magnitude while `total_cost_usd` — which does bill the
                    // cache — stayed correct, so a run showed a plausible dollar figure beside a
                    // token count too small to explain it, and the pair invited belief in both.
                    let input_tokens = field("input_tokens")
                        .saturating_add(field("cache_read_input_tokens"))
                        .saturating_add(field("cache_creation_input_tokens"));
                    let output_tokens = field("output_tokens");
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
/// Spelled out one literal path per entry, never a `~/.wicked*` glob. A glob that the CLI's matcher
/// does not interpret the way we assume denies nothing while reading like a wider boundary — and a
/// boundary you cannot verify is not one. Adding a sibling directory here is a one-line cost; getting
/// the glob subtly wrong costs the store.
const DENIED_HOME_SUBDIRS: &[&str] = &[
    ".claude", // operator CLAUDE.md, hooks, memory, plugins, transcripts
    ".wicked", // this engine's own run state, decisions logs, settings files
    // THE OPERATIONAL STORE (FINDING-067). `~/.wicked-crew/core.db` is every run, unit, phase, policy
    // and repo registration the platform has — the daemon's default state home. It was missing here
    // while `.wicked` (the same engine's *run* state) was listed, so the file tools had a clear path to
    // the one database whose loss ends the campaign. The MCP no longer hands a worker this store; this
    // is the other half — a worker that goes looking for it by path.
    ".wicked-crew",
    ".wicked-estate", // the operator's own default code graph — a DIFFERENT repo's index
    ".wicked-brain",  // an index of a DIFFERENT repo than the one under test
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

/// How `dir` is spelled inside a permission rule, or `None` when it has no faithful spelling.
///
/// Two different problems, and only the first is about Windows:
///
///  - **Separators.** A Windows path is `C:\Users\me\.claude`, and in the rule's glob syntax a
///    backslash is the ESCAPE character, not a separator. Left alone, `Read(C:\Users\me\.claude/**)`
///    unescapes to `C:Usersme.claude/**` and matches nothing — the fence would be listed in the
///    settings file, look present to anyone reading it, and deny NOTHING at runtime. So on a
///    backslash-separator OS the separators are rewritten to `/`, which the matcher accepts
///    everywhere. The rewrite is conditioned on the OS separator because on POSIX a backslash is a
///    legal filename byte: rewriting it there would fence off a DIFFERENT directory than the one
///    asked for, which is the same silent-hole failure in the other direction.
///  - **Characters with no literal spelling.** After that rewrite, a surviving backslash (POSIX only,
///    by the above) or a comma — the character `--disallowedTools` joins its list on — still cannot be
///    written into a rule. Refused rather than mangled, so the caller can warn.
///
/// Glob metacharacters (`*`, `?`, `[`) are deliberately NOT refused. A directory named `a*b` yields a
/// rule that denies MORE than intended, and on a DENY list erring wide is the safe direction; refusing
/// it would instead open the exact hole this function exists to close.
fn rule_path(dir: &Path) -> Option<String> {
    rule_path_sep(dir.to_str()?, std::path::MAIN_SEPARATOR)
}

/// [`rule_path`] with the OS separator injected, so the Windows behaviour is testable from any host.
/// A `#[cfg(windows)]` test would only ever run on one of the three CI platforms, which is exactly
/// how a separator bug survives review in the first place.
fn rule_path_sep(p: &str, os_sep: char) -> Option<String> {
    let p = if os_sep == '\\' {
        p.replace('\\', "/")
    } else {
        p.to_string()
    };
    (!p.contains('\\') && !p.contains(',')).then_some(p)
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
///  - **An individual directory cannot be expressed** — see [`rule_path`]. Skipped with a warning; the
///    remaining rules still ship. Dropping the other dozen because one path is unrepresentable would
///    trade a small hole for a total one.
///
/// This is documented as a deny-list rather than a sandbox (see [`inject_isolation_flags`]), so a
/// partial list is a real if reduced boundary. What must never happen is a gap being SILENT — an
/// operator who reads "the worker is fenced off from `~/.ssh`" needs to hear when it isn't.
///
/// The return is a plain `Vec` because it can never be empty: `DENIED_BASH` is unconditional.
///
/// The rule strings themselves are built by [`rule_path`], which is where the platform difference
/// lives.
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
        // Skip what cannot be spelled, but SAY SO — see the doc comment: the hole is acceptable,
        // hiding it is not.
        let Some(p) = rule_path(&dir) else {
            eprintln!(
                "wicked-core: worker isolation cannot express a deny rule for {} (non-UTF8, or it \
                 contains a backslash or a comma); this path is NOT fenced off from workers",
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
        let mut argv = build_argv(&invocation, &unit_prompt(input), &input.unit.allowed_skills);

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
            // A governed unit on a CLI with no gate-hook adapter. It still runs — failing it would
            // take out every `evaluator_distinct` unit, since that router exists to move the
            // evaluator OFF the creator's CLI and claude is the only governable one — but it must
            // not run quietly. `governed: false` alone cannot be told apart from a unit that never
            // asked for governance, which is exactly how this stayed invisible (FINDING-063).
            //
            // Only when there is a binary to name. An empty `argv` means no invocation is
            // configured at all: the unit fails hard below with exactly that message, and
            // reporting "governed but unenforced" for it would be a false disclosure — nothing
            // ran, so nothing ran unchecked. An event whose `cli` is `""` also can't be acted on.
            (Some(_), false) => {
                if let Some(cli) = argv.first() {
                    self.emit_event(crate::event::CoreEvent::GovernanceUnenforced {
                        session: input.run_id.clone(),
                        ord: input.unit.ord,
                        attempt: input.attempt,
                        cli: cli.clone(),
                        reason: format!(
                            "unit is governed but '{cli}' has no input-governance adapter \
                             (gate-hook injection is claude-only); its tool calls are unchecked"
                        ),
                    });
                }
                None
            }
            (None, _) => None,
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
            // No estate tool the worker spawns may inherit a store from the environment (FINDING-067).
            // Stripped UNCONDITIONALLY — governed or not, set by us or exported by whoever started the
            // daemon. `wicked-estate`, `wicked-estate-mcp` and `wicked-core` all resolve `--db` ELSE
            // this variable, so leaving it in place is the difference between a worker's `wicked-estate
            // index .` building its repo's graph and it re-indexing the platform's operational store on
            // top of itself. A boundary that depends on the daemon's environment is not a boundary.
            // Harden FIRST so the gate-hook variables set below survive as deliberate exceptions.
            cmd.hardened();
            cmd.args(&argv[1..]).current_dir(&cwd);
            // The gate-hook subprocess (spawned by claude) reads these: the append-only decisions log,
            // the absolute operational store path, and the unit's scope/phase. Scope/phase travel via
            // ENV (NOT interpolated into the shell hook command) so caller-controlled ids can never
            // inject shell metacharacters — the command string carries only the trusted exe
            // (DES-OUTGOV-003 §8).
            if let Some(g) = &gov_env {
                cmd.env(crate::gate_hook::DECISIONS_PATH_ENV, &g.decisions_path);
                cmd.env(crate::gate_hook::GATE_DB_ENV, &g.db_path);
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
///
/// The fallible half is [`resolve_wicked_core_exe_opt`]; this wrapper adds step 4. The split exists
/// because tolerance for "not found" is a per-caller decision and resolution is not: the gate-hook
/// can emit a bare name and let PATH sort it out at hook time, while the validator (FINDING-093)
/// must NOT — see the call site in `validator.rs` for why a wrong value there is worse than none.
pub(crate) fn resolve_wicked_core_exe() -> String {
    resolve_wicked_core_exe_opt().unwrap_or_else(|| "wicked-core".to_string())
}

/// Steps 1–3 of [`resolve_wicked_core_exe`]: a path we actually found, or `None`.
///
/// `None` means "this host has no locatable wicked-core", which is a fact a caller may need to act
/// on rather than paper over with a name that will fail later and elsewhere.
pub(crate) fn resolve_wicked_core_exe_opt() -> Option<String> {
    // 1. Operator override — trim so trailing whitespace/newlines don't break the hook command.
    if let Ok(v) = std::env::var("WICKED_CORE_EXE") {
        let trimmed = v.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    // 2. current_exe() — valid unless it is the Node.js interpreter.
    if let Ok(path) = std::env::current_exe() {
        if !is_node_interpreter(&path) {
            return Some(path.to_string_lossy().into_owned());
        }
    }
    // 3. PATH lookup.
    if let Ok(found) = which_binary("wicked-core") {
        return Some(found);
    }
    None
}

/// Is this path the Node.js interpreter rather than a wicked-core binary?
///
/// The whole reason `current_exe()` cannot be trusted: under napi-rs the host process IS node, so
/// `current_exe()` answers a different question than the one being asked. Extracted from the two
/// resolvers that each spelled this check out, so the predicate has one definition and a test.
pub(crate) fn is_node_interpreter(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        // Case-insensitive: Windows may report Node.exe / NODE.EXE.
        .map(|n| n.eq_ignore_ascii_case("node") || n.eq_ignore_ascii_case("node.exe"))
        .unwrap_or(false)
}

/// Locate `wicked-estate-mcp` binary for injecting estate tools into governed workers.
/// Priority: sibling of wicked-core binary → user-local install → cargo → PATH.
fn resolve_estate_mcp_exe() -> String {
    // 1. Sibling of the wicked-core binary (monorepo dev: target/release/).
    if let Ok(path) = std::env::current_exe() {
        if !is_node_interpreter(&path) {
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
/// Probe the resolved `wicked-core` CLI for the gate protocol it speaks, ONCE per process.
///
/// The launcher and the hook are two build artifacts (engine `.node` module vs. an installed CLI on
/// PATH) that must agree on a set of environment-variable names, because the injected hook command
/// carries no arguments. Nothing checked that agreement, so a stale CLI turned into a hook that
/// denied every tool call of every governed run with a message that named the wrong problem
/// (core#167).
///
/// Cached because it spawns a subprocess and the wrapped path spawns enough already.
///
/// Keyed on the binary's IDENTITY, not its path (FINDING-083). The previous comment here claimed
/// "the answer cannot change within a process, since the exe path is resolved from the same
/// environment" — true of the path, false of the binary. `cargo install` replaces the file AT THE
/// SAME PATH, so a cache keyed on the path alone keeps answering for the artifact it probed first.
///
/// That is not a stale-data nicety, it is a remedy that does not work: on a protocol mismatch the
/// operator is told to upgrade the CLI, upgrades it, and the daemon goes on denying every tool call
/// from its cached answer until someone restarts it. FINDING-066's shape — a prescribed remedy that
/// is inert — sitting on top of FINDING-081's — the artifact that runs is not the one that was
/// built.
///
/// A `stat` per lookup is the cost, against the subprocess spawn it avoids. If the metadata cannot
/// be read the binary is not cacheable AT ALL — see the guard in the body; an empty fingerprint is
/// still a stable key, so caching under it would reintroduce exactly the staleness this removes.
/// Fail toward doing the work, never toward trusting a stale answer.
fn probe_gate_protocol(exe: &str) -> Result<u32, String> {
    let key = probe_key(exe);
    // An unfingerprintable binary is NOT cacheable. `fingerprint: None` is a perfectly stable map
    // key, so storing under it would make every unreadable-metadata probe hit that one entry
    // forever — the exact staleness this change exists to remove, reintroduced through the
    // degenerate case. Review on #194 caught that my comment claimed "misses the cache and
    // re-probes" while the code cached under it. Neither read nor write when we cannot tell the
    // binary apart: pay the spawn, stay correct.
    if key.fingerprint.is_none() {
        return probe_gate_protocol_uncached(exe);
    }
    if let Ok(mut guard) = PROBED.lock() {
        if let Some(hit) = guard.as_ref().and_then(|m| m.get(&key)).cloned() {
            return hit;
        }
        let fresh = probe_gate_protocol_uncached(exe);
        guard
            .get_or_insert_with(std::collections::HashMap::new)
            .insert(key, fresh.clone());
        return fresh;
    }
    // A poisoned lock must not silently skip the handshake.
    Err(format!("gate protocol probe cache unavailable for `{exe}`"))
}

/// The subprocess probe itself — one spawn, no caching.
fn probe_gate_protocol_uncached(exe: &str) -> Result<u32, String> {
    let out = Command::new(exe)
        .hardened()
        .args(["gate-hook", "--protocol-version"])
        .output()
        .map_err(|e| format!("could not run `{exe} gate-hook --protocol-version`: {e}"))?;
    if !out.status.success() {
        // A CLI old enough to not know the flag is exactly the skew this detects; it must not
        // read as "probe unavailable, carry on".
        return Err(format!(
            "`{exe} gate-hook --protocol-version` exited {} — too old to report a protocol \
                     version, so it cannot be trusted to speak the current one",
            out.status.code().unwrap_or(-1)
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    crate::gate_hook::parse_protocol_version(&stdout).ok_or_else(|| {
        format!(
            "`{exe} gate-hook --protocol-version` printed no parseable version: {:?}",
            stdout.chars().take(200).collect::<String>()
        )
    })
}

/// What makes one probe result apply to another lookup: the path AND the file's identity.
///
/// The path alone is not enough (see `probe_gate_protocol`), and identity alone is not either —
/// `resolve_wicked_core_exe()` reads `$WICKED_CORE_EXE` every call, so a process that changes it
/// must not keep answering for the binary it probed first.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct ProbeKey {
    path: String,
    /// `None` when the metadata could not be read — an unreadable file simply misses the cache.
    fingerprint: Option<(u64, i64)>,
}

/// `(len, mtime-secs)` for `exe`. Cheap enough to pay per lookup; the alternative is trusting a
/// stale answer.
fn probe_key(exe: &str) -> ProbeKey {
    let fingerprint = std::fs::metadata(exe).ok().and_then(|m| {
        // Signed seconds, so a pre-epoch mtime stays distinct. `unwrap_or(0)` collapsed every
        // pre-epoch timestamp onto the same value, which would make two such files with equal
        // length share a key (review on #194). Rare, but the whole point here is telling binaries
        // apart.
        let t = m.modified().ok()?;
        let mtime = match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        };
        Some((m.len(), mtime))
    });
    ProbeKey {
        path: exe.to_string(),
        fingerprint,
    }
}

/// Probe results, keyed by [`ProbeKey`].
static PROBED: std::sync::Mutex<Option<std::collections::HashMap<ProbeKey, Result<u32, String>>>> =
    std::sync::Mutex::new(None);

/// Seed the probe cache, so a unit test can arm governance without a `wicked-core` CLI on PATH.
///
/// `#[cfg(test)]`, deliberately: production has no way to bypass the handshake. A runtime escape
/// hatch would be a fail-OPEN switch on the exact path core#167 exists to keep fail-closed, and
/// FINDING-063 is what that looks like in practice.
#[cfg(test)]
pub(crate) fn seed_probe_for_test(exe: &str, v: Result<u32, String>) {
    if let Ok(mut g) = PROBED.lock() {
        g.get_or_insert_with(std::collections::HashMap::new)
            .insert(probe_key(exe), v);
    }
}

/// Refuse to arm governance against a CLI that speaks a different protocol.
///
/// Returns the operator-facing reason on mismatch. Naming BOTH versions and the resolved path is the
/// point: the failure this replaces was a run whose every tool call was denied, with an error that
/// described a missing store rather than a stale binary.
///
/// Fails CLOSED — an unprobeable CLI refuses the run rather than falling through to an ungoverned
/// one, which is FINDING-063's failure mode.
fn check_gate_protocol(exe: &str) -> Result<(), String> {
    let theirs = probe_gate_protocol(exe)?;
    let ours = crate::gate_hook::GATE_PROTOCOL_VERSION;
    if theirs == ours {
        return Ok(());
    }
    Err(format!(
        "gate protocol mismatch: this engine speaks {ours}, the `wicked-core` CLI at `{exe}` speaks \
         {theirs}. They exchange every gate argument by environment variable, so a mismatch denies \
         every tool call of every governed run. Rebuild/reinstall the CLI so both come from the same \
         source tree (core#167)."
    ))
}

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
    // Refuse to arm against a CLI speaking a different gate protocol, BEFORE the run starts. The two
    // artifacts exchange every gate argument by environment variable, so skew is not a degraded run —
    // it is a run whose every tool call is denied, diagnosed as a missing store (core#167). Fails
    // closed: an unprobeable CLI refuses rather than falling through to an ungoverned run.
    if let Err(why) = check_gate_protocol(&exe) {
        return Err(std::io::Error::other(why));
    }
    // exit 2 = deny ⇒ claude aborts the tool-call; matcher "*" governs EVERY tool. Only the exe is
    // interpolated (scope/phase go via env). Quote it per-platform so a `$`/backtick in the install path
    // can't be shell-expanded (POSIX single-quote disables ALL expansion; on Windows cmd `$`/backtick are
    // not special, so double-quote for spaces — a `"` is illegal in a Windows path anyway).
    let command = quote_exe_command(&exe);
    // Include the wicked-estate MCP server so governed workers have access to the 23 estate tools
    // (graph-view, code-graph, memory recall, etc.), pointed at the run's REPO-LOCAL graph. Using the
    // resolved exe directory to find wicked-estate-mcp avoids hardcoding a PATH dependency.
    //
    // This USED to pass `gov.db_path` — the operational store — "so no separate lookup is needed".
    // That handed every governed worker a writable handle to the platform's own state, and FINDING-067
    // is what happened next: a worker told to recon its repo did the obvious thing, ran the estate
    // indexer against the store it had been given, and the indexer's delete-sweep removed all 833
    // operational nodes (`agent_session/<id>` and friends live at synthetic locations that are not
    // files, so "the file is gone" is true of every one of them). The store it wiped held zero source
    // files, so the worker got nothing out of it either.
    //
    // `None` ⇒ NO estate MCP. Falling back to `gov.db_path` is the bug; falling back to a scratch db
    // is worse than nothing (tools that answer "not found" for a repo that plainly has the symbol are
    // how an agent concludes the code does not exist — estate's own R3).
    let estate_mcp = gov.code_graph_db.as_ref().map(|graph_db| {
        serde_json::json!({
            "wicked-estate": {
                "command": resolve_estate_mcp_exe(),
                "args": ["--db", graph_db]
            }
        })
    });
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
        "mcpServers": estate_mcp.unwrap_or_else(|| serde_json::json!({}))
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
/// ([`crate::assumptions::PROMPT_CONVENTION`]) and, when the caller has a worktree to describe, a
/// one-line map of it (`layout`, from [`crate::repo::worktree_layout`] — see [`unit_prompt`]);
/// engine-internal `validator`/`triage` sessions return the authored prompt byte-exact.
pub(crate) fn skill_prompt(unit: &WorkUnit, layout: Option<&str>) -> String {
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
    // Engine-internal judge/triage prompts are fully authored — no conventions appendix, and no
    // layout either (their verdict contracts must stay byte-exact).
    if matches!(unit.session_id.as_str(), "validator" | "triage") {
        return base;
    }
    // FINDING-048: the unit knows WHAT to do and nothing about WHERE. 12 of 32 pilot sessions burned
    // turns on `cd: no such file or directory` guessing at a monorepo's shape. One line up front is
    // cheaper than the turns it saves. Placed before the conventions appendix so the appendix stays
    // the last thing the model reads — it is an output contract, and output contracts read last.
    let layout = layout
        .map(|l| format!("{LAYOUT_PREFIX}{l}"))
        .unwrap_or_default();
    format!("{base}{layout}{}", crate::assumptions::PROMPT_CONVENTION)
}

/// Frames the [`crate::repo::worktree_layout`] map inside a prompt. Single-line, like everything else
/// appended here — the PTY session runner writes prompts line-based.
const LAYOUT_PREFIX: &str =
    " ||| WORKTREE LAYOUT (the root of your working copy — every path below \
     is relative to it; `dir/ [manifest]` marks a project root, `dir/ {…}` a container of them): ";

/// The prompt for `input`'s unit, including the worktree map when there is a worktree to map.
///
/// The split exists because [`skill_prompt`] is pure and testable without a filesystem, while the map
/// is a directory read; this is the one place the two meet, so every runner (wrapped, PTY, ACP) gets
/// the same prompt from the same code rather than three chances to diverge.
pub(crate) fn unit_prompt(input: &StepInput) -> String {
    let layout = input
        .workdir
        .as_deref()
        .and_then(crate::repo::worktree_layout);
    skill_prompt(&input.unit, layout.as_deref())
}

/// Ceiling on a prompt written to a pty as a single line, with headroom under `MAX_CANON`.
///
/// A pty in canonical mode assembles input into a fixed line buffer — `MAX_CANON`, 1024 bytes on
/// Darwin and Linux. A line that reaches it is not truncated and not delivered: it is DISCARDED, and
/// the reader simply never sees a turn. Probed on this platform: a 1022-byte line round-trips, a
/// 1023-byte line never returns. There is no error and no signal, so the caller waits out its whole
/// turn timeout for output that cannot arrive.
pub(crate) const PTY_PROMPT_LIMIT: usize = 1000;

/// Below this the map cannot say anything a worker could not get from `ls`, so it is not worth
/// spending the line budget on.
const MIN_USEFUL_LAYOUT: usize = 40;

/// [`unit_prompt`] constrained to one pty-writable line.
///
/// Only the PTY runner needs this. The wrapped runner passes the prompt as an argv element and the
/// ACP runner as a JSON string field; neither goes through a terminal line discipline, so neither has
/// a length limit worth imposing.
///
/// The worktree map is the elastic part and gets whatever the task text leaves. `Err` is reserved for
/// the case no trimming can fix — a unit description that alone overruns the line — because the
/// honest outcome there is a fast, named failure rather than a turn that burns its timeout in silence
/// (which is what this path did for any description over ~509 bytes, before and after FINDING-048).
pub(crate) fn pty_unit_prompt(input: &StepInput) -> Result<String, String> {
    // `+ 1` for the newline the runner appends to submit the turn — it occupies the same buffer.
    let plain = skill_prompt(&input.unit, None);
    let overhead = plain.len() + 1;
    if overhead > PTY_PROMPT_LIMIT {
        return Err(format!(
            "prompt is {overhead} bytes and a pty turn cannot exceed {PTY_PROMPT_LIMIT}; \
             a longer line is discarded by the terminal, not truncated. Shorten unit {}'s \
             description ({} bytes) or route this unit to a non-interactive CLI.",
            input.unit.ord,
            input.unit.description.len(),
        ));
    }
    // Everything the map could cost beyond its own budget: the heading, plus the truncation marker
    // `worktree_layout_within` may append after spending the budget.
    let framing = LAYOUT_PREFIX.len() + crate::repo::LAYOUT_TRUNCATED.len();
    let room = PTY_PROMPT_LIMIT - overhead;
    let layout = room
        .checked_sub(framing)
        .filter(|budget| *budget >= MIN_USEFUL_LAYOUT)
        .and_then(|budget| {
            input
                .workdir
                .as_deref()
                .and_then(|d| crate::repo::worktree_layout_within(d, budget))
        });
    Ok(skill_prompt(&input.unit, layout.as_deref()))
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

    /// Why the two emptiness guards on this path (`argv.is_empty()` before spawning, and the
    /// `argv.first()` guard on the FINDING-063 disclosure event) are DEFENSIVE and not live: the
    /// `!placed` fallback at the end of `build_argv` unconditionally pushes the prompt, so there is
    /// no invocation string — empty, whitespace, or one whose only token elides — that yields an
    /// empty argv. Pinned rather than assumed: if that fallback is ever made conditional, an empty
    /// argv becomes reachable, and both guards start carrying real weight instead of documenting a
    /// state that cannot occur.
    #[test]
    fn build_argv_never_yields_an_empty_argv() {
        for inv in ["", "   ", "\t", "{SKILLS}", "--allowedTools={SKILLS}"] {
            let argv = build_argv(inv, "the prompt", &[]);
            assert!(
                !argv.is_empty(),
                "build_argv({inv:?}) returned an empty argv; the emptiness guards on the wrapped \
                 path are no longer merely defensive"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_bounded_streams_each_stdout_line_live() {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = lines.clone();
        let emit = move |line: &str| sink.lock().unwrap().push(line.to_string());
        // spawn-audit: test-only — `printf` fixture proving run_bounded emits each stdout line live.
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

    /// FINDING-063. A governed unit routed to a non-claude CLI cannot be armed — gate-hook
    /// injection rides `--settings`, which only claude reads — so it runs with its tool calls
    /// unchecked. That is a deliberate trade (failing it would take out every `evaluator_distinct`
    /// unit), but it must not be SILENT: `governed: false` is also what an ungoverned-by-design
    /// unit reports, and the actor keys the ARMED-marker fail-closed fold off exactly that flag
    /// (actor.rs), so an unenforced unit slips past the erasure check too. The event is the only
    /// thing that distinguishes "not asked for" from "asked for and not applied".
    #[cfg(unix)]
    #[test]
    fn a_governed_unit_on_a_cli_that_cannot_be_armed_says_so_instead_of_going_quiet() {
        let (tx, rx) = std::sync::mpsc::channel();
        let runner = WrappedCliStepRunner::with_tx(tx);

        let mut u = WorkUnit::pending("s:u4", "s", 4, "verify the work");
        // Exactly the shape `evaluator_distinct` produces: the unit is governed, and the router has
        // moved it off claude precisely BECAUSE the creator was claude.
        u.assigned_cli = Some("agy".to_string());
        u.assigned_invocation = Some("/bin/echo {PROMPT}".to_string());
        let dir = std::env::temp_dir().join(format!(
            "wicked-unenf-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let input = StepInput {
            run_id: "run-unenf".to_string(),
            unit_ix: 0,
            attempt: 0,
            unit: u,
            workflow_id: "wf-x".to_string(),
            entity_mode: crate::scope::EntityMode::Shared,
            workdir: Some(dir.clone()),
            governance: Some(crate::workflow::GovernanceContext {
                db_path: dir.join("estate.db").to_string_lossy().to_string(),
                code_graph_db: None,
            }),
            prior_outputs: vec![],
        };

        let out = runner.run_unit(&input);

        let unenforced = rx
            .try_iter()
            .filter_map(|c| match c {
                crate::command::Command::EmitEvent(
                    crate::event::CoreEvent::GovernanceUnenforced {
                        cli, reason, ord, ..
                    },
                ) => Some((cli, reason, ord)),
                _ => None,
            })
            .next()
            .expect("a governed unit that could not be armed must announce it");
        assert_eq!(
            unenforced.0, "/bin/echo",
            "names the binary, not the seat key"
        );
        assert_eq!(unenforced.2, 4);
        assert!(
            unenforced.1.contains("claude-only"),
            "the reason must say WHY it could not be armed: {}",
            unenforced.1
        );
        // The unit still runs — this is a disclosure, not a fallback or a failure.
        assert_eq!(out.status, StepStatus::Ok);
        assert!(!out.governed, "and it is honestly reported as ungoverned");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The converse, so the event cannot degrade into noise on every ungoverned internal call:
    /// a unit that never asked for governance says nothing.
    #[cfg(unix)]
    #[test]
    fn a_unit_that_never_asked_for_governance_stays_silent() {
        let (tx, rx) = std::sync::mpsc::channel();
        let runner = WrappedCliStepRunner::with_tx(tx);
        let mut u = WorkUnit::pending("s:u1", "s", 1, "just run");
        u.assigned_cli = Some("agy".to_string());
        u.assigned_invocation = Some("/bin/echo {PROMPT}".to_string());
        let dir = std::env::temp_dir().join(format!(
            "wicked-nogov-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let input = StepInput {
            run_id: "run-nogov".to_string(),
            unit_ix: 0,
            attempt: 0,
            unit: u,
            workflow_id: "wf-x".to_string(),
            entity_mode: crate::scope::EntityMode::Shared,
            workdir: Some(dir.clone()),
            governance: None,
            prior_outputs: vec![],
        };
        let _ = runner.run_unit(&input);
        assert!(
            !rx.try_iter().any(|c| matches!(
                c,
                crate::command::Command::EmitEvent(
                    crate::event::CoreEvent::GovernanceUnenforced { .. }
                )
            )),
            "ungoverned-by-design must not be reported as unenforced governance"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#167's stated test — through `check_gate_protocol`, not a hand-built string.
    ///
    /// A REAL file to stand in for an installed CLI.
    ///
    /// The probe cache is keyed on the binary's identity (FINDING-083), and an unfingerprintable
    /// path is deliberately never served from cache — so a fictional `/fixture/...` literal cannot
    /// be seeded any more. That is the guard working, not a test inconvenience: seeding an answer
    /// for a path with no file was always pretending. These fixtures are real files, which also
    /// makes the tests faithful to what `check_gate_protocol` actually receives.
    fn fixture_exe(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!("wc_fixture_{}_{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("wicked-core");
        std::fs::write(&p, tag.as_bytes()).unwrap();
        p.to_str().unwrap().to_string()
    }

    /// The earlier version formatted its own message and asserted on that, so it could not fail if
    /// the real refusal stopped naming the versions, or if the comparison logic broke. It tested the
    /// fixture. This seeds the cache for a specific exe and calls the function the launcher calls.
    #[test]
    fn a_skewed_cli_refuses_to_arm_and_names_both_versions() {
        let ours = crate::gate_hook::GATE_PROTOCOL_VERSION;
        let theirs = ours + 98;
        let exe = fixture_exe("skewed");
        seed_probe_for_test(&exe, Ok(theirs));

        let err = check_gate_protocol(&exe).expect_err("a skewed CLI must refuse to arm");
        assert!(
            err.contains(&ours.to_string()),
            "must name the engine's version: {err}"
        );
        assert!(
            err.contains(&theirs.to_string()),
            "must name the CLI's version: {err}"
        );
        assert!(err.contains(&exe), "must name the resolved path: {err}");
    }

    /// A matching CLI arms. Without this the test above passes for a `check` that refuses everything.
    #[test]
    fn a_matching_cli_is_allowed_to_arm() {
        let exe = fixture_exe("matching");
        seed_probe_for_test(&exe, Ok(crate::gate_hook::GATE_PROTOCOL_VERSION));
        assert!(check_gate_protocol(&exe).is_ok());
    }

    /// A CLI that cannot be probed refuses the run rather than arming an ungoverned one — the
    /// FINDING-063 shape. The earlier version asserted that an `Err` was an error, which is true of
    /// every `Err` ever constructed and told us nothing about `check_gate_protocol`.
    #[test]
    fn an_unprobeable_cli_refuses_rather_than_arming_ungoverned() {
        let exe = fixture_exe("unprobeable");
        seed_probe_for_test(&exe, Err("could not run `wicked-core`: not found".into()));

        let err = check_gate_protocol(&exe).expect_err("an unprobeable CLI must not arm");
        assert!(
            err.contains("could not run"),
            "the cause must survive: {err}"
        );
    }

    /// FINDING-083: the probe cache used to be keyed on the exe PATH alone, and its comment
    /// asserted "the answer cannot change within a process". True of the path, false of the
    /// binary: `cargo install` replaces the file in place. The operator is told to upgrade the
    /// CLI, does, and the daemon keeps denying from its cached answer until a restart.
    #[test]
    fn replacing_the_binary_in_place_invalidates_the_probe() {
        let dir = std::env::temp_dir().join(format!("probe_id_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("wicked-core-probe-fixture");
        std::fs::write(&exe, b"v1").unwrap();
        let path = exe.to_str().unwrap();

        let before = probe_key(path);

        // The stale answer an operator would be stuck behind.
        seed_probe_for_test(path, Err("too old to report a protocol version".into()));
        assert!(
            probe_gate_protocol(path).is_err(),
            "precondition: the cache should be serving the stale answer"
        );

        // The upgrade: same path, different bytes. `len` alone moves here, and mtime backs it up
        // for a same-size replacement.
        std::fs::write(&exe, b"v2-upgraded-binary").unwrap();

        assert_ne!(
            probe_key(path),
            before,
            "the key must move when the file at that path is replaced"
        );
        // The cache must now MISS rather than return the seeded staleness. It re-probes, and the
        // fixture is not a real CLI, so the error names the spawn — not the seeded message.
        let after = probe_gate_protocol(path);
        assert!(
            after.is_err(),
            "a non-executable fixture cannot report a version"
        );
        assert!(
            !format!("{after:?}").contains("too old to report"),
            "still serving the pre-upgrade answer after the binary changed: {after:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unfingerprintable path must never be SERVED from the cache. `fingerprint: None` is a
    /// stable map key, so a seeded answer under it would be returned forever — the staleness this
    /// change removes, reintroduced through the degenerate case. Review on #194 caught the code
    /// doing exactly that while the comment claimed otherwise.
    #[test]
    fn a_binary_whose_metadata_cannot_be_read_is_never_served_from_cache() {
        let missing = std::env::temp_dir()
            .join(format!("probe_absent_{}", std::process::id()))
            .join("wicked-core-does-not-exist");
        let path = missing.to_str().unwrap();
        assert!(
            probe_key(path).fingerprint.is_none(),
            "precondition: this path must not be stattable"
        );

        // Seed a PASS under the degenerate key. If the cache is consulted, this is what comes back.
        seed_probe_for_test(path, Ok(crate::gate_hook::GATE_PROTOCOL_VERSION));

        let got = probe_gate_protocol(path);
        assert!(
            got.is_err(),
            "a nonexistent binary cannot report a protocol version, so the cached PASS must not be \
             served: {got:?}"
        );
    }

    /// The other half: an UNCHANGED binary must still hit the cache, or the fix trades a stale
    /// answer for a subprocess spawn on every tool call of every governed run.
    #[test]
    fn an_unchanged_binary_still_hits_the_cache() {
        let dir = std::env::temp_dir().join(format!("probe_hit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("wicked-core-probe-stable");
        std::fs::write(&exe, b"stable").unwrap();
        let path = exe.to_str().unwrap();

        seed_probe_for_test(path, Ok(crate::gate_hook::GATE_PROTOCOL_VERSION));
        assert_eq!(
            probe_gate_protocol(path),
            Ok(crate::gate_hook::GATE_PROTOCOL_VERSION),
            "an untouched binary must not be re-probed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cache is keyed by PATH. `resolve_wicked_core_exe()` re-reads `$WICKED_CORE_EXE` on every
    /// call, so one global slot would keep answering for whichever binary was probed first.
    #[test]
    fn two_exes_get_two_answers() {
        let good = fixture_exe("two_good");
        let bad = fixture_exe("two_bad");
        seed_probe_for_test(&good, Ok(crate::gate_hook::GATE_PROTOCOL_VERSION));
        seed_probe_for_test(&bad, Ok(crate::gate_hook::GATE_PROTOCOL_VERSION + 7));
        assert!(check_gate_protocol(&good).is_ok());
        assert!(
            check_gate_protocol(&bad).is_err(),
            "the second exe reused the first's answer"
        );
    }

    #[test]
    fn arm_input_governance_writes_a_pretool_settings_file_and_returns_env() {
        // Governance now refuses to arm against a CLI speaking another protocol (core#167).
        // This test is about what arming WRITES, so give it a matching CLI.
        seed_probe_for_test(
            &resolve_wicked_core_exe(),
            Ok(crate::gate_hook::GATE_PROTOCOL_VERSION),
        );
        let mut u = WorkUnit::pending("s:u1", "s", 3, "do it");
        u.assigned_cli = Some("claude".to_string());
        let gov = crate::workflow::GovernanceContext {
            db_path: "/abs/estate.db".to_string(),
            code_graph_db: Some("/abs/repo/.codegraph/estate.db".to_string()),
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

    /// FINDING-067, the channel the settings file does not cover. The gate-hook is a GRANDCHILD of the
    /// worker, so its store path can only reach it through the worker's own environment — and the
    /// variable it used was `WICKED_ESTATE_DB`, the exact name every estate binary resolves as its
    /// `--db` fallback. A worker running a bare `wicked-estate index .` in a Bash call therefore
    /// indexed the platform's operational store, and the indexer's delete-sweep removed every node in
    /// it. Fixing the MCP's `--db` alone leaves this path wide open, because it is not the MCP.
    ///
    /// The strip is unconditional, so this exercises the case that survives a rename: the variable is
    /// already exported in the environment the daemon was started with, and the launcher never sets it.
    #[cfg(unix)]
    #[test]
    fn no_worker_inherits_an_estate_store_through_the_environment() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("wicked-envstrip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let probe = dir.join("probe.sh");
        std::fs::write(&probe, "echo \"SEEN=[${WICKED_ESTATE_DB:-UNSET}]\"\n").unwrap();

        // The daemon's own environment names the operational store — the launcher must strip it
        // rather than merely decline to add it.
        let op_db = dir.join("operational-core.db");
        std::env::set_var(crate::gate_hook::ESTATE_DB_ENV, &op_db);

        let mut u = WorkUnit::pending("s:u1", "s", 1, "do it");
        u.assigned_cli = Some("probe".to_string());
        u.assigned_invocation = Some(format!("/bin/sh {} {{PROMPT}}", probe.display()));
        let input = StepInput {
            run_id: "run-envstrip".to_string(),
            unit_ix: 0,
            attempt: 0,
            unit: u,
            workflow_id: "wf-x".to_string(),
            entity_mode: crate::scope::EntityMode::Shared,
            workdir: Some(dir.clone()),
            governance: None,
            prior_outputs: vec![],
        };
        let out = WrappedCliStepRunner::default().run_unit(&input);
        std::env::remove_var(crate::gate_hook::ESTATE_DB_ENV);

        assert!(
            out.output.contains("SEEN=[UNSET]"),
            "the worker must not inherit an estate store from the environment; got: {}",
            out.output
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDING-067. The settings file is the ONE place the worker's own tools are handed a store, and
    /// the store it must never be handed is the operational one — the launcher used to pass exactly
    /// that, and a worker that ran the estate indexer against it deleted all 833 operational nodes.
    ///
    /// Two halves, and the second is the one that decays: with a repo the MCP points at the repo-local
    /// graph, and with NO repo there is no MCP at all. A future "sensible default" that falls back to
    /// `gov.db_path` when `code_graph_db` is `None` re-opens the whole hole while still looking correct
    /// in the happy path — so the `None` case asserts on the whole serialized file, not just the args.
    #[test]
    fn the_worker_mcp_never_receives_the_operational_store() {
        // Governance now refuses to arm against a CLI speaking another protocol (core#167).
        // This test is about what arming WRITES, so give it a matching CLI.
        seed_probe_for_test(
            &resolve_wicked_core_exe(),
            Ok(crate::gate_hook::GATE_PROTOCOL_VERSION),
        );
        let read_settings = |gov: &crate::workflow::GovernanceContext, run_id: &str| {
            let mut u = WorkUnit::pending("s:u1", "s", 3, "do it");
            u.assigned_cli = Some("claude".to_string());
            let input = StepInput {
                run_id: run_id.to_string(),
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
            arm_input_governance(&input, gov, &mut argv).unwrap();
            let raw = std::fs::read(std::path::PathBuf::from(&argv[2])).unwrap();
            let _ = std::fs::remove_dir_all(gov_run_dir_for_test(run_id));
            (
                serde_json::from_slice::<serde_json::Value>(&raw).unwrap(),
                String::from_utf8(raw).unwrap(),
            )
        };

        let op_db = "/abs/operational-core.db";
        let graph_db = "/abs/repo/.codegraph/estate.db";

        // WITH a registered repo: the estate MCP is armed, pointed at the REPO-LOCAL graph.
        let (json, raw) = read_settings(
            &crate::workflow::GovernanceContext {
                db_path: op_db.to_string(),
                code_graph_db: Some(graph_db.to_string()),
            },
            &format!("mcptest-repo-{}", std::process::id()),
        );
        let args = json["mcpServers"]["wicked-estate"]["args"]
            .as_array()
            .expect("the estate MCP is armed when a repo-local graph exists");
        assert_eq!(args[0], "--db");
        assert_eq!(args[1], graph_db, "the MCP opens the repo-local graph");
        assert!(
            !raw.contains(op_db),
            "the operational store must not appear ANYWHERE in the worker's settings: {raw}"
        );

        // WITHOUT one: NO estate MCP. Not the operational store, not a scratch db beside it.
        let (json, raw) = read_settings(
            &crate::workflow::GovernanceContext {
                db_path: op_db.to_string(),
                code_graph_db: None,
            },
            &format!("mcptest-norepo-{}", std::process::id()),
        );
        assert!(
            json["mcpServers"]["wicked-estate"].is_null(),
            "no repo-local graph ⇒ no estate MCP: {json}"
        );
        assert!(
            !raw.contains(op_db),
            "the operational store must not be substituted when no graph is known: {raw}"
        );
        // The hook still arms — this is a scoping fix, not a governance downgrade.
        assert_eq!(json["hooks"]["PreToolUse"][0]["matcher"], "*");
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
        assert_eq!(skill_prompt(&u, None), format!("add SSO login{appendix}"));
        // skill-driven: leads with /<skill> so the harness expands the named skill deterministically.
        u.skill_ref = Some("wicked-testing-semantic-reviewer".to_string());
        assert_eq!(
            skill_prompt(&u, None),
            format!("Invoke your skill \"wicked-testing:semantic-reviewer\" (via the Skill tool) and complete this task under its instructions: add SSO login{appendix}")
        );
        // an empty skill_ref is treated as no skill (authored path), never a bare "/ ...".
        u.skill_ref = Some(String::new());
        assert_eq!(skill_prompt(&u, None), format!("add SSO login{appendix}"));
        // Engine-internal judge/triage prompts stay byte-exact — no appendix.
        let judge = WorkUnit::pending("validator-agent", "validator", 1, "judge this");
        assert_eq!(skill_prompt(&judge, None), "judge this");
        let triage = WorkUnit::pending("triage-agent", "triage", 1, "triage this");
        assert_eq!(skill_prompt(&triage, None), "triage this");
    }

    /// FINDING-048. Three things have to hold at once for the map to be worth carrying: a real unit
    /// gets it, an engine-internal judge/triage prompt still does NOT (its verdict contract is
    /// byte-exact), and the appendix stays last so the output contract is the final instruction read.
    #[test]
    fn the_worktree_map_reaches_work_units_and_no_engine_internal_prompt() {
        let appendix = crate::assumptions::PROMPT_CONVENTION;
        let map = "src/ [Cargo.toml]; root files: README.md";
        let u = WorkUnit::pending("s:build", "s", 1, "add SSO login");

        let with = skill_prompt(&u, Some(map));
        assert_eq!(
            with,
            format!("add SSO login{LAYOUT_PREFIX}{map}{appendix}"),
            "the map sits between the task and the appendix"
        );
        assert!(!with.contains('\n'), "prompts stay single-line: {with}");
        // No worktree ⇒ the prompt is byte-identical to the pre-FINDING-048 one. A caller with
        // nothing to say must say nothing, not print an empty heading.
        assert_eq!(skill_prompt(&u, None), format!("add SSO login{appendix}"));

        for internal in ["validator", "triage"] {
            let unit = WorkUnit::pending("agent", internal, 1, "judge this");
            assert_eq!(
                skill_prompt(&unit, Some(map)),
                "judge this",
                "{internal} prompts are authored end to end — a map would corrupt the verdict contract"
            );
        }
    }

    /// The seam between the pure prompt builder and the filesystem: `unit_prompt` must actually read
    /// the workdir it is given, and must degrade to the plain prompt when there is no workdir (or the
    /// path does not exist) rather than failing the unit.
    #[test]
    fn unit_prompt_reads_the_workdir_and_tolerates_its_absence() {
        let root = std::env::temp_dir().join(format!("wicked-unitprompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("backend")).unwrap();
        std::fs::write(root.join("backend").join("pyproject.toml"), "").unwrap();

        let unit = WorkUnit::pending("s:build", "s", 1, "fix the API");
        let mut input = StepInput {
            run_id: "layout-seam".to_string(),
            unit_ix: 0,
            attempt: 0,
            unit: unit.clone(),
            workflow_id: "wf-x".to_string(),
            entity_mode: crate::scope::EntityMode::Isolated,
            workdir: Some(root.clone()),
            governance: None,
            prior_outputs: vec![],
        };
        let seen = unit_prompt(&input);
        assert!(
            seen.contains("backend/ [pyproject.toml]"),
            "the real tree must reach the prompt: {seen}"
        );

        input.workdir = None;
        assert_eq!(unit_prompt(&input), skill_prompt(&unit, None));
        input.workdir = Some(root.join("gone"));
        assert_eq!(
            unit_prompt(&input),
            skill_prompt(&unit, None),
            "a workdir that is not there degrades to the plain prompt — it never fails the unit"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A pty turn is submitted as one line, and a canonical-mode terminal DISCARDS any line that
    /// reaches `MAX_CANON` (1024) — it does not truncate it and does not report anything, so the
    /// runner waits out its full timeout for output that can never come. Two `session_runner` tests
    /// hung for 80 minutes on exactly this before the limit existed: with a workdir set, the map put
    /// the prompt 854 bytes over the line even with an empty description.
    #[test]
    fn a_pty_prompt_always_fits_one_terminal_line() {
        let root = std::env::temp_dir().join(format!("wicked-ptyprompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // A tree wide enough that an unbounded map would blow the line on its own.
        for i in 0..40 {
            std::fs::create_dir_all(root.join(format!("service-{i:03}"))).unwrap();
            std::fs::write(root.join(format!("service-{i:03}")).join("Cargo.toml"), "").unwrap();
        }

        let mut input = StepInput {
            run_id: "pty-line".to_string(),
            unit_ix: 0,
            attempt: 0,
            unit: WorkUnit::pending("s:build", "s", 1, "fix the API"),
            workflow_id: "wf-x".to_string(),
            entity_mode: crate::scope::EntityMode::Isolated,
            workdir: Some(root.clone()),
            governance: None,
            prior_outputs: vec![],
        };

        let p = pty_unit_prompt(&input).expect("a short description must not fail");
        // The submitting newline occupies the same line buffer, so it counts against the limit.
        let line_bytes = p.len() + 1;
        assert!(
            line_bytes <= PTY_PROMPT_LIMIT,
            "prompt is {line_bytes} bytes with the newline, over the {PTY_PROMPT_LIMIT} limit"
        );
        assert!(
            !p.contains('\n'),
            "an embedded newline would end the turn early: {p}"
        );
        // Trimmed, not dropped: the map is the point of FINDING-048 and still has to survive.
        assert!(
            p.contains("service-000/ [Cargo.toml]"),
            "the map must still reach the worker: {p}"
        );
        // The unbounded prompt is what would have deadlocked, so the cap has to be doing real work.
        assert!(
            unit_prompt(&input).len() + 1 > PTY_PROMPT_LIMIT,
            "this fixture no longer exercises the cap"
        );

        // A description that cannot fit fails fast and names the cause. Silence here is the bug:
        // this path burned the whole turn timeout for any description over ~509 bytes, pre-048 too.
        input.unit.description = "x".repeat(PTY_PROMPT_LIMIT);
        let err = pty_unit_prompt(&input).expect_err("an over-long description must not be sent");
        assert!(
            err.contains("pty turn cannot exceed"),
            "the failure must say why: {err}"
        );

        // No worktree ⇒ same prompt the non-pty runners build; the cap changes nothing on its own.
        input.unit.description = "fix the API".to_string();
        input.workdir = None;
        assert_eq!(pty_unit_prompt(&input).unwrap(), unit_prompt(&input));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_skill_prompt_flows_through_build_argv_as_one_guarded_arg() {
        let mut u = WorkUnit::pending("s:build", "s", 1, "do it");
        u.skill_ref = Some("wicked-testing-plan".to_string());
        let argv = build_argv("claude -p {PROMPT}", &skill_prompt(&u, None), &[]);
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
        // Input is fresh + cache-creation + cache-read = 25789 + 26103 + 34098. This test used to
        // assert the fresh 25789 alone while the fixture carried all three all along — 70% of the
        // context the model was actually given, and the part `total_cost_usd` bills for, was read
        // past (FINDING-058).
        assert_eq!(
            out.usage,
            Some(Usage {
                input_tokens: 85_990,
                output_tokens: 83,
                cost_usd: Some(0.409099),
            })
        );
        // Files from the tool_use `input.file_path`.
        assert_eq!(out.files, vec!["/tmp/wc-probe.txt".to_string()]);
    }

    #[test]
    fn claude_input_tokens_agree_with_the_acp_paths_definition() {
        // The two execution paths feed one `Usage` struct, one `CliUsage.inputTokens` on the wire,
        // and one column in the studio, so "input tokens" has to mean the same thing on both.
        // `acp_runner::parse_result_usage` sums fresh + cached reads/writes and says so in its doc;
        // this path must match, or a run's totals are not comparable across its own seats.
        let mut adapter = ClaudeStreamJson::default();
        let out = drive(
            &mut adapter,
            &[
                r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","usage":{"input_tokens":2,"cache_read_input_tokens":15273,"cache_creation_input_tokens":18195,"output_tokens":4}}"#,
            ],
        );
        let u = out.usage.expect("usage");
        assert_eq!(u.input_tokens, 2 + 15273 + 18195);
        assert_eq!(u.output_tokens, 4);

        // Absent cache fields are simply zero — an older CLI, or a turn with no cache, still
        // reports its fresh input rather than degrading to nothing.
        let mut adapter = ClaudeStreamJson::default();
        let out = drive(
            &mut adapter,
            &[
                r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","usage":{"input_tokens":7,"output_tokens":1}}"#,
            ],
        );
        assert_eq!(out.usage.expect("usage").input_tokens, 7);
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
        // FINDING-067: the daemon's OWN state home. `.wicked` was fenced and `.wicked-crew` was not,
        // which left `~/.wicked-crew/core.db` — every run, unit, policy and repo registration the
        // platform has — reachable by the file tools. Built through `rule_path` (not string-joined)
        // so this asserts the rule as the matcher will actually see it, separators and all, and all
        // three verbs are checked: a READ of that store is a cross-org leak on its own, before any
        // write destroys anything.
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .expect("a home to derive rules from");
        let crew = rule_path(&home.join(".wicked-crew")).expect("the state home is expressible");
        for tool in ["Read", "Edit", "Write"] {
            let rule = format!("{tool}({crew}/**)");
            assert!(
                denied.contains(&rule),
                "the operational store's home must be denied: expected `{rule}` in {denied}"
            );
        }
        assert!(
            denied.contains("Bash(find /:*)") && denied.contains("Bash(pkill:*)"),
            "the two verbs no path rule can catch, both observed in campaign transcripts: {denied}"
        );
    }

    /// A Windows path must reach the rule as a path, not as a string of escapes.
    ///
    /// `C:\Users\me\.claude` written into a glob rule verbatim unescapes to `C:Usersme.claude`, so
    /// the rule matches nothing and the fence is gone — while still LISTED in the settings file,
    /// which is the worst version: an operator reading it sees `.claude` denied, and it is not. The
    /// separator is injected rather than `#[cfg(windows)]`-gated so this runs on all three CI
    /// platforms; a Windows-only test is precisely how a separator bug survives review.
    #[test]
    fn a_windows_path_is_spelled_with_separators_the_matcher_understands() {
        let got =
            rule_path_sep(r"C:\Users\me\.claude", '\\').expect("a Windows path is expressible");
        assert_eq!(got, "C:/Users/me/.claude");
        assert!(
            !got.contains('\\'),
            "a surviving backslash is an escape, not a separator: {got}"
        );

        // The same bytes on POSIX are a filename containing backslashes, NOT separators. Rewriting
        // them would fence off a different directory than the one asked for, so this is refused (and
        // the caller warns) rather than silently mangled.
        assert_eq!(
            rule_path_sep(r"/home/me/we\ird", '/'),
            None,
            "a POSIX path with a literal backslash has no faithful rule spelling"
        );
        // Unchanged on POSIX, and the comma refusal survives the rewrite on both.
        assert_eq!(
            rule_path_sep("/home/me/.ssh", '/').as_deref(),
            Some("/home/me/.ssh")
        );
        assert_eq!(rule_path_sep("/home/a,b/.ssh", '/'), None);
        assert_eq!(rule_path_sep(r"C:\Users\a,b", '\\'), None);
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
