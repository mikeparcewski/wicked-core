//! (core#1198) The failing-set retry and base, through the real floor: a stand-in vitest whose
//! exit codes are each tree's truth. Every scenario runs twice, on twin repositories, once with the
//! `rerun` declaration and once without it, and must reach the SAME decision. Only the number of
//! test files the floor ran differs.

#![cfg(unix)]

use super::test_seam as seam;
use super::tests::*;
use super::*;

/// A stand-in `vitest` in the shape the floor reads: ` ✓ <file> (1 test)` per passing file,
/// ` FAIL  <file> > t` per failing one, the `Test Files` summary, exit 1 on any failure. `suite.txt`
/// holds `<file> <pass|fail|flake>` lines (`flake`: fails the first time this tree runs it in a floor,
/// passes after). File arguments run only those files (one the suite does not list is skipped:
/// `--passWithNoTests`); none runs the whole suite. `unhandled` present: an error outside any test on
/// a whole-suite run, exit 1. Every invocation appends the number of files it ran to `$counters/calls`.
fn fake_vitest(tag: &str) -> String {
    let bin = scratch(&format!("{tag}-bin"));
    let path = bin.join("vitest");
    std::fs::write(
        &path,
        "#!/bin/sh\n\
         want=' '; for a in \"$@\"; do case \"$a\" in *.test.ts) want=\"$want$a \";; esac; done\n\
         failed=0; passed=0; ran=0\n\
         while read -r f o; do\n\
           [ -z \"$f\" ] && continue\n\
           [ \"$want\" = ' ' ] || case \"$want\" in *\" $f \"*) ;; *) continue;; esac\n\
           ran=$((ran+1))\n\
           if [ \"$o\" = flake ]; then k=\"$TMPDIR/fl-$(pwd | cksum | cut -d' ' -f1)-$(echo $f | cksum | cut -d' ' -f1)\"; \
             if [ -f \"$k\" ]; then o=pass; else : > \"$k\"; o=fail; fi; fi\n\
           if [ \"$o\" = pass ]; then echo \" ✓ $f (1 test) 1ms\"; passed=$((passed+1)); \
           else echo \" FAIL  $f > suite > t\" >&2; failed=$((failed+1)); fi\n\
         done < suite.txt\n\
         [ -f counters.dir ] && { d=$(cat counters.dir); mkdir -p \"$d\" && echo $ran >> \"$d/calls\"; } 2>/dev/null\n\
         if [ $ran = 0 ]; then echo 'No test files found, exiting with code 0'; exit 0; fi\n\
         if [ $failed -gt 0 ]; then echo \" Test Files  $failed failed | $passed passed ($ran)\"; \
         else echo \" Test Files  $passed passed ($ran)\"; fi\n\
         [ -f unhandled ] && [ \"$want\" = ' ' ] && { echo '      Errors  1 error'; exit 1; }\n\
         [ $failed -gt 0 ] && exit 1; exit 0\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

/// The twelve files of the stand-in suite.
fn files() -> Vec<String> {
    (0..12).map(|i| format!("tests/t{i:02}.test.ts")).collect()
}

/// `suite.txt` with every file passing except `overrides`.
fn suite(overrides: &[(&str, &str)]) -> String {
    files()
        .iter()
        .map(|f| {
            let o = overrides
                .iter()
                .find(|(n, _)| *n == f.as_str())
                .map_or("pass", |(_, o)| *o);
            format!("{f} {o}\n")
        })
        .collect()
}

struct Twin {
    repo: PathBuf,
    ctx: FloorContext,
    counters: PathBuf,
}

/// A repository whose base commit runs `base_suite` and whose head (uncommitted) runs `head_suite`,
/// with the `rerun` declaration `rerun` in the head's checks config (or none).
fn twin(
    tag: &str,
    base_suite: &str,
    head_suite: &str,
    head_extra: &[(&str, &str)],
    rerun: Option<serde_json::Value>,
) -> Twin {
    let repo = scratch(tag);
    let vitest = fake_vitest(tag);
    let test = serde_json::json!([vitest, "run"]);
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    std::fs::create_dir_all(repo.join("tests")).unwrap();
    std::fs::write(repo.join(".gitignore"), "tmp/\n").unwrap();
    for f in files() {
        std::fs::write(repo.join(&f), "// a test file\n").unwrap();
    }
    std::fs::write(
        repo.join(CONFIG_PATH),
        serde_json::json!({ "test": test, "timeout_s": 60 }).to_string(),
    )
    .unwrap();
    std::fs::write(repo.join("suite.txt"), base_suite).unwrap();
    let counters = repo.join("tmp/counters");
    std::fs::write(
        repo.join("counters.dir"),
        counters.to_string_lossy().as_bytes(),
    )
    .unwrap();
    let base = git_repo_with_commit(&repo);
    std::fs::write(repo.join("suite.txt"), head_suite).unwrap();
    for (name, body) in head_extra {
        std::fs::write(repo.join(name), body).unwrap();
    }
    let mut cfg = serde_json::json!({ "test": test, "timeout_s": 60 });
    if let Some(r) = rerun {
        cfg["rerun"] = serde_json::json!({ "test": r });
    }
    std::fs::write(repo.join(CONFIG_PATH), cfg.to_string()).unwrap();
    let ctx = FloorContext {
        stage: FloorStage::Verify,
        base_head: Some(base),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    Twin {
        repo,
        ctx,
        counters,
    }
}

/// The `rerun` entry for the stand-in.
fn declared(self_retry: bool) -> serde_json::Value {
    let mut d = serde_json::json!({
        "command": ["__VITEST__", "run", "--passWithNoTests", "{files}"],
        "by": "file",
    });
    if self_retry {
        d["self_retry"] = true.into();
    }
    d
}

/// One floor of `twin` under load (the retry is due): its `test` check and the number of test
/// files each invocation ran, in order.
fn floor(t: &Twin) -> Option<(RepoChecksReport, CheckRun, Vec<usize>)> {
    // The declared command names the stand-in by its absolute path.
    let raw = std::fs::read_to_string(t.repo.join(CONFIG_PATH)).unwrap();
    let cfg: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let vitest = cfg["test"][0].as_str().unwrap().to_string();
    std::fs::write(t.repo.join(CONFIG_PATH), raw.replace("__VITEST__", &vitest)).unwrap();
    let _ = std::fs::remove_dir_all(&t.counters);
    let _load = seam::pin(4.0 * 2.5, 4);
    let report = run_floor(&t.repo, &t.ctx);
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
    let calls = std::fs::read_to_string(t.counters.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().parse().unwrap())
        .collect();
    Some((report, c, calls))
}

/// What must be identical with and without the declaration.
fn verdict(r: &RepoChecksReport, c: &CheckRun) -> (bool, String, Option<String>, bool) {
    let d = c.decision.as_ref().expect("decided");
    (
        r.passed,
        d.outcome.clone(),
        c.classification.clone(),
        d.flaky,
    )
}

/// Run `scenario` on twin repos with and without the declaration; both verdicts must equal `want`.
/// Returns (scoped, whole) runs.
#[allow(clippy::type_complexity)]
fn both(
    tag: &str,
    base_suite: &str,
    head_suite: &str,
    head_extra: &[(&str, &str)],
    decl: serde_json::Value,
    want: (bool, &str, Option<&str>, bool),
) -> Option<((CheckRun, Vec<usize>), (CheckRun, Vec<usize>))> {
    let _off = seam::exclude(false);
    let scoped = twin(
        &format!("{tag}-s"),
        base_suite,
        head_suite,
        head_extra,
        Some(decl),
    );
    let whole = twin(
        &format!("{tag}-w"),
        base_suite,
        head_suite,
        head_extra,
        None,
    );
    let (rs, cs, ns) = floor(&scoped)?;
    let (rw, cw, nw) = floor(&whole)?;
    let want = (
        want.0,
        want.1.to_string(),
        want.2.map(str::to_string),
        want.3,
    );
    assert_eq!(verdict(&rw, &cw), want, "whole: {cw:#?}");
    assert_eq!(verdict(&rs, &cs), want, "scoped: {cs:#?}");
    let _ = std::fs::remove_dir_all(&scoped.repo);
    let _ = std::fs::remove_dir_all(&whole.repo);
    Some(((cs, ns), (cw, nw)))
}

/// `checks[0]` of the `repoChecksEvaluated` the floor emits for `c`, through the real serializer.
fn wire_check(c: &CheckRun) -> serde_json::Value {
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
    .to_json()["checks"][0]
        .clone()
}

const B: &str = "tests/t03.test.ts";
const C: &str = "tests/t07.test.ts";

#[test]
fn a_flake_passes_on_a_retry_of_its_failing_file_only() {
    let head = suite(&[(B, "flake")]);
    let Some(((cs, ns), (cw, nw))) = both(
        "fs-flake",
        &suite(&[]),
        &head,
        &[],
        declared(false),
        (true, OUTCOME_PASS, Some(FLAKY_UNDER_LOAD), true),
    ) else {
        return;
    };
    assert_eq!(nw, vec![12, 12], "whole: the head and the identical retry");
    assert_eq!(ns, vec![12, 1], "scoped: the head and ONE file");
    let r = cs.retry.as_ref().expect("the retry is recorded");
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(r.scope.as_ref().unwrap().ids, vec![B.to_string()]);
    assert_eq!(r.whole_because, None);
    assert_eq!(cs.reruns.len(), 1);
    assert_eq!(cs.reruns[0].argv.last().map(String::as_str), Some(B));
    assert!(
        cs.base.is_none() && cw.base.is_none(),
        "a pass reads no base"
    );
    assert!(cw.retry.as_ref().is_some_and(|r| r.scope.is_none()));
    // The wire carries the retry with its own duration.
    let wire = wire_check(&cs);
    assert_eq!(wire["retry"]["exitCode"], 0);
    assert!(wire["retry"]["durationMs"].is_u64(), "{wire}");
    assert_eq!(wire["retry"]["scope"]["by"], "file");
    assert_eq!(wire["retry"]["scope"]["ids"][0], B);
    assert_eq!(wire["reruns"][0]["scope"]["ids"][0], B);
    assert_eq!(wire["selfRetry"], false);
}

#[test]
fn a_change_only_failure_compares_its_file_on_the_base_and_denies() {
    let Some(((cs, ns), (_, nw))) = both(
        "fs-new",
        &suite(&[]),
        &suite(&[(B, "fail")]),
        &[],
        declared(false),
        (false, OUTCOME_FLAKY_TWICE, Some(REGRESSION), false),
    ) else {
        return;
    };
    assert_eq!(nw, vec![12, 12, 12], "whole: head, retry, base");
    assert_eq!(ns, vec![12, 1, 1], "scoped: head, then one file twice");
    let b = cs.base.as_ref().unwrap();
    assert!(!b.cached);
    assert_eq!(
        b.run.as_ref().unwrap().scope.as_ref().unwrap().ids,
        vec![B.to_string()]
    );
    assert_eq!(cs.regressions, vec![format!("FAIL {B} > suite > t")]);
}

#[test]
fn a_failure_the_base_shares_escalates_as_before() {
    let Some(((_, ns), (_, nw))) = both(
        "fs-shared",
        &suite(&[(B, "fail")]),
        &suite(&[(B, "fail"), (C, "flake")]),
        &[],
        declared(false),
        (
            false,
            OUTCOME_BASE_RED_UNSEPARATED,
            Some(BASE_RED_UNSEPARATED),
            false,
        ),
    ) else {
        return;
    };
    assert_eq!(nw, vec![12, 12, 12]);
    assert_eq!(ns, vec![12, 2, 2]);
}

#[test]
fn an_error_outside_any_test_makes_a_passing_scoped_retry_run_the_whole_check() {
    // The head fails a flaky file AND has an unhandled error its ids do not name: the failing file
    // passes alone, but the whole check would still fail. The whole retry decides.
    let Some(((cs, ns), (_, nw))) = both(
        "fs-unhandled",
        &suite(&[]),
        &suite(&[(B, "flake")]),
        &[("unhandled", "")],
        declared(false),
        (false, OUTCOME_FLAKY_TWICE, Some(REGRESSION), false),
    ) else {
        return;
    };
    assert_eq!(nw, vec![12, 12, 12], "whole: head, retry, base");
    assert_eq!(
        ns,
        vec![12, 1, 12, 1],
        "head, the set, the whole retry, base set"
    );
    let r = cs.retry.as_ref().unwrap();
    assert!(r.scope.is_none(), "the deciding retry was the whole check");
    assert!(
        r.whole_because
            .as_deref()
            .is_some_and(|w| w.contains("errors outside any test")),
        "{r:?}"
    );
    assert_eq!(
        cs.reruns.len(),
        2,
        "the failing-set retry, then the whole one"
    );
    assert!(cs.reruns[0].scope.is_some() && cs.reruns[1].scope.is_none());
}

#[test]
fn a_failing_file_the_change_added_is_a_new_failure_without_a_whole_base() {
    let added = "tests/t99.test.ts";
    let mut head = suite(&[]);
    head.push_str(&format!("{added} fail\n"));
    let Some(((cs, ns), (_, nw))) = both(
        "fs-added",
        &suite(&[]),
        &head,
        &[(added, "// new\n")],
        declared(false),
        (false, OUTCOME_FLAKY_TWICE, Some(REGRESSION), false),
    ) else {
        return;
    };
    assert_eq!(nw, vec![13, 13, 12]);
    assert_eq!(
        ns,
        vec![13, 1, 0],
        "the base has no such file: it runs nothing and passes"
    );
    assert!(
        cs.earlier_passes.is_empty(),
        "no whole base: {:?}",
        cs.earlier_passes
    );
}

#[test]
fn a_base_pass_that_does_not_show_the_file_runs_the_whole_base() {
    // The base has the file but its suite does not list it (a filter that matched nothing): the
    // set's pass proves nothing, and the whole base decides.
    let head = suite(&[(B, "fail")]);
    let base: String = suite(&[])
        .lines()
        .filter(|l| !l.starts_with(B))
        .map(|l| format!("{l}\n"))
        .collect();
    let Some(((cs, ns), (_, nw))) = both(
        "fs-unproven",
        &base,
        &head,
        &[],
        declared(false),
        (false, OUTCOME_FLAKY_TWICE, Some(REGRESSION), false),
    ) else {
        return;
    };
    assert_eq!(nw, vec![12, 12, 11]);
    assert_eq!(
        ns,
        vec![12, 1, 0, 11],
        "the set on the base, then the whole base"
    );
    assert_eq!(cs.earlier_passes.len(), 1, "{:?}", cs.earlier_passes);
    assert!(cs
        .base
        .as_ref()
        .unwrap()
        .run
        .as_ref()
        .unwrap()
        .scope
        .is_none());
}

#[test]
fn a_check_that_retries_itself_is_compared_on_the_base_without_an_engine_retry() {
    let _off = seam::exclude(false);
    let t = twin(
        "fs-self",
        &suite(&[]),
        &suite(&[(B, "fail")]),
        &[],
        Some(declared(true)),
    );
    let Some((r, c, n)) = floor(&t) else {
        return;
    };
    assert_eq!(
        verdict(&r, &c),
        (
            false,
            OUTCOME_NEW_FAILURE.into(),
            Some(REGRESSION.into()),
            false
        )
    );
    assert_eq!(
        n,
        vec![12, 1],
        "the head, then its failing file on the base"
    );
    assert!(c.reruns.is_empty() && c.retry.is_none() && c.self_retry);
    assert_eq!(wire_check(&c)["selfRetry"], true);
    let _ = std::fs::remove_dir_all(&t.repo);
}

#[test]
fn a_base_red_only_where_the_head_passes_compares_the_heads_failures_test_by_test() {
    // The one place the comparison is finer than the whole base's: the base fails `C`, which the
    // head passes, and passes `B`, which the head fails. The whole base is red, so it cannot
    // separate and escalates. The failing-set base shows `B` passing without the change: a new
    // failure. Both deny.
    let _off = seam::exclude(false);
    let base = suite(&[(C, "fail")]);
    let head = suite(&[(B, "fail")]);
    let s = twin("fs-elsewhere-s", &base, &head, &[], Some(declared(false)));
    let w = twin("fs-elsewhere-w", &base, &head, &[], None);
    let (Some((rs, cs, _)), Some((rw, cw, _))) = (floor(&s), floor(&w)) else {
        return;
    };
    assert!(!rs.passed && !rw.passed);
    assert_eq!(
        cw.decision.as_ref().unwrap().outcome,
        OUTCOME_BASE_RED_UNSEPARATED
    );
    assert_eq!(cs.decision.as_ref().unwrap().outcome, OUTCOME_FLAKY_TWICE);
    let _ = std::fs::remove_dir_all(&s.repo);
    let _ = std::fs::remove_dir_all(&w.repo);
}

#[test]
fn a_cached_whole_base_answers_a_failing_set() {
    // A floor of the run ran the whole base; a later floor of the same run with the declaration
    // reads it and runs no base at all.
    let _off = seam::exclude(false);
    let t = twin("fs-cached", &suite(&[]), &suite(&[(B, "fail")]), &[], None);
    let Some((_, first, _)) = floor(&t) else {
        return;
    };
    assert!(first.base.as_ref().is_some_and(|b| !b.cached));
    let raw = std::fs::read_to_string(t.repo.join(CONFIG_PATH)).unwrap();
    let mut cfg: serde_json::Value = serde_json::from_str(&raw).unwrap();
    cfg["rerun"] = serde_json::json!({ "test": declared(false) });
    std::fs::write(t.repo.join(CONFIG_PATH), cfg.to_string()).unwrap();
    let Some((r, c, n)) = floor(&t) else {
        return;
    };
    assert_eq!(
        verdict(&r, &c),
        (
            false,
            OUTCOME_FLAKY_TWICE.into(),
            Some(REGRESSION.into()),
            false
        )
    );
    assert_eq!(
        n,
        vec![12, 1],
        "the head and its failing file; the base is the cached one"
    );
    assert!(c.base.as_ref().is_some_and(|b| b.cached));
    let _ = std::fs::remove_dir_all(&t.repo);
}

#[test]
fn a_bad_rerun_entry_fails_detection() {
    let repo = scratch("fs-bad-config");
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    std::fs::write(
        repo.join(CONFIG_PATH),
        serde_json::json!({
            "test": ["x"],
            "rerun": {"test": {"command": ["x"], "by": "file"}},
        })
        .to_string(),
    )
    .unwrap();
    let err = detect(&repo).expect_err("a malformed rerun entry is a detection error");
    assert!(
        err.contains("rerun.test") && err.contains("exactly once"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// A real cargo crate whose lib test `t::a` is red when `a_red`, and whose integration test `c`
/// is flaky when `c_flaky` (it fails the first time a floor runs it: a marker in the floor's own
/// `TMPDIR`). The base commit is green; the head is the edit, uncommitted.
fn real_twin(tag: &str, a_red: bool, c_flaky: bool, rerun: bool) -> (PathBuf, FloorContext) {
    let repo = scratch(tag);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join("tests")).unwrap();
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"scoped_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(repo.join(".gitignore"), "Cargo.lock\ntmp/\n").unwrap();
    let lib = |red: bool| {
        format!(
            "#[cfg(test)]\nmod t {{\n    #[test]\n    fn a() {{\n        assert!({}, \"BOOM\");\n    }}\n    #[test]\n    fn b() {{}}\n}}\n",
            !red
        )
    };
    let it = |flaky: bool| {
        if flaky {
            "#[test]\nfn c() {\n    let m = std::env::temp_dir().join(\"scoped-fixture-c\");\n    \
             if !m.exists() {\n        std::fs::write(&m, \"\").unwrap();\n        panic!(\"first run\");\n    }\n}\n\
             #[test]\nfn d() {}\n"
                .to_string()
        } else {
            "#[test]\nfn c() {}\n#[test]\nfn d() {}\n".to_string()
        }
    };
    let test = serde_json::json!(["cargo", "test", "--no-fail-fast"]);
    std::fs::write(
        repo.join(CONFIG_PATH),
        serde_json::json!({ "test": test, "timeout_s": 600 }).to_string(),
    )
    .unwrap();
    std::fs::write(repo.join("src/lib.rs"), lib(false)).unwrap();
    std::fs::write(repo.join("tests/it.rs"), it(false)).unwrap();
    let base = git_repo_with_commit(&repo);
    std::fs::write(repo.join("src/lib.rs"), lib(a_red)).unwrap();
    std::fs::write(repo.join("tests/it.rs"), it(c_flaky)).unwrap();
    let mut cfg = serde_json::json!({ "test": test, "timeout_s": 600 });
    if rerun {
        // Core's own declaration, minus `--locked` (the fixture has no lockfile).
        cfg["rerun"] = serde_json::json!({ "test": {
            "command": ["cargo", "test", "--no-fail-fast", "--", "{names}", "--exact"],
            "by": "name",
        }});
    }
    std::fs::write(repo.join(CONFIG_PATH), cfg.to_string()).unwrap();
    let ctx = FloorContext {
        stage: FloorStage::Verify,
        base_head: Some(base),
        git_dir: Some(repo.join(".git")),
        ..FloorContext::default()
    };
    (repo, ctx)
}

/// One real-cargo floor under load: the `test` check.
fn real_floor(repo: &Path, ctx: &FloorContext) -> Option<(RepoChecksReport, CheckRun)> {
    let _load = seam::pin(4.0 * 2.5, 4);
    let report = run_floor(repo, ctx);
    if report.sandbox_error.is_some() {
        eprintln!("repo_checks: no sandbox tool here — the floor cannot run");
        return None;
    }
    let c = report.checks.iter().find(|c| c.name == "test")?.clone();
    Some((report, c))
}

#[test]
fn real_cargo_retries_and_compares_only_the_failing_test_names() {
    let _off = seam::exclude(false);
    for (tag, a_red, want) in [
        // `c` is flaky and nothing else fails: its retry alone passes.
        (
            "fs-cargo-flake",
            false,
            (true, OUTCOME_PASS, Some(FLAKY_UNDER_LOAD), true),
        ),
        // `t::a` really fails and `c` is flaky: the retry of the two fails, and the base passes
        // them both.
        (
            "fs-cargo-red",
            true,
            (false, OUTCOME_FLAKY_TWICE, Some(REGRESSION), false),
        ),
    ] {
        let want = (
            want.0,
            want.1.to_string(),
            want.2.map(str::to_string),
            want.3,
        );
        let (ws, cs) = real_twin(&format!("{tag}-s"), a_red, true, true);
        let (ww, cw) = real_twin(&format!("{tag}-w"), a_red, true, false);
        let (Some((rs, s)), Some((rw, w))) = (real_floor(&ws, &cs), real_floor(&ww, &cw)) else {
            return;
        };
        assert_eq!(verdict(&rw, &w), want, "{tag} whole: {w:#?}");
        assert_eq!(verdict(&rs, &s), want, "{tag} scoped: {s:#?}");
        let retry = s.reruns.last().expect("retried");
        let names: Vec<&str> = if a_red { vec!["t::a", "c"] } else { vec!["c"] };
        assert_eq!(
            retry.argv,
            ["cargo", "test", "--no-fail-fast", "--"]
                .into_iter()
                .chain(names.iter().copied())
                .chain(["--exact"])
                .collect::<Vec<_>>(),
            "{tag}"
        );
        assert_eq!(s.reruns.len(), 1, "{tag}: no whole retry after it");
        assert_eq!(
            s.retry.as_ref().and_then(|r| r.whole_because.clone()),
            None,
            "{tag}"
        );
        if a_red {
            let b = s
                .base
                .as_ref()
                .and_then(|b| b.run.as_ref())
                .expect("the base ran");
            assert_eq!(b.argv, retry.argv, "{tag}: the base ran the same set");
            assert!(b.scope.is_some());
            assert_eq!(
                w.base.as_ref().and_then(|b| b.run.as_ref()).unwrap().argv,
                w.argv
            );
        }
        for r in [ws, ww] {
            let _ = std::fs::remove_dir_all(&r);
        }
    }
}
