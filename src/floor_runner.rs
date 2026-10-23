//! FLOOR RUNNERS — what the repo-checks floor knows about the test runners whose failures it may
//! EXCLUDE (DES-FLOOR-V2): cargo test (libtest + rustdoc) and pytest. For each, the argv that
//! prints the runner's own inventory of a selection (its TOC), the strict reading of that
//! inventory, the failing names a run's own output reports, and the runner's native, precise
//! per-test deselection — plus the allowlist an argv must be on before anything is appended to it.
//! Pure over argv and text; the floor ([`crate::repo_checks`]) runs the processes and decides.
//!
//! go test is not here on purpose: its subtests are created at run time and absent from
//! `go test -list`, so skipping a failing top-level test can skip a passing subtest the head then
//! breaks, and no inventory can show otherwise (#1066 review round 2). A red go base escalates, like
//! vitest's and jest's.

use std::path::Path;

/// A test runner whose inventory and native per-test deselection the floor knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Runner {
    /// `cargo test` (libtest + rustdoc): `-- --list`, `--skip <name> --exact`.
    Cargo,
    /// pytest: `--collect-only -q`, `--deselect <nodeid>`.
    Pytest,
}

fn bin_name(argv: &[String]) -> Option<&str> {
    Path::new(argv.first()?).file_stem()?.to_str()
}

/// The runner a check's argv invokes, when it is one the floor can inventory and deselect.
pub(crate) fn runner_of(argv: &[String]) -> Option<Runner> {
    if cargo_test_sub(argv).is_some() {
        return Some(Runner::Cargo);
    }
    match bin_name(argv)? {
        "pytest" | "py.test" => Some(Runner::Pytest),
        "python" | "python3" if argv.get(1..3) == Some(&["-m".into(), "pytest".into()]) => {
            Some(Runner::Pytest)
        }
        _ => None,
    }
}

/// The index of `test` in a `cargo [+toolchain] test …` argv, or `None` when it is not one.
pub(crate) fn cargo_test_sub(argv: &[String]) -> Option<usize> {
    let file = Path::new(argv.first()?).file_name()?.to_str()?;
    if file != "cargo" && file != "cargo.exe" {
        return None;
    }
    let sub = if argv.get(1)?.starts_with('+') { 2 } else { 1 };
    (argv.get(sub)? == "test").then_some(sub)
}

/// (core#1060 r16) A `cargo [+toolchain] test` argv the floor understands element by element,
/// split at `--` into (cargo arguments, test-binary arguments), or `None`. Only such a check is
/// deselected: the floor appends `--skip <name>` and `--exact`, and anything it does not know — a
/// positional TESTNAME (`--exact` turns it into a name no test has), a `--skip` or `--exact` of
/// its own, `--ignored` / `--include-ignored`, `-q`, an unknown flag, a second `--` — could make
/// that select something else.
fn understood_cargo_test(argv: &[String]) -> Option<(&[String], &[String])> {
    let sub = cargo_test_sub(argv)?;
    const CARGO_BARE: &[&str] = &[
        "--workspace",
        "--all",
        "--all-features",
        "--no-default-features",
        "--release",
        "--locked",
        "--frozen",
        "--offline",
        "--no-fail-fast",
    ];
    const CARGO_VALUED: &[&str] = &[
        "-p",
        "--package",
        "--exclude",
        "-F",
        "--features",
        "--profile",
        "--target",
        "-j",
        "--jobs",
        "--color",
    ];
    const TEST_BARE: &[&str] = &["--nocapture", "--show-output"];
    const TEST_VALUED: &[&str] = &["--test-threads", "--color"];
    fn allowed(args: &[String], bare: &[&str], valued: &[&str]) -> bool {
        let mut i = 0;
        while let Some(a) = args.get(i).map(String::as_str) {
            i += 1;
            if bare.contains(&a) {
                continue;
            }
            if valued.contains(&a) {
                match args.get(i) {
                    Some(v) if !v.is_empty() && !v.starts_with('-') => i += 1,
                    _ => return false,
                }
                continue;
            }
            let joined = a
                .split_once('=')
                .is_some_and(|(k, v)| k.starts_with("--") && valued.contains(&k) && !v.is_empty());
            // `-pfoo`, `-Ffeat`, `-j8`: a short valued flag with its value attached.
            let short = a.len() > 2
                && !a.starts_with("--")
                && valued.iter().any(|f| f.len() == 2 && a.starts_with(f));
            if !joined && !short {
                return false;
            }
        }
        true
    }
    let split = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    let (cargo_args, test_args) = (&argv[..split], argv.get(split + 1..).unwrap_or(&[]));
    (allowed(&cargo_args[sub + 1..], CARGO_BARE, CARGO_VALUED)
        && allowed(test_args, TEST_BARE, TEST_VALUED))
    .then_some((cargo_args, test_args))
}

/// Is `argv` on the deselection allowlist for its runner? cargo: the r16 allowlist plus
/// `--no-fail-fast`; pytest refuses its own selection and state flags (a `--deselect` of its own,
/// collect-only, last-failed / stepwise state, `--`).
pub(crate) fn deselectable(runner: Runner, argv: &[String]) -> bool {
    match runner {
        Runner::Cargo => {
            understood_cargo_test(argv).is_some() && argv.iter().any(|a| a == "--no-fail-fast")
        }
        Runner::Pytest => !argv.iter().any(|a| {
            let k = a.split('=').next().unwrap_or(a);
            matches!(
                k,
                "--" | "--deselect"
                    | "--co"
                    | "--collect-only"
                    | "--lf"
                    | "--last-failed"
                    | "--ff"
                    | "--failed-first"
                    | "--nf"
                    | "--new-first"
                    | "--sw"
                    | "--stepwise"
                    | "--sw-skip"
                    | "--stepwise-skip"
            )
        }),
    }
}

/// The argv that prints the runner's own inventory of `argv`'s selection (its TOC).
pub(crate) fn toc_argv(runner: Runner, argv: &[String]) -> Vec<String> {
    match runner {
        Runner::Cargo => {
            let split = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
            let mut out = argv[..split].to_vec();
            out.push("--".into());
            out.extend(argv.get(split + 1..).unwrap_or(&[]).iter().cloned());
            out.push("--list".into());
            out
        }
        Runner::Pytest => {
            // Verbosity stacks (`-q -q` lists per-file counts, `-v -q` the tree): the check's own
            // flags are dropped so the inventory prints exactly one node id per line.
            let verbosity = |a: &str| {
                matches!(a, "--quiet" | "--verbose")
                    || (a.len() > 1
                        && a.starts_with('-')
                        && !a.starts_with("--")
                        && a[1..].chars().all(|c| c == 'q' || c == 'v'))
            };
            let mut out: Vec<String> = argv.iter().filter(|a| !verbosity(a)).cloned().collect();
            out.extend(["--collect-only".to_string(), "-q".to_string()]);
            out
        }
    }
}

/// The test names an inventory lists, or `None` when any stdout line is not the runner's own
/// inventory format (a custom harness, a warning, a collection error) — an inventory the floor
/// cannot read completely is no inventory.
pub(crate) fn parse_toc(runner: Runner, stdout: &str) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut listed = 0usize;
    let mut summaries = 0usize;
    for line in stdout.lines() {
        let l = line.trim_end_matches('\r');
        if l.trim().is_empty() {
            continue;
        }
        match runner {
            Runner::Cargo => {
                // rustdoc's own line after merged (edition 2024) doctests.
                if l.starts_with("all doctests ran in ") {
                    continue;
                }
                if let Some(n) = l
                    .strip_suffix(": test")
                    .or_else(|| l.strip_suffix(": benchmark"))
                {
                    names.push(libtest_name(n).to_string());
                    continue;
                }
                // `N tests, M benchmarks` — one per binary; the counts must add up.
                let (t, b) = l.split_once(", ")?;
                let count = |s: &str, unit: &str| -> Option<usize> {
                    let (n, u) = s.split_once(' ')?;
                    (u == unit || u == format!("{unit}s")).then(|| n.parse().ok())?
                };
                listed += count(t, "test")? + count(b, "benchmark")?;
                summaries += 1;
            }
            Runner::Pytest => {
                if l.contains("::") && !l.contains(' ') {
                    names.push(l.to_string());
                    continue;
                }
                // `N tests collected in 0.01s` / `1 test collected in …` / `no tests collected …`
                let (n, rest) = l.split_once(' ')?;
                if !(rest.starts_with("test collected") || rest.starts_with("tests collected")) {
                    return None;
                }
                listed += if n == "no" { 0 } else { n.parse().ok()? };
                summaries += 1;
            }
        }
    }
    (listed == names.len() && summaries > 0).then_some(names)
}

/// libtest's result-line name, without the ` - should panic` it prints for a `#[should_panic]`
/// test (its inventory lists the bare name).
fn libtest_name(r: &str) -> &str {
    let r = r.strip_suffix(" - should panic").unwrap_or(r);
    r.strip_suffix(" - compile fail").unwrap_or(r)
}

/// A libtest result line's test name and verdict (`ok`, `ignored` — with or without its `, reason`
/// — or `FAILED`), or `None` for any other line.
fn libtest_result(l: &str) -> Option<(&str, &str)> {
    let r = l.trim_end_matches('\r').strip_prefix("test ")?;
    let (name, verdict) = r.rsplit_once(" ... ")?;
    let verdict = match verdict {
        "ok" | "FAILED" => verdict,
        v if v == "ignored" || v.starts_with("ignored, ") => "ignored",
        _ => return None,
    };
    Some((libtest_name(name), verdict))
}

/// The failing test names a run's OWN stdout (the base's) carries, read only from the runner's
/// own result lines: libtest `test X ... FAILED`; pytest's LAST `short test summary info` section
/// (`FAILED <id>` / `ERROR <id>`), and only when the `=` line that closes it is the run's final
/// line — the outer run prints its summary and footer at the very end, so a child pytest's summary
/// inside a test's captured output (which comes earlier and is followed by more output), or a run
/// whose summary is suppressed (`-rN`) or unbannered (`-q`), names nothing.
pub(crate) fn failing_names(runner: Runner, text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_summary = false;
    // pytest: the section is closed by an `=` footer, which must be the last non-empty line.
    let mut closed_at_end = false;
    for line in text.lines() {
        let l = line.trim_end_matches('\r');
        if runner == Runner::Pytest && !l.trim().is_empty() {
            closed_at_end = false;
        }
        if runner == Runner::Pytest && l.starts_with('=') {
            if l.contains(" short test summary info ") {
                in_summary = true;
                out.clear();
            } else if in_summary {
                in_summary = false;
                closed_at_end = true;
            }
            continue;
        }
        let name = match runner {
            Runner::Cargo => libtest_result(l)
                .filter(|(_, v)| *v == "FAILED")
                .map(|(n, _)| n),
            Runner::Pytest if in_summary => l
                .strip_prefix("FAILED ")
                .or_else(|| l.strip_prefix("ERROR "))
                .map(|r| r.split(" - ").next().unwrap_or(r).trim_end()),
            Runner::Pytest => None,
        };
        if let Some(n) = name.filter(|n| !n.is_empty()) {
            if !out.iter().any(|o| o == n) && out.len() < crate::repo_checks::MAX_FAILURE_IDS {
                out.push(n.to_string());
            }
        }
    }
    if runner == Runner::Pytest && !closed_at_end {
        out.clear();
    }
    out
}

/// The test names a run's output reports PASSING, where the runner prints a line per test by
/// default (libtest `test X ... ok`); empty for runners that list only failures.
pub(crate) fn passing_names(runner: Runner, text: &str) -> Vec<String> {
    match runner {
        Runner::Cargo => text
            .lines()
            .filter_map(libtest_result)
            .filter(|(_, v)| *v != "FAILED")
            .map(|(n, _)| n.to_string())
            .collect(),
        Runner::Pytest => Vec::new(),
    }
}

/// (cargo) The argv that runs ONLY `name` (exactly) of `argv`'s selection: the floor's
/// confirmation, on the base, that a name its output reports failing fails when run alone.
pub(crate) fn alone_argv(argv: &[String], name: &str) -> Vec<String> {
    let split = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    let mut out = argv[..split].to_vec();
    out.push("--".into());
    out.extend(argv.get(split + 1..).unwrap_or(&[]).iter().cloned());
    out.extend([name.to_string(), "--exact".to_string()]);
    out
}

/// (pytest) `--deselect` matches node-id PREFIXES: the first deselected name that would also remove
/// a head test outside `names` (`a.py::test_x` takes `a.py::test_x_extra` and `a.py::test_x[1]`),
/// or `None` when every match is intended.
pub(crate) fn pytest_prefix_collision<'a>(names: &'a [String], head: &[String]) -> Option<&'a str> {
    names
        .iter()
        .find(|n| {
            head.iter()
                .any(|h| h.starts_with(n.as_str()) && !names.contains(h))
        })
        .map(String::as_str)
}

/// `argv` with `names` deselected natively: libtest `--skip <name>` … `--exact` (exact names,
/// every binary), pytest `--deselect <nodeid>`.
pub(crate) fn deselect_argv(runner: Runner, argv: &[String], names: &[String]) -> Vec<String> {
    match runner {
        Runner::Cargo => {
            let split = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
            let mut out = argv[..split].to_vec();
            out.push("--".into());
            out.extend(argv.get(split + 1..).unwrap_or(&[]).iter().cloned());
            for n in names {
                out.push(format!("--skip={n}"));
            }
            out.push("--exact".into());
            out
        }
        Runner::Pytest => {
            let mut out = argv.to_vec();
            for n in names {
                out.push(format!("--deselect={n}"));
            }
            out
        }
    }
}

/// (core#1060 r21) A cargo config that sets `rustdocflags` or test arguments: `--exact` would
/// then turn its filter into an exact name, and a doctest failure would deselect to nothing.
/// Every config cargo loads is read — `.cargo/config[.toml]` in `check_dir` and EVERY ancestor up
/// to the filesystem root, and `config[.toml]` in `cargo_home` (the floor's scratch, which a test
/// can write: the floor re-reads it before each further run). `Some(why)` refuses exclusion; an
/// unreadable, linked or unparseable config refuses too.
pub(crate) fn cargo_config_bends_test_args(check_dir: &Path, cargo_home: &Path) -> Option<String> {
    let mut files: Vec<std::path::PathBuf> = check_dir
        .ancestors()
        .flat_map(|d| [d.join(".cargo/config"), d.join(".cargo/config.toml")])
        .collect();
    files.extend([cargo_home.join("config"), cargo_home.join("config.toml")]);
    {
        for p in files {
            let name = p.display().to_string();
            match std::fs::symlink_metadata(&p) {
                Err(_) => continue,
                Ok(m) if !m.is_file() => {
                    return Some(format!("a cargo config is not a regular file ({name})"));
                }
                Ok(_) => {}
            }
            let raw = match std::fs::read_to_string(&p) {
                Ok(r) => r,
                Err(e) => return Some(format!("a cargo config could not be read: {e}")),
            };
            // Decoded as cargo decodes it (`"rustdoc\u0066lags"` is `rustdocflags`); a config
            // that does not parse, or that includes another, cannot be vouched for.
            let Ok(v) = toml::from_str::<toml::Value>(&raw) else {
                return Some("a cargo config does not parse as TOML".to_string());
            };
            fn bends(v: &toml::Value) -> bool {
                let word = |s: &str| {
                    let s = s.to_ascii_lowercase();
                    s.contains("rustdocflags") || s.contains("test-args") || s.contains("test_args")
                };
                match v {
                    toml::Value::String(s) => word(s),
                    toml::Value::Array(a) => a.iter().any(bends),
                    toml::Value::Table(t) => {
                        t.iter().any(|(k, v)| word(k) || k == "include" || bends(v))
                    }
                    _ => false,
                }
            }
            if bends(&v) {
                return Some(
                    "a cargo config sets rustdocflags, test args or an include, which `--exact` \
                     would change"
                        .to_string(),
                );
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// Inventories, failing names and native deselection, per runner — pure.
    #[test]
    fn inventories_failures_and_deselection_are_read_per_runner() {
        // libtest: list lines and the count must agree; rustdoc's merged-doctest timing line is
        // its own; anything else (a custom harness, a stray line) is no inventory.
        let toc = "t::a: test\nt::b: test\n\n2 tests, 0 benchmarks\nsrc/lib.rs - f (line 1): test\n\n\
                   1 test, 0 benchmarks\nall doctests ran in 1.27s; merged doctests compilation took 1.00s\n";
        assert_eq!(
            parse_toc(Runner::Cargo, toc),
            Some(s(&["t::a", "t::b", "src/lib.rs - f (line 1)"]))
        );
        assert_eq!(
            parse_toc(Runner::Cargo, "t::a: test\n\n2 tests, 0 benchmarks\n"),
            None
        );
        assert_eq!(
            parse_toc(Runner::Cargo, "t::a: test\nhello\n\n1 test, 0 benchmarks\n"),
            None
        );
        assert_eq!(
            parse_toc(
                Runner::Pytest,
                "tests/a.py::test_x\ntests/a.py::test_y[1]\n\n2 tests collected in 0.01s\n"
            ),
            Some(s(&["tests/a.py::test_x", "tests/a.py::test_y[1]"]))
        );
        assert_eq!(
            parse_toc(
                Runner::Pytest,
                "tests/a.py::test_x\n=== warnings summary ===\n"
            ),
            None
        );
        assert_eq!(
            failing_names(
                Runner::Cargo,
                "test t::a ... FAILED\ntest t::b ... ok\n    test t::c ... FAILED trailing\n"
            ),
            s(&["t::a"])
        );
        assert_eq!(
            failing_names(
                Runner::Pytest,
                "=== short test summary info ===\nFAILED tests/a.py::test_x - assert 1 == 2\n\
                 ERROR tests/a.py::test_e\n=== 1 failed, 1 error ===\n"
            ),
            s(&["tests/a.py::test_x", "tests/a.py::test_e"])
        );
        // (agy r2) libtest prints `- should panic` after a `#[should_panic]` test's name; the
        // inventory lists the bare name.
        assert_eq!(
            failing_names(Runner::Cargo, "test t::sp - should panic ... FAILED\n"),
            s(&["t::sp"])
        );
        assert_eq!(
            passing_names(Runner::Cargo, "test t::sp - should panic ... ok\n"),
            s(&["t::sp"])
        );
        // (agy r4) `ignored, reason` and `- compile fail` are libtest's own spellings.
        assert_eq!(
            passing_names(
                Runner::Cargo,
                "test a ... ignored, flaky on CI\ntest src/lib.rs - f (line 3) - compile fail ... ok\n"
            ),
            s(&["a", "src/lib.rs - f (line 3)"])
        );
        assert_eq!(
            alone_argv(
                &s(&["cargo", "test", "--no-fail-fast", "--", "--test-threads=1"]),
                "t::a"
            ),
            s(&[
                "cargo",
                "test",
                "--no-fail-fast",
                "--",
                "--test-threads=1",
                "t::a",
                "--exact"
            ])
        );
        // (agy r4) a pytest summary counts only when its closing footer is the run's last line.
        let unclosed = "=== short test summary info ===\nFAILED a.py::t - x\n1 failed in 0.01s\n";
        assert!(failing_names(Runner::Pytest, unclosed).is_empty());
        let child_then_more =
            "=== short test summary info ===\nFAILED a.py::fine - x\n=== 1 failed in 0.1s ===\n\
                               more captured output\n=== 2 passed, 1 failed in 0.5s ===\n";
        assert!(failing_names(Runner::Pytest, child_then_more).is_empty());
        let cargo = s(&["cargo", "test", "--locked", "--", "--test-threads=4"]);
        assert_eq!(
            toc_argv(Runner::Cargo, &cargo),
            s(&[
                "cargo",
                "test",
                "--locked",
                "--",
                "--test-threads=4",
                "--list"
            ])
        );
        assert_eq!(
            deselect_argv(Runner::Cargo, &cargo, &s(&["t::a", "t::b"])),
            s(&[
                "cargo",
                "test",
                "--locked",
                "--",
                "--test-threads=4",
                "--skip=t::a",
                "--skip=t::b",
                "--exact"
            ])
        );
        assert_eq!(
            deselect_argv(
                Runner::Pytest,
                &s(&["pytest", "tests"]),
                &s(&["tests/a.py::test_x"])
            ),
            s(&["pytest", "tests", "--deselect=tests/a.py::test_x"])
        );
        assert_eq!(
            runner_of(&s(&["python3", "-m", "pytest", "-q"])),
            Some(Runner::Pytest)
        );
        assert_eq!(runner_of(&s(&["npx", "vitest", "run"])), None);
        assert_eq!(runner_of(&s(&["npm", "run", "test"])), None);
        // (#1066 review round 2) go's subtests are not in its inventory: no precise deselect.
        assert_eq!(runner_of(&s(&["go", "test", "./..."])), None);
    }

    /// Rounds 1-2 of #1066: an empty inventory is no inventory; pytest's verbosity is pinned for
    /// the inventory; only the runners' OWN result lines name failures (pytest's last short
    /// summary); `ignored` is a libtest witness; a pytest prefix that would take another test is
    /// caught.
    #[test]
    fn inventories_and_failures_read_only_the_runners_own_lines() {
        for r in [Runner::Cargo, Runner::Pytest] {
            assert_eq!(
                parse_toc(r, ""),
                None,
                "{r:?}: an empty stdout is no inventory"
            );
        }
        assert_eq!(
            toc_argv(
                Runner::Pytest,
                &s(&["pytest", "-q", "-vv", "--quiet", "tests"])
            ),
            s(&["pytest", "tests", "--collect-only", "-q"])
        );
        let py = "tests/a.py::test_x PASSED\nFAILED tests/a.py::test_ok - printed by a test\n\
                  =========== short test summary info ===========\n\
                  FAILED tests/a.py::test_x - assert 1 == 2\n=========== 1 failed ===========\n";
        assert_eq!(
            failing_names(Runner::Pytest, py),
            s(&["tests/a.py::test_x"])
        );
        // (codex r2) A child pytest's summary inside a test's captured output comes BEFORE the
        // outer run's own, which is last: only the last section counts.
        let nested = "---- Captured stdout call ----\n=== short test summary info ===\n\
                      FAILED tests/a.py::test_fine - a child run's line\n=== 1 failed ===\n\
                      =========== short test summary info ===========\n\
                      FAILED tests/a.py::test_old - boom\n=========== 1 failed ===========\n";
        assert_eq!(
            failing_names(Runner::Pytest, nested),
            s(&["tests/a.py::test_old"])
        );
        assert_eq!(
            passing_names(
                Runner::Cargo,
                "test a ... ok\ntest b ... ignored\ntest c ... FAILED\n"
            ),
            s(&["a", "b"])
        );
        let head = s(&[
            "a.py::test_case",
            "a.py::test_case_extra",
            "a.py::test_case[1]",
            "b.py::t",
        ]);
        assert_eq!(
            pytest_prefix_collision(&s(&["a.py::test_case"]), &head),
            Some("a.py::test_case")
        );
        assert_eq!(pytest_prefix_collision(&s(&["b.py::t"]), &head), None);
    }

    /// (core#1060 r16) The allowlist: what the floor appends cannot be bent by the argv.
    #[test]
    fn only_an_allowlisted_argv_is_deselected() {
        let ok = |a: &[&str]| deselectable(runner_of(&s(a)).unwrap(), &s(a));
        assert!(ok(&[
            "cargo",
            "test",
            "--locked",
            "--no-fail-fast",
            "-p",
            "a",
            "--",
            "--test-threads",
            "4"
        ]));
        assert!(ok(&[
            "cargo",
            "+nightly",
            "test",
            "--workspace",
            "-Ffeat",
            "--no-fail-fast"
        ]));
        // (agy r1) Without `--no-fail-fast` cargo stops at the first red binary, so later tests
        // print no result line — no witness for the base's names.
        assert!(!ok(&["cargo", "test", "--workspace"]));
        for bad in [
            &["cargo", "test", "foo"][..],
            &["cargo", "test", "--", "--skip", "x"],
            &["cargo", "test", "--", "--exact"],
            &["cargo", "test", "--", "--ignored"],
            &["cargo", "test", "--", "--include-ignored"],
            &["cargo", "test", "-q"],
            &["cargo", "test", "--doc", "--", "x"],
            &["cargo", "test", "--", "--nocapture", "--", "x"],
            &["pytest", "--deselect", "a::b"],
            &["pytest", "--lf"],
            &["pytest", "--", "tests"],
        ] {
            assert!(!ok(bad), "{bad:?} must not be deselected");
        }
    }
}
