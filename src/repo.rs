//! REPO REGISTRY — first-class, persistent registration of the git repositories the orchestrator
//! works within, plus the git-worktree isolation a run uses so the user's working tree is never
//! touched.
//!
//! A [`RepoEntry`] is a `Node(Other("repo_entry"))` on the shared estate store (mirrors the
//! `AgentSession` projection in [`crate::domain`]). A run that targets a registered repo gets its own
//! worktree at `<repo>/.wicked/worktrees/<run_id>` on branch `wicked/<run_id>`; the worker runs there
//! (augment mode — see `ORCHESTRATOR.md` §4). Worktrees are reaped on a terminal run status — but
//! only when CLEAN ([`reap_worktree_if_clean`]): a tree holding uncommitted work is kept and logged,
//! never force-deleted, because those bytes may be the only copy of the work. The startup orphan
//! reaper ([`reap_orphan_worktrees`]) applies the same rule to terminal runs' leftovers and
//! force-removes only worktrees whose run id no longer exists on the store at all. The
//! `wicked/<run_id>` BRANCH is never deleted by any of this — the branch is the durable record of a
//! run's landed work; the worktree is scaffolding.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, FromNode, GraphRead, GraphStore, HardenedCommand, Language, Location, Node,
    NodeKind, Span, ToNode, SYMBOL_SCHEME,
};
use wicked_estate_core::SymbolQuery;

use crate::domain::put_node;

/// Node-kind for a registered repository.
pub const REPO_ENTRY: &str = "repo_entry";

/// A registered repository the orchestrator can run within.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoEntry {
    /// Stable id (slug of the name) — the node identity.
    pub id: String,
    /// Human display name.
    pub name: String,
    /// Absolute path to the git repository root.
    pub root_path: String,
    /// The branch worktrees are based on (resolved at registration).
    pub default_branch: String,
    /// Registration timestamp (unix seconds), supplied by the caller (no wall-clock in the lib).
    #[serde(default)]
    pub registered_at: i64,
    /// ABSOLUTE path of this repo's code graph. **Derived, never authoritative in the record.**
    ///
    /// It exists so out-of-process consumers stop re-deriving it. crew spelled
    /// `join(root_path, '.codegraph', 'estate.db')` in five places against the engine's own sixth
    /// spelling, and nothing failed when they disagreed — the worker just queried an empty database
    /// (FINDING-069). A field on the record they already read makes the engine the one source.
    ///
    /// [`RepoEntry::from_node`] recomputes it from `root_path` and discards whatever was persisted, so
    /// a record written before this field existed reads back correct, and a repo that moves does not
    /// carry a stale path forward. Do not write to it expecting it to stick.
    #[serde(default)]
    pub code_graph_db: String,
}

impl ToNode for RepoEntry {
    fn node_kind() -> &'static str {
        REPO_ENTRY
    }
    fn to_node(&self) -> Node {
        let mut node = Node::new(
            synthetic_symbol(REPO_ENTRY, &self.id),
            NodeKind::Other(REPO_ENTRY.to_string()),
            self.id.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{REPO_ENTRY}/{}", self.id), Span::ZERO),
        );
        if let serde_json::Value::Object(map) =
            serde_json::to_value(self).expect("RepoEntry serializes to JSON")
        {
            node.metadata = map;
        }
        node
    }
}

impl FromNode for RepoEntry {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == REPO_ENTRY => {}
            other => anyhow::bail!("expected NodeKind::Other({REPO_ENTRY:?}), got {other:?}"),
        }
        let mut entry: RepoEntry =
            serde_json::from_value(serde_json::Value::Object(node.metadata.clone()))
                .map_err(|e| anyhow::anyhow!("node {} is not a valid RepoEntry: {e}", node.name))?;
        entry.code_graph_db = code_graph_db(&entry.root_path);
        Ok(entry)
    }
}

/// This repo's code-graph path, absolute, derived from its root. The only spelling any consumer needs.
fn code_graph_db(root_path: &str) -> String {
    Path::new(root_path)
        .join(crate::code_graph::code_graph_rel())
        .to_string_lossy()
        .into_owned()
}

/// What a caller asks to register. The id/branch are resolved by [`register_repo`].
#[derive(Debug, Clone)]
pub struct RepoSpec {
    pub name: String,
    pub root_path: String,
    pub registered_at: i64,
}

/// A 4-word kebab slug of `name` (mirrors the UI's slug, minus the timestamp suffix).
fn slug(name: &str) -> String {
    let base: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let words: Vec<&str> = base.split('-').filter(|w| !w.is_empty()).take(4).collect();
    if words.is_empty() {
        "repo".to_string()
    } else {
        words.join("-")
    }
}

/// Run `git -C <root> <args...>` and return `(success, stdout, stderr)`.
fn git(root: &str, args: &[&str]) -> anyhow::Result<(bool, String, String)> {
    let out = Command::new("git")
        .hardened()
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("git could not run: {e}"))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}

/// Validate `root` is a git repo with at least one commit (a worktree requires a base commit), and
/// return its current branch name.
pub fn validate_git_repo(root: &str) -> anyhow::Result<String> {
    if !Path::new(root).is_dir() {
        anyhow::bail!("{root} is not a directory");
    }
    let (ok, _, _) = git(root, &["rev-parse", "--is-inside-work-tree"])?;
    if !ok {
        anyhow::bail!("{root} is not a git repository");
    }
    let (has_commit, _, _) = git(root, &["rev-parse", "HEAD"])?;
    if !has_commit {
        anyhow::bail!("{root} has no commits yet (a worktree needs at least one commit)");
    }
    let (_, branch, _) = git(root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    Ok(if branch.is_empty() {
        "main".to_string()
    } else {
        branch
    })
}

/// Register a repository: validate it, resolve its id + default branch, persist the [`RepoEntry`].
pub fn register_repo(store: &mut dyn GraphStore, spec: RepoSpec) -> anyhow::Result<RepoEntry> {
    // Validate FIRST (its "not a directory" / "not a git repository" errors are friendlier than a raw
    // canonicalize ENOENT), then resolve the root to an ABSOLUTE, symlink-free path before persisting.
    // A caller may register with a relative path (`register-repo --path ./repo`), but the daemon and
    // every downstream consumer — worktree creation, code-graph resolution — run from a DIFFERENT cwd
    // and assume the stored root_path is absolute. Persisting the as-given relative path yields a
    // root_path (and a code_graph_db derived from it) that resolves to nothing outside the registering
    // cwd. `canonicalize` also collapses `..`/symlink spellings to ONE identity, so the same repo
    // registered two ways lands on one RepoEntry (core#214).
    let default_branch = validate_git_repo(&spec.root_path)?;
    let root_path = std::fs::canonicalize(&spec.root_path)
        .map_err(|e| {
            anyhow::anyhow!(
                "cannot resolve repo path {} to an absolute path: {e}",
                spec.root_path
            )
        })?
        .to_string_lossy()
        .into_owned();
    let entry = RepoEntry {
        id: slug(&spec.name),
        name: spec.name,
        code_graph_db: code_graph_db(&root_path),
        root_path,
        default_branch,
        registered_at: spec.registered_at,
    };
    put_node(store, entry.to_node())?;
    Ok(entry)
}

/// Every registered repo on the store.
pub fn list_repos(store: &dyn GraphRead) -> anyhow::Result<Vec<RepoEntry>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(REPO_ENTRY.to_string())],
        ..Default::default()
    };
    Ok(store
        .find_symbols(&query)?
        .iter()
        .filter_map(|n| RepoEntry::from_node(n).ok())
        .collect())
}

/// Read one registered repo by id.
pub fn get_repo(store: &dyn GraphRead, repo_id: &str) -> anyhow::Result<Option<RepoEntry>> {
    match store.get_node(&synthetic_symbol(REPO_ENTRY, repo_id))? {
        Some(node) => Ok(Some(RepoEntry::from_node(&node)?)),
        None => Ok(None),
    }
}

/// The directory worktrees for `repo_root` live under.
fn worktrees_root(repo_root: &str) -> PathBuf {
    Path::new(repo_root).join(".wicked").join("worktrees")
}

/// Is `wt` a live git worktree, rather than merely a directory that sits where one should?
///
/// A worktree carries a `.git` **file** (not a directory) pointing at its admin dir, and `rev-parse`
/// inside it resolves. Both are checked: the file alone can outlive the admin entry that gives it
/// meaning, and `rev-parse` alone succeeds anywhere beneath the parent repo — including an empty
/// `.wicked/worktrees/<id>/`, which is exactly the case this exists to reject.
fn is_live_worktree(wt: &Path) -> bool {
    if !wt.join(".git").is_file() {
        return false;
    }
    let Some(p) = wt.to_str() else { return false };
    matches!(git(p, &["rev-parse", "--git-dir"]), Ok((true, _, _)))
}

/// Create an isolated git worktree for `run_id` at `<repo>/.wicked/worktrees/<run_id>` on a fresh
/// `wicked/<run_id>` branch. Idempotent for a genuine resume: a live worktree already at the path is
/// reused. Returns the worktree path.
///
/// The reuse test is [`is_live_worktree`], not `is_dir()`. It used to be `is_dir()`, and the
/// difference is FINDING-059: `remove_worktree` falls back to `remove_dir_all`, a partial removal
/// leaves the directory shell behind, and the `git worktree prune` that follows then deregisters the
/// admin entry *because* the path no longer has a `.git` file. The result is an empty, unregistered
/// directory sitting exactly where a worktree belongs — which `is_dir()` accepted and returned as an
/// isolated checkout. The worker handed one noticed ("the assigned worktree is an empty,
/// unregistered directory, so I'll work in the main repo checkout") and wrote 297 lines onto
/// `master` of the operator's real clone. A cwd is not a boundary; the worktree is, so its existence
/// has to be verified rather than inferred from a stat.
pub fn create_worktree(repo_root: &str, run_id: &str) -> anyhow::Result<PathBuf> {
    let wt = worktrees_root(repo_root).join(run_id);
    if wt.is_dir() {
        if is_live_worktree(&wt) {
            return Ok(wt); // genuine resume — reuse it
        }
        // Not a worktree. Recoverable only while it holds nothing: an empty shell can be cleared and
        // re-added, and `worktree add` accepts an existing empty directory anyway. Anything else is
        // a directory of unknown provenance, and calling it an isolated checkout is the failure
        // above — fail the run loudly instead of handing it over.
        // `unwrap_or(false)` reads an unreadable directory as NOT empty, so a permissions error
        // fails the run rather than silently taking the recovery branch on a tree we cannot see.
        let empty = std::fs::read_dir(&wt)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if !empty {
            anyhow::bail!(
                "{} exists but is not a git worktree, and is not empty; refusing to run a unit \
                 against it (a worker given a non-checkout works in the parent repo instead)",
                wt.display()
            );
        }
        let _ = std::fs::remove_dir(&wt);
        // The other half of this state: git may still hold an admin entry for the path whose `.git`
        // file we just found missing, and `worktree add` refuses a path that is "already
        // registered". Prune drops exactly those dangling entries and touches no live worktree.
        let _ = git(repo_root, &["worktree", "prune"]);
    }
    std::fs::create_dir_all(worktrees_root(repo_root))?;
    let branch = format!("wicked/{run_id}");
    let wt_str = wt.to_string_lossy().to_string();
    let (ok, _, err) = git(repo_root, &["worktree", "add", &wt_str, "-b", &branch])?;
    if !ok {
        // A stale branch from a prior run can block re-add; retry without -b (reuse the branch).
        let (ok2, _, err2) = git(repo_root, &["worktree", "add", &wt_str, &branch])?;
        if !ok2 {
            anyhow::bail!("git worktree add failed: {err}{err2}");
        }
    }
    Ok(wt)
}

/// Remove a run's worktree unconditionally (best-effort — a failure to clean up is logged, not
/// fatal). This is the DESTRUCTIVE form: `--force` deletes uncommitted work. It is reserved for the
/// two cases where discarding is the point — an operator's explicit Cancel (abandonment), and a
/// startup leftover whose run id no longer exists on the store (no record, nothing to preserve).
/// A run that merely FINISHED goes through [`reap_worktree_if_clean`] instead (FINDING-003).
pub fn remove_worktree(repo_root: &str, run_id: &str) {
    let wt = worktrees_root(repo_root).join(run_id);
    let wt_str = wt.to_string_lossy().to_string();
    let _ = git(repo_root, &["worktree", "remove", "--force", &wt_str]);
    // If git refused (e.g. already gone), drop the dir directly.
    if wt.is_dir() {
        let _ = std::fs::remove_dir_all(&wt);
    }
}

/// FINDING-003 — reap a TERMINAL run's worktree, but only when it is CLEAN. Returns whether the
/// path is gone.
///
/// Deliberately NOT `--force` and NO `remove_dir_all` fallback: git's non-forced `worktree remove`
/// refuses a tree with modified or untracked files, and that refusal is the safety property this
/// function is built on. A terminal run's uncommitted files are work that never landed on the
/// `wicked/<run_id>` branch (the known artifact-landing gap — 3 of the finding's 14 orphans carried
/// exactly that), so force-deleting them here would make the REAPER the thing that destroys the
/// only copy of the work. A kept tree is announced on stderr each time, so it is a visible,
/// named leftover rather than a silent leak; a clean tree adds nothing the branch doesn't already
/// carry, and goes. The branch itself is never touched either way.
pub fn reap_worktree_if_clean(repo_root: &str, run_id: &str) -> bool {
    let wt = worktrees_root(repo_root).join(run_id);
    if !wt.is_dir() {
        // Nothing on disk. Drop any dangling admin entry so the path is re-usable.
        let _ = git(repo_root, &["worktree", "prune"]);
        return true;
    }
    let wt_str = wt.to_string_lossy().to_string();
    match git(repo_root, &["worktree", "remove", &wt_str]) {
        Ok((true, _, _)) => true,
        Ok((false, _, err)) => {
            eprintln!(
                "wicked-core: keeping worktree {} — git refused a non-forced remove ({}); it \
                 likely holds uncommitted work the wicked/{run_id} branch does not carry",
                wt.display(),
                err.trim()
            );
            false
        }
        Err(e) => {
            eprintln!("wicked-core: could not reap worktree {}: {e}", wt.display());
            false
        }
    }
}

/// Prune worktrees whose run is not live, on actor startup. For each
/// `<repo>/.wicked/worktrees/<id>`:
///  - `<id>` in `live_run_ids` (a session in a NON-terminal status) → kept, it may resume;
///  - `<id>` in `terminal_run_ids` (a session that finished) → [`reap_worktree_if_clean`] — the
///    same rule the terminal-status reap applies, re-run here so a crash between a run going
///    terminal and its reap (or a run predating the reap) converges on the next start instead of
///    surviving restarts forever (FINDING-003: 14 did);
///  - unknown to the store → [`remove_worktree`] (force): no session record exists, so there is no
///    run to resume and no outcome the leftover documents.
pub fn reap_orphan_worktrees(
    repos: &[RepoEntry],
    live_run_ids: &HashSet<String>,
    terminal_run_ids: &HashSet<String>,
) {
    for repo in repos {
        let root = worktrees_root(&repo.root_path);
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in entries.flatten() {
            if let Some(name) = e.file_name().to_str() {
                if live_run_ids.contains(name) {
                    continue;
                }
                if terminal_run_ids.contains(name) {
                    let _ = reap_worktree_if_clean(&repo.root_path, name);
                } else {
                    remove_worktree(&repo.root_path, name);
                }
            }
        }
        // Tidy git's worktree administrative list.
        let _ = git(&repo.root_path, &["worktree", "prune"]);
    }
}

// ── Worktree layout summary (FINDING-048) ────────────────────────────────────────────────────────

/// Directory names never worth a line in the summary: build output, vendored dependencies and
/// virtualenvs. They are large, uninformative, and present in nearly every repo.
const LAYOUT_NOISE: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    "vendor",
    "__pycache__",
    "venv",
    "site-packages",
    "coverage",
];

/// Files that mark a directory as a project root. Their presence is the signal a worker needs: it is
/// what makes `autogpt_platform/backend` a place you can `cd` into and run something.
const PROJECT_MANIFESTS: &[&str] = &[
    "package.json",
    "pyproject.toml",
    "setup.py",
    "Cargo.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "Gemfile",
    "composer.json",
    "CMakeLists.txt",
    "Makefile",
];

/// Total character budget. The prompt-composition audit behind FINDING-048 found task-specific text
/// was already only 5% of a 74.5k-char prompt; a layout that solves path-guessing by drowning the
/// task would trade one problem for a worse one.
const LAYOUT_BUDGET: usize = 1200;

/// Caps on breadth. A repo with 200 top-level entries is not made legible by listing all 200.
const MAX_TOP_LEVEL: usize = 32;
const MAX_CHILDREN: usize = 10;
/// Root files are the cheapest thing to rediscover (`ls`) and the least directional, so they get the
/// tightest cap — ragflow's 33 of them would otherwise be over half the map.
const MAX_ROOT_FILES: usize = 12;

/// Appended in place of whatever did not fit, so a reader can always tell a complete map from a
/// clipped one and knows the cheap way to get the rest.
pub(crate) const LAYOUT_TRUNCATED: &str = "; …truncated, run `ls` for the rest";

/// Joins one map entry to the next.
///
/// Named rather than written as a literal at the join site because its width is charged against the
/// budget: a hardcoded `2` at the accounting site and a `"; "` at the join site are free to drift,
/// and the first cut of this did exactly that — charging the separator for every entry when `join`
/// only writes one BETWEEN entries, so a map was billed 2 bytes it never spent (PR #157 review).
const LAYOUT_SEP: &str = "; ";

/// What `part` adds to the width of the joined map: itself, plus a separator only when something
/// already precedes it. `join` writes N-1 separators for N parts, so charging one per part bills the
/// map for bytes it never occupies.
fn joined_cost(part: &str, preceded: bool) -> usize {
    part.len() + if preceded { LAYOUT_SEP.len() } else { 0 }
}

/// A compact, deterministic map of what is at `dir`'s root — the thing no unit prompt carried.
///
/// FINDING-048: 0 of 32 prompts described the target tree, and 12 of 32 sessions burned turns on
/// `cd: no such file or directory` rediscovering that AutoGPT is a two-era monorepo. The worker knows
/// its task and nothing about where the task lives, so it guesses paths and pays for each miss.
///
/// SINGLE-LINE by contract, for the same reason as [`crate::assumptions::PROMPT_CONVENTION`]: the PTY
/// session runner writes a prompt line-based, so an embedded newline would end the turn early and send
/// the rest of the map as its own turn. `;` separates top-level entries, `{…}` holds a descent.
///
/// Deliberately shallow. Depth 1 always; depth 2 ONLY for a top-level directory that is not itself a
/// project root but contains ones — precisely the monorepo shape that produced the failures, and the
/// only case where the extra level carries information a worker cannot get from `ls`. Entries are
/// sorted, so the same tree yields the same string on every host and every run.
///
/// Returns `None` when `dir` cannot be read or has nothing worth reporting, so a caller that has no
/// worktree (or an empty one) appends nothing rather than an empty heading.
#[must_use]
pub(crate) fn worktree_layout(dir: &Path) -> Option<String> {
    worktree_layout_within(dir, LAYOUT_BUDGET)
}

/// [`worktree_layout`] against a caller-supplied character budget.
///
/// The PTY session runner needs this: it writes a prompt as ONE line, and a pty in canonical mode
/// discards any line that reaches `MAX_CANON` (1024 bytes) without ever delivering it, so the map has
/// to fit in whatever the rest of the prompt leaves rather than in a fixed 1200. A budget too small
/// for even one entry yields `None`, which reads as "no map" and costs the caller nothing.
#[must_use]
pub(crate) fn worktree_layout_within(dir: &Path, layout_budget: usize) -> Option<String> {
    let (dirs, files) = read_split(dir)?;
    if dirs.is_empty() && files.is_empty() {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();
    let mut budget = layout_budget;
    let mut truncated = dirs.len() > MAX_TOP_LEVEL;

    for name in dirs.iter().take(MAX_TOP_LEVEL) {
        let child = dir.join(name);
        let mut part = format!("{name}/");
        if let Some(m) = manifest_of(&child) {
            part.push_str(&format!(" [{m}]"));
        }
        // Both, not either. An earlier cut of this treated a manifest as "this is the project, stop
        // descending" — and AutoGPT, the repo the finding is ABOUT, rendered as
        // `autogpt_platform/ [Makefile]` with `backend/` and `frontend/` still invisible, because a
        // container can carry a Makefile that drives the projects underneath it. A directory being a
        // project root and being a container of project roots are independent facts; report both.
        if let Some(inner) = project_children(&child) {
            part.push_str(&format!(" {{{}}}", inner.join(", ")));
        }
        // The separator is only paid for when there is a previous part to join this one to.
        let cost = joined_cost(&part, !parts.is_empty());
        if cost > budget {
            truncated = true;
            break;
        }
        budget -= cost;
        parts.push(part);
    }

    // Root files last and cheaply: they matter far less than the directory shape, and a worker can
    // always `ls`. Listing them at all is what tells it whether the root IS the project.
    if !files.is_empty() {
        if files.len() > MAX_ROOT_FILES {
            truncated = true;
        }
        let joined = files
            .iter()
            .take(MAX_ROOT_FILES)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let part = format!("root files: {joined}");
        let cost = joined_cost(&part, !parts.is_empty());
        if cost <= budget {
            parts.push(part);
        } else {
            truncated = true;
        }
    }

    if parts.is_empty() {
        return None;
    }
    let mut out = parts.join(LAYOUT_SEP);
    if truncated {
        out.push_str(LAYOUT_TRUNCATED);
    }
    Some(out)
}

/// Split a directory into (subdirectory names, file names), both sorted, both filtered of hidden
/// entries and [`LAYOUT_NOISE`]. `None` if the dir cannot be read at all.
fn read_split(dir: &Path) -> Option<(Vec<String>, Vec<String>)> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Hidden entries are excluded wholesale — `.git` above all, which is enormous and never the
        // subject of a work unit.
        if name.starts_with('.') || LAYOUT_NOISE.contains(&name.as_str()) {
            continue;
        }
        match entry.file_type() {
            Ok(t) if t.is_dir() => dirs.push(name),
            Ok(_) => files.push(name),
            Err(_) => continue,
        }
    }
    dirs.sort();
    files.sort();
    Some((dirs, files))
}

/// The first [`PROJECT_MANIFESTS`] entry present directly in `dir`, if any. Order is the constant's
/// order, so the answer is stable for a directory carrying more than one.
fn manifest_of(dir: &Path) -> Option<&'static str> {
    PROJECT_MANIFESTS
        .iter()
        .copied()
        .find(|m| dir.join(m).is_file())
}

/// The child directories of `dir` that ARE project roots, rendered `name/ [manifest]`. `None` when
/// there are none — which is what keeps depth 2 from firing on ordinary nested directories.
fn project_children(dir: &Path) -> Option<Vec<String>> {
    let (dirs, _) = read_split(dir)?;
    let found: Vec<String> = dirs
        .iter()
        .filter_map(|name| manifest_of(&dir.join(name)).map(|m| format!("{name}/ [{m}]")))
        .take(MAX_CHILDREN)
        .collect();
    (!found.is_empty()).then_some(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_takes_four_kebab_words() {
        assert_eq!(slug("My Cool Repo Name Extra"), "my-cool-repo-name");
        assert_eq!(slug("!!!"), "repo");
    }

    /// A scratch tree, named per-test AND per-process so concurrent test binaries never collide.
    /// Each entry is `"a/b/c"` for a directory or `"a/b/file.ext"` for an (empty) file — the layout
    /// only ever looks at names and file-vs-dir, never at content.
    fn scratch(name: &str, entries: &[&str]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "wicked-layout-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for e in entries {
            let p = root.join(e);
            if p.extension().is_some() {
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, "").unwrap();
            } else {
                std::fs::create_dir_all(&p).unwrap();
            }
        }
        root
    }

    /// The exact shape FINDING-048 was about: `cd autogpt_platform/backend` is not guessable from
    /// depth 1, so depth 1 alone would have left the 12 failing sessions failing. Pinned as a whole
    /// string, which also holds the sort (a map that reorders between runs is not a stable prompt)
    /// and the single-line contract.
    ///
    /// `classic/` is the case that a first cut of this got WRONG and the real AutoGPT clone exposed:
    /// it carries a manifest AND contains project roots. Treating the manifest as "stop here" hid
    /// `autogpt_platform/backend` behind `autogpt_platform/ [Makefile]` — the one path the finding
    /// exists to surface. Both facts are reported.
    #[test]
    fn a_project_root_that_also_contains_projects_reports_both() {
        let root = scratch(
            "monorepo",
            &[
                "autogpt_platform/backend/pyproject.toml",
                "autogpt_platform/frontend/package.json",
                "classic/pyproject.toml",
                "classic/forge/setup.py",
                "docs/content",
                "README.md",
            ],
        );
        let map = worktree_layout(&root).expect("a populated tree has a map");
        assert_eq!(
            map,
            "autogpt_platform/ {backend/ [pyproject.toml], frontend/ [package.json]}; \
             classic/ [pyproject.toml] {forge/ [setup.py]}; docs/; root files: README.md"
        );
        // Stated separately from the equality above because it is a CONTRACT, not an incidental
        // property of this fixture: the PTY runner writes the prompt line-based.
        assert!(!map.contains('\n'), "the map must stay single-line: {map}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `.git` alone would dwarf the rest of the map, and no work unit is ever about `node_modules`.
    #[test]
    fn hidden_and_build_output_directories_never_reach_the_prompt() {
        let root = scratch(
            "noise",
            &[
                ".git/objects",
                ".venv/lib",
                "node_modules/react",
                "target/debug",
                "dist/bundle.js",
                "src/main.rs",
                "Cargo.toml",
            ],
        );
        let map = worktree_layout(&root).unwrap();
        assert_eq!(map, "src/; root files: Cargo.toml");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A budget that exactly fits the map spends all of it and clips nothing.
    ///
    /// The separator is only written BETWEEN entries, so an N-entry map pays for N-1 of them. An
    /// earlier cut charged one per entry (PR #157 review), which made a map cost 2 bytes more than
    /// it occupies — enough to drop the last entry and stamp a complete map `…truncated` a byte
    /// before it had to. Harmless-looking, but the pty path is precisely where the budget is small
    /// and computed, so 2 bytes is a whole root-files line there.
    ///
    /// Pinned at the exact boundary in both directions: one byte less genuinely does not fit.
    #[test]
    fn a_budget_that_exactly_fits_the_map_clips_nothing() {
        let root = scratch("exact", &["src/main.rs", "Cargo.toml"]);
        let whole = "src/; root files: Cargo.toml";
        // `whole.len()` rather than a literal: the point is that the budget equals the OUTPUT, and
        // hardcoding it would restate the same off-by-two this test exists to catch.
        let map = worktree_layout_within(&root, whole.len()).expect("an exactly-fitting map");
        assert_eq!(map, whole, "a budget equal to the map must not clip it");

        let tight = worktree_layout_within(&root, whole.len() - 1).unwrap();
        assert!(
            tight.ends_with(LAYOUT_TRUNCATED) && !tight.contains("Cargo.toml"),
            "one byte short must actually clip, or the budget means nothing: {tight}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Bounded, and HONEST about being bounded — a clipped map that reads as complete would send a
    /// worker looking for a directory it was simply never shown.
    #[test]
    fn a_wide_tree_is_clipped_and_says_so() {
        let mut entries: Vec<String> = (0..MAX_TOP_LEVEL + 5)
            .map(|i| format!("dir{i:03}"))
            .collect();
        entries.extend((0..MAX_ROOT_FILES + 5).map(|i| format!("file{i:03}.txt")));
        let refs: Vec<&str> = entries.iter().map(String::as_str).collect();
        let root = scratch("wide", &refs);
        let map = worktree_layout(&root).unwrap();
        assert!(
            map.ends_with(LAYOUT_TRUNCATED),
            "a clipped map must say so: {map}"
        );
        assert!(
            map.len() <= LAYOUT_BUDGET + LAYOUT_TRUNCATED.len(),
            "the map must stay inside its budget, got {} chars",
            map.len()
        );
        assert!(
            map.contains(&format!("dir{:03}/", MAX_TOP_LEVEL - 1))
                && !map.contains(&format!("dir{MAX_TOP_LEVEL:03}/")),
            "the directory cap is where it says it is: {map}"
        );
        assert!(
            map.contains(&format!("file{:03}.txt", MAX_ROOT_FILES - 1))
                && !map.contains(&format!("file{MAX_ROOT_FILES:03}.txt")),
            "root files get their own tighter cap: {map}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// No worktree, or an empty one, must append NOTHING — a bare heading over an empty map is worse
    /// than silence, because it reads as "this repo has nothing in it".
    #[test]
    fn nothing_worth_saying_yields_no_map_at_all() {
        assert_eq!(
            worktree_layout(&std::env::temp_dir().join("wicked-layout-does-not-exist")),
            None
        );
        let root = scratch("empty", &[]);
        assert_eq!(worktree_layout(&root), None);
        let hidden_only = scratch("hidden-only", &[".git/objects"]);
        assert_eq!(worktree_layout(&hidden_only), None);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&hidden_only);
    }

    #[test]
    fn repo_entry_round_trips_through_node() {
        let e = RepoEntry {
            id: "demo".into(),
            name: "Demo".into(),
            root_path: "/tmp/demo".into(),
            default_branch: "main".into(),
            registered_at: 42,
            code_graph_db: code_graph_db("/tmp/demo"),
        };
        assert_eq!(RepoEntry::from_node(&e.to_node()).unwrap(), e);
    }

    /// A record written before `code_graph_db` existed must read back with the path filled in, not
    /// with an empty string that a consumer would join onto or hand to a store opener.
    ///
    /// This is the arm that makes the field safe to add without a migration: `from_node` derives it
    /// from `root_path` and ignores whatever the metadata said, so every record ever persisted — and
    /// every record persisted by a future version that gets the derivation wrong — reads correct.
    #[test]
    fn a_record_predating_the_field_still_resolves_its_code_graph() {
        let mut node = RepoEntry {
            id: "legacy".into(),
            name: "Legacy".into(),
            root_path: "/tmp/legacy".into(),
            default_branch: "main".into(),
            registered_at: 7,
            code_graph_db: String::new(),
        }
        .to_node();
        node.metadata.remove("code_graph_db");
        // Also covers the stale case: a persisted value from before the repo moved.
        let mut moved = node.clone();
        moved.metadata.insert(
            "code_graph_db".into(),
            serde_json::Value::String("/somewhere/else/old.db".into()),
        );

        for n in [node, moved] {
            let back = RepoEntry::from_node(&n).unwrap();
            assert_eq!(back.code_graph_db, code_graph_db("/tmp/legacy"));
        }
    }

    /// The literal, pinned. Every out-of-process consumer joins this onto a repo root — crew did it in
    /// five places, and when the engine's spelling and crew's disagreed the worker got a database
    /// nothing had written (FINDING-069). Changing it is a coordinated release, not a rename.
    #[test]
    fn the_code_graph_spelling_is_the_one_consumers_expect() {
        assert_eq!(crate::code_graph::CODE_GRAPH_DB_REL, ".codegraph/estate.db");
        // Joined SEGMENT BY SEGMENT, so the separator is the platform's and a consumer's
        // `join(root, '.codegraph', 'estate.db')` produces a byte-identical string.
        //
        // This assertion is a no-op on Unix and load-bearing on Windows: `Path::join` given the whole
        // `.codegraph/estate.db` appends it as one component and leaves the `/` alone, which on Unix
        // is indistinguishable from doing it right. The first cut of this fix did exactly that and
        // only Windows CI could see it. Do not "simplify" the two `join`s back into one.
        assert_eq!(
            code_graph_db("/repo"),
            Path::new("/repo")
                .join(".codegraph")
                .join("estate.db")
                .to_string_lossy()
        );
    }

    // ── worktree isolation (FINDING-059) ──────────────────────────────────────
    //
    // The defect these pin was one stat: `create_worktree` returned any directory sitting at the
    // worktree path as an isolated checkout. The state that exploited it — an empty, unregistered
    // directory left by a partial `remove_worktree` — is cheap to construct, so it is constructed
    // here rather than described.

    /// A git repo with one commit at a scratch path. Identity and signing are set locally because
    /// `commit` fails without the first and can hang on the second, and neither is what these are
    /// about. Named per-process AND per-thread so concurrent test binaries never collide.
    fn git_repo(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "wicked-wt-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let p = root.to_str().unwrap();
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "t@example.invalid"],
            &["config", "user.name", "wicked-test"],
            &["config", "commit.gpgsign", "false"],
        ] {
            assert!(git(p, args).unwrap().0, "git {args:?} failed");
        }
        std::fs::write(root.join("README.md"), "base\n").unwrap();
        assert!(git(p, &["add", "-A"]).unwrap().0);
        assert!(git(p, &["commit", "-qm", "base"]).unwrap().0);
        root
    }

    /// core#214. `register_repo` must persist an ABSOLUTE, canonical root — never the caller's
    /// as-given spelling. A relative `--path ./repo` (or any `..`/symlink spelling) stored verbatim
    /// resolves to nothing from the daemon's cwd, and the `code_graph_db` derived from it inherits the
    /// break. Uses a non-canonical-but-absolute input (`<root>/../<name>`) so the falsification holds
    /// on every platform — on Linux CI `/tmp` is not a symlink, so ONLY the `..` guarantees a
    /// difference between as-given and canonical.
    #[test]
    fn register_repo_stores_a_canonical_absolute_root() {
        let root = git_repo("core214");
        let name = root.file_name().unwrap();
        let non_canonical = root.join("..").join(name);
        assert!(
            non_canonical.to_string_lossy().contains(".."),
            "precondition: the registered path is non-canonical: {}",
            non_canonical.display()
        );

        let mut store = wicked_apps_core::open_store(Some(":memory:")).unwrap();
        let entry = register_repo(
            &mut store,
            RepoSpec {
                name: "Core 214 Repo".into(),
                root_path: non_canonical.to_string_lossy().into_owned(),
                registered_at: 0,
            },
        )
        .unwrap();

        let canonical = std::fs::canonicalize(&root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            entry.root_path, canonical,
            "root_path is stored canonical + absolute, not as-given"
        );
        assert!(
            !entry.root_path.contains(".."),
            "the '..' must be resolved away: {}",
            entry.root_path
        );
        assert!(
            Path::new(&entry.code_graph_db).is_absolute()
                && entry.code_graph_db.starts_with(&canonical),
            "code_graph_db is absolute and under the canonical root: {}",
            entry.code_graph_db
        );

        // The persisted node round-trips to the SAME canonical paths (FromNode re-derives code_graph_db
        // from root_path), so a consumer reading the store back never sees the as-given spelling.
        let fetched = get_repo(&store, &entry.id).unwrap().unwrap();
        assert_eq!(fetched.root_path, canonical);
        assert_eq!(fetched.code_graph_db, entry.code_graph_db);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The heart of FINDING-059. `rev-parse` succeeds anywhere beneath the parent repo, including in
    /// an empty `.wicked/worktrees/<id>/` — so a check that trusts it (or trusts `is_dir`, as the
    /// original did) calls the parent repo's own working tree an isolated checkout.
    #[test]
    fn an_empty_dir_under_the_repo_is_not_a_worktree_though_rev_parse_succeeds_in_it() {
        let root = git_repo("revparse");
        let empty = worktrees_root(root.to_str().unwrap()).join("run-1");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(
            git(empty.to_str().unwrap(), &["rev-parse", "--git-dir"])
                .unwrap()
                .0,
            "precondition: rev-parse resolves here, which is the trap"
        );
        assert!(!is_live_worktree(&empty));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_worktree_reuses_a_live_worktree() {
        let root = git_repo("reuse");
        let p = root.to_str().unwrap();
        let first = create_worktree(p, "run-1").unwrap();
        std::fs::write(first.join("worker-output.txt"), "from turn 1\n").unwrap();

        let second = create_worktree(p, "run-1").unwrap();
        assert_eq!(first, second);
        // Reuse has to mean the same checkout, not a re-add that discards the last turn's work.
        assert_eq!(
            std::fs::read_to_string(second.join("worker-output.txt")).unwrap(),
            "from turn 1\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The observed state: `remove_worktree`'s `remove_dir_all` fallback stripped the `.git` file
    /// but left the directory, and the `prune` that follows deregistered the path *because* `.git`
    /// was gone. Recovery, not reuse — the shell is not a checkout.
    #[test]
    fn create_worktree_recovers_an_empty_shell_left_by_a_partial_removal() {
        let root = git_repo("shell");
        let p = root.to_str().unwrap();
        let wt = create_worktree(p, "run-1").unwrap();
        std::fs::remove_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        assert!(git(p, &["worktree", "prune"]).unwrap().0);
        assert!(!is_live_worktree(&wt), "precondition: shell, not worktree");

        let again = create_worktree(p, "run-1").unwrap();
        assert_eq!(again, wt);
        assert!(
            is_live_worktree(&again),
            "the run must get a real checkout, not the shell it was handed before"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Same shell, prune never ran, so git may still hold an admin entry for the path — and
    /// `worktree add` refuses a path it considers registered. The prune inside `create_worktree`
    /// is what keeps this recoverable.
    #[test]
    fn create_worktree_recovers_a_shell_whose_registration_was_never_pruned() {
        let root = git_repo("stale-reg");
        let p = root.to_str().unwrap();
        let wt = create_worktree(p, "run-1").unwrap();
        std::fs::remove_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&wt).unwrap();

        let again = create_worktree(p, "run-1").unwrap();
        assert!(
            is_live_worktree(&again),
            "worktrees still registered: {}",
            git(p, &["worktree", "list"]).unwrap().1
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The case that must fail the run rather than proceed: a directory of unknown provenance. It
    /// cannot be cleared (that would delete someone's work) and it cannot be handed over (that is
    /// the 297 lines on `master`), so the only honest answer is to stop.
    #[test]
    fn create_worktree_refuses_a_non_empty_directory_that_is_not_a_worktree() {
        let root = git_repo("occupied");
        let p = root.to_str().unwrap();
        let wt = worktrees_root(p).join("run-1");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join("someone-elses-work.txt"), "not ours\n").unwrap();

        let err = create_worktree(p, "run-1").unwrap_err().to_string();
        assert!(err.contains("is not a git worktree"), "{err}");
        // A refusal must not double as a delete.
        assert!(wt.join("someone-elses-work.txt").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// FINDING-003, the common case: a terminal run whose tree is clean loses the CHECKOUT but
    /// keeps the BRANCH — the branch is the durable record an operator reviews/merges; the
    /// worktree was scaffolding.
    #[test]
    fn reap_if_clean_removes_a_clean_worktree_but_never_its_branch() {
        let root = git_repo("reap-clean");
        let p = root.to_str().unwrap();
        let wt = create_worktree(p, "run-1").unwrap();
        assert!(is_live_worktree(&wt), "precondition: a real checkout");

        assert!(reap_worktree_if_clean(p, "run-1"), "a clean tree reaps");
        assert!(!wt.exists(), "the checkout is gone");
        let (ok, branches, _) = git(p, &["branch", "--list", "wicked/run-1"]).unwrap();
        assert!(
            ok && branches.contains("wicked/run-1"),
            "the wicked/run-1 branch must survive the reap — it is the record, got: {branches}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// FINDING-003, the safety property the whole design leans on: a tree holding uncommitted
    /// work (3 of the finding's 14 orphans did — the artifact-landing gap) is KEPT, bytes intact.
    /// If this fails, the reaper has become the thing that destroys the only copy of the work.
    #[test]
    fn reap_if_clean_keeps_a_dirty_worktree_and_its_unlanded_bytes() {
        let root = git_repo("reap-dirty");
        let p = root.to_str().unwrap();
        let wt = create_worktree(p, "run-1").unwrap();
        std::fs::write(wt.join("unlanded-artifact.txt"), "never committed\n").unwrap();

        assert!(
            !reap_worktree_if_clean(p, "run-1"),
            "a dirty tree must be reported KEPT"
        );
        assert!(
            is_live_worktree(&wt),
            "the dirty tree stays a live checkout, not a half-removed shell"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("unlanded-artifact.txt")).unwrap(),
            "never committed\n",
            "the uncommitted bytes are untouched"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// FINDING-003's restart half: the startup reaper must converge terminal runs' leftovers
    /// (14 survived restarts) while never touching a live run's checkout, and still force-clear
    /// worktrees whose run id the store has never heard of.
    #[test]
    fn startup_reaper_reaps_terminal_keeps_live_and_forces_unknown() {
        let root = git_repo("reap-startup");
        let p = root.to_str().unwrap();
        let wt_live = create_worktree(p, "run-live").unwrap();
        let wt_done = create_worktree(p, "run-done").unwrap();
        let wt_dirty = create_worktree(p, "run-dirty").unwrap();
        std::fs::write(wt_dirty.join("unlanded.txt"), "keep me\n").unwrap();
        let wt_gone = create_worktree(p, "run-unknown").unwrap();
        // The unknown-id worktree is dirty TOO — force removal is exactly the point there.
        std::fs::write(wt_gone.join("scratch.txt"), "no session owns this\n").unwrap();

        let repo = RepoEntry {
            id: "r".into(),
            name: "r".into(),
            root_path: p.to_string(),
            default_branch: "main".into(),
            registered_at: 0,
            code_graph_db: String::new(),
        };
        let live: HashSet<String> = ["run-live".to_string()].into_iter().collect();
        let terminal: HashSet<String> = ["run-done".to_string(), "run-dirty".to_string()]
            .into_iter()
            .collect();
        reap_orphan_worktrees(std::slice::from_ref(&repo), &live, &terminal);

        assert!(
            is_live_worktree(&wt_live),
            "a non-terminal run keeps its checkout across restarts (resume)"
        );
        assert!(!wt_done.exists(), "a clean terminal leftover converges");
        assert!(
            is_live_worktree(&wt_dirty) && wt_dirty.join("unlanded.txt").is_file(),
            "a dirty terminal leftover is kept — same rule as the terminal-status reap"
        );
        assert!(
            !wt_gone.exists(),
            "a worktree no session owns is force-removed, dirty or not"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
