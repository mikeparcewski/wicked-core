use super::*;

#[test]
fn the_runner_switch_reads_auto_container_and_sandbox_and_refuses_a_typo() {
    assert_eq!(mode_from(None), Ok(Mode::Auto));
    assert_eq!(mode_from(Some(" ")), Ok(Mode::Auto));
    assert_eq!(mode_from(Some("auto")), Ok(Mode::Auto));
    assert_eq!(mode_from(Some("container")), Ok(Mode::Container));
    assert_eq!(mode_from(Some("sandbox")), Ok(Mode::Sandbox));
    assert!(mode_from(Some("contianer"))
        .unwrap_err()
        .contains(RUNNER_ENV));
}

#[test]
fn the_memory_cap_sizes_the_cargo_jobs_one_per_two_gigabytes() {
    assert_eq!(memory_and_jobs(None, 8).unwrap(), ("6g".into(), 6 << 30, 3));
    assert_eq!(memory_and_jobs(Some("8g"), 8).unwrap().2, 4);
    assert_eq!(memory_and_jobs(Some("3072m"), 8).unwrap().2, 1);
    assert_eq!(
        memory_and_jobs(Some("24g"), 4).unwrap().2,
        4,
        "never past the VM's CPUs"
    );
    assert!(memory_and_jobs(Some("512m"), 8).is_err());
    assert!(memory_and_jobs(Some("6x"), 8).is_err());
}

#[test]
fn a_node_modules_volume_is_keyed_by_every_install_input() {
    let a = nm_volume("img:1", "", b"lock-a", b"in");
    assert_eq!(a, nm_volume("img:1", "", b"lock-a", b"in"));
    assert_ne!(a, nm_volume("img:1", "", b"lock-b", b"in"));
    assert_ne!(a, nm_volume("img:2", "", b"lock-a", b"in"));
    assert_ne!(a, nm_volume("img:1", "pkg", b"lock-a", b"in"));
    assert_ne!(a, nm_volume("img:1", "", b"lock-a", b"other"));
    assert!(a.starts_with("wfloor-nm-") && a.len() == "wfloor-nm-".len() + 16);
}

/// (xreview) A head whose lockfile matches the base's but whose manifests, npm configuration or
/// local `file:` package differ never shares the base's node_modules.
#[test]
fn the_install_inputs_hold_every_manifest_npmrc_and_a_local_package_s_tree() {
    let e = |p: &str, o: &str| Entry {
        mode: 0o100644,
        oid: o.into(),
        path: p.into(),
    };
    let base = vec![
        e("package.json", "1"),
        e("packages/a/package.json", "2"),
        e(".npmrc", "3"),
        e("src/index.ts", "4"),
    ];
    let lock = br#"{"packages":{"node_modules/x":{"resolved":"https://registry/x.tgz"}}}"#;
    let inputs = npm_inputs(&base, lock, "tree-1");
    // A source file is not an install input; a manifest and an .npmrc are.
    let mut src = base.clone();
    src[3].oid = "9".into();
    assert_eq!(npm_inputs(&src, lock, "tree-2"), inputs);
    let mut member = base.clone();
    member[1].oid = "9".into();
    assert_ne!(npm_inputs(&member, lock, "tree-1"), inputs);
    let mut rc = base.clone();
    rc[2].oid = "9".into();
    assert_ne!(npm_inputs(&rc, lock, "tree-1"), inputs);
    // (xreview) Read structurally: any JSON spelling, and a v1 `version: file:` too.
    assert!(installs_local_bytes(
        br#"{"packages":{"x":{"resolved" : "file:vendor/dep"}}}"#
    ));
    assert!(installs_local_bytes(
        br#"{"dependencies":{"x":{"version":"file:../x"}}}"#
    ));
    assert!(!installs_local_bytes(
        br#"{"packages":{"x":{"resolved":"https://r/x.tgz"}}}"#
    ));
    assert!(
        installs_local_bytes(b"not json"),
        "unknown is assumed local"
    );
    // A local `file:` package's bytes are the tree's: the tree is an input then.
    let local = br#"{"packages":{"node_modules/dep":{"resolved": "file:vendor/dep"}}}"#;
    assert_ne!(
        npm_inputs(&base, local, "tree-1"),
        npm_inputs(&base, local, "tree-2")
    );
}

/// (xreview) A fetch marker holds every manifest: a head that changes only a `Cargo.toml` (a
/// feature, a `[patch]`) is fetched again.
#[test]
fn the_cargo_fetch_inputs_hold_every_manifest() {
    let e = |p: &str, o: &str| Entry {
        mode: 0o100644,
        oid: o.into(),
        path: p.into(),
    };
    let a = cargo_inputs(
        &[
            e("Cargo.toml", "1"),
            e("crates/x/Cargo.toml", "2"),
            e("src/lib.rs", "3"),
        ],
        "t1",
    );
    let b = cargo_inputs(
        &[
            e("Cargo.toml", "1"),
            e("crates/x/Cargo.toml", "9"),
            e("src/lib.rs", "3"),
        ],
        "t1",
    );
    let c = cargo_inputs(
        &[
            e("Cargo.toml", "1"),
            e("crates/x/Cargo.toml", "2"),
            e("src/lib.rs", "8"),
        ],
        "t2",
    );
    assert_ne!(a, b);
    assert_eq!(a, c, "a source file is not a fetch input");
    // (xreview round 4) A linked manifest puts the tree in the marker.
    let mut l = e("Cargo.lock", "5");
    l.mode = 0o120000;
    assert_ne!(cargo_inputs(&[l.clone()], "t1"), cargo_inputs(&[l], "t2"));
}

/// (xreview) The cargo fetch's copy has no `.cargo` configuration (a `build.rustc` there would
/// run with the network and the shared cargo home) and no toolchain file.
#[test]
fn the_cargo_fetch_copy_leaves_out_cargo_configuration() {
    let e = |p: &str| Entry {
        mode: 0o100644,
        oid: "1".into(),
        path: p.into(),
    };
    let kept: Vec<String> = cargo_fill_entries(&[
        e("Cargo.toml"),
        e("Cargo.lock"),
        e(".cargo/config.toml"),
        e("crates/x/.cargo/config"),
        e("rust-toolchain.toml"),
        e("src/lib.rs"),
    ])
    .into_iter()
    .map(|e| e.path)
    .collect();
    assert_eq!(kept, vec!["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
}

#[test]
fn a_nested_package_s_mount_point_parents_are_listed_outermost_first() {
    let m = |dir: &str| NmMount {
        dir: dir.into(),
        volume: "v".into(),
        failed: None,
    };
    assert_eq!(
        nested_parents(
            "/work/head",
            &[m(""), m("crates/core-ts"), m("crates/other")]
        ),
        vec![
            "/work/head/crates",
            "/work/head/crates/core-ts",
            "/work/head/crates/other"
        ]
    );
    assert!(nested_parents("/work/head", &[m("")]).is_empty());
}

#[test]
fn a_target_volume_is_per_run_leaf_and_side() {
    let l = Path::new("/c/repo-checks/r/run-a");
    assert_ne!(target_volume(l, Tree::Head), target_volume(l, Tree::Base));
    assert_ne!(
        target_volume(l, Tree::Head),
        target_volume(Path::new("/c/repo-checks/r/run-b"), Tree::Head)
    );
}

#[test]
fn only_an_npm_ci_install_can_be_filled_from_a_lockfile() {
    let c = |argv: &[&str]| RepoCheck {
        name: "install".into(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        source: String::new(),
        timeout_s: None,
        dir: String::new(),
    };
    assert!(npm_ci(&c(&["npm", "ci", "--ignore-scripts"])).is_some());
    assert!(npm_ci(&c(&["npm", "install", "--ignore-scripts"])).is_none());
    assert!(npm_ci(&c(&["pnpm", "install", "--frozen-lockfile"])).is_none());
}

#[test]
fn ls_tree_records_keep_blobs_and_links_skip_submodules_and_refuse_escapes() {
    let raw = b"100644 blob aaa\tsrc/a.rs\x00100755 blob bbb\tbin/run\x00120000 blob ccc\tlink\x00160000 commit ddd\tvendor/sub\x00";
    let e = parse_ls_tree(raw).unwrap();
    assert_eq!(e.len(), 3);
    assert_eq!(e[1].mode, 0o100755);
    assert_eq!(e[2].mode, 0o120000);
    assert!(parse_ls_tree(b"100644 blob aaa\t../x\0").is_err());
    assert!(parse_ls_tree(b"100644 blob aaa\t/etc/x\0").is_err());
    // (xreview) A newline in a path would forge a PAX record in the copy: refused.
    assert!(parse_ls_tree(b"100644 blob aaa\tx\n27 path=/wicked/vol/evil\0").is_err());
}

/// (xreview round 3) A git dependency (whose `prepare` npm runs even with scripts off) is found
/// in any lockfile spelling; a registry one is not.
#[test]
fn a_git_dependency_is_found_in_the_lockfile() {
    assert!(git_dependency(
        br#"{"packages":{"node_modules/x":{"resolved":"git+ssh://git@github.com/a/x.git#abc"}}}"#
    )
    .is_some());
    assert!(git_dependency(br#"{"dependencies":{"x":{"version":"github:a/x#abc"}}}"#).is_some());
    // (xreview round 4) Any case, a gist, a byte-order mark, and an unparsable lockfile.
    assert!(git_dependency(br#"{"packages":{"x":{"resolved":"GIT+https://h/x.git"}}}"#).is_some());
    assert!(git_dependency(br#"{"packages":{"x":{"resolved":"gist:abc"}}}"#).is_some());
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(br#"{"packages":{"x":{"resolved":"git+ssh://h/x.git"}}}"#);
    assert!(git_dependency(&bom).is_some());
    assert!(git_dependency(b"{not json").is_some(), "fails closed");
    assert!(git_dependency(
        br#"{"packages":{"node_modules/x":{"resolved":"https://registry.npmjs.org/x/-/x-1.tgz"}}}"#
    )
    .is_none());
}

/// (xreview round 3) A linked npm input's bytes are its referent's: the whole tree joins the key.
#[test]
fn a_linked_npm_input_puts_the_tree_in_the_key() {
    let e = |p: &str, mode: u32| Entry {
        mode,
        oid: "1".into(),
        path: p.into(),
    };
    let lock = br#"{"packages":{}}"#;
    let plain = vec![e("package.json", 0o100644), e(".npmrc", 0o100644)];
    let linked = vec![e("package.json", 0o100644), e(".npmrc", 0o120000)];
    assert_eq!(
        npm_inputs(&plain, lock, "t1"),
        npm_inputs(&plain, lock, "t2")
    );
    assert_ne!(
        npm_inputs(&linked, lock, "t1"),
        npm_inputs(&linked, lock, "t2")
    );
}

/// (xreview round 3) A link target that is not UTF-8 is refused, never rewritten lossily.
#[test]
fn a_non_utf8_link_target_is_refused() {
    let mut out = Vec::new();
    assert!(tar_member(&mut out, "l", 0o120000, 1, b"target\xff").is_err());
}

/// (xreview) A link whose target holds a control character is refused, never written into a PAX
/// record.
#[test]
fn a_link_target_with_a_control_character_is_refused() {
    let mut out = Vec::new();
    let long = format!("{}\n27 path=/etc/x\n", "a".repeat(120));
    assert!(tar_member(&mut out, "l", 0o120000, 1, long.as_bytes()).is_err());
    assert!(tar_member(&mut out, "l", 0o120000, 1, b"ok/target").is_ok());
}

#[test]
fn an_unchanged_blob_keeps_its_mtime_and_a_changed_or_new_one_is_stamped_now() {
    let e = |p: &str, o: &str| Entry {
        mode: 0o100644,
        oid: o.into(),
        path: p.into(),
    };
    let first = Mtimes::default();
    // A changed file is stamped past the copy: later than any build an earlier copy ran, even in
    // the same second (xreview: whole-second stamps let cargo keep a stale artifact).
    let m = MTIME_MARGIN_S;
    let (t1, m1) = first.assign(&[e("a", "1"), e("b", "2")], 100);
    assert_eq!(t1, vec![100 + m, 100 + m]);
    let (t2, m2) = m1.assign(&[e("a", "1"), e("b", "3"), e("c", "4")], 100);
    assert_eq!(
        t2,
        vec![100 + m, 101 + m, 100 + m],
        "same second: changed b strictly later than the mtime its old bytes had"
    );
    let (t2, m2b) = m2.assign(&[e("a", "1"), e("b", "3"), e("c", "4")], 200);
    assert_eq!(t2, vec![100 + m, 101 + m, 100 + m], "unchanged: kept");
    // Reverting a file is a change too.
    let (t3, _) = m2b.assign(&[e("a", "1"), e("b", "2")], 300);
    assert_eq!(t3, vec![100 + m, 300 + m]);
    // A recorded time in the future (a clock step back) is never kept.
    let (t4, _) = m2b.assign(&[e("a", "1")], 50);
    assert_eq!(t4, vec![50 + m]);
}

/// The tar the engine writes extracts with the system `tar` to the same files, modes, links
/// and mtimes, long names included.
#[cfg(unix)]
#[test]
fn the_engine_written_tar_extracts_to_the_same_tree() {
    let root = std::env::temp_dir().join(format!("wfc-tar-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    struct Gone(PathBuf);
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _gone = Gone(root.clone());
    let tar = root.join("t.tar");
    let long = format!("{}/file.txt", "d".repeat(120));
    let long_link = "x".repeat(150);
    {
        let mut f = std::fs::File::create(&tar).unwrap();
        tar_member(&mut f, "a.txt", 0o100644, 1_000_000, b"hello").unwrap();
        tar_member(&mut f, "bin/run", 0o100755, 1_000_001, b"#!/bin/sh\n").unwrap();
        tar_member(&mut f, "ln", 0o120000, 1_000_002, b"a.txt").unwrap();
        tar_member(&mut f, &long, 0o100644, 1_000_003, &vec![b'z'; 1300]).unwrap();
        tar_member(&mut f, "lnlong", 0o120000, 1_000_004, long_link.as_bytes()).unwrap();
        f.write_all(&[0u8; 1024]).unwrap();
    }
    let out = root.join("out");
    std::fs::create_dir(&out).unwrap();
    // spawn-audit: test-only — the system tar reads the engine's archive.
    let st = Command::new("tar")
        .hardened()
        .args(["-x", "-f"])
        .arg(&tar)
        .arg("-C")
        .arg(&out)
        .status()
        .unwrap();
    assert!(st.success());
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    assert_eq!(std::fs::read(out.join("a.txt")).unwrap(), b"hello");
    let m = std::fs::metadata(out.join("bin/run")).unwrap();
    assert_ne!(m.permissions().mode() & 0o111, 0, "executable bit kept");
    assert_eq!(
        std::fs::metadata(out.join("a.txt")).unwrap().mtime(),
        1_000_000
    );
    assert_eq!(
        std::fs::read_link(out.join("ln")).unwrap(),
        Path::new("a.txt")
    );
    assert_eq!(std::fs::read(out.join(&long)).unwrap().len(), 1300);
    assert_eq!(
        std::fs::read_link(out.join("lnlong")).unwrap(),
        Path::new(&long_link)
    );
}

/// (xreview) The fill is a fixed command: no repository argv (`--ignore-scripts=false`) reaches
/// the networked install container.
#[test]
fn an_install_container_runs_a_fixed_install_with_scripts_off() {
    assert_eq!(
        NPM_FILL,
        ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"]
    );
    assert!(!NPM_FILL.iter().any(|a| a.contains("=false")));
}

/// The check runs as-is inside a shell that ends what it left behind and keeps its exit code.
/// (Never executed here: `kill -1` on the host would signal every process of the user. The
/// container floor's live proof runs it, where the checks' user owns only the check.)
#[test]
fn a_check_runs_inside_the_reaping_shell_with_its_argv_untouched() {
    let argv: Vec<String> = ["cargo", "test", "--", "a b", "$HOME"]
        .map(String::from)
        .to_vec();
    let w = reaping(&argv);
    assert_eq!(&w[..2], ["sh", "-c"]);
    assert!(w[2].starts_with("\"$@\"; rc=$?; kill -9 -1") && w[2].ends_with("exit $rc"));
    assert_eq!(
        &w[4..],
        argv.as_slice(),
        "the check's argv is passed as positional args"
    );
}

#[test]
fn an_oom_kill_is_read_from_the_cgroup_events() {
    assert_eq!(
        oom_kills("low 0\nhigh 0\nmax 37\noom 1\noom_kill 2\noom_group_kill 0\n"),
        2
    );
    assert_eq!(oom_kills(""), 0);
}

#[test]
fn a_runtime_error_line_is_the_launcher_not_the_check() {
    assert!(launcher_failure("OCI runtime exec failed: exec failed: chdir", Some(127)).is_some());
    assert!(launcher_failure(
        "Error response from daemon: No such container: x",
        Some(126)
    )
    .is_some());
    assert!(launcher_failure("error[E0425]: cannot find value", Some(127)).is_none());
    // (xreview) The line alone is the check's to print: with the check's own exit code it is the
    // check's failure.
    assert!(launcher_failure("OCI runtime exec failed: forged", Some(1)).is_none());
    assert!(launcher_failure("Error response from daemon: forged", Some(101)).is_none());
}

#[test]
fn eviction_takes_idle_volumes_extra_node_modules_then_the_least_recently_used() {
    let t = |s: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(s);
    let v = |n: &str, repo: &str, gb: u64, used: u64| VolumeUse {
        name: n.into(),
        repo: repo.into(),
        bytes: gb * 1_000_000_000,
        used: t(used),
    };
    let now = t(1_000_000);
    let ttl = Duration::from_secs(500_000);
    let vols = vec![
        v("wfloor-nm-1", "crew", 1, 999_000),
        v("wfloor-nm-2", "crew", 1, 998_000),
        v("wfloor-nm-3", "crew", 1, 997_000),
        v("wfloor-nm-4", "crew", 1, 996_000),
        v("wfloor-target-old", "core", 5, 100),
        v("wfloor-target-a", "core", 10, 990_000),
        v("wfloor-target-b", "core", 10, 995_000),
        v(CARGO_HOME_VOLUME, "", 1, 0),
    ];
    let none = std::collections::HashSet::new();
    let plan = eviction_plan(
        vols.clone(),
        now,
        ttl,
        100_000_000_000,
        100_000_000_000,
        &none,
    );
    assert_eq!(plan, vec!["wfloor-nm-4", "wfloor-target-old"]);
    // A 15 GB total cap takes the least recently used target next (a before b).
    let plan = eviction_plan(
        vols.clone(),
        now,
        ttl,
        15_000_000_000,
        100_000_000_000,
        &none,
    );
    assert!(plan.contains(&"wfloor-target-a".to_string()));
    assert!(!plan.contains(&"wfloor-target-b".to_string()));
    assert!(
        !plan.contains(&CARGO_HOME_VOLUME.to_string()),
        "the registry is shared"
    );
    // A 12 GB per-repository cap does the same for core alone.
    let plan = eviction_plan(
        vols.clone(),
        now,
        ttl,
        100_000_000_000,
        12_000_000_000,
        &none,
    );
    assert!(plan.contains(&"wfloor-target-a".to_string()));
    assert!(!plan.contains(&"wfloor-nm-1".to_string()));
    // (xreview) A leased volume is never chosen, still counts, and the next one is taken instead:
    // a 15 GB cap with `a` leased takes `b`.
    let leased: std::collections::HashSet<String> = ["wfloor-target-a".to_string()].into();
    let plan = eviction_plan(vols, now, ttl, 15_000_000_000, 100_000_000_000, &leased);
    assert!(!plan.contains(&"wfloor-target-a".to_string()));
    assert!(plan.contains(&"wfloor-target-b".to_string()));
}

/// `repoChecksEvaluated` says which floor ran (`floorRunner`), and a frame without one reads
/// as before the field.
#[test]
fn the_event_carries_which_floor_ran() {
    let frame = |runner: Option<FloorRunner>| {
        crate::event::CoreEvent::RepoChecksEvaluated {
            session: "s".into(),
            ord: 2,
            attempt: 0,
            passed: true,
            criterion: "c".into(),
            checks: Vec::new(),
            skipped: Vec::new(),
            sandbox_level: "sandboxed".into(),
            sandbox_error: None,
            detect_error: None,
            outcome: "passed".into(),
            floor: "creator".into(),
            claim: None,
            env: None,
            changed: Vec::new(),
            changed_truncated: false,
            reused_from: None,
            floor_runner: runner.map(Box::new),
        }
        .to_json()
    };
    assert!(frame(None).get("floorRunner").is_none());
    let c = frame(Some(FloorRunner {
        runner: "container".into(),
        memory_cap: Some("6g".into()),
        network: "none".into(),
        ..Default::default()
    }));
    assert_eq!(c["floorRunner"]["runner"], "container");
    assert_eq!(c["floorRunner"]["memoryCap"], "6g");
    assert_eq!(c["floorRunner"]["network"], "none");
    assert_eq!(c["floor"], "creator", "the stage keeps its own field");
    let h = frame(Some(FloorRunner::host(
        "sandboxed",
        &["sandbox-exec".into()],
        Some("the container runtime does not answer".into()),
    )));
    assert_eq!(h["floorRunner"]["runner"], "macos-sandbox");
    assert_eq!(
        h["floorRunner"]["fallbackReason"],
        "the container runtime does not answer"
    );
}

#[test]
fn runtime_sizes_parse() {
    assert_eq!(parse_size("336MB"), 336_000_000);
    assert_eq!(parse_size("13.2GB"), 13_200_000_000);
    assert_eq!(parse_size("0B"), 0);
    assert_eq!(parse_size("1.5GiB"), 1_610_612_736);
    assert_eq!(parse_size("512MiB"), 536_870_912);
}

#[test]
fn the_host_record_names_the_launcher() {
    assert_eq!(
        FloorRunner::host("sandboxed", &["/usr/bin/sandbox-exec".into()], None).runner,
        "macos-sandbox"
    );
    assert_eq!(
        FloorRunner::host("sandboxed", &["bwrap".into()], None).runner,
        "linux-bwrap"
    );
    assert_eq!(FloorRunner::host("none", &[], None).runner, "none");
    let r = FloorRunner::host(
        "sandboxed",
        &["sandbox-exec".into()],
        Some("docker is down".into()),
    );
    assert_eq!(r.fallback_reason.as_deref(), Some("docker is down"));
    // (xreview) A refused runner ran nothing: `none`, never `container`.
    let refused = FloorRunner::refused("WICKED_FLOOR_RUNNER=container and no runtime".into());
    assert_eq!(refused.runner, "none");
    assert!(refused.fallback_reason.unwrap().contains("no runtime"));
}

#[test]
fn a_pax_record_counts_its_own_length() {
    let r = pax_record("path", "abc");
    let s = String::from_utf8(r.clone()).unwrap();
    let n: usize = s.split(' ').next().unwrap().parse().unwrap();
    assert_eq!(n, r.len());
    let r = pax_record("path", &"p".repeat(95));
    let s = String::from_utf8(r.clone()).unwrap();
    let n: usize = s.split(' ').next().unwrap().parse().unwrap();
    assert_eq!(n, r.len());
}

/// The container floor end to end, against the real runtime and image (opt-in: it needs a
/// container runtime and builds the checks image on first use, so CI's default suite leaves it to
/// this env switch; run it with `WICKED_TEST_CONTAINER_FLOOR=1 cargo test --lib
/// checks_container::tests::live::` — 3 tests; they serialize on one lock). One fixture repository per case: a package with no
/// dependencies (its `npm ci` makes no node_modules), whose scripts are the checks.
#[cfg(unix)]
mod live {
    use super::super::{test_seam, Mode};
    use crate::repo_checks::{run_floor, FloorContext, FloorStage};
    use std::path::{Path, PathBuf};

    fn enabled(test: &str) -> bool {
        let on = std::env::var("WICKED_TEST_CONTAINER_FLOOR").is_ok_and(|v| v == "1");
        if !on {
            eprintln!(
                "SKIP {test}: set WICKED_TEST_CONTAINER_FLOOR=1 to run the live container floor"
            );
        }
        on
    }

    fn git(dir: &Path, args: &[&str]) {
        // spawn-audit: test-only — builds the fixture repository.
        let st = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }

    /// A committed fixture repository whose package scripts are `scripts`; returns (dir, commit).
    fn fixture(name: &str, scripts: &[(&str, &str)], extra: &[(&str, &str)]) -> (PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("wfc-live-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let scripts: serde_json::Map<String, serde_json::Value> = scripts
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
            .collect();
        let pkg = serde_json::json!({"name": "fx", "version": "1.0.0", "scripts": scripts});
        std::fs::write(
            dir.join("package.json"),
            serde_json::to_vec_pretty(&pkg).unwrap(),
        )
        .unwrap();
        let lock = serde_json::json!({
            "name": "fx", "version": "1.0.0", "lockfileVersion": 3, "requires": true,
            "packages": {"": {"name": "fx", "version": "1.0.0"}}
        });
        std::fs::write(
            dir.join("package-lock.json"),
            serde_json::to_vec_pretty(&lock).unwrap(),
        )
        .unwrap();
        for (p, body) in extra {
            std::fs::write(dir.join(p), body).unwrap();
        }
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["add", "-A"]);
        git(
            &dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "fx",
            ],
        );
        // spawn-audit: test-only — reads the fixture's head commit.
        let head = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&dir)
            .output()
            .unwrap();
        (
            dir,
            String::from_utf8_lossy(&head.stdout).trim().to_string(),
        )
    }

    /// The live tests run one at a time: they share the runtime and one of them sets the
    /// process-wide memory cap.
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn floor(dir: &Path, base: &str) -> crate::repo_checks::RepoChecksReport {
        test_seam::set_mode(Some(Mode::Container));
        let run = format!(
            "live-{}-{}",
            dir.file_name().unwrap().to_string_lossy(),
            std::process::id()
        );
        let ctx = FloorContext {
            stage: FloorStage::Creator,
            base_head: Some(base.to_string()),
            git_dir: Some(dir.join(".git")),
            run_id: Some(run.clone()),
            ..Default::default()
        };
        let r = run_floor(dir, &ctx);
        test_seam::set_mode(None);
        crate::repo_checks::remove_check_scratch(dir, &run);
        r
    }

    #[test]
    fn live_a_passing_floor_runs_in_the_container_with_no_network() {
        if !enabled("live_a_passing_floor_runs_in_the_container_with_no_network") {
            return;
        }
        let _one = ONE.lock().unwrap_or_else(|p| p.into_inner());
        // `test` passes only when the network is unreachable and it runs as the checks' user in
        // the copy (no .git) with the marker set.
        let (dir, head) = fixture(
            "pass",
            &[("test", "node t.js")],
            &[(
                "t.js",
                "const net=require('net');const fs=require('fs');\
                 if(process.env.WICKED_CHECKS_FLOOR!=='container'||fs.existsSync('.git')){process.exit(2)}\
                 const s=net.connect(443,'1.1.1.1');s.setTimeout(3000,()=>process.exit(3));\
                 s.on('error',()=>process.exit(0));s.on('connect',()=>process.exit(4));",
            )],
        );
        let r = floor(&dir, &head);
        let runner = r.runner.clone().unwrap();
        assert_eq!(
            runner.runner, "container",
            "{runner:?} {:?}",
            r.sandbox_error
        );
        assert!(r.passed, "{:#?}", r.checks);
        assert_eq!(runner.network, "none");
        assert!(runner.containers.len() == 1, "{runner:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_a_check_that_rewrites_the_tree_stands_for_nothing() {
        if !enabled("live_a_check_that_rewrites_the_tree_stands_for_nothing") {
            return;
        }
        let _one = ONE.lock().unwrap_or_else(|p| p.into_inner());
        let (dir, head) = fixture(
            "mutate",
            &[
                (
                    "lint",
                    "node -e \"require('fs').writeFileSync('src.js','fixed')\"",
                ),
                ("test", "node src.js"),
            ],
            &[("src.js", "process.exit(1)")],
        );
        let r = floor(&dir, &head);
        let lint = r
            .checks
            .iter()
            .find(|c| c.name == "lint")
            .expect("lint ran");
        assert!(
            lint.spawn_error
                .as_deref()
                .is_some_and(|e| e.contains("changed tracked files") && e.contains("src.js")),
            "{lint:#?}"
        );
        assert!(!r.passed);
        assert!(
            !r.checks.iter().any(|c| c.name == "test" && c.passed()),
            "the test never passes on the rewritten copy"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_an_oom_kill_is_floor_infrastructure_whatever_the_exit_code() {
        if !enabled("live_an_oom_kill_is_floor_infrastructure_whatever_the_exit_code") {
            return;
        }
        let _one = ONE.lock().unwrap_or_else(|p| p.into_inner());
        // A child that outgrows the 1g cap is killed; the check itself exits 0 (it tolerates the
        // death), and the floor still refuses to stand on it.
        std::env::set_var(super::super::MEMORY_ENV, "1g");
        let (dir, head) = fixture(
            "oom",
            &[(
                "test",
                "node -e \"const r=require('child_process').spawnSync('node',['-e','const a=[];for(;;)a.push(Buffer.alloc(64<<20,1))']);process.exit(0)\"",
            )],
            &[],
        );
        let r = floor(&dir, &head);
        std::env::remove_var(super::super::MEMORY_ENV);
        let t = r
            .checks
            .iter()
            .find(|c| c.name == "test")
            .expect("test ran");
        assert_eq!(t.exit_code, Some(0));
        assert!(
            t.spawn_error
                .as_deref()
                .is_some_and(|e| e.contains("ran out of memory")),
            "{t:#?}"
        );
        assert!(!r.passed);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
