//! Whether a tool call's path lies inside the boundary this unit was given (FINDING-045).
//!
//! # The gap
//!
//! A unit is handed a worktree. Nothing enforced it. Re-measuring the campaign transcripts — 959
//! tool calls, 915 path-bearing — found **40 hostile references outside the worktree**: the
//! operator's brain store, `~/.wicked-crew/core.db`, another org group's tree, whole-filesystem
//! scans. Twenty of them were WRITES.
//!
//! A unit writing outside its worktree is not a hypothetical: FINDING-067 is a governed worker that
//! ran `wicked-estate index .` and deleted all 833 nodes of the platform's operational state.
//!
//! # What this covers, and what it does not
//!
//! This is the POLICY layer. It sees a tool call's path ARGUMENT, so it covers the tools that carry
//! one — `Write`, `Edit`, `Read`, `NotebookEdit`. Measured against the same corpus that produced the
//! finding, that is **23 of 40 hostile escapes (58%), including all 20 hostile writes**.
//!
//! It does NOT see paths inside a shell string. `bash -c 'cat ~/.wicked-brain/x'` is one allowed
//! call and unbounded reach; 17 of the 40 were exactly that. Parsing shell to find them would be
//! pattern-matching dressed as a boundary — variables, substitution and `eval` make it unbounded —
//! and this codebase has enough presence-shaped gates already. Closing that residual needs a kernel
//! boundary, which is a separate layer with per-platform availability.
//!
//! **The claim is therefore quantified, never "confined":** policy-checked, N shell calls
//! unexamined. A caller that reports this as confinement is asserting something it does not have.
//!
//! # Allow by root, not by pattern
//!
//! A denylist of `~/.wicked-brain` is one rename from useless. The allowed roots are closed by
//! construction: everything outside them denies. The read-only roots are not a convenience — the
//! same corpus shows **17 legitimate out-of-worktree reads** (the worker loading its own skill
//! definitions, language runtimes, package caches). A boundary that breaks every real run gets
//! turned off, and a boundary that is off is worse than none because it is believed.

use std::path::{Component, Path, PathBuf};

/// The boundary a unit runs inside.
#[derive(Debug, Clone, Default)]
pub struct AllowedRoots {
    /// Readable AND writable — in practice the unit's worktree.
    pub write: Vec<PathBuf>,
    /// Readable only. Evidence-derived (see module docs), not guessed.
    pub read: Vec<PathBuf>,
}

/// Why a path was refused. Carries what the agent needs to retry.
#[derive(Debug, Clone, PartialEq)]
pub struct Denial {
    /// The path as resolved, not as written — `~`, `..` and symlinks already collapsed.
    pub resolved: PathBuf,
    /// True when the call wanted to write.
    pub write: bool,
    /// Where the call WOULD have been allowed.
    pub allowed: Vec<PathBuf>,
}

impl std::fmt::Display for Denial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Naming the allowed roots is the point, not decoration. An agent told only "denied" retries
        // the same thing or fails the unit; one told where it MAY look adapts. Same principle as
        // FINDING-066 — a remedy that cannot be acted on is not a remedy.
        write!(
            f,
            "path outside this unit's boundary: {} ({}). Allowed: {}",
            self.resolved.display(),
            if self.write { "write" } else { "read" },
            self.allowed
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Collapse `~`, `.` and `..` WITHOUT touching the filesystem.
///
/// `std::fs::canonicalize` cannot be the whole answer: a `Write` to a file that does not exist yet
/// fails it, and that is the single most important case to check. So resolve logically first, then
/// canonicalize the nearest EXISTING ancestor to defeat symlinks.
fn normalize(raw: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    let expanded: PathBuf = match (raw.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => h.join(rest),
        _ if raw == "~" => home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(raw)),
        _ => PathBuf::from(raw),
    };
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };

    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                // Popping is what makes `../../etc/hosts` resolvable at all. Refusing to pop past
                // the root is deliberate: `/..` is `/`, not an error.
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve symlinks as far as the filesystem allows.
///
/// Walks up to the nearest existing ancestor, canonicalizes THAT, then re-appends the tail. A
/// symlink inside the worktree pointing at `~/.ssh` is the obvious escape and must not survive.
fn resolve_symlinks(p: &Path) -> PathBuf {
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(real) = std::fs::canonicalize(cur) {
            let mut out = real;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                cur = parent;
            }
            // Nothing on this path exists (or we hit the root) — the logical form is the best
            // available answer, and it is still comparable against the roots.
            _ => return p.to_path_buf(),
        }
    }
}

/// Is `path` inside the unit's boundary?
///
/// `write` selects which root set applies: a read may use either list, a write only the write list.
/// Fails CLOSED — an empty root set allows nothing, because "no boundary configured" must not read
/// as "no boundary needed".
pub fn check(
    raw: &str,
    roots: &AllowedRoots,
    write: bool,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<PathBuf, Denial> {
    let resolved = resolve_symlinks(&normalize(raw, cwd, home));

    let permitted: Vec<&PathBuf> = if write {
        roots.write.iter().collect()
    } else {
        roots.write.iter().chain(roots.read.iter()).collect()
    };

    for root in &permitted {
        if resolved_is_within(&resolved, root) {
            return Ok(resolved);
        }
    }
    Err(Denial {
        resolved,
        write,
        allowed: permitted.into_iter().cloned().collect(),
    })
}

/// Is `resolved` the root itself or a descendant of it, compared against the SYMLINK-RESOLVED root?
///
/// The per-root containment test [`check`] applies, factored out so a caller deciding a carve-out
/// (the `~/.claude` advisory downgrade in [`crate::gate_hook::boundary_denial`], core#235) uses the
/// IDENTICAL symlink-aware matching rather than a naive `starts_with` that a `/tmp`→`/private/tmp`
/// symlink would defeat. `resolved` is expected already symlink-resolved (as [`Denial::resolved`]
/// is); `root` is resolved here. Comparing against the resolved ROOT too is what lets a worktree
/// reached through a symlink match at all.
pub fn resolved_is_within(resolved: &Path, root: &Path) -> bool {
    let root_real = resolve_symlinks(root);
    resolved == root_real || resolved.starts_with(&root_real)
}

/// Validate launcher-declared extra write roots at LAUNCH time (core#259), before any session is
/// persisted. Fails the launch loudly rather than arming a boundary that would reopen FINDING-098.
///
/// Two rules, both fail-closed:
/// - Every root must be ABSOLUTE. A relative root would be resolved against whatever cwd the
///   launcher happened to have, which is not a statement of intent.
/// - No root may contain or be contained by the engine's own config tree
///   (`~/.config/wicked-core` — the workflow overlays and gate pins). "Contain" cuts both ways:
///   `~/.config/wicked-core/x` is inside the pin tree, and `~` / `/` CONTAIN it — either direction
///   hands a governed worker write access to the pin that gates its own work.
///
/// Symlink-resolved with the same [`resolve_symlinks`] the boundary check uses, so a root reached
/// through `/tmp`→`/private/tmp` (or a symlinked config dir) cannot dodge the comparison.
pub fn validate_extra_write_roots(roots: &[String], home: Option<&Path>) -> Result<(), String> {
    if roots.is_empty() {
        return Ok(());
    }
    let config_tree = match home {
        Some(h) => resolve_symlinks(&h.join(".config").join("wicked-core")),
        // No HOME ⇒ the pin tree cannot be located, so containment cannot be proven either way.
        // Fail CLOSED: refuse the widening rather than arm roots we cannot judge.
        None => {
            return Err(
                "extra write roots need $HOME to validate against the engine config tree; \
                 refusing to widen the boundary without it"
                    .to_string(),
            )
        }
    };
    for raw in roots {
        let p = Path::new(raw);
        if !p.is_absolute() {
            return Err(format!(
                "extra write root is not absolute: {raw} (a relative root binds to the \
                 launcher's incidental cwd, not a declared destination)"
            ));
        }
        let resolved = resolve_symlinks(p);
        if resolved_is_within(&resolved, &config_tree)
            || resolved_is_within(&config_tree, &resolved)
        {
            return Err(format!(
                "extra write root {raw} would expose the engine config tree ({}) — a governed \
                 worker could rewrite the pin that gates its own work (FINDING-098); refused",
                config_tree.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(dir: &Path) -> AllowedRoots {
        AllowedRoots {
            write: vec![dir.to_path_buf()],
            read: vec![],
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pp_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(&d).unwrap()
    }

    /// The containment predicate `check` uses per-root, and the carve-out in
    /// `gate_hook::boundary_denial` (core#235) reuses: the root itself and descendants match, a
    /// sibling does not. Mutation: hardcode the body `true` → the sibling assert fails; `false` →
    /// the root/descendant asserts fail.
    #[test]
    fn resolved_is_within_matches_the_root_and_descendants_but_not_siblings() {
        let base = scratch("within");
        assert!(
            resolved_is_within(&base, &base),
            "the root is within itself"
        );
        assert!(
            resolved_is_within(&base.join("a/b/c.rs"), &base),
            "a descendant is within"
        );
        let sibling = base.parent().unwrap().join("pp_within_sibling");
        assert!(
            !resolved_is_within(&sibling, &base),
            "a sibling that merely shares a parent is NOT within"
        );
    }

    /// C1 — the finding's own shape: a read of the operator's brain store from inside a unit.
    #[test]
    fn a_path_outside_every_root_is_denied_and_names_the_resolved_path() {
        let wt = scratch("outside");
        // A REAL directory, not a hand-written "/Users/someone": on Windows a path beginning with
        // `/` carries no drive and is therefore NOT absolute, so `normalize` correctly joins it to
        // cwd and this assertion became meaningless. The bug was in this test, not the policy —
        // CI on windows-latest is what caught it.
        let home = scratch("outside_home");
        let d = check(
            "~/.wicked-brain/projects/x/brain.json",
            &roots(&wt),
            false,
            &wt,
            Some(&home),
        )
        .expect_err("must deny");
        assert!(
            d.resolved.starts_with(&home),
            "`~` must be expanded before comparison, got {}",
            d.resolved.display()
        );
        assert!(d.to_string().contains(".wicked-brain"), "{d}");
        // A5: the message must say where the agent MAY look, or it cannot retry.
        assert!(
            d.to_string().contains(&wt.display().to_string()),
            "no allowed root named: {d}"
        );
    }

    /// C3 — `..` traversal. The check must resolve, not string-match.
    #[test]
    fn dot_dot_traversal_out_of_the_worktree_is_denied() {
        let wt = scratch("dotdot");
        let d = check("../../etc/hosts", &roots(&wt), false, &wt, None).expect_err("must deny");
        assert!(
            !d.resolved.to_string_lossy().contains(".."),
            "unresolved: {}",
            d.resolved.display()
        );
        assert!(
            d.resolved.ends_with("etc/hosts"),
            "{}",
            d.resolved.display()
        );
    }

    /// C2 — a symlink inside the worktree pointing out of it. The obvious escape.
    #[cfg(unix)]
    #[test]
    fn a_symlink_escaping_the_worktree_is_denied() {
        let wt = scratch("symlink");
        let outside = scratch("symlink_target");
        std::fs::write(outside.join("secret"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, wt.join("escape")).unwrap();

        let d = check("escape/secret", &roots(&wt), false, &wt, None)
            .expect_err("a symlink out of the worktree must not be followed into an allow");
        assert!(
            d.resolved.starts_with(&outside),
            "symlink was not resolved: {}",
            d.resolved.display()
        );
    }

    /// C4 — the invariant that decides shippability. Ordinary in-worktree work must be untouched,
    /// including a write to a file that does NOT exist yet (which `canonicalize` alone fails).
    #[test]
    fn ordinary_work_inside_the_worktree_is_allowed() {
        let wt = scratch("inside");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join("src/main.rs"), b"fn main(){}").unwrap();

        check("src/main.rs", &roots(&wt), false, &wt, None).expect("existing file read");
        check("src/new_file.rs", &roots(&wt), true, &wt, None).expect("write to a NEW file");
        check(
            wt.join("src/main.rs").to_str().unwrap(),
            &roots(&wt),
            true,
            &wt,
            None,
        )
        .expect("absolute in-worktree write");
    }

    /// The read allowlist exists because 17 measured escapes were legitimate — the worker loading
    /// its own skills. Read-only means read-only: the same path must still refuse a WRITE.
    #[test]
    fn a_read_only_root_permits_reads_and_refuses_writes() {
        let wt = scratch("ro_wt");
        let skills = scratch("ro_skills");
        std::fs::write(skills.join("skill.md"), b"x").unwrap();
        let r = AllowedRoots {
            write: vec![wt.clone()],
            read: vec![skills.clone()],
        };
        let p = skills.join("skill.md");
        check(p.to_str().unwrap(), &r, false, &wt, None).expect("read of an allowlisted root");
        let d = check(p.to_str().unwrap(), &r, true, &wt, None)
            .expect_err("a read-only root must refuse a write");
        assert!(d.write);
    }

    /// Fails closed: no configured boundary allows nothing. "Not configured" must never read as
    /// "not needed" — that is the degrade-silently pattern this codebase keeps paying for.
    #[test]
    fn an_empty_root_set_allows_nothing() {
        let wt = scratch("empty");
        let d = check("src/main.rs", &AllowedRoots::default(), false, &wt, None)
            .expect_err("empty roots must deny");
        assert!(d.allowed.is_empty());
    }

    /// core#259 — the launch-time judgement on launcher-declared deliverable roots. A scratch dir
    /// passes; anything touching the pin tree, in EITHER containment direction, is refused, and so
    /// is a relative root (it would bind to the launcher's incidental cwd).
    #[test]
    fn extra_write_roots_validation_admits_scratch_and_refuses_the_pin_tree() {
        let home = scratch("xwr_home");
        let inbox = scratch("xwr_inbox");

        // Declaring nothing is always fine — and needs no HOME.
        validate_extra_write_roots(&[], None).expect("empty roots need no validation");

        // A scratch inbox outside the pin tree is the intended use.
        validate_extra_write_roots(&[inbox.to_string_lossy().into_owned()], Some(&home))
            .expect("a scratch inbox must be admitted");

        // Relative → refused (binds to incidental cwd, not a declared destination).
        let e = validate_extra_write_roots(&["relative/inbox".to_string()], Some(&home))
            .expect_err("a relative root must be refused");
        assert!(e.contains("not absolute"), "names the failure: {e}");

        // Inside the pin tree → refused (FINDING-098: the worker could rewrite its own gate pin).
        let pin_child = home.join(".config/wicked-core/workflows");
        let e =
            validate_extra_write_roots(&[pin_child.to_string_lossy().into_owned()], Some(&home))
                .expect_err("a root inside the pin tree must be refused");
        assert!(e.contains("FINDING-098"), "names the escape: {e}");

        // CONTAINING the pin tree (the home dir itself) → refused for the same reason.
        let e = validate_extra_write_roots(&[home.to_string_lossy().into_owned()], Some(&home))
            .expect_err("a root containing the pin tree must be refused");
        assert!(e.contains("FINDING-098"), "names the escape: {e}");

        // Roots present but no HOME to judge against → fail CLOSED, not open.
        let e = validate_extra_write_roots(&[inbox.to_string_lossy().into_owned()], None)
            .expect_err("no HOME must refuse the widening, never wave it through");
        assert!(e.contains("HOME"), "names the missing prerequisite: {e}");
    }
}
