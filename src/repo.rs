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
    synthetic_symbol, FromNode, GraphRead, Language, Location, Node, NodeKind, Span, SqliteStore,
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
pub fn register_repo(store: &mut SqliteStore, spec: RepoSpec) -> anyhow::Result<RepoEntry> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_takes_four_kebab_words() {
        assert_eq!(slug("My Cool Repo Name Extra"), "my-cool-repo-name");
        assert_eq!(slug("!!!"), "repo");
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
