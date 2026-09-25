//! Presets (DES-TEAMING-002 §8.4, seam C2): a preset is ONLY a saved phase selection — a named
//! `steps[]` over the phase catalog — stored as a row in the core estate store.
//!
//! Rows are `Node(Other("plan_preset"))` with `{name, scope, steps[], created_by, updated_at}`,
//! where `scope` is `"global"` or `"project:<id>"`. They are written through
//! `Core::{put_preset, delete_preset, list_presets}` (the actor is the single writer) and read at
//! launch by [`resolve`], which is what makes `LaunchSpec.workflow` name a preset: the campaign
//! driver, the bus launch bridge and `Core::launch_run` all reach the same actor resolver, so no
//! launcher resolves a preset itself.
//!
//! Built-in presets are code data (`crate::catalog::builtin_presets`), written to the store at
//! boot by [`seed_builtins`] (idempotent by name, `created_by: "builtin"`). A user cannot delete a
//! built-in or overwrite it globally; a project-scoped preset of the same name shadows it for that
//! project only.

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, FromNode, GraphRead, GraphStore, Language, Location, Node, NodeKind, Span,
    ToNode, SYMBOL_SCHEME,
};

use crate::plan::PlanStep;

/// Node-kind for a preset row.
pub const PLAN_PRESET: &str = "plan_preset";
/// The scope of a preset every project sees.
pub const GLOBAL_SCOPE: &str = "global";
/// `created_by` of a seeded built-in.
pub const BUILTIN_CREATED_BY: &str = "builtin";

/// One stored preset (DES-TEAMING-002 §8.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preset {
    /// The name a launch's `workflow` field names.
    pub name: String,
    /// `"global"` or `"project:<id>"`.
    pub scope: String,
    /// The saved phase selection.
    pub steps: Vec<PlanStep>,
    /// `"builtin"` for a seeded built-in; otherwise the writing surface.
    pub created_by: String,
    /// Last write (unix millis).
    pub updated_at: i64,
    /// Delete tombstone (unix millis): the store has no node deletion, so a deleted preset is a
    /// row with this set, and it never lists or resolves. A later put clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<i64>,
}

/// What a caller asks to save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetSpec {
    pub name: String,
    /// `None` ⇒ global; `Some(id)` ⇒ `project:<id>` (the project must exist).
    pub project_id: Option<String>,
    pub steps: Vec<PlanStep>,
    /// The writing surface (`api`, `studio`, …). `"builtin"` is reserved.
    pub created_by: String,
}

/// Why a preset write was refused. Every variant has a stable [`reason`](PresetError::reason)
/// token, which leads the message so a caller across the napi string boundary can match on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetError {
    /// The name is not `[A-Za-z0-9._-]{1,64}`.
    InvalidName(String),
    /// A global write or a delete aimed at a built-in.
    BuiltinReadonly(String),
    /// `created_by: "builtin"` from a caller.
    ReservedCreatedBy,
    /// The project scope names no project.
    UnknownProject(String),
    /// The steps do not compose (the named catalog refusal).
    InvalidSteps(String),
}

impl PresetError {
    pub fn reason(&self) -> &'static str {
        match self {
            PresetError::InvalidName(_) => "preset_invalid_name",
            PresetError::BuiltinReadonly(_) => "preset_builtin_readonly",
            PresetError::ReservedCreatedBy => "preset_reserved_created_by",
            PresetError::UnknownProject(_) => "preset_unknown_project",
            PresetError::InvalidSteps(_) => "preset_invalid_steps",
        }
    }
}

impl std::fmt::Display for PresetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let r = self.reason();
        match self {
            PresetError::InvalidName(n) => write!(
                f,
                "{r}: preset name `{n}` must be 1-64 characters of letters, digits, `.`, `_` or `-`"
            ),
            PresetError::BuiltinReadonly(n) => write!(
                f,
                "{r}: `{n}` is a built-in preset — it cannot be deleted or overwritten globally; \
                 save a project-scoped preset of that name to shadow it for one project"
            ),
            PresetError::ReservedCreatedBy => {
                write!(
                    f,
                    "{r}: created_by `builtin` is reserved for seeded built-ins"
                )
            }
            PresetError::UnknownProject(p) => write!(f, "{r}: no project `{p}`"),
            PresetError::InvalidSteps(e) => write!(f, "{r}: {e}"),
        }
    }
}
impl std::error::Error for PresetError {}

/// The scope string for a project id (`None` ⇒ global).
pub fn scope_for(project_id: Option<&str>) -> String {
    match project_id {
        Some(p) => format!("project:{p}"),
        None => GLOBAL_SCOPE.to_string(),
    }
}

fn row_id(scope: &str, name: &str) -> String {
    format!("{scope}/{name}")
}

impl ToNode for Preset {
    fn node_kind() -> &'static str {
        PLAN_PRESET
    }
    fn to_node(&self) -> Node {
        let id = row_id(&self.scope, &self.name);
        let mut node = Node::new(
            synthetic_symbol(PLAN_PRESET, &id),
            NodeKind::Other(PLAN_PRESET.to_string()),
            self.name.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{PLAN_PRESET}/{id}"), Span::ZERO),
        );
        if let serde_json::Value::Object(map) =
            serde_json::to_value(self).expect("Preset serializes to JSON")
        {
            node.metadata = map;
        }
        node
    }
}

impl FromNode for Preset {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == PLAN_PRESET => {}
            other => anyhow::bail!("expected NodeKind::Other({PLAN_PRESET:?}), got {other:?}"),
        }
        serde_json::from_value(serde_json::Value::Object(node.metadata.clone()))
            .map_err(|e| anyhow::anyhow!("node {} is not a valid Preset: {e}", node.name))
    }
}

/// `[A-Za-z0-9._-]{1,64}` — every workflow id in use today fits, and `:` (per-run def ids) and
/// `/` (the row id separator) cannot appear.
fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn is_builtin_name(name: &str) -> bool {
    crate::catalog::builtin_presets()
        .iter()
        .any(|(n, _)| *n == name)
}

/// The live row at `(scope, name)`, if any.
fn get_row(store: &dyn GraphRead, scope: &str, name: &str) -> anyhow::Result<Option<Preset>> {
    let Some(node) = store.get_node(&synthetic_symbol(PLAN_PRESET, &row_id(scope, name)))? else {
        return Ok(None);
    };
    let preset = Preset::from_node(&node)?;
    Ok((preset.deleted_at.is_none()).then_some(preset))
}

/// Compose a preset's steps over the catalog into the def a launch runs. The def is named after
/// the preset, so the run reports the name it launched.
pub fn compose_preset(
    preset: &Preset,
) -> Result<crate::workflow::WorkflowDef, crate::plan::PlanRefusal> {
    let plan = crate::plan::PlanSteps {
        steps: preset.steps.clone(),
        ..crate::plan::PlanSteps::default()
    };
    let mut def = crate::plan::compose(crate::catalog::catalog(), &plan)?;
    def.id = preset.name.clone();
    Ok(def)
}

/// Write the built-in presets to the store (boot). Idempotent by name: a row that already holds
/// the built-in's steps is not rewritten. Returns the names written.
pub fn seed_builtins(store: &mut dyn GraphStore, now_ms: i64) -> anyhow::Result<Vec<String>> {
    let mut rows = Vec::new();
    for (name, steps) in crate::catalog::builtin_presets() {
        let current = get_row(store, GLOBAL_SCOPE, name)?;
        if current
            .as_ref()
            .is_some_and(|p| p.created_by == BUILTIN_CREATED_BY && p.steps == steps)
        {
            continue;
        }
        rows.push(Preset {
            name: name.to_string(),
            scope: GLOBAL_SCOPE.to_string(),
            steps,
            created_by: BUILTIN_CREATED_BY.to_string(),
            updated_at: now_ms,
            deleted_at: None,
        });
    }
    if !rows.is_empty() {
        let nodes: Vec<Node> = rows.iter().map(ToNode::to_node).collect();
        crate::domain::put_nodes(store, &nodes)?;
    }
    Ok(rows.into_iter().map(|p| p.name).collect())
}

/// Save a preset (create or replace in its scope). Refused for a bad name, the reserved
/// `created_by`, an unknown project, steps that do not compose, and a global write to a built-in's
/// name (a project-scoped one shadows it instead).
pub fn put_preset(
    store: &mut dyn GraphStore,
    spec: PresetSpec,
    now_ms: i64,
) -> anyhow::Result<Preset> {
    if !valid_name(&spec.name) {
        return Err(PresetError::InvalidName(spec.name).into());
    }
    if spec.created_by == BUILTIN_CREATED_BY {
        return Err(PresetError::ReservedCreatedBy.into());
    }
    match spec.project_id.as_deref() {
        None if is_builtin_name(&spec.name) => {
            return Err(PresetError::BuiltinReadonly(spec.name).into());
        }
        None => {}
        // `get_project` is always `None` for the synthesized `default`, so it is refused here too:
        // like members, a preset cannot be filed under a project that is never stored.
        Some(p) if crate::project::get_project(store, p)?.is_none() => {
            return Err(PresetError::UnknownProject(p.to_string()).into());
        }
        Some(_) => {}
    }
    let preset = Preset {
        name: spec.name,
        scope: scope_for(spec.project_id.as_deref()),
        steps: spec.steps,
        created_by: spec.created_by,
        updated_at: now_ms,
        deleted_at: None,
    };
    compose_preset(&preset).map_err(|e| PresetError::InvalidSteps(e.to_string()))?;
    crate::domain::put_node(store, preset.to_node())?;
    Ok(preset)
}

/// Delete a preset in its scope (a tombstone: the store has no node deletion). `Ok(false)` = no
/// such live preset there. A built-in is refused.
pub fn delete_preset(
    store: &mut dyn GraphStore,
    name: &str,
    project_id: Option<&str>,
    now_ms: i64,
) -> anyhow::Result<bool> {
    let scope = scope_for(project_id);
    let Some(mut preset) = get_row(store, &scope, name)? else {
        return Ok(false);
    };
    if preset.created_by == BUILTIN_CREATED_BY {
        return Err(PresetError::BuiltinReadonly(name.to_string()).into());
    }
    preset.deleted_at = Some(now_ms);
    preset.updated_at = now_ms;
    crate::domain::put_node(store, preset.to_node())?;
    Ok(true)
}

/// The presets a launch in `project_id` sees, sorted by name: every live global preset, with the
/// project's row replacing the global row of the same name. `None` ⇒ the global set.
pub fn list_presets(
    store: &dyn GraphRead,
    project_id: Option<&str>,
) -> anyhow::Result<Vec<Preset>> {
    let query = wicked_estate_core::SymbolQuery {
        kinds: vec![NodeKind::Other(PLAN_PRESET.to_string())],
        ..Default::default()
    };
    let project_scope = project_id.map(|p| scope_for(Some(p)));
    let mut by_name: std::collections::BTreeMap<String, Preset> = Default::default();
    let mut rows: Vec<Preset> = store
        .find_symbols(&query)?
        .iter()
        .filter_map(|n| Preset::from_node(n).ok())
        .filter(|p| p.deleted_at.is_none())
        .filter(|p| p.scope == GLOBAL_SCOPE || Some(&p.scope) == project_scope.as_ref())
        .collect();
    // Global first, so the project's row overwrites it by name.
    rows.sort_by_key(|p| p.scope != GLOBAL_SCOPE);
    for p in rows {
        by_name.insert(p.name.clone(), p);
    }
    Ok(by_name.into_values().collect())
}

/// The preset a launch naming `name` in `project_id` runs: the project's live row, else the
/// global one. `None` ⇒ no preset of that name (the caller falls back to the registered defs).
pub fn resolve(
    store: &dyn GraphRead,
    project_id: Option<&str>,
    name: &str,
) -> anyhow::Result<Option<Preset>> {
    if let Some(p) = project_id {
        if let Some(preset) = get_row(store, &scope_for(Some(p)), name)? {
            return Ok(Some(preset));
        }
    }
    get_row(store, GLOBAL_SCOPE, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wicked_apps_core::open_store;

    fn mem_store() -> wicked_apps_core::SqliteStore {
        open_store(Some(":memory:")).unwrap()
    }

    fn understand_only() -> Vec<PlanStep> {
        vec![PlanStep {
            catalog: "understand".into(),
            id: "u".into(),
            ..Default::default()
        }]
    }

    fn user(name: &str, project: Option<&str>) -> PresetSpec {
        PresetSpec {
            name: name.into(),
            project_id: project.map(str::to_string),
            steps: understand_only(),
            created_by: "api".into(),
        }
    }

    #[test]
    fn a_preset_round_trips_through_its_node() {
        let p = Preset {
            name: "my-flow".into(),
            scope: "project:proj_1".into(),
            steps: understand_only(),
            created_by: "studio".into(),
            updated_at: 7,
            deleted_at: None,
        };
        let node = p.to_node();
        assert_eq!(node.kind, NodeKind::Other("plan_preset".into()));
        assert_eq!(Preset::from_node(&node).unwrap(), p);
    }

    #[test]
    fn seeding_is_idempotent_by_name() {
        let mut store = mem_store();
        assert_eq!(seed_builtins(&mut store, 10).unwrap(), ["feature"]);
        assert!(seed_builtins(&mut store, 20).unwrap().is_empty());
        let f = resolve(&store, None, "feature").unwrap().unwrap();
        assert_eq!((f.created_by.as_str(), f.updated_at), ("builtin", 10));
    }

    #[test]
    fn seeding_repairs_a_stale_builtin_row() {
        let mut store = mem_store();
        let stale = Preset {
            name: "feature".into(),
            scope: GLOBAL_SCOPE.into(),
            steps: understand_only(),
            created_by: BUILTIN_CREATED_BY.into(),
            updated_at: 1,
            deleted_at: None,
        };
        crate::domain::put_node(&mut store, stale.to_node()).unwrap();
        assert_eq!(seed_builtins(&mut store, 5).unwrap(), ["feature"]);
        let f = resolve(&store, None, "feature").unwrap().unwrap();
        assert_eq!(f.steps.len(), 6);
    }

    #[test]
    fn names_are_checked() {
        for ok in [
            "feature",
            "domain-graph-slice",
            "qe.author_tests",
            &"a".repeat(64),
        ] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "a b", "run:plan-1", "a/b", &"a".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_reserved_created_by_is_refused() {
        let mut store = mem_store();
        let mut spec = user("x", None);
        spec.created_by = BUILTIN_CREATED_BY.into();
        let err = put_preset(&mut store, spec, 1).unwrap_err();
        assert_eq!(
            err.downcast_ref::<PresetError>(),
            Some(&PresetError::ReservedCreatedBy)
        );
    }

    #[test]
    fn a_put_after_a_delete_revives_the_name() {
        let mut store = mem_store();
        put_preset(&mut store, user("x", None), 1).unwrap();
        assert!(delete_preset(&mut store, "x", None, 2).unwrap());
        assert!(resolve(&store, None, "x").unwrap().is_none());
        put_preset(&mut store, user("x", None), 3).unwrap();
        let x = resolve(&store, None, "x").unwrap().unwrap();
        assert_eq!((x.updated_at, x.deleted_at), (3, None));
    }

    #[test]
    fn a_preset_cannot_be_filed_under_the_synthesized_default_project() {
        let mut store = mem_store();
        let err = put_preset(
            &mut store,
            user("x", Some(crate::project::DEFAULT_PROJECT_ID)),
            1,
        )
        .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<PresetError>(),
            Some(PresetError::UnknownProject(_))
        ));
        assert!(
            list_presets(&store, Some(crate::project::DEFAULT_PROJECT_ID))
                .unwrap()
                .iter()
                .all(|p| p.scope != "project:default")
        );
    }

    #[test]
    fn a_project_row_shadows_only_its_project() {
        let mut store = mem_store();
        seed_builtins(&mut store, 1).unwrap();
        let pid = crate::project::create_project(&mut store, "alpha", None, 1)
            .unwrap()
            .id;
        put_preset(&mut store, user("feature", Some(&pid)), 2).unwrap();
        assert_eq!(
            resolve(&store, Some(&pid), "feature")
                .unwrap()
                .unwrap()
                .steps
                .len(),
            1
        );
        assert_eq!(
            resolve(&store, None, "feature")
                .unwrap()
                .unwrap()
                .steps
                .len(),
            6
        );
        assert_eq!(
            resolve(&store, Some("other"), "feature")
                .unwrap()
                .unwrap()
                .steps
                .len(),
            6
        );
        let listed: Vec<_> = list_presets(&store, Some(&pid))
            .unwrap()
            .into_iter()
            .map(|p| (p.name, p.scope))
            .collect();
        assert_eq!(listed, [("feature".to_string(), format!("project:{pid}"))]);
    }

    #[test]
    fn the_composed_def_is_named_after_the_preset() {
        let p = Preset {
            name: "my-flow".into(),
            scope: GLOBAL_SCOPE.into(),
            steps: understand_only(),
            created_by: "api".into(),
            updated_at: 0,
            deleted_at: None,
        };
        let def = compose_preset(&p).unwrap();
        assert_eq!(def.id, "my-flow");
        assert_eq!(def.phases.len(), 1);
    }
}
