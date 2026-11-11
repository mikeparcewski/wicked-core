//! B-2 tests: the replica sync codec, the checked write-back, the replica plan, the unit env,
//! exit-code honesty, the bound-pod gate, the read-only view, and the gateway writer scripts.
//! Each names its PARITY row.

use std::path::{Path, PathBuf};

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
    use std::io::Write as _;
    let mut c = std::process::Command::new("python3");
    c.hardened()
        .args(["-I", "-c", sync::POD_CODEC])
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
    let packed = codec(&replica, &["pack", &plan], b"");
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
    let packed = codec(&replica, &["pack", &plan], b"");
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
    let wait = &d["spec"]["template"]["spec"]["initContainers"][1];
    assert_eq!(wait["name"], "wait-config");
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
    let packed = codec(&replica, &["pack", &plan], b"");
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
    let (body, plan, base, _) =
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
    };
    let e = seat::copy_in_stream(None, &host, &[r], false, &Default::default()).unwrap_err();
    assert!(e.contains("larger than"), "{e}");
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

#[test]
fn k8s_a_failed_settlement_keeps_the_clis_output_as_could_not_run() {
    let err = format!(
        "{}the write-back was refused: `x` is inside a git dir",
        crate::execute_wrapped::SETTLE_FAILED
    );
    let out = crate::execute_wrapped::settle_failure_output("claude", &err, "minutes of work");
    assert!(
        out.starts_with(crate::execute_wrapped::COULD_NOT_RUN_PREFIX),
        "{out}"
    );
    assert!(out.contains("inside a git dir"));
    assert!(out.ends_with("minutes of work"));
    // Review r4: the CLI's stderr is kept too.
    let err = format!(
        "{}the rc could not be read{}rate limited: retry later",
        crate::execute_wrapped::SETTLE_FAILED,
        crate::execute_wrapped::SETTLE_SEP
    );
    let out = crate::execute_wrapped::settle_failure_output("claude", &err, "");
    assert!(
        out.starts_with(crate::execute_wrapped::COULD_NOT_RUN_PREFIX),
        "{out}"
    );
    assert!(out.contains("the rc could not be read)"), "{out}");
    assert!(out.ends_with("rate limited: retry later"), "{out}");
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

#[cfg(unix)]
#[test]
fn k8s_the_pod_codec_refuses_an_oversized_file_before_reading_it() {
    let replica = tmp("bigpack");
    std::fs::create_dir_all(replica.join("w")).unwrap();
    let f = std::fs::File::create(replica.join("w/big")).unwrap();
    f.set_len(sync::MAX_STREAM + 1).unwrap();
    let plan = json!([["w", "plain", [], []]]).to_string();
    let out = codec(&replica, &["pack", &plan], b"");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("limit"));
    assert!(sync::POD_CODEC.contains(&format!("MAX = {}", sync::MAX_STREAM)));
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
    let packed = codec(&replica, &["pack", &plan], b"");
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

/// r6 #5: a wrapper setup failure writes its rc, so it is the unit's failure (exit 125), never a
/// lost stream.
#[test]
fn k8s_a_wrapper_setup_failure_is_an_exit_code_not_a_lost_stream() {
    assert_eq!(
        seat::WRAPPER.matches("echo 125 > $u/rc; exit 125").count(),
        3
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
    };
    let (stream, _, _, baseline) =
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
        b"",
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
