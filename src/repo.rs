//! REPO REGISTRY — first-class, persistent registration of the git repositories the orchestrator
//! works within, plus the git-worktree isolation a run uses so the user's working tree is never
//! touched.
//!
//! A [`RepoEntry`] is a `Node(Other("repo_entry"))` on the shared estate store (mirrors the
//! `AgentSession` projection in [`crate::domain`]). A run that targets a registered repo gets its own
//! worktree at `<repo>/.wicked/worktrees/<run_id>` on branch `wicked/<run_id>`; the worker runs there
//! (augment mode — see `ORCHESTRATOR.md` §4). Worktrees are cleaned up on a terminal run status, and
//! an orphan reaper prunes stale ones on actor startup.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, FromNode, GraphRead, GraphStore, Language, Location, Node, NodeKind, Span,
    ToNode, SYMBOL_SCHEME,
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
        serde_json::from_value(serde_json::Value::Object(node.metadata.clone()))
            .map_err(|e| anyhow::anyhow!("node {} is not a valid RepoEntry: {e}", node.name))
    }
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
    let default_branch = validate_git_repo(&spec.root_path)?;
    let entry = RepoEntry {
        id: slug(&spec.name),
        name: spec.name,
        root_path: spec.root_path,
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

/// Create an isolated git worktree for `run_id` at `<repo>/.wicked/worktrees/<run_id>` on a fresh
/// `wicked/<run_id>` branch. Idempotent-ish: if the path already exists (a resumed run), it is
/// returned as-is. Returns the worktree path.
pub fn create_worktree(repo_root: &str, run_id: &str) -> anyhow::Result<PathBuf> {
    let wt = worktrees_root(repo_root).join(run_id);
    if wt.is_dir() {
        return Ok(wt); // already created (resume) — reuse it
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

/// Remove a run's worktree (best-effort — a failure to clean up is logged, not fatal).
pub fn remove_worktree(repo_root: &str, run_id: &str) {
    let wt = worktrees_root(repo_root).join(run_id);
    let wt_str = wt.to_string_lossy().to_string();
    let _ = git(repo_root, &["worktree", "remove", "--force", &wt_str]);
    // If git refused (e.g. already gone), drop the dir directly.
    if wt.is_dir() {
        let _ = std::fs::remove_dir_all(&wt);
    }
}

/// Prune worktrees whose run is no longer live: any `<repo>/.wicked/worktrees/<id>` whose `<id>` is
/// not in `live_run_ids`. Called on actor startup so a crashed run doesn't leak its worktree.
pub fn reap_orphan_worktrees(repos: &[RepoEntry], live_run_ids: &HashSet<String>) {
    for repo in repos {
        let root = worktrees_root(&repo.root_path);
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in entries.flatten() {
            if let Some(name) = e.file_name().to_str() {
                if !live_run_ids.contains(name) {
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
        // +2 for the "; " that will join this part to the previous one.
        if part.len() + 2 > budget {
            truncated = true;
            break;
        }
        budget -= part.len() + 2;
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
        if part.len() + 2 <= budget {
            parts.push(part);
        } else {
            truncated = true;
        }
    }

    if parts.is_empty() {
        return None;
    }
    let mut out = parts.join("; ");
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
        };
        assert_eq!(RepoEntry::from_node(&e.to_node()).unwrap(), e);
    }
}
