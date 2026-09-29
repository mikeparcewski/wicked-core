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

/// [`resolve_symlinks`] for a ROOT DECLARATION (core#410 hardening, reviewer R11): a spelling with
/// a `.` or `..` component is REFUSED before any filesystem call — the rule `validate_chat_scope`
/// applies to a chat's paths — and only a spelled-clean root goes through the symlink walk. A `..`
/// beyond the existing ancestors, re-appended lexically, would be resolved by the kernel against the
/// real directory at every consumer (`<repo>/missing/../..` judged as inside the repo, resolved to
/// its grandparent); and no filesystem probe judges it the same way on every OS — Windows' path
/// layer collapses `..` BEFORE any lookup, so `<repo>\missing\..\..` "exists" as the grandparent
/// there. Spelling is the one platform-uniform test. [`check`] and [`deliverable_exists`] normalize
/// before resolving and are unchanged; a declared root is spelled by the caller.
fn resolve_symlinks_for_root(p: &Path) -> Result<PathBuf, String> {
    refuse_dot_components(p)?;
    Ok(resolve_symlinks(p))
}

/// The spelling rule shared by the root resolver and `acp_runner::canonical_ish`: any `.` or `..`
/// SEGMENT refuses the path — judged on the literal spelling, split at both separators, exactly as
/// `spawn::refuse_dot_segments` does. Not `Path::components()`: for a Windows verbatim path
/// (`\\?\C:\…`, which `canonicalize` produces) `components()` does NOT normalize, so a `..` there
/// is a plain `Normal("..")` and a component match misses the very segment the filesystem then
/// collapses (windows-latest, core#435).
pub(crate) fn refuse_dot_components(p: &Path) -> Result<(), String> {
    let spelled = p.as_os_str().to_string_lossy();
    if spelled
        .split(['/', '\\'])
        .any(|segment| segment == "." || segment == "..")
    {
        return Err(format!(
            "{} has a `.`/`..` segment and cannot be resolved safely on every platform; spell the \
             root plainly",
            p.display()
        ));
    }
    Ok(())
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

/// Does a tool call's RAW path (as the agent spelled it: relative, `~`-prefixed, `..`-bearing or
/// absolute) land inside `root`? The same normalize → symlink-resolve → containment chain
/// [`check`] runs, exposed for a judgement that is not "inside ANY root" but "inside THIS one" —
/// the creator write fence (F-4R2-004, `gate_hook::phase_scope_denial` /
/// `acp_runner::answer_permission_request`) asks whether a write that already passed the
/// filesystem boundary targets the tree under review (`cwd`) or one of the declared deliverable
/// roots. Sharing the chain is what keeps "inside" meaning one thing on both carriers and on
/// every OS (`/tmp`→`/private/tmp`, Windows verbatim prefixes).
pub(crate) fn raw_resolves_within(raw: &str, cwd: &Path, home: Option<&Path>, root: &Path) -> bool {
    let resolved = resolve_symlinks(&normalize(raw, cwd, home));
    resolved_is_within(&resolved, root)
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

/// The home directory the launch-time root validation judges against: `$HOME`, or `$USERPROFILE`
/// on Windows — the same two-step every other home lookup in the engine already does
/// (`code_graph`, `execute_wrapped`, `acp_runner`, `mcp_isolation`).
///
/// core#640: the two launch call sites read `$HOME` ALONE, so on native Windows — where `HOME` is
/// normally unset and `USERPROFILE` is the home — the `None` arm of [`validate_extra_roots`] fired
/// on every launch that declared `extra_write_roots` or `extra_read_roots`. The fail-closed refusal
/// was correct for a home that genuinely cannot be located and wrong for a host that simply spells
/// it differently: every Windows document, draft and demo run, which is exactly the set of runs
/// that declares extra roots, was refused at launch.
pub fn launch_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
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
        // No home directory ⇒ the pin tree cannot be located, so containment cannot be proven
        // either way. Fail CLOSED: refuse the widening rather than arm roots we cannot judge.
        //
        // The message names BOTH variables on purpose (core#640): callers resolve the home with
        // [`launch_home`], so on Windows — where `HOME` is normally unset and `USERPROFILE` is the
        // home — an operator told only to "set $HOME" would be chasing the wrong knob.
        None => {
            return Err(format!(
                "extra {kind} roots need a home directory ($HOME, or $USERPROFILE on Windows) to \
                 validate against the engine config tree; refusing to widen the boundary without it"
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
        // The wrapped carrier transports roots via std::env::join_paths; a root containing the
        // platform path-list separator makes that join fail AT SPAWN, silently degrading the
        // boundary mid-run. Refuse it here so the launch fails loudly on both carriers alike.
        if std::env::join_paths([p]).is_err() {
            return Err(format!(
                "extra {kind} root {raw} cannot ride the env path-list carrier (it contains a \
                 character join_paths refuses); refused at launch rather than degraded at spawn"
            ));
        }
        let resolved =
            resolve_symlinks_for_root(p).map_err(|e| format!("extra {kind} root {e}; refused"))?;
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
/// A deliverable counts as produced if it is a NON-EMPTY file or a directory holding at least one
/// entry (a phase may declare a directory of outputs). A zero-byte file or an empty directory is
/// reported as `<path> (empty)`: it is the "nothing was produced" this floor exists to catch
/// (DES-TEAMING-002 X3; crew's floor, which M9 deletes, refused it first). This is a SUBSTANCE
/// check, the opposite of the presence-shaped gates this campaign keeps filing: the phase said it
/// would produce X, so require X, not a status code claiming it did.
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
///
/// # Freshness: a prior run's leftover is not this run's evidence (core#640)
///
/// `launch_floor_ms` is the run's own launch clock in epoch millis — see
/// [`crate::event_log::run_started_ms`], the `ts` of the first record in the run's durable event
/// log. A deliverable that carries bytes but was last written BEFORE the run launched is reported
/// as not produced, with how far ahead of the launch it was written.
///
/// Presence alone was not enough. Every crew interactive seam (chat, draft, demo) declares a write
/// root keyed by DOCUMENT, not by run, so the same directory is handed to run after run: once one
/// run wrote `draft.html` there, every later run on that document passed this floor without
/// producing anything at all. crew's floor caught it
/// (`packages/crew/src/core/deliverable-floor.ts`, crew#320) and migration M9 deletes crew's, so
/// the engine's has to catch it first.
///
/// The floor is the run's FIRST-EVER event, so it is conservative by construction: output an
/// earlier unit of the SAME run produced is newer than it and stays produced, across a resume or a
/// redrive (which continue the same log). `None` — an embedder or a test whose event sink records
/// nowhere — keeps the presence-only judgement; the daemon always records.
pub(crate) fn missing_deliverables(
    declared: &[String],
    cwd: &Path,
    write_roots: &[String],
    launch_floor_ms: Option<i64>,
) -> Option<String> {
    let missing: Vec<String> = declared
        .iter()
        .filter(|d| !d.trim().is_empty())
        .filter_map(
            |d| match deliverable_state(d, cwd, write_roots, launch_floor_ms) {
                Found::Produced => None,
                Found::Absent => Some(d.clone()),
                Found::Empty => Some(format!("{d} (empty)")),
                Found::Stale(why) => Some(format!("{d} ({why})")),
            },
        )
        .collect();
    (!missing.is_empty()).then(|| missing.join(", "))
}

/// Slack on the freshness comparison, in millis — the same 1 s crew's floor allows
/// (`DELIVERABLE_FLOOR_MTIME_SLACK_MS`). The launch stamp and a file's mtime come from two
/// different clocks, and a coarse filesystem truncates mtimes to the second, so a file written in
/// the same second as the launch must not read as older than it.
const FRESHNESS_SLACK_MS: i64 = 1_000;

/// What one declared deliverable resolved to.
#[derive(PartialEq, Eq, Clone)]
enum Found {
    Produced,
    /// Present but hollow: a zero-byte file or an empty directory.
    Empty,
    /// Present and non-empty, but last written before this run launched (core#640): a PRIOR run's
    /// artifact. Carries the parenthetical the miss is reported with.
    Stale(String),
    Absent,
}

/// One deliverable's presence test — see [`missing_deliverables`] for the rules and why.
fn deliverable_state(
    declared: &str,
    cwd: &Path,
    write_roots: &[String],
    launch_floor_ms: Option<i64>,
) -> Found {
    let p = Path::new(declared);
    let candidates: Vec<PathBuf> = if p.is_absolute() {
        let resolved = resolve_symlinks(p);
        if write_roots
            .iter()
            .any(|r| resolved_is_within(&resolved, Path::new(r)))
        {
            vec![p.to_path_buf()]
        } else {
            Vec::new()
        }
    } else if p.components().any(|c| matches!(c, Component::ParentDir)) {
        Vec::new()
    } else {
        // Each candidate must stay inside the base it was joined to once symlinks resolve: a
        // link out of the root is not this run's evidence (the absolute branch's rule).
        std::iter::once(cwd)
            .chain(write_roots.iter().map(Path::new))
            .filter_map(|base| {
                let c = base.join(p);
                resolved_is_within(&resolve_symlinks(&c), base).then_some(c)
            })
            .collect()
    };
    let states: Vec<Found> = candidates
        .iter()
        .map(|c| content_state(c, launch_floor_ms))
        .collect();
    // Best evidence wins across the candidate bases, and a STALE candidate outranks an empty or
    // absent one: "it is there but it predates this run" is the more actionable miss.
    if states.contains(&Found::Produced) {
        Found::Produced
    } else if let Some(stale) = states.iter().find(|s| matches!(s, Found::Stale(_))) {
        stale.clone()
    } else if states.contains(&Found::Empty) {
        Found::Empty
    } else {
        Found::Absent
    }
}

/// A file counts when it carries bytes AND was written after the run launched; a directory when it
/// holds at least one such entry. `launch_floor_ms` is the launch clock — see
/// [`missing_deliverables`] for why a `None` there keeps the presence-only judgement.
fn content_state(path: &Path, launch_floor_ms: Option<i64>) -> Found {
    match std::fs::metadata(path) {
        Err(_) => Found::Absent,
        Ok(m) if m.is_dir() => {
            let entries: Vec<std::path::PathBuf> = match std::fs::read_dir(path) {
                Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
                Err(_) => return Found::Absent,
            };
            if entries.is_empty() {
                return Found::Empty;
            }
            let Some(floor) = launch_floor_ms else {
                return Found::Produced;
            };
            // The freshest entry decides: one file this run wrote makes the directory this run's
            // output, however much of a prior run's output sits beside it.
            let newest = entries.iter().filter_map(|e| written_at_ms(e)).max();
            match newest {
                Some(ts) if ts >= floor - FRESHNESS_SLACK_MS => Found::Produced,
                // Unreadable mtimes on every entry: the engine cannot date the directory, so it
                // cannot call it stale either.
                None => Found::Produced,
                Some(ts) => Found::Stale(format!(
                    "stale: {} entries, none written by this run — the newest predates the launch \
                     by {}",
                    entries.len(),
                    ago(floor - ts)
                )),
            }
        }
        Ok(m) if m.len() > 0 => match (launch_floor_ms, written_at_ms(path)) {
            (Some(floor), Some(ts)) if ts < floor - FRESHNESS_SLACK_MS => Found::Stale(format!(
                "stale: last written {} before this run launched — a PRIOR run's artifact",
                ago(floor - ts)
            )),
            _ => Found::Produced,
        },
        Ok(_) => Found::Empty,
    }
}

/// A path's mtime in epoch millis, or `None` when the filesystem will not report one (a clock
/// before the epoch included). Unreadable is never "stale": the engine says what it can prove.
fn written_at_ms(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

/// A millisecond gap in the coarsest unit that still reads as a number — the floor's message has
/// to be legible to an operator, and the engine carries no date formatter.
fn ago(ms: i64) -> String {
    let s = ms / 1_000;
    match s {
        _ if s >= 86_400 => format!("{}d", s / 86_400),
        _ if s >= 3_600 => format!("{}h", s / 3_600),
        _ if s >= 60 => format!("{}m", s / 60),
        _ => format!("{s}s"),
    }
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

        // A root the env carrier cannot transport would make join_paths fail AT SPAWN, silently
        // dropping the carrier's roots — refused at LAUNCH instead, on both mirrors. The char
        // join_paths rejects is ':' on Unix and '"' on Windows (';' rides quoted there).
        let sep = if cfg!(windows) { '"' } else { ':' };
        let poisoned = format!("{}{sep}sneaky", repo.to_string_lossy());
        let e = validate_extra_read_roots(std::slice::from_ref(&poisoned), Some(&home))
            .expect_err("a separator-poisoned read root must be refused");
        assert!(e.contains("carrier"), "names the failure: {e}");
        let e = validate_extra_write_roots(&[poisoned], Some(&home))
            .expect_err("a separator-poisoned write root must be refused");
        assert!(e.contains("carrier"), "names the failure: {e}");
    }

    /// core#410 hardening (reviewer R11): a root spelled with `..` is refused on both mirrors BY
    /// SPELLING — never re-appended lexically and judged as if it sat where it was spelled, and
    /// never left to a filesystem probe that Windows' path layer answers differently. A plain
    /// missing leaf (a directory the run will create) still resolves and is admitted.
    #[test]
    fn a_root_with_dot_segments_beyond_its_existing_ancestors_is_refused_not_re_appended() {
        let home = scratch("xrr_dots_home");
        let repo = scratch("xrr_dots_repo");
        std::fs::create_dir_all(repo.join("sub")).unwrap();

        // The platform-independent property: a `..` spelling is NEVER accepted. On unix the
        // spelling rule names the `..`; on Windows the path layer may collapse a `..` before any
        // code of ours sees it — `scratch()` hands back a verbatim `\\?\` path there — in which case
        // the root reaches the containment check as the grandparent (the temp dir, which contains
        // `home`) and is refused for THAT. Either refusal is correct; acceptance is the bug.
        let refused = |name: &str, e: &str| {
            assert!(e.contains("refused"), "{name}: {e}");
            #[cfg(unix)]
            assert!(e.contains("`..`"), "{name}: {e}");
            #[cfg(windows)]
            assert!(
                e.contains("`..`") || e.contains("would expose"),
                "{name}: {e}"
            );
        };
        // `<repo>/missing/../..`, spelled as a RAW string through the production entry points:
        // lexically "inside the repo", really its grandparent.
        let sep = std::path::MAIN_SEPARATOR;
        let sneaky = format!("{}{sep}missing{sep}..{sep}..", repo.display());
        for (name, res) in [
            (
                "read",
                validate_extra_read_roots(std::slice::from_ref(&sneaky), Some(&home)),
            ),
            (
                "write",
                validate_extra_write_roots(std::slice::from_ref(&sneaky), Some(&home)),
            ),
        ] {
            let e = res.expect_err("a dot segment beyond the existing ancestors must be refused");
            refused(name, &e);
        }
        // A `..` in the MIDDLE of the missing tail is the same refusal.
        let mid = format!("{}{sep}missing{sep}..{sep}leaf", repo.display());
        let e = validate_extra_read_roots(std::slice::from_ref(&mid), Some(&home))
            .expect_err("a `..` beyond the existing ancestors must be refused");
        refused("mid", &e);
        // A plain missing leaf still resolves: the run creates it.
        let leaf = repo.join("not-yet").join("created");
        validate_extra_read_roots(&[leaf.to_string_lossy().into_owned()], Some(&home))
            .expect("a plain missing leaf is admitted");
        // A `..` under an EXISTING path is refused just the same — the rule is spelling.
        let real_dots = format!("{}{sep}sub{sep}..", repo.display());
        let e = validate_extra_read_roots(std::slice::from_ref(&real_dots), Some(&home))
            .expect_err("a `..` is refused by spelling even under existing directories");
        refused("real_dots", &e);
        // A spelled-clean root that RESOLVES into the home is still judged on the real target.
        #[cfg(unix)]
        {
            std::fs::create_dir_all(home.join("x")).unwrap();
            let via_link = repo.join("to-home");
            std::os::unix::fs::symlink(&home, &via_link).unwrap();
            let e =
                validate_extra_read_roots(&[via_link.to_string_lossy().into_owned()], Some(&home))
                    .expect_err("resolved into the home: refused as containing the pin tree");
            assert!(e.contains("FINDING-098"), "{e}");
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// core#640, second half. The launch path resolved the home from `$HOME` alone, so on native
    /// Windows — where `HOME` is normally unset and `USERPROFILE` IS the home — the fail-closed
    /// `None` arm fired on every launch that declared extra roots: every document, draft and demo
    /// run on that host, refused for a home that was sitting right there under another name.
    ///
    /// The refusal itself still stands when NEITHER resolves, and its message now names both
    /// variables so the operator is not sent after the wrong knob.
    #[test]
    fn the_launch_home_is_userprofile_when_home_is_unset() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let prior_home = std::env::var_os("HOME");
        let prior_profile = std::env::var_os("USERPROFILE");
        let restore = || {
            match &prior_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match &prior_profile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        };

        std::env::remove_var("HOME");
        std::env::set_var("USERPROFILE", "/windows/home");
        assert_eq!(
            launch_home(),
            Some(PathBuf::from("/windows/home")),
            "USERPROFILE is the home on Windows"
        );

        // HOME wins where both are set — the POSIX spelling stays authoritative.
        std::env::set_var("HOME", "/posix/home");
        assert_eq!(launch_home(), Some(PathBuf::from("/posix/home")));

        // An EMPTY value is not a home: joining `.config/wicked-core` onto it would judge every
        // root against a relative path.
        std::env::set_var("HOME", "");
        std::env::set_var("USERPROFILE", "");
        assert_eq!(launch_home(), None);

        // Neither resolves ⇒ still refused, and the message names both variables.
        std::env::remove_var("HOME");
        std::env::remove_var("USERPROFILE");
        assert_eq!(launch_home(), None);
        let e = validate_extra_write_roots(&["/tmp/inbox".into()], launch_home().as_deref())
            .expect_err("a home that cannot be located is still a fail-closed refusal");
        assert!(e.contains("$HOME") && e.contains("$USERPROFILE"), "{e}");
        restore();
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
        assert!(missing_deliverables(&[], &d, &[], None).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_declared_file_that_exists_passes_and_one_that_does_not_is_named() {
        let d = tmp("named");
        std::fs::write(d.join("coverage-report.json"), "{}").unwrap();
        assert!(missing_deliverables(&["coverage-report.json".into()], &d, &[], None).is_none());
        let miss = missing_deliverables(
            &["coverage-report.json".into(), "domain-model.json".into()],
            &d,
            &[],
            None,
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
        std::fs::write(d.join(".wicked/domain/model.json"), "{}").unwrap();
        assert!(missing_deliverables(&[".wicked/domain".into()], &d, &[], None).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// (DES-TEAMING-002 X3) A ZERO-BYTE file or an EMPTY directory is not a produced deliverable:
    /// it is the same "nothing was produced" the floor exists to catch, and crew's own floor
    /// (`packages/crew/src/core/deliverable-floor.ts`) already refuses both — so the engine must,
    /// before migration M9 deletes crew's. The miss says why, so the operator never guesses.
    #[test]
    fn an_empty_file_or_directory_is_not_a_produced_deliverable() {
        let d = tmp("hollow");
        std::fs::write(d.join("draft.html"), "").unwrap();
        std::fs::create_dir_all(d.join("out")).unwrap();
        let miss = missing_deliverables(&["draft.html".into(), "out".into()], &d, &[], None)
            .expect("an empty file and an empty directory are missing");
        assert!(miss.contains("draft.html (empty)"), "{miss}");
        assert!(miss.contains("out (empty)"), "{miss}");
        // Inside a declared write root, absolute, the same.
        let sandbox = tmp("hollow-sandbox");
        let abs = s(&d.join("draft.html"));
        assert!(
            missing_deliverables(std::slice::from_ref(&abs), &sandbox, &[s(&d)], None).is_some()
        );
        std::fs::write(d.join("draft.html"), "<p>x</p>").unwrap();
        std::fs::write(d.join("out/fragment-1.html"), "<p>y</p>").unwrap();
        assert!(
            missing_deliverables(&["draft.html".into(), "out".into()], &d, &[], None).is_none()
        );
        assert!(missing_deliverables(&[abs], &sandbox, &[s(&d)], None).is_none());
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&sandbox);
    }

    /// A deliverable the engine cannot locate is not evidence the phase completed — with NO
    /// declared write roots, an absolute path or a `..` escape is reported missing, never
    /// silently skipped.
    #[test]
    fn an_unverifiable_deliverable_is_reported_missing_not_skipped() {
        let d = tmp("unverifiable");
        std::fs::write(d.join("real.json"), "{}").unwrap();
        assert!(missing_deliverables(&["/etc/passwd".into()], &d, &[], None).is_some());
        assert!(missing_deliverables(&["../escape.json".into()], &d, &[], None).is_some());
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
            missing_deliverables(&[s(&file)], &sandbox, &[s(&inbox)], None).is_some(),
            "declared but never written is still missing"
        );
        std::fs::write(&file, "the answer").unwrap();
        assert!(
            missing_deliverables(&[s(&file)], &sandbox, &[s(&inbox)], None).is_none(),
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
            missing_deliverables(&[s(&file)], &sandbox, &[s(&inbox)], None).is_some(),
            "an existing file outside every declared root is not this run's evidence"
        );
        // …and it is admitted the moment the launcher actually declares that root.
        assert!(
            missing_deliverables(&[s(&file)], &sandbox, &[s(&elsewhere)], None).is_none(),
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
            missing_deliverables(&["hand-off.md".into()], &sandbox, &[s(&inbox)], None).is_some(),
            "absent from both the cwd and the declared root"
        );
        std::fs::write(inbox.join("hand-off.md"), "x").unwrap();
        assert!(
            missing_deliverables(&["hand-off.md".into()], &sandbox, &[s(&inbox)], None).is_none(),
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
            missing_deliverables(&["../escape.json".into()], &sandbox, &[s(&inbox)], None)
                .is_some(),
            "a `..` escape is ambiguous by construction and must stay unverifiable"
        );
        let _ = std::fs::remove_file(&sibling);
        let _ = std::fs::remove_dir_all(&sandbox);
        let _ = std::fs::remove_dir_all(&inbox);
    }

    /// (codex on #636) A relative deliverable that is a SYMLINK out of the base it resolved
    /// against is not this run's evidence: a worker can `ln -s /etc/hosts draft.html` from a
    /// shell, and following it would count bytes the run never wrote. The same containment rule
    /// the absolute branch applies. A symlink that stays inside its base still counts.
    #[cfg(unix)]
    #[test]
    fn a_relative_deliverable_symlinked_out_of_its_base_is_missing() {
        let inbox = tmp("link-inbox");
        let sandbox = tmp("link-sandbox");
        let outside = tmp("link-outside");
        std::fs::write(outside.join("hosts"), "127.0.0.1 localhost").unwrap();
        std::os::unix::fs::symlink(outside.join("hosts"), inbox.join("draft.html")).unwrap();
        assert!(
            missing_deliverables(&["draft.html".into()], &sandbox, &[s(&inbox)], None).is_some(),
            "a symlink to bytes outside the declared root is not a produced deliverable"
        );
        std::fs::write(inbox.join("real.html"), "<p>x</p>").unwrap();
        std::os::unix::fs::symlink(inbox.join("real.html"), inbox.join("alias.html")).unwrap();
        assert!(
            missing_deliverables(&["alias.html".into()], &sandbox, &[s(&inbox)], None).is_none(),
            "a symlink that stays inside the root still counts"
        );
        let _ = std::fs::remove_dir_all(&inbox);
        let _ = std::fs::remove_dir_all(&sandbox);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// Whitespace-only entries are not declarations — they are formatting, and must not be
    /// reported as a missing artifact named "  ".
    #[test]
    fn a_blank_declaration_is_ignored() {
        let d = tmp("blank");
        assert!(missing_deliverables(&["   ".into(), "".into()], &d, &[], None).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A file's mtime in epoch millis — the value the floor dates a deliverable by.
    fn mtime_ms(p: &Path) -> i64 {
        written_at_ms(p).expect("the test filesystem reports an mtime")
    }

    /// core#640, the defect this batch exists for. The engine floor judged PRESENCE only, so a
    /// file a PRIOR run left in a write root passed for every later run handed the same root —
    /// which is every chat, draft and demo run, because crew keys that root by DOCUMENT and not
    /// by run. The miss now says the file predates the launch, and by how much, so the operator
    /// reads "a prior run's artifact" instead of "produced".
    ///
    /// Deterministic without sleeping or rewriting mtimes: the file is written, then judged
    /// against a launch floor an hour AFTER it — the same ordering as a stale leftover, with no
    /// clock manipulation.
    #[test]
    fn a_deliverable_older_than_the_launch_is_not_produced() {
        let sandbox = tmp("stale-sandbox");
        let inbox = tmp("stale-inbox");
        let prior = inbox.join("draft.html");
        std::fs::write(&prior, "<p>a PRIOR run's draft</p>").unwrap();
        let written = mtime_ms(&prior);

        // Launched an hour after that file was written: a leftover, not this run's output.
        let miss = missing_deliverables(
            &["draft.html".into()],
            &sandbox,
            &[s(&inbox)],
            Some(written + 3_600_000),
        )
        .expect("a file written before the launch is not this run's deliverable");
        assert!(miss.contains("draft.html"), "{miss}");
        assert!(miss.contains("stale"), "the miss must say WHY: {miss}");
        assert!(
            miss.contains("1h"),
            "the miss must say how far ahead of the launch it was written: {miss}"
        );

        // Written after the launch: produced, the ordinary case.
        assert!(
            missing_deliverables(
                &["draft.html".into()],
                &sandbox,
                &[s(&inbox)],
                Some(written - 60_000),
            )
            .is_none(),
            "a file written after the launch is this run's deliverable"
        );

        // Inside the slack window (the launch stamp and the filesystem are two clocks, and a
        // coarse filesystem truncates to the second) it still counts.
        assert!(
            missing_deliverables(
                &["draft.html".into()],
                &sandbox,
                &[s(&inbox)],
                Some(written + FRESHNESS_SLACK_MS),
            )
            .is_none(),
            "a file written in the same second as the launch is not stale"
        );

        // No launch clock (a sink that records nowhere) ⇒ the presence-only judgement, unchanged.
        assert!(
            missing_deliverables(&["draft.html".into()], &sandbox, &[s(&inbox)], None).is_none()
        );
        let _ = std::fs::remove_dir_all(&sandbox);
        let _ = std::fs::remove_dir_all(&inbox);
    }

    /// The directory arm of the same rule. A declared directory of outputs is produced when ONE
    /// entry is this run's — a run that appends a fragment beside a prior run's is still producing
    /// — and stale when every entry predates the launch.
    #[test]
    fn a_directory_whose_every_entry_predates_the_launch_is_stale() {
        let d = tmp("stale-dir");
        std::fs::create_dir_all(d.join("out")).unwrap();
        let old = d.join("out/fragment-1.html");
        std::fs::write(&old, "<p>prior</p>").unwrap();
        let written = mtime_ms(&old);
        let floor = written + 3_600_000;

        let miss = missing_deliverables(&["out".into()], &d, &[], Some(floor))
            .expect("a directory holding only a prior run's output produced nothing");
        assert!(miss.contains("out (stale"), "{miss}");
        assert!(
            miss.contains("1 entries"),
            "the miss must say what IS there: {miss}"
        );

        // One entry from this run makes the directory this run's output.
        let fresh = d.join("out/fragment-2.html");
        std::fs::write(&fresh, "<p>this run</p>").unwrap();
        let floor = mtime_ms(&fresh) - 60_000;
        assert!(missing_deliverables(&["out".into()], &d, &[], Some(floor)).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// An EMPTY file stays "(empty)" rather than becoming "(stale)": zero bytes is the sharper
    /// statement about what the phase produced, and the empty test runs before the date test.
    #[test]
    fn an_empty_file_is_reported_empty_not_stale() {
        let d = tmp("empty-not-stale");
        let f = d.join("draft.html");
        std::fs::write(&f, "").unwrap();
        let miss = missing_deliverables(
            &["draft.html".into()],
            &d,
            &[],
            Some(mtime_ms(&f) + 3_600_000),
        )
        .expect("a zero-byte file is not produced");
        assert!(miss.contains("draft.html (empty)"), "{miss}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
