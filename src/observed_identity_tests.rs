use super::*;

/// The identity of an interpreter process names the program it runs (pure).
#[test]
fn an_interpreters_identity_is_the_program_it_runs() {
    let h = |p: &Path| Some(format!("sha256:{}", p.file_name()?.to_str()?));
    let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let cwd = Path::new("/w");
    // A script, relative to the process's own cwd.
    assert_eq!(
        interpreter_identity(
            "sha256:node",
            "node",
            &argv(&["node", "--x", "cli.js"]),
            Some(cwd),
            &h
        ),
        Some("sha256:node+sha256:cli.js".into())
    );
    // Two programs on one interpreter are two identities.
    assert_ne!(
        interpreter_identity(
            "sha256:node",
            "node",
            &argv(&["node", "a.js"]),
            Some(cwd),
            &h
        ),
        interpreter_identity(
            "sha256:node",
            "node",
            &argv(&["node", "b.js"]),
            Some(cwd),
            &h
        )
    );
    // Inline text.
    let a = interpreter_identity(
        "sha256:sh",
        "sh",
        &argv(&["sh", "-c", "claude -p x"]),
        None,
        &h,
    );
    assert!(
        a.as_deref()
            .is_some_and(|s| s.starts_with("sha256:sh+inline:")),
        "{a:?}"
    );
    // `bash -p` is not inline text (privileged mode), so the script after it is the program.
    assert_eq!(
        interpreter_identity(
            "sha256:bash",
            "bash",
            &argv(&["bash", "-p", "/s.sh"]),
            None,
            &h
        ),
        Some("sha256:bash+sha256:s.sh".into())
    );
    // A relative script with no readable cwd, and `env` about to exec: unobserved.
    assert_eq!(
        interpreter_identity("sha256:node", "node", &argv(&["node", "cli.js"]), None, &h),
        None
    );
    assert_eq!(
        interpreter_identity("sha256:env", "env", &argv(&["env", "node", "x"]), None, &h),
        None
    );
    assert_eq!(
        interpreter(Path::new("/usr/bin/python3.12")),
        Some("python")
    );
    assert_eq!(interpreter(Path::new("/opt/x/claude")), None);
}

/// (core#1543, KEY) Real processes: whatever a launch is called — the binary itself, a script that
/// execs it, a script that spawns it, a relative-path alias in its own cwd — its principal identity
/// is the binary's content; another binary is another identity; a tool the agent runs on a
/// captured pipe is never a principal.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_launch_is_what_it_runs_under_any_name() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::process::CommandExt;
    let dir = std::env::temp_dir().join(format!("wcore-observe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = |name: &str, body: &str| {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    };
    // Two different "CLIs" (a copied platform binary will not run on macOS): scripts whose own
    // children never hold the launch's stdout, so each is its own principal.
    let claude = script(
        "claude",
        "# agent A\nwhile :; do sleep 0.05 >/dev/null 2>&1; done",
    );
    let codex = script(
        "codex",
        "# agent B\nwhile :; do sleep 0.05 >/dev/null 2>&1; done",
    );
    let execs = script("execs", &format!("exec \"{}\" 3", claude.display()));
    let spawns = script("spawns", &format!("\"{}\" 3\n", claude.display()));
    let with_tool = script(
        "with-tool",
        &format!(
            "\"{}\" > /dev/null 2>&1 &\nexec \"{}\" 3",
            codex.display(),
            claude.display()
        ),
    );
    std::os::unix::fs::symlink(&claude, dir.join("worker-alias")).unwrap();
    let observe = |prog: &Path, args: &[&str]| -> Option<Vec<Identity>> {
        let mut child = std::process::Command::new(prog)
            .args(args)
            .current_dir(&dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let fd = child.stdout.as_ref().unwrap().as_raw_fd();
        let mut obs = Observer::new(child.id(), fd).expect("observable here");
        for _ in 0..8 {
            std::thread::sleep(std::time::Duration::from_millis(150));
            obs.sample();
        }
        let _ = child.kill();
        unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
        let _ = child.wait();
        obs.finish()
    };
    let direct = observe(&claude, &["3"]).expect("observed");
    assert_eq!(direct.len(), 1, "{direct:?}");
    assert_eq!(
        observe(&execs, &[]).as_ref(),
        Some(&direct),
        "a script that execs it"
    );
    assert_eq!(
        observe(&spawns, &[]).as_ref(),
        Some(&direct),
        "a script that spawns it"
    );
    assert_eq!(
        observe(Path::new("./worker-alias"), &["3"]).as_ref(),
        Some(&direct),
        "a relative-path alias, resolved where it ran"
    );
    assert_eq!(
        observe(&with_tool, &[]).as_ref(),
        Some(&direct),
        "a tool on a captured pipe is not a principal"
    );
    let other = observe(&codex, &[]).expect("observed");
    assert!(!meets(&direct, &other), "two binaries, two identities");
    let _ = std::fs::remove_dir_all(&dir);
}
