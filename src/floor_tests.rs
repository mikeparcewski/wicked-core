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
        reused_from: None,
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
    let mut all = probe_checks(repo, vec![guide])?;
    let path = repo.join("probe-guide.json");
    let written: crate::floor_guide::BuildGuide =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let v = written.checks[0]
        .verified
        .clone()
        .expect("verified is written");
    assert_eq!(v.base_sha, git(repo, &["rev-parse", "HEAD"]));
    Some(all.remove(0).1)
}

/// Probe a guide of `checks` (written beside the committed tree) against `repo`'s HEAD: each
/// probed check's results; `None` when the floor sandbox does not arm here.
#[cfg(unix)]
fn probe_checks(
    repo: &Path,
    checks: Vec<serde_json::Value>,
) -> Option<Vec<(String, crate::floor_probe::ProbeResults)>> {
    let path = repo.join("probe-guide.json");
    std::fs::write(
        &path,
        serde_json::json!({"version": 1, "checks": checks}).to_string(),
    )
    .unwrap();
    match crate::floor_probe::probe_guide(repo, &path, None) {
        Ok(all) => Some(all),
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
    // (codex on #1066) A link a run planted in one copy never reaches a mutation: every phase
    // writes its mutations into a FRESH copy before any command runs there.
    let r = with("plant.link", &g).unwrap();
    assert_eq!(r["mutation"], "pass", "{r:?}");
    // A link the tree itself carries at the mutation path is refused, never followed.
    std::os::unix::fs::symlink(&outside, repo.join("zz_m")).unwrap();
    let r = with("link.committed", &g).unwrap();
    assert!(r["mutation"].contains("passes through a symlink"), "{r:?}");
    std::fs::remove_file(repo.join("zz_m")).unwrap();
    git(&repo, &["commit", "-qam", "the link removed"]);
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

/// (codex on #1066) A repository command that replaces its tree's ROOT with a link to an outside
/// directory — in the guide's install step, or in the clean run right after it ran green (the
/// engine used to write the next mutation into that same copy, by path: through the link) —
/// never redirects a write of the engine's into that directory, and the probe fails rather than
/// passes: the mutations are written into a FRESH copy before any command runs there, every later
/// write goes through the copy's held directory fd ([`crate::repo_checks::HeldDir`]), and a next
/// copy whose path is a link is refused. Under the floor sandbox the swap itself is refused too
/// (macOS: a write root's own path is not writable; Linux: the root is a bind mount) — so the
/// command can only empty its copy. Skipped where no floor sandbox arms (required on CI's ubuntu
/// job).
#[cfg(unix)]
#[test]
fn a_command_that_swaps_its_tree_root_for_a_link_never_redirects_the_probe() {
    let _load = seam::pin(0.5, 4);
    let outside = scratch("probe-swap-outside");
    std::fs::write(outside.join("marker"), "untouched").unwrap();
    // Every route a shell has: rename the root away (else empty and remove it), then put a link
    // to `outside` at its path. Always exits 0, so the install "succeeds".
    let swap = format!(
        "d=$(pwd -P); cd /; mv \"$d\" \"$d.moved\" 2>/dev/null || rm -rf \"$d\" 2>/dev/null; \
         ln -s '{}' \"$d\" 2>/dev/null; exit 0\n",
        outside.display()
    );
    for (tag, in_install) in [("probe-swap-run", false), ("probe-swap-install", true)] {
        let repo = scratch(tag);
        std::fs::write(repo.join("runner.sh"), STAND_IN).unwrap();
        std::fs::write(repo.join("swap.sh"), &swap).unwrap();
        std::fs::write(repo.join(".gitignore"), "tmp/\nprobe-guide.json\n").unwrap();
        git_repo_with_commit(&repo);
        let mut test = stand_in_guide();
        let mut checks = Vec::new();
        if in_install {
            checks.push(serde_json::json!({"name": "install", "run": ["sh", "swap.sh"]}));
        } else {
            // The run reports first (green on the clean copy), then swaps.
            test["run"] = serde_json::json!([
                "sh",
                "-c",
                "sh runner.sh \"$@\"; s=$?; sh swap.sh; exit $s",
                "sh"
            ]);
        }
        checks.push(test);
        let path = repo.join("probe-guide.json");
        std::fs::write(
            &path,
            serde_json::json!({"version": 1, "checks": checks}).to_string(),
        )
        .unwrap();
        let probed = crate::floor_probe::probe_guide(&repo, &path, None);
        // First, whatever the probe answered: nothing landed outside (an engine before the fix
        // wrote the run variant's mutation, `zz_m`, there).
        let mut seen: Vec<String> = std::fs::read_dir(&outside)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            ["marker"],
            "{tag}: nothing lands outside ({probed:?})"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("marker")).unwrap(),
            "untouched",
            "{tag}"
        );
        let all = match probed {
            Ok(all) => all,
            Err(e) if e.contains("does not arm") => {
                assert!(!tools_required(), "the floor sandbox is required here: {e}");
                eprintln!("repo_checks: {e} — skipping the root-swap probe");
                let _ = std::fs::remove_dir_all(&repo);
                break;
            }
            Err(e) => panic!("{tag}: the probe ran to its verdicts: {e}"),
        };
        let r = &all
            .iter()
            .find(|(name, _)| name == "test")
            .unwrap_or_else(|| panic!("{tag}: the test check was probed: {all:?}"))
            .1;
        eprintln!("{tag}: {r:?}");
        let must_fail: &[&str] = if in_install {
            &["list", "run", "mutation", "multi"]
        } else {
            &["mutation", "multi"]
        };
        for p in must_fail {
            assert!(
                r.get(*p).is_some_and(|v| v.starts_with("fail")),
                "{tag}: `{p}` must fail: {r:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&repo);
    }
    let _ = std::fs::remove_dir_all(&outside);
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
    let retry =
        retry_if_loaded(&wt, None, &one, &exec, &first, &sandbox, &scratch).expect("retried");
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
    let held = HeldDir::open(&root).unwrap();
    assert!(open_results(&held, "fresh.xml", since).is_ok());
    for (rel, why) in [
        ("old.xml", "not written by this run"),
        ("link.xml", "symlink"),
        ("dir.xml", "not a regular file"),
        ("../x.xml", "inside the tree"),
        ("missing.xml", "no results file"),
    ] {
        let e = open_results(&held, rel, since).expect_err(rel);
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
    for name in ["post-index-change", "post-checkout", "post-commit"] {
        let hook = hooks.join(name);
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
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
    git(&repo, &["config", "filter.evil.process", &smudge]);
    git(&repo, &["config", "filter.evil.required", "true"]);
    let scratch_dir = CheckScratch::prepare(&repo).unwrap();
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let root = crate::repo_checks::HeldDir::open(&repo).unwrap();
    let tree = BaseTree::export(&repo, &root, &repo.join(".git"), &head).unwrap();
    assert_eq!(
        std::fs::read_to_string(tree.dir.join("a.txt")).unwrap(),
        "plain\n"
    );
    let _ = remove_base_export(&root);
    drop(scratch_dir);
    // The guarded (verify/deliver) checkout exports through the same conversion: it too runs no
    // configured driver or hook, and still writes the file.
    let tree_id = git(&repo, &["rev-parse", "HEAD^{tree}"]);
    let guarded =
        crate::repo_checks::checkout_guarded_tree(&repo, &repo.join(".git"), &tree_id, "hooks")
            .unwrap();
    assert_eq!(
        std::fs::read_to_string(guarded.dir.join("a.txt")).unwrap(),
        "plain\n"
    );
    drop(guarded);
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

/// (power-speed) When the decision cannot exclude, the deciding run is the head itself, so its
/// identical retry runs BEFORE the base. A retry that passes decides alone, and the base, a whole
/// suite the decision would never read, is not run. A retry that fails is the decision's only
/// retry, and the base then runs as before: the outcome is the one the old order gave.
#[cfg(unix)]
#[test]
fn without_exclusion_the_retry_runs_before_the_base_and_a_pass_skips_the_base() {
    let cpus = 4;
    let _off = seam::exclude(false);
    let (repo, ctx, argv) = red_base("retry-first");
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
    let runs = |c: &Path| {
        std::fs::read_to_string(c.join("run"))
            .map(|t| t.lines().count())
            .unwrap_or(0)
    };
    // A flake under load: red once, green on the identical retry. The red base never runs.
    write_tree(
        &repo,
        &argv,
        &[
            ("run.out", "test old ... FAILED\n"),
            ("run.exit", "0"),
            ("flake", ""),
            ("toc.txt", &toc_of(&["old", "new", "fine"])),
        ],
    );
    {
        let _load = seam::pin(cpus as f64 * 2.5, cpus);
        let Some((report, c)) = floor_test(&repo, &ctx) else {
            return;
        };
        let d = c.decision.as_ref().expect("decided");
        assert_eq!(d.outcome, OUTCOME_PASS, "{d:?}");
        assert!(d.flaky && report.passed && !c.denies(), "{d:?}");
        assert_eq!(c.classification.as_deref(), Some(FLAKY_UNDER_LOAD));
        assert_eq!(c.reruns.len(), 1, "one identical retry");
        assert_eq!(c.reruns[0].argv, c.argv, "the SAME argv");
        assert!(c.base.is_none(), "the base was not run: {:?}", c.base);
        assert!(
            d.log_refs.iter().all(|l| l.attempt != "base"),
            "{:?}",
            d.log_refs
        );
        assert_eq!(runs(&counters), 2, "the head and its retry, no base run");
        // The label says the wait, not the wait plus the retry's own run (none in tests).
        assert!(
            c.reruns[0].source.contains("after waiting 0s for headroom"),
            "{}",
            c.reruns[0].source
        );
    }
    // Red twice: the base runs after the retry, and the outcome is the old one.
    let _ = std::fs::remove_dir_all(&counters);
    write_tree(
        &repo,
        &argv,
        &[
            ("run.out", "test old ... FAILED\n"),
            ("run.exit", "101"),
            ("toc.txt", &toc_of(&["old", "new", "fine"])),
        ],
    );
    {
        let _load = seam::pin(cpus as f64 * 2.5, cpus);
        let Some((report, c)) = floor_test(&repo, &ctx) else {
            return;
        };
        let d = c.decision.as_ref().expect("decided");
        assert_eq!(d.outcome, OUTCOME_BASE_RED_UNSEPARATED, "{d:?}");
        assert!(!report.passed && !d.flaky);
        assert_eq!(
            c.reruns.len(),
            1,
            "one retry, never a second one after the base"
        );
        assert!(
            c.base.as_ref().is_some_and(|b| b.run.is_some()),
            "the base ran: {:?}",
            c.base
        );
        assert_eq!(runs(&counters), 3, "the head, its retry, then the base");
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
/// the change's files cannot be listed, and the floor refuses (detection, or an unusable guide). (Where the file system refuses
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
    // Fail closed: the floor refuses — detection that needs the change's files reports it, or
    // the guide is unusable for the check.
    let report = run_floor(&repo, &ctx);
    if report.sandbox_error.is_some() {
        return;
    }
    let named = report
        .detect_error
        .as_deref()
        .is_some_and(|e| e.contains("not UTF-8"))
        || report.checks.iter().any(|c| {
            c.decision
                .as_ref()
                .and_then(|d| d.guide.as_ref())
                .is_some_and(|g| {
                    !g.usable && g.reason.as_deref().is_some_and(|r| r.contains("not UTF-8"))
                })
        });
    assert!(!report.passed && named, "{report:#?}");
    let _ = std::fs::remove_dir_all(&repo);
}

/// A guide check with an `affected` piece that ALWAYS passes, verified (every probe `pass`,
/// `affected` included) on the repo's HEAD, committed. Returns the new HEAD.
#[cfg(unix)]
fn commit_guide_with_passing_affected(repo: &Path, argv: &[String]) -> String {
    let mut g = libtest_check(argv);
    g["affected"] = serde_json::json!({"run": ["sh", "-c", "exit 0", "sh", "{files}"]});
    write_guide(repo, &[g], &git(repo, &["rev-parse", "HEAD"]));
    let path = repo.join(crate::floor_guide::GUIDE_PATH);
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    doc["checks"][0]["verified"]["results"]["affected"] = "pass".into();
    std::fs::write(&path, doc.to_string()).unwrap();
    git(repo, &["add", "-A"]);
    git(
        repo,
        &["commit", "-qm", "a guide with a passing affected piece"],
    );
    git(repo, &["rev-parse", "HEAD"])
}

/// (confirmation round, codex P2) A tracked config replaced by a link is a TYPE change (`T`): it
/// is in the change's files (what `invalidatedBy` and `derivedFrom` match), and like a deletion it
/// rules the creator's `affected` pre-pass out. With exclusion explicitly off, an affected run
/// that would pass where the full run fails: the full check runs, and denies.
#[cfg(unix)]
#[test]
fn a_config_replaced_by_a_link_runs_the_full_check_and_denies() {
    let _load = seam::pin(0.5, 4);
    let _off = seam::exclude(false);
    let repo = scratch("type-change");
    let argv = vec![
        fake_cargo("type-change"),
        "test".into(),
        "--no-fail-fast".into(),
    ];
    std::fs::write(repo.join(".gitignore"), "tmp/\n").unwrap();
    let toc = toc_of(&["new", "fine"]);
    // A green base: the full run passes there.
    write_tree(
        &repo,
        &argv,
        &[
            ("run.out", "test new ... ok\ntest fine ... ok\n"),
            ("run.exit", "0"),
            ("toc.txt", toc.as_str()),
        ],
    );
    std::fs::write(repo.join("runner.toml"), "setting = 1\n").unwrap();
    std::fs::write(repo.join("other.toml"), "setting = 2\n").unwrap();
    git_repo_with_commit(&repo);
    let ctx = FloorContext {
        base_head: Some(commit_guide_with_passing_affected(&repo, &argv)),
        stage: FloorStage::Creator,
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    // The head's full run is red with a new failure; only the affected pre-pass can pass it.
    // Every tracked fixture file stays (a deletion would rule `affected` out on its own).
    let head = [
        ("run.out", "test new ... FAILED\n"),
        ("run.exit", "101"),
        ("toc.txt", toc.as_str()),
    ];
    write_tree(&repo, &argv, &head);
    std::fs::write(repo.join("other.toml"), "setting = 3\n").unwrap();
    let Some((report, c)) = floor_test(&repo, &ctx) else {
        return;
    };
    assert!(
        report.passed && c.source.contains("affected set"),
        "control: a plain edit lets the affected run decide: {c:?}"
    );
    // The config becomes a link: `git diff` reports it as `T`, in no added/modified list.
    std::fs::remove_file(repo.join("runner.toml")).unwrap();
    std::os::unix::fs::symlink("other.toml", repo.join("runner.toml")).unwrap();
    assert!(!touched_files(&repo, &ctx)
        .unwrap()
        .contains(&"runner.toml".to_string()));
    assert!(changed_files(&repo, &ctx)
        .unwrap()
        .contains(&"runner.toml".to_string()));
    let (report, c) = floor_test(&repo, &ctx).unwrap();
    assert!(!report.passed, "the full, red check denies: {c:?}");
    assert!(!c.source.contains("affected set"), "{}", c.source);
    let d = c.decision.as_ref().expect("decided");
    assert_eq!(d.outcome, OUTCOME_NEW_FAILURE, "{d:?}");
    let _ = std::fs::remove_dir_all(&repo);
}

/// (confirmation round, agy P1 #2) An `invalidatedBy` file the BASE changed since the check was
/// verified makes it unusable, as a `derivedFrom` file does.
#[cfg(unix)]
#[test]
fn an_invalidated_by_file_changed_on_the_base_since_verification_makes_the_check_unusable() {
    let _load = seam::pin(0.5, 4);
    let (repo, _, argv) = red_base("invalidated-since");
    std::fs::create_dir_all(repo.join("cfg")).unwrap();
    std::fs::write(repo.join("cfg/a.toml"), "setting = 1\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "a runner config"]);
    let mut check = libtest_check(&argv);
    check["invalidatedBy"] = serde_json::json!(["cfg/*.toml"]);
    write_guide(&repo, &[check], &git(&repo, &["rev-parse", "HEAD"]));
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &["commit", "-qm", "the guide, invalidated by cfg/*.toml"],
    );
    std::fs::write(repo.join("cfg/a.toml"), "setting = 2\n").unwrap();
    git(&repo, &["commit", "-qam", "the config changed on the base"]);
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
    let g = c.decision.as_ref().and_then(|d| d.guide.clone()).unwrap();
    assert!(
        !report.passed
            && g.present
            && !g.usable
            && g.reason.as_deref().is_some_and(|r| {
                r.starts_with("stale: `cfg/a.toml`") && r.contains("invalidatedBy")
            }),
        "{g:?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// (confirmation round, agy P1 #1) A STALE guide may name outdated commands: in the directories
/// it covers, the floor runs the UNION of its checks and the detected ones — when the base changed
/// a `derivedFrom` file since the guide was written, or when the change touches one. A fresh guide
/// replaces detection there.
#[cfg(unix)]
#[test]
fn a_stale_guide_runs_beside_the_detected_checks() {
    let repo = scratch("stale-union");
    // Detection (`.wicked/checks.json`) names `test`; the guide names only `gate`.
    let argv = vec!["detected-runner".to_string(), "test".into()];
    write_tree(&repo, &argv, &[]);
    std::fs::write(repo.join("notes.md"), "v1\n").unwrap();
    git_repo_with_commit(&repo);
    let gate = serde_json::json!({"name": "gate", "run": ["sh", "-c", "exit 0"]});
    write_guide(&repo, &[gate], &git(&repo, &["rev-parse", "HEAD"]));
    let path = repo.join(crate::floor_guide::GUIDE_PATH);
    let raw = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        raw.replace("\"derivedFrom\": []", "\"derivedFrom\": [\"notes.md\"]"),
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &["commit", "-qm", "the guide, derived from notes.md"],
    );
    let ctx_at = |base: String| FloorContext {
        base_head: Some(base),
        stage: FloorStage::Verify,
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    let checks = |ctx: &FloorContext| -> Vec<(String, String)> {
        detect_with(&repo, ctx)
            .unwrap()
            .into_iter()
            .map(|c| (c.name, c.source))
            .collect()
    };
    let fresh = ctx_at(git(&repo, &["rev-parse", "HEAD"]));
    let names: Vec<String> = checks(&fresh).into_iter().map(|c| c.0).collect();
    assert_eq!(names, ["gate"], "a fresh guide replaces detection");
    // The change touches a `derivedFrom` file: the union.
    std::fs::write(repo.join("notes.md"), "v2\n").unwrap();
    let got = checks(&fresh);
    assert!(
        got.iter().any(|c| c.0 == "gate")
            && got.iter().any(|c| c.0 == "test"
                && c.1.contains("stale build guide")
                && c.1.contains("notes.md")),
        "{got:?}"
    );
    // The base changed it since the guide was written: the union.
    git(&repo, &["commit", "-qam", "notes changed on the base"]);
    let moved = ctx_at(git(&repo, &["rev-parse", "HEAD"]));
    let got = checks(&moved);
    assert!(
        got.iter().any(|c| c.0 == "gate")
            && got
                .iter()
                .any(|c| c.0 == "test" && c.1.contains("changed on the base")),
        "{got:?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// (confirmation round on d32b2b2, codex P2) The probe runs the guide's `install` step (a check
/// named `install` in the same `cwd`) in EVERY fresh copy before probing — the retry's included,
/// and under `--check`. The runner here exists only after the install: `node_modules/.bin`,
/// generated from a committed source. An install that fails fails every probe.
#[cfg(unix)]
#[test]
fn the_probe_runs_the_guides_install_step_in_every_fresh_copy() {
    let _load = seam::pin(0.5, 4);
    let repo = scratch("probe-install");
    std::fs::create_dir_all(repo.join("tools")).unwrap();
    std::fs::write(repo.join("tools/runner.src"), STAND_IN).unwrap();
    // The first list fails: the whole probe re-runs on a fresh copy, which has no
    // `node_modules` until its own install runs.
    std::fs::write(repo.join("flaky.list"), "").unwrap();
    std::fs::write(
        repo.join(".gitignore"),
        "tmp/\nprobe-guide.json\nnode_modules/\n",
    )
    .unwrap();
    git_repo_with_commit(&repo);
    let test: serde_json::Value = serde_json::from_str(
        &stand_in_guide()
            .to_string()
            .replace("\"runner.sh\"", "\"node_modules/.bin/runner.sh\""),
    )
    .unwrap();
    let path = repo.join("probe-guide.json");
    let probe_with = |install: &str| {
        let install = serde_json::json!({"name": "install", "run": ["sh", "-c", install]});
        std::fs::write(
            &path,
            serde_json::json!({"version": 1, "checks": [install, test.clone()]}).to_string(),
        )
        .unwrap();
        crate::floor_probe::probe_guide(&repo, &path, Some("test"))
    };
    let generate = "mkdir -p node_modules/.bin && cp tools/runner.src node_modules/.bin/runner.sh";
    let results = match probe_with(generate) {
        Ok(mut all) => all.remove(0).1,
        Err(e) if e.contains("does not arm") => {
            assert!(!tools_required(), "the floor sandbox is required here: {e}");
            return;
        }
        Err(e) => panic!("the probe ran: {e}"),
    };
    for p in crate::floor_guide::REQUIRED_PROBES
        .iter()
        .chain(&["version"])
    {
        assert_eq!(
            results.get(*p).map(String::as_str),
            Some("pass (retried)"),
            "{p}: {results:?}"
        );
    }
    assert!(
        !repo.join("node_modules").exists(),
        "the install ran in the copies only"
    );
    // A failing install fails every probe, on the first copy and on the retry's.
    let results = probe_with("exit 7").unwrap().remove(0).1;
    assert_eq!(results.len(), crate::floor_guide::REQUIRED_PROBES.len() + 1);
    for (p, r) in &results {
        assert!(
            r.starts_with("fail: install failed") && r.ends_with(" (retried)"),
            "{p}: {r}"
        );
    }
    let _ = std::fs::remove_dir_all(&repo);
}

/// A guide derived from `notes.md` whose one check was verified on `verified_on`.
#[cfg(unix)]
fn notes_guide(verified_on: &str) -> crate::floor_guide::BuildGuide {
    let mut c = libtest_check(&["runner".to_string()]);
    c["verified"] = libtest_guide(&[], verified_on)["verified"].clone();
    serde_json::from_value(
        serde_json::json!({"version": 1, "derivedFrom": ["notes.md"], "checks": [c]}),
    )
    .unwrap()
}

/// (confirmation round on d32b2b2, agy P2) A staleness anchor that is not an ancestor of the
/// base — a pre-squash `verified.baseSha` still in the object store, or one a shallow clone does
/// not have — says nothing about what changed since: the guide is stale (fail-closed).
#[cfg(unix)]
#[test]
fn a_pre_squash_or_missing_verified_commit_makes_the_guide_stale() {
    let repo = scratch("stale-anchor");
    std::fs::write(repo.join("notes.md"), "v1\n").unwrap();
    git_repo_with_commit(&repo);
    let root = git(&repo, &["rev-parse", "HEAD"]);
    // A side line (the pre-squash branch), then the guide on the main line.
    git(&repo, &["checkout", "-qb", "side"]);
    std::fs::write(repo.join("side.txt"), "x\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "pre-squash"]);
    let side = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "-"]);
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    std::fs::write(repo.join(crate::floor_guide::GUIDE_PATH), "{}").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "the guide"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let git_dir = repo.join(".git");
    let none: Result<Vec<String>, String> = Ok(Vec::new());
    assert_eq!(
        notes_guide(&root).stale(&repo, &git_dir, &base, &none),
        None,
        "control: verified on an ancestor, nothing derived changed"
    );
    let why = notes_guide(&side)
        .stale(&repo, &git_dir, &base, &none)
        .expect("a pre-squash anchor is stale");
    assert!(why.contains("not an ancestor"), "{why}");
    // A shallow clone holds the base but not the commit the check was verified on.
    let shallow = scratch("stale-anchor-shallow");
    let _ = std::fs::remove_dir_all(&shallow);
    let url = format!("file://{}", repo.display());
    git(
        &repo,
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &url,
            &shallow.to_string_lossy(),
        ],
    );
    let why = notes_guide(&root)
        .stale(&shallow, &shallow.join(".git"), &base, &none)
        .expect("an anchor the shallow history lost is stale");
    // Either the boundary the clone was cut at, or the verified commit it lacks.
    assert!(
        why.contains("shallow boundary") || why.contains("not an ancestor"),
        "{why}"
    );
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&shallow);
}

/// A creator-stage fixture: a GREEN base whose guide check has an `affected` piece that always
/// passes, and a head whose full run is red with a new failure (only `affected` can pass it).
/// Tracked: `runner.toml`, `other.toml`. Exclusion is the caller's to switch.
#[cfg(unix)]
fn affected_fixture(tag: &str) -> (PathBuf, FloorContext) {
    let repo = scratch(tag);
    let argv = vec![fake_cargo(tag), "test".into(), "--no-fail-fast".into()];
    std::fs::write(repo.join(".gitignore"), "tmp/\n").unwrap();
    let toc = toc_of(&["new", "fine"]);
    write_tree(
        &repo,
        &argv,
        &[
            ("run.out", "test new ... ok\ntest fine ... ok\n"),
            ("run.exit", "0"),
            ("toc.txt", toc.as_str()),
        ],
    );
    std::fs::write(repo.join("runner.toml"), "setting = 1\n").unwrap();
    std::fs::write(repo.join("other.toml"), "setting = 2\n").unwrap();
    git_repo_with_commit(&repo);
    let ctx = FloorContext {
        base_head: Some(commit_guide_with_passing_affected(&repo, &argv)),
        stage: FloorStage::Creator,
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    write_tree(
        &repo,
        &argv,
        &[
            ("run.out", "test new ... FAILED\n"),
            ("run.exit", "101"),
            ("toc.txt", toc.as_str()),
        ],
    );
    (repo, ctx)
}

/// (confirmation round on d32b2b2, agy P2) A path deleted from the index but still on disk is
/// both removed (git's D list) and untracked (touched): read from the D/T list directly, it still
/// rules `affected` out — the full check runs and denies.
#[cfg(unix)]
#[test]
fn a_deleted_path_still_on_disk_untracked_runs_the_full_check() {
    let _load = seam::pin(0.5, 4);
    let _off = seam::exclude(false);
    let (repo, ctx) = affected_fixture("deleted-untracked");
    git(&repo, &["rm", "-q", "--cached", "other.toml"]);
    let other = "other.toml".to_string();
    assert!(touched_files(&repo, &ctx).unwrap().contains(&other));
    assert!(removed_files(&repo, &ctx).unwrap().contains(&other));
    let Some((report, c)) = floor_test(&repo, &ctx) else {
        return;
    };
    assert!(!report.passed, "the full, red check denies: {c:?}");
    assert!(!c.source.contains("affected set"), "{}", c.source);
    let _ = std::fs::remove_dir_all(&repo);
}

/// (confirmation round on d32b2b2, agy P2) In a stale union a covered directory's detected
/// `install` runs FIRST, before every guided check (a guided install with the same argv is that
/// install, once); a derived targeted creator check that a guided check already runs (argv and
/// dir) is not run twice.
#[cfg(unix)]
#[test]
fn a_stale_union_installs_first_and_runs_a_targeted_set_once() {
    let ci = vec![
        "npm".to_string(),
        "ci".into(),
        "--ignore-scripts".into(),
        "--no-audit".into(),
        "--no-fund".into(),
    ];
    // A Node package: detection adds `install` (no `node_modules/`) and `test`.
    let repo = scratch("stale-order-node");
    std::fs::write(
        repo.join("package.json"),
        r#"{"name":"p","scripts":{"test":"x"}}"#,
    )
    .unwrap();
    std::fs::write(repo.join("package-lock.json"), "{}").unwrap();
    git_repo_with_commit(&repo);
    let gate = serde_json::json!({"name": "gate", "run": ["sh", "-c", "exit 0"]});
    let install = serde_json::json!({"name": "install", "run": ci});
    write_guide(&repo, &[gate, install], &git(&repo, &["rev-parse", "HEAD"]));
    let path = repo.join(crate::floor_guide::GUIDE_PATH);
    let raw = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        raw.replace("\"derivedFrom\": []", "\"derivedFrom\": [\"package.json\"]"),
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "the guide"]);
    let ctx = FloorContext {
        base_head: Some(git(&repo, &["rev-parse", "HEAD"])),
        stage: FloorStage::Verify,
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    // Fresh: the guide's own order.
    let names = |ctx: &FloorContext| -> Vec<String> {
        detect_with(&repo, ctx)
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect()
    };
    assert_eq!(names(&ctx), ["gate", "install"]);
    // Stale (the change touches `package.json`): install first, once; then the guide; then the
    // detected rest.
    std::fs::write(
        repo.join("package.json"),
        r#"{"name":"p","scripts":{"test":"x"},"private":true}"#,
    )
    .unwrap();
    let got = detect_with(&repo, &ctx).unwrap();
    let order: Vec<&str> = got.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(order, ["install", "gate", "test"], "{got:#?}");
    assert!(got[0].source.contains("stale build guide"), "{got:#?}");
    let _ = std::fs::remove_dir_all(&repo);

    // A cargo package at the creator: detection derives `cargo test -p a` from the touched file;
    // the guide already runs exactly that.
    let repo = scratch("stale-order-cargo");
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    std::fs::write(repo.join("notes.md"), "v1\n").unwrap();
    git_repo_with_commit(&repo);
    let targeted = serde_json::json!({"name": "t-a", "run": ["cargo", "test", "-p", "a"]});
    write_guide(&repo, &[targeted], &git(&repo, &["rev-parse", "HEAD"]));
    let raw = std::fs::read_to_string(path_of(&repo)).unwrap();
    std::fs::write(
        path_of(&repo),
        raw.replace("\"derivedFrom\": []", "\"derivedFrom\": [\"notes.md\"]"),
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "the guide"]);
    let ctx = FloorContext {
        base_head: Some(git(&repo, &["rev-parse", "HEAD"])),
        stage: FloorStage::Creator,
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    std::fs::write(repo.join("notes.md"), "v2\n").unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn f() -> u8 {\n    1\n}\n").unwrap();
    let got = detect_with(&repo, &ctx).unwrap();
    let ta = ["cargo", "test", "-p", "a"];
    assert_eq!(
        got.iter().filter(|c| c.argv == ta).count(),
        1,
        "the targeted set once: {got:#?}"
    );
    assert!(
        got.iter().any(|c| c.source.contains("stale build guide")),
        "the guide is stale here: {got:#?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

#[cfg(unix)]
fn path_of(repo: &Path) -> PathBuf {
    repo.join(crate::floor_guide::GUIDE_PATH)
}

/// One plain check run of `argv` in `wt` with the guide env `env`, under `sandbox`.
#[cfg(unix)]
fn run_with_env(
    wt: &Path,
    argv: &[&str],
    env: &[(&str, &str)],
    sandbox: &crate::validator::WorkerSandbox,
) -> CheckRun {
    let scratch = CheckScratch::prepare(wt).expect("scratch");
    let check = RepoCheck {
        name: "gate".into(),
        argv: argv.iter().map(|a| a.to_string()).collect(),
        source: String::new(),
        timeout_s: Some(30),
        dir: String::new(),
    };
    let exec = ExecOpts {
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        merge: false,
    };
    run_one_with(wt, None, &check, &exec, sandbox, &scratch, Tree::Head)
}

/// (confirmation round on 5f93846, codex P1) A guide's env reaches the command INSIDE the
/// boundary only: the launcher (here a stand-in that records its own environment, then execs the
/// rest) gets the floor's sanitized environment and never the guide's; the command gets it. With
/// no launcher, the command still gets it.
#[cfg(unix)]
#[test]
fn the_guide_env_reaches_the_command_inside_the_boundary_never_the_launcher() {
    let wt = scratch("guide-env-launcher");
    let launcher_env = wt.join("launcher.env");
    let launcher = crate::validator::WorkerSandbox {
        wrapper: vec![
            "/bin/sh".into(),
            "-c".into(),
            "env > \"$0\"; exec \"$@\"".into(),
            launcher_env.to_string_lossy().into_owned(),
        ],
        level: crate::validator::SandboxLevel::Sandboxed,
        downgrade_reason: None,
    };
    let inner = ["sh", "-c", "printf %s \"$GUIDE_K\" > inner.out"];
    let r = run_with_env(&wt, &inner, &[("GUIDE_K", "guided")], &launcher);
    assert!(r.passed(), "{r:?}");
    let seen = std::fs::read_to_string(&launcher_env).unwrap();
    assert!(
        !seen.contains("GUIDE_K"),
        "the launcher saw the guide env: {seen}"
    );
    assert!(
        seen.contains("TMPDIR="),
        "the launcher has the floor's own env: {seen}"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("inner.out")).unwrap(),
        "guided"
    );
    // No launcher: the spawned process is the command, and it carries the env.
    std::fs::remove_file(wt.join("inner.out")).unwrap();
    let none = crate::validator::WorkerSandbox {
        wrapper: Vec::new(),
        level: crate::validator::SandboxLevel::BestEffort,
        downgrade_reason: Some("test".into()),
    };
    let r = run_with_env(&wt, &inner, &[("GUIDE_K", "plain")], &none);
    assert!(r.passed(), "{r:?}");
    assert_eq!(
        std::fs::read_to_string(wt.join("inner.out")).unwrap(),
        "plain"
    );
    let _ = std::fs::remove_dir_all(&wt);
}

/// (confirmation round on 5f93846, codex P1) Linux, the real floor sandbox: a preload library
/// whose constructor writes a sentinel OUTSIDE every write root. Handed to the run as guide env
/// (the load check refuses the name; this proves the boundary by itself), it loads only inside
/// bwrap, where that write fails — never in the launcher before it arms. Skipped without a C
/// compiler or an armed floor sandbox.
#[cfg(target_os = "linux")]
#[test]
fn a_preload_in_the_guide_env_never_runs_outside_the_boundary() {
    let wt = scratch("preload-wt");
    let outside = scratch("preload-outside");
    let sentinel = outside.join("sentinel");
    let lib_dir = scratch("preload-lib");
    let src = lib_dir.join("hijack.c");
    std::fs::write(
        &src,
        format!(
            "#include <fcntl.h>\n#include <unistd.h>\n\
             __attribute__((constructor)) static void hijack(void) {{\n\
             int fd = open(\"{}\", O_CREAT | O_WRONLY, 0644);\n\
             if (fd >= 0) close(fd);\n}}\n",
            sentinel.display()
        ),
    )
    .unwrap();
    let lib = lib_dir.join("hijack.so");
    // spawn-audit: test-only — builds the stand-in preload library with the host's C compiler.
    let built = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-o"])
        .arg(&lib)
        .arg(&src)
        .status();
    if !built.is_ok_and(|s| s.success()) {
        eprintln!("repo_checks: no C compiler here — skipping the preload test");
        return;
    }
    // Control: the library does write the sentinel when nothing confines it.
    // spawn-audit: test-only — the unconfined control: proves the stand-in hook writes the sentinel.
    let probe_run = std::process::Command::new("true")
        .env("LD_PRELOAD", &lib)
        .status()
        .unwrap();
    assert!(
        probe_run.success() && sentinel.exists(),
        "the stand-in hook works"
    );
    std::fs::remove_file(&sentinel).unwrap();
    let scratch_dir = CheckScratch::prepare(&wt).expect("scratch");
    let sandbox = crate::repo_checks::floor_sandbox(&[
        wt.clone(),
        scratch_dir.tmp_path().to_path_buf(),
        scratch_dir.root().to_path_buf(),
    ]);
    if sandbox.level != crate::validator::SandboxLevel::Sandboxed {
        eprintln!("repo_checks: the floor sandbox does not arm here — skipping");
        return;
    }
    let lib_s = lib.to_string_lossy().into_owned();
    let _ = run_with_env(&wt, &["true"], &[("LD_PRELOAD", lib_s.as_str())], &sandbox);
    assert!(
        !sentinel.exists(),
        "a preload in the guide env ran outside the boundary"
    );
    for d in [&wt, &outside, &lib_dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// (confirmation round on 5f93846, codex P2) A depth-1 clone: the only commit is the shallow
/// boundary, where `git log` sees the guide "added". A gate-only guide (no verified block to
/// anchor on) whose `derivedFrom` file changed before the cut cannot show it is fresh: it stays in
/// the stale union.
#[cfg(unix)]
#[test]
fn a_shallow_boundary_anchor_keeps_a_gate_only_guide_stale() {
    let repo = scratch("shallow-gate");
    let argv = vec!["detected-runner".to_string(), "test".into()];
    write_tree(&repo, &argv, &[]);
    std::fs::write(repo.join("notes.md"), "v1\n").unwrap();
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    std::fs::write(
        path_of(&repo),
        serde_json::json!({"version": 1, "derivedFrom": ["notes.md"],
            "checks": [{"name": "gate", "run": ["sh", "-c", "exit 0"]}]})
        .to_string(),
    )
    .unwrap();
    git_repo_with_commit(&repo);
    std::fs::write(repo.join("notes.md"), "v2\n").unwrap();
    git(&repo, &["commit", "-qam", "notes changed after the guide"]);
    let full_ctx = FloorContext {
        base_head: Some(git(&repo, &["rev-parse", "HEAD"])),
        stage: FloorStage::Verify,
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    let names = |wt: &Path, ctx: &FloorContext| -> Vec<String> {
        detect_with(wt, ctx)
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect()
    };
    // The complete history: the guide's commit predates the change — stale.
    assert!(names(&repo, &full_ctx).contains(&"test".to_string()));
    let shallow = scratch("shallow-gate-clone");
    let _ = std::fs::remove_dir_all(&shallow);
    let url = format!("file://{}", repo.display());
    git(
        &repo,
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &url,
            &shallow.to_string_lossy(),
        ],
    );
    assert_eq!(
        git(&shallow, &["rev-parse", "--is-shallow-repository"]),
        "true"
    );
    let ctx = FloorContext {
        git_dir: Some(shallow.join(".git")),
        ..full_ctx
    };
    let got = names(&shallow, &ctx);
    assert!(
        got.contains(&"gate".to_string()) && got.contains(&"test".to_string()),
        "the stale union: {got:?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&shallow);
}

/// (sweep review on #1066, P1) The base export is removed through the worktree held before any
/// check ran: a `tmp` swapped (after the handle was opened) for a link to an outside directory
/// holding a `wicked-floor-base` is refused and the outside directory survives; a directory INSIDE
/// the export swapped for a link has the link removed, never what it points at.
#[cfg(unix)]
#[test]
fn the_base_export_is_removed_through_the_held_worktree_never_through_a_swapped_link() {
    let wt = scratch("held-remove");
    let outside = scratch("held-remove-outside");
    let export = wt.join("tmp").join(BASE_EXPORT_SUBDIR);
    std::fs::create_dir_all(export.join("src")).unwrap();
    std::fs::write(export.join("src/a.rs"), "x").unwrap();
    let precious = outside.join(BASE_EXPORT_SUBDIR).join("keep/file");
    std::fs::create_dir_all(precious.parent().unwrap()).unwrap();
    std::fs::write(&precious, "precious").unwrap();
    let held = HeldDir::open(&wt).unwrap();
    // A command (or a process it left running) swaps `tmp` for a link out of the tree.
    std::fs::rename(wt.join("tmp"), wt.join("tmp-moved")).unwrap();
    std::os::unix::fs::symlink(&outside, wt.join("tmp")).unwrap();
    let e = remove_base_export(&held).expect_err("a linked `tmp` is refused");
    assert!(e.contains("not a real directory"), "{e}");
    assert!(precious.is_file(), "nothing removed through the link");
    // The real `tmp` back; a directory inside the export swapped for a link out of the tree.
    std::fs::remove_file(wt.join("tmp")).unwrap();
    std::fs::rename(wt.join("tmp-moved"), wt.join("tmp")).unwrap();
    std::fs::remove_dir_all(export.join("src")).unwrap();
    std::os::unix::fs::symlink(outside.join(BASE_EXPORT_SUBDIR), export.join("src")).unwrap();
    remove_base_export(&held).unwrap();
    assert!(
        std::fs::symlink_metadata(&export).is_err(),
        "the export is gone"
    );
    assert!(precious.is_file(), "the link was removed, never followed");
    // `remove_tree` itself, on the leaf: a link there is unlinked, its target kept.
    std::fs::create_dir_all(wt.join("tmp")).unwrap();
    std::os::unix::fs::symlink(&outside, wt.join("tmp/leaf")).unwrap();
    assert_eq!(held.remove_tree("tmp/leaf"), Ok(true));
    assert!(precious.is_file());
    let _ = std::fs::remove_dir_all(&wt);
    let _ = std::fs::remove_dir_all(&outside);
}

/// (sweep review on #1066, P1/P2) A results file is opened through the held tree and checked on
/// the OPENED file: a tree renamed away with a link left in its place reads nothing from the
/// link's target; a FIFO is no regular file (and never blocks); an oversized file is refused.
#[cfg(unix)]
#[test]
fn a_results_file_is_read_through_the_held_tree_only() {
    let root = scratch("held-results");
    let outside = scratch("held-results-outside");
    let since = std::time::SystemTime::now();
    std::fs::write(outside.join("report.xml"), "<testsuite name=\"outside\"/>").unwrap();
    let held = HeldDir::open(&root).unwrap();
    let moved = root.with_file_name(format!(
        "{}-moved",
        root.file_name().unwrap().to_string_lossy()
    ));
    std::fs::rename(&root, &moved).unwrap();
    std::os::unix::fs::symlink(&outside, &root).unwrap();
    let e = open_results(&held, "report.xml", since).expect_err("nothing in the held tree");
    assert!(e.contains("no results file"), "{e}");
    // What the held (renamed) tree holds is what is read.
    std::fs::write(moved.join("report.xml"), "<testsuite name=\"held\"/>").unwrap();
    let mut text = String::new();
    std::io::Read::read_to_string(
        &mut open_results(&held, "report.xml", since).unwrap(),
        &mut text,
    )
    .unwrap();
    assert!(text.contains("held"), "{text}");
    // A FIFO at the results path.
    let fifo = std::ffi::CString::new(moved.join("fifo.xml").to_string_lossy().as_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    let e = open_results(&held, "fifo.xml", since).expect_err("a FIFO");
    assert!(e.contains("not a regular file"), "{e}");
    // An oversized file (sparse).
    std::fs::File::create(moved.join("big.xml"))
        .unwrap()
        .set_len(crate::floor_log::MAX_STREAM_BYTES + 1)
        .unwrap();
    let e = open_results(&held, "big.xml", since).expect_err("oversized");
    assert!(e.contains("larger than the log cap"), "{e}");
    let _ = std::fs::remove_file(&root);
    let _ = std::fs::remove_dir_all(&moved);
    let _ = std::fs::remove_dir_all(&outside);
}

/// (sweep review on #1066, P2) No command starts in a held tree whose root, or whose check
/// directory, was swapped for a link after the handle was opened: the run could not start.
#[cfg(unix)]
#[test]
fn no_command_starts_in_a_held_tree_swapped_for_a_link() {
    let wt = scratch("held-spawn");
    let outside = scratch("held-spawn-outside");
    std::fs::create_dir_all(wt.join("pkg")).unwrap();
    std::fs::create_dir_all(outside.join("pkg")).unwrap();
    let marker = outside.join("ran");
    let held = HeldDir::open(&wt).unwrap();
    let scratch_dir = CheckScratch::prepare(&wt).unwrap();
    let sandbox = crate::validator::WorkerSandbox {
        wrapper: Vec::new(),
        level: crate::validator::SandboxLevel::BestEffort,
        downgrade_reason: None,
    };
    let check = |dir: &str| RepoCheck {
        name: "test".into(),
        argv: s(&["sh", "-c", &format!("touch '{}'", marker.display())]),
        source: "test".into(),
        timeout_s: Some(30),
        dir: dir.into(),
    };
    let run = |c: &RepoCheck| {
        run_one_with(
            &wt,
            Some(&held),
            c,
            &ExecOpts::default(),
            &sandbox,
            &scratch_dir,
            Tree::Base,
        )
    };
    // The check's directory swapped for a link out of the tree.
    std::fs::rename(wt.join("pkg"), wt.join("pkg-moved")).unwrap();
    std::os::unix::fs::symlink(outside.join("pkg"), wt.join("pkg")).unwrap();
    let r = run(&check("pkg"));
    assert!(r.spawn_error.is_some(), "{r:?}");
    // The root swapped for a link out of the tree.
    let moved = wt.with_file_name(format!(
        "{}-moved",
        wt.file_name().unwrap().to_string_lossy()
    ));
    std::fs::rename(&wt, &moved).unwrap();
    std::os::unix::fs::symlink(&outside, &wt).unwrap();
    let r = run(&check(""));
    assert!(
        r.spawn_error
            .as_deref()
            .is_some_and(|e| e.contains("no longer the directory the floor held")),
        "{r:?}"
    );
    assert!(!marker.exists(), "a command ran outside the held tree");
    std::fs::remove_file(&wt).unwrap();
    std::fs::rename(&moved, &wt).unwrap();
    drop(scratch_dir);
    let _ = remove_check_scratch_in(&check_cache_root().unwrap(), &wt, None);
    let _ = std::fs::remove_dir_all(&wt);
    let _ = std::fs::remove_dir_all(&outside);
}

/// (codex on #1066, P2) A check STARTS in the held directory itself, never in what its pathname
/// names at spawn time: a swap of the check directory for a link to an outside directory AFTER
/// the directory checks and BEFORE the child's chdir (a detached descendant of an earlier check)
/// leaves the command running in the original — now detached — directory.
#[cfg(unix)]
#[test]
fn a_check_starts_in_the_held_directory_when_its_path_is_swapped_before_spawn() {
    if crate::validator::skip_when_nested(
        "a_check_starts_in_the_held_directory_when_its_path_is_swapped_before_spawn",
    ) {
        return;
    }
    let wt = scratch("held-fchdir");
    let outside = scratch("held-fchdir-outside");
    std::fs::create_dir_all(wt.join("pkg")).unwrap();
    std::fs::create_dir_all(outside.join("pkg")).unwrap();
    let held = HeldDir::open(&wt).unwrap();
    let scratch_dir = CheckScratch::prepare(&wt).unwrap();
    let sandbox = crate::validator::WorkerSandbox {
        wrapper: Vec::new(),
        level: crate::validator::SandboxLevel::BestEffort,
        downgrade_reason: None,
    };
    let check = RepoCheck {
        name: "test".into(),
        argv: s(&["sh", "-c", "touch here"]),
        source: "test".into(),
        timeout_s: Some(30),
        dir: "pkg".into(),
    };
    let (w, o) = (wt.clone(), outside.clone());
    seam::before_spawn(move || {
        std::fs::rename(w.join("pkg"), w.join("pkg-moved")).unwrap();
        std::os::unix::fs::symlink(o.join("pkg"), w.join("pkg")).unwrap();
    });
    let r = run_one_with(
        &wt,
        Some(&held),
        &check,
        &ExecOpts::default(),
        &sandbox,
        &scratch_dir,
        Tree::Head,
    );
    assert!(
        std::fs::symlink_metadata(wt.join("pkg"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the swap ran before the spawn"
    );
    assert!(
        !outside.join("pkg").join("here").exists(),
        "the check ran in the outside directory: {r:?}"
    );
    assert!(r.spawn_error.is_none() && r.exit_code == Some(0), "{r:?}");
    assert!(
        wt.join("pkg-moved").join("here").is_file(),
        "the check ran in the held directory: {r:?}"
    );
    drop(scratch_dir);
    let _ = remove_check_scratch_in(&check_cache_root().unwrap(), &wt, None);
    let _ = std::fs::remove_dir_all(&wt);
    let _ = std::fs::remove_dir_all(&outside);
}

/// (codex on #1066, P2) A symlink entry materialized without symlinks (non-unix: a regular file
/// holding the link's target) is unchanged only while it holds exactly that target — a check that
/// rewrote the placeholder is a change to what ships. The comparison runs on every platform.
#[test]
fn a_link_placeholder_is_compared_by_content() {
    let dir = scratch("link-placeholder");
    let held = HeldDir::open(&dir).unwrap();
    held.write_bytes("sub/link", b"app.txt", false).unwrap();
    assert!(same_placeholder(&held, "sub/link", b"app.txt"));
    // Same length, other target.
    std::fs::write(dir.join("sub").join("link"), b"new.txt").unwrap();
    assert!(!same_placeholder(&held, "sub/link", b"app.txt"));
    // Longer.
    std::fs::write(dir.join("sub").join("link"), b"app.txt.bak").unwrap();
    assert!(!same_placeholder(&held, "sub/link", b"app.txt"));
    // Not a file at all.
    std::fs::remove_file(dir.join("sub").join("link")).unwrap();
    std::fs::create_dir(dir.join("sub").join("link")).unwrap();
    assert!(!same_placeholder(&held, "sub/link", b"app.txt"));
    assert!(!same_placeholder(&held, "sub/missing", b"app.txt"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// (sweep review on #1066, P3) A red base that exited OUTSIDE the guide's `failedExit` did not run
/// to a test verdict: nothing of it is inventoried or excluded — the head's failure escalates as
/// `base_red_unseparated`, even though its results name the failure and the alone run confirms it.
#[cfg(unix)]
#[test]
fn a_base_that_exited_outside_failed_exit_is_never_inventoried() {
    let _load = seam::pin(0.5, 4);
    let (repo, ctx, argv) = red_base("base-crash");
    let toc = toc_of(&["old", "new", "fine"]);
    let named = "test new ... ok\ntest fine ... ok\ntest old ... FAILED\n\nfailures:\n    old\n\n\
                 test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n";
    // The base names `old` failing — and exits 2, not one of `failedExit` [101].
    write_tree(
        &repo,
        &argv,
        &[
            ("run.out", named),
            ("run.exit", "2"),
            ("toc.txt", &toc),
            ("alone.fail", "old\n"),
        ],
    );
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "commit",
            "-qm",
            "a base that crashed after naming a failure",
        ],
    );
    let ctx = FloorContext {
        base_head: Some(git(&repo, &["rev-parse", "HEAD"])),
        ..ctx.clone()
    };
    write_tree(
        &repo,
        &argv,
        &[
            ("run.out", named),
            ("run.exit", "101"),
            ("toc.txt", &toc),
            ("excluded.exit", "0"),
        ],
    );
    let Some((report, c)) = floor_test(&repo, &ctx) else {
        return;
    };
    assert!(!report.passed, "{c:?}");
    let d = c.decision.expect("a decision");
    assert_eq!(d.outcome, OUTCOME_BASE_RED_UNSEPARATED, "{d:?}");
    assert!(
        d.reason
            .as_deref()
            .is_some_and(|r| r.contains("not one of the guide's `failedExit` codes")),
        "{d:?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

// ─── J4: a floor a daemon restart interrupted resumes on the same tree ─────────────────────────

/// A repo whose `lint` and `test` checks each append a line to a counter file per run, so a test
/// can tell which checks a floor actually ran.
#[cfg(unix)]
fn counting_repo(tag: &str) -> (PathBuf, FloorContext) {
    let repo = scratch(tag);
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    std::fs::write(repo.join(".gitignore"), "tmp/\ncounts.*\n").unwrap();
    std::fs::write(
        repo.join(CONFIG_PATH),
        serde_json::json!({
            "lint": ["sh", "-c", "echo x >> counts.lint"],
            "test": ["sh", "-c", "echo x >> counts.test"],
        })
        .to_string(),
    )
    .unwrap();
    git_repo_with_commit(&repo);
    let ctx = FloorContext {
        stage: FloorStage::Verify,
        run_id: Some(format!("{tag}-{}", std::process::id())),
        ..FloorContext::default()
    };
    (repo, ctx)
}

#[cfg(unix)]
fn runs_of(repo: &Path, name: &str) -> usize {
    std::fs::read_to_string(repo.join(format!("counts.{name}")))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// Leave behind the journal an interrupted floor of `ord` (attempt `attempt`, on `tree`) would
/// have: `finished` recorded, the rest not yet run.
#[cfg(unix)]
fn interrupted_journal(
    repo: &Path,
    ctx: &FloorContext,
    report: &RepoChecksReport,
    ord: u32,
    attempt: u32,
    tree: &str,
    finished: &[&str],
) {
    let logs = CheckScratch::prepare_for(repo, ctx.run_id.as_deref())
        .unwrap()
        .logs
        .clone();
    let checks = report
        .detected
        .iter()
        .filter(|c| finished.contains(&c.name.as_str()))
        .map(|c| JournalEntry {
            dir: c.dir.clone(),
            name: c.name.clone(),
            argv: c.argv.clone(),
            run: report
                .checks
                .iter()
                .find(|r| r.name == c.name)
                .unwrap()
                .clone(),
        })
        .collect();
    let rec = JournalRecord {
        floor_policy: FLOOR_POLICY_VERSION,
        key: FloorKey::of(tree, ctx, &report.detected),
        exclude: exclusion_enabled(),
        attempt,
        checks,
    };
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        journal_path(&logs, ord),
        serde_json::to_string(&rec).unwrap(),
    )
    .unwrap();
}

/// (J4, rig run 29bcace4) The daemon died mid-floor: `lint` had finished on tree T, `test` had
/// not. The unit's next floor on T, same base and checks, reuses `lint` (it does not run again)
/// and runs only `test`; it says so on the report, and its journal is gone once it ends.
#[cfg(unix)]
#[test]
fn a_floor_a_restart_interrupted_resumes_running_only_the_unfinished_checks() {
    let (repo, ctx) = counting_repo("resume-floor");
    let tree = "1111111111111111111111111111111111111111";
    let first = run_floor_journaled(&repo, &ctx, None, None);
    if first.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return;
    }
    assert!(first.passed, "{first:#?}");
    assert!(first.resumed.is_none());
    assert_eq!((runs_of(&repo, "lint"), runs_of(&repo, "test")), (1, 1));
    interrupted_journal(&repo, &ctx, &first, 6, 8, tree, &["lint"]);

    let j = FloorJournal {
        ord: 6,
        attempt: 9,
        tree: tree.to_string(),
        phase: None,
    };
    let resumed = run_floor_journaled(&repo, &ctx, None, Some(&j));
    assert!(resumed.passed, "{resumed:#?}");
    assert_eq!(
        (runs_of(&repo, "lint"), runs_of(&repo, "test")),
        (1, 2),
        "lint finished before the restart and is reused; only test runs again"
    );
    assert_eq!(
        resumed.resumed,
        Some(FloorResume {
            from_attempt: 8,
            reused: vec!["lint".to_string()],
            restarted: None
        })
    );
    assert_eq!(
        resumed
            .checks
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        names(&resumed.detected),
        "the report still carries every check, reused ones included"
    );
    let note = resumed.classification_note().unwrap_or_default();
    assert!(note.contains("resumed the floor of attempt 8"), "{note}");

    // The journal went with the floor's end: the next floor of the unit resumes nothing.
    let again = run_floor_journaled(&repo, &ctx, None, Some(&j));
    assert!(again.resumed.is_none(), "{:?}", again.resumed);
    assert_eq!((runs_of(&repo, "lint"), runs_of(&repo, "test")), (2, 3));
    let _ = std::fs::remove_dir_all(&repo);
}

/// The interrupted floor ran on ANOTHER tree (the re-dispatched step changed the work): nothing is
/// reused, every check runs, and the report says the floor restarted and why.
#[cfg(unix)]
#[test]
fn an_interrupted_floor_on_another_tree_restarts_and_says_so() {
    let (repo, ctx) = counting_repo("resume-other-tree");
    let first = run_floor_journaled(&repo, &ctx, None, None);
    if first.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return;
    }
    interrupted_journal(
        &repo,
        &ctx,
        &first,
        6,
        8,
        "1111111111111111111111111111111111111111",
        &["lint", "test"],
    );
    let j = FloorJournal {
        ord: 6,
        attempt: 9,
        tree: "2222222222222222222222222222222222222222".to_string(),
        phase: None,
    };
    let r = run_floor_journaled(&repo, &ctx, None, Some(&j));
    assert_eq!((runs_of(&repo, "lint"), runs_of(&repo, "test")), (2, 2));
    let resume = r.resumed.clone().expect("the restart is disclosed");
    assert_eq!(resume.from_attempt, 8);
    assert!(resume.reused.is_empty());
    assert!(
        resume
            .restarted
            .as_deref()
            .unwrap_or_default()
            .contains("tree"),
        "{resume:?}"
    );
    assert!(r
        .classification_note()
        .unwrap_or_default()
        .contains("could not be resumed"));
    let _ = std::fs::remove_dir_all(&repo);
}

/// An operator's re-run never stands on a journal: its check set is not the ordinary floor's.
#[cfg(unix)]
#[test]
fn an_operator_rerun_ignores_an_interrupted_floors_journal() {
    let (repo, ctx) = counting_repo("resume-rerun");
    let tree = "1111111111111111111111111111111111111111";
    let first = run_floor_journaled(&repo, &ctx, None, None);
    if first.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return;
    }
    interrupted_journal(&repo, &ctx, &first, 6, 8, tree, &["lint"]);
    let rerun = FloorRerun {
        mode: FloorRerunMode::Extend,
        waive: Vec::new(),
        output: String::new(),
        fix: None,
    };
    let j = FloorJournal {
        ord: 6,
        attempt: 9,
        tree: tree.to_string(),
        phase: None,
    };
    let r = run_floor_journaled(&repo, &ctx, Some(&rerun), Some(&j));
    assert!(r.resumed.is_none());
    assert_eq!(runs_of(&repo, "lint"), 2, "the re-run ran lint itself");
    let _ = std::fs::remove_dir_all(&repo);
}

/// (PRODUCT-WORKS J4) The floor's copies are what `git checkout` would write: a `.cmd` file stored
/// LF under `*.cmd text eol=crlf` is CRLF in the base export AND in the guarded (verify/deliver)
/// checkout — converted under the attributes of the EXPORTED tree (the head's uncommitted,
/// conflicting `.gitattributes` is never consulted for the base) — and an untouched converted
/// checkout still passes `hold_to`, while a changed byte does not.
#[cfg(unix)]
#[test]
fn the_floors_copies_keep_the_declared_line_endings() {
    let _load = seam::pin(0.5, 4);
    if !git_has_attr_source() {
        eprintln!("SKIP: this git has no --attr-source (< 2.40); the export writes raw blobs (see without_attr_source_the_export_writes_the_raw_blobs)");
        return;
    }
    let repo = scratch("export-eol");
    std::fs::write(repo.join(".gitattributes"), "*.cmd text eol=crlf\n").unwrap();
    std::fs::write(repo.join("run.cmd"), "echo ok\n").unwrap();
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    std::fs::write(repo.join("sub/b.cmd"), "a\nb\n").unwrap();
    std::fs::write(repo.join("plain.txt"), "keep\n").unwrap();
    std::os::unix::fs::symlink("run.cmd", repo.join("link.cmd")).unwrap();
    let head = git_repo_with_commit(&repo);
    // The stored blob is LF.
    assert_eq!(git(&repo, &["cat-file", "blob", "HEAD:run.cmd"]), "echo ok");
    let git_dir = repo.join(".git");
    let tree_id = git(&repo, &["rev-parse", "HEAD^{tree}"]);

    // The head's attributes now say the opposite (uncommitted): the base must not read them.
    std::fs::write(repo.join(".gitattributes"), "*.cmd -text\n").unwrap();
    let _scratch = CheckScratch::prepare(&repo).unwrap();
    let root = crate::repo_checks::HeldDir::open(&repo).unwrap();
    let base = BaseTree::export(&repo, &root, &git_dir, &head).unwrap();
    assert_eq!(
        std::fs::read(base.dir.join("run.cmd")).unwrap(),
        b"echo ok\r\n"
    );
    assert_eq!(
        std::fs::read(base.dir.join("sub/b.cmd")).unwrap(),
        b"a\r\nb\r\n"
    );
    assert_eq!(
        std::fs::read(base.dir.join("plain.txt")).unwrap(),
        b"keep\n"
    );
    // A symlink target is never converted.
    assert_eq!(
        std::fs::read_link(base.dir.join("link.cmd")).unwrap(),
        Path::new("run.cmd")
    );
    let _ = remove_base_export(&root);

    // The guarded checkout of the committed tree (the verify/deliver copy).
    std::fs::write(repo.join(".gitattributes"), "*.cmd text eol=crlf\n").unwrap();
    let checkout = crate::repo_checks::checkout_guarded_tree(&repo, &git_dir, &tree_id, "eol")
        .expect("guarded checkout");
    let dir = checkout.dir.clone();
    assert_eq!(std::fs::read(dir.join("run.cmd")).unwrap(), b"echo ok\r\n");
    assert_eq!(std::fs::read(dir.join("sub/b.cmd")).unwrap(), b"a\r\nb\r\n");
    // Untouched, the converted copy passes the integrity re-check…
    let mut report = guarded_report_stub(&repo);
    checkout.hold_to(&tree_id, &mut report);
    assert!(report.passed, "an untouched converted checkout: {report:?}");
    // …and one changed byte does not.
    std::fs::write(dir.join("run.cmd"), b"echo no\r\n").unwrap();
    let mut changed = guarded_report_stub(&repo);
    checkout.hold_to(&tree_id, &mut changed);
    assert!(!changed.passed, "a changed byte must fail: {changed:?}");
    // Even the raw LF blob (what the old export wrote) is a change now.
    std::fs::write(dir.join("run.cmd"), b"echo ok\n").unwrap();
    let mut raw = guarded_report_stub(&repo);
    checkout.hold_to(&tree_id, &mut raw);
    assert!(
        !raw.passed,
        "the unconverted bytes are not the checkout: {raw:?}"
    );
    drop(checkout);
    let _ = std::fs::remove_dir_all(&repo);
}

/// On a git without `--attr-source` (older than 2.40) the export falls back to the raw blobs, as
/// before eol conversion existed, instead of failing every floor export: every entry is still
/// written, a regular file holds its stored bytes and a symlink keeps its target.
#[cfg(unix)]
#[test]
fn without_attr_source_the_export_writes_the_raw_blobs() {
    let _load = seam::pin(0.5, 4);
    let repo = scratch("export-raw");
    std::fs::write(repo.join(".gitattributes"), "*.cmd text eol=crlf\n").unwrap();
    std::fs::write(repo.join("run.cmd"), "echo ok\n").unwrap();
    std::os::unix::fs::symlink("run.cmd", repo.join("link.cmd")).unwrap();
    let head = git_repo_with_commit(&repo);
    let git_dir = repo.join(".git");
    let tree_id = git(&repo, &["rev-parse", "HEAD^{tree}"]);
    let out = scratch("export-raw-out");
    let held = crate::repo_checks::HeldDir::open(&out).unwrap();
    let conv = scratch("export-raw-conv");
    let conv_held = crate::repo_checks::HeldDir::open(&conv).unwrap();
    write_private_git_dir(&conv_held, "git", &git_dir, &repo, &head, true).unwrap();
    let written = write_tree_blobs_with(
        &repo,
        &git_dir,
        &tree_id,
        &out,
        &held,
        &conv.join("git"),
        false,
    )
    .unwrap();
    assert_eq!(written.len(), 3, "every entry is written: {written:?}");
    assert_eq!(std::fs::read(out.join("run.cmd")).unwrap(), b"echo ok\n");
    assert_eq!(
        std::fs::read_link(out.join("link.cmd")).unwrap(),
        Path::new("run.cmd")
    );
    for d in [&repo, &out, &conv] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// (codex on #1127, P2) Configuration handed down through the environment (`GIT_CONFIG_COUNT` /
/// `GIT_CONFIG_KEY_<n>`, `GIT_CONFIG_PARAMETERS`) never reaches the export's private git: a
/// `filter.<x>.smudge` driver injected that way would otherwise run under `cat-file --filters`.
#[cfg(unix)]
#[test]
fn inherited_git_config_never_reaches_the_private_repository() {
    let repo = scratch("export-env-config");
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    git_repo_with_commit(&repo);
    let ask = |isolate: bool, key: &str| -> bool {
        // spawn-audit: hardened — `git config --get` on the test's own scratch repository.
        let mut cmd = std::process::Command::new("git");
        cmd.hardened()
            .args(["config", "--get", key])
            .current_dir(&repo)
            .env("GIT_DIR", repo.join(".git"))
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "filter.evil.smudge")
            .env("GIT_CONFIG_VALUE_0", "touch /nonexistent/ran")
            .env(
                "GIT_CONFIG_PARAMETERS",
                "'filter.evil.process'='touch /nonexistent/ran'",
            )
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if isolate {
            isolate_git_config(&mut cmd);
        }
        cmd.status().unwrap().success()
    };
    // Without isolation the environment does hand the driver over (the test's premise)…
    assert!(ask(false, "filter.evil.smudge"));
    assert!(ask(false, "filter.evil.process"));
    // …and isolated, it does not.
    assert!(!ask(true, "filter.evil.smudge"));
    assert!(!ask(true, "filter.evil.process"));
    let _ = std::fs::remove_dir_all(&repo);
}

/// (codex on #1127, P2) A tree entry that Windows would resolve to the export's `.git` (NTFS
/// aliases, as git's `is_ntfs_dotgit`) is refused like `.git` itself; ordinary names are not.
#[test]
fn a_tree_entry_aliasing_dot_git_is_refused() {
    for alias in [
        ".git",
        ".GIT",
        ".git.",
        ".git ",
        ".git. .",
        "git~1",
        "GIT~1",
        ".git::$INDEX_ALLOCATION",
        "git~1:stream",
    ] {
        assert!(names_git_metadata(alias), "{alias:?} aliases .git");
    }
    for name in [
        ".gitignore",
        ".github",
        "git",
        "git~2",
        ".gitattributes",
        "x.git",
        ".git-blame-ignore-revs",
    ] {
        assert!(!names_git_metadata(name), "{name:?} is an ordinary entry");
    }
}

/// A passing report to hand `hold_to` (it only ever flips `passed` to false).
#[cfg(unix)]
fn guarded_report_stub(repo: &Path) -> RepoChecksReport {
    let mut r = guarded_checkout_refused(repo, &FloorContext::default(), "stub");
    r.passed = true;
    r.detect_error = None;
    r
}

/// (PRODUCT-WORKS J5) A git call inside a base check resolves to the BASE: its own toplevel, HEAD
/// and index — from the root and from a package subdirectory — never the head worktree's index or
/// its untracked `tmp/`.
#[cfg(unix)]
#[test]
fn git_inside_a_base_check_sees_the_base_and_never_the_head() {
    let _load = seam::pin(0.5, 4);
    let repo = scratch("base-git-context");
    std::fs::create_dir_all(repo.join("pkg")).unwrap();
    std::fs::write(repo.join("identity.txt"), "BASE_ONLY\n").unwrap();
    std::fs::write(repo.join("pkg/inner.txt"), "BASE_ONLY inner\n").unwrap();
    let head = git_repo_with_commit(&repo);
    // The head diverges and carries an untracked decoy under `tmp/`.
    std::fs::write(repo.join("identity.txt"), "HEAD_ONLY\n").unwrap();
    std::fs::write(repo.join("pkg/inner.txt"), "HEAD_ONLY inner\n").unwrap();
    git(&repo, &["add", "-A"]);
    std::fs::create_dir_all(repo.join("tmp")).unwrap();
    std::fs::write(repo.join("tmp/decoy.txt"), "BASE_ONLY HEAD_ONLY decoy\n").unwrap();
    let head_index = std::fs::read(repo.join(".git/index")).unwrap();

    // About git context, not sandboxing: no launcher, so it also passes inside a sandboxed floor
    // (a nested sandbox is refused there).
    let sandbox = crate::validator::WorkerSandbox {
        wrapper: Vec::new(),
        level: crate::validator::SandboxLevel::BestEffort,
        downgrade_reason: Some("test".into()),
    };
    let scratch_dir = CheckScratch::prepare(&repo).unwrap();
    let root = crate::repo_checks::HeldDir::open(&repo).unwrap();
    let base = BaseTree::export(&repo, &root, &repo.join(".git"), &head).unwrap();
    // The check's own fixture repository (`git init <dir>`, `git -C <dir>`) is ITS repository,
    // not the base's: nothing pins the base's git dir for descendants (codex on #1127).
    let probe_script = "git rev-parse --show-toplevel && git rev-parse HEAD && \
         git rev-parse --absolute-git-dir && git grep -l BASE_ONLY -- . && \
         ! git grep -q HEAD_ONLY -- . && ! git grep -q decoy -- . && \
         fx=\"$TMPDIR/fixture-$$\" && rm -rf \"$fx\" && mkdir -p \"$fx\" && git init -q \"$fx\" && \
         test -d \"$fx/.git\" && [ \"$(git -C \"$fx\" rev-parse --absolute-git-dir)\" = \"$(cd \"$fx/.git\" && pwd -P)\" ]";
    for dir in ["", "pkg"] {
        let check = RepoCheck {
            name: "test".into(),
            argv: ["sh", "-c", probe_script].map(String::from).to_vec(),
            source: "fixture".into(),
            timeout_s: None,
            dir: dir.into(),
        };
        let r = run_held(
            &base.dir,
            Some(&base.held),
            &check,
            &sandbox,
            &scratch_dir,
            crate::repo_checks::Tree::Base,
        );
        assert!(r.passed(), "dir {dir:?}: {r:?}");
        let out: Vec<&str> = r.stdout_tail.lines().collect();
        assert_eq!(
            std::fs::canonicalize(out[0]).unwrap(),
            std::fs::canonicalize(&base.dir).unwrap(),
            "the toplevel is the base export: {out:?}"
        );
        assert_eq!(out[1], head, "HEAD is the base commit: {out:?}");
        assert_eq!(
            std::fs::canonicalize(out[2]).unwrap(),
            std::fs::canonicalize(base.dir.join(".git")).unwrap(),
            "{out:?}"
        );
    }
    // The head's index was never read or rewritten.
    assert_eq!(std::fs::read(repo.join(".git/index")).unwrap(), head_index);
    let _ = remove_base_export(&root);
    drop(scratch_dir);
    let _ = std::fs::remove_dir_all(&repo);
}

/// A SHA-256 repository exports too: the private repository hashes like its source.
#[cfg(unix)]
#[test]
fn a_sha256_repository_exports_with_its_line_endings() {
    let _load = seam::pin(0.5, 4);
    if !git_has_attr_source() {
        eprintln!("SKIP: this git has no --attr-source (< 2.40); the export writes raw blobs");
        return;
    }
    let repo = scratch("export-sha256");
    std::fs::write(repo.join(".gitattributes"), "*.cmd text eol=crlf\n").unwrap();
    std::fs::write(repo.join("run.cmd"), "echo ok\n").unwrap();
    // spawn-audit: hardened — git init on the test's own scratch repository.
    let mut init_cmd = std::process::Command::new("git");
    init_cmd
        .hardened()
        .args(["init", "-q", "--object-format=sha256"])
        .current_dir(&repo);
    let init = init_cmd.output();
    if !init.is_ok_and(|o| o.status.success())
        || git(&repo, &["rev-parse", "--show-object-format"]) != "sha256"
    {
        let _ = std::fs::remove_dir_all(&repo);
        return; // this git cannot make a SHA-256 repository
    }
    git(&repo, &["config", "user.email", "t@example.invalid"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let _scratch = CheckScratch::prepare(&repo).unwrap();
    let root = crate::repo_checks::HeldDir::open(&repo).unwrap();
    let base = BaseTree::export(&repo, &root, &repo.join(".git"), &head).unwrap();
    assert_eq!(
        std::fs::read(base.dir.join("run.cmd")).unwrap(),
        b"echo ok\r\n"
    );
    let _ = remove_base_export(&root);
    let _ = std::fs::remove_dir_all(&repo);
}

// ─── J4 xreview on #1132: what a journal may never hand a later floor ──────────────────────────

/// A journal of `ord` on `tree` whose interrupted floor had finished `entries` (check name and
/// its decision's extra log refs), opened by the next floor of the same key.
#[cfg(unix)]
fn journal_offering(tag: &str, entries: &[(&str, Vec<crate::floor_log::LogRef>)]) -> OpenJournal {
    let logs = scratch(tag).join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let ctx = FloorContext::default();
    let check = |name: &str| RepoCheck {
        name: name.to_string(),
        argv: vec!["sh".into(), "-c".into(), "true".into()],
        source: "test".into(),
        timeout_s: None,
        dir: String::new(),
    };
    let detected: Vec<RepoCheck> = entries.iter().map(|(n, _)| check(n)).collect();
    let tree = "1111111111111111111111111111111111111111";
    let key = FloorKey::of(tree, &ctx, &detected);
    let checks = entries
        .iter()
        .map(|(name, refs)| {
            let mut run: CheckRun = serde_json::from_value(serde_json::json!({
                "name": name, "argv": ["sh", "-c", "true"], "source": "test",
                "exit_code": 0, "timed_out": false, "duration_ms": 1,
                "stdout_tail": "", "stderr_tail": "",
                "decision": { "outcome": OUTCOME_PASS },
            }))
            .unwrap();
            run.decision.as_mut().unwrap().log_refs = refs.clone();
            JournalEntry {
                dir: String::new(),
                name: name.to_string(),
                argv: run.argv.clone(),
                run,
            }
        })
        .collect();
    let rec = JournalRecord {
        floor_policy: FLOOR_POLICY_VERSION,
        key: key.clone(),
        exclude: true,
        attempt: 4,
        checks,
    };
    std::fs::write(journal_path(&logs, 2), serde_json::to_string(&rec).unwrap()).unwrap();
    let j = FloorJournal {
        ord: 2,
        attempt: 5,
        tree: tree.to_string(),
        phase: None,
    };
    let open = OpenJournal::open(&logs, &j, key, true);
    assert!(open.resume.as_ref().is_some_and(|r| r.restarted.is_none()));
    open
}

/// (xreview P2) `install` finished before the restart, but a guarded floor's fresh checkout has
/// none of its output (`node_modules`): it runs again. An independent check is still reused.
#[cfg(unix)]
#[test]
fn an_interrupted_floors_install_is_never_reused() {
    let mut j = journal_offering("journal-install", &[("install", vec![]), ("lint", vec![])]);
    let logs = j.path.parent().unwrap().to_path_buf();
    let check = |name: &str| RepoCheck {
        name: name.to_string(),
        argv: vec!["sh".into(), "-c".into(), "true".into()],
        source: "test".into(),
        timeout_s: None,
        dir: String::new(),
    };
    assert!(
        j.reuse(&check("install"), &logs).is_none(),
        "install runs again"
    );
    assert!(j.reuse(&check("lint"), &logs).is_some(), "lint is reused");
    assert_eq!(j.resume.as_ref().unwrap().reused, vec!["lint".to_string()]);
}

/// (xreview P3) A decision whose own evidence (a base inventory, a failing test's confirmation —
/// referenced only from `decision.log_refs`) is gone from the store is not reused.
#[cfg(unix)]
#[test]
fn a_journaled_decision_whose_own_log_is_gone_runs_again() {
    let gone = crate::floor_log::LogRef {
        attempt: "toc-base".into(),
        stream: "stdout".into(),
        sha256: "ab".repeat(32),
        bytes: 7,
        truncated: false,
    };
    let mut j = journal_offering("journal-gone-log", &[("lint", vec![gone])]);
    let logs = j.path.parent().unwrap().to_path_buf();
    let lint = RepoCheck {
        name: "lint".into(),
        argv: vec!["sh".into(), "-c".into(), "true".into()],
        source: "test".into(),
        timeout_s: None,
        dir: String::new(),
    };
    assert!(
        j.reuse(&lint, &logs).is_none(),
        "the missing blob voids the reuse"
    );
}

/// (xreview P1) A check that passed after it changed the guarded tree's tracked content is never
/// journaled, so a fresh checkout of the original tree (the next floor after a restart) cannot
/// reuse a pass that ran on other content. A check that leaves the tree alone is journaled.
#[cfg(unix)]
#[test]
fn a_check_that_changed_the_guarded_tree_is_never_journaled() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    for (tag, lint) in [
        ("journal-intact", "true"),
        ("journal-mutated", "echo v2 > app.txt"),
    ] {
        let wt = scratch(tag);
        std::fs::write(wt.join("app.txt"), "v1\n").unwrap();
        std::fs::write(wt.join(".gitignore"), "tmp/\ncaptured.json\njournal-path\n").unwrap();
        std::fs::create_dir_all(wt.join(".wicked")).unwrap();
        // `test` runs second and copies the journal as it stands mid-floor.
        std::fs::write(
            wt.join(CONFIG_PATH),
            serde_json::json!({
                "typecheck": false,
                "lint": ["sh", "-c", lint],
                "test": ["sh", "-c",
                    "cp \"$(cat journal-path)\" captured.json 2>/dev/null || echo none > captured.json"],
            })
            .to_string(),
        )
        .unwrap();
        git_repo_with_commit(&wt);
        let git_dir = PathBuf::from(git(&wt, &["rev-parse", "--absolute-git-dir"]));
        let snap = crate::worktree_guard::snapshot_through(&wt, &git_dir).expect("snapshot");
        let checkout = checkout_guarded_tree(&wt, &git_dir, &snap.tree, tag).expect("checkout");
        let ctx = FloorContext {
            stage: FloorStage::Verify,
            run_id: Some(format!("{tag}-{}", std::process::id())),
            ..FloorContext::default()
        };
        let logs = CheckScratch::prepare_for(&checkout.dir, ctx.run_id.as_deref())
            .unwrap()
            .logs
            .clone();
        std::fs::write(
            checkout.dir.join("journal-path"),
            journal_path(&logs, 3).to_string_lossy().as_bytes(),
        )
        .unwrap();
        let j = FloorJournal {
            ord: 3,
            attempt: 1,
            tree: snap.tree.clone(),
            phase: None,
        };
        let r = checkout.run_floor_journaled(&ctx, None, &j);
        if r.sandbox_error.is_some() || r.checks.is_empty() {
            eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
            return;
        }
        let captured =
            std::fs::read_to_string(checkout.dir.join("captured.json")).unwrap_or_default();
        if lint == "true" {
            assert!(r.passed, "{r:#?}");
            assert!(
                captured.contains("\"name\":\"lint\""),
                "an untouched pass is journaled (the capture works): {captured}"
            );
        } else {
            assert!(!r.passed, "hold_to refuses the changed tree: {r:#?}");
            assert!(
                !captured.contains("\"name\":\"lint\""),
                "a pass on changed content is never journaled: {captured}"
            );
        }
        assert!(
            !journal_path(&logs, 3).exists(),
            "the journal goes with the floor"
        );
        drop(checkout);
        let _ = std::fs::remove_dir_all(&wt);
    }
}

/// (core#1144) A repo whose checks are `lint` + `test` (and optionally a verify-only `e2e`) in a
/// fresh git worktree; every check prints a fresh nonce, so a reused result carries the creator's.
#[cfg(unix)]
struct Core1144 {
    wt: PathBuf,
    git_dir: PathBuf,
    tree: String,
    base: String,
    run_id: String,
}

#[cfg(unix)]
impl Core1144 {
    fn new(tag: &str, checks: serde_json::Value) -> Self {
        Self::with(tag, |wt| {
            std::fs::write(wt.join(CONFIG_PATH), checks.to_string()).unwrap();
        })
    }

    /// (core#1152) A repo whose checks come from a build guide committed in the base: `checks` in
    /// order, each `{"name", "run", "stage"?, "cwd"?}` (no `version` piece: nothing to go stale).
    fn guided(tag: &str, checks: &[serde_json::Value]) -> Self {
        Self::with(tag, |wt| {
            for c in checks {
                if let Some(cwd) = c["cwd"].as_str() {
                    std::fs::create_dir_all(wt.join(cwd)).unwrap();
                    std::fs::write(wt.join(cwd).join("keep.txt"), "k\n").unwrap();
                }
            }
            std::fs::write(
                wt.join(crate::floor_guide::GUIDE_PATH),
                serde_json::to_string_pretty(
                    &serde_json::json!({"version": 1, "derivedFrom": [], "checks": checks}),
                )
                .unwrap(),
            )
            .unwrap();
        })
    }

    fn with(tag: &str, files: impl FnOnce(&Path)) -> Self {
        let wt = scratch(tag);
        std::fs::write(wt.join("app.txt"), "v1\n").unwrap();
        std::fs::write(wt.join(".gitignore"), "tmp/\nout/\n").unwrap();
        std::fs::create_dir_all(wt.join(".wicked")).unwrap();
        files(&wt);
        let base = git_repo_with_commit(&wt);
        let git_dir = PathBuf::from(git(&wt, &["rev-parse", "--absolute-git-dir"]));
        let tree = crate::worktree_guard::snapshot_through(&wt, &git_dir)
            .expect("snapshot")
            .tree;
        let run_id = format!("{tag}-{}", std::process::id());
        Core1144 {
            wt,
            git_dir,
            tree,
            base,
            run_id,
        }
    }

    const NONCE: [&'static str; 3] = [
        "sh",
        "-c",
        "echo nonce-$$-$(od -An -N8 -tx8 /dev/urandom | tr -d ' ')",
    ];

    fn ctx(&self, stage: FloorStage, base: &str) -> FloorContext {
        FloorContext {
            stage,
            base_head: Some(base.to_string()),
            git_dir: Some(self.git_dir.clone()),
            run_id: Some(self.run_id.clone()),
            ..FloorContext::default()
        }
    }

    fn journal(&self, ord: u32, phase: &str) -> FloorJournal {
        FloorJournal {
            ord,
            attempt: 1,
            tree: self.tree.clone(),
            phase: Some(phase.to_string()),
        }
    }

    /// A floor of `stage` on a clean checkout of the tree.
    fn guarded(&self, stage: FloorStage, base: &str, ord: u32, phase: &str) -> RepoChecksReport {
        // The run id keeps parallel tests' checkouts apart (the checkout nonce is a clock read).
        let label = format!("{}-{ord}-{phase}", self.run_id);
        let checkout = checkout_guarded_tree(&self.wt, &self.git_dir, &self.tree, &label).unwrap();
        checkout.run_floor_journaled(&self.ctx(stage, base), None, &self.journal(ord, phase))
    }

    fn logs(&self) -> PathBuf {
        CheckScratch::prepare_for(&self.wt, Some(&self.run_id))
            .unwrap()
            .logs
            .clone()
    }

    fn record_exists(&self) -> bool {
        creator_record_path(&self.logs(), &self.tree, &self.base).exists()
    }
}

#[cfg(unix)]
fn stdout_of(r: &RepoChecksReport, name: &str) -> String {
    r.checks
        .iter()
        .find(|c| c.name == name)
        .map(|c| c.stdout_tail.clone())
        .unwrap_or_else(|| panic!("`{name}` ran: {r:#?}"))
}

/// (core#1144) The verify floor reuses the checks the guarded creator floor already ran on the
/// same tree against the same base, and says so ("checks reused from fix"); another base reuses
/// nothing; a later creator floor on the live worktree removes the record it cannot replace.
#[cfg(unix)]
#[test]
fn core_1144_verify_reuses_the_guarded_creator_floor_of_the_same_tree_and_base() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let r = Core1144::new(
        "core-1144-reuse",
        serde_json::json!({"typecheck": false, "lint": Core1144::NONCE, "test": Core1144::NONCE}),
    );
    let creator = r.guarded(FloorStage::Creator, &r.base, 4, "fix");
    if creator.sandbox_error.is_some() || creator.checks.is_empty() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return;
    }
    assert!(creator.passed, "{creator:#?}");
    assert!(r.record_exists(), "the creator floor left its record");

    let verify = r.guarded(FloorStage::Verify, &r.base, 6, "verify");
    assert!(verify.passed, "{verify:#?}");
    assert_eq!(
        verify.reused_from,
        Some(FloorReuse {
            phase: Some("fix".into()),
            ord: 4,
            attempt: 1,
            reused: vec!["lint".into(), "test".into()],
            fresh: Vec::new(),
            voided: None,
        })
    );
    for name in ["lint", "test"] {
        assert_eq!(
            stdout_of(&verify, name),
            stdout_of(&creator, name),
            "`{name}` reused"
        );
    }
    let note = verify.classification_note().unwrap_or_default();
    assert!(
        note.contains("checks reused from fix (unit 4, attempt 1): lint, test ran there"),
        "the gate's floor note says where the checks came from: {note}"
    );
    // The floor log's own line for a reused check.
    let record: CreatorFloorRecord = serde_json::from_str(
        &std::fs::read_to_string(creator_record_path(&r.logs(), &r.tree, &r.base)).unwrap(),
    )
    .unwrap();
    let seed = CreatorSeed {
        record,
        reused: Vec::new(),
        fresh: Vec::new(),
    };
    let test_run = verify.checks.iter().find(|c| c.name == "test").unwrap();
    assert_eq!(
        seed.log_line(test_run),
        "wicked-core: repo checks floor — `test` passed on this same tree and base at fix (unit \
         4, attempt 1); checks reused from that floor, not run again"
    );

    // Another base (a real commit of the same tree): nothing is reused, every check runs fresh.
    let other = git(&r.wt, &["commit-tree", "HEAD^{tree}", "-m", "another base"]);
    assert_ne!(other, r.base);
    let other_base = r.guarded(FloorStage::Verify, &other, 6, "verify");
    assert!(other_base.reused_from.is_none(), "{other_base:#?}");
    assert_ne!(stdout_of(&other_base, "test"), stdout_of(&creator, "test"));

    // (xreview r1) A later creator floor of the same tree and base on the LIVE worktree cannot
    // stand for the tree: it removes the record, and verify runs everything.
    let _ = run_floor_journaled(
        &r.wt,
        &r.ctx(FloorStage::Creator, &r.base),
        None,
        Some(&r.journal(4, "fix")),
    );
    assert!(
        !r.record_exists(),
        "the live creator floor removed the stale record"
    );
    let after_live = r.guarded(FloorStage::Verify, &r.base, 6, "verify");
    assert!(after_live.reused_from.is_none(), "{after_live:#?}");
    let _ = std::fs::remove_dir_all(&r.wt);
}

/// (core#1152) Reuse is in any order: a verify-only check that runs LAST (`e2e`) runs fresh, and the
/// checks before it are still reused. The note and log name both lists, and the
/// `repoChecksEvaluated` frame carries them as `reusedFrom` (crew#1180).
#[cfg(unix)]
#[test]
fn core_1152_a_verify_only_check_ordered_last_runs_fresh_and_the_rest_is_reused() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let n = Core1144::NONCE;
    let r = Core1144::new(
        "core-1152-last",
        serde_json::json!({"typecheck": false, "lint": n, "test": n, "e2e": n}),
    );
    let creator = r.guarded(FloorStage::Creator, &r.base, 4, "fix");
    if creator.sandbox_error.is_some() || creator.checks.is_empty() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return;
    }
    assert!(creator.passed && r.record_exists(), "{creator:#?}");
    assert!(
        creator.checks.iter().all(|c| c.name != "e2e"),
        "e2e is verify-only"
    );
    let verify = r.guarded(FloorStage::Verify, &r.base, 6, "verify");
    assert!(verify.passed, "{verify:#?}");
    let reuse = FloorReuse {
        phase: Some("fix".into()),
        ord: 4,
        attempt: 1,
        reused: vec!["lint".into(), "test".into()],
        fresh: vec!["e2e".into()],
        voided: None,
    };
    assert_eq!(verify.reused_from.as_ref(), Some(&reuse), "{verify:#?}");
    for name in ["lint", "test"] {
        assert_eq!(stdout_of(&verify, name), stdout_of(&creator, name));
    }
    assert!(stdout_of(&verify, "e2e").contains("nonce-"), "e2e ran here");
    let note = verify.classification_note().unwrap_or_default();
    assert!(
        note.contains(
            "checks reused from fix (unit 4, attempt 1): lint, test ran there on this same tree"
        ) && note.contains("not run again here; ran fresh here: e2e"),
        "the note names the reused and the fresh checks: {note}"
    );
    // (crew#1180) The event stream carries it.
    let frame = |reused_from: Option<FloorReuse>| {
        crate::event::CoreEvent::RepoChecksEvaluated {
            session: "s".into(),
            ord: 6,
            attempt: 0,
            passed: true,
            criterion: CRITERION.into(),
            checks: verify.checks.clone(),
            skipped: Vec::new(),
            sandbox_level: verify.sandbox_level.clone(),
            sandbox_error: None,
            detect_error: None,
            outcome: verify.outcome().into(),
            floor: "verify".into(),
            claim: None,
            env: None,
            changed: Vec::new(),
            changed_truncated: false,
            reused_from,
        }
        .to_json()
    };
    assert_eq!(
        frame(verify.reused_from.clone())["reusedFrom"],
        serde_json::json!({
            "phase": "fix", "ord": 4, "attempt": 1,
            "reused": ["lint", "test"], "fresh": ["e2e"],
        })
    );
    assert!(frame(None).get("reusedFrom").is_none());
    let _ = std::fs::remove_dir_all(&r.wt);
}

/// (core#1152, the reason core#1145 was suffix-only) A fresh check that needs what a reused check
/// leaves in the checkout (`lint` writes an ignored `out/marker` that `e2e` reads) fails after the
/// reuse; the floor then runs every check again from the first one with nothing reused, and passes
/// as it did before reuse existed — saying so.
#[cfg(unix)]
#[test]
fn core_1152_a_fresh_check_failing_after_reuse_runs_the_floor_again_without_reuse() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    // Headroom: no identical retry, so the e2e run count below is exact (core#1191).
    let _load = seam::pin(0.5, 4);
    let r = Core1144::new(
        "core-1152-again",
        serde_json::json!({
            "typecheck": false,
            "lint": ["sh", "-c", "mkdir -p out && echo m > out/marker && echo nonce-$$-$(od -An -N8 -tx8 /dev/urandom | tr -d ' ')"],
            "test": Core1144::NONCE,
            // A unique line per run, so the run log store counts the e2e runs (core#1191).
            "e2e": ["sh", "-c", "echo e2e-run-$$-$(od -An -N8 -tx8 /dev/urandom | tr -d ' '); test -f out/marker"],
        }),
    );
    let creator = r.guarded(FloorStage::Creator, &r.base, 4, "fix");
    if creator.sandbox_error.is_some() || creator.checks.is_empty() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return;
    }
    assert!(creator.passed && r.record_exists(), "{creator:#?}");
    let verify = r.guarded(FloorStage::Verify, &r.base, 6, "verify");
    assert!(
        verify.passed,
        "the floor ran again in order and passed: {verify:#?}"
    );
    for name in ["lint", "test"] {
        assert_ne!(
            stdout_of(&verify, name),
            stdout_of(&creator, name),
            "`{name}` ran again here"
        );
    }
    let reuse = verify
        .reused_from
        .as_ref()
        .expect("the set-aside reuse is disclosed");
    assert_eq!(reuse.reused, vec!["lint".to_string(), "test".to_string()]);
    let why = reuse.voided.as_deref().unwrap_or_default();
    assert!(
        why.contains("`e2e` ran fresh after the reused lint, test and failed")
            && why.contains("every check ran again here from the first one, nothing reused"),
        "{why}"
    );
    let note = verify.classification_note().unwrap_or_default();
    assert!(
        note.contains("checks reused from fix (unit 4, attempt 1) were set aside: `e2e`"),
        "{note}"
    );
    // (core#1191) The set-aside pass did not classify the failure first: `e2e` ran once there
    // (no base run, no retry) and once in the pass without reuse, where it passed.
    let e2e_runs = std::fs::read_dir(r.logs())
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| {
                    std::fs::read(e.path())
                        .is_ok_and(|b| String::from_utf8_lossy(&b).starts_with("e2e-run-"))
                })
                .count()
        })
        .unwrap_or(0);
    assert_eq!(e2e_runs, 2, "e2e ran on the first pass, then on the second");
    let _ = std::fs::remove_dir_all(&r.wt);
}

/// (core#1191, xreview on #1192) A fresh check after reused ones that fails once under load and
/// passes its identical retry is ACCEPTED (`passed on retry (flaky)`), as before: the set-aside is
/// decided early only when nothing left could accept the failure, so the reuse stands.
#[cfg(unix)]
#[test]
fn core_1191_a_fresh_check_that_passes_its_retry_after_reuse_keeps_the_reuse() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let r = Core1144::new(
        "core-1191-flaky",
        serde_json::json!({
            "typecheck": false,
            "lint": Core1144::NONCE,
            "test": Core1144::NONCE,
            // Red the first time in this floor, green after (the check's private TMPDIR persists
            // across one floor's runs).
            "e2e": ["sh", "-c", "if [ -f \"$TMPDIR/e2e-tried\" ]; then exit 0; fi; touch \"$TMPDIR/e2e-tried\"; exit 1"],
        }),
    );
    let creator = r.guarded(FloorStage::Creator, &r.base, 4, "fix");
    if creator.sandbox_error.is_some() || creator.checks.is_empty() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return;
    }
    let verify = {
        let _load = seam::pin(10.0, 4);
        r.guarded(FloorStage::Verify, &r.base, 6, "verify")
    };
    let reuse = verify.reused_from.as_ref().expect("the reuse is disclosed");
    assert_eq!(reuse.voided, None, "the reuse stands: {reuse:?}");
    assert_eq!(reuse.reused, vec!["lint".to_string(), "test".to_string()]);
    let e2e = verify
        .checks
        .iter()
        .find(|c| c.name == "e2e")
        .expect("e2e ran");
    let d = e2e.decision.as_ref().expect("decided");
    assert!(
        verify.passed && d.outcome == OUTCOME_PASS && d.flaky,
        "{verify:#?}"
    );
    let _ = std::fs::remove_dir_all(&r.wt);
}

/// (core#1152, xreview) The no-reuse re-run runs the checks the first pass read, never ones read
/// again from a checkout repository commands have run in: a failing test that leaves an untracked
/// `.wicked/checks.json` turning every check off must not leave a re-run with no checks (green).
#[cfg(unix)]
#[test]
fn core_1152_the_rerun_never_re_reads_the_checks_from_the_checkout() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let r = Core1144::with("core-1152-frozen", |wt| {
        std::fs::remove_dir_all(wt.join(".wicked")).unwrap();
        std::fs::write(wt.join(".gitignore"), "tmp/\ntarget/\nCargo.lock\n").unwrap();
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(
            wt.join("Cargo.toml"),
            "[package]\nname = \"core1152_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(
            wt.join("src/lib.rs"),
            "#[test]\nfn poisons_and_fails() {\n    std::fs::create_dir_all(\".wicked\").unwrap();\n    \
             std::fs::write(\".wicked/checks.json\", r#\"{\"formatter\":false,\"test\":false}\"#).unwrap();\n    \
             panic!(\"red\");\n}\n",
        )
        .unwrap();
    });
    let first = r.guarded(FloorStage::Creator, &r.base, 4, "fix");
    if first.sandbox_error.is_some() || first.checks.is_empty() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return;
    }
    let names = |rep: &RepoChecksReport| -> Vec<String> {
        rep.checks.iter().map(|c| c.name.clone()).collect()
    };
    assert_eq!(
        names(&first),
        ["cargo-fmt-check", "cargo-test"],
        "{first:#?}"
    );
    // The creator floor resumes `cargo-test` from an interrupted journal, so its record holds only
    // `cargo-fmt-check` and verify runs `cargo-test` fresh after a reused check.
    let ctx = r.ctx(FloorStage::Creator, &r.base);
    let test_run = first
        .checks
        .iter()
        .find(|c| c.name == "cargo-test")
        .unwrap();
    let check = first
        .detected
        .iter()
        .find(|c| c.name == "cargo-test")
        .unwrap();
    let record = JournalRecord {
        floor_policy: FLOOR_POLICY_VERSION,
        key: FloorKey::of(&r.tree, &ctx, &first.detected),
        exclude: exclusion_enabled(),
        attempt: 1,
        checks: vec![JournalEntry {
            dir: check.dir.clone(),
            name: check.name.clone(),
            argv: check.argv.clone(),
            run: test_run.clone(),
        }],
    };
    std::fs::write(
        journal_path(&r.logs(), 4),
        serde_json::to_string(&record).unwrap(),
    )
    .unwrap();
    let resumed = r.guarded(FloorStage::Creator, &r.base, 4, "fix");
    assert_eq!(
        resumed.resumed.as_ref().map(|x| x.reused.clone()),
        Some(vec!["cargo-test".to_string()]),
        "{resumed:#?}"
    );
    assert!(r.record_exists(), "{resumed:#?}");
    let verify = r.guarded(FloorStage::Verify, &r.base, 6, "verify");
    assert!(
        !verify.passed,
        "the re-run ran the failing test again: {verify:#?}"
    );
    assert_eq!(
        names(&verify),
        ["cargo-fmt-check", "cargo-test"],
        "{verify:#?}"
    );
    let why = verify
        .reused_from
        .as_ref()
        .and_then(|r| r.voided.clone())
        .unwrap_or_default();
    assert!(
        why.contains("`cargo-test` ran fresh after the reused cargo-fmt-check and failed"),
        "{verify:#?}"
    );
    let _ = std::fs::remove_dir_all(&r.wt);
}

/// (core#1152, xreview) A failure that the pass a daemon restart interrupted ran after reused checks
/// (restored from the verify floor's journal) sends the floor round again, as it would have there.
#[cfg(unix)]
#[test]
fn core_1152_a_journal_restored_failure_after_reuse_runs_the_floor_again() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let e2e = serde_json::json!(["sh", "-c", "test -f out/marker"]);
    let r = Core1144::new(
        "core-1152-journal",
        serde_json::json!({
            "typecheck": false,
            "lint": ["sh", "-c", "mkdir -p out && echo m > out/marker && echo nonce-$$-$(od -An -N8 -tx8 /dev/urandom | tr -d ' ')"],
            "test": Core1144::NONCE,
            "e2e": e2e,
        }),
    );
    let creator = r.guarded(FloorStage::Creator, &r.base, 4, "fix");
    if creator.sandbox_error.is_some() || creator.checks.is_empty() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return;
    }
    assert!(creator.passed && r.record_exists(), "{creator:#?}");
    // A failing `e2e` run in this run's log store: the same check on a repo whose `lint` leaves no
    // marker (another tree, the same run id).
    let mut other = Core1144::new(
        "core-1152-journal-red",
        serde_json::json!({"typecheck": false, "lint": Core1144::NONCE, "e2e": e2e}),
    );
    other.run_id = r.run_id.clone();
    let red = other.guarded(FloorStage::Verify, &other.base, 6, "verify");
    let red_e2e = red
        .checks
        .iter()
        .find(|c| c.name == "e2e")
        .expect("{red:#?}");
    assert!(red_e2e.denies(), "{red:#?}");
    // The interrupted verify pass had journaled that failure after reusing `lint` and `test`.
    let ctx = r.ctx(FloorStage::Verify, &r.base);
    let detected = detect_with(&r.wt, &ctx).unwrap();
    let check = detected.iter().find(|c| c.name == "e2e").unwrap();
    let record = JournalRecord {
        floor_policy: FLOOR_POLICY_VERSION,
        key: FloorKey::of(&r.tree, &ctx, &detected),
        exclude: exclusion_enabled(),
        attempt: 1,
        checks: vec![JournalEntry {
            dir: check.dir.clone(),
            name: check.name.clone(),
            argv: check.argv.clone(),
            run: red_e2e.clone(),
        }],
    };
    std::fs::write(
        journal_path(&r.logs(), 6),
        serde_json::to_string(&record).unwrap(),
    )
    .unwrap();
    let verify = r.guarded(FloorStage::Verify, &r.base, 6, "verify");
    assert!(
        verify.passed,
        "the floor ran again in order and passed: {verify:#?}"
    );
    let why = verify
        .reused_from
        .as_ref()
        .and_then(|x| x.voided.clone())
        .unwrap_or_default();
    assert!(
        why.contains("`e2e` ran fresh after the reused lint, test and failed"),
        "{verify:#?}"
    );
    let _ = std::fs::remove_dir_all(&r.wt);
    let _ = std::fs::remove_dir_all(&other.wt);
}

/// (core#1152) The direct guard that replaces the suffix rule: a fresh check that changes the
/// checkout's tracked content voids the reused results, even when a later fresh check restores it
/// (the final comparison alone would pass), and the floor fails.
#[cfg(unix)]
#[test]
fn core_1152_a_fresh_check_that_changes_the_tree_voids_the_reuse() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let n = Core1144::NONCE;
    let verify_only = |name: &str, run: serde_json::Value| serde_json::json!({"name": name, "cwd": ".", "run": run, "stage": ["verify"]});
    let r = Core1144::guided(
        "core-1152-mutate",
        &[
            serde_json::json!({"name": "lint", "cwd": ".", "run": n}),
            verify_only(
                "prepare",
                serde_json::json!(["sh", "-c", "echo v2 > app.txt"]),
            ),
            serde_json::json!({"name": "test", "cwd": ".", "run": n}),
            verify_only(
                "restore",
                serde_json::json!(["sh", "-c", "echo v1 > app.txt"]),
            ),
        ],
    );
    let creator = r.guarded(FloorStage::Creator, &r.base, 4, "fix");
    if creator.sandbox_error.is_some() || creator.checks.is_empty() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return;
    }
    let names: Vec<&str> = creator.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["lint", "test"], "{creator:#?}");
    assert!(creator.passed && r.record_exists(), "{creator:#?}");
    let verify = r.guarded(FloorStage::Verify, &r.base, 6, "verify");
    assert_eq!(
        std::fs::read_to_string(r.wt.join("app.txt")).unwrap(),
        "v1\n",
        "the seat's tree is never the one the checks ran in"
    );
    assert!(!verify.passed, "{verify:#?}");
    let reuse = verify.reused_from.as_ref().expect("{verify:#?}");
    assert_eq!(reuse.reused, vec!["lint".to_string(), "test".to_string()]);
    assert_eq!(
        reuse.fresh,
        vec!["prepare".to_string(), "restore".to_string()]
    );
    let why = reuse.voided.as_deref().unwrap_or_default();
    assert!(
        why.contains(
            "a check that ran fresh here (prepare, restore) changed the checkout's tracked content"
        ) && why.contains("void, and the floor fails"),
        "{why}"
    );
    assert!(
        verify
            .detect_error
            .as_deref()
            .unwrap_or_default()
            .contains(why),
        "the failure says why: {verify:#?}"
    );
    let _ = std::fs::remove_dir_all(&r.wt);
}

/// (core#1152) wicked-core's own check order (its `.wicked/build-guide.json`): the verify-only
/// builds sit before `test` and the core-ts checks after it. Verify reuses every shared check,
/// `test` included, and runs only the verify-only ones; a reused failing `test` (the base fails it
/// too, as on rig run c5320434) stops the floor there, as a fresh one does.
#[cfg(unix)]
#[test]
fn core_1152_cores_real_check_order_reuses_every_shared_check() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let guide = |test: serde_json::Value| -> Vec<serde_json::Value> {
        let n = serde_json::json!(Core1144::NONCE);
        let both = |name: &str, run: &serde_json::Value| serde_json::json!({"name": name, "cwd": ".", "run": run});
        let verify = |name: &str, cwd: &str| serde_json::json!({"name": name, "cwd": cwd, "run": Core1144::NONCE, "stage": ["verify"]});
        let ts = "crates/wicked-core-ts";
        vec![
            both("cargo-fmt-check", &n),
            both("typecheck", &n),
            both("lint", &n),
            verify("build-workspace", "."),
            verify("build-postgres", "."),
            serde_json::json!({"name": "test", "cwd": ".", "run": test, "stage": ["creator", "verify"]}),
            verify("core-ts-fmt", ts),
            verify("core-ts-clippy", ts),
            verify("core-ts-test", ts),
            verify("core-ts-finalize-dts", ts),
            verify("install", ts),
            verify("core-ts-tsc", ts),
        ]
    };
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();

    let r = Core1144::guided("core-1152-core", &guide(serde_json::json!(Core1144::NONCE)));
    let creator = r.guarded(FloorStage::Creator, &r.base, 4, "fix");
    if creator.sandbox_error.is_some() || creator.checks.is_empty() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return;
    }
    assert!(creator.passed && r.record_exists(), "{creator:#?}");
    let verify = r.guarded(FloorStage::Verify, &r.base, 6, "verify");
    assert!(verify.passed, "{verify:#?}");
    let reuse = verify.reused_from.as_ref().expect("{verify:#?}");
    assert_eq!(
        reuse.reused,
        s(&["cargo-fmt-check", "typecheck", "lint", "test"])
    );
    assert_eq!(
        reuse.fresh,
        s(&[
            "build-workspace",
            "build-postgres",
            "core-ts-fmt",
            "core-ts-clippy",
            "core-ts-test",
            "core-ts-finalize-dts",
            "install",
            "core-ts-tsc",
        ])
    );
    assert_eq!(reuse.voided, None);
    assert_eq!(stdout_of(&verify, "test"), stdout_of(&creator, "test"));
    let _ = std::fs::remove_dir_all(&r.wt);

    // The J4 shape: `test` fails at fix on a red base; verify reuses that failure and stops there.
    let red = Core1144::guided(
        "core-1152-core-red",
        &guide(serde_json::json!(["sh", "-c", "echo FAILED-$$; exit 1"])),
    );
    let creator = red.guarded(FloorStage::Creator, &red.base, 4, "fix");
    assert!(!creator.passed && red.record_exists(), "{creator:#?}");
    let verify = red.guarded(FloorStage::Verify, &red.base, 6, "verify");
    assert!(!verify.passed, "{verify:#?}");
    let reuse = verify.reused_from.as_ref().expect("{verify:#?}");
    assert_eq!(
        reuse.reused,
        s(&["cargo-fmt-check", "typecheck", "lint", "test"])
    );
    assert_eq!(reuse.fresh, s(&["build-workspace", "build-postgres"]));
    assert_eq!(
        reuse.voided, None,
        "a reused failure is the fix floor's own word"
    );
    assert_eq!(stdout_of(&verify, "test"), stdout_of(&creator, "test"));
    assert_eq!(
        verify.skipped,
        s(&[
            "core-ts-fmt",
            "core-ts-clippy",
            "core-ts-test",
            "core-ts-finalize-dts",
            "install",
            "core-ts-tsc",
        ])
    );
    let _ = std::fs::remove_dir_all(&red.wt);
}

/// (core#1144, xreview r1) A creator floor whose check changed the checkout's tracked content
/// leaves no record, even when a later check restored it (the final probe alone would pass).
#[cfg(unix)]
#[test]
fn core_1144_a_floor_that_changed_its_checkout_leaves_no_record() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let r = Core1144::new(
        "core-1144-spoiled",
        serde_json::json!({
            "typecheck": false,
            "lint": ["sh", "-c", "echo v2 > app.txt"],
            "test": ["sh", "-c", "echo v1 > app.txt"],
        }),
    );
    let creator = r.guarded(FloorStage::Creator, &r.base, 4, "fix");
    if creator.sandbox_error.is_some() || creator.checks.is_empty() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return;
    }
    assert_eq!(creator.checks.len(), 2, "{creator:#?}");
    assert!(!r.record_exists(), "a spoiled floor publishes nothing");
    let _ = std::fs::remove_dir_all(&r.wt);
}

/// (core#1144, xreview r1) A check a creator floor took from an interrupted floor's journal (which
/// may have run in the live worktree) is never published for verify: only what this clean-checkout
/// floor ran itself is.
#[cfg(unix)]
#[test]
fn core_1144_a_journal_resumed_check_is_never_published() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let n = Core1144::NONCE;
    let r = Core1144::new(
        "core-1144-resumed",
        serde_json::json!({"typecheck": false, "lint": n, "test": n}),
    );
    let first = r.guarded(FloorStage::Creator, &r.base, 4, "fix");
    if first.sandbox_error.is_some() || first.checks.is_empty() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return;
    }
    // An interrupted floor's journal holding `lint` (as a live-worktree floor would leave it).
    let ctx = r.ctx(FloorStage::Creator, &r.base);
    let lint = first
        .checks
        .iter()
        .find(|c| c.name == "lint")
        .unwrap()
        .clone();
    let check = first.detected.iter().find(|c| c.name == "lint").unwrap();
    let record = JournalRecord {
        floor_policy: FLOOR_POLICY_VERSION,
        key: FloorKey::of(&r.tree, &ctx, &first.detected),
        exclude: exclusion_enabled(),
        attempt: 1,
        checks: vec![JournalEntry {
            dir: check.dir.clone(),
            name: check.name.clone(),
            argv: check.argv.clone(),
            run: lint.clone(),
        }],
    };
    std::fs::write(
        journal_path(&r.logs(), 4),
        serde_json::to_string(&record).unwrap(),
    )
    .unwrap();
    let resumed = r.guarded(FloorStage::Creator, &r.base, 4, "fix");
    assert_eq!(
        resumed.resumed.as_ref().map(|x| x.reused.clone()),
        Some(vec!["lint".to_string()]),
        "{resumed:#?}"
    );
    let verify = r.guarded(FloorStage::Verify, &r.base, 6, "verify");
    assert_eq!(
        verify.reused_from.as_ref().map(|x| x.reused.clone()),
        Some(vec!["test".to_string()]),
        "only the check the clean-checkout floor ran is reused: {verify:#?}"
    );
    assert_ne!(stdout_of(&verify, "lint"), stdout_of(&first, "lint"));
    let _ = std::fs::remove_dir_all(&r.wt);
}
