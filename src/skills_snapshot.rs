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
//! - both governance carriers read-widen to the snapshot (`execute_wrapped::assemble_read_roots`);
//!   the worker Read fence over crew's state home is an EXPLICIT denylist (`state_home`,
//!   `execute_wrapped::deny_rules`) under which the resolved `skills/snapshots/<gen>/` is the one
//!   non-denied path. The state home is the ACTUAL one — DERIVED from the snapshot's own path and
//!   from nothing else (`<state home>/skills/snapshots/<gen>`: the parent literally `snapshots`,
//!   the grandparent literally `skills`, the state home three components up —
//!   `state_home::of_snapshot`; design v3.4 §2 retired the round-4 companion variable
//!   `WICKED_CREW_STATE_HOME`, which is not read anywhere) — never a `.wicked-crew` basename: a
//!   scratch daemon on a custom state home is fenced exactly like the default one. A snapshot
//!   without that shape is a config error at load naming the path, and one anywhere else inside
//!   the fence FAILS the launch (`execute_wrapped::fence_check`, v3.1 §1); writes under it stay
//!   denied — a snapshot is immutable by contract, and this module never writes into one;
//! - the skill directive is CLI-aware (`execute_wrapped::plugin_skill_invocation`): the plugin
//!   form for Claude, the mirrored directory name for every other CLI.
//!
//! # Where the root comes from — the degradation ladder (v3 §3 / v3.1 §2), no unconditional fail-open
//!
//! The engine reads exactly ONE input. (`WICKED_SKILLS_CURRENT`, a second rung pass 1 added, is
//! withdrawn: crew resolves its `current` pointer and passes the concrete generation path.)
//!
//! 1. [`SKILLS_SNAPSHOT_ENV`] set ⇒ that path, strictly ([`load_published`]): it must be absolute,
//!    EVERY component including the LAST must be a real directory — no ancestor may be a symlink
//!    (a link above a pinned generation could re-aim it later) and neither may the final component
//!    (crew's `current -> snapshots/<gen>` is resolved by CREW before the handoff, v3.4 §2; a
//!    handed `current` is a config error naming the link and its target, so a fresh launch can
//!    never change generation between two units of one run) — its identity must hold
//!    (`snapshot.json.gen` equals the generation directory's name; `contentHash` and `gardenSource`
//!    present; every skill keyed by its frontmatter `name`, which must equal the path-derived
//!    name), and its index must describe files that actually exist — every component from the
//!    root down (`.claude-plugin/`, `plugin.json`, `snapshot.json`, `skills/`, each skill
//!    directory, each `SKILL.md`) is lstat-verified NOT to be a symlink and read without following
//!    one. Any shortfall is a config error — a deliberately chosen snapshot is never silently
//!    swapped. Set but EMPTY is invalid explicit configuration, not "unset".
//! 2. Unset ⇒ the LIVE installed garden: the marketplace cache's highest version under the
//!    daemon's `CLAUDE_CONFIG_DIR` (else `~/.claude`), logged as `skills.fallback`. NOT the hand
//!    copy at `<config>/plugins/wicked-garden` — that stale copy is the defect being fixed, never
//!    a fallback. Nothing in the cache ⇒ no root at all (a run that needs a skill is then refused;
//!    a run that needs none proceeds). ABSENCE is told apart from FAILURE (codex round 7,
//!    [`Ladder`]): a cache that cannot be listed, a symlinked or unresolvable version candidate, a
//!    malformed manifest — each is logged with its error and takes the no-root rung WITH the
//!    reason, which rides the refusal of any run that names a skill
//!    ([`SkillsError::FallbackFailed`]); never a silent "no installed garden". The fallback root is
//!    CLAUDE-ONLY ([`SkillsError::FallbackClaudeOnly`]): its index carries no publish-time
//!    portability verdict (cwd-relative script detection is crew's publish analyzer, not
//!    re-implemented here), so no non-Claude seat is handed anything from it, and a non-Claude
//!    unit that invokes a skill is refused naming the seat — non-Claude delivery requires a
//!    published snapshot.
//!
//! A PUBLISHED root is verified for exact INDEX/TREE PARITY (codex round 7,
//! [`SkillsSnapshot::verify_delivered_tree`]): the whole delivered closure is lstat-walked — every
//! `SKILL.md` under `skills/` must be indexed (a disabled or unpublished skill copied in is refused
//! by path), every indexed entry must exist, `views/` may hold only the copilot view (judged whole,
//! `copilot_view_for`), and NO symlink may sit anywhere in the generation — the one exception is
//! crew's root-level `.venv` link, accepted only when `snapshot.json.venv` is `synced` and it
//! resolves inside canonical `<state home>/skills/baseline/<recorded baseline>/.venv` — the
//! `gardenSource.baseline` the same file records, never any other bundle's env (crew's own
//! `verifyCurrent` rule; review pass 11). A `synced` generation without the link is refused too.
//!
//! # Admission — before any process starts
//!
//! EXISTENCE is judged plan-wide: every `skill_ref` the run names ([`StepInput::required_skills`]
//! plus the unit's own), expanded through each skill's transitive `mandates`, must be in the root
//! ([`admit_refs`]). A missing skill of ANY family is a refusal naming it
//! ([`SkillsError::Missing`]) — the snapshot is the worker's only skills source, so nothing
//! outside it is exempt. INVOCABILITY is judged per seat (v3.1 §5): only the skills THIS unit's
//! seat will invoke — its own `skill_ref` and what that mandates — are checked against the CLI
//! that runs it: a nested skill has no Claude identity (Claude Code discovers a plugin's skills
//! one directory deep — verified against the live roster: 92/92 top-level garden dirs listed,
//! 0/50 nested) and is refused for a Claude seat ([`SkillsError::NotInvocable`]); a
//! `portable: false` skill is Claude-only and is refused for every other CLI
//! ([`SkillsError::NotPortable`]). A Codex unit is therefore not refused because a Claude unit
//! elsewhere in the plan needs a non-portable skill, and vice versa. Never a silent proceed
//! without the required method.
//!
//! On the ACP path a CACHED session is admitted against ITS pinned snapshot before any ambient
//! resolution happens (v3.1 §4) — `acp_runner::exec_turn_inner` consults the session cache first
//! and hands what it pinned to [`admit_turn`], ONE policy for fresh and cached turns alike: the
//! pinned generation if there is one, else the ambient root through the ladder and the fence
//! check (codex round 4). The inherit-config escape hatch
//! (`execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV`) bypasses NONE of this (codex round 6): it
//! decides only whether the operator's ambient configuration is inherited IN ADDITION to the
//! snapshot — an invalid explicit snapshot is a launch error, a missing required skill a refusal
//! by name, and a template `--plugin-dir` is stripped, hatch or not.
//!
//! A TOOL-COMMAND unit spawns no worker, but the run-wide EXISTENCE admission still runs before
//! it ([`admit_plan`], codex round 6): a plan whose later agent unit names a skill the root lacks
//! is refused at its first unit whatever kind that unit is — a tool command that mutates state
//! before the missing skill is discovered is exactly the "work before refusal" the plan-wide set
//! exists to prevent.
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
/// There is no second input — crew resolves its `current` pointer itself (v3.1 §2).
pub(crate) const SKILLS_SNAPSHOT_ENV: &str = "WICKED_SKILLS_SNAPSHOT";

/// The plugin's name — the `<plugin>` half of the `wicked-garden:<dir>` identity Claude uses, the
/// marketplace + cache directory name, and the prefix every shipped skill's frontmatter `name`
/// carries.
pub(crate) const PLUGIN_NAME: &str = "wicked-garden";

/// The harness's plugin manifest, relative to a plugin root. Its presence is what makes a
/// directory a plugin root at all.
const PLUGIN_MANIFEST: &str = ".claude-plugin/plugin.json";

/// Crew's index of a published snapshot, relative to its root (crew's `SnapshotManifest`):
/// `{gen, contentHash, gardenSource, venv, skills: [{name, dir, kind, core, portable, nested,
/// mandates?}], views}`. A row's `dir` is PLUGIN-relative — `skills/<dir>`, the `skills/` prefix
/// literal (crew's row validator requires it); the loader strips it, keying the skill by the path
/// under `skills/` ([`SkillEntry::dir`]).
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
/// non-Claude CLI invokes it by); `dir` is its path under `skills/` (the index spells it
/// plugin-relative, `skills/<dir>`; the prefix is stripped at load), `/`-separated on every OS,
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

/// How a CLI receives skills for ONE launch (design v3.2 §2): a wicked-OWNED lever the engine
/// pulls at spawn — never a write into the user's own CLI directories (`~/.codex/skills`,
/// `~/.pi/agent/skills`, `~/.config/opencode/skills`, `~/.copilot`, `~/.claude/plugins`), which
/// the additive mirror of v3 would have been and which is withdrawn. Decided off the binary the
/// launch actually runs — the CLI itself on the wrapped path, the ACP BRIDGE on the ACP path: a
/// bridge that is a separate program (`pi-acp`, `codex-acp`) forwards no CLI flags, so it has no
/// lever even where the CLI does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillsLever {
    /// Claude: the plugin loader — `--plugin-dir <snapshot>` (wrapped) / `session/new`
    /// `plugins: [{type: "local", path}]` (ACP).
    ClaudePlugin,
    /// pi 0.84: `--no-skills` + one `--skill <snapshot>/skills/<dir>` per portable skill
    /// (`pi --help` lists both).
    PiSkillFlags,
    /// copilot 1.0: `--add-dir <snapshot>/views/copilot` — copilot loads `.github/skills` from an
    /// added directory (`copilot --help`); crew publishes that immutable view in the snapshot.
    CopilotAddDir,
    /// opencode 1.17: `OPENCODE_CONFIG_CONTENT` with `skills.paths` (verified in the installed
    /// 1.17.18: "Register skills from non-default locations via `skills.paths` (scanned
    /// recursively for `**/SKILL.md`)"), composed WITH the governance content the seat already
    /// injects.
    OpencodeConfig,
    /// No per-launch lever: codex 0.153 (`-c`/`--add-dir`/profiles load no skills), any ACP bridge
    /// that is not the CLI itself, any unknown binary. No lever ⇒ no skills, never a side channel
    /// (v3.2 §3): a unit that requires a skill on such a seat is refused by name.
    ///
    /// Documented residual (codex round 9, ADJUDICATED; follow-up core#400): "no skills" is what
    /// WICKED delivers — nothing. The seat still runs under the operator's own configuration
    /// directory (`~/.codex`), which v3.2 forbids the engine to touch, so whatever the operator
    /// installed there is theirs to see, exactly like the rest of their codex settings; isolating
    /// that ambient discovery needs an engine-minted `CODEX_HOME` worker home (auth relocation
    /// included), tracked in core#400.
    Absent,
}

impl SkillsLever {
    /// The lever for the binary a launch runs, by file stem (`claude`, `/usr/bin/pi`,
    /// `copilot.exe` …) — the same test `execute_wrapped::binary_is_claude` applies.
    pub(crate) fn for_binary(binary: &str) -> Self {
        match Path::new(binary)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
        {
            "claude" => SkillsLever::ClaudePlugin,
            "pi" => SkillsLever::PiSkillFlags,
            "copilot" => SkillsLever::CopilotAddDir,
            "opencode" => SkillsLever::OpencodeConfig,
            _ => SkillsLever::Absent,
        }
    }

    /// Does this lever hand the CLI ORIGINAL skill directories that the CLI then scans itself —
    /// pi's `--skill <dir>`, opencode's `skills.paths` (recursive `**/SKILL.md`)? Only such a
    /// lever carries a parent's nested children along with the parent, so only such a lever is
    /// judged by [`SkillsError::NestsNonPortable`] (codex round 4). Copilot is handed a PUBLISHED
    /// VIEW, judged as a whole tree on what it actually holds
    /// ([`SkillsSnapshot::copilot_view_for`]); Claude's plugin loader and an absent lever hand over
    /// no directory at all.
    pub(crate) fn delivers_directories(self) -> bool {
        matches!(
            self,
            SkillsLever::PiSkillFlags | SkillsLever::OpencodeConfig
        )
    }
}

/// The CLI a unit will run on, as admission needs to know it: Claude reaches skills through the
/// plugin loader (top-level dirs only, portable or not); every other CLI reaches only the
/// PORTABLE skills, through its own per-launch lever ([`SkillsLever`]) — or nothing at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerCli {
    Claude,
    Other { key: String, lever: SkillsLever },
}

impl WorkerCli {
    /// From the facts the runners already establish: `cli_binary` is the seat's CLI (the
    /// template's first token on the wrapped path, the seat record's `binary` on ACP — what
    /// `execute_wrapped::binary_is_claude` judges), `carrier_binary` is what this launch actually
    /// spawns (the same CLI when wrapped; the ACP bridge when not), and `cli_key` names the seat
    /// in a refusal.
    pub(crate) fn for_binaries(cli_binary: &str, carrier_binary: &str, cli_key: &str) -> Self {
        if crate::execute_wrapped::binary_is_claude(cli_binary) {
            return WorkerCli::Claude;
        }
        WorkerCli::Other {
            key: cli_key.to_string(),
            lever: SkillsLever::for_binary(carrier_binary),
        }
    }

    pub(crate) fn lever(&self) -> SkillsLever {
        match self {
            WorkerCli::Claude => SkillsLever::ClaudePlugin,
            WorkerCli::Other { lever, .. } => *lever,
        }
    }
}

impl std::fmt::Display for WorkerCli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerCli::Claude => f.write_str("claude"),
            WorkerCli::Other { key, .. } => f.write_str(key),
        }
    }
}

/// What ONE launch is handed, in the shape its lever takes (v3.2 §2) — computed from the admitted
/// snapshot and the seat by [`SkillsSnapshot::delivery`], consumed by the spawn paths. Every
/// variant names paths INSIDE the snapshot; nothing here is ever written anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SkillsDelivery {
    /// No lever, or no snapshot: the worker runs without wicked skills.
    None,
    /// The snapshot root, for Claude's plugin loader.
    ClaudePlugin(PathBuf),
    /// The portable skill directories, for pi's `--skill` flags (after `--no-skills`).
    PiSkillFlags(Vec<PathBuf>),
    /// `<snapshot>/views/copilot`, for copilot's `--add-dir`.
    CopilotAddDir(PathBuf),
    /// The portable skill directories, for opencode's `skills.paths`.
    OpencodeConfig(Vec<PathBuf>),
}

/// The env var opencode reads its whole configuration from (the seat's governance content
/// already rides it — `wicked_council::registry`); skills paths are COMPOSED into that same
/// value, never into `~/.config/opencode`.
pub(crate) const OPENCODE_CONFIG_ENV: &str = "OPENCODE_CONFIG_CONTENT";

impl SkillsDelivery {
    /// The argv flags this delivery rides, for the levers that are flags: pi's
    /// `--no-skills --skill <dir> …` (discovery OFF first, so the user's `~/.pi/agent/skills` is
    /// never a side channel), copilot's `--add-dir <view>`. Empty for the rest.
    pub(crate) fn argv_flags(&self) -> Vec<String> {
        match self {
            SkillsDelivery::PiSkillFlags(dirs) => std::iter::once("--no-skills".to_string())
                .chain(
                    dirs.iter()
                        .flat_map(|d| ["--skill".to_string(), d.to_string_lossy().into_owned()]),
                )
                .collect(),
            SkillsDelivery::CopilotAddDir(view) => {
                vec!["--add-dir".to_string(), view.to_string_lossy().into_owned()]
            }
            _ => Vec::new(),
        }
    }

    /// opencode's `OPENCODE_CONFIG_CONTENT`, composed: `existing` (the seat's governance content,
    /// or whatever the daemon's environment carries) with `skills.paths` set to this delivery's
    /// directories — existing paths kept, ours appended, duplicates dropped. `Ok(None)` for every
    /// other lever.
    ///
    /// FAILS CLOSED (codex round 3): an `existing` value that is not valid JSON, or not a JSON
    /// object — or whose `skills` is not an object, or whose `skills.paths` is not an array — is
    /// an `Err` naming the reason, which both carriers surface as a launch error
    /// ([`SkillsError::LeverConfig`]). Pass 2 replaced such a value with a bare document and
    /// logged a notice, which turned a configuration error into a launch with defaults: the
    /// governance content the seat depends on was dropped, and the contract requires composition
    /// WITH the existing content, never replacement.
    pub(crate) fn opencode_config(&self, existing: Option<&str>) -> Result<Option<String>, String> {
        let SkillsDelivery::OpencodeConfig(dirs) = self else {
            return Ok(None);
        };
        let mut doc = match existing {
            None => serde_json::json!({}),
            Some(text) => match serde_json::from_str::<Value>(text) {
                Ok(v) if v.is_object() => v,
                Ok(v) => {
                    return Err(format!(
                        "{OPENCODE_CONFIG_ENV} holds a JSON {}, not an object; the skills paths \
                         can only be composed into an object, and the seat's governance content \
                         is never replaced to make room for them",
                        json_kind(&v)
                    ))
                }
                Err(e) => {
                    return Err(format!(
                        "{OPENCODE_CONFIG_ENV} is not valid JSON ({e}); the seat's governance \
                         content must parse before the skills paths can be composed into it"
                    ))
                }
            },
        };
        let obj = doc.as_object_mut().expect("an object by construction");
        obj.entry("$schema")
            .or_insert_with(|| Value::String("https://opencode.ai/config.json".to_string()));
        let skills = obj.entry("skills").or_insert_with(|| serde_json::json!({}));
        let Some(skills) = skills.as_object_mut() else {
            return Err(format!(
                "{OPENCODE_CONFIG_ENV}.skills is not an object; the skills paths cannot be \
                 composed into it without discarding what it holds"
            ));
        };
        let paths = skills
            .entry("paths")
            .or_insert_with(|| serde_json::json!([]));
        let Some(list) = paths.as_array_mut() else {
            return Err(format!(
                "{OPENCODE_CONFIG_ENV}.skills.paths is not an array; the skills paths cannot be \
                 appended to it without discarding what it holds"
            ));
        };
        for d in dirs {
            let s = Value::String(d.to_string_lossy().into_owned());
            if !list.contains(&s) {
                list.push(s);
            }
        }
        Ok(Some(doc.to_string()))
    }
}

fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The baseline env's state crew records in `snapshot.json.venv` at publish (`SkillVenvState`):
/// `synced` ⇒ the generation carries the root `.venv` link into the recorded baseline's env;
/// `skipped` ⇒ nothing to provision, no link; `pending`/`failed` ⇒ no link either (a failed
/// provisioning is blocking on crew's side, so a published generation should not carry it, but
/// the engine reads whatever was written and binds the link to it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VenvState {
    Pending,
    Synced,
    Failed,
    Skipped,
}

impl VenvState {
    const SPELLINGS: &'static str = "pending|synced|failed|skipped";

    fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "synced" => Some(Self::Synced),
            "failed" => Some(Self::Failed),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }

    fn spelled(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Synced => "synced",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// Is `s` a sha256 content hash as crew spells one — exactly 64 lowercase hex digits?
fn is_content_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A resolved skills root: WHERE it is, where it came from, and WHAT it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillsSnapshot {
    /// The plugin root a worker is pointed at — the CANONICAL real path (absolute, every symlink
    /// resolved), so the worker and the engine name the same directory and a link flipped later
    /// cannot re-aim a pinned generation.
    pub root: PathBuf,
    pub source: SnapshotSource,
    /// The VERIFIED generation: the generation directory's name (`000007`, as crew publishes it
    /// and reaps by), which `snapshot.json.gen` was checked to equal at load. `None` for a
    /// fallback root, which has no index.
    pub gen: Option<String>,
    /// `snapshot.json`'s `contentHash` — required for a published snapshot; `None` for a
    /// fallback root.
    pub content_hash: Option<String>,
    /// The crew state home this generation was published under — DERIVED from the root's own
    /// shape (`<state home>/skills/snapshots/<gen>`, `state_home::derive`) and from nothing else.
    /// The worker Read fence over that directory is the registry (`execute_wrapped::deny_rules`).
    /// `None` for a fallback root, which has no state home (it sits in the claude config dir).
    pub state_home: Option<PathBuf>,
    /// `snapshot.json.gardenSource.baseline` — the sha256 content hash of the bundle this
    /// generation was published from, which names the ONE baseline env its `.venv` link may reach
    /// (`<state home>/skills/baseline/<baseline>/.venv`; review pass 11 — crew's `verifyCurrent`
    /// binds the link to the recorded hash, never to any hash that happens to exist). 64 lowercase
    /// hex digits, validated at load; `None` for a fallback root.
    pub baseline: Option<String>,
    /// `snapshot.json.venv` — whether crew provisioned the baseline env for this generation: the
    /// `.venv` link may exist only when it is `synced`, and a `synced` generation must carry it.
    /// `None` for a fallback root.
    pub venv: Option<VenvState>,
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
    /// The entry a `skill_ref` names — by frontmatter `name` ONLY (design v3 §5; codex round 6).
    /// A published index whose `name` diverges from the path-derived name (`wicked-garden-` + the
    /// dir path joined by `-`, [`derived_name`]) is a config error at load, never an alias here:
    /// rounds 1–5 fell back to the derived name, which gave one skill two identities.
    pub(crate) fn skill(&self, skill_ref: &str) -> Option<&SkillEntry> {
        self.skills.iter().find(|s| s.name == skill_ref)
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

    /// The skills a non-Claude lever may DELIVER as directories, sorted by `dir`: every PORTABLE
    /// skill (v3 §5: a `portable: false` skill leans on `${CLAUDE_PLUGIN_ROOT}` or sibling links
    /// and is Claude-only) EXCEPT one whose directory NESTS a non-portable skill. The directory
    /// levers hand whole directories — opencode scans each `skills.paths` entry recursively for
    /// `**/SKILL.md` (verified in 1.17.18), and pi's `--skill <dir>` semantics are not pinned
    /// either way — so a portable parent containing a non-portable child cannot be delivered
    /// without exposing the child (codex round 3). Such a parent is undeliverable to every
    /// non-Claude seat (admission refuses a unit that invokes it, [`SkillsError::NestsNonPortable`]);
    /// its PORTABLE descendants are still delivered on their own paths.
    pub(crate) fn deliverable_portable(&self) -> Vec<&SkillEntry> {
        let mut out: Vec<&SkillEntry> = self
            .skills
            .iter()
            .filter(|s| s.portable && self.nonportable_nested(s).is_empty())
            .collect();
        out.sort_by(|a, b| a.dir.cmp(&b.dir));
        out
    }

    /// The `portable: false` skills nested strictly BELOW `entry`'s directory, sorted by `dir`.
    pub(crate) fn nonportable_nested(&self, entry: &SkillEntry) -> Vec<&SkillEntry> {
        let prefix = format!("{}/", entry.dir);
        let mut out: Vec<&SkillEntry> = self
            .skills
            .iter()
            .filter(|s| !s.portable && s.dir.starts_with(prefix.as_str()))
            .collect();
        out.sort_by(|a, b| a.dir.cmp(&b.dir));
        out
    }

    /// The directory of every deliverable portable skill ([`deliverable_portable`]
    /// (Self::deliverable_portable)) — `<root>/skills/<dir>`, joined component-wise.
    pub(crate) fn portable_skill_dirs(&self) -> Vec<PathBuf> {
        self.deliverable_portable()
            .into_iter()
            .map(|s| self.skill_path(&s.dir))
            .collect()
    }

    fn skill_path(&self, dir: &str) -> PathBuf {
        dir.split('/')
            .fold(self.root.join(SKILLS_DIR), |p, c| p.join(c))
    }

    /// The copilot view crew publishes in a snapshot — `<root>/views/copilot`, holding
    /// `.github/skills/<name>/` copies of the enabled portable skills (v3.2 §4) — when this
    /// generation carries one as a REAL directory reached through real directories: `views` and
    /// `views/copilot` are both lstat-checked, and a symlink at either is NOT a view (admission
    /// refuses it as a containment defect, [`copilot_view_for`](Self::copilot_view_for); here it
    /// is simply nothing to hand). `None` ⇒ this generation gives copilot nothing to load.
    pub(crate) fn copilot_view_dir(&self) -> Option<PathBuf> {
        let views = self.root.join("views");
        let view = views.join("copilot");
        for p in [&views, &view] {
            let m = std::fs::symlink_metadata(p).ok()?;
            if m.file_type().is_symlink() || !m.is_dir() {
                return None;
            }
        }
        Some(view)
    }

    /// The copilot view VERIFIED as a WHOLE tree (design v3.3 §2; codex rounds 3, 4 and 5). The
    /// launch hands the ENTIRE `views/copilot` to copilot through `--add-dir`, so admission judges
    /// everything that directory holds, not only what the seat invokes — round 3 checked the
    /// required skills' entries and left an extra non-portable copy, an unindexed entry or a
    /// symlinked one unexamined (and, for a unit invoking nothing, examined no entry at all);
    /// round 4 started the enumeration at `.github/skills`, leaving a sibling ANYWHERE above it
    /// (`views/copilot/leak -> …`, `.github/copilot-instructions.md`) uninspected though delivered.
    ///
    /// The enumeration starts at `views/copilot` ITSELF (round 5): every entry of the delivered
    /// directory must be expected. `views/copilot` may hold exactly `.github` (a real directory);
    /// `.github` may hold exactly `skills` (a real directory); `.github/skills/` holds one
    /// directory per indexed PORTABLE skill, each a safe single path segment ([`safe_segments`])
    /// with a `SKILL.md` whose frontmatter `name` equals the entry's, and every file below each
    /// entry is inspected — a nested `SKILL.md` deeper inside must itself name an indexed portable
    /// skill (the Claude-only child a directory lever would leak is, for copilot, judged on the
    /// view crew published: present in the copy ⇒ refused by name; excluded ⇒ the parent is
    /// admitted). No symlink anywhere in the view — a link would hand the worker an external
    /// tree — and any OTHER file, directory or link at any level refuses the launch naming it.
    /// Every joined path is checked for canonical containment in the view before it is read
    /// ([`contained_under`], v3.3 §3). Enumeration errors PROPAGATE (an unlistable or
    /// uninspectable entry is a defect, never "nothing here").
    ///
    /// `Ok(None)` when the generation publishes no view at all (`views` or `views/copilot`
    /// absent — the caller decides whether that matters); `Err(Config)` naming the entry for any
    /// unexpected content — a symlink, a stray file, an unindexed or non-portable entry, a name
    /// mismatch, a leaked nested child — whether or not the unit invokes anything; `Err(Missing)`
    /// naming the required skills a well-formed view does not hold — an EMPTY view (`.github` or
    /// `.github/skills` absent) is missing every one of them.
    pub(crate) fn copilot_view_for(
        &self,
        required: &[&SkillEntry],
    ) -> Result<Option<PathBuf>, SkillsError> {
        let config = |why: String| SkillsError::Config {
            var: SKILLS_SNAPSHOT_ENV,
            path: self.root.clone(),
            why,
        };
        let views = self.root.join("views");
        let view = views.join("copilot");
        // Absent ⇒ no view published; present ⇒ a real directory, or a containment defect.
        for (p, rel) in [(&views, "views"), (&view, "views/copilot")] {
            match std::fs::symlink_metadata(p) {
                Err(_) => return Ok(None),
                Ok(m) if m.file_type().is_symlink() => return Err(config(linked(rel))),
                Ok(m) if !m.is_dir() => return Err(config(format!("{rel} is not a directory"))),
                Ok(_) => {}
            }
        }
        const VIEW_REL: &str = "views/copilot";
        const GITHUB_REL: &str = "views/copilot/.github";
        const SKILLS_REL: &str = "views/copilot/.github/skills";
        // The two fixed levels of the view, enumerated from the delivered directory DOWN: each
        // may hold nothing (an EMPTY view — every required skill is missing) or exactly the one
        // expected real directory; anything else at either level refuses the launch by name.
        let github = view.join(".github");
        let skills_dir = github.join("skills");
        let empty = !only_entry(&view, &view, VIEW_REL, ".github").map_err(config)?
            || !only_entry(&view, &github, GITHUB_REL, "skills").map_err(config)?;
        let mut held: BTreeSet<String> = BTreeSet::new();
        if !empty {
            let names = list_sorted(&skills_dir)
                .map_err(|e| config(format!("{SKILLS_REL} cannot be listed ({e})")))?;
            for name in names {
                let rel_dir = format!("{SKILLS_REL}/{name}");
                // Hygiene BEFORE the join (v3.3 §3): a listed name is a real file name, but the
                // spelling is still required to be a plain single segment before it is joined.
                safe_segments(&name, false, "view entry")
                    .map_err(|why| config(format!("{SKILLS_REL}: {why}")))?;
                let dir = skills_dir.join(&name);
                match std::fs::symlink_metadata(&dir) {
                    Ok(m) if m.file_type().is_symlink() => return Err(config(linked(&rel_dir))),
                    Ok(m) if m.is_dir() => {}
                    Ok(_) => {
                        return Err(config(format!(
                            "{rel_dir} is not a directory — the copilot view holds only skill \
                             directories, one per enabled portable skill"
                        )))
                    }
                    Err(e) => return Err(config(format!("{rel_dir} cannot be inspected ({e})"))),
                }
                contained_under(&view, &dir, &rel_dir).map_err(config)?;
                let Some(entry) = self.skills.iter().find(|s| s.name == name) else {
                    return Err(config(format!(
                        "{rel_dir} is not a skill this snapshot indexes — the copilot view may \
                         hold only the enabled portable skills"
                    )));
                };
                if !entry.portable {
                    return Err(config(format!(
                        "{rel_dir} is `{name}`, which the index marks portable: false \
                         (Claude-only); the copilot view may hold only portable skills"
                    )));
                }
                let file = dir.join(SKILL_FILE);
                let rel_file = format!("{rel_dir}/{SKILL_FILE}");
                match std::fs::symlink_metadata(&file) {
                    Ok(m) if m.file_type().is_symlink() => return Err(config(linked(&rel_file))),
                    Ok(m) if m.is_file() => {}
                    Ok(_) => return Err(config(format!("{rel_file} is not a regular file"))),
                    Err(e) => return Err(config(format!("{rel_file} is missing ({e})"))),
                }
                contained_under(&view, &file, &rel_file).map_err(config)?;
                match view_skill_name(&file) {
                    Some(n) if n == name => {}
                    Some(n) => {
                        return Err(config(format!(
                            "{rel_file} declares name `{n}`, not `{name}` — the entry is not the \
                             skill its directory says it is"
                        )))
                    }
                    None => {
                        return Err(config(format!(
                            "{rel_file} has no parseable frontmatter `name` to match `{name}` \
                             against"
                        )))
                    }
                }
                self.walk_view_entry(&view, &dir, &rel_dir, true)
                    .map_err(config)?;
                held.insert(name);
            }
        }
        let mut missing: Vec<String> = required
            .iter()
            .filter(|e| !held.contains(&e.name))
            .map(|e| e.name.clone())
            .collect();
        missing.sort();
        missing.dedup();
        if !missing.is_empty() {
            return Err(SkillsError::Missing {
                root: Some(view),
                missing,
            });
        }
        Ok(Some(view))
    }

    /// Everything below one validated view entry, recursively (v3.3 §2): no symlink anywhere
    /// (the launch `--add-dir`s the whole view), every path canonically contained in the view,
    /// and a NESTED `SKILL.md` — any but the entry's own top-level one (`top`) — must name an
    /// indexed PORTABLE skill: a `portable: false` child copied into the view, or a skill the
    /// index does not know, refuses the launch by path.
    fn walk_view_entry(&self, view: &Path, dir: &Path, rel: &str, top: bool) -> Result<(), String> {
        for name in list_sorted(dir).map_err(|e| format!("{rel} cannot be listed ({e})"))? {
            let child = dir.join(&name);
            let child_rel = format!("{rel}/{name}");
            let m = std::fs::symlink_metadata(&child)
                .map_err(|e| format!("{child_rel} cannot be inspected ({e})"))?;
            if m.file_type().is_symlink() {
                return Err(linked(&child_rel));
            }
            contained_under(view, &child, &child_rel)?;
            if m.is_dir() {
                self.walk_view_entry(view, &child, &child_rel, false)?;
            } else if name == SKILL_FILE && !top {
                let held = view_skill_name(&child);
                match held
                    .as_deref()
                    .and_then(|n| self.skills.iter().find(|s| s.name == n))
                {
                    Some(e) if e.portable => {}
                    Some(e) => {
                        return Err(format!(
                            "{child_rel} is `{}`, which the index marks portable: false — a \
                             Claude-only skill nested inside a copilot view entry; publish the \
                             view without it",
                            e.name
                        ))
                    }
                    None => {
                        return Err(format!(
                            "{child_rel} is a nested {SKILL_FILE} that names no indexed skill \
                             ({}) — the copilot view holds only enabled portable skills",
                            held.as_deref().unwrap_or("no parseable frontmatter `name`")
                        ))
                    }
                }
            }
        }
        Ok(())
    }

    /// What a launch on `cli` is handed from this root, in its lever's shape (v3.2 §2). A
    /// LIVE-CACHE fallback root hands a non-Claude seat NOTHING (codex round 7): its portability
    /// flags are an approximation with no publish-time verdict behind them, so only Claude's
    /// plugin loader — which does not depend on portability — is handed the root.
    pub(crate) fn delivery(&self, cli: &WorkerCli) -> SkillsDelivery {
        if self.source == SnapshotSource::LiveCache && !matches!(cli, WorkerCli::Claude) {
            return SkillsDelivery::None;
        }
        match cli.lever() {
            SkillsLever::ClaudePlugin => SkillsDelivery::ClaudePlugin(self.root.clone()),
            SkillsLever::PiSkillFlags => SkillsDelivery::PiSkillFlags(self.portable_skill_dirs()),
            SkillsLever::CopilotAddDir => match self.copilot_view_dir() {
                Some(view) => SkillsDelivery::CopilotAddDir(view),
                None => SkillsDelivery::None,
            },
            SkillsLever::OpencodeConfig => {
                SkillsDelivery::OpencodeConfig(self.portable_skill_dirs())
            }
            SkillsLever::Absent => SkillsDelivery::None,
        }
    }

    /// Exact INDEX/TREE PARITY over the whole delivered closure of a PUBLISHED generation (codex
    /// round 7). The index verification ([`load_published`]) proves every indexed entry exists as
    /// stated; this walk proves the converse and the containment of everything else the worker is
    /// handed: the root, `.claude-plugin/**`, `skills/**` and every support file are lstat-walked
    /// (sorted, errors propagate) — (1) every `SKILL.md` found under `skills/` must be INDEXED (a
    /// disabled or unpublished skill copied into the generation, or a nested extra a directory
    /// lever would deliver, is refused by path); (2) `views/` may hold only `copilot`, whose whole
    /// tree is then judged by [`copilot_view_for`](Self::copilot_view_for) (indexed portable
    /// skills only, no links, no strays); (3) NO symlink anywhere in the generation — the one
    /// exception is crew's root-level `.venv` link, accepted only when `snapshot.json.venv` is
    /// `synced` and it resolves inside canonical `<state home>/skills/baseline/<recorded
    /// baseline>/.venv` — the `gardenSource.baseline` the metadata records (crew's `verifyCurrent`
    /// rule for the one link a generation may carry; review pass 11), and a `synced` generation
    /// must carry it. Everything else at the root (crew's support closure — `scripts/`,
    /// `schemas/`, `docs/`, `pyproject.toml`, `uv.lock` …) is not enumerated against a list
    /// (crew's bundle is crew's), only contained.
    pub(crate) fn verify_delivered_tree(&self) -> Result<(), SkillsError> {
        let config = |why: String| SkillsError::Config {
            var: SKILLS_SNAPSHOT_ENV,
            path: self.root.clone(),
            why,
        };
        let mut found: BTreeSet<String> = BTreeSet::new();
        self.walk_delivered(&self.root, "", &mut found)
            .map_err(config)?;
        // (review pass 11) A `synced` generation WITHOUT the `.venv` link is not what publish
        // wrote either (crew's `verifyCurrent`): the walk above judged the link when present; its
        // absence is judged here, by lstat of the one root name it may have.
        if self.venv == Some(VenvState::Synced) {
            let is_link = std::fs::symlink_metadata(self.root.join(".venv"))
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            if !is_link {
                return Err(config(
                    "snapshot.json records the baseline env as synced but the generation has no \
                     .venv link — a synced generation carries crew's root-level .venv into its \
                     recorded baseline env"
                        .to_string(),
                ));
            }
        }
        let indexed: BTreeSet<&str> = self.skills.iter().map(|s| s.dir.as_str()).collect();
        let unindexed: Vec<String> = found
            .iter()
            .filter(|d| !indexed.contains(d.as_str()))
            .map(|d| format!("{SKILLS_DIR}/{d}/{SKILL_FILE}"))
            .collect();
        if !unindexed.is_empty() {
            return Err(config(format!(
                "the generation delivers skills its index does not carry: {} — a disabled or \
                 unpublished skill must not ride in a published generation (Claude's loader and \
                 the directory levers would load it); the index and the tree must agree exactly",
                unindexed.join(", ")
            )));
        }
        // The copilot view, judged as a whole tree (v3.3 §2) whether or not any seat invokes it.
        self.copilot_view_for(&[]).map(|_| ())
    }

    /// The lstat walk behind [`verify_delivered_tree`](Self::verify_delivered_tree): records the
    /// dir of every `skills/**/SKILL.md`, refuses any symlink but the root `.venv`, refuses any
    /// `views/` child but `copilot` (whose tree is judged separately), propagates every
    /// enumeration error.
    fn walk_delivered(
        &self,
        dir: &Path,
        rel: &str,
        found: &mut BTreeSet<String>,
    ) -> Result<(), String> {
        let here = if rel.is_empty() { "the root" } else { rel };
        for name in list_sorted(dir).map_err(|e| format!("{here} cannot be listed ({e})"))? {
            let child = dir.join(&name);
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let meta = std::fs::symlink_metadata(&child)
                .map_err(|e| format!("{child_rel} cannot be inspected ({e})"))?;
            if meta.file_type().is_symlink() {
                if rel.is_empty() && name == ".venv" {
                    // (review pass 11) The link may exist only when snapshot.json says an env was
                    // provisioned (`venv: synced`) — crew's `verifyCurrent` rule; a live-cache
                    // root records nothing and is refused below for having no state home.
                    if let Some(state) = self.venv.filter(|s| *s != VenvState::Synced) {
                        let target = std::fs::read_link(&child)
                            .map(|t| t.display().to_string())
                            .unwrap_or_default();
                        return Err(format!(
                            ".venv -> `{target}` is present although snapshot.json records the \
                             env as `{}` — only a `synced` generation carries the baseline env \
                             link",
                            state.spelled()
                        ));
                    }
                    self.check_venv_link(&child)?;
                    continue;
                }
                return Err(format!(
                    "{child_rel} is a symlink — the delivered generation must be contained: no \
                     link anywhere in it but crew's root-level .venv into the baseline env"
                ));
            }
            if rel.is_empty() && name == "views" && meta.is_dir() {
                for view in
                    list_sorted(&child).map_err(|e| format!("views cannot be listed ({e})"))?
                {
                    if view != "copilot" {
                        return Err(format!(
                            "views/{view} is not a delivery view this engine knows — only \
                             views/copilot is published and delivered"
                        ));
                    }
                }
                continue; // judged whole by `copilot_view_for`
            }
            if meta.is_dir() {
                self.walk_delivered(&child, &child_rel, found)?;
                continue;
            }
            if name == SKILL_FILE {
                if let Some(under) = child_rel.strip_prefix(&format!("{SKILLS_DIR}/")) {
                    match under.strip_suffix(&format!("/{SKILL_FILE}")) {
                        Some(skill_dir) if !skill_dir.is_empty() => {
                            found.insert(skill_dir.to_string());
                        }
                        _ => {
                            return Err(format!(
                                "{child_rel} is a {SKILL_FILE} at the skills root itself — no \
                                 skill directory holds it, so no index entry can name it"
                            ))
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The one link a generation may carry (crew's rule, mirrored): root-level `.venv`, resolving
    /// INSIDE canonical `<state home>/skills/baseline/<64-hex>/.venv` — the shared, read-only
    /// baseline env crew provisions once per bundle hash — where the `<64-hex>` IS the baseline
    /// `snapshot.json.gardenSource.baseline` records (review pass 11). Anything else it points at
    /// is refused; the caller has already required `venv: synced`.
    fn check_venv_link(&self, link: &Path) -> Result<(), String> {
        let Some(state_home) = &self.state_home else {
            return Err(
                ".venv is a symlink but this root has no state home whose baseline env it could \
                 point into"
                    .to_string(),
            );
        };
        // (codex round 8) Every component of `<state home>/skills/baseline/<hash>/.venv` is
        // lstat-checked to be a REAL directory — never `canonicalize` and trust: a linked
        // `skills/` or `baseline/` would be walked through. The state home itself is canonical
        // (derived from the canonical root).
        let real_dir = |p: &Path, rel: &str| -> Result<(), String> {
            match std::fs::symlink_metadata(p) {
                Ok(m) if m.file_type().is_symlink() => Err(format!(
                    ".venv cannot be admitted: `<state home>/{rel}` is a symlink — every \
                     component of the path to the baseline env must be a real directory"
                )),
                Ok(m) if !m.is_dir() => Err(format!(
                    ".venv cannot be admitted: `<state home>/{rel}` is not a directory"
                )),
                Ok(_) => Ok(()),
                Err(e) => Err(format!(
                    ".venv is a symlink but the state home has no baseline env root at \
                     `<state home>/{rel}` ({e})"
                )),
            }
        };
        let skills = state_home.join(SKILLS_DIR);
        let baseline = skills.join("baseline");
        real_dir(&skills, SKILLS_DIR)?;
        real_dir(&baseline, &format!("{SKILLS_DIR}/baseline"))?;
        // The link's own target, resolved LEXICALLY against the snapshot root (crew writes
        // `../../baseline/<hash>/.venv`, or the absolute real path on Windows) — never by
        // following it — must END exactly at `<baseline>/<64-hex>/.venv`: no component beyond.
        let target = std::fs::read_link(link)
            .map_err(|e| format!(".venv is a symlink whose target cannot be read ({e})"))?;
        let joined = if target.is_absolute() {
            target.clone()
        } else {
            link.parent()
                .map(|p| p.join(&target))
                .unwrap_or(target.clone())
        };
        // (review pass 12) The absolute target crew writes for a Windows junction is compared
        // against a `baseline` spelled WITHOUT the verbatim prefix, so the target drops it too —
        // one normalization on both sides of every prefix check.
        let normalized = lexical_normalize(&simplify_verbatim(joined));
        let outside = || {
            format!(
                ".venv is a symlink to `{}` (`{}`), which is not exactly `{}/<64-hex>/.venv` — the \
                 one link a generation may carry ends at the baseline env crew provisioned for its \
                 bundle hash, with no component beyond it",
                target.display(),
                normalized.display(),
                baseline.display()
            )
        };
        let rest = normalized.strip_prefix(&baseline).map_err(|_| outside())?;
        let parts: Vec<&str> = rest
            .components()
            .map(|c| match c {
                std::path::Component::Normal(s) => s.to_str().unwrap_or(""),
                _ => "",
            })
            .collect();
        let [hash, venv] = parts.as_slice() else {
            return Err(outside());
        };
        if !is_content_hash(hash) || *venv != ".venv" {
            return Err(outside());
        }
        // (review pass 11) BOUND to the metadata: the env the link reaches must be the one
        // `gardenSource.baseline` records — never any baseline that happens to exist (crew's
        // `verifyCurrent`: an altered link into a sibling bundle's env would otherwise verify).
        if self.baseline.as_deref() != Some(*hash) {
            return Err(format!(
                ".venv is a symlink to `{}`, whose baseline env `{hash}` is not the baseline \
                 snapshot.json records (`{}`); the link may only reach the env crew provisioned \
                 for this generation's own bundle",
                target.display(),
                self.baseline.as_deref().unwrap_or("none recorded")
            ));
        }
        real_dir(
            &baseline.join(hash),
            &format!("{SKILLS_DIR}/baseline/{hash}"),
        )?;
        real_dir(
            &baseline.join(hash).join(".venv"),
            &format!("{SKILLS_DIR}/baseline/{hash}/.venv"),
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn skills(&self) -> &[SkillEntry] {
        &self.skills
    }
}

/// Lexical normalization of a path: `.` dropped, `..` pops the previous normal component (a `..`
/// with nothing to pop is kept, so the result cannot pretend to be under a prefix it left),
/// prefixes and the root kept. Never touches the filesystem — the containment checks that use it
/// (`check_venv_link`) lstat the components themselves.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                let popped = matches!(
                    out.components().next_back(),
                    Some(std::path::Component::Normal(_))
                ) && out.pop();
                if !popped {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
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

/// `engineering/frontend` → `wicked-garden-engineering-frontend`: the name a skill at
/// `skills/<dir>` MUST declare (crew's `derivedSkillName`, checked at every publish); the loader
/// re-checks it at load ([`load_published`]) so a skill is keyed by exactly one identity.
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
    /// A non-Claude seat was asked for a PORTABLE skill whose directory NESTS a non-portable one
    /// — `(parent name, nested dir)` pairs. The directory levers deliver whole directories
    /// (opencode scans them recursively), so the parent cannot be handed over without exposing
    /// the child (v3 §5, codex round 3).
    NestsNonPortable {
        root: PathBuf,
        cli: String,
        skills: Vec<(String, String)>,
    },
    /// A seat's per-launch lever cannot be pulled because the configuration it composes into is
    /// malformed — opencode's `OPENCODE_CONFIG_CONTENT` that is not a JSON object. A config error
    /// surfaced on both carriers; never a launch with defaults (codex round 3).
    LeverConfig {
        cli: String,
        var: &'static str,
        why: String,
    },
    /// A seat whose launch has NO wicked-owned way to deliver skills (v3.2 §3) — codex, an ACP
    /// bridge that forwards no flags, copilot when the generation publishes no `views/copilot` —
    /// was asked for skills. No lever ⇒ no skills, never a side channel through the user's own
    /// CLI directories.
    NoLever {
        cli: String,
        skills: Vec<String>,
        why: String,
    },
    /// A CACHED ACP session that was opened WITHOUT a snapshot (nothing pinned — no root on the
    /// ladder at the time) was asked, on a later turn, for skills. The plugin is handed at
    /// `session/new` and never afterwards, so the session cannot be given what a NOW-available
    /// root holds; the turn is refused naming the skills, and the fix is a fresh session (codex
    /// round 5 — resolving the ambient configuration here would admit a session against a plugin
    /// it never loaded and generate a directive for a skill it cannot invoke).
    NotDelivered { cli: String, skills: Vec<String> },
    /// The live-cache FALLBACK root (no snapshot published) is Claude-only (codex round 7): its
    /// index carries no publish-time portability verdict — cwd-relative script detection is crew's
    /// publish analyzer and is not re-implemented here — so no non-Claude seat is handed anything
    /// from it, and a non-Claude unit that invokes a skill is refused naming the seat.
    FallbackClaudeOnly {
        root: PathBuf,
        cli: String,
        skills: Vec<String>,
    },
    /// The ladder yielded NO root because the live-cache discovery FAILED (codex round 7) — the
    /// cache could not be listed, a version candidate was a symlink or could not be resolved, the
    /// highest version's manifest was malformed — as opposed to a genuine absence. A failure takes
    /// the no-root rung like an absence but is never silent: this refusal carries the reason for a
    /// run that names a skill.
    FallbackFailed { why: String, missing: Vec<String> },
    /// A run that invokes a skill was routed to a CARRIER that does not load the skills snapshot
    /// (codex round 8; ADJUDICATED): the persistent PTY session runner opens the raw CLI with no
    /// snapshot resolution, admission, isolation or delivery lever, so a skill directive there
    /// would tell the worker to invoke a skill nothing loaded. Refused by name; no directive is
    /// ever emitted on that carrier. PLAN-WIDE (codex round 9): every unit carries the run's
    /// whole skill set (`StepInput::required_skills`), so the refusal lands at the run's FIRST
    /// unit — a skill-free first unit does no work ahead of a later unit the carrier cannot serve.
    CarrierWithoutSkills {
        carrier: String,
        skills: Vec<String>,
    },
    /// The roster holds NO seat that can be handed what a unit invokes (core#401): a unit whose
    /// skill is `portable: false` in the handed snapshot — or whose root is the Claude-only
    /// live-cache fallback — needs a `required_seat` (today: claude), and the roster has none.
    /// Refused at DISTRIBUTION, before the first unit does any work, naming the skills, why they
    /// need that seat kind and the roster that lacks it — never a council pick the ladder then
    /// refuses by name mid-run, with an escalation gate that cannot retarget the seat.
    NoEligibleSeat {
        ord: u32,
        skills: Vec<String>,
        required_seat: &'static str,
        roster: Vec<String>,
        why: String,
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
                         skills source and every skill it holds is keyed `{PLUGIN_NAME}-<dir>` \
                         (its frontmatter name, which crew requires to equal the path-derived \
                         name at publish), so a ref of another family can never resolve in it — \
                         fix the workflow's skill_ref, or add the skill to the effective root \
                         under the catalog's naming convention and publish before a run names it",
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
                "no skills root is available ({SKILLS_SNAPSHOT_ENV} unset and no installed \
                 {PLUGIN_NAME} found under the claude config dir) but this run requires skills: \
                 {}; install {PLUGIN_NAME} or publish a snapshot",
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
            SkillsError::NestsNonPortable { root, cli, skills } => write!(
                f,
                "the skills snapshot at {} holds {} as a directory that nests a non-portable skill \
                 ({}); '{cli}' is handed skills by DIRECTORY, scanned recursively, so the parent \
                 cannot be delivered without exposing the Claude-only child — route this unit to \
                 a claude seat, or publish the child as portable or outside the parent's directory",
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
            SkillsError::LeverConfig { cli, var, why } => write!(
                f,
                "'{cli}' cannot be handed its skills: {why}; the skills paths are composed INTO \
                 {var}, never in place of it, so fix the value (the seat's [cli.acp] \
                 acp_governance_env, or the daemon's environment) — the unit is not launched with \
                 defaults"
            ),
            SkillsError::NoLever { cli, skills, why } => write!(
                f,
                "'{cli}' cannot be handed skills on this launch ({why}) but the unit requires {}; \
                 wicked delivers skills only through a per-launch lever it owns — never through a \
                 CLI's own skills directory — so route this unit to a seat with one (claude, pi, \
                 copilot with a published copilot view, opencode) or drop its skill_ref",
                skills.join(", ")
            ),
            SkillsError::CarrierWithoutSkills { carrier, skills } => write!(
                f,
                "{carrier} sessions do not load the skills snapshot, but this run requires {} \
                 (every skill any of its units names — the refusal is plan-wide, at the run's \
                 first unit); run skill-bearing units on the wrapped or ACP carrier (which \
                 resolve, admit and hand the snapshot) — no invocation directive is emitted on a \
                 carrier that cannot load the skill",
                skills.join(", ")
            ),
            SkillsError::NotDelivered { cli, skills } => write!(
                f,
                "the cached ACP session for '{cli}' was opened without a skills snapshot (none was \
                 available when it started), so no plugin was ever handed to it, but this unit \
                 requires {}; a snapshot reaches a session only at session/new, never mid-run — \
                 start a fresh session (a new run, or restart this one) now that a root is \
                 available, rather than invoking a skill the session never loaded",
                skills.join(", ")
            ),
            SkillsError::FallbackClaudeOnly { root, cli, skills } => write!(
                f,
                "the {} at {} is this launch's skills root ({SKILLS_SNAPSHOT_ENV} is unset, so the \
                 ladder fell back to the installed {PLUGIN_NAME}) and it carries no publish-time \
                 portability verdict — cwd-relative script detection is crew's publish analyzer, \
                 not re-implemented by the engine — so it is Claude-only: '{cli}' cannot be handed \
                 {}; non-Claude delivery requires a published snapshot (publish one), or route this \
                 unit to a claude seat",
                SnapshotSource::LiveCache,
                root.display(),
                skills.join(", ")
            ),
            SkillsError::FallbackFailed { why, missing } => write!(
                f,
                "no skills root is available: {SKILLS_SNAPSHOT_ENV} is unset and the installed \
                 {PLUGIN_NAME} could not be used as the fallback ({why}) — not merely absent — but \
                 this run requires skills: {}; repair the installation named in the reason, or \
                 publish a snapshot",
                missing.join(", ")
            ),
            SkillsError::NoEligibleSeat {
                ord,
                skills,
                required_seat,
                roster,
                why,
            } => write!(
                f,
                "unit {ord} requires {}, which only a {required_seat} seat can be handed ({why}), \
                 and the roster [{}] holds no seat that resolves to {required_seat} on both \
                 carriers (the merged registry record for the key and the launch template decide, \
                 not the key's spelling); add a {required_seat} seat to the roster, or publish the \
                 skill as portable / drop the unit's skill_ref — refused at plan time, before any \
                 unit ran",
                skills.join(", "),
                roster.join(", ")
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
#[cfg(test)]
pub(crate) fn resolve() -> Result<Option<SkillsSnapshot>, SkillsError> {
    resolve_ladder().map(Ladder::root)
}

/// What the ladder yielded (codex round 7): a ROOT; a genuine ABSENCE — no explicit input and no
/// installed plugin, the documented no-root rung; or a FAILURE of the fallback discovery — a root
/// may well be installed but could not be listed, resolved or trusted — which takes the no-root
/// rung too, with its reason kept so the refusal of a skill-naming run says why
/// ([`SkillsError::FallbackFailed`]). Never a failure flattened into "no installed garden".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ladder {
    Root(SkillsSnapshot),
    Absent,
    Failed(String),
}

impl Ladder {
    /// The root, when the ladder found one (absence and failure alike are `None` — production
    /// callers match the variants; the `Option` view serves the tests' ladder assertions).
    #[cfg(test)]
    pub(crate) fn root(self) -> Option<SkillsSnapshot> {
        match self {
            Ladder::Root(s) => Some(s),
            Ladder::Absent | Ladder::Failed(_) => None,
        }
    }
}

/// The ladder from the process environment, absence and failure told apart. Exactly ONE skills
/// input (v3.1 §2, v3.4 §2): the snapshot path; the state home is derived from it and no
/// companion variable is read.
pub(crate) fn resolve_ladder() -> Result<Ladder, SkillsError> {
    let explicit = env_path(SKILLS_SNAPSHOT_ENV)?;
    resolve_ladder_in(
        explicit,
        std::env::var_os(crate::acp_runner::CLAUDE_CONFIG_DIR_ENV).map(PathBuf::from),
        home_dir(),
        &mut |line| eprintln!("{line}"),
    )
}

/// [`resolve`] with its inputs and its log sink explicit, so the ladder is testable without
/// touching the process environment and the "logged" half of each step is asserted, not assumed.
#[cfg(test)]
pub(crate) fn resolve_in(
    explicit: Option<PathBuf>,
    claude_config_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    log: &mut dyn FnMut(String),
) -> Result<Option<SkillsSnapshot>, SkillsError> {
    resolve_ladder_in(explicit, claude_config_dir, home, log).map(Ladder::root)
}

/// [`resolve_ladder`] with its inputs and its log sink explicit.
pub(crate) fn resolve_ladder_in(
    explicit: Option<PathBuf>,
    claude_config_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    log: &mut dyn FnMut(String),
) -> Result<Ladder, SkillsError> {
    if let Some(path) = explicit {
        return load_published(SKILLS_SNAPSHOT_ENV, &path).map(Ladder::Root);
    }
    let Some(config) = claude_config_dir.or_else(|| home.map(|h| h.join(".claude"))) else {
        log(format!(
            "[wicked-core] skills.fallback {SKILLS_SNAPSHOT_ENV} is unset and neither \
             CLAUDE_CONFIG_DIR nor a home directory resolves; workers run WITHOUT {PLUGIN_NAME} \
             skills"
        ));
        return Ok(Ladder::Absent);
    };
    // The live marketplace cache and nothing else: `<config>/plugins/wicked-garden` (the operator's
    // hand copy) is NOT a candidate — it is the stale copy the snapshot mechanism exists to retire.
    let cache = config
        .join("plugins")
        .join("cache")
        .join(PLUGIN_NAME)
        .join(PLUGIN_NAME);
    // The fallback is a directory the operator did not choose, so it too is pinned to its real
    // path — and every step of finding it either succeeds, finds NOTHING, or FAILS with a reason
    // (codex round 7); a failure is never read as absence.
    let root = match live_candidate(&cache) {
        Candidate::Absent => {
            log(format!(
                "[wicked-core] skills.fallback {SKILLS_SNAPSHOT_ENV} is unset and no installed \
                 {PLUGIN_NAME} was found in the plugin cache under {}; workers run WITHOUT \
                 {PLUGIN_NAME} skills, and a run that names one is refused",
                config.display()
            ));
            return Ok(Ladder::Absent);
        }
        Candidate::Failed(why) => {
            log(format!(
                "[wicked-core] skills.fallback FAILED: {why}; {SKILLS_SNAPSHOT_ENV} is unset and \
                 the installed {PLUGIN_NAME} under {} could not be used as the fallback — workers \
                 run WITHOUT {PLUGIN_NAME} skills, and a run that names one is refused with this \
                 reason",
                config.display()
            ));
            return Ok(Ladder::Failed(why));
        }
        Candidate::Found(root) => root,
    };
    // A candidate that is not a CONTAINED, fully indexable tree (a link anywhere in it, an entry
    // resolving outside it, an unlistable directory, a `SKILL.md` that cannot be read or indexed)
    // is a FAILURE of the fallback (codex rounds 5, 7 and 8): the no-root rung with the reason —
    // never indexed, never silently skipped, never "no installed garden".
    let snapshot = match load_live(root.clone(), SnapshotSource::LiveCache) {
        Ok(snapshot) => snapshot,
        Err(why) => {
            let why = format!(
                "the installed {PLUGIN_NAME} at {} is not a contained, fully indexable tree: {why}",
                root.display()
            );
            log(format!(
                "[wicked-core] skills.fallback FAILED: {why}; {SKILLS_SNAPSHOT_ENV} is unset and \
                 the installed plugin could not be used as the fallback — workers run WITHOUT \
                 {PLUGIN_NAME} skills, and a run that names one is refused with this reason"
            ));
            return Ok(Ladder::Failed(why));
        }
    };
    log(format!(
        "[wicked-core] skills.fallback {SKILLS_SNAPSHOT_ENV} is unset; using the {} at {} ({} \
         skills) — Claude-only: the installed plugin carries no publish-time portability verdict, \
         so non-Claude seats are handed nothing from it. Publish a snapshot to pin a generation \
         (and to deliver to non-Claude seats) — the installed plugin also carries its interactive \
         hooks, which a snapshot excludes",
        snapshot.source,
        snapshot.root.display(),
        snapshot.skills.len()
    ));
    Ok(Ladder::Root(snapshot))
}

/// The outcome of looking for the installed plugin in the marketplace cache (codex round 7).
enum Candidate {
    /// No cache directory, or no version-named directory in it — genuinely nothing installed.
    Absent,
    /// The highest version, verified to be this plugin, pinned to its real path.
    Found(PathBuf),
    /// Something is there but could not be listed, inspected, trusted or resolved — the reason.
    Failed(String),
}

/// The highest `X.Y.Z`-named subdirectory of the marketplace cache, compared NUMERICALLY (`12.32.0`
/// beats `12.9.0`, which a lexical sort gets wrong), verified to be this plugin and pinned to its
/// real path. Candidates are sorted before the pick so the result — and the single line logged
/// about it — never depends on directory iteration order. ABSENCE vs FAILURE (codex round 7): a
/// missing cache dir or no version-named directory is `Absent`; an unlistable cache, an entry that
/// cannot be inspected, a SYMLINKED version candidate (never followed), a highest version whose
/// manifest is missing/malformed or names another plugin, or a candidate that cannot be resolved
/// to a real path is `Failed` with the reason. Names that are not versions are not candidates.
fn live_candidate(cache: &Path) -> Candidate {
    let entries = match std::fs::read_dir(cache) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Candidate::Absent,
        Err(e) => {
            return Candidate::Failed(format!(
                "the plugin cache {} cannot be listed ({e})",
                cache.display()
            ))
        }
    };
    let mut candidates: Vec<(Vec<u64>, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                return Candidate::Failed(format!(
                    "the plugin cache {} cannot be listed ({e})",
                    cache.display()
                ))
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue; // not a version name
        };
        let parsed: Option<Vec<u64>> = name.split('.').map(|s| s.parse().ok()).collect();
        let Some(version) = parsed else {
            continue;
        };
        let path = entry.path();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                return Candidate::Failed(format!(
                    "the plugin cache entry {} cannot be inspected ({e})",
                    path.display()
                ))
            }
        };
        if meta.file_type().is_symlink() {
            return Candidate::Failed(format!(
                "the plugin cache entry {} is a symlink where an installed version directory is \
                 expected; the fallback never follows a link",
                path.display()
            ));
        }
        if !meta.is_dir() {
            continue; // a file named like a version is not an installed plugin
        }
        candidates.push((version, path));
    }
    candidates.sort();
    let Some((_, latest)) = candidates.pop() else {
        return Candidate::Absent;
    };
    match plugin_manifest_name(&latest) {
        Err(why) => return Candidate::Failed(format!("{}: {why}", latest.display())),
        Ok(None) => {
            return Candidate::Failed(format!(
                "{} has no parseable {PLUGIN_MANIFEST} with a `name` — a missing or malformed \
                 manifest",
                latest.display()
            ))
        }
        Ok(Some(name)) if name != PLUGIN_NAME => {
            return Candidate::Failed(format!(
                "{} names plugin `{name}`, expected `{PLUGIN_NAME}`",
                latest.display()
            ))
        }
        Ok(Some(_)) => {}
    }
    match std::fs::canonicalize(&latest) {
        Ok(real) => Candidate::Found(simplify_verbatim(real)),
        Err(e) => Candidate::Failed(format!(
            "{} cannot be resolved to a real path ({e})",
            latest.display()
        )),
    }
}

/// Pin `named` to the ABSOLUTE REAL directory it denotes, or say why it cannot be trusted.
///
/// - Relative ⇒ refused: the engine would validate it against ITS cwd and hand it to a worker
///   running somewhere else.
/// - An ancestor that is a symlink ⇒ refused: canonicalizing would pin the generation, but a
///   link ABOVE it is a lever anyone who can flip it holds over every later resolution of the
///   same spelling; the operator is told the real path to pass instead.
/// - The FINAL component that is a symlink ⇒ refused too (design v3.4 §2; codex round 6). Rounds
///   1–5 followed crew's `current -> snapshots/<gen>` once, at load — which made the generation a
///   FRESH launch gets depend on when it resolved the link: two wrapped units of one run could
///   land on two generations across a publish. Crew resolves `current` BEFORE the handoff and
///   passes the concrete generation path; a handed link is a config error naming the link and
///   its target (a dangling one included — there is no generation to follow either way).
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
    // The LAST component, lstat'ed: a link here is never followed (v3.4 §2). The target is named
    // so an operator who passed `current` sees which generation it pointed at.
    if std::fs::symlink_metadata(named).is_ok_and(|m| m.file_type().is_symlink()) {
        let target = std::fs::read_link(named)
            .map(|t| t.display().to_string())
            .unwrap_or_else(|_| "<unreadable>".to_string());
        let real = match std::fs::canonicalize(named) {
            Ok(p) => format!("pass the real path `{}`", simplify_verbatim(p).display()),
            Err(e) => format!("and it dangles ({e}) — publish the generation it names"),
        };
        return Err(format!(
            "it is a symlink to `{target}`; every component of the snapshot path, the last \
             included, must be a real directory — crew resolves `current` before the handoff and \
             passes the concrete generation, so a fresh launch cannot change generation between \
             units — {real}"
        ));
    }
    std::fs::canonicalize(named)
        .map(simplify_verbatim)
        .map_err(|e| format!("cannot resolve it to a real path: {e}"))
}

/// `path` must exist, must NOT be a symlink, and must be a regular file (`want_file`) or a
/// directory. lstat, never a following stat: containment means the entry IS inside the root, not
/// pointed at from it. `rel` is how the entry is named in the error.
fn contained(path: &Path, rel: &str, want_file: bool) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{rel} is missing ({e})"))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{rel} is a symlink — a snapshot's files must be contained in it, not linked"
        ));
    }
    if want_file && !meta.is_file() {
        return Err(format!("{rel} is not a regular file"));
    }
    if !want_file && !meta.is_dir() {
        return Err(format!("{rel} is not a directory"));
    }
    Ok(())
}

/// A persisted spelling — an index entry's `name` or `dir`, a view entry's name — validated as
/// a SAFE RELATIVE segment set BEFORE it is joined onto any root (codex round 4; design v3.3 §3).
/// Refused, with the reason: empty; absolute (`/x`); any backslash (a Windows separator, a `\\?\`
/// verbatim or a UNC prefix); any colon (a drive or stream prefix on Windows — `C:/x`, `C:x`);
/// a NUL; an empty, `.` or `..` component; and any `/` at all unless `nested` allows the nested
/// `dir` form. On the target platform each component must also parse as exactly one `Normal`
/// path component, so a prefix spelling this portable list does not anticipate is refused where
/// it would bite. `what` names the field in the error. Returns the components.
fn safe_segments<'a>(spelling: &'a str, nested: bool, what: &str) -> Result<Vec<&'a str>, String> {
    let bad = |why: &str| format!("{what} `{spelling}` is not a clean relative path: {why}");
    if spelling.is_empty() {
        return Err(bad("it is empty"));
    }
    if spelling.contains('\\') {
        return Err(bad(
            "it contains a backslash (a Windows separator, a verbatim or a UNC prefix)",
        ));
    }
    if spelling.contains(':') {
        return Err(bad(
            "it contains a colon (a drive or stream prefix on Windows)",
        ));
    }
    if spelling.contains('\0') {
        return Err(bad("it contains a NUL byte"));
    }
    if spelling.starts_with('/') {
        return Err(bad("it is absolute"));
    }
    if !nested && spelling.contains('/') {
        return Err(bad("it must be a single path component"));
    }
    let components: Vec<&str> = spelling.split('/').collect();
    for c in &components {
        if c.is_empty() {
            return Err(bad("it has an empty component (a doubled or trailing `/`)"));
        }
        if *c == "." || *c == ".." {
            return Err(bad("it contains a `.` or `..` component"));
        }
        let mut parts = Path::new(c).components();
        match (parts.next(), parts.next()) {
            (Some(std::path::Component::Normal(_)), None) => {}
            _ => return Err(bad("a component is not a plain name on this platform")),
        }
    }
    Ok(components)
}

/// Belt to the lstat walk's braces (codex round 4; v3.3 §3): `path`, joined from `root` and
/// already walked link-free, must ALSO canonicalize to a real path under `root` — which is
/// canonical by construction (a published root is pinned at load, the live cache at resolution,
/// a view sits under real directories of such a root) — so a spelling the walk did not anticipate
/// cannot land outside. Checked BEFORE any read of `path`; `rel` names the entry in the error.
fn contained_under(root: &Path, path: &Path, rel: &str) -> Result<(), String> {
    let real = std::fs::canonicalize(path)
        .map(simplify_verbatim)
        .map_err(|e| format!("{rel} cannot be resolved to a real path ({e})"))?;
    if !real.starts_with(root) {
        return Err(format!(
            "{rel} resolves to `{}`, outside the root `{}` it was joined onto",
            real.display(),
            root.display()
        ));
    }
    Ok(())
}

/// The entry names of `dir`, sorted (so every walk and every error is the same on every platform,
/// whatever order the directory iterates in). A name that is not UTF-8 is an error: it cannot be
/// matched against an index or spelled in a refusal.
fn list_sorted(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else {
            return Err(std::io::Error::other(format!(
                "entry `{}` has a non-UTF-8 name",
                name.to_string_lossy()
            )));
        };
        names.push(name.to_string());
    }
    names.sort();
    Ok(names)
}

/// The containment refusal for a linked component of a view, worded once.
fn linked(rel: &str) -> String {
    format!("{rel} is a symlink — a snapshot's views must be contained in it, not linked")
}

/// One FIXED level of the copilot view (codex round 5): `dir` (named `rel`) may hold nothing, or
/// exactly the one entry `expected` as a REAL directory (lstat: not a symlink, not a file)
/// canonically contained in `view`; any other entry — a file, a directory, a link, whatever its
/// name — is a defect naming it, since the launch delivers the whole view. Enumeration errors
/// propagate. `Ok(true)` ⇒ `expected` is present; `Ok(false)` ⇒ the level is empty.
fn only_entry(view: &Path, dir: &Path, rel: &str, expected: &str) -> Result<bool, String> {
    let mut present = false;
    for name in list_sorted(dir).map_err(|e| format!("{rel} cannot be listed ({e})"))? {
        let entry_rel = format!("{rel}/{name}");
        if name != expected {
            return Err(format!(
                "{entry_rel} is not part of a copilot view — the delivered directory may hold \
                 only `.github/skills/<skill>/…` (one directory per enabled portable skill), and \
                 it is handed to copilot whole"
            ));
        }
        let path = dir.join(&name);
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => return Err(linked(&entry_rel)),
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(format!("{entry_rel} is not a directory")),
            Err(e) => return Err(format!("{entry_rel} cannot be inspected ({e})")),
        }
        contained_under(view, &path, &entry_rel)?;
        present = true;
    }
    Ok(present)
}

/// The frontmatter `name` of a view's `SKILL.md`, read without following a link; `None` when the
/// file is unreadable, not UTF-8, or has no parseable frontmatter name.
fn view_skill_name(file: &Path) -> Option<String> {
    read_no_follow(file)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| parse_frontmatter(&text).ok())
        .and_then(|fm| fm.name)
}

/// Read a regular file WITHOUT following a final-component symlink. On unix the open itself
/// carries `O_NOFOLLOW`, so the lstat→open window cannot be raced into a link; elsewhere the
/// lstat is re-done immediately before the read.
fn read_no_follow(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut file = {
        let meta = std::fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            return Err(std::io::Error::other("is a symlink"));
        }
        std::fs::File::open(path)?
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Drop the `\\?\` verbatim prefix Windows `canonicalize` adds (`\\?\C:\x` → `C:\x`,
/// `\\?\UNC\srv\share\x` → `\\srv\share\x`). A worker CLI (and a permission-rule glob) wants the
/// ordinary spelling. A no-op for every other prefix and on every other OS.
pub(crate) fn simplify_verbatim(path: PathBuf) -> PathBuf {
    match path.to_str() {
        Some(s) => PathBuf::from(simplify_verbatim_str(s)),
        None => path,
    }
}

/// [`simplify_verbatim`] on a spelling — shared with `state_home::under_spelled`, so every
/// containment comparison in the fence drops the prefix the same way (review pass 12).
pub(crate) fn simplify_verbatim_str(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.to_string()
    }
}

/// Load the snapshot `var` names. Strict: every shortfall is a config error naming the variable,
/// the path and the reason — this path was chosen deliberately, so nothing here degrades. The
/// generation's STATE HOME is derived from the canonical root's shape and from nothing else
/// ([`crate::state_home::derive`]).
///
/// IDENTITY (codex round 6): the snapshot must be the generation its directory says it is —
/// `snapshot.json.gen` (a number, or a string of digits) must equal the directory's name
/// numerically (crew zero-pads the directory, `000007`, and writes `gen: 7`), the directory's name
/// must be a generation name (decimal digits), and the `contentHash` and `gardenSource` crew
/// writes must be present — a `snapshot.json` without them was not published by crew. The
/// generation REPORTED (`gen`, the reaping token) is the verified directory name, never the
/// index's unverified claim. Each skill is keyed by its frontmatter `name` ONLY, which must equal
/// the path-derived name (`wicked-garden-<dir joined by ->`): a divergence is a defect, not an
/// alias.
fn load_published(var: &'static str, named: &Path) -> Result<SkillsSnapshot, SkillsError> {
    let config_err = |why: String| SkillsError::Config {
        var,
        path: named.to_path_buf(),
        why,
    };
    let root = canonical_root(named).map_err(config_err)?;
    let path = root.as_path();
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(config_err("not a directory".to_string())),
        Err(e) => return Err(config_err(format!("cannot read it: {e}"))),
    }
    // The containment walk covers EVERY component below the (canonical, hence real) root: the
    // manifest directory and file, the index, `skills/`, each skill directory, each `SKILL.md`.
    match plugin_manifest_name(path).map_err(config_err)? {
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
    contained(&index_path, SNAPSHOT_INDEX, true).map_err(config_err)?;
    let bytes = read_no_follow(&index_path)
        .map_err(|e| config_err(format!("cannot read {SNAPSHOT_INDEX}: {e}")))?;
    let index: Value = serde_json::from_slice(&bytes)
        .map_err(|e| config_err(format!("{SNAPSHOT_INDEX} is not valid JSON: {e}")))?;
    let claimed = match index.get("gen") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => {
            return Err(config_err(format!(
                "{SNAPSHOT_INDEX} has no `gen` (string or number)"
            )))
        }
    };
    // The generation IS the directory: its name must be a generation name, and the index's
    // claim must be the same generation. The verified directory name is what is reported.
    let gen = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| crate::state_home::is_generation_name(n))
        .ok_or_else(|| {
            config_err(format!(
                "its directory `{}` is not a generation name (decimal digits, as crew publishes \
                 them: `000007`); a snapshot is `<state home>/skills/snapshots/<gen>`",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ))
        })?
        .to_string();
    let same_generation = match (claimed.parse::<u64>(), gen.parse::<u64>()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same_generation {
        return Err(config_err(format!(
            "{SNAPSHOT_INDEX} says gen `{claimed}` but the directory is `{gen}`; the snapshot is \
             not the generation its path says it is (a copied or edited index) — the generation \
             directory and its index must agree, so the reported generation is the verified one"
        )));
    }
    let content_hash = match index.get("contentHash") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(other) => {
            return Err(config_err(format!(
                "{SNAPSHOT_INDEX} `contentHash` is a JSON {}, not a non-empty string — crew \
                 records the tree's content hash in every published generation",
                json_kind(other)
            )))
        }
        None => {
            return Err(config_err(format!(
                "{SNAPSHOT_INDEX} has no `contentHash` — crew records the tree's content hash in \
                 every published generation; an index without one was not published by crew"
            )))
        }
    };
    // `gardenSource` — where the bundle came from: `{kind, path, plugin_version, baseline}`, the
    // field names crew's `SnapshotManifest` writes. Required as an object with a non-empty
    // `baseline` (the content-hash identity of the bundle) and string `kind`, `path`,
    // `plugin_version` (the latter two may be empty for a source that has none).
    let source = index.get("gardenSource").ok_or_else(|| {
        config_err(format!(
            "{SNAPSHOT_INDEX} has no `gardenSource` — crew records the bundle's source \
             ({{kind, path, plugin_version, baseline}}) in every published generation"
        ))
    })?;
    let Some(source_obj) = source.as_object() else {
        return Err(config_err(format!(
            "{SNAPSHOT_INDEX} `gardenSource` is a JSON {}, not an object",
            json_kind(source)
        )));
    };
    for key in ["kind", "path", "plugin_version", "baseline"] {
        let must_be_non_empty = matches!(key, "kind" | "baseline");
        match source_obj.get(key) {
            Some(Value::String(s)) if !(must_be_non_empty && s.is_empty()) => {}
            Some(Value::String(_)) => {
                return Err(config_err(format!(
                    "{SNAPSHOT_INDEX} `gardenSource.{key}` is empty; crew records the bundle's \
                     {key} in every published generation"
                )))
            }
            Some(other) => {
                return Err(config_err(format!(
                    "{SNAPSHOT_INDEX} `gardenSource.{key}` is a JSON {}, not a string",
                    json_kind(other)
                )))
            }
            None => {
                return Err(config_err(format!(
                    "{SNAPSHOT_INDEX} `gardenSource` has no `{key}`"
                )))
            }
        }
    }
    // The recorded baseline AUTHORIZES the `.venv` link (review pass 11; crew's `verifyCurrent`):
    // it must be a sha256 content hash — never free text, which could carry `..` — and the env
    // state must be one of the four crew writes, so the link can be bound to both.
    let baseline = match source_obj.get("baseline").and_then(Value::as_str) {
        Some(s) if is_content_hash(s) => s.to_string(),
        Some(s) => {
            return Err(config_err(format!(
                "{SNAPSHOT_INDEX} `gardenSource.baseline` `{s}` is not a sha256 content hash (64 \
                 lowercase hex digits) — the recorded baseline authorizes the `.venv` link and is \
                 never trusted as free text"
            )))
        }
        None => unreachable!("checked above: gardenSource.baseline is a non-empty string"),
    };
    let venv = match index.get("venv") {
        Some(Value::String(s)) => VenvState::parse(s).ok_or_else(|| {
            config_err(format!(
                "{SNAPSHOT_INDEX} `venv` is `{s}`, not one of {} — crew records the baseline \
                 env's state in every published generation",
                VenvState::SPELLINGS
            ))
        })?,
        Some(other) => {
            return Err(config_err(format!(
                "{SNAPSHOT_INDEX} `venv` is a JSON {}, not a string ({})",
                json_kind(other),
                VenvState::SPELLINGS
            )))
        }
        None => {
            return Err(config_err(format!(
                "{SNAPSHOT_INDEX} has no `venv` — crew records the baseline env's state ({}) in \
                 every published generation; it decides whether the generation may carry the \
                 `.venv` link",
                VenvState::SPELLINGS
            )))
        }
    };
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
        // crew's `SnapshotSkillRow.dir` is PLUGIN-relative — `skills/<dir>`, the `skills/` prefix
        // literal (its row validator requires exactly that). The engine keys everything below by
        // the path UNDER `skills/`, so the prefix is stripped here; a row without it was not
        // written by crew and is a defect naming the spelling crew writes (review pass 7: rounds
        // 1–6 read the field as already relative to `skills/`, so a real generation would have
        // failed to load at its first skill).
        let spelled = field("dir")?;
        let Some(dir) = spelled
            .strip_prefix(&format!("{SKILLS_DIR}/"))
            .map(str::to_string)
        else {
            return Err(config_err(format!(
                "{SNAPSHOT_INDEX} skills[{i}] (`{name}`) dir `{spelled}` is not plugin-relative — \
                 crew writes `{SKILLS_DIR}/<dir>` (the `{SKILLS_DIR}/` prefix literal), and the \
                 loader keys the skill by the path under it"
            )));
        };
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
        // v3.3 §3: the name is joined onto the copilot view (`.github/skills/<name>/`) and the
        // dir onto `skills/`; both are validated as safe relative spellings HERE, at index
        // verification, before anything is ever joined (the dir inside `verify_skill_file`).
        if let Err(why) = safe_segments(&name, false, "name") {
            defects.push(format!("skills[{i}]: {why}"));
        }
        // ONE identity per skill (v3 §5; codex round 6): the frontmatter/index `name` must be the
        // name its directory derives — crew refuses to publish anything else, and the loader no
        // longer aliases a divergent pair (a ref could otherwise reach one skill by two names).
        let derived = derived_name(&dir);
        if safe_segments(&dir, true, "dir").is_ok() && derived != name {
            defects.push(format!(
                "skills[{i}]: `{name}` at skills/{dir} is not the path-derived name `{derived}` — \
                 a skill is keyed by its frontmatter name, which must equal the name its \
                 directory derives (no alias is made for the divergence)"
            ));
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
    // A valid plugin root, indexed and contained — now WHERE it is: the state home whose fence
    // is opened around it is derived from the root's own shape (three components up) and from
    // nothing else. Checked last so an operator pointing at something that is not a snapshot at
    // all is told that first.
    let state_home = crate::state_home::derive(path).map_err(config_err)?;
    let snapshot = SkillsSnapshot {
        root,
        source: SnapshotSource::Published,
        gen: Some(gen),
        content_hash: Some(content_hash),
        state_home: Some(state_home),
        baseline: Some(baseline),
        venv: Some(venv),
        skills,
    };
    // Exact index/tree PARITY over the whole delivered closure (codex round 7): every `SKILL.md`
    // the tree holds is indexed, nothing but the copilot view sits under `views/`, and no link
    // sits anywhere but crew's `.venv` into the baseline env.
    snapshot.verify_delivered_tree().map_err(|e| match e {
        SkillsError::Config { why, .. } => config_err(why),
        other => other,
    })?;
    Ok(snapshot)
}

/// The `SKILL.md` an index entry names, verified: `dir` is a safe relative `/`-path
/// ([`safe_segments`] — no absolute, drive, verbatim or UNC spelling, no `.`/`..`; v3.3 §3), EVERY
/// component from the root down — `skills/` itself first, then each directory of `dir`, then the
/// leaf — exists and is NOT a symlink (lstat walk — the file must be contained in the root, not
/// pointed at from it; a `skills -> /outside` link is refused at `skills`, before anything under
/// it is looked at), the leaf is a regular file that canonicalizes to a path INSIDE the root
/// ([`contained_under`], checked before the read), and it is readable (no-follow) with a
/// frontmatter block. Returns the frontmatter.
fn verify_skill_file(root: &Path, dir: &str) -> Result<Frontmatter, String> {
    let components = safe_segments(dir, true, "dir")?;
    let mut at = root.to_path_buf();
    let rel = |p: &Path| {
        p.strip_prefix(root)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| p.display().to_string())
    };
    let mut walk: Vec<&str> = Vec::with_capacity(components.len() + 2);
    walk.push(SKILLS_DIR);
    walk.extend(components.iter().copied());
    walk.push(SKILL_FILE);
    let last = walk.len() - 1;
    for (i, component) in walk.into_iter().enumerate() {
        at.push(component);
        contained(&at, &rel(&at), i == last)?;
    }
    contained_under(root, &at, &rel(&at))?;
    let bytes = read_no_follow(&at).map_err(|e| format!("{} is not readable ({e})", rel(&at)))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", rel(&at)))?;
    parse_frontmatter(&text).map_err(|e| match e {
        FrontmatterError::NoBlock => format!("{} has no `---` frontmatter block", rel(&at)),
        FrontmatterError::Malformed(why) => {
            format!("{} has malformed frontmatter: {why}", rel(&at))
        }
    })
}

/// Index an INSTALLED plugin root (no `snapshot.json`): every directory under `skills/` holding a
/// `SKILL.md`, nested ones included, keyed by frontmatter `name`. `portable` is approximated from
/// the text — advisory only: a live root is Claude-only ([`SkillsError::FallbackClaudeOnly`]).
///
/// FAIL-CLOSED whole-tree containment, the same standard as a published generation (codex rounds
/// 5, 7 and 8): `root` and `root/skills` are lstat-checked BEFORE `skills/` is read (a
/// `skills -> /outside` link is refused at `skills`); inside the walk a SYMLINK anywhere — a linked
/// skill directory, a linked file — is refused by path (round 5 skipped it; round 8: a delivered
/// tree may hold no link the worker would read through); a `SKILL.md` that cannot be read, is not
/// UTF-8, has no frontmatter block, malformed frontmatter or no `name` is refused with the error
/// (round 5 skipped it with a notice — a live root must be FULLY indexable, or it is not a root);
/// every indexed entry must canonicalize inside `root` ([`contained_under`]); a directory that
/// cannot be listed or an entry that cannot be inspected is an error. After indexing, the whole
/// delivered closure — support files included — is walked by
/// [`SkillsSnapshot::verify_delivered_tree`], exactly as for a published generation (with no state
/// home, even a `.venv` link is refused). A plugin with NO `skills/` at all is an empty index.
/// `Err(why)` ⇒ the ladder takes the no-root rung WITH the reason ([`Ladder::Failed`]); `rel`
/// spellings in the error are `/`-joined from the root.
fn load_live(root: PathBuf, source: SnapshotSource) -> Result<SkillsSnapshot, String> {
    contained(&root, "the root", false)?;
    let skills_dir = root.join(SKILLS_DIR);
    let mut skills = Vec::new();
    match std::fs::symlink_metadata(&skills_dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{SKILLS_DIR} cannot be inspected ({e})")),
        Ok(_) => {
            contained(&skills_dir, SKILLS_DIR, false)?;
            contained_under(&root, &skills_dir, SKILLS_DIR)?;
            walk_skills(&root, &skills_dir, &[], &mut skills)?;
        }
    }
    skills.sort_by(|a, b| a.dir.cmp(&b.dir));
    let snapshot = SkillsSnapshot {
        root,
        source,
        gen: None,
        content_hash: None,
        state_home: None,
        baseline: None,
        venv: None,
        skills,
    };
    // The same whole-tree containment a published generation gets: no link anywhere in the
    // delivered closure (support tree included), every SKILL.md indexed (the walk above already
    // refused any it could not index), only the copilot view under `views/`.
    snapshot.verify_delivered_tree().map_err(|e| match e {
        SkillsError::Config { why, .. } => why,
        other => other.to_string(),
    })?;
    Ok(snapshot)
}

fn walk_skills(
    root: &Path,
    dir: &Path,
    rel: &[String],
    out: &mut Vec<SkillEntry>,
) -> Result<(), String> {
    let here = || {
        if rel.is_empty() {
            SKILLS_DIR.to_string()
        } else {
            format!("{SKILLS_DIR}/{}", rel.join("/"))
        }
    };
    // Sorted so the walk — and every line it logs — is the same on every platform, whatever
    // order the directory iterates in. Every entry is inspected or the walk fails: a directory
    // that cannot be listed, or an entry that cannot be stat'ed, is not "empty".
    let names = list_sorted(dir).map_err(|e| format!("{} cannot be listed ({e})", here()))?;
    for file_name in names {
        let path = dir.join(&file_name);
        let child_rel_str = format!("{}/{file_name}", here());
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("{child_rel_str} cannot be inspected ({e})"))?;
        // A link is never followed — and (codex round 8) never tolerated either: the whole root
        // is handed to Claude's plugin loader, which WOULD follow it.
        if meta.file_type().is_symlink() {
            return Err(format!(
                "{child_rel_str} is a symlink — the installed plugin must be a contained tree to \
                 serve as the skills fallback (no link anywhere in it)"
            ));
        }
        if !meta.is_dir() {
            continue;
        }
        contained_under(root, &path, &child_rel_str)?;
        let mut child_rel = rel.to_vec();
        child_rel.push(file_name);
        let skill_md = path.join(SKILL_FILE);
        match std::fs::symlink_metadata(&skill_md) {
            Err(_) => {}
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!(
                    "{child_rel_str}/{SKILL_FILE} is a symlink — the installed plugin must be a \
                     contained tree to serve as the skills fallback"
                ))
            }
            Ok(m) if !m.is_file() => {
                return Err(format!(
                    "{child_rel_str}/{SKILL_FILE} is not a regular file"
                ))
            }
            Ok(_) => {
                let dir = child_rel.join("/");
                let rel_file = format!("{child_rel_str}/{SKILL_FILE}");
                contained_under(root, &skill_md, &rel_file)?;
                // Every SKILL.md the fallback holds must be INDEXABLE, or the root is not a
                // fallback: an unreadable or non-UTF-8 file, a missing or malformed frontmatter
                // block, or no `name` is refused with the error (round 5 skipped it with a notice,
                // leaving Claude to load a skill the engine could not name).
                let bytes = read_no_follow(&skill_md)
                    .map_err(|e| format!("{rel_file} cannot be read ({e})"))?;
                let text = String::from_utf8(bytes)
                    .map_err(|e| format!("{rel_file} is not UTF-8 ({e})"))?;
                let fm = parse_frontmatter(&text).map_err(|e| match e {
                    FrontmatterError::NoBlock => {
                        format!(
                            "{rel_file} has no `---` frontmatter block, so it cannot be indexed"
                        )
                    }
                    FrontmatterError::Malformed(why) => {
                        format!("{rel_file} has malformed frontmatter ({why})")
                    }
                })?;
                let Some(name) = fm.name else {
                    return Err(format!(
                        "{rel_file} has no frontmatter `name`, so it cannot be indexed (the \
                         harness would not know it by a derived name either)"
                    ));
                };
                out.push(SkillEntry {
                    name,
                    dir,
                    portable: !has_nonportable_markers(&text),
                    mandates: fm.mandates,
                });
            }
        }
        walk_skills(root, &path, &child_rel, out)?;
    }
    Ok(())
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

/// Why a `SKILL.md` yielded no frontmatter.
#[derive(Debug, PartialEq, Eq)]
enum FrontmatterError {
    /// The text does not open with a `---` block at all.
    NoBlock,
    /// The block is there but is not usable YAML for this purpose: unterminated, an invalid
    /// document (an unterminated flow list, a tab where YAML wants spaces), not a mapping, a
    /// `name` that is not a string, a `mandates` that is neither a string nor a list of strings.
    Malformed(String),
}

/// `name:` and `mandates:` from a `---`-fenced frontmatter block, parsed with YAML SEMANTICS
/// (`serde_yaml`): a comment after a value is a comment (`name: x # comment` is `x`), quoted
/// scalars are unquoted, `mandates` may be a flow list (`[a, b] # comment`), a block list (`- a`
/// lines) or a single scalar. The hand parser pass 2 shipped read the comment into the name and
/// the `# comment` suffix into a bogus mandate — so a VALID published skill blocked every launch
/// that named it (codex round 3). A malformed document — an unterminated block, an unterminated
/// flow list, a tab in indentation, a non-mapping, a non-string `name` — is an error the caller
/// names the file in ([`verify_skill_file`]); never a default identity.
fn parse_frontmatter(text: &str) -> Result<Frontmatter, FrontmatterError> {
    use serde_yaml::Value as Yaml;
    let mut lines = text.lines();
    match lines.next() {
        Some(first) if first.trim_end() == "---" => {}
        _ => return Err(FrontmatterError::NoBlock),
    }
    let mut block = String::new();
    let mut terminated = false;
    for line in lines {
        let t = line.trim_end();
        if t == "---" || t == "..." {
            terminated = true;
            break;
        }
        block.push_str(line);
        block.push('\n');
    }
    if !terminated {
        return Err(FrontmatterError::Malformed(
            "the `---` frontmatter block is not terminated by a closing `---`".to_string(),
        ));
    }
    let doc: Yaml = serde_yaml::from_str(&block)
        .map_err(|e| FrontmatterError::Malformed(format!("not valid YAML ({e})")))?;
    let map = match doc {
        Yaml::Null => return Ok(Frontmatter::default()),
        Yaml::Mapping(m) => m,
        other => {
            return Err(FrontmatterError::Malformed(format!(
                "the frontmatter is a YAML {}, not a mapping",
                yaml_kind(&other)
            )))
        }
    };
    let mut fm = Frontmatter::default();
    match map.get("name") {
        None | Some(Yaml::Null) => {}
        Some(Yaml::String(s)) => {
            let s = s.trim();
            if !s.is_empty() {
                fm.name = Some(s.to_string());
            }
        }
        Some(other) => {
            return Err(FrontmatterError::Malformed(format!(
                "`name` is a YAML {}, not a string",
                yaml_kind(other)
            )))
        }
    }
    match map.get("mandates") {
        None | Some(Yaml::Null) => {}
        Some(Yaml::String(s)) => {
            let s = s.trim();
            if !s.is_empty() {
                fm.mandates.push(s.to_string());
            }
        }
        Some(Yaml::Sequence(items)) => {
            for item in items {
                match item {
                    Yaml::String(s) => {
                        let s = s.trim();
                        if !s.is_empty() {
                            fm.mandates.push(s.to_string());
                        }
                    }
                    other => {
                        return Err(FrontmatterError::Malformed(format!(
                            "`mandates` holds a YAML {}, not a string",
                            yaml_kind(other)
                        )))
                    }
                }
            }
        }
        Some(other) => {
            return Err(FrontmatterError::Malformed(format!(
                "`mandates` is a YAML {}, not a string or a list of strings",
                yaml_kind(other)
            )))
        }
    }
    fm.mandates.sort();
    fm.mandates.dedup();
    Ok(fm)
}

fn yaml_kind(v: &serde_yaml::Value) -> &'static str {
    match v {
        serde_yaml::Value::Null => "null",
        serde_yaml::Value::Bool(_) => "boolean",
        serde_yaml::Value::Number(_) => "number",
        serde_yaml::Value::String(_) => "string",
        serde_yaml::Value::Sequence(_) => "sequence",
        serde_yaml::Value::Mapping(_) => "mapping",
        serde_yaml::Value::Tagged(_) => "tagged value",
    }
}

/// The `name` in `<root>/.claude-plugin/plugin.json`: `Ok(None)` when the directory or the file
/// is absent, is not a regular file, or does not parse to an object with a string `name`;
/// `Err` when `.claude-plugin/` or `plugin.json` IS present but is a symlink — a linked manifest
/// is a containment defect, not "no manifest", so it is named rather than treated as absent.
fn plugin_manifest_name(root: &Path) -> Result<Option<String>, String> {
    let dir = root.join(".claude-plugin");
    match std::fs::symlink_metadata(&dir) {
        Err(_) => return Ok(None),
        Ok(m) if m.file_type().is_symlink() => {
            return Err(
                ".claude-plugin is a symlink — a snapshot's files must be contained in it, not \
                 linked"
                    .to_string(),
            )
        }
        Ok(m) if !m.is_dir() => return Ok(None),
        Ok(_) => {}
    }
    let path = dir.join("plugin.json");
    match std::fs::symlink_metadata(&path) {
        Err(_) => return Ok(None),
        Ok(m) if m.file_type().is_symlink() => {
            return Err(format!(
                "{PLUGIN_MANIFEST} is a symlink — a snapshot's files must be contained in it, not \
                 linked"
            ))
        }
        Ok(m) if !m.is_file() => return Ok(None),
        Ok(_) => {}
    }
    let Ok(bytes) = read_no_follow(&path) else {
        return Ok(None);
    };
    let Ok(manifest) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(None);
    };
    Ok(manifest
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

// ── Admission ─────────────────────────────────────────────────────────────────

/// What one unit's launch requires of the skills root, in the two senses admission judges
/// separately (v3.1 §5): `plan` — every ref the RUN names, whose EXISTENCE is required so a
/// missing skill refuses the run at its first unit, not at the unit that needed it; `seat` — the
/// refs THIS unit's seat will actually invoke, the only ones whose CLI compatibility (Claude's
/// one-directory-deep discovery, the mirrors' `portable` requirement) is judged. `seat ⊆ plan`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequiredRefs<'a> {
    pub plan: Vec<&'a str>,
    pub seat: Vec<&'a str>,
}

impl<'a> RequiredRefs<'a> {
    /// From a launch input: the run-wide set the actor computed ([`StepInput::required_skills`])
    /// plus the unit's own `skill_ref` (so a directly-constructed input is still admitted on its
    /// own terms) is the plan; the unit's own `skill_ref` is the seat.
    pub(crate) fn of(input: &'a StepInput) -> Self {
        let seat: Vec<&str> = input
            .unit
            .skill_ref
            .as_deref()
            .filter(|r| !r.is_empty())
            .into_iter()
            .collect();
        let plan: Vec<&str> = input
            .required_skills
            .iter()
            .map(String::as_str)
            .chain(seat.iter().copied())
            .filter(|r| !r.is_empty())
            .collect();
        Self { plan, seat }
    }

    /// A unit judged on its own terms: what its seat invokes is the whole plan.
    #[cfg(test)]
    pub(crate) fn seat(refs: impl IntoIterator<Item = &'a str>) -> Self {
        let seat: Vec<&str> = refs.into_iter().filter(|r| !r.is_empty()).collect();
        Self {
            plan: seat.clone(),
            seat,
        }
    }

    /// A plan-wide set with the seat's own subset spelled out.
    #[cfg(test)]
    pub(crate) fn plan_and_seat(
        plan: impl IntoIterator<Item = &'a str>,
        seat: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let seat: Vec<&str> = seat.into_iter().filter(|r| !r.is_empty()).collect();
        let plan: Vec<&str> = plan
            .into_iter()
            .chain(seat.iter().copied())
            .filter(|r| !r.is_empty())
            .collect();
        Self { plan, seat }
    }
}

/// Admit a launch on `cli` that requires `refs`.
///
/// EXISTENCE, plan-wide: every ref in `refs.plan` — of ANY family — and everything it
/// transitively `mandates` must resolve in `snapshot`, or the launch is refused naming the
/// missing ones. INVOCABILITY, seat-specific: for the closure of `refs.seat` only, the CLI's own
/// limits apply — a Claude seat cannot be handed a nested skill (no Claude identity exists for
/// it), and a non-Claude seat cannot be handed a `portable: false` one (its mirror excludes it).
/// A skill another seat invokes is required to EXIST here but is not judged for THIS CLI: a
/// Codex unit is not refused because a Claude unit needs a non-portable skill, and a Claude unit
/// is not refused because a mirror seat uses a nested one. `Ok(None)` ⇒ nothing required and no
/// root to hand.
pub(crate) fn admit_refs(
    snapshot: Option<SkillsSnapshot>,
    refs: &RequiredRefs<'_>,
    cli: &WorkerCli,
) -> Result<Option<SkillsSnapshot>, SkillsError> {
    let plan: Vec<&str> = refs
        .plan
        .iter()
        .copied()
        .filter(|r| !r.is_empty())
        .collect();
    require_existence(snapshot.as_ref(), &plan)?;
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };
    let invoked = snapshot.closure(refs.seat.iter().copied());
    match cli {
        WorkerCli::Claude => {
            let nested: Vec<(String, String)> = invoked
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
        }
        WorkerCli::Other { key, lever } => {
            // codex round 7: the live-cache FALLBACK is Claude-only. Its `portable` flags are an
            // approximation from the `SKILL.md` text (`${CLAUDE_PLUGIN_ROOT}`, `../`) that cannot
            // see cwd-relative script invocations — crew's publish analyzer can — so nothing from
            // it is delivered to a non-Claude seat, and a unit that invokes a skill there is
            // refused naming the seat rather than handed a directory whose portability nobody
            // judged. A skill-free non-Claude unit still runs (with no delivery).
            if snapshot.source == SnapshotSource::LiveCache && !invoked.required.is_empty() {
                return Err(SkillsError::FallbackClaudeOnly {
                    root: snapshot.root.clone(),
                    cli: key.clone(),
                    skills: invoked.required.iter().map(|e| e.name.clone()).collect(),
                });
            }
            let nonportable: Vec<String> = invoked
                .required
                .iter()
                .filter(|e| !e.portable)
                .map(|e| e.name.clone())
                .collect();
            if !nonportable.is_empty() {
                return Err(SkillsError::NotPortable {
                    root: snapshot.root.clone(),
                    cli: key.clone(),
                    skills: nonportable,
                });
            }
            // A portable skill whose directory nests a non-portable one is undeliverable to a
            // DIRECTORY lever — the parent's directory would carry the Claude-only child into a
            // recursive scan (codex round 3). Refused by name, parent and child both — but only
            // for the levers that hand over original directories (pi, opencode; codex round 4):
            // copilot is judged on the VIEW crew published, whose whole tree is validated below
            // (a child copied into it refuses there; one excluded from it does not refuse the
            // parent), and an absent lever delivers no directory at all.
            if lever.delivers_directories() {
                let nesting: Vec<(String, String)> = invoked
                    .required
                    .iter()
                    .flat_map(|e| {
                        snapshot
                            .nonportable_nested(e)
                            .into_iter()
                            .map(move |n| (e.name.clone(), n.dir.clone()))
                    })
                    .collect();
                if !nesting.is_empty() {
                    return Err(SkillsError::NestsNonPortable {
                        root: snapshot.root.clone(),
                        cli: key.clone(),
                        skills: nesting,
                    });
                }
            }
            // v3.2 §3: the seat must have a wicked-owned way to DELIVER what it invokes — a unit
            // that needs no skill on a lever-less seat runs (without skills); one that names a
            // skill is refused rather than served through the user's own CLI directories. The
            // copilot view is VERIFIED for what this seat invokes (every component contained,
            // every required skill present as stated) — an empty or partial view refuses by
            // name; a linked component refuses as a containment defect even when nothing is
            // invoked, since the launch would still `--add-dir` it.
            if !invoked.required.is_empty() {
                let why = match lever {
                    SkillsLever::Absent => Some(
                        "this CLI has no per-launch skills lever the engine can pull".to_string(),
                    ),
                    SkillsLever::CopilotAddDir => {
                        match snapshot.copilot_view_for(&invoked.required)? {
                            Some(_) => None,
                            None => Some(format!(
                                "the skills snapshot at {} publishes no views/copilot for \
                                 `--add-dir` to load",
                                snapshot.root.display()
                            )),
                        }
                    }
                    _ => None,
                };
                if let Some(why) = why {
                    return Err(SkillsError::NoLever {
                        cli: key.clone(),
                        skills: invoked.required.iter().map(|e| e.name.clone()).collect(),
                        why,
                    });
                }
            } else if *lever == SkillsLever::CopilotAddDir {
                snapshot.copilot_view_for(&[])?;
            }
        }
    }
    Ok(Some(snapshot))
}

/// EXISTENCE, plan-wide: every ref in `plan` — of ANY family — and everything it transitively
/// `mandates` must resolve in `snapshot`, or the launch is refused naming the missing ones (sorted,
/// deduplicated); with no root at all every ref is missing, and an empty plan needs nothing.
fn require_existence(snapshot: Option<&SkillsSnapshot>, plan: &[&str]) -> Result<(), SkillsError> {
    let Some(snapshot) = snapshot else {
        let mut missing: Vec<String> = plan.iter().map(|r| r.to_string()).collect();
        missing.sort();
        missing.dedup();
        if missing.is_empty() {
            return Ok(());
        }
        return Err(SkillsError::Missing {
            root: None,
            missing,
        });
    };
    let existence = snapshot.closure(plan.iter().copied());
    if !existence.missing.is_empty() {
        return Err(SkillsError::Missing {
            root: Some(snapshot.root.clone()),
            missing: existence.missing,
        });
    }
    Ok(())
}

/// The launch admission for one FRESH launch, on either spawn path — [`admit_turn`] for a
/// [`Turn::Fresh`]: resolve the root (the ladder), refuse a root the worker Read fence would deny
/// ([`fence_admit`]), then require every skill the run names — see [`admit_refs`]. `Ok(None)` ⇒
/// the unit needs no skill and there is no root to hand it.
pub(crate) fn admit_unit(
    input: &StepInput,
    cli: &WorkerCli,
    operational_home: Option<&Path>,
) -> Result<Option<SkillsSnapshot>, SkillsError> {
    admit_turn(Turn::Fresh, input, cli, operational_home)
}

/// The run-wide EXISTENCE admission for a unit that spawns NO worker — a TOOL COMMAND
/// (`actor::dispatch_unit`; codex round 6). The plan-wide set ([`StepInput::required_skills`])
/// exists so a run whose snapshot lacks a skill is refused before its FIRST unit does work; a
/// tool-command first unit bypasses both worker runners, so without this it executed — and could
/// mutate state — before a later agent unit discovered the missing skill. Judged like a fresh
/// launch's existence half: the ladder is resolved (an explicit path that is not a snapshot is the
/// same config error every launch gets) and every ref the RUN names must exist, transitive
/// mandates included; nothing seat-specific applies (no CLI runs) and no fence is opened (no
/// worker reads the root). A run that names NO skill has nothing to admit and resolves nothing
/// (`Ok(None)`). `Ok(Some(root))` is the generation the run was judged against — the actor
/// reports it like a handoff (`SkillsSnapshotHanded`, `path: "tool_cmd"`), so crew's ledger pins
/// it for the session from its first unit and the operator can see which generation admitted the
/// plan.
pub(crate) fn admit_plan(input: &StepInput) -> Result<Option<SkillsSnapshot>, SkillsError> {
    let refs = RequiredRefs::of(input);
    let plan: Vec<&str> = refs
        .plan
        .iter()
        .copied()
        .filter(|r| !r.is_empty())
        .collect();
    if plan.is_empty() {
        return Ok(None);
    }
    match resolve_ladder()? {
        Ladder::Root(s) => {
            require_existence(Some(&s), &plan)?;
            Ok(Some(s))
        }
        Ladder::Absent => {
            require_existence(None, &plan)?;
            Ok(None)
        }
        Ladder::Failed(why) => {
            refuse_plan_on_failed_ladder(&plan, why)?;
            Ok(None)
        }
    }
}

/// A FAILED fallback discovery is the no-root rung WITH its reason attached (codex round 7): a run
/// that names no skill proceeds without a root exactly as under a genuine absence; one that names
/// any is refused naming the skills AND why no root could be used — never "no installed garden"
/// for a garden that is installed but unreadable, symlinked or malformed.
fn refuse_plan_on_failed_ladder(plan: &[&str], why: String) -> Result<(), SkillsError> {
    let mut missing: Vec<String> = plan
        .iter()
        .filter(|r| !r.is_empty())
        .map(|r| r.to_string())
        .collect();
    missing.sort();
    missing.dedup();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(SkillsError::FallbackFailed { why, missing })
    }
}

// ── Seat eligibility (routing) ────────────────────────────────────────────────

/// What a unit's skills require of the SEAT that runs it, decided from the handed snapshot at
/// DISTRIBUTION (core#401) — the routing-time half of what [`admit_refs`] enforces at launch, so
/// the council is never asked to pick among seats the ladder would then refuse by name (the live
/// case: a `capture-learnings` unit carrying `wicked-garden-repo-learn`, `portable: false`, was
/// council-routed to copilot, refused correctly, and the escalation gate could only re-dispatch to
/// the same seat or cancel). Judged on the unit's OWN `skill_ref` and its transitive mandates —
/// the `seat` half of [`RequiredRefs`], exactly the closure `admit_refs` judges for the seat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SeatRequirement {
    /// Any roster seat: the unit names no skill, names one the root does not hold (EXISTENCE is
    /// the launch admission's refusal, plan-wide at the first unit), or every skill it invokes is
    /// `portable: true` in a PUBLISHED snapshot. Nothing here narrows a lever-less seat (codex, a
    /// bridge that forwards no flags): a portable skill on such a seat is still refused by name at
    /// launch, exactly as today — this decides portability, not delivery.
    Any,
    /// Only a Claude seat can be handed what the unit invokes (design v3.2 §3, both carriers).
    /// `skills` are the ones that need it; `why` is the wire account
    /// ([`CoreEvent::UnitDistributed`]`.seat_constraint`) and the refusal's reason when no such
    /// seat is on the roster ([`SkillsError::NoEligibleSeat`]).
    ClaudeOnly { skills: Vec<String>, why: String },
}

/// The seat kind a non-portable requirement admits — the only CLI whose delivery loads a
/// `portable: false` skill (the plugin loader, on both carriers).
pub(crate) const NONPORTABLE_SEAT: &str = "claude";

/// The [`SeatRequirement`] of a unit whose `skill_ref` is `skill_ref`, against `snapshot`.
///
/// - A published snapshot: the closure over `mandates` is taken exactly as [`admit_refs`] takes
///   it for the seat; any `portable: false` entry in it makes the unit Claude-only, named.
/// - The live-cache FALLBACK is Claude-only for ANY skill it holds (codex round 7 on #396: its
///   `portable` flags are a text approximation nobody published, so `admit_refs` refuses every
///   non-Claude seat with [`SkillsError::FallbackClaudeOnly`]) — the routing says the same thing
///   before the council does, rather than after the ladder has.
/// - Refs the root does not hold are not judged here: with no entry there is no portability to
///   read, and the launch admission refuses the run by name at its first unit.
pub(crate) fn seat_requirement(
    snapshot: &SkillsSnapshot,
    skill_ref: Option<&str>,
) -> SeatRequirement {
    let Some(skill_ref) = skill_ref.filter(|r| !r.is_empty()) else {
        return SeatRequirement::Any;
    };
    let invoked = snapshot.closure([skill_ref]);
    if invoked.required.is_empty() {
        return SeatRequirement::Any;
    }
    if snapshot.source == SnapshotSource::LiveCache {
        let skills: Vec<String> = invoked.required.iter().map(|e| e.name.clone()).collect();
        return SeatRequirement::ClaudeOnly {
            why: format!(
                "the {} at {} is this run's skills root ({SKILLS_SNAPSHOT_ENV} is unset) and \
                 carries no publish-time portability verdict, so it is Claude-only: {}",
                SnapshotSource::LiveCache,
                snapshot.root.display(),
                skills.join(", ")
            ),
            skills,
        };
    }
    let nonportable: Vec<String> = invoked
        .required
        .iter()
        .filter(|e| !e.portable)
        .map(|e| e.name.clone())
        .collect();
    if nonportable.is_empty() {
        return SeatRequirement::Any;
    }
    SeatRequirement::ClaudeOnly {
        why: format!(
            "the skills snapshot at {} ({}) marks {} as portable: false (Claude-only — they lean \
             on ${{CLAUDE_PLUGIN_ROOT}}, cwd-relative scripts or ../ links no mirror carries)",
            snapshot.root.display(),
            snapshot.gen_label(),
            nonportable.join(", ")
        ),
        skills: nonportable,
    }
}

/// The skills root DISTRIBUTION judges seat eligibility against (core#401): the ladder's root
/// when it has one. `None` — the council picks among the whole roster, unconstrained — when the
/// ladder is absent, failed or misconfigured: no seat can be handed anything then, and the launch
/// admission refuses a skill-naming run at its FIRST unit, before work, exactly as today. The
/// routing anticipates the ladder; it never replaces it, and it decides nothing the ladder would
/// not. The ladder logs its own step; a config error is logged here too, since the refusal it
/// produces belongs to the launch, not to this call.
pub(crate) fn routing_snapshot() -> Option<SkillsSnapshot> {
    routing_root(resolve_ladder(), &mut |line| eprintln!("{line}"))
}

/// [`routing_snapshot`] on an already-resolved ladder, with its log sink explicit.
pub(crate) fn routing_root(
    ladder: Result<Ladder, SkillsError>,
    log: &mut dyn FnMut(String),
) -> Option<SkillsSnapshot> {
    match ladder {
        Ok(Ladder::Root(s)) => Some(s),
        Ok(Ladder::Absent | Ladder::Failed(_)) => None,
        Err(e) => {
            log(format!(
                "[wicked-core] skills.routing the skills root could not be resolved for seat \
                 selection ({e}); seats are unconstrained and the launch admission decides"
            ));
            None
        }
    }
}

/// What a turn is judged against (codex round 5): a FRESH launch resolves the ambient root; a
/// CACHED ACP session is judged against what it was opened with — and ONLY that, `None` included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Turn {
    /// A launch that will open a new process: nothing pinned yet; the ladder decides.
    Fresh,
    /// A turn on a cached ACP session, carrying the snapshot the session was opened with
    /// (`proc.skills`) — `None` when it was opened WITHOUT one (no root on the ladder then).
    Cached(Option<SkillsSnapshot>),
}

/// ONE admission policy for every turn on either carrier — a fresh launch and a cached ACP session
/// alike (codex round 4), with the two kinds of turn told apart (codex round 5). Round 3 sent a
/// cached session straight to [`admit_refs`] against `proc.skills` while a fresh one went through
/// [`admit_unit`], so the two disagreed where the cache held NO snapshot; round 4 then treated
/// "cached with nothing pinned" exactly like "fresh" — resolving the AMBIENT root for a session
/// that never received it. A snapshot reaches an ACP session ONLY at `session/new`, so that
/// admitted a skill-bearing turn against a plugin the bridge never loaded and generated a
/// directive for a skill the session could not invoke.
///
/// - The inherit-config escape hatch (`execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV`) bypasses
///   NOTHING here (codex round 6; rounds 2–5 returned `Ok(None)` before resolving anything, so
///   under the hatch an invalid explicit snapshot loaded nothing and said nothing, a missing
///   required skill proceeded on the operator's plugins, and the template's stale `--plugin-dir`
///   survived). The hatch decides ONLY whether the operator's ambient configuration is inherited
///   IN ADDITION to the snapshot (`execute_wrapped::inject_isolation_flags`,
///   `acp_runner::worker_claude_config_dir`); the snapshot is resolved, admitted and handed
///   exactly as without it.
/// - [`Turn::Fresh`] resolves the ambient root (the ladder), passes it through the fence check,
///   and is admitted against it.
/// - [`Turn::Cached`]`(Some(pinned))` — the generation the session was opened with (v3.1 §4) — is
///   judged against THAT, never against a re-resolved `current`: the bridge holds the plugin it
///   loaded. Its fence was checked when it was opened.
/// - [`Turn::Cached`]`(None)` — a session opened with NO snapshot — is admitted only for a turn
///   whose plan names no skill; a skill-bearing turn is REFUSED ([`SkillsError::NotDelivered`],
///   naming the skills and advising a fresh session). The ambient configuration is never
///   consulted for it: whatever a root now holds, this session did not receive it.
pub(crate) fn admit_turn(
    turn: Turn,
    input: &StepInput,
    cli: &WorkerCli,
    // The engine's OPERATIONAL state home (the canonical parent of its own database, codex round
    // 8) — fenced on every launch, so the fence check must know it (`fence_admit`).
    operational_home: Option<&Path>,
) -> Result<Option<SkillsSnapshot>, SkillsError> {
    let refs = RequiredRefs::of(input);
    let snapshot = match turn {
        Turn::Fresh => match resolve_ladder()? {
            Ladder::Root(s) => {
                fence_admit(&s, operational_home)?;
                Some(s)
            }
            Ladder::Absent => None,
            Ladder::Failed(why) => {
                refuse_plan_on_failed_ladder(&refs.plan, why)?;
                None
            }
        },
        Turn::Cached(Some(pinned)) => Some(pinned),
        Turn::Cached(None) => {
            let mut skills: Vec<String> = refs
                .plan
                .iter()
                .filter(|r| !r.is_empty())
                .map(|r| r.to_string())
                .collect();
            skills.sort();
            skills.dedup();
            if skills.is_empty() {
                return Ok(None);
            }
            return Err(SkillsError::NotDelivered {
                cli: cli.to_string(),
                skills,
            });
        }
    };
    admit_refs(snapshot, &refs, cli)
}

/// The fence check at admission (v3.1 §1), before any process starts and on both carriers.
///
/// A PUBLISHED snapshot that lies under a tree the worker Read fence denies — anywhere but the
/// one read slot, `<state home>/skills/snapshots/<gen>/` — is a config error naming both paths:
/// Claude's deny beats any allow, so the worker could load the plugin and never read a file in
/// it, and the engine never widens the fence to fix that. A snapshot IN the slot is admitted only
/// when the state home's entries are all classified by the fence registry
/// (`execute_wrapped::fence_check` → `state_home`); an unclassified entry refuses the launch by
/// name rather than leaving it unfenced.
///
/// The LIVE-CACHE fallback always sits under the daemon's claude config dir, which the fence
/// denies: that is not a config error (day one has no snapshot yet, and refusing every run until
/// one is published is the fail-open ladder's opposite mistake), but it is said out loud — the
/// plugin loads, its support files stay unreadable to the worker's file tools, and the fix is to
/// publish a snapshot.
fn fence_admit(
    snapshot: &SkillsSnapshot,
    operational_home: Option<&Path>,
) -> Result<(), SkillsError> {
    match crate::execute_wrapped::fence_check(&snapshot.root, operational_home) {
        Ok(()) => Ok(()),
        Err(why) if snapshot.source == SnapshotSource::Published => Err(SkillsError::Config {
            var: SKILLS_SNAPSHOT_ENV,
            path: snapshot.root.clone(),
            why,
        }),
        Err(why) => {
            eprintln!(
                "[wicked-core] skills.notice the {} at {} is inside the worker Read fence ({why}); \
                 the plugin loads, but the worker's file tools cannot read its support files — \
                 publish a snapshot to hand workers a readable root",
                snapshot.source,
                snapshot.root.display()
            );
            Ok(())
        }
    }
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

    /// `<base>/skills/snapshots/<gen>` — the ONE shape a published generation has; its state
    /// home is `base`, derived three components up (`state_home::of_snapshot`). Every fixture
    /// snapshot is spelled this way: a root anywhere else is a config error at load, and a
    /// fixture whose state home holds an unclassified entry (a worktree, a ledger) is refused at
    /// admission — so a test that launches keeps such files OUTSIDE its snapshot's `base`.
    pub(crate) fn gen_dir(base: &Path, gen: &str) -> PathBuf {
        base.join(super::SKILLS_DIR).join("snapshots").join(gen)
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

    /// The bundle hash the fixture's `snapshot.json` records as `gardenSource.baseline` — a
    /// sha256-shaped 64-hex value (review pass 11: the recorded baseline authorizes the `.venv`
    /// link, so a fixture that plants one must target THIS hash).
    pub(crate) fn fixture_baseline() -> String {
        format!("{:0>64}", "ab12")
    }

    /// The `gardenSource` crew writes into every `snapshot.json` (`SnapshotManifest`), as a
    /// fixture value — required at load (codex round 6).
    pub(crate) fn garden_source() -> serde_json::Value {
        serde_json::json!({
            "kind": "directory",
            "path": "/fixture/garden",
            "plugin_version": "0.0.0",
            "baseline": fixture_baseline()
        })
    }

    /// A complete `snapshot.json` for `gen` over `skills` (JSON rows) — the required identity
    /// fields (`gen`, `contentHash`, `gardenSource`, `venv: skipped` — no env, no link) plus the
    /// rows — for tests that hand-write an index to say exactly what is wrong with ITS rows.
    pub(crate) fn index_json(gen: &str, skills: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "gen": gen,
            "contentHash": format!("sha256:{gen}"),
            "gardenSource": garden_source(),
            "venv": "skipped",
            "skills": skills
        }))
        .unwrap()
    }

    /// Rewrite ONE top-level field of the fixture's `snapshot.json` at `root` (`venv`, say) —
    /// the engine does not re-hash the metadata, so a test can state the recorded env state.
    /// `#[cfg(unix)]`: its only caller is the symlink (`.venv`) section of the parity test, so on
    /// Windows it would be dead code under `-D warnings`.
    #[cfg(unix)]
    pub(crate) fn set_index_field(root: &Path, key: &str, value: serde_json::Value) {
        let path = root.join(super::SNAPSHOT_INDEX);
        let mut index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        index[key] = value;
        std::fs::write(&path, serde_json::to_vec(&index).unwrap()).unwrap();
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
                    "name": name, "dir": format!("{}/{dir}", super::SKILLS_DIR),
                    "kind": "fork-worker", "core": false, "portable": portable,
                    "nested": dir.contains('/')
                })
            })
            .collect();
        std::fs::write(
            root.join(super::SNAPSHOT_INDEX),
            index_json(gen, serde_json::Value::Array(entries)),
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

    fn published(root: &Path) -> Result<Option<SkillsSnapshot>, SkillsError> {
        resolve_in(Some(root.to_path_buf()), None, None, &mut |_| {})
    }

    /// The admission entry points with NO operational state home (the tests here fence nothing
    /// but the defaults); shadows the glob import.
    fn admit_turn(
        turn: Turn,
        input: &StepInput,
        cli: &WorkerCli,
    ) -> Result<Option<SkillsSnapshot>, SkillsError> {
        super::admit_turn(turn, input, cli, None)
    }

    /// The explicit path is loaded from its index: gen, hash, and the skills keyed by their
    /// frontmatter `name` ONLY (codex round 6 — rounds 1–5 also resolved a ref by the dir-derived
    /// name, giving a divergent skill two identities; now a `name` that is not what its directory
    /// derives is a config error at load naming both). The Claude identity is the top-level
    /// DIRECTORY (what Claude Code's plugin loader exposes), and a nested skill has none — its
    /// identity is not invented. The reported generation is the DIRECTORY's name, verified equal
    /// to the index's claim (a zero-padded directory reports its padded name).
    #[test]
    fn an_explicit_snapshot_is_indexed_and_refs_resolve_by_frontmatter_name_only() {
        let base = scratch("published");
        let root = snapshot_root(
            &gen_dir(&base, "7"),
            "7",
            &[
                ("domain", "wicked-garden-domain"),
                ("engineering/frontend", "wicked-garden-engineering-frontend"),
                ("qe-oracle", "wicked-garden-qe-oracle"),
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
        assert_eq!(
            s.state_home.as_deref(),
            Some(base.as_path()),
            "the state home is the directory three components above the generation"
        );
        assert_eq!(s.skills().len(), 3);
        assert_eq!(s.skill("wicked-garden-domain").unwrap().dir, "domain");
        assert!(s.skill("wicked-garden-domain").unwrap().portable);
        assert_eq!(
            s.skill("wicked-garden-qe-oracle").unwrap().dir,
            "qe-oracle",
            "by frontmatter name"
        );
        assert!(
            s.skill("wicked-garden-test-oracle").is_none(),
            "no second identity: a name the index does not carry resolves to nothing"
        );
        assert_eq!(
            s.claude_skill_dir("wicked-garden-qe-oracle").as_deref(),
            Some("qe-oracle"),
            "Claude's id is the directory"
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

        // The reported generation is the verified DIRECTORY name — crew zero-pads it and writes
        // the number in the index; the two are the same generation.
        let padded = snapshot_root(&gen_dir(&base, "000009"), "9", &[]);
        let s = published(&padded).unwrap().unwrap();
        assert_eq!(s.gen.as_deref(), Some("000009"));
        assert_eq!(s.gen_label(), "gen=000009");

        // A DIVERGENT frontmatter/index name (the round-5 alias case) is a defect at load, naming
        // the skill, its directory and the name the directory derives — never an alias.
        let divergent = snapshot_root(
            &gen_dir(&base.join("divergent"), "8"),
            "8",
            &[
                ("domain", "wicked-garden-domain"),
                ("qe-oracle", "wicked-garden-test-oracle"),
            ],
        );
        let err = published(&divergent).expect_err("a divergent name is not an alias");
        let SkillsError::Config { why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert!(
            why.contains("`wicked-garden-test-oracle` at skills/qe-oracle")
                && why.contains("path-derived name `wicked-garden-qe-oracle`")
                && !why.contains("wicked-garden-domain"),
            "{why}"
        );
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
        // A valid plugin root whose path is NOT `<state home>/skills/snapshots/<gen>` (codex
        // round 3): the fence has no state home to derive, so the load is a config error naming
        // the shape — judged last, after the root itself has been found sound.
        let shapeless = snapshot_root(&base.join("elsewhere").join("7"), "7", &[]);
        expect_err(&shapeless, "skills/snapshots/<gen>");
        let bad = snapshot_root(
            &gen_dir(&base.join("bad"), "3"),
            "3",
            &[("core", "wicked-garden-core")],
        );
        std::fs::write(
            bad.join(SNAPSHOT_INDEX),
            index_json("3", serde_json::json!([{"name": "x"}])),
        )
        .unwrap();
        expect_err(&bad, "skills[0] has no string `dir`");
        std::fs::write(
            bad.join(SNAPSHOT_INDEX),
            index_json(
                "3",
                serde_json::json!([{"name": "wicked-garden-core", "dir": "skills/core"}]),
            ),
        )
        .unwrap();
        expect_err(&bad, "no boolean `portable`");
        // crew spells a row's `dir` plugin-relative (`skills/<dir>`); a row without the prefix
        // was not written by crew (review pass 7 — the loader used to read it as already relative
        // to `skills/`, which no real generation is).
        std::fs::write(
            bad.join(SNAPSHOT_INDEX),
            index_json(
                "3",
                serde_json::json!([{"name": "wicked-garden-core", "dir": "core", "portable": true}]),
            ),
        )
        .unwrap();
        expect_err(&bad, "dir `core` is not plugin-relative");
        std::fs::write(bad.join(SNAPSHOT_INDEX), "{\"skills\":[]}").unwrap();
        expect_err(&bad, "`gen`");
        std::fs::write(bad.join(SNAPSHOT_INDEX), "not json").unwrap();
        expect_err(&bad, "not valid JSON");
        // IDENTITY (codex round 6): the index's `gen` must be the generation the directory says
        // (a copied or edited index is refused naming both); `contentHash` and `gardenSource` —
        // with every field crew writes — are required; the directory itself must be a generation
        // name.
        std::fs::write(
            bad.join(SNAPSHOT_INDEX),
            index_json("4", serde_json::json!([])),
        )
        .unwrap();
        expect_err(&bad, "says gen `4` but the directory is `3`");
        std::fs::write(
            bad.join(SNAPSHOT_INDEX),
            format!(
                r#"{{"gen":"3","gardenSource":{},"skills":[]}}"#,
                garden_source()
            ),
        )
        .unwrap();
        expect_err(&bad, "has no `contentHash`");
        std::fs::write(
            bad.join(SNAPSHOT_INDEX),
            r#"{"gen":"3","contentHash":"","gardenSource":{},"skills":[]}"#,
        )
        .unwrap();
        expect_err(
            &bad,
            "`contentHash` is a JSON string, not a non-empty string",
        );
        std::fs::write(
            bad.join(SNAPSHOT_INDEX),
            r#"{"gen":"3","contentHash":"sha256:3","skills":[]}"#,
        )
        .unwrap();
        expect_err(&bad, "has no `gardenSource`");
        std::fs::write(
            bad.join(SNAPSHOT_INDEX),
            r#"{"gen":"3","contentHash":"sha256:3","gardenSource":"garden","skills":[]}"#,
        )
        .unwrap();
        expect_err(&bad, "`gardenSource` is a JSON string, not an object");
        std::fs::write(
            bad.join(SNAPSHOT_INDEX),
            r#"{"gen":"3","contentHash":"sha256:3","gardenSource":{"kind":"directory","path":"","plugin_version":""},"skills":[]}"#,
        )
        .unwrap();
        expect_err(&bad, "`gardenSource` has no `baseline`");
        std::fs::write(
            bad.join(SNAPSHOT_INDEX),
            r#"{"gen":"3","contentHash":"sha256:3","gardenSource":{"kind":"directory","path":"","plugin_version":"","baseline":""},"skills":[]}"#,
        )
        .unwrap();
        expect_err(&bad, "`gardenSource.baseline` is empty");
        // (review pass 11) The recorded baseline must be a sha256 content hash — it authorizes
        // the `.venv` link — and `venv` is required, one of crew's four states.
        let hex = fixture_baseline();
        let with_source = |baseline: &str, venv: &str| {
            format!(
                r#"{{"gen":3,"contentHash":"sha256:3","gardenSource":{{"kind":"directory","path":"","plugin_version":"","baseline":"{baseline}"}}{venv},"skills":[{{"name":"wicked-garden-core","dir":"skills/core","portable":true}}]}}"#
            )
        };
        std::fs::write(bad.join(SNAPSHOT_INDEX), with_source("b", "")).unwrap();
        expect_err(
            &bad,
            "`gardenSource.baseline` `b` is not a sha256 content hash",
        );
        std::fs::write(bad.join(SNAPSHOT_INDEX), with_source(&hex, "")).unwrap();
        expect_err(&bad, "has no `venv`");
        std::fs::write(
            bad.join(SNAPSHOT_INDEX),
            with_source(&hex, r#","venv":"maybe""#),
        )
        .unwrap();
        expect_err(
            &bad,
            "`venv` is `maybe`, not one of pending|synced|failed|skipped",
        );
        std::fs::write(bad.join(SNAPSHOT_INDEX), with_source(&hex, r#","venv":7"#)).unwrap();
        expect_err(&bad, "`venv` is a JSON number, not a string");
        // Empty `path` / `plugin_version` are what crew writes for a source without them: fine
        // (the row for the fixture's one skill rides along — the tree and the index must agree).
        std::fs::write(
            bad.join(SNAPSHOT_INDEX),
            with_source(&hex, r#","venv":"skipped""#),
        )
        .unwrap();
        let loaded = published(&bad).unwrap().unwrap();
        assert_eq!(loaded.gen.as_deref(), Some("3"));
        assert_eq!(loaded.baseline.as_deref(), Some(hex.as_str()));
        assert_eq!(loaded.venv, Some(VenvState::Skipped));
        let not_a_gen = snapshot_root(
            &base
                .join("named")
                .join(SKILLS_DIR)
                .join("snapshots")
                .join("gen-7"),
            "7",
            &[],
        );
        expect_err(&not_a_gen, "directory `gen-7` is not a generation name");
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
            &gen_dir(&base, "5"),
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
            &gen_dir(&base.join("dup"), "6"),
            "6",
            &[("a", "wicked-garden-a"), ("b", "wicked-garden-b")],
        );
        std::fs::write(
            dup.join(SNAPSHOT_INDEX),
            index_json(
                "6",
                serde_json::json!([
                    {"name": "wicked-garden-a", "dir": "skills/a", "portable": true},
                    {"name": "wicked-garden-a", "dir": "skills/b", "portable": true},
                    {"name": "wicked-garden-c", "dir": "skills/../escape", "portable": true}
                ]),
            ),
        )
        .unwrap();
        let err = published(&dup).expect_err("duplicate + unclean");
        let SkillsError::Config { why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert!(why.contains("duplicate name `wicked-garden-a`"), "{why}");
        assert!(why.contains("not a clean relative"), "{why}");
        assert!(
            why.contains(
                "`wicked-garden-a` at skills/b is not the path-derived name `wicked-garden-b`"
            ),
            "the duplicate is also a divergent identity: {why}"
        );
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
            &gen_dir(&base, "8"),
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

        // The walk covers EVERY component (codex round 2): `skills/` itself as a link to an
        // outside tree whose descendants are regular files with the right names is refused AT
        // `skills`, before anything under it is consulted.
        let linked = snapshot_root(
            &gen_dir(&base.join("linked"), "9"),
            "9",
            &[
                ("domain", "wicked-garden-domain"),
                ("mem", "wicked-garden-mem"),
            ],
        );
        let elsewhere = base.join("elsewhere");
        write_skill(&elsewhere, "domain", "wicked-garden-domain");
        write_skill(&elsewhere, "mem", "wicked-garden-mem");
        std::fs::remove_dir_all(linked.join(SKILLS_DIR)).unwrap();
        std::os::unix::fs::symlink(elsewhere.join(SKILLS_DIR), linked.join(SKILLS_DIR)).unwrap();
        let err = published(&linked).expect_err("skills -> /outside");
        let SkillsError::Config { why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert!(
            why.contains("skills is a symlink") && !why.contains("is missing"),
            "refused at the skills component itself: {why}"
        );
        // `snapshot.json` as a link (to a valid index elsewhere) is refused, not followed.
        let idx_linked = snapshot_root(
            &gen_dir(&base.join("idx"), "10"),
            "10",
            &[("domain", "wicked-garden-domain")],
        );
        let real_index = base.join("real-index.json");
        std::fs::rename(idx_linked.join(SNAPSHOT_INDEX), &real_index).unwrap();
        std::os::unix::fs::symlink(&real_index, idx_linked.join(SNAPSHOT_INDEX)).unwrap();
        let err = published(&idx_linked).expect_err("snapshot.json -> elsewhere");
        let SkillsError::Config { why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert!(why.contains("snapshot.json is a symlink"), "{why}");
        // …and so is `.claude-plugin/` (the manifest dir) — a linked manifest is a containment
        // defect, not "no manifest".
        let man_linked = snapshot_root(
            &gen_dir(&base.join("man"), "11"),
            "11",
            &[("domain", "wicked-garden-domain")],
        );
        let real_manifest_dir = base.join("real-claude-plugin");
        std::fs::rename(man_linked.join(".claude-plugin"), &real_manifest_dir).unwrap();
        std::os::unix::fs::symlink(&real_manifest_dir, man_linked.join(".claude-plugin")).unwrap();
        let err = published(&man_linked).expect_err(".claude-plugin -> elsewhere");
        let SkillsError::Config { why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert!(why.contains(".claude-plugin is a symlink"), "{why}");
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
        let picked = resolve_in(None, Some(config.clone()), None, &mut collect(&mut lines))
            .unwrap()
            .expect("the live cache is a root");
        assert_eq!(picked.source, SnapshotSource::LiveCache);
        assert_eq!(picked.state_home, None, "a fallback root has no state home");
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
        let none = resolve_in(None, Some(config.clone()), None, &mut collect(&mut lines)).unwrap();
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
        let picked = resolve_in(None, None, Some(home.clone()), &mut |_| {})
            .unwrap()
            .unwrap();
        assert_eq!(picked.root, home_cache);

        // Numeric `gen` in a published index (what crew writes): the same generation as the
        // directory loads and reports the directory's name; another generation is refused (codex
        // round 6 — the reported generation is the verified one, never the index's claim).
        let numeric = snapshot_root(&gen_dir(&base.join("num"), "9"), "9", &[]);
        let with_gen = |gen: u64| {
            format!(
                r#"{{"gen":{gen},"contentHash":"sha256:9","gardenSource":{},"venv":"skipped","skills":[]}}"#,
                garden_source()
            )
        };
        std::fs::write(numeric.join(SNAPSHOT_INDEX), with_gen(9)).unwrap();
        let s = published(&numeric).unwrap().unwrap();
        assert_eq!(s.gen.as_deref(), Some("9"));
        std::fs::write(numeric.join(SNAPSHOT_INDEX), with_gen(12)).unwrap();
        let err = published(&numeric).expect_err("the index claims another generation");
        let SkillsError::Config { why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert!(
            why.contains("says gen `12` but the directory is `9`"),
            "{why}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The ONE input (v3.1 §2, v3.4 §2): a variable that is SET BUT EMPTY is invalid explicit
    /// configuration — a config error, not "unset" — and neither withdrawn companion is read at
    /// all: `WICKED_SKILLS_CURRENT` (pass 1's second rung) and `WICKED_CREW_STATE_HOME` (round 4's
    /// "passed alongside" state home, retired by v3.4 §2) set to anything, empty included, neither
    /// steer the ladder nor break it — the state home is derived from the snapshot path alone.
    #[test]
    fn an_empty_explicit_value_is_a_config_error_not_unset_and_there_is_no_second_input() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        const WITHDRAWN: &str = "WICKED_SKILLS_CURRENT";
        const RETIRED_STATE_HOME: &str = "WICKED_CREW_STATE_HOME";
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            [SKILLS_SNAPSHOT_ENV, WITHDRAWN, RETIRED_STATE_HOME]
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect();
        std::env::remove_var(SKILLS_SNAPSHOT_ENV);
        std::env::remove_var(RETIRED_STATE_HOME);
        std::env::set_var(SKILLS_SNAPSHOT_ENV, "");
        let err = env_path(SKILLS_SNAPSHOT_ENV).expect_err("empty is not unset");
        let SkillsError::Config { var: v, why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert_eq!(*v, SKILLS_SNAPSHOT_ENV);
        assert!(why.contains("set but empty"), "{why}");
        assert!(
            resolve().is_err(),
            "the ladder itself refuses an empty {SKILLS_SNAPSHOT_ENV}"
        );
        // The withdrawn input: an EMPTY value used to error before explicit-path precedence was
        // applied (codex round 2); now a valid explicit snapshot loads regardless of it.
        let base = scratch("one-input");
        let root = snapshot_root(
            &gen_dir(&base, "2"),
            "2",
            &[("domain", "wicked-garden-domain")],
        );
        std::env::set_var(SKILLS_SNAPSHOT_ENV, &root);
        std::env::set_var(WITHDRAWN, "");
        let s = resolve()
            .expect("the withdrawn variable cannot break a valid explicit snapshot")
            .expect("a root");
        assert_eq!(s.root, root);
        std::env::set_var(WITHDRAWN, base.join("nowhere"));
        assert_eq!(resolve().unwrap().unwrap().root, root);

        // The RETIRED state-home variable (v3.4 §2; codex round 6 — round 3 made a set-but-empty,
        // relative, unresolvable or DISAGREEING value a config error): it is not read. Set to
        // nothing, to a relative spelling, to a directory that is not the snapshot's state home,
        // the snapshot still loads and its state home is still the one DERIVED from its path.
        let other = base.join("other-state");
        std::fs::create_dir_all(&other).unwrap();
        for value in ["", "relative/state", &other.display().to_string()] {
            std::env::set_var(RETIRED_STATE_HOME, value);
            let s = resolve()
                .unwrap_or_else(|e| panic!("{RETIRED_STATE_HOME}={value:?} must not be read: {e}"))
                .expect("a root");
            assert_eq!(s.root, root);
            assert_eq!(
                s.state_home.as_deref(),
                Some(base.as_path()),
                "the state home is derived from the snapshot path alone"
            );
        }
        // And with no snapshot handed it steers nothing either: the fallback rung is unchanged.
        std::env::remove_var(SKILLS_SNAPSHOT_ENV);
        std::env::set_var(RETIRED_STATE_HOME, &other);
        let mut lines = Vec::new();
        let none = resolve_in(
            None,
            Some(base.join("no-config")),
            None,
            &mut collect(&mut lines),
        )
        .unwrap();
        assert!(none.is_none(), "{none:?}");
        assert_eq!(lines.len(), 1, "{lines:?}");
        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An explicit path whose FINAL component is a symlink — crew's `current -> snapshots/<gen>` —
    /// is REFUSED (design v3.4 §2; codex round 6): every component including the last must be a
    /// real directory, and crew resolves `current` before the handoff. Rounds 1–5 followed the
    /// link once at load, which let two fresh launches of one run land on two generations across
    /// a publish. The refusal names the link's target and the real path to pass; the CONCRETE
    /// generation path loads, and after crew publishes the next generation and re-exports the
    /// concrete path, that one loads while the earlier snapshot still names its own generation.
    /// An ANCESTOR symlink is refused with the real path to pass instead; a loop and a dangling
    /// link are refused as links too (config errors, never a hang, never a follow).
    #[cfg(unix)]
    #[test]
    fn a_current_link_is_refused_and_the_concrete_generation_path_is_what_loads() {
        let base = scratch("pin");
        let gen7 = snapshot_root(
            &gen_dir(&base, "7"),
            "7",
            &[("domain", "wicked-garden-domain")],
        );
        let gen8 = snapshot_root(
            &gen_dir(&base, "8"),
            "8",
            &[("domain", "wicked-garden-domain")],
        );
        // crew's `current` lives beside `snapshots/` in the skills root.
        let skills = base.join(SKILLS_DIR);
        let current = skills.join("current");
        std::os::unix::fs::symlink("snapshots/7", &current).unwrap();

        let err = published(&current).expect_err("a handed `current` link is refused");
        let SkillsError::Config { path, why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert_eq!(path, &current, "the error names the path as given");
        assert!(
            why.contains("it is a symlink to `snapshots/7`")
                && why.contains("the last included")
                && why.contains("crew resolves `current` before the handoff")
                && why.contains(&format!("pass the real path `{}`", gen7.display())),
            "names the link, its target and the concrete path to pass: {why}"
        );

        // The concrete generation loads; after crew publishes gen 8 and re-exports the concrete
        // path, THAT loads — the snapshot loaded earlier still names its own generation.
        let first = published(&gen7).unwrap().unwrap();
        assert_eq!(first.root, gen7);
        assert_eq!(first.gen.as_deref(), Some("7"));
        std::fs::remove_file(&current).unwrap();
        std::os::unix::fs::symlink(&gen8, &current).unwrap();
        assert!(
            published(&current).is_err(),
            "an absolute link target is refused all the same"
        );
        let second = published(&gen8).unwrap().unwrap();
        assert_eq!(second.root, gen8);
        assert_eq!(second.gen.as_deref(), Some("8"));
        assert_eq!(first.root, gen7);
        assert_ne!(first.root, second.root);

        // An ancestor link: `<skills>/linked-snapshots -> snapshots`, then `.../linked-snapshots/7`.
        let linked = skills.join("linked-snapshots");
        std::os::unix::fs::symlink(skills.join("snapshots"), &linked).unwrap();
        let err = published(&linked.join("7")).expect_err("an ancestor symlink");
        let SkillsError::Config { path, why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert_eq!(path, &linked.join("7"), "the error names the path as given");
        assert!(
            why.contains("ancestor") && why.contains(&gen7.display().to_string()),
            "names the link and the real path to pass: {why}"
        );

        // A loop: the final component is a link, so it is refused as one (never followed, never
        // a hang) — the config error names the path the operator set.
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
        assert!(
            why.contains("it is a symlink to") && why.contains("dangles"),
            "{why}"
        );

        // A DANGLING link — crew's `current` aimed at a reaped or unpublished generation: refused
        // as a link, naming the target it points at and that there is no generation behind it.
        let dangling = base.join("current-dangling");
        std::os::unix::fs::symlink("snapshots/99", &dangling).unwrap();
        assert!(std::fs::symlink_metadata(&dangling).is_ok());
        let err = published(&dangling).expect_err("a dangling link is not a snapshot");
        let SkillsError::Config { path, why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert_eq!(path, &dangling);
        assert!(
            why.contains("snapshots/99") && why.contains("dangles"),
            "names the missing target: {why}"
        );
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
        let root = snapshot_root(&gen_dir(&base, "21"), "21", &[]);
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
        )
        .unwrap();
        let json = live
            .handed_event("run-1", 3, 1, "wrapped_cli", "claude")
            .to_json();
        assert!(json["gen"].is_null() && json["contentHash"].is_null());
        assert_eq!(json["source"], "live-cache");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A run naming skills the snapshot does not hold is REFUSED with exactly those names (sorted,
    /// deduplicated); a ref of ANOTHER FAMILY is judged the same way — refused when absent, with
    /// the message saying why no snapshot can hold it (every published skill is keyed
    /// `wicked-garden-<dir>`, codex round 6 — a foreign-named entry is a load-time defect, as it
    /// is at crew's publish); with no root at all every ref is missing while a skill-less run is
    /// admitted.
    #[test]
    fn missing_required_skills_are_refused_by_name_whatever_their_family() {
        let base = scratch("admit");
        // A foreign-family entry cannot be published: crew requires `name == wicked-garden-<dir>`
        // and so does the loader — a user-added skill of another family is a defect naming it.
        let foreign = snapshot_root(
            &gen_dir(&base.join("foreign"), "3"),
            "3",
            &[
                ("domain", "wicked-garden-domain"),
                (
                    "acceptance-test-writer",
                    "wicked-testing-acceptance-test-writer",
                ),
            ],
        );
        let err = published(&foreign).expect_err("a foreign-named entry is not an identity");
        let SkillsError::Config { why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert!(
            why.contains(
                "`wicked-testing-acceptance-test-writer` at skills/acceptance-test-writer is not \
                 the path-derived name `wicked-garden-acceptance-test-writer`"
            ),
            "{why}"
        );
        let root = snapshot_root(
            &gen_dir(&base, "4"),
            "4",
            &[
                ("domain", "wicked-garden-domain"),
                (
                    "acceptance-test-writer",
                    "wicked-garden-acceptance-test-writer",
                ),
            ],
        );
        let snapshot = load(&root);
        let claude = WorkerCli::Claude;

        // Both present ⇒ admitted.
        let ok = admit_refs(
            Some(snapshot.clone()),
            &RequiredRefs::seat([
                "wicked-garden-domain",
                "wicked-garden-acceptance-test-writer",
            ]),
            &claude,
        )
        .expect("both are present");
        assert_eq!(ok.as_ref().map(|s| &s.root), Some(&root));

        // A foreign-family ref is refused — there is no exemption by family — and the message
        // says why no snapshot can hold it under that name.
        let err = admit_refs(
            Some(snapshot.clone()),
            &RequiredRefs::seat(["wicked-garden-domain", "wicked-testing-plan"]),
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
                && msg.contains("can never resolve in it")
                && msg.contains("fix the workflow's skill_ref"),
            "the refusal says why no snapshot holds it by that name: {msg}"
        );

        // Plan-wide: a skill only ANOTHER seat invokes must still exist (the seat here names one
        // skill; the plan names two more, of which two are missing).
        let err = admit_refs(
            Some(snapshot.clone()),
            &RequiredRefs::plan_and_seat(
                [
                    "wicked-garden-domain-coverage",
                    "wicked-garden-domain-extractor",
                    "wicked-garden-domain-coverage",
                ],
                ["wicked-garden-domain"],
            ),
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

        let err = admit_refs(None, &RequiredRefs::seat(["wicked-garden-domain"]), &claude)
            .expect_err("no root");
        assert_eq!(
            err,
            SkillsError::Missing {
                root: None,
                missing: vec!["wicked-garden-domain".to_string()],
            }
        );
        assert!(err.to_string().contains(SKILLS_SNAPSHOT_ENV));
        assert!(
            admit_refs(None, &RequiredRefs::seat([]), &claude)
                .unwrap()
                .is_none(),
            "a run that names no skill proceeds without a root"
        );
        assert!(
            admit_refs(None, &RequiredRefs::seat([""]), &claude)
                .unwrap()
                .is_none(),
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
            &gen_dir(&base, "10"),
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
            .retain(|e| e["dir"] != "skills/core");
        std::fs::write(
            root.join(SNAPSHOT_INDEX),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
        let s = load(&root);
        let err = admit_refs(
            Some(s),
            &RequiredRefs::seat(["wicked-garden-repo-learn"]),
            &WorkerCli::Claude,
        )
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
            &gen_dir(&base.join("idx"), "11"),
            "11",
            &[("a", "wicked-garden-a"), ("b", "wicked-garden-b")],
        );
        std::fs::write(
            idx.join(SNAPSHOT_INDEX),
            index_json(
                "11",
                serde_json::json!([
                    {"name": "wicked-garden-a", "dir": "skills/a", "portable": true,
                     "mandates": ["wicked-garden-b"]},
                    {"name": "wicked-garden-b", "dir": "skills/b", "portable": true}
                ]),
            ),
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
            &gen_dir(&base, "12"),
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
        // pi has a per-launch lever (`--no-skills --skill …`); codex has none (v3.2) — the
        // portability rules are exercised on pi, the lever rule on codex below.
        let (claude, pi) = (WorkerCli::Claude, WorkerCli::for_binaries("pi", "pi", "pi"));
        let codex = WorkerCli::for_binaries("codex", "codex", "codex");
        assert_eq!(pi.lever(), SkillsLever::PiSkillFlags);
        assert_eq!(codex.lever(), SkillsLever::Absent);

        // Nested: refused for Claude (directly AND through a mandate), admitted for pi.
        let err = admit_refs(
            Some(s.clone()),
            &RequiredRefs::seat(["wicked-garden-engineering-frontend"]),
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
                admit_refs(
                    Some(s.clone()),
                    &RequiredRefs::seat(["wicked-garden-engineering"]),
                    &claude
                ),
                Err(SkillsError::NotInvocable { .. })
            ),
            "a top-level skill that MANDATES a nested one is refused for claude too"
        );
        assert!(admit_refs(
            Some(s.clone()),
            &RequiredRefs::seat(["wicked-garden-engineering-frontend"]),
            &pi
        )
        .unwrap()
        .is_some());

        // Non-portable: refused for pi, admitted for Claude.
        let err = admit_refs(
            Some(s.clone()),
            &RequiredRefs::seat(["wicked-garden-domain", "wicked-garden-domain-extractor"]),
            &pi,
        )
        .expect_err("non-portable on pi");
        assert_eq!(
            err,
            SkillsError::NotPortable {
                root: root.clone(),
                cli: "pi".to_string(),
                skills: vec!["wicked-garden-domain-extractor".to_string()],
            }
        );
        assert!(err.to_string().contains("'pi'"), "{err}");
        assert!(admit_refs(
            Some(s.clone()),
            &RequiredRefs::seat(["wicked-garden-domain", "wicked-garden-domain-extractor"]),
            &claude
        )
        .unwrap()
        .is_some());

        // v3.1 §5 — existence is plan-wide, invocability is seat-specific. A MIXED-CLI plan: the
        // Codex unit invokes `domain` (portable) while a Claude unit elsewhere in the plan needs
        // the non-portable `domain-extractor` — the Codex unit is ADMITTED (the extractor must
        // exist, and does; it is not judged for codex).
        let mixed = RequiredRefs::plan_and_seat(
            ["wicked-garden-domain-extractor", "wicked-garden-domain"],
            ["wicked-garden-domain"],
        );
        assert!(
            admit_refs(Some(s.clone()), &mixed, &pi).unwrap().is_some(),
            "a non-portable skill another seat invokes does not refuse the pi unit"
        );
        // …and a Claude unit invoking `domain` while a mirror seat uses the NESTED
        // `engineering/frontend` is admitted too: the nested skill exists, and only the Claude
        // seat's own invocation is judged for Claude's one-directory-deep discovery.
        let mixed = RequiredRefs::plan_and_seat(
            ["wicked-garden-engineering-frontend", "wicked-garden-domain"],
            ["wicked-garden-domain"],
        );
        assert!(
            admit_refs(Some(s.clone()), &mixed, &claude)
                .unwrap()
                .is_some(),
            "a nested skill another seat invokes does not refuse the claude unit"
        );
        // Existence stays plan-wide: a plan naming a skill NO seat here invokes, and the root
        // lacks, is refused for every seat.
        let missing =
            RequiredRefs::plan_and_seat(["wicked-garden-absent"], ["wicked-garden-domain"]);
        assert!(matches!(
            admit_refs(Some(s.clone()), &missing, &pi),
            Err(SkillsError::Missing { .. })
        ));

        // v3.2 §3 — no lever ⇒ no skills, never a side channel: a codex unit that INVOKES a skill
        // is refused naming the skill and the reason; one that names none runs (the snapshot is
        // handed for the record, the delivery is empty); a codex unit in a plan where OTHER seats
        // invoke skills is admitted (existence is plan-wide, the lever rule is seat-specific).
        let err = admit_refs(
            Some(s.clone()),
            &RequiredRefs::seat(["wicked-garden-domain"]),
            &codex,
        )
        .expect_err("codex has no lever");
        let SkillsError::NoLever { cli, skills, why } = &err else {
            panic!("expected NoLever, got {err:?}");
        };
        assert_eq!(cli, "codex");
        assert_eq!(skills, &vec!["wicked-garden-domain".to_string()]);
        assert!(why.contains("no per-launch skills lever"), "{why}");
        assert!(
            err.to_string()
                .contains("never through a CLI's own skills directory"),
            "{err}"
        );
        let handed = admit_refs(Some(s.clone()), &RequiredRefs::seat([]), &codex)
            .unwrap()
            .expect("the root is still handed for the record");
        assert_eq!(handed.delivery(&codex), SkillsDelivery::None);
        assert!(admit_refs(
            Some(s.clone()),
            &RequiredRefs::plan_and_seat(["wicked-garden-domain"], []),
            &codex
        )
        .unwrap()
        .is_some());
        // copilot's lever needs the generation to publish `views/copilot`; without it the unit is
        // refused naming the view. WITH one, the view is VERIFIED for what the seat invokes (codex
        // round 3 — pass 2 admitted an EMPTY view): an empty view is `Missing` naming the skill,
        // a partial view names only what it lacks, a copy whose frontmatter `name` disagrees is
        // not that skill, and a complete view is admitted with `--add-dir <view>` as the delivery.
        let copilot = WorkerCli::for_binaries("copilot", "copilot", "copilot");
        let err = admit_refs(
            Some(s.clone()),
            &RequiredRefs::seat(["wicked-garden-domain"]),
            &copilot,
        )
        .expect_err("no copilot view in this generation");
        assert!(
            matches!(&err, SkillsError::NoLever { why, .. } if why.contains("views/copilot")),
            "{err:?}"
        );
        let view = root.join("views").join("copilot");
        let view_skills = view.join(".github").join("skills");
        std::fs::create_dir_all(&view_skills).unwrap();
        let s = load(&root);
        let err = admit_refs(
            Some(s.clone()),
            &RequiredRefs::seat(["wicked-garden-domain"]),
            &copilot,
        )
        .expect_err("an empty view holds nothing the seat invokes");
        assert_eq!(
            err,
            SkillsError::Missing {
                root: Some(view.clone()),
                missing: vec!["wicked-garden-domain".to_string()],
            }
        );
        assert!(
            err.to_string().contains(&view.display().to_string()),
            "{err}"
        );
        let copy = |name: &str, fm_name: &str| {
            let d = view_skills.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(SKILL_FILE), format!("---\nname: {fm_name}\n---\n")).unwrap();
        };
        copy("wicked-garden-domain", "wicked-garden-domain");
        // Partial: only what the view lacks is named.
        let both =
            RequiredRefs::seat(["wicked-garden-domain", "wicked-garden-engineering-frontend"]);
        let err = admit_refs(Some(s.clone()), &both, &copilot).expect_err("a partial view");
        assert_eq!(
            err,
            SkillsError::Missing {
                root: Some(view.clone()),
                missing: vec!["wicked-garden-engineering-frontend".to_string()],
            }
        );
        // A copy whose frontmatter disagrees is not that skill — and (v3.3 §2) a view entry that
        // is not the skill its directory says it is refuses the launch as a CONFIG error naming
        // the mismatch, not merely as a missing skill: the view was published wrong.
        copy(
            "wicked-garden-engineering-frontend",
            "wicked-garden-something-else",
        );
        let err = admit_refs(Some(s.clone()), &both, &copilot).expect_err("name mismatch");
        assert!(
            matches!(&err, SkillsError::Config { why, .. }
                if why.contains("wicked-garden-engineering-frontend/SKILL.md declares name \
                                 `wicked-garden-something-else`, not \
                                 `wicked-garden-engineering-frontend`")),
            "{err:?}"
        );
        copy(
            "wicked-garden-engineering-frontend",
            "wicked-garden-engineering-frontend",
        );
        let handed = admit_refs(Some(s.clone()), &both, &copilot)
            .unwrap()
            .unwrap();
        assert_eq!(
            handed.delivery(&copilot),
            SkillsDelivery::CopilotAddDir(view.clone())
        );
        assert_eq!(
            handed.delivery(&copilot).argv_flags(),
            vec!["--add-dir".to_string(), view.to_string_lossy().into_owned()]
        );
        // v3.3 §2 (codex round 4): the WHOLE view is judged, invoked or not. An entry the index
        // does not list, a copy of a non-portable skill, a stray file, and a Claude-only child
        // nested inside a portable parent's copy each refuse by name as a config error — for a
        // unit that invokes nothing too, since the launch would `--add-dir` the whole view.
        let none = RequiredRefs::seat([]);
        assert!(
            admit_refs(Some(s.clone()), &none, &copilot)
                .unwrap()
                .is_some(),
            "a well-formed view admits a unit invoking nothing"
        );
        let refused = |needle: &str| {
            for refs in [&none, &both] {
                let err = admit_refs(Some(s.clone()), refs, &copilot).expect_err(needle);
                assert!(
                    matches!(&err, SkillsError::Config { why, .. } if why.contains(needle)),
                    "{needle}: {err:?}"
                );
            }
        };
        copy("wicked-garden-unknown", "wicked-garden-unknown");
        refused("wicked-garden-unknown is not a skill this snapshot indexes");
        std::fs::remove_dir_all(view_skills.join("wicked-garden-unknown")).unwrap();
        copy(
            "wicked-garden-domain-extractor",
            "wicked-garden-domain-extractor",
        );
        refused(
            "wicked-garden-domain-extractor is `wicked-garden-domain-extractor`, which the index \
             marks portable: false",
        );
        std::fs::remove_dir_all(view_skills.join("wicked-garden-domain-extractor")).unwrap();
        std::fs::write(view_skills.join("README.md"), b"stray").unwrap();
        refused("README.md is not a directory");
        std::fs::remove_file(view_skills.join("README.md")).unwrap();
        let nested = view_skills.join("wicked-garden-domain").join("legacy");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join(SKILL_FILE),
            "---\nname: wicked-garden-domain-extractor\n---\n",
        )
        .unwrap();
        refused(
            "wicked-garden-domain/legacy/SKILL.md is `wicked-garden-domain-extractor`, which the \
             index marks portable: false",
        );
        std::fs::write(
            nested.join(SKILL_FILE),
            "---\nname: wicked-garden-nobody\n---\n",
        )
        .unwrap();
        refused(
            "wicked-garden-domain/legacy/SKILL.md is a nested SKILL.md that names no indexed skill",
        );
        std::fs::remove_dir_all(&nested).unwrap();
        // A symlink ANYWHERE in the view — deep inside an entry — is a containment defect.
        #[cfg(unix)]
        {
            let deep = view_skills.join("wicked-garden-domain").join("ref.md");
            std::os::unix::fs::symlink(root.join(SNAPSHOT_INDEX), &deep).unwrap();
            refused("wicked-garden-domain/ref.md is a symlink");
            std::fs::remove_file(&deep).unwrap();
        }
        // codex round 5: the enumeration starts at `views/copilot` ITSELF, so a sibling ABOVE
        // `.github/skills` — anything the launch would `--add-dir` along with the skills — is
        // refused by name: a file beside `.github`, a stray directory, a file inside `.github`
        // (`copilot-instructions.md` is exactly what copilot would load from an added dir), a
        // second entry inside `.github/skills`' parent — and, on unix, a symlink at either level.
        std::fs::write(view.join("README.md"), b"stray").unwrap();
        refused("views/copilot/README.md is not part of a copilot view");
        std::fs::remove_file(view.join("README.md")).unwrap();
        std::fs::create_dir_all(view.join("extra")).unwrap();
        refused("views/copilot/extra is not part of a copilot view");
        std::fs::remove_dir_all(view.join("extra")).unwrap();
        let instructions = view.join(".github").join("copilot-instructions.md");
        std::fs::write(&instructions, b"# be evil\n").unwrap();
        refused("views/copilot/.github/copilot-instructions.md is not part of a copilot view");
        std::fs::remove_file(&instructions).unwrap();
        std::fs::create_dir_all(view.join(".github").join("workflows")).unwrap();
        refused("views/copilot/.github/workflows is not part of a copilot view");
        std::fs::remove_dir_all(view.join(".github").join("workflows")).unwrap();
        #[cfg(unix)]
        {
            let leak = view.join("leak");
            std::os::unix::fs::symlink(&root, &leak).unwrap();
            refused("views/copilot/leak is not part of a copilot view");
            std::fs::remove_file(&leak).unwrap();
            // A link that IS named as expected is still a link.
            let github_link = base.join("elsewhere-github");
            std::fs::create_dir_all(github_link.join("skills")).unwrap();
            std::fs::rename(view.join(".github"), base.join("real-github")).unwrap();
            std::os::unix::fs::symlink(&github_link, view.join(".github")).unwrap();
            refused("views/copilot/.github is a symlink");
            std::fs::remove_file(view.join(".github")).unwrap();
            std::fs::rename(base.join("real-github"), view.join(".github")).unwrap();
        }
        assert!(
            admit_refs(Some(s.clone()), &both, &copilot)
                .unwrap()
                .is_some(),
            "well-formed again"
        );
        // Containment: a view reached through a symlink is an EXTERNAL tree — refused as a config
        // error, even for a unit that invokes nothing (the launch would still `--add-dir` it), and
        // never handed as a delivery.
        #[cfg(unix)]
        {
            let linked_root = snapshot_root(
                &gen_dir(&base.join("linked"), "13"),
                "13",
                &[("domain", "wicked-garden-domain")],
            );
            let outside = base.join("outside-view");
            let outside_skill = outside
                .join(".github")
                .join("skills")
                .join("wicked-garden-domain");
            std::fs::create_dir_all(&outside_skill).unwrap();
            std::fs::write(
                outside_skill.join(SKILL_FILE),
                "---\nname: wicked-garden-domain\n---\n",
            )
            .unwrap();
            std::fs::create_dir_all(linked_root.join("views")).unwrap();
            std::os::unix::fs::symlink(&outside, linked_root.join("views").join("copilot"))
                .unwrap();
            // (codex round 7) refused at LOAD — the tree-parity walk refuses any link in the
            // delivered generation — so no launch of any seat ever sees it; the admission-time
            // view check (`copilot_view_for`) still refuses it for a snapshot struct built before
            // the link appeared.
            let err = published(&linked_root).expect_err("a linked view is an external tree");
            assert!(
                matches!(&err, SkillsError::Config { why, .. }
                    if why.contains("views/copilot is a symlink")),
                "{err:?}"
            );
            std::fs::remove_file(linked_root.join("views").join("copilot")).unwrap();
            let ls = load(&linked_root);
            std::os::unix::fs::symlink(&outside, linked_root.join("views").join("copilot"))
                .unwrap();
            assert!(
                matches!(
                    admit_refs(Some(ls.clone()), &RequiredRefs::seat([]), &copilot),
                    Err(SkillsError::Config { .. })
                ),
                "refused at admission too, even when nothing is invoked"
            );
            assert_eq!(ls.delivery(&copilot), SkillsDelivery::None);
        }
        // pi: discovery OFF, then one --skill per PORTABLE skill (the non-portable extractor is
        // not delivered; nested portable skills are).
        let pi_flags = handed.delivery(&pi).argv_flags();
        assert_eq!(pi_flags[0], "--no-skills");
        let delivered: Vec<&str> = pi_flags
            .windows(2)
            .filter(|w| w[0] == "--skill")
            .map(|w| w[1].as_str())
            .collect();
        let expected = [
            skill_dir(&root, "domain"),
            skill_dir(&root, "engineering"),
            skill_dir(&root, "engineering/frontend"),
        ];
        assert_eq!(
            delivered,
            expected
                .iter()
                .map(|p| p.to_str().unwrap())
                .collect::<Vec<_>>(),
            "{pi_flags:?}"
        );
        // opencode: `skills.paths` composed WITH the governance content, nothing else touched.
        let opencode = WorkerCli::for_binaries("opencode", "opencode", "opencode");
        let composed = handed
            .delivery(&opencode)
            .opencode_config(Some(
                r#"{"$schema":"https://opencode.ai/config.json","permission":{"read":"ask"}}"#,
            ))
            .expect("a JSON object composes")
            .expect("opencode has a lever");
        let doc: Value = serde_json::from_str(&composed).unwrap();
        assert_eq!(
            doc["permission"]["read"], "ask",
            "governance content survives"
        );
        assert_eq!(
            doc["skills"]["paths"].as_array().unwrap().len(),
            3,
            "one path per portable skill: {composed}"
        );
        assert!(
            doc["skills"]["paths"].as_array().unwrap().iter().all(|p| p
                .as_str()
                .unwrap()
                .starts_with(&root.to_string_lossy().to_string())),
            "{composed}"
        );
        assert!(handed
            .delivery(&pi)
            .opencode_config(None)
            .unwrap()
            .is_none());
        let bare: Value = serde_json::from_str(
            &handed
                .delivery(&opencode)
                .opencode_config(None)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(bare["$schema"], "https://opencode.ai/config.json");
        // Fail CLOSED (codex round 3): a governance value that is not a JSON object — or whose
        // `skills` / `skills.paths` cannot take the paths — is an error naming the variable and
        // the reason, never a bare document that drops the seat's governance.
        for (bad, needle) in [
            ("not json", "not valid JSON"),
            ("[1, 2]", "JSON array, not an object"),
            (r#"{"skills": "x"}"#, "skills is not an object"),
            (
                r#"{"skills": {"paths": "x"}}"#,
                "skills.paths is not an array",
            ),
        ] {
            let err = handed
                .delivery(&opencode)
                .opencode_config(Some(bad))
                .expect_err(bad);
            assert!(
                err.contains(OPENCODE_CONFIG_ENV) && err.contains(needle),
                "{bad}: {err}"
            );
        }
        // …while a seat without the opencode lever composes nothing, malformed or not.
        assert_eq!(
            handed.delivery(&pi).opencode_config(Some("not json")),
            Ok(None)
        );
        // The carrier decides the lever: pi's separate ACP bridge forwards no flags.
        assert_eq!(
            WorkerCli::for_binaries("pi", "pi-acp", "pi").lever(),
            SkillsLever::Absent
        );
        assert_eq!(
            WorkerCli::for_binaries("copilot", "copilot", "copilot").lever(),
            SkillsLever::CopilotAddDir
        );
        assert_eq!(
            WorkerCli::for_binaries("claude", "claude-agent-acp", "claude"),
            WorkerCli::Claude
        );
        // `RequiredRefs::of` reads the plan from `required_skills` and the seat from `skill_ref`.
        let mut u = crate::domain::WorkUnit::pending("r:u1", "r", 1, "do");
        u.skill_ref = Some("wicked-garden-domain".to_string());
        let input = StepInput {
            run_id: "r".to_string(),
            unit_ix: 0,
            attempt: 0,
            unit: u,
            workflow_id: "wf".to_string(),
            entity_mode: crate::scope::EntityMode::Isolated,
            workdir: None,
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: vec!["wicked-garden-domain-extractor".to_string()],
        };
        let of = RequiredRefs::of(&input);
        assert_eq!(of.seat, vec!["wicked-garden-domain"]);
        assert_eq!(
            of.plan,
            vec!["wicked-garden-domain-extractor", "wicked-garden-domain"]
        );
        assert!(admit_refs(Some(s.clone()), &of, &pi).unwrap().is_some());
        assert_eq!(
            WorkerCli::for_binaries("/opt/bin/claude.exe", "whatever", "k"),
            WorkerCli::Claude
        );
        assert_eq!(
            WorkerCli::for_binaries("pi", "/usr/local/bin/pi", "pi"),
            WorkerCli::Other {
                key: "pi".into(),
                lever: SkillsLever::PiSkillFlags
            }
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The live walk indexes nested skills and approximates `portable` from the text — and is
    /// FAIL-CLOSED like a published generation (codex round 8): a `SKILL.md` without a frontmatter
    /// name (or block, or unreadable, or non-UTF-8) is REFUSED with the error, not skipped with a
    /// notice; a symlink ANYWHERE in the root — a linked skill directory, a link under `scripts/`,
    /// a linked `skills/` root — is refused by path; a plugin with no `skills/` at all is an empty
    /// index. Through the ladder every such defect is `Ladder::Failed` with the reason (the
    /// `skills.fallback FAILED` log), never a silent skip and never "no installed garden".
    #[cfg(unix)]
    #[test]
    fn the_live_walk_indexes_nested_skills_and_refuses_nameless_skills_and_links() {
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
        let s = load_live(root.clone(), SnapshotSource::LiveCache).unwrap();
        let names: Vec<&str> = s.skills().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "wicked-garden-domain-extractor",
                "wicked-garden-qe",
                "wicked-garden-qe-a11y"
            ],
            "sorted by dir, nested included"
        );
        assert_eq!(s.skill("wicked-garden-qe-a11y").unwrap().dir, "qe/a11y");
        assert!(s.skill("wicked-garden-qe").unwrap().portable);
        assert!(
            !s.skill("wicked-garden-domain-extractor").unwrap().portable,
            "a ${{CLAUDE_PLUGIN_ROOT}} reference marks the skill Claude-only"
        );
        let refused = |needle: &str| {
            let err = load_live(root.clone(), SnapshotSource::LiveCache).expect_err(needle);
            assert!(err.contains(needle), "{needle}: {err}");
            err
        };
        // A nameless SKILL.md (no frontmatter block) ⇒ refused naming it, not skipped.
        std::fs::create_dir_all(root.join("skills").join("bare")).unwrap();
        std::fs::write(
            root.join("skills").join("bare").join("SKILL.md"),
            "# no frontmatter\n",
        )
        .unwrap();
        refused("skills/bare/SKILL.md has no `---` frontmatter block");
        // Non-UTF-8 ⇒ refused with the error.
        std::fs::write(
            root.join("skills").join("bare").join("SKILL.md"),
            [b"---\nname: x\n---\n".as_slice(), &[0xff, 0xfe, 0xfd]].concat(),
        )
        .unwrap();
        refused("skills/bare/SKILL.md is not UTF-8");
        std::fs::remove_dir_all(root.join("skills").join("bare")).unwrap();
        // A linked skill DIRECTORY ⇒ refused (round 5 skipped it).
        let outside = base.join("outside");
        write_skill(&outside, "leak", "wicked-garden-leak");
        std::os::unix::fs::symlink(outside.join("skills"), root.join("skills").join("linked"))
            .unwrap();
        refused("skills/linked is a symlink");
        std::fs::remove_file(root.join("skills").join("linked")).unwrap();
        // A link under the support tree (`scripts/`) ⇒ refused by the whole-tree walk.
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("scripts").join("vendored")).unwrap();
        refused("scripts/vendored is a symlink");
        std::fs::remove_file(root.join("scripts").join("vendored")).unwrap();
        // A linked SKILL.md file ⇒ refused.
        std::fs::create_dir_all(root.join("skills").join("lf")).unwrap();
        std::os::unix::fs::symlink(
            skill_dir(&outside, "leak").join(SKILL_FILE),
            root.join("skills").join("lf").join(SKILL_FILE),
        )
        .unwrap();
        refused("skills/lf/SKILL.md is a symlink");
        std::fs::remove_dir_all(root.join("skills").join("lf")).unwrap();
        assert!(
            load_live(root.clone(), SnapshotSource::LiveCache).is_ok(),
            "well-formed again"
        );

        // codex round 5: a linked `skills/` ROOT is refused AT `skills`, before anything under it
        // is looked at. (A skill-less `live_root` writes no `skills/` — a plugin without one is an
        // EMPTY index, not a defect — so the link is planted where the directory would be.)
        let linked_root = live_root(&base.join("linked-root"), "1.0.0", &[]);
        assert!(
            load_live(linked_root.clone(), SnapshotSource::LiveCache)
                .unwrap()
                .skills()
                .is_empty(),
            "no skills/ at all is an empty index, not a containment defect"
        );
        std::os::unix::fs::symlink(outside.join("skills"), linked_root.join("skills")).unwrap();
        let err = load_live(linked_root.clone(), SnapshotSource::LiveCache)
            .expect_err("a linked skills/ root is not a contained tree");
        assert!(err.contains("skills is a symlink"), "{err}");
        // Through the ladder (codex round 8): a FAILURE with the reason and a `FAILED` log — the
        // no-root rung, so a skill-naming run is refused with it and a skill-free one proceeds.
        let config = base.join("claude-config");
        let cache = config
            .join("plugins")
            .join("cache")
            .join("wicked-garden")
            .join("wicked-garden")
            .join("2.0.0");
        live_root(&cache, "2.0.0", &[]);
        std::os::unix::fs::symlink(outside.join("skills"), cache.join("skills")).unwrap();
        let mut lines = Vec::new();
        let ladder =
            resolve_ladder_in(None, Some(config.clone()), None, &mut collect(&mut lines)).unwrap();
        let Ladder::Failed(why) = &ladder else {
            panic!("expected Failed, got {ladder:?}");
        };
        assert!(
            why.contains("skills is a symlink") && why.contains(&cache.display().to_string()),
            "{why}"
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("skills.fallback FAILED") && !lines[0].contains("using the"),
            "a refused fallback is logged as FAILED, never as taken: {lines:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Frontmatter is read with YAML SEMANTICS (codex round 3): a comment after a value is a
    /// comment, quoted scalars are unquoted, `mandates` may be a flow list (with a trailing
    /// comment), a block list or a single scalar. A document that is not valid YAML — an
    /// unterminated flow list, a tab in indentation, an unterminated block, a non-string `name`,
    /// a non-mapping — is `Malformed`, which index verification reports as a config error NAMING
    /// the file and the live walk skips with a notice; text without a block is `NoBlock`.
    #[test]
    fn frontmatter_is_parsed_with_yaml_semantics_and_malformed_documents_are_errors() {
        let fm = parse_frontmatter(
            "---\ndescription: |\n  multi\n  line\nname: \"wicked-garden-x\"\nmandates: [wicked-garden-b, \"wicked-garden-a\"]\n---\nbody\nmandates: not-in-frontmatter\n",
        )
        .unwrap();
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
        // Comments after values are comments — the pass-2 hand parser read `# the domain router`
        // into the name and minted a bogus mandate from `] # what it leans on`.
        let commented = parse_frontmatter(
            "---\nname: wicked-garden-domain # the domain router\nmandates: [wicked-garden-search] # what it leans on\n---\n",
        )
        .unwrap();
        assert_eq!(commented.name.as_deref(), Some("wicked-garden-domain"));
        assert_eq!(commented.mandates, vec!["wicked-garden-search"]);
        let single =
            parse_frontmatter("---\nname: 'wicked-garden-z'\nmandates: wicked-garden-mem\n---\n")
                .unwrap();
        assert_eq!(single.name.as_deref(), Some("wicked-garden-z"));
        assert_eq!(single.mandates, vec!["wicked-garden-mem"]);
        assert_eq!(
            parse_frontmatter("# no frontmatter\nname: not-in-frontmatter\n"),
            Err(FrontmatterError::NoBlock)
        );
        assert_eq!(
            parse_frontmatter("---\ndescription: only\n---\n"),
            Ok(Frontmatter::default())
        );
        assert_eq!(
            parse_frontmatter("---\n---\n"),
            Ok(Frontmatter::default()),
            "an empty block"
        );
        for (doc, needle) in [
            (
                "---\nname: x\nmandates: [wicked-garden-a, wicked-garden-b\n---\n",
                "not valid YAML",
            ),
            (
                "---\nname: x\nmandates:\n\t- wicked-garden-a\n---\n",
                "not valid YAML",
            ),
            ("---\nname: x\nmandates: [a]\n", "not terminated"),
            ("---\nname: [x]\n---\n", "`name` is a YAML sequence"),
            ("---\nname: 12\n---\n", "`name` is a YAML number"),
            (
                "---\nmandates: {a: b}\n---\n",
                "`mandates` is a YAML mapping",
            ),
            (
                "---\nmandates: [a, 1]\n---\n",
                "`mandates` holds a YAML number",
            ),
            ("---\n- just\n- a list\n---\n", "not a mapping"),
        ] {
            match parse_frontmatter(doc) {
                Err(FrontmatterError::Malformed(why)) => {
                    assert!(why.contains(needle), "{doc:?}: {why}")
                }
                other => panic!("{doc:?} should be Malformed, got {other:?}"),
            }
        }
        // Through index verification a malformed SKILL.md is a config error NAMING the file…
        let base = scratch("fm");
        let root = snapshot_root(
            &gen_dir(&base, "3"),
            "3",
            &[("domain", "wicked-garden-domain")],
        );
        std::fs::write(
            skill_dir(&root, "domain").join(SKILL_FILE),
            "---\nname: wicked-garden-domain\nmandates: [wicked-garden-search\n---\n",
        )
        .unwrap();
        let err = published(&root).expect_err("malformed frontmatter");
        let SkillsError::Config { why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert!(
            why.contains("skills/domain/SKILL.md has malformed frontmatter"),
            "{why}"
        );
        // …and the live walk REFUSES it with the error naming the file (codex round 8 — round 5
        // skipped it with a notice, leaving Claude to load a skill the engine could not index).
        let live = live_root(&base.join("live"), "1.0.0", &[("qe", "wicked-garden-qe")]);
        std::fs::write(
            skill_dir(&live, "qe").join(SKILL_FILE),
            "---\nname: wicked-garden-qe\nmandates:\n\t- x\n---\n",
        )
        .unwrap();
        let err = load_live(live, SnapshotSource::LiveCache)
            .expect_err("a live root with a malformed SKILL.md is not a fallback");
        assert!(
            err.contains("skills/qe/SKILL.md has malformed frontmatter"),
            "{err}"
        );
        assert_eq!(derived_name("qe/a11y"), "wicked-garden-qe-a11y");
        assert!(is_garden_name("wicked-garden-qe") && !is_garden_name("wicked-testing-qe"));
        assert!(!is_garden_name("wicked-garden-") && !is_garden_name("wicked-garden"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A PORTABLE parent whose directory NESTS a non-portable child (codex round 3): the
    /// directory levers scan what they are handed recursively, so the parent is not deliverable
    /// — absent from pi's `--skill` flags and opencode's `skills.paths` — while its portable
    /// descendants and siblings are delivered on their own paths; a non-Claude unit invoking the
    /// parent is refused naming parent AND child; a Claude unit (plugin loader, no directory
    /// hand-off) is admitted; existence stays plan-wide.
    #[test]
    fn a_portable_parent_nesting_a_nonportable_child_is_not_delivered_by_directory() {
        let base = scratch("nesting");
        let root = snapshot_root_with(
            &gen_dir(&base, "14"),
            "14",
            &[
                ("engineering", "wicked-garden-engineering", true, &[]),
                (
                    "engineering/legacy",
                    "wicked-garden-engineering-legacy",
                    false,
                    &[],
                ),
                (
                    "engineering/frontend",
                    "wicked-garden-engineering-frontend",
                    true,
                    &[],
                ),
                ("domain", "wicked-garden-domain", true, &[]),
            ],
        );
        let s = load(&root);
        let pi = WorkerCli::for_binaries("pi", "pi", "pi");
        let opencode = WorkerCli::for_binaries("opencode", "opencode", "opencode");
        assert_eq!(
            s.portable_skill_dirs(),
            vec![
                skill_dir(&root, "domain"),
                skill_dir(&root, "engineering/frontend")
            ],
            "the parent is excluded; its portable child and the sibling are delivered"
        );
        let parent = skill_dir(&root, "engineering");
        let flags = s.delivery(&pi).argv_flags();
        assert!(!flags.iter().any(|f| Path::new(f) == parent), "{flags:?}");
        let composed: Value = serde_json::from_str(
            &s.delivery(&opencode)
                .opencode_config(None)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let paths = composed["skills"]["paths"].as_array().unwrap();
        assert_eq!(paths.len(), 2, "{composed}");
        assert!(
            !paths
                .iter()
                .any(|p| Path::new(p.as_str().unwrap()) == parent),
            "{composed}"
        );
        let err = admit_refs(
            Some(s.clone()),
            &RequiredRefs::seat(["wicked-garden-engineering"]),
            &opencode,
        )
        .expect_err("an undeliverable parent");
        assert_eq!(
            err,
            SkillsError::NestsNonPortable {
                root: root.clone(),
                cli: "opencode".to_string(),
                skills: vec![(
                    "wicked-garden-engineering".to_string(),
                    "engineering/legacy".to_string()
                )],
            }
        );
        let msg = err.to_string();
        assert!(
            msg.contains("skills/engineering/legacy") && msg.contains("recursively"),
            "{msg}"
        );
        assert!(matches!(
            admit_refs(
                Some(s.clone()),
                &RequiredRefs::seat(["wicked-garden-engineering"]),
                &pi
            ),
            Err(SkillsError::NestsNonPortable { .. })
        ));
        assert!(admit_refs(
            Some(s.clone()),
            &RequiredRefs::seat(["wicked-garden-engineering-frontend", "wicked-garden-domain"]),
            &opencode
        )
        .unwrap()
        .is_some());
        assert!(admit_refs(
            Some(s.clone()),
            &RequiredRefs::seat(["wicked-garden-engineering"]),
            &WorkerCli::Claude
        )
        .unwrap()
        .is_some());
        assert!(
            admit_refs(
                Some(s.clone()),
                &RequiredRefs::plan_and_seat(
                    ["wicked-garden-engineering"],
                    ["wicked-garden-domain"]
                ),
                &opencode
            )
            .unwrap()
            .is_some(),
            "another seat's use of the parent does not refuse this unit"
        );
        // codex round 4: the nesting rule is scoped to the DIRECTORY levers. Copilot is handed
        // the published VIEW and judged on it (v3.3 §2): a view whose copy of the parent EXCLUDES
        // the Claude-only child admits the parent with `--add-dir <view>`; one that carries the
        // child refuses by path. Codex (no lever) is refused as NoLever, not NestsNonPortable.
        let copilot = WorkerCli::for_binaries("copilot", "copilot", "copilot");
        let codex = WorkerCli::for_binaries("codex", "codex", "codex");
        let parent = RequiredRefs::seat(["wicked-garden-engineering"]);
        assert!(matches!(
            admit_refs(Some(s.clone()), &parent, &codex),
            Err(SkillsError::NoLever { .. })
        ));
        let view = root.join("views").join("copilot");
        let parent_copy = view
            .join(".github")
            .join("skills")
            .join("wicked-garden-engineering");
        std::fs::create_dir_all(&parent_copy).unwrap();
        std::fs::write(
            parent_copy.join(SKILL_FILE),
            "---\nname: wicked-garden-engineering\n---\n",
        )
        .unwrap();
        let handed = admit_refs(Some(s.clone()), &parent, &copilot)
            .expect("the view excludes the child")
            .expect("handed");
        assert_eq!(
            handed.delivery(&copilot),
            SkillsDelivery::CopilotAddDir(view.clone())
        );
        assert!(
            matches!(
                admit_refs(Some(s.clone()), &parent, &pi),
                Err(SkillsError::NestsNonPortable { .. })
            ),
            "the directory levers are still refused"
        );
        std::fs::create_dir_all(parent_copy.join("legacy")).unwrap();
        std::fs::write(
            parent_copy.join("legacy").join(SKILL_FILE),
            "---\nname: wicked-garden-engineering-legacy\n---\n",
        )
        .unwrap();
        let err =
            admit_refs(Some(s.clone()), &parent, &copilot).expect_err("the view carries the child");
        assert!(
            matches!(&err, SkillsError::Config { why, .. }
                if why.contains("wicked-garden-engineering/legacy/SKILL.md")
                    && why.contains("portable: false")),
            "{err:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// v3.3 §3 (codex round 4): every persisted spelling the engine joins onto a root — an index
    /// entry's `name` (joined onto the copilot view) and `dir` (joined onto `skills/`) — is
    /// validated as a safe relative segment set AT INDEX VERIFICATION, before any join: an
    /// absolute name, `..`, a drive prefix, a UNC/verbatim prefix, a name with a separator are
    /// each refused, all listed in one config error. After a join, the walked path must also
    /// canonicalize INSIDE the root before it is read (`contained_under`).
    #[test]
    fn persisted_names_and_dirs_are_safe_relative_segments_refused_at_index_verification() {
        let base = scratch("hygiene");
        let root = snapshot_root(
            &gen_dir(&base, "31"),
            "31",
            &[("domain", "wicked-garden-domain")],
        );
        // Overwrite the index with hostile spellings; the valid `domain` entry stays.
        let index = serde_json::json!({
            "gen": "31",
            "contentHash": "sha256:31",
            "gardenSource": garden_source(),
            "venv": "skipped",
            "skills": [
                {"name": "wicked-garden-domain", "dir": "skills/domain", "portable": true},
                {"name": "/etc", "dir": "skills/domain", "portable": true},
                {"name": "wicked-garden-dotdot", "dir": "skills/../outside", "portable": true},
                {"name": "wicked-garden-drive", "dir": "skills/C:/outside", "portable": true},
                {"name": "wicked-garden-unc", "dir": r"skills/\\?\C:\outside", "portable": true},
                {"name": "a/b", "dir": "skills/domain", "portable": true},
                {"name": "wicked-garden-colon", "dir": "skills/C:outside", "portable": true},
            ]
        });
        std::fs::write(
            root.join(SNAPSHOT_INDEX),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
        let err = published(&root).expect_err("hostile spellings are refused at load");
        let SkillsError::Config { why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        for needle in [
            "name `/etc`",
            "is absolute",
            "dir `../outside`",
            "`.` or `..`",
            "dir `C:/outside`",
            "colon",
            r"dir `\\?\C:\outside`",
            "backslash",
            "name `a/b`",
            "single path component",
            "dir `C:outside`",
            "not a clean relative",
        ] {
            assert!(why.contains(needle), "{needle}: {why}");
        }
        // `safe_segments` itself: the nested `dir` form and the single-segment `name` form.
        assert_eq!(
            safe_segments("engineering/frontend", true, "dir"),
            Ok(vec!["engineering", "frontend"])
        );
        assert_eq!(
            safe_segments("wicked-garden-domain", false, "name"),
            Ok(vec!["wicked-garden-domain"])
        );
        for bad in [
            "", "/x", "x/", "a//b", "./x", "x/..", "..", r"a\b", "C:", "a:b", "x\0y",
        ] {
            assert!(safe_segments(bad, true, "dir").is_err(), "{bad:?}");
        }
        assert!(safe_segments("engineering/frontend", false, "name").is_err());
        // Containment after the join: a path that canonicalizes OUTSIDE the root is refused
        // before any read; one inside passes.
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("file"), b"").unwrap();
        assert_eq!(
            contained_under(&root, &root.join(SNAPSHOT_INDEX), SNAPSHOT_INDEX),
            Ok(())
        );
        let err = contained_under(&root, &outside.join("file"), "x").unwrap_err();
        assert!(err.contains("outside the root"), "{err}");
        // A valid snapshot still loads exactly as before — the belt does not tighten the braces.
        let root2 = snapshot_root(
            &gen_dir(&base, "32"),
            "32",
            &[
                ("domain", "wicked-garden-domain"),
                ("engineering/frontend", "wicked-garden-engineering-frontend"),
            ],
        );
        assert_eq!(published(&root2).unwrap().unwrap().skills().len(), 2);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// codex round 4: ONE admission policy for a fresh launch and a cached session (`admit_turn`);
    /// codex round 5: the two kinds of turn are told apart. The inherit-config escape hatch
    /// bypasses admission whatever the turn; a FRESH launch resolves the ambient root through the
    /// ladder and the fence; a cached session PINNED to a generation is judged against it and never
    /// the ambient one; a cached session opened with NO snapshot is admitted for a skill-free turn
    /// and REFUSED for a skill-bearing one naming the skills — the ambient root, whatever it now
    /// holds, is never resolved for it (the session never received a plugin).
    #[test]
    fn admit_turn_applies_one_policy_to_fresh_and_cached_sessions() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        struct Pin(&'static str, Option<std::ffi::OsString>);
        impl Pin {
            fn set(key: &'static str, value: Option<&std::ffi::OsStr>) -> Self {
                let prev = std::env::var_os(key);
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
                Pin(key, prev)
            }
        }
        impl Drop for Pin {
            fn drop(&mut self) {
                match &self.1 {
                    Some(v) => std::env::set_var(self.0, v),
                    None => std::env::remove_var(self.0),
                }
            }
        }
        let base = scratch("turn");
        // The fence over the fixture's state home (`base`) must classify it: HOME is pinned to the
        // base so no default fenced directory can contain the scratch.
        let _home = Pin::set("HOME", Some(base.as_os_str()));
        let ambient = snapshot_root(
            &gen_dir(&base, "2"),
            "2",
            &[
                ("domain", "wicked-garden-domain"),
                ("search", "wicked-garden-search"),
            ],
        );
        let pinned_root = snapshot_root(
            &gen_dir(&base, "1"),
            "1",
            &[("domain", "wicked-garden-domain")],
        );
        let pinned = load(&pinned_root);
        let _snap = Pin::set(SKILLS_SNAPSHOT_ENV, Some(ambient.as_os_str()));
        let input = |skill: &str| {
            let mut u = crate::domain::WorkUnit::pending("r:u1", "r", 1, "do");
            u.skill_ref = Some(skill.to_string());
            StepInput {
                run_id: "r".to_string(),
                unit_ix: 0,
                attempt: 0,
                unit: u,
                workflow_id: "wf".to_string(),
                entity_mode: crate::scope::EntityMode::Isolated,
                workdir: None,
                governance: None,
                prior_outputs: vec![],
                elicitation_epoch: 0,
                process_gen: None,
                launch_seq: 0,
                required_skills: Vec::new(),
            }
        };
        let claude = WorkerCli::Claude;
        // Fresh: the ambient generation is resolved, fenced and handed.
        let fresh = admit_turn(Turn::Fresh, &input("wicked-garden-search"), &claude)
            .unwrap()
            .expect("handed");
        assert_eq!(fresh.root, ambient);
        // Pinned: judged against the pinned generation ONLY — `search` lives in the ambient one
        // and is refused naming the PINNED root; `domain` is admitted from it.
        let err = admit_turn(
            Turn::Cached(Some(pinned.clone())),
            &input("wicked-garden-search"),
            &claude,
        )
        .expect_err("the pinned generation wins over the ambient one");
        assert!(
            matches!(&err, SkillsError::Missing { root: Some(r), .. } if r == &pinned_root),
            "{err:?}"
        );
        assert_eq!(
            admit_turn(
                Turn::Cached(Some(pinned.clone())),
                &input("wicked-garden-domain"),
                &claude
            )
            .unwrap()
            .unwrap()
            .root,
            pinned_root
        );
        // Cached with NOTHING pinned (codex round 5): the ambient generation — which holds
        // `search` and would admit a fresh launch — is NOT resolved for it. A skill-bearing turn
        // is refused naming the skill and the seat, advising a fresh session; a skill-free turn is
        // admitted with nothing handed; the plan-wide set counts (a skill another unit names,
        // this one invoking nothing, still refuses — the session cannot serve that run).
        let err = admit_turn(Turn::Cached(None), &input("wicked-garden-search"), &claude)
            .expect_err("a session that never received a plugin cannot be given one mid-run");
        assert_eq!(
            err,
            SkillsError::NotDelivered {
                cli: "claude".to_string(),
                skills: vec!["wicked-garden-search".to_string()],
            }
        );
        let text = err.to_string();
        assert!(
            text.contains("wicked-garden-search")
                && text.contains("opened without a skills snapshot")
                && text.contains("fresh session")
                && !text.contains(&ambient.display().to_string()),
            "names the skill, advises a fresh session, never mentions the ambient root: {text}"
        );
        let mut none = input("");
        none.unit.skill_ref = None;
        assert_eq!(admit_turn(Turn::Cached(None), &none, &claude), Ok(None));
        let mut plan_wide = none.clone();
        plan_wide.required_skills = vec!["wicked-garden-domain".to_string()];
        assert_eq!(
            admit_turn(Turn::Cached(None), &plan_wide, &claude),
            Err(SkillsError::NotDelivered {
                cli: "claude".to_string(),
                skills: vec!["wicked-garden-domain".to_string()],
            })
        );
        // The inherit-config escape hatch bypasses NOTHING (codex round 6): under it a fresh
        // launch still resolves, fences and is handed the ambient generation; a missing required
        // skill is still refused by name against it; a pinned session is still judged against its
        // generation; a session that never received a plugin still refuses a skill turn; and an
        // explicit path that is not a snapshot is still a launch error naming it. Rounds 2–5
        // returned `Ok(None)` for every one of these.
        {
            let _hatch = Pin::set(
                crate::execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV,
                Some(std::ffi::OsStr::new("1")),
            );
            let handed = admit_turn(Turn::Fresh, &input("wicked-garden-search"), &claude)
                .unwrap()
                .expect("the snapshot is handed under the hatch too");
            assert_eq!(handed.root, ambient);
            let err = admit_turn(Turn::Fresh, &input("wicked-garden-absent"), &claude)
                .expect_err("a missing skill is refused under the hatch");
            assert_eq!(
                err,
                SkillsError::Missing {
                    root: Some(ambient.clone()),
                    missing: vec!["wicked-garden-absent".to_string()],
                }
            );
            let err = admit_turn(
                Turn::Cached(Some(pinned.clone())),
                &input("wicked-garden-search"),
                &claude,
            )
            .expect_err("the pinned generation is still the judge");
            assert!(
                matches!(&err, SkillsError::Missing { root: Some(r), .. } if r == &pinned_root),
                "{err:?}"
            );
            assert_eq!(
                admit_turn(Turn::Cached(None), &input("wicked-garden-search"), &claude),
                Err(SkillsError::NotDelivered {
                    cli: "claude".to_string(),
                    skills: vec!["wicked-garden-search".to_string()],
                })
            );
            let bad = base.join("no-such-snapshot");
            let _bad = Pin::set(SKILLS_SNAPSHOT_ENV, Some(bad.as_os_str()));
            let err = admit_turn(Turn::Fresh, &none, &claude)
                .expect_err("an invalid explicit snapshot is a launch error under the hatch");
            assert!(
                matches!(&err, SkillsError::Config { var, path, .. }
                    if *var == SKILLS_SNAPSHOT_ENV && path == &bad),
                "{err:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The run-wide EXISTENCE admission a TOOL-COMMAND unit gets (`admit_plan`, codex round 6):
    /// a plan that names a skill the ambient root lacks is refused naming it — before the command
    /// runs, whatever unit invokes the skill; a plan whose skills all exist is admitted; an
    /// explicit path that is not a snapshot is the same config error every launch gets; and a
    /// run that names NO skill has nothing to admit — nothing is resolved, so an invalid path is
    /// not even looked at for it.
    #[test]
    fn a_tool_command_unit_is_admitted_for_plan_wide_existence_before_it_runs() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        struct Pin(&'static str, Option<std::ffi::OsString>);
        impl Pin {
            fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
                let prev = std::env::var_os(key);
                std::env::set_var(key, value);
                Pin(key, prev)
            }
        }
        impl Drop for Pin {
            fn drop(&mut self) {
                match &self.1 {
                    Some(v) => std::env::set_var(self.0, v),
                    None => std::env::remove_var(self.0),
                }
            }
        }
        let base = scratch("plan");
        let root = snapshot_root(
            &gen_dir(&base, "1"),
            "1",
            &[("domain", "wicked-garden-domain")],
        );
        let tool = |plan: &[&str]| {
            let mut u = crate::domain::WorkUnit::pending("r:u1", "r", 1, "index the repo");
            u.tool_cmd = Some(vec!["true".to_string()]);
            StepInput {
                run_id: "r".to_string(),
                unit_ix: 0,
                attempt: 0,
                unit: u,
                workflow_id: "wf".to_string(),
                entity_mode: crate::scope::EntityMode::Isolated,
                workdir: None,
                governance: None,
                prior_outputs: vec![],
                elicitation_epoch: 0,
                process_gen: None,
                launch_seq: 0,
                required_skills: plan.iter().map(|s| s.to_string()).collect(),
            }
        };
        {
            let _snap = Pin::set(SKILLS_SNAPSHOT_ENV, root.as_os_str());
            assert_eq!(
                admit_plan(&tool(&["wicked-garden-domain"]))
                    .unwrap()
                    .expect("the generation the run was judged against is returned")
                    .root,
                root,
                "so the actor can report it like a handoff"
            );
            assert_eq!(
                admit_plan(&tool(&[])),
                Ok(None),
                "a skill-free run has nothing to admit and resolves nothing"
            );
            assert_eq!(
                admit_plan(&tool(&["wicked-garden-domain", "wicked-garden-mem"])),
                Err(SkillsError::Missing {
                    root: Some(root.clone()),
                    missing: vec!["wicked-garden-mem".to_string()],
                }),
                "a later agent unit's skill is judged before the tool command runs"
            );
        }
        {
            let bad = base.join("no-such-snapshot");
            let _snap = Pin::set(SKILLS_SNAPSHOT_ENV, bad.as_os_str());
            assert!(
                matches!(admit_plan(&tool(&["wicked-garden-domain"])),
                    Err(SkillsError::Config { var, path, .. })
                        if var == SKILLS_SNAPSHOT_ENV && path == bad),
                "an invalid explicit snapshot is the same launch error"
            );
            assert_eq!(
                admit_plan(&tool(&[])),
                Ok(None),
                "a skill-free run has nothing to admit and resolves nothing — the invalid path is \
                 not even looked at"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (codex round 7) Exact INDEX/TREE PARITY: a published generation may deliver nothing its index
    /// does not carry — an unindexed `skills/disabled/SKILL.md` (a disabled skill copied in), a
    /// nested extra `skills/domain/extra/SKILL.md`, a `SKILL.md` at the skills root, a `views/`
    /// child other than `copilot`, a stray file in the copilot view — and no symlink anywhere in
    /// it (a root-level `scripts -> …`, a link under `views/`), EXCEPT crew's root `.venv` link,
    /// accepted only when it resolves inside `<state home>/skills/baseline/<64-hex>/.venv`. Support
    /// files that are not skills (`skills/domain/refs/notes.md`, `scripts/x.py`) are fine.
    #[test]
    fn the_delivered_tree_must_match_the_index_exactly() {
        let base = scratch("parity");
        let root = snapshot_root(
            &gen_dir(&base, "3"),
            "3",
            &[
                ("domain", "wicked-garden-domain"),
                ("engineering/frontend", "wicked-garden-engineering-frontend"),
            ],
        );
        assert!(published(&root).is_ok(), "the intact fixture loads");
        let refused = |needle: &str| {
            let err = published(&root).expect_err(needle);
            let SkillsError::Config { why, .. } = &err else {
                panic!("expected Config, got {err:?}");
            };
            assert!(why.contains(needle), "{needle}: {why}");
        };
        // Support files that are not skills are part of the closure.
        std::fs::create_dir_all(skill_dir(&root, "domain").join("refs")).unwrap();
        std::fs::write(
            skill_dir(&root, "domain").join("refs").join("notes.md"),
            "x",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("scripts").join("x.py"), "print()").unwrap();
        std::fs::write(root.join("pyproject.toml"), "[project]\n").unwrap();
        assert!(
            published(&root).is_ok(),
            "support files are contained, not indexed"
        );
        // (1) an unindexed skill — disabled, copied in anyway.
        write_skill(&root, "disabled", "wicked-garden-disabled");
        refused("skills/disabled/SKILL.md");
        refused("index does not carry");
        std::fs::remove_dir_all(skill_dir(&root, "disabled")).unwrap();
        // (2) a nested extra under an indexed skill.
        write_skill(&root, "domain/extra", "wicked-garden-domain-extra");
        refused("skills/domain/extra/SKILL.md");
        std::fs::remove_dir_all(skill_dir(&root, "domain").join("extra")).unwrap();
        // (3) a SKILL.md at the skills root itself.
        std::fs::write(
            root.join(SKILLS_DIR).join(SKILL_FILE),
            "---\nname: x\n---\n",
        )
        .unwrap();
        refused("skills/SKILL.md is a SKILL.md at the skills root");
        std::fs::remove_file(root.join(SKILLS_DIR).join(SKILL_FILE)).unwrap();
        // (4) views: only copilot; a stray inside the copilot view is judged by the view walk.
        std::fs::create_dir_all(root.join("views").join("other")).unwrap();
        refused("views/other is not a delivery view");
        std::fs::remove_dir_all(root.join("views").join("other")).unwrap();
        std::fs::create_dir_all(root.join("views").join("copilot")).unwrap();
        std::fs::write(root.join("views").join("copilot").join("README.md"), "x").unwrap();
        refused("views/copilot/README.md is not part of a copilot view");
        std::fs::remove_dir_all(root.join("views")).unwrap();
        assert!(published(&root).is_ok(), "well-formed again");
        // (5) links: none anywhere — except crew's .venv into the baseline env.
        #[cfg(unix)]
        {
            let outside = base.join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::os::unix::fs::symlink(&outside, root.join("scripts").join("vendored")).unwrap();
            refused("scripts/vendored is a symlink");
            std::fs::remove_file(root.join("scripts").join("vendored")).unwrap();
            std::fs::create_dir_all(root.join("views")).unwrap();
            std::os::unix::fs::symlink(&outside, root.join("views").join("copilot")).unwrap();
            refused("views/copilot is a symlink");
            std::fs::remove_dir_all(root.join("views")).unwrap();
            // .venv: with NO baseline env root in the state home, any link is refused naming the
            // missing root; with one, a link elsewhere ⇒ refused ("not exactly"); crew's link into
            // `<state home>/skills/baseline/<RECORDED baseline>/.venv` — RELATIVE as crew writes
            // it, or absolute — ⇒ fine once snapshot.json records the env as `synced` (review
            // pass 11: the link is BOUND to the metadata — `gardenSource.baseline` names the one
            // env it may reach and `venv` says whether one was provisioned; crew's
            // `verifyCurrent`); a non-hex baseline name ⇒ refused; dangling ⇒ refused; (codex
            // round 8) a target with a component BEYOND `.venv` (`<hash>/.venv/bin`), one that
            // climbs back out (`<hash>/.venv/../x`), and a SYMLINKED `baseline` are refused.
            let venv = root.join(".venv");
            std::os::unix::fs::symlink(&outside, &venv).unwrap();
            set_index_field(&root, "venv", serde_json::json!("synced"));
            refused("has no baseline env root");
            let hash = fixture_baseline();
            let env = base
                .join(SKILLS_DIR)
                .join("baseline")
                .join(&hash)
                .join(".venv");
            std::fs::create_dir_all(env.join("bin")).unwrap();
            refused("not exactly");
            std::fs::remove_file(&venv).unwrap();
            std::os::unix::fs::symlink(&env, &venv).unwrap();
            assert!(
                published(&root).is_ok(),
                "crew's .venv link into the RECORDED baseline env is the one accepted link: {:?}",
                published(&root).err()
            );
            std::fs::remove_file(&venv).unwrap();
            std::os::unix::fs::symlink(format!("../../baseline/{hash}/.venv"), &venv).unwrap();
            assert!(
                published(&root).is_ok(),
                "the RELATIVE spelling crew writes is accepted: {:?}",
                published(&root).err()
            );
            // (review pass 11) A link into ANOTHER valid baseline env (B) while snapshot.json
            // records A ⇒ refused naming both hashes — B exists, is a real env, spells the exact
            // shape, and is still not this generation's.
            let other = format!("{:0>64}", "beef");
            let other_env = base
                .join(SKILLS_DIR)
                .join("baseline")
                .join(&other)
                .join(".venv");
            std::fs::create_dir_all(&other_env).unwrap();
            std::fs::remove_file(&venv).unwrap();
            std::os::unix::fs::symlink(format!("../../baseline/{other}/.venv"), &venv).unwrap();
            refused(&format!("baseline env `{other}`"));
            refused(&format!("snapshot.json records (`{hash}`)"));
            // The RIGHT link while the env is recorded as NOT provisioned ⇒ refused, whatever the
            // recorded state …
            std::fs::remove_file(&venv).unwrap();
            std::os::unix::fs::symlink(format!("../../baseline/{hash}/.venv"), &venv).unwrap();
            for state in ["skipped", "pending", "failed"] {
                set_index_field(&root, "venv", serde_json::json!(state));
                refused(&format!("records the env as `{state}`"));
            }
            set_index_field(&root, "venv", serde_json::json!("synced"));
            assert!(published(&root).is_ok());
            // … and `synced` WITHOUT the link is not what publish wrote either.
            std::fs::remove_file(&venv).unwrap();
            refused("records the baseline env as synced but the generation has no .venv link");
            std::os::unix::fs::symlink(env.join("bin"), &venv).unwrap();
            refused("not exactly");
            std::fs::remove_file(&venv).unwrap();
            std::os::unix::fs::symlink(format!("../../baseline/{hash}/.venv/../x"), &venv).unwrap();
            refused("not exactly");
            std::fs::remove_file(&venv).unwrap();
            let bad_env = base
                .join(SKILLS_DIR)
                .join("baseline")
                .join("not-a-hash")
                .join(".venv");
            std::fs::create_dir_all(&bad_env).unwrap();
            std::os::unix::fs::symlink(&bad_env, &venv).unwrap();
            refused("not exactly");
            std::fs::remove_file(&venv).unwrap();
            std::os::unix::fs::symlink(base.join("nowhere").join(".venv"), &venv).unwrap();
            refused("not exactly");
            std::fs::remove_file(&venv).unwrap();
            // A symlinked `baseline` ⇒ refused even for a target that spells the right path.
            let real_baseline = base.join("real-baseline");
            std::fs::rename(base.join(SKILLS_DIR).join("baseline"), &real_baseline).unwrap();
            std::os::unix::fs::symlink(&real_baseline, base.join(SKILLS_DIR).join("baseline"))
                .unwrap();
            std::os::unix::fs::symlink(&env, &venv).unwrap();
            refused("skills/baseline` is a symlink");
            std::fs::remove_file(&venv).unwrap();
            std::fs::remove_file(base.join(SKILLS_DIR).join("baseline")).unwrap();
            std::fs::rename(&real_baseline, base.join(SKILLS_DIR).join("baseline")).unwrap();
            // A .venv link that is NOT at the root is an ordinary (refused) link (the root one,
            // valid and recorded `synced`, is back in place).
            std::os::unix::fs::symlink(&env, &venv).unwrap();
            std::os::unix::fs::symlink(&env, skill_dir(&root, "domain").join(".venv")).unwrap();
            refused("skills/domain/.venv is a symlink");
            std::fs::remove_file(skill_dir(&root, "domain").join(".venv")).unwrap();
            let loaded = published(&root).expect("the recorded, synced link verifies");
            assert_eq!(loaded.unwrap().venv, Some(VenvState::Synced));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (codex round 7) The live-cache FALLBACK root is Claude-only: a non-Claude seat that invokes
    /// a skill is refused naming the seat and told that non-Claude delivery requires a published
    /// snapshot; a Claude seat is admitted; a skill-free non-Claude unit is admitted but handed
    /// NOTHING (`delivery` is `None` for every non-Claude lever) — the `portable` approximation
    /// from the `SKILL.md` text never becomes a delivery.
    #[test]
    fn the_live_cache_fallback_is_claude_only() {
        let base = scratch("fallback-claude-only");
        let live = load_live(
            live_root(
                &base.join("live"),
                "1.0.0",
                &[("core", "wicked-garden-core"), ("mem", "wicked-garden-mem")],
            ),
            SnapshotSource::LiveCache,
        )
        .unwrap();
        let claude = WorkerCli::Claude;
        let pi = WorkerCli::for_binaries("pi", "pi", "pi");
        let opencode = WorkerCli::for_binaries("opencode", "opencode", "opencode");
        let copilot = WorkerCli::for_binaries("copilot", "copilot", "copilot");
        assert!(
            admit_refs(
                Some(live.clone()),
                &RequiredRefs::seat(["wicked-garden-core"]),
                &claude
            )
            .unwrap()
            .is_some(),
            "Claude is the documented fallback rung"
        );
        for seat in [&pi, &opencode, &copilot] {
            let err = admit_refs(
                Some(live.clone()),
                &RequiredRefs::seat(["wicked-garden-core"]),
                seat,
            )
            .expect_err("non-Claude delivery from the fallback is refused");
            assert_eq!(
                err,
                SkillsError::FallbackClaudeOnly {
                    root: live.root.clone(),
                    cli: seat.to_string(),
                    skills: vec!["wicked-garden-core".to_string()],
                },
                "{seat}"
            );
            let text = err.to_string();
            assert!(
                text.contains(&seat.to_string())
                    && text.contains("Claude-only")
                    && text.contains("non-Claude delivery requires a published snapshot")
                    && text.contains(&live.root.display().to_string()),
                "{text}"
            );
            // Skill-free: admitted, nothing handed.
            assert!(
                admit_refs(Some(live.clone()), &RequiredRefs::seat([]), seat)
                    .unwrap()
                    .is_some()
            );
            assert_eq!(live.delivery(seat), SkillsDelivery::None, "{seat}");
        }
        assert_eq!(
            live.delivery(&claude),
            SkillsDelivery::ClaudePlugin(live.root.clone())
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (codex round 7) Fallback discovery tells ABSENCE from FAILURE: no cache, or no version-named
    /// directory in it ⇒ `Ladder::Absent` and the "no installed" log; a highest version with a
    /// malformed manifest, a SYMLINKED version candidate, or a cache that cannot be listed
    /// (permission denied) ⇒ `Ladder::Failed(reason)` with a `skills.fallback FAILED` log naming the
    /// reason — never read as absence. Through admission, a failed ladder refuses a skill-naming run
    /// with `FallbackFailed` carrying the reason, and admits a skill-free one with no root.
    #[test]
    fn fallback_discovery_distinguishes_absence_from_failure() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        struct Pin(&'static str, Option<std::ffi::OsString>);
        impl Pin {
            fn set(key: &'static str, value: Option<&std::ffi::OsStr>) -> Self {
                let prev = std::env::var_os(key);
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
                Pin(key, prev)
            }
        }
        impl Drop for Pin {
            fn drop(&mut self) {
                match &self.1 {
                    Some(v) => std::env::set_var(self.0, v),
                    None => std::env::remove_var(self.0),
                }
            }
        }
        let base = scratch("ladder-failure");
        let config = base.join("claude-config");
        let cache = config
            .join("plugins")
            .join("cache")
            .join(PLUGIN_NAME)
            .join(PLUGIN_NAME);
        // Absent: no cache dir; then a cache dir with no version-named entry.
        let mut lines = Vec::new();
        assert_eq!(
            resolve_ladder_in(None, Some(config.clone()), None, &mut collect(&mut lines)).unwrap(),
            Ladder::Absent
        );
        std::fs::create_dir_all(cache.join("not-a-version")).unwrap();
        assert_eq!(
            resolve_ladder_in(None, Some(config.clone()), None, &mut collect(&mut lines)).unwrap(),
            Ladder::Absent
        );
        assert!(
            lines
                .iter()
                .all(|l| l.contains("no installed") && !l.contains("FAILED")),
            "{lines:?}"
        );
        // Failed: the highest version's manifest is malformed.
        let latest = live_root(
            &cache.join("2.0.0"),
            "2.0.0",
            &[("core", "wicked-garden-core")],
        );
        std::fs::write(
            latest.join(".claude-plugin").join("plugin.json"),
            "not json",
        )
        .unwrap();
        let mut lines = Vec::new();
        let ladder =
            resolve_ladder_in(None, Some(config.clone()), None, &mut collect(&mut lines)).unwrap();
        let Ladder::Failed(why) = &ladder else {
            panic!("a malformed manifest is a failure, not absence: {ladder:?}");
        };
        assert!(
            why.contains("2.0.0") && why.contains("malformed"),
            "names the candidate and the defect: {why}"
        );
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("skills.fallback FAILED") && lines[0].contains(why.as_str()),
            "{lines:?}"
        );
        // Through admission (the process env): a skill-naming run is refused WITH the reason; a
        // skill-free one proceeds with no root.
        let _snap = Pin::set(SKILLS_SNAPSHOT_ENV, None);
        let _config = Pin::set(
            crate::acp_runner::CLAUDE_CONFIG_DIR_ENV,
            Some(config.as_os_str()),
        );
        let input = |skill: Option<&str>| {
            let mut u = crate::domain::WorkUnit::pending("r:u1", "r", 1, "do");
            u.skill_ref = skill.map(str::to_string);
            StepInput {
                run_id: "r".to_string(),
                unit_ix: 0,
                attempt: 0,
                unit: u,
                workflow_id: "wf".to_string(),
                entity_mode: crate::scope::EntityMode::Isolated,
                workdir: None,
                governance: None,
                prior_outputs: vec![],
                elicitation_epoch: 0,
                process_gen: None,
                launch_seq: 0,
                required_skills: Vec::new(),
            }
        };
        let err = admit_turn(
            Turn::Fresh,
            &input(Some("wicked-garden-core")),
            &WorkerCli::Claude,
        )
        .expect_err("a failed ladder refuses a skill-naming run");
        assert_eq!(
            err,
            SkillsError::FallbackFailed {
                why: why.clone(),
                missing: vec!["wicked-garden-core".to_string()],
            }
        );
        let text = err.to_string();
        assert!(
            text.contains("could not be used as the fallback")
                && text.contains("not merely absent")
                && text.contains(why.as_str())
                && text.contains("wicked-garden-core"),
            "{text}"
        );
        assert_eq!(
            admit_turn(Turn::Fresh, &input(None), &WorkerCli::Claude),
            Ok(None),
            "a skill-free run proceeds without a root, as under absence"
        );
        let mut tool = input(None);
        tool.unit.tool_cmd = Some(vec!["true".to_string()]);
        tool.required_skills = vec!["wicked-garden-core".to_string()];
        assert!(
            matches!(admit_plan(&tool), Err(SkillsError::FallbackFailed { .. })),
            "the tool-command admission carries the reason too"
        );
        // A symlinked version candidate and an unlistable cache are failures too.
        #[cfg(unix)]
        {
            std::fs::write(
                latest.join(".claude-plugin").join("plugin.json"),
                "{\"name\":\"wicked-garden\",\"version\":\"2.0.0\"}",
            )
            .unwrap();
            let elsewhere = base.join("elsewhere");
            std::fs::create_dir_all(&elsewhere).unwrap();
            std::os::unix::fs::symlink(&elsewhere, cache.join("3.0.0")).unwrap();
            let ladder = resolve_ladder_in(None, Some(config.clone()), None, &mut |_| {}).unwrap();
            assert!(
                matches!(&ladder, Ladder::Failed(why) if why.contains("3.0.0") && why.contains("symlink")),
                "{ladder:?}"
            );
            std::fs::remove_file(cache.join("3.0.0")).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o000)).unwrap();
            let ladder = resolve_ladder_in(None, Some(config.clone()), None, &mut |_| {}).unwrap();
            std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o755)).unwrap();
            // root ignores mode bits: the listing succeeds there, so the failure is only
            // observable for an unprivileged runner.
            if !is_root_user() {
                assert!(
                    matches!(&ladder, Ladder::Failed(why) if why.contains("cannot be listed")),
                    "permission denied is a failure, not absence: {ladder:?}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    fn is_root_user() -> bool {
        // SAFETY: `geteuid` reads the process's effective uid; no pointers, no state.
        unsafe { libc::geteuid() == 0 }
    }

    /// The snapshot's STATE HOME is derived from its own shape at load (codex round 3; v3.4 §2):
    /// a root in a custom state home derives THAT directory — never a `.wicked-crew` basename,
    /// never a companion variable — and a root whose parent is not literally `snapshots` or whose
    /// grandparent is not literally `skills` is a config error naming the path; the live cache
    /// has no state home.
    #[test]
    fn the_state_home_is_derived_from_the_snapshot_shape_alone() {
        let base = scratch("state-home");
        let crew_state = base.join("crew-state");
        let root = snapshot_root(
            &gen_dir(&crew_state, "1"),
            "1",
            &[("domain", "wicked-garden-domain")],
        );
        let s = published(&root).unwrap().unwrap();
        assert_eq!(
            s.state_home.as_deref(),
            Some(crew_state.as_path()),
            "a custom state home is derived from the shape, not from a directory name"
        );
        for (parent, grandparent) in [("generations", SKILLS_DIR), ("snapshots", "plugins")] {
            let misshapen = snapshot_root(
                &base.join(grandparent).join(parent).join("2"),
                "2",
                &[("domain", "wicked-garden-domain")],
            );
            let err = published(&misshapen).expect_err("the shape is fixed");
            let SkillsError::Config { var, path, why } = &err else {
                panic!("expected Config, got {err:?}");
            };
            assert_eq!(*var, SKILLS_SNAPSHOT_ENV);
            assert_eq!(path, &misshapen);
            assert!(
                why.contains(&misshapen.display().to_string())
                    && why.contains("parent must be `snapshots`")
                    && why.contains("grandparent `skills`"),
                "{why}"
            );
        }
        let live = load_live(
            live_root(&base.join("live"), "1.0.0", &[]),
            SnapshotSource::LiveCache,
        )
        .unwrap();
        assert_eq!(live.state_home, None);
        let _ = std::fs::remove_dir_all(&base);
    }

    // ── Seat eligibility (routing, core#401) ─────────────────────────────────────────────────

    /// The routing-time judgement reads portability through the SAME closure the launch admission
    /// judges for the seat: a portable skill that MANDATES a non-portable one makes the unit
    /// Claude-only, naming the mandated skill (the one that needs the seat), not the portable
    /// parent; a portable skill with portable mandates is unconstrained; no ref, an empty ref and
    /// a ref the root does not hold are unconstrained (existence is the launch admission's
    /// refusal, by name, at the first unit). The live-cache fallback is Claude-only for ANY skill
    /// it holds, portable by the text approximation or not — nobody published a verdict.
    #[test]
    fn seat_requirement_follows_mandates_reads_only_a_published_verdict_and_skips_unknown_refs() {
        let base = scratch("seat-req");
        let root = snapshot_root_with(
            &gen_dir(&base, "9"),
            "9",
            &[
                ("a", "wicked-garden-a", true, &["wicked-garden-b"]),
                ("b", "wicked-garden-b", false, &[]),
                ("c", "wicked-garden-c", true, &["wicked-garden-d"]),
                ("d", "wicked-garden-d", true, &[]),
            ],
        );
        let s = load(&root);
        assert_eq!(seat_requirement(&s, None), SeatRequirement::Any);
        assert_eq!(seat_requirement(&s, Some("")), SeatRequirement::Any);
        assert_eq!(
            seat_requirement(&s, Some("wicked-garden-c")),
            SeatRequirement::Any,
            "portable through portable mandates"
        );
        assert_eq!(
            seat_requirement(&s, Some("wicked-garden-nope")),
            SeatRequirement::Any,
            "an unknown ref has no portability to read — existence refuses it at launch"
        );
        let SeatRequirement::ClaudeOnly { skills, why } =
            seat_requirement(&s, Some("wicked-garden-a"))
        else {
            panic!("a mandates the non-portable b");
        };
        assert_eq!(skills, vec!["wicked-garden-b".to_string()]);
        assert!(
            why.contains("wicked-garden-b")
                && !why.contains("wicked-garden-a")
                && why.contains("portable: false")
                && why.contains("gen=9")
                && why.contains(&root.display().to_string()),
            "{why}"
        );
        assert!(matches!(
            seat_requirement(&s, Some("wicked-garden-b")),
            SeatRequirement::ClaudeOnly { ref skills, .. } if skills == &["wicked-garden-b".to_string()]
        ));

        let live = load_live(
            live_root(&base.join("live"), "1.0.0", &[("d", "wicked-garden-d")]),
            SnapshotSource::LiveCache,
        )
        .unwrap();
        assert!(live.skill("wicked-garden-d").unwrap().portable);
        let SeatRequirement::ClaudeOnly { skills, why } =
            seat_requirement(&live, Some("wicked-garden-d"))
        else {
            panic!("the fallback is Claude-only");
        };
        assert_eq!(skills, vec!["wicked-garden-d".to_string()]);
        assert!(
            why.contains("live plugin cache")
                && why.contains("no publish-time portability verdict")
                && why.contains("wicked-garden-d"),
            "{why}"
        );
        assert_eq!(
            seat_requirement(&live, Some("wicked-garden-nope")),
            SeatRequirement::Any,
            "unknown under the fallback too: existence refuses it at launch"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// What distribution judges against: the ladder's root when it has one; nothing — no
    /// constraint, no refusal of its own — when the ladder is absent or failed (silently: the
    /// ladder logged its own step) or misconfigured (logged here, since the refusal it produces is
    /// the launch's). The routing anticipates the ladder and never decides what it would not.
    #[test]
    fn routing_root_takes_only_a_resolved_root_and_logs_a_config_error() {
        let base = scratch("routing-root");
        let s = load(&snapshot_root(
            &gen_dir(&base, "3"),
            "3",
            &[("core", "wicked-garden-core")],
        ));
        let mut lines = Vec::new();
        assert_eq!(
            routing_root(Ok(Ladder::Root(s.clone())), &mut collect(&mut lines)),
            Some(s)
        );
        assert_eq!(
            routing_root(Ok(Ladder::Absent), &mut collect(&mut lines)),
            None
        );
        assert_eq!(
            routing_root(
                Ok(Ladder::Failed("cache unreadable".into())),
                &mut collect(&mut lines)
            ),
            None
        );
        assert!(
            lines.is_empty(),
            "absence and failure were logged by the ladder: {lines:?}"
        );
        let err = SkillsError::Config {
            var: SKILLS_SNAPSHOT_ENV,
            path: base.join("nowhere"),
            why: "not a snapshot".into(),
        };
        assert_eq!(
            routing_root(Err(err.clone()), &mut collect(&mut lines)),
            None
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("skills.routing")
                && lines[0].contains(&err.to_string())
                && lines[0].contains("launch admission decides"),
            "{}",
            lines[0]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The plan-time refusal names everything the operator needs to act: the unit, the skills,
    /// the seat kind only they can run on, WHY (portability, from the snapshot), and the roster
    /// that lacks it — and says it happened before any unit ran.
    #[test]
    fn no_eligible_seat_names_unit_skills_seat_kind_reason_and_roster() {
        let text = SkillsError::NoEligibleSeat {
            ord: 1,
            skills: vec!["wicked-garden-repo-learn".into()],
            required_seat: NONPORTABLE_SEAT,
            roster: vec!["copilot".into(), "pi".into()],
            why: "the skills snapshot at /s (gen=7) marks wicked-garden-repo-learn as portable: \
                  false"
                .into(),
        }
        .to_string();
        for needle in [
            "unit 1 requires wicked-garden-repo-learn",
            "only a claude seat can be handed",
            "portable: false",
            "roster [copilot, pi] holds no seat that resolves to claude on both carriers",
            "not the key's spelling",
            "add a claude seat to the roster",
            "refused at plan time, before any unit ran",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in: {text}");
        }
    }
}
