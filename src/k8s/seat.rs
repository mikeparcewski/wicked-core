//! A governed seat on a pod (PARITY B-2): the replica copy-in, the seat's Sandbox and its
//! bound-pod gate, the unit's env over exec stdin, the CLI over `kubectl exec -i`, the exit code
//! read back from the pod, the reap and the checked write-back.
//!
//! **The runner contract holds.** The seat is still a child process of the engine (the kubectl
//! client): stdio streams, the deadline and the cancel kill are the existing spawn path's. A
//! pod outlives a killed client, so [`settle`] (and every guard's drop) deletes the Sandbox and
//! waits until its pod is gone before anything of the unit's is read back: by then no seat
//! process can be alive (A2, A12).
//!
//! **What the pod sees** is the unit's mount plan, as replica copies at the same absolute paths
//! (B6, B11): its tree (what git lists, plus a `.git` the engine builds fresh for every unit), its
//! write and read roots, its seat home, the pinned launch files. The clone root, the sibling
//! worktrees and the worktree's git dir are absent, and a masked path is never copied, which is
//! the mask. A read-only unit's tree is a throwaway view (B17): copied into an emptyDir in the
//! pod, written to freely, never read back.
//!
//! **Secrets and size never ride an exec argv** (B8): the unit's env (unit keys, the claude
//! token, every carrier variable) is a shell-quoted file written into the pod's in-memory
//! `/wicked-unit` over stdin, sourced and removed by the fixed [`WRAPPER`] before the launcher
//! runs; the CLI's exit code comes back through `/wicked-unit/rc`, so a lost stream is
//! `exec_stream_lost`, never an exit code (A3).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::render::{self, ReplicaMount, RunBudget, SeatPod};
use super::sync::{self, Entry, Mode, Root};
use super::{cluster, label_value, Kube, ROLE_LABEL, UNIT_LABEL};
use crate::worker_sandbox::FloorUnarmed;

/// The fixed in-pod wrapper (`sh -c WRAPPER <marker> <launcher argv> -- <cli argv>`): the unit's
/// env sourced and removed, its working directory entered, the launcher run, its exit code
/// written where [`settle`] reads it.
pub(crate) const WRAPPER: &str = "set -u; u=/wicked-unit; \
[ -f $u/env ] || { echo 'wicked-seat: the unit env is missing' >&2; exit 125; }; \
. $u/env; /floor/busybox rm -f $u/env; \
cd \"$WICKED_UNIT_CWD\" || { echo 'wicked-seat: the unit directory is missing' >&2; exit 125; }; \
unset WICKED_UNIT_CWD; \"$@\"; rc=$?; \
echo $rc > $u/rc.tmp && /floor/busybox mv $u/rc.tmp $u/rc; exit $rc";

/// The marker prefix of a seat's exec (`$0` of [`WRAPPER`]): its id in [`SEATS`].
pub(crate) const MARKER_PREFIX: &str = "wicked-unit-";

/// How long a seat pod may take to run once its Sandbox is applied before the unit waits as
/// `admission_pending`.
const POD_START: Duration = Duration::from_secs(150);
const CALL: Duration = Duration::from_secs(60);
/// The largest copy-in or write-back the engine moves in one stream.
const STREAM_LIMIT: u64 = sync::MAX_STREAM;
/// The replica's shallow history.
const REPLICA_DEPTH: &str = "50";

/// One root of a unit's replica: where it is on the host, how it syncs, what of it the seat may
/// never write (masks and nested roots, relative), and whether it comes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanRoot {
    pub(crate) host: PathBuf,
    pub(crate) mode: Mode,
    pub(crate) skip: Vec<String>,
    /// Written back to the host after the unit.
    pub(crate) back: bool,
    /// `Files` roots: the file names of `host` (a directory) the unit binds one by one.
    pub(crate) files: Vec<String>,
}

/// A mount of the floor's plan, as the pod renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlanMount {
    Bind { path: PathBuf, rw: bool },
    Mask { path: PathBuf },
}

/// The replica plan of one unit (pure): which roots are copied in, in which mode, which come
/// back, and the pod's mounts. `tree` = the unit's worktree; `view` = a read-only unit (its tree is
/// a throwaway view); `protected` = the repository boundary (absent from the pod, with every
/// git-state path under it); `write_roots` = the unit's admitted write roots (written back);
/// `seed_base` = the seat caches' base (seeded once, never written back).
pub(crate) fn plan(
    tree: &Path,
    view: bool,
    mounts: &[PlanMount],
    protected: &[PathBuf],
    write_roots: &[PathBuf],
    seed_base: Option<&Path>,
) -> (Vec<PlanRoot>, Vec<ReplicaMount>) {
    let masks: Vec<PathBuf> = mounts
        .iter()
        .filter_map(|m| match m {
            PlanMount::Mask { path } => Some(path.clone()),
            _ => None,
        })
        .collect();
    let mut binds: Vec<(PathBuf, bool)> = Vec::new();
    // The tree is always a root of its own (a read-only plan may expose it only through the
    // repository boundary, which the pod does not mount).
    if !mounts
        .iter()
        .any(|m| matches!(m, PlanMount::Bind { path, .. } if path == tree))
    {
        binds.push((tree.to_path_buf(), false));
    }
    for m in mounts {
        let PlanMount::Bind { path, rw } = m else {
            continue;
        };
        // The repository boundary and the git state under it are absent (B6): the replica's
        // own `.git` stands in. The tree and an admitted write root under it are kept.
        let under_protected = protected.iter().any(|p| path.starts_with(p));
        let is_tree = path == tree || path.starts_with(tree);
        let admitted = write_roots.iter().any(|w| path.starts_with(w));
        if under_protected && !is_tree && !admitted {
            continue;
        }
        // The tree's own git pointer: replaced by the replica's `.git`.
        if path.parent() == Some(tree) && path.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        binds.retain(|(p, _)| p != path);
        binds.push((path.clone(), *rw));
    }
    let dirs: Vec<PathBuf> = binds
        .iter()
        .filter(|(p, _)| !p.is_file())
        .map(|(p, _)| p.clone())
        .collect();
    let mut roots: Vec<PlanRoot> = Vec::new();
    let mut pod: Vec<ReplicaMount> = Vec::new();
    let mut files: HashMap<PathBuf, (Vec<String>, bool)> = HashMap::new();
    for (path, rw) in &binds {
        pod.push(ReplicaMount {
            path: path.clone(),
            rw: *rw && !(view && path == tree),
        });
        if path.is_file() {
            if let (Some(parent), Some(name)) =
                (path.parent(), path.file_name().and_then(|n| n.to_str()))
            {
                let e = files.entry(parent.to_path_buf()).or_default();
                e.0.push(name.to_string());
                e.1 |= *rw;
            }
            continue;
        }
        let mut skip = sync::nested(path, &dirs);
        skip.extend(sync::nested(path, &masks));
        let seed = seed_base.is_some_and(|b| path.starts_with(b));
        let mode = if path == tree {
            Mode::Tree
        } else if seed {
            Mode::Seed
        } else {
            Mode::Plain
        };
        let back = *rw && !seed && !(view && path == tree);
        if path == tree {
            skip.push(".git".into());
        }
        roots.push(PlanRoot {
            host: path.clone(),
            mode,
            skip,
            back,
            files: Vec::new(),
        });
    }
    let mut file_roots: Vec<(PathBuf, (Vec<String>, bool))> = files.into_iter().collect();
    file_roots.sort();
    for (dir, (mut names, rw)) in file_roots {
        names.sort();
        names.dedup();
        roots.push(PlanRoot {
            host: dir,
            mode: Mode::Files,
            skip: Vec::new(),
            back: rw,
            files: names,
        });
    }
    // Parents before their nested roots (a nested root lands inside its parent's copy).
    roots.sort_by_key(|r| r.host.components().count());
    (roots, pod)
}

/// The pod codec's plan JSON for `roots` (`[[replica, mode, list], ...]`): `list` is the skip
/// list, or for a root of single files their names (mode `files`).
pub(crate) fn codec_plan(roots: &[(String, &PlanRoot)]) -> Value {
    Value::Array(
        roots
            .iter()
            .map(|(replica, r)| {
                let list = if r.mode == Mode::Files {
                    &r.files
                } else {
                    &r.skip
                };
                serde_json::json!([replica, r.mode.as_str(), list])
            })
            .collect(),
    )
}

/// The replica path of a host path under an optional scope.
pub(crate) fn replica_of(scope: Option<&str>, p: &Path) -> Result<String, String> {
    render::scoped_sub_path(scope, p)
        .ok_or_else(|| format!("{} cannot be a replica path", p.display()))
}

/// `KEY='value'` lines the wrapper sources (pure). A key that is not a shell name is refused.
pub(crate) fn env_file(vars: &[(String, String)]) -> Result<String, String> {
    let mut s = String::new();
    for (k, v) in vars {
        let name_ok = !k.is_empty()
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !k.starts_with(|c: char| c.is_ascii_digit());
        if !name_ok {
            return Err(format!("`{k}` is not an environment variable name"));
        }
        if v.contains('\0') {
            return Err(format!("`{k}` holds a NUL"));
        }
        s.push_str(&format!("export {k}='{}'\n", v.replace('\'', "'\\''")));
    }
    Ok(s)
}

/// The exit code of a seat exec: the pod's rc file is the evidence, kubectl's own code only
/// when it agrees (pure). `None` = the stream was lost (no rc, or a disagreement).
pub(crate) fn exit_code(kubectl: i32, rc_file: Option<&str>) -> Option<i32> {
    let rc: i32 = rc_file?.trim().parse().ok()?;
    (rc == kubectl).then_some(rc)
}

/// What one live seat is (in [`SEATS`] from arming until it is settled or dropped).
#[derive(Debug)]
pub(crate) struct Seat {
    pub(crate) id: String,
    pub(crate) kube: Kube,
    pub(crate) ns: String,
    pub(crate) sandbox: String,
    pub(crate) pod: String,
    pub(crate) scope: Option<String>,
    pub(crate) cli: String,
    pub(crate) tree: PathBuf,
    pub(crate) roots: Vec<PlanRoot>,
    /// The host HEAD the replica's `.git` was built at (`None`: no commits, or no git).
    pub(crate) base: Option<String>,
    /// The unit's seat homes, re-synced right before the CLI starts (placeholder files).
    pub(crate) seat_dirs: Vec<PathBuf>,
    /// The canonical spelling of every path the pod mounts (env values are re-spelled to it).
    pub(crate) mounted: Vec<PathBuf>,
    pub(crate) unit_label: String,
    pub(crate) settled: Mutex<bool>,
}

/// Live seats by id.
static SEATS: Mutex<Option<HashMap<String, Arc<Seat>>>> = Mutex::new(None);

fn register(seat: Seat) -> Arc<Seat> {
    let seat = Arc::new(seat);
    if let Ok(mut m) = SEATS.lock() {
        m.get_or_insert_with(HashMap::new)
            .insert(seat.id.clone(), Arc::clone(&seat));
    }
    seat
}

pub(crate) fn lookup(id: &str) -> Option<Arc<Seat>> {
    SEATS.lock().ok()?.as_ref()?.get(id).cloned()
}

fn unregister(id: &str) {
    if let Ok(mut m) = SEATS.lock() {
        if let Some(m) = m.as_mut() {
            m.remove(id);
        }
    }
}

/// The seat id a wrapper argv carries (its [`WRAPPER`] marker), or `None`.
pub(crate) fn seat_id_of(args: &[String]) -> Option<String> {
    let i = args
        .windows(3)
        .position(|w| w[0] == "sh" && w[1] == "-c" && w[2] == WRAPPER)?;
    args.get(i + 3)?
        .strip_prefix(MARKER_PREFIX)
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(str::to_string)
}

/// The engine's one-shot session prefixes (a judge, a ballot, a triage call): they share the
/// daemon's session namespace, each in its own replica scope, deleted when the session ends.
pub(crate) fn one_shot_scope(run_id: &str) -> Option<String> {
    ["council-", "validator-", "triage-", "member-", "send-back-"]
        .iter()
        .any(|p| run_id.starts_with(p))
        .then(|| format!("s-{}", label_value(run_id)))
}

/// The namespace a run's pods live in: its own, or the daemon's shared session namespace for a
/// one-shot session.
pub(crate) fn place(kube: &Kube, run_id: &str) -> (String, Option<String>, String) {
    match one_shot_scope(run_id) {
        Some(scope) => (
            super::run_namespace_name(&kube.owner, "sessions"),
            Some(scope),
            "sessions".to_string(),
        ),
        None => (
            super::run_namespace_name(&kube.owner, run_id),
            None,
            run_id.to_string(),
        ),
    }
}

/// The sync pod of `ns`, created when absent and waited for.
fn ensure_sync(
    kube: &Kube,
    ns: &str,
    run: &str,
    image: &str,
    floor: &str,
) -> Result<(), (FloorUnarmed, String)> {
    let running = |kube: &Kube| -> Result<bool, String> {
        let pods = kube.list_owned(Some(ns), "pods", &[(ROLE_LABEL, "sync")])?;
        Ok(cluster::running_pod(&pods).as_deref() == Some("sync"))
    };
    if running(kube).map_err(|e| (FloorUnarmed::ClusterUnreachable, e))? {
        return Ok(());
    }
    let existing = kube
        .get_json(Some(ns), "pod", "sync")
        .map_err(|e| (FloorUnarmed::ClusterUnreachable, e))?;
    if let Some(p) = existing {
        // A sync pod that ended (or a predecessor's): replaced.
        if p.pointer("/status/phase").and_then(Value::as_str) != Some("Pending") {
            kube.delete_owned(Some(ns), "pod", "sync", true)
                .map_err(|e| (FloorUnarmed::ClusterUnreachable, e))?;
        }
    }
    let doc = render::sync_pod(ns, &kube.owner, &label_value(run), image, floor);
    if kube
        .get_json(Some(ns), "pod", "sync")
        .ok()
        .flatten()
        .is_none()
    {
        kube.apply(&[doc])
            .map_err(|e| (FloorUnarmed::PolicyRefused, e))?;
    }
    let start = Instant::now();
    while start.elapsed() < POD_START {
        if running(kube).unwrap_or(false) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err((
        FloorUnarmed::AdmissionPending,
        format!("the run's sync pod in {ns} did not start"),
    ))
}

/// Run the pod codec in the sync pod.
fn codec(
    kube: &Kube,
    ns: &str,
    args: &[&str],
    stdin: Option<&[u8]>,
    limit: Duration,
) -> super::Called {
    let mut a = vec!["exec"];
    if stdin.is_some() {
        a.push("-i");
    }
    a.extend([
        "-c",
        "sync",
        "sync",
        "--",
        "python3",
        "-I",
        "-c",
        sync::POD_CODEC,
    ]);
    a.extend(args);
    kube.call(Some(ns), &a, stdin, limit)
}

/// The host's side of a tree root's `.git` for the replica: a shallow clone of the tree's HEAD,
/// its branch and index set, read as a `git` root. `None` = the tree has no commit.
fn replica_git(tree: &Path) -> Result<Option<(String, Vec<Entry>)>, String> {
    let head = sync::git(tree)
        .args(["rev-parse", "--verify", "-q", "HEAD^{commit}"])
        .output()
        .map_err(|e| e.to_string())?;
    if !head.status.success() {
        return Ok(None);
    }
    let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
    let branch = sync::git(tree)
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .map_err(|e| e.to_string())?;
    let branch = branch
        .status
        .success()
        .then(|| String::from_utf8_lossy(&branch.stdout).trim().to_string())
        .filter(|b| b.starts_with("refs/heads/"));
    let tmp = std::env::temp_dir().join(format!(
        "wicked-replica-git-{}",
        uuid::Uuid::new_v4().simple()
    ));
    struct Rm(PathBuf);
    impl Drop for Rm {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _rm = Rm(tmp.clone());
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let run = |args: &[&str]| -> Result<(), String> {
        let o = sync::git(&tmp)
            .args(args)
            .output()
            .map_err(|e| e.to_string())?;
        if o.status.success() {
            Ok(())
        } else {
            Err(format!(
                "git {}: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&o.stderr).trim()
            ))
        }
    };
    run(&["init", "-q"])?;
    let src = format!("file://{}", tree.display());
    run(&[
        "-c",
        "protocol.file.allow=always",
        "fetch",
        "-q",
        "--no-tags",
        &format!("--depth={REPLICA_DEPTH}"),
        &src,
        &sha,
    ])
    .or_else(|_| {
        run(&[
            "-c",
            "protocol.file.allow=always",
            "fetch",
            "-q",
            "--no-tags",
            &format!("--depth={REPLICA_DEPTH}"),
            &src,
            "HEAD",
        ])
    })?;
    match &branch {
        Some(b) => {
            run(&["update-ref", b, &sha])?;
            run(&["symbolic-ref", "HEAD", b])?;
        }
        None => run(&["update-ref", "--no-deref", "HEAD", &sha])?,
    }
    run(&["read-tree", &sha])?;
    let _ = std::fs::remove_file(tmp.join(".git/FETCH_HEAD"));
    let gitdir = tmp.join(".git");
    let entries = sync::host_plain_listing(&gitdir, &[])
        .iter()
        .filter_map(|rel| sync::read_host_entry(&gitdir, rel))
        .collect();
    Ok(Some((sha, entries)))
}

/// The copy-in stream of `roots` (the host's side), and the tree's base commit.
fn copy_in_stream(
    scope: Option<&str>,
    tree: &Path,
    roots: &[PlanRoot],
    with_git: bool,
) -> Result<(Vec<u8>, Value, Option<String>), String> {
    let mut stream: Vec<Root> = Vec::new();
    let mut plan_roots: Vec<(String, PlanRoot)> = Vec::new();
    let mut base = None;
    for r in roots {
        let replica = replica_of(scope, &r.host)?;
        let rels: Vec<String> = if r.mode == Mode::Files {
            r.files.clone()
        } else {
            match r.mode {
                Mode::Tree => sync::host_tree_listing(&r.host)?,
                _ => sync::host_plain_listing(&r.host, &r.skip),
            }
        };
        let entries: Vec<Entry> = rels
            .iter()
            .filter(|p| !r.skip.iter().any(|s| sync::under(p, s)))
            .filter(|p| r.mode != Mode::Tree || !p.split('/').any(sync::is_git_name))
            .filter_map(|p| sync::read_host_entry(&r.host, p))
            .collect();
        // The tree's `.git` first: the codec mirrors the tree by what that git lists.
        if r.mode == Mode::Tree && r.host == tree && with_git {
            if let Some((sha, git_entries)) = replica_git(tree)? {
                let gr = format!("{replica}/.git");
                stream.push(Root {
                    replica: gr.clone(),
                    mode: Mode::Git,
                    entries: git_entries,
                });
                plan_roots.push((
                    gr,
                    PlanRoot {
                        host: tree.join(".git"),
                        mode: Mode::Git,
                        skip: Vec::new(),
                        back: false,
                        files: Vec::new(),
                    },
                ));
                base = Some(sha);
            }
        }
        stream.push(Root {
            replica: replica.clone(),
            mode: r.mode,
            entries,
        });
        plan_roots.push((replica.clone(), r.clone()));
    }
    // The codec writes `files` roots by name; the stream carries them as plain entries.
    let refs: Vec<(String, &PlanRoot)> = plan_roots.iter().map(|(a, b)| (a.clone(), b)).collect();
    let plan = codec_plan(&refs);
    Ok((sync::encode(&stream), plan, base))
}

/// Copy `roots` into the replica through the sync pod.
fn copy_in(
    kube: &Kube,
    ns: &str,
    scope: Option<&str>,
    tree: &Path,
    roots: &[PlanRoot],
    with_git: bool,
) -> Result<Option<String>, (FloorUnarmed, String)> {
    let (body, plan, base) = copy_in_stream(scope, tree, roots, with_git)
        .map_err(|e| (FloorUnarmed::RootUnresolvable, e))?;
    if body.len() as u64 > STREAM_LIMIT {
        return Err((
            FloorUnarmed::RootUnresolvable,
            format!("the unit's copy-in is larger than {STREAM_LIMIT} bytes"),
        ));
    }
    let plan = plan.to_string();
    let r = codec(
        kube,
        ns,
        &["unpack", &plan],
        Some(&body),
        Duration::from_secs(300),
    );
    if !r.ok() {
        return Err((
            FloorUnarmed::ClusterUnreachable,
            format!(
                "the copy-in into the run's replica failed: {}",
                r.err_text()
            ),
        ));
    }
    Ok(base)
}

/// The facts one seat pod is armed with.
#[derive(Debug, Clone)]
pub(crate) struct ArmSpec<'a> {
    pub(crate) run_id: &'a str,
    pub(crate) ord: u32,
    pub(crate) cli: &'a str,
    /// The seat image (in the builder's store, which the cluster shares: D2).
    pub(crate) image: &'a str,
    pub(crate) floor_image: &'a str,
    pub(crate) sync_image: &'a str,
    pub(crate) workdir: &'a Path,
    pub(crate) tree: &'a Path,
    pub(crate) view: bool,
    pub(crate) mounts: &'a [PlanMount],
    pub(crate) protected: &'a [PathBuf],
    pub(crate) write_roots: &'a [PathBuf],
    pub(crate) seat_dirs: &'a [PathBuf],
    pub(crate) seed_base: Option<&'a Path>,
    pub(crate) memory: &'a str,
    pub(crate) gateway_ip: Option<&'a str>,
    pub(crate) deadline: Duration,
    /// `seat`, or `judge` for an engine-internal session.
    pub(crate) role: &'a str,
}

/// The pod's state as the bound-pod gate reads it (pure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PodState {
    Ready,
    Waiting,
    Failed(FloorUnarmed, String),
}

/// Read a pod: Ready only when it is the one `sandbox` owns, carries `unit`, runs with every
/// container ready and is not being deleted (B15).
pub(crate) fn pod_state(pod: &Value, sandbox: &str, unit: &str) -> PodState {
    let owned = pod
        .pointer("/metadata/ownerReferences")
        .and_then(Value::as_array)
        .is_some_and(|o| {
            o.iter().any(|r| {
                r.get("kind").and_then(Value::as_str) == Some("Sandbox")
                    && r.get("name").and_then(Value::as_str) == Some(sandbox)
            })
        });
    let labelled = pod
        .pointer(&format!(
            "/metadata/labels/{}",
            UNIT_LABEL.replace('/', "~1")
        ))
        .and_then(Value::as_str)
        == Some(unit);
    if !owned || !labelled {
        return PodState::Failed(
            FloorUnarmed::PolicyRefused,
            "the pod is not the one this unit's Sandbox owns".into(),
        );
    }
    let statuses = pod
        .pointer("/status/initContainerStatuses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            pod.pointer("/status/containerStatuses")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        );
    for c in statuses {
        let reason = c
            .pointer("/state/waiting/reason")
            .and_then(Value::as_str)
            .unwrap_or("");
        if matches!(
            reason,
            "ErrImageNeverPull" | "ErrImagePull" | "ImagePullBackOff"
        ) {
            return PodState::Failed(
                FloorUnarmed::ImageUnavailable,
                format!(
                    "{}: {reason}",
                    c.get("image").and_then(Value::as_str).unwrap_or("image")
                ),
            );
        }
        if matches!(
            reason,
            "CreateContainerConfigError" | "CreateContainerError" | "RunContainerError"
        ) {
            let msg = c
                .pointer("/state/waiting/message")
                .and_then(Value::as_str)
                .unwrap_or(reason);
            return PodState::Failed(FloorUnarmed::PolicyRefused, msg.to_string());
        }
        if let Some(code) = c
            .pointer("/state/terminated/exitCode")
            .and_then(Value::as_i64)
        {
            if code != 0 {
                return PodState::Failed(
                    FloorUnarmed::PolicyRefused,
                    format!(
                        "container {} exited {code} before the seat ran",
                        c.get("name").and_then(Value::as_str).unwrap_or("?")
                    ),
                );
            }
        }
    }
    match pod.pointer("/status/phase").and_then(Value::as_str) {
        Some("Failed") | Some("Succeeded") => {
            return PodState::Failed(
                FloorUnarmed::PolicyRefused,
                "the seat pod ended before its unit ran".into(),
            )
        }
        _ => {}
    }
    if cluster::running_pod(std::slice::from_ref(pod)).is_some() {
        PodState::Ready
    } else {
        PodState::Waiting
    }
}

/// A Sandbox the controller could not make a pod for: why, from its status (pure).
pub(crate) fn sandbox_refusal(sbx: &Value) -> Option<(FloorUnarmed, String)> {
    let conds = sbx
        .pointer("/status/conditions")
        .and_then(Value::as_array)?;
    conds.iter().find_map(|c| {
        let msg = c.get("message").and_then(Value::as_str).unwrap_or("");
        if msg.contains("exceeded quota") || msg.contains("Insufficient memory") {
            Some((FloorUnarmed::AdmissionPending, msg.to_string()))
        } else if msg.contains("violates PodSecurity") || msg.contains("forbidden") {
            Some((FloorUnarmed::PolicyRefused, msg.to_string()))
        } else {
            None
        }
    })
}

/// The deny check before a seat's first exec (B5, F1): from the seat, the API server and the
/// internet are refused, and (online) the gateway's CONNECT port answers. Retried briefly: a
/// fresh pod's policy is programmed asynchronously.
fn deny_check(
    kube: &Kube,
    ns: &str,
    pod: &str,
    online: bool,
) -> Result<(), (FloorUnarmed, String)> {
    let script = format!(
        "nc_() {{ if /floor/busybox nc -w 2 \"$1\" \"$2\" </dev/null >/dev/null 2>&1; then echo \"$3=open\"; else echo \"$3=closed\"; fi; }}; \
         nc_ \"$KUBERNETES_SERVICE_HOST\" \"$KUBERNETES_SERVICE_PORT\" api & nc_ 1.1.1.1 443 net & {} wait",
        if online { "nc_ agw 3000 gw & " } else { "" }
    );
    let start = Instant::now();
    let mut last = String::new();
    while start.elapsed() < Duration::from_secs(20) {
        let r = kube.call(
            Some(ns),
            &[
                "exec",
                "-c",
                "seat",
                pod,
                "--",
                "/floor/busybox",
                "sh",
                "-c",
                &script,
            ],
            None,
            Duration::from_secs(15),
        );
        let out = String::from_utf8_lossy(&r.stdout).to_string();
        if r.ok()
            && out.contains("api=closed")
            && out.contains("net=closed")
            && (!online || out.contains("gw=open"))
        {
            return Ok(());
        }
        last = if r.ok() {
            out.replace('\n', " ")
        } else {
            r.err_text()
        };
        std::thread::sleep(Duration::from_millis(500));
    }
    Err((
        FloorUnarmed::PolicyRefused,
        format!(
            "the seat's network policy did not hold before its first exec ({})",
            last.trim()
        ),
    ))
}

/// Arm one seat pod: the run's namespace and sync pod, the copy-in, the Sandbox, the bound-pod
/// gate and the deny check. Answers the seat (registered) and the wrapper argv the CLI argv
/// follows (`landrun` = the launcher's Landlock options, ending before the CLI).
pub(crate) fn arm(
    kube: &Kube,
    s: &ArmSpec<'_>,
    landrun: Vec<String>,
) -> Result<(Arc<Seat>, Vec<String>), (FloorUnarmed, String)> {
    let (ns, scope, run) = place(kube, s.run_id);
    if scope.is_some() {
        if let Ok(mut m) = SCOPES.lock() {
            m.get_or_insert_with(HashMap::new)
                .insert(s.run_id.to_string(), kube.clone());
        }
    }
    cluster::ensure_run(kube, &run, &RunBudget::default(), "20Gi")
        .map_err(|e| (classify_apply(&e), e))?;
    ensure_sync(kube, &ns, &run, s.sync_image, s.floor_image)?;
    let (roots, pod_mounts) = plan(
        s.tree,
        s.view,
        s.mounts,
        s.protected,
        s.write_roots,
        s.seed_base,
    );
    let base = copy_in(kube, &ns, scope.as_deref(), s.tree, &roots, true)?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let sandbox = format!("{}-{}-{}", s.role, s.ord, &id[..10]);
    let unit_label = label_value(&format!("{}-{}", s.ord, &id[..10]));
    let secs = s.deadline.as_secs() + 600;
    let spec = SeatPod {
        name: sandbox.clone(),
        namespace: ns.clone(),
        owner: kube.owner.clone(),
        run_label: label_value(s.run_id),
        unit_label: unit_label.clone(),
        boot: super::boot_id().into(),
        role: s.role.into(),
        image: s.image.into(),
        floor_image: s.floor_image.into(),
        workdir: s.workdir.to_path_buf(),
        mounts: pod_mounts.clone(),
        memory_limit: s.memory.into(),
        memory_request: if s.role == "judge" {
            "256Mi".into()
        } else {
            "768Mi".into()
        },
        cpu_request: "100m".into(),
        gateway_ip: s.gateway_ip.map(str::to_string),
        shutdown_time: super::rfc3339_after(Duration::from_secs(secs)),
        deadline_secs: secs,
        scope: scope.clone(),
        view: s.view.then(|| s.tree.to_path_buf()),
    };
    let doc = render::seat_sandbox(&spec).map_err(|e| (FloorUnarmed::RootUnresolvable, e))?;
    kube.apply(&[doc]).map_err(|e| (classify_apply(&e), e))?;
    let mounted: Vec<PathBuf> = pod_mounts.iter().map(|m| m.path.clone()).collect();
    let seat = register(Seat {
        id: id.clone(),
        kube: kube.clone(),
        ns: ns.clone(),
        sandbox: sandbox.clone(),
        pod: sandbox.clone(),
        scope,
        cli: s.cli.to_string(),
        tree: s.tree.to_path_buf(),
        roots,
        base,
        seat_dirs: s.seat_dirs.to_vec(),
        mounted,
        unit_label: unit_label.clone(),
        settled: Mutex::new(false),
    });
    // From here every failure reaps the Sandbox (the caller's guard holds the id).
    let start = Instant::now();
    loop {
        let pod = kube
            .get_json(Some(&ns), "pod", &sandbox)
            .map_err(|e| (FloorUnarmed::ClusterUnreachable, e))?;
        match pod.as_ref().map(|p| pod_state(p, &sandbox, &unit_label)) {
            Some(PodState::Ready) => break,
            Some(PodState::Failed(why, msg)) => {
                reap(&id);
                return Err((why, msg));
            }
            _ => {}
        }
        if start.elapsed() > POD_START {
            let why = kube
                .get_json(Some(&ns), "sandboxes.agents.x-k8s.io", &sandbox)
                .ok()
                .flatten()
                .and_then(|sb| sandbox_refusal(&sb))
                .unwrap_or((
                    FloorUnarmed::AdmissionPending,
                    format!("the seat pod {sandbox} did not start in {POD_START:?}"),
                ));
            reap(&id);
            return Err(why);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if let Err(e) = deny_check(kube, &ns, &sandbox, s.gateway_ip.is_some()) {
        reap(&id);
        return Err(e);
    }
    let mut wrapper = kube.head(Some(&ns));
    wrapper.extend(
        [
            "exec",
            "-i",
            "-c",
            "seat",
            sandbox.as_str(),
            "--",
            "/floor/busybox",
            "sh",
            "-c",
            WRAPPER,
        ]
        .map(str::to_string),
    );
    wrapper.push(format!("{MARKER_PREFIX}{id}"));
    wrapper.extend(landrun);
    Ok((seat, wrapper))
}

fn classify_apply(e: &str) -> FloorUnarmed {
    if e.contains("exceeded quota") {
        FloorUnarmed::AdmissionPending
    } else if e.contains("Unable to connect")
        || e.contains("connection refused")
        || e.contains("timed out")
    {
        FloorUnarmed::ClusterUnreachable
    } else {
        FloorUnarmed::PolicyRefused
    }
}

/// Make a seat command complete right before it spawns: every variable the carrier set
/// explicitly goes into the pod's unit env file (over stdin) and is passed by name through
/// landrun; the seat homes are re-synced (the runner wrote their placeholder files after
/// arming). The returned command is kubectl alone, with no carrier variable in its own env.
/// `Ok(None)`: `cmd` is not a pod seat's.
pub(crate) fn finalize(cmd: &Command) -> Result<Option<(Command, Arc<Seat>)>, String> {
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let Some(id) = seat_id_of(&args) else {
        return Ok(None);
    };
    let seat = lookup(&id).ok_or("the seat's pod is no longer armed")?;
    // Where landrun's own options end (`--` before the launcher's `--check`).
    let land = args
        .iter()
        .position(|a| a == "/floor/landrun")
        .ok_or("the seat argv has no landrun")?;
    let end = args[land..]
        .windows(2)
        .position(|w| w == ["--", "/floor/launch"])
        .map(|i| land + i)
        .ok_or("the seat argv's landrun options do not end")?;
    let mut vars: Vec<(String, String)> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut home: Option<String> = None;
    let mounted = &seat.mounted;
    for (k, v) in cmd.get_envs() {
        let key = k.to_string_lossy().into_owned();
        let Some(v) = v else { continue };
        match key.as_str() {
            "PWD"
            | "OLDPWD"
            | "SHELL"
            | "TMPDIR"
            | "DYLD_LIBRARY_PATH"
            | "DYLD_INSERT_LIBRARIES" => continue,
            "HOME" => {
                home = Some(v.to_string_lossy().into_owned());
                continue;
            }
            "PATH" => {
                vars.push((
                    "PATH".into(),
                    crate::container_floor::container_path(v, mounted),
                ));
                continue;
            }
            _ => {}
        }
        vars.push((
            key.clone(),
            crate::container_floor::canonical_spelling(v, mounted)
                .to_string_lossy()
                .into_owned(),
        ));
        names.push(key);
    }
    // agy's configuration home IS its home: the carrier's seat home when the pod mounts it
    // writable; every other CLI keeps the pod's in-memory home.
    let home = home
        .filter(|_| {
            wicked_apps_core::spawn::SeatCli::from_binary(&seat.cli)
                == wicked_apps_core::spawn::SeatCli::Agy
        })
        .filter(|h| {
            seat.roots
                .iter()
                .any(|r| r.back && Path::new(h).starts_with(&r.host))
        })
        .unwrap_or_else(|| "/home/seat".into());
    vars.push(("HOME".into(), home));
    vars.push(("TMPDIR".into(), "/tmp".into()));
    let cwd = cmd
        .get_current_dir()
        .map(|d| d.canonicalize().unwrap_or_else(|_| d.to_path_buf()))
        .unwrap_or_else(|| seat.tree.clone());
    vars.push(("WICKED_UNIT_CWD".into(), cwd.to_string_lossy().into_owned()));
    let body = env_file(&vars)?;
    // The seat homes again: the placeholder files the runner wrote after arming.
    let homes: Vec<PlanRoot> = seat
        .roots
        .iter()
        .filter(|r| seat.seat_dirs.contains(&r.host))
        .cloned()
        .collect();
    if !homes.is_empty() {
        copy_in(
            &seat.kube,
            &seat.ns,
            seat.scope.as_deref(),
            &seat.tree,
            &homes,
            false,
        )
        .map_err(|(_, e)| e)?;
    }
    let r = seat.kube.call(
        Some(&seat.ns),
        &[
            "exec",
            "-i",
            "-c",
            "seat",
            seat.pod.as_str(),
            "--",
            "/floor/busybox",
            "sh",
            "-c",
            "umask 077; rm -f /wicked-unit/rc; cat > /wicked-unit/env.tmp && /floor/busybox mv /wicked-unit/env.tmp /wicked-unit/env",
        ],
        Some(body.as_bytes()),
        CALL,
    );
    if !r.ok() {
        return Err(format!(
            "could not hand the seat its unit env: {}",
            r.err_text()
        ));
    }
    let mut out_args = args[..end].to_vec();
    for n in &names {
        if !args[land..end]
            .windows(2)
            .any(|w| w == ["--env", n.as_str()])
        {
            out_args.push("--env".into());
            out_args.push(n.clone());
        }
    }
    out_args.extend_from_slice(&args[end..]);
    let mut out = Command::new(cmd.get_program());
    {
        use wicked_apps_core::HardenedCommand;
        out.hardened();
    }
    out.args(&out_args);
    out.env_clear();
    for k in ["PATH", "LANG", "USER", "LOGNAME"] {
        if let Some(v) = std::env::var_os(k) {
            out.env(k, v);
        }
    }
    if let Some(h) = std::env::var_os("HOME") {
        out.env("HOME", h);
    }
    if let Some(dir) = cmd.get_current_dir() {
        out.current_dir(dir);
    }
    Ok(Some((out, seat)))
}

/// Delete the seat's Sandbox (grace 0) and wait until its pod is gone; unregister it. `Err` when
/// the pod could not be confirmed gone (then nothing of the unit may be read back).
pub(crate) fn delete_pod(seat: &Seat) -> Result<(), String> {
    seat.kube.delete_owned(
        Some(&seat.ns),
        "sandboxes.agents.x-k8s.io",
        &seat.sandbox,
        false,
    )?;
    let start = Instant::now();
    loop {
        match seat.kube.get_json(Some(&seat.ns), "pod", &seat.pod)? {
            None => return Ok(()),
            Some(_) if start.elapsed() > Duration::from_secs(90) => {
                return Err(format!("the seat pod {} was not gone in time", seat.pod))
            }
            Some(p) => {
                if p.pointer("/metadata/deletionTimestamp").is_none()
                    && start.elapsed() > Duration::from_secs(5)
                {
                    // The controller has not taken it down: the pod itself goes.
                    let _ = seat.kube.call(
                        Some(&seat.ns),
                        &[
                            "delete",
                            "pod",
                            seat.pod.as_str(),
                            "--grace-period=0",
                            "--force",
                            "--wait=false",
                        ],
                        None,
                        CALL,
                    );
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

/// Reap a seat however its unit ended (a guard's drop, an early refusal): its pod goes, nothing
/// is read back.
pub(crate) fn reap(id: &str) {
    if let Some(seat) = lookup(id) {
        let mut settled = seat.settled.lock().unwrap_or_else(|p| p.into_inner());
        if !*settled {
            if let Err(e) = delete_pod(&seat) {
                eprintln!("wicked-core: seat pod {} not reaped: {e}", seat.pod);
            }
            *settled = true;
        }
    }
    unregister(id);
}

/// How a seat's unit ended, for [`settle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ending {
    /// The kubectl client exited with this code.
    Exited(i32),
    /// The engine's deadline killed the client: the unit's work so far still comes back.
    TimedOut,
    /// Cancelled: the pod goes, nothing comes back.
    Cancelled,
}

/// Settle a seat after its client ended: read its exit code from the pod, delete the pod and
/// wait it out, then write back every root that comes back. `Ok(code)` = the CLI's own exit code
/// (or the client's for a timeout or cancel); `Err` = the unit has no result (a lost stream, a
/// pod not confirmed gone, a write-back refused): could-not-run, never a pass.
pub(crate) fn settle(seat: &Seat, ending: Ending) -> Result<i32, String> {
    let mut settled = seat.settled.lock().unwrap_or_else(|p| p.into_inner());
    if *settled {
        return Err("the seat was already reaped".into());
    }
    let code = match ending {
        Ending::Exited(c) => {
            let rc = seat.kube.call(
                Some(&seat.ns),
                &[
                    "exec",
                    "-c",
                    "seat",
                    seat.pod.as_str(),
                    "--",
                    "/floor/busybox",
                    "cat",
                    "/wicked-unit/rc",
                ],
                None,
                Duration::from_secs(20),
            );
            let rc_text = rc
                .ok()
                .then(|| String::from_utf8_lossy(&rc.stdout).to_string());
            match exit_code(c, rc_text.as_deref()) {
                Some(code) => Ok(code),
                None => Err(format!(
                    "{}: kubectl exited {c} and the pod's rc file {}",
                    FloorUnarmed::ExecStreamLost.describe(),
                    match &rc_text {
                        Some(t) => format!("says {}", t.trim()),
                        None => "is missing".into(),
                    }
                )),
            }
        }
        Ending::TimedOut | Ending::Cancelled => Ok(-1),
    };
    let gone = delete_pod(seat);
    *settled = true;
    drop(settled);
    unregister(&seat.id);
    gone.map_err(|e| format!("{}: {e}", FloorUnarmed::ExecStreamLost.describe()))?;
    let code = code?;
    if ending != Ending::Cancelled {
        write_back(seat)?;
    }
    Ok(code)
}

/// The write-back (B11): every root that comes back is packed in the sync pod, checked on the
/// host and applied with the no-follow walk; then the tree's new commits are fetched as
/// untrusted input and the host's HEAD moves only on a fast-forward.
pub(crate) fn write_back(seat: &Seat) -> Result<(), String> {
    let back: Vec<&PlanRoot> = seat.roots.iter().filter(|r| r.back).collect();
    if back.is_empty() {
        return Ok(());
    }
    let mut refs: Vec<(String, &PlanRoot)> = Vec::new();
    for r in &back {
        refs.push((replica_of(seat.scope.as_deref(), &r.host)?, r));
    }
    let plan = codec_plan(&refs).to_string();
    let r = codec(
        &seat.kube,
        &seat.ns,
        &["pack", &plan],
        None,
        Duration::from_secs(300),
    );
    if !r.ok() {
        return Err(format!(
            "the write-back could not read the replica: {}",
            r.err_text()
        ));
    }
    let roots = sync::decode(&mut r.stdout.as_slice(), STREAM_LIMIT)
        .map_err(|e| format!("the write-back was refused: {e}"))?;
    if roots.len() != back.len() {
        return Err("the write-back's roots do not match the unit's".into());
    }
    // Every root checked before any is applied.
    let mut checked: Vec<(&PlanRoot, Vec<Entry>)> = Vec::new();
    for ((replica, want), got) in refs.iter().zip(&roots) {
        if &got.replica != replica {
            return Err("the write-back's roots are out of order".into());
        }
        let mut entries = sync::check_entries(got, &want.skip)
            .map_err(|e| format!("the write-back of {} was refused: {e}", want.host.display()))?;
        if want.mode == Mode::Files {
            entries.retain(|e| want.files.contains(&e.path));
        }
        checked.push((want, entries));
    }
    for (want, entries) in &checked {
        let listed = if want.mode == Mode::Files {
            want.files.clone()
        } else if want.mode == Mode::Tree {
            sync::host_tree_listing(&want.host)?
        } else {
            sync::host_plain_listing(&want.host, &want.skip)
        };
        let delete = sync::deletions(&listed, entries, &want.skip);
        sync::apply(&want.host, entries, &delete)
            .map_err(|e| format!("the write-back of {} failed: {e}", want.host.display()))?;
    }
    if let Some(base) = &seat.base {
        if back
            .iter()
            .any(|r| r.mode == Mode::Tree && r.host == seat.tree)
        {
            fetch_commits(seat, base)?;
        }
    }
    Ok(())
}

/// The unit's new commits: a bundle from the replica, fetched with fsck into
/// `refs/wicked/k8s/<unit>`, and the host's HEAD moved to it only when it is a fast-forward of
/// the base the replica was built at.
fn fetch_commits(seat: &Seat, base: &str) -> Result<(), String> {
    let tree = replica_of(seat.scope.as_deref(), &seat.tree)?;
    let r = codec(
        &seat.kube,
        &seat.ns,
        &["bundle", &tree, base],
        None,
        Duration::from_secs(300),
    );
    if !r.ok() {
        return Err(format!(
            "the unit's commits could not be read: {}",
            r.err_text()
        ));
    }
    if r.stdout.is_empty() {
        return Ok(());
    }
    let file = std::env::temp_dir().join(format!("wicked-unit-{}.bundle", seat.id));
    std::fs::write(&file, &r.stdout).map_err(|e| e.to_string())?;
    struct Rm(PathBuf);
    impl Drop for Rm {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _rm = Rm(file.clone());
    let git = |args: &[&str]| -> Result<String, String> {
        let o = sync::git(&seat.tree)
            .args(args)
            .output()
            .map_err(|e| e.to_string())?;
        if o.status.success() {
            Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
        } else {
            Err(format!(
                "git {}: {}",
                args.iter().find(|a| !a.starts_with('-')).unwrap_or(&""),
                String::from_utf8_lossy(&o.stderr).trim()
            ))
        }
    };
    let refname = format!("refs/wicked/k8s/{}", seat.unit_label);
    let f = file.to_string_lossy().into_owned();
    git(&["bundle", "verify", "-q", &f])?;
    git(&[
        "-c",
        "fetch.fsckObjects=true",
        "-c",
        "transfer.fsckObjects=true",
        "fetch",
        "-q",
        "--no-tags",
        "--no-write-fetch-head",
        &f,
        &format!("+HEAD:{refname}"),
    ])?;
    let new = git(&["rev-parse", "--verify", &format!("{refname}^{{commit}}")])?;
    let ff = sync::git(&seat.tree)
        .args(["merge-base", "--is-ancestor", base, &new])
        .status()
        .map_err(|e| e.to_string())?
        .success();
    let result = if ff {
        git(&[
            "update-ref",
            "-m",
            "wicked: the unit's commits (k8s write-back)",
            "HEAD",
            &new,
            base,
        ])
        .and_then(|_| git(&["read-tree", &new]))
        .map(|_| ())
    } else {
        Err(format!(
            "the unit's branch is not a fast-forward of {base} (its history was rewritten); its commits are kept at {refname}"
        ))
    };
    if result.is_ok() {
        let _ = git(&["update-ref", "-d", &refname]);
    }
    result
}

/// The cluster each one-shot session armed on (its scope is deleted there when it ends).
static SCOPES: Mutex<Option<HashMap<String, Kube>>> = Mutex::new(None);

/// Delete a one-shot session's replica scope (its session ended). A session that never armed a
/// pod has nothing to delete.
pub(crate) fn drop_scope(run_id: &str) {
    let Some(scope) = one_shot_scope(run_id) else {
        return;
    };
    let kube = SCOPES
        .lock()
        .ok()
        .and_then(|mut m| m.as_mut().and_then(|m| m.remove(run_id)));
    let Some(kube) = kube else {
        return;
    };
    let (ns, _, _) = place(&kube, run_id);
    let r = kube.call(
        Some(&ns),
        &[
            "exec",
            "-c",
            "sync",
            "sync",
            "--",
            "/floor/busybox",
            "rm",
            "-rf",
            &format!("/replica/{scope}"),
        ],
        None,
        CALL,
    );
    if !r.ok() {
        eprintln!(
            "wicked-core: session scope {scope} not removed: {}",
            r.err_text()
        );
    }
}

/// Turn a plan's mounts (from the floor) into [`PlanMount`]s.
pub(crate) fn plan_mounts(mounts: &[crate::container_floor::Mount]) -> Vec<PlanMount> {
    mounts
        .iter()
        .map(|m| match m {
            crate::container_floor::Mount::Bind { path, rw } => PlanMount::Bind {
                path: path.clone(),
                rw: *rw,
            },
            crate::container_floor::Mount::NullFile { path }
            | crate::container_floor::Mount::Tmpfs { path } => {
                PlanMount::Mask { path: path.clone() }
            }
        })
        .collect()
}

/// A registered seat with no roots, for the runner's wiring tests (pod `seat-1` in `ns`).
#[cfg(test)]
pub(crate) fn register_for_test(kube: &Kube, id: &str, tree: &Path, cli: &str) -> Arc<Seat> {
    register(Seat {
        id: id.into(),
        kube: kube.clone(),
        ns: "ns".into(),
        sandbox: "seat-1".into(),
        pod: "seat-1".into(),
        scope: None,
        cli: cli.into(),
        tree: tree.to_path_buf(),
        roots: Vec::new(),
        base: None,
        seat_dirs: Vec::new(),
        mounted: vec![tree.to_path_buf()],
        unit_label: "u".into(),
        settled: Mutex::new(false),
    })
}

/// A replica entry kind for the tests' fixtures.
#[cfg(test)]
pub(crate) fn file(path: &str, data: &[u8]) -> Entry {
    Entry {
        kind: sync::Kind::File,
        path: path.into(),
        data: data.to_vec(),
    }
}
