use super::red_report;

/// (core#1173) `generated` parses strictly: paths and an argv (array or line), in order; a
/// path that escapes the repo, an empty set, an unknown key or a non-command is an error, never
/// "nothing declared". Globs match by segment.
#[test]
fn generated_declarations_parse_strictly() {
    let (sets, timeout) = crate::repo_checks::generated_from_config(
        r#"{"timeout_s": 600, "generated": [
                {"paths": ["packages/crew/endpoint-manifest.json"], "command": "npm run manifest"},
                {"paths": ["packages/crew/tests/generated/**"], "command": ["npm", "run", "gen"]}
            ]}"#,
    )
    .unwrap();
    assert_eq!(timeout, Some(600));
    assert_eq!(sets.len(), 2);
    assert_eq!(
        sets[0].argv,
        vec!["npm".to_string(), "run".to_string(), "manifest".to_string()]
    );
    assert!(sets[0].covers("packages/crew/endpoint-manifest.json"));
    assert!(!sets[0].covers("packages/crew/endpoint-manifest.json.bak"));
    assert!(sets[1].covers("packages/crew/tests/generated/api-sample.generated.test.ts"));
    assert!(!sets[1].covers("packages/crew/tests/other.test.ts"));
    assert_eq!(
        crate::repo_checks::generated_from_config("{}").unwrap().0,
        Vec::new()
    );
    for bad in [
        r#"{"generated": [{"paths": ["../x"], "command": "a"}]}"#,
        r#"{"generated": [{"paths": ["/abs"], "command": "a"}]}"#,
        r#"{"generated": [{"paths": ["a//b"], "command": "a"}]}"#,
        r#"{"generated": [{"paths": [], "command": "a"}]}"#,
        r#"{"generated": [{"paths": ["a"], "command": false}]}"#,
        r#"{"generated": [{"paths": ["a"], "command": []}]}"#,
        r#"{"generated": [{"paths": ["a"], "command": "a", "shell": true}]}"#,
    ] {
        assert!(
            crate::repo_checks::generated_from_config(bad).is_err(),
            "{bad}"
        );
    }
}

/// (j4-gates) The floor names the change's own failures — head minus base — and says when
/// every failure is the base's; with no names it says nothing it does not know.
#[test]
fn the_floor_names_head_only_failures_and_says_when_all_are_base_red() {
    let r = red_report(
        &["test a::mine", "test b::shared"],
        Some(&["test b::shared", "test c::other"]),
    );
    let s = r.failure_split().unwrap();
    assert_eq!(s.head_only, vec!["test: test a::mine".to_string()]);
    assert_eq!(s.base_shared, 1);
    assert!(!s.all_base_red);
    let why = r.denial_reason();
    assert!(
        why.contains("HEAD-ONLY failures") && why.contains("`test: test a::mine`"),
        "{why}"
    );
    assert!(
        !why.contains("`test: test b::shared`"),
        "a base failure is not named as the change's: {why}"
    );

    let r = red_report(
        &["test b::shared"],
        Some(&["test b::shared", "test c::other"]),
    );
    let s = r.failure_split().unwrap();
    assert!(s.all_base_red && s.head_only.is_empty(), "{s:?}");
    assert!(r.denial_reason().contains("none of them is this change's"));

    // A green base: every named failure is the change's.
    let s = red_report(&["test a::mine"], Some(&[]))
        .failure_split()
        .unwrap();
    assert_eq!(s.head_only.len(), 1);
    // No base at all: listed, never shown shared, never "all base red".
    let s = red_report(&["test a::mine"], None).failure_split().unwrap();
    assert!(
        s.head_only.is_empty() && !s.all_base_red && s.unknown.len() == 1,
        "{s:?}"
    );
    // No names (a lint, a type error): nothing to split.
    assert!(red_report(&[], Some(&["x"])).failure_split().is_none());
}

/// (xreview) A run that printed a base failure and then hung is not a complete account: the
/// split lists the shared failure but never says "none of them is this change's".
#[test]
fn a_timed_out_run_with_partial_names_is_never_all_base_red() {
    let mut r = red_report(&["test b::shared"], Some(&["test b::shared"]));
    r.checks[0].timed_out = true;
    r.checks[0].exit_code = None;
    let s = r.failure_split().unwrap();
    assert_eq!(s.base_shared, 1);
    assert!(!s.all_base_red, "{s:?}");
    assert!(!r.denial_reason().contains("none of them is this change's"));
}

/// (xreview) An excluded run named a head-only failure and the retry after it named nothing:
/// the head-only failure is still named, and the split is not "all base red".
/// (core#1172) The gate's split on the J4 shape: the check's final pass failed only a test the
/// base fails too, and four first-pass failures cleared on its own retry. They are named as
/// flaky (passed on retry), never as this change's, and the check still denies as before.
#[test]
fn failures_the_checks_retry_cleared_are_named_flaky_not_head_only() {
    let shared = "FAIL tests/system-route.test.ts > gateway";
    let mut r = red_report(&[shared], Some(&[shared]));
    r.checks[0].flaky_ids = vec![
        "FAIL tests/deliver-e2e.test.ts > delivers".to_string(),
        "FAIL tests/run-diff-790.test.ts > 500".to_string(),
    ];
    let denies = r.checks[0].denies();
    let s = r.failure_split().unwrap();
    assert!(s.head_only.is_empty(), "{s:?}");
    assert!(s.all_base_red, "{s:?}");
    assert_eq!(s.flaky.len(), 2, "{s:?}");
    let why = r.denial_reason();
    assert!(
        why.contains("flaky (passed on retry), not counted as this change's")
            && why.contains("`test: FAIL tests/deliver-e2e.test.ts > delivers`"),
        "{why}"
    );
    assert!(!why.contains("HEAD-ONLY"), "{why}");
    assert_eq!(r.checks[0].denies(), denies, "the verdict is untouched");
    assert!(!r.passed);
}

#[test]
fn an_unnamed_retry_never_erases_a_head_only_failure_an_earlier_run_named() {
    let mut r = red_report(&["test b::shared"], Some(&["test b::shared"]));
    let mut excluded = r.checks[0].clone();
    excluded.decision = None;
    excluded.base = None;
    excluded.failure_ids = vec!["test a::mine".to_string()];
    let mut retry = excluded.clone();
    retry.failure_ids = Vec::new();
    r.checks[0].reruns = vec![excluded, retry];
    let s = r.failure_split().unwrap();
    assert_eq!(s.head_only, vec!["test: test a::mine".to_string()], "{s:?}");
    assert!(!s.all_base_red, "{s:?}");
}
