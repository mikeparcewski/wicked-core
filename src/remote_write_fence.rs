//! REMOTE-WRITE FENCE — a worker seat never pushes, never opens or edits a pull request, never
//! mutates GitHub (F-7R2-012, wave 6).
//!
//! ## The defect this closes
//!
//! In the Phase 7 re-run (run `b86c14c1`) the unit-7 agent opened wicked-studio PR #258 from its
//! own shell — `git push` to `origin` and `gh pr create`, on the daemon's ambient `gh` login. The
//! engine never saw it: the session recorded `delivery: "none"`, `/deliver-text` said "no repo
//! checks", `/acceptance` found nothing, the home strip did not count it. Delivery is the
//! ENGINE's job — the `deliver` tool phase lifts the run branch onto the current base,
//! re-verifies the repository's own checks on the lifted tree, pushes, and composes the PR — so a
//! PR the worker pushes around that seam is asserted, never re-derived, and invisible to the
//! ledger the acceptance gate reads.
//!
//! ## Three layers, one module
//!
//! 1. **Bash deny rules** ([`REMOTE_WRITE_BASH_RULES`]) — spelled the way the claude CLI enforces
//!    them (`Bash(<prefix>:*)`, the #434/#436 machinery), joined into every Bash deny list the
//!    engine writes (`execute_wrapped::denied_bash_rules`): the shared worker `settings.json`,
//!    each per-session settings file and ACP `session/new` options, and the council ballot's
//!    `--disallowedTools` argv.
//! 2. **The command filter** ([`remote_write_command`]) — for the carriers that see the command
//!    text per call: the wrapped CLAUDE carrier's `PreToolUse` gate hook
//!    (`gate_hook::boundary_denial_with`) and the ACP permission bridge
//!    (`acp_runner::answer_permission_request`, every role and posture). A prefix rule is blind to
//!    `cd x && git push`, `git -C <path> push`, `sh -c 'gh pr create …'`; the filter splits the
//!    command on shell separators (quote-aware) and judges every segment's program + verb.
//!    DENY-DOMINATES on `git`: a subcommand that is not a known git builtin is refused as a
//!    possible ALIAS (`git -c alias.p=push p`, `git config alias.p push && git p`), a `-c` /
//!    `--config-env` / `GIT_CONFIG_*` override of a fenced key (`alias.`, `url.`, `remote.`,
//!    `credential.`, …) is refused before the verb is even read, and `gh alias` is refused whole.
//!    A refusal is answered to the seat with [`REMEDY`], disclosed as `workerToolCallDenied`, and
//!    logged. Wrapped NON-claude single-shot seats (codex exec, pi, opencode, copilot) and ACP
//!    chat sessions have no per-call hook: for them layers 1 (claude only) and 3 are the fence.
//! 3. **Credential and transport stripping**
//!    (`wicked_apps_core::spawn::fence_remote_credentials`) — every seat spawn runs without the
//!    `GH_*`/`GITHUB_*` tokens, without `SSH_AUTH_SOCK` / `GIT_SSH*` / `GIT_ASKPASS` /
//!    `GIT_CONFIG_*`, with `gh` aimed at a credential-less config directory, with git's global
//!    and system config re-pointed at seat-owned files that RESET credential helpers, and with a
//!    transport-agnostic push kill (`url.wicked-nopush://.pushInsteadOf = https:// | ssh:// |
//!    git@ | file:// | …`) so a push over ANY transport — aliased or not, keys on disk or not —
//!    fails "unable to find remote helper" while fetch and pull keep working. The deliver tool
//!    phase applies no seat config and keeps the daemon's login.
//!
//! Stated limit, as everywhere in this codebase: the shell is Turing-complete, so a determined
//! escape (`base64 | sh`, a script file, a variable holding the verb, a renamed binary) can evade
//! a scan of the literal command. Layer 3 is what holds then — and since wave 6's review it holds
//! on every transport git knows, not only on gh-authenticated https.

/// The claude CLI's Bash deny rules for remote-writing `git`/`gh` invocations — PREFIX rules
/// (`Bash(<prefix>:*)`), the one Bash rule form the CLI matches (wicked-crew#524 / F-3R2-004:
/// path-tool rules are `Read(...)`/`Edit(...)`; Bash rules are exact or `:*`-prefixed). Every
/// `gh api` is denied whole: the CLI cannot see the method, and a mutation is one flag away from
/// a read (the command filter below lets a plain `GET` through on the carriers that can judge it).
pub(crate) const REMOTE_WRITE_BASH_RULES: &[&str] = &[
    "Bash(git push:*)",
    "Bash(git push)",
    "Bash(git send-pack:*)",
    "Bash(git send-email:*)",
    "Bash(git imap-send:*)",
    "Bash(git svn dcommit:*)",
    "Bash(git p4 submit:*)",
    "Bash(git lfs push:*)",
    "Bash(git config alias.:*)",
    "Bash(git config --global:*)",
    "Bash(git config --system:*)",
    "Bash(git --config-env:*)",
    "Bash(gh pr create:*)",
    "Bash(gh pr merge:*)",
    "Bash(gh pr edit:*)",
    "Bash(gh pr comment:*)",
    "Bash(gh pr review:*)",
    "Bash(gh pr close:*)",
    "Bash(gh pr reopen:*)",
    "Bash(gh pr ready:*)",
    "Bash(gh pr lock:*)",
    "Bash(gh pr unlock:*)",
    "Bash(gh api:*)",
    "Bash(gh alias:*)",
    "Bash(gh release:*)",
    "Bash(gh issue create:*)",
    "Bash(gh issue comment:*)",
    "Bash(gh issue edit:*)",
    "Bash(gh issue close:*)",
    "Bash(gh issue reopen:*)",
    "Bash(gh issue delete:*)",
    "Bash(gh repo create:*)",
    "Bash(gh repo delete:*)",
    "Bash(gh repo edit:*)",
    "Bash(gh repo fork:*)",
    "Bash(gh repo sync:*)",
    "Bash(gh repo archive:*)",
    "Bash(gh repo rename:*)",
    "Bash(gh auth:*)",
    "Bash(gh workflow run:*)",
    "Bash(gh workflow enable:*)",
    "Bash(gh workflow disable:*)",
    "Bash(gh run cancel:*)",
    "Bash(gh run rerun:*)",
    "Bash(gh run delete:*)",
    "Bash(gh secret:*)",
    "Bash(gh variable:*)",
    "Bash(gh label:*)",
    "Bash(gh gist create:*)",
    "Bash(gh gist edit:*)",
    "Bash(gh gist delete:*)",
];

/// The remedy every refusal carries to the seat and onto the wire (`workerToolCallDenied.remedy`).
pub(crate) const REMEDY: &str = "delivery is performed by the run's deliver phase: the engine \
    lifts the run branch onto the current base, re-verifies the repository's own checks, pushes \
    and opens the pull request, so the ledger records it. Commit your work on the run branch and \
    finish the unit; never push or open/edit a PR from a worker seat";

/// One remote-writing invocation the filter found in a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteWriteHit {
    /// The program judged (`git`, `gh`, or `env` for a fenced environment variable set in the
    /// seat's shell).
    pub program: &'static str,
    /// The verb path that writes remotely (`push`, `pr create`, `api (mutation)`, `alias`,
    /// `-c alias.p (config override)`, `unknown subcommand p (possible alias)`, …).
    pub verb: String,
    /// The command segment the hit was found in (trimmed).
    pub segment: String,
}

impl RemoteWriteHit {
    /// The operator- and seat-facing reason for the refusal (the `reason` on the wire).
    pub(crate) fn reason(&self) -> String {
        format!(
            "remote-write fence: `{} {}` is not available to a worker seat (segment: `{}`) — {}",
            self.program, self.verb, self.segment, REMEDY
        )
    }
}

/// `gh` subcommand → verbs that WRITE remotely. `api` is judged separately (by method/flags);
/// `auth` and `alias` are denied whole (credentials; a verb alias defeats every other rule).
/// Absent subcommands (`pr view`, `pr list`, `pr checkout`, `pr diff`, `pr checks`, `issue view`,
/// `run view`, `repo view`, `repo clone`) are reads or local and pass.
const GH_REMOTE_WRITE_VERBS: &[(&str, &[&str])] = &[
    (
        "pr",
        &[
            "create",
            "merge",
            "edit",
            "comment",
            "review",
            "close",
            "reopen",
            "ready",
            "lock",
            "unlock",
            "update-branch",
        ],
    ),
    (
        "issue",
        &[
            "create", "comment", "edit", "close", "reopen", "delete", "transfer", "pin", "unpin",
            "lock", "unlock", "develop",
        ],
    ),
    (
        "repo",
        &[
            "create",
            "delete",
            "edit",
            "fork",
            "sync",
            "archive",
            "unarchive",
            "rename",
            "deploy-key",
            "autolink",
        ],
    ),
    (
        "release",
        &["create", "delete", "edit", "upload", "delete-asset"],
    ),
    ("gist", &["create", "edit", "delete"]),
    ("label", &["create", "edit", "delete", "clone"]),
    ("secret", &["set", "delete", "remove"]),
    ("variable", &["set", "delete", "remove"]),
    ("workflow", &["run", "enable", "disable"]),
    ("run", &["cancel", "rerun", "delete"]),
    ("cache", &["delete"]),
    (
        "project",
        &[
            "create",
            "edit",
            "delete",
            "close",
            "item-add",
            "item-edit",
            "item-delete",
        ],
    ),
];

/// `gh` subcommands denied WHOLE, whatever follows: `auth` mints, prints or moves credentials;
/// `alias` defines a verb every other rule is blind to (review of #449, FN-1).
const GH_DENIED_WHOLE: &[&str] = &["auth", "alias"];

/// `gh` global options that take a separate value.
const GH_VALUE_FLAGS: &[&str] = &["-R", "--repo", "--hostname"];

/// git's own subcommands (`git --list-cmds=builtins`, 2.50) plus the porcelain most repos
/// carry. A `git <verb>` whose verb is NOT here is refused as a possible ALIAS — deny-dominates:
/// with `-c alias.*`, `GIT_CONFIG_*` and `git config alias.*` refused (below) and the seat's
/// global/system git config re-pointed (layer 3), the only alias source left is the repo's own
/// config, which the filter cannot read — so an unknown verb is treated as one.
const GIT_KNOWN_SUBCOMMANDS: &[&str] = &[
    "add",
    "am",
    "annotate",
    "apply",
    "archive",
    "backfill",
    "bisect",
    "blame",
    "branch",
    "bugreport",
    "bundle",
    "cat-file",
    "check-attr",
    "check-ignore",
    "check-mailmap",
    "check-ref-format",
    "checkout",
    "checkout-index",
    "cherry",
    "cherry-pick",
    "citool",
    "clean",
    "clone",
    "column",
    "commit",
    "commit-graph",
    "commit-tree",
    "config",
    "count-objects",
    "credential",
    "credential-cache",
    "credential-store",
    "describe",
    "diagnose",
    "diff",
    "diff-files",
    "diff-index",
    "diff-pairs",
    "diff-tree",
    "difftool",
    "fast-export",
    "fast-import",
    "fetch",
    "fetch-pack",
    "filter-branch",
    "filter-repo",
    "fmt-merge-msg",
    "for-each-ref",
    "for-each-repo",
    "format-patch",
    "fsck",
    "fsck-objects",
    "gc",
    "get-tar-commit-id",
    "grep",
    "gui",
    "hash-object",
    "help",
    "hook",
    "index-pack",
    "init",
    "init-db",
    "instaweb",
    "interpret-trailers",
    "lfs",
    "log",
    "ls-files",
    "ls-remote",
    "ls-tree",
    "mailinfo",
    "mailsplit",
    "maintenance",
    "merge",
    "merge-base",
    "merge-file",
    "merge-index",
    "merge-ours",
    "merge-recursive",
    "merge-subtree",
    "merge-tree",
    "mergetool",
    "mktag",
    "mktree",
    "multi-pack-index",
    "mv",
    "name-rev",
    "notes",
    "p4",
    "pack-objects",
    "pack-redundant",
    "pack-refs",
    "patch-id",
    "prune",
    "prune-packed",
    "pull",
    "range-diff",
    "read-tree",
    "rebase",
    "reflog",
    "refs",
    "remote",
    "repack",
    "replace",
    "replay",
    "request-pull",
    "rerere",
    "reset",
    "restore",
    "rev-list",
    "rev-parse",
    "revert",
    "rm",
    "shortlog",
    "show",
    "show-branch",
    "show-index",
    "show-ref",
    "sparse-checkout",
    "stage",
    "stash",
    "status",
    "stripspace",
    "submodule",
    "subtree",
    "svn",
    "switch",
    "symbolic-ref",
    "tag",
    "unpack-file",
    "unpack-objects",
    "update-index",
    "update-ref",
    "update-server-info",
    "var",
    "verify-commit",
    "verify-pack",
    "verify-tag",
    "version",
    "whatchanged",
    "worktree",
    "write-tree",
];

/// git verbs that WRITE REMOTELY (or send the work elsewhere), refused outright.
const GIT_REMOTE_WRITE_VERBS: &[&str] = &["push", "send-pack", "send-email", "imap-send"];

/// Two-token git verbs that write remotely.
const GIT_REMOTE_WRITE_PAIRS: &[(&str, &str)] = &[
    ("svn", "dcommit"),
    ("svn", "branch"),
    ("svn", "tag"),
    ("p4", "submit"),
    ("lfs", "push"),
    ("lfs", "migrate"),
];

/// git config keys (case-folded prefixes) a seat may not override or write: an alias defeats the
/// verb filter; `url.*` / `remote.*` re-aim where a push goes (and would undo layer 3's
/// `pushInsteadOf` kill); `credential.*`, `core.sshCommand`, `core.askPass`, `core.gitProxy`,
/// `http.*`, `ssh.*`, `protocol.*` reach credentials or transports; `include.*` pulls in a file the
/// filter cannot read; `core.hooksPath` runs scripts the filter never sees.
const FENCED_GIT_CONFIG_KEY_PREFIXES: &[&str] = &[
    "alias.",
    "url.",
    "remote.",
    "credential.",
    "core.sshcommand",
    "core.askpass",
    "core.gitproxy",
    "core.hookspath",
    "http.",
    "https.",
    "ssh.",
    "protocol.",
    "include.",
    "includeif.",
    "uploadpack.",
    "receive.",
];

/// Environment variables (name prefixes) a seat may not set for a git invocation — they inject
/// config, re-point transports or credentials, or swap git's own subcommand directory. Layer 3
/// strips them from the spawn; the filter refuses a seat re-adding them in its shell.
const FENCED_GIT_ENV_PREFIXES: &[&str] = &[
    "GIT_CONFIG",
    "GIT_SSH",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "SSH_AUTH_SOCK",
    "GIT_CREDENTIAL",
    "GIT_EXEC_PATH",
    "GIT_PROXY_COMMAND",
    "GIT_TEMPLATE_DIR",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_CONFIG_DIR",
];

/// Shell builtins that SET environment for later commands in the same shell.
const ENV_SETTERS: &[&str] = &["export", "declare", "typeset", "setenv", "set"];

/// Shell operators that separate one command from the next — judged OUTSIDE quotes. `$(…)` and
/// backticks are separators too: a substitution's content is executed.
const SEGMENT_SEPARATORS: &[char] = &[';', '|', '&', '(', ')', '`', '\n', '\r', '{', '}'];

/// Wrappers that run the NEXT token as the program: `env`, `sudo`, `command`, `exec`, `nohup`,
/// `time`, `nice`, `stdbuf`, `xargs`, `timeout <n>`. Their own flags/args are skipped.
const WRAPPERS: &[&str] = &[
    "env",
    "sudo",
    "command",
    "exec",
    "nohup",
    "time",
    "nice",
    "stdbuf",
    "xargs",
    "timeout",
    "doas",
    "caffeinate",
    "script",
];

/// POSIX-family shells whose `-c` cluster names a script argument.
const SH_INTERPRETERS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish", "ash", "busybox"];
/// PowerShell: `-Command`/`-c` takes the REST of the line.
const PWSH_INTERPRETERS: &[&str] = &["pwsh", "powershell"];
/// Windows `cmd`: `/c` / `/k` take the REST of the line (unquoted is the common spelling).
const CMD_INTERPRETERS: &[&str] = &["cmd"];

/// Judge one shell command: `Some(hit)` names the FIRST remote-writing invocation in it, `None`
/// when every segment is a read or local. Pure; case-sensitive on the verb (git and gh are),
/// basename-tolerant on the program (`/usr/bin/git`, `git.exe`, `git-push`). Quotes group tokens
/// (a quoted sentence is data), except as the script argument of a shell interpreter or `eval`,
/// which is judged as a command in its own right.
pub(crate) fn remote_write_command(command: &str) -> Option<RemoteWriteHit> {
    for segment in split_segments(command) {
        let tokens = tokenize(&segment);
        if let Some(hit) = judge_tokens(&tokens) {
            return Some(RemoteWriteHit {
                program: hit.0,
                verb: hit.1,
                segment: segment.trim().to_string(),
            });
        }
    }
    None
}

/// Split on [`SEGMENT_SEPARATORS`] outside single/double quotes (a backslash escapes the next
/// character outside single quotes). Empty segments are dropped.
fn split_segments(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' {
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    cur.push(c);
                } else if c == '\\' {
                    cur.push(c);
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                } else if SEGMENT_SEPARATORS.contains(&c) {
                    if !cur.trim().is_empty() {
                        out.push(std::mem::take(&mut cur));
                    } else {
                        cur.clear();
                    }
                } else {
                    cur.push(c);
                }
            }
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Whitespace tokenizer that keeps a quoted string as ONE token (quotes stripped, `\x` → `x`
/// outside single quotes). A `$` stays a token character (a variable is not a verb).
fn tokenize(segment: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    let mut chars = segment.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' {
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    in_token = true;
                } else if c == '\\' {
                    if let Some(n) = chars.next() {
                        cur.push(n);
                        in_token = true;
                    }
                } else if c.is_whitespace() {
                    if in_token {
                        out.push(std::mem::take(&mut cur));
                        in_token = false;
                    }
                } else {
                    cur.push(c);
                    in_token = true;
                }
            }
        }
    }
    if in_token {
        out.push(cur);
    }
    out
}

/// The program's file stem, case-folded: `/usr/local/bin/GIT.exe` → `git`.
fn program_stem(tok: &str) -> String {
    let base = tok.rsplit(['/', '\\']).next().unwrap_or(tok);
    let lower = base.to_ascii_lowercase();
    let stem = lower
        .strip_suffix(".exe")
        .or_else(|| lower.strip_suffix(".cmd"))
        .or_else(|| lower.strip_suffix(".bat"))
        .unwrap_or(&lower);
    stem.to_string()
}

/// Is `tok` a `NAME=value` environment assignment? Returns the name.
fn env_assignment(tok: &str) -> Option<&str> {
    let (name, _) = tok.split_once('=')?;
    let ok = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    ok.then_some(name)
}

/// Is `name` an environment variable a seat may not set for git/gh?
fn fenced_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    FENCED_GIT_ENV_PREFIXES.iter().any(|p| upper.starts_with(p))
}

/// Is `key` (a git config key) one a seat may not override or write?
fn fenced_config_key(key: &str) -> bool {
    let lower = key.trim().to_ascii_lowercase();
    FENCED_GIT_CONFIG_KEY_PREFIXES
        .iter()
        .any(|p| lower.starts_with(p))
}

/// Skip leading env assignments and wrapper programs (with their own options) to the token that
/// is the program being run. Returns the index of that token and every environment assignment
/// seen on the way (leading ones and `env`'s), or `None` when the segment has no program left
/// (e.g. `FOO=1` alone).
fn program_index(tokens: &[String]) -> Option<(usize, Vec<String>)> {
    let mut assigned: Vec<String> = Vec::new();
    let mut i = 0;
    loop {
        while i < tokens.len() {
            match env_assignment(&tokens[i]) {
                Some(name) => {
                    assigned.push(name.to_string());
                    i += 1;
                }
                None => break,
            }
        }
        let tok = tokens.get(i)?;
        let stem = program_stem(tok);
        if WRAPPERS.contains(&stem.as_str()) {
            i += 1;
            if stem == "timeout" {
                // `timeout [-k <v>] [-s <v>] [--foreground] <duration> <command>`.
                while i < tokens.len() && tokens[i].starts_with('-') {
                    let opt = tokens[i].as_str();
                    i += 1;
                    if matches!(opt, "-k" | "--kill-after" | "-s" | "--signal") && i < tokens.len()
                    {
                        i += 1;
                    }
                }
                if i < tokens.len() && env_assignment(&tokens[i]).is_none() {
                    i += 1; // the duration
                }
            } else {
                // `nice -n N` / `stdbuf -oL` / `env -i` / `sudo -u user`: skip the wrapper's own
                // dashed options and their values.
                while i < tokens.len() && tokens[i].starts_with('-') {
                    let opt = tokens[i].as_str();
                    i += 1;
                    if matches!(opt, "-u" | "-g" | "-n" | "-o" | "-e" | "-i" | "-C" | "-P")
                        && i < tokens.len()
                        && !tokens[i].starts_with('-')
                    {
                        i += 1;
                    }
                }
            }
            continue;
        }
        return Some((i, assigned));
    }
}

/// The SCRIPT a POSIX-family shell is asked to run: the argument after a short-flag cluster
/// containing `c` (`-c`, `-lc`, `-ic`, `-ec`, `-xc`); `-o <opt>` pairs are skipped; a bare `-e`
/// (errexit) is NOT a script flag (review of #449, FN-2). `None` when the shell is invoked on a
/// file or interactively.
fn sh_script(args: &[String]) -> Option<&str> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-o" {
            i += 2;
            continue;
        }
        if a == "--" {
            return None;
        }
        let cluster = a.starts_with('-') && !a.starts_with("--") && a.len() <= 6;
        if cluster && a[1..].contains('c') {
            return args.get(i + 1).map(String::as_str);
        }
        if !a.starts_with('-') {
            return None; // a script FILE — the stated limit
        }
        i += 1;
    }
    None
}

/// The script `pwsh -Command …` / `cmd /c …` runs: the REST of the arguments, joined — both
/// shells take the remainder of the line, quoted or not (the unquoted `cmd /c git push origin
/// main` is the common Windows spelling).
fn rest_script(args: &[String], flags: &[&str]) -> Option<String> {
    let pos = args
        .iter()
        .position(|a| flags.iter().any(|f| a.eq_ignore_ascii_case(f)))?;
    let rest = &args[pos + 1..];
    (!rest.is_empty()).then(|| rest.join(" "))
}

/// Judge one segment's tokens: the program (after env assignments and wrappers) is `git`/`gh`
/// with a remote-writing verb, a fenced config override, or an unknown (alias) verb; a shell
/// interpreter / `eval` whose script is judged as a command in its own right; an environment
/// setter naming a fenced variable; or a fenced variable assigned in front of `git`/`gh`.
fn judge_tokens(tokens: &[String]) -> Option<(&'static str, String)> {
    let (start, assigned) = program_index(tokens)?;
    let program = program_stem(&tokens[start]);
    let args = &tokens[start + 1..];
    let program_is_git = program == "git" || program.starts_with("git-");
    if (program_is_git || program == "gh") && assigned.iter().any(|n| fenced_env_name(n)) {
        let names: Vec<&str> = assigned
            .iter()
            .filter(|n| fenced_env_name(n))
            .map(String::as_str)
            .collect();
        return Some(("env", format!("{} (fenced variable)", names.join(", "))));
    }
    if ENV_SETTERS.contains(&program.as_str()) {
        if let Some(name) = args
            .iter()
            .filter_map(|a| env_assignment(a).or(Some(a.as_str())))
            .find(|n| fenced_env_name(n))
        {
            return Some(("env", format!("{program} {name} (fenced variable)")));
        }
        return None;
    }
    match program.as_str() {
        "git" => judge_git(args).map(|v| ("git", v)),
        p if p.starts_with("git-") => {
            // `git-push …` — the dashed spelling of a git verb.
            let verb = p["git-".len()..].to_string();
            let mut full: Vec<String> = vec![verb];
            full.extend(args.iter().cloned());
            judge_git(&full).map(|v| ("git", v))
        }
        "gh" => judge_gh(args).map(|v| ("gh", v)),
        "eval" => {
            let script = args.join(" ");
            remote_write_command(&script).map(|h| (h.program, h.verb))
        }
        p if SH_INTERPRETERS.contains(&p) => sh_script(args)
            .and_then(remote_write_command)
            .map(|h| (h.program, h.verb)),
        p if PWSH_INTERPRETERS.contains(&p) => {
            rest_script(args, &["-Command", "-c", "-EncodedCommand"])
                .and_then(|s| remote_write_command(&s))
                .map(|h| (h.program, h.verb))
        }
        p if CMD_INTERPRETERS.contains(&p) => rest_script(args, &["/c", "/k"])
            .and_then(|s| remote_write_command(&s))
            .map(|h| (h.program, h.verb)),
        _ => None,
    }
}

/// `git [global options] <subcommand> …`. Refused: a remote-writing subcommand; a `-c <k=v>`
/// whose key is fenced; any `--config-env`; any `--exec-path` (re-points git's subcommand
/// directory); a `config` write of a fenced key; and any subcommand not in
/// [`GIT_KNOWN_SUBCOMMANDS`] — a possible alias. Other global options (`-C <p>`, `--git-dir`,
/// `--work-tree`, `--namespace`) are skipped with their value; bare flags alone.
fn judge_git(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let tok = args[i].as_str();
        if !tok.starts_with('-') {
            return judge_git_subcommand(tok, &args[i + 1..]);
        }
        if tok == "-c" {
            let kv = args.get(i + 1).map(String::as_str).unwrap_or("");
            let key = kv.split('=').next().unwrap_or(kv);
            if fenced_config_key(key) {
                return Some(format!("-c {key} (config override)"));
            }
            i += 2;
            continue;
        }
        if tok == "--config-env" || tok.starts_with("--config-env=") {
            return Some("--config-env (config from the environment)".to_string());
        }
        if tok == "--exec-path" || tok.starts_with("--exec-path=") {
            return Some("--exec-path (git subcommand directory override)".to_string());
        }
        let takes_value = matches!(
            tok,
            "-C" | "--git-dir" | "--work-tree" | "--namespace" | "--super-prefix" | "--list-cmds"
        );
        i += if takes_value { 2 } else { 1 };
    }
    None
}

/// The verdict for `git <sub> <rest…>`.
fn judge_git_subcommand(sub: &str, rest: &[String]) -> Option<String> {
    if GIT_REMOTE_WRITE_VERBS.contains(&sub) {
        return Some(sub.to_string());
    }
    if let Some((_, second)) = GIT_REMOTE_WRITE_PAIRS
        .iter()
        .find(|(a, b)| *a == sub && rest.iter().any(|r| r == b))
    {
        return Some(format!("{sub} {second}"));
    }
    if sub == "config" {
        return judge_git_config(rest);
    }
    if sub.starts_with("remote-") {
        // `git remote-https origin <url>` speaks the transport protocol directly.
        return Some(format!("{sub} (transport helper)"));
    }
    if !GIT_KNOWN_SUBCOMMANDS.contains(&sub) {
        return Some(format!("unknown subcommand {sub} (possible alias)"));
    }
    None
}

/// `git config …`: a READ (`--get*`, `-l`/`--list`, `--show-*`) passes; a write naming a fenced
/// key — `alias.*`, `url.*`, `remote.*`, `credential.*`, `core.sshCommand`, … — is refused,
/// whichever file it targets (`--global`, `--system`, `--file`, the repo's).
fn judge_git_config(rest: &[String]) -> Option<String> {
    const READ_FLAGS: &[&str] = &[
        "--get",
        "--get-all",
        "--get-regexp",
        "--get-urlmatch",
        "--get-color",
        "--get-colorbool",
        "-l",
        "--list",
        "--show-origin",
        "--show-scope",
        "list",
        "get",
    ];
    if rest.iter().any(|r| READ_FLAGS.contains(&r.as_str())) {
        return None;
    }
    rest.iter()
        .filter(|r| !r.starts_with('-'))
        .find(|r| fenced_config_key(r))
        .map(|k| format!("config {k} (fenced key)"))
}

/// `gh [global options] <command> [options] <subcommand> …`. Global options: `-R/--repo <v>`,
/// `--hostname <v>` (and their `=` forms) are skipped with their value; the verb is the first
/// non-flag token after the command, skipping the same value-taking options (`gh pr -R o/r
/// create`, review of #449 FN-2).
fn judge_gh(args: &[String]) -> Option<String> {
    let mut i = skip_gh_options(args, 0);
    let command = args.get(i)?.as_str();
    if GH_DENIED_WHOLE.contains(&command) {
        return Some(command.to_string());
    }
    if command == "api" {
        return judge_gh_api(&args[i + 1..]).then(|| "api (mutation)".to_string());
    }
    let (_, verbs) = GH_REMOTE_WRITE_VERBS.iter().find(|(c, _)| *c == command)?;
    i = skip_gh_options(args, i + 1);
    let verb = args.get(i)?.as_str();
    verbs.contains(&verb).then(|| format!("{command} {verb}"))
}

/// Advance past gh options starting at `i`: value-taking ones with their value, `--x=y` and bare
/// flags alone. Returns the index of the first positional token.
fn skip_gh_options(args: &[String], mut i: usize) -> usize {
    while i < args.len() && args[i].starts_with('-') {
        let tok = args[i].as_str();
        i += if GH_VALUE_FLAGS.contains(&tok) { 2 } else { 1 };
    }
    i
}

/// `gh api` mutates when it names a non-GET method or carries a body: `-X/--method <M>` (or
/// attached: `-XPOST`, `--method=PATCH`) with `M != GET`, or any of `-f/-F/--field/--raw-field/
/// --input` (attached forms `-ftitle=x`, `--field=…` included — they imply `POST`). A plain
/// `gh api repos/o/r/pulls` is a read and passes.
fn judge_gh_api(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let tok = args[i].as_str();
        match tok {
            "-X" | "--method" => {
                let method = args.get(i + 1).map(|m| m.to_ascii_uppercase());
                if method.as_deref() != Some("GET") {
                    return true;
                }
                i += 2;
                continue;
            }
            "-f" | "-F" | "--field" | "--raw-field" | "--input" => return true,
            _ => {}
        }
        if let Some(m) = tok
            .strip_prefix("--method=")
            .or_else(|| tok.strip_prefix("-X"))
        {
            if !m.is_empty() && !m.eq_ignore_ascii_case("GET") {
                return true;
            }
        }
        if tok.starts_with("--field=")
            || tok.starts_with("--raw-field=")
            || tok.starts_with("--input=")
            || (tok.starts_with("-f") && tok.len() > 2)
            || (tok.starts_with("-F") && tok.len() > 2)
        {
            return true;
        }
        i += 1;
    }
    false
}

/// (review of #449, FN-1/FN-2) The bypass spellings the independent review reproduced against
/// the first cut, plus the observed F-7R2-012 spelling — EVERY one must be refused by the filter,
/// by the wrapped gate hook and by the ACP permission bridge (the three carriers' tests iterate
/// this list).
#[cfg(test)]
pub(crate) const REVIEW_BYPASS_STRINGS: &[&str] = &[
    "git push -u origin wicked/b86c14c1",
    "gh pr create --title \"x\" --body-file /tmp/b.md",
    "git -c alias.p=push p origin main",
    "git -c alias.p='push origin main' p",
    "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=alias.p GIT_CONFIG_VALUE_0=push git p",
    "git config alias.p push && git p",
    "gh alias set pc 'pr create'; gh pc --fill",
    "git --config-env=alias.p=P p",
    "git -c url.wicked-nopush://.pushInsteadOf= push",
    "git -c remote.origin.pushurl=git@example.invalid:o/r.git push",
    "git -c credential.helper=store fetch",
    "export GIT_CONFIG_COUNT=1; git p",
    "env GIT_SSH_COMMAND='ssh -i /tmp/k' git fetch",
    "bash -e -c 'git push'",
    "sh -e -c 'gh pr create'",
    "gh pr -R o/r create --fill",
    "gh pr --repo o/r create",
    "gh --hostname github.com pr create",
    "gh api -XPOST repos/o/r/pulls",
    "gh api -ftitle=x repos/o/r/issues",
    "cmd /c git push origin main",
    "timeout -k 5 30 git push",
    "git-push origin main",
    "git remote-https origin https://example.invalid/o/r.git",
    "git send-email --to x@example.invalid HEAD~1",
    "git svn dcommit",
    "git lfs push origin main",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(cmd: &str) -> Option<(&'static str, String)> {
        remote_write_command(cmd).map(|h| (h.program, h.verb))
    }

    #[test]
    fn the_observed_spellings_are_denied() {
        // Exactly what the unit-7 agent ran in run b86c14c1.
        assert_eq!(
            hit("git push -u origin wicked/b86c14c1"),
            Some(("git", "push".into()))
        );
        assert_eq!(
            hit("gh pr create --title \"x\" --body-file /tmp/b.md"),
            Some(("gh", "pr create".into()))
        );
        assert_eq!(hit("git push"), Some(("git", "push".into())));
        assert_eq!(
            hit("git push --force-with-lease"),
            Some(("git", "push".into()))
        );
    }

    #[test]
    fn prefix_evasions_are_caught_by_the_segment_scan() {
        assert_eq!(
            hit("cd /wt && git -C /wt push origin HEAD"),
            Some(("git", "push".into()))
        );
        assert_eq!(hit("ls; git push"), Some(("git", "push".into())));
        assert_eq!(
            hit("true || gh pr merge 12 --squash"),
            Some(("gh", "pr merge".into()))
        );
        assert_eq!(
            hit("sh -c 'git push origin main'"),
            Some(("git", "push".into()))
        );
        assert_eq!(
            hit("bash -lc \"gh pr create -f\""),
            Some(("gh", "pr create".into()))
        );
        assert_eq!(
            hit("GIT_SSH_COMMAND=ssh env GH_TOKEN=x sudo -u me /usr/bin/git push"),
            Some(("env", "GIT_SSH_COMMAND, GH_TOKEN (fenced variable)".into()))
        );
        assert_eq!(hit("timeout 30 git push"), Some(("git", "push".into())));
        assert_eq!(
            hit("echo $(gh pr create --fill)"),
            Some(("gh", "pr create".into()))
        );
        assert_eq!(
            hit("git --git-dir=/x/.git --work-tree /x push"),
            Some(("git", "push".into()))
        );
        assert_eq!(
            hit("gh -R o/r pr comment 3 -b hi"),
            Some(("gh", "pr comment".into()))
        );
        assert_eq!(
            hit("git send-pack --all host:repo"),
            Some(("git", "send-pack".into()))
        );
        assert_eq!(hit("gh auth token"), Some(("gh", "auth".into())));
        assert_eq!(
            hit("gh release create v1 ./a.tgz"),
            Some(("gh", "release create".into()))
        );
        assert_eq!(hit("GIT.EXE push"), Some(("git", "push".into())));
        assert_eq!(
            hit("eval git push origin main"),
            Some(("git", "push".into()))
        );
        assert_eq!(hit("zsh -ic 'git push'"), Some(("git", "push".into())));
        assert_eq!(
            hit("cmd /c \"git push origin main\""),
            Some(("git", "push".into()))
        );
        assert_eq!(
            hit("pwsh -Command \"gh pr merge 3\""),
            Some(("gh", "pr merge".into()))
        );
        assert_eq!(
            hit("git -c core.pager=cat push"),
            Some(("git", "push".into()))
        );
    }

    /// Review of #449, FN-1: aliases, config injection and environment injection — each string
    /// the review reproduced as a miss is a hit, named for what it is.
    #[test]
    fn alias_config_and_environment_injection_are_refused() {
        assert_eq!(
            hit("git -c alias.p=push p origin main"),
            Some(("git", "-c alias.p (config override)".into()))
        );
        assert_eq!(
            hit("git -c alias.p='push origin main' p"),
            Some(("git", "-c alias.p (config override)".into()))
        );
        assert_eq!(
            hit("GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=alias.p GIT_CONFIG_VALUE_0=push git p"),
            Some((
                "env",
                "GIT_CONFIG_COUNT, GIT_CONFIG_KEY_0, GIT_CONFIG_VALUE_0 (fenced variable)".into()
            ))
        );
        assert_eq!(
            hit("git config alias.p push && git p"),
            Some(("git", "config alias.p (fenced key)".into()))
        );
        assert_eq!(
            hit("git p"),
            Some(("git", "unknown subcommand p (possible alias)".into()))
        );
        assert_eq!(
            hit("gh alias set pc 'pr create'; gh pc --fill"),
            Some(("gh", "alias".into()))
        );
        assert_eq!(
            hit("git --config-env=alias.p=P p"),
            Some(("git", "--config-env (config from the environment)".into()))
        );
        assert_eq!(
            hit("git --exec-path=/tmp/evil status"),
            Some((
                "git",
                "--exec-path (git subcommand directory override)".into()
            ))
        );
        assert!(hit("git -c url.wicked-nopush://.pushInsteadOf= push").is_some());
        assert!(hit("git -c remote.origin.pushurl=git@example.invalid:o/r.git fetch").is_some());
        assert!(hit("git -c credential.helper=store fetch").is_some());
        assert!(hit("git -c core.sshCommand='ssh -i k' fetch").is_some());
        assert!(hit("git config --global credential.helper osxkeychain").is_some());
        assert!(hit("git config url.x.pushInsteadOf y").is_some());
        assert_eq!(
            hit("export GIT_CONFIG_COUNT=1; git p"),
            Some(("env", "export GIT_CONFIG_COUNT (fenced variable)".into()))
        );
        assert!(hit("env GIT_SSH_COMMAND='ssh -i /tmp/k' git fetch").is_some());
        assert!(hit("SSH_AUTH_SOCK=/tmp/s git fetch").is_some());
        assert!(hit("GH_CONFIG_DIR=/tmp/gh gh pr view 1").is_some());
        assert_eq!(hit("git-push origin main"), Some(("git", "push".into())));
        assert!(hit("git remote-https origin https://example.invalid/o/r.git").is_some());
        assert_eq!(
            hit("git send-email --to x@example.invalid HEAD~1"),
            Some(("git", "send-email".into()))
        );
        assert_eq!(hit("git svn dcommit"), Some(("git", "svn dcommit".into())));
        assert_eq!(
            hit("git lfs push origin main"),
            Some(("git", "lfs push".into()))
        );
        // …while reads of the same keys, and ordinary config writes, pass.
        assert_eq!(hit("git config --get alias.p"), None);
        assert_eq!(hit("git config --list"), None);
        assert_eq!(hit("git config user.name 'wicked worker'"), None);
        assert_eq!(hit("git config core.autocrlf false"), None);
        assert_eq!(hit("git -c user.email=a@b.invalid commit -m x"), None);
        assert_eq!(hit("GIT_AUTHOR_NAME=w git commit -m x"), None);
        assert_eq!(hit("export PATH=/usr/bin; git status"), None);
    }

    /// Review of #449, FN-2: the parser gaps, each a direct spelling.
    #[test]
    fn parser_gaps_from_the_review_are_closed() {
        assert_eq!(hit("bash -e -c 'git push'"), Some(("git", "push".into())));
        assert_eq!(
            hit("sh -e -c 'gh pr create'"),
            Some(("gh", "pr create".into()))
        );
        assert_eq!(
            hit("bash -o pipefail -c 'git push'"),
            Some(("git", "push".into()))
        );
        assert_eq!(
            hit("gh pr -R o/r create --fill"),
            Some(("gh", "pr create".into()))
        );
        assert_eq!(
            hit("gh pr --repo o/r create"),
            Some(("gh", "pr create".into()))
        );
        assert_eq!(
            hit("gh --hostname github.com pr create"),
            Some(("gh", "pr create".into()))
        );
        assert_eq!(
            hit("gh --repo=o/r pr merge 1"),
            Some(("gh", "pr merge".into()))
        );
        assert!(hit("gh api -XPOST repos/o/r/pulls").is_some());
        assert!(hit("gh api -ftitle=x repos/o/r/issues").is_some());
        assert!(hit("gh api -Fbody=@f repos/o/r/issues").is_some());
        assert_eq!(
            hit("cmd /c git push origin main"),
            Some(("git", "push".into()))
        );
        assert_eq!(hit("cmd /k git push"), Some(("git", "push".into())));
        assert_eq!(
            hit("timeout -k 5 30 git push"),
            Some(("git", "push".into()))
        );
        assert_eq!(
            hit("timeout -s KILL 30 gh pr merge 1"),
            Some(("gh", "pr merge".into()))
        );
        assert_eq!(
            hit("powershell -c \"git push\""),
            Some(("git", "push".into()))
        );
        // …and the read-shaped neighbours still pass.
        assert_eq!(hit("bash -e script.sh"), None);
        assert_eq!(hit("gh pr -R o/r view 1"), None);
        assert_eq!(hit("gh --hostname github.com pr list"), None);
        assert_eq!(hit("gh api -XGET repos/o/r"), None);
        assert_eq!(hit("timeout -k 5 30 git fetch"), None);
    }

    #[test]
    fn gh_api_is_judged_by_method_and_body() {
        assert_eq!(hit("gh api repos/o/r/pulls"), None);
        assert_eq!(hit("gh api -X GET repos/o/r"), None);
        assert_eq!(hit("gh api --method=get repos/o/r"), None);
        assert!(hit("gh api -X POST repos/o/r/pulls").is_some());
        assert!(hit("gh api --method PATCH repos/o/r/pulls/1").is_some());
        assert!(hit("gh api repos/o/r/issues -f title=x").is_some());
        assert!(hit("gh api repos/o/r/issues --input body.json").is_some());
        assert!(hit("gh api --method=DELETE repos/o/r").is_some());
    }

    #[test]
    fn reads_and_local_git_pass() {
        for cmd in [
            "git status",
            "git add -A && git commit -m 'x'",
            "git fetch origin && git rebase origin/main",
            "git pull --rebase",
            "git log --oneline -5",
            "git diff HEAD~1",
            "git remote -v",
            "git remote add upstream https://example.invalid/o/r.git",
            "git branch -a",
            "git checkout -b feat/x",
            "git worktree list",
            "git stash push -m wip",
            "git lfs pull",
            "git submodule update --init",
            "gh pr view 258 --json state",
            "gh pr list --state open",
            "gh pr checkout 12",
            "gh pr diff 12",
            "gh pr checks 12",
            "gh issue view 4",
            "gh run view 123 --log",
            "gh repo view o/r",
            "gh repo clone o/r",
            "gh --version",
            "npm test && cargo test",
            "echo 'do not git push from here'", // a quoted sentence is data, not a verb
            "grep -rn 'git push' docs/",
            "git commit -m 'never git push from a seat'",
            "echo \"gh pr create is the deliver phase's job\"",
            "python3 -c 'print(\"git push\")'", // not a SHELL interpreter
            "pushd /tmp; popd",
            "gitk",
            "FOO=1",
        ] {
            assert_eq!(hit(cmd), None, "{cmd}");
        }
    }

    /// Every string the review reproduced as a bypass is a hit for the filter itself; the gate
    /// hook and the ACP bridge tests iterate the same list on their carriers.
    #[test]
    fn every_review_bypass_string_is_a_hit() {
        for cmd in REVIEW_BYPASS_STRINGS {
            assert!(hit(cmd).is_some(), "not refused: {cmd}");
        }
    }

    #[test]
    fn the_reason_names_the_verb_the_segment_and_the_remedy() {
        let h = remote_write_command("cd x && gh pr create --fill").unwrap();
        let reason = h.reason();
        assert!(reason.contains("`gh pr create`"), "{reason}");
        assert!(
            reason.contains("segment: `gh pr create --fill`"),
            "{reason}"
        );
        assert!(reason.contains(REMEDY), "{reason}");
        assert!(
            reason.starts_with(crate::gate_hook::REMOTE_WRITE_REASON_PREFIX),
            "the gate hook routes on this prefix: {reason}"
        );
    }

    #[test]
    fn the_deny_rules_are_claude_bash_prefix_rules() {
        for rule in REMOTE_WRITE_BASH_RULES {
            assert!(
                rule.starts_with("Bash(") && rule.ends_with(')'),
                "a Bash rule: {rule}"
            );
            let inner = &rule["Bash(".len()..rule.len() - 1];
            assert!(
                inner.ends_with(":*") || inner == "git push",
                "prefix form (`:*`) or the bare exact command: {rule}"
            );
            assert!(
                inner.starts_with("git ") || inner.starts_with("gh "),
                "only git/gh verbs are fenced here: {rule}"
            );
        }
        for must in [
            "Bash(git push:*)",
            "Bash(gh pr create:*)",
            "Bash(gh api:*)",
            "Bash(gh release:*)",
            "Bash(gh alias:*)",
            "Bash(git config alias.:*)",
            "Bash(git --config-env:*)",
        ] {
            assert!(REMOTE_WRITE_BASH_RULES.contains(&must), "{must}");
        }
    }

    /// The git builtin allow-list is what makes deny-unknown-verb livable: every builtin of the
    /// git this engine is developed against is present, so a creator's ordinary work is never
    /// refused as an alias.
    #[test]
    fn known_git_subcommands_cover_the_builtins() {
        for verb in [
            "add",
            "commit",
            "checkout",
            "switch",
            "restore",
            "rebase",
            "merge",
            "stash",
            "tag",
            "worktree",
            "sparse-checkout",
            "range-diff",
            "maintenance",
            "bisect",
            "cherry-pick",
            "revert",
            "reset",
            "rev-parse",
            "ls-files",
            "diff",
            "log",
            "show",
            "status",
            "fetch",
            "pull",
            "clone",
            "init",
            "config",
            "remote",
            "submodule",
            "lfs",
            "notes",
            "reflog",
        ] {
            assert!(
                GIT_KNOWN_SUBCOMMANDS.contains(&verb),
                "{verb} must be a known subcommand"
            );
            assert_eq!(hit(&format!("git {verb} --help")), None, "git {verb}");
        }
    }
}
