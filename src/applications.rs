//! APPLICATIONS — the top-level object. An application is a thing you're building/understanding; it
//! MUST start from one of three seeds — documentation, code, or a prompt — and then accretes one or
//! more code repos plus additional code/non-code documentation. Graphs, docs, and enrichment are all
//! facets OF an application, not standalone.
//!
//! Persisted as JSON at `~/.wicked/applications.json` (simple + portable; no estate schema coupling).

use std::path::PathBuf;

use crate::sources::base_dir;

/// How an application was seeded — every application starts from exactly one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedKind {
    Documentation,
    Code,
    Prompt,
}

impl SeedKind {
    pub fn label(&self) -> &'static str {
        match self {
            SeedKind::Documentation => "documentation",
            SeedKind::Code => "code",
            SeedKind::Prompt => "prompt",
        }
    }
}

/// One code repo tied to an application (indexed into its own code graph).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AppRepo {
    pub name: String,
    pub path: String,
    pub graph_db: String,
    #[serde(default)]
    pub origin: Option<String>,
}

/// One documentation item attached to an application (a markdown doc path; may be CODE docs or
/// NON-code docs that were imported/indexed).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AppDoc {
    pub title: String,
    pub path: String,
    /// `true` if this doc was derived from indexing code/docs (vs. a hand-written note).
    #[serde(default)]
    pub indexed: bool,
    /// Operator-dashboard slot this doc fills (e.g. "overview", "architecture", "apis", "building",
    /// "running") — set by onboarding runs. `None` = a free doc/note.
    #[serde(default)]
    pub slot: Option<String>,
    /// The onboarding run (session id) that produced this doc, if any.
    #[serde(default)]
    pub from_run: Option<String>,
}

/// An application.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Application {
    pub id: String,
    pub name: String,
    pub seed: SeedKind,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub repos: Vec<AppRepo>,
    #[serde(default)]
    pub docs: Vec<AppDoc>,
    pub created: i64,
}

fn store_path() -> PathBuf {
    base_dir().join("applications.json")
}

/// Load all applications (empty if none / unreadable).
pub fn list_apps() -> Vec<Application> {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_all(apps: &[Application]) -> anyhow::Result<()> {
    std::fs::create_dir_all(base_dir())?;
    let json = serde_json::to_string_pretty(apps)?;
    std::fs::write(store_path(), json).map_err(|e| anyhow::anyhow!("save applications: {e}"))
}

/// Fetch one application by id.
pub fn get_app(id: &str) -> Option<Application> {
    list_apps().into_iter().find(|a| a.id == id)
}

/// Upsert (insert or replace) an application.
pub fn save_app(app: &Application) -> anyhow::Result<()> {
    let mut apps = list_apps();
    match apps.iter_mut().find(|a| a.id == app.id) {
        Some(slot) => *slot = app.clone(),
        None => apps.push(app.clone()),
    }
    save_all(&apps)
}

/// Delete an application by id.
pub fn delete_app(id: &str) -> anyhow::Result<()> {
    let mut apps = list_apps();
    apps.retain(|a| a.id != id);
    save_all(&apps)
}

fn id_from(name: &str, now: i64) -> String {
    let slug: String = name
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
    let slug = slug.trim_matches('-');
    format!("{}-{now}", if slug.is_empty() { "app" } else { slug })
}

/// Create a new application from a seed. The caller supplies any seed artefacts already produced
/// (a first repo for Code, a first doc for Documentation, the text for Prompt).
pub fn create_app(
    name: &str,
    seed: SeedKind,
    prompt: Option<String>,
    first_repo: Option<AppRepo>,
    first_doc: Option<AppDoc>,
    now: i64,
) -> anyhow::Result<Application> {
    let app = Application {
        id: id_from(name, now),
        name: name.trim().to_string(),
        seed,
        prompt,
        repos: first_repo.into_iter().collect(),
        docs: first_doc.into_iter().collect(),
        created: now,
    };
    save_app(&app)?;
    Ok(app)
}

/// Attach a repo to an application (after it's been added/indexed).
pub fn attach_repo(app_id: &str, repo: AppRepo) -> anyhow::Result<()> {
    let mut app = get_app(app_id).ok_or_else(|| anyhow::anyhow!("no such application: {app_id}"))?;
    if !app.repos.iter().any(|r| r.path == repo.path) {
        app.repos.push(repo);
    }
    save_app(&app)
}

/// Attach a doc to an application.
pub fn attach_doc(app_id: &str, doc: AppDoc) -> anyhow::Result<()> {
    let mut app = get_app(app_id).ok_or_else(|| anyhow::anyhow!("no such application: {app_id}"))?;
    if !app.docs.iter().any(|d| d.path == doc.path) {
        app.docs.push(doc);
    }
    save_app(&app)
}
