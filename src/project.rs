//! PROJECT MODEL (DES-PROJECT-001) — the control-plane container whose members are work units
//! from any plane: crew runs, chats, repos, interactive docs. Projects and memberships are durable
//! control facts on the single-writer store, exactly like [`crate::repo::RepoEntry`]:
//! a [`Project`] is a `Node(Other("project"))`, a [`ProjectMember`] a
//! `Node(Other("project_member"))`, both written only by the actor.
//!
//! Shape notes (the ADR's `CREATE TABLE` rendered into this store's graph idiom):
//! - The ADR's `UNIQUE (project_id, member_kind, member_ref)` is implemented structurally: a
//!   member's node id is DERIVED (`pm_<hash16>` of exactly that triple), so a duplicate attach
//!   upserts the same node instead of minting a second row.
//! - The store has no node deletion, so detach is a tombstone (`detached_at`), mirroring the
//!   retire-not-delete contract governance uses. Detached members never list; re-attach clears
//!   the tombstone. "Removing the row removes nothing else" (ADR §1.3) holds either way.
//! - `member_kind` is an OPEN `<product>.<noun>` grammar — validated for shape, never enumerated,
//!   so a new plane's kind (`studio.session`) needs no engine edit (DES-EXEC-001 stability law).
//! - The reserved id `default` (the synthesized "Unfiled" project) is REJECTED everywhere here:
//!   it is an API-layer synthesis, never a stored row.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, FromNode, GraphRead, GraphStore, Language, Location, Node, NodeKind, Span,
    ToNode, SYMBOL_SCHEME,
};
use wicked_estate_core::SymbolQuery;

use crate::domain::put_node;

/// Node-kind for a project.
pub const PROJECT: &str = "project";
/// Node-kind for a project membership.
pub const PROJECT_MEMBER: &str = "project_member";
/// The reserved, synthesized "Unfiled" project id — never stored, never attachable (ADR §1.1/§7).
pub const DEFAULT_PROJECT_ID: &str = "default";
/// The `member_kind` the engine attaches at launch (ADR §2.2).
pub const MEMBER_KIND_RUN: &str = "crew.run";

/// Project lifecycle: `active ⇄ archived`, no hard delete (ADR §1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectStatus {
    Active,
    Archived,
}

impl ProjectStatus {
    /// Parse the wire token (`active` | `archived`) — fail closed on anything else.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "active" => Ok(Self::Active),
            "archived" => Ok(Self::Archived),
            other => anyhow::bail!(
                "unrecognised project status '{other}' (expected 'active' or 'archived')"
            ),
        }
    }
}

/// A named, control-plane-owned container (ADR §1). Persisted as `Node(Other("project"))`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// Stable id (`proj_<sortable>`), minted at create — never derived from the name (§1.1).
    pub id: String,
    /// Human label, 1–120 chars. Unique among ACTIVE projects (enforced here, surfaced as 409).
    pub name: String,
    /// Optional free-text description.
    #[serde(default)]
    pub description: Option<String>,
    /// Lifecycle status.
    pub status: ProjectStatus,
    /// The estate scope path for this project's record — STORED, not derived on read, so a future
    /// tenancy prefix (`org:<o>/project:<id>`) can arrive without renaming the grammar (ADR §3.1).
    pub scope: String,
    /// Creation timestamp (unix millis).
    pub created_at: i64,
    /// Last-update timestamp (unix millis).
    pub updated_at: i64,
}

impl ToNode for Project {
    fn node_kind() -> &'static str {
        PROJECT
    }
    fn to_node(&self) -> Node {
        let mut node = Node::new(
            synthetic_symbol(PROJECT, &self.id),
            NodeKind::Other(PROJECT.to_string()),
            self.id.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{PROJECT}/{}", self.id), Span::ZERO),
        );
        if let serde_json::Value::Object(map) =
            serde_json::to_value(self).expect("Project serializes to JSON")
        {
            node.metadata = map;
        }
        node
    }
}

impl FromNode for Project {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == PROJECT => {}
            other => anyhow::bail!("expected NodeKind::Other({PROJECT:?}), got {other:?}"),
        }
        serde_json::from_value(serde_json::Value::Object(node.metadata.clone()))
            .map_err(|e| anyhow::anyhow!("node {} is not a valid Project: {e}", node.name))
    }
}

/// A typed, opaque membership reference (ADR §1.2). Persisted as `Node(Other("project_member"))`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMember {
    /// Derived id: `pm_` + hash16(project_id, member_kind, member_ref) — the UNIQUE constraint.
    pub id: String,
    /// The owning project id.
    pub project_id: String,
    /// `<product>.<noun>` — open grammar, shape-validated only.
    pub member_kind: String,
    /// Opaque to the engine; only `crew.*` kinds are resolved (at the API layer, not here).
    pub member_ref: String,
    /// Optional skin hints (doc root, display title, …), carried verbatim as JSON text.
    #[serde(default)]
    pub meta: Option<String>,
    /// Attach timestamp (unix millis).
    pub attached_at: i64,
    /// The attaching surface: `studio` | `interactive` | `cli` | `api`.
    pub attached_by: String,
    /// Detach tombstone (unix millis). `Some` ⇒ not a member; re-attach clears it. The store has
    /// no node deletion, so this is the graph-idiom spelling of the ADR's DELETE (retire-not-delete).
    #[serde(default)]
    pub detached_at: Option<i64>,
}

impl ToNode for ProjectMember {
    fn node_kind() -> &'static str {
        PROJECT_MEMBER
    }
    fn to_node(&self) -> Node {
        let mut node = Node::new(
            synthetic_symbol(PROJECT_MEMBER, &self.id),
            NodeKind::Other(PROJECT_MEMBER.to_string()),
            self.id.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{PROJECT_MEMBER}/{}", self.id), Span::ZERO),
        );
        if let serde_json::Value::Object(map) =
            serde_json::to_value(self).expect("ProjectMember serializes to JSON")
        {
            node.metadata = map;
        }
        node
    }
}

impl FromNode for ProjectMember {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == PROJECT_MEMBER => {}
            other => anyhow::bail!("expected NodeKind::Other({PROJECT_MEMBER:?}), got {other:?}"),
        }
        serde_json::from_value(serde_json::Value::Object(node.metadata.clone()))
            .map_err(|e| anyhow::anyhow!("node {} is not a valid ProjectMember: {e}", node.name))
    }
}

/// A partial update for [`update_project`]. `None` = leave unchanged.
#[derive(Debug, Clone, Default)]
pub struct ProjectPatch {
    pub name: Option<String>,
    /// `Some("")` clears the description.
    pub description: Option<String>,
    pub status: Option<ProjectStatus>,
}

/// What a caller asks to attach (ADR §1.2). The member id is derived, never supplied.
#[derive(Debug, Clone)]
pub struct MemberSpec {
    pub project_id: String,
    pub member_kind: String,
    pub member_ref: String,
    /// Optional skin hints, as JSON text (validated as JSON at the API layer, opaque here).
    pub meta: Option<String>,
    pub attached_by: String,
}

/// Mint a lowercase, time-sortable project id: `proj_<millis:013><seq:05>`. The seq disambiguates
/// same-millisecond creates. Sortability comes from here; UNIQUENESS does not — a counter alone
/// cannot promise it across process restarts (Copilot, PR #246), so [`create_project`] verifies
/// the minted id against the store and re-mints on the (pathological) hit. The seq deliberately
/// does NOT wrap: past 99_999 in one process lifetime the id merely widens, which stays unique
/// and stays lexicographically sortable for ids sharing a millisecond prefix width.
pub fn mint_project_id(now_ms: i64) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("proj_{:013}{:05}", now_ms.max(0), seq)
}

/// Shape-validate a `member_kind`: exactly `<product>.<noun>`, lowercase alnum/`-`/`_` segments.
fn validate_member_kind(kind: &str) -> anyhow::Result<()> {
    let ok = matches!(kind.split('.').collect::<Vec<_>>().as_slice(),
        [product, noun] if !product.is_empty() && !noun.is_empty()
            && kind.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-' || c == '_'));
    if !ok {
        anyhow::bail!(
            "invalid member kind '{kind}' (expected '<product>.<noun>', lowercase, e.g. 'crew.run')"
        );
    }
    Ok(())
}

/// Validate a project name: 1–120 chars after trimming.
fn validate_name(name: &str) -> anyhow::Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 120 {
        anyhow::bail!("project name must be 1–120 characters");
    }
    Ok(trimmed.to_string())
}

/// Create a project: validate the name, enforce active-name uniqueness (the API's 409), mint the
/// id + scope, persist. `description: Some("")` is normalized to `None`.
pub fn create_project(
    store: &mut dyn GraphStore,
    name: &str,
    description: Option<String>,
    now_ms: i64,
) -> anyhow::Result<Project> {
    let name = validate_name(name)?;
    if list_projects(store)?
        .iter()
        .any(|p| p.status == ProjectStatus::Active && p.name == name)
    {
        anyhow::bail!("project name '{name}' is already in use by an active project");
    }
    // Uniqueness is VERIFIED, not assumed: the seq counter resets on restart, so the mint alone
    // cannot rule out a collision with a stored id. An upsert on a hit would silently overwrite
    // an existing project — re-mint instead (the seq advances each try; termination is bounded).
    let mut id = mint_project_id(now_ms);
    for _ in 0..8 {
        if get_project(store, &id)?.is_none() {
            break;
        }
        id = mint_project_id(now_ms);
    }
    if get_project(store, &id)?.is_some() {
        anyhow::bail!("could not mint a unique project id (8 collisions — store anomaly?)");
    }
    let project = Project {
        scope: format!("project:{id}"),
        id,
        name,
        description: description.filter(|d| !d.trim().is_empty()),
        status: ProjectStatus::Active,
        created_at: now_ms,
        updated_at: now_ms,
    };
    put_node(store, project.to_node())?;
    Ok(project)
}

/// Every project on the store (all statuses — the API filters), newest first.
pub fn list_projects(store: &dyn GraphRead) -> anyhow::Result<Vec<Project>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(PROJECT.to_string())],
        ..Default::default()
    };
    let mut projects: Vec<Project> = store
        .find_symbols(&query)?
        .iter()
        .filter_map(|n| Project::from_node(n).ok())
        // The reserved id can never be created through this module, but a migrated or hand-edited
        // store could carry one — filter it so the "synthesized, never stored" invariant holds on
        // READ regardless of what the store holds (Copilot, PR #246).
        .filter(|p| p.id != DEFAULT_PROJECT_ID)
        .collect();
    projects.sort_by(|a, b| b.id.cmp(&a.id)); // ids are time-sortable → newest first
    Ok(projects)
}

/// Read one project by id (`None` for unknown — and always `None` for the synthesized `default`).
pub fn get_project(store: &dyn GraphRead, id: &str) -> anyhow::Result<Option<Project>> {
    // Enforce the reserved-id contract at the read seam itself, not just in the writers: even a
    // store carrying a rogue `default` node must never surface it as a real project.
    if id == DEFAULT_PROJECT_ID {
        return Ok(None);
    }
    match store.get_node(&synthetic_symbol(PROJECT, id))? {
        Some(node) => Ok(Some(Project::from_node(&node)?)),
        None => Ok(None),
    }
}

/// Rename / describe / archive / restore (ADR §1.3: `active ⇄ archived`, no hard delete).
pub fn update_project(
    store: &mut dyn GraphStore,
    id: &str,
    patch: ProjectPatch,
    now_ms: i64,
) -> anyhow::Result<Project> {
    if id == DEFAULT_PROJECT_ID {
        anyhow::bail!("the synthesized 'default' project cannot be modified");
    }
    let mut project =
        get_project(store, id)?.ok_or_else(|| anyhow::anyhow!("project not registered: {id}"))?;
    if let Some(name) = patch.name {
        let name = validate_name(&name)?;
        if name != project.name
            && list_projects(store)?
                .iter()
                .any(|p| p.id != project.id && p.status == ProjectStatus::Active && p.name == name)
        {
            anyhow::bail!("project name '{name}' is already in use by an active project");
        }
        project.name = name;
    }
    if let Some(description) = patch.description {
        project.description = Some(description).filter(|d| !d.trim().is_empty());
    }
    if let Some(status) = patch.status {
        project.status = status;
    }
    project.updated_at = now_ms;
    put_node(store, project.to_node())?;
    Ok(project)
}

/// Validate an attach without persisting: the project must exist, be active, and not be the
/// synthesized `default`; the kind must parse. `Ok(Some(_))` ⇒ already attached (idempotent hit),
/// `Ok(None)` ⇒ clear to attach. Split from [`attach_member`] so the launch path can validate
/// BEFORE creating the run stub and then write both nodes in ONE batch (ADR §2.2 atomicity).
pub fn validate_attach(
    store: &dyn GraphRead,
    spec: &MemberSpec,
) -> anyhow::Result<Option<ProjectMember>> {
    if spec.project_id == DEFAULT_PROJECT_ID {
        anyhow::bail!("cannot attach members to the synthesized 'default' project");
    }
    validate_member_kind(&spec.member_kind)?;
    if spec.member_ref.trim().is_empty() {
        anyhow::bail!("member ref must not be empty");
    }
    let project = get_project(store, &spec.project_id)?
        .ok_or_else(|| anyhow::anyhow!("project not registered: {}", spec.project_id))?;
    if project.status == ProjectStatus::Archived {
        anyhow::bail!(
            "project {} is archived and blocks new attachments",
            spec.project_id
        );
    }
    let id = member_id(&spec.project_id, &spec.member_kind, &spec.member_ref);
    match store.get_node(&synthetic_symbol(PROJECT_MEMBER, &id))? {
        Some(node) => {
            let existing = ProjectMember::from_node(&node)?;
            // A detached tombstone is attachable again; a live row is the idempotent hit.
            Ok(Some(existing).filter(|m| m.detached_at.is_none()))
        }
        None => Ok(None),
    }
}

/// The derived member node id — the ADR's `UNIQUE (project_id, member_kind, member_ref)`.
pub fn member_id(project_id: &str, member_kind: &str, member_ref: &str) -> String {
    format!(
        "pm_{}",
        crate::pipeline::deterministic_id(&[project_id, member_kind, member_ref])
    )
}

/// Build the member row for a validated spec (does not persist — see [`validate_attach`]).
pub fn member_from_spec(spec: &MemberSpec, now_ms: i64) -> ProjectMember {
    ProjectMember {
        id: member_id(&spec.project_id, &spec.member_kind, &spec.member_ref),
        project_id: spec.project_id.clone(),
        member_kind: spec.member_kind.clone(),
        member_ref: spec.member_ref.clone(),
        meta: spec.meta.clone(),
        attached_at: now_ms,
        attached_by: spec.attached_by.clone(),
        detached_at: None,
    }
}

/// Attach a member. Idempotent: an existing live row is returned unchanged with `created=false`
/// (no second row, no event on the caller's side); a tombstoned row is revived (`created=true`).
pub fn attach_member(
    store: &mut dyn GraphStore,
    spec: MemberSpec,
    now_ms: i64,
) -> anyhow::Result<(ProjectMember, bool)> {
    if let Some(existing) = validate_attach(store, &spec)? {
        return Ok((existing, false));
    }
    let member = member_from_spec(&spec, now_ms);
    put_node(store, member.to_node())?;
    Ok((member, true))
}

/// Detach a member (tombstone). `Ok(false)` if the member does not exist, is already detached, or
/// belongs to a different project — the caller answers 404 rather than a silent success.
pub fn detach_member(
    store: &mut dyn GraphStore,
    project_id: &str,
    member_id: &str,
    now_ms: i64,
) -> anyhow::Result<bool> {
    let Some(node) = store.get_node(&synthetic_symbol(PROJECT_MEMBER, member_id))? else {
        return Ok(false);
    };
    let mut member = ProjectMember::from_node(&node)?;
    if member.project_id != project_id || member.detached_at.is_some() {
        return Ok(false);
    }
    member.detached_at = Some(now_ms);
    put_node(store, member.to_node())?;
    Ok(true)
}

/// Every LIVE member of `project_id`, oldest attach first.
pub fn list_members(store: &dyn GraphRead, project_id: &str) -> anyhow::Result<Vec<ProjectMember>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(PROJECT_MEMBER.to_string())],
        ..Default::default()
    };
    let mut members: Vec<ProjectMember> = store
        .find_symbols(&query)?
        .iter()
        .filter_map(|n| ProjectMember::from_node(n).ok())
        .filter(|m| m.project_id == project_id && m.detached_at.is_none())
        .collect();
    members.sort_by(|a, b| a.attached_at.cmp(&b.attached_at).then(a.id.cmp(&b.id)));
    Ok(members)
}

/// The project ids holding a LIVE membership for `(member_kind, member_ref)` — the reverse read
/// the daemon uses to tag a run's frames and scope its memory (many-to-many by design; §9.4).
pub fn member_projects(
    store: &dyn GraphRead,
    member_kind: &str,
    member_ref: &str,
) -> anyhow::Result<Vec<String>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(PROJECT_MEMBER.to_string())],
        ..Default::default()
    };
    let mut ids: Vec<String> = store
        .find_symbols(&query)?
        .iter()
        .filter_map(|n| ProjectMember::from_node(n).ok())
        .filter(|m| {
            m.member_kind == member_kind && m.member_ref == member_ref && m.detached_at.is_none()
        })
        .map(|m| m.project_id)
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Member refs of one kind across ALL live memberships, with their project ids — the daemon's
/// "which runs are explicitly filed" read that synthesizes the `default` project (ADR §7).
pub fn members_of_kind(
    store: &dyn GraphRead,
    member_kind: &str,
) -> anyhow::Result<Vec<ProjectMember>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(PROJECT_MEMBER.to_string())],
        ..Default::default()
    };
    Ok(store
        .find_symbols(&query)?
        .iter()
        .filter_map(|n| ProjectMember::from_node(n).ok())
        .filter(|m| m.member_kind == member_kind && m.detached_at.is_none())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wicked_apps_core::open_store;

    fn mem_store() -> wicked_apps_core::SqliteStore {
        open_store(Some(":memory:")).unwrap()
    }

    #[test]
    fn project_round_trips_through_node() {
        let p = Project {
            id: "proj_000000000000100001".into(),
            name: "keystone".into(),
            description: Some("the e2e project".into()),
            status: ProjectStatus::Active,
            scope: "project:proj_000000000000100001".into(),
            created_at: 42,
            updated_at: 43,
        };
        assert_eq!(Project::from_node(&p.to_node()).unwrap(), p);
    }

    #[test]
    fn member_round_trips_through_node() {
        let m = ProjectMember {
            id: member_id("proj_x", "crew.run", "run-1"),
            project_id: "proj_x".into(),
            member_kind: "crew.run".into(),
            member_ref: "run-1".into(),
            meta: Some(r#"{"title":"t"}"#.into()),
            attached_at: 7,
            attached_by: "api".into(),
            detached_at: None,
        };
        assert_eq!(ProjectMember::from_node(&m.to_node()).unwrap(), m);
    }

    #[test]
    fn create_enforces_name_rules_and_active_uniqueness() {
        let mut store = mem_store();
        let p = create_project(&mut store, "  keystone  ", None, 1).unwrap();
        assert_eq!(p.name, "keystone"); // trimmed
        assert_eq!(p.scope, format!("project:{}", p.id));
        assert!(create_project(&mut store, "keystone", None, 2).is_err()); // active collision
        assert!(create_project(&mut store, "", None, 3).is_err());
        assert!(create_project(&mut store, &"x".repeat(121), None, 4).is_err());
        // Archiving frees the name (ADR §1.1).
        update_project(
            &mut store,
            &p.id,
            ProjectPatch {
                status: Some(ProjectStatus::Archived),
                ..Default::default()
            },
            5,
        )
        .unwrap();
        assert!(create_project(&mut store, "keystone", None, 6).is_ok());
    }

    #[test]
    fn attach_is_idempotent_and_detach_tombstones() {
        let mut store = mem_store();
        let p = create_project(&mut store, "p", None, 1).unwrap();
        let spec = MemberSpec {
            project_id: p.id.clone(),
            member_kind: "interactive.doc".into(),
            member_ref: "brief".into(),
            meta: None,
            attached_by: "interactive".into(),
        };
        let (m1, created1) = attach_member(&mut store, spec.clone(), 2).unwrap();
        let (m2, created2) = attach_member(&mut store, spec.clone(), 3).unwrap();
        assert!(created1);
        assert!(!created2, "duplicate attach must be the idempotent hit");
        assert_eq!(m1.id, m2.id);
        assert_eq!(m2.attached_at, 2, "idempotent hit returns the original row");
        assert_eq!(list_members(&store, &p.id).unwrap().len(), 1);
        assert!(detach_member(&mut store, &p.id, &m1.id, 4).unwrap());
        assert!(list_members(&store, &p.id).unwrap().is_empty());
        assert!(
            !detach_member(&mut store, &p.id, &m1.id, 5).unwrap(),
            "double detach reports false (404), not silent success"
        );
        // Re-attach revives the tombstone as a NEW attachment.
        let (m3, created3) = attach_member(&mut store, spec, 6).unwrap();
        assert!(created3);
        assert_eq!(m3.attached_at, 6);
        assert_eq!(list_members(&store, &p.id).unwrap().len(), 1);
    }

    #[test]
    fn archived_projects_block_new_attachments() {
        let mut store = mem_store();
        let p = create_project(&mut store, "p", None, 1).unwrap();
        update_project(
            &mut store,
            &p.id,
            ProjectPatch {
                status: Some(ProjectStatus::Archived),
                ..Default::default()
            },
            2,
        )
        .unwrap();
        let err = attach_member(
            &mut store,
            MemberSpec {
                project_id: p.id.clone(),
                member_kind: "crew.run".into(),
                member_ref: "r1".into(),
                meta: None,
                attached_by: "api".into(),
            },
            3,
        )
        .unwrap_err();
        assert!(err.to_string().contains("archived"));
        // Restore re-opens it.
        update_project(
            &mut store,
            &p.id,
            ProjectPatch {
                status: Some(ProjectStatus::Active),
                ..Default::default()
            },
            4,
        )
        .unwrap();
        assert!(attach_member(
            &mut store,
            MemberSpec {
                project_id: p.id,
                member_kind: "crew.run".into(),
                member_ref: "r1".into(),
                meta: None,
                attached_by: "api".into(),
            },
            5,
        )
        .is_ok());
    }

    #[test]
    fn default_project_is_rejected_everywhere() {
        let mut store = mem_store();
        assert!(
            update_project(&mut store, DEFAULT_PROJECT_ID, ProjectPatch::default(), 1).is_err()
        );
        assert!(validate_attach(
            &store,
            &MemberSpec {
                project_id: DEFAULT_PROJECT_ID.into(),
                member_kind: "crew.run".into(),
                member_ref: "r".into(),
                meta: None,
                attached_by: "api".into(),
            },
        )
        .is_err());
    }

    #[test]
    fn member_kind_grammar_is_shape_validated_not_enumerated() {
        for ok in ["crew.run", "interactive.doc", "studio.session", "a-b.c_d"] {
            assert!(validate_member_kind(ok).is_ok(), "{ok} should parse");
        }
        for bad in [
            "run",
            "crew.",
            ".run",
            "crew.run.extra",
            "Crew.Run",
            "crew run",
        ] {
            assert!(
                validate_member_kind(bad).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn member_projects_reverse_read() {
        let mut store = mem_store();
        let p1 = create_project(&mut store, "p1", None, 1).unwrap();
        let p2 = create_project(&mut store, "p2", None, 2).unwrap();
        for pid in [&p1.id, &p2.id] {
            attach_member(
                &mut store,
                MemberSpec {
                    project_id: pid.clone(),
                    member_kind: "crew.repo".into(),
                    member_ref: "shared-repo".into(),
                    meta: None,
                    attached_by: "api".into(),
                },
                3,
            )
            .unwrap();
        }
        let mut expect = vec![p1.id, p2.id];
        expect.sort();
        assert_eq!(
            member_projects(&store, "crew.repo", "shared-repo").unwrap(),
            expect
        );
        assert!(member_projects(&store, "crew.run", "shared-repo")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn ids_are_time_sortable_and_list_is_newest_first() {
        let a = mint_project_id(1_000);
        let b = mint_project_id(2_000);
        assert!(b > a);
        let mut store = mem_store();
        create_project(&mut store, "older", None, 1_000).unwrap();
        create_project(&mut store, "newer", None, 2_000).unwrap();
        let names: Vec<String> = list_projects(&store)
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["newer".to_string(), "older".to_string()]);
    }
}
