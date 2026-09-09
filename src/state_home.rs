//! The worker Read fence over crew's state home as an EXPLICIT, tested denylist (skills keystone
//! design v3.1 §1; core#396 review passes 2 and 3).
//!
//! # Why a registry and not a blanket
//!
//! Every worker is fenced off from the daemon's state home — the operational store, the event
//! log, the audit log, the project graphs: `Read(<state home>/**)` for the file tools. The
//! published skills snapshot a worker is handed (`WICKED_SKILLS_SNAPSHOT`) lives under that SAME
//! root — `<state home>/skills/snapshots/<gen>/` — because the skills root is one of crew's stores
//! and the operator keeps ONE storage root. Claude's deny rules win over any allow, so no allow
//! rule can open the snapshot under a blanket deny: the deny itself must not cover it.
//!
//! Pass 1 solved that by ENUMERATING the state home's siblings at launch and denying each one —
//! which fails open: an entry created after the listing is unfenced, a listing error leaves a
//! level unfenced, and an unspellable name is skipped. So the rule list is STATIC — this module's
//! registry, embedded from [`REGISTRY_JSON`] (the same file crew mirrors and audits against its
//! own stores) — and the launch-time listing only ever REFUSES: an entry the registry does not
//! classify fails the launch by name ([`read_rules_around_snapshot`]). Fail closed, never widen;
//! no runtime enumeration builds a rule.
//!
//! # Which directory IS the state home (pass 3; design v3.4 §2)
//!
//! Pass 2 recognized the state home by its default basename, `~/.wicked-crew` — so a daemon
//! running on a custom state home (`crewStateHome()` is the `--db` parent: a scratch daemon on
//! `/private/tmp/crew-state`, say) handed a snapshot from `/private/tmp/crew-state/skills/
//! snapshots/1` that passed the fence without that daemon's sibling stores ever being classified
//! or denied. The state home is DERIVED from the snapshot itself, and from NOTHING else: a
//! published generation is `<state home>/skills/snapshots/<gen>` by contract (the parent literally
//! `snapshots`, the grandparent literally `skills`), so the state home is three components up from
//! the resolved (canonical) root ([`of_snapshot`]) — and a snapshot whose ancestors do not have
//! that shape is a config error naming the path, never "not under the fence". The engine reads
//! exactly ONE skills input, `WICKED_SKILLS_SNAPSHOT`; the round-4 companion variable
//! (`WICKED_CREW_STATE_HOME`, "passed alongside") is RETIRED as an engine input (v3.4 §2) and is
//! not read anywhere — crew's engine-env stops exporting it. The default `~/.wicked-crew` keeps
//! its blanket rule whenever it is not the state home in play. Residual, stated: a CUSTOM state
//! home is fenced only through a snapshot handed from it — with no snapshot handed (the live-cache
//! rung) nothing derives it, and only the default directory is fenced.
//!
//! Every classified top-level entry is also checked for its ACTUAL kind (codex round 6): the
//! registry declares each entry `file`, `dir` or `file-with-sidecars`, and the launch-time listing
//! `lstat`s each entry it classified — a symlink of any name, a directory named `audit.log`, a
//! file named `daemon-x` — refuses the launch naming the entry. The rule emitted for an entry
//! follows its DECLARED kind (`/**` for anything that can have children), so an entry whose kind
//! on disk disagrees would otherwise pass admission and receive a rule that does not cover it.
//!
//! The only non-denied path under the state home is the HANDED `skills/snapshots/<gen>/` — and
//! only that one (design v3.3 §1; codex round 4). `skills/snapshots/` is the ONE directory listed
//! at launch to BUILD rules: one deny per sibling entry — every older and newer generation, every
//! staging and temp dir mid-publish — in addition to the static registry rules, and it fails
//! closed: an unlistable slot refuses the launch, and so does an entry that is neither a
//! generation directory nor a recognised staging name. A worker handed generation 7 cannot read
//! generation 6 (nor the skills a later publish disabled) — round 3 left every sibling readable,
//! which exceeded the single-generation exception v3.1 grants.
//!
//! Residuals, stated: a top-level entry created WHILE a session runs is fenced at the next launch
//! (crew is the only writer of that directory, and the listing refuses it then); a generation
//! published while a session runs is readable by that session until its next launch —
//! generations are immutable and hold only the enabled skills of a newer publish.

use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;

/// The DEFAULT state home's directory name under `$HOME` — always a fenced entry
/// (`execute_wrapped::DENIED_HOME_SUBDIRS` lists it), and the state home in play only when the
/// handed snapshot's derived state home is that very directory.
pub(crate) const DEFAULT_STATE_HOME_DIRNAME: &str = ".wicked-crew";

/// The registry, embedded so the binary and the fixture crew mirrors cannot drift: the test
/// suite parses this same text, and `tests/fixtures/state-home-subtrees.json` is the file crew
/// copies into `packages/crew/tests/fixtures/`.
pub(crate) const REGISTRY_JSON: &str = include_str!("../tests/fixtures/state-home-subtrees.json");

/// How an entry claims a top-level name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Claim {
    /// The exact name.
    Name(String),
    /// The name and every sidecar sharing its prefix (`core.db`, `core.db-wal`, `core.db.events`).
    Prefix(String),
}

/// One registered top-level entry of the state home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub claim: Claim,
    /// `file`, `dir` or `file-with-sidecars` — decides whether a `/**` rule is emitted.
    pub kind: String,
    /// The skills root only: the child directory whose generations are the read slot
    /// (`snapshots`), and the children denied beneath the entry. A child spelled with a `/` is a
    /// PATTERN rule below the slot (`snapshots/.staging-*`), not a classified name.
    pub read_slot: Option<String>,
    pub denied_children: Vec<String>,
}

impl Entry {
    /// The top-level name this entry is spelled by in a rule (`core.db*` for a prefix).
    fn rule_name(&self) -> String {
        match &self.claim {
            Claim::Name(n) => n.clone(),
            Claim::Prefix(p) => format!("{p}*"),
        }
    }

    fn claims(&self, top_level: &str) -> bool {
        match &self.claim {
            Claim::Name(n) => n == top_level,
            Claim::Prefix(p) => top_level.starts_with(p.as_str()),
        }
    }
}

/// The parsed registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Registry {
    pub version: u64,
    pub entries: Vec<Entry>,
}

impl Registry {
    /// The entry that classifies `top_level`, or `None` — the launch is then refused.
    pub(crate) fn classify(&self, top_level: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.claims(top_level))
    }

    /// The entry carrying the read slot (the skills root).
    pub(crate) fn skills_entry(&self) -> Option<&Entry> {
        self.entries.iter().find(|e| e.read_slot.is_some())
    }

    /// `(skills root name, read slot name)` — the two components that, with a generation, make
    /// the shape `<state home>/skills/snapshots/<gen>`. `None` when the registry names no slot.
    fn slot_shape(&self) -> Option<(&str, &str)> {
        let skills = self.skills_entry()?;
        let Claim::Name(name) = &skills.claim else {
            return None;
        };
        Some((name.as_str(), skills.read_slot.as_deref()?))
    }
}

/// Parse a registry document. Strict: every entry has exactly one of `name`/`prefix`, a `kind`,
/// and at most one entry carries a `read_slot`; a malformed registry is a reason to keep the
/// blanket fence, never to guess.
pub(crate) fn parse_registry(json: &str) -> Result<Registry, String> {
    let doc: Value = serde_json::from_str(json).map_err(|e| format!("not valid JSON: {e}"))?;
    let version = doc
        .get("version")
        .and_then(Value::as_u64)
        .ok_or("no numeric `version`")?;
    let raw = doc
        .get("entries")
        .and_then(Value::as_array)
        .ok_or("no `entries` array")?;
    let mut entries = Vec::with_capacity(raw.len());
    let mut slots = 0usize;
    for (i, e) in raw.iter().enumerate() {
        let name = e.get("name").and_then(Value::as_str);
        let prefix = e.get("prefix").and_then(Value::as_str);
        let claim = match (name, prefix) {
            (Some(n), None) if !n.is_empty() => Claim::Name(n.to_string()),
            (None, Some(p)) if !p.is_empty() => Claim::Prefix(p.to_string()),
            _ => {
                return Err(format!(
                    "entries[{i}] must have exactly one of `name`/`prefix`"
                ))
            }
        };
        let kind = e
            .get("kind")
            .and_then(Value::as_str)
            .filter(|k| ["file", "dir", "file-with-sidecars"].contains(k))
            .ok_or_else(|| format!("entries[{i}] has no `kind` of file/dir/file-with-sidecars"))?
            .to_string();
        let read_slot = e
            .get("read_slot")
            .and_then(Value::as_str)
            .map(str::to_string);
        if read_slot.is_some() {
            slots += 1;
        }
        // Strict (Copilot, review pass 7): a `denied_children` that is not an array, or an entry
        // that is not a string, is a parse error — never silently dropped, which would change the
        // fence without an obvious failure.
        let denied_children: Vec<String> = match e.get("denied_children") {
            None => Vec::new(),
            Some(Value::Array(a)) => a
                .iter()
                .enumerate()
                .map(|(j, c)| {
                    c.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| format!("entries[{i}] denied_children[{j}] is not a string"))
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err(format!("entries[{i}] `denied_children` is not an array")),
        };
        if read_slot.is_none() && !denied_children.is_empty() {
            return Err(format!(
                "entries[{i}] lists `denied_children` without a `read_slot`"
            ));
        }
        for child in &denied_children {
            if child.is_empty()
                || child
                    .split('/')
                    .any(|c| c.is_empty() || c == "." || c == "..")
                || child.contains('\\')
            {
                return Err(format!(
                    "entries[{i}] denied child `{child}` is not a clean relative `/`-path"
                ));
            }
        }
        entries.push(Entry {
            claim,
            kind,
            read_slot,
            denied_children,
        });
    }
    if slots > 1 {
        return Err("more than one entry carries a `read_slot`".to_string());
    }
    Ok(Registry { version, entries })
}

/// The embedded registry, parsed once. `Err` ⇒ the fence stays a blanket (fail closed) and the
/// reason is logged by the caller; the test suite makes this unreachable for a shipped binary.
pub(crate) fn registry() -> Result<&'static Registry, &'static str> {
    static PARSED: OnceLock<Result<Registry, String>> = OnceLock::new();
    PARSED
        .get_or_init(|| parse_registry(REGISTRY_JSON))
        .as_ref()
        .map_err(String::as_str)
}

fn normal(c: Component<'_>) -> Option<&str> {
    match c {
        Component::Normal(s) => s.to_str(),
        _ => None,
    }
}

/// The state home a snapshot root sits in, by SHAPE alone (pure — nothing on disk is consulted):
/// `root` is `<state home>/skills/snapshots/<gen>` — the registry's skills root, its read slot,
/// then one generation whose name does not start with `.` (staging and temp dirs are not
/// generations) — and the state home is the ancestor three components up. `None` for any other
/// spelling, the filesystem root included. The root is expected canonical (a published snapshot
/// is pinned to its real path at load), so the derived state home is canonical too.
pub(crate) fn of_snapshot(root: &Path) -> Option<PathBuf> {
    let (skills_name, slot) = registry().ok()?.slot_shape()?;
    let mut back = root.components().rev();
    let gen = normal(back.next()?)?;
    if gen.starts_with('.') {
        return None;
    }
    if normal(back.next()?)? != slot || normal(back.next()?)? != skills_name {
        return None;
    }
    let home = root.ancestors().nth(3)?;
    // The filesystem root (or a bare Windows prefix) is not anyone's state home.
    home.parent()?;
    Some(home.to_path_buf())
}

/// The state home of a published snapshot at `root` (canonical), derived from its shape alone
/// ([`of_snapshot`]; design v3.4 §2): `<state home>/skills/snapshots/<gen>` — the parent literally
/// the registry's read slot (`snapshots`), the grandparent literally the skills root (`skills`).
/// Any other spelling is a config error naming the path. Nothing else is consulted — the engine
/// reads one skills input and no companion variable.
pub(crate) fn derive(root: &Path) -> Result<PathBuf, String> {
    of_snapshot(root).ok_or_else(|| {
        format!(
            "its path `{}` does not have the shape `<state home>/{}/{}/<gen>` that crew publishes \
             generations in (the parent must be `{}`, the grandparent `{}`) — the worker Read \
             fence over the daemon's state home is derived from that shape and from nothing else, \
             so a snapshot anywhere else cannot be fenced around; publish it under the state \
             home's skills root and pass that concrete generation path",
            root.display(),
            skills_name_or_default(),
            slot_or_default(),
            slot_or_default(),
            skills_name_or_default()
        )
    })
}

fn skills_name_or_default() -> String {
    registry()
        .ok()
        .and_then(|r| r.slot_shape())
        .map_or_else(|| "skills".to_string(), |(n, _)| n.to_string())
}

fn slot_or_default() -> String {
    registry()
        .ok()
        .and_then(|r| r.slot_shape())
        .map_or_else(|| "snapshots".to_string(), |(_, s)| s.to_string())
}

/// Do `a` and `b` name the same directory — spelled identically, or resolving to the same real
/// path (a home directory that is a symlink; a `\\?\`-prefixed spelling)? A path that cannot be
/// resolved is compared by spelling only.
pub(crate) fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ra), Ok(rb)) => ra == rb,
        _ => false,
    }
}

/// The spelling of `dir` that `root` sits under — `dir` itself, else its canonical form (a home
/// directory that is a symlink) — or `None` when `root` is not under `dir` at all.
pub(crate) fn base_under(dir: &Path, root: &Path) -> Option<PathBuf> {
    if root.starts_with(dir) {
        return Some(dir.to_path_buf());
    }
    let canonical = std::fs::canonicalize(dir).ok()?;
    root.starts_with(&canonical).then_some(canonical)
}

/// The Read rules for `state_home` with the HANDED generation (`handed_gen`, the final component
/// of the snapshot root) in its read slot as the ONLY non-denied path: one rule per registered
/// top-level entry (plus `/**` for anything that can have children); for the skills root one rule
/// per denied child (`baseline/**`, `effective/**`, `manifest.json`, `current`, `.uv-cache/**`,
/// `snapshots/.staging-*/**`, …); and — design v3.3 §1 — one rule per SIBLING entry of the read
/// slot, every entry of `skills/snapshots/` but `handed_gen`: older and newer generations,
/// staging and temp dirs mid-publish. `spell` renders a path in the permission-rule syntax
/// (`execute_wrapped::rule_path`).
///
/// FAILS CLOSED — `Err` names the reason and the caller keeps the blanket rule — when: the
/// registry does not parse; the state home, its skills root or the read slot cannot be listed; a
/// top-level entry, or a child of the skills root other than the read slot, is not classified by
/// the registry; an entry of the read slot is neither a GENERATION DIRECTORY (a real directory —
/// not a link, not a file — named by decimal digits, as crew publishes them: `000007`) nor a
/// recognised STAGING/TEMP name (the registry's `snapshots/<pattern>` denied children: `.staging-*`
/// and `.tmp-*`); or a rule path cannot be spelled. The read-slot listing is the ONE runtime
/// listing that BUILDS rules, and it only ever ADDS denies — an entry it cannot classify refuses
/// the launch rather than being left readable. Residual (accepted, documented): a generation
/// published WHILE a session runs is readable by that session until its next launch —
/// generations are immutable and contain only the enabled skills of a newer publish.
pub(crate) fn read_rules_around_snapshot(
    state_home: &Path,
    handed_gen: &str,
    spell: &dyn Fn(&Path) -> Option<String>,
) -> Result<Vec<String>, String> {
    let registry = registry().map_err(|e| format!("the state-home registry is unusable ({e})"))?;
    let skills = registry
        .skills_entry()
        .ok_or("the state-home registry names no skills root (no entry with a `read_slot`)")?;
    let Claim::Name(skills_name) = &skills.claim else {
        return Err("the skills root must be claimed by exact `name`".to_string());
    };
    let slot = skills
        .read_slot
        .as_deref()
        .ok_or("the skills root has no `read_slot`")?;

    // (a) Every top-level entry must be classified — fail closed on anything else — AND must be
    // on disk what the registry declares it to be (codex round 6): the rule emitted below follows
    // the DECLARED kind, so a directory named like a registered file (or a symlink of any name)
    // would pass admission and receive a rule that does not cover it.
    for name in list_names(state_home)? {
        let Some(entry) = registry.classify(&name) else {
            return Err(unclassified(state_home, &name));
        };
        check_entry_kind(state_home, &name, entry)?;
    }
    // (b) Every child of the skills root must be a denied child or the read slot itself.
    let skills_dir = state_home.join(skills_name);
    if std::fs::symlink_metadata(&skills_dir).is_ok() {
        let known: Vec<&str> = skills
            .denied_children
            .iter()
            .map(String::as_str)
            .filter(|c| !c.contains('/'))
            .chain(std::iter::once(slot))
            .collect();
        for name in list_names(&skills_dir)? {
            if !known.iter().any(|k| *k == name) {
                return Err(unclassified(&skills_dir, &name));
            }
        }
    }
    // (c) v3.3 §1: the read slot itself — the ONE listing that builds rules. Every entry but the
    // handed generation is denied by name; an entry that is neither a generation directory nor a
    // recognised staging/temp name refuses the launch, and so does an unlistable slot.
    let slot_dir = skills_dir.join(slot);
    let patterns = slot_patterns(skills, slot);
    let mut siblings: Vec<PathBuf> = Vec::new();
    for name in list_names(&slot_dir)? {
        if name == handed_gen {
            continue;
        }
        let path = slot_dir.join(&name);
        if patterns.iter().any(|p| p.matches(&name)) {
            // (codex round 7) a recognised staging/temp NAME must be a real directory too, judged
            // by lstat — a symlink or a file of that name is refused by name rather than denied
            // as if it were the staging directory crew writes.
            match std::fs::symlink_metadata(&path) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(format!(
                        "`{name}` under {} is a symlink where a staging/temp directory is \
                         expected; the worker Read fence cannot classify it — the launch is \
                         refused rather than leaving it readable; remove it",
                        slot_dir.display()
                    ))
                }
                Ok(m) if m.is_dir() => {
                    siblings.push(path);
                    continue;
                }
                Ok(_) => {
                    return Err(format!(
                        "`{name}` under {} is not a directory where a staging/temp directory is \
                         expected; the worker Read fence cannot classify it — the launch is \
                         refused rather than leaving it readable; remove it",
                        slot_dir.display()
                    ))
                }
                Err(e) => {
                    return Err(format!(
                        "cannot inspect `{name}` under {} to check the Read fence ({e})",
                        slot_dir.display()
                    ))
                }
            }
        }
        if is_generation_name(&name) {
            match std::fs::symlink_metadata(&path) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(format!(
                        "`{name}` under {} is a symlink where a generation directory is expected; \
                         the worker Read fence cannot classify it — the launch is refused rather \
                         than leaving it readable; remove it (a generation is a real, immutable \
                         directory)",
                        slot_dir.display()
                    ))
                }
                Ok(m) if m.is_dir() => {
                    siblings.push(path);
                    continue;
                }
                Ok(_) => {
                    return Err(format!(
                        "`{name}` under {} is not a directory where a generation directory is \
                         expected; the worker Read fence cannot classify it — the launch is \
                         refused rather than leaving it readable; remove it",
                        slot_dir.display()
                    ))
                }
                Err(e) => {
                    return Err(format!(
                        "cannot inspect `{name}` under {} to check the Read fence ({e})",
                        slot_dir.display()
                    ))
                }
            }
        }
        return Err(format!(
            "`{name}` under {} is neither a generation directory (decimal digits) nor a \
             staging/temp entry ({}); the worker Read fence cannot classify it — the launch is \
             refused rather than leaving it readable; remove it, or publish it as a generation",
            slot_dir.display(),
            patterns
                .iter()
                .map(SlotPattern::spelled)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    // The static rules. Deterministic order: registry order, then the skills children in order,
    // then (v3.3) the read slot's siblings in listing (sorted) order.
    let mut rules = Vec::new();
    let mut push = |path: PathBuf, tree: bool| -> Result<(), String> {
        let p = spell(&path).ok_or_else(|| {
            format!(
                "the fence cannot express a deny rule for {} (non-UTF8, or it contains a \
                 backslash or a comma)",
                path.display()
            )
        })?;
        rules.push(format!("Read({p})"));
        if tree {
            rules.push(format!("Read({p}/**)"));
        }
        Ok(())
    };
    for entry in &registry.entries {
        if std::ptr::eq(entry, skills) {
            for child in &skills.denied_children {
                push(join_slashed(&skills_dir, child), true)?;
            }
            continue;
        }
        let tree = entry.kind != "file";
        push(state_home.join(entry.rule_name()), tree)?;
    }
    for sibling in siblings {
        push(sibling, true)?;
    }
    Ok(rules)
}

/// The ACTUAL kind of a classified top-level entry must be the registry's DECLARED kind (codex
/// round 6), judged by `lstat` — never a following stat: `file` ⇒ a regular file; `dir` ⇒ a
/// directory; `file-with-sidecars` ⇒ a regular file or a directory (`core.db` is a file, its
/// `core.db.events/` sidecar a directory — both covered by the `/**` rule such an entry gets); a
/// SYMLINK of any classified name is refused whatever it points at — the rule would name the
/// link, and the worker would read through it. Fail closed, naming the entry.
fn check_entry_kind(state_home: &Path, name: &str, entry: &Entry) -> Result<(), String> {
    let path = state_home.join(name);
    let meta = std::fs::symlink_metadata(&path).map_err(|e| {
        format!(
            "cannot inspect `{name}` under {} to check the Read fence ({e})",
            state_home.display()
        )
    })?;
    let ft = meta.file_type();
    let refuse = |actual: &str| {
        format!(
            "`{name}` under {} is {actual} where the state-home registry declares a `{}` \
             (tests/fixtures/state-home-subtrees.json); the worker Read fence would emit a rule \
             for the declared kind and leave the actual entry uncovered, so the launch is refused \
             rather than fenced wrongly — remove the entry, or fix what wrote it",
            state_home.display(),
            entry.kind
        )
    };
    if ft.is_symlink() {
        return Err(refuse("a symlink"));
    }
    let ok = match entry.kind.as_str() {
        "file" => ft.is_file(),
        "dir" => ft.is_dir(),
        // `file-with-sidecars`: the prefix covers a file and its sidecars, one of which is a
        // directory (`core.db.events/`).
        _ => ft.is_file() || ft.is_dir(),
    };
    if ok {
        Ok(())
    } else if ft.is_dir() {
        Err(refuse("a directory"))
    } else if ft.is_file() {
        Err(refuse("a regular file"))
    } else {
        Err(refuse("neither a regular file nor a directory"))
    }
}

/// A generation directory's name as crew publishes it — decimal digits (`000007`, zero-padded so a
/// lexical listing is the generation order; core accepts any non-empty run of ASCII digits). The
/// read-slot listing denies such an entry as a sibling generation and refuses anything else it
/// cannot recognise ([`read_rules_around_snapshot`]); the snapshot loader requires the handed
/// generation's directory to be one (`skills_snapshot::load_published`).
pub(crate) fn is_generation_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit())
}

/// One `snapshots/<pattern>` denied child of the skills root, as the read-slot listing recognises
/// a staging/temp entry: a trailing `*` matches any suffix (`.staging-*`, `.tmp-*`), else the exact
/// name.
struct SlotPattern {
    stem: String,
    glob: bool,
}

impl SlotPattern {
    fn matches(&self, name: &str) -> bool {
        if self.glob {
            name.starts_with(self.stem.as_str())
        } else {
            name == self.stem
        }
    }

    fn spelled(&self) -> String {
        if self.glob {
            format!("{}*", self.stem)
        } else {
            self.stem.clone()
        }
    }
}

/// The read slot's staging/temp patterns from the skills entry's `denied_children`: the entries
/// spelled `<slot>/<pattern>` with nothing deeper.
fn slot_patterns(skills: &Entry, slot: &str) -> Vec<SlotPattern> {
    skills
        .denied_children
        .iter()
        .filter_map(|c| c.strip_prefix(slot)?.strip_prefix('/'))
        .filter(|rest| !rest.is_empty() && !rest.contains('/'))
        .map(|rest| match rest.strip_suffix('*') {
            Some(stem) => SlotPattern {
                stem: stem.to_string(),
                glob: true,
            },
            None => SlotPattern {
                stem: rest.to_string(),
                glob: false,
            },
        })
        .collect()
}

/// `base` joined with a `/`-separated relative spelling, component by component (a `/` inside a
/// single `join` argument yields a mixed-separator path on Windows).
fn join_slashed(base: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(base.to_path_buf(), |p, c| p.join(c))
}

/// The top-level names in `dir`, sorted (so the first unclassified entry named is the same on
/// every platform). A dir that cannot be listed, or a name that is not UTF-8, is an `Err`.
fn list_names(dir: &Path) -> Result<Vec<String>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        format!(
            "cannot list {} to check the Read fence ({e})",
            dir.display()
        )
    })?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            format!(
                "cannot list {} to check the Read fence ({e})",
                dir.display()
            )
        })?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            format!(
                "{} holds an entry whose name is not UTF-8 ({}); the Read fence cannot classify it",
                dir.display(),
                name.to_string_lossy()
            )
        })?;
        names.push(name.to_string());
    }
    names.sort();
    Ok(names)
}

fn unclassified(dir: &Path, name: &str) -> String {
    format!(
        "`{name}` under {} is not in the state-home registry (tests/fixtures/state-home-subtrees.json), \
         so the worker Read fence cannot classify it; the launch is refused rather than leaving \
         it unfenced — remove the entry, or register it in core AND crew (a new crew store must \
         be registered in both before it may appear there)",
        dir.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spell(p: &Path) -> Option<String> {
        p.to_str().map(str::to_string)
    }

    /// The embedded registry is the fixture crew mirrors: it parses, carries exactly one read
    /// slot (the skills root, `snapshots`), and classifies every top-level entry the live
    /// daemon's state home holds today plus every store crew's own audit names.
    #[test]
    fn the_embedded_registry_is_well_formed_and_classifies_every_known_store() {
        let r = registry().expect("the embedded fixture parses");
        assert_eq!(r.version, 1);
        let skills = r.skills_entry().expect("a skills root");
        assert_eq!(skills.claim, Claim::Name("skills".into()));
        assert_eq!(skills.read_slot.as_deref(), Some("snapshots"));
        assert_eq!(r.slot_shape(), Some(("skills", "snapshots")));
        for child in [
            "baseline",
            "effective",
            "manifest.json",
            "current",
            ".uv-cache",
        ] {
            assert!(
                skills.denied_children.iter().any(|c| c == child),
                "{child} must be a denied child of the skills root"
            );
        }
        for live in [
            "audit.log",
            "core.db",
            "core.db-shm",
            "core.db-wal",
            "core.db.events",
            "core.db.knowledge",
            "core.db.knowledge-wal",
            "core.db.mem",
            "core.db.mem.embedder",
            "core.db.mem.memext",
            "bus.db",
            "bus.db-wal",
            "daemon-local.log",
            "daemon-stdout.log",
            "evals",
            "interactive-demos",
            "interactive-drafts",
            "interactive-chats",
            "interactive-edits",
            "interactive-chat-ledger.json",
            "project-graphs",
            "project-settings.json",
            "skills",
        ] {
            assert!(r.classify(live).is_some(), "{live} must be classified");
        }
        assert!(r.classify("scratch.txt").is_none());
        assert!(r.classify(".DS_Store").is_none());
        // v3.3: what the read-slot listing recognises — generation names are decimal digits; the
        // staging/temp patterns are the registry's `snapshots/<pattern>` denied children.
        for gen in ["000007", "1", "42"] {
            assert!(is_generation_name(gen), "{gen}");
        }
        for not in ["", ".staging-x", "7a", "gen-7", "-1"] {
            assert!(!is_generation_name(not), "{not}");
        }
        let patterns = slot_patterns(skills, "snapshots");
        assert_eq!(
            patterns
                .iter()
                .map(SlotPattern::spelled)
                .collect::<Vec<_>>(),
            vec![".staging-*".to_string(), ".tmp-*".to_string()]
        );
        assert!(patterns.iter().any(|p| p.matches(".staging-ab12")));
        assert!(patterns.iter().any(|p| p.matches(".tmp-x")));
        assert!(!patterns.iter().any(|p| p.matches("000001")));
        assert!(!patterns.iter().any(|p| p.matches("staging-x")));
        // Parse strictness.
        assert!(parse_registry("{\"version\":1,\"entries\":[{\"kind\":\"dir\"}]}").is_err());
        // (review pass 7) a non-string denied child, or a non-array `denied_children`, is a parse
        // error — never a silently narrower fence.
        let err = parse_registry(
            "{\"version\":1,\"entries\":[{\"name\":\"a\",\"kind\":\"dir\",\"read_slot\":\"s\",\
             \"denied_children\":[\"x\",7]}]}",
        )
        .expect_err("a non-string denied child");
        assert!(err.contains("denied_children[1] is not a string"), "{err}");
        let err = parse_registry(
            "{\"version\":1,\"entries\":[{\"name\":\"a\",\"kind\":\"dir\",\"read_slot\":\"s\",\
             \"denied_children\":\"x\"}]}",
        )
        .expect_err("a non-array denied_children");
        assert!(err.contains("`denied_children` is not an array"), "{err}");
        assert!(parse_registry(
            "{\"version\":1,\"entries\":[{\"name\":\"a\",\"kind\":\"dir\",\"denied_children\":[\"x\"]}]}"
        )
        .is_err());
        assert!(parse_registry(
            "{\"version\":1,\"entries\":[{\"name\":\"a\",\"kind\":\"dir\",\"read_slot\":\"s\"},\
             {\"name\":\"b\",\"kind\":\"dir\",\"read_slot\":\"t\"}]}"
        )
        .is_err());
    }

    /// The state home is DERIVED from the snapshot's shape — `<state home>/skills/snapshots/<gen>`
    /// — whatever the state home is called and wherever it is: the default `~/.wicked-crew`, a
    /// scratch daemon's `/private/tmp/crew-state`, anything. Not the snapshots dir, not a file
    /// inside a generation, not a staging dir, not another store, not a bare filesystem root.
    #[test]
    fn the_state_home_is_derived_from_the_snapshot_shape_wherever_it_lives() {
        let of = |p: &str| of_snapshot(Path::new(p));
        assert_eq!(
            of("/h/.wicked-crew/skills/snapshots/000007"),
            Some(PathBuf::from("/h/.wicked-crew"))
        );
        assert_eq!(
            of("/private/tmp/crew-state/skills/snapshots/1"),
            Some(PathBuf::from("/private/tmp/crew-state")),
            "a custom state home is derived exactly like the default one"
        );
        assert_eq!(of("/h/.wicked-crew/skills/snapshots"), None);
        assert_eq!(of("/h/.wicked-crew/skills/snapshots/000007/skills"), None);
        assert_eq!(of("/h/.wicked-crew/skills/snapshots/.staging-ab"), None);
        assert_eq!(of("/h/.wicked-crew/skills/effective"), None);
        assert_eq!(of("/h/.wicked-crew/evals/7"), None);
        assert_eq!(of("/h/elsewhere/snapshots/7"), None, "no skills component");
        assert_eq!(
            of("/skills/snapshots/7"),
            None,
            "the filesystem root is no state home"
        );
        assert_eq!(
            of("/h/.claude/plugins/cache/wicked-garden/wicked-garden/12.32.0"),
            None,
            "the live cache has no state home"
        );
        // Derivation is by shape ALONE (v3.4 §2): the shaped root derives its state home, and a
        // shapeless root fails naming the path and the shape — including a parent that is not
        // literally `snapshots` and a grandparent that is not literally `skills`.
        let root = Path::new("/h/.wicked-crew/skills/snapshots/000007");
        assert_eq!(derive(root), Ok(PathBuf::from("/h/.wicked-crew")));
        for shapeless in [
            "/h/elsewhere/7",
            "/h/.wicked-crew/skills/generations/000007",
            "/h/.wicked-crew/plugins/snapshots/000007",
        ] {
            let err = derive(Path::new(shapeless)).expect_err("shapeless");
            assert!(
                err.contains(shapeless)
                    && err.contains("skills/snapshots/<gen>")
                    && err.contains("parent must be `snapshots`")
                    && err.contains("grandparent `skills`"),
                "{shapeless}: {err}"
            );
        }
    }

    /// Fail closed: an unclassified top-level entry (or an unclassified child of the skills
    /// root) refuses by name; a classified tree yields the static rules — one per entry, the
    /// skills root per denied child, nothing enumerated at those levels, nothing for the handed
    /// generation — plus (v3.3 §1) one deny per SIBLING entry of the read slot: the older
    /// generation and the staging dir each get their own rule pair, an entry that is neither a
    /// generation nor a staging name refuses by name, a FILE named like a generation refuses,
    /// and an unlistable slot refuses. Exercised on a state home that is NOT called
    /// `.wicked-crew`: the fence follows the directory, not its name.
    #[test]
    fn rules_are_static_and_an_unclassified_entry_refuses_by_name() {
        let base = std::env::temp_dir().join(format!(
            "wstate-home-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("crew-state");
        let skills = home.join("skills");
        std::fs::create_dir_all(skills.join("snapshots").join("000007")).unwrap();
        std::fs::create_dir_all(skills.join("snapshots").join("000006")).unwrap();
        std::fs::create_dir_all(skills.join("snapshots").join(".staging-x")).unwrap();
        std::fs::create_dir_all(skills.join("effective")).unwrap();
        std::fs::create_dir_all(skills.join("baseline")).unwrap();
        std::fs::write(skills.join("manifest.json"), "{}").unwrap();
        std::fs::create_dir_all(home.join("core.db.events")).unwrap();
        std::fs::write(home.join("core.db"), "db").unwrap();
        std::fs::write(home.join("core.db-wal"), "").unwrap();
        std::fs::write(home.join("daemon-stdout.log"), "").unwrap();
        std::fs::create_dir_all(home.join("evals")).unwrap();

        assert_eq!(
            of_snapshot(&skills.join("snapshots").join("000007")),
            Some(home.clone())
        );
        let rules = read_rules_around_snapshot(&home, "000007", &spell).expect("classified tree");
        let s = |p: &Path| p.to_str().unwrap().to_string();
        assert!(rules.contains(&format!("Read({}/**)", s(&skills.join("effective")))));
        assert!(rules.contains(&format!("Read({})", s(&skills.join("manifest.json")))));
        assert!(rules.contains(&format!("Read({}/**)", s(&skills.join("current")))));
        assert!(rules.contains(&format!(
            "Read({}/**)",
            s(&skills.join("snapshots").join(".staging-*"))
        )));
        // v3.3 §1: the read slot's SIBLINGS are denied by name — the older generation and the
        // staging dir each get their own rule pair (the staging dir on top of the static pattern).
        let gen6 = skills.join("snapshots").join("000006");
        assert!(rules.contains(&format!("Read({})", s(&gen6))), "{rules:?}");
        assert!(
            rules.contains(&format!("Read({}/**)", s(&gen6))),
            "{rules:?}"
        );
        assert!(
            rules.contains(&format!(
                "Read({}/**)",
                s(&skills.join("snapshots").join(".staging-x"))
            )),
            "{rules:?}"
        );
        assert!(rules.contains(&format!("Read({}*)", s(&home.join("core.db")))));
        assert!(rules.contains(&format!("Read({}*/**)", s(&home.join("core.db")))));
        assert!(rules.contains(&format!("Read({}*)", s(&home.join("daemon-")))));
        assert!(rules.contains(&format!("Read({}/**)", s(&home.join("evals")))));
        // Static: a registered entry that is ABSENT on disk is still denied (nothing enumerated).
        assert!(rules.contains(&format!("Read({}/**)", s(&home.join("project-graphs")))));
        // Nothing names the read slot or the generation.
        assert!(
            !rules.iter().any(|r| r.contains("snapshots/000007")
                || r == &format!("Read({}/**)", s(&skills.join("snapshots")))
                || r == &format!("Read({}/**)", s(&skills))
                || r == &format!("Read({}/**)", s(&home))),
            "{rules:?}"
        );
        let n = rules.len();
        // Deterministic and enumeration-free at the top level: adding a CLASSIFIED sidecar
        // changes nothing …
        std::fs::write(home.join("core.db.mem"), "").unwrap();
        assert_eq!(
            read_rules_around_snapshot(&home, "000007", &spell)
                .unwrap()
                .len(),
            n
        );
        // … while a new sibling generation in the read slot adds exactly its own pair (v3.3), and
        // handing THAT generation instead denies 000007 and not 000005.
        std::fs::create_dir_all(skills.join("snapshots").join("000005")).unwrap();
        let more = read_rules_around_snapshot(&home, "000007", &spell).unwrap();
        assert_eq!(more.len(), n + 2, "{more:?}");
        let other = read_rules_around_snapshot(&home, "000005", &spell).unwrap();
        assert!(
            other.contains(&format!(
                "Read({}/**)",
                s(&skills.join("snapshots").join("000007"))
            )) && !other
                .iter()
                .any(|r| r.contains(&s(&skills.join("snapshots").join("000005")))),
            "{other:?}"
        );
        std::fs::remove_dir_all(skills.join("snapshots").join("000005")).unwrap();

        // An unclassified top-level entry refuses, naming it.
        std::fs::write(home.join("stray.txt"), "").unwrap();
        let err = read_rules_around_snapshot(&home, "000007", &spell).expect_err("unclassified");
        assert!(
            err.contains("`stray.txt`") && err.contains("refused"),
            "{err}"
        );
        std::fs::remove_file(home.join("stray.txt")).unwrap();
        // (codex round 6) A CLASSIFIED entry whose kind on disk is not the declared one refuses
        // by name too: a DIRECTORY named `audit.log` (declared `file`) or `daemon-x` (declared
        // `file`, the `daemon-` prefix), a FILE named `evals` (declared `dir`). The rule emitted
        // for a `file` has no `/**`, so a directory of that name would pass admission unfenced.
        // A regular file named `daemon-x` is exactly what the registry declares and is admitted.
        let kind_err = |what: &str| {
            let err = read_rules_around_snapshot(&home, "000007", &spell)
                .expect_err("a kind mismatch refuses");
            assert!(
                err.contains(&format!("`{what}`"))
                    && err.contains("state-home registry declares")
                    && err.contains("refused"),
                "{what}: {err}"
            );
        };
        std::fs::create_dir_all(home.join("audit.log")).unwrap();
        kind_err("audit.log");
        std::fs::remove_dir_all(home.join("audit.log")).unwrap();
        std::fs::create_dir_all(home.join("daemon-x")).unwrap();
        kind_err("daemon-x");
        std::fs::remove_dir_all(home.join("daemon-x")).unwrap();
        std::fs::write(home.join("daemon-x"), "log").unwrap();
        assert_eq!(
            read_rules_around_snapshot(&home, "000007", &spell).unwrap(),
            rules,
            "a regular file named daemon-x is what the registry declares"
        );
        std::fs::remove_file(home.join("daemon-x")).unwrap();
        std::fs::remove_dir_all(home.join("evals")).unwrap();
        std::fs::write(home.join("evals"), "not a dir").unwrap();
        kind_err("evals");
        std::fs::remove_file(home.join("evals")).unwrap();
        std::fs::create_dir_all(home.join("evals")).unwrap();
        // A SYMLINK of a classified name — `skills` (declared `dir`) aimed at a real directory,
        // `audit.log` (declared `file`) aimed at a real file — is refused whatever it points at:
        // the worker would read through the link.
        #[cfg(unix)]
        {
            let real_skills = base.join("real-skills");
            std::fs::rename(&skills, &real_skills).unwrap();
            std::os::unix::fs::symlink(&real_skills, &skills).unwrap();
            let err = read_rules_around_snapshot(&home, "000007", &spell)
                .expect_err("a linked skills root");
            assert!(
                err.contains("`skills`") && err.contains("a symlink"),
                "{err}"
            );
            std::fs::remove_file(&skills).unwrap();
            std::fs::rename(&real_skills, &skills).unwrap();
            let real_log = base.join("real-audit.log");
            std::fs::write(&real_log, "").unwrap();
            std::os::unix::fs::symlink(&real_log, home.join("audit.log")).unwrap();
            let err = read_rules_around_snapshot(&home, "000007", &spell)
                .expect_err("a linked audit.log");
            assert!(
                err.contains("`audit.log`") && err.contains("a symlink"),
                "{err}"
            );
            std::fs::remove_file(home.join("audit.log")).unwrap();
        }
        assert_eq!(
            read_rules_around_snapshot(&home, "000007", &spell).unwrap(),
            rules,
            "the classified tree yields the same rules once the mismatches are gone"
        );
        // …and so does an unclassified child of the skills root.
        std::fs::create_dir_all(skills.join("scratch")).unwrap();
        let err =
            read_rules_around_snapshot(&home, "000007", &spell).expect_err("unclassified child");
        assert!(err.contains("`scratch`"), "{err}");
        std::fs::remove_dir_all(skills.join("scratch")).unwrap();
        // …and (v3.3) an entry of the read slot that is neither a generation nor a staging name,
        // and a FILE where a generation directory is expected.
        std::fs::write(skills.join("snapshots").join("junk.txt"), "").unwrap();
        let err = read_rules_around_snapshot(&home, "000007", &spell)
            .expect_err("unclassified read-slot entry");
        assert!(
            err.contains("`junk.txt`")
                && err.contains("neither a generation")
                && err.contains(".staging-*"),
            "{err}"
        );
        std::fs::remove_file(skills.join("snapshots").join("junk.txt")).unwrap();
        std::fs::write(skills.join("snapshots").join("000004"), "").unwrap();
        let err = read_rules_around_snapshot(&home, "000007", &spell)
            .expect_err("a file where a generation is expected");
        assert!(
            err.contains("`000004`") && err.contains("not a directory"),
            "{err}"
        );
        std::fs::remove_file(skills.join("snapshots").join("000004")).unwrap();
        // (codex round 7) a recognised staging/temp NAME must be a real directory too: a FILE
        // named `.staging-file` and (unix) a SYMLINK named `.tmp-link` are refused by name.
        std::fs::write(skills.join("snapshots").join(".staging-file"), "").unwrap();
        let err = read_rules_around_snapshot(&home, "000007", &spell)
            .expect_err("a file where a staging directory is expected");
        assert!(
            err.contains("`.staging-file`") && err.contains("not a directory"),
            "{err}"
        );
        std::fs::remove_file(skills.join("snapshots").join(".staging-file")).unwrap();
        #[cfg(unix)]
        {
            let target = base.join("real-tmp");
            std::fs::create_dir_all(&target).unwrap();
            std::os::unix::fs::symlink(&target, skills.join("snapshots").join(".tmp-link"))
                .unwrap();
            let err = read_rules_around_snapshot(&home, "000007", &spell)
                .expect_err("a symlink where a staging directory is expected");
            assert!(
                err.contains("`.tmp-link`") && err.contains("a symlink"),
                "{err}"
            );
            std::fs::remove_file(skills.join("snapshots").join(".tmp-link")).unwrap();
        }
        assert_eq!(
            read_rules_around_snapshot(&home, "000007", &spell).unwrap(),
            rules
        );
        // A state home that cannot be listed refuses too — and so does an unlistable read slot.
        assert!(read_rules_around_snapshot(&base.join("absent"), "1", &spell).is_err());
        let slotless = base.join("slotless");
        std::fs::create_dir_all(slotless.join("skills")).unwrap();
        let err =
            read_rules_around_snapshot(&slotless, "1", &spell).expect_err("no read slot to list");
        assert!(err.contains("cannot list"), "{err}");
        // `same_dir`: identical spellings agree; a real path and its own spelling agree; two
        // distinct directories do not.
        assert!(same_dir(&home, &home));
        assert!(same_dir(&home, &std::fs::canonicalize(&home).unwrap()));
        assert!(!same_dir(&home, &skills));
        let _ = std::fs::remove_dir_all(&base);
    }
}
