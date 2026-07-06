//! REPO INTEL — the architect's-eye view of a repo: git history + churn hotspots, plus graph
//! intelligence from the indexer (stats, entry points, dead code). Everything here is computed from
//! the repo + its code graph — NO agent CLI required — so it's always available.

use std::collections::HashMap;
use std::process::Command;

use crate::code_graph::indexer_bin;

/// A recent commit (subject + author + relative time).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Commit {
    pub hash: String,
    pub author: String,
    pub when: String,
    pub subject: String,
}

/// A churn hotspot — a file changed often (a place a developer should be careful / focus).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Hotspot {
    pub file: String,
    pub changes: usize,
}

/// A code symbol reference (entry point / dead code).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct CodeRef {
    pub name: String,
    pub file: String,
    pub kind: String,
}

/// Graph-level stats (from the indexer).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct GraphStats {
    pub files: usize,
    pub nodes: usize,
    pub edges: usize,
    pub branch: String,
    pub dirty: bool,
}

/// The full architect profile of one repo.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RepoProfile {
    pub stats: GraphStats,
    pub recent: Vec<Commit>,
    pub hotspots: Vec<Hotspot>,
    pub entrypoints: Vec<CodeRef>,
    pub dead_code: Vec<CodeRef>,
}

fn git(repo: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// The last `n` commits (subject, author, relative time).
pub fn recent_commits(repo: &str, n: usize) -> Vec<Commit> {
    let fmt = "--pretty=format:%h%x09%an%x09%ar%x09%s";
    let Some(out) = git(repo, &["log", fmt, &format!("-n{n}")]) else {
        return vec![];
    };
    out.lines()
        .filter_map(|l| {
            let mut p = l.splitn(4, '\t');
            Some(Commit {
                hash: p.next()?.to_string(),
                author: p.next()?.to_string(),
                when: p.next()?.to_string(),
                subject: p.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// Churn hotspots — files changed most often over the PAST 6 MONTHS.
pub fn hotspots(repo: &str, n: usize) -> Vec<Hotspot> {
    let Some(out) = git(
        repo,
        &["log", "--since=6.months.ago", "--name-only", "--pretty=format:"],
    ) else {
        return vec![];
    };
    let mut counts: HashMap<String, usize> = HashMap::new();
    for line in out.lines() {
        let l = line.trim();
        if !l.is_empty() {
            *counts.entry(l.to_string()).or_default() += 1;
        }
    }
    let mut v: Vec<Hotspot> = counts
        .into_iter()
        .map(|(file, changes)| Hotspot { file, changes })
        .collect();
    v.sort_by(|a, b| b.changes.cmp(&a.changes).then(a.file.cmp(&b.file)));
    v.truncate(n);
    v
}

/// Commits over the past `days` (subject, author, relative time), capped at `max` — for the Recent
/// Changes view's time-window toggle.
pub fn commits_since(repo: &str, days: u32, max: usize) -> Vec<Commit> {
    let fmt = "--pretty=format:%h%x09%an%x09%ar%x09%s";
    let since = format!("--since={days}.days.ago");
    let Some(out) = git(repo, &["log", fmt, &since, &format!("-n{max}")]) else {
        return vec![];
    };
    out.lines()
        .filter_map(|l| {
            let mut p = l.splitn(4, '\t');
            Some(Commit {
                hash: p.next()?.to_string(),
                author: p.next()?.to_string(),
                when: p.next()?.to_string(),
                subject: p.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// A compact diff digest over the past `days` (`--stat` of files + churn) — fed to the change-review
/// agent's brief so the controlled run reviews the ACTUAL changes, not just commit subjects.
pub fn change_digest_since(repo: &str, days: u32) -> String {
    let since = format!("--since={days}.days.ago");
    git(repo, &["log", &since, "--stat", "--pretty=format:%h %an · %ar%n  %s"])
        .map(|s| s.chars().take(16_000).collect())
        .unwrap_or_default()
}

/// Parse the indexer's `--json` `[{file,kind,name}]` output.
fn indexer_refs(graph_db: &str, subcommand: &str, n: usize) -> Vec<CodeRef> {
    let out = Command::new(indexer_bin())
        .args([subcommand, "--json", "--db", graph_db])
        .output();
    let Ok(out) = out else { return vec![] };
    if !out.status.success() {
        return vec![];
    }
    let json = String::from_utf8_lossy(&out.stdout);
    let mut refs: Vec<CodeRef> = serde_json::from_str(&json).unwrap_or_default();
    refs.truncate(n);
    refs
}

/// Graph stats (`nodes=.. edges=.. files=..` + `repo: branch=.. dirty`).
pub fn graph_stats(graph_db: &str) -> GraphStats {
    let mut s = GraphStats::default();
    let Ok(out) = Command::new(indexer_bin())
        .args(["stats", "--db", graph_db])
        .output()
    else {
        return s;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let kv = |key: &str| -> Option<usize> {
        text.split_whitespace()
            .find_map(|t| t.strip_prefix(key))
            .and_then(|v| v.parse().ok())
    };
    s.nodes = kv("nodes=").unwrap_or(0);
    s.edges = kv("edges=").unwrap_or(0);
    s.files = kv("files=").unwrap_or(0);
    if let Some(line) = text.lines().find(|l| l.contains("branch=")) {
        if let Some(b) = line
            .split_whitespace()
            .find_map(|t| t.strip_prefix("branch="))
        {
            s.branch = b.to_string();
        }
        s.dirty = line.contains("dirty");
    }
    s
}

/// The full architect profile for `repo` (with its indexed `graph_db`). Best-effort: each field is
/// empty/default if its source is unavailable.
pub fn profile_repo(repo: &str, graph_db: &str) -> RepoProfile {
    RepoProfile {
        stats: graph_stats(graph_db),
        recent: recent_commits(repo, 8),
        hotspots: hotspots(repo, 6),
        entrypoints: indexer_refs(graph_db, "entrypoints", 8),
        dead_code: indexer_refs(graph_db, "dead-code", 8),
    }
}
