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
//!   [{type: "local", path: <snapshot>}]` (`acp_runner::start_acp_process_with_write_roots`), and
//!   the snapshot is BOUND to the cached session: every later turn of that session uses the
//!   generation it was opened with, whatever `current` points at by then;
//! - Claude wrapped: `--plugin-dir <snapshot>` (`execute_wrapped::inject_isolation_flags`), with
//!   the template's own `--plugin-dir` stripped as a retired input;
//! - both governance carriers read-widen to the snapshot (`execute_wrapped::assemble_read_roots`)
//!   and the Read deny fence is CARVED around it (`execute_wrapped::deny_rules_for`); writes under
//!   it stay denied — a snapshot is immutable by contract, and this module never writes into one;
//! - the skill directive is CLI-aware (`execute_wrapped::plugin_skill_invocation`): the plugin
//!   form for Claude, the mirrored directory name for every other CLI.
//!
//! # Where the root comes from — the degradation ladder (v3 §3), no unconditional fail-open
//!
//! 1. [`SKILLS_SNAPSHOT_ENV`] set ⇒ that path, strictly ([`load_published`]): it must be absolute,
//!    no ancestor may be a symlink (a link above a pinned generation could re-aim it later), it
//!    is pinned to its canonical real path (a FINAL-component link such as crew's `current` is
//!    followed once, at load), and its index must describe files that actually exist. Any
//!    shortfall is a config error — a deliberately chosen snapshot is never silently swapped.
//!    Set but EMPTY is invalid explicit configuration, not "unset".
//! 2. Unset, [`SKILLS_CURRENT_ENV`] set ⇒ crew's `current` pointer. When it resolves it is loaded
//!    with the same strictness; when it points at nothing yet (no snapshot published so far) the
//!    ladder logs `skills.fallback` and continues. Set but empty ⇒ config error.
//! 3. Neither ⇒ the LIVE installed garden: the marketplace cache's highest version under the
//!    daemon's `CLAUDE_CONFIG_DIR` (else `~/.claude`), logged as `skills.fallback`. NOT the hand
//!    copy at `<config>/plugins/wicked-garden` — that stale copy is the defect being fixed, never
//!    a fallback. Nothing in the cache ⇒ no root at all (a run that needs a skill is then refused;
//!    a run that needs none proceeds).
//!
//! # Admission — before any process starts
//!
//! Every `skill_ref` the run names ([`StepInput::required_skills`] plus the unit's own), expanded
//! through each skill's transitive `mandates`, must be in the root ([`admit_refs`]). A missing
//! skill of ANY family is a refusal naming it ([`SkillsError::Missing`]) — the snapshot is the
//! worker's only skills source, so nothing outside it is exempt. Admission also knows WHICH CLI
//! will run: a nested skill has no Claude identity (Claude Code discovers a plugin's skills one
//! directory deep — verified against the live roster: 92/92 top-level garden dirs listed, 0/50
//! nested) and is refused for a Claude seat ([`SkillsError::NotInvocable`]); a `portable: false`
//! skill is Claude-only and is refused for every other CLI ([`SkillsError::NotPortable`]). Never a
//! silent proceed without the required method.
//!
//! The generation in use is reported at every launch — the `skills.snapshot gen=…` log line
//! ([`SkillsSnapshot::report`]) and the [`CoreEvent::SkillsSnapshotHanded`] event
//! ([`SkillsSnapshot::handed_event`]) — so crew can reap old generations only once no live session
//! references them.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::event::CoreEvent;
use crate::workflow::StepInput;

/// The one input crew passes: the ABSOLUTE path of the published snapshot generation to use.
pub(crate) const SKILLS_SNAPSHOT_ENV: &str = "WICKED_SKILLS_SNAPSHOT";

/// Crew's `current` pointer (`<stateHome>/skills/current -> snapshots/<gen>`): the ladder's second
/// rung, consulted only when [`SKILLS_SNAPSHOT_ENV`] is unset. A pointer at nothing means no
/// generation has been published yet and the ladder continues; a pointer at something is loaded
/// as strictly as an explicit snapshot.
pub(crate) const SKILLS_CURRENT_ENV: &str = "WICKED_SKILLS_CURRENT";

/// The plugin's name — the `<plugin>` half of the `wicked-garden:<dir>` identity Claude uses, the
/// marketplace + cache directory name, and the prefix every shipped skill's frontmatter `name`
/// carries.
pub(crate) const PLUGIN_NAME: &str = "wicked-garden";

/// The harness's plugin manifest, relative to a plugin root. Its presence is what makes a
/// directory a plugin root at all.
const PLUGIN_MANIFEST: &str = ".claude-plugin/plugin.json";

/// Crew's index of a published snapshot, relative to its root:
/// `{gen, contentHash, skills: [{name, dir, kind, core, portable, mandates?}]}`.
const SNAPSHOT_INDEX: &str = "snapshot.json";

/// The skills subtree of a plugin root, and the file that marks a directory as a skill.
const SKILLS_DIR: &str = "skills";
const SKILL_FILE: &str = "SKILL.md";

/// Where a skills root came from — reported with every launch so an operator can tell a published
/// generation from the installed-plugin fallback at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotSource {
    /// The snapshot [`SKILLS_SNAPSHOT_ENV`] (or [`SKILLS_CURRENT_ENV`]) named — crew-published,
    /// immutable.
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
    /// Usable outside Claude (v3 §5): `false` when the skill leans on `${CLAUDE_PLUGIN_ROOT}`,
    /// cwd-relative scripts or `../` sibling links, which no mirror carries. From the index for a
    /// published snapshot; approximated from the `SKILL.md` text for the live fallback.
    pub portable: bool,
    /// The skills this one declares it needs (`mandates:` in its frontmatter, plus any the index
    /// records) — expanded transitively at admission, so a run that names `repo-learn` is also
    /// refused when `search` is missing.
    pub mandates: Vec<String>,
}

impl SkillEntry {
    /// A nested skill lives more than one directory below `skills/`. Claude Code discovers a
    /// plugin's skills ONE directory deep and names each by that directory, so a nested skill has
    /// no Claude identity — see [`SkillsSnapshot::claude_skill_dir`].
    pub(crate) fn is_nested(&self) -> bool {
        self.dir.contains('/')
    }
}

/// The CLI a unit will run on, as admission needs to know it: Claude reaches skills through the
/// plugin loader (top-level dirs only, portable or not); every other CLI reaches them through an
/// additive mirror of the PORTABLE skills by frontmatter name (v3 §2/§5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerCli {
    Claude,
    Other(String),
}

impl WorkerCli {
    /// From the fact the runners already establish — `execute_wrapped::binary_is_claude` on the
    /// template's binary (wrapped) or the seat record's binary (ACP) — plus the seat key for the
    /// refusal message.
    pub(crate) fn for_seat(is_claude: bool, cli_key: &str) -> Self {
        if is_claude {
            WorkerCli::Claude
        } else {
            WorkerCli::Other(cli_key.to_string())
        }
    }

    pub(crate) fn is_claude(&self) -> bool {
        matches!(self, WorkerCli::Claude)
    }
}

impl std::fmt::Display for WorkerCli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerCli::Claude => f.write_str("claude"),
            WorkerCli::Other(key) => f.write_str(key),
        }
    }
}

/// A resolved skills root: WHERE it is, where it came from, and WHAT it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillsSnapshot {
    /// The plugin root a worker is pointed at — the CANONICAL real path (absolute, every symlink
    /// resolved), so the worker and the engine name the same directory and a link flipped later
    /// cannot re-aim a pinned generation.
    pub root: PathBuf,
    pub source: SnapshotSource,
    /// `snapshot.json`'s `gen` — `None` for a fallback root, which has no index.
    pub gen: Option<String>,
    /// `snapshot.json`'s `contentHash`, when the index carries one.
    pub content_hash: Option<String>,
    skills: Vec<SkillEntry>,
}

/// The transitive requirement of a set of refs against one root: what resolved (with everything
/// it mandates, recursively) and what did not.
pub(crate) struct Closure<'a> {
    pub required: Vec<&'a SkillEntry>,
    /// Sorted, deduplicated — ready to be named in a refusal.
    pub missing: Vec<String>,
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

    /// The Claude-side identity of a `skill_ref`'s skill — the `<skill-dir>` half of
    /// `wicked-garden:<skill-dir>` — or `None` when Claude exposes no such skill.
    ///
    /// Claude Code names a plugin's skills by DIRECTORY, one level deep (`skills/*/SKILL.md`);
    /// the frontmatter `name` is not the id (probed on this host's roster: `hookify/writing-rules`
    /// with `name: writing-hookify-rules` is listed as `hookify:writing-rules`, and none of
    /// garden's 50 nested `SKILL.md`s is listed at all). So a nested skill has NO Claude identity,
    /// and nothing here invents one — admission refuses it for a Claude seat instead
    /// ([`SkillsError::NotInvocable`]).
    pub(crate) fn claude_skill_dir(&self, skill_ref: &str) -> Option<String> {
        self.skill(skill_ref)
            .filter(|s| !s.is_nested())
            .map(|s| s.dir.clone())
    }

    /// Expand `refs` to their transitive closure over `mandates`, resolving each name in this
    /// root. Unknown refs — of any family — are `missing`; an unknown mandate of a resolved skill
    /// is missing too, named as itself.
    pub(crate) fn closure<'a>(&self, refs: impl IntoIterator<Item = &'a str>) -> Closure<'_> {
        let mut queue: Vec<String> = refs
            .into_iter()
            .filter(|r| !r.is_empty())
            .map(str::to_string)
            .collect();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut required: Vec<&SkillEntry> = Vec::new();
        let mut missing: BTreeSet<String> = BTreeSet::new();
        while let Some(r) = queue.pop() {
            if !seen.insert(r.clone()) {
                continue;
            }
            match self.skill(&r) {
                Some(entry) => {
                    if !required.iter().any(|e| e.dir == entry.dir) {
                        required.push(entry);
                        queue.extend(entry.mandates.iter().cloned());
                    }
                }
                None => {
                    missing.insert(r);
                }
            }
        }
        required.sort_by(|a, b| a.dir.cmp(&b.dir));
        Closure {
            required,
            missing: missing.into_iter().collect(),
        }
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

    /// Report the generation handed to a launch. Every spawn path calls this once per launch it
    /// hands the root to (and once per reused ACP turn, marked `reused`), so a generation is
    /// never in use without a line saying so.
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

/// Does `skill_ref` carry this plugin's name prefix? Only used to WORD a refusal: a ref from
/// another family (a retired plugin's, an operator's own) is judged exactly like a garden one —
/// present in the snapshot or refused — but the operator is told why the snapshot could not hold
/// it by default.
fn is_garden_name(skill_ref: &str) -> bool {
    skill_ref
        .strip_prefix(PLUGIN_NAME)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|rest| !rest.is_empty())
}

/// `engineering/frontend` → `wicked-garden-engineering-frontend`.
fn derived_name(dir: &str) -> String {
    format!("{PLUGIN_NAME}-{}", dir.replace('/', "-"))
}

/// Why a launch could not be handed its skills. Every variant is a REFUSAL the runner turns into
/// a failed unit before any process starts (`execute_wrapped::skills_refusal`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SkillsError {
    /// `var` names a path that is not a usable snapshot. The operator (or crew) set it; falling
    /// back would hide the misconfiguration.
    Config {
        var: &'static str,
        path: PathBuf,
        why: String,
    },
    /// The run names skills the root does not hold (or there is no root at all).
    Missing {
        root: Option<PathBuf>,
        missing: Vec<String>,
    },
    /// A Claude seat was asked for skills Claude cannot load: nested `(name, dir)` entries.
    NotInvocable {
        root: PathBuf,
        skills: Vec<(String, String)>,
    },
    /// A non-Claude seat was asked for skills its mirror excludes (`portable: false`).
    NotPortable {
        root: PathBuf,
        cli: String,
        skills: Vec<String>,
    },
}

impl std::fmt::Display for SkillsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillsError::Config { var, path, why } => write!(
                f,
                "{var}={} is not a usable skills snapshot ({why}); point it at a published \
                 snapshot generation, or unset it to fall back to the installed {PLUGIN_NAME}",
                path.display()
            ),
            SkillsError::Missing {
                root: Some(root),
                missing,
            } => {
                write!(
                    f,
                    "the skills snapshot at {} does not hold the skills this run requires: {}; \
                     enable and republish them (or fix the workflow's skill_ref)",
                    root.display(),
                    missing.join(", ")
                )?;
                let foreign: Vec<&str> = missing
                    .iter()
                    .map(String::as_str)
                    .filter(|r| !is_garden_name(r))
                    .collect();
                if !foreign.is_empty() {
                    write!(
                        f,
                        " — {} {} not a {PLUGIN_NAME} name: the snapshot is the worker's only \
                         skills source, so a skill of another family must be added to the \
                         effective root and published before a run can name it",
                        foreign.join(", "),
                        if foreign.len() == 1 { "is" } else { "are" }
                    )?;
                }
                Ok(())
            }
            SkillsError::Missing {
                root: None,
                missing,
            } => write!(
                f,
                "no skills root is available ({SKILLS_SNAPSHOT_ENV} and {SKILLS_CURRENT_ENV} \
                 unset and no installed {PLUGIN_NAME} found under the claude config dir) but this \
                 run requires skills: {}; install {PLUGIN_NAME} or publish a snapshot",
                missing.join(", ")
            ),
            SkillsError::NotInvocable { root, skills } => write!(
                f,
                "the skills snapshot at {} holds {} only as NESTED skills ({}), which Claude Code \
                 does not discover (a plugin's skills are loaded one directory deep and named by \
                 that directory); route this unit to a skill at the top of skills/ or to a CLI \
                 whose mirror carries nested skills by name",
                root.display(),
                skills
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                skills
                    .iter()
                    .map(|(_, d)| format!("skills/{d}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            SkillsError::NotPortable { root, cli, skills } => write!(
                f,
                "the skills snapshot at {} marks {} as portable: false (Claude-only — they lean on \
                 ${{CLAUDE_PLUGIN_ROOT}}, cwd-relative scripts or ../ links no mirror carries), so \
                 they cannot be handed to '{cli}'; route this unit to a claude seat",
                root.display(),
                skills.join(", ")
            ),
        }
    }
}

impl std::error::Error for SkillsError {}

// ── Resolution ────────────────────────────────────────────────────────────────

/// One explicit path variable: `Ok(None)` when unset, `Ok(Some(path))` when set to a value, and
/// a config error when set to the EMPTY string — an operator who wrote `VAR=` configured
/// something, and "nothing" is not a snapshot.
fn env_path(var: &'static str) -> Result<Option<PathBuf>, SkillsError> {
    match std::env::var_os(var) {
        None => Ok(None),
        Some(v) if v.is_empty() => Err(SkillsError::Config {
            var,
            path: PathBuf::new(),
            why: "it is set but empty — an explicit value must name a snapshot; unset it to use \
                  the ladder"
                .to_string(),
        }),
        Some(v) => Ok(Some(PathBuf::from(v))),
    }
}

/// Resolve the skills root from the process environment, logging the ladder step taken.
/// `Ok(None)` ⇒ no root anywhere (already logged). `Err` ⇒ an explicit input is misconfigured.
pub(crate) fn resolve() -> Result<Option<SkillsSnapshot>, SkillsError> {
    let explicit = env_path(SKILLS_SNAPSHOT_ENV)?;
    let current = env_path(SKILLS_CURRENT_ENV)?;
    resolve_in(
        explicit,
        current,
        std::env::var_os(crate::acp_runner::CLAUDE_CONFIG_DIR_ENV).map(PathBuf::from),
        home_dir(),
        &mut |line| eprintln!("{line}"),
    )
}

/// [`resolve`] with its inputs and its log sink explicit, so the ladder is testable without
/// touching the process environment and the "logged" half of each step is asserted, not assumed.
pub(crate) fn resolve_in(
    explicit: Option<PathBuf>,
    current: Option<PathBuf>,
    claude_config_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    log: &mut dyn FnMut(String),
) -> Result<Option<SkillsSnapshot>, SkillsError> {
    if let Some(path) = explicit {
        return load_published(SKILLS_SNAPSHOT_ENV, &path).map(Some);
    }
    if let Some(pointer) = current {
        // A pointer at SOMETHING is loaded as strictly as an explicit path: a `current` that
        // resolves to a broken snapshot is a misconfiguration, not a reason to fall back. A
        // pointer at NOTHING is the ordinary pre-first-publish state and the ladder continues.
        if std::fs::symlink_metadata(&pointer).is_ok() {
            return load_published(SKILLS_CURRENT_ENV, &pointer).map(Some);
        }
        log(format!(
            "[wicked-core] skills.fallback {SKILLS_SNAPSHOT_ENV} is unset and {SKILLS_CURRENT_ENV}={} \
             points at nothing yet (no snapshot published); trying the installed plugin cache",
            pointer.display()
        ));
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
    // The fallback is a directory the operator did not choose, so it too is pinned to its real
    // path; a cache dir that cannot be canonicalized is not a root.
    let latest = highest_version_dir(&cache)
        .filter(|latest| plugin_manifest_name(latest).as_deref() == Some(PLUGIN_NAME))
        .and_then(|latest| std::fs::canonicalize(latest).ok())
        .map(simplify_verbatim);
    let found = latest.map(|root| load_live(root, SnapshotSource::LiveCache, log));
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

/// Pin `named` to the ABSOLUTE REAL directory it denotes, or say why it cannot be trusted.
///
/// - Relative ⇒ refused: the engine would validate it against ITS cwd and hand it to a worker
///   running somewhere else.
/// - An ancestor that is a symlink ⇒ refused: canonicalizing would pin the generation, but a
///   link ABOVE it is a lever anyone who can flip it holds over every later resolution of the
///   same spelling; the operator is told the real path to pass instead.
/// - The final component MAY be a link (crew's `current -> snapshots/<gen>`): `canonicalize`
///   follows it once, here, so the session keeps the generation it was handed even if the link is
///   flipped under it. A loop or a dangling link is what the OS reports.
/// - On Windows the `\\?\` verbatim prefix `canonicalize` adds is dropped
///   ([`simplify_verbatim`]): a worker CLI is handed a spelling it can open.
fn canonical_root(named: &Path) -> Result<PathBuf, String> {
    if !named.is_absolute() {
        return Err(format!(
            "`{}` is a relative path; the engine validates it here but hands it to a worker \
             running in another directory — pass an absolute path",
            named.display()
        ));
    }
    let mut ancestors: Vec<&Path> = named.ancestors().skip(1).collect();
    ancestors.reverse();
    for ancestor in ancestors {
        if ancestor.parent().is_none() {
            continue; // the filesystem root (or a Windows prefix) is not a link
        }
        if let Ok(meta) = std::fs::symlink_metadata(ancestor) {
            if meta.file_type().is_symlink() {
                let real = std::fs::canonicalize(named)
                    .map(simplify_verbatim)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "<unresolvable>".to_string());
                return Err(format!(
                    "its ancestor `{}` is a symlink; a link above a pinned generation can re-aim \
                     it later — pass the real path `{real}`",
                    ancestor.display()
                ));
            }
        }
    }
    std::fs::canonicalize(named)
        .map(simplify_verbatim)
        .map_err(|e| format!("cannot resolve it to a real path: {e}"))
}

/// Drop the `\\?\` verbatim prefix Windows `canonicalize` adds (`\\?\C:\x` → `C:\x`,
/// `\\?\UNC\srv\share\x` → `\\srv\share\x`). A worker CLI (and a permission-rule glob) wants the
/// ordinary spelling. A no-op for every other prefix and on every other OS.
fn simplify_verbatim(path: PathBuf) -> PathBuf {
    match path.to_str() {
        Some(s) => PathBuf::from(simplify_verbatim_str(s)),
        None => path,
    }
}

fn simplify_verbatim_str(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.to_string()
    }
}

/// Load the snapshot `var` names. Strict: every shortfall is a config error naming the variable,
/// the path and the reason — this path was chosen deliberately, so nothing here degrades.
fn load_published(var: &'static str, named: &Path) -> Result<SkillsSnapshot, SkillsError> {
    let config_err = |why: String| SkillsError::Config {
        var,
        path: named.to_path_buf(),
        why,
    };
    let root = canonical_root(named).map_err(config_err)?;
    let path = root.as_path();
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
    // Every index entry is VERIFIED against the tree it claims to describe, and every defect is
    // collected so one error names them all (an operator fixing a broken publish should not
    // discover them one launch at a time).
    let mut defects: Vec<String> = Vec::new();
    let mut names: BTreeSet<String> = BTreeSet::new();
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
        let name = field("name")?;
        let dir = field("dir")?;
        let portable = entry
            .get("portable")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                config_err(format!(
                    "{SNAPSHOT_INDEX} skills[{i}] (`{name}`) has no boolean `portable` — the \
                     index must say which CLIs may be handed each skill"
                ))
            })?;
        let mut mandates: Vec<String> = entry
            .get("mandates")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if !names.insert(name.clone()) {
            defects.push(format!("skills[{i}]: duplicate name `{name}`"));
        }
        match verify_skill_file(path, &dir) {
            Ok(fm) => {
                match &fm.name {
                    Some(n) if *n == name => {}
                    Some(n) => defects.push(format!(
                        "skills[{i}]: skills/{dir}/{SKILL_FILE} declares name `{n}`, the index \
                         says `{name}`"
                    )),
                    None => defects.push(format!(
                        "skills[{i}]: skills/{dir}/{SKILL_FILE} has no frontmatter `name` to \
                         match `{name}` against"
                    )),
                }
                mandates.extend(fm.mandates);
            }
            Err(why) => defects.push(format!("skills[{i}] (`{name}`): {why}")),
        }
        mandates.sort();
        mandates.dedup();
        skills.push(SkillEntry {
            name,
            dir,
            portable,
            mandates,
        });
    }
    if !defects.is_empty() {
        return Err(config_err(format!(
            "{SNAPSHOT_INDEX} describes skills the tree does not hold as stated: {}",
            defects.join("; ")
        )));
    }
    Ok(SkillsSnapshot {
        root,
        source: SnapshotSource::Published,
        gen: Some(gen),
        content_hash,
        skills,
    })
}

/// The `SKILL.md` an index entry names, verified: `dir` is a clean relative `/`-path, every
/// component from the root down exists and is NOT a symlink (lstat walk — the file must be
/// contained in the root, not pointed at from it), the leaf is a regular file, and it is
/// readable with a frontmatter block. Returns the frontmatter.
fn verify_skill_file(root: &Path, dir: &str) -> Result<Frontmatter, String> {
    let components: Vec<&str> = dir.split('/').collect();
    if dir.contains('\\')
        || components
            .iter()
            .any(|c| c.is_empty() || *c == "." || *c == "..")
    {
        return Err(format!(
            "dir `{dir}` is not a clean relative `/`-separated path under {SKILLS_DIR}/"
        ));
    }
    let mut at = root.join(SKILLS_DIR);
    let rel = |p: &Path| {
        p.strip_prefix(root)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| p.display().to_string())
    };
    for component in components
        .iter()
        .copied()
        .chain(std::iter::once(SKILL_FILE))
    {
        at.push(component);
        let meta =
            std::fs::symlink_metadata(&at).map_err(|e| format!("{} is missing ({e})", rel(&at)))?;
        if meta.file_type().is_symlink() {
            return Err(format!(
                "{} is a symlink — a snapshot's skills must be contained in it, not linked",
                rel(&at)
            ));
        }
    }
    let meta = std::fs::symlink_metadata(&at).map_err(|e| format!("{} ({e})", rel(&at)))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", rel(&at)));
    }
    let text =
        std::fs::read_to_string(&at).map_err(|e| format!("{} is not readable ({e})", rel(&at)))?;
    parse_frontmatter(&text).ok_or_else(|| format!("{} has no `---` frontmatter block", rel(&at)))
}

/// Index an INSTALLED plugin root (no `snapshot.json`): every directory under `skills/` holding a
/// `SKILL.md`, nested ones included, keyed by frontmatter `name`. Symlinks are never followed —
/// the walk stays inside the root it was given. A `SKILL.md` without a parseable frontmatter
/// `name` is SKIPPED and named in a `skills.notice` — never given a derived identity the harness
/// would not agree with. `portable` is approximated from the text (the index is authoritative for
/// a published snapshot; the fallback has none).
fn load_live(root: PathBuf, source: SnapshotSource, log: &mut dyn FnMut(String)) -> SkillsSnapshot {
    let mut skills = Vec::new();
    walk_skills(&root, &root.join(SKILLS_DIR), &[], &mut skills, log);
    skills.sort_by(|a, b| a.dir.cmp(&b.dir));
    SkillsSnapshot {
        root,
        source,
        gen: None,
        content_hash: None,
        skills,
    }
}

fn walk_skills(
    root: &Path,
    dir: &Path,
    rel: &[String],
    out: &mut Vec<SkillEntry>,
    log: &mut dyn FnMut(String),
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // Sorted so the walk — and every line it logs — is the same on every platform, whatever
    // order the directory iterates in.
    let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    children.sort();
    for path in children {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let Some(file_name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let mut child_rel = rel.to_vec();
        child_rel.push(file_name);
        let skill_md = path.join(SKILL_FILE);
        if std::fs::symlink_metadata(&skill_md).is_ok_and(|m| m.is_file()) {
            let dir = child_rel.join("/");
            match std::fs::read_to_string(&skill_md)
                .ok()
                .and_then(|text| parse_frontmatter(&text).map(|fm| (fm, text)))
            {
                Some((
                    Frontmatter {
                        name: Some(name),
                        mandates,
                    },
                    text,
                )) => out.push(SkillEntry {
                    name,
                    dir,
                    portable: !has_nonportable_markers(&text),
                    mandates,
                }),
                _ => log(format!(
                    "[wicked-core] skills.notice {}/{SKILLS_DIR}/{dir}/{SKILL_FILE} has no \
                     parseable frontmatter `name`; the skill is not indexed (the harness would \
                     not know it by a derived name either)",
                    root.display()
                )),
            }
        }
        walk_skills(root, &path, &child_rel, out, log);
    }
}

/// The v3 §5 non-portability markers detectable from a `SKILL.md` alone: a `${CLAUDE_PLUGIN_ROOT}`
/// reference or a `../` sibling link. (cwd-relative script invocations need the publish-time
/// analysis crew runs; a published index carries its verdict.)
fn has_nonportable_markers(text: &str) -> bool {
    text.contains("${CLAUDE_PLUGIN_ROOT}") || text.contains("../")
}

/// The two frontmatter fields this module reads.
#[derive(Debug, Default, PartialEq, Eq)]
struct Frontmatter {
    name: Option<String>,
    mandates: Vec<String>,
}

/// `name:` and `mandates:` from a `---`-fenced YAML frontmatter block, unquoted; `None` when the
/// text has no such block. Line-based on purpose: `name` is a scalar on its own line in every one
/// of garden's skills, `mandates` is a flow list (`[a, b]`) or a block list (`- a` lines), and a
/// YAML dependency for two keys is a dependency too many.
fn parse_frontmatter(text: &str) -> Option<Frontmatter> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let mut fm = Frontmatter::default();
    let mut in_mandates = false;
    for line in lines {
        if line.trim_end() == "---" {
            break;
        }
        if in_mandates {
            // A block-list item: `  - name` (indented) or `- name`; anything else ends the list.
            let item = line
                .trim_start()
                .strip_prefix("- ")
                .filter(|_| line.starts_with([' ', '\t', '-']));
            match item {
                Some(item) => {
                    let item = unquote(item);
                    if !item.is_empty() {
                        fm.mandates.push(item);
                    }
                    continue;
                }
                None => in_mandates = false,
            }
        }
        if let Some(value) = line.strip_prefix("name:") {
            let value = unquote(value);
            if !value.is_empty() {
                fm.name = Some(value);
            }
        } else if let Some(value) = line.strip_prefix("mandates:") {
            let value = value.trim();
            if let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
                fm.mandates
                    .extend(inner.split(',').map(unquote).filter(|s| !s.is_empty()));
            } else if value.is_empty() {
                in_mandates = true;
            } else {
                let single = unquote(value);
                if !single.is_empty() {
                    fm.mandates.push(single);
                }
            }
        }
    }
    fm.mandates.sort();
    fm.mandates.dedup();
    Some(fm)
}

fn unquote(value: &str) -> String {
    value
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .to_string()
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
/// entries are ignored. Candidates are sorted before the pick so the result — and the single
/// line logged about it — never depends on directory iteration order.
fn highest_version_dir(cache: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<(Vec<u64>, PathBuf)> = std::fs::read_dir(cache)
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
        .collect();
    candidates.sort();
    candidates.pop().map(|(_, path)| path)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

// ── Admission ─────────────────────────────────────────────────────────────────

/// Admit a launch on `cli` that requires `refs`: every ref — of ANY family — and everything it
/// transitively `mandates` must resolve in `snapshot`, or the launch is refused naming the
/// missing ones. Then the CLI's own limits apply: a Claude seat cannot be handed a nested skill
/// (no Claude identity exists for it), and a non-Claude seat cannot be handed a `portable: false`
/// one (its mirror excludes it). `Ok(None)` ⇒ nothing required and no root to hand.
pub(crate) fn admit_refs<'a>(
    snapshot: Option<SkillsSnapshot>,
    refs: impl IntoIterator<Item = &'a str>,
    cli: &WorkerCli,
) -> Result<Option<SkillsSnapshot>, SkillsError> {
    let refs: Vec<&str> = refs.into_iter().filter(|r| !r.is_empty()).collect();
    let Some(snapshot) = snapshot else {
        let mut missing: Vec<String> = refs.into_iter().map(str::to_string).collect();
        missing.sort();
        missing.dedup();
        if missing.is_empty() {
            return Ok(None);
        }
        return Err(SkillsError::Missing {
            root: None,
            missing,
        });
    };
    let closure = snapshot.closure(refs);
    if !closure.missing.is_empty() {
        return Err(SkillsError::Missing {
            root: Some(snapshot.root.clone()),
            missing: closure.missing,
        });
    }
    if cli.is_claude() {
        let nested: Vec<(String, String)> = closure
            .required
            .iter()
            .filter(|e| e.is_nested())
            .map(|e| (e.name.clone(), e.dir.clone()))
            .collect();
        if !nested.is_empty() {
            return Err(SkillsError::NotInvocable {
                root: snapshot.root.clone(),
                skills: nested,
            });
        }
    } else {
        let nonportable: Vec<String> = closure
            .required
            .iter()
            .filter(|e| !e.portable)
            .map(|e| e.name.clone())
            .collect();
        if !nonportable.is_empty() {
            return Err(SkillsError::NotPortable {
                root: snapshot.root.clone(),
                cli: cli.to_string(),
                skills: nonportable,
            });
        }
    }
    Ok(Some(snapshot))
}

/// The refs one unit's launch requires: the run-wide set the actor computed
/// ([`StepInput::required_skills`]) plus the unit's own `skill_ref`, so a directly-constructed
/// input is still admitted on its own terms.
pub(crate) fn unit_skill_refs(input: &StepInput) -> impl Iterator<Item = &str> {
    input
        .required_skills
        .iter()
        .map(String::as_str)
        .chain(input.unit.skill_ref.as_deref())
        .filter(|r| !r.is_empty())
}

/// The launch admission for one unit, on either spawn path: resolve the root (the ladder), then
/// require every skill the run names — see [`admit_refs`]. `Ok(None)` ⇒ the unit needs no skill
/// and there is no root to hand it.
///
/// Under the operator's inherit-config escape hatch
/// (`execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV`) the worker runs with the operator's OWN
/// plugins, so no snapshot is handed to it and none is required of it — said out loud, since a
/// set-but-ignored `WICKED_SKILLS_SNAPSHOT` would otherwise read as a silent no-op.
pub(crate) fn admit_unit(
    input: &StepInput,
    cli: &WorkerCli,
) -> Result<Option<SkillsSnapshot>, SkillsError> {
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
    admit_refs(snapshot, unit_skill_refs(input), cli)
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Fixture roots for this module's tests and the two spawn paths'.

    use std::path::{Path, PathBuf};

    /// One index entry for [`snapshot_root_with`]: `(dir, frontmatter name, portable, mandates)`.
    pub(crate) type Entry<'a> = (&'a str, &'a str, bool, &'a [&'a str]);

    /// A process-scoped scratch base, pre-cleaned and CANONICAL (the OS temp dir is a symlink on
    /// macOS — `/var` → `/private/var` — and a short name on Windows CI; snapshot roots are pinned
    /// to their real path, so fixture paths are spelled that way from the start). Keyed by name +
    /// pid + counter so parallel tests (and a stranded dir from a killed run) never share one.
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
        super::simplify_verbatim(std::fs::canonicalize(&dir).unwrap())
    }

    /// `<root>/skills/<dir>` with `dir` joined COMPONENT-WISE (a `/` inside a `join` argument
    /// yields a mixed-separator spelling on Windows).
    pub(crate) fn skill_dir(root: &Path, dir: &str) -> PathBuf {
        dir.split('/')
            .fold(root.join(super::SKILLS_DIR), |p, c| p.join(c))
    }

    /// Write `skills/<dir>/SKILL.md` with a frontmatter `name` under `root`.
    pub(crate) fn write_skill(root: &Path, dir: &str, name: &str) {
        write_skill_with(root, dir, name, "", "");
    }

    /// [`write_skill`] with extra frontmatter lines and body text.
    pub(crate) fn write_skill_with(root: &Path, dir: &str, name: &str, extra_fm: &str, body: &str) {
        let skill = skill_dir(root, dir);
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join(super::SKILL_FILE),
            format!("---\nname: {name}\ndescription: fixture\n{extra_fm}---\n\n# {name}\n{body}"),
        )
        .unwrap();
    }

    /// An INSTALLED-plugin-shaped root at `root` (plugin.json + skills, no index) — the fallback
    /// ladder's candidates.
    pub(crate) fn live_root(root: &Path, version: &str, skills: &[(&str, &str)]) -> PathBuf {
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::write(
            root.join(".claude-plugin").join("plugin.json"),
            format!("{{\"name\":\"wicked-garden\",\"version\":\"{version}\"}}"),
        )
        .unwrap();
        for (dir, name) in skills {
            write_skill(root, dir, name);
        }
        root.to_path_buf()
    }

    /// A PUBLISHED snapshot at `root`: plugin.json + skills + `snapshot.json` indexing them under
    /// `gen`. `skills` are `(dir, frontmatter name)`, all portable, no mandates.
    pub(crate) fn snapshot_root(root: &Path, gen: &str, skills: &[(&str, &str)]) -> PathBuf {
        let entries: Vec<Entry<'_>> = skills
            .iter()
            .map(|(dir, name)| (*dir, *name, true, &[][..]))
            .collect();
        snapshot_root_with(root, gen, &entries)
    }

    /// [`snapshot_root`] with per-skill `portable` and `mandates` (declared in the frontmatter,
    /// as garden will spell them).
    pub(crate) fn snapshot_root_with(root: &Path, gen: &str, skills: &[Entry<'_>]) -> PathBuf {
        live_root(root, "0.0.0", &[]);
        for (dir, name, _, mandates) in skills {
            let fm = if mandates.is_empty() {
                String::new()
            } else {
                format!(
                    "mandates:\n{}",
                    mandates
                        .iter()
                        .map(|m| format!("  - {m}\n"))
                        .collect::<String>()
                )
            };
            write_skill_with(root, dir, name, &fm, "");
        }
        let entries: Vec<serde_json::Value> = skills
            .iter()
            .map(|(dir, name, portable, _)| {
                serde_json::json!({
                    "name": name, "dir": dir, "kind": "fork-worker", "core": false,
                    "portable": portable
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
        super::resolve_in(Some(root.to_path_buf()), None, None, None, &mut |_| {})
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

    fn published(root: &Path) -> Result<Option<SkillsSnapshot>, SkillsError> {
        resolve_in(Some(root.to_path_buf()), None, None, None, &mut |_| {})
    }

    /// The explicit path is loaded from its index: gen, hash, and the skills keyed by name; a ref
    /// resolves by frontmatter name first and by the dir-derived name second; the Claude identity
    /// is the top-level DIRECTORY (what Claude Code's plugin loader exposes), and a nested skill
    /// has none — its identity is not invented.
    #[test]
    fn an_explicit_snapshot_is_indexed_and_refs_resolve_by_name_then_by_dir() {
        let base = scratch("published");
        let root = snapshot_root(
            &base.join("snapshots").join("7"),
            "7",
            &[
                ("domain", "wicked-garden-domain"),
                ("engineering/frontend", "wicked-garden-engineering-frontend"),
                ("qe-oracle", "wicked-garden-test-oracle"),
            ],
        );
        let mut lines = Vec::new();
        let s = resolve_in(
            Some(root.clone()),
            None,
            None,
            None,
            &mut collect(&mut lines),
        )
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
        assert!(s.skill("wicked-garden-domain").unwrap().portable);
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
            s.claude_skill_dir("wicked-garden-test-oracle").as_deref(),
            Some("qe-oracle"),
            "Claude's id is the directory, not the frontmatter name"
        );
        assert_eq!(
            s.claude_skill_dir("wicked-garden-engineering-frontend"),
            None,
            "a nested skill has no Claude identity — none is invented"
        );
        assert!(s
            .skill("wicked-garden-engineering-frontend")
            .unwrap()
            .is_nested());
        assert!(s.skill("wicked-garden-jam").is_none());
        assert_eq!(s.gen_label(), "gen=7");
        assert!(s
            .launch_line("run=r1 unit=2")
            .starts_with("[wicked-core] skills.snapshot gen=7 root="));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An explicit path that is missing, not a plugin root, another plugin, or has no parseable
    /// index is a CONFIG ERROR naming the variable, the path and the reason — never a fallback.
    #[test]
    fn an_explicit_path_that_is_not_a_snapshot_is_a_config_error_naming_why() {
        let base = scratch("config-err");
        let expect_err = |path: &Path, needle: &str| {
            let mut lines = Vec::new();
            let err = resolve_in(
                Some(path.to_path_buf()),
                None,
                None,
                None,
                &mut collect(&mut lines),
            )
            .expect_err("a config error");
            assert!(
                lines.is_empty(),
                "no fallback log on a config error: {lines:?}"
            );
            let SkillsError::Config { var, path: p, why } = &err else {
                panic!("expected Config, got {err:?}");
            };
            assert_eq!(*var, SKILLS_SNAPSHOT_ENV);
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
        expect_err(&base.join("does-not-exist"), "cannot resolve");
        let file = base.join("a-file");
        std::fs::write(&file, "x").unwrap();
        expect_err(&file, "not a directory");
        let bare = base.join("bare");
        std::fs::create_dir_all(bare.join("skills")).unwrap();
        expect_err(&bare, PLUGIN_MANIFEST);
        let other = live_root(&base.join("other"), "1.0.0", &[]);
        std::fs::write(
            other.join(".claude-plugin").join("plugin.json"),
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
        std::fs::write(
            bad.join(SNAPSHOT_INDEX),
            "{\"gen\":\"3\",\"skills\":[{\"name\":\"wicked-garden-core\",\"dir\":\"core\"}]}",
        )
        .unwrap();
        expect_err(&bad, "no boolean `portable`");
        std::fs::write(bad.join(SNAPSHOT_INDEX), "{\"skills\":[]}").unwrap();
        expect_err(&bad, "`gen`");
        std::fs::write(bad.join(SNAPSHOT_INDEX), "not json").unwrap();
        expect_err(&bad, "not valid JSON");
        // Relative paths are refused outright: they mean something else in the worker's cwd.
        let rel = Path::new("snapshots/7");
        let err = published(rel).expect_err("relative");
        let SkillsError::Config { why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert!(why.contains("relative"), "{why}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The index is VERIFIED against the tree: an entry whose `SKILL.md` is missing, is reached
    /// through a symlink, is not a regular file, or declares a different `name` is a config error
    /// — and ALL such entries are listed in the one error, so a broken publish is diagnosed at
    /// once rather than one launch at a time.
    #[test]
    fn an_index_entry_the_tree_does_not_hold_as_stated_is_a_config_error_listing_every_defect() {
        let base = scratch("verify");
        let root = snapshot_root(
            &base.join("snap"),
            "5",
            &[
                ("domain", "wicked-garden-domain"),
                ("mem", "wicked-garden-mem"),
                ("search", "wicked-garden-search"),
                ("qe", "wicked-garden-qe"),
            ],
        );
        assert!(published(&root).is_ok(), "the intact fixture loads");

        // (1) listed but absent; (2) frontmatter name disagrees with the index;
        // (3) SKILL.md is a directory, not a file.
        std::fs::remove_dir_all(skill_dir(&root, "domain")).unwrap();
        write_skill(&root, "mem", "wicked-garden-memory");
        let qe = skill_dir(&root, "qe");
        std::fs::remove_file(qe.join(SKILL_FILE)).unwrap();
        std::fs::create_dir_all(qe.join(SKILL_FILE)).unwrap();

        let err = published(&root).expect_err("three defects");
        let SkillsError::Config { why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert!(
            why.contains("`wicked-garden-domain`") && why.contains("is missing"),
            "{why}"
        );
        assert!(
            why.contains(
                "declares name `wicked-garden-memory`, the index says `wicked-garden-mem`"
            ),
            "{why}"
        );
        assert!(
            why.contains("skills/qe/SKILL.md is not a regular file"),
            "{why}"
        );
        assert!(
            !why.contains("wicked-garden-search"),
            "the intact entry is not accused: {why}"
        );

        // A duplicate name and an unclean dir are index defects too.
        let dup = snapshot_root(
            &base.join("dup"),
            "6",
            &[("a", "wicked-garden-a"), ("b", "wicked-garden-b")],
        );
        std::fs::write(
            dup.join(SNAPSHOT_INDEX),
            r#"{"gen":"6","skills":[
                {"name":"wicked-garden-a","dir":"a","portable":true},
                {"name":"wicked-garden-a","dir":"b","portable":true},
                {"name":"wicked-garden-c","dir":"../escape","portable":true}]}"#,
        )
        .unwrap();
        let err = published(&dup).expect_err("duplicate + unclean");
        let SkillsError::Config { why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert!(why.contains("duplicate name `wicked-garden-a`"), "{why}");
        assert!(why.contains("not a clean relative"), "{why}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Containment is an lstat walk: a `SKILL.md` (or a directory on the way to it) that is a
    /// symlink — even to a file INSIDE the root — is refused, because a snapshot's skills are
    /// contained in it, not pointed at from it.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_skill_file_is_refused_even_when_it_points_inside_the_root() {
        let base = scratch("contain");
        let root = snapshot_root(
            &base.join("snap"),
            "8",
            &[
                ("domain", "wicked-garden-domain"),
                ("mem", "wicked-garden-mem"),
            ],
        );
        // mem/SKILL.md -> ../domain/SKILL.md (inside the root, wrong identity anyway).
        let mem = skill_dir(&root, "mem").join(SKILL_FILE);
        std::fs::remove_file(&mem).unwrap();
        std::os::unix::fs::symlink(skill_dir(&root, "domain").join(SKILL_FILE), &mem).unwrap();
        let err = published(&root).expect_err("a linked skill file");
        let SkillsError::Config { why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert!(why.contains("skills/mem/SKILL.md is a symlink"), "{why}");
        // A linked DIRECTORY on the way is refused at the directory.
        let outside = base.join("outside");
        write_skill(&outside, "domain", "wicked-garden-domain");
        std::fs::remove_dir_all(skill_dir(&root, "domain")).unwrap();
        std::os::unix::fs::symlink(skill_dir(&outside, "domain"), skill_dir(&root, "domain"))
            .unwrap();
        let err = published(&root).expect_err("a linked skill dir");
        let SkillsError::Config { why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert!(why.contains("skills/domain is a symlink"), "{why}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The ladder when both variables are UNSET: the live cache's HIGHEST version (numerically)
    /// wins and is LOGGED as `skills.fallback` exactly once; no cache ⇒ no root, also logged — the
    /// operator's hand copy at `<config>/plugins/wicked-garden` is NEVER a fallback (it is the
    /// stale copy the snapshot retires). Paths are compared as `Path`s and the logged root is the
    /// one the resolver picked, spelled as IT spells it.
    #[test]
    fn unset_falls_back_to_the_live_cache_only_never_the_hand_copy_and_logs_each_step() {
        let base = scratch("ladder");
        let config = base.join("claude-config");
        let cache = config
            .join("plugins")
            .join("cache")
            .join("wicked-garden")
            .join("wicked-garden");
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
            &config.join("plugins").join("wicked-garden"),
            "12.28.1",
            &[("core", "wicked-garden-core")],
        );
        let mut lines = Vec::new();
        let picked = resolve_in(
            None,
            None,
            Some(config.clone()),
            None,
            &mut collect(&mut lines),
        )
        .unwrap()
        .expect("the live cache is a root");
        assert_eq!(picked.source, SnapshotSource::LiveCache);
        assert_eq!(
            picked.root,
            cache.join("12.32.0"),
            "numeric, not lexical (compared as paths)"
        );
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
                && lines[0].contains(&picked.root.display().to_string()),
            "the fallback is logged with the root it chose: {}",
            lines[0]
        );

        // Cache gone, hand copy still there ⇒ NO root: the hand copy is not on the ladder.
        std::fs::remove_dir_all(config.join("plugins").join("cache")).unwrap();
        let mut lines = Vec::new();
        let none = resolve_in(
            None,
            None,
            Some(config.clone()),
            None,
            &mut collect(&mut lines),
        )
        .unwrap();
        assert!(
            none.is_none(),
            "the hand copy at <config>/plugins/wicked-garden must never be a fallback: {none:?}"
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("skills.fallback") && lines[0].contains("WITHOUT"),
            "{lines:?}"
        );

        // No config dir given ⇒ `<home>/.claude` is the config dir.
        let home = base.join("home");
        let home_cache = home
            .join(".claude")
            .join("plugins")
            .join("cache")
            .join("wicked-garden")
            .join("wicked-garden")
            .join("1.0.0");
        live_root(&home_cache, "1.0.0", &[("core", "wicked-garden-core")]);
        let picked = resolve_in(None, None, None, Some(home.clone()), &mut |_| {})
            .unwrap()
            .unwrap();
        assert_eq!(picked.root, home_cache);

        // Numeric `gen` in a published index.
        let numeric = snapshot_root(&base.join("num"), "9", &[]);
        std::fs::write(numeric.join(SNAPSHOT_INDEX), "{\"gen\":12,\"skills\":[]}").unwrap();
        let s = published(&numeric).unwrap().unwrap();
        assert_eq!(s.gen.as_deref(), Some("12"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The ladder's SECOND rung: with the snapshot variable unset, crew's `current` pointer is
    /// used when it resolves (loaded as strictly as an explicit path — a broken target is a config
    /// error naming THAT variable), and when it points at nothing yet the ladder logs the step
    /// and continues to the live cache.
    #[test]
    fn unset_snapshot_uses_current_when_it_resolves_and_falls_through_when_it_does_not() {
        let base = scratch("current");
        let gen3 = snapshot_root(
            &base.join("snapshots").join("3"),
            "3",
            &[("domain", "wicked-garden-domain")],
        );
        let config = base.join("claude-config");
        let cache_root = config
            .join("plugins")
            .join("cache")
            .join("wicked-garden")
            .join("wicked-garden")
            .join("1.0.0");
        live_root(&cache_root, "1.0.0", &[("core", "wicked-garden-core")]);

        // `current` resolves (here: the concrete generation dir itself) ⇒ the published root,
        // no fallback line.
        let mut lines = Vec::new();
        let s = resolve_in(
            None,
            Some(gen3.clone()),
            Some(config.clone()),
            None,
            &mut collect(&mut lines),
        )
        .unwrap()
        .unwrap();
        assert_eq!(s.root, gen3);
        assert_eq!(s.source, SnapshotSource::Published);
        assert!(lines.is_empty(), "{lines:?}");

        // `current` points at nothing ⇒ logged, then the live cache.
        let mut lines = Vec::new();
        let s = resolve_in(
            None,
            Some(base.join("skills").join("current")),
            Some(config.clone()),
            None,
            &mut collect(&mut lines),
        )
        .unwrap()
        .unwrap();
        assert_eq!(s.root, cache_root);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].contains(SKILLS_CURRENT_ENV) && lines[0].contains("points at nothing"),
            "{lines:?}"
        );

        // `current` resolves to something that is NOT a snapshot ⇒ a config error naming
        // WICKED_SKILLS_CURRENT — never a silent fall-through.
        let broken = base.join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        let err = resolve_in(None, Some(broken.clone()), Some(config), None, &mut |_| {})
            .expect_err("a resolving pointer is loaded strictly");
        let SkillsError::Config { var, path, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert_eq!(*var, SKILLS_CURRENT_ENV);
        assert_eq!(path, &broken);
        assert!(err.to_string().contains(SKILLS_CURRENT_ENV));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A variable that is SET BUT EMPTY is invalid explicit configuration — a config error, not
    /// "unset" — for both inputs.
    #[test]
    fn an_empty_explicit_value_is_a_config_error_not_unset() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            [SKILLS_SNAPSHOT_ENV, SKILLS_CURRENT_ENV]
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect();
        for var in [SKILLS_SNAPSHOT_ENV, SKILLS_CURRENT_ENV] {
            std::env::remove_var(SKILLS_SNAPSHOT_ENV);
            std::env::remove_var(SKILLS_CURRENT_ENV);
            std::env::set_var(var, "");
            let err = env_path(var).expect_err("empty is not unset");
            let SkillsError::Config { var: v, why, .. } = &err else {
                panic!("expected Config, got {err:?}");
            };
            assert_eq!(*v, var);
            assert!(why.contains("set but empty"), "{why}");
            assert!(
                resolve().is_err(),
                "the ladder itself refuses an empty {var}"
            );
        }
        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }

    /// An explicit path whose FINAL component is a symlink (crew's `current -> snapshots/<gen>`)
    /// is PINNED to the real generation it points at when loaded, so a session keeps the
    /// generation it was handed while a later resolve — after crew flips the link — gets the next
    /// one. An ANCESTOR symlink is refused with the real path to pass instead; a loop is a config
    /// error, not a hang.
    #[cfg(unix)]
    #[test]
    fn a_current_link_is_pinned_to_its_concrete_generation_at_load() {
        let base = scratch("pin");
        let gen7 = snapshot_root(
            &base.join("snapshots").join("7"),
            "7",
            &[("domain", "wicked-garden-domain")],
        );
        let gen8 = snapshot_root(
            &base.join("snapshots").join("8"),
            "8",
            &[("domain", "wicked-garden-domain")],
        );
        let current = base.join("current");
        std::os::unix::fs::symlink("snapshots/7", &current).unwrap();

        let first = published(&current).unwrap().unwrap();
        assert_eq!(
            first.root, gen7,
            "the link is resolved to the real generation it names"
        );
        assert_eq!(first.gen.as_deref(), Some("7"));

        // crew publishes gen 8 and flips `current` — the session already handed gen 7 is unaffected.
        std::fs::remove_file(&current).unwrap();
        std::os::unix::fs::symlink(base.join("snapshots").join("8"), &current).unwrap();
        let second = published(&current).unwrap().unwrap();
        assert_eq!(second.root, gen8, "an absolute link target resolves too");
        assert_eq!(second.gen.as_deref(), Some("8"));
        assert_eq!(
            first.root, gen7,
            "the earlier snapshot still names its own generation"
        );
        assert_ne!(first.root, second.root);

        // An ancestor link: `<base>/linked-snapshots -> snapshots`, then `.../linked-snapshots/7`.
        let linked = base.join("linked-snapshots");
        std::os::unix::fs::symlink(base.join("snapshots"), &linked).unwrap();
        let err = published(&linked.join("7")).expect_err("an ancestor symlink");
        let SkillsError::Config { path, why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert_eq!(path, &linked.join("7"), "the error names the path as given");
        assert!(
            why.contains("ancestor") && why.contains(&gen7.display().to_string()),
            "names the link and the real path to pass: {why}"
        );

        // A loop is what the OS reports, as a config error for the path the operator set.
        let a = base.join("loop-a");
        let b = base.join("loop-b");
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        let err = published(&a).expect_err("a loop");
        let SkillsError::Config { path, why, .. } = err else {
            panic!("expected Config");
        };
        assert_eq!(
            path, a,
            "the error names the path the operator set, not a hop"
        );
        assert!(why.contains("cannot resolve"), "{why}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The Windows verbatim prefix `canonicalize` adds is dropped from the spelling a worker is
    /// handed; every other spelling passes through untouched. String-level so the rule is tested
    /// on every CI platform, not only the one where it bites.
    #[test]
    fn the_windows_verbatim_prefix_is_simplified_and_nothing_else_is_touched() {
        assert_eq!(
            simplify_verbatim_str(r"\\?\C:\Users\me\snap"),
            r"C:\Users\me\snap"
        );
        assert_eq!(
            simplify_verbatim_str(r"\\?\UNC\srv\share\snap"),
            r"\\srv\share\snap"
        );
        assert_eq!(
            simplify_verbatim_str("/private/var/snap"),
            "/private/var/snap"
        );
        assert_eq!(simplify_verbatim_str(r"C:\plain"), r"C:\plain");
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
            &mut |_| {},
        );
        let json = live
            .handed_event("run-1", 3, 1, "wrapped_cli", "claude")
            .to_json();
        assert!(json["gen"].is_null() && json["contentHash"].is_null());
        assert_eq!(json["source"], "live-cache");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A run naming skills the snapshot does not hold is REFUSED with exactly those names (sorted,
    /// deduplicated); a ref of ANOTHER FAMILY is judged the same way — refused when absent, with
    /// the message saying why the snapshot could not hold it by default, admitted when present;
    /// with no root at all every ref is missing while a skill-less run is admitted.
    #[test]
    fn missing_required_skills_are_refused_by_name_whatever_their_family() {
        let base = scratch("admit");
        let root = snapshot_root(
            &base.join("snap"),
            "4",
            &[
                ("domain", "wicked-garden-domain"),
                (
                    "acceptance-test-writer",
                    "wicked-testing-acceptance-test-writer",
                ),
            ],
        );
        let snapshot = load(&root);
        let claude = WorkerCli::Claude;

        // A foreign-family skill that IS in the snapshot (a user-added skill) is admitted.
        let ok = admit_refs(
            Some(snapshot.clone()),
            [
                "wicked-garden-domain",
                "wicked-testing-acceptance-test-writer",
            ],
            &claude,
        )
        .expect("both are present");
        assert_eq!(ok.as_ref().map(|s| &s.root), Some(&root));

        // A foreign-family skill that is NOT is refused — there is no exemption by family.
        let err = admit_refs(
            Some(snapshot.clone()),
            ["wicked-garden-domain", "wicked-testing-plan"],
            &claude,
        )
        .expect_err("the foreign ref is missing");
        assert_eq!(
            err,
            SkillsError::Missing {
                root: Some(root.clone()),
                missing: vec!["wicked-testing-plan".to_string()],
            }
        );
        let msg = err.to_string();
        assert!(
            msg.contains("wicked-testing-plan is not a wicked-garden name")
                && msg.contains("added to the effective root"),
            "the refusal says why the snapshot could not hold it by default: {msg}"
        );

        let err = admit_refs(
            Some(snapshot.clone()),
            [
                "wicked-garden-domain-coverage",
                "wicked-garden-domain",
                "wicked-garden-domain-extractor",
                "wicked-garden-domain-coverage",
            ],
            &claude,
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
                && msg.contains(&root.display().to_string())
                && !msg.contains("not a wicked-garden name"),
            "{msg}"
        );

        let err = admit_refs(None, ["wicked-garden-domain"], &claude).expect_err("no root");
        assert_eq!(
            err,
            SkillsError::Missing {
                root: None,
                missing: vec!["wicked-garden-domain".to_string()],
            }
        );
        assert!(err.to_string().contains(SKILLS_SNAPSHOT_ENV));
        assert!(
            admit_refs(None, [], &claude).unwrap().is_none(),
            "a run that names no skill proceeds without a root"
        );
        assert!(
            admit_refs(None, [""], &claude).unwrap().is_none(),
            "an empty ref is no ref"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The requirement is the transitive closure over `mandates`: a run that names `repo-learn`
    /// also requires what repo-learn mandates (`search`, `mem`) and what THOSE mandate; a missing
    /// mandate is named as itself in the refusal. Mandates come from the frontmatter and from the
    /// index (union); a cycle terminates.
    #[test]
    fn required_skills_expand_through_transitive_mandates() {
        let base = scratch("mandates");
        let root = snapshot_root_with(
            &base.join("snap"),
            "10",
            &[
                (
                    "repo-learn",
                    "wicked-garden-repo-learn",
                    true,
                    &["wicked-garden-search", "wicked-garden-mem"],
                ),
                (
                    "search",
                    "wicked-garden-search",
                    true,
                    &["wicked-garden-core"],
                ),
                (
                    "mem",
                    "wicked-garden-mem",
                    true,
                    &["wicked-garden-repo-learn"],
                ),
                ("core", "wicked-garden-core", true, &[]),
                ("domain", "wicked-garden-domain", true, &[]),
            ],
        );
        let s = load(&root);
        assert_eq!(
            s.skill("wicked-garden-repo-learn").unwrap().mandates,
            vec!["wicked-garden-mem", "wicked-garden-search"],
            "frontmatter mandates are read (sorted)"
        );
        let closure = s.closure(["wicked-garden-repo-learn"]);
        assert!(closure.missing.is_empty());
        assert_eq!(
            closure
                .required
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "wicked-garden-core",
                "wicked-garden-mem",
                "wicked-garden-repo-learn",
                "wicked-garden-search",
            ],
            "the transitive closure, cycle included, domain excluded"
        );
        // Remove `core` from the tree AND the index: repo-learn's closure now has a hole two
        // hops down, and the refusal names the hole, not repo-learn.
        std::fs::remove_dir_all(skill_dir(&root, "core")).unwrap();
        let mut index: Value =
            serde_json::from_slice(&std::fs::read(root.join(SNAPSHOT_INDEX)).unwrap()).unwrap();
        index["skills"]
            .as_array_mut()
            .unwrap()
            .retain(|e| e["dir"] != "core");
        std::fs::write(
            root.join(SNAPSHOT_INDEX),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
        let s = load(&root);
        let err = admit_refs(Some(s), ["wicked-garden-repo-learn"], &WorkerCli::Claude)
            .expect_err("a mandate two hops down is missing");
        assert_eq!(
            err,
            SkillsError::Missing {
                root: Some(root.clone()),
                missing: vec!["wicked-garden-core".to_string()],
            }
        );
        // Index-declared mandates are honoured too (union with the frontmatter).
        let idx = snapshot_root(
            &base.join("idx"),
            "11",
            &[("a", "wicked-garden-a"), ("b", "wicked-garden-b")],
        );
        std::fs::write(
            idx.join(SNAPSHOT_INDEX),
            r#"{"gen":"11","skills":[
                {"name":"wicked-garden-a","dir":"a","portable":true,"mandates":["wicked-garden-b"]},
                {"name":"wicked-garden-b","dir":"b","portable":true}]}"#,
        )
        .unwrap();
        let s = load(&idx);
        assert_eq!(
            s.closure(["wicked-garden-a"]).required.len(),
            2,
            "the index's mandate pulled b in"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Admission knows the CLI: a Claude seat is refused a NESTED skill (Claude Code exposes
    /// plugin skills one directory deep, named by directory — a nested one has no identity to
    /// invoke), while a mirror CLI reaches it by frontmatter name; a mirror CLI is refused a
    /// `portable: false` skill (its mirror excludes it), while Claude loads it from the plugin.
    /// Both refusals are structured and name the skills.
    #[test]
    fn admission_refuses_nested_skills_for_claude_and_nonportable_skills_for_other_clis() {
        let base = scratch("cli");
        let root = snapshot_root_with(
            &base.join("snap"),
            "12",
            &[
                ("domain", "wicked-garden-domain", true, &[]),
                (
                    "engineering/frontend",
                    "wicked-garden-engineering-frontend",
                    true,
                    &[],
                ),
                (
                    "domain-extractor",
                    "wicked-garden-domain-extractor",
                    false,
                    &[],
                ),
                (
                    "engineering",
                    "wicked-garden-engineering",
                    true,
                    &["wicked-garden-engineering-frontend"],
                ),
            ],
        );
        let s = load(&root);
        let (claude, codex) = (WorkerCli::Claude, WorkerCli::Other("codex".into()));

        // Nested: refused for Claude (directly AND through a mandate), admitted for codex.
        let err = admit_refs(
            Some(s.clone()),
            ["wicked-garden-engineering-frontend"],
            &claude,
        )
        .expect_err("nested on claude");
        assert_eq!(
            err,
            SkillsError::NotInvocable {
                root: root.clone(),
                skills: vec![(
                    "wicked-garden-engineering-frontend".to_string(),
                    "engineering/frontend".to_string()
                )],
            }
        );
        let msg = err.to_string();
        assert!(
            msg.contains("skills/engineering/frontend") && msg.contains("one directory deep"),
            "{msg}"
        );
        assert!(
            matches!(
                admit_refs(Some(s.clone()), ["wicked-garden-engineering"], &claude),
                Err(SkillsError::NotInvocable { .. })
            ),
            "a top-level skill that MANDATES a nested one is refused for claude too"
        );
        assert!(admit_refs(
            Some(s.clone()),
            ["wicked-garden-engineering-frontend"],
            &codex
        )
        .unwrap()
        .is_some());

        // Non-portable: refused for codex, admitted for Claude.
        let err = admit_refs(
            Some(s.clone()),
            ["wicked-garden-domain", "wicked-garden-domain-extractor"],
            &codex,
        )
        .expect_err("non-portable on codex");
        assert_eq!(
            err,
            SkillsError::NotPortable {
                root: root.clone(),
                cli: "codex".to_string(),
                skills: vec!["wicked-garden-domain-extractor".to_string()],
            }
        );
        assert!(err.to_string().contains("'codex'"), "{err}");
        assert!(admit_refs(
            Some(s.clone()),
            ["wicked-garden-domain", "wicked-garden-domain-extractor"],
            &claude
        )
        .unwrap()
        .is_some());
        assert_eq!(WorkerCli::for_seat(true, "whatever"), WorkerCli::Claude);
        assert_eq!(
            WorkerCli::for_seat(false, "pi"),
            WorkerCli::Other("pi".into())
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The live walk indexes nested skills, SKIPS (with a `skills.notice` naming it) a `SKILL.md`
    /// without a frontmatter name rather than inventing one, never follows a symlink out of the
    /// root, and approximates `portable` from the text.
    #[cfg(unix)]
    #[test]
    fn the_live_walk_indexes_nested_skills_skips_nameless_ones_and_does_not_follow_links() {
        let base = scratch("walk");
        let root = live_root(
            &base.join("plugin"),
            "1.0.0",
            &[
                ("qe", "wicked-garden-qe"),
                ("qe/a11y", "wicked-garden-qe-a11y"),
            ],
        );
        write_skill_with(
            &root,
            "domain-extractor",
            "wicked-garden-domain-extractor",
            "",
            "Run `python3 ${CLAUDE_PLUGIN_ROOT}/scripts/x.py`\n",
        );
        std::fs::create_dir_all(root.join("skills").join("bare")).unwrap();
        std::fs::write(
            root.join("skills").join("bare").join("SKILL.md"),
            "# no frontmatter\n",
        )
        .unwrap();
        let outside = base.join("outside");
        write_skill(&outside, "leak", "wicked-garden-leak");
        std::os::unix::fs::symlink(outside.join("skills"), root.join("skills").join("linked"))
            .unwrap();

        let mut lines = Vec::new();
        let s = load_live(
            root.clone(),
            SnapshotSource::LiveCache,
            &mut collect(&mut lines),
        );
        let names: Vec<&str> = s.skills().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "wicked-garden-domain-extractor",
                "wicked-garden-qe",
                "wicked-garden-qe-a11y"
            ],
            "sorted by dir; the nameless one is skipped, the linked tree is not walked"
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("skills.notice")
                && lines[0].contains("skills/bare/SKILL.md")
                && lines[0].contains("not indexed"),
            "{lines:?}"
        );
        assert_eq!(s.skill("wicked-garden-qe-a11y").unwrap().dir, "qe/a11y");
        assert!(s.skill("wicked-garden-qe").unwrap().portable);
        assert!(
            !s.skill("wicked-garden-domain-extractor").unwrap().portable,
            "a ${{CLAUDE_PLUGIN_ROOT}} reference marks the skill Claude-only"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn frontmatter_reads_name_and_mandates_in_both_list_spellings() {
        let base = scratch("fm");
        let quoted = base.join("SKILL.md");
        std::fs::write(
            &quoted,
            "---\ndescription: |\n  multi\n  line\nname: \"wicked-garden-x\"\nmandates: [wicked-garden-b, \"wicked-garden-a\"]\n---\nbody\nmandates: not-in-frontmatter\n",
        )
        .unwrap();
        let fm = parse_frontmatter(&std::fs::read_to_string(&quoted).unwrap()).unwrap();
        assert_eq!(fm.name.as_deref(), Some("wicked-garden-x"));
        assert_eq!(fm.mandates, vec!["wicked-garden-a", "wicked-garden-b"]);
        let block = parse_frontmatter(
            "---\nname: wicked-garden-y\nmandates:\n  - wicked-garden-search\n  - 'wicked-garden-mem'\nuser-invocable: true\n---\n",
        )
        .unwrap();
        assert_eq!(block.name.as_deref(), Some("wicked-garden-y"));
        assert_eq!(
            block.mandates,
            vec!["wicked-garden-mem", "wicked-garden-search"]
        );
        assert_eq!(
            parse_frontmatter("# no frontmatter\nname: not-in-frontmatter\n"),
            None
        );
        assert_eq!(
            parse_frontmatter("---\ndescription: only\n---\n"),
            Some(Frontmatter::default())
        );
        assert_eq!(derived_name("qe/a11y"), "wicked-garden-qe-a11y");
        assert!(is_garden_name("wicked-garden-qe") && !is_garden_name("wicked-testing-qe"));
        assert!(!is_garden_name("wicked-garden-") && !is_garden_name("wicked-garden"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
