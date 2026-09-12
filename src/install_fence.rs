//! Package-manager install fence (F-E2E-029): a worker seat's dependency install must land in the
//! run's worktree — never in the customer's clone root, never in another checkout.
//!
//! During run `01234444` (wicked-studio, `bug` workflow) the creator's fix unit ran on the claude
//! ACP seat, whose registry record arms no OS write boundary (`os_sandbox: false`); the ACP
//! permission bridge judges `fs/write_text_file` PATHS against the unit's posture and `execute`
//! commands against the remote-write fence, but nothing judged WHERE a shell command would put its
//! bytes. 194 MB of `node_modules/` (`.package-lock.json` 12:19:25Z) appeared in the CUSTOMER'S
//! CLONE ROOT — the worktree's parent — with no run event recording it. Whether the seat `cd`'d to
//! the clone root (the worktree's `.git` file names it) or passed `--prefix`, the hole is the same:
//! an install is a large, hard-to-reverse write whose target the command names, and the run's
//! write boundary is the worktree.
//!
//! This module is the command-text half of that fence, on the two carriers that see the text per
//! call (the wrapped claude carrier's `PreToolUse` hook and the ACP permission bridge — the same
//! seams as [`crate::remote_write_fence`]). It follows `cd`/`pushd` across a command's segments
//! from the unit's cwd, resolves the package manager's own target-directory flags (`npm --prefix`,
//! `pnpm -C/--dir`, `yarn --cwd`, `bun --cwd`), and refuses a MUTATING invocation of `npm`, `pnpm`,
//! `yarn` or `bun` whose effective directory is outside the worktree. Reads (`npm ls`, `npm view`),
//! scripts (`npm run`, `npm test`) and installs INTO the worktree (any depth) pass. A refusal is
//! ADVISORY (one tool call, not the unit): the seat gets the remedy and continues, and the call is
//! disclosed as `workerToolCallDenied`. Same honest limit as every command filter here: a
//! Turing-complete shell can evade a scan of the literal text; OS containment is the hermetic
//! guarantee, and the seats that have it armed never reach this rule with an escape.

use std::path::{Component, Path, PathBuf};

use crate::remote_write_fence::{program_index, program_stem, sh_script, split_segments, tokenize};

/// The leading text every install-fence refusal reason carries — the hook routes on it.
pub(crate) const REASON_PREFIX: &str = "install fence:";

/// The remedy every refusal carries to the seat and onto the wire (`workerToolCallDenied.remedy`).
pub(crate) const REMEDY: &str = "install dependencies in the run's worktree (the unit's working \
    directory) and nowhere else — the engine provisions the worktree's own dependencies before it \
    runs the repository's checks, so a hollow tree is not yours to fix by installing into the \
    source checkout or another directory; never `cd` out of the worktree to install";

/// One foreign-targeted install the filter found in a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstallHit {
    /// The package manager (`npm`, `pnpm`, `yarn`, `bun`).
    pub program: String,
    /// The mutating verb (`ci`, `install`, `add`, …).
    pub verb: String,
    /// The directory the install would land in, resolved.
    pub target: PathBuf,
    /// The command segment the hit was found in (trimmed).
    pub segment: String,
}

impl InstallHit {
    /// The operator- and seat-facing reason for the refusal (the `reason` on the wire).
    pub(crate) fn reason(&self) -> String {
        format!(
            "{REASON_PREFIX} `{} {}` would install into `{}`, outside the run's worktree \
             (segment: `{}`) — {REMEDY}",
            self.program,
            self.verb,
            self.target.display(),
            self.segment
        )
    }
}

/// Judge `command`, run by a seat whose working directory is `cwd` (the run worktree): `Some(hit)`
/// when a mutating package-manager invocation would install OUTSIDE `cwd`, following `cd`/`pushd`
/// across segments and the managers' own directory flags; `None` otherwise. `home` expands `~`.
pub(crate) fn foreign_install(
    command: &str,
    cwd: &Path,
    home: Option<&Path>,
) -> Option<InstallHit> {
    let mut here = cwd.to_path_buf();
    judge_script(command, cwd, &mut here, home)
}

/// A top-level piece of a script: plain text (segments to judge in order), or a parenthesised
/// group — a subshell `( … )` / a substitution `$( … )` — whose `cd`s must not leak into the
/// parent's later segments.
enum Piece {
    Text(String),
    Group(String),
}

/// Split `script` into [`Piece`]s at balanced top-level parentheses outside quotes (a backslash
/// escapes the next character outside single quotes). Unbalanced text is judged as plain text.
fn split_groups(script: &str) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    let mut chars = script.chars().peekable();
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
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '\\' => {
                    cur.push(c);
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                }
                '(' => {
                    if depth == 0 {
                        if !cur.trim().is_empty() {
                            out.push(Piece::Text(std::mem::take(&mut cur)));
                        } else {
                            cur.clear();
                        }
                    } else {
                        cur.push(c);
                    }
                    depth += 1;
                }
                ')' if depth > 0 => {
                    depth -= 1;
                    if depth == 0 {
                        out.push(Piece::Group(std::mem::take(&mut cur)));
                    } else {
                        cur.push(c);
                    }
                }
                _ => cur.push(c),
            },
        }
    }
    if !cur.trim().is_empty() {
        out.push(if depth > 0 {
            Piece::Group(cur)
        } else {
            Piece::Text(cur)
        });
    }
    out
}

fn judge_script(
    script: &str,
    worktree: &Path,
    here: &mut PathBuf,
    home: Option<&Path>,
) -> Option<InstallHit> {
    for piece in split_groups(script) {
        match piece {
            Piece::Group(inner) => {
                // A subshell starts where its parent stands and its `cd`s stay inside it.
                let mut sub = here.clone();
                if let Some(hit) = judge_script(&inner, worktree, &mut sub, home) {
                    return Some(hit);
                }
            }
            Piece::Text(text) => {
                if let Some(hit) = judge_segments(&text, worktree, here, home) {
                    return Some(hit);
                }
            }
        }
    }
    None
}

fn judge_segments(
    script: &str,
    worktree: &Path,
    here: &mut PathBuf,
    home: Option<&Path>,
) -> Option<InstallHit> {
    for segment in split_segments(script) {
        let tokens = tokenize(&segment);
        let Some((start, _assigned)) = program_index(&tokens) else {
            continue;
        };
        let program = program_stem(&tokens[start]);
        let args = &tokens[start + 1..];
        match program.as_str() {
            "cd" | "pushd" => {
                // `cd` alone / `cd ~` → home; `cd -` → unknown (leave `here` as is: the fence
                // cannot know the previous directory, and a stale `here` errs towards the
                // worktree, which is where the unit started).
                let dest = args
                    .iter()
                    .find(|a| !a.starts_with('-') || a.as_str() == "-");
                match dest.map(String::as_str) {
                    None | Some("~") => {
                        if let Some(h) = home {
                            *here = h.to_path_buf();
                        }
                    }
                    Some("-") => {}
                    Some(d) => *here = resolve(here, d, home),
                }
            }
            "npm" | "pnpm" | "yarn" | "bun" => {
                if let Some((verb, dir)) = mutating_install(&program, args) {
                    let target = match dir {
                        Some(d) => resolve(here, &d, home),
                        None => here.clone(),
                    };
                    if !inside(&target, worktree) {
                        return Some(InstallHit {
                            program: program.clone(),
                            verb,
                            target,
                            segment: segment.trim().to_string(),
                        });
                    }
                }
            }
            "eval" => {
                let inner = args.join(" ");
                if let Some(hit) = judge_script(&inner, worktree, here, home) {
                    return Some(hit);
                }
            }
            p if ["sh", "bash", "zsh", "dash", "ksh", "ash", "busybox"].contains(&p) => {
                let args = match args.first().map(|a| program_stem(a)) {
                    Some(applet)
                        if p == "busybox"
                            && ["sh", "bash", "ash", "dash"].contains(&applet.as_str()) =>
                    {
                        &args[1..]
                    }
                    _ => args,
                };
                if let Some(inner) = sh_script(args) {
                    // A subshell script starts where its parent stands; its `cd`s stay inside it.
                    let mut sub = here.clone();
                    if let Some(hit) = judge_script(inner, worktree, &mut sub, home) {
                        return Some(hit);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// The mutating verb and the explicit target directory (if any) of a package-manager invocation,
/// or `None` for a read, a script run, or a non-mutating verb.
fn mutating_install(program: &str, args: &[String]) -> Option<(String, Option<String>)> {
    let (dir_flags, verbs, bare_is_install): (&[&str], &[&str], bool) = match program {
        "npm" => (
            &["--prefix", "-C"],
            &[
                "install",
                "i",
                "in",
                "ins",
                "inst",
                "isnt",
                "isntall",
                "add",
                "ci",
                "clean-install",
                "install-clean",
                "isntall-clean",
                "install-ci-test",
                "cit",
                "clean-install-test",
                "sit",
                "install-test",
                "it",
                "uninstall",
                "un",
                "unlink",
                "remove",
                "rm",
                "r",
                "update",
                "up",
                "upgrade",
                "udpate",
                "dedupe",
                "ddp",
                "prune",
                "rebuild",
                "rb",
                "link",
                "ln",
            ],
            false,
        ),
        "pnpm" => (
            &["-C", "--dir"],
            &[
                "install",
                "i",
                "add",
                "remove",
                "rm",
                "uninstall",
                "un",
                "update",
                "up",
                "upgrade",
                "dedupe",
                "prune",
                "link",
                "ln",
                "unlink",
                "rebuild",
                "rb",
                "import",
                "fetch",
                "install-test",
                "it",
            ],
            false,
        ),
        "yarn" => (
            &["--cwd", "--modules-folder"],
            &[
                "install",
                "add",
                "remove",
                "upgrade",
                "upgrade-interactive",
                "up",
                "dedupe",
                "link",
                "unlink",
                "import",
                "rebuild",
                "workspaces",
            ],
            true,
        ),
        "bun" => (
            &["--cwd"],
            &[
                "install", "i", "add", "a", "remove", "rm", "update", "link", "unlink", "pm",
            ],
            true,
        ),
        _ => return None,
    };
    let mut dir: Option<String> = None;
    let mut verb: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let tok = args[i].as_str();
        if let Some((flag, value)) = tok.split_once('=') {
            if dir_flags.contains(&flag) {
                dir = Some(value.to_string());
            }
            i += 1;
            continue;
        }
        if dir_flags.contains(&tok) {
            if let Some(v) = args.get(i + 1) {
                dir = Some(v.clone());
            }
            i += 2;
            continue;
        }
        if tok == "--" {
            break;
        }
        if tok.starts_with('-') {
            i += 1;
            continue;
        }
        if verb.is_none() {
            verb = Some(tok.to_string());
        }
        i += 1;
    }
    match verb {
        Some(v) if verbs.contains(&v.as_str()) => Some((v, dir)),
        None if bare_is_install => Some(("install".to_string(), dir)),
        _ => None,
    }
}

/// `path` resolved against `base` (a `~` against `home`), lexically normalised (`.`/`..` folded),
/// and canonicalised when it exists so `/tmp/x` and `/private/tmp/x` compare equal on macOS.
fn resolve(base: &Path, path: &str, home: Option<&Path>) -> PathBuf {
    let raw = if let Some(rest) = path.strip_prefix("~/") {
        home.map_or_else(|| PathBuf::from(path), |h| h.join(rest))
    } else if path == "~" {
        home.map_or_else(|| PathBuf::from(path), Path::to_path_buf)
    } else if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for c in raw.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    std::fs::canonicalize(&out).unwrap_or(out)
}

/// Whether `target` is `worktree` or under it, comparing canonical spellings when both exist.
fn inside(target: &Path, worktree: &Path) -> bool {
    let wt = std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
    let t = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    t.starts_with(&wt)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch worktree at `<tmp>/<name>/repo/wicked-worktrees/run-1` — the engine's layout,
    /// the parent `repo` standing for the customer's clone root. No shell metacharacters in the
    /// name: the paths are spliced into command text unquoted, as a seat would type them.
    fn wt(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wicked-install-fence-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("repo/wicked-worktrees/run-1/sub")).unwrap();
        std::fs::canonicalize(dir.join("repo/wicked-worktrees/run-1")).unwrap()
    }

    fn judge(cmd: &str, wt: &Path) -> Option<InstallHit> {
        foreign_install(cmd, wt, Some(Path::new("/home/seat")))
    }

    #[test]
    fn a_cd_to_the_clone_root_followed_by_an_install_is_refused_and_names_the_target() {
        let wt = wt("clone-root");
        let clone = wt.parent().unwrap().parent().unwrap().to_path_buf();
        let hit = judge(&format!("cd {} && npm ci", clone.display()), &wt).expect("refused");
        assert_eq!(hit.program, "npm");
        assert_eq!(hit.verb, "ci");
        assert_eq!(hit.target, clone);
        let reason = hit.reason();
        assert!(reason.starts_with(REASON_PREFIX), "{reason}");
        assert!(reason.contains(REMEDY), "{reason}");
        // Relative spellings of the same escape.
        assert!(judge("cd ../.. && npm install", &wt).is_some());
        assert!(judge("cd .. ; npm i", &wt).is_some());
        assert!(judge("pushd ../..; pnpm install", &wt).is_some());
        // The manager's own target flags.
        assert!(judge("npm ci --prefix ../..", &wt).is_some());
        assert!(judge("npm --prefix=../../ install", &wt).is_some());
        assert!(judge("pnpm -C ../.. add left-pad", &wt).is_some());
        assert!(judge(&format!("pnpm --dir {} install", clone.display()), &wt).is_some());
        assert!(
            judge("yarn --cwd ../..", &wt).is_some(),
            "bare yarn is an install"
        );
        assert!(judge("yarn --cwd ../.. add x", &wt).is_some());
        assert!(judge("bun install --cwd ../..", &wt).is_some());
        // Through a wrapper, a subshell, an env prefix, eval.
        assert!(judge("cd ../.. && env CI=1 npm ci", &wt).is_some());
        assert!(judge("bash -lc 'cd ../.. && npm ci'", &wt).is_some());
        assert!(judge("eval cd ../.. '&&' npm ci", &wt).is_some());
        assert!(
            judge("cd ~ && npm install", &wt).is_some(),
            "home is outside"
        );
    }

    #[test]
    fn installs_inside_the_worktree_reads_and_script_runs_pass() {
        let wt = wt("inside");
        assert_eq!(judge("npm ci", &wt), None);
        assert_eq!(judge("npm ci --ignore-scripts && npm test", &wt), None);
        assert_eq!(judge("cd sub && npm install", &wt), None);
        assert_eq!(judge("cd sub/.. && npm install", &wt), None);
        assert_eq!(judge(&format!("cd {} && npm ci", wt.display()), &wt), None);
        assert_eq!(judge("npm install --prefix ./sub", &wt), None);
        assert_eq!(judge("pnpm -C sub install", &wt), None);
        // Reads, scripts and non-install verbs anywhere.
        assert_eq!(judge("cd ../.. && npm ls wicked-crew-api-types", &wt), None);
        assert_eq!(judge("cd ../.. && npm run build", &wt), None);
        assert_eq!(judge("cd ../.. && npm test", &wt), None);
        assert_eq!(judge("cd ../.. && npm view vitest version", &wt), None);
        assert_eq!(judge("cd ../.. && git status", &wt), None);
        assert_eq!(
            judge("echo 'cd ../.. && npm ci'", &wt),
            None,
            "quoted data is not a command"
        );
        assert_eq!(judge("grep -r 'npm install' docs", &wt), None);
        // A subshell's `cd` does not leak into the parent's next segment…
        assert_eq!(judge("(cd ../.. && npm ls) && npm ci", &wt), None);
        assert_eq!(judge("bash -c 'cd ../.. && npm ls' && npm ci", &wt), None);
        assert_eq!(judge("echo $(cd ../.. && npm ls) && npm ci", &wt), None);
        // …while an install INSIDE the subshell is still judged where the subshell stands.
        assert!(judge("(cd ../.. && npm ci)", &wt).is_some());
        assert!(judge("cd sub && (cd ../../.. && npm ci)", &wt).is_some());
    }

    #[test]
    fn the_worktree_itself_and_its_parent_compare_canonically() {
        // A symlinked spelling of a directory INSIDE the worktree is inside.
        let wt = wt("canonical");
        let link = wt.parent().unwrap().join("alias-run-1");
        let _ = std::fs::remove_file(&link);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&wt, &link).unwrap();
            assert_eq!(
                judge(&format!("cd {} && npm ci", link.display()), &wt),
                None,
                "a symlink INTO the worktree resolves inside"
            );
        }
    }
}
