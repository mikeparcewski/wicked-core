//! OPERATOR-CONFIG FENCE (core#803) — a worker seat never installs an MCP server, never runs the
//! installer that writes the operator's CLI configurations, and never names those configuration
//! files from its shell.
//!
//! ## The defect this closes
//!
//! On the `mcp-server` dogfood run `c3aa0bfb` an operator's Send back on the failed `install`
//! Tool unit was routed to the build creator (fixed separately: a Tool phase is now its own
//! send-back target). The creator read "retry the install" as an instruction and ran garden's
//! `scripts/mcp/install.py --from-run` from its own shell. The script staged
//! `~/.wicked/mcp-servers/<key>` and ran `wicked-installer mcp upsert … --cli all`, which wrote
//! `~/.codex/config.toml`, the opencode configuration and the seat's `.claude.json`. The gate hook
//! saw both Bash calls and allowed them (`firedPolicies: []`): the boundary check judges a tool's
//! PATH argument and a shell command's literal write targets (`>`, `tee`, `cp`, …), and these
//! writes were made by a child process the command only NAMED. The claude seat carries no OS
//! write boundary (`sandboxPosture: not on the OS-sandbox floor`), so nothing else stood between
//! the seat and the operator's CLI configuration.
//!
//! ## What is refused
//!
//! Judged per shell segment (quote-aware, the shared [`crate::remote_write_fence`] tokenizer),
//! looking through `sh -c` / `bash -c` scripts and the usual wrappers (`env`, `timeout`, `nice`):
//!
//! 1. garden's MCP install script (`scripts/mcp/install.py`) in any position — it is the
//!    workflow's consent-gated Tool phase, never a seat's;
//! 2. `wicked-installer`, run directly or through a package runner (`npx`, `pnpx`, `bunx`,
//!    `npm exec`, `pnpm dlx`, `yarn dlx`) — it writes the operator's CLI configurations;
//! 3. a coding-agent CLI's `mcp` subcommand other than a read (`claude mcp add`, `codex mcp add`,
//!    `opencode mcp …`): each writes that CLI's configuration;
//! 4. a path to a known CLI configuration file or directory rooted OUTSIDE the worktree — `~`,
//!    `$HOME`, another `$VAR`, or an absolute path not under the worktree — whatever the verb:
//!    a read is refused too, because those files hold the operator's tokens and server entries.
//!    A worktree-relative path with the same name (a test fixture) passes.
//!
//! ADVISORY like the remote-write and install fences: the one call is blocked and audited, the
//! seat is told the remedy and continues, and the fold discloses it as `workerToolCallDenied`.
//! Same stated limit as every literal scan here: a script file, a variable holding the program,
//! or `base64 | sh` can evade it; the OS sandbox is the hermetic layer, which this seat lacks.

use std::path::{Component, Path, PathBuf};

use crate::remote_write_fence::{program_index, program_stem, sh_script, split_segments, tokenize};

/// The leading text every refusal reason carries — the hook and the fold route on it.
pub(crate) const REASON_PREFIX: &str = "operator-config fence:";

/// The remedy every refusal carries to the seat and onto the wire.
pub(crate) const REMEDY: &str = "a governed unit never installs or registers an MCP server and \
    never reads or writes a coding-agent CLI's configuration on the operator's machine — the \
    workflow's install phase does that, and only after the operator consents. Build and test \
    inside the run's worktree and say in your output what should be installed";

/// The CLI configuration files and directories a seat may not name outside the worktree. Matched
/// as a path SUFFIX or an inner segment of the normalised token (forward slashes).
const OPERATOR_CONFIG_PATHS: &[&str] = &[
    "/.codex/config.toml",
    "/.claude.json",
    "/.claude/settings.json",
    "/.config/opencode/",
    "/.config/opencode",
    "/.gemini/settings.json",
    "/.gemini/antigravity/",
    "/.cursor/mcp.json",
    "/.copilot/mcp-config.json",
    "/.pi/agent/",
    "/.wicked/mcp-servers",
];

/// Coding-agent CLIs whose `mcp` subcommand writes their configuration.
const AGENT_CLIS: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "gemini",
    "agy",
    "antigravity",
    "copilot",
    "cursor-agent",
    "pi",
    "qwen",
];

/// `mcp` verbs that only read.
const MCP_READ_VERBS: &[&str] = &["list", "ls", "get", "show", "help", "--help", "-h"];

/// Package runners that execute their first non-flag argument as a program.
const PACKAGE_RUNNERS: &[&str] = &["npx", "pnpx", "bunx"];

/// Shells whose `-c` script is judged as a command in its own right.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh"];

/// One refused invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OperatorConfigHit {
    /// What was refused, in a few words (`wicked-installer`, `scripts/mcp/install.py`,
    /// `codex mcp add`, `~/.codex/config.toml`).
    pub what: String,
    /// The command segment the hit was found in (trimmed).
    pub segment: String,
}

impl OperatorConfigHit {
    /// The operator- and seat-facing reason for the refusal (the `reason` on the wire).
    pub(crate) fn reason(&self) -> String {
        format!(
            "{REASON_PREFIX} `{}` installs, registers or reads an operator CLI configuration, \
             which a worker seat may not do (segment: `{}`) — {REMEDY}",
            self.what, self.segment
        )
    }
}

/// Judge `command` for a seat whose worktree is `worktree`; `home` expands `~`. `Some` when the
/// command must be refused.
///
/// `here` is where the seat's shell stands (the install fence's tracked cwd; the worktree at a
/// unit's first call): a relative path is resolved from it, lexically (`..` folded), and so is
/// every absolute one, before it is compared with the worktree.
pub(crate) fn judge(
    command: &str,
    worktree: &Path,
    here: &Path,
    home: Option<&Path>,
) -> Option<OperatorConfigHit> {
    let normalised;
    let command = if cfg!(windows) {
        normalised = command.replace('\\', "/");
        normalised.as_str()
    } else {
        command
    };
    let ctx = Ctx {
        worktree: normalize(worktree),
        here: normalize(here),
        home: home.map(normalize),
        // A `cd`/`pushd` anywhere in the command moves the shell where this literal scan cannot
        // follow every spelling: a RELATIVE config path is then judged as if outside (fail closed).
        moves: false,
    };
    judge_script(command, &ctx, 0)
}

/// Whether `script` moves the shell (`cd`/`pushd`/`popd` in any segment).
fn script_moves(script: &str) -> bool {
    split_segments(script).iter().any(|seg| {
        let t = tokenize(seg);
        program_index(&t)
            .is_some_and(|(i, _)| matches!(program_stem(&t[i]).as_str(), "cd" | "pushd" | "popd"))
    })
}

struct Ctx {
    worktree: PathBuf,
    here: PathBuf,
    home: Option<PathBuf>,
    moves: bool,
}

fn judge_script(script: &str, outer: &Ctx, depth: u8) -> Option<OperatorConfigHit> {
    // Movement is judged per script, inner `sh -c` strings included (codex round 2:
    // `bash -c 'cd ~ && cat .codex/config.toml'`); an outer move still covers the inner script.
    let ctx = &Ctx {
        worktree: outer.worktree.clone(),
        here: outer.here.clone(),
        home: outer.home.clone(),
        moves: outer.moves || script_moves(script),
    };
    for segment in split_segments(script) {
        let tokens = tokenize(&segment);
        let hit = |what: String| {
            Some(OperatorConfigHit {
                what,
                segment: segment.trim().to_string(),
            })
        };
        // (1) + (4): any token, whatever its position — an argument, a redirect target.
        for tok in &tokens {
            let path = strip_redirect(tok);
            if names_install_script(path) {
                return hit("scripts/mcp/install.py".to_string());
            }
            if let Some(marker) = names_operator_config(path, ctx) {
                return hit(format!("{path} ({})", marker.trim_matches('/')));
            }
        }
        let Some((start, _)) = program_index(&tokens) else {
            continue;
        };
        let program = program_stem(&tokens[start]);
        let args = &tokens[start + 1..];
        // A shell's `-c` script, and `eval`'s words, are judged as commands of their own.
        if depth < 3 {
            let inner = if SHELLS.contains(&program.as_str()) {
                sh_script(args).map(str::to_string)
            } else if program == "eval" {
                Some(args.join(" "))
            } else {
                None
            };
            if let Some(inner) = inner {
                if let Some(h) = judge_script(&inner, ctx, depth + 1) {
                    return Some(h);
                }
            }
        }
        // (2) wicked-installer, directly or through a package runner.
        if is_installer(&program) {
            return hit("wicked-installer".to_string());
        }
        if let Some(pkg) = runner_package(&program, args) {
            if is_installer(&program_stem(pkg)) {
                return hit(format!("{program} wicked-installer"));
            }
        }
        // (3) a coding-agent CLI's mcp mutation. `mcp` is looked for ANYWHERE in the argv, not
        // as the first non-flag word: a global option's value (`codex -c model=o3 mcp add`)
        // would otherwise stand in front of it (codex review).
        if AGENT_CLIS.contains(&program.as_str()) {
            if let Some(at) = args.iter().position(|a| a == "mcp") {
                if let Some(verb) = args[at + 1..].iter().find(|a| !a.starts_with('-')) {
                    if !MCP_READ_VERBS.contains(&verb.as_str()) {
                        return hit(format!("{program} mcp {verb}"));
                    }
                }
            }
        }
    }
    None
}

/// A redirect operator glued to its target (`>~/.codex/config.toml`, `2>>x`) leaves the target.
fn strip_redirect(tok: &str) -> &str {
    tok.trim_start_matches(|c: char| c.is_ascii_digit() || c == '&')
        .trim_start_matches(['>', '<'])
}

fn names_install_script(tok: &str) -> bool {
    let t = tok.replace('\\', "/");
    t == "mcp/install.py" || t.ends_with("/mcp/install.py")
}

/// `wicked-installer`, `wicked-installer@0.4.2`, `wicked-installer.cmd`.
fn is_installer(stem: &str) -> bool {
    let name = stem.split('@').next().unwrap_or(stem);
    name == "wicked-installer"
}

/// The package a runner executes: `npx [-y] <pkg>`, `npm exec [--] <pkg>`, `pnpm dlx <pkg>`,
/// `yarn dlx <pkg>`.
fn runner_package<'a>(program: &str, args: &'a [String]) -> Option<&'a str> {
    let rest: &[String] = if PACKAGE_RUNNERS.contains(&program) {
        args
    } else if matches!(program, "npm" | "pnpm" | "yarn" | "bun") {
        let verb_ix = args.iter().position(|a| !a.starts_with('-'))?;
        if !matches!(args[verb_ix].as_str(), "exec" | "dlx" | "x") {
            return None;
        }
        &args[verb_ix + 1..]
    } else {
        return None;
    };
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        if a == "--" {
            i += 1;
            continue;
        }
        if matches!(a, "-p" | "--package") {
            // `npx -p wicked-installer wicked-installer …`: the package named by the flag counts.
            if let Some(p) = rest.get(i + 1) {
                if is_installer(&program_stem(p)) {
                    return Some(p.as_str());
                }
            }
            i += 2;
            continue;
        }
        if let Some(p) = a.strip_prefix("--package=") {
            if is_installer(&program_stem(p)) {
                return Some(p);
            }
            i += 1;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        return Some(a);
    }
    None
}

/// The configuration marker a token names when it resolves OUTSIDE the worktree.
fn names_operator_config(tok: &str, ctx: &Ctx) -> Option<&'static str> {
    let t = tok.replace('\\', "/");
    // `~`, `$HOME`, `${HOME}` resolve to the home directory; any other `$VAR` root is unknown and
    // therefore outside.
    let resolved: Option<PathBuf> = if let Some(rest) = t
        .strip_prefix("~/")
        .or_else(|| t.strip_prefix("$HOME/"))
        .or_else(|| t.strip_prefix("${HOME}/"))
    {
        ctx.home.as_ref().map(|h| normalize(&h.join(rest)))
    } else if t.starts_with('$') {
        None
    } else if t.starts_with('/') || has_drive_prefix(&t) {
        Some(normalize(Path::new(&t)))
    } else {
        Some(normalize(&ctx.here.join(&t)))
    };
    let shown = resolved
        .as_ref()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| t.clone());
    let probe = format!("/{}", shown.trim_start_matches('/'));
    let marker = OPERATOR_CONFIG_PATHS
        .iter()
        .find(|m| probe.contains(*m) || probe.ends_with(m.trim_end_matches('/')))?;
    let relative =
        !(t.starts_with('~') || t.starts_with('$') || t.starts_with('/') || has_drive_prefix(&t));
    let outside = match &resolved {
        None => true,
        Some(p) => !p.starts_with(&ctx.worktree) || (relative && ctx.moves),
    };
    outside.then_some(*marker)
}

/// Lexical normalisation: `.` dropped, `..` pops (never above the root). No filesystem access.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn has_drive_prefix(t: &str) -> bool {
    let b = t.as_bytes();
    b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/'
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn wt() -> PathBuf {
        PathBuf::from("/runs/wt/abc")
    }

    fn home() -> PathBuf {
        PathBuf::from("/home/op")
    }

    fn j(cmd: &str) -> Option<OperatorConfigHit> {
        judge(cmd, &wt(), &wt(), Some(&home()))
    }

    /// The dogfood's two creator calls (run c3aa0bfb), and the spellings around them.
    #[test]
    fn the_install_script_and_the_installer_are_refused_in_every_spelling() {
        for cmd in [
            "wicked-garden run scripts/mcp/install.py --from-run --json",
            "WICKED_GARDEN_ROOT=/snap/000008 \"$WICKED_GARDEN_ROOT/scripts/wicked-garden\" run scripts/mcp/install.py --from-run",
            "cd /snap && python3 scripts/mcp/install.py --from-run",
            "bash -c 'wicked-garden run scripts/mcp/install.py --from-run'",
            "wicked-installer mcp upsert petstore --cli all",
            "npx -y wicked-installer@latest mcp upsert petstore --cli all",
            "npm exec -- wicked-installer mcp upsert x",
            "pnpm dlx wicked-installer mcp upsert x",
            "npx -p wicked-installer wicked-installer mcp upsert x",
            "timeout 60 env FOO=1 wicked-installer mcp upsert x",
            "sh -c \"cd /tmp && npx wicked-installer mcp upsert x\"",
        ] {
            let hit = j(cmd).unwrap_or_else(|| panic!("refused: {cmd}"));
            assert!(hit.reason().starts_with(REASON_PREFIX), "{cmd}");
            assert!(hit.reason().contains(REMEDY), "{cmd}");
        }
    }

    #[test]
    fn agent_cli_mcp_mutations_are_refused_and_reads_pass() {
        for cmd in [
            "claude mcp add petstore -- node dist/index.js",
            "codex mcp add petstore -- node dist/index.js",
            "opencode mcp add",
            "gemini mcp remove petstore",
            "/usr/local/bin/claude mcp add-json x '{}'",
            // codex review: a global option's value in front of `mcp`
            "CODEX_HOME=/home/op/.codex codex -c model='\"o3\"' mcp add x -- node server.js",
            "claude --model opus mcp add x",
        ] {
            assert!(j(cmd).is_some(), "refused: {cmd}");
        }
        for cmd in [
            "claude mcp list",
            "codex mcp get petstore",
            "codex --version",
            "codex mcp",
            "claude -p 'hello'",
        ] {
            assert_eq!(j(cmd), None, "passes: {cmd}");
        }
    }

    #[test]
    fn operator_config_paths_outside_the_worktree_are_refused_inside_it_they_pass() {
        for cmd in [
            "cat ~/.codex/config.toml",
            "echo x >> ~/.codex/config.toml",
            "echo x >~/.claude.json",
            "sed -i '' 's/a/b/' $HOME/.claude.json",
            "cp server.json ${HOME}/.config/opencode/opencode.jsonc",
            "ls /home/op/.wicked/mcp-servers/petstore",
            "cat \"$CLAUDE_CONFIG_DIR/.claude.json\"",
            "python3 -c 'print(1)' > /other/.codex/config.toml",
            // codex review: traversal out of the worktree, absolute and relative
            "cat /runs/wt/abc/../../../home/op/.codex/config.toml",
            "cat ../../../home/op/.claude.json",
            "cd ~ && cat .codex/config.toml",
            "bash -c 'cd ~ && cat .codex/config.toml'",
        ] {
            assert!(j(cmd).is_some(), "refused: {cmd}");
        }
        for cmd in [
            "cat tests/fixtures/.claude.json",
            "cat /runs/wt/abc/tests/fixtures/.codex/config.toml",
            "rg '.claude.json' src/",
            "npm test",
            "git status",
            "wicked-garden run scripts/mcp/scaffold.py --lang typescript",
        ] {
            assert_eq!(j(cmd), None, "passes: {cmd}");
        }
    }
}
