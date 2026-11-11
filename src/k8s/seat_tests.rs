//! B-2 tests: the replica sync codec, the checked write-back, the replica plan, the unit env,
//! exit-code honesty, the bound-pod gate, the read-only view, and the gateway writer scripts.
//! Each names its PARITY row.

use std::path::{Path, PathBuf};

use serde_json::json;

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
    use std::io::Write as _;
    let mut c = std::process::Command::new("python3");
    c.args(["-I", "-c", sync::POD_CODEC])
        .args(args)
        .env("WICKED_SYNC_ROOT", replica)
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
    assert!(by(&home).back);
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
    c.args(["-c", "yes x | head -c 1000000"]);
    let r = super::run_capped(c, None, std::time::Duration::from_secs(20), 1000);
    assert!(!r.ok(), "a capped call reported success");
    assert!(r.stdout.is_empty());
    assert!(String::from_utf8_lossy(&r.stderr).contains("cap"));
    let mut c = std::process::Command::new("sh");
    c.args(["-c", "printf abc"]);
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
