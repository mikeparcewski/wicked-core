//! (DES-FLOOR-V2, DES-BUILD-GUIDE) The guide-driven floor's tests: stand-in runners whose exit
//! codes are each tree's truth, real cargo, pytest and vitest fixtures, the probe, and the forgery
//! rounds of core#1060 / #1066 — each must deny or escalate, never pass. A child of
//! `repo_checks`, so it reads the floor's private items.

#![cfg_attr(not(unix), allow(unused_imports, dead_code))]

#[cfg(unix)]
use super::test_seam as seam;
use super::tests::*;
use super::*;

// ─── DES-FLOOR-V2: the suite's TOC plus exit codes ─────────────────────────────────────────

/// A stand-in `cargo` whose EXIT CODES are the truth of each tree and whose TEXT is whatever
/// a test (or a forger) prints. It reads its behaviour from files in the tree it runs in:
/// `-- --list` prints `toc.txt` (exit `toc.exit`, default 0); a run with any `--skip=` is the
/// excluded run (`excluded.out` / `excluded.err`, exit `excluded.exit`, sleeping
/// `excluded.sleep` seconds first); any other run prints `run.out` / `run.err` and exits
/// `run.exit` — or, with `flake` present, 101 the first time in this floor and `run.exit`
/// after (`flake.excluded`: the same for the excluded run). Absolute path, outside every tree.
#[cfg(unix)]
fn fake_cargo(tag: &str) -> String {
    fake_runner(tag, "cargo")
}

/// [`fake_cargo`] under another runner's name (`pytest`): `--collect-only` is its inventory,
/// `--deselect=` its excluded run.
#[cfg(unix)]
fn fake_runner(tag: &str, name: &str) -> String {
    let bin = scratch(&format!("{tag}-bin"));
    let path = bin.join(name);
    std::fs::write(
        &path,
        "#!/bin/sh\n\
         mode=run\n\
         prev=; name=; fail=101; case \"$0\" in *pytest) fail=1;; esac\n\
         skip=' '\n\
         for a in \"$@\"; do case \"$a\" in --list|--collect-only) mode=toc;; --skip=*|--deselect=*) skip=\"$skip${a#--*=} \"; [ $mode = toc ] || mode=excluded;; \
         *::*) [ $mode = run ] && mode=alone && name=$a;; \
         --exact) [ $mode = run ] && mode=alone && name=$prev;; esac; prev=$a; done\n\
         key=\"$TMPDIR/n-$(pwd | cksum | cut -d' ' -f1)-$mode\"\n\
         [ -f counters.dir ] && { d=$(cat counters.dir); mkdir -p \"$d\" && echo x >> \"$d/$mode\"; } 2>/dev/null\n\
         n=$(cat \"$key\" 2>/dev/null || echo 0); n=$((n+1)); echo $n > \"$key\"\n\
         code() { if [ -f \"$1\" ]; then cat \"$1\"; else echo 0; fi; }\n\
         case $mode in\n\
           toc) if [ -f toc.err ]; then cat toc.err >&2; else echo '     Running unittests src/lib.rs (target/debug/deps/fake-0)' >&2; fi\n\
             while IFS= read -r l; do t=${l%: test}; case \"$skip\" in *\" $t \"*) [ \"$t\" != \"$l\" ] && continue;; esac; if [ -f skip.prefix ] && [ \"$t\" != \"$l\" ]; then p=0; for s in $skip; do case \"$t\" in \"$s\"*) p=1;; esac; done; [ $p = 1 ] && continue; fi; printf '%s\\n' \"$l\"; done < toc.txt 2>/dev/null\n\
             exit $(code toc.exit);;\n\
           alone) grep -qx \"$name\" alone.fail 2>/dev/null && exit $fail; exit 0;;\n\
           excluded) [ -f excluded.sleep ] && sleep $(cat excluded.sleep)\n\
             cat excluded.out 2>/dev/null; cat excluded.err >&2 2>/dev/null\n\
             [ -f excluded.signal ] && kill -KILL $$\n\
             if [ -f flake.excluded ] && [ $n = 1 ]; then exit 101; fi; exit $(code excluded.exit);;\n\
           *) if [ -f toc.err ]; then cat toc.err >&2; else echo '     Running unittests src/lib.rs (target/debug/deps/fake-0)' >&2; fi\n\
             cat run.out 2>/dev/null; cat run.err >&2 2>/dev/null\n\
             case \"$0\" in *cargo) if [ -f run.out ] && ! grep -q '^test result:' run.out; then \
             f=$(sed -n 's/^test \\(.*\\) \\.\\.\\. FAILED$/    \\1/p' run.out); \
             [ -n \"$f\" ] && printf '\\nfailures:\\n%s\\n\\ntest result: FAILED.\\n' \"$f\"; fi;; esac\n\
             [ -f run.signal ] && kill -KILL $$\n\
             if [ -f flake ] && [ $n = 1 ]; then exit 101; fi; exit $(code run.exit);;\n\
         esac\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path.to_string_lossy().into_owned()
}

/// The libtest inventory format: `name: test` lines and the per-binary count.
#[cfg(unix)]
fn toc_of(names: &[&str]) -> String {
    let mut t: String = names.iter().map(|n| format!("{n}: test\n")).collect();
    t.push_str(&format!("\n{} tests, 0 benchmarks\n", names.len()));
    t
}

/// A tree driven by [`fake_cargo`]: `files` written, `.wicked/checks.json` running `argv`.
#[cfg(unix)]
fn write_tree(repo: &Path, argv: &[String], files: &[(&str, &str)]) {
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    std::fs::write(
        repo.join(CONFIG_PATH),
        serde_json::json!({ "test": argv, "timeout_s": 60 }).to_string(),
    )
    .unwrap();
    for f in [
        "run.out",
        "run.err",
        "run.exit",
        "excluded.out",
        "excluded.err",
        "excluded.exit",
        "excluded.sleep",
        "toc.txt",
        "toc.exit",
        "toc.err",
        "flake",
        "flake.excluded",
        "skip.prefix",
        "run.signal",
        "excluded.signal",
    ] {
        let _ = std::fs::remove_file(repo.join(f));
    }
    for (name, body) in files {
        std::fs::write(repo.join(name), body).unwrap();
    }
}

/// A base commit red on `old` (its own inventory lists `old`, `new`, `fine`), a floor context
/// against it, and the fake cargo's argv.
#[cfg(unix)]
fn red_base(tag: &str) -> (PathBuf, FloorContext, Vec<String>) {
    let repo = scratch(tag);
    let argv = vec![fake_cargo(tag), "test".into(), "--no-fail-fast".into()];
    std::fs::write(repo.join(".gitignore"), "tmp/\n").unwrap();
    write_tree(
        &repo,
        &argv,
        &[
            // A real libtest run prints a result line for every test it lists, and its footer.
            (
                "run.out",
                "test new ... ok\ntest fine ... ok\ntest old ... FAILED\n\nfailures:\n    old\n\n\
                 test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
            ),
            ("run.exit", "101"),
            ("excluded.exit", "0"),
            ("toc.txt", &toc_of(&["old", "new", "fine"])),
            // Run alone on the base, only `old` fails.
            ("alone.fail", "old\n"),
        ],
    );
    git_repo_with_commit(&repo);
    let base = commit_guide(&repo, &[libtest_check(&argv)]);
    let ctx = FloorContext {
        stage: FloorStage::Verify,
        base_head: Some(base),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    (repo, ctx, argv)
}

#[cfg(unix)]
include!("../tests/support/libtest_guide.rs");

/// [`libtest_guide`], not yet verified.
#[cfg(unix)]
pub(crate) fn libtest_check(argv: &[String]) -> serde_json::Value {
    libtest_guide(argv, "")
}

/// Commit `checks` as the repo's `.wicked/build-guide.json`, each verified (every probe
/// `pass`) on the current HEAD — so verified on an ancestor of the new base. Returns the new
/// HEAD.
#[cfg(unix)]
pub(crate) fn commit_guide(repo: &Path, checks: &[serde_json::Value]) -> String {
    let head = git(repo, &["rev-parse", "HEAD"]);
    write_guide(repo, checks, &head);
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "the build guide"]);
    git(repo, &["rev-parse", "HEAD"])
}

/// Write `checks` as `repo`'s `.wicked/build-guide.json`, verified (every probe `pass`) on
/// `verified_on`.
#[cfg(unix)]
pub(crate) fn write_guide(repo: &Path, checks: &[serde_json::Value], verified_on: &str) {
    let checks: Vec<serde_json::Value> = checks
        .iter()
        .map(|c| {
            let mut c = c.clone();
            c["verified"] = libtest_guide(&[], verified_on)["verified"].clone();
            c
        })
        .collect();
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    std::fs::write(
        repo.join(crate::floor_guide::GUIDE_PATH),
        serde_json::to_string_pretty(
            &serde_json::json!({"version": 1, "derivedFrom": [], "checks": checks}),
        )
        .unwrap(),
    )
    .unwrap();
}

/// The `checks[].decision` the wire carries for `c`, through the real event serializer.
#[cfg(unix)]
fn wire_decision(c: &CheckRun) -> serde_json::Value {
    crate::event::CoreEvent::RepoChecksEvaluated {
        session: "s".into(),
        ord: 1,
        attempt: 0,
        passed: !c.denies(),
        criterion: CRITERION.into(),
        checks: vec![c.clone()],
        skipped: Vec::new(),
        sandbox_level: "sandboxed".into(),
        sandbox_error: None,
        detect_error: None,
        outcome: "failed".into(),
        floor: "verify".into(),
        claim: None,
        env: None,
        changed: Vec::new(),
        changed_truncated: false,
    }
    .to_json()["checks"][0]["decision"]
        .clone()
}

/// The one check a fake-cargo floor ran (`test`), or `None` when the host has no boundary.
#[cfg(unix)]
fn floor_test(repo: &Path, ctx: &FloorContext) -> Option<(RepoChecksReport, CheckRun)> {
    let report = run_floor(repo, ctx);
    if report.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return None;
    }
    let c = report
        .checks
        .iter()
        .find(|c| c.name == "test")
        .unwrap_or_else(|| panic!("the test check ran: {report:#?}"))
        .clone();
    Some((report, c))
}

/// DONE WHEN 2: every forgery of rounds 1-21 of core#1060, replayed as the HEAD's own text
/// beside an honest new failure (`new`, the excluded run's exit 101), denies — none passes.
/// The text is the creator's to write; the decision reads only exit codes and the base.
#[cfg(unix)]
#[test]
fn every_forgery_of_rounds_1_to_21_denies_or_escalates_never_passes() {
    let _load = seam::pin(0.5, 4);
    let (repo, ctx, argv) = red_base("forge");
    let forgeries: &[(&str, &str, &str)] = &[
        ("r1 bare id across crates", "test old ... FAILED\ntest new ... FAILED\n", ""),
        ("r2 child result line in captured output", "running 2 tests\ntest old ... FAILED\n---- old stdout ----\ntest result: FAILED. 0 passed; 9 failed\ntest new ... FAILED\n", ""),
        ("r3 silent harness + nested runs", "running 1 test\nrunning 3 tests\ntest result: ok. 3 passed; 0 failed\ntest old ... FAILED\n", ""),
        ("r4 coloured + quiet output", "\u{1b}[31mtest new ... FAILED\u{1b}[0m\nnew --- FAILED\n", ""),
        ("r5 merged doctest footer", "all doctests ran in 0.1s; merged doctests compilation took 0.1s\ntest old ... FAILED\n", ""),
        ("r7 unclosed nested run", "running 2 tests\n---- old stdout ----\nrunning 1 test\ntest new ... FAILED\n", ""),
        ("r7 child Running line", "     Running unittests src/lib.rs (target/x)\ntest old ... FAILED\n", "error: test failed, to rerun pass `-p other --lib`\n"),
        ("r8 wrapper showing only failing suites", "test old ... FAILED\n", ""),
        ("r9 oversized nesting", &"running 1 test\n".repeat(50), ""),
        ("r10 section header before failures", "---- new stdout ----\ntest new ... FAILED\nfailures:\n", ""),
        ("r11 raw failures: + section", "test a ... ok\nfailures:\n---- x stdout ----\ntest new ... FAILED\ntest result: FAILED. 1 passed; 1 failed\n", ""),
        ("r12 forged closing summary", "test new ... FAILED\ntest result: ok. 2 passed; 0 failed; 0 ignored\n", ""),
        ("r13 forged stderr selector", "test old ... FAILED\n", "error: test failed, to rerun pass `-p a --lib`\n"),
        ("r14 crash behind a named failure", "test old ... FAILED\n", "process didn't exit successfully: (signal: 11, SIGSEGV)\n"),
        ("r15 unnamed failures / unterminated escape", "\u{1b}[31", "Couldn't compile the test.\n"),
        ("r16 positional-filter confirmation", "running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 5 filtered out\n", ""),
        ("r17 partial CSI eats the t", "\u{1b}[test new ... FAILED\n", ""),
        ("r18 truncated failure set", &"test x ... FAILED\n".repeat(1200), ""),
        ("r19 parameterised CSI/OSC", "\u{1b}[31ERROR\n\u{1b}]8;;x\u{7}FAILED\n", ""),
        ("r20 control characters", "test new\u{0} ... FAILED\r\n\u{9b}test old ... ok\n", ""),
        ("r21 long unfinished line", &"y".repeat(9000), ""),
    ];
    for (round, out, err) in forgeries {
        // The head: `new` really fails, so its excluded run exits 101 — whatever it prints.
        write_tree(
            &repo,
            &argv,
            &[
                ("run.out", out),
                ("run.err", err),
                ("run.exit", "101"),
                ("excluded.out", out),
                ("excluded.err", err),
                ("excluded.exit", "101"),
                ("toc.txt", &toc_of(&["old", "new", "fine"])),
            ],
        );
        let Some((report, c)) = floor_test(&repo, &ctx) else {
            return;
        };
        let d = c.decision.as_ref().expect("a decision");
        assert!(
            matches!(
                d.outcome.as_str(),
                OUTCOME_NEW_FAILURE | OUTCOME_BASE_RED_UNSEPARATED | OUTCOME_FLAKY_TWICE
            ),
            "{round}: {d:?}"
        );
        assert!(!report.passed && c.denies(), "{round}: {d:?}");
        assert_eq!(
            d.excluded,
            s(&["old"]),
            "{round}: only the base's own failure is deselected: {d:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&repo);
}

/// DONE WHEN 3 + the escalation class, guide-driven: base red, cannot separate — with the
/// reason — for a name not unique in the base or head list, a head list that does not run
/// clean, an argv that is not the guide's `run` (a filter; a runner it does not describe), a
/// touched file the guide's `invalidatedBy` names, a deselect that removes more than it was
/// asked to, a truncated base log, a crashed base, and one name under two qualifiers.
#[cfg(unix)]
#[test]
fn imprecise_exclusion_escalates_as_base_red_unseparated() {
    let _load = seam::pin(0.5, 4);
    let (repo, ctx, argv) = red_base("unsep");
    fn head_red<'a>(extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        let mut f = vec![
            ("run.out", "test old ... FAILED\n"),
            ("run.exit", "101"),
            ("excluded.exit", "0"),
        ];
        f.extend_from_slice(extra);
        f
    }
    let unseparated = |repo: &Path, ctx: &FloorContext, why: &str| {
        let Some((report, c)) = floor_test(repo, ctx) else {
            return;
        };
        let d = c.decision.as_ref().expect("a decision");
        assert_eq!(d.outcome, OUTCOME_BASE_RED_UNSEPARATED, "{why}: {d:?}");
        assert!(
            d.reason
                .as_deref()
                .is_some_and(|r| r.starts_with("base red, cannot separate")),
            "{why}: {d:?}"
        );
        assert!(d.excluded.is_empty(), "{why}: nothing ran deselected");
        assert_eq!(c.classification.as_deref(), Some(BASE_RED_UNSEPARATED));
        assert!(!report.passed && c.denies());
        assert!(
            report
                .denial_reason()
                .contains("the operator's floor override is the path through"),
            "{}",
            report.denial_reason()
        );
        assert!(report
            .classification_note()
            .is_some_and(|n| n.contains("cannot separate")));
        assert_eq!(wire_decision(&c)["outcome"], OUTCOME_BASE_RED_UNSEPARATED);
    };
    // (a) The head's inventory lists `old` twice (another binary has it): not unique.
    let toc2 = toc_of(&["old", "old", "fine"]);
    write_tree(&repo, &argv, &head_red(&[("toc.txt", &toc2)]));
    unseparated(&repo, &ctx, "duplicate in the head inventory");
    // (b) A head list that does not run clean.
    write_tree(
        &repo,
        &argv,
        &head_red(&[("toc.txt", &toc_of(&["old", "fine"])), ("toc.exit", "1")]),
    );
    unseparated(&repo, &ctx, "a head list that exits non-zero");
    // (c) A head `.wicked/checks.json` that adds a positional filter changes nothing: the base's
    // guide is the check list, so the check runs the guide's argv (and excludes precisely).
    let filtered: Vec<String> = argv.iter().cloned().chain(["old".to_string()]).collect();
    write_tree(
        &repo,
        &filtered,
        &head_red(&[("toc.txt", &toc_of(&["old", "new", "fine"]))]),
    );
    if let Some((_, c)) = floor_test(&repo, &ctx) {
        assert_eq!(c.argv, argv, "the guide's argv, not the head config's");
        let d = c.decision.as_ref().expect("a decision");
        assert_eq!(d.outcome, OUTCOME_PASS_EXCLUDED, "{d:?}");
    }
    // (d) A touched file the guide's `invalidatedBy` names (a runner config that bends its
    // selection): exclusion is off for this change.
    write_tree(
        &repo,
        &argv,
        &head_red(&[("toc.txt", &toc_of(&["old", "new", "fine"]))]),
    );
    std::fs::create_dir_all(repo.join(".cargo")).unwrap();
    std::fs::write(repo.join(".cargo/config.toml"), "[build]\n").unwrap();
    unseparated(&repo, &ctx, "an invalidatedBy file touched");
    std::fs::remove_dir_all(repo.join(".cargo")).unwrap();
    // (e) A base inventory that lists the failing name twice (a fresh base commit).
    let (repo2, ctx2, argv2) = red_base("unsep-dup");
    write_tree(
        &repo2,
        &argv2,
        &[
            ("run.out", "test old ... FAILED\n"),
            ("run.exit", "101"),
            ("toc.txt", &toc_of(&["old", "old"])),
        ],
    );
    git(&repo2, &["commit", "-qam", "base lists old twice"]);
    let ctx2 = FloorContext {
        base_head: Some(git(&repo2, &["rev-parse", "HEAD"])),
        ..ctx2
    };
    write_tree(
        &repo2,
        &argv2,
        &head_red(&[("toc.txt", &toc_of(&["old", "new"]))]),
    );
    unseparated(&repo2, &ctx2, "duplicate in the base inventory");
    // (e2, review round 15 item 18) The base lists `old` under two qualifiers and the guide's
    // `alone`/`deselect` do not name `{q}`: one id, more than one test.
    let (repo9, ctx9, argv9) = red_base("unsep-two-qs");
    let two_qs = format!(
        "{}     Running tests/b.rs (target/debug/deps/b-2)\nold: test\n",
        toc_of(&["old", "fine"])
    );
    write_tree(
        &repo9,
        &argv9,
        &[
            ("run.out", "test old ... FAILED\n"),
            ("run.exit", "101"),
            ("toc.txt", &two_qs),
            ("alone.fail", "old\n"),
        ],
    );
    git(
        &repo9,
        &["commit", "-qam", "base lists old under two binaries"],
    );
    let ctx9 = FloorContext {
        base_head: Some(git(&repo9, &["rev-parse", "HEAD"])),
        ..ctx9
    };
    write_tree(
        &repo9,
        &argv9,
        &head_red(&[("toc.txt", &toc_of(&["old", "fine"]))]),
    );
    unseparated(&repo9, &ctx9, "one id under two qualifiers on the base");
    let _ = std::fs::remove_dir_all(&repo9);
    // (h, codex r1) A deselect that matches by PREFIX (`old` would also take `old_extra`): the
    // head's list deselecting the base's failure lists fewer than the rest — never skipped.
    let (repo5, ctx5, argv5) = red_base("unsep-prefix");
    write_tree(
        &repo5,
        &argv5,
        &head_red(&[
            ("toc.txt", &toc_of(&["old", "old_extra", "fine"])),
            ("skip.prefix", ""),
        ]),
    );
    unseparated(&repo5, &ctx5, "a deselect by prefix");
    // (i, codex r1) A base whose output was cut at the log cap may have lost a witness.
    let (repo6, ctx6, argv6) = red_base("unsep-cap");
    let long = format!("test old ... FAILED\n{}", "x".repeat(4096));
    write_tree(
        &repo6,
        &argv6,
        &[
            ("run.out", &long),
            ("run.exit", "101"),
            ("toc.txt", &toc_of(&["old", "fine"])),
        ],
    );
    git(&repo6, &["add", "-A"]);
    git(&repo6, &["commit", "-qm", "a long base"]);
    let ctx6 = FloorContext {
        base_head: Some(git(&repo6, &["rev-parse", "HEAD"])),
        ..ctx6
    };
    write_tree(
        &repo6,
        &argv6,
        &[
            ("run.out", "test old ... FAILED\n"),
            ("run.exit", "101"),
            ("excluded.exit", "0"),
            ("toc.txt", &toc_of(&["old", "fine"])),
        ],
    );
    {
        let _cap = seam::cap(1024);
        unseparated(&repo6, &ctx6, "a truncated base log");
    }
    // (j, codex r3) A base binary that stopped early (a crash) printed no result for some
    // listed tests: a FAILED line without its `ok` witness then proves nothing.
    let (repo7, ctx7, argv7) = red_base("unsep-crash");
    write_tree(
        &repo7,
        &argv7,
        &[
            ("run.out", "test new ... FAILED\ntest old ... FAILED\n"),
            ("run.exit", "101"),
            ("toc.txt", &toc_of(&["old", "new", "fine"])),
        ],
    );
    git(&repo7, &["add", "-A"]);
    git(
        &repo7,
        &["commit", "-qm", "a base whose binary stopped early"],
    );
    let ctx7 = FloorContext {
        base_head: Some(git(&repo7, &["rev-parse", "HEAD"])),
        ..ctx7
    };
    write_tree(
        &repo7,
        &argv7,
        &[
            ("run.out", "test new ... FAILED\n"),
            ("run.exit", "101"),
            ("excluded.exit", "0"),
            ("toc.txt", &toc_of(&["old", "new", "fine"])),
        ],
    );
    unseparated(&repo7, &ctx7, "a base binary that stopped early");
    // (k, codex r5) The base's failing `old` (tests/a.rs) is renamed away on the head, and a
    // NEW failing `old` appears in tests/b.rs: one name, under another qualifier — never
    // skipped.
    let (repo8, ctx8, argv8) = red_base("unsep-target");
    let ra = "     Running tests/a.rs (target/debug/deps/a-1)\n";
    let rb = "     Running tests/b.rs (target/debug/deps/b-2)\n";
    let two = |a: &str, b: &str| {
        format!("{ra}{a}: test\n\n1 test, 0 benchmarks\n{rb}{b}: test\n\n1 test, 0 benchmarks\n")
    };
    write_tree(
        &repo8,
        &argv8,
        &[
            (
                "run.out",
                &format!("{ra}test old ... FAILED\n{rb}test fine ... ok\n"),
            ),
            ("run.exit", "101"),
            ("toc.txt", &two("old", "fine")),
            ("toc.err", ""),
        ],
    );
    git(&repo8, &["add", "-A"]);
    git(
        &repo8,
        &["commit", "-qm", "old in tests/a.rs, fine in tests/b.rs"],
    );
    let ctx8 = FloorContext {
        base_head: Some(git(&repo8, &["rev-parse", "HEAD"])),
        ..ctx8
    };
    write_tree(
        &repo8,
        &argv8,
        &[
            ("run.out", &format!("{rb}test old ... FAILED\n")),
            ("run.exit", "101"),
            ("excluded.exit", "0"),
            ("toc.txt", &two("renamed", "old")),
            ("toc.err", ""),
        ],
    );
    unseparated(&repo8, &ctx8, "one name under two qualifiers");
    for r in [repo, repo2, repo5, repo6, repo7, repo8] {
        let _ = std::fs::remove_dir_all(r);
    }
}

/// DONE WHEN 4: a deciding run that fails on an oversubscribed host is re-run ONCE, with the
/// same argv, after waiting for headroom: a pass is `passed on retry (flaky)` with both
/// attempts on the record; failing twice is `flaky_twice` and denies. With headroom, nothing
/// is re-run.
#[cfg(unix)]
#[test]
fn a_flake_under_load_passes_on_its_identical_retry_and_failing_twice_denies() {
    let (repo, ctx, argv) = red_base("flake");
    let cpus = 4;
    // A green base, so the unchanged head run decides.
    write_tree(
        &repo,
        &argv,
        &[("run.exit", "0"), ("toc.txt", &toc_of(&["old"]))],
    );
    git(&repo, &["commit", "-qam", "green base"]);
    let ctx = FloorContext {
        base_head: Some(git(&repo, &["rev-parse", "HEAD"])),
        ..ctx
    };
    write_tree(
        &repo,
        &argv,
        &[
            ("run.exit", "0"),
            ("flake", ""),
            ("toc.txt", &toc_of(&["old"])),
        ],
    );
    // (codex r9) A cargo config that bends `--exact` concerns only the EXCLUDED run: the
    // unchanged command is still retried.
    std::fs::create_dir_all(repo.join(".cargo")).unwrap();
    std::fs::write(
        repo.join(".cargo/config.toml"),
        "[build]\nrustdocflags = [\"-D\", \"warnings\"]\n",
    )
    .unwrap();
    {
        let _load = seam::pin(cpus as f64 * 2.5, cpus);
        let Some((report, c)) = floor_test(&repo, &ctx) else {
            return;
        };
        let d = c.decision.as_ref().unwrap();
        assert_eq!(d.outcome, OUTCOME_PASS, "{d:?}");
        assert!(
            d.flaky && d.reason.as_deref() == Some("passed on retry (flaky)"),
            "{d:?}"
        );
        assert!(report.passed && !c.denies());
        assert_eq!(c.classification.as_deref(), Some(FLAKY_UNDER_LOAD));
        assert_eq!(c.reruns.len(), 1, "one identical retry");
        assert_eq!(c.reruns[0].argv, c.argv, "the SAME argv");
        assert!(c.reruns[0].passed() && !c.passed());
        assert!(
            d.log_refs.iter().any(|l| l.attempt == "head")
                && d.log_refs.iter().any(|l| l.attempt == "retry")
        );
        assert!(report
            .classification_note()
            .unwrap()
            .contains("passed on its identical retry"));
        let w = wire_decision(&c);
        assert_eq!(
            (w["outcome"].as_str(), w["flaky"].as_bool()),
            (Some("pass"), Some(true))
        );
    }
    // Red every time ⇒ failing twice denies.
    write_tree(
        &repo,
        &argv,
        &[("run.exit", "101"), ("toc.txt", &toc_of(&["old"]))],
    );
    {
        let _load = seam::pin(cpus as f64 * 2.5, cpus);
        let Some((report, c)) = floor_test(&repo, &ctx) else {
            return;
        };
        let d = c.decision.as_ref().unwrap();
        assert_eq!(d.outcome, OUTCOME_FLAKY_TWICE, "{d:?}");
        assert!(!d.flaky && !report.passed && c.denies());
        assert_eq!(c.reruns.len(), 1);
        assert_eq!(wire_decision(&c)["outcome"], OUTCOME_FLAKY_TWICE);
    }
    // Headroom ⇒ no retry: a failure is a new failure.
    {
        let _load = seam::pin(1.0, cpus);
        let Some((_, c)) = floor_test(&repo, &ctx) else {
            return;
        };
        assert_eq!(c.decision.as_ref().unwrap().outcome, OUTCOME_NEW_FAILURE);
        assert!(c.reruns.is_empty());
        assert_eq!(wire_decision(&c)["outcome"], OUTCOME_NEW_FAILURE);
    }
    // The EXCLUDED run is the deciding run when exclusion is precise: it is the one retried.
    let (repo2, ctx2, argv2) = red_base("flake-x");
    write_tree(
        &repo2,
        &argv2,
        &[
            ("run.out", "test old ... FAILED\n"),
            ("run.exit", "101"),
            ("excluded.exit", "0"),
            ("flake.excluded", ""),
            ("toc.txt", &toc_of(&["old", "fine"])),
        ],
    );
    {
        let _load = seam::pin(cpus as f64 * 2.5, cpus);
        let Some((report, c)) = floor_test(&repo2, &ctx2) else {
            return;
        };
        let d = c.decision.as_ref().unwrap();
        assert_eq!(
            (d.outcome.as_str(), d.flaky),
            (OUTCOME_PASS_EXCLUDED, true),
            "{d:?}"
        );
        assert!(report.passed);
        assert_eq!(c.reruns.len(), 2, "the excluded run, then its retry");
        assert_eq!(c.reruns[0].argv, c.reruns[1].argv);
    }
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&repo2);
}

/// CI's ubuntu job sets `WICKED_REQUIRE_PROBE_TOOLS`: there a probe fixture that would skip (no
/// pytest, no vitest, no floor sandbox) fails instead.
#[cfg(unix)]
fn tools_required() -> bool {
    std::env::var_os("WICKED_REQUIRE_PROBE_TOOLS").is_some()
}

/// Probe `guide` (written beside the committed tree) against `repo`'s HEAD; `None` when the
/// floor sandbox does not arm here.
#[cfg(unix)]
fn probe(repo: &Path, guide: serde_json::Value) -> Option<crate::floor_probe::ProbeResults> {
    let path = repo.join("probe-guide.json");
    std::fs::write(
        &path,
        serde_json::json!({"version": 1, "checks": [guide]}).to_string(),
    )
    .unwrap();
    match crate::floor_probe::probe_guide(repo, &path, None) {
        Ok(mut all) => {
            let written: crate::floor_guide::BuildGuide =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let v = written.checks[0]
                .verified
                .clone()
                .expect("verified is written");
            assert_eq!(v.base_sha, git(repo, &["rev-parse", "HEAD"]));
            Some(all.remove(0).1)
        }
        Err(e) if e.contains("does not arm") => {
            assert!(!tools_required(), "the floor sandbox is required here: {e}");
            eprintln!("repo_checks: {e} — skipping the probe");
            None
        }
        Err(e) => panic!("the probe ran: {e}"),
    }
}

/// DES-BUILD-GUIDE DONE WHEN 1 (cargo): every probe of a libtest guide passes on a real crate
/// under the real floor sandbox, the mutation probe included.
#[cfg(unix)]
#[test]
fn the_probe_passes_every_piece_of_a_real_cargo_guide() {
    let _load = seam::pin(0.5, 4);
    let repo = scratch("probe-cargo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"probe_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "#[cfg(test)]\nmod t {\n    #[test]\n    fn a() {}\n    #[test]\n    fn b() {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        repo.join(".gitignore"),
        "Cargo.lock\ntmp/\nprobe-guide.json\n",
    )
    .unwrap();
    git_repo_with_commit(&repo);
    let mut g = libtest_check(&s(&["cargo", "test", "--no-fail-fast"]));
    g["probes"] = serde_json::json!({
        "mutation": {
            "file": "tests/zz_mutation.rs",
            "content": "#[test]\nfn floor_mutation() {\n    panic!(\"mutated\");\n}\n\n#[test]\nfn floor_mutation_beside() {}\n",
            "expectId": "floor_mutation"},
        "mutation2": {
            "file": "tests/zz_mutation_two.rs",
            "content": "#[test]\nfn floor_mutation_two() {\n    panic!(\"mutated\");\n}\n",
            "expectId": "floor_mutation_two"}});
    let Some(results) = probe(&repo, g) else {
        return;
    };
    for p in crate::floor_guide::REQUIRED_PROBES {
        assert!(
            results
                .get(p)
                .is_some_and(|r| crate::floor_guide::passed(r)),
            "{p}: {results:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&repo);
}

/// DES-BUILD-GUIDE DONE WHEN 1 (pytest, JUnit via `--junitxml`): every probe passes. Skipped
/// where pytest is not on `PATH`.
#[cfg(unix)]
#[test]
fn the_probe_passes_every_piece_of_a_junit_pytest_guide() {
    let Some(py) = crate::validator::find_on_path("pytest") else {
        assert!(
            !tools_required(),
            "pytest is required here (WICKED_REQUIRE_PROBE_TOOLS)"
        );
        eprintln!("repo_checks: pytest not on PATH — skipping the pytest guide probe");
        return;
    };
    let py = py.to_string_lossy().into_owned();
    let _load = seam::pin(0.5, 4);
    let repo = scratch("probe-pytest");
    std::fs::create_dir_all(repo.join("tests")).unwrap();
    std::fs::write(
        repo.join("tests/test_a.py"),
        "def test_x():\n    assert True\n\n\ndef test_y():\n    assert True\n",
    )
    .unwrap();
    std::fs::write(
        repo.join(".gitignore"),
        "tmp/\n__pycache__/\nprobe-guide.json\nfloor-junit.xml\n",
    )
    .unwrap();
    git_repo_with_commit(&repo);
    let run = [py.as_str(), "-p", "no:cacheprovider"];
    let g = serde_json::json!({
        "name": "test", "run": run, "failedExit": [1],
        "results": {"kind": "junit", "id": ["file", "name"],
                    "how": {"argvAppend": ["--junitxml=floor-junit.xml", "-o", "junit_family=xunit1"],
                            "path": "floor-junit.xml"}},
        "list": {"run": [py.as_str(), "-p", "no:cacheprovider", "--collect-only", "-q"],
                 "item": r"^(?P<id>\S+::\S+)$"},
        "alone": {"run": [py.as_str(), "-p", "no:cacheprovider", "{id}"]},
        "deselect": {"append": ["--deselect={id}"]},
        "probes": {
            "mutation": {
                "file": "tests/test_zz_mutation.py",
                "content": "def test_mutated():\n    assert False\n\n\ndef test_beside():\n    assert True\n",
                "expectId": "tests/test_zz_mutation.py::test_mutated"},
            "mutation2": {
                "file": "tests/test_zz_mutation_two.py",
                "content": "def test_mutated_two():\n    assert False\n",
                "expectId": "tests/test_zz_mutation_two.py::test_mutated_two"}},
    });
    let Some(results) = probe(&repo, g) else {
        return;
    };
    for p in crate::floor_guide::REQUIRED_PROBES {
        assert!(
            results
                .get(p)
                .is_some_and(|r| crate::floor_guide::passed(r)),
            "{p}: {results:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&repo);
}

/// DES-BUILD-GUIDE (vitest, JUnit reporter; garden's seed, wicked-garden#1332): every probe passes,
/// with `{id:re}` running one test and `{ids:re|}` deselecting several in one pattern. Skipped
/// unless `WICKED_TEST_VITEST` names a vitest binary.
#[cfg(unix)]
#[test]
fn the_probe_passes_every_piece_of_a_junit_vitest_guide() {
    let Some(vt) = std::env::var("WICKED_TEST_VITEST")
        .ok()
        .filter(|v| Path::new(v).exists())
    else {
        assert!(
            !tools_required(),
            "vitest is required here (WICKED_REQUIRE_PROBE_TOOLS)"
        );
        eprintln!("repo_checks: WICKED_TEST_VITEST unset — skipping the vitest guide probe");
        return;
    };
    let _load = seam::pin(0.5, 4);
    let repo = scratch("probe-vitest");
    std::fs::write(
        repo.join("a.test.mjs"),
        "test(\"adds\", () => {\n  expect(1 + 1).toBe(2);\n});\n\ntest(\"subtracts\", () => {\n  expect(2 - 1).toBe(1);\n});\n",
    )
    .unwrap();
    std::fs::write(
        repo.join(".gitignore"),
        "tmp/\nprobe-guide.json\nfloor-junit.xml\nnode_modules/\n",
    )
    .unwrap();
    git_repo_with_commit(&repo);
    let vt = vt.as_str();
    let failing = |name: &str| format!("test(\"{name}\", () => {{\n  expect(1).toBe(2);\n}});\n");
    let g = serde_json::json!({
        "name": "test", "run": [vt, "run", "--globals"], "failedExit": [1],
        "results": {"kind": "junit", "id": ["name"],
                    "how": {"argvAppend": ["--reporter=junit", "--outputFile=floor-junit.xml"],
                            "path": "floor-junit.xml"}},
        "list": {"run": [vt, "list", "--globals"], "item": r"^\S+ > (?P<id>.+)$"},
        "alone": {"run": [vt, "run", "--globals", "--testNamePattern=^{id:re}$"]},
        "deselect": {"append": ["--testNamePattern=^(?!(?:{ids:re|})$)"]},
        "version": {"run": [vt, "--version"]},
        "probes": {
            "mutation": {"file": "zz.test.mjs", "content": failing("mutated"), "expectId": "mutated"},
            "mutation2": {"file": "zz2.test.mjs", "content": failing("mutated two"),
                          "expectId": "mutated two"}},
    });
    let Some(results) = probe(&repo, g) else {
        return;
    };
    for p in crate::floor_guide::REQUIRED_PROBES
        .iter()
        .chain(&["version"])
    {
        assert!(
            results
                .get(*p)
                .is_some_and(|r| crate::floor_guide::passed(r)),
            "{p}: {results:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&repo);
}

/// DES-BUILD-GUIDE DONE WHEN 3-5: the floor reads the BASE's guide (a head edit to the guide
/// does not apply to its own run); a guide whose probe failed, or one a base change made stale
/// (a `derivedFrom` file changed since it was verified), never excludes — the check
/// escalates, and `decision.guide` says why.
#[cfg(unix)]
#[test]
fn the_floor_reads_the_base_guide_and_never_relies_on_a_failed_or_stale_one() {
    let _load = seam::pin(0.5, 4);
    let head_red = |repo: &Path, argv: &[String]| {
        write_tree(
            repo,
            argv,
            &[
                ("run.out", "test old ... FAILED\n"),
                ("run.exit", "101"),
                ("excluded.exit", "0"),
                ("toc.txt", &toc_of(&["old", "new", "fine"])),
            ],
        );
    };
    // (3) The head rewrites the guide into nonsense: the base's still decides.
    let (repo, ctx, argv) = red_base("guide-head-edit");
    head_red(&repo, &argv);
    std::fs::write(
        repo.join(crate::floor_guide::GUIDE_PATH),
        "{\"version\": 9}",
    )
    .unwrap();
    let Some((report, c)) = floor_test(&repo, &ctx) else {
        return;
    };
    let d = c.decision.as_ref().expect("a decision");
    assert_eq!(d.outcome, OUTCOME_PASS_EXCLUDED, "{d:?}");
    assert!(report.passed);
    let g = d
        .guide
        .as_ref()
        .expect("the guide's state rides the decision");
    assert!(g.present && g.usable && g.reason.is_none(), "{g:?}");
    // (4) A probe that did not pass: the check escalates, the reason names the probe.
    let (repo2, _, argv2) = red_base("guide-failed-probe");
    let mut guide = libtest_check(&argv2);
    guide["verified"] = serde_json::json!({});
    write_guide(&repo2, &[guide], &git(&repo2, &["rev-parse", "HEAD~1"]));
    let path = repo2.join(crate::floor_guide::GUIDE_PATH);
    let raw = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        raw.replace(
            "\"mutation\": \"pass\"",
            "\"mutation\": \"fail: the run exited 0\"",
        ),
    )
    .unwrap();
    git(
        &repo2,
        &["commit", "-qam", "a guide whose mutation probe failed"],
    );
    let ctx2 = FloorContext {
        base_head: Some(git(&repo2, &["rev-parse", "HEAD"])),
        stage: FloorStage::Verify,
        git_dir: Some(repo2.join(".git")),
        ..FloorContext::default()
    };
    head_red(&repo2, &argv2);
    let (report, c) = floor_test(&repo2, &ctx2).unwrap();
    let d = c.decision.as_ref().expect("a decision");
    assert_eq!(d.outcome, OUTCOME_BASE_RED_UNSEPARATED, "{d:?}");
    assert!(!report.passed && d.excluded.is_empty());
    let g = d.guide.as_ref().unwrap();
    assert!(g.present && !g.usable, "{g:?}");
    assert!(
        g.reason
            .as_deref()
            .is_some_and(|r| r.contains("probe `mutation`")),
        "{g:?}"
    );
    // (5) Stale: the guide was derived from `.wicked/checks.json`, which the base changed
    // since it was verified.
    let (repo3, _, argv3) = red_base("guide-stale");
    let raw = std::fs::read_to_string(repo3.join(crate::floor_guide::GUIDE_PATH)).unwrap();
    std::fs::write(
        repo3.join(crate::floor_guide::GUIDE_PATH),
        raw.replace(
            "\"derivedFrom\": []",
            "\"derivedFrom\": [\".wicked/checks.json\"]",
        ),
    )
    .unwrap();
    git(
        &repo3,
        &["commit", "-qam", "the guide names what it was derived from"],
    );
    let mut cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(repo3.join(CONFIG_PATH)).unwrap()).unwrap();
    cfg["timeout_s"] = serde_json::json!(61);
    std::fs::write(repo3.join(CONFIG_PATH), cfg.to_string()).unwrap();
    git(&repo3, &["commit", "-qam", "the checks config changed"]);
    let ctx3 = FloorContext {
        base_head: Some(git(&repo3, &["rev-parse", "HEAD"])),
        stage: FloorStage::Verify,
        git_dir: Some(repo3.join(".git")),
        ..FloorContext::default()
    };
    write_tree(
        &repo3,
        &argv3,
        &[
            ("run.out", "test old ... FAILED\n"),
            ("run.exit", "101"),
            ("excluded.exit", "0"),
            ("toc.txt", &toc_of(&["old", "new", "fine"])),
        ],
    );
    std::fs::write(repo3.join(CONFIG_PATH), cfg.to_string()).unwrap();
    let (report, c) = floor_test(&repo3, &ctx3).unwrap();
    let d = c.decision.as_ref().expect("a decision");
    assert_eq!(d.outcome, OUTCOME_BASE_RED_UNSEPARATED, "{d:?}");
    assert!(!report.passed);
    assert!(
        d.guide
            .as_ref()
            .and_then(|g| g.reason.as_deref())
            .is_some_and(|r| r.starts_with("stale")),
        "{d:?}"
    );
    for r in [repo, repo2, repo3] {
        let _ = std::fs::remove_dir_all(r);
    }
}

/// Every outcome serializes on the wire as `checks[].decision` (the coordinator's addendum):
/// `pass_excluded` (DONE WHEN 1), `timed_out` (an excluded run past its bound) and
/// `could_not_run`; the others are asserted in their own DONE WHEN tests.
#[cfg(unix)]
#[test]
fn each_outcome_rides_the_wire_as_a_typed_decision() {
    let _load = seam::pin(0.5, 4);
    let (repo, ctx, argv) = red_base("wire");
    write_tree(
        &repo,
        &argv,
        &[
            ("run.out", "test old ... FAILED\n"),
            ("run.exit", "101"),
            ("excluded.exit", "0"),
            ("toc.txt", &toc_of(&["old", "new", "fine"])),
        ],
    );
    let Some((report, c)) = floor_test(&repo, &ctx) else {
        return;
    };
    let w = wire_decision(&c);
    assert_eq!(w["outcome"], OUTCOME_PASS_EXCLUDED, "{w}");
    assert_eq!(w["baseSha"], serde_json::json!(ctx.base_head));
    assert_eq!(w["baseFailing"], serde_json::json!(["old"]));
    assert_eq!(w["excluded"], serde_json::json!(["old"]));
    assert_eq!(w["reason"], "1 pre-existing excluded: old");
    let attempts: Vec<&str> = w["logRefs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["attempt"].as_str().unwrap())
        .collect();
    for a in ["head", "base", "toc-base", "toc-head", "excluded"] {
        assert!(attempts.contains(&a), "{a} in {attempts:?}");
    }
    for r in w["logRefs"].as_array().unwrap() {
        assert!(
            r["sha256"].as_str().is_some_and(|h| h.len() == 64)
                && r["bytes"].is_u64()
                && r["stream"].is_string(),
            "{r}"
        );
    }
    assert!(report.passed);
    // timed_out: the excluded run outlives its bound.
    std::fs::write(
        repo.join(CONFIG_PATH),
        serde_json::json!({ "test": argv, "timeout_s": 2 }).to_string(),
    )
    .unwrap();
    std::fs::write(repo.join("excluded.sleep"), "30").unwrap();
    let Some((report, c)) = floor_test(&repo, &ctx) else {
        return;
    };
    assert_eq!(wire_decision(&c)["outcome"], OUTCOME_TIMED_OUT);
    assert!(
        !report.passed && report.timed_out() && report.denial_source() == DENIAL_SOURCE_TIMEOUT
    );
    // could_not_run: an exit outside the guide's `failedExit` (a crash).
    std::fs::write(
        repo.join(CONFIG_PATH),
        serde_json::json!({ "test": argv }).to_string(),
    )
    .unwrap();
    let _ = std::fs::remove_file(repo.join("excluded.sleep"));
    std::fs::write(repo.join("run.exit"), "2").unwrap();
    let Some((report, c)) = floor_test(&repo, &ctx) else {
        return;
    };
    assert_eq!(wire_decision(&c)["outcome"], OUTCOME_COULD_NOT_RUN);
    assert!(!report.passed);
    // A passing check: `pass`, its own two streams on the record.
    std::fs::write(
        repo.join(CONFIG_PATH),
        serde_json::json!({ "test": argv }).to_string(),
    )
    .unwrap();
    std::fs::write(repo.join("run.exit"), "0").unwrap();
    let Some((_, c)) = floor_test(&repo, &ctx) else {
        return;
    };
    let w = wire_decision(&c);
    assert_eq!(
        (w["outcome"].as_str(), w["logRefs"].as_array().map(Vec::len)),
        (Some("pass"), Some(2))
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// A real cargo crate: `a` red on the base, `b` and the doctests green.
#[cfg(unix)]
fn real_crate(tag: &str) -> (PathBuf, FloorContext) {
    let repo = scratch(tag);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"floorv2_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(repo.join(".gitignore"), "Cargo.lock\ntmp/\n").unwrap();
    std::fs::write(repo.join("src/lib.rs"), real_lib(true, false)).unwrap();
    git_repo_with_commit(&repo);
    // The guide: its one check is the floor's.
    let base = commit_guide(
        &repo,
        &[libtest_check(&s(&["cargo", "test", "--no-fail-fast"]))],
    );
    let ctx = FloorContext {
        stage: FloorStage::Verify,
        base_head: Some(base),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    (repo, ctx)
}

/// `a` red when asked, `b` red when asked, and a doctest that always passes.
#[cfg(unix)]
fn real_lib(a_red: bool, b_red: bool) -> String {
    let body = |red: bool| {
        if red {
            "{\n        assert!(false, \"BOOM\");\n    }"
        } else {
            "{}"
        }
    };
    format!(
        "/// ```\n/// assert_eq!(floorv2_fixture::two(), 2);\n/// ```\npub fn two() -> u32 {{\n    2\n}}\n\n\
         #[cfg(test)]\nmod t {{\n    #[test]\n    fn a() {}\n    #[test]\n    fn b() {}\n}}\n",
        body(a_red),
        body(b_red)
    )
}

/// The `cargo test` check a real-cargo floor ran (the full suite or the derived set).
#[cfg(unix)]
fn cargo_check(report: &RepoChecksReport) -> &CheckRun {
    report
        .checks
        .iter()
        .find(|c| c.argv.get(1).map(String::as_str) == Some("test"))
        .unwrap_or_else(|| panic!("a cargo test check ran: {report:#?}"))
}

/// (codex r13, r14) Real cargo, `--test-threads=1`: `fine` depends on order (it passes in the
/// suite, fails alone) and spawns a child that inherits stdout, SPLITTING its own result line
/// (`test t::fine ... x` then `ok`); `old` prints a child-style `test t::fine ... FAILED` line
/// and fails. Only libtest's own `failures:` list names a base failure (`t::old`), so the head
/// breaking `fine` denies.
#[cfg(unix)]
#[test]
fn an_order_dependent_test_the_base_passed_is_never_excluded() {
    let _load = seam::pin(0.5, 4);
    let (repo, _) = real_crate("realv2-order");
    let lib = |fine: &str| {
        format!(
            "#[cfg(test)]\nmod t {{\n    use std::sync::atomic::{{AtomicBool, Ordering}};\n\n    \
             static READY: AtomicBool = AtomicBool::new(false);\n\n    #[test]\n    fn a_setup() {{\n        \
             READY.store(true, Ordering::SeqCst);\n    }}\n\n    #[test]\n    fn fine() {{\n        \
             {fine}\n    }}\n\n    #[test]\n    fn old() {{\n        println!(\"test t::fine ... FAILED\");\n        \
             panic!(\"old\");\n    }}\n}}\n"
        )
    };
    // spawn-audit: test-only — fixture SOURCE text: the fixture crate's own test spawns a child
    // that inherits stdout, to split its libtest result line.
    let fine = "let _ = std::process::Command::new(\"echo\").arg(\"x\").status();\n        \
                assert!(READY.load(Ordering::SeqCst));";
    std::fs::write(repo.join("src/lib.rs"), lib(fine)).unwrap();
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    std::fs::write(
        repo.join(CONFIG_PATH),
        r#"{"test":["cargo","test","--no-fail-fast","--","--test-threads=1"]}"#,
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "an order-dependent base"]);
    commit_guide(
        &repo,
        &[libtest_check(&s(&[
            "cargo",
            "test",
            "--no-fail-fast",
            "--",
            "--test-threads=1",
        ]))],
    );
    let ctx = FloorContext {
        stage: FloorStage::Verify,
        base_head: Some(git(&repo, &["rev-parse", "HEAD"])),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    std::fs::write(repo.join("src/lib.rs"), lib("panic!(\"broken\");")).unwrap();
    let report = run_floor(&repo, &ctx);
    if report.sandbox_error.is_some() {
        return;
    }
    let c = cargo_check(&report);
    let d = c.decision.as_ref().expect("decided");
    assert_eq!(d.base_failing, s(&["t::old"]), "{c:#?}");
    assert_eq!(d.outcome, OUTCOME_NEW_FAILURE, "{c:#?}");
    assert!(!report.passed);
    let _ = std::fs::remove_dir_all(&repo);
}

/// (agy r14) Real cargo: `smoke` passes in the lib and fails in an integration test — one
/// name in two binaries. libtest's own list names the failing one, the base inventory lists
/// the name twice, so the floor cannot separate it: `base_red_unseparated`, the failure on
/// the record.
#[cfg(unix)]
#[test]
fn a_name_failing_in_one_binary_and_passing_in_another_cannot_be_separated() {
    let _load = seam::pin(0.5, 4);
    let (repo, _) = real_crate("realv2-smoke");
    std::fs::write(repo.join("src/lib.rs"), "#[test]\nfn smoke() {}\n").unwrap();
    std::fs::create_dir_all(repo.join("tests")).unwrap();
    std::fs::write(
        repo.join("tests/it.rs"),
        "#[test]\nfn smoke() {\n    panic!(\"it\");\n}\n",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "smoke in two binaries"]);
    let ctx = FloorContext {
        stage: FloorStage::Verify,
        base_head: Some(git(&repo, &["rev-parse", "HEAD"])),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    std::fs::write(repo.join("README.md"), "head\n").unwrap();
    let report = run_floor(&repo, &ctx);
    if report.sandbox_error.is_some() {
        return;
    }
    let c = cargo_check(&report);
    let d = c.decision.as_ref().expect("decided");
    assert_eq!(d.base_failing, s(&["smoke"]), "{c:#?}");
    assert_eq!(d.outcome, OUTCOME_BASE_RED_UNSEPARATED, "{c:#?}");
    assert!(
        d.reason
            .as_deref()
            .is_some_and(|r| r.contains("listed under more than one qualifier")),
        "{d:?}"
    );
    assert!(!report.passed);
    let _ = std::fs::remove_dir_all(&repo);
}

/// (codex r10-r11, ported) Package `b` compiles in `a`'s source by `include_str!`, with no
/// cargo edge. A base with a guide (and no `affected` piece) runs the full check at the
/// creator and at verify, so `b`'s regression denies at both.
#[cfg(unix)]
#[test]
fn an_include_str_across_packages_denies_at_the_creator_and_at_verify() {
    let _load = seam::pin(0.5, 4);
    let repo = scratch("include-str");
    for (name, body) in [
        ("a", "pub fn one() -> u32 {\n    1\n}\n"),
        (
            "b",
            "#[cfg(test)]\nmod t {\n    #[test]\n    fn reads_a() {\n        assert!(include_str!(\"../../a/src/lib.rs\").contains(\"fn one\"));\n    }\n}\n",
        ),
    ] {
        std::fs::create_dir_all(repo.join(name).join("src")).unwrap();
        std::fs::write(
            repo.join(name).join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        std::fs::write(repo.join(name).join("src/lib.rs"), body).unwrap();
    }
    std::fs::write(
        repo.join("Cargo.toml"),
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::write(repo.join(".gitignore"), "Cargo.lock\ntmp/\ntarget/\n").unwrap();
    git_repo_with_commit(&repo);
    let base = commit_guide(
        &repo,
        &[
            libtest_check(&s(&["cargo", "test", "--no-fail-fast"])),
            libtest_check(&s(&["cargo", "test", "--locked", "--no-fail-fast"])),
        ],
    );
    let ctx = |stage| FloorContext {
        stage,
        base_head: Some(base.clone()),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    std::fs::write(
        repo.join("a/src/lib.rs"),
        "pub fn two() -> u32 {\n    2\n}\n",
    )
    .unwrap();
    for stage in [FloorStage::Creator, FloorStage::Verify] {
        let report = run_floor(&repo, &ctx(stage));
        if report.sandbox_error.is_some() {
            return;
        }
        assert!(
            report.checks.iter().all(|c| c.name != "test_targeted"),
            "{stage:?}: a guided base never narrows without `affected`: {report:#?}"
        );
        assert!(
            !report.passed,
            "{stage:?}: `b`'s regression denies: {report:#?}"
        );
    }
    let _ = std::fs::remove_dir_all(&repo);
}

/// DONE WHEN 1 + 6 + 7, on real cargo under the real floor sandbox: base red on `a`, head red
/// on `a` only ⇒ passes with "1 pre-existing excluded: t::a"; head also red on `b` ⇒ a new
/// failure, denied. The doctest runs inside the floor's boundary on every run. A log range
/// fetched by its receipt ref verifies against the hash.
#[cfg(unix)]
#[test]
fn real_cargo_excludes_the_base_failure_and_denies_a_new_one() {
    let _load = seam::pin(0.5, 4);
    let (repo, ctx) = real_crate("realv2");
    // Head red on `a` only (an unrelated edit, so the tree changed).
    std::fs::write(
        repo.join("src/lib.rs"),
        real_lib(true, false) + "\n// edited\n",
    )
    .unwrap();
    // The tree is unconfigured, so verify runs its full suite.
    let report = run_floor(&repo, &ctx);
    if report.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the real floor cannot run");
        return;
    }
    let c = cargo_check(&report);
    let d = c.decision.as_ref().expect("decided");
    assert_eq!(d.outcome, OUTCOME_PASS_EXCLUDED, "{c:#?}");
    assert_eq!(d.reason.as_deref(), Some("1 pre-existing excluded: t::a"));
    assert_eq!(d.excluded, s(&["t::a"]));
    assert_eq!(d.base_failing, s(&["t::a"]));
    assert_eq!(d.base_sha, ctx.base_head);
    assert!(report.passed, "{report:#?}");
    assert_eq!(c.pre_existing, s(&["t::a"]));
    let excluded = &c.reruns[0];
    assert!(
        excluded.argv.ends_with(&s(&["--skip=t::a", "--exact"])),
        "{:?}",
        excluded.argv
    );
    assert!(excluded.passed());
    // DONE WHEN 7: the doctest RAN inside the floor's sandbox (rustdoc made its temp dir).
    let store = report_logs(&repo, &ctx);
    let out = crate::floor_log::text(&store, &excluded.logs[0]).unwrap();
    assert!(
        out.contains("test src/lib.rs - two (line 1) ... ok"),
        "{out}"
    );
    let err = crate::floor_log::text(&store, &excluded.logs[1]).unwrap();
    assert!(
        !err.contains("failed to create temporary directory"),
        "{err}"
    );
    // DONE WHEN 6: the receipt's ref fetches a verifiable range.
    let r = &d
        .log_refs
        .iter()
        .find(|l| l.attempt == "excluded" && l.stream == "stdout")
        .unwrap();
    let t = crate::floor_log::toc(&store, r, &[]).unwrap();
    let (a, b) = t.lines[t.lines.len() - 1];
    assert!(crate::floor_log::read_range(&store, r, a, b).is_ok());
    assert_eq!(wire_decision(c)["outcome"], OUTCOME_PASS_EXCLUDED);

    // Head red on `a` AND `b`: the excluded run still fails ⇒ a new failure, denied.
    std::fs::write(repo.join("src/lib.rs"), real_lib(true, true)).unwrap();
    let report = run_floor(&repo, &ctx);
    let c = cargo_check(&report);
    let d = c.decision.as_ref().unwrap();
    assert!(
        matches!(
            d.outcome.as_str(),
            OUTCOME_NEW_FAILURE | OUTCOME_FLAKY_TWICE
        ),
        "{c:#?}"
    );
    assert_eq!(d.excluded, s(&["t::a"]));
    assert!(!report.passed && c.denies());
    assert_eq!(c.classification.as_deref(), Some(REGRESSION));
    assert!(
        c.regressions.contains(&"test t::b".to_string()),
        "annotated: {:?}",
        c.regressions
    );
    let denial = report.denial_reason();
    assert!(
        denial.contains("NEW FAILURE") && denial.contains("t::a"),
        "{denial}"
    );
    assert_eq!(
        wire_decision(c)["outcome"].as_str(),
        Some(d.outcome.as_str())
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// The run's log store a test floor over `repo` wrote to.
#[cfg(unix)]
fn report_logs(repo: &Path, ctx: &FloorContext) -> PathBuf {
    crate::floor_log::run_dir(
        &check_cache_root()
            .unwrap()
            .parent()
            .unwrap()
            .join(crate::floor_log::LOG_SUBDIR),
        ctx.run_id.as_deref(),
        repo,
    )
}

/// DONE WHEN 7: a real doctest runs under the real macOS floor sandbox — the production path
/// (`run_floor`) and the test seam, which now arms the private `TMPDIR` as production does
/// (rustdoc creates its temp dir there; the seam used to leave it out, so no doctest ran).
#[cfg(unix)]
#[test]
fn doctests_run_under_the_real_floor_sandbox() {
    let _load = seam::pin(0.5, 4);
    let (repo, ctx) = real_crate("doctest");
    std::fs::write(repo.join("src/lib.rs"), real_lib(false, false)).unwrap();
    let report = run_floor(
        &repo,
        &FloorContext {
            base_head: None,
            git_dir: None,
            ..ctx.clone()
        },
    );
    if report.sandbox_error.is_some() {
        return;
    }
    let c = cargo_check(&report);
    let store = report_logs(&repo, &ctx);
    let out = crate::floor_log::text(&store, &c.logs[0]).unwrap();
    assert!(
        c.passed() && out.contains("test src/lib.rs - two (line 1) ... ok"),
        "{c:#?}\n{out}"
    );
    let seam = run_with_sandbox(&repo, sandbox_for(&repo));
    let c = cargo_check(&seam);
    assert!(c.passed(), "the test seam arms the private TMPDIR: {c:#?}");
    let _ = std::fs::remove_dir_all(&repo);
}

/// A libtest-shaped stand-in for the probe: tests `a` and `b`, plus `m` / `m2` while the
/// mutation files `zz_m` / `zz_m2` exist (they fail). `--list` lists, `--skip=<id>` deselects,
/// `-- <id> --exact` runs one. Markers: `flaky.list` fails the first list of a probe scratch,
/// `broken.alone` fails `a` run alone, `red.base` fails `a` always, `alone.all` makes a one-test
/// run run them all, `plant.link` plants `zz_m` as a link to `$OUTSIDE`, `swallow` exits 0 on a
/// failure, `dup.list` lists `a` twice.
#[cfg(unix)]
const STAND_IN: &str = r#"mode=run; name=; skip=' '
for a in "$@"; do case "$a" in --list) mode=list;; --skip=*) skip="$skip${a#--skip=} ";; --|--exact) ;; *) name=$a;; esac; done
all="a b"; [ -f zz_m ] && [ ! -L zz_m ] && all="$all m"; [ -f zz_m2 ] && all="$all m2"
if [ -n "$name" ] && [ ! -f alone.all ]; then case " $all " in *" $name "*) all=$name;; *) all=;; esac; fi
[ -n "$name" ] && [ -f broken.alone ] && [ "$name" = a ] && exit 1
[ -n "$LEAK" ] && [ $mode = list ] && exit 3
[ -f plant.link ] && [ ! -L zz_m ] && ln -s "$OUTSIDE" zz_m
case $mode in
  list) if [ -f flaky.list ] && [ ! -f "$TMPDIR/listed" ]; then : > "$TMPDIR/listed"; exit 1; fi
    for t in $all; do case "$skip" in *" $t "*) continue;; esac; echo "$t: test"; done
    [ -f dup.list ] && case "$skip" in *" a "*) ;; *) echo "a: test";; esac; exit 0;;
  *) f=; for t in $all; do case "$skip" in *" $t "*) continue;; esac; case $t in m|m2) f="$f $t";; a) [ -f red.base ] && f="$f a";; esac; done
    if [ -z "$f" ]; then echo 'test result: ok.'; exit 0; fi
    printf '\nfailures:\n'; for t in $f; do echo "    $t"; done; printf '\ntest result: FAILED.\n'
    [ -f swallow ] && exit 0; exit 101;;
esac
"#;

/// A guide for [`STAND_IN`] (`runner.sh`), with a `version` command.
#[cfg(unix)]
fn stand_in_guide() -> serde_json::Value {
    serde_json::json!({
        "name": "test", "run": ["sh", "runner.sh"], "failedExit": [101],
        "results": {"kind": "text"},
        "parser": {"blocks": [{"start": "^failures:$", "item": r"^    (?P<id>\S+)$",
                               "end": "^test result: FAILED"}]},
        "list": {"run": ["sh", "runner.sh", "--list"], "item": r"^(?P<id>\S+): test$"},
        "alone": {"run": ["sh", "runner.sh", "--", "{id}", "--exact"]},
        "deselect": {"append": ["--skip={id}"]},
        "version": {"run": ["sh", "-c", "echo; echo stand-in 1.0"]},
        "probes": {
            "mutation": {"file": "zz_m", "content": "m\n", "expectId": "m"},
            "mutation2": {"file": "zz_m2", "content": "m2\n", "expectId": "m2"}},
    })
}

/// Rulings 2 and 3: the probe records the `version` command's first line as
/// `verified.runnerVersion`; a probe that fails once and passes its one retry is
/// `pass (retried)` (relied on); one that fails its retry too is `fail: … (retried)`.
#[cfg(unix)]
#[test]
fn a_failed_probe_is_retried_once_and_the_runner_version_is_recorded() {
    let _load = seam::pin(0.5, 4);
    let repo = scratch("probe-retry");
    std::fs::write(repo.join("runner.sh"), STAND_IN).unwrap();
    std::fs::write(repo.join("flaky.list"), "").unwrap();
    std::fs::write(repo.join(".gitignore"), "tmp/\nprobe-guide.json\n").unwrap();
    git_repo_with_commit(&repo);
    let Some(results) = probe(&repo, stand_in_guide()) else {
        return;
    };
    for p in crate::floor_guide::REQUIRED_PROBES
        .iter()
        .chain(&["version"])
    {
        // The first list failed: the whole probe re-ran, and its results are the result.
        assert_eq!(
            results.get(*p).map(String::as_str),
            Some("pass (retried)"),
            "{p}: {results:?}"
        );
    }
    let written: crate::floor_guide::BuildGuide =
        serde_json::from_str(&std::fs::read_to_string(repo.join("probe-guide.json")).unwrap())
            .unwrap();
    assert_eq!(
        written.checks[0].verified.as_ref().unwrap().runner_version,
        "stand-in 1.0"
    );
    // Run alone, a passing test fails every time: its retry fails too.
    std::fs::remove_file(repo.join("flaky.list")).unwrap();
    std::fs::write(repo.join("broken.alone"), "").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "alone is broken"]);
    let results = probe(&repo, stand_in_guide()).unwrap();
    let alone = results.get("alone").cloned().unwrap_or_default();
    assert!(
        alone.starts_with("fail: ") && alone.ends_with(" (retried)"),
        "{results:?}"
    );
    assert_eq!(
        results.get("list").map(String::as_str),
        Some("pass (retried)")
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// Review round 15 (items 1, 3, 4): the probes need a green base; a one-test run that runs more
/// than its test fails `alone`; a mutation never lands outside the copy — not through `..`, not
/// through a link the runner planted.
#[cfg(unix)]
#[test]
fn the_probe_needs_a_green_base_an_isolated_alone_and_confined_mutations() {
    let _load = seam::pin(0.5, 4);
    let repo = scratch("probe-strict");
    let outside = scratch("probe-strict-outside").join("marker");
    std::fs::write(&outside, "untouched").unwrap();
    std::fs::write(repo.join("runner.sh"), STAND_IN).unwrap();
    std::fs::write(repo.join(".gitignore"), "tmp/\nprobe-guide.json\n").unwrap();
    git_repo_with_commit(&repo);
    let mut g = stand_in_guide();
    // (round 16, item 7) The results env reaches the results runs only: a list that saw it fails.
    g["results"]["how"] =
        serde_json::json!({"env": {"OUTSIDE": outside.to_string_lossy(), "LEAK": "1"}});
    let with = |marker: &str, g: &serde_json::Value| {
        std::fs::write(repo.join(marker), "").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", marker]);
        let r = probe(&repo, g.clone());
        std::fs::remove_file(repo.join(marker)).unwrap();
        git(&repo, &["commit", "-qam", "cleared"]);
        r
    };
    let Some(r) = with("red.base", &g) else {
        return;
    };
    assert!(r["run"].starts_with("fail: needs a green base"), "{r:?}");
    for p in ["ids", "alone", "deselect", "mutation", "multi"] {
        assert_eq!(r[p], "fail: needs a green base (retried)", "{p}: {r:?}");
    }
    let r = with("alone.all", &g).unwrap();
    assert!(r["alone"].starts_with("fail: `m` alone"), "{r:?}");
    let r = with("plant.link", &g).unwrap();
    assert!(r["mutation"].contains("passes through a symlink"), "{r:?}");
    let mut esc = g.clone();
    esc["probes"]["mutation"]["file"] = "../escape".into();
    let r = with("escape.marker", &esc).unwrap();
    assert!(r["mutation"].contains("must stay inside the tree"), "{r:?}");
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched");
    // (round 16, items 4, 6, 12) `affected` must fail with a `failedExit`, not only name the test;
    // a `mutation2` naming the first mutation's test proves nothing; a second mutation that cannot
    // be written fails the probes still open, and leaves `mutation`'s own verdict.
    let mut odd = g.clone();
    odd["affected"] = serde_json::json!({"run": ["sh", "-c",
        "printf '\\nfailures:\\n    m\\n\\ntest result: FAILED.\\n'; exit 0", "sh", "{files}"]});
    odd["probes"]["mutation2"]["expectId"] = "m".into();
    let r = with("odd.marker", &odd).unwrap();
    assert!(
        r["affected"].starts_with("fail: the affected run did not fail"),
        "{r:?}"
    );
    assert!(r["multi"].starts_with("fail:"), "{r:?}");
    assert_eq!(
        r["list"], "pass (retried)",
        "the list never saw the results env: {r:?}"
    );
    // (round 17, items 2, 3) A runner that swallows its failures (exit 0) proves nothing; a
    // deselect of a test listed twice proves nothing.
    let r = with("swallow", &g).unwrap();
    assert!(r["mutation"].starts_with("fail:"), "{r:?}");
    let r = with("dup.list", &g).unwrap();
    assert!(r["deselect"].starts_with("fail:"), "{r:?}");
    let mut bad2 = g.clone();
    bad2["probes"]["mutation2"]["file"] = "../m2".into();
    let r = with("bad2.marker", &bad2).unwrap();
    assert_eq!(r["mutation"], "pass (retried)", "{r:?}");
    assert!(
        r["multi"].contains("second mutation could not be written"),
        "{r:?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(outside.parent().unwrap());
}

/// Ruling 2: a check that declares a `version` command is relied on only while the runner still
/// prints the version its probes ran under; otherwise the guide is stale (exit codes only).
#[cfg(unix)]
#[test]
fn a_runner_whose_version_moved_since_the_probe_makes_the_guide_stale() {
    let _load = seam::pin(0.5, 4);
    for (tag, recorded, usable) in [
        ("version-same", "runner 2", true),
        ("version-moved", "runner 1", false),
    ] {
        let (repo, _, argv) = red_base(tag);
        let mut g = libtest_check(&argv);
        g["version"] = serde_json::json!({"run": ["sh", "-c", "echo; echo runner 2"]});
        write_guide(&repo, &[g], &git(&repo, &["rev-parse", "HEAD~1"]));
        let path = repo.join(crate::floor_guide::GUIDE_PATH);
        let mut doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        doc["checks"][0]["verified"]["runnerVersion"] = recorded.into();
        doc["checks"][0]["verified"]["results"]["version"] = "pass".into();
        std::fs::write(&path, doc.to_string()).unwrap();
        git(&repo, &["commit", "-qam", "a guide with a runner version"]);
        let ctx = FloorContext {
            base_head: Some(git(&repo, &["rev-parse", "HEAD"])),
            stage: FloorStage::Verify,
            git_dir: Some(repo.join(".git")),
            ..FloorContext::default()
        };
        write_tree(
            &repo,
            &argv,
            &[
                ("run.out", "test old ... FAILED\n"),
                ("run.exit", "101"),
                ("excluded.exit", "0"),
                ("toc.txt", &toc_of(&["old", "new", "fine"])),
            ],
        );
        let Some((_, c)) = floor_test(&repo, &ctx) else {
            return;
        };
        let d = c.decision.as_ref().expect("a decision");
        let g = d.guide.as_ref().expect("the guide's state");
        if usable {
            assert_eq!(d.outcome, OUTCOME_PASS_EXCLUDED, "{d:?}");
            assert!(g.usable, "{g:?}");
        } else {
            assert_eq!(d.outcome, OUTCOME_BASE_RED_UNSEPARATED, "{d:?}");
            assert!(
                g.reason.as_deref().is_some_and(|r| r.starts_with(
                    "stale: the runner reports `runner 2`, the guide was verified under `runner 1`"
                )),
                "{g:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&repo);
    }
}

/// Ruling 4: an exit outside `failedExit` and 0 (a crash, a build error) is `could_not_run`, on
/// the head's run and on its deselected run; a deselected run that fails naming nothing is a
/// `new_failure`; a red base whose results name nothing has no candidates and escalates.
#[cfg(unix)]
#[test]
fn an_exit_outside_failed_exit_could_not_run_and_a_failure_naming_nothing_never_passes() {
    let _load = seam::pin(0.5, 4);
    let (repo, ctx, argv) = red_base("exit-codes");
    let toc = toc_of(&["old", "new", "fine"]);
    let outcome = |files: &[(&str, &str)]| {
        write_tree(&repo, &argv, files);
        floor_test(&repo, &ctx).map(|(report, c)| {
            assert!(!report.passed, "{c:?}");
            c.decision.expect("a decision")
        })
    };
    // (a) The head's run crashes.
    let Some(d) = outcome(&[
        ("run.out", "error: could not build\n"),
        ("run.exit", "2"),
        ("toc.txt", &toc),
    ]) else {
        return;
    };
    assert_eq!(d.outcome, OUTCOME_COULD_NOT_RUN, "{d:?}");
    assert!(
        d.reason.as_deref().is_some_and(
            |r| r.contains("exit 2 is not one of the guide's `failedExit` codes [101]")
        ),
        "{d:?}"
    );
    let red = [
        ("run.out", "test old ... FAILED\n"),
        ("run.exit", "101"),
        ("toc.txt", toc.as_str()),
    ];
    // (b) The deselected run crashes.
    let d = outcome(&[red[0], red[1], red[2], ("excluded.exit", "2")]).unwrap();
    assert_eq!(d.outcome, OUTCOME_COULD_NOT_RUN, "{d:?}");
    // (c) The deselected run fails naming nothing.
    let d = outcome(&[red[0], red[1], red[2], ("excluded.exit", "101")]).unwrap();
    assert_eq!(d.outcome, OUTCOME_NEW_FAILURE, "{d:?}");
    // (d) A base that fails naming nothing: no candidates.
    write_tree(
        &repo,
        &argv,
        &[
            ("run.out", "something went wrong\n"),
            ("run.exit", "101"),
            ("toc.txt", &toc),
            ("alone.fail", "old\n"),
        ],
    );
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "a base red with nothing named"]);
    let ctx = FloorContext {
        base_head: Some(git(&repo, &["rev-parse", "HEAD"])),
        ..ctx.clone()
    };
    write_tree(
        &repo,
        &argv,
        &[red[0], red[1], red[2], ("excluded.exit", "0")],
    );
    let (_, c) = floor_test(&repo, &ctx).unwrap();
    let d = c.decision.expect("a decision");
    assert_eq!(d.outcome, OUTCOME_BASE_RED_UNSEPARATED, "{d:?}");
    let _ = std::fs::remove_dir_all(&repo);
}

/// Review round 15 (items 12, 13, 15): a signal is `could_not_run`, on the head's run and on the
/// deselected run, never `pass_excluded`; with a guide the change made unusable a non-zero exit
/// is a plain failure (no `failedExit` semantics); deleting a file `invalidatedBy` names makes the
/// guide unusable like editing it.
#[cfg(unix)]
#[test]
fn a_signal_could_not_run_and_an_unusable_guide_has_no_exit_code_semantics() {
    let _load = seam::pin(0.5, 4);
    let (repo, ctx, argv) = red_base("signals");
    let toc = toc_of(&["old", "new", "fine"]);
    let red = [
        ("run.out", "test old ... FAILED\n"),
        ("run.exit", "101"),
        ("toc.txt", toc.as_str()),
        ("excluded.exit", "0"),
    ];
    let decided = |files: &[(&str, &str)]| {
        write_tree(&repo, &argv, files);
        floor_test(&repo, &ctx).map(|(report, c)| (report.passed, c.decision.expect("decided")))
    };
    let Some((passed, d)) = decided(&[red[0], red[1], red[2], red[3], ("run.signal", "")]) else {
        return;
    };
    assert!(!passed && d.outcome == OUTCOME_COULD_NOT_RUN, "{d:?}");
    let (passed, d) = decided(&[red[0], red[1], red[2], red[3], ("excluded.signal", "")]).unwrap();
    assert!(!passed && d.outcome == OUTCOME_COULD_NOT_RUN, "{d:?}");
    // (round 16, item 11) Under load, a deselected run that crashed is decided, not retried.
    {
        let _loaded = seam::pin(64.0, 4);
        write_tree(
            &repo,
            &argv,
            &[red[0], red[1], red[2], ("excluded.exit", "2")],
        );
        let (_, c) = floor_test(&repo, &ctx).unwrap();
        let d = c.decision.as_ref().expect("decided");
        assert_eq!(d.outcome, OUTCOME_COULD_NOT_RUN, "{d:?}");
        assert_eq!(
            c.reruns.len(),
            1,
            "the deselected run only, no retry: {c:?}"
        );
    }
    let _load = seam::pin(0.5, 4);
    // A change touching `invalidatedBy`: the exit outside `failedExit` is a plain failure.
    std::fs::create_dir_all(repo.join(".cargo")).unwrap();
    std::fs::write(repo.join(".cargo/config.toml"), "[build]\n").unwrap();
    let (_, d) = decided(&[red[0], ("run.exit", "2"), red[2], red[3]]).unwrap();
    assert_eq!(d.outcome, OUTCOME_BASE_RED_UNSEPARATED, "{d:?}");
    assert!(d.guide.is_some_and(|g| !g.usable), "unusable");
    // Committed on the base, then deleted by the change: unusable too.
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "a runner config on the base"]);
    let ctx = FloorContext {
        base_head: Some(git(&repo, &["rev-parse", "HEAD"])),
        ..ctx.clone()
    };
    std::fs::remove_dir_all(repo.join(".cargo")).unwrap();
    write_tree(&repo, &argv, &red);
    let (_, c) = floor_test(&repo, &ctx).unwrap();
    let g = c.decision.and_then(|d| d.guide).expect("the guide's state");
    assert!(
        !g.usable
            && g.reason
                .as_deref()
                .is_some_and(|r| r.contains(".cargo/config.toml")),
        "{g:?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// Review round 15 (items 9, 24): the `affected` piece gets a root check's touched files (a
/// root-level check's `cwd` is the repo); a touched file named like a flag (`-x`) runs the full
/// check instead.
#[cfg(unix)]
#[test]
fn affected_gets_the_root_checks_files_and_a_flag_shaped_file_runs_the_full_check() {
    let _load = seam::pin(0.5, 4);
    let (repo, _, argv) = red_base("affected");
    let mut g = libtest_check(&argv);
    // Passes only when it is handed `marker.txt`.
    g["affected"] = serde_json::json!({"run": ["sh", "-c",
        "for f in \"$@\"; do [ \"$f\" = marker.txt ] && exit 0; done; exit 1", "sh", "{files}"]});
    write_guide(&repo, &[g], &git(&repo, &["rev-parse", "HEAD~1"]));
    let path = repo.join(crate::floor_guide::GUIDE_PATH);
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    doc["checks"][0]["verified"]["results"]["affected"] = "pass".into();
    std::fs::write(&path, doc.to_string()).unwrap();
    git(&repo, &["commit", "-qam", "a guide with an affected piece"]);
    let ctx = FloorContext {
        base_head: Some(git(&repo, &["rev-parse", "HEAD"])),
        stage: FloorStage::Creator,
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    // The full check is red with a new failure: only the affected run can pass it.
    // Every tracked fixture file stays (a deletion would rule `affected` out, below).
    let toc = toc_of(&["old", "new", "fine"]);
    let head = [
        ("run.out", "test new ... FAILED\n"),
        ("run.exit", "101"),
        ("excluded.exit", "101"),
        ("toc.txt", toc.as_str()),
    ];
    write_tree(&repo, &argv, &head);
    std::fs::write(repo.join("marker.txt"), "x").unwrap();
    let Some((report, c)) = floor_test(&repo, &ctx) else {
        return;
    };
    assert!(report.passed, "the affected run got `marker.txt`: {c:?}");
    assert!(c.source.contains("affected set"), "{}", c.source);
    std::fs::write(repo.join("-x"), "x").unwrap();
    let (report, c) = floor_test(&repo, &ctx).unwrap();
    assert!(
        !report.passed,
        "a flag-shaped file runs the full, red check: {c:?}"
    );
    // (round 16, item 2) A deleted file: what it left behind cannot be mapped — the full check.
    std::fs::remove_file(repo.join("-x")).unwrap();
    std::fs::remove_file(repo.join("alone.fail")).unwrap();
    let (report, c) = floor_test(&repo, &ctx).unwrap();
    assert!(!report.passed, "a deletion runs the full, red check: {c:?}");
    assert!(!c.source.contains("affected set"), "{}", c.source);
    let _ = std::fs::remove_dir_all(&repo);
}

/// Review round 15 (item 16): the identical retry runs with the deciding run's options (the
/// guide's env), not the defaults.
#[cfg(unix)]
#[test]
fn the_identical_retry_keeps_the_deciding_runs_env() {
    let _load = seam::pin(64.0, 4);
    let wt = scratch("retry-env");
    let sandbox = sandbox_for(&wt);
    if sandbox.level != crate::validator::SandboxLevel::Sandboxed {
        return;
    }
    let scratch = CheckScratch::prepare(&wt).unwrap();
    let one = RepoCheck {
        name: "test".into(),
        argv: s(&["sh", "-c", "[ \"$FLOOR_ENV\" = 1 ]"]),
        source: "test".into(),
        timeout_s: Some(30),
        dir: String::new(),
    };
    let exec = ExecOpts {
        env: vec![("FLOOR_ENV".into(), "1".into())],
        merge: false,
    };
    let first = run_one(&wt, &one, &sandbox, &scratch, Tree::Head);
    assert!(!first.passed() && ran_loaded(&first), "{first:?}");
    let retry = retry_if_loaded(&wt, &one, &exec, &first, &sandbox, &scratch).expect("retried");
    assert!(retry.passed(), "the retry ran with FLOOR_ENV=1: {retry:?}");
    let _ = std::fs::remove_dir_all(&wt);
}

/// Review round 15 (item 10): a results file is read only when it is a regular file inside the
/// tree, written since the run started.
#[cfg(unix)]
#[test]
fn a_results_file_is_a_fresh_regular_file_inside_the_tree() {
    let root = scratch("results-file");
    let outside = scratch("results-file-outside").join("secret");
    std::fs::write(&outside, "<testsuite/>").unwrap();
    std::fs::write(root.join("old.xml"), "<testsuite/>").unwrap();
    let since = std::time::SystemTime::now();
    std::fs::File::options()
        .write(true)
        .open(root.join("old.xml"))
        .unwrap()
        .set_modified(since - Duration::from_secs(10))
        .unwrap();
    std::fs::write(root.join("fresh.xml"), "<testsuite/>").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("link.xml")).unwrap();
    std::fs::create_dir_all(root.join("dir.xml")).unwrap();
    assert!(open_results(&root, "fresh.xml", since).is_ok());
    for (rel, why) in [
        ("old.xml", "not written by this run"),
        ("link.xml", "symlink"),
        ("dir.xml", "not a regular file"),
        ("../x.xml", "inside the tree"),
        ("missing.xml", "no results file"),
    ] {
        let e = open_results(&root, rel, since).expect_err(rel);
        assert!(e.contains(why), "{rel}: {e}");
    }
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(outside.parent().unwrap());
}

/// Ruling 5: with a guide in the base, the guide's checks for the stage ARE the floor's checks
/// (detection would add the config's `test` at the creator); without one, detection decides.
#[cfg(unix)]
#[test]
fn the_base_guides_checks_for_the_stage_are_the_floors_checks() {
    let (repo, _, argv) = red_base("guide-checks");
    let mut test = libtest_check(&argv);
    test["stage"] = serde_json::json!(["verify"]);
    // A gate check: no `list`, so no `results`.
    let lint = serde_json::json!({"name": "lint", "run": ["sh", "-c", "exit 0"],
        "failedExit": [1], "stage": ["creator"]});
    let base = commit_guide(&repo, &[test, lint]);
    let at = |stage, base: &str| {
        detect_with(
            &repo,
            &FloorContext {
                stage,
                base_head: Some(base.to_string()),
                git_dir: Some(repo.join(".git")),
                ..FloorContext::default()
            },
        )
        .unwrap()
    };
    for (stage, want) in [(FloorStage::Verify, "test"), (FloorStage::Creator, "lint")] {
        let checks = at(stage, &base);
        assert_eq!(names(&checks), [want], "{checks:?}");
        assert!(
            checks.iter().all(|c| c.source == GUIDE_SOURCE),
            "{checks:?}"
        );
    }
    // (round 16, item 1) A package the guide does not cover keeps its detected checks.
    std::fs::create_dir_all(repo.join("sub/node_modules")).unwrap();
    std::fs::write(
        repo.join("sub/package.json"),
        r#"{"scripts":{"test":"exit 0"}}"#,
    )
    .unwrap();
    let checks = at(FloorStage::Verify, &base);
    assert!(
        checks
            .iter()
            .any(|c| c.dir == "sub" && c.source != GUIDE_SOURCE),
        "{checks:?}"
    );
    assert!(checks
        .iter()
        .any(|c| c.name == "test" && c.source == GUIDE_SOURCE));
    std::fs::remove_dir_all(repo.join("sub")).unwrap();
    // The first commit carries no guide: detection.
    let checks = at(FloorStage::Verify, &git(&repo, &["rev-parse", "HEAD~2"]));
    assert_eq!(names(&checks), ["test"], "{checks:?}");
    assert!(
        checks.iter().all(|c| c.source != GUIDE_SOURCE),
        "{checks:?}"
    );
    // (review round 15, item 14) A guide that misspells or drops `checks` does not parse: detection.
    for bad in [r#"{"version": 1, "check": []}"#, r#"{"version": 1}"#] {
        std::fs::write(repo.join(crate::floor_guide::GUIDE_PATH), bad).unwrap();
        git(&repo, &["commit", "-qam", "a malformed guide"]);
        let checks = at(FloorStage::Verify, &git(&repo, &["rev-parse", "HEAD"]));
        assert_eq!(names(&checks), ["test"], "{bad}: {checks:?}");
        assert!(checks.iter().all(|c| c.source != GUIDE_SOURCE), "{bad}");
    }
    let _ = std::fs::remove_dir_all(&repo);
}

/// (round 16, item 8) A claim of "pre-existing" is judged on the check that DENIED, and a head
/// that dropped the base's failing test (nothing to deselect) still rejects it.
#[cfg(unix)]
#[test]
fn a_pre_existing_claim_is_judged_on_the_denying_check() {
    let run = |name: &str, outcome: &str, base_failing: &[&str], excluded: &[&str]| {
        let mut c: CheckRun = serde_json::from_value(serde_json::json!({
            "name": name, "argv": ["x"], "source": "s", "exit_code": 1, "timed_out": false,
            "duration_ms": 0, "stdout_tail": "", "stderr_tail": ""}))
        .unwrap();
        c.decision = Some(CheckDecision {
            outcome: outcome.into(),
            base_sha: Some("b".into()),
            base_failing: s(base_failing),
            excluded: s(excluded),
            log_refs: Vec::new(),
            flaky: false,
            reason: None,
            guide: None,
        });
        c
    };
    let ctx = FloorContext {
        stage: FloorStage::Creator,
        claim_text: Some("The failing test is pre-existing.".into()),
        ..FloorContext::default()
    };
    let checks = [
        run("first", OUTCOME_PASS_EXCLUDED, &["a"], &["a"]),
        run("second", OUTCOME_NEW_FAILURE, &[], &[]),
    ];
    let mut second = checks[1].clone();
    second.base = Some(Box::new(BaseRun {
        head: "b".into(),
        cached: false,
        run: Some(run("second", OUTCOME_PASS, &[], &[])),
        error: None,
    }));
    second
        .base
        .as_mut()
        .unwrap()
        .run
        .as_mut()
        .unwrap()
        .exit_code = Some(0);
    let c = judge_claim(&ctx, &[checks[0].clone(), second]).unwrap();
    assert_eq!(
        (c.check.as_str(), c.verdict.as_str()),
        ("second", CLAIM_REJECTED)
    );
    let c = judge_claim(&ctx, &[run("t", OUTCOME_NEW_FAILURE, &["old"], &[])]).unwrap();
    assert_eq!(
        c.verdict, CLAIM_REJECTED,
        "the head dropped `old`; what fails is its own"
    );
}

/// (round 17, codex P1) Exporting the base for a probe or a floor runs no repository hook and no
/// filter driver: a `post-index-change` hook and a smudge filter that would write outside never do.
#[cfg(unix)]
#[test]
fn the_base_export_runs_no_repository_hook_or_filter() {
    let _load = seam::pin(0.5, 4);
    let repo = scratch("export-hooks");
    let outside = scratch("export-hooks-outside");
    let marker = outside.join("ran");
    std::fs::write(repo.join("runner.sh"), STAND_IN).unwrap();
    std::fs::write(repo.join("a.txt"), "plain\n").unwrap();
    std::fs::write(repo.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "tmp/\nprobe-guide.json\n").unwrap();
    git_repo_with_commit(&repo);
    // Configured AFTER the commit, as a repository could carry them.
    let hooks = outside.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("post-index-change");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(
        &repo,
        &["config", "core.hooksPath", &hooks.to_string_lossy()],
    );
    let smudge = format!("sh -c 'touch {}; cat'", marker.display());
    git(&repo, &["config", "filter.evil.smudge", &smudge]);
    git(&repo, &["config", "filter.evil.clean", "cat"]);
    let scratch_dir = CheckScratch::prepare(&repo).unwrap();
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let tree = BaseTree::export(&repo, &scratch_dir, &repo.join(".git"), &head).unwrap();
    assert_eq!(
        std::fs::read_to_string(tree.dir.join("a.txt")).unwrap(),
        "plain\n"
    );
    let _ = remove_base_export(&repo, &scratch_dir);
    // The probe exports through the same path.
    let _ = probe(&repo, stand_in_guide());
    assert!(
        !marker.exists(),
        "a repository hook or filter ran outside the sandbox"
    );
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&outside);
}

/// (round 17, codex P2) A base candidate that PASSES run alone is not a confirmed base failure:
/// the check escalates instead of excluding it.
#[cfg(unix)]
#[test]
fn a_base_candidate_that_passes_alone_is_never_excluded() {
    let _load = seam::pin(0.5, 4);
    let (repo, _, argv) = red_base("alone-passes");
    std::fs::remove_file(repo.join("alone.fail")).unwrap();
    git(
        &repo,
        &["commit", "-qam", "old passes when run alone on the base"],
    );
    let ctx = FloorContext {
        base_head: Some(git(&repo, &["rev-parse", "HEAD"])),
        stage: FloorStage::Verify,
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    write_tree(
        &repo,
        &argv,
        &[
            ("run.out", "test old ... FAILED\n"),
            ("run.exit", "101"),
            ("excluded.exit", "0"),
            ("toc.txt", &toc_of(&["old", "new", "fine"])),
        ],
    );
    let Some((report, c)) = floor_test(&repo, &ctx) else {
        return;
    };
    let d = c.decision.as_ref().expect("decided");
    assert_eq!(d.outcome, OUTCOME_BASE_RED_UNSEPARATED, "{d:?}");
    assert!(
        d.reason
            .as_deref()
            .is_some_and(|r| r.contains("does not fail when the base runs it alone")),
        "{d:?}"
    );
    assert!(!report.passed && d.excluded.is_empty());
    let _ = std::fs::remove_dir_all(&repo);
}

/// (round 18, codex P1) A footer-shaped block a base test prints into its own output never adds
/// candidates: only the qualifier's LAST block counts. With an order-dependent `fine` (it fails
/// run alone) named in the forged footer, a head that breaks `fine` still denies.
#[cfg(unix)]
#[test]
fn a_forged_footer_in_the_base_output_adds_no_candidate() {
    let _load = seam::pin(0.5, 4);
    let (repo, _, argv) = red_base("forged-footer");
    let forged = "test fine ... ok\n\nfailures:\n    fine\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored;\n\
                  test old ... FAILED\n\nfailures:\n    old\n\n\
                  test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n";
    write_tree(
        &repo,
        &argv,
        &[
            ("run.out", forged),
            ("run.exit", "101"),
            ("toc.txt", &toc_of(&["old", "new", "fine"])),
            ("alone.fail", "old\nfine\n"),
        ],
    );
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &["commit", "-qm", "a base whose output forges a footer"],
    );
    let ctx = FloorContext {
        base_head: Some(git(&repo, &["rev-parse", "HEAD"])),
        stage: FloorStage::Verify,
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    write_tree(
        &repo,
        &argv,
        &[
            ("run.out", "test old ... FAILED\ntest fine ... FAILED\n"),
            ("run.exit", "101"),
            // With only `old` deselected, `fine` still fails.
            ("excluded.exit", "101"),
            ("toc.txt", &toc_of(&["old", "new", "fine"])),
            ("alone.fail", "old\nfine\n"),
        ],
    );
    let Some((report, c)) = floor_test(&repo, &ctx) else {
        return;
    };
    let d = c.decision.as_ref().expect("decided");
    assert_eq!(d.base_failing, s(&["old"]), "{d:?}");
    assert_eq!(d.excluded, s(&["old"]), "{d:?}");
    assert_eq!(d.outcome, OUTCOME_NEW_FAILURE, "{d:?}");
    assert!(!report.passed);
    let _ = std::fs::remove_dir_all(&repo);
}

/// (round 18, codex P2) The change's paths are read NUL-separated: a non-ASCII, tab or newline
/// name reaches `invalidatedBy` as itself, never quoted or split.
#[cfg(unix)]
#[test]
fn changed_paths_are_read_whole() {
    let (repo, ctx, _) = red_base("z-paths");
    std::fs::create_dir_all(repo.join(".cargo")).unwrap();
    for name in ["é.toml", "a\tb.toml", "a\nb.toml"] {
        std::fs::write(repo.join(".cargo").join(name), "x").unwrap();
    }
    let changed = changed_files(&repo, &ctx).unwrap();
    for name in [".cargo/é.toml", ".cargo/a\tb.toml", ".cargo/a\nb.toml"] {
        assert!(changed.iter().any(|f| f == name), "{name:?} in {changed:?}");
        assert!(crate::floor_guide::glob_match(".cargo/**", name));
    }
    let _ = std::fs::remove_dir_all(&repo);
}

/// (round 18, agy A P2) A head that changes a file the guide was derived from makes it unusable
/// for that head, as `invalidatedBy` does.
#[cfg(unix)]
#[test]
fn a_head_that_changes_a_derived_from_file_cannot_use_the_guide() {
    let _load = seam::pin(0.5, 4);
    let (repo, _, argv) = red_base("derived-head");
    let path = repo.join(crate::floor_guide::GUIDE_PATH);
    let raw = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        raw.replace("\"derivedFrom\": []", "\"derivedFrom\": [\"**/notes.md\"]"),
    )
    .unwrap();
    git(
        &repo,
        &["commit", "-qam", "the guide names what it was derived from"],
    );
    let ctx = FloorContext {
        base_head: Some(git(&repo, &["rev-parse", "HEAD"])),
        stage: FloorStage::Verify,
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    write_tree(
        &repo,
        &argv,
        &[
            ("run.out", "test old ... FAILED\n"),
            ("run.exit", "101"),
            ("excluded.exit", "0"),
            ("toc.txt", &toc_of(&["old", "new", "fine"])),
        ],
    );
    std::fs::write(repo.join("notes.md"), "a root file the glob matches\n").unwrap();
    let Some((_, c)) = floor_test(&repo, &ctx) else {
        return;
    };
    let d = c.decision.as_ref().expect("decided");
    assert_eq!(d.outcome, OUTCOME_BASE_RED_UNSEPARATED, "{d:?}");
    let g = d.guide.as_ref().unwrap();
    assert!(
        g.reason.as_deref().is_some_and(|r| r.contains("notes.md")),
        "{g:?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// Exclusion ships OFF (`WICKED_FLOOR_EXCLUDE`): a red base and a failing head escalate as
/// `base_red_unseparated`, saying so, and neither the base's list, a one-test run, nor a
/// deselected head run is executed. The guide still names the check and is still checked.
#[cfg(unix)]
#[test]
fn with_exclusion_off_a_red_base_escalates_and_nothing_is_deselected() {
    let _load = seam::pin(0.5, 4);
    let _off = seam::exclude(false);
    let (repo, ctx, argv) = red_base("exclude-off");
    // The fake runner counts every run by mode under `tmp/counters` (ignored, inside the tree):
    // committed, so the base export counts too.
    let counters = repo.join("tmp/counters");
    std::fs::write(
        repo.join("counters.dir"),
        counters.to_string_lossy().as_bytes(),
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "count the runs"]);
    let ctx = FloorContext {
        base_head: Some(git(&repo, &["rev-parse", "HEAD"])),
        ..ctx
    };
    write_tree(
        &repo,
        &argv,
        &[
            ("run.out", "test old ... FAILED\n"),
            ("run.exit", "101"),
            ("excluded.exit", "0"),
            ("toc.txt", &toc_of(&["old", "new", "fine"])),
        ],
    );
    let Some((report, c)) = floor_test(&repo, &ctx) else {
        return;
    };
    let d = c.decision.as_ref().expect("decided");
    assert_eq!(d.outcome, OUTCOME_BASE_RED_UNSEPARATED, "{d:?}");
    assert!(
        d.reason
            .as_deref()
            .is_some_and(|r| r.contains("exclusion disabled (WICKED_FLOOR_EXCLUDE)")),
        "{d:?}"
    );
    assert!(
        !report.passed && d.excluded.is_empty() && c.reruns.is_empty(),
        "{c:?}"
    );
    let attempts: Vec<&str> = d.log_refs.iter().map(|l| l.attempt.as_str()).collect();
    assert!(
        attempts.iter().all(|a| *a == "head" || *a == "base"),
        "no list, one-test or deselected run: {attempts:?}"
    );
    assert!(
        d.guide.as_ref().is_some_and(|g| g.present && g.usable),
        "{d:?}"
    );
    // The head and the base ran; no list, no one-test run, no deselected run did.
    assert!(counters.join("run").is_file(), "the runs were counted");
    for mode in ["toc", "alone", "excluded"] {
        assert!(!counters.join(mode).exists(), "a `{mode}` run happened");
    }
    let _ = std::fs::remove_dir_all(&repo);
}

/// The switch reads exactly `on` or `1`: nothing trimmed, nothing case-folded.
#[test]
fn the_exclusion_switch_reads_exactly_on_or_1() {
    for (v, on) in [
        (None, false),
        (Some("on"), true),
        (Some("1"), true),
        (Some("ON"), false),
        (Some("true"), false),
        (Some(" on"), false),
        (Some("1\n"), false),
    ] {
        assert_eq!(exclusion_from(v), on, "{v:?}");
    }
}

/// (final review, codex P2) A head that swaps a guided `cwd` for a link out of the tree: the
/// check does not run there (`could_not_run`), and nothing lands outside.
#[cfg(unix)]
#[test]
fn a_guided_cwd_turned_into_a_link_does_not_run() {
    let _load = seam::pin(0.5, 4);
    let repo = scratch("cwd-link");
    let outside = scratch("cwd-link-outside");
    std::fs::create_dir_all(repo.join("pkg")).unwrap();
    std::fs::write(repo.join("pkg/keep.txt"), "x").unwrap();
    std::fs::write(repo.join(".gitignore"), "tmp/\n").unwrap();
    git_repo_with_commit(&repo);
    let gate = serde_json::json!({"name": "gate", "cwd": "pkg",
        "run": ["sh", "-c", "touch escaped; exit 0"]});
    let base = commit_guide(&repo, &[gate]);
    std::fs::remove_dir_all(repo.join("pkg")).unwrap();
    std::os::unix::fs::symlink(&outside, repo.join("pkg")).unwrap();
    let ctx = FloorContext {
        stage: FloorStage::Verify,
        base_head: Some(base),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    let report = run_floor(&repo, &ctx);
    if report.sandbox_error.is_some() {
        return;
    }
    let c = report
        .checks
        .iter()
        .find(|c| c.name == "gate")
        .unwrap_or_else(|| panic!("the gate ran: {report:#?}"));
    assert!(!report.passed, "{report:#?}");
    assert!(
        c.spawn_error
            .as_deref()
            .is_some_and(|e| e.contains("not a plain directory")),
        "{c:?}"
    );
    assert!(!outside.join("escaped").exists());
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&outside);
}

/// (final review, codex P2) A `derivedFrom` file the base RENAMED since the probe makes the guide
/// stale (renames read as the old path gone).
#[cfg(unix)]
#[test]
fn a_derived_from_file_renamed_on_the_base_makes_the_guide_stale() {
    let _load = seam::pin(0.5, 4);
    let (repo, _, argv) = red_base("derived-rename");
    std::fs::create_dir_all(repo.join("cfg")).unwrap();
    std::fs::write(repo.join("cfg/a.toml"), "setting = 1\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "a runner config"]);
    write_guide(
        &repo,
        &[libtest_check(&argv)],
        &git(&repo, &["rev-parse", "HEAD"]),
    );
    let path = repo.join(crate::floor_guide::GUIDE_PATH);
    let raw = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        raw.replace("\"derivedFrom\": []", "\"derivedFrom\": [\"cfg/a.toml\"]"),
    )
    .unwrap();
    git(
        &repo,
        &["commit", "-qam", "the guide, derived from cfg/a.toml"],
    );
    git(&repo, &["mv", "cfg/a.toml", "cfg/b.toml"]);
    git(&repo, &["commit", "-qm", "the config renamed"]);
    let ctx = FloorContext {
        base_head: Some(git(&repo, &["rev-parse", "HEAD"])),
        stage: FloorStage::Verify,
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    write_tree(
        &repo,
        &argv,
        &[
            ("run.out", "test old ... FAILED\n"),
            ("run.exit", "101"),
            ("excluded.exit", "0"),
            ("toc.txt", &toc_of(&["old", "new", "fine"])),
        ],
    );
    let Some((_, c)) = floor_test(&repo, &ctx) else {
        return;
    };
    let g = c.decision.as_ref().and_then(|d| d.guide.clone()).unwrap();
    assert!(
        !g.usable
            && g.reason
                .as_deref()
                .is_some_and(|r| r.starts_with("stale: `cfg/a.toml`")),
        "{g:?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// (final review, G4) A changed path that is not UTF-8 cannot be matched against `invalidatedBy`:
/// the change's files cannot be listed and the guide is unusable. (Where the file system refuses
/// such a name — APFS — there is nothing to test.)
#[cfg(unix)]
#[test]
fn a_changed_path_that_is_not_utf8_makes_the_guide_unusable() {
    use std::os::unix::ffi::OsStrExt;
    let _load = seam::pin(0.5, 4);
    let (repo, ctx, argv) = red_base("non-utf8");
    let name = std::ffi::OsStr::from_bytes(b"bad-\xff.txt");
    if std::fs::write(repo.join(name), "x").is_err() {
        eprintln!("repo_checks: this file system refuses non-UTF-8 names — nothing to test");
        return;
    }
    assert!(changed_files(&repo, &ctx).is_err());
    write_tree(
        &repo,
        &argv,
        &[
            ("run.out", "test old ... FAILED\n"),
            ("run.exit", "101"),
            ("excluded.exit", "0"),
            ("toc.txt", &toc_of(&["old", "new", "fine"])),
        ],
    );
    let Some((_, c)) = floor_test(&repo, &ctx) else {
        return;
    };
    let g = c.decision.as_ref().and_then(|d| d.guide.clone()).unwrap();
    assert!(
        !g.usable && g.reason.as_deref().is_some_and(|r| r.contains("not UTF-8")),
        "{g:?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
}
