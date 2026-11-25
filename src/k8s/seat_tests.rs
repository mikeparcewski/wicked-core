//! B-2 tests: the replica sync codec, the checked write-back, the replica plan, the unit env,
//! exit-code honesty, the bound-pod gate, the read-only view, and the gateway writer scripts.
//! Each names its PARITY row.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use wicked_apps_core::HardenedCommand as _;

use super::gw;
use super::render::{self, ReplicaMount, SeatPod};
use super::seat::{self, Ending, PlanMount, PlanRoot, PodState};
use super::sync::{self, Entry, Kind, Mode, Root};
use crate::worker_sandbox::FloorUnarmed;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "wicked-k8s-{tag}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn entry(kind: Kind, path: &str, data: &[u8]) -> Entry {
    Entry {
        kind,
        path: path.into(),
        data: data.to_vec(),
    }
}

fn root(entries: Vec<Entry>) -> Root {
    Root {
        replica: "home/u/wt".into(),
        mode: Mode::Tree,
        entries,
    }
}

// ── B11: the stream ──────────────────────────────────────────────────────────────────────────

#[test]
fn k8s_the_sync_stream_round_trips_and_refuses_a_truncated_or_oversized_one() {
    let roots = vec![
        root(vec![
            entry(Kind::File, "a.txt", b"hello"),
            entry(Kind::Exec, "bin/run", b"#!/bin/sh\n"),
            entry(Kind::Link, "l", b"a.txt"),
        ]),
        Root {
            replica: "home/u/notes".into(),
            mode: Mode::Plain,
            entries: vec![entry(Kind::File, "n.md", b"")],
        },
    ];
    let bytes = sync::encode(&roots);
    assert_eq!(
        sync::decode(&mut bytes.as_slice(), sync::MAX_STREAM).unwrap(),
        roots
    );
    // Any cut before the end marker refuses the whole stream (never applied in part).
    for cut in [5, 20, bytes.len() - 1] {
        assert!(
            sync::decode(&mut &bytes[..cut], sync::MAX_STREAM).is_err(),
            "cut {cut}"
        );
    }
    assert!(sync::decode(&mut bytes.as_slice(), 10)
        .unwrap_err()
        .contains("larger"));
    let mut bad = bytes.clone();
    bad[0] = b'X';
    assert!(sync::decode(&mut bad.as_slice(), sync::MAX_STREAM).is_err());
}

// ── B11: the write-back's checks ─────────────────────────────────────────────────────────────

#[test]
fn k8s_write_back_refuses_escaping_paths_links_and_git_dir_entries() {
    let refused = [
        entry(Kind::File, "/etc/passwd", b"x"),
        entry(Kind::File, "../out", b"x"),
        entry(Kind::File, "a/../../out", b"x"),
        entry(Kind::File, ".git/hooks/pre-commit", b"x"),
        entry(Kind::File, "sub/.GIT/config", b"x"),
        entry(Kind::File, ".g\u{200c}it/config", b"x"),
        entry(Kind::File, "GIT~1/config", b"x"),
        entry(Kind::Other, "fifo", b""),
        entry(Kind::Link, "l", b"/etc/passwd"),
        entry(Kind::Link, "d/l", b"../../outside"),
        entry(Kind::File, "a//b", b"x"),
    ];
    for e in refused {
        let r = sync::check_entries(&root(vec![e.clone()]), &[]);
        assert!(r.is_err(), "{:?} was admitted", e.path);
    }
    // A file and a directory at one path, or two spellings of one path on a case-folding host.
    assert!(sync::check_entries(
        &root(vec![
            entry(Kind::File, "a", b"1"),
            entry(Kind::File, "a/b", b"2")
        ]),
        &[]
    )
    .is_err());
    assert!(sync::check_entries(
        &root(vec![
            entry(Kind::File, "Readme", b"1"),
            entry(Kind::File, "README", b"2")
        ]),
        &[]
    )
    .is_err());
    // Links that stay inside, and masked paths dropped silently (the seat never saw them).
    let ok = sync::check_entries(
        &root(vec![
            entry(Kind::Link, "d/l", b"../a.txt"),
            entry(Kind::File, "a.txt", b"x"),
            entry(Kind::File, ".env", b"planted"),
            entry(Kind::File, "secrets/k", b"planted"),
        ]),
        &[".env".into(), "secrets".into()],
    )
    .unwrap();
    let paths: Vec<&str> = ok.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, vec!["d/l", "a.txt"]);
}

#[test]
fn k8s_write_back_deletions_never_touch_a_mask_or_git() {
    let listed: Vec<String> = ["a", "b", ".env", "secrets/k", ".git/x"]
        .map(String::from)
        .to_vec();
    let entries = vec![entry(Kind::File, "a", b"")];
    let d = sync::deletions(&listed, &entries, &[".env".into(), "secrets".into()]);
    assert_eq!(d, vec!["b".to_string()]);
}

#[cfg(unix)]
#[test]
fn k8s_write_back_applies_without_following_a_planted_link() {
    let host = tmp("apply");
    let outside = tmp("outside");
    std::fs::create_dir_all(host.join("src")).unwrap();
    std::fs::write(host.join("src/keep.rs"), "same").unwrap();
    std::fs::write(host.join("gone.txt"), "old").unwrap();
    // A directory on the host the seat turned into a link to a path outside (planted between
    // the seat's view and the host's: the walk must replace it, never follow it).
    std::os::unix::fs::symlink(&outside, host.join("out")).unwrap();
    let entries = vec![
        entry(Kind::File, "src/keep.rs", b"same"),
        entry(Kind::Exec, "bin/run.sh", b"#!/bin/sh\n"),
        entry(Kind::File, "out/escape.txt", b"x"),
        entry(Kind::Link, "link", b"src/keep.rs"),
    ];
    let before = std::fs::metadata(host.join("src/keep.rs"))
        .unwrap()
        .modified()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let done = sync::apply(&host, &entries, &["gone.txt".into()], &[]).unwrap();
    assert_eq!((done.written, done.unchanged, done.deleted), (3, 1, 1));
    assert!(!host.join("gone.txt").exists());
    assert_eq!(
        std::fs::metadata(host.join("src/keep.rs"))
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
    assert!(
        std::fs::read_dir(&outside).unwrap().next().is_none(),
        "the walk followed the link"
    );
    assert!(std::fs::symlink_metadata(host.join("out"))
        .unwrap()
        .is_dir());
    assert_eq!(std::fs::read(host.join("out/escape.txt")).unwrap(), b"x");
    use std::os::unix::fs::PermissionsExt;
    assert_ne!(
        std::fs::metadata(host.join("bin/run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert_eq!(
        std::fs::read_link(host.join("link")).unwrap(),
        Path::new("src/keep.rs")
    );
    // A delete through a link on the way deletes nothing outside.
    std::fs::write(outside.join("victim"), "v").unwrap();
    std::os::unix::fs::symlink(&outside, host.join("via")).unwrap();
    sync::apply(&host, &[], &["via/victim".into()], &[]).unwrap();
    assert!(outside.join("victim").exists());
    let _ = std::fs::remove_dir_all(&host);
    let _ = std::fs::remove_dir_all(&outside);
}

// ── B11, B6: the pod codec (the same Python the sync pod runs), against a local replica ──────

#[cfg(unix)]
fn codec(replica: &Path, args: &[&str], stdin: &[u8]) -> std::process::Output {
    codec_jailed(replica, args, stdin, "-")
}

/// The codec with `jail` as its Landlock launcher (`-` = none: this host has no Landlock).
#[cfg(unix)]
fn codec_jailed(replica: &Path, args: &[&str], stdin: &[u8], jail: &str) -> std::process::Output {
    codec_program(replica, args, stdin, jail, sync::POD_CODEC)
}

#[cfg(unix)]
fn codec_program(
    replica: &Path,
    args: &[&str],
    stdin: &[u8],
    jail: &str,
    program: &str,
) -> std::process::Output {
    use std::io::Write as _;
    let mut c = std::process::Command::new("python3");
    c.hardened()
        .args(["-I", "-c", program])
        .args(args)
        .env("WICKED_SYNC_ROOT", replica)
        .env("WICKED_SYNC_JAIL", jail)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = c.spawn().expect("python3");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

#[cfg(unix)]
#[test]
fn k8s_the_pod_codec_mirrors_a_tree_without_following_links_and_packs_it_back() {
    let replica = tmp("replica");
    let outside = tmp("codec-outside");
    let tree = "home/u/wt";
    // A seat-planted link where a directory belongs: the copy-in replaces it, never follows it.
    std::fs::create_dir_all(replica.join(tree)).unwrap();
    std::os::unix::fs::symlink(&outside, replica.join(tree).join("src")).unwrap();
    std::fs::write(replica.join(tree).join("stale.txt"), "s").unwrap();
    let stream = sync::encode(&[Root {
        replica: tree.into(),
        mode: Mode::Tree,
        entries: vec![
            entry(Kind::File, "src/main.rs", b"fn main() {}"),
            entry(Kind::Link, "l", b"src/main.rs"),
        ],
    }]);
    let plan = json!([[tree, "tree", [".git"]]]).to_string();
    let out = codec(&replica, &["unpack", &plan], &stream);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        std::fs::read_dir(&outside).unwrap().next().is_none(),
        "the codec followed a link"
    );
    assert_eq!(
        std::fs::read(replica.join(tree).join("src/main.rs")).unwrap(),
        b"fn main() {}"
    );
    assert!(
        !replica.join(tree).join("stale.txt").exists(),
        "the mirror kept a stale file"
    );
    // A root the plan does not name is refused.
    let other = sync::encode(&[Root {
        replica: "etc".into(),
        mode: Mode::Plain,
        entries: vec![entry(Kind::File, "x", b"x")],
    }]);
    assert!(!codec(&replica, &["unpack", &plan], &other).status.success());
    // Pack it back: the same entries.
    let packed = codec(&replica, &["pack", &plan], b"{}");
    assert!(
        packed.status.success(),
        "{}",
        String::from_utf8_lossy(&packed.stderr)
    );
    let roots = sync::decode(&mut packed.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    let mut got: Vec<(String, Kind)> = roots[0]
        .entries
        .iter()
        .map(|e| (e.path.clone(), e.kind))
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        vec![("l".into(), Kind::Link), ("src/main.rs".into(), Kind::File)]
    );
    let _ = std::fs::remove_dir_all(&replica);
    let _ = std::fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[test]
fn k8s_the_pod_codec_lists_a_git_tree_by_git_and_never_packs_its_git_dir() {
    let replica = tmp("replica-git");
    let tree = replica.join("w");
    std::fs::create_dir_all(&tree).unwrap();
    let git = |args: &[&str]| {
        let o = sync::git(&tree).args(args).output().unwrap();
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    };
    git(&["init", "-q"]);
    std::fs::write(tree.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(tree.join("a.rs"), "a").unwrap();
    std::fs::create_dir_all(tree.join("target")).unwrap();
    std::fs::write(tree.join("target/big.o"), "o").unwrap();
    // A seat-written fsmonitor in the replica's config never runs in the sync pod.
    let marker = replica.join("fsmonitor-ran");
    git(&[
        "config",
        "core.fsmonitor",
        &format!("touch {}", marker.display()),
    ]);
    let plan = json!([["w", "tree", [".git"]]]).to_string();
    let packed = codec(&replica, &["pack", &plan], br#"{"w": []}"#);
    assert!(
        packed.status.success(),
        "{}",
        String::from_utf8_lossy(&packed.stderr)
    );
    let roots = sync::decode(&mut packed.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    let mut paths: Vec<&str> = roots[0].entries.iter().map(|e| e.path.as_str()).collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![".gitignore", "a.rs"],
        "ignored output or .git was packed"
    );
    assert!(!marker.exists(), "the replica's fsmonitor ran");
    let _ = std::fs::remove_dir_all(&replica);
}

// ── B11, B6, B17: the replica plan ───────────────────────────────────────────────────────────

#[test]
fn k8s_the_replica_plan_leaves_the_clone_out_and_comes_back_only_for_write_roots() {
    let clone = PathBuf::from("/home/u/.wicked/repos/app");
    let tree = clone.join("wt/run-1");
    let gitdir = clone.join(".git/worktrees/run-1");
    let notes = PathBuf::from("/home/u/.wicked/notes/run-1");
    let cache = PathBuf::from("/home/u/.wicked-worker/cache/claude");
    let home = PathBuf::from("/home/u/.wicked-worker/seat/claude");
    let skills = PathBuf::from("/home/u/.wicked/skills");
    let mounts = vec![
        PlanMount::Bind {
            path: tree.clone(),
            rw: true,
        },
        PlanMount::Bind {
            path: clone.clone(),
            rw: false,
        },
        PlanMount::Bind {
            path: gitdir.clone(),
            rw: true,
        },
        PlanMount::Bind {
            path: tree.join(".git"),
            rw: true,
        },
        PlanMount::Bind {
            path: notes.clone(),
            rw: true,
        },
        PlanMount::Bind {
            path: cache.clone(),
            rw: true,
        },
        PlanMount::Bind {
            path: home.clone(),
            rw: true,
        },
        PlanMount::Bind {
            path: skills.clone(),
            rw: false,
        },
        PlanMount::Mask {
            path: tree.join(".env"),
        },
    ];
    let write_roots = vec![tree.clone(), notes.clone()];
    let base = PathBuf::from("/home/u/.wicked-worker/cache");
    let (roots, pod) = seat::plan(
        &tree,
        false,
        &mounts,
        std::slice::from_ref(&clone),
        &write_roots,
        Some(&base),
    );
    let pod_paths: Vec<&Path> = pod.iter().map(|m| m.path.as_path()).collect();
    assert!(
        !pod_paths.contains(&clone.as_path()),
        "the clone root is mounted"
    );
    assert!(
        !pod_paths.contains(&gitdir.as_path()),
        "the worktree git dir is mounted"
    );
    assert!(!pod_paths.contains(&tree.join(".git").as_path()));
    let by = |p: &Path| {
        roots
            .iter()
            .find(|r| r.host == p)
            .unwrap_or_else(|| panic!("{p:?}"))
    };
    let t = by(&tree);
    assert_eq!(t.mode, Mode::Tree);
    assert!(t.back);
    assert!(t.skip.contains(&".env".to_string()) && t.skip.contains(&".git".to_string()));
    assert!(by(&notes).back);
    assert_eq!(by(&cache).mode, Mode::Seed);
    assert!(!by(&cache).back, "a cache came back");
    assert!(!by(&skills).back, "a read root came back");
    // Review r6 #1/#8: a seat home (and any other writable dir that is not the tree or an
    // admitted root, such as a fenced unit's scratch) stays in the replica.
    assert!(!by(&home).back, "a seat home came back");
    // B17: the same plan, read-only: the tree is a view and never comes back; the notes still do.
    let (roots, pod) = seat::plan(
        &tree,
        true,
        &mounts,
        &[clone],
        std::slice::from_ref(&notes),
        Some(&base),
    );
    let t = roots.iter().find(|r| r.host == tree).unwrap();
    assert!(!t.back, "a read-only unit's view came back");
    assert!(pod.iter().any(|m| m.path == tree && !m.rw));
    assert!(roots.iter().find(|r| r.host == notes).unwrap().back);
}

/// A container seat's cargo home and target dir (`<worker home>/seat-build/<cli>`) are a home in the
/// replica: not under the seed base (the host's cache is never copied into them), and no root that
/// comes back, so a build's `target/` never reaches the host even where the tree's `.gitignore`
/// lacks it (the target is not in the tree at all).
#[test]
fn k8s_the_cargo_state_dir_is_a_home_that_is_neither_seeded_nor_written_back() {
    let tree = PathBuf::from("/home/u/.wicked/repos/app/wt/run-1");
    let cache = PathBuf::from("/home/u/.wicked-worker/cache/claude");
    let build = PathBuf::from("/home/u/.wicked-worker/seat-build/claude");
    let mounts: Vec<PlanMount> = [&tree, &cache, &build]
        .into_iter()
        .map(|p| PlanMount::Bind {
            path: p.clone(),
            rw: true,
        })
        .collect();
    let seed_base = PathBuf::from("/home/u/.wicked-worker/cache");
    let (mut roots, _) = seat::plan(
        &tree,
        false,
        &mounts,
        &[],
        std::slice::from_ref(&tree),
        Some(&seed_base),
    );
    seat::mark_homes(&mut roots, &[cache.clone(), build.clone()], None);
    let by = |p: &Path| roots.iter().find(|r| r.host == p).unwrap();
    assert_eq!(by(&cache).mode, Mode::Seed);
    assert_eq!(by(&build).mode, Mode::Home, "not a seed");
    assert!(!by(&build).back, "the cargo state came back");
    // No root that comes back holds the cargo home or the target dir.
    for dir in [build.join("cargo-home"), build.join("target")] {
        assert!(
            !roots.iter().any(|r| r.back && dir.starts_with(&r.host)),
            "{dir:?} is under a write-back root"
        );
    }
}

/// After a cargo build in a unit, the write-back of a tree whose `.gitignore` lacks `target/` holds
/// no `target/` entry: the unit's real `CARGO_TARGET_DIR` (from the floor plan) is outside the
/// tree, so the build output lands in the cargo state home, which is never packed. Run through the
/// pod's own codec, as the real write-back is. With cargo's default (`<tree>/target`) the same
/// build WOULD be packed, because git lists it.
#[cfg(unix)]
#[test]
fn k8s_a_cargo_build_in_a_tree_without_a_target_ignore_writes_no_target_back() {
    use wicked_apps_core::spawn::{SeatCli, SeatConfig, WORKER_HOME_ENV};
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let host = tmp("cargo-wb");
    let tree = host.join("wt");
    let worker = host.join("worker");
    std::fs::create_dir_all(&tree).unwrap();
    let prior = std::env::var_os(WORKER_HOME_ENV);
    std::env::set_var(WORKER_HOME_ENV, &worker);
    let config = SeatConfig::Isolated {
        cli: SeatCli::Claude,
        root: None,
        set: Vec::new(),
        strip: Vec::new(),
    };
    let plan = crate::worker_sandbox::plan_seat_floor(
        crate::worker_sandbox::SeatLaunch {
            cwd: &tree,
            cli: SeatCli::Claude,
            config: &config,
            read_only: false,
            scratch: None,
            gov_log_dir: None,
            own_ports: &[],
        },
        vec![tree.clone()],
    );
    match prior {
        Some(v) => std::env::set_var(WORKER_HOME_ENV, v),
        None => std::env::remove_var(WORKER_HOME_ENV),
    }
    // Resolve the production floor and mount/replica plan, including every admitted root.
    let floor = crate::worker_sandbox::resolve_seat_floor(&plan.inputs).unwrap();
    let mounts = seat::plan_mounts(&crate::container_floor::mount_plan(&floor).unwrap());
    let (mut planned_roots, _) = seat::plan(
        &tree,
        plan.inputs.read_only,
        &mounts,
        &floor.protected,
        &plan.inputs.write_roots,
        Some(&worker.join("cache")),
    );
    seat::mark_homes(&mut planned_roots, &plan.inputs.seat_dirs, None);
    let replica = tmp("cargo-wb-replica");
    let scope = Some("cargo-unit");
    let mapped = |path: &Path| replica.join(seat::replica_of(scope, path).unwrap());
    for root in &planned_roots {
        std::fs::create_dir_all(mapped(&root.host)).unwrap();
    }
    let w = mapped(&tree);
    let init = std::process::Command::new("git")
        .hardened()
        .args(["init", "-q"])
        .current_dir(&w)
        .status()
        .expect("git init");
    assert!(init.success());
    std::fs::create_dir_all(w.join("src")).unwrap();
    std::fs::write(w.join(".gitignore"), "*.log\n").unwrap();
    std::fs::write(
        w.join("Cargo.toml"),
        "[package]\nname = \"cargo-wb-probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(w.join("src/main.rs"), "fn main() {}\n").unwrap();
    let planned_env = |key| plan.env.iter().find(|(k, _)| *k == key).unwrap().1.clone();
    let target = planned_env("CARGO_TARGET_DIR");
    let cargo_home = planned_env("CARGO_HOME");
    for path in [&target, &cargo_home] {
        assert!(!planned_roots
            .iter()
            .any(|r| r.mode == Mode::Seed && path.starts_with(&r.host)));
        assert!(!seat::write_back_roots(&planned_roots)
            .iter()
            .any(|r| path.starts_with(&r.host)));
        assert!(planned_roots.iter().any(|r| path.starts_with(&r.host)));
    }
    // A retained replica's old cargo seed must not influence this real cargo invocation.
    let legacy = mapped(&worker.join("cache/claude/cargo"));
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(
        legacy.join("config.toml"),
        "invalid cargo configuration [\n",
    )
    .unwrap();
    let mut cargo = std::process::Command::new("cargo");
    cargo
        .hardened()
        .args(["build", "--offline"])
        .current_dir(&w);
    for key in ["CARGO_HOME", "CARGO_TARGET_DIR"] {
        cargo.env(key, mapped(&planned_env(key)));
    }
    let built = cargo.output().expect("cargo must be on PATH");
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let artifact = mapped(&target).join("debug/cargo-wb-probe");
    assert!(artifact.is_file(), "cargo produced no binary: {artifact:?}");
    assert!(!w.join("target").exists());
    assert_ne!(mapped(&cargo_home), legacy);
    let refs: Vec<_> = seat::write_back_roots(&planned_roots)
        .into_iter()
        .map(|r| (seat::replica_of(scope, &r.host).unwrap(), r))
        .collect();
    let pack_plan = seat::codec_plan(&refs).to_string();
    let tracked = json!({(seat::replica_of(scope, &tree).unwrap()): []}).to_string();
    let packed = codec(&replica, &["pack", &pack_plan], tracked.as_bytes());
    assert!(
        packed.status.success(),
        "{}",
        String::from_utf8_lossy(&packed.stderr)
    );
    let roots = sync::decode(&mut packed.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    assert_eq!(roots.len(), refs.len());
    let paths: Vec<&str> = roots
        .iter()
        .flat_map(|r| r.entries.iter().map(|e| e.path.as_str()))
        .collect();
    assert!(paths.contains(&"src/main.rs"), "{paths:?}");
    assert!(
        !paths
            .iter()
            .any(|p| p.split('/').any(|part| part == "target") || p.contains("cargo-wb-probe")),
        "a target/ entry came back: {paths:?}"
    );
    let _ = std::fs::remove_dir_all(&host);
    let _ = std::fs::remove_dir_all(&replica);
}

#[test]
fn k8s_a_read_only_tree_exposed_only_by_the_boundary_is_still_its_own_root() {
    let clone = PathBuf::from("/home/u/repo");
    let tree = clone.join("wt");
    let (roots, pod) = seat::plan(
        &tree,
        true,
        &[PlanMount::Bind {
            path: clone.clone(),
            rw: false,
        }],
        &[clone],
        &[],
        None,
    );
    assert!(roots
        .iter()
        .any(|r| r.host == tree && r.mode == Mode::Tree && !r.back));
    assert!(pod.iter().any(|m| m.path == tree));
}

#[test]
fn k8s_bound_files_are_a_files_root_of_their_directory() {
    let tree = PathBuf::from("/home/u/wt");
    let dir = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "wicked-k8s-files-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("settings-build.json"), "{}").unwrap();
    std::fs::write(dir.join("decisions.ndjson"), "").unwrap();
    let (roots, _) = seat::plan(
        &tree,
        false,
        &[PlanMount::Bind {
            path: dir.join("settings-build.json"),
            rw: false,
        }],
        &[],
        &[],
        None,
    );
    let f = roots.iter().find(|r| r.host == dir).expect("a files root");
    assert_eq!(f.mode, Mode::Files);
    assert_eq!(
        f.files,
        vec!["settings-build.json".to_string()],
        "the rest of the dir is not copied"
    );
    assert!(!f.back);
    let plan = seat::codec_plan(&[("x".into(), f)]);
    assert_eq!(plan, json!([["x", "files", ["settings-build.json"], []]]));
    let _ = std::fs::remove_dir_all(&dir);
}

// ── B8: the unit env and the exec argv ───────────────────────────────────────────────────────

#[test]
fn k8s_no_secret_or_prompt_is_ever_on_an_exec_argv() {
    let env = seat::env_file(&[
        ("ANTHROPIC_API_KEY".into(), "wk-secret'; rm -rf / #".into()),
        ("MULTI".into(), "a\nb".into()),
    ])
    .unwrap();
    assert_eq!(
        env,
        "export ANTHROPIC_API_KEY='wk-secret'\\''; rm -rf / #'\nexport MULTI='a\nb'\n"
    );
    assert!(seat::env_file(&[("BAD-NAME".into(), "x".into())]).is_err());
    assert_eq!(
        seat::argv_line(&["claude".into(), "-p".into(), "it's".into()]).unwrap(),
        "set -- 'claude' '-p' 'it'\\''s'\n"
    );
    assert!(seat::argv_line(&[]).is_err());
    assert!(seat::env_file(&[("1X".into(), "x".into())]).is_err());
    // The wrapper's fixed script carries no value; the marker is an id.
    assert!(!seat::WRAPPER.contains("wk-"));
    let args: Vec<String> = [
        "exec",
        "-i",
        "-c",
        "seat",
        "p",
        "--",
        "/floor/busybox",
        "sh",
        "-c",
        seat::WRAPPER,
        "wicked-unit-abc123",
        "/floor/launch",
    ]
    .map(String::from)
    .to_vec();
    assert_eq!(seat::seat_id_of(&args).as_deref(), Some("abc123"));
    let mut forged = args.clone();
    forged[10] = "wicked-unit-../x".into();
    assert_eq!(seat::seat_id_of(&forged), None);
    let mut other = args;
    other[9] = "echo".into();
    assert_eq!(
        seat::seat_id_of(&other),
        None,
        "a command that is not the wrapper is not a seat"
    );
}

/// #1456: only identity reaches git through the file after the wrapper removes the unit env.
/// The no-push fence reaches git independently through the carrier's environment entries.
#[cfg(unix)]
#[test]
fn k8s_git_config_handoff_preserves_operator_identity_and_push_fence() {
    let host = tmp("git-config-host");
    let pod = tmp("git-config-pod");
    let unit = pod.join("unit's files");
    let tree = pod.join("repo");
    std::fs::create_dir_all(&unit).unwrap();
    std::fs::create_dir_all(&tree).unwrap();
    let global = host.join("gitconfig-copy");
    let system = host.join("gitconfig-system");
    let identity = concat!(
        "[user]\n\tname = \"Op \\\"quoted\\\" \\\\ path's $(false)\"\n",
        "\temail = \"op@users.noreply.example\"\n",
    );
    let content = format!(
        "{identity}\
         [commit]\ngpgsign = true\n\
         [gpg]\nprogram = /nonexistent\n\
         [filter \"lfs\"]\nrequired = true\nclean = /nonexistent %f\n\
         [url \"wicked-nopush::\"]\npushInsteadOf =\n\
         [protocol \"ssh\"]\nallow = never\n"
    );
    std::fs::write(&global, content).unwrap();
    std::fs::write(&system, "[user]\nname = System Identity\n").unwrap();
    let mut vars = vec![
        (
            "GIT_CONFIG_GLOBAL".into(),
            global.to_string_lossy().into_owned(),
        ),
        (
            "GIT_CONFIG_SYSTEM".into(),
            system.to_string_lossy().into_owned(),
        ),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
    ];
    // These are the actual COUNT/KEY/VALUE entries that finalize passes through landrun's
    // --env flags. The shipped identity file deliberately contains no push-fence entries.
    let fence = wicked_apps_core::spawn::nopush_git_config_env();
    vars.extend(fence.clone());
    let (prelude, reads) = seat::git_config_handoff(&mut vars, std::slice::from_ref(&tree), &unit);
    assert!(!prelude.contains("exit"));
    assert_eq!(reads, [unit.join("gitconfig-global").to_string_lossy()]);
    assert_eq!(vars[0].1, reads[0]);
    assert_eq!(vars[1].1, system.to_string_lossy());
    assert_eq!(&vars[3..], fence.as_slice());
    let env = unit.join("env");
    std::fs::write(
        &env,
        format!("{}{prelude}", seat::env_file(&vars).unwrap())
            .replace("/floor/busybox", "/usr/bin/env"),
    )
    .unwrap();
    // Host files are unavailable to the pod. No inherited identity can rescue the commit.
    std::fs::remove_dir_all(&host).unwrap();
    let out = std::process::Command::new("/bin/sh")
        .hardened()
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", &pod)
        .current_dir(&tree)
        .args([
            "-c",
            "set -eu; . \"$1\"; rm \"$1\"; git init -q -b main; \
             printf '*.bin filter=lfs\\n' > .gitattributes; echo content > tracked.bin; \
             git add .gitattributes tracked.bin; git commit -qm unit; \
             git log -1 --format='%an <%ae>|%cn <%ce>'; \
             git -c user.email=override@example.test commit --allow-empty -qm override; \
             git log -1 --format='%an <%ae>|%cn <%ce>'; \
             git remote add origin https://example.invalid/repo; \
             git remote get-url --push origin; git config --get protocol.ssh.allow; \
             if git push origin HEAD >push.log 2>&1; then false; fi; \
             cat push.log >&2; grep -q wicked-nopush push.log",
            "seat",
        ])
        .arg(&env)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let name = "Op \"quoted\" \\ path's $(false)";
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!(
            "{name} <op@users.noreply.example>|{name} <op@users.noreply.example>\n\
             {name} <override@example.test>|{name} <override@example.test>\n\
             wicked-nopush::https://example.invalid/repo\nnever"
        )
    );
    assert!(!env.exists());
    assert_eq!(std::fs::read_to_string(&reads[0]).unwrap(), identity);
    assert!(!unit.join("gitconfig-system").exists());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&reads[0]).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::remove_dir_all(pod).unwrap();
}

#[test]
fn k8s_git_config_handoff_keeps_visible_disabled_and_system_paths() {
    let mut vars = vec![
        ("GIT_CONFIG_GLOBAL".into(), "/replica/gitconfig-copy".into()),
        (
            "GIT_CONFIG_SYSTEM".into(),
            "/unmounted/system-config".into(),
        ),
        ("GIT_AUTHOR_NAME".into(), "Explicit Author".into()),
    ];
    let original = vars.clone();
    assert_eq!(
        seat::git_config_handoff(&mut vars, &["/replica".into()], Path::new(render::UNIT_DIR)),
        (String::new(), Vec::new())
    );
    assert_eq!(vars, original);
    for disabled in ["", "/dev/null"] {
        vars[0].1 = disabled.into();
        let original = vars.clone();
        assert_eq!(
            seat::git_config_handoff(&mut vars, &[], Path::new(render::UNIT_DIR)),
            (String::new(), Vec::new())
        );
        assert_eq!(vars, original);
    }
    assert_eq!(
        seat::git_config_handoff(&mut [], &[], Path::new(render::UNIT_DIR)),
        (String::new(), Vec::new())
    );
}

#[test]
fn k8s_git_config_handoff_skips_missing_unreadable_oversized_or_invalid_copies() {
    let host = tmp("git-config-skip");
    let file = host.join("gitconfig-copy");
    let skip = |path: &Path| {
        let mut vars = vec![(
            "GIT_CONFIG_GLOBAL".into(),
            path.to_string_lossy().into_owned(),
        )];
        let original = vars.clone();
        assert_eq!(
            seat::git_config_handoff(&mut vars, &[], Path::new(render::UNIT_DIR)),
            (String::new(), Vec::new())
        );
        assert_eq!(
            vars, original,
            "an unusable identity must not refuse the unit"
        );
    };
    skip(&file); // Missing file.
    skip(&host); // Not a regular file.
    std::fs::write(&file, vec![b'#'; 64 * 1024 + 1]).unwrap();
    skip(&file);
    for invalid in [
        b"[user]\nname=Op\0".as_slice(),
        b"[user\nname=Op\n",
        b"[user]\nname=Op\nemail=\"line\\nbreak\"\n",
    ] {
        std::fs::write(&file, invalid).unwrap();
        skip(&file);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&file, "[user]\nname=Op\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o0)).unwrap();
        // A privileged test runner can still read mode 000; the error path applies when denied.
        if std::fs::File::open(&file).is_err() {
            skip(&file);
        }
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = host.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        skip(&link);
    }
    std::fs::remove_dir_all(host).unwrap();
}

#[test]
fn k8s_git_config_handoff_ignores_includes_and_allows_single_keys_at_size_limit() {
    let host = tmp("git-config-keys");
    let file = host.join("gitconfig-copy");
    let included = host.join("included");
    std::fs::write(
        &included,
        "[user]\nname=Included\nemail=included@example.test\n",
    )
    .unwrap();
    let include = format!("[include]\npath = {}\n", included.display());
    // Includes must not supply either identity key, even when both are otherwise absent.
    std::fs::write(&file, &include).unwrap();
    let mut vars = vec![(
        "GIT_CONFIG_GLOBAL".into(),
        file.to_string_lossy().into_owned(),
    )];
    let original = vars.clone();
    assert_eq!(
        seat::git_config_handoff(&mut vars, &[], &host),
        (String::new(), Vec::new())
    );
    assert_eq!(vars, original);
    for (key, value) in [("name", "Op"), ("email", "op@example.test")] {
        let mut content = format!("[user]\n{key} = {value}\n{include}#");
        content.extend(std::iter::repeat_n('x', 64 * 1024 - content.len()));
        std::fs::write(&file, content).unwrap();
        let (prelude, reads) = seat::git_config_handoff(&mut vars, &[], &host);
        assert!(!prelude.contains("exit"));
        assert!(prelude.contains(&format!("[user]\n\t{key} = \"{value}\"\n")));
        assert_eq!(reads, [host.join("gitconfig-global").to_string_lossy()]);
        vars.clone_from(&original);
    }
    std::fs::remove_dir_all(host).unwrap();
}

#[cfg(unix)]
#[test]
fn k8s_git_config_handoff_write_failure_falls_back_to_empty_config() {
    let host = tmp("git-config-write-failure");
    let file = host.join("gitconfig-copy");
    std::fs::write(&file, "[user]\nname=Op\nemail=op@example.test\n").unwrap();
    let mut vars = vec![(
        "GIT_CONFIG_GLOBAL".into(),
        file.to_string_lossy().into_owned(),
    )];
    // A failed temporary write must remove any stale/partial final config as well.
    let dest = host.join("gitconfig-global");
    let partial = host.join("gitconfig-global.tmp");
    std::fs::write(&dest, "[user\n").unwrap();
    std::fs::create_dir(&partial).unwrap();
    let (prelude, _) = seat::git_config_handoff(&mut vars, &[], &host);
    assert!(!prelude.contains("exit"));
    vars.push((
        "WICKED_UNIT_CWD".into(),
        host.to_string_lossy().into_owned(),
    ));
    vars.push(("GIT_CONFIG_NOSYSTEM".into(), "1".into()));
    let env = host.join("env");
    std::fs::write(
        &env,
        format!(
            "{}{prelude}{}",
            seat::env_file(&vars).unwrap(),
            seat::argv_line(&[
                "/bin/sh".into(),
                "-c".into(),
                "test \"$GIT_CONFIG_GLOBAL\" = /dev/null && git config --list && \
                 git init -q -b main && git -c user.name=Unit \
                 -c user.email=unit@example.test -c commit.gpgsign=false \
                 commit --allow-empty -qm unit"
                    .into(),
            ])
            .unwrap()
        )
        .replace("/floor/busybox", "/usr/bin/env"),
    )
    .unwrap();
    let wrapper = seat::WRAPPER
        .replace("/wicked-unit", &host.to_string_lossy())
        .replace("/floor/busybox", "/usr/bin/env");
    let out = std::process::Command::new("/bin/sh")
        .hardened()
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", &host)
        .args(["-c", &wrapper])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr)
            .matches("wicked-seat: the git identity could not be written")
            .count(),
        1
    );
    assert!(!dest.exists());
    assert!(!partial.exists());
    assert!(!env.exists());
    assert_eq!(std::fs::read_to_string(host.join("rc")).unwrap(), "0\n");
    std::fs::remove_dir_all(host).unwrap();
}

#[cfg(unix)]
#[test]
fn k8s_git_config_handoff_bounds_host_git_and_discards_incomplete_identity() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    const CHILD_ROOT: &str = "WICKED_TEST_GIT_IDENTITY_ROOT";
    if let Some(host) = std::env::var_os(CHILD_ROOT) {
        let host = PathBuf::from(host);
        let mut vars = vec![(
            "GIT_CONFIG_GLOBAL".into(),
            host.join("gitconfig-copy").to_string_lossy().into_owned(),
        )];
        let original = vars.clone();
        assert_eq!(
            seat::git_config_handoff(&mut vars, &[], &host),
            (String::new(), Vec::new())
        );
        assert_eq!(vars, original, "a failed git call must not refuse the unit");
        return;
    }

    let host = tmp("git-config-bounded");
    let git = host.join("git");
    std::fs::write(
        host.join("gitconfig-copy"),
        "[user]\nname=Op\nemail=op@example.test\n",
    )
    .unwrap();
    // Isolate PATH in a child test process so concurrent tests keep using the real git.
    // The name succeeds; a failed email must discard the identity already extracted.
    for failure in [
        "exec /bin/sleep 60",
        "kill -KILL $$",
        "value=x; i=0; while [ $i -lt 17 ]; do value=$value$value; i=$((i + 1)); done; \
         printf '%s\\000' \"$value\"",
    ] {
        std::fs::write(
            &git,
            format!(
                "#!/bin/sh\nif [ \"$7\" = user.name ]; then printf 'Op\\000'; exit 0; fi\n\
                 {failure}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .hardened()
            .env("PATH", &host)
            .env(CHILD_ROOT, &host)
            .args([
                "--exact",
                concat!(
                    "k8s::seat_tests::",
                    "k8s_git_config_handoff_bounds_host_git_and_discards_incomplete_identity"
                ),
                "--nocapture",
            ]);
        let out = super::run_capped(child, None, Duration::from_secs(20), 64 * 1024);
        assert!(out.ok(), "{failure}: {}", out.err_text());
        assert_eq!(
            String::from_utf8_lossy(&out.stderr)
                .matches("wicked-core: the seat git identity could not be read")
                .count(),
            1,
            "{failure}"
        );
    }
    std::fs::remove_dir_all(host).unwrap();
}

// ── A3: exit-code honesty ────────────────────────────────────────────────────────────────────

#[test]
fn k8s_a_lost_exec_stream_is_never_an_exit_code() {
    assert_eq!(seat::exit_code(0, Some("0\n")), Some(0));
    assert_eq!(seat::exit_code(3, Some("3")), Some(3));
    // kubectl lost the stream: its own code (1) with no rc file, or one that disagrees.
    assert_eq!(seat::exit_code(1, None), None);
    assert_eq!(seat::exit_code(1, Some("0")), None);
    assert_eq!(seat::exit_code(0, Some("garbage")), None);
    assert_ne!(Ending::Exited(0), Ending::Cancelled);
}

// ── B15: the bound-pod gate ──────────────────────────────────────────────────────────────────

fn pod(owner: &str, unit: &str, phase: &str, ready: bool) -> serde_json::Value {
    json!({
        "metadata": {
            "name": "seat-1-abc",
            "labels": {"wicked.dev/unit": unit},
            "ownerReferences": [{"kind": "Sandbox", "name": owner}],
        },
        "status": {
            "phase": phase,
            "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}],
            "containerStatuses": [{"name": "seat", "ready": ready, "image": "wicked-seat-x"}],
        }
    })
}

#[test]
fn k8s_exec_targets_only_the_pod_bound_to_this_claim() {
    assert_eq!(
        seat::pod_state(
            &pod("seat-1-abc", "u1", "Running", true),
            "seat-1-abc",
            "u1"
        ),
        PodState::Ready
    );
    assert_eq!(
        seat::pod_state(
            &pod("seat-1-abc", "u1", "Pending", false),
            "seat-1-abc",
            "u1"
        ),
        PodState::Waiting
    );
    for (o, u) in [("other", "u1"), ("seat-1-abc", "u2")] {
        assert!(matches!(
            seat::pod_state(&pod(o, u, "Running", true), "seat-1-abc", "u1"),
            PodState::Failed(FloorUnarmed::PolicyRefused, _)
        ));
    }
    let mut gc = pod("seat-1-abc", "u1", "Pending", false);
    gc["status"]["containerStatuses"][0]["state"] =
        json!({"waiting": {"reason": "ErrImageNeverPull"}});
    assert!(matches!(
        seat::pod_state(&gc, "seat-1-abc", "u1"),
        PodState::Failed(FloorUnarmed::ImageUnavailable, _)
    ));
    let quota = json!({"status": {"conditions": [{"message": "pods \"x\" is forbidden: exceeded quota: run"}]}});
    assert_eq!(
        seat::sandbox_refusal(&quota).map(|r| r.0),
        Some(FloorUnarmed::AdmissionPending)
    );
}

// ── B17: the read-only view in the pod ───────────────────────────────────────────────────────

fn seat_pod(view: Option<PathBuf>, scope: Option<String>) -> SeatPod {
    let tree = PathBuf::from("/home/u/wt");
    SeatPod {
        name: "judge-1".into(),
        namespace: "ns".into(),
        owner: "o".into(),
        run_label: "r".into(),
        unit_label: "u".into(),
        boot: "b".into(),
        role: "judge".into(),
        image: "img".into(),
        floor_image: "floor".into(),
        workdir: tree.clone(),
        mounts: vec![
            ReplicaMount {
                path: tree.clone(),
                rw: false,
            },
            ReplicaMount {
                path: tree.join("notes"),
                rw: true,
            },
        ],
        memory_limit: "1Gi".into(),
        memory_request: "256Mi".into(),
        cpu_request: "100m".into(),
        gateway_ip: None,
        shutdown_time: "2026-01-01T00:00:00Z".into(),
        deadline_secs: 60,
        scope,
        view,
        deps: None,
    }
}

#[test]
fn k8s_a_read_only_units_view_is_never_in_the_write_back() {
    let spec = render::seat_pod_spec(&seat_pod(
        Some("/home/u/wt".into()),
        Some("s-validator-1".into()),
    ))
    .unwrap();
    let mounts = spec["containers"][0]["volumeMounts"].as_array().unwrap();
    let at = |p: &str| {
        mounts
            .iter()
            .filter(|m| m["mountPath"] == p)
            .cloned()
            .collect::<Vec<_>>()
    };
    // The tree's path is the throwaway emptyDir, never the replica.
    let tree = at("/home/u/wt");
    assert_eq!(tree.len(), 1);
    assert_eq!(tree[0]["name"], "view");
    // One level down in the emptyDir, in a dir the view init makes as the pod's uid: the
    // emptyDir's root is the kubelet's, and git refuses a tree it does not own (phase C).
    assert_eq!(tree[0]["subPath"], render::VIEW_TREE);
    // The replica's copy is the view's read-only source, scoped to the session.
    let src = at(render::VIEW_SOURCE);
    assert_eq!(src[0]["readOnly"], true);
    assert_eq!(src[0]["subPath"], "s-validator-1/home/u/wt");
    // An admitted root under the tree is the replica's, read-write, over the view.
    let notes = at("/home/u/wt/notes");
    assert_eq!(notes[0]["name"], "replica");
    assert_eq!(notes[0]["subPath"], "s-validator-1/home/u/wt/notes");
    let init: Vec<&str> = spec["initContainers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(init, vec!["floor", "view"]);
    let copy = spec["initContainers"][1]["command"][3].as_str().unwrap();
    assert!(
        copy.contains(&format!("rm -rf /wicked-view/{}", render::VIEW_TREE))
            && copy.contains(&format!("mkdir /wicked-view/{}", render::VIEW_TREE))
            && copy.contains(&format!("-C /wicked-view/{} -xf", render::VIEW_TREE)),
        "{copy}"
    );
    let vol = spec["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "view")
        .unwrap();
    assert!(vol["emptyDir"].is_object());
    // Without a view, the tree is the replica's.
    let spec = render::seat_pod_spec(&seat_pod(None, None)).unwrap();
    let mounts = spec["containers"][0]["volumeMounts"].as_array().unwrap();
    assert!(mounts.iter().any(|m| m["mountPath"] == "/home/u/wt"
        && m["name"] == "replica"
        && m["subPath"] == "home/u/wt"));
}

#[test]
fn k8s_one_shot_sessions_share_the_session_namespace_in_their_own_scope() {
    assert!(seat::one_shot_scope("validator-12-3").is_some());
    assert!(seat::one_shot_scope("council-1").is_some());
    assert_eq!(seat::one_shot_scope("run-abc"), None);
    assert_ne!(
        seat::one_shot_scope("validator-1-1"),
        seat::one_shot_scope("validator-1-2")
    );
}

// ── B8, B9: the gateway writer ───────────────────────────────────────────────────────────────

#[test]
fn k8s_a_writer_write_carries_its_digest_never_its_body_and_commits_only_whole() {
    let s = gw::write_script(gw::Volume::Broker, "anthropic.token", &"a".repeat(64)).unwrap();
    assert!(s.contains(&"a".repeat(64)) && s.contains("/broker/anthropic.token"));
    assert!(
        s.find("sha256sum").unwrap() < s.find("mv -f").unwrap(),
        "committed before its check"
    );
    assert!(gw::write_script(gw::Volume::Broker, "../x", &"a".repeat(64)).is_err());
    assert!(gw::write_script(gw::Volume::Broker, "a;rm", &"a".repeat(64)).is_err());
    assert!(gw::write_script(gw::Volume::Config, "config.json", "short").is_err());
    // The key rides stdin (`read K`).
    assert!(gw::confirm_script(true).starts_with("read K;"));
    assert!(gw::confirm_script(false).contains(" 401 "));
}

#[test]
fn k8s_broker_reads_parse_whole_or_not_at_all() {
    let s = gw::read_script(&[("anthropic.token", false), ("openai.key", true)]).unwrap();
    assert!(s.contains("[ ! -L /broker/anthropic.token ]"));
    assert!(gw::read_script(&[("../etc", false)]).is_err());
    let out = b"anthropic.token 5\nhelloopenai.key 1\n1";
    assert_eq!(
        gw::parse_read(out).unwrap(),
        vec![
            ("anthropic.token".into(), b"hello".to_vec()),
            ("openai.key".into(), b"1".to_vec())
        ]
    );
    assert!(gw::parse_read(b"anthropic.token 9\nhello").is_err());
    assert_eq!(
        gw::parse_read(b"").unwrap(),
        Vec::<(String, Vec<u8>)>::new()
    );
}

#[test]
fn k8s_the_gateway_reaches_the_host_alias_it_resolved() {
    let out = "Server:\t\t10.43.0.10\nAddress:\t10.43.0.10:53\n\nName:\thost.lima.internal\nAddress: 192.168.5.2\n";
    assert_eq!(gw::parse_nslookup(out).as_deref(), Some("192.168.5.2"));
    assert_eq!(gw::parse_nslookup("** server can't find x: NXDOMAIN"), None);
    let policy = json!({"spec": {"egress": [
        {"to": [{"ipBlock": {"cidr": "0.0.0.0/0"}}], "ports": [{"port": 443}]},
        {"to": [{"ipBlock": {"cidr": "192.168.5.2/32"}}], "ports": [{"protocol": "TCP", "port": 7711}]},
    ]}});
    let hp = gw::render_host_ports(&policy);
    assert_eq!(hp.len(), 1);
    assert_eq!((hp[0].ip.as_str(), hp[0].port), ("192.168.5.2", 7711));
}

#[test]
fn k8s_memory_limits_render_as_quantities() {
    assert_eq!(crate::container_floor::k8s_memory("3g"), "3Gi");
    assert_eq!(crate::container_floor::k8s_memory("512m"), "512Mi");
    assert_eq!(crate::container_floor::k8s_memory("2Gi"), "2Gi");
}

#[allow(dead_code)]
fn _plan_root_is_constructible(r: PlanRoot) -> PlanRoot {
    r
}

// ── B17: what the gate hook stops judging in a pod, and what it still does ───────────────────

#[test]
fn k8s_a_pod_seats_read_only_writes_are_left_to_the_view_but_pre_build_is_still_judged() {
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let cwd = PathBuf::from("/home/u/wt");
    let ctx = json!({"command": "echo x > src/lib.rs && python3 -c 'open(\"a\",\"w\")' && rm b"});
    let ro = crate::write_posture::WritePosture::ReadOnly;
    let judge = |pre: bool, posture| {
        crate::gate_hook::phase_scope_denial(pre, posture, &ctx, "Bash", &cwd, None, &[])
    };
    std::env::remove_var(crate::gate_hook::SEAT_POD_ENV);
    assert!(
        judge(false, ro).is_some(),
        "the host floor must still judge a read-only write"
    );
    std::env::set_var(crate::gate_hook::SEAT_POD_ENV, "1");
    assert!(
        judge(false, ro).is_none(),
        "a pod's read-only writes land in its throwaway view"
    );
    assert!(
        judge(true, crate::write_posture::WritePosture::Full).is_some(),
        "a pre-build creator's tree comes back: still judged in a pod"
    );
    std::env::remove_var(crate::gate_hook::SEAT_POD_ENV);
}

// ── review r1 folds ──────────────────────────────────────────────────────────────────────────

#[test]
fn k8s_a_mask_covers_every_case_spelling_of_its_path() {
    let r = root(vec![
        entry(Kind::File, ".ENV", b"planted"),
        entry(Kind::File, "Secrets/K", b"planted"),
        entry(Kind::File, "ok.txt", b"x"),
    ]);
    let ok = sync::check_entries(&r, &[".env".into(), "secrets".into()]).unwrap();
    assert_eq!(
        ok.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        vec!["ok.txt"]
    );
    assert!(sync::under(".e\u{200b}NV", ".env"));
}

#[test]
fn k8s_links_that_escape_together_are_refused() {
    let r = root(vec![
        entry(Kind::Link, "a", b"."),
        entry(Kind::Link, "b", b"a/../outside"),
    ]);
    assert!(
        sync::check_entries(&r, &[]).is_err(),
        "a -> . and b -> a/../outside escape together"
    );
    let ok = root(vec![
        entry(Kind::Link, "d/a", b".."),
        entry(Kind::Link, "d/b", b"a/x"),
        entry(Kind::File, "x", b"1"),
    ]);
    assert!(sync::check_entries(&ok, &[]).is_ok());
    let cycle = root(vec![
        entry(Kind::Link, "a", b"b"),
        entry(Kind::Link, "b", b"a"),
    ]);
    assert!(sync::check_entries(&cycle, &[]).is_err());
}

#[test]
fn k8s_write_back_applies_only_the_seats_delta_and_refuses_a_conflict() {
    let host = tmp("delta");
    std::fs::write(host.join("seat.txt"), "v1").unwrap();
    std::fs::write(host.join("hosts.txt"), "v1").unwrap();
    std::fs::write(host.join("both.txt"), "v1").unwrap();
    std::fs::write(host.join("gone.txt"), "v1").unwrap();
    let v1 = sync::digest(&entry(Kind::File, "x", b"v1"));
    let base: std::collections::HashMap<String, [u8; 32]> =
        ["seat.txt", "hosts.txt", "both.txt", "gone.txt"]
            .iter()
            .map(|p| (p.to_string(), v1))
            .collect();
    // While the unit ran, the host changed `hosts.txt` and created `new-on-host.txt`.
    std::fs::write(host.join("hosts.txt"), "host-v2").unwrap();
    std::fs::write(host.join("new-on-host.txt"), "h").unwrap();
    let replica = vec![
        entry(Kind::File, "seat.txt", b"seat-v2"),
        entry(Kind::File, "hosts.txt", b"v1"),
        entry(Kind::File, "both.txt", b"v1"),
        entry(Kind::File, "added.txt", b"a"),
    ];
    let (writes, deletes) = seat::delta(&host, &base, replica.clone()).unwrap();
    let w: Vec<&str> = writes.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        w,
        vec!["seat.txt", "added.txt"],
        "the host's own change was written over"
    );
    assert_eq!(deletes, vec!["gone.txt".to_string()]);
    // Both changed one file: refused, never overwritten.
    std::fs::write(host.join("both.txt"), "host-v2").unwrap();
    let mut conflict = replica;
    conflict[2] = entry(Kind::File, "both.txt", b"seat-v2");
    assert!(seat::delta(&host, &base, conflict)
        .unwrap_err()
        .contains("both.txt"));
    let _ = std::fs::remove_dir_all(&host);
}

#[cfg(unix)]
#[test]
fn k8s_a_file_replacing_a_nested_tree_lands_after_its_contents_go() {
    let host = tmp("dir2file");
    std::fs::create_dir_all(host.join("dir/nested")).unwrap();
    std::fs::write(host.join("dir/nested/old"), "o").unwrap();
    sync::apply(
        &host,
        &[entry(Kind::File, "dir", b"now a file")],
        &["dir/nested/old".into()],
        &[],
    )
    .unwrap();
    assert_eq!(std::fs::read(host.join("dir")).unwrap(), b"now a file");
    // A directory still holding a file (ignored output) is never removed.
    std::fs::create_dir_all(host.join("keep/sub")).unwrap();
    std::fs::write(host.join("keep/sub/build.o"), "b").unwrap();
    assert!(sync::apply(&host, &[entry(Kind::File, "keep", b"x")], &[], &[]).is_err());
    assert!(host.join("keep/sub/build.o").exists());
    let _ = std::fs::remove_dir_all(&host);
}

#[cfg(unix)]
#[test]
fn k8s_the_pod_codec_scrubs_a_planted_masked_file_at_copy_in() {
    let replica = tmp("scrub");
    let tree = "w";
    std::fs::create_dir_all(replica.join(tree)).unwrap();
    std::fs::write(replica.join(tree).join(".ENV"), "planted by a seat").unwrap();
    let stream = sync::encode(&[Root {
        replica: tree.into(),
        mode: Mode::Plain,
        entries: vec![entry(Kind::File, "a", b"a")],
    }]);
    let plan = json!([[tree, "plain", [".env"], [".env"]]]).to_string();
    let out = codec(&replica, &["unpack", &plan], &stream);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !replica.join(tree).join(".ENV").exists(),
        "a planted masked file survived the copy-in"
    );
    let _ = std::fs::remove_dir_all(&replica);
}

#[test]
fn k8s_a_capped_call_stops_at_its_cap() {
    let mut c = std::process::Command::new("sh");
    c.hardened().args(["-c", "yes x | head -c 1000000"]);
    let r = super::run_capped(c, None, std::time::Duration::from_secs(20), 1000);
    assert!(!r.ok(), "a capped call reported success");
    assert!(r.stdout.is_empty());
    assert!(String::from_utf8_lossy(&r.stderr).contains("cap"));
    let mut c = std::process::Command::new("sh");
    c.hardened().args(["-c", "printf abc"]);
    let r = super::run_capped(c, None, std::time::Duration::from_secs(20), 1000);
    assert!(r.ok() && r.stdout == b"abc");
}

#[test]
fn k8s_a_quarantined_gateway_starts_only_on_a_new_boot_token() {
    let dir = tmp("boot");
    let t1 = gw::boot_token(&dir, false).unwrap();
    assert_eq!(
        gw::boot_token(&dir, false).unwrap(),
        t1,
        "the token is stable across arms"
    );
    let t2 = gw::boot_token(&dir, true).unwrap();
    assert_ne!(t1, t2, "a quarantine keeps the old token");
    let spec = render::GatewaySpec {
        namespace: "n".into(),
        owner: "o".into(),
        image: "g".into(),
        writer_image: "w".into(),
        boot: t2.clone(),
    };
    let d = render::gateway_deployment(&spec);
    let wait = d["spec"]["template"]["spec"]["initContainers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "wait-config")
        .unwrap();
    assert!(wait["command"][2].as_str().unwrap().contains("/cfg/boot"));
    assert_eq!(wait["env"][0]["value"], t2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn k8s_a_tree_under_tmp_can_be_mounted_but_tmp_itself_cannot() {
    let mut p = seat_pod(None, None);
    p.workdir = "/tmp/wicked-core-sandbox/x".into();
    p.mounts = vec![ReplicaMount {
        path: "/tmp/wicked-core-sandbox/x".into(),
        rw: true,
    }];
    assert!(render::seat_pod_spec(&p).is_ok());
    p.mounts = vec![ReplicaMount {
        path: "/tmp".into(),
        rw: true,
    }];
    assert!(render::seat_pod_spec(&p).is_err());
    p.mounts = vec![ReplicaMount {
        path: "/wicked-unit/x".into(),
        rw: true,
    }];
    assert!(render::seat_pod_spec(&p).is_err());
}

#[cfg(unix)]
#[test]
fn k8s_a_key_registers_and_revokes_without_deadlocking_the_publish_lock() {
    let dir = tmp("gwlock");
    let kubectl = dir.join("kubectl");
    crate::test_env::install_stub(&kubectl, "#!/bin/sh\ncat >/dev/null 2>&1\nexit 0\n");
    let g = gw::Gw {
        kube: super::Kube {
            kubectl,
            kubeconfig: None,
            context: "t".into(),
            owner: "o".into(),
        },
        ns: "ns".into(),
        pod: "agw-1".into(),
        service_ip: "10.0.0.1".into(),
        dir: dir.join("gateway"),
    };
    std::fs::create_dir_all(&g.dir).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let g2 = g.clone();
    std::thread::spawn(move || {
        let key = crate::seat_gateway::UnitKey::new("r", "r#1", "codex");
        let a = gw::register_live(&g2, key.clone());
        let b = gw::deregister_live(&g2, &key.key);
        let _ = tx.send((a, b));
    });
    let (a, b) = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("register/deregister deadlocked");
    a.unwrap();
    b.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

// ── review r2 folds ──────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
#[test]
fn k8s_the_production_plan_packs_and_unpacks_through_the_pod_codec() {
    let replica = tmp("plan4");
    let r = PlanRoot {
        host: "/w".into(),
        mode: Mode::Plain,
        skip: vec![".env".into()],
        back: true,
        files: Vec::new(),
        masks: vec![".env".into()],
        repo: false,
        ro: false,
        optional: false,
    };
    let plan = seat::codec_plan(&[("w".into(), &r)]).to_string();
    let stream = sync::encode(&[Root {
        replica: "w".into(),
        mode: Mode::Plain,
        entries: vec![entry(Kind::File, "a", b"a")],
    }]);
    let up = codec(&replica, &["unpack", &plan], &stream);
    assert!(
        up.status.success(),
        "{}",
        String::from_utf8_lossy(&up.stderr)
    );
    let packed = codec(&replica, &["pack", &plan], b"{}");
    assert!(
        packed.status.success(),
        "{}",
        String::from_utf8_lossy(&packed.stderr)
    );
    let roots = sync::decode(&mut packed.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    assert_eq!(roots[0].entries.len(), 1);
    let _ = std::fs::remove_dir_all(&replica);
}

#[cfg(unix)]
#[test]
fn k8s_a_nested_mask_is_scrubbed_in_any_case_and_an_empty_git_root_leaves_no_git() {
    let replica = tmp("scrub2");
    std::fs::create_dir_all(replica.join("w/Secrets")).unwrap();
    std::fs::write(replica.join("w/Secrets/k"), "planted").unwrap();
    std::fs::create_dir_all(replica.join("w/.git/hooks")).unwrap();
    std::fs::write(replica.join("w/.git/hooks/pre-commit"), "planted").unwrap();
    let stream = sync::encode(&[
        Root {
            replica: "w/.git".into(),
            mode: Mode::Git,
            entries: vec![],
        },
        Root {
            replica: "w".into(),
            mode: Mode::Tree,
            entries: vec![entry(Kind::File, "a", b"a")],
        },
    ]);
    let plan = json!([
        ["w/.git", "git", [], []],
        ["w", "tree", [".git", "secrets/k"], ["secrets/k"]]
    ])
    .to_string();
    let out = codec(&replica, &["unpack", &plan], &stream);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !replica.join("w/Secrets/k").exists(),
        "a case-varied nested mask survived"
    );
    assert!(
        !replica.join("w/.git").exists(),
        "a planted .git survived a repo-less copy-in"
    );
    let _ = std::fs::remove_dir_all(&replica);
}

#[cfg(unix)]
#[test]
fn k8s_a_host_link_the_seat_never_saw_cannot_carry_its_link_outside() {
    let host = tmp("hostlink");
    std::os::unix::fs::symlink(".", host.join("a")).unwrap();
    let writes = vec![entry(Kind::Link, "b", b"a/../outside")];
    assert!(
        sync::links_stay_inside(&writes).is_ok(),
        "lexically inside on its own"
    );
    assert!(sync::links_stay_inside_on(Some(&host), &writes).is_err());
    let _ = std::fs::remove_dir_all(&host);
}

#[cfg(unix)]
#[test]
fn k8s_a_write_naming_a_mask_by_another_spelling_is_refused_by_inode() {
    let host = tmp("alias");
    std::fs::write(host.join(".env"), "HOST_SECRET").unwrap();
    // On a case-insensitive host `.ENV` IS `.env`; on a case-sensitive one it is another file.
    let aliases = host.join(".ENV").exists();
    let r = sync::apply(
        &host,
        &[entry(Kind::File, ".ENV", b"x")],
        &[],
        &[".env".into()],
    );
    if aliases {
        assert!(r.is_err(), "an alias of a mask was written");
        assert_eq!(
            std::fs::read_to_string(host.join(".env")).unwrap(),
            "HOST_SECRET"
        );
    } else {
        assert!(r.is_ok());
    }
    assert!(sync::apply(&host, &[], &[".env".into()], &[".env".into()]).is_err());
    let _ = std::fs::remove_dir_all(&host);
}

#[test]
fn k8s_an_ancestor_read_root_leaves_the_clone_out() {
    let clone = PathBuf::from("/srv/repos/app");
    let tree = clone.join("wt/run-1");
    let mounts = vec![
        PlanMount::Bind {
            path: "/srv/repos".into(),
            rw: false,
        },
        PlanMount::Bind {
            path: tree.clone(),
            rw: true,
        },
    ];
    let (roots, _) = seat::plan(
        &tree,
        false,
        &mounts,
        std::slice::from_ref(&clone),
        std::slice::from_ref(&tree),
        None,
    );
    let up = roots
        .iter()
        .find(|r| r.host == Path::new("/srv/repos"))
        .unwrap();
    assert!(up.skip.iter().any(|s| s == "app"), "{:?}", up.skip);
}

// ── review r3 folds ──────────────────────────────────────────────────────────────────────────

#[test]
fn k8s_two_spellings_a_host_folds_together_are_refused() {
    // NFC `é` and NFD `e` + U+0301 are one name on APFS.
    let r = root(vec![
        entry(Kind::File, "caf\u{e9}", b"a"),
        entry(Kind::File, "cafe\u{301}", b"b"),
    ]);
    assert!(
        sync::check_entries(&r, &[]).is_err(),
        "NFC and NFD of one name"
    );
    // A file and a directory of one folded name.
    let r = root(vec![
        entry(Kind::File, "\u{c9}/a", b"a"),
        entry(Kind::File, "e\u{301}", b"b"),
    ]);
    assert!(
        sync::check_entries(&r, &[]).is_err(),
        "a folded file/dir pair"
    );
    // The long s folds to `s` (APFS): a mask of `.ssh` covers `.ſsh`.
    assert!(sync::under(".\u{17f}sh/k", ".ssh"));
    assert!(sync::under(".SSH/k", ".ssh"));
    assert!(!sync::under(".sshx/k", ".ssh"));
    let r = root(vec![
        entry(Kind::File, "a", b"a"),
        entry(Kind::File, "b", b"b"),
    ]);
    assert!(sync::check_entries(&r, &[]).is_ok());
}

#[cfg(unix)]
#[test]
fn k8s_a_refusal_in_a_later_root_leaves_every_earlier_root_unchanged() {
    let a = tmp("pre-a");
    let b = tmp("pre-b");
    std::fs::write(b.join("secret.txt"), "HOST").unwrap();
    // Another name for the masked file (a hard link stands in for an APFS alias on any host).
    std::fs::hard_link(b.join("secret.txt"), b.join("alias.txt")).unwrap();
    let plain = |host: &Path, masks: Vec<String>| PlanRoot {
        host: host.to_path_buf(),
        mode: Mode::Plain,
        skip: masks.clone(),
        back: true,
        files: Vec::new(),
        masks,
        repo: false,
        ro: false,
        optional: false,
    };
    let ra = plain(&a, Vec::new());
    let rb = plain(&b, vec!["secret.txt".into()]);
    let plans: Vec<seat::RootPlan<'_>> = vec![
        (&ra, vec![entry(Kind::File, "ok.txt", b"seat")], Vec::new()),
        (
            &rb,
            vec![entry(Kind::File, "alias.txt", b"seat")],
            Vec::new(),
        ),
    ];
    assert!(seat::apply_roots(&plans).is_err());
    assert!(
        !a.join("ok.txt").exists(),
        "an earlier root changed before a later root was refused"
    );
    assert_eq!(
        std::fs::read_to_string(b.join("secret.txt")).unwrap(),
        "HOST"
    );
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}

#[cfg(unix)]
#[test]
fn k8s_a_commit_path_naming_a_mask_on_this_host_is_refused() {
    let t = tmp("commit-alias");
    std::fs::write(t.join("secret.txt"), "HOST").unwrap();
    std::fs::hard_link(t.join("secret.txt"), t.join("alias.txt")).unwrap();
    let guard = vec!["secret.txt".to_string()];
    assert!(sync::guard_refusal(&t, &guard, ["src/a.rs", "alias.txt"]).is_err());
    assert!(sync::guard_refusal(&t, &guard, ["src/a.rs", "new.txt"]).is_ok());
    let _ = std::fs::remove_dir_all(&t);
}

#[cfg(unix)]
#[test]
fn k8s_a_tree_with_no_commit_still_sends_an_empty_git_root_that_replaces_a_planted_one() {
    let host = tmp("unborn");
    let ok = std::process::Command::new("git")
        .hardened()
        .args(["init", "-q"])
        .current_dir(&host)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("skipping: git init failed");
        return;
    }
    std::fs::write(host.join("a"), "a").unwrap();
    let (roots, _) = seat::plan(
        &host,
        false,
        &[PlanMount::Bind {
            path: host.clone(),
            rw: true,
        }],
        &[],
        &[],
        None,
    );
    let (body, plan, base, _, _) =
        seat::copy_in_stream(None, &host, &roots, true, &Default::default()).unwrap();
    assert!(base.is_none(), "an unborn tree has no base");
    let decoded = sync::decode(&mut body.as_slice(), sync::MAX_STREAM).unwrap();
    assert_eq!(decoded[0].mode, Mode::Git);
    assert!(decoded[0].entries.is_empty());
    let sub = decoded[1].replica.clone();
    let replica = tmp("unborn-replica");
    std::fs::create_dir_all(replica.join(&sub).join(".git/hooks")).unwrap();
    std::fs::write(replica.join(&sub).join(".git/hooks/pre-commit"), "planted").unwrap();
    let out = codec(&replica, &["unpack", &plan.to_string()], &body);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !replica.join(&sub).join(".git/hooks/pre-commit").exists(),
        "a planted hook survived the production copy-in"
    );
    assert_eq!(std::fs::read(replica.join(&sub).join("a")).unwrap(), b"a");
    let _ = std::fs::remove_dir_all(&host);
    let _ = std::fs::remove_dir_all(&replica);
}

#[cfg(unix)]
#[test]
fn k8s_a_first_commit_in_a_tree_with_no_history_is_refused_not_dropped() {
    let replica = tmp("first-commit");
    let w = replica.join("w");
    std::fs::create_dir_all(&w).unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .hardened()
            .args(args)
            .current_dir(&w)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if !git(&["init", "-q"]) {
        eprintln!("skipping: git init failed");
        return;
    }
    let none = codec(&replica, &["bundle", "w", seat::NO_BASE], b"");
    assert!(
        none.status.success(),
        "no commit on either side: nothing to land"
    );
    assert!(none.stdout.is_empty());
    std::fs::write(w.join("a"), "a").unwrap();
    assert!(git(&["add", "a"]));
    assert!(git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "first"
    ]));
    let made = codec(&replica, &["bundle", "w", seat::NO_BASE], b"");
    assert!(
        !made.status.success(),
        "a first commit was silently dropped"
    );
    assert!(String::from_utf8_lossy(&made.stderr).contains("first commit"));
    let _ = std::fs::remove_dir_all(&replica);
}

#[cfg(unix)]
#[test]
fn k8s_a_copy_in_over_its_limit_is_refused_before_it_is_read() {
    let host = tmp("huge");
    let f = std::fs::File::create(host.join("big.bin")).unwrap();
    // Sparse: no disk, but a size over the whole stream's limit.
    f.set_len(sync::MAX_STREAM + 1).unwrap();
    let r = PlanRoot {
        host: host.clone(),
        mode: Mode::Plain,
        skip: Vec::new(),
        back: false,
        files: Vec::new(),
        masks: Vec::new(),
        repo: false,
        ro: false,
        optional: false,
    };
    let e = seat::copy_in_stream(None, &host, &[r], false, &Default::default()).unwrap_err();
    assert!(e.detail.contains("larger than"), "{}", e.detail);
    assert!(
        sync::read_host_entry(&host, "big.bin").is_err(),
        "a file over the stream's limit is never read whole"
    );
    let _ = std::fs::remove_dir_all(&host);
}

#[test]
fn k8s_an_earlier_writer_write_is_fenced_before_a_body_is_taken() {
    let s = gw::write_script(gw::Volume::Config, "config.json", &"a".repeat(64)).unwrap();
    let marker = gw::write_marker(gw::Volume::Config, "config.json");
    assert_eq!(marker, "wicked-writer-W-cfg-config.json-");
    // The kill pattern matches the marker but not the script's own text.
    let pattern = format!("[{}]{}", &marker[..1], &marker[1..]);
    assert!(s.contains(&format!("pgrep -f '{pattern}'")));
    assert!(
        !s.contains(&marker),
        "the script's text would match its own fence"
    );
    let kill = s.find("kill -9").unwrap();
    let wait = s.find("[m]v -f /cfg/.config.json.").unwrap();
    let take = s.find("cat > ").unwrap();
    assert!(kill < wait && wait < take, "fence first, then the body");
    assert!(s.contains("[ \"$p\" = \"$$\" ]"), "never kills itself");
}

#[test]
fn k8s_a_quarantine_owed_by_a_dead_engine_still_rotates_the_boot_token() {
    let dir = tmp("rotate");
    let t1 = gw::boot_token(&dir, false).unwrap();
    // The quarantine recorded its rotation, then its engine died before rotating.
    gw::begin_rotation(&dir).unwrap();
    // A new engine's start: nothing in its memory, the marker on disk.
    let t2 = gw::boot_for_start(&dir, false).unwrap();
    assert_ne!(
        t1, t2,
        "the gateway restarted on the token it was stopped under"
    );
    assert!(!gw::rotation_pending(&dir));
    assert_eq!(
        gw::boot_for_start(&dir, false).unwrap(),
        t2,
        "stable once rotated"
    );
    // Review r4: the host could not record it; the Deployment's annotation still does.
    let t3 = gw::boot_for_start(&dir, true).unwrap();
    assert_ne!(t2, t3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// (core#1722) The CLI RAN: a failed settlement is the engine losing its result, never a seat that
/// could not start (which the failover ladder reads as the seat's own failure).
#[test]
fn k8s_a_failed_settlement_keeps_the_clis_output_as_the_result_lost_frame() {
    let err = format!(
        "{}the write-back was refused: `x` is inside a git dir",
        crate::execute_wrapped::SETTLE_FAILED
    );
    let out = crate::execute_wrapped::settle_failure_output(
        "claude",
        ("t", 0, 0),
        &err,
        "minutes of work",
    );
    assert!(
        out.starts_with(crate::execute_wrapped::RESULT_LOST_PREFIX),
        "{out}"
    );
    assert!(out.contains("inside a git dir"));
    assert!(
        crate::execute_wrapped::spawn_failure_detail(&out).is_none(),
        "{out}"
    );
    assert!(out.ends_with("minutes of work"));
    // Review r4: the CLI's stderr is kept too.
    let err = format!(
        "{}the rc could not be read{}rate limited: retry later",
        crate::execute_wrapped::SETTLE_FAILED,
        crate::execute_wrapped::SETTLE_SEP
    );
    let out = crate::execute_wrapped::settle_failure_output("claude", ("t", 0, 0), &err, "");
    assert!(
        out.starts_with(crate::execute_wrapped::RESULT_LOST_PREFIX),
        "{out}"
    );
    assert!(out.contains("the rc could not be read)"), "{out}");
    assert!(out.ends_with("rate limited: retry later"), "{out}");
}

/// core#1743: the engine's reason is the frame's ONE line, whatever it holds (a codec error
/// carries `\n`), so every reader takes the whole first line as the reason and bounds only the
/// seat's output after it. Red on main: the reason's second line fell into the seat's output.
#[test]
fn k8s_core_1743_a_multi_line_settlement_reason_stays_on_the_frame_line() {
    let err = format!(
        "{}the write-back could not read the replica: wicked-sync: refused\ncommand terminated \
         with exit code 3",
        crate::execute_wrapped::SETTLE_FAILED
    );
    let out =
        crate::execute_wrapped::settle_failure_output("claude", ("t", 0, 0), &err, "seat words");
    let first = out.lines().next().unwrap();
    assert!(
        first.starts_with(crate::execute_wrapped::RESULT_LOST_PREFIX),
        "{out}"
    );
    assert!(
        first.ends_with("command terminated with exit code 3)"),
        "the whole reason is the first line: {out}"
    );
    assert_eq!(out.lines().nth(1), Some("seat words"), "{out}");
}

// ── review r4 folds ──────────────────────────────────────────────────────────────────────────

#[test]
fn k8s_a_quarantine_patches_the_deployment_and_records_the_rotation_there() {
    let p = gw::quarantine_patch();
    assert_eq!(
        p["spec"]["replicas"], 0,
        "stopped by the Deployment itself (no scale verb)"
    );
    let dep = json!({"metadata": {"annotations": {gw::ROTATE_ANNOTATION: "1"}}});
    assert_eq!(p["metadata"], dep["metadata"]);
    assert!(gw::cluster_rotation_pending(Some(&dep)));
    assert!(!gw::cluster_rotation_pending(Some(
        &json!({"metadata": {}})
    )));
    assert!(!gw::cluster_rotation_pending(None));
    // The engine's role may patch deployments (the quarantine's only verb on them).
    let docs = super::cluster::install_docs("wabc");
    let role = docs
        .iter()
        .find(|d| d["kind"] == "ClusterRole" && d["rules"].to_string().contains("\"deployments\""))
        .expect("the engine's role");
    let rule = role["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| {
            r["resources"] == json!(["deployments"]) && r["verbs"].as_array().unwrap().len() > 1
        })
        .expect("the deployments rule");
    assert!(rule["verbs"].as_array().unwrap().contains(&json!("patch")));
}

#[cfg(unix)]
#[test]
fn k8s_links_in_two_roots_cannot_combine_into_an_escape() {
    let root = tmp("xroot");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let plain = |host: &Path, skip: Vec<String>| PlanRoot {
        host: host.to_path_buf(),
        mode: Mode::Plain,
        skip,
        back: true,
        files: Vec::new(),
        masks: Vec::new(),
        repo: false,
        ro: false,
        optional: false,
    };
    let outer = plain(&root, vec!["sub".into()]);
    let inner = plain(&root.join("sub"), Vec::new());
    let plans: Vec<seat::RootPlan<'_>> = vec![
        (
            &outer,
            vec![entry(Kind::Link, "escape", b"sub/x/../../outside")],
            Vec::new(),
        ),
        (&inner, vec![entry(Kind::Link, "x", b".")], Vec::new()),
    ];
    assert!(
        seat::preflight(&plans).is_err(),
        "a link led into a nested root"
    );
    assert!(!root.join("escape").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn k8s_a_host_link_the_write_back_replaces_is_not_followed() {
    let root = tmp("replaced-link");
    std::os::unix::fs::symlink(".", root.join("a")).unwrap();
    let r = PlanRoot {
        host: root.clone(),
        mode: Mode::Plain,
        skip: Vec::new(),
        back: true,
        files: Vec::new(),
        masks: Vec::new(),
        repo: false,
        ro: false,
        optional: false,
    };
    // `a` becomes a directory; `b -> a/../out` then resolves to `out`, inside the root.
    let plans: Vec<seat::RootPlan<'_>> = vec![(
        &r,
        vec![
            entry(Kind::File, "a/c", b"c"),
            entry(Kind::Link, "b", b"a/../out"),
        ],
        vec!["a".into()],
    )];
    assert!(
        seat::preflight(&plans).is_ok(),
        "{:?}",
        seat::preflight(&plans)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn k8s_a_spelling_change_never_deletes_the_file() {
    let host = tmp("respell");
    std::fs::write(host.join("a"), "X").unwrap();
    let base: std::collections::HashMap<String, [u8; 32]> =
        [("a".to_string(), sync::digest(&entry(Kind::File, "a", b"X")))].into();
    let folds = host.join("A").exists();
    let (writes, deletes) = seat::delta(&host, &base, vec![entry(Kind::File, "A", b"X")]).unwrap();
    if folds {
        assert!(deletes.is_empty(), "the only copy was deleted: {deletes:?}");
        assert!(writes.is_empty());
    } else {
        assert_eq!(writes.len(), 1);
        assert_eq!(deletes, vec!["a".to_string()]);
    }
    // Renamed and edited: no false conflict.
    let (writes, _) = seat::delta(&host, &base, vec![entry(Kind::File, "A", b"Y")]).unwrap();
    assert_eq!(writes.len(), 1);
    let _ = std::fs::remove_dir_all(&host);
}

#[test]
fn k8s_a_file_and_a_directory_of_one_folded_name_are_refused_however_they_sort() {
    let r = root(vec![
        entry(Kind::File, "A", b"a"),
        entry(Kind::File, "A-", b"b"),
        entry(Kind::File, "a/b", b"c"),
    ]);
    assert!(sync::check_entries(&r, &[]).is_err());
}

/// (core#1498, run ea75313b seq 6215-6217) An untracked file of a git tree that would take the
/// write-back past the stream's limit is LEFT OUT, never read and never the whole write-back refused:
/// the seat's other files still come back, and the stream names what stayed behind.
#[cfg(unix)]
#[test]
fn k8s_the_pod_codec_leaves_an_oversized_untracked_file_out_before_reading_it() {
    let replica = tmp("bigpack");
    let t = replica.join("w");
    std::fs::create_dir_all(&t).unwrap();
    git_in(&t, &["init", "-q"]);
    let f = std::fs::File::create(t.join("big")).unwrap();
    f.set_len(sync::MAX_STREAM + 1).unwrap();
    std::fs::write(t.join("notes.md"), "n").unwrap();
    std::fs::create_dir_all(replica.join("e")).unwrap();
    std::fs::write(replica.join("e/verdict.json"), "{}").unwrap();
    let plan = json!([["w", "tree", [".git"], []], ["e", "plain", [], []]]).to_string();
    let out = codec(&replica, &["pack", &plan], br#"{"w": []}"#);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut roots = sync::decode(&mut out.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    let left = sync::take_skipped(&mut roots[0]).unwrap();
    assert_eq!(
        left,
        vec![sync::Skipped {
            path: "big".into(),
            why: "limit".into()
        }]
    );
    let paths: Vec<&str> = roots[0].entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["notes.md"], "the seat's other output comes back");
    let paths: Vec<&str> = roots[1].entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["verdict.json"], "the evidence comes back");
    assert!(left[0].describe(Path::new("/h/w")).contains("stream limit"));
    assert!(sync::POD_CODEC.contains(&format!("MAX = {}", sync::MAX_STREAM)));
    let _ = std::fs::remove_dir_all(&replica);
}

/// (core#1535 item 1) A seat's tag proves nothing in a root with no host repository.
#[cfg(unix)]
#[test]
fn k8s_1535_a_repository_free_root_never_honors_a_planted_tag() {
    let replica = tmp("1535-tag");
    let p = replica.join("p");
    std::fs::create_dir_all(p.join("out")).unwrap();
    std::fs::write(
        p.join("out/CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();
    std::fs::write(p.join("out/verdict.json"), "FAIL\n").unwrap();
    let left = [sync::Skipped {
        path: "out".into(),
        why: "cache".into(),
    }];
    for mode in [Mode::Plain, Mode::Tree] {
        let plan = json!([["p", mode.as_str(), [], []]]).to_string();
        let out = codec(&replica, &["pack", &plan], b"{}");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut roots = sync::decode(&mut out.stdout.as_slice(), sync::MAX_STREAM).unwrap();
        assert!(sync::take_skipped(&mut roots[0]).unwrap().is_empty());
        let paths: Vec<&str> = roots[0].entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["out/CACHEDIR.TAG", "out/verdict.json"]);
        let err = sync::refuse_unsafe_skips(&p, mode, &left, None).unwrap_err();
        assert!(
            err.contains("out") && err.contains("no repository on the host"),
            "{err}"
        );
    }
    // A plain repository still skips untracked output, protecting its tracked files too.
    git_in(&p, &["init", "-q"]);
    let plan = json!([["p", "plain", [], []]]).to_string();
    let out = codec(&replica, &["pack", &plan], br#"{"p": []}"#);
    assert!(out.status.success(), "{:?}", out.stderr);
    let mut roots = sync::decode(&mut out.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    assert_eq!(sync::take_skipped(&mut roots[0]).unwrap(), left);
    sync::refuse_unsafe_skips(&p, Mode::Plain, &left, None).unwrap();
    git_in(&p, &["add", "out/verdict.json"]);
    let out = codec(
        &replica,
        &["pack", &plan],
        br#"{"p": ["out/verdict.json"]}"#,
    );
    assert!(out.status.success(), "{:?}", out.stderr);
    let mut roots = sync::decode(&mut out.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    assert!(sync::take_skipped(&mut roots[0]).unwrap().is_empty());
    assert!(roots[0]
        .entries
        .iter()
        .any(|e| e.path == "out/verdict.json" && e.data == b"FAIL\n"));
    let err = sync::refuse_unsafe_skips(&p, Mode::Plain, &left, None).unwrap_err();
    assert!(err.contains("out/verdict.json"), "{err}");
    // Its snapshot never makes a plain root's untracked files leavable for the limit.
    std::fs::File::create(p.join("untracked.bin"))
        .unwrap()
        .set_len(sync::MAX_STREAM + 1)
        .unwrap();
    let out = codec(&replica, &["pack", &plan], br#"{"p": []}"#);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && err.contains("p/untracked.bin"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&replica);
}

/// (core#1535 item 2) The installed tip's tracked paths count even before they reach the index.
#[cfg(unix)]
#[test]
fn k8s_1535_a_skip_over_a_file_the_incoming_commit_tracks_is_refused() {
    let t = tmp("1535-incoming");
    git_in(&t, &["init", "-q"]);
    git_in(&t, &["commit", "-q", "--allow-empty", "-m", "base"]);
    git_in(&t, &["checkout", "-q", "-b", "incoming"]);
    std::fs::create_dir_all(t.join("src")).unwrap();
    std::fs::write(t.join("src/new.rs"), "new\n").unwrap();
    git_in(&t, &["add", "src/new.rs"]);
    git_in(&t, &["commit", "-q", "-m", "incoming"]);
    let incoming = git_out(&t, &["rev-parse", "HEAD"]);
    git_in(&t, &["checkout", "-q", "--detach", "HEAD~1"]);
    for (path, why) in [("src", "cache"), ("src/new.rs", "limit")] {
        let left = [sync::Skipped {
            path: path.into(),
            why: why.into(),
        }];
        sync::refuse_unsafe_skips(&t, Mode::Tree, &left, None).unwrap();
        let err = sync::refuse_unsafe_skips(&t, Mode::Tree, &left, Some(&incoming)).unwrap_err();
        assert!(
            err.contains("src/new.rs") && err.contains("tracks"),
            "{err}"
        );
        let err =
            sync::refuse_unsafe_skips(&t, Mode::Tree, &left, Some("missing-1535-tip")).unwrap_err();
        assert!(err.contains("ls-tree"), "{err}");
    }
    std::fs::write(t.join("local.rs"), "index only\n").unwrap();
    git_in(&t, &["add", "local.rs"]);
    let left = [sync::Skipped {
        path: "local.rs".into(),
        why: "cache".into(),
    }];
    let err = sync::refuse_unsafe_skips(&t, Mode::Tree, &left, Some(&incoming)).unwrap_err();
    assert!(err.contains("local.rs"), "{err}");
    // ls-tree, like ls-files, names paths relative to the checked root.
    let src = t.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let left = [sync::Skipped {
        path: "new.rs".into(),
        why: "limit".into(),
    }];
    let err = sync::refuse_unsafe_skips(&src, Mode::Tree, &left, Some(&incoming)).unwrap_err();
    assert!(err.contains("`new.rs`") && err.contains("tracks"), "{err}");
    let _ = std::fs::remove_dir_all(&t);
}

/// (core#1535 item 3) A folded keep brings the host link into the retained-link check.
#[cfg(unix)]
#[test]
fn k8s_1535_a_link_kept_by_a_folded_alias_joins_the_retained_link_check() {
    let host = tmp("1535-alias-link");
    std::fs::create_dir_all(host.join("src")).unwrap();
    std::os::unix::fs::symlink("../a/..", host.join("src/b")).unwrap();
    let base = [(
        "src/b".into(),
        sync::digest(&entry(Kind::Link, "src/b", b"../a/..")),
    )]
    .into_iter()
    .collect();
    let left = [sync::Skipped {
        path: "s\u{200c}rc".into(),
        why: "cache".into(),
    }];
    let skips = sync::SkipSet::new(&left);
    let aliased = seat::kept_by_alias(&base, &[], &skips);
    assert_eq!(aliased, ["src/b"]);
    let links = seat::left_out_links(&host, &left, &aliased).unwrap();
    assert_eq!(links.len(), 1, "{links:?}");
    assert_eq!(links[0].path, "src/b");
    assert_eq!(links[0].data, b"../a/..");
    let (writes, deletes) =
        seat::delta_keeping(&host, &base, Vec::new(), &skips, &|| false).unwrap();
    assert!(writes.is_empty() && deletes.is_empty());
    let sent = [entry(Kind::Link, "src/b", b"../a/..")];
    assert!(seat::kept_by_alias(&base, &sent, &skips).is_empty());
    let exact = sync::SkipSet::new(&[sync::Skipped {
        path: "src".into(),
        why: "cache".into(),
    }]);
    assert!(seat::kept_by_alias(&base, &[], &exact).is_empty());
    let _ = std::fs::remove_dir_all(&host);
}

/// (core#1535 item 4) The replica's index and ignore rules cannot hide a host-tracked edit.
#[cfg(unix)]
#[test]
fn k8s_1535_a_host_tracked_file_the_seat_untracked_and_ignored_comes_back() {
    let replica = tmp("1535-ignored");
    let w = replica.join("w");
    std::fs::create_dir_all(&w).unwrap();
    git_in(&w, &["init", "-q"]);
    std::fs::write(w.join("a.txt"), "old\n").unwrap();
    git_in(&w, &["add", "a.txt"]);
    git_in(&w, &["commit", "-q", "-m", "base"]);
    std::fs::write(w.join("a.txt"), "edited\n").unwrap();
    git_in(&w, &["rm", "-q", "--cached", "a.txt"]);
    std::fs::write(w.join(".gitignore"), "a.txt\n").unwrap();
    let plan = json!([["w", "tree", [".git"], []]]).to_string();
    let out = codec(&replica, &["pack", &plan], br#"{"w": ["a.txt"]}"#);
    assert!(out.status.success(), "{:?}", out.stderr);
    let mut roots = sync::decode(&mut out.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    assert!(sync::take_skipped(&mut roots[0]).unwrap().is_empty());
    assert!(roots[0]
        .entries
        .iter()
        .any(|e| e.path == "a.txt" && e.data == b"edited\n"));
    let _ = std::fs::remove_dir_all(&replica);
}

/// (core#1535 item 5) Must-file sizing includes the markers, so the refusal names a file.
#[cfg(unix)]
#[test]
fn k8s_1535_the_limit_refusal_names_a_file_not_the_end_marker() {
    let replica = tmp("1535-end");
    std::fs::create_dir_all(replica.join("p")).unwrap();
    std::fs::write(replica.join("p/a"), "a").unwrap();
    // The root (19) and file (15) fit exactly; the END (13) does not.
    let n = 34;
    assert!(sync::POD_CODEC.contains("MAX = 536870912"));
    let program = sync::POD_CODEC.replace("MAX = 536870912", &format!("MAX = {n}"));
    let plan = json!([["p", "plain", [], []]]).to_string();
    let out = codec_program(&replica, &["pack", &plan], b"{}", "-", &program);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("p/a") && !err.contains("at ''"), "{err}");
    let _ = std::fs::remove_dir_all(&replica);
}

/// (core#1518 codex-check, W 20) The limit only ever leaves out an untracked file of a git tree: an
/// untracked artifact that fills the budget never pushes out a tracked edit or the QE evidence (the
/// codex repro: a 23-byte tracked change and a 19-byte verdict after a ~448 MiB artifact were both
/// left out while the pack exited 0). A small new file still comes back (smallest first).
#[cfg(unix)]
#[test]
fn k8s_the_pod_codec_never_leaves_a_tracked_edit_or_evidence_out_for_an_artifact() {
    let replica = tmp("budgetpack");
    let t = replica.join("w");
    std::fs::create_dir_all(&t).unwrap();
    git_in(&t, &["init", "-q"]);
    std::fs::write(t.join("z_source.rs"), "old\n").unwrap();
    git_in(&t, &["add", "z_source.rs"]);
    std::fs::write(t.join("z_source.rs"), "new source should land\n").unwrap();
    std::fs::write(t.join("zz_new.rs"), "fn new() {}\n").unwrap();
    let f = std::fs::File::create(t.join("a_generated.bin")).unwrap();
    // Sparse: fills the budget less what the two small files take (the codex repro's size).
    f.set_len((448 << 20) - 46).unwrap();
    std::fs::create_dir_all(replica.join("e")).unwrap();
    std::fs::write(replica.join("e/verdict.json"), "{\"verdict\":\"FAIL\"}\n").unwrap();
    let plan = json!([["w", "tree", [".git"], []], ["e", "plain", [], []]]).to_string();
    let out = codec(&replica, &["pack", &plan], br#"{"w": ["z_source.rs"]}"#);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut roots = sync::decode(&mut out.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    let left = sync::take_skipped(&mut roots[0]).unwrap();
    assert_eq!(
        left,
        vec![sync::Skipped {
            path: "a_generated.bin".into(),
            why: "limit".into()
        }]
    );
    let got: Vec<(&str, &[u8])> = roots[0]
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.data.as_slice()))
        .collect();
    assert_eq!(
        got,
        [
            ("z_source.rs", &b"new source should land\n"[..]),
            ("zz_new.rs", &b"fn new() {}\n"[..])
        ]
    );
    let got: Vec<(&str, &[u8])> = roots[1]
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.data.as_slice()))
        .collect();
    assert_eq!(got, [("verdict.json", &b"{\"verdict\":\"FAIL\"}\n"[..])]);
    let _ = std::fs::remove_dir_all(&replica);
}

/// (core#1518 codex-check) A tracked file or a plain root's file (QE evidence) that cannot fit is
/// never left out: the write-back is refused, loudly, naming it, never a success without it.
#[cfg(unix)]
#[test]
fn k8s_the_pod_codec_refuses_a_write_back_whose_tracked_files_or_evidence_cannot_fit() {
    let replica = tmp("mustpack");
    let t = replica.join("w");
    std::fs::create_dir_all(&t).unwrap();
    git_in(&t, &["init", "-q"]);
    std::fs::write(t.join("data.bin"), "old").unwrap();
    git_in(&t, &["add", "data.bin"]);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(t.join("data.bin"))
        .unwrap();
    f.set_len(sync::MAX_STREAM + 1).unwrap();
    // The replica index cannot make a host-tracked file eligible for a limit skip.
    git_in(&t, &["rm", "-q", "-f", "--cached", "data.bin"]);
    let plan = json!([["w", "tree", [".git"], []]]).to_string();
    let out = codec(&replica, &["pack", &plan], br#"{"w": ["data.bin"]}"#);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a tracked file was left out silently"
    );
    assert!(
        err.contains("w/data.bin") && err.contains("nothing was written back"),
        "{err}"
    );
    std::fs::create_dir_all(replica.join("e")).unwrap();
    let f = std::fs::File::create(replica.join("e/evidence.log")).unwrap();
    f.set_len(sync::MAX_STREAM + 1).unwrap();
    let plan = json!([["e", "plain", [], []]]).to_string();
    let out = codec(&replica, &["pack", &plan], b"{}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "evidence was left out silently");
    assert!(err.contains("e/evidence.log"), "{err}");
    let _ = std::fs::remove_dir_all(&replica);
}

/// (core#1518 codex-check, W 10) A tagged dir whose name folds to a tracked file's dir (`s<ZWNJ>rc`
/// against `src`) is no cache: it never hides tracked `src/file.rs` (the host would delete it).
/// And a cache dir covers exactly its own paths: `cache/x.txt` beside a tagged `c<ZWNJ>ache` comes back.
#[cfg(unix)]
#[test]
fn k8s_the_pod_codec_matches_cache_dirs_exactly_and_never_over_tracked_files() {
    const SIG: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n";
    let replica = tmp("aliaspack");
    let t = replica.join("w");
    std::fs::create_dir_all(t.join("src")).unwrap();
    git_in(&t, &["init", "-q"]);
    std::fs::write(t.join("src/file.rs"), "old\n").unwrap();
    git_in(&t, &["add", "src/file.rs"]);
    std::fs::write(t.join("src/file.rs"), "changed\n").unwrap();
    std::fs::create_dir_all(t.join("s\u{200c}rc")).unwrap();
    std::fs::write(t.join("s\u{200c}rc/CACHEDIR.TAG"), SIG).unwrap();
    std::fs::create_dir_all(t.join("c\u{200c}ache")).unwrap();
    std::fs::write(t.join("c\u{200c}ache/CACHEDIR.TAG"), SIG).unwrap();
    std::fs::write(t.join("c\u{200c}ache/obj.o"), "obj").unwrap();
    std::fs::create_dir_all(t.join("cache")).unwrap();
    std::fs::write(t.join("cache/x.txt"), "x").unwrap();
    let plan = json!([["w", "tree", [".git"], []]]).to_string();
    let out = codec(&replica, &["pack", &plan], br#"{"w": ["src/file.rs"]}"#);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut roots = sync::decode(&mut out.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    let left = sync::take_skipped(&mut roots[0]).unwrap();
    assert_eq!(
        left,
        vec![sync::Skipped {
            path: "c\u{200c}ache".into(),
            why: "cache".into()
        }]
    );
    let mut paths: Vec<&str> = roots[0].entries.iter().map(|e| e.path.as_str()).collect();
    paths.sort();
    assert_eq!(
        paths,
        ["cache/x.txt", "src/file.rs", "s\u{200c}rc/CACHEDIR.TAG"]
    );
    // The host half (codex's host probe): the edit is written, nothing is deleted.
    let host = tmp("aliashost");
    std::fs::create_dir_all(host.join("src")).unwrap();
    std::fs::write(host.join("src/file.rs"), "old\n").unwrap();
    let base: std::collections::HashMap<String, [u8; 32]> = [(
        "src/file.rs".to_string(),
        sync::digest(&entry(Kind::File, "src/file.rs", b"old\n")),
    )]
    .into_iter()
    .collect();
    let mut entries = sync::check_entries_against(&roots[0], &[], &base).unwrap();
    entries.retain(|e| !left.iter().any(|s| s.covers(&e.path)));
    let (writes, deletes) =
        seat::delta_keeping(&host, &base, entries, &sync::SkipSet::new(&left), &|| false).unwrap();
    assert!(deletes.is_empty(), "{deletes:?}");
    assert!(
        writes
            .iter()
            .any(|e| e.path == "src/file.rs" && e.data == b"changed\n"),
        "the tracked edit lands"
    );
    let _ = std::fs::remove_dir_all(&host);
    let _ = std::fs::remove_dir_all(&replica);
}

/// (core#1498) Build output never comes back: a directory holding a cache-directory tag (cargo writes
/// one into every target dir, whatever its name: `target/`, `baseline-target/`, a CARGO_TARGET_DIR in
/// a written-back root) is named once as left out and none of it is packed, in a plain repository
/// and in an untracked, unignored dir of a git tree. A file NAMED `CACHEDIR.TAG` without the tag's
/// signature marks nothing.
#[cfg(unix)]
#[test]
fn k8s_the_pod_codec_leaves_tagged_build_output_out() {
    const SIG: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n# cargo\n";
    let replica = tmp("cachepack");
    // (core#1535 item 1) A plain repository with a CARGO_TARGET_DIR inside it.
    let plain = replica.join("p");
    std::fs::create_dir_all(plain.join("cargo-target/debug/build")).unwrap();
    git_in(&plain, &["init", "-q"]);
    std::fs::write(plain.join("cargo-target/CACHEDIR.TAG"), SIG).unwrap();
    std::fs::write(plain.join("cargo-target/debug/build/parser.o"), "obj").unwrap();
    std::fs::create_dir_all(plain.join("cargo-target/inner")).unwrap();
    std::fs::write(plain.join("cargo-target/inner/CACHEDIR.TAG"), SIG).unwrap();
    std::fs::write(plain.join("notes.md"), "n").unwrap();
    std::fs::create_dir_all(plain.join("fake")).unwrap();
    std::fs::write(plain.join("fake/CACHEDIR.TAG"), "not a tag").unwrap();
    // A git tree with an untracked, unignored baseline-target/ (run ea75313b's shape).
    let t = replica.join("w");
    std::fs::create_dir_all(&t).unwrap();
    git_in(&t, &["init", "-q"]);
    std::fs::write(t.join("lib.rs"), "a").unwrap();
    git_in(&t, &["add", "lib.rs"]);
    git_in(&t, &["commit", "-q", "-m", "a"]);
    std::fs::write(t.join("lib.rs"), "b").unwrap();
    std::fs::create_dir_all(t.join("baseline-target/debug")).unwrap();
    std::fs::write(t.join("baseline-target/CACHEDIR.TAG"), SIG).unwrap();
    std::fs::write(t.join("baseline-target/debug/x.o"), "obj").unwrap();
    let plan = json!([["p", "plain", [], []], ["w", "tree", [".git"], []]]).to_string();
    let out = codec(&replica, &["pack", &plan], br#"{"p": [], "w": ["lib.rs"]}"#);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut roots = sync::decode(&mut out.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    let left_p = sync::take_skipped(&mut roots[0]).unwrap();
    assert_eq!(
        left_p,
        vec![sync::Skipped {
            path: "cargo-target".into(),
            why: "cache".into()
        }]
    );
    let mut paths: Vec<&str> = roots[0].entries.iter().map(|e| e.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, ["fake/CACHEDIR.TAG", "notes.md"]);
    let left_w = sync::take_skipped(&mut roots[1]).unwrap();
    assert_eq!(
        left_w,
        vec![sync::Skipped {
            path: "baseline-target".into(),
            why: "cache".into()
        }]
    );
    let paths: Vec<&str> = roots[1].entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["lib.rs"], "the seat's real change still comes back");
    assert!(left_w[0].describe(&t).contains("build output"));
    let _ = std::fs::remove_dir_all(&replica);
}

/// (core#1518 codex-check and confirm, SILENT-OK / KEY / HIDE) The host's own check of what the
/// pod left out, against what no seat can change: a `limit` skip only in a git tree, and no skip
/// covering a file the host's repository tracks, folded. Each refuses the write-back, naming it.
#[cfg(unix)]
#[test]
fn k8s_the_host_refuses_a_skip_over_tracked_work_or_outside_a_git_tree() {
    let sk = |p: &str, why: &str| sync::Skipped {
        path: p.into(),
        why: why.into(),
    };
    // A plain root (the QE evidence) and a repository-free tree never lose a file to the limit.
    let plain = tmp("skip-plain");
    for mode in [Mode::Plain, Mode::Files, Mode::Tree] {
        let err = sync::refuse_unsafe_skips(&plain, mode, &[sk("verdict.json", "limit")], None)
            .unwrap_err();
        assert!(
            err.contains("verdict.json") && err.contains("nothing was written back"),
            "{mode:?}: {err}"
        );
    }
    let err = sync::refuse_unsafe_skips(&plain, Mode::Plain, &[sk("cargo-target", "cache")], None)
        .unwrap_err();
    assert!(err.contains("no repository on the host"), "{err}");
    // A git tree: an untracked file may be left out; tracked work never, through any alias.
    let t = tmp("skip-tree");
    git_in(&t, &["init", "-q"]);
    std::fs::create_dir_all(t.join("src")).unwrap();
    std::fs::write(t.join("src/file.rs"), "old\n").unwrap();
    std::fs::write(t.join("lib"), "a tracked file named like a dir\n").unwrap();
    git_in(&t, &["add", "src/file.rs", "lib"]);
    git_in(&t, &["commit", "-q", "-m", "a"]);
    sync::refuse_unsafe_skips(
        &t,
        Mode::Tree,
        &[
            sk("a_generated.bin", "limit"),
            sk("baseline-target", "cache"),
        ],
        None,
    )
    .unwrap();
    for (bad, why) in [
        ("src/file.rs", "limit"),
        ("SRC/file.rs", "limit"),
        ("src", "cache"),
        ("s\u{200c}rc", "cache"),
        ("SRC", "cache"),
        ("lib", "cache"),
    ] {
        let err = sync::refuse_unsafe_skips(&t, Mode::Tree, &[sk(bad, why)], None).unwrap_err();
        assert!(err.contains("tracks"), "{bad} {why}: {err}");
    }
    // An additional writable repository is a plain root: its tracked files are protected too.
    let err = sync::refuse_unsafe_skips(&t, Mode::Plain, &[sk("src", "cache")], None).unwrap_err();
    assert!(err.contains("src/file.rs"), "{err}");
    // (codex confirm r2) A repository NESTED in a plain root answers for its own tracked files:
    // cargo's tag in `extra/target` (tracked fixture) or a tag at the nested repository's root.
    let outer = tmp("skip-nested");
    // (core#1535 item 1) Keep exercising the nested checks, beyond the repository-free gate.
    git_in(&outer, &["init", "-q"]);
    let extra = outer.join("extra");
    std::fs::create_dir_all(extra.join("target")).unwrap();
    git_in(&extra, &["init", "-q"]);
    std::fs::write(extra.join("target/fixture.bin"), "f").unwrap();
    git_in(&extra, &["add", "target/fixture.bin"]);
    git_in(&extra, &["commit", "-q", "-m", "a"]);
    std::fs::create_dir_all(outer.join("scratch-target")).unwrap();
    for bad in ["extra/target", "extra", "extra/TARGET"] {
        let err =
            sync::refuse_unsafe_skips(&outer, Mode::Plain, &[sk(bad, "cache")], None).unwrap_err();
        assert!(err.contains("extra/target/fixture.bin"), "{bad}: {err}");
    }
    sync::refuse_unsafe_skips(&outer, Mode::Plain, &[sk("scratch-target", "cache")], None).unwrap();
    sync::refuse_unsafe_skips(
        &outer,
        Mode::Plain,
        &[sk("extra/new-target", "cache")],
        None,
    )
    .unwrap();
    let _ = std::fs::remove_dir_all(&outer);
    // The seat rewrote its own index (`git rm --cached`) and planted a tag beside its edit: the
    // host's copy-in tracked set protects the edit even after the pod index loses it.
    let replica = tmp("skip-index");
    let w = replica.join("w");
    std::fs::create_dir_all(w.join("src")).unwrap();
    git_in(&w, &["init", "-q"]);
    std::fs::write(w.join("src/file.rs"), "old\n").unwrap();
    git_in(&w, &["add", "src/file.rs"]);
    git_in(&w, &["commit", "-q", "-m", "a"]);
    std::fs::write(w.join("src/file.rs"), "the seat's edit\n").unwrap();
    git_in(&w, &["rm", "-q", "-r", "--cached", "src"]);
    std::fs::write(
        w.join("src/CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();
    let plan = json!([["w", "tree", [".git"], []]]).to_string();
    let tracked = json!({"w": ["src/file.rs"]}).to_string();
    let out = codec(&replica, &["pack", &plan], tracked.as_bytes());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut roots = sync::decode(&mut out.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    let left = sync::take_skipped(&mut roots[0]).unwrap();
    assert!(left.is_empty(), "the host's tracked paths are protected");
    assert!(roots[0]
        .entries
        .iter()
        .any(|e| e.path == "src/file.rs" && e.data == b"the seat's edit\n"));
    sync::refuse_unsafe_skips(&t, Mode::Tree, &left, None).unwrap();
    // A path a skip may alias on a folding host is never deleted (`src/file.rs` vs a skipped
    // `SRC/file.rs`), while a path the pod really dropped still is.
    let base: std::collections::HashMap<String, [u8; 32]> = ["src/file.rs", "lib"]
        .iter()
        .map(|r| {
            let d = std::fs::read(t.join(r)).unwrap();
            (r.to_string(), sync::digest(&entry(Kind::File, r, &d)))
        })
        .collect();
    let (_, deletes) = seat::delta_keeping(
        &t,
        &base,
        Vec::new(),
        &sync::SkipSet::new(&[sk("SRC/file.rs", "limit")]),
        &|| false,
    )
    .unwrap();
    assert_eq!(deletes, vec!["lib".to_string()]);
    for d in [plain, t, replica] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// (core#1518 codex-check b, W20) A dir is skipped only when it holds no `.git` entry at any
/// depth and no file any repository tracks: a repository nested BELOW a tagged dir (the codex
/// repro), one BESIDE it (the skip inside another repository), and a submodule each refuse on the
/// host, naming the path; in the pod a tagged dir holding a repository is no cache at all.
#[cfg(unix)]
#[test]
fn k8s_a_skip_never_covers_a_repository_below_beside_or_a_submodule() {
    const SIG: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n";
    let sk = |p: &str| sync::Skipped {
        path: p.into(),
        why: "cache".into(),
    };
    // Below: plain root, `build/CACHEDIR.TAG` + repository `build/extra` tracking `file.rs`.
    let replica = tmp("git-below");
    let host = replica.join("p");
    let extra = host.join("build/extra");
    std::fs::create_dir_all(&extra).unwrap();
    git_in(&host, &["init", "-q"]);
    std::fs::write(host.join("build/CACHEDIR.TAG"), SIG).unwrap();
    git_in(&extra, &["init", "-q"]);
    std::fs::write(extra.join("file.rs"), "old\n").unwrap();
    git_in(&extra, &["add", "file.rs"]);
    git_in(&extra, &["commit", "-q", "-m", "a"]);
    for bad in ["build", "BUILD", "b\u{200c}uild"] {
        let err = sync::refuse_unsafe_skips(&host, Mode::Plain, &[sk(bad)], None).unwrap_err();
        assert!(
            err.contains("extra/.git") && err.contains("nothing was written back"),
            "{bad}: {err}"
        );
    }
    // The pod never calls it a cache: the nested repository's edit comes back.
    std::fs::write(extra.join("file.rs"), "edited\n").unwrap();
    let plan = json!([["p", "plain", [], []]]).to_string();
    let out = codec(&replica, &["pack", &plan], br#"{"p": []}"#);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut roots = sync::decode(&mut out.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    assert!(sync::take_skipped(&mut roots[0]).unwrap().is_empty());
    assert!(roots[0]
        .entries
        .iter()
        .any(|e| e.path == "build/extra/file.rs" && e.data == b"edited\n"));
    // Beside: the skip sits inside another repository that tracks a file under it.
    let beside = host.join("build/extra");
    std::fs::create_dir_all(beside.join("target")).unwrap();
    std::fs::write(beside.join("target/fixture.bin"), "f").unwrap();
    git_in(&beside, &["add", "target/fixture.bin"]);
    git_in(&beside, &["commit", "-q", "-m", "b"]);
    let err = sync::refuse_unsafe_skips(&host, Mode::Plain, &[sk("build/extra/target")], None)
        .unwrap_err();
    assert!(err.contains("build/extra/target/fixture.bin"), "{err}");
    // A submodule of the tree: the dir itself, and a tagged dir inside it holding its tracked file.
    let tree = tmp("git-sub");
    git_in(&tree, &["init", "-q"]);
    std::fs::write(tree.join("lib.rs"), "a").unwrap();
    git_in(&tree, &["add", "lib.rs"]);
    git_in(&tree, &["commit", "-q", "-m", "a"]);
    git_in(
        &tree,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &extra.to_string_lossy(),
            "sub",
        ],
    );
    for bad in ["sub", "sub/target"] {
        let err = sync::refuse_unsafe_skips(&tree, Mode::Tree, &[sk(bad)], None).unwrap_err();
        assert!(err.contains("nothing was written back"), "{bad}: {err}");
    }
    sync::refuse_unsafe_skips(&tree, Mode::Tree, &[sk("baseline-target")], None).unwrap();
    // (confirm 3, HIDE) A closer repository never masks an enclosing one: `r` tracks
    // `inner/target/file.rs`, `r/inner` is a repository that does not.
    let enc = tmp("git-enclosing");
    git_in(&enc, &["init", "-q"]);
    let r = enc.join("r");
    std::fs::create_dir_all(r.join("inner/target")).unwrap();
    git_in(&r, &["init", "-q"]);
    std::fs::write(r.join("inner/target/file.rs"), "old\n").unwrap();
    git_in(&r, &["add", "inner/target/file.rs"]);
    git_in(&r, &["commit", "-q", "-m", "a"]);
    git_in(&r.join("inner"), &["init", "-q"]);
    std::fs::write(r.join("inner/target/CACHEDIR.TAG"), SIG).unwrap();
    let err =
        sync::refuse_unsafe_skips(&enc, Mode::Plain, &[sk("r/inner/target")], None).unwrap_err();
    assert!(err.contains("r/inner/target/file.rs"), "{err}");
    // (confirm 3, KEY) A folded alias of a repository is asked too (`R` for `r`), and an exact
    // dir never hides a folded sibling holding a repository (`build` beside `b<ZWNJ>uild`).
    let err =
        sync::refuse_unsafe_skips(&enc, Mode::Plain, &[sk("R/inner/target")], None).unwrap_err();
    assert!(err.contains("file.rs"), "{err}");
    std::fs::create_dir_all(enc.join("build")).unwrap();
    std::fs::create_dir_all(enc.join("b\u{200c}uild/extra")).unwrap();
    git_in(&enc.join("b\u{200c}uild/extra"), &["init", "-q"]);
    let err = sync::refuse_unsafe_skips(&enc, Mode::Plain, &[sk("build")], None).unwrap_err();
    assert!(err.contains("extra/.git"), "{err}");
    for d in [replica, tree, enc] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// (core#1518 codex confirm #2, #4) A repository-free tree's files all come back (none is
/// "untracked output"): past the limit the pack is refused, never a success without them. A tracked
/// FILE replaced by a tagged dir of its name is no cache.
#[cfg(unix)]
#[test]
fn k8s_the_pod_codec_never_leaves_out_a_repository_free_trees_files_or_a_tracked_leaf() {
    let replica = tmp("norepo");
    let t = replica.join("w");
    std::fs::create_dir_all(&t).unwrap();
    let f = std::fs::File::create(t.join("result.bin")).unwrap();
    f.set_len(sync::MAX_STREAM + 1).unwrap();
    let plan = json!([["w", "tree", [".git"], []]]).to_string();
    let out = codec(&replica, &["pack", &plan], b"{}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a repository-free tree's file was left out"
    );
    assert!(
        err.contains("w/result.bin") && err.contains("nothing was written back"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&replica);

    let replica = tmp("leaf");
    let t = replica.join("w");
    std::fs::create_dir_all(&t).unwrap();
    git_in(&t, &["init", "-q"]);
    std::fs::write(t.join("src"), "a file\n").unwrap();
    git_in(&t, &["add", "src"]);
    git_in(&t, &["commit", "-q", "-m", "a"]);
    std::fs::remove_file(t.join("src")).unwrap();
    std::fs::create_dir_all(t.join("src")).unwrap();
    std::fs::write(
        t.join("src/CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();
    std::fs::write(t.join("src/new.rs"), "fn n() {}\n").unwrap();
    let out = codec(&replica, &["pack", &plan], br#"{"w": ["src"]}"#);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut roots = sync::decode(&mut out.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    assert!(sync::take_skipped(&mut roots[0]).unwrap().is_empty());
    let mut paths: Vec<&str> = roots[0].entries.iter().map(|e| e.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, ["src/CACHEDIR.TAG", "src/new.rs"]);
    let _ = std::fs::remove_dir_all(&replica);
}

/// (core#1498) What the pod left out is kept as the host has it: never a deletion, while a file the
/// seat really removed is still deleted. A skip naming a git path or an unsafe path refuses.
#[test]
fn k8s_a_left_out_path_is_never_a_deletion() {
    let host = tmp("keep-host");
    std::fs::create_dir_all(host.join("target/debug")).unwrap();
    std::fs::write(host.join("target/debug/old.o"), "o").unwrap();
    std::fs::write(host.join("big.bin"), "b").unwrap();
    std::fs::write(host.join("gone.txt"), "g").unwrap();
    let digest = |rel: &str| {
        sync::digest(&entry(
            Kind::File,
            rel,
            &std::fs::read(host.join(rel)).unwrap(),
        ))
    };
    let base: std::collections::HashMap<String, [u8; 32]> =
        ["target/debug/old.o", "big.bin", "gone.txt"]
            .iter()
            .map(|r| (r.to_string(), digest(r)))
            .collect();
    let left = vec![
        sync::Skipped {
            path: "target".into(),
            why: "cache".into(),
        },
        sync::Skipped {
            path: "big.bin".into(),
            why: "limit".into(),
        },
    ];
    let (writes, deletes) = seat::delta_keeping(
        &host,
        &base,
        Vec::new(),
        &sync::SkipSet::new(&left),
        &|| false,
    )
    .unwrap();
    assert!(writes.is_empty());
    assert_eq!(deletes, vec!["gone.txt".to_string()]);
    let (_, all) = seat::delta(&host, &base, Vec::new()).unwrap();
    assert_eq!(
        all.len(),
        3,
        "without the skips every path would have been deleted"
    );
    for (bad, why) in [
        (".git", "cache"),
        ("a/../b", "cache"),
        ("/abs", "cache"),
        ("", "cache"),
        ("x", "anything"),
    ] {
        let mut r = Root {
            replica: "t".into(),
            mode: Mode::Plain,
            entries: vec![entry(Kind::Skipped, bad, why.as_bytes())],
        };
        assert!(sync::take_skipped(&mut r).is_err(), "{bad} {why}");
    }
    // (codex r1) A file left out for the limit covers that path only: a subtree it replaced is
    // still a deletion.
    let limit = sync::Skipped {
        path: "src".into(),
        why: "limit".into(),
    };
    assert!(limit.covers("src") && !limit.covers("src/gone.rs"));
    // (codex r1) A host change under a skip is no conflict: nothing there is written or deleted.
    std::fs::write(host.join("target/debug/old.o"), "changed on the host").unwrap();
    let (_, deletes) = seat::delta_keeping(
        &host,
        &base,
        Vec::new(),
        &sync::SkipSet::new(&left),
        &|| false,
    )
    .unwrap();
    assert_eq!(deletes, vec!["gone.txt".to_string()]);
    assert!(
        seat::delta(&host, &base, Vec::new()).is_err(),
        "delta alone calls it a conflict"
    );
    let _ = std::fs::remove_dir_all(&host);
}

/// (core#1517) A pod cannot force unbounded skip matching work on the host.
#[test]
fn k8s_the_host_bounds_left_out_paths() {
    let mut root = Root {
        replica: "w".into(),
        mode: Mode::Tree,
        entries: vec![entry(Kind::Skipped, "cache", b"cache"); sync::MAX_SKIPS],
    };
    assert_eq!(
        sync::take_skipped(&mut root).unwrap().len(),
        sync::MAX_SKIPS
    );
    root.entries = vec![entry(Kind::Skipped, "cache", b"cache"); sync::MAX_SKIPS + 1];
    let err = sync::take_skipped(&mut root).unwrap_err();
    assert!(err.contains(&(sync::MAX_SKIPS + 1).to_string()), "{err}");
    assert!(err.contains("limit"), "{err}");
}

/// (core#1517) The codec bounds cache and limit skips together, before returning success.
#[cfg(unix)]
#[test]
fn k8s_the_pod_bounds_left_out_paths() {
    let replica = tmp("skip-bound");
    let tree = replica.join("w");
    std::fs::create_dir_all(tree.join("cache")).unwrap();
    git_in(&tree, &["init", "-q"]);
    std::fs::write(
        tree.join("cache/CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();
    for name in ["a.bin", "b.bin"] {
        std::fs::File::create(tree.join(name))
            .unwrap()
            .set_len(sync::MAX_STREAM + 1)
            .unwrap();
    }
    // Keep the fixture small while exercising pack's real skip emission for both reasons.
    let bound = format!("MAX_SKIPS = {}", sync::MAX_SKIPS);
    assert!(sync::POD_CODEC.contains(bound.as_str()));
    let program = sync::POD_CODEC.replace(bound.as_str(), "MAX_SKIPS = 2");
    let plan = json!([["w", "tree", [".git"], []]]).to_string();
    let out = codec_program(&replica, &["pack", &plan], br#"{"w": []}"#, "-", &program);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        err.contains("3 left-out paths") && err.contains("limit of 2"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&replica);
}

/// (core#1517) A host tree with no repository sends no tracked set: a `.git` the seat made is never
/// consulted, so nothing is protected or left out for the limit and the seat's files come back.
#[cfg(unix)]
#[test]
fn k8s_the_pod_never_reads_its_own_index_when_the_host_sent_none() {
    let replica = tmp("missing-tracked");
    let tree = replica.join("w");
    std::fs::create_dir_all(&tree).unwrap();
    git_in(&tree, &["init", "-q"]);
    std::fs::write(tree.join("new.rs"), "new\n").unwrap();
    let plan = json!([["w", "tree", [".git"], []]]).to_string();
    let out = codec(&replica, &["pack", &plan], b"{}");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let roots = sync::decode(&mut out.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    assert!(roots[0].entries.iter().any(|e| e.path == "new.rs"));
    let _ = std::fs::remove_dir_all(&replica);
}

/// (core#1535 item 1) Trees and written-back plain roots snapshot the index apart from content.
#[cfg(unix)]
#[test]
fn k8s_copy_in_snapshots_host_tracked_paths_for_each_tree() {
    let host = tmp("tracked-snapshot");
    let repo = host.join("repo");
    let empty = host.join("empty");
    let scratch = host.join("scratch");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::create_dir_all(&scratch).unwrap();
    git_in(&repo, &["init", "-q"]);
    git_in(&empty, &["init", "-q"]);
    std::fs::write(repo.join("src/file.rs"), "tracked\n").unwrap();
    std::fs::write(repo.join("new.rs"), "untracked\n").unwrap();
    git_in(&repo, &["add", "src/file.rs"]);
    let mut snapshots = Vec::new();
    for (mode, back) in [
        (Mode::Tree, true),
        (Mode::Tree, false),
        (Mode::Plain, true),
        (Mode::Plain, false),
    ] {
        let roots: Vec<PlanRoot> = [&repo, &empty, &scratch]
            .into_iter()
            .map(|path| PlanRoot {
                host: path.clone(),
                mode,
                skip: vec![".git".into()],
                back,
                files: Vec::new(),
                masks: Vec::new(),
                repo: false,
                ro: false,
                optional: false,
            })
            .collect();
        let (_, _, _, baseline, tracked) =
            seat::copy_in_stream(None, &repo, &roots, false, &Default::default()).unwrap();
        if back {
            assert!(baseline[&repo].contains_key("new.rs"));
        }
        snapshots.push((mode, back, tracked));
    }
    git_in(&repo, &["rm", "-q", "-r", "--cached", "src"]);
    for (mode, back, tracked) in snapshots {
        if mode == Mode::Plain && !back {
            assert!(tracked.is_empty());
        } else {
            assert_eq!(tracked.get(&repo), Some(&Some(vec!["src/file.rs".into()])));
            assert_eq!(tracked.get(&empty), Some(&Some(Vec::new())));
            assert_eq!(tracked.get(&scratch), Some(&None));
        }
    }
    let _ = std::fs::remove_dir_all(&host);
}

/// (core#1517) Both entry indexing and baseline matching observe cancellation every 4096 paths.
#[test]
fn k8s_skip_matching_can_be_cancelled_mid_loop() {
    for matching_baseline in [false, true] {
        let paths: Vec<String> = (0..4097).map(|i| format!("cache/{i}")).collect();
        let (base, entries) = if matching_baseline {
            (
                paths.into_iter().map(|p| (p, [0; 32])).collect(),
                Vec::new(),
            )
        } else {
            (
                std::collections::HashMap::new(),
                paths.iter().map(|p| entry(Kind::File, p, b"x")).collect(),
            )
        };
        let skips = sync::SkipSet::new(&[sync::Skipped {
            path: "cache".into(),
            why: "cache".into(),
        }]);
        let calls = std::cell::Cell::new(0);
        let cancelled = || {
            calls.set(calls.get() + 1);
            calls.get() == 2
        };
        let err = seat::delta_keeping(Path::new("unused"), &base, entries, &skips, &cancelled)
            .unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        assert_eq!(calls.get(), 2);
    }
}

/// (core#1517) Indexed skip matching preserves exact coverage and folded alias semantics.
#[test]
fn k8s_skip_set_matches_individual_skips() {
    let cases = [
        ("src", "cache"),
        ("src/nested", "cache"),
        ("one.bin", "limit"),
        ("SRC", "cache"),
        ("s\u{200c}rc", "cache"),
        ("café", "cache"),
        ("STRASSE.bin", "limit"),
    ];
    let paths = [
        "src",
        "src/file.rs",
        "src/nested/file.rs",
        "src2/file.rs",
        "srcfile.rs",
        "SRC/file.rs",
        "s\u{200c}rc/file.rs",
        "one.bin",
        "ONE.BIN",
        "one.bin/child",
        "cafe\u{301}/file",
        "straße.bin",
        "straße.bin/child",
        "other",
    ];
    // Each skip alone, the combined set, and the empty set catch interactions and boundaries.
    for skips in (0..cases.len())
        .map(|i| &cases[i..i + 1])
        .chain([&cases[..], &[]])
    {
        let left: Vec<sync::Skipped> = skips
            .iter()
            .map(|(path, why)| sync::Skipped {
                path: (*path).into(),
                why: (*why).into(),
            })
            .collect();
        let set = sync::SkipSet::new(&left);
        for path in paths {
            assert_eq!(
                set.covers(path),
                left.iter().any(|s| s.covers(path)),
                "{left:?}: {path}"
            );
            assert_eq!(
                set.may_alias(path),
                left.iter().any(|s| s.may_alias(path)),
                "{left:?}: {path}"
            );
        }
    }
}

/// (core#1498, codex r1) A planted tag never hides tracked work, an ignored tag still marks its
/// build dir, and the root is never left out: in a git tree, a tag in a dir holding a tracked file
/// marks nothing (the tracked edit and deletion still come back); a tag the tree ignores still
/// marks an untracked dir; a tag at the root marks nothing.
#[cfg(unix)]
#[test]
fn k8s_a_cache_tag_never_hides_tracked_work_and_needs_no_listing() {
    const SIG: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n";
    let replica = tmp("cachepack-guard");
    let t = replica.join("w");
    std::fs::create_dir_all(t.join("src")).unwrap();
    git_in(&t, &["init", "-q"]);
    std::fs::write(t.join("src/changed.rs"), "a").unwrap();
    std::fs::write(t.join("src/deleted.rs"), "d").unwrap();
    std::fs::write(t.join(".gitignore"), "CACHEDIR.TAG\n").unwrap();
    git_in(&t, &["add", "-A"]);
    git_in(&t, &["commit", "-q", "-m", "a"]);
    std::fs::write(t.join("src/changed.rs"), "b").unwrap();
    std::fs::remove_file(t.join("src/deleted.rs")).unwrap();
    std::fs::write(t.join("src/CACHEDIR.TAG"), SIG).unwrap();
    std::fs::write(t.join("CACHEDIR.TAG"), SIG).unwrap();
    std::fs::create_dir_all(t.join("baseline-target/debug")).unwrap();
    std::fs::write(t.join("baseline-target/CACHEDIR.TAG"), SIG).unwrap();
    std::fs::write(t.join("baseline-target/debug/parser.o"), "obj").unwrap();
    let plan = json!([["w", "tree", [".git"], []]]).to_string();
    let out = codec(
        &replica,
        &["pack", &plan],
        br#"{"w": [".gitignore", "src/changed.rs", "src/deleted.rs"]}"#,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut roots = sync::decode(&mut out.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    let left = sync::take_skipped(&mut roots[0]).unwrap();
    assert_eq!(
        left,
        vec![sync::Skipped {
            path: "baseline-target".into(),
            why: "cache".into()
        }],
        "only the untracked build dir, though git ignores its tag"
    );
    let paths: Vec<&str> = roots[0].entries.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.contains(&"src/changed.rs"), "{paths:?}");
    assert!(!paths.contains(&"src/deleted.rs"), "{paths:?}");
    assert!(
        !paths.iter().any(|p| p.starts_with("baseline-target")),
        "{paths:?}"
    );
    let _ = std::fs::remove_dir_all(&replica);
}

#[test]
fn k8s_an_offline_seat_is_never_selected_by_the_gateway_policies() {
    let docs =
        render::run_namespace_docs("ns", "o", "r", "dns", &render::RunBudget::default(), "1Gi");
    let egress = docs
        .iter()
        .find(|d| d["metadata"]["name"] == "seat-egress-gateway-only")
        .unwrap();
    assert_eq!(
        egress["spec"]["podSelector"]["matchLabels"][render::NET_LABEL],
        "online"
    );
    let ingress = render::gateway_ingress("dns", "o", &[3000]);
    assert_eq!(
        ingress["spec"]["ingress"][0]["from"][0]["podSelector"]["matchLabels"][render::NET_LABEL],
        "online"
    );
    let mut s = seat_pod(None, None);
    s.gateway_ip = None;
    let sb = render::seat_sandbox(&s).unwrap();
    assert_eq!(
        sb["spec"]["podTemplate"]["metadata"]["labels"][render::NET_LABEL],
        "offline"
    );
    s.gateway_ip = Some("10.0.0.1".into());
    let sb = render::seat_sandbox(&s).unwrap();
    assert_eq!(
        sb["spec"]["podTemplate"]["metadata"]["labels"][render::NET_LABEL],
        "online"
    );
}

// ── review r5 folds ──────────────────────────────────────────────────────────────────────────

#[test]
fn k8s_a_writer_write_holds_a_kernel_lock_from_before_its_body_to_after_its_rename() {
    let s = gw::write_script(gw::Volume::Config, "config.json", &"a".repeat(64)).unwrap();
    // busybox `flock` (the writer's) has no `-w`: a bounded `-n` retry (B-4 live run).
    assert!(!s.contains("flock -w"));
    let lock = s.find("until flock -n 9").expect("the kernel lock");
    let take = s.find("cat > ").unwrap();
    let rename = s.find("mv -f \"$t\"").unwrap();
    assert!(s.find("exec 9>/cfg/.config.json.lock").unwrap() < lock);
    assert!(
        lock < take && take < rename,
        "locked before the body, renamed under it"
    );
}

#[test]
fn k8s_the_deny_check_proves_the_seats_own_node_closed_on_every_gateway_port() {
    let s = seat::deny_script(true, Some("192.168.5.15"));
    for p in render::seat_gateway_ports() {
        assert!(s.contains(&format!("nc_ 192.168.5.15 {p} node{p}")), "{s}");
    }
    assert!(
        s.contains("nc_ 192.168.5.15 10250 node10250"),
        "the kubelet port listens"
    );
    let mut out = String::from("api=closed\nnet=closed\ngw=open\nnode10250=closed\n");
    for p in render::seat_gateway_ports() {
        out.push_str(&format!("node{p}=closed\n"));
    }
    assert!(seat::deny_holds(&out, true, true));
    let open = out.replacen("=closed\n", "=open\n", 3);
    assert!(!seat::deny_holds(&open, true, true), "a node port answered");
    assert!(seat::deny_holds(
        "api=closed net=closed gw=open",
        true,
        false
    ));
}

#[cfg(unix)]
#[test]
fn k8s_a_force_added_ignored_file_is_never_read_as_deleted() {
    let host = tmp("forced");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .hardened()
            .args(args)
            .current_dir(&host)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if !git(&["init", "-q"]) {
        eprintln!("skipping: git init failed");
        return;
    }
    std::fs::write(host.join(".gitignore"), "out/\n").unwrap();
    std::fs::create_dir_all(host.join("out")).unwrap();
    std::fs::write(host.join("out/important"), "keep").unwrap();
    std::fs::write(host.join("src.rs"), "s").unwrap();
    assert!(git(&["add", "-f", "out/important", "src.rs", ".gitignore"]));
    let ignored = seat::host_ignored(&host, &["out/important".into(), "src.rs".into()]).unwrap();
    assert!(ignored.contains("out/important"));
    assert!(!ignored.contains("src.rs"), "a plain deletion still counts");
    let _ = std::fs::remove_dir_all(&host);
}

#[cfg(unix)]
#[test]
fn k8s_a_file_replacing_a_directory_with_unseen_contents_is_refused_before_any_change() {
    let host = tmp("dir-replace");
    std::fs::create_dir_all(host.join("dir")).unwrap();
    std::fs::write(host.join("dir/tracked"), "t").unwrap();
    std::fs::write(host.join("dir/ignored"), "i").unwrap();
    let r = PlanRoot {
        host: host.clone(),
        mode: Mode::Plain,
        skip: Vec::new(),
        back: true,
        files: Vec::new(),
        masks: Vec::new(),
        repo: false,
        ro: false,
        optional: false,
    };
    let plans: Vec<seat::RootPlan<'_>> = vec![(
        &r,
        vec![entry(Kind::File, "dir", b"now a file")],
        vec!["dir/tracked".into()],
    )];
    assert!(seat::apply_roots(&plans).is_err());
    assert!(
        host.join("dir/tracked").exists(),
        "deleted before the refusal"
    );
    let _ = std::fs::remove_dir_all(&host);
}

#[cfg(target_os = "linux")]
#[test]
fn k8s_a_name_the_stream_cannot_carry_refuses_the_copy_in() {
    use std::os::unix::ffi::OsStrExt;
    let host = tmp("non-utf8");
    let ok = std::process::Command::new("git")
        .hardened()
        .args(["init", "-q"])
        .current_dir(&host)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        return;
    }
    let name = std::ffi::OsStr::from_bytes(b"bad\xff");
    if std::fs::write(host.join(name), "x").is_err() {
        return;
    }
    assert!(sync::host_tree_listing(&host).is_err());
    let _ = std::fs::remove_dir_all(&host);
}

// ── Review r6 (pi) ─────────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
fn git_in(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .hardened()
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// r6 #3: content the seat left exactly as the host gave it is the host's own: a case pair on a
/// case-sensitive host and a tracked absolute link are written back unchanged. The same entries
/// new or changed by the seat are still refused.
#[test]
fn k8s_unchanged_host_content_never_refuses_a_write_back() {
    let a = entry(Kind::File, "xt_TCPMSS.c", b"upper");
    let b = entry(Kind::File, "xt_tcpmss.c", b"lower");
    let l = entry(Kind::Link, "abs", b"/etc/hosts");
    let r = Root {
        replica: "t".into(),
        mode: Mode::Tree,
        entries: vec![a.clone(), b.clone(), l.clone()],
    };
    let base: std::collections::HashMap<String, [u8; 32]> = [&a, &b, &l]
        .iter()
        .map(|e| (e.path.clone(), sync::digest(e)))
        .collect();
    assert_eq!(
        sync::check_entries_against(&r, &[], &base).unwrap().len(),
        3
    );
    // New (no baseline) or changed: refused as before.
    assert!(sync::check_entries(&r, &[]).is_err());
    let mut changed = base.clone();
    changed.insert("abs".into(), [0; 32]);
    assert!(sync::check_entries_against(&r, &[], &changed).is_err());
    let mut changed = base;
    changed.insert("xt_tcpmss.c".into(), [0; 32]);
    assert!(sync::check_entries_against(&r, &[], &changed).is_err());
}

/// r6 #2: a repository a seat cloned into its tree (listed by git as `dir/`) neither breaks the
/// pack nor survives the next copy-in.
#[cfg(unix)]
#[test]
fn k8s_an_embedded_repo_neither_breaks_the_codec_nor_persists() {
    let replica = tmp("replica-embedded");
    let tree = "home/u/wt";
    let t = replica.join(tree);
    std::fs::create_dir_all(&t).unwrap();
    git_in(&t, &["init", "-q"]);
    std::fs::write(t.join("a.txt"), "a").unwrap();
    git_in(&t, &["add", "a.txt"]);
    git_in(&t, &["commit", "-q", "-m", "a"]);
    std::fs::create_dir_all(t.join("sub")).unwrap();
    git_in(&t.join("sub"), &["init", "-q"]);
    std::fs::write(t.join("sub/x"), "x").unwrap();
    let plan = json!([[tree, "tree", [".git"]]]).to_string();
    let packed = codec(&replica, &["pack", &plan], br#"{"home/u/wt": ["a.txt"]}"#);
    assert!(
        packed.status.success(),
        "{}",
        String::from_utf8_lossy(&packed.stderr)
    );
    let roots = sync::decode(&mut packed.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    let paths: Vec<&str> = roots[0].entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["a.txt"]);
    // The next copy-in mirrors the tree: the embedded repo goes.
    let stream = sync::encode(&[Root {
        replica: tree.into(),
        mode: Mode::Tree,
        entries: vec![entry(Kind::File, "a.txt", b"a")],
    }]);
    let out = codec(&replica, &["unpack", &plan], &stream);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !t.join("sub").exists(),
        "an embedded repo survived the copy-in"
    );
    let _ = std::fs::remove_dir_all(&replica);
}

/// r6 #13: a seat-written alternates file (another scope's objects) refuses the bundle.
#[cfg(unix)]
#[test]
fn k8s_a_bundle_refuses_a_git_dir_naming_another_object_store() {
    let replica = tmp("replica-alternates");
    let t = replica.join("wt");
    std::fs::create_dir_all(&t).unwrap();
    git_in(&t, &["init", "-q"]);
    git_in(&t, &["commit", "-q", "--allow-empty", "-m", "base"]);
    let base = String::from_utf8(
        std::process::Command::new("git")
            .hardened()
            .args(["rev-parse", "HEAD"])
            .current_dir(&t)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    git_in(&t, &["commit", "-q", "--allow-empty", "-m", "seat"]);
    std::fs::write(
        t.join(".git/objects/info/alternates"),
        "/replica/other/.git/objects\n",
    )
    .unwrap();
    let out = codec(&replica, &["bundle", "wt", &base], b"");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("another object store"));
    let _ = std::fs::remove_dir_all(&replica);
}

/// r6 #6: a seat or judge pod another engine boot created is stale; this boot's and other roles'
/// are not. Its Sandbox is named so it can be deleted with it.
#[test]
fn k8s_a_dead_engines_seat_pods_are_found_before_a_copy_in() {
    let pod = |name: &str, role: &str, boot: &str| {
        json!({"metadata": {"name": name,
            "labels": {super::ROLE_LABEL: role, super::BOOT_LABEL: boot},
            "ownerReferences": [{"kind": "Sandbox", "name": format!("sbx-{name}")}]}})
    };
    let pods = vec![
        pod("a", "seat", "old"),
        pod("b", "seat", "now"),
        pod("c", "judge", "old"),
        pod("d", "sync", "old"),
        json!({"metadata": {"name": "e", "labels": {super::ROLE_LABEL: "seat"}}}),
    ];
    assert_eq!(
        seat::stale_seat_pods(&pods, "now"),
        vec![
            ("a".to_string(), Some("sbx-a".to_string())),
            ("c".to_string(), Some("sbx-c".to_string())),
            ("e".to_string(), None),
        ]
    );
}

/// r6 #7: only a killed client resets the shared sync pod; a codec's own refusal does not.
#[test]
fn k8s_a_codec_refusal_never_resets_the_shared_sync_pod() {
    assert!(seat::resets_sync(None));
    // r7 #6: a cut exec stream exits 1 while the remote codec may still run.
    assert!(seat::resets_sync(Some(1)));
    assert!(!seat::resets_sync(Some(3)));
    assert!(!seat::resets_sync(Some(0)));
}

/// r6 #9: no node address, no arm (the node probe is never skipped).
#[test]
fn k8s_a_seat_without_a_readable_node_address_is_refused() {
    assert!(seat::node_address(None).is_err());
    assert!(seat::node_address(Some(&json!({"status": {}}))).is_err());
    assert!(seat::node_address(Some(&json!({"status": {"hostIP": "1.2.3.4; x"}}))).is_err());
    assert_eq!(
        seat::node_address(Some(&json!({"status": {"hostIP": "192.168.5.15"}}))).unwrap(),
        "192.168.5.15"
    );
}

/// r6 #16: a one-shot scope is a digest of the exact session id, so two ids one label would
/// collapse get different scopes.
#[test]
fn k8s_one_shot_scopes_never_collide_on_a_label() {
    // The literal label of one id is itself a valid id with the same label.
    let lit = super::label_value("validator-a b");
    assert_eq!(super::label_value(&lit), lit);
    let a = seat::one_shot_scope("validator-a b").unwrap();
    let b = seat::one_shot_scope(&lit).unwrap();
    assert_ne!(a, b);
    assert_eq!(a, seat::one_shot_scope("validator-a b").unwrap());
    assert!(seat::one_shot_scope("run-1").is_none());
}

/// wicked-garden#1365: every claude seat with a config home is handed its record path (so its
/// units clean it), and only a claude seat signed in by a token is handed a record, in claude's
/// own env-token shape. Every other CLI gets neither. Red on the reverted wrapper handoff: no
/// record, so a `claude -p` the seat starts from its Bash tool is "Not logged in".
#[test]
fn k8s_only_a_claude_oauth_seat_is_handed_a_credential_record() {
    // Built at runtime: no bearer-looking literal in the source (core#1024's GitGuardian fix).
    let token = format!("sk-ant-oat01-{}", "W".repeat(40));
    let vars = vec![
        ("CLAUDE_CODE_OAUTH_TOKEN".to_string(), token.clone()),
        ("CLAUDE_CONFIG_DIR".to_string(), "/seat/claude/".to_string()),
    ];
    let file = (
        "WICKED_CLAUDE_RECORD_FILE".to_string(),
        "/seat/claude/.credentials.json".to_string(),
    );
    let record = seat::claude_record_vars("claude", &vars);
    assert_eq!(
        record.len(),
        2,
        "{:?}",
        record.iter().map(|(k, _)| k).collect::<Vec<_>>()
    );
    assert_eq!(record[0], file);
    assert_eq!(record[1].0, "WICKED_CLAUDE_RECORD");
    let body: serde_json::Value = serde_json::from_str(&record[1].1).unwrap();
    assert_eq!(body["claudeAiOauth"]["accessToken"], token.as_str());
    assert!(body["claudeAiOauth"]["refreshToken"].is_null());
    assert!(body["claudeAiOauth"]["expiresAt"].is_null());
    // An API-key claude seat: the path alone, so the wrapper only cleans.
    assert_eq!(seat::claude_record_vars("claude", &vars[1..]), [file]);
    assert!(seat::claude_record_vars("codex", &vars).is_empty());
    assert!(seat::claude_record_vars("claude", &vars[..1]).is_empty());
}

/// The stub kubectl of the wicked-garden#1365 tests: it logs every call's argv
/// (`kubectl-argv.log`) and keeps the unit body `finalize` hands the pod on stdin (`unit-body`).
#[cfg(unix)]
fn record_kube(dir: &Path) -> super::Kube {
    let kubectl = dir.join("kubectl");
    crate::test_env::install_stub(
        &kubectl,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n\
             case \" $* \" in *' exec -i '*) /bin/cat > '{body}';; esac\nexit 0\n",
            log = dir.join("kubectl-argv.log").display(),
            body = dir.join("unit-body").display()
        ),
    );
    super::Kube {
        kubectl,
        kubeconfig: None,
        context: "t".into(),
        owner: "o".into(),
    }
}

/// A registered `cli` seat (tree `dir`) finalized for a launch command with `envs`, as the runner
/// does right before the unit's exec. Returns the live seat and its id.
#[cfg(unix)]
fn finalized_seat(
    kube: &super::Kube,
    dir: &Path,
    cli: &str,
    envs: &[(String, String)],
) -> (std::sync::Arc<seat::Seat>, String) {
    // Alphanumeric only: `seat_id_of` reads no other marker, and finalize would pass it by.
    let id = format!("record{}", uuid::Uuid::new_v4().simple());
    let live = seat::register_for_test(kube, &id, dir, cli);
    // spawn-audit: test-only — never spawned: finalize reads its args and env, and spawns the stub.
    let mut cmd = std::process::Command::new("kubectl");
    cmd.args([
        "exec",
        "-i",
        "-c",
        "seat",
        "seat-1",
        "--",
        "sh",
        "-c",
        seat::WRAPPER,
    ])
    .arg(format!("{}{id}", seat::MARKER_PREFIX))
    .args(["/floor/landrun", "--", "/floor/launch", cli, "-p", "hi"]);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let finalized = seat::finalize(&cmd);
    assert!(matches!(finalized, Ok(Some(_))), "{:?}", finalized.err());
    (live, id)
}

/// The unit file `finalize` hands a registered `cli` seat for a launch command with `envs`, as
/// the pod's exec receives it on stdin (a stub kubectl keeps it).
#[cfg(unix)]
fn unit_body_from_finalize(dir: &Path, cli: &str, envs: &[(String, String)]) -> String {
    let kube = record_kube(dir);
    let (live, id) = finalized_seat(&kube, dir, cli, envs);
    seat::forget_for_test(&id);
    drop(live);
    std::fs::read_to_string(dir.join("unit-body")).unwrap()
}

/// Runs [`seat::WRAPPER`] on the host as the pod runs it, on a unit `body` from
/// [`unit_body_from_finalize`] whose argv is replaced by `/bin/sh -c script`.
/// Every argv the wrapper hands `/floor/busybox` is logged to `busybox-argv.log`.
#[cfg(unix)]
fn run_wrapper(host: &Path, body: &str, script: &str) -> std::process::Output {
    let unit = host.join("unit");
    std::fs::create_dir_all(&unit).unwrap();
    let body = body.replace("/wicked-unit", &unit.to_string_lossy());
    let env_part = &body[..body.rfind("set --").expect("the body ends with its argv")];
    std::fs::write(
        unit.join("env"),
        format!(
            "{env_part}{}",
            seat::argv_line(&["/bin/sh".into(), "-c".into(), script.into()]).unwrap()
        ),
    )
    .unwrap();
    // `/floor/busybox` is a shim that logs every argv it is given (`busybox-argv.log`).
    let shim = host.join("busybox");
    crate::test_env::install_stub(
        &shim,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec /usr/bin/env \"$@\"\n",
            host.join("busybox-argv.log").display()
        ),
    );
    let wrapper = seat::WRAPPER
        .replace("/wicked-unit", &unit.to_string_lossy())
        .replace("/floor/busybox", &shim.to_string_lossy());
    std::process::Command::new("/bin/sh")
        .hardened()
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .args(["-c", &wrapper])
        .output()
        .unwrap()
}

/// wicked-garden#1365, through the production handoff: the unit file `finalize` builds for a
/// claude OAuth seat makes the wrapper write the record before the CLI starts, so the CLI and
/// anything it starts read it from its config home. The carriers never reach the CLI (unset by
/// the wrapper and never named to landrun). Whatever a seat or an interrupted unit left at the
/// record path or its temporary name (a directory, a link) is removed first and never followed,
/// and both names are removed once the CLI exits, whatever its exit code.
#[cfg(unix)]
#[test]
fn k8s_the_wrapper_holds_the_claude_record_only_while_the_cli_runs() {
    let host = tmp("claude-record");
    let cfg = host.join("cfg");
    let rec = cfg.join(".credentials.json");
    let tmp_name = cfg.join(".credentials.json.wicked-tmp");
    std::fs::create_dir_all(rec.join("planted-dir")).unwrap();
    let target = host.join("planted-target");
    std::fs::write(&target, "untouched").unwrap();
    std::os::unix::fs::symlink(&target, &tmp_name).unwrap();
    let token = format!("sk-ant-oat01-{}", "X".repeat(40));
    let body = unit_body_from_finalize(
        &host,
        "claude",
        &[
            ("CLAUDE_CODE_OAUTH_TOKEN".into(), token.clone()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                cfg.to_string_lossy().into_owned(),
            ),
        ],
    );
    let argv = &body[body.rfind("set --").unwrap()..];
    assert!(argv.contains("'--env' 'CLAUDE_CODE_OAUTH_TOKEN'"), "{argv}");
    assert!(
        !argv.contains("WICKED_CLAUDE"),
        "landrun is never told the carriers: {argv}"
    );
    let out = run_wrapper(
        &host,
        &body,
        "cat \"$CLAUDE_CONFIG_DIR/.credentials.json\" > seen; env > seen-env; exit 3",
    );
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(host.join("seen")).unwrap()).unwrap();
    assert_eq!(seen["claudeAiOauth"]["accessToken"], token.as_str());
    let seen_env = std::fs::read_to_string(host.join("seen-env")).unwrap();
    assert!(
        !seen_env.contains("WICKED_CLAUDE_RECORD"),
        "the carriers never reach the CLI"
    );
    assert!(
        !seen_env.contains("claudeAiOauth"),
        "nor does the record, by any name"
    );
    // The record is written by the shell's builtin printf: no exec argv ever carries it (node
    // exec auditing records argv). Red with `/floor/busybox printf`.
    let argv = std::fs::read_to_string(host.join("busybox-argv.log")).unwrap();
    assert!(
        argv.contains("mv -f"),
        "the shim saw the wrapper's execs: {argv}"
    );
    assert!(!argv.contains(&token), "the token rode an exec argv");
    assert!(
        !argv.contains("claudeAiOauth"),
        "the record rode an exec argv"
    );
    assert!(
        !rec.exists() && !rec.is_symlink(),
        "removed once the CLI exits"
    );
    assert!(!tmp_name.exists() && !tmp_name.is_symlink());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
    assert_eq!(
        std::fs::read_to_string(host.join("unit/rc")).unwrap(),
        "3\n"
    );
    std::fs::remove_dir_all(host).unwrap();
}

/// wicked-garden#1365: an API-key claude seat is handed no record, and its unit still removes a
/// record (and a temporary name) a unit that ended without its wrapper left behind, before its
/// CLI starts.
#[cfg(unix)]
#[test]
fn k8s_an_api_key_claude_unit_removes_a_left_record() {
    let host = tmp("claude-record-apikey");
    let cfg = host.join("cfg");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join(".credentials.json"), "left").unwrap();
    std::fs::write(cfg.join(".credentials.json.wicked-tmp"), "left").unwrap();
    let body = unit_body_from_finalize(
        &host,
        "claude",
        &[
            ("ANTHROPIC_API_KEY".into(), "k".into()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                cfg.to_string_lossy().into_owned(),
            ),
        ],
    );
    assert!(body.contains("export WICKED_CLAUDE_RECORD_FILE="));
    assert!(!body.contains("export WICKED_CLAUDE_RECORD="));
    let out = run_wrapper(
        &host,
        &body,
        "for f in \"$CLAUDE_CONFIG_DIR\"/.credentials.json*; do [ -e \"$f\" ] && exit 9; done; exit 0",
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_dir(&cfg).unwrap().count(), 0);
    std::fs::remove_dir_all(host).unwrap();
}

/// wicked-garden#1365: a record the wrapper cannot remove after the CLI fails the unit (125),
/// with a named reason, never a success that leaves the token behind silently.
#[cfg(unix)]
#[test]
fn k8s_a_claude_record_that_cannot_be_removed_fails_the_unit() {
    use std::os::unix::fs::PermissionsExt as _;
    let host = tmp("claude-record-stuck");
    let cfg = host.join("cfg");
    std::fs::create_dir_all(&cfg).unwrap();
    let body = unit_body_from_finalize(
        &host,
        "claude",
        &[
            (
                "CLAUDE_CODE_OAUTH_TOKEN".into(),
                format!("sk-ant-oat01-{}", "Y".repeat(40)),
            ),
            (
                "CLAUDE_CONFIG_DIR".into(),
                cfg.to_string_lossy().into_owned(),
            ),
        ],
    );
    // The CLI makes its config home read-only: the record can no longer be removed.
    let out = run_wrapper(&host, &body, "chmod a-w \"$CLAUDE_CONFIG_DIR\"; exit 0");
    let stuck = std::fs::metadata(cfg.join(".credentials.json")).is_ok();
    std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o755)).unwrap();
    if stuck {
        assert_eq!(out.status.code(), Some(125));
        assert!(String::from_utf8_lossy(&out.stderr).contains("could not be removed"));
        assert_eq!(
            std::fs::read_to_string(host.join("unit/rc")).unwrap(),
            "125\n"
        );
    } else {
        // Root ignores the mode bits (a container CI runner): the record was removed after all.
        assert_eq!(out.status.code(), Some(0));
    }
    std::fs::remove_dir_all(host).unwrap();
}

/// wicked-garden#1365: a claude unit that times out, is cancelled or is abandoned before it
/// settles (`None`: its guard reaps it) never reaches the wrapper's cleanup (its pod is deleted
/// under it), so the record and its temporary name are removed in the pod first. No kubectl argv
/// carries the token.
#[cfg(unix)]
#[test]
fn k8s_a_claude_unit_that_never_reaches_its_cleanup_has_its_record_removed_before_its_pod_goes() {
    for ending in [Some(Ending::TimedOut), Some(Ending::Cancelled), None] {
        let host = tmp("claude-record-settle");
        let cfg = host.join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        let token = format!("sk-ant-oat01-{}", "Z".repeat(40));
        let kube = record_kube(&host);
        let (live, id) = finalized_seat(
            &kube,
            &host,
            "claude",
            &[
                ("CLAUDE_CODE_OAUTH_TOKEN".into(), token.clone()),
                (
                    "CLAUDE_CONFIG_DIR".into(),
                    cfg.to_string_lossy().into_owned(),
                ),
            ],
        );
        match ending {
            Some(ending) => assert_eq!(seat::settle(&live, ending), Ok(-1)),
            None => seat::reap(&id),
        }
        seat::forget_for_test(&id);
        let log = std::fs::read_to_string(host.join("kubectl-argv.log")).unwrap();
        let rec = format!("{}/.credentials.json", cfg.display());
        let removal =
            format!("exec -c seat seat-1 -- /floor/busybox rm -rf {rec} {rec}.wicked-tmp");
        let lines: Vec<&str> = log.lines().collect();
        let rm = lines
            .iter()
            .position(|l| l.ends_with(&removal))
            .unwrap_or_else(|| panic!("{ending:?}: no record removal: {log}"));
        let gone = lines
            .iter()
            .position(|l| l.contains("sandboxes.agents.x-k8s.io"))
            .unwrap_or_else(|| panic!("{ending:?}: no pod deletion: {log}"));
        assert!(
            rm < gone,
            "{ending:?}: removed after the pod's deletion began: {log}"
        );
        assert!(!log.contains(&token), "a kubectl argv carried the token");
        std::fs::remove_dir_all(host).unwrap();
    }
}

/// r6 #5: a wrapper setup failure writes its rc, so it is the unit's failure (exit 125), never a
/// lost stream.
#[test]
fn k8s_a_wrapper_setup_failure_is_an_exit_code_not_a_lost_stream() {
    assert_eq!(
        seat::WRAPPER.matches("echo 125 > $u/rc; exit 125").count(),
        4
    );
    assert_eq!(seat::exit_code(125, Some("125\n")), Some(125));
}

/// r6 #1: a seat cache already in the replica is not read or sent again (the codec keeps it).
#[cfg(unix)]
#[test]
fn k8s_a_seeded_cache_is_not_sent_again() {
    let host = tmp("seed-host");
    let cache = host.join("cache/claude");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("blob"), vec![7u8; 4096]).unwrap();
    let root = PlanRoot {
        host: cache.clone(),
        mode: Mode::Seed,
        skip: Vec::new(),
        back: false,
        files: Vec::new(),
        masks: Vec::new(),
        repo: false,
        ro: false,
        optional: false,
    };
    let rep = seat::replica_of(None, &cache).unwrap();
    let first = seat::copy_in_stream(
        None,
        &host,
        std::slice::from_ref(&root),
        false,
        &Default::default(),
    )
    .unwrap();
    let again = seat::copy_in_stream(
        None,
        &host,
        std::slice::from_ref(&root),
        false,
        &[rep].into_iter().collect(),
    )
    .unwrap();
    let decode = |b: &[u8]| sync::decode(&mut &b[..], sync::MAX_STREAM).unwrap();
    assert_eq!(decode(&first.0)[0].entries.len(), 1);
    assert!(decode(&again.0)[0].entries.is_empty());
    let _ = std::fs::remove_dir_all(&host);
}

/// r7 #1: a seat home keeps what the CLI wrote in earlier units (its sessions, so a resume finds
/// them): the copy-in writes the engine's files over it and never mirrors it.
#[cfg(unix)]
#[test]
fn k8s_a_seat_home_keeps_its_sessions_across_units() {
    let replica = tmp("replica-home");
    let home = "home/u/.wicked-container/run/claude";
    std::fs::create_dir_all(replica.join(home).join("sessions")).unwrap();
    std::fs::write(replica.join(home).join("sessions/s1.jsonl"), "turn 1").unwrap();
    let stream = sync::encode(&[Root {
        replica: home.into(),
        mode: Mode::Home,
        entries: vec![entry(Kind::File, "settings.json", b"{}")],
    }]);
    let plan = json!([[home, "home", []]]).to_string();
    let out = codec(&replica, &["unpack", &plan], &stream);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(replica.join(home).join("settings.json")).unwrap(),
        b"{}"
    );
    assert!(
        replica.join(home).join("sessions/s1.jsonl").exists(),
        "a session was mirrored away"
    );
    let _ = std::fs::remove_dir_all(&replica);
}

/// r7 #8: a `.git` that is a file (a gitfile naming a sibling scope) refuses the bundle.
#[cfg(unix)]
#[test]
fn k8s_a_bundle_refuses_a_gitfile() {
    let replica = tmp("replica-gitfile");
    let t = replica.join("wt");
    std::fs::create_dir_all(&t).unwrap();
    std::fs::write(t.join(".git"), "gitdir: /replica/other/.git\n").unwrap();
    let out = codec(&replica, &["bundle", "wt", &"a".repeat(40)], b"");
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a directory"));
    let _ = std::fs::remove_dir_all(&replica);
}

// ── codex per-module round (b2-seat, b2-sync): folds ────────────────────────────────────────

/// A git repository at `dir` with one commit per `(file, body)`; `None` when git is unusable.
#[cfg(unix)]
fn repo_with(dir: &Path, commits: &[(&str, &str)]) -> Option<()> {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .hardened()
            .args(args)
            .current_dir(dir)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    std::fs::create_dir_all(dir).ok()?;
    git(&["init", "-q"]).then_some(())?;
    for (f, body) in commits {
        std::fs::write(dir.join(f), body).ok()?;
        git(&["add", f]).then_some(())?;
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            f,
        ])
        .then_some(())?;
    }
    Some(())
}

#[cfg(unix)]
fn git_out(dir: &Path, args: &[&str]) -> String {
    let o = std::process::Command::new("git")
        .hardened()
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

#[cfg(unix)]
#[test]
fn k8s_codec_git_runs_jailed_to_the_units_own_tree_and_fails_closed() {
    use std::os::unix::fs::PermissionsExt;
    let replica = tmp("jail");
    let w = replica.join("scope-a/w");
    if repo_with(&w, &[("a", "1"), ("b", "2")]).is_none() {
        eprintln!("skipping: git unusable");
        return;
    }
    let base = git_out(&w, &["rev-parse", "HEAD~1"]);
    // A fake launcher: records its argv, then runs what follows its `--`.
    let bin = tmp("jail-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let rec = bin.join("argv");
    let jail = bin.join("landrun");
    let script = format!(
        "#!/bin/sh\ncase \"$*\" in *-canary*) [ -f '{r}.open' ] || exit 7;; esac\n\
         printf '%s\\n' \"$@\" >> '{r}'\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n",
        r = rec.display()
    );
    std::fs::write(&jail, script).unwrap();
    std::fs::set_permissions(&jail, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = codec_jailed(
        &replica,
        &["bundle", "scope-a/w", &base],
        b"",
        jail.to_str().unwrap(),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.stdout.is_empty(),
        "the commit came back through the jail"
    );
    let argv = std::fs::read_to_string(&rec).unwrap();
    let lines: Vec<&str> = argv.lines().collect();
    let granted = |flag: &str| -> Vec<String> {
        lines
            .windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].to_string())
            .collect()
    };
    let root = format!("{}/scope-a/w", replica.display());
    assert!(granted("--ro").contains(&root), "{argv}");
    // Nothing of the replica beyond the unit's own tree, and no write but a private temp dir.
    for flag in ["--ro", "--rox", "--rw", "--rwx"] {
        for p in granted(flag) {
            assert!(
                !p.starts_with(&replica.display().to_string()) || p == root,
                "{flag} {p} grants more of the replica than the unit's tree"
            );
        }
    }
    assert!(granted("--rw")
        .iter()
        .all(|p| p == "/dev/null" || p.starts_with("/tmp/wicked-unit-")));
    // A launcher that confines nothing (a kernel without Landlock under `--best-effort`) is
    // caught by the probe before git runs: refused.
    std::fs::write(format!("{}.open", rec.display()), "").unwrap();
    let open = codec_jailed(
        &replica,
        &["bundle", "scope-a/w", &base],
        b"",
        jail.to_str().unwrap(),
    );
    assert!(!open.status.success());
    assert!(String::from_utf8_lossy(&open.stderr).contains("Landlock is not in force"));
    // A launcher that is not there refuses: the git call never runs open.
    let none = codec_jailed(
        &replica,
        &["bundle", "scope-a/w", &base],
        b"",
        "/nonexistent/landrun",
    );
    assert!(!none.status.success());
    let _ = std::fs::remove_dir_all(&replica);
    let _ = std::fs::remove_dir_all(&bin);
}

#[cfg(unix)]
#[test]
fn k8s_a_promisor_remote_in_a_seat_git_never_runs_its_command() {
    let replica = tmp("promisor");
    let w = replica.join("w");
    if repo_with(&w, &[("a", "1"), ("b", "2")]).is_none() {
        eprintln!("skipping: git unusable");
        return;
    }
    let base = git_out(&w, &["rev-parse", "HEAD~1"]);
    let blob = git_out(&w, &["rev-parse", "HEAD:b"]);
    std::fs::remove_file(w.join(".git/objects").join(&blob[..2]).join(&blob[2..])).unwrap();
    let marker = tmp("promisor-ran").join("ran");
    for (k, v) in [
        ("extensions.partialClone", "origin".to_string()),
        ("remote.origin.url", "ssh://h/r".to_string()),
        ("remote.origin.promisor", "true".to_string()),
        (
            "core.sshCommand",
            format!("touch '{}'; false", marker.display()),
        ),
    ] {
        git_out(&w, &["config", k, &v]);
    }
    let out = codec(&replica, &["bundle", "w", &base], b"");
    assert!(!out.status.success(), "a missing object refuses the bundle");
    assert!(
        !marker.exists(),
        "the seat's configured command ran in the sync pod"
    );
    let _ = std::fs::remove_dir_all(&replica);
}

#[cfg(unix)]
#[test]
fn k8s_a_tree_with_no_history_and_no_git_dir_has_no_commits_to_land() {
    let replica = tmp("nogit");
    std::fs::create_dir_all(replica.join("w")).unwrap();
    std::fs::write(replica.join("w/edited"), "e").unwrap();
    let out = codec(&replica, &["bundle", "w", seat::NO_BASE], b"");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty());
    // A link where the git dir belongs is still refused.
    std::os::unix::fs::symlink("/tmp", replica.join("w/.git")).unwrap();
    assert!(!codec(&replica, &["bundle", "w", seat::NO_BASE], b"")
        .status
        .success());
    let _ = std::fs::remove_dir_all(&replica);
}

#[cfg(unix)]
#[test]
fn k8s_a_deleted_force_added_file_is_a_deletion_and_an_ignored_untracked_one_is_not() {
    let host = tmp("forced2");
    if repo_with(&host, &[("src.rs", "s")]).is_none() {
        eprintln!("skipping: git unusable");
        return;
    }
    std::fs::write(host.join(".gitignore"), "out/\n").unwrap();
    std::fs::create_dir_all(host.join("out")).unwrap();
    std::fs::write(host.join("out/important"), "keep").unwrap();
    std::fs::write(host.join("out/scratch"), "x").unwrap();
    git_out(&host, &["add", "-f", "out/important", ".gitignore"]);
    let d = seat::tree_deletions(
        &host,
        vec![
            "out/important".into(),
            "out/scratch".into(),
            "src.rs".into(),
        ],
    )
    .unwrap();
    assert!(
        d.contains(&"out/important".to_string()),
        "a tracked file the seat deleted: {d:?}"
    );
    assert!(d.contains(&"src.rs".to_string()));
    assert!(
        !d.contains(&"out/scratch".to_string()),
        "ignored and untracked: kept"
    );
    let _ = std::fs::remove_dir_all(&host);
}

#[cfg(unix)]
#[test]
fn k8s_a_kept_link_cannot_be_made_to_escape_by_changing_what_it_passes() {
    let host = tmp("retained");
    std::fs::create_dir_all(host.join("a")).unwrap();
    std::fs::write(host.join("a/file"), "f").unwrap();
    std::os::unix::fs::symlink("a/..", host.join("b")).unwrap();
    std::os::unix::fs::symlink("../elsewhere", host.join("c")).unwrap();
    let b = entry(Kind::Link, "b", b"a/..");
    let c = entry(Kind::Link, "c", b"../elsewhere");
    let writes = vec![entry(Kind::Link, "a", b".")];
    let deletes = vec!["a/file".to_string()];
    let e = seat::check_retained_links(&host, std::slice::from_ref(&b), &writes, &deletes, &[])
        .unwrap_err();
    assert!(e.contains("`b`"), "{e}");
    // A link that already led outside is the host's own: left as it is.
    seat::check_retained_links(&host, &[c], &writes[..0], &deletes, &[]).unwrap();
    // Nothing changed around it: fine.
    seat::check_retained_links(&host, &[b], &[], &[], &[]).unwrap();
    let base: std::collections::HashMap<String, [u8; 32]> = [(
        "b".to_string(),
        sync::digest(&entry(Kind::Link, "b", b"a/..")),
    )]
    .into();
    assert_eq!(
        seat::retained_links(
            &[
                entry(Kind::Link, "b", b"a/.."),
                entry(Kind::Link, "n", b"x")
            ],
            &base
        )
        .len(),
        1
    );
    let _ = std::fs::remove_dir_all(&host);
}

#[test]
fn pr_c2_ignored_host_links_are_checked_before_any_write() {
    let host = tmp("ignored-retained");
    std::fs::create_dir(host.join("a")).unwrap();
    std::fs::write(host.join(".gitignore"), "b\n").unwrap();
    std::os::unix::fs::symlink("a/..", host.join("b")).unwrap();
    let writes = [entry(Kind::Link, "a", b".")];
    let err = seat::check_retained_links(&host, &[], &writes, &[], &[]).unwrap_err();
    assert!(err.contains("`b`"), "{err}");
    assert!(host.join("a").is_dir());
    assert_eq!(
        std::fs::read_link(host.join("b")).unwrap(),
        Path::new("a/..")
    );
    // A replaced link does not survive; a link already escaping is left alone.
    seat::check_retained_links(&host, &[], &writes, &["b".into()], &[]).unwrap();
    std::fs::remove_file(host.join("b")).unwrap();
    std::os::unix::fs::symlink("../outside", host.join("b")).unwrap();
    seat::check_retained_links(&host, &[], &writes, &[], &[]).unwrap();
    std::fs::remove_dir_all(host).unwrap();
}

#[cfg(unix)]
#[test]
fn k8s_a_host_link_under_another_spelling_is_still_followed() {
    let host = tmp("spelling-link");
    std::fs::create_dir_all(&host).unwrap();
    std::os::unix::fs::symlink("/etc", host.join("a")).unwrap();
    // `A` (a file) replaces nothing at `a` on a case-sensitive host; `b -> a` must follow the
    // host's `a`, which leaves the root.
    let entries = vec![entry(Kind::File, "A", b"x"), entry(Kind::Link, "b", b"a")];
    assert!(sync::links_stay_inside_final(Some(&host), &entries, &[], &[]).is_err());
    let _ = std::fs::remove_dir_all(&host);
}

#[cfg(unix)]
#[test]
fn k8s_a_replaced_private_file_keeps_its_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let host = tmp("modes");
    std::fs::create_dir_all(&host).unwrap();
    std::fs::write(host.join("secret"), "old").unwrap();
    std::fs::set_permissions(host.join("secret"), std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(host.join("tool"), "old").unwrap();
    std::fs::set_permissions(host.join("tool"), std::fs::Permissions::from_mode(0o640)).unwrap();
    sync::apply(
        &host,
        &[
            entry(Kind::File, "secret", b"new"),
            entry(Kind::Exec, "tool", b"#!"),
        ],
        &[],
        &[],
    )
    .unwrap();
    let mode = |n: &str| {
        std::fs::metadata(host.join(n))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("secret"), 0o600);
    assert_eq!(mode("tool"), 0o750);
    assert_eq!(std::fs::read(host.join("secret")).unwrap(), b"new");
    let _ = std::fs::remove_dir_all(&host);
}

#[cfg(unix)]
#[test]
fn k8s_host_state_that_cannot_be_read_is_never_read_as_absent() {
    use std::os::unix::fs::PermissionsExt;
    let host = tmp("unreadable");
    std::fs::create_dir_all(host.join("d")).unwrap();
    std::fs::write(host.join("ignored.env"), "secret").unwrap();
    std::fs::set_permissions(
        host.join("ignored.env"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    if std::fs::read(host.join("ignored.env")).is_ok() {
        eprintln!("skipping: running as a user that reads mode-000 files");
        return;
    }
    // A new path in the write-back over a host file the engine cannot compare: refused.
    let e = seat::delta(
        &host,
        &Default::default(),
        vec![entry(Kind::File, "ignored.env", b"seat")],
    )
    .unwrap_err();
    assert!(e.contains("could not be read"), "{e}");
    // A listing that cannot read a directory is refused, never short.
    std::fs::write(host.join("d/f"), "f").unwrap();
    std::fs::set_permissions(host.join("d"), std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(sync::host_plain_listing(&host, &[]).is_err());
    // A deletion under a directory the engine cannot open is an error, never a skip.
    assert!(sync::apply(&host, &[], &["d/f".into()], &[]).is_err());
    std::fs::set_permissions(host.join("d"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(
        host.join("ignored.env"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let _ = std::fs::remove_dir_all(&host);
}

#[cfg(unix)]
#[test]
fn k8s_a_repository_git_cannot_read_is_refused_not_copied_whole() {
    let host = tmp("broken-repo");
    std::fs::create_dir_all(host.join(".git")).unwrap();
    std::fs::write(host.join(".git/HEAD"), "garbage").unwrap();
    std::fs::write(host.join(".env"), "ignored secret").unwrap();
    assert!(sync::host_tree_listing(&host).is_err());
    let _ = std::fs::remove_dir_all(&host);
}

#[cfg(unix)]
#[test]
fn k8s_a_cut_seed_is_never_taken_as_seeded() {
    let replica = tmp("seed-cut");
    std::fs::create_dir_all(&replica).unwrap();
    let full = sync::encode(&[Root {
        replica: "cache/c".into(),
        mode: Mode::Seed,
        entries: vec![
            entry(Kind::File, "one", b"1"),
            entry(Kind::File, "two", b"2"),
        ],
    }]);
    let plan = json!([["cache/c", "seed", []]]).to_string();
    let cut = &full[..full.len() - 20];
    assert!(!codec(&replica, &["unpack", &plan], cut).status.success());
    assert!(
        !replica.join("cache/c").exists(),
        "a partial seed was published"
    );
    let out = codec(&replica, &["unpack", &plan], &full);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(replica.join("cache/c/two")).unwrap(), b"2");
    assert!(!replica.join("cache/c.wicked-seeding").exists());
    let _ = std::fs::remove_dir_all(&replica);
}

#[cfg(unix)]
#[test]
fn k8s_a_plain_roots_git_metadata_never_travels_either_way() {
    let host = tmp("plain-git");
    std::fs::create_dir_all(host.join(".git")).unwrap();
    std::fs::write(host.join(".git/config"), "c").unwrap();
    std::fs::write(host.join("evidence.json"), "{}").unwrap();
    let r = PlanRoot {
        host: host.clone(),
        mode: Mode::Plain,
        skip: Vec::new(),
        back: true,
        files: Vec::new(),
        masks: Vec::new(),
        repo: false,
        ro: false,
        optional: false,
    };
    let (stream, _, _, baseline, _) =
        seat::copy_in_stream(None, &host, &[r], false, &Default::default()).unwrap();
    let roots = sync::decode(&mut stream.as_slice(), sync::MAX_STREAM).unwrap();
    let paths: Vec<&str> = roots[0].entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["evidence.json"]);
    assert!(baseline.values().all(|b| !b.contains_key(".git/config")));
    // The pod side packs no `.git` of a plain root either.
    let replica = tmp("plain-git-replica");
    let rp = replica.join("p");
    std::fs::create_dir_all(rp.join(".git")).unwrap();
    std::fs::write(rp.join(".git/config"), "c").unwrap();
    std::fs::write(rp.join("e"), "e").unwrap();
    let out = codec(
        &replica,
        &["pack", &json!([["p", "plain", []]]).to_string()],
        b"{}",
    );
    let back = sync::decode(&mut out.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    assert_eq!(back[0].entries.len(), 1);
    let _ = std::fs::remove_dir_all(&host);
    let _ = std::fs::remove_dir_all(&replica);
}

#[test]
fn k8s_a_stream_of_root_records_alone_is_bounded() {
    let roots: Vec<Root> = (0..sync::MAX_ROOTS + 1)
        .map(|i| Root {
            replica: format!("r{i}"),
            mode: Mode::Plain,
            entries: Vec::new(),
        })
        .collect();
    let s = sync::encode(&roots);
    let e = sync::decode(&mut s.as_slice(), sync::MAX_STREAM).unwrap_err();
    assert!(e.contains("roots"), "{e}");
}

#[test]
fn k8s_the_fenced_scratch_is_no_home_and_goes_at_settle() {
    let mk = |p: &str| PlanRoot {
        host: PathBuf::from(p),
        mode: Mode::Plain,
        skip: Vec::new(),
        back: false,
        files: Vec::new(),
        masks: Vec::new(),
        repo: false,
        ro: false,
        optional: false,
    };
    let mut roots = vec![mk("/w/home"), mk("/w/scratch")];
    let dirs = [PathBuf::from("/w/home"), PathBuf::from("/w/scratch")];
    seat::mark_homes(&mut roots, &dirs, Some(Path::new("/w/scratch")));
    assert_eq!(roots[0].mode, Mode::Home);
    assert_eq!(
        roots[1].mode,
        Mode::Plain,
        "the scratch would stay in the replica"
    );
}

#[cfg(unix)]
#[test]
fn k8s_the_landing_index_is_built_aside_and_a_held_lock_refuses() {
    let host = tmp("index");
    if repo_with(&host, &[("a", "1"), ("b", "2")]).is_none() {
        eprintln!("skipping: git unusable");
        return;
    }
    let head = git_out(&host, &["rev-parse", "HEAD"]);
    let before = std::fs::read(host.join(".git/index")).unwrap();
    std::fs::write(host.join(".git/index.lock"), "").unwrap();
    assert!(seat::prepare_index(&host, &head, "u1").is_err());
    assert_eq!(std::fs::read(host.join(".git/index")).unwrap(), before);
    std::fs::remove_file(host.join(".git/index.lock")).unwrap();
    let staged = seat::prepare_index(&host, &head, "u1").unwrap();
    assert!(host.join(".git/index.wicked-u1").exists());
    drop(staged);
    assert!(
        !host.join(".git/index.wicked-u1").exists(),
        "an unused staged index is removed"
    );
    let staged = seat::prepare_index(&host, &head, "u2").unwrap();
    staged.install().unwrap();
    assert!(!host.join(".git/index.wicked-u2").exists());
    assert_eq!(git_out(&host, &["status", "--porcelain"]), "");
    let _ = std::fs::remove_dir_all(&host);
}

#[cfg(unix)]
#[test]
fn k8s_the_landing_branch_is_read_as_the_copy_in_saw_it() {
    let host = tmp("branch");
    if repo_with(&host, &[("a", "1")]).is_none() {
        eprintln!("skipping: git unusable");
        return;
    }
    let b = seat::head_branch(&host).unwrap().expect("a branch");
    assert!(b.starts_with("refs/heads/"));
    git_out(&host, &["checkout", "-q", "--detach"]);
    assert_eq!(seat::head_branch(&host).unwrap(), None);
    let _ = std::fs::remove_dir_all(&host);
}

#[cfg(unix)]
#[test]
fn k8s_a_bundle_dir_is_private_and_new() {
    use std::os::unix::fs::PermissionsExt;
    let d = seat::private_temp_dir("wicked-test-private").unwrap();
    assert_eq!(
        std::fs::metadata(&d.0).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let p = d.0.clone();
    drop(d);
    assert!(!p.exists());
}

// ── codex per-module round (b2-gw-glue): folds ──────────────────────────────────────────────

#[test]
fn k8s_a_writer_write_removes_killed_writes_bodies_under_its_lock() {
    let s = gw::write_script(gw::Volume::Broker, "anthropic.token", &"a".repeat(64)).unwrap();
    let lock = s.find("flock -n 9").unwrap();
    let rm = s
        .find("rm -f /broker/.anthropic.token.[0-9]*")
        .expect("stale bodies removed");
    let body = s.find("cat > \"$t\"").unwrap();
    assert!(lock < rm && rm < body, "{s}");
}

#[test]
fn k8s_key_confirmation_bounds_every_attempt() {
    assert!(gw::confirm_script(true).contains("wget -T 2 "));
}

#[cfg(unix)]
#[test]
fn k8s_the_boot_token_never_follows_a_planted_link() {
    let dir = tmp("boot");
    let outside = tmp("boot-outside").join("victim");
    std::fs::write(&outside, "keep").unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("config/boot-token")).unwrap();
    let t = gw::boot_token(&dir, false).unwrap();
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
    assert!(!std::fs::symlink_metadata(dir.join("config/boot-token"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        gw::boot_token(&dir, false).unwrap(),
        t,
        "the token it wrote is read back"
    );
    // A token this engine never wrote (any alphanumeric text) is not taken.
    std::fs::write(dir.join("config/boot-token"), "operatorsecret").unwrap();
    assert_ne!(gw::boot_token(&dir, false).unwrap(), "operatorsecret");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn k8s_a_seat_image_link_at_a_pod_mount_is_refused() {
    for at in ["/wicked-unit", "/wicked-ro"] {
        let facts = crate::container_floor::ImageFacts {
            symlinks: [PathBuf::from(at)].into(),
            ..Default::default()
        };
        assert!(
            crate::container_floor::check_seat_image(&facts, &[]).is_err(),
            "{at}"
        );
    }
}

#[test]
fn k8s_a_read_only_view_copy_fails_when_either_side_fails() {
    let pod = render::seat_pod_spec(&seat_pod(Some(PathBuf::from("/w/t")), None)).unwrap();
    let init = pod.to_string();
    assert!(init.contains("set -o pipefail;"), "{init}");
}

/// core#1424: a seat on the run's read-only `node_modules` claim gets the tool caches inside it
/// writable and its own (Vite's config bundle goes to node_modules/.vite-temp).
#[test]
fn k8s_a_deps_seat_writes_its_tool_caches_over_the_read_only_claim() {
    let mut s = seat_pod(None, None);
    s.deps = Some(("deps-1".into(), PathBuf::from("/home/u/wt")));
    let pod = render::seat_pod_spec(&s).unwrap();
    let mounts = pod
        .pointer("/containers/0/volumeMounts")
        .unwrap()
        .as_array()
        .unwrap();
    let at = |p: &str| mounts.iter().find(|m| m["mountPath"] == p).cloned();
    assert_eq!(at("/home/u/wt/node_modules").unwrap()["readOnly"], true);
    for d in render::NM_WRITABLE {
        let m = at(&format!("/home/u/wt/node_modules/{d}")).expect(d);
        assert_ne!(m["readOnly"], true, "{d}");
        let vol = pod["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == m["name"])
            .unwrap();
        assert_eq!(vol["emptyDir"]["medium"], "Memory");
    }
    assert!(render::NM_WRITABLE.contains(&".vite-temp"));
}

/// k8s-final arm timing: a plain root's host read is reused while its host signature holds,
/// and a change on the host (content, a new file) is always read again.
#[test]
fn k8s_a_plain_roots_host_read_is_reused_only_while_the_host_is_unchanged() {
    let base = std::env::temp_dir().join(format!("wicked-rc-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(base.join("skills/a")).unwrap();
    let base = base.canonicalize().unwrap();
    struct Rm(PathBuf);
    impl Drop for Rm {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _rm = Rm(base.clone());
    let skills = base.join("skills");
    std::fs::write(skills.join("a/SKILL.md"), "one").unwrap();
    let r = PlanRoot {
        host: skills.clone(),
        mode: Mode::Plain,
        skip: Vec::new(),
        back: false,
        files: Vec::new(),
        masks: Vec::new(),
        repo: false,
        ro: false,
        optional: false,
    };
    let body = |r: &PlanRoot| {
        let (b, _, _, _, _) = seat::copy_in_stream(
            None,
            &base,
            std::slice::from_ref(r),
            false,
            &Default::default(),
        )
        .unwrap();
        let d = sync::decode(&mut b.as_slice(), sync::MAX_STREAM).unwrap();
        d.into_iter()
            .flat_map(|root| root.entries)
            .map(|e| (e.path, String::from_utf8(e.data).unwrap()))
            .collect::<Vec<_>>()
    };
    let reads = || seat::HOST_READS.with(|c| c.get());
    let first = body(&r);
    assert_eq!(first, vec![("a/SKILL.md".to_string(), "one".to_string())]);
    let n = reads();
    assert_eq!(body(&r), first, "a cached read differs from the host");
    assert_eq!(reads(), n, "an unchanged root was read from the host again");
    // Another skip list is another signature: read again.
    let skipping = PlanRoot {
        skip: vec!["zz".into()],
        ..r.clone()
    };
    body(&skipping);
    assert_eq!(reads(), n + 1);
    // A directory moved out and back is read again (codex r2 #1).
    std::fs::rename(skills.join("a"), base.join("a-away")).unwrap();
    std::fs::rename(base.join("a-away"), skills.join("a")).unwrap();
    body(&r);
    assert_eq!(reads(), n + 2, "a moved directory kept the cached read");
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(skills.join("a/SKILL.md"), "two").unwrap();
    std::fs::write(skills.join("b.md"), "b").unwrap();
    let mut now = body(&r);
    now.sort();
    assert_eq!(
        now,
        vec![
            ("a/SKILL.md".to_string(), "two".to_string()),
            ("b.md".to_string(), "b".to_string())
        ]
    );
}

/// k8s-final arm timing: the replica's rebuilt `.git` is reused for the same commit, and a new
/// commit (or another mask) rebuilds it.
#[cfg(unix)]
#[test]
fn k8s_the_replica_git_is_rebuilt_only_for_a_new_commit_or_mask() {
    let t = tmp("git-cache");
    git_in(&t, &["init", "-q", "-b", "main"]);
    std::fs::write(t.join("a.txt"), "a").unwrap();
    git_in(&t, &["add", "a.txt"]);
    git_in(&t, &["commit", "-q", "-m", "a"]);
    let tree = PlanRoot {
        host: t.clone(),
        mode: Mode::Tree,
        skip: vec![".git".into()],
        back: true,
        files: Vec::new(),
        masks: Vec::new(),
        repo: false,
        ro: false,
        optional: false,
    };
    let git_of = |r: &PlanRoot| {
        let (b, _, base, _, _) =
            seat::copy_in_stream(None, &t, std::slice::from_ref(r), true, &Default::default())
                .unwrap();
        let d = sync::decode(&mut b.as_slice(), sync::MAX_STREAM).unwrap();
        let g = d.into_iter().find(|x| x.mode == Mode::Git).unwrap();
        (base.unwrap(), g.entries)
    };
    let builds = || seat::GIT_BUILDS.with(|c| c.get());
    let (base1, g1) = git_of(&tree);
    let n = builds();
    let (base2, g2) = git_of(&tree);
    assert_eq!((&base1, &g1), (&base2, &g2));
    assert_eq!(builds(), n, "the same commit rebuilt its .git");
    std::fs::write(t.join("b.txt"), "b").unwrap();
    git_in(&t, &["add", "b.txt"]);
    git_in(&t, &["commit", "-q", "-m", "b"]);
    let (base3, g3) = git_of(&tree);
    assert_ne!(base3, base1, "a new commit read the cached .git");
    assert_ne!(g3, g1);
    // A mask makes a different index (skip-worktree): never the unmasked one.
    let masked = PlanRoot {
        masks: vec!["b.txt".into()],
        repo: false,
        ro: false,
        optional: false,
        ..tree.clone()
    };
    let (_, g4) = git_of(&masked);
    assert_ne!(g4, g3);
    // A shallow host repository is never cached (codex r1 #6): each arm rebuilds.
    let shallow = tmp("git-cache-shallow");
    let src = format!("file://{}", t.display());
    git_in(&shallow, &["clone", "-q", "--depth", "1", &src, "s"]);
    let s = shallow.join("s");
    let st = PlanRoot {
        host: s.clone(),
        ..tree.clone()
    };
    let shallow_git = |r: &PlanRoot| {
        seat::copy_in_stream(None, &s, std::slice::from_ref(r), true, &Default::default()).unwrap()
    };
    shallow_git(&st);
    let m = builds();
    shallow_git(&st);
    assert_eq!(builds(), m + 1, "a shallow host's .git was cached");
    let _ = std::fs::remove_dir_all(&shallow);
    let _ = std::fs::remove_dir_all(&t);
}

// ── fix-k8s-chat: read-only repositories (a chat's "All repos" scope, crew#1034) ─────────────

#[cfg(unix)]
fn git_init(dir: &Path) -> bool {
    std::process::Command::new("git")
        .hardened()
        .args(["init", "-q"])
        .current_dir(dir)
        .status()
        .is_ok_and(|s| s.success())
}

/// `tree` (a scratch tree, no repository) and `repo` bound as `rw`: the unit's replica plan.
#[cfg(unix)]
fn plan_with_repo(tree: &Path, repo: &Path, rw: bool) -> Vec<PlanRoot> {
    let mounts = [
        PlanMount::Bind {
            path: tree.to_path_buf(),
            rw: true,
        },
        PlanMount::Bind {
            path: repo.to_path_buf(),
            rw,
        },
    ];
    seat::plan(tree, false, &mounts, &[], &[], None).0
}

/// A repository with source, ignored build output (sparse: over the whole stream's limit), an
/// ignored secret, a run worktree under it, and an untracked embedded repository.
#[cfg(unix)]
fn registered_repo(host: &Path) -> Option<PathBuf> {
    let repo = host.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    if !git_init(&repo) {
        return None;
    }
    std::fs::write(
        repo.join(".gitignore"),
        "target/\n.env\nwicked-worktrees/\n",
    )
    .unwrap();
    for (p, d) in [
        ("src/a.rs", "fn a() {}"),
        (".env", "TOKEN=secret"),
        ("wicked-worktrees/run-1/other.rs", "another run's work"),
    ] {
        std::fs::create_dir_all(repo.join(p).parent().unwrap()).unwrap();
        std::fs::write(repo.join(p), d).unwrap();
    }
    std::fs::create_dir_all(repo.join("target/debug")).unwrap();
    let f = std::fs::File::create(repo.join("target/debug/huge.rlib")).unwrap();
    f.set_len(sync::MAX_STREAM + 1).unwrap();
    std::fs::create_dir_all(repo.join("vendor/lib")).unwrap();
    if !git_init(&repo.join("vendor/lib")) {
        return None;
    }
    std::fs::write(repo.join("vendor/lib/x.rs"), "x").unwrap();
    Some(repo)
}

/// The P1 (plugins-e2e, chat b9d791b1): a read-only repository was copied whole, so its ignored
/// build output passed the stream's limit and every chat turn was refused. It is now copied as
/// git lists it: never its build output, its ignored `.env`, the runs' worktrees under it, its
/// `.git`, or an embedded repository.
#[cfg(unix)]
#[test]
fn k8s_a_read_only_repo_is_copied_as_git_lists_it_not_whole() {
    let host = tmp("ro-repo");
    let tree = host.join("chat");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("note.md"), "n").unwrap();
    let Some(repo) = registered_repo(&host) else {
        eprintln!("skipping: git init failed");
        return;
    };
    let roots = plan_with_repo(&tree, &repo, false);
    // Before the fix: "the unit's copy-in is larger than 536870912 bytes (at …/target/…)".
    let (body, plan, _, _, _) =
        seat::copy_in_stream(None, &tree, &roots, false, &Default::default()).unwrap();
    let r = roots.iter().find(|r| r.host == repo).unwrap();
    assert!(r.repo && r.mode == Mode::Plain && !r.back, "{r:?}");
    let decoded = sync::decode(&mut body.as_slice(), sync::MAX_STREAM).unwrap();
    let rep = seat::replica_of(None, &repo).unwrap();
    let got = decoded.iter().find(|d| d.replica == rep).unwrap();
    let mut paths: Vec<&str> = got.entries.iter().map(|e| e.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, vec![".gitignore", "src/a.rs"]);
    // The pod codec lands it as listed.
    let replica = tmp("ro-repo-replica");
    let out = codec(&replica, &["unpack", &plan.to_string()], &body);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(replica.join(&rep).join("src/a.rs").is_file());
    assert!(!replica.join(&rep).join(".env").exists());
    let _ = std::fs::remove_dir_all(&host);
    let _ = std::fs::remove_dir_all(&replica);
}

/// core#1781 (run faa858e7): a governed unit on a repository inside a project armed no seat. The
/// container's read roots carried the bound project's graph directory, the plan made it a plain
/// root, and its `code-graph.db` (814 MB on the operator's daemon) passed the stream's limit, so
/// every seat was refused `copy_in_refused` before its first step. The seat reads its graph through
/// crew's estate relay; its copy-in now carries no project graph. Before the fix: "the unit's
/// copy-in is larger than 536870912 bytes (at …/project-graphs/<id>/code-graph.db)".
#[cfg(unix)]
#[test]
fn k8s_a_unit_bound_to_an_oversized_project_graph_arms_without_copying_it() {
    let host = tmp("project-graph");
    let tree = host.join("wt");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("lib.rs"), "pub fn mode() {}").unwrap();
    let state_home = host.join("state");
    let graph_dir = state_home.join("project-graphs").join("proj-a");
    std::fs::create_dir_all(&graph_dir).unwrap();
    let db = graph_dir.join("code-graph.db");
    // Sparse: over the whole stream's limit, with no disk behind it.
    std::fs::File::create(&db)
        .unwrap()
        .set_len(sync::MAX_STREAM + 1)
        .unwrap();
    std::fs::write(graph_dir.join("manifest.json"), "{}").unwrap();
    let extra = host.join("handoff");
    std::fs::create_dir_all(&extra).unwrap();
    std::fs::write(extra.join("brief.md"), "b").unwrap();
    let gov = crate::workflow::GovernanceContext {
        human_confirm: Default::default(),
        db_path: state_home.join("core.db").to_string_lossy().into_owned(),
        code_graph_db: Some(db.to_string_lossy().into_owned()),
        extra_write_roots: Vec::new(),
        extra_read_roots: vec![extra.to_string_lossy().into_owned()],
        optional_read_roots: Vec::new(),
        project_id: Some("proj-a".to_string()),
        repo_id: None,
        apps: None,
    };
    // A codex seat (no gate-hook roots) and a claude seat whose gate-hook roots name the graph.
    for gov_roots in [Vec::new(), vec![graph_dir.clone()]] {
        let reads = crate::execute_wrapped::seat_read_roots(
            Some(&gov),
            None,
            gov_roots,
            &[],
            Some(&state_home),
        );
        let mut mounts = vec![PlanMount::Bind {
            path: tree.clone(),
            rw: true,
        }];
        mounts.extend(
            reads
                .into_iter()
                .map(|path| PlanMount::Bind { path, rw: false }),
        );
        let (roots, pod) = seat::plan(&tree, false, &mounts, &[], &[], None);
        let (body, _, _, _, _) =
            seat::copy_in_stream(None, &tree, &roots, false, &Default::default())
                .unwrap_or_else(|e| panic!("the unit's copy-in is refused: {e:?}"));
        assert!(
            pod.iter().all(|m| !m.path.starts_with(&graph_dir)),
            "the pod mounts no project graph: {pod:?}"
        );
        let decoded = sync::decode(&mut body.as_slice(), sync::MAX_STREAM).unwrap();
        let graph_replica = seat::replica_of(None, &graph_dir).unwrap();
        assert!(
            decoded
                .iter()
                .all(|d| !sync::under(&d.replica, &graph_replica)),
            "the copy-in carries no project graph"
        );
        // The unit's other read roots still ride.
        let extra_replica = seat::replica_of(None, &extra).unwrap();
        assert!(decoded.iter().any(|d| d.replica == extra_replica));
    }
    let _ = std::fs::remove_dir_all(&host);
}

/// Only a read-only bind is a repository projection: a writable plain root with a `.git` (a
/// seat's scratch) is still copied whole, as it was.
#[cfg(unix)]
#[test]
fn k8s_a_writable_plain_root_with_a_git_dir_is_not_a_repo_projection() {
    let host = tmp("rw-repo");
    let tree = host.join("chat");
    std::fs::create_dir_all(&tree).unwrap();
    let repo = host.join("scratch");
    std::fs::create_dir_all(&repo).unwrap();
    if !git_init(&repo) {
        eprintln!("skipping: git init failed");
        return;
    }
    std::fs::write(repo.join(".gitignore"), "out/\n").unwrap();
    std::fs::create_dir_all(repo.join("out")).unwrap();
    std::fs::write(repo.join("out/kept"), "k").unwrap();
    let roots = plan_with_repo(&tree, &repo, true);
    let r = roots.iter().find(|r| r.host == repo).unwrap();
    assert!(!r.repo, "{r:?}");
    let (body, _, _, _, _) =
        seat::copy_in_stream(None, &tree, &roots, false, &Default::default()).unwrap();
    let decoded = sync::decode(&mut body.as_slice(), sync::MAX_STREAM).unwrap();
    let rep = seat::replica_of(None, &repo).unwrap();
    let got = decoded.iter().find(|d| d.replica == rep).unwrap();
    assert!(got.entries.iter().any(|e| e.path == "out/kept"));
    let _ = std::fs::remove_dir_all(&host);
}

/// A read-only repository whose `.git` went after the plan is refused, never copied whole.
#[cfg(unix)]
#[test]
fn k8s_a_read_only_repo_that_lost_its_git_is_refused_not_copied_whole() {
    let host = tmp("ro-repo-gone");
    let tree = host.join("chat");
    std::fs::create_dir_all(&tree).unwrap();
    let Some(repo) = registered_repo(&host) else {
        eprintln!("skipping: git init failed");
        return;
    };
    let roots = plan_with_repo(&tree, &repo, false);
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    let e = seat::copy_in_stream(None, &tree, &roots, false, &Default::default()).unwrap_err();
    assert!(e.detail.contains("no longer a repository"), "{}", e.detail);
    let _ = std::fs::remove_dir_all(&host);
}

/// A refused copy-in says so (`copy_in_refused`), never `root_unresolvable` (the working directory).
#[cfg(unix)]
#[test]
fn k8s_a_refused_copy_in_is_reported_as_copy_in_refused() {
    let host = tmp("copy-in-refused");
    let kubectl = host.join("kubectl");
    crate::test_env::install_stub(&kubectl, "#!/bin/sh\nexit 0\n");
    let kube = super::Kube {
        kubectl,
        kubeconfig: None,
        context: "t".into(),
        owner: "o".into(),
    };
    let data = host.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let f = std::fs::File::create(data.join("big.bin")).unwrap();
    f.set_len(sync::MAX_STREAM + 1).unwrap();
    let r = PlanRoot {
        host: data.clone(),
        mode: Mode::Plain,
        skip: Vec::new(),
        back: false,
        files: Vec::new(),
        masks: Vec::new(),
        repo: false,
        ro: false,
        optional: false,
    };
    let (why, e) = seat::copy_in(&kube, "ns", None, &data, &[r], false).unwrap_err();
    assert_eq!(why, FloorUnarmed::CopyInRefused, "{e}");
    assert_eq!(why.as_str(), "copy_in_refused");
    assert!(e.contains("larger than"), "{e}");
    let _ = std::fs::remove_dir_all(&host);
}

#[cfg(unix)]
fn partial_clone(host: &Path) -> (PathBuf, PathBuf, PlanRoot) {
    let source = host.join("source");
    let part = host.join("partial");
    std::fs::create_dir_all(&source).unwrap();
    let git = |dir: &Path, args: &[&str]| {
        let out = sync::git(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&source, &["init", "-q", "-b", "main"]);
    for content in ["one", "two", "three"] {
        std::fs::write(source.join("file.txt"), content).unwrap();
        git(&source, &["add", "file.txt"]);
        git(&source, &["commit", "-q", "-m", content]);
    }
    git(&source, &["config", "uploadpack.allowFilter", "true"]);
    git(
        host,
        &[
            "-c",
            "protocol.file.allow=always",
            "clone",
            "-q",
            "--no-local",
            "--filter=blob:none",
            &format!("file://{}", source.display()),
            part.to_str().unwrap(),
        ],
    );
    let root = PlanRoot {
        host: part.clone(),
        mode: Mode::Tree,
        skip: vec![".git".into()],
        back: true,
        files: Vec::new(),
        masks: Vec::new(),
        repo: false,
        ro: false,
        optional: false,
    };
    (source, part, root)
}

#[cfg(unix)]
#[test]
fn k8s_a_partial_clone_with_missing_objects_is_refused_as_partial_clone() {
    let host = tmp("partial-clone-missing");
    let (source, part, root) = partial_clone(&host);
    std::fs::remove_dir_all(source).unwrap();
    let kubectl = host.join("kubectl");
    crate::test_env::install_stub(&kubectl, "#!/bin/sh\nexit 0\n");
    let kube = super::Kube {
        kubectl,
        kubeconfig: None,
        context: "t".into(),
        owner: "o".into(),
    };
    let (why, detail) = seat::copy_in(&kube, "ns", None, &part, &[root], true).unwrap_err();
    assert_eq!(why.as_str(), "partial_clone", "{detail}");
    assert!(why.describe().contains("partial clone"));
    assert!(why.describe().contains("--refetch"));
    assert!(detail.contains("promisor remote `origin`"), "{detail}");
    let _ = std::fs::remove_dir_all(&host);
}

#[cfg(unix)]
#[test]
fn k8s_a_partial_clone_with_its_objects_fetched_is_copied_in() {
    let host = tmp("partial-clone-fetched");
    let (_, part, root) = partial_clone(&host);
    for args in [
        &["config", "--unset", "remote.origin.partialclonefilter"][..],
        &[
            "-c",
            "protocol.file.allow=always",
            "fetch",
            "-q",
            "--refetch",
            "origin",
        ][..],
    ] {
        let out = sync::git(&part).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let promisor = sync::git(&part)
        .args(["config", "--get", "remote.origin.promisor"])
        .output()
        .unwrap();
    assert!(promisor.status.success());
    assert_eq!(String::from_utf8_lossy(&promisor.stdout).trim(), "true");
    let head = sync::git(&part)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(head.status.success());
    let (_, _, base, _, _) =
        seat::copy_in_stream(None, &part, &[root], true, &Default::default()).unwrap();
    assert_eq!(
        base.as_deref(),
        Some(String::from_utf8_lossy(&head.stdout).trim())
    );
    let _ = std::fs::remove_dir_all(&host);
}

/// (core#1498, codex r2) A skip covers its exact spelling only (a folded alias such as a
/// zero-width-joined or re-cased name never covers another dir's files); the host links a skip keeps
/// are read as links (a changed one included) and a regular file under it is never read; a tag
/// inside git metadata marks nothing.
#[cfg(unix)]
#[test]
fn k8s_left_out_paths_are_exact_and_their_host_links_are_checked() {
    let cache = sync::Skipped {
        path: "s\u{200c}rc".into(),
        why: "cache".into(),
    };
    assert!(!cache.covers("src/file") && cache.covers("s\u{200c}rc/x.o"));
    assert!(!sync::Skipped {
        path: "big".into(),
        why: "limit".into()
    }
    .covers("BIG"));
    let host = tmp("left-out-links");
    std::fs::create_dir_all(host.join("cache")).unwrap();
    std::os::unix::fs::symlink("../a/..", host.join("cache/b")).unwrap();
    let f = std::fs::File::create(host.join("cache/huge.o")).unwrap();
    f.set_len(sync::MAX_STREAM + 1).unwrap();
    let links = seat::left_out_links(
        &host,
        &[sync::Skipped {
            path: "cache".into(),
            why: "cache".into(),
        }],
        &[],
    )
    .unwrap();
    assert_eq!(links.len(), 1, "{links:?}");
    assert_eq!(
        (links[0].path.as_str(), links[0].data.as_slice()),
        ("cache/b", &b"../a/.."[..])
    );
    let _ = std::fs::remove_dir_all(&host);
    const SIG: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n";
    let replica = tmp("cachepack-git");
    std::fs::create_dir_all(replica.join("p/.git/objects/cache")).unwrap();
    std::fs::write(replica.join("p/.git/objects/cache/CACHEDIR.TAG"), SIG).unwrap();
    std::fs::write(replica.join("p/notes.md"), "n").unwrap();
    let plan = json!([["p", "plain", [], []]]).to_string();
    let out = codec(&replica, &["pack", &plan], b"{}");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut roots = sync::decode(&mut out.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    assert!(sync::take_skipped(&mut roots[0]).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&replica);
}

/// (core#1491) A container runtime that timed out (or said nothing) under load is transient: the pod
/// waits for the arm's deadline. Any other create error is still the cluster's refusal, never
/// message-less.
#[test]
fn a_create_timeout_under_load_waits_and_other_create_errors_are_refused() {
    let waiting = |reason: &str, message: &str| {
        let mut p = pod("seat-1-abc", "u1", "Pending", false);
        p["status"]["containerStatuses"][0]["state"] =
            json!({"waiting": {"reason": reason, "message": message}});
        p
    };
    for (reason, msg) in [
        ("CreateContainerError", "context deadline exceeded"),
        (
            "RunContainerError",
            "rpc error: code = DeadlineExceeded desc = timed out",
        ),
        ("CreateContainerError", ""),
    ] {
        assert_eq!(
            seat::pod_state(&waiting(reason, msg), "seat-1-abc", "u1"),
            PodState::Waiting,
            "{reason}: {msg}"
        );
    }
    assert_eq!(
        seat::pod_state(
            &waiting("CreateContainerError", "mount denied by policy"),
            "seat-1-abc",
            "u1"
        ),
        PodState::Failed(FloorUnarmed::PolicyRefused, "mount denied by policy".into())
    );
    assert_eq!(
        seat::pod_state(
            &waiting("CreateContainerConfigError", ""),
            "seat-1-abc",
            "u1"
        ),
        PodState::Failed(
            FloorUnarmed::PolicyRefused,
            "CreateContainerConfigError".into()
        ),
        "a config error is never transient, and never message-less"
    );
}

/// (core#1491) The evidence a not-ready pod leaves: its phase, the conditions that are not True,
/// and its last events.
#[test]
fn a_not_ready_pods_diagnosis_names_phase_conditions_and_last_events() {
    let mut p = pod("seat-1-abc", "u1", "Pending", false);
    p["status"]["conditions"] = json!([
        {"type": "PodScheduled", "status": "True"},
        {"type": "ContainersReady", "status": "False", "reason": "ContainersNotReady", "message": "containers with unready status: [seat]"}
    ]);
    let events = json!({"items": [
        {"reason": "Scheduled", "message": "assigned to node"},
        {"reason": "Pulled", "message": "image present"},
        {"reason": "Failed", "message": "Error: context deadline exceeded"}
    ]});
    let d = seat::pod_diagnosis(Some(&p), Some(&events), 2);
    assert!(d.starts_with("phase Pending"), "{d}");
    assert!(
        d.contains("ContainersReady ContainersNotReady containers with unready status"),
        "{d}"
    );
    assert!(
        !d.contains("PodScheduled"),
        "a True condition is not news: {d}"
    );
    assert!(
        d.ends_with(
            "last events (1 earlier event(s) not shown): Pulled: image present | Failed: Error: \
             context deadline exceeded"
        ),
        "{d}"
    );
    assert_eq!(seat::pod_diagnosis(None, None, 5), "no pod was created");
}

#[test]
fn an_unreadable_events_response_is_said_in_the_pod_diagnosis() {
    let p = json!({"status": {"phase": "Pending"}});
    for (code, stderr, expected) in [
        (
            Some(1),
            "Forbidden: events is forbidden",
            "Forbidden: events is forbidden",
        ),
        (Some(1), "", "kubectl exited 1"),
        (None, "", "kubectl timed out"),
    ] {
        let result = super::Called {
            code,
            stdout: br#"{"items":[]}"#.to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };
        assert_eq!(
            seat::pod_diagnosis_with_events(Some(&p), &result, 5, &[]),
            format!("phase Pending; events unavailable: {expected}")
        );
    }
    let mut result = super::Called {
        code: Some(0),
        stdout: b"not JSON".to_vec(),
        stderr: Vec::new(),
    };
    let parse_error = serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap_err();
    assert_eq!(
        seat::pod_diagnosis_with_events(Some(&p), &result, 5, &[]),
        format!("phase Pending; events unavailable: {parse_error}")
    );
    result.stdout = br#"{"items":[{"reason":"Failed","message":"operation timeout"}]}"#.to_vec();
    assert_eq!(
        seat::pod_diagnosis_with_events(Some(&p), &result, 5, &[]),
        "phase Pending; last events: Failed: operation timeout"
    );
}

#[test]
fn plain_create_timeouts_wait_but_denials_and_config_errors_still_refuse() {
    for message in [
        "operation timeout",
        "operation timed out",
        "timed out",
        "deadline exceeded",
    ] {
        assert!(seat::transient_create_error(message), "{message}");
        let denied = format!("permission denied: {message}");
        assert!(!seat::transient_create_error(&denied), "{denied}");
        for reason in [
            "CreateContainerError",
            "RunContainerError",
            "CreateContainerConfigError",
        ] {
            let mut p = pod("seat-1-abc", "u1", "Pending", false);
            p["status"]["containerStatuses"][0]["state"] =
                json!({"waiting": {"reason": reason, "message": message}});
            let expected = if reason == "CreateContainerConfigError" {
                PodState::Failed(FloorUnarmed::PolicyRefused, message.into())
            } else {
                PodState::Waiting
            };
            assert_eq!(seat::pod_state(&p, "seat-1-abc", "u1"), expected);
            p["status"]["containerStatuses"][0]["state"]["waiting"]["message"] = json!(denied);
            assert_eq!(
                seat::pod_state(&p, "seat-1-abc", "u1"),
                PodState::Failed(FloorUnarmed::PolicyRefused, denied.clone())
            );
        }
    }
}

#[test]
fn pod_diagnosis_clips_each_long_message_at_a_character_boundary() {
    let long = "界".repeat(300);
    let p = json!({"status": {
        "phase": "Pending",
        "conditions": [{"type": "Ready", "status": "False", "reason": "Waiting", "message": long}],
        "initContainerStatuses": [{"name": "sync", "state": {"terminated": {"reason": "Error", "message": long}}}],
        "containerStatuses": [{"name": "seat", "state": {"waiting": {"reason": "CreateContainerError", "message": long}}}]
    }});
    let events = json!({"items": [{"reason": "Failed", "message": long}]});
    let seen = vec![format!("container earlier waiting Error {long}")];
    let d = seat::pod_diagnosis_seen(Some(&p), Some(&events), 5, &seen);
    for part in d.split("; ").skip(1) {
        let message = part
            .strip_prefix("last events: Failed: ")
            .or_else(|| part.strip_prefix("seen during the wait: "))
            .unwrap_or(part);
        assert_eq!(message.chars().count(), 200, "{part}");
    }
    assert!(d.contains("Ready Waiting "));
    assert!(d.contains("init container sync terminated Error "));
    assert!(d.contains("container seat waiting CreateContainerError "));
    assert!(d.contains("seen during the wait: container earlier waiting Error "));
    assert!(d.contains("last events: Failed: "));
}

#[test]
fn a_bounded_pod_diagnosis_keeps_conditions_and_all_five_events_in_the_suffix() {
    let long = "x".repeat(1000);
    let p = json!({"status": {"phase": "Pending", "conditions": [
        {"type": "PodScheduled", "status": "False", "message": long},
        {"type": "Initialized", "status": "False", "message": long},
        {"type": "ContainersReady", "status": "False", "message": long},
        {"type": "Ready", "status": "False", "message": long}
    ]}});
    let events = json!({"items": (1..=5).map(|i| json!({
        "reason": format!("Event{i}"), "message": long
    })).collect::<Vec<_>>()});
    let d = seat::pod_diagnosis(Some(&p), Some(&events), 5);
    let suffix = crate::runtime_wait::diagnosis_suffix(&d);
    assert!(suffix.len() < 4096, "{}", suffix.len());
    for condition in ["PodScheduled", "Initialized", "ContainersReady", "Ready"] {
        assert!(suffix.contains(condition), "{suffix}");
    }
    for i in 1..=5 {
        assert!(suffix.contains(&format!("Event{i}: ")), "{suffix}");
    }
}

#[test]
fn event_recency_prefers_series_then_last_timestamp_then_event_time_then_creation() {
    let events = json!({"items": [
        {"reason": "Series", "message": "s", "series": {"lastObservedTime": "2026-11-17T05:00:00Z"},
         "lastTimestamp": "2026-11-17T00:00:00Z", "eventTime": "2026-11-17T00:00:00Z"},
        {"reason": "Last", "message": "l", "lastTimestamp": "2026-11-17T04:00:00Z",
         "eventTime": "2026-11-17T00:00:00Z"},
        {"reason": "Event", "message": "e", "series": {"lastObservedTime": ""},
         "lastTimestamp": "", "eventTime": "2026-11-17T03:00:00Z", "metadata": {"creationTimestamp": "2026-11-17T09:00:00Z"}},
        {"reason": "Created", "message": "c", "metadata": {"creationTimestamp": "2026-11-17T02:00:00Z"}}
    ]});
    let d = seat::pod_diagnosis(None, Some(&events), 4);
    assert!(
        d.ends_with("last events: Created: c | Event: e | Last: l | Series: s"),
        "{d}"
    );
    let d = seat::pod_diagnosis(None, Some(&events), 1);
    assert!(
        d.ends_with("last events (3 earlier event(s) not shown): Series: s"),
        "{d}"
    );
}

/// (core#1491) An arm whose pod was not ready is retried after each backoff, then reports how many
/// arms were tried; a refusal (policy, admission) is never retried, and a run that ended during a
/// backoff gets no further arm.
#[test]
fn a_not_ready_arm_is_retried_with_the_backoff_and_a_refusal_is_not() {
    use std::cell::{Cell, RefCell};
    let slept = RefCell::new(Vec::new());
    let sleep = |d: Duration| slept.borrow_mut().push(d);
    let never = || false;
    let tries = Cell::new(0);
    let r: Result<(), _> = seat::retry_arm(&seat::ARM_RETRY_BACKOFF, &sleep, &never, || {
        tries.set(tries.get() + 1);
        Err((FloorUnarmed::PodNotReady, "not ready".into()))
    });
    let (why, msg) = r.unwrap_err();
    assert_eq!((why, tries.get()), (FloorUnarmed::PodNotReady, 3));
    assert!(msg.contains("after 3 arm attempts"), "{msg}");
    assert_eq!(*slept.borrow(), seat::ARM_RETRY_BACKOFF.to_vec());
    tries.set(0);
    let r = seat::retry_arm(&seat::ARM_RETRY_BACKOFF, &|_| {}, &never, || {
        tries.set(tries.get() + 1);
        if tries.get() == 1 {
            Err((FloorUnarmed::PodNotReady, "slow".into()))
        } else {
            Ok(7)
        }
    });
    assert_eq!((r.unwrap(), tries.get()), (7, 2));
    for refusal in [FloorUnarmed::PolicyRefused, FloorUnarmed::AdmissionPending] {
        tries.set(0);
        let r: Result<(), _> = seat::retry_arm(&seat::ARM_RETRY_BACKOFF, &|_| {}, &never, || {
            tries.set(tries.get() + 1);
            Err((refusal, "refused".into()))
        });
        assert_eq!((r.unwrap_err().0, tries.get()), (refusal, 1), "{refusal:?}");
    }
    // The run ended during the first backoff: no second arm.
    tries.set(0);
    let ended = Cell::new(false);
    let r: Result<(), _> = seat::retry_arm(
        &seat::ARM_RETRY_BACKOFF,
        &|_| ended.set(true),
        &|| ended.get(),
        || {
            tries.set(tries.get() + 1);
            Err((FloorUnarmed::PodNotReady, "slow".into()))
        },
    );
    assert_eq!(tries.get(), 1);
    assert!(r.unwrap_err().1.contains("the run ended"));
}

/// (codex on #1527) A denial is never read as a timeout, whatever path it names; an admission
/// webhook's denial is a policy refusal.
#[test]
fn a_denial_is_never_a_timeout_and_a_webhook_denial_is_a_policy_refusal() {
    for m in [
        "error mounting /replica/home/timeout-service: permission denied",
        "context deadline exceeded: denied by policy",
    ] {
        assert!(!seat::transient_create_error(m), "{m}");
    }
    assert!(seat::transient_create_error(
        "rpc error: code = DeadlineExceeded desc = context deadline exceeded"
    ));
    let sbx = json!({"status": {"conditions": [{"type": "Ready", "status": "False",
        "message": "admission webhook \"policy.example\" denied the request: unsigned image"}]}});
    assert_eq!(
        seat::sandbox_refusal(&sbx).map(|r| r.0),
        Some(FloorUnarmed::PolicyRefused)
    );
}

/// (codex on #1527) The last events are the most recent by when they last occurred, not list order.
#[test]
fn the_diagnosis_takes_the_most_recent_events() {
    let events = json!({"items": [
        {"reason": "Old", "message": "o", "lastTimestamp": "2026-11-17T01:00:00Z"},
        {"reason": "Newest", "message": "n", "lastTimestamp": "2026-11-17T03:00:00Z"},
        {"reason": "Middle", "message": "m", "lastTimestamp": "2026-11-17T02:00:00Z"}
    ]});
    let d = seat::pod_diagnosis(None, Some(&events), 2);
    // (lane-c2, HIDE) The event left out is counted, never silently cut.
    assert!(
        d.ends_with("last events (1 earlier event(s) not shown): Middle: m | Newest: n"),
        "{d}"
    );
    // (codex confirmation, HIDE) The runtime's own error the poll waited through is kept even
    // with no event: the container's waiting state is in the diagnosis.
    let mut p = json!({"status": {"phase": "Pending", "containerStatuses": [
        {"name": "seat", "state": {"waiting": {
            "reason": "CreateContainerError", "message": "context deadline exceeded"}}}
    ]}});
    let d = seat::pod_diagnosis(Some(&p), None, 5);
    assert!(
        d.contains("container seat waiting CreateContainerError context deadline exceeded"),
        "{d}"
    );
    p["status"]["initContainerStatuses"] =
        json!([{"name": "sync", "state": {"terminated": {"reason": "Error", "message": "x"}}}]);
    assert!(
        seat::pod_diagnosis(Some(&p), None, 5).contains("init container sync terminated Error x")
    );
    let all = seat::pod_diagnosis(None, Some(&events), 5);
    assert!(
        all.ends_with("last events: Old: o | Middle: m | Newest: n"),
        "{all}"
    );
}

/// (coord codex-check on #1527, HIDE) The two-snapshot repro: one poll sees the runtime's
/// `CreateContainerError: context deadline exceeded` (transient, so the wait goes on), the next
/// only `ContainerCreating`, and no event is readable. The error is still in the diagnosis.
#[test]
fn a_transient_runtime_error_seen_by_an_earlier_poll_stays_in_the_diagnosis() {
    let snap = |reason: &str, message: &str| {
        json!({"status": {"phase": "Pending", "containerStatuses": [
            {"name": "seat", "state": {"waiting": {"reason": reason, "message": message}}}
        ]}})
    };
    let first = snap("CreateContainerError", "context deadline exceeded");
    let last = snap("ContainerCreating", "");
    let mut seen = Vec::new();
    seat::note_states(&mut seen, &first);
    seat::note_states(&mut seen, &last);
    seat::note_states(&mut seen, &last);
    let d = seat::pod_diagnosis_seen(Some(&last), None, 5, &seen);
    assert!(
        d.contains("seen during the wait: container seat waiting CreateContainerError context deadline exceeded"),
        "{d}"
    );
    assert_eq!(d.matches("ContainerCreating").count(), 1, "no repeat: {d}");
    // Without an earlier error, nothing is added.
    let mut quiet = Vec::new();
    seat::note_states(&mut quiet, &last);
    assert!(!seat::pod_diagnosis_seen(Some(&last), None, 5, &quiet).contains("seen during"));
}

/// (coord codex-check on #1527, HIDE) First poll: the runtime's `CreateContainerError: context
/// deadline exceeded` (transient, the wait goes on); next poll: the GET fails. The API error the
/// arm returns still carries what the earlier poll saw.
#[test]
fn a_failed_read_after_a_runtime_error_keeps_that_error() {
    let first = json!({"status": {"phase": "Pending", "containerStatuses": [
        {"name": "seat", "state": {"waiting": {
            "reason": "CreateContainerError", "message": "context deadline exceeded"}}}
    ]}});
    let mut seen = Vec::new();
    seat::note_states(&mut seen, &first);
    let msg = seat::with_history("Unable to connect to the server".to_string(), &seen);
    assert!(msg.starts_with("Unable to connect to the server"), "{msg}");
    assert!(
        msg.contains("CreateContainerError context deadline exceeded"),
        "{msg}"
    );
    assert_eq!(
        seat::with_history("e".into(), &[]),
        "e",
        "nothing seen, nothing added"
    );
}

// core#1699: a first arm and a restarted engine's first arm must admit later worker launches.
#[cfg(unix)]
#[test]
fn a_k8s_seat_arm_leaves_the_state_home_admissible_across_restarts() {
    let host = tmp("admissible-host");
    let home = tmp("admissible-home");
    let snapshot = home.join("skills/snapshots/000001");
    std::fs::create_dir_all(&snapshot).unwrap();
    assert_eq!(
        crate::state_home::of_snapshot(&snapshot),
        Some(home.clone())
    );
    git_in(&host, &["init", "-q", "-b", "main"]);
    std::fs::write(host.join("a"), "one").unwrap();
    git_in(&host, &["add", "a"]);
    git_in(&host, &["commit", "-qm", "c1"]);
    let sha = std::process::Command::new("git")
        .hardened()
        .args(["rev-parse", "HEAD"])
        .current_dir(&host)
        .output()
        .unwrap();
    assert!(sha.status.success());
    let sha = String::from_utf8(sha.stdout).unwrap().trim().to_string();
    for arm in ["first arm", "first arm after restart"] {
        // Fresh process-local state; the second arm wipes and re-creates the persisted cache.
        let builders = std::sync::Mutex::new(seat::GitBuilders::default());
        seat::GitBuild {
            tree: &host,
            sha: &sha,
            branch: &Some("refs/heads/main".to_string()),
            masks: &[],
            budget: sync::MAX_STREAM,
        }
        .build(&builders, Some(&home))
        .unwrap();
        assert!(home.join("k8s-replica-git").is_dir(), "{arm}");
        let unregistered = crate::state_home::survey(&home).unwrap();
        assert!(unregistered.is_empty(), "{arm}: found {unregistered:?}");
        let admitted = crate::state_home::intake_fence_for(Some(&snapshot));
        assert!(admitted.is_ok(), "{arm}: next launch refused: {admitted:?}");
    }
    for p in [host, home] {
        let _ = std::fs::remove_dir_all(p);
    }
}

// (core#1441) A kept builder fetches the next commit and repairs a broken object store.
#[cfg(unix)]
#[test]
fn k8s_replica_git_builder_is_incremental_and_repairs_corruption() {
    let host = tmp("incremental-host");
    let home = tmp("incremental-home");
    let replica = tmp("incremental-replica");
    git_in(&host, &["init", "-q", "-b", "main"]);
    let builders = std::sync::Mutex::new(seat::GitBuilders::default());
    let build = |branch: &str| {
        let sha = std::process::Command::new("git")
            .hardened()
            .args(["rev-parse", "HEAD"])
            .current_dir(&host)
            .output()
            .unwrap();
        let sha = String::from_utf8(sha.stdout).unwrap().trim().to_string();
        let entries = seat::GitBuild {
            tree: &host,
            sha: &sha,
            branch: &Some(format!("refs/heads/{branch}")),
            masks: &[],
            budget: sync::MAX_STREAM,
        }
        .build(&builders, Some(&home))
        .unwrap();
        (sha, entries)
    };
    seat::FRESH_BUILDS.with(|n| n.set(0));
    std::fs::write(host.join("a"), "one").unwrap();
    git_in(&host, &["add", "a"]);
    git_in(&host, &["commit", "-qm", "c1"]);
    build("main");
    std::fs::write(host.join("a"), "two").unwrap();
    git_in(&host, &["commit", "-qam", "c2"]);
    let (c2, entries) = build("main");
    seat::FRESH_BUILDS.with(|n| assert_eq!(n.get(), 1));
    let body = sync::encode(&[Root {
        replica: "repo/.git".into(),
        mode: Mode::Git,
        entries,
    }]);
    let out = codec(
        &replica,
        &["unpack", &json!([["repo/.git", "git", []]]).to_string()],
        &body,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let checkout = replica.join("repo");
    git_in(&checkout, &["checkout", "-q", "--", "."]);
    let output = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .hardened()
            .args(args)
            .current_dir(&checkout)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    assert_eq!(output(&["rev-parse", "HEAD"]).trim(), c2);
    assert_eq!(output(&["symbolic-ref", "HEAD"]).trim(), "refs/heads/main");
    assert_eq!(
        output(&["log", "-2", "--format=%s"])
            .lines()
            .collect::<Vec<_>>(),
        ["c2", "c1"]
    );
    assert!(output(&["status", "--porcelain"]).is_empty());
    let kept = std::fs::read_dir(home.join("k8s-replica-git"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::remove_dir_all(kept.join(".git/objects")).unwrap();
    let (repaired, entries) = build("main");
    assert_eq!(repaired, c2);
    let body = sync::encode(&[Root {
        replica: "repo/.git".into(),
        mode: Mode::Git,
        entries,
    }]);
    let out = codec(
        &replica,
        &["unpack", &json!([["repo/.git", "git", []]]).to_string()],
        &body,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(output(&["rev-parse", "HEAD"]).trim(), c2);
    assert_eq!(output(&["log", "-2", "--format=%s"]).lines().count(), 2);
    assert!(output(&["status", "--porcelain"]).is_empty());
    seat::FRESH_BUILDS.with(|n| assert_eq!(n.get(), 2));
    git_in(&host, &["checkout", "-qb", "other"]);
    build("other");
    seat::FRESH_BUILDS.with(|n| assert_eq!(n.get(), 3));
    for p in [host, home, replica] {
        let _ = std::fs::remove_dir_all(p);
    }
}

// (core#1441) Published generations use their identity; lookalikes still walk.
#[cfg(unix)]
#[test]
fn k8s_skills_generation_signs_without_a_walk_and_reuses_its_read() {
    let home = tmp("generation-signature");
    let host = home.join("skills/snapshots/123");
    std::fs::create_dir_all(host.join("skills/a")).unwrap();
    std::fs::write(host.join("skills/a/SKILL.md"), "skill").unwrap();
    let index = host.join("snapshot.json");
    std::fs::write(&index, r#"{"gen":"123","contentHash":"sha256:abc"}"#).unwrap();
    let r = PlanRoot {
        host: host.clone(),
        mode: Mode::Plain,
        skip: vec![],
        back: false,
        files: vec![],
        masks: vec![],
        repo: false,
        ro: false,
        optional: false,
    };
    seat::SIG_WALKS.with(|n| n.set(0));
    let before = seat::HOST_READS.with(|n| n.get());
    let copy = || {
        seat::copy_in_stream(
            None,
            &home,
            std::slice::from_ref(&r),
            false,
            &Default::default(),
        )
        .unwrap()
    };
    let first = copy();
    assert_eq!(copy().0, first.0);
    seat::SIG_WALKS.with(|n| assert_eq!(n.get(), 0));
    seat::HOST_READS.with(|n| assert_eq!(n.get(), before + 1));
    std::fs::write(&index, r#"{"gen":"124","contentHash":"sha256:abc"}"#).unwrap();
    copy();
    seat::SIG_WALKS.with(|n| assert!(n.get() > 0));
    let _ = std::fs::remove_dir_all(home);
}

#[cfg(unix)]
fn repo_stamp_fixture() -> (PathBuf, PathBuf, PlanRoot) {
    let host = tmp("repo-stamp-host");
    let replica = tmp("repo-stamp-replica");
    git_in(&host, &["init", "-q"]);
    std::fs::write(host.join("payload"), "stamp payload one").unwrap();
    git_in(&host, &["add", "payload"]);
    let r = PlanRoot {
        host: host.clone(),
        mode: Mode::Plain,
        skip: vec![],
        back: false,
        files: vec![],
        masks: vec![],
        repo: true,
        ro: false,
        optional: false,
    };
    (host, replica, r)
}

#[cfg(unix)]
fn stamped_copy(
    replica: &Path,
    r: &PlanRoot,
    have: std::collections::HashMap<String, String>,
) -> seat::CopyInStream {
    let mut stamps = seat::RepoStamps::new(None, std::slice::from_ref(r)).unwrap();
    stamps.have = have;
    let result = seat::copy_in_stream_stamped(
        None,
        &r.host,
        std::slice::from_ref(r),
        false,
        &Default::default(),
        &stamps,
    )
    .unwrap();
    let out = codec(replica, &["unpack", &result.1.to_string()], &result.0);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    result
}

#[cfg(unix)]
fn read_repo_stamps(replica: &Path, rep: &str) -> std::collections::HashMap<String, String> {
    let out = codec(replica, &["stamps", &json!([rep]).to_string()], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[cfg(unix)]
#[test]
fn k8s_repo_stamp_keeps_an_unchanged_repo_without_reading_or_streaming_it() {
    let (host, replica, r) = repo_stamp_fixture();
    let rep = seat::replica_of(None, &host).unwrap();
    let before = seat::HOST_READS.with(|n| n.get());
    let first = stamped_copy(&replica, &r, Default::default());
    let have = read_repo_stamps(&replica, &rep);
    assert!(have.contains_key(&rep));
    let n = seat::HOST_READS.with(|n| n.get());
    let next = stamped_copy(&replica, &r, have);
    assert_eq!(n, before + 1);
    assert!(!sync::decode(&mut first.0.as_slice(), sync::MAX_STREAM)
        .unwrap()
        .is_empty());
    assert!(!next
        .0
        .windows(b"stamp payload one".len())
        .any(|w| w == b"stamp payload one"));
    assert!(sync::decode(&mut next.0.as_slice(), sync::MAX_STREAM)
        .unwrap()
        .is_empty());
    assert_eq!(next.1, json!([]));
    seat::HOST_READS.with(|c| assert_eq!(c.get(), n));
    assert_eq!(
        std::fs::read_to_string(replica.join(&rep).join("payload")).unwrap(),
        "stamp payload one"
    );
    for p in [host, replica] {
        let _ = std::fs::remove_dir_all(p);
    }
}

#[cfg(unix)]
#[test]
fn k8s_repo_stamp_changes_when_a_host_file_is_edited() {
    let (host, replica, r) = repo_stamp_fixture();
    let rep = seat::replica_of(None, &host).unwrap();
    stamped_copy(&replica, &r, Default::default());
    let have = read_repo_stamps(&replica, &rep);
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(host.join("payload"), "edited payload").unwrap();
    let next = stamped_copy(&replica, &r, have.clone());
    assert!(!sync::decode(&mut next.0.as_slice(), sync::MAX_STREAM)
        .unwrap()
        .is_empty());
    assert_ne!(read_repo_stamps(&replica, &rep), have);
    assert_eq!(
        std::fs::read_to_string(replica.join(&rep).join("payload")).unwrap(),
        "edited payload"
    );
    for p in [host, replica] {
        let _ = std::fs::remove_dir_all(p);
    }
}

#[cfg(unix)]
#[test]
fn k8s_a_plain_ancestor_stream_invalidates_the_repo_stamp() {
    let (host, replica, r) = repo_stamp_fixture();
    let rep = seat::replica_of(None, &host).unwrap();
    stamped_copy(&replica, &r, Default::default());
    let ancestor = rep.rsplit_once('/').unwrap().0;
    let body = sync::encode(&[Root {
        replica: ancestor.into(),
        mode: Mode::Plain,
        entries: vec![],
    }]);
    let out = codec(
        &replica,
        &["unpack", &json!([[ancestor, "plain", []]]).to_string()],
        &body,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(read_repo_stamps(&replica, &rep).is_empty());
    // The codec refuses a root in its own stamp store, whatever the host sends.
    let forged = sync::encode(&[Root {
        replica: ".wicked-stamps".into(),
        mode: Mode::Plain,
        entries: vec![],
    }]);
    let out = codec(
        &replica,
        &[
            "unpack",
            &json!([[".wicked-stamps", "plain", []]]).to_string(),
        ],
        &forged,
    );
    assert!(
        !out.status.success(),
        "a root in the stamp store was unpacked"
    );
    for p in [host, replica] {
        let _ = std::fs::remove_dir_all(p);
    }
}

#[cfg(unix)]
#[test]
fn k8s_a_repo_stamp_from_another_secret_is_not_kept() {
    let (host, replica, r) = repo_stamp_fixture();
    let rep = seat::replica_of(None, &host).unwrap();
    let (body, mut plan, _, _, _) = stamped_copy(&replica, &r, Default::default());
    let foreign = seat::repo_key(&r, &rep, &[0x42; 32]).unwrap();
    plan[0][4] = json!(foreign);
    let out = codec(&replica, &["unpack", &plan.to_string()], &body);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let have = read_repo_stamps(&replica, &rep);
    assert_eq!(have.get(&rep), Some(&foreign));
    let next = stamped_copy(&replica, &r, have);
    assert!(!sync::decode(&mut next.0.as_slice(), sync::MAX_STREAM)
        .unwrap()
        .is_empty());
    assert_ne!(read_repo_stamps(&replica, &rep).get(&rep), Some(&foreign));
    for p in [host, replica] {
        let _ = std::fs::remove_dir_all(p);
    }
}

#[cfg(unix)]
#[test]
fn k8s_a_live_writable_seat_mount_prevents_a_repo_stamp_hit() {
    let (host, replica, r) = repo_stamp_fixture();
    let rep = seat::replica_of(None, &host).unwrap();
    let kube = super::Kube {
        kubectl: host.join("unused-kubectl"),
        kubeconfig: None,
        context: "t".into(),
        owner: "o".into(),
    };
    for mount in [
        host.clone(),
        host.parent().unwrap().to_path_buf(),
        host.join("subdir"),
    ] {
        stamped_copy(&replica, &r, Default::default());
        let have = read_repo_stamps(&replica, &rep);
        let id = format!("stamps-{}", uuid::Uuid::new_v4());
        let live = seat::register_for_test(&kube, &id, &mount, "test");
        let mut stamps = seat::RepoStamps::new(None, std::slice::from_ref(&r)).unwrap();
        stamps.have = have;
        stamps.block_writers("ns");
        let next = seat::copy_in_stream_stamped(
            None,
            &host,
            std::slice::from_ref(&r),
            false,
            &Default::default(),
            &stamps,
        )
        .unwrap();
        seat::forget_for_test(&id);
        drop(live);
        assert!(!sync::decode(&mut next.0.as_slice(), sync::MAX_STREAM)
            .unwrap()
            .is_empty());
        let out = codec(&replica, &["unpack", &next.1.to_string()], &next.0);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            read_repo_stamps(&replica, &rep).is_empty(),
            "a live writer left a trusted stamp"
        );
    }
    for p in [host, replica] {
        let _ = std::fs::remove_dir_all(p);
    }
}

// (core#1463) The codec's stamp store can never be a seat-mounted replica root.
#[test]
fn k8s_replica_stamp_storage_cannot_be_a_seat_root() {
    assert!(seat::replica_of(None, Path::new("/.wicked-stamps")).is_err());
    assert!(seat::replica_of(None, Path::new("/.wicked-stamps/guessed-key")).is_err());
    assert!(seat::replica_of(None, Path::new("/.wicked-stamps-other")).is_ok());
}

/// (core#1747) One arm's copy-in, as `copy_in` runs it on a chat's plan: the replica's stamps are
/// read, the host roots signed, the stream unpacked by the pod codec. Returns the stream and the
/// `copy_in_read` arm-timing detail.
#[cfg(unix)]
fn chat_turn(replica: &Path, tree: &Path, roots: &[PlanRoot]) -> (Vec<u8>, String) {
    let mut stamps = seat::RepoStamps::new(None, roots).unwrap();
    let paths: Vec<String> = roots
        .iter()
        .filter(|r| r.stamped())
        .map(|r| seat::replica_of(None, &r.host).unwrap())
        .collect();
    let out = codec(replica, &["stamps", &json!(paths).to_string()], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stamps.have = serde_json::from_slice(&out.stdout).unwrap();
    super::clock::begin();
    let (body, plan, ..) =
        seat::copy_in_stream_stamped(None, tree, roots, false, &Default::default(), &stamps)
            .unwrap();
    let (phases, _) = super::clock::take().unwrap();
    let detail = phases
        .into_iter()
        .find(|p| p.name == "copy_in_read")
        .and_then(|p| p.detail)
        .unwrap();
    let out = codec(replica, &["unpack", &plan.to_string()], &body);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    (body, detail)
}

/// (core#1747) A chat's follow-up turn arms in the same replica as its first: every read-only root
/// whose host state did not change (its repositories, the skills snapshot, the attachments
/// directory) is kept there, never read or sent again, and the arm timing says which roots were
/// kept and which were re-read and why. The mounts are those `container_floor::mount_plan` and
/// `add_read_roots` make for a read-only chat unit (src/container_floor.rs:1437-1455, 3273-3287):
/// the chat's scratch tree, a seat home read-write, the run's read roots read-only.
#[cfg(unix)]
#[test]
fn k8s_a_follow_up_chat_turn_keeps_every_unchanged_read_only_root() {
    let host = tmp("chat-follow-up");
    let replica = tmp("chat-follow-up-replica");
    let tree = host.join("chats/c1");
    let home = host.join("worker/homes/claude");
    let repo = host.join("repos/estate");
    let skills = host.join("state/skills/snapshots/7");
    let attach = host.join("state/attachments/c1");
    for d in [&tree, &home, &repo, &skills.join("a"), &attach] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(tree.join("AGENTS.md"), "scope").unwrap();
    std::fs::write(home.join("settings.json"), "{}").unwrap();
    git_in(&repo, &["init", "-q"]);
    std::fs::write(repo.join("lib.rs"), "repository payload").unwrap();
    git_in(&repo, &["add", "lib.rs"]);
    std::fs::write(skills.join("a/SKILL.md"), "skills payload").unwrap();
    std::fs::write(attach.join("notes.txt"), "attachment one").unwrap();
    let bind = |path: &PathBuf, rw: bool| PlanMount::Bind {
        path: path.clone(),
        rw,
    };
    let mounts = vec![
        bind(&home, true),
        bind(&tree, false),
        bind(&repo, false),
        bind(&skills, false),
        bind(&attach, false),
    ];
    let (mut roots, _) = seat::plan(&tree, true, &mounts, &[], &[], None);
    seat::mark_homes(&mut roots, std::slice::from_ref(&home), None);
    let rep = |p: &Path| seat::replica_of(None, p).unwrap();
    let stamped: Vec<String> = roots
        .iter()
        .filter(|r| r.stamped())
        .map(|r| rep(&r.host))
        .collect();
    assert_eq!(stamped.len(), 3, "{roots:?}");
    for p in [&repo, &skills, &attach] {
        assert!(
            stamped.contains(&rep(p)),
            "{} is not kept: {roots:?}",
            p.display()
        );
    }

    // Turn 1: nothing is in the replica yet; every read-only root is read, and says why.
    let (_, first) = chat_turn(&replica, &tree, &roots);
    assert!(first.starts_with("kept 0 re-read 3;"), "{first}");
    assert!(
        first.contains(&format!("{}:1/", rep(&repo)))
            && first.contains(&format!("{}:", rep(&repo)))
            && first.contains(":re-read:no-stamp"),
        "{first}"
    );

    // Turn 2, nothing changed on the host: the read-only roots stay in the replica.
    let reads = seat::HOST_READS.with(|c| c.get());
    let (body, second) = chat_turn(&replica, &tree, &roots);
    assert!(second.starts_with("kept 3 re-read 0;"), "{second}");
    for p in [&repo, &skills, &attach] {
        assert!(second.contains(&format!("{}:kept", rep(p))), "{second}");
    }
    for payload in ["repository payload", "skills payload", "attachment one"] {
        assert!(
            !body.windows(payload.len()).any(|w| w == payload.as_bytes()),
            "{payload} was sent again"
        );
    }
    assert_eq!(
        seat::HOST_READS.with(|c| c.get()),
        reads,
        "a kept root was read from the host"
    );
    // What the seat reads is still there, as the host has it.
    assert_eq!(
        std::fs::read_to_string(replica.join(rep(&skills)).join("a/SKILL.md")).unwrap(),
        "skills payload"
    );

    // Turn 3, one attachment changed: only that root is re-read, and the timing says why.
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(attach.join("notes.txt"), "attachment two").unwrap();
    let (_, third) = chat_turn(&replica, &tree, &roots);
    assert!(third.starts_with("kept 2 re-read 1;"), "{third}");
    assert!(
        third.contains(&format!("{}:1/14/", rep(&attach))) && third.contains(":re-read:changed"),
        "{third}"
    );
    assert_eq!(
        std::fs::read_to_string(replica.join(rep(&attach)).join("notes.txt")).unwrap(),
        "attachment two"
    );
    // Turn 4 keeps all three again.
    let (_, fourth) = chat_turn(&replica, &tree, &roots);
    assert!(fourth.starts_with("kept 3 re-read 0;"), "{fourth}");

    // Another replica of this daemon (another run's chat on the same roots): its first copy of
    // the plain roots comes from the read cache, and is stamped all the same, so its next turn
    // keeps them too.
    let replica2 = tmp("chat-follow-up-replica-2");
    let reads = seat::HOST_READS.with(|c| c.get());
    let (_, other) = chat_turn(&replica2, &tree, &roots);
    assert!(other.starts_with("kept 0 re-read 3;"), "{other}");
    assert_eq!(
        seat::HOST_READS.with(|c| c.get()),
        reads + 1,
        "only the repository is read from the host: {other}"
    );
    let (_, other_next) = chat_turn(&replica2, &tree, &roots);
    assert!(other_next.starts_with("kept 3 re-read 0;"), "{other_next}");
    for p in [host, replica, replica2] {
        let _ = std::fs::remove_dir_all(p);
    }
}

/// (core#1747) A read-only root a live seat of the namespace can write is never kept, and the arm
/// timing names the writer as the reason.
#[cfg(unix)]
#[test]
fn k8s_a_writable_overlap_names_the_writer_as_why_a_root_is_re_read() {
    let host = tmp("chat-writer");
    let replica = tmp("chat-writer-replica");
    let plugins = host.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(plugins.join("p.md"), "plugin").unwrap();
    let mounts = vec![PlanMount::Bind {
        path: plugins.clone(),
        rw: false,
    }];
    let (roots, _) = seat::plan(&host.join("t"), true, &mounts, &[], &[], None);
    let plugin_roots: Vec<PlanRoot> = roots.into_iter().filter(|r| r.host == plugins).collect();
    assert!(plugin_roots[0].stamped());
    chat_turn(&replica, &host, &plugin_roots);
    let kube = super::Kube {
        kubectl: host.join("unused-kubectl"),
        kubeconfig: None,
        context: "t".into(),
        owner: "o".into(),
    };
    let id = format!("writer-{}", uuid::Uuid::new_v4());
    let live = seat::register_for_test(&kube, &id, &host, "test");
    let mut stamps = seat::RepoStamps::new(None, &plugin_roots).unwrap();
    let rep = seat::replica_of(None, &plugins).unwrap();
    stamps.have = read_repo_stamps(&replica, &rep);
    stamps.block_writers("ns");
    super::clock::begin();
    seat::copy_in_stream_stamped(
        None,
        &host,
        &plugin_roots,
        false,
        &Default::default(),
        &stamps,
    )
    .unwrap();
    let (phases, _) = super::clock::take().unwrap();
    seat::forget_for_test(&id);
    drop(live);
    let detail = phases
        .into_iter()
        .find(|p| p.name == "copy_in_read")
        .and_then(|p| p.detail)
        .unwrap();
    assert!(
        detail.starts_with("kept 0 re-read 1;") && detail.contains(":re-read:writer"),
        "{detail}"
    );
    for p in [host, replica] {
        let _ = std::fs::remove_dir_all(p);
    }
}

// (core#1541) Every failed arm contributes evidence, with the final attempt first.
#[test]
fn a_not_ready_arm_keeps_each_attempts_diagnosis() {
    let mut tries = 0;
    let result: Result<(), _> =
        seat::retry_arm(&seat::ARM_RETRY_BACKOFF, &|_| {}, &|| false, || {
            tries += 1;
            Err((FloorUnarmed::PodNotReady, format!("diagnosis {tries}")))
        });
    assert_eq!(
        result.unwrap_err().1,
        "diagnosis 3 (after 3 arm attempts; earlier: attempt 1: diagnosis 1 | \
         attempt 2: diagnosis 2)"
    );
}

#[test]
fn earlier_arm_diagnoses_are_clipped_at_a_character_boundary() {
    let mut tries = 0;
    let result: Result<(), _> = seat::retry_arm(&[Duration::ZERO], &|_| {}, &|| false, || {
        tries += 1;
        let msg = if tries == 1 {
            format!("{}omitted", "é".repeat(1001))
        } else {
            "last diagnosis".into()
        };
        Err((FloorUnarmed::PodNotReady, msg))
    });
    assert_eq!(
        result.unwrap_err().1,
        format!(
            "last diagnosis (after 2 arm attempts; earlier: attempt 1: {})",
            "é".repeat(1000)
        )
    );
}

#[test]
fn ending_during_arm_backoff_keeps_earlier_attempts() {
    use std::cell::Cell;

    let ended = Cell::new(false);
    let tries = Cell::new(0);
    let result: Result<(), _> = seat::retry_arm(
        &seat::ARM_RETRY_BACKOFF,
        &|_| ended.set(tries.get() == 2),
        &|| ended.get(),
        || {
            tries.set(tries.get() + 1);
            Err((
                FloorUnarmed::PodNotReady,
                format!("diagnosis {}", tries.get()),
            ))
        },
    );
    let msg = result.unwrap_err().1;
    assert_eq!(tries.get(), 2);
    assert!(msg.starts_with("diagnosis 2"), "{msg}");
    assert!(
        msg.contains("the run ended during the arm's backoff"),
        "{msg}"
    );
    assert!(
        msg.contains("after 2 arm attempts; earlier: attempt 1: diagnosis 1"),
        "{msg}"
    );
}

#[test]
fn an_api_error_keeps_the_last_pod_diagnosis_and_wait_history() {
    let last = json!({"status": {"phase": "Pending", "conditions": [
        {"type": "Ready", "status": "False", "reason": "ContainersNotReady"}
    ]}});
    let diagnosis = seat::pod_diagnosis(Some(&last), None, 5);
    let seen =
        vec!["container seat waiting CreateContainerError context deadline exceeded".to_string()];
    let msg = seat::api_error_message("Unable to connect to the server", &diagnosis, &seen);
    assert!(msg.starts_with("Unable to connect to the server"), "{msg}");
    assert!(msg.contains("phase Pending"), "{msg}");
    assert!(msg.contains("Ready ContainersNotReady"), "{msg}");
    assert!(
        msg.contains("seen during the wait: container seat waiting CreateContainerError"),
        "{msg}"
    );
}

/// Restores the container runtime choice a warm fixture set (its tests hold the env lock).
#[cfg(unix)]
struct WarmRuntime(Option<std::ffi::OsString>);

#[cfg(unix)]
impl Drop for WarmRuntime {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var(crate::container_floor::RUNTIME_ENV, value),
            None => std::env::remove_var(crate::container_floor::RUNTIME_ENV),
        }
    }
}

// Exercise the real warm arm/copy-in/settle path against a stateful API stub. The caller holds
// `ENV_LOCK` for writing: the fixture selects a stand-in container runtime.
#[cfg(unix)]
fn warm_fixture() -> (PathBuf, super::Kube, seat::WarmKey, WarmRuntime) {
    let dir = tmp("warm-judge");
    let runtime = WarmRuntime(std::env::var_os(crate::container_floor::RUNTIME_ENV));
    // The arm loads images through the node's store; a docker-runtime node shares the
    // builder's store, so no load runs and the stand-in only answers `version`.
    let docker = dir.join("docker");
    crate::test_env::install_stub(
        &docker,
        "#!/bin/sh\ncase \"$1\" in version) echo 27;; *) exit 98;; esac\n",
    );
    std::env::set_var(crate::container_floor::RUNTIME_ENV, &docker);
    let kubectl = dir.join("kubectl");
    crate::test_env::install_stub(
        &kubectl,
        r#"#!/usr/bin/env python3
import json, pathlib, sys
base = pathlib.Path(__file__).parent
args = sys.argv[1:]
ns = args[args.index('--namespace') + 1] if '--namespace' in args else ''
a = next(i for i, v in enumerate(args) if v in ('apply', 'get', 'exec', 'delete', 'create'))
args = args[a:]
statefile = base / 'state.json'
state = json.loads(statefile.read_text()) if statefile.exists() else {}
with (base / 'calls').open('a') as f: f.write(json.dumps(args) + '\n')
def save(): statefile.write_text(json.dumps(state))
if args[0] == 'create':
    sys.stdin.buffer.read()
    sys.stderr.write('Error from server (AlreadyExists): exists\n')
    sys.exit(1)
if args[0] == 'apply':
    for d in json.load(sys.stdin)['items']:
        if d['kind'] != 'Sandbox': continue
        name = d['metadata']['name']
        md = dict(d['spec']['podTemplate']['metadata'], name=name, uid=name+'-uid')
        md['ownerReferences'] = [{'kind':'Sandbox', 'name':name, 'uid':name+'-owner'}]
        d['metadata']['uid'] = name+'-owner'
        state[name] = dict(metadata=md, spec=d['spec']['podTemplate']['spec'], _sandbox=d,
            status={'phase':'Running', 'hostIP':'10.0.0.2',
                    'conditions':[{'type':'Ready','status':'True'}],
                    'containerStatuses':[{'name':'seat','ready':True}]})
    save()
elif args[0] == 'get':
    if '-l' in args:
        if 'wicked.dev/role=sync' in ' '.join(args):
            sync = {
                'metadata': {'name': 'sync', 'labels': {
                    'wicked.dev/boot': (base/'boot').read_text()}},
                'status': {'phase': 'Running', 'conditions': [
                    {'type': 'Ready', 'status': 'True'}],
                    'containerStatuses': [{'ready': True}]}}
            print(json.dumps({'items': [sync]}))
        else: print(json.dumps({'items':[]}))
    elif args[1] == 'nodes':
        print(json.dumps({'items': [{'metadata': {'name': 'n', 'uid': 'n-uid'},
            'spec': {}, 'status': {'nodeInfo': {
                'containerRuntimeVersion': 'docker://27.0.0'}}}]}))
    elif args[2] in state:
        obj = json.loads(json.dumps(state[args[2]]))
        if args[1] == 'sandboxes.agents.x-k8s.io': obj = obj['_sandbox']
        else: obj.pop('_sandbox', None)
        print(json.dumps(obj))
elif args[0] == 'delete':
    state.pop(args[2], None); save()
elif args[0] == 'exec':
    if '-i' in args: sys.stdin.buffer.read()
    if args[-1] == '/wicked-unit/rc': print('0')
    elif 'nc_()' in args[-1]:
        import re
        nodes = re.findall(r'node(\d+)', args[-1])
        print('api=closed net=closed gw=open ' + ' '.join('node%s=closed'%p for p in nodes))
    elif 'wicked-warm-quiet' in args[-1]:
        if (base/'fail-reset').exists(): sys.exit(1)
        print('wicked-warm-quiet')
    elif 'wicked-warm-reset' in args[-1]: print('wicked-warm-reset')
"#,
    );
    std::fs::write(dir.join("boot"), super::boot_id()).unwrap();
    std::fs::create_dir(dir.join("tree")).unwrap();
    let key = seat::WarmKey::new(&format!("parent-{}", uuid::Uuid::new_v4()), "codex#2");
    let kube = super::Kube {
        kubectl,
        kubeconfig: None,
        context: "warm-test".into(),
        owner: "owner".into(),
    };
    (dir, kube, key, runtime)
}

#[cfg(unix)]
fn warm_arm(
    kube: &super::Kube,
    key: Option<&seat::WarmKey>,
    tree: &Path,
    image: &str,
) -> (std::sync::Arc<seat::Seat>, Vec<String>) {
    warm_arm_plan(kube, key, tree, image, &[], &[])
}

#[cfg(unix)]
fn warm_arm_plan(
    kube: &super::Kube,
    key: Option<&seat::WarmKey>,
    tree: &Path,
    image: &str,
    mounts: &[PlanMount],
    homes: &[PathBuf],
) -> (std::sync::Arc<seat::Seat>, Vec<String>) {
    let checkout = key.and_then(|k| seat::checkout_warm(k.clone()).unwrap());
    warm_arm_checkout(kube, checkout, tree, image, mounts, homes)
}

#[cfg(unix)]
fn warm_arm_checkout(
    kube: &super::Kube,
    checkout: Option<std::sync::Arc<seat::WarmCheckout>>,
    tree: &Path,
    image: &str,
    mounts: &[PlanMount],
    homes: &[PathBuf],
) -> (std::sync::Arc<seat::Seat>, Vec<String>) {
    try_warm_arm(kube, checkout, tree, image, mounts, homes, true, &[]).unwrap()
}

/// [`warm_arm_checkout`] with the judge's view and write roots spelled, answering the arm.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn try_warm_arm(
    kube: &super::Kube,
    checkout: Option<std::sync::Arc<seat::WarmCheckout>>,
    tree: &Path,
    image: &str,
    mounts: &[PlanMount],
    homes: &[PathBuf],
    view: bool,
    write_roots: &[PathBuf],
) -> Result<(std::sync::Arc<seat::Seat>, Vec<String>), (FloorUnarmed, String)> {
    let call = crate::validator::one_shot_run_id("validator");
    let spec = seat::ArmSpec {
        run_id: &call,
        generation: super::lifecycle(&call),
        ord: 1,
        cli: "codex",
        image,
        floor_image: "floor",
        sync_image: "sync",
        workdir: tree,
        tree,
        view,
        mounts,
        protected: &[],
        write_roots,
        seat_dirs: homes,
        scratch: None,
        seed_base: None,
        memory: "1Gi",
        gateway_ip: None,
        deadline: Duration::from_secs(30),
        role: "judge",
        deps: None,
        optional: &[],
    };
    seat::arm_warm(kube, &spec, vec![], checkout, Some((image, "floor")))
}

// W24: the wrapped runner arms every ungoverned judge with its own tree as its write root
// (`worker_write_roots` = [cwd]); the warm guard refused that shape, and the "cold" fallback
// re-passed the same checkout into the same refusal, so no warm judge could ever arm.
#[cfg(unix)]
#[test]
fn warm_judge_the_warm_guard_refuses_still_arms_a_true_cold_seat_w24() {
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let (dir, kube, key, _runtime) = warm_fixture();
    let tree = dir.join("tree");
    // A write root outside the judge's view: never warm, so the fallback must be a cold arm.
    let notes = dir.join("notes");
    std::fs::create_dir(&notes).unwrap();
    let checkout = seat::checkout_warm(key.clone()).unwrap().unwrap();
    let (cold, _) = try_warm_arm(
        &kube,
        Some(checkout.clone()),
        &tree,
        "image",
        &[],
        &[],
        true,
        std::slice::from_ref(&notes),
    )
    .unwrap_or_else(|(_, e)| panic!("the cold fallback after a warm refusal must arm: {e}"));
    assert_ne!(
        cold.ns,
        super::run_namespace_name(&kube.owner, &key.parent),
        "a cold arm places by its own call, never in the warm parent's namespace"
    );
    assert_ne!(cold.scope.as_deref(), Some(checkout.scope().as_str()));
    seat::reap(&cold.id);
    drop(cold);
    drop(checkout);
    seat::reap_warm_parent(&key.parent);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn warm_judge_with_its_own_tree_as_write_root_reuses_the_pod_and_writes_nothing_back_w24() {
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let (dir, kube, key, _runtime) = warm_fixture();
    let tree = dir.join("tree");
    let out = tree.join("out");
    std::fs::create_dir(&out).unwrap();
    let mounts = [PlanMount::Bind {
        path: out.clone(),
        rw: true,
    }];
    let arm = || {
        let checkout = seat::checkout_warm(key.clone()).unwrap();
        assert!(checkout.is_some(), "the slot is free between calls");
        try_warm_arm(
            &kube,
            checkout,
            &tree,
            "image",
            &mounts,
            &[],
            true,
            std::slice::from_ref(&tree),
        )
        .unwrap_or_else(|(_, e)| panic!("a production-shaped warm judge must arm: {e}"))
    };
    let (a, _) = arm();
    assert_eq!(
        a.ns,
        super::run_namespace_name(&kube.owner, &key.parent),
        "the judge armed warm, in its parent's namespace"
    );
    assert!(
        a.roots.iter().all(|r| !r.back),
        "a warm judge's replica never comes back to the host: {:?}",
        a.roots
    );
    let pod = a.pod.clone();
    seat::settle(&a, Ending::Exited(0)).unwrap();
    drop(a);
    let (b, _) = arm();
    assert_eq!(pod, b.pod, "the second call reuses the warm pod");
    seat::reap(&b.id);
    drop(b);
    seat::reap_warm_parent(&key.parent);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn warm_judge_reuses_physical_pod_with_fresh_execution_and_gateway_identity() {
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let (dir, kube, key, _runtime) = warm_fixture();
    let (a, argv_a) = warm_arm(&kube, Some(&key), &dir.join("tree"), "image");
    let id = a.id.clone();
    let pod = a.pod.clone();
    let unit_a = crate::container_floor::gateway_unit(&id, 1);
    let key_a = crate::seat_gateway::UnitKey::new(&id, &unit_a, "codex");
    seat::settle(&a, Ending::Exited(0)).unwrap();
    drop(a);
    let (b, argv_b) = warm_arm(&kube, Some(&key), &dir.join("tree"), "image");
    let unit_b = crate::container_floor::gateway_unit(&b.id, 1);
    let key_b = crate::seat_gateway::UnitKey::new(&b.id, &unit_b, "codex");
    assert_eq!(pod, b.pod);
    assert_ne!(id, b.id);
    assert_ne!(argv_a.last(), argv_b.last());
    assert_ne!(unit_a, unit_b);
    assert_ne!(key_a.key, key_b.key);
    assert!(seat::lookup(&id).is_none());
    seat::reap(&b.id);
    drop(b);
    seat::reap_warm_parent(&key.parent);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn warm_judge_other_parent_other_seat_and_parentless_calls_are_isolated() {
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let (dir, kube, key, _runtime) = warm_fixture();
    let (a, _) = warm_arm(&kube, Some(&key), &dir.join("tree"), "image");
    let pod = a.pod.clone();
    seat::settle(&a, Ending::Exited(0)).unwrap();
    drop(a);
    for other in [
        Some(seat::WarmKey::new(&key.parent, "codex#3")),
        Some(seat::WarmKey::new("other-parent", "codex#2")),
        None,
        None,
    ] {
        let (b, _) = warm_arm(&kube, other.as_ref(), &dir.join("tree"), "image");
        assert_ne!(pod, b.pod);
        seat::reap(&b.id);
        drop(b);
        if let Some(k) = other {
            seat::reap_warm_parent(&k.parent);
        }
    }
    seat::reap_warm_parent(&key.parent);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn warm_judge_fingerprint_change_and_failed_reset_reap_before_rearm() {
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let (dir, kube, key, _runtime) = warm_fixture();
    let (a, _) = warm_arm(&kube, Some(&key), &dir.join("tree"), "image");
    let pod_a = a.pod.clone();
    seat::settle(&a, Ending::Exited(0)).unwrap();
    drop(a);
    let (b, _) = warm_arm(&kube, Some(&key), &dir.join("tree"), "changed-image");
    assert_ne!(pod_a, b.pod);
    let pod_b = b.pod.clone();
    seat::settle(&b, Ending::Exited(0)).unwrap();
    drop(b);
    std::fs::write(dir.join("fail-reset"), "1").unwrap();
    let (c, _) = warm_arm(&kube, Some(&key), &dir.join("tree"), "changed-image");
    assert_ne!(pod_b, c.pod);
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
    assert!(state.get(&pod_a).is_none());
    assert!(state.get(&pod_b).is_none());
    seat::reap(&c.id);
    drop(c);
    seat::reap_warm_parent(&key.parent);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn warm_judge_busy_checkout_runs_cold_and_leaves_the_cached_pod_for_reuse() {
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let (dir, kube, key, _runtime) = warm_fixture();
    let held = seat::checkout_warm(key.clone()).unwrap().unwrap();
    let (first, _) = warm_arm_checkout(
        &kube,
        Some(held.clone()),
        &dir.join("tree"),
        "image",
        &[],
        &[],
    );
    let pod = first.pod.clone();
    seat::settle(&first, Ending::Exited(0)).unwrap();
    drop(first);
    assert!(seat::checkout_warm(key.clone()).unwrap().is_none());
    // The same-key call gets an ordinary run-id scope while the warm checkout is held.
    std::fs::create_dir(dir.join("cold-tree")).unwrap();
    let (cold, _) = warm_arm(&kube, Some(&key), &dir.join("cold-tree"), "image");
    assert_ne!(pod, cold.pod);
    assert_ne!(cold.scope.as_deref(), Some(held.scope().as_str()));
    assert!(cold.scope.as_deref().unwrap().starts_with("s-"));
    assert_ne!(cold.ns, super::run_namespace_name(&kube.owner, &key.parent));
    seat::settle(&cold, Ending::Exited(0)).unwrap();
    drop(cold);
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
    assert!(
        state.get(&pod).is_some(),
        "cold settle leaves the cached pod alone"
    );
    assert_eq!(state.as_object().unwrap().len(), 1);
    drop(held);
    let (next, _) = warm_arm(&kube, Some(&key), &dir.join("tree"), "image");
    assert_eq!(pod, next.pod);
    seat::reap(&next.id);
    drop(next);
    seat::reap_warm_parent(&key.parent);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn warm_judge_parent_end_refuses_checkout_even_while_busy() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let key = seat::WarmKey::new(&format!("parent-{}", uuid::Uuid::new_v4()), "pi#2");
    let first = seat::checkout_warm(key.clone()).unwrap().unwrap();
    assert!(seat::checkout_warm(key.clone()).unwrap().is_none());
    super::end_run(&key.parent);
    assert!(seat::checkout_warm(key.clone()).is_err());
    seat::reap_warm_parent(&key.parent);
    drop(first);
    assert!(seat::checkout_warm(key).is_err());
}

#[test]
fn warm_reset_preserves_nested_mounts_and_rebuilds_disposable_view() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let mounts = vec![
        PathBuf::from("/tmp/nested/repo"),
        PathBuf::from("/tmp/nested/config"),
    ];
    let script = seat::warm_reset_script(Path::new("/tmp/nested/repo"), &mounts, &[]);
    assert!(script.contains("/tmp/nested/repo"));
    assert!(script.contains("/tmp/nested/config"));
    assert!(script.contains("/wicked-ro"));
    assert!(script.contains("/home/seat"));
    assert!(script.contains("/wicked-unit"));
    assert!(seat::WARM_QUIESCE.contains("kill -9 -1"));
    assert!(seat::WARM_QUIESCE.contains("/proc/"));
}

#[cfg(unix)]
#[test]
fn warm_judge_changed_mounts_uid_owner_or_remaining_lifetime_rearm() {
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let (dir, kube, key, _runtime) = warm_fixture();
    let tree = dir.join("tree");
    let config = dir.join("config");
    std::fs::create_dir(&config).unwrap();
    let mounts = [PlanMount::Bind {
        path: config.clone(),
        rw: true,
    }];
    let (a, _) = warm_arm(&kube, Some(&key), &tree, "image");
    let old = a.pod.clone();
    seat::settle(&a, Ending::Exited(0)).unwrap();
    drop(a);
    let (b, _) = warm_arm_plan(
        &kube,
        Some(&key),
        &tree,
        "image",
        &mounts,
        std::slice::from_ref(&config),
    );
    assert_ne!(old, b.pod, "mount fingerprint changed");
    let mut old = b.pod.clone();
    seat::settle(&b, Ending::Exited(0)).unwrap();
    drop(b);
    for field in ["uid", "owner", "deadline"] {
        if field == "deadline" {
            seat::expire_warm_for_test(&key);
        } else {
            let file = dir.join("state.json");
            let mut state: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
            if field == "uid" {
                state[&old]["metadata"]["uid"] = json!("replacement");
            } else {
                state[&old]["metadata"]["ownerReferences"][0]["uid"] = json!("replacement");
            }
            std::fs::write(file, serde_json::to_vec(&state).unwrap()).unwrap();
        }
        let (next, _) = warm_arm_plan(
            &kube,
            Some(&key),
            &tree,
            "image",
            &mounts,
            std::slice::from_ref(&config),
        );
        assert_ne!(old, next.pod, "{field} must force a cold arm");
        old.clone_from(&next.pod);
        seat::settle(&next, Ending::Exited(0)).unwrap();
        drop(next);
    }
    super::end_run(&key.parent);
    seat::reap_warm_parent(&key.parent);
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
    assert!(
        state.as_object().unwrap().is_empty(),
        "parent end reaps idle pods"
    );
    assert!(seat::checkout_warm(key).is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn warm_judge_successive_checkouts_reset_homes_and_retirement_removes_them() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let dir = tmp("warm-homes");
    let key = seat::WarmKey::new(&format!("parent-{}", uuid::Uuid::new_v4()), "pi#1");
    let first = seat::checkout_warm(key.clone()).unwrap().unwrap();
    first.fresh_dir(&dir).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("old-history"), "old").unwrap();
    assert!(seat::checkout_warm(key.clone()).unwrap().is_none());
    assert!(
        dir.join("old-history").exists(),
        "busy checkout never resets warm paths"
    );
    drop(first);
    let second = seat::checkout_warm(key.clone()).unwrap().unwrap();
    second.fresh_dir(&dir).unwrap();
    assert!(!dir.exists());
    std::fs::create_dir_all(&dir).unwrap();
    super::end_run(&key.parent);
    seat::reap_warm_parent(&key.parent);
    super::forget_ended(&key.parent);
    assert!(
        seat::checkout_warm(key.clone()).is_err(),
        "old lifecycle still owns stable paths"
    );
    drop(second);
    assert!(!dir.exists());
    let resumed = seat::checkout_warm(key.clone()).unwrap().unwrap();
    drop(resumed);
    seat::reap_warm_parent(&key.parent);
}

#[cfg(unix)]
#[test]
fn warm_judge_failed_settle_barrier_reaps_and_never_caches_the_pod() {
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let (dir, kube, key, _runtime) = warm_fixture();
    let (a, _) = warm_arm(&kube, Some(&key), &dir.join("tree"), "image");
    let pod = a.pod.clone();
    std::fs::write(dir.join("fail-reset"), "1").unwrap();
    assert_eq!(seat::settle(&a, Ending::Exited(0)).unwrap(), 0);
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
    assert!(state.get(&pod).is_none(), "failed barrier pod was deleted");
    drop(a);
    std::fs::remove_file(dir.join("fail-reset")).unwrap();
    let (b, _) = warm_arm(&kube, Some(&key), &dir.join("tree"), "image");
    assert_ne!(pod, b.pod);
    seat::reap(&b.id);
    drop(b);
    seat::reap_warm_parent(&key.parent);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn warm_reset_script_removes_history_tmp_unit_and_view_sentinels_without_crossing_mounts() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let dir = tmp("warm-reset-script");
    for d in [
        "home",
        "shm",
        "tmp/mounted/config",
        "tmp/mounted/tree",
        "unit",
        "canary",
        "source",
    ] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    for d in [
        "home",
        "shm",
        "tmp",
        "unit",
        "canary",
        "tmp/mounted/config",
        "tmp/mounted/tree",
    ] {
        std::fs::write(dir.join(d).join("sentinel"), "old").unwrap();
    }
    std::fs::write(dir.join("source/fresh"), "fresh").unwrap();
    let tree = dir.join("tmp/mounted/tree");
    let config = dir.join("tmp/mounted/config");
    let mounts = vec![tree.clone(), config.clone()];
    // Run only the reset, in temporary directories. The process-kill barrier is never run
    // in the host PID namespace. `host_tools` stands in for the floor's busybox dispatcher.
    let mut script = seat::warm_reset_script(&tree, &mounts, std::slice::from_ref(&config));
    // The IPC tables are the pod's own; the host test never touches the host's.
    assert!(script.contains(seat::WARM_IPC_CLEAR));
    script = script.replace(seat::WARM_IPC_CLEAR, "");
    for (from, to) in [
        ("wipe /home/seat", "home"),
        ("wipe /dev/shm", "shm"),
        ("wipe /tmp", "tmp"),
        ("wipe /wicked-unit", "unit"),
        ("wipe /wicked-canary", "canary"),
        ("cd /wicked-ro", "source"),
    ] {
        let cmd = from.split_whitespace().next().unwrap();
        script = script.replace(from, &format!("{cmd} '{}'", dir.join(to).display()));
    }
    script = script.replace(
        "F=/floor/busybox",
        "host_tools() { command \"$@\"; }; F=host_tools",
    );
    let out = std::process::Command::new("bash")
        .hardened()
        .args(["-c", &script])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for d in [
        "home",
        "shm",
        "tmp",
        "unit",
        "canary",
        "tmp/mounted/config",
        "tmp/mounted/tree",
    ] {
        assert!(!dir.join(d).join("sentinel").exists(), "{d}");
    }
    assert_eq!(std::fs::read(tree.join("fresh")).unwrap(), b"fresh");
    assert!(config.is_dir());
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn warm_quiescence_checks_detached_processes_and_refuses_a_survivor() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let dir = tmp("warm-proc-barrier");
    for pid in ["1", "22", "23"] {
        std::fs::create_dir_all(dir.join(pid)).unwrap();
        let ppid = if pid == "23" { "22" } else { "1" };
        std::fs::write(
            dir.join(pid).join("stat"),
            format!("{pid} (child) S {ppid} 0"),
        )
        .unwrap();
    }
    // A synthetic proc table and a shell function substitute the signal primitive. Never
    // execute kill -1 in the host PID namespace. PID 22 is detached; PID 23 is its child.
    let prefix = format!(
        r#"
mkdir -p '{p}'/$$; echo "$$ (helper) S 1 0" > '{p}'/$$/stat
trap 'rm -rf "{p}/$$"' EXIT
host_tools() {{ if [ "$1" = id ]; then echo 1000; else command "$@"; fi; }}
kill() {{ [ -f '{p}/survive' ] || rm -rf '{p}/22' '{p}/23'; }}
"#,
        p = dir.display()
    );
    let script = seat::WARM_QUIESCE
        .replace("/proc/", &format!("{}/", dir.display()))
        .replace("F=/floor/busybox", "F=host_tools");
    let run = || {
        std::process::Command::new("bash")
            .hardened()
            .args(["-c", &format!("{prefix}{script}")])
            .output()
            .unwrap()
    };
    std::fs::write(dir.join("survive"), "1").unwrap();
    assert!(
        !run().status.success(),
        "a surviving detached process must fail the barrier"
    );
    std::fs::remove_file(dir.join("survive")).unwrap();
    let out = run();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!dir.join("22").exists() && !dir.join("23").exists());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "wicked-warm-quiet"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn warm_seed_copy_bypasses_the_old_replica_stamp_and_restores_trusted_contents() {
    let _env = crate::test_env::ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let dir = tmp("warm-seeds");
    std::fs::write(dir.join("cache"), "trusted").unwrap();
    let root = PlanRoot {
        host: dir.clone(),
        mode: Mode::Seed,
        skip: vec![],
        back: false,
        files: vec![],
        masks: vec![],
        repo: false,
        ro: false,
        optional: false,
    };
    let rep = seat::replica_of(Some("warm-test"), &dir).unwrap();
    let seeded = [rep].into_iter().collect();
    let encode = || {
        seat::copy_in_stream_fresh(
            Some("warm-test"),
            &dir,
            std::slice::from_ref(&root),
            false,
            &seeded,
            true,
        )
        .unwrap()
        .0
    };
    for content in ["trusted", "updated"] {
        std::fs::write(dir.join("cache"), content).unwrap();
        let bytes = encode();
        let roots = sync::decode(&mut bytes.as_slice(), sync::MAX_STREAM).unwrap();
        assert_eq!(
            roots[0].mode,
            Mode::Home,
            "restore into the already-mounted directory"
        );
        assert_eq!(roots[0].entries[0].data, content.as_bytes());
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn warm_parent_end_during_execution_cannot_return_a_pod_to_the_cache() {
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let (dir, kube, key, _runtime) = warm_fixture();
    let (seat, _) = warm_arm(&kube, Some(&key), &dir.join("tree"), "image");
    super::end_run(&key.parent);
    seat::reap_warm_parent(&key.parent);
    seat::settle(&seat, Ending::Exited(0)).unwrap();
    drop(seat);
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
    assert!(state.as_object().unwrap().is_empty());
    assert!(seat::checkout_warm(key).is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn pr_c2_dependency_pack_prunes_before_sizing_or_validating() {
    let replica = tmp("deps-pack");
    let tree = replica.join("w");
    std::fs::create_dir_all(tree.join("pkg/Node_Modules")).unwrap();
    std::fs::create_dir_all(tree.join("node_modules")).unwrap();
    std::fs::write(tree.join("source.rs"), "legitimate edit").unwrap();
    std::fs::File::create(tree.join("node_modules/huge"))
        .unwrap()
        .set_len(sync::MAX_STREAM + 1)
        .unwrap();
    std::os::unix::fs::symlink("../../../../outside", tree.join("pkg/Node_Modules/link")).unwrap();
    let want = PlanRoot {
        host: PathBuf::from("/work/tree"),
        mode: Mode::Tree,
        skip: vec![],
        back: true,
        files: vec![],
        masks: vec![],
        repo: false,
        ro: false,
        optional: false,
    };
    let refs = [("w".into(), &want)];
    let plan = seat::pack_plan(&refs, Some(&want.host)).to_string();
    let out = codec(
        &replica,
        &["pack", &plan],
        br#"{"w":["node_modules/huge","source.rs"]}"#,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let roots = sync::decode(&mut out.stdout.as_slice(), sync::MAX_STREAM).unwrap();
    let entries = sync::check_entries(&roots[0], &[]).unwrap();
    assert_eq!(
        entries,
        vec![entry(Kind::File, "source.rs", b"legitimate edit")]
    );
    std::fs::remove_dir_all(replica).unwrap();
}

#[test]
fn pr_c2_dependency_mount_destination_is_a_directory_before_sandbox_creation() {
    let replica = tmp("deps-destination");
    let tree = replica.join("w");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("node_modules"), "ignored old file").unwrap();
    let out = codec(&replica, &["deps", "w"], b"");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(tree.join("node_modules").is_dir());
    std::fs::write(tree.join("node_modules/kept"), "warm").unwrap();
    assert!(codec(&replica, &["deps", "w"], b"").status.success());
    assert!(tree.join("node_modules/kept").is_file());
    std::fs::remove_dir_all(tree.join("node_modules")).unwrap();
    let outside = replica.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("kept"), "safe").unwrap();
    std::os::unix::fs::symlink(&outside, tree.join("node_modules")).unwrap();
    assert!(codec(&replica, &["deps", "w"], b"").status.success());
    assert!(!std::fs::symlink_metadata(tree.join("node_modules"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(outside.join("kept").is_file());
    std::fs::remove_dir_all(replica).unwrap();
}

/// core#1750: a seat argument holding a NUL is refused at finalize with a named reason (red on
/// main: std kept `<string-with-nul>` in its place, the NUL check never saw it, and the CLI ran
/// on those 17 bytes; here main failed only later, at the armed-seat lookup).
#[test]
fn finalize_refuses_a_seat_argument_holding_a_nul_1750() {
    // spawn-audit: test-only — never spawned: finalize only reads the launch command's args.
    let mut cmd = std::process::Command::new("kubectl");
    cmd.args([
        "exec",
        "-i",
        "-c",
        "seat",
        "p",
        "--",
        "sh",
        "-c",
        seat::WRAPPER,
    ])
    .arg(format!("{}nulcheck1750", seat::MARKER_PREFIX))
    .args(["claude", "-p", "quote a .pyc: \0\u{1}"]);
    let Err(err) = seat::finalize(&cmd) else {
        panic!("a NUL argument is refused");
    };
    assert_eq!(err, "a seat argument holds a NUL");
}

/// (core#1463) The roots of an "All repos" chat unit: a scratch tree, a seat home, and read-only
/// repositories, as `seat::plan` makes them from the mounts `container_floor` builds for one.
#[cfg(unix)]
fn optional_chat_roots(
    host: &Path,
    repos: &[(&str, usize)],
    optional: &[&str],
) -> (PathBuf, Vec<PlanRoot>) {
    let tree = host.join("chats/c1");
    let home = host.join("worker/homes/claude");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(tree.join("AGENTS.md"), "scope").unwrap();
    let mut mounts = vec![
        PlanMount::Bind {
            path: home.clone(),
            rw: true,
        },
        PlanMount::Bind {
            path: tree.clone(),
            rw: false,
        },
    ];
    for (name, size) in repos {
        let repo = host.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        git_in(&repo, &["init", "-q"]);
        std::fs::write(repo.join("lib.rs"), "x".repeat(*size)).unwrap();
        git_in(&repo, &["add", "lib.rs"]);
        mounts.push(PlanMount::Bind {
            path: repo,
            rw: false,
        });
    }
    let (mut roots, _) = seat::plan(&tree, true, &mounts, &[], &[], None);
    seat::mark_homes(&mut roots, std::slice::from_ref(&home), None);
    let optional: Vec<PathBuf> = optional.iter().map(|n| host.join(n)).collect();
    seat::mark_optional(&mut roots, &optional);
    (tree, roots)
}

/// Run `f` with the copy-in's byte cap at `cap` on this thread.
#[cfg(unix)]
fn with_cap<T>(cap: u64, f: impl FnOnce() -> T) -> T {
    seat::TEST_STREAM_LIMIT.with(|c| c.set(Some(cap)));
    let out = f();
    seat::TEST_STREAM_LIMIT.with(|c| c.set(None));
    out
}

/// (core#1463) An "All repos" chat whose optional repositories pass the copy-in's cap arms: the
/// required roots and the optional ones that fit are copied, the one that does not is left out with
/// a note in its place, the arm timing says so, and a copy an earlier turn kept is removed (its
/// stamp with it), so the seat never reads a stale repository.
#[cfg(unix)]
#[test]
fn k8s_core_1463_an_optional_root_over_the_cap_is_left_out_with_a_note() {
    // (No "left-out" in the fixture's own path: the arm detail names it.)
    let host = tmp("optional-omitted");
    let replica = tmp("optional-omitted-replica");
    let (tree, roots) = optional_chat_roots(
        &host,
        &[
            ("repos/alpha", 1000),
            ("repos/beta", 1000),
            ("repos/gamma", 900),
        ],
        &["repos/beta", "repos/gamma"],
    );
    let opt: Vec<&PlanRoot> = roots.iter().filter(|r| r.optional).collect();
    assert_eq!(opt.len(), 2, "{roots:?}");
    let rep = |n: &str| seat::replica_of(None, &host.join(n)).unwrap();

    // Turn 1, room for all: every repository is copied and kept.
    let (_, first) = with_cap(64 * 1024, || chat_turn(&replica, &tree, &roots));
    assert!(!first.contains("left-out"), "{first}");
    assert_eq!(
        std::fs::read_to_string(replica.join(rep("repos/gamma")).join("lib.rs")).unwrap(),
        "x".repeat(900)
    );
    // (codex r1 #3) Turn 1 left gamma a stamp in the replica.
    assert!(
        read_repo_stamps(&replica, &rep("repos/gamma")).contains_key(&rep("repos/gamma")),
        "turn 1 stamps gamma"
    );

    // Turn 2: gamma grew past what is left of the cap (two optional roots reserve 2 KiB).
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(host.join("repos/gamma/lib.rs"), "y".repeat(3000)).unwrap();
    let cap = 2 * seat::NOTE_RESERVE + 2500;
    let (body, second) = with_cap(cap, || chat_turn(&replica, &tree, &roots));
    assert!(second.contains("left-out 1;"), "{second}");
    // (codex r1 #2) Gamma's one file passed the cap and was never read: none of it counts as fit.
    assert!(
        second.contains(&format!("{}:left-out:0/", rep("repos/gamma"))),
        "{second}"
    );
    // (codex r1 #3) Leaving gamma out removed turn 1's stamp from the replica.
    assert!(
        read_repo_stamps(&replica, &rep("repos/gamma")).is_empty(),
        "a left-out root keeps no stamp"
    );
    assert!(
        !body.windows(3000).any(|w| w == "y".repeat(3000).as_bytes()),
        "the left-out repository was sent"
    );
    let gamma = replica.join(rep("repos/gamma"));
    let names: Vec<String> = std::fs::read_dir(&gamma)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != ".git")
        .collect();
    assert_eq!(names, vec![seat::LEFT_OUT_NOTE.to_string()], "{gamma:?}");
    let note = std::fs::read_to_string(gamma.join(seat::LEFT_OUT_NOTE)).unwrap();
    assert!(note.contains("left out") && note.contains("chat scoped to this repository"));
    assert!(
        note.contains("capped at 1 MiB") && note.contains("about 0 MiB of it fit"),
        "{note}"
    );
    assert!(
        note.len() < 600,
        "the note must fit its reserve: {}",
        note.len()
    );
    // The required root and the optional root that fit are there.
    assert_eq!(
        std::fs::read_to_string(replica.join(rep("repos/alpha")).join("lib.rs")).unwrap(),
        "x".repeat(1000)
    );
    assert!(replica.join(rep("repos/beta")).join("lib.rs").exists());

    // Turn 3, same host state: the left-out root carries no stamp, so it is decided again.
    let (_, third) = with_cap(cap, || chat_turn(&replica, &tree, &roots));
    assert!(
        third.contains(&format!("{}:left-out:", rep("repos/gamma"))),
        "{third}"
    );
    assert!(
        !third.contains(&format!("{}:kept", rep("repos/gamma"))),
        "{third}"
    );
}

/// (core#1463, codex r1 #3) A plain optional root that is not a repository is left out through
/// the read cache too: turn 1 reads it (and caches what it read), turn 2 at the same host state
/// serves it from the cache, finds it over a smaller cap, and leaves it out without reading it.
#[cfg(unix)]
#[test]
fn k8s_core_1463_an_optional_root_is_left_out_from_the_read_cache() {
    let host = tmp("optional-cached");
    let tree = host.join("chats/c1");
    let (docs, notes) = (host.join("docs"), host.join("notes"));
    for d in [&tree, &docs, &notes] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(tree.join("AGENTS.md"), "scope").unwrap();
    std::fs::write(docs.join("guide.md"), "d".repeat(100)).unwrap();
    std::fs::write(notes.join("big.md"), "n".repeat(3000)).unwrap();
    let mounts = vec![
        PlanMount::Bind {
            path: tree.clone(),
            rw: false,
        },
        PlanMount::Bind {
            path: docs.clone(),
            rw: false,
        },
        PlanMount::Bind {
            path: notes.clone(),
            rw: false,
        },
    ];
    let (mut roots, _) = seat::plan(&tree, true, &mounts, &[], &[], None);
    seat::mark_optional(&mut roots, std::slice::from_ref(&notes));
    let r = roots.iter().find(|r| r.host == notes).unwrap();
    assert!(r.optional && !r.repo, "{roots:?}");
    let turn = |cap: u64| {
        super::clock::begin();
        let out = with_cap(cap, || {
            seat::copy_in_stream(None, &tree, &roots, false, &Default::default())
        });
        let (phases, _) = super::clock::take().unwrap();
        out.expect("the unit arms");
        phases
            .into_iter()
            .find(|p| p.name == "copy_in_read")
            .and_then(|p| p.detail)
            .unwrap()
    };
    let rep = seat::replica_of(None, &notes).unwrap();
    let first = turn(64 * 1024);
    assert!(!first.contains("left-out"), "{first}");
    let reads = seat::HOST_READS.with(|n| n.get());
    let second = turn(seat::NOTE_RESERVE + 1000);
    assert_eq!(
        seat::HOST_READS.with(|n| n.get()),
        reads,
        "turn 2 is served from the read cache"
    );
    assert!(second.contains("left-out 1;"), "{second}");
    assert!(second.contains(&format!("{rep}:left-out:0/")), "{second}");
    let _ = std::fs::remove_dir_all(host);
}

/// (core#1463, codex r1 #2) A root left out by the entry cap says so, not the byte cap.
#[test]
fn k8s_core_1463_the_note_names_the_cap_that_was_passed() {
    let files = seat::left_out_note(5 << 20, seat::OverCap::Files(399_998));
    assert!(files.contains("capped at 399998 files"), "{files}");
    assert!(files.contains("about 5 MiB of it fit"), "{files}");
    let bytes = seat::left_out_note(0, seat::OverCap::Bytes(512 << 20));
    assert!(bytes.contains("capped at 512 MiB"), "{bytes}");
    for n in [&files, &bytes] {
        assert!(n.len() < 600, "the note must fit its reserve: {}", n.len());
    }
}

/// (core#1463) A required root never gives way: over the cap it refuses the unit as before, even
/// with optional roots in the plan.
#[cfg(unix)]
#[test]
fn k8s_core_1463_a_required_root_over_the_cap_still_refuses() {
    let host = tmp("optional-required-over");
    let (tree, roots) = optional_chat_roots(
        &host,
        &[("repos/alpha", 5000), ("repos/beta", 10)],
        &["repos/beta"],
    );
    let err = with_cap(seat::NOTE_RESERVE + 4000, || {
        seat::copy_in_stream(None, &tree, &roots, false, &Default::default())
    })
    .err()
    .expect("a required root over the cap refuses the unit");
    assert!(matches!(err.why, FloorUnarmed::CopyInRefused), "{err:?}");
    assert!(err.detail.contains("larger than"), "{}", err.detail);
}

/// (core#1463) Required roots are read before optional ones, whatever the plan's order: an optional
/// repository listed first never takes the budget a required one needs.
#[cfg(unix)]
#[test]
fn k8s_core_1463_required_roots_are_read_before_optional_ones() {
    let host = tmp("optional-order");
    // `a` (optional) sorts before the deeper required `b/c/d` in the plan.
    let (tree, roots) = optional_chat_roots(&host, &[("a", 3000), ("b/c/d", 3000)], &["a"]);
    let first_repo = roots.iter().position(|r| r.host == host.join("a")).unwrap();
    let required_repo = roots
        .iter()
        .position(|r| r.host == host.join("b/c/d"))
        .unwrap();
    assert!(first_repo < required_repo, "{roots:?}");
    super::clock::begin();
    let out = with_cap(seat::NOTE_RESERVE + 4000, || {
        seat::copy_in_stream(None, &tree, &roots, false, &Default::default())
    });
    let (phases, _) = super::clock::take().unwrap();
    out.expect("the required root fits once the optional one gives way");
    let detail = phases
        .into_iter()
        .find(|p| p.name == "copy_in_read")
        .and_then(|p| p.detail)
        .unwrap();
    let rep = |n: &str| seat::replica_of(None, &host.join(n)).unwrap();
    assert!(
        detail.contains(&format!("{}:left-out:", rep("a"))),
        "{detail}"
    );
    assert!(
        detail.contains(&format!("{}:1/3000/", rep("b/c/d"))),
        "{detail}"
    );
}

/// (core#1463) Only a plain, read-only root that does not come back can be optional.
#[test]
fn k8s_core_1463_only_a_read_only_plain_root_is_marked_optional() {
    let tree = PathBuf::from("/h/tree");
    let ro = PathBuf::from("/h/ro");
    let rw = PathBuf::from("/h/rw");
    let mounts = vec![
        PlanMount::Bind {
            path: tree.clone(),
            rw: true,
        },
        PlanMount::Bind {
            path: ro.clone(),
            rw: false,
        },
        PlanMount::Bind {
            path: rw.clone(),
            rw: true,
        },
    ];
    let (mut roots, _) = seat::plan(&tree, false, &mounts, &[], std::slice::from_ref(&rw), None);
    seat::mark_optional(&mut roots, &[tree.clone(), ro.clone(), rw.clone()]);
    let optional: Vec<&PathBuf> = roots
        .iter()
        .filter(|r| r.optional)
        .map(|r| &r.host)
        .collect();
    assert_eq!(optional, vec![&ro]);
}
