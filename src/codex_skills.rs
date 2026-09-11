//! F-079 (core#441) — the codex skills lever, `SkillsLever::CodexSkillsDir`.
//!
//! codex 0.153 loads skills from `$CODEX_HOME/skills/<name>/SKILL.md` and has no per-launch flag
//! for them (`-c` / `--add-dir` / profiles load none), so until core#426 minted a per-seat
//! `CODEX_HOME` the engine had no wicked-owned lever for codex at all (`Absent`, core#400). Now it
//! POPULATES the seat's ENGINE-MINTED `CODEX_HOME` (`<worker home>/codex`, private, never the
//! operator's `~/.codex`) from the pinned snapshot at every launch:
//!
//! - one directory per deliverable portable skill, FLAT by its frontmatter `name` — the shape
//!   crew's `views/copilot/.github/skills/<name>/` takes, and the shape codex discovers;
//! - COPIES, never links (codex refuses symlinked skill directories; the snapshot is read-only by
//!   mode bits, so each copy has its read-only bit cleared and stays removable on every OS);
//! - a skill's OWN files only: an indexed skill nested below it is excluded from its copy and
//!   lands under its own name instead;
//! - a generation MARKER (`.wicked-skills-gen`) inside the tree listing the generation and every
//!   entry written, so a relaunch on an unchanged generation is a no-op and a NEW generation
//!   replaces exactly the entries the previous one wrote — stale generations never accumulate;
//! - nothing else under `CODEX_HOME` is read or written beyond this module's own lock file, and an
//!   entry the marker does not list is never replaced: it is carried into the new tree unchanged,
//!   and one that collides with a skill's name refuses the population by path.
//!
//! Populations of ONE seat home are SERIALIZED and ATOMIC (review of core#441): every unit of
//! every run on a daemon shares the one `<worker home>/codex`, and units run on parallel threads
//! across runs (`actor.rs`), so a second launch used to be able to strip a first launch's
//! in-flight copies and a seat could spawn against a half-rebuilt tree. Now a population takes an
//! exclusive OS file lock on `<CODEX_HOME>/.wicked-skills.lock` (blocking; `flock` / `LockFileEx`
//! through `std::fs::File::lock`), RE-CHECKS the marker under it (a waiter whose generation the
//! winner just built returns without writing), builds the whole new tree in a uniquely named
//! sibling directory (`skills.<pid>.<seq>.<nanos>.tmp`), and swaps it into place by rename — the
//! visible `skills/` is always a COMPLETE tree with a `done` marker, never a mixture of two
//! generations. A crash leaves the previous complete tree in place plus `skills.*.tmp` /
//! `skills.*.old` debris (or, between the two renames, no `skills/` at all), which the next
//! population sweeps and rebuilds under the same lock.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The subdirectory of `CODEX_HOME` codex scans for skills.
pub(crate) const SKILLS_SUBDIR: &str = "skills";
/// The generation marker inside `<CODEX_HOME>/skills`.
pub(crate) const GEN_MARKER: &str = ".wicked-skills-gen";
/// The lock file under `CODEX_HOME` (a sibling of `skills/`, never inside the swapped tree) every
/// population of that home holds exclusively for its whole duration.
pub(crate) const LOCK_FILE: &str = ".wicked-skills.lock";

/// One skill to populate: where it lands (`name`), where it comes from (`dir`, inside the
/// snapshot) and which directories below `dir` are indexed skills of their own (`excluded`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexSkill {
    pub name: String,
    pub dir: PathBuf,
    pub excluded: Vec<PathBuf>,
}

/// What one population did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Populated {
    pub skills_dir: PathBuf,
    pub names: Vec<String>,
    /// `false` when the marker already recorded this generation complete and every entry it
    /// lists is present — nothing was written (a relaunch, or a waiter behind the winner).
    pub changed: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct Marker {
    generation: String,
    names: Vec<String>,
}

fn io_err(what: &str, path: &Path, e: &std::io::Error) -> String {
    format!("{what} {}: {e}", path.display())
}

/// A process-unique suffix for the build and retired directories and the marker temp: pid (two
/// daemons on one host), a process-wide counter (two threads in one process) and a nanosecond
/// stamp (a counter reset across restarts).
fn unique_suffix() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}.{}.{nanos}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// The exclusive lock on one seat home, held for the whole population; released on drop.
struct HomeLock(std::fs::File);

fn lock_home(codex_home: &Path) -> Result<HomeLock, String> {
    let path = codex_home.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| io_err("could not open the skills lock", &path, &e))?;
    file.lock()
        .map_err(|e| io_err("could not take the skills lock", &path, &e))?;
    Ok(HomeLock(file))
}

impl Drop for HomeLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// The marker at `path`: `None` when absent or not a complete (`done`) record — an unparseable
/// or interrupted marker names nothing wicked owns, so nothing is dropped on its account and any
/// collision refuses.
fn read_marker(path: &Path) -> Result<Option<Marker>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err("could not read the generation marker", path, &e)),
    };
    let mut lines = text.lines();
    let Some(generation) = lines.next().and_then(|head| head.strip_prefix("done ")) else {
        return Ok(None);
    };
    Ok(Some(Marker {
        generation: generation.to_string(),
        names: lines
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect(),
    }))
}

/// Write the `done` marker atomically (unique temp + rename) into a tree being built.
fn write_marker(path: &Path, generation: &str, names: &[String]) -> Result<(), String> {
    let mut text = format!("done {generation}\n");
    for n in names {
        text.push_str(n);
        text.push('\n');
    }
    let tmp = path.with_file_name(format!("{GEN_MARKER}.{}.tmp", unique_suffix()));
    std::fs::write(&tmp, text).map_err(|e| io_err("could not write", &tmp, &e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io_err("could not commit the generation marker", path, &e)
    })
}

/// Clear the read-only bit on a copied file so a later generation can remove it on every OS
/// (Windows refuses to delete a read-only file; the snapshot's files are read-only by mode bits).
/// On unix only the OWNER's write bit is added (the file sits in the seat's private 0700 home);
/// Windows has the one read-only attribute, which is what the lint warns about on unix.
#[cfg_attr(not(unix), allow(clippy::permissions_set_readonly_false))]
fn make_writable(path: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| io_err("could not stat", path, &e))?;
    let mut perms = meta.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(perms.mode() | 0o200);
    }
    #[cfg(not(unix))]
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms).map_err(|e| io_err("could not make writable", path, &e))
}

/// Remove `path` whatever it is (a directory tree, a file, a dangling link); absent is fine.
fn remove_entry(path: &Path) -> Result<(), String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_err("could not stat", path, &e)),
    };
    if meta.is_dir() && !meta.file_type().is_symlink() {
        make_tree_writable(path)?;
        std::fs::remove_dir_all(path).map_err(|e| io_err("could not remove", path, &e))
    } else {
        std::fs::remove_file(path).map_err(|e| io_err("could not remove", path, &e))
    }
}

/// Belt and braces for `remove_entry` on Windows: every file under `dir` writable first.
fn make_tree_writable(dir: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| io_err("could not list", dir, &e))? {
        let entry = entry.map_err(|e| io_err("could not list", dir, &e))?;
        let path = entry.path();
        let meta =
            std::fs::symlink_metadata(&path).map_err(|e| io_err("could not stat", &path, &e))?;
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            make_tree_writable(&path)?;
        } else {
            make_writable(&path)?;
        }
    }
    Ok(())
}

/// Copy `src` into `dst` — regular files and directories only, sorted for determinism, every
/// `excluded` directory skipped whole. A symlink ANYWHERE in the source refuses the copy: the
/// snapshot is refused at load when it holds one, and a copy that followed a link could reach
/// outside the generation.
fn copy_tree(src: &Path, dst: &Path, excluded: &[PathBuf]) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(src).map_err(|e| io_err("could not stat", src, &e))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{} is a symlink; a skill is copied from the snapshot, never through a link",
            src.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", src.display()));
    }
    std::fs::create_dir_all(dst).map_err(|e| io_err("could not create", dst, &e))?;
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(src)
        .map_err(|e| io_err("could not list", src, &e))?
        .collect::<Result<_, _>>()
        .map_err(|e| io_err("could not list", src, &e))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let meta =
            std::fs::symlink_metadata(&path).map_err(|e| io_err("could not stat", &path, &e))?;
        if meta.file_type().is_symlink() {
            return Err(format!(
                "{} is a symlink; a skill is copied from the snapshot, never through a link",
                path.display()
            ));
        }
        let target = dst.join(entry.file_name());
        if meta.is_dir() {
            if excluded.iter().any(|x| x == &path) {
                continue;
            }
            copy_tree(&path, &target, excluded)?;
        } else {
            std::fs::copy(&path, &target).map_err(|e| {
                format!(
                    "could not copy {} to {}: {e}",
                    path.display(),
                    target.display()
                )
            })?;
            make_writable(&target)?;
        }
    }
    Ok(())
}

/// Is `name` a build or retired tree this module left behind (`skills.<suffix>.tmp` /
/// `skills.<suffix>.old`)? Only ever judged under the lock, where no population is in flight, so
/// a match is crash debris.
fn is_debris(name: &std::ffi::OsStr) -> bool {
    let s = name.to_string_lossy();
    s.starts_with(&format!("{SKILLS_SUBDIR}."))
        && (s.ends_with(".tmp") || s.ends_with(".old"))
        && s.len() > SKILLS_SUBDIR.len() + 5
}

fn sweep_debris(codex_home: &Path) -> Result<(), String> {
    for entry in
        std::fs::read_dir(codex_home).map_err(|e| io_err("could not list", codex_home, &e))?
    {
        let entry = entry.map_err(|e| io_err("could not list", codex_home, &e))?;
        if is_debris(&entry.file_name()) {
            remove_entry(&entry.path())?;
        }
    }
    Ok(())
}

/// Populate `<codex_home>/skills` with `skills` for `generation` — see the module doc for the
/// contract. Serialized per home by the exclusive lock; idempotent: a marker recording this
/// generation complete with every listed entry present returns `changed: false` without writing.
/// Fails closed (an `Err` names the path and the reason; the caller refuses the launch): a linked
/// or non-directory `skills`, a colliding entry the marker does not list, a symlink in the source
/// or in a carried entry, any I/O failure — and a refusal leaves the visible tree exactly as it
/// found it.
pub(crate) fn populate(
    codex_home: &Path,
    generation: &str,
    skills: &[CodexSkill],
) -> Result<Populated, String> {
    let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
    {
        let mut seen = std::collections::BTreeSet::new();
        for n in &names {
            if n.is_empty() || n.contains(['/', '\\']) || n == "." || n == ".." || n == GEN_MARKER {
                return Err(format!(
                    "skill name {n:?} cannot name a directory under {}",
                    codex_home.join(SKILLS_SUBDIR).display()
                ));
            }
            if !seen.insert(n) {
                return Err(format!(
                    "two skills share the name {n:?}; a flat skills directory can hold one"
                ));
            }
        }
    }
    std::fs::create_dir_all(codex_home).map_err(|e| io_err("could not create", codex_home, &e))?;
    let _lock = lock_home(codex_home)?;
    let skills_dir = codex_home.join(SKILLS_SUBDIR);
    let current = match std::fs::symlink_metadata(&skills_dir) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(format!(
                "{} is a symlink; refusing to populate skills through a link",
                skills_dir.display()
            ))
        }
        Ok(m) if !m.is_dir() => {
            return Err(format!(
                "{} exists and is not a directory",
                skills_dir.display()
            ))
        }
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(io_err("could not stat", &skills_dir, &e)),
    };
    // Re-checked UNDER the lock: a waiter whose generation the winner just built is done.
    let previous = if current {
        read_marker(&skills_dir.join(GEN_MARKER))?
    } else {
        None
    };
    if let Some(m) = &previous {
        if m.generation == generation
            && m.names == names
            && names
                .iter()
                .all(|n| skills_dir.join(n).join("SKILL.md").is_file())
        {
            return Ok(Populated {
                skills_dir,
                names,
                changed: false,
            });
        }
    }
    // Everything in the current tree that wicked did NOT write is carried across unchanged; one
    // that a skill's name would overwrite refuses — before anything is built.
    let mut carry: Vec<PathBuf> = Vec::new();
    if current {
        let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(&skills_dir)
            .map_err(|e| io_err("could not list", &skills_dir, &e))?
            .collect::<Result<_, _>>()
            .map_err(|e| io_err("could not list", &skills_dir, &e))?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name();
            if name == GEN_MARKER {
                continue;
            }
            let owned = previous
                .as_ref()
                .is_some_and(|m| m.names.iter().any(|n| std::ffi::OsStr::new(n) == name));
            if owned {
                continue;
            }
            if names.iter().any(|n| std::ffi::OsStr::new(n) == name) {
                return Err(format!(
                    "{} already exists and no generation marker lists it as wicked's; refusing \
                     to replace it — remove it to let the snapshot populate this seat",
                    entry.path().display()
                ));
            }
            carry.push(entry.path());
        }
    }
    sweep_debris(codex_home)?;
    let suffix = unique_suffix();
    let build = codex_home.join(format!("{SKILLS_SUBDIR}.{suffix}.tmp"));
    let built = (|| -> Result<(), String> {
        std::fs::create_dir_all(&build).map_err(|e| io_err("could not create", &build, &e))?;
        for s in skills {
            copy_tree(&s.dir, &build.join(&s.name), &s.excluded)?;
        }
        for path in &carry {
            let Some(file_name) = path.file_name() else {
                continue;
            };
            let target = build.join(file_name);
            let meta =
                std::fs::symlink_metadata(path).map_err(|e| io_err("could not stat", path, &e))?;
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "{} is a symlink; nothing under an engine-minted seat home is carried \
                     through a link",
                    path.display()
                ));
            }
            if meta.is_dir() {
                copy_tree(path, &target, &[])?;
            } else {
                std::fs::copy(path, &target).map_err(|e| {
                    format!(
                        "could not carry {} to {}: {e}",
                        path.display(),
                        target.display()
                    )
                })?;
                make_writable(&target)?;
            }
        }
        write_marker(&build.join(GEN_MARKER), generation, &names)
    })();
    if let Err(e) = built {
        let _ = remove_entry(&build);
        return Err(e);
    }
    // The swap: the visible tree is always complete — the old one, or the new one.
    let retired = codex_home.join(format!("{SKILLS_SUBDIR}.{suffix}.old"));
    if current {
        if let Err(e) = std::fs::rename(&skills_dir, &retired) {
            let _ = remove_entry(&build);
            return Err(format!(
                "could not retire {} for the swap: {e}",
                skills_dir.display()
            ));
        }
    }
    if let Err(e) = std::fs::rename(&build, &skills_dir) {
        if current {
            let _ = std::fs::rename(&retired, &skills_dir);
        }
        let _ = remove_entry(&build);
        return Err(format!(
            "could not swap the built skills tree into {}: {e}",
            skills_dir.display()
        ));
    }
    if current {
        // Best effort: a retired tree that cannot be removed now is swept by the next population.
        let _ = remove_entry(&retired);
    }
    Ok(Populated {
        skills_dir,
        names,
        changed: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills_snapshot::test_support::{scratch, tree_fingerprint};

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A snapshot-shaped source: `a` (with a support file and an INDEXED nested child
    /// `a/nested`), `b`; the children are read-only like a published generation.
    fn source(base: &Path) -> (PathBuf, Vec<CodexSkill>) {
        let snap = base.join("snap");
        let skills = snap.join("skills");
        write(&skills.join("a").join("SKILL.md"), "---\nname: a\n---\nA\n");
        write(&skills.join("a").join("refs").join("x.md"), "x\n");
        write(
            &skills.join("a").join("nested").join("SKILL.md"),
            "---\nname: a-nested\n---\nN\n",
        );
        write(&skills.join("b").join("SKILL.md"), "---\nname: b\n---\nB\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in [
                skills.join("a").join("SKILL.md"),
                skills.join("a").join("refs").join("x.md"),
                skills.join("a").join("nested").join("SKILL.md"),
                skills.join("b").join("SKILL.md"),
            ] {
                std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o444)).unwrap();
            }
        }
        let list = vec![
            CodexSkill {
                name: "a".into(),
                dir: skills.join("a"),
                excluded: vec![skills.join("a").join("nested")],
            },
            CodexSkill {
                name: "a-nested".into(),
                dir: skills.join("a").join("nested"),
                excluded: vec![],
            },
            CodexSkill {
                name: "b".into(),
                dir: skills.join("b"),
                excluded: vec![],
            },
        ];
        (snap, list)
    }

    fn marker(home: &Path) -> String {
        std::fs::read_to_string(home.join(SKILLS_SUBDIR).join(GEN_MARKER)).unwrap()
    }

    /// The build / retired directories this module may leave under `home` — none, after any
    /// completed population.
    fn debris(home: &Path) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(home)
            .unwrap()
            .flatten()
            .filter(|e| is_debris(&e.file_name()))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    /// Everything under `home` EXCEPT `skills/` and this module's own lock file — the part a
    /// population must never touch. The fingerprint spells relative paths with the HOST separator
    /// (`\` on Windows), so the first component is judged, not a `/`-spelled prefix.
    fn outside_skills(home: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        tree_fingerprint(home)
            .into_iter()
            .filter(|(k, _)| {
                let first = k.split(['/', '\\']).next().unwrap_or_default();
                first != SKILLS_SUBDIR && first != LOCK_FILE
            })
            .collect()
    }

    /// Population lands one FLAT directory per skill by name, copies (regular, writable files),
    /// the nested indexed child excluded from its parent's copy and present under its own name;
    /// the marker records the generation complete; the lock file is the ONE other thing under
    /// the home and no build/retired directory is left behind. A second call on the same
    /// generation writes nothing (byte-identical tree, `changed: false`). Nothing outside
    /// `skills/` moves and the snapshot is untouched.
    #[test]
    fn populates_flat_by_name_with_copies_and_is_a_noop_on_the_same_generation() {
        let base = scratch("codex-populate");
        let (snap, skills) = source(&base);
        let home = base.join("codex-home");
        write(&home.join("config.toml"), "model = \"x\"\n");
        write(&home.join("sessions").join("s1.jsonl"), "{}\n");
        let outside_before = outside_skills(&home);
        let snap_before = tree_fingerprint(&snap);

        let p = populate(&home, "7 abc", &skills).unwrap();
        assert!(p.changed);
        assert_eq!(p.names, vec!["a", "a-nested", "b"]);
        let sd = home.join(SKILLS_SUBDIR);
        assert_eq!(p.skills_dir, sd);
        assert_eq!(
            std::fs::read_to_string(sd.join("a").join("SKILL.md")).unwrap(),
            "---\nname: a\n---\nA\n"
        );
        assert!(sd.join("a").join("refs").join("x.md").is_file());
        assert!(
            std::fs::symlink_metadata(sd.join("a").join("nested")).is_err(),
            "the indexed nested child is excluded from its parent's copy"
        );
        assert!(sd.join("a-nested").join("SKILL.md").is_file());
        assert!(sd.join("b").join("SKILL.md").is_file());
        for f in [sd.join("a").join("SKILL.md"), sd.join("b").join("SKILL.md")] {
            let m = std::fs::symlink_metadata(&f).unwrap();
            assert!(
                m.is_file() && !m.file_type().is_symlink(),
                "{}",
                f.display()
            );
            assert!(
                !m.permissions().readonly(),
                "copies are writable so a later generation can remove them: {}",
                f.display()
            );
        }
        assert_eq!(marker(&home), "done 7 abc\na\na-nested\nb\n");
        assert!(
            home.join(LOCK_FILE).is_file(),
            "the lock lives beside skills/"
        );
        assert!(debris(&home).is_empty(), "{:?}", debris(&home));
        assert_eq!(
            outside_skills(&home),
            outside_before,
            "nothing outside skills/ moved"
        );
        assert_eq!(
            tree_fingerprint(&snap),
            snap_before,
            "the snapshot is never written"
        );

        let before = tree_fingerprint(&home);
        let again = populate(&home, "7 abc", &skills).unwrap();
        assert!(!again.changed, "an unchanged generation is a no-op");
        assert_eq!(again.names, p.names);
        assert_eq!(tree_fingerprint(&home), before);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A NEW generation replaces exactly the entries the previous one wrote — a skill that left
    /// the set is gone, one that stayed is rewritten, a new one lands — and CARRIES an entry the
    /// marker never listed (an operator's own) unchanged. Crash debris (a stranded build dir, a
    /// retired tree, a `skills/` missing between the two renames) is swept and the tree rebuilt;
    /// a tree whose marker claims a generation but lacks an entry is rebuilt too.
    #[test]
    fn a_new_generation_replaces_only_wickeds_entries_and_debris_is_swept() {
        let base = scratch("codex-regen");
        let (snap, skills) = source(&base);
        let home = base.join("codex-home");
        let sd = home.join(SKILLS_SUBDIR);
        write(&sd.join("operator-skill").join("SKILL.md"), "mine\n");
        populate(&home, "1 h1", &skills).unwrap();
        assert!(sd.join("a").is_dir() && sd.join("a-nested").is_dir() && sd.join("b").is_dir());
        assert_eq!(
            std::fs::read_to_string(sd.join("operator-skill").join("SKILL.md")).unwrap(),
            "mine\n",
            "carried into the first generation"
        );

        // Generation 2 drops `a` (and its nested child) and adds `c`.
        let c = snap.join("skills").join("c");
        write(&c.join("SKILL.md"), "---\nname: c\n---\nC\n");
        let gen2 = vec![
            CodexSkill {
                name: "b".into(),
                dir: snap.join("skills").join("b"),
                excluded: vec![],
            },
            CodexSkill {
                name: "c".into(),
                dir: c.clone(),
                excluded: vec![],
            },
        ];
        let p = populate(&home, "2 h2", &gen2).unwrap();
        assert!(p.changed);
        assert!(
            std::fs::symlink_metadata(sd.join("a")).is_err(),
            "stale entry removed"
        );
        assert!(std::fs::symlink_metadata(sd.join("a-nested")).is_err());
        assert!(sd.join("b").join("SKILL.md").is_file());
        assert!(sd.join("c").join("SKILL.md").is_file());
        assert_eq!(
            std::fs::read_to_string(sd.join("operator-skill").join("SKILL.md")).unwrap(),
            "mine\n",
            "an entry the marker never listed is carried, not wicked's to drop"
        );
        assert_eq!(marker(&home), "done 2 h2\nb\nc\n");
        assert!(debris(&home).is_empty(), "{:?}", debris(&home));

        // Crash debris: a stranded build, a retired tree, and no `skills/` at all (a crash
        // between the two renames). The next population sweeps and rebuilds.
        write(
            &home.join("skills.1.2.3.tmp").join("b").join("SKILL.md"),
            "torn",
        );
        write(
            &home.join("skills.4.5.6.old").join("z").join("SKILL.md"),
            "old",
        );
        std::fs::rename(&sd, home.join("skills.7.8.9.old")).unwrap();
        let p = populate(&home, "2 h2", &gen2).unwrap();
        assert!(
            p.changed,
            "a missing tree is rebuilt, never mistaken for complete"
        );
        assert!(sd.join("b").join("SKILL.md").is_file() && sd.join("c").join("SKILL.md").is_file());
        assert_eq!(marker(&home), "done 2 h2\nb\nc\n");
        assert!(debris(&home).is_empty(), "{:?}", debris(&home));

        // A marker that claims a generation whose entry is missing: rebuilt.
        std::fs::remove_dir_all(sd.join("b")).unwrap();
        let p = populate(&home, "2 h2", &gen2).unwrap();
        assert!(p.changed);
        assert!(sd.join("b").join("SKILL.md").is_file());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Fail closed, and leave the visible tree exactly as found: a colliding entry no marker lists
    /// (refused by path, nothing built, nothing swapped); a `skills` that is a symlink or a file;
    /// a symlink inside a source skill (no `skills/` appears, no build dir remains); a name that
    /// cannot be a directory; a duplicate name.
    #[test]
    fn refuses_collisions_links_and_bad_names_without_touching_the_tree() {
        let base = scratch("codex-refuse");
        let (snap, skills) = source(&base);
        let home = base.join("codex-home");
        let sd = home.join(SKILLS_SUBDIR);
        write(&sd.join("b").join("SKILL.md"), "someone else's b\n");
        let before = tree_fingerprint(&sd);
        let err = populate(&home, "1 h", &skills).unwrap_err();
        assert!(
            err.contains(&sd.join("b").display().to_string())
                && err.contains("no generation marker lists it"),
            "{err}"
        );
        assert_eq!(
            tree_fingerprint(&sd),
            before,
            "a refusal leaves the tree as found"
        );
        assert!(debris(&home).is_empty(), "{:?}", debris(&home));

        // An unparseable (or interrupted) marker owns nothing: the same collision, the same
        // refusal.
        std::fs::write(sd.join(GEN_MARKER), "pending 1 h\nb\n").unwrap();
        let before = tree_fingerprint(&sd);
        assert!(populate(&home, "1 h", &skills).is_err());
        assert_eq!(tree_fingerprint(&sd), before);

        // Names that cannot be directories, and duplicates — judged before any lock or write.
        let home2 = base.join("home2");
        for bad in ["", "..", "x/y", "x\\y", GEN_MARKER] {
            let err = populate(
                &home2,
                "1 h",
                &[CodexSkill {
                    name: bad.into(),
                    dir: snap.join("skills").join("b"),
                    excluded: vec![],
                }],
            )
            .unwrap_err();
            assert!(err.contains("cannot name a directory"), "{bad:?}: {err}");
        }
        let dup = vec![skills[2].clone(), skills[2].clone()];
        assert!(populate(&home2, "1 h", &dup)
            .unwrap_err()
            .contains("share the name"));
        assert!(
            std::fs::symlink_metadata(&home2).is_err(),
            "a name refusal happens before the home is even created"
        );

        // `skills` as a file.
        let home3 = base.join("home3");
        write(&home3.join(SKILLS_SUBDIR), "not a dir");
        assert!(populate(&home3, "1 h", &skills)
            .unwrap_err()
            .contains("not a directory"));

        #[cfg(unix)]
        {
            // `skills` as a symlink.
            let home4 = base.join("home4");
            std::fs::create_dir_all(&home4).unwrap();
            let elsewhere = base.join("elsewhere");
            std::fs::create_dir_all(&elsewhere).unwrap();
            std::os::unix::fs::symlink(&elsewhere, home4.join(SKILLS_SUBDIR)).unwrap();
            let err = populate(&home4, "1 h", &skills).unwrap_err();
            assert!(err.contains("symlink"), "{err}");
            assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());

            // A symlink inside a source skill refuses the copy: no tree appears, no build stays.
            let home5 = base.join("home5");
            let linked = snap.join("skills").join("linked");
            std::fs::create_dir_all(&linked).unwrap();
            std::fs::write(linked.join("SKILL.md"), "---\nname: linked\n---\n").unwrap();
            std::os::unix::fs::symlink(snap.join("skills").join("b"), linked.join("sib")).unwrap();
            let err = populate(
                &home5,
                "1 h",
                &[CodexSkill {
                    name: "linked".into(),
                    dir: linked,
                    excluded: vec![],
                }],
            )
            .unwrap_err();
            assert!(err.contains("symlink"), "{err}");
            assert!(std::fs::symlink_metadata(home5.join(SKILLS_SUBDIR)).is_err());
            assert!(debris(&home5).is_empty(), "{:?}", debris(&home5));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Two threads populating ONE home with DIFFERENT generations, many times over, serialize on
    /// the lock: every call returns `Ok`, and at the end the visible tree is COMPLETE and
    /// CONSISTENT — its marker names one of the two generations and exactly that generation's
    /// entries are present with that generation's content — with no build or retired directory
    /// left behind. Without the lock and the swap, a second launch could strip a first launch's
    /// in-flight copies and leave a mixture of the two.
    #[test]
    fn concurrent_populations_of_one_home_serialize_and_leave_a_complete_tree() {
        let base = scratch("codex-concurrent");
        let (snap, gen_a) = source(&base);
        let c = snap.join("skills").join("c");
        write(&c.join("SKILL.md"), "---\nname: c\n---\nC\n");
        let gen_b = vec![
            CodexSkill {
                name: "b".into(),
                dir: snap.join("skills").join("b"),
                excluded: vec![],
            },
            CodexSkill {
                name: "c".into(),
                dir: c,
                excluded: vec![],
            },
        ];
        let home = base.join("codex-home");
        std::fs::create_dir_all(&home).unwrap();
        const ROUNDS: usize = 8;
        let results: Vec<Result<Populated, String>> = std::thread::scope(|s| {
            let ta = s.spawn(|| {
                (0..ROUNDS)
                    .map(|_| populate(&home, "A ha", &gen_a))
                    .collect::<Vec<_>>()
            });
            let tb = s.spawn(|| {
                (0..ROUNDS)
                    .map(|_| populate(&home, "B hb", &gen_b))
                    .collect::<Vec<_>>()
            });
            let mut all = ta.join().unwrap();
            all.extend(tb.join().unwrap());
            all
        });
        for r in &results {
            assert!(r.is_ok(), "every population completes: {r:?}");
        }
        assert_eq!(results.len(), 2 * ROUNDS);
        let sd = home.join(SKILLS_SUBDIR);
        let m = marker(&home);
        let (expect_present, expect_absent): (&[&str], &[&str]) = if m.starts_with("done A ha\n") {
            (&["a", "a-nested", "b"], &["c"])
        } else {
            assert!(m.starts_with("done B hb\n"), "{m}");
            (&["b", "c"], &["a", "a-nested"])
        };
        for n in expect_present {
            let text = std::fs::read_to_string(sd.join(n).join("SKILL.md"))
                .unwrap_or_else(|e| panic!("{n} present in the winning tree: {e}"));
            assert!(
                text.contains(&format!("name: {n}\n")),
                "{n} holds its own generation's content: {text}"
            );
        }
        for n in expect_absent {
            assert!(
                std::fs::symlink_metadata(sd.join(n)).is_err(),
                "{n} does not belong to the winning generation"
            );
        }
        let listed: Vec<&str> = m.lines().skip(1).collect();
        assert_eq!(
            listed,
            expect_present.to_vec(),
            "the marker lists exactly the tree"
        );
        assert!(debris(&home).is_empty(), "{:?}", debris(&home));
        let _ = std::fs::remove_dir_all(&base);
    }
}
