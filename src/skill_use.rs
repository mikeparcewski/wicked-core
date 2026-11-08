//! Which skills a seat actually uses (operator 2026-11-08: "we should be recording skills usage -
//! that way we know which ones we can turn off over time"). Design: program lane skill-usage,
//! `notes/DESIGN.md`.
//!
//! The engine already says what a seat was HANDED (`skillsSnapshotHanded`, which now carries the
//! names the lever delivered). This module records the other signals, one
//! [`CoreEvent::SkillUsed`] each:
//!
//! - `directed`: the unit's directive named the skill ([`directed_events`]: the phase `skill_ref`,
//!   the run's base skill, the required QE skill);
//! - `opened`: the seat loaded the skill's instructions: Claude's `Skill` tool (its delivered
//!   `wicked-garden:<top-level dir>` identity), opencode's `skill` tool (a delivered name), or a
//!   read of a FILE inside a skill directory the seat was handed (a read tool, or a reading shell
//!   command naming the file), and only once the call succeeded;
//! - `browsed`: ONE shell command named files from more than [`BROWSE_OVER`] distinct skills (a
//!   `head` over many `SKILL.md`s, a loop): each is recorded as browsed, which is not use;
//! - `ran`: the seat ran a garden script: the launcher (`wicked-garden run scripts/<rel>`, the bare
//!   name or the delivered `scripts/wicked-garden`), an interpreter on a snapshot script, or the
//!   script path itself as the program. Core names the script only, and only a script that exists
//!   in the snapshot; crew attributes it to a skill through the generation's own files.
//!
//! NAMES ONLY: a skill's frontmatter name and a snapshot-relative `scripts/<rel>` checked against
//! the snapshot's own files. Never a host path, an argument or any content.
//!
//! Detection reads the seat's own tool stream (every adapter hands its calls to [`SkillUseSink`] in
//! claude's tool words) BEFORE and independent of the live tool frames, which are rate-limited and
//! clipped at 160 characters (on the rig 584 frames lost the skill name). A path counts only when
//! it is a file inside a root THIS launch was handed (the snapshot's `skills/`, codex's
//! engine-minted `CODEX_HOME/skills`, copilot's view): a worktree's own `skills/x/SKILL.md` (a run
//! that edits garden), a listing, a glob or a test of a path never counts. A shell command is read
//! per simple command: `ls`, `test`, `echo` and the like name paths without reading them, a comment
//! is skipped, and `bash -c '<script>'` is read inside.
//!
//! One event per (unit attempt, signal, skill or script), across every launch of that attempt (a
//! runtime-wait relaunch, an isolated QE reviewer on the same attempt), released when the run
//! ends ([`forget_run`]).

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::event::CoreEvent;
use crate::skills_snapshot::{SkillsDelivery, SkillsSnapshot, PLUGIN_NAME};

/// A shell command that names files from MORE than this many distinct skills is browsing them
/// (coord, design approval): each is recorded as `browsed`, not `opened`.
pub(crate) const BROWSE_OVER: usize = 3;
/// Calls one sink holds while their outcome is unknown; the oldest is dropped past this.
const PENDING_MAX: usize = 256;
/// The longest call id kept while its outcome is unknown.
const ID_MAX: usize = 128;
/// The longest script identifier recorded.
const SCRIPT_MAX: usize = 200;
/// How deep `bash -c '<script>'` is read into.
const SHELL_DEPTH: usize = 2;
/// Unit attempts whose recorded signals are remembered for dedupe; past this the memory restarts.
const ATTEMPTS_MAX: usize = 4096;

/// The emitter a sink sends through (the runner's own event channel).
pub(crate) type Emit = Arc<dyn Fn(CoreEvent) + Send + Sync>;

type AttemptKey = (String, u32, u32);
type Recorded = std::sync::Mutex<HashMap<AttemptKey, HashSet<(&'static str, String)>>>;

fn recorded() -> &'static Recorded {
    static R: std::sync::OnceLock<Recorded> = std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

/// `true` the first time (attempt, signal, key) is seen in this process.
fn first_time(session: &str, ord: u32, attempt: u32, signal: &'static str, key: &str) -> bool {
    let mut r = recorded().lock().unwrap_or_else(|p| p.into_inner());
    let k = (session.to_string(), ord, attempt);
    if !r.contains_key(&k) && r.len() >= ATTEMPTS_MAX {
        r.clear();
    }
    r.entry(k).or_default().insert((signal, key.to_string()))
}

/// Forget a run's recorded signals (its end; `tool_activity::forget_run`).
pub(crate) fn forget_run(run_id: &str) {
    recorded()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|(r, _, _), _| r != run_id);
}

/// How a root lays its skills out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// `<root>/<index dir>/…`: the snapshot's own `skills/`, nested dirs verbatim.
    Nested,
    /// `<root>/<frontmatter name>/…`: codex's `CODEX_HOME/skills`, the copilot view.
    Flat,
}

/// One root a seat may read skills from, as the seat spells it.
#[derive(Debug, Clone)]
struct Root {
    prefix: PathBuf,
    layout: Layout,
}

/// Where THIS launch's skills and scripts are, and the index that names them.
#[derive(Debug, Clone, Default)]
pub(crate) struct SkillPaths {
    roots: Vec<Root>,
    /// `<snapshot>/scripts` (and its canonical spelling).
    scripts: Vec<PathBuf>,
    /// The snapshot root: a script identifier must name a file under `<root>/scripts`.
    root: PathBuf,
    /// The seat's `CODEX_HOME` (a codex launch), for `$CODEX_HOME` in its shell commands.
    codex_home: Option<PathBuf>,
    /// `(index dir, frontmatter name)`, deepest dir first, so a nested skill owns its own files.
    dirs: Vec<(String, String)>,
    /// The names a FLAT root may hold (what that lever delivered).
    flat_names: HashSet<String>,
    /// Claude's plugin identities this launch was handed: top-level dir → name.
    plugin_dirs: HashMap<String, String>,
    /// The names a native `skill` tool may load (opencode: what its config delivered).
    tool_names: HashSet<String>,
}

impl SkillPaths {
    /// The roots of `snapshot` as `delivery` hands them, plus codex's populated home
    /// (`codex_home`, the seat's `CODEX_HOME` after the container rewrite).
    pub(crate) fn new(
        snapshot: &SkillsSnapshot,
        delivery: &SkillsDelivery,
        codex_home: Option<&Path>,
    ) -> Self {
        let mut p = SkillPaths {
            root: snapshot.root.clone(),
            ..SkillPaths::default()
        };
        for s in snapshot.skills() {
            p.dirs.push((s.dir.clone(), s.name.clone()));
        }
        p.dirs
            .sort_by_key(|d| std::cmp::Reverse(d.0.matches('/').count()));
        p.add_root(snapshot.root.join("skills"), Layout::Nested);
        let scripts = snapshot.root.join("scripts");
        if let Ok(c) = std::fs::canonicalize(&scripts) {
            if c != scripts {
                p.scripts.push(c);
            }
        }
        p.scripts.push(scripts);
        let handed: HashSet<String> = snapshot.handed_names(delivery).into_iter().collect();
        match delivery {
            SkillsDelivery::ClaudePlugin(_) => {
                p.plugin_dirs = snapshot
                    .skills()
                    .iter()
                    .filter(|s| !s.is_nested())
                    .map(|s| (s.dir.clone(), s.name.clone()))
                    .collect();
            }
            SkillsDelivery::OpencodeConfig { .. } => p.tool_names = handed,
            SkillsDelivery::CodexSkillsDir { .. } => {
                p.flat_names = handed;
                if let Some(home) = codex_home {
                    p.codex_home = Some(home.to_path_buf());
                    p.add_root(home.join("skills"), Layout::Flat);
                }
            }
            SkillsDelivery::CopilotAddDir { view, .. } => {
                p.flat_names = handed;
                p.add_root(view.join(".github").join("skills"), Layout::Flat);
            }
            _ => {}
        }
        p
    }

    fn add_root(&mut self, prefix: PathBuf, layout: Layout) {
        if let Ok(c) = std::fs::canonicalize(&prefix) {
            if c != prefix {
                self.roots.push(Root { prefix: c, layout });
            }
        }
        self.roots.push(Root { prefix, layout });
    }

    /// The skill whose directory holds the FILE `path` names (literally: no expansion), `None`
    /// when it is a pattern, outside every handed root, not a regular file, or no indexed skill.
    pub(crate) fn skill_of_path(&self, path: &str) -> Option<String> {
        if path.contains(['*', '?', '[']) {
            return None;
        }
        let p = normalize(path)?;
        if !std::fs::metadata(&p).is_ok_and(|m| m.is_file()) {
            return None;
        }
        for root in &self.roots {
            let Ok(rel) = p.strip_prefix(&root.prefix) else {
                continue;
            };
            let parts: Vec<&str> = rel
                .components()
                .filter_map(|c| match c {
                    Component::Normal(s) => s.to_str(),
                    _ => None,
                })
                .collect();
            match root.layout {
                Layout::Flat => {
                    if parts.len() >= 2 && self.flat_names.contains(parts[0]) {
                        return Some(parts[0].to_string());
                    }
                }
                Layout::Nested => {
                    for (dir, name) in &self.dirs {
                        let d: Vec<&str> = dir.split('/').collect();
                        if parts.len() > d.len() && parts[..d.len()] == d[..] {
                            return Some(name.clone());
                        }
                    }
                }
            }
        }
        None
    }

    /// The skill a `Skill`-tool argument loads, judged against what THIS launch was handed:
    /// Claude's `wicked-garden:<top-level dir>`, or a name a native skill tool was delivered.
    pub(crate) fn skill_of_tool_arg(&self, arg: &str) -> Option<String> {
        let arg = arg.trim().trim_start_matches('/');
        if let Some(dir) = arg.strip_prefix(&format!("{PLUGIN_NAME}:")) {
            return self.plugin_dirs.get(dir).cloned();
        }
        self.tool_names.contains(arg).then(|| arg.to_string())
    }

    /// `scripts/<rel>` when `operand` (`scripts/<rel>`, `./scripts/<rel>`) names a regular file
    /// of the snapshot's `scripts/`; anything else (an argument, a missing file) is `None`.
    fn script_of_operand(&self, operand: &str) -> Option<String> {
        let rel = operand.trim_start_matches("./").strip_prefix("scripts/")?;
        self.script_rel(rel)
    }

    fn script_rel(&self, rel: &str) -> Option<String> {
        if rel.is_empty() || rel.len() > SCRIPT_MAX {
            return None;
        }
        let mut parts = Vec::new();
        for c in rel.split('/') {
            match c {
                "" | "." => {}
                ".." => return None,
                s if s
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.')) =>
                {
                    parts.push(s)
                }
                _ => return None,
            }
        }
        if parts.is_empty() {
            return None;
        }
        let file = parts
            .iter()
            .fold(self.root.join("scripts"), |p, c| p.join(c));
        std::fs::metadata(&file)
            .is_ok_and(|m| m.is_file())
            .then(|| format!("scripts/{}", parts.join("/")))
    }

    /// `scripts/<rel>` when the shell word names a file under the snapshot's `scripts/`.
    fn script_of_path(&self, word: &str) -> Option<String> {
        let p = normalize(&self.expand(word))?;
        self.scripts.iter().find_map(|dir| {
            let rel = p.strip_prefix(dir).ok()?;
            self.script_rel(&rel.to_string_lossy().replace('\\', "/"))
        })
    }

    /// A launcher: the bare `wicked-garden` (the seat's `PATH` leads with the snapshot's
    /// `scripts/`) or the delivered `scripts/wicked-garden[.cmd|.mjs]` itself; never another path.
    fn is_launcher(&self, word: &str) -> bool {
        const NAMES: [&str; 3] = ["wicked-garden", "wicked-garden.cmd", "wicked-garden.mjs"];
        if NAMES.contains(&word) {
            return true;
        }
        let Some(p) = normalize(&self.expand(word)) else {
            return false;
        };
        p.file_name()
            .and_then(|f| f.to_str())
            .is_some_and(|f| NAMES.contains(&f))
            && self.scripts.iter().any(|d| p.parent() == Some(d.as_path()))
    }

    /// `$CODEX_HOME` and `$WICKED_GARDEN_ROOT` as THIS launch set them (a shell word only).
    fn expand(&self, word: &str) -> String {
        let vars: [(&str, Option<&Path>); 2] = [
            ("CODEX_HOME", self.codex_home.as_deref()),
            ("WICKED_GARDEN_ROOT", Some(self.root.as_path())),
        ];
        for (var, value) in vars {
            let Some(value) = value else { continue };
            for spelled in [format!("${{{var}}}"), format!("${var}")] {
                if let Some(rest) = word.strip_prefix(&spelled) {
                    if rest.is_empty() || rest.starts_with('/') {
                        return format!("{}{rest}", value.to_string_lossy());
                    }
                }
            }
        }
        word.to_string()
    }
}

/// An absolute path with `.`/`..` resolved lexically; `None` for a relative one.
fn normalize(p: &str) -> Option<PathBuf> {
    let path = Path::new(p);
    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// Programs that load what a file says: a skill file named as one of their operands is read.
/// Anything else naming a file (`ls`, `test`, `echo`, `rg --files`, an unknown tool) is not
/// evidence of a read, though it still counts toward the browse guard.
const READERS: [&str; 24] = [
    "cat", "head", "tail", "sed", "awk", "gawk", "less", "more", "bat", "nl", "grep", "egrep",
    "rg", "cut", "sort", "uniq", "diff", "cmp", "jq", "yq", "tac", "od", "xxd", "strings",
];
/// Programs that run the script named by their first operand.
const INTERPRETERS: [&str; 8] = [
    "python", "python3", "node", "bash", "sh", "zsh", "deno", "bun",
];
/// Prefixes that run the rest of the command: wrappers, and the shell's own keywords that open
/// a command (`if …; then cat F; fi`, `do cat F`, `{ cat F; }`).
const WRAPPERS: [&str; 18] = [
    "sudo", "command", "exec", "env", "time", "nice", "nohup", "if", "then", "else", "elif", "do",
    "while", "until", "!", "{", "}", "done",
];

/// One shell token: a word as the shell passes it (quotes removed), and whether a `$` in it may
/// be literal (single quotes, a backslash escape: the shell does not expand it), or a command
/// boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word { text: String, quoted: bool },
    Sep,
}

/// POSIX-ish tokens: quotes and escapes kept in a word, unquoted `;`, `|`, `&`, `(`, `)`, a
/// backtick and a NEWLINE end a simple command, an unquoted `#` at a word's start comments out
/// the rest of its line. `None` on an unterminated quote: nothing is read from such a command.
fn tokens(s: &str) -> Option<Vec<Tok>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut quoted = false;
    let mut it = s.chars().peekable();
    let flush = |out: &mut Vec<Tok>, cur: &mut String, have: &mut bool, quoted: &mut bool| {
        if *have {
            out.push(Tok::Word {
                text: std::mem::take(cur),
                quoted: *quoted,
            });
        }
        *have = false;
        *quoted = false;
    };
    while let Some(c) = it.next() {
        match c {
            '\'' => {
                have = true;
                quoted = true;
                loop {
                    match it.next()? {
                        '\'' => break,
                        ch => cur.push(ch),
                    }
                }
            }
            '"' => {
                // Double quotes still expand `$VAR`; only an escaped `\$` is literal.
                have = true;
                loop {
                    match it.next()? {
                        '"' => break,
                        '\\' => match it.next()? {
                            e @ ('"' | '\\' | '$' | '`') => {
                                quoted |= e == '$';
                                cur.push(e)
                            }
                            '\n' => {}
                            e => {
                                cur.push('\\');
                                cur.push(e);
                            }
                        },
                        ch => cur.push(ch),
                    }
                }
            }
            '\\' => {
                have = true;
                quoted = true;
                match it.next() {
                    Some('\n') | None => {}
                    Some(e) => cur.push(e),
                }
            }
            '#' if !have => {
                for ch in it.by_ref() {
                    if ch == '\n' {
                        break;
                    }
                }
                flush(&mut out, &mut cur, &mut have, &mut quoted);
                out.push(Tok::Sep);
            }
            ';' | '|' | '&' | '(' | ')' | '`' | '\n' => {
                flush(&mut out, &mut cur, &mut have, &mut quoted);
                out.push(Tok::Sep);
            }
            ch if ch.is_whitespace() => flush(&mut out, &mut cur, &mut have, &mut quoted),
            ch => {
                have = true;
                cur.push(ch);
            }
        }
    }
    flush(&mut out, &mut cur, &mut have, &mut quoted);
    Some(out)
}

/// A command's simple commands, each its words with their quoting.
fn segments(cmd: &str) -> Vec<Vec<(String, bool)>> {
    let Some(toks) = tokens(cmd) else {
        return Vec::new();
    };
    let mut out: Vec<Vec<(String, bool)>> = vec![Vec::new()];
    for t in toks {
        match t {
            Tok::Word { text, quoted } => {
                if let Some(last) = out.last_mut() {
                    last.push((text, quoted));
                }
            }
            Tok::Sep => {
                if out.last().is_some_and(|s| !s.is_empty()) {
                    out.push(Vec::new());
                }
            }
        }
    }
    out.retain(|s| !s.is_empty());
    out
}

/// `NAME=value`: a shell assignment, whatever its value holds.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn base(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// What one call shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Hit {
    Opened { skill: String, how: &'static str },
    Browsed { skill: String },
    Ran { script: String },
}

/// The hits of one call, in claude's tool words (`Skill`, `Read`, `Bash`; every adapter maps its
/// CLI's own tools onto these).
pub(crate) fn classify(paths: &SkillPaths, tool: &str, input: &Value) -> Vec<Hit> {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    match tool {
        "Skill" => ["skill", "command", "name"]
            .iter()
            .map(|k| s(k))
            .find(|v| !v.is_empty())
            .and_then(|v| paths.skill_of_tool_arg(v))
            .map(|skill| {
                vec![Hit::Opened {
                    skill,
                    how: "skill_tool",
                }]
            })
            .unwrap_or_default(),
        "Read" | "NotebookRead" => ["file_path", "path", "notebook_path"]
            .iter()
            .map(|k| s(k))
            .find(|v| !v.is_empty())
            .and_then(|p| paths.skill_of_path(p))
            .map(|skill| vec![Hit::Opened { skill, how: "read" }])
            .unwrap_or_default(),
        "Bash" => classify_command(paths, s("command")),
        _ => Vec::new(),
    }
}

/// A shell command's hits: the skills whose files it reads (opened), and the garden scripts it
/// runs. When the command NAMES files from more than [`BROWSE_OVER`] skills (whatever reads
/// them: `head` over many, a loop, a listing beside a read), each is browsed, not opened.
pub(crate) fn classify_command(paths: &SkillPaths, cmd: &str) -> Vec<Hit> {
    let mut named: BTreeSet<String> = BTreeSet::new();
    let mut read: BTreeSet<String> = BTreeSet::new();
    let mut scripts: BTreeSet<String> = BTreeSet::new();
    read_command(paths, cmd, 0, &mut named, &mut read, &mut scripts);
    let mut out: Vec<Hit> = if named.len() > BROWSE_OVER {
        named
            .into_iter()
            .map(|skill| Hit::Browsed { skill })
            .collect()
    } else {
        read.into_iter()
            .map(|skill| Hit::Opened {
                skill,
                how: "shell",
            })
            .collect()
    };
    out.extend(scripts.into_iter().map(|script| Hit::Ran { script }));
    out
}

fn read_command(
    paths: &SkillPaths,
    cmd: &str,
    depth: usize,
    named: &mut BTreeSet<String>,
    read: &mut BTreeSet<String>,
    scripts: &mut BTreeSet<String>,
) {
    for seg in segments(cmd) {
        // Leading assignments and wrappers run what follows.
        let mut i = 0;
        while i < seg.len() && (WRAPPERS.contains(&seg[i].0.as_str()) || is_assignment(&seg[i].0)) {
            i += 1;
        }
        let words: Vec<&str> = seg[i..].iter().map(|(w, _)| w.as_str()).collect();
        // Every literal skill file the command names, for the browse guard.
        for (w, quoted) in &seg[i..] {
            for candidate in w.split(['=', '<', '>']).filter(|c| !c.is_empty()) {
                let path = if *quoted && candidate.contains('$') {
                    candidate.to_string()
                } else {
                    paths.expand(candidate)
                };
                if let Some(skill) = paths.skill_of_path(&path) {
                    named.insert(skill);
                }
            }
        }
        let Some(&prog_word) = words.first() else {
            continue;
        };
        let prog = base(prog_word);
        // `bash -c '<script>'`: read the script.
        if matches!(prog, "bash" | "sh" | "zsh")
            && matches!(words.get(1).copied(), Some("-c" | "-lc" | "-ic"))
        {
            if depth < SHELL_DEPTH {
                if let Some(inner) = words.get(2) {
                    read_command(paths, inner, depth + 1, named, read, scripts);
                }
            }
            continue;
        }
        // An interpreter's script operand: its first non-option word (`deno run` / `bun run`
        // take a subcommand first); `--help`/`--version` run nothing.
        let operand = if INTERPRETERS.contains(&prog) {
            let mut rest = words.iter().skip(1).filter(|w| !w.starts_with('-'));
            let first = rest.next().copied();
            let operand = if matches!(prog, "deno" | "bun") && first == Some("run") {
                rest.next().copied()
            } else {
                first
            };
            // The interpreter's OWN `--help` / `--version` (before its script) runs nothing; one
            // after the script is the script's argument.
            let own = words
                .iter()
                .skip(1)
                .take_while(|w| Some(**w) != operand)
                .any(|w| matches!(*w, "--help" | "-h" | "--version" | "-V"));
            if own {
                continue;
            }
            operand
        } else {
            None
        };
        // The launcher: `wicked-garden run scripts/<rel>`, bare or interpreted.
        let launcher_at = if paths.is_launcher(prog_word) {
            Some(0)
        } else {
            operand
                .filter(|o| paths.is_launcher(o))
                .and_then(|o| words.iter().position(|w| *w == o))
        };
        if let Some(at) = launcher_at {
            if words.get(at + 1).copied() == Some("run") {
                if let Some(s) = words.get(at + 2).and_then(|w| paths.script_of_operand(w)) {
                    scripts.insert(s);
                }
            }
            continue;
        }
        // A snapshot script run as the program, or as an interpreter's script operand.
        if let Some(s) = paths
            .script_of_path(prog_word)
            .or_else(|| operand.and_then(|o| paths.script_of_path(o)))
        {
            scripts.insert(s);
            continue;
        }
        // A reading program: its operands that name a file inside a handed skill dir.
        if !READERS.contains(&prog)
            || (prog == "rg" && words.iter().any(|w| matches!(*w, "--files" | "-l" | "-c")))
        {
            continue;
        }
        for (w, quoted) in &seg[i + 1..] {
            for candidate in w.split(['=', '<', '>']).filter(|c| !c.is_empty()) {
                let path = if *quoted && candidate.contains('$') {
                    candidate.to_string()
                } else {
                    paths.expand(candidate)
                };
                if let Some(skill) = paths.skill_of_path(&path) {
                    read.insert(skill);
                }
            }
        }
    }
}

/// A call id as a pending key: at most [`ID_MAX`] bytes (cut at a char boundary), so held calls
/// are bounded in bytes as well as count.
fn pending_key(id: &str) -> String {
    let mut end = id.len().min(ID_MAX);
    while !id.is_char_boundary(end) {
        end -= 1;
    }
    id[..end].to_string()
}

/// One unit attempt's usage recorder: the adapters hand it every call; a read counts once the
/// call succeeded, a script run once it finished (any exit: the script ran); each (signal,
/// skill/script) is emitted once per unit attempt.
pub(crate) struct SkillUseSink {
    session: String,
    ord: u32,
    attempt: u32,
    cli: String,
    gen: Option<String>,
    source: String,
    paths: Arc<SkillPaths>,
    pending: HashMap<String, Vec<Hit>>,
    order: VecDeque<String>,
    /// What this launch emitted: a live sink never repeats itself, even if the process-wide
    /// memory ([`first_time`]) restarted past [`ATTEMPTS_MAX`].
    seen: HashSet<(&'static str, String)>,
    tx: Emit,
}

impl SkillUseSink {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session: &str,
        ord: u32,
        attempt: u32,
        cli: &str,
        snapshot: &SkillsSnapshot,
        paths: Arc<SkillPaths>,
        tx: Emit,
    ) -> Self {
        Self {
            session: session.to_string(),
            ord,
            attempt,
            cli: cli.to_string(),
            gen: snapshot.gen.clone(),
            source: snapshot.source.token().to_string(),
            paths,
            pending: HashMap::new(),
            order: VecDeque::new(),
            seen: HashSet::new(),
            tx,
        }
    }

    /// The sink for one wrapped launch: `None` when no snapshot was handed or the runner has no
    /// event channel.
    pub(crate) fn for_launch(
        input: &crate::workflow::StepInput,
        cli: &str,
        handed: Option<&SkillsSnapshot>,
        paths: Option<Arc<SkillPaths>>,
        tx: Option<std::sync::mpsc::Sender<crate::command::Command>>,
    ) -> Option<Self> {
        let (snap, paths, tx) = (handed?, paths?, tx?);
        let tx = std::sync::Mutex::new(tx);
        Some(Self::new(
            &input.run_id,
            input.unit.ord,
            input.attempt,
            cli,
            snap,
            paths,
            Arc::new(move |ev| {
                if let Ok(t) = tx.lock() {
                    let _ = t.send(crate::command::Command::EmitEvent(ev));
                }
            }),
        ))
    }

    /// A call `id` started (claude `tool_use`, codex `item.started`, pi `tool_execution_start`):
    /// held until [`finished`](Self::finished) says how it went.
    pub(crate) fn started(&mut self, id: &str, tool: &str, input: &Value) {
        let id = pending_key(id);
        let id = id.as_str();
        if id.is_empty() || self.pending.contains_key(id) {
            return;
        }
        let hits = classify(&self.paths, tool, input);
        if hits.is_empty() {
            return;
        }
        if self.order.len() >= PENDING_MAX {
            if let Some(old) = self.order.pop_front() {
                self.pending.remove(&old);
            }
        }
        self.order.push_back(id.to_string());
        self.pending.insert(id.to_string(), hits);
    }

    /// Call `id` ended: a read counts when it succeeded; a script run counts either way.
    pub(crate) fn finished(&mut self, id: &str, ok: bool) {
        let id = pending_key(id);
        let id = id.as_str();
        let Some(hits) = self.pending.remove(id) else {
            return;
        };
        self.order.retain(|o| o != id);
        for hit in hits {
            if ok || matches!(hit, Hit::Ran { .. }) {
                self.emit(hit);
            }
        }
    }

    /// A call seen only once it was done (opencode's completed `tool_use`, agy's `DONE` step).
    pub(crate) fn completed(&mut self, tool: &str, input: &Value, ok: bool) {
        for hit in classify(&self.paths, tool, input) {
            if ok || matches!(hit, Hit::Ran { .. }) {
                self.emit(hit);
            }
        }
    }

    /// A codex / pi call as [`crate::tool_activity::codex_item`] / `pi_event` read it.
    pub(crate) fn call(&mut self, c: &crate::tool_activity::CodexCall) {
        if c.started {
            self.started(&c.id, &c.tool, &c.input);
        } else {
            if !self.pending.contains_key(&pending_key(&c.id)) {
                self.started(&c.id, &c.tool, &c.input);
            }
            self.finished(&c.id, c.ok);
        }
    }

    /// The stream closed: a read with no answer is not counted; a script that was started ran.
    pub(crate) fn close(&mut self) {
        let ids: Vec<String> = self.order.drain(..).collect();
        for id in ids {
            if let Some(hits) = self.pending.remove(&id) {
                for hit in hits.into_iter().filter(|h| matches!(h, Hit::Ran { .. })) {
                    self.emit(hit);
                }
            }
        }
    }

    fn emit(&mut self, hit: Hit) {
        let (signal, key, skill, how, script) = match hit {
            Hit::Opened { skill, how } => ("opened", skill.clone(), Some(skill), Some(how), None),
            Hit::Browsed { skill } => ("browsed", skill.clone(), Some(skill), None, None),
            Hit::Ran { script } => ("ran", script.clone(), None, None, Some(script)),
        };
        if !self.seen.insert((signal, key.clone()))
            || !first_time(&self.session, self.ord, self.attempt, signal, &key)
        {
            return;
        }
        (self.tx)(CoreEvent::SkillUsed {
            session: self.session.clone(),
            ord: self.ord,
            attempt: self.attempt,
            cli: self.cli.clone(),
            signal: signal.to_string(),
            skill,
            via: None,
            how: how.map(str::to_string),
            script,
            gen: self.gen.clone(),
            source: self.source.clone(),
        });
    }
}

/// The `directed` events of one launch: the skills its directive names (`skill_prompt`): the
/// phase `skill_ref` (`via: phase`), the run's base skill (`via: base`, not on an engine-internal
/// unit, whose prompt is authored byte-exact) and the QE skill (`via: qe`, on a unit whose QE
/// acceptance is required). Each name once per unit attempt (a relaunch of the same attempt
/// records nothing new), the first reason winning.
pub(crate) fn directed_events(
    input: &crate::workflow::StepInput,
    cli: &str,
    snapshot: Option<&SkillsSnapshot>,
) -> Vec<CoreEvent> {
    let unit = &input.unit;
    let internal = crate::execute_wrapped::is_engine_internal(unit);
    let mut named: Vec<(&str, &'static str)> = Vec::new();
    if let Some(s) = unit.skill_ref.as_deref().filter(|s| !s.is_empty()) {
        named.push((s, "phase"));
    }
    if !internal {
        if let Some(s) = unit.base_skill_ref.as_deref().filter(|s| !s.is_empty()) {
            named.push((s, "base"));
        }
        // Named only when acceptance is REQUIRED: a waived or skipped unit is told not to run it.
        if unit.qe_acceptance.as_ref().is_some_and(|q| q.required()) {
            named.push((crate::execute_wrapped::QE_SKILL, "qe"));
        }
    }
    let mut out = Vec::new();
    for (skill, via) in named {
        let name = snapshot
            .and_then(|s| s.skill(skill))
            .map(|e| e.name.clone())
            .unwrap_or_else(|| skill.to_string());
        if !first_time(&input.run_id, unit.ord, input.attempt, "directed", &name) {
            continue;
        }
        out.push(CoreEvent::SkillUsed {
            session: input.run_id.clone(),
            ord: unit.ord,
            attempt: input.attempt,
            cli: cli.to_string(),
            signal: "directed".to_string(),
            skill: Some(name),
            via: Some(via.to_string()),
            how: None,
            script: None,
            gen: snapshot.and_then(|s| s.gen.clone()),
            source: snapshot
                .map(|s| s.source.token().to_string())
                .unwrap_or_else(|| "none".to_string()),
        });
    }
    out
}

#[cfg(test)]
#[path = "skill_use_tests.rs"]
mod tests;
