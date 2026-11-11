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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::render::{self, ReplicaMount, RunBudget, SeatPod};
use super::sync::{self, Entry, Mode, Root};
use super::{cluster, label_value, Kube, ROLE_LABEL, UNIT_LABEL};
use crate::worker_sandbox::FloorUnarmed;

/// The fixed in-pod wrapper (`sh -c WRAPPER <marker>`): the unit file sourced and removed (its env,
/// and `set --` with the launcher and CLI argv, so neither a secret nor the prompt is ever on the
/// exec's argv, which is a URL query on the API server), its working directory entered, the
/// launcher run, its exit code written where [`settle`] reads it.
pub(crate) const WRAPPER: &str = "set -u; u=/wicked-unit; \
[ -f $u/env ] || { echo 'wicked-seat: the unit env is missing' >&2; echo 125 > $u/rc; exit 125; }; \
. $u/env; /floor/busybox rm -f $u/env; \
[ $# -gt 0 ] || { echo 'wicked-seat: the unit argv is missing' >&2; echo 125 > $u/rc; exit 125; }; \
cd \"$WICKED_UNIT_CWD\" || { echo 'wicked-seat: the unit directory is missing' >&2; echo 125 > $u/rc; exit 125; }; \
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
/// The most a seat cache may carry into a replica (review r6 #1): a cache is a speed-up, so a
/// larger one is left out (the seat starts with it empty), never a refusal of the unit.
const SEED_LIMIT: u64 = 256 * 1024 * 1024;

/// The seat-cache roots already seeded, per namespace (`<ns>\0<replica path>`): the codec skips a
/// seed root the replica holds, so the host does not read or send it again (review r6 #1).
static SEEDED: std::sync::Mutex<Option<HashSet<String>>> = std::sync::Mutex::new(None);

fn seeded_key(ns: &str, replica: &str) -> String {
    format!("{ns}\0{replica}")
}

/// A read-only root's host signature (no file is opened: a link-free walk and one `lstat` per
/// entry, its kind, size, mode, inode, mtime and ctime), with the root's skip list and masks.
/// `None` when it cannot be taken: the root is sent.
#[cfg(unix)]
fn root_signature(r: &PlanRoot) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    let mut h = Sha256::new();
    let field = |h: &mut Sha256, b: &[u8]| {
        h.update((b.len() as u64).to_le_bytes());
        h.update(b);
    };
    for s in &r.skip {
        field(&mut h, s.as_bytes());
    }
    h.update([0xff]);
    for s in &r.masks {
        field(&mut h, s.as_bytes());
    }
    h.update([0xff]);
    let mut listed = sync::host_plain_listing(&r.host, &r.skip).ok()?;
    listed.sort();
    for rel in &listed {
        let m = std::fs::symlink_metadata(r.host.join(rel)).ok()?;
        field(&mut h, rel.as_bytes());
        for n in [
            u64::from(m.mode()),
            m.len(),
            m.ino(),
            m.mtime() as u64,
            m.mtime_nsec() as u64,
            m.ctime() as u64,
            m.ctime_nsec() as u64,
        ] {
            h.update(n.to_le_bytes());
        }
    }
    Some(h.finalize().into())
}

#[cfg(not(unix))]
fn root_signature(_r: &PlanRoot) -> Option<[u8; 32]> {
    None
}

/// Host plain roots already read, by host path, at the signature they had then (k8s-final). An
/// empty root or one larger than [`READ_CACHE_ROOT`] is not kept; past [`READ_CACHE_ALL`] bytes
/// or [`READ_CACHE_ROOTS`] roots the cache starts over.
#[allow(clippy::type_complexity)]
static READ_CACHE: std::sync::Mutex<Option<HashMap<PathBuf, ([u8; 32], Vec<Entry>)>>> =
    std::sync::Mutex::new(None);
const READ_CACHE_ROOT: usize = 32 * 1024 * 1024;
const READ_CACHE_ALL: usize = 128 * 1024 * 1024;
const READ_CACHE_ROOTS: usize = 256;

fn read_cache_get(host: &Path, sig: &[u8; 32]) -> Option<Vec<Entry>> {
    let g = READ_CACHE.lock().ok()?;
    let (have, entries) = g.as_ref()?.get(host)?;
    (have == sig).then(|| entries.clone())
}

fn read_cache_put(host: &Path, sig: &[u8; 32], entries: &[Entry]) {
    let size = |es: &[Entry]| {
        es.iter()
            .map(|e| e.data.len() + e.path.len())
            .sum::<usize>()
    };
    if entries.is_empty() || size(entries) > READ_CACHE_ROOT {
        return;
    }
    if let Ok(mut g) = READ_CACHE.lock() {
        let m = g.get_or_insert_with(HashMap::new);
        m.insert(host.to_path_buf(), (*sig, entries.to_vec()));
        if m.len() > READ_CACHE_ROOTS
            || m.values().map(|(_, es)| size(es)).sum::<usize>() > READ_CACHE_ALL
        {
            m.clear();
        }
    }
}

/// The replica's shallow history.
const REPLICA_DEPTH: &str = "50";
/// The deadline of one engine git call on a host tree.
const GIT_CALL: Duration = Duration::from_secs(300);

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
    /// The masks under this root (relative): never copied, scrubbed from the replica at every
    /// copy-in, never written back, never committed.
    pub(crate) masks: Vec<String>,
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
/// `seed_base` = the seat caches' base (seeded once per replica, never written back). Only the
/// tree (a creator's) and the admitted write roots come back (review r6).
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
        // The repository boundary under an ancestor root (a read root above the clone) stays
        // absent there too (review r2); the tree and admitted roots are their own roots.
        skip.extend(sync::nested(path, protected));
        let root_masks = sync::nested(path, &masks);
        skip.extend(root_masks.iter().cloned());
        let seed = seed_base.is_some_and(|b| path.starts_with(b));
        let mode = if path == tree {
            Mode::Tree
        } else if seed {
            Mode::Seed
        } else {
            Mode::Plain
        };
        // What comes back (review r6 #1, #8): a creator's tree and the unit's admitted write
        // roots, nothing else. Seat homes, caches and scratch (a fenced unit's `CARGO_TARGET_DIR`)
        // stay in the replica: they are not the unit's result, a parallel unit of the same CLI
        // rewrites the same home, and nothing a seat leaves there reaches the host.
        let admitted = path == tree || write_roots.iter().any(|w| path.starts_with(w));
        let back = *rw && !seed && !(view && path == tree) && admitted;
        if path == tree {
            skip.push(".git".into());
        }
        roots.push(PlanRoot {
            host: path.clone(),
            mode,
            skip,
            back,
            files: Vec::new(),
            masks: root_masks,
        });
    }
    let mut file_roots: Vec<(PathBuf, (Vec<String>, bool))> = files.into_iter().collect();
    file_roots.sort();
    for (dir, (mut names, rw)) in file_roots {
        names.sort();
        names.dedup();
        let admitted = write_roots.iter().any(|w| dir.starts_with(w));
        roots.push(PlanRoot {
            host: dir,
            mode: Mode::Files,
            skip: Vec::new(),
            back: rw && admitted,
            files: names,
            masks: Vec::new(),
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
                serde_json::json!([replica, r.mode.as_str(), list, r.masks])
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

/// The `set --` line that hands the wrapper the launcher and CLI argv (pure, single-quoted).
pub(crate) fn argv_line(argv: &[String]) -> Result<String, String> {
    if argv.is_empty() {
        return Err("the seat has no argv".into());
    }
    let mut s = String::from("set --");
    for a in argv {
        if a.contains('\0') {
            return Err("a seat argument holds a NUL".into());
        }
        s.push_str(&format!(" '{}'", a.replace('\'', "'\\''")));
    }
    s.push('\n');
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
    /// The branch the host's HEAD named at copy-in (`None`: detached, or no commits): the
    /// commits land on it and nowhere else.
    pub(crate) branch: Option<String>,
    /// What every root that comes back held at copy-in.
    pub(crate) baseline: Mutex<Baseline>,
    /// The unit's seat homes, re-synced right before the CLI starts (placeholder files).
    pub(crate) seat_dirs: Vec<PathBuf>,
    /// Writable dirs that neither come back nor stay (fenced scratch): removed at settle.
    pub(crate) scratch: Vec<PathBuf>,
    /// The canonical spelling of every path the pod mounts (env values are re-spelled to it).
    pub(crate) mounted: Vec<PathBuf>,
    pub(crate) unit_label: String,
    pub(crate) settled: Mutex<bool>,
    /// The run's node_modules claim is mounted: every `node_modules` path of the tree is the
    /// engine's, never the seat's, and never comes back (B-3 review r2).
    pub(crate) deps: bool,
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
        // A digest of the exact id, never a label (labels can collide, review r6 #16).
        .then(|| {
            use sha2::{Digest, Sha256};
            let d = Sha256::digest(run_id.as_bytes());
            format!(
                "s-{}",
                d[..12]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            )
        })
}

/// The namespace a run's pods live in: its own, or the daemon's shared session namespace for a
/// one-shot session.
pub(crate) fn place(kube: &Kube, run_id: &str) -> (String, Option<String>, String) {
    match one_shot_scope(run_id) {
        Some(scope) => (
            super::run_namespace_name(&kube.owner, super::SESSIONS_RUN),
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

/// The seat's homes among `roots` become [`Mode::Home`] (kept in the replica, never back); the
/// fenced `scratch` is no home and stays plain, removed at settle (codex b2-seat #12).
pub(crate) fn mark_homes(roots: &mut [PlanRoot], seat_dirs: &[PathBuf], scratch: Option<&Path>) {
    for r in roots.iter_mut() {
        if r.mode == Mode::Plain
            && seat_dirs.contains(&r.host)
            && scratch.is_none_or(|x| x != r.host)
        {
            r.mode = Mode::Home;
            r.back = false;
        }
    }
}

/// Namespaces whose sync pod a cut codec call left unconfirmed gone (codex b2-seat #5): no codec
/// runs there, and no new copy-in starts, until its deletion is confirmed.
static SYNC_SUSPECT: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Mark (`true`) or clear the sync pod of `ns` as suspect.
pub(crate) fn mark_sync_suspect(ns: &str, on: bool) {
    let mut g = SYNC_SUSPECT.lock().unwrap_or_else(|p| p.into_inner());
    let set = g.get_or_insert_with(HashSet::new);
    if on {
        set.insert(ns.to_string());
    } else {
        set.remove(ns);
    }
}

/// `Ok` when the sync pod of `ns` is not suspect, or its deletion is now confirmed.
fn clear_sync_suspect(kube: &Kube, ns: &str) -> Result<(), String> {
    let suspect = SYNC_SUSPECT
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .is_some_and(|s| s.contains(ns));
    if !suspect {
        return Ok(());
    }
    kube.delete_owned(Some(ns), "pod", "sync", true)
        .map_err(|e| {
            format!(
                "the sync pod in {ns} may still run a cut codec and was not confirmed gone ({e}); \
             the replica is not used until it is"
            )
        })?;
    mark_sync_suspect(ns, false);
    Ok(())
}

/// The sync pod of `ns`, created when absent and waited for.
fn ensure_sync(
    kube: &Kube,
    ns: &str,
    run: &str,
    image: &str,
    floor: &str,
) -> Result<(), (FloorUnarmed, String)> {
    clear_sync_suspect(kube, ns).map_err(|e| (FloorUnarmed::ClusterUnreachable, e))?;
    // Only a sync pod of this boot is used (codex b5 #10): a predecessor's may still run a codec
    // its engine left, and the boot sweep removes it; it is replaced here, never shared.
    let running = |kube: &Kube| -> Result<bool, String> {
        let pods = kube.list_owned(Some(ns), "pods", &[(ROLE_LABEL, "sync")])?;
        let ours: Vec<Value> = pods
            .into_iter()
            .filter(|p| {
                p.pointer(&format!(
                    "/metadata/labels/{}",
                    super::BOOT_LABEL.replace('/', "~1")
                ))
                .and_then(Value::as_str)
                    == Some(super::boot_id())
            })
            .collect();
        Ok(cluster::running_pod(&ours).as_deref() == Some("sync"))
    };
    if running(kube).map_err(|e| (FloorUnarmed::ClusterUnreachable, e))? {
        return Ok(());
    }
    let existing = kube
        .get_json(Some(ns), "pod", "sync")
        .map_err(|e| (FloorUnarmed::ClusterUnreachable, e))?;
    if let Some(p) = existing {
        // A sync pod that ended (or a predecessor's): replaced.
        // A Pending pod is kept only when it is this boot's (codex r2): an earlier boot's holds
        // the name and is replaced whatever its phase.
        let this_boot = p
            .pointer(&format!(
                "/metadata/labels/{}",
                super::BOOT_LABEL.replace('/', "~1")
            ))
            .and_then(Value::as_str)
            == Some(super::boot_id());
        if p.pointer("/status/phase").and_then(Value::as_str) != Some("Pending") || !this_boot {
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
    if let Err(e) = clear_sync_suspect(kube, ns) {
        return super::Called {
            code: None,
            stdout: Vec::new(),
            stderr: e.into_bytes(),
        };
    }
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
    let r = kube.call_capped(Some(ns), &a, stdin, limit, STREAM_LIMIT);
    // Only a client that was cut off or killed can leave a codec running; the codec's own refusal
    // ended it, and resetting the shared pod then would end every other unit's codec in the
    // namespace (review r6 #7, r7 #6).
    if resets_sync(r.code) {
        // A client that gave up may leave its codec running in the sync pod, still writing the
        // replica (review r5): the pod goes, and every process with it, before the replica is
        // used again (the next arm makes a new sync pod).
        if let Err(e) = kube.delete_owned(Some(ns), "pod", "sync", true) {
            eprintln!("wicked-core: the sync pod in {ns} was not reset: {e}");
            mark_sync_suspect(ns, true);
        }
    }
    r
}

/// Whether a codec call's ending leaves a codec that may still be running (pure): only a client
/// that was killed (no exit code). A codec that refused exited itself (review r6 #7).
pub(crate) fn resets_sync(code: Option<i32>) -> bool {
    // A codec ends 0 (done) or 3 (refused, its own `die`); anything else is a client whose stream
    // was cut or killed, whose remote codec may still run (review r7 #6).
    !matches!(code, Some(0) | Some(3))
}

/// The host's side of a tree root's `.git` for the replica: a shallow clone of the tree's HEAD,
/// its branch and index set, read as a `git` root. `None` = the tree has no commit.
fn replica_git(
    tree: &Path,
    masks: &[String],
    budget: u64,
) -> Result<Option<(String, Vec<Entry>)>, String> {
    // Every git call bounded in time and output (codex b2-seat #13).
    let bounded = |dir: &Path, args: &[&str]| -> super::Called {
        let mut c = sync::git(dir);
        c.args(args);
        super::run_capped(c, None, GIT_CALL, 64 << 20)
    };
    let head = bounded(tree, &["rev-parse", "--verify", "-q", "HEAD^{commit}"]);
    if head.code.is_none() {
        return Err(format!(
            "git rev-parse in {}: {}",
            tree.display(),
            head.err_text()
        ));
    }
    if !head.ok() {
        return Ok(None);
    }
    let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
    let branch = head_branch(tree)?;
    // The rebuilt `.git` is a function of the commit, the branch and the masks alone (k8s-final:
    // about eight git processes, up to 7 s at an arm): rebuilt once per key, then reused.
    let key = (
        tree.to_path_buf(),
        sha.clone(),
        branch.clone(),
        masks.to_vec(),
    );
    // Never for a shallow host repository (codex r1 #6): deepening it changes what the fetch
    // brings without moving the commit.
    let shallow = bounded(tree, &["rev-parse", "--is-shallow-repository"]);
    let cacheable = shallow.ok() && String::from_utf8_lossy(&shallow.stdout).trim() == "false";
    let hit = GIT_CACHE
        .lock()
        .ok()
        .filter(|_| cacheable)
        .and_then(|g| g.as_ref().and_then(|m| m.get(&key).cloned()));
    if let Some(entries) = hit {
        let size: u64 = entries
            .iter()
            .map(|e| (e.data.len() + e.path.len()) as u64)
            .sum();
        if size > budget {
            return Err(format!(
                "the replica's .git ({size} bytes) is over the copy-in's remaining {budget} bytes"
            ));
        }
        return Ok(Some((sha, entries)));
    }
    let built = rebuild_replica_git(tree, &sha, &branch, masks, budget)?;
    let size = |es: &[Entry]| {
        es.iter()
            .map(|e| e.data.len() + e.path.len())
            .sum::<usize>()
    };
    // Bounded in bytes (codex r1 #7): a history over a root's share is not kept, and past the
    // whole the cache starts over.
    if cacheable && size(&built) <= GIT_CACHE_ROOT {
        if let Ok(mut g) = GIT_CACHE.lock() {
            let m = g.get_or_insert_with(HashMap::new);
            m.insert(key, built.clone());
            if m.len() > 64 || m.values().map(|es| size(es)).sum::<usize>() > GIT_CACHE_ALL {
                m.clear();
            }
        }
    }
    Ok(Some((sha, built)))
}

/// Rebuilt replica `.git`s by (tree, commit, branch, masks) (k8s-final), each at most
/// [`GIT_CACHE_ROOT`] bytes and [`GIT_CACHE_ALL`] in all.
const GIT_CACHE_ROOT: usize = 16 * 1024 * 1024;
const GIT_CACHE_ALL: usize = 64 * 1024 * 1024;
#[allow(clippy::type_complexity)]
static GIT_CACHE: std::sync::Mutex<
    Option<HashMap<(PathBuf, String, Option<String>, Vec<String>), Vec<Entry>>>,
> = std::sync::Mutex::new(None);

/// [`replica_git`]'s build: a shallow clone of `sha` at `branch`, masks skip-worktree, read whole.
fn rebuild_replica_git(
    tree: &Path,
    sha: &str,
    branch: &Option<String>,
    masks: &[String],
    budget: u64,
) -> Result<Vec<Entry>, String> {
    let bounded = |dir: &Path, args: &[&str]| -> super::Called {
        let mut c = sync::git(dir);
        c.args(args);
        super::run_capped(c, None, GIT_CALL, 64 << 20)
    };
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
        let o = bounded(&tmp, args);
        if o.ok() {
            Ok(())
        } else {
            Err(format!(
                "git {}: {}",
                args.first().unwrap_or(&""),
                o.err_text()
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
        sha,
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
            run(&["update-ref", b, sha])?;
            run(&["symbolic-ref", "HEAD", b])?;
        }
        None => run(&["update-ref", "--no-deref", "HEAD", sha])?,
    }
    run(&["read-tree", sha])?;
    // A masked path git tracks is absent from the replica's tree: marked skip-worktree, so the
    // seat's `git add -A` never stages its deletion (review r1). Its blob stays in the history,
    // as the Docker floor's read-only object store held it.
    if !masks.is_empty() {
        // Every tracked path, matched by the same folded rule as the copy-in (review r2: a
        // pathspec is case-sensitive and glob-shaped; the mask is neither).
        let o = bounded(&tmp, &["ls-files", "-z"]);
        if !o.ok() {
            return Err(format!("git ls-files: {}", o.err_text()));
        }
        let tracked: Vec<String> = o
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .filter(|p| masks.iter().any(|m| sync::under(p, m)))
            .collect();
        if !tracked.is_empty() {
            let mut args = vec!["update-index", "--skip-worktree", "--"];
            args.extend(tracked.iter().map(String::as_str));
            run(&args)?;
        }
    }
    let _ = std::fs::remove_file(tmp.join(".git/FETCH_HEAD"));
    let gitdir = tmp.join(".git");
    // Every file of the rebuilt git dir, or a refusal (review r4: an object pack over the
    // stream's limit was left out silently, leaving a `.git` whose index named missing objects).
    // Sized from metadata before any byte is read (codex b2-seat #13): a history of many small
    // objects over the copy-in's remaining budget is refused without being held in memory.
    let listed = sync::host_plain_listing(&gitdir, &[])?;
    let size: u64 = listed
        .iter()
        .map(|rel| sync::entry_size(&gitdir, rel) + rel.len() as u64)
        .sum();
    if size > budget {
        return Err(format!(
            "the replica's .git ({size} bytes) is over the copy-in's remaining {budget} bytes"
        ));
    }
    let mut entries = Vec::new();
    for rel in listed {
        match sync::read_host_entry(&gitdir, &rel)? {
            Some(e) => entries.push(e),
            None => {
                return Err(format!(
                    "the replica's .git could not be read whole (`{rel}` went away)"
                ))
            }
        }
    }
    Ok(entries)
}

/// The branch a host tree's HEAD names (`refs/heads/...`), or `None` when it is detached.
pub(crate) fn head_branch(tree: &Path) -> Result<Option<String>, String> {
    let mut c = sync::git(tree);
    c.args(["symbolic-ref", "-q", "HEAD"]);
    let o = super::run_capped(c, None, GIT_CALL, 1 << 20);
    match o.code {
        Some(0) => Ok(Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|b| b.starts_with("refs/heads/"))),
        Some(1) => Ok(None),
        _ => Err(format!(
            "git symbolic-ref in {}: {}",
            tree.display(),
            o.err_text()
        )),
    }
}

/// What each root that comes back held at copy-in (relative path -> content digest): the
/// write-back applies only what the seat changed against it (review r1).
pub(crate) type Baseline = HashMap<PathBuf, HashMap<String, [u8; 32]>>;

/// The copy-in stream of `roots` (the host's side), the tree's base commit and the baseline.
pub(crate) fn copy_in_stream(
    scope: Option<&str>,
    tree: &Path,
    roots: &[PlanRoot],
    with_git: bool,
    seeded: &HashSet<String>,
) -> Result<(Vec<u8>, Value, Option<String>, Baseline), String> {
    let mut stream: Vec<Root> = Vec::new();
    let mut plan_roots: Vec<(String, PlanRoot)> = Vec::new();
    let mut base = None;
    let mut baseline = Baseline::new();
    // The bytes read so far (review r3): a root over the stream's limit is refused as it is read,
    // never after the whole of it is in memory.
    let mut budget: u64 = 0;
    let mut count: usize = 0;
    let mut seed_used: u64 = 0;
    // How long each root took on the host (k8s-final arm timing), by replica path.
    let mut took: HashMap<String, u128> = HashMap::new();
    for r in roots {
        let t_root = Instant::now();
        let replica = replica_of(scope, &r.host)?;
        if r.mode == Mode::Seed {
            // Sent once per replica, outside the unit's budget: an oversized cache is left out.
            let entries = if seeded.contains(&replica) {
                Vec::new()
            } else {
                seed_entries(&r.host, &r.skip, &mut seed_used)
            };
            stream.push(Root {
                replica: replica.clone(),
                mode: r.mode,
                entries,
            });
            took.insert(replica.clone(), t_root.elapsed().as_millis());
            plan_roots.push((replica, r.clone()));
            continue;
        }
        // A plain root read before at the same host signature is not read again (k8s-final: the
        // skills snapshot took up to 35 s to open file by file, for every replica and judge).
        // Signed BEFORE it is listed and read, and kept only when signed the same after (codex
        // r1 #5): the entries kept are those of the state the signature names.
        let cached = (r.mode == Mode::Plain)
            .then(|| root_signature(r))
            .flatten()
            .map(|sig| (sig, read_cache_get(&r.host, &sig)));
        let rels: Vec<String> = if r.mode == Mode::Files {
            r.files.clone()
        } else {
            match r.mode {
                Mode::Tree => sync::host_tree_listing(&r.host)?,
                _ => sync::host_plain_listing(&r.host, &r.skip)?,
            }
        };
        let mut entries: Vec<Entry> = Vec::new();
        if let Some((_, Some(hit))) = &cached {
            for e in hit {
                count += 1;
                if count > sync::MAX_ENTRIES {
                    return Err(format!(
                        "the unit's copy-in has more than {} files",
                        sync::MAX_ENTRIES
                    ));
                }
                let size = if e.kind == sync::Kind::Link {
                    256
                } else {
                    e.data.len() as u64
                };
                budget += size.max(e.path.len() as u64);
                if budget > STREAM_LIMIT {
                    return Err(format!(
                        "the unit's copy-in is larger than {STREAM_LIMIT} bytes (at {})",
                        r.host.join(&e.path).display()
                    ));
                }
                entries.push(e.clone());
            }
        }
        // Neither a tree nor a plain root carries git metadata (codex b2-sync #17): only the
        // tree's own rebuilt `.git` root does, and nothing of a `.git` ever comes back.
        let no_git = matches!(r.mode, Mode::Tree | Mode::Plain);
        let read_now = !matches!(&cached, Some((_, Some(_))));
        for p in rels
            .iter()
            .filter(|_| read_now)
            .filter(|p| !r.skip.iter().any(|s| sync::under(p, s)))
            .filter(|p| !no_git || !p.split('/').any(sync::is_git_name))
        {
            // As many entries as a write-back may carry, or refused now (codex b2-sync #16): a
            // unit is never armed on a tree it could not bring back.
            count += 1;
            if count > sync::MAX_ENTRIES {
                return Err(format!(
                    "the unit's copy-in has more than {} files",
                    sync::MAX_ENTRIES
                ));
            }
            budget += sync::entry_size(&r.host, p).max(p.len() as u64);
            if budget > STREAM_LIMIT {
                return Err(format!(
                    "the unit's copy-in is larger than {STREAM_LIMIT} bytes (at {})",
                    r.host.join(p).display()
                ));
            }
            // A file that is there but cannot be read refuses the copy-in (review r5); a path
            // that is gone, or a directory (a gitlink), is simply absent.
            if let Some(e) = sync::read_host_entry(&r.host, p)
                .map_err(|e| format!("the copy-in was refused: {e}"))?
            {
                entries.push(e);
            }
        }
        if let Some((sig, None)) = &cached {
            if root_signature(r).as_ref() == Some(sig) {
                read_cache_put(&r.host, sig, &entries);
            }
        }
        // The tree's `.git` first: the codec mirrors the tree by what that git lists. Always
        // sent, empty when the host has no commit (review r3): an empty root still replaces a
        // `.git` a seat planted in the replica, so its hooks never reach a later unit.
        if r.mode == Mode::Tree && r.host == tree && with_git {
            let t_git = Instant::now();
            let rebuilt = replica_git(tree, &r.masks, STREAM_LIMIT - budget)?;
            took.insert(format!("{replica}/.git"), t_git.elapsed().as_millis());
            let (sha, git_entries) = match rebuilt {
                Some((sha, entries)) => (Some(sha), entries),
                None => (None, Vec::new()),
            };
            // The rebuilt git dir counts against the same budget (review r4).
            budget += git_entries
                .iter()
                .map(|e| (e.data.len() + e.path.len()) as u64)
                .sum::<u64>();
            if budget > STREAM_LIMIT {
                return Err(format!(
                    "the unit's copy-in is larger than {STREAM_LIMIT} bytes (its .git)"
                ));
            }
            {
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
                        masks: Vec::new(),
                    },
                ));
                base = sha;
            }
        }
        if r.back {
            baseline.insert(
                r.host.clone(),
                entries
                    .iter()
                    .map(|e| (e.path.clone(), sync::digest(e)))
                    .collect(),
            );
        }
        let ms = t_root
            .elapsed()
            .as_millis()
            .saturating_sub(took.get(&format!("{replica}/.git")).copied().unwrap_or(0));
        took.insert(replica.clone(), ms);
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
    // What each root moved (k8s-final arm timing): `replica:entries/bytes/ms`.
    super::clock::mark_with(
        "copy_in_read",
        Some(
            stream
                .iter()
                .map(|r| {
                    let bytes: usize = r.entries.iter().map(|e| e.data.len()).sum();
                    let ms = took.get(&r.replica).copied().unwrap_or(0);
                    format!("{}:{}/{}/{}ms", r.replica, r.entries.len(), bytes, ms)
                })
                .collect::<Vec<_>>()
                .join(" "),
        ),
    );
    Ok((sync::encode(&stream), plan, base, baseline))
}

/// A seat cache's entries for its first copy into a replica; empty (with a note) when the caches
/// of one copy-in (`used` so far) would pass [`SEED_LIMIT`].
fn seed_entries(host: &Path, skip: &[String], used: &mut u64) -> Vec<Entry> {
    let mut size: u64 = 0;
    let mut out = Vec::new();
    // A cache is optional (codex b2-sync #11): one that cannot be listed or read whole starts
    // the seat with it empty, said so, never in part.
    let listed = match sync::host_plain_listing(host, skip) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "wicked-core: the seat cache {} is left out: {e}",
                host.display()
            );
            return Vec::new();
        }
    };
    for p in listed {
        size += sync::entry_size(host, &p).max(p.len() as u64);
        if *used + size > SEED_LIMIT {
            eprintln!(
                "wicked-core: the seat cache {} is over {SEED_LIMIT} bytes; the seat starts with it empty",
                host.display()
            );
            return Vec::new();
        }
        match sync::read_host_entry(host, &p) {
            Ok(Some(e)) => out.push(e),
            Ok(None) => {}
            Err(e) => {
                eprintln!(
                    "wicked-core: the seat cache {} is left out: {e}",
                    host.display()
                );
                return Vec::new();
            }
        }
    }
    *used += size;
    out
}

/// Copy `roots` into the replica through the sync pod.
fn copy_in(
    kube: &Kube,
    ns: &str,
    scope: Option<&str>,
    tree: &Path,
    roots: &[PlanRoot],
    with_git: bool,
) -> Result<(Option<String>, Baseline), (FloorUnarmed, String)> {
    let seeded: HashSet<String> = {
        let g = SEEDED.lock().unwrap_or_else(|p| p.into_inner());
        roots
            .iter()
            .filter(|r| r.mode == Mode::Seed)
            .filter_map(|r| replica_of(scope, &r.host).ok())
            .filter(|rep| g.as_ref().is_some_and(|s| s.contains(&seeded_key(ns, rep))))
            .collect()
    };
    let (body, plan, base, baseline) = copy_in_stream(scope, tree, roots, with_git, &seeded)
        .map_err(|e| (FloorUnarmed::RootUnresolvable, e))?;
    if body.len() as u64 > STREAM_LIMIT + SEED_LIMIT {
        return Err((
            FloorUnarmed::RootUnresolvable,
            format!(
                "the unit's copy-in is larger than {} bytes",
                STREAM_LIMIT + SEED_LIMIT
            ),
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
    super::clock::mark_with("copy_in_unpack", Some(format!("{} bytes", body.len())));
    if let Ok(mut g) = SEEDED.lock() {
        let set = g.get_or_insert_with(HashSet::new);
        for r in roots.iter().filter(|r| r.mode == Mode::Seed) {
            if let Ok(rep) = replica_of(scope, &r.host) {
                set.insert(seeded_key(ns, &rep));
            }
        }
    }
    Ok((base, baseline))
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
    /// The unit's fenced scratch (one of `seat_dirs`): removed at settle, never kept.
    pub(crate) scratch: Option<&'a Path>,
    pub(crate) seed_base: Option<&'a Path>,
    pub(crate) memory: &'a str,
    pub(crate) gateway_ip: Option<&'a str>,
    pub(crate) deadline: Duration,
    /// `seat`, or `judge` for an engine-internal session.
    pub(crate) role: &'a str,
    /// The run's read-only `node_modules` claim for the tree (B14), when one was filled.
    pub(crate) deps: Option<&'a str>,
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
/// The deny check's in-pod script (pure): the API server, the internet and (with `host_ip`) the
/// pod's node on every gateway port must be closed; the gateway open when `online`.
pub(crate) fn deny_script(online: bool, host_ip: Option<&str>) -> String {
    let mut node = String::new();
    if let Some(ip) = host_ip {
        // The kubelet port listens on every node address: closed, it proves the policy covers
        // the node at all; the gateway ports are what the seat's Landlock would let through.
        for p in std::iter::once(&10250u16).chain(render::seat_gateway_ports()) {
            node.push_str(&format!("nc_ {ip} {p} node{p} & "));
        }
    }
    format!(
        "nc_() {{ if /floor/busybox nc -w 2 \"$1\" \"$2\" </dev/null >/dev/null 2>&1; then echo \"$3=open\"; else echo \"$3=closed\"; fi; }}; \
         nc_ \"$KUBERNETES_SERVICE_HOST\" \"$KUBERNETES_SERVICE_PORT\" api & nc_ 1.1.1.1 443 net & {node}{} wait",
        if online { "nc_ agw 3000 gw & " } else { "" }
    )
}

/// Whether the deny check's output holds (pure).
pub(crate) fn deny_holds(out: &str, online: bool, node: bool) -> bool {
    out.contains("api=closed")
        && out.contains("net=closed")
        && (!online || out.contains("gw=open"))
        && (!node
            || std::iter::once(&10250u16)
                .chain(render::seat_gateway_ports())
                .all(|p| out.contains(&format!("node{p}=closed"))))
}

fn deny_check(
    kube: &Kube,
    ns: &str,
    pod: &str,
    online: bool,
) -> Result<(), (FloorUnarmed, String)> {
    // The pod's own node on every port the seat's Landlock grants (review r5: NetworkPolicy
    // does not portably cover traffic to the local node), so a node-local listener on such a
    // port is refused here, at arm time, on the cluster as it is.
    let host_ip = node_address(kube.get_json(Some(ns), "pod", pod).ok().flatten().as_ref())?;
    let script = deny_script(online, Some(&host_ip));
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
        if r.ok() && deny_holds(&out, online, true) {
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

/// The node address a seat pod's deny check probes (pure). Without one the node probe cannot
/// run: the arm is refused, never the probe skipped (review r6 #9).
pub(crate) fn node_address(pod: Option<&Value>) -> Result<String, (FloorUnarmed, String)> {
    pod.and_then(|p| p.pointer("/status/hostIP"))
        .and_then(Value::as_str)
        .filter(|ip| {
            !ip.is_empty()
                && ip
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == '.' || c == ':')
        })
        .map(str::to_string)
        .ok_or_else(|| {
            (
                FloorUnarmed::PolicyRefused,
                "the seat pod's node address could not be read, so its isolation from the node \
                 could not be checked"
                    .into(),
            )
        })
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
    super::arming(s.run_id);
    cluster::ensure_run(kube, &run, &RunBudget::default(), "20Gi")
        .map_err(|e| (classify_apply(&e), e))?;
    super::clock::mark("namespace");
    // The sync pod is inspected and replaced under the namespace's arm lock (codex r2): two
    // arms never delete each other's replacement.
    {
        let arming = arm_lock(&ns);
        let _arming = arming.lock().unwrap_or_else(|p| p.into_inner());
        ensure_sync(kube, &ns, &run, s.sync_image, s.floor_image)?;
    }
    super::clock::mark("sync_pod");
    let (roots, pod_mounts) = plan(
        s.tree,
        s.view,
        s.mounts,
        s.protected,
        s.write_roots,
        s.seed_base,
    );
    // Seat homes stay in the replica (review r7 #1); the unit's other writable dirs that do not
    // come back (fenced scratch) are removed from it when the unit settles (review r7 #3).
    // The fenced scratch is among the seat's dirs but is no home (codex b2-seat #12): it stays
    // plain, so it is selected below and removed at settle, never kept in the replica.
    let mut roots = roots;
    mark_homes(&mut roots, s.seat_dirs, s.scratch);
    let scratch: Vec<PathBuf> = pod_mounts
        .iter()
        .filter(|m| m.rw)
        .filter(|m| {
            roots
                .iter()
                .any(|r| r.host == m.path && r.mode == Mode::Plain && !r.back)
        })
        .map(|m| m.path.clone())
        .collect();
    // One arm at a time copies into a namespace's replica (review r7 #7): the stale-pod check and
    // the copy-in see the same pods.
    let arming = arm_lock(&ns);
    let _arming = arming.lock().unwrap_or_else(|p| p.into_inner());
    clear_stale(kube, &ns)?;
    super::clock::mark("clear_stale");
    let (base, baseline) = copy_in(kube, &ns, scope.as_deref(), s.tree, &roots, true)?;
    drop(_arming);
    // The branch the commits land on, as it was at copy-in (codex b2-seat #9).
    let branch = match &base {
        Some(_) => head_branch(s.tree).map_err(|e| (FloorUnarmed::RootUnresolvable, e))?,
        None => None,
    };
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
        deps: s.deps.map(|c| (c.to_string(), s.tree.to_path_buf())),
    };
    let doc = render::seat_sandbox(&spec).map_err(|e| (FloorUnarmed::RootUnresolvable, e))?;
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
        branch,
        baseline: Mutex::new(baseline),
        seat_dirs: s.seat_dirs.to_vec(),
        scratch,
        mounted,
        unit_label: unit_label.clone(),
        settled: Mutex::new(false),
        deps: s.deps.is_some(),
    });
    // Owned from here: every way out before the caller holds the seat reaps it (review r1: an
    // API error while waiting, or an apply whose reply was lost, leaked the Sandbox).
    struct Guard(Option<String>);
    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(id) = self.0.take() {
                reap(&id);
            }
        }
    }
    let mut guard = Guard(Some(id.clone()));
    kube.apply(&[doc]).map_err(|e| (classify_apply(&e), e))?;
    // From here every failure reaps the Sandbox (the caller's guard holds the id).
    super::clock::mark("sandbox_apply");
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
    super::clock::mark("pod_ready");
    if let Err(e) = deny_check(kube, &ns, &sandbox, s.gateway_ip.is_some()) {
        reap(&id);
        return Err(e);
    }
    super::clock::mark("deny_check");
    // The nested workspace node_modules the fill archived (B14): unpacked into the seat's tree
    // before its CLI starts. Ignored by git, so never written back.
    if s.deps.is_some() {
        let script = nested_unpack_script(s.tree);
        let r = kube.call(
            Some(&ns),
            &[
                "exec",
                "-c",
                "seat",
                sandbox.as_str(),
                "--",
                "/floor/busybox",
                "sh",
                "-c",
                &script,
            ],
            None,
            Duration::from_secs(120),
        );
        // A seat never starts on half its dependencies (review r1).
        if !r.ok() {
            reap(&id);
            return Err((
                FloorUnarmed::ClusterUnreachable,
                format!(
                    "the seat's nested node_modules could not be unpacked: {}",
                    r.err_text()
                ),
            ));
        }
    }
    if s.deps.is_some() {
        super::clock::mark("deps_unpack");
    }
    guard.0 = None;
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
                // In the mounts' own spelling (review r4: macOS spells `/tmp` canonically under `/private`).
                home = Some(
                    crate::container_floor::canonical_spelling(v, mounted)
                        .to_string_lossy()
                        .into_owned(),
                );
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
                .any(|r| (r.back || r.mode == Mode::Home) && Path::new(h).starts_with(&r.host))
        })
        .unwrap_or_else(|| "/home/seat".into());
    vars.push(("HOME".into(), home));
    vars.push(("TMPDIR".into(), "/tmp".into()));
    let cwd = cmd
        .get_current_dir()
        .map(|d| d.canonicalize().unwrap_or_else(|_| d.to_path_buf()))
        .unwrap_or_else(|| seat.tree.clone());
    vars.push(("WICKED_UNIT_CWD".into(), cwd.to_string_lossy().into_owned()));
    // The launcher and CLI argv (after the marker), each carrier variable passed by name
    // through landrun, ride the unit file; the exec's own argv ends at the marker.
    let marker = args
        .iter()
        .position(|a| *a == format!("{MARKER_PREFIX}{id}"))
        .ok_or("the seat argv has no marker")?;
    let mut launch = args[marker + 1..end].to_vec();
    for n in &names {
        if !args[land..end]
            .windows(2)
            .any(|w| w == ["--env", n.as_str()])
        {
            launch.push("--env".into());
            launch.push(n.clone());
        }
    }
    launch.extend_from_slice(&args[end..]);
    let body = format!("{}{}", env_file(&vars)?, argv_line(&launch)?);
    // The seat homes again: the placeholder files the runner wrote after arming.
    let homes: Vec<PlanRoot> = seat
        .roots
        .iter()
        .filter(|r| seat.seat_dirs.contains(&r.host) && r.mode != Mode::Seed)
        .cloned()
        .collect();
    if !homes.is_empty() {
        let (_, more) = copy_in(
            &seat.kube,
            &seat.ns,
            seat.scope.as_deref(),
            &seat.tree,
            &homes,
            false,
        )
        .map_err(|(_, e)| e)?;
        if let Ok(mut b) = seat.baseline.lock() {
            b.extend(more);
        }
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
    let out_args = args[..=marker].to_vec();
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
    delete_seat_pod(&seat.kube, &seat.ns, Some(&seat.sandbox), &seat.pod)
}

/// [`delete_pod`] by name: the Sandbox (when the pod has one), then the pod waited out.
fn delete_seat_pod(kube: &Kube, ns: &str, sandbox: Option<&str>, pod: &str) -> Result<(), String> {
    if let Some(sandbox) = sandbox {
        kube.delete_owned(Some(ns), "sandboxes.agents.x-k8s.io", sandbox, false)?;
    }
    let start = Instant::now();
    loop {
        match kube.get_json(Some(ns), "pod", pod)? {
            None => return Ok(()),
            Some(_) if start.elapsed() > Duration::from_secs(90) => {
                return Err(format!("the seat pod {pod} was not gone in time"))
            }
            Some(p) => {
                if p.pointer("/metadata/deletionTimestamp").is_none()
                    && start.elapsed() > Duration::from_secs(5)
                {
                    // The controller has not taken it down: the pod itself is deleted, never
                    // forced (review r1: a forced delete removes the object while its processes
                    // may still run; an ordinary one is gone only once the kubelet stopped them).
                    let _ = kube.call(
                        Some(ns),
                        &["delete", "pod", pod, "--wait=false"],
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
            match delete_pod(&seat) {
                Ok(()) => drop_scratch(&seat),
                Err(e) => {
                    eprintln!("wicked-core: seat pod {} not reaped: {e}", seat.pod);
                    unreaped(&seat);
                }
            }
            *settled = true;
        }
    }
    unregister(id);
}

/// The per-namespace arm lock ([`arm`], review r7 #7).
fn arm_lock(ns: &str) -> Arc<Mutex<()>> {
    static LOCKS: Mutex<Option<HashMap<String, Arc<Mutex<()>>>>> = Mutex::new(None);
    let mut g = LOCKS.lock().unwrap_or_else(|p| p.into_inner());
    g.get_or_insert_with(HashMap::new)
        .entry(ns.to_string())
        .or_default()
        .clone()
}

/// Remove a settled unit's scratch from the replica (review r7 #3): a fenced unit's
/// `CARGO_TARGET_DIR` and caches would otherwise fill the run's replica unit by unit.
fn drop_scratch(seat: &Seat) {
    for p in &seat.scratch {
        let Ok(rep) = replica_of(seat.scope.as_deref(), p) else {
            continue;
        };
        let target = format!("/replica/{rep}");
        let r = seat.kube.call(
            Some(&seat.ns),
            &[
                "exec",
                "-c",
                "sync",
                "sync",
                "--",
                "/floor/busybox",
                "rm",
                "-rf",
                &target,
            ],
            None,
            CALL,
        );
        if !r.ok() {
            eprintln!(
                "wicked-core: the unit scratch {target} in {} was not removed: {}",
                seat.ns,
                r.err_text()
            );
        }
    }
}

/// Seat pods of this engine whose deletion was not confirmed (`(namespace, sandbox, pod)`): the
/// next arm in their namespace deletes them before any copy-in (review r6 #6).
static UNREAPED: std::sync::Mutex<Vec<(String, String, String)>> =
    std::sync::Mutex::new(Vec::new());

fn unreaped(seat: &Seat) {
    if let Ok(mut u) = UNREAPED.lock() {
        u.push((seat.ns.clone(), seat.sandbox.clone(), seat.pod.clone()));
    }
}

/// Before a copy-in into `ns`'s replica (review r6 #6): no seat or judge pod may still be able to
/// write it except a live seat of this engine. A pod this engine could not confirm gone, and any
/// seat pod an earlier engine left (another boot), is deleted and waited out; if that cannot be
/// confirmed, the arm is refused (its write-back could carry another unit's writes).
fn clear_stale(kube: &Kube, ns: &str) -> Result<(), (FloorUnarmed, String)> {
    let refused = |e: String| {
        (
            FloorUnarmed::ClusterUnreachable,
            format!("an earlier seat pod in this namespace could not be confirmed gone: {e}"),
        )
    };
    let pending: Vec<(String, String, String)> = {
        let mut u = UNREAPED.lock().unwrap_or_else(|p| p.into_inner());
        let (mine, rest): (Vec<_>, Vec<_>) = u.drain(..).partition(|(n, _, _)| n == ns);
        *u = rest;
        mine
    };
    for (i, (n, sandbox, pod)) in pending.iter().enumerate() {
        if let Err(e) = delete_seat_pod(kube, n, Some(sandbox), pod) {
            if let Ok(mut u) = UNREAPED.lock() {
                u.extend(pending[i..].iter().cloned());
            }
            return Err(refused(e));
        }
    }
    let pods = kube.list_owned(Some(ns), "pods", &[]).map_err(refused)?;
    for p in stale_seat_pods(&pods, super::boot_id()) {
        delete_seat_pod(kube, ns, p.1.as_deref(), &p.0).map_err(refused)?;
    }
    Ok(())
}

/// The seat and judge pods of `pods` another engine boot created (pure): `(pod, its Sandbox)`.
pub(crate) fn stale_seat_pods(pods: &[Value], boot: &str) -> Vec<(String, Option<String>)> {
    pods.iter()
        .filter(|p| {
            let label = |k: &str| {
                p.pointer("/metadata/labels")
                    .and_then(|l| l.get(k))
                    .and_then(Value::as_str)
            };
            matches!(label(super::ROLE_LABEL), Some("seat") | Some("judge"))
                && label(super::BOOT_LABEL) != Some(boot)
        })
        .filter_map(|p| {
            let name = p.pointer("/metadata/name")?.as_str()?.to_string();
            let sandbox = p
                .pointer("/metadata/ownerReferences")
                .and_then(Value::as_array)
                .and_then(|o| {
                    o.iter()
                        .find(|r| r.get("kind").and_then(Value::as_str) == Some("Sandbox"))
                })
                .and_then(|r| r.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Some((name, sandbox))
        })
        .collect()
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
#[cfg(test)]
pub(crate) fn settle(seat: &Seat, ending: Ending) -> Result<i32, String> {
    settle_unless(seat, ending, &|| false)
}

/// [`settle`], with `cancelled` asked again once the pod is gone, right before the write-back
/// (codex b2-gw #4): a unit cancelled or reassigned while its rc was read or its pod deleted
/// brings nothing back.
pub(crate) fn settle_unless(
    seat: &Seat,
    ending: Ending,
    cancelled: &dyn Fn() -> bool,
) -> Result<i32, String> {
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
    if gone.is_err() {
        unreaped(seat);
    }
    *settled = true;
    drop(settled);
    unregister(&seat.id);
    gone.map_err(|e| format!("{}: {e}", FloorUnarmed::ExecStreamLost.describe()))?;
    // The pod is gone: the scratch goes on every path from here (codex b2-seat #14), a lost rc
    // included; only a unit with a result, still wanted, is written back.
    let back = if code.is_ok() && ending != Ending::Cancelled && !cancelled() {
        write_back(seat, cancelled)
    } else {
        Ok(())
    };
    drop_scratch(seat);
    let code = code?;
    back?;
    Ok(code)
}

/// The write-back (B11): every root that comes back is packed in the sync pod and checked on the
/// host whole; the unit's new commits are fetched (fsck), checked for a fast-forward and for a
/// masked or `.git` path, all BEFORE any host file changes (review r1); then only what the seat
/// changed against the copy-in baseline is applied with the no-follow walk, a file the host also
/// changed meanwhile is a conflict (refused, never overwritten), and the host HEAD moves last.
pub(crate) fn write_back(seat: &Seat, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
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
    let baseline = seat
        .baseline
        .lock()
        .map_err(|_| "the copy-in baseline is poisoned")?
        .clone();
    // Every root checked before anything is fetched or applied; diffed against the host only
    // after the commits are in (below), so the conflict check reads the host as it is when the
    // write-back lands, not minutes earlier (codex b2-seat #4).
    let mut checked: Vec<(&PlanRoot, Vec<Entry>)> = Vec::new();
    for ((replica, want), got) in refs.iter().zip(&roots) {
        if &got.replica != replica {
            return Err("the write-back's roots are out of order".into());
        }
        let empty = HashMap::new();
        let mut entries = sync::check_entries_against(
            got,
            &want.skip,
            baseline.get(&want.host).unwrap_or(&empty),
        )
        .map_err(|e| format!("the write-back of {} was refused: {e}", want.host.display()))?;
        if want.mode == Mode::Files {
            entries.retain(|e| want.files.contains(&e.path));
        }
        if seat.deps && want.mode == Mode::Tree && want.host == seat.tree {
            entries.retain(|e| !in_node_modules(&e.path));
        }
        checked.push((want, entries));
    }
    let tree_back = back
        .iter()
        .any(|r| r.mode == Mode::Tree && r.host == seat.tree);
    // A tree with no commit on the host: a first commit made in the pod is refused, never
    // dropped while the files land (review r3). The engine imports commits only on top of the
    // host's HEAD.
    if seat.base.is_none() && tree_back {
        let tree = replica_of(seat.scope.as_deref(), &seat.tree)?;
        let r = codec(
            &seat.kube,
            &seat.ns,
            &["bundle", &tree, NO_BASE],
            None,
            Duration::from_secs(60),
        );
        if !r.ok() {
            return Err(format!(
                "the write-back was refused: {} (the host's tree has no commit to land them on)",
                r.err_text()
            ));
        }
    }
    let commits = match &seat.base {
        Some(base) if tree_back => {
            // The tree's whole write policy (codex b2-seat #11): its masks and its nested roots,
            // which a commit could otherwise change without a file write.
            let fence = back
                .iter()
                .find(|r| r.host == seat.tree)
                .map(|r| r.skip.clone())
                .unwrap_or_default();
            fetch_commits(seat, base, &fence)?
        }
        _ => None,
    };
    // The host's HEAD must still be the base the commits fast-forward (review r2), checked
    // before any file changes.
    if let (Some(_), Some(base)) = (&commits, &seat.base) {
        let o = sync::git(&seat.tree)
            .args(["rev-parse", "--verify", "-q", "HEAD^{commit}"])
            .output()
            .map_err(|e| e.to_string())?;
        if String::from_utf8_lossy(&o.stdout).trim() != base {
            return Err(format!(
                "the host's HEAD moved off {base} while the unit ran; its commits are not landed"
            ));
        }
    }
    let mut plans: Vec<RootPlan<'_>> = Vec::new();
    for (want, entries) in checked {
        let base = baseline.get(&want.host).cloned().unwrap_or_default();
        let retained = retained_links(&entries, &base);
        let (writes, mut deletes) = delta(&want.host, &base, entries)?;
        if seat.deps && want.mode == Mode::Tree && want.host == seat.tree {
            deletes.retain(|d| !in_node_modules(d));
        }
        // The pod's `ls-files -c -o --exclude-standard` lists every tracked file, ignored or
        // not, so a tracked path it does not list was deleted (codex b2-seat #10). Only an
        // untracked one the host's ignore rules now match is no deletion (the safe side).
        if want.mode == Mode::Tree && !deletes.is_empty() {
            deletes = tree_deletions(&want.host, deletes)?;
        }
        check_retained_links(&want.host, &retained, &writes, &deletes, &want.skip)
            .map_err(|e| format!("the write-back of {} was refused: {e}", want.host.display()))?;
        plans.push((want, writes, deletes));
    }
    preflight(&plans)?;
    // The destination is the copy-in's checkout, still as it was (codex b2-seat #9): its branch,
    // or still detached (a detached copy-in never lands on a branch checked out since).
    if commits.is_some() && head_branch(&seat.tree)? != seat.branch {
        return Err(format!(
            "the host's checkout moved off {} while the unit ran; its commits are not landed",
            seat.branch.as_deref().unwrap_or("its detached HEAD")
        ));
    }
    // The index the landing installs, made before any host file changes (codex b2-seat #7): a
    // held `index.lock` or a failed build refuses the write-back with the host untouched.
    let staged_index = match &commits {
        Some((new, _)) => Some(prepare_index(&seat.tree, new, &seat.id)?),
        None => None,
    };
    // The last word before the host changes (codex r2): a unit cancelled or reassigned while
    // its changes were packed, fetched or checked brings nothing back.
    if cancelled() {
        return Err(
            "the unit was cancelled before its write-back landed; nothing came back".into(),
        );
    }
    apply_roots(&plans)?;
    if let (Some((new, refname)), Some(base)) = (commits, &seat.base) {
        let git = |args: &[&str]| -> Result<(), String> {
            let o = sync::git(&seat.tree)
                .args(args)
                .output()
                .map_err(|e| e.to_string())?;
            if o.status.success() {
                Ok(())
            } else {
                Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
            }
        };
        let mut update = vec![
            "update-ref",
            "-m",
            "wicked: the unit's commits (k8s write-back)",
        ];
        match &seat.branch {
            Some(b) => update.push(b.as_str()),
            None => update.extend(["--no-deref", "HEAD"]),
        }
        update.extend([new.as_str(), base.as_str()]);
        git(&update).map_err(|e| format!("the unit's commits could not land: {e}"))?;
        if let Some(staged) = staged_index {
            staged.install().map_err(|e| {
                format!(
                    "the unit's commits landed but the index could not be updated ({e}); run \
                     `git read-tree HEAD` in {}",
                    seat.tree.display()
                )
            })?;
        }
        let _ = git(&["update-ref", "-d", &refname]);
    }
    Ok(())
}

/// A new owner-only directory under the temp dir, removed with everything in it when dropped.
pub(crate) struct PrivateDir(pub(crate) PathBuf);

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Make a [`PrivateDir`] (`0700`, created new: an existing path of that name is an error).
pub(crate) fn private_temp_dir(tag: &str) -> Result<PrivateDir, String> {
    let p = std::env::temp_dir().join(format!("{tag}-{}", uuid::Uuid::new_v4().simple()));
    // `mode` is Unix-only, so the binding is mutated only there.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(PrivateDir(p))
}

/// The index a landing installs, built aside from the host's own (removed unless installed).
pub(crate) struct StagedIndex {
    staged: PathBuf,
    index: PathBuf,
    done: bool,
}

impl StagedIndex {
    /// Put the staged index in place of the host's (one rename).
    pub(crate) fn install(mut self) -> std::io::Result<()> {
        std::fs::rename(&self.staged, &self.index)?;
        self.done = true;
        Ok(())
    }
}

impl Drop for StagedIndex {
    fn drop(&mut self) {
        if !self.done {
            let _ = std::fs::remove_file(&self.staged);
        }
        // The alternate index's own lock, when a killed `read-tree` left it (codex r2).
        let mut lock = self.staged.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }
}

/// Build the index of commit `new` for the host tree beside its own index (codex b2-seat #7):
/// refused, with nothing changed, while another git holds `index.lock`.
pub(crate) fn prepare_index(tree: &Path, new: &str, id: &str) -> Result<StagedIndex, String> {
    let mut c = sync::git(tree);
    c.args(["rev-parse", "--path-format=absolute", "--git-path", "index"]);
    let o = super::run_capped(c, None, GIT_CALL, 1 << 20);
    if !o.ok() {
        return Err(format!(
            "the host's index could not be found: {}",
            o.err_text()
        ));
    }
    let index = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim());
    let mut lock = index.clone().into_os_string();
    lock.push(".lock");
    if std::fs::symlink_metadata(&lock).is_ok() {
        return Err(format!(
            "{} is held by another git; the unit's commits are not landed",
            PathBuf::from(lock).display()
        ));
    }
    let staged = index.with_file_name(format!("index.wicked-{id}"));
    let guard = StagedIndex {
        staged: staged.clone(),
        index,
        done: false,
    };
    let mut c = sync::git(tree);
    c.env("GIT_INDEX_FILE", &staged).args(["read-tree", new]);
    let o = super::run_capped(c, None, GIT_CALL, 1 << 20);
    if !o.ok() {
        return Err(format!(
            "the index of the unit's commits could not be built: {}",
            o.err_text()
        ));
    }
    Ok(guard)
}

/// The deletions of a tree root that are real (codex b2-seat #10): every tracked path, and an
/// untracked one only when the host's ignore rules do not match it.
pub(crate) fn tree_deletions(root: &Path, deletes: Vec<String>) -> Result<Vec<String>, String> {
    let tracked = host_tracked(root)?;
    let untracked: Vec<String> = deletes
        .iter()
        .filter(|d| !tracked.contains(*d))
        .cloned()
        .collect();
    if untracked.is_empty() {
        return Ok(deletes);
    }
    let ignored = host_ignored(root, &untracked)?;
    Ok(deletes
        .into_iter()
        .filter(|d| !ignored.contains(d))
        .collect())
}

/// The tracked paths of the host tree `root` (its index).
pub(crate) fn host_tracked(root: &Path) -> Result<HashSet<String>, String> {
    let mut c = sync::git(root);
    c.args(["ls-files", "-z", "-c"]);
    let r = super::run_capped(c, None, GIT_CALL, 256 << 20);
    if !r.ok() {
        return Err(format!("git ls-files failed: {}", r.err_text()));
    }
    Ok(r.stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect())
}

/// The links of a root's write-back the seat left exactly as the host gave them.
pub(crate) fn retained_links(entries: &[Entry], base: &HashMap<String, [u8; 32]>) -> Vec<Entry> {
    entries
        .iter()
        .filter(|e| e.kind == sync::Kind::Link)
        .filter(|e| base.get(&e.path) == Some(&sync::digest(e)))
        .cloned()
        .collect()
}

/// A link the seat kept unchanged must not come to resolve outside its root through what the
/// write-back changes around it (codex b2-sync #4: `b -> a/..` stays, `a` becomes `a -> .`). A
/// link that already resolved outside before the write-back is the host's own, left as it is.
pub(crate) fn check_retained_links(
    host: &Path,
    retained: &[Entry],
    writes: &[Entry],
    deletes: &[String],
    fence: &[String],
) -> Result<(), String> {
    if writes.is_empty() && deletes.is_empty() {
        return Ok(());
    }
    for l in retained {
        let mut after: Vec<Entry> = writes.to_vec();
        after.push(l.clone());
        if sync::links_stay_inside_final(Some(host), &after, deletes, fence).is_ok() {
            continue;
        }
        if sync::links_stay_inside_final(Some(host), std::slice::from_ref(l), &[], fence).is_ok() {
            return Err(format!(
                "`{}` would resolve outside its root once the unit's changes land",
                l.path
            ));
        }
    }
    Ok(())
}

/// Which of `paths` (relative to the tree `root`) the host's ignore rules match.
pub(crate) fn host_ignored(
    root: &Path,
    paths: &[String],
) -> Result<std::collections::HashSet<String>, String> {
    let mut c = sync::git(root);
    c.args(["check-ignore", "--no-index", "-z", "--stdin"]);
    let mut stdin = Vec::new();
    for p in paths {
        stdin.extend_from_slice(p.as_bytes());
        stdin.push(0);
    }
    let r = super::run_capped(c, Some(&stdin), Duration::from_secs(60), 64 << 20);
    // 0 = some ignored, 1 = none; anything else is an error.
    if !matches!(r.code, Some(0) | Some(1)) {
        return Err(format!("git check-ignore failed: {}", r.err_text()));
    }
    Ok(r.stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect())
}

/// One root's write-back: the root, the entries to write and the paths to delete.
pub(crate) type RootPlan<'a> = (&'a PlanRoot, Vec<Entry>, Vec<String>);

/// Every refusal the host's file system decides (masked paths by inode, links leaving their
/// root), for every root, before anything is applied (review r3: a refusal in a later root
/// must not leave an earlier root changed).
pub(crate) fn preflight(plans: &[RootPlan<'_>]) -> Result<(), String> {
    for (want, writes, deletes) in plans {
        let refused =
            |e: String| format!("the write-back of {} was refused: {e}", want.host.display());
        sync::guard_refusal(
            &want.host,
            &want.skip,
            writes
                .iter()
                .map(|e| e.path.as_str())
                .chain(deletes.iter().map(String::as_str)),
        )
        .map_err(refused)?;
        sync::links_stay_inside_final(Some(&want.host), writes, deletes, &want.skip)
            .map_err(refused)?;
        // A file or link replacing a host directory: everything under it must be a planned
        // deletion, or the apply would refuse halfway, after deleting (review r5).
        let gone: std::collections::HashSet<&str> = deletes.iter().map(String::as_str).collect();
        for e in writes {
            let at = want.host.join(&e.path);
            if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_dir()) {
                if let Some(kept) = sync::host_plain_listing(&at, &[])
                    .map_err(refused)?
                    .into_iter()
                    .map(|rel| format!("{}/{rel}", e.path))
                    .find(|p| !gone.contains(p.as_str()))
                {
                    return Err(refused(format!(
                        "`{}` replaces a directory that keeps `{kept}` (ignored or never seen \
                         by the seat)",
                        e.path
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Apply every root's plan, after [`preflight`] has passed for all of them.
pub(crate) fn apply_roots(plans: &[RootPlan<'_>]) -> Result<(), String> {
    preflight(plans)?;
    for (want, writes, deletes) in plans {
        sync::apply(&want.host, writes, deletes, &want.skip)
            .map_err(|e| format!("the write-back of {} failed: {e}", want.host.display()))?;
    }
    Ok(())
}

/// What the seat changed in one root against its copy-in `base` (pure over the host's current
/// state): the entries to write and the paths to delete. A path the host changed too since the
/// copy-in is a conflict (`Err`, naming it), unless both now hold the same content.
pub(crate) fn delta(
    host: &Path,
    base: &HashMap<String, [u8; 32]>,
    entries: Vec<Entry>,
) -> Result<(Vec<Entry>, Vec<String>), String> {
    // The host's content now; a file that is there but cannot be compared (unreadable, too
    // large, not a file) refuses the write-back, never reads as absent (codex b2-sync #9).
    let now = |rel: &str| -> Result<Option<[u8; 32]>, String> {
        Ok(sync::read_host_entry(host, rel)?.map(|e| sync::digest(&e)))
    };
    // A spelling change (`a` -> `A`, review r4): on a host that folds them, the new spelling
    // names the old file, so the old one is neither a conflict nor a deletion there; on a host
    // that does not, they are two files, as in the pod.
    let folded_base: HashMap<String, &[u8; 32]> =
        base.iter().map(|(p, d)| (sync::fold(p), d)).collect();
    let mut seen: Vec<String> = Vec::new();
    let mut writes = Vec::new();
    for e in entries {
        seen.push(e.path.clone());
        let d = sync::digest(&e);
        let cur = now(&e.path)?;
        let was = base.get(&e.path).or_else(|| {
            cur.as_ref()
                .and(folded_base.get(&sync::fold(&e.path)).copied())
        });
        if was == Some(&d) {
            continue; // unchanged by the seat
        }
        if cur == Some(d) {
            continue; // the host already holds it
        }
        if cur.as_ref() != was {
            return Err(format!(
                "`{}` changed on the host while the unit ran; the unit's version is not written over it",
                e.path
            ));
        }
        writes.push(e);
    }
    let same_file = |a: &str, b: &str| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            match (
                std::fs::symlink_metadata(host.join(a)),
                std::fs::symlink_metadata(host.join(b)),
            ) {
                (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
                _ => false,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (a, b);
            false
        }
    };
    let mut deletes = Vec::new();
    for (rel, was) in base {
        if seen.iter().any(|p| p == rel) {
            continue;
        }
        // Kept under another spelling the host folds to this very file.
        if seen
            .iter()
            .any(|p| sync::fold(p) == sync::fold(rel) && same_file(p, rel))
        {
            continue;
        }
        match now(rel)? {
            None => {}
            Some(cur) if &cur == was => deletes.push(rel.clone()),
            Some(_) => {
                return Err(format!(
                    "`{rel}` changed on the host while the unit deleted it; it is kept and the write-back refused"
                ))
            }
        }
    }
    deletes.sort();
    Ok((writes, deletes))
}

/// The unit's new commits: a bundle from the replica, verified and fetched with fsck into
/// `refs/wicked/k8s/<unit>`, refused unless it fast-forwards `base` and touches no masked or
/// `.git` path. `Ok(None)` = no new commit; `Ok(Some((commit, ref)))` = ready to land.
fn fetch_commits(
    seat: &Seat,
    base: &str,
    masks: &[String],
) -> Result<Option<(String, String)>, String> {
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
        return Ok(None);
    }
    // A private dir made new for this bundle and a file created exclusively in it, both owner
    // only (codex b2-seat #2): no planted link is followed and the history is nobody else's.
    let dir = private_temp_dir("wicked-unit-bundle")?;
    let file = dir.0.join("unit.bundle");
    {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut f = o.open(&file).map_err(|e| e.to_string())?;
        std::io::Write::write_all(&mut f, &r.stdout).map_err(|e| e.to_string())?;
    }
    // Bounded in time and output (review r5): the history is the seat's, and a small bundle can
    // make `git log` print without end.
    let git = |args: &[&str]| -> Result<String, String> {
        let mut c = sync::git(&seat.tree);
        c.args(args);
        let r = super::run_capped(c, None, Duration::from_secs(300), 64 << 20);
        if r.ok() {
            Ok(String::from_utf8_lossy(&r.stdout).trim().to_string())
        } else {
            Err(format!(
                "git {}: {}",
                args.iter().find(|a| !a.starts_with('-')).unwrap_or(&""),
                r.err_text()
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
    if !ff {
        return Err(format!(
            "the unit's branch is not a fast-forward of {base} (its history was rewritten); its commits are kept at {refname}"
        ));
    }
    // Every imported commit's own changes, merges included (review r2: a masked change undone
    // by a later commit is absent from the endpoint diff).
    let changed = git(&[
        "log",
        "--format=",
        "--name-only",
        "--no-renames",
        "-m",
        "-z",
        &format!("{base}..{new}"),
    ])?;
    if let Some(p) = changed.split('\0').filter(|p| !p.is_empty()).find(|p| {
        p.split('/').any(sync::is_git_name)
                || masks.iter().any(|m| sync::under(p, m))
                // The run's dependency tree is the engine's (codex b3-floor #6): a commit may
                // not track it, as no file write of it comes back.
                || (seat.deps && in_node_modules(p))
    }) {
        return Err(format!(
            "the unit's commits change `{p}`, a path the seat may not write; they are kept at {refname}"
        ));
    }
    // The same paths as the host's file system resolves them (review r3): a commit-only
    // `.ſsh/k` writes no file, so only this check sees that it names masked `.ssh/k` on APFS.
    sync::guard_refusal(
        &seat.tree,
        masks,
        changed.split('\0').filter(|p| !p.is_empty()),
    )
    .map_err(|e| format!("the unit's commits were refused: {e}; they are kept at {refname}"))?;
    Ok(Some((new, refname)))
}

/// Whether a tree path lies in a `node_modules` (any depth; folded as the host compares).
pub(crate) fn in_node_modules(rel: &str) -> bool {
    rel.split('/').any(|c| sync::fold(c) == "node_modules")
}

/// The in-pod script that unpacks the fill's nested `node_modules` archive into `tree` (pure).
pub(crate) fn nested_unpack_script(tree: &Path) -> String {
    let t = format!("'{}'", tree.to_string_lossy().replace('\'', "'\\''"));
    // Every nested node_modules already in the tree goes first (review r1: an ignored one the
    // replica kept from an earlier seat could carry a planted `.bin` shim), as `npm ci` empties
    // them; the root one is the read-only claim and is left alone.
    format!(
        "set -e; cd {t}; F=/floor/busybox; $F find . -path ./node_modules -prune -o -name node_modules -prune -exec $F rm -rf {{}} \\; ; [ -z \"$($F find . -path ./node_modules -prune -o -name node_modules -print)\" ] || {{ echo 'wicked-seat: an earlier node_modules could not be removed' >&2; exit 1; }}; \
         a=./node_modules/{n}; [ -f \"$a\" ] || exit 0; $F tar -x -f \"$a\"",
        n = crate::checks_container::NESTED_TAR
    )
}

/// The `bundle` base that says the host tree has no commit: the pod codec answers nothing when
/// the replica has none either, and refuses when the seat made one.
pub(crate) const NO_BASE: &str = "-";

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
    if let Ok(mut g) = SEEDED.lock() {
        if let Some(set) = g.as_mut() {
            let prefix = seeded_key(&ns, &format!("{scope}/"));
            set.retain(|k| !k.starts_with(&prefix));
        }
    }

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
        branch: None,
        baseline: Mutex::new(Baseline::new()),
        seat_dirs: Vec::new(),
        scratch: Vec::new(),
        mounted: vec![tree.to_path_buf()],
        unit_label: "u".into(),
        settled: Mutex::new(false),
        deps: false,
    })
}
