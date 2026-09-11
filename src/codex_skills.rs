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
//! - a generation MARKER (`.wicked-skills-gen`) listing the generation and every entry written,
//!   so a relaunch on an unchanged generation is a no-op and a NEW generation removes exactly the
//!   entries the previous one wrote — stale generations never accumulate;
//! - nothing else under `CODEX_HOME` is read or written, and an entry the marker does not list is
//!   never replaced (refused by path).
//!
//! The marker is written `pending` before the copies and `done` after them, so a launch that dies
//! mid-copy leaves a state the next launch repairs (its listed entries are removed and rewritten)
//! rather than one it mistakes for complete.

use std::path::{Path, PathBuf};

/// The subdirectory of `CODEX_HOME` codex scans for skills.
pub(crate) const SKILLS_SUBDIR: &str = "skills";
/// The generation marker inside `<CODEX_HOME>/skills`.
pub(crate) const GEN_MARKER: &str = ".wicked-skills-gen";

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
    /// lists is present — nothing was written.
    pub changed: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct Marker {
    done: bool,
    generation: String,
    names: Vec<String>,
}

fn io_err(what: &str, path: &Path, e: &std::io::Error) -> String {
    format!("{what} {}: {e}", path.display())
}

/// The marker at `path`: `None` when absent or unparseable (an unparseable marker names nothing
/// wicked owns, so nothing is removed and any collision refuses).
fn read_marker(path: &Path) -> Result<Option<Marker>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err("could not read the generation marker", path, &e)),
    };
    let mut lines = text.lines();
    let Some(head) = lines.next() else {
        return Ok(None);
    };
    let (state, generation) = head.split_once(' ').unwrap_or((head, ""));
    let done = match state {
        "done" => true,
        "pending" => false,
        _ => return Ok(None),
    };
    Ok(Some(Marker {
        done,
        generation: generation.to_string(),
        names: lines
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect(),
    }))
}

/// Write the marker atomically (temp + rename) — a reader never sees a half-written list.
fn write_marker(path: &Path, done: bool, generation: &str, names: &[String]) -> Result<(), String> {
    let mut text = format!("{} {generation}\n", if done { "done" } else { "pending" });
    for n in names {
        text.push_str(n);
        text.push('\n');
    }
    let tmp = path.with_file_name(format!("{GEN_MARKER}.{}.tmp", std::process::id()));
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

/// Populate `<codex_home>/skills` with `skills` for `generation` — see the module doc for the
/// contract. Idempotent: a marker recording this generation `done` with every listed entry
/// present returns `changed: false` without writing. Fails closed (an `Err` names the path and
/// the reason; the caller refuses the launch): a linked or non-directory `skills`, a colliding
/// entry the marker does not list, a symlink in the source, any I/O failure.
pub(crate) fn populate(
    codex_home: &Path,
    generation: &str,
    skills: &[CodexSkill],
) -> Result<Populated, String> {
    let skills_dir = codex_home.join(SKILLS_SUBDIR);
    match std::fs::symlink_metadata(&skills_dir) {
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
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(&skills_dir)
                .map_err(|e| io_err("could not create", &skills_dir, &e))?;
        }
        Err(e) => return Err(io_err("could not stat", &skills_dir, &e)),
    }
    let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
    {
        let mut seen = std::collections::BTreeSet::new();
        for n in &names {
            if n.is_empty() || n.contains(['/', '\\']) || n == "." || n == ".." {
                return Err(format!(
                    "skill name {n:?} cannot name a directory under {}",
                    skills_dir.display()
                ));
            }
            if !seen.insert(n) {
                return Err(format!(
                    "two skills share the name {n:?}; a flat skills directory can hold one"
                ));
            }
        }
    }
    let marker_path = skills_dir.join(GEN_MARKER);
    let previous = read_marker(&marker_path)?;
    if let Some(m) = &previous {
        if m.done
            && m.generation == generation
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
        // Exactly what the previous population wrote — done or interrupted — is removed.
        for n in &m.names {
            remove_entry(&skills_dir.join(n))?;
        }
    }
    // An entry wicked did not write is never replaced.
    for n in &names {
        let target = skills_dir.join(n);
        if std::fs::symlink_metadata(&target).is_ok() {
            return Err(format!(
                "{} already exists and no generation marker lists it as wicked's; refusing to \
                 replace it — remove it to let the snapshot populate this seat",
                target.display()
            ));
        }
    }
    write_marker(&marker_path, false, generation, &names)?;
    for s in skills {
        copy_tree(&s.dir, &skills_dir.join(&s.name), &s.excluded)?;
    }
    write_marker(&marker_path, true, generation, &names)?;
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

    /// Everything under `home` EXCEPT `skills/` — the part a population must never touch. The
    /// fingerprint spells relative paths with the HOST separator (`\` on Windows), so the first
    /// component is judged, not a `/`-spelled prefix.
    fn outside_skills(home: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        tree_fingerprint(home)
            .into_iter()
            .filter(|(k, _)| k.split(['/', '\\']).next().unwrap_or_default() != SKILLS_SUBDIR)
            .collect()
    }

    /// Population lands one FLAT directory per skill by name, copies (regular, writable files),
    /// the nested indexed child excluded from its parent's copy and present under its own name;
    /// the marker records the generation complete. A second call on the same generation writes
    /// nothing (byte-identical tree, `changed: false`). Nothing outside `skills/` moves and the
    /// snapshot is untouched.
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

    /// A NEW generation removes exactly the entries the previous one wrote — a skill that left
    /// the set is gone, one that stayed is rewritten, a new one lands — and leaves an entry the
    /// marker never listed (an operator's own) untouched. A `pending` marker (a launch that died
    /// mid-copy) is repaired: its listed entries are removed and rewritten, and the marker ends
    /// `done`.
    #[test]
    fn a_new_generation_replaces_only_wickeds_entries_and_a_pending_marker_is_repaired() {
        let base = scratch("codex-regen");
        let (snap, skills) = source(&base);
        let home = base.join("codex-home");
        let sd = home.join(SKILLS_SUBDIR);
        write(&sd.join("operator-skill").join("SKILL.md"), "mine\n");
        populate(&home, "1 h1", &skills).unwrap();
        assert!(sd.join("a").is_dir() && sd.join("a-nested").is_dir() && sd.join("b").is_dir());

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
            "an entry the marker never listed is not wicked's to touch"
        );
        assert_eq!(marker(&home), "done 2 h2\nb\nc\n");

        // A launch that died mid-copy: marker `pending`, `c` half-written, `b` missing.
        std::fs::write(sd.join(GEN_MARKER), "pending 3 h3\nb\nc\n").unwrap();
        let _ = std::fs::remove_dir_all(sd.join("b"));
        std::fs::write(sd.join("c").join("SKILL.md"), "torn").unwrap();
        let p = populate(&home, "3 h3", &gen2).unwrap();
        assert!(p.changed, "a pending marker is never mistaken for complete");
        assert_eq!(
            std::fs::read_to_string(sd.join("c").join("SKILL.md")).unwrap(),
            "---\nname: c\n---\nC\n"
        );
        assert!(sd.join("b").join("SKILL.md").is_file());
        assert_eq!(marker(&home), "done 3 h3\nb\nc\n");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Fail closed, and leave the tree as found: a colliding entry no marker lists (refused by
    /// path, nothing written — not even the marker); a `skills` that is a symlink or a file; a
    /// symlink inside a source skill; a name that cannot be a directory; a duplicate name.
    #[test]
    fn refuses_collisions_links_and_bad_names_without_touching_the_tree() {
        let base = scratch("codex-refuse");
        let (snap, skills) = source(&base);
        let home = base.join("codex-home");
        let sd = home.join(SKILLS_SUBDIR);
        write(&sd.join("b").join("SKILL.md"), "someone else's b\n");
        let before = tree_fingerprint(&home);
        let err = populate(&home, "1 h", &skills).unwrap_err();
        assert!(
            err.contains(&sd.join("b").display().to_string())
                && err.contains("no generation marker lists it"),
            "{err}"
        );
        assert_eq!(tree_fingerprint(&home), before, "a refusal writes nothing");

        // An unparseable marker owns nothing: the same collision, the same refusal.
        std::fs::write(sd.join(GEN_MARKER), "garbage\nb\n").unwrap();
        let before = tree_fingerprint(&home);
        assert!(populate(&home, "1 h", &skills).is_err());
        assert_eq!(tree_fingerprint(&home), before);

        // Names that cannot be directories, and duplicates.
        let home2 = base.join("home2");
        for bad in ["", "..", "x/y", "x\\y"] {
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

            // A symlink inside a source skill refuses the copy.
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
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
