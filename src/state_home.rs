//! The worker Read fence over crew's state home (`~/.wicked-crew`) as an EXPLICIT, tested
//! denylist (skills keystone design v3.1 §1; core#396 review pass 2).
//!
//! # Why a registry and not a blanket
//!
//! Every worker is fenced off from the daemon's state home — the operational store, the event
//! log, the audit log, the project graphs: `Read(~/.wicked-crew/**)` for the file tools. The
//! published skills snapshot a worker is handed (`WICKED_SKILLS_SNAPSHOT`) lives under that SAME
//! root — `<state home>/skills/snapshots/<gen>/` — because the skills root is one of crew's stores
//! and the operator keeps ONE storage root. Claude's deny rules win over any allow, so no allow
//! rule can open the snapshot under a blanket deny: the deny itself must not cover it.
//!
//! Pass 1 solved that by ENUMERATING the state home's siblings at launch and denying each one —
//! which fails open: an entry created after the listing is unfenced, a listing error leaves a
//! level unfenced, and an unspellable name is skipped. So the rule list is now STATIC — this
//! module's registry, embedded from [`REGISTRY_JSON`] (the same file crew mirrors and audits
//! against its own stores) — and the launch-time listing only ever REFUSES: an entry the registry
//! does not classify fails the launch by name ([`read_rules_around_snapshot`]). Fail closed,
//! never widen; no runtime enumeration builds a rule.
//!
//! The only non-denied path under the state home is the resolved `skills/snapshots/<gen>/`.
//! Residuals, stated: a top-level entry created WHILE a session runs is fenced at the next launch
//! (crew is the only writer of that directory, and the listing refuses it then); sibling
//! immutable generations under `skills/snapshots/` stay readable until crew reaps them — a static
//! rule cannot deny "every sibling but this one" without the enumeration this module exists to
//! remove, and a published generation holds nothing but plugin files.

use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;

/// The default state home's directory name under `$HOME` — the fenced entry this registry
/// classifies the contents of (`execute_wrapped::DENIED_HOME_SUBDIRS` lists it).
pub(crate) const STATE_HOME_DIRNAME: &str = ".wicked-crew";

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
        let denied_children: Vec<String> = e
            .get("denied_children")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
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

/// Is `dir` the crew state home — the one fenced directory whose contents this registry
/// classifies?
pub(crate) fn is_state_home(dir: &Path) -> bool {
    dir.file_name().is_some_and(|n| n == STATE_HOME_DIRNAME)
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

/// The generation directory name when `root` (a snapshot's canonical real path) sits EXACTLY in
/// the read slot of `state_home` — `<state home>/skills/snapshots/<gen>`, no deeper and no
/// shallower — else `None`. A snapshot anywhere else under the state home is not carveable and
/// the caller refuses it.
pub(crate) fn snapshot_slot(state_home: &Path, root: &Path) -> Option<String> {
    let registry = registry().ok()?;
    let skills = registry.skills_entry()?;
    let Claim::Name(skills_name) = &skills.claim else {
        return None;
    };
    let slot = skills.read_slot.as_deref()?;
    let base = base_under(state_home, root)?;
    let rel = root.strip_prefix(&base).ok()?;
    let parts: Vec<&str> = rel
        .components()
        .map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    match parts.as_slice() {
        [a, b, gen] if *a == skills_name && *b == slot && !gen.starts_with('.') => {
            Some((*gen).to_string())
        }
        _ => None,
    }
}

/// The Read rules for `state_home` with the snapshot in its read slot as the ONLY non-denied
/// path: one rule per registered top-level entry (plus `/**` for anything that can have
/// children), and for the skills root one rule per denied child (`baseline/**`, `effective/**`,
/// `manifest.json`, `current`, `.uv-cache/**`, `snapshots/.staging-*/**`, …). `spell` renders a
/// path in the permission-rule syntax (`execute_wrapped::rule_path`).
///
/// FAILS CLOSED — `Err` names the reason and the caller keeps the blanket rule — when: the
/// registry does not parse; the state home (or its skills root) cannot be listed; a top-level
/// entry, or a child of the skills root other than the read slot, is not classified by the
/// registry; or a rule path cannot be spelled. The listing NEVER adds a rule: it only decides
/// whether the static list is allowed to stand in for the blanket. Nothing under
/// `skills/snapshots/` is listed — generations are the read slot and the staging patterns are
/// static rules.
pub(crate) fn read_rules_around_snapshot(
    state_home: &Path,
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

    // (a) Every top-level entry must be classified — fail closed on anything else.
    for name in list_names(state_home)? {
        if registry.classify(&name).is_none() {
            return Err(unclassified(state_home, &name));
        }
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

    // The static rules. Deterministic order: registry order, then the skills children in order.
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
    Ok(rules)
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
        // Parse strictness.
        assert!(parse_registry("{\"version\":1,\"entries\":[{\"kind\":\"dir\"}]}").is_err());
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

    /// The read slot is EXACTLY `<state home>/skills/snapshots/<gen>` — not the snapshots dir,
    /// not a file inside a generation, not a staging dir, not another store.
    #[test]
    fn the_read_slot_is_exactly_one_generation_deep() {
        let home = Path::new("/h/.wicked-crew");
        let slot = |p: &str| snapshot_slot(home, Path::new(p));
        assert_eq!(
            slot("/h/.wicked-crew/skills/snapshots/000007"),
            Some("000007".into())
        );
        assert_eq!(slot("/h/.wicked-crew/skills/snapshots"), None);
        assert_eq!(slot("/h/.wicked-crew/skills/snapshots/000007/skills"), None);
        assert_eq!(slot("/h/.wicked-crew/skills/snapshots/.staging-ab"), None);
        assert_eq!(slot("/h/.wicked-crew/skills/effective"), None);
        assert_eq!(slot("/h/.wicked-crew/evals/7"), None);
        assert_eq!(slot("/h/elsewhere/skills/snapshots/7"), None);
        assert!(is_state_home(home) && !is_state_home(Path::new("/h/.claude")));
    }

    /// Fail closed: an unclassified top-level entry (or an unclassified child of the skills
    /// root) refuses by name; a classified tree yields the static rules — one per entry, the
    /// skills root per denied child, nothing enumerated, nothing for the read slot.
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
        let home = base.join(STATE_HOME_DIRNAME);
        let skills = home.join("skills");
        std::fs::create_dir_all(skills.join("snapshots").join("000007")).unwrap();
        std::fs::create_dir_all(skills.join("snapshots").join(".staging-x")).unwrap();
        std::fs::create_dir_all(skills.join("effective")).unwrap();
        std::fs::create_dir_all(skills.join("baseline")).unwrap();
        std::fs::write(skills.join("manifest.json"), "{}").unwrap();
        std::fs::create_dir_all(home.join("core.db.events")).unwrap();
        std::fs::write(home.join("core.db"), "db").unwrap();
        std::fs::write(home.join("core.db-wal"), "").unwrap();
        std::fs::write(home.join("daemon-stdout.log"), "").unwrap();
        std::fs::create_dir_all(home.join("evals")).unwrap();

        let rules = read_rules_around_snapshot(&home, &spell).expect("classified tree");
        let s = |p: &Path| p.to_str().unwrap().to_string();
        assert!(rules.contains(&format!("Read({}/**)", s(&skills.join("effective")))));
        assert!(rules.contains(&format!("Read({})", s(&skills.join("manifest.json")))));
        assert!(rules.contains(&format!("Read({}/**)", s(&skills.join("current")))));
        assert!(rules.contains(&format!(
            "Read({}/**)",
            s(&skills.join("snapshots").join(".staging-*"))
        )));
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
        // Deterministic and enumeration-free: adding a CLASSIFIED sidecar changes nothing.
        std::fs::write(home.join("core.db.mem"), "").unwrap();
        assert_eq!(read_rules_around_snapshot(&home, &spell).unwrap().len(), n);

        // An unclassified top-level entry refuses, naming it.
        std::fs::write(home.join("stray.txt"), "").unwrap();
        let err = read_rules_around_snapshot(&home, &spell).expect_err("unclassified");
        assert!(
            err.contains("`stray.txt`") && err.contains("refused"),
            "{err}"
        );
        std::fs::remove_file(home.join("stray.txt")).unwrap();
        // …and so does an unclassified child of the skills root.
        std::fs::create_dir_all(skills.join("scratch")).unwrap();
        let err = read_rules_around_snapshot(&home, &spell).expect_err("unclassified child");
        assert!(err.contains("`scratch`"), "{err}");
        std::fs::remove_dir_all(skills.join("scratch")).unwrap();
        // A state home that cannot be listed refuses too.
        assert!(read_rules_around_snapshot(&base.join("absent"), &spell).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
