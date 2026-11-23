//! A governed seat on a pod (PARITY B-2): the replica copy-in, the seat's Sandbox and its
//! bound-pod gate, the unit's env over exec stdin, the CLI over `kubectl exec -i`, the exit code
//! read back from the pod, the reap and the checked write-back.
//!
//! **The runner contract holds.** The seat is still a child process of the engine (the kubectl
//! client): stdio streams, the deadline and the cancel kill are the existing spawn path's. A
//! pod outlives a killed client. A2/A12 require deletion and confirmed pod termination before
//! replica write-back. In container mode, repo-less, ungoverned `validator-` calls with session
//! `validator`, unit `validator-agent`, and a non-empty parent and assigned seat may reuse a pod
//! keyed by (parent run id, exact judge seat). This includes rule-check and agent-gate judges;
//! reprobes and all other sessions stay cold. A busy or reaping slot falls back to a cold call.
//! Exclusive checkout, physical UID/owner/fingerprint/lifetime checks and a verified SIGKILL/
//! quiescence barrier permit read-only (no-op) write-back and warm return. Any doubt or
//! abandonment reaps; cold calls confirm termination before replica write-back.
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
///
/// A claude seat's credential record ([`claude_record_vars`], wicked-garden#1365) lives in its
/// config home only while the launcher runs. Before it, the record path and its temporary name
/// are removed (whatever a seat or an interrupted unit left there, a planted link or directory
/// included), and an OAuth seat's record is written under the temporary name with no-clobber,
/// then renamed in place. After the launcher exits, whatever its exit code, both names are
/// removed again; a removal that fails makes the unit fail (exit 125). The record is written by
/// the shell's builtin `printf`, never an exec, so it is on no process's argv (node exec auditing
/// records argv). An API-key claude seat is handed the path alone, so its units only clean. The
/// carrier variables are unset before the launcher (and landrun passes no variable it is not
/// named), so the record never reaches the host or the replica codec. A unit that times out, is
/// cancelled or loses its exec stream never reaches the wrapper's cleanup: [`settle_unless`]
/// (or [`reap`], for a unit abandoned before it settles) removes the record in the pod, best
/// effort and bounded, before deleting it. Residual: a pod or daemon that dies before the unit
/// settles (eviction, node loss, a daemon crash), or a removal that fails, leaves the record in
/// that seat's replica until the next unit of that run and posture removes it, or the run's
/// teardown.
pub(crate) const WRAPPER: &str = "set -u; u=/wicked-unit; \
[ -f $u/env ] || { echo 'wicked-seat: the unit env is missing' >&2; echo 125 > $u/rc; exit 125; }; \
. $u/env; /floor/busybox rm -f $u/env; \
[ $# -gt 0 ] || { echo 'wicked-seat: the unit argv is missing' >&2; echo 125 > $u/rc; exit 125; }; \
cd \"$WICKED_UNIT_CWD\" || { echo 'wicked-seat: the unit directory is missing' >&2; echo 125 > $u/rc; exit 125; }; \
unset WICKED_UNIT_CWD; \
c=${WICKED_CLAUDE_RECORD_FILE:-}; r=${WICKED_CLAUDE_RECORD:-}; unset WICKED_CLAUDE_RECORD WICKED_CLAUDE_RECORD_FILE; \
drop() { /floor/busybox rm -rf \"$c\" \"$c.wicked-tmp\"; }; \
[ -z \"$c\" ] || { drop && { [ -z \"$r\" ] || { (umask 077; set -C; printf '%s' \"$r\" > \"$c.wicked-tmp\") && /floor/busybox mv -f \"$c.wicked-tmp\" \"$c\"; }; }; } || { echo 'wicked-seat: the claude credential record could not be written' >&2; drop; echo 125 > $u/rc; exit 125; }; \
unset r; \
\"$@\"; rc=$?; \
[ -z \"$c\" ] || drop || { echo 'wicked-seat: the claude credential record could not be removed' >&2; rc=125; }; \
echo $rc > $u/rc.tmp && /floor/busybox mv $u/rc.tmp $u/rc; exit $rc";

/// The carrier variables that hand [`WRAPPER`] a claude seat's credential record
/// (wicked-garden#1365): claude 2.1.296 deletes `CLAUDE_CODE_OAUTH_TOKEN` from every tool child's
/// env, so a `claude -p` the seat starts from its Bash tool (a harness's rule model) reads its
/// sign-in from `$CLAUDE_CONFIG_DIR/.credentials.json`. The record is claude's own env-token
/// shape ([`crate::seat_credentials::claude_oauth_token_record`]). Every claude seat with a config
/// home is handed the path, so the wrapper cleans it for every unit; only a seat signed in by a
/// token is handed a record. Empty for every other CLI. A gateway placeholder cannot stand in for
/// the record: claude's own non-inference calls go straight to api.anthropic.com, past the
/// gateway (core#1832).
pub(crate) fn claude_record_vars(cli: &str, vars: &[(String, String)]) -> Vec<(String, String)> {
    if wicked_apps_core::spawn::SeatCli::from_binary(cli)
        != wicked_apps_core::spawn::SeatCli::Claude
    {
        return Vec::new();
    }
    let get = |name: &str| {
        vars.iter()
            .find(|(k, v)| k == name && !v.is_empty())
            .map(|(_, v)| v.as_str())
    };
    let Some(home) = get("CLAUDE_CONFIG_DIR") else {
        return Vec::new();
    };
    let mut out = vec![(
        "WICKED_CLAUDE_RECORD_FILE".to_string(),
        format!("{}/.credentials.json", home.trim_end_matches('/')),
    )];
    if let Some(token) = get("CLAUDE_CODE_OAUTH_TOKEN") {
        out.push((
            "WICKED_CLAUDE_RECORD".into(),
            crate::seat_credentials::claude_oauth_token_record(token),
        ));
    }
    out
}

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

// (core#1463) Sign the listing metadata once, without opening its files.
#[cfg(unix)]
fn signature_field(h: &mut sha2::Sha256, bytes: &[u8]) {
    use sha2::Digest;
    h.update((bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

#[cfg(unix)]
fn signature_metadata(h: &mut sha2::Sha256, m: &std::fs::Metadata) {
    use sha2::Digest;
    use std::os::unix::fs::MetadataExt;
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

#[cfg(unix)]
fn generation_signature(r: &PlanRoot, h: &mut sha2::Sha256) -> Option<()> {
    let snapshots = r.host.parent()?;
    let skills = snapshots.parent()?;
    if snapshots.file_name()? != "snapshots" || skills.file_name()? != "skills" {
        return None;
    }
    // (core#1441) Only the published, link-free generation shape has immutable identity.
    let root = std::fs::symlink_metadata(&r.host).ok()?;
    if !root.is_dir() {
        return None;
    }
    for p in [snapshots, skills] {
        if !std::fs::symlink_metadata(p).ok()?.is_dir() {
            return None;
        }
    }
    let index = r.host.join("snapshot.json");
    let meta = std::fs::symlink_metadata(&index).ok()?;
    if !meta.is_file() {
        return None;
    }
    let bytes = std::fs::read(index).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let gen = match &value["gen"] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    if gen != r.host.file_name()?.to_str()? || value["contentHash"].as_str()?.is_empty() {
        return None;
    }
    signature_field(h, b"skills-generation");
    signature_field(h, r.host.as_os_str().as_encoded_bytes());
    signature_field(h, &bytes);
    use sha2::Digest;
    use std::os::unix::fs::MetadataExt;
    for m in [&root, &meta] {
        for n in [
            m.ino(),
            m.mtime() as u64,
            m.mtime_nsec() as u64,
            m.ctime() as u64,
            m.ctime_nsec() as u64,
        ] {
            h.update(n.to_le_bytes());
        }
    }
    Some(())
}

#[cfg(unix)]
fn root_digest(r: &PlanRoot, prefix: &[u8]) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(prefix);
    for s in &r.skip {
        signature_field(&mut h, s.as_bytes());
    }
    h.update([0xff]);
    for s in &r.masks {
        signature_field(&mut h, s.as_bytes());
    }
    h.update([0xff]);
    if !r.repo && generation_signature(r, &mut h).is_some() {
        return Some(h.finalize().into());
    }
    #[cfg(test)]
    SIG_WALKS.with(|c| c.set(c.get() + 1));
    let mut listed = if r.repo {
        sync::host_repo_listing(&r.host).ok()?
    } else {
        sync::host_plain_listing(&r.host, &r.skip).ok()?
    };
    let mut dirs: Vec<String> = listed
        .iter()
        .flat_map(|rel| {
            let parts: Vec<&str> = rel.split('/').collect();
            (0..parts.len()).map(move |i| parts[..i].join("/"))
        })
        .collect();
    // The root and every directory on the way too (codex r2 #1): a directory moved out and back
    // changes their metadata, not its files'.
    dirs.push(String::new());
    dirs.sort();
    dirs.dedup();
    listed.sort();
    for rel in dirs.iter().chain(&listed) {
        let m = std::fs::symlink_metadata(r.host.join(rel)).ok()?;
        signature_field(&mut h, rel.as_bytes());
        signature_metadata(&mut h, &m);
    }
    Some(h.finalize().into())
}

/// A read-only root's host signature (no file is opened: a link-free walk and one `lstat` per
/// entry, its kind, size, mode, inode, mtime and ctime, or a published skills generation's
/// identity), with the root's skip list and masks. `None` when it cannot be taken: the root is
/// sent.
#[cfg(unix)]
fn root_signature(r: &PlanRoot) -> Option<[u8; 32]> {
    root_digest(r, &[])
}

#[cfg(not(unix))]
fn root_signature(_r: &PlanRoot) -> Option<[u8; 32]> {
    None
}

/// (core#1463) A read-only repository root's stamp key: its signature under the process secret
/// and its replica path. A replica stamp with this key holds exactly this state of the root.
#[cfg(unix)]
pub(crate) fn repo_key(r: &PlanRoot, replica: &str, secret: &[u8; 32]) -> Option<String> {
    let mut prefix = secret.to_vec();
    prefix.extend_from_slice(&(replica.len() as u64).to_le_bytes());
    prefix.extend_from_slice(replica.as_bytes());
    root_digest(r, &prefix).map(|digest| digest.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(unix)]
fn current_repo_key(r: &PlanRoot, replica: &str) -> Result<Option<String>, String> {
    Ok(repo_key(r, replica, stamp_secret()?))
}

#[cfg(not(unix))]
fn current_repo_key(_r: &PlanRoot, _replica: &str) -> Result<Option<String>, String> {
    Ok(None)
}

#[cfg(unix)]
fn stamp_secret() -> Result<&'static [u8; 32], String> {
    static SECRET: std::sync::OnceLock<Result<[u8; 32], String>> = std::sync::OnceLock::new();
    SECRET
        .get_or_init(|| {
            // (core#1463) Process-private randomness makes a seat's guessed stamp useless.
            use std::io::Read;
            let mut bytes = [0; 32];
            std::fs::File::open("/dev/urandom")
                .and_then(|mut f| f.read_exact(&mut bytes))
                .map_err(|e| format!("the replica stamp secret could not be read: {e}"))?;
            Ok(bytes)
        })
        .as_ref()
        .map_err(Clone::clone)
}

#[derive(Default)]
pub(crate) struct RepoStamps {
    keys: HashMap<String, String>,
    pub(crate) have: HashMap<String, String>,
    writable: Vec<String>,
}

impl RepoStamps {
    pub(crate) fn new(scope: Option<&str>, roots: &[PlanRoot]) -> Result<Self, String> {
        let mut result = Self::default();
        for r in roots.iter().filter(|r| r.repo) {
            let rep = replica_of(scope, &r.host)?;
            if let Some(key) = current_repo_key(r, &rep)? {
                result.keys.insert(rep, key);
            }
        }
        Ok(result)
    }

    pub(crate) fn block_writers(&mut self, ns: &str) {
        let seats = SEATS.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(seats) = seats.as_ref() {
            for seat in seats.values().filter(|s| s.ns == ns) {
                for p in &seat.writable {
                    if let Ok(rep) = replica_of(seat.scope.as_deref(), p) {
                        self.writable.push(rep);
                    }
                }
            }
        }
    }

    fn blocked(&self, replica: &str) -> bool {
        self.writable
            .iter()
            .any(|p| sync::under(p, replica) || sync::under(replica, p))
    }

    fn kept(&self, replica: &str) -> bool {
        self.keys
            .get(replica)
            .is_some_and(|key| self.have.get(replica) == Some(key))
            && !self.blocked(replica)
    }
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
            .map(|e| e.data.capacity() + e.path.capacity() + ENTRY_OVERHEAD)
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

/// What a kept entry costs beyond its bytes (codex r2 #3: many empty files are not free).
const ENTRY_OVERHEAD: usize = 64;

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
    /// A read-only repository (a plain root bound read-only, with its own `.git`): copied as what
    /// git lists, never its ignored files, never from the read cache (fix-k8s-chat: a chat's "All
    /// repos" read roots, crew#1034, were copied whole, build output and run worktrees included).
    pub(crate) repo: bool,
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
        let repo =
            mode == Mode::Plain && !*rw && std::fs::symlink_metadata(path.join(".git")).is_ok();
        roots.push(PlanRoot {
            host: path.clone(),
            mode,
            skip,
            back,
            files: Vec::new(),
            masks: root_masks,
            repo,
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
            repo: false,
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

/// A dependency-backed tree excludes every node_modules component in the codec itself.
pub(crate) fn pack_plan(roots: &[(String, &PlanRoot)], deps_tree: Option<&Path>) -> Value {
    let mut plan = codec_plan(roots);
    for (item, (_, root)) in plan
        .as_array_mut()
        .expect("plan array")
        .iter_mut()
        .zip(roots)
    {
        item.as_array_mut()
            .expect("root array")
            .push(serde_json::json!(
                root.mode == Mode::Tree && deps_tree == Some(root.host.as_path())
            ));
    }
    plan
}

/// The replica path of a host path under an optional scope.
pub(crate) fn replica_of(scope: Option<&str>, p: &Path) -> Result<String, String> {
    let replica = render::scoped_sub_path(scope, p)
        .ok_or_else(|| format!("{} cannot be a replica path", p.display()))?;
    // (core#1463) Stamp storage is never exposed through a seat's replica mount.
    if sync::under(&replica, ".wicked-stamps") {
        return Err(format!("{} overlaps the replica stamp store", p.display()));
    }
    Ok(replica)
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

/// What std's unix `Command` stores for an argument holding a NUL (`get_args` reads it back).
const STD_NUL_ARG_PLACEHOLDER: &str = "<string-with-nul>";

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

/// Extract only the identity from the explicit seat-owned copy, without resolving includes or
/// consulting the operator's config. A bad copy must never prevent a unit from running.
fn git_identity_config(path: &Path) -> Result<Option<String>, ()> {
    use std::io::Read;
    use wicked_apps_core::HardenedCommand as _;

    const LIMIT: u64 = 64 * 1024;
    let meta = std::fs::symlink_metadata(path).map_err(|_| ())?;
    if !path.is_absolute() || !meta.is_file() || meta.len() > LIMIT {
        return Err(());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| ())?
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() as u64 > LIMIT || bytes.contains(&0) {
        return Err(());
    }
    let mut content = String::from("[user]\n");
    for key in ["name", "email"] {
        let mut cmd = Command::new("git");
        cmd.hardened()
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .args(["config", "--no-includes", "--file"])
            .arg(path)
            .args(["--null", "--get", &format!("user.{key}")]);
        let out = super::run_capped(cmd, None, Duration::from_secs(10), LIMIT);
        if out.code == Some(1) && out.stdout.is_empty() {
            continue; // A missing key is allowed; git still parsed the file.
        }
        if !out.ok() {
            return Err(());
        }
        let value = std::str::from_utf8(&out.stdout).map_err(|_| ())?;
        let value = value.strip_suffix('\0').ok_or(())?;
        if value.contains(['\n', '\0']) {
            return Err(());
        }
        let value = value.replace('\\', "\\\\").replace('"', "\\\"");
        content.push_str(&format!("\t{key} = \"{value}\"\n"));
    }
    Ok((content != "[user]\n").then_some(content))
}

/// Carry only spawn hardening's seat-owned user.name/user.email when the global config's
/// host path is absent from the replica mounts. No system config or other global settings
/// are shipped. The prelude rides the unit env over stdin; the identity file outlives that env
/// file in the pod's per-unit memory volume, written as the seat uid with mode 0600. It needs
/// an individual Landlock read rule (the rest of `/wicked-unit` stays inaccessible).
pub(crate) fn git_config_handoff(
    vars: &mut [(String, String)],
    mounted: &[PathBuf],
    unit_dir: &Path,
) -> (String, Vec<String>) {
    for (key, value) in vars {
        if key != "GIT_CONFIG_GLOBAL" {
            continue;
        }
        let path = Path::new(value);
        // Empty disables the file; /dev/null already exists and is admitted in the pod.
        if path.as_os_str().is_empty()
            || path == Path::new("/dev/null")
            || mounted.iter().any(|root| path.starts_with(root))
        {
            continue;
        }
        let content = match git_identity_config(path) {
            Ok(Some(content)) => content,
            Ok(None) => continue,
            Err(()) => {
                // One engine-written line, with no file contents, paths, or git diagnostics.
                eprintln!("wicked-core: the seat git identity could not be read");
                continue;
            }
        };
        let dest = unit_dir
            .join("gitconfig-global")
            .to_string_lossy()
            .into_owned();
        let prelude = format!(
            "(umask 077; printf '%s' '{}' > '{dest}.tmp' && \
             /floor/busybox mv '{dest}.tmp' '{dest}') || {{ \
             /floor/busybox rm -rf '{dest}.tmp' '{dest}' || :; \
             export GIT_CONFIG_GLOBAL=/dev/null; \
             echo 'wicked-seat: the git identity could not be written' >&2; }}\n",
            content.replace('\'', "'\\''"),
            dest = dest.replace('\'', "'\\''")
        );
        *value = dest.clone();
        return (prelude, vec![dest]);
    }
    (String::new(), Vec::new())
}

/// The exit code of a seat exec: the pod's rc file is the evidence, kubectl's own code only
/// when it agrees (pure). `None` = the stream was lost (no rc, or a disagreement).
pub(crate) fn exit_code(kubectl: i32, rc_file: Option<&str>) -> Option<i32> {
    let rc: i32 = rc_file?.trim().parse().ok()?;
    (rc == kubectl).then_some(rc)
}

/// A physical judge belongs to exactly this parent and assigned seat (including `#instance`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct WarmKey {
    pub(crate) parent: String,
    seat: String,
}

impl WarmKey {
    pub(crate) fn new(parent: &str, seat: &str) -> Self {
        Self {
            parent: parent.into(),
            seat: seat.into(),
        }
    }

    pub(crate) fn scope(&self) -> String {
        use sha2::{Digest, Sha256};
        // Length framing prevents ambiguous parent/seat concatenations.
        let mut h = Sha256::new();
        h.update((self.parent.len() as u64).to_le_bytes());
        h.update(self.parent.as_bytes());
        h.update(self.seat.as_bytes());
        format!("warm-{:x}", h.finalize())
    }
}

#[derive(Debug, Default)]
struct WarmState {
    busy: bool,
    retired: bool,
    reaping: bool,
    pod: Option<PhysicalJudge>,
    host_dirs: Vec<PathBuf>,
    replica: Option<(Kube, String, String)>,
}

#[derive(Debug, Default)]
struct WarmSlot {
    state: Mutex<WarmState>,
}

type WarmCache = HashMap<WarmKey, Arc<WarmSlot>>;

static WARM: Mutex<Option<WarmCache>> = Mutex::new(None);

/// Held from before host configuration is prepared until after the gateway lease is released.
/// The slot contains only physical state; no execution, gateway key, credential or conversation.
#[derive(Debug)]
pub(crate) struct WarmCheckout {
    key: WarmKey,
    slot: Arc<WarmSlot>,
    epoch: u64,
    generation: u64,
    completed: std::sync::atomic::AtomicBool,
}

impl WarmCheckout {
    pub(crate) fn live(&self) -> bool {
        !super::run_ended_since(&self.key.parent, self.epoch)
            && super::lifecycle(&self.key.parent) == self.generation
            && super::admit_warm(&self.key.parent, self.generation)
    }

    /// Host directories are private engine output. Start each batch with fresh trusted seeds.
    pub(crate) fn scope(&self) -> String {
        self.key.scope()
    }

    pub(crate) fn fresh_dir(&self, path: &Path) -> Result<(), String> {
        let mut state = self.slot.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.retired || !self.live() {
            return Err("the judge's parent ended".into());
        }
        if !state.host_dirs.iter().any(|p| p == path) {
            state.host_dirs.push(path.to_path_buf());
        }
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => {
                let pod = state.pod.take();
                drop(state);
                if let Some(pod) = pod {
                    pod.reap();
                }
                Err(format!(
                    "cannot reset judge directory {}: {e}",
                    path.display()
                ))
            }
        }
    }
}

impl Drop for WarmCheckout {
    fn drop(&mut self) {
        let mut state = self.slot.state.lock().unwrap_or_else(|p| p.into_inner());
        let abandoned = if !self.completed.load(std::sync::atomic::Ordering::SeqCst) {
            state.pod.take()
        } else {
            None
        };
        // Keep the key exclusive through an abandonment's deletion.
        if let Some(pod) = abandoned {
            drop(state);
            pod.reap();
            state = self.slot.state.lock().unwrap_or_else(|p| p.into_inner());
        }
        state.busy = false;
        if state.retired && !state.reaping && clean_warm_dirs(&mut state) {
            forget_warm_slot(&self.key, &self.slot);
        }
    }
}

fn forget_warm_slot(key: &WarmKey, slot: &Arc<WarmSlot>) {
    if let Some(cache) = WARM.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        if cache.get(key).is_some_and(|have| Arc::ptr_eq(have, slot)) {
            cache.remove(key);
        }
    }
}

fn clean_warm_dirs(state: &mut WarmState) -> bool {
    if let Some((kube, ns, _)) = &state.replica {
        if let Err((_, e)) = clear_stale(kube, ns) {
            eprintln!("wicked-core: warm judge scope cleanup deferred: {e}");
            return false;
        }
    }
    if let Some((kube, ns, scope)) = &state.replica {
        if let Err(e) = drop_replica_scope(kube, ns, scope) {
            eprintln!("wicked-core: warm judge cleanup deferred: {e}");
            return false;
        }
        if let Ok(mut g) = SEEDED.lock() {
            if let Some(set) = g.as_mut() {
                let prefix = seeded_key(ns, &format!("{scope}/"));
                set.retain(|k| !k.starts_with(&prefix));
            }
        }
    }
    state.replica = None;
    for path in state.host_dirs.drain(..) {
        let _ = std::fs::remove_dir_all(path);
    }
    true
}

/// Busy or reaping slots fall back to cold immediately; ended parents are refused.
pub(crate) fn checkout_warm(key: WarmKey) -> Result<Option<Arc<WarmCheckout>>, String> {
    let generation = super::lifecycle(&key.parent);
    let epoch = super::run_epoch(&key.parent);
    let slot = {
        let mut cache = WARM.lock().unwrap_or_else(|p| p.into_inner());
        if !super::admit_warm(&key.parent, generation) {
            return Err("the judge's parent ended before checkout".into());
        }
        cache
            .get_or_insert_with(HashMap::new)
            .entry(key.clone())
            .or_default()
            .clone()
    };
    // Reset and retirement can hold the state lock during I/O. Never queue a judge behind them.
    let mut state = match slot.state.try_lock() {
        Ok(state) => state,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            return if super::admit_warm(&key.parent, generation) {
                Ok(None)
            } else {
                Err("the judge's parent ended before checkout".into())
            };
        }
    };
    if state.retired || !super::admit_warm(&key.parent, generation) {
        return Err("the judge's parent ended before checkout".into());
    }
    if state.busy || state.reaping {
        return Ok(None);
    }
    state.busy = true;
    drop(state);
    Ok(Some(Arc::new(WarmCheckout {
        key,
        slot,
        epoch,
        generation,
        completed: std::sync::atomic::AtomicBool::new(false),
    })))
}

/// Only this immutable identity and mount contract may be carried across calls.
#[derive(Debug)]
struct PhysicalJudge {
    kube: Kube,
    ns: String,
    sandbox: String,
    unit: String,
    uid: String,
    owner_uid: String,
    fingerprint: Value,
    pod_spec: Value,
    pod_labels: Value,
    sandbox_spec: Value,
    expires: Instant,
    wall_expires: std::time::SystemTime,
}

impl PhysicalJudge {
    fn delete(&self) -> Result<(), String> {
        delete_seat_pod(&self.kube, &self.ns, Some(&self.sandbox), &self.sandbox)
    }

    fn reap(&self) {
        if let Err(e) = self.delete() {
            eprintln!("wicked-core: warm judge {} not reaped: {e}", self.sandbox);
            UNREAPED.lock().unwrap_or_else(|p| p.into_inner()).push((
                self.ns.clone(),
                self.sandbox.clone(),
                self.sandbox.clone(),
            ));
        }
    }

    fn verify(&self, timeout: Duration) -> Result<(), String> {
        let needed = timeout + Duration::from_secs(60);
        if self.expires.saturating_duration_since(Instant::now()) < needed
            || self
                .wall_expires
                .duration_since(std::time::SystemTime::now())
                .unwrap_or_default()
                < needed
        {
            return Err("insufficient physical pod lifetime".into());
        }
        let pod = self
            .kube
            .get_json(Some(&self.ns), "pod", &self.sandbox)?
            .ok_or("the cached judge pod disappeared")?;
        let owner = pod
            .pointer("/metadata/ownerReferences")
            .and_then(Value::as_array)
            .and_then(|owners| {
                owners
                    .iter()
                    .find(|o| o["kind"] == "Sandbox" && o["name"] == self.sandbox)
            })
            .and_then(|o| o["uid"].as_str());
        let sandbox = self
            .kube
            .get_json(Some(&self.ns), "sandboxes.agents.x-k8s.io", &self.sandbox)?
            .ok_or("the cached judge Sandbox disappeared")?;
        let sandbox_uid = sandbox.pointer("/metadata/uid").and_then(Value::as_str);
        if sandbox_uid != Some(self.owner_uid.as_str())
            || !super::is_owned_by(&sandbox, &self.kube.owner)
            || pod.pointer("/metadata/uid").and_then(Value::as_str) != Some(self.uid.as_str())
            || owner != Some(self.owner_uid.as_str())
            || !super::is_owned_by(&pod, &self.kube.owner)
            || pod_state(&pod, &self.sandbox, &self.unit) != PodState::Ready
            || pod.get("spec") != Some(&self.pod_spec)
            || pod.pointer("/metadata/labels") != Some(&self.pod_labels)
            || sandbox.get("spec") != Some(&self.sandbox_spec)
        {
            return Err(
                "the cached judge's UID, owner, readiness or pod specification changed".into(),
            );
        }
        Ok(())
    }

    fn quiet(&self) -> Result<(), String> {
        self.verify(Duration::ZERO)?;
        warm_exec(self, WARM_QUIESCE, "wicked-warm-quiet")?;
        self.verify(Duration::ZERO)
    }
}

#[derive(Debug)]
struct WarmExecution {
    checkout: Arc<WarmCheckout>,
    physical: PhysicalJudge,
}

/// SIGKILL spares PID 1 and this ash (kill is an ash builtin). Poll the entire PID namespace,
/// including detached children. Only the trusted PID 1's busybox sleep may coexist with us.
/// Missing proc evidence or a surviving process fails closed. The exact stdout is a receipt;
/// an empty successful kubectl response is not proof.
pub(crate) const WARM_QUIESCE: &str = r#"set -eu
F=/floor/busybox
[ -r /proc/1/stat ] && [ -r /proc/$$/stat ] || exit 1
[ "$($F id -u)" = 1000 ] || exit 1
kill -9 -1 2>/dev/null || :
n=0
while [ "$n" -lt 40 ]; do
    quiet=1
    for d in /proc/[0-9]*; do
        p=${d##*/}
        [ "$p" = 1 ] && continue
        [ "$p" = "$$" ] && continue
        [ -d "$d" ] || continue
        if ! read -r pid comm state ppid rest < "$d/stat"; then
            [ ! -d "$d" ] && continue
            exit 1
        fi
        if [ "$ppid" = 1 ] && [ "$state" = S ] &&
           [ "$($F readlink "$d/exe" 2>/dev/null)" = /floor/busybox ] &&
           [ "$($F tr '\000' ' ' < "$d/cmdline")" = '/floor/busybox sleep 2 ' ]; then
            continue
        fi
        quiet=0
    done
    if [ "$quiet" = 1 ]; then echo wicked-warm-quiet; exit 0; fi
    n=$((n + 1)); $F sleep 0.05
done
exit 1"#;

/// API-server defaults may add fields, but cannot replace a requested field or add containers,
/// mounts or privileges inside an explicitly rendered list.
fn pod_contract_matches(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(want), Value::Object(have)) => want
            .iter()
            .all(|(k, v)| have.get(k).is_some_and(|h| pod_contract_matches(v, h))),
        (Value::Array(want), Value::Array(have)) => {
            want.len() == have.len()
                && want
                    .iter()
                    .zip(have)
                    .all(|(w, h)| pod_contract_matches(w, h))
        }
        _ => expected == actual,
    }
}

fn shell_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// Remove every SysV IPC object of the pod's IPC namespace (busybox `ipcrm` has no `-a`), then
/// prove all three tables are empty. Fails closed when /proc/sysvipc cannot be read.
pub(crate) const WARM_IPC_CLEAR: &str = r#"for t in shm:m sem:s msg:q; do
    f=/proc/sysvipc/${t%%:*}
    o=${t##*:}
    [ -r "$f" ] || exit 1
    $F tail -n +2 "$f" | while read -r key id rest; do $F ipcrm -"$o" "$id" || exit 1; done
done
for f in shm sem msg; do
    [ "$($F tail -n +2 /proc/sysvipc/$f | $F wc -l)" -eq 0 ] || exit 1
done
"#;

/// Clear disposable contents, preserving separately mounted roots (including beneath /tmp).
/// Traverse only ancestors of a protected mount; rm never follows a seat-planted symlink.
pub(crate) fn warm_reset_script(tree: &Path, mounts: &[PathBuf], homes: &[PathBuf]) -> String {
    let protected = mounts
        .iter()
        .map(|p| shell_path(p))
        .collect::<Vec<_>>()
        .join(" ");
    let mut script = format!(
        r#"set -eu
F=/floor/busybox
wipe() {{
    dir=$1
    [ -d "$dir" ] && [ ! -L "$dir" ] || return 1
    for p in "$dir"/* "$dir"/.[!.]* "$dir"/..?*; do
        [ -e "$p" ] || [ -L "$p" ] || continue
        keep=0; descend=0
        for m in {protected}; do
            [ "$p" = "$m" ] && keep=1
            case "$m" in "$p"/*) descend=1;; esac
        done
        [ "$keep" = 1 ] && continue
        if [ "$descend" = 1 ]; then (wipe "$p") || return 1
        else $F rm -rf -- "$p" || return 1
            [ ! -e "$p" ] && [ ! -L "$p" ] || return 1
        fi
    done
}}
wipe /home/seat
wipe /tmp
wipe /dev/shm
wipe /wicked-unit
wipe /wicked-canary
{WARM_IPC_CLEAR}"#
    );
    for home in homes {
        script.push_str(&format!("wipe {}\n", shell_path(home)));
    }
    script.push_str(&format!(
        "wipe {}\ncd /wicked-ro\nset -o pipefail\n$F tar -cf -",
        shell_path(tree)
    ));
    for mount in mounts
        .iter()
        .filter(|p| p.as_path() != tree && p.starts_with(tree))
    {
        if let Ok(rel) = mount.strip_prefix(tree) {
            script.push_str(&format!(
                " --exclude={}",
                shell_path(&Path::new(".").join(rel))
            ));
        }
    }
    script.push_str(&format!(
        " . | $F tar -C {} -xf -\necho wicked-warm-reset",
        shell_path(tree)
    ));
    script
}

fn warm_exec(pod: &PhysicalJudge, script: &str, receipt: &str) -> Result<(), String> {
    let r = pod.kube.call(
        Some(&pod.ns),
        &[
            "exec",
            "-c",
            "seat",
            &pod.sandbox,
            "--",
            "/floor/busybox",
            "sh",
            "-c",
            script,
        ],
        None,
        CALL,
    );
    if r.ok() && String::from_utf8_lossy(&r.stdout).trim() == receipt {
        Ok(())
    } else {
        Err(format!(
            "judge process barrier/reset was not proved: {}",
            r.err_text()
        ))
    }
}

/// Off the actor thread, before namespace release. Retiring slots never wait for an active
/// execution: parent cancellation stops it, its guards reap it, and its checkout removes homes.
pub(crate) fn reap_warm_parent(parent: &str) {
    let slots: Vec<(WarmKey, Arc<WarmSlot>)> = WARM
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|cache| {
            cache
                .iter()
                .filter(|(k, _)| k.parent == parent)
                .map(|(k, slot)| (k.clone(), slot.clone()))
                .collect()
        })
        .unwrap_or_default();
    for (key, slot) in slots {
        let mut state = slot.state.lock().unwrap_or_else(|p| p.into_inner());
        state.retired = true;
        state.reaping = true;
        let pod = state.pod.take();
        drop(state);
        let gone = pod.as_ref().is_none_or(|p| match p.delete() {
            Ok(()) => true,
            Err(e) => {
                eprintln!("wicked-core: parent {parent}'s warm judge was not reaped: {e}");
                p.reap();
                false
            }
        });
        let mut state = slot.state.lock().unwrap_or_else(|p| p.into_inner());
        state.reaping = !gone;
        if gone && !state.busy && clean_warm_dirs(&mut state) {
            forget_warm_slot(&key, &slot);
        }
    }
}

#[cfg(all(test, unix))]
pub(crate) fn expire_warm_for_test(key: &WarmKey) {
    let slot = WARM
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .get(key)
        .unwrap()
        .clone();
    slot.state.lock().unwrap().pod.as_mut().unwrap().expires = Instant::now();
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
    /// The host index for each tree at copy-in; the pod index never decides what may be skipped.
    pub(crate) tracked: Mutex<Tracked>,
    /// The unit's seat homes, re-synced right before the CLI starts (placeholder files).
    pub(crate) seat_dirs: Vec<PathBuf>,
    /// Writable dirs that neither come back nor stay (fenced scratch): removed at settle.
    pub(crate) scratch: Vec<PathBuf>,
    /// The canonical spelling of every path the pod mounts (env values are re-spelled to it).
    pub(crate) mounted: Vec<PathBuf>,
    writable: Vec<PathBuf>,
    pub(crate) unit_label: String,
    pub(crate) settled: Mutex<bool>,
    /// The run's node_modules claim is mounted: every `node_modules` path of the tree is the
    /// engine's, never the seat's, and never comes back (B-3 review r2).
    pub(crate) deps: bool,
    warm: Mutex<Option<WarmExecution>>,
    /// The in-pod path of the claude credential record finalize handed the wrapper
    /// (wicked-garden#1365): settle removes it when the wrapper cannot (timeout, cancel).
    pub(crate) claude_record: Mutex<Option<String>>,
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

/// The engine's cold one-shot session prefixes: they share the daemon's session namespace,
/// each in its own replica scope. Warm validator judges instead use their parent's namespace.
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
    let rt = crate::container_floor::runtime().map_err(|w| (w, w.describe().to_string()))?;
    super::images::ensure_for(kube, &rt, &[image, floor])?;
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
) -> Result<Option<(String, Vec<Entry>)>, CopyInRefusal> {
    // Every git call bounded in time and output (codex b2-seat #13).
    let bounded = |dir: &Path, args: &[&str]| -> super::Called {
        let mut c = sync::git(dir);
        c.args(args);
        super::run_capped(c, None, GIT_CALL, 64 << 20)
    };
    let head = bounded(
        tree,
        &["rev-parse", "--is-shallow-repository", "HEAD^{commit}"],
    );
    if head.code.is_none() {
        return Err(format!("git rev-parse in {}: {}", tree.display(), head.err_text()).into());
    }
    if !head.ok() {
        return Ok(None);
    }
    let head_text = String::from_utf8_lossy(&head.stdout);
    let mut lines = head_text.lines();
    let cacheable = lines.next() == Some("false");
    let sha = lines
        .next()
        .ok_or_else(|| "git rev-parse omitted HEAD".to_string())?
        .to_string();
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
            )
            .into());
        }
        return Ok(Some((sha, entries)));
    }
    let built = GitBuild {
        tree,
        sha: &sha,
        branch: &branch,
        masks,
        budget,
    }
    .build(&GIT_BUILDERS, super::engine_home().as_deref())?;
    #[cfg(test)]
    GIT_BUILDS.with(|c| c.set(c.get() + 1));
    // Still not shallow once built (codex r2 #2): a source that changed under the build is not kept.
    let still = bounded(tree, &["rev-parse", "--is-shallow-repository"]);
    let cacheable =
        cacheable && still.ok() && String::from_utf8_lossy(&still.stdout).trim() == "false";
    let size = |es: &[Entry]| {
        es.iter()
            .map(|e| e.data.capacity() + e.path.capacity() + ENTRY_OVERHEAD)
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

#[cfg(test)]
thread_local! {
    /// (tests) Plain roots read from the host, and replica `.git`s built, on this thread.
    pub(crate) static HOST_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static SIG_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static FRESH_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static GIT_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Rebuilt replica `.git`s by (tree, commit, branch, masks) (k8s-final), each at most
/// [`GIT_CACHE_ROOT`] bytes and [`GIT_CACHE_ALL`] in all.
const GIT_CACHE_ROOT: usize = 16 * 1024 * 1024;
const GIT_CACHE_ALL: usize = 64 * 1024 * 1024;
#[allow(clippy::type_complexity)]
static GIT_CACHE: std::sync::Mutex<
    Option<HashMap<(PathBuf, String, Option<String>, Vec<String>), Vec<Entry>>>,
> = std::sync::Mutex::new(None);

// (core#1441) Only builders made in this process are reused, with eight LRU slots.
#[derive(Default)]
pub(crate) struct GitBuilders {
    homes: HashSet<PathBuf>,
    slots: HashMap<PathBuf, GitSlot>,
    clock: u64,
}

#[derive(Default)]
struct GitSlot {
    used: u64,
    builder: Arc<Mutex<GitBuilder>>,
}

#[derive(Default)]
struct GitBuilder {
    built: bool,
    fresh_size: u64,
}

static GIT_BUILDERS: std::sync::LazyLock<Mutex<GitBuilders>> =
    std::sync::LazyLock::new(|| Mutex::new(GitBuilders::default()));

pub(crate) struct GitBuild<'a> {
    pub(crate) tree: &'a Path,
    pub(crate) sha: &'a str,
    pub(crate) branch: &'a Option<String>,
    pub(crate) masks: &'a [String],
    pub(crate) budget: u64,
}

struct RemoveGitBuilder(PathBuf);
impl Drop for RemoveGitBuilder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn builder_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

impl GitBuild<'_> {
    pub(crate) fn build(
        &self,
        cache: &Mutex<GitBuilders>,
        home: Option<&Path>,
    ) -> Result<Vec<Entry>, CopyInRefusal> {
        let Some(home) = home else {
            let tmp = std::env::temp_dir().join(format!(
                "wicked-replica-git-{}",
                uuid::Uuid::new_v4().simple()
            ));
            let _rm = RemoveGitBuilder(tmp.clone());
            builder_dir(&tmp)?;
            return self.populate(&tmp, true);
        };
        use sha2::{Digest, Sha256};
        let base = home.join("k8s-replica-git");
        let data =
            serde_json::to_vec(&(self.tree, self.branch, self.masks)).map_err(|e| e.to_string())?;
        let key: String = Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let path = base.join(key);
        let mut cache = cache.lock().map_err(|e| e.to_string())?;
        if !cache.homes.contains(&base) {
            match std::fs::remove_dir_all(&base) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string().into()),
            }
            builder_dir(&base)?;
            cache.homes.insert(base);
        }
        cache.clock += 1;
        let now = cache.clock;
        if !cache.slots.contains_key(&path) && cache.slots.len() >= 8 {
            let oldest = cache
                .slots
                .iter()
                .min_by_key(|(_, slot)| slot.used)
                .map(|(p, slot)| (p.clone(), Arc::clone(&slot.builder)));
            if let Some((old, slot)) = oldest {
                let _guard = slot.lock().map_err(|e| e.to_string())?;
                std::fs::remove_dir_all(&old)
                    .or_else(|e| {
                        if e.kind() == std::io::ErrorKind::NotFound {
                            Ok(())
                        } else {
                            Err(e)
                        }
                    })
                    .map_err(|e| e.to_string())?;
                cache.slots.remove(&old);
            }
        }
        let slot = cache.slots.entry(path.clone()).or_default();
        slot.used = now;
        let slot = Arc::clone(&slot.builder);
        let mut builder = slot.lock().map_err(|e| e.to_string())?;
        drop(cache);
        if builder.built {
            if let Ok(entries) = self.populate(&path, false) {
                let size: u64 = entries.iter().map(|e| e.data.len() as u64).sum();
                if size <= builder.fresh_size.saturating_mul(2) {
                    return Ok(entries);
                }
            }
        }
        builder.built = false;
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string().into()),
        }
        builder_dir(&path)?;
        let entries = self.populate(&path, true)?;
        builder.fresh_size = entries.iter().map(|e| e.data.len() as u64).sum();
        builder.built = true;
        Ok(entries)
    }

    fn populate(&self, tmp: &Path, fresh: bool) -> Result<Vec<Entry>, CopyInRefusal> {
        let Self {
            tree,
            sha,
            branch,
            masks,
            budget,
        } = *self;
        let bounded = |dir: &Path, args: &[&str]| -> super::Called {
            let mut c = sync::git(dir);
            c.args(args);
            super::run_capped(c, None, GIT_CALL, 64 << 20)
        };
        let run = |args: &[&str]| -> Result<(), String> {
            let o = bounded(tmp, args);
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
        if fresh {
            #[cfg(test)]
            FRESH_BUILDS.with(|c| c.set(c.get() + 1));
            run(&["init", "-q"])?;
        }
        let src = format!("file://{}", tree.display());
        let target = format!("+{sha}:{}", branch.as_deref().unwrap_or("refs/wicked/head"));
        let fetched = if fresh {
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
            })
        } else {
            // (core#1441) The builder has an index, but no checked-out worktree to protect.
            run(&[
                "-c",
                "core.bare=true",
                "-c",
                "protocol.file.allow=always",
                "fetch",
                "-q",
                "--no-tags",
                &format!("--depth={REPLICA_DEPTH}"),
                &src,
                &target,
            ])
        };
        if let Err(detail) = fetched {
            if !fresh {
                return Err(detail.into());
            }
            let promisor = bounded(tree, &["config", "--get-regexp", r"^remote\..*\.promisor$"]);
            let remote = if promisor.ok() {
                String::from_utf8_lossy(&promisor.stdout)
                    .lines()
                    .find_map(|line| {
                        let mut fields = line.split_whitespace();
                        let key = fields.next()?;
                        if !fields.next()?.eq_ignore_ascii_case("true") {
                            return None;
                        }
                        key.strip_prefix("remote.")?
                            .strip_suffix(".promisor")
                            .map(str::to_string)
                    })
            } else {
                None
            }
            .or_else(|| {
                let partial = bounded(tree, &["config", "--get", "extensions.partialClone"]);
                partial
                    .ok()
                    .then(|| String::from_utf8_lossy(&partial.stdout).trim().to_string())
                    .filter(|name| !name.is_empty())
            });
            if let Some(remote) = remote {
                let mut c = sync::git(tree);
                c.args([
                    "rev-list",
                    "--objects",
                    "--missing=print",
                    "-n",
                    REPLICA_DEPTH,
                    sha,
                ])
                .env("GIT_NO_LAZY_FETCH", "1");
                let objects = super::run_capped(c, None, GIT_CALL, 64 << 20);
                if objects.ok() {
                    let stdout = String::from_utf8_lossy(&objects.stdout);
                    let mut missing = stdout.lines().filter_map(|line| line.strip_prefix('?'));
                    if let Some(first) = missing.next() {
                        let count = 1 + missing.count();
                        return Err(CopyInRefusal {
                            why: FloorUnarmed::PartialClone,
                            detail: format!(
                            "the repository is a partial clone (promisor remote `{remote}`) and \
                             {count} object(s) of its last {REPLICA_DEPTH} commits were never \
                             fetched (first {first}); git fetch: {detail}"
                        ),
                        });
                    }
                }
            }
            return Err(detail.into());
        }
        match (branch, fresh) {
            (Some(b), true) => {
                run(&["update-ref", b, sha])?;
                run(&["symbolic-ref", "HEAD", b])?;
            }
            (None, _) => run(&["update-ref", "--no-deref", "HEAD", sha])?,
            (Some(_), false) => {}
        }
        run(&["read-tree", sha])?;
        // A masked path git tracks is absent from the replica's tree: marked skip-worktree, so the
        // seat's `git add -A` never stages its deletion (review r1). Its blob stays in the history,
        // as the Docker floor's read-only object store held it.
        if !masks.is_empty() {
            // Every tracked path, matched by the same folded rule as the copy-in (review r2: a
            // pathspec is case-sensitive and glob-shaped; the mask is neither).
            let o = bounded(tmp, &["ls-files", "-z"]);
            if !o.ok() {
                return Err(format!("git ls-files: {}", o.err_text()).into());
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
            )
            .into());
        }
        let mut entries = Vec::new();
        for rel in listed {
            match sync::read_host_entry(&gitdir, &rel)? {
                Some(e) => entries.push(e),
                None => {
                    return Err(format!(
                        "the replica's .git could not be read whole (`{rel}` went away)"
                    )
                    .into())
                }
            }
        }
        Ok(entries)
    }
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

/// (core#1535 item 1) The host index for trees and written-back plain roots; `None` means no repo.
pub(crate) type Tracked = HashMap<PathBuf, Option<Vec<String>>>;

/// The encoded roots, pod plan, base commit, baseline and host tracked sets from copy-in.
pub(crate) type CopyInStream = (Vec<u8>, Value, Option<String>, Baseline, Tracked);

/// A copy-in refusal's floor reason and diagnostic detail for the daemon log.
#[derive(Debug)]
pub(crate) struct CopyInRefusal {
    pub(crate) why: FloorUnarmed,
    pub(crate) detail: String,
}

impl From<String> for CopyInRefusal {
    fn from(detail: String) -> Self {
        Self {
            why: FloorUnarmed::CopyInRefused,
            detail,
        }
    }
}

/// The copy-in stream of `roots` (the host's side), with its baseline and tracked sets.
#[cfg(test)]
pub(crate) fn copy_in_stream(
    scope: Option<&str>,
    tree: &Path,
    roots: &[PlanRoot],
    with_git: bool,
    seeded: &HashSet<String>,
) -> Result<CopyInStream, CopyInRefusal> {
    copy_in_stream_stamped(scope, tree, roots, with_git, seeded, &RepoStamps::default())
}

/// [`copy_in_stream_with`] for a cold arm: the seed stamp is honoured.
#[cfg(test)]
pub(crate) fn copy_in_stream_stamped(
    scope: Option<&str>,
    tree: &Path,
    roots: &[PlanRoot],
    with_git: bool,
    seeded: &HashSet<String>,
    stamps: &RepoStamps,
) -> Result<CopyInStream, CopyInRefusal> {
    copy_in_stream_with(scope, tree, roots, with_git, seeded, stamps, false)
}

/// [`copy_in_stream_with`] with no repo stamps and an explicit seed policy.
#[cfg(test)]
pub(crate) fn copy_in_stream_fresh(
    scope: Option<&str>,
    tree: &Path,
    roots: &[PlanRoot],
    with_git: bool,
    seeded: &HashSet<String>,
    fresh_seeds: bool,
) -> Result<CopyInStream, CopyInRefusal> {
    let stamps = RepoStamps::default();
    copy_in_stream_with(scope, tree, roots, with_git, seeded, &stamps, fresh_seeds)
}

/// `fresh_seeds`: a warm judge's reset emptied its seeded roots, so they are sent again.
fn copy_in_stream_with(
    scope: Option<&str>,
    tree: &Path,
    roots: &[PlanRoot],
    with_git: bool,
    seeded: &HashSet<String>,
    stamps: &RepoStamps,
    fresh_seeds: bool,
) -> Result<CopyInStream, CopyInRefusal> {
    let mut kept = Vec::new();
    let mut signed = HashMap::new();
    let mut stream: Vec<Root> = Vec::new();
    let mut plan_roots: Vec<(String, PlanRoot)> = Vec::new();
    let mut base = None;
    let mut baseline = Baseline::new();
    let mut tracked = Tracked::new();
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
        if r.repo && stamps.kept(&replica) {
            kept.push(format!("{replica}:kept"));
            continue;
        }
        if r.mode == Mode::Tree || (r.mode == Mode::Plain && r.back) {
            tracked.insert(r.host.clone(), sync::host_tracked(&r.host)?);
        }
        if r.mode == Mode::Seed {
            // Sent once per replica, outside the unit's budget: an oversized cache is left out.
            let entries = if !fresh_seeds && seeded.contains(&replica) {
                Vec::new()
            } else {
                seed_entries(&r.host, &r.skip, &mut seed_used)
            };
            // A warm reset emptied the mounted root without changing its inode. Home mode
            // restores this bounded trusted seed in place; the one-time seed stamp is bypassed.
            let mut copied = r.clone();
            if fresh_seeds {
                copied.mode = Mode::Home;
            }
            stream.push(Root {
                replica: replica.clone(),
                mode: copied.mode,
                entries,
            });
            took.insert(replica.clone(), t_root.elapsed().as_millis());
            plan_roots.push((replica, copied));
            continue;
        }
        // A plain root read before at the same host signature is not read again (k8s-final: the
        // skills snapshot took up to 35 s to open file by file, for every replica and judge).
        // Signed BEFORE it is listed and read, and kept only when signed the same after (codex
        // r1 #5): the entries kept are those of the state the signature names.
        let cached = (r.mode == Mode::Plain && !r.repo)
            .then(|| root_signature(r))
            .flatten()
            .map(|sig| (sig, read_cache_get(&r.host, &sig)));
        let rels: Vec<String> = if matches!(&cached, Some((_, Some(_)))) {
            Vec::new()
        } else if r.mode == Mode::Files {
            r.files.clone()
        } else {
            match r.mode {
                Mode::Tree => sync::host_tree_listing(&r.host)?,
                // Never a plain listing (a `.git` gone since the plan is refused, not copied whole).
                Mode::Plain if r.repo => sync::host_repo_listing(&r.host)?,
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
                    )
                    .into());
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
                    )
                    .into());
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
                )
                .into());
            }
            budget += sync::entry_size(&r.host, p).max(p.len() as u64);
            if budget > STREAM_LIMIT {
                return Err(format!(
                    "the unit's copy-in is larger than {STREAM_LIMIT} bytes (at {})",
                    r.host.join(p).display()
                )
                .into());
            }
            // A file that is there but cannot be read refuses the copy-in (review r5); a path
            // that is gone, or a directory (a gitlink), is simply absent.
            if let Some(e) = sync::read_host_entry(&r.host, p)
                .map_err(|e| format!("the copy-in was refused: {e}"))?
            {
                entries.push(e);
            }
        }
        #[cfg(test)]
        if read_now && r.mode == Mode::Plain {
            HOST_READS.with(|c| c.set(c.get() + 1));
        }
        if let Some((sig, None)) = &cached {
            if root_signature(r).as_ref() == Some(sig) {
                read_cache_put(&r.host, sig, &entries);
            }
        }
        if r.repo && !stamps.blocked(&replica) {
            if let Some(key) = stamps.keys.get(&replica) {
                if current_repo_key(r, &replica)?.as_ref() == Some(key) {
                    signed.insert(replica.clone(), key.clone());
                }
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
                )
                .into());
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
                        repo: false,
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
    let mut plan = codec_plan(&refs);
    if let Some(items) = plan.as_array_mut() {
        for item in items {
            if let Some(key) = item[0].as_str().and_then(|rep| signed.get(rep)) {
                let key = Value::String(key.clone());
                if let Some(fields) = item.as_array_mut() {
                    fields.push(key);
                }
            }
        }
    }
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
                .chain(kept)
                .collect::<Vec<_>>()
                .join(" "),
        ),
    );
    Ok((sync::encode(&stream), plan, base, baseline, tracked))
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
pub(crate) fn copy_in(
    kube: &Kube,
    ns: &str,
    scope: Option<&str>,
    tree: &Path,
    roots: &[PlanRoot],
    with_git: bool,
) -> Result<(Option<String>, Baseline, Tracked), (FloorUnarmed, String)> {
    copy_in_fresh(kube, ns, scope, tree, roots, with_git, false)
}

fn copy_in_fresh(
    kube: &Kube,
    ns: &str,
    scope: Option<&str>,
    tree: &Path,
    roots: &[PlanRoot],
    with_git: bool,
    fresh_seeds: bool,
) -> Result<(Option<String>, Baseline, Tracked), (FloorUnarmed, String)> {
    let seeded: HashSet<String> = {
        let g = SEEDED.lock().unwrap_or_else(|p| p.into_inner());
        roots
            .iter()
            .filter(|r| r.mode == Mode::Seed)
            .filter_map(|r| replica_of(scope, &r.host).ok())
            .filter(|rep| g.as_ref().is_some_and(|s| s.contains(&seeded_key(ns, rep))))
            .collect()
    };
    let mut stamps = RepoStamps::new(scope, roots).map_err(|e| (FloorUnarmed::CopyInRefused, e))?;
    if roots.iter().any(|r| r.repo) {
        let paths: Vec<String> = roots
            .iter()
            .filter(|r| r.repo)
            .map(|r| replica_of(scope, &r.host))
            .collect::<Result<_, _>>()
            .map_err(|e| (FloorUnarmed::CopyInRefused, e))?;
        let request = serde_json::json!(paths).to_string();
        let got = codec(kube, ns, &["stamps", &request], None, CALL);
        if !got.ok() {
            return Err((
                FloorUnarmed::ClusterUnreachable,
                format!("the replica stamps could not be read: {}", got.err_text()),
            ));
        }
        stamps.have = serde_json::from_slice(&got.stdout).map_err(|e| {
            (
                FloorUnarmed::CopyInRefused,
                format!("invalid replica stamps: {e}"),
            )
        })?;
        stamps.block_writers(ns);
    }
    // What could not be copied in is said as that (fix-k8s-chat), never `root_unresolvable`.
    let (body, plan, base, baseline, tracked) =
        copy_in_stream_with(scope, tree, roots, with_git, &seeded, &stamps, fresh_seeds)
            .map_err(|r| (r.why, r.detail))?;
    if body.len() as u64 > STREAM_LIMIT + SEED_LIMIT {
        return Err((
            FloorUnarmed::CopyInRefused,
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
    Ok((base, baseline, tracked))
}

/// The facts one seat pod is armed with.
#[derive(Debug, Clone)]
pub(crate) struct ArmSpec<'a> {
    pub(crate) run_id: &'a str,
    /// (coord codex-check on #1527) The run's lifecycle generation when this launch's arming
    /// began ([`super::lifecycle`]): a retry of an ended lifecycle is never admitted.
    pub(crate) generation: u64,
    pub(crate) ord: u32,
    pub(crate) cli: &'a str,
    /// The immutable per-id seat tag, already inspected and loaded into the node store.
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
                .unwrap_or("");
            // (core#1491) A container runtime that timed out (or said nothing) under load is
            // transient: the kubelet retries it, and the arm's deadline decides.
            if reason != "CreateContainerConfigError" && transient_create_error(msg) {
                continue;
            }
            let msg = if msg.trim().is_empty() { reason } else { msg };
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

/// (core#1491) A container create/run error that is the runtime timing out (or saying nothing)
/// under load, not a refusal: `context deadline exceeded`, `DeadlineExceeded`, `timed out`, empty.
pub(crate) fn transient_create_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    // A denial is never a timeout, whatever else the message says (codex on #1527: a path named
    // `timeout-service` in a mount denial).
    if [
        "denied",
        "permission",
        "forbidden",
        "policy",
        "not allowed",
        "unauthorized",
    ]
    .iter()
    .any(|d| m.contains(d))
    {
        return false;
    }
    m.trim().is_empty()
        || m.contains("operation timeout")
        || m.contains("timed out")
        || m.contains("deadline exceeded")
        || m.contains("code = deadlineexceeded")
        || m.contains("i/o timeout")
}

/// (core#1491) Each (init) container's state that is not running, as `<kind> <name> <state>
/// <reason> <message>` (pure): the runtime's own words about why a seat is not up.
pub(crate) fn container_states(p: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for (kind, list) in [
        ("init container", "/status/initContainerStatuses"),
        ("container", "/status/containerStatuses"),
    ] {
        for c in p
            .pointer(list)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let name = c.get("name").and_then(Value::as_str).unwrap_or("?");
            for st in ["waiting", "terminated"] {
                if let Some(v) = c.pointer(&format!("/state/{st}")) {
                    let f = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
                    out.push(
                        format!("{kind} {name} {st} {} {}", f("reason"), f("message"))
                            .trim()
                            .to_string(),
                    );
                }
            }
        }
    }
    out
}

/// (coord codex-check on #1527, HIDE) Record every container state an arm's polls saw (`seen`,
/// first sighting order, no repeats): a transient runtime error (`CreateContainerError: context
/// deadline exceeded`) the wait went on through is kept even when a later poll shows only
/// `ContainerCreating` and no event survives.
pub(crate) fn note_states(seen: &mut Vec<String>, pod: &Value) {
    for st in container_states(pod) {
        if !seen.contains(&st) {
            seen.push(st);
        }
    }
}

/// (coord codex-check on #1527, HIDE) An arm that ends on an API error (a pod or Sandbox GET
/// failed) still carries every container state its earlier polls saw: the runtime's own words
/// (`CreateContainerError: context deadline exceeded`) survive a failed read.
pub(crate) fn with_history(err: String, seen: &[String]) -> String {
    if seen.is_empty() {
        err
    } else {
        format!("{err}; seen during the wait: {}", seen.join(" | "))
    }
}

/// (core#1541) API failures carry the last pod diagnosis before the guard reaps it.
pub(crate) fn api_error_message(err: &str, diagnosis: &str, seen: &[String]) -> String {
    with_history(format!("{err} ({diagnosis})"), seen)
}

/// Bound each condition, container state, history entry and event message without splitting a
/// character. Long runtime messages must leave room for the rest of the pod's evidence.
fn diagnosis_message(message: &str) -> String {
    message.chars().take(200).collect()
}

/// The pod's evidence plus an explicit error when its events could not be read or parsed (pure).
pub(crate) fn pod_diagnosis_with_events(
    pod: Option<&Value>,
    result: &super::Called,
    n: usize,
    seen: &[String],
) -> String {
    let events = if result.ok() {
        serde_json::from_slice::<Value>(&result.stdout).map_err(|e| e.to_string())
    } else {
        Err(result.err_text())
    };
    match events {
        Ok(events) => pod_diagnosis_seen(pod, Some(&events), n, seen),
        Err(e) => format!(
            "{}; events unavailable: {e}",
            pod_diagnosis_seen(pod, None, n, seen)
        ),
    }
}

/// [`pod_diagnosis`] plus every state an earlier poll saw that the final snapshot no longer shows.
pub(crate) fn pod_diagnosis_seen(
    pod: Option<&Value>,
    events: Option<&Value>,
    n: usize,
    seen: &[String],
) -> String {
    let mut d = pod_diagnosis(pod, events, n);
    let now = pod.map(container_states).unwrap_or_default();
    let earlier: Vec<String> = seen
        .iter()
        .filter(|s| !now.contains(s))
        .map(|s| diagnosis_message(s))
        .collect();
    if !earlier.is_empty() {
        d.push_str(&format!("; seen during the wait: {}", earlier.join(" | ")));
    }
    d
}

/// (core#1491) What a pod that did not become ready looks like, for the failure's message (pure):
/// its phase, every condition that is not True (type, reason, message), and its last `n` events
/// (reason: message), newest last, with how many earlier ones are not shown. Evidence that
/// outlives the run's namespace.
pub(crate) fn pod_diagnosis(pod: Option<&Value>, events: Option<&Value>, n: usize) -> String {
    let mut out = Vec::new();
    match pod {
        None => out.push("no pod was created".to_string()),
        Some(p) => {
            out.push(format!(
                "phase {}",
                p.pointer("/status/phase")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ));
            for c in p
                .pointer("/status/conditions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|c| c.get("status").and_then(Value::as_str) != Some("True"))
            {
                let field = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("");
                out.push(diagnosis_message(
                    format!("{} {} {}", field("type"), field("reason"), field("message")).trim(),
                ));
            }
            // (codex confirmation on #1527, HIDE) Each container's own state that is not
            // running — the runtime's words, init containers first — so it outlives the pod even
            // when no event kept it.
            out.extend(container_states(p).iter().map(|s| diagnosis_message(s)));
        }
    }
    let mut items: Vec<&Value> = events
        .and_then(|e| e.get("items"))
        .and_then(Value::as_array)
        .map(|v| v.iter().collect())
        .unwrap_or_default();
    // Newest LAST by when each event last occurred (RFC 3339 sorts as text), not by list order.
    let when = |e: &Value| {
        [
            "/series/lastObservedTime",
            "/lastTimestamp",
            "/eventTime",
            "/metadata/creationTimestamp",
        ]
        .iter()
        .find_map(|p| {
            e.pointer(p)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("")
        .to_string()
    };
    items.sort_by_key(|e| when(e));
    let last: Vec<String> = items
        .iter()
        .rev()
        .take(n)
        .rev()
        .map(|e| {
            format!(
                "{}: {}",
                e.get("reason").and_then(Value::as_str).unwrap_or("?"),
                diagnosis_message(
                    e.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                )
            )
        })
        .collect();
    if !last.is_empty() {
        // (lane-c2, HIDE) Never a silent cut: the count left out is said.
        let earlier = items.len().saturating_sub(last.len());
        let note = if earlier > 0 {
            format!(" ({earlier} earlier event(s) not shown)")
        } else {
            String::new()
        };
        out.push(format!("last events{note}: {}", last.join(" | ")));
    }
    out.join("; ")
}

/// (core#1491) The backoff before each retry of an arm whose pod did not become ready.
pub(crate) const ARM_RETRY_BACKOFF: [Duration; 2] =
    [Duration::from_secs(10), Duration::from_secs(30)];

/// (core#1491) Run `arm` again after each backoff while it fails as `pod_not_ready` (a slow node:
/// waiting can help) and `stop` does not say the run ended; any other result is final. The last
/// failure says how many arms were tried and keeps the earlier diagnoses.
pub(crate) fn retry_arm<T>(
    backoff: &[Duration],
    sleep: &dyn Fn(Duration),
    stop: &dyn Fn() -> bool,
    mut arm: impl FnMut() -> Result<T, (FloorUnarmed, String)>,
) -> Result<T, (FloorUnarmed, String)> {
    let mut tries = 0usize;
    let mut earlier = Vec::new();
    loop {
        match arm() {
            Err((why @ FloorUnarmed::PodNotReady, msg)) if tries < backoff.len() && !stop() => {
                eprintln!(
                    "wicked-core: seat arm {} ({msg}); retrying in {:?}",
                    why.as_str(),
                    backoff[tries]
                );
                sleep(backoff[tries]);
                tries += 1;
                if stop() {
                    return Err((
                        why,
                        format!(
                            "{} (the run ended during the arm's backoff)",
                            with_arm_attempts(&msg, &earlier)
                        ),
                    ));
                }
                earlier.push(msg.chars().take(1000).collect());
            }
            Err((why, msg)) if tries > 0 => return Err((why, with_arm_attempts(&msg, &earlier))),
            other => return other,
        }
    }
}

// (core#1541) The final diagnosis comes first; earlier attempts have bounded messages.
fn with_arm_attempts(last: &str, earlier: &[String]) -> String {
    if earlier.is_empty() {
        return last.to_string();
    }
    let attempts = earlier.len() + 1;
    let history = earlier
        .iter()
        .enumerate()
        .map(|(i, msg)| format!("attempt {}: {msg}", i + 1))
        .collect::<Vec<_>>()
        .join(" | ");
    format!("{last} (after {attempts} arm attempts; earlier: {history})")
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
        } else if msg.contains("violates PodSecurity")
            || msg.contains("forbidden")
            || msg.contains("Forbidden")
            // (codex on #1527) An admission webhook's denial is a policy refusal too.
            || msg.contains("admission webhook")
            || msg.contains("denied the request")
        {
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
    arm_warm(kube, s, landrun, None, None)
}

/// `checkout` is explicit: only eligible wrapped validator judges may request reuse.
pub(crate) fn arm_warm(
    kube: &Kube,
    s: &ArmSpec<'_>,
    landrun: Vec<String>,
    checkout: Option<Arc<WarmCheckout>>,
    images: Option<(&str, &str)>,
) -> Result<(Arc<Seat>, Vec<String>), (FloorUnarmed, String)> {
    let result = arm_physical(kube, s, landrun.clone(), checkout.clone(), images);
    if result.is_err() && checkout.as_ref().is_some_and(|c| c.live()) {
        if let Some(c) = &checkout {
            let old = c
                .slot
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pod
                .take();
            if let Some(old) = old {
                old.reap();
            }
        }
        if let Err((_, e)) = &result {
            eprintln!(
                "wicked-core: warm judge preparation unproved; reaping and taking a cold arm: {e}"
            );
        }
        // (W24) A true cold arm: the checkout's own refusal (a judge the warm guard turns away)
        // must never decide the fallback too.
        return arm_physical(kube, s, landrun, None, images);
    }
    result
}

/// (W24) A warm judge is a read-only view whose write roots are at most its own tree: the view
/// is thrown away with the pod and the warm plan writes nothing back. The wrapped runner arms
/// every ungoverned judge with its tree as its write root (`worker_write_roots` = [cwd]).
fn warm_admits(s: &ArmSpec<'_>) -> bool {
    s.view && s.write_roots.iter().all(|w| w.starts_with(s.tree))
}

fn arm_physical(
    kube: &Kube,
    s: &ArmSpec<'_>,
    landrun: Vec<String>,
    checkout: Option<Arc<WarmCheckout>>,
    images: Option<(&str, &str)>,
) -> Result<(Arc<Seat>, Vec<String>), (FloorUnarmed, String)> {
    // A retry is another arm: a replaced node needs the images again before any pod apply.
    let rt = crate::container_floor::runtime().map_err(|w| (w, w.describe().to_string()))?;
    super::images::ensure_for(kube, &rt, &[s.image, s.floor_image, s.sync_image])?;
    let (ns, scope, run) = match &checkout {
        Some(c) if c.live() && warm_admits(s) && images.is_some() => (
            super::run_namespace_name(&kube.owner, &c.key.parent),
            Some(c.key.scope()),
            c.key.parent.clone(),
        ),
        Some(_) => {
            return Err((
                FloorUnarmed::PodNotReady,
                "the warm judge is not read-only or its parent ended".into(),
            ))
        }
        None => place(kube, s.run_id),
    };
    let lifecycle_run = checkout
        .as_ref()
        .map_or(s.run_id, |c| c.key.parent.as_str());
    let generation = checkout.as_ref().map_or(s.generation, |c| c.generation);

    if scope.is_some() && checkout.is_none() {
        let arming = arm_lock(&ns);
        let _arming = arming.lock().unwrap_or_else(|p| p.into_inner());
        SCOPES
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_or_insert_with(HashMap::new)
            .insert(
                s.run_id.to_string(),
                ScopeRecord {
                    kube: kube.clone(),
                    pending: false,
                },
            );
    }
    if let Some(c) = &checkout {
        c.slot
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .replica = Some((kube.clone(), ns.clone(), c.key.scope()));
    }
    super::admit_arm(lifecycle_run, generation, || {
        cluster::ensure_run(
            kube,
            &run,
            &RunBudget::default(),
            &format!("{}Gi", render::REPLICA_CLAIM_GI),
        )
    })
    .ok_or_else(|| {
        (
            FloorUnarmed::PodNotReady,
            "the run ended before its seat could arm".to_string(),
        )
    })?
    .map_err(|e| (classify_apply(&e), e))?;
    super::clock::mark("namespace");
    // The sync pod is inspected and replaced under the namespace's arm lock (codex r2): two
    // arms never delete each other's replacement.
    {
        let arming = arm_lock(&ns);
        let _arming = arming.lock().unwrap_or_else(|p| p.into_inner());
        ensure_sync(kube, &ns, &run, s.sync_image, s.floor_image)?;
        // An earlier session's failed cleanup is retried here; its failure never refuses
        // this arm (the record stays for the next drain).
        if scope.is_some() {
            if let Err(e) = drain_scopes(kube, &ns) {
                eprintln!("wicked-core: session scopes in {ns} not removed: {e}");
            }
        }
    }
    super::clock::mark("sync_pod");
    // A warm judge's replica never comes back to the host (W24, [`warm_admits`]).
    let write_back: &[PathBuf] = if checkout.is_some() {
        &[]
    } else {
        s.write_roots
    };
    let (roots, pod_mounts) = plan(
        s.tree,
        s.view,
        s.mounts,
        s.protected,
        write_back,
        s.seed_base,
    );
    // Seat homes stay in the replica (review r7 #1); the unit's other writable dirs that do not
    // come back (fenced scratch) are removed from it when the unit settles (review r7 #3).
    // The fenced scratch is among the seat's dirs but is no home (codex b2-seat #12): it stays
    // plain, so it is selected below and removed at settle, never kept in the replica.
    let mut roots = roots;
    mark_homes(&mut roots, s.seat_dirs, s.scratch);
    let mut reset_dirs: Vec<PathBuf> = s.seat_dirs.to_vec();
    reset_dirs.extend(
        pod_mounts
            .iter()
            .filter(|m| checkout.is_some() && m.rw && m.path != s.tree && m.path.is_dir())
            .map(|m| m.path.clone()),
    );
    if s.deps.is_some() {
        reset_dirs.extend(
            render::NM_WRITABLE
                .iter()
                .map(|d| s.tree.join("node_modules").join(d)),
        );
    }
    reset_dirs.sort();
    reset_dirs.dedup();
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
    let id = uuid::Uuid::new_v4().simple().to_string();
    let mut sandbox = format!("{}-{}-{}", s.role, s.ord, &id[..10]);
    let mut unit_label = label_value(&format!("{}-{}", s.ord, &id[..10]));
    let secs = if checkout.is_some() {
        (6 * 3600).max(s.deadline.as_secs() + 600)
    } else {
        s.deadline.as_secs() + 600
    };
    // Shutdown timestamps round down to seconds; keep our estimate strictly earlier.
    let lifetime = Duration::from_secs(secs.saturating_sub(2));
    let mut expires = Instant::now() + lifetime;
    let mut wall_expires = std::time::SystemTime::now() + lifetime;
    let spec = SeatPod {
        name: sandbox.clone(),
        namespace: ns.clone(),
        owner: kube.owner.clone(),
        run_label: label_value(lifecycle_run),
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
    let mut doc = render::seat_sandbox(&spec).map_err(|e| (FloorUnarmed::RootUnresolvable, e))?;
    let mut expected_sandbox = doc["spec"].clone();
    let expected_pod = expected_sandbox["podTemplate"]["spec"].clone();
    let mut fingerprint = doc.clone();
    fingerprint["metadata"]["name"] = Value::Null;
    fingerprint["metadata"]["labels"][UNIT_LABEL] = Value::Null;
    fingerprint["spec"]["podTemplate"]["metadata"]["labels"][UNIT_LABEL] = Value::Null;
    fingerprint["spec"]["shutdownTime"] = Value::Null;
    fingerprint["plan"] = Value::String(format!(
        "{roots:?} {:?} {:?} {:?} {:?} {}",
        s.protected, s.seat_dirs, s.scratch, s.seed_base, generation
    ));
    fingerprint["images"] = serde_json::json!(images);
    fingerprint["launcher"] = serde_json::json!([s.cli, s.sync_image, format!("{landrun:?}")]);
    // File subPaths pin inodes: the codec replaces files. Reuse only when their trusted
    // contents are identical; changed files must be bound afresh by a cold arm.
    let mut file_hashes = Vec::new();
    for root in roots
        .iter()
        .filter(|r| checkout.is_some() && r.mode == Mode::Files)
    {
        for name in &root.files {
            if sync::entry_size(&root.host, name) > STREAM_LIMIT {
                return Err((
                    FloorUnarmed::CopyInRefused,
                    "a pinned file exceeds the stream limit".into(),
                ));
            }
            let e = sync::read_host_entry(&root.host, name)
                .map_err(|e| (FloorUnarmed::CopyInRefused, e))?
                .ok_or_else(|| {
                    (
                        FloorUnarmed::CopyInRefused,
                        "a pinned file disappeared".into(),
                    )
                })?;
            file_hashes.push((root.host.join(name), sync::digest(&e)));
        }
    }
    fingerprint["file_subpaths"] = Value::String(format!("{file_hashes:?}"));
    struct Pending(Option<PhysicalJudge>);
    impl Drop for Pending {
        fn drop(&mut self) {
            if let Some(p) = self.0.take() {
                p.reap();
            }
        }
    }
    let mut pending = Pending(checkout.as_ref().and_then(|c| {
        c.slot
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pod
            .take()
    }));
    if let Some(p) = pending.0.as_ref() {
        let checked = if p.kube != *kube || p.fingerprint != fingerprint {
            Err("the judge mount plan or cluster fingerprint changed".into())
        } else {
            p.verify(s.deadline).and_then(|()| p.quiet())
        };
        if let Err(e) = checked {
            eprintln!("wicked-core: warm judge discarded; cold arm: {e}");
            let old = pending.0.take().expect("checked physical judge");
            old.delete().map_err(|e| {
                old.reap();
                (FloorUnarmed::ClusterUnreachable, e)
            })?;
        }
    }
    let reused = pending.0.is_some();
    if let Some(p) = pending.0.as_ref() {
        sandbox.clone_from(&p.sandbox);
        unit_label.clone_from(&p.unit);
    }
    let mut reset_mounts: Vec<PathBuf> = pod_mounts.iter().map(|m| m.path.clone()).collect();
    if s.deps.is_some() {
        reset_mounts.push(s.tree.join("node_modules"));
    }
    // One arm at a time copies into a namespace's replica (review r7 #7): the stale-pod check and
    // the copy-in see the same pods.
    let arming = arm_lock(&ns);
    let _arming = arming.lock().unwrap_or_else(|p| p.into_inner());
    clear_stale(kube, &ns)?;
    if checkout.is_some() && !reused {
        drop_replica_scope(kube, &ns, scope.as_deref().expect("warm scope"))
            .map_err(|e| (FloorUnarmed::ClusterUnreachable, e))?;
    }
    super::clock::mark("clear_stale");
    if let Some(p) = pending.0.as_ref() {
        // Reset before copy-in; rebuild the view again after fresh replica inputs arrive.
        warm_exec(
            p,
            &warm_reset_script(s.tree, &reset_mounts, &reset_dirs),
            "wicked-warm-reset",
        )
        .map_err(|e| (FloorUnarmed::ClusterUnreachable, e))?;
    }
    let (base, baseline, tracked) = copy_in_fresh(
        kube,
        &ns,
        scope.as_deref(),
        s.tree,
        &roots,
        true,
        checkout.is_some(),
    )?;
    if s.deps.is_some() {
        let tree = replica_of(scope.as_deref(), s.tree)
            .map_err(|e| (FloorUnarmed::RootUnresolvable, e))?;
        let r = codec(kube, &ns, &["deps", &tree], None, CALL);
        if !r.ok() {
            return Err((
                FloorUnarmed::CopyInRefused,
                format!(
                    "the dependency mount destination could not be prepared: {}",
                    r.err_text()
                ),
            ));
        }
    }
    if let Some(p) = pending.0.as_ref() {
        warm_exec(
            p,
            &warm_reset_script(s.tree, &reset_mounts, &[]),
            "wicked-warm-reset",
        )
        .map_err(|e| (FloorUnarmed::ClusterUnreachable, e))?;
    }
    // The branch the commits land on, as it was at copy-in (codex b2-seat #9).
    let branch = match &base {
        Some(_) => head_branch(s.tree).map_err(|e| (FloorUnarmed::RootUnresolvable, e))?,
        None => None,
    };
    if !reused {
        // A new pod's clock starts after copy-in, just as on the cold path. Reuse never
        // extends the physical generation's absolute lifetime.
        expires = Instant::now() + lifetime;
        wall_expires = std::time::SystemTime::now() + lifetime;
        let shutdown = Value::String(super::rfc3339_after(Duration::from_secs(secs)));
        doc["spec"]["shutdownTime"] = shutdown.clone();
        expected_sandbox["shutdownTime"] = shutdown;
    }
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
        tracked: Mutex::new(tracked),
        seat_dirs: s.seat_dirs.to_vec(),
        scratch,
        mounted,
        writable: pod_mounts
            .iter()
            .filter(|m| m.rw)
            .map(|m| m.path.clone())
            .collect(),
        unit_label: unit_label.clone(),
        settled: Mutex::new(false),
        deps: s.deps.is_some(),
        warm: Mutex::new(None),
        claude_record: Mutex::new(None),
    });
    // (core#1463) The next copy-in must see this seat's writable mounts.
    drop(_arming);
    // Owned from here: every way out before the caller holds the seat reaps it (review r1: an
    // API error while waiting, or an apply whose reply was lost, leaked the Sandbox).
    struct Guard(
        Option<String>,
        Option<crate::checks_container::DependencyMount>,
    );
    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(id) = self.0.take() {
                reap(&id);
            }
        }
    }
    let mut guard = Guard(Some(id.clone()), None);
    guard.1 = crate::checks_container::dependency_mounts(kube, &ns, s.deps)
        .map_err(|e| (classify_apply(&e), e))?;
    if !reused {
        kube.apply(&[doc]).map_err(|e| (classify_apply(&e), e))?;
    }
    // From here every failure reaps the Sandbox (the caller's guard holds the id).
    super::clock::mark("sandbox_apply");
    let start = Instant::now();
    // Every container state any poll saw, for the diagnosis (coord codex-check on #1527).
    let mut seen: Vec<String> = Vec::new();
    let mut last: Option<Value> = None;
    // (core#1491) The pod's phase, conditions and last events, read before the reap.
    let diagnose = |pod: Option<&Value>, seen: &[String]| {
        let events = kube.call(
            Some(&ns),
            &[
                "get",
                "events",
                "--field-selector",
                &format!("involvedObject.name={sandbox}"),
                "-o",
                "json",
            ],
            None,
            Duration::from_secs(15),
        );
        pod_diagnosis_with_events(pod, &events, 5, seen)
    };
    loop {
        let pod = kube.get_json(Some(&ns), "pod", &sandbox).map_err(|e| {
            (
                FloorUnarmed::ClusterUnreachable,
                api_error_message(&e, &diagnose(last.as_ref(), &[]), &seen),
            )
        })?;
        if let Some(p) = pod.as_ref() {
            note_states(&mut seen, p);
        }
        match pod.as_ref().map(|p| pod_state(p, &sandbox, &unit_label)) {
            Some(PodState::Ready) => {
                last = pod;
                break;
            }
            Some(PodState::Failed(why, msg)) => {
                let msg = if why == FloorUnarmed::ImageUnavailable {
                    format!("{msg}; {}", super::images::start_fix(kube))
                } else {
                    msg
                };
                let msg = format!("{msg} ({})", diagnose(pod.as_ref(), &seen));
                reap(&id);
                return Err((why, msg));
            }
            _ => {}
        }
        if start.elapsed() > POD_START {
            // (codex on #1527) A Sandbox that cannot be read is no proof of "no refusal": the
            // cluster did not answer. Every outcome carries the pod's diagnosis.
            let why = match kube.get_json(Some(&ns), "sandboxes.agents.x-k8s.io", &sandbox) {
                Err(e) => (
                    FloorUnarmed::ClusterUnreachable,
                    api_error_message(
                        &format!("the seat's Sandbox could not be read at the arm's deadline: {e}"),
                        &diagnose(pod.as_ref(), &[]),
                        &seen,
                    ),
                ),
                Ok(sb) => match sb.as_ref().and_then(sandbox_refusal) {
                    Some((why, msg)) => (why, format!("{msg} ({})", diagnose(pod.as_ref(), &seen))),
                    None => (
                        FloorUnarmed::PodNotReady,
                        format!(
                            "the seat pod {sandbox} was not ready in {POD_START:?}: {}",
                            diagnose(pod.as_ref(), &seen)
                        ),
                    ),
                },
            };
            reap(&id);
            return Err(why);
        }
        // (core#1541) The last snapshot read, for a later API error's diagnosis.
        if pod.is_some() {
            last = pod;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if let Some(dependencies) = guard.1.take() {
        dependencies.mounted();
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
    if let Some(c) = checkout.as_ref() {
        if !c.live() {
            return Err((
                FloorUnarmed::PodNotReady,
                "the judge's parent ended during arm".into(),
            ));
        }
        let sandbox_object = kube
            .get_json(Some(&ns), "sandboxes.agents.x-k8s.io", &sandbox)
            .ok()
            .flatten();
        let physical = pending.0.take().or_else(|| {
            let pod = last.as_ref()?;
            let uid = pod.pointer("/metadata/uid")?.as_str()?.to_string();
            let owner_uid = pod
                .pointer("/metadata/ownerReferences")?
                .as_array()?
                .iter()
                .find(|o| o["kind"] == "Sandbox" && o["name"] == sandbox)?["uid"]
                .as_str()?
                .to_string();
            let owner = sandbox_object.as_ref()?;
            if uid.is_empty()
                || owner_uid.is_empty()
                || !super::is_owned_by(pod, &kube.owner)
                || !super::is_owned_by(owner, &kube.owner)
                || owner.pointer("/metadata/uid").and_then(Value::as_str)
                    != Some(owner_uid.as_str())
                || !pod_contract_matches(&expected_pod, pod.get("spec")?)
                || !pod_contract_matches(&expected_sandbox, owner.get("spec")?)
                || !pod_contract_matches(
                    &expected_sandbox["podTemplate"]["metadata"]["labels"],
                    pod.pointer("/metadata/labels")?,
                )
            {
                return None;
            }
            Some(PhysicalJudge {
                kube: kube.clone(),
                ns: ns.clone(),
                sandbox: sandbox.clone(),
                unit: unit_label.clone(),
                uid,
                owner_uid,
                fingerprint,
                pod_spec: pod.get("spec")?.clone(),
                pod_labels: pod.pointer("/metadata/labels")?.clone(),
                sandbox_spec: owner.get("spec")?.clone(),
                expires,
                wall_expires,
            })
        });
        match physical {
            Some(physical) => {
                *seat.warm.lock().unwrap_or_else(|p| p.into_inner()) = Some(WarmExecution {
                    checkout: c.clone(),
                    physical,
                })
            }
            None => {
                return Err((
                    FloorUnarmed::PolicyRefused,
                    "the new judge's physical identity or rendered contract was not proved".into(),
                ))
            }
        }
    }
    if checkout.is_some() && !super::admit_warm(lifecycle_run, generation) {
        return Err((
            FloorUnarmed::PodNotReady,
            "the run ended during its seat arm".into(),
        ));
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

#[cfg(test)]
#[test]
fn named_dependency_refusal_keeps_the_admission_pending_classification() {
    let error = "admission_pending: dependency claim pvc/wfloor-nm-old was retired";
    assert_eq!(classify_apply(error), FloorUnarmed::AdmissionPending);
}

fn classify_apply(e: &str) -> FloorUnarmed {
    if e.starts_with("admission_pending:") || e.contains("exceeded quota") {
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
/// landrun; unmounted seat-owned git identity rides the same handoff. The seat homes are re-synced
/// (the runner wrote their placeholder files after arming). The returned command is kubectl
/// alone, with no carrier variable in its own env.
/// `Ok(None)`: `cmd` is not a pod seat's.
pub(crate) fn finalize(cmd: &Command) -> Result<Option<(Command, Arc<Seat>)>, String> {
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let Some(id) = seat_id_of(&args) else {
        return Ok(None);
    };
    // (core#1750) std's unix `Command` keeps an argument holding a NUL as this placeholder and
    // reports it only when that same command spawns. This rebuild never spawns it, so the CLI
    // would run on the placeholder: refuse it here, with the reason `argv_line` would name.
    if args.iter().any(|a| a == STD_NUL_ARG_PLACEHOLDER) {
        return Err("a seat argument holds a NUL".into());
    }
    let seat = lookup(&id).ok_or("the seat's pod is no longer armed")?;
    if let Some(warm) = seat.warm.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        if !warm.checkout.live() {
            return Err("the judge's parent ended before exec".into());
        }
        warm.physical.verify(Duration::ZERO)?;
    }
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
    // Read by the wrapper alone (never named to landrun, so never the CLI's env).
    let record = claude_record_vars(&seat.cli, &vars);
    if let Some((_, path)) = record
        .iter()
        .find(|(k, _)| k == "WICKED_CLAUDE_RECORD_FILE")
    {
        *seat.claude_record.lock().unwrap_or_else(|p| p.into_inner()) = Some(path.clone());
    }
    vars.extend(record);
    // nopush_git_config_env()'s GIT_CONFIG_COUNT/KEY/VALUE entries also ride this env
    // handoff and landrun's --env entries, so the push fence does not depend on a config file.
    let (git_files, git_reads) =
        git_config_handoff(&mut vars, mounted, Path::new(render::UNIT_DIR));
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
    for path in git_reads {
        launch.extend(["--ro".into(), path]);
    }
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
    // The prelude's /dev/null fallback must override the exported identity path.
    let body = format!("{}{git_files}{}", env_file(&vars)?, argv_line(&launch)?);
    // The seat homes again: the placeholder files the runner wrote after arming.
    let homes: Vec<PlanRoot> = seat
        .roots
        .iter()
        .filter(|r| seat.seat_dirs.contains(&r.host) && r.mode != Mode::Seed)
        .cloned()
        .collect();
    if !homes.is_empty() {
        let arming = arm_lock(&seat.ns);
        let _arming = arming.lock().unwrap_or_else(|p| p.into_inner());
        let (_, more, tracked) = copy_in(
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
        if let Ok(mut t) = seat.tracked.lock() {
            t.extend(tracked);
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
/// is read back. A claude unit's credential record goes first (wicked-garden#1365).
pub(crate) fn reap(id: &str) {
    if let Some(seat) = lookup(id) {
        let mut settled = seat.settled.lock().unwrap_or_else(|p| p.into_inner());
        if !*settled {
            remove_claude_record(&seat);
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
    type ArmLocks = HashMap<String, Arc<Mutex<()>>>;
    static LOCKS: Mutex<Option<ArmLocks>> = Mutex::new(None);
    let mut g = LOCKS.lock().unwrap_or_else(|p| p.into_inner());
    g.get_or_insert_with(HashMap::new)
        .entry(ns.to_string())
        .or_default()
        .clone()
}

/// Remove a settled unit's scratch from the replica (review r7 #3): a fenced unit's
/// `CARGO_TARGET_DIR` and caches would otherwise fill the run's replica unit by unit.
fn drop_scratch(seat: &Seat) {
    if seat.scratch.is_empty() {
        return;
    }
    // (core#1463) Removal and stamp publication share the copy-in lock.
    let arming = arm_lock(&seat.ns);
    let _arming = arming.lock().unwrap_or_else(|p| p.into_inner());
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
                "/replica/.wicked-stamps",
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

/// Settle a seat after its client ended: read its exit code, then establish the stop barrier
/// (confirmed deletion, or verified quiescence for an exclusive warm read-only judge) before
/// write-back. `Ok(code)` = the CLI's own exit code
/// (or the client's for a timeout or cancel); `Err` = the unit has no result (a lost stream, a
/// pod not confirmed gone, a write-back refused): could-not-run, never a pass.
#[cfg(test)]
pub(crate) fn settle(seat: &Seat, ending: Ending) -> Result<i32, String> {
    settle_unless(seat, ending, &|| false).map(|(code, _)| code)
}

/// [`settle`], with `cancelled` asked again once the pod is gone, right before the write-back
/// (codex b2-gw #4): a unit cancelled or reassigned while its rc was read or its pod deleted
/// brings nothing back. (core#1498) With the exit code, what the write-back left out, as step
/// warnings ([`write_back`]).
pub(crate) fn settle_unless(
    seat: &Seat,
    ending: Ending,
    cancelled: &dyn Fn() -> bool,
) -> Result<(i32, Vec<String>), String> {
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
    // The wrapper's own cleanup never runs once the pod is deleted under it.
    if matches!(ending, Ending::TimedOut | Ending::Cancelled) || code.is_err() {
        remove_claude_record(seat);
    }
    let warm = seat.warm.lock().unwrap_or_else(|p| p.into_inner()).take();
    if let Some(warm) = warm {
        // A read-only judge's write-back is empty. Prove the stop barrier before returning
        // its physical state; retain exclusive checkout in the runner through lease revocation.
        let barrier = warm.physical.quiet();
        let eligible = code.is_ok()
            && matches!(ending, Ending::Exited(_))
            && !cancelled()
            && warm.checkout.live()
            && write_back_roots(&seat.roots).is_empty();
        let mut state = warm
            .checkout
            .slot
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if barrier.is_ok() && eligible && !state.retired {
            warm.checkout
                .completed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            state.pod = Some(warm.physical);
            *settled = true;
            drop(state);
            drop(settled);
            unregister(&seat.id);
            return code.map(|c| (c, Vec::new()));
        }
        drop(state);
        let gone = delete_pod(seat);
        if gone.is_err() {
            unreaped(seat);
        }
        *settled = true;
        drop(settled);
        unregister(&seat.id);
        gone?;
        drop_scratch(seat);
        // Confirmed deletion proves the stop barrier even if quiescence was not proved.
        if let Err(e) = barrier {
            eprintln!(
                "wicked-core: seat {} quiescence failed; pod deletion confirmed: {e}",
                seat.id
            );
        }
        return code.map(|c| (c, Vec::new()));
    }
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
        Ok(Vec::new())
    };
    drop_scratch(seat);
    let code = code?;
    let warnings = back?;
    Ok((code, warnings))
}

/// Best effort, bounded: remove a claude seat's credential record (and its temporary name) in
/// the pod before the pod is deleted, for a unit whose wrapper will not (wicked-garden#1365). A
/// failure never blocks the deletion; it is logged by seat id only. The next unit of that run
/// and posture, or the run's teardown, removes what is left.
fn remove_claude_record(seat: &Seat) {
    let Some(c) = seat
        .claude_record
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
    else {
        return;
    };
    let tmp = format!("{c}.wicked-tmp");
    let r = seat.kube.call(
        Some(&seat.ns),
        &[
            "exec",
            "-c",
            "seat",
            seat.pod.as_str(),
            "--",
            "/floor/busybox",
            "rm",
            "-rf",
            c.as_str(),
            tmp.as_str(),
        ],
        None,
        Duration::from_secs(10),
    );
    if !r.ok() {
        eprintln!(
            "wicked-core: seat {}: the claude credential record could not be removed before its \
             pod's deletion; the next unit or the run's teardown removes it",
            seat.id
        );
    }
}

/// Select the roots returned by a unit; cargo homes and scratch must never be among them.
pub(crate) fn write_back_roots(roots: &[PlanRoot]) -> Vec<&PlanRoot> {
    roots.iter().filter(|r| r.back).collect()
}

/// The write-back (B11): every root that comes back is packed in the sync pod and checked on the
/// host whole; the unit's new commits are fetched (fsck), checked for a fast-forward and for a
/// masked or `.git` path, all BEFORE any host file changes (review r1); then only what the seat
/// changed against the copy-in baseline is applied with the no-follow walk, a file the host also
/// changed meanwhile is a conflict (refused, never overwritten), and the host HEAD moves last.
///
/// (core#1498) What the pod LEFT OUT (build output in a tagged cache dir, a file past the stream's
/// limit) is never written and never a deletion: it stays as the host has it, and each is returned
/// as a warning for the step, so a large build in a written-back root never fails the unit.
pub(crate) fn write_back(seat: &Seat, cancelled: &dyn Fn() -> bool) -> Result<Vec<String>, String> {
    let back = write_back_roots(&seat.roots);
    if back.is_empty() {
        return Ok(Vec::new());
    }
    let mut refs: Vec<(String, &PlanRoot)> = Vec::new();
    for r in &back {
        refs.push((replica_of(seat.scope.as_deref(), &r.host)?, r));
    }
    let plan = pack_plan(&refs, seat.deps.then_some(seat.tree.as_path())).to_string();
    let host_tracked = {
        let tracked = seat
            .tracked
            .lock()
            .map_err(|_| "the copy-in tracked sets are poisoned")?;
        let mut host_tracked = HashMap::new();
        for (replica, root) in &refs {
            if root.mode == Mode::Tree || (root.mode == Mode::Plain && root.back) {
                let snapshot = tracked.get(&root.host).ok_or_else(|| {
                    format!(
                        "the copy-in tracked set for {} is missing",
                        root.host.display()
                    )
                })?;
                if let Some(paths) = snapshot {
                    host_tracked.insert(replica.as_str(), paths);
                }
            }
        }
        serde_json::to_vec(&host_tracked).map_err(|e| e.to_string())?
    };
    let r = codec(
        &seat.kube,
        &seat.ns,
        &["pack", &plan],
        Some(&host_tracked),
        Duration::from_secs(300),
    );
    if !r.ok() {
        return Err(format!(
            "the write-back could not read the replica: {}",
            r.err_text()
        ));
    }
    let mut roots = sync::decode(&mut r.stdout.as_slice(), STREAM_LIMIT)
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
    let mut checked: Vec<(&PlanRoot, Vec<Entry>, Vec<sync::Skipped>, sync::SkipSet)> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for ((replica, want), got) in refs.iter().zip(roots.iter_mut()) {
        if &got.replica != replica {
            return Err("the write-back's roots are out of order".into());
        }
        if seat.deps && want.mode == Mode::Tree && want.host == seat.tree {
            got.entries.retain(|e| !in_node_modules(&e.path));
        }
        let left_out = sync::take_skipped(got)
            .map_err(|e| format!("the write-back of {} was refused: {e}", want.host.display()))?;
        warnings.extend(left_out.iter().map(|s| s.describe(&want.host)));
        let empty = HashMap::new();
        let mut entries = sync::check_entries_against(
            got,
            &want.skip,
            baseline.get(&want.host).unwrap_or(&empty),
        )
        .map_err(|e| format!("the write-back of {} was refused: {e}", want.host.display()))?;
        let skips = sync::SkipSet::new(&left_out);
        let mut matched = 0usize;
        let mut matching_cancelled = false;
        entries.retain(|e| {
            if matched.is_multiple_of(4096) && cancelled() {
                matching_cancelled = true;
            }
            matched += 1;
            !matching_cancelled && !skips.covers(&e.path)
        });
        if matching_cancelled {
            return Err("the unit was cancelled during write-back; nothing came back".into());
        }
        if want.mode == Mode::Files {
            entries.retain(|e| want.files.contains(&e.path));
        }
        checked.push((want, entries, left_out, skips));
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
    // (core#1535 item 2) Check once, with the fetched tip, before HEAD checks or host changes.
    for (want, _, left_out, _) in &checked {
        let incoming = if want.mode == Mode::Tree && want.host == seat.tree {
            commits.as_ref().map(|(new, _)| new.as_str())
        } else {
            None
        };
        sync::refuse_unsafe_skips(&want.host, want.mode, left_out, incoming)
            .map_err(|e| format!("the write-back of {} was refused: {e}", want.host.display()))?;
    }
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
    for (want, entries, left_out, skips) in checked {
        let mut base = baseline.get(&want.host).cloned().unwrap_or_default();
        if seat.deps && want.mode == Mode::Tree && want.host == seat.tree {
            base.retain(|p, _| !in_node_modules(p));
        }
        let aliased = kept_by_alias(&base, &entries, &skips);
        let mut retained = retained_links(&entries, &base);
        retained.extend(
            left_out_links(&want.host, &left_out, &aliased).map_err(|e| {
                format!("the write-back of {} was refused: {e}", want.host.display())
            })?,
        );
        let (writes, mut deletes) = delta_keeping(&want.host, &base, entries, &skips, cancelled)?;
        // (core#1535 item 4) The pod lists the host's tracked set too: an absent tracked path
        // was deleted. An untracked one the host's ignore rules match is no deletion.
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
    Ok(warnings)
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
    // (core#1423) A host link the stream never carried (an ignored one) can only resolve
    // differently when the write-back adds a link or replaces or deletes one: only then is the
    // host walked for them (a plain file edit leaves every resolution as it was).
    let is_host_link = |rel: &str| {
        std::fs::symlink_metadata(host.join(rel)).is_ok_and(|m| m.file_type().is_symlink())
    };
    let changes_a_link = writes
        .iter()
        .any(|w| w.kind == sync::Kind::Link || is_host_link(&w.path))
        || deletes.iter().any(|d| is_host_link(d));
    let host_links = if changes_a_link {
        sync::surviving_host_links(host, writes, deletes, fence)?
    } else {
        Vec::new()
    };
    for l in retained.iter().chain(&host_links) {
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

fn alias_keeps(path: &str, sent: &HashSet<&str>, skips: &sync::SkipSet) -> bool {
    !skips.covers(path) && !sent.contains(path) && skips.may_alias(path)
}

/// (core#1535 item 3) Baseline paths kept only because a skip may alias them, sorted.
pub(crate) fn kept_by_alias(
    base: &HashMap<String, [u8; 32]>,
    entries: &[Entry],
    skips: &sync::SkipSet,
) -> Vec<String> {
    let sent: HashSet<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    let mut kept: Vec<String> = base
        .keys()
        .filter(|p| alias_keeps(p, &sent, skips))
        .cloned()
        .collect();
    kept.sort();
    kept
}

/// (core#1498) [`delta`], with every path the pod left out kept as the host has it: never written
/// (the pod sent none of it) and never a deletion.
pub(crate) fn delta_keeping(
    host: &Path,
    base: &HashMap<String, [u8; 32]>,
    entries: Vec<Entry>,
    skips: &sync::SkipSet,
    cancelled: &dyn Fn() -> bool,
) -> Result<(Vec<Entry>, Vec<String>), String> {
    // (codex r1) Left out BEFORE the deletion check: a path the host changed meanwhile under a
    // skip is no conflict, because nothing of it is written or deleted.
    // (core#1518 codex confirm #5) Nor a host path the pod sent nothing for that a skip may alias
    // on a folding host (`src/file.rs` under a skipped `SRC/file.rs`): kept, never deleted.
    let mut sent = HashSet::new();
    for (i, e) in entries.iter().enumerate() {
        if i.is_multiple_of(4096) && cancelled() {
            return Err("the unit was cancelled during write-back; nothing came back".into());
        }
        sent.insert(e.path.as_str());
    }
    let mut kept = HashMap::new();
    for (i, (p, d)) in base.iter().enumerate() {
        if i.is_multiple_of(4096) && cancelled() {
            return Err("the unit was cancelled during write-back; nothing came back".into());
        }
        if !skips.covers(p) && !alias_keeps(p, &sent, skips) {
            kept.insert(p.clone(), *d);
        }
    }
    delta(host, &kept, entries)
}

/// (codex r1, r2) The host's links a skip keeps as they are: every link the host holds NOW under a
/// left-out path (a cache dir, or a file left out for the limit), whether or not it changed since
/// the copy-in. They join the retained-link check, so what the unit changes around them can never
/// make one resolve outside its root. Only link targets are read, never a regular file's content
/// (a host file grown past the stream limit under a skip is no refusal).
pub(crate) fn left_out_links(
    host: &Path,
    left_out: &[sync::Skipped],
    aliased: &[String],
) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    let mut push_link = |rel: String| -> Result<(), String> {
        let at = host.join(&rel);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_symlink() => {
                let target = std::fs::read_link(&at)
                    .map_err(|e| format!("`{rel}` could not be read: {e}"))?;
                let data = target
                    .to_str()
                    .ok_or_else(|| format!("`{rel}` points at a non-UTF-8 path"))?
                    .as_bytes()
                    .to_vec();
                out.push(Entry {
                    kind: sync::Kind::Link,
                    path: rel,
                    data,
                });
                Ok(())
            }
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("`{rel}` could not be read: {e}")),
        }
    };
    for s in left_out {
        push_link(s.path.clone())?;
        if s.why == "cache"
            && std::fs::symlink_metadata(host.join(&s.path)).is_ok_and(|m| m.is_dir())
        {
            for rel in sync::host_plain_listing(&host.join(&s.path), &[])? {
                push_link(format!("{}/{rel}", s.path))?;
            }
        }
    }
    // (core#1535 item 3) Folded keeps retain host links outside the skip's exact spelling.
    for rel in aliased {
        push_link(rel.clone())?;
    }
    Ok(out)
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

/// Active scopes and failed cleanups share one registry; only a successful removal forgets one.
struct ScopeRecord {
    kube: Kube,
    pending: bool,
}

static SCOPES: Mutex<Option<HashMap<String, ScopeRecord>>> = Mutex::new(None);

/// The boot sweep has confirmed stale pods gone. Reconcile replica scopes against this
/// process's registry under the same lock that protects registration and copy-in.
pub(crate) fn sweep_session_scopes(kube: &Kube, ns: &str) -> Result<(), String> {
    let arming = arm_lock(ns);
    let _arming = arming.lock().unwrap_or_else(|p| p.into_inner());
    ensure_sync(
        kube,
        ns,
        super::SESSIONS_RUN,
        &crate::checks_container::image_tag(),
        &crate::container_floor::floor_image_tag(),
    )
    .map_err(|(_, e)| e)?;
    let keep: Vec<String> = SCOPES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .into_iter()
        .flat_map(|m| m.iter())
        .filter(|(id, record)| place(&record.kube, id).0 == ns)
        .filter_map(|(id, _)| one_shot_scope(id))
        .collect();
    let keep = serde_json::to_string(&keep).map_err(|e| e.to_string())?;
    let r = codec(kube, ns, &["scopes", &keep], None, CALL);
    if r.ok() {
        Ok(())
    } else {
        Err(r.err_text())
    }
}

/// Delete a one-shot session's replica scope, retrying pending cleanups in its namespace.
pub(crate) fn drop_scope(run_id: &str) {
    let kube = SCOPES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .and_then(|m| m.get(run_id))
        .map(|r| r.kube.clone());
    let Some(kube) = kube else {
        return;
    };
    let (ns, _, _) = place(&kube, run_id);
    let arming = arm_lock(&ns);
    let _arming = arming.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(record) = SCOPES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_mut()
        .and_then(|m| m.get_mut(run_id))
    {
        record.pending = true;
    }
    if let Err(e) = drain_scopes(&kube, &ns) {
        eprintln!("wicked-core: session scopes in {ns} not removed: {e}");
    }
}

/// Caller holds this namespace's arm lock, including against scope registration.
fn drain_scopes(kube: &Kube, ns: &str) -> Result<(), String> {
    let mut scopes = SCOPES.lock().unwrap_or_else(|p| p.into_inner());
    let Some(scopes) = scopes.as_mut() else {
        return Ok(());
    };
    let pending: Vec<String> = scopes
        .iter()
        .filter(|(id, r)| r.pending && place(&r.kube, id).0 == ns)
        .map(|(id, _)| id.clone())
        .collect();
    for id in pending {
        let scope = one_shot_scope(&id).ok_or("a session scope is missing")?;
        if let Ok(mut g) = SEEDED.lock() {
            if let Some(set) = g.as_mut() {
                let prefix = seeded_key(ns, &format!("{scope}/"));
                set.retain(|k| !k.starts_with(&prefix));
            }
        }
        drop_replica_scope(kube, ns, &scope)?;
        scopes.remove(&id);
    }
    Ok(())
}

fn drop_replica_scope(kube: &Kube, ns: &str, scope: &str) -> Result<(), String> {
    let r = kube.call(
        Some(ns),
        &[
            "exec",
            "-c",
            "sync",
            "sync",
            "--",
            "/floor/busybox",
            "rm",
            "-rf",
            "/replica/.wicked-stamps",
            &format!("/replica/{scope}"),
        ],
        None,
        CALL,
    );
    if !r.ok() {
        return Err(format!(
            "session scope {scope} not removed: {}",
            r.err_text()
        ));
    }
    Ok(())
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
        tracked: Mutex::new(Tracked::new()),
        seat_dirs: Vec::new(),
        scratch: Vec::new(),
        mounted: vec![tree.to_path_buf()],
        writable: vec![tree.to_path_buf()],
        unit_label: "u".into(),
        settled: Mutex::new(false),
        deps: false,
        warm: Mutex::new(None),
        claude_record: Mutex::new(None),
    })
}

#[cfg(all(test, unix))]
pub(crate) fn forget_for_test(id: &str) {
    unregister(id);
}

#[cfg(all(test, unix))]
mod image_loading_tests {
    use super::*;

    #[test]
    fn missing_sync_pod_loads_kind_images_before_apply() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var(crate::container_floor::RUNTIME_ENV, value),
                    None => std::env::remove_var(crate::container_floor::RUNTIME_ENV),
                }
            }
        }
        let _restore = Restore(std::env::var_os(crate::container_floor::RUNTIME_ENV));
        let dir = crate::container_floor::RemoveOnDrop(
            std::env::temp_dir().join(format!("sync-images-{}", uuid::Uuid::new_v4().simple())),
        );
        std::fs::create_dir_all(&dir.0).unwrap();
        let log = dir.0.join("calls");
        let applied = dir.0.join("applied");
        let cli = dir.0.join("docker");
        let script = r#"#!/bin/sh
printf 'docker %s\n' "$*" >> '@LOG@'
case "$1" in
version) echo 27;;
image) case "$*" in *sync:test) echo 'sha256:@SYNC@';; *) echo 'sha256:@FLOOR@';; esac;;
save) : > "$3";;
*) exit 98;;
esac
"#
        .replace("@LOG@", &log.display().to_string())
        .replace("@SYNC@", &"d".repeat(64))
        .replace("@FLOOR@", &"e".repeat(64));
        crate::test_env::install_stub(&cli, &script);
        crate::test_env::install_stub(
            &dir.0.join("kind"),
            &format!("#!/bin/sh\necho \"kind $*\" >> '{}'\n", log.display()),
        );
        let kubectl = dir.0.join("kubectl");
        let nodes = serde_json::json!({"items": [{
            "metadata": {"name": "n", "uid": dir.0.display().to_string()},
            "spec": {"providerID": "kind://docker/c/n"},
            "status": {"nodeInfo": {"containerRuntimeVersion": "containerd://1.7"}}
        }]});
        let pods = serde_json::json!({"items": [{
            "metadata": {"name": "sync", "labels": {
                "wicked.dev/owner": "wtest", "wicked.dev/boot": crate::k8s::boot_id()
            }},
            "status": {"phase": "Running", "conditions": [{
                "type": "Ready", "status": "True"
            }], "containerStatuses": [{"ready": true}]}
        }]});
        let script = r#"#!/bin/sh
printf 'kubectl %s\n' "$*" >> '@LOG@'
case "$*" in
*'get nodes -o json'*) echo '@NODES@';;
*'get pods '*) if [ -f '@APPLIED@' ]; then echo '@PODS@'; else echo '{"items":[]}'; fi;;
*'get pod sync '*) exit 0;;
*'apply '*) cat >/dev/null; touch '@APPLIED@';;
*) exit 98;;
esac
"#
        .replace("@LOG@", &log.display().to_string())
        .replace("@APPLIED@", &applied.display().to_string())
        .replace("@NODES@", &nodes.to_string())
        .replace("@PODS@", &pods.to_string());
        crate::test_env::install_stub(&kubectl, &script);
        std::env::set_var(crate::container_floor::RUNTIME_ENV, cli);
        let kube = crate::k8s::Kube {
            kubectl,
            kubeconfig: None,
            context: dir.0.display().to_string(),
            owner: "wtest".into(),
        };
        ensure_sync(&kube, "sync-images", "run", "sync:test", "floor:test").unwrap();
        let calls = std::fs::read_to_string(&log).unwrap();
        let apply = calls.find("apply ").unwrap();
        let loads: Vec<_> = calls.match_indices("kind load image-archive ").collect();
        assert_eq!(loads.len(), 2, "{calls}");
        assert!(loads.iter().all(|(at, _)| *at < apply), "{calls}");
        for image in ["sync:test", "floor:test"] {
            assert!(calls
                .lines()
                .any(|l| l.starts_with("docker save ") && l.ends_with(image)));
        }
    }
}

#[cfg(all(test, unix))]
mod pr_c2_tests {
    use super::*;

    #[test]
    fn boot_scope_sweep_waits_for_pods_and_keeps_a_concurrently_registered_scope() {
        let dir = std::env::temp_dir().join(format!("scope-sweep-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let kubectl = dir.join("kubectl");
        let kube = Kube {
            kubectl: kubectl.clone(),
            kubeconfig: None,
            context: "test".into(),
            owner: format!("sweep-{}", uuid::Uuid::new_v4()),
        };
        let run = format!("validator-{}", uuid::Uuid::new_v4());
        let (ns, current, _) = place(&kube, &run);
        let current = current.unwrap();
        let old = "s-000000000000000000000000";
        std::fs::create_dir(dir.join(old)).unwrap();
        std::fs::create_dir(dir.join(&current)).unwrap();
        let namespace = render::run_namespace_docs(
            &ns,
            &kube.owner,
            super::super::SESSIONS_RUN,
            "daemon",
            &RunBudget::default(),
            "20Gi",
        )
        .remove(0);
        let ready = serde_json::json!({"metadata": {"name": "sync", "labels": {
            super::super::OWNER_LABEL: kube.owner, super::super::MANAGED_LABEL: super::super::MANAGED_BY,
            super::super::BOOT_LABEL: super::super::boot_id()}}, "status": {
            "phase": "Running", "conditions": [{"type": "Ready", "status": "True"}],
            "containerStatuses": [{"ready": true}]}});
        let script = format!(
            r#"#!/usr/bin/env python3
import os, sys, json
from pathlib import Path
d = Path({dir})
a = sys.argv[1:]
if 'namespaces' in a:
    print(json.dumps({{'items': [json.loads({namespace})]}}))
elif 'delete' in a:
    (d / 'deleted').touch()
elif 'get' in a and 'pods' in a:
    assert (d / 'deleted').exists()
    if not (d / 'polled').exists():
        (d / 'polled').touch()
        print(json.dumps({{'items': [{{'metadata': {{'labels': {{'wicked.dev/boot': 'old'}}}}}}]}}))
    else:
        (d / 'barrier').touch()
        print(json.dumps({{'items': [json.loads({ready})]}}))
elif 'exec' in a:
    assert (d / 'barrier').exists()
    i = a.index('python3')
    os.environ['WICKED_SYNC_ROOT'] = str(d)
    os.environ['WICKED_SYNC_JAIL'] = '-'
    code = a[i + 3]
    sys.argv = ['codec'] + a[i + 4:]
    exec(code)
else:
    sys.exit(9)
"#,
            dir = serde_json::to_string(&dir).unwrap(),
            namespace = serde_json::to_string(&namespace.to_string()).unwrap(),
            ready = serde_json::to_string(&ready.to_string()).unwrap()
        );
        crate::test_env::install_stub(&kubectl, &script);
        let arming = arm_lock(&ns);
        let held = arming.lock().unwrap();
        let worker_kube = kube.clone();
        let sweep = std::thread::spawn(move || cluster::sweep(&worker_kube, &|_| true, None));
        let start = Instant::now();
        while !dir.join("barrier").exists() {
            assert!(
                start.elapsed() < crate::test_env::PATIENT,
                "sweep did not reach its pod barrier"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        SCOPES
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(
                run.clone(),
                ScopeRecord {
                    kube,
                    pending: false,
                },
            );
        drop(held);
        sweep.join().unwrap().unwrap();
        assert!(!dir.join(old).exists());
        assert!(dir.join(current).is_dir());
        SCOPES.lock().unwrap().as_mut().unwrap().remove(&run);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_scope_removal_keeps_its_record_for_the_next_drain() {
        let dir = std::env::temp_dir().join(format!("scope-retry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let kubectl = dir.join("kubectl");
        crate::test_env::install_stub(&kubectl, &format!(
            "#!/bin/sh\nif [ ! -f '{0}/failed' ]; then touch '{0}/failed'; exit 1; fi\necho removed >>'{0}/removed'\n",
            dir.display()
        ));
        let kube = Kube {
            kubectl,
            kubeconfig: None,
            context: "test".into(),
            owner: format!("retry-{}", uuid::Uuid::new_v4()),
        };
        let run = format!("validator-{}", uuid::Uuid::new_v4());
        SCOPES
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(
                run.clone(),
                ScopeRecord {
                    kube,
                    pending: false,
                },
            );
        drop_scope(&run);
        assert!(SCOPES.lock().unwrap().as_ref().unwrap().contains_key(&run));
        drop_scope(&run);
        assert!(!SCOPES.lock().unwrap().as_ref().unwrap().contains_key(&run));
        assert_eq!(
            std::fs::read_to_string(dir.join("removed"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
