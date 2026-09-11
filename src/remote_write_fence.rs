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
//!    them (`Bash(<prefix>:*)`, the #434/#436 machinery), joined into the engine's `DENIED_BASH`
//!    so they ride the shared worker `settings.json`, every per-session settings file, the ACP
//!    `session/new` options and the council ballot's `--disallowedTools`.
//! 2. **The command filter** ([`remote_write_command`]) — for the carriers that see the command
//!    text per call: the wrapped carrier's `PreToolUse` gate hook (`gate_hook::boundary_denial`)
//!    and the ACP permission bridge (`acp_runner::answer_permission_request`). A prefix rule is
//!    blind to `cd x && git push`, `git -C <path> push`, `sh -c 'gh pr create …'`; the filter
//!    splits the command on shell separators and judges every segment's program + verb. A refusal
//!    is answered to the seat with [`REMEDY`], disclosed as `workerToolCallDenied`, and logged.
//! 3. **Credential stripping** (`wicked_apps_core::spawn::fence_remote_credentials`) — every seat
//!    spawn runs without `GH_TOKEN`/`GITHUB_TOKEN` and with `gh` aimed at a credential-less
//!    config directory, so even a spelling the filter misses has nothing to authenticate with.
//!    The deliver tool phase applies no seat config and keeps the daemon's login.
//!
//! Stated limit, as everywhere in this codebase: the shell is Turing-complete, so a determined
//! escape (`base64 | sh`, a variable holding the verb) can evade a scan of the literal command.
//! Layer 3 is what holds then; layers 1 and 2 close the direct, observed spellings and name the
//! remedy to the seat.

/// The claude CLI's Bash deny rules for remote-writing `git`/`gh` invocations — PREFIX rules
/// (`Bash(<prefix>:*)`), the one Bash rule form the CLI matches (wicked-crew#524 / F-3R2-004:
/// path-tool rules are `Read(...)`/`Edit(...)`; Bash rules are exact or `:*`-prefixed). Every
/// `gh api` is denied whole: the CLI cannot see the method, and a mutation is one flag away from
/// a read (the command filter below lets a plain `GET` through on the carriers that can judge it).
pub(crate) const REMOTE_WRITE_BASH_RULES: &[&str] = &[
    "Bash(git push:*)",
    "Bash(git push)",
    "Bash(git send-pack:*)",
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
    /// The program judged (`git` or `gh`).
    pub program: &'static str,
    /// The verb path that writes remotely (`push`, `pr create`, `api (mutation)`, …).
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
/// `auth` is denied whole (it mints, prints or moves credentials). Absent subcommands (`pr view`,
/// `pr list`, `pr checkout`, `pr diff`, `pr checks`, `issue view`, `run view`, `repo view`,
/// `repo clone`) are reads or local and pass.
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

/// `gh` subcommands denied WHOLE, whatever follows.
const GH_DENIED_WHOLE: &[&str] = &["auth"];

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

/// Shell interpreters whose `-c`-style argument is a SCRIPT the seat runs — judged recursively.
/// `grep 'git push'` is not one of these, so a quoted sentence is never mistaken for a verb.
const SHELL_INTERPRETERS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "ash",
    "busybox",
    "pwsh",
    "powershell",
    "cmd",
];

/// Judge one shell command: `Some(hit)` names the FIRST remote-writing `git`/`gh` invocation in
/// it, `None` when every segment is a read or local. Pure; case-sensitive on the verb (git and gh
/// are), basename-tolerant on the program (`/usr/bin/git`, `git.exe`). Quotes group tokens (a
/// quoted sentence is data), except as the script argument of a shell interpreter or `eval`,
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

/// Whitespace tokenizer that keeps a quoted string as ONE token (quotes stripped, `\\x` → `x`
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

/// Is `tok` a leading `NAME=value` environment assignment?
fn is_env_assignment(tok: &str) -> bool {
    let Some((name, _)) = tok.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

/// Skip leading env assignments and wrapper programs (with their own options) to the token that
/// is the program being run. Returns the index of that token, or `None` when the segment has
/// no program left (e.g. `FOO=1` alone).
fn program_index(tokens: &[String]) -> Option<usize> {
    let mut i = 0;
    loop {
        while i < tokens.len() && is_env_assignment(&tokens[i]) {
            i += 1;
        }
        let tok = tokens.get(i)?;
        let stem = program_stem(tok);
        if WRAPPERS.contains(&stem.as_str()) {
            i += 1;
            // `timeout <duration>` / `nice -n N` / `stdbuf -oL` / `env -i` / `sudo -u user`:
            // skip the wrapper's own dashed options and their values, and `timeout`'s duration.
            if stem == "timeout" {
                while i < tokens.len() && tokens[i].starts_with('-') {
                    i += 1;
                }
                if i < tokens.len() && !tokens[i].starts_with('-') {
                    i += 1; // the duration
                }
            } else {
                while i < tokens.len() && tokens[i].starts_with('-') {
                    let opt = tokens[i].as_str();
                    i += 1;
                    // Options that take a separate value.
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
        return Some(i);
    }
}

/// The SCRIPT a shell interpreter is asked to run — the argument after a `-c`-style flag (`-c`,
/// `-lc`, `-ic`, `/c`, `/C`, `-Command`, `-command`, `-e`), or `None` when the interpreter is
/// invoked on a file or interactively.
fn interpreter_script(args: &[String]) -> Option<&str> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let is_script_flag = a.eq_ignore_ascii_case("-command")
            || a == "/c"
            || a == "/C"
            || (a.starts_with('-') && !a.starts_with("--") && a[1..].contains('c') && a.len() <= 4)
            || a == "-e";
        if is_script_flag {
            return args.get(i + 1).map(String::as_str);
        }
        i += 1;
    }
    None
}
/// Judge one segment's tokens: the program (after env assignments and wrappers) is `git` or
/// `gh` with a remote-writing verb; or a shell interpreter / `eval` whose script is judged as a
/// command in its own right.
fn judge_tokens(tokens: &[String]) -> Option<(&'static str, String)> {
    let start = program_index(tokens)?;
    let program = program_stem(&tokens[start]);
    let args = &tokens[start + 1..];
    match program.as_str() {
        "git" => judge_git(args).map(|v| ("git", v)),
        "gh" => judge_gh(args).map(|v| ("gh", v)),
        "eval" => {
            let script = args.join(" ");
            remote_write_command(&script).map(|h| (h.program, h.verb))
        }
        p if SHELL_INTERPRETERS.contains(&p) => interpreter_script(args)
            .and_then(remote_write_command)
            .map(|h| (h.program, h.verb)),
        _ => None,
    }
}

/// `git [global options] <subcommand> …` — the remote-writing subcommands are `push` and
/// `send-pack`. Global options that take a value (`-C <path>`, `-c <k=v>`, `--git-dir <d>`,
/// `--work-tree <d>`, `--namespace <n>`, `--exec-path <p>`) are skipped with their value; the
/// `=`-attached forms and bare flags (`--no-pager`, `-p`, `--bare`) are skipped alone.
fn judge_git(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let tok = args[i].as_str();
        if !tok.starts_with('-') {
            return match tok {
                "push" | "send-pack" => Some(tok.to_string()),
                _ => None,
            };
        }
        let takes_value = matches!(
            tok,
            "-C" | "-c"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--exec-path"
                | "--super-prefix"
                | "--config-env"
                | "--list-cmds"
        );
        i += if takes_value { 2 } else { 1 };
    }
    None
}

/// `gh [global options] <command> [<subcommand>] …`.
fn judge_gh(args: &[String]) -> Option<String> {
    let mut i = 0;
    // gh's global options: `-R/--repo <owner/repo>` takes a value; `--help`/`--version` do not.
    while i < args.len() && args[i].starts_with('-') {
        let tok = args[i].as_str();
        i += if matches!(tok, "-R" | "--repo") { 2 } else { 1 };
    }
    let command = args.get(i)?.as_str();
    if GH_DENIED_WHOLE.contains(&command) {
        return Some(command.to_string());
    }
    if command == "api" {
        return judge_gh_api(&args[i + 1..]).then(|| "api (mutation)".to_string());
    }
    let (_, verbs) = GH_REMOTE_WRITE_VERBS.iter().find(|(c, _)| *c == command)?;
    // The verb is the first non-flag token after the command (gh accepts `pr -R x create`).
    let verb = args[i + 1..].iter().find(|t| !t.starts_with('-'))?.as_str();
    verbs.contains(&verb).then(|| format!("{command} {verb}"))
}

/// `gh api` mutates when it names a non-GET method or carries a body: `-X/--method <M>` with
/// `M != GET`, `--method=<M>`, or any of `-f/-F/--field/--raw-field/--input` (which imply
/// `POST`). A plain `gh api repos/o/r/pulls` is a read and passes.
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
        if let Some(m) = tok.strip_prefix("--method=") {
            if !m.eq_ignore_ascii_case("GET") {
                return true;
            }
        }
        if tok.starts_with("--field=")
            || tok.starts_with("--raw-field=")
            || tok.starts_with("--input=")
            || tok.starts_with("-f=")
            || tok.starts_with("-F=")
        {
            return true;
        }
        i += 1;
    }
    false
}

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
            Some(("git", "push".into()))
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
            "git branch -a",
            "git checkout -b feat/x",
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
        assert!(REMOTE_WRITE_BASH_RULES.contains(&"Bash(git push:*)"));
        assert!(REMOTE_WRITE_BASH_RULES.contains(&"Bash(gh pr create:*)"));
        assert!(REMOTE_WRITE_BASH_RULES.contains(&"Bash(gh api:*)"));
        assert!(REMOTE_WRITE_BASH_RULES.contains(&"Bash(gh release:*)"));
    }
}
