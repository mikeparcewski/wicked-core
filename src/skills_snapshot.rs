//! The immutable skills snapshot every worker is handed (core#396; skills keystone design v3,
//! DECISION 2/3).
//!
//! # The defect
//!
//! Skills are files, and neither worker path could see them. The ACP path re-sanitizes the
//! engine-owned config home on every spawn — `plugins/` is one of the vectors it deletes — and the
//! wrapped path drops user scope (`--setting-sources project,local`), so the operator's
//! marketplace plugins never load either. The engine kept telling workers to `Invoke your skill
//! "wicked-garden:<dir>"` (`execute_wrapped::plugin_skill_invocation`) while guaranteeing no such
//! skill was loaded. The operator's stop-gap was a `clis.toml` template pinning `--plugin-dir` at
//! a stale hand copy.
//!
//! # The fix
//!
//! Crew PUBLISHES immutable snapshots — a garden-shaped plugin root holding only the enabled
//! skills, their dependency closure, and a `snapshot.json` index — and passes exactly ONE input to
//! the engine: [`SKILLS_SNAPSHOT_ENV`], the concrete path of the generation to use. Core hands that
//! path to each worker through the mechanism its CLI actually has, and COPIES NOTHING:
//!
//! - Claude over ACP: `session/new` carries `_meta.claudeCode.options.plugins =
//!   [{type: "local", path: <snapshot>}]` (`acp_runner::start_acp_process_with_write_roots`);
//! - Claude wrapped: `--plugin-dir <snapshot>` (`execute_wrapped::inject_isolation_flags`), with
//!   the template's own `--plugin-dir` stripped as superseded;
//! - both governance carriers read-widen to the snapshot (`execute_wrapped::assemble_read_roots`);
//!   writes under it stay denied — a snapshot is immutable by contract, and this module never
//!   writes into one;
//! - the skill directive is CLI-aware (`execute_wrapped::plugin_skill_invocation`): the plugin
//!   form for Claude, the mirrored directory name for every other CLI.
//!
//! # Degradation ladder (v3 §3) — no unconditional fail-open
//!
//! - env UNSET ⇒ `skills.fallback` is logged and the LIVE installed garden is used: the
//!   marketplace cache's highest version under the daemon's `CLAUDE_CONFIG_DIR` (else
//!   `~/.claude`). NOT the hand copy at `<config>/plugins/wicked-garden` — that stale copy is the
//!   defect being fixed, never a fallback. Nothing in the cache ⇒ no root at all (a run that needs
//!   a skill is then refused below, a run that needs none proceeds). Resolving crew's `current`
//!   link is crew's job — it passes the concrete generation; the engine has no other input;
//! - env SET to a path that is missing, unreadable, or not a snapshot (no `plugin.json`, no
//!   parseable `snapshot.json`) ⇒ the launch FAILS with a config error — a misconfigured snapshot is
//!   never silently swapped for a fallback. A path that IS a symlink (crew's `current`) is pinned
//!   to the concrete generation it points at when loaded, so a session keeps the generation it was
//!   handed even if the link is flipped under it;
//! - before a unit launches, every `skill_ref` its run names ([`StepInput::required_skills`] plus
//!   the unit's own) must resolve in the snapshot — by frontmatter `name`, else by the dir-derived
//!   name garden's convention guarantees equal — or the launch is REFUSED naming the missing
//!   skills ([`SkillsError::Missing`]). Never a silent proceed without the required method.
//!
//! The generation in use is reported at every launch — the `skills.snapshot gen=…` log line
//! ([`SkillsSnapshot::report`]) and the [`CoreEvent::SkillsSnapshotHanded`] event
//! ([`SkillsSnapshot::handed_event`]) — so crew can reap old generations only once no live session
//! references them.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::event::CoreEvent;
use crate::workflow::StepInput;

/// The one input crew passes: the ABSOLUTE path of the published snapshot generation to use.
pub(crate) const SKILLS_SNAPSHOT_ENV: &str = "WICKED_SKILLS_SNAPSHOT";

/// The plugin's name — the `<plugin>` half of the `wicked-garden:<dir>` identity Claude uses, the
/// marketplace + cache directory name, and the prefix every skill's frontmatter `name` carries.
pub(crate) const PLUGIN_NAME: &str = "wicked-garden";

/// The harness's plugin manifest, relative to a plugin root. Its presence is what makes a
/// directory a plugin root at all.
const PLUGIN_MANIFEST: &str = ".claude-plugin/plugin.json";

/// Crew's index of a published snapshot, relative to its root:
/// `{gen, contentHash, skills: [{name, dir, kind, core, portable}]}`.
const SNAPSHOT_INDEX: &str = "snapshot.json";

/// The skills subtree of a plugin root, and the file that marks a directory as a skill.
const SKILLS_DIR: &str = "skills";
const SKILL_FILE: &str = "SKILL.md";

/// Where a skills root came from — reported with every launch so an operator can tell a published
/// generation from the installed-plugin fallback at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotSource {
    /// The snapshot [`SKILLS_SNAPSHOT_ENV`] named — crew-published, immutable.
    Published,
    /// The live marketplace cache under the daemon's claude config dir (its highest version).
    LiveCache,
}

impl SnapshotSource {
    /// The wire token ([`CoreEvent::SkillsSnapshotHanded`]`.source`, the `fallback=` log token).
    pub(crate) fn token(self) -> &'static str {
        match self {
            SnapshotSource::Published => "published",
            SnapshotSource::LiveCache => "live-cache",
        }
    }
}

impl std::fmt::Display for SnapshotSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SnapshotSource::Published => "published snapshot",
            SnapshotSource::LiveCache => "live plugin cache",
        })
    }
}

/// One skill the root holds. `name` is the frontmatter `name` (the mirrored directory name every
/// non-Claude CLI invokes it by); `dir` is its path under `skills/`, `/`-separated on every OS,
/// nested verbatim (`engineering/frontend`) — never renamed, sibling links depend on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillEntry {
    pub name: String,
    pub dir: String,
}

/// A resolved skills root: WHERE it is, where it came from, and WHAT it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillsSnapshot {
    /// The plugin root a worker is pointed at.
    pub root: PathBuf,
    pub source: SnapshotSource,
    /// `snapshot.json`'s `gen` — `None` for a fallback root, which has no index.
    pub gen: Option<String>,
    /// `snapshot.json`'s `contentHash`, when the index carries one.
    pub content_hash: Option<String>,
    skills: Vec<SkillEntry>,
}

impl SkillsSnapshot {
    /// The entry a `skill_ref` names: by frontmatter `name` first, else by the dir-derived name
    /// (`wicked-garden-` + the dir path joined by `-`), which garden's convention makes equal to
    /// the frontmatter name for every shipped skill — the fallback matters for a user-added skill
    /// whose frontmatter diverged from its directory.
    pub(crate) fn skill(&self, skill_ref: &str) -> Option<&SkillEntry> {
        self.skills
            .iter()
            .find(|s| s.name == skill_ref)
            .or_else(|| {
                self.skills
                    .iter()
                    .find(|s| derived_name(&s.dir) == skill_ref)
            })
    }

    /// The Claude-side identity of a `skill_ref`'s skill: the `<skill-dir>` half of
    /// `wicked-garden:<skill-dir>`. Claude discovers a plugin's skills one directory deep
    /// (`skills/*/SKILL.md`) and names each by that directory; a NESTED dir — which Claude does not
    /// discover — is spelled path-joined, which coincides with the frontmatter-name convention, so
    /// the directive reads the same with or without a snapshot to resolve against.
    pub(crate) fn claude_skill_dir(&self, skill_ref: &str) -> Option<String> {
        self.skill(skill_ref).map(|s| s.dir.replace('/', "-"))
    }

    /// The `skill_ref`s among `refs` that belong to this plugin's catalog but are NOT in the root
    /// — sorted and deduplicated, ready to be named in a refusal. A ref outside the catalog (another
    /// plugin family) is not this snapshot's to judge; see [`admit_refs`].
    pub(crate) fn missing<'a>(&self, refs: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut missing: Vec<String> = refs
            .into_iter()
            .filter(|r| in_catalog(r) && self.skill(r).is_none())
            .map(str::to_string)
            .collect();
        missing.sort();
        missing.dedup();
        missing
    }

    /// The generation this root represents, as the token crew reaps by: `gen=<gen>` for a
    /// published snapshot, `fallback=<source>` otherwise (nothing to reap — it is the installed
    /// plugin itself).
    pub(crate) fn gen_label(&self) -> String {
        match &self.gen {
            Some(gen) => format!("gen={gen}"),
            None => format!("fallback={}", self.source.token()),
        }
    }

    /// The `skills.snapshot` log line for one launch — `context` names the launch (run, unit,
    /// seat). Pure, so the shape crew greps for is pinned by a test.
    pub(crate) fn launch_line(&self, context: &str) -> String {
        format!(
            "[wicked-core] skills.snapshot {} root={} {context}",
            self.gen_label(),
            self.root.display()
        )
    }

    /// Report the generation handed to a launch. Every spawn path calls this exactly once per
    /// launch it hands the root to, so a generation is never in use without a line saying so.
    pub(crate) fn report(&self, context: &str) {
        eprintln!("{}", self.launch_line(context));
    }

    /// The [`CoreEvent::SkillsSnapshotHanded`] record for one launch — the machine-readable half
    /// of [`report`](Self::report), on the same event stream crew already consumes, so it can tell
    /// which generations live sessions still reference. `path` is the carrier (`"wrapped_cli"` /
    /// `"acp"`, the `GovernanceContextArmed` vocabulary) and `cli` the seat it was handed to.
    pub(crate) fn handed_event(
        &self,
        session: &str,
        ord: u32,
        attempt: u32,
        path: &str,
        cli: &str,
    ) -> CoreEvent {
        CoreEvent::SkillsSnapshotHanded {
            session: session.to_string(),
            ord,
            attempt,
            path: path.to_string(),
            cli: cli.to_string(),
            gen: self.gen.clone(),
            content_hash: self.content_hash.clone(),
            root: self.root.to_string_lossy().into_owned(),
            source: self.source.token().to_string(),
        }
    }

    #[cfg(test)]
    pub(crate) fn skills(&self) -> &[SkillEntry] {
        &self.skills
    }
}

/// Is `skill_ref` one this plugin's catalog can vouch for? Every garden skill's frontmatter name
/// carries the plugin prefix; a ref from another family (a retired plugin's, an operator's own)
/// is outside the snapshot's authority and is reported, not refused — see [`admit_refs`].
fn in_catalog(skill_ref: &str) -> bool {
    skill_ref
        .strip_prefix(PLUGIN_NAME)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|rest| !rest.is_empty())
}

/// `engineering/frontend` → `wicked-garden-engineering-frontend`.
fn derived_name(dir: &str) -> String {
    format!("{PLUGIN_NAME}-{}", dir.replace('/', "-"))
}

/// Why a launch could not be handed its skills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SkillsError {
    /// [`SKILLS_SNAPSHOT_ENV`] names a path that is not a usable snapshot. The operator (or crew)
    /// set it; falling back would hide the misconfiguration.
    Config { path: PathBuf, why: String },
    /// The run names skills the root does not hold (or there is no root at all).
    Missing {
        root: Option<PathBuf>,
        missing: Vec<String>,
    },
}

impl std::fmt::Display for SkillsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillsError::Config { path, why } => write!(
                f,
                "{SKILLS_SNAPSHOT_ENV}={} is not a usable skills snapshot ({why}); point it at a \
                 published snapshot generation, or unset it to fall back to the installed \
                 {PLUGIN_NAME}",
                path.display()
            ),
            SkillsError::Missing {
                root: Some(root),
                missing,
            } => write!(
                f,
                "the skills snapshot at {} does not hold the skills this run requires: {}; enable \
                 and republish them (or fix the workflow's skill_ref)",
                root.display(),
                missing.join(", ")
            ),
            SkillsError::Missing {
                root: None,
                missing,
            } => write!(
                f,
                "no {PLUGIN_NAME} skills root is available ({SKILLS_SNAPSHOT_ENV} unset and no \
                 installed plugin found under the claude config dir) but this run requires skills: \
                 {}; install {PLUGIN_NAME} or publish a snapshot",
                missing.join(", ")
            ),
        }
    }
}

impl std::error::Error for SkillsError {}

// ── Resolution ────────────────────────────────────────────────────────────────

/// Resolve the skills root from the process environment, logging the ladder step taken.
/// `Ok(None)` ⇒ no root anywhere (already logged). `Err` ⇒ the explicit path is misconfigured.
pub(crate) fn resolve() -> Result<Option<SkillsSnapshot>, SkillsError> {
    let explicit = std::env::var_os(SKILLS_SNAPSHOT_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    resolve_in(
        explicit,
        std::env::var_os(crate::acp_runner::CLAUDE_CONFIG_DIR_ENV).map(PathBuf::from),
        home_dir(),
        &mut |line| eprintln!("{line}"),
    )
}

/// [`resolve`] with its inputs and its log sink explicit, so the ladder is testable without
/// touching the process environment and the "logged" half of each step is asserted, not assumed.
pub(crate) fn resolve_in(
    explicit: Option<PathBuf>,
    claude_config_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    log: &mut dyn FnMut(String),
) -> Result<Option<SkillsSnapshot>, SkillsError> {
    if let Some(path) = explicit {
        return load_published(&path).map(Some);
    }
    let Some(config) = claude_config_dir.or_else(|| home.map(|h| h.join(".claude"))) else {
        log(format!(
            "[wicked-core] skills.fallback {SKILLS_SNAPSHOT_ENV} is unset and neither \
             CLAUDE_CONFIG_DIR nor a home directory resolves; workers run WITHOUT {PLUGIN_NAME} \
             skills"
        ));
        return Ok(None);
    };
    // The live marketplace cache and nothing else: `<config>/plugins/wicked-garden` (the operator's
    // hand copy) is NOT a candidate — it is the stale copy the snapshot mechanism exists to retire.
    let cache = config
        .join("plugins")
        .join("cache")
        .join(PLUGIN_NAME)
        .join(PLUGIN_NAME);
    let found = highest_version_dir(&cache)
        .filter(|latest| plugin_manifest_name(latest).as_deref() == Some(PLUGIN_NAME))
        .map(|latest| load_live(latest, SnapshotSource::LiveCache));
    match &found {
        Some(snapshot) => log(format!(
            "[wicked-core] skills.fallback {SKILLS_SNAPSHOT_ENV} is unset; using the {} at {} \
             ({} skills). Publish a snapshot to pin a generation — the installed plugin also \
             carries its interactive hooks, which a snapshot excludes",
            snapshot.source,
            snapshot.root.display(),
            snapshot.skills.len()
        )),
        None => log(format!(
            "[wicked-core] skills.fallback {SKILLS_SNAPSHOT_ENV} is unset and no installed \
             {PLUGIN_NAME} was found in the plugin cache under {}; workers run WITHOUT \
             {PLUGIN_NAME} skills, and a run that names one is refused",
            config.display()
        )),
    }
    Ok(found)
}

/// How many symlink hops [`pin_generation`] follows before calling the chain a loop.
const MAX_LINK_HOPS: usize = 8;

/// Pin `path` to the concrete directory it names: when `path` itself is a symlink (crew's
/// `current -> snapshots/<gen>`), follow it — relative targets resolve against the link's parent —
/// until a non-link is reached. Only the FINAL component is resolved (no `canonicalize`): the rest
/// of the path is left spelled as given, so a `/tmp` that is itself a link, or a Windows drive
/// path, is not rewritten into a form the worker's CLI never saw.
fn pin_generation(path: &Path) -> Result<PathBuf, String> {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_LINK_HOPS {
        let meta =
            std::fs::symlink_metadata(&current).map_err(|e| format!("cannot read it: {e}"))?;
        if !meta.file_type().is_symlink() {
            return Ok(current);
        }
        let target = std::fs::read_link(&current)
            .map_err(|e| format!("cannot read the link {}: {e}", current.display()))?;
        current = match current.parent() {
            Some(parent) if target.is_relative() => parent.join(target),
            _ => target,
        };
    }
    Err(format!(
        "symlink chain longer than {MAX_LINK_HOPS} hops — a loop, not a generation"
    ))
}

/// Load the snapshot [`SKILLS_SNAPSHOT_ENV`] names. Strict: every shortfall is a config error
/// naming the path and the reason — this path was chosen deliberately, so nothing here degrades.
fn load_published(named: &Path) -> Result<SkillsSnapshot, SkillsError> {
    let config_err = |why: String| SkillsError::Config {
        path: named.to_path_buf(),
        why,
    };
    let pinned = pin_generation(named).map_err(config_err)?;
    let path = pinned.as_path();
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(config_err("not a directory".to_string())),
        Err(e) => return Err(config_err(format!("cannot read it: {e}"))),
    }
    match plugin_manifest_name(path) {
        Some(name) if name == PLUGIN_NAME => {}
        Some(name) => {
            return Err(config_err(format!(
                "{PLUGIN_MANIFEST} names plugin `{name}`, expected `{PLUGIN_NAME}`"
            )))
        }
        None => {
            return Err(config_err(format!(
                "no parseable {PLUGIN_MANIFEST} with a `name` — not a plugin root"
            )))
        }
    }
    let index_path = path.join(SNAPSHOT_INDEX);
    let bytes = std::fs::read(&index_path)
        .map_err(|e| config_err(format!("cannot read {SNAPSHOT_INDEX}: {e}")))?;
    let index: Value = serde_json::from_slice(&bytes)
        .map_err(|e| config_err(format!("{SNAPSHOT_INDEX} is not valid JSON: {e}")))?;
    let gen = match index.get("gen") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => {
            return Err(config_err(format!(
                "{SNAPSHOT_INDEX} has no `gen` (string or number)"
            )))
        }
    };
    let content_hash = index
        .get("contentHash")
        .and_then(Value::as_str)
        .map(str::to_string);
    let entries = index
        .get("skills")
        .and_then(Value::as_array)
        .ok_or_else(|| config_err(format!("{SNAPSHOT_INDEX} has no `skills` array")))?;
    let mut skills = Vec::with_capacity(entries.len());
    for (i, entry) in entries.iter().enumerate() {
        let field = |key: &str| {
            entry
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| {
                    config_err(format!(
                        "{SNAPSHOT_INDEX} skills[{i}] has no string `{key}`"
                    ))
                })
        };
        skills.push(SkillEntry {
            name: field("name")?,
            dir: field("dir")?,
        });
    }
    Ok(SkillsSnapshot {
        root: path.to_path_buf(),
        source: SnapshotSource::Published,
        gen: Some(gen),
        content_hash,
        skills,
    })
}

/// Index an INSTALLED plugin root (no `snapshot.json`): every directory under `skills/` holding a
/// `SKILL.md`, nested ones included, keyed by frontmatter `name`. Symlinks are never followed —
/// the walk stays inside the root it was given.
fn load_live(root: PathBuf, source: SnapshotSource) -> SkillsSnapshot {
    let mut skills = Vec::new();
    walk_skills(&root.join(SKILLS_DIR), &[], &mut skills);
    skills.sort_by(|a, b| a.dir.cmp(&b.dir));
    SkillsSnapshot {
        root,
        source,
        gen: None,
        content_hash: None,
        skills,
    }
}

fn walk_skills(dir: &Path, rel: &[String], out: &mut Vec<SkillEntry>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let mut child_rel = rel.to_vec();
        child_rel.push(entry.file_name().to_string_lossy().into_owned());
        let skill_md = path.join(SKILL_FILE);
        if std::fs::symlink_metadata(&skill_md).is_ok_and(|m| m.is_file()) {
            let dir = child_rel.join("/");
            out.push(SkillEntry {
                name: frontmatter_name(&skill_md).unwrap_or_else(|| derived_name(&dir)),
                dir,
            });
        }
        walk_skills(&path, &child_rel, out);
    }
}

/// `name:` from a `---`-fenced YAML frontmatter block, unquoted. Line-based on purpose: the field
/// is a scalar on its own line in every one of garden's skills, and a YAML dependency for one key
/// is a dependency too many.
fn frontmatter_name(skill_md: &Path) -> Option<String> {
    let text = std::fs::read_to_string(skill_md).ok()?;
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    for line in lines {
        if line.trim_end() == "---" {
            break;
        }
        if let Some(value) = line.strip_prefix("name:") {
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// The `name` in `<root>/.claude-plugin/plugin.json`, or `None` when the file is absent, is not a
/// regular file, or does not parse to an object with a string `name`.
fn plugin_manifest_name(root: &Path) -> Option<String> {
    let path = root.join(PLUGIN_MANIFEST);
    if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
        return None;
    }
    let manifest: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    manifest.get("name")?.as_str().map(str::to_string)
}

/// The highest `X.Y.Z`-named subdirectory of a marketplace cache dir, compared NUMERICALLY
/// (`12.32.0` beats `12.9.0`, which a lexical sort gets wrong). Non-numeric names and symlinked
/// entries are ignored.
fn highest_version_dir(cache: &Path) -> Option<PathBuf> {
    std::fs::read_dir(cache)
        .ok()?
        .flatten()
        .filter_map(|e| {
            if !std::fs::symlink_metadata(e.path()).ok()?.is_dir() {
                return None;
            }
            let name = e.file_name().to_str()?.to_string();
            let parsed: Option<Vec<u64>> = name.split('.').map(|s| s.parse().ok()).collect();
            parsed.map(|v| (v, e.path()))
        })
        .max()
        .map(|(_, path)| path)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

// ── Admission ─────────────────────────────────────────────────────────────────

/// Admit a launch that requires `refs`: every catalog ref must resolve in `snapshot`, or the launch
/// is refused naming the missing ones. A ref OUTSIDE the catalog (another plugin family — e.g. the
/// retired `wicked-testing-*` refs some engine-internal prompts still carry) cannot be vouched for
/// by a garden snapshot either way; it is reported through `log`, never silently passed and never
/// refused on this snapshot's authority.
pub(crate) fn admit_refs<'a>(
    snapshot: Option<SkillsSnapshot>,
    refs: impl IntoIterator<Item = &'a str>,
    log: &mut dyn FnMut(String),
) -> Result<Option<SkillsSnapshot>, SkillsError> {
    let mut catalog: Vec<&str> = Vec::new();
    for r in refs {
        if in_catalog(r) {
            catalog.push(r);
        } else {
            log(format!(
                "[wicked-core] skills.notice skill_ref `{r}` is outside the {PLUGIN_NAME} catalog; \
                 the skills snapshot cannot vouch for it"
            ));
        }
    }
    let missing = match &snapshot {
        Some(s) => s.missing(catalog),
        None => {
            let mut all: Vec<String> = catalog.into_iter().map(str::to_string).collect();
            all.sort();
            all.dedup();
            all
        }
    };
    if !missing.is_empty() {
        return Err(SkillsError::Missing {
            root: snapshot.map(|s| s.root),
            missing,
        });
    }
    Ok(snapshot)
}

/// The launch admission for one unit, on either spawn path: resolve the root (the ladder), then
/// require every skill the run names — [`StepInput::required_skills`] (the actor's run-wide set)
/// plus the unit's own `skill_ref` (so a directly-constructed input is still admitted on its own
/// terms). `Ok(None)` ⇒ the unit needs no skill and there is no root to hand it.
///
/// Under the operator's inherit-config escape hatch
/// (`execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV`) the worker runs with the operator's OWN
/// plugins, so no snapshot is handed to it and none is required of it — said out loud, since a
/// set-but-ignored `WICKED_SKILLS_SNAPSHOT` would otherwise read as a silent no-op.
pub(crate) fn admit_unit(input: &StepInput) -> Result<Option<SkillsSnapshot>, SkillsError> {
    if crate::execute_wrapped::inherits_operator_config() {
        eprintln!(
            "[wicked-core] skills.snapshot bypassed for {}:{}: {} is set, so the worker runs under \
             the operator's own configuration and plugins",
            input.run_id,
            input.unit.ord,
            crate::execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV
        );
        return Ok(None);
    }
    let snapshot = resolve()?;
    let refs = input
        .required_skills
        .iter()
        .map(String::as_str)
        .chain(input.unit.skill_ref.as_deref())
        .filter(|r| !r.is_empty());
    admit_refs(snapshot, refs, &mut |line| eprintln!("{line}"))
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Fixture roots for this module's tests and the two spawn paths'.

    use std::path::{Path, PathBuf};

    /// A process-scoped scratch base, pre-cleaned. Keyed by name + pid + counter so parallel
    /// tests (and a stranded dir from a killed run) never share one.
    pub(crate) fn scratch(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "wskills-{name}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write `skills/<dir>/SKILL.md` with a frontmatter `name` under `root`.
    pub(crate) fn write_skill(root: &Path, dir: &str, name: &str) {
        let skill = root.join(super::SKILLS_DIR).join(dir);
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join(super::SKILL_FILE),
            format!("---\nname: {name}\ndescription: fixture\n---\n\n# {name}\n"),
        )
        .unwrap();
    }

    /// An INSTALLED-plugin-shaped root at `root` (plugin.json + skills, no index) — the fallback
    /// ladder's candidates.
    pub(crate) fn live_root(root: &Path, version: &str, skills: &[(&str, &str)]) -> PathBuf {
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::write(
            root.join(super::PLUGIN_MANIFEST),
            format!("{{\"name\":\"wicked-garden\",\"version\":\"{version}\"}}"),
        )
        .unwrap();
        for (dir, name) in skills {
            write_skill(root, dir, name);
        }
        root.to_path_buf()
    }

    /// A PUBLISHED snapshot at `root`: plugin.json + skills + `snapshot.json` indexing them under
    /// `gen`. `skills` are `(dir, frontmatter name)`.
    pub(crate) fn snapshot_root(root: &Path, gen: &str, skills: &[(&str, &str)]) -> PathBuf {
        live_root(root, "0.0.0", skills);
        let entries: Vec<serde_json::Value> = skills
            .iter()
            .map(|(dir, name)| {
                serde_json::json!({
                    "name": name, "dir": dir, "kind": "fork-worker", "core": false, "portable": true
                })
            })
            .collect();
        std::fs::write(
            root.join(super::SNAPSHOT_INDEX),
            serde_json::to_vec(&serde_json::json!({
                "gen": gen,
                "contentHash": format!("sha256:{gen}"),
                "skills": entries
            }))
            .unwrap(),
        )
        .unwrap();
        root.to_path_buf()
    }

    /// The loaded snapshot for a fixture written by [`snapshot_root`].
    pub(crate) fn load(root: &Path) -> super::SkillsSnapshot {
        super::resolve_in(Some(root.to_path_buf()), None, None, &mut |_| {})
            .expect("the fixture is a snapshot")
            .expect("an explicit path always yields a snapshot")
    }

    /// A recursive `(relative path, bytes)` listing of `root`, for asserting a snapshot dir was not
    /// written into: compare before and after.
    pub(crate) fn tree_fingerprint(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        fn walk(dir: &Path, root: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                if std::fs::symlink_metadata(&path).unwrap().is_dir() {
                    out.insert(format!("{rel}/"), Vec::new());
                    walk(&path, root, out);
                } else {
                    out.insert(rel, std::fs::read(&path).unwrap());
                }
            }
        }
        let mut out = std::collections::BTreeMap::new();
        walk(root, root, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn collect(lines: &mut Vec<String>) -> impl FnMut(String) + '_ {
        move |l| lines.push(l)
    }

    /// The explicit path is loaded from its index: gen, hash, and the skills keyed by name; a ref
    /// resolves by frontmatter name first and by the dir-derived name second; the Claude identity
    /// is the top-level dir, and a nested dir is spelled path-joined.
    #[test]
    fn an_explicit_snapshot_is_indexed_and_refs_resolve_by_name_then_by_dir() {
        let base = scratch("published");
        let root = snapshot_root(
            &base.join("snapshots/7"),
            "7",
            &[
                ("domain", "wicked-garden-domain"),
                ("engineering/frontend", "wicked-garden-engineering-frontend"),
                ("qe-oracle", "wicked-garden-test-oracle"),
            ],
        );
        let mut lines = Vec::new();
        let s = resolve_in(Some(root.clone()), None, None, &mut collect(&mut lines))
            .unwrap()
            .unwrap();
        assert!(
            lines.is_empty(),
            "an explicit path logs no fallback: {lines:?}"
        );
        assert_eq!(s.source, SnapshotSource::Published);
        assert_eq!(s.gen.as_deref(), Some("7"));
        assert_eq!(s.content_hash.as_deref(), Some("sha256:7"));
        assert_eq!(s.root, root);
        assert_eq!(s.skills().len(), 3);
        assert_eq!(s.skill("wicked-garden-domain").unwrap().dir, "domain");
        assert_eq!(
            s.skill("wicked-garden-test-oracle").unwrap().dir,
            "qe-oracle",
            "by frontmatter name"
        );
        assert_eq!(
            s.skill("wicked-garden-qe-oracle").unwrap().name,
            "wicked-garden-test-oracle",
            "by the dir-derived name when the frontmatter diverged"
        );
        assert_eq!(
            s.claude_skill_dir("wicked-garden-engineering-frontend")
                .as_deref(),
            Some("engineering-frontend")
        );
        assert!(s.skill("wicked-garden-jam").is_none());
        assert_eq!(s.gen_label(), "gen=7");
        assert!(s
            .launch_line("run=r1 unit=2")
            .starts_with("[wicked-core] skills.snapshot gen=7 root="));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An explicit path that is missing, not a plugin root, another plugin, or has no parseable
    /// index is a CONFIG ERROR naming the path and the reason — never a fallback.
    #[test]
    fn an_explicit_path_that_is_not_a_snapshot_is_a_config_error_naming_why() {
        let base = scratch("config-err");
        let expect_err = |path: &Path, needle: &str| {
            let mut lines = Vec::new();
            let err = resolve_in(
                Some(path.to_path_buf()),
                None,
                None,
                &mut collect(&mut lines),
            )
            .expect_err("a config error");
            assert!(
                lines.is_empty(),
                "no fallback log on a config error: {lines:?}"
            );
            let SkillsError::Config { path: p, why } = &err else {
                panic!("expected Config, got {err:?}");
            };
            assert_eq!(p, path);
            assert!(
                why.contains(needle),
                "why=`{why}` should mention `{needle}`"
            );
            let msg = err.to_string();
            assert!(
                msg.contains(SKILLS_SNAPSHOT_ENV) && msg.contains(&path.display().to_string()),
                "the operator-facing message names the variable and the path: {msg}"
            );
        };
        expect_err(&base.join("does-not-exist"), "cannot read");
        let file = base.join("a-file");
        std::fs::write(&file, "x").unwrap();
        expect_err(&file, "not a directory");
        let bare = base.join("bare");
        std::fs::create_dir_all(bare.join("skills")).unwrap();
        expect_err(&bare, PLUGIN_MANIFEST);
        let other = live_root(&base.join("other"), "1.0.0", &[]);
        std::fs::write(
            other.join(PLUGIN_MANIFEST),
            "{\"name\":\"some-other-plugin\",\"version\":\"1.0.0\"}",
        )
        .unwrap();
        expect_err(&other, "some-other-plugin");
        let no_index = live_root(
            &base.join("no-index"),
            "1.0.0",
            &[("core", "wicked-garden-core")],
        );
        expect_err(&no_index, SNAPSHOT_INDEX);
        let bad = snapshot_root(&base.join("bad"), "3", &[("core", "wicked-garden-core")]);
        std::fs::write(
            bad.join(SNAPSHOT_INDEX),
            "{\"gen\":\"3\",\"skills\":[{\"name\":\"x\"}]}",
        )
        .unwrap();
        expect_err(&bad, "skills[0] has no string `dir`");
        std::fs::write(bad.join(SNAPSHOT_INDEX), "{\"skills\":[]}").unwrap();
        expect_err(&bad, "`gen`");
        std::fs::write(bad.join(SNAPSHOT_INDEX), "not json").unwrap();
        expect_err(&bad, "not valid JSON");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The ladder when the env is UNSET: the live cache's HIGHEST version (numerically) wins and
    /// is LOGGED as `skills.fallback`; no cache ⇒ no root, also logged — the operator's hand copy
    /// at `<config>/plugins/wicked-garden` is NEVER a fallback (it is the stale copy the snapshot
    /// retires). A numeric `gen` in the index is accepted too.
    #[test]
    fn unset_falls_back_to_the_live_cache_only_never_the_hand_copy_and_logs_each_step() {
        let base = scratch("ladder");
        let config = base.join("claude-config");
        let cache = config.join("plugins/cache/wicked-garden/wicked-garden");
        for ver in ["12.9.0", "12.32.0", "not-a-version"] {
            live_root(
                &cache.join(ver),
                ver,
                &[
                    ("core", "wicked-garden-core"),
                    ("mem/capture", "wicked-garden-mem-capture"),
                ],
            );
        }
        // The stale hand copy sits beside the cache the whole time and must never be chosen.
        live_root(
            &config.join("plugins/wicked-garden"),
            "12.28.1",
            &[("core", "wicked-garden-core")],
        );
        let mut lines = Vec::new();
        let picked = resolve_in(None, Some(config.clone()), None, &mut collect(&mut lines))
            .unwrap()
            .expect("the live cache is a root");
        assert_eq!(picked.source, SnapshotSource::LiveCache);
        assert_eq!(picked.root, cache.join("12.32.0"), "numeric, not lexical");
        assert_eq!(picked.gen, None);
        assert_eq!(picked.gen_label(), "fallback=live-cache");
        assert_eq!(
            picked
                .skills()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["wicked-garden-core", "wicked-garden-mem-capture"],
            "nested skills are indexed by their frontmatter name"
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("skills.fallback")
                && lines[0].contains(&cache.join("12.32.0").display().to_string()),
            "the fallback is logged with the root it chose: {}",
            lines[0]
        );

        // Cache gone, hand copy still there ⇒ NO root: the hand copy is not on the ladder.
        std::fs::remove_dir_all(config.join("plugins/cache")).unwrap();
        let mut lines = Vec::new();
        let none = resolve_in(None, Some(config.clone()), None, &mut collect(&mut lines)).unwrap();
        assert!(
            none.is_none(),
            "the hand copy at <config>/plugins/wicked-garden must never be a fallback: {none:?}"
        );
        assert!(
            lines[0].contains("skills.fallback") && lines[0].contains("WITHOUT"),
            "{lines:?}"
        );

        // No config dir given ⇒ `<home>/.claude` is the config dir.
        let home = base.join("home");
        let home_cache = home.join(".claude/plugins/cache/wicked-garden/wicked-garden/1.0.0");
        live_root(&home_cache, "1.0.0", &[("core", "wicked-garden-core")]);
        let picked = resolve_in(None, None, Some(home.clone()), &mut |_| {})
            .unwrap()
            .unwrap();
        assert_eq!(picked.root, home_cache);

        // Numeric `gen` in a published index.
        let numeric = snapshot_root(&base.join("num"), "9", &[]);
        std::fs::write(numeric.join(SNAPSHOT_INDEX), "{\"gen\":12,\"skills\":[]}").unwrap();
        let s = resolve_in(Some(numeric), None, None, &mut |_| {})
            .unwrap()
            .unwrap();
        assert_eq!(s.gen.as_deref(), Some("12"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An explicit path that is a symlink (crew's `current -> snapshots/<gen>`) is PINNED to the
    /// generation it points at when loaded: the snapshot's root is the concrete directory, so a
    /// session keeps the generation it was handed while a later resolve — after crew flips the
    /// link — gets the next one. Two sessions on two generations therefore never share a root.
    /// A link loop is a config error, not a hang.
    #[cfg(unix)]
    #[test]
    fn a_current_link_is_pinned_to_its_concrete_generation_at_load() {
        let base = scratch("pin");
        let gen7 = snapshot_root(
            &base.join("snapshots/7"),
            "7",
            &[("domain", "wicked-garden-domain")],
        );
        let gen8 = snapshot_root(
            &base.join("snapshots/8"),
            "8",
            &[("domain", "wicked-garden-domain")],
        );
        let current = base.join("current");
        std::os::unix::fs::symlink("snapshots/7", &current).unwrap();

        let first = resolve_in(Some(current.clone()), None, None, &mut |_| {})
            .unwrap()
            .unwrap();
        assert_eq!(
            first.root, gen7,
            "the link is resolved to the generation it names"
        );
        assert_eq!(first.gen.as_deref(), Some("7"));

        // crew publishes gen 8 and flips `current` — the session already handed gen 7 is unaffected.
        std::fs::remove_file(&current).unwrap();
        std::os::unix::fs::symlink(base.join("snapshots/8"), &current).unwrap();
        let second = resolve_in(Some(current.clone()), None, None, &mut |_| {})
            .unwrap()
            .unwrap();
        assert_eq!(second.root, gen8, "an absolute link target resolves too");
        assert_eq!(second.gen.as_deref(), Some("8"));
        assert_eq!(
            first.root, gen7,
            "the earlier snapshot still names its own generation"
        );
        assert_ne!(first.root, second.root);

        // A loop names itself as the reason instead of following forever.
        let a = base.join("loop-a");
        let b = base.join("loop-b");
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        let err = resolve_in(Some(a.clone()), None, None, &mut |_| {}).expect_err("a loop");
        let SkillsError::Config { path, why } = err else {
            panic!("expected Config");
        };
        assert_eq!(
            path, a,
            "the error names the path the operator set, not a hop"
        );
        assert!(why.contains("loop"), "{why}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The machine-readable half of the launch report carries exactly what crew reaps by — the
    /// generation and content hash — plus the launch it was handed to.
    #[test]
    fn the_handed_event_names_the_generation_the_carrier_and_the_seat() {
        let base = scratch("event");
        let root = snapshot_root(&base.join("snap"), "21", &[]);
        let s = load(&root);
        let ev = s.handed_event("run-1", 3, 1, "acp", "claude");
        assert_eq!(
            ev,
            CoreEvent::SkillsSnapshotHanded {
                session: "run-1".to_string(),
                ord: 3,
                attempt: 1,
                path: "acp".to_string(),
                cli: "claude".to_string(),
                gen: Some("21".to_string()),
                content_hash: Some("sha256:21".to_string()),
                root: root.to_string_lossy().into_owned(),
                source: "published".to_string(),
            }
        );
        let json = ev.to_json();
        assert_eq!(json["type"], "skillsSnapshotHanded");
        assert_eq!(json["gen"], "21");
        assert_eq!(json["contentHash"], "sha256:21");
        // A fallback root has no generation to reap.
        let live = load_live(
            live_root(&base.join("live"), "1.0.0", &[]),
            SnapshotSource::LiveCache,
        );
        let json = live
            .handed_event("run-1", 3, 1, "wrapped_cli", "claude")
            .to_json();
        assert!(json["gen"].is_null() && json["contentHash"].is_null());
        assert_eq!(json["source"], "live-cache");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A run naming skills the snapshot does not hold is REFUSED with exactly those names (sorted,
    /// deduplicated); a ref outside the catalog is reported, not refused; with no root at all every
    /// catalog ref is missing while a skill-less run is admitted.
    #[test]
    fn missing_required_skills_are_refused_by_name_and_foreign_refs_are_reported() {
        let base = scratch("admit");
        let root = snapshot_root(
            &base.join("snap"),
            "4",
            &[("domain", "wicked-garden-domain")],
        );
        let snapshot = load(&root);

        let mut lines = Vec::new();
        let ok = admit_refs(
            Some(snapshot.clone()),
            [
                "wicked-garden-domain",
                "wicked-testing-acceptance-test-writer",
            ],
            &mut collect(&mut lines),
        )
        .expect("the catalog ref is present");
        assert_eq!(ok.as_ref().map(|s| &s.root), Some(&root));
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("skills.notice")
                && lines[0].contains("wicked-testing-acceptance-test-writer"),
            "{lines:?}"
        );

        let err = admit_refs(
            Some(snapshot.clone()),
            [
                "wicked-garden-domain-coverage",
                "wicked-garden-domain",
                "wicked-garden-domain-extractor",
                "wicked-garden-domain-coverage",
            ],
            &mut |_| {},
        )
        .expect_err("two skills are missing");
        assert_eq!(
            err,
            SkillsError::Missing {
                root: Some(root.clone()),
                missing: vec![
                    "wicked-garden-domain-coverage".to_string(),
                    "wicked-garden-domain-extractor".to_string(),
                ],
            }
        );
        let msg = err.to_string();
        assert!(
            msg.contains("wicked-garden-domain-coverage")
                && msg.contains("wicked-garden-domain-extractor")
                && msg.contains(&root.display().to_string()),
            "{msg}"
        );

        let err = admit_refs(None, ["wicked-garden-domain"], &mut |_| {}).expect_err("no root");
        assert_eq!(
            err,
            SkillsError::Missing {
                root: None,
                missing: vec!["wicked-garden-domain".to_string()],
            }
        );
        assert!(err.to_string().contains(SKILLS_SNAPSHOT_ENV));
        assert!(
            admit_refs(None, [], &mut |_| {}).unwrap().is_none(),
            "a run that names no skill proceeds without a root"
        );
        assert!(
            admit_refs(None, ["wicked-garden-", "", "wicked-garden"], &mut |_| {})
                .unwrap()
                .is_none(),
            "a bare prefix is not a catalog skill"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The live walk indexes nested skills, derives a name when the frontmatter has none, and
    /// never follows a symlink out of the root.
    #[cfg(unix)]
    #[test]
    fn the_live_walk_indexes_nested_skills_and_does_not_follow_links() {
        let base = scratch("walk");
        let root = live_root(
            &base.join("plugin"),
            "1.0.0",
            &[
                ("qe", "wicked-garden-qe"),
                ("qe/a11y", "wicked-garden-qe-a11y"),
            ],
        );
        std::fs::create_dir_all(root.join("skills/bare")).unwrap();
        std::fs::write(root.join("skills/bare/SKILL.md"), "# no frontmatter\n").unwrap();
        let outside = base.join("outside");
        write_skill(&outside, "leak", "wicked-garden-leak");
        std::os::unix::fs::symlink(outside.join("skills"), root.join("skills/linked")).unwrap();

        let s = load_live(root, SnapshotSource::LiveCache);
        let names: Vec<&str> = s.skills().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "wicked-garden-bare",
                "wicked-garden-qe",
                "wicked-garden-qe-a11y"
            ],
            "sorted by dir; the linked tree is not walked"
        );
        assert_eq!(s.skill("wicked-garden-qe-a11y").unwrap().dir, "qe/a11y");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn frontmatter_name_reads_the_scalar_and_the_derived_name_joins_the_path() {
        let base = scratch("fm");
        let quoted = base.join("SKILL.md");
        std::fs::write(
            &quoted,
            "---\ndescription: |\n  multi\n  line\nname: \"wicked-garden-x\"\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(
            frontmatter_name(&quoted).as_deref(),
            Some("wicked-garden-x")
        );
        let none = base.join("NOFM.md");
        std::fs::write(&none, "# no frontmatter\nname: not-in-frontmatter\n").unwrap();
        assert_eq!(frontmatter_name(&none), None);
        assert_eq!(derived_name("qe/a11y"), "wicked-garden-qe-a11y");
        assert!(in_catalog("wicked-garden-qe") && !in_catalog("wicked-testing-qe"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
