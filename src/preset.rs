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

/// Write the built-in presets to the store (boot). Idempotent by name: an unchanged row is not
/// rewritten. Returns the names written.
pub fn seed_builtins(_store: &mut dyn GraphStore, _now_ms: i64) -> anyhow::Result<Vec<String>> {
    anyhow::bail!("seam C2: seed_builtins not built")
}

/// Save a preset (create or replace in its scope).
pub fn put_preset(
    _store: &mut dyn GraphStore,
    _spec: PresetSpec,
    _now_ms: i64,
) -> anyhow::Result<Preset> {
    anyhow::bail!("seam C2: put_preset not built")
}

/// Delete a preset in its scope. `Ok(false)` = no such live preset there.
pub fn delete_preset(
    _store: &mut dyn GraphStore,
    _name: &str,
    _project_id: Option<&str>,
    _now_ms: i64,
) -> anyhow::Result<bool> {
    anyhow::bail!("seam C2: delete_preset not built")
}

/// The presets a launch in `project_id` sees, by name.
pub fn list_presets(
    _store: &dyn GraphRead,
    _project_id: Option<&str>,
) -> anyhow::Result<Vec<Preset>> {
    anyhow::bail!("seam C2: list_presets not built")
}

/// The preset a launch naming `name` in `project_id` runs: the project's row, else the global one.
pub fn resolve(
    _store: &dyn GraphRead,
    _project_id: Option<&str>,
    _name: &str,
) -> anyhow::Result<Option<Preset>> {
    Ok(None)
}
