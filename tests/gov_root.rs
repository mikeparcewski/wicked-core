//! Exercise governance root resolution through the non-test library, with a fresh cache per child.
//! No governance ctor override: each parent test configures only its probe subprocess.

use std::path::{Path, PathBuf};
use std::process::Command;

use wicked_core::gov_run_dir;

const ROOT_MARKER: &str = "GOV_ROOT=";
const HOME_AFTER_FIRST: &str = "WICKED_GOV_PROBE_HOME_AFTER_FIRST";

fn scratch() -> PathBuf {
    std::env::temp_dir().join(format!("wicked-core-gov-root-{}", std::process::id()))
}

fn probe(
    home: Option<&Path>,
    profile: Option<&Path>,
    root: Option<&Path>,
    later_home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut cmd = Command::new(std::env::current_exe().expect("current test executable"));
    cmd.args([
        "--exact",
        "probe_gov_root",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ])
    .env_remove("HOME")
    .env_remove("USERPROFILE")
    .env_remove("WICKED_GOV_ROOT")
    .env_remove(HOME_AFTER_FIRST);
    if let Some(home) = home {
        cmd.env("HOME", home);
    }
    if let Some(profile) = profile {
        cmd.env("USERPROFILE", profile);
    }
    if let Some(root) = root {
        cmd.env("WICKED_GOV_ROOT", root);
    }
    if let Some(home) = later_home {
        cmd.env(HOME_AFTER_FIRST, home);
    }
    let output = cmd.output().expect("run governance root probe");
    assert!(
        output.status.success(),
        "probe failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("probe stdout is UTF-8")
        .lines()
        // libtest may print its test-name prefix before the first marker on the same line.
        .filter_map(|line| line.split_once(ROOT_MARKER).map(|(_, path)| path))
        .map(|path| serde_json::from_str(path).expect("probe prints a JSON path"))
        .collect()
}

#[test]
fn home_wins_over_userprofile() {
    let base = scratch();
    let home = base.join("home");
    let profile = base.join("profile");
    assert_eq!(
        probe(Some(&home), Some(&profile), None, None),
        vec![home.join(".wicked/core-gov")]
    );
}

#[test]
fn userprofile_is_used_when_home_is_unset() {
    let profile = scratch().join("profile");
    assert_eq!(
        probe(None, Some(&profile), None, None),
        vec![profile.join(".wicked/core-gov")]
    );
}

#[test]
fn unusable_homes_fall_back_to_temp() {
    let temp = std::env::temp_dir();
    for (home, profile) in [
        (None, None),
        (Some(Path::new("")), Some(Path::new("relative-profile"))),
        (Some(Path::new("relative-home")), Some(Path::new(""))),
    ] {
        assert_eq!(
            probe(home, profile, None, None),
            vec![temp.join("wicked-core-gov")]
        );
    }
}

#[test]
fn absolute_override_wins_over_home() {
    let base = scratch();
    let home = base.join("home");
    let root = base.join("override");
    assert_eq!(probe(Some(&home), None, Some(&root), None), vec![root]);
}

#[test]
fn empty_or_relative_override_is_ignored() {
    let home = scratch().join("home");
    for root in [Path::new(""), Path::new("relative-root")] {
        assert_eq!(
            probe(Some(&home), None, Some(root), None),
            vec![home.join(".wicked/core-gov")]
        );
    }
}

#[test]
fn changing_home_after_first_lookup_keeps_the_cached_root() {
    let base = scratch();
    let home = base.join("first-home");
    let later_home = base.join("later-home");
    let expected = home.join(".wicked/core-gov");
    assert_eq!(
        probe(Some(&home), None, None, Some(&later_home)),
        vec![expected.clone(), expected]
    );
}

#[test]
#[ignore = "only run by the parent tests with an isolated environment"]
fn probe_gov_root() {
    let print_root = || {
        let run_dir = gov_run_dir("probe");
        let root = run_dir.parent().expect("run directory has a root");
        println!("{ROOT_MARKER}{}", serde_json::to_string(root).unwrap());
    };
    print_root();
    if let Some(home) = std::env::var_os(HOME_AFTER_FIRST) {
        // This subprocess runs only this helper, with one test thread.
        std::env::set_var("HOME", home);
        print_root();
    }
}

#[cfg(unix)]
#[test]
fn an_unwritable_home_is_named_at_boot() {
    use std::os::unix::fs::PermissionsExt;

    let base = scratch().join("unwritable-home-boot");
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::write(home.join("write-check"), b"").is_ok() {
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
        eprintln!("skipping: 0500 HOME is still writable (running as root)");
        return;
    }

    let output = Command::new(env!("CARGO_BIN_EXE_wicked-core"))
        .args(["status", "--db"])
        .arg(base.join("store.db"))
        .env_remove("USERPROFILE")
        .env_remove("WICKED_GOV_ROOT")
        .env("HOME", &home)
        .output();
    // Restore permissions and clean up before asserting, including when the child failed to spawn.
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::remove_dir_all(&base).unwrap();

    let output = output.expect("status runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let root = home.join(".wicked/core-gov");
    assert!(
        stderr.contains(root.to_str().unwrap()),
        "boot must name the unwritable governance root {}: {stderr}",
        root.display()
    );
    assert!(
        stderr.contains("WICKED_GOV_ROOT"),
        "boot must name the writable-root remedy: {stderr}"
    );
    assert!(
        stderr.contains("every governed unit will be refused"),
        "boot must explain the effect on governed units: {stderr}"
    );
    assert!(
        output.status.success(),
        "status must remain usable: {stderr}"
    );
}

/// SAFETY (`ctor(unsafe)`): pre-main, single-threaded; only arms the per-process emit spool.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
