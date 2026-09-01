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
    validate_extra_roots(
        roots,
        home,
        "write",
        "a governed worker could rewrite the pin that gates its own work (FINDING-098)",
    )
}

/// Validate launcher-declared extra READ roots at LAUNCH time (core#294) — the read-only mirror of
/// [`validate_extra_write_roots`], and judged by the same two fail-closed rules.
///
/// A read root never widens write scope ([`check`] tests a write against the write list alone), so
/// the FINDING-098 pin-REWRITE escape cannot ride one. The pin-tree containment rule still applies,
/// in both directions, because the grant is still a grant:
/// - a root INSIDE the config tree hands a governed worker the text of the very pins and overlays
///   that gate its own work — a creator that can read its evaluator's rules can write to them;
/// - a root CONTAINING it (`~`, `/`) is an over-broad grant by construction — "ground this run in
///   X" names X, never the operator's whole home.
pub fn validate_extra_read_roots(roots: &[String], home: Option<&Path>) -> Result<(), String> {
    validate_extra_roots(
        roots,
        home,
        "read",
        "even read-only, the pin that gates a governed worker's own work must stay outside \
         every launch-declared root (core#294, mirroring FINDING-098)",
    )
}

/// The shared body of [`validate_extra_write_roots`] / [`validate_extra_read_roots`]: one judgement,
/// two spellings, so the read mirror cannot drift from the write original (core#294). `kind` names
/// the root set in every message; `exposure` states what admitting a pin-tree root would hand over.
fn validate_extra_roots(
    roots: &[String],
    home: Option<&Path>,
    kind: &str,
    exposure: &str,
) -> Result<(), String> {
    if roots.is_empty() {
        return Ok(());
    }
    let config_tree = match home {
        Some(h) => resolve_symlinks(&h.join(".config").join("wicked-core")),
        // No HOME ⇒ the pin tree cannot be located, so containment cannot be proven either way.
        // Fail CLOSED: refuse the widening rather than arm roots we cannot judge.
        None => {
            return Err(format!(
                "extra {kind} roots need $HOME to validate against the engine config tree; \
                 refusing to widen the boundary without it"
            ))
        }
    };
    for raw in roots {
        let p = Path::new(raw);
        if !p.is_absolute() {
            return Err(format!(
                "extra {kind} root is not absolute: {raw} (a relative root binds to the \
                 launcher's incidental cwd, not a declared destination)"
            ));
        }
        let resolved = resolve_symlinks(p);
        if resolved_is_within(&resolved, &config_tree)
            || resolved_is_within(&config_tree, &resolved)
        {
            return Err(format!(
                "extra {kind} root {raw} would expose the engine config tree ({}) — {exposure}; \
                 refused",
                config_tree.display()
            ));
        }
    }
    Ok(())
}

/// The declared deliverables a unit did NOT produce, or `None` if all are present (FINDING-101,
/// widened by core#297 §3).
///
/// A deliverable counts as produced if it exists as a file OR a directory (a phase may declare a
/// directory of outputs). Checking existence is the whole point — this is a SUBSTANCE check, the
/// opposite of the presence-shaped gates this campaign keeps filing: the phase said it would
/// produce X, so require X, not a status code claiming it did.
///
/// # Where a deliverable may live
///
/// `cwd` is the unit's own working directory (its worktree, or the per-run sandbox for an unbound
/// run). `write_roots` is the run's launch-validated
/// [`crate::workflow::GovernanceContext::extra_write_roots`] — the boundary the launcher DECLARED
/// this run may write outside its cwd, already vetted by [`validate_extra_write_roots`].
///
/// Both are searched, because searching only `cwd` left an unbound run with no working spelling at
/// all (core#297 §3): every crew interactive seam omits `repoRef` on purpose, so its cwd is a
/// throwaway sandbox while its actual deliverable is a file in a per-run inbox declared as a write
/// root. Relative resolved against the sandbox; absolute was rejected by construction. A run that
/// cannot honestly declare what it must produce declares nothing, which is how a floor dies.
///
/// # What stays unverifiable
///
/// Widening to the DECLARED roots is a resolution rule, not an amnesty. Fail-closed in both
/// remaining directions, because "the engine could not locate it" is not evidence a phase
/// completed:
///
/// - An ABSOLUTE deliverable must resolve inside one of the declared roots. Without that clause a
///   workflow could aim the floor at any pre-existing file on the box (`/etc/hosts`) and pass
///   forever — and this run never had permission to create it anyway.
/// - A `..`-escaping RELATIVE deliverable is refused outright rather than resolved-then-checked:
///   its target depends on which base it is joined to, so the same declaration would name
///   different files per root, and a floor whose subject is ambiguous is not a floor.
///
/// Symlink-resolved with the same [`resolve_symlinks`] the boundary check uses, so a root reached
/// through `/tmp`→`/private/tmp` cannot dodge the containment test (macOS temp dirs are exactly
/// that symlink, so this is the common case, not the exotic one).
pub(crate) fn missing_deliverables(
    declared: &[String],
    cwd: &Path,
    write_roots: &[String],
) -> Option<String> {
    let missing: Vec<&str> = declared
        .iter()
        .filter(|d| !d.trim().is_empty())
        .filter(|d| !deliverable_exists(d, cwd, write_roots))
        .map(String::as_str)
        .collect();
    (!missing.is_empty()).then(|| missing.join(", "))
}

/// One deliverable's presence test — see [`missing_deliverables`] for the rules and why.
fn deliverable_exists(declared: &str, cwd: &Path, write_roots: &[String]) -> bool {
    let p = Path::new(declared);
    if p.is_absolute() {
        let resolved = resolve_symlinks(p);
        return write_roots
            .iter()
            .any(|r| resolved_is_within(&resolved, Path::new(r)))
            && p.exists();
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    cwd.join(p).exists() || write_roots.iter().any(|r| Path::new(r).join(p).exists())
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

    /// core#294 — the launch-time judgement on launcher-declared READ roots mirrors the write one
    /// rule for rule: a scratch source dir is admitted; relative, pin-tree-containing (either
    /// direction) and HOME-less declarations are refused. Read-only never relaxes the vetting —
    /// the grant is still a grant.
    #[test]
    fn extra_read_roots_validation_mirrors_the_write_rules() {
        let home = scratch("xrr_home");
        let repo = scratch("xrr_repo");

        // Declaring nothing is always fine — and needs no HOME.
        validate_extra_read_roots(&[], None).expect("empty roots need no validation");

        // A repo checkout outside the pin tree is the intended use ("ground this run in X").
        validate_extra_read_roots(&[repo.to_string_lossy().into_owned()], Some(&home))
            .expect("a source tree must be admitted");

        // Relative → refused (binds to incidental cwd, not a declared source).
        let e = validate_extra_read_roots(&["relative/repo".to_string()], Some(&home))
            .expect_err("a relative root must be refused");
        assert!(e.contains("not absolute"), "names the failure: {e}");

        // Inside the pin tree → refused even read-only (the worker would read the very pins that
        // gate its own work).
        let pin_child = home.join(".config/wicked-core/workflows");
        let e = validate_extra_read_roots(&[pin_child.to_string_lossy().into_owned()], Some(&home))
            .expect_err("a root inside the pin tree must be refused");
        assert!(e.contains("FINDING-098"), "names the escape: {e}");

        // CONTAINING the pin tree (the home dir itself) → refused: an over-broad grant.
        let e = validate_extra_read_roots(&[home.to_string_lossy().into_owned()], Some(&home))
            .expect_err("a root containing the pin tree must be refused");
        assert!(e.contains("FINDING-098"), "names the escape: {e}");

        // Roots present but no HOME to judge against → fail CLOSED, not open.
        let e = validate_extra_read_roots(&[repo.to_string_lossy().into_owned()], None)
            .expect_err("no HOME must refuse the widening, never wave it through");
        assert!(e.contains("HOME"), "names the missing prerequisite: {e}");
    }
}

/// The DELIVERABLE FLOOR's resolution rules (FINDING-101, core#297 §3). The floor's WIRING — that
/// the runner-independent fold consults it and rejects on a miss — is proved in
/// `crate::actor::deliverable_floor_tests`; these cover the rules themselves.
#[cfg(test)]
mod deliverables_tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "wicked-deliv-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn no_declared_deliverables_is_always_satisfied() {
        let d = tmp("empty");
        assert!(missing_deliverables(&[], &d, &[]).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_declared_file_that_exists_passes_and_one_that_does_not_is_named() {
        let d = tmp("named");
        std::fs::write(d.join("coverage-report.json"), "{}").unwrap();
        assert!(missing_deliverables(&["coverage-report.json".into()], &d, &[]).is_none());
        let miss = missing_deliverables(
            &["coverage-report.json".into(), "domain-model.json".into()],
            &d,
            &[],
        )
        .expect("the absent deliverable must be reported");
        assert!(miss.contains("domain-model.json"), "{miss}");
        assert!(
            !miss.contains("coverage-report.json"),
            "the present one must not be named: {miss}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_declared_directory_of_outputs_counts_as_produced() {
        let d = tmp("dir");
        std::fs::create_dir_all(d.join(".wicked/domain")).unwrap();
        assert!(missing_deliverables(&[".wicked/domain".into()], &d, &[]).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A deliverable the engine cannot locate is not evidence the phase completed — with NO
    /// declared write roots, an absolute path or a `..` escape is reported missing, never
    /// silently skipped.
    #[test]
    fn an_unverifiable_deliverable_is_reported_missing_not_skipped() {
        let d = tmp("unverifiable");
        std::fs::write(d.join("real.json"), "{}").unwrap();
        assert!(missing_deliverables(&["/etc/passwd".into()], &d, &[]).is_some());
        assert!(missing_deliverables(&["../escape.json".into()], &d, &[]).is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// core#297 §3, the unbound-run shape: cwd is a throwaway sandbox, the deliverable is an
    /// ABSOLUTE path inside a declared write root. It resolves when written and is reported when
    /// not — the widening is a resolution rule, not an amnesty.
    #[test]
    fn an_absolute_deliverable_resolves_inside_a_declared_write_root() {
        let sandbox = tmp("sandbox");
        let inbox = tmp("inbox");
        let file = inbox.join("hand-off.md");

        assert!(
            missing_deliverables(&[s(&file)], &sandbox, &[s(&inbox)]).is_some(),
            "declared but never written is still missing"
        );
        std::fs::write(&file, "the answer").unwrap();
        assert!(
            missing_deliverables(&[s(&file)], &sandbox, &[s(&inbox)]).is_none(),
            "an absolute deliverable inside a DECLARED root is verifiable"
        );
        let _ = std::fs::remove_dir_all(&sandbox);
        let _ = std::fs::remove_dir_all(&inbox);
    }

    /// The containment clause is what keeps the widening honest: an existing file OUTSIDE every
    /// declared root stays missing. Without it a workflow could aim the floor at `/etc/hosts` and
    /// pass forever — and the run never had permission to create that file anyway.
    #[test]
    fn an_absolute_deliverable_outside_every_declared_root_stays_missing() {
        let sandbox = tmp("sandbox-out");
        let inbox = tmp("inbox-out");
        let elsewhere = tmp("elsewhere-out");
        let file = elsewhere.join("already-here.md");
        std::fs::write(&file, "not this run's work").unwrap();

        assert!(
            missing_deliverables(&[s(&file)], &sandbox, &[s(&inbox)]).is_some(),
            "an existing file outside every declared root is not this run's evidence"
        );
        // …and it is admitted the moment the launcher actually declares that root.
        assert!(
            missing_deliverables(&[s(&file)], &sandbox, &[s(&elsewhere)]).is_none(),
            "declaring the root is exactly what makes it verifiable"
        );
        let _ = std::fs::remove_dir_all(&sandbox);
        let _ = std::fs::remove_dir_all(&inbox);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    /// A RELATIVE deliverable searches the cwd first and the declared roots after, so a workflow
    /// can name `hand-off.md` once and have it resolve whether the run is bound to a repo or not.
    #[test]
    fn a_relative_deliverable_also_resolves_against_a_declared_write_root() {
        let sandbox = tmp("sandbox-rel");
        let inbox = tmp("inbox-rel");
        assert!(
            missing_deliverables(&["hand-off.md".into()], &sandbox, &[s(&inbox)]).is_some(),
            "absent from both the cwd and the declared root"
        );
        std::fs::write(inbox.join("hand-off.md"), "x").unwrap();
        assert!(
            missing_deliverables(&["hand-off.md".into()], &sandbox, &[s(&inbox)]).is_none(),
            "a relative deliverable resolves against the declared root too"
        );
        let _ = std::fs::remove_dir_all(&sandbox);
        let _ = std::fs::remove_dir_all(&inbox);
    }

    /// A `..` escape stays refused even WITH declared roots: its target depends on which base it
    /// is joined to, so the same declaration would name a different file per root. A floor whose
    /// subject is ambiguous is not a floor.
    #[test]
    fn a_parent_escaping_relative_deliverable_is_refused_even_with_declared_roots() {
        let sandbox = tmp("sandbox-esc");
        let inbox = tmp("inbox-esc");
        let sibling = inbox.parent().unwrap().join("escape.json");
        std::fs::write(&sibling, "{}").unwrap();
        assert!(
            missing_deliverables(&["../escape.json".into()], &sandbox, &[s(&inbox)]).is_some(),
            "a `..` escape is ambiguous by construction and must stay unverifiable"
        );
        let _ = std::fs::remove_file(&sibling);
        let _ = std::fs::remove_dir_all(&sandbox);
        let _ = std::fs::remove_dir_all(&inbox);
    }

    /// Whitespace-only entries are not declarations — they are formatting, and must not be
    /// reported as a missing artifact named "  ".
    #[test]
    fn a_blank_declaration_is_ignored() {
        let d = tmp("blank");
        assert!(missing_deliverables(&["   ".into(), "".into()], &d, &[]).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}
