use super::*;

#[cfg(unix)]
mod dependency_claim_admission {
    use super::*;
    use serde_json::json;

    struct Claims {
        dir: PathBuf,
        place: crate::k8s::floor::Place,
    }

    impl Claims {
        fn new(count: u64) -> Self {
            let id = uuid::Uuid::new_v4();
            let dir = std::env::temp_dir().join(format!("nm-admission-{id}"));
            std::fs::create_dir_all(&dir).unwrap();
            let kubectl = dir.join("kubectl");
            crate::test_env::install_stub(
                &kubectl,
                r#"#!/bin/sh
dir=${0%/*}
while [ "$#" -gt 0 ]; do
    case "$1" in get|delete|apply) break ;; esac
    shift
done
printf '%s\n' "$*" >> "$dir/calls"
case "$1 $2 $3" in
    'get pvc -l') cat "$dir/claims.json" ;;
    'get pods -o')
        cat "$dir/pods.json"
        if [ -f "$dir/pause-pod-list" ]; then
            touch "$dir/pods-listed"
            while [ -f "$dir/pause-pod-list" ]; do sleep 0.01; done
        fi
        ;;
    get\ pvc\ *) if [ -f "$dir/$3.json" ]; then cat "$dir/$3.json"; fi ;;
    delete\ pvc\ *) printf '%s\n' "$3" >> "$dir/deleted" ;;
    apply\ *) cat > "$dir/applied.json" ;;
    *) echo "unexpected kubectl: $*" >&2; exit 1 ;;
esac
"#,
            );
            let mut items = Vec::new();
            for i in 0..count {
                let name = format!("wfloor-nm-{i:02}");
                let claim = json!({"metadata": {
                    "name": name, "labels": {"wicked.dev/owner": "me"}
                }});
                std::fs::write(dir.join(format!("{name}.json")), claim.to_string()).unwrap();
                items.push(claim);
            }
            // Neither another owner's nm claim nor our target claims consume this budget.
            items.push(json!({"metadata": {
                "name": "wfloor-nm-foreign", "labels": {"wicked.dev/owner": "other"}
            }}));
            items.push(json!({"metadata": {
                "name": "wfloor-target", "labels": {"wicked.dev/owner": "me"}
            }}));
            std::fs::write(dir.join("claims.json"), json!({"items": items}).to_string()).unwrap();
            std::fs::write(dir.join("pods.json"), r#"{"items":[]}"#).unwrap();
            Self {
                place: crate::k8s::floor::Place {
                    kube: crate::k8s::Kube {
                        kubectl,
                        kubeconfig: None,
                        context: "test".into(),
                        owner: "me".into(),
                    },
                    ns: format!("nm-{id}"),
                    run: "run".into(),
                    owned: false,
                },
                dir,
            }
        }

        fn mounted(&self, count: u64) {
            let volumes: Vec<_> = (0..count)
                .map(|i| {
                    json!({"persistentVolumeClaim": {
                        "claimName": format!("wfloor-nm-{i:02}")
                    }})
                })
                .collect();
            // Mounts by every live pod count, regardless of its owner label or readiness.
            let pods = json!({"items": [{
                "metadata": {"labels": {"wicked.dev/owner": "other"}},
                "status": {"phase": "Pending"}, "spec": {"volumes": volumes}
            }]});
            std::fs::write(self.dir.join("pods.json"), pods.to_string()).unwrap();
        }

        fn allocate(&self) -> Result<(), String> {
            self.place.claim("wfloor-nm-new", "16Gi")
        }

        fn deleted(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.join("deleted"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    impl Drop for Claims {
        fn drop(&mut self) {
            forget_namespace(&self.place.ns);
            std::fs::remove_dir_all(&self.dir).unwrap();
        }
    }

    #[test]
    fn nm_at_bound_evicts_lru_idle_claim_before_creating_and_forgets_fill() {
        let claims = Claims::new(MAX_DEPENDENCY_CLAIMS);
        for i in 0..MAX_DEPENDENCY_CLAIMS {
            mark_filled(&format!("{}/wfloor-nm-{i:02}", claims.place.ns));
        }
        // A fill hit refreshes claim 00, making 01 the least recently used.
        let filler = Filler {
            place: &claims.place,
            tag: "image",
            floor_image: "floor",
            memory: "6g",
            git_dir: Path::new("unused.git"),
        };
        assert!(matches!(
            filler.fill_nm("tree", &[], "", "wfloor-nm-00", &[]),
            Ok(Fill::Hit)
        ));
        claims.allocate().unwrap();
        assert_eq!(claims.deleted(), ["wfloor-nm-01"]);
        assert!(!filled(&format!("{}/wfloor-nm-01", claims.place.ns)));
        let calls = std::fs::read_to_string(claims.dir.join("calls")).unwrap();
        assert!(calls.contains("wicked.dev/owner=me"), "{calls}");
        assert!(calls.find("delete pvc").unwrap() < calls.find("apply").unwrap());
        let applied = std::fs::read_to_string(claims.dir.join("applied.json")).unwrap();
        assert!(applied.contains("wfloor-nm-new"), "{applied}");
    }

    #[test]
    fn nm_mounted_by_live_pod_is_never_evicted() {
        let claims = Claims::new(MAX_DEPENDENCY_CLAIMS);
        claims.mounted(1);
        claims.allocate().unwrap();
        assert_eq!(claims.deleted(), ["wfloor-nm-01"]);
    }

    #[test]
    fn nm_pending_mount_allows_another_mount_and_allocation_without_eviction() {
        let claims = Claims::new(MAX_DEPENDENCY_CLAIMS);
        let mount = dependency_mounts(&claims.place.kube, &claims.place.ns, ["wfloor-nm-00"])
            .unwrap()
            .unwrap();
        // No pod is visible yet. Both operations must finish while the first guard lives.
        let result = std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel();
            let claims = &claims;
            let worker = scope.spawn(move || {
                let result =
                    dependency_mounts(&claims.place.kube, &claims.place.ns, ["wfloor-nm-00"])
                        .and_then(|second| {
                            drop(second);
                            claims.allocate()
                        });
                tx.send(result).unwrap();
            });
            let result = rx.recv_timeout(Duration::from_secs(2));
            // Release before joining even on timeout, so a lock regression fails cleanly.
            drop(mount);
            worker.join().unwrap();
            result
        });
        result
            .expect("pending mount blocked namespace admission")
            .unwrap();
        assert_eq!(claims.deleted(), ["wfloor-nm-01"]);
    }

    #[test]
    fn nm_pending_mount_counts_last_until_the_final_guard_drops() {
        let claims = Claims::new(MAX_DEPENDENCY_CLAIMS);
        let first = dependency_mounts(&claims.place.kube, &claims.place.ns, ["wfloor-nm-00"])
            .unwrap()
            .unwrap();
        let second = dependency_mounts(&claims.place.kube, &claims.place.ns, ["wfloor-nm-00"])
            .unwrap()
            .unwrap();
        drop(first);
        claims.allocate().unwrap();
        assert_eq!(claims.deleted(), ["wfloor-nm-01"]);
        // A failed pod start drops its guard without ever calling mounted().
        drop(second);
        claims.allocate().unwrap();
        assert_eq!(claims.deleted(), ["wfloor-nm-01", "wfloor-nm-00"]);
    }

    #[test]
    fn nm_mount_completing_during_pod_inventory_is_still_protected() {
        let claims = Claims::new(MAX_DEPENDENCY_CLAIMS);
        let mount = dependency_mounts(&claims.place.kube, &claims.place.ns, ["wfloor-nm-00"])
            .unwrap()
            .unwrap();
        let pause = claims.dir.join("pause-pod-list");
        std::fs::write(&pause, "").unwrap();
        let listed = std::thread::scope(|scope| {
            let worker = scope.spawn(|| claims.allocate());
            let listed = claims.dir.join("pods-listed");
            let deadline = Instant::now() + Duration::from_secs(2);
            while !listed.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let listed = listed.exists();
            // Complete the mount after the API captured an empty pod list, but before
            // eviction receives that stale reply. Release both barriers even on timeout.
            mount.mounted();
            drop(mount);
            std::fs::remove_file(&pause).unwrap();
            worker.join().unwrap().unwrap();
            listed
        });
        assert!(listed, "pending mount blocked the pod inventory");
        assert_eq!(claims.deleted(), ["wfloor-nm-01"]);
    }

    #[test]
    fn nm_all_claims_pending_refuses_until_the_mount_guard_drops() {
        let claims = Claims::new(MAX_DEPENDENCY_CLAIMS);
        let names: Vec<_> = (0..MAX_DEPENDENCY_CLAIMS)
            .map(|i| format!("wfloor-nm-{i:02}"))
            .collect();
        let mount = dependency_mounts(
            &claims.place.kube,
            &claims.place.ns,
            names.iter().map(String::as_str),
        )
        .unwrap()
        .unwrap();
        let error = claims.allocate().unwrap_err();
        assert!(error.starts_with("admission_pending:"), "{error}");
        assert!(claims.deleted().is_empty());
        assert!(!claims.dir.join("applied.json").exists());
        drop(mount);
        claims.allocate().unwrap();
        assert_eq!(claims.deleted(), ["wfloor-nm-00"]);
    }

    #[test]
    fn nm_invalid_mount_claim_leaves_no_pending_counts() {
        let claims = Claims::new(MAX_DEPENDENCY_CLAIMS);
        for metadata in [
            json!({"labels": {"wicked.dev/owner": "other"}}),
            json!({"labels": {"wicked.dev/owner": "me"},
                "deletionTimestamp": "2026-11-21T00:00:00Z"}),
        ] {
            std::fs::write(
                claims.dir.join("wfloor-nm-invalid.json"),
                json!({"metadata": metadata}).to_string(),
            )
            .unwrap();
            let result = dependency_mounts(
                &claims.place.kube,
                &claims.place.ns,
                ["wfloor-nm-00", "wfloor-nm-invalid"],
            );
            let Err(error) = result else {
                panic!("an invalid claim was admitted");
            };
            assert!(error.starts_with("admission_pending:"), "{error}");
        }
        claims.allocate().unwrap();
        assert_eq!(claims.deleted(), ["wfloor-nm-00"]);
    }

    #[test]
    fn nm_in_flight_fill_is_never_evicted() {
        let claims = Claims::new(MAX_DEPENDENCY_CLAIMS);
        let _fill = lock_volume(&format!("{}-wfloor-nm-00", claims.place.ns)).unwrap();
        claims.allocate().unwrap();
        assert_eq!(claims.deleted(), ["wfloor-nm-01"]);
    }

    #[test]
    fn nm_all_claims_in_use_refuses_as_admission_pending_without_creating() {
        let claims = Claims::new(MAX_DEPENDENCY_CLAIMS);
        claims.mounted(MAX_DEPENDENCY_CLAIMS);
        let error = claims.allocate().unwrap_err();
        assert!(error.starts_with("admission_pending:"), "{error}");
        assert!(claims.deleted().is_empty());
        assert!(!claims.dir.join("applied.json").exists());
    }

    #[test]
    fn nm_upgraded_namespace_evicts_to_bound_including_new_allocation() {
        let count = MAX_DEPENDENCY_CLAIMS + 3;
        let claims = Claims::new(count);
        claims.allocate().unwrap();
        assert_eq!(
            claims.deleted(),
            [
                "wfloor-nm-00",
                "wfloor-nm-01",
                "wfloor-nm-02",
                "wfloor-nm-03"
            ]
        );
        assert_eq!(
            count - claims.deleted().len() as u64 + 1,
            MAX_DEPENDENCY_CLAIMS
        );
    }

    #[test]
    fn nm_unknown_last_use_is_older_than_a_recorded_fill() {
        let claims = Claims::new(MAX_DEPENDENCY_CLAIMS);
        for i in 0..MAX_DEPENDENCY_CLAIMS - 1 {
            mark_filled(&format!("{}/wfloor-nm-{i:02}", claims.place.ns));
        }
        claims.allocate().unwrap();
        assert_eq!(claims.deleted(), ["wfloor-nm-31"]);
    }

    #[test]
    fn nm_mount_refreshes_last_use_after_the_pod_finishes() {
        let claims = Claims::new(MAX_DEPENDENCY_CLAIMS);
        for i in 0..MAX_DEPENDENCY_CLAIMS {
            mark_filled(&format!("{}/wfloor-nm-{i:02}", claims.place.ns));
        }
        let mount = dependency_mounts(&claims.place.kube, &claims.place.ns, ["wfloor-nm-00"])
            .unwrap()
            .unwrap();
        mount.mounted();
        drop(mount);
        claims.allocate().unwrap();
        assert_eq!(claims.deleted(), ["wfloor-nm-01"]);
    }

    #[test]
    fn nm_existing_claim_at_bound_needs_no_free_slot() {
        let claims = Claims::new(MAX_DEPENDENCY_CLAIMS);
        claims.mounted(MAX_DEPENDENCY_CLAIMS);
        claims.place.claim("wfloor-nm-00", "16Gi").unwrap();
        assert!(claims.deleted().is_empty());
        assert!(claims.dir.join("applied.json").exists());
    }

    #[test]
    fn nm_upgraded_namespace_reusing_a_claim_evicts_only_the_excess() {
        let claims = Claims::new(MAX_DEPENDENCY_CLAIMS + 3);
        claims.place.claim("wfloor-nm-00", "16Gi").unwrap();
        assert_eq!(
            claims.deleted(),
            ["wfloor-nm-01", "wfloor-nm-02", "wfloor-nm-03"]
        );
    }
}

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
            id,
            &entries,
            &vec![0; entries.len()],
            files,
            &mut archive,
        )
        .unwrap();
        assert_eq!(held.len(), entries.len());
        assert!(held.iter().all(|(p, _)| !p.starts_with(".git/")));
        extract_archive(f, name, &archive)
    }

    fn extract_archive(f: &Fixture, name: &str, archive: &[u8]) -> PathBuf {
        let out = f.root.join(name);
        std::fs::create_dir(&out).unwrap();
        // spawn-audit: test-only — extracts the exact floor copy-in stream into a private dir.
        let mut c = Command::new("tar");
        c.hardened().args(["-x", "-f", "-", "-C"]).arg(&out);
        let result = run_fed(c, PROBE, |w| {
            w.write_all(archive).map_err(|e| e.to_string())
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
    fn the_pod_copy_in_matches_a_checkout_byte_for_byte() {
        use sha2::{Digest, Sha256};

        if !crate::repo_checks::git_has_attr_source() {
            eprintln!("SKIP: this git has no --attr-source (< 2.40); the floor writes raw blobs");
            return;
        }
        let root = std::env::temp_dir().join(format!(
            "wicked-floor-eol-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        // Own the directory before any assertion, so failures also clean up the fixture.
        let mut f = Fixture {
            git_dir: repo.join(".git"),
            root,
            repo,
            base: String::new(),
            head_tree: String::new(),
        };
        git(&f.repo, &["init", "-q", "-b", "main", "--template="]);
        git(&f.repo, &["config", "core.autocrlf", "false"]);
        std::fs::write(
            f.repo.join(".gitattributes"),
            "*.cmd text eol=crlf filter=evil\n*.bin binary\n",
        )
        .unwrap();
        std::fs::write(f.repo.join("run.cmd"), b"echo ok\n").unwrap();
        std::fs::create_dir(f.repo.join("sub")).unwrap();
        std::fs::write(f.repo.join("sub/b.cmd"), b"a\nb\n").unwrap();
        std::fs::write(f.repo.join("plain.txt"), b"keep\n").unwrap();
        let binary = b"\x00\x01\r\nraw\n\xff";
        std::fs::write(f.repo.join("data.bin"), binary).unwrap();
        std::os::unix::fs::symlink("run.cmd", f.repo.join("link.cmd")).unwrap();
        git(&f.repo, &["add", "-A"]);
        git(&f.repo, &["commit", "-q", "-m", "checkout conversion"]);
        f.base = git(&f.repo, &["rev-parse", "HEAD"]);
        f.head_tree = git(&f.repo, &["rev-parse", "HEAD^{tree}"]);
        assert_eq!(
            read_blob(&f.git_dir, &f.base, "run.cmd").unwrap(),
            b"echo ok\n"
        );
        assert_eq!(read_blob(&f.git_dir, &f.base, "data.bin").unwrap(), binary);

        // Build the reference before configuring the driver: an unconfigured filter is ignored.
        let checkout = f.root.join("checkout");
        git(
            &f.repo,
            &[
                "worktree",
                "add",
                "--detach",
                checkout.to_str().unwrap(),
                &f.base,
            ],
        );
        assert_eq!(
            std::fs::read(checkout.join("run.cmd")).unwrap(),
            b"echo ok\r\n"
        );
        assert_eq!(
            std::fs::read(checkout.join("sub/b.cmd")).unwrap(),
            b"a\r\nb\r\n"
        );
        assert_eq!(
            std::fs::read(checkout.join("plain.txt")).unwrap(),
            b"keep\n"
        );
        assert_eq!(std::fs::read(checkout.join("data.bin")).unwrap(), binary);

        let marker = f.root.join("evil-filter-ran");
        let smudge = format!(
            "touch '{}'; exit 1",
            marker.to_str().unwrap().replace('\'', "'\\''")
        );
        git(&f.repo, &["config", "filter.evil.smudge", &smudge]);
        git(&f.repo, &["config", "filter.evil.required", "true"]);

        // Neither writer may consult the source worktree's uncommitted attributes.
        std::fs::write(f.repo.join(".gitattributes"), "*.cmd -text\n").unwrap();
        for (side, id) in [("commit", &f.base), ("tree", &f.head_tree)] {
            let entries = ls_tree(&f.git_dir, id).unwrap();
            let times = vec![0; entries.len()];
            for fill in [false, true] {
                let mut archive = Vec::new();
                let held = if fill {
                    write_tree_tar(&f.git_dir, id, &entries, &times, &mut archive)
                } else {
                    write_side_tar(&f.git_dir, id, &entries, &times, None, &mut archive)
                }
                .unwrap();
                assert!(!marker.exists(), "a repository filter driver ran");
                let out = extract_archive(&f, &format!("{side}-{fill}"), &archive);
                assert_eq!(held.len(), entries.len());
                for ((path, expect), entry) in held.iter().zip(&entries) {
                    assert_eq!(path, &entry.path, "hold order must match ls-tree");
                    if entry.is_link() {
                        assert_eq!(
                            std::fs::read_link(out.join(path)).unwrap(),
                            Path::new("run.cmd")
                        );
                        assert_eq!(expect, &Expect::Link("run.cmd".into()));
                    } else {
                        let expected = std::fs::read(checkout.join(path)).unwrap();
                        assert_eq!(std::fs::read(out.join(path)).unwrap(), expected, "{path}");
                        let digest: String = Sha256::digest(&expected)
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect();
                        assert_eq!(expect, &Expect::File(digest, entry.mode & 0o111 != 0));
                    }
                }
            }
        }
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
            assert!(write_side_tar(
                &f.git_dir,
                &f.head_tree,
                &entries,
                &[0],
                None,
                &mut Vec::new(),
            )
            .is_err());
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
            let held = write_side_tar(
                &f.git_dir,
                &f.head_tree,
                &entries,
                &[0],
                None,
                &mut Vec::new(),
            )
            .unwrap();
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

#[test]
fn the_floor_path_has_no_rustup_proxies_and_the_image_links_the_pinned_toolchain() {
    assert!(!PATH_IN.contains("/usr/local/cargo/bin"), "{PATH_IN}");
    assert!(PATH_IN.split(':').any(|d| d == "/usr/local/bin"));
    assert!(DOCKERFILE.contains("ln -sf \"$f\" /usr/local/bin/"));
    assert!(!DOCKERFILE
        .lines()
        .any(|l| l.starts_with("ENV PATH=") && l.contains("/usr/local/cargo/bin")));
    assert!(PLATFORM_SCOPE.contains(
        "built with the floor's pinned Rust toolchain; a repository rust-toolchain file is not applied"
    ));
}

/// core#1406: a tree that declares no native addon keeps sharing the floor's own claim; one that
/// declares some gets the seat's own claim beside the floor's (filled as the floor's, then built
/// in place), so nothing the repository's build writes reaches the floor's shared claim.
#[test]
fn a_seat_claim_with_native_addons_is_its_own_beside_the_floors() {
    let (tag, lock, inputs) = (
        "wicked-checks-floor:abc",
        b"lock".as_slice(),
        b"in".as_slice(),
    );
    let plain = seat_nm_volume(tag, lock, inputs, &[]);
    assert_eq!(plain, nm_volume(tag, "", lock, inputs, &[]));
    let native = seat_nm_volume(tag, lock, inputs, &["better-sqlite3"]);
    let floor = nm_volume(tag, "", lock, inputs, &["better-sqlite3"]);
    assert_eq!(native, format!("{floor}-seat"));
    assert_ne!(native, plain);
    assert!(native.starts_with(DEPENDENCY_PREFIX), "{native}");
    assert!(
        format!("build-{native}").len() <= 63,
        "a pod name of it fits: build-{native}"
    );
}

/// core#1406: the seat claim's offline build is the floor's own native-build step, verbatim, run
/// under the floor's landrun with ONLY each addon's `build/`, the scratch and the home writable
/// (the claim itself read-only to it, and no best effort: a kernel without Landlock builds
/// nothing). The engine empties each `build/` before (a shipped binding is never reused) and after
/// a failed build, and marks the claim only after success; a marked claim is a hit.
#[cfg(unix)]
#[test]
fn the_seat_claims_native_build_is_the_floors_step_confined_and_marked_only_on_success() {
    let script = native_build_script(&["better-sqlite3"]);
    let argv = crate::repo_checks::native_build_argv(&["better-sqlite3"]);
    assert!(
        script.contains(&format!("-- /bin/sh -c {}; then", sh_quote(&argv[2]))),
        "{script}"
    );
    let policy = script
        .split("if /floor/landrun ")
        .nth(1)
        .and_then(|r| r.split(" -- /bin/sh").next())
        .unwrap_or_else(|| panic!("{script}"));
    // The launcher takes the ABI the kernel has, so the jail is proven in force first, under the
    // same policy, against a canary the engine wrote: a scratch write succeeds, and a write-open
    // and a truncate of the canary are refused (EACCES) and leave it unchanged.
    assert!(
        policy.starts_with("--best-effort --ignore-missing "),
        "{policy}"
    );
    let canary = format!("{FILL_DIR}/{JAIL_PROBE}");
    let prove = format!(
        "printf x > {canary} || {{ echo 'wicked-core: the claim is not writable; nothing is built' >&2; \
         exit {NATIVE_REFUSED}; }}; jail=0; /floor/landrun {policy} -- /bin/sh -c {} || jail=$?; \
         if [ \"$jail\" = 5 ]; then",
        sh_quote(&format!(
            "touch /tmp/wicked-native/.jail-granted && python3 -I -c {} {canary}",
            sh_quote(JAIL_PROOF)
        ))
    );
    assert!(
        script.find(&prove).unwrap_or_else(|| panic!("{script}"))
            < script.find("if /floor/landrun ").unwrap()
    );
    assert!(JAIL_PROOF.contains("os.truncate(p, 0)") && JAIL_PROOF.contains("errno.EACCES"));
    assert!(policy.contains(&format!("--rox {FILL_DIR} ")), "{policy}");
    assert!(policy.contains(&format!("--rwx '{FILL_DIR}/better-sqlite3/build'")));
    let writable: Vec<&str> = policy
        .split(" --")
        .filter_map(|g| g.strip_prefix("rwx ").or_else(|| g.strip_prefix("rw ")))
        .collect();
    assert_eq!(
        writable,
        [
            "/dev/null",
            "/tmp",
            HOME,
            format!("'{FILL_DIR}/better-sqlite3/build'").as_str()
        ],
        "{policy}"
    );
    // The clean comes before the build, under the engine (outside landrun).
    let clean = format!(
        "rm -rf '{FILL_DIR}/better-sqlite3/build' && mkdir '{FILL_DIR}/better-sqlite3/build';"
    );
    assert!(script.find(&clean).unwrap() < script.find("/floor/landrun").unwrap());

    // Run it for real against a stand-in claim: FILL_DIR and /tmp are re-pointed into a scratch
    // dir, landrun is a stand-in that runs the command, and `node`/`npm` are stubs that "build"
    // when asked (or leave a partial binding and fail).
    let root = std::env::temp_dir().join(format!("native-claim-{}", uuid::Uuid::new_v4()));
    let claim = root.join("vol");
    let bin = root.join("bin");
    let gyp = root.join("lib/node_modules/npm/node_modules/node-gyp/bin/node-gyp.js");
    std::fs::create_dir_all(claim.join("better-sqlite3/build/Release")).unwrap();
    std::fs::write(claim.join("better-sqlite3/build/Release/shipped.node"), "").unwrap();
    std::fs::write(claim.join("better-sqlite3/package.json"), "{}").unwrap();
    std::fs::create_dir_all(gyp.parent().unwrap()).unwrap();
    std::fs::write(&gyp, "").unwrap();
    std::fs::create_dir_all(root.join("lib/node_modules/npm/bin")).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let npm = root.join("lib/node_modules/npm/bin/npm-cli.js");
    crate::test_env::install_stub(&npm, "#!/bin/sh\nexit 0\n");
    std::os::unix::fs::symlink(&npm, bin.join("npm")).unwrap();
    // With JAIL_ABI2 set, the jail proof answers as on a Landlock ABI v1/v2 kernel (core#1886):
    // the open refused, the truncate allowed.
    crate::test_env::install_stub(
        &bin.join("python3"),
        "#!/bin/sh\n[ -z \"$JAIL_ABI2\" ] || exit 5\nPATH=/usr/bin:/bin exec python3 \"$@\"\n",
    );
    crate::test_env::install_stub(
        &bin.join("node"),
        "#!/bin/sh\nif [ -n \"$GYP_FAILS\" ]; then mkdir -p build/Release && : > build/Release/partial.node; exit 7; fi\nif [ \"$2\" = build ]; then mkdir -p build/Release && : > build/Release/x.node; fi\nexit 0\n",
    );
    // The stand-in launcher "enforces" by making the claim's top and the canary unwritable while
    // its command runs (each addon's build/ stays writable). With JAIL_OPEN set it confines
    // nothing, as a kernel without Landlock under `--best-effort` would; with CANARY_OPEN only the
    // top is closed (new files refused, an existing one still truncatable), as an ABI that does
    // not cover truncation would leave it.
    let landrun = root.join("landrun");
    crate::test_env::install_stub(
        &landrun,
        "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\n\
         if [ -n \"$JAIL_OPEN\" ]; then exec \"$@\"; fi\n\
         c=\"$CLAIM/.wicked-jail-probe\"; [ -n \"$CANARY_OPEN\" ] || ! [ -e \"$c\" ] || chmod a-w \"$c\"\n\
         chmod a-w \"$CLAIM\"; \"$@\"; rc=$?; chmod u+w \"$CLAIM\"\n\
         ! [ -e \"$c\" ] || chmod u+w \"$c\"; exit $rc\n",
    );
    let local = script
        .replace(FILL_DIR, &claim.display().to_string())
        .replace("/tmp/wicked-native", &root.join("t").display().to_string())
        .replace("/floor/landrun", &landrun.display().to_string());
    let run_code = |env: &[(&str, &str)]| {
        // spawn-audit: test-only — runs the engine's build script against a stand-in claim, with
        // the environment cleared and only the stand-ins' variables set.
        let mut c = std::process::Command::new("sh");
        c.hardened()
            .args(["-c", &local])
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("CLAIM", &claim);
        for (k, v) in env {
            c.env(k, v);
        }
        let o = c.output().unwrap();
        (
            o.status.code(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        )
    };
    let run = |env: &[(&str, &str)]| {
        let (code, out) = run_code(env);
        (code == Some(0), out)
    };
    let build = claim.join("better-sqlite3/build");
    // A jail not in force: nothing is built (the stub node never runs), no marker, no probe left.
    let refused = |env: &[(&str, &str)], why: &str| {
        // A refusal no retry within the run changes has its own exit code (core#1887).
        let (code, out) = run_code(env);
        assert!(
            code == Some(NATIVE_REFUSED) && out.contains(why),
            "{env:?}: {code:?} {out}"
        );
        assert!(!claim.join(NATIVE_BUILT).exists(), "{env:?}");
        assert!(
            !claim.join(JAIL_PROBE).exists(),
            "{env:?}: the canary is left"
        );
        assert_eq!(
            std::fs::read_dir(&build).unwrap().count(),
            0,
            "{env:?}: {out}"
        );
    };
    refused(&[("JAIL_OPEN", "1")], "Landlock is not in force");
    // An existing file at the claim's top still truncatable under the jail: refused.
    refused(&[("CANARY_OPEN", "1")], "Landlock is not in force");
    // A kernel whose Landlock cannot deny truncation says so (core#1886).
    refused(
        &[("JAIL_ABI2", "1")],
        "Landlock on this kernel is too old to confine truncation (it needs ABI v3, Linux 6.2 or newer)",
    );
    // A claim the engine cannot write (full, read-only): the confined refusal would prove nothing.
    let mut ro = std::fs::metadata(&claim).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut ro, 0o555);
    std::fs::set_permissions(&claim, ro).unwrap();
    refused(&[("JAIL_OPEN", "1")], "the claim is not writable");
    let mut rw = std::fs::metadata(&claim).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut rw, 0o755);
    std::fs::set_permissions(&claim, rw).unwrap();
    // A failed build is no refusal: a later unit may build again.
    let (code, _) = run_code(&[("GYP_FAILS", "1")]);
    assert_eq!(code, Some(1));
    assert!(
        !claim.join(NATIVE_BUILT).exists(),
        "a failed build leaves no marker"
    );
    assert_eq!(
        std::fs::read_dir(&build).unwrap().count(),
        0,
        "nor any output"
    );
    let (ok, out) = run(&[]);
    assert!(ok, "{out}");
    assert!(build.join("Release/x.node").is_file());
    assert!(!build.join("Release/shipped.node").exists());
    assert!(claim.join(NATIVE_BUILT).is_file());
    let (ok, out) = run(&[("GYP_FAILS", "1")]);
    assert!(ok && out.contains("WICKED-HIT"), "{out}");
    let _ = std::fs::remove_dir_all(&root);
}

/// core#1406: the confined half of the jail proof passes only when a write-open AND a
/// `truncate(2)` of the canary each fail with `EACCES`. Landlock ABI v1/v2 deny the open but not
/// the truncate, and another error (a full claim) is no denial; both must fail the proof. The
/// denials are injected into the proof's own code (an interpreter prelude) so each is exercised
/// on its own, which a filesystem stand-in cannot do.
#[cfg(unix)]
#[test]
fn the_jail_proof_needs_both_the_open_and_the_truncate_refused_with_eacces() {
    let root = std::env::temp_dir().join(format!("jail-proof-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let canary = root.join(JAIL_PROBE);
    let proof = |open: &str, truncate: &str| -> (i32, u64) {
        std::fs::write(&canary, "x").unwrap();
        let prelude = format!(
            "import builtins, errno, os\n\
             real_open, real_truncate = builtins.open, os.truncate\n\
             def fail(e):\n    raise OSError(e, os.strerror(e))\n\
             builtins.open = lambda *a, **k: fail({open}) if {open} else real_open(*a, **k)\n\
             os.truncate = lambda p, n: fail({truncate}) if {truncate} else real_truncate(p, n)\n\
             exec(compile({:?}, \"proof\", \"exec\"))\n",
            JAIL_PROOF
        );
        // spawn-audit: test-only — runs the jail proof's own code with injected denials.
        let o = std::process::Command::new("python3")
            .hardened()
            .args(["-I", "-c", &prelude])
            .arg(&canary)
            .output()
            .unwrap();
        (
            o.status.code().unwrap_or(-1),
            std::fs::metadata(&canary).unwrap().len(),
        )
    };
    let (eacces, enospc) = ("errno.EACCES", "errno.ENOSPC");
    assert_eq!(
        proof(eacces, eacces),
        (0, 1),
        "both refused by Landlock: proven"
    );
    // ABI v1/v2: the open refused, the truncate allowed (it ran: the canary is empty). Its own
    // code (core#1886), so the script says the kernel's Landlock is too old, not absent.
    assert_eq!(proof(eacces, "0"), (5, 0));
    assert_eq!(proof("0", eacces).0, 4, "a writable canary");
    assert_eq!(proof(enospc, eacces).0, 4, "a full claim is no denial");
    assert_eq!(proof(eacces, enospc).0, 4);
    let _ = std::fs::remove_dir_all(&root);
}

/// core#1886, core#1887: the note and the refusal read from the build pod's `kubectl exec`
/// stderr, shaped as kubectl writes it: the script's stderr, then kubectl's own last line.
#[test]
fn a_native_build_note_is_the_scripts_reason_and_only_its_refusals_are_kept() {
    let exec =
        |stderr: &str, code: i32| format!("{stderr}\ncommand terminated with exit code {code}");
    let jail = exec(
        "wicked-core: Landlock is not in force in the build pod; nothing is built",
        NATIVE_REFUSED,
    );
    assert_eq!(
        native_build_note(&jail),
        "Landlock is not in force in the build pod; nothing is built"
    );
    assert!(native_build_refused(&jail));
    // node-gyp's own failure: its last line, never kubectl's; and a build may be tried again.
    let gyp = exec(
        "gyp ERR! build error\ngyp ERR! stack Error: `make` failed with exit code: 2\n",
        1,
    );
    assert_eq!(
        native_build_note(&gyp),
        "gyp ERR! stack Error: `make` failed with exit code: 2"
    );
    assert!(!native_build_refused(&gyp));
    // The script's last `wicked-core:` line wins over output after it.
    let built = exec(
        "wicked-core: no node-gyp beside npm (/usr/bin/npm)\nls: cannot access 'x': No such file",
        1,
    );
    assert_eq!(
        native_build_note(&built),
        "no node-gyp beside npm (/usr/bin/npm)"
    );
    // Only kubectl's line: it is all there is to say.
    assert_eq!(
        native_build_note("command terminated with exit code 137"),
        "command terminated with exit code 137"
    );
    // A refusal code in the build's own output is not kubectl's line.
    let spoof = exec(
        &format!("command terminated with exit code {NATIVE_REFUSED}"),
        1,
    );
    assert!(!native_build_refused(&spoof));
    assert!(!native_build_refused("kubectl timed out"));
}

/// core#1886, core#1887: a seat claim's native build through `build_native`, against a stand-in
/// cluster whose `kubectl exec` runs the engine's own build script (its paths re-pointed into a
/// scratch dir, a stand-in launcher, and a `python3` that answers the jail proof as a Landlock ABI
/// v1/v2 kernel does: the open refused, the truncate allowed). kubectl's stderr is then the
/// script's own, ended by kubectl's `command terminated with exit code N` line, as a real
/// `kubectl exec` writes it (src/k8s/seat_tests.rs pins that line for a write-back error).
#[cfg(unix)]
mod native_build_refusals {
    use super::*;

    struct RuntimeChoice(Option<std::ffi::OsString>);

    impl Drop for RuntimeChoice {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var(crate::container_floor::RUNTIME_ENV, v),
                None => std::env::remove_var(crate::container_floor::RUNTIME_ENV),
            }
        }
    }

    const KUBECTL: &str = r#"#!/usr/bin/env python3
import json, pathlib, subprocess, sys
base = pathlib.Path(__file__).parent
args = sys.argv[1:]
args = args[next(i for i, v in enumerate(args) if v in ('apply', 'get', 'exec', 'delete')):]
with (base / 'calls').open('a') as f: f.write(json.dumps(args) + '\n')
statefile = base / 'state.json'
state = json.loads(statefile.read_text()) if statefile.exists() else {}
def save(): statefile.write_text(json.dumps(state))
if args[0] == 'apply':
    body = json.load(sys.stdin)
    if (base / 'refuse-apply').exists():
        sys.stderr.write('Error from server (Forbidden): exceeded quota\n')
        sys.exit(1)
    for d in body['items']:
        d['status'] = {'phase': 'Running', 'hostIP': '10.0.0.2',
            'conditions': [{'type': 'Ready', 'status': 'True'}],
            'containerStatuses': [{'name': 'floor', 'ready': True}]}
        state[d['metadata']['name']] = d
    save()
elif args[0] == 'get':
    if args[1] == 'nodes':
        print(json.dumps({'items': [{'metadata': {'name': 'n', 'uid': 'n-uid'}, 'spec': {},
            'status': {'nodeInfo': {'containerRuntimeVersion': 'docker://27.0.0'}}}]}))
    elif args[1] == 'pvc':
        print(json.dumps({'metadata': {'name': args[2], 'labels': {'wicked.dev/owner': 'me'}}}))
    elif args[1] == 'pod' and args[2] in state:
        print(json.dumps(state[args[2]]))
elif args[0] == 'delete':
    state.pop(args[2], None)
    save()
elif args[0] == 'exec':
    script = args[-1]
    if 'nc_()' in script:
        print('net=closed node=closed')
        sys.exit(0)
    with (base / 'builds').open('a') as f: f.write('build\n')
    for k, v in json.loads((base / 'paths.json').read_text()).items():
        script = script.replace(k, v)
    p = subprocess.run(['/bin/sh', '-c', script], capture_output=True,
        env={'PATH': str(base / 'bin') + ':/usr/bin:/bin'})
    sys.stdout.buffer.write(p.stdout)
    sys.stderr.buffer.write(p.stderr)
    if p.returncode:
        sys.stderr.write('command terminated with exit code %d\n' % p.returncode)
        sys.exit(p.returncode)
else:
    sys.stderr.write('unexpected kubectl: %s\n' % args)
    sys.exit(1)
"#;

    fn lines(path: &Path) -> usize {
        std::fs::read_to_string(path)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    #[test]
    fn a_build_the_kernel_refuses_says_why_and_is_not_retried_by_the_next_unit() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("native-refused-{}", uuid::Uuid::new_v4()));
        let claim = dir.join("vol");
        std::fs::create_dir_all(claim.join("better-sqlite3")).unwrap();
        std::fs::write(claim.join("better-sqlite3/package.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::create_dir_all(dir.join("t")).unwrap();
        // A docker-runtime node shares the builder's image store: no load runs, and the stand-in
        // runtime only answers `version`.
        let runtime = RuntimeChoice(std::env::var_os(crate::container_floor::RUNTIME_ENV));
        let docker = dir.join("docker");
        crate::test_env::install_stub(
            &docker,
            "#!/bin/sh\ncase \"$1\" in version) echo 27;; *) exit 98;; esac\n",
        );
        std::env::set_var(crate::container_floor::RUNTIME_ENV, &docker);
        let landrun = dir.join("landrun");
        crate::test_env::install_stub(
            &landrun,
            "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n",
        );
        // Landlock ABI v1/v2: the write-open of the canary is refused, its truncate is not.
        crate::test_env::install_stub(&dir.join("bin/python3"), "#!/bin/sh\nexit 5\n");
        let paths = std::collections::BTreeMap::from([
            (FILL_DIR.to_string(), claim.display().to_string()),
            (
                "/tmp/wicked-native".into(),
                dir.join("t").display().to_string(),
            ),
            ("/floor/landrun".into(), landrun.display().to_string()),
        ]);
        std::fs::write(
            dir.join("paths.json"),
            serde_json::to_string(&paths).unwrap(),
        )
        .unwrap();
        let kubectl = dir.join("kubectl");
        crate::test_env::install_stub(&kubectl, KUBECTL);
        let place = crate::k8s::floor::Place {
            kube: crate::k8s::Kube {
                kubectl,
                kubeconfig: None,
                context: "test".into(),
                owner: "me".into(),
            },
            ns: format!("nb-{}", uuid::Uuid::new_v4()),
            run: "run".into(),
            owned: false,
        };
        let filler = Filler {
            place: &place,
            tag: "image",
            floor_image: "floor",
            memory: "6g",
            git_dir: Path::new("unused.git"),
        };
        let volume = seat_nm_volume("image", b"{}", b"", &["better-sqlite3"]);
        let (calls, builds) = (dir.join("calls"), dir.join("builds"));

        // A pod the cluster does not admit is transient: the next unit tries again.
        std::fs::write(dir.join("refuse-apply"), "").unwrap();
        let first = filler.build_native(&volume, &["better-sqlite3"]);
        assert!(
            first.as_ref().is_err_and(|e| e.contains("not admitted")),
            "{first:?}"
        );
        std::fs::remove_file(dir.join("refuse-apply")).unwrap();

        // core#1886: the note is the script's own reason, never kubectl's exit line; and on a
        // kernel whose Landlock cannot deny truncation it says so (not "not in force").
        let refused = filler
            .build_native(&volume, &["better-sqlite3"])
            .expect_err("the jail proof fails on this kernel");
        assert_eq!(lines(&builds), 1, "the retry reached the build");
        assert!(
            refused.contains("Landlock on this kernel is too old to confine truncation")
                && refused.contains("ABI v3, Linux 6.2 or newer"),
            "{refused}"
        );
        assert!(!refused.contains("command terminated"), "{refused}");
        assert!(!refused.contains("not in force"), "{refused}");
        assert!(
            place
                .kube
                .get_json(Some(&place.ns), "pod", &format!("build-{volume}"))
                .unwrap()
                .is_none(),
            "the build pod is gone"
        );

        // core#1887: the jail proof cannot pass in this run, so a later unit starts no pod (no
        // kubectl call at all) and gets the same note.
        let before = lines(&calls);
        let again = filler.build_native(&volume, &["better-sqlite3"]);
        assert_eq!(again.as_ref().err(), Some(&refused), "{again:?}");
        assert_eq!(lines(&calls), before, "no build pod, no kubectl call");
        assert_eq!(lines(&builds), 1);

        forget_namespace(&place.ns);
        drop(runtime);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// core#1406: the native addons a seat's claim is built for come from the snapshot tree's own
/// `.wicked/checks.json` blob (never the worktree's file), validated as the floor validates them.
#[cfg(unix)]
#[test]
fn a_seat_reads_its_declared_native_addons_from_the_snapshot_tree() {
    let root = std::env::temp_dir().join(format!("native-decl-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join(".wicked")).unwrap();
    let git = |args: &[&str]| -> String {
        // spawn-audit: test-only — builds the fixture repository.
        let o = std::process::Command::new("git")
            .hardened()
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8(o.stdout).unwrap().trim().to_string()
    };
    git(&["init", "-q"]);
    std::fs::write(root.join("README.md"), "x\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "none"]);
    let gd = root.join(".git");
    let none = git(&["rev-parse", "HEAD^{tree}"]);
    assert_eq!(declared_native_in(&gd, &none).unwrap(), Vec::<&str>::new());
    std::fs::write(
        root.join(crate::repo_checks::CONFIG_PATH),
        r#"{"native_rebuild":["better-sqlite3"]}"#,
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "declares"]);
    let declares = git(&["rev-parse", "HEAD^{tree}"]);
    assert_eq!(
        declared_native_in(&gd, &declares).unwrap(),
        ["better-sqlite3"]
    );
    // The worktree's own edit is not the snapshot's.
    std::fs::write(
        root.join(crate::repo_checks::CONFIG_PATH),
        r#"{"native_rebuild":["leftpad"]}"#,
    )
    .unwrap();
    assert_eq!(
        declared_native_in(&gd, &declares).unwrap(),
        ["better-sqlite3"]
    );
    git(&["add", "-A"]);
    git(&["commit", "-qm", "bad"]);
    let bad = git(&["rev-parse", "HEAD^{tree}"]);
    assert!(declared_native_in(&gd, &bad)
        .unwrap_err()
        .contains("leftpad"));
    let _ = std::fs::remove_dir_all(&root);
}
