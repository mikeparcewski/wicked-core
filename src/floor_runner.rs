//! FLOOR RUNNERS — what the repo-checks floor knows about the test runners whose failures it may
//! EXCLUDE (DES-FLOOR-V2): cargo test (libtest + rustdoc), pytest and go test. For each, the argv
//! that prints the runner's own inventory of a selection (its TOC), the strict reading of that
//! inventory, the failing names a run's own output reports, and the runner's native, precise
//! per-test deselection — plus the allowlist an argv must be on before anything is appended to it.
//! Pure over argv and text; the floor ([`crate::repo_checks`]) runs the processes and decides.

use std::path::Path;

/// A test runner whose inventory and native per-test deselection the floor knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Runner {
    /// `cargo test` (libtest + rustdoc): `-- --list`, `--skip <name> --exact`.
    Cargo,
    /// pytest: `--collect-only -q`, `--deselect <nodeid>`.
    Pytest,
    /// `go test`: `-list .`, `-skip '^(Name)$'`.
    Go,
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
        "go" if argv.get(1).map(String::as_str) == Some("test") => Some(Runner::Go),
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

/// Is `argv` on the deselection allowlist for its runner? pytest and go refuse their own selection
/// and state flags (a `--deselect`/`-skip` of their own, collect-only, last-failed / stepwise
/// state, `-run`, `-list`, `-json`, `-exec`).
pub(crate) fn deselectable(runner: Runner, argv: &[String]) -> bool {
    match runner {
        Runner::Cargo => understood_cargo_test(argv).is_some(),
        Runner::Pytest => !argv.iter().any(|a| {
            let k = a.split('=').next().unwrap_or(a);
            matches!(
                k,
                "--deselect"
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
        Runner::Go => !argv[2..].iter().any(|a| {
            let k = a.trim_start_matches('-');
            let k = k.split('=').next().unwrap_or(k);
            a.starts_with('-') && matches!(k, "run" | "skip" | "list" | "json" | "exec" | "c")
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
            let mut out = argv.to_vec();
            out.extend(["--collect-only".to_string(), "-q".to_string()]);
            out
        }
        Runner::Go => {
            let mut out = argv[..2].to_vec();
            out.extend(["-list".to_string(), ".".to_string()]);
            out.extend(argv[2..].iter().cloned());
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
                    names.push(n.to_string());
                    continue;
                }
                // `N tests, M benchmarks` — one per binary; the counts must add up.
                let (t, b) = l.split_once(", ")?;
                let count = |s: &str, unit: &str| -> Option<usize> {
                    let (n, u) = s.split_once(' ')?;
                    (u == unit || u == format!("{unit}s")).then(|| n.parse().ok())?
                };
                listed += count(t, "test")? + count(b, "benchmark")?;
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
            }
            Runner::Go => {
                if l.starts_with("ok ") || l.starts_with("?   ") {
                    continue;
                }
                let ident = |s: &str| {
                    s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                };
                if !ident(l) {
                    return None;
                }
                names.push(l.to_string());
            }
        }
    }
    let counted = runner == Runner::Go || listed == names.len();
    counted.then_some(names)
}

/// The failing test names a run's OWN output (the base's) carries, in its runner's format.
pub(crate) fn failing_names(runner: Runner, text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let l = line.trim_end_matches('\r');
        let name = match runner {
            Runner::Cargo => l
                .strip_prefix("test ")
                .and_then(|r| r.strip_suffix(" ... FAILED")),
            Runner::Pytest => l
                .strip_prefix("FAILED ")
                .or_else(|| l.strip_prefix("ERROR "))
                .map(|r| r.split(" - ").next().unwrap_or(r).trim_end()),
            Runner::Go => l
                .trim_start()
                .strip_prefix("--- FAIL: ")
                .map(|r| r.split(' ').next().unwrap_or(r)),
        };
        if let Some(n) = name.filter(|n| !n.is_empty()) {
            if !out.iter().any(|o| o == n) && out.len() < crate::repo_checks::MAX_FAILURE_IDS {
                out.push(n.to_string());
            }
        }
    }
    out
}

/// The test names a run's output reports PASSING, where the runner prints a line per test by
/// default (libtest `test X ... ok`); empty for runners that list only failures.
pub(crate) fn passing_names(runner: Runner, text: &str) -> Vec<String> {
    match runner {
        Runner::Cargo => text
            .lines()
            .filter_map(|l| {
                l.trim_end_matches('\r')
                    .strip_prefix("test ")?
                    .strip_suffix(" ... ok")
                    .map(str::to_string)
            })
            .collect(),
        Runner::Pytest | Runner::Go => Vec::new(),
    }
}

/// `argv` with `names` deselected natively: libtest `--skip <name>` … `--exact` (exact names,
/// every binary), pytest `--deselect <nodeid>`, go `-skip '^(A|B)$'`.
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
        Runner::Go => {
            let mut out = argv[..2].to_vec();
            out.push(format!("-skip=^({})$", names.join("|")));
            out.extend(argv[2..].iter().cloned());
            out
        }
    }
}

/// (core#1060 r21) A head `.cargo/config[.toml]` at or above the check's directory (inside the
/// tree) that sets `rustdocflags` or test arguments: `--exact` would then turn its filter into an
/// exact name, and a doctest failure would deselect to nothing. `Some(why)` refuses exclusion; an
/// unreadable or linked config refuses too.
pub(crate) fn cargo_config_bends_test_args(worktree: &Path, dir: &str) -> Option<String> {
    let mut at = worktree.join(dir);
    loop {
        for name in [".cargo/config", ".cargo/config.toml"] {
            let p = at.join(name);
            match std::fs::symlink_metadata(&p) {
                Err(_) => continue,
                Ok(m) if !m.is_file() => {
                    return Some(format!("`{name}` is not a regular file"));
                }
                Ok(_) => {}
            }
            let raw = match std::fs::read_to_string(&p) {
                Ok(r) => r,
                Err(e) => return Some(format!("`{name}` could not be read: {e}")),
            };
            if raw.contains("rustdocflags") || raw.contains("test-args") {
                return Some(format!(
                    "the head's `{name}` sets rustdocflags or test-args, which `--exact` would \
                     change"
                ));
            }
        }
        if at == worktree || !at.pop() || !at.starts_with(worktree) {
            return None;
        }
    }
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
            parse_toc(Runner::Go, "TestA\nTestB\nok  \texample.com/m\t0.004s\n?   \texample.com/m/x\t[no test files]\n"),
            Some(s(&["TestA", "TestB"]))
        );
        assert_eq!(parse_toc(Runner::Go, "TestA\npanic: boom\n"), None);
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
                "FAILED tests/a.py::test_x - assert 1 == 2\nERROR tests/a.py::test_e\n"
            ),
            s(&["tests/a.py::test_x", "tests/a.py::test_e"])
        );
        assert_eq!(
            failing_names(
                Runner::Go,
                "--- FAIL: TestA (0.00s)\n    --- FAIL: TestA/sub (0.00s)\n"
            ),
            s(&["TestA", "TestA/sub"])
        );
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
            deselect_argv(
                Runner::Go,
                &s(&["go", "test", "./..."]),
                &s(&["TestA", "TestB"])
            ),
            s(&["go", "test", "-skip=^(TestA|TestB)$", "./..."])
        );
        assert_eq!(
            toc_argv(Runner::Go, &s(&["go", "test", "./..."])),
            s(&["go", "test", "-list", ".", "./..."])
        );
        assert_eq!(
            runner_of(&s(&["python3", "-m", "pytest", "-q"])),
            Some(Runner::Pytest)
        );
        assert_eq!(runner_of(&s(&["npx", "vitest", "run"])), None);
        assert_eq!(runner_of(&s(&["npm", "run", "test"])), None);
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
        assert!(ok(&["cargo", "+nightly", "test", "--workspace", "-Ffeat"]));
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
            &["go", "test", "-run", "TestA", "./..."],
            &["go", "test", "-json", "./..."],
        ] {
            assert!(!ok(bad), "{bad:?} must not be deselected");
        }
    }
}
