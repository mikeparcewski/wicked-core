//! `wicked-core floor probe` (DES-BUILD-GUIDE) never defaults to the operator's worker home:
//! without an explicit `WICKED_WORKER_HOME` it runs in a fresh worker home under the system temp
//! dir and removes it at the end. Driven through the real binary with `HOME` pointed at a temp
//! dir, so a regression lands in that dir, never in a real home.

#![cfg_attr(not(unix), allow(unused_imports))]

use std::path::Path;
use std::process::Command;

/// Route every fire-and-forget `wicked.*` emission this binary triggers to a per-process temp spool
/// instead of the operator's real replay queue (core#311). Every binary in this suite carries this
/// block; `harness_hygiene.rs` fails the suite if one is missing.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

#[cfg(unix)]
fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

#[cfg(unix)]
#[test]
fn the_probe_without_a_worker_home_writes_nothing_under_the_default_home() {
    let root = std::env::temp_dir().join(format!("wicked-core-probe-home-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, repo) = (root.join("home"), root.join("repo"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    // A runner that lists two tests and passes them.
    std::fs::write(
        repo.join("runner.sh"),
        "if [ \"$1\" = list ]; then echo 't1: test'; echo 't2: test'; exit 0; fi\necho ok\n",
    )
    .unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "a repo with a runner"]);
    let guide = root.join("guide.json");
    let check = serde_json::json!({
        "name": "test", "cwd": ".", "run": ["sh", "runner.sh"], "failedExit": [1],
        "results": {"kind": "text"},
        "parser": {"failed": [r"^FAIL (?P<id>\S+)$"]},
        "list": {"run": ["sh", "runner.sh", "list"], "item": r"^(?P<id>\S+): test$"},
        "alone": {"run": ["sh", "runner.sh", "one", "{id}"]},
        "deselect": {"append": ["--skip={id}"]},
    });
    let doc = serde_json::json!({"version": 1, "derivedFrom": [], "checks": [check]});
    std::fs::write(&guide, doc.to_string()).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_wicked-core"))
        .args(["floor", "probe", "--repo"])
        .arg(&repo)
        .arg("--guide")
        .arg(&guide)
        .env_remove(wicked_apps_core::spawn::WORKER_HOME_ENV)
        .env_remove("USERPROFILE")
        .env("HOME", &home)
        .output()
        .expect("the probe runs");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let seen = format!(
        "exit {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status.code()
    );
    // The probe got as far as its scratch: it ran the list, or the host has no floor sandbox (the
    // scratch is prepared before the sandbox is armed).
    assert!(
        stdout.contains("test\tlist\tpass")
            || stderr.contains("the floor sandbox does not arm here"),
        "{seen}"
    );
    assert!(
        !home.join(".wicked-worker").exists(),
        "the probe wrote under the default worker home: {:?}\n{seen}",
        std::fs::read_dir(&home).map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
    );
    let used = stderr
        .lines()
        .find_map(|l| l.strip_prefix("wicked-core floor probe: worker home "))
        .and_then(|rest| rest.split(" (temporary").next())
        .unwrap_or_else(|| panic!("the probe names its temporary worker home\n{seen}"));
    let tmp = std::env::temp_dir().canonicalize().unwrap();
    assert!(
        Path::new(used).starts_with(&tmp),
        "{used} is not under {}",
        tmp.display()
    );
    assert!(
        !Path::new(used).exists(),
        "the temporary worker home {used} was not removed"
    );
    let _ = std::fs::remove_dir_all(&root);
}
