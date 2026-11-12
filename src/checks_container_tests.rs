use super::*;

#[test]
fn the_floor_image_trusts_both_copied_sides_in_system_git_config() {
    for dir in ["/work/head", "/work/base"] {
        assert!(DOCKERFILE.contains(&format!("git config --system --add safe.directory {dir}")));
    }
}

#[test]
fn host_git_disables_transports_lazy_fetch_hooks_and_host_configuration() {
    let c = git_cmd(Path::new("pinned.git"), &["rev-parse", "HEAD"]);
    let args: Vec<_> = c.get_args().map(|a| a.to_str().unwrap()).collect();
    for setting in [
        "core.hooksPath=/dev/null",
        "core.fsmonitor=false",
        "protocol.allow=never",
    ] {
        assert!(args.windows(2).any(|a| a == ["-c", setting]));
    }
    let env: std::collections::BTreeMap<_, _> = c.get_envs().collect();
    for (key, value) in [
        ("GIT_NO_LAZY_FETCH", "1"),
        ("GIT_ALLOW_PROTOCOL", "none"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_OPTIONAL_LOCKS", "0"),
    ] {
        assert_eq!(
            env.get(std::ffi::OsStr::new(key)),
            Some(&Some(std::ffi::OsStr::new(value)))
        );
    }
}

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
    let a = nm_volume("img:1", "", b"lock-a", b"in", &[]);
    assert_eq!(a, nm_volume("img:1", "", b"lock-a", b"in", &[]));
    assert_ne!(a, nm_volume("img:1", "", b"lock-b", b"in", &[]));
    assert_ne!(a, nm_volume("img:2", "", b"lock-a", b"in", &[]));
    assert_ne!(a, nm_volume("img:1", "pkg", b"lock-a", b"in", &[]));
    assert_ne!(a, nm_volume("img:1", "", b"lock-a", b"other", &[]));
    // (core#1393) A volume whose addons were built is never the one without.
    assert_ne!(
        a,
        nm_volume("img:1", "", b"lock-a", b"in", &["better-sqlite3"])
    );
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
        native: Vec::new(),
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

#[cfg(unix)]
mod private_git {
    use super::*;

    struct Fixture {
        root: PathBuf,
        repo: PathBuf,
        git_dir: PathBuf,
        base: String,
        head_tree: String,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let mut c = git_cmd(&dir.join(".git"), &[]);
        c.args(["-c", "safe.directory=*"])
            .args(args)
            .current_dir(dir)
            .env("HOME", dir)
            .env("GIT_AUTHOR_NAME", "floor test")
            .env("GIT_AUTHOR_EMAIL", "test@wicked.invalid")
            .env("GIT_COMMITTER_NAME", "floor test")
            .env("GIT_COMMITTER_EMAIL", "test@wicked.invalid")
            .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
            .stdin(Stdio::null());
        let o = output_within(c, PROBE).expect("fixture git timed out");
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap().trim().to_string()
    }

    fn assert_object_absent(dir: &Path, id: &str) {
        let mut c = git_cmd(&dir.join(".git"), &["cat-file", "-e", id]);
        c.current_dir(dir).stdin(Stdio::null());
        let o = output_within(c, PROBE).expect("cat-file timed out");
        assert!(!o.status.success(), "exported unrelated object {id}");
    }

    fn fixture() -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "wicked-floor-git-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main", "--template="]);
        std::fs::write(repo.join("tracked.txt"), "older\n").unwrap();
        std::fs::write(repo.join("ancestor-only.txt"), "pre-base secret\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "older than base"]);
        git(&repo, &["rm", "-q", "ancestor-only.txt"]);
        std::fs::write(repo.join("tracked.txt"), "A\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "A"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["update-ref", "refs/remotes/origin/main", &base]);
        git(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("committed.txt"), "B\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "B"]);
        std::fs::write(repo.join("tracked.txt"), "C\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "C"]);
        std::fs::write(repo.join("tracked.txt"), "uncommitted\n").unwrap();
        std::fs::write(repo.join("untracked-new.txt"), "new\n").unwrap();
        let git_dir = repo.join(".git");
        let head_tree = crate::worktree_guard::snapshot_through(&repo, &git_dir)
            .unwrap()
            .tree;
        Fixture {
            root,
            repo,
            git_dir,
            base,
            head_tree,
        }
    }

    fn extract(f: &Fixture, id: &str, files: Option<&GitFiles>, name: &str) -> PathBuf {
        let entries = ls_tree(&f.git_dir, id).unwrap();
        let mut archive = Vec::new();
        let held = write_side_tar(
            &f.git_dir,
            &entries,
            &vec![0; entries.len()],
            files,
            &mut archive,
        )
        .unwrap();
        assert_eq!(held.len(), entries.len());
        assert!(held.iter().all(|(p, _)| !p.starts_with(".git/")));
        let out = f.root.join(name);
        std::fs::create_dir(&out).unwrap();
        // spawn-audit: test-only — extracts the exact floor copy-in stream into a private dir.
        let mut c = Command::new("tar");
        c.hardened().args(["-x", "-f", "-", "-C"]).arg(&out);
        let result = run_fed(c, PROBE, |w| {
            w.write_all(&archive).map_err(|e| e.to_string())
        })
        .unwrap();
        assert!(
            result.status.is_some_and(|s| s.success()),
            "{}",
            tail(&result.err, 6)
        );
        result.fed.unwrap();
        out
    }

    #[test]
    fn floor_git_head_snapshot_survives_removing_the_source() {
        let f = fixture();
        let pinned_head = git(&f.repo, &["rev-parse", "HEAD"]);
        let intermediate_blob = git(&f.repo, &["rev-parse", "HEAD:tracked.txt"]);
        let older_blob = git(
            &f.repo,
            &["rev-parse", &format!("{}^:ancestor-only.txt", f.base)],
        );
        let files = floor_git(&f.git_dir, &f.head_tree, Tree::Head, Some(&f.base)).unwrap();
        let again = floor_git(&f.git_dir, &f.head_tree, Tree::Head, Some(&f.base)).unwrap();
        let file = |files: &GitFiles, rel: &str| {
            files.iter().find(|(p, _, _)| p == rel).unwrap().2.clone()
        };
        assert_eq!(
            file(&files, "refs/heads/feature"),
            file(&again, "refs/heads/feature")
        );
        let out = extract(&f, &f.head_tree, Some(&files), "head");
        std::fs::remove_dir_all(&f.repo).unwrap();
        assert_eq!(
            git(&out, &["ls-files"]),
            "committed.txt\ntracked.txt\nuntracked-new.txt"
        );
        assert_eq!(git(&out, &["merge-base", "HEAD", "origin/main"]), f.base);
        assert_eq!(
            git(&out, &["symbolic-ref", "refs/remotes/origin/HEAD"]),
            "refs/remotes/origin/main"
        );
        assert_eq!(
            git(&out, &["diff", "--name-only", &f.base]),
            "committed.txt\ntracked.txt\nuntracked-new.txt"
        );
        assert_eq!(
            git(&out, &["show", &format!("{}:tracked.txt", f.base)]),
            "A"
        );
        assert_eq!(git(&out, &["status", "--porcelain"]), "");
        assert_eq!(git(&out, &["rev-parse", "--abbrev-ref", "HEAD"]), "feature");
        let head = git(&out, &["rev-parse", "HEAD"]);
        assert_eq!(
            git(&out, &["rev-list", "HEAD"]),
            format!("{head}\n{}", f.base)
        );
        assert_eq!(
            git(&out, &["log", "--format=%s", "-1"]),
            "wicked floor snapshot"
        );
        assert_eq!(
            std::fs::read_to_string(out.join(".git/shallow")).unwrap(),
            format!("{}\n", f.base)
        );
        for id in [&pinned_head, &intermediate_blob, &older_blob] {
            assert_object_absent(&out, id);
        }
        assert!(!out.join(".git/objects/info/alternates").exists());
        assert!(!out.join(".git/hooks").exists());
        for dir in [
            "refs",
            "refs/heads",
            "refs/tags",
            "objects",
            "objects/info",
            "objects/pack",
        ] {
            assert!(out.join(".git").join(dir).is_dir(), "missing {dir}");
        }
    }

    #[test]
    fn floor_git_base_has_the_base_commit_and_index() {
        let f = fixture();
        let files = floor_git(&f.git_dir, &f.base, Tree::Base, Some(&f.base)).unwrap();
        let out = extract(&f, &f.base, Some(&files), "base");
        std::fs::remove_dir_all(&f.repo).unwrap();
        assert_eq!(git(&out, &["rev-parse", "HEAD"]), f.base);
        assert_eq!(git(&out, &["ls-files"]), "tracked.txt");
        assert_eq!(git(&out, &["status", "--porcelain"]), "");
        assert_eq!(git(&out, &["rev-list", "HEAD"]), f.base);
        assert_eq!(
            std::fs::read_to_string(out.join(".git/shallow")).unwrap(),
            format!("{}\n", f.base)
        );
    }

    #[test]
    fn floor_git_plain_git_discovers_the_copy_from_a_subdirectory() {
        let f = fixture();
        std::fs::create_dir(f.repo.join("nested")).unwrap();
        std::fs::write(f.repo.join("nested/from-subdir.txt"), "nested\n").unwrap();
        let tree = crate::worktree_guard::snapshot_through(&f.repo, &f.git_dir)
            .unwrap()
            .tree;
        let files = floor_git(&f.git_dir, &tree, Tree::Head, Some(&f.base)).unwrap();
        let out = extract(&f, &tree, Some(&files), "plain");
        std::fs::remove_dir_all(&f.repo).unwrap();
        let plain = |args: &[&str]| {
            // spawn-audit: test-only — ordinary discovery, no GIT_DIR or safe.directory override.
            let mut c = Command::new("git");
            c.hardened()
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", &out)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .current_dir(out.join("nested"))
                .args(args)
                .stdin(Stdio::null());
            let o = output_within(c, PROBE).expect("plain git timed out");
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8(o.stdout).unwrap().trim().to_string()
        };
        assert_eq!(plain(&["ls-files"]), "from-subdir.txt");
        assert_eq!(plain(&["merge-base", "HEAD", "origin/main"]), f.base);
    }

    #[test]
    fn floor_git_diverged_head_uses_the_base_without_exporting_the_common_ancestor() {
        let f = fixture();
        let ancestor = git(&f.repo, &["rev-parse", &format!("{}^", f.base)]);
        let blob = git(
            &f.repo,
            &["rev-parse", &format!("{ancestor}:ancestor-only.txt")],
        );
        git(&f.repo, &["clean", "-fd"]);
        git(
            &f.repo,
            &["checkout", "-q", "-f", "-B", "feature", &ancestor],
        );
        git(&f.repo, &["rm", "-q", "ancestor-only.txt"]);
        std::fs::write(f.repo.join("tracked.txt"), "diverged\n").unwrap();
        git(&f.repo, &["add", "-A"]);
        git(&f.repo, &["commit", "-q", "-m", "sibling of base"]);
        assert_eq!(git(&f.repo, &["merge-base", "HEAD", &f.base]), ancestor);
        let pinned_head = git(&f.repo, &["rev-parse", "HEAD"]);
        let tree = git(&f.repo, &["rev-parse", "HEAD^{tree}"]);
        let files = floor_git(&f.git_dir, &tree, Tree::Head, Some(&f.base)).unwrap();
        let out = extract(&f, &tree, Some(&files), "diverged");
        std::fs::remove_dir_all(&f.repo).unwrap();
        assert_eq!(git(&out, &["merge-base", "HEAD", "origin/main"]), f.base);
        assert_eq!(git(&out, &["rev-list", "--count", "HEAD"]), "2");
        assert_eq!(
            git(&out, &["show", &format!("{}:tracked.txt", f.base)]),
            "A"
        );
        for id in [&ancestor, &blob, &pinned_head] {
            assert_object_absent(&out, id);
        }
    }

    #[test]
    fn floor_git_alternates_chain_does_not_export_an_unrelated_blob() {
        let f = fixture();
        let other = f.root.join("other");
        std::fs::create_dir(&other).unwrap();
        git(&other, &["init", "-q", "--template="]);
        std::fs::write(other.join("unrelated.txt"), "alternate-only secret\n").unwrap();
        let blob = git(&other, &["hash-object", "-w", "unrelated.txt"]);
        std::fs::write(
            f.git_dir.join("objects/info/alternates"),
            format!("{}\n", other.join(".git/objects").display()),
        )
        .unwrap();
        git(&f.repo, &["cat-file", "-e", &blob]);
        let files = floor_git(&f.git_dir, &f.head_tree, Tree::Head, Some(&f.base)).unwrap();
        let out = extract(&f, &f.head_tree, Some(&files), "alternates-chain");
        std::fs::remove_dir_all(&f.repo).unwrap();
        std::fs::remove_dir_all(&other).unwrap();
        assert_eq!(git(&out, &["merge-base", "HEAD", "origin/main"]), f.base);
        assert_object_absent(&out, &blob);
        assert!(!out.join(".git/objects/info/alternates").exists());
    }

    #[test]
    fn the_copy_in_writer_refuses_only_exact_dot_git_components() {
        let f = fixture();
        let oid = git(&f.repo, &["rev-parse", "HEAD:tracked.txt"]);
        for path in [".git", ".git/config", "nested/.git/HEAD"] {
            let entries = vec![Entry {
                mode: 0o100644,
                oid: oid.clone(),
                path: path.into(),
            }];
            assert!(write_side_tar(&f.git_dir, &entries, &[0], None, &mut Vec::new()).is_err());
        }
        for path in [
            "git~1/HEAD",
            "nested/GIT~1/config",
            ".GiT/config",
            "nested/.gitignore",
        ] {
            let entries = vec![Entry {
                mode: 0o100644,
                oid: oid.clone(),
                path: path.into(),
            }];
            let held = write_side_tar(&f.git_dir, &entries, &[0], None, &mut Vec::new()).unwrap();
            assert_eq!(held[0].0, path);
        }
    }

    #[test]
    fn floor_git_failure_still_copies_the_complete_tree() {
        let f = fixture();
        let missing = "0000000000000000000000000000000000000001";
        let error = floor_git(&f.git_dir, &f.head_tree, Tree::Head, Some(missing)).unwrap_err();
        assert!(error.starts_with("rev-parse: exit status: "), "{error}");
        assert!(
            !error.contains(missing),
            "only the step and exit status are reported"
        );
        let files = optional_floor_git(&f.git_dir, &f.head_tree, Tree::Head, Some(missing));
        assert!(files.is_none());
        let out = extract(&f, &f.head_tree, files.as_deref(), "without-git");
        assert!(!out.join(".git").exists());
        for name in ["committed.txt", "tracked.txt", "untracked-new.txt"] {
            assert_eq!(
                std::fs::read(out.join(name)).unwrap(),
                std::fs::read(f.repo.join(name)).unwrap()
            );
        }
    }

    #[test]
    fn floor_git_unborn_head_gets_a_parentless_snapshot_without_origin() {
        let f = fixture();
        git(&f.repo, &["symbolic-ref", "HEAD", "refs/heads/unborn"]);
        let files = floor_git(&f.git_dir, &f.head_tree, Tree::Head, None).unwrap();
        let out = extract(&f, &f.head_tree, Some(&files), "unborn");
        std::fs::remove_dir_all(&f.repo).unwrap();
        assert_eq!(git(&out, &["rev-list", "--count", "HEAD"]), "1");
        assert_eq!(git(&out, &["rev-parse", "--abbrev-ref", "HEAD"]), "unborn");
        assert_eq!(git(&out, &["for-each-ref", "refs/remotes"]), "");
        assert_eq!(git(&out, &["status", "--porcelain"]), "");
        assert!(!git(&out, &["cat-file", "-p", "HEAD"])
            .lines()
            .any(|l| l.starts_with("parent ")));
    }

    #[test]
    fn floor_git_without_a_run_base_ignores_the_pinned_history() {
        let f = fixture();
        let pinned_head = git(&f.repo, &["rev-parse", "HEAD"]);
        let files = floor_git(&f.git_dir, &f.head_tree, Tree::Head, None).unwrap();
        let out = extract(&f, &f.head_tree, Some(&files), "no-base");
        std::fs::remove_dir_all(&f.repo).unwrap();
        let head = git(&out, &["rev-parse", "HEAD"]);
        assert_eq!(git(&out, &["rev-list", "HEAD"]), head);
        assert_eq!(git(&out, &["for-each-ref", "refs/remotes"]), "");
        assert_eq!(
            std::fs::read_to_string(out.join(".git/shallow")).unwrap(),
            format!("{head}\n")
        );
        assert!(!git(&out, &["cat-file", "-p", "HEAD"])
            .lines()
            .any(|l| l.starts_with("parent ")));
        assert_object_absent(&out, &pinned_head);
    }

    #[test]
    fn floor_git_detached_head_gets_a_snapshot_and_defaults_to_origin_main() {
        let f = fixture();
        let head = git(&f.repo, &["rev-parse", "HEAD"]);
        let tree = git(&f.repo, &["rev-parse", "HEAD^{tree}"]);
        git(&f.repo, &["checkout", "-q", "--detach", &head]);
        git(
            &f.repo,
            &["update-ref", "--no-deref", "-d", "refs/remotes/origin/HEAD"],
        );
        let files = floor_git(&f.git_dir, &tree, Tree::Head, Some(&f.base)).unwrap();
        let out = extract(&f, &tree, Some(&files), "detached");
        std::fs::remove_dir_all(&f.repo).unwrap();
        assert_ne!(git(&out, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(&out, &["rev-list", "--count", "HEAD"]), "2");
        assert_eq!(
            git(&out, &["log", "--format=%s", "-1"]),
            "wicked floor snapshot"
        );
        assert_eq!(
            git(&out, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "wicked-floor"
        );
        assert_eq!(git(&out, &["merge-base", "HEAD", "origin/main"]), f.base);
        assert_eq!(git(&out, &["status", "--porcelain"]), "");
    }

    #[test]
    fn floor_git_uses_the_pinned_common_dir_after_worktree_git_redirection() {
        let f = fixture();
        let linked = f.root.join("linked");
        git(
            &f.repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "linked",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        );
        let pinned = crate::worktree_guard::pinned_git_dir(&linked, &f.repo).unwrap();
        std::fs::write(linked.join("tracked.txt"), "linked snapshot\n").unwrap();
        let snapshot = crate::worktree_guard::snapshot_through(&linked, &pinned).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: missing-redirected-git\n").unwrap();
        git(
            &f.repo,
            &["update-ref", "refs/remotes/origin/trunk", &f.base],
        );
        git(
            &f.repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/trunk",
            ],
        );
        let files = floor_git(&pinned, &snapshot.tree, Tree::Head, Some(&f.base)).unwrap();
        let out = extract(&f, &snapshot.tree, Some(&files), "linked-copy");
        std::fs::remove_dir_all(&f.repo).unwrap();
        std::fs::remove_dir_all(&linked).unwrap();
        assert_eq!(git(&out, &["rev-parse", "--abbrev-ref", "HEAD"]), "linked");
        assert_eq!(git(&out, &["merge-base", "HEAD", "origin/trunk"]), f.base);
        assert_eq!(git(&out, &["status", "--porcelain"]), "");
    }
}

/// The fill pod's install is fixed, with every lifecycle script off (B-6 review: the Kubernetes
/// fill runs exactly this argv).
#[test]
fn the_fill_install_is_fixed_with_scripts_off() {
    assert_eq!(
        NPM_FILL,
        ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"]
    );
    assert!(!NPM_FILL.iter().any(|a| a.contains("=false")));
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
/// checks_container::tests::live::` with `WICKED_K8S_ADMIN_CONTEXT` set — one test). One fixture repository per case: a package with no
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

    /// The cluster the floor's pods run on for these tests: installed into a throwaway home with
    /// the admin context `WICKED_K8S_ADMIN_CONTEXT` (B-1 `cluster install`), the floor then on
    /// the engine's own account; uninstalled on drop.
    struct Cluster(crate::k8s::Kube);
    impl Drop for Cluster {
        fn drop(&mut self) {
            match crate::k8s::cluster::uninstall(&self.0, false) {
                Ok(gone) => eprintln!("uninstalled: {gone:?}"),
                Err(e) => eprintln!("UNINSTALL FAILED: {e}"),
            }
        }
    }

    fn cluster() -> Cluster {
        let ctx = std::env::var("WICKED_K8S_ADMIN_CONTEXT")
            .expect("the live floor needs WICKED_K8S_ADMIN_CONTEXT");
        let home = std::env::temp_dir().join(format!(
            "wicked-k8s-floor-live-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let mut admin = crate::k8s::resolve_settings(
            &home,
            None,
            None,
            Some(&ctx),
            std::env::var_os("HOME").map(PathBuf::from).as_deref(),
            crate::validator::find_on_path,
        )
        .expect("kubectl and the admin context");
        admin.owner = crate::k8s::owner_id(&home);
        let guard = Cluster(admin.clone());
        crate::k8s::cluster::install(&admin, &home).expect("cluster install");
        // Each live test its own home: the first noted would otherwise win for the process.
        crate::k8s::forget_engine_home_for_test();
        crate::k8s::note_engine_home(&home);
        guard
    }

    #[test]
    fn k8s_live_studios_vitest_runs_in_a_floor_pod() {
        // core#1424: Vite writes node_modules/.vite-temp (its config bundle) and .vite; on a
        // read-only node_modules claim it could not start. A clean, committed studio checkout at
        // WICKED_K8S_LIVE_STUDIO; its own `test` script (vitest run) must pass in a floor pod.
        if !enabled("k8s_live_studios_vitest_runs_in_a_floor_pod") {
            return;
        }
        let Some(dir) = std::env::var_os("WICKED_K8S_LIVE_STUDIO").map(PathBuf::from) else {
            eprintln!("SKIP: set WICKED_K8S_LIVE_STUDIO to a studio checkout");
            return;
        };
        let _one = ONE.lock().unwrap_or_else(|p| p.into_inner());
        let _cluster = cluster();
        // spawn-audit: test-only — reads the checkout's head commit.
        let head = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&dir)
            .output()
            .unwrap();
        let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
        // The full suite (a power run's hand-off): the whole `vitest run`, not only the tests
        // related to a change.
        test_seam::set_mode(Some(Mode::Container));
        let run = format!("live-studio-{}", std::process::id());
        crate::power::set_full_suite(&run, true);
        let ctx = FloorContext {
            stage: FloorStage::Creator,
            base_head: Some(head.clone()),
            git_dir: Some(dir.join(".git")),
            run_id: Some(run.clone()),
            ..Default::default()
        };
        let r = run_floor(&dir, &ctx);
        crate::power::set_full_suite(&run, false);
        test_seam::set_mode(None);
        crate::repo_checks::remove_check_scratch(&dir, &run);
        let names: Vec<&str> = r.checks.iter().map(|c| c.name.as_str()).collect();
        let test = r
            .checks
            .iter()
            // The package's own `test` script (`npm run test` -> `vitest run`).
            .find(|c| {
                c.argv.first().is_some_and(|a| a == "npm") && c.argv.iter().any(|a| a == "test")
            })
            .unwrap_or_else(|| panic!("no vitest run among {names:?}: {:?}", r.sandbox_error));
        // Vite bundled its config and the workers started. The private .git also lets Studio's
        // inventory and tracked-file tests use git just as they do in a host worktree.
        let all = format!("{}{}", test.stdout_tail, test.stderr_tail);
        assert!(all.contains("Test Files"), "{test:#?}");
        for bad in ["EROFS", "read-only file system", ".vite-temp"] {
            assert!(!all.contains(bad), "{bad}: {test:#?}");
        }
        assert_eq!(test.exit_code, Some(0), "{test:#?}");
    }

    /// PARITY B13 on pods, one install for every case: a passing floor runs with no network in
    /// its copy (a private `.git`, the marker set); a check that rewrites the tree stands for
    /// nothing; an OOM kill is floor infrastructure whatever the exit code; a check's own exit
    /// code survives the exec transport. Run: `WICKED_TEST_CONTAINER_FLOOR=1 WICKED_K8S_ADMIN_CONTEXT=<ctx>
    /// cargo test --lib checks_container::tests::live:: -- --test-threads 1`.
    #[test]
    fn k8s_live_the_checks_floor_runs_on_pods() {
        if !enabled("k8s_live_the_checks_floor_runs_on_pods") {
            return;
        }
        let _one = ONE.lock().unwrap_or_else(|p| p.into_inner());
        let _cluster = cluster();

        // 1. Passing, offline, in the copy, as the checks.
        let (dir, head) = fixture(
            "pass",
            &[("test", "node t.js")],
            &[(
                "t.js",
                "const net=require('net');const fs=require('fs');\
                 if(process.env.WICKED_CHECKS_FLOOR!=='container'||!fs.existsSync('.git')){process.exit(2)}\
                 const cp=require('child_process');\
                 const files=cp.execFileSync('git',['ls-files'],{encoding:'utf8'});\
                 if(!files.split('\\n').includes('t.js')){process.exit(5)}\
                 const base=cp.execFileSync('git',['merge-base','HEAD','origin/main'],{encoding:'utf8'}).trim();\
                 const origin=cp.execFileSync('git',['rev-parse','origin/main'],{encoding:'utf8'}).trim();\
                 if(base!==origin){process.exit(6)}\
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

        // 2. A check that rewrites the tree.
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
            .unwrap_or_else(|| {
                panic!(
                    "lint ran: {:#?} {:?} {:?}",
                    r.checks, r.sandbox_error, r.runner
                )
            });
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

        // 3. The exit code is the check's own, through the exec transport (B13).
        let (dir, head) = fixture("rc", &[("test", "node -e \"process.exit(7)\"")], &[]);
        let r = floor(&dir, &head);
        let t = r
            .checks
            .iter()
            .find(|c| c.name == "test")
            .unwrap_or_else(|| panic!("test ran: {:#?} {:?}", r.checks, r.sandbox_error));
        assert_eq!(t.exit_code, Some(7), "{t:#?}");
        assert!(t.spawn_error.is_none(), "{t:#?}");
        let _ = std::fs::remove_dir_all(&dir);

        // 4. An OOM kill under a check that exits 0.
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
            .unwrap_or_else(|| {
                panic!(
                    "test ran: {:#?} {:?} {:?}",
                    r.checks, r.sandbox_error, r.runner
                )
            });
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
