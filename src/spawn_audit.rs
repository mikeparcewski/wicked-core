//! Mechanical enforcement of the one-chokepoint spawn rule.
//!
//! [`wicked_apps_core::spawn`] states the rule: every `Command::new` in this workspace is followed by
//! `.hardened()`. This module is what makes that statement load-bearing instead of aspirational.
//!
//! # Why a source scan and not a type
//!
//! The obvious alternative is to make the safe thing the only thing — a newtype wrapping `Command`
//! whose constructor hardens, with `std::process::Command` unreachable. That is better where it can
//! be done, and it cannot be done here: `Command` is used across two crates with different builder
//! shapes (`.output()`, `.spawn()`, `.status()`, `process_group`, `pre_exec`), and a wrapper thin
//! enough to cover them all would just be `Command` with a different name — trivially bypassed by
//! `use std::process::Command` in a new file.
//!
//! The failure being defended against is not "someone chose the unsafe API on purpose". It is
//! "someone added a spawn site and nobody thought about environment inheritance". A scan of what is
//! actually written in the tree catches that, and catches it in a *new file the author forgot to
//! wire into anything* — which a type-level guard, by definition, cannot.
//!
//! # Every line is scanned; test sites are exempted one at a time
//!
//! Test code genuinely must spawn unhardened — several sibling tests set `WICKED_ESTATE_DB` on a
//! child on purpose, to prove the old name no longer resolves. An earlier version of this audit
//! handled that by skipping everything after a file's first `#[cfg(test)]`, and that was a fail-open:
//! `#[cfg(test)]` also decorates individual test-only helpers *mid-file*, so in `src/validator.rs`
//! and `wicked-council/src/dispatch.rs` the skip began ~900 and ~600 lines early. The audit saw 20 of
//! the workspace's 36 spawn sites and reported success — a guard covering 55% of the tree while
//! claiming to cover it.
//!
//! Any region-detection scheme has that failure mode, so there is none. Every line of every file is
//! scanned, and a site is exempt only when [`tests::EXEMPT_MARKER`] appears in its window. That makes
//! each exemption a deliberate, greppable act rather than a side effect of where an attribute
//! happened to sit — the same argument [`wicked_apps_core::spawn`] makes for having no allowlist.
//! The marker carries no authority of its own: it is honoured wherever it is written, and writing one
//! on production code is a reviewable lie, not a bypass the tooling blesses.
//!
//! # What it does not prove
//!
//! Textual presence of `.hardened()`, not correct ordering. A site that hardens and then re-sets
//! `WICKED_ESTATE_DB` passes this audit. That is deliberate scope: this test answers "was the
//! chokepoint considered at every spawn site", and behavioural proof of the strip lives in
//! `execute_wrapped::tests::no_worker_inherits_an_estate_store_through_the_environment`, which runs a
//! real child and reads what it saw. Guard for coverage, behaviour test for correctness.

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// Lines AFTER a `Command::new` that may carry its `.hardened()`. The spawn line itself is
    /// always scanned too (the single-line `Command::new(x).hardened()` shape), so the window is
    /// `LOOKAHEAD + 1` lines wide.
    ///
    /// Generous on purpose. Builder chains here run long (args, stdio, `current_dir`, cfg-gated
    /// `process_group`), and the cost of being too tight is a false failure that teaches the next
    /// person to treat this test as noise and add an exemption. The cost of being too loose is
    /// pairing a spawn with a *neighbouring* spawn's hardening — visible in review, and both sites
    /// still had to be hardened for the file to pass at all.
    const LOOKAHEAD: usize = 30;

    /// The one way to exempt a spawn site, written as a comment in its window.
    ///
    /// Spelled `test-only` because that is the only justification that has ever been valid: a test
    /// asserting on what a child inherits cannot have the thing under test stripped out from under
    /// it. Production code has no exemption — if a path must pass one of these variables, it hardens
    /// first and sets the variable afterwards (see [`wicked_apps_core::spawn`]'s ordering contract),
    /// which satisfies this audit without a marker.
    const EXEMPT_MARKER: &str = "spawn-audit: test-only";

    /// The workspace's true spawn-site count, floored rather than pinned.
    ///
    /// The count only ever grows, and pinning it exactly would make every new spawn site edit this
    /// number — a test you must edit to make green is a test you edit without reading. The floor
    /// exists for one failure: the detector silently stopping matching (a rename, a formatter change,
    /// a walk that misses a directory), which would otherwise show up as a passing test with nothing
    /// behind it. Raise it when the real count moves well clear.
    const MIN_EXPECTED_SITES: usize = 30;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// Every `.rs` file under a directory, recursively. `target/` is skipped: generated code is not
    /// ours to harden, and scanning it makes the test's runtime depend on build state.
    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if name != "target" && !name.starts_with('.') {
                    rust_files(&path, out);
                }
            } else if name.ends_with(".rs") {
                out.push(path);
            }
        }
    }

    fn is_comment(line: &str) -> bool {
        line.trim_start().starts_with("//")
    }

    /// A `Command::new` occurrence that is real code rather than prose about code.
    ///
    /// Doc comments and line comments are excluded — several modules discuss `Command::new` in their
    /// documentation (this one included), and a guard that fails on its own explanation of itself
    /// gets deleted rather than satisfied.
    fn is_spawn_site(line: &str) -> bool {
        !is_comment(line) && line.contains("Command::new")
    }

    /// Whether a window carries a real hardening call.
    ///
    /// Comment lines do not count. `// cmd.hardened();` is what a commented-out call looks like, and
    /// reading that as "hardened" would let the guard bless the exact edit that removes the
    /// protection.
    fn is_hardened(window: &[String]) -> bool {
        window
            .iter()
            .any(|l| !is_comment(l) && l.contains(".hardened()"))
    }

    /// Whether the spawn site at `i` carries an exemption marker.
    ///
    /// The marker must be on the spawn line itself or in the contiguous comment block directly above
    /// it — the block a reader would take as belonging to this site. Anchoring it that way (rather
    /// than "somewhere within N lines") means a marker can never bleed onto a neighbouring spawn
    /// site: any line of code between the two ends the block.
    fn is_exempt(lines: &[String], i: usize) -> bool {
        if lines[i].contains(EXEMPT_MARKER) {
            return true;
        }
        lines[..i]
            .iter()
            .rev()
            .take_while(|l| is_comment(l) || l.trim().is_empty())
            .any(|l| l.contains(EXEMPT_MARKER))
    }

    #[test]
    fn every_spawn_site_passes_through_the_hardening_chokepoint() {
        let root = workspace_root();
        let mut files = Vec::new();
        rust_files(&root.join("src"), &mut files);
        rust_files(&root.join("crates"), &mut files);
        assert!(
            files.len() > 20,
            "scanned only {} files from {} — the walk is broken, and a guard that scans nothing \
             passes for the wrong reason",
            files.len(),
            root.display()
        );

        let mut unhardened = Vec::new();
        let mut checked = 0usize;
        let mut exempted = 0usize;
        for file in &files {
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            let lines: Vec<String> = text.lines().map(str::to_string).collect();
            for (i, line) in lines.iter().enumerate() {
                if !is_spawn_site(line) {
                    continue;
                }
                checked += 1;
                let window = &lines[i..(i + 1 + LOOKAHEAD).min(lines.len())];
                if is_exempt(&lines, i) {
                    exempted += 1;
                } else if !is_hardened(window) {
                    let rel = file.strip_prefix(&root).unwrap_or(file);
                    unhardened.push(format!("{}:{} — {}", rel.display(), i + 1, line.trim()));
                }
            }
        }

        assert!(
            checked >= MIN_EXPECTED_SITES,
            "found only {checked} spawn sites, expected at least {MIN_EXPECTED_SITES} — the \
             detector stopped matching, which fails open"
        );
        assert!(
            unhardened.is_empty(),
            "\n{} spawn site(s) neither call .hardened() within {LOOKAHEAD} lines nor carry a \
             `{EXEMPT_MARKER}` marker:\n\n{}\n\n\
             ({checked} sites scanned, {exempted} exempt.)\n\n\
             Every Command::new in this workspace must chain .hardened() — see \
             wicked_apps_core::spawn. It strips the engine's internal environment (the operational \
             store, the gate hook's store and argument channel) so a child cannot inherit state \
             belonging to the engine instead of state belonging to its own job. FINDING-067 is what \
             happens otherwise: a worker ran `wicked-estate index .`, inherited WICKED_ESTATE_DB, \
             and deleted all 833 nodes of the platform's operational state.\n\n\
             If this new site genuinely must receive one of those variables, harden FIRST and set it \
             explicitly afterwards. Do not skip the call — the rule has no allowlist on purpose. If \
             it is a TEST that must observe what a child inherits, add a `{EXEMPT_MARKER}` comment \
             saying why.\n",
            unhardened.len(),
            unhardened.join("\n")
        );
    }

    /// The audit above is textual, so it cannot notice if `ENGINE_INTERNAL_ENV` drifts from the
    /// consts the engine actually sets. This closes that: the list is spelled in `wicked-apps-core`
    /// (which sits below `gate_hook` in the dependency graph and cannot import it), so the two
    /// spellings are pinned against each other here, in the one crate that can see both.
    ///
    /// Without this, renaming a const would leave the stripper removing a variable nothing sets any
    /// more while the new name flowed straight through — hardening that reads as present and does
    /// nothing, which is worse than none, because it stops anyone from looking.
    #[test]
    fn the_stripped_list_covers_every_variable_the_engine_actually_sets() {
        use wicked_apps_core::spawn::ENGINE_INTERNAL_ENV;

        for name in [
            crate::gate_hook::ESTATE_DB_ENV,
            crate::gate_hook::GATE_DB_ENV,
            crate::gate_hook::DECISIONS_PATH_ENV,
            crate::gate_hook::GATE_SCOPE_ENV,
            crate::gate_hook::GATE_PHASE_ENV,
            crate::gate_hook::GATE_PHASE_ID_ENV,
        ] {
            assert!(
                ENGINE_INTERNAL_ENV.contains(&name),
                "`{name}` is set by the engine but absent from ENGINE_INTERNAL_ENV, so children \
                 inherit it — add it to wicked_apps_core::spawn"
            );
        }
    }
}
