//! Smoke test for the operator CLI's validator subcommands (Lane D finding 5). Proves the new
//! `provision-validator` / `approve-validator` subcommands arg-parse and are advertised in the usage
//! string — WITHOUT running the live authoring (`provision-validator` shells out to real `claude`, so
//! only the missing-arg / usage paths, which fail BEFORE any store or CLI call, are exercised here).

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_wicked-core")
}

#[test]
fn provision_validator_requires_a_criterion() {
    let out = Command::new(bin())
        .arg("provision-validator")
        .output()
        .expect("run wicked-core");
    assert!(
        !out.status.success(),
        "provision-validator with no --criterion must exit non-zero"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--criterion"),
        "the error names the missing flag: {err}"
    );
}

#[test]
fn approve_validator_requires_a_pin() {
    let out = Command::new(bin())
        .arg("approve-validator")
        .output()
        .expect("run wicked-core");
    assert!(
        !out.status.success(),
        "approve-validator with no --pin must exit non-zero"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--pin"),
        "the error names the missing flag: {err}"
    );
}

#[test]
fn usage_advertises_the_validator_subcommands() {
    // An unknown subcommand prints the usage string (exit 2). Point --db at a throwaway path so the
    // actor's store open does not litter the working directory.
    let db = std::env::temp_dir().join(format!("wicked-cli-smoke-{}.db", std::process::id()));
    let out = Command::new(bin())
        .args(["bogus-subcommand", "--db"])
        .arg(&db)
        .output()
        .expect("run wicked-core");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("provision-validator") && err.contains("approve-validator"),
        "usage advertises the new subcommands: {err}"
    );
    let _ = std::fs::remove_file(&db);
}

#[test]
fn usage_labels_launch_as_a_stub_self_test() {
    // Seam finding #6: `launch` fabricates deterministic stub success through a gate-less path — it must
    // be plainly LABELLED as a stub self-test in the usage, never presented as a real `run`.
    let db = std::env::temp_dir().join(format!("wicked-cli-smoke-stub-{}.db", std::process::id()));
    let out = Command::new(bin())
        .args(["bogus-subcommand", "--db"])
        .arg(&db)
        .output()
        .expect("run wicked-core");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("launch") && err.contains("STUB"),
        "usage must label `launch` as a STUB self-test: {err}"
    );
    let _ = std::fs::remove_file(&db);
}
