//! DOCS — markdown documents (recon docs from CLI enrichment, plus any notes) stored as plain files
//! under `~/.wicked/docs`, so they can be browsed AND edited in the UI and re-ingested into knowledge
//! for recall. Files, not opaque rows — editable by design.

use std::path::{Path, PathBuf};

use crate::sources::base_dir;

/// `~/.wicked/docs`.
pub fn docs_dir() -> PathBuf {
    base_dir().join("docs")
}

/// A markdown doc on disk.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct DocMeta {
    pub path: String,
    pub title: String,
    /// The source/origin this doc is about (from a `> source: …` line), if any.
    pub source: Option<String>,
}

fn title_of(content: &str, fallback: &str) -> String {
    content
        .lines()
        .find_map(|l| l.strip_prefix("# "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn source_of(content: &str) -> Option<String> {
    content
        .lines()
        .find_map(|l| l.strip_prefix("> source:"))
        .map(|s| s.trim().to_string())
}

/// List all docs (sorted by title).
pub fn list_docs() -> Vec<DocMeta> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(docs_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            let content = std::fs::read_to_string(&p).unwrap_or_default();
            let stem = p
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            out.push(DocMeta {
                path: p.to_string_lossy().to_string(),
                title: title_of(&content, &stem),
                source: source_of(&content),
            });
        }
    }
    out.sort_by_key(|d| d.title.to_lowercase());
    out
}

/// Read a doc's full markdown.
pub fn read_doc(path: &str) -> anyhow::Result<String> {
    std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("read doc {path}: {e}"))
}

/// Write (save) a doc's markdown, creating the dir if needed.
pub fn write_doc(path: &str, content: &str) -> anyhow::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content).map_err(|e| anyhow::anyhow!("write doc {path}: {e}"))
}

fn file_slug(title: &str) -> String {
    let s: String = title
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').replace("--", "-");
    if s.is_empty() {
        "doc".into()
    } else {
        s
    }
}

/// Create a new doc (slugged filename under `docs_dir`); returns its path. A `# <title>` header is
/// prepended if `content` doesn't already start with one.
pub fn new_doc(title: &str, content: &str) -> anyhow::Result<String> {
    std::fs::create_dir_all(docs_dir())?;
    let path = docs_dir().join(format!("{}.md", file_slug(title)));
    let body = if content.trim_start().starts_with("# ") {
        content.to_string()
    } else {
        format!("# {title}\n\n{content}")
    };
    std::fs::write(&path, body).map_err(|e| anyhow::anyhow!("create doc: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}
