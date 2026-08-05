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
        // Compare against the resolved ROOT too: a worktree reached through a symlink (macOS
        // `/tmp` -> `/private/tmp` is the everyday case) would otherwise never match.
        let root_real = resolve_symlinks(root);
        if resolved == root_real || resolved.starts_with(&root_real) {
            return Ok(resolved);
        }
    }
    Err(Denial {
        resolved,
        write,
        allowed: permitted.into_iter().cloned().collect(),
    })
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

    /// C1 — the finding's own shape: a read of the operator's brain store from inside a unit.
    #[test]
    fn a_path_outside_every_root_is_denied_and_names_the_resolved_path() {
        let wt = scratch("outside");
        let home = PathBuf::from("/Users/someone");
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
}
