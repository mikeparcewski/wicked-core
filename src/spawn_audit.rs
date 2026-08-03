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

    /// Source lines of `path`, truncated at the first `#[cfg(test)]`.
    ///
    /// Test code is exempt, and must be: the audit's own siblings deliberately spawn children with
    /// `WICKED_ESTATE_DB` set to prove the old name no longer resolves. Hardening those would delete
    /// the very condition under test. Truncating at the marker (rather than tracking brace depth)
    /// relies on `#[cfg(test)] mod tests` sitting at the end of the file, which is this codebase's
    /// invariable layout — and errs toward *under*-scanning a file, never toward a false pass on
    /// production code, since anything before the marker is still scanned.
    fn production_lines(path: &Path) -> Vec<String> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        text.lines()
            .take_while(|l| !l.trim_start().starts_with("#[cfg(test)]"))
            .map(str::to_string)
            .collect()
    }

    /// A `Command::new` occurrence that is real code rather than prose about code.
    ///
    /// Doc comments and line comments are excluded — several modules discuss `Command::new` in their
    /// documentation (this one included), and a guard that fails on its own explanation of itself
    /// gets deleted rather than satisfied.
    fn is_spawn_site(line: &str) -> bool {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            return false;
        }
        line.contains("Command::new")
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
        for file in &files {
            let lines = production_lines(file);
            for (i, line) in lines.iter().enumerate() {
                if !is_spawn_site(line) {
                    continue;
                }
                checked += 1;
                let end = (i + 1 + LOOKAHEAD).min(lines.len());
                if !lines[i..end].iter().any(|l| l.contains(".hardened()")) {
                    let rel = file.strip_prefix(&root).unwrap_or(file);
                    unhardened.push(format!("{}:{} — {}", rel.display(), i + 1, line.trim()));
                }
            }
        }

        assert!(
            checked > 15,
            "found only {checked} spawn sites — the detector stopped matching, which fails open"
        );
        assert!(
            unhardened.is_empty(),
            "\n{} spawn site(s) do not call .hardened() within {LOOKAHEAD} lines after them:\n\n{}\n\n\
             Every Command::new in this workspace must chain .hardened() — see \
             wicked_apps_core::spawn. It strips the engine's internal environment (the operational \
             store, the gate hook's store and argument channel) so a child cannot inherit state \
             belonging to the engine instead of state belonging to its own job. FINDING-067 is what \
             happens otherwise: a worker ran `wicked-estate index .`, inherited WICKED_ESTATE_DB, \
             and deleted all 833 nodes of the platform's operational state.\n\n\
             If this new site genuinely must receive one of those variables, harden FIRST and set it \
             explicitly afterwards. Do not skip the call — the rule has no allowlist on purpose.\n",
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
