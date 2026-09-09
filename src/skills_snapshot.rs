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
//!   non-denied path. The state home is the ACTUAL one — DERIVED from the snapshot's own path
//!   (`<state home>/skills/snapshots/<gen>`, three components up: `state_home::of_snapshot`), and
//!   required to agree with `WICKED_CREW_STATE_HOME` when the daemon states one — never a
//!   `.wicked-crew` basename: a scratch daemon on a custom state home is fenced exactly like the
//!   default one. A snapshot without that shape is a config error at load, and one anywhere else
//!   inside the fence FAILS the launch (`execute_wrapped::fence_check`, v3.1 §1); writes under
//!   it stay denied — a snapshot is immutable by contract, and this module never writes into one;
//! - the skill directive is CLI-aware (`execute_wrapped::plugin_skill_invocation`): the plugin
//!   form for Claude, the mirrored directory name for every other CLI.
//!
//! # Where the root comes from — the degradation ladder (v3 §3 / v3.1 §2), no unconditional fail-open
//!
//! The engine reads exactly ONE input. (`WICKED_SKILLS_CURRENT`, a second rung pass 1 added, is
//! withdrawn: crew resolves its `current` pointer and passes the concrete generation path.)
//!
//! 1. [`SKILLS_SNAPSHOT_ENV`] set ⇒ that path, strictly ([`load_published`]): it must be absolute,
//!    no ancestor may be a symlink (a link above a pinned generation could re-aim it later), it
//!    is pinned to its canonical real path (a FINAL-component link such as crew's `current` is
//!    followed once, at load; a DANGLING one is a config error naming the missing target), and
//!    its index must describe files that actually exist — every component from the root down
//!    (`.claude-plugin/`, `plugin.json`, `snapshot.json`, `skills/`, each skill directory, each
//!    `SKILL.md`) is lstat-verified NOT to be a symlink and read without following one. Any
//!    shortfall is a config error — a deliberately chosen snapshot is never silently swapped.
//!    Set but EMPTY is invalid explicit configuration, not "unset".
//! 2. Unset ⇒ the LIVE installed garden: the marketplace cache's highest version under the
//!    daemon's `CLAUDE_CONFIG_DIR` (else `~/.claude`), logged as `skills.fallback`. NOT the hand
//!    copy at `<config>/plugins/wicked-garden` — that stale copy is the defect being fixed, never
//!    a fallback. Nothing in the cache ⇒ no root at all (a run that needs a skill is then refused;
//!    a run that needs none proceeds).
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
//! and calls [`admit_unit`] (which resolves) only for a fresh session.
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
    /// The crew state home this generation was published under — DERIVED from the root's own
    /// shape (`<state home>/skills/snapshots/<gen>`, `state_home::derive`) and, when the daemon
    /// states `WICKED_CREW_STATE_HOME`, checked to agree with it. The worker Read fence over that
    /// directory is the registry (`execute_wrapped::deny_rules`). `None` for a fallback root,
    /// which has no state home (it sits in the claude config dir).
    pub state_home: Option<PathBuf>,
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

    /// The copilot view VERIFIED for the skills a copilot seat will invoke (codex round 3; pass 2
    /// checked only that `views/copilot` existed, which followed a link at `views` and admitted
    /// an empty view). Every component from `views` down is lstat-walked — `views`,
    /// `views/copilot`, `.github`, `.github/skills`, each required skill's directory and its
    /// `SKILL.md` — and none may be a symlink: a link at `views` would hand the worker an
    /// external tree through `--add-dir`. Every required skill must be PRESENT in the view as
    /// `.github/skills/<name>/SKILL.md` with a frontmatter `name` equal to the skill's.
    ///
    /// `Ok(None)` when the generation publishes no view at all (`views` or `views/copilot`
    /// absent — the caller decides whether that matters); `Err(Config)` for a symlink or a
    /// non-directory on the walk (containment, not absence); `Err(Missing)` naming the skills the
    /// view does not hold as stated — an EMPTY or partial view is missing every required skill it
    /// lacks.
    pub(crate) fn copilot_view_for(
        &self,
        required: &[&SkillEntry],
    ) -> Result<Option<PathBuf>, SkillsError> {
        let config = |why: String| SkillsError::Config {
            var: SKILLS_SNAPSHOT_ENV,
            path: self.root.clone(),
            why,
        };
        let linked = |rel: &str| {
            format!("{rel} is a symlink — a snapshot's views must be contained in it, not linked")
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
        let skills_dir = view.join(".github").join("skills");
        // `.github` / `.github/skills` absent ⇒ an EMPTY view: every required skill is missing.
        let mut empty = false;
        for (p, rel) in [
            (view.join(".github"), "views/copilot/.github"),
            (skills_dir.clone(), "views/copilot/.github/skills"),
        ] {
            match std::fs::symlink_metadata(&p) {
                Err(_) => {
                    empty = true;
                    break;
                }
                Ok(m) if m.file_type().is_symlink() => return Err(config(linked(rel))),
                Ok(m) if !m.is_dir() => return Err(config(format!("{rel} is not a directory"))),
                Ok(_) => {}
            }
        }
        let mut missing: Vec<String> = Vec::new();
        for entry in required {
            if empty {
                missing.push(entry.name.clone());
                continue;
            }
            let rel_dir = format!("views/copilot/.github/skills/{}", entry.name);
            let dir = skills_dir.join(&entry.name);
            match std::fs::symlink_metadata(&dir) {
                Ok(m) if m.file_type().is_symlink() => return Err(config(linked(&rel_dir))),
                Ok(m) if m.is_dir() => {}
                _ => {
                    missing.push(entry.name.clone());
                    continue;
                }
            }
            let file = dir.join(SKILL_FILE);
            match std::fs::symlink_metadata(&file) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(config(linked(&format!("{rel_dir}/{SKILL_FILE}"))))
                }
                Ok(m) if m.is_file() => {}
                _ => {
                    missing.push(entry.name.clone());
                    continue;
                }
            }
            let held = read_no_follow(&file)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .and_then(|text| parse_frontmatter(&text).ok())
                .and_then(|fm| fm.name);
            if held.as_deref() != Some(entry.name.as_str()) {
                missing.push(entry.name.clone());
            }
        }
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

    /// What a launch on `cli` is handed from this root, in its lever's shape (v3.2 §2).
    pub(crate) fn delivery(&self, cli: &WorkerCli) -> SkillsDelivery {
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
}

impl std::fmt::Display for SkillsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillsError::Config { var, path, why } if *var == crate::state_home::STATE_HOME_ENV => {
                write!(
                    f,
                    "{var}={} is not a usable crew state home ({why}); pass the daemon's actual \
                     state home — the directory whose skills/snapshots/<gen> holds the published \
                     generation — or unset it to derive the state home from the snapshot path",
                    path.display()
                )
            }
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
    // The daemon's explicit state home (crew#480), when stated: resolved here so a malformed
    // value — set but empty, relative, unresolvable — refuses the launch as a config error
    // naming it, whether or not a snapshot is handed (the fence over it depends on it).
    let state_home =
        crate::state_home::explicit_state_home().map_err(|why| SkillsError::Config {
            var: crate::state_home::STATE_HOME_ENV,
            path: std::env::var_os(crate::state_home::STATE_HOME_ENV)
                .map(PathBuf::from)
                .unwrap_or_default(),
            why,
        })?;
    resolve_in(
        explicit,
        state_home.as_deref(),
        std::env::var_os(crate::acp_runner::CLAUDE_CONFIG_DIR_ENV).map(PathBuf::from),
        home_dir(),
        &mut |line| eprintln!("{line}"),
    )
}

/// [`resolve`] with its inputs and its log sink explicit, so the ladder is testable without
/// touching the process environment and the "logged" half of each step is asserted, not assumed.
/// `explicit_state_home` is the daemon's resolved `WICKED_CREW_STATE_HOME`, if any — a published
/// snapshot's derived state home must agree with it ([`crate::state_home::derive`]).
pub(crate) fn resolve_in(
    explicit: Option<PathBuf>,
    explicit_state_home: Option<&Path>,
    claude_config_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    log: &mut dyn FnMut(String),
) -> Result<Option<SkillsSnapshot>, SkillsError> {
    if let Some(path) = explicit {
        return load_published(SKILLS_SNAPSHOT_ENV, &path, explicit_state_home).map(Some);
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
        .filter(|latest| {
            plugin_manifest_name(latest)
                .ok()
                .flatten()
                .is_some_and(|name| name == PLUGIN_NAME)
        })
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
///   flipped under it. A DANGLING final link (`lstat` succeeds, `canonicalize` fails — crew's
///   `current` aimed at a reaped or not-yet-published generation) is a config error that names
///   the target it points at; a loop is what the OS reports.
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
    match std::fs::canonicalize(named) {
        Ok(real) => Ok(simplify_verbatim(real)),
        Err(e) => {
            // Both halves of the dangling case are handled explicitly: the lstat SUCCEEDS (the
            // link exists) while canonicalize FAILS (its target does not) — so the error names
            // the target, which is the generation the operator must publish or stop pointing at.
            if std::fs::symlink_metadata(named).is_ok_and(|m| m.file_type().is_symlink()) {
                let target = std::fs::read_link(named)
                    .map(|t| t.display().to_string())
                    .unwrap_or_else(|_| "<unreadable>".to_string());
                return Err(format!(
                    "it is a symlink to `{target}`, which cannot resolve to a real path ({e}) — a \
                     dangling link is not a snapshot; publish the generation it names or pass a \
                     concrete generation path"
                ));
            }
            Err(format!("cannot resolve it to a real path: {e}"))
        }
    }
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
/// the path and the reason — this path was chosen deliberately, so nothing here degrades. The
/// generation's STATE HOME is derived from the canonical root's shape and must agree with
/// `explicit_state_home` when the daemon stated one ([`crate::state_home::derive`]).
fn load_published(
    var: &'static str,
    named: &Path,
    explicit_state_home: Option<&Path>,
) -> Result<SkillsSnapshot, SkillsError> {
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
    // A valid plugin root, indexed and contained — now WHERE it is: the state home whose fence
    // is opened around it is derived from the root's own shape (three components up), and must
    // be the one the daemon states when it states one. Checked last so an operator pointing at
    // something that is not a snapshot at all is told that first.
    let state_home = crate::state_home::derive(path, explicit_state_home).map_err(config_err)?;
    Ok(SkillsSnapshot {
        root,
        source: SnapshotSource::Published,
        gen: Some(gen),
        content_hash,
        state_home: Some(state_home),
        skills,
    })
}

/// The `SKILL.md` an index entry names, verified: `dir` is a clean relative `/`-path, EVERY
/// component from the root down — `skills/` itself first, then each directory of `dir`, then the
/// leaf — exists and is NOT a symlink (lstat walk — the file must be contained in the root, not
/// pointed at from it; a `skills -> /outside` link is refused at `skills`, before anything under
/// it is looked at), the leaf is a regular file, and it is readable (no-follow) with a
/// frontmatter block. Returns the frontmatter.
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
        state_home: None,
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
            let text = read_no_follow(&skill_md)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok());
            match text.as_deref().map(parse_frontmatter) {
                Some(Ok(Frontmatter {
                    name: Some(name),
                    mandates,
                })) => out.push(SkillEntry {
                    name,
                    dir,
                    portable: !has_nonportable_markers(text.as_deref().unwrap_or_default()),
                    mandates,
                }),
                Some(Err(FrontmatterError::Malformed(why))) => log(format!(
                    "[wicked-core] skills.notice {}/{SKILLS_DIR}/{dir}/{SKILL_FILE} has malformed \
                     frontmatter ({why}); the skill is not indexed",
                    root.display()
                )),
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
    let Some(snapshot) = snapshot else {
        let mut missing: Vec<String> = plan.into_iter().map(str::to_string).collect();
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
    let existence = snapshot.closure(plan);
    if !existence.missing.is_empty() {
        return Err(SkillsError::Missing {
            root: Some(snapshot.root.clone()),
            missing: existence.missing,
        });
    }
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
            // directory lever — the parent's directory would carry the Claude-only child into a
            // recursive scan (codex round 3). Refused by name, parent and child both.
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

/// The launch admission for one FRESH launch, on either spawn path: resolve the root (the
/// ladder), refuse a root the worker Read fence would deny ([`fence_admit`]), then require every
/// skill the run names — see [`admit_refs`]. `Ok(None)` ⇒ the unit needs no skill and there is no
/// root to hand it. A CACHED ACP session does not come through here: it is admitted against its
/// pinned snapshot with [`admit_refs`] directly (v3.1 §4).
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
    if let Some(s) = &snapshot {
        fence_admit(s)?;
    }
    admit_refs(snapshot, &RequiredRefs::of(input), cli)
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
fn fence_admit(snapshot: &SkillsSnapshot) -> Result<(), SkillsError> {
    match crate::execute_wrapped::fence_check(&snapshot.root) {
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
            &gen_dir(&base, "7"),
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
        assert_eq!(
            s.state_home.as_deref(),
            Some(base.as_path()),
            "the state home is the directory three components above the generation"
        );
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
        let numeric = snapshot_root(&gen_dir(&base.join("num"), "9"), "9", &[]);
        std::fs::write(numeric.join(SNAPSHOT_INDEX), "{\"gen\":12,\"skills\":[]}").unwrap();
        let s = published(&numeric).unwrap().unwrap();
        assert_eq!(s.gen.as_deref(), Some("12"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The ONE input (v3.1 §2): a variable that is SET BUT EMPTY is invalid explicit
    /// configuration — a config error, not "unset" — and the withdrawn second input
    /// (`WICKED_SKILLS_CURRENT`) is not read at all: set to anything, empty included, it neither
    /// steers the ladder nor breaks it.
    #[test]
    fn an_empty_explicit_value_is_a_config_error_not_unset_and_there_is_no_second_input() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        const WITHDRAWN: &str = "WICKED_SKILLS_CURRENT";
        const STATE_HOME: &str = crate::state_home::STATE_HOME_ENV;
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            [SKILLS_SNAPSHOT_ENV, WITHDRAWN, STATE_HOME]
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect();
        std::env::remove_var(SKILLS_SNAPSHOT_ENV);
        std::env::remove_var(STATE_HOME);
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

        // The daemon's explicit state home (codex round 3): set but EMPTY, relative, or not a
        // real directory ⇒ a config error naming the variable — the fence depends on it;
        // agreeing with the snapshot's derived state home ⇒ loads; another directory ⇒ a config
        // error naming both.
        let expect_state_home_err = |value: &str, needle: &str| {
            std::env::set_var(STATE_HOME, value);
            let err = resolve().expect_err(needle);
            let SkillsError::Config { var, why, .. } = &err else {
                panic!("expected Config, got {err:?}");
            };
            assert_eq!(*var, STATE_HOME, "{err}");
            assert!(why.contains(needle), "{why}");
            assert!(
                err.to_string().contains(STATE_HOME) && err.to_string().contains("crew state home"),
                "{err}"
            );
        };
        expect_state_home_err("", "set but empty");
        expect_state_home_err("relative/state", "relative");
        expect_state_home_err(
            &base.join("does-not-exist").display().to_string(),
            "cannot be resolved",
        );
        std::env::set_var(STATE_HOME, &base);
        assert_eq!(
            resolve().unwrap().unwrap().state_home.as_deref(),
            Some(base.as_path())
        );
        let other = base.join("other-state");
        std::fs::create_dir_all(&other).unwrap();
        std::env::set_var(STATE_HOME, &other);
        let err = resolve().expect_err("a different state home");
        let SkillsError::Config { var, why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert_eq!(*var, SKILLS_SNAPSHOT_ENV);
        assert!(
            why.contains(&base.display().to_string()) && why.contains(&other.display().to_string()),
            "names both directories: {why}"
        );
        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        let _ = std::fs::remove_dir_all(&base);
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

        let first = published(&current).unwrap().unwrap();
        assert_eq!(
            first.root, gen7,
            "the link is resolved to the real generation it names"
        );
        assert_eq!(first.gen.as_deref(), Some("7"));

        // crew publishes gen 8 and flips `current` — the session already handed gen 7 is unaffected.
        std::fs::remove_file(&current).unwrap();
        std::os::unix::fs::symlink(&gen8, &current).unwrap();
        let second = published(&current).unwrap().unwrap();
        assert_eq!(second.root, gen8, "an absolute link target resolves too");
        assert_eq!(second.gen.as_deref(), Some("8"));
        assert_eq!(
            first.root, gen7,
            "the earlier snapshot still names its own generation"
        );
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

        // A DANGLING link — crew's `current` aimed at a reaped or unpublished generation: lstat
        // succeeds (the link exists), canonicalize fails (its target does not). Both halves are
        // handled: a config error naming the path as given AND the target it points at.
        let dangling = base.join("current-dangling");
        std::os::unix::fs::symlink("snapshots/99", &dangling).unwrap();
        assert!(std::fs::symlink_metadata(&dangling).is_ok());
        let err = published(&dangling).expect_err("a dangling link is not a snapshot");
        let SkillsError::Config { path, why, .. } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert_eq!(path, &dangling);
        assert!(
            why.contains("snapshots/99") && why.contains("dangling"),
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
            &gen_dir(&base, "4"),
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
            &RequiredRefs::seat([
                "wicked-garden-domain",
                "wicked-testing-acceptance-test-writer",
            ]),
            &claude,
        )
        .expect("both are present");
        assert_eq!(ok.as_ref().map(|s| &s.root), Some(&root));

        // A foreign-family skill that is NOT is refused — there is no exemption by family.
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
                && msg.contains("added to the effective root"),
            "the refusal says why the snapshot could not hold it by default: {msg}"
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
            .retain(|e| e["dir"] != "core");
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
        // A copy whose frontmatter disagrees is not that skill.
        copy(
            "wicked-garden-engineering-frontend",
            "wicked-garden-something-else",
        );
        let err = admit_refs(Some(s.clone()), &both, &copilot).expect_err("name mismatch");
        assert!(
            matches!(&err, SkillsError::Missing { missing, .. }
                if missing == &vec!["wicked-garden-engineering-frontend".to_string()]),
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
            let ls = load(&linked_root);
            let err = admit_refs(
                Some(ls.clone()),
                &RequiredRefs::seat(["wicked-garden-domain"]),
                &copilot,
            )
            .expect_err("a linked view is an external tree");
            assert!(
                matches!(&err, SkillsError::Config { why, .. }
                    if why.contains("views/copilot is a symlink")),
                "{err:?}"
            );
            assert!(
                matches!(
                    admit_refs(Some(ls.clone()), &RequiredRefs::seat([]), &copilot),
                    Err(SkillsError::Config { .. })
                ),
                "refused even when nothing is invoked"
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
        // …and the live walk skips it with a notice naming the file and the reason.
        let live = live_root(&base.join("live"), "1.0.0", &[("qe", "wicked-garden-qe")]);
        std::fs::write(
            skill_dir(&live, "qe").join(SKILL_FILE),
            "---\nname: wicked-garden-qe\nmandates:\n\t- x\n---\n",
        )
        .unwrap();
        let mut lines = Vec::new();
        let s = load_live(live, SnapshotSource::LiveCache, &mut collect(&mut lines));
        assert!(s.skills().is_empty(), "{:?}", s.skills());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("malformed frontmatter") && lines[0].contains("skills/qe/SKILL.md"),
            "{lines:?}"
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
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The snapshot's STATE HOME is derived from its own shape at load (codex round 3): a root in
    /// a custom state home derives THAT directory — never a `.wicked-crew` basename — and agrees
    /// with an explicit statement of the same directory; a different explicit state home is a
    /// config error naming both; the live cache has none.
    #[test]
    fn the_state_home_is_derived_from_the_snapshot_and_must_agree_with_an_explicit_one() {
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
        let same = resolve_in(
            Some(root.clone()),
            Some(&crew_state),
            None,
            None,
            &mut |_| {},
        )
        .unwrap()
        .unwrap();
        assert_eq!(same.state_home.as_deref(), Some(crew_state.as_path()));
        let other = base.join("other-state");
        std::fs::create_dir_all(&other).unwrap();
        let err = resolve_in(Some(root.clone()), Some(&other), None, None, &mut |_| {})
            .expect_err("the two must agree");
        let SkillsError::Config { var, path, why } = &err else {
            panic!("expected Config, got {err:?}");
        };
        assert_eq!(*var, SKILLS_SNAPSHOT_ENV);
        assert_eq!(path, &root);
        assert!(
            why.contains(&crew_state.display().to_string())
                && why.contains(&other.display().to_string())
                && why.contains(crate::state_home::STATE_HOME_ENV),
            "names both: {why}"
        );
        let live = load_live(
            live_root(&base.join("live"), "1.0.0", &[]),
            SnapshotSource::LiveCache,
            &mut |_| {},
        );
        assert_eq!(live.state_home, None);
        let _ = std::fs::remove_dir_all(&base);
    }
}
