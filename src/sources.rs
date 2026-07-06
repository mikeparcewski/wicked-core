//! CODE SOURCES — add a codebase to explore WITHOUT launching a run: clone a remote URL (or accept a
//! local path) so it can be indexed into a code graph and enriched by CLIs. Standalone from the repo
//! registry used for runs.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::execute_wrapped::build_argv;

/// `~/.wicked` — the managed base dir for cloned sources + docs.
pub fn base_dir() -> PathBuf {
    std::env::var_os("WICKED_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".wicked")))
        .unwrap_or_else(|| PathBuf::from(".wicked"))
}

/// `~/.wicked/sources` — where cloned remote sources live.
pub fn sources_dir() -> PathBuf {
    base_dir().join("sources")
}

fn slug(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "source".into()
    } else {
        s
    }
}

/// Does `s` look like a git remote (clone) rather than a local path?
pub fn is_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("git@")
        || s.starts_with("ssh://")
        || s.ends_with(".git")
}

/// Add a code source: clone a remote URL into `~/.wicked/sources/<name>`, or accept an existing local
/// path. Returns the on-disk repo path (ready to index).
pub fn add_source(origin: &str, name: &str) -> anyhow::Result<String> {
    let origin = origin.trim();
    if origin.is_empty() {
        anyhow::bail!("empty source origin");
    }
    if is_url(origin) {
        std::fs::create_dir_all(sources_dir())?;
        let dest = sources_dir().join(slug(name));
        if dest.join(".git").exists() {
            // Already cloned → ensure FULL history (older clones were shallow → churn / recent-changes
            // need real history), then fast-forward to latest (best-effort).
            if Command::new("git")
                .arg("-C")
                .arg(&dest)
                .args(["rev-parse", "--is-shallow-repository"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
                .unwrap_or(false)
            {
                let _ = Command::new("git").arg("-C").arg(&dest).args(["fetch", "--unshallow"]).output();
            }
            let _ = Command::new("git")
                .arg("-C")
                .arg(&dest)
                .args(["pull", "--ff-only"])
                .output();
            return Ok(dest.to_string_lossy().into());
        }
        // Blobless PARTIAL clone: full COMMIT history (so git log / churn / recent-changes work) but
        // blobs fetched lazily — fast + small, unlike `--depth 1` which has no history.
        let out = Command::new("git")
            .args(["clone", "--filter=blob:none", origin])
            .arg(&dest)
            .output()
            .map_err(|e| anyhow::anyhow!("could not run git clone ({e}); is git installed?"))?;
        if !out.status.success() {
            anyhow::bail!(
                "git clone failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(dest.to_string_lossy().into())
    } else {
        let p = Path::new(origin);
        if !p.exists() {
            anyhow::bail!("path does not exist: {origin}");
        }
        Ok(p.to_string_lossy().into())
    }
}

/// The result of one CLI enriching a source.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ReconDoc {
    pub cli: String,
    pub title: String,
    /// The saved markdown doc path (browsable + editable in the Docs view).
    pub doc_path: String,
    /// The recon markdown itself (so the caller can ingest it into knowledge).
    pub body: String,
    /// How many symbol annotations this CLI wrote to the graph.
    pub annotations: usize,
}

/// Run a CLI invocation with a prompt in `cwd`, killing it after `timeout`. Returns stdout, or None on
/// spawn failure / timeout / non-zero exit.
fn run_cli(invocation: &str, prompt: &str, cwd: &Path, timeout: Duration) -> Option<String> {
    let argv = build_argv(invocation, prompt);
    let (bin, rest) = argv.split_first()?;
    let mut child = Command::new(bin)
        .args(rest)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(mut so) = child.stdout.take() {
                    use std::io::Read;
                    let _ = so.read_to_string(&mut out);
                }
                return status.success().then_some(out);
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// The recon prompt for one CLI: analyse the codebase + tag the central symbols.
fn recon_prompt(repo: &str, top: &[(String, String)]) -> String {
    let list = top
        .iter()
        .map(|(name, file)| format!("- {name} ({file})"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are doing a fast architecture recon of the codebase at {repo}.\n\
         Its most central symbols (by PageRank) are:\n{list}\n\n\
         Produce concise markdown with three short sections — ## Capabilities, ## Seams, ## Gaps.\n\
         Then a final section '## Symbol notes' with one line per symbol in the exact form\n\
         `SYMBOL: <one-line role>` (use the symbol names above)."
    )
}

/// Parse `SYMBOL: note` lines from a recon body, matching only the known symbol names.
fn parse_symbol_notes(body: &str, known: &[(String, String)]) -> Vec<(String, String)> {
    let names: std::collections::HashSet<&str> = known.iter().map(|(n, _)| n.as_str()).collect();
    body.lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(name, note)| (name.trim().to_string(), note.trim().to_string()))
        .filter(|(name, note)| names.contains(name.as_str()) && !note.is_empty())
        .collect()
}

/// Attach a manual NOTE to a graph node (by stable symbol id). Shows in the node's detail + feeds
/// recall; provenance = manual, author = the person/agent who wrote it.
pub fn add_node_note(graph_db: &str, node_id: &str, note: &str, author: &str) -> anyhow::Result<()> {
    let bin = crate::code_graph::indexer_bin();
    let out = Command::new(&bin)
        .args(["annotate", "--symbol", node_id, "--key", "note", "--value", note])
        .args(["--type", "note", "--provenance", "manual", "--author", author])
        .args(["--db", graph_db])
        .output()
        .map_err(|e| anyhow::anyhow!("could not run the indexer to annotate ({e})"))?;
    if !out.status.success() {
        anyhow::bail!(
            "annotate failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Index a documentation path (a folder or file of code/non-code docs) into a graph db so its content
/// becomes part of the application's graph. Returns the graph db path.
pub fn index_docs(path: &str, graph_db: &str) -> anyhow::Result<String> {
    let bin = crate::code_graph::indexer_bin();
    if let Some(parent) = Path::new(graph_db).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let out = Command::new(&bin)
        .args(["index", path, "--db", graph_db])
        .output()
        .map_err(|e| anyhow::anyhow!("could not run the indexer ({e})"))?;
    if !out.status.success() {
        anyhow::bail!(
            "indexing docs failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(graph_db.to_string())
}

/// Write a `role` annotation for `symbol` onto the graph (provenance = enrichment, author = the CLI).
fn annotate_symbol(graph_db: &str, symbol: &str, note: &str, cli: &str) -> bool {
    let bin = crate::code_graph::indexer_bin();
    Command::new(&bin)
        .args(["annotate", symbol, "--key", "role", "--value", note])
        .args(["--type", "note", "--provenance", "enrichment", "--author", cli])
        .args(["--db", graph_db])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Enrich a code source with one or more CLIs: each runs a recon pass over the codebase + ranked
/// symbols, producing (a) a recon doc (saved as markdown, returned for knowledge ingest) and
/// (b) per-symbol `role` annotations written onto the graph (provenance per CLI). "Both."
pub fn enrich_source(
    repo: &str,
    graph_db: &str,
    source_name: &str,
    top: &[(String, String)],
    clis: &[(String, String)],
    timeout_secs: u64,
) -> Vec<ReconDoc> {
    let cwd = Path::new(repo);
    let mut docs = Vec::new();
    for (key, invocation) in clis {
        let prompt = recon_prompt(repo, top);
        let Some(body) = run_cli(invocation, &prompt, cwd, Duration::from_secs(timeout_secs)) else {
            continue;
        };
        let notes = parse_symbol_notes(&body, top);
        let mut written = 0usize;
        for (sym, note) in &notes {
            if annotate_symbol(graph_db, sym, note, key) {
                written += 1;
            }
        }
        let title = format!("{key} recon · {source_name}");
        let md = format!("# {title}\n\n> source: {repo}\n> cli: {key}\n\n{}", body.trim());
        let doc_path = crate::docs::new_doc(&title, &md).unwrap_or_default();
        docs.push(ReconDoc {
            cli: key.clone(),
            title,
            doc_path,
            body: md,
            annotations: written,
        });
    }
    docs
}
