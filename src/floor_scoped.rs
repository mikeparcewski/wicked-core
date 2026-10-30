//! (core#1198) Failure-scoped retry and base comparison.
//!
//! When a test check fails, the floor re-runs it (the identical retry under load, DES-FLOOR-V2 §4)
//! and compares it with the run base. Both used to run the WHOLE check. Rig run ea73afc0 (a crew
//! change to `routes.ts`) paid three full suites per hand-off floor (head 12 min, retry 11 min,
//! base 9 min) for 17–21 failing files out of 409.
//!
//! A repository that declares HOW to re-run a set of its tests in `.wicked/checks.json`
//!
//! ```json
//! "rerun": {
//!   "test":          { "command": ["npm", "run", "test", "--", "{files}"], "by": "file" },
//!   "test_targeted": { "command": ["node", "run.mjs", "--tests", "{files}"], "by": "file",
//!                      "self_retry": true },
//!   "cargo-test":    { "command": ["cargo", "test", "--", "{names}", "--exact"], "by": "name" }
//! }
//! ```
//!
//! gets the retry and the base run of ONLY the head's failing set (its parsed `failureIds`). The
//! exit codes still decide; the head's text only chooses what is re-run. Each scoped result has to
//! be one the whole check would have given, or the whole check runs instead, as before:
//!
//! * **A scoped run that FAILS stands.** The failure reproduced: the retry denies, the base is red.
//! * **A scoped RETRY that passes stands only when** the head's own failure count matches the
//!   selection ([`head_complete`]: nothing failed that the ids do not name, such as an unhandled
//!   error or a crashed test binary) **and** the retry shows each selected test ran and passed
//!   ([`proven`]). Otherwise the whole identical retry runs and decides.
//! * **A scoped BASE run that passes stands only when** every selected test the base has is shown
//!   to have run and passed there. A selected file the base does not have was added by the change,
//!   so the base cannot fail it, and the whole base would not have run it either. Otherwise the
//!   whole base runs and decides.
//!
//! The base answers "do THESE failures happen without the change?". A base that is red only on
//! tests the head passes does not make the head's own failures inseparable: they are compared
//! test by test.
//!
//! `self_retry: true` says the check already re-runs its own failures (crew's
//! `scripts/test-related.mjs`, crew#649). The engine then skips its own retry and goes straight to
//! the base comparison. A check with no `rerun` entry runs as before.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{resolve, CheckCommand, Resolved, RunScope, CONFIG_PATH, MAX_FAILURE_IDS};

/// The test checks a `rerun` entry may name.
pub(crate) const RERUN_CHECKS: &[&str] = &["test", "test_targeted", "cargo-test"];
/// The placeholder a `by: "file"` command carries, as one argv element.
pub(crate) const FILES_PLACEHOLDER: &str = "{files}";
/// The placeholder a `by: "name"` command carries, as one argv element.
pub(crate) const NAMES_PLACEHOLDER: &str = "{names}";

/// One `rerun` entry of [`CONFIG_PATH`] as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RerunEntry {
    #[serde(default)]
    command: Option<CheckCommand>,
    #[serde(default)]
    by: Option<String>,
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    self_retry: bool,
}

/// How a failing set is spelled to the re-run command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RerunBy {
    /// Test files (`FAIL tests/x.test.ts > …`, `FAILED tests/x.py::t`), relative to `root`.
    File,
    /// libtest names (`test a::b ... FAILED`), run with `--exact`.
    Name,
}

impl RerunBy {
    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            RerunBy::File => "file",
            RerunBy::Name => "name",
        }
    }
}

/// A validated `rerun` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RerunDecl {
    /// The failing-set command with its placeholder, and how ids are spelled. `None`: the entry
    /// only says `self_retry`.
    pub(crate) command: Option<(Vec<String>, RerunBy)>,
    /// (`by: "file"`) The directory the file ids are relative to, relative to the check's own
    /// directory (`packages/crew` for crew: vitest names files from its root). Empty: the check's
    /// directory.
    pub(crate) root: String,
    /// The check re-runs its own failures: the engine does not retry it again.
    pub(crate) self_retry: bool,
}

/// A repo-relative directory: `/`-separated, no empty, `.` or `..` segment, no leading `/`.
fn is_rel_dir(p: &str) -> bool {
    !p.is_empty()
        && p == p.trim()
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.split('/').any(|s| s.is_empty() || s == "." || s == "..")
}

/// Validate the `rerun` map: a typo must never silently mean "no declaration".
pub(super) fn rerun_of(
    raw: &BTreeMap<String, RerunEntry>,
) -> Result<BTreeMap<String, RerunDecl>, String> {
    let mut out = BTreeMap::new();
    for (key, e) in raw {
        let at = format!("`{CONFIG_PATH}` `rerun.{key}`");
        if !RERUN_CHECKS.contains(&key.as_str()) {
            return Err(format!(
                "{at}: not a test check (one of {})",
                RERUN_CHECKS.join(", ")
            ));
        }
        let command = match &e.command {
            None => {
                if e.by.is_some() || e.root.is_some() {
                    return Err(format!("{at}: `by` and `root` need a `command`"));
                }
                if !e.self_retry {
                    return Err(format!(
                        "{at}: declares nothing (give a `command` and `by`, or `self_retry: true`)"
                    ));
                }
                None
            }
            Some(cmd) => {
                let argv = match resolve(&Some(cmd.clone()), &format!("rerun.{key}.command"))? {
                    Resolved::Command(argv) => argv,
                    _ => {
                        return Err(format!(
                            "{at}: `command` must be a command (an argv array or a string)"
                        ))
                    }
                };
                let by = match e.by.as_deref() {
                    Some("file") => RerunBy::File,
                    Some("name") => RerunBy::Name,
                    Some(other) => {
                        return Err(format!("{at}: `by` is `file` or `name`, not `{other}`"))
                    }
                    None => return Err(format!("{at}: a `command` needs `by` (`file` or `name`)")),
                };
                let want = match by {
                    RerunBy::File => FILES_PLACEHOLDER,
                    RerunBy::Name => NAMES_PLACEHOLDER,
                };
                let standalone = argv.iter().filter(|a| *a == want).count();
                let embedded = argv.iter().any(|a| {
                    a != want && (a.contains(FILES_PLACEHOLDER) || a.contains(NAMES_PLACEHOLDER))
                });
                if standalone != 1 || embedded {
                    return Err(format!(
                        "{at}: `command` must carry `{want}` exactly once, as its own element \
                         (and no other placeholder)"
                    ));
                }
                Some((argv, by))
            }
        };
        let root = match (&e.root, &command) {
            (None, _) => String::new(),
            (Some(r), Some((_, RerunBy::File))) if is_rel_dir(r) => r.clone(),
            (Some(r), Some((_, RerunBy::File))) => {
                return Err(format!(
                    "{at}: `root` `{r}` is not a relative directory (`/`-separated, no empty, \
                     `.` or `..` segment)"
                ))
            }
            (Some(_), _) => return Err(format!("{at}: `root` applies to `by: \"file\"` only")),
        };
        out.insert(
            key.clone(),
            RerunDecl {
                command,
                root,
                self_retry: e.self_retry,
            },
        );
    }
    Ok(out)
}

/// An id that can be handed to a command as one argument: no flag shape, no whitespace, no
/// placeholder, no control character.
fn plain_arg(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.contains('{')
        && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// A test file named by a failure id, relative to the runner's root.
fn file_of(id: &str) -> Option<&str> {
    // vitest / jest-file: `FAIL tests/x.test.ts > suite > name`, `FAIL tests/x.test.ts [ … ]`.
    // pytest: `FAILED tests/x.py::test_y`.
    // The path ends at the first space (` > suite > name`, ` [ file ]`).
    let path = match id.strip_prefix("FAIL ") {
        Some(rest) => rest.trim().split(' ').next().unwrap_or(""),
        None => id
            .strip_prefix("FAILED ")?
            .trim()
            .split("::")
            .next()
            .unwrap_or(""),
    };
    // `|project|` is a vitest workspace project's label, not a path; `*` and `?` would be patterns.
    let shaped = !path.contains(['|', '*', '?']);
    (shaped && plain_arg(path) && is_rel_dir(path)).then_some(path)
}

/// A libtest name named by a failure id (`test a::b`). A doc test (`src/lib.rs - f (line 3)`)
/// cannot be selected by `--exact`: it is not scopeable.
fn name_of(id: &str) -> Option<&str> {
    let name = id.strip_prefix("test ")?.trim();
    plain_arg(name).then_some(name)
}

/// The failing set to re-run, from the head's parsed failure ids, deduplicated in order — or why
/// the whole check runs instead.
pub(crate) fn select(by: RerunBy, failure_ids: &[String]) -> Result<Vec<String>, String> {
    if failure_ids.is_empty() {
        return Err("the head's output names no failing test".into());
    }
    if failure_ids.len() >= MAX_FAILURE_IDS {
        return Err(format!(
            "the head named {} failures, the cap, so the list may be cut",
            failure_ids.len()
        ));
    }
    let mut out: Vec<String> = Vec::new();
    for id in failure_ids {
        let sel = match by {
            RerunBy::File => file_of(id),
            RerunBy::Name => name_of(id),
        };
        let Some(sel) = sel else {
            return Err(format!(
                "`{id}` names no test {} the re-run command can select",
                match by {
                    RerunBy::File => "file",
                    RerunBy::Name => "name",
                }
            ));
        };
        if !out.iter().any(|x| x == sel) {
            out.push(sel.to_string());
        }
    }
    Ok(out)
}

/// The declared command with its placeholder element replaced by one element per id.
pub(crate) fn scoped_argv(command: &[String], by: RerunBy, ids: &[String]) -> Vec<String> {
    let want = match by {
        RerunBy::File => FILES_PLACEHOLDER,
        RerunBy::Name => NAMES_PLACEHOLDER,
    };
    let mut out = Vec::with_capacity(command.len() + ids.len());
    for a in command {
        if a == want {
            out.extend(ids.iter().cloned());
        } else {
            out.push(a.clone());
        }
    }
    out
}

/// Strip ANSI styling (CSI and OSC-8) so a coloured runner's lines read as plain ones.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                // Parameters and intermediates, then one final byte in `@`..=`~`.
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                // Up to BEL or ST (`ESC \`).
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The number before `word` in `line` (`… 19 failed | …` → 19 for `failed`).
fn count_before(line: &str, word: &str) -> Option<usize> {
    let toks: Vec<&str> = line
        .split(|c: char| c.is_whitespace() || c == '|' || c == ';' || c == ',' || c == '.')
        .filter(|t| !t.is_empty())
        .collect();
    toks.windows(2)
        .find(|w| w[1] == word)
        .and_then(|w| w[0].parse().ok())
}

/// Does the head's output account for EVERY failure in `ids`, by the runner's own count? Only
/// then can a scoped retry that passes stand for the whole check: a failure the ids do not name
/// (an unhandled error, a crashed test binary, a build error) would otherwise go un-retried.
pub(crate) fn head_complete(
    by: RerunBy,
    stdout: &str,
    stderr: &str,
    ids: &[String],
) -> Result<(), String> {
    let text = plain(&format!("{stdout}\n{stderr}"));
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    match by {
        RerunBy::File => {
            // vitest: ` Test Files  19 failed | 385 passed (409)` and, for unhandled errors,
            // `      Errors  1 error`. The LAST summary is the final pass's.
            if lines.iter().any(|l| {
                l.starts_with("Errors ") && (l.ends_with(" error") || l.ends_with(" errors"))
            }) {
                return Err("the runner reported errors outside any test".into());
            }
            let Some(summary) = lines.iter().rev().find(|l| l.starts_with("Test Files ")) else {
                return Err("the runner printed no `Test Files … failed` count".into());
            };
            match count_before(summary, "failed") {
                Some(n) if n == ids.len() => Ok(()),
                Some(n) => Err(format!(
                    "the runner counted {n} failed files, the ids name {}",
                    ids.len()
                )),
                None => Err("the runner's `Test Files` line counts no failed file".into()),
            }
        }
        RerunBy::Name => {
            // libtest: `test a::b ... FAILED`, one `test result: FAILED. P passed; F failed; …`
            // per failing binary, and cargo's `error: test failed, to rerun pass …` per failing
            // target (a crashed binary has the error but no result line).
            if lines
                .iter()
                .any(|l| l.starts_with("error: could not compile") || l.starts_with("error["))
            {
                return Err("the build failed".into());
            }
            let failed_lines = lines
                .iter()
                .filter(|l| l.starts_with("test ") && l.ends_with(" ... FAILED"))
                .count();
            let results: Vec<&&str> = lines
                .iter()
                .filter(|l| l.starts_with("test result: FAILED."))
                .collect();
            let counted: usize = results
                .iter()
                .map(|l| count_before(l, "failed").unwrap_or(usize::MAX / 4))
                .sum();
            let targets = lines
                .iter()
                .filter(|l| {
                    l.starts_with("error: test failed, to rerun pass")
                        || l.starts_with("error: doctest failed, to rerun pass")
                })
                .count();
            if results.is_empty() {
                return Err("the runner printed no `test result: FAILED.` line".into());
            }
            if targets != results.len() {
                return Err(format!(
                    "{targets} test targets failed but {} printed a result (a target crashed)",
                    results.len()
                ));
            }
            if counted != failed_lines {
                return Err(format!(
                    "the runner counted {counted} failed tests, its output names {failed_lines}"
                ));
            }
            Ok(())
        }
    }
}

/// Does a passing run's output show that `id` RAN and passed? A selection the command did not
/// actually run (a filter that matched nothing) proves nothing.
pub(crate) fn proven(by: RerunBy, stdout: &str, stderr: &str, id: &str) -> bool {
    let text = plain(&format!("{stdout}\n{stderr}"));
    text.lines().any(|l| match by {
        // vitest: `✓ tests/x.test.ts > suite > name 12ms` (verbose) or `✓ tests/x.test.ts (3 tests)`
        // (default); `√` where the terminal has no unicode.
        RerunBy::File => {
            let l = l.trim_start();
            ["✓ ", "√ "]
                .iter()
                .filter_map(|m| l.strip_prefix(m))
                .any(|rest| {
                    rest.strip_prefix(id)
                        .is_some_and(|after| after.is_empty() || after.starts_with(' '))
                })
        }
        RerunBy::Name => l.trim() == format!("test {id} ... ok"),
    })
}

/// The scope record of a run of `ids`.
pub(crate) fn scope_of(by: RerunBy, ids: &[String]) -> RunScope {
    RunScope {
        by: by.as_wire().to_string(),
        ids: ids.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn entry(v: serde_json::Value) -> Result<BTreeMap<String, RerunDecl>, String> {
        let raw: BTreeMap<String, RerunEntry> =
            serde_json::from_value(v).map_err(|e| e.to_string())?;
        rerun_of(&raw)
    }

    #[test]
    fn a_rerun_entry_is_validated_strictly() {
        let ok = entry(serde_json::json!({
            "test": {"command": ["npm", "run", "test", "--", "{files}"], "by": "file", "root": "packages/crew"},
            "test_targeted": {"self_retry": true},
            "cargo-test": {"command": "cargo test -- {names} --exact", "by": "name"},
        }))
        .unwrap();
        assert_eq!(ok["test"].root, "packages/crew");
        assert_eq!(ok["test"].command.as_ref().unwrap().1, RerunBy::File);
        assert!(ok["test_targeted"].self_retry && ok["test_targeted"].command.is_none());
        assert_eq!(
            ok["cargo-test"].command.as_ref().unwrap().0,
            ids(&["cargo", "test", "--", "{names}", "--exact"])
        );
        for (bad, why) in [
            (
                serde_json::json!({"lint": {"self_retry": true}}),
                "not a test check",
            ),
            (serde_json::json!({"test": {}}), "declares nothing"),
            (
                serde_json::json!({"test": {"self_retry": false}}),
                "declares nothing",
            ),
            (
                serde_json::json!({"test": {"by": "file", "self_retry": true}}),
                "need a `command`",
            ),
            (
                serde_json::json!({"test": {"command": ["x", "{files}"]}}),
                "needs `by`",
            ),
            (
                serde_json::json!({"test": {"command": ["x", "{files}"], "by": "files"}}),
                "`by` is",
            ),
            (
                serde_json::json!({"test": {"command": ["x"], "by": "file"}}),
                "exactly once",
            ),
            (
                serde_json::json!({"test": {"command": ["x", "{names}"], "by": "file"}}),
                "exactly once",
            ),
            (
                serde_json::json!({"test": {"command": ["x", "{files}", "{files}"], "by": "file"}}),
                "exactly once",
            ),
            (
                serde_json::json!({"test": {"command": ["x", "--f={files}", "{files}"], "by": "file"}}),
                "exactly once",
            ),
            (
                serde_json::json!({"test": {"command": ["x", "{files}", "{names}"], "by": "file"}}),
                "exactly once",
            ),
            (
                serde_json::json!({"test": {"command": false, "by": "file"}}),
                "must be a command",
            ),
            (
                serde_json::json!({"test": {"command": ["x", "{names}"], "by": "name", "root": "a"}}),
                "`by: \"file\"` only",
            ),
            (
                serde_json::json!({"test": {"command": ["x", "{files}"], "by": "file", "root": "../a"}}),
                "not a relative directory",
            ),
            (
                serde_json::json!({"test": {"command": ["x", "{files}"], "by": "file", "selfRetry": true}}),
                "unknown field",
            ),
        ] {
            let err = entry(bad.clone()).expect_err(&bad.to_string());
            assert!(err.contains(why), "{bad}: {err}");
        }
    }

    #[test]
    fn the_failing_set_is_read_off_the_ids_or_the_whole_check_runs() {
        let files = ids(&[
            "FAIL tests/a.test.ts > suite > one",
            "FAIL tests/a.test.ts > suite > two",
            "FAIL tests/b.test.ts [ tests/b.test.ts ]",
            "FAILED py/t_x.py::test_y",
        ]);
        assert_eq!(
            select(RerunBy::File, &files).unwrap(),
            ids(&["tests/a.test.ts", "tests/b.test.ts", "py/t_x.py"])
        );
        assert_eq!(
            select(RerunBy::Name, &ids(&["test a::b", "test c", "test a::b"])).unwrap(),
            ids(&["a::b", "c"])
        );
        for (by, bad) in [
            (RerunBy::File, "● suite › name"),
            (RerunBy::File, "FAIL -c=evil.ts > x"),
            (RerunBy::File, "FAIL /abs/a.test.ts > x"),
            (RerunBy::File, "FAIL ../a.test.ts > x"),
            (RerunBy::File, "FAIL |proj| tests/a.test.ts > x"),
            (RerunBy::File, "test a::b"),
            (RerunBy::Name, "test src/lib.rs - f (line 3)"),
            (RerunBy::Name, "test --nocapture"),
            (RerunBy::Name, "FAIL tests/a.test.ts > x"),
        ] {
            let good = match by {
                RerunBy::File => "FAIL tests/ok.test.ts > x",
                RerunBy::Name => "test ok::one",
            };
            let err = select(by, &ids(&[good, bad])).expect_err(bad);
            assert!(err.contains(bad), "{bad}: {err}");
        }
        assert!(select(RerunBy::File, &[])
            .unwrap_err()
            .contains("names no failing test"));
        let many: Vec<String> = (0..MAX_FAILURE_IDS).map(|i| format!("test t{i}")).collect();
        assert!(select(RerunBy::Name, &many)
            .unwrap_err()
            .contains("the cap"));
        assert_eq!(
            scoped_argv(
                &ids(&["npm", "test", "--", "{files}", "-x"]),
                RerunBy::File,
                &ids(&["a", "b"])
            ),
            ids(&["npm", "test", "--", "a", "b", "-x"])
        );
    }

    #[test]
    fn a_head_is_complete_only_when_the_runner_counts_exactly_its_ids() {
        let two = ids(&["tests/a.test.ts", "tests/b.test.ts"]);
        let out = " FAIL  tests/a.test.ts > x\n FAIL  tests/b.test.ts > y\n\n \x1b[2m Test Files \x1b[22m \x1b[1m\x1b[31m2 failed\x1b[39m\x1b[22m | 5 passed (7)\n      Tests  3 failed | 40 passed (43)\n";
        assert_eq!(head_complete(RerunBy::File, out, "", &two), Ok(()));
        assert!(head_complete(RerunBy::File, out, "", &two[..1])
            .unwrap_err()
            .contains("counted 2"));
        let errs = format!("{out}      Errors  1 error\n");
        assert!(head_complete(RerunBy::File, &errs, "", &two)
            .unwrap_err()
            .contains("outside any test"));
        assert!(head_complete(RerunBy::File, "FAIL x\n", "", &two)
            .unwrap_err()
            .contains("no `Test Files"));
        // A check that retried on its own: the LAST summary is the final pass's.
        let retried =
            format!("{out}WICKED-RETRY … re-running\n Test Files  1 failed | 1 passed (2)\n");
        assert_eq!(
            head_complete(RerunBy::File, &retried, "", &two[..1]),
            Ok(())
        );

        // libtest, real `cargo test --workspace --no-fail-fast` shapes (two binaries, a doc test).
        let stdout = "test t::bad1 ... FAILED\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\
                      test bad2 ... FAILED\n\ntest result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\
                      test t::bad1 ... FAILED\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
        let stderr = "error: test failed, to rerun pass `-p a --lib`\nerror: test failed, to rerun pass `-p a --test it`\nerror: test failed, to rerun pass `-p b --lib`\nerror: 3 targets failed:\n";
        assert_eq!(
            head_complete(RerunBy::Name, stdout, stderr, &ids(&["t::bad1", "bad2"])),
            Ok(())
        );
        // A binary that crashed (SIGABRT) has cargo's error but no result line.
        let crashed = format!("{stderr}error: test failed, to rerun pass `-p a --test crash`\n");
        assert!(head_complete(RerunBy::Name, stdout, &crashed, &[])
            .unwrap_err()
            .contains("crashed"));
        // A failure counted that no `... FAILED` line names.
        let hidden = stdout.replacen("test bad2 ... FAILED\n", "", 1);
        assert!(head_complete(RerunBy::Name, &hidden, stderr, &[])
            .unwrap_err()
            .contains("counted 3"));
        assert!(head_complete(RerunBy::Name, "error[E0425]: x\n", "", &[])
            .unwrap_err()
            .contains("build failed"));
        assert!(head_complete(RerunBy::Name, "", "error: could not compile `a`\n", &[]).is_err());
    }

    #[test]
    fn a_pass_proves_only_the_tests_it_shows_running() {
        let out = " ✓ tests/a.test.ts > suite > one 12ms\n \x1b[32m✓\x1b[39m tests/b.test.ts (3 tests) 5ms\n √ tests/c.test.ts (1 test)\n";
        for f in ["tests/a.test.ts", "tests/b.test.ts", "tests/c.test.ts"] {
            assert!(proven(RerunBy::File, out, "", f), "{f}");
        }
        assert!(
            !proven(RerunBy::File, out, "", "tests/a.test"),
            "a prefix is not the file"
        );
        assert!(!proven(RerunBy::File, out, "", "tests/d.test.ts"));
        let lib = "running 1 test\ntest t::bad1 ... ok\n\ntest result: ok. 1 passed; 0 failed\n";
        assert!(proven(RerunBy::Name, lib, "", "t::bad1"));
        assert!(!proven(RerunBy::Name, lib, "", "t::bad"));
        assert!(!proven(RerunBy::Name, "running 0 tests\n", "", "t::bad1"));
    }
}
