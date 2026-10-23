/// A build-guide check (DES-BUILD-GUIDE, contract v1) for a libtest-style runner — `cargo test` and
/// the stand-ins that mimic it: the binaries' `Running` lines (stderr, merged) qualify ids; a
/// failure is a name in the final `failures:` list; one test runs alone with `<name> --exact`, and
/// deselects with `--skip=<name> --exact`. Verified (every probe `pass`) on `verified` when it is
/// not empty. Test fixture data, shared by the lib's and the integration tests: the floor knows
/// none of it.
#[allow(dead_code)]
pub fn libtest_guide(argv: &[String], verified: &str) -> serde_json::Value {
    let tail = |extra: &[&str]| {
        let mut v = argv.to_vec();
        if !argv.iter().any(|a| a == "--") {
            v.push("--".into());
        }
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    };
    let section = r"^\s+(?:Running .*\((?:.*[/\\])?|Doc-tests )(?P<q>[^/\\()\s]+)\)?$";
    let mut g = serde_json::json!({
        "name": "test", "cwd": ".", "stage": ["creator", "verify"],
        "run": argv, "failedExit": [101],
        "results": {"kind": "text", "how": {"merge": true}},
        "parser": {"section": section, "blocks": [{
            "start": "^failures:$", "item": r"^    (?P<id>\S+)$",
            "end": "^test result: FAILED", "use": "last-before-end"}]},
        "list": {"run": tail(&["--list"]), "item": r"^(?P<id>\S+): test$",
                 "section": section, "merge": true},
        "alone": {"run": tail(&["{id}", "--exact"])},
        "deselect": {"append": ["--", "--skip={id}", "--exact"]},
        "invalidatedBy": [".cargo/**", "**/.cargo/**"],
    });
    if !verified.is_empty() {
        g["verified"] = serde_json::json!({
            "baseSha": verified, "runnerVersion": "", "at": "",
            "results": {"list": "pass", "run": "pass", "alone": "pass",
                        "deselect": "pass", "mutation": "pass"}});
    }
    g
}
